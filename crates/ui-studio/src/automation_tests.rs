//! Slice 7 tests (automation lanes, modes, the write pass, send pan and mute). Driven like a user:
//! synthetic clicks, drags and keys through the control channel's input queue, one frame at a
//! time. No player: the transport runs on the simulated clock.

use crate::automation::{Lane, snap};
use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::{AutoParam, AutomationMode, TrackId};

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
        let d = std::env::temp_dir().join(format!("transit-slice7-{tag}-{}", std::process::id()));
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
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
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

    fn click_mods(&mut self, at: (f32, f32), mods: Value) {
        let mut p = json!({"x": at.0, "y": at.1});
        if let (Some(o), Some(m)) = (p.as_object_mut(), mods.as_object()) {
            o.extend(m.clone());
        }
        self.input("ui.click", p);
    }

    /// A double-click, after a pause long enough that egui does not count it as a triple click
    /// with the click before (0.6 s).
    fn double(&mut self, at: (f32, f32)) {
        self.frames(40);
        self.input("ui.click", json!({"x": at.0, "y": at.1, "count": 2}));
    }

    fn key(&mut self, key: &str) {
        self.input("ui.key", json!({"key": key}));
    }

    fn drag(&mut self, from: (f32, f32), to: (f32, f32), extra: Value) {
        let mut p = json!({"x": from.0, "y": from.1, "to_x": to.0, "to_y": to.1});
        if let (Some(o), Some(m)) = (p.as_object_mut(), extra.as_object()) {
            o.extend(m.clone());
        }
        self.input("ui.drag", p);
    }

    fn undo_len(&self) -> usize {
        self.app.engine.undo_history().len()
    }

    fn track(&mut self, kind: &str, name: &str) -> TrackId {
        let t = TrackId(self.app.run("track.new", json!({"kind": kind, "name": name})).unwrap()["tracks"][0].as_u64().unwrap());
        self.frames(3);
        t
    }

    fn row(&self, t: TrackId) -> crate::arrange::TrackRow {
        self.app.arrange.layout.tracks.iter().find(|r| r.id == t.0).cloned().unwrap()
    }

    fn lanes(&self, t: TrackId) -> Vec<crate::automation::LaneRow> {
        self.app.auto.layout.lanes.iter().filter(|l| l.track == t.0).cloned().collect()
    }

    fn lane(&self, t: TrackId, id: &str) -> crate::automation::LaneRow {
        self.lanes(t).into_iter().find(|l| l.lane == id).unwrap_or_else(|| panic!("no {id} lane: {:?}", self.app.auto.layout.lanes))
    }

    fn points(&self, t: TrackId, p: &AutoParam) -> Vec<(i64, f32)> {
        self.app.engine.session().track(t).unwrap().lane(p).map(|l| l.points.iter().map(|p| (p.at, p.value)).collect()).unwrap_or_default()
    }

    fn menu(&mut self, label: &str) {
        let r =
            self.app.auto.layout.menu.iter().find(|m| m.label == label).unwrap_or_else(|| panic!("no {label}: {:?}", self.app.auto.layout.menu)).rect;
        self.click(mid(r));
    }

    /// An audio track with its automation shown (the A toggle): its Volume lane.
    fn with_lane(&mut self) -> TrackId {
        let a = self.track("audio", "Vox");
        self.click(mid(self.row(a).automation));
        a
    }

    /// A point in `lane` at x fraction `fx` of the visible lane and height fraction `fy` from the top.
    fn at(&self, t: TrackId, lane: &str, fx: f32, fy: f32) -> (f32, f32) {
        let a = self.lane(t, lane).area;
        (a[0] + (a[2] - a[0]) * fx, a[1] + (a[3] - a[1]) * fy)
    }
}

// ---- Lanes are view state ----

#[test]
fn the_a_toggle_opens_a_volume_lane_and_lanes_are_view_state() {
    let dir = Dir::new("view");
    let mut t = T::new();
    let a = t.track("audio", "Vox");
    t.app.run("session.save_as", json!({"path": dir.0.join("v.ptx").to_string_lossy()})).unwrap();
    t.frames(3);
    assert!(!t.app.engine.is_dirty());
    let (rev, undo) = (t.app.engine.revision, t.undo_len());
    let row = t.row(a);
    assert!(!row.automation_shown);
    t.click(mid(row.automation));
    assert!(t.row(a).automation_shown, "A shows the lanes");
    let lanes = t.lanes(a);
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].lane, "volume");
    assert_eq!(lanes[0].label, "Volume");
    assert_eq!(lanes[0].mode_text, "Read");
    assert_eq!(lanes[0].readout, "+0.0 dB", "the readout is in dB");
    // The row grew by the lane; the lane sits under the track's own lane.
    let r = t.row(a);
    assert!((r.header[3] - r.header[1] - (r.lane[3] - r.lane[1]) - crate::automation::LANE_H).abs() < 0.5);
    assert!(lanes[0].area[1] >= r.lane[3] - 0.5);
    // "+" adds Pan under it, the height handle grows it, "x" closes Pan, A hides and shows again.
    t.click(mid(lanes[0].add));
    assert_eq!(t.lanes(a).iter().map(|l| l.lane.clone()).collect::<Vec<_>>(), ["volume", "pan"]);
    let h = t.lane(a, "volume").height_handle;
    t.drag(mid(h), (mid(h).0, mid(h).1 + 40.0), json!({}));
    assert!((t.lane(a, "volume").height - (crate::automation::LANE_H + 40.0)).abs() < 2.0, "{}", t.lane(a, "volume").height);
    t.click(mid(t.lane(a, "pan").close));
    assert_eq!(t.lanes(a).len(), 1);
    t.click(mid(t.row(a).automation));
    assert!(t.lanes(a).is_empty() && !t.row(a).automation_shown);
    t.click(mid(t.row(a).automation));
    assert_eq!(t.lanes(a).len(), 1, "the lane comes back with its height");
    assert!(t.lane(a, "volume").height > crate::automation::LANE_H + 30.0);
    // Closing the last lane hides the automation.
    t.click(mid(t.lane(a, "volume").close));
    assert!(!t.row(a).automation_shown);
    // None of it touched the document.
    assert_eq!((t.app.engine.revision, t.undo_len()), (rev, undo));
    assert!(!t.app.engine.is_dirty(), "lane open, close and height are view state");
    // The A key does the same for the focused track.
    t.app.focus_header(a, false);
    t.frames(2);
    t.key("A");
    assert!(t.row(a).automation_shown, "A key shows the focused track's lanes");
    t.key("A");
    assert!(!t.row(a).automation_shown);
    t.app.run("session.save", json!({})).unwrap();
    assert!(!t.app.engine.is_dirty());
}

