//! `--midi-roundtrip` against a fake device and a fake MIDI port, in virtual time: a fake synth
//! hears each note-on and answers on the input a scripted number of samples after the send's
//! sample position. The measured latency must follow the script, and every exit path must leave
//! the note released.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use audio_bench::midi_roundtrip::{Driver, Settings, measure, open_output};
use audio_bench::session::Request;
use std::collections::VecDeque;
use std::time::Duration;
use transit_audio_backend as ab;
use transit_audio_backend::fake::{FakeBackend, SliceSink, SliceSource};
use transit_midi_io::PortKey;
use transit_midi_io::fake::Fake;

const RATE: u32 = 48_000;
const PERIOD: u32 = 64;
/// The driver's reported latency of each direction (the fake reports one figure for both).
const LATENCY: u32 = 64;
const ORIGIN_NS: u64 = 1_000_000_000;

/// The device, the synth and the virtual clock. Position `p`'s callback arrives at `t(p)`.
struct Rig {
    audio: FakeBackend,
    midi: Fake,
    port: PortKey,
    /// The next block's position.
    position: u64,
    /// MIDI messages already heard.
    heard: usize,
    /// The channel the synth answers on (0-based).
    synth_channel: u8,
    /// Samples from the send's position to the answer, one per note-on.
    delays: VecDeque<u64>,
    /// Positions where an answer starts.
    answers: Vec<u64>,
    /// Interrupt (as Ctrl+C) once a note-on is heard.
    interrupt_on_note: bool,
    interrupted: bool,
    noise: u32,
}

fn t(position: u64) -> u64 {
    ORIGIN_NS + u64::try_from(u128::from(position) * 1_000_000_000 / u128::from(RATE)).unwrap()
}

fn ns_of(frames: u32) -> i64 {
    i64::from(frames) * 1_000_000_000 / i64::from(RATE)
}

impl Rig {
    fn new(delays: &[u64]) -> (Self, FakeBackend) {
        let format = ab::Format::new(RATE, 2, ab::SampleFormat::F32);
        let attempt = ab::OpenRequest {
            format,
            period: ab::Period::Frames(PERIOD),
            mode: ab::Mode::Shared,
            origin: ab::HostOrigin::default(),
            calibration: None,
        };
        let audio = FakeBackend::new(format)
            .named("ASIO", "Fake ASIO")
            .with_attempts(vec![attempt])
            .with_capture(2, true)
            .with_latency(Duration::from_nanos(ns_of(LATENCY) as u64));
        let midi = Fake::default();
        let port = midi.add_output("Fake Synth");
        let rig = Self {
            audio: audio.clone(),
            midi,
            port,
            position: 4800,
            heard: 0,
            synth_channel: 0,
            delays: delays.iter().copied().collect(),
            answers: Vec::new(),
            interrupt_on_note: false,
            interrupted: false,
            noise: 0x1234_5678,
        };
        (rig, audio)
    }

    fn noise(&mut self) -> f32 {
        self.noise ^= self.noise << 13;
        self.noise ^= self.noise >> 17;
        self.noise ^= self.noise << 5;
        (self.noise as f32 / u32::MAX as f32 * 2.0 - 1.0) * 0.002
    }

    /// The synth: a note-on sent now (at `t(position)`, whose sample position is `position`)
    /// answers `delay` samples later with a fast attack that decays over 10 ms.
    fn listen(&mut self) {
        let sent = self.midi.sent(&self.port);
        for message in &sent[self.heard..] {
            if message.len() == 3
                && message[0] == 0x90 | self.synth_channel
                && message[2] > 0
                && let Some(delay) = self.delays.pop_front()
            {
                self.answers.push(self.position + delay);
            }
            if message[0] & 0xF0 == 0x90 && self.interrupt_on_note {
                self.interrupted = true;
            }
        }
        self.heard = sent.len();
    }

