//! Automation capture for plug-in parameters: a knob move in a built-in panel (or an edit from a
//! hosted editor) records into the parameter's lane in Write, Touch and Latch like a fader move.
//! Driven like a user: synthetic drags through the control channel, one frame at a time. No player.

use crate::automation::Lane;
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

impl T {
    fn new() -> T {
        let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        app.ui.show_mixer = true;
        app.ui.show_devices = true;
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

    fn send(&mut self, method: &str, p: Value) {
        let (req, _rx) = crate::ControlRequest::new(method, p);
        let ctx = egui::Context::default();
        let _ = crate::control::handle_for_test(&mut self.app, &ctx, &req);
        self.frames(12);
    }

    fn new_track(&mut self, kind: &str) -> TrackId {
        let r = self.app.run("track.new", json!({"kind": kind})).unwrap();
        let t = TrackId(r["tracks"][0].as_u64().unwrap());
        self.app.focus(t);
        self.frames(4);
        t
    }

    fn open_editor(&mut self) {
        let (x, y) = mid(self.app.mixer.devices.cards[0].editor_button);
        self.send("ui.click", json!({"x": x, "y": y}));
    }

    /// An audio track with an EQ whose editor is open, in automation mode `mode`.
    fn eq(&mut self, mode: AutomationMode) -> TrackId {
        let t = self.new_track("audio");
        self.app.add_device(t, "eq_7band").unwrap();
        self.frames(4);
        self.app.set_automation_mode(t, mode).unwrap();
        self.open_editor();
        t
    }

    fn knob(&self, t: TrackId, param: &str) -> (f32, f32) {
        let w = self.app.builtin_editor.windows.iter().find(|w| w.key.0 == t).expect("an editor window is open");
        mid(w.rows.iter().find(|r| r.param == param).unwrap_or_else(|| panic!("a knob for {param}")).control)
    }

    /// Turn the knob up by `dy` points over `steps` frames, then let the release arrive.
    fn turn(&mut self, t: TrackId, param: &str, dy: f32, steps: usize) {
        let (x, y) = self.knob(t, param);
        self.send("ui.drag", json!({"x": x, "y": y, "to_x": x, "to_y": y - dy, "steps": steps}));
        self.frames(steps + 5);
    }

    fn play(&mut self) {
        self.send("ui.key", json!({"key": "Space"}));
    }

    fn lane(&self, t: TrackId, slot: u8, param: &str) -> Vec<(i64, f32)> {
        let p = AutoParam::Plugin { slot, param: param.into() };
        self.app.engine.session().track(t).unwrap().lane(&p).map(|l| l.points.iter().map(|p| (p.at, p.value)).collect()).unwrap_or_default()
    }

    fn value(&self, t: TrackId, param: &str) -> f32 {
        self.app.engine.session().track(t).unwrap().mixer.inserts[0].as_ref().unwrap().params.get(param).copied().unwrap_or(0.0)
    }
}

const GAIN: &str = "mid_gain";

#[test]
fn latch_writes_a_knob_drag_at_the_playhead_and_holds_after_release() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Latch);
    t.play();
    assert!(t.app.is_playing());
    t.turn(tr, GAIN, 60.0, 20);
    let pts = t.lane(tr, 0, GAIN);
    assert!(pts.len() >= 5, "points while the knob turned: {}", pts.len());
    assert!(pts.windows(2).all(|w| w[0].0 < w[1].0), "at the playhead, in time order");
    let v = t.value(tr, GAIN);
    assert!(v > 5.0, "the knob moved the band: {v}");
    t.frames(60);
    assert!(t.lane(tr, 0, GAIN).len() >= pts.len());
    let s = t.app.engine.session_arc();
    let lane = Lane::Plugin { slot: 0, param: GAIN.into() };
    let far = t.app.engine.transport.position + 480_000;
    assert!((lane.value_at(s.track(tr).unwrap(), far) - v).abs() < 1e-3, "Latch holds the released value");
    t.play();
    assert!(!t.app.is_playing());
}

#[test]
fn touch_stops_writing_on_release() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Touch);
    t.play();
    t.frames(20);
    assert!(t.lane(tr, 0, GAIN).is_empty(), "Touch writes nothing until a control is held");
    t.turn(tr, GAIN, 60.0, 20);
    let n = t.lane(tr, 0, GAIN).len();
    assert!(n >= 5, "written while held: {n}");
    t.frames(60);
    assert_eq!(t.lane(tr, 0, GAIN).len(), n, "released: Touch stops writing");
    t.play();
}

