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
//! right before the send call, mapped to the device's sample position by the bench's position
//! clock ([`crate::fit`]): a least-squares line of the capture blocks' arrival time against their
//! sample position over the last 2 s, the same reference as the app's ClockFitter input map (device
//! sample position against capture-callback arrival time) with a longer window. The app's MIDI
//! output thread schedules on the output presentation map, which slice 3 of #71 has to reconcile.
//! The app's own fitter is fed the same blocks and reported beside it.
//! The position clock is used, not the presentation one: no audio output is in this chain. The
//! input is recorded by sample position, so the onset is a position too, and both ends count the
//! same driver counter. What is not measured is when the OS and the interface actually transmit
//! the bytes.
//!
//! Before the first trial the bench plays one unmeasured warm-up note (`--no-warmup` skips it).
//! The noise floor is the lowest of the reference window and every trial's pre-send window, and
//! each trial's onset is found again against it at the end.
//!
//! The callbacks only store atomics into storage allocated before the streams start: no
//! allocation, no lock. Analysis and every MIDI send run on the control thread.

use crate::fit::{self, PositionFit};
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
/// How long the unmeasured warm-up note is held.
pub const WARMUP_NOTE_MS: u64 = 200;
/// Seconds of input the position ring holds beyond the measured window.
pub(crate) const RING_SECONDS: u32 = 4;
/// Capture blocks the control thread can fall behind by before blocks are lost to the clock map.
const BLOCKS: usize = 4096;
/// The control thread's poll interval.
pub(crate) const POLL_NS: u64 = 1_000_000;
/// Extra time past the onset window before a silent input counts as stalled.
pub(crate) const STALL_MS: u64 = 2000;
/// Defaults of the command line.
pub const DEFAULT_NOTE: u8 = 60;
pub const DEFAULT_VELOCITY: u8 = 100;
pub const DEFAULT_TRIALS: u32 = 10;
pub const DEFAULT_GAP_MS: u32 = 1500;
pub const DEFAULT_MAX_MS: u32 = 1000;
/// A trial further than this many standard deviations from the mean is listed as an outlier.
pub const OUTLIER_SIGMAS: f64 = 2.0;
/// The default limit of the clock fit's standard error at a send (`--max-fit-error-ms`), in ns.
/// A send above it is skipped. A sanity limit: a driver whose buffers arrive one block apart at
/// buffer 256 (5.3 ms of scatter) fits to about 0.3 ms over the 2 s history.
pub const DEFAULT_MAX_FIT_ERROR_NS: u64 = 500_000;
/// The warm-up waits for a clock fit over at least this much history.
pub const WARMUP_FIT_SPAN_NS: u64 = 1_000_000_000;
/// A pre-send window more than this many times the floor (+6 dB) gets a warning (and a retry).
pub const PRE_SEND_WARN_FACTOR: f32 = onset::QUIET_FACTOR;
/// An onset this far after its threshold crossing gets a slow-attack warning, in ms.
pub const SLOW_ATTACK_MS: f64 = 1.0;

pub(crate) const NONE: u64 = u64::MAX;

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
    /// The clock fit's standard error at the send above which a trial is skipped, in ns.
    pub max_fit_error_ns: u64,
    /// 0-based input channels of the return, summed (one for mono, two for a stereo pair).
    pub returns: Vec<u16>,
    /// Play one unmeasured note before the first trial (`--no-warmup` turns it off).
    pub warmup: bool,
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
            max_fit_error_ns: DEFAULT_MAX_FIT_ERROR_NS,
            returns,
            warmup: true,
        }
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        if self.channel > 15 || self.note > 127 || !(1..=127).contains(&self.velocity) {
            return Err("MIDI channel 1-16, note 0-127 and velocity 1-127".into());
        }
        if self.trials == 0 || self.max_ms == 0 || self.returns.is_empty() {
            return Err("--trials, --max-ms and --in need at least one".into());
        }
        Ok(())
    }

    /// The return channels, 1-based, as printed.
    pub(crate) fn returns_label(&self) -> String {
        self.returns.iter().map(|c| (c + 1).to_string()).collect::<Vec<_>>().join("+")
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
    /// Let about `ns` pass while the streams run (a wait at or under [`crate::placement::SPIN_NS`]
    /// yields instead of sleeping, as the MIDI output thread does before a deadline). An error
    /// (Ctrl+C) ends the measurement; every note is released on the way out.
    fn wait(&mut self, ns: u64) -> Result<(), String>;
}

/// What Ctrl+C and the control thread share: the stop request, and whether a note-on may be
/// outstanding. The Ctrl+C handler only sets flags; it may end the process at once only while no
/// note can be sounding ([`Interrupt::ctrl_c`]). The note's guard sets `note_on` before it sends a
/// note-on and clears it once the note-off and All Notes Off went out.
#[derive(Debug, Default)]
pub struct Interrupt {
    stop: AtomicBool,
    note_on: AtomicBool,
    release_failed: AtomicBool,
}

impl Interrupt {
    /// Ctrl+C was pressed: request the stop. True when the process may exit right now: on a second
    /// press, and only while no note-on is outstanding. Otherwise the control thread sees the stop
    /// at its next poll, releases the note and ends the run. Sequentially consistent with
    /// [`Interrupt::arm_note`]: either the handler sees the note outstanding, or the control
    /// thread sees the stop and does not send.
    pub fn ctrl_c(&self) -> bool {
        let again = self.stop.swap(true, Ordering::SeqCst);
        again && !self.note_on.load(Ordering::SeqCst)
    }