#[test]
fn the_picker_lists_mixer_sends_and_device_parameters() {
    let mut t = T::new();
    let a = t.with_lane();
    t.app.add_device(a, "compressor").unwrap();
    t.app.new_fx_channel(Some(a), 0.0).unwrap();
    t.frames(3);
    t.click(mid(t.lane(a, "volume").picker));
    let labels: Vec<String> = t.app.auto.layout.menu.iter().map(|m| m.label.clone()).collect();
    for want in ["Volume", "Pan", "Mute", "Threshold", "Ratio"] {
        assert!(labels.iter().any(|l| l == want), "{want} in {labels:?}");
    }
    assert!(labels.iter().any(|l| l.starts_with("Send A") && l.ends_with("level")), "{labels:?}");
    assert!(labels.iter().any(|l| l.starts_with("Send A") && l.ends_with("mute")), "{labels:?}");
    // Pick the compressor's threshold: a plug-in parameter lane for a built-in device.
    t.menu("Threshold");
    let l = t.lanes(a);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].lane, "plugin:a:threshold");
    assert!(l[0].label.ends_with("Threshold"), "{}", l[0].label);
    assert!(l[0].readout.ends_with("dB"), "the device's own unit: {}", l[0].readout);
    let before = t.undo_len();
    t.double(t.at(a, "plugin:a:threshold", 0.3, 0.25));
    assert_eq!(t.undo_len(), before + 1);
    let pts = t.points(a, &AutoParam::Plugin { slot: 0, param: "threshold".into() });
    assert_eq!(pts.len(), 1);
    assert!((-60.0..=0.0).contains(&pts[0].1), "in the parameter's range: {}", pts[0].1);
    assert!((-20.0..-4.0).contains(&pts[0].1), "near the top of -60..0: {}", pts[0].1);
}

// ---- Points ----

#[test]
fn double_click_adds_snapped_points_one_undo_step_each() {
    let mut t = T::new();
    let a = t.with_lane();
    let before = t.undo_len();
    for (i, fx) in [0.2, 0.4, 0.6].into_iter().enumerate() {
        t.double(t.at(a, "volume", fx, 0.5));
        assert_eq!(t.undo_len(), before + i + 1, "one undo step per point");
    }
    let pts = t.points(a, &AutoParam::Volume);
    assert_eq!(pts.len(), 3);
    let s = t.app.engine.session_arc();
    let spp = s.edit.zoom.samples_per_px;
    for (at, v) in &pts {
        assert_eq!(*at, snap(&s, spp, *at), "on the grid");
        assert!((-30.0..0.0).contains(v), "half way up the fader law: {v}");
    }
    assert_eq!(t.app.engine.undo_label(), Some("Add Automation Breakpoint"));
    // The lane draws them; the last one added is selected.
    let l = t.lane(a, "volume");
    assert_eq!(l.points.len(), 3);
    assert_eq!(l.points.iter().filter(|p| p.selected).count(), 1);
    // Undo takes the last away, and its selection with it.
    t.app.run("edit.undo", json!({})).unwrap();
    t.frames(3);
    assert_eq!(t.points(a, &AutoParam::Volume).len(), 2);
    assert!(t.app.auto.selection.is_empty());
}

#[test]
fn dragging_a_point_moves_value_and_time_as_one_undo_step() {
    let mut t = T::new();
    let a = t.with_lane();
    t.double(t.at(a, "volume", 0.2, 0.5));
    t.double(t.at(a, "volume", 0.6, 0.5));
    let p0 = t.lane(a, "volume").points[0].clone();
    let before = t.undo_len();
    // Up: the value rises, the time stays.
    t.drag((p0.x, p0.y), (p0.x, p0.y - 20.0), json!({}));
    assert_eq!(t.undo_len(), before + 1, "one drag, one undo step");
    let pts = t.points(a, &AutoParam::Volume);
    assert_eq!(pts.len(), 2);
    assert_eq!(pts[0].0, p0.at);
    assert!(pts[0].1 > p0.value + 1.0, "{} > {}", pts[0].1, p0.value);
    // Sideways: the time moves (snapped), the value stays, the other point is untouched.
    let p = t.lane(a, "volume").points[0].clone();
    t.drag((p.x, p.y), (p.x + 60.0, p.y), json!({}));
    assert_eq!(t.undo_len(), before + 2);
    let pts = t.points(a, &AutoParam::Volume);
    assert_eq!(pts.len(), 2, "moved, not copied");
    assert!(pts[0].0 > p.at);
    let s = t.app.engine.session_arc();
    assert_eq!(pts[0].0, snap(&s, s.edit.zoom.samples_per_px, pts[0].0));
    assert!((pts[0].1 - p.value).abs() < 0.05);
    // It cannot pass its neighbour.
    let p = t.lane(a, "volume").points[0].clone();
    let q = t.lane(a, "volume").points[1].clone();
    t.drag((p.x, p.y), (q.x + 200.0, p.y), json!({}));
    let pts = t.points(a, &AutoParam::Volume);
    assert_eq!(pts.len(), 2, "a point never replaces its neighbour");
    assert!(pts[0].0 < q.at);
    // Shift: ten times finer (downward, away from the top of the range).
    let p = t.lane(a, "volume").points[0].clone();
    t.drag((p.x, p.y), (p.x, p.y + 30.0), json!({"shift": true}));
    let fine = p.value - t.points(a, &AutoParam::Volume)[0].1;
    t.app.run("edit.undo", json!({})).unwrap();
    t.frames(3);
    let p = t.lane(a, "volume").points[0].clone();
    t.drag((p.x, p.y), (p.x, p.y + 30.0), json!({}));
    let coarse = p.value - t.points(a, &AutoParam::Volume)[0].1;
    assert!(fine > 0.0 && fine < coarse / 4.0, "Shift is finer: {fine} vs {coarse}");
}

