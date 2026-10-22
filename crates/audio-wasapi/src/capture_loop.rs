//! The capture callback loop, over a small trait of the client calls so it runs on a fake in
//! tests, like `render_loop`. Everything here runs on the stream thread after `Start`: nothing
//! allocates, locks or formats text, and every `HRESULT` is translated to the `Copy` [`Fault`].
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use crate::convert::{ConvertSource, Wire};
use crate::render_loop::{Exit, Fault, Held, Position, Wake, contain, fail, quality_of, relative_ns};
use transit_audio_backend::{Calibration, CaptureBlock, CaptureCallback, ErrorCallback, Format, HostOrigin, Stamp, StreamError};

/// The most packets drained after one wake. The buffer holds a few periods, so this is a bound on
/// a device that keeps delivering, not a limit that real streams reach.
const MAX_PACKETS_PER_WAKE: u32 = 256;

/// One packet of the capture buffer, as `GetBuffer` described it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packet {
    pub(crate) frames: u32,
    /// `AUDCLNT_BUFFERFLAGS_SILENT`: the content is undefined and must be taken as zeros.
    pub(crate) silent: bool,
    /// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`: frames were lost before this packet.
    pub(crate) discontinuity: bool,
    /// `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR`: the device or QPC position of this packet is unreliable.
    pub(crate) timestamp_error: bool,
    /// The device position of the packet's first frame, in frames.
    pub(crate) device_position: u64,
    /// The QPC time of the packet's first frame, in 100 ns units.
    pub(crate) qpc_100ns: u64,
    /// The clock service's position read right after `GetBuffer` (the callback instant), or
    /// `None` when it could not be read.
    pub(crate) clock: Option<Position>,
}

/// The calls the loop makes on a started capture client.
pub(crate) trait CaptureDevice {
    /// Wait for the buffer event or the stop event, for at most `timeout_ms`.
    fn wait(&mut self, timeout_ms: u32) -> Wake;
    /// Lend the next packet to `fill` (`GetBuffer`, then `ReleaseBuffer` with its frames). The
    /// bytes are `None` for a silent packet. `Ok(false)` when no packet is waiting.
    fn read(&mut self, fill: &mut dyn FnMut(&Packet, Option<&[u8]>)) -> Result<bool, Fault>;
    /// Stop and reset the client. Idempotent; called once the loop has ended, however it ended.
    fn halt(&mut self);
}

/// Maps packets to stamps: signed nanoseconds in the Player's host origin through the
/// `(QPC, Instant)` pair captured when the stream started.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CaptureStamper {
    origin_qpc: u64,
    origin_ns: i64,
    calibration: Option<Calibration>,
    last: Held,
}

impl CaptureStamper {
    pub(crate) fn new(origin_qpc_100ns: u64, origin_ns: i64, calibration: Option<Calibration>) -> Self {
        Self { origin_qpc: origin_qpc_100ns, origin_ns, calibration, last: Held::default() }
    }

    /// The callback instant is the position read after `GetBuffer`; the frame time is the QPC of the
    /// packet's first frame, moved earlier by the calibrated round trip when there is one. A
    /// discontinuity, a timestamp error or an unreadable clock gives an invalid stamp (the clock fit
    /// restarts) and forgets the history.
    pub(crate) fn stamp(&mut self, packet: &Packet, host_ns: Option<u64>) -> Stamp {
        let (Some(clock), Some(host_ns)) = (packet.clock, host_ns) else { return self.invalid() };
        if packet.timestamp_error || packet.discontinuity || packet.frames == 0 {
            return self.invalid();
        }
        let callback = relative_ns(clock.qpc_100ns, self.origin_qpc, self.origin_ns);
        let round_trip = self.calibration.map_or(0, |c| i64::try_from(c.round_trip_ns).unwrap_or(i64::MAX));
        let frame = relative_ns(packet.qpc_100ns, self.origin_qpc, self.origin_ns).saturating_sub(round_trip);
        let (callback, frame, host) = self.last.hold(callback, frame, host_ns, true);
        let (quality, offset_uncertainty_ns) = quality_of(self.calibration);
        Stamp { host_callback_ns: host, backend_callback_ns: callback, backend_frame_ns: frame, quality, offset_uncertainty_ns }
    }

    fn invalid(&mut self) -> Stamp {
        self.last.reset();
        Stamp::INVALID
    }
}

/// Everything the loop needs that is fixed before `Start`.
pub(crate) struct CaptureConfig {
    pub(crate) format: Format,
    pub(crate) wire: Wire,
    pub(crate) stamper: CaptureStamper,
    pub(crate) origin: HostOrigin,
    pub(crate) timeout_ms: u32,
}

