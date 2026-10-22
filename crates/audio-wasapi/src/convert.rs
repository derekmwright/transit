//! Device-format conversion for the render callback: the `f32` the Player produces goes to the
//! bytes of the endpoint's format. Allocation-free (scratch is lent by the stream thread) and
//! byte-identical to the cpal adapter's conversion for samples in `[-1.0, 1.0)`.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use transit_audio_backend::{CaptureSource, RenderSink};

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

    /// Convert device bytes into `f32` (`from.len()` is `to.len() * self.bytes()`), like the cpal
    /// adapter's `f32::from_sample`: the integer value over its full-scale magnitude.
    fn read(self, from: &[u8], to: &mut [f32]) {
        let width = self.bytes();
        for (out, x) in to.iter_mut().zip(from.chunks_exact(width)) {
            *out = match self {
                Self::F32 => f32::from_le_bytes([x[0], x[1], x[2], x[3]]),
                Self::I16 => f32::from(i16::from_le_bytes([x[0], x[1]])) / 32_768.0,
                // Sign-extend the packed 24-bit value through the top byte.
                Self::I24 => (i32::from_le_bytes([0, x[0], x[1], x[2]]) >> 8) as f32 / 8_388_608.0,
                Self::I32 => i32::from_le_bytes([x[0], x[1], x[2], x[3]]) as f32 / 2_147_483_648.0,
            };
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

/// The capture source of one packet: converts the device bytes chunk by chunk through `scratch`,
/// or yields silence for a packet flagged silent (`bytes` is `None`; its buffer content is
/// undefined). `scratch` is a whole number of frames.
pub(crate) struct ConvertSource<'a> {
    pub(crate) scratch: &'a mut [f32],
    pub(crate) channels: usize,
    pub(crate) frames: usize,
    pub(crate) bytes: Option<&'a [u8]>,
    pub(crate) wire: Wire,
}

impl CaptureSource for ConvertSource<'_> {
    fn chunks(&mut self, each: &mut dyn FnMut(usize, &[f32])) {
        let channels = self.channels.max(1);
        let per_chunk = (self.scratch.len() / channels).max(1);
        let width = self.wire.bytes();
        let mut done = 0;
        while done < self.frames {
            let frames = per_chunk.min(self.frames - done);
            let Some(out) = self.scratch.get_mut(..frames * channels) else { return };
            match self.bytes {
                Some(bytes) => {
                    let Some(from) = bytes.get(done * channels * width..(done + frames) * channels * width) else { return };
                    self.wire.read(from, out);
                }
                None => out.fill(0.0),
            }
            each(done, out);
            done += frames;
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

    /// The `width`-byte samples of `bytes`.
    fn words(bytes: &[u8], width: usize) -> impl Iterator<Item = &[u8]> {
        bytes.chunks_exact(width)
    }

    fn bytes_of(wire: Wire, x: &[f32]) -> Vec<u8> {
        through(wire, x)
    }

    fn read_through(wire: Wire, bytes: &[u8], channels: usize, scratch_frames: usize) -> Vec<f32> {
        let mut scratch = vec![0.0f32; scratch_frames * channels];
        let frames = bytes.len() / (wire.bytes() * channels);
        let mut got = vec![f32::NAN; frames * channels];
        ConvertSource { scratch: &mut scratch, channels, frames, bytes: Some(bytes), wire }.chunks(&mut |offset, s| {
            got[offset * channels..offset * channels + s.len()].copy_from_slice(s);
        });
        got
    }

    #[test]
    fn capture_conversion_matches_the_cpal_adapters_for_every_wire_format() {
        let x = input();
        // Stereo frames, a scratch of three frames: chunks and their offsets cross every boundary.
        for wire in [Wire::F32, Wire::I16, Wire::I24, Wire::I32] {
            let bytes = bytes_of(wire, &x);
            let got = read_through(wire, &bytes, 2, 3);
            let want: Vec<f32> = match wire {
                Wire::F32 => x.clone(),
                Wire::I16 => words(&bytes, 2).map(|b| f32::from_sample(i16::from_le_bytes([b[0], b[1]]))).collect(),
                Wire::I32 => words(&bytes, 4).map(|b| f32::from_sample(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))).collect(),
                Wire::I24 => {
                    words(&bytes, 3).map(|b| f32::from_sample(I24::new((i32::from_le_bytes([0, b[0], b[1], b[2]])) >> 8).unwrap())).collect()
                }
            };
            assert_eq!(got.len(), want.len());
            assert!(got.iter().zip(&want).all(|(a, b)| a.to_bits() == b.to_bits()), "{wire:?} differs from cpal");
        }
    }

    #[test]
    fn capture_round_trips_the_render_conversion_within_one_lsb() {
        let x = input();
        for (wire, lsb) in [(Wire::I16, 1.0f32 / 32_768.0), (Wire::I24, 1.0 / 8_388_608.0), (Wire::I32, 1.0 / 2_147_483_648.0)] {
            let got = read_through(wire, &bytes_of(wire, &x), 2, 5);
            assert!(got.iter().zip(&x).all(|(a, b)| (a - b).abs() <= lsb.max(f32::EPSILON)), "{wire:?}");
        }
    }

    #[test]
    fn a_silent_packet_yields_zeros_and_a_short_scratch_still_covers_every_frame() {
        let mut scratch = vec![1.0f32; 6];
        let mut seen = Vec::new();
        ConvertSource { scratch: &mut scratch, channels: 2, frames: 7, bytes: None, wire: Wire::I16 }
            .chunks(&mut |offset, s| seen.push((offset, s.len(), s.iter().all(|v| *v == 0.0))));
        assert_eq!(seen, [(0, 6, true), (3, 6, true), (6, 2, true)]);
    }
}
