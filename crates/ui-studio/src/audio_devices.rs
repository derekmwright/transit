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
use soundcraft_playback::devices::{self, AudioSystem, DeviceInfo};
use soundcraft_playback::{AudioConfig, Backend};

/// Said when Apply (or `ui.set {audio}`) is asked while recording.
pub const REFUSED_RECORDING: &str = "Stop recording before changing the audio devices.";
/// The label of every "no choice" entry.
/// Why Reconnect, Apply and Rescan wait while a native ASIO driver's settings panel is open: the
/// panel holds the driver's host thread, which each of them would wait on.
pub const PANEL_OPEN_FIRST: &str = "Close the driver settings panel first";
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
    /// `backend` (only where offered), `paired_input` (native WASAPI only), `rate`, `buffer`, `apply`, `reconnect`, `rescan`,
    /// `panel` (cpal ASIO's "Driver Control Panel" or native ASIO's "Driver settings")).
    pub controls: Vec<MenuItem>,
    /// The entries of the open list: label is the text shown, value the control it belongs to.
    pub items: Vec<MenuItem>,
    /// What is open now.
    pub opened: Vec<String>,
    /// Fallbacks and notes, as shown with the warning icon.
    pub notes: Vec<String>,
    /// The path table the I/O section showed (the engine's, read this frame).
    pub io: Option<soundcraft_model::IoMap>,
    /// The Calibration line (only under a native backend: under the Paired input for WASAPI).
    pub calibration: Option<String>,
    /// The drivers of the chosen system that failed to load, as said under the Output list
    /// (native ASIO's per-driver diagnostics).
    pub diagnostics: Vec<String>,
    /// The Hardware inserts subsection as drawn.
    pub hardware: crate::audio_devices_io::HardwareLayout,
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
    pub(crate) systems: Option<Vec<AudioSystem>>,
    pub fake: Option<FakeDevices>,
    /// Fallbacks found at the last reopen (or at startup).
    pub notes: Vec<String>,
    startup_checked: bool,
    /// The (wanted, opened) input already reported, so it is said once.
    input_reported: Option<(String, String)>,
    pub layout: AudioDevicesLayout,
    verification: Option<Verification>,
    io_editor: crate::audio_devices_io::IoEditor,
    /// The measurement the Hardware inserts Measure started and has not collected yet.
    pub(crate) measuring: Option<crate::hardware_measure::Pending>,
    /// What to show as the last measurement's result, set where the editor is not at hand.
    pub(crate) measure_note: Option<String>,
    #[cfg(test)]
    pub(crate) fake_probe: Option<crate::hardware_measure::FakeProbe>,
    /// Scripted device-loop counters (output, input) standing in for the streams, so tests can
    /// render the Backend lines without a native backend.
    #[cfg(test)]
    pub(crate) injected_backend: Option<(Option<soundcraft_playback::telemetry::StreamStats>, Option<soundcraft_playback::telemetry::StreamStats>)>,
    /// The wake counters sampled at the last panel refreshes (output, input), for the rates.
    wake_rates: (WakeRate, WakeRate),
    /// A native ASIO driver's settings panel that is open; its result arrives when it closes.
    pub(crate) panel: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// The driver whose settings the window's "Driver settings" opened with fake devices (tests).
    pub(crate) panel_opened: Option<String>,
    /// Channel names standing in for the open device's (tests without a player).
    pub(crate) fake_channel_names: Option<crate::audio_devices_io::ChannelNames>,
}

/// The device loop's wake counter at the last sample, and the rate it gave.
#[derive(Default, Clone, Copy)]
pub(crate) struct WakeRate {
    sample: Option<(f64, u64)>,
    rate: Option<u64>,
}

impl WakeRate {
    /// The shortest span between samples a rate is computed over, in seconds; a refresh sooner
    /// than that shows the previous rate.
    const MIN_SPAN: f64 = 0.5;

