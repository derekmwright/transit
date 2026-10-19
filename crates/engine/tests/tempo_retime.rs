//! Tempo changes keep musical-time (ticks-timebase) material on its bars.
//!
//! Clip starts are stored in samples, so every command that changes the tempo map has to move the
//! clips (and automation, markers, key changes and the selection) of ticks-timebase tracks to the
//! sample positions their bars now fall on. Samples-timebase tracks never move.

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_model::{Clip, ClipId, Session, Source, SourceId, TrackId};
use soundcraft_time::{BarBeat, Samples};

/// One bar of 4/4 at 120 BPM and 48 kHz.
const BAR: Samples = 96_000;

fn bar(n: i64) -> Samples {
    (n - 1) * BAR
}

fn run(e: &mut Engine, id: &str, p: Value) -> Value {
    e.execute(id, &p).unwrap_or_else(|err| panic!("{id} {p}: {err}"))
}

fn new_track(e: &mut Engine, kind: &str) -> TrackId {
    TrackId(run(e, "track.new", json!({"kind": kind}))["tracks"][0].as_u64().unwrap())
}

fn midi_clip(e: &mut Engine, t: TrackId, start: Samples, length: Samples) -> ClipId {
    ClipId(run(e, "midi.new_clip", json!({"track": t.0, "start": start, "length": length}))["clip"].as_u64().unwrap())
}

fn source(id: u64) -> Source {
    Source {
        id: SourceId(id),
        name: "src".into(),
        path: "src.wav".into(),
        channels: 1,
        frames: 10_000_000,
        sample_rate: 48_000,
        format: soundcraft_audio_io::FileFormat::Wav,
        time_reference: 0,
        unsaved: false,
    }
}

/// An audio track holding clips at the given (start, length).
fn audio_track(e: &mut Engine, ticks: bool, spans: &[(Samples, Samples)]) -> (TrackId, Vec<ClipId>) {
    let t = new_track(e, "audio");
    run(e, "track.timebase", json!({"tracks": [t.0], "ticks": ticks}));
    let s = e.session_mut();
    if s.sources.is_empty() {
        s.sources.push(source(7));
    }
    let ids: Vec<ClipId> = spans.iter().map(|_| s.new_clip_id()).collect();
    s.track_mut(t).and_then(|t| t.playlist_mut()).unwrap().clips =
        spans.iter().zip(&ids).map(|(&(start, len), id)| Clip::audio(*id, "a", SourceId(7), 1_000, start, len)).collect();
    (t, ids)
}

fn clip(e: &Engine, id: ClipId) -> Clip {
    e.session().find_clip(id).unwrap().1.clone()
}

fn bb(s: &Session, at: Samples) -> BarBeat {
    s.tempo.bar_beat_at(at, s.sample_rate)
}

fn on_bar(s: &Session, at: Samples, b: i64) {
    assert_eq!(bb(s, at), BarBeat { bar: b, beat: 1, tick: 0 }, "sample {at} must start bar {b}");
}

/// A MIDI clip spanning bars 5 and 6, with a note on its first beat.
fn midi_at_bar_5() -> (Engine, TrackId, ClipId) {
    let mut e = Engine::default();
    let t = new_track(&mut e, "midi");
    let c = midi_clip(&mut e, t, bar(5), 2 * BAR);
    run(&mut e, "midi.note_add", json!({"clip": c.0, "pitch": 60, "start_ticks": 0, "length_ticks": 960}));
    (e, t, c)
}

#[test]
fn a_midi_clip_at_bar_5_stays_at_bar_5_when_the_tempo_halves() {
    let (mut e, _, c) = midi_at_bar_5();
    run(&mut e, "event.tempo", json!({"bpm": 60, "at": 0}));
    let k = clip(&e, c);
    on_bar(e.session(), k.start, 5);
    on_bar(e.session(), k.end(), 7);
    assert_eq!((k.start, k.length), (2 * bar(5), 4 * BAR), "twice as many samples per bar at 60 BPM");
}

#[test]
fn a_midi_clip_at_bar_5_stays_at_bar_5_when_the_tempo_rises() {
    let (mut e, _, c) = midi_at_bar_5();
    run(&mut e, "event.tempo", json!({"bpm": 150, "at": 0}));
    let k = clip(&e, c);
    on_bar(e.session(), k.start, 5);
    on_bar(e.session(), k.end(), 7);
}

#[test]
fn a_samples_timebase_audio_clip_keeps_its_samples() {
    let mut e = Engine::default();
    let (_, ids) = audio_track(&mut e, false, &[(bar(5), 50_000)]);
    let before = clip(&e, ids[0]);
    run(&mut e, "event.tempo", json!({"bpm": 60, "at": 0}));
    assert_eq!(clip(&e, ids[0]), before);
    run(&mut e, "event.tempo", json!({"bpm": 150, "at": 0}));
    assert_eq!(clip(&e, ids[0]), before);
}

