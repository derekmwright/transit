//! Level meters: an ITU-R BS.1770 loudness meter
//! (momentary, short-term and gated integrated LUFS plus 4× oversampled true peak).
//!
//! None of the `process` methods allocate.

use crate::biquad::{Biquad, Coeffs};
use crate::util::sane_sr;
use crate::{MIN_DB, gain_to_db};

const SUB_BLOCKS: usize = 30; // 3 s of 100 ms sub-blocks
const HIST_MIN: f32 = -70.0;
const HIST_STEP: f32 = 0.05;
const HIST_BINS: usize = 1800; // -70 .. +20 LUFS
const TP_PHASES: usize = 4;
const TP_TAPS: usize = 12;

/// ITU-R BS.1770 / EBU R128 loudness meter.
#[derive(Debug, Clone)]
pub struct LoudnessMeter {
    sr: f32,
    weights: Vec<f32>,
    filters: Vec<[Biquad; 2]>,
    block_len: usize,
    block_pos: usize,
    block_acc: Vec<f64>,
    sub: [f64; SUB_BLOCKS],
    sub_idx: usize,
    sub_filled: usize,
    hist_count: Vec<u64>,
    hist_energy: Vec<f64>,
    tp_coefs: [[f32; TP_TAPS]; TP_PHASES],
    tp_hist: Vec<[f32; TP_TAPS]>,
    true_peak: f32,
    max_momentary: f32,
}

/// Loudness from mean-square energy (BS.1770 eq. 2).
fn lufs(energy: f64) -> f32 {
    if energy <= 1e-20 { MIN_DB } else { (-0.691 + 10.0 * energy.log10()) as f32 }
}

/// K-weighting pre-filter (high shelf) and RLB high-pass designed for any sample rate.
fn k_weighting(sr: f32) -> [Coeffs; 2] {
    let sr = f64::from(sr);
    // Bilinear prewarp. A corner at or past Nyquist wraps tan() negative (poles outside the unit
    // circle) or aliases it, so it is held just below Nyquist, as `Coeffs::design` does.
    let prewarp = |f0: f64| (std::f64::consts::PI * f0.min(sr * 0.49) / sr).tan();
    // Stage 1: shelving.
    let (f0, g, q) = (1_681.974_450_955_533, 3.999_843_853_973_347, 0.707_175_236_955_419_6);
    let k = prewarp(f0);
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let s1 = Coeffs {
        b0: ((vh + vb * k / q + k * k) / a0) as f32,
        b1: (2.0 * (k * k - vh) / a0) as f32,
        b2: ((vh - vb * k / q + k * k) / a0) as f32,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / q + k * k) / a0) as f32,
    };
    // Stage 2: high-pass.
    let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = prewarp(f0);
    let a0 = 1.0 + k / q + k * k;
    let s2 = Coeffs { b0: 1.0, b1: -2.0, b2: 1.0, a1: (2.0 * (k * k - 1.0) / a0) as f32, a2: ((1.0 - k / q + k * k) / a0) as f32 };
    [s1, s2]
}

