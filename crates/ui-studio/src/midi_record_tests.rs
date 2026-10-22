//! MIDI recording through the host: a finished capture pass becomes clips on the armed tracks.
//! The audio thread's capture is exercised in `soundcraft-playback`; here a synthetic pass stands
//! in for it (120 bpm, 4/4, 48 kHz: a beat is 24,000 samples and 960 ticks).

use crate::{Services, StudioApp};
use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_midi::Sequence;
use soundcraft_mix::{Message, NoteId};
use soundcraft_model::{ClipContent, TrackId};
use soundcraft_playback::MidiTake;
use soundcraft_playback::midi_capture::CapturedEvent;
use soundcraft_playback::record::start::Point;
use soundcraft_playback::timeline_map::Position;

const BEAT: i64 = 24_000;
const TICKS: i64 = soundcraft_time::TICKS_PER_QUARTER;
/// The device frame the synthetic latch sits at.
const LATCH: u64 = 10_000;

fn app() -> (StudioApp, TrackId) {
    let mut app = StudioApp::new(Engine::default(), None, Services::default());
    let t = TrackId(app.run("track.new", json!({"kind": "midi"})).unwrap()["tracks"][0].as_u64().unwrap());
    app.run("mix.record_arm", json!({"tracks": [t.0], "value": true})).unwrap();
    (app, t)
}

fn on(frame: u64, track: TrackId, key: u8) -> CapturedEvent {
    CapturedEvent { frame, track, channel: 0, message: Message::NoteOn { key, velocity: 100, id: NoteId::NONE }, approximate: false }
}

fn off(frame: u64, track: TrackId, key: u8) -> CapturedEvent {
    CapturedEvent { message: Message::NoteOff { key, velocity: 0, id: NoteId::NONE }, ..on(frame, track, key) }
}

/// A finished pass whose latch sits at session sample `sample` (device frame [`LATCH`]).
fn pass(track: TrackId, sample: Option<i64>, events: Vec<CapturedEvent>) -> MidiTake {
    let start = sample.map(|sample| Point { host_ns: 0, output_frame: LATCH, position: Position { sample, fraction: 0, denominator: 1 } });
    MidiTake { tracks: vec![track], start, events, dropped: 0, device_rate: 48_000 }
}

/// Record from `start` to `stop` (session samples) with `take` as what the audio thread captured,
/// then Stop through the host.
fn record(app: &mut StudioApp, start: i64, pass_len: Option<i64>, stop: i64, take: MidiTake) {
    app.record_start = start;
    app.take_plan = Some(soundcraft_engine::io::Recording {
        start,
        pass: pass_len,
        punch: None,
        rate: 48_000,
        takes: Vec::new(),
        midi_journal: None,
        midi_placed: false,
    });
    app.engine.transport.recording = true;
    app.start_play(stop, None, None);
    app.finished_test_midi = Some(take);
    app.stop_play();
}

fn clip_notes(app: &StudioApp, id: soundcraft_model::ClipId) -> Vec<(u8, i64, i64, u8)> {
    let s = app.engine.session_arc();
    let clip = s.tracks.iter().flat_map(|t| t.playlists.iter()).flat_map(|p| p.clips.iter()).find(|c| c.id == id).unwrap();
    let ClipContent::Midi { sequence: Sequence { notes, .. } } = &clip.content else { panic!("not MIDI") };
    notes.iter().map(|n| (n.pitch, n.start, n.length, n.velocity)).collect()
}

#[test]
fn two_played_notes_become_one_clip_at_the_latch_with_their_ticks_and_one_undo_step() {
    let (mut app, t) = app();
    let before = app.engine.undo_history().len();
    // Played a beat and a half and three beats after the latch; held a beat and half a beat.
    let frames = |beats: f64| LATCH + (beats * BEAT as f64) as u64;
    let events = vec![on(frames(1.5), t, 60), off(frames(2.5), t, 60), on(frames(3.0), t, 64), off(frames(3.5), t, 64)];
    record(&mut app, 2 * BEAT, None, 2 * BEAT + 4 * BEAT, pass(t, Some(2 * BEAT), events));
    let clips = app.engine.session().track(t).unwrap().clips().to_vec();
    assert_eq!(clips.len(), 1);
    assert_eq!((clips[0].start, clips[0].length), (2 * BEAT, 4 * BEAT));
    assert_eq!(clip_notes(&app, clips[0].id), vec![(60, TICKS * 3 / 2, TICKS, 100), (64, TICKS * 3, TICKS / 2, 100)]);
    assert_eq!(app.engine.undo_history().len(), before + 1);
    assert_eq!(app.engine.undo_history().last().map(String::as_str), Some("Record"));
    assert!(app.status.starts_with("Recorded 1 MIDI clip (2 notes)"), "{}", app.status);
    assert!(app.take_plan.is_none() && !app.recording_active());
    // The new clip is selected, ready to edit.
    assert_eq!(app.engine.session().edit.selected_clips, vec![clips[0].id]);
    app.run("edit.undo", json!({})).unwrap();
    assert!(app.engine.session().track(t).unwrap().clips().is_empty(), "undo removes the clip");
}

