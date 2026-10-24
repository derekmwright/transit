//! transit's studio UI: an arrangement window and a transport over the transit engine.
//!
//! "Cubase bones, Ableton manners" (ADR 0008). The UI draws engine state and acts by executing
//! engine commands; audio goes through [`soundcraft_playback::Player`] and the host loop in
//! [`host`], which is a marked copy of `soundcraft-ui-egui`'s `SoundApp` loop (plan 06, section 3).
//! Nothing here depends on `soundcraft-ui-egui`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod about;
pub mod actions;
pub mod arrange;
mod audio_connection;
pub mod audio_devices;
pub mod audio_devices_io;
pub mod automation;
mod automation_picker;
pub mod browser;
pub mod builtin_editor;
pub mod control;
pub mod devices;
pub mod fades;
pub mod fine;
pub mod finish;
pub mod grid;
pub mod groups;
mod hardware_measure;
pub mod hardware_ui;
pub mod host;
pub mod icons;
pub mod import;
mod io_settings;
pub mod keys;
pub mod load_readout;
mod loopbar;
pub mod menus;
pub mod metronome;
mod midi_input;
mod midi_output;
pub mod mixer;
pub mod panel;
pub mod plugin_header;
pub mod plugin_scan;
pub mod presets;
pub mod recent;
pub mod scope_strip;
pub mod shortcuts;
pub mod splash;
pub mod telemetry;
pub mod tempo_edit;
pub mod theme;
pub mod transport;
pub mod update;
pub mod waveform;
pub mod widgets;

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_model::TrackId;
use soundcraft_playback::Player;
use soundcraft_time::{Range, Samples};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;

pub use control::ControlRequest;

/// The most frames `ui.set imports_pace` may stretch a fake prepare over.
const MAX_IMPORTS_PACE: u32 = 600;

/// Our own UI state (serde, so agents can read and set it; saved as `ui.json` in the transit
/// settings folder). Not `soundcraft-ui-egui`'s `UiState` (plan 06, L13).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UiState {
    /// The audio device choice, kept with the preferences: per machine.
    pub audio: soundcraft_playback::AudioConfig,
    /// This machine's I/O paths, one table per audio system plus device (ADR 0015).
    pub io_maps: Vec<soundcraft_model::IoMap>,
    /// File > Open Recent: sessions last opened or saved here, most recent first (at most 10).
    /// A per-machine preference; never part of the document or its undo history.
    pub recent_files: Vec<recent::RecentFile>,
    /// The user's own VST3 and CLAP folders.
    pub plugin_dirs: plugin_scan::PluginDirs,
    /// Plug-in editors to have open, as (track, slot); the instrument is
    /// `soundcraft_mix::INSTRUMENT_SLOT`. `ui.set` opens and closes editors through this list
    /// (the same field `tools/exp-quit.ps1` sets in SoundCraft).
    pub plugin_windows: Vec<(TrackId, usize)>,
    /// Track row height in points (plan 06, L9: ours, not the model's seven sizes).
    pub track_height: f32,
    /// Width of the track list.
    pub header_width: f32,
    /// The main window's size (logical points) and maximized state.
    pub window_size: [f32; 2],
    pub window_maximized: bool,
    /// The mixer as a pane under the arrangement (Ableton's Ctrl+Alt+M).
    pub show_mixer: bool,
    /// The device chain of the focused track under the arrangement (Ableton's detail view, Ctrl+Alt+L).
    pub show_devices: bool,
    /// The mixer fills the window instead of the arrangement (Cubase's F3).
    pub mixer_full: bool,
    /// Height of the mixer pane in points.
    pub mixer_height: f32,
    /// The edit cursor: where Play starts and Paste lands. View state of the open session, not
    /// part of the document (setting it never dirties it) and not saved with the preferences.
    #[serde(skip)]
    pub cursor: Samples,
    /// The track the cursor was last set on (paste target).
    #[serde(skip)]
    pub cursor_track: Option<TrackId>,
    /// The focused track: the one the device chain shows and the mixer and track list highlight.
    /// Follows clicks in both views. View state, never in the document.
    #[serde(skip)]
    pub focus_track: Option<TrackId>,
    /// The piano roll's view preferences (slice 3).
    pub keys: keys::KeysPrefs,
    /// Collapsed groups (slice 2c): their members are out of the track list and the mixer. View
    /// state of the open session, never in the document; shown by `ui.inspect`, never loaded.
    #[serde(skip_deserializing)]
    pub collapsed: Vec<TrackId>,
    /// The browser's view preferences (slice 4): shown, width, tab, locations, favorites.
    pub browser: browser::BrowserPrefs,
    /// View > Appearance (slice 5): System follows the OS; Dark and Light pin the theme.
    pub appearance: theme::Appearance,
    /// Help > Keyboard Shortcuts is open, and its filter (view state, not saved).
    #[serde(skip)]
    pub show_shortcuts: bool,
    #[serde(skip)]
    pub shortcuts_filter: String,
    /// Transport > Count-in: Record clicks `count_in_bars` bars before it starts recording, with
    /// the playhead parked at the start (no room before bar 1 needed). A preference, not part of
    /// the document.
    pub count_in: bool,
    /// How many bars the count-in lasts when it is on: 1, 2 or 4 (the metronome popup).
    pub count_in_bars: u8,
    /// The click's level in dB on top of Setup > Click/Countoff, from the metronome popup. A
    /// preference: it reaches the mixer without touching the document or its undo history.
    pub metronome_db: f32,
    /// Automation > Thin after write (follow-ups): when a write pass ends, `automation.thin`
    /// runs on the tracks it wrote, as part of the pass's undo step. A preference.
    pub thin_after_write: bool,
    /// Help > Check for New Versions on Launch: at most once a day, one anonymous request to
    /// GitHub's public API (see [`update`]). A preference.
    pub check_for_updates: bool,
    /// When the last update check started (seconds since the Unix epoch; 0: never).
    pub last_update_check: u64,
    /// Tracks whose take lanes are shown (slice 6). View state of the open session, never in the
    /// document; the lanes it opens are in `ui.inspect`'s slice 6 layout.
    #[serde(skip)]
    pub takes_open: Vec<TrackId>,
    /// Edit > Undo History is open (view state, not saved).
    #[serde(skip)]
    pub show_history: bool,
    /// Tracks whose automation lanes are shown (slice 7, the A toggle), and the lanes open on
    /// them with their heights. View state of the open session, never in the document; the lanes
    /// as drawn are in `ui.inspect`'s `automation`.
    #[serde(skip)]
    pub auto_shown: Vec<TrackId>,
    #[serde(skip)]
    pub auto_lanes: Vec<automation::LaneView>,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            audio: soundcraft_playback::AudioConfig::default(),
            io_maps: Vec::new(),
            recent_files: Vec::new(),
            plugin_dirs: plugin_scan::PluginDirs::default(),
            plugin_windows: Vec::new(),
            track_height: 46.0,
            header_width: 236.0,
            window_size: [1600.0, 1000.0],
            window_maximized: false,
            show_mixer: false,
            show_devices: true,
            mixer_full: false,
            mixer_height: 420.0,
            cursor: 0,
            cursor_track: None,
            focus_track: None,
            keys: keys::KeysPrefs::default(),
            collapsed: Vec::new(),
            browser: browser::BrowserPrefs::default(),
            appearance: theme::Appearance::System,
            show_shortcuts: false,
            shortcuts_filter: String::new(),
            count_in: false,
            count_in_bars: 1,
            metronome_db: 0.0,
            thin_after_write: true,
            check_for_updates: true,
            last_update_check: 0,
            takes_open: Vec::new(),
            show_history: false,
            auto_shown: Vec::new(),
            auto_lanes: Vec::new(),
        }
    }
}

