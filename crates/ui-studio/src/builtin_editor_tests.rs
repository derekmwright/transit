//! The editor window of built-in plug-ins (the device card's "Open editor" for a built-in), driven
//! the way a user drives it: synthetic clicks and drags, one frame at a time. No player.

use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::TrackId;

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
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

    fn send(&mut self, id: &str, p: Value) {
        let (req, _rx) = crate::ControlRequest::new(id, p);
        let ctx = egui::Context::default();
        let _ = crate::control::handle_for_test(&mut self.app, &ctx, &req);
        self.frames(12);
    }

    fn click(&mut self, at: (f32, f32), count: u32) {
        self.send("ui.click", json!({"x": at.0, "y": at.1, "count": count}));
    }

    fn track(&mut self, kind: &str) -> TrackId {
        let r = self.app.run("track.new", json!({"kind": kind})).unwrap();
        let t = TrackId(r["tracks"][0].as_u64().unwrap());
        self.app.focus(t);
        self.frames(4);
        t
    }

    fn undo_len(&mut self) -> usize {
        self.app.engine.execute("engine.history", &json!({})).unwrap()["undo"].as_array().unwrap().len()
    }

    fn window(&self, t: TrackId) -> crate::builtin_editor::EditorLayout {
        self.app.builtin_editor.windows.iter().find(|w| w.key.0 == t).cloned().expect("an editor window is open")
    }

    fn row(&self, t: TrackId, param: &str) -> crate::builtin_editor::RowLayout {
        self.window(t).rows.into_iter().find(|r| r.param == param).unwrap_or_else(|| panic!("a row for `{param}`"))
    }

    fn param(&self, t: TrackId, slot: Option<usize>, id: &str) -> Option<f32> {
        let tr = self.app.engine.session().track(t).unwrap();
        let ins = match slot {
            Some(s) => tr.mixer.inserts[s].as_ref(),
            None => tr.instrument.as_ref(),
        };
        ins.and_then(|i| i.params.get(id).copied())
    }

    /// Click the focused track's editor button on card `card`.
    fn open_editor(&mut self, card: usize) {
        let at = mid(self.app.mixer.devices.cards[card].editor_button);
        self.click(at, 1);
    }
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

fn info(plugin: &str) -> &'static soundcraft_dsp::PluginInfo {
    soundcraft_dsp::plugin_info(plugin).unwrap()
}

fn default_of(plugin: &str, id: &str) -> f32 {
    info(plugin).param(id).unwrap().default
}

#[test]
fn the_editor_button_opens_a_window_for_a_built_in_instrument_and_effect() {
    let mut t = T::new();
    let tr = t.track("instrument");
    t.app.add_device(tr, "gain").unwrap();
    t.frames(4);
    assert!(t.app.mixer.devices.cards.iter().all(|c| c.editor_enabled), "built-in editors need no player");
    assert_eq!(t.app.mixer.devices.cards[0].plugin, "subtractive_synth");
    t.open_editor(0);
    let key = (tr, soundcraft_mix::INSTRUMENT_SLOT);
    assert_eq!(t.app.ui.plugin_windows, vec![key]);
    let w = t.window(tr);
    assert_eq!(w.title, format!("{} · Subtractive Synth", t.app.engine.session().track(tr).unwrap().name));
    assert_eq!(w.rows.len(), info("subtractive_synth").params.len(), "one row per parameter");
    assert!(t.app.mixer.devices.cards[0].editor_open);
    t.open_editor(1);
    assert_eq!(t.app.ui.plugin_windows.len(), 2);
    assert_eq!(t.app.builtin_editor.windows.len(), 2);
    assert_eq!(t.app.builtin_editor.windows[1].title.rsplit(" · ").next(), Some("Gain"));
    // The button closes it again, and the window is gone.
    t.open_editor(0);
    assert_eq!(t.app.ui.plugin_windows, vec![(tr, 0)]);
    assert_eq!(t.app.builtin_editor.windows.len(), 1);
}

