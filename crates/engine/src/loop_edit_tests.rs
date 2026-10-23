//! Edits around a looped clip (ADR 0019) must not change how it sounds: every start-trim path
//! keeps the cycles on the old grid, consolidate writes the cycles out, and operations that render
//! audio to a new file clear the loop.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::Engine;
use serde_json::json;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_midi::{CtrlEvent, CtrlKind, Note, Sequence};
use soundcraft_model::{ChannelFormat, Clip, ClipId, Session, TrackId, TrackKind};
use soundcraft_time::{Range, Samples, TICKS_PER_QUARTER as Q};

fn note(pitch: u8, beat: i64) -> Note {
    Note { pitch, velocity: 100, release_velocity: 64, channel: 0, start: beat * Q, length: Q }
}

/// A 16-beat MIDI clip at 0: C60 at beat 0, E64 at beat 2, a CC at beat 1; looped over beats 0..4.
fn midi() -> (Engine, TrackId, ClipId, Samples) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Midi, ChannelFormat::Mono, None);
    let beat = s.tempo.tick_to_samples(Q, s.sample_rate);
    let mut seq = Sequence::default();
    seq.notes.push(note(60, 0));
    seq.notes.push(note(64, 2));
    seq.ctrls.push(CtrlEvent { tick: Q, channel: 0, kind: CtrlKind::Cc(1), value: 90 });
    let id = s.new_clip_id();
    crate::edit::place_clip(&mut s, t, Clip::midi(id, "m", 0, 16 * beat, seq));
    s.edit.selected_tracks = vec![t];
    let mut e = Engine::new(s);
    e.execute("clip.set_loop", &json!({"clip": id.0, "start": 0, "end": 4 * beat})).unwrap();
    (e, t, id, beat)
}

fn ons(e: &Engine, t: TrackId, beat: Samples) -> Vec<(f64, u8)> {
    soundcraft_mix::clip_note_ons(e.session(), t, 0, (20 * beat) as usize).into_iter().map(|(a, k)| (a as f64 / beat as f64, k)).collect()
}

fn full() -> Vec<(f64, u8)> {
    vec![(0.0, 60), (2.0, 64), (4.0, 60), (6.0, 64), (8.0, 60), (10.0, 64), (12.0, 60), (14.0, 64)]
}

#[test]
fn baseline_plays_every_cycle() {
    let (e, t, _, beat) = midi();
    assert_eq!(ons(&e, t, beat), full());
}

#[test]
fn trim_drag_keeps_the_cycles_on_the_old_grid_and_the_cut_note_silent() {
    let (mut e, t, id, beat) = midi();
    e.execute("edit.trim_clip", &json!({"clip": id.0, "start": beat})).unwrap();
    assert_eq!(ons(&e, t, beat), full()[1..].to_vec(), "no C at the new start; Cs at 4, 8, 12");
    // The controller the trim cut is kept: it still plays in the later cycles.
    let c = e.session().find_clip(id).unwrap().1.clone();
    let soundcraft_model::ClipContent::Midi { sequence } = &c.content else { panic!() };
    assert_eq!(sequence.ctrls.len(), 1);
}

#[test]
fn trim_start_into_a_later_cycle_keeps_the_phase() {
    let (mut e, t, _, beat) = midi();
    e.execute("edit.trim_start_to_insertion", &json!({"tracks": [t.0], "at": 9 * beat})).unwrap();
    assert_eq!(ons(&e, t, beat), vec![(10.0, 64), (12.0, 60), (14.0, 64)]);
}

#[test]
fn trim_to_selection_keeps_the_phase() {
    let (mut e, t, _, beat) = midi();
    e.execute("edit.trim_to_selection", &json!({"tracks": [t.0], "start": 9 * beat, "end": 15 * beat})).unwrap();
    assert_eq!(ons(&e, t, beat), vec![(10.0, 64), (12.0, 60), (14.0, 64)]);
}

