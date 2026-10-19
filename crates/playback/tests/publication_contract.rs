//! Contract tests for the actual publication primitive and its exactly pinned queue.
#![forbid(unsafe_code)]

use soundcraft_playback::publication::{Service, channel};
use soundcraft_rt::testing::measure;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, ThreadId};
use std::time::Duration;

#[global_allocator]
static ALLOC: soundcraft_rt::testing::CountingAlloc = soundcraft_rt::testing::CountingAlloc;

const WATCHDOG: Duration = Duration::from_secs(5);

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct Trace {
    owner: ThreadId,
    local: AtomicUsize,
    foreign: AtomicUsize,
}

impl Trace {
    fn new() -> Arc<Self> {
        Arc::new(Self { owner: thread::current().id(), local: AtomicUsize::new(0), foreign: AtomicUsize::new(0) })
    }

    fn counts(&self) -> (usize, usize) {
        (self.local.load(Ordering::Relaxed), self.foreign.load(Ordering::Relaxed))
    }
}

struct Payload {
    id: usize,
    trace: Arc<Trace>,
    // Make the no-free check cover an actual owned heap resource, not just an Arc count.
    _storage: Vec<u8>,
}

impl Payload {
    fn new(id: usize, trace: &Arc<Trace>) -> Self {
        Self { id, trace: Arc::clone(trace), _storage: vec![0; 128] }
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        let counter = if thread::current().id() == self.trace.owner { &self.trace.local } else { &self.trace.foreign };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn a_stalled_reclaimer_exhausts_credits_even_after_forward_queue_is_empty() {
    for capacity in [1, 2, 4, 16] {
        let trace = Trace::new();
        let (mut control, mut audio) = channel(NonZeroUsize::new(capacity).unwrap());
        for id in 0..capacity {
            assert!(control.try_publish(Payload::new(id, &trace)).is_ok());
        }
        assert_eq!(control.credits(), 0);
        let audio = thread::spawn(move || {
            let ((), stats) = measure(|| {
                for id in 0..capacity {
                    assert_eq!(audio.service_one(|p| assert_eq!(p.id, id)), Service::Returned);
                }
                for _ in 0..1000 {
                    assert_eq!(audio.service_one(|_| panic!("empty ingress")), Service::Empty);
                }
            });
            assert!(stats.is_zero(), "{stats:?}");
            audio
        })
        .join()
        .unwrap();
        assert_eq!(trace.counts(), (0, 0));
        // Forward has room, but unreclaimed returns still own all credits.
        let unsent = control.try_publish(Payload::new(capacity, &trace)).err().unwrap();
        assert_eq!(unsent.id, capacity);
        assert_eq!(trace.counts(), (0, 0));
        drop(unsent);
        for expected in 0..capacity {
            assert!(control.reclaim_one(|p| assert_eq!(p.id, expected)));
            assert_eq!(control.credits(), expected + 1);
        }
        assert!(!control.reclaim_one(|_| panic!("no extra return")));
        drop(audio);
        drop(control);
        assert_eq!(trace.counts(), (capacity + 1, 0));
    }
}

#[test]
fn repeated_wraparound_push_pop_and_reclamation_allocate_and_free_nothing() {
    let (mut control, mut audio) = channel(NonZeroUsize::new(3).unwrap());
    let ((), stats) = measure(|| {
        for base in 0..10_000 {
            for offset in 0..3 {
                assert!(control.try_publish(base * 3 + offset).is_ok());
            }
            assert_eq!(control.try_publish(999), Err(999));
            for offset in 0..3 {
                assert_eq!(audio.service_one(|v| *v += 1), Service::Returned);
                assert!(control.reclaim_one(|v| assert_eq!(v, base * 3 + offset + 1)));
            }
            assert_eq!(control.credits(), 3);
        }
    });
    assert!(stats.is_zero(), "{stats:?}");
}

#[test]
fn an_interrupted_adoption_keeps_its_envelope_in_the_holder_until_return() {
    let trace = Trace::new();
    let (mut control, mut audio) = channel(NonZeroUsize::new(1).unwrap());
    assert!(control.try_publish(Payload::new(1, &trace)).is_ok());
    let audio = thread::spawn(move || {
        let fault = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audio.service_one(|p| {
                p.id = 2;
                panic!("injected adoption fault");
            });
        }));
        assert!(fault.is_err());
        let (result, stats) = measure(|| audio.service_one(|_| panic!("do not reapply after a fault")));
        assert_eq!(result, Service::Returned);
        assert!(stats.is_zero(), "{stats:?}");
        audio
    })
    .join()
    .unwrap();
    assert_eq!(trace.counts(), (0, 0));
    assert!(control.reclaim_one(|p| assert_eq!(p.id, 2)));
    assert_eq!(trace.counts(), (1, 0));
    drop(audio);
}