#[test]
fn a_ticks_timebase_audio_clip_moves_its_start_and_keeps_its_audio() {
    let mut e = Engine::default();
    let (_, ids) = audio_track(&mut e, true, &[(bar(5), 50_000)]);
    let before = clip(&e, ids[0]);
    run(&mut e, "event.tempo", json!({"bpm": 60, "at": 0}));
    let k = clip(&e, ids[0]);
    on_bar(e.session(), k.start, 5);
    assert_eq!((k.length, k.source_offset()), (before.length, before.source_offset()), "audio is not stretched");
}

#[test]
fn faster_tempo_never_overlaps_ticks_timebase_audio_clips() {
    let mut e = Engine::default();
    // Back to back from bar 2, each one bar long.
    let (t, ids) = audio_track(&mut e, true, &[(bar(2), BAR), (bar(3), BAR), (bar(4), BAR)]);
    run(&mut e, "event.tempo", json!({"bpm": 240, "at": 0}));
    let s = e.session();
    let cl = s.track(t).unwrap().clips();
    for (c, b) in cl.iter().zip([2, 3, 4]) {
        on_bar(s, c.start, b);
    }
    for w in cl.windows(2) {
        assert!(w[0].end() <= w[1].start, "{:?} overlaps {:?}", w[0].range(), w[1].range());
    }
    // The last clip has nothing after it and keeps its full length.
    assert_eq!(clip(&e, ids[2]).length, BAR);
}

#[test]
fn removing_a_tempo_change_keeps_midi_clips_on_their_bars() {
    let (mut e, _, c) = midi_at_bar_5();
    run(&mut e, "event.tempo", json!({"bpm": 90, "at": bar(3)}));
    on_bar(e.session(), clip(&e, c).start, 5);
    let at = e.session().tempo.tick_to_samples(2 * 4 * 960, e.session().sample_rate);
    let r = run(&mut e, "event.tempo_remove", json!({"at": at}));
    assert_eq!(r["removed"], json!(true));
    assert_eq!(clip(&e, c).start, bar(5), "back to the original samples at 120 BPM");
}

#[test]
fn scaling_the_tempo_keeps_midi_clips_on_their_bars() {
    let (mut e, _, c) = midi_at_bar_5();
    run(&mut e, "event.tempo_scale", json!({"factor": 1.5}));
    let k = clip(&e, c);
    on_bar(e.session(), k.start, 5);
    on_bar(e.session(), k.end(), 7);
    assert_ne!(k.start, bar(5));
}

#[test]
fn a_constant_tempo_range_keeps_midi_clips_on_their_bars() {
    let (mut e, t, c) = midi_at_bar_5();
    let late = midi_clip(&mut e, t, bar(12), BAR);
    run(&mut e, "event.tempo_constant", json!({"bpm": 100, "start": bar(2), "end": bar(8)}));
    let s = e.session();
    on_bar(s, clip(&e, c).start, 5);
    on_bar(s, clip(&e, c).end(), 7);
    on_bar(s, clip(&e, late).start, 12);
    assert_ne!(clip(&e, late).start, bar(12), "material after the range moves too");
}

#[test]
fn tempo_curves_and_stretch_keep_midi_clips_on_their_bars() {
    for (id, p) in [
        ("event.tempo_linear", json!({"start": bar(2), "end": bar(4), "end_bpm": 80})),
        ("event.tempo_parabolic", json!({"start": bar(2), "end": bar(4), "end_bpm": 80})),
        ("event.tempo_s_curve", json!({"start": bar(2), "end": bar(4), "end_bpm": 80})),
        ("event.tempo_stretch", json!({"start": bar(2), "end": bar(4), "factor": 1.5})),
        ("event.identify_beat", json!({"start": bar(2), "end": bar(2) + 100_000, "bars": 1})),
    ] {
        let (mut e, _, c) = midi_at_bar_5();
        run(&mut e, id, p);
        assert_ne!(e.session().tempo.tempos().len(), 0);
        let k = clip(&e, c);
        assert_ne!(k.start, bar(5), "{id} must change the tempo before bar 5");
        on_bar(e.session(), k.start, 5);
        on_bar(e.session(), k.end(), 7);
    }
}

