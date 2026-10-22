//! Output callback diagnostics, accumulated without locks, allocation or retry loops.
//!
//! Duration measures our rendering and sample conversion, excluding telemetry publication and
//! CPAL's work before/after our closure (for example ASIO interleaving and OutputReady). It does
//! not measure device latency or scheduling lateness. The nominal budget is frames / device
//! rate, NOT the time remaining until a hardware deadline. In particular WASAPI supplies the
//! available buffer space, while the queued padding determines how long before it runs dry.
//! A callback can miss that deadline without exceeding the nominal budget; no over-budget
//! count therefore does not prove glitch-free audio. CPAL 0.16 has no portable typed xrun report.
//!
//! Counters and maxima last for one Player/stream. Reopening starts a new set. The first callback
//! is included: cold-start work can cause an audible failure and must not be hidden as warm-up.
//! Suggested labels: "Peak callback load (since open)" and "Over-budget callbacks (nominal)".

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Inclusive duration histogram limits in nanoseconds; the last bin has no upper limit.
pub const DURATION_LIMITS_NS: [u64; 15] =
    [1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 256_000, 512_000, 1_024_000, 2_048_000, 4_096_000, 8_192_000, 16_384_000];

/// Approximate, nonblocking view of lifetime output diagnostics.
///
/// Fields are loaded independently and can span several publications if the reader is delayed:
/// `timed_callbacks` can exceed `callbacks`, and the histogram sum can disagree with either.
/// Individual fields are atomic, but cross-field arithmetic needs checked/saturating operations
/// and cannot establish an exact average or rate. After all writers stop with synchronization,
/// the snapshot is exact. Do not use it for transport synchronization. The silent fallback clock
/// does not contribute callback durations/counts, but can contribute render-silence counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallbackTelemetry {
    /// Device callbacks, including zero-frame callbacks, counted once regardless of scratch chunks.
    pub callbacks: u64,
    pub frames: u64,
    /// Callbacks with a wall-time measurement. Native only; WebAudio timing is unavailable.
    pub timed_callbacks: u64,
    pub total_duration_ns: u64,
    /// Largest measured callback work time since this player opened, including startup.
    pub max_duration_ns: u64,
    /// Largest work time / nominal frame duration since open, in millionths (1_000_000 = 100%).
    /// This is not remaining device headroom or CPU utilization; startup is included.
    pub max_load_ppm: u64,
    /// Work time strictly exceeded frames / device rate; zero-size/unknown-rate blocks excluded.
    /// A throughput diagnostic, not a count of missed hardware deadlines or xruns.
    pub over_budget_callbacks: u64,
    /// Over-budget callbacks that overlapped a plug-in creation on the control thread (module
    /// load, `initialize`, state restore). Nonzero means the stall coincided with a load, which
    /// points at the process (a vendor lock, memory pressure, CPU contention), not at a lock of
    /// ours: the callback takes none. A subset of `over_budget_callbacks`.
    pub over_budget_while_creating: u64,
    /// Highest callback load in the recent window (about one second of callbacks, see
    /// [`RECENT_SLOTS`]), in millionths. Unlike `max_load_ppm` it decays once the callbacks that
    /// caused it leave the window. Zero before the first callback.
    pub current_load_ppm: u64,
    /// Over-budget callbacks inside the same recent window; decays like `current_load_ppm`.
    pub recent_over_budget: u64,
    /// Frames of the most recent callback (the device buffer as the driver delivers it).
    pub callback_frames: u32,
    /// Device sample rate the most recent callback was budgeted against.
    pub sample_rate: u32,
    /// Exclusive bins with inclusive upper limits [`DURATION_LIMITS_NS`], followed by overflow.
    pub duration_histogram: [u64; 16],
    /// CPAL error callbacks of all kinds. These are not necessarily xruns.
    pub backend_errors: u64,
    pub device_lost_errors: u64,
    /// Render frames silenced because the holder's lock was busy during teardown. With the
    /// current ownership model these teardown diagnostics cannot be read after Player::drop;
    /// they are not live underrun counters. Internal teardown probes can retain the storage.
    pub contention_silence_frames: u64,
    /// Teardown frames silenced after the state was taken; same observation limit as contention.
    pub unavailable_silence_frames: u64,
    /// Render frames silenced because rendering panicked, or an earlier panic latched the fault.
    pub fault_silence_frames: u64,
}

