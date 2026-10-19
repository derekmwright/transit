//! Ring, counter, sequence and hot-plug behavior against the fake backend.

use crate::fake::Fake;
use crate::ring::InputSink;
use crate::*;
use soundcraft_rt::testing::measure;
use std::time::Duration;

#[global_allocator]
static ALLOC: soundcraft_rt::testing::CountingAlloc = soundcraft_rt::testing::CountingAlloc;

fn config(capacity: usize, reserve: usize) -> InputConfig {
    InputConfig { release_reserve: reserve, ..InputConfig::new(capacity) }
}

fn open(fake: &Fake, name: &str, cfg: InputConfig) -> (MidiIo, InputHandle, InputConsumer) {
    fake.add_input(name);
    let io = fake.io();
    let (handle, consumer) = io.open_input(&Fake::key(name, 0), cfg).unwrap();
    (io, handle, consumer)
}

fn drain(consumer: &mut InputConsumer) -> Vec<InputEvent> {
    std::iter::from_fn(|| consumer.pop()).collect()
}

/// Two-way handshake: the test captures a cutoff while the callback is paused. Bounded waits
/// make a broken hook fail instead of hanging the suite; assertions run after releasing it.
fn publication_pause() -> (Box<dyn FnMut() + Send>, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (paused_tx, paused_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let hook = Box::new(move || {
        paused_tx.send(()).unwrap();
        resume_rx.recv_timeout(Duration::from_secs(30)).expect("test did not resume publication");
    });
    (hook, paused_rx, resume_tx)
}

#[test]
fn cutoff_covers_published_events_drained_by_another_owner_before_counters_update() {
    fn assert_traits<T: Clone + Send + Sync>() {}
    assert_traits::<InputBoundary>();

    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", config(4, 1));
    let boundary = handle.boundary().clone();
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    let (hook, paused, resume) = publication_pause();
    slot.with_sink(|sink| sink.after_publish = Some(hook)).unwrap();
    let callback = std::thread::spawn(move || slot.deliver(0, &[0x90, 60, 100]));
    paused.recv_timeout(Duration::from_secs(30)).unwrap();
    // Ingress already took the event; neither ring length nor delivered can be the boundary.
    let event = std::thread::spawn(move || consumer.pop()).join().unwrap();
    let cutoff = boundary.cutoff();
    let delivered = handle.counters().delivered;
    resume.send(()).unwrap();
    assert!(callback.join().unwrap());

    assert_eq!(delivered, 0);
    assert_eq!(event.unwrap().seq, 0);
    assert!(event.unwrap().seq < cutoff, "published event escaped cutoff {cutoff}");
    assert_eq!(cutoff, 1);
}

#[test]
fn cutoff_covers_exposed_offer_before_publication_but_not_a_later_offer() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", config(4, 1));
    let boundary = handle.boundary();
    assert_eq!(boundary.cutoff(), 0);
    assert!(!boundary.is_exhausted());
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    let (hook, paused, resume) = publication_pause();
    slot.with_sink(|sink| sink.before_publish = Some(hook)).unwrap();
    let producer = slot.clone();
    let callback = std::thread::spawn(move || producer.deliver(0, &[0x90, 60, 100]));
    paused.recv_timeout(Duration::from_secs(30)).unwrap();
    let empty = consumer.is_empty();
    let cutoff = boundary.cutoff();
    let consumer_cutoff = consumer.boundary().cutoff();
    resume.send(()).unwrap();
    assert!(callback.join().unwrap());
    slot.with_sink(|sink| sink.before_publish = None).unwrap();
    let delayed = consumer.pop().unwrap();
    assert!(slot.deliver(1, &[0x80, 60, 0]));
    let later = consumer.pop().unwrap();

    assert!(empty, "the first offer was paused before publication");
    assert!(delayed.seq < cutoff, "exposed offer escaped cutoff {cutoff}");
    assert_eq!(cutoff, 1);
    assert_eq!(consumer_cutoff, cutoff);
    assert!(later.seq >= cutoff);
    assert_eq!(later.seq, cutoff);
    assert_eq!(boundary.cutoff(), 2);
}

