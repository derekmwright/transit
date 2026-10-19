#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread::{self, ThreadId};

struct DropSpy(mpsc::Sender<ThreadId>);

impl Drop for DropSpy {
    fn drop(&mut self) {
        self.0.send(thread::current().id()).unwrap();
    }
}

#[test]
fn exclusive_access_accepts_send_but_not_sync_values() {
    fn is_send<T: Send>() {}
    is_send::<AudioAccess<Cell<u32>>>();
    let (anchor, mut audio) = Anchor::new(Cell::new(7));
    let audio = thread::spawn(move || {
        audio.enter().unwrap().set(19);
        audio
    })
    .join()
    .unwrap();
    drop(audio);
    assert_eq!(anchor.take().get(), 19);
}

#[test]
fn close_is_permanent_even_when_a_guard_exits_later() {
    let (anchor, mut audio) = Anchor::new(10);
    let mut guard = audio.enter().unwrap();
    anchor.close();
    anchor.close();
    *guard += 1;
    assert!(!anchor.shared.admission.is_idle());
    drop(guard);
    anchor.quiesce();
    anchor.quiesce();
    for _ in 0..100 {
        assert!(audio.enter().is_none());
    }
    assert_eq!(anchor.take(), 11);
    assert!(audio.enter().is_none());
}

#[test]
fn busy_admission_fails_without_changing_participation() {
    let admission = Admission::default();
    assert!(admission.enter());
    assert!(!admission.enter());
    assert!(!admission.is_idle());
    admission.close();
    assert!(!admission.enter());
    assert!(!admission.is_idle());
    admission.leave();
    assert!(admission.is_idle());
    assert!(!admission.enter());
}

#[test]
fn callback_unwind_releases_and_does_not_poison() {
    let (anchor, mut audio) = Anchor::new(0);
    let audio = thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut guard = audio.enter().unwrap();
            *guard = 41;
            panic!("synthetic callback panic");
        }));
        assert!(result.is_err());
        *audio.enter().unwrap() += 1;
        audio
    })
    .join()
    .unwrap();
    assert_eq!(anchor.take(), 42);
    drop(audio);
}

#[test]
fn unwind_after_close_keeps_admission_closed() {
    let (anchor, mut audio) = Anchor::new(3);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = audio.enter().unwrap();
        anchor.close();
        *guard = 4;
        panic!("synthetic closed callback panic");
    }));
    assert!(result.is_err());
    assert!(anchor.shared.admission.is_idle());
    assert!(audio.enter().is_none());
    assert_eq!(anchor.take(), 4);
}

#[test]
fn failed_open_endpoint_drop_still_allows_main_take() {
    let main = thread::current().id();
    let (tx, rx) = mpsc::channel();
    let (anchor, audio) = Anchor::new(DropSpy(tx));
    thread::spawn(move || drop(audio)).join().unwrap();
    assert!(rx.try_recv().is_err());
    let value = anchor.take();
    assert!(rx.try_recv().is_err());
    drop(value);
    assert_eq!(rx.recv().unwrap(), main);
    assert!(rx.try_recv().is_err());
}

#[test]
fn anchor_drop_reclaims_on_main_before_endpoint_drop() {
    let main = thread::current().id();
    let (tx, rx) = mpsc::channel();
    let (anchor, mut audio) = Anchor::new(DropSpy(tx));
    drop(anchor);
    assert_eq!(rx.recv().unwrap(), main);
    thread::spawn(move || {
        assert!(audio.enter().is_none());
        drop(audio);
    })
    .join()
    .unwrap();
    assert!(rx.try_recv().is_err());
}

#[test]
fn anchor_drop_reclaims_on_main_after_endpoint_drop() {
    let main = thread::current().id();
    let (tx, rx) = mpsc::channel();
    let (anchor, audio) = Anchor::new(DropSpy(tx));
    thread::spawn(move || drop(audio)).join().unwrap();
    assert!(rx.try_recv().is_err());
    drop(anchor);
    assert_eq!(rx.recv().unwrap(), main);
    assert!(rx.try_recv().is_err());
}

#[test]
fn quiesce_observes_writes_from_a_callback_inside_when_closed() {
    let (anchor, mut audio) = Anchor::new(0);
    let admission = Arc::clone(&anchor.shared);
    let (entered_tx, entered_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut guard = audio.enter().unwrap();
        entered_tx.send(()).unwrap();
        while admission.admission.0.load(Ordering::Relaxed) & CLOSED == 0 {
            thread::yield_now();
        }
        *guard = 123;
        drop(guard);
        assert!(audio.enter().is_none());
    });
    entered_rx.recv().unwrap();
    anchor.quiesce();
    // Check before join: publication must come from quiesce, not thread join.
    assert!(anchor.shared.admission.is_idle());
    assert_eq!(anchor.take(), 123);
    worker.join().unwrap();
}