#[test]
fn copy_range_and_paste_keeps_the_phase() {
    let (mut e, t, _, beat) = midi();
    let s = e.session_mut();
    let parts = crate::edit::copy_range(s, t, Range::new(beat, 16 * beat));
    let t2 = s.add_track(TrackKind::Midi, ChannelFormat::Mono, None);
    crate::edit::paste(s, t2, beat, &parts, 15 * beat, false).unwrap();
    assert_eq!(ons(&e, t2, beat), full()[1..].to_vec());
}

#[test]
fn split_keeps_both_halves_on_the_old_grid() {
    let (mut e, t, _, beat) = midi();
    e.execute("edit.separate", &json!({"tracks": [t.0], "at": 5 * beat})).unwrap();
    assert_eq!(e.session().track(t).unwrap().clips().len(), 2);
    assert_eq!(ons(&e, t, beat), full());
}

#[test]
fn consolidate_writes_the_cycles_out_as_they_sound() {
    let (mut e, t, _, beat) = midi();
    let before = ons(&e, t, beat);
    e.execute("edit.consolidate", &json!({"tracks": [t.0], "start": 0, "end": 16 * beat})).unwrap();
    let clips = e.session().track(t).unwrap().clips().to_vec();
    assert_eq!(clips.len(), 1);
    assert!(clips[0].looping.is_none());
    assert_eq!(ons(&e, t, beat), before);
    let soundcraft_model::ClipContent::Midi { sequence } = &clips[0].content else { panic!() };
    assert_eq!(sequence.ctrls.len(), 4, "the controller in every cycle");
}

fn audio() -> (Engine, TrackId, ClipId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    let data: Vec<f32> = (0..1000).map(|i| (i + 1) as f32 / 1000.0).collect();
    let src =
        crate::io::add_source(&mut s, "a", AudioBuffer { sample_rate: 48_000, channels: vec![data] }, None, soundcraft_audio_io::FileFormat::Wav);
    let id = s.new_clip_id();
    crate::edit::place_clip(&mut s, t, Clip::audio(id, "a", src, 0, 0, 100));
    s.edit.selected_tracks = vec![t];
    (Engine::new(s), t, id)
}

fn render(e: &Engine, t: TrackId, len: Samples) -> Vec<f32> {
    soundcraft_mix::render_clips(e.session(), t, Range::new(0, len)).remove(0)
}

#[test]
fn reverse_of_a_looped_clip_equals_the_reversed_live_render() {
    let (mut e, t, id) = audio();
    e.execute("clip.set_loop", &json!({"clip": id.0, "start": 20, "end": 100})).unwrap();
    e.execute("clip.loop_to", &json!({"clip": id.0, "to": 330})).unwrap();
    let mut want = render(&e, t, 330);
    want.reverse();
    e.execute("audiosuite.process", &json!({"process": "reverse", "clip": id.0})).unwrap();
    let c = e.session().track(t).unwrap().clips()[0].clone();
    assert_eq!(c.looping, None, "the new file holds every cycle");
    assert_eq!(render(&e, t, 330), want);
}

#[test]
fn time_stretch_of_a_looped_clip_stretches_every_cycle() {
    let (mut e, t, id) = audio();
    e.execute("clip.loop_to", &json!({"clip": id.0, "to": 400})).unwrap();
    // The live render of all four cycles, stretched the way the process does.
    let live = render(&e, t, 400);
    let want = soundcraft_dsp::offline::time_stretch(&[live], 2.0, 48_000.0).remove(0);
    e.execute("audiosuite.process", &json!({"process": "time_stretch", "clip": id.0, "params": {"ratio": 2.0}})).unwrap();
    let c = e.session().track(t).unwrap().clips()[0].clone();
    assert_eq!((c.looping, c.length), (None, 800));
    assert_eq!(render(&e, t, 800), want[..800].to_vec());
}