#[test]
fn cutoff_counts_ring_saturation_and_release_drop_gaps() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", config(1, 1));
    let boundary = handle.boundary();
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    slot.deliver(0, &[0x90, 60, 100, 61, 100, 0x80, 60, 0, 61, 0, 0x90, 62, 100]);
    let cutoff = boundary.cutoff();
    // Filtered, malformed and SysEx bytes never receive a sequence.
    slot.deliver(1, &[0xFE, 0x40, 0xF0, 1, 0xF7]);
    let events = drain(&mut consumer);
    assert_eq!(cutoff, 5);
    assert_eq!(boundary.cutoff(), cutoff);
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 2]);
    assert!(events.iter().all(|e| e.seq < cutoff));
    let counters = handle.counters();
    assert_eq!((counters.delivered, counters.dropped, counters.dropped_releases), (2, 2, 1));
    slot.deliver(2, &[0x90, 63, 100]);
    assert_eq!(consumer.pop().unwrap().seq, cutoff);
}

#[test]
fn cutoff_exhaustion_never_wraps_even_when_the_last_offer_is_dropped() {
    for drop_last in [false, true] {
        let fake = Fake::default();
        let (_io, handle, mut consumer) = open(&fake, "Keys", config(1, 0));
        let boundary = handle.boundary();
        let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
        slot.with_sink(|sink| sink.seed_sequence(u64::MAX - 2)).unwrap();
        assert_eq!(boundary.cutoff(), u64::MAX - 2);
        assert!(!boundary.is_exhausted());
        slot.deliver(0, &[0x90, 60, 100]);
        assert_eq!(boundary.cutoff(), u64::MAX - 1);
        assert!(!consumer.is_exhausted());
        if !drop_last {
            assert_eq!(consumer.pop().unwrap().seq, u64::MAX - 2);
        }
        slot.deliver(1, &[0x90, 61, 100]);
        let terminal = boundary.cutoff();
        assert_eq!(terminal, u64::MAX);
        assert!(boundary.is_exhausted() && consumer.is_exhausted());
        assert_eq!(handle.counters().exhausted_dropped, 0);
        let last = consumer.pop().unwrap();
        assert_eq!(last.seq, if drop_last { u64::MAX - 2 } else { u64::MAX - 1 });
        assert!(last.seq < terminal);
        assert!(consumer.is_empty());

        let ((), stats) = measure(|| {
            // A free ring must still reject notes, releases and real-time messages forever.
            for _ in 0..4 {
                slot.deliver(2, &[0x90, 62, 100, 0x80, 60, 0, 0xF8]);
            }
        });
        assert!(stats.is_zero(), "exhausted ingress allocated: {stats:?}");
        assert!(consumer.is_empty(), "exhausted input reused a sequence");
        assert_eq!(boundary.cutoff(), terminal);
        assert!(boundary.is_exhausted() && consumer.is_exhausted());
        let counters = handle.counters();
        assert_eq!(counters.exhausted_dropped, 12);
        assert_eq!(counters.dropped, u64::from(drop_last));
        assert_eq!(counters.dropped_releases, 0);
    }
}

#[test]
fn cutoffs_are_per_open_port_and_boundary_does_not_keep_connection_open() {
    let fake = Fake::default();
    let (io, first, mut first_consumer) = open(&fake, "First", config(4, 1));
    let (_io, second, _second_consumer) = open(&fake, "Second", config(4, 1));
    let old = first.boundary();
    let other = second.boundary();
    let slot = fake.input_port(&Fake::key("First", 0)).unwrap();
    slot.deliver(0, &[0x90, 60, 100]);
    assert_eq!((old.cutoff(), other.cutoff()), (1, 0));
    drop(first);
    assert!(first_consumer.is_closed());
    assert!(!slot.deliver(1, &[0x90, 61, 100]));
    assert!(first_consumer.pop().unwrap().seq < old.cutoff());
    let (reopened, mut consumer) = io.open_input(&Fake::key("First", 0), config(4, 1)).unwrap();
    let fresh = reopened.boundary();
    assert_eq!((old.cutoff(), fresh.cutoff()), (1, 0));
    slot.deliver(2, &[0x90, 62, 100, 63, 100]);
    assert_eq!(consumer.pop().unwrap().seq, 0);
    assert_eq!((old.cutoff(), fresh.cutoff(), other.cutoff()), (1, 2, 0));
}

