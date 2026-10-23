//! MIDI-to-audio onset latency (`--midi-roundtrip`, issue #71): a note sent to a hardware synth
//! through a MIDI output port, its audio found on an input of the interface, timed in the driver's
//! sample positions.
//!
//! What the figure is: L = the onset's input sample position minus the send instant's sample
//! position. It includes MIDI delivery (the OS, driver and USB or DIN transport), the synth's
//! response and the audio input path (converter and driver buffering). It is the offset a track
//! with that synth needs to land on the grid (what #71 stores); it does not isolate the MIDI
//! transport, whose transmit instant is not observable here. "Minus the driver's reported input
//! latency" is printed as an estimate of delivery plus synth response, no more.
//!
//! The send instant: the host time H (`HostOrigin`, QPC on Windows) read on the control thread
//! right before the send call, mapped to the device's sample position by a [`ClockFitter`] (the
//! clock map the app's MIDI output thread uses, ADR 0017) fitted to the capture blocks' (sample
//! position, position system time, arrival) triples, the native ASIO backend's own clock. The
//! position clock is used, not the presentation one: no audio output is in this chain. The input
//! is recorded by sample position, so the onset is a position too, and both ends count the same
//! driver counter. Sample-accurate in the scheduling sense: what is not measured is when the OS
//! and the interface actually transmit the bytes.
//!
//! The callbacks only store atomics into storage allocated before the streams start: no
//! allocation, no lock. Analysis and every MIDI send run on the control thread.

use crate::onset;
use crate::session::{self, Callbacks, Request};
use soundcraft_playback::device_clock::{ClockFitter, ClockKey, Observation};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use transit_audio_backend as ab;

/// Length of the reference noise floor taken before the first note, and of each pre-send window.
pub const FLOOR_MS: u32 = 100;
/// The clock map and the input must be running within this time of the streams' start.
pub const WARMUP_MS: u64 = 3000;
/// Seconds of input the position ring holds beyond the onset window.
const RING_SECONDS: u32 = 4;
/// Capture blocks the control thread can fall behind by before blocks are lost to the clock map.
const BLOCKS: usize = 4096;
/// The control thread's poll interval.
const POLL_NS: u64 = 1_000_000;
/// Extra time past the onset window before a silent input counts as stalled.
const STALL_MS: u64 = 2000;
/// Defaults of the command line.
pub const DEFAULT_NOTE: u8 = 60;
pub const DEFAULT_VELOCITY: u8 = 100;
pub const DEFAULT_TRIALS: u32 = 10;
pub const DEFAULT_GAP_MS: u32 = 750;
pub const DEFAULT_MAX_MS: u32 = 1000;
/// A trial further than this many standard deviations from the mean is listed as an outlier.
pub const OUTLIER_SIGMAS: f64 = 2.0;

const NONE: u64 = u64::MAX;

/// The `--midi-roundtrip` command line: the port (a name or a unique part of one), what to play,
/// and where to write the JSON result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub port: String,
    pub settings: Settings,
    pub json: Option<std::path::PathBuf>,
}

/// What one measurement plays and listens to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// 0-based MIDI channel (0..=15).
    pub channel: u8,
    pub note: u8,
    pub velocity: u8,
    pub trials: u32,
    /// Silence between a note-off and the next trial's pre-send window; must exceed the synth's release.
    pub gap_ms: u32,
    /// The sanity window: an onset must come within this time of the send.
    pub max_ms: u32,
    /// 0-based input channels of the return, summed (one for mono, two for a stereo pair).
    pub returns: Vec<u16>,
}

impl Settings {
    /// The defaults for `channel` (0-based) and `returns` (0-based).
    pub fn new(channel: u8, returns: Vec<u16>) -> Self {
        Self {
            channel,
            note: DEFAULT_NOTE,
            velocity: DEFAULT_VELOCITY,
            trials: DEFAULT_TRIALS,
            gap_ms: DEFAULT_GAP_MS,
            max_ms: DEFAULT_MAX_MS,
            returns,
        }
    }