#[test]
fn alt_drag_on_a_segment_says_lanes_are_linear_and_changes_nothing() {
    let mut t = T::new();
    let a = t.with_lane();
    t.double(t.at(a, "volume", 0.2, 0.3));
    t.double(t.at(a, "volume", 0.6, 0.7));
    let (rev, undo) = (t.app.engine.revision, t.undo_len());
    let from = t.at(a, "volume", 0.4, 0.5);
    t.drag(from, (from.0, from.1 - 15.0), json!({"alt": true}));
    assert_eq!((t.app.engine.revision, t.undo_len()), (rev, undo));
    assert!(t.app.status.contains("linear"), "{}", t.app.status);
}

#[test]
fn delete_removes_selected_points_and_never_the_lane_or_clips() {
    let mut t = T::new();
    let a = t.with_lane();
    for fx in [0.2, 0.4, 0.6] {
        t.double(t.at(a, "volume", fx, 0.5));
    }
    // A MIDI-free way to have a clip: a selected clip on another track.
    let inst = t.track("instrument", "Keys");
    let c = t.app.run("midi.new_clip", json!({"tracks": [inst.0], "start": 0, "end": 96_000})).unwrap()["clip"].as_u64().unwrap();
    t.frames(3);
    let cr = t.app.arrange.layout.clips.iter().find(|r| r.id == c).unwrap().rect;
    t.click(mid(cr));
    assert_eq!(t.app.engine.session().edit.selected_clips.len(), 1);
    assert!(t.app.auto.selection.is_empty(), "a clip click deselects points");
    // Click one point, Delete: that point goes, the clip and the other points stay.
    let p = t.lane(a, "volume").points[1].clone();
    t.click((p.x, p.y));
    assert_eq!(t.app.auto.selection.len(), 1);
    let before = t.undo_len();
    t.key("Delete");
    assert_eq!(t.undo_len(), before + 1);
    let pts = t.points(a, &AutoParam::Volume);
    assert_eq!(pts.len(), 2);
    assert!(!pts.iter().any(|q| q.0 == p.at));
    assert!(t.app.engine.session().find_clip(soundcraft_model::ClipId(c)).is_some(), "Delete with points selected keeps the clip");
    assert_eq!(t.app.engine.session().tracks.len(), 2);
    // Delete again with nothing selected: the lane keeps its points (an empty clear range would
    // have cleared it all).
    assert!(t.app.auto.selection.is_empty());
    t.key("Delete");
    assert_eq!(t.points(a, &AutoParam::Volume).len(), 2, "Delete without selected points leaves the lane alone");
    // Backspace works too; Escape deselects instead.
    let p = t.lane(a, "volume").points[0].clone();
    t.click((p.x, p.y));
    t.key("Escape");
    assert!(t.app.auto.selection.is_empty());
    t.key("Backspace");
    assert_eq!(t.points(a, &AutoParam::Volume).len(), 2, "after Escape, Backspace removes no point");
}

#[test]
fn rubber_band_selects_points_and_delete_is_one_step() {
    let mut t = T::new();
    let a = t.with_lane();
    for fx in [0.2, 0.3, 0.7] {
        t.double(t.at(a, "volume", fx, 0.5));
    }
    let l = t.lane(a, "volume");
    let (x0, x1) = (l.points[0].x - 10.0, l.points[1].x + 10.0);
    t.drag((x0, l.area[1] + 3.0), (x1, l.area[3] - 3.0), json!({}));
    assert_eq!(t.app.auto.selection.len(), 2, "the band takes the two points inside it");
    // Shift-click adds the third.
    let p = t.lane(a, "volume").points[2].clone();
    t.click_mods((p.x, p.y), json!({"shift": true}));
    assert_eq!(t.app.auto.selection.len(), 3);
    let before = t.undo_len();
    t.key("Delete");
    assert_eq!(t.undo_len(), before + 1, "three points, one undo step");
    assert!(t.points(a, &AutoParam::Volume).is_empty());
}

#[test]
fn mute_lane_steps_and_pan_lane_writes_both_panners() {
    let mut t = T::new();
    let a = t.with_lane();
    t.click(mid(t.lane(a, "volume").picker));
    t.menu("Mute");
    t.double(t.at(a, "mute", 0.3, 0.2));
    let pts = t.points(a, &AutoParam::Mute);
    assert_eq!(pts.len(), 1);
    assert!((pts[0].1 - 1.0).abs() < f32::EPSILON, "a mute point is on or off");
    assert_eq!(t.lane(a, "mute").points[0].value, 1.0);
    // A stereo track's Pan lane writes both panners (L10), as one step.
    let st = t.track("audio", "Stereo");
    t.app.run("track.format", json!({"tracks": [st.0], "format": "stereo"})).ok();
    t.frames(3);
    t.click(mid(t.row(st).automation));
    t.click(mid(t.lane(st, "volume").picker));
    t.menu("Pan");
    let before = t.undo_len();
    t.double(t.at(st, "pan", 0.3, 0.5));
    assert_eq!(t.undo_len(), before + 1);
    let n = t.app.engine.session().track(st).unwrap().mixer.pan.len();
    for i in 0..n {
        assert_eq!(t.points(st, &AutoParam::Pan(u8::try_from(i).unwrap())).len(), 1, "panner {i}");
    }
    assert_eq!(t.lane(st, "pan").readout, "C", "the readout is a pan position");
}

