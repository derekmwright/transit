//! What a hosted plug-in's slot outputs for a block the plug-in is not called for while it is in
//! the engine (review R1 of audit F08): the main thread holds the instance (a restart, a rescan,
//! a mapping read) or a restart has parked it. The slot then outputs silence, reached by a short
//! fade from the last sample the plug-in put out, and fades back in when the plug-in processes
//! again. Never the unprocessed input: it would pass dry and un-delayed while the slot still
//! reports its latency to delay compensation. Allocation-free.

/// Fade length: 2 ms.
const FADE_SECONDS: f64 = 0.002;

/// Per channel, at most this many (the host strips' cap).
pub(crate) const MAX_CHANNELS: usize = 64;

pub(crate) struct Gate {
    /// The last sample of each channel the plug-in put out.
    last: [f32; MAX_CHANNELS],
    /// Fade length in samples (at least 1).
    fade: usize,
    /// Samples into the current run of skipped blocks.
    out_pos: usize,
    /// Samples processed since the last skipped block (`usize::MAX`: no fade in due).
    in_pos: usize,
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate { last: [0.0; MAX_CHANNELS], fade: 96, out_pos: 0, in_pos: usize::MAX }
    }

    /// The rate the plug-in is prepared for (sets the fade length).
    pub(crate) fn set_rate(&mut self, sample_rate: f64) {
        let n = (sample_rate * FADE_SECONDS).round();
        self.fade = if n.is_finite() && n >= 1.0 { n as usize } else { 1 };
    }

    fn gain_out(&self, k: usize) -> f32 {
        if k < self.fade { 1.0 - k as f32 / self.fade as f32 } else { 0.0 }
    }

    /// A block the plug-in is not called for: the first `ch` channels of `io` become the fade out
    /// of the last output (continued across skipped blocks), then silence.
    pub(crate) fn skip(&mut self, io: &mut [Vec<f32>], ch: usize, frames: usize) {
        for (c, buf) in io.iter_mut().take(ch).enumerate() {
            let last = self.last.get(c).copied().unwrap_or(0.0);
            for (i, x) in buf.iter_mut().take(frames).enumerate() {
                *x = last * self.gain_out(self.out_pos.saturating_add(i));
            }
        }
        self.out_pos = self.out_pos.saturating_add(frames);
        self.in_pos = 0;
    }

    /// A block the plug-in processed: faded in after skipped blocks, and its last samples kept.
    pub(crate) fn processed(&mut self, io: &mut [Vec<f32>], ch: usize, frames: usize) {
        if self.in_pos < self.fade {
            for buf in io.iter_mut().take(ch) {
                for (i, x) in buf.iter_mut().take(frames).enumerate() {
                    let k = self.in_pos.saturating_add(i);
                    if k < self.fade {
                        *x *= k as f32 / self.fade as f32;
                    }
                }
            }
            self.in_pos = self.in_pos.saturating_add(frames);
        }
        if frames > 0 {
            for (c, buf) in io.iter().take(ch).enumerate() {
                if let (Some(l), Some(&x)) = (self.last.get_mut(c), buf.get(frames - 1)) {
                    *l = x;
                }
            }
        }
        self.out_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_blocks_fade_out_to_silence_and_processing_fades_back_in() {
        let mut g = Gate::new();
        g.set_rate(2000.0); // 4-sample fades
        let mut io = vec![vec![0.8f32; 3]];
        g.processed(&mut io, 1, 3);
        assert_eq!(io[0], [0.8, 0.8, 0.8], "no fade before any skip");
        let mut io = vec![vec![0.5f32; 3]];
        g.skip(&mut io, 1, 3);
        assert_eq!(io[0], [0.8, 0.6, 0.4], "from the last output, not the input");
        g.skip(&mut io, 1, 3);
        assert!((io[0][0] - 0.2).abs() < 1e-6 && io[0][1] == 0.0 && io[0][2] == 0.0, "{:?}", io[0]);
        let mut io = vec![vec![1.0f32; 3]];
        g.processed(&mut io, 1, 3);
        assert_eq!(io[0], [0.0, 0.25, 0.5]);
        let mut io = vec![vec![1.0f32; 3]];
        g.processed(&mut io, 1, 3);
        assert_eq!(io[0], [0.75, 1.0, 1.0]);
        let mut odd = vec![vec![0.3f32; 2]];
        let mut g = Gate::new();
        g.set_rate(f64::NAN);
        g.skip(&mut odd, 1, 2);
        assert_eq!(odd[0], [0.0, 0.0], "no output before: silence");
    }
}
