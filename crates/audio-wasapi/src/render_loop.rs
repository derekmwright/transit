//! The render callback loop, over a small trait of the client calls so it runs on a fake in
//! tests (ADR 0016, section 4: "logic is tested behind a `ComApi` trait"). Everything here runs
//! on the stream thread after `Start`: nothing allocates, locks or formats text, and every
//! `HRESULT` is translated to the `Copy` [`Fault`] from its raw code.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use crate::convert::{ConvertSink, Wire};
use std::panic::{AssertUnwindSafe, catch_unwind};
use transit_audio_backend::{Calibration, ErrorCallback, Format, HostOrigin, Quality, RenderBlock, RenderCallback, Stamp, StreamError};

/// `AUDCLNT_E_DEVICE_INVALIDATED`: the endpoint was removed, disabled or its format changed.
const E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
/// `AUDCLNT_E_RESOURCES_INVALIDATED`: the hardware resources of the stream were taken away.
const E_RESOURCES_INVALIDATED: i32 = 0x8889_0026_u32 as i32;
/// `AUDCLNT_E_SERVICE_NOT_RUNNING`: the audio service stopped.
const E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010_u32 as i32;
/// `AUDCLNT_E_DEVICE_IN_USE`: another client holds the device exclusively.
pub(crate) const E_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;

/// What a failed client call means, from the raw `HRESULT` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The device is gone or must be reopened: reported as [`StreamError::DeviceNotAvailable`].
    DeviceGone,
    /// Any other failure; the code is kept for the control thread to format.
    Other(i32),
}

impl Fault {
    pub(crate) const fn from_hresult(code: i32) -> Self {
        match code {
            E_DEVICE_INVALIDATED | E_RESOURCES_INVALIDATED | E_SERVICE_NOT_RUNNING => Self::DeviceGone,
            other => Self::Other(other),
        }
    }

    const fn stream_error(self) -> StreamError {
        match self {
            Self::DeviceGone => StreamError::DeviceNotAvailable,
            Self::Other(_) => StreamError::Other,
        }
    }
}

/// A device position read: frames played and the QPC time of the read, in 100 ns units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) frames: u64,
    pub(crate) qpc_100ns: u64,
}

/// Why a wait returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The device wants data.
    Event,
    /// The control thread asked the stream to stop.
    Stop,
    Timeout,
    Failed(Fault),
}

/// The calls the loop makes on a started render client.
pub(crate) trait RenderDevice {
    /// Wait for the buffer event or the stop event, for at most `timeout_ms`.
    fn wait(&mut self, timeout_ms: u32) -> Wake;
    /// Frames queued and not yet played (`GetCurrentPadding`).
    fn padding(&mut self) -> Result<u32, Fault>;
    /// The device position and its QPC time (`IAudioClock2::GetDevicePosition`).
    fn position(&mut self) -> Result<Position, Fault>;
    /// Lend the `frames` of the device buffer to `fill` (`GetBuffer`, then `ReleaseBuffer`).
    fn write(&mut self, frames: u32, fill: &mut dyn FnMut(&mut [u8])) -> Result<(), Fault>;
    /// Stop and reset the client. Idempotent; called once the loop has ended, however it ended.
    fn halt(&mut self);
}

/// Runs a closure when dropped unless disarmed: the unwind half of the `GetBuffer` /
/// `ReleaseBuffer` pairing, so a panic in the render callback still releases the buffer.
pub(crate) struct UnwindGuard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> UnwindGuard<F> {
    pub(crate) fn new(on_unwind: F) -> Self {
        Self(Some(on_unwind))
    }

