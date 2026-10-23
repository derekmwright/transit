//! The work of one buffer switch, in safe code over the plane traits of [`crate::convert`], so it
//! runs on plain vectors in tests: stamps, conversion, the render and capture callbacks, counters
//! and the delivery of driver events as stream errors. [`crate::callback`] calls it from the
//! driver's thread with the real buffers; nothing here allocates, locks or formats text after open.
//!
//! **Stamps.** The backend clock domain is the driver's sample position (`samplePosition` of the
//! time info, or `getSamplePosition` on the `bufferSwitch` path), in nanoseconds at the nominal
//! rate, anchored once per driver start to the QPC read at the first callback that carried a valid
//! position. Both directions share that anchor, so their frame times are comparable to the frame
//! (what the loopback calibration subtracts). The host time of a block is the QPC read at callback
//! entry, mapped to the stream's `HostOrigin` through the `(QPC, Instant)` pair the stream captured
//! at its `start()` (`relative_ns`, as `transit-audio-wasapi` does). The driver's `systemTime` is not
//! the host stamp: it is a clock of the driver's choosing, often `timeGetTime` with millisecond
//! grain; its smallest step is recorded as a diagnostic only ([`Engine::system_time_step_ns`]).
//! Quality: `BackendEstimate` with a valid position (`Measured` under a calibration), `Arrival`
//! (the QPC arrival as the backend time too) without one.
//!
//! Frame times: a render block's first frame is presented `getLatencies` output frames after the
//! buffer switch, a capture block's first frame was captured `getLatencies` input frames before it
//! (the SDK's latencies count from the buffer switch and include the buffer). The calibration moves
//! the capture frame time earlier by its round trip, as on WASAPI.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use crate::convert::{InputPlanes, OutputPlanes, PlanarSink, PlanarSource, Wire, silence};
use crate::stats::{DriverEvents, LoopStats};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use transit_audio_backend::{
    Calibration, CaptureBlock, CaptureCallback, ErrorCallback, Format, Quality, RenderBlock, RenderCallback, Stamp, StreamError,
};

/// The life of one direction, published by the control side and read by the callback.
pub(crate) const OPENED: u8 = 0;
pub(crate) const STARTED: u8 = 1;
pub(crate) const STOPPED: u8 = 2;
/// The stream was dropped; its callback stays allocated (never called) until the driver stops.
pub(crate) const DETACHED: u8 = 3;

/// What the control side and the callback share for one direction.
pub(crate) struct SideShared {
    pub(crate) state: AtomicU8,
    /// A buffer switch is inside this direction's part (set before it reads `state`, cleared after
    /// the user callback returned): what [`SideShared::quiesce`] waits for.
    in_switch: AtomicBool,
    /// The owner asked for a stop from inside a driver callback: the direction is already
    /// [`STOPPED`], and the host thread finishes the stop (the driver's `stop` when it was the last).
    stop_requested: AtomicBool,
    /// The `(QPC in 100 ns, host-origin ns)` pair of the stream's `start()`, written before
    /// `state` becomes [`STARTED`] (release) and read after it is seen (acquire).
    origin_qpc: AtomicU64,
    origin_ns: AtomicI64,
    pub(crate) stats: LoopStats,
}

impl SideShared {
    pub(crate) fn new(events: Arc<DriverEvents>) -> Self {
        Self {
            state: AtomicU8::new(OPENED),
            in_switch: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            origin_qpc: AtomicU64::new(0),
            origin_ns: AtomicI64::new(0),
            stats: LoopStats::new(events),
        }
    }

    /// The buffer switch enters this direction: marks it in flight, then reads whether it is
    /// started. Both are `SeqCst`, as are the control side's state change and its read of the mark
    /// in [`SideShared::quiesce`], so either the switch sees the stop or the stop sees the switch.
    fn enter(&self) -> bool {
        self.in_switch.store(true, Ordering::SeqCst);
        self.state.load(Ordering::SeqCst) == STARTED
    }

    fn leave(&self) {
        self.in_switch.store(false, Ordering::SeqCst);
    }

