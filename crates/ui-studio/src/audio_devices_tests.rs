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

#[test]
fn reconnect_drops_in_order_restores_monitoring_and_reopens_editors() {
    let mut t = T::new();
    t.app.add_track("audio").unwrap();
    let track = t.app.engine.session().tracks[0].id;
    t.app.engine.session_mut().track_mut(track).unwrap().mixer.input_monitor = true;
    let key = (track, 0);
    t.app.want_editor(key);
    t.frames(2);
    assert!(t.app.editors.iter().any(|w| w.opened));
    let fake = t.app.audio_devices.fake.as_ref().unwrap();
    assert!(fake.input.is_some());
    fake.trace.borrow_mut().clear();

    t.app.apply_audio(AudioConfig { input: Some(SCARLETT_IN.into()), ..Default::default() }).unwrap();
    assert_eq!(
        *t.app.audio_devices.fake.as_ref().unwrap().trace.borrow(),
        ["close editors", "drop input", "drop output", "open output", "open input", "connect monitor"]
    );
    t.frames(2);
    assert_eq!(t.app.ui.plugin_windows, [key], "a reopen is not a user close");
    assert!(t.app.editors.iter().any(|w| w.key == key && w.opened));
    let fake = t.app.audio_devices.fake.as_ref().unwrap();
    assert!(fake.connection.as_ref().unwrap().editors.borrow().contains(&key));
    assert_eq!(fake.trace.borrow().last(), Some(&"open editor"));

    // Real user closure still removes the editor from the wanted list.
    fake.connection.as_ref().unwrap().editors.borrow_mut().clear();
    t.frames(2);
    assert!(!t.app.ui.plugin_windows.contains(&key));
}

#[test]
fn only_open_and_rescan_enumerate_devices() {
    let mut t = T::new();
    t.open_window();
    assert_eq!(t.app.audio_devices.fake.as_ref().unwrap().enumerations, 1);
    // Simulate a provider change: the cached listing remains authoritative until Rescan.
    t.app.audio_devices.fake.as_mut().unwrap().systems[0].inputs.clear();
    t.control("ui.set", json!({"audio": {"system": "WASAPI", "input": SCARLETT_IN}})).unwrap();
    assert_eq!(t.app.audio_devices.fake.as_ref().unwrap().enumerations, 1);
    assert!(!t.app.status.contains("Input “"), "Apply uses the cached input list");
    t.app.apply_audio(t.app.ui.audio.clone()).unwrap();
    t.frames(10);
    assert_eq!(t.app.audio_devices.fake.as_ref().unwrap().enumerations, 1);
    t.click(mid(t.control_rect("rescan")));
    assert_eq!(t.app.audio_devices.fake.as_ref().unwrap().enumerations, 2);
    t.app.apply_audio(t.app.ui.audio.clone()).unwrap();
    assert!(t.app.status.contains("Input “"), "Rescan refreshes the fallback check");
    t.app.open_audio_devices();
    t.frames(2);
    assert_eq!(t.app.audio_devices.fake.as_ref().unwrap().enumerations, 3);
}

#[test]
fn audio_patch_merges_fields_and_rejects_zero_atomically() {
    let mut t = T::new();
    t.control("ui.set", json!({"audio": {"system": "WASAPI", "input": SCARLETT_IN, "sample_rate": 48000}})).unwrap();
    t.control("ui.set", json!({"audio": {"buffer_size": 128}})).unwrap();
    assert_eq!(
        t.app.ui.audio,
        AudioConfig {
            system: Some("WASAPI".into()),
            input: Some(SCARLETT_IN.into()),
            sample_rate: Some(48000),
            buffer_size: Some(128),
            output: None,
        }
    );
    let before = t.app.ui.audio.clone();
    let reopened = t.reopened();
    for field in ["sample_rate", "buffer_size"] {
        assert!(t.control("ui.set", json!({"audio": {field: 0}, "count_in": true})).unwrap_err().contains("greater than zero"));
        assert_eq!(t.app.ui.audio, before);
        assert!(!t.app.ui.count_in, "the whole invalid patch is refused");
        let mut bad = before.clone();
        if field == "sample_rate" {
            bad.sample_rate = Some(0);
        } else {
            bad.buffer_size = Some(0);
        }
        assert!(t.app.apply_audio(bad).is_err());
    }
    assert_eq!(t.reopened(), reopened);
    t.control("ui.set", json!({"audio": {"input": null}})).unwrap();
    assert_eq!(t.app.ui.audio.input, None, "null still means system default");
    assert_eq!(t.app.ui.audio.buffer_size, Some(128));
}

