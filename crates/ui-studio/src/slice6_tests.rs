//! Slice 6 tests (finishing a track): rename, takes, the clip-end handle, bounce, undo history,
//! count-in. Driven like a user where there is a gesture: synthetic clicks, drags and keys through
//! the control channel's input queue, one frame at a time. No player; take files and bounces go to
//! a per-test temp folder that is removed again.

use crate::finish::RenameTarget;
use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::{ClipId, TrackId};

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

/// A temp folder for one test, removed on drop.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("transit-slice6-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A 16-bit mono WAV of `frames` frames at 48 kHz (a ramp, so it is not silence).
fn write_wav(path: &std::path::Path, frames: u32) {
    let data = frames * 2;
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + data).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(48_000u32.to_le_bytes());
    b.extend(96_000u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data.to_le_bytes());
    for i in 0..frames {
        b.extend((((i % 200) as i16 - 100) * 100).to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// (channels, bits, frames) of a PCM or float WAV file.
fn wav_frames(path: &std::path::Path) -> (u16, u16, u64) {
    let b = std::fs::read(path).unwrap();
    let (mut i, mut ch, mut bits) = (12usize, 0u16, 0u16);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        if id == b"fmt " {
            ch = u16::from_le_bytes([b[i + 10], b[i + 11]]);
            bits = u16::from_le_bytes([b[i + 22], b[i + 23]]);
        }
        if id == b"data" {
            return (ch, bits, (len / (usize::from(ch) * usize::from(bits / 8))) as u64);
        }
        i += 8 + len + (len & 1);
    }
    panic!("no data chunk in {}", path.display());
}

impl T {
    fn new() -> T {
        let app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let mut t = T { app, ctx: egui::Context::default(), time: 0.0 };
        t.frames(3);
        t
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.time += 1.0 / 60.0;
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 800.0))),
                time: Some(self.time),
                ..Default::default()
            };
            self.app.raw_input_hook(&mut raw);
            let app = &mut self.app;
            let mut out = self.ctx.run_ui(raw, |ui| {
                app.logic(ui.ctx());
                app.ui(ui);
            });
            out.textures_delta.clear();
        }
    }

    fn input(&mut self, method: &str, p: Value) {
        let (req, _rx) = crate::ControlRequest::new(method, p);
        let ctx = egui::Context::default();
        let _ = crate::control::handle_for_test(&mut self.app, &ctx, &req);
        self.frames(30);
    }

    fn click(&mut self, at: (f32, f32)) {
        self.input("ui.click", json!({"x": at.0, "y": at.1}));
    }

    fn double(&mut self, at: (f32, f32)) {
        self.input("ui.click", json!({"x": at.0, "y": at.1, "count": 2}));
    }

    fn key(&mut self, key: &str, mods: Value) {
        let mut p = json!({"key": key});
        if let (Some(o), Some(m)) = (p.as_object_mut(), mods.as_object()) {
            o.extend(m.clone());
        }
        self.input("ui.key", p);
    }

    fn typed(&mut self, text: &str) {
        self.input("ui.text", json!({"text": text}));
    }

    fn drag(&mut self, from: (f32, f32), to: (f32, f32)) {
        self.input("ui.drag", json!({"x": from.0, "y": from.1, "to_x": to.0, "to_y": to.1}));
    }

    fn menu(&mut self, label: &str) {
        let r =
            self.app.arrange.bar_menu.iter().find(|m| m.label == label).unwrap_or_else(|| panic!("no {label}: {:?}", self.app.arrange.bar_menu)).rect;
        self.click(mid(r));
    }

    fn undo_len(&self) -> usize {
        self.app.engine.undo_history().len()
    }

    fn track(&mut self, p: Value) -> TrackId {
        let t = TrackId(self.app.run("track.new", p).unwrap()["tracks"][0].as_u64().unwrap());
        self.frames(3);
        t
    }

    fn name(&self, t: TrackId) -> String {
        self.app.engine.session().track(t).unwrap().name.clone()
    }

    fn row(&self, t: TrackId) -> crate::arrange::TrackRow {
        self.app.arrange.layout.tracks.iter().find(|r| r.id == t.0).cloned().unwrap()
    }

    /// An instrument track with a one-bar MIDI clip at bar 1: (track, clip).
    fn midi_clip(&mut self) -> (TrackId, ClipId) {
        let t = self.track(json!({"kind": "instrument"}));
        let c = self.app.run("midi.new_clip", json!({"tracks": [t.0], "start": 0, "end": 0})).unwrap();
        self.frames(3);
        (t, ClipId(c["clip"].as_u64().unwrap()))
    }

    /// Place a recorded take of `secs` seconds on `t` at `start`, as Stop does.
    fn record(&mut self, dir: &Dir, t: TrackId, start: i64, secs: f64, n: usize) -> String {
        let path = dir.0.join(format!("take{n}.wav"));
        let frames = (secs * 48_000.0) as u32;
        write_wav(&path, frames);
        let plan = soundcraft_engine::io::Recording {
            start,
            pass: None,
            punch: None,
            rate: 48_000,
            takes: Vec::new(),
            midi_journal: None,
            midi_placed: false,
        };
        self.app.place_takes(&plan, vec![(t, path)], start + i64::from(frames), 1);
        self.frames(3);
        self.app.status.clone()
    }
}

// ---- Shortcuts ----

#[test]
fn ctrl_r_renames_now_and_r_and_f9_still_record() {
    use crate::shortcuts::{Action, label};
    assert_eq!(label(Action::Rename), "Ctrl+R");
    assert_eq!(label(Action::Record), "R");
    assert_eq!(label(Action::Bounce), "Ctrl+Shift+R");
    assert_eq!(label(Action::UndoHistory), "Alt+Z");
    assert!(!crate::shortcuts::rows(Action::Record).any(|s| s.mods.command), "Ctrl+R is no longer Record");
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Vox"}));
    t.app.focus_header(a, false);
    t.key("R", json!({"command": true}));
    assert!(t.app.finish.rename.is_some(), "Ctrl+R opens the name field");
    assert!(!t.app.engine.transport.recording && t.app.record_hint().is_none(), "Ctrl+R does not try to record");
    // Accident path: typing an R into the name records nothing and keeps the field.
    t.typed("R");
    assert!(t.app.finish.rename.is_some());
    assert!(t.app.record_hint().is_none(), "an R typed into a name is text, not Record");
    t.key("Escape", json!({}));
    assert!(t.app.finish.rename.is_none());
    assert_eq!(t.name(a), "Vox", "Escape keeps the old name");
    // Bare R and F9 still ask to record (no track armed: the hint says so).
    t.key("R", json!({}));
    assert!(t.app.record_hint().is_some(), "R is Record");
    t.key("F9", json!({}));
    assert!(t.app.record_hint().is_some(), "F9 is Record");
    assert!(t.app.finish.rename.is_none(), "R never renames");
}

