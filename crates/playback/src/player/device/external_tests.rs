#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! External MIDI output (#61) through the real Player: the audio callback schedules a MIDI
//! track's clip into the output thread, which sends on the fake backend. No hardware MIDI.
use super::*;
use soundcraft_model::{ChannelFormat, Clip, ClipId, MidiOutChannel, MidiOutPort, MidiOutput, TrackKind};
use std::time::{Duration, Instant};
use transit_midi_io::fake::Fake;
use transit_rt::testing::measure;

const QUARTER: i64 = 24_000;

fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "did not reach: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A MIDI track (no instrument) playing to `Synth OUT`: C at 0 for an eighth, E from the eighth
/// for a quarter, so E sounds across a one-beat loop's end.
fn session(fake: &Fake) -> (Session, TrackId, transit_midi_io::PortKey) {
    let key = fake.add_output("Synth OUT");
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Midi, ChannelFormat::Mono, None);
    let ppq = soundcraft_time::TICKS_PER_QUARTER;
    // Playback does not depend on the sequence crate; the clip's sequence comes from its JSON.
    let seq = serde_json::from_value(serde_json::json!({"notes": [
        {"pitch": 60, "velocity": 100, "release_velocity": 64, "channel": 4, "start": 0, "length": ppq / 2},
        {"pitch": 64, "velocity": 90, "release_velocity": 64, "channel": 4, "start": ppq / 2, "length": ppq},
    ], "ctrls": []}))
    .unwrap();
    let track = s.track_mut(t).unwrap();
    track.playlist_mut().unwrap().clips.push(Clip::midi(ClipId(1), "phrase", 0, 4 * QUARTER, seq));
    track.midi_output = MidiOutput::External {
        port: MidiOutPort { backend: key.backend.as_str().into(), name: key.name.clone(), ordinal: key.ordinal, id: key.id.clone() },
        channel: MidiOutChannel::AsRecorded,
        offset_ms: 0.0,
    };
    (s, t, key)
}

#[test]
fn a_looped_midi_track_plays_its_clip_to_the_port_off_the_audio_thread_and_stop_releases() {
    let fake = Fake::default();
    let (s, track, key) = session(&fake);
    assert_eq!(s.sample_rate.hz(), 48_000);
    let (mut player, access, _clock) = Player::prepare(Arc::new(s), 48_000, soundcraft_model::IoMap::default()).unwrap();
    player.midi.get_mut().as_mut().unwrap().enable(fake.io());
    let mut callback = DeviceOutput::new(access, &player, 2);
    until("the destination opens", || {
        player.service().unwrap();
        player.midi_status().unwrap().output.track(track) == Some(&crate::player::midi_out::DestinationState::Connected)
    });
    // This thread plays the device callback, marked as the audio thread like a real one.
    crate::mark_audio_thread();
    let audio = std::thread::current().id();
    let mut block = [0.0f32; 1024];
    callback.fill_f32(&mut block);
    player.play(0, None, Some(soundcraft_time::Range { start: 0, end: QUARTER })).unwrap();
    // Real-time pacing (512 frames is 10.7 ms at 48 kHz): the fallback clock stamps blocks by
    // callback time, so callbacks must not run ahead of the wall clock. 80 blocks are 40 960
    // samples: lap 2 stops inside E.
    for _ in 0..80 {
        let started = Instant::now();
        let ((), heap) = measure(|| callback.fill_f32(&mut block));
        assert!(heap.is_zero(), "the callback allocated while scheduling external MIDI: {heap:?}");
        std::thread::sleep(Duration::from_micros(10_700).saturating_sub(started.elapsed()));
    }
    player.stop().unwrap();
    for _ in 0..4 {
        callback.fill_f32(&mut block);
        std::thread::sleep(Duration::from_millis(11));
    }
    soundcraft_mix::unmark_audio_thread();
    until("Stop's All Notes Off", || fake.sent(&key).contains(&vec![0xB4, 123, 0]));
    let sends = fake.sends(&key);
    let messages: Vec<Vec<u8>> = sends.iter().map(|s| s.message.clone()).collect();
    // Lap 1: C, then C off and E at the eighth. The wrap ends E (it sounds across the loop end);
    // lap 2 starts C again. Stop releases what is held and resets the channel.
    let expected_start: Vec<Vec<u8>> = vec![
        vec![0x94, 60, 100],
        vec![0x84, 60, 64],
        vec![0x94, 64, 90],
        vec![0x84, 64, 64],
        vec![0x94, 60, 100],
        vec![0x84, 60, 64],
        vec![0x94, 64, 90],
    ];
    assert_eq!(messages.get(..expected_start.len()), Some(&expected_start[..]), "{messages:x?}");
    assert_eq!(messages.get(messages.len() - 2..), Some(&[vec![0xB4, 64, 0], vec![0xB4, 123, 0]][..]), "{messages:x?}");
    assert!(messages.contains(&vec![0x84, 64, 64]));
    for send in &sends {
        assert_ne!(send.thread, audio, "a MIDI send ran on the audio thread");
    }
    let status = player.midi_status().unwrap().output;
    assert_eq!(status.audio_thread_sends, 0);
    assert_eq!(status.dropped, 0);
    // E is held at the stop: its off comes first, then the channel reset.
    assert_eq!(messages.get(expected_start.len()), Some(&vec![0x84, 64, 64]), "{messages:x?}");
}

#[test]
fn live_input_on_an_armed_external_track_is_echoed_without_an_instrument() {
    let fake = Fake::default();
    let (mut s, track, key) = session(&fake);
    let input = fake.add_input("Keys");
    s.track_mut(track).unwrap().mixer.record_arm = true;
    s.track_mut(track).unwrap().midi_output = MidiOutput::External {
        port: MidiOutPort { backend: key.backend.as_str().into(), name: key.name.clone(), ordinal: key.ordinal, id: key.id.clone() },
        channel: MidiOutChannel::Channel(1),
        offset_ms: 100.0,
    };
    let (mut player, access, _clock) = Player::prepare(Arc::new(s), 48_000, soundcraft_model::IoMap::default()).unwrap();
    player.midi.get_mut().as_mut().unwrap().enable(fake.io());
    let mut callback = DeviceOutput::new(access, &player, 2);
    until("the input route and the destination are ready", || {
        player.service().unwrap();
        callback.fill_f32(&mut [0.0f32; 256]);
        let status = player.midi_status().unwrap();
        status.routes == 1 && status.output.track(track) == Some(&crate::player::midi_out::DestinationState::Connected)
    });
    let played = Instant::now();
    assert!(fake.input_port(&input).unwrap().deliver(0, &[0x90, 60, 100]));
    until("the echo", || fake.sent(&key).contains(&vec![0x91, 60, 100]));
    let latency = fake.sends(&key).iter().find(|s| s.message == vec![0x91, 60, 100]).unwrap().at - played;
    // Two 1 ms polling threads (ingress, output); generous for a loaded CI machine.
    assert!(latency < Duration::from_millis(50), "echo took {latency:?}");
    assert!(fake.input_port(&input).unwrap().deliver(1_000, &[0x80, 60, 0]));
    until("the echoed release", || fake.sent(&key).contains(&vec![0x81, 60, 0]));
}