/// Host services (file dialogs) injected by the app; absent in tests.
#[derive(Default)]
pub struct Services {
    pub pick_open: Option<Box<dyn Fn(&str) -> Option<String>>>,
    pub pick_save: Option<Box<dyn Fn(&str) -> Option<String>>>,
    /// Asks a yes/no question (deleting a plug-in preset, issue #68); `None`: such actions are
    /// refused.
    pub confirm: Option<Box<dyn Fn(&str) -> bool>>,
    /// The app's settings folder; user presets live under `presets/` in it. `None`: factory presets only.
    pub config_dir: Option<std::path::PathBuf>,
    /// Replaces the deferred launch open of the audio device (tests); `None`: the real open.
    pub open_audio: Option<Box<dyn Fn(std::sync::Arc<soundcraft_model::Session>, &soundcraft_playback::AudioConfig) -> Player>>,
    /// Starts a new-version check (the app's, behind its `update-check` feature); `None`: no check.
    pub update_check: Option<update::UpdateCheck>,
    /// The check is off because `TRANSIT_NO_UPDATE_CHECK` is set (says why in the Help menu).
    pub update_off_by_env: bool,
}

/// Ballistic meter display state (copied from `soundcraft-ui-egui`, see [`host::feed_meter`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterDisplay {
    pub level: [f32; 2],
    pub hold: [f32; 2],
    pub hold_age: f32,
    pub clip: bool,
    /// The loudest sample since the last reset (linear), held until the strip's peak readout is
    /// clicked. Fed from the same peaks as `level`.
    pub peak: f32,
    pub gr: f32,
    pub plugin_failures: u32,
}

/// A modal question.
#[derive(Debug, Clone)]
pub enum Dialog {
    /// Save changes before running `then` (session.new/open, app.quit).
    SaveChanges { then: (String, Value) },
    /// Delete several tracks, or tracks that hold clips (Delete key or the track menu).
    DeleteTracks { tracks: Vec<TrackId>, clips: bool },
    /// File > Bounce: its form, then its progress (slice 6, `finish`).
    Bounce,
    /// Delete one of a track's two takes (slice 6): the take lane's Delete asks first.
    DeleteTake { track: TrackId, index: usize },
}

/// An editor asked for (by `ui.set` or a menu) and not open yet: retried until the plug-in is
/// loaded, or given up at `deadline` (seconds of UI time).
#[derive(Debug, Clone, Copy)]
struct EditorWanted {
    key: (TrackId, usize),
    deadline: f64,
    next_try: f64,
    opened: bool,
    /// After a reorder: the player binds the instance to its old slot until the new plan is
    /// adopted, so "not open" is not yet "closed by the user" before this time.
    settle_until: f64,
}

