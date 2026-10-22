//! The callbacks of the harness: they only stamp and count. Every buffer is allocated before a
//! stream starts, logs are fixed-size and single-writer, and nothing locks or allocates.

use std::f32::consts::TAU;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use transit_audio_backend as ab;

/// Arrival value of a callback whose stamp was invalid.
pub const INVALID_ARRIVAL: u64 = u64::MAX;
/// Capacity of the detector ring: a power of two above the longest click (rate / 1000 + 1).
pub const DETECTOR_CAP: usize = 512;
/// The first click, in milliseconds after the first render callback.
pub const START_DELAY_MS: u64 = 250;
/// Clicks repeat at this interval.
pub const INTERVAL_MS: u64 = 500;
/// Peak level of the click, linear.
pub const CLICK_LEVEL: f32 = 0.5;
/// Frequency of the burst.
pub const BURST_HZ: f32 = 3000.0;
/// Normalized correlation needed to call a window a click.
pub const MIN_CORRELATION: f32 = 0.7;
/// The window energy must be at least this fraction of the template's (amplitude 0.2 = -14 dB).
pub const MIN_ENERGY_RATIO: f32 = 0.04;

/// A fixed-size, single-writer log of `u64`. Pushing past the capacity counts a drop.
pub struct Log {
    slots: Box<[AtomicU64]>,
    len: AtomicUsize,
    dropped: AtomicU64,
}