#[test]
fn rename_keys_do_nothing_without_a_track_or_clip() {
    let mut t = T::new();
    t.key("F2", json!({}));
    assert!(t.app.finish.rename.is_none());
    assert!(t.app.status.contains("Select a track or a clip"), "{}", t.app.status);
    assert_eq!(t.undo_len(), 0);
}

// ---- Rename ----

#[test]
fn double_click_a_header_name_renames_in_place_enter_applies_escape_cancels() {
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Audio 1"}));
    let before = t.undo_len();
    t.double(mid(t.row(a).name_rect));
    assert!(matches!(t.app.finish.rename.as_ref().map(|r| r.target), Some(RenameTarget::Track(x)) if x == a), "the field opens");
    // The whole name is selected, so typing replaces it.
    t.typed("Bass DI");
    t.key("Enter", json!({}));
    assert!(t.app.finish.rename.is_none());
    assert_eq!(t.name(a), "Bass DI");
    assert_eq!(t.undo_len(), before + 1, "one undo step");
    // Escape cancels.
    t.double(mid(t.row(a).name_rect));
    t.typed("Nope");
    t.key("Escape", json!({}));
    assert_eq!(t.name(a), "Bass DI");
    assert_eq!(t.undo_len(), before + 1, "a cancelled rename adds no undo step");
    // Accident path: an empty name is refused and the old one stays.
    t.double(mid(t.row(a).name_rect));
    t.key("Backspace", json!({}));
    t.key("Enter", json!({}));
    assert_eq!(t.name(a), "Bass DI");
    assert!(t.app.status.contains("can't be empty"), "{}", t.app.status);
    assert_eq!(t.undo_len(), before + 1);
    t.key("Escape", json!({}));
    // Accident path: Delete while typing edits the text; the track stays.
    t.double(mid(t.row(a).name_rect));
    t.key("Delete", json!({}));
    t.key("Escape", json!({}));
    assert!(t.app.engine.session().track(a).is_some(), "Delete in the name field never deletes the track");
}

#[test]
fn f2_renames_the_focused_track_and_a_group_can_be_renamed() {
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Gtr"}));
    t.click(mid(t.row(a).name_rect));
    t.key("F2", json!({}));
    t.typed("Lead Gtr");
    t.key("Enter", json!({}));
    assert_eq!(t.name(a), "Lead Gtr");
    // A group header (slice 2c gap: no group rename).
    t.app.focus_header(a, false);
    t.app.group_selected().unwrap();
    t.frames(3);
    let g = t.app.engine.session().tracks.iter().find(|x| x.is_folder()).unwrap().id;
    t.double(mid(t.row(g).name_rect));
    t.typed("Guitars");
    t.key("Enter", json!({}));
    assert_eq!(t.name(g), "Guitars");
    // A duplicate name is refused by the engine; the status says why.
    t.double(mid(t.row(a).name_rect));
    t.typed("Guitars");
    t.key("Enter", json!({}));
    assert_eq!(t.name(a), "Lead Gtr");
    assert!(t.app.status.contains("already exists"), "{}", t.app.status);
}

#[test]
fn a_clip_renames_from_its_name_strip_or_f2_and_its_body_still_opens_the_piano_roll() {
    let mut t = T::new();
    let (_, c) = t.midi_clip();
    let r = t.app.arrange.layout.clips.iter().find(|x| x.id == c.0).unwrap().rect;
    t.double((r[0] + 12.0, r[1] + 4.0));
    assert!(matches!(t.app.finish.rename.as_ref().map(|r| r.target), Some(RenameTarget::Clip(x)) if x == c));
    assert!(!t.app.keys_shown(), "a double-click on the name strip renames, not edits");
    t.typed("Hook");
    t.key("Enter", json!({}));
    let name = |t: &T| t.app.engine.session().find_clip(c).unwrap().1.name.clone();
    assert_eq!(name(&t), "Hook");
    // F2 with the clip selected renames the clip.
    t.app.click_clip(c, false);
    t.key("F2", json!({}));
    assert!(matches!(t.app.finish.rename.as_ref().map(|r| r.target), Some(RenameTarget::Clip(x)) if x == c));
    t.typed("Verse");
    t.key("Enter", json!({}));
    assert_eq!(name(&t), "Verse");
    // The body still opens the piano roll.
    let r = t.app.arrange.layout.clips.iter().find(|x| x.id == c.0).unwrap().rect;
    t.double((r[0] + 12.0, (r[1] + r[3]) / 2.0 + 6.0));
    assert!(t.app.keys_shown(), "a double-click on the body opens the piano roll");
}

#[test]
fn a_mixer_strip_name_renames_on_double_click() {
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Snare"}));
    t.app.ui.show_mixer = true;
    t.frames(3);
    let strip = t.app.mixer.layout.strips.iter().find(|s| s.track == a.0).unwrap().name_rect;
    t.double(mid(strip));
    assert!(t.app.finish.rename.is_some(), "the strip's name opens the field");
    t.typed("Snare Top");
    t.key("Enter", json!({}));
    assert_eq!(t.name(a), "Snare Top");
}

#[test]
fn the_track_menu_offers_rename() {
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Pad"}));
    t.app.focus_header(a, false);
    t.menu("Track");
    t.menu("Rename");
    t.typed("Pads");
    t.key("Enter", json!({}));
    assert_eq!(t.name(a), "Pads");
}

// ---- Record and takes ----

