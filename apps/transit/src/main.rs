//! transit desktop app: the studio UI over the transit engine.
//!
//! Usage: `transit [--demo] [--control <port>] [--no-audio] [session.scraft]`
//!
//! `--control <port>` (or `TRANSIT_CONTROL_PORT`) starts a localhost JSON-lines control server
//! (`0` picks a free port and prints it). Connections must start with `auth <token>`, the token in the
//! `control-token` file of the settings folder (`TRANSIT_CONTROL_TOKEN_FILE` overrides). See `transit_ui_studio::control` for the methods.
//!
//! Structure copied from `apps/soundcraft/src/main.rs` at `transit` 2af868e (file last changed
//! 04e1fe8): the scan child answered first in `main`, the logger, prefs, `--control`, `--no-audio`,
//! `--demo`, and the exit order in `on_exit` (E5: `close_input` before the player is dropped).
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod config;
mod control_server;
#[cfg(any(target_os = "windows", test))]
mod graphics;
mod logging;

#[cfg(test)]
mod audio_review_tests;

use soundcraft_engine::Engine;
use std::sync::Arc;
use transit_ui_studio::{Services, StudioApp, UiState};

struct App(StudioApp, Option<&'static logging::AppLogger>);

/// Sends the title only when it changes (copied from `apps/soundcraft`).
fn set_title(ctx: &egui::Context, title: String) {
    let id = egui::Id::new("window_title");
    if ctx.data(|d| d.get_temp::<String>(id)).as_deref() != Some(title.as_str()) {
        ctx.data_mut(|d| d.insert_temp(id, title.clone()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
    }
}

macro_rules! font {
    ($path:literal) => {
        Arc::new(egui::FontData::from_static(include_bytes!(concat!("../assets/fonts/", $path))))
    };
}

/// Inter for the UI, JetBrains Mono for numbers, the extra weights as named families, and the
/// SoundCraft definitions (egui's built-in fonts, the system Hangul fallback) behind them.
fn font_definitions() -> egui::FontDefinitions {
    use egui::FontFamily;
    let mut defs = soundcraft_ui_egui::fonts::definitions();
    let faces = [
        ("inter-regular", font!("inter/Inter-Regular.ttf")),
        ("inter-medium", font!("inter/Inter-Medium.ttf")),
        ("inter-semibold", font!("inter/Inter-SemiBold.ttf")),
        ("inter-bold", font!("inter/Inter-Bold.ttf")),
        ("mono-regular", font!("jetbrains-mono/JetBrainsMono-Regular.ttf")),
        ("mono-medium", font!("jetbrains-mono/JetBrainsMono-Medium.ttf")),
        ("mono-bold", font!("jetbrains-mono/JetBrainsMono-Bold.ttf")),
    ];
    for (name, data) in faces {
        defs.font_data.insert(name.into(), data);
    }
    // Each family: its face first, then whatever was there (fallbacks for emoji, icons, Hangul).
    let mut put = |family: FontFamily, face: &str| {
        let rest = defs.families.get(&family).cloned().unwrap_or_default();
        let mut list = vec![face.to_string()];
        list.extend(rest.into_iter().filter(|f| f != face));
        defs.families.insert(family, list);
    };
    put(FontFamily::Proportional, "inter-regular");
    put(FontFamily::Monospace, "mono-regular");
    let proportional = defs.families[&FontFamily::Proportional].clone();
    let monospace = defs.families[&FontFamily::Monospace].clone();
    let mut named = |name: &str, face: &str, base: &[String]| {
        let mut list = vec![face.to_string()];
        list.extend(base.iter().filter(|f| *f != face).cloned());
        defs.families.insert(FontFamily::Name(name.into()), list);
    };
    named("inter-medium", "inter-medium", &proportional);
    named("inter-semibold", "inter-semibold", &proportional);
    named("inter-bold", "inter-bold", &proportional);
    named("mono-medium", "mono-medium", &monospace);
    named("mono-bold", "mono-bold", &monospace);
    // SoundCraft's own bold family (headings, theme::bold) becomes Inter Bold too.
    named("bold", "inter-bold", &proportional);
    defs
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // What the realtime audio thread logged is written here, off that thread.
        if let Some(logger) = self.1 {
            logger.report_audio_thread();
        }
        self.0.guarded(|app| app.logic(ctx));
        // The shared UI installs its own fonts on its first frame; ours go in right after, once.
        let id = egui::Id::new("transit_fonts");
        if ctx.data(|d| d.get_temp::<bool>(id)).is_none() {
            ctx.data_mut(|d| d.insert_temp(id, true));
            ctx.set_fonts(font_definitions());
        }
        let dropped: Vec<String> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_string_lossy().into_owned()).filter(|s| !s.is_empty()).collect());
        for p in dropped {
            open_path(&mut self.0, &p);
        }
        if self.0.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if ctx.input(|i| i.viewport().close_requested()) && self.0.engine.is_dirty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.0.run_or_ask("app.quit", serde_json::json!({}));
        }
        // Maximize after the window exists: `with_maximized` on Windows only sets the WS_MAXIMIZE
        // style at creation, so the window reports maximized yet keeps its normal size.
        if START_MAXIMIZED.swap(false, std::sync::atomic::Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        } else {
            let (inner, maximized) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().maximized.unwrap_or(false)));
            self.0.ui.window_maximized = maximized;
            if let Some(r) = inner.filter(|_| !maximized) {
                self.0.ui.window_size = (r.size() * ctx.zoom_factor()).round().into();
            }
        }
        let dirty = if self.0.engine.is_dirty() { " *" } else { "" };
        set_title(ctx, format!("{}{dirty} — transit", self.0.engine.session().name));
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.0.raw_input_hook(raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.guarded(|app| app.ui(ui));
    }
    fn on_exit(&mut self) {
        self.0.close_autosave();
        save_prefs(&self.0.ui);
        if let Some(logger) = self.1 {
            logger.report_audio_thread();
        }
        // E5 quit order (copied from `apps/soundcraft`): finish any take and hand the input back
        // to the player, then release the player's plugin instances, then shut plugin hosting down.
        self.0.close_input();
        self.0.player = None;
        soundcraft_engine::shutdown_plugin_hosts();
    }
}