/// Callbacks kept for the recent window: one second at 48 kHz down to 47-frame buffers. Smaller
/// buffers see a shorter window (still the last `RECENT_SLOTS` callbacks).
pub const RECENT_SLOTS: usize = 1024;
const OVER_BIT: u64 = 1 << 32;

/// The last [`RECENT_SLOTS`] callbacks' loads, one relaxed store per callback and no allocation.
/// Slot `(n - 1) % RECENT_SLOTS` holds callback `n`: load in millionths (saturated to 32 bits)
/// with [`OVER_BIT`] set when over budget. Only the output callback writes; the UI reads.
struct Recent([AtomicU64; RECENT_SLOTS]);

impl Default for Recent {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}

/// Atomic publication storage. Only the output callback's `Accumulator` writes timing fields;
/// the UI only reads them. Other counters have their own writers and are never reset in place.
#[derive(Default)]
pub(crate) struct Counters {
    callbacks: AtomicU64,
    frames: AtomicU64,
    timed_callbacks: AtomicU64,
    total_duration_ns: AtomicU64,
    max_duration_ns: AtomicU64,
    max_load_ppm: AtomicU64,
    over_budget_callbacks: AtomicU64,
    over_budget_while_creating: AtomicU64,
    /// Peak reset requests from the UI; the callback zeroes its maxima when this moves.
    peak_resets: AtomicU64,
    last_frames: AtomicU64,
    last_rate: AtomicU64,
    recent: Recent,
    duration_histogram: [AtomicU64; 16],
    pub backend_errors: AtomicU64,
    pub device_lost_errors: AtomicU64,
    pub contention_silence_frames: AtomicU64,
    pub unavailable_silence_frames: AtomicU64,
    pub fault_silence_frames: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> CallbackTelemetry {
        let callbacks = self.callbacks.load(Ordering::Relaxed);
        let frames = self.last_frames.load(Ordering::Relaxed);
        let rate = self.last_rate.load(Ordering::Relaxed);
        // About one second of callbacks at the current buffer size and rate.
        let window = if frames == 0 { RECENT_SLOTS as u64 } else { rate.div_ceil(frames).clamp(1, RECENT_SLOTS as u64) };
        let (mut current, mut over) = (0u64, 0u64);
        for back in 0..window.min(callbacks) {
            let slot = self.recent.0[((callbacks - 1 - back) % RECENT_SLOTS as u64) as usize].load(Ordering::Relaxed);
            current = current.max(slot & 0xffff_ffff);
            over += u64::from(slot & OVER_BIT != 0);
        }
        CallbackTelemetry {
            current_load_ppm: current,
            recent_over_budget: over,
            callback_frames: frames.min(u64::from(u32::MAX)) as u32,
            sample_rate: rate.min(u64::from(u32::MAX)) as u32,
            callbacks,
            frames: self.frames.load(Ordering::Relaxed),
            timed_callbacks: self.timed_callbacks.load(Ordering::Relaxed),
            total_duration_ns: self.total_duration_ns.load(Ordering::Relaxed),
            max_duration_ns: self.max_duration_ns.load(Ordering::Relaxed),
            max_load_ppm: self.max_load_ppm.load(Ordering::Relaxed),
            over_budget_callbacks: self.over_budget_callbacks.load(Ordering::Relaxed),
            over_budget_while_creating: self.over_budget_while_creating.load(Ordering::Relaxed),
            duration_histogram: std::array::from_fn(|i| self.duration_histogram[i].load(Ordering::Relaxed)),
            backend_errors: self.backend_errors.load(Ordering::Relaxed),
            device_lost_errors: self.device_lost_errors.load(Ordering::Relaxed),
            contention_silence_frames: self.contention_silence_frames.load(Ordering::Relaxed),
            unavailable_silence_frames: self.unavailable_silence_frames.load(Ordering::Relaxed),
            fault_silence_frames: self.fault_silence_frames.load(Ordering::Relaxed),
        }
    }