    fn check(&self) -> Result<(), String> {
        if self.channel > 15 || self.note > 127 || !(1..=127).contains(&self.velocity) {
            return Err("MIDI channel 1-16, note 0-127 and velocity 1-127".into());
        }
        if self.trials == 0 || self.max_ms == 0 || self.returns.is_empty() {
            return Err("--trials, --max-ms and --in need at least one".into());
        }
        Ok(())
    }
}

/// Where note messages go: a MIDI output port (or a test's fake one). Control side only.
pub trait MidiSink {
    fn send(&mut self, message: &[u8]) -> Result<(), String>;
    /// The port's name, for the report.
    fn name(&self) -> String;
}

impl MidiSink for transit_midi_io::OutputHandle {
    fn send(&mut self, message: &[u8]) -> Result<(), String> {
        transit_midi_io::OutputHandle::send(self, message).map_err(|e| format!("MIDI send: {e}"))
    }
    fn name(&self) -> String {
        self.key().to_string()
    }
}

/// The passage of time on the control thread: the real clock, or a test's virtual device.
pub trait Driver {
    /// Host time H now, in the origin of the streams' stamps.
    fn now_ns(&mut self) -> u64;
    /// Let about `ns` pass while the streams run. An error (Ctrl+C) ends the measurement; every
    /// note is released on the way out.
    fn wait(&mut self, ns: u64) -> Result<(), String>;
}

/// Real time: [`ab::HostOrigin`] (the origin the backends stamp in) and sleeps, ended by `interrupted`.
pub struct RealTime<'a> {
    pub origin: ab::HostOrigin,
    pub interrupted: &'a AtomicBool,
}

impl Driver for RealTime<'_> {
    fn now_ns(&mut self) -> u64 {
        self.origin.now_ns().unwrap_or(0)
    }
    fn wait(&mut self, ns: u64) -> Result<(), String> {
        if self.interrupted.load(Ordering::Relaxed) {
            return Err("interrupted".into());
        }
        std::thread::sleep(std::time::Duration::from_nanos(ns.min(10_000_000)));
        if self.interrupted.load(Ordering::Relaxed) { Err("interrupted".into()) } else { Ok(()) }
    }
}

/// The output port whose display name is `wanted`, ignoring case, else the one name containing
/// it; none or several is an error listing the outputs.
pub fn open_output(io: &transit_midi_io::MidiIo, wanted: &str) -> Result<transit_midi_io::OutputHandle, String> {
    let ports = io.list_outputs().map_err(|e| format!("the MIDI outputs could not be listed: {e}"))?;
    let lower = wanted.to_lowercase();
    let exact: Vec<_> = ports.iter().filter(|p| p.display_name.eq_ignore_ascii_case(wanted)).collect();
    let found = if exact.len() == 1 { exact } else { ports.iter().filter(|p| p.display_name.to_lowercase().contains(&lower)).collect() };
    let list = || ports.iter().map(|p| format!("\"{}\"", p.display_name)).collect::<Vec<_>>().join(", ");
    match found.as_slice() {
        [port] => io.open_output(&port.key).map_err(|e| format!("--midi-out \"{}\": {e}", port.display_name)),
        [] => Err(format!("--midi-out \"{wanted}\" matches no MIDI output; the outputs are: {}", list())),
        _ => Err(format!("--midi-out \"{wanted}\" matches several MIDI outputs; name one: {}", list())),
    }
}

/// Holds the note: note-on and note-off, and on drop (every exit path, an error or Ctrl+C
/// included) the note-off and All Notes Off on the channel, so no note is left sounding.
struct Notes<'a> {
    midi: &'a mut dyn MidiSink,
    channel: u8,
    note: u8,
}

impl Notes<'_> {
    fn on(&mut self, velocity: u8) -> Result<(), String> {
        self.midi.send(&[0x90 | self.channel, self.note, velocity])
    }
    fn off(&mut self) -> Result<(), String> {
        self.midi.send(&[0x80 | self.channel, self.note, 0])
    }
}

impl Drop for Notes<'_> {
    fn drop(&mut self) {
        let off = self.off();
        let all = self.midi.send(&[0xB0 | self.channel, 123, 0]);
        if let Err(e) = off.and(all) {
            eprintln!("audio-bench: releasing the note failed ({e}); check that the synth is silent");
        }
    }
}

