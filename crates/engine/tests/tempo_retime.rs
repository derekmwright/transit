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

/// The playlist rule on every track: sorted, every clip at least a sample long, no overlaps
/// (these fixtures have no crossfades).
fn assert_playlists_valid(s: &Session) {
    for t in &s.tracks {
        for pl in &t.playlists {
            for c in &pl.clips {
                assert!(c.length >= 1, "{} is empty", c.name);
            }
            for w in pl.clips.windows(2) {
                assert!(w[0].start <= w[1].start, "unsorted: {:?} {:?}", w[0].range(), w[1].range());
                assert!(w[0].end() <= w[1].start, "{:?} overlaps {:?}", w[0].range(), w[1].range());
            }
        }
    }
}

fn drag(e: &mut Engine, bpms: impl IntoIterator<Item = f64>) {
    for bpm in bpms {
        e.execute_merged("event.tempo", &json!({"bpm": bpm, "at": 0}), "tempo_drag").unwrap();
    }
    e.end_merge();
}

/// Audio A spanning bars 2 and 3, B right after it at bar 4, on a ticks-timebase track.
fn adjacent_audio() -> (Engine, [ClipId; 2]) {
    let mut e = Engine::default();
    let (_, ids) = audio_track(&mut e, true, &[(bar(2), 2 * BAR), (bar(4), BAR)]);
    (e, [ids[0], ids[1]])
}

#[test]
fn a_dragged_tempo_change_that_returns_leaves_audio_exactly_as_it_was() {
    let (mut e, ids) = adjacent_audio();
    let before = positions(e.session());
    drag(&mut e, [240.0, 120.0]);
    assert_eq!(clip(&e, ids[0]).length, 2 * BAR, "the drag must not trim A for good");
    assert_eq!(positions(e.session()), before);
}

#[test]
fn four_hundred_drag_steps_leave_material_exact_in_one_undo_step_and_one_journal_line() {
    let (mut e, _) = adjacent_audio();
    let t = new_track(&mut e, "midi");
    let c = midi_clip(&mut e, t, bar(5) + 13, BAR + 7);
    run(&mut e, "automation.set_point", json!({"track": t.0, "param": "volume", "at": bar(6) + 11, "value": -3.0}));
    let start = e.session().clone();
    let before = positions(&start);
    let journal = e.journal.len();
    let steps: Vec<f64> = (0..400).map(|i| 120.0 + 180.0 * (std::f64::consts::PI * i as f64 / 399.0).sin() + (i % 7) as f64 * 0.37).collect();
    drag(&mut e, steps.iter().copied().chain([120.0]));
    assert_eq!(positions(e.session()), before);
    assert_playlists_valid(e.session());
    run(&mut e, "edit.undo", json!({}));
    assert_eq!(positions(e.session()), before);
    run(&mut e, "edit.redo", json!({}));

    // A second gesture ends somewhere else; the material follows the bars of its last step.
    drag(&mut e, steps.iter().copied().chain([97.0]));
    let s = e.session();
    assert!((bb(s, clip(&e, c).start).bar, bb(s, clip(&e, c).start).beat) == (5, 1));
    assert_playlists_valid(s);
    run(&mut e, "edit.undo", json!({}));
    run(&mut e, "edit.undo", json!({}));
    assert_eq!(positions(e.session()), before, "each gesture is one undo step");
    run(&mut e, "edit.redo", json!({}));
    run(&mut e, "edit.redo", json!({}));

    // The journal keeps one line per gesture, and replaying it gives the same document.
    let tempo_lines = e.journal[journal..].iter().filter(|j| j.0 == "event.tempo").count();
    assert_eq!(tempo_lines, 2, "{:?}", &e.journal[journal..]);
    let mut replayed = Engine::new(start);
    for (id, p) in &e.journal[journal..] {
        replayed.execute(id, p).unwrap();
    }
    assert_eq!(positions(replayed.session()), positions(e.session()));
    assert_eq!(replayed.session().tempo, e.session().tempo);
}

#[test]
fn a_tempo_that_would_squeeze_a_clip_to_nothing_is_refused() {
    let mut e = Engine::default();
    run(&mut e, "event.tempo", json!({"bpm": 20, "at": 0}));
    let t = new_track(&mut e, "audio");
    run(&mut e, "track.timebase", json!({"tracks": [t.0], "ticks": true}));
    let s = e.session_mut();
    s.sources.push(source(7));
    // The later clip has the lower id, so a (start, id) sort would put it first on a tie.
    let (late, early) = (s.new_clip_id(), s.new_clip_id());
    s.track_mut(t).and_then(|t| t.playlist_mut()).unwrap().clips =
        vec![Clip::audio(early, "early", SourceId(7), 0, 100_000, 10), Clip::audio(late, "late", SourceId(7), 0, 100_010, 50_000)];
    let before = positions(e.session());
    let err = e.execute("event.tempo", &json!({"bpm": 999, "at": 0})).unwrap_err().to_string();
    assert!(err.contains("early"), "{err}");
    assert_eq!(positions(e.session()), before, "a refused change leaves everything in place");
    assert_eq!(e.session().tempo.tempo_at_tick(0), 20.0);
    assert_playlists_valid(e.session());
}

