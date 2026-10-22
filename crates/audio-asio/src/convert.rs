//! Sample conversion between the driver's planar per-channel buffers and the trait's interleaved
//! `f32` chunks. Allocation-free: the scratch is lent by the callback state, which allocated it at
//! open. Runs on the driver's callback thread.
//!
//! The integer scaling is the one `transit-audio-wasapi`'s `convert.rs` (and through it cpal's
//! `Sample` conversions) uses: the integer over its full-scale magnitude when reading, the float
//! times that magnitude truncated toward zero and clamped when writing. That code is crate-private
//! and works on interleaved bytes, so it is mirrored here per sample rather than shared; the tests
//! below pin the same values. ASIO adds `Float64LSB` and the 32-bit containers whose 16, 18, 20 or
//! 24 valid bits sit at the **least** significant end (sign-extended), unlike WASAPI's
//! left-justified 24-in-32.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use crate::driver::SampleType;
use transit_audio_backend::{CaptureSource, RenderSink};

/// The sample encodings the callback converts, with their byte width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    I16,
    /// Packed 24-bit, three bytes per sample.
    I24,
    I32,
    F32,
    F64,
    /// A 32-bit container with this many valid bits at the least significant end.
    I32Low(u8),
}

impl Wire {
    /// The wire of a supported sample type.
    pub(crate) fn of(sample: SampleType) -> Option<Self> {
        Some(match sample {
            SampleType::Int16Lsb => Self::I16,
            SampleType::Int24Lsb => Self::I24,
            SampleType::Int32Lsb => Self::I32,
            SampleType::Float32Lsb => Self::F32,
            SampleType::Float64Lsb => Self::F64,
            SampleType::Int32LsbAligned(bits @ (16 | 18 | 20 | 24)) => Self::I32Low(bits),
            SampleType::Int32LsbAligned(_) | SampleType::Unsupported(_) => return None,
        })
    }

    /// Bytes per sample in the driver buffer.
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 | Self::I32Low(_) => 4,
            Self::F64 => 8,
        }
    }

    /// One sample of `x.len() == self.bytes()` bytes as `f32`.
    #[inline]
    pub(crate) fn read(self, x: &[u8]) -> f32 {
        match self {
            Self::I16 => f32::from(i16::from_le_bytes([x[0], x[1]])) / 32_768.0,
            // Sign-extend the packed 24-bit value through the top byte.
            Self::I24 => (i32::from_le_bytes([0, x[0], x[1], x[2]]) >> 8) as f32 / 8_388_608.0,
            Self::I32 => i32::from_le_bytes([x[0], x[1], x[2], x[3]]) as f32 / 2_147_483_648.0,
            Self::F32 => f32::from_le_bytes([x[0], x[1], x[2], x[3]]),
            Self::F64 => f64::from_le_bytes([x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7]]) as f32,
            Self::I32Low(bits) => {
                // Sign-extend from the valid bits, whatever the driver left above them.
                let shift = 32 - u32::from(bits);
                let v = (i32::from_le_bytes([x[0], x[1], x[2], x[3]]) << shift) >> shift;
                v as f32 / (1u32 << (bits - 1)) as f32
            }
        }
    }

    /// One `f32` into `out` (`out.len() == self.bytes()`).
    #[inline]
    pub(crate) fn write(self, x: f32, out: &mut [u8]) {
        match self {
            Self::I16 => out.copy_from_slice(&((x * 32_768.0) as i16).to_le_bytes()),
            Self::I24 => {
                let v = ((x * 8_388_608.0) as i32).clamp(-8_388_608, 8_388_607);
                out.copy_from_slice(&v.to_le_bytes()[..3]);
            }
            Self::I32 => out.copy_from_slice(&((x * 2_147_483_648.0) as i32).to_le_bytes()),
            Self::F32 => out.copy_from_slice(&x.to_le_bytes()),
            Self::F64 => out.copy_from_slice(&f64::from(x).to_le_bytes()),
            Self::I32Low(bits) => {
                let full = 1i64 << (bits - 1);
                let v = ((f64::from(x) * full as f64) as i64).clamp(-full, full - 1) as i32;
                out.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}

/// The output channel buffers of one buffer half, one plane per channel.
pub(crate) trait OutputPlanes {
    fn count(&self) -> usize;
    /// Channel `channel`'s bytes (`frames * wire.bytes()`); empty past the count.
    fn plane(&mut self, channel: usize) -> &mut [u8];
}

/// The input channel buffers of one buffer half, one plane per channel.
pub(crate) trait InputPlanes {
    fn count(&self) -> usize;
    /// Channel `channel`'s bytes (`frames * wire.bytes()`); empty past the count.
    fn plane(&self, channel: usize) -> &[u8];
}

/// The render sink of one buffer switch: the callback fills interleaved `f32` chunks in `scratch`
/// (a whole number of frames), each converted into the planar channel buffers at its offset.
pub(crate) struct PlanarSink<'a> {
    pub(crate) scratch: &'a mut [f32],
    pub(crate) planes: &'a mut dyn OutputPlanes,
    pub(crate) frames: usize,
    pub(crate) wire: Wire,
}

impl RenderSink for PlanarSink<'_> {
    fn render(&mut self, fill: &mut dyn FnMut(&mut [f32])) {
        let count = self.planes.count().max(1);
        let per_chunk = (self.scratch.len() / count).max(1);
        let width = self.wire.bytes();
        let mut done = 0;
        while done < self.frames {
            let frames = per_chunk.min(self.frames - done);
            let Some(chunk) = self.scratch.get_mut(..frames * count) else { return };
            fill(chunk);
            for c in 0..self.planes.count() {
                let Some(out) = self.planes.plane(c).get_mut(done * width..(done + frames) * width) else { continue };
                for (frame, bytes) in out.chunks_exact_mut(width).enumerate() {
                    let x = chunk.get(frame * count + c).copied().unwrap_or(0.0);
                    self.wire.write(x, bytes);
                }
            }
            done += frames;
        }
    }
}