#[test]
fn a_recording_is_selected_and_named_in_the_status_line() {
    let dir = Dir::new("rec");
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Audio 1"}));
    let status = t.record(&dir, a, 48_000, 2.3, 1);
    assert_eq!(status, "Recorded 1 clip (2.3 s) on Audio 1");
    let tr = t.app.engine.session().track(a).unwrap().clone();
    assert_eq!(tr.playlists.len(), 1, "nothing under it: no new take");
    assert_eq!(t.app.engine.session().edit.selected_clips, vec![tr.clips()[0].id], "the take's clip is selected");
    assert_eq!(t.app.focus_track(), Some(a));
}

#[test]
fn recording_over_a_clip_makes_a_take_and_the_old_clip_stays_as_take_1() {
    let dir = Dir::new("takes");
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Audio 1"}));
    t.record(&dir, a, 0, 2.0, 1);
    let first = t.app.engine.session().track(a).unwrap().clips()[0].id;
    let before = t.undo_len();
    let status = t.record(&dir, a, 24_000, 1.0, 2);
    assert!(status.ends_with("as take 2"), "{status}");
    assert_eq!((t.undo_len(), t.app.engine.undo_history().pop().as_deref()), (before + 1, Some("Record")), "a take is one undo step");
    let tr = t.app.engine.session().track(a).unwrap().clone();
    assert_eq!((tr.playlists.len(), tr.active_playlist), (2, 1));
    assert!(tr.playlists[0].clips.iter().any(|c| c.id == first && c.length == 96_000), "take 1 keeps the old clip whole");
    assert!(tr.clips().iter().any(|c| c.start == 24_000 && c.length == 48_000), "take 2 has the new clip");
    // A take recorded where nothing is adds no playlist.
    t.record(&dir, a, 480_000, 0.5, 3);
    assert_eq!(t.app.engine.session().track(a).unwrap().playlists.len(), 2);
}

/// An audio track with two takes over bars 1-2, take lanes not yet shown: (track).
fn two_takes(t: &mut T, dir: &Dir) -> TrackId {
    let a = t.track(json!({"kind": "audio", "name": "Audio 1"}));
    t.record(dir, a, 0, 4.0, 1);
    t.record(dir, a, 0, 4.0, 2);
    let _ = t.app.engine.execute("view.zoom_set", &json!({"samples_per_px": 500.0}));
    t.frames(3);
    a
}

#[test]
fn the_take_chip_shows_the_lanes_and_a_lane_or_label_click_switches_the_take() {
    let dir = Dir::new("chip");
    let mut t = T::new();
    let a = two_takes(&mut t, &dir);
    let chip = t.app.finish.layout.chips.first().cloned().expect("a takes chip on the clip");
    assert_eq!((chip.track, chip.takes, chip.open), (a.0, 2, false));
    assert!(t.app.finish.layout.takes.is_empty(), "collapsed by default");
    let rev = t.app.engine.revision;
    t.click(mid(chip.rect));
    assert_eq!(t.app.ui.takes_open, vec![a]);
    assert_eq!(t.app.engine.revision, rev, "showing takes is view state");
    let rows = t.app.finish.layout.takes.clone();
    assert_eq!(rows.len(), 2);
    assert!(rows[1].active && !rows[0].active);
    // The track row grew by the take lanes; the next row moved down.
    let h = t.row(a).header;
    assert!(h[3] - h[1] >= t.app.ui.track_height + 2.0 * crate::arrange::TAKE_H - 1.0, "{h:?}");
    // A lane click plays take 1; a label click plays take 2 again.
    t.click(mid(rows[0].lane));
    assert_eq!(t.app.engine.session().track(a).unwrap().active_playlist, 0);
    assert!(t.app.status.contains("Take 1 of Audio 1"), "{}", t.app.status);
    let rows = t.app.finish.layout.takes.clone();
    t.click(mid(rows[1].label));
    assert_eq!(t.app.engine.session().track(a).unwrap().active_playlist, 1);
    // The chip hides them again.
    let chip = t.app.finish.layout.chips.first().cloned().unwrap();
    t.click(mid(chip.rect));
    assert!(t.app.ui.takes_open.is_empty() && t.app.finish.layout.takes.is_empty());
}

#[test]
fn promote_keeps_one_take_in_one_undo_step() {
    let dir = Dir::new("promote");
    let mut t = T::new();
    let a = two_takes(&mut t, &dir);
    t.app.toggle_takes(a);
    t.frames(3);
    let before = t.undo_len();
    let take1 = t.app.engine.session().track(a).unwrap().playlists[0].clips.clone();
    let rows = t.app.finish.layout.takes.clone();
    t.click(mid(rows[0].promote));
    let tr = t.app.engine.session().track(a).unwrap().clone();
    assert_eq!(tr.playlists.len(), 1);
    assert_eq!(tr.clips(), take1.as_slice(), "take 1 is what is left");
    assert_eq!(t.undo_len(), before + 1, "one undo step");
    assert!(t.app.finish.layout.takes.is_empty() && t.app.finish.layout.chips.is_empty(), "no takes left to show");
    t.app.run("edit.undo", json!({})).unwrap();
    let tr = t.app.engine.session().track(a).unwrap().clone();
    assert_eq!((tr.playlists.len(), tr.active_playlist), (2, 1), "undo brings both takes back");
}

#[test]
fn delete_take_keeps_the_other_and_is_refused_with_three() {
    let dir = Dir::new("delete");
    let mut t = T::new();
    let a = two_takes(&mut t, &dir);
    t.app.toggle_takes(a);
    t.frames(3);
    let rows = t.app.finish.layout.takes.clone();
    assert!(rows.iter().all(|r| r.delete_enabled));
    let take2 = t.app.engine.session().track(a).unwrap().playlists[1].clips.clone();
    t.click(mid(rows[0].delete));
    t.key("Enter", json!({}));
    let tr = t.app.engine.session().track(a).unwrap().clone();
    assert_eq!(tr.playlists.len(), 1);
    assert_eq!(tr.clips(), take2.as_slice(), "take 2 stays");
    assert!(t.app.status.contains("Deleted take 1"), "{}", t.app.status);
    // Accident path: with three takes the delete button is disabled and a click changes nothing.
    t.record(&dir, a, 0, 4.0, 3);
    t.record(&dir, a, 0, 4.0, 4);
    t.app.ui.takes_open = vec![a];
    t.frames(3);
    let rows = t.app.finish.layout.takes.clone();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| !r.delete_enabled));
    let before = (t.undo_len(), t.app.engine.session().track(a).unwrap().playlists.len());
    t.click(mid(rows[0].delete));
    assert_eq!((t.undo_len(), t.app.engine.session().track(a).unwrap().playlists.len()), before);
}

