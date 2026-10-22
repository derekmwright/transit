//! Round-trip measurement for hardware inserts (ADR 0015 section 5, slice d part 2).
//!
//! A click train goes out the device channels of a send path and the probe records the device
//! channels of a return path. Both are counted on the plan's own timeline, the frames the plan
//! has rendered since the probe armed: a click is added to the output at plan frame `t`, and the
//! live input block the plan reads while rendering frame `t` is recorded at `t`. That is the
//! delay a hardware insert on the pair adds to its strip, which is what delay compensation needs
//! (it includes the device buffers, the converters and the input ring's backlog as they are now).
//!
//! The click is the one `tools/audio-bench` uses: an impulse followed by a 1 ms raised-cosine
//! 3 kHz burst (a tone, so an AC-coupled input still passes it). The control side finds it again
//! with a matched filter, one lag per click, and reports the median with the spread.
//!
//! The audio thread only reads the immutable template and parameters, stores recorded samples as
//! `f32` bits into a buffer allocated with the Player, and bumps two counters. It never allocates
//! or locks; the analysis runs on the control thread after the probe is done.

use std::f32::consts::TAU;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// Channels a send or a return may span (device channels of the two paths).
pub const MAX_PROBE_CHANNELS: usize = 16;
/// The first click, in milliseconds after the probe arms.
pub const START_MS: u64 = 100;
/// Clicks repeat at this interval.
pub const INTERVAL_MS: u64 = 250;
/// Clicks are sent for this long after arming.
pub const DURATION_MS: u64 = 2_000;
/// The longest round trip the analysis looks for after each click.
pub const MAX_ROUND_TRIP_MS: u64 = 200;
/// Recording continues this long after the last click, to catch its return.
pub const TAIL_MS: u64 = 300;
/// Peak level of the click, linear.
pub const CLICK_LEVEL: f32 = 0.5;
/// Frequency of the burst.
pub const BURST_HZ: f32 = 3_000.0;
/// Normalized correlation needed to call a window a click.
pub const MIN_CORRELATION: f32 = 0.7;
/// The window energy must be at least this fraction of the template's.
pub const MIN_ENERGY_RATIO: f32 = 0.04;
/// Clicks that must be found for a measurement to count.
pub const MIN_FOUND: usize = 3;

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const DONE: u8 = 2;

/// The click as emitted and as matched.
#[must_use]
pub fn click_template(rate: u32) -> Vec<f32> {
    let burst = usize::try_from(rate / 1000).unwrap_or(0).max(8);
    let mut samples = vec![0.0f32; burst + 1];
    samples[0] = CLICK_LEVEL;
    for n in 0..burst {
        let phase = (n as f32 + 0.5) / burst as f32;
        let window = 0.5 * (1.0 - (TAU * phase).cos());
        samples[n + 1] = CLICK_LEVEL * window * (TAU * BURST_HZ * n as f32 / rate as f32).sin();
    }
    samples
}

fn frames_of(rate: u32, ms: u64) -> u64 {
    u64::from(rate) * ms / 1000
}

/// What a measurement found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    /// Median round trip in frames of the session rate.
    pub round_trip_frames: u32,
    /// Largest minus smallest round trip over the clicks found: the uncertainty.
    pub spread_frames: u32,
    /// Clicks found of the clicks sent.
    pub found: usize,
    pub sent: usize,
}

/// Why a measurement failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// The click did not come back loud and clean enough, on enough clicks.
    NoSignal { found: usize, sent: usize },
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSignal { found, sent } => write!(f, "the click came back on {found} of {sent} tries"),
        }
    }
}

/// The probe shared by the control thread and the audio thread. One per Player.
pub struct HwProbe {
    rate: u32,
    template: Box<[f32]>,
    /// The sum of the return channels, as `f32` bits, by plan frame since arming.
    capture: Box<[AtomicU32]>,
    /// Plan frames rendered since arming: the first frame of the next block.
    frame: AtomicU64,
    state: AtomicU8,
    send_n: AtomicU8,
    send: [AtomicU16; MAX_PROBE_CHANNELS],
    ret_n: AtomicU8,
    ret: [AtomicU16; MAX_PROBE_CHANNELS],
}

impl HwProbe {
    /// A probe for a plan at `rate`. Allocates the capture buffer (about 2.3 s of mono).
    #[must_use]
    pub fn new(rate: u32) -> Self {
        let rate = rate.max(8_000);
        let len = usize::try_from(Self::total_for(rate)).unwrap_or(0);
        Self {
            rate,
            template: click_template(rate).into_boxed_slice(),
            capture: (0..len).map(|_| AtomicU32::new(0)).collect(),
            frame: AtomicU64::new(0),
            state: AtomicU8::new(IDLE),
            send_n: AtomicU8::new(0),
            send: std::array::from_fn(|_| AtomicU16::new(0)),
            ret_n: AtomicU8::new(0),
            ret: std::array::from_fn(|_| AtomicU16::new(0)),
        }
    }

