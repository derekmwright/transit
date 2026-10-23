//! Converter phase in integer units. The denominator stays fixed for one device
//! incarnation, so callback partitions and speed changes cannot round its phase.
use crate::timeline_map::MapError;
#[cfg(not(target_arch = "wasm32"))]
use soundcraft_mix::RenderFrame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Phase {
    units: u64,
    denominator: u64,
}

impl Phase {
    pub(super) fn new(device_rate: u32) -> Result<Self, MapError> {
        if device_rate == 0 {
            return Err(MapError::Invalid);
        }
        Ok(Self { units: 0, denominator: u64::from(device_rate) * 1000 })
    }

    pub(super) fn reset(&mut self) {
        self.units = 0;
    }

    pub(super) fn covers(self, frames: usize) -> bool {
        u128::from(self.units) >= frames as u128 * u128::from(self.denominator)
    }

    pub(super) fn remove(&mut self, frames: usize) -> Result<(), MapError> {
        let units = u64::try_from(frames).ok().and_then(|frames| frames.checked_mul(self.denominator)).ok_or(MapError::Arithmetic)?;
        self.units = self.units.checked_sub(units).ok_or(MapError::Arithmetic)?;
        Ok(())
    }

    pub(super) fn advance(&mut self, step: u64) -> Result<(), MapError> {
        self.units = self.units.checked_add(step).ok_or(MapError::Arithmetic)?;
        Ok(())
    }

    pub(super) fn index(self) -> Result<(usize, f32), MapError> {
        let index = usize::try_from(self.units / self.denominator).map_err(|_| MapError::Arithmetic)?;
        let fraction = (self.units % self.denominator) as f64 / self.denominator as f64;
        Ok((index, fraction as f32))
    }

    pub(super) fn quantum(self, output: usize, step: u64, max: usize) -> usize {
        let frames = (output as u128 * u128::from(step)).div_ceil(u128::from(self.denominator));
        frames.clamp(1, max.max(1) as u128) as usize
    }

    /// Output-device frames from the current output frame to render frame 0 of the next block
    /// (zero or negative: that frame is the current one or already behind it). Render frame `k`
    /// of that block is at `lead + k * denominator / step` output frames.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn lead(self, step: u64) -> f64 {
        if step == 0 { 0.0 } else { -(self.units as f64) / step as f64 }
    }

    /// Output frames per render frame at `step`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn ratio(self, step: u64) -> f64 {
        if step == 0 { 1.0 } else { self.denominator as f64 / step as f64 }
    }

    /// Map an output ordinal through the converter's current consumed phase.
    /// `next_render` is after all rendered pending frames; it is not the output
    /// playhead. Past events retain their past due frame so the mixer counts them
    /// as late, rather than adding another block of latency at dispatch.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn project(
        self,
        next_render: RenderFrame,
        pending: usize,
        current_output: u64,
        output: u64,
        step: u64,
    ) -> Result<RenderFrame, MapError> {
        let pending = u64::try_from(pending).map_err(|_| MapError::Arithmetic)?;
        let origin = next_render.0.checked_sub(pending).ok_or(MapError::Arithmetic)?;
        let delta = i128::from(output) - i128::from(current_output);
        let units = delta.checked_mul(i128::from(step)).and_then(|delta| delta.checked_add(i128::from(self.units))).ok_or(MapError::Arithmetic)?;
        let frames = units.checked_add(i128::from(self.denominator / 2)).ok_or(MapError::Arithmetic)?.div_euclid(i128::from(self.denominator));
        let frame = i128::from(origin).checked_add(frames).ok_or(MapError::Arithmetic)?;
        // Before engine frame zero is necessarily late. Do not wrap it forward.
        Ok(RenderFrame(u64::try_from(frame.max(0)).map_err(|_| MapError::Arithmetic)?))
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn projection_uses_consumed_phase_not_the_render_ahead_boundary() {
        let mut phase = Phase::new(48_000).unwrap();
        phase.advance(44_100 * 1000 * 3).unwrap();
        assert_eq!(phase.index().unwrap().0, 2);
        assert_eq!(phase.project(RenderFrame(512), 512, 3, 480, 44_100 * 1000).unwrap(), RenderFrame(441));
        assert_eq!(phase.project(RenderFrame(512), 512, 3, 0, 44_100 * 1000).unwrap(), RenderFrame(0));
    }

    #[test]
    fn projection_is_independent_of_render_partition_and_retains_large_ordinals() {
        for device in [44_100, 48_000, 96_000] {
            for session in [44_100, 48_000, 96_000] {
                for quantum in [1, 17, 64, 127, 512] {
                    let step = session * 1000;
                    let mut phase = Phase::new(device).unwrap();
                    let origin = 1_u64 << 56;
                    let mut next = origin;
                    let mut pending = 0;
                    for output in 0..4000 {
                        while phase.covers(pending) {
                            phase.remove(pending).unwrap();
                            pending = quantum;
                            next += quantum as u64;
                        }
                        let expected = origin + (4096 * step + u64::from(device) * 500) / (u64::from(device) * 1000);
                        assert_eq!(phase.project(RenderFrame(next), pending, output, 4096, step).unwrap(), RenderFrame(expected));
                        phase.advance(step).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn speed_change_keeps_the_existing_fraction() {
        let mut phase = Phase::new(48_000).unwrap();
        phase.advance(44_100 * 1000).unwrap();
        phase.advance(44_100 * 1250).unwrap();
        assert_eq!(phase.units, 99_225_000);
        assert_eq!(phase.project(RenderFrame(512), 512, 2, 4, 44_100 * 1250).unwrap(), RenderFrame(4));
        phase.reset();
        assert_eq!(phase.index().unwrap(), (0, 0.0));
        assert_eq!(Phase::new(0), Err(MapError::Invalid));
    }
}