    /// Ask the output callback to start the since-open peak (load and longest callback) over.
    pub fn reset_peak(&self) {
        self.peak_resets.fetch_add(1, Ordering::Relaxed);
    }

    pub fn error(&self, device_lost: bool) {
        self.backend_errors.fetch_add(1, Ordering::Relaxed);
        if device_lost {
            self.device_lost_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Owned by the FnMut device callback. Local maxima avoid atomic compare/exchange retry loops.
#[derive(Default)]
pub(crate) struct Accumulator(CallbackTelemetry, u64);

impl Accumulator {
    /// Test shorthand for a callback that overlapped no plug-in creation.
    #[cfg(test)]
    pub fn record(&mut self, counters: &Counters, frames: usize, rate: u32, elapsed: Option<Duration>) {
        self.record_overlap(counters, frames, rate, elapsed, false);
    }

    /// Records one callback; `creating` says a plug-in creation was running during it.
    pub fn record_overlap(&mut self, counters: &Counters, frames: usize, rate: u32, elapsed: Option<Duration>, creating: bool) {
        let seen = counters.peak_resets.load(Ordering::Relaxed);
        if seen != self.1 {
            // The UI asked for a new peak: forget the since-open maxima, once per request.
            self.1 = seen;
            self.0.max_duration_ns = 0;
            self.0.max_load_ppm = 0;
        }
        let s = &mut self.0;
        s.callbacks = s.callbacks.saturating_add(1);
        s.frames = s.frames.saturating_add(frames as u64);
        let mut slot = 0u64;
        if let Some(elapsed) = elapsed {
            let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
            s.timed_callbacks = s.timed_callbacks.saturating_add(1);
            s.total_duration_ns = s.total_duration_ns.saturating_add(ns);
            s.max_duration_ns = s.max_duration_ns.max(ns);
            let bin = DURATION_LIMITS_NS.iter().position(|&limit| ns <= limit).unwrap_or(DURATION_LIMITS_NS.len());
            if let Some(count) = s.duration_histogram.get_mut(bin) {
                *count = count.saturating_add(1);
                if let Some(published) = counters.duration_histogram.get(bin) {
                    published.store(*count, Ordering::Relaxed);
                }
            }
            if frames > 0 && rate > 0 {
                let used = u128::from(ns) * u128::from(rate);
                let budget = frames as u128 * 1_000_000_000;
                let load = (used * 1_000_000 / budget).min(u128::from(u64::MAX)) as u64;
                s.max_load_ppm = s.max_load_ppm.max(load);
                slot = load.min(u64::from(u32::MAX)) | if used > budget { OVER_BIT } else { 0 };
                s.over_budget_callbacks = s.over_budget_callbacks.saturating_add(u64::from(used > budget));
                s.over_budget_while_creating = s.over_budget_while_creating.saturating_add(u64::from(used > budget && creating));
            }
            counters.timed_callbacks.store(s.timed_callbacks, Ordering::Relaxed);
            counters.total_duration_ns.store(s.total_duration_ns, Ordering::Relaxed);
            counters.max_duration_ns.store(s.max_duration_ns, Ordering::Relaxed);
            counters.max_load_ppm.store(s.max_load_ppm, Ordering::Relaxed);
            counters.over_budget_callbacks.store(s.over_budget_callbacks, Ordering::Relaxed);
            counters.over_budget_while_creating.store(s.over_budget_while_creating, Ordering::Relaxed);
        }
        // The slot first: a reader that sees this callback counted also sees its load.
        if let Some(cell) = counters.recent.0.get((s.callbacks.saturating_sub(1) % RECENT_SLOTS as u64) as usize) {
            cell.store(slot, Ordering::Relaxed);
        }
        counters.last_frames.store(frames as u64, Ordering::Relaxed);
        counters.last_rate.store(u64::from(rate), Ordering::Relaxed);
        counters.frames.store(s.frames, Ordering::Relaxed);
        counters.callbacks.store(s.callbacks, Ordering::Relaxed);
    }
}

/// std::Instant is not implemented on wasm32-unknown-unknown. Do not call it there or invent
/// timing from the nominal WebAudio buffer size. A browser clock can be added separately.
pub(crate) struct Timer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
}

impl Timer {
    pub fn start() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
        }
    }

