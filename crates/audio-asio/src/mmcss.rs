//! MMCSS for the thread that runs the buffer switch. ASIO calls back on a thread the driver owns,
//! so it cannot be promoted when it starts, as the WASAPI worker is: the first buffer switch after
//! each driver start registers its own thread with the MMCSS task "Pro Audio"
//! (`AvSetMmThreadCharacteristicsW`, once per start: one OS call, not per buffer), unless the
//! driver calls back on the host thread, which stays as it is. When the driver stops, the host
//! asks that thread to revert at its next buffer switch (`AvRevertMmThreadCharacteristics` on the
//! registered thread, where MMCSS expects it) and waits a few buffers for it; a driver that does
//! not call again by then is reverted from the host thread, and the result is logged.
//!
//! All of it is atomics: the callback side never blocks, allocates or locks. The OS calls are
//! [`Hooks`] ([`crate::com::MMCSS`] for real, counting fakes in tests). The state is what
//! `Stream::thread_priority` reports.
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use transit_audio_backend::WorkerPriority;

/// The OS calls, as plain functions so the callback can make them without allocation.
#[derive(Clone, Copy)]
pub(crate) struct Hooks {
    /// Registers the calling thread with "Pro Audio": its handle, or the OS error.
    pub(crate) register: fn() -> Result<usize, u32>,
    /// Reverts the registration `handle`: Ok, or the OS error.
    pub(crate) revert: fn(usize) -> Result<(), u32>,
    /// The calling thread's id.
    pub(crate) thread_id: fn() -> u32,
}

/// Not started, or stopped and nothing registered.
const OFF: u8 = 0;
/// The driver started: the next buffer switch registers its thread.
const ARMED: u8 = 1;
/// A buffer switch or the host is registering or reverting right now.
const BUSY: u8 = 2;
const REGISTERED: u8 = 3;
const FAILED: u8 = 4;
/// The driver called back on the host thread: not registered.
const HOST_THREAD: u8 = 5;
/// The driver is stopping: the registered thread reverts at its next buffer switch.
const RELEASE: u8 = 6;
const RELEASED: u8 = 7;
const RELEASE_FAILED: u8 = 8;

/// One driver session's registration, shared by the callback, the session and its streams.
pub(crate) struct Mmcss {
    state: AtomicU8,
    handle: AtomicUsize,
    thread: AtomicU32,
    /// The OS error of the last failed call, 0 when none failed.
    error: AtomicU32,
    host_thread: u32,
    hooks: Hooks,
}

/// What a release ended with, for the host's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Released {
    /// Nothing was registered.
    Nothing,
    /// Reverted by the registered thread at its last buffer switch.
    OnItsThread,
    /// The driver did not call again: reverted from the host thread.
    FromHost,
    /// The revert failed with this OS error.
    Failed(u32),
    /// A buffer switch was still registering when the wait ended; nothing was reverted.
    Abandoned,
}

impl Mmcss {
    /// A registration for a session whose host thread is the calling thread.
    pub(crate) fn new(hooks: Hooks) -> Self {
        Self {
            state: AtomicU8::new(OFF),
            handle: AtomicUsize::new(0),
            thread: AtomicU32::new(0),
            error: AtomicU32::new(0),
            host_thread: (hooks.thread_id)(),
            hooks,
        }
    }

    /// Host thread, before the driver starts: the next buffer switch registers its thread.
    pub(crate) fn arm(&self) {
        let mut state = self.state.load(Ordering::Acquire);
        while !matches!(state, BUSY | REGISTERED | RELEASE) {
            match self.state.compare_exchange(state, ARMED, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => state = now,
            }
        }
    }

