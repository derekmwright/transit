//! The bench's position clock (issue #71): a least-squares line of the capture blocks' host
//! arrival time against their first sample position, over a long history.
//!
//! The app's [`ClockFitter`](soundcraft_playback::device_clock::ClockFitter) fits about 64
//! points (at least 4 ms apart, 100 to 256 ms) and refuses a fit whose largest residual is above
//! 2 ms. A driver that delivers its buffers in bursts (the Scarlett 2i2 at buffer 256: arrival
//! scatter of about one buffer, 5.3 ms) never gets a map from it. This fit keeps the last
//! [`HISTORY_NS`] of blocks and never fewer than [`KEEP_POINTS`] (when there are that many), takes
//! equal or coarse arrival times, drops points far outside the scatter once and refits, and judges
//! the line by its standard error at the send, not by the scatter. It is the same kind of map the
//! app's MIDI output thread uses (a linear relation of host time and device sample position,
//! [`Line::host_to_frame`] and [`Line::frame_to_host`]), with a longer window.
//!
//! What the figures are:
//! - **scatter**: the largest distance of a point from the line, in ns. How irregular the
//!   arrivals are; not a bound on the send's offset (the line averages over the points).
//! - **standard error at the send**: the line's own uncertainty at the send position (random part).
//!   A systematic offset of the arrivals (the driver's dispatch delay) is in the line and in every
//!   measured latency alike; no fit can see it.
//! - **leave-last-out error**: the newest point against the line fitted without it: what the fit
//!   predicts for a block it has not seen, scatter of that block included.

use std::collections::VecDeque;

/// The history the fit keeps.
pub const HISTORY_NS: u64 = 2_000_000_000;
/// Points kept beyond [`HISTORY_NS`] when there are fewer than this many inside it (large buffers).
pub const KEEP_POINTS: usize = 32;
/// A fit needs this many points ...
pub const MIN_POINTS: usize = 16;
/// ... over at least this span of host time.
pub const MIN_SPAN_NS: u64 = 250_000_000;
/// At most this many points (a 2 s history at small buffers and high rates is cut short).
const MAX_POINTS: usize = 8192;
/// The line's rate must be within this fraction of the nominal rate.
const MAX_RATE_ERROR: f64 = 0.005;
/// A scatter above this is not a clock (blocks lost, or arrivals unrelated to the positions).
pub const MAX_SCATTER_NS: f64 = 25_000_000.0;
/// A point further than this many robust standard deviations from the first line is dropped.
const TRIM_SIGMAS: f64 = 5.0;
/// An arrival this far before the previous one starts the history over (a clock jump).
const MAX_BACKSTEP_NS: u64 = 50_000_000;
/// An instant further than this past the newest point (the input stalled) or before the oldest is not mapped.
const MAX_EXTRAPOLATION_NS: u64 = 500_000_000;

/// One capture block: its first sample position and its host arrival time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    position: u64,
    frames: u64,
    host: u64,
}

/// The rolling history of capture blocks.
#[derive(Debug, Clone)]
pub struct PositionFit {
    rate: u32,
    points: VecDeque<Point>,
    /// Times the history was started over (a position going back, a clock jump, an invalid stamp).
    pub restarts: u64,
}

/// A fitted line: host time = `host0 + intercept + slope * (position - position0)`, centered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    position0: u64,
    host0: u64,
    /// The points' mean position and host time, relative to the origins.
    mean_x: f64,
    mean_y: f64,
    /// ns per sample.
    pub slope: f64,
    /// Sum of squared position deviations (relative positions).
    sxx: f64,
    /// Points in the line (after the trim).
    pub n: usize,
    /// Points dropped by the trim.
    pub trimmed: usize,
    /// Largest distance of a point from the line, ns.
    pub scatter_ns: f64,
    /// Standard deviation of the points about the line (n - 2 degrees of freedom), ns.
    pub sigma_ns: f64,
    /// The newest point against the line fitted without it, ns (0 with too few points).
    pub leave_last_out_ns: f64,
    /// Host time span of the points, ns.
    pub span_ns: u64,
    /// The newest point's host time.
    pub newest_host: u64,
}

