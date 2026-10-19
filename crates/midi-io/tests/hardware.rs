//! Hardware check, run by hand with a controller attached:
//!
//! ```text
//! $env:MIDI_IO_TEST_PORT = "KeyStep Pro MIDI IN"
//! cargo test -p soundcraft-midi-io --test hardware -- --ignored --nocapture
//! ```
//!
//! Play at least 20 notes within 30 seconds of the prompt.

use soundcraft_midi_io::{InputConfig, MidiIo};
use std::time::{Duration, Instant};

#[test]
#[ignore = "needs a MIDI controller named by MIDI_IO_TEST_PORT and someone playing it"]
fn played_events_arrive_in_order_with_monotonic_host_time() {
    let wanted = std::env::var("MIDI_IO_TEST_PORT").expect("set MIDI_IO_TEST_PORT to (part of) an input port name");
    let io = MidiIo::new("SoundCraft hardware test");
    let inputs = io.list_inputs().unwrap();
    let port = inputs.iter().find(|p| p.key.name.contains(&wanted)).unwrap_or_else(|| panic!("no input contains {wanted:?}: {inputs:?}"));
    let (handle, mut consumer) = io.open_input(&port.key, InputConfig::default()).unwrap();
    eprintln!("play at least 20 notes on {:?} within 30 s", port.display_name);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    while Instant::now() < deadline && events.iter().filter(|e: &&soundcraft_midi_io::InputEvent| e.bytes[0] & 0xF0 == 0x90).count() < 20 {
        while let Some(e) = consumer.pop() {
            events.push(e);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let counters = handle.counters();
    handle.close();
    eprintln!("{} events, {counters:?}", events.len());

    assert!(events.len() >= 20, "too few events: {}", events.len());
    assert_eq!(counters.dropped + counters.dropped_releases, 0);
    for pair in events.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1, "no gaps without drops");
        assert!(pair[1].host_arrival_ns >= pair[0].host_arrival_ns, "host time went backwards: {pair:?}");
        assert!(pair[1].backend_time_us >= pair[0].backend_time_us, "backend time went backwards: {pair:?}");
    }
}