#[test]
fn messages_keep_order_running_status_is_expanded_and_seq_counts_up() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", InputConfig::default());
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    assert!(slot.deliver(1000, &[0x90, 60, 100]));
    // Running status, a clock tick inside a message, program change.
    assert!(slot.deliver(2000, &[0x90, 62, 90, 64, 0xF8, 80, 0xC1, 5]));
    assert!(slot.deliver(3000, &[0x80, 60, 0]));
    let events = drain(&mut consumer);
    let got: Vec<(&[u8], u64)> = events.iter().map(|e| (e.message(), e.backend_time_us)).collect();
    assert_eq!(
        got,
        [
            (&[0x90, 60, 100][..], 1000),
            (&[0x90, 62, 90][..], 2000),
            (&[0xF8][..], 2000),
            (&[0x90, 64, 80][..], 2000),
            (&[0xC1, 5][..], 2000),
            (&[0x80, 60, 0][..], 3000),
        ]
    );
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5]);
    let c = handle.counters();
    assert_eq!((c.callbacks, c.delivered, c.dropped, c.malformed_bytes), (3, 6, 0, 0));
}

#[test]
fn a_full_ring_drops_and_counts_without_blocking() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", config(8, 2));
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    // A producer thread that would hang forever if a full ring blocked it.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let producer = std::thread::spawn(move || {
        for i in 0..100_000u64 {
            slot.deliver(i, &[0x90, (i % 128) as u8, 1]);
        }
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(Duration::from_secs(30)).expect("producer blocked on a full ring");
    producer.join().unwrap();
    let c = handle.counters();
    assert_eq!(c.delivered, 8, "general capacity only; the reserve is for releases");
    assert_eq!(c.dropped, 100_000 - 8);
    assert_eq!(c.dropped_releases, 0);
    let events = drain(&mut consumer);
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), (0..8).collect::<Vec<_>>());
    // After a drain the ring accepts again, and seq shows the gap.
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    slot.deliver(0, &[0x90, 1, 1]);
    assert_eq!(consumer.pop().unwrap().seq, 100_000);
}

#[test]
fn releases_use_the_reserve_and_are_counted_separately_when_even_it_is_full() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", config(4, 3));
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    for key in 0..10 {
        slot.deliver(0, &[0x90, key, 100]);
    }
    // Releases of each kind: note-off, note-on velocity 0, sustain up, then one too many.
    slot.deliver(1, &[0x80, 0, 0]);
    slot.deliver(1, &[0x91, 1, 0]);
    slot.deliver(1, &[0xB0, 64, 0]);
    slot.deliver(1, &[0xB0, 123, 0]);
    // A non-release still finds no room.
    slot.deliver(1, &[0xB0, 1, 0]);
    let c = handle.counters();
    assert_eq!((c.delivered, c.dropped, c.dropped_releases), (7, 7, 1));
    let events = drain(&mut consumer);
    let releases: Vec<&[u8]> = events.iter().filter(|e| e.is_release()).map(InputEvent::message).collect();
    assert_eq!(releases, [&[0x80, 0, 0][..], &[0x91, 1, 0], &[0xB0, 64, 0]]);
}

#[test]
fn sysex_is_dropped_and_counted_across_chunks_while_real_time_bytes_pass() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", InputConfig::default());
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    slot.deliver(0, &[0xF0, 0x7E, 0x7F, 0xF8, 0x06]);
    // WinMM-style continuation chunk: data bytes, the end, then a channel message.
    slot.deliver(1, &[0x01, 0x02, 0xF7, 0x90, 60, 1]);
    slot.deliver(2, &[0xF0, 1, 2, 3, 0xF7]);
    let msgs: Vec<Vec<u8>> = drain(&mut consumer).iter().map(|e| e.message().to_vec()).collect();
    assert_eq!(msgs, [vec![0xF8], vec![0x90, 60, 1]]);
    let c = handle.counters();
    assert_eq!((c.sysex_dropped, c.sysex_bytes_dropped, c.malformed_bytes), (2, 7 + 5, 0));
}

