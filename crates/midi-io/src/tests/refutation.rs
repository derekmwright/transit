//! Independent ADR 0009 probes. Direct sinks avoid the fake backend's mutex on the push path.

use super::*;
use std::sync::{Arc, Barrier};
use std::thread;

#[test]
fn refutation_terminal_stamp_precedes_delayed_last_release() {
    let (mut sink, mut consumer, _) = InputSink::new(&config(1, 1));
    sink.push(0, &[0x90, 60, 1]);
    sink.seed_sequence(u64::MAX - 1);
    let boundary = consumer.boundary();
    let stamped = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let stamped_callback = Arc::clone(&stamped);
    let resume_callback = Arc::clone(&resume);
    sink.before_publish = Some(Box::new(move || {
        stamped_callback.wait();
        resume_callback.wait();
    }));
    let producer = thread::spawn(move || {
        sink.push(1, &[0x80, 60, 0]);
        sink.before_publish = None;
        let ((), stats) = measure(|| sink.push(2, &[0x90, 61, 1, 0x80, 61, 0, 0xF8]));
        (sink, stats)
    });
    stamped.wait();
    let cutoff = boundary.cutoff();
    let exhausted = boundary.is_exhausted();
    let pending_before_resume = consumer.len();
    resume.wait();
    let (sink, stats) = producer.join().unwrap();
    assert_eq!(cutoff, u64::MAX);
    assert!(exhausted);
    assert_eq!(pending_before_resume, 1);
    let events = drain(&mut consumer);
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, u64::MAX - 1]);
    assert!(events.iter().all(|e| e.seq < cutoff));
    assert!(events.last().unwrap().is_release());
    assert!(stats.is_zero(), "direct exhausted callback allocated: {stats:?}");
    assert_eq!(consumer.counters().exhausted_dropped, 3);
    drop(sink);
    drop(consumer);
    assert_eq!(boundary.cutoff(), u64::MAX);
    assert!(boundary.is_exhausted());
}

#[test]
fn refutation_release_drops_and_sysex_do_not_alias_later_sequences() {
    let (mut sink, mut consumer, _) = InputSink::new(&config(1, 1));
    let boundary = consumer.boundary();
    let published = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let published_callback = Arc::clone(&published);
    let resume_callback = Arc::clone(&resume);
    let producer = thread::spawn(move || {
        // seq 0 enters, 1 drops, 2 uses reserve, 3 drops as a release, 4 drops
        // as a real-time event inside SysEx; the SysEx itself takes no sequence.
        sink.push(0, &[0x90, 60, 1, 61, 1, 0x80, 60, 0, 61, 0, 0xF0, 1, 0xF8]);
        published_callback.wait();
        resume_callback.wait();
        sink.push(1, &[2, 0xF7, 0xFE, 0x90, 62, 1, 0x80, 62, 0]);
        sink
    });
    published.wait();
    let cutoff = boundary.cutoff();
    let old = drain(&mut consumer);
    resume.wait();
    let sink = producer.join().unwrap();
    let later = drain(&mut consumer);
    assert_eq!(cutoff, 5);
    assert_eq!(old.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 2]);
    assert!(old.iter().all(|e| e.seq < cutoff));
    assert_eq!(later.iter().map(|e| e.seq).collect::<Vec<_>>(), [5, 6]);
    assert!(later.iter().all(|e| e.seq >= cutoff));
    let counters = consumer.counters();
    assert_eq!((counters.delivered, counters.dropped, counters.dropped_releases), (4, 2, 1));
    assert_eq!((counters.sysex_dropped, counters.sysex_bytes_dropped, counters.filtered), (1, 4, 1));
    assert_eq!(boundary.cutoff(), 7);
    drop(sink);
}

#[test]
fn refutation_multiple_readers_and_worker_pop_then_cutoff() {
    let (mut sink, mut consumer, _) = InputSink::new(&config(4, 2));
    let boundary = consumer.boundary();
    let worker_boundary = boundary.clone();
    let observer_boundary = boundary.clone();
    let start = Arc::new(Barrier::new(4));
    let producer_start = Arc::clone(&start);
    let worker_start = Arc::clone(&start);
    let observer_start = Arc::clone(&start);
    let producer = thread::spawn(move || {
        producer_start.wait();
        let ((), stats) = measure(|| {
            for _ in 0..20_000 {
                sink.push(0, &[0x90, 60, 1, 0x80, 60, 0]);
            }
        });
        stats
    });
    let worker = thread::spawn(move || {
        worker_start.wait();
        let mut last = None;
        loop {
            if let Some(event) = consumer.pop() {
                // Acquiring the ring BEFORE reading the stamp supplies the needed edge.
                assert!(event.seq < worker_boundary.cutoff());
                assert!(last.is_none_or(|previous| previous < event.seq));
                last = Some(event.seq);
            } else if consumer.is_closed() && consumer.is_empty() {
                break;
            } else {
                thread::yield_now();
            }
        }
        consumer.counters()
    });
    let observer = thread::spawn(move || {
        observer_start.wait();
        let mut last = 0;
        for _ in 0..40_000 {
            let cutoff = observer_boundary.cutoff();
            assert!(cutoff >= last);
            last = cutoff;
        }
    });
    start.wait();
    assert!(producer.join().unwrap().is_zero());
    let counters = worker.join().unwrap();
    observer.join().unwrap();
    assert_eq!(boundary.cutoff(), 40_000);
    assert_eq!(counters.delivered + counters.dropped + counters.dropped_releases, 40_000);
}

#[test]
fn refutation_cutoff_before_pop_may_leave_a_later_event() {
    let (mut sink, mut consumer, _) = InputSink::new(&config(1, 0));
    let boundary = consumer.boundary();
    let resume = Arc::new(Barrier::new(2));
    let producer_resume = Arc::clone(&resume);
    let producer = thread::spawn(move || {
        producer_resume.wait();
        sink.push(0, &[0x90, 60, 1]);
    });
    let cutoff = boundary.cutoff();
    resume.wait();
    producer.join().unwrap();
    let event = consumer.pop().unwrap();
    assert_eq!(cutoff, 0);
    assert_eq!(event.seq, cutoff);
    assert!(boundary.cutoff() > event.seq);
}

#[test]
fn refutation_boundary_outlives_both_endpoints_and_reopening() {
    let fake = Fake::default();
    let (io, handle, consumer) = open(&fake, "Boundary lifetime", config(1, 0));
    let from_handle = handle.boundary();
    let from_consumer = consumer.boundary();
    let slot = fake.input_port(handle.key()).unwrap();
    slot.deliver(0, &[0xF8, 0xF8]);
    let key = handle.key().clone();
    drop(consumer);
    drop(handle);
    assert!(!slot.deliver(1, &[0xF8]));
    let (reopened, new_consumer) = io.open_input(&key, config(1, 0)).unwrap();
    let fresh = reopened.boundary();
    let reader = thread::spawn(move || {
        let (values, stats) = measure(|| (from_handle.cutoff(), from_consumer.cutoff(), from_handle.is_exhausted()));
        assert!(stats.is_zero());
        assert_eq!(values, (2, 2, false));
    });
    slot.deliver(2, &[0xF8]);
    reader.join().unwrap();
    assert_eq!(fresh.cutoff(), 1);
    drop(new_consumer);
    drop(reopened);
    assert_eq!(fresh.cutoff(), 1);
}