/// Why there is no line, with what the history looks like.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub why: String,
    pub points: usize,
    pub span_ns: u64,
    /// Median, smallest and largest spacing of consecutive arrivals, ns (0 without two points).
    pub spacing_ns: (u64, u64, u64),
    /// The block length in ns at the nominal rate (the median block).
    pub block_ns: u64,
    /// Scatter and rate of the line when one could be computed.
    pub scatter_ns: Option<f64>,
    pub rate_hz: Option<f64>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (median, min, max) = self.spacing_ns;
        write!(
            f,
            "{}; {} capture blocks over {:.0} ms, arrival spacing median {:.2} ms (min {:.2}, max {:.2}) against a block of {:.2} ms",
            self.why,
            self.points,
            self.span_ns as f64 / 1e6,
            median as f64 / 1e6,
            min as f64 / 1e6,
            max as f64 / 1e6,
            self.block_ns as f64 / 1e6
        )?;
        if let Some(scatter) = self.scatter_ns {
            write!(f, ", fit scatter {:.2} ms", scatter / 1e6)?;
        }
        if let Some(rate) = self.rate_hz {
            write!(f, ", fitted rate {rate:.1} Hz")?;
        }
        Ok(())
    }
}

/// Least squares of `(x, y)`: the means, slope and sum of squared x deviations; None when the
/// positions do not spread.
fn least_squares(points: &[(f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    let n = points.len() as f64;
    let (mx, my) = points.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + x, b + y));
    let (mx, my) = (mx / n, my / n);
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for &(x, y) in points {
        sxx += (x - mx) * (x - mx);
        sxy += (x - mx) * (y - my);
    }
    (sxx > 0.0).then(|| (mx, my, sxy / sxx, sxx))
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    match values.len() {
        0 => 0.0,
        n if n % 2 == 1 => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2.0,
    }
}

impl PositionFit {
    pub fn new(rate: u32) -> Self {
        Self { rate, points: VecDeque::new(), restarts: 0 }
    }

    /// Start the history over.
    pub fn restart(&mut self) {
        if !self.points.is_empty() {
            self.restarts += 1;
        }
        self.points.clear();
    }

    /// Add a block that starts at `position`, `frames` long, which arrived at host time `host`.
    /// A block overlapping the previous one, or arriving far before it, starts the history over.
    pub fn push(&mut self, position: u64, frames: u64, host: u64) {
        if let Some(last) = self.points.back()
            && (position < last.position.saturating_add(last.frames) || host.saturating_add(MAX_BACKSTEP_NS) < last.host)
        {
            self.restart();
        }
        self.points.push_back(Point { position, frames, host });
        while self.points.len() > MAX_POINTS
            || (self.points.len() > KEEP_POINTS
                && self.points.back().zip(self.points.get(1)).is_some_and(|(newest, second)| newest.host.saturating_sub(second.host) >= HISTORY_NS))
        {
            self.points.pop_front();
        }
    }