#[test]
fn a_mid_song_tempo_change_moves_only_later_material() {
    let (mut e, t, early) = midi_at_bar_5();
    let late = midi_clip(&mut e, t, bar(12), BAR);
    let (_, ticks_audio) = audio_track(&mut e, true, &[(bar(3), 10_000), (bar(11), 10_000)]);
    run(&mut e, "event.tempo", json!({"bpm": 60, "at": bar(9)}));
    let s = e.session();
    assert_eq!(clip(&e, early).start, bar(5));
    assert_eq!(clip(&e, early).length, 2 * BAR);
    assert_eq!(clip(&e, ticks_audio[0]).start, bar(3));
    on_bar(s, clip(&e, late).start, 12);
    on_bar(s, clip(&e, ticks_audio[1]).start, 11);
    assert_eq!(clip(&e, late).start, bar(9) + 3 * 2 * BAR);
}

#[test]
fn automation_on_a_midi_track_follows_and_on_a_samples_track_stays() {
    let (mut e, t, _) = midi_at_bar_5();
    let a = new_track(&mut e, "audio");
    for tr in [t, a] {
        run(&mut e, "automation.set_point", json!({"track": tr.0, "param": "volume", "at": bar(5), "value": -6.0}));
        run(&mut e, "automation.set_point", json!({"track": tr.0, "param": "volume", "at": bar(9), "value": -12.0}));
    }
    run(&mut e, "event.tempo", json!({"bpm": 75, "at": 0}));
    let s = e.session();
    let pts = |tr: TrackId| s.track(tr).unwrap().automation[0].points.iter().map(|p| p.at).collect::<Vec<_>>();
    let m = pts(t);
    on_bar(s, m[0], 5);
    on_bar(s, m[1], 9);
    assert_eq!(pts(a), vec![bar(5), bar(9)]);
}

#[test]
fn markers_key_changes_and_the_loop_range_stay_on_their_bars() {
    let (mut e, _, _) = midi_at_bar_5();
    run(&mut e, "markers.add", json!({"name": "Chorus", "at": bar(9)}));
    run(&mut e, "event.add_key_change", json!({"key": "A minor", "at": bar(9)}));
    e.session_mut().edit.timeline_selection = soundcraft_time::Range::new(bar(3), bar(7));
    e.session_mut().edit.selection = soundcraft_time::Range::new(bar(3), bar(7));
    run(&mut e, "event.tempo", json!({"bpm": 140, "at": 0}));
    let s = e.session();
    on_bar(s, s.markers[0].start, 9);
    on_bar(s, s.markers[0].end, 9);
    on_bar(s, s.key_signatures[0].0, 9);
    for r in [s.edit.timeline_selection, s.edit.selection] {
        on_bar(s, r.start, 3);
        on_bar(s, r.end, 7);
    }
}

#[test]
fn a_meter_change_moves_nothing() {
    let (mut e, _, c) = midi_at_bar_5();
    run(&mut e, "event.meter", json!({"numerator": 3, "denominator": 4, "at": bar(2)}));
    assert_eq!(clip(&e, c).start, bar(5));
}

/// Every position the retime touches, for exact comparisons.
fn positions(s: &Session) -> (Vec<Vec<Clip>>, Vec<Vec<Samples>>, Vec<(Samples, Samples)>, Vec<Samples>) {
    (
        s.tracks.iter().map(|t| t.clips().to_vec()).collect(),
        s.tracks.iter().map(|t| t.automation.iter().flat_map(|l| l.points.iter().map(|p| p.at)).collect()).collect(),
        s.markers.iter().map(|m| (m.start, m.end)).collect(),
        s.key_signatures.iter().map(|k| k.0).collect(),
    )
}

/// Material deliberately off the tick grid, so rounding has something to bite.
fn off_grid() -> Engine {
    let mut e = Engine::default();
    let t = new_track(&mut e, "instrument");
    midi_clip(&mut e, t, bar(5) + 13, BAR + 7);
    midi_clip(&mut e, t, bar(9) + 1, 2 * BAR - 3);
    run(&mut e, "automation.set_point", json!({"track": t.0, "param": "volume", "at": bar(6) + 11, "value": -3.0}));
    run(&mut e, "markers.add", json!({"name": "M", "at": bar(7) + 5}));
    audio_track(&mut e, true, &[(bar(4) + 17, 20_000)]);
    audio_track(&mut e, false, &[(bar(4) + 17, 20_000)]);
    e
}

#[test]
fn undo_restores_every_sample_position_exactly() {
    let mut e = off_grid();
    let before = positions(e.session());
    let tempo = e.session().tempo.clone();
    for _ in 0..100 {
        run(&mut e, "event.tempo", json!({"bpm": 150, "at": 0}));
        run(&mut e, "event.tempo", json!({"bpm": 120, "at": 0}));
    }
    for _ in 0..200 {
        run(&mut e, "edit.undo", json!({}));
    }
    assert_eq!(e.session().tempo, tempo);
    assert_eq!(positions(e.session()), before);
}