#[test]
fn malformed_bytes_and_active_sensing_are_counted_not_delivered() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", InputConfig::default());
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    slot.deliver(0, &[0x40, 0x41]); // data with no status
    slot.deliver(0, &[0x90, 60]); // cut short by the end
    slot.deliver(0, &[0xB0, 7, 0x90, 61, 2]); // cut short by a status
    slot.deliver(0, &[0xF4, 0xFD, 0xF7]); // undefined and stray EOX
    slot.deliver(0, &[0xFE, 0xFE]);
    let msgs: Vec<Vec<u8>> = drain(&mut consumer).iter().map(|e| e.message().to_vec()).collect();
    assert_eq!(msgs, [vec![0x90, 61, 2]]);
    let c = handle.counters();
    assert_eq!((c.malformed_bytes, c.filtered), (2 + 2 + 2 + 3, 2));
    let (_io2, _h2, mut passing) = {
        let fake = Fake::default();
        let r = open(&fake, "AS", InputConfig { pass_active_sensing: true, ..InputConfig::default() });
        fake.input_port(&Fake::key("AS", 0)).unwrap().deliver(0, &[0xFE]);
        r
    };
    assert_eq!(passing.pop().map(|e| e.message().to_vec()), Some(vec![0xFE]));
}

#[test]
fn the_push_path_allocates_nothing() {
    let cfg = config(64, 8);
    let (mut sink, mut consumer, _shared) = InputSink::new(&cfg);
    let traffic: [&[u8]; 6] = [&[0x90, 60, 100, 62, 90], &[0xF8], &[0xF0, 1, 2, 3], &[4, 5, 0xF7, 0x80, 60, 0], &[0x40, 0xF4], &[0xB0, 64, 0, 0xFE]];
    let (_, control) = measure(|| std::hint::black_box(Vec::<u8>::with_capacity(4)));
    assert_eq!(control.allocs, 1, "the counting allocator is installed");
    let ((), stats) = measure(|| {
        for round in 0..2_000u64 {
            for bytes in traffic {
                sink.push(round, bytes);
            }
            // Drain only now and then, so the ring overflows and the drop paths run too.
            if round % 20 == 0 {
                while consumer.pop().is_some() {}
            }
        }
    });
    assert!(stats.is_zero(), "push path allocated: {stats:?}");
    let c = consumer.counters();
    assert!(c.dropped > 0 && c.delivered > 0 && c.sysex_dropped == 2_000 && c.malformed_bytes > 0, "{c:?}");

    // The same through the fake backend's callback stand-in (an uncontended std mutex).
    let fake = Fake::default();
    let (_io, _handle, _consumer) = open(&fake, "Keys", cfg);
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    let ((), stats) = measure(|| {
        for round in 0..1_000u64 {
            slot.deliver(round, &[0x90, 60, 100]);
        }
    });
    assert!(stats.is_zero(), "{stats:?}");
}

#[test]
fn host_arrival_is_monotonic_and_seq_strictly_increases() {
    let fake = Fake::default();
    let origin = host_origin();
    let (_io, _handle, mut consumer) = open(&fake, "Keys", InputConfig { host_origin: origin, ..InputConfig::new(4096) });
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    let before = host_now_ns();
    for i in 0..1000u64 {
        slot.deliver(i * 1000, &[0x90, 1, 1, 0x80, 1, 0]);
    }
    let after = host_now_ns();
    let events = drain(&mut consumer);
    assert_eq!(events.len(), 2000);
    for pair in events.windows(2) {
        assert!(pair[1].host_arrival_ns >= pair[0].host_arrival_ns);
        assert!(pair[1].backend_time_us >= pair[0].backend_time_us);
        assert_eq!(pair[1].seq, pair[0].seq + 1);
    }
    assert!(events[0].host_arrival_ns >= before && events[1999].host_arrival_ns <= after);
    // Both messages of one callback share its arrival time.
    assert_eq!(events[0].host_arrival_ns, events[1].host_arrival_ns);
}