    /// Callback side, at each buffer switch: one atomic load unless there is work. Registers the
    /// thread once after a start, or reverts it when the host asked.
    pub(crate) fn tend(&self) {
        match self.state.load(Ordering::Acquire) {
            ARMED => {
                if self.state.compare_exchange(ARMED, BUSY, Ordering::AcqRel, Ordering::Acquire).is_err() {
                    return;
                }
                let me = (self.hooks.thread_id)();
                let next = if me == self.host_thread {
                    HOST_THREAD
                } else {
                    match (self.hooks.register)() {
                        Ok(handle) => {
                            self.handle.store(handle, Ordering::Relaxed);
                            self.thread.store(me, Ordering::Relaxed);
                            REGISTERED
                        }
                        Err(code) => {
                            self.error.store(code, Ordering::Relaxed);
                            FAILED
                        }
                    }
                };
                self.state.store(next, Ordering::Release);
            }
            RELEASE => {
                if (self.hooks.thread_id)() != self.thread.load(Ordering::Relaxed)
                    || self.state.compare_exchange(RELEASE, BUSY, Ordering::AcqRel, Ordering::Acquire).is_err()
                {
                    return;
                }
                let next = self.revert();
                self.state.store(next, Ordering::Release);
            }
            _ => {}
        }
    }

    fn revert(&self) -> u8 {
        match (self.hooks.revert)(self.handle.swap(0, Ordering::Relaxed)) {
            Ok(()) => RELEASED,
            Err(code) => {
                self.error.store(code, Ordering::Relaxed);
                RELEASE_FAILED
            }
        }
    }

    /// Host thread, before the driver's `stop`: asks the registered thread to revert at its next
    /// buffer switch and waits up to `wait` for it, then reverts from here.
    pub(crate) fn release(&self, wait: Duration) -> Released {
        let deadline = Instant::now() + wait;
        loop {
            match self.state.load(Ordering::Acquire) {
                REGISTERED => {
                    let _ = self.state.compare_exchange(REGISTERED, RELEASE, Ordering::AcqRel, Ordering::Acquire);
                }
                ARMED => {
                    if self.state.compare_exchange(ARMED, OFF, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        return Released::Nothing;
                    }
                }
                RELEASE if Instant::now() >= deadline => {
                    if self.state.compare_exchange(RELEASE, BUSY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        let next = self.revert();
                        self.state.store(next, Ordering::Release);
                        return if next == RELEASED { Released::FromHost } else { Released::Failed(self.error.load(Ordering::Relaxed)) };
                    }
                }
                BUSY if Instant::now() >= deadline => return Released::Abandoned,
                RELEASE | BUSY => std::thread::sleep(Duration::from_millis(1)),
                RELEASED => return Released::OnItsThread,
                RELEASE_FAILED => return Released::Failed(self.error.load(Ordering::Relaxed)),
                _ => return Released::Nothing,
            }
        }
    }

    /// What `Stream::thread_priority` reports.
    pub(crate) fn priority(&self) -> WorkerPriority {
        match self.state.load(Ordering::Acquire) {
            REGISTERED | RELEASE => WorkerPriority::MmcssProAudio,
            FAILED => WorkerPriority::PromotionFailed,
            HOST_THREAD => WorkerPriority::Unobserved,
            RELEASED => WorkerPriority::Released,
            RELEASE_FAILED => WorkerPriority::ReleaseFailed,
            _ => WorkerPriority::Pending,
        }
    }

