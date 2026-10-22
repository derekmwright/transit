//! Device observations and bounded clock fitting. Device ordinals are not render work.
//! Fitting runs on control; callbacks only publish Copy observations into prepared storage.
//! An estimate with unknown offset uncertainty never becomes a calibrated hardware clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const OBSERVATIONS: usize = 128;
const FIT_POINTS: usize = 64;
// Retain a time span rather than the most recent N callbacks. Small ASIO buffers
// can repeat the same millisecond tick; a few adjacent callbacks cannot fit its rate.
const FIT_SPACING_NS: u64 = 4_000_000;
const MIN_FIT_SPAN_NS: u64 = 100_000_000;
const HORIZON_NS: u64 = 100_000_000;
const MAX_LOCAL_SPAN: i128 = 1 << 40;
const MAX_RESIDUAL_NS: f64 = 2_000_000.0;
// Estimated timestamps can jitter across the previous value. This tolerance is
// bounded by the existing fit residual policy, not permission to relax that fit.
const MAX_ESTIMATE_BACKSTEP_NS: i128 = 2_000_000;

pub use soundcraft_audio_backend::{Direction, HostOrigin, Quality, Stamp};

/// A stream incarnation and a backend clock domain. Reopening A after B uses a new key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockKey {
    pub generation: u64,
    pub domain: u64,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub key: ClockKey,
    pub first_frame: u64,
    pub frames: u32,
    pub nominal_rate: u32,
    pub stamp: Stamp,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PublicationCounts {
    pub dropped: u64,
    pub zero_frames: u64,
    pub exhausted: u64,
}

#[derive(Default)]
struct SharedCounts {
    dropped: AtomicU64,
    zero_frames: AtomicU64,
    exhausted: AtomicU64,
}

pub struct Observer {
    key: ClockKey,
    rate: u32,
    next_frame: u64,
    faulted: bool,
    send: rtrb::Producer<Observation>,
    counts: Arc<SharedCounts>,
}

pub struct Observations {
    receive: rtrb::Consumer<Observation>,
    counts: Arc<SharedCounts>,
}

/// Prepare the stream's sole callback producer and control consumer before starting it.
pub fn observation_lane(key: ClockKey, nominal_rate: u32) -> (Observer, Observations) {
    let (send, receive) = rtrb::RingBuffer::new(OBSERVATIONS);
    let counts = Arc::new(SharedCounts::default());
    (Observer { key, rate: nominal_rate, next_frame: 0, faulted: false, send, counts: Arc::clone(&counts) }, Observations { receive, counts })
}