pub struct StudioApp {
    pub engine: Engine,
    pub player: Option<Player>,
    player_requests: host::PlayerRequests,
    /// Input capture, opened on first record.
    pub recorder: Option<soundcraft_playback::record::Recorder>,
    /// Synthetic recorder output for tests that must not open an input device.
    #[cfg(test)]
    finished_test_takes: Option<soundcraft_playback::record::Takes>,
    /// Synthetic MIDI capture for tests that must not run the audio thread.
    #[cfg(test)]
    finished_test_midi: Option<soundcraft_playback::MidiTake>,
    record_start: Samples,
    /// Where the pass under way has got to, counting laps of the cycle: the transport position
    /// wraps, the capture's device time does not.
    record_clock: Option<host::RecordClock>,
    /// The MIDI pass under way as drawn: provisional notes, rebuilt each frame (or every other).
    pub(crate) midi_live: Option<host::MidiLive>,
    midi_live_tick: u32,
    /// The MIDI take journal of the pass under way (crash recovery, K11).
    pub(crate) midi_journal: Option<soundcraft_engine::midi_journal::Writer>,
    /// The journal of a finished pass, until the autosave holds its clips (K13).
    pub(crate) placed_journal: Option<host::PlacedJournal>,
    /// Makes the next Stop see an approximate start, standing in for the recorder's latch.
    #[cfg(test)]
    test_approximate_start: bool,
    /// Punch range to trim the take to (loop recording only in this UI).
    punch: Option<Range>,
    recorder_failed: bool,
    /// When to reopen the device after its driver asked for it (ASIO reset), and what to say.
    pub(crate) reopen: soundcraft_playback::reopen::Reopener,
    /// Opens the replacement Player of an automatic reopen in tests (a scripted driver); the app
    /// uses `Player::with_config`.
    #[cfg(test)]
    pub(crate) reopen_with: Option<Box<dyn Fn(std::sync::Arc<soundcraft_model::Session>, &soundcraft_playback::AudioConfig) -> Player>>,
    lost_shown: u64,
    take_plan: Option<soundcraft_engine::io::Recording>,
    pub ui: UiState,
    /// The status line.
    pub status: String,
    /// The last plug-in state capture left plug-ins out (issue #56): the notice, for a status
    /// that replaces it in the same action (a bounce starting).
    state_notice: Option<String>,
    pub services: Services,
    pub autosave_dir: Option<std::path::PathBuf>,
    autosaver: Option<soundcraft_engine::autosave::Autosaver>,
    autosaved_rev: u64,
    pub leftovers: Vec<soundcraft_engine::autosave::Leftover>,
    panic_rev: u64,
    last_autosave: Option<std::time::Instant>,
    pub main_meter: MeterDisplay,
    pub meters: HashMap<TrackId, MeterDisplay>,
    meter_snapshot: soundcraft_playback::MeterSnapshot,
    last_meter_poll: f64,
    /// Transport simulation when there is no player (tests, `--no-audio`).
    sim: Option<(Samples, Option<Samples>, Option<Range>)>,
    /// The same for a record count-in: samples left, then what to play when it ends.
    sim_count_in: Option<(Samples, (Samples, Option<Samples>, Option<Range>))>,
    /// The running count-in's grid, for the transport's beat counter.
    count_in_plan: Option<soundcraft_playback::CountIn>,
    /// The metronome popup is open.
    pub(crate) metro_popup: bool,
    /// The loop the session asked for at the last frame, so a change while playing retargets the running loop.
    loop_seen: Option<Range>,
    record_hint: Option<(f64, &'static str)>,
    pub quit_requested: bool,
    pub frame_ms: f32,
    last_frame: Option<f64>,
    last_rev: u64,
    /// The opened device still needs its saved I/O table; the engine's table revision last passed on.
    io_adopt: bool,
    io_pushed: u64,
    /// The status-bar path warning, with the (document, I/O map) revisions it was computed for.
    io_warning_cache: Option<((u64, u64), Option<String>)>,
    /// A ResetSession boundary awaiting successful admission or device reopen.
    restore_pending: bool,
    control_rx: Option<Receiver<ControlRequest>>,
    pending_shots: Vec<control::PendingShot>,
    pub synthetic: Vec<egui::Event>,
    synthetic_grace: u32,
    /// Modifiers held by the synthetic gesture being played (`ui.drag {alt: true}`).
    synthetic_mods: Option<egui::Modifiers>,
    /// The appearance last given to egui (re-applied when `ui.appearance` changes).
    theme_applied: Option<theme::Appearance>,
    pub dialog: Option<Dialog>,
    /// The selected tracks were picked by clicking their headers (arrangement header or mixer
    /// strip): only then does Delete take the tracks (Ableton). Any other selection change, such
    /// as a clip click, a lane click or Select All, clears it. View state, not the document.
    pub tracks_selected_by_header: bool,
    editors: Vec<EditorWanted>,
    /// Plug-in host threads not responding, as last reported in the status line (ADR 0018).
    host_wedged_shown: usize,
    /// The hosted editor windows' header strips: preset operations under way (issue #68).
    pub plugin_header: plugin_header::HeaderState,
    /// The arrangement: gesture in progress, scroll, and last frame's layout (for `ui.inspect`).
    pub arrange: arrange::ArrangeState,
    /// What the last Copy took, so a Paste onto the original is refused (UX gap, plan 06).
    pub copied: Vec<(TrackId, Range)>,
    /// The mixer and device chain: gesture in progress, scroll, and last frame's layout.
    pub mixer: mixer::MixerState,
    pub tempo_ui: tempo_edit::TempoUi,
    /// The piano roll (slice 3): open clip, selection, gesture, scroll, last frame's layout.
    pub keys: keys::KeysState,
    /// The browser (slice 4): listings, selection, drag, last frame's layout.
    pub browser: browser::BrowserState,
    /// Audio imports in flight (L30): prepared by the engine's workers, committed in order.
    pub imports: import::ImportQueue,
    /// Help > Keyboard Shortcuts as last drawn (slice 5).
    pub shortcuts_window: menus::ShortcutsLayout,
    /// Help > About transit: open flag and last frame's layout.
    pub about: about::AboutState,
    /// The new-version notice and the check behind it.
    pub update: update::UpdateState,
    /// The launch splash (shown while plug-ins scan or the first device opens).
    pub splash: splash::SplashState,
    /// The output-callback telemetry readout and its window (polled a few times a second).
    pub telemetry: telemetry::TelemetryState,
    /// Slice 6: the name being typed, the bounce dialog and job, take lanes, the undo history.
    pub finish: finish::FinishState,
    /// Slice 7: the automation lanes' gesture, point selection, write pass and last layout.
    pub auto: automation::AutoState,
    /// Audio › Audio Devices: the window, its draft choices and the device listing.
    pub audio_devices: audio_devices::AudioDevicesState,
    /// The Open Recent entries as last drawn (empty while the submenu is closed).
    pub recent_menu: Vec<recent::RecentItemLayout>,
    /// The built-in plug-in editor windows as last drawn.
    pub builtin_editor: builtin_editor::EditorState,
}

impl StudioApp {
    pub fn new(engine: Engine, player: Option<Player>, services: Services) -> Self {
        StudioApp {
            engine,
            player,
            player_requests: host::PlayerRequests::default(),
            recorder: None,
            #[cfg(test)]
            finished_test_takes: None,
            #[cfg(test)]
            finished_test_midi: None,
            record_start: 0,
            record_clock: None,
            midi_live: None,
            midi_live_tick: 0,
            midi_journal: None,
            placed_journal: None,
            #[cfg(test)]
            test_approximate_start: false,
            punch: None,
            recorder_failed: false,
            reopen: soundcraft_playback::reopen::Reopener::default(),
            #[cfg(test)]
            reopen_with: None,
            lost_shown: 0,
            take_plan: None,
            ui: UiState::default(),
            status: String::new(),
            state_notice: None,
            services,
            autosave_dir: None,
            autosaver: None,
            autosaved_rev: 0,
            leftovers: Vec::new(),
            panic_rev: 0,
            last_autosave: None,
            main_meter: MeterDisplay::default(),
            meters: HashMap::new(),
            meter_snapshot: soundcraft_playback::MeterSnapshot::default(),
            last_meter_poll: f64::NEG_INFINITY,
            sim: None,
            sim_count_in: None,
            count_in_plan: None,
            metro_popup: false,
            loop_seen: None,
            record_hint: None,
            quit_requested: false,
            frame_ms: 0.0,
            last_frame: None,
            last_rev: 0,
            io_adopt: true,
            io_pushed: 0,
            io_warning_cache: None,
            restore_pending: false,
            control_rx: None,
            pending_shots: Vec::new(),
            synthetic: Vec::new(),
            synthetic_grace: 0,
            synthetic_mods: None,
            theme_applied: None,
            dialog: None,
            tracks_selected_by_header: false,
            editors: Vec::new(),
            host_wedged_shown: 0,
            plugin_header: plugin_header::HeaderState::default(),
            arrange: arrange::ArrangeState::default(),
            copied: Vec::new(),
            mixer: mixer::MixerState::default(),
            tempo_ui: tempo_edit::TempoUi::default(),
            keys: keys::KeysState::default(),
            browser: browser::BrowserState::default(),
            imports: import::ImportQueue::default(),
            shortcuts_window: menus::ShortcutsLayout::default(),
            about: about::AboutState::default(),
            update: update::UpdateState::default(),
            splash: splash::SplashState::default(),
            telemetry: telemetry::TelemetryState::default(),
            finish: finish::FinishState::default(),
            auto: automation::AutoState::default(),
            audio_devices: audio_devices::AudioDevicesState::default(),
            recent_menu: Vec::new(),
            builtin_editor: builtin_editor::EditorState::default(),
        }
    }