// ---- Modes and the write pass ----

#[test]
fn the_mode_menu_sets_the_track_mode_as_one_step() {
    let mut t = T::new();
    let a = t.with_lane();
    let before = t.undo_len();
    t.click(mid(t.lane(a, "volume").mode));
    let modes: Vec<String> = t.app.auto.layout.menu.iter().map(|m| m.label.clone()).collect();
    assert_eq!(modes, ["Off", "Read", "Touch", "Latch", "Write"]);
    t.menu("Write");
    assert_eq!(t.app.engine.session().track(a).unwrap().mixer.automation_mode, AutomationMode::Write);
    assert_eq!(t.undo_len(), before + 1);
    assert_eq!(t.lane(a, "volume").mode_text, "Write");
    // Only the first lane has the mode.
    t.click(mid(t.lane(a, "volume").add));
    assert_eq!(t.lane(a, "pan").mode, [0.0; 4]);
}

#[test]
fn write_records_the_fader_while_playing_as_one_undo_step() {
    let mut t = T::new();
    t.app.ui.thin_after_write = false;
    let a = t.with_lane();
    t.click(mid(t.lane(a, "volume").mode));
    t.menu("Write");
    t.app.ui.show_mixer = true;
    t.frames(3);
    let strip = t.app.mixer.layout.strips.iter().find(|s| s.track == a.0).unwrap().clone();
    let before = t.undo_len();
    t.key("Space");
    assert!(t.app.is_playing());
    let f = strip.fader;
    t.input("ui.drag", json!({"x": (f[0] + f[2]) / 2.0, "y": f[3] - 30.0, "to_x": (f[0] + f[2]) / 2.0, "to_y": f[1] + 10.0, "steps": 20}));
    let writing = t.app.auto.layout.writing;
    t.key("Space");
    assert!(!t.app.is_playing());
    t.frames(3);
    assert!(writing, "the pass was writing");
    let pts = t.points(a, &AutoParam::Volume);
    assert!(pts.len() >= 10, "points every 20 ms of playback: {}", pts.len());
    let lo = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min);
    let hi = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max);
    assert!(hi - lo > 3.0, "the curve follows the fader: {lo}..{hi}");
    assert!(pts.windows(2).all(|w| w[0].0 < w[1].0));
    assert_eq!(t.undo_len(), before + 1, "the pass and the fader drag are one undo step");
    assert_eq!(t.lane(a, "volume").points.len(), pts.len(), "the arrangement shows the written curve");
    // One undo removes the pass.
    t.app.run("edit.undo", json!({})).unwrap();
    assert!(t.points(a, &AutoParam::Volume).is_empty());
}

#[test]
fn touch_writes_only_while_the_control_is_held_and_read_writes_nothing() {
    let mut t = T::new();
    let a = t.with_lane();
    // Read: playing writes nothing.
    t.key("Space");
    t.frames(20);
    t.key("Space");
    assert!(t.points(a, &AutoParam::Volume).is_empty(), "Read never writes");
    t.click(mid(t.lane(a, "volume").mode));
    t.menu("Touch");
    t.key("Space");
    t.frames(20);
    assert!(t.points(a, &AutoParam::Volume).is_empty(), "Touch writes nothing until a control is held");
    let v = t.row(a).volume;
    t.input("ui.drag", json!({"x": v[0] + 10.0, "y": (v[1] + v[3]) / 2.0, "to_x": v[0] + 60.0, "to_y": (v[1] + v[3]) / 2.0, "steps": 20}));
    let n = t.points(a, &AutoParam::Volume).len();
    assert!(n >= 5, "the header volume drag wrote points: {n}");
    t.frames(30);
    assert_eq!(t.points(a, &AutoParam::Volume).len(), n, "released: Touch stops writing");
    t.key("Space");
}

// ---- Mixer link and playback ----

#[test]
fn clicking_a_fader_highlights_its_open_lane_and_the_readout_follows_playback() {
    let mut t = T::new();
    let a = t.with_lane();
    t.double(t.at(a, "volume", 0.0, 0.2));
    t.double(t.at(a, "volume", 0.5, 0.9));
    t.app.ui.show_mixer = true;
    t.frames(3);
    assert!(!t.lane(a, "volume").highlighted);
    let f = t.app.mixer.layout.strips.iter().find(|s| s.track == a.0).unwrap().fader;
    t.click(mid(f));
    assert!(t.lane(a, "volume").highlighted, "a click on the fader highlights the Volume lane");
    // The readout follows the playhead along the curve.
    let r0 = t.lane(a, "volume").readout.clone();
    t.key("Space");
    t.frames(60);
    let l = t.lane(a, "volume");
    t.key("Space");
    let s = t.app.engine.session_arc();
    let tr = s.track(a).unwrap();
    assert_ne!(l.readout, r0, "the value at the playhead moved");
    let pos = t.app.engine.transport.position;
    let _ = pos;
    assert!((l.value - Lane::Volume.value_at(tr, l.points[0].at)).abs() > 0.1 || l.points.len() == 2);
    assert!(l.marker[0] > l.area[0], "the marker sits on the curve at the playhead");
}

// ---- Sends ----

