#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The MIDI output thread against the fake backend. No hardware MIDI is ever opened.
use super::*;
use transit_midi_io::fake::{Fake, FakeSend};

// A deadline is only a deadlock guard; success always needs the observed state.
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "MIDI output thread did not reach: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn port_of(key: &PortKey) -> MidiOutPort {
    MidiOutPort { backend: key.backend.as_str().into(), name: key.name.clone(), ordinal: key.ordinal, id: key.id.clone() }
}

fn destination(track: u64, key: &PortKey, channel: MidiOutChannel, offset_ms: i64) -> Destination {
    Destination { track: TrackId(track), port: port_of(key), channel, offset_ns: offset_ms * 1_000_000 }
}

fn message(track: u64, at_ns: u64, bytes: [u8; 3]) -> OutEvent {
    OutEvent { kind: Kind::Message, track, at_ns, bytes, len: 3 }
}

struct Rig {
    fake: Fake,
    key: PortKey,
    origin: HostOrigin,
    out: Out,
    playback: Producer,
    echo: Producer,
}

impl Rig {
    fn new(destinations: impl FnOnce(&PortKey) -> Vec<Destination>) -> Self {
        let fake = Fake::default();
        let key = fake.add_output("Synth OUT");
        let origin = HostOrigin::default();
        let (mut out, playback, echo) = Out::new();
        out.start(fake.io(), origin).unwrap();
        out.configure(destinations(&key));
        let rig = Self { fake, key, origin, out, playback, echo };
        until("the destination opens", || rig.out.status().tracks.iter().all(|t| t.state == DestinationState::Connected));
        rig
    }

    fn now(&self) -> u64 {
        self.origin.now_ns().unwrap()
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        self.fake.sent(&self.key)
    }

    fn wait_sent(&self, count: usize) -> Vec<FakeSend> {
        until(&format!("{count} sends"), || self.fake.sends(&self.key).len() >= count);
        self.fake.sends(&self.key)
    }
}

#[test]
fn events_go_out_in_order_at_their_instant_plus_the_offset_and_never_from_the_audio_thread() {
    let mut rig = Rig::new(|key| vec![destination(1, key, MidiOutChannel::AsRecorded, 15)]);
    let base = rig.now() + 20_000_000;
    // The producer runs on a thread marked as the audio thread, as the callback is.
    soundcraft_mix::mark_audio_thread();
    let audio = std::thread::current().id();
    let (pushed, stats) = transit_rt::testing::measure(|| {
        [
            message(1, base, [0x90, 60, 100]),
            message(1, base + 5_000_000, [0x90, 64, 100]),
            message(1, base + 10_000_000, [0x80, 60, 64]),
            message(1, base + 10_000_000, [0x80, 64, 64]),
        ]
        .into_iter()
        .all(|e| rig.playback.push(e))
    });
    soundcraft_mix::unmark_audio_thread();
    assert!(pushed);
    assert!(stats.is_zero(), "pushing allocated: {stats:?}");
    let sends = rig.wait_sent(4);
    assert_eq!(sends.iter().map(|s| s.message.clone()).collect::<Vec<_>>(), [vec![0x90, 60, 100], vec![0x90, 64, 100], vec![0x80, 60, 64], vec![0x80, 64, 64]]);
    let origin = rig.origin.instant();
    for (send, at) in sends.iter().zip([base, base + 5_000_000, base + 10_000_000, base + 10_000_000]) {
        let due = origin + Duration::from_nanos(at + 15_000_000);
        assert!(send.at >= due, "sent {:?} before its instant plus the 15 ms offset", due - send.at);
        // A loose bound for a loaded CI machine; the jitter itself is documented, not asserted.
        assert!(send.at - due < Duration::from_millis(50), "sent {:?} late", send.at - due);
        assert_ne!(send.thread, audio, "a send ran on the audio thread");
        assert_ne!(send.thread, std::thread::current().id());
    }
    let status = rig.out.status();
    assert_eq!(status.audio_thread_sends, 0);
    assert_eq!(status.sent, 4);
}

#[test]
fn stop_drops_scheduled_playback_and_releases_held_notes_and_pedals() {
    let mut rig = Rig::new(|key| vec![destination(1, key, MidiOutChannel::Channel(3), 0)]);
    assert!(rig.playback.push(message(1, 0, [0x93, 60, 100])));
    assert!(rig.playback.push(message(1, 0, [0xB3, 64, 127])));
    rig.wait_sent(2);
    // A note scheduled a second ahead belongs to audio that Stop discards.
    assert!(rig.playback.push(message(1, rig.now() + 1_000_000_000, [0x93, 72, 100])));
    assert!(rig.playback.push(OutEvent { kind: Kind::Stop, ..OutEvent::default() }));
    // The held note's off and the pedal it held up, then Sustain off and All Notes Off on the
    // channel the port was used on.
    let sends = rig.wait_sent(6);
    let tail: Vec<_> = sends.iter().skip(2).map(|s| s.message.clone()).collect();
    assert_eq!(tail, [vec![0x83, 60, 64], vec![0xB3, 64, 0], vec![0xB3, 64, 0], vec![0xB3, 123, 0]]);
    std::thread::sleep(Duration::from_millis(30));
    assert!(!rig.sent().contains(&vec![0x93, 72, 100]), "a note scheduled past Stop was sent");
}