impl LoudnessMeter {
    /// Creates a meter. Channel weights default to 1.0, except a 6-channel (5.1: L R C LFE Ls Rs)
    /// layout which uses 0 for LFE and 1.41 for the surrounds.
    pub fn new(sample_rate: f32, channels: usize) -> Self {
        let sr = sane_sr(sample_rate);
        let channels = channels.clamp(1, crate::util::MAX_CHANNELS);
        let weights = if channels == 6 { vec![1.0, 1.0, 1.0, 0.0, 1.41, 1.41] } else { vec![1.0; channels] };
        let [c1, c2] = k_weighting(sr);
        // Interpolation filter: windowed sinc at the original Nyquist, split into 4 phases.
        let mut tp_coefs = [[0.0f32; TP_TAPS]; TP_PHASES];
        let total = TP_TAPS * TP_PHASES;
        let center = (total as f64 - 1.0) / 2.0;
        for i in 0..total {
            let t = (i as f64 - center) / TP_PHASES as f64;
            let sinc = if t.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t) };
            let x = i as f64 / (total as f64 - 1.0);
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * x).cos() + 0.08 * (4.0 * std::f64::consts::PI * x).cos();
            if let Some(slot) = tp_coefs.get_mut(i % TP_PHASES).and_then(|ph| ph.get_mut(i / TP_PHASES)) {
                *slot = (sinc * w) as f32;
            }
        }
        for ph in &mut tp_coefs {
            let s: f32 = ph.iter().sum();
            if s.abs() > 1e-6 {
                ph.iter_mut().for_each(|c| *c /= s);
            }
        }
        Self {
            sr,
            weights,
            filters: vec![[Biquad::new(c1), Biquad::new(c2)]; channels],
            block_len: ((sr * 0.1).round() as usize).max(1),
            block_pos: 0,
            block_acc: vec![0.0; channels],
            sub: [0.0; SUB_BLOCKS],
            sub_idx: 0,
            sub_filled: 0,
            hist_count: vec![0; HIST_BINS],
            hist_energy: vec![0.0; HIST_BINS],
            tp_coefs,
            tp_hist: vec![[0.0; TP_TAPS]; channels],
            true_peak: 0.0,
            max_momentary: MIN_DB,
        }
    }

    /// Overrides a channel's weighting (e.g. 1.41 for surrounds, 0 to exclude LFE).
    pub fn set_channel_weight(&mut self, channel: usize, weight: f32) {
        if let Some(w) = self.weights.get_mut(channel) {
            *w = if weight.is_finite() { weight.clamp(0.0, 4.0) } else { 1.0 };
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }

    pub fn reset(&mut self) {
        for f in &mut self.filters {
            f[0].reset();
            f[1].reset();
        }
        self.block_pos = 0;
        self.block_acc.iter_mut().for_each(|v| *v = 0.0);
        self.sub = [0.0; SUB_BLOCKS];
        self.sub_idx = 0;
        self.sub_filled = 0;
        self.hist_count.iter_mut().for_each(|v| *v = 0);
        self.hist_energy.iter_mut().for_each(|v| *v = 0.0);
        self.tp_hist.iter_mut().for_each(|h| *h = [0.0; TP_TAPS]);
        self.true_peak = 0.0;
        self.max_momentary = MIN_DB;
    }

    /// Feeds planar audio. Channels beyond the meter's channel count are ignored.
    pub fn process(&mut self, io: &[Vec<f32>], frames: usize) {
        let ch = self.filters.len().min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        for n in 0..frames {
            for c in 0..ch {
                let x = io.get(c).and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
                if let Some([f1, f2]) = self.filters.get_mut(c) {
                    let y = f2.process_sample(f1.process_sample(x));
                    if let Some(acc) = self.block_acc.get_mut(c) {
                        *acc += f64::from(y) * f64::from(y);
                    }
                }
                self.true_peak_sample(c, x);
            }
            self.block_pos += 1;
            if self.block_pos >= self.block_len {
                self.finish_sub_block();
            }
        }
    }

    fn true_peak_sample(&mut self, c: usize, x: f32) {
        let Some(h) = self.tp_hist.get_mut(c) else { return };
        h.copy_within(0..TP_TAPS - 1, 1);
        h[0] = x;
        let mut peak = x.abs();
        for ph in &self.tp_coefs {
            let y: f32 = ph.iter().zip(h.iter()).map(|(a, b)| a * b).sum();
            peak = peak.max(y.abs());
        }
        if peak > self.true_peak {
            self.true_peak = peak;
        }
    }

    fn finish_sub_block(&mut self) {
        let len = self.block_pos.max(1) as f64;
        let mut energy = 0.0;
        for (acc, w) in self.block_acc.iter_mut().zip(&self.weights) {
            energy += f64::from(*w) * *acc / len;
            *acc = 0.0;
        }
        self.block_pos = 0;
        if let Some(s) = self.sub.get_mut(self.sub_idx) {
            *s = energy;
        }
        self.sub_idx = (self.sub_idx + 1) % SUB_BLOCKS;
        self.sub_filled = (self.sub_filled + 1).min(SUB_BLOCKS);
        if self.sub_filled >= 4 {
            let z = self.recent_energy(4);
            let l = lufs(z);
            self.max_momentary = self.max_momentary.max(l);
            if l > HIST_MIN {
                let bin = (((l - HIST_MIN) / HIST_STEP) as usize).min(HIST_BINS - 1);
                if let (Some(c), Some(e)) = (self.hist_count.get_mut(bin), self.hist_energy.get_mut(bin)) {
                    *c += 1;
                    *e += z;
                }
            }
        }
    }

    /// Mean energy of the last `n` completed 100 ms sub-blocks.
    fn recent_energy(&self, n: usize) -> f64 {
        let n = n.min(self.sub_filled);
        if n == 0 {
            return 0.0;
        }
        let mut sum = 0.0;
        for k in 1..=n {
            let idx = (self.sub_idx + SUB_BLOCKS - k) % SUB_BLOCKS;
            sum += self.sub.get(idx).copied().unwrap_or(0.0);
        }
        sum / n as f64
    }

    /// Momentary loudness (400 ms window), LUFS. -144 until 400 ms have been measured.
    pub fn momentary_lufs(&self) -> f32 {
        if self.sub_filled < 4 { MIN_DB } else { lufs(self.recent_energy(4)) }
    }

    /// Short-term loudness (3 s window, or what is available so far), LUFS.
    pub fn short_term_lufs(&self) -> f32 {
        if self.sub_filled < 4 { MIN_DB } else { lufs(self.recent_energy(SUB_BLOCKS)) }
    }

    /// Gated integrated loudness (absolute gate -70 LUFS, relative gate -10 LU), LUFS.
    pub fn integrated_lufs(&self) -> f32 {
        let (mut n, mut e) = (0u64, 0.0f64);
        for (c, s) in self.hist_count.iter().zip(&self.hist_energy) {
            n += c;
            e += s;
        }
        if n == 0 {
            return MIN_DB;
        }
        let gate = lufs(e / n as f64) - 10.0;
        let (mut n2, mut e2) = (0u64, 0.0f64);
        for (i, (c, s)) in self.hist_count.iter().zip(&self.hist_energy).enumerate() {
            let center = HIST_MIN + (i as f32 + 0.5) * HIST_STEP;
            if center > gate {
                n2 += c;
                e2 += s;
            }
        }
        if n2 == 0 { MIN_DB } else { lufs(e2 / n2 as f64) }
    }

    /// Maximum true peak (4× oversampled) in dBTP.
    pub fn true_peak_db(&self) -> f32 {
        gain_to_db(self.true_peak)
    }
}