/// What the callbacks share with the control thread. Allocated before the streams start.
struct Streams {
    /// The return by sample position: `f32` bits at `position % len`.
    ring: Box<[AtomicU32]>,
    /// The first position recorded, [`NONE`] before any.
    captured_from: AtomicU64,
    /// One past the last position recorded (0 before any).
    captured_end: AtomicU64,
    /// Capture blocks for the clock map: host, position time, frame count and position.
    blocks: Box<[[AtomicU64; 4]]>,
    blocks_written: AtomicU64,
    no_position: AtomicBool,
    returns: Box<[u16]>,
    render: OnceLock<ab::Achieved>,
    capture: OnceLock<ab::Achieved>,
    errors: AtomicU64,
}

impl Streams {
    fn new(rate: u32, settings: &Settings) -> Self {
        let len = u64::from(rate) * u64::from(RING_SECONDS) + u64::from(rate) * u64::from(settings.max_ms) / 1000;
        Self {
            ring: (0..len).map(|_| AtomicU32::new(0)).collect(),
            captured_from: AtomicU64::new(NONE),
            captured_end: AtomicU64::new(0),
            blocks: (0..BLOCKS).map(|_| std::array::from_fn(|_| AtomicU64::new(0))).collect(),
            blocks_written: AtomicU64::new(0),
            no_position: AtomicBool::new(false),
            returns: settings.returns.clone().into_boxed_slice(),
            render: OnceLock::new(),
            capture: OnceLock::new(),
            errors: AtomicU64::new(0),
        }
    }

    /// Positions `from..to` of the return. An error when they are not all recorded or the ring has
    /// overwritten some (the control thread fell behind).
    fn read(&self, from: u64, to: u64) -> Result<Vec<f32>, String> {
        let len = self.ring.len() as u64;
        let fits = |end: u64| to <= end && end.saturating_sub(from) <= len && from >= self.captured_from.load(Ordering::Acquire);
        if !fits(self.captured_end.load(Ordering::Acquire)) {
            return Err("the input ring does not hold the positions asked for (the control thread fell behind)".into());
        }
        let out = (from..to).map(|p| self.ring.get((p % len) as usize).map_or(0.0, |s| f32::from_bits(s.load(Ordering::Relaxed)))).collect();
        if !fits(self.captured_end.load(Ordering::Acquire)) {
            return Err("the input ring overwrote positions while they were read (the control thread fell behind)".into());
        }
        Ok(out)
    }
}

impl Callbacks for Arc<Streams> {
    fn render(&self, _channel: usize) -> ab::RenderCallback {
        Box::new(move |block| block.render(&mut |chunk| chunk.fill(0.0)))
    }

    fn capture(&self, _channel: usize) -> ab::CaptureCallback {
        let streams = Arc::clone(self);
        Box::new(move |block| {
            let Some(position) = block.device_position else {
                streams.no_position.store(true, Ordering::Relaxed);
                return;
            };
            let (frames, channels, len) = (block.frames as u64, usize::from(block.channels).max(1), streams.ring.len() as u64);
            if len == 0 {
                return;
            }
            block.chunks(&mut |offset, samples| {
                for (i, frame) in samples.chunks_exact(channels).enumerate() {
                    let sum: f32 = streams.returns.iter().filter_map(|&c| frame.get(usize::from(c))).sum();
                    let at = position.saturating_add((offset + i) as u64);
                    if let Some(slot) = streams.ring.get((at % len) as usize) {
                        slot.store(sum.to_bits(), Ordering::Relaxed);
                    }
                }
            });
            let _ = streams.captured_from.compare_exchange(NONE, position, Ordering::Relaxed, Ordering::Relaxed);
            streams.captured_end.fetch_max(position.saturating_add(frames), Ordering::Release);
            // The block for the clock map: single writer, published by the count.
            let stamp = block.stamp;
            let written = streams.blocks_written.load(Ordering::Relaxed);
            if let Some(slot) = streams.blocks.get((written % BLOCKS as u64) as usize) {
                let valid = stamp.quality != ab::Quality::Invalid;
                slot[0].store(if valid { stamp.host_callback_ns } else { NONE }, Ordering::Relaxed);
                slot[1].store(stamp.backend_callback_ns as u64, Ordering::Relaxed);
                slot[2].store(frames, Ordering::Relaxed);
                slot[3].store(position, Ordering::Relaxed);
            }
            streams.blocks_written.store(written + 1, Ordering::Release);
        })
    }

