//! The stream watchdog: notices a driver that stopped calling `bufferSwitch` while its streams
//! should run. It is a timer on the host thread (checked between requests, the thread waking at
//! least every [`Watchdog::tick`] while the driver runs), never work on the callback thread: the
//! callback only stores the QPC of its entry, which the host thread compares with the clock here.
//!
//! The limit is [`STALL_PERIODS`] buffers, at least [`STALL_FLOOR`]; until the first callback after
//! a start the driver gets at least [`FIRST_CALL_GRACE`], since some take a moment to begin. It
//! fires once per driver start. Plain logic over QPC readings in 100 ns units, so its tests use a
//! fake clock.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use std::time::Duration;

/// Buffers without a callback before the stream counts as stalled.
pub(crate) const STALL_PERIODS: u64 = 8;
/// The shortest stall limit, whatever the buffer size.
pub(crate) const STALL_FLOOR: Duration = Duration::from_millis(250);
/// The least time the first callback after a start may take.
pub(crate) const FIRST_CALL_GRACE: Duration = Duration::from_secs(1);

fn units(d: Duration) -> u64 {
    u64::try_from(d.as_nanos() / 100).unwrap_or(u64::MAX)
}

/// One driver session's watchdog, owned by the host thread.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Watchdog {
    /// The stall limit in 100 ns units.
    limit: u64,
    /// The limit before the first callback of a start.
    first: u64,
    /// The QPC (100 ns) of the driver's start, while it runs.
    armed_at: Option<u64>,
    fired: bool,
}

impl Watchdog {
    /// A watchdog for buffers of `frames` at `rate`, with `floor` as the shortest limit.
    pub(crate) fn new(frames: usize, rate: u32, floor: Duration) -> Self {
        let period = u64::try_from(frames).unwrap_or(u64::MAX).saturating_mul(10_000_000) / u64::from(rate.max(1));
        let limit = period.saturating_mul(STALL_PERIODS).max(units(floor));
        Self { limit, first: limit.max(units(FIRST_CALL_GRACE)), armed_at: None, fired: false }
    }

    /// The stall limit.
    pub(crate) fn limit(&self) -> Duration {
        Duration::from_nanos(self.limit.saturating_mul(100))
    }

    /// The driver started at `now` (QPC, 100 ns).
    pub(crate) fn arm(&mut self, now: u64) {
        self.armed_at = Some(now);
        self.fired = false;
    }

    /// The driver stopped: nothing is due.
    pub(crate) fn disarm(&mut self) {
        self.armed_at = None;
    }

    /// How often the host thread should look while the driver runs: a quarter of the limit, at
    /// least 10 ms; `None` while it does not run (the host thread then sleeps until a request).
    pub(crate) fn tick(&self) -> Option<Duration> {
        self.armed_at.filter(|_| !self.fired).map(|_| Duration::from_nanos(self.limit.saturating_mul(25)).max(Duration::from_millis(10)))
    }

    /// True once (per start) when the driver has not called back for longer than the limit at
    /// `now`. `last_switch` is the QPC of the last callback's entry, 0 before the first since
    /// [`Self::arm`]; `running` says whether any stream of the driver is started (a driver that
    /// runs only for streams already stopped is not watched).
    pub(crate) fn check(&mut self, now: u64, last_switch: u64, running: bool) -> bool {
        let Some(armed_at) = self.armed_at else { return false };
        if self.fired || !running {
            return false;
        }
        let (since, limit) = if last_switch > armed_at { (last_switch, self.limit) } else { (armed_at, self.first) };
        if now.saturating_sub(since) > limit {
            self.fired = true;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 10_000;

    #[test]
    fn the_limit_is_eight_buffers_with_a_250_ms_floor() {
        assert_eq!(Watchdog::new(64, 48_000, STALL_FLOOR).limit(), Duration::from_millis(250), "8 x 1.33 ms is under the floor");
        assert_eq!(Watchdog::new(2048, 48_000, STALL_FLOOR).limit(), Duration::from_nanos(341_332_800), "8 x 42.7 ms");
        assert_eq!(Watchdog::new(256, 44_100, Duration::from_secs(5)).limit(), Duration::from_secs(5), "a test's longer floor");
    }

    #[test]
    fn a_driver_that_stops_calling_is_caught_once_after_the_limit() {
        let mut w = Watchdog::new(256, 48_000, STALL_FLOOR);
        assert!(!w.check(10_000 * MS, 0, true), "not armed: the driver is not running");
        assert_eq!(w.tick(), None);
        w.arm(1_000 * MS);
        assert_eq!(w.tick(), Some(Duration::from_nanos(62_500_000)), "a quarter of the limit");
        let mut last = 1_000 * MS;
        for step in 1..100 {
            last = 1_000 * MS + step * 5 * MS;
            assert!(!w.check(last + MS, last, true), "a callback every 5 ms");
        }
        assert!(!w.check(last + 250 * MS, last, true), "exactly the limit is not over it");
        assert!(w.check(last + 251 * MS, last, true), "a quarter second and more without one");
        assert!(!w.check(last + 900 * MS, last, true), "said once");
        assert_eq!(w.tick(), None, "nothing more to watch until the next start");
        w.arm(5_000 * MS);
        assert!(w.check(5_000 * MS + 2_000 * MS, 0, true), "a new start is watched again");
    }

    #[test]
    fn the_first_callback_gets_a_second_and_a_stopped_driver_nothing() {
        let mut w = Watchdog::new(64, 48_000, STALL_FLOOR);
        w.arm(0);
        assert!(!w.check(900 * MS, 0, true), "no callback yet, but within the start grace");
        assert!(w.check(1_001 * MS, 0, true));
        w.arm(10_000 * MS);
        assert!(!w.check(10_000 * MS + 5_000 * MS, 0, false), "no stream started: not watched");
        w.disarm();
        assert!(!w.check(u64::MAX, 0, true));
    }

    #[test]
    fn a_callback_from_before_the_start_does_not_count() {
        let mut w = Watchdog::new(64, 48_000, STALL_FLOOR);
        w.arm(2_000 * MS);
        assert!(!w.check(2_500 * MS, 1_990 * MS, true), "the last run's callback: the start grace applies");
        assert!(w.check(3_001 * MS, 1_990 * MS, true));
    }
}
