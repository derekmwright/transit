//! Audio › Audio Devices: the audio system, output and input device, sample rate and buffer size,
//! kept as `UiState.audio` (saved in `ui.json` with the other preferences). Every choice starts at
//! "System default".
//!
//! The lists come from [`soundcraft_playback::devices::audio_systems`], read when the window opens
//! and on Rescan, never per frame. Without a player (`--no-audio`, tests) nothing is enumerated
//! (listing ASIO loads each driver): the choices are only saved, for the next start.
//!
//! Apply reopens the devices through [`StudioApp::reconnect_audio`] (plug-in editors close first,
//! on the main thread, and the old device closes before the new one opens: an ASIO driver is
//! process-wide). It stops playback first and is refused while recording. `ui.set {audio}` takes
//! the same path. A choice that cannot be honored falls back to the default (the playback crate
//! logs it); the fallback is said on the status line and in the window.

use crate::StudioApp;
use crate::arrange::{R4, r4};
use crate::icons::{self, Icon};
use crate::mixer::MenuItem;
use crate::theme::{SPACE, TOKENS};
use soundcraft_playback::AudioConfig;
use soundcraft_playback::devices::{self, AudioSystem, DeviceInfo};

/// Said when Apply (or `ui.set {audio}`) is asked while recording.
pub const REFUSED_RECORDING: &str = "Stop recording before changing the audio devices.";
/// The label of every "no choice" entry.
pub const SYSTEM_DEFAULT: &str = "System default";
const CHOOSE_ANOTHER: &str = "Choose another in Audio › Audio Devices.";

/// Where the window and its controls were last drawn, for `ui.inspect` and tests.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AudioDevicesLayout {
    pub open: bool,
    pub window: R4,
    /// The choices being edited (Apply makes them `ui.audio`).
    pub draft: AudioConfig,
    /// The audio systems listed.
    pub systems: Vec<String>,
    /// The controls: label is the text shown, value the control (`system`, `output`, `input`,
    /// `rate`, `buffer`, `apply`, `reconnect`, `rescan`, `panel`).
    pub controls: Vec<MenuItem>,
    /// The entries of the open list: label is the text shown, value the control it belongs to.
    pub items: Vec<MenuItem>,
    /// What is open now.
    pub opened: Vec<String>,
    /// Fallbacks and notes, as shown with the warning icon.
    pub notes: Vec<String>,
}

/// Devices standing in for the machine's (the control channel's `audio.fake_devices`, tests):
/// listed instead of enumerating, with device operations injected into the real reconnect path.
#[derive(Debug, Default)]
pub struct FakeDevices {
    pub systems: Vec<AudioSystem>,
    /// Every configuration a reopen was asked with, in order.
    pub reopened: Vec<AudioConfig>,
    pub(crate) connection: Option<crate::audio_connection::FakeConnection>,
    pub(crate) input: Option<crate::audio_connection::FakeInput>,
    pub(crate) trace: crate::audio_connection::Trace,
    pub(crate) enumerations: usize,
}

impl FakeDevices {
    pub(crate) fn input_channels_for(&self, cfg: &AudioConfig) -> usize {
        let Some(sys) = pick_system(&self.systems, cfg.system.as_deref()) else { return 0 };
        let named = cfg.input.as_deref().and_then(|w| sys.inputs.iter().find(|d| d.name == w));
        named.or_else(|| sys.inputs.iter().find(|d| d.is_default)).map_or(0, |d| usize::from(d.max_channels))
    }

    /// The output a real reopen would get for `cfg`: the named device when listed, else the
    /// system's default.
    fn output_for(&self, cfg: &AudioConfig) -> Option<String> {
        let sys = pick_system(&self.systems, cfg.system.as_deref())?;
        let named = cfg.output.as_deref().and_then(|w| sys.outputs.iter().find(|d| d.name == w));
        named.or_else(|| sys.outputs.iter().find(|d| d.is_default)).map(|d| d.name.clone())
    }
}

