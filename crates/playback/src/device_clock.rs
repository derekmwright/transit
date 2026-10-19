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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

/// A stream incarnation and a backend clock domain. Reopening A after B uses a new key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockKey {
    pub generation: u64,
    pub domain: u64,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Invalid,
    Arrival,
    BackendEstimate,
    Measured,
}

/// Backend times are integer nanoseconds in this stream's explicitly identified domain.
/// `host_callback_ns` is in a common process origin. An arrival observation can have an
/// unknown dispatch bias; the residual jitter of a fit does not bound that bias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub host_callback_ns: u64,
    pub backend_callback_ns: i64,
    pub backend_frame_ns: i64,
    pub quality: Quality,
    pub offset_uncertainty_ns: Option<u64>,
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
            if stamp.host_callback_ns <= previous.stamp.host_callback_ns
                || stamp.backend_callback_ns < previous.stamp.backend_callback_ns
                || stamp.backend_frame_ns < previous.stamp.backend_frame_ns
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
        let residual_ns = host_line.residual + frame_line.residual * host_line.slope;
        if residual_ns > 2_000_000.0 {
            return Err(Unavailable::Discontinuity);
        }
        let origin_delta = frame_line.y0 - host_line.x0;
        if origin_delta.abs() > MAX_LOCAL_SPAN {
            return Err(Unavailable::Arithmetic);
        }
        let offset = host_line.intercept + host_line.slope * (origin_delta as f64 + frame_line.intercept);
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

/// The same timestamp extraction is used by real CPAL callbacks and fake-device tests.
pub struct CpalObserver {
    observer: Observer,
    backend_origin: Option<cpal::StreamInstant>,
}

fn relative_ns(instant: cpal::StreamInstant, origin: cpal::StreamInstant) -> Option<i64> {
    if let Some(elapsed) = instant.duration_since(&origin) {
        i64::try_from(elapsed.as_nanos()).ok()
    } else {
        let elapsed = origin.duration_since(&instant)?;
        i64::try_from(-i128::try_from(elapsed.as_nanos()).ok()?).ok()
    }
}

impl CpalObserver {
    pub fn new(observer: Observer) -> Self {
        Self { observer, backend_origin: None }
    }
    pub fn input(&mut self, frames: usize, info: &cpal::InputCallbackInfo, host_ns: Option<u64>) {
        let time = info.timestamp();
        self.record(frames, time.callback, time.capture, host_ns);
    }
    pub fn output(&mut self, frames: usize, info: &cpal::OutputCallbackInfo, host_ns: Option<u64>) {
        let time = info.timestamp();
        self.record(frames, time.callback, time.playback, host_ns);
    }
    fn record(&mut self, frames: usize, callback: cpal::StreamInstant, frame: cpal::StreamInstant, host_ns: Option<u64>) {
        if frames == 0 {
            self.observer.counts.zero_frames.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let origin = *self.backend_origin.get_or_insert(callback);
        let callback = relative_ns(callback, origin);
        let frame = relative_ns(frame, origin);
        let valid = callback.is_some() && frame.is_some() && host_ns.is_some();
        self.observer.record(
            frames,
            Stamp {
                host_callback_ns: host_ns.unwrap_or(0),
                backend_callback_ns: callback.unwrap_or(0),
                backend_frame_ns: frame.unwrap_or(0),
                // Pinned CPAL does not expose the validity flags or calibrated ADC/DAC offset.
                quality: if valid { Quality::BackendEstimate } else { Quality::Invalid },
                offset_uncertainty_ns: None,
            },
        );
    }
}

/// Initialized on control. Reading the cached origin performs no lazy callback initialization.
#[derive(Clone, Copy)]
pub struct HostOrigin {
    #[cfg(not(target_arch = "wasm32"))]
    instant: std::time::Instant,
}

impl Default for HostOrigin {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            Self { instant: *ORIGIN.get_or_init(std::time::Instant::now) }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self {}
        }
    }
}

impl HostOrigin {
    /// The exact origin used by native audio observations. Pass this to other
    /// device adapters rather than comparing nanoseconds from separate origins.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn instant(self) -> std::time::Instant {
        self.instant
    }

    pub fn now_ns(self) -> Option<u64> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            u64::try_from(self.instant.elapsed().as_nanos()).ok()
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }
}

pub struct CallbackClock {
    pub cpal: CpalObserver,
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
    Ok((
        CallbackClock { cpal: CpalObserver::new(observer), origin },
        ClockReader { inner: Box::new(Reader { observations, fitter: ClockFitter::new(key), origin }) },
    ))
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
