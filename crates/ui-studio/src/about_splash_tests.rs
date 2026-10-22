//! Help > About and the launch splash, driven like a user: synthetic clicks through the control
//! channel's input queue, one frame at a time.

use crate::plugin_scan::ScanProgress;
use crate::{Services, StudioApp};
use serde_json::{Value, json};

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
    copied: Vec<String>,
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

impl T {
    fn new() -> T {
        let app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let mut t = T { app, ctx: egui::Context::default(), time: 0.0, copied: Vec::new() };
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
            for c in &out.platform_output.commands {
                if let egui::OutputCommand::CopyText(t) = c {
                    self.copied.push(t.clone());
                }
            }
        }
    }

    fn click(&mut self, at: (f32, f32)) {
        let (req, _rx) = crate::ControlRequest::new("ui.click", json!({"x": at.0, "y": at.1}));
        let ctx = egui::Context::default();
        let _ = crate::control::handle_for_test(&mut self.app, &ctx, &req);
        self.frames(30);
    }

    fn inspect(&self) -> Value {
        self.app.inspect(&self.ctx)
    }

    fn menu(&mut self, name: &str) {
        let r =
            self.inspect()["bar_menu"].as_array().and_then(|a| a.iter().find(|i| i["label"] == name).map(|i| i["rect"].clone())).unwrap_or_default();
        let r: [f32; 4] = serde_json::from_value(r).unwrap_or_else(|_| panic!("no menu item {name}"));
        self.click(mid(r));
    }
}

fn rect(v: &Value) -> [f32; 4] {
    serde_json::from_value(v.clone()).unwrap()
}

#[test]
fn the_help_menu_opens_about_with_wordmark_version_and_credit() {
    let mut t = T::new();
    t.app.about.app_version = "9.9.9-test".into();
    assert_eq!(t.inspect()["about"]["open"], false);
    t.menu("Help");
    t.menu("About transit…");
    let about = t.inspect()["about"].clone();
    assert_eq!(about["open"], true);
    assert_eq!(about["version"], "transit 9.9.9-test");
    assert_eq!(about["credit"], "Based on SoundCraft by storytold");
    assert_eq!(about["licenses"], "MIT OR Apache-2.0");
    let w = rect(&about["wordmark"]);
    assert!((w[2] - w[0] - 260.0).abs() < 1.5, "260 points wide: {w:?}");
    assert!(((w[2] - w[0]) / (w[3] - w[1]) - 3.0).abs() < 0.05, "3:1, never stretched");
    let win = rect(&about["window"]);
    let cx = (win[0] + win[2]) / 2.0;
    assert!((cx - 700.0).abs() < 2.0, "centered: {win:?}");
}

#[test]
fn the_wordmark_follows_the_theme_without_tinting() {
    let mut t = T::new();
    t.app.about.open = true;
    t.frames(2);
    let dark = crate::icons::rasterized(&t.ctx);
    t.ctx.set_theme(egui::Theme::Light);
    t.frames(3);
    assert!(crate::icons::rasterized(&t.ctx) > dark, "the graphite artwork is a second texture");
    let n = crate::icons::rasterized(&t.ctx);
    t.frames(5);
    assert_eq!(crate::icons::rasterized(&t.ctx), n, "cached, not rasterized every frame");
}

#[test]
fn copy_version_info_puts_the_facts_on_the_clipboard_and_close_closes() {
    let mut t = T::new();
    t.app.about.app_version = "9.9.9-test".into();
    t.app.about.open = true;
    t.frames(2);
    let copy = rect(&t.inspect()["about"]["copy"]);
    t.click(mid(copy));
    let text = t.copied.last().expect("a copy command").clone();
    assert!(
        text.starts_with(
            "transit 9.9.9-test
"
        ),
        "{text}"
    );
    assert!(text.contains(&format!("OS: {}", std::env::consts::OS)), "{text}");
    assert!(text.contains("Audio: no device open"), "{text}");
    let close = rect(&t.inspect()["about"]["close"]);
    t.click(mid(close));
    assert_eq!(t.inspect()["about"]["open"], false);
}

fn scanning(index: usize, total: usize) -> Option<ScanProgress> {
    Some(ScanProgress { format: "VST3", index, total, folder: "VST3".into() })
}

#[test]
fn the_splash_shows_scan_counts_then_hides_when_the_scan_completes() {
    let mut t = T::new();
    assert_eq!(t.inspect()["splash"]["visible"], false, "unarmed: never shows");
    t.app.splash.arm();
    t.app.splash.scan_override = Some(scanning(12, 40));
    t.frames(3);
    let s = t.inspect()["splash"].clone();
    assert_eq!(s["visible"], true);
    assert!(s["status"].as_str().unwrap().starts_with("Scanning plug-ins… 12 of 40"), "{s}");
    assert!((s["fraction"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    assert!(rect(&s["wordmark"])[2] > 0.0 && rect(&s["bar"])[2] > 0.0 && rect(&s["skip"])[2] > 0.0);
    t.app.splash.scan_override = Some(scanning(40, 40));
    t.frames(2);
    assert_eq!(t.inspect()["splash"]["visible"], true);
    t.app.splash.scan_override = None; // the global scan state: nothing running
    t.frames(2);
    assert_eq!(t.inspect()["splash"]["visible"], false);
    t.app.splash.scan_override = Some(scanning(1, 2));
    t.frames(2);
    assert_eq!(t.inspect()["splash"]["visible"], false, "it does not come back");
}

#[test]
fn a_click_or_skip_hides_the_splash_early_and_the_device_line_shows() {
    let mut t = T::new();
    t.app.splash.arm();
    t.app.splash.opening_device = Some("Focusrite USB ASIO".into());
    t.frames(3);
    assert_eq!(t.inspect()["splash"]["status"], "Opening Focusrite USB ASIO…");
    t.click((300.0, 400.0));
    assert_eq!(t.inspect()["splash"]["visible"], false, "a click anywhere hides it");

    let mut t = T::new();
    t.app.splash.arm();
    t.app.splash.scan_override = Some(None);
    t.frames(3);
    assert_eq!(t.inspect()["splash"]["status"], "Scanning plug-ins…");
    let skip = rect(&t.inspect()["splash"]["skip"]);
    t.click(mid(skip));
    assert_eq!(t.inspect()["splash"]["visible"], false);
}

#[test]
fn the_splash_reports_the_device_stage_before_the_scan_stage() {
    let mut t = T::new();
    let opened = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let seen = std::rc::Rc::clone(&opened);
    t.app.services.open_audio = Some(Box::new(move |session, _| {
        seen.set(seen.get() + 1);
        soundcraft_playback::Player::prepare_device_free(session).unwrap()
    }));
    t.app.splash.arm();
    t.app.splash.scan_override = Some(scanning(1, 4));
    t.app.splash.defer_open(soundcraft_playback::AudioConfig { output: Some("Focusrite USB ASIO".into()), ..Default::default() });
    t.frames(1);
    assert_eq!(opened.get(), 0, "the first frame only announces the open");
    assert_eq!(t.inspect()["splash"]["status"], "Opening Focusrite USB ASIO…");
    t.frames(2);
    assert_eq!(opened.get(), 1, "the next frame opens the device");
    assert!(t.app.player.is_some());
    let s = t.inspect()["splash"].clone();
    assert!(s["status"].as_str().unwrap().starts_with("Scanning plug-ins…"), "{s}");
    // The device-free player counts as not opened: its failure shows for exactly one frame.
    assert_eq!(t.app.splash.stages, ["device", "failed", "scan"], "the device stage comes before the scan stage");
}