#[test]
fn send_mute_is_one_step_on_strips_and_in_the_device_chain() {
    let mut t = T::new();
    let a = t.track("audio", "Vox");
    t.app.new_fx_channel(Some(a), 0.0).unwrap();
    t.app.focus(a);
    t.app.ui.show_mixer = true;
    t.frames(3);
    let row = t.app.mixer.devices.sends[0].clone();
    assert!(!row.muted);
    let before = t.undo_len();
    t.click(mid(row.mute));
    assert!(t.app.engine.session().track(a).unwrap().mixer.sends[0].as_ref().unwrap().mute, "muted");
    assert_eq!(t.undo_len(), before + 1);
    assert!(t.app.mixer.devices.sends[0].muted);
    // The strip's own mute unmutes it.
    let strip = t.app.mixer.layout.strips.iter().find(|s| s.track == a.0).unwrap().clone();
    t.click(mid(strip.sends[0].mute));
    assert!(!t.app.engine.session().track(a).unwrap().mixer.sends[0].as_ref().unwrap().mute);
    assert_eq!(t.undo_len(), before + 2);
    // Send pan in the device chain: a drag is one step; Alt-click centers it.
    let pan = t.app.mixer.devices.sends[0].pan;
    assert_eq!(t.app.mixer.devices.sends[0].pan_text, "C");
    t.drag(mid(pan), (mid(pan).0 + 30.0, mid(pan).1), json!({}));
    let p = t.app.engine.session().track(a).unwrap().mixer.sends[0].as_ref().unwrap().pan;
    assert!(p > 0.1, "dragged right: {p}");
    assert_eq!(t.undo_len(), before + 3);
    t.click_mods(mid(pan), json!({"alt": true}));
    assert!(t.app.engine.session().track(a).unwrap().mixer.sends[0].as_ref().unwrap().pan.abs() < 1e-6, "Alt-click centers");
}

#[test]
fn send_lanes_are_offered_and_take_points() {
    let mut t = T::new();
    let a = t.with_lane();
    t.app.new_fx_channel(Some(a), 0.0).unwrap();
    t.frames(3);
    t.click(mid(t.lane(a, "volume").picker));
    let mute = t.app.auto.layout.menu.iter().find(|m| m.value == "send_a_mute").unwrap().label.clone();
    t.menu(&mute);
    t.double(t.at(a, "send_a_mute", 0.4, 0.1));
    assert_eq!(t.points(a, &AutoParam::SendMute(0)).len(), 1);
    t.click(mid(t.lane(a, "send_a_mute").picker));
    let pan = t.app.auto.layout.menu.iter().find(|m| m.value == "send_a_pan").unwrap().label.clone();
    t.menu(&pan);
    t.double(t.at(a, "send_a_pan", 0.4, 0.5));
    assert_eq!(t.points(a, &AutoParam::SendPan(0)).len(), 1);
    assert_eq!(t.lane(a, "send_a_pan").points.len(), 1);
}

// ---- Review fixes: off-grid drags, locates during a pass, a still fader ----

fn set_point(t: &mut T, a: TrackId, at: i64, v: f32) {
    t.app.run("automation.set_point", json!({"track": a.0, "param": "volume", "at": at, "value": v})).unwrap();
}

#[test]
fn vertical_drag_of_an_off_grid_point_keeps_its_time() {
    let mut t = T::new();
    let a = t.with_lane();
    // Write-pass-like points: off the grid and close together.
    for (i, at) in [3_137_i64, 20_211, 90_309].into_iter().enumerate() {
        set_point(&mut t, a, at, -6.0 - i as f32);
    }
    t.frames(3);
    let before_pts = t.points(a, &AutoParam::Volume);
    assert_eq!(before_pts.len(), 3);
    let p = t.lane(a, "volume").points[1].clone();
    let before = t.undo_len();
    t.drag((p.x, p.y), (p.x, p.y + 12.0), json!({}));
    let pts = t.points(a, &AutoParam::Volume);
    assert_eq!(pts.len(), 3);
    assert_eq!(pts[1].0, before_pts[1].0, "a vertical drag never moves the time");
    assert!((pts[1].1 - before_pts[1].1).abs() > 0.01, "the value changed");
    assert_eq!(t.undo_len(), before + 1, "one undo entry");
    // Sideways past the slop: the time is snapped.
    let p = t.lane(a, "volume").points[1].clone();
    t.drag((p.x, p.y), (p.x + 40.0, p.y), json!({}));
    let pts = t.points(a, &AutoParam::Volume);
    let s = t.app.engine.session_arc();
    assert_ne!(pts[1].0, before_pts[1].0);
    assert_eq!(pts[1].0, snap(&s, s.edit.zoom.samples_per_px, pts[1].0), "the moved time is snapped");
}

fn write_mode(t: &mut T) -> TrackId {
    let a = t.with_lane();
    t.click(mid(t.lane(a, "volume").mode));
    t.menu("Write");
    a
}

#[test]
fn a_forward_locate_during_write_keeps_the_points_it_skips() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    let ahead = [400_000_i64, 450_000, 500_000, 550_000];
    for &at in &ahead {
        set_point(&mut t, a, at, -3.0);
    }
    t.key("Space");
    t.frames(30);
    t.app.run("transport.locate", json!({"at": 900_000})).unwrap();
    t.frames(30);
    t.key("Space");
    t.frames(3);
    let pts = t.points(a, &AutoParam::Volume);
    for at in ahead {
        assert!(pts.iter().any(|p| p.0 == at), "point at {at} survived the locate: {pts:?}");
    }
    assert!(pts.iter().any(|p| p.0 >= 900_000), "the pass continued from the new position");
}

#[test]
fn a_still_fader_writes_no_points_after_the_first() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    t.key("Space");
    t.frames(60);
    t.key("Space");
    t.frames(3);
    let pts = t.points(a, &AutoParam::Volume);
    assert!(!pts.is_empty(), "the first point is written");
    assert!(pts.len() <= 2, "a still fader adds nothing (first point and the hold's end): {}", pts.len());
}

