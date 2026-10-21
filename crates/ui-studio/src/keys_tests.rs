//! Piano roll (slice 3) tests. Every gesture is driven the way a user drives it: synthetic
//! clicks, drags, wheel and keys through the control channel's input queue, one frame at a time.
//! No player, no plug-ins, no scan.

use crate::keys::Snap;
use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_midi::Note;
use soundcraft_model::{ClipContent, ClipId, TrackId};

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
    track: TrackId,
    clip: ClipId,
}

impl T {
    /// An instrument track with a one-bar MIDI clip at bar 1, the mixer pane shown.
    fn new() -> T {
        let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let r = app.run("track.new", json!({"kind": "instrument"})).unwrap();
        let track = TrackId(r["tracks"][0].as_u64().unwrap());
        let c = app.run("midi.new_clip", json!({"tracks": [track.0], "start": 0, "end": 0})).unwrap();
        let clip = ClipId(c["clip"].as_u64().unwrap());
        app.ui.show_mixer = true;
        app.ui.show_devices = false;
        let mut t = T { app, ctx: egui::Context::default(), time: 0.0, track, clip };
        t.frames(3);
        t
    }

    /// Opened by a double-click on the clip in the arrangement.
    fn open() -> T {
        let mut t = T::new();
        let r = t.app.arrange.layout.clips.iter().find(|c| c.id == t.clip.0).unwrap().rect;
        t.click_n(mid(r), 2, json!({}));
        assert!(t.app.keys.layout.visible, "double-click opened the piano roll");
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

    fn click_n(&mut self, at: (f32, f32), count: u64, mods: Value) {
        self.input("ui.click", merged(json!({"x": at.0, "y": at.1, "count": count}), mods));
    }

    fn click(&mut self, at: (f32, f32)) {
        self.click_n(at, 1, json!({}));
    }

    fn drag(&mut self, from: (f32, f32), to: (f32, f32), mods: Value) {
        self.input("ui.drag", merged(json!({"x": from.0, "y": from.1, "to_x": to.0, "to_y": to.1}), mods));
    }

    fn key(&mut self, key: &str, mods: Value) {
        self.input("ui.key", merged(json!({"key": key}), mods));
    }

    /// The point on the grid at `tick` (from the clip start) in the middle of `pitch`'s row.
    fn at(&self, tick: f64, pitch: u8) -> (f32, f32) {
        let l = &self.app.keys.layout;
        let x = l.grid[0] + ((tick - l.scroll_ticks) * l.px_per_tick) as f32;
        let y = l.grid[1] + (l.top_pitch - f32::from(pitch)) * l.key_h + l.key_h / 2.0;
        (x, y)
    }

    fn notes(&self) -> Vec<Note> {
        match &self.app.engine.session().find_clip(self.clip).unwrap().1.content {
            ClipContent::Midi { sequence } => sequence.notes.clone(),
            _ => Vec::new(),
        }
    }

    fn undo_len(&self) -> usize {
        self.app.engine.undo_history().len()
    }

    fn add(&mut self, pitch: u8, start: i64, length: i64) {
        self.app.run("midi.note_add", json!({"clip": self.clip.0, "pitch": pitch, "start_ticks": start, "length_ticks": length})).unwrap();
        self.frames(2);
    }

    fn note_rect(&self, pitch: u8, start: i64) -> [f32; 4] {
        self.app.keys.layout.notes.iter().find(|n| n.pitch == pitch && n.start == start).map(|n| n.rect).unwrap()
    }

    fn selected(&self) -> usize {
        self.app.keys.layout.selected
    }
}

fn merged(mut a: Value, b: Value) -> Value {
    if let (Some(o), Some(m)) = (a.as_object_mut(), b.as_object()) {
        o.extend(m.clone());
    }
    a
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

#[test]
fn a_double_click_on_a_midi_clip_opens_the_piano_roll_and_tabs_shortcut_and_escape_switch_back() {
    let mut t = T::new();
    assert!(t.app.mixer.layout.visible, "the mixer pane to start with");
    let undo = t.undo_len();
    let r = t.app.arrange.layout.clips.iter().find(|c| c.id == t.clip.0).unwrap().rect;
    t.click_n(mid(r), 2, json!({}));
    let l = t.app.keys.layout.clone();
    assert!(l.visible && l.clip == Some(t.clip.0) && l.track == Some(t.track.0), "{l:?}");
    assert!(!t.app.mixer.layout.visible, "the piano roll replaces the mixer pane");
    assert!(l.pane[1] > t.app.arrange.layout.timeline[1], "in the bottom pane");
    // Devices tab: the device chain comes back in its place.
    t.click(mid(l.tab_devices));
    assert!(!t.app.keys.layout.visible && t.app.ui.show_devices && !t.app.ui.show_mixer);
    // Ctrl+Alt+K brings the same clip back.
    t.key("K", json!({"cmd": true, "alt": true}));
    assert!(t.app.keys.layout.visible && t.app.keys.layout.clip == Some(t.clip.0));
    t.click(mid(t.app.keys.layout.tab_mixer));
    assert!(!t.app.keys.layout.visible && t.app.mixer.layout.visible, "Mixer tab");
    t.key("K", json!({"cmd": true, "alt": true}));
    assert!(t.app.keys.layout.visible);
    // Escape in the piano roll returns to the mixer that was shown before.
    t.key("Escape", json!({}));
    assert!(!t.app.keys.layout.visible && t.app.mixer.layout.visible, "Escape");
    assert_eq!(t.undo_len(), undo, "opening and closing is view state");
}

#[test]
fn a_double_click_on_the_grid_adds_a_snapped_note_and_a_double_click_on_it_deletes_it() {
    let mut t = T::open();
    let undo = t.undo_len();
    // Just after the first sixteenth: the note goes in that cell, a sixteenth long, velocity 100.
    t.click_n(t.at(300.0, 62), 2, json!({}));
    let notes = t.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!((notes[0].pitch, notes[0].start, notes[0].length, notes[0].velocity), (62, 240, 240, 100));
    assert_eq!(t.undo_len(), undo + 1, "one undo entry");
    assert_eq!(t.app.keys.layout.last_audition, Some((62, 100)), "placing auditions it");
    assert_eq!(t.selected(), 1, "the new note is selected");
    t.click_n(mid(t.note_rect(62, 240)), 2, json!({}));
    assert!(t.notes().is_empty(), "double-click on a note deletes it");
    assert_eq!(t.undo_len(), undo + 2);
}

#[test]
fn click_shift_click_rubber_band_ctrl_a_and_delete_select_and_remove() {
    let mut t = T::open();
    t.add(60, 0, 240);
    t.add(64, 480, 240);
    t.add(67, 960, 240);
    t.click(mid(t.note_rect(60, 0)));
    assert_eq!(t.selected(), 1);
    t.click_n(mid(t.note_rect(64, 480)), 1, json!({"shift": true}));
    assert_eq!(t.selected(), 2, "Shift-click adds");
    t.click(t.at(1800.0, 62));
    assert_eq!(t.selected(), 0, "a click on empty grid clears the selection");
    // Rubber band over the first two notes, from empty space.
    let from = t.at(1.0, 66);
    let to = t.at(700.0, 58);
    t.drag(from, to, json!({}));
    assert_eq!(t.selected(), 2, "rubber band");
    let undo = t.undo_len();
    t.key("Delete", json!({}));
    let left: Vec<u8> = t.notes().iter().map(|n| n.pitch).collect();
    assert_eq!(left, vec![67], "Delete removes the selection");
    assert_eq!(t.undo_len(), undo + 1);
    t.key("A", json!({"cmd": true}));
    assert_eq!(t.selected(), 1, "Ctrl+A");
    t.key("Backspace", json!({}));
    assert!(t.notes().is_empty());
    assert_eq!(t.app.arrange.layout.clips.len(), 1, "the arrangement's Delete did not run: the clip stays");
}

#[test]
fn dragging_moves_snapped_the_edge_resizes_and_ctrl_drag_copies_a_note_or_the_selection() {
    let mut t = T::open();
    t.add(60, 0, 480);
    let undo = t.undo_len();
    // Move: about two sixteenths and a bit later, two keys up.
    let grab = t.at(120.0, 60);
    let to = t.at(120.0 + 510.0, 62);
    t.drag(grab, to, json!({}));
    let n = t.notes();
    assert_eq!((n.len(), n[0].start, n[0].pitch, n[0].length), (1, 480, 62, 480), "{n:?}");
    assert_eq!(t.undo_len(), undo + 1, "a move is one undo entry");
    // Resize from the right edge by a sixteenth and a bit.
    let r = t.note_rect(62, 480);
    let px = 260.0 * t.app.keys.layout.px_per_tick as f32;
    t.drag((r[2] - 2.0, (r[1] + r[3]) / 2.0), (r[2] - 2.0 + px, (r[1] + r[3]) / 2.0), json!({}));
    let n = t.notes();
    assert_eq!((n[0].start, n[0].length), (480, 720), "{n:?}");
    assert_eq!(t.undo_len(), undo + 2);
    // Ctrl+drag copies one bar later; the original stays.
    let grab = t.at(600.0, 62);
    let to = t.at(600.0 + 3840.0, 62);
    t.drag(grab, to, json!({"cmd": true}));
    let n = t.notes();
    assert_eq!(n.iter().map(|n| (n.start, n.pitch)).collect::<Vec<_>>(), vec![(480, 62), (4320, 62)], "{n:?}");
    assert_eq!(t.undo_len(), undo + 3, "a copy is one undo entry");
    // Highlight both and Ctrl+drag the selection down an octave: two more notes in one step.
    let a = t.at(300.0, 64);
    let b = t.at(5400.0, 61);
    t.drag(a, b, json!({}));
    assert_eq!(t.selected(), 2);
    let grab = t.at(600.0, 62);
    let to = t.at(600.0, 50);
    t.drag(grab, to, json!({"cmd": true}));
    let mut pitches: Vec<u8> = t.notes().iter().map(|n| n.pitch).collect();
    pitches.sort();
    assert_eq!(pitches, vec![50, 50, 62, 62]);
    assert_eq!(t.undo_len(), undo + 4, "copying a selection is one undo entry");
    assert_eq!(t.selected(), 2, "the copies are selected");
    assert!(t.app.keys.layout.notes.iter().filter(|n| n.selected).all(|n| n.pitch == 50));
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.notes().len(), 2, "undo removes both copies");
}

#[test]
fn alt_drag_and_the_velocity_lane_set_velocity_in_one_step() {
    let mut t = T::open();
    t.add(60, 0, 480);
    let undo = t.undo_len();
    let grab = t.at(200.0, 60);
    t.drag(grab, (grab.0, grab.1 - 30.0), json!({"alt": true}));
    let n = t.notes();
    assert_eq!((n[0].velocity, n[0].start, n[0].pitch), (120, 0, 60), "Alt+drag up 30 points: +20");
    assert_eq!(t.undo_len(), undo + 1);
    assert_eq!(t.app.ui.keys.velocity, 100, "editing a note leaves the new-note velocity alone");
    // The velocity lane: drag the stem to about a quarter height.
    let l = t.app.keys.layout.clone();
    let stem = l.notes[0].vel_rect;
    let y = l.velocity[3] - (l.velocity[3] - l.velocity[1] - 6.0) * 32.0 / 127.0;
    t.drag((stem[0] + 1.0, stem[1] + 2.0), (stem[0] + 1.0, y), json!({}));
    let v = t.notes()[0].velocity;
    assert!((30..=34).contains(&v), "lane drag set {v}");
    assert_eq!(t.undo_len(), undo + 2);
    // A click in the lane sets it at that height.
    let y = l.velocity[3] - (l.velocity[3] - l.velocity[1] - 6.0) * 100.0 / 127.0;
    t.click((stem[0] + 1.0, y));
    let v = t.notes()[0].velocity;
    assert!((98..=102).contains(&v), "lane click set {v}");
    // A new note uses the independent toolbar preference.
    t.click_n(t.at(1000.0, 64), 2, json!({}));
    let new = t.notes().into_iter().find(|n| n.pitch == 64).unwrap();
    assert_eq!(new.velocity, 100);
}

#[test]
fn new_note_velocity_field_survives_note_edits_and_drives_drawing_and_audition() {
    let mut t = T::open();
    t.add(60, 0, 480);
    let before = serde_json::to_value(t.app.engine.session()).unwrap();
    let undo = t.undo_len();
    // Edit the actual toolbar field, rather than assigning the preference in the test.
    t.click(mid(t.app.keys.layout.new_note_velocity));
    t.key("A", json!({"cmd": true}));
    t.input("ui.text", json!({"text": "87"}));
    t.key("Enter", json!({}));
    assert_eq!(t.app.ui.keys.velocity, 87);
    assert_eq!(t.undo_len(), undo);
    assert_eq!(serde_json::to_value(t.app.engine.session()).unwrap(), before);

    let grab = t.at(200.0, 60);
    t.drag(grab, (grab.0, grab.1 + 148.5), json!({"alt": true}));
    assert_eq!(t.notes()[0].velocity, 1, "reproduce the near-silent edited note");
    assert_eq!(t.app.ui.keys.velocity, 87, "Alt+drag must not change the field");
    // Selecting the quiet note auditions at the field's velocity without adopting its velocity.
    t.click(mid(t.note_rect(60, 0)));
    assert_eq!(t.app.keys.last_audition, Some((60, 87)));
    assert_eq!(t.app.ui.keys.velocity, 87);
    // Both velocity-lane dragging and clicking leave the preference alone, too.
    let l = t.app.keys.layout.clone();
    let stem = l.notes[0].vel_rect;
    let y = l.velocity[3] - (l.velocity[3] - l.velocity[1] - 6.0) * 32.0 / 127.0;
    t.drag((stem[0] + 1.0, stem[3] - 1.0), (stem[0] + 1.0, y), json!({}));
    assert!((30..=34).contains(&t.notes()[0].velocity));
    assert_eq!(t.app.ui.keys.velocity, 87);
    t.click((stem[0] + 1.0, l.velocity[3] - 0.1));
    assert_eq!(t.notes()[0].velocity, 1);
    assert_eq!(t.app.ui.keys.velocity, 87);

    t.click_n(t.at(1000.0, 64), 2, json!({}));
    assert_eq!(t.notes().iter().find(|n| n.pitch == 64).unwrap().velocity, 87);
    assert_eq!(t.app.keys.last_audition, Some((64, 87)));
    // Moving an existing quiet note auditions its new pitch at the same toolbar velocity.
    let grab = t.at(200.0, 60);
    t.drag(grab, (grab.0, grab.1 - t.app.ui.keys.key_h), json!({}));
    assert_eq!(t.app.keys.last_audition, Some((61, 87)));
    assert_eq!(t.notes().iter().find(|n| n.pitch == 61).unwrap().velocity, 1);
    let kb = t.app.keys.layout.keyboard;
    t.click(((kb[0] + kb[2]) / 2.0, t.at(0.0, 65).1));
    assert_eq!(t.app.keys.last_audition, Some((65, 87)));
}

#[test]
fn velocity_eyedropper_requires_one_selected_note_and_is_ui_state_only() {
    let mut t = T::open();
    t.add(60, 0, 480);
    t.add(64, 960, 480);
    t.app.run("midi.note_edit", json!({"clip": t.clip.0, "index": 0, "velocity": 37})).unwrap();
    t.frames(2);
    t.click(mid(t.app.keys.layout.use_selected_velocity));
    assert_eq!(t.app.ui.keys.velocity, 100, "no selection: disabled");
    t.click(mid(t.note_rect(60, 0)));
    let before = serde_json::to_value(t.app.engine.session()).unwrap();
    let undo = t.undo_len();
    assert_eq!(t.app.ui.keys.velocity, 100, "selection alone never copies");
    t.click(mid(t.app.keys.layout.use_selected_velocity));
    assert_eq!(t.app.ui.keys.velocity, 37, "explicit eyedropper copies the selected note");
    assert_eq!(t.undo_len(), undo);
    assert_eq!(serde_json::to_value(t.app.engine.session()).unwrap(), before);
    t.app.run("ui.set", json!({"keys": {"velocity": 87}})).unwrap();
    t.key("A", json!({"cmd": true}));
    assert_eq!(t.selected(), 2);
    t.click(mid(t.app.keys.layout.use_selected_velocity));
    assert_eq!(t.app.ui.keys.velocity, 87, "multiple notes: disabled");
}

#[test]
fn new_note_velocity_defaults_bounds_and_control_updates_preserve_other_tool_settings() {
    let mut t = T::open();
    let old: crate::UiState = serde_json::from_value(json!({"keys": {"snap": "1/8"}})).unwrap();
    assert_eq!(old.keys.velocity, 100, "old ui.json without velocity gets the default");
    t.app.run("ui.set", json!({"keys": {"snap": "1/8", "audition": false}})).unwrap();
    for (value, expected) in [(-10, 1), (0, 1), (1, 1), (87, 87), (127, 127), (128, 127), (300, 127)] {
        t.app.run("ui.set", json!({"keys": {"velocity": value}})).unwrap();
        assert_eq!(t.app.ui.keys.velocity, expected);
        assert_eq!(t.app.ui.keys.snap, Snap::Eighth);
        assert!(!t.app.ui.keys.audition);
        assert!(t.app.keys_shown());
        assert_eq!(t.app.inspect(&t.ctx)["ui"]["keys"]["velocity"], json!(expected));
        let saved = serde_json::to_vec(&t.app.ui).unwrap();
        let restored: crate::UiState = serde_json::from_slice(&saved).unwrap();
        assert_eq!(restored.keys.velocity, expected);
    }
    for value in [json!("bad"), json!(null), json!(1.5)] {
        assert!(t.app.run("ui.set", json!({"keys": {"velocity": value}})).is_err());
        assert_eq!(t.app.ui.keys.velocity, 127, "invalid updates are atomic");
    }
}

#[test]
fn ctrl_d_arrows_ctrl_up_and_ctrl_u_edit_the_selection() {
    let mut t = T::open();
    t.add(60, 0, 240);
    t.click(mid(t.note_rect(60, 0)));
    let undo = t.undo_len();
    t.key("D", json!({"cmd": true}));
    let n = t.notes();
    assert_eq!(n.iter().map(|n| n.start).collect::<Vec<_>>(), vec![0, 240], "Ctrl+D: one grid length after itself");
    assert_eq!(t.undo_len(), undo + 1);
    assert_eq!(t.app.keys.selection, vec![n[1]], "the copy is selected");
    assert_eq!(t.app.arrange.layout.clips.len(), 1, "the arrangement's Ctrl+D did not run");
    t.key("ArrowRight", json!({}));
    t.key("ArrowUp", json!({}));
    t.key("ArrowUp", json!({"shift": true}));
    let moved = t.app.keys.selection[0];
    assert_eq!((moved.start, moved.pitch), (480, 73), "right a sixteenth, up one, up an octave");
    assert_eq!(t.undo_len(), undo + 4, "each nudge is one step");
    // Ctrl+Up with everything selected: event.transpose on the clip.
    t.key("A", json!({"cmd": true}));
    t.key("ArrowUp", json!({"cmd": true}));
    let mut p: Vec<u8> = t.notes().iter().map(|n| n.pitch).collect();
    p.sort();
    assert_eq!(p, vec![72, 85]);
    assert_eq!(t.app.engine.undo_history().last().map(String::as_str), Some("Transpose..."));
    assert_eq!(t.selected(), 2, "the selection is read back after the edit");
    // Ctrl+U on one off-grid note of three: only that one moves.
    t.add(62, 1000, 200);
    t.click(mid(t.note_rect(62, 1000)));
    t.key("U", json!({"cmd": true}));
    let n = t.notes();
    assert!(n.iter().any(|n| n.pitch == 62 && n.start == 960), "{n:?}");
    assert!(n.iter().any(|n| n.pitch == 85 && n.start == 480), "the other notes stay");
    // The Quantize button with everything selected runs event.quantize.
    t.app.run("midi.notes_move", json!({"clip": t.clip.0, "indices": [0, 1, 2], "ticks": 30})).unwrap();
    t.frames(2);
    t.key("A", json!({"cmd": true}));
    t.click(mid(t.app.keys.layout.quantize));
    assert_eq!(t.app.engine.undo_history().last().map(String::as_str), Some("Quantize..."));
    assert!(t.notes().iter().all(|n| n.start % 240 == 0), "{:?}", t.notes());
}

#[test]
fn the_audition_switch_and_the_keyboard_column() {
    let mut t = T::open();
    assert!(t.app.ui.keys.audition, "on by default");
    t.click(mid(t.app.keys.layout.audition));
    assert!(!t.app.ui.keys.audition);
    let before = t.app.keys.layout.auditions;
    t.click_n(t.at(10.0, 60), 2, json!({}));
    assert_eq!(t.notes().len(), 1);
    assert_eq!(t.app.keys.layout.auditions, before, "off: placing a note is silent");
    t.click(mid(t.app.keys.layout.audition));
    assert!(t.app.ui.keys.audition, "and back on");
    t.click(mid(t.note_rect(60, 0)));
    assert_eq!(t.app.keys.layout.auditions, before + 1, "clicking a note plays it");
    // A key in the keyboard column plays its pitch.
    let kb = t.app.keys.layout.keyboard;
    let (_, y) = t.at(0.0, 65);
    t.click(((kb[0] + kb[2]) / 2.0, y));
    assert_eq!(t.app.keys.layout.last_audition.map(|a| a.0), Some(65));
    assert_eq!(t.app.keys.layout.sounding, Vec::<u8>::new(), "released after the click");
}

#[test]
fn view_gestures_change_the_ui_state_and_never_the_document() {
    let mut t = T::open();
    let doc = serde_json::to_value(t.app.engine.session()).unwrap();
    let undo = t.undo_len();
    // Grid menu.
    t.click(mid(t.app.keys.layout.snap));
    let item = t.app.keys.layout.snap_menu.iter().find(|m| m.label == "1/8").cloned().expect("1/8 listed");
    assert_eq!(t.app.keys.layout.snap_menu.len(), Snap::ALL.len());
    t.click(mid(item.rect));
    assert_eq!(t.app.ui.keys.snap, Snap::Eighth);
    // Pane height: drag the top edge up.
    let h0 = t.app.ui.keys.height;
    let hd = mid(t.app.keys.layout.handle);
    t.drag(hd, (hd.0, hd.1 - 60.0), json!({}));
    assert!((t.app.ui.keys.height - (h0 + 60.0)).abs() < 1.0, "{}", t.app.ui.keys.height);
    assert!((t.app.keys.layout.pane[3] - t.app.keys.layout.pane[1] - t.app.ui.keys.height).abs() < 1.5);
    // Ctrl+wheel zooms time; Ctrl+Shift+wheel and Alt+wheel zoom the keys.
    let g = mid(t.app.keys.layout.grid);
    let z0 = t.app.ui.keys.px_per_quarter;
    t.input("ui.scroll", json!({"x": g.0, "y": g.1, "dy": 120, "cmd": true}));
    assert!(t.app.ui.keys.px_per_quarter > z0, "Ctrl+wheel zoomed in: {}", t.app.ui.keys.px_per_quarter);
    let k0 = t.app.ui.keys.key_h;
    t.input("ui.scroll", json!({"x": g.0, "y": g.1, "dy": 120, "cmd": true, "shift": true}));
    let k1 = t.app.ui.keys.key_h;
    assert!(k1 > k0, "Ctrl+Shift+wheel: {k0} -> {k1}");
    t.input("ui.scroll", json!({"x": g.0, "y": g.1, "dy": -120, "alt": true}));
    assert!(t.app.ui.keys.key_h < k1, "Alt+wheel: {k1} -> {}", t.app.ui.keys.key_h);
    // Plain wheel scrolls the keys.
    let top = t.app.keys.layout.top_pitch;
    t.input("ui.scroll", json!({"x": g.0, "y": g.1, "dy": 80}));
    assert!(t.app.keys.layout.top_pitch > top, "wheel scrolled up");
    // Ruler drag down zooms time in; keyboard drag sideways zooms the keys.
    let z1 = t.app.ui.keys.px_per_quarter;
    let r = mid(t.app.keys.layout.ruler);
    t.drag(r, (r.0, r.1 + 40.0), json!({}));
    assert!(t.app.ui.keys.px_per_quarter > z1 * 1.3, "ruler drag: {z1} -> {}", t.app.ui.keys.px_per_quarter);
    let k2 = t.app.ui.keys.key_h;
    let kb = mid(t.app.keys.layout.keyboard);
    t.drag(kb, (kb.0 + 30.0, kb.1), json!({}));
    assert!(t.app.ui.keys.key_h > k2, "keyboard drag: {k2} -> {}", t.app.ui.keys.key_h);
    // A ruler click moves the cursor (view state too).
    let r = t.app.keys.layout.ruler;
    t.click((r[0] + 5.0, (r[1] + r[3]) / 2.0));
    assert_eq!(t.undo_len(), undo, "no undo entries");
    assert_eq!(serde_json::to_value(t.app.engine.session()).unwrap(), doc, "the document is untouched");
    // The preferences go out with ui.inspect and come back through ui.set.
    let v = t.app.inspect(&t.ctx);
    assert_eq!(v["ui"]["keys"]["snap"], json!("1/8"));
    t.app.run("ui.set", json!({"keys": {"snap": "1/32", "height": 300.0, "audition": false}})).unwrap();
    assert_eq!((t.app.ui.keys.snap, t.app.ui.keys.audition), (Snap::ThirtySecond, false));
}

#[test]
fn the_playhead_follows_and_the_editor_stays_on_its_clip() {
    let mut t = T::open();
    t.app.keys.scroll = 20_000.0;
    t.frames(2);
    t.app.run("transport.play", json!({"from": 0})).unwrap();
    t.frames(10);
    assert!(t.app.is_playing());
    let l = &t.app.keys.layout;
    assert!(l.scroll_ticks < 20_000.0, "the view paged to the playhead: {}", l.scroll_ticks);
    assert_eq!(l.clip, Some(t.clip.0));
    t.key("Space", json!({}));
    assert!(!t.app.is_playing(), "Space still toggles the transport");
    // Undoing the clip away closes the editor.
    while t.app.engine.session().find_clip(t.clip).is_some() {
        t.app.run("edit.undo", json!({})).unwrap();
    }
    t.frames(2);
    assert!(!t.app.keys.layout.visible && t.app.keys.clip.is_none());
}

impl T {
    /// One frame with the given focus state; returns the repaint delay egui was asked for.
    fn frame_with(&mut self, focused: Option<bool>) -> std::time::Duration {
        self.time += 1.0 / 60.0;
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 800.0))),
            time: Some(self.time),
            ..Default::default()
        };
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().focused = focused;
        self.app.raw_input_hook(&mut raw);
        let app = &mut self.app;
        let out = self.ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        out.viewport_output.get(&egui::ViewportId::ROOT).map_or(std::time::Duration::MAX, |v| v.repaint_delay)
    }
}