#[test]
fn shutdown_recovers_queued_returned_and_interrupted_owners_on_control() {
    for audio_first in [false, true] {
        let trace = Trace::new();
        let (mut control, mut audio) = channel(NonZeroUsize::new(3).unwrap());
        for id in 0..3 {
            assert!(control.try_publish(Payload::new(id, &trace)).is_ok());
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            assert_eq!(audio.service_one(|_| {}), Service::Returned);
            let fault = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                audio.service_one(|_| {
                    entered_tx.send(()).unwrap();
                    resume_rx.recv_timeout(WATCHDOG).unwrap();
                    panic!("leave one in-flight owner for shutdown");
                });
            }));
            assert!(fault.is_err());
            audio
        });
        entered_rx.recv_timeout(WATCHDOG).unwrap();
        assert_eq!(trace.counts(), (0, 0));
        // The handshake establishes an actual in-progress adoption; no quiet-time assumption.
        resume_tx.send(()).unwrap();
        let audio = worker.join().unwrap();
        assert_eq!(trace.counts(), (0, 0));
        if audio_first {
            drop(audio);
            drop(control);
        } else {
            drop(control);
            drop(audio);
        }
        assert_eq!(trace.counts(), (3, 0));
    }
}

#[test]
fn unpublished_dependency_slot_returns_empty_before_producer_resumes() {
    // The chunk API exposes the pre-publication boundary deterministically. This is a proxy
    // for push's payload-write / Release-tail sequence, verified in pinned 0.4.0 source;
    // it does not instrument a suspension inside Producer::push itself.
    let (mut producer, mut consumer) = rtrb::RingBuffer::<u64>::new(2);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut chunk = producer.write_chunk(1).unwrap();
        chunk.as_mut_slices().0[0] = 42;
        ready_tx.send(()).unwrap();
        resume_rx.recv_timeout(WATCHDOG).unwrap();
        chunk.commit_all();
        producer
    });
    ready_rx.recv_timeout(WATCHDOG).unwrap();
    let (before_publication, stats) = measure(|| consumer.pop());
    resume_tx.send(()).unwrap();
    let _producer = worker.join().unwrap();
    assert!(stats.is_zero(), "{stats:?}");
    assert_eq!(before_publication, Err(rtrb::PopError::Empty));
    assert_eq!(consumer.pop(), Ok(42));
}

#[test]
fn concurrent_publication_and_reclamation_preserve_credit_fifo_and_drop_affinity() {
    const COUNT: usize = 20_000;
    const NO_PROGRESS: Duration = Duration::from_secs(30);
    for capacity in [1, 2, 3, 7] {
        let trace = Trace::new();
        let (mut control, mut audio) = channel(NonZeroUsize::new(capacity).unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let done_audio = Arc::clone(&done);
        let started = Arc::new(std::sync::Barrier::new(2));
        let started_audio = Arc::clone(&started);
        let worker = thread::spawn(move || {
            let mut applied = 0;
            started_audio.wait();
            let ((), stats) = measure(|| {
                while !done_audio.load(Ordering::Acquire) {
                    let result = audio.service_one(|p: &mut Payload| {
                        assert_eq!(p.id, applied, "FIFO and exactly once");
                        applied += 1;
                    });
                    assert_ne!(result, Service::ReturnPending);
                    if result == Service::Empty {
                        thread::yield_now();
                    }
                }
            });
            (audio, applied, stats)
        });
        // An assertion failure on control must not leave a detached worker spinning.
        let _stop_on_unwind = StopOnDrop(Arc::clone(&done));
        let (mut submitted, mut reclaimed) = (0, 0);
        let mut random = 0x1234_5678_u64;
        let mut waiting = None;
        started.wait();
        let mut last_progress = std::time::Instant::now();
        while reclaimed < COUNT {
            let before = (submitted, reclaimed);
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            if submitted < COUNT {
                let payload = waiting.take().unwrap_or_else(|| Payload::new(submitted, &trace));
                match control.try_publish(payload) {
                    Ok(()) => submitted += 1,
                    Err(payload) => waiting = Some(payload),
                }
            }
            if random & 3 != 0 {
                control.reclaim_one(|p| {
                    assert_eq!(p.id, reclaimed, "returned FIFO");
                    reclaimed += 1;
                });
            }
            assert_eq!(control.credits(), capacity - (submitted - reclaimed));
            if reclaimed != before.1 {
                last_progress = std::time::Instant::now();
            } else if last_progress.elapsed() >= NO_PROGRESS {
                break; // Deadlock guard only; a slow but progressing run remains valid.
            }
            if before == (submitted, reclaimed) {
                thread::yield_now();
            }
        }
        done.store(true, Ordering::Release);
        let (audio, applied, stats) = worker.join().unwrap();
        assert_eq!(reclaimed, COUNT, "no-progress watchdog is failure, never proof of quiescence");
        assert_eq!(applied, COUNT);
        assert!(stats.is_zero(), "{stats:?}");
        assert_eq!(trace.counts(), (COUNT, 0));
        assert_eq!(control.credits(), capacity);
        drop(audio);
        drop(control);
    }
}

#[test]
fn dependency_full_preserves_ownership_and_last_endpoint_controls_drop_thread() {
    let trace = Trace::new();
    let (mut producer, consumer) = rtrb::RingBuffer::new(1);
    assert!(producer.push(Payload::new(1, &trace)).is_ok());
    let extra = Payload::new(2, &trace);
    let (rejected, stats) = measure(|| producer.push(extra));
    let Err(rtrb::PushError::Full(extra)) = rejected else { panic!("must return Full with owner") };
    assert!(stats.is_zero(), "{stats:?}");
    assert_eq!(trace.counts(), (0, 0));
    drop(extra);
    // Negative control: the bare primitive permits the final consumer to free a queued item.
    drop(producer);
    thread::spawn(move || drop(consumer)).join().unwrap();
    assert_eq!(trace.counts(), (1, 1));
}