    /// Take the counter `wakes` read at UI time `now` (seconds); returns the wakes per second
    /// since the previous sample, `None` until there are two. A counter that went down is a new
    /// stream: start over.
    pub(crate) fn update(&mut self, now: f64, wakes: u64) -> Option<u64> {
        match self.sample {
            Some((at, before)) if wakes >= before && now > at => {
                if now - at >= Self::MIN_SPAN {
                    self.rate = Some(((wakes - before) as f64 / (now - at)).round() as u64);
                    self.sample = Some((now, wakes));
                }
            }
            _ => {
                self.rate = None;
                self.sample = Some((now, wakes));
            }
        }
        self.rate
    }
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
        // A listing probes the drivers on the host thread, which the open settings panel holds:
        // keep showing the last list until the panel closes.
        if self.panel.is_some() {
            return;
        }
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
    obj.insert("io_maps".into(), serde_json::to_value(&ui.io_maps).map_err(|e| e.to_string())?);
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
    /// The one-line warning about the path table (`audio_devices_io::warning_text`).
    ///
    /// Cached by (document revision, I/O map revision): the status bar asks every frame, and
    /// finding the unresolved routes plans the sinks.
    pub fn io_warning(&mut self) -> Option<String> {
        let key = (self.engine.revision, self.engine.io_revision());
        if self.io_warning_cache.as_ref().is_none_or(|(k, _)| *k != key) {
            let text = crate::audio_devices_io::warning_text(&self.engine.unresolved_routes(), &self.engine.io_map().overlaps());
            self.io_warning_cache = Some((key, text));
        }
        self.io_warning_cache.as_ref().and_then(|(_, t)| t.clone())
    }

    /// Audio › Audio Devices: open the window with the saved choices.
    pub fn open_audio_devices(&mut self) {
        let s = &mut self.audio_devices;
        s.open = true;
        s.draft = self.ui.audio.clone();
        s.base = Some(self.ui.audio.clone());
        s.forget_listing();
    }

    /// The window's Apply: keep `cfg` as `ui.audio` and reopen the devices with it. Playback stops
    /// first; refused while recording (the take would be cut), leaving `ui.audio` as it was.
    pub fn apply_audio(&mut self, cfg: AudioConfig) -> Result<(), String> {
        validate(&cfg)?;
        if self.recording_active() {
            self.status = REFUSED_RECORDING.into();
            return Err(REFUSED_RECORDING.into());
        }
        let old = std::mem::replace(&mut self.ui.audio, cfg);
        if let Err(error) = self.reopen_audio() {
            self.ui.audio = old;
            return Err(error);
        }
        self.audio_devices.base = Some(self.ui.audio.clone());
        self.audio_devices.draft = self.ui.audio.clone();
        Ok(())
    }

    /// Reopen the devices with `ui.audio` (Apply, `ui.set {audio}`). Without a player
    /// (`--no-audio`) the choice is only saved.
    pub(crate) fn reopen_audio(&mut self) -> Result<(), String> {
        let was_playing = self.is_playing();
        let cfg = self.ui.audio.clone();
        self.audio_devices.input_reported = None;
        let (opened, captured) = if self.player.is_some() || self.audio_devices.fake.is_some() {
            let captured = match self.reconnect_audio_using(true, soundcraft_playback::Player::with_config) {
                Ok(captured) => {
                    self.reopen.reconnected();
                    captured
                }
                Err(error) => {
                    self.player_requests.fail(&mut self.status, error.clone());
                    return Err(error);
                }
            };
            (self.opened(), captured)
        } else {
            self.audio_devices.notes.clear();
            self.status = "Audio devices saved. Audio is off in this run (--no-audio); they open at the next start.".into();
            return Ok(());
        };
        let mut notes = fallback_notes(&cfg, self.audio_devices.systems.as_deref(), &opened);
        notes.extend(self.player.as_ref().and_then(|p| p.backend_note.clone()));
        let stopped = if was_playing { "Playback stopped to change devices. " } else { "" };
        self.status = if notes.is_empty() {
            let name = opened.output.unwrap_or_else(|| "no audio device".into());
            format!("{stopped}Audio devices reopened: {name}.")
        } else {
            format!("{stopped}{} {CHOOSE_ANOTHER}", notes.join(" "))
        };
        if !captured {
            self.status.push_str(" Audio recovered from the document state; live plug-in state was unavailable after the audio fault.");
        }
        self.audio_devices.notes = notes;
        Ok(())
    }