#[test]
fn count_in_is_a_preference_from_the_transport_menu_and_measures_bars_from_the_tempo_map() {
    let mut t = T::new();
    t.menu("Transport");
    t.menu("Count-in");
    assert!(t.app.ui.count_in);
    assert!(!t.app.engine.is_dirty(), "a preference, not the document");
    // 120 BPM in 4/4: a beat is half a second, a bar two seconds.
    let one = t.app.count_in_for(10 * 48_000).unwrap();
    assert_eq!((one.frames, one.beat, one.per_bar), (96_000, 24_000, 4));
    t.app.ui.count_in_bars = 2;
    assert_eq!(t.app.count_in_for(0).unwrap().frames, 192_000, "bar 1 needs no room before it");
    assert_eq!(t.app.lead_in(10 * 48_000), 0, "the count-in is not a pre-roll");
    t.menu("Transport");
    t.menu("Count-in");
    assert!(!t.app.ui.count_in);
    assert!(t.app.count_in_for(0).is_none());
}

#[test]
fn record_with_a_count_in_at_bar_one_counts_in_then_plays_from_the_start() {
    let mut t = T::new();
    t.app.ui.count_in = true;
    t.app.ui.count_in_bars = 2;
    t.app.set_cursor(0, None);
    t.app.begin_pass(Vec::new());
    assert_eq!(t.app.status, "Count-in");
    let (left, total) = t.app.count_in_state().expect("counting in");
    assert_eq!((left, total), (192_000, 192_000));
    assert_eq!(t.app.count_in_label().as_deref(), Some("-2| 1"));
    assert!(!t.app.is_playing(), "the playhead waits at the start");
    assert_eq!(t.app.position(), 0);
    // One second in (two beats): still bar -2, beat 3.
    t.app.handle_transport(1.0);
    assert_eq!(t.app.count_in_label().as_deref(), Some("-2| 3"));
    assert!(t.app.beat_phase().is_some(), "the metronome button has a beat to flash");
    t.app.handle_transport(1.0);
    assert_eq!(t.app.count_in_label().as_deref(), Some("-1| 1"));
    // The rest of the two bars, and the transport plays from the cursor.
    t.app.handle_transport(2.1);
    assert!(t.app.count_in_state().is_none());
    assert!(t.app.is_playing());
    assert_eq!(t.app.record_start, 0);
    t.app.handle_transport(0.0);
    assert_ne!(t.app.status, "Count-in", "the status line moves on once the count-in ends");
    t.app.stop_play();
}

#[test]
fn stop_during_the_count_in_cancels_it_without_playing() {
    let mut t = T::new();
    t.app.ui.count_in = true;
    t.app.set_cursor(10 * 48_000, None);
    t.app.begin_pass(Vec::new());
    assert!(t.app.count_in_state().is_some());
    t.app.handle_transport(0.5);
    t.app.stop_play();
    assert!(t.app.count_in_state().is_none());
    t.app.handle_transport(5.0);
    assert!(!t.app.is_playing(), "a cancelled count-in never starts playing");
    assert_eq!(t.app.cursor(), 10 * 48_000);
}

fn request(t: &mut T, request: soundcraft_engine::TransportRequest) {
    t.app.engine.transport_requests.push(request);
    t.app.handle_transport(0.0);
}

#[test]
fn space_during_the_count_in_stops_it_without_a_take_and_space_again_plays_plainly() {
    use soundcraft_engine::TransportRequest::TogglePlay;
    let mut t = T::new();
    t.app.ui.count_in = true;
    t.app.ui.count_in_bars = 2;
    t.app.set_cursor(10 * 48_000, None);
    t.app.begin_pass(Vec::new());
    assert!(t.app.count_in_state().is_some() && t.app.take_plan.is_some());
    assert_eq!(t.app.status, "Count-in");
    t.app.handle_transport(0.5);
    request(&mut t, TogglePlay);
    assert!(t.app.count_in_state().is_none());
    assert!(!t.app.is_playing(), "Space during the count-in stops; it does not play unarmed");
    assert!(t.app.take_plan.is_none() && !t.app.recording_active(), "no take, recorder released");
    assert!(t.app.recorder.as_ref().is_none_or(|r| !r.has_pending_take()));
    assert_ne!(t.app.status, "Recording");
    assert_ne!(t.app.status, "Count-in");
    t.app.handle_transport(5.0);
    assert!(!t.app.is_playing() && t.app.take_plan.is_none());
    assert_eq!(t.app.cursor(), 10 * 48_000);
    // Space again is plain play.
    request(&mut t, TogglePlay);
    assert!(t.app.is_playing());
    assert!(t.app.take_plan.is_none() && !t.app.recording_active());
    t.app.stop_play();
}

#[test]
fn half_speed_and_play_selection_during_the_count_in_cancel_it_too() {
    use soundcraft_engine::TransportRequest::{HalfSpeed, PlaySelection};
    for which in [HalfSpeed, PlaySelection] {
        let mut t = T::new();
        t.app.ui.count_in = true;
        t.app.set_cursor(10 * 48_000, None);
        t.app.begin_pass(Vec::new());
        assert!(t.app.count_in_state().is_some());
        let name = format!("{which:?}");
        request(&mut t, which);
        assert!(t.app.count_in_state().is_none() && !t.app.is_playing(), "{name}");
        assert!(t.app.take_plan.is_none() && !t.app.recording_active(), "{name}");
    }
}

#[test]
fn the_first_stop_during_the_count_in_keeps_the_cursor_and_the_second_returns_to_start() {
    use soundcraft_engine::TransportRequest::Stop;
    let mut t = T::new();
    t.app.ui.count_in = true;
    t.app.set_cursor(10 * 48_000, None);
    t.app.begin_pass(Vec::new());
    assert!(t.app.count_in_state().is_some());
    request(&mut t, Stop);
    assert!(t.app.count_in_state().is_none() && t.app.take_plan.is_none());
    assert_eq!(t.app.cursor(), 10 * 48_000, "the first Stop only cancels the count-in");
    request(&mut t, Stop);
    assert_eq!(t.app.cursor(), 0, "the second Stop returns to the start");
}