#[test]
fn events_before_the_latch_and_on_other_tracks_are_not_recorded_and_the_first_note_is_relative_to_the_latch() {
    let (mut app, t) = app();
    let other = TrackId(t.0 + 100);
    // A key pressed during the count-in (frame 500, before the latch), one on an unarmed track, then
    // the first real note a quarter second after the latch, which a count-in placed at the cursor.
    let events = vec![
        on(500, t, 48),
        off(900, t, 48),
        on(LATCH + 12_000, other, 70),
        off(LATCH + 13_000, other, 70),
        on(LATCH + 12_000, t, 60),
        off(LATCH + 24_000, t, 60),
    ];
    record(&mut app, 4 * BEAT, None, 8 * BEAT, pass(t, Some(4 * BEAT), events));
    let clips = app.engine.session().track(t).unwrap().clips().to_vec();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].start, 4 * BEAT, "the clip starts at the latch, not at the first note");
    assert_eq!(clip_notes(&app, clips[0].id), vec![(60, TICKS / 2, TICKS / 2, 100)]);
}

#[test]
fn stop_during_the_count_in_or_with_nothing_played_records_nothing() {
    let (mut app, t) = app();
    let before = app.engine.undo_history().len();
    // The latch never started: Stop cancelled the count-in.
    record(&mut app, 0, None, 0, pass(t, None, vec![on(LATCH, t, 60), off(LATCH + 100, t, 60)]));
    assert!(app.engine.session().track(t).unwrap().clips().is_empty());
    // Started, but only a pedal: no notes, no clip.
    let pedal = CapturedEvent { message: Message::Control { cc: 64, value: 127 }, ..on(LATCH, t, 0) };
    record(&mut app, 0, None, 4 * BEAT, pass(t, Some(0), vec![pedal]));
    assert!(app.engine.session().track(t).unwrap().clips().is_empty());
    assert_eq!(app.engine.undo_history().len(), before, "no undo step either");
}

#[test]
fn a_note_still_held_at_stop_is_closed_at_the_stop_sample() {
    let (mut app, t) = app();
    record(&mut app, 0, None, 3 * BEAT, pass(t, Some(0), vec![on(LATCH + BEAT as u64, t, 60)]));
    let id = app.engine.session().track(t).unwrap().clips()[0].id;
    assert_eq!(clip_notes(&app, id), vec![(60, TICKS, 2 * TICKS, 100)]);
}

#[test]
fn recording_over_a_clip_overdubs_into_it_and_undoes_alone() {
    let (mut app, t) = app();
    record(&mut app, 0, None, 4 * BEAT, pass(t, Some(0), vec![on(LATCH, t, 60), off(LATCH + BEAT as u64, t, 60)]));
    let first = app.engine.session().track(t).unwrap().clips()[0].id;
    let undo = app.engine.undo_history().len();
    record(&mut app, BEAT * 2, None, 4 * BEAT, pass(t, Some(2 * BEAT), vec![on(LATCH, t, 64), off(LATCH + BEAT as u64, t, 64)]));
    assert_eq!(app.engine.session().track(t).unwrap().clips().len(), 1);
    assert_eq!(clip_notes(&app, first), vec![(60, 0, TICKS, 100), (64, 2 * TICKS, TICKS, 100)]);
    assert_eq!(app.engine.undo_history().len(), undo + 1);
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(clip_notes(&app, first).len(), 1);
}