#[test]
fn a_wrap_ends_held_notes_at_the_wrap_instant_and_drops_discarded_playback() {
    let mut rig = Rig::new(|key| vec![destination(1, key, MidiOutChannel::AsRecorded, 0)]);
    let wrap = rig.now() + 30_000_000;
    assert!(rig.playback.push(message(1, 0, [0x90, 60, 100])));
    // Rendered ahead past the wrap, then discarded by a locate: must never sound.
    assert!(rig.playback.push(message(1, wrap + 5_000_000, [0x90, 61, 100])));
    assert!(rig.playback.push(OutEvent { kind: Kind::Release, at_ns: wrap, ..OutEvent::default() }));
    // The next lap restarts the note (the mix side chases it) at the wrap instant.
    assert!(rig.playback.push(message(1, wrap, [0x90, 60, 100])));
    let sends = rig.wait_sent(3);
    assert_eq!(sends.iter().map(|s| s.message.clone()).collect::<Vec<_>>(), [vec![0x90, 60, 100], vec![0x80, 60, 64], vec![0x90, 60, 100]]);
    let due = rig.origin.instant() + Duration::from_nanos(wrap);
    assert!(sends[1].at >= due, "the release went out before the wrap");
    assert!(sends[1].at <= sends[2].at);
    std::thread::sleep(Duration::from_millis(40));
    assert!(!rig.sent().contains(&vec![0x90, 61, 100]));
}

#[test]
fn a_destination_change_releases_on_the_old_port_only_on_the_tracks_channels() {
    let mut rig = Rig::new(|key| vec![destination(1, key, MidiOutChannel::Channel(0), 0), destination(2, key, MidiOutChannel::Channel(1), 0)]);
    let other = rig.fake.add_output("Other OUT");
    assert!(rig.playback.push(message(1, 0, [0x90, 60, 100])));
    assert!(rig.playback.push(message(2, 0, [0x91, 62, 100])));
    rig.wait_sent(2);
    rig.out.configure(vec![destination(1, &other, MidiOutChannel::Channel(0), 0), destination(2, &rig.key, MidiOutChannel::Channel(1), 0)]);
    let sends = rig.wait_sent(5);
    assert_eq!(sends.iter().skip(2).map(|s| s.message.clone()).collect::<Vec<_>>(), [vec![0x80, 60, 64], vec![0xB0, 64, 0], vec![0xB0, 123, 0]]);
    until("the new port opens", || rig.out.status().tracks.iter().all(|t| t.state == DestinationState::Connected));
    assert!(rig.playback.push(message(1, 0, [0x90, 67, 100])));
    until("the new port receives", || rig.fake.sent(&other).contains(&vec![0x90, 67, 100]));
    assert!(!rig.sent().iter().any(|m| m[0] & 0x0F == 1 && m[0] & 0xF0 != 0x90), "track 2 on the old port kept its note");
}

#[test]
fn device_loss_marks_the_destination_missing_and_the_returning_port_reconnects() {
    let mut rig = Rig::new(|key| vec![destination(1, key, MidiOutChannel::AsRecorded, 0)]);
    assert!(rig.playback.push(message(1, 0, [0x90, 60, 100])));
    rig.wait_sent(1);
    let (name, id) = (rig.key.name.clone(), rig.key.id.clone());
    rig.fake.remove_output(&rig.key);
    until("the destination is missing", || rig.out.status().track(TrackId(1)) == Some(&DestinationState::Missing));
    assert!(rig.playback.push(message(1, 0, [0x90, 62, 100])));
    until("the event is counted unsent", || rig.out.status().unsent >= 1);
    let key = rig.fake.add_output_with_id(&name, &id);
    until("the destination reconnects", || rig.out.status().track(TrackId(1)) == Some(&DestinationState::Connected));
    // A reconnect starts clean on the channels the port was used on.
    until("the reconnect resets", || rig.fake.sent(&key).len() >= 2);
    assert_eq!(rig.fake.sent(&key), [vec![0xB0, 64, 0], vec![0xB0, 123, 0]]);
    assert!(rig.playback.push(message(1, 0, [0x90, 64, 100])));
    until("the reconnected port receives", || rig.fake.sent(&key).contains(&vec![0x90, 64, 100]));
}