    pub fn with_control(mut self, rx: Receiver<ControlRequest>) -> Self {
        self.control_rx = Some(rx);
        self
    }

    /// The edit cursor: where Play starts and Paste lands. Distinct from the selection
    /// (`edit.selection`); view state in [`UiState`], never in the document.
    pub fn cursor(&self) -> Samples {
        self.ui.cursor.max(0)
    }

    /// The track the cursor was last set on (paste target), if it still exists.
    pub fn cursor_track(&self) -> Option<TrackId> {
        let id = self.ui.cursor_track?;
        self.engine.session().track(id).map(|t| t.id)
    }

    /// Set the cursor (and its track). View state only: the document is untouched.
    pub fn set_cursor(&mut self, at: Samples, track: Option<TrackId>) {
        self.ui.cursor = at.max(0);
        if let Some(t) = track {
            self.ui.cursor_track = Some(t);
        }
        if !self.engine.transport.playing {
            self.engine.transport.position = self.ui.cursor;
        }
    }

    /// Plan 06, L7: this UI offers neither Shuffle nor Spot, so edit commands run as Slip while the
    /// document keeps the mode it was saved with. Only the commands see the override.
    fn slip_override(&self, id: &str) -> bool {
        use soundcraft_model::EditMode;
        let edits = id.starts_with("edit.") || id.starts_with("clip.") || id.starts_with("midi.");
        edits
            && !matches!(id, "edit.mode" | "edit.undo" | "edit.redo")
            && matches!(self.engine.session().edit.edit_mode, EditMode::Shuffle | EditMode::Spot)
    }