    pub fn elapsed(self) -> Option<Duration> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Some(self.start.elapsed())
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_follow_actual_callback_frames_and_device_rate() {
        let counters = Counters::default();
        let mut accumulate = Accumulator::default();
        for (frames, rate, ns) in [(48, 48_000, 1_000_000), (96, 48_000, 1_500_000), (96, 96_000, 1_500_000), (48, 48_000, 1_000_001)] {
            accumulate.record(&counters, frames, rate, Some(Duration::from_nanos(ns)));
        }
        let s = counters.snapshot();
        assert_eq!((s.callbacks, s.frames, s.timed_callbacks), (4, 288, 4));
        assert_eq!((s.total_duration_ns, s.max_duration_ns), (5_000_001, 1_500_000));
        assert_eq!((s.over_budget_callbacks, s.max_load_ppm), (2, 1_500_000));
        assert_eq!(s.duration_histogram.iter().sum::<u64>(), 4);
        assert_eq!(s.backend_errors, 0, "inferred overruns are not backend reports");
    }

    #[test]
    fn histogram_boundaries_and_zero_or_unknown_budgets() {
        let counters = Counters::default();
        let mut accumulate = Accumulator::default();
        for ns in [0, 1_000, 1_001, 16_384_000, 16_384_001, u64::MAX] {
            accumulate.record(&counters, 0, 48_000, Some(Duration::from_nanos(ns)));
        }
        accumulate.record(&counters, 64, 0, Some(Duration::MAX));
        accumulate.record(&counters, 64, 48_000, None);
        let s = counters.snapshot();
        assert_eq!((s.callbacks, s.timed_callbacks, s.frames), (8, 7, 128));
        assert_eq!((s.over_budget_callbacks, s.max_load_ppm), (0, 0));
        assert_eq!((s.total_duration_ns, s.max_duration_ns), (u64::MAX, u64::MAX));
        assert_eq!(s.duration_histogram, [2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 3]);
    }

    #[test]
    fn error_notifications_do_not_invent_over_budget_callbacks() {
        let counters = Counters::default();
        counters.error(false);
        counters.error(true);
        let s = counters.snapshot();
        assert_eq!((s.backend_errors, s.device_lost_errors), (2, 1));
        assert_eq!((s.callbacks, s.over_budget_callbacks), (0, 0));
    }

    #[test]
    fn startup_work_remains_visible_in_lifetime_peaks() {
        let counters = Counters::default();
        let mut accumulate = Accumulator::default();
        accumulate.record(&counters, 48, 48_000, Some(Duration::from_millis(2)));
        for _ in 0..128 {
            accumulate.record(&counters, 48, 48_000, Some(Duration::from_micros(10)));
        }
        let s = counters.snapshot();
        assert_eq!((s.max_duration_ns, s.max_load_ppm, s.over_budget_callbacks), (2_000_000, 2_000_000, 1));
        assert_eq!(Counters::default().snapshot(), CallbackTelemetry::default());
    }