#[test]
fn drop_waits_for_inside_callback_and_destroys_only_on_main() {
    let main = thread::current().id();
    let (tx, rx) = mpsc::channel();
    let (anchor, mut audio) = Anchor::new(DropSpy(tx));
    let shared = Arc::clone(&anchor.shared);
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = thread::spawn(move || {
        let guard = audio.enter().unwrap();
        worker_barrier.wait();
        while shared.admission.0.load(Ordering::Relaxed) & CLOSED == 0 {
            thread::yield_now();
        }
        assert!(!shared.admission.is_idle());
        drop(guard);
        assert!(audio.enter().is_none());
    });
    barrier.wait();
    drop(anchor);
    assert_eq!(rx.recv().unwrap(), main);
    worker.join().unwrap();
    assert!(rx.try_recv().is_err());
}

#[test]
fn real_threads_hammer_entry_while_main_closes() {
    let rounds = if cfg!(miri) { 3 } else { 64 };
    let attempts = if cfg!(miri) { 50 } else { 20_000 };
    for _ in 0..rounds {
        let (anchor, mut audio) = Anchor::new(0_usize);
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let closed = Arc::new(AtomicBool::new(false));
        let worker_closed = Arc::clone(&closed);
        let worker = thread::spawn(move || {
            let mut count = 0;
            // Guarantee actual successful callback access before racing close.
            *audio.enter().unwrap() += 1;
            count += 1;
            worker_barrier.wait();
            for _ in 0..attempts {
                if let Some(mut guard) = audio.enter() {
                    *guard += 1;
                    count += 1;
                }
            }
            while !worker_closed.load(Ordering::Acquire) {
                thread::yield_now();
            }
            assert!(audio.enter().is_none());
            count
        });
        barrier.wait();
        anchor.close();
        anchor.quiesce();
        let value = anchor.take();
        closed.store(true, Ordering::Release);
        assert_eq!(value, worker.join().unwrap());
    }
}

#[test]
fn entry_exit_and_closed_failure_allocate_and_free_nothing() {
    let (anchor, mut audio) = Anchor::new(0_u32);
    let ((), stats) = crate::testing::measure(|| {
        for _ in 0..1000 {
            let mut guard = audio.enter().unwrap();
            *guard += 1;
            std::hint::black_box(&mut *guard);
        }
    });
    assert!(stats.is_zero(), "{stats:?}");
    anchor.close();
    let ((), stats) = crate::testing::measure(|| {
        for _ in 0..1000 {
            assert!(audio.enter().is_none());
        }
    });
    assert!(stats.is_zero(), "{stats:?}");
    assert_eq!(anchor.take(), 1000);
}

#[test]
fn panicking_value_drop_cannot_reopen_or_drop_twice() {
    struct PanicDrop(Arc<AtomicUsize>);
    impl Drop for PanicDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
            panic!("synthetic destructor panic");
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let (anchor, mut audio) = Anchor::new(PanicDrop(Arc::clone(&drops)));
    assert!(catch_unwind(AssertUnwindSafe(|| drop(anchor))).is_err());
    assert!(audio.enter().is_none());
    drop(audio);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn zero_sized_and_borrowed_values() {
    let (anchor, mut audio) = Anchor::new(());
    drop(audio.enter().unwrap());
    let () = anchor.take();
    let mut value = 0;
    let (anchor, mut audio) = Anchor::new(&mut value);
    **audio.enter().unwrap() = 5;
    *anchor.take() += 1;
    drop(audio);
    assert_eq!(value, 6);
}

// Enumerate all schedules of two ordered actors: audio enter/leave twice and
// Main close/observe twice (70 schedules). Observation models one wait iteration;
// a busy observation never authorizes reclamation. Execute the production atomic
// transitions, checking against independent logical lifetime/ownership state.
// This explores atomic modification order, not weak-memory reorderings (not Loom).
fn model_schedule(schedule: &[bool]) {
    let admission = Admission::default();
    let mut audio_step = 0;
    let mut main_step = 0;
    let mut inside = false;
    let mut closed = false;
    let mut reclaimed = false;
    for &audio in schedule {
        if audio {
            if audio_step % 2 == 0 {
                let entered = admission.enter();
                assert_eq!(entered, !closed && !inside);
                assert!(!entered || !reclaimed, "entered reclaimed storage");
                inside = entered;
            } else if inside {
                admission.leave();
                inside = false;
            }
            audio_step += 1;
        } else {
            if main_step % 2 == 0 {
                admission.close();
                closed = true;
            } else if admission.is_idle() {
                assert!(!inside, "false quiescence while callback borrows");
                reclaimed = true;
            }
            main_step += 1;
        }
        assert_eq!(admission.0.load(Ordering::Relaxed) & CLOSED != 0, closed);
        assert_eq!(admission.is_idle(), !inside);
        assert!(!(inside && reclaimed));
    }
    assert!(!inside);
    assert!(closed);
    assert!(admission.is_idle());
    assert!(!admission.enter());
}

fn enumerate_schedules(schedule: &mut Vec<bool>, audio: usize, main: usize) -> usize {
    if audio == 4 && main == 4 {
        model_schedule(schedule);
        return 1;
    }
    let mut count = 0;
    for (is_audio, allowed) in [(true, audio < 4), (false, main < 4)] {
        if allowed {
            schedule.push(is_audio);
            count += enumerate_schedules(schedule, audio + usize::from(is_audio), main + usize::from(!is_audio));
            schedule.pop();
        }
    }
    count
}

#[test]
fn exhaustive_small_state_interleavings() {
    assert_eq!(enumerate_schedules(&mut Vec::new(), 0, 0), 70);
}
