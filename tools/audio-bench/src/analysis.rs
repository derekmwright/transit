//! From the raw logs to the numbers of a row. Runs on the control thread after the streams stop.
//!
//! Xruns are counted by drift, not by callback gaps. Some drivers (the Focusrite ASIO driver at
//! 64 and 32 frames) deliver callbacks in bursts: a two-period wait, then two callbacks back to
//! back. Every gap is long, yet the frames delivered keep pace with the host clock, so nothing was
//! lost. The detector therefore asks when each block was due (the first valid arrival after the
//! warm-up plus the frames delivered since, at the nominal rate), takes the arrival's drift from
//! that, removes the straight line of the device clock's rate error by least squares and counts
//! each sustained upward step of the residual: a skipped buffer leaves the stream permanently one
//! block behind wall time, a burst leaves a spike that returns to the baseline. On the render
//! stream, callbacks whose body took longer than the period also count: on ASIO a host-side
//! overrun does not change the callback cadence, so only the body time shows it.

use crate::probe::{CallbackLog, INVALID_ARRIVAL, Shared, click_start};
use std::sync::atomic::Ordering;

/// Callbacks at the start of a stream that are left out of the jitter figures: drivers deliver
/// the first few back to back while they fill their buffers.
pub const WARMUP_CALLBACKS: usize = 8;
/// Callbacks in each window the drift detector compares (at least four periods, as every
/// callback delivers one period).
pub const DRIFT_WINDOW: usize = 8;
/// A step of the residual drift above this many periods counts as an xrun.
pub const XRUN_STEP_PERIODS: f64 = 0.5;
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
    /// Xruns of the render stream: sustained steps of its arrival drift (see the module doc) plus
    /// callbacks whose body time exceeded the period.
    pub xruns_out: u64,
    /// Sustained steps of the capture stream's arrival drift, when a capture stream ran. The
    /// capture body time is not counted.
    pub xruns_in: Option<u64>,
    /// Callback body time of the render stream, in microseconds.
    pub cpu_us: Percentiles,
    pub round_trip: Option<RoundTrip>,
    /// The round trip in the stamps' own time, when a capture stream ran (see [`FrameTrip`]).
    pub frame_trip: Option<FrameTrip>,
    /// Calls of the backends' error callbacks (render plus capture).
    pub backend_errors: u64,
    /// Callbacks with an invalid stamp, and log entries lost to a full log.
    pub invalid_stamps: u64,
    pub dropped: u64,
    /// Peak absolute sample (linear) per captured channel over the run; empty without a capture stream.
    pub input_peaks: Vec<f32>,
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
    /// `backend_frame_ns` of each block (first frame, in the backend's clock).
    pub frame_ns: Vec<i64>,
    pub frames: Vec<u64>,
    pub cpu_ns: Vec<u64>,
    pub rate: u32,
}