    fn total_for(rate: u32) -> u64 {
        frames_of(rate, DURATION_MS + TAIL_MS)
    }

    /// Whether a measurement is running (control: to refuse a second one).
    #[must_use]
    pub fn armed(&self) -> bool {
        self.state.load(Ordering::Acquire) == ARMED
    }

    /// Start a measurement: `send` and `ret` are 0-based device channels. False when one is
    /// already running or a list is empty or too long.
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
        self.send_n.store(send.len() as u8, Ordering::Relaxed);
        self.ret_n.store(ret.len() as u8, Ordering::Relaxed);
        self.frame.store(0, Ordering::Relaxed);
        self.state.store(ARMED, Ordering::Release);
        true
    }

    /// Stop a measurement that is running (control: the Player is going away or was cancelled).
    pub fn cancel(&self) {
        self.state.store(IDLE, Ordering::Release);
    }

    /// The result, once the measurement is done (and only once).
    #[must_use]
    pub fn take_result(&self) -> Option<Result<Measured, ProbeError>> {
        if self.state.compare_exchange(DONE, IDLE, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        let total = usize::try_from(Self::total_for(self.rate)).unwrap_or(0);
        let recorded: Vec<f32> = self.capture.iter().take(total).map(|s| f32::from_bits(s.load(Ordering::Relaxed))).collect();
        Some(analyze(&recorded, self.rate))
    }

    /// Audio thread: the plan rendered a block of `frames` with this live `input` (planar device
    /// channels). Records the return and returns the plan frame the block starts at, `None` when
    /// no measurement is running.
    pub fn record_block(&self, input: &[Vec<f32>], frames: usize) -> Option<u64> {
        if self.state.load(Ordering::Acquire) != ARMED {
            return None;
        }
        let start = self.frame.load(Ordering::Relaxed);
        let n = usize::from(self.ret_n.load(Ordering::Relaxed)).min(MAX_PROBE_CHANNELS);
        for j in 0..frames {
            let Some(slot) = usize::try_from(start).ok().and_then(|s| self.capture.get(s + j)) else { break };
            let mut sum = 0.0f32;
            for ch in self.ret.iter().take(n) {
                if let Some(x) = input.get(usize::from(ch.load(Ordering::Relaxed))).and_then(|c| c.get(j)) {
                    sum += *x;
                }
            }
            slot.store(sum.to_bits(), Ordering::Relaxed);
        }
        let next = start + frames as u64;
        self.frame.store(next, Ordering::Relaxed);
        if next >= Self::total_for(self.rate) {
            self.state.store(DONE, Ordering::Release);
        }
        Some(start)
    }

    /// Audio thread: add the click, when plan frame `t` falls inside one, to the send channels of
    /// the device frame `out`. Only those channels are touched, so no other sink hears it.
    pub fn add_click(&self, out: &mut [f32], t: u64) {
        let first = frames_of(self.rate, START_MS);
        if t < first || t >= frames_of(self.rate, DURATION_MS) {
            return;
        }
        let offset = (t - first) % frames_of(self.rate, INTERVAL_MS).max(1);
        let Some(&x) = usize::try_from(offset).ok().and_then(|o| self.template.get(o)) else { return };
        let n = usize::from(self.send_n.load(Ordering::Relaxed)).min(MAX_PROBE_CHANNELS);
        for ch in self.send.iter().take(n) {
            if let Some(o) = out.get_mut(usize::from(ch.load(Ordering::Relaxed))) {
                *o += x;
            }
        }
    }
}