#[derive(Default)]
pub struct AudioDevicesState {
    pub open: bool,
    pub draft: AudioConfig,
    /// The `ui.audio` the draft was taken from: when it changes elsewhere (`ui.set`), the draft follows.
    base: Option<AudioConfig>,
    systems: Option<Vec<AudioSystem>>,
    pub fake: Option<FakeDevices>,
    /// Fallbacks found at the last reopen (or at startup).
    pub notes: Vec<String>,
    startup_checked: bool,
    /// The (wanted, opened) input already reported, so it is said once.
    input_reported: Option<(String, String)>,
    pub layout: AudioDevicesLayout,
    verification: Option<Verification>,
}

impl AudioDevicesState {
    pub(crate) fn set_fake(&mut self, fake: Option<FakeDevices>) {
        self.systems = fake.as_ref().map(|f| f.systems.clone());
        self.fake = fake;
    }
    pub(crate) fn begin_verification(&mut self) {
        self.verification = Some(Verification::default());
    }
    /// List the devices again on the next frame the window is drawn (Rescan, a new fake).
    pub fn forget_listing(&mut self) {
        self.systems = None;
    }
}

pub(crate) fn validate(cfg: &AudioConfig) -> Result<(), String> {
    if cfg.sample_rate == Some(0) || cfg.buffer_size == Some(0) {
        return Err("Audio sample rate and buffer size must be greater than zero (or null for default).".into());
    }
    Ok(())
}

/// A new stream must deliver two seconds of audio AND remain healthy for two seconds of UI
/// time. A silent clock, a callback burst followed by a stall, or any error cannot qualify.
#[derive(Default)]
struct Verification {
    first: Option<(f64, u64)>,
    last_progress: Option<(f64, u64)>,
}

impl Verification {
    fn ready(&mut self, now: f64, rate: u32, t: soundcraft_playback::telemetry::CallbackTelemetry) -> bool {
        if rate == 0 || t.callbacks == 0 || !now.is_finite() {
            return false;
        }
        let progressed = self.last_progress.is_none_or(|(_, callbacks)| t.callbacks > callbacks);
        if self.last_progress.is_some_and(|(at, _)| now - at > 2.0) {
            self.first = None;
        }
        if progressed {
            self.last_progress = Some((now, t.callbacks));
        }
        let (at, frames) = *self.first.get_or_insert((now, t.frames));
        progressed && now - at >= 2.0 && t.frames.saturating_sub(frames) >= u64::from(rate) * 2
    }
}

impl StudioApp {
    pub(crate) fn save_good_audio(&mut self) {
        if self.audio_devices.verification.is_none() {
            return;
        }
        let (rate, telemetry, failed) = if let Some(p) = self.audio_devices.fake.as_ref().and_then(|f| f.connection.as_ref()) {
            (p.config.sample_rate.unwrap_or(48_000), p.telemetry, p.failed)
        } else if let Some(p) = &self.player {
            (p.device_rate, p.callback_telemetry(), p.silent || p.trouble().is_some() || self.recorder_failed)
        } else {
            return;
        };
        if failed || telemetry.backend_errors != 0 || telemetry.device_lost_errors != 0 || telemetry.fault_silence_frames != 0 {
            self.audio_devices.verification = None;
            return;
        }
        let now = self.last_frame.unwrap_or_default();
        if !self.audio_devices.verification.as_mut().is_some_and(|v| v.ready(now, rate, telemetry)) {
            return;
        }
        self.audio_devices.verification = None;
        // The desktop host supplies the settings directory through its Autosave sibling. Tests
        // without that service never write preferences. Honor the host's no-preferences switches.
        if std::env::var_os("TRANSIT_NO_PREFS").is_some() || std::env::var_os("SOUNDCRAFT_NO_PREFS").is_some() {
            return;
        }
        let Some(dir) = self.autosave_dir.as_deref().and_then(std::path::Path::parent) else { return };
        if let Err(e) = persist_audio(&dir.join("ui.json"), &self.ui) {
            self.status = format!("Audio is running, but its settings could not be saved: {e}");
        }
    }
}