/// What the loop saw, for the log and the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureStats {
    pub(crate) packets: u64,
    pub(crate) frames: u64,
    pub(crate) silent: u64,
    pub(crate) discontinuities: u64,
    pub(crate) timestamp_errors: u64,
}

/// Serve the device until it stops or fails, draining every waiting packet after each wake. A fault
/// or timeout is reported once through `error` before this returns. `scratch` is the preallocated
/// `f32` chunk buffer, a whole number of frames.
pub(crate) fn run(
    device: &mut impl CaptureDevice,
    config: &CaptureConfig,
    scratch: &mut [f32],
    capture: &mut CaptureCallback,
    error: &mut ErrorCallback,
    stats: &mut CaptureStats,
) -> Exit {
    let mut stamper = config.stamper;
    let channels = usize::from(config.format.channels).max(1);
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
        for _ in 0..MAX_PACKETS_PER_WAKE {
            let read = device.read(&mut |packet, bytes| {
                stats.packets += 1;
                stats.frames += u64::from(packet.frames);
                stats.silent += u64::from(packet.silent);
                stats.discontinuities += u64::from(packet.discontinuity);
                stats.timestamp_errors += u64::from(packet.timestamp_error);
                let stamp = stamper.stamp(packet, config.origin.now_ns());
                let bytes = if packet.silent { None } else { bytes };
                let mut source = ConvertSource { scratch: &mut *scratch, channels, frames: packet.frames as usize, bytes, wire: config.wire };
                let mut block = CaptureBlock::new(packet.frames as usize, stamp, config.format, &mut source);
                block.device_position = Some(packet.device_position);
                capture(&mut block);
            });
            match read {
                Ok(true) => {}
                Ok(false) => break,
                Err(fault) => return fail(error, fault),
            }
        }
    }
}

