//! The shared counters of one stream's device loop, exported through `Stream::stats`. Created
//! when the stream opens, on the control thread; the loop only does relaxed atomic adds, maxima and
//! minima on preallocated cells (no allocation, no lock). The reader takes a snapshot on any thread.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use transit_audio_backend::{STATS_BINS, StreamStats};

/// The padding cell before any wake read one.
const NO_PADDING: u32 = u32::MAX;

/// Live counters of a render or capture loop.
pub(crate) struct LoopStats {
    wakes: AtomicU64,
    timeouts: AtomicU64,
    skipped_wakes: AtomicU64,
    faults: AtomicU64,
    wake_late: [AtomicU64; STATS_BINS],
    max_wake_late_us: AtomicU64,
    service: [AtomicU64; STATS_BINS],
    max_service_us: AtomicU64,
    min_padding: AtomicU32,
    max_packets_per_wake: AtomicU32,
}

impl LoopStats {
    pub(crate) fn new() -> Self {
        Self {
            wakes: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            skipped_wakes: AtomicU64::new(0),
            faults: AtomicU64::new(0),
            wake_late: std::array::from_fn(|_| AtomicU64::new(0)),
            max_wake_late_us: AtomicU64::new(0),
            service: std::array::from_fn(|_| AtomicU64::new(0)),
            max_service_us: AtomicU64::new(0),
            min_padding: AtomicU32::new(NO_PADDING),
            max_packets_per_wake: AtomicU32::new(0),
        }
    }

    pub(crate) fn wake(&self) {
        self.wakes.fetch_add(1, Relaxed);
    }

    pub(crate) fn timeout(&self) {
        self.timeouts.fetch_add(1, Relaxed);
    }

    pub(crate) fn skipped(&self) {
        self.skipped_wakes.fetch_add(1, Relaxed);
    }

    pub(crate) fn fault(&self) {
        self.faults.fetch_add(1, Relaxed);
    }

    /// A wake `late_100ns` after it was due (QPC units of 100 ns).
    pub(crate) fn wake_late(&self, late_100ns: u64) {
        record(&self.wake_late, &self.max_wake_late_us, late_100ns / 10);
    }

    /// The work of a wake took `elapsed_100ns`.
    pub(crate) fn service(&self, elapsed_100ns: u64) {
        record(&self.service, &self.max_service_us, elapsed_100ns / 10);
    }

    pub(crate) fn padding(&self, frames: u32) {
        self.min_padding.fetch_min(frames, Relaxed);
    }

    pub(crate) fn packets(&self, count: u32) {
        self.max_packets_per_wake.fetch_max(count, Relaxed);
    }

    pub(crate) fn snapshot(&self) -> StreamStats {
        let padding = self.min_padding.load(Relaxed);
        StreamStats {
            wakes: self.wakes.load(Relaxed),
            timeouts: self.timeouts.load(Relaxed),
            skipped_wakes: self.skipped_wakes.load(Relaxed),
            faults: self.faults.load(Relaxed),
            wake_late_us: load(&self.wake_late),
            max_wake_late_us: self.max_wake_late_us.load(Relaxed),
            service_us: load(&self.service),
            max_service_us: self.max_service_us.load(Relaxed),
            min_padding_frames: if padding == NO_PADDING { 0 } else { padding },
            max_packets_per_wake: self.max_packets_per_wake.load(Relaxed),
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
    fn values_land_in_their_bins_with_exact_maxima() {
        let stats = LoopStats::new();
        stats.wake_late(100); // 10 us
        stats.wake_late(160); // 16 us
        stats.wake_late(5_000_000); // 500 ms
        stats.service(3_000);
        stats.padding(300);
        stats.padding(120);
        stats.padding(900);
        stats.packets(2);
        stats.packets(1);
        let s = stats.snapshot();
        assert_eq!((s.wake_late_us[0], s.wake_late_us[1], s.wake_late_us[15]), (1, 1, 1));
        assert_eq!(s.max_wake_late_us, 500_000);
        assert_eq!((s.service_us[5], s.max_service_us), (1, 300), "300 us is under 512");
        assert_eq!((s.min_padding_frames, s.max_packets_per_wake), (120, 2));
    }

    #[test]
    fn an_unread_padding_snapshots_as_zero() {
        assert_eq!(LoopStats::new().snapshot(), StreamStats::default());
    }
}