/// Preserve the other saved preferences and atomically replace only the known-good audio block.
fn persist_audio(path: &std::path::Path, ui: &crate::UiState) -> Result<(), String> {
    use std::io::{Read, Write};
    let mut value = match std::fs::File::open(path) {
        Ok(f) => {
            let mut bytes = Vec::new();
            f.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            if bytes.len() > 4 * 1024 * 1024 {
                return Err("Settings file exceeds the 4 MiB limit".into());
            }
            serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| e.to_string())?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut saved = ui.clone();
            saved.plugin_windows.clear();
            serde_json::to_value(saved).map_err(|e| e.to_string())?
        }
        Err(e) => return Err(e.to_string()),
    };
    let obj = value.as_object_mut().ok_or("Settings must be an object")?;
    obj.insert("audio".into(), serde_json::to_value(&ui.audio).map_err(|e| e.to_string())?);
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    let tmp = path.with_extension(format!("audio.{}.{nonce}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| e.to_string())
}

/// The chosen system in `systems`, else the default one.
fn pick_system<'a>(systems: &'a [AudioSystem], name: Option<&str>) -> Option<&'a AudioSystem> {
    name.and_then(|n| systems.iter().find(|s| s.name.eq_ignore_ascii_case(n))).or_else(|| systems.iter().find(|s| s.is_default))
}

/// A device as a list shows it: its name and channel count (unknown until an ASIO driver is open).
pub fn device_label(d: &DeviceInfo) -> String {
    match d.max_channels {
        0 => d.name.clone(),
        1 => format!("{} · 1 ch", d.name),
        n => format!("{} · {n} ch", d.name),
    }
}

/// What the player opened, for comparing with what was asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Opened {
    pub output: Option<String>,
    pub rate: Option<u32>,
    pub buffer: Option<u32>,
}

/// The choices in `cfg` that were not honored: a system not on this machine, an output that did
/// not open (unplugged, renamed), an input not listed, a rate or buffer size the device refused.
pub fn fallback_notes(cfg: &AudioConfig, systems: Option<&[AudioSystem]>, opened: &Opened) -> Vec<String> {
    let mut notes = Vec::new();
    if let (Some(want), Some(list)) = (cfg.system.as_deref(), systems)
        && !list.iter().any(|s| s.name.eq_ignore_ascii_case(want))
    {
        notes.push(format!("Audio system “{want}” is not available; using the default."));
    }
    if let (Some(want), Some(got)) = (cfg.output.as_deref(), opened.output.as_deref())
        && want != got
    {
        notes.push(format!("Output “{want}” is not available; using “{got}”."));
    }
    let asio = cfg.system.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(devices::ASIO_SYSTEM));
    if let (Some(want), Some(list)) = (cfg.input.as_deref(), systems)
        && !asio
        && let Some(sys) = pick_system(list, cfg.system.as_deref())
        && !sys.inputs.iter().any(|d| d.name == want)
    {
        notes.push(format!("Input “{want}” is not available; recording uses the default input."));
    }
    if let (Some(want), Some(got)) = (cfg.sample_rate, opened.rate)
        && want != got
    {
        notes.push(format!("{want} Hz is not available; running at {got} Hz."));
    }
    if let Some(want) = cfg.buffer_size
        && opened.output.is_some()
        && opened.buffer != Some(want)
    {
        notes.push(format!("A {want}-sample buffer is not available; using the device's default."));
    }
    notes
}

impl StudioApp {
    /// Audio › Audio Devices: open the window with the saved choices.
    pub fn open_audio_devices(&mut self) {
        let s = &mut self.audio_devices;
        s.open = true;
        s.draft = self.ui.audio.clone();
        s.base = Some(self.ui.audio.clone());
        s.systems = None;
    }