#[test]
fn alternating_tempo_edits_do_not_drift() {
    let mut e = off_grid();
    let ticks = |e: &Engine| {
        let s = e.session();
        let (clips, autos, marks, _) = positions(s);
        let mut v: Vec<Samples> = clips.iter().flatten().flat_map(|c| [c.start, c.end()]).collect();
        v.extend(autos.iter().flatten().copied());
        v.extend(marks.iter().map(|m| m.0));
        v.iter().map(|&x| s.tempo.samples_to_ticks_f(x, s.sample_rate)).collect::<Vec<_>>()
    };
    let start = ticks(&e);
    for i in 0..100 {
        let bpm = [150.0, 97.3, 61.0, 133.7][i % 4];
        run(&mut e, "event.tempo", json!({"bpm": bpm, "at": 0}));
        run(&mut e, "event.tempo", json!({"bpm": 120, "at": 0}));
    }
    // Back at 120 BPM: within one tick of where everything started. Samples-timebase audio is
    // checked exactly by the samples test; its tick position is unchanged here as well.
    for (a, b) in ticks(&e).iter().zip(&start) {
        assert!((a - b).abs() <= 1.0, "drifted from tick {b} to {a}");
    }
}

#[test]
fn the_journal_replays_to_the_same_session() {
    let script = |e: &mut Engine| {
        let t = new_track(e, "midi");
        midi_clip(e, t, bar(5) + 13, BAR);
        midi_clip(e, t, bar(9), BAR);
        run(e, "automation.set_point", json!({"track": t.0, "param": "volume", "at": bar(6), "value": -3.0}));
        run(e, "markers.add", json!({"at": bar(7)}));
        run(e, "event.tempo", json!({"bpm": 87.5, "at": bar(3)}));
        run(e, "event.tempo_scale", json!({"factor": 1.25}));
        run(e, "event.tempo_constant", json!({"bpm": 100, "start": bar(2), "end": bar(6)}));
        run(e, "event.tempo_linear", json!({"start": bar(6), "end": bar(8), "end_bpm": 70}));
        let at = e.session().tempo.tick_to_samples(2 * 4 * 960, e.session().sample_rate);
        run(e, "event.tempo_remove", json!({"at": at}));
    };
    let mut direct = Engine::default();
    script(&mut direct);
    let mut replayed = Engine::default();
    for (id, p) in direct.journal.clone() {
        replayed.execute(&id, &p).unwrap();
    }
    assert_eq!(positions(replayed.session()), positions(direct.session()));
    assert_eq!(replayed.session().tempo, direct.session().tempo);
}

#[test]
fn a_tempo_change_is_one_undo_step() {
    let mut e = off_grid();
    let before = positions(e.session());
    run(&mut e, "event.tempo", json!({"bpm": 60, "at": 0}));
    assert_ne!(positions(e.session()), before);
    run(&mut e, "edit.undo", json!({}));
    assert_eq!(positions(e.session()), before);
}

#[test]
fn importing_a_midi_tempo_map_keeps_existing_midi_clips_on_their_bars() {
    let (mut e, _, c) = midi_at_bar_5();
    let mut seq = soundcraft_midi::Sequence::default();
    seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 960 });
    let smf = soundcraft_midi::Smf {
        format: 1,
        tracks: vec![soundcraft_midi::SmfTrack { name: "Lead".into(), sequence: seq }],
        tempos: vec![(0, 72.0)],
        ..Default::default()
    };
    let bytes = soundcraft_midi::write_smf(&smf).unwrap();
    soundcraft_engine::io::import_midi_bytes(&mut e, &bytes, 0, true).unwrap();
    assert!((e.session().tempo.tempo_at_tick(0) - 72.0).abs() < 0.01);
    let k = clip(&e, c);
    on_bar(e.session(), k.start, 5);
    on_bar(e.session(), k.end(), 7);
}

#[test]
fn every_playlist_on_a_ticks_track_follows_and_midi_fades_keep_their_length_in_beats() {
    let (mut e, t, c) = midi_at_bar_5();
    {
        let s = e.session_mut();
        let k = s.find_clip_mut(c).unwrap();
        k.fade_in.len = BAR / 4;
        k.fade_out.len = BAR / 2;
        // An alternate playlist holding a copy one bar later.
        let mut alt = s.track(t).unwrap().playlists[0].clone();
        alt.name = "alt".into();
        alt.clips[0].start = bar(6);
        let tr = s.track_mut(t).unwrap();
        tr.playlists.push(alt);
    }
    run(&mut e, "event.tempo", json!({"bpm": 60, "at": 0}));
    let s = e.session();
    let tr = s.track(t).unwrap();
    on_bar(s, tr.playlists[1].clips[0].start, 6);
    let k = clip(&e, c);
    assert_eq!((k.fade_in.len, k.fade_out.len), (BAR / 2, BAR), "a beat and two beats at 60 BPM");
}