#[test]
fn a_custom_origin_shifts_host_time() {
    let late = host_origin() + Duration::from_secs(3600);
    assert_eq!(host_ns_since(late), 0, "an origin in the future clamps to 0");
}

#[test]
fn dropping_the_handle_closes_the_port() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", InputConfig::default());
    let slot = fake.input_port(&Fake::key("Keys", 0)).unwrap();
    slot.deliver(0, &[0x90, 60, 1]);
    assert!(!consumer.is_closed());
    drop(handle);
    assert!(consumer.is_closed());
    assert!(!slot.deliver(0, &[0x90, 61, 1]), "no sink after close");
    assert_eq!(drain(&mut consumer).len(), 1, "events already queued stay readable");
    // The port can be opened again.
    let io = fake.io();
    let (again, _c) = io.open_input(&Fake::key("Keys", 0), InputConfig::default()).unwrap();
    again.close();
}

#[test]
fn the_watcher_reports_added_and_removed_ports_and_flags_open_ones() {
    let fake = Fake::default();
    fake.add_input("A");
    fake.add_output("A");
    let io = fake.io();
    let mut watcher = io.watcher().unwrap();
    assert_eq!(watcher.poll().unwrap(), []);
    let (handle, consumer) = io.open_input(&Fake::key("A", 0), InputConfig::default()).unwrap();

    fake.add_input("A");
    fake.add_input("B");
    let changes = watcher.poll().unwrap();
    let added: Vec<String> = changes
        .iter()
        .map(|c| match c {
            PortChange::Added(p) => p.display_name.clone(),
            PortChange::Removed { .. } => String::from("removed?"),
        })
        .collect();
    assert_eq!(added, ["A #2", "B"]);

    fake.remove_input(&Fake::key("B", 0));
    let changes = watcher.poll().unwrap();
    assert!(matches!(&changes[..], [PortChange::Removed { port, was_open: false }] if port.key.name == "B"));
    assert!(!handle.is_disconnected());

    // Unplug both "A" inputs (a hub going away).
    fake.remove_input(&Fake::key("A", 1));
    fake.remove_input(&Fake::key("A", 0));
    let changes = watcher.poll().unwrap();
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().any(|c| matches!(c, PortChange::Removed { port, was_open: true } if port.key.name == "A" && port.key.ordinal == 0)));
    assert!(handle.is_disconnected() && consumer.is_disconnected());
    assert_eq!(watcher.ports().count(), 1, "the output A remains");

    fake.add_input("A");
    let changes = watcher.poll().unwrap();
    assert!(matches!(&changes[..], [PortChange::Added(p)] if p.key.name == "A" && p.key.ordinal == 0 && p.direction == Direction::Input));
    assert!(handle.is_disconnected(), "a returning port does not revive the old handle");
}

#[test]
fn outputs_send_and_errors_are_typed() {
    let fake = Fake::default();
    fake.add_output("Synth");
    let io = fake.io();
    assert_eq!(io.list_outputs().unwrap().len(), 1);
    let mut out = io.open_output(&Fake::key("Synth", 0)).unwrap();
    out.send(&[0x90, 60, 1]).unwrap();
    assert_eq!(fake.sent(&Fake::key("Synth", 0)), [vec![0x90, 60, 1]]);
    assert!(matches!(io.open_output(&Fake::key("Nope", 0)), Err(MidiIoError::PortNotFound(_))));
    let wrong = PortKey { backend: BackendId::WinMm, ..Fake::key("Synth", 0) };
    assert!(matches!(io.open_output(&wrong), Err(MidiIoError::WrongBackend(_))));
    fake.add_input("Keys");
    assert!(matches!(io.open_input(&Fake::key("Keys", 0), config(0, 4)), Err(MidiIoError::InvalidConfig(_))));
}