#[test]
fn a_failed_start_after_the_count_in_releases_the_take_and_says_why() {
    let engine = soundcraft_engine::Engine::default();
    let player = soundcraft_playback::Player::prepare_device_free(engine.session_arc()).unwrap();
    let mut app = StudioApp::new(engine, Some(player), crate::Services::default());
    app.take_plan = Some(soundcraft_engine::io::Recording {
        start: 0,
        pass: None,
        punch: None,
        rate: 48_000,
        takes: Vec::new(),
        midi_journal: None,
        midi_placed: false,
    });
    app.status = "Recording".into();
    assert!(app.recording_active());
    app.player.as_ref().unwrap().report_start_failure(soundcraft_playback::StartFailure::Clock);
    app.handle_transport(0.0);
    assert!(app.take_plan.is_none() && !app.recording_active());
    assert!(app.status.starts_with("Recording could not start: "), "{}", app.status);
    assert!(app.status.contains("clock"), "{}", app.status);
    // Reported once.
    app.status.clear();
    app.handle_transport(0.0);
    assert!(app.status.is_empty());
}

#[test]
fn the_metronome_button_toggles_the_click_and_its_popup_sets_preferences_without_undo() {
    let mut t = T::new();
    let undo = t.undo_len();
    let button = t.app.arrange.layout.transport["metronome"];
    t.click(mid(button));
    assert!(t.app.engine.session().edit.click, "one click turns the click on");
    t.click(mid(button));
    assert!(!t.app.engine.session().edit.click);
    assert_eq!(t.undo_len(), undo, "options.click is not an undo step");
    // Right-click: the popup.
    t.input("ui.click", json!({"x": mid(button).0, "y": mid(button).1, "button": "right"}));
    assert!(t.app.metro_popup);
    let at = |t: &T, name: &str| mid(t.app.arrange.layout.transport[name]);
    let bars = at(&t, "metro_bars_4");
    t.click(bars);
    assert!(t.app.ui.count_in);
    assert_eq!(t.app.ui.count_in_bars, 4);
    for _ in 0..3 {
        let up = at(&t, "metro_level_inc");
        t.click(up);
    }
    assert_eq!(t.app.ui.metronome_db, 3.0);
    let down = at(&t, "metro_level_dec");
    t.click(down);
    assert_eq!(t.app.ui.metronome_db, 2.0);
    let off = at(&t, "metro_bars_off");
    t.click(off);
    assert!(!t.app.ui.count_in);
    assert_eq!(t.undo_len(), undo, "the popup edits preferences, not the document");
    assert!(!t.app.engine.is_dirty());
    // Both persist with the other UI settings.
    let saved = serde_json::to_value(&t.app.ui).unwrap();
    assert_eq!((saved["count_in_bars"].as_u64(), saved["metronome_db"].as_f64()), (Some(4), Some(2.0)));
}

// ---- Piano roll clip-end handle ----

#[test]
fn the_clip_end_handle_extends_the_clip_so_a_note_past_the_old_end_sounds() {
    let mut t = T::new();
    let (_, c) = t.midi_clip();
    assert!(t.app.open_keys(c));
    t.frames(5);
    let k = t.app.keys.layout.clone();
    let bar = k.clip_ticks;
    let at = |k: &crate::keys::KeysLayout, tick: i64, pitch: u8| {
        (
            (k.grid[0] as f64 + (tick as f64 - k.scroll_ticks) * k.px_per_tick) as f32,
            k.grid[1] + (k.top_pitch - f32::from(pitch)) * k.key_h + k.key_h / 2.0,
        )
    };
    // Past the end: refused, and the hint names the handle.
    let pitch = (k.top_pitch - 4.0) as u8;
    t.double(at(&k, bar + bar / 4 + 10, pitch));
    assert_eq!(t.app.keys.layout.notes.len(), 0);
    assert_eq!(t.app.status, "Outside the clip: drag the clip end to extend");
    // Drag the handle one bar to the right.
    let before = t.undo_len();
    let h = k.end_handle;
    let px = (bar as f64 * k.px_per_tick) as f32;
    t.drag(mid(h), (mid(h).0 + px, mid(h).1));
    assert_eq!(t.app.keys.layout.clip_ticks, 2 * bar, "the clip is two bars long");
    assert_eq!(t.undo_len(), before + 1, "one undo step");
    let k = t.app.keys.layout.clone();
    t.double(at(&k, bar + bar / 4 + 10, pitch));
    let notes = t.app.keys.layout.notes.clone();
    assert_eq!(notes.len(), 1, "{}", t.app.status);
    assert!(!notes[0].outside, "the note is inside the clip and sounds");
    // Dragging it back left shortens it; the note is outside again (and kept).
    let h = t.app.keys.layout.end_handle;
    t.drag(mid(h), (mid(h).0 - px, mid(h).1));
    assert_eq!(t.app.keys.layout.clip_ticks, bar);
    assert!(t.app.keys.layout.notes.iter().all(|n| n.outside));
}

// ---- Bounce ----

#[test]
fn file_bounce_writes_the_selection_with_the_length_asked_for() {
    let dir = Dir::new("bounce");
    let mut t = T::new();
    t.midi_clip();
    t.app.run("edit.select", json!({"start": 0, "end": 48_000})).unwrap();
    t.menu("File");
    t.menu("Bounce…");
    assert!(t.app.finish.bounce_form.is_some(), "File > Bounce… opens the form");
    assert_eq!(t.inspect_dialog(), "bounce");
    let lay = t.app.finish.layout.bounce.clone().unwrap();
    let sel = lay.ranges.iter().find(|r| r.value == "selection").unwrap().rect;
    t.click(mid(sel));
    let d16 = lay.depths.iter().find(|r| r.value == "16").unwrap().rect;
    t.click(mid(d16));
    // Type the destination: select the field's text and replace it.
    let out = dir.0.join("mix.wav");
    t.click(mid(lay.path));
    t.key("A", json!({"command": true}));
    t.typed(&out.to_string_lossy());
    let ok = t.app.finish.layout.bounce.clone().unwrap().ok;
    t.click(mid(ok));
    for _ in 0..50 {
        if t.app.finish.job.is_none() {
            break;
        }
        t.frames(5);
    }
    assert!(t.app.finish.job.is_none(), "the bounce finished");
    assert!(t.app.status.starts_with("Bounced to"), "{}", t.app.status);
    assert!(t.app.status.contains("mix.wav"), "{}", t.app.status);
    let (ch, bits, frames) = wav_frames(&out);
    assert_eq!((ch, bits, frames), (2, 16, 48_000), "stereo, 16-bit, one second");
    assert!(t.app.dialog.is_none());
    assert!(t.app.finish.layout.last_bounce.is_some());
}