    /// The normal path released the buffer itself: do nothing on drop.
    pub(crate) fn disarm(mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for UnwindGuard<F> {
    fn drop(&mut self) {
        if let Some(on_unwind) = self.0.take() {
            on_unwind();
        }
    }
}

/// Maps device positions to stamps: signed nanoseconds in the Player's host origin through the
/// `(QPC, Instant)` pair captured when the stream opened, like the cpal adapter's `Stamper`.
///
/// Stamps never step backward: a position or padding read that jitters below the previous one is
/// held at it (the clock fit rejects a backward step of a `Measured` stamp as a discontinuity), and
/// host time strictly increases.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stamper {
    /// QPC, in 100 ns units, at the capture of the pair.
    origin_qpc: u64,
    /// The host-origin nanoseconds at the same instant (negative if before the origin).
    origin_ns: i64,
    rate: u32,
    /// `GetStreamLatency`, in nanoseconds.
    latency_ns: i64,
    calibration: Option<Calibration>,
    last: Held,
}

/// The previous stamp's times (callback, frame, host), so the next never goes backward.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Held(Option<(i64, i64, u64)>);

impl Held {
    /// Clamp `callback`, `frame` and `host` to be no earlier than the previous stamp's. With
    /// `frame_before_callback` (an input) the frame time is also held at or before the callback.
    pub(crate) fn hold(&mut self, callback: i64, frame: i64, host: u64, frame_before_callback: bool) -> (i64, i64, u64) {
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

    /// Forget the history: the next stamp follows a gap or a fit reset.
    pub(crate) fn reset(&mut self) {
        self.0 = None;
    }
}

/// The quality a stream's stamps carry: `Measured` with its bound under a calibration.
pub(crate) fn quality_of(calibration: Option<Calibration>) -> (Quality, Option<u64>) {
    match calibration {
        Some(c) => (Quality::Measured, Some(c.uncertainty_ns)),
        None => (Quality::BackendEstimate, None),
    }
}

/// A QPC reading in 100 ns units as signed nanoseconds in the host origin.
pub(crate) fn relative_ns(qpc_100ns: u64, origin_qpc: u64, origin_ns: i64) -> i64 {
    let since = (i128::from(qpc_100ns) - i128::from(origin_qpc)) * 100;
    i64::try_from(i128::from(origin_ns) + since).unwrap_or(i64::MAX)
}

impl Stamper {
    pub(crate) fn new(origin_qpc_100ns: u64, origin_ns: i64, rate: u32, latency_100ns: i64) -> Self {
        Self {
            origin_qpc: origin_qpc_100ns,
            origin_ns,
            rate: rate.max(1),
            latency_ns: latency_100ns.saturating_mul(100),
            calibration: None,
            last: Held::default(),
        }
    }

    /// Stamp as `Measured` under `calibration`. The round trip is carried by the input stream, so
    /// the output keeps its presentation time and only claims the bound.
    pub(crate) fn calibrated(mut self, calibration: Option<Calibration>) -> Self {
        self.calibration = calibration;
        self
    }

    /// The stamp of a block: the callback instant is the position read, and the first frame
    /// written is presented one padding plus one stream latency later (ADR 0016, section 3).
    pub(crate) fn stamp(&mut self, position: Option<Position>, padding: u32, host_ns: Option<u64>) -> Stamp {
        let (Some(position), Some(host_ns)) = (position, host_ns) else { return Stamp::INVALID };
        let callback = relative_ns(position.qpc_100ns, self.origin_qpc, self.origin_ns);
        let queued = i64::try_from(u128::from(padding) * 1_000_000_000 / u128::from(self.rate)).unwrap_or(i64::MAX);
        let frame = callback.saturating_add(queued).saturating_add(self.latency_ns);
        let (callback, frame, host) = self.last.hold(callback, frame, host_ns, false);
        let (quality, offset_uncertainty_ns) = quality_of(self.calibration);
        Stamp { host_callback_ns: host, backend_callback_ns: callback, backend_frame_ns: frame, quality, offset_uncertainty_ns }
    }
}

/// How much of the buffer each wake fills, fixed by the mode at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fill {
    /// Shared mode: the frames the engine has consumed, `buffer_frames - GetCurrentPadding`.
    Free,
    /// Exclusive event-driven mode: the buffer is one period and the driver ping-pongs two of
    /// them, so every wake fills the whole buffer, whatever `GetCurrentPadding` says (drivers
    /// report it inconsistently in this mode, and a full-looking buffer would starve the device).
    /// The written frames are presented after the period the device is playing now, so the stamp
    /// counts one buffer as queued.
    Whole,
}

/// Everything the loop needs that is fixed before `Start`.
pub(crate) struct LoopConfig {
    pub(crate) fill: Fill,
    pub(crate) buffer_frames: u32,
    pub(crate) format: Format,
    pub(crate) wire: Wire,
    pub(crate) stamper: Stamper,
    pub(crate) origin: HostOrigin,
    pub(crate) timeout_ms: u32,
}

/// How the loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Stopped,
    Timeout,
    Faulted(Fault),
    /// The render callback (or the loop) panicked; reported through `error` as `Other`.
    Panicked,
}

