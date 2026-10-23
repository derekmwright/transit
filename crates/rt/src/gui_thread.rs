//! The OS message queue of a thread that hosts windows it does not get from the UI toolkit: the
//! plug-in host thread (ADR 0018). Plug-in editors and the hidden windows plug-in frameworks
//! create at load time belong to the thread that created them, and that thread must dispatch
//! their messages or they freeze.
//!
//! | Platform | [`MessageQueue::attach`] | [`MessageQueue::wait`] | [`MessageQueue::dispatch`] |
//! |---|---|---|---|
//! | Windows | `OleInitialize` (a single-threaded apartment, as winit gives the UI thread) and the thread's message queue | `MsgWaitForMultipleObjectsEx` on any input | `PeekMessageW`, `TranslateMessage`, `DispatchMessageW` |
//! | elsewhere | nothing | `std::thread::park_timeout` | nothing (no window here needs dispatching) |
//!
//! Contract, on every platform:
//! - Everything acts on the **calling** thread: attach, wait and dispatch on the thread that
//!   owns the windows. [`MessageQueue`] is not `Send`, so it cannot leave that thread, and its
//!   drop (which ends the apartment) runs there.
//! - [`QueueWaker::wake`] may be called from any thread. It makes the next or the current
//!   [`MessageQueue::wait`] return early; wakes coalesce, so a burst posts one OS message.
//! - Nothing here panics. A wait never blocks longer than its timeout.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The calling thread's message queue (see the module docs). Not `Send`: it belongs to the
/// thread that attached it.
pub struct MessageQueue {
    shared: Arc<Shared>,
    /// Windows: `OleInitialize` succeeded, so drop must call `OleUninitialize`.
    apartment: bool,
    _not_send: PhantomData<*const ()>,
}

/// Wakes a thread waiting in [`MessageQueue::wait`]; `Send`, `Sync` and cheap to clone.
#[derive(Clone)]
pub struct QueueWaker {
    shared: Arc<Shared>,
}

struct Shared {
    /// A wake is pending that no wait has consumed yet.
    woken: AtomicBool,
    thread: std::thread::Thread,
    /// Windows: the OS id of the owning thread, for `PostThreadMessageW` (0 elsewhere).
    os_thread: u32,
}

impl MessageQueue {
    /// Makes the calling thread a window-hosting thread: on Windows, initializes OLE (an STA)
    /// and creates the thread's message queue so a wake can be posted before the first wait.
    #[must_use]
    pub fn attach() -> MessageQueue {
        let (apartment, os_thread) = imp::attach();
        MessageQueue {
            shared: Arc::new(Shared { woken: AtomicBool::new(false), thread: std::thread::current(), os_thread }),
            apartment,
            _not_send: PhantomData,
        }
    }

    /// Whether the thread joined a single-threaded OLE apartment (Windows only; `false`
    /// elsewhere and when OLE refused, for example because the thread was already in a
    /// multithreaded apartment).
    #[must_use]
    pub fn apartment(&self) -> bool {
        self.apartment
    }

    /// A waker for this queue, to hand to the threads that send it work.
    #[must_use]
    pub fn waker(&self) -> QueueWaker {
        QueueWaker { shared: Arc::clone(&self.shared) }
    }

    /// Dispatches up to `max` pending window messages of this thread to their window procedures
    /// and returns how many it took. Window procedures (plug-in code) run inside this call.
    // The count is diagnostic; a pump loop ignores it.
    #[allow(clippy::must_use_candidate)]
    pub fn dispatch(&self, max: usize) -> usize {
        imp::dispatch(max)
    }

    /// Returns when a window message arrives for this thread, a [`QueueWaker::wake`] happens (or
    /// happened since the last wait), or `timeout` passes, whichever is first.
    pub fn wait(&self, timeout: Duration) {
        if self.shared.woken.swap(false, Ordering::AcqRel) {
            imp::consume();
            return;
        }
        imp::wait(timeout);
        self.shared.woken.store(false, Ordering::Release);
    }
}

impl Drop for MessageQueue {
    fn drop(&mut self) {
        imp::detach(self.apartment);
    }
}

impl QueueWaker {
    /// Makes the owning thread's current or next [`MessageQueue::wait`] return. Any thread.
    pub fn wake(&self) {
        if !self.shared.woken.swap(true, Ordering::AcqRel) {
            imp::post(self.shared.os_thread);
            self.shared.thread.unpark();
        }
    }
}