    /// The window's Apply: keep `cfg` as `ui.audio` and reopen the devices with it. Playback stops
    /// first; refused while recording (the take would be cut), leaving `ui.audio` as it was.
    pub fn apply_audio(&mut self, cfg: AudioConfig) -> Result<(), String> {
        validate(&cfg)?;
        if self.recording_active() {
            self.status = REFUSED_RECORDING.into();
            return Err(REFUSED_RECORDING.into());
        }
        self.ui.audio = cfg;
        self.audio_devices.base = Some(self.ui.audio.clone());
        self.audio_devices.draft = self.ui.audio.clone();
        self.reopen_audio();
        Ok(())
    }

    /// Reopen the devices with `ui.audio` (Apply, `ui.set {audio}`). Without a player
    /// (`--no-audio`) the choice is only saved.
    pub(crate) fn reopen_audio(&mut self) {
        let was_playing = self.is_playing();
        if was_playing {
            self.stop_play();
        }
        let cfg = self.ui.audio.clone();
        self.audio_devices.input_reported = None;
        let opened = if self.player.is_some() || self.audio_devices.fake.is_some() {
            self.reconnect_audio();
            self.opened()
        } else {
            self.audio_devices.notes.clear();
            self.status = "Audio devices saved. Audio is off in this run (--no-audio); they open at the next start.".into();
            return;
        };
        let notes = fallback_notes(&cfg, self.audio_devices.systems.as_deref(), &opened);
        let stopped = if was_playing { "Playback stopped to change devices. " } else { "" };
        self.status = if notes.is_empty() {
            let name = opened.output.unwrap_or_else(|| "no audio device".into());
            format!("{stopped}Audio devices reopened: {name}.")
        } else {
            format!("{stopped}{} {CHOOSE_ANOTHER}", notes.join(" "))
        };
        self.audio_devices.notes = notes;
    }

    /// The window's Reconnect: reopen the same choices (a device came back, a driver changed).
    pub fn reconnect_audio_devices(&mut self) {
        if self.recording_active() {
            self.status = REFUSED_RECORDING.into();
            return;
        }
        self.reopen_audio();
    }

    fn opened(&self) -> Opened {
        if let Some(f) = &self.audio_devices.fake
            && let Some(p) = &f.connection
        {
            return Opened { output: f.output_for(&p.config), rate: Some(p.config.sample_rate.unwrap_or(48_000)), buffer: p.config.buffer_size };
        }
        match &self.player {
            Some(p) if !p.silent => Opened { output: Some(p.device_name.clone()), rate: Some(p.device_rate), buffer: p.device_buffer },
            _ => Opened::default(),
        }
    }

    /// The devices to offer: the fake's, else the machine's (only with a player).
    fn listing(&mut self) -> Vec<AudioSystem> {
        match (&mut self.audio_devices.fake, &self.player) {
            (Some(f), _) => {
                f.enumerations += 1;
                f.systems.clone()
            }
            (None, Some(_)) => devices::audio_systems(),
            (None, None) => Vec::new(),
        }
    }

    fn running_asio(&self) -> bool {
        let cfg = self.audio_devices.fake.as_ref().and_then(|f| f.connection.as_ref()).map_or(&self.ui.audio, |p| &p.config);
        self.opened().output.is_some() && cfg.system.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(devices::ASIO_SYSTEM))
    }
}

/// Say once what the saved choices did not get: the output at startup, the input when it opens.
fn check(app: &mut StudioApp) {
    if !app.audio_devices.startup_checked && app.player.is_some() {
        app.audio_devices.startup_checked = true;
        let notes = fallback_notes(&app.ui.audio, None, &app.opened());
        if !notes.is_empty() {
            app.status = format!("{} {CHOOSE_ANOTHER}", notes.join(" "));
            app.audio_devices.notes = notes;
        }
    }
    let (Some(want), Some(r)) = (app.ui.audio.input.as_deref(), &app.recorder) else { return };
    let pair = (want.to_string(), r.device_name.clone());
    if pair.0 == pair.1 || app.audio_devices.input_reported.as_ref() == Some(&pair) {
        return;
    }
    let asio = app.ui.audio.system.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(devices::ASIO_SYSTEM));
    let note = if asio {
        format!("On ASIO the input is the output's device: recording from “{}”, not “{want}”.", pair.1)
    } else {
        format!("Input “{want}” is not available; recording from “{}”.", pair.1)
    };
    app.status = format!("{note} {CHOOSE_ANOTHER}");
    app.audio_devices.notes.retain(|n| !n.starts_with("Input ") && !n.starts_with("On ASIO"));
    app.audio_devices.notes.push(note);
    app.audio_devices.input_reported = Some(pair);
}

