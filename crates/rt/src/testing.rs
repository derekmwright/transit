//! Test support (feature `testing`, never in release builds): counts OS promotion attempts, and
//! [`CountingAlloc`], a global allocator that counts heap allocations and frees made on threads
//! a test marks, so a test can assert that a render path allocates nothing on the audio thread.
//!
//! A test binary opts in with its own `#[global_allocator]`; nothing else in the process changes:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: soundcraft_rt::testing::CountingAlloc = soundcraft_rt::testing::CountingAlloc;
//!
//! let ((), stats) = soundcraft_rt::testing::measure(|| render_one_block());
//! assert!(stats.is_zero());
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub(crate) static PROMOTION_CALLS: AtomicUsize = AtomicUsize::new(0);

/// How many times [`crate::promote_audio_thread`] has been called in this process, counted
/// before the platform call. A render path that must make no OS call asserts it stays put.
#[must_use]
pub fn promotion_calls() -> usize {
    PROMOTION_CALLS.load(Ordering::Relaxed)
}

/// Heap operations counted on marked threads. A `realloc` counts as one allocation and one free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocStats {
    /// Allocations (`alloc`, `alloc_zeroed`, `realloc`).
    pub allocs: u64,
    /// Frees (`dealloc`, `realloc`).
    pub frees: u64,
    /// Bytes requested by those allocations (the new size for a `realloc`).
    pub bytes: u64,
}

impl AllocStats {
    /// The counts since `earlier`, a snapshot taken before.
    #[must_use]
    pub fn since(self, earlier: AllocStats) -> AllocStats {
        AllocStats {
            allocs: self.allocs.wrapping_sub(earlier.allocs),
            frees: self.frees.wrapping_sub(earlier.frees),
            bytes: self.bytes.wrapping_sub(earlier.bytes),
        }
    }

    /// No allocation and no free.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.allocs == 0 && self.frees == 0
    }
}

// Per-thread state: `const`-initialized `Cell`s of `Copy` types have no destructor and need no
// lazy initialization, so touching them from inside the allocator never allocates or recurses.
std::thread_local! {
    static COUNTED: Cell<bool> = const { Cell::new(false) };
    static T_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static T_FREES: Cell<u64> = const { Cell::new(0) };
    static T_BYTES: Cell<u64> = const { Cell::new(0) };
}

static P_ALLOCS: AtomicU64 = AtomicU64::new(0);
static P_FREES: AtomicU64 = AtomicU64::new(0);
static P_BYTES: AtomicU64 = AtomicU64::new(0);

/// Start (`true`) or stop counting the heap operations made on the calling thread. Mark the thread
/// that plays the audio thread in a test, and any worker rendering for it.
pub fn count_this_thread(on: bool) {
    COUNTED.set(on);
}

/// What the calling thread has allocated and freed while it was counted.
#[must_use]
pub fn thread_stats() -> AllocStats {
    AllocStats { allocs: T_ALLOCS.get(), frees: T_FREES.get(), bytes: T_BYTES.get() }
}

/// What all counted threads of the process have allocated and freed.
#[must_use]
pub fn process_stats() -> AllocStats {
    AllocStats { allocs: P_ALLOCS.load(Ordering::Relaxed), frees: P_FREES.load(Ordering::Relaxed), bytes: P_BYTES.load(Ordering::Relaxed) }
}

/// Runs `f` with the calling thread counted and returns what it allocated and freed on this
/// thread. Other threads, counted or not, do not show up here. The thread's previous marking is
/// restored afterwards.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
    let was = COUNTED.get();
    let before = thread_stats();
    COUNTED.set(true);
    let r = f();
    COUNTED.set(was);
    (r, thread_stats().since(before))
}

fn record(allocs: u64, frees: u64, bytes: usize) {
    // `try_with`: while a thread is torn down its slots may be gone; those operations are not counted.
    if !COUNTED.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    let bytes = bytes as u64;
    let _ = T_ALLOCS.try_with(|c| c.set(c.get().wrapping_add(allocs)));
    let _ = T_FREES.try_with(|c| c.set(c.get().wrapping_add(frees)));
    let _ = T_BYTES.try_with(|c| c.set(c.get().wrapping_add(bytes)));
    P_ALLOCS.fetch_add(allocs, Ordering::Relaxed);
    P_FREES.fetch_add(frees, Ordering::Relaxed);
    P_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// A [`GlobalAlloc`] that forwards every call to [`System`] and counts the calls made on threads
/// marked with [`count_this_thread`] (or inside [`measure`]). Install it with
/// `#[global_allocator]` in a test binary only.
#[derive(Clone, Copy, Debug, Default)]
pub struct CountingAlloc;

// SAFETY: every method passes its exact arguments (pointer, layout, new size) to `System`, which
// implements the `GlobalAlloc` contract, and returns `System`'s result unchanged; every pointer
// this allocator hands out or takes back is therefore `System`'s, with `System`'s layout pairing.
// The counting around each call only touches `const`-initialized thread-locals of `Copy` types
// (no destructor, no lazy initialization, accessed through `try_with`) and relaxed atomics. It
// never allocates, so it cannot re-enter the allocator, and it never panics or unwinds.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(1, 0, layout.size());
        // SAFETY: the caller's contract for `alloc` (a non-zero-size layout) is passed on unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(1, 0, layout.size());
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(0, 1, 0);
        // SAFETY: the caller guarantees `ptr` came from this allocator with `layout`, and every
        // pointer this allocator returns came from `System` with that same layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(1, 1, new_size);
        // SAFETY: `ptr` and `layout` are `System`'s (see `dealloc`); the caller's contract on
        // `new_size` is passed on unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vec_allocation_on_this_thread_is_counted_only_while_counted() {
        let (v, made) = measure(|| std::hint::black_box(Vec::<u64>::with_capacity(32)));
        assert_eq!(made, AllocStats { allocs: 1, frees: 0, bytes: 32 * 8 });
        let ((), freed) = measure(|| drop(v));
        assert_eq!(freed, AllocStats { allocs: 0, frees: 1, bytes: 0 });
        let before = thread_stats();
        drop(std::hint::black_box(vec![1u8; 100]));
        assert_eq!(thread_stats(), before, "not counted outside `measure`");
        let ((), none) = measure(|| {
            std::hint::black_box([0u8; 64]);
        });
        assert!(none.is_zero(), "stack data is not a heap allocation");
    }

    #[test]
    fn a_marked_thread_counts_in_the_process_totals_not_in_other_threads() {
        let before_here = thread_stats();
        let before = process_stats();
        let own = std::thread::spawn(|| {
            count_this_thread(true);
            let ((), own) = measure(|| drop(std::hint::black_box(vec![0u8; 10])));
            count_this_thread(false);
            own
        })
        .join();
        assert!(matches!(own, Ok(AllocStats { allocs: 1, frees: 1, bytes: 10 })), "{own:?}");
        let total = process_stats().since(before);
        assert!(total.allocs >= 1 && total.frees >= 1, "{total:?}");
        assert_eq!(thread_stats(), before_here, "this thread was not counted");
    }
}