#[test]
fn a_port_absent_at_load_is_missing_and_sends_nothing_until_it_appears() {
    let fake = Fake::default();
    let origin = HostOrigin::default();
    let (mut out, mut playback, _echo) = Out::new();
    out.start(fake.io(), origin).unwrap();
    let absent = MidiOutPort { backend: "fake".into(), name: "Synth OUT".into(), ordinal: 0, id: "usb-synth".into() };
    let foreign = MidiOutPort { backend: "coremidi".into(), name: "Synth OUT".into(), ordinal: 0, id: String::new() };
    out.configure(vec![
        Destination { track: TrackId(1), port: absent, channel: MidiOutChannel::AsRecorded, offset_ns: 0 },
        Destination { track: TrackId(2), port: foreign, channel: MidiOutChannel::AsRecorded, offset_ns: 0 },
    ]);
    until("both are missing", || {
        let s = out.status();
        s.track(TrackId(1)) == Some(&DestinationState::Missing) && s.track(TrackId(2)) == Some(&DestinationState::Missing)
    });
    assert!(playback.push(message(1, 0, [0x90, 60, 100])));
    until("unsent", || out.status().unsent >= 1);
    let key = fake.add_output_with_id("Synth OUT", "usb-synth");
    until("it opens when it appears", || out.status().track(TrackId(1)) == Some(&DestinationState::Connected));
    assert_eq!(out.status().track(TrackId(2)), Some(&DestinationState::Missing), "a port of another backend never opens");
    assert!(playback.push(message(1, 0, [0x90, 61, 100])));
    until("sent", || fake.sent(&key).contains(&vec![0x90, 61, 100]));
    assert!(!fake.sent(&key).contains(&vec![0x90, 60, 100]));
    assert!(out.status().ports.iter().any(|p| p.key == key), "the picker lists the output");
}

#[test]
fn echo_goes_out_at_once_on_the_destination_channel_with_bounded_latency() {
    let mut rig = Rig::new(|key| vec![destination(5, key, MidiOutChannel::Channel(9), 250)]);
    let pushed = Instant::now();
    // Echo ignores the offset: live playing is never delayed for alignment.
    assert!(rig.echo.push(message(5, rig.now(), [0x90, 60, 100])));
    let sends = rig.wait_sent(1);
    assert_eq!(sends[0].message, vec![0x99, 60, 100]);
    let latency = sends[0].at - pushed;
    assert!(latency < Duration::from_millis(30), "echo took {latency:?}");
    // Stop ends notes that live input holds too.
    assert!(rig.playback.push(OutEvent { kind: Kind::Stop, ..OutEvent::default() }));
    let sends = rig.wait_sent(4);
    assert_eq!(sends.iter().skip(1).map(|s| s.message.clone()).collect::<Vec<_>>(), [vec![0x89, 60, 64], vec![0xB9, 64, 0], vec![0xB9, 123, 0]]);
}

#[test]
fn exit_releases_every_held_note_and_the_panic_releases_now() {
    let fake = Fake::default();
    let key = fake.add_output("Synth OUT");
    let (mut out, mut playback, _echo) = Out::new();
    out.start(fake.io(), HostOrigin::default()).unwrap();
    out.configure(vec![destination(1, &key, MidiOutChannel::AsRecorded, 0)]);
    assert!(playback.push(message(1, 0, [0x92, 60, 100])));
    until("sent", || fake.sent(&key).len() == 1);
    out.panic();
    until("panic", || fake.sent(&key).len() == 4);
    assert_eq!(fake.sent(&key)[1..], [vec![0x82, 60, 64], vec![0xB2, 64, 0], vec![0xB2, 123, 0]]);
    assert!(playback.push(message(1, 0, [0x92, 62, 100])));
    until("sent", || fake.sent(&key).len() == 5);
    drop(out);
    assert_eq!(fake.sent(&key)[5..], [vec![0x82, 62, 64], vec![0xB2, 64, 0], vec![0xB2, 123, 0]]);
}

#[test]
fn a_full_ring_keeps_room_for_releases() {
    let (out, mut playback, _echo) = Out::new();
    let mut accepted = 0;
    while playback.push(message(1, 0, [0x90, 60, 100])) {
        accepted += 1;
    }
    assert_eq!(accepted, QUEUE - RELEASE_RESERVE);
    assert!(playback.push(message(1, 0, [0x80, 60, 0])));
    assert!(playback.push(OutEvent { kind: Kind::Stop, ..OutEvent::default() }));
    assert_eq!(out.status().dropped, 1);
    assert_eq!(out.status().dropped_releases, 0);
}