#[test]
fn an_audition_releases_by_itself_with_the_mouse_still_and_on_focus_loss_or_a_tab_switch() {
    let mut t = T::open();
    t.app.audition(60, t.time);
    let delay = t.frame_with(None);
    assert!(delay <= std::time::Duration::from_millis(50), "a repaint is requested while a note is held: {delay:?}");
    assert_eq!(t.app.keys.layout.sounding, vec![60]);
    // No input at all: frames spaced like the repaint requests release it after 150 ms.
    for _ in 0..3 {
        t.time += 0.05;
        t.frame_with(None);
    }
    assert!(t.app.keys.layout.sounding.is_empty(), "released without any event");
    // Focus loss releases at once.
    t.app.audition(61, t.time);
    t.frame_with(None);
    assert_eq!(t.app.keys.layout.sounding, vec![61]);
    t.frame_with(Some(false));
    assert!(t.app.keys.layout.sounding.is_empty(), "released on focus loss");
    // A tab switch (pane hidden) releases too.
    t.app.audition(62, t.time);
    t.frame_with(None);
    t.app.ui.keys.show = false;
    t.frame_with(None);
    assert!(t.app.keys.held_pitches().is_empty(), "released when the pane is hidden");
    let delay = t.frame_with(None);
    assert!(delay > std::time::Duration::from_millis(50), "no repaint once nothing is held");
}