/// Zeroes every plane: the output of a direction that is not running plays silence.
pub(crate) fn silence(planes: &mut dyn OutputPlanes) {
    for c in 0..planes.count() {
        planes.plane(c).fill(0);
    }
}

/// The capture source of one buffer switch: the planar channel buffers interleaved into `f32`
/// chunks of `scratch` (a whole number of frames).
pub(crate) struct PlanarSource<'a> {
    pub(crate) scratch: &'a mut [f32],
    pub(crate) planes: &'a dyn InputPlanes,
    pub(crate) frames: usize,
    pub(crate) wire: Wire,
}

impl CaptureSource for PlanarSource<'_> {
    fn chunks(&mut self, each: &mut dyn FnMut(usize, &[f32])) {
        let count = self.planes.count().max(1);
        let per_chunk = (self.scratch.len() / count).max(1);
        let width = self.wire.bytes();
        let mut done = 0;
        while done < self.frames {
            let frames = per_chunk.min(self.frames - done);
            let Some(chunk) = self.scratch.get_mut(..frames * count) else { return };
            for c in 0..self.planes.count() {
                let Some(from) = self.planes.plane(c).get(done * width..(done + frames) * width) else { continue };
                for (frame, bytes) in from.chunks_exact(width).enumerate() {
                    if let Some(slot) = chunk.get_mut(frame * count + c) {
                        *slot = self.wire.read(bytes);
                    }
                }
            }
            each(done, chunk);
            done += frames;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Wire; 9] =
        [Wire::I16, Wire::I24, Wire::I32, Wire::F32, Wire::F64, Wire::I32Low(16), Wire::I32Low(18), Wire::I32Low(20), Wire::I32Low(24)];

    fn input() -> Vec<f32> {
        let mut v: Vec<f32> = (0..512).map(|i| (i as f32 / 256.0 - 1.0) * 0.999).collect();
        v.extend([-1.0, 0.0, 0.5, -0.5, 0.999_999, 1e-9, -1e-9, 0.123_456_79]);
        v
    }

    /// Planes in plain vectors.
    pub(crate) struct VecPlanes(pub(crate) Vec<Vec<u8>>);

    impl OutputPlanes for VecPlanes {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn plane(&mut self, channel: usize) -> &mut [u8] {
            self.0.get_mut(channel).map_or(&mut [], Vec::as_mut_slice)
        }
    }

    impl InputPlanes for VecPlanes {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn plane(&self, channel: usize) -> &[u8] {
            self.0.get(channel).map_or(&[], Vec::as_slice)
        }
    }

    /// Renders `x` (interleaved, `channels` wide) through a scratch of `scratch_frames` frames into
    /// planar buffers.
    fn render(wire: Wire, x: &[f32], channels: usize, scratch_frames: usize) -> Vec<Vec<u8>> {
        let frames = x.len() / channels;
        let mut planes = VecPlanes((0..channels).map(|_| vec![0xAA; frames * wire.bytes()]).collect());
        let mut scratch = vec![0.0f32; scratch_frames * channels];
        let mut at = 0;
        PlanarSink { scratch: &mut scratch, planes: &mut planes, frames, wire }.render(&mut |chunk| {
            chunk.copy_from_slice(&x[at..at + chunk.len()]);
            at += chunk.len();
        });
        assert_eq!(at, x.len());
        planes.0
    }

    fn capture(wire: Wire, planes: &[Vec<u8>], scratch_frames: usize) -> Vec<f32> {
        let channels = planes.len();
        let frames = planes[0].len() / wire.bytes();
        let planes = VecPlanes(planes.to_vec());
        let mut scratch = vec![0.0f32; scratch_frames * channels];
        let mut got = vec![f32::NAN; frames * channels];
        PlanarSource { scratch: &mut scratch, planes: &planes, frames, wire }.chunks(&mut |offset, s| {
            got[offset * channels..offset * channels + s.len()].copy_from_slice(s);
        });
        got
    }

    /// The integers of a plane (sign-extended to i64).
    fn ints(wire: Wire, plane: &[u8]) -> Vec<i64> {
        plane
            .chunks_exact(wire.bytes())
            .map(|b| match wire {
                Wire::I16 => i64::from(i16::from_le_bytes([b[0], b[1]])),
                Wire::I24 => i64::from(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8),
                _ => i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            })
            .collect()
    }

    #[test]
    fn every_supported_sample_type_has_a_wire_and_the_others_do_not() {
        let cases = [
            (SampleType::Int16Lsb, Some(Wire::I16)),
            (SampleType::Int24Lsb, Some(Wire::I24)),
            (SampleType::Int32Lsb, Some(Wire::I32)),
            (SampleType::Float32Lsb, Some(Wire::F32)),
            (SampleType::Float64Lsb, Some(Wire::F64)),
            (SampleType::Int32LsbAligned(16), Some(Wire::I32Low(16))),
            (SampleType::Int32LsbAligned(24), Some(Wire::I32Low(24))),
            (SampleType::Int32LsbAligned(12), None),
            (SampleType::Unsupported(crate::abi::ASIOST_INT32_MSB), None),
        ];
        for (sample, wire) in cases {
            assert_eq!(Wire::of(sample), wire, "{sample:?}");
        }
    }

    #[test]
    fn known_values_write_and_read_exactly_in_every_wire() {
        let x = [0.5f32, -0.5, 0.25, -1.0, 0.0];
        let cases: [(Wire, [i64; 5]); 7] = [
            (Wire::I16, [16_384, -16_384, 8_192, -32_768, 0]),
            (Wire::I24, [4_194_304, -4_194_304, 2_097_152, -8_388_608, 0]),
            (Wire::I32, [1 << 30, -(1 << 30), 1 << 29, -(1 << 31), 0]),
            (Wire::I32Low(16), [16_384, -16_384, 8_192, -32_768, 0]),
            (Wire::I32Low(18), [65_536, -65_536, 32_768, -131_072, 0]),
            (Wire::I32Low(20), [262_144, -262_144, 131_072, -524_288, 0]),
            (Wire::I32Low(24), [4_194_304, -4_194_304, 2_097_152, -8_388_608, 0]),
        ];
        for (wire, want) in cases {
            let planes = render(wire, &x, 1, 2);
            assert_eq!(ints(wire, &planes[0]), want, "{wire:?} write");
            assert_eq!(capture(wire, &planes, 2), x, "{wire:?} read back");
        }
        for wire in [Wire::F32, Wire::F64] {
            assert_eq!(capture(wire, &render(wire, &x, 1, 3), 3), x, "{wire:?}");
        }
        let f64_plane = render(Wire::F64, &[0.1], 1, 1);
        assert_eq!(f64_plane[0], f64::from(0.1f32).to_le_bytes(), "Float64LSB is the f32 widened");
    }

    #[test]
    fn the_integer_scaling_is_the_wasapi_and_cpal_one() {
        // The same expressions `transit-audio-wasapi`'s `convert.rs` pins against cpal.
        let x = input();
        for (wire, scale) in [(Wire::I16, 32_768.0f32), (Wire::I24, 8_388_608.0), (Wire::I32, 2_147_483_648.0)] {
            let planes = render(wire, &x, 1, 7);
            let want: Vec<i64> = x
                .iter()
                .map(|v| match wire {
                    Wire::I16 => i64::from((v * scale) as i16),
                    Wire::I24 => i64::from(((v * scale) as i32).clamp(-8_388_608, 8_388_607)),
                    _ => i64::from((v * scale) as i32),
                })
                .collect();
            assert_eq!(ints(wire, &planes[0]), want, "{wire:?}");
        }
    }

    #[test]
    fn low_aligned_containers_sign_extend_and_ignore_the_bits_above() {
        // A driver may leave garbage above the valid bits: 0x7F00_0001 in a 24-bit container is +1.
        let plane = [0x01u8, 0x00, 0x00, 0x7F, 0xFF, 0xFF, 0xFF, 0x00];
        let got = capture(Wire::I32Low(24), &[plane.to_vec()], 4);
        assert_eq!(got, [1.0 / 8_388_608.0, -1.0 / 8_388_608.0]);
        // Written values are sign-extended across the whole container.
        assert_eq!(ints(Wire::I32Low(20), &render(Wire::I32Low(20), &[-1.0 / 524_288.0], 1, 1)[0]), [-1]);
    }

    #[test]
    fn out_of_range_input_saturates_in_every_integer_wire() {
        let x = [1.0f32, 1.5, -1.0, -1.5, f32::INFINITY, f32::NEG_INFINITY];
        for (wire, max) in [
            (Wire::I16, 32_767i64),
            (Wire::I24, 8_388_607),
            (Wire::I32, i64::from(i32::MAX)),
            (Wire::I32Low(16), 32_767),
            (Wire::I32Low(24), 8_388_607),
        ] {
            let min = -max - 1;
            assert_eq!(ints(wire, &render(wire, &x, 1, 2)[0]), [max, max, min, min, max, min], "{wire:?}");
        }
    }

    #[test]
    fn planar_buffers_interleave_both_ways_through_a_chunked_scratch() {
        // Three channels, 11 frames, a 4-frame scratch: chunks and offsets cross every boundary.
        let x: Vec<f32> = (0..33).map(|i| (i as f32 - 16.0) / 32.0).collect();
        for wire in ALL {
            let planes = render(wire, &x, 3, 4);
            assert_eq!(planes.len(), 3);
            assert_eq!(capture(wire, &planes, 4), x, "{wire:?}");
            // Channel 1 holds samples 1, 4, 7, ... of the interleaved block.
            let one = capture(wire, &planes[1..2], 5);
            assert_eq!(one, x.iter().skip(1).step_by(3).copied().collect::<Vec<_>>(), "{wire:?}");
        }
    }

    #[test]
    fn round_trips_within_one_lsb_for_every_wire() {
        let x = input();
        for wire in ALL {
            let lsb = match wire {
                Wire::I16 => 1.0 / 32_768.0,
                Wire::I24 => 1.0 / 8_388_608.0,
                Wire::I32Low(bits) => 1.0 / (1u32 << (bits - 1)) as f32,
                _ => f32::EPSILON,
            };
            let got = capture(wire, &render(wire, &x, 2, 5), 3);
            assert!(got.iter().zip(&x).all(|(a, b)| (a - b).abs() <= lsb.max(f32::EPSILON)), "{wire:?}");
        }
    }

    #[test]
    fn silence_is_zero_bytes_both_ways() {
        for wire in ALL {
            let planes = render(wire, &[0.0; 8], 2, 3);
            assert!(planes.iter().flatten().all(|b| *b == 0), "{wire:?}");
            assert!(capture(wire, &planes, 3).iter().all(|v| *v == 0.0), "{wire:?}");
        }
        let mut idle = VecPlanes(vec![vec![0x55; 12], vec![0x66; 12]]);
        silence(&mut idle);
        assert!(idle.0.iter().flatten().all(|b| *b == 0));
    }
}