    /// The window's Reconnect: reopen the same choices (a device came back, a driver changed).
    pub fn reconnect_audio_devices(&mut self) {
        if self.recording_active() {
            self.status = REFUSED_RECORDING.into();
            return;
        }
        if self.audio_devices.panel.is_some() {
            self.status = PANEL_OPEN_FIRST.into();
            return;
        }
        let _ = self.reopen_audio();
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

    /// The ASIO driver the native backend has open, if it serves the running device (with fake
    /// devices: the running configuration chose it and this build has it).
    fn running_native_asio(&self) -> Option<String> {
        if !self.running_asio() {
            return None;
        }
        if let Some(f) = &self.audio_devices.fake
            && let Some(p) = &f.connection
        {
            return (p.config.backend.native_on(p.config.system.as_deref())).then(|| f.output_for(&p.config)).flatten();
        }
        self.player.as_ref().filter(|p| p.native_backend && devices::is_asio_system(Some(&p.device_system))).map(|p| p.device_name.clone())
    }

    /// Opens the settings panel of the native ASIO driver that runs (the window's "Driver
    /// settings"): on the backend's host thread, from a thread of its own, so a modal panel does
    /// not hold the UI; the result is said when it closes.
    fn open_native_asio_panel(&mut self, driver: &str) {
        if self.audio_devices.fake.is_some() {
            self.audio_devices.panel_opened = Some(driver.to_owned());
            return;
        }
        match devices::native_asio_control_panel(driver) {
            Ok(rx) => {
                self.audio_devices.panel = Some(rx);
                self.status = format!(
                    "“{driver}” driver settings are open. A driver that asks for a reset is reopened when they close; otherwise Reconnect after changing the buffer size or the rate."
                );
            }
            Err(e) => self.status = format!("ASIO driver settings: {e}"),
        }
    }

    /// Says how the driver settings panel ended, once it has.
    fn poll_native_asio_panel(&mut self) {
        let Some(rx) = &self.audio_devices.panel else { return };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.audio_devices.panel = None;
                self.status = if self.reopen.pending() {
                    "ASIO driver settings closed. The driver asked for a reset: reopening the device.".into()
                } else {
                    "ASIO driver settings closed. If you changed the buffer size or the rate, use Reconnect.".into()
                };
            }
            Ok(Err(e)) => {
                self.audio_devices.panel = None;
                self.status = format!("ASIO driver settings: {e}");
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.audio_devices.panel = None,
        }
    }
}