impl Log {
    pub fn new(capacity: usize) -> Self {
        Self { slots: (0..capacity).map(|_| AtomicU64::new(0)).collect(), len: AtomicUsize::new(0), dropped: AtomicU64::new(0) }
    }
    /// Append one value (callback side: no allocation, no lock).
    pub fn push(&self, value: u64) {
        let at = self.len.load(Ordering::Relaxed);
        if let Some(slot) = self.slots.get(at) {
            slot.store(value, Ordering::Relaxed);
            self.len.store(at + 1, Ordering::Release);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// The values so far (control side).
    pub fn snapshot(&self) -> Vec<u64> {
        let len = self.len.load(Ordering::Acquire);
        self.slots[..len].iter().map(|s| s.load(Ordering::Relaxed)).collect()
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// What every callback of one stream records, in step: arrival, frames and the time spent in it.
pub struct CallbackLog {
    pub arrival_ns: Log,
    /// `backend_frame_ns` of each block, an `i64` stored as its bits: the time of its first frame in the
    /// backend's clock (presentation for render, capture for input).
    pub frame_ns: Log,
    pub frames: Log,
    pub cpu_ns: Log,
    pub errors: AtomicU64,
    pub invalid: AtomicU64,
}

impl CallbackLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            arrival_ns: Log::new(capacity),
            frame_ns: Log::new(capacity),
            frames: Log::new(capacity),
            cpu_ns: Log::new(capacity),
            errors: AtomicU64::new(0),
            invalid: AtomicU64::new(0),
        }
    }
    fn record(&self, stamp: &ab::Stamp, frames: usize, cpu_ns: u64) {
        let valid = stamp.quality != ab::Quality::Invalid;
        if !valid {
            self.invalid.fetch_add(1, Ordering::Relaxed);
        }
        self.arrival_ns.push(if valid { stamp.host_callback_ns } else { INVALID_ARRIVAL });
        self.frame_ns.push(stamp.backend_frame_ns as u64);
        self.frames.push(frames as u64);
        self.cpu_ns.push(cpu_ns);
    }
    /// The error callback of the stream.
    pub fn error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// The click as emitted and as matched: one impulse, then a 1 ms raised-cosine tone burst
/// (a tone, so an AC-coupled input still passes it). The timeline is in frames of its stream.
pub struct ClickTemplate {
    pub rate: u32,
    pub samples: Box<[f32]>,
    pub energy: f32,
}

impl ClickTemplate {
    pub fn new(rate: u32) -> Result<Self, String> {
        let burst = usize::try_from(rate / 1000).unwrap_or(0).max(8);
        if burst + 1 > DETECTOR_CAP {
            return Err(format!("rate {rate} Hz is too high for the click detector"));
        }
        let mut samples = vec![0.0f32; burst + 1];
        samples[0] = CLICK_LEVEL;
        for n in 0..burst {
            let phase = (n as f32 + 0.5) / burst as f32;
            let window = 0.5 * (1.0 - (TAU * phase).cos());
            samples[n + 1] = CLICK_LEVEL * window * (TAU * BURST_HZ * n as f32 / rate as f32).sin();
        }
        let energy = samples.iter().map(|s| s * s).sum();
        Ok(Self { rate, samples: samples.into_boxed_slice(), energy })
    }
    /// The first frame of click `k` on this stream's timeline.
    pub fn start(&self, k: u64) -> u64 {
        click_start(self.rate, k)
    }
    fn interval(&self) -> u64 {
        click_interval(self.rate)
    }
}

pub fn click_start(rate: u32, k: u64) -> u64 {
    u64::from(rate) * START_DELAY_MS / 1000 + k * click_interval(rate)
}

pub fn click_interval(rate: u32) -> u64 {
    u64::from(rate) * INTERVAL_MS / 1000
}

/// Everything the callbacks and the control thread share.
pub struct Shared {
    pub render: CallbackLog,
    pub capture: CallbackLog,
    /// Frame positions (on the capture stream) of the detected clicks.
    pub detections: Log,
    /// Set before the streams start, once the achieved rates are known.
    pub render_click: OnceLock<ClickTemplate>,
    pub capture_click: OnceLock<ClickTemplate>,
}

impl Shared {
    /// Logs sized for `seconds` of callbacks of at least 16 frames at `rate`, plus slack.
    pub fn new(rate: u32, seconds: f64) -> Arc<Self> {
        let callbacks = (seconds.max(0.0) * f64::from(rate) / 16.0).ceil() as usize + 4096;
        let clicks = (seconds.max(0.0) * 1000.0 / INTERVAL_MS as f64).ceil() as usize + 64;
        Arc::new(Self {
            render: CallbackLog::new(callbacks),
            capture: CallbackLog::new(callbacks),
            detections: Log::new(clicks),
            render_click: OnceLock::new(),
            capture_click: OnceLock::new(),
        })
    }
}

fn write_click(chunk: &mut [f32], channels: usize, channel: usize, base: u64, next: &mut u64, click: &ClickTemplate) {
    if channel >= channels {
        return;
    }
    let length = click.samples.len() as u64;
    for (i, frame) in chunk.chunks_exact_mut(channels).enumerate() {
        let at = base + i as u64;
        while at >= *next + length {
            *next += click.interval();
        }
        if at >= *next {
            frame[channel] = click.samples[(at - *next) as usize];
        }
    }
}

fn nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The render callback: silence, and in loopback mode the click train on `channel` (0-based).
pub fn render_callback(shared: Arc<Shared>, channel: usize, loopback: bool) -> ab::RenderCallback {
    let (mut position, mut next) = (0u64, u64::MAX);
    Box::new(move |block| {
        let started = Instant::now();
        let (stamp, frames, channels) = (block.stamp, block.frames, usize::from(block.channels).max(1));
        let click = if loopback { shared.render_click.get() } else { None };
        if let (Some(click), true) = (click, next == u64::MAX) {
            next = click.start(0);
        }
        let base = position;
        let mut done = 0usize;
        block.render(&mut |chunk| {
            chunk.fill(0.0);
            if let Some(click) = click {
                write_click(chunk, channels, channel, base + done as u64, &mut next, click);
            }
            done += chunk.len() / channels;
        });
        position += frames as u64;
        shared.render.record(&stamp, frames, nanos(started));
    })
}

/// A matched filter over the last click-length of samples of one channel.
struct Detector {
    ring: Box<[f32]>,
    count: usize,
    armed: bool,
    best: f32,
    best_at: u64,
}

impl Detector {
    fn new() -> Self {
        Self { ring: vec![0.0; DETECTOR_CAP].into_boxed_slice(), count: 0, armed: false, best: 0.0, best_at: 0 }
    }
    /// `at` is the stream frame of `sample`. A click is reported (its first frame) once the
    /// correlation falls back below the threshold after its peak.
    fn feed(&mut self, click: &ClickTemplate, sample: f32, at: u64, found: &Log) {
        let length = click.samples.len();
        self.ring[self.count & (DETECTOR_CAP - 1)] = sample;
        self.count += 1;
        if self.count < length {
            return;
        }
        let (mut dot, mut energy) = (0.0f32, 0.0f32);
        for (j, want) in click.samples.iter().enumerate() {
            let have = self.ring[(self.count - length + j) & (DETECTOR_CAP - 1)];
            dot += have * want;
            energy += have * have;
        }
        let correlation = if energy > 0.0 { dot / (energy * click.energy).sqrt() } else { 0.0 };
        if energy >= MIN_ENERGY_RATIO * click.energy && correlation >= MIN_CORRELATION {
            if correlation > self.best {
                (self.best, self.best_at) = (correlation, at);
            }
            self.armed = true;
        } else if self.armed {
            found.push(self.best_at + 1 - length as u64);
            (self.armed, self.best) = (false, 0.0);
        }
    }
}

/// The capture callback: logs arrivals and scans `channel` (0-based) for the click.
pub fn capture_callback(shared: Arc<Shared>, channel: usize) -> ab::CaptureCallback {
    let (mut position, mut detector) = (0u64, Detector::new());
    Box::new(move |block| {
        let started = Instant::now();
        let (stamp, frames, channels) = (block.stamp, block.frames, usize::from(block.channels).max(1));
        let click = shared.capture_click.get();
        let base = position;
        block.chunks(&mut |offset, samples| {
            let Some(click) = click else { return };
            for (i, frame) in samples.chunks_exact(channels).enumerate() {
                let at = base + (offset + i) as u64;
                detector.feed(click, frame.get(channel).copied().unwrap_or(0.0), at, &shared.detections);
            }
        });
        position += frames as u64;
        shared.capture.record(&stamp, frames, nanos(started));
    })
}