#[test]
fn notes_past_the_clip_end_are_refused_on_add_and_flagged_when_moved_or_loaded() {
    let mut t = T::open();
    let len = t.app.keys.layout.clip_ticks;
    assert!(len > 0);
    t.add(60, 0, 240);
    // A double-click past the end adds nothing and says why.
    let undo = t.undo_len();
    t.click_n(t.at(len as f64 + 480.0, 64), 2, json!({}));
    assert_eq!(t.notes().len(), 1, "no note added past the end");
    assert_eq!(t.undo_len(), undo);
    assert_eq!(t.app.status, "Outside the clip: drag the clip end to extend");
    assert!(t.app.keys.layout.notes.iter().all(|n| !n.outside));
    // Moving a note past the end is allowed; it is flagged (drawn dimmed and hatched).
    t.app.keys.selection = t.notes();
    t.app.keys_move(len + 480, 0).unwrap();
    t.frames(2);
    assert_eq!(t.notes()[0].start, len + 480);
    assert!(t.app.keys.layout.notes[0].outside, "{:?}", t.app.keys.layout.notes);
    // A note already past the end (a loaded session) is flagged the same way.
    t.add(65, len + 10, 120);
    let n = t.app.keys.layout.notes.iter().find(|n| n.pitch == 65).unwrap();
    assert!(n.outside);
}