fn open_path(app: &mut StudioApp, p: &str) {
    let lower = p.to_ascii_lowercase();
    if lower.ends_with(".scraft") {
        app.run_or_ask("session.open", serde_json::json!({"path": p}));
        return;
    }
    let r = if lower.ends_with(".mid") || lower.ends_with(".midi") || lower.ends_with(".smf") {
        app.run("file.import_midi", serde_json::json!({"path": p}))
    } else {
        let at = app.cursor();
        let mut params = serde_json::json!({"path": p, "at": at});
        if let Some(t) = app.cursor_track() {
            params["track"] = serde_json::json!(t.0);
        }
        app.run("file.import_audio", params)
    };
    if let Err(e) = r {
        log::warn!("{p}: {e}");
    }
}

/// Set in `main` when the window should start maximized (first launch, or it was when last closed).
static START_MAXIMIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn prefs_path() -> Option<std::path::PathBuf> {
    config::config_dir().map(|b| b.join("ui.json"))
}

/// Runs without preferences (`TRANSIT_NO_PREFS` or `SOUNDCRAFT_NO_PREFS`, agents' test runs)
/// neither read nor write them, keep no scan cache and write no log file.
fn prefs_enabled() -> bool {
    std::env::var_os("TRANSIT_NO_PREFS").is_none() && std::env::var_os("SOUNDCRAFT_NO_PREFS").is_none()
}

fn load_prefs() -> Option<UiState> {
    if !prefs_enabled() {
        return None;
    }
    match config::load_ui(&prefs_path()?) {
        Ok(state) => state,
        Err(error) => {
            log::warn!("{error}");
            None
        }
    }
}

fn save_prefs(ui: &UiState) {
    if !prefs_enabled() {
        return;
    }
    let mut saved = ui.clone();
    // save_good_audio persists each choice after two seconds of healthy callbacks. Keep that
    // latest verified choice (or defaults on first launch), even if quit follows Apply immediately.
    saved.audio = load_prefs().map(|previous| previous.audio).unwrap_or_default();
    if let Some(path) = prefs_path()
        && let Err(error) = config::save_ui(&path, &saved)
    {
        log::warn!("{error}");
    }
}

fn services() -> Services {
    Services {
        pick_open: Some(Box::new(|purpose: &str| {
            let d = rfd::FileDialog::new();
            if purpose == "folder" {
                // The browser's Add Folder (slice 4).
                return d.pick_folder().map(|p| p.to_string_lossy().into_owned());
            }
            let d = match purpose {
                "session.open" => d.add_filter("Session", &["scraft"]),
                _ => d.add_filter("Audio", &["wav", "wave", "aif", "aiff", "flac", "mp3", "ogg"]),
            };
            d.pick_file().map(|p| p.to_string_lossy().into_owned())
        })),
        pick_save: Some(Box::new(|default: &str| {
            // A bounce (slice 6) asks with its audio file name; everything else is a session.
            let ext = std::path::Path::new(default).extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
            let d = match ext.as_str() {
                "wav" | "aif" | "aiff" | "flac" => rfd::FileDialog::new().add_filter("Audio", &[ext.as_str()]),
                _ => rfd::FileDialog::new().add_filter("Session", &["scraft"]),
            };
            d.set_file_name(default).save_file().map(|p| p.to_string_lossy().into_owned())
        })),
        config_dir: config::config_dir(),
        open_audio: None,
    }
}

/// A saved window size, or the default when the prefs file holds nonsense.
fn sane_window_size([w, h]: [f32; 2]) -> [f32; 2] {
    if w.is_finite() && h.is_finite() && w >= 960.0 && h >= 600.0 { [w.min(16_384.0), h.min(16_384.0)] } else { [1600.0, 1000.0] }
}

