//! Audio › Audio Devices tests. Driven like a user: synthetic clicks through the control channel's
//! input queue, one frame at a time, over stand-in devices (`audio.fake_devices`). No player, no
//! audio device is listed or opened.

use crate::audio_devices::{Opened, PAIRED_AUTOMATIC, REFUSED_RECORDING, SHARE_LABEL, calibration_command, calibration_line, fallback_notes};
use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_playback::{AudioConfig, Backend};

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

fn has_control(t: &T, key: &str) -> bool {
    t.layout()["controls"].as_array().unwrap().iter().any(|c| c["value"] == key)
}

#[test]
fn the_backend_selector_is_offered_only_with_the_feature_on_the_wasapi_system() {
    let mut t = T::new();
    t.open_window();
    let compiled = soundcraft_playback::devices::NATIVE_WASAPI_COMPILED;
    assert_eq!(has_control(&t, "backend"), compiled, "WASAPI is the default system of the fake devices");
    t.pick("system", "ASIO");
    assert_eq!(has_control(&t, "backend"), soundcraft_playback::devices::NATIVE_ASIO_COMPILED, "ASIO has a choice only with native ASIO built");
    t.pick("system", "WASAPI");
    assert_eq!(has_control(&t, "backend"), compiled);
    if compiled {
        t.pick("backend", Backend::NativeWasapi.label());
        assert_eq!(t.app.audio_devices.draft.backend, Backend::NativeWasapi);
        let apply = t.control_rect("apply");
        t.click(mid(apply));
        assert_eq!(t.reopened().last().map(|c| c.backend), Some(Backend::NativeWasapi), "changing the backend reopens the device");
        assert_eq!(t.app.ui.audio.backend, Backend::NativeWasapi);
    }
}

const MODEL12: &str = "Model Mixer ASIO Driver";

/// Fake devices with the ASIO system as the native backend lists it: two drivers by registry name,
/// the Model 12 probed at 12 in / 10 out, and one driver that failed to load.
fn asio_devices() -> Value {
    json!({"systems": [
        {"name": "WASAPI", "is_default": true, "outputs": [{"name": "Speakers (Realtek)", "is_default": true}], "inputs": []},
        {"name": "ASIO",
         "outputs": [{"name": MODEL12, "max_channels": 10, "is_default": true}, {"name": "Focusrite USB ASIO", "max_channels": 2},
                     {"name": "Old Interface ASIO", "max_channels": 2}],
         "inputs": [{"name": MODEL12, "max_channels": 12, "is_default": true}, {"name": "Focusrite USB ASIO", "max_channels": 2}],
         "diagnostics": ["Old Interface ASIO: init failed: No device connected"]}
    ]})
}