#[test]
fn an_empty_instrument_lane_shows_the_create_clip_hint_until_a_clip_exists() {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let r = app.run("track.new", json!({"kind": "instrument"})).unwrap();
    let track = TrackId(r["tracks"][0].as_u64().unwrap());
    let mut t = T { app, ctx: egui::Context::default(), time: 0.0, track, clip: ClipId(0) };
    t.frames(3);
    assert_eq!(t.app.keys.layout.hint_tracks, vec![track.0], "hint on the empty instrument lane");
    let lane = t.app.arrange.layout.tracks[0].lane;
    t.click_n(mid(lane), 2, json!({}));
    assert!(t.app.keys.layout.hint_tracks.is_empty(), "gone once a clip exists");
}

fn track_order(t: &T) -> Vec<TrackId> {
    t.app.engine.session().tracks.iter().map(|x| x.id).collect()
}

#[test]
fn alt_down_moves_the_track_while_the_piano_roll_is_active_and_leaves_the_notes() {
    let mut t = T::open();
    t.add(60, 0, 480);
    let second = TrackId(t.app.run("track.new", json!({"kind": "instrument"})).unwrap()["tracks"][0].as_u64().unwrap());
    t.frames(2);
    t.app.run("edit.select", json!({"tracks": [t.track.0]})).unwrap();
    t.key("ArrowDown", json!({}));
    let moved = t.notes();
    t.app.run("edit.select", json!({"tracks": [t.track.0]})).unwrap();
    assert!(t.app.keys.active && t.app.keys_shown(), "the piano roll has the keyboard");
    let before = track_order(&t);
    let at = before.iter().position(|x| *x == t.track).unwrap();
    t.key("ArrowDown", json!({"alt": true}));
    let after = track_order(&t);
    assert_eq!(after.iter().position(|x| *x == t.track).unwrap(), at + 1, "Alt+Down moved the track below {second:?}");
    assert_eq!(t.notes(), moved, "and the notes did not move");
}