#[test]
fn unplugging_the_first_of_two_identical_inputs_flags_the_dead_handle_not_the_live_one() {
    let fake = Fake::default();
    let k0 = fake.add_input("KeyStep");
    let k1 = fake.add_input("KeyStep");
    assert_ne!(k0.id, k1.id);
    let io = fake.io();
    let mut watcher = io.watcher().unwrap();
    let (h0, c0) = io.open_input(&k0, InputConfig::default()).unwrap();
    let (h1, mut c1) = io.open_input(&k1, InputConfig::default()).unwrap();
    let live = fake.input_port(&k1).unwrap();
    fake.remove_input(&k0);
    let changes = watcher.poll().unwrap();
    assert!(matches!(&changes[..], [PortChange::Removed { port, was_open: true }] if port.key.id == k0.id), "{changes:?}");
    assert!(h0.is_disconnected() && c0.is_disconnected(), "the dead handle is flagged");
    assert!(!h1.is_disconnected() && !c1.is_disconnected(), "the live handle is not");
    // The survivor is now first in enumeration order, yet still the same port.
    assert_eq!(io.list_inputs().unwrap()[0].key.id, k1.id);
    assert!(live.deliver(0, &[0x90, 60, 100]));
    assert_eq!(c1.pop().map(|e| e.message().to_vec()), Some(vec![0x90, 60, 100]));
    // Opening by the survivor's old key still finds it by id.
    drop(h1);
    assert!(io.open_input(&k1, InputConfig::default()).is_ok());
}

#[test]
fn mixed_ids_unplugging_identified_input_keeps_anonymous_input_live() {
    let fake = Fake::default();
    let identified = fake.add_input("Keys");
    let anonymous = fake.add_input_anonymous("Keys");
    let io = fake.io();
    let mut watcher = io.watcher().unwrap();
    let (dead, dead_consumer) = io.open_input(&identified, InputConfig::default()).unwrap();
    let (live, mut consumer) = io.open_input(&anonymous, InputConfig::default()).unwrap();
    let sender = fake.input_port(&anonymous).unwrap();

    fake.remove_input(&identified);
    let changes = watcher.poll().unwrap();
    assert!(sender.deliver(1, &[0x90, 60, 100]));
    assert_eq!(consumer.pop().unwrap().message(), &[0x90, 60, 100]);
    assert!(dead.is_disconnected() && dead_consumer.is_disconnected());
    assert!(!live.is_disconnected() && !consumer.is_disconnected(), "live port flagged: {changes:?}");
    assert!(matches!(&changes[..], [PortChange::Removed { port, was_open: true }] if port.key.id == identified.id), "{changes:?}");
    assert_eq!(io.list_inputs().unwrap()[0].key, anonymous);
    assert!(watcher.poll().unwrap().is_empty());

    // The saved anonymous key still opens the survivor after its enumeration index changes.
    drop(live);
    let (reopened, mut consumer) = io.open_input(&anonymous, InputConfig::default()).unwrap();
    assert!(sender.deliver(2, &[0x80, 60, 0]));
    assert_eq!(consumer.pop().unwrap().message(), &[0x80, 60, 0]);
    fake.remove_input(&anonymous);
    assert!(matches!(&watcher.poll().unwrap()[..], [PortChange::Removed { was_open: true, .. }]));
    assert!(reopened.is_disconnected() && consumer.is_disconnected());
}

#[test]
fn missing_port_with_max_ordinal_formats_without_overflow() {
    let io = Fake::default().io();
    let key = Fake::key("missing", u32::MAX);
    let error = io.open_input(&key, InputConfig::default()).err().unwrap();
    assert!(matches!(&error, MidiIoError::PortNotFound(missing) if *missing == key));
    assert_eq!(error.to_string(), "MIDI port not found: missing #4294967296");
}

#[test]
fn id_less_keys_fall_back_to_name_and_ordinal() {
    let fake = Fake::default();
    let k = fake.add_input_anonymous("Old");
    assert!(k.id.is_empty());
    let io = fake.io();
    let (handle, _c) = io.open_input(&k, InputConfig::default()).unwrap();
    let mut watcher = io.watcher().unwrap();
    fake.remove_input(&k);
    assert!(matches!(&watcher.poll().unwrap()[..], [PortChange::Removed { was_open: true, .. }]));
    assert!(handle.is_disconnected());
}