    fn sample(&mut self, at: u64) -> f32 {
        let tone = self
            .answers
            .iter()
            .filter(|&&a| at >= a && at < a + 480)
            .map(|&a| {
                let k = at - a;
                0.5 * (1.0 - k as f32 / 480.0) * if k.is_multiple_of(2) { 1.0 } else { -1.0 }
            })
            .sum::<f32>();
        tone + self.noise()
    }

    fn pump(&mut self) {
        let p = self.position;
        let host = t(p);
        let stamp = |frame: i64| ab::Stamp {
            host_callback_ns: host,
            backend_callback_ns: host as i64,
            backend_frame_ns: frame,
            quality: ab::Quality::BackendEstimate,
            offset_uncertainty_ns: None,
        };
        let mut render = vec![0.0f32; PERIOD as usize * 2];
        assert!(self.audio.pump_render_at(
            PERIOD as usize,
            stamp(host as i64 + ns_of(LATENCY)),
            Some(p),
            &mut SliceSink { data: &mut render, channels: 2 }
        ));
        let capture: Vec<f32> = (0..u64::from(PERIOD)).flat_map(|i| [self.sample(p + i), 0.3]).collect();
        let format = ab::Format::new(RATE, 2, ab::SampleFormat::F32);
        assert!(self.audio.pump_capture_at(
            PERIOD as usize,
            stamp(host as i64 - ns_of(LATENCY)),
            Some(p),
            format,
            &mut SliceSource { data: &capture, channels: 2 }
        ));
        self.position += u64::from(PERIOD);
    }
}

impl Driver for Rig {
    fn now_ns(&mut self) -> u64 {
        t(self.position)
    }
    fn wait(&mut self, ns: u64) -> Result<(), String> {
        self.listen();
        if self.interrupted {
            return Err("interrupted".into());
        }
        let until = t(self.position) + ns;
        while t(self.position) < until {
            self.pump();
        }
        Ok(())
    }
}

fn request() -> Request {
    Request {
        device: None,
        in_device: None,
        rate: RATE,
        period: Some(PERIOD),
        loopback: false,
        mode: ab::Mode::Shared,
        in_mode: None,
        out_ch: 1,
        in_ch: 1,
    }
}

fn settings(trials: u32) -> Settings {
    // Input 1 carries the synth; input 2 a constant level that must not be summed in.
    let mut settings = Settings::new(0, vec![0]);
    (settings.trials, settings.gap_ms, settings.max_ms) = (trials, 100, 200);
    settings
}

/// The last two messages: the note-off and All Notes Off on `channel`.
fn assert_released(rig: &Rig, channel: u8) {
    let sent = rig.midi.sent(&rig.port);
    let n = sent.len();
    assert!(n >= 2, "{sent:?}");
    assert_eq!(sent[n - 2], vec![0x80 | channel, 60, 0], "{sent:?}");
    assert_eq!(sent[n - 1], vec![0xB0 | channel, 123, 0], "{sent:?}");
}