#[test]
fn ctrl_alt_c_is_not_copy_but_ctrl_c_is() {
    let mut t = T::new();
    t.app.click_clip(t.clip, false);
    t.frames(2);
    t.key("C", json!({"cmd": true, "alt": true}));
    assert!(t.app.copied.is_empty(), "Ctrl+Alt+C is not Copy");
    t.key("C", json!({"cmd": true}));
    assert!(!t.app.copied.is_empty(), "Ctrl+C copies");
}

#[test]
fn ctrl_a_selects_every_clip_in_the_arrangement_and_delete_removes_clips_never_tracks() {
    let mut t = T::new();
    let extra = TrackId(t.app.run("track.new", json!({"kind": "instrument"})).unwrap()["tracks"][0].as_u64().unwrap());
    let c2 = t.app.run("midi.new_clip", json!({"tracks": [extra.0], "start": 0, "end": 0})).unwrap()["clip"].as_u64().unwrap();
    t.frames(2);
    t.app.run("edit.select_none", json!({})).unwrap();
    t.app.keys.active = false;
    t.key("A", json!({"cmd": true}));
    let mut sel: Vec<u64> = t.app.engine.session().edit.selected_clips.iter().map(|c| c.0).collect();
    sel.sort();
    assert_eq!(sel, vec![t.clip.0.min(c2), t.clip.0.max(c2)], "both clips");
    let tracks = track_order(&t).len();
    t.key("Delete", json!({}));
    assert!(t.app.engine.session().find_clip(t.clip).is_none() && t.app.engine.session().find_clip(ClipId(c2)).is_none(), "clips gone");
    assert_eq!(track_order(&t).len(), tracks, "tracks stay");
    // Nothing selected but tracks (no header pick): Delete does nothing.
    t.app.run("edit.select_all", json!({})).unwrap();
    t.key("Delete", json!({}));
    assert_eq!(track_order(&t).len(), tracks);
}

