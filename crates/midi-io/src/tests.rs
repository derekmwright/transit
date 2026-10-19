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

#[test]
fn messages_keep_order_running_status_is_expanded_and_seq_counts_up() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", InputConfig::default());
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    assert!(Fake::deliver(&slot, 1000, &[0x90, 60, 100]));
    // Running status, a clock tick inside a message, program change.
    assert!(Fake::deliver(&slot, 2000, &[0x90, 62, 90, 64, 0xF8, 80, 0xC1, 5]));
    assert!(Fake::deliver(&slot, 3000, &[0x80, 60, 0]));
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
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    // A producer thread that would hang forever if a full ring blocked it.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let producer = std::thread::spawn(move || {
        for i in 0..100_000u64 {
            Fake::deliver(&slot, i, &[0x90, (i % 128) as u8, 1]);
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
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    Fake::deliver(&slot, 0, &[0x90, 1, 1]);
    assert_eq!(consumer.pop().unwrap().seq, 100_000);
}

#[test]
fn releases_use_the_reserve_and_are_counted_separately_when_even_it_is_full() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", config(4, 3));
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    for key in 0..10 {
        Fake::deliver(&slot, 0, &[0x90, key, 100]);
    }
    // Releases of each kind: note-off, note-on velocity 0, sustain up, then one too many.
    Fake::deliver(&slot, 1, &[0x80, 0, 0]);
    Fake::deliver(&slot, 1, &[0x91, 1, 0]);
    Fake::deliver(&slot, 1, &[0xB0, 64, 0]);
    Fake::deliver(&slot, 1, &[0xB0, 123, 0]);
    // A non-release still finds no room.
    Fake::deliver(&slot, 1, &[0xB0, 1, 0]);
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
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    Fake::deliver(&slot, 0, &[0xF0, 0x7E, 0x7F, 0xF8, 0x06]);
    // WinMM-style continuation chunk: data bytes, the end, then a channel message.
    Fake::deliver(&slot, 1, &[0x01, 0x02, 0xF7, 0x90, 60, 1]);
    Fake::deliver(&slot, 2, &[0xF0, 1, 2, 3, 0xF7]);
    let msgs: Vec<Vec<u8>> = drain(&mut consumer).iter().map(|e| e.message().to_vec()).collect();
    assert_eq!(msgs, [vec![0xF8], vec![0x90, 60, 1]]);
    let c = handle.counters();
    assert_eq!((c.sysex_dropped, c.sysex_bytes_dropped, c.malformed_bytes), (2, 7 + 5, 0));
}

#[test]
fn malformed_bytes_and_active_sensing_are_counted_not_delivered() {
    let fake = Fake::default();
    let (_io, handle, mut consumer) = open(&fake, "Keys", InputConfig::default());
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    Fake::deliver(&slot, 0, &[0x40, 0x41]); // data with no status
    Fake::deliver(&slot, 0, &[0x90, 60]); // cut short by the end
    Fake::deliver(&slot, 0, &[0xB0, 7, 0x90, 61, 2]); // cut short by a status
    Fake::deliver(&slot, 0, &[0xF4, 0xFD, 0xF7]); // undefined and stray EOX
    Fake::deliver(&slot, 0, &[0xFE, 0xFE]);
    let msgs: Vec<Vec<u8>> = drain(&mut consumer).iter().map(|e| e.message().to_vec()).collect();
    assert_eq!(msgs, [vec![0x90, 61, 2]]);
    let c = handle.counters();
    assert_eq!((c.malformed_bytes, c.filtered), (2 + 2 + 2 + 3, 2));
    let (_io2, _h2, mut passing) = {
        let fake = Fake::default();
        let r = open(&fake, "AS", InputConfig { pass_active_sensing: true, ..InputConfig::default() });
        Fake::deliver(&fake.input_slot(&Fake::key("AS", 0)).unwrap(), 0, &[0xFE]);
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
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    let ((), stats) = measure(|| {
        for round in 0..1_000u64 {
            Fake::deliver(&slot, round, &[0x90, 60, 100]);
        }
    });
    assert!(stats.is_zero(), "{stats:?}");
}

#[test]
fn host_arrival_is_monotonic_and_seq_strictly_increases() {
    let fake = Fake::default();
    let origin = host_origin();
    let (_io, _handle, mut consumer) = open(&fake, "Keys", InputConfig { host_origin: origin, ..InputConfig::new(4096) });
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    let before = host_now_ns();
    for i in 0..1000u64 {
        Fake::deliver(&slot, i * 1000, &[0x90, 1, 1, 0x80, 1, 0]);
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
    let slot = fake.input_slot(&Fake::key("Keys", 0)).unwrap();
    Fake::deliver(&slot, 0, &[0x90, 60, 1]);
    assert!(!consumer.is_closed());
    drop(handle);
    assert!(consumer.is_closed());
    assert!(!Fake::deliver(&slot, 0, &[0x90, 61, 1]), "no sink after close");
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
    assert!(matches!(&changes[..], [PortChange::Removed { port, was_open: false }] if port.key == Fake::key("B", 0)));
    assert!(!handle.is_disconnected());

    // Unplug both "A" inputs (a hub going away). Unplugging only the first would leave its key
    // naming the second: name-plus-ordinal identity cannot tell identical devices apart.
    fake.remove_input(&Fake::key("A", 1));
    fake.remove_input(&Fake::key("A", 0));
    let changes = watcher.poll().unwrap();
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().any(|c| matches!(c, PortChange::Removed { port, was_open: true } if port.key == Fake::key("A", 0))));
    assert!(handle.is_disconnected() && consumer.is_disconnected());
    assert_eq!(watcher.ports().count(), 1, "the output A remains");

    fake.add_input("A");
    let changes = watcher.poll().unwrap();
    assert!(matches!(&changes[..], [PortChange::Added(p)] if p.key == Fake::key("A", 0) && p.direction == Direction::Input));
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
