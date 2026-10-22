//! Exclusive callback access with reclamation on the creating (Main) thread.
//!
//! One allocation is made by [`Anchor::new`]. Callback entry and exit use one atomic
//! read-modify-write each, with no allocation, locks, OS calls, or retry loops.
//! This portable protocol is identical on all supported platforms, including the
//! scheduling fallback and WASM; it needs no platform-specific OS implementation.
//!
//! `T: Send` permits exclusive access on another thread; `T: Sync` is not needed.
//! Main means the thread that creates the anchor, not an OS-verified UI thread.
//! The helper only reclaims its stored value on Main. As with any `&mut T` API,
//! callers must not replace/drop the value or its owned resources in a callback
//! if those destructors must run on Main. After `take`, destruction is the caller's
//! responsibility. Dropping the endpoint may free the empty shared allocation,
//! but never destroys the stored value.
//!
//! A leaked guard (for example, `mem::forget`) leaves participation set forever:
//! Main waits forever rather than reclaiming potentially borrowed memory. Never
//! quiesce, take, or drop the anchor while holding its guard on the same thread,
//! or while holding a resource that the callback needs to finish.
//!
//! ```
//! use transit_rt::handoff::Anchor;
//! let (anchor, mut audio) = Anchor::new(0_u64);
//! if let Some(mut guard) = audio.enter() { *guard += 1; }
//! anchor.close();
//! anchor.quiesce();
//! assert!(audio.enter().is_none());
//! assert_eq!(anchor.take(), 1);
//! ```

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const INSIDE: usize = 1;
const CLOSED: usize = 2;

#[derive(Default)]
struct Admission(AtomicUsize);

impl Admission {
    fn enter(&self) -> bool {
        // Acquire pairs with the preceding guard's Release exit. Initial publication
        // is provided by transferring the endpoint through Rust's thread primitives.
        // Failure touches no value, so Relaxed suffices. Strong CAS: one attempt only.
        self.0.compare_exchange(0, INSIDE, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    fn leave(&self) {
        // Release publishes every value access to Main's Acquire idle observation
        // (or a later entry). Preserve CLOSED even when Main closed during this guard.
        self.0.fetch_and(!INSIDE, Ordering::Release);
    }

    fn close(&self) {
        // Only atomic modification order is needed to exclude subsequent entries.
        // Preserve INSIDE: closing admission is not evidence of quiescence.
        self.0.fetch_or(CLOSED, Ordering::Relaxed);
    }

    fn is_idle(&self) -> bool {
        // Acquire observes the final Release exit, including via close's RMW release
        // sequence. After close, coherence forbids observing an earlier open state.
        self.0.load(Ordering::Acquire) & INSIDE == 0
    }
}

struct Shared<T> {
    admission: Admission,
    // MaybeUninit has no T destructor: the last Arc may be dropped on audio.
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: T is Send; admission grants exclusive access and Main alone reclaims.
unsafe impl<T: Send> Send for Shared<T> {}
// SAFETY: admission serializes T access; Main reclaims only after all guards exit.
unsafe impl<T: Send> Sync for Shared<T> {}

/// Main's owner; neither `Send` nor `Sync`. Dropping it closes, waits, and drops `T`.
///
/// ```compile_fail
/// use transit_rt::handoff::Anchor;
/// fn send<T: Send>() {}
/// send::<Anchor<u32>>();
/// ```
/// ```compile_fail
/// use transit_rt::handoff::Anchor;
/// fn sync<T: Sync>() {}
/// sync::<Anchor<u32>>();
/// ```
pub struct Anchor<T> {
    shared: Arc<Shared<T>>,
    owns_value: bool,
    main: PhantomData<Rc<()>>,
}

impl<T: Send> Anchor<T> {
    /// Allocate the value on Main and create its sole audio endpoint.
    ///
    /// ```compile_fail
    /// use transit_rt::handoff::Anchor;
    /// let _ = Anchor::new(std::rc::Rc::new(1)); // not transferable to audio
    /// ```
    #[must_use]
    pub fn new(value: T) -> (Self, AudioAccess<T>) {
        let shared = Arc::new(Shared { admission: Admission::default(), value: UnsafeCell::new(MaybeUninit::new(value)) });
        (Self { shared: Arc::clone(&shared), owns_value: true, main: PhantomData }, AudioAccess { shared })
    }
}

impl<T> Anchor<T> {
    /// Permanently close admission. An already admitted callback may still be inside.
    pub fn close(&self) {
        self.shared.admission.close();
    }

    /// Close admission and wait on Main until no callback is inside.
    ///
    /// Idempotent; requires no further callbacks or endpoint drop. May wait forever
    /// if a guard is leaked or its callback never finishes. Only Main yields.
    pub fn quiesce(&self) {
        self.close();
        while !self.shared.admission.is_idle() {
            std::thread::yield_now();
        }
    }

    /// Close, wait, and return the value on Main; the caller now owns its destruction.
    #[must_use]
    pub fn take(mut self) -> T {
        self.quiesce();
        self.owns_value = false;
        // SAFETY: quiescence excludes audio; owns_value prevents a second reclamation.
        unsafe { (*self.shared.value.get()).assume_init_read() }
    }
}

impl<T> Drop for Anchor<T> {
    fn drop(&mut self) {
        if self.owns_value {
            self.quiesce();
            self.owns_value = false;
            // SAFETY: quiescence excludes audio; the initialized value is dropped once on Main.
            unsafe { (*self.shared.value.get()).assume_init_drop() }
        }
    }
}

/// The single, non-clone endpoint. `Send` when `T: Send`; never owns `T`'s destruction.
///
/// ```compile_fail
/// use transit_rt::handoff::Anchor;
/// let (_, audio) = Anchor::new(1);
/// let _ = audio.clone();
/// ```
pub struct AudioAccess<T> {
    shared: Arc<Shared<T>>,
}

impl<T> AudioAccess<T> {
    /// Try once to borrow the value exclusively for this callback.
    ///
    /// Returns `None` if closed or already inside. The guard borrows this endpoint,
    /// so safe callers cannot overlap callbacks. No allocation, OS calls, or retry.
    pub fn enter(&mut self) -> Option<Guard<'_, T>> {
        if !self.shared.admission.enter() {
            return None;
        }
        // SAFETY: successful admission exclusively borrows initialized T until guard release.
        let value = unsafe { (*self.shared.value.get()).assume_init_mut() };
        Some(Guard { value, admission: &self.shared.admission, main: PhantomData })
    }
}

/// An exclusive callback borrow. Drop releases participation, including during unwind.
///
/// The guard is neither `Send` nor `Sync`; its borrow cannot outlive the endpoint.
/// Forgetting it prevents reclamation forever, preserving memory safety.
///
/// ```compile_fail
/// use transit_rt::handoff::Guard;
/// fn send<T: Send>() {}
/// send::<Guard<'static, u32>>();
/// ```
/// ```compile_fail
/// use transit_rt::handoff::Anchor;
/// let (_, mut audio) = Anchor::new(1);
/// let mut first = audio.enter().unwrap();
/// let _second = audio.enter();
/// *first += 1;
/// ```
pub struct Guard<'a, T> {
    value: &'a mut T,
    admission: &'a Admission,
    main: PhantomData<Rc<()>>,
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.admission.leave();
    }
}

#[cfg(test)]
mod tests;