#[test]
fn ctrl_a_in_the_piano_roll_selects_notes_not_clips() {
    let mut t = T::open();
    t.add(60, 0, 480);
    t.app.run("edit.select_none", json!({})).unwrap();
    t.key("A", json!({"cmd": true}));
    assert_eq!(t.selected(), 1, "the note");
    assert!(t.app.engine.session().edit.selected_clips.is_empty(), "no clip selected by the arrangement's Ctrl+A");
}

#[test]
fn ctrl_t_adds_an_audio_track_and_ctrl_shift_t_an_instrument_track_and_neither_splits() {
    use soundcraft_model::TrackKind;
    let mut t = T::new();
    t.app.keys.active = false;
    let undo = t.undo_len();
    let n = track_order(&t).len();
    t.key("T", json!({"cmd": true}));
    let kinds: Vec<TrackKind> = t.app.engine.session().tracks.iter().map(|x| x.kind).collect();
    assert_eq!(kinds.len(), n + 1);
    assert_eq!(t.app.engine.session().track(t.app.cursor_track().unwrap()).unwrap().kind, TrackKind::Audio);
    t.key("T", json!({"cmd": true, "shift": true}));
    assert_eq!(track_order(&t).len(), n + 2);
    assert!(t.app.engine.session().track(t.app.cursor_track().unwrap()).unwrap().kind.is_midi());
    assert_eq!(t.undo_len(), undo + 2, "two tracks, no split");
    t.key("E", json!({"cmd": true}));
    assert_eq!(track_order(&t).len(), n + 2, "Ctrl+E still splits only");
}

// ---- Shift+wheel and horizontal-wheel panning (arrangement and piano roll) ----

/// Fraction of the visible width one wheel notch pans.
const NOTCH: f64 = 0.10;

impl T {
    fn notch(&mut self, at: (f32, f32), extra: Value) {
        // A real keyboard reports Shift as held, which synthetic wheel events alone do not.
        let shift = extra.get("shift").and_then(Value::as_bool).unwrap_or(false);
        if shift {
            self.app.synthetic.push(egui::Event::ModifiersChanged(egui::Modifiers::SHIFT));
        }
        self.input("ui.scroll", merged(json!({"x": at.0, "y": at.1, "unit": "line", "steps": 1}), extra));
        if shift {
            self.app.synthetic.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
            self.frames(2);
        }
    }
}

fn arrange_view(t: &T) -> (f64, f64) {
    let l = &t.app.arrange.layout;
    (l.scroll as f64, f64::from(l.timeline[2] - l.timeline[0]) * l.samples_per_px)
}