/// Say once what the saved choices did not get: the output at startup, the input when it opens.
fn check(app: &mut StudioApp) {
    app.poll_native_asio_panel();
    if !app.audio_devices.startup_checked && app.player.is_some() {
        app.audio_devices.startup_checked = true;
        let mut notes = fallback_notes(&app.ui.audio, None, &app.opened());
        notes.extend(app.player.as_ref().and_then(|p| p.backend_note.clone()));
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

/// The "no choice" entry of the Paired input list.
pub const PAIRED_AUTOMATIC: &str = "Automatic (same device)";
/// The button that opens the running native ASIO driver's own settings panel.
pub const DRIVER_SETTINGS: &str = "Driver settings";

/// The `ui.json` the running app saves to, as `audio-bench --settings` should name it.
fn settings_path(app: &StudioApp) -> String {
    app.autosave_dir
        .as_deref()
        .and_then(std::path::Path::parent)
        .map_or_else(|| r"%APPDATA%\transit\ui.json".to_string(), |dir| dir.join("ui.json").display().to_string())
}

/// The checkbox under the Backend selector (native WASAPI): unchecked is shared, the default;
/// checked is exclusive (`share_device` false).
pub const SHARE_LABEL: &str = "Take exclusive control of the device";

/// The one-line hint under the checkbox, for the state it is in.
pub fn share_hint(shared: bool) -> &'static str {
    if shared {
        "Shared: Windows mixes other apps in with transit; the most dependable choice."
    } else {
        "Exclusive: other apps are silenced while transit runs, and latency is only as good as the driver; some drivers open silent."
    }
}

/// The Calibration line under the Paired input: the saved loopback of this exact pair, rate,
/// buffer and mode (the checkbox's: shared or exclusive), or the command that measures it
/// (`device` is the output's name, `settings` the `ui.json` to merge into; close the app before
/// running it). `paired_input` is the input the automatic pairing resolved to (the open Player's,
/// else the open recorder's), `None` when none is open for `device`: a record naming it matches an
/// automatic pairing (see [`devices::input_matches`]).
pub fn calibration_line(cfg: &AudioConfig, device: &str, paired_input: Option<&str>, rate: u32, settings: &str) -> String {
    let period = cfg.buffer_size.unwrap_or(0);
    let mode = if cfg.share_device { devices::MODE_SHARED } else { devices::MODE_EXCLUSIVE };
    if let Some(c) = cfg.find_calibration_paired(device, paired_input, mode, rate, period) {
        return format!("measured {} frames \u{b1}{} ({}) \u{b7} {mode}", c.round_trip_frames, c.spread_frames, c.measured_at);
    }
    let mut command = format!("audio-bench --backend wasapi --loopback --out-device \"{device}\"");
    if !cfg.share_device {
        command.push_str(" --mode exclusive");
    }
    if let Some(input) = cfg.input_pairing.as_deref().or(paired_input) {
        command.push_str(&format!(" --in-device \"{input}\""));
    }
    if period != 0 {
        command.push_str(&format!(" --period {period}"));
    }
    command.push_str(&format!(" --rate {rate} --settings \"{settings}\""));
    format!("not calibrated \u{b7} {mode}: run {command} (with this app closed)")
}

/// The Calibration line under the native ASIO backend: a saved loopback of this driver (its
/// registry name), rate and buffer under the mode key `asio`, else the driver's own figures. The
/// bench writes no ASIO calibration yet: the Focusrite driver's `getLatencies` over-reports (known
/// issue K31), so the stamps carry the driver's figures and a placement can be off by it.
pub fn asio_calibration_line(cfg: &AudioConfig, driver: &str, rate: u32) -> String {
    let period = cfg.buffer_size.unwrap_or(0);
    let mode = devices::MODE_ASIO;
    if let Some(c) = cfg.find_calibration(driver, mode, rate, period) {
        return format!("measured {} frames \u{b1}{} ({}) \u{b7} {mode}", c.round_trip_frames, c.spread_frames, c.measured_at);
    }
    format!(
        "not calibrated \u{b7} {mode}: the driver's own latency figures are used (no loopback calibration over native ASIO yet, see known issue K31)"
    )
}

/// The bench command inside a "not calibrated" [`calibration_line`] (`None` for a measured one).
pub fn calibration_command(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("not calibrated")?.split_once(": run ")?.1;
    Some(rest.strip_suffix(" (with this app closed)").unwrap_or(rest))
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
    choose_or(ui, key, enabled, value, options, items, SYSTEM_DEFAULT)
}

/// [`choose`] with its own words for "no choice".
fn choose_or<T: Clone + PartialEq>(
    ui: &mut egui::Ui,
    key: &str,
    enabled: bool,
    value: &mut Option<T>,
    options: &[(T, String)],
    items: &mut Vec<MenuItem>,
    none: &str,
) -> (egui::Rect, String) {
    let shown = value.as_ref().and_then(|v| options.iter().find(|(o, _)| o == v).map(|(_, t)| t.clone()));
    // A saved choice not offered here (unplugged) still shows, marked.
    let text = match (value.as_ref(), shown) {
        (None, _) => none.to_string(),
        (Some(_), Some(t)) => t,
        (Some(_), None) => "Not available (falls back to the default)".to_string(),
    };
    let r = ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(("audio_devices", key)).selected_text(&text).width(360.0).show_ui(ui, |ui| {
            let b = crate::menus::check_button(ui, value.is_none(), none);
            items.push(MenuItem::at(ui, none.into(), key.into(), b.rect));
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

/// The Backend selector: Auto, cpal, and the native backend of the system (`options`). Returns the
/// combo's rect and text.
fn choose_backend(ui: &mut egui::Ui, enabled: bool, value: &mut Backend, options: &[Backend], items: &mut Vec<MenuItem>) -> (egui::Rect, String) {
    let text = value.label().to_string();
    let r = ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(("audio_devices", "backend")).selected_text(&text).width(360.0).show_ui(ui, |ui| {
            for option in options.iter().copied() {
                let b = crate::menus::check_button(ui, *value == option, option.label());
                items.push(MenuItem::at(ui, option.label().into(), "backend".into(), b.rect));
                if b.clicked() {
                    *value = option;
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
    if let Some(note) = app.audio_devices.measure_note.take() {
        app.audio_devices.io_editor.hw_result = note;
    }
    if !app.audio_devices.open {
        app.cancel_measure("");
        return;
    }
    if app.audio_devices.base.as_ref() != Some(&app.ui.audio) {
        app.audio_devices.draft = app.ui.audio.clone();
        app.audio_devices.base = Some(app.ui.audio.clone());
    }
    if app.audio_devices.systems.is_none() && app.audio_devices.panel.is_none() {
        app.audio_devices.systems = Some(app.listing());
    }
    let systems = app.audio_devices.systems.clone().unwrap_or_default();
    let mut draft = app.audio_devices.draft.clone();
    let mut lay = AudioDevicesLayout { open: true, systems: systems.iter().map(|s| s.name.clone()).collect(), ..Default::default() };
    let recording = app.recording_active();
    let panel_open = app.audio_devices.panel.is_some();
    let audio_on = app.player.is_some() || app.audio_devices.fake.is_some();
    let (mut apply, mut reconnect, mut rescan, mut panel) = (false, false, false, false);
    let mut native_panel: Option<String> = None;
    // The path table is the engine's, read again every frame; one edit at most per frame.
    let io = app.engine.io_map().clone();
    let wasapi = crate::audio_devices_io::is_wasapi(&if io.system.is_empty() {
        pick_system(&systems, app.ui.audio.system.as_deref()).map(|s| s.name.clone()).unwrap_or_default()
    } else {
        io.system.clone()
    });
    // The open device's own channel names, for the table of the device that is open.
    let channel_names = app
        .player
        .as_ref()
        .filter(|p| !p.silent && p.device_name == io.device)
        .map(|p| crate::audio_devices_io::ChannelNames { outputs: p.output_channel_names.clone(), inputs: p.input_channel_names.clone() })
        .or_else(|| app.audio_devices.fake_channel_names.clone())
        .unwrap_or_default();
    let mut editor = std::mem::take(&mut app.audio_devices.io_editor);
    let mut io_edit = None;
    let mut io_items = Vec::new();
    let mut hw_action = None;
    let mut hw_layout = Default::default();
    let blocker = app.measure_blocker();
    let progress = app.measure_progress();
    let rate = app.engine.session().sample_rate.hz();
    let mut open = true;
    let r = egui::Window::new("Audio Devices")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_pos(ctx.content_rect().center() - egui::vec2(260.0, 320.0))
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
                    // Devices belong to their system, and so does a native backend.
                    (draft.output, draft.input, draft.input_pairing) = (None, None, None);
                    if draft.backend.is_native() && !draft.backend.native_on(draft.system.as_deref()) {
                        draft.backend = Backend::Auto;
                    }
                }
                controls.push((text, "system", rect));
                ui.end_row();

                // Offered only where it can apply: a build with the native backend of this system
                // (WASAPI or ASIO).
                if devices::native_offered(draft.system.as_deref()) {
                    label(ui, "Backend");
                    let options = Backend::offered(draft.system.as_deref());
                    let (rect, text) = choose_backend(ui, audio_on, &mut draft.backend, &options, &mut lay.items);
                    controls.push((text, "backend", rect));
                    ui.end_row();
                    if draft.backend == Backend::NativeWasapi && devices::native_wasapi_offered(draft.system.as_deref()) {
                        ui.label("");
                        let mut exclusive = !draft.share_device;
                        let b = ui.add_enabled(audio_on, egui::Checkbox::new(&mut exclusive, SHARE_LABEL));
                        draft.share_device = !exclusive;
                        controls.push((SHARE_LABEL.into(), "share_device", b.rect));
                        ui.end_row();
                        ui.label("");
                        ui.label(egui::RichText::new(share_hint(draft.share_device)).color(TOKENS.text_dim).size(11.0));
                        ui.end_row();
                    }
                }

                label(ui, "Output");
                let outs: Vec<(String, String)> =
                    sys.map(|s| s.outputs.iter().map(|d| (d.name.clone(), device_label(d))).collect()).unwrap_or_default();
                let (rect, text) = choose(ui, "output", audio_on, &mut draft.output, &outs, &mut lay.items);
                controls.push((text, "output", rect));
                ui.end_row();
                // Only the selected driver's own diagnostic is said under the list (a driver that
                // failed to load or open, with why); the others show nothing.
                let selected_output =
                    draft.output.clone().or_else(|| sys.and_then(|s| s.outputs.iter().find(|d| d.is_default)).map(|d| d.name.clone()));
                let selected_problem = selected_output
                    .as_deref()
                    .and_then(|name| sys.and_then(|s| s.diagnostics.iter().find(|p| p.strip_prefix(name).is_some_and(|r| r.starts_with(": ")))));
                if let Some(problem) = selected_problem {
                    ui.label("");
                    let line = short_problem(problem);
                    let shown = ui.add(egui::Label::new(egui::RichText::new(&line).color(TOKENS.text_dim).size(11.0)).wrap());
                    let shown = shown.on_hover_text(problem);
                    controls.push((line.clone(), "diagnostic", shown.rect));
                    lay.diagnostics.push(line);
                    ui.end_row();
                }

                // The chosen output (the system's default output otherwise): its rates and buffers
                // are offered, and its name keys a calibration.
                let dev = sys.and_then(|s| {
                    let named = draft.output.as_deref().and_then(|w| s.outputs.iter().find(|d| d.name == w));
                    named.or_else(|| s.outputs.iter().find(|d| d.is_default))
                });
                let native = devices::native_wasapi_offered(draft.system.as_deref()) && draft.backend == Backend::NativeWasapi;
                let native_asio = devices::native_asio_offered(draft.system.as_deref()) && draft.backend == Backend::NativeAsio;
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

                // The native backend pairs the output with a capture endpoint for recording, and a
                // loopback calibration belongs to that pair.
                if native {
                    label(ui, "Paired input");
                    let (rect, text) = choose_or(ui, "paired_input", audio_on, &mut draft.input_pairing, &ins, &mut lay.items, PAIRED_AUTOMATIC);
                    controls.push((text, "paired_input", rect));
                    ui.end_row();
                    label(ui, "Calibration");
                    let rate = draft.sample_rate.or(app.player.as_ref().map(|p| p.device_rate)).unwrap_or(48_000);
                    let device = dev.map(|d| d.name.as_str()).or(app.player.as_ref().map(|p| p.device_name.as_str())).unwrap_or_default();
                    let open_pair =
                        app.player.as_ref().filter(|p| p.device_name.eq_ignore_ascii_case(device)).and_then(|p| p.paired_input.as_deref());
                    let paired = open_pair.or_else(|| app.recorder.as_ref().map(|r| r.device_name.as_str()));
                    let line = calibration_line(&draft, device, paired, rate, &settings_path(app));
                    // The bench command is long: wrap it inside the panel, with a Copy button for it.
                    ui.vertical(|ui| {
                        ui.set_max_width(ui.available_width());
                        let text = egui::RichText::new(&line).color(TOKENS.text_dim).size(11.5);
                        let shown = ui.add(egui::Label::new(text).wrap());
                        controls.push((line.clone(), "calibration_text", shown.rect));
                        if let Some(command) = calibration_command(&line) {
                            let copy = ui.small_button("Copy").on_hover_text("Copy the command to the clipboard");
                            if copy.clicked() {
                                ui.ctx().copy_text(command.to_string());
                            }
                            controls.push(("Copy".into(), "calibration_copy", copy.rect));
                        }
                    });
                    lay.calibration = Some(line);
                    ui.end_row();
                }
                // Native ASIO: the input is the driver's own, so there is no pairing; a calibration
                // is the driver's (mode `asio`).
                if native_asio {
                    label(ui, "Calibration");
                    let rate = draft.sample_rate.or(app.player.as_ref().map(|p| p.device_rate)).unwrap_or(48_000);
                    let driver = dev.map(|d| d.name.as_str()).or(app.player.as_ref().map(|p| p.device_name.as_str())).unwrap_or_default();
                    let line = asio_calibration_line(&draft, driver, rate);
                    let shown = ui.add(egui::Label::new(egui::RichText::new(&line).color(TOKENS.text_dim).size(11.5)).wrap());
                    controls.push((line.clone(), "calibration_text", shown.rect));
                    lay.calibration = Some(line);
                    ui.end_row();
                }

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
            lay.opened = opened_lines(app, ui.input(|i| i.time));
            // The diagnostics sit in an inset card: the open devices, the backend figures (monospace) and the notes.
            egui::Frame::NONE
                .fill(TOKENS.bg)
                .corner_radius(egui::CornerRadius::same(crate::theme::RADIUS as u8))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in &lay.opened {
                        let text = egui::RichText::new(line).color(TOKENS.text_dim).size(11.5);
                        ui.label(if line.starts_with("Backend") { text.monospace() } else { text });
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
                });
            if recording {
                ui.label(egui::RichText::new(REFUSED_RECORDING).color(TOKENS.record).size(11.5));
            }
            io_edit = crate::audio_devices_io::section(ui, &io, wasapi, &channel_names, &mut editor, &mut io_items);
            (hw_action, hw_layout) = crate::audio_devices_io::hardware_section(ui, &io, rate, blocker, progress, &mut editor, &mut io_items);
            ui.add_space(SPACE);
            ui.horizontal(|ui| {
                let b = ui.add_enabled(!recording && !panel_open, egui::Button::new("Apply"));
                controls.push(("Apply".into(), "apply", b.rect));
                apply = b
                    .on_hover_text("Reopen the devices with these choices. Playback stops first.")
                    .on_disabled_hover_text(if panel_open { PANEL_OPEN_FIRST } else { REFUSED_RECORDING })
                    .clicked();
                let b = ui.add_enabled(!recording && audio_on && !panel_open, egui::Button::new("Reconnect"));
                controls.push(("Reconnect".into(), "reconnect", b.rect));
                reconnect = b
                    .on_hover_text("Reopen the saved devices (after plugging one back in).")
                    .on_disabled_hover_text(if panel_open { PANEL_OPEN_FIRST } else { REFUSED_RECORDING })
                    .clicked();
                let b = ui.add_enabled(audio_on && !panel_open, egui::Button::new("Rescan Devices"));
                controls.push(("Rescan Devices".into(), "rescan", b.rect));
                rescan = b.on_disabled_hover_text(PANEL_OPEN_FIRST).clicked();
                if let Some(driver) = app.running_native_asio() {
                    let b = ui.add_enabled(!panel_open, egui::Button::new(DRIVER_SETTINGS));
                    controls.push((DRIVER_SETTINGS.into(), "panel", b.rect));
                    if b.on_hover_text("The open ASIO driver's own settings (buffer size, clock). Reconnect afterwards.")
                        .on_disabled_hover_text(PANEL_OPEN_FIRST)
                        .clicked()
                    {
                        native_panel = Some(driver);
                    }
                } else if app.running_asio() && (devices::ASIO_CONTROL_PANEL || app.audio_devices.fake.is_some()) {
                    let b = ui.button("Driver Control Panel");
                    controls.push(("Driver Control Panel".into(), "panel", b.rect));
                    panel = b.on_hover_text("The open ASIO driver's own settings. Reconnect afterwards.").clicked();
                }
            });
            lay.controls = controls.into_iter().map(|(text, key, rect)| MenuItem::at(ui, text, key.into(), rect)).collect();
            lay.controls.append(&mut io_items);
        });
    if let Some(r) = r {
        lay.window = r4(r.response.rect);
    }
    lay.io = Some(io);
    lay.hardware = hw_layout;
    lay.draft = draft.clone();
    lay.notes = app.audio_devices.notes.clone();
    app.audio_devices.draft = draft.clone();
    app.audio_devices.layout = lay;
    if let Some(edit) = io_edit {
        match app.run("setup.io_map", edit.params()) {
            Ok(_) => editor.done(&edit),
            Err(error) => app.status = error,
        }
    }
    if let Some(action) = hw_action {
        use crate::audio_devices_io::HardwareAction;
        match action {
            HardwareAction::Measure { send, ret } => {
                editor.hw_result.clear();
                if let Err(why) = app.start_measure(&send, &ret) {
                    app.status.clone_from(&why);
                    editor.hw_result = why;
                }
            }
            HardwareAction::Cancel => {
                editor.hw_result.clear();
                app.cancel_measure("Measurement cancelled.");
            }
            HardwareAction::Clear { send, ret } => {
                editor.hw_result.clear();
                let _ = app.run("io.clear_hardware_latency", serde_json::json!({"send": send, "ret": ret}));
            }
        }
    }
    app.audio_devices.io_editor = editor;
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
    if let Some(driver) = native_panel {
        app.open_native_asio_panel(&driver);
    }
    if !open {
        app.cancel_measure("");
        app.audio_devices.open = false;
        app.audio_devices.layout.open = false;
    }
}

/// What is open now: the output with its rate, channels and buffer, and the input.
fn opened_lines(app: &mut StudioApp, now: f64) -> Vec<String> {
    let mut lines = Vec::new();
    // A native backend reports what it opened: the device sample format and its latency figure.
    let native = app.player.as_ref().is_some_and(|p| p.native_backend);
    match &app.player {
        Some(p) => {
            let buffer = p.device_buffer.map_or("default buffer".to_string(), |b| format!("{b}-sample buffer"));
            let host = if p.native_backend {
                format!(" · {}{}", crate::telemetry::host_name(p), achieved_detail(p.output_sample, p.output_latency, "out"))
            } else {
                String::new()
            };
            lines.push(format!("Output open: {} · {} Hz · {} ch · {buffer}{host}", p.device_name, p.device_rate, p.device_channels));
        }
        None => lines.push("Output open: none".into()),
    }
    let output = app.player.as_ref().and_then(|p| p.backend_stats());
    // The input is the recorder's once a track takes it; until then a native duplex output holds
    // the one it opened with, and the panel reads that.
    let held = app.player.as_ref().and_then(|p| p.with_held_input(|r| (input_line(r, native), r.backend_stats())));
    let input = match &app.recorder {
        Some(r) => r.backend_stats(),
        None => held.as_ref().and_then(|(_, stats)| *stats),
    };
    #[cfg(test)]
    let (output, input) = app.audio_devices.injected_backend.unwrap_or((output, input));
    let (mut out_rate, mut in_rate) = app.audio_devices.wake_rates;
    let output_rate = output.and_then(|s| out_rate.update(now, s.wakes));
    let input_rate = input.and_then(|s| in_rate.update(now, s.wakes));
    let keep = |present: bool, rate: WakeRate| if present { rate } else { WakeRate::default() };
    app.audio_devices.wake_rates = (keep(output.is_some(), out_rate), keep(input.is_some(), in_rate));
    lines.extend(output.map(|s| crate::telemetry::backend_line(&s, false, output_rate)));
    lines.push(match (&app.recorder, &held) {
        (Some(r), _) => input_line(r, native),
        (None, Some((line, _))) => line.clone(),
        (None, None) => "Input open: none yet (it opens when a track is armed or monitored)".into(),
    });
    lines.extend(input.map(|s| crate::telemetry::backend_line(&s, true, input_rate)));
    // After a driver reset: what the automatic reopen did and does next, with its counts.
    lines.extend(app.reopen.status(app.audio_devices.panel.is_some()));
    lines
}

/// The "Input open:" line of an input, the recorder's or the player's held one.
fn input_line(r: &soundcraft_playback::record::Recorder, native: bool) -> String {
    if native {
        let buffer = r.buffer_frames.map_or(String::new(), |b| format!(" · {b}-sample buffer"));
        let detail = achieved_detail(r.sample_format, r.latency, "in");
        format!("Input open: {} · {} Hz · {} ch{buffer}{detail}", r.device_name, r.sample_rate, r.channels)
    } else {
        format!("Input open: {} · {} Hz · {} ch", r.device_name, r.sample_rate, r.channels)
    }
}

/// What a native stream reported at open, for its "open" line: " · i24 · 15.8 ms out" (each part
/// only where reported). `direction` is "out" or "in".
pub fn achieved_detail(sample: Option<impl std::fmt::Debug>, latency: Option<std::time::Duration>, direction: &str) -> String {
    let mut detail = String::new();
    if let Some(sample) = sample {
        detail.push_str(&format!(" · {}", format!("{sample:?}").to_lowercase()));
    }
    if let Some(latency) = latency {
        detail.push_str(&format!(" · {:.1} ms {direction}", latency.as_secs_f64() * 1000.0));
    }
    detail
}

/// "<driver>: <reason>" for a diagnostic, without the repeated `ASIO driver "<driver>": ` prefix.
fn short_problem(problem: &str) -> String {
    if let Some((driver, reason)) = problem.split_once(": ") {
        let prefix = format!("ASIO driver \"{driver}\": ");
        if let Some(rest) = reason.strip_prefix(&prefix) {
            return format!("{driver}: {rest}");
        }
    }
    problem.to_string()
}

/// `audio.fake_devices {systems: [{name, is_default?, outputs: [...], inputs: [...], diagnostics?: [...]}]}`:
/// stand-in devices for screenshot runs and tests (each device `{name, is_default?, max_channels?,
/// sample_rates?}`, each diagnostic a "<driver>: <problem>" text); `{systems: null}` removes them.
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
                diagnostics: s
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|d| d.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut fake = FakeDevices { systems, ..Default::default() };
    fake.connection = Some(crate::audio_connection::FakeConnection::new(AudioConfig::default(), std::rc::Rc::clone(&fake.trace)));
    Ok(Some(fake))
}