    /// Number of blocks in the history.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    fn span(&self) -> u64 {
        match (self.points.front(), self.points.back()) {
            (Some(a), Some(b)) => b.host.saturating_sub(a.host),
            _ => 0,
        }
    }

    fn failure(&self, why: impl Into<String>, scatter_ns: Option<f64>, rate_hz: Option<f64>) -> Failure {
        let mut steps: Vec<f64> = self.points.iter().zip(self.points.iter().skip(1)).map(|(a, b)| b.host.saturating_sub(a.host) as f64).collect();
        let (min, max) = steps.iter().fold((f64::INFINITY, 0.0f64), |(lo, hi), &s| (lo.min(s), hi.max(s)));
        let spacing = if steps.is_empty() { (0, 0, 0) } else { (median(&mut steps) as u64, min as u64, max as u64) };
        let mut frames: Vec<f64> = self.points.iter().map(|p| p.frames as f64).collect();
        let block_ns = (median(&mut frames) * 1e9 / f64::from(self.rate.max(1))) as u64;
        Failure { why: why.into(), points: self.points.len(), span_ns: self.span(), spacing_ns: spacing, block_ns, scatter_ns, rate_hz }
    }

    /// The line over the history, or why there is none.
    pub fn line(&self) -> Result<Line, Failure> {
        let (Some(first), Some(newest)) = (self.points.front().copied(), self.points.back().copied()) else {
            return Err(self.failure("no capture blocks", None, None));
        };
        if self.points.len() < MIN_POINTS || self.span() < MIN_SPAN_NS {
            return Err(self.failure(format!("warming up (a fit needs {MIN_POINTS} blocks over {} ms)", MIN_SPAN_NS / 1_000_000), None, None));
        }
        let relative = |p: &Point| ((p.position - first.position) as f64, p.host as f64 - first.host as f64);
        let all: Vec<(f64, f64)> = self.points.iter().map(relative).collect();
        let Some((mx, my, slope, _)) = least_squares(&all) else {
            return Err(self.failure("the positions do not advance", None, None));
        };
        // One trim: points far outside the robust spread (a late callback after a stall) go.
        let mut deviations: Vec<f64> = all.iter().map(|&(x, y)| (y - (my + slope * (x - mx))).abs()).collect();
        let robust = 1.4826 * median(&mut deviations);
        let limit = (TRIM_SIGMAS * robust).max(1.0);
        let kept: Vec<(f64, f64)> = all.iter().copied().filter(|&(x, y)| (y - (my + slope * (x - mx))).abs() <= limit).collect();
        let kept = if kept.len() * 4 >= all.len() * 3 && kept.len() >= MIN_POINTS { kept } else { all.clone() };
        let Some((mx, my, slope, sxx)) = least_squares(&kept) else {
            return Err(self.failure("the positions do not advance", None, None));
        };
        let residuals: Vec<f64> = kept.iter().map(|&(x, y)| y - (my + slope * (x - mx))).collect();
        let scatter = residuals.iter().fold(0.0f64, |m, r| m.max(r.abs()));
        let dof = kept.len().saturating_sub(2).max(1) as f64;
        let sigma = (residuals.iter().map(|r| r * r).sum::<f64>() / dof).sqrt();
        let rate_hz = 1e9 / slope;
        if !(slope.is_finite() && slope > 0.0) || ((rate_hz / f64::from(self.rate)) - 1.0).abs() > MAX_RATE_ERROR {
            return Err(self.failure(
                format!("the fitted rate is more than {:.1}% from the nominal {} Hz", MAX_RATE_ERROR * 100.0, self.rate),
                Some(scatter),
                Some(rate_hz),
            ));
        }
        if scatter > MAX_SCATTER_NS {
            return Err(self.failure(format!("the fit scatter is above {:.0} ms", MAX_SCATTER_NS / 1e6), Some(scatter), Some(rate_hz)));
        }
        // The newest point against the line without it.
        let leave_last_out =
            all.split_last().and_then(|(&(x, y), rest)| least_squares(rest).map(|(mx, my, slope, _)| y - (my + slope * (x - mx)))).unwrap_or(0.0);
        Ok(Line {
            position0: first.position,
            host0: first.host,
            mean_x: mx,
            mean_y: my,
            slope,
            sxx,
            n: kept.len(),
            trimmed: all.len() - kept.len(),
            scatter_ns: scatter,
            sigma_ns: sigma,
            leave_last_out_ns: leave_last_out,
            span_ns: self.span(),
            newest_host: newest.host,
        })
    }
}

impl Line {
    /// The nearest sample position at host time `host`, or why it is not mapped.
    pub fn host_to_frame(&self, host: u64) -> Result<u64, String> {
        if host > self.newest_host.saturating_add(MAX_EXTRAPOLATION_NS) {
            return Err(format!("the instant is {:.0} ms after the newest capture block: the input stalled", (host - self.newest_host) as f64 / 1e6));
        }
        if host.saturating_add(MAX_EXTRAPOLATION_NS) < self.host0 {
            return Err("the instant is before the clock fit's history".into());
        }
        let y = host as f64 - self.host0 as f64;
        let x = self.mean_x + (y - self.mean_y) / self.slope;
        let position = self.position0 as f64 + x.round();
        if position < 0.0 { Err("the instant is before the first capture block".into()) } else { Ok(position as u64) }
    }

    /// The host time of sample position `frame`.
    pub fn frame_to_host(&self, frame: u64) -> u64 {
        let x = frame as f64 - self.position0 as f64;
        let host = self.host0 as f64 + self.mean_y + self.slope * (x - self.mean_x);
        host.round().max(0.0) as u64
    }