/// `AUDCLNT_E_BUFFER_TOO_LARGE`: `GetBuffer` asked for more frames than are free. In exclusive
/// mode a driver can signal the event before the buffer it ping-pongs is free; that wake is skipped.
pub(crate) const E_BUFFER_TOO_LARGE: i32 = 0x8889_0006_u32 as i32;

/// What the render loop saw, for the log and the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderStats {
    /// Buffer events (wakes that were not a stop, timeout or failure).
    pub(crate) wakes: u64,
    /// Exclusive-mode wakes whose buffer was not free yet (`AUDCLNT_E_BUFFER_TOO_LARGE`), skipped.
    pub(crate) skipped_wakes: u64,
    pub(crate) timeouts: u64,
}

/// Serve the device until it stops or fails. A fault or timeout is reported once through
/// `error` before this returns. `scratch` is the preallocated `f32` chunk buffer.
pub(crate) fn run(
    device: &mut impl RenderDevice,
    config: &LoopConfig,
    scratch: &mut [f32],
    render: &mut RenderCallback,
    error: &mut ErrorCallback,
    stats: &mut RenderStats,
) -> Exit {
    let mut stamper = config.stamper;
    loop {
        match device.wait(config.timeout_ms) {
            Wake::Event => stats.wakes += 1,
            Wake::Stop => return Exit::Stopped,
            Wake::Timeout => {
                stats.timeouts += 1;
                error(StreamError::DeviceNotAvailable);
                return Exit::Timeout;
            }
            Wake::Failed(fault) => return fail(error, fault),
        }
        let (padding, frames) = match config.fill {
            Fill::Free => match device.padding() {
                Ok(padding) => (padding, config.buffer_frames.saturating_sub(padding)),
                Err(fault) => return fail(error, fault),
            },
            Fill::Whole => (config.buffer_frames, config.buffer_frames),
        };
        if frames == 0 {
            continue;
        }
        // A clock that cannot be read publishes an invalid stamp and the block still renders.
        let position = device.position().ok();
        let stamp = stamper.stamp(position, padding, config.origin.now_ns());
        let written = device.write(frames, &mut |bytes| {
            let mut sink = ConvertSink { scratch: &mut *scratch, bytes, wire: config.wire };
            let mut block = RenderBlock::new(frames as usize, stamp, config.format, &mut sink);
            block.device_position = position.map(|p| p.frames);
            render(&mut block);
        });
        match written {
            Ok(()) => {}
            // The early event of an exclusive driver: `GetBuffer` acquired nothing and the
            // callback did not run; the next event brings the free buffer.
            Err(Fault::Other(E_BUFFER_TOO_LARGE)) if config.fill == Fill::Whole => stats.skipped_wakes += 1,
            Err(fault) => return fail(error, fault),
        }
    }
}