#[test]
fn the_playlist_rule_holds_at_20_and_999_bpm() {
    let mut e = Engine::default();
    // Clips of many sizes, back to back and with gaps, on ticks audio and MIDI tracks.
    let mut spans = Vec::new();
    let mut at = 0;
    for i in 0..40 {
        let len = BAR / 2 + (i % 5) * 9_973;
        spans.push((at, len));
        at += len + if i % 3 == 0 { 0 } else { 517 * (i % 4) };
    }
    audio_track(&mut e, true, &spans);
    let m = new_track(&mut e, "midi");
    for (start, len) in &spans {
        midi_clip(&mut e, m, *start, *len);
    }
    assert_playlists_valid(e.session());
    for bpm in [20.0, 999.0, 61.0, 999.0, 20.0, 120.0] {
        run(&mut e, "event.tempo", json!({"bpm": bpm, "at": 0}));
        assert_playlists_valid(e.session());
    }
}

#[test]
fn a_time_locked_clip_keeps_its_samples_and_neighbors_trim_against_it() {
    let mut e = Engine::default();
    // Moving clip M at bar 3 runs onto locked L when the tempo slows.
    let (_, ids) = audio_track(&mut e, true, &[(bar(3), 150_000), (bar(5), BAR)]);
    e.session_mut().find_clip_mut(ids[1]).unwrap().time_locked = true;
    run(&mut e, "event.tempo", json!({"bpm": 80, "at": 0}));
    let (m, l) = (clip(&e, ids[0]), clip(&e, ids[1]));
    on_bar(e.session(), m.start, 3);
    assert_eq!((l.start, l.length), (bar(5), BAR), "the locked clip stays at its samples");
    assert_eq!(m.end(), l.start, "M's tail is trimmed at the locked clip");
    assert_playlists_valid(e.session());

    // A clip that lands on a locked clip's tail loses its head instead.
    let mut e = Engine::default();
    let (_, ids) = audio_track(&mut e, true, &[(0, 180_000), (bar(3), 50_000)]);
    e.session_mut().find_clip_mut(ids[0]).unwrap().time_locked = true;
    run(&mut e, "event.tempo", json!({"bpm": 160, "at": 0}));
    let (l, m) = (clip(&e, ids[0]), clip(&e, ids[1]));
    assert_eq!((l.start, l.length), (0, 180_000));
    assert_eq!((m.start, m.end()), (180_000, 144_000 + 50_000));
    assert_eq!(m.source_offset(), 1_000 + 36_000, "the head is trimmed; the audio stays where it plays");
    assert_playlists_valid(e.session());
}

#[test]
fn beat_mapping_moves_nothing() {
    let (mut e, _, c) = midi_at_bar_5();
    run(&mut e, "markers.add", json!({"at": 480_000}));
    run(&mut e, "event.identify_beat", json!({"start": 0, "end": 480_000, "bars": 4}));
    assert!((e.session().tempo.tempo_at_tick(0) - 96.0).abs() < 1e-9);
    assert_eq!(e.session().markers[0].start, 480_000);
    assert_eq!(clip(&e, c).start, bar(5));
}

#[test]
fn track_lane_markers_follow_their_own_tracks_timebase() {
    let (mut e, midi, _) = midi_at_bar_5();
    let audio = new_track(&mut e, "audio");
    for t in [midi, audio] {
        run(&mut e, "markers.add", json!({"at": bar(9)}));
        e.session_mut().markers.last_mut().unwrap().track = Some(t);
    }
    run(&mut e, "event.tempo", json!({"bpm": 60, "at": 0}));
    let s = e.session();
    on_bar(s, s.markers[0].start, 9);
    assert_eq!(s.markers[1].start, bar(9), "a marker on a samples-timebase track stays");
}

#[test]
fn moving_the_song_start_moves_all_musical_material() {
    let (mut e, t, c) = midi_at_bar_5();
    {
        let s = e.session_mut();
        let mut alt = s.track(t).unwrap().playlists[0].clone();
        alt.name = "alt".into();
        s.track_mut(t).unwrap().playlists.push(alt);
    }
    run(&mut e, "automation.set_point", json!({"track": t.0, "param": "volume", "at": bar(6), "value": -3.0}));
    run(&mut e, "markers.add", json!({"at": bar(7)}));
    let a = new_track(&mut e, "audio");
    run(&mut e, "automation.set_point", json!({"track": a.0, "param": "volume", "at": bar(6), "value": -3.0}));
    run(&mut e, "event.move_song_start", json!({"to": BAR}));
    let s = e.session();
    let tr = s.track(t).unwrap();
    assert_eq!(clip(&e, c).start, bar(6));
    assert_eq!(tr.playlists[1].clips[0].start, bar(6), "the alternate playlist moves too");
    assert_eq!(tr.automation[0].points[0].at, bar(7));
    assert_eq!(s.markers[0].start, bar(8));
    assert_eq!(s.track(a).unwrap().automation[0].points[0].at, bar(6), "samples-timebase automation stays");
}