    #[test]
    fn ui_reads_do_not_reset_or_contend_with_callback_publication() {
        let counters = std::sync::Arc::new(Counters::default());
        let writer = std::sync::Arc::clone(&counters);
        let worker = std::thread::spawn(move || {
            let mut accumulate = Accumulator::default();
            for _ in 0..20_000 {
                accumulate.record(&writer, 48, 48_000, Some(Duration::from_micros(500)));
            }
        });
        while !worker.is_finished() {
            let s = counters.snapshot();
            assert!(s.callbacks <= 20_000);
            assert_eq!(s.over_budget_callbacks, 0);
        }
        worker.join().unwrap();
        let s = counters.snapshot();
        assert_eq!((s.callbacks, s.frames, s.timed_callbacks), (20_000, 960_000, 20_000));
        assert_eq!(s.duration_histogram.iter().sum::<u64>(), 20_000);
    }
}

#[cfg(test)]
mod recent_tests {
    use super::*;

    #[test]
    fn current_load_follows_recent_callbacks_and_decays_while_the_peak_stays() {
        let counters = Counters::default();
        let mut accumulate = Accumulator::default();
        // 4800 frames at 48 kHz: a 100 ms budget, so a window of 10 callbacks.
        let run = |accumulate: &mut Accumulator, ms: u64| accumulate.record(&counters, 4800, 48_000, Some(Duration::from_millis(ms)));
        run(&mut accumulate, 150);
        run(&mut accumulate, 120);
        let s = counters.snapshot();
        assert_eq!((s.current_load_ppm, s.recent_over_budget, s.max_load_ppm), (1_500_000, 2, 1_500_000));
        assert_eq!((s.callback_frames, s.sample_rate), (4800, 48_000));
        for _ in 0..5 {
            run(&mut accumulate, 10);
        }
        let s = counters.snapshot();
        assert_eq!((s.current_load_ppm, s.recent_over_budget), (1_500_000, 2), "still inside the window");
        for _ in 0..95 {
            run(&mut accumulate, 10);
        }
        let s = counters.snapshot();
        assert_eq!((s.current_load_ppm, s.recent_over_budget), (100_000, 0), "the window moved past both");
        assert_eq!((s.max_load_ppm, s.over_budget_callbacks), (1_500_000, 2), "the lifetime figures stay");
    }

    #[test]
    fn the_window_covers_about_one_second_of_callbacks() {
        let counters = Counters::default();
        let mut accumulate = Accumulator::default();
        accumulate.record(&counters, 128, 48_000, Some(Duration::from_millis(5)));
        for _ in 0..374 {
            accumulate.record(&counters, 128, 48_000, Some(Duration::from_micros(100)));
        }
        let s = counters.snapshot();
        assert_eq!((s.recent_over_budget, s.current_load_ppm), (1, 1_875_000), "375 callbacks of 2.67 ms is one second");
        accumulate.record(&counters, 128, 48_000, Some(Duration::from_micros(100)));
        assert_eq!(counters.snapshot().recent_over_budget, 0);
    }

    #[test]
    fn recording_a_callback_does_not_allocate() {
        use transit_rt::testing::measure;
        let counters = Counters::default();
        let mut accumulate = Accumulator::default();
        let (_, stats) = measure(|| {
            for i in 0..3000u64 {
                accumulate.record_overlap(&counters, 128, 48_000, Some(Duration::from_micros(i % 4000)), i % 7 == 0);
            }
        });
        assert!(stats.is_zero());
        assert_eq!(counters.snapshot().callbacks, 3000);
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;

    #[test]
    fn a_peak_reset_restarts_the_peak_but_not_the_counts() {
        let counters = Counters::default();
        let mut accumulate = Accumulator::default();
        accumulate.record(&counters, 48, 48_000, Some(Duration::from_millis(2)));
        counters.reset_peak();
        accumulate.record(&counters, 48, 48_000, Some(Duration::from_micros(100)));
        let s = counters.snapshot();
        assert_eq!((s.max_load_ppm, s.max_duration_ns), (100_000, 100_000));
        assert_eq!((s.over_budget_callbacks, s.callbacks), (1, 2));
        accumulate.record(&counters, 48, 48_000, Some(Duration::from_micros(50)));
        assert_eq!(counters.snapshot().max_load_ppm, 100_000, "one reset applies once");
    }
}
