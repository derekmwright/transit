//! Linux: `SCHED_FIFO` at a moderate priority through `pthread_setschedparam`.
//!
//! Needs `CAP_SYS_NICE` or an `RLIMIT_RTPRIO` of at least [`FIFO_PRIORITY`] (the usual
//! `@audio - rtprio 95` in `/etc/security/limits.d`). Without either, the kernel answers `EPERM`
//! and the result is [`Promotion::Denied`]. Asking rtkit over D-Bus is out of scope for this crate
//! (it would add a D-Bus dependency); a desktop without realtime limits gets `Denied` and the
//! thread keeps its normal priority.
//!
//! Compiled and tested in the Linux CI leg (ADR 0013, section 5); device-level hardware
//! measurements remain separate from these scheduling tests.

use crate::{Level, Promotion};
use std::cell::Cell;

/// The `SCHED_FIFO` priority: moderate, below the kernel's threaded IRQ handlers (50 on
/// `PREEMPT_RT`) and below JACK/PipeWire's driver threads, so the audio device's own thread wins.
pub(crate) const FIFO_PRIORITY: i32 = 40;

/// What this crate changed on the calling thread.
#[derive(Clone, Copy)]
struct Saved {
    /// The successful promotion, returned again by a repeated `promote`.
    promotion: Option<Promotion>,
    /// The policy and priority before the change.
    previous: (libc::c_int, libc::c_int),
}

const NOTHING: Saved = Saved { promotion: None, previous: (libc::SCHED_OTHER, 0) };

std::thread_local! {
    static SAVED: Cell<Saved> = const { Cell::new(NOTHING) };
}

/// An all-zero `sched_param` (musl's has private padding fields, so no struct literal).
fn zeroed_param() -> libc::sched_param {
    // SAFETY: `sched_param` is a C struct of plain integers (plus integer padding on musl), for
    // which all-zero bytes are a valid value.
    unsafe { std::mem::zeroed() }
}

/// The calling thread's policy and priority, or the error code.
fn get() -> Result<(libc::c_int, libc::c_int), libc::c_int> {
    let mut policy: libc::c_int = 0;
    let mut param = zeroed_param();
    // SAFETY: `pthread_self()` is the calling thread, alive for the call; `policy` and `param`
    // are live, writable locals of the types the function fills in.
    let rc = unsafe { libc::pthread_getschedparam(libc::pthread_self(), &raw mut policy, &raw mut param) };
    if rc == 0 { Ok((policy, param.sched_priority)) } else { Err(rc) }
}

/// Set the calling thread's policy and priority; the error code on failure.
fn set(policy: libc::c_int, priority: libc::c_int) -> Result<(), libc::c_int> {
    let mut param = zeroed_param();
    param.sched_priority = priority;
    // SAFETY: `pthread_self()` is the calling thread; `param` is a live, initialized
    // `sched_param` read by the call. An invalid policy or priority is reported as an error
    // code, not undefined behavior.
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), policy, &raw const param) };
    if rc == 0 { Ok(()) } else { Err(rc) }
}

pub(crate) fn promote() -> Promotion {
    let saved = SAVED.get();
    if let Some(p) = saved.promotion {
        return p;
    }
    let previous = match get() {
        Ok(v) => v,
        Err(e) => return Promotion::Denied(e),
    };
    // SAFETY: `sched_get_priority_min`/`max` only read their integer argument.
    let (lo, hi) = unsafe { (libc::sched_get_priority_min(libc::SCHED_FIFO), libc::sched_get_priority_max(libc::SCHED_FIFO)) };
    if lo < 0 || hi < lo {
        return Promotion::Unsupported("SCHED_FIFO priority range unavailable");
    }
    let priority = FIFO_PRIORITY.clamp(lo, hi);
    match set(libc::SCHED_FIFO, priority) {
        Ok(()) => {
            let p = Promotion::Priority(priority);
            SAVED.set(Saved { promotion: Some(p), previous });
            p
        }
        Err(e) => Promotion::Denied(e),
    }
}

pub(crate) fn demote() {
    let saved = SAVED.replace(NOTHING);
    if saved.promotion.is_some() {
        let (policy, priority) = saved.previous;
        // Best effort: going back to a lower class is always permitted for the thread itself.
        let _ = set(policy, priority);
    }
}

pub(crate) fn current_level() -> Option<Level> {
    let (policy, priority) = get().ok()?;
    Some(Level { priority, realtime: policy == libc::SCHED_FIFO || policy == libc::SCHED_RR })
}