/// 10,000 clips and 100,000 automation points under 2,001 tempo events: one retime must fit in a
/// drag step. Run with `cargo test --release -p soundcraft-engine --test tempo_retime -- --ignored`.
#[test]
#[ignore]
fn retiming_a_large_session_is_fast() {
    let mut s = Session::default();
    for i in 0..2_000i64 {
        s.tempo.set_tempo(i * 960, 100.0 + (i % 40) as f64).unwrap();
    }
    let t = s.add_track(soundcraft_model::TrackKind::Midi, soundcraft_model::ChannelFormat::Mono, Some("big"));
    let end = s.tempo.tick_to_samples(2_000 * 960, s.sample_rate);
    let step = end / 10_000;
    let clips: Vec<Clip> =
        (0..10_000).map(|i| Clip::midi(ClipId(1_000_000 + i as u64), "c", i * step, step - 10, soundcraft_midi::Sequence::default())).collect();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips = clips;
    let lane = tr.lane_mut(&soundcraft_model::AutoParam::Volume);
    lane.points = (0..100_000).map(|i| soundcraft_model::AutomationPoint { at: i * (end / 100_000), value: 0.0 }).collect();
    let old = s.tempo.clone();
    s.tempo.scale_tempos(1.1).unwrap();
    let t0 = std::time::Instant::now();
    soundcraft_engine::retime::retime_to_tempo(&mut s, &old).unwrap();
    let took = t0.elapsed();
    eprintln!("retime of 10k clips and 100k points under 2,001 tempo events: {took:?}");
    assert_playlists_valid(&s);
    assert!(took.as_millis() < 20, "took {took:?}");
}

/// Locked audio at bars 3 and 4 with a short clip at bar 5 on a second ticks track: at 200 BPM
/// bar 5 falls inside the locked clip, so that step is refused.
fn refusing_fixture() -> (Engine, ClipId) {
    let mut e = Engine::default();
    let (_, d) = audio_track(&mut e, true, &[(0, 2 * BAR), (bar(3), BAR)]);
    let (_, l) = audio_track(&mut e, true, &[(bar(3), BAR), (bar(5), 24_000)]);
    e.session_mut().find_clip_mut(l[0]).unwrap().time_locked = true;
    (e, d[0])
}

#[test]
fn a_refused_step_mid_drag_is_invisible_to_the_gesture() {
    let (mut e, d) = refusing_fixture();
    let before = positions(e.session());
    let (undo, journal) = (e.undo_history().len(), e.journal.len());
    e.execute_merged("event.tempo", &json!({"bpm": 130, "at": 0}), "tempo_drag").unwrap();
    assert!(e.execute_merged("event.tempo", &json!({"bpm": 200, "at": 0}), "tempo_drag").is_err());
    e.execute_merged("event.tempo", &json!({"bpm": 130, "at": 0}), "tempo_drag").unwrap();
    e.execute_merged("event.tempo", &json!({"bpm": 120, "at": 0}), "tempo_drag").unwrap();
    e.end_merge();
    assert_eq!(clip(&e, d).range(), soundcraft_time::Range::new(0, 2 * BAR), "no truncation from the refused step");
    assert_eq!(positions(e.session()), before);
    assert_eq!(e.undo_history().len(), undo + 1, "one undo step");
    assert_eq!(e.journal.len(), journal + 1, "one journal line: {:?}", &e.journal[journal..]);
}

#[test]
fn a_tempo_drag_during_an_automation_pass_still_converts_from_the_gestures_start() {
    let (mut e, ids) = adjacent_audio();
    let samples_track = new_track(&mut e, "audio");
    let before = positions(e.session());
    e.hold_merge(Some("pass"));
    e.execute_merged("event.tempo", &json!({"bpm": 240, "at": 0}), "tempo_drag").unwrap();
    e.execute_merged("automation.set_point", &json!({"track": samples_track.0, "param": "volume", "at": 1_000, "value": -3.0}), "write").unwrap();
    e.execute_merged("event.tempo", &json!({"bpm": 120, "at": 0}), "tempo_drag").unwrap();
    e.hold_merge(None);
    e.end_merge();
    assert_eq!(clip(&e, ids[0]).length, 2 * BAR, "A keeps its length");
    let (clips, autos, marks, keys) = positions(e.session());
    assert_eq!((clips, marks, keys), (before.0, before.2, before.3));
    assert_eq!(autos.iter().flatten().copied().collect::<Vec<_>>(), vec![1_000]);
}