#[test]
fn audio_zero_rate_and_buffer_are_refused() {
    let mut t = T::new();
    for field in ["sample_rate", "buffer_size"] {
        assert!(t.control("ui.set", json!({"audio": {field: 0}})).is_err());
        assert_eq!(t.app.ui.audio, AudioConfig::default());
        assert!(t.reopened().is_empty());
    }
}

#[test]
fn editors_return_after_device_reopen() {
    let mut t = T::new();
    let key = (soundcraft_model::TrackId(1), 0);
    t.app.want_editor(key);
    t.frames(2);
    assert!(t.app.editors[0].opened);
    t.app.apply_audio(AudioConfig::default()).unwrap();
    t.frames(2);
    assert_eq!(t.app.ui.plugin_windows, [key]);
    assert!(t.app.audio_devices.fake.as_ref().unwrap().connection.as_ref().unwrap().editors.borrow().contains(&key));
}

#[test]
fn reopened_input_is_connected_to_monitor() {
    let mut t = T::new();
    t.app.add_track("audio").unwrap();
    t.app.engine.session_mut().tracks[0].mixer.record_arm = true;
    t.app.apply_audio(AudioConfig::default()).unwrap();
    let f = t.app.audio_devices.fake.as_ref().unwrap();
    assert!(f.input.is_some());
    assert_eq!(f.trace.borrow().last(), Some(&"connect monitor"));
}

#[test]
fn reconnect_menu_and_direct_call_refuse_recording() {
    let mut t = T::new();
    t.app.engine.transport.recording = true;
    t.app.reconnect_audio();
    assert_eq!(t.app.status, REFUSED_RECORDING);
    assert!(t.app.engine.transport.recording);
    assert!(t.reopened().is_empty());
    let bar = serde_json::to_value(&t.app.arrange.bar_menu).unwrap();
    t.click(mid(T::rect(&bar, Some("Audio"), "")));
    let bar = serde_json::to_value(&t.app.arrange.bar_menu).unwrap();
    t.click(mid(T::rect(&bar, Some("Reconnect Device"), "")));
    assert!(t.reopened().is_empty());
    assert!(t.app.engine.transport.recording);
    // The same menu works when the take has stopped.
    t.app.engine.transport.recording = false;
    t.frames(2);
    if !t.app.arrange.bar_menu.iter().any(|m| m.label == "Reconnect Device") {
        let bar = serde_json::to_value(&t.app.arrange.bar_menu).unwrap();
        t.click(mid(T::rect(&bar, Some("Audio"), "")));
    }
    let bar = serde_json::to_value(&t.app.arrange.bar_menu).unwrap();
    t.click(mid(T::rect(&bar, Some("Reconnect Device"), "")));
    assert_eq!(t.reopened().len(), 1);
}

#[test]
fn driver_panel_follows_running_system_not_draft() {
    let mut t = T::new();
    t.open_window();
    let has_panel = |t: &T| t.layout()["controls"].as_array().unwrap().iter().any(|c| c["value"] == "panel");
    t.pick("system", "ASIO");
    assert!(!has_panel(&t), "WASAPI is still running");
    t.pick("output", "Focusrite USB ASIO · 2 ch");
    t.click(mid(t.control_rect("apply")));
    assert!(has_panel(&t), "ASIO is running now");
    t.pick("system", "WASAPI");
    assert!(has_panel(&t), "the draft alone doesn't close ASIO");
    t.click(mid(t.control_rect("apply")));
    assert!(!has_panel(&t));
}