impl Stream {
    pub fn read(log: &CallbackLog, rate: u32) -> Self {
        Self {
            arrival_ns: log.arrival_ns.snapshot(),
            frame_ns: log.frame_ns.snapshot().into_iter().map(|bits| bits as i64).collect(),
            frames: log.frames.snapshot(),
            cpu_ns: log.cpu_ns.snapshot(),
            rate,
        }
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
    /// Sustained steps of the arrival drift: the arrivals' lag behind the frames delivered, after
    /// the clock-rate line is removed. A burst is a spike that returns, a lost buffer a step.
    fn drift_steps(&self) -> u64 {
        let period = self.period_ns();
        let starts = self.starts();
        let valid: Vec<usize> = (WARMUP_CALLBACKS..self.arrival_ns.len()).filter(|&i| self.arrival_ns[i] != INVALID_ARRIVAL).collect();
        let (Some(&first), true) = (valid.first(), period > 0.0) else { return 0 };
        let rate = f64::from(self.rate.max(1));
        let x: Vec<f64> = valid.iter().map(|&i| (starts[i] - starts[first]) as f64).collect();
        let y: Vec<f64> = valid.iter().zip(&x).map(|(&i, x)| (self.arrival_ns[i] as f64 - self.arrival_ns[first] as f64) - x * 1e9 / rate).collect();
        let n = x.len() as f64;
        let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
        let sxx: f64 = x.iter().map(|x| (x - mx) * (x - mx)).sum();
        let slope = if sxx > 0.0 { x.iter().zip(&y).map(|(x, y)| (x - mx) * (y - my)).sum::<f64>() / sxx } else { 0.0 };
        let residual: Vec<f64> = x.iter().zip(&y).map(|(x, y)| y - my - slope * (x - mx)).collect();
        let median = |window: &[f64]| percentile(&sorted(window.to_vec()), 0.5);
        let (w, mut steps, mut at) = (DRIFT_WINDOW, 0u64, DRIFT_WINDOW);
        while at + w <= residual.len() {
            if median(&residual[at..at + w]) - median(&residual[at - w..at]) > period * XRUN_STEP_PERIODS {
                steps += 1;
                at += w;
            } else {
                at += 1;
            }
        }
        steps
    }
    /// Callbacks after the warm-up whose body time exceeded the period.
    fn overruns(&self) -> u64 {
        let period = self.period_ns();
        self.cpu_ns.iter().skip(WARMUP_CALLBACKS).filter(|ns| **ns as f64 > period).count() as u64
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
    /// Backend time in ns of stream frame `at`: the `backend_frame_ns` of its block plus the offset
    /// in it. A block whose stamp was invalid has no time.
    fn frame_time_ns(&self, starts: &[u64], at: u64) -> Option<f64> {
        let block = starts.partition_point(|s| *s <= at).checked_sub(1)?;
        let valid = *self.arrival_ns.get(block)? != INVALID_ARRIVAL;
        (valid && at < starts[block] + self.frames[block])
            .then(|| *self.frame_ns.get(block).unwrap_or(&0) as f64 + (at - starts[block]) as f64 * 1e9 / f64::from(self.rate))
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

/// The time in ns of stream frame `at` on one clock: `Stream::host_ns` or `Stream::frame_time_ns`.
type Clock = fn(&Stream, &[u64], u64) -> Option<f64>;

/// Pair each detection (a frame of the capture stream) with the latest click that left before it
/// on the same clock, as (clicks emitted, trips in ms).
fn pair_clicks(render: &Stream, capture: &Stream, detections: &[u64], time: Clock) -> (usize, Vec<f64>) {
    let (render_starts, capture_starts) = (render.starts(), capture.starts());
    let total: u64 = render.frames.iter().sum();
    let emitted: Vec<f64> = (0..)
        .map(|k| click_start(render.rate, k))
        .take_while(|start| *start < total)
        .filter_map(|start| time(render, &render_starts, start))
        .collect();
    let mut seen = vec![false; emitted.len()];
    let mut trips = Vec::new();
    for at in detections {
        let Some(arrived) = time(capture, &capture_starts, *at) else { continue };
        let Some(k) = emitted.partition_point(|sent| *sent <= arrived).checked_sub(1) else { continue };
        let trip_ms = (arrived - emitted[k]) / 1e6;
        if !seen[k] && (0.0..=MAX_ROUND_TRIP_MS).contains(&trip_ms) {
            seen[k] = true;
            trips.push(trip_ms);
        }
    }
    (emitted.len(), sorted(trips))
}

/// Round trips: click `k` leaves at (render host time of its first frame); the detector finds
/// it at (capture host time of its first frame). Each detection pairs with the latest click that
/// left before it. The bias is the callback dispatch jitter of both streams (not removed).
pub fn round_trips(render: &Stream, capture: &Stream, detections: &[u64]) -> RoundTrip {
    let (clicks, trips) = pair_clicks(render, capture, detections, Stream::host_ns);
    let frames = |ms: f64| ms * f64::from(render.rate) / 1000.0;
    let (min, median, p95) = (trips.first().copied().unwrap_or(0.0), percentile(&trips, 0.5), percentile(&trips, 0.95));
    RoundTrip {
        clicks,
        paired: trips.len(),
        min_ms: min,
        median_ms: median,
        p95_ms: p95,
        min_frames: frames(min),
        median_frames: frames(median),
        p95_frames: frames(p95),
    }
}

/// The loopback round trip in the stamps' own time: per click, the capture frame time of the
/// detected click minus the render presentation time of the emitted click (`backend_frame_ns` of
/// both streams, read at the frame's offset in its block). It is the delay a calibration carries,
/// and is meaningful only when both streams stamp one clock (the native backend's shared host
/// origin).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameTrip {
    pub clicks: usize,
    pub paired: usize,
    pub min_ns: f64,
    pub median_ns: f64,
    pub p95_ns: f64,
}

pub fn frame_trips(render: &Stream, capture: &Stream, detections: &[u64]) -> FrameTrip {
    let (clicks, trips) = pair_clicks(render, capture, detections, Stream::frame_time_ns);
    let ns = |ms: f64| ms * 1e6;
    FrameTrip {
        clicks,
        paired: trips.len(),
        min_ns: ns(trips.first().copied().unwrap_or(0.0)),
        median_ns: ns(percentile(&trips, 0.5)),
        p95_ns: ns(percentile(&trips, 0.95)),
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
    let frame_trip = capture.as_ref().map(|capture| frame_trips(&render, capture, &shared.detections.snapshot()));
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
        xruns_out: render.drift_steps() + render.overruns(),
        xruns_in: capture.as_ref().map(Stream::drift_steps),
        cpu_us: cpu,
        round_trip,
        frame_trip,
        backend_errors: errors,
        invalid_stamps: invalid,
        dropped,
        input_peaks: input_peaks(shared),
    }
}

/// The per-channel input peaks the capture callback tracked.
fn input_peaks(shared: &Shared) -> Vec<f32> {
    let channels = shared.input_channels.load(Ordering::Relaxed);
    shared.input_peaks[..channels].iter().map(|p| f32::from_bits(p.load(Ordering::Relaxed))).collect()
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
        let stream = Stream {
            arrival_ns: vec![1000, 2000, INVALID_ARRIVAL],
            frame_ns: vec![0; 3],
            frames: vec![10, 20, 10],
            cpu_ns: vec![0; 3],
            rate: 1_000_000_000,
        };
        let starts = stream.starts();
        assert_eq!(starts, [0, 10, 30]);
        assert_eq!(stream.host_ns(&starts, 0), Some(1000.0));
        assert_eq!(stream.host_ns(&starts, 15), Some(2005.0));
        assert_eq!(stream.host_ns(&starts, 31), None, "invalid block");
        assert_eq!(stream.host_ns(&starts, 99), None, "past the end");
    }

    /// Blocks of `frames` at 48 kHz, arriving at `arrive(k)` ns, with body time `cpu`.
    fn stream(frames: u64, blocks: u64, arrive: impl Fn(u64, f64) -> f64, cpu: impl Fn(u64) -> u64) -> Stream {
        let period = frames as f64 * 1e9 / 48_000.0;
        Stream {
            arrival_ns: (0..blocks).map(|k| (1e9 + arrive(k, period)) as u64).collect(),
            frame_ns: vec![0; blocks as usize],
            frames: vec![frames; blocks as usize],
            cpu_ns: (0..blocks).map(cpu).collect(),
            rate: 48_000,
        }
    }

    fn on_time(k: u64, period: f64) -> f64 {
        k as f64 * period
    }

    #[test]
    fn a_steady_stream_has_no_xruns() {
        let s = stream(64, 3000, on_time, |_| 1000);
        assert_eq!((s.drift_steps(), s.overruns()), (0, 0));
    }

    #[test]
    fn one_true_gap_counts_once() {
        // From block 1500 on every arrival is one period late: the frames never catch up.
        let s = stream(64, 3000, |k, p| on_time(k, p) + if k >= 1500 { p } else { 0.0 }, |_| 0);
        assert_eq!(s.drift_steps(), 1);
        // The same with two separate losses.
        let s = stream(64, 3000, |k, p| on_time(k, p) + p * f64::from(u8::from(k >= 1000)) + p * f64::from(u8::from(k >= 2000)), |_| 0);
        assert_eq!(s.drift_steps(), 2);
    }

    #[test]
    fn bursty_delivery_counts_nothing() {
        // A two-period wait, then two blocks back to back, frames keeping pace.
        for frames in [64, 32] {
            let s = stream(frames, 4000, |k, p| (k | 1) as f64 * p, |_| 0);
            assert_eq!(s.drift_steps(), 0, "{frames} frames");
        }
    }

    #[test]
    fn a_clock_rate_mismatch_is_not_an_xrun() {
        // 300 ppm slow over 60 s at 32 frames: about 18 ms, hundreds of periods, as a straight line.
        let blocks = 60 * 48_000 / 32;
        let s = stream(32, blocks, |k, p| on_time(k, p) * 1.0003, |_| 0);
        assert_eq!(s.drift_steps(), 0);
        // A real gap on top of the mismatch is still found.
        let s = stream(32, blocks, |k, p| on_time(k, p) * 1.0003 + if k >= blocks / 2 { p } else { 0.0 }, |_| 0);
        assert_eq!(s.drift_steps(), 1);
    }

    #[test]
    fn invalid_arrivals_are_skipped() {
        let mut s = stream(64, 3000, on_time, |_| 0);
        for i in (100..3000).step_by(7) {
            s.arrival_ns[i] = INVALID_ARRIVAL;
        }
        assert_eq!(s.drift_steps(), 0);
    }

    #[test]
    fn a_render_body_longer_than_the_period_counts() {
        let period_ns = (64.0 * 1e9 / 48_000.0) as u64;
        let s = stream(64, 3000, on_time, |k| if k == 1500 { period_ns + 1 } else { period_ns / 2 });
        assert_eq!((s.drift_steps(), s.overruns()), (0, 1));
    }
}
