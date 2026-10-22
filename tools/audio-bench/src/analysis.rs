//! From the raw logs to the numbers of a row. Runs on the control thread after the streams stop.

use crate::probe::{CallbackLog, INVALID_ARRIVAL, Shared, click_start};
use std::sync::atomic::Ordering;

/// Callbacks at the start of a stream that are left out of the jitter figures: drivers deliver
/// the first few back to back while they fill their buffers.
pub const WARMUP_CALLBACKS: usize = 8;
/// An arrival interval longer than this many periods counts as an xrun.
pub const XRUN_PERIODS: f64 = 1.5;
/// The longest round trip the pairing of clicks and detections accepts.
pub const MAX_ROUND_TRIP_MS: f64 = 450.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundTrip {
    pub clicks: usize,
    pub paired: usize,
    pub min_ms: f64,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub min_frames: f64,
    pub median_frames: f64,
    pub p95_frames: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Measurements {
    pub rate: u32,
    pub callbacks: usize,
    /// Median block size in frames, and the extremes.
    pub achieved_frames: u32,
    pub min_frames: u32,
    pub max_frames: u32,
    pub achieved_ms: f64,
    /// Deviation of the render arrival intervals from the achieved period, in ms.
    pub jitter_ms: Percentiles,
    pub duration_s: f64,
    pub xruns_out: u64,
    /// Capture arrival gaps, when a capture stream ran.
    pub xruns_in: Option<u64>,
    /// Callback body time of the render stream, in microseconds.
    pub cpu_us: Percentiles,
    pub round_trip: Option<RoundTrip>,
    /// Calls of the backends' error callbacks (render plus capture).
    pub backend_errors: u64,
    /// Callbacks with an invalid stamp, and log entries lost to a full log.
    pub invalid_stamps: u64,
    pub dropped: u64,
}

impl Measurements {
    pub fn xruns_per_minute(&self) -> f64 {
        if self.duration_s > 0.0 { self.xruns_out as f64 * 60.0 / self.duration_s } else { 0.0 }
    }
}

/// Nearest-rank percentile of a sorted slice (`p` in 0..=1); 0 when empty.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn sorted(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(f64::total_cmp);
    values
}

fn summary(values: Vec<f64>) -> Percentiles {
    let values = sorted(values);
    Percentiles { p50: percentile(&values, 0.5), p95: percentile(&values, 0.95), max: values.last().copied().unwrap_or(0.0) }
}

/// The callbacks of one stream, read back.
pub struct Stream {
    pub arrival_ns: Vec<u64>,
    pub frames: Vec<u64>,
    pub cpu_ns: Vec<u64>,
    pub rate: u32,
}

impl Stream {
    pub fn read(log: &CallbackLog, rate: u32) -> Self {
        Self { arrival_ns: log.arrival_ns.snapshot(), frames: log.frames.snapshot(), cpu_ns: log.cpu_ns.snapshot(), rate }
    }
    fn median_frames(&self) -> u32 {
        let frames = sorted(self.frames.iter().map(|f| *f as f64).collect());
        percentile(&frames, 0.5) as u32
    }
    /// Nominal period in ns from the median block.
    fn period_ns(&self) -> f64 {
        f64::from(self.median_frames()) * 1e9 / f64::from(self.rate.max(1))
    }
    /// Arrival intervals in ns after the warm-up, with both ends valid.
    fn intervals(&self) -> Vec<f64> {
        (WARMUP_CALLBACKS.max(1)..self.arrival_ns.len())
            .filter(|&i| self.arrival_ns[i] != INVALID_ARRIVAL && self.arrival_ns[i - 1] != INVALID_ARRIVAL)
            .map(|i| self.arrival_ns[i] as f64 - self.arrival_ns[i - 1] as f64)
            .collect()
    }
    fn gaps(&self) -> u64 {
        let limit = self.period_ns() * XRUN_PERIODS;
        self.intervals().into_iter().filter(|interval| *interval > limit).count() as u64
    }
    fn duration_s(&self) -> f64 {
        let mut valid = self.arrival_ns.iter().filter(|a| **a != INVALID_ARRIVAL);
        match (valid.next(), valid.next_back()) {
            (Some(first), Some(last)) => (*last - *first) as f64 / 1e9,
            _ => 0.0,
        }
    }
    /// Host time in ns of stream frame `at`: the arrival of its block plus the offset in it.
    fn host_ns(&self, starts: &[u64], at: u64) -> Option<f64> {
        let block = starts.partition_point(|s| *s <= at).checked_sub(1)?;
        let arrival = *self.arrival_ns.get(block)?;
        (arrival != INVALID_ARRIVAL && at < starts[block] + self.frames[block])
            .then(|| arrival as f64 + (at - starts[block]) as f64 * 1e9 / f64::from(self.rate))
    }
    /// First stream frame of each block.
    fn starts(&self) -> Vec<u64> {
        self.frames
            .iter()
            .scan(0u64, |sum, f| {
                let start = *sum;
                *sum += f;
                Some(start)
            })
            .collect()
    }
}