fn pan_cases(mut scroll: impl FnMut(&mut T, Value) -> (f64, f64)) {
    // (extra wheel params, direction sign): wheel down and a right tilt move later in time.
    let cases =
        [(json!({"dy": -1, "shift": true}), 1.0), (json!({"dy": 1, "shift": true}), -1.0), (json!({"dx": -1}), 1.0), (json!({"dx": 1}), -1.0)];
    for ppp in [1.0f32, 1.5] {
        for (i, (extra, sign)) in cases.iter().enumerate() {
            let mut t = T::open();
            t.ctx.set_pixels_per_point(ppp);
            let (moved, visible) = scroll(&mut t, extra.clone());
            let want = sign * NOTCH * visible;
            assert!((moved - want).abs() <= 0.02 * visible, "case {i} ppp {ppp}: moved {moved}, want {want} (visible {visible})");
        }
    }
}

#[test]
fn shift_wheel_and_a_horizontal_wheel_pan_the_arrangement_a_tenth_of_the_view_per_notch_at_any_zoom() {
    for spp in [100.0, 1500.0] {
        pan_cases(|t, extra| {
            t.app.run("view.zoom_set", json!({"samples_per_px": spp})).unwrap();
            t.app.run("view.scroll", json!({"to": 4_000_000})).unwrap();
            t.frames(3);
            let (s0, visible) = arrange_view(t);
            let l = &t.app.arrange.layout;
            let at = ((l.timeline[0] + l.timeline[2]) / 2.0, (l.timeline[1] + l.timeline[3]) / 2.0);
            t.notch(at, extra);
            (arrange_view(t).0 - s0, visible)
        });
    }
}

#[test]
fn shift_wheel_and_a_horizontal_wheel_pan_the_piano_roll_a_tenth_of_the_view_per_notch_at_any_zoom() {
    for ppq in [60.0f32, 240.0] {
        pan_cases(|t, extra| {
            t.app.ui.keys.px_per_quarter = ppq;
            t.app.keys.scroll = 5000.0;
            t.frames(3);
            let g = t.app.keys.layout.grid;
            let visible = f64::from(g[2] - g[0]) / t.app.keys.layout.px_per_tick;
            let s0 = t.app.keys.scroll;
            t.notch(mid(g), extra);
            (t.app.keys.scroll - s0, visible)
        });
    }
}

#[test]
fn the_plain_wheel_still_scrolls_rows_and_pitches_and_never_time() {
    let mut t = T::open();
    let g = mid(t.app.keys.layout.grid);
    let (s0, top) = (t.app.keys.scroll, t.app.keys.layout.top_pitch);
    t.notch(g, json!({"dy": 1}));
    assert_eq!(t.app.keys.scroll, s0, "plain wheel does not pan time");
    assert!(t.app.keys.layout.top_pitch != top, "plain wheel scrolls pitches");
    let sc = t.app.arrange.layout.scroll;
    let l = &t.app.arrange.layout;
    let at = ((l.timeline[0] + l.timeline[2]) / 2.0, l.timeline[1] + 5.0);
    t.notch(at, json!({"dy": -1}));
    assert_eq!(t.app.arrange.layout.scroll, sc, "plain wheel does not pan the arrangement");
}

// Step entry. There is no MIDI device in the harness: `StudioApp::live_midi` is the entry point
// `step_poll` feeds from `Player::take_played_notes` (the ingress tap), so these tests call it.

impl T {
    fn step_on(&mut self) {
        self.key("S", json!({}));
        assert!(self.app.keys.layout.step_on, "Step is on");
    }

    fn played(&mut self, pitch: u8, velocity: u8, on: bool) {
        self.app.live_midi(soundcraft_playback::PlayedNote { pitch, velocity, on });
        self.frames(2);
    }

    /// A note played and released.
    fn tap(&mut self, pitch: u8, velocity: u8) {
        self.played(pitch, velocity, true);
        self.played(pitch, 0, false);
    }

    fn clip_ticks(&self) -> i64 {
        self.app.keys.layout.clip_ticks
    }
}

const SIXTEENTH: i64 = soundcraft_time::TICKS_PER_QUARTER / 4;

#[test]
fn step_toggles_from_the_toolbar_and_the_key_and_shows_a_cursor() {
    let mut t = T::open();
    assert!(!t.app.keys.layout.step_on);
    t.click(mid(t.app.keys.layout.step));
    assert!(t.app.keys.layout.step_on && t.app.keys.layout.step_cursor == 0 && t.app.keys.layout.step_ticks == SIXTEENTH);
    t.key("S", json!({}));
    assert!(!t.app.keys.layout.step_on, "S toggles it off");
    t.key("S", json!({}));
    assert!(t.app.keys.layout.step_on);
}

#[test]
fn a_played_note_enters_at_the_cursor_with_the_step_length_as_one_undo_step() {
    let mut t = T::open();
    t.step_on();
    let undo = t.undo_len();
    t.played(60, 90, true);
    assert!(t.notes().is_empty(), "entered when the key is released");
    t.played(60, 0, false);
    let n = t.notes();
    assert_eq!((n.len(), n[0].pitch, n[0].start, n[0].length, n[0].velocity), (1, 60, 0, SIXTEENTH, 90));
    assert_eq!(t.app.keys.layout.step_cursor, SIXTEENTH);
    assert_eq!(t.undo_len(), undo + 1);
    t.tap(62, 70);
    assert_eq!(t.notes().iter().map(|n| (n.pitch, n.start)).collect::<Vec<_>>(), vec![(60, 0), (62, SIXTEENTH)]);
    assert_eq!(t.undo_len(), undo + 2);
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.notes().len(), 1, "one step undone");
}