#[test]
fn huge_capacities_are_rejected_not_allocated() {
    let fake = Fake::default();
    let key = fake.add_input("A");
    let io = fake.io();
    for cfg in [
        InputConfig { capacity: usize::MAX, release_reserve: 16, ..InputConfig::default() },
        InputConfig { capacity: MAX_INPUT_EVENTS + 1, ..InputConfig::default() },
        InputConfig { release_reserve: usize::MAX, ..InputConfig::default() },
    ] {
        assert!(matches!(io.open_input(&key, cfg), Err(MidiIoError::InvalidConfig(_))));
    }
    assert!(io.open_input(&key, InputConfig { capacity: MAX_INPUT_EVENTS, release_reserve: 1, ..InputConfig::default() }).is_ok());
}

/// Pushes each chunk as one backend callback and returns what the ring held, plus the counters.
fn framed(chunks: &[&[u8]]) -> (Vec<Vec<u8>>, InputCounters) {
    let (mut sink, mut consumer, shared) = InputSink::new(&InputConfig::new(256));
    for chunk in chunks {
        sink.push(0, chunk);
    }
    (drain(&mut consumer).iter().map(|e| e.message().to_vec()).collect(), shared.snapshot())
}

#[test]
fn sysex_clears_running_status() {
    // M1: data after a complete SysEx must not reuse the earlier 0x90.
    let (msgs, c) = framed(&[&[0x90, 60, 1, 0xF0, 1, 0xF7, 61, 1]]);
    assert_eq!(msgs, [vec![0x90, 60, 1]]);
    assert_eq!((c.malformed_bytes, c.sysex_dropped, c.sysex_bytes_dropped), (2, 1, 3));
}

#[test]
fn f7_ends_sysex_so_later_bare_data_is_malformed_not_sysex() {
    // M3: after F0 .. F7, data in a later callback is stray data.
    let (msgs, c) = framed(&[&[0xF0, 1, 2, 0xF7], &[0x11, 0x12]]);
    assert!(msgs.is_empty());
    assert_eq!((c.sysex_bytes_dropped, c.malformed_bytes), (4, 2));
}

#[test]
fn a_status_byte_inside_sysex_ends_it() {
    // M13: the cut-off SysEx must not keep swallowing data bytes (here, running status data).
    let (msgs, c) = framed(&[&[0xF0, 1, 2], &[0x90, 60, 1, 61, 1]]);
    assert_eq!(msgs, [vec![0x90, 60, 1], vec![0x90, 61, 1]]);
    assert_eq!((c.sysex_bytes_dropped, c.malformed_bytes), (3, 0));
}

#[test]
fn system_common_does_not_set_running_status() {
    // M6: data after a system common message is not another system common message.
    let (msgs, c) = framed(&[&[0xF1, 0x10, 0x20], &[0x90, 60, 1, 0xF3, 2, 5]]);
    assert_eq!(msgs, [vec![0xF1, 0x10], vec![0x90, 60, 1], vec![0xF3, 2]]);
    assert_eq!(c.malformed_bytes, 2);
}

#[test]
fn song_position_is_three_bytes_and_tune_request_is_one() {
    // M14 and M15.
    let (msgs, c) = framed(&[&[0xF2, 1, 2, 0xF6, 0xF3, 4]]);
    assert_eq!(msgs, [vec![0xF2, 1, 2], vec![0xF6], vec![0xF3, 4]]);
    assert_eq!(c.malformed_bytes, 0);
}

#[test]
fn an_undefined_status_clears_running_status() {
    // M7: data after F4 must not reuse the earlier 0x90.
    let (msgs, c) = framed(&[&[0x90, 60, 1, 0xF4, 61, 1]]);
    assert_eq!(msgs, [vec![0x90, 60, 1]]);
    assert_eq!(c.malformed_bytes, 3);
}