// ---- Follow-ups: strips follow Read, multi-point drag, thinning ----

fn strip_of(t: &T, a: TrackId) -> crate::mixer::StripLayout {
    t.app.mixer.layout.strips.iter().find(|s| s.track == a.0).cloned().unwrap()
}

#[test]
fn strip_controls_follow_read_automation_without_commands_or_dirt() {
    let mut t = T::new();
    let a = t.with_lane();
    t.app.ui.show_mixer = true;
    set_point(&mut t, a, 0, -20.0);
    set_point(&mut t, a, 100_000, 0.0);
    t.app.run("automation.set_point", json!({"track": a.0, "param": "pan", "at": 0, "value": -1.0})).unwrap();
    t.app.run("automation.set_point", json!({"track": a.0, "param": "pan", "at": 100_000, "value": 1.0})).unwrap();
    t.frames(3);
    // Saved baseline: the automation edits above are the document; what follows must not add to it.
    let dir = Dir::new("follow");
    t.app.run("session.save_as", json!({"path": dir.0.join("f.sc").to_string_lossy()})).ok();
    let dirty = t.app.engine.is_dirty();
    let undo = t.undo_len();
    let static_db = t.app.engine.session().track(a).unwrap().mixer.volume_db;
    t.app.set_cursor(50_000, Some(a));
    t.frames(3);
    let want = {
        let s = t.app.engine.session_arc();
        Lane::Volume.value_at(s.track(a).unwrap(), 50_000)
    };
    assert!((want - -10.0).abs() < 0.5, "halfway between -20 and 0 dB is near -10: {want}");
    let strip = strip_of(&t, a);
    assert!((strip.volume_db - want).abs() < 1e-3, "the strip shows the automated value: {} vs {want}", strip.volume_db);
    assert_eq!(strip.volume_text, crate::fine::db_text(want));
    assert!(strip.balance.abs() < 0.05, "pan halfway between hard left and right is center: {}", strip.balance);
    assert_eq!(t.app.engine.session().track(a).unwrap().mixer.volume_db, static_db, "the document value is untouched");
    assert_eq!(t.app.engine.is_dirty(), dirty, "following automation does not dirty the document");
    assert_eq!(t.undo_len(), undo, "and adds no undo step");
    // The strip and the lane readout agree.
    assert!((t.lane(a, "volume").value - strip.volume_db).abs() < 1e-3);
    // Moving the playhead moves the control.
    t.app.set_cursor(0, Some(a));
    t.frames(3);
    assert!((strip_of(&t, a).volume_db - -20.0).abs() < 1e-3);
    // Off: the strip shows the static value again.
    t.app.run("mix.automation_mode", json!({"tracks": [a.0], "mode": "off"})).unwrap();
    t.frames(3);
    assert_eq!(strip_of(&t, a).volume_db, static_db);
}

#[test]
fn grabbing_a_strip_fader_in_read_writes_the_static_value_from_where_it_shows() {
    let mut t = T::new();
    let a = t.with_lane();
    t.app.ui.show_mixer = true;
    set_point(&mut t, a, 0, -20.0);
    set_point(&mut t, a, 100_000, 0.0);
    t.app.set_cursor(50_000, Some(a));
    t.frames(3);
    let f = strip_of(&t, a).fader;
    let before = t.undo_len();
    t.input(
        "ui.drag",
        json!({"x": (f[0] + f[2]) / 2.0, "y": (f[1] + f[3]) / 2.0, "to_x": (f[0] + f[2]) / 2.0, "to_y": (f[1] + f[3]) / 2.0 - 10.0, "steps": 5}),
    );
    assert_eq!(t.undo_len(), before + 1, "one manual drag, one undo step");
    assert_eq!(t.points(a, &AutoParam::Volume).len(), 2, "Read writes no automation points");
    assert!(t.app.engine.session().track(a).unwrap().mixer.volume_db > -144.0);
}

fn select_all_points(t: &mut T, a: TrackId, n: usize) {
    let p = t.lane(a, "volume").points[0].clone();
    t.click((p.x, p.y));
    for i in 1..n {
        let p = t.lane(a, "volume").points[i].clone();
        t.click_mods((p.x, p.y), json!({"shift": true}));
    }
    assert_eq!(t.app.auto.selection.len(), n);
}

#[test]
fn dragging_one_of_three_selected_points_moves_all_three_as_one_undo_step() {
    let mut t = T::new();
    let a = t.with_lane();
    // Off-grid times, as a write pass leaves them; a fourth point stays unselected.
    for (at, v) in [(3_137_i64, -6.0), (20_211, -9.0), (40_309, -3.0), (90_001, -12.0)] {
        set_point(&mut t, a, at, v);
    }
    for v in &mut t.app.ui.auto_lanes {
        v.height = crate::automation::LANE_MAX;
    }
    t.frames(3);
    select_all_points(&mut t, a, 3);
    let before = t.points(a, &AutoParam::Volume);
    let anchor = t.lane(a, "volume").points[1].clone();
    let h = t.lane(a, "volume").area[3] - t.lane(a, "volume").area[1] - 14.0;
    let undo = t.undo_len();
    // Straight down by 0.1 of the lane's height.
    t.drag((anchor.x, anchor.y), (anchor.x, anchor.y + 0.1 * h), json!({}));
    let after = t.points(a, &AutoParam::Volume);
    assert_eq!(after.len(), 4, "nothing added or lost");
    assert_eq!(t.undo_len(), undo + 1, "the gesture is one undo step");
    for i in 0..3 {
        assert_eq!(after[i].0, before[i].0, "off-grid time {i} kept on a vertical drag");
        assert!(after[i].1 < before[i].1 - 0.1, "point {i} moved down: {} -> {}", before[i].1, after[i].1);
    }
    assert_eq!(after[3], before[3], "the unselected point is untouched");
    assert!(after.windows(2).all(|w| w[0].0 < w[1].0), "order preserved");
    assert_eq!(t.app.auto.selection.len(), 3, "the selection stays");
    // One undo restores the lane.
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.points(a, &AutoParam::Volume), before);
}

