//! Onset of a sound on the audio return, by sample position, for the MIDI-to-audio onset latency
//! (`--midi-roundtrip`).
//!
//! A synth note is not a known waveform, so the click's matched filter ([`crate::probe`], the
//! app's loop probe) cannot find it. Instead: a short causal envelope (the mean absolute value over
//! [`window`] samples, about 0.25 ms), a threshold 12 dB over the envelope's peak in a stretch of
//! noise floor measured before any note, and the first crossing of it; then a refinement to the
//! first sample where the envelope exceeds 10 percent of its peak in the [`PEAK_MS`] after that
//! crossing (the attack's rise, independent of the floor). The refined sample is the onset; the
//! distance between the two crossings is reported as the detector's uncertainty. The causal
//! envelope never reports an onset before the sound starts, and lags it by at most one window.

/// The threshold is this many times the reference floor's envelope peak (+12 dB).
pub const FLOOR_FACTOR: f32 = 4.0;
/// A pre-send window counts as at the noise floor while its envelope peak is at most this many
/// times the reference floor's (+6 dB).
pub const QUIET_FACTOR: f32 = 2.0;
/// The threshold is never below this level (-60 dBFS), so a digitally silent floor still needs a
/// real signal.
pub const MIN_THRESHOLD: f32 = 0.001;
/// A reference floor louder than this (-20 dBFS) is not a floor: a drone, a held note or the wrong input.
pub const MAX_FLOOR: f32 = 0.1;
/// The refinement looks for the attack's peak this long after the threshold crossing.
pub const PEAK_MS: u32 = 20;
/// The refinement's level: this fraction of the attack's peak.
pub const REFINE_FRACTION: f32 = 0.1;

/// The envelope window at `rate`: about 0.25 ms, at least one sample.
pub fn window(rate: u32) -> usize {
    usize::try_from(rate / 4000).unwrap_or(1).max(1)
}

/// Magnitude of one sample; a non-finite sample counts as full scale (a broken sample is not silence).
fn magnitude(x: f32) -> f32 {
    if x.is_finite() { x.abs() } else { 1.0 }
}

/// The envelope at sample `n`: the mean magnitude of the `w` samples ending at `n` (fewer at the
/// start of `samples`, divided by `w` all the same).
pub fn envelope_at(samples: &[f32], n: usize, w: usize) -> f32 {
    let w = w.max(1);
    let start = (n + 1).saturating_sub(w);
    samples.get(start..=n).map_or(0.0, |s| s.iter().map(|&x| magnitude(x)).sum::<f32>() / w as f32)
}

/// The level of the return over a stretch with no note.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Floor {
    /// Peak of the envelope (the first `w - 1` samples, whose window is partial, are left out).
    pub peak: f32,
    /// Root mean square of the samples.
    pub rms: f32,
}

/// The floor of `samples` with envelope window `w` (silence when empty).
pub fn noise_floor(samples: &[f32], w: usize) -> Floor {
    let peak = (w.max(1) - 1..samples.len()).map(|n| envelope_at(samples, n, w)).fold(0.0f32, f32::max);
    let sum: f64 = samples.iter().map(|&x| f64::from(magnitude(x)).powi(2)).sum();
    let rms = if samples.is_empty() { 0.0 } else { (sum / samples.len() as f64).sqrt() as f32 };
    Floor { peak, rms }
}

/// The level an onset's envelope must exceed over the `reference` floor, or why that floor is too
/// loud to measure over.
pub fn threshold(reference: Floor) -> Result<f32, String> {
    if reference.peak > MAX_FLOOR {
        return Err(format!(
            "the return is not quiet with no note playing (envelope peak {}, above {}): a drone, a held note or the wrong input; check --in",
            dbfs(reference.peak),
            dbfs(MAX_FLOOR)
        ));
    }
    Ok((reference.peak * FLOOR_FACTOR).max(MIN_THRESHOLD))
}

/// Whether the window before a send (`pre`) is at the `reference` noise floor; why not otherwise.
pub fn at_floor(reference: Floor, pre: Floor) -> Result<(), String> {
    let limit = (reference.peak * QUIET_FACTOR).max(MIN_THRESHOLD / 2.0);
    if pre.peak > limit {
        return Err(format!(
            "the return before the note is above the noise floor (envelope peak {}, floor {}): the previous note's release is still sounding or something else plays; use the synth's shortest release, raise --gap-ms",
            dbfs(pre.peak),
            dbfs(reference.peak)
        ));
    }
    Ok(())
}

/// The first sample at or after `from` whose envelope exceeds `threshold`.
pub fn first_crossing(samples: &[f32], from: usize, w: usize, threshold: f32) -> Option<usize> {
    (from..samples.len()).find(|&n| envelope_at(samples, n, w) > threshold)
}

/// An onset found on the envelope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Onset {
    /// The first crossing of the floor threshold.
    pub crossing: usize,
    /// The first crossing of [`REFINE_FRACTION`] of the attack's peak: the onset.
    pub at: usize,
    /// The envelope's peak in the [`PEAK_MS`] after the threshold crossing.
    pub peak: f32,
}

