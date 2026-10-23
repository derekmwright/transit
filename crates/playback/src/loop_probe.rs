//! Loopback calibration over native ASIO (issue #20, known issue K31): the click of the
//! hardware-insert probe ([`crate::hardware_probe`]), counted in the driver's sample positions
//! instead of the plan's frames.
//!
//! The drivers' `getLatencies` figures cannot be corrected by any fixed model (the error flips
//! sign with the buffer size and moves with the rate), so the app measures the loop at the
//! configured period and rate. A cable from an output to an input carries the click back: the
//! render callback adds it to the device buffer at sample position `origin + t` and the capture
//! callback records the input by position, so the lag of the click is the loop in device frames,
//! the figure the ASIO stamps need (`transit-audio-asio` places a calibrated capture frame from
//! it). Counting positions, not the plan's frames, keeps the input ring's backlog (known issue
//! K22) and the plan's resampling out of it, and counts in the stamps' own domain.
//!
//! The callbacks only read immutable parameters, store `f32` bits into a buffer allocated here
//! and bump atomics; they never allocate or lock. The analysis (the matched filter of
//! [`crate::hardware_probe::analyze`]: the median lag and the spread) runs on the control thread.

use crate::hardware_probe::{
    DURATION_MS, INTERVAL_MS, MAX_PROBE_CHANNELS, Measured, ProbeError, START_MS, TAIL_MS, analyze, click_template, frames_of,
};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const DONE: u8 = 2;
/// `origin` before the first render block of a measurement.
const NO_ORIGIN: u64 = u64::MAX;

/// The probe shared by the control thread and the device callbacks. One per Player.
pub struct LoopProbe {
    rate: u32,
    template: Box<[f32]>,
    /// The sum of the return channels, as `f32` bits, by probe frame (position minus `origin`).
    capture: Box<[AtomicU32]>,
    /// The sample position of probe frame 0: the first render block after arming.
    origin: AtomicU64,
    /// Probe frames rendered so far (for the progress line).
    played: AtomicU64,
    state: AtomicU8,
    /// A block came without a sample position: this driver cannot be measured this way.
    no_position: AtomicBool,
    send_n: AtomicU8,
    send: [AtomicU16; MAX_PROBE_CHANNELS],
    ret_n: AtomicU8,
    ret: [AtomicU16; MAX_PROBE_CHANNELS],
}

impl LoopProbe {
    /// A probe for a device at `rate`. Allocates the capture buffer (about 2.3 s of mono).
    #[must_use]
    pub fn new(rate: u32) -> Self {
        let rate = rate.max(8_000);
        let len = usize::try_from(Self::total_for(rate)).unwrap_or(0);
        Self {
            rate,
            template: click_template(rate).into_boxed_slice(),
            capture: (0..len).map(|_| AtomicU32::new(0)).collect(),
            origin: AtomicU64::new(NO_ORIGIN),
            played: AtomicU64::new(0),
            state: AtomicU8::new(IDLE),
            no_position: AtomicBool::new(false),
            send_n: AtomicU8::new(0),
            send: std::array::from_fn(|_| AtomicU16::new(0)),
            ret_n: AtomicU8::new(0),
            ret: std::array::from_fn(|_| AtomicU16::new(0)),
        }
    }

    fn total_for(rate: u32) -> u64 {
        frames_of(rate, DURATION_MS + TAIL_MS)
    }

    /// The device rate the probe counts at.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Whether a measurement is running.
    #[must_use]
    pub fn armed(&self) -> bool {
        self.state.load(Ordering::Acquire) == ARMED
    }

    /// Whether a block of the running (or last) measurement came without a sample position.
    #[must_use]
    pub fn lacks_position(&self) -> bool {
        self.no_position.load(Ordering::Relaxed)
    }

    /// Start a measurement: `send` and `ret` are 0-based device channels. False when one is
    /// already running or a list is empty or too long. Control side; clears the capture buffer.
    pub fn arm(&self, send: &[u16], ret: &[u16]) -> bool {
        if self.armed() || send.is_empty() || ret.is_empty() || send.len() > MAX_PROBE_CHANNELS || ret.len() > MAX_PROBE_CHANNELS {
            return false;
        }
        for (slot, ch) in self.send.iter().zip(send) {
            slot.store(*ch, Ordering::Relaxed);
        }
        for (slot, ch) in self.ret.iter().zip(ret) {
            slot.store(*ch, Ordering::Relaxed);
        }
        for slot in &self.capture {
            slot.store(0, Ordering::Relaxed);
        }
        self.send_n.store(send.len() as u8, Ordering::Relaxed);
        self.ret_n.store(ret.len() as u8, Ordering::Relaxed);
        self.origin.store(NO_ORIGIN, Ordering::Relaxed);
        self.played.store(0, Ordering::Relaxed);
        self.no_position.store(false, Ordering::Relaxed);
        self.state.store(ARMED, Ordering::Release);
        true
    }