#[test]
fn write_records_a_touched_parameter_only_while_playing_like_a_fader() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Write);
    // Stopped: the drag sets the value and writes no point.
    t.turn(tr, GAIN, 40.0, 10);
    assert!(t.value(tr, GAIN) > 0.0);
    assert!(t.lane(tr, 0, GAIN).is_empty(), "no points while stopped");
    t.play();
    t.frames(20);
    assert!(t.lane(tr, 0, GAIN).is_empty(), "Write records the touched control, not every parameter");
    t.turn(tr, GAIN, -40.0, 20);
    assert!(t.lane(tr, 0, GAIN).len() >= 5, "points while playing");
    t.play();
}

#[test]
fn read_ignores_the_gesture_but_the_value_changes() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Read);
    t.play();
    t.turn(tr, GAIN, 60.0, 20);
    assert!(t.value(tr, GAIN) > 5.0, "the value changed");
    assert!(t.lane(tr, 0, GAIN).is_empty(), "Read records nothing");
    t.play();
}

#[test]
fn a_double_click_reset_is_one_point() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Touch);
    t.app.run("mix.insert_param", json!({"track": tr.0, "slot": 0, "param": GAIN, "value": 9.0})).unwrap();
    t.frames(3);
    t.play();
    let (x, y) = t.knob(tr, GAIN);
    t.send("ui.click", json!({"x": x, "y": y, "count": 2}));
    t.frames(20);
    let pts = t.lane(tr, 0, GAIN);
    assert_eq!(pts.len(), 1, "one instantaneous point: {pts:?}");
    assert_eq!(pts[0].1, 0.0, "the default");
    t.play();
}

#[test]
fn a_preset_recall_in_latch_writes_one_point_per_changed_parameter() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Latch);
    t.play();
    t.frames(6);
    let info = soundcraft_dsp::plugin_info("eq_7band").unwrap();
    let mut preset = crate::presets::Preset { plugin: "eq_7band".into(), version: 1, params: Default::default() };
    preset.params.insert("mid_gain".into(), 6.0);
    preset.params.insert("high_shelf_gain".into(), -4.0);
    t.app.apply_preset((tr, 0), info, "Test", &preset);
    t.frames(40);
    assert_eq!(t.lane(tr, 0, "mid_gain").len(), 1);
    assert_eq!(t.lane(tr, 0, "high_shelf_gain").len(), 1);
    assert!(t.lane(tr, 0, "low_shelf_gain").is_empty(), "an unchanged parameter writes nothing");
    t.play();
}

#[test]
fn an_instrument_knob_records_into_the_instrument_lane() {
    let mut t = T::new();
    let tr = t.new_track("instrument");
    t.app.set_automation_mode(tr, AutomationMode::Latch).unwrap();
    t.open_editor();
    t.play();
    t.turn(tr, "cutoff", 50.0, 20);
    let pts = t.lane(tr, AutoParam::INSTRUMENT_SLOT, "cutoff");
    assert!(pts.len() >= 5, "the instrument lane took points: {}", pts.len());
    let s = t.app.engine.session_arc();
    let lane = Lane::of_param(tr, soundcraft_mix::INSTRUMENT_SLOT, "cutoff").unwrap().1;
    assert!(lane.valid(s.track(tr).unwrap()));
    assert_eq!(lane.id(), "plugin:instrument:cutoff");
    assert_eq!(Lane::parse(&lane.id()), Some(lane));
    t.play();
}

#[test]
fn hosted_edits_within_the_hold_window_are_one_gesture() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Touch);
    t.play();
    t.frames(6);
    // The edits a hosted editor reports, one value per frame.
    for i in 0..12 {
        t.app.apply_edits(vec![(tr, 0, GAIN.to_string(), i as f32)]);
        t.frames(1);
    }
    assert!(t.lane(tr, 0, GAIN).len() >= 4, "a run of edits recorded a lane");
    // The parameter stays held for 150 ms (9 frames) after the last value; then Touch lets go.
    t.frames(12);
    let n = t.lane(tr, 0, GAIN).len();
    t.frames(30);
    assert_eq!(t.lane(tr, 0, GAIN).len(), n);
    assert_eq!(t.value(tr, GAIN), 11.0);
    assert!(t.app.auto.touches.is_empty());
    t.play();
}

#[test]
fn the_picker_offers_the_lane_and_a_recorded_lane_opens_when_automation_shows() {
    let mut t = T::new();
    let tr = t.eq(AutomationMode::Latch);
    t.app.toggle_automation(tr);
    t.play();
    t.turn(tr, GAIN, 60.0, 20);
    let id = Lane::Plugin { slot: 0, param: GAIN.into() }.id();
    assert!(t.app.ui.auto_lanes.iter().any(|v| v.track == tr && v.lane == id), "the recorded lane opened");
    let s = t.app.engine.session_arc();
    let ch = crate::automation::choices(&mut t.app, &s, s.track(tr).unwrap());
    assert!(ch.iter().any(|c| c.2 == id));
    t.play();
}