    fn error(&self, _render: bool) -> ab::ErrorCallback {
        let streams = Arc::clone(self);
        Box::new(move |_| {
            streams.errors.fetch_add(1, Ordering::Relaxed);
        })
    }

    fn render_opened(&self, achieved: &ab::Achieved) -> Result<(), String> {
        self.render.set(*achieved).map_err(|_| "the output opened twice".to_string())
    }

    fn capture_opened(&self, achieved: &ab::Achieved) -> Result<(), String> {
        if let Some(c) = self.returns.iter().find(|&&c| c >= achieved.format.channels) {
            return Err(format!("the input opened with {} channels; --in {} does not exist", achieved.format.channels, c + 1));
        }
        self.capture.set(*achieved).map_err(|_| "the input opened twice".to_string())
    }
}

/// The position clock on the control thread: the capture blocks fitted as the app fits a stream.
struct Clock {
    fitter: ClockFitter,
    key: ClockKey,
    read: u64,
    rate: u32,
    lost: u64,
}

impl Clock {
    fn new(rate: u32) -> Self {
        let key = ClockKey { generation: 1, domain: 1, direction: ab::Direction::Input };
        Self { fitter: ClockFitter::new(key), key, read: 0, rate, lost: 0 }
    }

    /// Feed the blocks the capture callback published since the last call.
    fn drain(&mut self, streams: &Streams) -> Result<(), String> {
        if streams.no_position.load(Ordering::Relaxed) {
            return Err("the driver gives no sample positions; this measurement needs them (--backend asio)".into());
        }
        let written = streams.blocks_written.load(Ordering::Acquire);
        if written.saturating_sub(self.read) > BLOCKS as u64 {
            self.lost += written - self.read - BLOCKS as u64;
            self.read = written - BLOCKS as u64;
        }
        while self.read < written {
            let Some(slot) = streams.blocks.get((self.read % BLOCKS as u64) as usize) else { break };
            let values: [u64; 4] = std::array::from_fn(|i| slot[i].load(Ordering::Relaxed));
            // A slot the callback lapped while it was read is dropped, not fitted.
            if streams.blocks_written.load(Ordering::Acquire).saturating_sub(self.read) >= BLOCKS as u64 {
                self.lost += 1;
            } else if values[0] != NONE {
                let backend = values[1] as i64;
                // The position clock: the block's first position at its position time.
                let stamp = ab::Stamp {
                    host_callback_ns: values[0],
                    backend_callback_ns: backend,
                    backend_frame_ns: backend,
                    quality: ab::Quality::BackendEstimate,
                    offset_uncertainty_ns: None,
                };
                let frames = u32::try_from(values[2]).unwrap_or(u32::MAX);
                let _ = self.fitter.observe(Observation { key: self.key, first_frame: values[3], frames, nominal_rate: self.rate, stamp });
            }
            self.read += 1;
        }
        Ok(())
    }

    /// The sample position at host time `host_ns`.
    fn position_at(&self, host_ns: u64) -> Result<u64, String> {
        let map = self.fitter.map().ok_or("the clock map is not valid (the position clock did not fit)")?;
        map.host_to_frame(self.key, host_ns, host_ns).map_err(|e| format!("the send instant is outside the clock map ({e:?})"))
    }
}

fn frames_of(rate: u32, ms: u64) -> u64 {
    u64::from(rate) * ms / 1000
}

/// One trial's figures, in sample positions.
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    /// The send instant's position.
    pub send: u64,
    /// The onset's position (the refined crossing).
    pub onset: u64,
    /// The floor threshold's crossing.
    pub crossing: u64,
    /// Envelope peak of the pre-send window, threshold, and the attack's peak.
    pub pre_send: f32,
    pub threshold: f32,
    pub peak: f32,
    /// Time the send call took, in ns (not part of the latency; for the record).
    pub send_call_ns: u64,
}