/// One list of choices (`None` is "System default"). Returns the combo's rect.
fn choose<T: Clone + PartialEq>(
    ui: &mut egui::Ui,
    key: &str,
    enabled: bool,
    value: &mut Option<T>,
    options: &[(T, String)],
    items: &mut Vec<MenuItem>,
) -> (egui::Rect, String) {
    let shown = value.as_ref().and_then(|v| options.iter().find(|(o, _)| o == v).map(|(_, t)| t.clone()));
    // A saved choice not offered here (unplugged) still shows, marked.
    let text = match (value.as_ref(), shown) {
        (None, _) => SYSTEM_DEFAULT.to_string(),
        (Some(_), Some(t)) => t,
        (Some(_), None) => "Not available (falls back to the default)".to_string(),
    };
    let r = ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(("audio_devices", key)).selected_text(&text).width(360.0).show_ui(ui, |ui| {
            let b = crate::menus::check_button(ui, value.is_none(), SYSTEM_DEFAULT);
            items.push(MenuItem::at(ui, SYSTEM_DEFAULT.into(), key.into(), b.rect));
            if b.clicked() {
                *value = None;
            }
            for (v, t) in options {
                let b = crate::menus::check_button(ui, value.as_ref() == Some(v), t);
                items.push(MenuItem::at(ui, t.clone(), key.into(), b.rect));
                if b.clicked() {
                    *value = Some(v.clone());
                }
            }
        })
    });
    (r.inner.response.rect, text)
}