/// The window, Dock, taskbar and Alt-Tab icon. macOS gets Apple's icon grid (transparent margin);
/// elsewhere the full tile. Regenerate with `packaging/icons.sh`.
fn app_icon() -> egui::IconData {
    #[cfg(target_os = "macos")]
    let png: &[u8] = include_bytes!("../../../assets/app-icon/transit-macos-512.png");
    #[cfg(not(target_os = "macos"))]
    let png: &[u8] = include_bytes!("../../../assets/app-icon/hicolor/256x256/apps/transit.png");
    eframe::icon_data::from_png_bytes(png).unwrap_or_else(|e| {
        log::warn!("app icon: {e}");
        egui::IconData::default()
    })
}

fn main() -> eframe::Result {
    // A plugin scan's child process: load one file, answer, exit. First, before anything else
    // (E1/E2: otherwise every scan child starts a whole app).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(reply) = transit_ui_studio::plugin_scan::scan_child(&args) {
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), "{reply}");
        let _ = std::io::stdout().flush();
        // Run the module exits now, from `main`, not from the exit handler: a plugin whose
        // `ExitDll` waits for its own threads deadlocks inside `exit()`.
        soundcraft_engine::shutdown_plugin_hosts();
        return Ok(());
    }
    let logger = logging::install();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("transit {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let demo = args.iter().any(|a| a == "--demo" || a == "--sample");
    let no_audio = args.iter().any(|a| a == "--no-audio");
    let control_port: Option<u16> = args
        .iter()
        .position(|a| a == "--control")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .or_else(|| std::env::var("TRANSIT_CONTROL_PORT").ok().and_then(|p| p.parse().ok()));
    let files: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !(a.starts_with("--") || (i > &0 && args.get(i - 1).is_some_and(|p| p == "--control"))))
        .map(|(_, a)| a.clone())
        .collect();
    log::info!("transit {} ({} {})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    if prefs_enabled() {
        config::migrate_legacy_config();
    }
    if let Some(logger) = logger {
        match config::config_dir() {
            _ if !prefs_enabled() => logger.no_file(),
            Some(dir) => match logger.attach_dir(&dir.join(logging::LOG_DIR)) {
                Ok(path) => log::info!("log file {}", path.display()),
                Err(e) => log::warn!("no log file: {e}"),
            },
            None => {
                logger.no_file();
                log::warn!("no log file: no settings directory");
            }
        }
    }

    let engine = if demo { soundcraft_engine::demo::demo_engine() } else { Engine::default() };
    let loaded = load_prefs();
    // First launch (no preferences yet) opens maximized.
    let mut prefs = loaded.unwrap_or_else(|| UiState { window_maximized: true, ..UiState::default() });
    START_MAXIMIZED.store(prefs.window_maximized, std::sync::atomic::Ordering::Relaxed);
    prefs.window_maximized = false;
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(app_icon())
            .with_title("transit")
            .with_inner_size(sane_window_size(prefs.window_size))
            .with_min_inner_size([960.0, 600.0])
            .with_drag_and_drop(true)
            .with_app_id("dev.derekmwright.transit"),
        ..Default::default()
    };
    // Before eframe creates the wgpu instance: default Windows to DirectX 12 with FXC (see graphics.rs).
    #[cfg(target_os = "windows")]
    graphics::configure(&mut options, eframe::wgpu::Backends::from_env(), eframe::wgpu::Dx12Compiler::from_env());
    eframe::run_native(
        "transit",
        options,
        Box::new(move |cc| {
            // The device opens on the frame after the splash first says so (`splash::advance_open`).
            let mut app = StudioApp::new(engine, None, services());
            if !no_audio {
                app.splash.defer_open(prefs.audio.clone());
            }
            // Our own autosave folder: the two apps must not recover each other's sessions. Runs
            // without preferences (agents' tests) autosave into a folder of their own, so they
            // neither offer the user's leftovers nor leave theirs for the user.
            app.autosave_dir = if prefs_enabled() {
                prefs_path().and_then(|p| p.parent().map(|d| d.join("Autosave")))
            } else {
                Some(std::env::temp_dir().join(format!("transit-test-{}", std::process::id())).join("Autosave"))
            };
            app.leftovers = app.autosave_dir.as_deref().map(soundcraft_engine::autosave::leftovers).unwrap_or_default();
            app.ui = prefs;
            // The plug-in scan cache is SoundCraft's, shared; runs without preferences keep none.
            let cache = config::soundcraft_dir().filter(|_| prefs_enabled());
            transit_ui_studio::plugin_scan::start(&app.ui.plugin_dirs, cache.as_deref(), std::env::current_exe().ok());
            app.splash.arm();
            app.about.app_version = env!("CARGO_PKG_VERSION").to_string();
            if let Some(port) = control_port {
                let rx = control_server::start(port, cc.egui_ctx.clone());
                app = app.with_control(rx);
            }
            for f in &files {
                open_path(&mut app, f);
            }
            Ok(Box::new(App(app, logger)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::sane_window_size;

    #[test]
    fn nonsense_window_sizes_fall_back() {
        assert_eq!(sane_window_size([f32::NAN, 10.0]), [1600.0, 1000.0]);
        assert_eq!(sane_window_size([1200.0, 800.0]), [1200.0, 800.0]);
    }
}