#[cfg(all(windows, not(feature = "force-fallback")))]
mod imp {
    use std::time::Duration;
    use windows_sys::Win32::System::Ole::{OleInitialize, OleUninitialize};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
        QS_ALLINPUT, TranslateMessage, WM_NULL, WM_QUIT, WM_USER,
    };

    pub(super) fn attach() -> (bool, u32) {
        // SAFETY: `OleInitialize` takes a reserved pointer that must be null and acts on the
        // calling thread only; S_OK and S_FALSE both mean a reference this thread must release
        // with `OleUninitialize` (done in `detach`, on this thread, because `MessageQueue` is not
        // `Send`); a failure (RPC_E_CHANGED_MODE) takes no reference.
        let hr = unsafe { OleInitialize(std::ptr::null()) };
        let mut msg = empty_msg();
        // SAFETY: `msg` is a valid, writable MSG; a null window and a WM_USER-only filter with
        // PM_NOREMOVE only make Windows create this thread's message queue (documented for
        // PostThreadMessage) and leave every message where it is.
        unsafe { PeekMessageW(&raw mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE) };
        // SAFETY: no arguments, no preconditions.
        let id = unsafe { GetCurrentThreadId() };
        (hr >= 0, id)
    }

    pub(super) fn detach(apartment: bool) {
        if apartment {
            // SAFETY: balances the successful `OleInitialize` of `attach`, on the same thread
            // (`MessageQueue` is not `Send`, so its drop runs where it was attached).
            unsafe { OleUninitialize() };
        }
    }

    fn empty_msg() -> MSG {
        MSG { hwnd: std::ptr::null_mut(), message: 0, wParam: 0, lParam: 0, time: 0, pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 } }
    }

    pub(super) fn dispatch(max: usize) -> usize {
        let mut done = 0;
        while done < max {
            let mut msg = empty_msg();
            // SAFETY: `msg` is a valid, writable MSG; a null window with no filter takes any
            // message of the calling thread's queue (window and thread messages alike).
            if unsafe { PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 {
                break;
            }
            done += 1;
            if msg.message == WM_QUIT {
                // Plug-in code asked this thread to quit; the host decides when it ends.
                continue;
            }
            // SAFETY: `msg` was just filled by `PeekMessageW` on this thread, which is what
            // `TranslateMessage` and `DispatchMessageW` require; a thread message (null window)
            // is ignored by `DispatchMessageW`.
            unsafe {
                TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        done
    }

    pub(super) fn wait(timeout: Duration) {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: zero handles with a null array is the documented way to wait on the message
        // queue alone; MWMO_INPUTAVAILABLE also returns for input already in the queue.
        unsafe { MsgWaitForMultipleObjectsEx(0, std::ptr::null(), ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
    }

    /// The wake's posted message stays queued for `dispatch`; nothing else to consume.
    pub(super) fn consume() {}

    pub(super) fn post(thread: u32) {
        // SAFETY: posts a WM_NULL thread message (no pointers in it) to the owning thread's
        // queue, created in `attach`. A failure (the thread ended, or its queue is full) only
        // means no early wake; the wait still ends at its timeout.
        unsafe { PostThreadMessageW(thread, WM_NULL, 0, 0) };
    }
}

#[cfg(any(not(windows), feature = "force-fallback"))]
mod imp {
    use std::time::Duration;

    pub(super) fn attach() -> (bool, u32) {
        (false, 0)
    }

    pub(super) fn detach(_apartment: bool) {}

    pub(super) fn dispatch(_max: usize) -> usize {
        0
    }

    pub(super) fn wait(timeout: Duration) {
        std::thread::park_timeout(timeout);
    }

    /// Takes the unpark token of the wake that ended a wait early, so the next wait waits.
    pub(super) fn consume() {
        std::thread::park_timeout(Duration::ZERO);
    }

    pub(super) fn post(_thread: u32) {}
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_wake_before_the_wait_returns_at_once() {
        let q = MessageQueue::attach();
        q.waker().wake();
        let t = Instant::now();
        q.wait(Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(2));
        // Consumed (and the wake's message dispatched): the next wait runs to its timeout.
        q.dispatch(64);
        let t = Instant::now();
        q.wait(Duration::from_millis(50));
        assert!(t.elapsed() >= Duration::from_millis(30));
        q.dispatch(64);
    }

    #[test]
    fn a_wake_from_another_thread_ends_a_wait() {
        let q = MessageQueue::attach();
        let w = q.waker();
        let t = Instant::now();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            w.wake();
        });
        q.wait(Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(2));
        h.join().unwrap();
    }

    #[test]
    fn an_empty_queue_dispatches_nothing() {
        let q = MessageQueue::attach();
        while q.dispatch(64) > 0 {}
        assert_eq!(q.dispatch(64), 0);
    }

    #[cfg(all(windows, not(feature = "force-fallback")))]
    #[test]
    fn a_posted_thread_message_is_taken_by_dispatch_and_ole_is_entered() {
        let q = MessageQueue::attach();
        assert!(q.apartment(), "a fresh test thread can enter an STA");
        q.waker().wake();
        q.wait(Duration::from_secs(1));
        assert!(q.dispatch(64) >= 1, "the wake's WM_NULL is in the queue");
    }
}
