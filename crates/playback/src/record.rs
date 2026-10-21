//! Audio input capture for recording.
//!
//! A [`Recorder`] opens the default input device. While armed, the input callback copies each
//! block into a preallocated lock-free ring (it never locks or allocates), and a writer thread
//! streams the ring into one WAV file per take, rewriting the header every second so a crash leaves
//! a playable file.

pub mod start;

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use soundcraft_model::TrackId;

/// What [`Recorder::finish`] returns: the takes that hold audio and the first problem met.
pub type Takes = (Vec<(TrackId, PathBuf)>, Option<String>);

/// Frames each block of a [`LiveTake`] sums up.
pub const LIVE_BLOCK: usize = 128;
/// Blocks a live take keeps per channel at most (three hours at 48 kHz); its file records on.
const LIVE_BLOCKS: usize = 1 << 22;

/// A take while it records, summed up for drawing: per channel, the (min, max) of each
/// [`LIVE_BLOCK`] frames so far.
pub type LiveTake = Vec<Vec<(f32, f32)>>;

const RING_SECONDS: usize = 4;
/// How long a recording input may go without a callback before it counts as gone.
const STALL: Duration = Duration::from_secs(2);
const GONE: u8 = 1;
const BROKEN: u8 = 2;
const ARMED: u64 = 1;
const CAPTURING: u64 = 2;
const CAPTURE_FLAGS: u64 = ARMED | CAPTURING;

/// Why the input can't go on.
#[derive(Debug, Clone, PartialEq)]
pub enum Trouble {
    /// The device went away or stopped sending audio, or its callback hit a bug.
    Device(String),
    /// Writing a take failed (a full disk, say).
    Disk(String),
}

/// Single-producer single-consumer FIFO of samples (input callback → writer thread). The slots
/// are atomics, so it needs no lock and no `unsafe`.
struct CaptureRing {
    slots: Box<[AtomicU32]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl CaptureRing {
    fn new(capacity: usize) -> CaptureRing {
        let n = capacity.max(2).next_power_of_two();
        CaptureRing { slots: (0..n).map(|_| AtomicU32::new(0)).collect(), mask: n - 1, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    /// Queue up to `n` samples (`sample(i)` is the i-th); returns how many fit.
    fn push(&self, n: usize, sample: impl Fn(usize) -> f32) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let n = n.min(self.slots.len().saturating_sub(head.wrapping_sub(self.tail.load(Ordering::Acquire))));
        for i in 0..n {
            if let Some(slot) = self.slots.get(head.wrapping_add(i) & self.mask) {
                slot.store(sample(i).to_bits(), Ordering::Relaxed);
            }
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    fn pop_all(&self, out: &mut Vec<f32>) {
        let (tail, head) = (self.tail.load(Ordering::Relaxed), self.head.load(Ordering::Acquire));
        out.extend(
            (0..head.wrapping_sub(tail))
                .map(|i| self.slots.get(tail.wrapping_add(i) & self.mask).map_or(0.0, |s| f32::from_bits(s.load(Ordering::Relaxed)))),
        );
        self.tail.store(head, Ordering::Release);
    }
}

/// An input sample, or silence for NaN and infinities, which would poison the take and the mix.
fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

struct Shared {
    /// Generation in the high bits; admission and in-flight producer in the low bits.
    capture_state: AtomicU64,
    ring: CaptureRing,
    first_frame: AtomicU64,
    /// Producer-owned while admitted; reset only when quiescent, drained by the writer at stop.
    pending_silence: AtomicUsize,
    /// Samples lost while the ring was full.
    dropped: AtomicUsize,
    /// Input callbacks so far, to tell a device that stopped sending.
    beats: AtomicU64,
    /// Set from the input stream when it can't go on: `GONE` or `BROKEN`.
    fault: AtomicU8,
    /// The first failure writing a take, until it's reported.
    failed: Mutex<Option<String>>,
}

impl Shared {
    fn new(ring: CaptureRing) -> Shared {
        let (beats, fault, failed) = (AtomicU64::new(0), AtomicU8::new(0), Mutex::new(None));
        Shared {
            capture_state: AtomicU64::new(0),
            ring,
            first_frame: AtomicU64::new(u64::MAX),
            pending_silence: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            beats,
            fault,
            failed,
        }
    }

    /// Control thread, after the preceding writer has joined (or failed startup quiesced).
    fn start_capture(&self) {
        let generation = (self.capture_state.load(Ordering::Relaxed) & !CAPTURE_FLAGS).wrapping_add(CAPTURE_FLAGS + 1);
        self.pending_silence.store(0, Ordering::Relaxed);
        self.capture_state.store(generation | ARMED, Ordering::Release);
    }

    fn stop_capture(&self) {
        self.capture_state.fetch_and(!ARMED, Ordering::AcqRel);
    }

    fn armed(&self) -> bool {
        self.capture_state.load(Ordering::Relaxed) & ARMED != 0
    }

    /// Single bounded admission attempt. Stop and admission are ordered by the same atomic;
    /// generation prevents an attempt delayed across stop/rearm from joining the next take.
    fn admit_capture(&self, observed: u64) -> Option<CaptureLease<'_>> {
        if observed & CAPTURE_FLAGS != ARMED {
            return None;
        }
        self.capture_state.compare_exchange(observed, observed | CAPTURING, Ordering::AcqRel, Ordering::Relaxed).ok()?;
        Some(CaptureLease(self))
    }

    fn begin_capture(&self) -> Option<CaptureLease<'_>> {
        self.admit_capture(self.capture_state.load(Ordering::Acquire))
    }

    fn quiescent(&self) -> bool {
        self.capture_state.load(Ordering::Acquire) & CAPTURE_FLAGS == 0
    }

    /// Control thread only, after closing admission. Also needed if no writer was spawned or
    /// its thread panicked: joining that thread alone would not prove producer quiescence.
    /// Total wait time depends on producer progress; the polling interval is not a timeout.
    fn wait_for_capture(&self) {
        while !self.quiescent() {
            #[cfg(test)]
            tests::on_capture_wait();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Writer only. Observe quiescence before the final pop, so it covers every admitted push.
    /// Once true no producer can enter until the control thread has joined this writer.
    fn drain_capture(&self, block: &mut Vec<f32>) -> bool {
        let finished = self.quiescent();
        self.ring.pop_all(block);
        finished
    }

    /// Run the input callback's `work`. After a panic in it, the input is broken and does
    /// nothing more.
    fn guarded(&self, work: impl FnOnce()) {
        self.beats.fetch_add(1, Ordering::Relaxed);
        if self.fault.load(Ordering::Relaxed) != BROKEN && std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
            self.fault.store(BROKEN, Ordering::Relaxed);
        }
    }

    /// The input callback's work under a CaptureLease. Samples that don't fit are owed and queued
    /// as silence before the next block, so the take keeps its timing.
    fn capture_at(&self, first_frame: u64, data: &[f32]) {
        let _ = self.first_frame.compare_exchange(u64::MAX, first_frame, Ordering::Release, Ordering::Relaxed);
        self.capture(data);
    }

    fn capture(&self, data: &[f32]) {
        let owed = self.pending_silence.load(Ordering::Relaxed);
        let owed = owed - self.ring.push(owed, |_| 0.0);
        let kept = if owed == 0 { self.ring.push(data.len(), |i| data.get(i).copied().map_or(0.0, finite)) } else { 0 };
        self.pending_silence.store(owed.saturating_add(data.len() - kept), Ordering::Relaxed);
        self.dropped.fetch_add(data.len() - kept, Ordering::Relaxed);
    }
}

/// Release on normal return and unwind. No callback must arrive after Stop to acknowledge it.
struct CaptureLease<'a>(&'a Shared);

impl Drop for CaptureLease<'_> {
    fn drop(&mut self) {
        self.0.capture_state.fetch_and(!CAPTURING, Ordering::Release);
    }
}

pub struct Recorder {
    shared: Arc<Shared>,
    /// Live input for monitoring (always fed while the input stream runs).
    pub monitor: Arc<InputRing>,
    #[cfg(not(target_os = "freebsd"))]
    _stream: Option<cpal::Stream>,
    clock: Option<crate::device_clock::MaintainedClock>,
    record_start: Option<start::Reader>,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: usize,
    /// The take writer, and what each track's take holds so far.
    writer: Option<(std::thread::JoinHandle<Takes>, Vec<(TrackId, Arc<Mutex<LiveTake>>)>)>,
    /// The callback count last seen, and since when.
    seen: (u64, Instant),
}

/// The input stream: device samples are converted to f32 in a scratch buffer allocated here (so
/// the callback never allocates), then feed the monitor ring and, while armed, the capture.
#[cfg(not(target_os = "freebsd"))]
fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Arc<Shared>,
    monitor: &Arc<InputRing>,
) -> Result<(cpal::Stream, crate::device_clock::ClockReader), cpal::BuildStreamError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    use cpal::traits::DeviceTrait;
    let (sh, errors) = (Arc::clone(shared), Arc::clone(shared));
    let mon = Arc::clone(monitor);
    let mut scratch = vec![0.0f32; 8192 * usize::from(config.channels).max(1)];
    let chunk = scratch.len();
    let channels = usize::from(config.channels).max(1);
    let (mut clock, reader) = crate::device_clock::stream_clock(crate::device_clock::Direction::Input, config.sample_rate.0)
        .map_err(|error| cpal::BackendSpecificError { description: error.into() })?;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            crate::mark_audio_thread();
            let first_frame = clock.cpal.next_frame();
            clock.cpal.input(data.len() / channels, info, clock.origin.now_ns());
            sh.guarded(|| {
                // One admission covers the entire device callback, including scratch chunks.
                let capture = sh.begin_capture();
                for (index, part) in data.chunks(chunk).enumerate() {
                    let Some(s) = scratch.get_mut(..part.len()) else { return };
                    for (o, x) in s.iter_mut().zip(part) {
                        *o = f32::from_sample(*x);
                    }
                    mon.push(s);
                    if capture.is_some()
                        && let Some(frame) = first_frame.checked_add((index * (chunk / channels)) as u64)
                    {
                        sh.capture_at(frame, s);
                    }
                }
            });
        },
        move |e| {
            // Possibly the audio thread (see `crate::mark_audio_thread`).
            crate::mark_audio_thread();
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                let _ = errors.fault.compare_exchange(0, GONE, Ordering::Relaxed, Ordering::Relaxed);
            }
            log::warn!("input stream error: {e}");
        },
        None,
    )?;
    Ok((stream, reader))
}