    /// Run a command; errors go to the status line. Plug-in state is captured before saves.
    pub fn run(&mut self, id: &str, params: Value) -> Result<Value, String> {
        if matches!(id, "edit.undo" | "edit.redo")
            && let Some(reason) = self.history_disabled_reason()
        {
            self.status = reason.into();
            return Err(reason.into());
        }
        if self.audio_devices.measuring.is_some()
            && matches!(
                id,
                "transport.play"
                    | "transport.toggle"
                    | "transport.record"
                    | "transport.half_speed"
                    | "transport.play_selection"
                    | "edit.play_timeline"
            )
        {
            // Playback would bleed into the return the measurement is listening on.
            self.status = crate::hardware_measure::REFUSED_TRANSPORT.into();
            return Err(self.status.clone());
        }
        if id == "ui.set" {
            return self.set_ui(&params);
        }
        if id == "help.check_updates" {
            self.check_for_updates_now()?;
            return Ok(json!({"checking": true}));
        }
        if id == "io.measure_hardware_latency" {
            return self.measure_hardware_latency(&params);
        }
        if id == "track.clear_clip_indicators" {
            self.clear_clip_latches();
            return Ok(json!({"cleared": true}));
        }
        if finish::refuses(self, id) {
            return Err(self.status.clone());
        }
        if id.starts_with("session.save")
            || id.starts_with("file.bounce")
            || matches!(id, "track.bounce" | "track.commit" | "track.freeze" | "track.flatten")
        {
            // Third-party plugins keep state the session can't see: store it first.
            self.capture_plugin_states()?;
        }
        // Any edit command (selection included) ends a header selection; `focus_header` sets it
        // again after its own `edit.select`.
        if id.starts_with("edit.") || id.starts_with("clip.") || id.starts_with("midi.") {
            self.tracks_selected_by_header = false;
        }
        let replacing = matches!(id, "session.new" | "session.open" | "session.close" | "session.revert");
        if replacing && self.recording_active() {
            self.stop_play();
            self.close_autosave();
        }
        let result = if id == "track.change_width" {
            self.change_track_width(&params)
        } else if self.slip_override(id) {
            self.engine.execute_with_mode(id, &params, soundcraft_model::EditMode::Slip)
        } else {
            self.engine.execute(id, &params)
        };
        match result {
            Ok(v) => {
                if replacing {
                    // Collapsed groups and open take lanes belong to the session that was open.
                    self.ui.collapsed.clear();
                    self.ui.takes_open.clear();
                    self.ui.auto_shown.clear();
                    self.ui.auto_lanes.clear();
                    self.auto.selection.clear();
                    self.auto.highlight = None;
                    self.finish.rename = None;
                }
                if replacing || (id.starts_with("session.save") && !self.engine.is_dirty()) {
                    self.release_autosave(false);
                }
                if matches!(id, "session.open" | "session.save" | "session.save_as") {
                    self.note_recent();
                }
                if id == "app.quit" {
                    self.quit_requested = true;
                }
                Ok(v)
            }
            Err(e) => {
                self.status = e.to_string();
                log::warn!("{id}: {e}");
                Err(e.to_string())
            }
        }
    }

    /// `ui.set`: patch [`UiState`] fields. Plug-in folders are not changed this way.
    fn set_ui(&mut self, p: &Value) -> Result<Value, String> {
        let mut cur = serde_json::to_value(&self.ui).map_err(|e| e.to_string())?;
        if let (Some(obj), Some(patch)) = (cur.as_object_mut(), p.as_object()) {
            for (k, v) in patch {
                if matches!(k.as_str(), "audio" | "keys")
                    && let (Some(current), Some(fields)) = (obj.get_mut(k).and_then(Value::as_object_mut), v.as_object())
                {
                    current.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
                    continue;
                }
                obj.insert(k.clone(), v.clone());
            }
        }
        // A test seam, not state: `imports_pace` fakes a slow prepare (frames per file, at most
        // 600; null off). Applied only once the rest of the request is accepted.
        let pace = match p.get("imports_pace") {
            None => None,
            Some(v) if v.is_null() => Some(None),
            Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n <= MAX_IMPORTS_PACE) {
                Some(n) => Some(Some(n)),
                None => return Err(format!("imports_pace must be null or 0-{MAX_IMPORTS_PACE} frames")),
            },
        };
        let ns: UiState = serde_json::from_value(cur).map_err(|e| format!("bad ui state: {e}"))?;
        audio_devices::validate(&ns.audio)?;
        if ns.plugin_dirs != self.ui.plugin_dirs {
            return Err("plugin folders can't be changed over the control channel".into());
        }
        let audio_changed = ns.audio != self.ui.audio;
        if audio_changed && self.recording_active() {
            return Err(audio_devices::REFUSED_RECORDING.into());
        }
        if let Some(pace) = pace {
            self.imports.pace = pace;
        }
        let mut ns = ns;
        (ns.cursor, ns.cursor_track, ns.focus_track) = (self.ui.cursor, self.ui.cursor_track, self.ui.focus_track);
        ns.collapsed = std::mem::take(&mut self.ui.collapsed);
        ns.takes_open = std::mem::take(&mut self.ui.takes_open);
        ns.auto_shown = std::mem::take(&mut self.ui.auto_shown);
        ns.auto_lanes = std::mem::take(&mut self.ui.auto_lanes);
        let before = std::mem::replace(&mut self.ui, ns);
        // Editors no longer listed close; newly listed ones open (retried until loaded).
        for key in before.plugin_windows.iter().filter(|k| !self.ui.plugin_windows.contains(k)) {
            if let Some(p) = &self.player {
                self.player_requests.result(&mut self.status, p.close_editor(key.0, key.1));
            }
            self.editors.retain(|e| e.key != *key);
        }
        let wanted: Vec<(TrackId, usize)> = self.ui.plugin_windows.clone();
        for key in wanted {
            self.want_editor(key);
        }
        // A new device choice takes effect now, as the window's Apply does.
        if audio_changed && let Err(error) = self.reopen_audio() {
            self.ui.audio = before.audio;
            return Err(error);
        }
        Ok(json!({"ui": self.ui}))
    }

    /// The device card's editor button: closes the editor when it is open (or wanted), else asks for it.
    pub fn toggle_editor(&mut self, key: (TrackId, usize)) {
        if self.ui.plugin_windows.contains(&key) {
            self.close_editor(key);
        } else {
            self.want_editor(key);
        }
    }

