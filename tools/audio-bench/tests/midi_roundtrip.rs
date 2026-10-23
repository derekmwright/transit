//! `--midi-roundtrip` and `--placement` against a fake device and a fake MIDI port, in virtual
//! time: a fake synth hears each note-on and answers on the input a scripted number of samples
//! after the send's sample position. The measured latency and placement must follow the script,
//! and every exit path must leave the note released.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use audio_bench::midi_roundtrip::{Driver, Interrupt, Settings, measure, open_output};
use audio_bench::placement::{self, Compensation};
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

/// The device, the synth and the virtual clock. Position `p`'s callback arrives at `t(p)` (or as
/// scripted by `jitter`, `coarse` and `skew`); the clock itself is `now`.
struct Rig {
    audio: FakeBackend,
    midi: Fake,
    port: PortKey,
    period: u32,
    now: u64,
    /// The next block's position.
    position: u64,
    /// MIDI messages already heard, and the note-ons among them.
    heard: usize,
    note_ons: usize,
    /// The channel the synth answers on (0-based).
    synth_channel: u8,
    /// The first note-on's delay (the warm-up note), when there is one.
    warmup_delay: Option<u64>,
    /// Samples from the send's position to the answer, one per note-on ...
    delays: VecDeque<u64>,
    /// ... then this one for every further note-on.
    fixed_delay: Option<u64>,
    /// Positions where an answer starts.
    answers: Vec<u64>,
    /// Interrupt (as Ctrl+C) once this many note-ons are heard.
    interrupt_after: Option<usize>,
    interrupted: bool,
    noise: u32,
    /// Arrival jitter of the blocks in `from..to` (positions), alternating `+ns` and `-ns`.
    jitter: Option<(u64, u64, u64)>,
    /// Arrivals in groups: a block arrives when the `coarse`-frame granule holding its end is complete.
    coarse: Option<u64>,
    /// Arrival times run this much fast (a clock that does not fit the nominal rate).
    skew: Option<f64>,
    /// A drone 20 dB over the noise until this position.
    drone_until: u64,
}

fn t(position: u64) -> u64 {
    ORIGIN_NS + u64::try_from(u128::from(position) * 1_000_000_000 / u128::from(RATE)).unwrap()
}

/// The sample position at host time `ns`, rounded.
fn frame_at(ns: u64) -> u64 {
    ((ns - ORIGIN_NS) as f64 * f64::from(RATE) / 1e9).round() as u64
}

fn ns_of(frames: u32) -> i64 {
    i64::from(frames) * 1_000_000_000 / i64::from(RATE)
}

impl Rig {
    fn new(delays: &[u64]) -> (Self, FakeBackend) {
        Self::with_period(delays, PERIOD)
    }

    fn with_period(delays: &[u64], period: u32) -> (Self, FakeBackend) {
        let format = ab::Format::new(RATE, 2, ab::SampleFormat::F32);
        let attempt = ab::OpenRequest {
            format,
            period: ab::Period::Frames(period),
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
            period,
            now: t(4800),
            position: 4800,
            heard: 0,
            note_ons: 0,
            synth_channel: 0,
            warmup_delay: Some(600),
            delays: delays.iter().copied().collect(),
            fixed_delay: None,
            answers: Vec::new(),
            interrupt_after: None,
            interrupted: false,
            noise: 0x1234_5678,
            jitter: None,
            coarse: None,
            skew: None,
            drone_until: 0,
        };
        (rig, audio)
    }

    fn noise(&mut self) -> f32 {
        self.noise ^= self.noise << 13;
        self.noise ^= self.noise >> 17;
        self.noise ^= self.noise << 5;
        (self.noise as f32 / u32::MAX as f32 * 2.0 - 1.0) * 0.002
    }

