//! macOS: the Mach `THREAD_TIME_CONSTRAINT_POLICY` (the class Core Audio's own I/O thread uses).
//!
//! Compiled and tested in both macOS CI legs (ADR 0013, section 5). Device-level hardware
//! measurements remain separate from these scheduling tests.
//!
//! The policy needs no privilege. Its numbers are in Mach absolute time units, converted from
//! nanoseconds with `mach_timebase_info`. Period 0 means "not periodic": the render workers
//! wake when a block is dispatched, not on a fixed clock. If a thread overruns its computation
//! budget repeatedly, the kernel demotes it to normal scheduling; that is the OS's safety valve
//! and is acceptable here.

use crate::{Level, Promotion};
use std::cell::Cell;

/// CPU time a worker may use per wake-up before it can be preempted: 5 ms.
const COMPUTATION_NS: u32 = 5_000_000;
/// The window that computation must complete in: 10 ms (more than a 256-frame block at 48 kHz).
const CONSTRAINT_NS: u32 = 10_000_000;

/// What this crate changed on the calling thread.
#[derive(Clone, Copy)]
struct Saved {
    /// The successful promotion, returned again by a repeated `promote`.
    promotion: Option<Promotion>,
    /// The time-constraint policy the thread had before, when it already had one (else the
    /// thread goes back to the standard policy).
    previous: Option<libc::thread_time_constraint_policy>,
}

const NOTHING: Saved = Saved { promotion: None, previous: None };

std::thread_local! {
    static SAVED: Cell<Saved> = const { Cell::new(NOTHING) };
}

/// The calling thread's Mach port. `pthread_mach_thread_np` does not add a port reference, so
/// there is nothing to deallocate (unlike `mach_thread_self`).
fn this_thread() -> libc::thread_t {
    // SAFETY: `pthread_self()` is the calling thread, alive for the call; the function only maps
    // it to its Mach port.
    unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
}

/// Nanoseconds to Mach absolute time units, saturating; `None` if the timebase is unavailable.
///
/// `libc` deprecates `mach_timebase_info` (the struct and the function) in favor of `mach2`,
/// which would be a second binding crate for one call; only those uses are allowed below.
fn ns_to_abs(ns: u32) -> Option<u32> {
    #[allow(deprecated)]
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: `tb` is a live, writable `mach_timebase_info` the call fills in.
    #[allow(deprecated)]
    let rc = unsafe { libc::mach_timebase_info(&raw mut tb) };
    #[allow(deprecated)]
    let (numer, denom) = (tb.numer, tb.denom);
    if rc != libc::KERN_SUCCESS || numer == 0 {
        return None;
    }
    let abs = u64::from(ns) * u64::from(denom) / u64::from(numer);
    Some(u32::try_from(abs).unwrap_or(u32::MAX))
}

/// The thread's current time-constraint policy, `Ok(None)` when it has the default (it is not
/// time-constrained), or the `kern_return_t` on failure.
fn get_policy() -> Result<Option<libc::thread_time_constraint_policy>, libc::kern_return_t> {
    let mut policy = libc::thread_time_constraint_policy { period: 0, computation: 0, constraint: 0, preemptible: 0 };
    let mut count = libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    let mut get_default: libc::boolean_t = 0;
    // SAFETY: `policy` is a live, writable `thread_time_constraint_policy`, and `count` holds its
    // size in `integer_t` units, so the kernel writes at most that many; `get_default` is a live,
    // writable `boolean_t`; the thread port is the calling thread's.
    let rc = unsafe {
        libc::thread_policy_get(
            this_thread(),
            libc::THREAD_TIME_CONSTRAINT_POLICY.unsigned_abs(),
            (&raw mut policy).cast(),
            &raw mut count,
            &raw mut get_default,
        )
    };
    if rc != libc::KERN_SUCCESS {
        return Err(rc);
    }
    Ok(if get_default == 0 { Some(policy) } else { None })
}

/// Apply a time-constraint policy to the calling thread.
fn set_policy(mut policy: libc::thread_time_constraint_policy) -> Result<(), libc::kern_return_t> {
    // SAFETY: `policy` is a live, initialized `thread_time_constraint_policy` and the count is its
    // size in `integer_t` units; the kernel only reads it. The thread port is the calling
    // thread's.
    let rc = unsafe {
        libc::thread_policy_set(
            this_thread(),
            libc::THREAD_TIME_CONSTRAINT_POLICY.unsigned_abs(),
            (&raw mut policy).cast(),
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    if rc == libc::KERN_SUCCESS { Ok(()) } else { Err(rc) }
}

/// Put the calling thread back on the standard (timesharing) policy.
fn set_standard() {
    let mut standard = libc::thread_standard_policy { no_data: 0 };
    // SAFETY: `THREAD_STANDARD_POLICY` takes no data (count 0); the pointer refers to a live
    // value the kernel does not read at count 0. The thread port is the calling thread's.
    let _ = unsafe {
        libc::thread_policy_set(
            this_thread(),
            libc::THREAD_STANDARD_POLICY.unsigned_abs(),
            (&raw mut standard).cast(),
            libc::THREAD_STANDARD_POLICY_COUNT.unsigned_abs(),
        )
    };
}

pub(crate) fn promote() -> Promotion {
    let saved = SAVED.get();
    if let Some(p) = saved.promotion {
        return p;
    }
    let previous = match get_policy() {
        Ok(v) => v,
        Err(e) => return Promotion::Denied(e),
    };
    let (Some(computation), Some(constraint)) = (ns_to_abs(COMPUTATION_NS), ns_to_abs(CONSTRAINT_NS)) else {
        return Promotion::Unsupported("mach timebase unavailable");
    };
    let policy = libc::thread_time_constraint_policy { period: 0, computation, constraint, preemptible: 1 };
    match set_policy(policy) {
        Ok(()) => {
            let p = Promotion::TimeConstraint { computation_ns: COMPUTATION_NS, constraint_ns: CONSTRAINT_NS };
            SAVED.set(Saved { promotion: Some(p), previous });
            p
        }
        Err(e) => Promotion::Denied(e),
    }
}

pub(crate) fn demote() {
    let saved = SAVED.replace(NOTHING);
    if saved.promotion.is_none() {
        return;
    }
    match saved.previous {
        Some(previous) => {
            let _ = set_policy(previous);
        }
        None => set_standard(),
    }
}

pub(crate) fn current_level() -> Option<Level> {
    let realtime = get_policy().ok()?.is_some();
    let mut policy: libc::c_int = 0;
    // SAFETY: `sched_param` is a C struct of plain integers and bytes, for which all-zero bytes
    // are a valid value (its padding field is private, so no struct literal).
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    // SAFETY: `pthread_self()` is the calling thread; `policy` and `param` are live, writable
    // locals of the types the function fills in.
    let rc = unsafe { libc::pthread_getschedparam(libc::pthread_self(), &raw mut policy, &raw mut param) };
    if rc != 0 {
        return None;
    }
    Some(Level { priority: param.sched_priority, realtime })
}