#[test]
fn keys_held_together_enter_one_chord_step() {
    let mut t = T::open();
    t.step_on();
    let undo = t.undo_len();
    t.played(60, 80, true);
    t.played(64, 100, true);
    t.played(67, 90, true);
    t.played(60, 0, false);
    t.played(64, 0, false);
    t.played(67, 0, false);
    let n = t.notes();
    assert_eq!(n.len(), 3);
    assert!(n.iter().all(|x| x.start == 0 && x.length == SIXTEENTH));
    assert_eq!(n.iter().map(|x| (x.pitch, x.velocity)).collect::<Vec<_>>(), vec![(60, 80), (64, 100), (67, 90)]);
    assert_eq!((t.app.keys.layout.step_cursor, t.undo_len()), (SIXTEENTH, undo + 1));
}

#[test]
fn rest_advances_and_backspace_takes_the_last_step_back() {
    let mut t = T::open();
    t.step_on();
    t.tap(60, 100);
    t.key("Period", json!({}));
    assert_eq!((t.app.keys.layout.step_cursor, t.notes().len()), (2 * SIXTEENTH, 1), "rest: cursor moves, no note");
    t.tap(62, 100);
    assert_eq!(t.notes().iter().map(|n| n.start).collect::<Vec<_>>(), vec![0, 2 * SIXTEENTH]);
    let undo = t.undo_len();
    t.key("Backspace", json!({}));
    assert_eq!(
        t.notes().iter().map(|n| n.pitch).collect::<Vec<_>>(),
        vec![60],
        "the last step's note is gone: {} {:?}",
        t.app.status,
        t.app.keys.step
    );
    assert_eq!(t.app.keys.layout.step_cursor, 2 * SIXTEENTH);
    assert_eq!(t.undo_len(), undo + 1, "undoable");
    t.key("Backspace", json!({}));
    assert_eq!((t.app.keys.layout.step_cursor, t.notes().len()), (SIXTEENTH, 1), "the rest is taken back, no note lost");
    t.key("Backspace", json!({}));
    assert!(t.notes().is_empty());
    assert_eq!(t.app.keys.layout.step_cursor, 0);
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.notes().len(), 1, "Backspace undone");
}

#[test]
fn number_keys_set_the_step_length() {
    let mut t = T::open();
    t.step_on();
    let q = soundcraft_time::TICKS_PER_QUARTER;
    for (key, ticks) in [("1", q * 4), ("2", q * 2), ("3", q), ("4", q / 2), ("5", q / 4), ("6", q / 8)] {
        t.key(key, json!({}));
        assert_eq!(t.app.keys.layout.step_ticks, ticks, "key {key}");
    }
    t.key("3", json!({}));
    t.tap(60, 100);
    assert_eq!((t.notes()[0].length, t.app.keys.layout.step_cursor), (q, q));
}

#[test]
fn entering_past_the_clip_end_extends_the_clip() {
    let mut t = T::open();
    t.step_on();
    let q = soundcraft_time::TICKS_PER_QUARTER;
    t.key("1", json!({}));
    assert_eq!(t.clip_ticks(), 4 * q);
    t.tap(60, 100);
    t.tap(62, 100);
    assert_eq!(t.clip_ticks(), 8 * q, "a whole-note step past the bar grows the clip to the step boundary");
    assert_eq!(t.notes().iter().map(|n| n.start).collect::<Vec<_>>(), vec![0, 4 * q]);
}

#[test]
fn entering_stops_at_the_next_clip_and_says_so() {
    let mut t = T::open();
    let (end, _) = {
        let s = t.app.engine.session();
        let c = s.find_clip(t.clip).unwrap().1;
        (c.end(), c.start)
    };
    t.app.run("midi.new_clip", json!({"tracks": [t.track.0], "start": end, "end": end + 48_000})).unwrap();
    t.frames(2);
    t.step_on();
    let q = soundcraft_time::TICKS_PER_QUARTER;
    t.key("1", json!({}));
    t.tap(60, 100);
    assert_eq!(t.notes().len(), 1);
    let undo = t.undo_len();
    t.tap(62, 100);
    assert_eq!((t.notes().len(), t.undo_len(), t.clip_ticks()), (1, undo, 4 * q), "nothing entered, clip not overlapped");
    assert!(t.app.status.contains("next clip"), "{}", t.app.status);
}

#[test]
fn midi_enters_nothing_while_step_is_off() {
    let mut t = T::open();
    t.tap(60, 100);
    assert!(t.notes().is_empty());
    t.step_on();
    t.key("S", json!({}));
    t.tap(60, 100);
    assert!(t.notes().is_empty(), "off again");
}

// Live MIDI keys: the keyboard column accents the keys being played.

#[test]
fn a_note_on_holds_the_key_and_the_note_off_lets_go() {
    let mut t = T::open();
    assert!(t.app.keys.layout.live.is_empty());
    t.played(64, 100, true);
    assert_eq!(t.app.keys.layout.live, vec![(64, 100)]);
    t.played(64, 0, false);
    assert!(t.app.keys.layout.live.is_empty());
}

#[test]
fn a_chord_holds_every_key_and_a_stop_lets_go_of_all() {
    let mut t = T::open();
    for p in [60, 64, 67] {
        t.played(p, 90, true);
    }
    assert_eq!(t.app.keys.layout.live.iter().map(|k| k.0).collect::<Vec<_>>(), vec![60, 64, 67]);
    t.played(64, 0, false);
    assert_eq!(t.app.keys.layout.live.iter().map(|k| k.0).collect::<Vec<_>>(), vec![60, 67]);
    t.app.stop_play();
    t.frames(2);
    assert!(t.app.keys.layout.live.is_empty(), "Stop clears them");
}

#[test]
fn step_mode_still_enters_the_note_and_highlights_the_key() {
    let mut t = T::open();
    t.step_on();
    t.played(60, 90, true);
    assert_eq!(t.app.keys.layout.live, vec![(60, 90)]);
    t.played(60, 0, false);
    assert!(t.app.keys.layout.live.is_empty());
    let n = t.notes();
    assert_eq!((n.len(), n[0].pitch), (1, 60), "the step was entered");
}

#[test]
fn reselecting_a_midi_input_lets_go_of_held_keys() {
    let mut t = T::open();
    t.played(60, 90, true);
    t.app.run("midi.input", json!({"track": t.track.0, "channel": 3})).unwrap();
    t.frames(2);
    assert!(t.app.keys.layout.live.is_empty());
}