    /// Stop a measurement that is running.
    pub fn cancel(&self) {
        self.state.store(IDLE, Ordering::Release);
    }

    /// Clicks sent so far of the clicks a measurement sends; zero when none is running.
    #[must_use]
    pub fn progress(&self) -> (u32, u32) {
        let first = frames_of(self.rate, START_MS);
        let interval = frames_of(self.rate, INTERVAL_MS).max(1);
        let duration = frames_of(self.rate, DURATION_MS);
        let total = u32::try_from((0..).map(|k| first + k * interval).take_while(|s| *s < duration).count()).unwrap_or(u32::MAX);
        match self.state.load(Ordering::Acquire) {
            ARMED => {
                let at = self.played.load(Ordering::Relaxed);
                let sent = if at <= first { 0 } else { (at - first - 1) / interval + 1 };
                (u32::try_from(sent).unwrap_or(u32::MAX).min(total), total)
            }
            DONE => (total, total),
            _ => (0, total),
        }
    }

    /// The result, once the measurement is done (and only once): the median lag in device frames
    /// and the spread of the clicks found.
    #[must_use]
    pub fn take_result(&self) -> Option<Result<Measured, ProbeError>> {
        if self.state.compare_exchange(DONE, IDLE, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        let recorded: Vec<f32> = self.capture.iter().map(|s| f32::from_bits(s.load(Ordering::Relaxed))).collect();
        Some(analyze(&recorded, self.rate))
    }

    /// The click sample at probe frame `t`, if one plays there.
    fn click_at(&self, t: u64) -> Option<f32> {
        let first = frames_of(self.rate, START_MS);
        if t < first || t >= frames_of(self.rate, DURATION_MS) {
            return None;
        }
        let offset = (t - first) % frames_of(self.rate, INTERVAL_MS).max(1);
        usize::try_from(offset).ok().and_then(|o| self.template.get(o)).copied()
    }

    /// Render callback: `out` is an interleaved chunk of `channels` the device plays from sample
    /// position `position` on (the block's position plus the chunk's offset). Adds the click to the
    /// send channels, clamped. The first chunk after arming fixes the origin.
    pub fn play(&self, out: &mut [f32], channels: usize, position: Option<u64>) {
        if self.state.load(Ordering::Acquire) != ARMED || channels == 0 {
            return;
        }
        let Some(position) = position else {
            self.no_position.store(true, Ordering::Relaxed);
            return;
        };
        let origin = match self.origin.load(Ordering::Relaxed) {
            NO_ORIGIN => {
                self.origin.store(position, Ordering::Release);
                position
            }
            origin => origin,
        };
        let n = usize::from(self.send_n.load(Ordering::Relaxed)).min(MAX_PROBE_CHANNELS);
        let frames = out.len() / channels;
        for (j, frame) in out.chunks_exact_mut(channels).enumerate() {
            let Some(t) = (position + j as u64).checked_sub(origin) else { continue };
            let Some(x) = self.click_at(t) else { continue };
            for ch in self.send.iter().take(n) {
                if let Some(o) = frame.get_mut(usize::from(ch.load(Ordering::Relaxed))) {
                    *o = (*o + x).clamp(-1.0, 1.0);
                }
            }
        }
        if let Some(end) = (position + frames as u64).checked_sub(origin) {
            self.played.fetch_max(end, Ordering::Relaxed);
        }
    }

    /// Capture callback: `samples` is an interleaved chunk of `channels` the device captured from
    /// sample position `position` on. Records the sum of the return channels by probe frame; the
    /// measurement is done once the capture reaches the end of its window.
    pub fn record(&self, samples: &[f32], channels: usize, position: Option<u64>) {
        if self.state.load(Ordering::Acquire) != ARMED || channels == 0 {
            return;
        }
        let Some(position) = position else {
            self.no_position.store(true, Ordering::Relaxed);
            return;
        };
        let origin = self.origin.load(Ordering::Acquire);
        if origin == NO_ORIGIN {
            return;
        }
        let n = usize::from(self.ret_n.load(Ordering::Relaxed)).min(MAX_PROBE_CHANNELS);
        let frames = samples.len() / channels;
        for (j, frame) in samples.chunks_exact(channels).enumerate() {
            let Some(t) = (position + j as u64).checked_sub(origin) else { continue };
            let Some(slot) = usize::try_from(t).ok().and_then(|t| self.capture.get(t)) else { break };
            let mut sum = 0.0f32;
            for ch in self.ret.iter().take(n) {
                if let Some(x) = frame.get(usize::from(ch.load(Ordering::Relaxed))) {
                    sum += *x;
                }
            }
            slot.store(sum.to_bits(), Ordering::Relaxed);
        }
        if (position + frames as u64).checked_sub(origin).is_some_and(|end| end >= Self::total_for(self.rate)) {
            let _ = self.state.compare_exchange(ARMED, DONE, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// A device of `period` frames and 2 channels whose input channel 1 is output channel 1
    /// delayed by `delay` device frames, starting at sample position `start`. Each switch runs the
    /// capture callback, then the render callback, as an ASIO driver does.
    fn run(probe: &LoopProbe, period: usize, delay: usize, start: u64) -> Result<Measured, ProbeError> {
        let mut sent: Vec<f32> = Vec::new();
        let mut position = start;
        for _ in 0..(10 * RATE as usize / period) {
            let input: Vec<f32> = (0..period)
                .flat_map(|j| {
                    let n = (position - start) as usize + j;
                    let x = n.checked_sub(delay).and_then(|k| sent.get(k)).copied().unwrap_or(0.0);
                    [0.0, x]
                })
                .collect();
            probe.record(&input, 2, Some(position));
            let mut output = vec![0.0f32; period * 2];
            probe.play(&mut output, 2, Some(position));
            sent.extend(output.as_chunks::<2>().0.iter().map(|f| f[1]));
            position += period as u64;
            if let Some(result) = probe.take_result() {
                return result;
            }
        }
        panic!("the probe did not finish");
    }

    #[test]
    fn the_lag_is_the_loop_in_device_frames_at_any_period_and_start_position() {
        for (period, delay, start) in [(64usize, 297usize, 0u64), (256, 1_065, 1_234_567), (128, 582, 64), (512, 2_250, 7)] {
            let probe = LoopProbe::new(RATE);
            assert!(probe.arm(&[1], &[1]));
            let m = run(&probe, period, delay, start).unwrap_or_else(|e| panic!("{period}: {e}"));
            assert_eq!((m.round_trip_frames, m.spread_frames), (delay as u32, 0), "period {period}");
            assert_eq!(m.found, m.sent);
            assert!(!probe.armed() && !probe.lacks_position());
        }
    }

    #[test]
    fn the_click_plays_on_the_send_channels_only_and_progress_counts_it() {
        let probe = LoopProbe::new(RATE);
        let mut out = vec![0.0f32; 4 * 4_800];
        probe.play(&mut out, 4, Some(0));
        assert!(out.iter().all(|x| *x == 0.0), "idle: nothing plays");
        assert!(probe.arm(&[2], &[0]));
        assert!(!probe.arm(&[2], &[0]), "one at a time");
        assert_eq!(probe.progress(), (0, 8));
        let mut peak = [0.0f32; 4];
        for k in 0..20u64 {
            let mut out = vec![0.0f32; 4 * 4_800];
            probe.play(&mut out, 4, Some(1_000 + k * 4_800));
            for frame in out.as_chunks::<4>().0 {
                for (p, x) in peak.iter_mut().zip(frame) {
                    *p = p.max(x.abs());
                }
            }
        }
        assert_eq!(peak[0] + peak[1] + peak[3], 0.0);
        assert!(peak[2] > 0.4);
        assert_eq!(probe.progress(), (8, 8));
        probe.cancel();
        assert_eq!(probe.progress(), (0, 8));
    }

    #[test]
    fn a_driver_without_positions_is_said_and_never_finishes() {
        let probe = LoopProbe::new(RATE);
        assert!(probe.arm(&[0], &[0]));
        let mut out = vec![0.0f32; 128];
        probe.play(&mut out, 2, None);
        probe.record(&out, 2, None);
        assert!(probe.lacks_position() && probe.armed() && probe.take_result().is_none());
    }

    #[test]
    fn silence_on_the_return_is_no_signal() {
        let probe = LoopProbe::new(RATE);
        assert!(probe.arm(&[0], &[0]));
        let mut position = 0u64;
        let result = loop {
            probe.record(&[0.0; 512], 2, Some(position));
            probe.play(&mut [0.0; 512], 2, Some(position));
            position += 256;
            if let Some(r) = probe.take_result() {
                break r;
            }
        };
        assert!(matches!(result, Err(ProbeError::NoSignal { found: 0, .. })));
    }
}
