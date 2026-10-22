//! Track > Freeze, Unfreeze and Flatten: a frozen track plays its render and makes no plug-in
//! instances, refuses edits to what was rendered, and every step is one undo.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use soundcraft_engine::{Engine, EngineError};
use soundcraft_mix::{DeviceGeneration, InstanceSpec, PlanCompiler, PlanLimits, RenderTarget, SlotGen, create_instance};
use soundcraft_model::{ClipContent, Session, TrackId, TrackKind};
use soundcraft_time::Range;
use std::sync::Arc;

/// An instrument track (a synth with a note) through a latent insert, with its fader down.
fn engine() -> (Engine, TrackId) {
    let mut e = Engine::default();
    let r = e.execute("track.new", &json!({"kind": "instrument", "name": "Keys", "instrument": "subtractive_synth"})).unwrap();
    let track = r["tracks"][0].as_u64().unwrap();
    let r = e.execute("midi.new_clip", &json!({"track": track, "start": 12_000, "length": 48_000})).unwrap();
    let clip = r["clip"].as_u64().unwrap();
    e.execute("event.midi_note", &json!({"clip": clip, "pitch": 60, "start_ticks": 0, "length_ticks": 960, "velocity": 100})).unwrap();
    e.execute("mix.insert", &json!({"track": track, "slot": 0, "plugin": "maximizer"})).unwrap();
    e.execute("mix.volume", &json!({"tracks": [track], "db": -6.0})).unwrap();
    (e, TrackId(track))
}

fn keys(e: &Engine, t: TrackId) -> soundcraft_model::Track {
    e.session().track(t).unwrap().clone()
}

fn mix(e: &Engine) -> Vec<Vec<f32>> {
    soundcraft_mix::render_range(e.session(), Range::new(0, 96_000), 512)
}

fn peak(a: &[Vec<f32>]) -> f32 {
    a.iter().flatten().map(|x| x.abs()).fold(0.0f32, f32::max)
}

/// How many plug-ins a live compile of `s` asks the factory for.
fn instances_made(s: &Session) -> usize {
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: s.sample_rate.hz(), max_block: 256, out_channels: 0, in_channels: 0 };
    let mut comp = PlanCompiler::new(target, PlanLimits::default());
    let made = std::cell::Cell::new(0usize);
    let mut make = |spec: &InstanceSpec, _: SlotGen| {
        made.set(made.get() + 1);
        create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 256, spec.ch)
    };
    let next = comp.active().checked_next().unwrap();
    let env = comp.compile(Arc::new(s.clone()), comp.active(), next, &mut make).unwrap();
    comp.reclaim(env);
    made.get()
}

fn source_audio(e: &Engine, id: soundcraft_model::SourceId) -> Vec<Vec<f32>> {
    e.session().pool.get(id).unwrap().buffer.channels.clone()
}

#[test]
fn a_freeze_is_the_pre_fader_render_a_bounce_in_place_makes() {
    let (mut e, t) = engine();
    let (mut b, _) = engine();
    let made = soundcraft_engine::io::commit_track(&mut b, t).unwrap().unwrap();
    let ClipContent::Audio { source, .. } = b.session().track(made).unwrap().clips()[0].content else { panic!("an audio clip") };
    let bounced = source_audio(&b, source);
    assert!(peak(&bounced) > 1e-4, "the fixture makes sound");

    e.execute("track.freeze", &json!({"tracks": [t.0]})).unwrap();
    let f = keys(&e, t).freeze.unwrap();
    assert_eq!(source_audio(&e, f.source), bounced, "bit for bit, with the fader out of it");
    assert!(f.latency > 0, "the maximizer's latency is kept: {f:?}");
    assert_eq!(f.hash, keys(&e, t).freeze_hash());
    assert_eq!((f.start, f.length), (0, 48_000 + 12_000 + 96_000));
}