    /// The synth: a note-on sent now (at host time `now`, sample position `frame_at(now)`)
    /// answers `delay` samples later with a fast attack that decays over 10 ms.
    fn listen(&mut self) {
        let sent = self.midi.sent(&self.port);
        for message in &sent[self.heard..] {
            if message.len() == 3 && message[0] == 0x90 | self.synth_channel && message[2] > 0 {
                let delay =
                    if self.note_ons == 0 && self.warmup_delay.is_some() { self.warmup_delay } else { self.delays.pop_front().or(self.fixed_delay) };
                if let Some(delay) = delay {
                    self.answers.push(frame_at(self.now) + delay);
                }
            }
            if message[0] & 0xF0 == 0x90 {
                self.note_ons += 1;
                if self.interrupt_after.is_some_and(|n| self.note_ons >= n) {
                    self.interrupted = true;
                }
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
        let drone = if at < self.drone_until { self.noise() * 10.0 } else { 0.0 };
        tone + drone + self.noise()
    }

    fn pump(&mut self) {
        let (p, period) = (self.position, u64::from(self.period));
        let mut host = match (self.jitter, self.coarse) {
            (_, Some(granule)) => t((p + period).div_ceil(granule) * granule),
            (Some((from, to, ns)), None) if (from..to).contains(&p) => {
                if (p / period).is_multiple_of(2) {
                    t(p) + ns
                } else {
                    t(p) - ns
                }
            }
            _ => t(p),
        };
        if let Some(skew) = self.skew {
            host = ORIGIN_NS + ((host - ORIGIN_NS) as f64 * skew) as u64;
        }
        let stamp = |frame: i64| ab::Stamp {
            host_callback_ns: host,
            backend_callback_ns: host as i64,
            backend_frame_ns: frame,
            quality: ab::Quality::BackendEstimate,
            offset_uncertainty_ns: None,
        };
        let mut render = vec![0.0f32; period as usize * 2];
        assert!(self.audio.pump_render_at(
            period as usize,
            stamp(host as i64 + ns_of(LATENCY)),
            Some(p),
            &mut SliceSink { data: &mut render, channels: 2 }
        ));
        let capture: Vec<f32> = (0..period).flat_map(|i| [self.sample(p + i), 0.3]).collect();
        let format = ab::Format::new(RATE, 2, ab::SampleFormat::F32);
        assert!(self.audio.pump_capture_at(
            period as usize,
            stamp(host as i64 - ns_of(LATENCY)),
            Some(p),
            format,
            &mut SliceSource { data: &capture, channels: 2 }
        ));
        self.position += period;
    }

    fn note_ons_sent(&self) -> usize {
        self.midi.sent(&self.port).iter().filter(|m| m[0] & 0xF0 == 0x90).count()
    }
}

impl Driver for Rig {
    fn now_ns(&mut self) -> u64 {
        self.now
    }
    fn wait(&mut self, ns: u64) -> Result<(), String> {
        self.listen();
        if self.interrupted {
            return Err("interrupted".into());
        }
        self.now += ns.max(1);
        while t(self.position) <= self.now {
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
fn the_latency_follows_the_synths_delay_and_the_warmup_note_is_not_measured() {
    let delays = [600u64, 610, 590, 600, 605, 600, 595, 600, 900, 600];
    let (mut rig, audio) = Rig::new(&delays);
    // The warm-up note answers far later than any trial: measured, it would be the outlier.
    rig.warmup_delay = Some(3000);
    let io = rig.midi.io();
    let mut port = open_output(&io, "fake synth").unwrap();
    let report = measure(&audio, &request(), &settings(10), &mut port, &mut rig, &Interrupt::default()).unwrap();
    let latencies: Vec<u64> = report.trials.iter().map(|t| t.latency()).collect();
    // The threshold crossing is the synth's first sample exactly: the send position has no offset.
    for (trial, delay) in report.trials.iter().zip(delays) {
        assert_eq!(trial.crossing - trial.send, delay, "{trial:?}");
    }
    // The onset is the 10 percent crossing of the 12-sample causal envelope: the scripted attack
    // (0.5 at once, alternating) reaches 0.5 x 2 / 12 = 0.083 > 10 percent of its ~0.49 peak on
    // its second sample, so the detector's bias is exactly 1 sample, every trial.
    let lag = 1;
    for (latency, delay) in latencies.iter().zip(delays) {
        assert_eq!(*latency, delay + lag, "{latencies:?}");
    }
    for trial in &report.trials {
        assert!(trial.fit.error_ns < 1.0 && trial.fit.scatter_ns < 1.0, "the virtual clock fits exactly: {trial:?}");
        assert_eq!(trial.fit.app_offset, Some(0), "the app's map agrees: {trial:?}");
        assert!(trial.uncertainty() <= audio_bench::onset::window(RATE) as u64, "{trial:?}");
        assert!(trial.onset >= trial.send);
        assert!(!trial.retried);
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
    assert!(text.contains("warm-up note sent, not measured"), "{text}");
    assert!(text.contains("effective compensation offset for this chain"), "{text}");
    assert!(text.contains("outliers (beyond 2 std dev): trial 9"), "{text}");
    assert!(text.contains("estimated MIDI delivery + synth response"), "{text}");
    assert!(text.contains("detector onset bias: late") && text.contains("up to 11 samples"), "{text}");
    assert!(text.contains("clock-fit scatter (not a bound on the send offset)"), "{text}");
    assert!(text.contains("valid at 10 of 10 sends, at most 0 samples from the bench fit"), "{text}");
    assert!(!text.contains("warning:"), "a fast attack gets no slow-attack warning: {text}");
    let json = report.json("audio-bench --midi-roundtrip", "2026-10-23", &audio_bench::report::Machine { cpu: "c".into(), os: "o".into() });
    assert_eq!(json["trials"].as_array().unwrap().len(), 10);
    assert_eq!(json["trials"][8]["latency_samples"], 900 + lag);
    assert_eq!(json["driver_input_latency_samples"], 64);
    // The warm-up note and ten notes on and off, then the release on the way out.
    let sent = rig.midi.sent(&rig.port);
    assert_eq!(sent.iter().filter(|m| m[0] == 0x90).count(), 11);
    assert_eq!(sent[0], vec![0x90, 60, 100], "the warm-up note goes first");
    assert_released(&rig, 0);
}

#[test]
fn no_warmup_sends_only_the_trials() {
    let (mut rig, audio) = Rig::new(&[600, 600]);
    rig.warmup_delay = None;
    let io = rig.midi.io();
    let mut port = open_output(&io, "fake synth").unwrap();
    let mut s = settings(2);
    s.warmup = false;
    let report = measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap();
    assert_eq!(report.trials.iter().map(|t| t.latency()).collect::<Vec<_>>(), vec![601, 601]);
    assert_eq!(rig.note_ons_sent(), 2);
    assert!(report.lines().join("\n").contains("warm-up note off"));
}

#[test]
fn the_floor_is_the_lowest_window_so_a_loud_start_does_not_raise_the_threshold() {
    // A drone 20 dB over the noise covers the reference window and trial 1's pre-send window, then
    // stops before trial 1's note sounds. The old bench took the floor from the reference window.
    let (mut rig, audio) = Rig::new(&[600, 600, 600]);
    (rig.warmup_delay, rig.drone_until) = (None, 53_000);
    let io = rig.midi.io();
    let mut port = open_output(&io, "fake synth").unwrap();
    let mut s = settings(3);
    (s.warmup, s.gap_ms) = (false, 300);
    let report = measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap();
    let old_threshold = report.reference_floor * audio_bench::onset::FLOOR_FACTOR;
    eprintln!(
        "reference floor {} (old threshold {}), run floor {}, thresholds {:?}",
        audio_bench::onset::dbfs(report.reference_floor),
        audio_bench::onset::dbfs(old_threshold),
        audio_bench::onset::dbfs(report.floor),
        report.trials.iter().map(|t| audio_bench::onset::dbfs(t.threshold)).collect::<Vec<_>>()
    );
    assert!(report.reference_floor > 0.008, "the drone is in the reference window: {report:?}");
    assert!(report.floor < report.reference_floor / 4.0, "the floor is the quiet windows': {report:?}");
    let threshold = (report.floor * audio_bench::onset::FLOOR_FACTOR).max(audio_bench::onset::MIN_THRESHOLD);
    for trial in &report.trials[1..] {
        assert_eq!(trial.threshold, threshold, "recomputed from the lowest floor: {trial:?}");
        assert!(trial.threshold < old_threshold / 4.0);
    }
    // Trial 1 ran over the drone: its threshold stays above its own pre-send level, with a warning.
    assert!(report.trials[0].threshold > report.trials[0].pre_send * 2.0, "{:?}", report.trials[0]);
    let text = report.lines().join("\n");
    assert!(text.contains("warning: trial 1's pre-send level") && !text.contains("warning: trial 2"), "{text}");
    assert_eq!(report.trials.iter().map(|t| t.latency()).collect::<Vec<_>>(), vec![601, 601, 601]);
}

#[test]
fn the_fit_converges_with_coarse_arrivals_of_256_frame_blocks() {
    // Buffer 256, blocks arriving in pairs (the second block of a pair at the same instant as the
    // first): an arrival scatter of one block, 5.3 ms. The app's fitter refuses it (the Scarlett at
    // 256 never got a map: "no clock map and input after 3000 ms"); the bench's fit converges.
    let (mut rig, audio) = Rig::with_period(&[600, 600, 600], 256);
    rig.coarse = Some(512);
    let io = rig.midi.io();
    let mut port = open_output(&io, "fake synth").unwrap();
    let mut req = request();
    req.period = Some(256);
    let report = measure(&audio, &req, &settings(3), &mut port, &mut rig, &Interrupt::default()).unwrap();
    let latencies: Vec<u64> = report.trials.iter().map(|t| t.latency()).collect();
    let fit = report.trials[0].fit;
    eprintln!("coarse 256: latencies {latencies:?}, fit {fit:?}");
    // Blocks from 4800 arrive 320 and 576 samples after their first position, 448 on average: the
    // line is that late, so every send maps 448 samples early (a constant, included in L).
    for latency in &latencies {
        assert!(latency.abs_diff(600 + 448 + 1) <= 2, "{latencies:?}");
    }
    assert!(latencies.iter().max().unwrap() - latencies.iter().min().unwrap() <= 1, "{latencies:?}");
    assert!((fit.scatter_ns - 2_666_667.0).abs() < 50_000.0, "{fit:?}");
    assert!(fit.error_ns < 500_000.0, "{fit:?}");
    assert!(report.trials.iter().all(|t| t.fit.app_offset.is_none()), "the app's fitter has no map");
    assert!(report.lines().join("\n").contains("valid at 0 of 3 sends"));
}

#[test]
fn a_clock_that_does_not_fit_reports_the_block_spacing_and_the_scatter() {
    let (mut rig, audio) = Rig::new(&[600]);
    rig.skew = Some(1.02);
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let refused = measure(&audio, &request(), &settings(2), &mut port, &mut rig, &Interrupt::default()).unwrap_err();
    assert!(refused.contains("no clock map after 3000 ms") && refused.contains("fitted rate 47058.8 Hz"), "{refused}");
    assert!(
        refused.contains("arrival spacing median 1.36 ms") && refused.contains("block of 1.33 ms") && refused.contains("fit scatter"),
        "{refused}"
    );
    assert!(!refused.contains("may not be running"), "{refused}");
    assert_eq!(rig.note_ons_sent(), 0);
    assert_released(&rig, 0);
}

#[test]
fn a_silent_synth_is_refused_and_the_note_released() {
    let (mut rig, audio) = Rig::new(&[600]);
    rig.synth_channel = 1; // listens on channel 2; the measurement plays on channel 1
    let io = rig.midi.io();
    let mut port = open_output(&io, "Fake Synth").unwrap();
    let refused = measure(&audio, &request(), &settings(3), &mut port, &mut rig, &Interrupt::default()).unwrap_err();
    assert!(refused.contains("trial 1: no onset within 200 ms"), "{refused}");
    assert!(refused.contains("MIDI channel 1") && refused.contains("input 1"), "{refused}");
    assert_eq!(rig.note_ons_sent(), 2, "the warm-up note, then it stops at the first silent trial");
    assert_released(&rig, 0);
}

#[test]
fn an_interrupt_mid_trial_releases_the_note() {
    let (mut rig, audio) = Rig::new(&[600, 600]);
    rig.interrupt_after = Some(2);
    rig.synth_channel = 4;
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let mut s = settings(2);
    s.channel = 4;
    let refused = measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap_err();
    assert!(refused.contains("interrupted"), "{refused}");
    assert_eq!(rig.note_ons_sent(), 2, "the warm-up note and trial 1's");
    assert_released(&rig, 4);
    assert!(!audio.running(ab::Direction::Output), "the streams stopped");
}

#[test]
fn a_ctrl_c_during_the_device_open_sends_no_note_and_releases() {
    let (mut rig, audio) = Rig::new(&[600]);
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let interrupt = Interrupt::default();
    assert!(!interrupt.ctrl_c(), "the first press only stops");
    let refused = measure(&audio, &request(), &settings(2), &mut port, &mut rig, &interrupt).unwrap_err();
    assert!(refused.contains("interrupted"), "{refused}");
    assert_eq!(rig.note_ons_sent(), 0, "no note-on after the stop, the warm-up note included");
    assert_released(&rig, 0);
    assert!(!interrupt.note_outstanding());
    assert!(interrupt.ctrl_c(), "with nothing outstanding a second press may exit");
}

#[test]
fn a_trial_with_an_uncertain_clock_fit_is_skipped_and_the_run_goes_on() {
    // The first 1.2 s of blocks jitter by 300 us: the first trial's fit (about 750 blocks over the
    // 1 s warm-up) has a standard error near 22 us at the send. Two seconds later the history holds
    // only clean blocks.
    let (mut rig, audio) = Rig::new(&[600, 600]);
    (rig.warmup_delay, rig.jitter) = (None, Some((0, 4800 + 57_600, 300_000)));
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let mut s = settings(3);
    (s.warmup, s.gap_ms, s.max_fit_error_ns) = (false, 2500, 15_000);
    let report = measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap();
    assert_eq!((report.skipped, report.trials.len()), (1, 2));
    assert_eq!(rig.note_ons_sent(), 2, "no note for the skipped trial");
    assert!(report.trials.iter().all(|t| t.fit.error_ns <= 15_000.0));
    let text = report.lines().join("\n");
    assert!(text.contains("skipped: 1 of 3 trials"), "{text}");
    assert!(text.contains("send-position uncertainty from the clock fit: standard error at the send"), "{text}");
    assert_released(&rig, 0);
}

#[test]
fn more_than_half_the_trials_skipped_refuses_the_run() {
    let (mut rig, audio) = Rig::new(&[600, 600, 600, 600]);
    (rig.warmup_delay, rig.jitter) = (None, Some((0, u64::MAX, 1_000_000)));
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let mut s = settings(4);
    (s.warmup, s.max_fit_error_ns) = (false, 40_000);
    let refused = measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap_err();
    assert!(refused.contains("3 of 4 trials skipped") && refused.contains("--max-fit-error-ms 0.04"), "{refused}");
    assert_eq!(rig.note_ons_sent(), 0);
    assert_released(&rig, 0);
}

#[test]
fn a_ringing_return_before_the_send_is_retried_then_refused() {
    // The synth's tail (480 samples) outlasts a 1 ms gap, and another 1 ms: the second trial's
    // pre-send window is not quiet.
    let (mut rig, audio) = Rig::new(&[600, 600]);
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let mut s = settings(2);
    s.gap_ms = 1;
    let refused = measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap_err();
    assert!(refused.contains("trial 2") && refused.contains("above the noise floor") && refused.contains("--gap-ms"), "{refused}");
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

/// A placement with the default pattern (8 notes, 500 ms apart) on a synth of fixed latency `l`.
fn placement_settings(compensation: Option<Compensation>) -> placement::Settings {
    let mut s = placement::Settings::new(0, vec![0]);
    (s.base.gap_ms, s.base.max_ms) = (300, 200);
    s.compensation = compensation;
    s
}

#[test]
fn placement_uncompensated_errors_are_the_latency_and_compensated_errors_are_near_zero() {
    const L: u64 = 1000;
    let (mut rig, audio) = Rig::new(&[]);
    rig.fixed_delay = Some(L);
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let s = placement_settings(Some(Compensation::Samples(L as i64)));
    let report = placement::measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap();
    assert_eq!(report.runs.len(), 2);
    let (plain, compensated) = (&report.runs[0], &report.runs[1]);
    assert_eq!((plain.compensation, compensated.compensation), (0, L as i64));
    assert_eq!((plain.notes.len(), compensated.notes.len()), (8, 8));
    let w = audio_bench::onset::window(RATE) as i64;
    for (a, b) in plain.notes.iter().zip(&compensated.notes) {
        // The send lands on its deadline position: the deadline wait and the clock map agree.
        assert_eq!((a.schedule_error(), b.schedule_error()), (0, 0), "{a:?} {b:?}");
        assert_eq!(a.error(), L as i64 + 1, "uncompensated: the latency plus the detector's 1-sample lag: {a:?}");
        assert!(b.error().abs() < w, "compensated: within one envelope window: {b:?}");
        assert_eq!(b.error(), 1);
        assert_eq!(b.target + L, b.intended);
    }
    // The grid: 500 ms apart.
    assert_eq!(plain.notes[1].intended - plain.notes[0].intended, 24_000);
    assert_eq!(report.compensated_within(), Some(1));
    let text = report.lines().join("\n");
    eprintln!("{text}");
    assert!(text.contains("note  1 at +0 ms: E +1001 samples (+20.85 ms) | E +1 samples (+0.02 ms)"), "{text}");
    assert!(text.contains("reading: compensated placement within +-0.02 ms (1 samples"), "{text}");
    assert!(text.contains("uncompensated placement error: mean +1001.0 samples"), "{text}");
    assert!(text.contains("sends against their deadlines") && text.contains("clock-fit scatter"), "{text}");
    let json = report.json("audio-bench --placement", "2026-10-24", &audio_bench::report::Machine { cpu: "c".into(), os: "o".into() });
    assert_eq!(json["runs"].as_array().unwrap().len(), 2);
    assert_eq!(json["runs"][1]["notes"][7]["error_samples"], 1);
    assert_eq!(json["runs"][0]["placement_error"]["mean_samples"], 1001.0);
    assert_eq!(json["compensation_samples"], L);
    // The warm-up note and 16 placed notes; released on the way out.
    assert_eq!(rig.note_ons_sent(), 17);
    assert_released(&rig, 0);
}

#[test]
fn placement_without_a_compensation_runs_once_and_suggests_one() {
    let (mut rig, audio) = Rig::new(&[]);
    rig.fixed_delay = Some(955);
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let mut s = placement_settings(None);
    s.pattern_ms = vec![0, 300, 700];
    let report = placement::measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap();
    assert_eq!(report.runs.len(), 1);
    assert!(report.runs[0].notes.iter().all(|n| n.error() == 956));
    let text = report.lines().join("\n");
    assert!(text.contains("pass --compensation 956 to check it"), "{text}");
}

#[test]
fn placement_refuses_a_negative_compensation_beyond_the_lead_before_opening_anything() {
    let (mut rig, audio) = Rig::new(&[]);
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let s = placement_settings(Some(Compensation::Samples(-13_000)));
    let refused = placement::measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap_err();
    assert!(refused.contains("negative by more than the lead (250 ms)"), "{refused}");
    assert!(rig.midi.sent(&rig.port).is_empty(), "no MIDI at all");
    assert!(!audio.running(ab::Direction::Output));
}

#[test]
fn placement_ctrl_c_mid_pattern_releases_the_note() {
    let (mut rig, audio) = Rig::new(&[]);
    (rig.fixed_delay, rig.interrupt_after) = (Some(1000), Some(4));
    let io = rig.midi.io();
    let mut port = open_output(&io, "Synth").unwrap();
    let s = placement_settings(Some(Compensation::Samples(1000)));
    let refused = placement::measure(&audio, &request(), &s, &mut port, &mut rig, &Interrupt::default()).unwrap_err();
    assert!(refused.contains("interrupted"), "{refused}");
    assert_eq!(rig.note_ons_sent(), 4, "the warm-up note and three of the pattern");
    assert_released(&rig, 0);
    assert!(!audio.running(ab::Direction::Output), "the streams stopped");
}