impl Onset {
    /// The detector's uncertainty: the distance between the two crossings, in samples.
    pub fn uncertainty(&self) -> usize {
        self.at.abs_diff(self.crossing)
    }
}

/// Refine the threshold `crossing` of `samples`: the envelope's peak over the `peak_len` samples
/// from it, then the first sample from `from` on whose envelope exceeds both
/// [`REFINE_FRACTION`] of that peak and `guard` (a level the floor stays under).
pub fn refine(samples: &[f32], from: usize, crossing: usize, w: usize, peak_len: usize, guard: f32) -> Onset {
    let end = crossing.saturating_add(peak_len).min(samples.len());
    let peak = (crossing..end).map(|n| envelope_at(samples, n, w)).fold(0.0f32, f32::max);
    let level = (peak * REFINE_FRACTION).max(guard);
    let at = (from.min(crossing)..end).find(|&n| envelope_at(samples, n, w) > level).unwrap_or(crossing);
    Onset { crossing, at, peak }
}

/// A level in dBFS for messages ("-inf dBFS" for silence).
pub fn dbfs(level: f32) -> String {
    if level > 0.0 { format!("{:.1} dBFS", 20.0 * level.log10()) } else { "-inf dBFS".to_string() }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Deterministic noise in `-amplitude..=amplitude`.
    fn noise(n: usize, amplitude: f32, seed: u32) -> Vec<f32> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    #[test]
    fn the_threshold_sits_above_the_noise_floor_and_the_onset_is_refined_on_the_attack() {
        let w = window(48_000);
        assert_eq!(w, 12);
        let reference = noise_floor(&noise(4800, 0.004, 7), w);
        assert!(reference.peak > 0.0005 && reference.peak < 0.004, "{reference:?}");
        assert!(reference.rms > 0.001 && reference.rms < 0.004, "{reference:?}");
        let level = threshold(reference).unwrap();
        assert!((level - reference.peak * FLOOR_FACTOR).abs() < 1e-6);
        // The same noise continues, then a note whose attack rises linearly over 200 samples from 300.
        let mut trial = noise(2000, 0.004, 9);
        for (i, x) in trial.iter_mut().enumerate().skip(300) {
            *x += 0.5 * ((i - 300) as f32 / 200.0).min(1.0) * if i.is_multiple_of(2) { 1.0 } else { -1.0 };
        }
        at_floor(reference, noise_floor(&trial[..300], w)).unwrap();
        assert_eq!(first_crossing(&trial[..300], 0, w, level), None, "the noise alone never crosses");
        let crossing = first_crossing(&trial, 0, w, level).unwrap();
        assert!((300..330).contains(&crossing), "threshold crossing at {crossing}");
        let onset = refine(&trial, 0, crossing, w, 960, level / 2.0);
        // The envelope peaks at 0.5 (plus noise); 10 percent of it is reached 20 samples into the
        // ramp, plus the half window the causal envelope lags.
        assert!((onset.peak - 0.5).abs() < 0.01, "{onset:?}");
        assert!((320..334).contains(&onset.at), "{onset:?}");
        assert!(onset.at >= 300, "never before the sound starts");
        assert_eq!(onset.uncertainty(), onset.at - crossing);
    }

    #[test]
    fn a_fast_attack_lands_within_one_window_of_its_start() {
        let w = window(48_000);
        let mut trial = noise(1000, 0.002, 3);
        for (i, x) in trial.iter_mut().enumerate().skip(500) {
            *x = if i.is_multiple_of(2) { 0.5 } else { -0.5 };
        }
        let level = threshold(noise_floor(&trial[..400], w)).unwrap();
        let crossing = first_crossing(&trial, 0, w, level).unwrap();
        let onset = refine(&trial, 0, crossing, w, 960, level / 2.0);
        assert_eq!(crossing, 500);
        assert!((500..500 + w).contains(&onset.at), "{onset:?}");
    }

    #[test]
    fn a_silent_floor_still_needs_a_real_signal_and_a_loud_one_is_refused() {
        assert_eq!(threshold(noise_floor(&[0.0; 64], 4)).unwrap(), MIN_THRESHOLD);
        assert_eq!(threshold(noise_floor(&[], 4)).unwrap(), MIN_THRESHOLD);
        assert_eq!(first_crossing(&[0.0005, -0.0009, 0.0011], 0, 1, MIN_THRESHOLD), Some(2));
        let loud = threshold(noise_floor(&noise(480, 0.3, 3), 4)).unwrap_err();
        assert!(loud.contains("not quiet"), "{loud}");
        let reference = noise_floor(&noise(480, 0.002, 5), 4);
        let ringing = at_floor(reference, noise_floor(&noise(480, 0.02, 6), 4)).unwrap_err();
        assert!(ringing.contains("above the noise floor") && ringing.contains("--gap-ms"), "{ringing}");
        assert_eq!(first_crossing(&[0.0, f32::NAN], 0, 1, 0.5), Some(1), "a broken sample is not silence");
        assert_eq!(dbfs(0.0), "-inf dBFS");
        assert_eq!(dbfs(1.0), "0.0 dBFS");
    }
}