    /// Whether a stop was requested.
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Whether a note-on may be outstanding.
    pub fn note_outstanding(&self) -> bool {
        self.note_on.load(Ordering::SeqCst)
    }

    /// Before a note-on: mark it outstanding, then refuse when a stop was requested (Ctrl+C during
    /// the device open or the warm-up included).
    fn arm_note(&self) -> Result<(), String> {
        self.note_on.store(true, Ordering::SeqCst);
        if self.stopped() { Err("interrupted".into()) } else { Ok(()) }
    }

    fn note_released(&self) {
        self.note_on.store(false, Ordering::SeqCst);
    }

    pub(crate) fn release_failed(&self) -> bool {
        self.release_failed.load(Ordering::SeqCst)
    }
}

/// Real time: [`ab::HostOrigin`] (the origin the backends stamp in) and sleeps, ended by a stop
/// request of `interrupt`.
pub struct RealTime<'a> {
    pub origin: ab::HostOrigin,
    pub interrupt: &'a Interrupt,
}

impl Driver for RealTime<'_> {
    fn now_ns(&mut self) -> u64 {
        self.origin.now_ns().unwrap_or(0)
    }
    fn wait(&mut self, ns: u64) -> Result<(), String> {
        if self.interrupt.stopped() {
            return Err("interrupted".into());
        }
        if ns <= crate::placement::SPIN_NS {
            std::thread::yield_now();
        } else {
            std::thread::sleep(std::time::Duration::from_nanos(ns.min(10_000_000)));
        }
        if self.interrupt.stopped() { Err("interrupted".into()) } else { Ok(()) }
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
pub(crate) struct Notes<'a> {
    pub(crate) midi: &'a mut dyn MidiSink,
    pub(crate) interrupt: &'a Interrupt,
    pub(crate) channel: u8,
    pub(crate) note: u8,
}

impl Notes<'_> {
    pub(crate) fn on(&mut self, velocity: u8) -> Result<(), String> {
        self.interrupt.arm_note()?;
        self.midi.send(&[0x90 | self.channel, self.note, velocity])
    }
    pub(crate) fn off(&mut self) -> Result<(), String> {
        self.midi.send(&[0x80 | self.channel, self.note, 0])?;
        self.interrupt.note_released();
        Ok(())
    }
}

impl Drop for Notes<'_> {
    fn drop(&mut self) {
        let off = self.midi.send(&[0x80 | self.channel, self.note, 0]);
        let all = self.midi.send(&[0xB0 | self.channel, 123, 0]);
        if let Err(e) = off.and(all) {
            self.interrupt.release_failed.store(true, Ordering::SeqCst);
            eprintln!("audio-bench: releasing the note failed ({e}); check that the synth is silent");
        }
        // Both messages were tried: a further Ctrl+C may end the process now.
        self.interrupt.note_released();
    }
}

/// What the callbacks share with the control thread. Allocated before the streams start.
pub(crate) struct Streams {
    /// The rate the ring was sized for.
    asked_rate: u32,
    /// The return by sample position: `f32` bits at `position % len`.
    ring: Box<[AtomicU32]>,
    /// The first position recorded, [`NONE`] before any.
    pub(crate) captured_from: AtomicU64,
    /// One past the last position recorded (0 before any).
    pub(crate) captured_end: AtomicU64,
    /// Capture blocks for the clock map: host, position time, frame count and position.
    blocks: Box<[[AtomicU64; 4]]>,
    blocks_written: AtomicU64,
    no_position: AtomicBool,
    returns: Box<[u16]>,
    pub(crate) render: OnceLock<ab::Achieved>,
    pub(crate) capture: OnceLock<ab::Achieved>,
    pub(crate) errors: AtomicU64,
}

impl Streams {
    /// A ring of `ring_ms` of input at `rate`.
    pub(crate) fn new(rate: u32, returns: &[u16], ring_ms: u64) -> Self {
        let len = u64::from(rate) * ring_ms / 1000;
        Self {
            asked_rate: rate,
            ring: (0..len).map(|_| AtomicU32::new(0)).collect(),
            captured_from: AtomicU64::new(NONE),
            captured_end: AtomicU64::new(0),
            blocks: (0..BLOCKS).map(|_| std::array::from_fn(|_| AtomicU64::new(0))).collect(),
            blocks_written: AtomicU64::new(0),
            no_position: AtomicBool::new(false),
            returns: returns.to_vec().into_boxed_slice(),
            render: OnceLock::new(),
            capture: OnceLock::new(),
            errors: AtomicU64::new(0),
        }
    }

