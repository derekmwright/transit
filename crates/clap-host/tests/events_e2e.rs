//! F10 (ADR 0012 G5), CLAP side: the fixture's events plug-in (built with the `events` feature)
//! writes every event it receives into its output, so these tests see the channel, `note_id`,
//! time and order of each one as the plug-in does.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_dsp::event::{Message, NoteId, PluginEvent};

mod common;
use common::events_fixture_dir;

const EVENTS: &str = "clap:org.soundcraft.test.events";

fn plugin(block: usize) -> Box<dyn Plugin> {
    events_fixture_dir();
    let mut p = soundcraft_clap_host::create(EVENTS).unwrap();
    p.prepare(48_000.0, block, 2);
    p
}

/// Runs one block and decodes the plug-in's records: (code, time, a, b).
fn records(p: &mut Box<dyn Plugin>, frames: usize) -> Vec<(u32, u32, i64, i64)> {
    let mut io = vec![vec![0.0f32; frames], vec![0.0f32; frames]];
    p.process(&mut io, frames);
    io[0].chunks(4).take_while(|r| r[0] != 0.0).map(|r| (r[0] as u32, r[1] as u32, r[2] as i64, r[3] as i64)).collect()
}

fn ev(channel: u8, note_id: i32, message: Message) -> PluginEvent {
    PluginEvent { channel, note_id, message }
}

#[test]
fn notes_carry_channel_and_note_id_and_pressure_is_an_expression() {
    let mut p = plugin(256);
    let n = NoteId::NONE;
    p.event(10, &ev(2, 7, Message::NoteOn { key: 60, velocity: 127, id: n }));
    p.event(10, &ev(9, 8, Message::NoteOn { key: 60, velocity: 127, id: n }));
    p.event(20, &ev(9, 8, Message::PolyPressure { key: 60, value: 127, id: n }));
    p.event(30, &ev(2, 7, Message::NoteOff { key: 60, velocity: 0, id: n }));
    p.event(40, &ev(3, -1, Message::Control { cc: 1, value: 5 }));
    let r = records(&mut p, 256);
    assert_eq!(
        r,
        vec![(1, 10, 2060, 7), (1, 10, 9060, 8), (3, 20, 9060, 1_000_000), (2, 30, 2060, 7), (5, 40, 0xB3_01_05, 0)],
        "controllers go to the MIDI dialect with the channel nibble"
    );
}

#[test]
fn set_param_at_sends_one_point_per_offset() {
    let mut p = plugin(256);
    assert!(p.set_param_at(40, "5", 0.8));
    assert!(p.set_param_at(5, "5", 0.25));
    assert!(p.set_param_at(5, "5", 0.2), "the same offset coalesces");
    let r = records(&mut p, 256);
    assert_eq!(r, vec![(4, 5, 5, 200_000), (4, 40, 5, 800_000)]);
}

#[test]
fn a_note_off_survives_an_overfull_queue() {
    let mut p = plugin(4096);
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
    let ons: Vec<i64> = r.iter().filter(|x| x.0 == 1).map(|x| x.3).collect();
    let offs: Vec<i64> = r.iter().filter(|x| x.0 == 2).map(|x| x.3).collect();
    assert_eq!(ons, admitted.iter().map(|&i| i64::from(i)).collect::<Vec<_>>());
    assert_eq!(offs, ons, "every admitted note-on got its off");
}

/// What `MixEngine::reset` does at Stop: the mix hands over the pedal-up and note-offs, then
/// `all_notes_off` and `reset`. The plug-in must still get every release on the next block
/// (review F10-1: a reset kept only 0x80, so CC 64 0 was dropped and sustain stuck).
#[test]
fn a_reset_keeps_the_pedal_up_and_the_note_offs() {
    let mut p = plugin(256);
    let n = NoteId::NONE;
    assert!(p.event(0, &ev(0, -1, Message::Control { cc: 64, value: 127 })));
    assert!(p.event(0, &ev(0, 5, Message::NoteOn { key: 60, velocity: 100, id: n })));
    let r = records(&mut p, 256);
    assert_eq!(r, vec![(5, 0, 0xB0_40_7F, 0), (1, 0, 60, 5)]);
    assert!(p.event(0, &ev(0, -1, Message::Control { cc: 64, value: 0 })));
    assert!(p.event(0, &ev(0, -1, Message::Control { cc: 123, value: 0 })));
    assert!(p.event(0, &ev(0, 5, Message::NoteOff { key: 60, velocity: 0, id: n })));
    assert!(p.event(0, &ev(0, -1, Message::Control { cc: 1, value: 9 })), "a non-release, dropped by the reset");
    p.all_notes_off();
    p.reset();
    let r = records(&mut p, 256);
    assert!(r.contains(&(5, 0, 0xB0_40_00, 0)), "the pedal-up survives the reset: {r:?}");
    assert!(r.contains(&(5, 0, 0xB0_7B_00, 0)), "CC 123 survives the reset: {r:?}");
    assert!(r.contains(&(2, 0, 60, 5)), "the note-off survives the reset: {r:?}");
    assert!(!r.iter().any(|x| x.0 == 5 && x.2 == 0xB0_01_09), "non-releases do not: {r:?}");
}

