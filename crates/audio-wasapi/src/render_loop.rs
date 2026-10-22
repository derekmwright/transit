//! The render callback loop, over a small trait of the client calls so it runs on a fake in
//! tests (ADR 0016, section 4: "logic is tested behind a `ComApi` trait"). Everything here runs
//! on the stream thread after `Start`: nothing allocates, locks or formats text, and every
//! `HRESULT` is translated to the `Copy` [`Fault`] from its raw code.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use crate::convert::{ConvertSink, Wire};
use std::panic::{AssertUnwindSafe, catch_unwind};
use transit_audio_backend::{ErrorCallback, Format, HostOrigin, Quality, RenderBlock, RenderCallback, Stamp, StreamError};

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
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stamper {
    /// QPC, in 100 ns units, at the capture of the pair.
    origin_qpc: u64,
    /// The host-origin nanoseconds at the same instant (negative if before the origin).
    origin_ns: i64,
    rate: u32,
    /// `GetStreamLatency`, in nanoseconds.
    latency_ns: i64,
}

impl Stamper {
    pub(crate) fn new(origin_qpc_100ns: u64, origin_ns: i64, rate: u32, latency_100ns: i64) -> Self {
        Self { origin_qpc: origin_qpc_100ns, origin_ns, rate: rate.max(1), latency_ns: latency_100ns.saturating_mul(100) }
    }

    /// The stamp of a block: the callback instant is the position read, and the first frame
    /// written is presented one padding plus one stream latency later (ADR 0016, section 3).
    pub(crate) fn stamp(&self, position: Option<Position>, padding: u32, host_ns: Option<u64>) -> Stamp {
        let (Some(position), Some(host_ns)) = (position, host_ns) else { return Stamp::INVALID };
        let since = (i128::from(position.qpc_100ns) - i128::from(self.origin_qpc)) * 100;
        let callback = i64::try_from(i128::from(self.origin_ns) + since).unwrap_or(i64::MAX);
        let queued = i64::try_from(u128::from(padding) * 1_000_000_000 / u128::from(self.rate)).unwrap_or(i64::MAX);
        Stamp {
            host_callback_ns: host_ns,
            backend_callback_ns: callback,
            backend_frame_ns: callback.saturating_add(queued).saturating_add(self.latency_ns),
            quality: Quality::BackendEstimate,
            offset_uncertainty_ns: None,
        }
    }
}

/// Everything the loop needs that is fixed before `Start`.
pub(crate) struct LoopConfig {
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

/// Serve the device until it stops or fails. A fault or timeout is reported once through
/// `error` before this returns. `scratch` is the preallocated `f32` chunk buffer.
pub(crate) fn run(
    device: &mut impl RenderDevice,
    config: &LoopConfig,
    scratch: &mut [f32],
    render: &mut RenderCallback,
    error: &mut ErrorCallback,
) -> Exit {
    loop {
        match device.wait(config.timeout_ms) {
            Wake::Event => {}
            Wake::Stop => return Exit::Stopped,
            Wake::Timeout => {
                error(StreamError::DeviceNotAvailable);
                return Exit::Timeout;
            }
            Wake::Failed(fault) => return fail(error, fault),
        }
        let padding = match device.padding() {
            Ok(padding) => padding,
            Err(fault) => return fail(error, fault),
        };
        let frames = config.buffer_frames.saturating_sub(padding);
        if frames == 0 {
            continue;
        }
        // A clock that cannot be read publishes an invalid stamp and the block still renders.
        let position = device.position().ok();
        let stamp = config.stamper.stamp(position, padding, config.origin.now_ns());
        let written = device.write(frames, &mut |bytes| {
            let mut sink = ConvertSink { scratch: &mut *scratch, bytes, wire: config.wire };
            let mut block = RenderBlock::new(frames as usize, stamp, config.format, &mut sink);
            block.device_position = position.map(|p| p.frames);
            render(&mut block);
        });
        if let Err(fault) = written {
            return fail(error, fault);
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
) -> Exit {
    let exit = match catch_unwind(AssertUnwindSafe(|| run(&mut *device, config, scratch, render, error))) {
        Ok(exit) => exit,
        Err(payload) => {
            // The error callback may panic too; there is nobody left to tell.
            let _ = catch_unwind(AssertUnwindSafe(|| error(StreamError::Other)));
            // A payload whose own drop panics must not abort the teardown either.
            let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
            Exit::Panicked
        }
    };
    device.halt();
    exit
}

fn fail(error: &mut ErrorCallback, fault: Fault) -> Exit {
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
        buffer: Vec<u8>,
        writes: u32,
        halts: u32,
        /// `GetBuffer` calls and `ReleaseBuffer` calls, as the real client counts them.
        gets: u32,
        releases: std::cell::Cell<u32>,
        /// A second `GetBuffer` while one is outstanding.
        double_get: bool,
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
                buffer: vec![0; 4 * 2 * 4 * PERIOD as usize],
                writes: 0,
                halts: 0,
                gets: 0,
                releases: std::cell::Cell::new(0),
                double_get: false,
            }
        }
    }

    impl RenderDevice for Fake {
        fn wait(&mut self, _timeout_ms: u32) -> Wake {
            self.wakes.pop_front().unwrap_or(Wake::Stop)
        }
        fn padding(&mut self) -> Result<u32, Fault> {
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
        let seen = Arc::new(Mutex::new(Vec::with_capacity(64)));
        let errors = Arc::new(Mutex::new(Vec::with_capacity(4)));
        let (log, faults) = (Arc::clone(&seen), Arc::clone(&errors));
        let mut render: RenderCallback = Box::new(move |block| {
            log.lock().unwrap().push(Seen { frames: block.frames, stamp: block.stamp, position: block.device_position });
            block.render(&mut |chunk| chunk.fill(0.25));
        });
        let mut error: ErrorCallback = Box::new(move |e| faults.lock().unwrap().push(e));
        let mut scratch = vec![0.0f32; 64];
        let exit = run(device, config, &mut scratch, &mut render, &mut error);
        let (seen, errors) = (seen.lock().unwrap().clone(), errors.lock().unwrap().clone());
        (exit, seen, errors)
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

    #[test]
    fn the_loop_body_allocates_nothing() {
        let mut device = Fake::new([Wake::Event; 200]);
        let config = config(0, 10_000);
        let mut render: RenderCallback = Box::new(|block| block.render(&mut |chunk| chunk.fill(0.1)));
        let mut error: ErrorCallback = Box::new(|_| {});
        let mut scratch = vec![0.0f32; 64];
        let (exit, stats) = measure(|| run(&mut device, &config, &mut scratch, &mut render, &mut error));
        assert_eq!(exit, Exit::Stopped);
        assert!(stats.is_zero(), "the loop allocated: {stats:?}");
        assert_eq!(device.writes, 200);
    }

    fn serve_with(device: &mut Fake, mut render: RenderCallback) -> (Exit, Vec<StreamError>) {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let faults = Arc::clone(&errors);
        let mut error: ErrorCallback = Box::new(move |e| faults.lock().unwrap().push(e));
        let mut scratch = vec![0.0f32; 64];
        let exit = serve(device, &config(0, 0), &mut scratch, &mut render, &mut error);
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
        let exit = serve(&mut device, &config(0, 0), &mut scratch, &mut { render }, &mut error);
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