    /// Positions `from..to` of the return. An error when they are not all recorded or the ring has
    /// overwritten some (the control thread fell behind).
    pub(crate) fn read(&self, from: u64, to: u64) -> Result<Vec<f32>, String> {
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

/// The clock fit's figures at one send.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SendFit {
    /// Largest distance of a block from the line (not a bound on the send's offset), ns.
    pub scatter_ns: f64,
    /// Standard deviation of the blocks about the line, ns.
    pub sigma_ns: f64,
    /// The line's standard error at the send position, ns: the send position's uncertainty.
    pub error_ns: f64,
    /// The newest block against the line fitted without it, ns.
    pub leave_last_out_ns: f64,
    /// Blocks in the line.
    pub points: usize,
    /// The app's clock map (ClockFitter, fed the same blocks) minus the bench's position at the
    /// send, in samples; None while the app's map is not valid.
    pub app_offset: Option<i64>,
}

/// The position clock on the control thread: the capture blocks fitted by the bench
/// ([`PositionFit`]) and, for comparison, by the app's fitter.
pub(crate) struct Clock {
    fitter: ClockFitter,
    key: ClockKey,
    pub(crate) fit: PositionFit,
    read: u64,
    rate: u32,
    pub(crate) lost: u64,
    pub(crate) blocks: u64,
}

impl Clock {
    pub(crate) fn new(rate: u32) -> Self {
        let key = ClockKey { generation: 1, domain: 1, direction: ab::Direction::Input };
        Self { fitter: ClockFitter::new(key), key, fit: PositionFit::new(rate), read: 0, rate, lost: 0, blocks: 0 }
    }

    /// Feed the blocks the capture callback published since the last call.
    pub(crate) fn drain(&mut self, streams: &Streams) -> Result<(), String> {
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
            } else if values[0] == NONE {
                // An invalid stamp: the driver's position anchor moved.
                self.fit.restart();
            } else {
                self.blocks += 1;
                let backend = values[1] as i64;
                // The app's position clock: the block's first position at its position time.
                let stamp = ab::Stamp {
                    host_callback_ns: values[0],
                    backend_callback_ns: backend,
                    backend_frame_ns: backend,
                    quality: ab::Quality::BackendEstimate,
                    offset_uncertainty_ns: None,
                };
                let frames = u32::try_from(values[2]).unwrap_or(u32::MAX);
                let _ = self.fitter.observe(Observation { key: self.key, first_frame: values[3], frames, nominal_rate: self.rate, stamp });
                self.fit.push(values[3], values[2], values[0]);
            }
            self.read += 1;
        }
        Ok(())
    }

    /// The bench's line, or why there is none.
    pub(crate) fn line(&self) -> Result<fit::Line, String> {
        self.fit.line().map_err(|f| format!("the position clock did not fit: {f}"))
    }

    /// The sample position at host time `host_ns`, and the fit's figures there.
    pub(crate) fn position_at(&self, host_ns: u64) -> Result<(u64, SendFit), String> {
        let line = self.line()?;
        let position = line.host_to_frame(host_ns)?;
        let app_offset = self.fitter.map().and_then(|m| m.host_to_frame(self.key, host_ns, host_ns).ok()).map(|a| a as i64 - position as i64);
        Ok((
            position,
            SendFit {
                scatter_ns: line.scatter_ns,
                sigma_ns: line.sigma_ns,
                error_ns: line.error_at_ns(position),
                leave_last_out_ns: line.leave_last_out_ns,
                points: line.n,
                app_offset,
            },
        ))
    }

    /// The host time of sample position `frame` by the bench's line.
    pub(crate) fn frame_to_host(&self, frame: u64) -> Result<u64, String> {
        Ok(self.line()?.frame_to_host(frame))
    }
}

pub(crate) fn frames_of(rate: u32, ms: u64) -> u64 {
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
    /// Time the send call took, in ns (inside L, since the instant is read before the call).
    pub send_call_ns: u64,
    /// The clock fit at the send.
    pub fit: SendFit,
    /// The pre-send window was above the floor and the trial waited another gap.
    pub retried: bool,
}