/// Whether an input config can be opened: some drivers report a default input with no channels,
/// and building a stream with 0 channels reaches `unreachable!` in asio-sys.
pub(crate) fn input_usable(config: &cpal::SupportedStreamConfig) -> bool {
    config.channels() > 0
}

/// The input config at `rate` when the device offers it (so input and output run together: the
/// best format first, then the most channels), else the device default, in a format we read.
#[cfg(not(target_os = "freebsd"))]
fn input_config(device: &cpal::Device, rate: Option<u32>) -> Result<cpal::SupportedStreamConfig, String> {
    use cpal::traits::DeviceTrait;
    let at_rate = rate.and_then(|r| {
        device
            .supported_input_configs()
            .ok()?
            .filter(|c| {
                c.channels() > 0
                    && usize::from(c.channels()) <= soundcraft_model::MAX_DEVICE_CHANNELS
                    && crate::devices::format_rank(c.sample_format()).is_some()
                    && c.min_sample_rate().0 <= r
                    && c.max_sample_rate().0 >= r
            })
            .min_by_key(|c| (crate::devices::format_rank(c.sample_format()), std::cmp::Reverse(c.channels())))
            .map(|c| c.with_sample_rate(cpal::SampleRate(r)))
    });
    let cfg = match at_rate {
        Some(c) => c,
        None => device.default_input_config().map_err(|e| e.to_string())?,
    };
    if !input_usable(&cfg) {
        return Err("the audio input has no channels".to_string());
    }
    let format = cfg.sample_format();
    if crate::devices::format_rank(format).is_none() {
        return Err(format!("unsupported input sample format {format:?}"));
    }
    Ok(cfg)
}

/// How many channels an input opened on `device` at `rate` would have (0 when it has none), for
/// the I/O map of a duplex device.
#[cfg(not(target_os = "freebsd"))]
pub(crate) fn input_channels_on(device: &cpal::Device, rate: u32) -> u16 {
    input_config(device, Some(rate)).map_or(0, |c| c.channels())
}

#[cfg(target_os = "freebsd")]
pub(crate) fn input_channels_on(_device: &cpal::Device, _rate: u32) -> u16 {
    0
}

impl Recorder {
    /// Observed scheduling state of this input worker generation. A configured feature is
    /// not evidence of successful registration; unsupported backends report Unobserved.
    pub fn thread_priority(&self) -> crate::scheduling::WorkerPriority {
        #[cfg(not(target_os = "freebsd"))]
        {
            crate::scheduling::observe(self._stream.as_ref())
        }
        #[cfg(target_os = "freebsd")]
        {
            crate::scheduling::WorkerPriority::NoWorker
        }
    }

    /// Open the default input device. Errors when there is none (the caller shows a message).
    pub fn open() -> Result<Recorder, String> {
        Recorder::open_with(&crate::AudioConfig::default())
    }