#[test]
fn a_frozen_track_plays_the_render_with_no_instances_and_unfreeze_brings_them_back() {
    let (mut e, t) = engine();
    let live = mix(&e);
    assert_eq!(instances_made(e.session()), 2, "the synth and the maximizer");
    e.execute("track.freeze", &json!({"tracks": [t.0]})).unwrap();
    assert_eq!(instances_made(e.session()), 0);
    let frozen = mix(&e);
    let worst = live.iter().flatten().zip(frozen.iter().flatten()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    assert!(worst < 1e-4 && peak(&live) > 1e-4, "the render sounds as the live track did: {worst}");

    // The fader stays live on a frozen track.
    e.execute("mix.volume", &json!({"tracks": [t.0], "db": -12.0})).unwrap();
    assert!(keys(&e, t).freeze.is_some());
    assert!(peak(&mix(&e)) < peak(&frozen));
    e.execute("mix.volume", &json!({"tracks": [t.0], "db": -6.0})).unwrap();

    e.execute("track.unfreeze", &json!({"tracks": [t.0]})).unwrap();
    assert!(keys(&e, t).freeze.is_none());
    assert_eq!(instances_made(e.session()), 2);
    assert_eq!(mix(&e), live, "the same sound as before the freeze");
}

#[test]
fn an_edit_to_what_was_rendered_is_refused_and_changes_nothing() {
    let (mut e, t) = engine();
    e.execute("track.freeze", &json!({"tracks": [t.0]})).unwrap();
    let before = e.session_arc();
    let clip = keys(&e, t).clips()[0].id.0;
    let refused = [
        ("event.midi_note", json!({"clip": clip, "pitch": 64, "start_ticks": 0, "length_ticks": 960})),
        ("mix.insert_param", json!({"track": t.0, "slot": 0, "param": "ceiling", "value": -3.0})),
        ("mix.insert_remove", json!({"track": t.0, "slot": 0})),
        ("mix.insert", json!({"track": t.0, "slot": 1, "plugin": "compressor"})),
        ("edit.move_clips", json!({"clip": clip, "by": 480})),
    ];
    for (id, params) in refused {
        let err = e.execute(id, &params).unwrap_err();
        assert_eq!(err, EngineError::TrackFrozen, "{id}");
        assert_eq!(err.to_string(), "Track is frozen; unfreeze to edit");
        assert!(Arc::ptr_eq(&before, &e.session_arc()), "{id} left the session as it was");
    }
    for (id, params) in [
        ("mix.volume", json!({"tracks": [t.0], "db": -3.0})),
        ("mix.pan", json!({"tracks": [t.0], "pan": 0.5})),
        ("mix.mute", json!({"tracks": [t.0]})),
    ] {
        e.execute(id, &params).unwrap();
    }
}

#[test]
fn freeze_is_one_undo_step_that_unfreezes_and_keeps_the_render_in_the_pool() {
    let (mut e, t) = engine();
    let steps = e.undo_history().len();
    e.execute("track.freeze", &json!({"tracks": [t.0]})).unwrap();
    assert_eq!(e.undo_history().len(), steps + 1);
    let source = keys(&e, t).freeze.unwrap().source;
    assert!(e.undo());
    assert!(keys(&e, t).freeze.is_none());
    assert!(!e.session().sources.iter().any(|s| s.id == source), "the render is not written until the session is saved");
    assert!(e.redo());
    assert_eq!(keys(&e, t).freeze.unwrap().source, source);
    assert!(e.session().pool.contains(source), "redo brings the render back");
}

#[test]
fn flatten_makes_an_audio_track_of_the_render_and_one_undo_restores_everything() {
    let (mut e, t) = engine();
    let before = keys(&e, t);
    let steps = e.undo_history().len();
    e.execute("track.flatten", &json!({"tracks": [t.0]})).unwrap();
    assert_eq!(e.undo_history().len(), steps + 1);
    let flat = keys(&e, t);
    assert_eq!(flat.kind, TrackKind::Audio);
    assert!(flat.freeze.is_none() && flat.instrument.is_none() && flat.mixer.inserts.iter().all(Option::is_none));
    assert_eq!((flat.clips().len(), flat.mixer.volume_db), (1, -6.0), "one audio clip, the fader kept");
    assert!(flat.clips()[0].is_audio());
    assert_eq!(instances_made(e.session()), 0);
    assert!(peak(&mix(&e)) > 1e-4);
    assert!(e.undo());
    assert_eq!(keys(&e, t), before, "instrument, inserts, state and clips are back");
}

#[test]
fn a_session_with_a_freeze_round_trips_and_older_files_load() {
    let (mut e, t) = engine();
    let unfrozen = serde_json::to_value(e.session()).unwrap();
    assert!(unfrozen["tracks"][0].get("freeze").is_none(), "an unfrozen track writes nothing new");
    e.execute("track.freeze", &json!({"tracks": [t.0]})).unwrap();
    let json = serde_json::to_value(e.session()).unwrap();
    let back: Session = serde_json::from_value(json).unwrap();
    assert_eq!(back.track(t).unwrap().freeze, keys(&e, t).freeze);
    // A file from before freezing existed, and one that carries the old `frozen` flag.
    let mut old = unfrozen.clone();
    old["tracks"][0]["frozen"] = json!(true);
    let loaded: Session = serde_json::from_value(old).unwrap();
    assert!(loaded.track(t).unwrap().freeze.is_none());
}

/// A synth track playing one held note.
fn synth(e: &mut Engine, name: &str, pitch: u32, vel: u32) -> TrackId {
    let r = e.execute("track.new", &json!({"kind": "instrument", "name": name, "instrument": "subtractive_synth"})).unwrap();
    let track = r["tracks"][0].as_u64().unwrap();
    let r = e.execute("midi.new_clip", &json!({"track": track, "start": 12_000, "length": 48_000})).unwrap();
    let clip = r["clip"].as_u64().unwrap();
    e.execute("event.midi_note", &json!({"clip": clip, "pitch": pitch, "start_ticks": 0, "length_ticks": 1920, "velocity": vel})).unwrap();
    TrackId(track)
}

/// A compressor on slot `slot` of `t` that clamps hard on anything over -50 dB.
fn clamp(e: &mut Engine, t: TrackId, slot: usize) {
    e.execute("mix.insert", &json!({"track": t.0, "slot": slot, "plugin": "compressor"})).unwrap();
    e.execute("mix.insert_param", &json!({"track": t.0, "slot": slot, "param": "threshold", "value": -50.0})).unwrap();
    e.execute("mix.insert_param", &json!({"track": t.0, "slot": slot, "param": "ratio", "value": 20.0})).unwrap();
}

fn diff(a: &[Vec<f32>], b: &[Vec<f32>]) -> f32 {
    a.iter().flatten().zip(b.iter().flatten()).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

#[test]
fn a_keyed_insert_on_a_frozen_track_keeps_its_ducking() {
    let mut e = Engine::default();
    let kick = synth(&mut e, "Kick", 36, 127);
    let bass = synth(&mut e, "Bass", 48, 100);
    clamp(&mut e, bass, 0);
    let unkeyed = mix(&e);
    e.execute("mix.insert_key", &json!({"track": bass.0, "slot": 0, "from": {"track": kick.0}})).unwrap();
    let live = mix(&e);
    assert!(diff(&live, &unkeyed) > 1e-3, "the key changes the sound live");
    e.execute("track.freeze", &json!({"tracks": [bass.0]})).unwrap();
    let frozen = mix(&e);
    assert!(diff(&frozen, &live) <= 1e-4, "frozen {} off live", diff(&frozen, &live));
    assert!(diff(&frozen, &unkeyed) > 1e-3);
}

#[test]
fn a_key_from_a_keyed_source_keeps_that_sources_own_key_running() {
    let mut e = Engine::default();
    let drums = synth(&mut e, "Drums", 36, 127);
    let kick = synth(&mut e, "Kick", 40, 110);
    let bass = synth(&mut e, "Bass", 48, 100);
    clamp(&mut e, kick, 0);
    clamp(&mut e, bass, 0);
    e.execute("mix.insert_key", &json!({"track": kick.0, "slot": 0, "from": {"track": drums.0}})).unwrap();
    e.execute("mix.insert_key", &json!({"track": bass.0, "slot": 0, "from": {"track": kick.0}})).unwrap();
    let live = mix(&e);
    e.execute("track.freeze", &json!({"tracks": [bass.0]})).unwrap();
    assert!(diff(&mix(&e), &live) <= 1e-4);
}

#[test]
fn a_frozen_track_with_latency_keys_another_track_as_it_did_live() {
    for tap in ["pre", "post"] {
        let mut e = Engine::default();
        let kick = synth(&mut e, "Kick", 36, 127);
        let pad = synth(&mut e, "Pad", 60, 100);
        clamp(&mut e, pad, 0);
        e.execute("mix.insert", &json!({"track": kick.0, "slot": 0, "plugin": "maximizer"})).unwrap();
        e.execute("mix.insert_key", &json!({"track": pad.0, "slot": 0, "from": {"track": kick.0}, "tap": tap})).unwrap();
        let live = mix(&e);
        e.execute("track.freeze", &json!({"tracks": [kick.0]})).unwrap();
        assert!(e.session().track(kick).unwrap().freeze.as_ref().unwrap().latency > 0);
        let frozen = mix(&e);
        assert!(diff(&frozen, &live) <= 1e-4, "{tap} tap: frozen {} off live", diff(&frozen, &live));
    }
}

#[test]
fn a_frozen_track_refuses_a_key_from_another_track() {
    let mut e = Engine::default();
    let kick = synth(&mut e, "Kick", 36, 127);
    let bass = synth(&mut e, "Bass", 48, 100);
    clamp(&mut e, bass, 0);
    e.execute("track.freeze", &json!({"tracks": [bass.0]})).unwrap();
    let r = e.execute("mix.insert_key", &json!({"track": bass.0, "slot": 0, "from": {"track": kick.0}}));
    assert!(matches!(r, Err(EngineError::TrackFrozen)), "{r:?}");
    assert!(e.session().track(bass).unwrap().mixer.inserts[0].as_ref().unwrap().key.is_none());
}