    /// Control side, after this direction's state left [`STARTED`] (with `SeqCst`): waits until no
    /// buffer switch is inside its callback, so none runs after `stop()` or a drop returns while
    /// the other direction keeps the driver running. Returns false if one did not leave within
    /// `limit` (a callback that blocks; logged by the caller).
    pub(crate) fn quiesce(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while self.in_switch.load(Ordering::SeqCst) {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    /// Publishes the origin pair, then the started state. False when the stream was not just opened.
    pub(crate) fn start(&self, origin_qpc_100ns: u64, origin_ns: i64) -> bool {
        if self.state.load(Ordering::Acquire) != OPENED {
            return false;
        }
        self.origin_qpc.store(origin_qpc_100ns, Ordering::Relaxed);
        self.origin_ns.store(origin_ns, Ordering::Relaxed);
        self.state.compare_exchange(OPENED, STARTED, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    pub(crate) fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// A stop asked for from inside a driver callback, where waiting for the host thread (or the
    /// driver's `stop`) would re-enter the driver: the direction leaves [`STARTED`] at once, so no
    /// later buffer switch calls it, and the stop is left to the host thread
    /// ([`SideShared::take_stop_request`]). Never blocks or allocates.
    pub(crate) fn request_stop(&self) {
        let _ = self.state.compare_exchange(OPENED, STOPPED, Ordering::SeqCst, Ordering::SeqCst);
        let _ = self.state.compare_exchange(STARTED, STOPPED, Ordering::SeqCst, Ordering::SeqCst);
        self.stop_requested.store(true, Ordering::SeqCst);
    }

    /// Host thread: whether a stop from inside a callback is waiting to be finished (cleared).
    pub(crate) fn take_stop_request(&self) -> bool {
        self.stop_requested.swap(false, Ordering::SeqCst)
    }

    fn origin(&self) -> (u64, i64) {
        (self.origin_qpc.load(Ordering::Relaxed), self.origin_ns.load(Ordering::Relaxed))
    }
}

/// `getLatencies`, in frames, refetched on `kAsioLatenciesChanged`.
#[derive(Default)]
pub(crate) struct Latencies {
    pub(crate) input: AtomicU32,
    pub(crate) output: AtomicU32,
}

/// A QPC reading in 100 ns units as signed nanoseconds in the host origin (as in
/// `transit-audio-wasapi`'s `render_loop.rs`).
pub(crate) fn relative_ns(qpc_100ns: u64, origin_qpc: u64, origin_ns: i64) -> i64 {
    let since = (i128::from(qpc_100ns) - i128::from(origin_qpc)) * 100;
    i64::try_from(i128::from(origin_ns) + since).unwrap_or(i64::MAX)
}

/// The previous stamp's times (callback, frame, host), so the next never goes backward (the same
/// rule as `transit-audio-wasapi`'s `Held`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Held(Option<(i64, i64, u64)>);

impl Held {
    fn hold(&mut self, callback: i64, frame: i64, host: u64, frame_before_callback: bool) -> (i64, i64, u64) {
        let (callback, mut frame, host) = match self.0 {
            Some((c, f, h)) => (callback.max(c), frame.max(f), host.max(h.saturating_add(1))),
            None => (callback, frame, host),
        };
        if frame_before_callback {
            frame = frame.min(callback);
        }
        self.0 = Some((callback, frame, host));
        (callback, frame, host)
    }

    fn reset(&mut self) {
        self.0 = None;
    }
}

/// The quality a stream's position stamps carry: `Measured` with its bound under a calibration.
fn quality_of(calibration: Option<Calibration>) -> (Quality, Option<u64>) {
    match calibration {
        Some(c) => (Quality::Measured, Some(c.uncertainty_ns)),
        None => (Quality::BackendEstimate, None),
    }
}

/// What the driver said about one buffer switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Time {
    /// The sample position, where the driver marked it valid.
    pub(crate) position: Option<u64>,
    /// `systemTime` in nanoseconds, where the driver marked it valid.
    pub(crate) system_time_ns: Option<u64>,
}

/// One direction inside the engine: its callbacks and what only the callback thread touches.
pub(crate) struct Side<C> {
    pub(crate) shared: Arc<SideShared>,
    callback: C,
    error: ErrorCallback,
    format: Format,
    wire: Wire,
    calibration: Option<Calibration>,
    scratch: Vec<f32>,
    held: Held,
    /// The callback panicked: it is not called again and its output is silence.
    faulted: bool,
    /// An error was delivered; one per stream.
    reported: bool,
    /// The next stamp is [`Quality::Invalid`]: the position anchor moved (a resync, a position
    /// that went back or skipped), so the clock fit restarts there, as after a WASAPI
    /// discontinuity.
    invalidate: bool,
}

impl<C> Side<C> {
    /// A direction of `format` on `wire`, with its scratch allocated here (on the control side).
    pub(crate) fn new(
        shared: Arc<SideShared>,
        callback: C,
        error: ErrorCallback,
        format: Format,
        wire: Wire,
        calibration: Option<Calibration>,
        frames: usize,
    ) -> Self {
        let channels = usize::from(format.channels).max(1);
        let scratch = vec![0.0; frames.clamp(1, transit_audio_backend::SCRATCH_FRAMES) * channels];
        Self {
            shared,
            callback,
            error,
            format,
            wire,
            calibration,
            scratch,
            held: Held::default(),
            faulted: false,
            reported: false,
            invalidate: false,
        }
    }

    pub(crate) fn channels(&self) -> usize {
        usize::from(self.format.channels)
    }

    pub(crate) fn wire(&self) -> Wire {
        self.wire
    }

    fn live(&self) -> bool {
        self.shared.state() == STARTED && !self.faulted
    }

    /// [`SideShared::enter`] for a side whose callback has not panicked.
    fn enter(&self) -> bool {
        self.shared.enter() && !self.faulted
    }

    /// Marks `stamp` invalid when a discontinuity asked for it (once).
    fn invalidated(&mut self, stamp: &mut Stamp) {
        if std::mem::take(&mut self.invalidate) {
            stamp.quality = Quality::Invalid;
            stamp.offset_uncertainty_ns = None;
        }
    }

    /// Reports `error` once, if the stream is still the owner's (not stopped or dropped).
    fn report(&mut self, error: StreamError) {
        if self.reported || matches!(self.shared.state(), STOPPED | DETACHED) {
            return;
        }
        self.reported = true;
        let report = &mut self.error;
        // The error callback may panic too; there is nobody left to tell.
        let _ = catch_unwind(AssertUnwindSafe(|| report(error)));
    }

    /// A panicking user callback: counted, reported as `Other`, and never called again.
    fn panicked(&mut self, payload: Box<dyn std::any::Any + Send>) {
        self.faulted = true;
        self.shared.stats.fault();
        // A payload whose own drop panics must not unwind into the driver either.
        let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
        self.report(StreamError::Other);
    }
}

/// The buffer-switch state of one driver session.
pub(crate) struct Engine {
    frames: usize,
    rate: u32,
    period_100ns: u64,
    pub(crate) render: Option<Side<RenderCallback>>,
    pub(crate) capture: Option<Side<CaptureCallback>>,
    /// `(sample position, QPC in 100 ns)` of the first callback with a valid position.
    anchor: Option<(u64, u64)>,
    last_arrival: Option<u64>,
    last_position: Option<u64>,
    last_system_time: Option<u64>,
    events: Arc<DriverEvents>,
    latencies: Arc<Latencies>,
    system_step: Arc<AtomicU64>,
}

impl Engine {
    pub(crate) fn new(frames: usize, rate: u32, events: Arc<DriverEvents>, latencies: Arc<Latencies>) -> Self {
        let rate = rate.max(1);
        Self {
            frames,
            rate,
            period_100ns: frames as u64 * 10_000_000 / u64::from(rate),
            render: None,
            capture: None,
            anchor: None,
            last_arrival: None,
            last_position: None,
            last_system_time: None,
            events,
            latencies,
            system_step: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    /// The smallest non-zero step between consecutive callbacks' `systemTime`, in ns (`u64::MAX`
    /// before two valid readings): an upper bound on the clock's grain, not a measure of it.
    pub(crate) fn system_time_step_ns(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.system_step)
    }

    /// A resync request: forgets the anchor and the history like [`Engine::restart`], and the next
    /// stamp of each direction is invalid, so the clock fit starts over at the new anchor.
    pub(crate) fn resync(&mut self) {
        self.restart();
        self.invalidate_next();
    }

    /// Marks the next stamp of each started direction invalid (one not started yet begins a fit
    /// of its own anyway).
    fn invalidate_next(&mut self) {
        for side in [self.render.as_mut().map(|s| (&s.shared, &mut s.invalidate)), self.capture.as_mut().map(|s| (&s.shared, &mut s.invalidate))]
            .into_iter()
            .flatten()
        {
            if side.0.state() == STARTED {
                *side.1 = true;
            }
        }
    }

    /// Forgets the anchor and the stamp history: the driver (re)started.
    pub(crate) fn restart(&mut self) {
        self.anchor = None;
        self.last_arrival = None;
        self.last_position = None;
        self.last_system_time = None;
        if let Some(side) = self.render.as_mut() {
            side.held.reset();
        }
        if let Some(side) = self.capture.as_mut() {
            side.held.reset();
        }
    }

    fn ns_of_frames(&self, frames: u64) -> i64 {
        i64::try_from(u128::from(frames) * 1_000_000_000 / u128::from(self.rate)).unwrap_or(i64::MAX)
    }

    /// The backend callback time of a block at `position` in the host origin `(origin_qpc,
    /// origin_ns)`, or `None` without a valid position and anchor.
    fn position_ns(&self, position: Option<u64>, origin_qpc: u64, origin_ns: i64) -> Option<i64> {
        let (p0, q0) = self.anchor?;
        let since = i128::from(position?) - i128::from(p0);
        let since_ns = since * 1_000_000_000 / i128::from(self.rate);
        i64::try_from(i128::from(relative_ns(q0, origin_qpc, origin_ns)) + since_ns).ok()
    }

    /// One buffer switch: counters, then the capture callback (its block was just recorded), then
    /// the render callback (or silence). `arrival_100ns` is the QPC read at callback entry.
    pub(crate) fn switch(&mut self, time: Time, arrival_100ns: u64, outputs: &mut dyn OutputPlanes, inputs: &dyn InputPlanes) {
        let late = self.last_arrival.map(|previous| arrival_100ns.saturating_sub(previous.saturating_add(self.period_100ns)));
        self.last_arrival = Some(arrival_100ns);
        if let (Some(now), Some(before)) = (time.position, self.last_position) {
            if now != before.saturating_add(self.frames as u64) {
                self.events.position_jumps.fetch_add(1, Ordering::Relaxed);
                // A skipped or repeated block: the stamps around it do not continue each other.
                self.invalidate_next();
            }
            if now < before {
                // The counter went back: the old anchor no longer maps it.
                self.anchor = None;
                for held in [self.render.as_mut().map(|s| &mut s.held), self.capture.as_mut().map(|s| &mut s.held)].into_iter().flatten() {
                    held.reset();
                }
            }
        }
        self.last_position = time.position;
        if let (Some(position), None) = (time.position, self.anchor) {
            self.anchor = Some((position, arrival_100ns));
        }
        if let Some(now) = time.system_time_ns {
            if let Some(step) = self.last_system_time.map(|before| now.saturating_sub(before)).filter(|step| *step > 0) {
                self.system_step.fetch_min(step, Ordering::Relaxed);
            }
            self.last_system_time = Some(now);
        }
        let latency_in = self.ns_of_frames(u64::from(self.latencies.input.load(Ordering::Relaxed)));
        let latency_out = self.ns_of_frames(u64::from(self.latencies.output.load(Ordering::Relaxed)));
        let frames = self.frames;
        // Each side's stamp, computed before borrowing the side mutably. `enter` marks the side in
        // flight before it reads whether the side runs; `leave` below ends that once its callback returned.
        let capture_stamp = self.capture.as_ref().filter(|s| s.enter()).map(|side| {
            let (origin_qpc, origin_ns) = side.shared.origin();
            let backend = self.position_ns(time.position, origin_qpc, origin_ns);
            (origin_qpc, origin_ns, backend)
        });
        let render_stamp = self.render.as_ref().filter(|s| s.enter()).map(|side| {
            let (origin_qpc, origin_ns) = side.shared.origin();
            let backend = self.position_ns(time.position, origin_qpc, origin_ns);
            (origin_qpc, origin_ns, backend)
        });
        if let (Some(side), Some((origin_qpc, origin_ns, backend))) = (self.capture.as_mut(), capture_stamp) {
            side.shared.stats.wake();
            if let Some(late) = late {
                side.shared.stats.wake_late(late);
            }
            let round_trip = side.calibration.map_or(0, |c| i64::try_from(c.round_trip_ns).unwrap_or(i64::MAX));
            let mut stamp = stamp(&mut side.held, arrival_100ns, origin_qpc, origin_ns, backend, side.calibration, true, |callback| {
                callback.saturating_sub(latency_in).saturating_sub(round_trip)
            });
            side.invalidated(&mut stamp);
            let mut source = PlanarSource { scratch: &mut side.scratch, planes: inputs, frames, wire: side.wire };
            let mut block = CaptureBlock::new(frames, stamp, side.format, &mut source);
            block.device_position = time.position;
            let callback = &mut side.callback;
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| callback(&mut block))) {
                side.panicked(payload);
            }
        }
        match (self.render.as_mut(), render_stamp) {
            // Checked again: the capture callback of this switch may have stopped the render
            // stream (a stop from inside the callback, `SideShared::request_stop`).
            (Some(side), Some((origin_qpc, origin_ns, backend))) if side.shared.state() == STARTED => {
                side.shared.stats.wake();
                if let Some(late) = late {
                    side.shared.stats.wake_late(late);
                }
                let mut stamp = stamp(&mut side.held, arrival_100ns, origin_qpc, origin_ns, backend, side.calibration, false, |callback| {
                    callback.saturating_add(latency_out)
                });
                side.invalidated(&mut stamp);
                let mut sink = PlanarSink { scratch: &mut side.scratch, planes: &mut *outputs, frames, wire: side.wire };
                let mut block = RenderBlock::new(frames, stamp, side.format, &mut sink);
                block.device_position = time.position;
                let callback = &mut side.callback;
                if let Err(payload) = catch_unwind(AssertUnwindSafe(|| callback(&mut block))) {
                    side.panicked(payload);
                    silence(outputs);
                }
            }
            _ => silence(outputs),
        }
        for shared in [self.capture.as_ref().map(|s| &s.shared), self.render.as_ref().map(|s| &s.shared)].into_iter().flatten() {
            shared.leave();
        }
    }

    /// The end of a buffer switch's work (after `outputReady`): the service time of each live side.
    pub(crate) fn finish(&mut self, arrival_100ns: u64, end_100ns: u64) {
        let elapsed = end_100ns.saturating_sub(arrival_100ns);
        if let Some(side) = self.render.as_ref().filter(|s| s.live()) {
            side.shared.stats.service(elapsed);
        }
        if let Some(side) = self.capture.as_ref().filter(|s| s.live()) {
            side.shared.stats.service(elapsed);
        }
    }

    /// Reports `error` once to each direction the owner still holds.
    pub(crate) fn report(&mut self, error: StreamError) {
        if let Some(side) = self.capture.as_mut() {
            side.report(error);
        }
        if let Some(side) = self.render.as_mut() {
            side.report(error);
        }
    }
}

/// The stamp of one block: the position time (or the arrival) as the backend callback time, the
/// frame time from it through `frame_of`, the arrival as host time; never backward.
fn stamp(
    held: &mut Held,
    arrival_100ns: u64,
    origin_qpc: u64,
    origin_ns: i64,
    backend: Option<i64>,
    calibration: Option<Calibration>,
    capture: bool,
    frame_of: impl FnOnce(i64) -> i64,
) -> Stamp {
    let arrival = relative_ns(arrival_100ns, origin_qpc, origin_ns);
    let Ok(host) = u64::try_from(arrival) else { return Stamp::INVALID };
    let (callback, (quality, offset_uncertainty_ns)) = match backend {
        Some(callback) => (callback, quality_of(calibration)),
        None => (arrival, (Quality::Arrival, None)),
    };
    let frame = frame_of(callback);
    let (callback, frame, host) = held.hold(callback, frame, host, capture);
    Stamp { host_callback_ns: host, backend_callback_ns: callback, backend_frame_ns: frame, quality, offset_uncertainty_ns }
}