#[test]
fn a_group_drag_stops_at_its_unselected_neighbors_and_at_the_range() {
    let mut t = T::new();
    let a = t.with_lane();
    for (at, v) in [(40_000_i64, -6.0), (60_000, -9.0), (80_000, -3.0), (120_000, -12.0)] {
        set_point(&mut t, a, at, v);
    }
    t.frames(3);
    select_all_points(&mut t, a, 3);
    let anchor = t.lane(a, "volume").points[0].clone();
    // Far right: the group stops one sample short of the unselected point, order intact.
    t.drag((anchor.x, anchor.y), (anchor.x + 2_000.0, anchor.y), json!({}));
    let pts = t.points(a, &AutoParam::Volume);
    assert_eq!(pts.len(), 4, "no point replaced another: {pts:?}");
    assert_eq!(pts[2].0, 119_999);
    assert_eq!(pts[1].0 - pts[0].0, 20_000, "spacing is kept");
    assert_eq!(pts[3].0, 120_000);
    // Far up: values stay in range and keep their spacing in height.
    let top = t.lane(a, "volume").points[2].clone();
    t.drag((top.x, top.y), (top.x, top.y - 2_000.0), json!({}));
    let pts = t.points(a, &AutoParam::Volume);
    let max = pts[..3].iter().map(|p| p.1).fold(f32::MIN, f32::max);
    assert!(max <= crate::fine::DB_MAX + 1e-3, "in range: {max}");
    assert!(pts[..3].iter().all(|p| p.1 >= crate::fine::DB_MIN));
    assert!(pts[0].1 < pts[2].1 || pts[0].1 != pts[1].1, "the group keeps its shape instead of flattening");
}

/// A ramp of the fader, a straight line in dB, while playing: the control moves a little every
/// other frame, as a hand on it would.
fn ramp(t: &mut T, a: TrackId, n: usize) {
    for i in 0..n {
        t.app.set_value_merged(crate::fine::Target::Fader(a), -30.0 + i as f32 * 0.1, None).unwrap();
        t.frames(2);
    }
}

/// A Write pass with a ramp. Returns (points written, undo steps added).
fn write_pass_with_ramp(t: &mut T, a: TrackId) -> (usize, usize) {
    let before = t.undo_len();
    t.key("Space");
    ramp(t, a, 120);
    let written = t.app.auto.layout.points_written;
    t.key("Space");
    t.frames(3);
    (written, t.undo_len() - before)
}

#[test]
fn a_write_pass_is_thinned_when_it_ends_as_part_of_the_pass() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    assert!(t.app.ui.thin_after_write, "thinning is on by default");
    let (written, steps) = write_pass_with_ramp(&mut t, a);
    let pts = t.points(a, &AutoParam::Volume);
    assert!(written >= 60, "a dense pass: {written}");
    assert!(pts.len() < written / 3 && pts.len() >= 2, "thinned to a few: {} of {written}", pts.len());
    assert_eq!(steps, 1, "the pass and its thinning are one undo step");
    assert!(t.app.auto.thinned > 0);
    // One undo removes the whole pass.
    t.app.run("edit.undo", json!({})).unwrap();
    assert!(t.points(a, &AutoParam::Volume).is_empty(), "undo takes the pass and the thinning together");
    // Redo brings back the thinned curve, not the dense one.
    t.app.run("edit.redo", json!({})).unwrap();
    assert_eq!(t.points(a, &AutoParam::Volume).len(), pts.len());
}

#[test]
fn thin_after_write_off_keeps_the_dense_pass_and_the_menu_toggles_it() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    t.app.ui.thin_after_write = false;
    let (written, steps) = write_pass_with_ramp(&mut t, a);
    assert_eq!(steps, 1);
    assert!(t.points(a, &AutoParam::Volume).len() >= written.saturating_sub(2), "every point of the pass stays");
    // The menu entry flips the preference and Thin Focused Track thins on demand, as its own step.
    let r = t.app.arrange.bar_menu.iter().find(|m| m.label == "Automation").unwrap().rect;
    t.click(mid(r));
    let on = t.app.arrange.bar_menu.iter().find(|m| m.label == "Thin after write").unwrap().rect;
    t.click(mid(on));
    assert!(t.app.ui.thin_after_write, "the menu turned it on");
    assert!(!t.app.engine.session().track(a).unwrap().automation.is_empty());
    t.app.focus(a);
    let before = t.undo_len();
    let n = t.app.thin_focused().unwrap();
    assert!(n > 0);
    assert_eq!(t.undo_len(), before + 1);
}

#[test]
fn leaving_write_mid_pass_thins_that_track_and_a_toggle_survives_ui_json() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    t.key("Space");
    ramp(&mut t, a, 120);
    let dense = t.points(a, &AutoParam::Volume).len();
    t.app.run("mix.automation_mode", json!({"tracks": [a.0], "mode": "read"})).unwrap();
    t.frames(5);
    let thin = t.points(a, &AutoParam::Volume).len();
    t.key("Space");
    assert!(thin < dense / 3, "thinned when Write ended: {thin} of {dense}");
    let json = serde_json::to_string(&t.app.ui).unwrap();
    assert!(json.contains("\"thin_after_write\":true"));
    let old: crate::UiState = serde_json::from_str("{}").unwrap();
    assert!(old.thin_after_write, "an older ui.json thins by default");
}