/// Round trips: click `k` leaves at (render host time of its first frame); the detector finds
/// it at (capture host time of its first frame). Each detection pairs with the latest click that
/// left before it. The bias is the callback dispatch jitter of both streams (not removed).
pub fn round_trips(render: &Stream, capture: &Stream, detections: &[u64]) -> RoundTrip {
    let (render_starts, capture_starts) = (render.starts(), capture.starts());
    let total: u64 = render.frames.iter().sum();
    let emitted: Vec<f64> = (0..)
        .map(|k| click_start(render.rate, k))
        .take_while(|start| *start < total)
        .filter_map(|start| render.host_ns(&render_starts, start))
        .collect();
    let mut seen = vec![false; emitted.len()];
    let mut trips = Vec::new();
    for at in detections {
        let Some(arrived) = capture.host_ns(&capture_starts, *at) else { continue };
        let Some(k) = emitted.partition_point(|sent| *sent <= arrived).checked_sub(1) else { continue };
        let trip_ms = (arrived - emitted[k]) / 1e6;
        if !seen[k] && (0.0..=MAX_ROUND_TRIP_MS).contains(&trip_ms) {
            seen[k] = true;
            trips.push(trip_ms);
        }
    }
    let trips = sorted(trips);
    let frames = |ms: f64| ms * f64::from(render.rate) / 1000.0;
    let (min, median, p95) = (trips.first().copied().unwrap_or(0.0), percentile(&trips, 0.5), percentile(&trips, 0.95));
    RoundTrip {
        clicks: emitted.len(),
        paired: trips.len(),
        min_ms: min,
        median_ms: median,
        p95_ms: p95,
        min_frames: frames(min),
        median_frames: frames(median),
        p95_frames: frames(p95),
    }
}

/// All figures of a run. `capture_rate` is `Some` when a capture stream ran.
pub fn measure(shared: &Shared, render_rate: u32, capture_rate: Option<u32>) -> Measurements {
    let render = Stream::read(&shared.render, render_rate);
    let capture = capture_rate.map(|rate| Stream::read(&shared.capture, rate));
    let period_ns = render.period_ns();
    let jitter = summary(render.intervals().into_iter().map(|interval| (interval - period_ns).abs() / 1e6).collect());
    let cpu = summary(render.cpu_ns.iter().map(|ns| *ns as f64 / 1e3).collect());
    let frames = sorted(render.frames.iter().map(|f| *f as f64).collect());
    let round_trip = capture.as_ref().map(|capture| round_trips(&render, capture, &shared.detections.snapshot()));
    let errors = shared.render.errors.load(Ordering::Relaxed) + shared.capture.errors.load(Ordering::Relaxed);
    let invalid = shared.render.invalid.load(Ordering::Relaxed) + shared.capture.invalid.load(Ordering::Relaxed);
    let dropped = [&shared.render, &shared.capture].iter().map(|l| l.arrival_ns.dropped()).sum::<u64>() + shared.detections.dropped();
    Measurements {
        rate: render_rate,
        callbacks: render.arrival_ns.len(),
        achieved_frames: render.median_frames(),
        min_frames: frames.first().copied().unwrap_or(0.0) as u32,
        max_frames: frames.last().copied().unwrap_or(0.0) as u32,
        achieved_ms: period_ns / 1e6,
        jitter_ms: jitter,
        duration_s: render.duration_s(),
        xruns_out: render.gaps(),
        xruns_in: capture.as_ref().map(Stream::gaps),
        cpu_us: cpu,
        round_trip,
        backend_errors: errors,
        invalid_stamps: invalid,
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.5), 50.0);
        assert_eq!(percentile(&values, 0.95), 95.0);
        assert_eq!(percentile(&values, 1.0), 100.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn frame_lookup_spans_uneven_blocks() {
        let stream = Stream { arrival_ns: vec![1000, 2000, INVALID_ARRIVAL], frames: vec![10, 20, 10], cpu_ns: vec![0; 3], rate: 1_000_000_000 };
        let starts = stream.starts();
        assert_eq!(starts, [0, 10, 30]);
        assert_eq!(stream.host_ns(&starts, 0), Some(1000.0));
        assert_eq!(stream.host_ns(&starts, 15), Some(2005.0));
        assert_eq!(stream.host_ns(&starts, 31), None, "invalid block");
        assert_eq!(stream.host_ns(&starts, 99), None, "past the end");
    }
}