#[test]
fn last_good_audio_persists_in_isolated_config_dir() {
    // Set environment only on a child process: no unsafe set_var, no parallel-test races, and
    // never the user's real settings directory.
    let dir = std::env::temp_dir().join(format!(
        "transit-audio-good-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "audio_devices_tests::last_good_audio_child", "--nocapture"])
        .env("TRANSIT_CONFIG_DIR", &dir)
        .env("TRANSIT_AUDIO_GOOD_CHILD", "1")
        .env_remove("TRANSIT_NO_PREFS")
        .env_remove("SOUNDCRAFT_NO_PREFS")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
}

#[test]
fn last_good_audio_child() {
    if std::env::var_os("TRANSIT_AUDIO_GOOD_CHILD").is_none() {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var_os("TRANSIT_CONFIG_DIR").unwrap());
    let path = dir.join("ui.json");
    let old = json!({"audio": {"input": "previous good input"}, "show_mixer": true, "future_preference": 7});
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let read = || serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap();
    let mut t = T::new();
    t.app.autosave_dir = Some(dir.join("Autosave"));
    let want = AudioConfig { input: Some(SCARLETT_IN.into()), sample_rate: Some(48_000), ..Default::default() };
    t.app.apply_audio(want.clone()).unwrap();
    assert_eq!(read(), old, "Apply does not persist an untested driver");
    t.frames(180);
    assert_eq!(read(), old, "UI time without callbacks proves nothing");
    fn observe(t: &mut T, at: f64, callbacks: u64, frames: u64, errors: u64) {
        t.time = at;
        let p = t.app.audio_devices.fake.as_mut().unwrap().connection.as_mut().unwrap();
        p.telemetry.callbacks = callbacks;
        p.telemetry.frames = frames;
        p.telemetry.backend_errors = errors;
        t.frames(1);
    }
    observe(&mut t, 10.0, 1, 128, 0);
    observe(&mut t, 11.0, 500, 48_128, 0);
    assert_eq!(read(), old, "one second is not enough");
    observe(&mut t, 12.0, 1000, 96_128, 1);
    observe(&mut t, 13.0, 1500, 144_128, 1);
    assert_eq!(read(), old, "a backend error disqualifies this open");
    t.app.apply_audio(want.clone()).unwrap();
    observe(&mut t, 20.0, 1, 128, 0);
    observe(&mut t, 24.0, 1000, 96_128, 0);
    assert_eq!(read(), old, "a long callback stall restarts verification");
    observe(&mut t, 25.0, 1500, 144_128, 0);
    observe(&mut t, 26.0, 2000, 192_128, 0);
    let saved = read();
    assert_eq!(saved["audio"], serde_json::to_value(want).unwrap());
    assert_eq!(saved["show_mixer"], true);
    assert_eq!(saved["future_preference"], 7);
    t.app.apply_audio(AudioConfig { buffer_size: Some(128), ..Default::default() }).unwrap();
    observe(&mut t, 30.0, 1, 128, 0);
    t.app.audio_devices.fake.as_mut().unwrap().connection.as_mut().unwrap().failed = true;
    observe(&mut t, 31.0, 500, 48_128, 0);
    observe(&mut t, 32.0, 1000, 96_128, 0);
    assert_eq!(read(), saved, "a failed/silent replacement leaves last known good intact");
}

#[test]
fn reorder_keeps_the_editor_wanted_until_the_plan_adopts() {
    let mut t = T::new();
    t.app.add_track("audio").unwrap();
    let track = t.app.engine.session().tracks[0].id;
    for p in ["gain", "saturator", "compressor"] {
        t.app.add_device(track, p).unwrap();
    }
    t.app.want_editor((track, 2));
    t.frames(2);
    assert!(t.app.editors.iter().any(|w| w.opened));
    // The native editor window belongs to the instance, which the player still binds to slot 2
    // until the new plan is adopted (a few frames later).
    t.app.reorder_device(track, 2, 0, false).unwrap();
    assert_eq!(t.app.ui.plugin_windows, [(track, 0)]);
    t.frames(1);
    assert_eq!(t.app.ui.plugin_windows, [(track, 0)], "one frame after the drop the editor must still be wanted");
}

mod io_paths {
    use super::*;
    use soundcraft_model::IoMap;

    fn with_window() -> T {
        let mut t = T::new();
        t.app.engine.set_io_map(IoMap::for_device("ASIO", "TASCAM Model 12", 4, 4));
        t.open_window();
        t
    }

    fn item(t: &T, value: &str, label: &str) -> [f32; 4] {
        T::rect(&t.layout()["controls"], Some(label), value)
    }

    #[test]
    fn the_panel_shows_the_engine_table_and_the_opened_channel_counts() {
        let t = with_window();
        let io = t.layout()["io"].clone();
        assert_eq!((io["inputs"].clone(), io["outputs"].clone(), io["main"].clone()), (json!(4), json!(4), json!("Out 1-2")));
        // Its rows are drawn: Remove is listed for every path.
        for name in ["Out 1-2", "Out 3-4", "In 1-2"] {
            item(&t, &format!("io_remove:{name}"), "Remove");
        }
    }

    #[test]
    fn a_channel_edit_is_exactly_one_setup_io_map_command_without_an_undo_step() {
        let mut t = with_window();
        let (revision, undo, doc) = (t.app.engine.io_revision(), t.app.engine.can_undo(), t.app.engine.revision);
        // Opening the channel picker is not an edit.
        t.click(mid(item(&t, "io_channels:Out 3-4", "Channels")));
        assert_eq!(t.app.engine.io_revision(), revision);
        t.click(mid(item(&t, "io_channel:Out 3-4", "1")));
        assert_eq!(t.app.engine.io_revision(), revision + 1, "one click, one command");
        assert_eq!(t.app.engine.io_map().path("Out 3-4").unwrap().channels, [0, 2, 3]);
        assert_eq!((t.app.engine.can_undo(), t.app.engine.revision), (undo, doc), "no undo entry, no document change");
        // The panel reads the engine's table again: the overlap with Out 1-2 is said.
        assert_eq!(t.layout()["io"]["paths"].as_array().unwrap().len(), t.app.engine.io_map().paths.len());
        assert_eq!(crate::audio_devices_io::overlap_lines(t.app.engine.io_map()).len(), 1);
    }

    #[test]
    fn a_refused_edit_reaches_the_status_line_and_changes_nothing() {
        let mut t = with_window();
        let revision = t.app.engine.io_revision();
        t.click(mid(item(&t, "io_channels:Out 3-4", "Channels")));
        t.click(mid(item(&t, "io_channel:Out 3-4", "3")));
        t.click(mid(item(&t, "io_channel:Out 3-4", "4")));
        assert_eq!(t.app.engine.io_revision(), revision + 1, "the first removal is fine, the last channel is refused");
        assert_eq!(t.app.engine.io_map().path("Out 3-4").unwrap().channels, [3]);
        t.click(mid(item(&t, "io_channel:Out 3-4", "4")));
        assert_eq!(t.app.engine.io_revision(), revision + 1);
        assert!(t.app.status.contains("1 to 16"), "{}", t.app.status);
    }

    #[test]
    fn choosing_main_is_one_command_and_a_wasapi_host_is_noted() {
        let mut t = with_window();
        let revision = t.app.engine.io_revision();
        t.click(mid(item(&t, "io_main", "Out 1-2")));
        t.click(mid(item(&t, "io_main_entry", "Out 3-4")));
        assert_eq!(t.app.engine.io_revision(), revision + 1);
        assert_eq!(t.app.engine.io_map().main, "Out 3-4");
        // The table names ASIO: no WASAPI note.
        assert!(t.layout()["controls"].as_array().unwrap().iter().all(|c| c["value"] != "io_wasapi_note"));
        // A table with no system yet (nothing opened) takes the chosen system, WASAPI here.
        let mut t = T::new();
        t.open_window();
        item(&t, "io_wasapi_note", crate::audio_devices_io::WASAPI_NOTE);
    }
}