/// Audio › Audio Devices.
pub fn window(app: &mut StudioApp, ctx: &egui::Context) {
    check(app);
    app.audio_devices.layout = AudioDevicesLayout::default();
    if !app.audio_devices.open {
        return;
    }
    if app.audio_devices.base.as_ref() != Some(&app.ui.audio) {
        app.audio_devices.draft = app.ui.audio.clone();
        app.audio_devices.base = Some(app.ui.audio.clone());
    }
    if app.audio_devices.systems.is_none() {
        app.audio_devices.systems = Some(app.listing());
    }
    let systems = app.audio_devices.systems.clone().unwrap_or_default();
    let mut draft = app.audio_devices.draft.clone();
    let mut lay = AudioDevicesLayout { open: true, systems: systems.iter().map(|s| s.name.clone()).collect(), ..Default::default() };
    let recording = app.recording_active();
    let audio_on = app.player.is_some() || app.audio_devices.fake.is_some();
    let (mut apply, mut reconnect, mut rescan, mut panel) = (false, false, false, false);
    let mut open = true;
    let r = egui::Window::new("Audio Devices")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_pos(ctx.content_rect().center() - egui::vec2(260.0, 200.0))
        .show(ctx, |ui| {
            ui.set_width(520.0);
            let sys = pick_system(&systems, draft.system.as_deref());
            let asio = sys.is_some_and(|s| s.name == devices::ASIO_SYSTEM);
            let mut controls = Vec::new();
            egui::Grid::new("audio_devices_grid").num_columns(2).spacing([2.0 * SPACE, SPACE]).show(ui, |ui| {
                let label = |ui: &mut egui::Ui, t: &str| ui.label(egui::RichText::new(t).color(TOKENS.text_dim));
                label(ui, "Audio system");
                let names: Vec<(String, String)> = systems.iter().map(|s| (s.name.clone(), s.name.clone())).collect();
                let before = draft.system.clone();
                let (rect, text) = choose(ui, "system", audio_on, &mut draft.system, &names, &mut lay.items);
                if draft.system != before {
                    // Devices belong to their system.
                    (draft.output, draft.input) = (None, None);
                }
                controls.push((text, "system", rect));
                ui.end_row();

                label(ui, "Output");
                let outs: Vec<(String, String)> =
                    sys.map(|s| s.outputs.iter().map(|d| (d.name.clone(), device_label(d))).collect()).unwrap_or_default();
                let (rect, text) = choose(ui, "output", audio_on, &mut draft.output, &outs, &mut lay.items);
                controls.push((text, "output", rect));
                ui.end_row();

                label(ui, "Input");
                let ins: Vec<(String, String)> =
                    sys.map(|s| s.inputs.iter().map(|d| (d.name.clone(), device_label(d))).collect()).unwrap_or_default();
                let (rect, text) = choose(ui, "input", audio_on && !asio, &mut draft.input, &ins, &mut lay.items);
                controls.push((text, "input", rect));
                ui.end_row();
                if asio {
                    ui.label("");
                    ui.label(egui::RichText::new("On ASIO the input is the output's device.").color(TOKENS.text_dim).size(11.0));
                    ui.end_row();
                }

                // Rates and buffers of the chosen output (the system's default output otherwise).
                let dev = sys.and_then(|s| {
                    let named = draft.output.as_deref().and_then(|w| s.outputs.iter().find(|d| d.name == w));
                    named.or_else(|| s.outputs.iter().find(|d| d.is_default))
                });
                label(ui, "Sample rate");
                let rates: Vec<(u32, String)> =
                    dev.map_or(devices::STANDARD_RATES.to_vec(), |d| d.sample_rates.clone()).into_iter().map(|r| (r, format!("{r} Hz"))).collect();
                let (rect, text) = choose(ui, "rate", audio_on, &mut draft.sample_rate, &rates, &mut lay.items);
                controls.push((text, "rate", rect));
                ui.end_row();

                label(ui, "Buffer size");
                let sizes = dev.map_or(devices::STANDARD_BUFFERS.to_vec(), DeviceInfo::buffer_sizes);
                let bufs: Vec<(u32, String)> = sizes.into_iter().map(|b| (b, format!("{b} samples"))).collect();
                let (rect, text) = choose(ui, "buffer", audio_on, &mut draft.buffer_size, &bufs, &mut lay.items);
                controls.push((text, "buffer", rect));
                ui.end_row();
            });
            ui.add_space(SPACE);
            lay.opened = opened_lines(app);
            for line in &lay.opened {
                ui.label(egui::RichText::new(line).color(TOKENS.text_dim).size(11.5));
            }
            if !audio_on {
                ui.label(
                    egui::RichText::new("Audio is off in this run (--no-audio): choices are saved and open at the next start.")
                        .color(TOKENS.text_dim)
                        .size(11.5),
                );
            }
            for n in &app.audio_devices.notes {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(icons::ICON, icons::ICON), egui::Sense::hover());
                    icons::paint(ui.painter(), r.center(), Icon::Warning, 14.0, TOKENS.record);
                    ui.label(egui::RichText::new(n).color(TOKENS.text).size(11.5));
                });
            }
            if recording {
                ui.label(egui::RichText::new(REFUSED_RECORDING).color(TOKENS.record).size(11.5));
            }
            ui.add_space(SPACE);
            ui.horizontal(|ui| {
                let b = ui.add_enabled(!recording, egui::Button::new("Apply"));
                controls.push(("Apply".into(), "apply", b.rect));
                apply = b.on_hover_text("Reopen the devices with these choices. Playback stops first.").clicked();
                let b = ui.add_enabled(!recording && audio_on, egui::Button::new("Reconnect"));
                controls.push(("Reconnect".into(), "reconnect", b.rect));
                reconnect = b.on_hover_text("Reopen the saved devices (after plugging one back in).").clicked();
                let b = ui.add_enabled(audio_on, egui::Button::new("Rescan Devices"));
                controls.push(("Rescan Devices".into(), "rescan", b.rect));
                rescan = b.clicked();
                if app.running_asio() && (devices::ASIO_CONTROL_PANEL || app.audio_devices.fake.is_some()) {
                    let b = ui.button("Driver Control Panel");
                    controls.push(("Driver Control Panel".into(), "panel", b.rect));
                    panel = b.on_hover_text("The open ASIO driver's own settings. Reconnect afterwards.").clicked();
                }
            });
            lay.controls = controls.into_iter().map(|(text, key, rect)| MenuItem::at(ui, text, key.into(), rect)).collect();
        });
    if let Some(r) = r {
        lay.window = r4(r.response.rect);
    }
    lay.draft = draft.clone();
    lay.notes = app.audio_devices.notes.clone();
    app.audio_devices.draft = draft.clone();
    app.audio_devices.layout = lay;
    if apply {
        let _ = app.apply_audio(draft);
    }
    if reconnect {
        app.reconnect_audio_devices();
    }
    if rescan {
        app.audio_devices.forget_listing();
    }
    if panel
        && app.audio_devices.fake.is_none()
        && let Err(e) = devices::asio_control_panel()
    {
        app.status = format!("ASIO control panel: {e}");
    }
    if !open {
        app.audio_devices.open = false;
        app.audio_devices.layout.open = false;
    }
}