#[test]
fn a_loop_must_be_at_least_the_minimum_cycle() {
    let (mut e, _, id) = audio();
    assert!(e.execute("clip.set_loop", &json!({"clip": id.0, "start": 0, "end": 10})).is_err());
    assert!(e.execute("clip.set_loop", &json!({"clip": id.0, "start": 0, "end": 64})).is_ok());
}

#[test]
fn set_loop_checks_the_file_through_the_stretch() {
    let (mut e, _, id) = audio();
    e.session_mut().find_clip_mut(id).unwrap().stretch = 2.0;
    // Clip time 0..2000 reads 0..1000 of the 1000-frame file at stretch 2.
    assert!(e.execute("clip.set_loop", &json!({"clip": id.0, "start": 0, "end": 2000})).is_ok());
    assert!(e.execute("clip.set_loop", &json!({"clip": id.0, "start": 0, "end": 2100})).is_err());
}

#[test]
fn remove_pitch_shift_of_a_looped_stretched_clip_reads_the_file_at_the_stretch() {
    let (mut e, t, id) = audio();
    e.session_mut().find_clip_mut(id).unwrap().stretch = 2.0;
    // Clip time 0..100 is file 0..50 at stretch 2; loop it out to 800.
    e.execute("clip.loop_to", &json!({"clip": id.0, "to": 800})).unwrap();
    let before = render(&e, t, 800);
    let peak = |v: &[f32]| v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    e.execute("clip.remove_pitch_shift", &json!({"clip": id.0})).unwrap();
    let c = e.session().track(t).unwrap().clips()[0].clone();
    assert_eq!(c.looping, None);
    let after = render(&e, t, c.length);
    // The source ramps to 0.05 inside a cycle's file range; reading past it (the old bug) reaches 0.1.
    assert!(peak(&after) < peak(&before) * 1.3, "{} vs {}", peak(&after), peak(&before));
}

/// A time-locked empty clip (when `with_lock`) and the looped clip of `midi()` at beat 8, retimed
/// from 120 to 240 bpm.
fn retime_rig(lock_beats: i64, with_lock: bool) -> (Session, TrackId, Samples) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Midi, ChannelFormat::Mono, None);
    let beat = s.tempo.tick_to_samples(Q, s.sample_rate);
    if with_lock {
        let id = s.new_clip_id();
        let mut a = Clip::midi(id, "lock", 0, lock_beats * beat, Sequence::default());
        a.time_locked = true;
        crate::edit::place_clip(&mut s, t, a);
    }
    let mut seq = Sequence::default();
    seq.notes.push(note(60, 0));
    seq.notes.push(note(64, 2));
    let id = s.new_clip_id();
    let mut b = Clip::midi(id, "m", 8 * beat, 16 * beat, seq);
    b.looping = Some(soundcraft_model::ClipLoop { start: 0, end: 4 * beat });
    crate::edit::place_clip(&mut s, t, b);
    let old = s.tempo.clone();
    s.tempo.set_tempo(0, 240.0).unwrap();
    crate::retime::retime_to_tempo(&mut s, &old).unwrap();
    (s, t, beat)
}

#[test]
fn retime_trims_a_looped_midi_clip_through_the_start_helper() {
    // At 240 bpm the looped clip starts at beat 8 and the time-locked clip ends at beat 10.
    let (s, t, beat) = retime_rig(5, true);
    let (r, rt, _) = retime_rig(5, false);
    let half = beat / 2;
    let ons = |s: &Session, t: TrackId| -> Vec<(f64, u8)> {
        soundcraft_mix::clip_note_ons(s, t, 0, (40 * half) as usize).into_iter().map(|(a, k)| (a as f64 / half as f64, k)).collect()
    };
    let want: Vec<(f64, u8)> = ons(&r, rt).into_iter().filter(|(a, _)| *a >= 10.0).collect();
    assert_eq!(ons(&s, t), want);
}