/// [`run`] under `catch_unwind`, then the device teardown. A panic is reported once through
/// `error` as [`StreamError::Other`] and the caller continues into its normal teardown, so the
/// Player is told and the client is stopped, reset and released instead of dying silently.
pub(crate) fn serve(
    device: &mut impl RenderDevice,
    config: &LoopConfig,
    scratch: &mut [f32],
    render: &mut RenderCallback,
    error: &mut ErrorCallback,
    stats: &mut RenderStats,
) -> Exit {
    let exit = contain(error, |error| run(&mut *device, config, scratch, render, error, stats));
    device.halt();
    exit
}

/// Run `work` under `catch_unwind`: a panic is reported once through `error` as
/// [`StreamError::Other`] and becomes [`Exit::Panicked`], so the caller still tears the stream down.
pub(crate) fn contain(error: &mut ErrorCallback, work: impl FnOnce(&mut ErrorCallback) -> Exit) -> Exit {
    match catch_unwind(AssertUnwindSafe(|| work(&mut *error))) {
        Ok(exit) => exit,
        Err(payload) => {
            // The error callback may panic too; there is nobody left to tell.
            let _ = catch_unwind(AssertUnwindSafe(|| error(StreamError::Other)));
            // A payload whose own drop panics must not abort the teardown either.
            let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
            Exit::Panicked
        }
    }
}