/// What is open now: the output with its rate, channels and buffer, and the input.
fn opened_lines(app: &StudioApp) -> Vec<String> {
    let mut lines = Vec::new();
    match &app.player {
        Some(p) => {
            let buffer = p.device_buffer.map_or("default buffer".to_string(), |b| format!("{b}-sample buffer"));
            lines.push(format!("Output open: {} · {} Hz · {} ch · {buffer}", p.device_name, p.device_rate, p.device_channels));
        }
        None => lines.push("Output open: none".into()),
    }
    lines.push(match &app.recorder {
        Some(r) => format!("Input open: {} · {} Hz · {} ch", r.device_name, r.sample_rate, r.channels),
        None => "Input open: none yet (it opens when a track is armed or monitored)".into(),
    });
    lines
}

/// `audio.fake_devices {systems: [{name, is_default?, outputs: [...], inputs: [...]}]}`: stand-in
/// devices for screenshot runs and tests (each device `{name, is_default?, max_channels?,
/// sample_rates?}`); `{systems: null}` removes them.
pub fn fake_from_json(p: &serde_json::Value) -> Result<Option<FakeDevices>, String> {
    use serde_json::Value;
    let Some(list) = p.get("systems") else { return Err("audio.fake_devices needs systems".into()) };
    if list.is_null() {
        return Ok(None);
    }
    let Some(list) = list.as_array() else { return Err("systems must be a list".into()) };
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let flag = |v: &Value, k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let devs = |v: &Value, k: &str| -> Result<Vec<DeviceInfo>, String> {
        v.get(k)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|d| {
                Ok(DeviceInfo {
                    name: text(d, "name").ok_or("every device needs a name")?,
                    is_default: flag(d, "is_default"),
                    max_channels: d.get("max_channels").and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok()).unwrap_or(2),
                    sample_rates: d
                        .get("sample_rates")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|r| r.as_u64().and_then(|r| u32::try_from(r).ok())).collect())
                        .unwrap_or_else(|| vec![44_100, 48_000]),
                    buffer_min: Some(64),
                    buffer_max: Some(2048),
                })
            })
            .collect()
    };
    let systems = list
        .iter()
        .map(|s| {
            Ok(AudioSystem {
                name: text(s, "name").ok_or("every system needs a name")?,
                is_default: flag(s, "is_default"),
                outputs: devs(s, "outputs")?,
                inputs: devs(s, "inputs")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut fake = FakeDevices { systems, ..Default::default() };
    fake.connection = Some(crate::audio_connection::FakeConnection::new(AudioConfig::default(), std::rc::Rc::clone(&fake.trace)));
    Ok(Some(fake))
}