/// [`run`] under `catch_unwind`, then the device teardown: a panic in the capture callback is
/// reported once and the client is stopped, reset and released.
pub(crate) fn serve(
    device: &mut impl CaptureDevice,
    config: &CaptureConfig,
    scratch: &mut [f32],
    capture: &mut CaptureCallback,
    error: &mut ErrorCallback,
    stats: &mut CaptureStats,
) -> Exit {
    let exit = contain(error, |error| run(&mut *device, config, scratch, capture, error, stats));
    device.halt();
    exit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_loop::UnwindGuard;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use transit_audio_backend::{Quality, SampleFormat};
    use transit_rt::testing::measure;

    const RATE: u32 = 48_000;
    const PERIOD: u32 = 128;
    const CHANNELS: usize = 2;
    const ORIGIN_QPC: u64 = 5_000_000;

    /// What one scripted wake delivers.
    enum Step {
        /// A wake with these packets waiting behind it.
        Wake(Vec<ScriptedPacket>),
        Wait(Wake),
        /// A wake with this many ordinary packets behind it (no allocation in the fake).
        Normal(usize),
    }

    #[derive(Clone, Copy)]
    struct ScriptedPacket {
        silent: bool,
        discontinuity: bool,
        timestamp_error: bool,
        frames: u32,
    }

    fn normal() -> ScriptedPacket {
        ScriptedPacket { silent: false, discontinuity: false, timestamp_error: false, frames: PERIOD }
    }

    struct Fake {
        steps: VecDeque<Step>,
        waiting: VecDeque<ScriptedPacket>,
        pending_normal: usize,
        /// Device frames delivered so far (the position of the next packet).
        position: u64,
        fail_read: Option<Fault>,
        clock_ok: bool,
        bytes: Vec<u8>,
        halts: u32,
        gets: u32,
        releases: std::cell::Cell<u32>,
    }

    impl Fake {
        fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Fake {
                steps: steps.into_iter().collect(),
                waiting: VecDeque::new(),
                pending_normal: 0,
                position: 0,
                fail_read: None,
                clock_ok: true,
                bytes: vec![0; 4 * CHANNELS * 4 * PERIOD as usize],
                halts: 0,
                gets: 0,
                releases: std::cell::Cell::new(0),
            }
        }
        fn wakes(count: usize) -> Self {
            Self::new((0..count).map(|_| Step::Wake(vec![normal()])))
        }
        fn qpc_of(&self, frames: u64) -> u64 {
            ORIGIN_QPC + frames * 10_000_000 / u64::from(RATE)
        }
    }

    impl CaptureDevice for Fake {
        fn wait(&mut self, _timeout_ms: u32) -> Wake {
            match self.steps.pop_front() {
                Some(Step::Wake(packets)) => {
                    self.waiting = packets.into();
                    Wake::Event
                }
                Some(Step::Wait(wake)) => wake,
                Some(Step::Normal(count)) => {
                    self.pending_normal = count;
                    Wake::Event
                }
                None => Wake::Stop,
            }
        }
        fn read(&mut self, fill: &mut dyn FnMut(&Packet, Option<&[u8]>)) -> Result<bool, Fault> {
            if let Some(fault) = self.fail_read {
                return Err(fault);
            }
            let scripted = match self.waiting.pop_front() {
                Some(scripted) => scripted,
                None if self.pending_normal > 0 => {
                    self.pending_normal -= 1;
                    normal()
                }
                None => return Ok(false),
            };
            self.gets += 1;
            let gets = self.gets;
            let packet = Packet {
                frames: scripted.frames,
                silent: scripted.silent,
                discontinuity: scripted.discontinuity,
                timestamp_error: scripted.timestamp_error,
                device_position: self.position,
                qpc_100ns: self.qpc_of(self.position),
                // The position read after GetBuffer is the end of the data captured so far.
                clock: self.clock_ok.then(|| Position {
                    frames: self.position + u64::from(scripted.frames),
                    qpc_100ns: self.qpc_of(self.position + u64::from(scripted.frames)),
                }),
            };
            let len = scripted.frames as usize * CHANNELS * 4;
            // Sample k of the device is the float k / 1000 in each packet, so a test can see order.
            for i in 0..len / 4 {
                self.bytes[i * 4..i * 4 + 4].copy_from_slice(&((self.position as f32 * CHANNELS as f32 + i as f32) / 1000.0).to_le_bytes());
            }
            self.position += u64::from(scripted.frames);
            let releases = &self.releases;
            let guard = UnwindGuard::new(|| releases.set(gets));
            fill(&packet, (!scripted.silent).then(|| &self.bytes[..len]));
            guard.disarm();
            self.releases.set(gets);
            Ok(true)
        }
        fn halt(&mut self) {
            self.halts += 1;
        }
    }

    fn config(calibration: Option<Calibration>) -> CaptureConfig {
        CaptureConfig {
            format: Format::new(RATE, CHANNELS as u16, SampleFormat::F32),
            wire: Wire::F32,
            stamper: CaptureStamper::new(ORIGIN_QPC, 0, calibration),
            origin: HostOrigin::default(),
            timeout_ms: 250,
        }
    }

    #[derive(Debug, Clone)]
    struct Seen {
        frames: usize,
        stamp: Stamp,
        position: Option<u64>,
        first_sample: f32,
        all_zero: bool,
    }

    struct Run {
        exit: Exit,
        seen: Vec<Seen>,
        errors: Vec<StreamError>,
        stats: CaptureStats,
    }

    fn drive(device: &mut Fake, config: &CaptureConfig) -> Run {
        let seen = Arc::new(Mutex::new(Vec::with_capacity(64)));
        let errors = Arc::new(Mutex::new(Vec::with_capacity(4)));
        let (log, faults) = (Arc::clone(&seen), Arc::clone(&errors));
        let mut capture: CaptureCallback = Box::new(move |block| {
            let mut first = f32::NAN;
            let mut all_zero = true;
            block.chunks(&mut |offset, s| {
                if offset == 0 {
                    first = s[0];
                }
                all_zero &= s.iter().all(|v| *v == 0.0);
            });
            log.lock().unwrap().push(Seen {
                frames: block.frames,
                stamp: block.stamp,
                position: block.device_position,
                first_sample: first,
                all_zero,
            });
        });
        let mut error: ErrorCallback = Box::new(move |e| faults.lock().unwrap().push(e));
        let mut scratch = vec![0.0f32; 64 * CHANNELS];
        let mut stats = CaptureStats::default();
        let exit = run(device, config, &mut scratch, &mut capture, &mut error, &mut stats);
        let (seen, errors) = (seen.lock().unwrap().clone(), errors.lock().unwrap().clone());
        Run { exit, seen, errors, stats }
    }

    #[test]
    fn every_waiting_packet_is_drained_in_order_and_stop_exits_cleanly() {
        let mut device = Fake::new([Step::Wake(vec![normal(); 3]), Step::Wake(vec![]), Step::Wake(vec![normal(); 2])]);
        let run = drive(&mut device, &config(None));
        assert_eq!((run.exit, run.seen.len(), run.errors.len()), (Exit::Stopped, 5, 0));
        assert_eq!(run.stats, CaptureStats { packets: 5, frames: 5 * u64::from(PERIOD), ..Default::default() });
        let positions: Vec<u64> = run.seen.iter().map(|s| s.position.unwrap()).collect();
        assert_eq!(positions, (0..5).map(|k| k * u64::from(PERIOD)).collect::<Vec<_>>());
        assert!(run.seen.iter().all(|s| s.frames == PERIOD as usize));
        // The converted samples are the device bytes, packet after packet.
        assert_eq!(run.seen[2].first_sample, (2.0 * PERIOD as f32 * CHANNELS as f32) / 1000.0);
        assert_eq!((device.gets, device.releases.get()), (5, 5));
    }

    #[test]
    fn a_silent_packet_is_delivered_as_zeros_whatever_its_bytes() {
        let silent = ScriptedPacket { silent: true, ..normal() };
        let mut device = Fake::new([Step::Wake(vec![normal(), silent, normal()])]);
        let run = drive(&mut device, &config(None));
        let zeros: Vec<bool> = run.seen.iter().map(|s| s.all_zero).collect();
        // The first packet starts at sample 0.0, so only its first sample is zero; check by value.
        assert_eq!(run.stats.silent, 1);
        assert_eq!(&zeros[1..], [true, false]);
        assert!(run.seen.iter().all(|s| s.stamp.quality == Quality::BackendEstimate), "silence is still timed");
    }

    #[test]
    fn a_discontinuity_is_counted_and_resets_the_fit_with_an_invalid_stamp() {
        let gap = ScriptedPacket { discontinuity: true, ..normal() };
        let mut device = Fake::new([Step::Wake(vec![normal(), normal(), gap, normal()])]);
        let run = drive(&mut device, &config(None));
        assert_eq!(run.stats.discontinuities, 1);
        let quality: Vec<Quality> = run.seen.iter().map(|s| s.stamp.quality).collect();
        assert_eq!(quality, [Quality::BackendEstimate, Quality::BackendEstimate, Quality::Invalid, Quality::BackendEstimate]);
        assert_eq!(run.seen[2].frames, PERIOD as usize, "the data still reaches the recorder");
    }

    #[test]
    fn a_timestamp_error_gives_an_invalid_stamp_and_is_counted() {
        let bad = ScriptedPacket { timestamp_error: true, ..normal() };
        let mut device = Fake::new([Step::Wake(vec![bad, normal()])]);
        let run = drive(&mut device, &config(None));
        assert_eq!(run.stats.timestamp_errors, 1);
        assert_eq!((run.seen[0].stamp.quality, run.seen[1].stamp.quality), (Quality::Invalid, Quality::BackendEstimate));
    }

    #[test]
    fn an_unreadable_clock_gives_invalid_stamps_and_keeps_capturing() {
        let mut device = Fake::wakes(3);
        device.clock_ok = false;
        let run = drive(&mut device, &config(None));
        assert_eq!((run.exit, run.seen.len(), run.errors.len()), (Exit::Stopped, 3, 0));
        assert!(run.seen.iter().all(|s| s.stamp.quality == Quality::Invalid));
    }

    #[test]
    fn stamps_are_origin_relative_monotonic_and_one_period_apart() {
        let mut device = Fake::wakes(10);
        let run = drive(&mut device, &config(None));
        let period_ns = i64::from(PERIOD) * 1_000_000_000 / i64::from(RATE);
        for pair in run.seen.windows(2) {
            let (a, b) = (pair[0].stamp, pair[1].stamp);
            assert!(b.host_callback_ns > a.host_callback_ns, "host time strictly increases");
            assert!((b.backend_frame_ns - a.backend_frame_ns - period_ns).abs() <= 100, "frames are a period apart");
            assert!(b.backend_callback_ns >= a.backend_callback_ns);
        }
        for seen in &run.seen {
            // The frame time is the packet's first frame; the callback is the read after it.
            assert!(seen.stamp.backend_frame_ns <= seen.stamp.backend_callback_ns);
        }
        assert_eq!(run.seen[0].stamp.backend_frame_ns, 0, "the first packet starts at the origin instant");
    }

    #[test]
    fn a_calibration_makes_stamps_measured_and_moves_the_frame_time_by_the_round_trip() {
        let calibration = Calibration { round_trip_ns: 6_000_000, uncertainty_ns: 40_000 };
        let (mut plain, mut calibrated) = (Fake::wakes(3), Fake::wakes(3));
        let a = drive(&mut plain, &config(None));
        let b = drive(&mut calibrated, &config(Some(calibration)));
        for (a, b) in a.seen.iter().zip(&b.seen) {
            assert_eq!(a.stamp.backend_frame_ns - b.stamp.backend_frame_ns, 6_000_000);
            assert_eq!((b.stamp.quality, b.stamp.offset_uncertainty_ns), (Quality::Measured, Some(40_000)));
            assert_eq!(a.stamp.backend_callback_ns, b.stamp.backend_callback_ns, "the callback instant is not shifted");
        }
    }

    #[test]
    fn a_read_fault_reports_once_and_exits() {
        let mut device = Fake::wakes(3);
        device.fail_read = Some(Fault::from_hresult(0x8889_0004_u32 as i32));
        let run = drive(&mut device, &config(None));
        assert_eq!((run.exit, run.errors), (Exit::Faulted(Fault::DeviceGone), vec![StreamError::DeviceNotAvailable]));
    }

    #[test]
    fn a_wait_timeout_reports_device_not_available_and_exits() {
        let mut device = Fake::new([Step::Wake(vec![normal()]), Step::Wait(Wake::Timeout), Step::Wake(vec![normal()])]);
        let run = drive(&mut device, &config(None));
        assert_eq!((run.exit, run.seen.len(), run.errors), (Exit::Timeout, 1, vec![StreamError::DeviceNotAvailable]));
    }

    #[test]
    fn a_failed_wait_reports_other() {
        let mut device = Fake::new([Step::Wait(Wake::Failed(Fault::Other(7)))]);
        let run = drive(&mut device, &config(None));
        assert_eq!((run.exit, run.errors), (Exit::Faulted(Fault::Other(7)), vec![StreamError::Other]));
    }

    #[test]
    fn the_loop_body_allocates_nothing() {
        let mut device = Fake::new((0..200).map(|_| Step::Normal(2)));
        let config = config(Some(Calibration { round_trip_ns: 1_000_000, uncertainty_ns: 1 }));
        let mut capture: CaptureCallback = Box::new(|block| block.chunks(&mut |_, s| assert!(s.len() <= 64 * CHANNELS)));
        let mut error: ErrorCallback = Box::new(|_| {});
        let mut scratch = vec![0.0f32; 64 * CHANNELS];
        let mut stats = CaptureStats::default();
        let (exit, allocations) = measure(|| run(&mut device, &config, &mut scratch, &mut capture, &mut error, &mut stats));
        assert_eq!(exit, Exit::Stopped);
        assert!(allocations.is_zero(), "the loop allocated: {allocations:?}");
        assert_eq!(stats.packets, 400);
    }

    fn serve_with(device: &mut Fake, mut capture: CaptureCallback) -> (Exit, Vec<StreamError>) {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let faults = Arc::clone(&errors);
        let mut error: ErrorCallback = Box::new(move |e| faults.lock().unwrap().push(e));
        let mut scratch = vec![0.0f32; 64 * CHANNELS];
        let exit = serve(device, &config(None), &mut scratch, &mut capture, &mut error, &mut CaptureStats::default());
        let errors = errors.lock().unwrap().clone();
        (exit, errors)
    }

    #[test]
    fn a_panicking_capture_callback_reports_other_once_and_tears_down() {
        let mut device = Fake::wakes(5);
        let calls = Arc::new(Mutex::new(0u32));
        let counted = Arc::clone(&calls);
        let capture: CaptureCallback = Box::new(move |_block| {
            *counted.lock().unwrap() += 1;
            panic!("capture callback bug");
        });
        let (exit, errors) = serve_with(&mut device, capture);
        assert_eq!(exit, Exit::Panicked);
        assert_eq!(errors, vec![StreamError::Other], "the error callback is called exactly once");
        assert_eq!(*calls.lock().unwrap(), 1, "the loop exited after the panic");
        assert_eq!(device.halts, 1, "Stop/Reset ran");
        assert_eq!((device.gets, device.releases.get()), (1, 1), "the packet was released on unwind");
    }

    #[test]
    fn serve_halts_after_a_normal_stop_and_a_fault() {
        let mut device = Fake::wakes(3);
        let (exit, errors) = serve_with(&mut device, Box::new(|block| block.chunks(&mut |_, _| {})));
        assert_eq!((exit, errors.len(), device.halts), (Exit::Stopped, 0, 1));
        let mut broken = Fake::wakes(1);
        broken.fail_read = Some(Fault::DeviceGone);
        let (exit, errors) = serve_with(&mut broken, Box::new(|_| {}));
        assert_eq!((exit, errors, broken.halts), (Exit::Faulted(Fault::DeviceGone), vec![StreamError::DeviceNotAvailable], 1));
    }
}