#[test]
fn dragging_a_slider_writes_the_value_in_one_undo_step() {
    let mut t = T::new();
    let tr = t.track("instrument");
    t.open_editor(0);
    let id = "cutoff";
    let p = info("subtractive_synth").param(id).expect("the synth has a cutoff");
    let before = t.undo_len();
    let r = t.row(tr, id);
    let from = mid(r.control);
    t.send("ui.drag", json!({"x": from.0, "y": from.1, "to_x": r.control[0] + 6.0, "to_y": from.1}));
    let v = t.param(tr, None, id).expect("the drag wrote the parameter");
    assert!(v < p.default, "dragged toward the left end: {v}");
    assert_eq!(t.undo_len(), before + 1, "one undo step for the whole drag");
    assert_eq!(t.row(tr, id).value, p.format(v), "the row shows the value");
    t.app.run("edit.undo", json!({})).unwrap();
    assert!(t.param(tr, None, id).is_none_or(|u| (u - p.default).abs() < 1e-3), "undo restores the default");
}

#[test]
fn double_clicking_a_control_resets_it() {
    let mut t = T::new();
    let tr = t.track("instrument");
    t.app.run("mix.instrument_param", json!({"track": tr.0, "param": "cutoff", "value": 300.0})).unwrap();
    t.frames(3);
    t.open_editor(0);
    t.click(mid(t.row(tr, "cutoff").control), 2);
    assert_eq!(t.param(tr, None, "cutoff"), Some(default_of("subtractive_synth", "cutoff")));
}

#[test]
fn an_effects_editor_has_a_bypass_toggle_and_follows_the_insert_when_reordered() {
    let mut t = T::new();
    let tr = t.track("audio");
    for p in ["gain", "saturator"] {
        t.app.add_device(tr, p).unwrap();
    }
    t.frames(4);
    t.open_editor(0);
    let uid = t.window(tr).uid;
    assert_eq!(t.window(tr).key, (tr, 0));
    t.click(mid(t.window(tr).bypass), 1);
    assert!(t.app.engine.session().track(tr).unwrap().mixer.inserts[0].as_ref().unwrap().bypass);
    // Move the Gain behind the saturator: the same window (same uid) now belongs to slot 1.
    t.app.reorder_device(tr, 0, 1, false).unwrap();
    t.frames(4);
    assert_eq!(t.app.builtin_editor.windows.len(), 1);
    assert_eq!((t.window(tr).uid, t.window(tr).key), (uid, (tr, 1)));
    assert_eq!(t.app.ui.plugin_windows, vec![(tr, 1)]);
    // Edits land on the insert it follows.
    let id = info("gain").params[0].id;
    t.app.run("mix.insert_param", json!({"track": tr.0, "slot": 1, "param": id, "value": info("gain").params[0].max})).unwrap();
    t.frames(2);
    t.click(mid(t.row(tr, id).control), 2);
    assert_eq!(t.param(tr, Some(1), id), Some(default_of("gain", id)));
}

/// A tiny mono 16-bit WAV.
fn wav(path: &std::path::Path) {
    let samples: Vec<i16> = (0..4800).map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16).collect();
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + data_len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(48_000u32.to_le_bytes());
    b.extend(96_000u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data_len.to_le_bytes());
    for s in samples {
        b.extend(s.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
fn the_sampler_window_has_a_sample_picker_and_names_the_sample() {
    let mut t = T::new();
    let tr = t.track("instrument");
    t.app.run("mix.instrument", json!({"track": tr.0, "plugin": "sampler"})).unwrap();
    t.frames(4);
    let dir = std::env::temp_dir().join(format!("transit-sampler-editor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("hit.wav");
    wav(&file);
    let path = file.to_string_lossy().into_owned();
    t.app.services.pick_open = Some(Box::new(move |purpose: &str| (purpose == "mix.sampler_load").then(|| path.clone())));
    t.open_editor(0);
    let w = t.window(tr);
    assert_eq!(w.sample, "No sample");
    assert!(w.picker != [0.0; 4], "the picker is shown");
    t.click(mid(w.picker), 1);
    let s = t.window(tr).sample;
    assert!(s.starts_with("hit"), "the loaded sample is named: {s}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_drum_synth_lists_its_parameters_by_pad() {
    let mut t = T::new();
    let tr = t.track("instrument");
    t.app.run("mix.instrument", json!({"track": tr.0, "plugin": "drum_synth"})).unwrap();
    t.frames(4);
    t.open_editor(0);
    let w = t.window(tr);
    assert_eq!(w.groups, ["Output", "Kick", "Snare", "Closed Hat", "Open Hat", "Toms", "Cymbals"]);
    assert_eq!(w.rows.len(), info("drum_synth").params.len());
}
