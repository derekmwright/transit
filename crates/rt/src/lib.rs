//! Realtime OS support for the audio engine: raise an audio worker thread to the operating
//! system's realtime scheduling class, and put it back.
//!
//! ADR 0013: all new `unsafe` in the fork lives in this crate, behind the safe functions below.
//! Callers (the mix render pool) stay `unsafe`-free. Each platform has its own module:
//!
//! | Platform | Mechanism | Fallback inside the platform |
//! |---|---|---|
//! | Windows | MMCSS task "Pro Audio" (`AvSetMmThreadCharacteristicsW`) at `AVRT_PRIORITY_CRITICAL` | `SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)` |
//! | Linux | `pthread_setschedparam(SCHED_FIFO, 40)` | none: `EPERM` is reported as [`Promotion::Denied`] (rtkit is out of scope) |
//! | macOS | Mach `THREAD_TIME_CONSTRAINT_POLICY` (untested on hardware) | none |
//! | anything else | nothing | [`Promotion::Unsupported`], logged once |
//!
//! The `force-fallback` feature selects the portable fallback on every platform.
//!
//! Contract, on every platform:
//! - Nothing here panics or fails the build. A refused or unsupported promotion is a value.
//! - The functions act on the **calling** thread only. Call them from the thread to promote
//!   (the render pool's worker start handler), never on behalf of another thread.
//! - [`promote_audio_thread`] is meant for thread start. The OS call may allocate inside the
//!   kernel or the OS library (MMCSS registers the thread with a service); this crate allocates
//!   nothing itself, on any call. A second call on an already promoted thread makes no OS call
//!   and returns the first result.
//! - Per-thread state (what to restore) is a `const`-initialized thread-local `Cell`, which needs
//!   no heap allocation.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod handoff;

/// Test support: counts OS promotion attempts and heap allocations ([`testing::CountingAlloc`]).
/// Not in release builds (feature `testing`).
#[cfg(any(test, feature = "testing"))]
pub mod testing;

// This crate's own unit-test binary counts allocations; no other binary is affected.
#[cfg(test)]
#[global_allocator]
static TEST_ALLOC: testing::CountingAlloc = testing::CountingAlloc;

#[cfg(all(windows, not(feature = "force-fallback")))]
mod windows;
#[cfg(all(windows, not(feature = "force-fallback")))]
use windows as imp;

#[cfg(all(target_os = "linux", not(feature = "force-fallback")))]
mod linux;
#[cfg(all(target_os = "linux", not(feature = "force-fallback")))]
use linux as imp;

#[cfg(all(target_os = "macos", not(feature = "force-fallback")))]
mod macos;
#[cfg(all(target_os = "macos", not(feature = "force-fallback")))]
use macos as imp;

// Compiled on every platform under `cfg(test)`, so its unit tests run everywhere (ADR 0013, 5).
#[cfg(any(test, feature = "force-fallback", not(any(windows, target_os = "linux", target_os = "macos"))))]
mod fallback;
#[cfg(any(feature = "force-fallback", not(any(windows, target_os = "linux", target_os = "macos"))))]
use fallback as imp;

/// What [`promote_audio_thread`] achieved on the calling thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Promotion {
    /// Windows: the thread joined the MMCSS "Pro Audio" task (`task_index` is the index MMCSS
    /// assigned); `critical` says whether the critical priority within the task was granted.
    Mmcss {
        /// The MMCSS task index returned by `AvSetMmThreadCharacteristicsW`.
        task_index: u32,
        /// True when `AvSetMmThreadPriority(AVRT_PRIORITY_CRITICAL)` was granted; false means
        /// the thread is in the task at the task's default priority.
        critical: bool,
    },
    /// A raised OS priority: on Windows the `SetThreadPriority` level (15, time critical) used
    /// when MMCSS is unavailable; on Linux the `SCHED_FIFO` priority.
    Priority(i32),
    /// macOS: the Mach time-constraint policy, in nanoseconds (computation and constraint; the
    /// period is 0, "not periodic").
    TimeConstraint {
        /// CPU time the thread may need in each constraint window, in nanoseconds.
        computation_ns: u32,
        /// The window the computation must finish in, in nanoseconds.
        constraint_ns: u32,
    },
    /// No realtime mechanism on this platform (or build); the thread is unchanged.
    Unsupported(&'static str),
    /// The OS refused (the value is the OS error: `GetLastError` on Windows, the `errno`-style
    /// return code on Linux, the `kern_return_t` on macOS). The thread is unchanged.
    Denied(i32),
}

impl Promotion {
    /// True when the thread now runs in a raised or realtime scheduling class.
    ///
    /// ```
    /// use transit_rt::Promotion;
    /// assert!(Promotion::Priority(15).is_promoted());
    /// assert!(!Promotion::Unsupported("platform").is_promoted());
    /// assert!(!Promotion::Denied(1).is_promoted());
    /// ```
    #[must_use]
    pub const fn is_promoted(&self) -> bool {
        matches!(self, Self::Mmcss { .. } | Self::Priority(_) | Self::TimeConstraint { .. })
    }
}

/// The calling thread's scheduling level as the OS reports it now (for tests and telemetry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    /// The OS priority value: `GetThreadPriority` on Windows, the `pthread_getschedparam`
    /// priority on Linux and macOS.
    pub priority: i32,
    /// True when the thread is in a realtime class: MMCSS-registered or time-critical on Windows,
    /// `SCHED_FIFO`/`SCHED_RR` on Linux, the time-constraint policy on macOS.
    pub realtime: bool,
}

/// Raise the calling thread to the platform's realtime audio class and say what was achieved.
///
/// Call it once, at thread start (the render pool's start handler). It never panics. A second
/// call on a thread already promoted makes no OS call and returns the first result; a call after
/// a failed attempt tries again. The crate allocates nothing; the OS call itself may (see the
/// crate docs), so never call it from inside a render or an audio callback.
///
/// ```
/// let p = transit_rt::promote_audio_thread();
/// // On a supported platform with the rights to do it, `p.is_promoted()`; otherwise the thread
/// // is unchanged and `p` says why.
/// transit_rt::demote(p);
/// ```
#[must_use = "the result says whether the thread is realtime; pass it to `demote` to undo"]
pub fn promote_audio_thread() -> Promotion {
    #[cfg(any(test, feature = "testing"))]
    testing::PROMOTION_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    imp::promote()
}

/// Undo [`promote_audio_thread`] on the calling thread: leave MMCSS, or restore the scheduling
/// class and priority the thread had before.
///
/// Pass the value `promote_audio_thread` returned on this same thread. A value that is not a
/// promotion, or a call on a thread this crate did not promote, does nothing. Never panics.
///
/// ```
/// let p = transit_rt::promote_audio_thread();
/// transit_rt::demote(p);
/// transit_rt::demote(p); // the second call does nothing
/// ```
pub fn demote(p: Promotion) {
    if p.is_promoted() {
        imp::demote();
    }
}

/// The calling thread's scheduling level as the OS reports it, or `None` where the platform has
/// no way to ask (the fallback) or the query failed.
///
/// ```
/// if let Some(level) = transit_rt::current_level() {
///     println!("priority {} realtime {}", level.priority, level.realtime);
/// }
/// ```
#[must_use]
pub fn current_level() -> Option<Level> {
    imp::current_level()
}