impl Observer {
    /// Advance the actual device ordinal even when diagnostic publication is full.
    /// Zero-length callbacks and arithmetic exhaustion never invent frames or wrap epochs.
    pub fn record(&mut self, frames: usize, stamp: Stamp) -> bool {
        if frames == 0 {
            self.counts.zero_frames.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if self.faulted {
            return false;
        }
        let Ok(frames) = u32::try_from(frames) else {
            return self.exhaust();
        };
        let Some(next) = self.next_frame.checked_add(u64::from(frames)) else {
            return self.exhaust();
        };
        let observation = Observation { key: self.key, first_frame: self.next_frame, frames, nominal_rate: self.rate, stamp };
        self.next_frame = next;
        if self.send.push(observation).is_err() {
            self.counts.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }
    fn exhaust(&mut self) -> bool {
        self.faulted = true;
        self.counts.exhausted.fetch_add(1, Ordering::Relaxed);
        false
    }
    pub fn next_frame(&self) -> u64 {
        self.next_frame
    }
}

impl Observations {
    pub fn pop(&mut self) -> Option<Observation> {
        self.receive.pop().ok()
    }
    pub fn counts(&self) -> PublicationCounts {
        PublicationCounts {
            dropped: self.counts.dropped.load(Ordering::Relaxed),
            zero_frames: self.counts.zero_frames.load(Ordering::Relaxed),
            exhausted: self.counts.exhausted.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    WrongClock,
    Invalid,
    Reordered,
    WarmingUp,
    Discontinuity,
    Stale,
    OutsideWindow,
    Arithmetic,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FitCounts {
    pub invalid: u64,
    pub stale_generation: u64,
    pub reordered: u64,
    pub discontinuities: u64,
    /// A complete window failed the slope/residual policy. History is retained so
    /// startup noise does not repeatedly reset the estimator before it can converge.
    pub rejected_fits: u64,
}

#[derive(Debug, Clone, Copy)]
struct Line {
    x0: i128,
    y0: i128,
    slope: f64,
    intercept: f64,
    residual: f64,
}

impl Line {
    /// Center before floating conversion, retaining precision with unrelated large epochs.
    fn fit(points: &[(i128, i128)]) -> Result<Self, Unavailable> {
        let Some(&(x0, y0)) = points.first() else {
            return Err(Unavailable::WarmingUp);
        };
        if points.len() < 3 {
            return Err(Unavailable::WarmingUp);
        }
        let (mut sx, mut sy) = (0.0, 0.0);
        for &(x, y) in points {
            let (dx, dy) = (x - x0, y - y0);
            if dx.abs() > MAX_LOCAL_SPAN || dy.abs() > MAX_LOCAL_SPAN {
                return Err(Unavailable::Arithmetic);
            }
            sx += dx as f64;
            sy += dy as f64;
        }
        let (mx, my) = (sx / points.len() as f64, sy / points.len() as f64);
        let (mut xx, mut xy) = (0.0, 0.0);
        for &(x, y) in points {
            let dx = (x - x0) as f64 - mx;
            xx += dx * dx;
            xy += dx * ((y - y0) as f64 - my);
        }
        if xx <= 0.0 {
            return Err(Unavailable::Invalid);
        }
        let slope = xy / xx;
        if !slope.is_finite() || slope <= 0.0 {
            return Err(Unavailable::Discontinuity);
        }
        let intercept = my - slope * mx;
        let residual = points.iter().map(|&(x, y)| ((y - y0) as f64 - (intercept + slope * (x - x0) as f64)).abs()).fold(0.0, f64::max);
        Ok(Self { x0, y0, slope, intercept, residual })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClockMap {
    pub key: ClockKey,
    pub revision: u64,
    pub quality: Quality,
    /// Reported observation bound, not the measured fit residual and not a hardware certificate.
    pub offset_uncertainty_ns: Option<u64>,
    pub residual_ns: f64,
    frame_origin: u64,
    host_origin: u64,
    offset: f64,
    ns_per_frame: f64,
    first_frame: u64,
    last_frame: u64,
    observed_at: u64,
    valid_until: u64,
}

fn checked_round(base: u64, delta: f64) -> Result<u64, Unavailable> {
    if !delta.is_finite() || delta.abs() > MAX_LOCAL_SPAN as f64 {
        return Err(Unavailable::Arithmetic);
    }
    u64::try_from(i128::from(base) + delta.round() as i128).map_err(|_| Unavailable::Arithmetic)
}

impl ClockMap {
    fn valid(&self, key: ClockKey, now: u64) -> Result<(), Unavailable> {
        if key != self.key {
            return Err(Unavailable::WrongClock);
        }
        if now < self.observed_at || now > self.valid_until {
            return Err(Unavailable::Stale);
        }
        Ok(())
    }
    fn frame_window(&self, frame: u64) -> Result<(), Unavailable> {
        if frame < self.first_frame || frame > self.last_frame {
            return Err(Unavailable::OutsideWindow);
        }
        Ok(())
    }
    pub fn frame_to_host(&self, key: ClockKey, frame: u64, now: u64) -> Result<u64, Unavailable> {
        self.valid(key, now)?;
        self.frame_window(frame)?;
        checked_round(self.host_origin, self.offset + (i128::from(frame) - i128::from(self.frame_origin)) as f64 * self.ns_per_frame)
    }
    /// Nearest device sample; retain the original host time separately for recording.
    pub fn host_to_frame(&self, key: ClockKey, host_ns: u64, now: u64) -> Result<u64, Unavailable> {
        self.valid(key, now)?;
        let delta = i128::from(host_ns) - i128::from(self.host_origin);
        if delta.abs() > MAX_LOCAL_SPAN {
            return Err(Unavailable::Arithmetic);
        }
        let frame = checked_round(self.frame_origin, (delta as f64 - self.offset) / self.ns_per_frame)?;
        self.frame_window(frame)?;
        Ok(frame)
    }
    /// The supported device-frame range, including the bounded future projection.
    /// Historical coverage follows the retained observation span (normally at least
    /// 248 ms once full). Place captures on control as observations arrive and retain
    /// the resulting placement with the take; never defer mapping until a disk writer
    /// wakes. Missing coverage is explicit, even if session history still exists.
    pub fn frame_range(&self) -> std::ops::RangeInclusive<u64> {
        self.first_frame..=self.last_frame
    }
    pub fn estimated_rate(&self) -> f64 {
        1e9 / self.ns_per_frame
    }
}

pub struct ClockFitter {
    key: ClockKey,
    points: [Option<Observation>; FIT_POINTS],
    len: usize,
    revision: u64,
    map: Option<ClockMap>,
    counts: FitCounts,
}

impl ClockFitter {
    pub fn new(key: ClockKey) -> Self {
        Self { key, points: [None; FIT_POINTS], len: 0, revision: 0, map: None, counts: FitCounts::default() }
    }
    pub fn counts(&self) -> FitCounts {
        self.counts
    }
    pub fn map(&self) -> Option<ClockMap> {
        self.map
    }
    fn invalidate(&mut self) {
        self.points.fill(None);
        self.len = 0;
        self.map = None;
    }

    /// Off-callback only. Bounded rolling least-squares fits relate frames→backend and
    /// backend→host separately. Unknown ADC/DAC/dispatch bias stays unknown.
    pub fn observe(&mut self, observation: Observation) -> Result<ClockMap, Unavailable> {
        if observation.key != self.key {
            self.counts.stale_generation = self.counts.stale_generation.saturating_add(1);
            return Err(Unavailable::WrongClock);
        }
        let stamp = observation.stamp;
        let direction_valid = match self.key.direction {
            Direction::Input => stamp.backend_frame_ns <= stamp.backend_callback_ns,
            Direction::Output => stamp.backend_frame_ns >= stamp.backend_callback_ns,
        };
        if observation.frames == 0
            || !(8_000..=768_000).contains(&observation.nominal_rate)
            || stamp.quality == Quality::Invalid
            || !direction_valid
            || observation.first_frame.checked_add(u64::from(observation.frames)).is_none()
        {
            self.counts.invalid = self.counts.invalid.saturating_add(1);
            self.invalidate();
            return Err(Unavailable::Invalid);
        }
        if let Some(previous) = self.points.get(self.len.saturating_sub(1)).copied().flatten() {
            if observation.first_frame < previous.first_frame + u64::from(previous.frames) {
                self.counts.reordered = self.counts.reordered.saturating_add(1);
                return Err(Unavailable::Reordered);
            }
            let callback_backstep = i128::from(previous.stamp.backend_callback_ns) - i128::from(stamp.backend_callback_ns);
            let frame_backstep = i128::from(previous.stamp.backend_frame_ns) - i128::from(stamp.backend_frame_ns);
            let estimate_jitter = stamp.quality == Quality::BackendEstimate
                && previous.stamp.quality == Quality::BackendEstimate
                && callback_backstep.max(frame_backstep) <= MAX_ESTIMATE_BACKSTEP_NS;
            if stamp.host_callback_ns <= previous.stamp.host_callback_ns
                || ((callback_backstep > 0 || frame_backstep > 0) && !estimate_jitter)
                || observation.nominal_rate != previous.nominal_rate
            {
                self.counts.discontinuities = self.counts.discontinuities.saturating_add(1);
                self.invalidate();
                return Err(Unavailable::Discontinuity);
            }
        }
        // Keep the newest observation, replacing the provisional tail until it is
        // at least 4 ms after its anchor. Every callback still goes through the above
        // monotonicity checks; coarse equal backend ticks are valid observations.
        if self.len >= 2 {
            let previous = self.points.get(self.len - 1).copied().flatten();
            let anchor = self.points.get(self.len - 2).copied().flatten();
            if previous.zip(anchor).is_some_and(|(p, a)| p.stamp.host_callback_ns - a.stamp.host_callback_ns < FIT_SPACING_NS) {
                self.len -= 1;
            }
        }
        if self.len == FIT_POINTS {
            self.points.rotate_left(1);
            self.len -= 1;
        }
        if let Some(slot) = self.points.get_mut(self.len) {
            *slot = Some(observation);
        }
        self.len += 1;
        let result = self.fit();
        match result {
            Ok(map) => {
                self.map = Some(map);
                Ok(map)
            }
            Err(Unavailable::WarmingUp) => Err(Unavailable::WarmingUp),
            Err(error) => {
                self.counts.rejected_fits = self.counts.rejected_fits.saturating_add(1);
                // Do not publish a questionable fit, but retain the bounded window.
                // A short-span fit failure must not prevent a longer-span recovery.
                self.map = None;
                Err(error)
            }
        }
    }

    fn fit(&mut self) -> Result<ClockMap, Unavailable> {
        let mut frames = [(0i128, 0i128); FIT_POINTS];
        let mut hosts = [(0i128, 0i128); FIT_POINTS];
        let mut quality = Quality::Measured;
        let mut uncertainty = Some(0u64);
        for ((frame, host), observation) in frames.iter_mut().zip(&mut hosts).zip(self.points.iter().flatten()) {
            *frame = (i128::from(observation.first_frame), i128::from(observation.stamp.backend_frame_ns));
            *host = (i128::from(observation.stamp.backend_callback_ns), i128::from(observation.stamp.host_callback_ns));
            quality = quality.min(observation.stamp.quality);
            uncertainty = uncertainty.zip(observation.stamp.offset_uncertainty_ns).map(|(a, b)| a.max(b));
        }
        let first = self.points.first().copied().flatten().ok_or(Unavailable::WarmingUp)?;
        let last = self.points.get(self.len.saturating_sub(1)).copied().flatten().ok_or(Unavailable::WarmingUp)?;
        if last.stamp.host_callback_ns - first.stamp.host_callback_ns < MIN_FIT_SPAN_NS {
            return Err(Unavailable::WarmingUp);
        }
        let frame_line = Line::fit(frames.get(..self.len).ok_or(Unavailable::Arithmetic)?)?;
        let host_line = Line::fit(hosts.get(..self.len).ok_or(Unavailable::Arithmetic)?)?;
        let last = self.points.get(self.len.saturating_sub(1)).copied().flatten().ok_or(Unavailable::WarmingUp)?;
        let ns_per_frame = frame_line.slope * host_line.slope;
        let nominal_period = 1e9 / f64::from(last.nominal_rate);
        // A bounded drift envelope is a validity policy, not a declared accuracy bound.
        if !(0.995..=1.005).contains(&(ns_per_frame / nominal_period)) || !(0.995..=1.005).contains(&host_line.slope) {
            return Err(Unavailable::Discontinuity);
        }
        let origin_delta = frame_line.y0 - host_line.x0;
        if origin_delta.abs() > MAX_LOCAL_SPAN {
            return Err(Unavailable::Arithmetic);
        }
        let offset = host_line.intercept + host_line.slope * (origin_delta as f64 + frame_line.intercept);
        let residual_ns = if quality == Quality::BackendEstimate {
            // Callback and frame stamps share the same backend clock error.
            // Evaluate their paired observation against the composed mapping;
            // adding the two independent maxima counts that shared error twice.
            // This is an observed residual, never an ADC/DAC uncertainty bound.
            let mut residual = 0.0f64;
            for observation in self.points.iter().flatten() {
                let stamp = observation.stamp;
                let backend_delta = i128::from(stamp.backend_frame_ns) - i128::from(stamp.backend_callback_ns);
                if backend_delta.abs() > MAX_LOCAL_SPAN {
                    return Err(Unavailable::Arithmetic);
                }
                let observed = (i128::from(stamp.host_callback_ns) - host_line.y0) as f64 + host_line.slope * backend_delta as f64;
                let predicted = offset + ns_per_frame * (i128::from(observation.first_frame) - frame_line.x0) as f64;
                residual = residual.max((observed - predicted).abs());
            }
            residual
        } else {
            host_line.residual + frame_line.residual * host_line.slope
        };
        if residual_ns > MAX_RESIDUAL_NS {
            return Err(Unavailable::Discontinuity);
        }
        let frame_origin = u64::try_from(frame_line.x0).map_err(|_| Unavailable::Arithmetic)?;
        let host_origin = u64::try_from(host_line.y0).map_err(|_| Unavailable::Arithmetic)?;
        let future_frames = (HORIZON_NS as f64 / ns_per_frame).ceil() as u64;
        let last_frame =
            last.first_frame.checked_add(u64::from(last.frames)).and_then(|n| n.checked_add(future_frames)).ok_or(Unavailable::Arithmetic)?;
        let valid_until = last.stamp.host_callback_ns.checked_add(HORIZON_NS).ok_or(Unavailable::Arithmetic)?;
        self.revision = self.revision.checked_add(1).ok_or(Unavailable::Arithmetic)?;
        Ok(ClockMap {
            key: self.key,
            revision: self.revision,
            quality,
            offset_uncertainty_ns: if quality == Quality::Measured { uncertainty } else { None },
            residual_ns,
            frame_origin,
            host_origin,
            offset,
            ns_per_frame,
            first_frame: frame_origin,
            last_frame,
            observed_at: last.stamp.host_callback_ns,
            valid_until,
        })
    }
}

/// What a stream callback owns: the observation lane producer and the stamp origin.
pub struct CallbackClock {
    pub observer: Observer,
    pub origin: HostOrigin,
}
pub struct ClockReader {
    inner: Box<Reader>,
}
struct Reader {
    observations: Observations,
    fitter: ClockFitter,
    origin: HostOrigin,
}

#[derive(Debug, Clone, Copy)]
pub struct ClockReport {
    pub map: Option<ClockMap>,
    pub publication: PublicationCounts,
    pub fitting: FitCounts,
}

/// Unique per stream incarnation, including failed opens. This is control-thread work.
pub fn stream_clock(direction: Direction, nominal_rate: u32) -> Result<(CallbackClock, ClockReader), &'static str> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    // fetch_update is the spelling available at the workspace MSRV (Rust 1.90).
    #[allow(deprecated)]
    let generation =
        NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)).map_err(|_| "stream clock generation exhausted")?;
    // Separate domains are conservative even if a backend happens to share a hardware clock.
    let key = ClockKey { generation, domain: generation, direction };
    let (observer, observations) = observation_lane(key, nominal_rate);
    let origin = HostOrigin::default();
    Ok((CallbackClock { observer, origin }, ClockReader { inner: Box::new(Reader { observations, fitter: ClockFitter::new(key), origin }) }))
}

impl ClockReader {
    /// Identity of this stream, available before its first callback or clock fit.
    /// The prepared transport must use this key, including on failed/retried opens.
    pub fn key(&self) -> ClockKey {
        self.inner.fitter.key
    }

    pub fn host_origin(&self) -> HostOrigin {
        self.inner.origin
    }

    /// Control/UI only. Bounded even when the producer continues publishing concurrently.
    pub fn poll(&mut self) -> ClockReport {
        let origin = self.inner.origin;
        self.poll_with(|| origin.now_ns())
    }

    fn poll_with(&mut self, now: impl FnOnce() -> Option<u64>) -> ClockReport {
        let reader = &mut self.inner;
        for _ in 0..OBSERVATIONS {
            let Some(observation) = reader.observations.pop() else {
                break;
            };
            let _ = reader.fitter.observe(observation);
        }
        let now = now();
        let map = reader.fitter.map().filter(|map| now.is_some_and(|now| map.valid(map.key, now).is_ok()));
        ClockReport { map, publication: reader.observations.counts(), fitting: reader.fitter.counts() }
    }
}

#[cfg(test)]
mod tests;

#[cfg(not(target_arch = "wasm32"))]
pub mod service;

/// Native device clocks are maintained independently of UI progress. The web
/// adapter retains the existing unavailable-host-clock behavior (no native worker).
pub(crate) struct MaintainedClock {
    #[cfg(not(target_arch = "wasm32"))]
    service: Box<service::ClockService>,
    #[cfg(target_arch = "wasm32")]
    reader: ClockReader,
}
impl MaintainedClock {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn prepare_recording(
        &mut self,
        start: crate::record::start::Reader,
    ) -> Result<crate::record::start::mapping::InputStart, &'static str> {
        self.service.prepare_recording(start)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn take_audio_reader(&mut self) -> Option<service::SnapshotReader> {
        self.service.take_audio_reader()
    }

    pub(crate) fn start(reader: ClockReader) -> std::io::Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Ok(Self { service: Box::new(service::ClockService::start(reader)?) })
        }
        #[cfg(target_arch = "wasm32")]
        {
            Ok(Self { reader })
        }
    }
    pub(crate) fn report(&mut self) -> ClockReport {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.service.report()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.reader.poll()
        }
    }
}
