//! The shared counters of one stream, exported through `Stream::stats`, in the shape of
//! `transit-audio-wasapi`'s `stats.rs`. Created when the stream opens, on the host thread; the
//! buffer-switch callback and `asioMessage` only do relaxed atomic adds and maxima on preallocated
//! cells (no allocation, no lock). The reader takes a snapshot on any thread.
//!
//! ASIO has no wait and no padding: `skipped_wakes`, `min_padding_frames` and
//! `max_packets_per_wake` stay 0, and `timeouts` counts the watchdog's stalls (the driver stopped
//! calling back, [`crate::watchdog`]). The driver-event counters (`overloads`, `position_jumps`,
//! `resets`, `timeouts`) belong to the driver, so both directions of one driver share them
//! ([`DriverEvents`]).
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use transit_audio_backend::{STATS_BINS, StreamStats};

/// What the driver reported or the callback saw, once per driver (both directions share it).
#[derive(Default)]
pub(crate) struct DriverEvents {
    pub(crate) overloads: AtomicU64,
    pub(crate) position_jumps: AtomicU64,
    pub(crate) resets: AtomicU64,
    /// Stalls the watchdog found: the driver stopped calling back while a stream should run.
    pub(crate) timeouts: AtomicU64,
}

/// Live counters of one direction.
pub(crate) struct LoopStats {
    wakes: AtomicU64,
    faults: AtomicU64,
    wake_late: [AtomicU64; STATS_BINS],
    max_wake_late_us: AtomicU64,
    service: [AtomicU64; STATS_BINS],
    max_service_us: AtomicU64,
    events: Arc<DriverEvents>,
}

impl LoopStats {
    pub(crate) fn new(events: Arc<DriverEvents>) -> Self {
        Self {
            wakes: AtomicU64::new(0),
            faults: AtomicU64::new(0),
            wake_late: std::array::from_fn(|_| AtomicU64::new(0)),
            max_wake_late_us: AtomicU64::new(0),
            service: std::array::from_fn(|_| AtomicU64::new(0)),
            max_service_us: AtomicU64::new(0),
            events,
        }
    }

    pub(crate) fn wake(&self) {
        self.wakes.fetch_add(1, Relaxed);
    }

    pub(crate) fn fault(&self) {
        self.faults.fetch_add(1, Relaxed);
    }

    /// A callback `late_100ns` after it was due (QPC units of 100 ns).
    pub(crate) fn wake_late(&self, late_100ns: u64) {
        record(&self.wake_late, &self.max_wake_late_us, late_100ns / 10);
    }

    /// The callback took `elapsed_100ns` from entry to the end of its work.
    pub(crate) fn service(&self, elapsed_100ns: u64) {
        record(&self.service, &self.max_service_us, elapsed_100ns / 10);
    }

    pub(crate) fn snapshot(&self) -> StreamStats {
        StreamStats {
            wakes: self.wakes.load(Relaxed),
            faults: self.faults.load(Relaxed),
            wake_late_us: load(&self.wake_late),
            max_wake_late_us: self.max_wake_late_us.load(Relaxed),
            service_us: load(&self.service),
            max_service_us: self.max_service_us.load(Relaxed),
            overloads: self.events.overloads.load(Relaxed),
            position_jumps: self.events.position_jumps.load(Relaxed),
            resets: self.events.resets.load(Relaxed),
            timeouts: self.events.timeouts.load(Relaxed),
            ..StreamStats::default()
        }
    }
}

fn record(bins: &[AtomicU64; STATS_BINS], max: &AtomicU64, us: u64) {
    if let Some(bin) = bins.get(StreamStats::bin_of_us(us)) {
        bin.fetch_add(1, Relaxed);
    }
    max.fetch_max(us, Relaxed);
}

fn load(bins: &[AtomicU64; STATS_BINS]) -> [u64; STATS_BINS] {
    let mut out = [0; STATS_BINS];
    for (value, bin) in out.iter_mut().zip(bins) {
        *value = bin.load(Relaxed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_land_in_their_bins_and_the_driver_events_are_shared() {
        let events = Arc::new(DriverEvents::default());
        let (render, capture) = (LoopStats::new(Arc::clone(&events)), LoopStats::new(Arc::clone(&events)));
        render.wake_late(100); // 10 us
        render.wake_late(5_000_000); // 500 ms
        render.service(3_000);
        render.wake();
        events.overloads.fetch_add(2, Relaxed);
        events.resets.fetch_add(1, Relaxed);
        events.timeouts.fetch_add(1, Relaxed);
        let s = render.snapshot();
        assert_eq!((s.wakes, s.wake_late_us[0], s.wake_late_us[15], s.max_wake_late_us), (1, 1, 1, 500_000));
        assert_eq!((s.service_us[5], s.max_service_us), (1, 300));
        assert_eq!((s.overloads, s.position_jumps, s.resets, s.timeouts), (2, 0, 1, 1));
        assert_eq!((capture.snapshot().overloads, capture.snapshot().wakes), (2, 0), "one driver, one set of events");
        assert_eq!((s.skipped_wakes, s.min_padding_frames, s.max_packets_per_wake), (0, 0, 0));
    }
}