#[test]
fn loop_recording_two_passes_makes_a_take_per_pass() {
    let (mut app, t) = app();
    let pass_frames = 2 * BEAT as u64;
    let events = vec![
        on(LATCH, t, 60),
        off(LATCH + 1_000, t, 60),
        on(LATCH + pass_frames + BEAT as u64, t, 62),
        off(LATCH + pass_frames + BEAT as u64 + 1_000, t, 62),
    ];
    record(&mut app, 4 * BEAT, Some(2 * BEAT), 4 * BEAT, pass(t, Some(4 * BEAT), events));
    let tr = app.engine.session().track(t).unwrap().clone();
    assert_eq!(tr.playlists.len(), 2, "each pass is a take on its own playlist, as audio takes are");
    for p in &tr.playlists {
        assert_eq!(p.clips.len(), 1);
        assert_eq!((p.clips[0].start, p.clips[0].length), (4 * BEAT, 2 * BEAT));
    }
    assert_eq!(clip_notes(&app, tr.playlists[0].clips[0].id)[0].1, 0);
    assert_eq!(clip_notes(&app, tr.playlists[1].clips[0].id)[0].1, TICKS);
}

#[test]
fn lost_events_are_reported() {
    let (mut app, t) = app();
    let mut take = pass(t, Some(0), vec![on(LATCH, t, 60), off(LATCH + 100, t, 60)]);
    take.dropped = 3;
    record(&mut app, 0, None, 2 * BEAT, take);
    assert!(app.status.contains("3 events lost"), "{}", app.status);
}

#[test]
fn a_midi_only_pass_needs_an_open_audio_device() {
    let (mut app, _) = app();
    app.start_recording();
    assert!(app.status.contains("no audio device is open to record MIDI"), "{}", app.status);
    assert!(app.take_plan.is_none());
}

#[test]
fn a_midi_only_pass_runs_the_real_player_plumbing_and_clears_it_at_stop() {
    let engine = Engine::default();
    let player = soundcraft_playback::Player::prepare_device_free(engine.session_arc()).unwrap();
    let mut app = StudioApp::new(engine, Some(player), Services::default());
    let t = TrackId(app.run("track.new", json!({"kind": "midi"})).unwrap()["tracks"][0].as_u64().unwrap());
    app.run("mix.record_arm", json!({"tracks": [t.0], "value": true})).unwrap();
    app.start_recording();
    let player = app.player.as_ref().unwrap();
    assert!(player.midi_recording_prepared(), "{}", app.status);
    assert!(app.take_plan.as_ref().is_some_and(|plan| plan.takes.is_empty()));
    player.inject_midi_capture(on(LATCH, t, 60));
    assert!(player.midi_recording_progress(48_000).is_empty(), "nothing counts before the latch is applied");
    app.stop_play();
    assert!(!app.player.as_ref().unwrap().midi_recording_prepared(), "Stop ends the pass");
    assert!(app.take_plan.is_none());
    assert!(app.engine.session().track(t).unwrap().clips().is_empty(), "the latch never started, so nothing is placed");
}

#[test]
fn the_lane_shows_a_growing_recording_clip_with_the_notes_played_so_far_until_stop_replaces_it() {
    let (mut app, t) = app();
    let ctx = egui::Context::default();
    let mut time = 0.0;
    let mut frames = |app: &mut StudioApp, n: usize| {
        for _ in 0..n {
            time += 1.0 / 60.0;
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 800.0))),
                time: Some(time),
                ..Default::default()
            };
            app.raw_input_hook(&mut raw);
            let mut out = ctx.run_ui(raw, |ui| {
                app.logic(ui.ctx());
                app.ui(ui);
            });
            out.textures_delta.clear();
        }
    };
    frames(&mut app, 3);
    assert!(app.arrange.layout.recording.is_empty());
    let events = vec![on(LATCH, t, 60), off(LATCH + BEAT as u64, t, 60), on(LATCH + 2 * BEAT as u64, t, 64)];
    app.record_start = 0;
    app.take_plan = Some(soundcraft_engine::io::Recording {
        start: 0,
        pass: None,
        punch: None,
        rate: 48_000,
        takes: Vec::new(),
        midi_journal: None,
        midi_placed: false,
    });
    app.engine.transport.recording = true;
    app.start_play(4 * BEAT, None, None);
    app.finished_test_midi = Some(pass(t, Some(0), events));
    frames(&mut app, 3);
    let lane = &app.arrange.layout.recording;
    assert_eq!(lane.len(), 1, "one recording clip on the armed lane");
    assert_eq!(lane[0].track, t.0);
    assert_eq!(lane[0].notes.len(), 2, "two provisional notes, the held one closed at the playhead");
    assert!(lane[0].rect[2] > lane[0].rect[0]);
    assert!(app.engine.session().track(t).unwrap().clips().is_empty(), "nothing is placed until Stop");
    app.stop_play();
    frames(&mut app, 3);
    assert!(app.arrange.layout.recording.is_empty());
    assert_eq!(app.engine.session().track(t).unwrap().clips().len(), 1, "the real clip replaces it");
}