#[test]
fn the_asio_system_lists_its_drivers_and_says_which_failed_to_load() {
    let mut t = T::new();
    t.control("audio.fake_devices", asio_devices()).unwrap();
    t.open_window();
    assert!(t.layout()["diagnostics"].as_array().unwrap().is_empty(), "WASAPI has none");
    t.pick("system", "ASIO");
    assert!(t.layout()["diagnostics"].as_array().unwrap().is_empty(), "the selected driver loaded: nothing is said");
    t.pick("output", "Old Interface ASIO · 2 ch");
    assert_eq!(t.layout()["diagnostics"], json!(["Old Interface ASIO: init failed: No device connected"]));
    t.click(mid(t.control_rect("output")));
    let outputs: Vec<String> =
        t.layout()["items"].as_array().unwrap().iter().filter(|i| i["value"] == "output").map(|i| i["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(
        outputs,
        [
            "System default".to_string(),
            format!("{MODEL12} · 10 ch"),
            "Focusrite USB ASIO · 2 ch".to_string(),
            "Old Interface ASIO · 2 ch".to_string()
        ]
    );
}

#[test]
fn native_asio_is_chosen_on_the_asio_system_with_its_calibration_line_and_driver_settings() {
    let mut t = T::new();
    t.control("audio.fake_devices", asio_devices()).unwrap();
    t.open_window();
    t.pick("system", "ASIO");
    if !soundcraft_playback::devices::NATIVE_ASIO_COMPILED {
        assert!(!has_control(&t, "backend"));
        return;
    }
    t.click(mid(t.control_rect("backend")));
    let offered: Vec<String> = t.layout()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["value"] == "backend")
        .map(|i| i["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(offered, ["Auto", "cpal", Backend::NativeAsio.label()], "ASIO offers its own native backend, not WASAPI's");
    t.click(mid(T::rect(&t.layout()["items"], Some(Backend::NativeAsio.label()), "backend")));
    assert_eq!(t.app.audio_devices.draft.backend, Backend::NativeAsio);
    assert!(!has_control(&t, "paired_input") && !has_control(&t, "share_device"), "no pairing and no sharing choice on ASIO");
    let line = t.layout()["calibration"].as_str().unwrap().to_string();
    assert!(line.starts_with("not calibrated \u{b7} asio: the driver's own latency figures"), "{line}");
    assert!(!has_control(&t, "panel"), "nothing runs on ASIO yet");
    t.pick("output", &format!("{MODEL12} · 10 ch"));
    t.click(mid(t.control_rect("apply")));
    let applied = t.reopened().last().cloned().unwrap();
    assert_eq!((applied.backend, applied.system.as_deref(), applied.output.as_deref()), (Backend::NativeAsio, Some("ASIO"), Some(MODEL12)));
    let panel = T::rect(&t.layout()["controls"], Some(crate::audio_devices::DRIVER_SETTINGS), "panel");
    t.click(mid(panel));
    assert_eq!(t.app.audio_devices.panel_opened.as_deref(), Some(MODEL12), "Driver settings opens the running driver's panel");
    // Back on WASAPI the ASIO choice does not carry over.
    t.pick("system", "WASAPI");
    assert_eq!(t.app.audio_devices.draft.backend, Backend::Auto);
}

#[test]
fn while_the_driver_settings_panel_is_open_reconnect_apply_and_rescan_wait_and_nothing_is_listed() {
    if !soundcraft_playback::devices::NATIVE_ASIO_COMPILED {
        return;
    }
    let mut t = T::new();
    t.control("audio.fake_devices", asio_devices()).unwrap();
    t.open_window();
    t.pick("system", "ASIO");
    t.click(mid(t.control_rect("backend")));
    t.click(mid(T::rect(&t.layout()["items"], Some(Backend::NativeAsio.label()), "backend")));
    t.pick("output", &format!("{MODEL12} · 10 ch"));
    t.click(mid(t.control_rect("apply")));
    let panel = T::rect(&t.layout()["controls"], Some(crate::audio_devices::DRIVER_SETTINGS), "panel");
    // The fake opens no panel of its own: hold one open the way the real panel's receiver is.
    let (_open, rx) = std::sync::mpsc::channel();
    t.app.audio_devices.panel = Some(rx);
    let (reopened, listed) = (t.reopened().len(), t.app.audio_devices.fake.as_ref().unwrap().enumerations);
    for key in ["apply", "reconnect", "rescan"] {
        t.click(mid(t.control_rect(key)));
    }
    t.click(mid(panel));
    assert_eq!(t.reopened().len(), reopened, "Apply and Reconnect do not reopen the devices while the panel is open");
    assert_eq!(t.app.audio_devices.fake.as_ref().unwrap().enumerations, listed, "Rescan lists nothing while the panel is open");
    assert!(t.app.audio_devices.panel_opened.is_none(), "Driver settings is disabled while it is open");
    t.app.reconnect_audio_devices();
    assert_eq!(t.reopened().len(), reopened, "the Audio menu's Reconnect waits as well");
    assert_eq!(t.app.status, crate::audio_devices::PANEL_OPEN_FIRST);
    t.app.open_audio_devices();
    t.frames(2);
    assert_eq!(t.app.audio_devices.fake.as_ref().unwrap().enumerations, listed, "reopening the window shows the last list");
    assert!(t.app.audio_devices.systems.is_some(), "the last list is still shown");
    // Closed, they work again.
    t.app.audio_devices.panel = None;
    t.click(mid(t.control_rect("reconnect")));
    assert_eq!(t.reopened().len(), reopened + 1);
}

#[test]
fn the_open_lines_and_the_load_readout_say_what_a_native_stream_reported() {
    use crate::audio_devices::achieved_detail;
    use crate::telemetry::native_host_name;
    assert_eq!(native_host_name("ASIO", true, false), "ASIO*", "native ASIO has one mode");
    assert_eq!(native_host_name("ASIO", false, false), "ASIO", "cpal's ASIO host is unstarred");
    assert_eq!(native_host_name("WASAPI", true, true), "WASAPI* exclusive");
    assert_eq!(native_host_name("WASAPI", true, false), "WASAPI* shared");
    // The sample format as the backend names it (`SampleFormat::I24`), lowercased.
    #[derive(Debug)]
    enum Sample {
        I24,
    }
    let detail = achieved_detail(Some(Sample::I24), Some(std::time::Duration::from_micros(15_760)), "out");
    assert_eq!(detail, " · i24 · 15.8 ms out");
    assert_eq!(achieved_detail(None::<Sample>, None, "in"), "");
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
            input_pairing: None,
            backend: Backend::Auto,
            calibrations: Vec::new(),
            share_device: true,
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
    // Hosted plug-ins: they have native editor windows, built-ins have ours (`builtin_editor`).
    for (slot, id) in ["clap:test.a", "clap:test.b", "clap:test.c"].into_iter().enumerate() {
        t.app.engine.session_mut().track_mut(track).unwrap().mixer.inserts[slot] = Some(soundcraft_model::Insert::new(id));
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

    pub(super) fn with_window() -> T {
        let mut t = T::new();
        t.app.engine.set_io_map(IoMap::for_device("ASIO", "TASCAM Model 12", 4, 4));
        t.open_window();
        t
    }

    pub(super) fn item(t: &T, value: &str, label: &str) -> [f32; 4] {
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
    fn the_channel_picker_lists_every_channel_of_a_12_in_10_out_driver_by_its_name() {
        use crate::audio_devices_io::{ChannelNames, channel_text};
        let mut t = T::new();
        t.app.engine.set_io_map(IoMap::for_device("ASIO", "Model Mixer ASIO Driver", 12, 10));
        let names = |prefix: &str, n: u16| (1..=n).map(|c| format!("{prefix} {c}")).collect();
        t.app.audio_devices.fake_channel_names = Some(ChannelNames { outputs: names("OUT", 10), inputs: names("IN", 12) });
        t.open_window();
        assert_eq!((t.layout()["io"]["inputs"].clone(), t.layout()["io"]["outputs"].clone()), (json!(12), json!(10)));
        t.click(mid(item(&t, "io_channels:In 1", "Channels")));
        let picks: Vec<String> = t.layout()["controls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["value"] == "io_channel:In 1")
            .map(|c| c["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(picks, (1..=12).map(|c| format!("IN {c}")).collect::<Vec<_>>(), "every input, by the driver's name");
        t.click(mid(item(&t, "io_channels:Out 1-2", "Channels")));
        t.click(mid(item(&t, "io_channel:Out 1-2", "OUT 8")));
        assert_eq!(t.app.engine.io_map().path("Out 1-2").unwrap().channels, [0, 1, 7], "a named channel is a channel like any other");
        assert_eq!(channel_text(0, &["Mic/Line".into()]), "1 Mic/Line");
        assert_eq!(channel_text(2, &[]), "3", "unnamed channels keep their number");
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

fn calibration(device: &str, input: Option<&str>, rate: u32, period: u32) -> soundcraft_playback::LoopbackCalibration {
    soundcraft_playback::LoopbackCalibration {
        device: device.into(),
        input: input.map(str::to_string),
        mode: "shared".into(),
        rate,
        period,
        round_trip_frames: 480,
        spread_frames: 25,
        measured_at: "2026-10-21".into(),
    }
}

#[test]
fn the_calibration_line_reports_the_measured_pair_or_the_command_that_measures_it() {
    use crate::audio_devices::calibration_line;
    let out = "Speakers (Focusrite USB Audio)";
    let saved = AudioConfig {
        input_pairing: Some(SCARLETT_IN.into()),
        buffer_size: Some(128),
        calibrations: vec![calibration(out, Some(SCARLETT_IN), 48_000, 128)],
        share_device: true,
        ..Default::default()
    };
    assert_eq!(calibration_line(&saved, out, None, 48_000, "ui.json"), "measured 480 frames \u{b1}25 (2026-10-21) \u{b7} shared");
    let unmeasured = calibration_line(&saved, out, None, 96_000, "C:/Users/me/transit/ui.json");
    assert_eq!(
        unmeasured,
        format!(
            "not calibrated \u{b7} shared: run audio-bench --backend wasapi --loopback --out-device \"{out}\" --in-device \"{SCARLETT_IN}\" --period 128 \
             --rate 96000 --settings \"C:/Users/me/transit/ui.json\" (with this app closed)"
        )
    );
    // Exclusive is the default: its calibration is another key, and its command measures exclusive.
    let exclusive = AudioConfig { share_device: false, ..saved.clone() };
    let line = calibration_line(&exclusive, out, None, 48_000, "ui.json");
    assert!(line.starts_with("not calibrated \u{b7} exclusive: run audio-bench --backend wasapi --loopback"), "{line}");
    assert!(line.contains("--mode exclusive "), "{line}");
    // Paired input on Automatic: a record that named the input the pairing resolved to applies.
    let automatic = AudioConfig { input_pairing: None, ..saved.clone() };
    assert!(calibration_line(&automatic, out, None, 48_000, "ui.json").starts_with("not calibrated"));
    assert!(calibration_line(&automatic, out, Some("Line (Other)"), 48_000, "ui.json").starts_with("not calibrated"));
    assert_eq!(
        calibration_line(&automatic, out, Some(&SCARLETT_IN.to_uppercase()), 48_000, "ui.json"),
        "measured 480 frames \u{b1}25 (2026-10-21) \u{b7} shared"
    );
    let measured = soundcraft_playback::LoopbackCalibration {
        mode: "exclusive".into(),
        round_trip_frames: 960,
        ..calibration(out, Some(SCARLETT_IN), 48_000, 128)
    };
    let both = AudioConfig { calibrations: vec![calibration(out, Some(SCARLETT_IN), 48_000, 128), measured], ..exclusive };
    assert_eq!(calibration_line(&both, out, None, 48_000, "ui.json"), "measured 960 frames \u{b1}25 (2026-10-21) \u{b7} exclusive");
    let other_period = AudioConfig { buffer_size: Some(64), share_device: true, ..both };
    assert!(
        calibration_line(&other_period, out, None, 48_000, "ui.json").starts_with("not calibrated \u{b7} shared: run "),
        "a calibration is for one buffer size"
    );
}

#[test]
fn the_paired_input_and_calibration_line_exist_only_under_the_native_backend() {
    let mut t = T::new();
    t.open_window();
    let calibration_shown = |t: &T| !t.layout()["calibration"].is_null();
    assert!(!has_control(&t, "paired_input") && !calibration_shown(&t), "the default backend has no pairing");
    if !soundcraft_playback::devices::NATIVE_WASAPI_COMPILED {
        return;
    }
    t.pick("backend", Backend::NativeWasapi.label());
    assert!(has_control(&t, "paired_input") && calibration_shown(&t), "the native backend pairs an input");
    let line = t.layout()["calibration"].as_str().unwrap().to_string();
    assert!(
        line.starts_with("not calibrated \u{b7} shared: run audio-bench --backend wasapi --loopback --out-device \"Speakers (Realtek)\""),
        "{line}"
    );
    t.pick("paired_input", &format!("{SCARLETT_IN} \u{b7} 2 ch"));
    assert_eq!(t.app.audio_devices.draft.input_pairing.as_deref(), Some(SCARLETT_IN));
    assert!(t.layout()["calibration"].as_str().unwrap().contains(&format!("--in-device \"{SCARLETT_IN}\"")));
    let apply = t.control_rect("apply");
    t.click(mid(apply));
    assert_eq!(t.reopened().last().and_then(|c| c.input_pairing.clone()).as_deref(), Some(SCARLETT_IN), "the pairing is applied and saved");
    t.pick("backend", Backend::Auto.label());
    assert!(!has_control(&t, "paired_input") && !calibration_shown(&t), "back to the default backend");
    t.pick("backend", Backend::NativeWasapi.label());
    t.pick("paired_input", PAIRED_AUTOMATIC);
    assert_eq!(t.app.audio_devices.draft.input_pairing, None);
}

#[test]
fn the_share_checkbox_follows_the_native_backend_and_applies_with_the_devices() {
    let mut t = T::new();
    t.open_window();
    assert!(!has_control(&t, "share_device"), "the default backend has no sharing choice");
    if !soundcraft_playback::devices::NATIVE_WASAPI_COMPILED {
        return;
    }
    t.pick("backend", Backend::NativeWasapi.label());
    assert!(has_control(&t, "share_device"), "the native backend offers it");
    assert_eq!(SHARE_LABEL, "Take exclusive control of the device");
    assert!(t.app.audio_devices.draft.share_device, "unchecked: shared is the default");
    assert!(t.layout()["calibration"].as_str().unwrap().contains("shared"), "the Calibration line names the mode");
    t.click(mid(t.control_rect("share_device")));
    assert!(!t.app.audio_devices.draft.share_device, "checking it asks for exclusive control");
    assert!(t.layout()["calibration"].as_str().unwrap().contains("exclusive"));
    assert!(t.app.ui.audio.share_device, "nothing is applied until Apply");
    t.click(mid(t.control_rect("apply")));
    assert_eq!(t.reopened().last().map(|c| c.share_device), Some(false), "Apply reopens the devices exclusive");
    assert!(!t.app.ui.audio.share_device);
    t.click(mid(t.control_rect("share_device")));
    assert!(t.app.audio_devices.draft.share_device);
    t.click(mid(t.control_rect("apply")));
    assert_eq!(t.reopened().last().map(|c| c.share_device), Some(true));
    t.pick("backend", Backend::Auto.label());
    assert!(!has_control(&t, "share_device"), "back to the default backend");
}

#[test]
fn a_saved_calibration_is_shown_for_its_pair_and_survives_apply() {
    if !soundcraft_playback::devices::NATIVE_WASAPI_COMPILED {
        return;
    }
    let mut t = T::new();
    let saved = AudioConfig {
        backend: Backend::NativeWasapi,
        system: Some("WASAPI".into()),
        output: Some("Speakers (Focusrite USB Audio)".into()),
        sample_rate: Some(48_000),
        calibrations: vec![calibration("Speakers (Focusrite USB Audio)", None, 48_000, 0)],
        share_device: true,
        ..Default::default()
    };
    t.app.apply_audio(saved).unwrap();
    t.open_window();
    assert_eq!(t.layout()["calibration"], json!("measured 480 frames \u{b1}25 (2026-10-21) \u{b7} shared"));
    t.pick("buffer", "128 samples");
    assert!(t.layout()["calibration"].as_str().unwrap().starts_with("not calibrated"), "another buffer size is another key");
    let apply = t.control_rect("apply");
    t.click(mid(apply));
    assert_eq!(t.app.ui.audio.calibrations.len(), 1, "Apply keeps the saved measurements");
}

mod hardware_inserts {
    use super::io_paths::{item, with_window};
    use super::*;
    use serde_json::json;

    fn measured() -> T {
        let mut t = with_window();
        t.app.engine.store_hardware_latency("Out 3-4", "In 3-4", 612);
        t.frames(3);
        t
    }

    #[test]
    fn the_measured_table_lists_the_pair_with_its_latency_and_clear_forgets_it() {
        let mut t = measured();
        let h = t.layout()["hardware"].clone();
        assert_eq!(h["rows"], json!(["Out 3-4 → In 3-4: 612 frames (12.8 ms)"]));
        let revision = t.app.engine.io_revision();
        t.click(mid(item(&t, "hw_clear:Out 3-4 → In 3-4", "Clear")));
        assert!(t.app.engine.io_map().hardware_latency.is_empty());
        assert_eq!(t.app.engine.io_revision(), revision + 1);
        assert_eq!(t.layout()["hardware"]["rows"], json!([]));
    }

    #[test]
    fn measure_is_off_without_a_live_duplex_device_and_says_so() {
        let mut t = measured();
        let h = t.layout()["hardware"].clone();
        assert_eq!(h["blocked"], "Measuring needs an open audio device.");
        item(&t, "hw_measure", "Measure");
        item(&t, "hw_remeasure:Out 3-4 → In 3-4", "Remeasure");
        // Choose a pair: the button is still off, and a click changes nothing.
        t.click(mid(item(&t, "hw_send", "")));
        t.click(mid(item(&t, "hw_send_entry", "Out 3-4")));
        t.click(mid(item(&t, "hw_ret", "")));
        t.click(mid(item(&t, "hw_ret_entry", "In 3-4")));
        let h = t.layout()["hardware"].clone();
        assert_eq!((h["send"].clone(), h["ret"].clone()), (json!("Out 3-4"), json!("In 3-4")));
        let revision = t.app.engine.io_revision();
        t.click(mid(item(&t, "hw_measure", "Measure")));
        assert_eq!(t.app.engine.io_revision(), revision);
        assert_eq!(t.app.engine.io_map().hardware_latency.len(), 1);
    }

    #[test]
    fn the_command_without_a_device_says_why_in_the_status_line() {
        let mut t = measured();
        let why = t.app.run("io.measure_hardware_latency", json!({"send": "Out 3-4", "ret": "In 3-4"})).unwrap_err();
        assert_eq!(why, "measuring needs a live audio device");
        assert_eq!(t.app.status, why);
        let why = t.app.run("io.measure_hardware_latency", json!({"send": "Nowhere", "ret": "In 3-4"})).unwrap_err();
        assert!(why.contains("Nowhere"), "{why}");
    }

    use crate::hardware_measure::FakeProbe;
    use soundcraft_playback::hardware_probe::{Measured, ProbeError};

    fn with_fake_probe() -> T {
        let mut t = with_window();
        t.app.audio_devices.fake_probe = Some(FakeProbe { progress: (0, 8), ..Default::default() });
        t.frames(3);
        t
    }

    fn pick_pair(t: &mut T) {
        t.click(mid(item(t, "hw_send", "")));
        t.click(mid(item(t, "hw_send_entry", "Out 3-4")));
        t.click(mid(item(t, "hw_ret", "")));
        t.click(mid(item(t, "hw_ret_entry", "In 3-4")));
    }

    fn probe(t: &mut T) -> &mut FakeProbe {
        t.app.audio_devices.fake_probe.as_mut().unwrap()
    }

    fn done(frames: u32, spread: u32) -> Result<Measured, ProbeError> {
        Ok(Measured { round_trip_frames: frames, spread_frames: spread, found: 8, sent: 8 })
    }

    #[test]
    fn measure_runs_without_blocking_shows_the_clicks_and_stores_the_result_when_it_completes() {
        let mut t = with_fake_probe();
        pick_pair(&mut t);
        assert_eq!(t.layout()["hardware"]["blocked"], "");
        t.click(mid(item(&t, "hw_measure", "Measure")));
        assert_eq!(probe(&mut t).started, [(vec![2, 3], vec![2, 3])]);
        assert_eq!(t.layout()["hardware"]["progress"], "Measuring\u{2026} 0/8 clicks");
        assert!(t.app.engine.io_map().hardware_latency.is_empty(), "nothing is stored while it runs");
        probe(&mut t).progress = (5, 8);
        t.frames(3);
        assert_eq!(t.layout()["hardware"]["progress"], "Measuring\u{2026} 5/8 clicks");
        item(&t, "hw_progress", "Measuring\u{2026} 5/8 clicks");
        item(&t, "hw_cancel", "Cancel");
        // Measure is off while it runs: a click starts nothing more.
        t.click(mid(item(&t, "hw_measure", "Measure")));
        assert_eq!(probe(&mut t).started.len(), 1);
        probe(&mut t).result = Some(done(612, 2));
        t.frames(3);
        assert_eq!(t.layout()["hardware"]["progress"], "");
        assert_eq!(t.layout()["hardware"]["result"], "612 frames (12.8 ms) \u{b1}2");
        assert_eq!(t.layout()["hardware"]["rows"], json!(["Out 3-4 \u{2192} In 3-4: 612 frames (12.8 ms)"]));
        assert_eq!(t.app.engine.io_map().hardware_latency.len(), 1);
        assert!(t.app.status.contains("612 frames"), "{}", t.app.status);
    }

    #[test]
    fn a_failed_measurement_says_why_and_stores_nothing() {
        let mut t = with_fake_probe();
        pick_pair(&mut t);
        t.click(mid(item(&t, "hw_measure", "Measure")));
        probe(&mut t).result = Some(Err(ProbeError::NoSignal { found: 1, sent: 8 }));
        t.frames(3);
        assert_eq!(t.layout()["hardware"]["result"], "no signal returned on In 3-4: check the cable and input gain");
        assert!(t.app.engine.io_map().hardware_latency.is_empty());
        assert_eq!(t.layout()["hardware"]["progress"], "");
    }

    #[test]
    fn cancel_stops_the_measurement_and_leaves_no_entry() {
        let mut t = with_fake_probe();
        pick_pair(&mut t);
        t.click(mid(item(&t, "hw_measure", "Measure")));
        t.click(mid(item(&t, "hw_cancel", "Cancel")));
        assert_eq!(probe(&mut t).cancelled, 1);
        t.frames(3);
        let h = t.layout()["hardware"].clone();
        assert_eq!((h["progress"].clone(), h["result"].clone()), (json!(""), json!("Measurement cancelled.")));
        // A result arriving after the cancel is not collected.
        probe(&mut t).result = Some(done(612, 0));
        t.frames(3);
        assert!(t.app.engine.io_map().hardware_latency.is_empty());
        // Measure is available again.
        t.click(mid(item(&t, "hw_measure", "Measure")));
        assert_eq!(probe(&mut t).started.len(), 2);
    }

    #[test]
    fn closing_the_dialog_cancels_the_measurement() {
        let mut t = with_fake_probe();
        pick_pair(&mut t);
        t.click(mid(item(&t, "hw_measure", "Measure")));
        t.app.audio_devices.open = false;
        t.frames(3);
        assert_eq!(probe(&mut t).cancelled, 1);
        assert!(t.app.audio_devices.measuring.is_none());
        probe(&mut t).result = Some(done(612, 0));
        t.frames(3);
        assert!(t.app.engine.io_map().hardware_latency.is_empty());
    }

    #[test]
    fn the_command_path_still_answers_once_when_done_and_stores_the_same() {
        let mut t = with_fake_probe();
        probe(&mut t).result = Some(done(612, 2));
        let v = t.app.run("io.measure_hardware_latency", json!({"send": "Out 3-4", "ret": "In 3-4"})).unwrap();
        assert_eq!(v["round_trip_frames"], 612);
        assert_eq!(t.app.engine.io_map().hardware_latency.len(), 1);
        assert!(t.app.status.contains("612 frames"), "{}", t.app.status);
    }

    #[test]
    fn transport_starts_are_refused_while_a_measurement_runs() {
        let mut t = with_fake_probe();
        pick_pair(&mut t);
        t.click(mid(item(&t, "hw_measure", "Measure")));
        for id in ["transport.play", "transport.toggle", "transport.record", "transport.half_speed", "transport.play_selection", "edit.play_timeline"]
        {
            t.app.status.clear();
            let why = t.app.run(id, json!({})).unwrap_err();
            assert_eq!(why, "Stop the hardware insert measurement first", "{id}");
            assert_eq!(t.app.status, why, "{id}");
        }
        t.frames(3);
        assert!(!t.app.is_playing());
        assert!(t.app.engine.transport_requests.is_empty());
        // Once it is done, Play works again.
        probe(&mut t).result = Some(done(612, 0));
        t.frames(3);
        assert!(t.app.run("transport.play", json!({})).is_ok());
    }

    #[test]
    fn reopening_the_device_cancels_a_pending_measurement_at_once() {
        let mut t = with_fake_probe();
        pick_pair(&mut t);
        t.click(mid(item(&t, "hw_measure", "Measure")));
        assert!(t.app.audio_devices.measuring.is_some());
        t.app.reopen_audio().unwrap();
        assert!(t.app.audio_devices.measuring.is_none());
        assert_eq!(probe(&mut t).cancelled, 1);
        t.frames(3);
        assert_eq!(t.layout()["hardware"]["progress"], "");
        assert_eq!(t.layout()["hardware"]["result"], "Measurement cancelled: the audio device was re-opened");
    }
}

/// Native ASIO opens its input with the output and the player holds it until a track takes it:
/// the window reports it open, with its own Backend line, and a hardware insert can be measured.
#[test]
fn the_held_native_asio_input_shows_as_open_and_measure_is_enabled_on_it() {
    use transit_audio_backend as ab;
    let names = |prefix: &str, n: u16| (1..=n).map(|c| format!("{prefix} {c}")).collect();
    let driver = ab::fake::FakeBackend::new(ab::Format::new(48_000, 10, ab::SampleFormat::I24))
        .named("ASIO", "Model Mixer ASIO Driver")
        .with_capture(12, true)
        .with_channel_names(names("Analog In", 12), names("Analog Out", 10))
        .with_latency(std::time::Duration::from_micros(6_800));
    let cfg = AudioConfig { backend: Backend::NativeAsio, system: Some("ASIO".into()), ..Default::default() };
    let mut t = T::new();
    let player = soundcraft_playback::Player::open_fake_native_asio(t.app.engine.session_arc(), &cfg, &driver).unwrap();
    t.app.engine.set_io_map(player.io_map().as_ref().clone());
    t.app.player = Some(player);
    assert!(t.app.recorder.is_none(), "no track is armed: the recorder has not opened");
    t.open_window();
    let lines: Vec<String> = t.layout()["opened"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[2], "Input open: Model Mixer ASIO Driver · 48000 Hz · 12 ch · f32 · 6.8 ms in", "{lines:?}");
    assert!(lines[3].starts_with("Backend: ") && lines[3].ends_with("max packets 0"), "the held input has its own Backend line: {lines:?}");
    assert_eq!(t.app.measure_blocker(), None, "a held duplex input is a live input");
    assert_eq!(t.layout()["hardware"]["blocked"], "");
    T::rect(&t.layout()["controls"], Some("Measure"), "hw_measure");
}

/// A native ASIO Player over the scripted duplex `driver` as the running device, with the window's
/// stand-in devices gone (a reopen goes through the real reopen path) and every automatic reopen
/// opening `reopen` (no device is listed or opened).
fn asio_rig(
    driver: &transit_audio_backend::fake::FakeBackend,
    reopen: impl Fn(std::sync::Arc<soundcraft_model::Session>, &AudioConfig) -> soundcraft_playback::Player + 'static,
) -> T {
    let cfg =
        AudioConfig { backend: Backend::NativeAsio, system: Some("ASIO".into()), output: Some("Focusrite USB ASIO".into()), ..Default::default() };
    let mut t = T::new();
    let player = soundcraft_playback::Player::open_fake_native_asio(t.app.engine.session_arc(), &cfg, driver).unwrap();
    t.app.player = Some(player);
    t.app.ui.audio = cfg;
    t.app.audio_devices.fake = None;
    t.app.reopen_with = Some(Box::new(reopen));
    t
}

/// Delivers `error` to `direction`'s error callback from a thread of its own, as a driver does (the
/// callback marks its thread as an audio thread, whose panics the engine's hook keeps quiet).
fn inject(
    driver: &transit_audio_backend::fake::FakeBackend,
    direction: transit_audio_backend::Direction,
    error: transit_audio_backend::StreamError,
) -> bool {
    std::thread::scope(|s| s.spawn(|| driver.inject_error(direction, error)).join().unwrap())
}

/// Runs `n` frames with the audio side running too: a block of the scripted driver per frame (on a
/// thread of its own, as a driver calls) while the device runs, else a block of the device-free
/// player a failed reopen left (the app's silent clock).
fn run(t: &mut T, driver: &transit_audio_backend::fake::FakeBackend, n: usize) {
    use transit_audio_backend as ab;
    for _ in 0..n {
        match &t.app.player {
            Some(p) if p.silent => p.step_device_free(),
            Some(p) if p.reopen_cause().is_none() => std::thread::scope(|s| {
                s.spawn(|| {
                    let mut data = [0.0f32; 512];
                    driver.pump_render(256, ab::Stamp::INVALID, &mut ab::fake::SliceSink { data: &mut data, channels: 2 });
                });
            }),
            _ => {}
        }
        t.frames(1);
    }
}

fn scarlett_driver() -> transit_audio_backend::fake::FakeBackend {
    use transit_audio_backend as ab;
    ab::fake::FakeBackend::new(ab::Format::new(48_000, 2, ab::SampleFormat::I32)).named("ASIO", "Focusrite USB ASIO").with_capture(2, true)
}

/// The driver asks for a reset (its buffer size changed in its own panel): output and input are
/// reopened with the same choices without user action, once the driver has settled.
#[test]
fn a_driver_reset_reopens_the_native_asio_output_and_input_without_user_action() {
    use transit_audio_backend as ab;
    let driver = scarlett_driver();
    let again = driver.clone();
    let mut t = asio_rig(&driver, move |session, cfg| soundcraft_playback::Player::open_fake_native_asio(session, cfg, &again).unwrap());
    let opens = driver.opens().len();
    assert_eq!(opens, 2, "input and output");
    t.app.add_track("audio").unwrap();
    let track = t.app.engine.session().tracks[0].id;
    t.app.engine.session_mut().track_mut(track).unwrap().mixer.input_monitor = true;
    run(&mut t, &driver, 10);
    assert!(t.app.recorder.is_some(), "a monitored track took the input");
    assert!(inject(&driver, ab::Direction::Output, ab::StreamError::ResetRequested));
    assert!(inject(&driver, ab::Direction::Input, ab::StreamError::ResetRequested));
    run(&mut t, &driver, 1);
    assert_eq!(t.app.status, "Driver reset. Reopening the device.");
    assert!(t.app.reopen.pending());
    assert_eq!(driver.opens().len(), opens, "not before the driver settles");
    run(&mut t, &driver, 20);
    let reopened: Vec<ab::Direction> = driver.opens()[opens..].iter().map(|(d, _)| *d).collect();
    assert_eq!(reopened, [ab::Direction::Input, ab::Direction::Output], "both reopened, the input first (ASIO's open order)");
    assert!(driver.running(ab::Direction::Output) && driver.running(ab::Direction::Input));
    let p = t.app.player.as_ref().unwrap();
    assert!(!p.silent && p.reopen_cause().is_none(), "the new player runs");
    assert!(t.app.recorder.as_mut().is_some_and(|r| r.trouble().is_none()), "and monitoring has the reopened input again");
    assert_eq!(t.app.status, "“Focusrite USB ASIO” reopened after the driver reset.");
    assert_eq!(t.app.reopen.status(false).as_deref(), Some("Recovery: driver reset, reopened automatically (resets 1, stalls 0, reopened 1)"));
    run(&mut t, &driver, 60);
    assert_eq!(driver.opens().len(), opens + 2, "once");
}

/// Delivers `blocks` capture blocks of 256 frames, every sample `value`, from a thread of its own.
fn pump_capture(driver: &transit_audio_backend::fake::FakeBackend, blocks: usize, value: f32) {
    use transit_audio_backend as ab;
    std::thread::scope(|s| {
        s.spawn(|| {
            let data = [value; 512];
            for _ in 0..blocks {
                let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
                assert!(driver.pump_capture(256, ab::Stamp::INVALID, format, &mut ab::fake::SliceSource { data: &data, channels: 2 }));
            }
        });
    });
}

/// A reset while recording ends the take there and keeps it (it is never spliced onto the reopened
/// stream), then the device reopens. The audio take holds what was captured up to the request and
/// nothing the driver delivered after it.
#[test]
fn a_driver_reset_while_recording_ends_the_take_keeps_it_and_then_reopens() {
    use soundcraft_playback::record::start::Point;
    use soundcraft_playback::timeline_map::Position;
    use transit_audio_backend as ab;
    let driver = scarlett_driver();
    let again = driver.clone();
    let mut t = asio_rig(&driver, move |session, cfg| soundcraft_playback::Player::open_fake_native_asio(session, cfg, &again).unwrap());
    let track = soundcraft_model::TrackId(t.app.run("track.new", json!({"kind": "midi"})).unwrap()["tracks"][0].as_u64().unwrap());
    t.app.add_track("audio").unwrap();
    let audio = t.app.engine.session().tracks.iter().find(|tr| tr.id != track).unwrap().id;
    t.app.engine.session_mut().track_mut(audio).unwrap().mixer.input_monitor = true;
    run(&mut t, &driver, 10);
    let dir = std::env::temp_dir().join(format!("transit-d1-ui-take-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    t.app.recorder.as_mut().expect("the monitored track took the input").arm(vec![(audio, vec![0], dir.join("take.wav"))]).unwrap();
    pump_capture(&driver, 4, 0.25);
    t.app.take_plan = Some(soundcraft_engine::io::Recording {
        start: 0,
        pass: None,
        punch: None,
        rate: 48_000,
        takes: Vec::new(),
        midi_journal: None,
        midi_placed: false,
    });
    t.app.engine.transport.recording = true;
    let note = |frame, message| soundcraft_playback::midi_capture::CapturedEvent { frame, track, channel: 0, message, approximate: false };
    let events = vec![
        note(1_000, soundcraft_mix::Message::NoteOn { key: 60, velocity: 100, id: soundcraft_mix::NoteId::NONE }),
        note(9_000, soundcraft_mix::Message::NoteOff { key: 60, velocity: 0, id: soundcraft_mix::NoteId::NONE }),
    ];
    let start = Point { host_ns: 0, output_frame: 0, position: Position { sample: 0, fraction: 0, denominator: 1 } };
    t.app.finished_test_midi =
        Some(soundcraft_playback::MidiTake { tracks: vec![track], start: Some(start), events, dropped: 0, device_rate: 48_000 });
    assert!(inject(&driver, ab::Direction::Input, ab::StreamError::ResetRequested));
    assert!(inject(&driver, ab::Direction::Output, ab::StreamError::ResetRequested));
    // The driver goes on calling back until the app stops it at its next frame.
    pump_capture(&driver, 4, 0.75);
    run(&mut t, &driver, 1);
    assert_eq!(t.app.status, "Recording ended: driver reset. The take is kept up to there. Reopening the device.");
    assert_eq!(t.app.engine.session().track(track).unwrap().clips().len(), 1, "the take is kept");
    let audio_clips = t.app.engine.session().track(audio).unwrap().clips().to_vec();
    assert_eq!(audio_clips.len(), 1, "the audio take is kept");
    assert_eq!(audio_clips[0].length, 1024, "up to the request: none of the blocks after it");
    run(&mut t, &driver, 30);
    assert!(!t.app.recording_active());
    assert_eq!(driver.opens().len(), 4, "then the device reopened");
    assert_eq!(t.app.engine.session().track(track).unwrap().clips().len(), 1, "and nothing was spliced onto the take");
    assert_eq!(t.app.reopen.reopened(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A driver the backend's watchdog found had stopped calling back takes the same reopen path,
/// counted apart from resets.
#[test]
fn a_driver_that_stopped_calling_back_is_reopened_the_same_way() {
    use transit_audio_backend as ab;
    let driver = scarlett_driver();
    let again = driver.clone();
    let mut t = asio_rig(&driver, move |session, cfg| soundcraft_playback::Player::open_fake_native_asio(session, cfg, &again).unwrap());
    run(&mut t, &driver, 10);
    assert!(inject(&driver, ab::Direction::Output, ab::StreamError::Stalled));
    assert!(inject(&driver, ab::Direction::Input, ab::StreamError::Stalled));
    run(&mut t, &driver, 1);
    assert_eq!(t.app.status, "Driver stopped calling back. Reopening the device.");
    run(&mut t, &driver, 20);
    assert_eq!(driver.opens().len(), 4, "input and output reopened");
    assert_eq!(t.app.status, "“Focusrite USB ASIO” reopened after the driver stopped calling back.");
    assert_eq!((t.app.reopen.resets(), t.app.reopen.stalls(), t.app.reopen.reopened()), (0, 1, 1));
}

/// While the driver's settings panel holds the driver's host thread, the reopen waits for it (a
/// reopen would be refused there), then runs.
#[test]
fn a_driver_reset_waits_for_the_settings_panel_to_close() {
    use transit_audio_backend as ab;
    let driver = scarlett_driver();
    let again = driver.clone();
    let mut t = asio_rig(&driver, move |session, cfg| soundcraft_playback::Player::open_fake_native_asio(session, cfg, &again).unwrap());
    driver.set_host_blocked(true);
    assert!(inject(&driver, ab::Direction::Output, ab::StreamError::ResetRequested));
    run(&mut t, &driver, 120);
    assert_eq!(driver.opens().len(), 2, "nothing reopened while the panel is open");
    assert_eq!(t.app.reopen.attempt(), Some(1), "and no attempt used up");
    assert!(t.app.reopen.status(true).unwrap().contains("waiting for the driver settings panel to close"));
    driver.set_host_blocked(false);
    run(&mut t, &driver, 2);
    assert_eq!(driver.opens().len(), 4);
    assert_eq!(t.app.reopen.reopened(), 1);
}

/// A device that does not come back is tried three times, with growing waits, then left to the
/// user, who is told to reconnect.
#[test]
fn a_device_that_does_not_reopen_is_tried_three_times_then_left_to_reconnect() {
    use transit_audio_backend as ab;
    let driver = scarlett_driver();
    let tries = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = std::rc::Rc::clone(&tries);
    let mut t = asio_rig(&driver, move |session, _| {
        counted.set(counted.get() + 1);
        soundcraft_playback::Player::prepare_device_free(session).unwrap()
    });
    run(&mut t, &driver, 10);
    assert!(inject(&driver, ab::Direction::Output, ab::StreamError::ResetRequested));
    run(&mut t, &driver, 20);
    assert_eq!(tries.get(), 1);
    assert!(t.app.status.contains("trying again"), "{}", t.app.status);
    run(&mut t, &driver, 60);
    assert_eq!(tries.get(), 2, "one second later");
    run(&mut t, &driver, 170);
    assert_eq!(tries.get(), 2, "the third waits three seconds");
    run(&mut t, &driver, 20);
    assert_eq!(tries.get(), 3);
    assert!(t.app.reopen.gave_up());
    assert!(t.app.status.ends_with("after 3 attempts. Use Audio › Reconnect Device."), "{}", t.app.status);
    run(&mut t, &driver, 300);
    assert_eq!(tries.get(), 3, "no more");
    // Play then is refused with the advice, not ignored; the Recovery line keeps it too.
    t.app.start_play(0, None, None);
    assert_eq!(t.app.status, "The audio device did not reopen after the driver reset. Use Audio › Reconnect Device.");
    assert!(!t.app.is_playing());
    assert!(t.app.reopen.status(false).unwrap().ends_with("use Reconnect (resets 1, stalls 0, reopened 0)"));
}

#[test]
fn the_open_lines_end_with_the_workers_mmcss_state() {
    use crate::audio_devices::priority_detail;
    use soundcraft_playback::scheduling::WorkerPriority as P;
    assert_eq!(priority_detail(P::MmcssProAudio), " · MMCSS Pro Audio");
    assert_eq!(priority_detail(P::Pending), " · MMCSS pending");
    assert_eq!(priority_detail(P::PromotionFailed), " · MMCSS refused");
    assert_eq!(priority_detail(P::Released), " · MMCSS released");
    assert_eq!(priority_detail(P::DriverRegistered), " · MMCSS by driver", "the ASIO driver registered its own thread (error 1552)");
    assert_eq!(priority_detail(P::Unobserved), "", "nothing where the backend observes none (a scripted driver, the host thread)");
    assert_eq!(priority_detail(P::NoWorker), "");
}

#[test]
fn the_window_shows_the_backend_loop_lines_under_the_open_lines_from_a_scripted_snapshot() {
    use soundcraft_playback::telemetry::StreamStats;
    let mut hist = [0u64; 16];
    hist[0] = 90;
    hist[3] = 10; // 10 wakes of 64..127 us
    let output = StreamStats {
        wakes: 1200,
        skipped_wakes: 2,
        wake_late_us: hist,
        max_wake_late_us: 90,
        service_us: hist,
        max_service_us: 100,
        min_padding_frames: 96,
        ..Default::default()
    };
    let input = StreamStats { wakes: 800, timeouts: 1, max_packets_per_wake: 3, max_wake_late_us: 5, wake_late_us: hist, ..Default::default() };
    let mut t = T::new();
    t.app.audio_devices.injected_backend = Some((Some(output), Some(input)));
    t.open_window();
    let lines: Vec<String> = t.layout()["opened"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].starts_with("Output open"));
    assert_eq!(lines[1], "Backend: wakes -, late p95 90 µs, max 90 µs; service p95 100 µs, max 100 µs; skipped 2, timeouts 0; min padding 96");
    assert!(lines[2].starts_with("Input open"));
    assert_eq!(
        lines[3], "Backend: wakes -, late p95 5 µs, max 5 µs; service p95 0 µs, max 0 µs; skipped 0, timeouts 1; max packets 3",
        "the capture line ends with the most packets of a wake"
    );
    // Two samples a second apart give a rate, not a raw counter.
    t.time += 1.0;
    let (mut output, mut input) = (output, input);
    output.wakes += 100;
    input.wakes += 99;
    t.app.audio_devices.injected_backend = Some((Some(output), Some(input)));
    t.frames(1);
    let lines: Vec<String> = t.layout()["opened"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect();
    // The first sample was taken at the window's first frame, so the span is a little over a second.
    assert!(lines[1].contains("wakes 6") || lines[1].contains("wakes 7") || lines[1].contains("wakes 8"), "{lines:?}");
    assert!(lines[1].starts_with("Backend: wakes ") && lines[1].contains("/s, late p95"), "a rate, not a counter: {lines:?}");
    assert!(lines[3].starts_with("Backend: wakes ") && lines[3].contains("/s, late p95"), "{lines:?}");
    // A native ASIO driver's events, busy skips with the overloads, join the line once one happened.
    output.overloads = 1;
    output.busy_skips = 2;
    t.app.audio_devices.injected_backend = Some((Some(output), Some(input)));
    t.frames(1);
    let lines: Vec<String> = t.layout()["opened"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect();
    assert!(lines[1].ends_with("min padding 96; overloads 1, busy skips 2, position jumps 0, resets 0"), "{lines:?}");
    t.app.audio_devices.injected_backend = None;
    t.frames(3);
    let plain = t.layout()["opened"].as_array().unwrap().len();
    assert_eq!(plain, 2, "no snapshot, no backend lines");
}

#[test]
fn a_wake_rate_is_the_counter_delta_over_the_time_delta() {
    use crate::audio_devices::WakeRate;
    let mut w = WakeRate::default();
    assert_eq!(w.update(10.0, 1_000), None, "one sample is no rate");
    assert_eq!(w.update(10.1, 1_010), None, "too soon: still no rate");
    assert_eq!(w.update(11.0, 1_100), Some(100));
    assert_eq!(w.update(11.1, 1_110), Some(100), "too soon: the previous rate stays");
    assert_eq!(w.update(13.0, 1_300), Some(100), "200 wakes over 2 s");
    assert_eq!(w.update(14.0, 5), None, "a counter that went down is a new stream");
}

#[test]
fn the_calibration_command_wraps_inside_the_window_and_has_a_copy_button() {
    let line = calibration_line(&AudioConfig::default(), "Speakers", Some("Mic"), 48_000, "ui.json");
    let command = calibration_command(&line).expect("an uncalibrated line carries its command");
    assert!(command.starts_with("audio-bench --backend wasapi") && command.ends_with("--settings \"ui.json\""), "{command}");
    let measured = "measured 300 frames \u{b1}2 (today) \u{b7} shared";
    assert_eq!(calibration_command(measured), None, "a measured line has nothing to copy");
    if !soundcraft_playback::devices::NATIVE_WASAPI_COMPILED {
        return;
    }
    let mut t = T::new();
    t.open_window();
    t.pick("backend", Backend::NativeWasapi.label());
    let window: [f32; 4] = serde_json::from_value(t.layout()["window"].clone()).unwrap();
    let text = t.control_rect("calibration_text");
    let copy = t.control_rect("calibration_copy");
    assert!(text[2] <= window[2] + 0.5, "the wrapped command stays inside the window: {text:?} in {window:?}");
    assert!(text[3] - text[1] > 20.0, "the long command wraps onto more than one line: {text:?}");
    assert!(copy[2] <= window[2], "the Copy button is inside the window");
}

#[test]
fn only_the_selected_asio_driver_says_why_it_did_not_load() {
    let mut t = T::new();
    t.control(
        "audio.fake_devices",
        json!({"systems": [
            {"name": "WASAPI", "is_default": true, "outputs": [{"name": "Speakers (Realtek)", "is_default": true}], "inputs": []},
            {"name": "ASIO",
             "outputs": [{"name": "Good ASIO", "max_channels": 2, "is_default": true}, {"name": "Move", "max_channels": 2}, {"name": "Overbridge 1", "max_channels": 2}, {"name": "Maschine", "max_channels": 2}],
             "inputs": [{"name": "Good ASIO", "max_channels": 2, "is_default": true}],
             "diagnostics": [
                "Move: ASIO driver \"Move\": init failed: No device is connected to the PC.",
                "Overbridge 1: ASIO driver \"Overbridge 1\": init failed: No device is connected to the PC.",
                "Maschine: ASIO driver \"Maschine\": init failed: No device is connected to the PC."]}
        ]}),
    )
    .unwrap();
    t.open_window();
    t.pick("system", "ASIO");
    assert!(t.layout()["diagnostics"].as_array().unwrap().is_empty(), "a loaded driver is selected: nothing is said");
    t.pick("output", "Move · 2 ch");
    assert_eq!(t.layout()["diagnostics"], json!(["Move: init failed: No device is connected to the PC."]));
    t.pick("output", "Good ASIO · 2 ch");
    assert!(t.layout()["diagnostics"].as_array().unwrap().is_empty());
}