impl Trial {
    /// L, in samples.
    pub fn latency(&self) -> u64 {
        self.onset - self.send
    }
    /// The detector's uncertainty: the distance between the two crossings, in samples.
    pub fn uncertainty(&self) -> u64 {
        self.onset.abs_diff(self.crossing)
    }
}

/// The distribution of the trials' latencies, in samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub mean: f64,
    pub median: f64,
    pub min: u64,
    pub max: u64,
    /// Sample standard deviation (0 for one trial).
    pub std_dev: f64,
    /// 1-based trial numbers further than [`OUTLIER_SIGMAS`] standard deviations from the mean.
    pub outliers: Vec<usize>,
}

/// Mean, median, min, max, standard deviation and outliers of `latencies` (None when empty).
pub fn summarize(latencies: &[u64]) -> Option<Summary> {
    let (&min, &max) = (latencies.iter().min()?, latencies.iter().max()?);
    let n = latencies.len();
    let mean = latencies.iter().map(|&l| l as f64).sum::<f64>() / n as f64;
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    let median = if n % 2 == 1 { sorted[n / 2] as f64 } else { (sorted[n / 2 - 1] + sorted[n / 2]) as f64 / 2.0 };
    let std_dev = if n > 1 { (latencies.iter().map(|&l| (l as f64 - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt() } else { 0.0 };
    let outliers = latencies
        .iter()
        .enumerate()
        .filter(|(_, l)| std_dev > 0.0 && (**l as f64 - mean).abs() > OUTLIER_SIGMAS * std_dev)
        .map(|(i, _)| i + 1)
        .collect();
    Some(Summary { n, mean, median, min, max, std_dev, outliers })
}

/// A measurement's result.
#[derive(Debug, Clone)]
pub struct Report {
    pub device: String,
    pub midi_port: String,
    pub settings: Settings,
    pub rate: u32,
    /// The buffer the driver runs, where it says.
    pub buffer: Option<u32>,
    /// The driver's reported input latency, in samples.
    pub input_latency: Option<u64>,
    /// The reference noise floor (envelope peak), taken before the first note.
    pub floor: f32,
    pub trials: Vec<Trial>,
    /// Stream error callbacks and capture blocks lost to the clock map during the run.
    pub stream_errors: u64,
    pub lost_blocks: u64,
}

impl Report {
    pub fn summary(&self) -> Option<Summary> {
        summarize(&self.trials.iter().map(Trial::latency).collect::<Vec<_>>())
    }

    /// Samples as ms at the run's rate.
    pub fn ms(&self, samples: f64) -> f64 {
        samples * 1000.0 / f64::from(self.rate.max(1))
    }

    /// The mean minus the driver's reported input latency: an estimate of MIDI delivery plus synth response.
    pub fn delivery_estimate(&self) -> Option<f64> {
        Some(self.summary()?.mean - self.input_latency? as f64)
    }

    /// The report as printed.
    pub fn lines(&self) -> Vec<String> {
        let s = &self.settings;
        let returns = s.returns.iter().map(|c| (c + 1).to_string()).collect::<Vec<_>>().join("+");
        let buffer = self.buffer.map_or("the driver's".to_string(), |b| b.to_string());
        let mut lines = vec![
            "MIDI-to-audio onset latency (MIDI delivery + synth response + audio input; not MIDI transport accuracy)".to_string(),
            format!(
                "MIDI \"{}\" channel {} note {} velocity {} -> \"{}\" input {returns}, {} Hz, buffer {buffer}, floor {}",
                self.midi_port,
                s.channel + 1,
                s.note,
                s.velocity,
                self.device,
                self.rate,
                onset::dbfs(self.floor)
            ),
        ];
        for (k, t) in self.trials.iter().enumerate() {
            lines.push(format!(
                "trial {:>2}: {} samples ({:.2} ms), detector +-{} samples, pre-send {}, threshold {}, attack peak {}",
                k + 1,
                t.latency(),
                self.ms(t.latency() as f64),
                t.uncertainty(),
                onset::dbfs(t.pre_send),
                onset::dbfs(t.threshold),
                onset::dbfs(t.peak)
            ));
        }
        if let Some(m) = self.summary() {
            lines.push(format!(
                "effective compensation offset for this chain: mean {:.1} samples ({:.2} ms), median {:.1} ({:.2} ms), min {} ({:.2} ms), max {} ({:.2} ms), std dev {:.1} samples ({:.2} ms), n = {}",
                m.mean,
                self.ms(m.mean),
                m.median,
                self.ms(m.median),
                m.min,
                self.ms(m.min as f64),
                m.max,
                self.ms(m.max as f64),
                m.std_dev,
                self.ms(m.std_dev),
                m.n
            ));
            let outliers = if m.outliers.is_empty() {
                "none".to_string()
            } else {
                m.outliers.iter().map(|k| format!("trial {k}")).collect::<Vec<_>>().join(", ")
            };
            lines.push(format!("outliers (beyond {OUTLIER_SIGMAS} std dev): {outliers}"));
            let widths: Vec<u64> = self.trials.iter().map(Trial::uncertainty).collect();
            let worst = widths.iter().copied().max().unwrap_or(0);
            let mean = widths.iter().sum::<u64>() as f64 / widths.len().max(1) as f64;
            lines.push(format!(
                "detector uncertainty (floor-threshold crossing to 10%-of-peak crossing): mean {mean:.1} samples, max {worst} samples ({:.2} ms)",
                self.ms(worst as f64)
            ));
        }
        match (self.input_latency, self.delivery_estimate()) {
            (Some(input), Some(estimate)) => lines.push(format!(
                "minus the driver's reported input latency ({input} samples, {:.2} ms) = estimated MIDI delivery + synth response: {estimate:.1} samples ({:.2} ms); an estimate: the driver's figure is unverified (K31)",
                self.ms(input as f64),
                self.ms(estimate)
            )),
            _ => lines.push("the driver reports no input latency: no estimate of MIDI delivery + synth response".to_string()),
        }
        if self.stream_errors > 0 || self.lost_blocks > 0 {
            lines.push(format!("notes: {} stream errors, {} capture blocks lost to the clock map", self.stream_errors, self.lost_blocks));
        }
        lines
    }

    /// The result as JSON, every trial included; `command` is the command line that produced it.
    pub fn json(&self, command: &str, date: &str, machine: &crate::report::Machine) -> serde_json::Value {
        let summary = self.summary().map(|m| {
            serde_json::json!({
                "n": m.n, "mean_samples": m.mean, "median_samples": m.median, "min_samples": m.min, "max_samples": m.max,
                "std_dev_samples": m.std_dev, "mean_ms": self.ms(m.mean), "median_ms": self.ms(m.median),
                "min_ms": self.ms(m.min as f64), "max_ms": self.ms(m.max as f64), "std_dev_ms": self.ms(m.std_dev),
                "outlier_trials": m.outliers,
            })
        });
        let trials: Vec<_> = self
            .trials
            .iter()
            .enumerate()
            .map(|(k, t)| {
                serde_json::json!({
                    "trial": k + 1, "send_position": t.send, "onset_position": t.onset, "threshold_crossing_position": t.crossing,
                    "latency_samples": t.latency(), "latency_ms": self.ms(t.latency() as f64), "detector_uncertainty_samples": t.uncertainty(),
                    "pre_send_envelope_peak": t.pre_send, "threshold": t.threshold, "attack_peak": t.peak, "send_call_ns": t.send_call_ns,
                })
            })
            .collect();
        serde_json::json!({
            "measurement": "MIDI-to-audio onset latency",
            "includes": "MIDI delivery + synth response + audio input; not MIDI transport accuracy",
            "command": command, "date": date, "cpu": machine.cpu, "os": machine.os,
            "device": self.device, "midi_port": self.midi_port,
            "midi_channel": self.settings.channel + 1, "note": self.settings.note, "velocity": self.settings.velocity,
            "inputs": self.settings.returns.iter().map(|c| c + 1).collect::<Vec<_>>(),
            "gap_ms": self.settings.gap_ms, "max_ms": self.settings.max_ms,
            "rate": self.rate, "buffer": self.buffer, "noise_floor_envelope_peak": self.floor,
            "effective_compensation_offset": summary,
            "driver_input_latency_samples": self.input_latency,
            "estimated_delivery_plus_synth_samples": self.delivery_estimate(),
            "trials": trials,
            "stream_errors": self.stream_errors, "lost_blocks": self.lost_blocks,
        })
    }
}

/// Open `request`'s device duplex through `backend`, then play `settings.trials` notes into
/// `midi` and time each one's onset on the return. Any trial without an onset, a return not at its
/// noise floor before a send, a stalled input or an error from `driver` ends the run with an
/// error; the note is released on every path.
pub fn measure(
    backend: &dyn ab::Backend,
    request: &Request,
    settings: &Settings,
    midi: &mut dyn MidiSink,
    driver: &mut dyn Driver,
) -> Result<Report, String> {
    settings.check()?;
    let streams = Arc::new(Streams::new(request.rate, settings));
    let highest = settings.returns.iter().copied().max().unwrap_or(0);
    let request = Request { loopback: true, in_ch: highest.saturating_add(1), out_ch: 1, ..request.clone() };
    let midi_port = midi.name();
    let mut outcome = None;
    let opened = session::run_with(backend, &request, &streams, &mut || {
        outcome = Some(trials(&streams, settings, &midi_port, &mut *midi, &mut *driver)?);
        Ok(())
    })?;
    let (floor, trials, lost_blocks) = outcome.ok_or("the measurement did not run")?;
    let rate = opened.render_rate;
    let input_latency = opened.capture.and_then(|c| c.latency).map(|d| (d.as_secs_f64() * f64::from(rate)).round() as u64);
    Ok(Report {
        device: opened.device,
        midi_port,
        settings: settings.clone(),
        rate,
        buffer: opened.render.period.frames(),
        input_latency,
        floor,
        trials,
        stream_errors: streams.errors.load(Ordering::Relaxed),
        lost_blocks,
    })
}

/// Wait `ns` while the clock keeps fitting.
fn pass(driver: &mut dyn Driver, clock: &mut Clock, streams: &Streams, ns: u64) -> Result<(), String> {
    let until = driver.now_ns().saturating_add(ns);
    loop {
        clock.drain(streams)?;
        if driver.now_ns() >= until {
            return Ok(());
        }
        driver.wait(POLL_NS)?;
    }
}

/// The trials, on the control thread while the streams run: the reference floor and the trials.
fn trials(
    streams: &Streams,
    settings: &Settings,
    port: &str,
    midi: &mut dyn MidiSink,
    driver: &mut dyn Driver,
) -> Result<(f32, Vec<Trial>, u64), String> {
    // First, so that every exit below releases the note.
    let mut notes = Notes { midi, channel: settings.channel, note: settings.note };
    let rate = streams.render.get().map(|a| a.format.rate).ok_or("the output did not open")?;
    if streams.capture.get().map(|a| a.format.rate) != Some(rate) {
        return Err("the input did not open at the output's rate".into());
    }
    let window_ms = u64::from(settings.max_ms) + u64::from(onset::PEAK_MS) + u64::from(FLOOR_MS);
    if frames_of(rate, window_ms + 1000) > streams.ring.len() as u64 {
        return Err(format!(
            "the device opened at {rate} Hz, above the {} Hz asked; pass --rate {rate}",
            streams.ring.len() as u64 / u64::from(RING_SECONDS)
        ));
    }
    let w = onset::window(rate);
    let floor_frames = frames_of(rate, u64::from(FLOOR_MS));
    let mut clock = Clock::new(rate);
    // Warm-up: a clock map and a floor's worth of input.
    let started = driver.now_ns();
    loop {
        clock.drain(streams)?;
        let from = streams.captured_from.load(Ordering::Acquire);
        let end = streams.captured_end.load(Ordering::Acquire);
        if clock.fitter.map().is_some() && from != NONE && end.saturating_sub(from) >= floor_frames {
            break;
        }
        if driver.now_ns().saturating_sub(started) > WARMUP_MS * 1_000_000 {
            return Err(format!(
                "no clock map and input after {WARMUP_MS} ms (clock fit: {:?}); the driver may not be running",
                clock.fitter.counts()
            ));
        }
        driver.wait(POLL_NS)?;
    }
    let end = streams.captured_end.load(Ordering::Acquire);
    let reference = onset::noise_floor(&streams.read(end - floor_frames, end)?, w);
    let threshold = onset::threshold(reference)?;
    let guard = (reference.peak * onset::QUIET_FACTOR).min(threshold);
    let mut out = Vec::new();
    for k in 1..=settings.trials {
        let fail = |e: String| format!("trial {k}: {e}");
        clock.drain(streams)?;
        let end = streams.captured_end.load(Ordering::Acquire);
        let pre = onset::noise_floor(&streams.read(end - floor_frames, end).map_err(fail)?, w);
        onset::at_floor(reference, pre).map_err(fail)?;
        let sent = driver.now_ns();
        notes.on(settings.velocity).map_err(fail)?;
        let send_call_ns = driver.now_ns().saturating_sub(sent);
        let send = clock.position_at(sent).map_err(fail)?;
        // The scan keeps one envelope window of samples before the send.
        let base = send.saturating_sub(w as u64);
        let stop = send + frames_of(rate, u64::from(settings.max_ms));
        let peak_len = usize::try_from(frames_of(rate, u64::from(onset::PEAK_MS))).unwrap_or(usize::MAX);
        let deadline = sent.saturating_add((u64::from(settings.max_ms) + STALL_MS) * 1_000_000);
        let mut samples: Vec<f32> = Vec::new();
        let (from, mut scanned, mut crossing) = ((send - base) as usize, (send - base) as usize, None);
        loop {
            clock.drain(streams)?;
            let want = crossing.map_or(stop, |c: usize| base + (c + peak_len) as u64);
            let to = streams.captured_end.load(Ordering::Acquire).min(want);
            let have = base + samples.len() as u64;
            if to > have {
                samples.extend(streams.read(have, to).map_err(fail)?);
            }
            if crossing.is_none() {
                crossing = onset::first_crossing(&samples, scanned, w, threshold);
                scanned = samples.len().max(from);
            }
            if base + samples.len() as u64 >= crossing.map_or(stop, |c| base + (c + peak_len) as u64) {
                break;
            }
            if driver.now_ns() > deadline {
                return Err(fail("the input stopped delivering samples".into()));
            }
            driver.wait(POLL_NS)?;
        }
        notes.off().map_err(fail)?;
        let Some(c) = crossing else {
            let heard = samples.get(from..).map_or(0.0, |s| onset::noise_floor(s, w).peak);
            let returns = settings.returns.iter().map(|c| (c + 1).to_string()).collect::<Vec<_>>().join("+");
            return Err(fail(format!(
                "no onset within {} ms of the note-on: the return peaked at {} against a threshold of {} (floor {}). Check that the synth plays on MIDI channel {} from \"{port}\", that its audio reaches input {returns}, and its level",
                settings.max_ms,
                onset::dbfs(heard),
                onset::dbfs(threshold),
                onset::dbfs(reference.peak),
                settings.channel + 1,
            )));
        };
        let found = onset::refine(&samples, from, c, w, peak_len, guard);
        out.push(Trial {
            send,
            onset: base + found.at as u64,
            crossing: base + found.crossing as u64,
            pre_send: pre.peak,
            threshold,
            peak: found.peak,
            send_call_ns,
        });
        if k < settings.trials {
            pass(driver, &mut clock, streams, u64::from(settings.gap_ms) * 1_000_000)?;
        }
    }
    drop(notes);
    Ok((reference.peak, out, clock.lost))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_lists_mean_median_spread_and_outliers() {
        let m = summarize(&[100, 102, 98, 100, 101, 99, 100, 140, 100, 100]).unwrap();
        assert_eq!((m.n, m.min, m.max, m.median), (10, 98, 140, 100.0));
        assert!((m.mean - 104.0).abs() < 1e-9);
        assert!(m.std_dev > 12.0 && m.std_dev < 13.0, "{m:?}");
        assert_eq!(m.outliers, vec![8]);
        let one = summarize(&[5]).unwrap();
        assert_eq!((one.std_dev, one.outliers.len(), one.median), (0.0, 0, 5.0));
        assert_eq!(summarize(&[1, 2]).unwrap().median, 1.5);
        assert!(summarize(&[]).is_none());
    }
}