pub(crate) fn fail(error: &mut ErrorCallback, fault: Fault) -> Exit {
    error(fault.stream_error());
    Exit::Faulted(fault)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use transit_audio_backend::SampleFormat;
    use transit_rt::testing::{CountingAlloc, measure};

    #[global_allocator]
    static ALLOC: CountingAlloc = CountingAlloc;

    const RATE: u32 = 48_000;
    const PERIOD: u32 = 128;
    /// One period in 100 ns units (2.666 ms).
    const PERIOD_100NS: u64 = PERIOD as u64 * 10_000_000 / RATE as u64;

    struct Fake {
        wakes: VecDeque<Wake>,
        padding: u32,
        position: u64,
        origin_qpc: u64,
        clock_ok: bool,
        fail_write: Option<Fault>,
        /// `GetBuffer` fails once with this fault on the write attempt of this index (0-based).
        fail_once: Option<(u32, Fault)>,
        attempts: u32,
        buffer: Vec<u8>,
        writes: u32,
        halts: u32,
        /// `GetBuffer` calls and `ReleaseBuffer` calls, as the real client counts them.
        gets: u32,
        releases: std::cell::Cell<u32>,
        /// A second `GetBuffer` while one is outstanding.
        double_get: bool,
        padding_reads: u32,
    }

    impl Fake {
        fn new(wakes: impl IntoIterator<Item = Wake>) -> Self {
            Fake {
                wakes: wakes.into_iter().collect(),
                padding: PERIOD,
                position: 0,
                origin_qpc: 5_000_000,
                clock_ok: true,
                fail_write: None,
                fail_once: None,
                attempts: 0,
                buffer: vec![0; 4 * 2 * 4 * PERIOD as usize],
                writes: 0,
                halts: 0,
                gets: 0,
                releases: std::cell::Cell::new(0),
                double_get: false,
                padding_reads: 0,
            }
        }
    }

    impl RenderDevice for Fake {
        fn wait(&mut self, _timeout_ms: u32) -> Wake {
            self.wakes.pop_front().unwrap_or(Wake::Stop)
        }
        fn padding(&mut self) -> Result<u32, Fault> {
            self.padding_reads += 1;
            Ok(self.padding)
        }
        fn position(&mut self) -> Result<Position, Fault> {
            if !self.clock_ok {
                return Err(Fault::Other(-1));
            }
            let at = Position { frames: self.position, qpc_100ns: self.origin_qpc + self.position * 10_000_000 / u64::from(RATE) };
            self.position += u64::from(PERIOD);
            Ok(at)
        }
        fn write(&mut self, frames: u32, fill: &mut dyn FnMut(&mut [u8])) -> Result<(), Fault> {
            if let Some(fault) = self.fail_write {
                return Err(fault);
            }
            let attempt = self.attempts;
            self.attempts += 1;
            if let Some((at, fault)) = self.fail_once
                && at == attempt
            {
                return Err(fault);
            }
            self.writes += 1;
            if self.gets != self.releases.get() {
                self.double_get = true;
            }
            self.gets += 1;
            let gets = self.gets;
            // The same pairing as `Live::write`: release on unwind unless the normal path did.
            let releases = &self.releases;
            let guard = UnwindGuard::new(|| releases.set(gets));
            fill(&mut self.buffer[..frames as usize * 2 * 4]);
            guard.disarm();
            self.releases.set(gets);
            Ok(())
        }
        fn halt(&mut self) {
            self.halts += 1;
        }
    }

    fn config(origin_ns: i64, latency_100ns: i64) -> LoopConfig {
        LoopConfig {
            fill: Fill::Free,
            buffer_frames: 2 * PERIOD,
            format: Format::new(RATE, 2, SampleFormat::F32),
            wire: Wire::F32,
            stamper: Stamper::new(5_000_000, origin_ns, RATE, latency_100ns),
            origin: HostOrigin::default(),
            timeout_ms: 250,
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct Seen {
        frames: usize,
        stamp: Stamp,
        position: Option<u64>,
    }

    fn drive(device: &mut Fake, config: &LoopConfig) -> (Exit, Vec<Seen>, Vec<StreamError>) {
        let (exit, seen, errors, _) = drive_stats(device, config);
        (exit, seen, errors)
    }

    fn drive_stats(device: &mut Fake, config: &LoopConfig) -> (Exit, Vec<Seen>, Vec<StreamError>, RenderStats) {
        let seen = Arc::new(Mutex::new(Vec::with_capacity(64)));
        let errors = Arc::new(Mutex::new(Vec::with_capacity(4)));
        let (log, faults) = (Arc::clone(&seen), Arc::clone(&errors));
        let mut render: RenderCallback = Box::new(move |block| {
            log.lock().unwrap().push(Seen { frames: block.frames, stamp: block.stamp, position: block.device_position });
            block.render(&mut |chunk| chunk.fill(0.25));
        });
        let mut error: ErrorCallback = Box::new(move |e| faults.lock().unwrap().push(e));
        let mut scratch = vec![0.0f32; 64];
        let mut stats = RenderStats::default();
        let exit = run(device, config, &mut scratch, &mut render, &mut error, &mut stats);
        let (seen, errors) = (seen.lock().unwrap().clone(), errors.lock().unwrap().clone());
        (exit, seen, errors, stats)
    }

    #[test]
    fn every_event_renders_the_free_frames_and_stop_exits_cleanly() {
        let mut device = Fake::new([Wake::Event; 10]);
        let (exit, seen, errors) = drive(&mut device, &config(0, 0));
        assert_eq!((exit, seen.len(), errors.len()), (Exit::Stopped, 10, 0));
        assert!(seen.iter().all(|s| s.frames == PERIOD as usize), "buffer minus padding");
        // 0.25 as f32 little-endian: the conversion wrote the device format.
        assert_eq!(&device.buffer[..4], &0.25f32.to_le_bytes());
    }

    #[test]
    fn stamps_are_origin_relative_and_strictly_increasing_one_period_apart() {
        let origin_ns = -1_234_000; // the pair was captured before the host origin
        let mut device = Fake::new([Wake::Event; 8]);
        let (_, seen, _) = drive(&mut device, &config(origin_ns, 0));
        assert_eq!(seen[0].stamp.backend_callback_ns, origin_ns, "the first read is the origin instant");
        for pair in seen.windows(2) {
            let step = pair[1].stamp.backend_callback_ns - pair[0].stamp.backend_callback_ns;
            // The fake truncates QPC to 100 ns, so a step is a period within one tick.
            assert!(step.abs_diff(i64::try_from(PERIOD_100NS).unwrap() * 100) <= 100, "one period in ns: {step}");
            assert!(pair[1].stamp.backend_frame_ns > pair[0].stamp.backend_frame_ns);
        }
        assert!(seen.iter().all(|s| s.stamp.quality == Quality::BackendEstimate));
    }

    #[test]
    fn the_presentation_time_adds_padding_and_stream_latency() {
        let mut device = Fake::new([Wake::Event]);
        // 1 ms of stream latency (10_000 hns); padding is one period (2.666 ms).
        let (_, seen, _) = drive(&mut device, &config(0, 10_000));
        let want = 1_000_000 + i64::from(PERIOD) * 1_000_000_000 / i64::from(RATE);
        assert_eq!(seen[0].stamp.backend_frame_ns - seen[0].stamp.backend_callback_ns, want);
    }

    #[test]
    fn device_position_increases_by_the_frames_played() {
        let mut device = Fake::new([Wake::Event; 6]);
        let (_, seen, _) = drive(&mut device, &config(0, 0));
        let positions: Vec<u64> = seen.iter().map(|s| s.position.unwrap()).collect();
        assert!(positions.windows(2).all(|w| w[1] == w[0] + u64::from(PERIOD)), "{positions:?}");
    }

    #[test]
    fn an_unreadable_clock_publishes_invalid_and_keeps_rendering() {
        let mut device = Fake::new([Wake::Event; 3]);
        device.clock_ok = false;
        let (exit, seen, errors) = drive(&mut device, &config(0, 0));
        assert_eq!((exit, seen.len(), errors.len()), (Exit::Stopped, 3, 0));
        assert!(seen.iter().all(|s| s.stamp.quality == Quality::Invalid && s.position.is_none()));
    }

    #[test]
    fn device_invalidation_reports_device_not_available_and_exits() {
        let mut device = Fake::new([Wake::Event, Wake::Event, Wake::Event]);
        device.fail_write = Some(Fault::from_hresult(E_DEVICE_INVALIDATED));
        let (exit, seen, errors) = drive(&mut device, &config(0, 0));
        assert_eq!(exit, Exit::Faulted(Fault::DeviceGone));
        assert_eq!((seen.len(), errors), (0, vec![StreamError::DeviceNotAvailable]));
        assert_eq!(Fault::from_hresult(E_DEVICE_IN_USE), Fault::Other(E_DEVICE_IN_USE));
    }

    #[test]
    fn a_wait_timeout_reports_device_not_available_and_exits() {
        let mut device = Fake::new([Wake::Event, Wake::Timeout, Wake::Event]);
        let (exit, seen, errors) = drive(&mut device, &config(0, 0));
        assert_eq!((exit, seen.len(), errors), (Exit::Timeout, 1, vec![StreamError::DeviceNotAvailable]));
    }

    #[test]
    fn a_failed_wait_or_other_fault_reports_other() {
        let mut device = Fake::new([Wake::Failed(Fault::Other(7))]);
        let (exit, _, errors) = drive(&mut device, &config(0, 0));
        assert_eq!((exit, errors), (Exit::Faulted(Fault::Other(7)), vec![StreamError::Other]));
    }

    #[test]
    fn a_full_buffer_renders_nothing() {
        let mut device = Fake::new([Wake::Event; 3]);
        device.padding = 2 * PERIOD;
        let (_, seen, _) = drive(&mut device, &config(0, 0));
        assert!(seen.is_empty() && device.writes == 0);
    }

    /// An exclusive stream: the buffer is one period, filled whole on every wake.
    fn exclusive(latency_100ns: i64) -> LoopConfig {
        LoopConfig { fill: Fill::Whole, buffer_frames: PERIOD, ..config(0, latency_100ns) }
    }

    #[test]
    fn exclusive_fills_the_whole_buffer_every_wake_whatever_the_padding_says() {
        // A driver that reports an empty buffer, one that reports a full one, and one in between.
        for padding in [0, PERIOD, PERIOD / 2] {
            let mut device = Fake::new([Wake::Event; 6]);
            device.padding = padding;
            let (exit, seen, errors) = drive(&mut device, &exclusive(0));
            assert_eq!((exit, seen.len(), errors.len(), device.writes), (Exit::Stopped, 6, 0, 6), "padding {padding}");
            assert!(seen.iter().all(|s| s.frames == PERIOD as usize), "every wake writes buffer_frames (padding {padding})");
            assert_eq!(device.padding_reads, 0, "exclusive mode does not ask for the padding");
            assert_eq!((device.gets, device.releases.get()), (6, 6));
        }
    }

    #[test]
    fn exclusive_presents_after_the_period_being_played_plus_the_stream_latency() {
        let mut device = Fake::new([Wake::Event; 2]);
        let (_, seen, _) = drive(&mut device, &exclusive(10_000));
        let want = 1_000_000 + i64::from(PERIOD) * 1_000_000_000 / i64::from(RATE);
        assert!(seen.iter().all(|s| s.stamp.backend_frame_ns - s.stamp.backend_callback_ns == want));
    }

    #[test]
    fn shared_mode_still_reads_the_padding_once_per_wake() {
        let mut device = Fake::new([Wake::Event; 4]);
        let (_, seen, _) = drive(&mut device, &config(0, 0));
        assert_eq!((device.padding_reads, seen.len()), (4, 4));
    }

    #[test]
    fn an_exclusive_early_event_with_no_free_buffer_is_skipped_and_the_stream_continues() {
        let mut device = Fake::new([Wake::Event; 6]);
        device.fail_once = Some((2, Fault::from_hresult(E_BUFFER_TOO_LARGE)));
        let (exit, seen, errors, stats) = drive_stats(&mut device, &exclusive(0));
        assert_eq!((exit, errors.len()), (Exit::Stopped, 0), "the stream did not end");
        assert_eq!((seen.len(), device.writes), (5, 5), "the third wake rendered nothing, the rest rendered");
        assert_eq!(stats, RenderStats { wakes: 6, skipped_wakes: 1, timeouts: 0 });
        assert_eq!((device.gets, device.releases.get()), (5, 5), "nothing was acquired on the skipped wake");
    }

    #[test]
    fn too_large_stays_fatal_in_shared_mode_and_other_faults_stay_fatal_in_exclusive() {
        let mut shared = Fake::new([Wake::Event; 3]);
        shared.fail_once = Some((0, Fault::from_hresult(E_BUFFER_TOO_LARGE)));
        let (exit, _, errors, _) = drive_stats(&mut shared, &config(0, 0));
        assert_eq!((exit, errors), (Exit::Faulted(Fault::Other(E_BUFFER_TOO_LARGE)), vec![StreamError::Other]));
        let mut gone = Fake::new([Wake::Event; 3]);
        gone.fail_once = Some((1, Fault::from_hresult(E_DEVICE_INVALIDATED)));
        let (exit, seen, errors, stats) = drive_stats(&mut gone, &exclusive(0));
        assert_eq!((exit, seen.len(), errors), (Exit::Faulted(Fault::DeviceGone), 1, vec![StreamError::DeviceNotAvailable]));
        assert_eq!(stats.skipped_wakes, 0);
    }

    #[test]
    fn a_timeout_is_counted() {
        let mut device = Fake::new([Wake::Event, Wake::Timeout]);
        let (_, _, _, stats) = drive_stats(&mut device, &exclusive(0));
        assert_eq!(stats, RenderStats { wakes: 1, skipped_wakes: 0, timeouts: 1 });
    }

    #[test]
    fn the_exclusive_loop_body_allocates_nothing() {
        let mut device = Fake::new([Wake::Event; 200]);
        // One early event on the way: the skip path is allocation-free too.
        device.fail_once = Some((50, Fault::from_hresult(E_BUFFER_TOO_LARGE)));
        let config = exclusive(10_000);
        let mut render: RenderCallback = Box::new(|block| block.render(&mut |chunk| chunk.fill(0.1)));
        let mut error: ErrorCallback = Box::new(|_| {});
        let mut scratch = vec![0.0f32; 64];
        let mut render_stats = RenderStats::default();
        let (exit, stats) = measure(|| run(&mut device, &config, &mut scratch, &mut render, &mut error, &mut render_stats));
        assert_eq!(exit, Exit::Stopped);
        assert!(stats.is_zero(), "the loop allocated: {stats:?}");
        assert_eq!((device.writes, render_stats.skipped_wakes), (199, 1));
    }

    #[test]
    fn the_loop_body_allocates_nothing() {
        let mut device = Fake::new([Wake::Event; 200]);
        let config = config(0, 10_000);
        let mut render: RenderCallback = Box::new(|block| block.render(&mut |chunk| chunk.fill(0.1)));
        let mut error: ErrorCallback = Box::new(|_| {});
        let mut scratch = vec![0.0f32; 64];
        let mut render_stats = RenderStats::default();
        let (exit, stats) = measure(|| run(&mut device, &config, &mut scratch, &mut render, &mut error, &mut render_stats));
        assert_eq!(exit, Exit::Stopped);
        assert!(stats.is_zero(), "the loop allocated: {stats:?}");
        assert_eq!(device.writes, 200);
    }

    fn serve_with(device: &mut Fake, mut render: RenderCallback) -> (Exit, Vec<StreamError>) {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let faults = Arc::clone(&errors);
        let mut error: ErrorCallback = Box::new(move |e| faults.lock().unwrap().push(e));
        let mut scratch = vec![0.0f32; 64];
        let exit = serve(device, &config(0, 0), &mut scratch, &mut render, &mut error, &mut RenderStats::default());
        let errors = errors.lock().unwrap().clone();
        (exit, errors)
    }

    #[test]
    fn a_panicking_render_callback_reports_other_once_and_tears_down() {
        let mut device = Fake::new([Wake::Event; 5]);
        let calls = Arc::new(Mutex::new(0u32));
        let counted = Arc::clone(&calls);
        let render: RenderCallback = Box::new(move |_block| {
            *counted.lock().unwrap() += 1;
            panic!("render callback bug");
        });
        let (exit, errors) = serve_with(&mut device, render);
        assert_eq!(exit, Exit::Panicked);
        assert_eq!(errors, vec![StreamError::Other], "the error callback is called exactly once");
        assert_eq!(*calls.lock().unwrap(), 1, "the loop exited after the panic");
        assert_eq!(device.halts, 1, "Stop/Reset ran");
        assert_eq!((device.gets, device.releases.get()), (1, 1), "the buffer was released on unwind");
        assert!(!device.double_get, "no second GetBuffer without ReleaseBuffer");
    }

    #[test]
    fn a_panicking_error_callback_does_not_stop_the_teardown() {
        let mut device = Fake::new([Wake::Event; 2]);
        let render: RenderCallback = Box::new(|_| panic!("render"));
        let mut error: ErrorCallback = Box::new(|_| panic!("error"));
        let mut scratch = vec![0.0f32; 64];
        let exit = serve(&mut device, &config(0, 0), &mut scratch, &mut { render }, &mut error, &mut RenderStats::default());
        assert_eq!((exit, device.halts), (Exit::Panicked, 1));
    }

    #[test]
    fn serve_halts_after_a_normal_stop_and_a_fault() {
        let mut device = Fake::new([Wake::Event; 3]);
        let (exit, errors) = serve_with(&mut device, Box::new(|block| block.render(&mut |c| c.fill(0.0))));
        assert_eq!((exit, errors.len(), device.halts), (Exit::Stopped, 0, 1));
        assert_eq!((device.gets, device.releases.get()), (3, 3));
        let mut broken = Fake::new([Wake::Event]);
        broken.fail_write = Some(Fault::DeviceGone);
        let (exit, errors) = serve_with(&mut broken, Box::new(|_| {}));
        assert_eq!((exit, errors, broken.halts), (Exit::Faulted(Fault::DeviceGone), vec![StreamError::DeviceNotAvailable], 1));
    }
}
