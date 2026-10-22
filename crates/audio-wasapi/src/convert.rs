//! Device-format conversion for the render callback: the `f32` the Player produces goes to the
//! bytes of the endpoint's format. Allocation-free (scratch is lent by the stream thread) and
//! byte-identical to the cpal adapter's conversion for samples in `[-1.0, 1.0)`.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use transit_audio_backend::RenderSink;

/// The sample formats a shared-mode endpoint mixes in: 32-bit float, and PCM of 16, 24 (packed
/// in three bytes) and 32 (container) bits. 24 valid bits in a 32-bit container is `I32`: the
/// sample is left-justified, so the low byte carries only rounding noise the device ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    F32,
    I16,
    I24,
    I32,
}

impl Wire {
    /// Bytes per sample in the device buffer.
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::F32 | Self::I32 => 4,
        }
    }

    /// Convert `from` into `to` (`to.len()` is `from.len() * self.bytes()`).
    fn write(self, from: &[f32], to: &mut [u8]) {
        let width = self.bytes();
        for (x, out) in from.iter().zip(to.chunks_exact_mut(width)) {
            match self {
                Self::F32 => out.copy_from_slice(&x.to_le_bytes()),
                Self::I16 => out.copy_from_slice(&((x * 32_768.0) as i16).to_le_bytes()),
                Self::I24 => {
                    let v = ((x * 8_388_608.0) as i32).clamp(-8_388_608, 8_388_607);
                    out.copy_from_slice(&v.to_le_bytes()[..3]);
                }
                Self::I32 => out.copy_from_slice(&((x * 2_147_483_648.0) as i32).to_le_bytes()),
            }
        }
    }
}

/// The render sink of one device callback: renders chunk by chunk through `scratch` and converts
/// each chunk into `bytes`, the device buffer.
pub(crate) struct ConvertSink<'a> {
    pub(crate) scratch: &'a mut [f32],
    pub(crate) bytes: &'a mut [u8],
    pub(crate) wire: Wire,
}

impl RenderSink for ConvertSink<'_> {
    fn render(&mut self, fill: &mut dyn FnMut(&mut [f32])) {
        let width = self.wire.bytes();
        let chunk = self.scratch.len();
        for out in self.bytes.chunks_mut(chunk * width) {
            let Some(s) = self.scratch.get_mut(..out.len() / width) else { break };
            fill(s);
            self.wire.write(s, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{I24, Sample as _};

    fn input() -> Vec<f32> {
        let mut v: Vec<f32> = (0..2048).map(|i| (i as f32 / 1024.0 - 1.0) * 0.999).collect();
        v.extend([-1.0, 0.0, 0.5, -0.5, 0.999_999, 1e-9, -1e-9, 0.123_456_79]);
        v
    }

    fn through(wire: Wire, x: &[f32]) -> Vec<u8> {
        let mut scratch = vec![0.0f32; 7]; // a scratch smaller than the block forces chunking
        let mut bytes = vec![0u8; x.len() * wire.bytes()];
        let mut at = 0;
        ConvertSink { scratch: &mut scratch, bytes: &mut bytes, wire }.render(&mut |s| {
            s.copy_from_slice(&x[at..at + s.len()]);
            at += s.len();
        });
        assert_eq!(at, x.len());
        bytes
    }

    #[test]
    fn float_is_copied_bit_for_bit() {
        let x = input();
        let want: Vec<u8> = x.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(through(Wire::F32, &x), want);
    }

    #[test]
    fn i16_matches_the_cpal_adapters_conversion() {
        let x = input();
        let want: Vec<u8> = x.iter().flat_map(|v| i16::from_sample(*v).to_le_bytes()).collect();
        assert_eq!(through(Wire::I16, &x), want);
    }

    #[test]
    fn i32_matches_the_cpal_adapters_conversion() {
        let x = input();
        let want: Vec<u8> = x.iter().flat_map(|v| i32::from_sample(*v).to_le_bytes()).collect();
        assert_eq!(through(Wire::I32, &x), want);
    }

    #[test]
    fn i24_is_cpals_i24_value_packed_in_three_bytes() {
        let x = input();
        let want: Vec<u8> = x.iter().flat_map(|v| I24::from_sample(*v).inner().to_le_bytes()[..3].to_vec()).collect();
        assert_eq!(through(Wire::I24, &x), want);
    }

    #[test]
    fn out_of_range_input_saturates_and_never_wraps() {
        assert_eq!(through(Wire::I24, &[1.5, -1.5]), [0xff, 0xff, 0x7f, 0x00, 0x00, 0x80]);
        assert_eq!(through(Wire::I16, &[2.0, -2.0]), [0xff, 0x7f, 0x00, 0x80]);
    }
}