fn midi_plugin(block: usize) -> Box<dyn Plugin> {
    events_fixture_dir();
    let mut p = soundcraft_clap_host::create("clap:org.soundcraft.test.events-midi").unwrap();
    p.prepare(48_000.0, block, 2);
    p
}

fn note_on(channel: u8, id: i32) -> PluginEvent {
    ev(channel, id, Message::NoteOn { key: 60, velocity: 100, id: NoteId::NONE })
}

fn note_off(channel: u8, id: i32) -> PluginEvent {
    ev(channel, id, Message::NoteOff { key: 60, velocity: 0, id: NoteId::NONE })
}

#[test]
fn midi_dialect_defers_off_until_the_last_identity_releases() {
    let mut p = midi_plugin(256);
    for first in [101, 102] {
        assert!(p.event(0, &note_on(3, 101)));
        assert!(p.event(1, &note_on(3, 102)));
        assert_eq!(records(&mut p, 256), vec![(5, 0, 0x933C64, 0), (5, 1, 0x933C64, 0)]);
        assert!(p.event(10, &note_off(3, first)));
        assert!(records(&mut p, 256).is_empty(), "the other source must keep sounding");
        assert!(p.event(20, &note_off(3, 203 - first)));
        assert_eq!(records(&mut p, 256), vec![(5, 20, 0x83_3C_00, 0)]);
    }
}

#[test]
fn midi_dialect_uses_delivery_order_and_keeps_channels_separate() {
    let mut p = midi_plugin(256);
    // Reverse arrival order; a future holder cannot suppress an earlier last-holder off.
    assert!(p.event(50, &note_on(3, 102)));
    assert!(p.event(10, &note_on(3, 101)));
    assert!(p.event(30, &note_off(3, 101)));
    assert!(p.event(0, &note_on(4, 101)));
    assert_eq!(records(&mut p, 256), vec![(5, 0, 0x943C64, 0), (5, 10, 0x933C64, 0), (5, 30, 0x83_3C_00, 0), (5, 50, 0x933C64, 0)]);
    assert!(p.event(15, &note_on(3, 103)));
    assert!(p.event(15, &note_off(3, 102)));
    assert!(p.event(25, &note_off(3, 103)));
    assert!(p.event(30, &note_off(4, 101)));
    assert_eq!(records(&mut p, 256), vec![(5, 15, 0x933C64, 0), (5, 25, 0x83_3C_00, 0), (5, 30, 0x84_3C_00, 0)]);
}

#[test]
fn midi_dialect_preserves_last_release_at_saturation_and_reclaims_identities() {
    let mut p = midi_plugin(4096);
    // More lifetimes than the bounded table can hold at once: slots must be reclaimed.
    for _ in 0..8 {
        for id in 0..224 {
            assert!(p.event(0, &note_on(3, id)));
        }
        records(&mut p, 4096);
        for i in 0..767 {
            assert!(p.event(i, &ev(3, -1, Message::Control { cc: 1, value: 1 })));
        }
        assert!(!p.event(0, &note_on(3, 9999)), "ordinary queue is saturated");
        for id in 0..224 {
            assert!(p.event(800, &note_off(3, id)));
        }
        let r = records(&mut p, 4096);
        assert_eq!(r.iter().filter(|r| r.2 == 0x83_3C_00).count(), 1);
        assert_eq!(r.last(), Some(&(5, 800, 0x83_3C_00, 0)));
    }
}

#[test]
fn midi_dialect_reset_and_all_notes_off_retire_remaining_holders() {
    for reset in [false, true] {
        let mut p = midi_plugin(256);
        p.event(0, &note_on(3, 101));
        p.event(0, &note_on(3, 102));
        records(&mut p, 256);
        p.event(0, &note_off(3, 101));
        assert!(records(&mut p, 256).is_empty());
        p.event(100, &note_on(3, 103));
        p.event(0, &ev(3, -1, Message::Control { cc: 64, value: 63 }));
        if reset {
            p.reset();
        } else {
            p.all_notes_off();
        }
        let r = records(&mut p, 256);
        assert_eq!(r.iter().filter(|r| r.2 == 0x83_3C_00).count(), 1);
        assert!(r.contains(&(5, 0, 0xB3_40_3F, 0)));
        assert!(!r.iter().any(|r| r.2 == 0x933C64));
        p.event(0, &note_on(3, 104));
        p.event(10, &note_off(3, 104));
        assert_eq!(records(&mut p, 256), vec![(5, 0, 0x933C64, 0), (5, 10, 0x83_3C_00, 0)]);
    }
}

#[test]
fn midi_dialect_velocity_zero_releases_its_identity() {
    let mut p = midi_plugin(256);
    p.event(0, &note_on(3, 101));
    p.event(0, &note_on(3, 102));
    records(&mut p, 256);
    for (id, last) in [(101, false), (102, true)] {
        assert!(p.event(10, &ev(3, id, Message::NoteOn { key: 60, velocity: 0, id: NoteId::NONE })));
        let r = records(&mut p, 256);
        assert_eq!(r, if last { vec![(5, 10, 0x83_3C_00, 0)] } else { vec![] });
    }
}