#[test]
fn the_latency_follows_the_synths_delay_in_sample_positions() {
    let delays = [600u64, 610, 590, 600, 605, 600, 595, 600, 900, 600];
    let (mut rig, audio) = Rig::new(&delays);
    let io = rig.midi.io();
    let mut port = open_output(&io, "fake synth").unwrap();
    let report = measure(&audio, &request(), &settings(10), &mut port, &mut rig).unwrap();
    let latencies: Vec<u64> = report.trials.iter().map(|t| t.latency()).collect();
    // The onset is the 10 percent crossing of a causal envelope: the same small lag every trial.
    let lag = latencies[0] - delays[0];
    assert!(lag < audio_bench::onset::window(RATE) as u64, "lag {lag}");
    for (latency, delay) in latencies.iter().zip(delays) {
        assert_eq!(*latency, delay + lag, "{latencies:?}");
    }
    for trial in &report.trials {
        assert!(trial.uncertainty() <= audio_bench::onset::window(RATE) as u64, "{trial:?}");
        assert!(trial.onset >= trial.send);
    }
    let summary = report.summary().unwrap();
    assert_eq!((summary.n, summary.min, summary.max, summary.median), (10, 590 + lag, 900 + lag, 600.0 + lag as f64));
    assert!((summary.mean - (630.0 + lag as f64)).abs() < 1e-9, "{summary:?}");
    assert_eq!(summary.outliers, vec![9], "the 900-sample trial stands out");
    // The driver's reported input latency (64) is subtracted for the estimate; the output's is not.
    assert_eq!(report.input_latency, Some(u64::from(LATENCY)));
    assert!((report.delivery_estimate().unwrap() - (summary.mean - 64.0)).abs() < 1e-9);
    assert_eq!(report.buffer, Some(PERIOD));
    let text = report.lines().join("\n");
    assert!(text.contains("MIDI-to-audio onset latency"), "{text}");
    assert!(text.contains("effective compensation offset for this chain"), "{text}");
    assert!(text.contains("outliers (beyond 2 std dev): trial 9"), "{text}");
    assert!(text.contains("estimated MIDI delivery + synth response"), "{text}");
    let json = report.json("audio-bench --midi-roundtrip", "2026-10-23", &audio_bench::report::Machine { cpu: "c".into(), os: "o".into() });
    assert_eq!(json["trials"].as_array().unwrap().len(), 10);
    assert_eq!(json["trials"][8]["latency_samples"], 900 + lag);
    assert_eq!(json["driver_input_latency_samples"], 64);
    // Ten notes on and off, then the release on the way out.
    let sent = rig.midi.sent(&rig.port);
    assert_eq!(sent.iter().filter(|m| m[0] == 0x90).count(), 10);
    assert_released(&rig, 0);
}

#[test]
fn a_silent_synth_is_refused_and_the_note_released() {
    let (mut rig, audio) = Rig::new(&[600]);
    rig.synth_channel = 1; // listens on channel 2; the measurement plays on channel 1
    let io = rig.midi.io();
    let mut port = open_output(&io, "Fake Synth").unwrap();
    let refused = measure(&audio, &request(), &settings(3), &mut port, &mut rig).unwrap_err();
    assert!(refused.contains("trial 1: no onset within 200 ms"), "{refused}");
    assert!(refused.contains("MIDI channel 1") && refused.contains("input 1"), "{refused}");
    assert_eq!(rig.midi.sent(&rig.port).iter().filter(|m| m[0] == 0x90).count(), 1, "it stops at the first silent trial");
    assert_released(&rig, 0);
}

#[test]
fn an_interrupt_mid_trial_releases_the_note() {
    let (mut rig, audio) = Rig::new(&[600, 600]);
    rig.interrupt_on_note = true;
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let mut s = settings(2);
    s.channel = 4;
    let refused = measure(&audio, &request(), &s, &mut port, &mut rig).unwrap_err();
    assert!(refused.contains("interrupted"), "{refused}");
    assert_released(&rig, 4);
    assert!(!audio.running(ab::Direction::Output), "the streams stopped");
}

#[test]
fn a_ringing_return_before_the_send_is_refused() {
    // The synth's tail (480 samples) outlasts a 1 ms gap: the second trial's pre-send window is not
    // at the floor.
    let (mut rig, audio) = Rig::new(&[600, 600]);
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let mut s = settings(2);
    s.gap_ms = 1;
    let refused = measure(&audio, &request(), &s, &mut port, &mut rig).unwrap_err();
    assert!(refused.contains("trial 2") && refused.contains("above the noise floor"), "{refused}");
    assert_released(&rig, 0);
}

#[test]
fn the_port_is_found_by_a_unique_part_of_its_name() {
    let midi = Fake::default();
    midi.add_output("Minilogue XD");
    midi.add_output("Minilogue Mono");
    midi.add_output("Scarlett 2i2 USB");
    let io = midi.io();
    assert_eq!(open_output(&io, "scarlett").unwrap().key().name, "Scarlett 2i2 USB");
    assert!(open_output(&io, "minilogue").unwrap_err().contains("several"));
    assert!(open_output(&io, "Prophet").unwrap_err().contains("matches no MIDI output"));
}