impl Trial {
    /// L, in samples.
    pub fn latency(&self) -> u64 {
        self.onset - self.send
    }
    /// The detector's uncertainty: how far the onset lies after the threshold crossing, in samples.
    /// Late side only: the true start, before the crossing, is not bounded.
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

/// The lines on the clock fit both modes print: its scatter (not a bound), the send position's
/// standard error, the leave-last-out error and the app's map beside the bench's.
pub(crate) fn fit_lines(rate: u32, fits: &[SendFit], max_fit_error_ns: u64) -> Vec<String> {
    let samples = |ns: f64| ns * f64::from(rate) / 1e9;
    let max = |f: fn(&SendFit) -> f64| fits.iter().map(f).fold(0.0f64, f64::max);
    let (scatter, sigma, error, loo) = (max(|f| f.scatter_ns), max(|f| f.sigma_ns), max(|f| f.error_ns), max(|f| f.leave_last_out_ns.abs()));
    let points = fits.iter().map(|f| f.points).min().unwrap_or(0);
    let mut lines = vec![
        format!(
            "clock-fit scatter (not a bound on the send offset): up to {:.1} us ({:.1} samples), standard deviation up to {:.1} us, over at least {points} capture blocks (the last {} s, at least {})",
            scatter / 1000.0,
            samples(scatter),
            sigma / 1000.0,
            fit::HISTORY_NS / 1_000_000_000,
            fit::KEEP_POINTS
        ),
        format!(
            "send-position uncertainty from the clock fit: standard error at the send up to +-{:.1} us ({:.2} samples); leave-last-out prediction error up to {:.1} us ({:.1} samples); a send above {:.2} ms standard error is skipped",
            error / 1000.0,
            samples(error),
            loo / 1000.0,
            samples(loo),
            max_fit_error_ns as f64 / 1e6
        ),
    ];
    let valid: Vec<i64> = fits.iter().filter_map(|f| f.app_offset).collect();
    let largest = valid.iter().map(|d| d.unsigned_abs()).max();
    lines.push(match largest {
        Some(d) => format!(
            "the app's clock map (ClockFitter, the same blocks): valid at {} of {} sends, at most {d} samples from the bench fit",
            valid.len(),
            fits.len()
        ),
        None => format!(
            "the app's clock map (ClockFitter, the same blocks): valid at 0 of {} sends (its window or its 2 ms residual limit refuses this driver's arrivals)",
            fits.len()
        ),
    });
    lines
}

/// The detector's lines both modes print: its late-side uncertainty and its bias.
pub(crate) fn detector_lines(rate: u32, envelope_window: usize, widths: &[u64]) -> Vec<String> {
    let ms = |samples: f64| samples * 1000.0 / f64::from(rate.max(1));
    let worst = widths.iter().copied().max().unwrap_or(0);
    let mean = widths.iter().sum::<u64>() as f64 / widths.len().max(1) as f64;
    let lag = envelope_window.saturating_sub(1);
    vec![
        format!(
            "detector uncertainty (late side only: onset after the floor-threshold crossing; the true start before the crossing is not bounded): mean {mean:.1} samples, max {worst} samples ({:.2} ms)",
            ms(worst as f64)
        ),
        format!(
            "detector onset bias: late, by the attack's rise to 10% of its peak plus up to {lag} samples ({:.2} ms) of envelope lag; included in L, nothing subtracted",
            ms(lag as f64)
        ),
    ]
}

/// The JSON of one send's fit.
pub(crate) fn fit_json(fit: &SendFit) -> serde_json::Value {
    serde_json::json!({
        "fit_scatter_ns": fit.scatter_ns, "fit_sigma_ns": fit.sigma_ns, "fit_error_ns": fit.error_ns,
        "fit_leave_last_out_ns": fit.leave_last_out_ns, "fit_points": fit.points, "app_clock_offset_samples": fit.app_offset,
    })
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
    /// The noise floor (envelope peak): the lowest of the reference window and the pre-send windows.
    pub floor: f32,
    /// The reference window's envelope peak, taken before the first note.
    pub reference_floor: f32,
    pub trials: Vec<Trial>,
    /// The detector's envelope window, in samples: the onset lags the attack by up to one less.
    pub envelope_window: usize,
    /// Trials skipped because the clock fit's standard error was above the limit (no note was sent).
    pub skipped: u32,
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

    /// The largest standard error of the clock fit at a send, in ns.
    pub fn fit_error_max_ns(&self) -> f64 {
        self.trials.iter().map(|t| t.fit.error_ns).fold(0.0f64, f64::max)
    }

    /// [`Report::fit_error_max_ns`] in samples: the send position's uncertainty.
    pub fn send_uncertainty_samples(&self) -> f64 {
        self.fit_error_max_ns() * f64::from(self.rate) / 1e9
    }

    /// The report as printed.
    pub fn lines(&self) -> Vec<String> {
        let s = &self.settings;
        let buffer = self.buffer.map_or("the driver's".to_string(), |b| b.to_string());
        let mut lines = vec![
            "MIDI-to-audio onset latency (MIDI delivery + synth response + audio input; not MIDI transport accuracy)".to_string(),
            format!("timing reference: {}", crate::fit::TIMING_REFERENCE),
            format!(
                "MIDI \"{}\" channel {} note {} velocity {} -> \"{}\" input {}, {} Hz, buffer {buffer}, floor {} (lowest of the reference window {} and {} pre-send windows), warm-up note {}",
                self.midi_port,
                s.channel + 1,
                s.note,
                s.velocity,
                self.device,
                s.returns_label(),
                self.rate,
                onset::dbfs(self.floor),
                onset::dbfs(self.reference_floor),
                self.trials.len(),
                if s.warmup { "sent, not measured" } else { "off" }
            ),
        ];
        for (k, t) in self.trials.iter().enumerate() {
            lines.push(format!(
                "trial {:>2}: {} samples ({:.2} ms), onset {} samples after the threshold crossing, fit error +-{:.1} us, pre-send {}, threshold {}, attack peak {}",
                k + 1,
                t.latency(),
                self.ms(t.latency() as f64),
                t.uncertainty(),
                t.fit.error_ns / 1000.0,
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
            lines.extend(detector_lines(self.rate, self.envelope_window, &self.trials.iter().map(Trial::uncertainty).collect::<Vec<_>>()));
            for (k, t) in self.trials.iter().enumerate() {
                if self.ms(t.uncertainty() as f64) > SLOW_ATTACK_MS {
                    lines.push(format!(
                        "warning: trial {}'s onset is {:.2} ms after its threshold crossing: a slow attack; use the synth's fastest attack",
                        k + 1,
                        self.ms(t.uncertainty() as f64)
                    ));
                }
            }
            lines.extend(fit_lines(self.rate, &self.trials.iter().map(|t| t.fit).collect::<Vec<_>>(), s.max_fit_error_ns));
        }
        for (k, t) in self.trials.iter().enumerate() {
            if let Some(line) = pre_send_warning(&format!("trial {}", k + 1), t.pre_send, self.floor, t.retried) {
                lines.push(line);
            }
        }
        if self.skipped > 0 {
            lines.push(format!(
                "skipped: {} of {} trials (clock fit standard error above {:.2} ms; no note sent)",
                self.skipped,
                s.trials,
                s.max_fit_error_ns as f64 / 1e6
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
                let mut trial = serde_json::json!({
                    "trial": k + 1, "send_position": t.send, "onset_position": t.onset, "threshold_crossing_position": t.crossing,
                    "latency_samples": t.latency(), "latency_ms": self.ms(t.latency() as f64), "detector_uncertainty_samples": t.uncertainty(),
                    "pre_send_envelope_peak": t.pre_send, "pre_send_retried": t.retried, "threshold": t.threshold, "attack_peak": t.peak,
                    "send_call_ns": t.send_call_ns,
                });
                merge(&mut trial, fit_json(&t.fit));
                trial
            })
            .collect();
        let fits: Vec<SendFit> = self.trials.iter().map(|t| t.fit).collect();
        serde_json::json!({
            "measurement": "MIDI-to-audio onset latency",
            "includes": "MIDI delivery + synth response + audio input; not MIDI transport accuracy",
            "timing_reference": crate::fit::TIMING_REFERENCE,
            "command": command, "date": date, "cpu": machine.cpu, "os": machine.os,
            "device": self.device, "midi_port": self.midi_port,
            "midi_channel": self.settings.channel + 1, "note": self.settings.note, "velocity": self.settings.velocity,
            "inputs": self.settings.returns.iter().map(|c| c + 1).collect::<Vec<_>>(),
            "gap_ms": self.settings.gap_ms, "max_ms": self.settings.max_ms, "warmup_note": self.settings.warmup,
            "rate": self.rate, "buffer": self.buffer,
            "noise_floor_envelope_peak": self.floor, "reference_floor_envelope_peak": self.reference_floor,
            "envelope_window_samples": self.envelope_window,
            "detector": "onset late by the attack's rise to 10% of its peak plus up to envelope_window_samples - 1; included in the latency, nothing subtracted",
            "effective_compensation_offset": summary,
            "driver_input_latency_samples": self.input_latency,
            "estimated_delivery_plus_synth_samples": self.delivery_estimate(),
            "trials": trials,
            "max_fit_error_ns": self.settings.max_fit_error_ns,
            "fit_error_max_ns": self.fit_error_max_ns(),
            "fit_scatter_max_ns": fits.iter().map(|f| f.scatter_ns).fold(0.0f64, f64::max),
            "fit_scatter_note": "clock-fit scatter, not a bound on the send offset",
            "fit_leave_last_out_max_ns": fits.iter().map(|f| f.leave_last_out_ns.abs()).fold(0.0f64, f64::max),
            "send_position_uncertainty_ns": self.fit_error_max_ns(),
            "send_position_uncertainty_samples": self.send_uncertainty_samples(),
            "skipped_fit_error": self.skipped,
            "stream_errors": self.stream_errors, "lost_blocks": self.lost_blocks,
        })
    }
}

/// Add `extra`'s fields to the object `into`.
pub(crate) fn merge(into: &mut serde_json::Value, extra: serde_json::Value) {
    if let (Some(into), serde_json::Value::Object(extra)) = (into.as_object_mut(), extra) {
        into.extend(extra);
    }
}

/// The warning for a pre-send window more than 6 dB above the floor.
pub(crate) fn pre_send_warning(what: &str, pre_send: f32, floor: f32, retried: bool) -> Option<String> {
    if pre_send <= floor * PRE_SEND_WARN_FACTOR {
        return None;
    }
    let above = if floor > 0.0 { format!("{:.1} dB above", 20.0 * (pre_send / floor).log10()) } else { "above".to_string() };
    let retried = if retried { " (waited another gap first)" } else { "" };
    Some(format!(
        "warning: {what}'s pre-send level {} is {above} the noise floor {}{retried}: the previous release may still be sounding; its threshold was raised over it; raise --gap-ms",
        onset::dbfs(pre_send),
        onset::dbfs(floor)
    ))
}

/// The threshold and the refinement's guard of a send whose pre-send window peaked at `pre`,
/// over the noise floor `floor`: the floor's threshold, raised over a pre-send window more than
/// 6 dB above the floor (a release still sounding must not count as the onset).
pub(crate) fn levels(floor: f32, pre: f32) -> Result<(f32, f32), String> {
    let base = onset::threshold(onset::Floor { peak: floor, rms: 0.0 })?;
    let raised = pre > floor * PRE_SEND_WARN_FACTOR;
    let threshold = if raised { base.max(pre * onset::FLOOR_FACTOR) } else { base };
    let level = if raised { pre } else { floor };
    Ok((threshold, (level * onset::QUIET_FACTOR).min(threshold)))
}

/// Open `request`'s device duplex through `backend`, then play `settings.trials` notes into
/// `midi` and time each one's onset on the return. Any trial without an onset, a return not quiet
/// before a send, a stalled input or an error from `driver` ends the run with an error; the note
/// is released on every path. A stop requested through `interrupt` before a note-on refuses that
/// note.
pub fn measure(
    backend: &dyn ab::Backend,
    request: &Request,
    settings: &Settings,
    midi: &mut dyn MidiSink,
    driver: &mut dyn Driver,
    interrupt: &Interrupt,
) -> Result<Report, String> {
    settings.check()?;
    let ring_ms = u64::from(RING_SECONDS) * 1000 + u64::from(settings.max_ms);
    let streams = Arc::new(Streams::new(request.rate, &settings.returns, ring_ms));
    let highest = settings.returns.iter().copied().max().unwrap_or(0);
    let request = Request { loopback: true, in_ch: highest.saturating_add(1), out_ch: 1, ..request.clone() };
    let midi_port = midi.name();
    let mut outcome = None;
    let opened = session::run_with(backend, &request, &streams, &mut || {
        outcome = Some(trials(&streams, settings, &midi_port, &mut *midi, &mut *driver, interrupt)?);
        Ok(())
    })?;
    let done = outcome.ok_or("the measurement did not run")?;
    let rate = opened.render_rate;
    let input_latency = opened.capture.and_then(|c| c.latency).map(|d| (d.as_secs_f64() * f64::from(rate)).round() as u64);
    Ok(Report {
        device: opened.device,
        midi_port,
        settings: settings.clone(),
        rate,
        buffer: opened.render.period.frames(),
        input_latency,
        floor: done.floor,
        reference_floor: done.reference_floor,
        trials: done.trials,
        envelope_window: onset::window(rate),
        skipped: done.skipped,
        stream_errors: streams.errors.load(Ordering::Relaxed),
        lost_blocks: done.lost_blocks,
    })
}

/// Wait `ns` while the clock keeps fitting.
pub(crate) fn pass(driver: &mut dyn Driver, clock: &mut Clock, streams: &Streams, ns: u64) -> Result<(), String> {
    let until = driver.now_ns().saturating_add(ns);
    loop {
        clock.drain(streams)?;
        if driver.now_ns() >= until {
            return Ok(());
        }
        driver.wait(POLL_NS)?;
    }
}

/// The streams are running with a clock map: rate, envelope window, clock and reference floor.
pub(crate) struct Started {
    pub(crate) rate: u32,
    pub(crate) w: usize,
    pub(crate) floor_frames: u64,
    pub(crate) clock: Clock,
    pub(crate) reference: onset::Floor,
}

/// The warm-up shared by both modes: the rates, a ring that holds `window_ms`, a clock fit and a
/// reference floor; then the unmeasured warm-up note when `settings.warmup` (released before
/// this returns; the caller waits a gap before its first send).
pub(crate) fn start(
    streams: &Streams,
    settings: &Settings,
    window_ms: u64,
    driver: &mut dyn Driver,
    notes: &mut Notes<'_>,
) -> Result<Started, String> {
    let rate = streams.render.get().map(|a| a.format.rate).ok_or("the output did not open")?;
    if streams.capture.get().map(|a| a.format.rate) != Some(rate) {
        return Err("the input did not open at the output's rate".into());
    }
    if frames_of(rate, window_ms + 1000) > streams.ring.len() as u64 {
        return Err(format!("the device opened at {rate} Hz, above the {} Hz asked; pass --rate {rate}", streams.asked_rate));
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
        let line = clock.line();
        if line.as_ref().is_ok_and(|l| l.span_ns >= WARMUP_FIT_SPAN_NS) && from != NONE && end.saturating_sub(from) >= floor_frames {
            break;
        }
        if driver.now_ns().saturating_sub(started) > WARMUP_MS * 1_000_000 {
            return Err(match line {
                _ if clock.blocks == 0 => format!("no capture blocks after {WARMUP_MS} ms; the driver may not be running"),
                Err(why) => format!("no clock map after {WARMUP_MS} ms: {why}"),
                Ok(_) => format!("no input after {WARMUP_MS} ms"),
            });
        }
        driver.wait(POLL_NS)?;
    }
    let end = streams.captured_end.load(Ordering::Acquire);
    let reference = onset::noise_floor(&streams.read(end - floor_frames, end)?, w);
    onset::threshold(reference)?;
    if settings.warmup {
        // The warm-up note: the synth's (or the port's) first-note behavior stays out of the trials.
        notes.on(settings.velocity).map_err(|e| format!("warm-up note: {e}"))?;
        pass(driver, &mut clock, streams, WARMUP_NOTE_MS * 1_000_000)?;
        notes.off().map_err(|e| format!("warm-up note: {e}"))?;
    }
    Ok(Started { rate, w, floor_frames, clock, reference })
}

/// The pre-send window ending now: its floor, the floor lowered to it, and when it is more than
/// 6 dB above the floor, a wait of another `gap_ns` and a second reading (`retried`). A window
/// louder than the onset threshold's ceiling is refused.
pub(crate) fn pre_send(
    streams: &Streams,
    started: &mut Started,
    floor: &mut f32,
    gap_ns: Option<u64>,
    driver: &mut dyn Driver,
    what: &str,
) -> Result<(onset::Floor, bool), String> {
    let read = |streams: &Streams, frames: u64, w: usize| -> Result<onset::Floor, String> {
        let end = streams.captured_end.load(Ordering::Acquire);
        Ok(onset::noise_floor(&streams.read(end.saturating_sub(frames), end)?, w))
    };
    let mut pre = read(streams, started.floor_frames, started.w)?;
    *floor = floor.min(pre.peak);
    let mut retried = false;
    if let Some(gap_ns) = gap_ns
        && pre.peak > *floor * PRE_SEND_WARN_FACTOR
    {
        eprintln!(
            "{what}: the return before the note ({}) is more than 6 dB above the noise floor ({}); waiting another gap",
            onset::dbfs(pre.peak),
            onset::dbfs(*floor)
        );
        pass(driver, &mut started.clock, streams, gap_ns)?;
        pre = read(streams, started.floor_frames, started.w)?;
        *floor = floor.min(pre.peak);
        retried = true;
    }
    onset::not_sounding(pre)?;
    Ok((pre, retried))
}

/// What a trial captured: enough to find its onset again against the run's final floor.
struct Capture {
    send: u64,
    base: u64,
    from: usize,
    samples: Vec<f32>,
    pre: onset::Floor,
    /// The threshold the trial ran with (its final one is never above it).
    threshold: f32,
    send_call_ns: u64,
    fit: SendFit,
    retried: bool,
}

/// What the trials found.
struct Done {
    floor: f32,
    reference_floor: f32,
    trials: Vec<Trial>,
    skipped: u32,
    lost_blocks: u64,
}

/// The trials, on the control thread while the streams run: the reference floor, the warm-up
/// note and the trials.
fn trials(
    streams: &Streams,
    settings: &Settings,
    port: &str,
    midi: &mut dyn MidiSink,
    driver: &mut dyn Driver,
    interrupt: &Interrupt,
) -> Result<Done, String> {
    // First, so that every exit below releases the note.
    let mut notes = Notes { midi, interrupt, channel: settings.channel, note: settings.note };
    let window_ms = u64::from(settings.max_ms) + u64::from(onset::PEAK_MS) + u64::from(FLOOR_MS);
    let mut started = start(streams, settings, window_ms, driver, &mut notes)?;
    let (rate, w) = (started.rate, started.w);
    let peak_len = usize::try_from(frames_of(rate, u64::from(onset::PEAK_MS))).unwrap_or(usize::MAX);
    let gap_ns = u64::from(settings.gap_ms) * 1_000_000;
    let mut floor = started.reference.peak;
    let (mut captures, mut skipped) = (Vec::new(), 0u32);
    for k in 1..=settings.trials {
        let fail = |e: String| format!("trial {k}: {e}");
        if k > 1 || settings.warmup {
            pass(driver, &mut started.clock, streams, gap_ns)?;
        }
        started.clock.drain(streams)?;
        // The send position is only as good as the clock fit: above the limit, skip the trial
        // before any note is sent.
        let (_, now_fit) = started.clock.position_at(driver.now_ns()).map_err(fail)?;
        if now_fit.error_ns > settings.max_fit_error_ns as f64 {
            skipped += 1;
            eprintln!("trial {k}: skipped, the clock fit's standard error is {:.3} ms", now_fit.error_ns / 1e6);
            if u64::from(skipped) * 2 > u64::from(settings.trials) {
                return Err(format!(
                    "{skipped} of {} trials skipped: the position clock's standard error ({:.3} ms, scatter {:.3} ms) is above --max-fit-error-ms {:.2}; the driver's arrival times are irregular",
                    settings.trials,
                    now_fit.error_ns / 1e6,
                    now_fit.scatter_ns / 1e6,
                    settings.max_fit_error_ns as f64 / 1e6
                ));
            }
            continue;
        }
        let (pre, retried) = pre_send(streams, &mut started, &mut floor, Some(gap_ns), driver, &format!("trial {k}")).map_err(fail)?;
        let (threshold, _) = levels(floor, pre.peak).map_err(fail)?;
        started.clock.drain(streams)?;
        let sent = driver.now_ns();
        notes.on(settings.velocity).map_err(fail)?;
        let send_call_ns = driver.now_ns().saturating_sub(sent);
        let (send, fit) = started.clock.position_at(sent).map_err(fail)?;
        // The scan keeps one envelope window of samples before the send.
        let base = send.saturating_sub(w as u64);
        let stop = send + frames_of(rate, u64::from(settings.max_ms));
        let deadline = sent.saturating_add((u64::from(settings.max_ms) + STALL_MS) * 1_000_000);
        let mut samples: Vec<f32> = Vec::new();
        let (from, mut scanned, mut crossing) = ((send - base) as usize, (send - base) as usize, None);
        loop {
            started.clock.drain(streams)?;
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
        if crossing.is_none() {
            let heard = samples.get(from..).map_or(0.0, |s| onset::noise_floor(s, w).peak);
            return Err(fail(format!(
                "no onset within {} ms of the note-on: the return peaked at {} against a threshold of {} (floor {}). Check that the synth plays on MIDI channel {} from \"{port}\", that its audio reaches input {}, and its level",
                settings.max_ms,
                onset::dbfs(heard),
                onset::dbfs(threshold),
                onset::dbfs(floor),
                settings.channel + 1,
                settings.returns_label(),
            )));
        }
        captures.push(Capture { send, base, from, samples, pre, threshold, send_call_ns, fit, retried });
    }
    drop(notes);
    if interrupt.release_failed() {
        return Err("releasing the note failed: check that the synth is silent".into());
    }
    // The onsets again, against the lowest floor of the run (never above the trial's own threshold).
    let mut out = Vec::new();
    for (k, c) in captures.into_iter().enumerate() {
        let (threshold, guard) = levels(floor, c.pre.peak).map_err(|e| format!("trial {}: {e}", k + 1))?;
        let threshold = threshold.min(c.threshold);
        let crossing = onset::first_crossing(&c.samples, c.from, w, threshold).ok_or_else(|| format!("trial {}: the onset was lost", k + 1))?;
        let found = onset::refine(&c.samples, c.from, crossing, w, peak_len, guard.min(threshold));
        out.push(Trial {
            send: c.send,
            onset: c.base + found.at as u64,
            crossing: c.base + found.crossing as u64,
            pre_send: c.pre.peak,
            threshold,
            peak: found.peak,
            send_call_ns: c.send_call_ns,
            fit: c.fit,
            retried: c.retried,
        });
    }
    Ok(Done { floor, reference_floor: started.reference.peak, trials: out, skipped, lost_blocks: started.clock.lost })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_exits_at_once_only_on_a_second_press_with_no_note_outstanding() {
        let interrupt = Interrupt::default();
        assert!(!interrupt.ctrl_c(), "a first press only requests the stop");
        assert!(interrupt.stopped());
        assert!(interrupt.ctrl_c(), "a second press with no note outstanding exits");

        let interrupt = Interrupt::default();
        interrupt.note_on.store(true, Ordering::SeqCst);
        assert!(!interrupt.ctrl_c());
        assert!(!interrupt.ctrl_c(), "a second press while a note may sound waits for the release");
        assert!(!interrupt.ctrl_c());
        assert!(interrupt.stopped(), "the stop stays requested");
        interrupt.note_released();
        assert!(interrupt.ctrl_c(), "once released, the next press exits");
    }

    #[test]
    fn a_stop_requested_before_a_note_on_refuses_it_and_the_guard_still_releases() {
        struct Log(Vec<Vec<u8>>);
        impl MidiSink for Log {
            fn send(&mut self, message: &[u8]) -> Result<(), String> {
                self.0.push(message.to_vec());
                Ok(())
            }
            fn name(&self) -> String {
                "log".into()
            }
        }
        let (interrupt, mut log) = (Interrupt::default(), Log(Vec::new()));
        assert!(!interrupt.ctrl_c(), "Ctrl+C during the device open");
        {
            let mut notes = Notes { midi: &mut log, interrupt: &interrupt, channel: 2, note: 60 };
            assert_eq!(notes.on(100).unwrap_err(), "interrupted");
            assert!(interrupt.note_outstanding(), "no exit until the guard has released");
            assert!(!interrupt.ctrl_c());
        }
        assert_eq!(log.0, vec![vec![0x82, 60, 0], vec![0xB2, 123, 0]], "no note-on, then the release");
        assert!(!interrupt.note_outstanding());
        assert!(interrupt.ctrl_c());
    }

    fn fit(error_ns: f64, app_offset: Option<i64>) -> SendFit {
        SendFit { scatter_ns: 2_600_000.0, sigma_ns: 1_500_000.0, error_ns, leave_last_out_ns: -900_000.0, points: 375, app_offset }
    }

    #[test]
    fn a_pre_send_level_above_the_floor_and_the_fit_figures_are_reported() {
        let trial = |pre_send: f32, error_ns: f64, retried: bool| Trial {
            send: 1000,
            onset: 2000,
            crossing: 1999,
            pre_send,
            threshold: 0.001,
            peak: 0.1,
            send_call_ns: 0,
            fit: fit(error_ns, None),
            retried,
        };
        let report = Report {
            device: "d".into(),
            midi_port: "m".into(),
            settings: Settings::new(0, vec![0]),
            rate: 44_100,
            buffer: Some(256),
            input_latency: Some(300),
            floor: 0.00005,
            reference_floor: 0.0005,
            trials: vec![trial(0.00005, 46_770.0, false), trial(0.00043, 49_670.0, true)],
            envelope_window: 11,
            skipped: 1,
            lost_blocks: 0,
            stream_errors: 0,
        };
        let text = report.lines().join("\n");
        assert!(
            text.contains("warning: trial 2's pre-send level -67.3 dBFS is 18.7 dB above the noise floor -86.0 dBFS (waited another gap first)"),
            "{text}"
        );
        assert!(!text.contains("warning: trial 1"), "{text}");
        assert!(text.contains("floor -86.0 dBFS (lowest of the reference window -66.0 dBFS and 2 pre-send windows), warm-up note sent"), "{text}");
        assert!(text.contains("clock-fit scatter (not a bound on the send offset): up to 2600.0 us"), "{text}");
        assert!(text.contains("standard error at the send up to +-49.7 us (2.19 samples); leave-last-out prediction error up to 900.0 us"), "{text}");
        assert!(text.contains("valid at 0 of 2 sends"), "{text}");
        assert!(text.contains("skipped: 1 of 10 trials"), "{text}");
        let json = report.json("c", "d", &crate::report::Machine { cpu: "c".into(), os: "o".into() });
        assert_eq!(json["fit_error_max_ns"], 49_670.0);
        assert_eq!(json["fit_scatter_max_ns"], 2_600_000.0);
        assert_eq!(json["skipped_fit_error"], 1);
        assert_eq!(json["max_fit_error_ns"], 500_000);
        assert_eq!(json["trials"][1]["pre_send_retried"], true);
        assert_eq!(json["trials"][0]["fit_points"], 375);
        assert_eq!(json["warmup_note"], true);
        assert_eq!(json["timing_reference"], crate::fit::TIMING_REFERENCE);
        assert!(text.contains("timing reference: capture-block arrival (includes input buffer and mean driver dispatch;"), "{text}");
    }

    #[test]
    fn the_app_map_beside_the_bench_fit() {
        let lines = fit_lines(48_000, &[fit(1000.0, Some(2)), fit(1000.0, Some(-3)), fit(1000.0, None)], 500_000);
        assert!(lines[2].contains("valid at 2 of 3 sends, at most 3 samples from the bench fit"), "{lines:?}");
    }

    #[test]
    fn the_threshold_rises_over_a_ringing_pre_send_window_only() {
        let (quiet, _) = levels(0.0005, 0.0006).unwrap();
        assert_eq!(quiet, 0.002, "the floor's threshold");
        let (ringing, guard) = levels(0.0005, 0.004).unwrap();
        assert_eq!((ringing, guard), (0.016, 0.008));
    }

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