#[test]
fn bounce_cancels_from_the_form_and_from_progress_and_leaves_no_file() {
    let dir = Dir::new("cancel");
    let mut t = T::new();
    // Ten minutes of song, so the bounce is still running when Cancel is clicked.
    let tr = t.track(json!({"kind": "instrument"}));
    t.app.run("midi.new_clip", json!({"tracks": [tr.0], "start": 0, "end": 48_000 * 600})).unwrap();
    t.key("R", json!({"command": true, "shift": true}));
    assert!(t.app.finish.bounce_form.is_some(), "Ctrl+Shift+R opens the form, not Rename");
    assert!(t.app.finish.rename.is_none());
    // Space while the form is open does not play.
    t.key("Space", json!({}));
    assert!(!t.app.is_playing());
    t.key("Escape", json!({}));
    assert!(t.app.finish.bounce_form.is_none() && t.app.dialog.is_none());
    // Start a whole-song bounce, then cancel from the progress window.
    t.app.open_bounce();
    let out = dir.0.join("whole.wav");
    if let Some(f) = &mut t.app.finish.bounce_form {
        f.path = out.to_string_lossy().into_owned();
    }
    t.app.start_bounce().unwrap();
    // A session switch is refused while it runs.
    assert!(t.app.run("session.new", json!({})).is_err());
    assert!(t.app.status.contains("cancel the bounce"), "{}", t.app.status);
    t.frames(3);
    assert!(t.app.finish.job.is_some(), "still bouncing");
    assert!(dir.0.join("whole.wav.part").exists(), "it writes a part file");
    let cancel = t.app.finish.cancel;
    t.click(mid(cancel));
    assert!(t.app.finish.job.is_none());
    assert_eq!(t.app.status, "Bounce cancelled");
    assert!(!out.exists(), "a cancelled bounce leaves no file");
    assert!(!dir.0.join("whole.wav.part").exists(), "and no part file");
}

#[test]
fn bounce_in_place_is_one_undo_step() {
    let mut t = T::new();
    let (tr, _) = t.midi_clip();
    t.app.focus_header(tr, false);
    let before = (t.undo_len(), t.app.engine.session().tracks.len());
    t.menu("Track");
    t.menu("Bounce in Place");
    let s = t.app.engine.session();
    assert_eq!(s.tracks.len(), before.1 + 1, "{}", t.app.status);
    assert!(s.track(tr).unwrap().hidden && s.track(tr).unwrap().inactive);
    assert_eq!(t.undo_len(), before.0 + 1);
    assert!(t.app.status.starts_with("Bounced"), "{}", t.app.status);
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.app.engine.session().tracks.len(), before.1);
    assert!(!t.app.engine.session().track(tr).unwrap().hidden);
}

// ---- Undo history ----

#[test]
fn undo_history_lists_the_steps_and_a_click_undoes_back_to_there() {
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "A"}));
    t.app.run("track.rename", json!({"track": a.0, "name": "B"})).unwrap();
    t.app.run("track.rename", json!({"track": a.0, "name": "C"})).unwrap();
    t.menu("Edit");
    t.menu("Undo History");
    assert!(t.app.ui.show_history);
    let rows = t.app.finish.layout.history.clone();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels.len(), 4, "{labels:?}");
    assert_eq!(labels[0], "Opened");
    assert!(rows[3].current);
    // A click on the first rename keeps it and undoes the second.
    t.click(mid(rows[2].rect));
    assert_eq!(t.name(a), "B");
    assert_eq!(t.undo_len(), 2);
    assert!(t.app.status.contains("Undid 1 step"), "{}", t.app.status);
    // Clicking the current row changes nothing.
    let rows = t.app.finish.layout.history.clone();
    assert!(rows[2].current);
    t.click(mid(rows[2].rect));
    assert_eq!(t.undo_len(), 2);
    // "Opened" undoes everything; Alt+Z closes the window.
    t.click(mid(rows[0].rect));
    assert_eq!(t.undo_len(), 0);
    assert!(t.app.engine.session().track(a).is_none());
    t.key("Z", json!({"alt": true}));
    assert!(!t.app.ui.show_history);
}

// ---- Review follow-ups ----

#[test]
fn deleting_a_take_asks_first_enter_deletes_escape_keeps_both() {
    let dir = Dir::new("delask");
    let mut t = T::new();
    let a = two_takes(&mut t, &dir);
    t.app.toggle_takes(a);
    t.frames(3);
    let rows = t.app.finish.layout.takes.clone();
    let before = t.undo_len();
    t.click(mid(rows[0].delete));
    assert_eq!(t.inspect_dialog(), "delete_take", "the click only asks");
    assert_eq!(t.app.engine.session().track(a).unwrap().playlists.len(), 2);
    assert_eq!(t.undo_len(), before);
    t.key("Escape", json!({}));
    assert!(t.app.dialog.is_none());
    assert_eq!(t.app.engine.session().track(a).unwrap().playlists.len(), 2, "Escape keeps both takes");
    assert_eq!(t.undo_len(), before);
    t.click(mid(rows[0].delete));
    t.key("Enter", json!({}));
    assert!(t.app.dialog.is_none());
    assert_eq!(t.app.engine.session().track(a).unwrap().playlists.len(), 1, "Enter deletes");
    assert!(t.app.status.contains("Deleted take 1"), "{}", t.app.status);
}