    /// Close a plug-in editor window (or stop waiting for it to open).
    pub fn close_editor(&mut self, key: (TrackId, usize)) {
        if self.builtin_at(key).is_some() {
            // A built-in's editor is a window of ours: nothing in the player to close.
            self.ui.plugin_windows.retain(|k| *k != key);
            return;
        }
        if let Some(p) = &self.player
            && self.player_requests.result(&mut self.status, p.close_editor(key.0, key.1)).is_none()
        {
            return;
        }
        self.editors.retain(|e| e.key != key);
        self.ui.plugin_windows.retain(|k| *k != key);
    }

    /// Ask for a plug-in editor window; it opens as soon as the plug-in is loaded.
    pub fn want_editor(&mut self, key: (TrackId, usize)) {
        if !self.ui.plugin_windows.contains(&key) {
            self.ui.plugin_windows.push(key);
        }
        if self.builtin_at(key).is_some() {
            // Drawn by `builtin_editor::windows`; the player has no editor to wait for.
            return;
        }
        if self.editors.iter().any(|e| e.key == key) {
            return;
        }
        let now = self.last_frame.unwrap_or_default();
        self.editors.push(EditorWanted { key, deadline: now + 30.0, next_try: now, opened: false, settle_until: 0.0 });
    }

    /// Open the editors asked for once their plug-ins are loaded; forget the ones the user closed.
    fn drive_editors(&mut self) {
        let now = self.last_frame.unwrap_or_default();
        let p: &dyn audio_connection::Editors = if let Some(fake) = self.audio_devices.fake.as_ref().and_then(|f| f.connection.as_ref()) {
            fake
        } else if let Some(p) = &self.player {
            p
        } else {
            self.editors.clear();
            return;
        };
        let mut keep = Vec::new();
        let mut closed = Vec::new();
        for mut w in std::mem::take(&mut self.editors) {
            let open = p.editor_open(w.key.0, w.key.1);
            if open {
                w.opened = true;
                keep.push(w);
                continue;
            }
            if w.opened && now < w.settle_until {
                keep.push(w);
                continue;
            }
            if w.opened {
                // Closed by the user (or the instance was recreated): no longer wanted.
                closed.push(w.key);
                continue;
            }
            if now >= w.next_try {
                match p.open_editor(w.key.0, w.key.1) {
                    Ok(()) => w.opened = true,
                    Err(e) if now > w.deadline => {
                        self.status = format!("Plug-in editor: {e}");
                        closed.push(w.key);
                        continue;
                    }
                    Err(_) => w.next_try = now + 0.5,
                }
            }
            keep.push(w);
        }
        self.editors = keep;
        self.ui.plugin_windows.retain(|k| !closed.contains(k));
        self.plugin_host_status();
    }

    /// Puts what the plug-in host thread reports (ADR 0018) in the status line: an editor that
    /// failed after its open was accepted, and a plug-in that stopped responding.
    fn plugin_host_status(&mut self) {
        let Some(p) = &self.player else { return };
        for (_, _, e) in p.take_editor_errors() {
            self.status = format!("Plug-in editor: {e}");
        }
        let wedged = p.plugin_host_wedged();
        let stuck = p.any_stuck();
        if wedged > self.host_wedged_shown {
            self.status = if stuck {
                "A plug-in did not finish loading within 10 s and was left out (Not responding); remove it and add it again to retry".into()
            } else {
                "A plug-in is not responding; the live state of the plug-ins on its thread is not saved until it responds again".into()
            };
        } else if wedged == 0 && self.host_wedged_shown > 0 {
            self.status = "The plug-in that was not responding has returned; its state is saved again".into();
        }
        self.host_wedged_shown = wedged;
    }

    pub fn is_playing(&self) -> bool {
        self.player.as_ref().map_or(self.engine.transport.playing, Player::is_playing)
    }

    /// Current transport position: the playhead while playing, else the cursor.
    pub fn position(&self) -> Samples {
        if let Some(p) = &self.player
            && p.is_playing()
        {
            return p.position();
        }
        if self.engine.transport.playing { self.engine.transport.position } else { self.cursor() }
    }

