//! Audio › Audio Devices tests. Driven like a user: synthetic clicks through the control channel's
//! input queue, one frame at a time, over stand-in devices (`audio.fake_devices`). No player, no
//! audio device is listed or opened.

use crate::audio_devices::{Opened, REFUSED_RECORDING, fallback_notes};
use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_playback::AudioConfig;

const SCARLETT_IN: &str = "Analogue 1 + 2 (Focusrite USB Audio)";
const WEBCAM_IN: &str = "Microphone (HD Webcam)";

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

fn devices() -> Value {
    json!({"systems": [
        {"name": "WASAPI", "is_default": true,
         "outputs": [{"name": "Speakers (Realtek)", "is_default": true}, {"name": "Speakers (Focusrite USB Audio)", "max_channels": 2}],
         "inputs": [{"name": WEBCAM_IN, "is_default": true, "max_channels": 1}, {"name": SCARLETT_IN, "max_channels": 2}]},
        {"name": "ASIO", "outputs": [{"name": "Focusrite USB ASIO", "max_channels": 2}], "inputs": [{"name": "Focusrite USB ASIO", "max_channels": 2}]}
    ]})
}

impl T {
    fn new() -> T {
        let app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let mut t = T { app, ctx: egui::Context::default(), time: 0.0 };
        t.frames(3);
        t.control("audio.fake_devices", devices()).unwrap();
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

    /// A control request; the reply's result, or its error.
    fn control(&mut self, method: &str, p: Value) -> Result<Value, String> {
        let (req, _rx) = crate::ControlRequest::new(method, p);
        let ctx = egui::Context::default();
        let v = crate::control::handle_for_test(&mut self.app, &ctx, &req).unwrap_or(json!({"ok": true}));
        self.frames(10);
        if v["ok"] == json!(true) { Ok(v["result"].clone()) } else { Err(v["error"].as_str().unwrap_or_default().to_string()) }
    }

    fn click(&mut self, at: (f32, f32)) {
        self.control("ui.click", json!({"x": at.0, "y": at.1})).unwrap();
        self.frames(20);
    }

    fn layout(&self) -> Value {
        self.app.inspect(&self.ctx)["audio_devices"].clone()
    }

    fn rect(list: &Value, label: Option<&str>, value: &str) -> [f32; 4] {
        let found = list.as_array().unwrap().iter().find(|m| m["value"] == value && label.is_none_or(|l| m["label"] == l));
        let r = found.unwrap_or_else(|| panic!("no {value} {label:?} in {list}"))["rect"].clone();
        serde_json::from_value(r).unwrap()
    }

    fn control_rect(&self, key: &str) -> [f32; 4] {
        Self::rect(&self.layout()["controls"], None, key)
    }

    /// Open the window from the Audio menu, as a user does.
    fn open_window(&mut self) {
        let bar = serde_json::to_value(&self.app.arrange.bar_menu).unwrap();
        self.click(mid(Self::rect(&bar, Some("Audio"), "")));
        let bar = serde_json::to_value(&self.app.arrange.bar_menu).unwrap();
        self.click(mid(Self::rect(&bar, Some("Audio Devices…"), "")));
        assert_eq!(self.layout()["open"], json!(true), "Audio › Audio Devices… opens the window");
    }

    /// Pick `label` in the `key` list.
    fn pick(&mut self, key: &str, label: &str) {
        self.click(mid(self.control_rect(key)));
        let items = self.layout()["items"].clone();
        self.click(mid(Self::rect(&items, Some(label), key)));
    }

    fn reopened(&self) -> Vec<AudioConfig> {
        self.app.audio_devices.fake.as_ref().map(|f| f.reopened.clone()).unwrap_or_default()
    }
}

#[test]
fn every_choice_starts_at_the_system_default_and_lists_channels() {
    let mut t = T::new();
    t.open_window();
    let lay = t.layout();
    for key in ["system", "output", "input", "rate", "buffer"] {
        let c = lay["controls"].as_array().unwrap().iter().find(|c| c["value"] == key).unwrap().clone();
        assert_eq!(c["label"], "System default", "{key}");
    }
    assert_eq!(lay["systems"], json!(["WASAPI", "ASIO"]));
    t.click(mid(t.control_rect("input")));
    let items: Vec<String> =
        t.layout()["items"].as_array().unwrap().iter().filter(|i| i["value"] == "input").map(|i| i["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(items, ["System default".to_string(), format!("{WEBCAM_IN} · 1 ch"), format!("{SCARLETT_IN} · 2 ch")]);
}

#[test]
fn choosing_an_input_persists_to_ui_state_and_reopens_the_devices() {
    let mut t = T::new();
    t.open_window();
    t.pick("input", &format!("{SCARLETT_IN} · 2 ch"));
    assert_eq!(t.app.ui.audio.input, None, "nothing changes before Apply");
    assert!(t.reopened().is_empty());
    t.click(mid(t.control_rect("apply")));
    assert_eq!(t.app.ui.audio.input.as_deref(), Some(SCARLETT_IN));
    assert_eq!(t.reopened(), vec![t.app.ui.audio.clone()], "Apply reopened the devices once, with the new choice");
    // Saved with the preferences (ui.json is UiState as JSON).
    let saved = serde_json::to_value(&t.app.ui).unwrap();
    assert_eq!(saved["audio"]["input"], SCARLETT_IN);
    let back: crate::UiState = serde_json::from_value(saved).unwrap();
    assert_eq!(back.audio.input.as_deref(), Some(SCARLETT_IN));
    assert!(t.app.status.starts_with("Audio devices reopened"), "{}", t.app.status);
}

#[test]
fn ui_set_audio_reopens_the_devices_too() {
    let mut t = T::new();
    t.control("ui.set", json!({"audio": {"input": SCARLETT_IN, "sample_rate": 48000}})).unwrap();
    assert_eq!(t.app.ui.audio.input.as_deref(), Some(SCARLETT_IN));
    assert_eq!(t.reopened().len(), 1, "ui.set {{audio}} takes Apply's path");
    // Other fields leave the devices alone.
    t.control("ui.set", json!({"count_in": true})).unwrap();
    assert_eq!(t.reopened().len(), 1);
}

#[test]
fn apply_while_recording_is_refused() {
    let mut t = T::new();
    t.open_window();
    t.app.engine.transport.recording = true;
    let want = AudioConfig { input: Some(SCARLETT_IN.into()), ..Default::default() };
    assert_eq!(t.app.apply_audio(want), Err(REFUSED_RECORDING.to_string()));
    assert_eq!(t.app.status, REFUSED_RECORDING);
    assert_eq!(t.app.ui.audio, AudioConfig::default(), "the choice is not taken");
    assert!(t.reopened().is_empty(), "nothing was reopened");
    assert!(t.app.engine.transport.recording, "the take goes on");
    let err = t.control("ui.set", json!({"audio": {"input": SCARLETT_IN}})).unwrap_err();
    assert_eq!(err, REFUSED_RECORDING);
    assert_eq!(t.app.ui.audio, AudioConfig::default());
    t.app.reconnect_audio_devices();
    assert!(t.reopened().is_empty(), "Reconnect is refused while recording too");
}

#[test]
fn apply_while_playing_stops_first() {
    let mut t = T::new();
    t.app.play_from_cursor();
    t.frames(5);
    assert!(t.app.is_playing());
    t.open_window();
    t.pick("output", "Speakers (Focusrite USB Audio) · 2 ch");
    t.click(mid(t.control_rect("apply")));
    assert!(!t.app.is_playing(), "playback stopped");
    assert_eq!(t.reopened().len(), 1);
    assert!(t.app.status.starts_with("Playback stopped to change devices."), "{}", t.app.status);
}

#[test]
fn an_unplugged_device_falls_back_with_a_visible_status() {
    let mut t = T::new();
    let want = AudioConfig { output: Some("Gone Interface".into()), input: Some("Gone Input".into()), ..Default::default() };
    t.app.apply_audio(want).unwrap();
    let s = &t.app.status;
    assert!(s.contains("Output “Gone Interface” is not available; using “Speakers (Realtek)”."), "{s}");
    assert!(s.contains("Input “Gone Input” is not available; recording uses the default input."), "{s}");
    assert!(s.ends_with("Choose another in Audio › Audio Devices."), "{s}");
    t.open_window();
    let lay = t.layout();
    assert_eq!(lay["notes"].as_array().unwrap().len(), 2, "the window shows both fallbacks: {lay}");
    let output = lay["controls"].as_array().unwrap().iter().find(|c| c["value"] == "output").unwrap()["label"].clone();
    assert_eq!(output, "Not available (falls back to the default)");
}

#[test]
fn changing_the_system_resets_its_devices_and_asio_takes_the_input_from_the_output() {
    let mut t = T::new();
    t.open_window();
    t.pick("input", &format!("{SCARLETT_IN} · 2 ch"));
    t.pick("system", "ASIO");
    assert_eq!(t.app.audio_devices.draft.input, None, "the WASAPI input does not belong to ASIO");
    let c = t.layout()["controls"].as_array().unwrap().iter().find(|c| c["value"] == "input").unwrap().clone();
    let r: [f32; 4] = serde_json::from_value(c["rect"].clone()).unwrap();
    t.click(mid(r));
    assert!(t.layout()["items"].as_array().unwrap().iter().all(|i| i["value"] != "input"), "the input list is disabled on ASIO");
}

#[test]
fn without_audio_the_choice_is_only_saved() {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.apply_audio(AudioConfig { input: Some(SCARLETT_IN.into()), ..Default::default() }).unwrap();
    assert_eq!(app.ui.audio.input.as_deref(), Some(SCARLETT_IN));
    assert!(app.player.is_none(), "--no-audio stays without audio");
    assert!(app.status.contains("--no-audio"), "{}", app.status);
}

#[test]
fn refused_rates_and_buffers_are_reported() {
    let cfg = AudioConfig { sample_rate: Some(96_000), buffer_size: Some(64), ..Default::default() };
    let opened = Opened { output: Some("X".into()), rate: Some(48_000), buffer: None };
    let notes = fallback_notes(&cfg, None, &opened);
    assert_eq!(notes, ["96000 Hz is not available; running at 48000 Hz.", "A 64-sample buffer is not available; using the device's default."]);
    let honored = Opened { output: Some("X".into()), rate: Some(96_000), buffer: Some(64) };
    assert!(fallback_notes(&cfg, None, &honored).is_empty());
    let missing_system = AudioConfig { system: Some("JACK".into()), ..Default::default() };
    assert_eq!(fallback_notes(&missing_system, Some(&[]), &Opened::default()), ["Audio system “JACK” is not available; using the default."]);
}