// K11: the MIDI take journal.

use soundcraft_engine::midi_journal;

fn folder(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("transit-midijournal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A pass of two notes under way with its journal armed and drained once (as a frame does).
fn journaled_pass(app: &mut StudioApp, t: TrackId, journal: &std::path::Path) {
    let frames = |beats: f64| LATCH + (beats * BEAT as f64) as u64;
    let events = vec![on(frames(1.5), t, 60), off(frames(2.5), t, 60), on(frames(3.0), t, 64), off(frames(3.5), t, 64)];
    app.midi_journal = Some(midi_journal::Writer::new(journal.to_path_buf()));
    app.record_start = 0;
    app.take_plan = Some(soundcraft_engine::io::Recording {
        start: 0,
        pass: None,
        punch: None,
        rate: 48_000,
        takes: Vec::new(),
        midi_journal: Some(journal.to_path_buf()),
        midi_placed: false,
    });
    app.engine.transport.recording = true;
    app.start_play(4 * BEAT, None, None);
    app.finished_test_midi = Some(pass(t, Some(0), events));
    app.update_midi_live();
    app.midi_journal.as_mut().unwrap().flush().unwrap();
}

#[test]
fn a_pass_is_journaled_each_frame_with_its_latch_and_the_clean_stop_deletes_the_journal() {
    let (mut app, t) = app();
    let dir = folder("clean");
    let path = midi_journal::journal_path(&dir, "Song");
    journaled_pass(&mut app, t, &path);
    let j = midi_journal::read(&path).unwrap();
    assert_eq!((j.header.output_frame, j.header.position, j.header.device_rate, j.header.session_rate), (LATCH, 0, 48_000, 48_000));
    assert_eq!((j.header.plan_start, j.header.tracks.clone()), (0, vec![t]));
    assert_eq!((j.records.len(), j.truncated), (4, false));
    // A second frame with nothing new appends nothing.
    app.update_midi_live();
    app.midi_journal.as_mut().unwrap().flush().unwrap();
    assert_eq!(midi_journal::read(&path).unwrap().records.len(), 4);
    app.stop_play();
    assert!(!path.exists(), "a clean Stop deletes the journal");
    assert_eq!(app.engine.session().track(t).unwrap().clips().len(), 1);
    assert!(app.midi_journal.is_none());
}

/// A pass whose app dies after its autosave is written: the journal and the plan stay on disk.
fn crash(root: &std::path::Path) -> (std::path::PathBuf, TrackId) {
    let (mut app, t) = app();
    app.autosave_dir = Some(root.join("Autosave"));
    let path = midi_journal::journal_path(&takes_of(&app, root), "Song");
    journaled_pass(&mut app, t, &path);
    assert!(app.write_autosave());
    drop(app);
    assert!(path.is_file());
    (path, t)
}

/// The folder an unsaved session records its takes (and journals) into.
fn takes_of(app: &StudioApp, root: &std::path::Path) -> std::path::PathBuf {
    root.join("Autosave").join(soundcraft_engine::io::sanitize_name(&app.engine.session().name)).join("Audio Files")
}

/// A pass that has been autosaved once, as the plan alone, and then stopped: the clip is placed
/// in memory and its journal is held.
fn stopped(root: &std::path::Path) -> (StudioApp, TrackId, std::path::PathBuf) {
    let (mut app, t) = app();
    app.autosave_dir = Some(root.join("Autosave"));
    let path = midi_journal::journal_path(&takes_of(&app, root), "Song");
    journaled_pass(&mut app, t, &path);
    assert!(app.write_autosave());
    app.stop_play();
    assert_eq!(app.engine.session().track(t).unwrap().clips().len(), 1);
    (app, t, path)
}

fn flush(app: &StudioApp) {
    app.autosaver.as_ref().unwrap().flush().unwrap();
}

fn recovered(root: &std::path::Path) -> (StudioApp, soundcraft_engine::autosave::Leftover) {
    let left = soundcraft_engine::autosave::leftovers(&root.join("Autosave"));
    assert_eq!(left.len(), 1);
    let mut next = StudioApp::new(Engine::default(), None, Services::default());
    next.autosave_dir = Some(root.join("Autosave"));
    next.recover(&left[0]);
    (next, left.into_iter().next().unwrap())
}

#[test]
fn a_crash_after_the_clip_is_placed_but_before_the_autosave_rebuilds_it_once() {
    let root = folder("placed-unsaved");
    let (app, t, path) = stopped(&root);
    assert!(path.is_file(), "the journal outlives Stop until the autosave has the clip");
    drop(app);
    let left = soundcraft_engine::autosave::leftovers(&root.join("Autosave"));
    assert!(left[0].recording && !left[0].midi_placed, "the leftover autosave still shows a pass under way");
    let (next, left) = recovered(&root);
    let clips = next.engine.session().track(t).unwrap().clips().to_vec();
    assert_eq!(clips.len(), 1, "rebuilt once");
    assert_eq!(clip_notes(&next, clips[0].id), vec![(60, TICKS * 3 / 2, TICKS, 100), (64, TICKS * 3, TICKS / 2, 100)]);
    assert!(!path.exists() && !left.dir.exists());
}

#[test]
fn a_crash_after_the_autosave_landed_discards_the_journal_without_a_duplicate_clip() {
    let root = folder("placed-saved");
    let (mut app, t, path) = stopped(&root);
    app.poll_journal();
    flush(&app);
    assert!(path.is_file(), "the plan still names it until a write without the recording lands");
    drop(app);
    let left = soundcraft_engine::autosave::leftovers(&root.join("Autosave"));
    assert!(left[0].midi_placed && left[0].midi_journal.as_deref() == Some(path.as_path()));
    let (next, left) = recovered(&root);
    assert_eq!(next.engine.session().track(t).unwrap().clips().len(), 1, "the clip from the autosave, not a second one");
    assert!(!next.status.contains("recovered MIDI clip"), "{}", next.status);
    assert!(!path.exists() && !left.dir.exists());
}

#[test]
fn a_clean_stop_deletes_the_journal_once_the_autosave_without_the_recording_landed() {
    let root = folder("placed-clean");
    let (mut app, _, path) = stopped(&root);
    app.poll_journal();
    flush(&app);
    app.poll_journal();
    flush(&app);
    assert!(path.is_file(), "not yet: the second write has only just landed");
    app.poll_journal();
    assert!(!path.exists() && app.placed_journal.is_none());
    app.close_autosave();
    let left = soundcraft_engine::autosave::leftovers(&root.join("Autosave"));
    assert_eq!(left.len(), 1);
    assert!(!left[0].recording && left[0].midi_journal.is_none());
}

#[test]
fn saving_the_session_releases_a_held_journal() {
    let root = folder("placed-saved-file");
    let (mut app, _, path) = stopped(&root);
    let file = root.join("Song.scraft").to_string_lossy().into_owned();
    app.run("session.save", json!({"path": file})).unwrap();
    app.poll_journal();
    assert!(!path.exists() && app.placed_journal.is_none());
}

#[test]
fn after_a_crash_the_next_launch_offers_recovery_and_rebuilds_the_clip_on_the_same_track() {
    let root = folder("crash");
    let (path, t) = crash(&root);
    let left = soundcraft_engine::autosave::leftovers(&root.join("Autosave"));
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].midi_journal.as_deref(), Some(path.as_path()));
    let mut next = StudioApp::new(Engine::default(), None, Services::default());
    next.autosave_dir = Some(root.join("Autosave"));
    next.recover(&left[0]);
    let track = next.engine.session().track(t).unwrap();
    let clips = track.clips().to_vec();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].name, format!("{}_recovered", soundcraft_engine::io::sanitize_name(&track.name)));
    assert_eq!(clip_notes(&next, clips[0].id), vec![(60, TICKS * 3 / 2, TICKS, 100), (64, TICKS * 3, TICKS / 2, 100)]);
    assert!(next.status.contains("recovered MIDI clip"), "{}", next.status);
    assert!(!path.exists() && !left[0].dir.exists(), "recovered: the journal and the leftover are gone");
}

#[test]
fn declining_recovery_deletes_the_journal() {
    let root = folder("decline");
    let (path, _) = crash(&root);
    let left = soundcraft_engine::autosave::leftovers(&root.join("Autosave"));
    soundcraft_engine::autosave::discard(&left[0]);
    assert!(!path.exists() && !left[0].dir.exists());
    assert!(soundcraft_engine::autosave::leftovers(&root.join("Autosave")).is_empty());
}