    /// The line's standard error at sample position `frame`, in ns: sigma x sqrt(1/n + (x - mean)^2 / Sxx).
    pub fn error_at_ns(&self, frame: u64) -> f64 {
        let x = frame as f64 - self.position0 as f64;
        let n = self.n.max(1) as f64;
        let lever = if self.sxx > 0.0 { (x - self.mean_x).powi(2) / self.sxx } else { 0.0 };
        self.sigma_ns * (1.0 / n + lever).sqrt()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn t(position: u64) -> u64 {
        1_000_000_000 + position * 1_000_000_000 / u64::from(RATE)
    }

    #[test]
    fn an_exact_clock_maps_both_ways_and_has_no_scatter() {
        let mut fit = PositionFit::new(RATE);
        for k in 0..400u64 {
            fit.push(k * 64, 64, t(k * 64));
        }
        let line = fit.line().unwrap();
        assert!(line.scatter_ns < 1.0 && line.sigma_ns < 1.0 && line.leave_last_out_ns.abs() < 1.0, "{line:?}");
        assert_eq!(line.host_to_frame(t(25_000)).unwrap(), 25_000);
        assert!(line.frame_to_host(25_000).abs_diff(t(25_000)) <= 1);
        assert!(line.error_at_ns(25_000) < 1.0);
    }

    #[test]
    fn coarse_arrivals_in_pairs_of_256_frame_blocks_still_fit() {
        // Two blocks share one arrival at the end of the pair: scatter of one block (5.3 ms) about
        // a line 384 samples late, which the app's 2 ms residual limit refuses.
        let mut fit = PositionFit::new(RATE);
        for k in 0..375u64 {
            let p = k * 256;
            fit.push(p, 256, t((p + 256).div_ceil(512) * 512));
        }
        let line = fit.line().unwrap();
        assert!((line.scatter_ns - 2_666_667.0).abs() < 10_000.0, "{line:?}");
        assert!((line.slope * 48_000.0 / 1e9 - 1.0).abs() < 1e-4, "{line:?}");
        // The line passes through the mean arrival: 384 samples after the position's own time.
        assert!(line.host_to_frame(t(50_000 + 384)).unwrap().abs_diff(50_000) <= 1, "{line:?}");
        assert!(line.error_at_ns(96_000) < 400_000.0, "the line is known far better than the scatter: {line:?}");
    }

    #[test]
    fn a_wrong_rate_is_refused_with_the_block_spacing() {
        let mut fit = PositionFit::new(RATE);
        for k in 0..200u64 {
            fit.push(k * 256, 256, 1_000_000_000 + k * 256 * 1_000_000_000 / 46_000);
        }
        let failure = fit.line().unwrap_err();
        let text = failure.to_string();
        assert!(text.contains("fitted rate") && text.contains("arrival spacing median 5.57 ms") && text.contains("block of 5.33 ms"), "{text}");
        assert!(PositionFit::new(RATE).line().unwrap_err().to_string().contains("no capture blocks"));
    }

    #[test]
    fn the_history_keeps_two_seconds_or_32_points_and_restarts_on_a_jump_back() {
        let mut fit = PositionFit::new(RATE);
        for k in 0..2000u64 {
            fit.push(k * 64, 64, t(k * 64));
        }
        assert!((1499..=1502).contains(&fit.len()), "{}", fit.len());
        let mut big = PositionFit::new(RATE);
        for k in 0..100u64 {
            big.push(k * 8192, 8192, t(k * 8192));
        }
        assert_eq!(big.len(), KEEP_POINTS);
        big.push(0, 8192, t(100 * 8192));
        assert_eq!((big.len(), big.restarts), (1, 1));
    }

    #[test]
    fn one_late_callback_is_trimmed() {
        let mut fit = PositionFit::new(RATE);
        for k in 0..400u64 {
            let late = if k == 390 {
                8_000_000
            } else if k % 2 == 0 {
                50_000
            } else {
                0
            };
            fit.push(k * 64, 64, t(k * 64) + late);
        }
        let line = fit.line().unwrap();
        assert_eq!(line.trimmed, 1, "{line:?}");
        assert!(line.scatter_ns < 30_000.0, "{line:?}");
    }
}
