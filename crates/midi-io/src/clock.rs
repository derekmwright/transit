//! Host time H: monotonic process time in integer nanoseconds from an explicit origin
//! (clock annex, "clock domains").

use std::sync::OnceLock;
use std::time::Instant;

static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// The process-wide default origin of host time H for this crate, fixed at its first use.
///
/// Every [`InputEvent::host_arrival_ns`](crate::InputEvent::host_arrival_ns) is measured from
/// the origin in the port's [`InputConfig`](crate::InputConfig), which defaults to this one. A
/// runtime with its own origin either passes it in the config (then no mapping is needed) or maps
/// with `origin_ns_of_ours = host_origin() - runtime_origin`. Call it on a control thread; the
/// input callback never reads it (the origin is copied into each open port).
#[must_use]
pub fn host_origin() -> Instant {
    *ORIGIN.get_or_init(Instant::now)
}

/// Nanoseconds from `origin` to now; 0 if `origin` is in the future, `u64::MAX` after ~584 years.
/// Allocation-free and lock-free (one `Instant::now`, QueryPerformanceCounter on Windows).
#[must_use]
pub fn host_ns_since(origin: Instant) -> u64 {
    u64::try_from(Instant::now().saturating_duration_since(origin).as_nanos()).unwrap_or(u64::MAX)
}

/// [`host_ns_since`] the default [`host_origin`].
#[must_use]
pub fn host_now_ns() -> u64 {
    host_ns_since(host_origin())
}
