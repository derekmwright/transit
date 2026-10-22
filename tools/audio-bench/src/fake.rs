//! The fake mode: `FakeBackend` pumped on a scripted cadence in virtual time (no sleeping, so a
//! long run takes no longer than its callbacks), with injected arrival jitter, one gap and a
//! software loopback with a known delay. It proves the harness logic, not any hardware.

use crate::session::Request;
use soundcraft_audio_backend as ab;
use soundcraft_audio_backend::fake::{FakeBackend, SliceSink, SliceSource};
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq)]
pub struct Script {
    pub seconds: f64,
    /// Peak arrival jitter, in microseconds, either way.
    pub jitter_us: u32,
    /// From this time on every callback arrives one period late: a single arrival gap.
    pub gap_at_s: Option<f64>,
    /// The software loopback delay, in frames.
    pub delay_frames: u32,
}

impl Script {
    pub fn new(seconds: f64) -> Self {
        Self { seconds, jitter_us: 100, gap_at_s: Some(seconds / 2.0), delay_frames: 144 }
    }
}

/// The fake system for `request`: one device with the requested rate and period.
pub fn backend(request: &Request, period: u32) -> FakeBackend {
    let format = ab::Format::new(request.rate, request.out_ch.max(2), ab::SampleFormat::F32);
    let attempt = ab::OpenRequest { format, period: ab::Period::Frames(period), mode: ab::Mode::Shared, origin: ab::HostOrigin::default() };
    let fake = FakeBackend::new(format).with_attempts(vec![attempt]);
    if request.loopback { fake.with_capture(request.in_ch.max(2), false) } else { fake }
}

/// Stateless jitter in `-peak..=peak` nanoseconds for block `k` of one stream.
fn jitter_ns(k: u64, salt: u64, peak: u64) -> i64 {
    let mut x = k.wrapping_mul(2).wrapping_add(salt).wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x % (2 * peak + 1)) as i64 - peak as i64
}

fn stamp(ns: u64) -> ab::Stamp {
    ab::Stamp {
        host_callback_ns: ns,
        backend_callback_ns: ns as i64,
        backend_frame_ns: ns as i64,
        quality: ab::Quality::Arrival,
        offset_uncertainty_ns: None,
    }
}

/// Pump `script.seconds` of blocks of `period` frames through the started streams.
pub fn drive(fake: &FakeBackend, request: &Request, period: u32, script: &Script) -> Result<(), String> {
    let (rate, period_frames) = (u64::from(request.rate), u64::from(period));
    let out_channels = usize::from(request.out_ch.max(2));
    let in_channels = usize::from(request.in_ch.max(2));
    let (out_index, in_index) = (usize::from(request.out_ch) - 1, usize::from(request.in_ch) - 1);
    let blocks = (script.seconds * rate as f64 / period_frames as f64).ceil() as u64;
    let gap_block = script.gap_at_s.map(|s| (s * rate as f64 / period_frames as f64).round() as u64);
    let peak = u64::from(script.jitter_us) * 1000;
    let origin_ns = 1_000_000_000 + peak;
    let at = |k: u64| u64::try_from(u128::from(k) * u128::from(period_frames) * 1_000_000_000 / u128::from(rate)).unwrap_or(u64::MAX);
    let arrival = |k: u64, salt: u64| {
        let late = if gap_block.is_some_and(|g| k >= g) { at(1) } else { 0 };
        (origin_ns + late + at(k)).saturating_add_signed(jitter_ns(k, salt, peak))
    };
    let mut render = vec![0.0f32; period as usize * out_channels];
    let mut capture = vec![0.0f32; period as usize * in_channels];
    let mut line: VecDeque<f32> = VecDeque::with_capacity(script.delay_frames as usize + period as usize + 1);
    line.extend(std::iter::repeat_n(0.0, script.delay_frames as usize));
    let format = ab::Format::new(request.rate, request.in_ch.max(2), ab::SampleFormat::F32);
    for k in 0..blocks {
        let mut sink = SliceSink { data: &mut render, channels: out_channels };
        if !fake.pump_render(period as usize, stamp(arrival(k, 1)), &mut sink) {
            return Err("the fake render stream is not running".into());
        }
        if request.loopback {
            line.extend(render.chunks_exact(out_channels).map(|frame| frame[out_index]));
            for frame in capture.chunks_exact_mut(in_channels) {
                frame[in_index] = line.pop_front().unwrap_or(0.0);
            }
            let mut source = SliceSource { data: &capture, channels: in_channels };
            if !fake.pump_capture(period as usize, stamp(arrival(k, 2)), format, &mut source) {
                return Err("the fake capture stream is not running".into());
            }
        }
    }
    Ok(())
}