#[test]
fn promote_is_one_undo_step_whose_label_is_documented_as_the_first_command() {
    let dir = Dir::new("promlabel");
    let mut t = T::new();
    let a = two_takes(&mut t, &dir);
    t.app.promote_take(a, 0).unwrap();
    // The engine has no relabel and orders select before delete-unused (L20): left as is.
    assert_eq!(t.app.engine.undo_label(), Some("Select Playlist"));
}

#[test]
fn fit_extension_replaces_only_known_audio_extensions() {
    use crate::finish::fit_extension;
    use std::path::Path;
    let f = |p: &str, fmt: &str| fit_extension(Path::new(p), fmt).to_string_lossy().into_owned();
    assert_eq!(f("mix.v2", "wav"), "mix.v2.wav");
    assert_eq!(f("mix", "wav"), "mix.wav");
    assert_eq!(f("mix.wav", "flac"), "mix.flac");
    assert_eq!(f("mix.WAV", "wav"), "mix.WAV");
    assert_eq!(f("mix.aiff", "aiff"), "mix.aiff");
    assert_eq!(f("mix.mp3", "aiff"), "mix.aif");
    assert_eq!(f("take.final", "flac"), "take.final.flac");
}

#[test]
fn bounce_to_an_existing_file_asks_and_choose_defaults_to_the_new_extension() {
    let dir = Dir::new("replace");
    let mut t = T::new();
    t.midi_clip();
    let out = dir.0.join("mix.wav");
    std::fs::write(&out, b"old").unwrap();
    t.app.open_bounce();
    t.frames(3);
    if let Some(f) = &mut t.app.finish.bounce_form {
        f.path = out.to_string_lossy().into_owned();
    }
    t.frames(2);
    let ok = t.app.finish.layout.bounce.clone().unwrap().ok;
    t.click(mid(ok));
    assert_eq!(t.app.finish.replace, crate::finish::Replace::Asking, "an existing destination asks");
    assert!(t.app.finish.job.is_none());
    assert_eq!(std::fs::read(&out).unwrap(), b"old", "nothing is overwritten yet");
    // Escape goes back to the form, which stays open.
    t.key("Escape", json!({}));
    assert_eq!(t.app.finish.replace, crate::finish::Replace::No);
    assert!(t.app.finish.bounce_form.is_some() && t.app.dialog.is_some(), "the form is still there");
    assert_eq!(std::fs::read(&out).unwrap(), b"old");
    // Ask again; Enter replaces.
    let ok = t.app.finish.layout.bounce.clone().unwrap().ok;
    t.click(mid(ok));
    assert_eq!(t.app.finish.replace, crate::finish::Replace::Asking);
    t.key("Enter", json!({}));
    for _ in 0..50 {
        if t.app.finish.job.is_none() {
            break;
        }
        t.frames(5);
    }
    assert!(t.app.status.starts_with("Bounced to"), "{}", t.app.status);
    assert_ne!(std::fs::read(&out).unwrap(), b"old", "Enter replaced the file");
    // Choose... starts from the new format's extension.
    t.app.open_bounce();
    t.frames(3);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let sink = seen.clone();
    t.app.services.pick_save = Some(Box::new(move |name| {
        *sink.borrow_mut() = name.to_string();
        None
    }));
    if let Some(f) = &mut t.app.finish.bounce_form {
        f.path = dir.0.join("mix.v2").to_string_lossy().into_owned();
    }
    t.frames(2);
    let lay = t.app.finish.layout.bounce.clone().unwrap();
    t.click(mid(lay.formats.iter().find(|r| r.value == "flac").unwrap().rect));
    let choose = t.app.finish.layout.bounce.clone().unwrap().choose;
    t.click(mid(choose));
    assert_eq!(*seen.borrow(), "mix.v2.flac", "Choose... starts from the new extension");
}

#[test]
fn the_take_glyph_keeps_the_lanes_reachable_at_a_narrow_zoom() {
    let dir = Dir::new("narrow");
    let mut t = T::new();
    let a = two_takes(&mut t, &dir);
    // 4 s is 192000 samples: at 4000 samples per pixel the clip is 48 px wide, too narrow for the chip.
    let _ = t.app.engine.execute("view.zoom_set", &json!({"samples_per_px": 4000.0}));
    t.frames(3);
    let chip = t.app.finish.layout.chips.first().cloned().expect("a glyph on the narrow clip");
    assert!(chip.rect[2] - chip.rect[0] <= 12.0, "{:?}", chip.rect);
    assert!(t.app.finish.layout.takes.is_empty());
    t.click(mid(chip.rect));
    assert_eq!(t.app.ui.takes_open, vec![a], "the glyph opens the lanes");
    assert_eq!(t.app.finish.layout.takes.len(), 2);
}

#[test]
fn a_refused_rename_keeps_the_field_and_the_typed_text() {
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Audio 1"}));
    t.track(json!({"kind": "audio", "name": "Drums"}));
    t.double(mid(t.row(a).name_rect));
    t.typed("Drums");
    t.key("Enter", json!({}));
    assert_eq!(t.name(a), "Audio 1");
    let r = t.app.finish.rename.as_ref().expect("the field stays open");
    assert_eq!(r.text, "Drums", "with what was typed");
    assert!(!t.app.status.is_empty());
    // The user corrects it and Enter applies.
    t.typed(" 2");
    t.key("Enter", json!({}));
    assert!(t.app.finish.rename.is_none(), "{}", t.app.status);
    assert_eq!(t.name(a), "Drums 2");
    // An empty name also keeps the field; Escape cancels.
    t.double(mid(t.row(a).name_rect));
    t.key("Backspace", json!({}));
    t.key("Enter", json!({}));
    assert!(t.app.finish.rename.is_some() && t.app.status.contains("can't be empty"));
    t.key("Escape", json!({}));
    assert!(t.app.finish.rename.is_none());
    assert_eq!(t.name(a), "Drums 2");
}

#[test]
fn only_a_double_click_on_the_name_renames() {
    let mut t = T::new();
    let a = t.track(json!({"kind": "audio", "name": "Audio 1"}));
    let row = t.row(a);
    // Below the name, still inside the header's name area.
    let (x, y) = (row.name_rect[0] + 6.0, row.name_rect[3] + 8.0);
    assert!(y < row.header[3], "{row:?}");
    t.double(mid(row.name_rect));
    assert!(t.app.finish.rename.is_some(), "on the name it does");
    t.key("Escape", json!({}));
    t.double((x, y));
    assert!(t.app.finish.rename.is_none(), "a double-click off the name does not rename");
    assert_eq!(t.app.focus_track(), Some(a), "it still selects the track");
}