/// Find each click in the recorded return and report the round trip. Pure; control side.
pub fn analyze(recorded: &[f32], rate: u32) -> Result<Measured, ProbeError> {
    let template = click_template(rate);
    let energy: f32 = template.iter().map(|s| s * s).sum();
    let first = frames_of(rate, START_MS) as usize;
    let interval = frames_of(rate, INTERVAL_MS).max(1) as usize;
    let duration = frames_of(rate, DURATION_MS) as usize;
    let reach = frames_of(rate, MAX_ROUND_TRIP_MS) as usize;
    let sent = (0..).map(|k| first + k * interval).take_while(|s| *s < duration).count();
    let mut lags: Vec<usize> = Vec::new();
    for k in 0..sent {
        let start = first + k * interval;
        let Some(window) = recorded.get(start..) else { break };
        let window = window.get(..(reach + template.len()).min(window.len())).unwrap_or(&[]);
        let mut best: Option<(usize, f32)> = None;
        for lag in 0..window.len().saturating_sub(template.len()) {
            let seg = &window[lag..lag + template.len()];
            let dot: f32 = seg.iter().zip(&template).map(|(a, b)| a * b).sum();
            let e: f32 = seg.iter().map(|a| a * a).sum();
            if e < energy * MIN_ENERGY_RATIO {
                continue;
            }
            let c = dot / (e * energy).sqrt();
            if c >= MIN_CORRELATION && best.is_none_or(|(_, b)| c > b) {
                best = Some((lag, c));
            }
        }
        if let Some((lag, _)) = best {
            lags.push(lag);
        }
    }
    if lags.len() < MIN_FOUND {
        return Err(ProbeError::NoSignal { found: lags.len(), sent });
    }
    lags.sort_unstable();
    let median = lags[lags.len() / 2];
    let spread = lags[lags.len() - 1] - lags[0];
    Ok(Measured {
        round_trip_frames: u32::try_from(median).unwrap_or(u32::MAX),
        spread_frames: u32::try_from(spread).unwrap_or(u32::MAX),
        found: lags.len(),
        sent,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// The click train as the probe emits it, delayed by `delay` and scaled by `gain`.
    fn loop_back(delay: usize, gain: f32, noise: f32) -> Vec<f32> {
        let total = HwProbe::total_for(RATE) as usize;
        let mut out = vec![0.0f32; total];
        let template = click_template(RATE);
        let mut seed = 12_345u32;
        for x in &mut out {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *x = noise * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
        }
        let first = frames_of(RATE, START_MS) as usize;
        let interval = frames_of(RATE, INTERVAL_MS) as usize;
        let mut k = 0;
        while first + k * interval < frames_of(RATE, DURATION_MS) as usize {
            for (n, x) in template.iter().enumerate() {
                if let Some(o) = out.get_mut(first + k * interval + delay + n) {
                    *o += x * gain;
                }
            }
            k += 1;
        }
        out
    }

    #[test]
    fn a_delayed_copy_measures_its_delay_exactly() {
        for delay in [0, 1, 143, 480, 4_000, 9_000] {
            let m = analyze(&loop_back(delay, 1.0, 0.0), RATE).unwrap();
            assert_eq!((m.round_trip_frames as usize, m.spread_frames), (delay, 0), "delay {delay}");
            assert_eq!(m.found, m.sent);
        }
    }

    #[test]
    fn a_quiet_noisy_return_still_measures_and_silence_does_not() {
        let m = analyze(&loop_back(700, 0.4, 0.05), RATE).unwrap();
        assert_eq!(m.round_trip_frames, 700);
        assert!(matches!(
            analyze(&loop_back(700, 1.0, 0.0).iter().map(|_| 0.0).collect::<Vec<_>>(), RATE),
            Err(ProbeError::NoSignal { found: 0, .. })
        ));
        assert!(analyze(&loop_back(700, 0.01, 0.0), RATE).is_err(), "too quiet to call a click");
        assert!(analyze(&[], RATE).is_err());
    }

    #[test]
    fn the_spread_is_the_range_of_the_lags_and_the_median_resists_a_stray() {
        let mut r = loop_back(300, 1.0, 0.0);
        // Move one click 5 frames later: the median stays, the spread says so.
        let first = frames_of(RATE, START_MS) as usize;
        let interval = frames_of(RATE, INTERVAL_MS) as usize;
        let template = click_template(RATE);
        for n in 0..template.len() + 5 {
            r[first + interval + 300 + n] = 0.0;
        }
        for (n, x) in template.iter().enumerate() {
            r[first + interval + 305 + n] = *x;
        }
        let m = analyze(&r, RATE).unwrap();
        assert_eq!((m.round_trip_frames, m.spread_frames), (300, 5));
    }

    #[test]
    fn the_audio_side_emits_on_the_send_channels_only_and_records_the_return_sum() {
        let p = HwProbe::new(RATE);
        assert!(p.record_block(&[vec![0.0; 8]], 8).is_none(), "idle");
        assert!(p.arm(&[2, 3], &[1, 0]));
        assert!(!p.arm(&[0], &[0]), "one measurement at a time");
        let mut frame = [0.0f32; 6];
        p.add_click(&mut frame, frames_of(RATE, START_MS));
        assert_eq!(frame, [0.0, 0.0, CLICK_LEVEL, CLICK_LEVEL, 0.0, 0.0]);
        let mut quiet = [0.0f32; 6];
        p.add_click(&mut quiet, frames_of(RATE, START_MS) + 5_000);
        assert_eq!(quiet, [0.0; 6]);
        let input = vec![vec![0.25f32; 4], vec![0.5; 4]];
        assert_eq!(p.record_block(&input, 4), Some(0));
        assert_eq!(p.record_block(&input, 4), Some(4));
        assert_eq!(f32::from_bits(p.capture[5].load(Ordering::Relaxed)), 0.75);
        assert!(p.take_result().is_none(), "not done yet");
        p.cancel();
        assert!(!p.armed());
    }
}