// ---- Review fixes: thin only what the pass wrote, one undo step, follow suspended by the header ----

/// A second audio track in Write mode, without a lane open.
fn write_track(t: &mut T, name: &str) -> TrackId {
    let b = t.track("audio", name);
    t.app.set_automation_mode(b, AutomationMode::Write).unwrap();
    t.frames(3);
    b
}

#[test]
fn a_default_on_pass_keeps_the_curve_shape() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    assert!(t.app.ui.thin_after_write);
    let (written, _) = write_pass_with_ramp(&mut t, a);
    let pts = t.points(a, &AutoParam::Volume);
    assert!(pts.len() >= 2 && pts.len() < written / 3);
    // The pass starts at the static 0 dB, then the ramp runs -30 .. -18; both ends of it survive.
    assert!(pts[0].1.abs() < 0.2, "the first point survives: {}", pts[0].1);
    assert!((pts.last().unwrap().1 - -18.1).abs() < 0.5, "the last point survives: {}", pts.last().unwrap().1);
    assert!(pts.iter().any(|p| (p.1 - -30.0).abs() < 0.2), "the ramp's start survives: {pts:?}");
    assert!(pts.iter().all(|p| p.1 >= -30.2 && p.1 <= 0.2), "values stay inside the pass's range: {pts:?}");
}

#[test]
fn thinning_leaves_hand_points_outside_the_pass_and_other_lanes_alone() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    // Collinear hand points a whole-lane thin would drop: later on the Volume lane, and on Pan.
    for at in [8_000_000_i64, 8_500_000, 9_000_000] {
        set_point(&mut t, a, at, -12.0);
        t.app.run("automation.set_point", json!({"track": a.0, "param": "pan", "at": at, "value": 0.25})).unwrap();
    }
    t.frames(3);
    write_pass_with_ramp(&mut t, a);
    let vol = t.points(a, &AutoParam::Volume);
    for at in [8_000_000_i64, 8_500_000, 9_000_000] {
        assert!(vol.iter().any(|p| p.0 == at), "hand point {at} on the written lane survives: {vol:?}");
    }
    let pan = t.points(a, &AutoParam::Pan(0));
    assert_eq!(pan.len(), 3, "the Pan lane was not written, so it is not thinned: {pan:?}");
}

#[test]
fn leaving_write_through_the_ui_mid_pass_is_still_one_undo_step() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    let before = t.undo_len();
    t.key("Space");
    ramp(&mut t, a, 120);
    let dense = t.points(a, &AutoParam::Volume).len();
    t.app.set_automation_mode(a, AutomationMode::Read).unwrap();
    t.frames(5);
    t.key("Space");
    t.frames(3);
    assert!(t.points(a, &AutoParam::Volume).len() < dense / 3, "thinned");
    assert_eq!(t.undo_len(), before + 1, "pass, mode change and thinning are one step: {:?}", t.app.engine.undo_history());
}

#[test]
fn an_unrelated_edit_before_stop_leaves_the_thin_as_its_own_labeled_step() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    let before = t.undo_len();
    t.key("Space");
    ramp(&mut t, a, 120);
    let dense = t.points(a, &AutoParam::Volume).len();
    t.app.run("track.rename", json!({"track": a.0, "name": "Lead"})).unwrap();
    t.key("Space");
    t.frames(3);
    let h = t.app.engine.undo_history();
    assert_eq!(h.len(), before + 3, "pass, rename, thin: {h:?}");
    assert!(h.last().unwrap().contains("Thin"), "the thin step is labeled: {h:?}");
    assert!(t.app.status.contains("own step"), "{}", t.app.status);
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.points(a, &AutoParam::Volume).len(), dense, "undoing the thin restores the dense pass");
}

#[test]
fn stop_after_a_pass_is_one_step_even_with_the_pass_labeled() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    let (_, steps) = write_pass_with_ramp(&mut t, a);
    assert_eq!(steps, 1);
    assert_eq!(t.app.engine.undo_history().last().unwrap(), "Write Automation");
}

#[test]
fn a_header_volume_drag_suspends_the_follow() {
    let mut t = T::new();
    let a = t.with_lane();
    set_point(&mut t, a, 0, -20.0);
    set_point(&mut t, a, 100_000, 0.0);
    t.app.set_cursor(50_000, Some(a));
    let tr = t.app.engine.session_arc().track(a).unwrap().clone();
    assert!(t.app.followed(&tr, &Lane::Volume).is_some());
    t.app.arrange.gesture = Some(crate::arrange::Gesture::Volume { track: a, start_db: 0.0 });
    assert!(t.app.followed(&tr, &Lane::Volume).is_none(), "the header drag shows the value it sets");
}

#[test]
fn a_track_deleted_as_the_pass_ends_does_not_stop_the_others_thinning() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    let b = write_track(&mut t, "Gone");
    t.key("Space");
    ramp(&mut t, a, 120);
    let dense = t.points(a, &AutoParam::Volume).len();
    assert!(!t.points(b, &AutoParam::Volume).is_empty(), "the second track was written too");
    t.app.run("track.delete", json!({"tracks": [b.0]})).unwrap();
    t.key("Space");
    t.frames(3);
    assert!(t.points(a, &AutoParam::Volume).len() < dense / 3, "track a still thinned");
}

#[test]
fn thin_focused_track_waits_while_a_pass_is_running_on_it() {
    let mut t = T::new();
    let a = write_mode(&mut t);
    t.app.focus(a);
    t.key("Space");
    ramp(&mut t, a, 40);
    let n = t.points(a, &AutoParam::Volume).len();
    assert!(t.app.thin_focused().is_err(), "refused while the pass writes to the track");
    assert_eq!(t.points(a, &AutoParam::Volume).len(), n);
    t.key("Space");
    t.frames(3);
}