    /// The OS error of the last failed registration or revert, 0 when none failed.
    pub(crate) fn error(&self) -> u32 {
        self.error.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Counting hooks, process-wide (the tests that stream hold one lock).
    use super::Hooks;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    pub(crate) static REGISTERS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static REVERTS: AtomicUsize = AtomicUsize::new(0);
    /// The thread of the last registration and of the last revert.
    pub(crate) static REGISTERED_ON: AtomicU32 = AtomicU32::new(0);
    pub(crate) static REVERTED_ON: AtomicU32 = AtomicU32::new(0);
    /// The next registration fails (with error 5, access denied).
    pub(crate) static FAIL: AtomicBool = AtomicBool::new(false);

    fn register() -> Result<usize, u32> {
        if FAIL.swap(false, Ordering::SeqCst) {
            return Err(5);
        }
        REGISTERED_ON.store(crate::com::current_thread_id(), Ordering::SeqCst);
        Ok(0x1000 + REGISTERS.fetch_add(1, Ordering::SeqCst))
    }

    fn revert(handle: usize) -> Result<(), u32> {
        assert!(handle >= 0x1000, "a handle register returned: {handle:#x}");
        REVERTED_ON.store(crate::com::current_thread_id(), Ordering::SeqCst);
        REVERTS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) const HOOKS: Hooks = Hooks { register, revert, thread_id: crate::com::current_thread_id };

    /// Zeroes the counters.
    pub(crate) fn reset() {
        for counter in [&REGISTERS, &REVERTS] {
            counter.store(0, Ordering::SeqCst);
        }
        for thread in [&REGISTERED_ON, &REVERTED_ON] {
            thread.store(0, Ordering::SeqCst);
        }
        FAIL.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static REVERTED: AtomicUsize = AtomicUsize::new(0);
    static THREAD: AtomicU32 = AtomicU32::new(1);

    fn hooks() -> Hooks {
        Hooks {
            register: || {
                CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(7)
            },
            revert: |handle| {
                assert_eq!(handle, 7);
                REVERTED.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            thread_id: || THREAD.load(Ordering::SeqCst),
        }
    }

    /// The host is thread 1, the driver's thread 2 (one test: the hooks are static).
    #[test]
    fn registers_once_per_start_on_the_drivers_thread_and_reverts_there() {
        THREAD.store(1, Ordering::SeqCst);
        let m = Mmcss::new(hooks());
        assert_eq!(m.priority(), WorkerPriority::Pending);
        m.tend();
        assert_eq!(CALLS.load(Ordering::SeqCst), 0, "not armed: nothing");
        m.arm();
        THREAD.store(2, Ordering::SeqCst);
        m.tend();
        m.tend();
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "once per start");
        assert_eq!(m.priority(), WorkerPriority::MmcssProAudio);
        // The release asks; the driver's next switch on thread 2 reverts.
        let releaser = std::thread::scope(|s| {
            let handle = s.spawn(|| m.release(Duration::from_secs(5)));
            while m.state.load(Ordering::SeqCst) != RELEASE {
                std::thread::yield_now();
            }
            m.tend();
            handle.join().unwrap()
        });
        assert_eq!(releaser, Released::OnItsThread);
        assert_eq!((REVERTED.load(Ordering::SeqCst), m.priority()), (1, WorkerPriority::Released));
        // A new start registers again; a driver that does not call at the stop is reverted from
        // the host after the wait.
        m.arm();
        m.tend();
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        THREAD.store(1, Ordering::SeqCst);
        assert_eq!(m.release(Duration::from_millis(20)), Released::FromHost);
        assert_eq!(REVERTED.load(Ordering::SeqCst), 2);
        // A driver that never called back after a start: nothing to revert.
        m.arm();
        assert_eq!(m.release(Duration::from_millis(20)), Released::Nothing);
        assert_eq!(m.priority(), WorkerPriority::Pending);
        // A driver that calls back on the host thread is left as it is.
        m.arm();
        m.tend();
        assert_eq!((CALLS.load(Ordering::SeqCst), m.priority()), (2, WorkerPriority::Unobserved));
        assert_eq!(m.release(Duration::from_millis(20)), Released::Nothing);
    }

    #[test]
    fn a_refused_registration_is_reported_and_not_retried_in_that_run() {
        static TRIES: AtomicUsize = AtomicUsize::new(0);
        let m = Mmcss::new(Hooks {
            register: || {
                TRIES.fetch_add(1, Ordering::SeqCst);
                Err(1552)
            },
            revert: |_| panic!("nothing to revert"),
            thread_id: || 9,
        });
        let m = Mmcss { host_thread: 1, ..m };
        m.arm();
        m.tend();
        m.tend();
        assert_eq!((TRIES.load(Ordering::SeqCst), m.priority(), m.error()), (1, WorkerPriority::PromotionFailed, 1552));
        assert_eq!(m.release(Duration::from_millis(5)), Released::Nothing);
    }
}