    /// Open the input device `audio` chooses (else the default) at its chosen rate and buffer
    /// size when the device offers them, else at the device's defaults. Integer devices (ASIO
    /// drivers usually are) are converted to f32.
    #[cfg(not(target_os = "freebsd"))]
    pub fn open_with(audio: &crate::AudioConfig) -> Result<Recorder, String> {
        let host = crate::devices::host(audio).ok_or("no audio system")?;
        let device = crate::devices::device(host, audio.input.as_deref(), false).ok_or("no audio input device")?;
        let cfg = input_config(&device, audio.sample_rate)?;
        let buffer = crate::devices::buffer_size(audio.buffer_size, cfg.buffer_size());
        // The chosen buffer size first; if the device refuses it, its default.
        let r = match Recorder::build(&device, &cfg, buffer) {
            Ok(r) => r,
            Err(e) if buffer != cpal::BufferSize::Default => {
                log::warn!("cannot open the input with buffer {buffer:?}: {e}; using the device default");
                Recorder::build(&device, &cfg, cpal::BufferSize::Default).map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };
        r.start()?;
        Ok(r)
    }

    #[cfg(target_os = "freebsd")]
    pub fn open_with(_audio: &crate::AudioConfig) -> Result<Recorder, String> {
        Err("recording is not supported on this platform yet".into())
    }

    /// Build the input stream on `device` (the output's own ASIO device) at `rate` and exactly
    /// `buffer`, without starting it: [`Player`](crate::Player) builds it before the output so
    /// the driver creates both directions' buffers once, then starts both ([`Recorder::start`]).
    /// The buffer must be the output's: cpal sizes the input's scratch buffer at build time,
    /// and the output's build recreates the buffers at its own size.
    #[cfg(not(target_os = "freebsd"))]
    pub(crate) fn build_on(device: &cpal::Device, rate: u32, buffer: cpal::BufferSize) -> Result<Recorder, String> {
        let cfg = input_config(device, Some(rate))?;
        if cfg.sample_rate().0 != rate {
            return Err(format!("the input does not run at {rate} Hz"));
        }
        Recorder::build(device, &cfg, buffer).map_err(|e| e.to_string())
    }

    #[cfg(target_os = "freebsd")]
    pub(crate) fn build_on(_device: &cpal::Device, _rate: u32, _buffer: cpal::BufferSize) -> Result<Recorder, String> {
        Err("recording is not supported on this platform yet".into())
    }

    /// The recorder around a new, not yet started input stream on `device` in `cfg`'s format.
    #[cfg(not(target_os = "freebsd"))]
    fn build(device: &cpal::Device, cfg: &cpal::SupportedStreamConfig, buffer: cpal::BufferSize) -> Result<Recorder, cpal::BuildStreamError> {
        use cpal::traits::DeviceTrait;
        #[allow(deprecated)]
        let name = device.name().unwrap_or_else(|_| "Input".into());
        let mut config = cfg.config();
        config.buffer_size = buffer;
        let channels = usize::from(config.channels).max(1);
        let ring = CaptureRing::new((config.sample_rate.0 as usize).saturating_mul(channels * RING_SECONDS).min(1 << 25));
        let shared = Arc::new(Shared::new(ring));
        let monitor = InputRing::new(usize::from(config.channels), config.sample_rate.0);
        let (stream, clock) = match cfg.sample_format() {
            cpal::SampleFormat::I32 => build_input::<i32>(device, &config, &shared, &monitor),
            cpal::SampleFormat::I16 => build_input::<i16>(device, &config, &shared, &monitor),
            _ => build_input::<f32>(device, &config, &shared, &monitor),
        }?;
        let clock = crate::device_clock::MaintainedClock::start(clock)
            .map_err(|error| {
                log::warn!("input clock maintenance unavailable: {error}");
            })
            .ok();
        let seen = (0, Instant::now());
        Ok(Recorder {
            monitor,
            shared,
            _stream: Some(stream),
            clock,
            record_start: None,
            device_name: name,
            sample_rate: config.sample_rate.0,
            channels,
            writer: None,
            seen,
        })
    }

    /// Start the input stream.
    pub(crate) fn start(&self) -> Result<(), String> {
        #[cfg(not(target_os = "freebsd"))]
        if let Some(stream) = &self._stream {
            use cpal::traits::StreamTrait;
            stream.play().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// A recorder without a device, for tests.
    #[cfg(test)]
    pub(crate) fn detached(name: &str) -> Recorder {
        Recorder {
            shared: Arc::new(Shared::new(CaptureRing::new(8))),
            monitor: InputRing::new(1, 48_000),
            #[cfg(not(target_os = "freebsd"))]
            _stream: None,
            clock: None,
            record_start: None,
            device_name: name.into(),
            sample_rate: 48_000,
            channels: 1,
            writer: None,
            seen: (0, Instant::now()),
        }
    }

    /// Create each take's file and start recording into them: a track's take records the given
    /// input channels into a new file.
    pub fn arm(&mut self, takes: Vec<(TrackId, Vec<usize>, PathBuf)>) -> Result<(), String> {
        self.arm_with(takes, None)
    }

    /// Prepare files and bounded input history. Only the applied audio transaction
    /// supplies the timestamp at which samples start belonging to the take.
    pub fn prepare(&mut self, takes: Vec<(TrackId, Vec<usize>, PathBuf)>, start: start::Reader) -> Result<(), String> {
        if self.has_pending_take() {
            return Err("already recording".into());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let clock = self.clock.as_mut().ok_or("input clock is unavailable for recording")?;
            let input = clock.report().map.ok_or("input clock is not ready for recording")?.key;
            if input.direction != crate::device_clock::Direction::Input {
                return Err("recording requires this input device's clock".into());
            }
            let input_start = clock.prepare_recording(start.clone())?;
            let gate = WriterStart {
                reader: start,
                history: start::History::new(self.channels, (self.sample_rate as usize).saturating_mul(RING_SECONDS)),
                input_start,
                placed: false,
                next: None,
            };
            self.arm_with(takes, Some(gate))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (takes, start);
            Err("timestamped audio recording is unavailable on this platform".into())
        }
    }

    /// The applied origin survives Stop until the caller has collected the take.
    pub(crate) fn prepared_start(&self) -> Option<start::Ticket> {
        self.record_start.as_ref().map(start::Reader::ticket)
    }

    /// The applied origin survives Stop until the caller has collected the take.
    pub fn applied_start(&self) -> Option<start::Point> {
        match self.record_start.as_ref()?.status() {
            start::Status::Started(point) => Some(point),
            _ => None,
        }
    }

    fn arm_with(&mut self, takes: Vec<(TrackId, Vec<usize>, PathBuf)>, gate: Option<WriterStart>) -> Result<(), String> {
        if self.writer.is_some() {
            return Err("already recording".into());
        }
        let takes =
            takes.into_iter().map(|(id, inputs, path)| TakeWriter::create(id, inputs, path, self.sample_rate)).collect::<Result<Vec<_>, _>>()?;
        let live = takes.iter().map(|t| (t.id, Arc::clone(&t.live))).collect();
        let ring = &self.shared.ring;
        ring.tail.store(ring.head.load(Ordering::Acquire), Ordering::Release);
        *self.shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.shared.first_frame.store(u64::MAX, Ordering::Relaxed);
        self.record_start = gate.as_ref().map(|gate| gate.reader.clone());
        self.shared.start_capture();
        let (shared, channels) = (Arc::clone(&self.shared), self.channels);
        match std::thread::Builder::new().name("take writer".into()).spawn(move || write_takes_started(&shared, takes, channels, gate)) {
            Ok(w) => self.writer = Some((w, live)),
            Err(e) => {
                self.shared.stop_capture();
                // No writer exists to join. Quiesce any admitted callback before a later arm
                // resets the ring/debt. This waits on the control thread, never the callback.
                self.shared.wait_for_capture();
                return Err(e.to_string());
            }
        }
        Ok(())
    }

    /// What the take recording onto `track` holds so far.
    pub fn live_take(&self, track: TrackId) -> Option<MutexGuard<'_, LiveTake>> {
        let (_, take) = self.writer.as_ref()?.1.iter().find(|(id, _)| *id == track)?;
        Some(take.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Why the input can't go on, if it can't: the device went away or, while recording, stopped
    /// sending audio; its callback hit a bug; or writing a take failed (reported once).
    pub fn trouble(&mut self) -> Option<Trouble> {
        if let Some(e) = self.shared.failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Some(Trouble::Disk(e));
        }
        let name = &self.device_name;
        match self.shared.fault.load(Ordering::Relaxed) {
            GONE => return Some(Trouble::Device(format!("“{name}” was disconnected"))),
            BROKEN => return Some(Trouble::Device("the audio input hit an internal error (see the log)".into())),
            _ => {}
        }
        let beats = self.shared.beats.load(Ordering::Relaxed);
        if beats != self.seen.0 || !self.shared.armed() {
            self.seen = (beats, Instant::now());
        }
        (self.seen.1.elapsed() >= STALL).then(|| Trouble::Device(format!("“{name}” stopped sending audio")))
    }

    /// Input the current take has lost to a slow disk so far, in milliseconds.
    pub fn lost_ms(&self) -> u64 {
        let frames = self.shared.dropped.load(Ordering::Relaxed) / self.channels.max(1);
        (frames as u64).saturating_mul(1000) / u64::from(self.sample_rate.max(1))
    }

    /// Independently maintained input clock snapshot; separate from output at every rate.
    pub fn clock_report(&mut self) -> Option<crate::device_clock::ClockReport> {
        self.clock.as_mut().map(crate::device_clock::MaintainedClock::report)
    }

    /// A prepared or active take must be finalized and collected before handing
    /// this input back to its device owner.
    pub fn has_pending_take(&self) -> bool {
        self.writer.is_some()
    }

    pub fn finish(&mut self) -> Takes {
        if let Some(start) = &self.record_start {
            start.cancel();
        }
        self.shared.stop_capture();
        self.shared.wait_for_capture();
        let (takes, problem) = self.writer.take().map_or_else(Takes::default, |(w, _)| {
            w.join().unwrap_or_else(|_| (Vec::new(), Some("the recording writer stopped unexpectedly; check the take files".into())))
        });
        let lost = self.shared.dropped.swap(0, Ordering::Relaxed) / self.channels.max(1);
        let slow =
            (lost > 0).then(|| format!("{} ms of input lost: the disk was too slow", lost.saturating_mul(1000) / self.sample_rate.max(1) as usize));
        (takes, problem.or(slow))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
fn write_takes(shared: &Shared, takes: Vec<TakeWriter>, channels: usize) -> Takes {
    write_takes_started(shared, takes, channels, None)
}

struct WriterStart {
    reader: start::Reader,
    history: start::History,
    input_start: start::mapping::InputStart,
    placed: bool,
    next: Option<u64>,
}

impl WriterStart {
    fn finished(&self) -> Result<bool, &'static str> {
        if self.placed || self.reader.status() == start::Status::Cancelled {
            return Ok(true);
        }
        // Mapping can fail after the last write. Propagate that failure before
        // finalizing; a terminal mapping state is not evidence of a saved take.
        self.input_start.frame()?;
        Ok(false)
    }

    fn write(&mut self, shared: &Shared, takes: &mut [TakeWriter], block: &[f32], channels: usize) -> Result<(), String> {
        if !self.placed {
            match self.reader.status() {
                start::Status::Pending => {}
                start::Status::Started(_) => {
                    if let Some(frame) = self.input_start.frame()? {
                        self.history.set_start(frame);
                        self.placed = true;
                    }
                }
                start::Status::Cancelled => {
                    shared.stop_capture();
                    return Ok(());
                }
            }
        }
        let first = *self.next.get_or_insert_with(|| shared.first_frame.load(Ordering::Acquire));
        if first == u64::MAX {
            if block.is_empty() {
                self.next = None;
                return Ok(());
            }
            return Err("recording input arrived without a device position".into());
        }
        self.history.push(first, block, |data| takes.iter_mut().for_each(|take| take.write(data, channels)))?;
        self.next = Some(first.checked_add((block.len() / channels.max(1)) as u64).ok_or("recording input position exhausted")?);
        Ok(())
    }
}

fn write_block(shared: &Shared, gate: &mut Option<WriterStart>, takes: &mut [TakeWriter], block: &[f32], channels: usize) {
    if let Some(gate) = gate {
        if let Err(error) = gate.write(shared, takes, block, channels) {
            shared.stop_capture();
            takes.iter_mut().for_each(|take| take.fail(&error));
        }
    } else {
        takes.iter_mut().for_each(|take| take.write(block, channels));
    }
}

fn write_takes_started(shared: &Shared, mut takes: Vec<TakeWriter>, channels: usize, mut gate: Option<WriterStart>) -> Takes {
    let (mut block, mut header_written, mut reported) = (Vec::new(), Instant::now(), false);
    loop {
        let stopping = shared.drain_capture(&mut block);
        // The ring holds interleaved samples and a pop can end mid-frame: write whole frames and
        // keep the remainder for the next pop, so channels never shift.
        let whole = block.len() - block.len() % channels.max(1);
        write_block(shared, &mut gate, &mut takes, &block[..whole], channels);
        block.drain(..whole);
        if !reported && let Some(e) = takes.iter().find_map(|t| t.error.clone()) {
            *shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
            reported = true;
        }
        if stopping {
            // No producer remains. Preserve the final overflow duration even if the device
            // never calls us again. Bound writer scratch instead of allocating the entire debt.
            let mut owed = shared.pending_silence.swap(0, Ordering::Relaxed);
            let chunk = 8192 * channels.max(1);
            while owed > 0 && takes.iter().any(|t| t.error.is_none()) {
                let n = owed.min(chunk);
                block.resize(block.len() + n, 0.0);
                let whole = block.len() - block.len() % channels.max(1);
                write_block(shared, &mut gate, &mut takes, &block[..whole], channels);
                block.drain(..whole);
                owed -= n;
            }
            // An applied start may still be in the independent clock worker.
            // Preserve the final input history until that worker acknowledges;
            // no further audio callback is required. Worker teardown fails the
            // pending mapping explicitly instead of leaving this join stranded.
            #[cfg(test)]
            tests::on_finalize_gate();
            let finished = match gate.as_ref().map_or(Ok(true), WriterStart::finished) {
                Ok(finished) => finished,
                Err(error) => {
                    takes.iter_mut().for_each(|take| take.fail(error));
                    true
                }
            };
            if finished || takes.iter().all(|take| take.error.is_some()) {
                break;
            }
            #[cfg(test)]
            tests::on_placement_wait();
        }
        if header_written.elapsed() >= Duration::from_secs(1) {
            header_written = Instant::now();
            takes.iter_mut().for_each(TakeWriter::write_header);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (mut done, mut problem) = (Vec::new(), None);
    for mut t in takes {
        t.write_header();
        if t.frames == 0 {
            let _ = std::fs::remove_file(&t.path);
        } else {
            done.push((t.id, t.path));
        }
        problem = problem.or(t.error);
    }
    (done, problem)
}

/// One take's file. After an error it stops writing; its header keeps covering what was written.
struct TakeWriter {
    id: TrackId,
    inputs: Vec<usize>,
    path: PathBuf,
    file: File,
    rate: u32,
    frames: u64,
    error: Option<String>,
    live: Arc<Mutex<LiveTake>>,
}

impl TakeWriter {
    fn create(id: TrackId, inputs: Vec<usize>, path: PathBuf, rate: u32) -> Result<TakeWriter, String> {
        let file = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| File::create_new(&path));
        let file = file.map_err(|e| format!("{}: {e}", path.display()))?;
        let live = Arc::new(Mutex::new(vec![Vec::new(); inputs.len()]));
        let mut take = TakeWriter { id, inputs, path, file, rate, frames: 0, error: None, live };
        take.write_header();
        take.error.take().map_or(Ok(take), Err)
    }

    /// Append this take's channels of `block` (interleaved, `channels` wide).
    fn write(&mut self, block: &[f32], channels: usize) {
        let channels = channels.max(1);
        if self.error.is_some() {
            return;
        }
        let bytes: Vec<u8> = block
            .chunks_exact(channels)
            .flat_map(|f| self.inputs.iter().flat_map(move |&i| f.get(i).copied().unwrap_or(0.0).to_le_bytes()))
            .collect();
        match self.file.write_all(&bytes) {
            Ok(()) => {
                self.summarize(block, channels);
                self.frames += (block.len() / channels) as u64;
            }
            Err(e) => self.fail(e),
        }
    }

    /// Add this take's channels of `block` to its live take.
    fn summarize(&self, block: &[f32], channels: usize) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        for (f, n) in block.chunks_exact(channels).zip(self.frames..) {
            let at = (n / LIVE_BLOCK as u64) as usize;
            for (blocks, &i) in live.iter_mut().zip(&self.inputs) {
                let v = f.get(i).copied().unwrap_or(0.0);
                match blocks.get_mut(at) {
                    Some(b) => *b = (b.0.min(v), b.1.max(v)),
                    None if at < LIVE_BLOCKS => blocks.push((v, v)),
                    None => {}
                }
            }
        }
    }

    fn write_header(&mut self) {
        let header = match soundcraft_audio_io::wav_float_header(self.inputs.len(), self.rate, self.frames) {
            Ok(h) => h,
            Err(e) => return self.fail(e),
        };
        let f = &mut self.file;
        if let Err(e) = f.seek(SeekFrom::Start(0)).and_then(|_| f.write_all(&header)).and_then(|()| f.seek(SeekFrom::End(0))) {
            self.fail(e);
        }
    }

    fn fail(&mut self, e: impl std::fmt::Display) {
        self.error.get_or_insert_with(|| format!("{}: {e}", self.path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        // Deterministic control-thread scheduling point. Never runs in the input callback;
        // absent from production builds and private to each test's control thread.
        static CAPTURE_WAIT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
        static PLACEMENT_WAIT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn on_capture_wait() {
        let hook = CAPTURE_WAIT_HOOK.with(|h| h.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(super) fn on_placement_wait() {
        if let Some(hook) = PLACEMENT_WAIT_HOOK.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
    }

    thread_local! {
        static FINALIZE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn on_finalize_gate() {
        if let Some(hook) = FINALIZE_HOOK.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn mapping_failure_between_write_and_stop_check_reports_failure() {
        let (key, map, now) = recording_map();
        let path = std::env::temp_dir().join(format!("clock-failure-{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let takes = vec![TakeWriter::create(TrackId(1), vec![0], path.clone(), 48_000).unwrap()];
        let shared = Shared::new(CaptureRing::new(32));
        shared.start_capture();
        {
            let _lease = shared.begin_capture().unwrap();
            shared.capture_at(100, &[0.5; 12]);
        }
        shared.stop_capture();
        let (mut control, mut publisher) = start::pair(soundcraft_mix::EngineEpoch(5));
        let reader = control.prepare().unwrap();
        let (mut mapping, input_start) = start::mapping::pair(reader.clone(), key);
        let point = start::Point {
            host_ns: map.frame_to_host(key, 104, now).unwrap(),
            output_frame: 900,
            position: crate::timeline_map::Position { sample: 500, fraction: 0, denominator: 1 },
        };
        assert!(publisher.start(reader.ticket(), point));
        let gate = WriterStart { reader, history: start::History::new(1, 24), input_start, placed: false, next: None };
        FINALIZE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                assert!(mapping.service(None, Some(now)));
            }))
        });
        let (takes, error) = write_takes_started(&shared, takes, 1, Some(gate));
        assert!(takes.is_empty());
        assert!(!path.exists());
        assert!(error.is_some(), "input mapping failed during Stop, but the take was deleted and success returned");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn writer_delay_overflow_and_input_partitions_preserve_boundary() {
        let (key, map, now) = recording_map();
        for (case, parts) in [vec![12], vec![4, 4, 4], vec![1, 3, 5, 3]].into_iter().enumerate() {
            let path = std::env::temp_dir().join(format!("boundary-overflow-{}-{case}.wav", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let takes = vec![TakeWriter::create(TrackId(1), vec![0], path.clone(), 48_000).unwrap()];
            let shared = Shared::new(CaptureRing::new(8));
            shared.start_capture();
            let (mut control, mut publisher) = start::pair(soundcraft_mix::EngineEpoch(5));
            let reader = control.prepare().unwrap();
            let (mut mapping, input_start) = start::mapping::pair(reader.clone(), key);
            let gate = WriterStart { reader: reader.clone(), history: start::History::new(1, 24), input_start, placed: false, next: None };
            let data: Vec<f32> = (0..12).map(|n| n as f32 / 20.0).collect();
            let mut offset = 0;
            for frames in parts {
                let (_, heap) = soundcraft_rt::testing::measure(|| {
                    let _lease = shared.begin_capture().unwrap();
                    shared.capture_at(100 + offset as u64, &data[offset..offset + frames]);
                });
                assert!(heap.is_zero(), "{heap:?}");
                offset += frames;
            }
            let point = start::Point {
                host_ns: map.frame_to_host(key, 104, now).unwrap(),
                output_frame: 900,
                position: crate::timeline_map::Position { sample: 500, fraction: 0, denominator: 1 },
            };
            assert!(publisher.start(reader.ticket(), point));
            assert!(mapping.service(Some(map), Some(now)));
            shared.stop_capture();
            let (takes, error) = write_takes_started(&shared, takes, 1, Some(gate));
            assert!(error.is_none(), "{error:?}");
            assert_eq!(takes.len(), 1);
            assert_eq!(decode(&path)[0], [0.2, 0.25, 0.3, 0.35, 0.0, 0.0, 0.0, 0.0]);
            assert_eq!(shared.dropped.load(Ordering::Relaxed), 4);
            std::fs::remove_file(path).unwrap();
        }
    }

    fn decode(path: &PathBuf) -> Vec<Vec<f32>> {
        soundcraft_audio_io::decode(&std::fs::read(path).unwrap(), None).unwrap().1.channels
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn recording_map() -> (crate::device_clock::ClockKey, crate::device_clock::ClockMap, u64) {
        use crate::device_clock::{self, ClockFitter, Direction, Observation, Quality, Stamp};
        let (_, reader) = device_clock::stream_clock(Direction::Input, 48_000).unwrap();
        let key = reader.key();
        drop(reader);
        let mut fitter = ClockFitter::new(key);
        for i in 0..=30u64 {
            let ns = i * 4_000_000;
            let _ = fitter.observe(Observation {
                key,
                first_frame: i * 192,
                frames: 192,
                nominal_rate: 48_000,
                stamp: Stamp {
                    host_callback_ns: 1_000_000_000 + ns,
                    backend_callback_ns: ns as i64,
                    backend_frame_ns: ns as i64,
                    quality: Quality::BackendEstimate,
                    offset_uncertainty_ns: None,
                },
            });
        }
        let map = fitter.map().unwrap();
        let now = 1_120_000_000;
        (key, map, now)
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn applied_record_boundary_trims_real_wav_channels_across_input_order_and_partitions() {
        let (key, map, now) = recording_map();
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let data: Vec<f32> = (0..12).flat_map(|frame| [frame as f32 / 20.0, -(frame as f32) / 20.0]).collect();
        for (case, parts) in [vec![12], vec![4, 4, 4], vec![1, 2, 5, 1, 3]].iter().enumerate() {
            for published_after in 0..=parts.len() {
                let path = std::env::temp_dir().join(format!("soundcraft-record-start-{}-{unique}-{case}-{published_after}.wav", std::process::id()));
                let mut takes = [TakeWriter::create(TrackId(1), vec![1, 0], path.clone(), 48_000).unwrap()];
                let shared = Shared::new(CaptureRing::new(64));
                shared.first_frame.store(100, Ordering::Release);
                let (mut control, mut publisher) = start::pair(soundcraft_mix::EngineEpoch(5));
                let reader = control.prepare().unwrap();
                let ticket = reader.ticket();
                let (mut mapping, input_start) = start::mapping::pair(reader.clone(), key);
                let mut gate = WriterStart { reader, history: start::History::new(2, 24), input_start, placed: false, next: None };
                let point = start::Point {
                    host_ns: map.frame_to_host(key, 104, now).unwrap(),
                    output_frame: 900,
                    position: crate::timeline_map::Position { sample: 500, fraction: 0, denominator: 1 },
                };
                let mut offset = 0;
                for (index, &frames) in parts.iter().enumerate() {
                    if index == published_after {
                        assert!(publisher.start(ticket, point));
                        // Supply the deterministic device-clock snapshot at the
                        // exact placement seam; all WAV/history work stays real.
                        assert!(mapping.service(Some(map), Some(now)));
                    }
                    gate.write(&shared, &mut takes, &data[offset * 2..(offset + frames) * 2], 2).unwrap();
                    if index < published_after {
                        assert_eq!(takes[0].frames, 0);
                    }
                    offset += frames;
                }
                if published_after == parts.len() {
                    assert!(publisher.start(ticket, point));
                    assert!(mapping.service(Some(map), Some(now)));
                }
                control.cancel(); // Stop retains the applied boundary for final drain.
                gate.write(&shared, &mut takes, &[], 2).unwrap();
                takes[0].write_header();
                assert_eq!(takes[0].frames, 8);
                assert!(takes[0].error.is_none());
                drop(takes);
                let channels = decode(&path);
                assert_eq!(channels[0], (4..12).map(|frame| -(frame as f32) / 20.0).collect::<Vec<_>>());
                assert_eq!(channels[1], (4..12).map(|frame| frame as f32 / 20.0).collect::<Vec<_>>());
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn stopped_writer_waits_for_clock_ack_without_another_input_callback() {
        use std::sync::mpsc;
        let (key, map, now) = recording_map();
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        for success in [false, true] {
            let path = std::env::temp_dir().join(format!("soundcraft-final-start-{}-{unique}-{success}.wav", std::process::id()));
            let takes = vec![TakeWriter::create(TrackId(1), vec![0], path.clone(), 48_000).unwrap()];
            let shared = Arc::new(Shared::new(CaptureRing::new(32)));
            shared.start_capture();
            {
                let _lease = shared.begin_capture().unwrap();
                shared.capture_at(100, &(0..12).map(|i| i as f32 / 20.0).collect::<Vec<_>>());
            }
            shared.stop_capture();
            assert!(shared.quiescent());
            let (mut control, mut publisher) = start::pair(soundcraft_mix::EngineEpoch(5));
            let reader = control.prepare().unwrap();
            let (mut mapping, input_start) = start::mapping::pair(reader.clone(), key);
            let gate = WriterStart { reader: reader.clone(), history: start::History::new(1, 24), input_start, placed: false, next: None };
            let point = start::Point {
                host_ns: map.frame_to_host(key, 104, now).unwrap(),
                output_frame: 900,
                position: crate::timeline_map::Position { sample: 500, fraction: 0, denominator: 1 },
            };
            assert!(publisher.start(reader.ticket(), point));
            control.cancel();
            let (entered, waiting) = mpsc::sync_channel(0);
            let (release, resume) = mpsc::sync_channel(0);
            let writer = std::thread::spawn(move || {
                PLACEMENT_WAIT_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        entered.send(()).unwrap();
                        resume.recv().unwrap();
                    }))
                });
                write_takes_started(&shared, takes, 1, Some(gate))
            });
            let reached = waiting.recv_timeout(Duration::from_secs(30));
            if success {
                assert!(mapping.service(Some(map), Some(now)));
            }
            drop(mapping); // Failure/shutdown must also end finalization.
            let _ = release.send(());
            let (takes, error) = writer.join().unwrap();
            assert!(reached.is_ok(), "writer returned before acknowledging its applied start");
            if success {
                assert!(error.is_none(), "{error:?}");
                assert_eq!(takes.len(), 1);
                assert_eq!(decode(&path)[0], (4..12).map(|i| i as f32 / 20.0).collect::<Vec<_>>());
                std::fs::remove_file(path).unwrap();
            } else {
                assert!(takes.is_empty());
                assert!(error.is_some());
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn a_full_ring_owes_the_lost_samples_as_silence() {
        let shared = Shared::new(CaptureRing::new(8));
        let mut out = Vec::new();
        shared.capture(&[1.0; 6]);
        shared.capture(&[2.0; 4]);
        shared.ring.pop_all(&mut out);
        shared.capture(&[3.0; 2]);
        shared.ring.pop_all(&mut out);
        assert_eq!(out, [vec![1.0; 6], vec![2.0; 2], vec![0.0; 2], vec![3.0; 2]].concat());
        assert_eq!((shared.pending_silence.load(Ordering::Relaxed), shared.dropped.load(Ordering::Relaxed)), (0, 2));
    }

    #[test]
    fn nan_and_infinite_input_records_as_silence() {
        let shared = Shared::new(CaptureRing::new(8));
        let mut out = Vec::new();
        shared.capture(&[0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5]);
        shared.ring.pop_all(&mut out);
        assert_eq!(out, [0.5, 0.0, 0.0, 0.0, -0.5]);
    }

    fn recorder(shared: &Arc<Shared>) -> Recorder {
        Recorder {
            shared: Arc::clone(shared),
            monitor: InputRing::new(1, 48_000),
            #[cfg(not(target_os = "freebsd"))]
            _stream: None,
            clock: None,
            record_start: None,
            device_name: "Mic In".into(),
            sample_rate: 48_000,
            channels: 1,
            writer: None,
            seen: (0, Instant::now()),
        }
    }

    #[test]
    fn stop_waits_for_an_admitted_push_before_the_final_drain() {
        use std::sync::mpsc::channel;
        let shared = Arc::new(Shared::new(CaptureRing::new(8)));
        shared.start_capture();
        let (entered_tx, entered_rx) = channel();
        let (resume_tx, resume_rx) = channel();
        let producer = Arc::clone(&shared);
        let worker = std::thread::spawn(move || {
            let _lease = producer.begin_capture().unwrap();
            producer.ring.push(4, |i| {
                if i == 0 {
                    entered_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                }
                (i + 1) as f32
            });
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        shared.stop_capture();
        let mut block = Vec::new();
        let premature_exit = shared.drain_capture(&mut block);
        // Release and join even if the assertion fails, so a negative-control run leaves no
        // parked producer. This is the writer's production exit predicate, not a timing sleep.
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(!premature_exit, "writer exited before the admitted producer published its head");
        assert!(shared.drain_capture(&mut block));
        assert_eq!(block, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn stop_closes_admission_and_rearm_rejects_an_old_generation() {
        let shared = Shared::new(CaptureRing::new(8));
        shared.start_capture();
        let before_stop = shared.capture_state.load(Ordering::Acquire);
        shared.stop_capture();
        assert!(shared.admit_capture(before_stop).is_none());
        assert!(shared.begin_capture().is_none());
        assert!(shared.quiescent(), "no future device callback is needed");
        shared.start_capture();
        assert!(shared.admit_capture(before_stop).is_none(), "an old callback must not enter the new take");
        let lease = shared.begin_capture().unwrap();
        assert!(shared.begin_capture().is_none(), "there can only be one producer");
        shared.stop_capture();
        assert!(!shared.quiescent());
        drop(lease);
        assert!(shared.quiescent());
    }

    #[test]
    fn a_panicking_producer_releases_its_capture_lease() {
        let shared = Shared::new(CaptureRing::new(8));
        shared.start_capture();
        shared.guarded(|| {
            let _lease = shared.begin_capture().unwrap();
            std::panic::panic_any("input failure after admission");
        });
        shared.stop_capture();
        assert!(shared.quiescent());
        assert_eq!(shared.fault.load(Ordering::Relaxed), BROKEN);
    }

    #[test]
    fn stop_before_any_input_callback_does_not_need_an_ack_callback() {
        let shared = Arc::new(Shared::new(CaptureRing::new(8)));
        let mut r = recorder(&shared);
        r.arm(Vec::new()).unwrap();
        assert_eq!(r.finish(), (Vec::new(), None));
        assert!(shared.quiescent());
    }

    #[test]
    fn a_writer_panic_is_reported_with_capture_closed() {
        let shared = Arc::new(Shared::new(CaptureRing::new(8)));
        shared.start_capture();
        let mut r = recorder(&shared);
        r.writer = Some((std::thread::spawn(|| std::panic::panic_any("writer failure")), Vec::new()));
        let (takes, problem) = r.finish();
        assert!(takes.is_empty());
        assert!(problem.is_some_and(|e| e.contains("writer stopped unexpectedly")));
        assert!(shared.quiescent());
        assert!(shared.begin_capture().is_none());
    }

    #[test]
    fn writer_panic_waits_for_admitted_capture_and_discards_old_debt_before_rearm() {
        use std::sync::mpsc::channel;
        let dir = std::env::temp_dir().join(format!("soundcraft-panic-rearm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("new.wav");
        let shared = Arc::new(Shared::new(CaptureRing::new(8)));
        shared.start_capture();
        let mut r = recorder(&shared);
        let (admitted_tx, admitted_rx) = channel();
        let (resume_tx, resume_rx) = channel();
        let producer_shared = Arc::clone(&shared);
        let producer = std::thread::spawn(move || {
            let _lease = producer_shared.begin_capture().unwrap();
            // No healthy writer drains this take. Overflow must not become silence in the next.
            producer_shared.capture(&[0.5; 12]);
            admitted_tx.send(()).unwrap();
            resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            producer_shared.capture(&[0.75; 2]);
        });
        admitted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        r.writer = Some((std::thread::spawn(|| std::panic::panic_any("writer failure with capture in flight")), Vec::new()));
        let hook_resume = resume_tx.clone();
        CAPTURE_WAIT_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let _ = hook_resume.send(());
            }))
        });
        let (old_takes, problem) = r.finish();
        let quiescent_at_return = shared.quiescent();
        CAPTURE_WAIT_HOOK.with(|h| h.borrow_mut().take());
        // If a mutation bypasses the wait, still release/join the producer and clean up, then
        // fail on the observed state. No elapsed quiet-time window determines correctness.
        let _ = resume_tx.send(());
        producer.join().unwrap();
        let old_debt = shared.pending_silence.load(Ordering::Relaxed);

        r.arm(vec![(TrackId(1), vec![0], path.clone())]).unwrap();
        {
            let _lease = shared.begin_capture().unwrap();
            shared.capture(&[0.25, -0.25]);
        }
        let (new_takes, new_problem) = r.finish();
        let audio = decode(&path);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(quiescent_at_return, "finish returned while the old callback still held its lease");
        assert!(old_takes.is_empty());
        assert!(problem.is_some_and(|e| e.contains("writer stopped unexpectedly")));
        assert_eq!(old_debt, 6, "the panicked writer could not perform its final silence drain");
        assert_eq!((new_takes.len(), new_problem), (1, None));
        assert_eq!(audio, [vec![0.25, -0.25]], "old-take silence debt entered the new WAV");
    }

    #[test]
    fn final_overflow_silence_preserves_multichannel_length_and_does_not_leak_on_rearm() {
        let dir = std::env::temp_dir().join(format!("soundcraft-final-capture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Arc::new(Shared::new(CaptureRing::new(8)));
        let mut r = recorder(&shared);
        r.channels = 3;
        let first_path = dir.join("first.wav");
        // Hold the producer lease through Stop: an admitted callback must finish as part of
        // this take. Pre-fill before starting the writer to force the overflow deterministically.
        shared.start_capture();
        {
            let _lease = shared.begin_capture().unwrap();
            shared.stop_capture();
            shared.capture(&(1..=12).map(|i| i as f32 / 16.0).collect::<Vec<_>>());
        }
        let take = TakeWriter::create(TrackId(1), vec![0, 1, 2], first_path.clone(), 48_000).unwrap();
        let (done, problem) = write_takes(&shared, vec![take], 3);
        assert_eq!((done.len(), problem), (1, None));
        assert_eq!(decode(&first_path), [vec![0.0625, 0.25, 0.4375, 0.0], vec![0.125, 0.3125, 0.5, 0.0], vec![0.1875, 0.375, 0.0, 0.0]]);
        assert_eq!(shared.pending_silence.load(Ordering::Relaxed), 0);
        r.finish();
        // No idle input callback between takes: the next take still starts with no silence debt.
        let second_path = dir.join("second.wav");
        r.arm(vec![(TrackId(2), vec![0, 1, 2], second_path.clone())]).unwrap();
        {
            let _lease = shared.begin_capture().unwrap();
            shared.capture(&[0.25, 0.5, 0.75]);
        }
        assert!(r.finish().1.is_none());
        assert_eq!(decode(&second_path), [vec![0.25], vec![0.5], vec![0.75]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bug_a_stall_or_a_lost_device_is_trouble() {
        let shared = Arc::new(Shared::new(CaptureRing::new(64)));
        let mut r = recorder(&shared);
        shared.start_capture();
        assert_eq!(r.trouble(), None);
        r.seen.1 = Instant::now().checked_sub(STALL).unwrap();
        assert_eq!(r.trouble(), Some(Trouble::Device("“Mic In” stopped sending audio".into())));
        shared.guarded(|| {});
        assert_eq!(r.trouble(), None, "it sent again");
        shared.stop_capture();
        r.seen.1 = Instant::now().checked_sub(STALL).unwrap();
        assert_eq!(r.trouble(), None, "a quiet input that isn't recording is fine");
        shared.guarded(|| std::panic::panic_any("a bug in the input"));
        let mut ran = false;
        shared.guarded(|| ran = true);
        assert!(!ran);
        assert_eq!(r.trouble(), Some(Trouble::Device("the audio input hit an internal error (see the log)".into())));
        let gone = Arc::new(Shared::new(CaptureRing::new(64)));
        gone.fault.store(GONE, Ordering::Relaxed);
        assert_eq!(recorder(&gone).trouble(), Some(Trouble::Device("“Mic In” was disconnected".into())));
    }

    #[test]
    fn a_failed_write_is_trouble_while_still_recording() {
        let dir = std::env::temp_dir().join(format!("soundcraft-failed-take-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut take = TakeWriter::create(TrackId(1), vec![0], dir.join("Vox_01.wav"), 48_000).unwrap();
        take.file = File::open(&take.path).unwrap();
        let shared = Arc::new(Shared::new(CaptureRing::new(1 << 12)));
        shared.start_capture();
        let mut r = recorder(&shared);
        let sh = Arc::clone(&shared);
        let writer = std::thread::spawn(move || write_takes(&sh, vec![take], 1));
        shared.capture(&[0.5; 480]);
        let trouble = (0..300).find_map(|_| {
            std::thread::sleep(Duration::from_millis(10));
            r.trouble()
        });
        assert!(matches!(&trouble, Some(Trouble::Disk(e)) if e.contains("Vox_01.wav")), "{trouble:?}");
        assert_eq!(r.trouble(), None, "reported once");
        shared.stop_capture();
        assert!(writer.join().unwrap().1.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn takes_stream_into_their_files() {
        let dir = std::env::temp_dir().join(format!("soundcraft-takes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let gtr = TakeWriter::create(TrackId(1), vec![1, 2], dir.join("Gtr_01.wav"), 48_000).unwrap();
        let mut vox = TakeWriter::create(TrackId(2), vec![0], dir.join("Vox_01.wav"), 48_000).unwrap();
        assert!(TakeWriter::create(TrackId(3), vec![0], dir.join("Vox_01.wav"), 48_000).is_err(), "never overwrites a file");
        vox.write(&[0.5; 300], 3);
        vox.write_header();
        vox.write(&[0.25; 150], 3);
        assert_eq!(decode(&dir.join("Vox_01.wav"))[0].len(), 100, "the header covers what it has seen");

        let shared = Arc::new(Shared::new(CaptureRing::new(1 << 16)));
        shared.start_capture();
        let sh = Arc::clone(&shared);
        let writer = std::thread::spawn(move || write_takes(&sh, vec![gtr, vox], 3));
        let input: Vec<f32> = (0..3 * 4_800).map(|i| i as f32 / 20_000.0).collect();
        input.chunks(3 * 480).for_each(|block| shared.capture(block));
        shared.stop_capture();
        let (done, problem) = writer.join().unwrap();
        let channel = |c: usize| input.iter().skip(c).step_by(3).copied().collect::<Vec<f32>>();
        assert_eq!((done.len(), problem), (2, None));
        assert_eq!(decode(&done[0].1), vec![channel(1), channel(2)]);
        assert_eq!(decode(&done[1].1), vec![[vec![0.5; 100], vec![0.25; 50], channel(0)].concat()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_sums_itself_up_for_drawing_while_it_records() {
        let dir = std::env::temp_dir().join(format!("soundcraft-live-take-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut take = TakeWriter::create(TrackId(4), vec![2, 0], dir.join("Gtr_01.wav"), 48_000).unwrap();
        let input: Vec<f32> = (0..300).flat_map(|f| [f as f32, -1.0, -(f as f32)]).collect();
        take.write(&input[..3 * 100], 3);
        take.write(&input[3 * 100..], 3);
        let live = take.live.lock().unwrap().clone();
        assert_eq!(live, [[(-127.0, 0.0), (-255.0, -128.0), (-299.0, -256.0)], [(0.0, 127.0), (128.0, 255.0), (256.0, 299.0)]]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Lifetime monitor diagnostics. Independent fields may straddle a callback.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MonitorTelemetry {
    pub overflow_frames: u64,
    pub underrun_frames: u64,
    pub input_contention_frames: u64,
    pub output_contention_frames: u64,
    pub input_fault_frames: u64,
    pub output_fault_frames: u64,
    /// Frames deliberately discarded at clear or after a real overflow.
    pub discarded_frames: u64,
    pub overflow_recoveries: u64,
    pub incomplete_input_samples: u64,
    /// Requested output frames that did not fit the supplied channel storage.
    pub short_output_frames: u64,
}

#[derive(Default)]
struct MonitorCounters {
    overflow: AtomicU64,
    underrun: AtomicU64,
    input_contention: AtomicU64,
    output_contention: AtomicU64,
    input_fault: AtomicU64,
    output_fault: AtomicU64,
    discarded: AtomicU64,
    recoveries: AtomicU64,
    incomplete: AtomicU64,
    short_output: AtomicU64,
}

struct MonitorReader {
    queue: rtrb::Consumer<f32>,
    left: Vec<f32>,
    right: Vec<f32>,
    left_valid: bool,
    right_valid: bool,
    phase: f64,
    removed_frames: u64,
}

impl MonitorReader {
    fn frame(queue: &mut rtrb::Consumer<f32>, frame: &mut [f32], removed: &mut u64) -> bool {
        if queue.slots() < frame.len() {
            return false;
        }
        for sample in frame {
            // Only this serialized consumer pops; a producer cannot remove available samples.
            *sample = queue.pop().unwrap_or(0.0);
        }
        *removed = removed.wrapping_add(1);
        true
    }

    fn history_frames(&self) -> usize {
        // `left` is already consumed when phase >= 1; a prefetched right is still pending.
        usize::from(self.left_valid && self.phase < 1.0) + usize::from(self.right_valid)
    }

    fn retired_input(&self) -> u64 {
        self.removed_frames.wrapping_sub(self.history_frames() as u64)
    }

    /// Discard only the frames present at entry, never chase a running producer.
    fn clear(&mut self) -> usize {
        let ch = self.left.len();
        let queued = self.queue.slots() / ch;
        if let Ok(chunk) = self.queue.read_chunk(queued * ch) {
            // Commit the discard in one queue operation; f32 carries no destructor.
            chunk.commit_all();
        }
        self.removed_frames = self.removed_frames.wrapping_add(queued as u64);
        let held = self.history_frames();
        self.left_valid = false;
        self.right_valid = false;
        self.phase = 0.0;
        queued + held
    }

    fn render(&mut self, out: &mut [Vec<f32>], frames: usize, step: f64) -> usize {
        let mut written = 0;
        for f in 0..frames {
            if !self.left_valid {
                self.left_valid = Self::frame(&mut self.queue, &mut self.left, &mut self.removed_frames);
                if !self.left_valid {
                    break;
                }
            }
            while self.phase >= 1.0 {
                if self.right_valid {
                    std::mem::swap(&mut self.left, &mut self.right);
                    self.right_valid = false;
                } else if !Self::frame(&mut self.queue, &mut self.left, &mut self.removed_frames) {
                    return written;
                }
                self.phase -= 1.0;
            }
            // An exact sample needs no lookahead. Equal rates therefore add no interpolation
            // frame of latency and consume the final available frame, even for a one-frame call.
            if self.phase > 0.0 && !self.right_valid {
                self.right_valid = Self::frame(&mut self.queue, &mut self.right, &mut self.removed_frames);
                if !self.right_valid {
                    break;
                }
            }
            let fraction = self.phase as f32;
            for ((channel, &a), &b) in out.iter_mut().zip(&self.left).zip(&self.right) {
                if let Some(x) = channel.get_mut(f) {
                    *x = if fraction == 0.0 { a } else { finite(a * (1.0 - fraction) + b * fraction) };
                }
            }
            self.phase += step;
            written += 1;
        }
        written
    }
}

/// A bounded interleaved FIFO for input monitoring. The input and output roles have
/// separate locks, so they never exclude one another. Each callback only tries its own
/// role lock; contention from a second caller or a control-side clear is counted.
///
/// Normal callback partitioning never discards audio. Full storage rejects whole new
/// frames and requests a counted flush on the next read. A smaller stall can leave a
/// persistent backlog, exposed by [`Self::buffered_input_frames`]; there is no automatic
/// latency-target policy. Rate conversion is currently linear interpolation: it is
/// not bandlimited SRC or compensation for independent device-clock drift.
pub struct InputRing {
    producer: Mutex<rtrb::Producer<f32>>,
    reader: Mutex<MonitorReader>,
    /// Device channel count, for consumers to size their prepared storage. Read-only by contract.
    pub channels: std::sync::atomic::AtomicUsize,
    ch: usize,
    fifo_frames: usize,
    accepted_frames: AtomicU64,
    retired_frames: AtomicU64,
    /// Input device sample rate in Hz (0 when unknown, which reads as "same as the session").
    rate: u32,
    overrun_pending: std::sync::atomic::AtomicBool,
    clear_pending: std::sync::atomic::AtomicBool,
    counters: MonitorCounters,
}

impl InputRing {
    /// Prepare up to half a second of storage, capped at 16 MiB of sample data. Channels
    /// follow CPAL's u16 domain (zero normalizes to one); an extreme format gets a shorter
    /// FIFO, not an unbounded allocation. Interpolation history adds two device frames.
    pub fn new(channels: usize, rate: u32) -> Arc<InputRing> {
        let ch = channels.clamp(1, usize::from(u16::MAX));
        let wanted = if rate == 0 { 24_000 } else { (rate as usize / 2).max(1) };
        let frames = wanted.min((1 << 22) / ch);
        let (producer, queue) = rtrb::RingBuffer::new(frames * ch);
        Arc::new(InputRing {
            producer: Mutex::new(producer),
            reader: Mutex::new(MonitorReader {
                queue,
                left: vec![0.0; ch],
                right: vec![0.0; ch],
                left_valid: false,
                right_valid: false,
                phase: 0.0,
                removed_frames: 0,
            }),
            channels: std::sync::atomic::AtomicUsize::new(ch),
            ch,
            fifo_frames: frames,
            accepted_frames: AtomicU64::new(0),
            retired_frames: AtomicU64::new(0),
            rate,
            overrun_pending: std::sync::atomic::AtomicBool::new(false),
            clear_pending: std::sync::atomic::AtomicBool::new(false),
            counters: MonitorCounters::default(),
        })
    }

    /// Each call supplies complete interleaved frames. A trailing partial frame is
    /// counted and discarded; the next call begins a new frame, not its continuation.
    pub fn push(&self, samples: &[f32]) {
        let frames = samples.len() / self.ch;
        self.counters.incomplete.fetch_add((samples.len() % self.ch) as u64, Ordering::Relaxed);
        let mut producer = match self.producer.try_lock() {
            Ok(p) => p,
            Err(std::sync::TryLockError::WouldBlock) => {
                self.counters.input_contention.fetch_add(frames as u64, Ordering::Relaxed);
                return;
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                self.counters.input_fault.fetch_add(frames as u64, Ordering::Relaxed);
                return;
            }
        };
        let accepted = frames.min(producer.slots() / self.ch);
        for &sample in samples.iter().take(accepted * self.ch) {
            // Capacity was reserved by this sole serialized producer; the consumer only frees it.
            let _ = producer.push(finite(sample));
        }
        self.accepted_frames.fetch_add(accepted as u64, Ordering::Release);
        let lost = frames - accepted;
        if lost > 0 {
            self.counters.overflow.fetch_add(lost as u64, Ordering::Relaxed);
            self.overrun_pending.store(true, Ordering::Release);
        }
    }

    /// Control thread: discard queued/history frames if the reader is idle, otherwise
    /// request a flush at the next output call (including input arriving before that flush).
    /// Never waits for the reader: its callback unlock must not wake a blocked control waiter.
    pub fn clear_monitor(&self) {
        if let Ok(mut reader) = self.reader.try_lock() {
            self.clear_pending.swap(false, Ordering::AcqRel);
            let overrun = self.overrun_pending.swap(false, Ordering::AcqRel);
            self.counters.discarded.fetch_add(reader.clear() as u64, Ordering::Relaxed);
            self.counters.recoveries.fetch_add(u64::from(overrun), Ordering::Relaxed);
            self.retired_frames.store(reader.retired_input(), Ordering::Release);
        } else {
            self.clear_pending.store(true, Ordering::Release);
        }
    }

    /// Fill `out` (planar, at `session_rate`) with up to `frames` frames, converting from the
    /// input rate. Missing frames are counted silence. No allocation or wait occurs here.
    pub fn pop_into(&self, out: &mut [Vec<f32>], frames: usize, session_rate: u32) {
        for c in out.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
        let usable = out.iter().take(self.ch).map(Vec::len).min().unwrap_or(0).min(frames);
        self.counters.short_output.fetch_add((frames - usable) as u64, Ordering::Relaxed);
        let frames = usable;
        let mut reader = match self.reader.try_lock() {
            Ok(r) => r,
            Err(std::sync::TryLockError::WouldBlock) => {
                self.counters.output_contention.fetch_add(frames as u64, Ordering::Relaxed);
                return;
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                self.counters.output_fault.fetch_add(frames as u64, Ordering::Relaxed);
                return;
            }
        };
        let overrun = self.overrun_pending.swap(false, Ordering::AcqRel);
        let clear = self.clear_pending.swap(false, Ordering::AcqRel);
        if overrun || clear {
            self.counters.discarded.fetch_add(reader.clear() as u64, Ordering::Relaxed);
            self.counters.recoveries.fetch_add(u64::from(overrun), Ordering::Relaxed);
        }
        let step = if self.rate == 0 || session_rate == 0 { 1.0 } else { f64::from(self.rate) / f64::from(session_rate) };
        let written = reader.render(out, frames, step);
        self.retired_frames.store(reader.retired_input(), Ordering::Release);
        self.counters.underrun.fetch_add((frames - written) as u64, Ordering::Relaxed);
    }

    /// Approximate buffered input frames, including pending interpolation history.
    /// Snapshots can straddle concurrent calls; this is a bounded diagnostic, not a
    /// calibrated latency measurement or a synchronization/control input.
    pub fn buffered_input_frames(&self) -> usize {
        let accepted = self.accepted_frames.load(Ordering::Acquire);
        let retired = self.retired_frames.load(Ordering::Acquire);
        accepted.saturating_sub(retired).min((self.fifo_frames + 2) as u64) as usize
    }

    /// FIFO capacity in input frames, excluding two interpolation-history frames.
    pub fn fifo_capacity_frames(&self) -> usize {
        self.fifo_frames
    }

    pub fn telemetry(&self) -> MonitorTelemetry {
        MonitorTelemetry {
            overflow_frames: self.counters.overflow.load(Ordering::Relaxed),
            underrun_frames: self.counters.underrun.load(Ordering::Relaxed),
            input_contention_frames: self.counters.input_contention.load(Ordering::Relaxed),
            output_contention_frames: self.counters.output_contention.load(Ordering::Relaxed),
            input_fault_frames: self.counters.input_fault.load(Ordering::Relaxed),
            output_fault_frames: self.counters.output_fault.load(Ordering::Relaxed),
            discarded_frames: self.counters.discarded.load(Ordering::Relaxed),
            overflow_recoveries: self.counters.recoveries.load(Ordering::Relaxed),
            incomplete_input_samples: self.counters.incomplete.load(Ordering::Relaxed),
            short_output_frames: self.counters.short_output.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod ring_tests {
    use super::InputRing;

    #[test]
    fn read_waits_for_a_whole_frame_when_publication_pauses_between_channels() {
        let ring = InputRing::new(3, 48_000);
        let mut producer = ring.producer.lock().unwrap();
        // Model a producer preempted between sample stores inside one complete-frame push.
        producer.push(10.0).unwrap();
        let mut output = vec![vec![9.0]; 3];
        ring.pop_into(&mut output, 1, 48_000);
        assert_eq!(output, vec![vec![0.0]; 3]);
        assert_eq!(ring.reader.lock().unwrap().queue.slots(), 1);
        producer.push(20.0).unwrap();
        producer.push(30.0).unwrap();
        ring.accepted_frames.fetch_add(1, super::Ordering::Release);
        drop(producer);
        ring.pop_into(&mut output, 1, 48_000);
        assert_eq!(output, vec![vec![10.0], vec![20.0], vec![30.0]]);
        assert_eq!(ring.buffered_input_frames(), 0);
        assert_eq!(ring.telemetry().underrun_frames, 1);
    }

    #[test]
    fn clear_preserves_a_partially_published_frame() {
        let ring = InputRing::new(3, 48_000);
        ring.push(&[1.0, 2.0, 3.0]);
        let mut producer = ring.producer.lock().unwrap();
        producer.push(10.0).unwrap();
        ring.clear_monitor();
        assert_eq!(ring.reader.lock().unwrap().queue.slots(), 1);
        assert_eq!(ring.telemetry().discarded_frames, 1);
        producer.push(20.0).unwrap();
        producer.push(30.0).unwrap();
        ring.accepted_frames.fetch_add(1, super::Ordering::Release);
        drop(producer);
        let mut output = vec![vec![0.0]; 3];
        ring.pop_into(&mut output, 1, 48_000);
        assert_eq!(output, vec![vec![10.0], vec![20.0], vec![30.0]]);
        assert_eq!(ring.buffered_input_frames(), 0);
    }

    #[test]
    fn input_and_output_roles_do_not_exclude_each_other() {
        let ring = InputRing::new(1, 48_000);
        let read_guard = ring.reader.lock().unwrap();
        ring.push(&[1.0, 2.0]);
        drop(read_guard);
        let write_guard = ring.producer.lock().unwrap();
        let mut output = vec![vec![0.0; 2]];
        ring.pop_into(&mut output, 2, 48_000);
        drop(write_guard);
        assert_eq!(output[0], [1.0, 2.0]);
        assert_eq!(ring.telemetry(), super::MonitorTelemetry::default());
    }

    #[test]
    fn same_role_contention_is_counted_and_never_waits() {
        let ring = InputRing::new(2, 48_000);
        let write_guard = ring.producer.lock().unwrap();
        ring.push(&[1.0; 6]);
        drop(write_guard);
        let read_guard = ring.reader.lock().unwrap();
        let mut output = vec![vec![1.0; 4]; 2];
        ring.pop_into(&mut output, 4, 48_000);
        drop(read_guard);
        assert!(output.iter().flatten().all(|&x| x == 0.0));
        assert_eq!(ring.telemetry().input_contention_frames, 3);
        assert_eq!(ring.telemetry().output_contention_frames, 4);
        assert_eq!(ring.telemetry().underrun_frames, 0);
    }

    #[test]
    fn clear_while_reading_defers_without_a_waiter_on_the_audio_lock() {
        let ring = InputRing::new(1, 48_000);
        ring.push(&[1.0, 2.0]);
        let read_guard = ring.reader.lock().unwrap();
        ring.clear_monitor();
        ring.push(&[3.0]);
        drop(read_guard);
        let mut output = vec![vec![9.0; 2]];
        ring.pop_into(&mut output, 2, 48_000);
        assert_eq!(output[0], [0.0, 0.0]);
        assert_eq!(ring.telemetry().discarded_frames, 3);
        assert_eq!(ring.telemetry().overflow_recoveries, 0);
    }

    #[test]
    fn poisoned_roles_fail_closed_and_are_distinct_from_contention() {
        let ring = InputRing::new(1, 48_000);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ring.producer.lock().unwrap();
            panic!("injected producer fault");
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ring.reader.lock().unwrap();
            panic!("injected reader fault");
        }));
        ring.push(&[1.0; 3]);
        let mut output = vec![vec![1.0; 4]];
        ring.pop_into(&mut output, 4, 48_000);
        assert_eq!(output[0], [0.0; 4]);
        let t = ring.telemetry();
        assert_eq!((t.input_fault_frames, t.output_fault_frames), (3, 4));
        assert_eq!((t.input_contention_frames, t.output_contention_frames), (0, 0));
    }

    /// A 1 kHz sine recorded at 16 kHz must come out as a 1 kHz sine at 48 kHz, across calls.
    #[test]
    fn monitor_converts_input_rate_to_session_rate() {
        let ring = InputRing::new(1, 16_000);
        let input: Vec<f32> = (0..4_000).map(|i| (2.0 * std::f64::consts::PI * 1_000.0 * f64::from(i) / 16_000.0).sin() as f32).collect();
        // Fed a block at a time, as the device does; callback size never trims the FIFO.
        ring.push(&input[..32]);
        let mut out = vec![vec![0.0f32; 48]];
        let mut k = 0usize;
        for i in 0..10 {
            ring.pop_into(&mut out, 48, 48_000);
            ring.push(&input[32 + 16 * i..48 + 16 * i]);
            for v in &out[0] {
                let want = (2.0 * std::f64::consts::PI * 1_000.0 * k as f64 / 48_000.0).sin() as f32;
                assert!((v - want).abs() < 0.05, "sample {k}: got {v}, want {want}");
                k += 1;
            }
        }
    }

    /// Input faster than the session (96 kHz into 48 kHz): every other frame is read, in order.
    #[test]
    fn monitor_decimates_when_input_is_faster() {
        let ring = InputRing::new(1, 96_000);
        let input: Vec<f32> = (0..400).map(|i| i as f32).collect();
        ring.push(&input);
        let mut out = vec![vec![0.0f32; 100]];
        ring.pop_into(&mut out, 100, 48_000);
        for (k, v) in out[0].iter().enumerate() {
            assert_eq!(*v, (2 * k) as f32, "output {k}");
        }
    }

    /// Same rate: samples pass through unchanged.
    #[test]
    fn monitor_passes_through_at_equal_rates() {
        let ring = InputRing::new(2, 48_000);
        let input: Vec<f32> = (0..200).map(|i| i as f32).collect();
        ring.push(&input);
        let mut out = vec![vec![0.0f32; 50], vec![0.0f32; 50]];
        ring.pop_into(&mut out, 50, 48_000);
        assert_eq!(out[0][0], 0.0);
        assert_eq!(out[1][0], 1.0);
        assert_eq!(out[0][49], 98.0);
    }

    /// A ring filled before anything read it (the ASIO input runs from startup) is emptied at
    /// hand-over: the monitor starts from the next block.
    #[test]
    fn clear_monitor_empties_the_ring() {
        let ring = InputRing::new(1, 48_000);
        ring.push(&vec![0.5; 48_000]);
        ring.clear_monitor();
        let mut out = vec![vec![1.0f32; 64]];
        ring.pop_into(&mut out, 64, 48_000);
        assert!(out[0].iter().all(|v| *v == 0.0), "nothing left to hear");
        assert_eq!(ring.reader.lock().unwrap().queue.slots(), 0);
    }

    /// Callback size is not a latency policy: a larger input block must survive smaller reads.
    #[test]
    fn pop_preserves_a_larger_input_block() {
        let ring = InputRing::new(1, 48_000);
        let input: Vec<f32> = (0..4 * 256).map(|i| i as f32).collect();
        ring.push(&input);
        let mut out = vec![vec![0.0f32; 256]];
        ring.pop_into(&mut out, 256, 48_000);
        assert_eq!(out[0], input[..256]);
        ring.pop_into(&mut out, 256, 48_000);
        assert_eq!(out[0], input[256..512]);
        assert_eq!(ring.telemetry(), super::MonitorTelemetry::default());
    }

    /// A device whose input reports no channels is not an input (asio-sys would hit `unreachable!`).
    #[test]
    fn an_input_with_no_channels_is_unusable() {
        let cfg = |ch| cpal::SupportedStreamConfig::new(ch, cpal::SampleRate(48_000), cpal::SupportedBufferSize::Unknown, cpal::SampleFormat::F32);
        assert!(!super::input_usable(&cfg(0)));
        assert!(super::input_usable(&cfg(2)));
    }

    /// Nothing buffered: silence, not stale data.
    #[test]
    fn monitor_underrun_is_silence() {
        let ring = InputRing::new(1, 16_000);
        let mut out = vec![vec![1.0f32; 16]];
        ring.pop_into(&mut out, 16, 48_000);
        assert!(out[0].iter().all(|v| *v == 0.0));
    }

    /// NaN and infinities from the device monitor as silence.
    #[test]
    fn monitor_silences_nan_and_infinite_input() {
        let ring = InputRing::new(1, 48_000);
        ring.push(&[0.5, f32::NAN, f32::INFINITY, -0.5, 0.0, 0.0, 0.0, 0.0]);
        let mut out = vec![vec![1.0f32; 4]];
        ring.pop_into(&mut out, 4, 48_000);
        assert_eq!(out[0], [0.5, 0.0, 0.0, -0.5]);
    }
}
