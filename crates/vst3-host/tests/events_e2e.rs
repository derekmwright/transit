//! F10 (ADR 0012 G5), VST3 side: the fixture's events synth ("Fixture Events")
//! writes every event and parameter point it receives into its output, so these tests see the
//! channel, voice id, offset and order of each one as the plug-in does.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_dsp::event::{Message, NoteId, PluginEvent};

mod common;
use common::fixture_dir;

/// The fixture's events synth, prepared for blocks of `block` frames.
fn synth(block: usize) -> Box<dyn Plugin> {
    let found = soundcraft_vst3_host::scan_paths(&[fixture_dir().to_path_buf()]);
    let id = found.into_iter().find(|d| d.name == "Fixture Events").unwrap().id;
    let mut p = soundcraft_vst3_host::create(&id).unwrap();
    p.prepare(48_000.0, block, 2);
    p
}

/// Runs one block and decodes the fixture's records: (code, offset, a, b).
fn records(p: &mut Box<dyn Plugin>, frames: usize) -> Vec<(u32, u32, u32, i64)> {
    let mut io = vec![vec![0.0f32; frames], vec![0.0f32; frames]];
    p.process(&mut io, frames);
    io[0].chunks(4).take_while(|r| r[0] != 0.0).map(|r| (r[0] as u32, r[1] as u32, r[2] as u32, r[3] as i64)).collect()
}

fn ev(channel: u8, note_id: i32, message: Message) -> PluginEvent {
    PluginEvent { channel, note_id, message }
}

#[test]
fn two_parameter_edges_per_block_reach_the_plug_in_in_order() {
    let mut p = synth(256);
    assert!(p.event(32, &ev(3, -1, Message::Control { cc: 64, value: 0 })));
    assert!(p.event(0, &ev(3, -1, Message::Control { cc: 64, value: 127 })));
    assert!(p.set_param_at(40, "5", 0.8));
    assert!(p.set_param_at(5, "5", 0.2));
    let r = records(&mut p, 256);
    let points = |id: u32| r.iter().filter(|x| x.0 == 4 && x.2 == id).map(|x| (x.1, x.3)).collect::<Vec<_>>();
    assert_eq!(points(103), vec![(0, 1_000_000), (32, 0)], "CC 64 on channel 4 drives its own channel's mapping: {r:?}");
    assert_eq!(points(5), vec![(5, 200_000), (40, 800_000)], "each set_param_at point, ascending");
    assert!(points(100).is_empty(), "channel 1's mapping is another parameter");
}

#[test]
fn notes_carry_their_channel_and_voice_id() {
    let mut p = synth(256);
    let n = NoteId::NONE;
    p.event(10, &ev(2, 7, Message::NoteOn { key: 60, velocity: 100, id: n }));
    p.event(10, &ev(9, 8, Message::NoteOn { key: 60, velocity: 100, id: n }));
    p.event(20, &ev(2, 7, Message::NoteOff { key: 60, velocity: 0, id: n }));
    p.event(30, &ev(9, 8, Message::PolyPressure { key: 60, value: 64, id: n }));
    let r = records(&mut p, 256);
    assert_eq!(r, vec![(1, 10, 2060, 7), (1, 10, 9060, 8), (2, 20, 2060, 7), (3, 30, 9060, 8)]);
}

#[test]
fn a_note_off_survives_an_overfull_queue() {
    let mut p = synth(4096);
    let n = NoteId::NONE;
    for i in 0..600u32 {
        assert!(p.event(1, &ev((i % 16) as u8, -1, Message::PolyPressure { key: (i % 128) as u8, value: 1, id: n })));
    }
    let admitted: Vec<i32> =
        (0..300).filter(|&i| p.event(2, &ev((i % 16) as u8, i, Message::NoteOn { key: (i % 128) as u8, velocity: 90, id: n }))).collect();
    assert_eq!(admitted.len(), 1024 - 257 - 600, "non-releases stop at the release reserve");
    for &i in &admitted {
        assert!(p.event(3, &ev((i % 16) as u8, i, Message::NoteOff { key: (i % 128) as u8, velocity: 0, id: n })), "the off of admitted note {i}");
    }
    let r = records(&mut p, 4096);
    let offs: Vec<i64> = r.iter().filter(|x| x.0 == 2).map(|x| x.3).collect();
    let ons: Vec<i64> = r.iter().filter(|x| x.0 == 1).map(|x| x.3).collect();
    assert_eq!(ons, admitted.iter().map(|&i| i64::from(i)).collect::<Vec<_>>());
    assert_eq!(offs, ons, "every admitted note-on got its off");
}

/// What `MixEngine::reset` does at Stop: the mix hands over the pedal-up and the note-off, then
/// `all_notes_off` and `reset`. The plug-in must still get both on the next block (review
/// F10-1: the reset cleared every controller point, so CC 64 0 was dropped and sustain stuck).
#[test]
fn a_reset_keeps_the_pedal_up_and_the_note_off() {
    let mut p = synth(256);
    let n = NoteId::NONE;
    assert!(p.event(0, &ev(0, -1, Message::Control { cc: 64, value: 127 })));
    assert!(p.event(0, &ev(0, 5, Message::NoteOn { key: 60, velocity: 100, id: n })));
    let r = records(&mut p, 256);
    assert!(r.contains(&(4, 0, 100, 1_000_000)) && r.contains(&(1, 0, 60, 5)), "down and on reached: {r:?}");
    assert!(p.event(0, &ev(0, -1, Message::Control { cc: 64, value: 0 })));
    assert!(p.event(0, &ev(0, 5, Message::NoteOff { key: 60, velocity: 0, id: n })));
    assert!(p.event(0, &ev(1, -1, Message::Control { cc: 64, value: 127 })), "a non-release, dropped by the reset");
    p.all_notes_off();
    p.reset();
    let r = records(&mut p, 256);
    assert!(r.contains(&(4, 0, 100, 0)), "the pedal-up survives the reset: {r:?}");
    assert!(r.contains(&(2, 0, 60, 5)), "the note-off survives the reset: {r:?}");
    assert!(!r.iter().any(|x| x.0 == 4 && x.2 == 101), "non-releases do not: {r:?}");
}