impl T {
    fn inspect_dialog(&self) -> String {
        self.app.inspect(&self.ctx)["dialog"].as_str().unwrap_or("").to_string()
    }
}

// ---- Freeze ----

#[test]
fn the_snowflake_freezes_with_one_command_and_a_frozen_track_shows_cold_and_refuses_edits() {
    let mut t = T::new();
    let (tr, _) = t.midi_clip();
    t.app.run("mix.insert", json!({"track": tr.0, "slot": 0, "plugin": "gain"})).unwrap();
    t.app.focus(tr);
    t.frames(6);
    assert!(!t.row(tr).frozen && !t.app.mixer.devices.cards.is_empty());
    assert!(t.app.mixer.devices.cards.iter().all(|c| !c.frozen));
    let before = t.undo_len();
    t.click(mid(t.row(tr).freeze_button));
    assert!(t.app.engine.session().track(tr).unwrap().freeze.is_some(), "{}", t.app.status);
    assert_eq!(t.undo_len(), before + 1, "one command, one undo step");
    assert!(t.app.status.starts_with("Froze"), "{}", t.app.status);
    t.frames(6);
    assert!(t.row(tr).frozen, "the lane is tinted cold");
    assert!(t.app.mixer.devices.cards.iter().all(|c| c.frozen), "the device cards are greyed");

    // Editing what was rendered is refused, and the status line says why.
    assert!(t.app.run("mix.insert_bypass", json!({"track": tr.0, "slot": 0})).is_err());
    assert_eq!(t.app.status, "Track is frozen; unfreeze to edit");
    assert_eq!(t.undo_len(), before + 1);
    // The fader is not material.
    t.app.run("mix.volume", json!({"tracks": [tr.0], "db": -3.0})).unwrap();

    t.click(mid(t.row(tr).freeze_button));
    assert!(t.app.engine.session().track(tr).unwrap().freeze.is_none());
    t.frames(6);
    assert!(!t.row(tr).frozen && t.app.mixer.devices.cards.iter().all(|c| !c.frozen));
}

#[test]
fn flatten_turns_the_track_to_audio_in_one_undo_step() {
    let mut t = T::new();
    let (tr, _) = t.midi_clip();
    t.app.run("mix.insert", json!({"track": tr.0, "slot": 0, "plugin": "gain"})).unwrap();
    let before = t.undo_len();
    t.app.flatten_track(tr).unwrap();
    let s = t.app.engine.session();
    assert_eq!(s.track(tr).unwrap().kind, soundcraft_model::TrackKind::Audio);
    assert!(s.track(tr).unwrap().mixer.inserts.iter().all(Option::is_none));
    assert_eq!(t.undo_len(), before + 1);
    assert!(t.app.status.starts_with("Flattened"), "{}", t.app.status);
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.app.engine.session().track(tr).unwrap().kind, soundcraft_model::TrackKind::Instrument);
    assert!(t.app.engine.session().track(tr).unwrap().mixer.inserts[0].is_some());
}

#[test]
fn stems_by_output_path_start_one_job_with_the_checked_paths() {
    let dir = Dir::new("stems");
    let mut t = T::new();
    t.app.engine.set_io_map(soundcraft_model::IoMap::for_device("test", "test", 2, 6));
    t.midi_clip();
    let (cue, _) = t.midi_clip();
    t.app.run("track.output", json!({"tracks": [cue.0], "output": "Out 3-4"})).unwrap();
    t.app.open_bounce();
    t.frames(3);
    // Main and the routed path start checked; the unrouted one is listed, unchecked.
    let form = t.app.finish.bounce_form.clone().unwrap();
    let shown: Vec<_> = form.paths.iter().map(|c| (c.name.as_str(), c.routed, c.checked)).collect();
    assert_eq!(shown, [("Out 1-2", true, true), ("Out 3-4", true, true), ("Out 5-6", false, false)]);
    let lay = t.app.finish.layout.bounce.clone().unwrap();
    assert!(lay.paths.is_empty(), "the path list shows in the by-path mode only");
    t.click(mid(lay.modes.iter().find(|m| m.value == "paths").unwrap().rect));
    let lay = t.app.finish.layout.bounce.clone().unwrap();
    assert_eq!(lay.paths.len(), 3);
    // Uncheck Out 3-4, check the unrouted Out 5-6, normalize each file to its own peak.
    t.click(mid(lay.paths.iter().find(|m| m.value == "Out 3-4").unwrap().rect));
    t.click(mid(lay.paths.iter().find(|m| m.value == "Out 5-6").unwrap().rect));
    t.click(mid(t.app.finish.layout.bounce.clone().unwrap().normalize));
    let each = t.app.finish.layout.bounce.clone().unwrap().norm_modes.iter().find(|m| m.value == "each").unwrap().rect;
    t.click(mid(each));
    if let Some(f) = &mut t.app.finish.bounce_form {
        f.dir = dir.0.to_string_lossy().into_owned();
    }
    t.frames(2);
    let ok = t.app.finish.layout.bounce.clone().unwrap().ok;
    t.click(mid(ok));
    let request = t.app.finish.paths_request.clone().expect("the form started a job");
    assert_eq!(request["paths"], json!(["Out 1-2", "Out 5-6"]));
    assert_eq!(request["normalize"], json!("each"));
    assert_eq!(request["dir"], json!(dir.0.to_string_lossy()));
    for _ in 0..50 {
        if t.app.finish.path_job.is_none() {
            break;
        }
        t.frames(5);
    }
    assert!(t.app.finish.path_job.is_none(), "the stems finished");
    assert!(t.app.status.starts_with("Bounced 2 stems"), "{}", t.app.status);
    assert!(t.app.status.contains("Out 5-6"), "the silent path is reported: {}", t.app.status);
    let mut files: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(files, ["Untitled_Out 1-2.wav", "Untitled_Out 5-6.wav"]);
    assert!(t.app.dialog.is_none());
}