    /// Why the last Record press did nothing, for a few seconds and while it still holds.
    pub fn record_hint(&self) -> Option<&'static str> {
        let (at, why) = self.record_hint?;
        let fresh = self.last_frame.unwrap_or_default() - at < 4.0;
        (fresh && !self.engine.session().tracks.iter().any(|t| t.mixer.record_arm)).then_some(why)
    }

    /// Per-frame logic, once per frame even when egui requests another layout pass.
    ///
    /// Ordering contract:
    /// 1. Drain control requests before polling player replies and editor changes so commands
    ///    can make progress in this frame. Poll before reading meters, MIDI and editor state.
    /// 2. Apply wheel gestures before ending merges; automation owns its merge until Stop.
    /// 3. Finish background bounce/import work before reconciling transport and the document.
    /// 4. Handle transport before publishing a replacement session, and wait for outstanding
    ///    play/locate/record admissions before syncing I/O and session state.
    /// 5. Capture automation before autosaving, then open any newly needed input and schedule
    ///    the next frame. Collect screenshots last, after the frame's state changes.
    pub fn logic(&mut self, ctx: &egui::Context) {
        if ctx.current_pass_index() > 0 {
            return;
        }
        if self.theme_applied != Some(self.ui.appearance) {
            theme::apply(ctx, self.ui.appearance);
            self.theme_applied = Some(self.ui.appearance);
        }
        theme::follow(ctx);
        let now = ctx.input(|i| i.time);
        let dt = self.last_frame.replace(now).map_or(1.0 / 60.0, |t| (now - t) as f32).clamp(0.0, 0.25);
        self.frame_ms = self.frame_ms * 0.9 + dt * 1000.0 * 0.1;
        control::drain(self, ctx);
        let plugin_requests = service_plugin_requests();
        // E7: plug-in editors only work while `Player::idle` runs every frame.
        self.apply_editor_edits();
        self.poll_player();
        self.step_measure();
        if self.audio_devices.measuring.is_some() {
            ctx.request_repaint();
        }
        self.update_midi_live();
        self.step_poll();
        self.drive_editors();
        self.drive_plugin_headers();
        theme::probe_fonts(ctx);
        let wheeling = self.drive_wheel(ctx, now) | self.drive_tempo_wheel(ctx, now);
        // A write pass (slice 7) holds its merge until Stop.
        if !wheeling && !automation::passing(self) && !ctx.input(|i| i.pointer.any_down()) && !ctx.egui_wants_keyboard_input() {
            self.engine.end_merge();
        }
        self.watch_output();
        self.watch_input();
        self.step_reopen();
        self.save_good_audio();
        finish::step_bounce(self, ctx);
        import::step(self, ctx);
        // Stop the old playback before sending a replacement session to the audio thread.
        self.handle_transport(dt);
        automation::write_pass(self, ctx);
        if self.player_requests.play.is_none() && self.player_requests.locate.is_none() && self.player_requests.recording.is_none() {
            self.sync_io();
            self.sync_player_session();
        }
        self.autosave_changes();
        self.autosave();
        if self.recorder.is_none()
            && !self.recorder_failed
            && self.engine.session().tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm)
        {
            self.ensure_input();
        }
        // Autosave only runs on frames, and plugin editors are native windows egui never hears from.
        // egui takes its predicted frame time off every delay, so add it back.
        let editors = self.player.as_ref().is_some_and(Player::any_editor_open)
            || !self.editors.is_empty()
            || self.scope_open()
            || self.plugin_header.any_busy();
        // A plug-in being made is created one frame after it is announced; come back for it.
        let loading = self.player.as_ref().is_some_and(Player::any_loading);
        ctx.request_repaint_after_secs(
            if editors || plugin_requests || loading || self.player_requests.pending() { 0.03 } else { 1.0 } + ctx.input(|i| i.predicted_dt),
        );
        self.player_requests.show_error(&mut self.status);
        control::collect_screenshots(self, ctx);
    }

    /// Feed queued synthetic input (from the control channel) into egui: one pointer event per
    /// frame (so drags span frames like real ones), keys as press+release pairs, text at once.
    /// Copied from `soundcraft-ui-egui` (`SoundApp::raw_input_hook`, transit 2af868e), plus the
    /// modifiers a synthetic gesture holds (`ui.drag {alt: true}`) applied to the frame's input.
    pub fn raw_input_hook(&mut self, raw: &mut egui::RawInput) {
        if !self.synthetic.is_empty() {
            self.synthetic_grace = 6;
        }
        if self.synthetic_grace > 0 {
            self.synthetic_grace -= 1;
            raw.events.retain(|e| {
                !matches!(e, egui::Event::PointerMoved(_) | egui::Event::PointerButton { .. } | egui::Event::PointerGone | egui::Event::MouseMoved(_))
            });
        }
        let Some(first) = self.synthetic.first().cloned() else {
            if let Some(m) = self.synthetic_mods.take() {
                raw.events.insert(0, egui::Event::ModifiersChanged(m));
            }
            return;
        };
        let take = match first {
            egui::Event::Key { pressed: true, .. } => {
                if matches!(self.synthetic.get(1), Some(egui::Event::Key { pressed: false, .. })) {
                    2
                } else {
                    1
                }
            }
            _ => 1,
        };
        let n = take.min(self.synthetic.len());
        let evs: Vec<egui::Event> = self.synthetic.drain(..n).collect();
        let mut release = false;
        for e in &evs {
            match e {
                egui::Event::PointerButton { pos, pressed, modifiers, .. } => {
                    if *pressed {
                        self.synthetic_mods = Some(*modifiers);
                    } else {
                        release = true;
                    }
                    // Keep the hover position current for widgets that check it.
                    raw.events.push(egui::Event::PointerMoved(*pos));
                }
                egui::Event::PointerMoved(pos) => raw.events.push(egui::Event::PointerMoved(*pos)),
                // A key press holds its modifiers for the frame, as a real keyboard reports them.
                egui::Event::Key { pressed, modifiers, .. } => {
                    if *pressed {
                        self.synthetic_mods = Some(*modifiers);
                    } else {
                        release = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(m) = self.synthetic_mods {
            raw.events.insert(0, egui::Event::ModifiersChanged(m));
        }
        if release {
            // Held through this frame's release, then let go on the next.
            self.synthetic_mods = Some(egui::Modifiers::NONE);
        }
        raw.events.extend(evs.into_iter().filter(|e| !matches!(e, egui::Event::PointerMoved(_))));
    }

    /// Lay out the whole window.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        theme::follow(&ctx);
        ui.set_style(ctx.global_style());
        finish::rename_keys(self, &ctx); // slice 6: a name being typed owns Escape and Enter
        tempo_edit::keys(self, &ctx); // the tempo and meter fields own Escape and Enter
        keys::shortcuts(self, &ctx); // slice 3: before the arrangement's keys
        automation::shortcuts(self, &ctx); // slice 7: selected points take Delete before clips
        groups::shortcuts(self, &ctx);
        browser::shortcuts(self, &ctx);
        finish::shortcuts(self, &ctx);
        menus::shortcuts(self, &ctx);
        menus::menu_bar(self, ui);
        transport::bar(self, ui);
        self.update_tick(&ctx);
        menus::status_bar(self, ui);
        browser::panel(self, ui); // slice 4: left of the panes and the arrangement
        // Slice 3: the piano roll replaces the mixer and device chain panes while it is open.
        if !keys::pane(self, ui) {
            mixer::panes(self, ui);
        }
        splash::advance_open(self, &ctx);
        if splash::update(self, &ctx) {
            splash::show(self, ui);
        } else if self.ui.mixer_full {
            mixer::show_full(self, ui);
        } else {
            arrange::show(self, ui);
        }
        import::paint(self, &ctx);
        finish::rename_field(self, &ctx);
        menus::dialogs(self, &ctx);
        finish::bounce_dialog(self, &ctx);
        finish::history_window(self, &ctx);
        menus::recovery(self, &ctx);
        menus::shortcuts_window(self, &ctx);
        about::window(self, &ctx);
        telemetry::window(self, &ctx);
        audio_devices::window(self, &ctx);
        builtin_editor::windows(self, &ctx);
        if self.is_playing() || self.engine.transport.recording {
            ctx.request_repaint_after(std::time::Duration::from_millis(1000 / 30));
        } else if self.main_meter.level[0] > 0.0001 {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        } else if !self.pending_shots.is_empty() || !self.synthetic.is_empty() {
            ctx.request_repaint();
        }
    }

    /// UI state as JSON (control channel `ui.inspect`).
    pub fn inspect(&self, ctx: &egui::Context) -> Value {
        let size = ctx.content_rect().size();
        let open: Vec<(TrackId, usize)> =
            self.player.as_ref().map(|p| self.ui.plugin_windows.iter().copied().filter(|(t, s)| p.editor_open(*t, *s)).collect()).unwrap_or_default();
        json!({
            "ui": self.ui,
            "window_size": [size.x, size.y],
            "playing": self.is_playing(),
            "recording": self.engine.transport.recording,
            "input_open": self.recorder.is_some(),
            "position": self.position(),
            "cursor": self.cursor(),
            "cursor_track": self.cursor_track(),
            "selected_clips": self.engine.session().edit.selected_clips,
            "dialog": self.dialog.as_ref().map(|d| match d {
                Dialog::SaveChanges { .. } => "save_changes",
                Dialog::DeleteTracks { .. } => "delete_tracks",
                Dialog::Bounce => "bounce",
                Dialog::DeleteTake { .. } => "delete_take",
            }),
            "frame_ms": self.frame_ms,
            "audio_device": self.player.as_ref().map(|p| p.device_name.clone()),
            "editors_open": open,
            "main_meter": self.main_meter.level,
            "status": self.status,
            "update_notice": self.update.notice,
            "update_checking": self.update.checking(),
            "layout": self.arrange.layout,
            "focus_track": self.focus_track(),
            "mixer": self.mixer.layout,
            "devices": self.mixer.devices,
            "hardware_picker": self.mixer.hw_picker_layout,
            "track_menu": self.arrange.menu,
            "bar_menu": self.arrange.bar_menu,
            "menu_state": {"filter_rect": self.mixer.filter_rect},
            "readout_edit": self.mixer.edit.as_ref().map(|e| json!({
                "control": format!("{:?}", e.target),
                "text": e.text,
                "rect": self.mixer.edit_rect,
            })),
            "wheel_merge": self.mixer.wheel.is_some(),
            "plugin_scan": plugin_scan::inspect(),
            "keys": self.keys.layout,
            "browser": self.browser.layout,
            "imports": self.imports.layout,
            "shortcuts": self.shortcuts_window,
            "about": self.about,
            "splash": self.splash,
            "telemetry": self.telemetry.layout,
            "finish": self.finish.inspect(),
            "automation": self.auto.layout,
            "audio_devices": self.audio_devices.layout,
            "recent_menu": self.recent_menu,
            "theme": if theme::current() == egui::Theme::Light { "light" } else { "dark" },
        })
    }
}

#[cfg(test)]
mod audio_devices_tests;
#[cfg(test)]
mod automation_picker_tests;
#[cfg(test)]
mod automation_tests;
#[cfg(test)]
mod browser_tests;
#[cfg(test)]
mod builtin_editor_tests;
#[cfg(test)]
mod groups_tests;
#[cfg(test)]
mod keys_tests;
#[cfg(test)]
mod midi_output_header_tests;
#[cfg(test)]
mod midi_record_tests;
#[cfg(test)]
mod param_capture_tests;
#[cfg(test)]
mod recent_tests;
#[cfg(test)]
mod slice5_tests;
#[cfg(test)]
mod slice6_tests;
/// Services what hosted plug-ins asked of the host (audit F08, ADR 0012 G7): CLAP restarts,
/// callbacks and rescans, VST3 `restartComponent`. Main thread, every frame, also with no player
/// and no editor open (the instances were created on this thread). Idempotent: the playback
/// runtime may call the same functions. True while a request is still in progress, so the frame
/// loop comes back soon instead of after its idle second.
fn service_plugin_requests() -> bool {
    let clap = soundcraft_clap_host::service_requests();
    let vst3 = soundcraft_vst3_host::service_requests();
    clap.pending + vst3.pending > 0
}

#[cfg(test)]
mod plugin_header_tests;
#[cfg(test)]
mod plugin_host_tests;
#[cfg(test)]
mod plugin_requests_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod tempo_tests;
#[cfg(test)]
mod transport_tests;
#[cfg(test)]
mod width_history_tests;

#[cfg(test)]
mod about_splash_tests;
