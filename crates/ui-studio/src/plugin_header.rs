//! The header strip of hosted plug-in editor windows (issue #68): what it shows, and what its
//! controls do.
//!
//! The strip itself is drawn by the editor window on the plug-in host thread
//! (`transit_rt::editor_window`); every frame the app sends each open hosted editor its
//! [`EditorHeader`] (sent on only when it changed) and takes the clicks back. A user preset is the
//! plug-in's full state as the format's preset file (`soundcraft_engine::presets`): a save reads
//! the live state on the plug-in host thread, a load loads it into the live instance there, and
//! both are polled once per frame, so the UI never waits for the plug-in. While one runs, or while
//! the plug-in is not responding, the strip is disabled with the reason.
//!
//! A load is one undo step (`plugin.preset_load` / `plugin.state_load`), recorded once the
//! plug-in has taken the state, with the values it reports; the live state before it is stored
//! first, so the undo returns to it. Undoing or redoing that step loads the stored state into the
//! live instance again (`Engine::take_state_restores`). A save is not undoable.

use crate::StudioApp;
use serde_json::{Map, Value, json};
use soundcraft_dsp::{EditorHeader, HeaderAction, JobPoll, LoadedValues, StateJob};
use soundcraft_engine::io::presets as files;
use soundcraft_engine::presets;
use soundcraft_model::{Insert, TrackId, b64};
use std::collections::HashMap;
use std::path::PathBuf;

/// How often the user preset folders are listed again (another host may add presets).
const LIST_EVERY_S: f64 = 2.0;

/// The preset name a new insert starts with (`Insert::new`): shown as no preset.
const NO_PRESET: &str = "<factory default>";

/// The header strip's side of the app (a field of [`StudioApp`]).
#[derive(Default)]
pub struct HeaderState {
    ops: Vec<Op>,
    clipboard: Option<Copied>,
    /// User preset names per plug-in id, with the UI time they were listed.
    lists: HashMap<String, (f64, Vec<String>)>,
}

impl HeaderState {
    /// Whether a preset operation is under way for this slot.
    #[must_use]
    pub fn busy(&self, key: (TrackId, usize)) -> Option<&'static str> {
        self.ops.iter().find(|o| o.key == key).map(|o| o.step.doing())
    }

    /// Whether any preset operation is under way (the frame loop comes back soon).
    #[must_use]
    pub fn any_busy(&self) -> bool {
        !self.ops.is_empty()
    }
}

/// A copied state, pasted only into the same plug-in type.
struct Copied {
    plugin: String,
    state: Vec<u8>,
    name: String,
}

/// Where a loaded state comes from.
enum Source {
    File(PathBuf),
    State { state: Vec<u8>, name: String },
}

/// What to do with a live state once it is read.
enum Then {
    Save(PathBuf),
    Copy,
    /// Store it (so the load's undo returns to it), then load this.
    Load(Source),
}

enum Step {
    Read {
        job: StateJob<Option<Vec<u8>>>,
        then: Then,
    },
    /// A fresh instance's state (Reset to Default).
    Default {
        job: StateJob<Option<Vec<u8>>>,
    },
    Load {
        job: StateJob<Option<LoadedValues>>,
        source: Source,
    },
    /// An undo or redo of a load: the stored state goes live again.
    Restore {
        job: StateJob<Option<LoadedValues>>,
    },
}

impl Step {
    fn doing(&self) -> &'static str {
        match self {
            Step::Read { then: Then::Save(_), .. } => "Saving preset…",
            Step::Read { then: Then::Copy, .. } => "Copying state…",
            Step::Read { then: Then::Load(_), .. } | Step::Load { .. } | Step::Restore { .. } => "Loading preset…",
            Step::Default { .. } => "Loading the default state…",
        }
    }
}

struct Op {
    key: (TrackId, usize),
    plugin: String,
    step: Step,
}

fn slot_json(slot: usize) -> Value {
    if slot == soundcraft_mix::INSTRUMENT_SLOT { json!("instrument") } else { json!(slot) }
}

fn slot_label(slot: usize) -> String {
    if slot == soundcraft_mix::INSTRUMENT_SLOT {
        "Instrument".into()
    } else {
        let letter = u8::try_from(slot).ok().filter(|s| *s < 26).map_or('?', |s| char::from(b'A' + s));
        format!("Insert {letter}")
    }
}

fn values_json(values: &LoadedValues) -> Value {
    Value::Object(values.iter().filter(|(_, v)| v.is_finite()).map(|(id, v)| (id.clone(), json!(v))).collect::<Map<String, Value>>())
}

impl StudioApp {
    /// The hosted plug-in insert (or instrument) at `key` that has preset files.
    fn hosted_insert(&self, key: (TrackId, usize)) -> Option<Insert> {
        let t = self.engine.session().track(key.0)?;
        let ins = if key.1 == soundcraft_mix::INSTRUMENT_SLOT { t.instrument.as_ref() } else { t.mixer.inserts.get(key.1)?.as_ref() }?;
        presets::format_of(&ins.plugin).is_some().then(|| ins.clone())
    }

    /// The plug-in's name: the live instance's own, else the scan caches'.
    fn live_plugin_name(&mut self, key: (TrackId, usize), plugin: &str) -> String {
        match self.player.as_ref().and_then(|p| p.live_name(key.0, key.1)) {
            Some(n) => n.to_owned(),
            None => self.plugin_name(plugin, key.1 == soundcraft_mix::INSTRUMENT_SLOT),
        }
    }

    /// The user presets of `plugin` (listed again every couple of seconds).
    fn user_presets(&mut self, plugin: &str) -> Vec<String> {
        let Some(root) = self.services.config_dir.clone() else { return Vec::new() };
        let now = self.last_frame.unwrap_or_default();
        if let Some((at, list)) = self.plugin_header.lists.get(plugin)
            && now - at < LIST_EVERY_S
            && now >= *at
        {
            return list.clone();
        }
        let list = files::list(&root, plugin);
        self.plugin_header.lists.insert(plugin.to_owned(), (now, list.clone()));
        list
    }

    /// What the header strip of the editor window at `key` shows (`None`: no hosted plug-in with
    /// preset files there).
    pub fn editor_header(&mut self, key: (TrackId, usize)) -> Option<EditorHeader> {
        let ins = self.hosted_insert(key)?;
        let track = self.engine.session().track(key.0)?.name.clone();
        let name = self.live_plugin_name(key, &ins.plugin);
        let user = self.user_presets(&ins.plugin);
        let preset = if ins.preset == NO_PRESET { String::new() } else { ins.preset.clone() };
        let disabled = match self.plugin_header.busy(key) {
            Some(doing) => Some(doing.to_owned()),
            None => self.player.as_ref().and_then(|p| p.editor_available(key.0, key.1).err()),
        };
        Some(EditorHeader {
            title: format!("{track} \u{b7} {} \u{b7} {name}", slot_label(key.1)),
            can_delete: user.contains(&preset),
            preset,
            user,
            factory: Vec::new(),
            bypassed: ins.bypass,
            can_bypass: key.1 != soundcraft_mix::INSTRUMENT_SLOT,
            can_paste: self.plugin_header.clipboard.as_ref().is_some_and(|c| c.plugin == ins.plugin),
            disabled,
        })
    }

    /// Runs what the user did in the header strip of the editor at (`t`, `slot`).
    pub fn header_action(&mut self, t: TrackId, slot: usize, action: HeaderAction) {
        let key = (t, slot);
        let Some(ins) = self.hosted_insert(key) else { return };
        if let Some(doing) = self.plugin_header.busy(key) {
            self.status = format!("{doing} Wait for it to finish.");
            return;
        }
        let plugin = ins.plugin.clone();
        let user = self.user_presets(&plugin);
        let root = self.services.config_dir.clone();
        let user_path = |name: &str| root.as_deref().and_then(|r| presets::path(r, &plugin, name));
        let current = user.iter().position(|n| *n == ins.preset);
        match action {
            HeaderAction::User(i) => {
                if let Some(path) = user.get(i).and_then(|n| user_path(n)) {
                    self.start_load(key, &plugin, Source::File(path));
                }
            }
            HeaderAction::Factory(_) => {}
            HeaderAction::Previous | HeaderAction::Next => {
                if user.is_empty() {
                    self.status = "No user presets yet: Save As… makes one".into();
                    return;
                }
                let n = user.len();
                let i = match (action, current) {
                    (HeaderAction::Next, Some(i)) => (i + 1) % n,
                    (HeaderAction::Next, None) => 0,
                    (_, Some(i)) => (i + n - 1) % n,
                    (_, None) => n - 1,
                };
                if let Some(path) = user.get(i).and_then(|name| user_path(name)) {
                    self.start_load(key, &plugin, Source::File(path));
                }
            }
            HeaderAction::Save => match current.and_then(|i| user.get(i)).and_then(|n| user_path(n)) {
                Some(path) => self.start_read(key, &plugin, Then::Save(path)),
                None => self.header_action(t, slot, HeaderAction::SaveAs),
            },
            HeaderAction::SaveAs => {
                let base = if ins.preset.is_empty() || ins.preset == NO_PRESET { self.live_plugin_name(key, &plugin) } else { ins.preset.clone() };
                let Some(default) = user_path(&base).or_else(|| user_path("Preset")) else {
                    self.status = "Saving a preset needs the settings folder".into();
                    return;
                };
                if let Some(dir) = default.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let Some(pick) = self.services.pick_save.as_ref() else {
                    self.status = "Saving a preset needs a file dialog".into();
                    return;
                };
                if let Some(path) = pick(&default.to_string_lossy()) {
                    let mut path = PathBuf::from(path);
                    if let Some(ext) = presets::extension(&plugin)
                        && !path.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext))
                    {
                        path.set_extension(ext);
                    }
                    self.start_read(key, &plugin, Then::Save(path));
                }
            }
            HeaderAction::Load => {
                if let Some(path) = self.services.pick_open.as_ref().and_then(|pick| pick("plugin.preset")) {
                    self.start_load(key, &plugin, Source::File(PathBuf::from(path)));
                }
            }
            HeaderAction::Bypass => {
                if slot != soundcraft_mix::INSTRUMENT_SLOT
                    && let Err(e) = self.toggle_bypass(t, slot)
                {
                    self.status = e;
                }
            }
            HeaderAction::Reset => {
                let job = self.player.as_ref().map(|p| p.begin_default_state(&plugin, 2));
                match job {
                    Some(Ok(job)) => self.plugin_header.ops.push(Op { key, plugin, step: Step::Default { job } }),
                    Some(Err(e)) => self.status = format!("Reset: {e}"),
                    None => {}
                }
            }
            HeaderAction::Copy => self.start_read(key, &plugin, Then::Copy),
            HeaderAction::Paste => {
                if let Some(c) = self.plugin_header.clipboard.as_ref().filter(|c| c.plugin == plugin) {
                    let source = Source::State { state: c.state.clone(), name: c.name.clone() };
                    self.start_load(key, &plugin, source);
                }
            }
            HeaderAction::Delete => {
                if let Some(path) = current.and_then(|i| user.get(i)).and_then(|n| user_path(n)) {
                    match self.engine.execute("plugin.preset_delete", &json!({"path": path.to_string_lossy()})) {
                        Ok(_) => self.status = format!("Deleted the preset \u{201c}{}\u{201d}", ins.preset),
                        Err(e) => self.status = format!("Preset: {e}"),
                    }
                    self.plugin_header.lists.remove(&plugin);
                }
            }
            HeaderAction::ShowInChain => {
                self.focus(t);
                self.ui.show_devices = true;
            }
        }
    }

    /// Starts reading the live state at `key`.
    fn start_read(&mut self, key: (TrackId, usize), plugin: &str, then: Then) {
        let Some(p) = &self.player else { return };
        match p.begin_save_state(key.0, key.1) {
            Ok(job) => self.plugin_header.ops.push(Op { key, plugin: plugin.to_owned(), step: Step::Read { job, then } }),
            Err(e) => self.status = format!("Preset: {e}"),
        }
    }

    /// Starts a preset load: the live state is read and stored first, then the preset loaded.
    fn start_load(&mut self, key: (TrackId, usize), plugin: &str, source: Source) {
        self.start_read(key, plugin, Then::Load(source));
    }

    /// Loads `state` into the live instance at `key`.
    fn start_live_load(&mut self, key: (TrackId, usize), plugin: String, state: Vec<u8>, source: Source) {
        let Some(p) = &self.player else { return };
        match p.begin_load_state(key.0, key.1, state) {
            Ok(job) => self.plugin_header.ops.push(Op { key, plugin, step: Step::Load { job, source } }),
            Err(e) => self.status = format!("Preset: {e}"),
        }
    }

    /// A finished live read.
    fn read_done(&mut self, key: (TrackId, usize), plugin: String, state: Option<Vec<u8>>, then: Then) {
        let slot = slot_json(key.1);
        match then {
            Then::Save(path) => {
                let Some(state) = state else {
                    self.status = "The plug-in gave no state to save".into();
                    return;
                };
                let p = json!({"track": key.0.0, "slot": slot, "path": path.to_string_lossy(), "state": b64::encode(&state)});
                match self.engine.execute("plugin.preset_save", &p) {
                    Ok(r) => self.status = format!("Saved the preset \u{201c}{}\u{201d}", r["name"].as_str().unwrap_or("")),
                    Err(e) => self.status = format!("Preset: {e}"),
                }
                self.plugin_header.lists.remove(&plugin);
            }
            Then::Copy => match state {
                Some(state) => {
                    let name = self.hosted_insert(key).map(|i| i.preset).filter(|n| n != NO_PRESET).unwrap_or_default();
                    self.plugin_header.clipboard = Some(Copied { plugin, state, name });
                    self.status = "Copied the plug-in's state".into();
                }
                None => self.status = "The plug-in gave no state to copy".into(),
            },
            Then::Load(source) => {
                // The live state goes into the session first, so undoing the load returns to it.
                if let Some(before) = state
                    && self.hosted_insert(key).and_then(|i| i.state_bytes()).as_deref() != Some(before.as_slice())
                    && let Err(e) = self.engine.execute("mix.insert_state", &json!({"track": key.0.0, "slot": slot, "state": b64::encode(&before)}))
                {
                    log::warn!("cannot store the live state before a preset load: {e}");
                }
                let data = match &source {
                    Source::File(path) => match files::read(path, &plugin) {
                        Ok(state) => state,
                        Err(e) => {
                            self.status = format!("Preset: {e}");
                            return;
                        }
                    },
                    Source::State { state, .. } => state.clone(),
                };
                self.start_live_load(key, plugin, data, source);
            }
        }
    }

    /// A finished live load: the session takes the state, its name and the values, as one undo
    /// step.
    fn load_done(&mut self, key: (TrackId, usize), values: Option<LoadedValues>, source: Source) {
        let Some(values) = values else {
            self.status = "The plug-in refused the preset; it keeps its state".into();
            return;
        };
        let slot = slot_json(key.1);
        let r = match source {
            Source::File(path) => self.engine.execute(
                "plugin.preset_load",
                &json!({"track": key.0.0, "slot": slot, "path": path.to_string_lossy(), "params": values_json(&values)}),
            ),
            Source::State { state, name } => self.engine.execute(
                "plugin.state_load",
                &json!({"track": key.0.0, "slot": slot, "state": b64::encode(&state), "name": name, "params": values_json(&values)}),
            ),
        };
        match r {
            Ok(_) => {
                let name = self.hosted_insert(key).map(|i| i.preset).unwrap_or_default();
                self.status = if name.is_empty() { "Loaded the plug-in's state".into() } else { format!("Loaded the preset \u{201c}{name}\u{201d}") };
            }
            Err(e) => self.status = format!("Preset: {e}"),
        }
    }

    /// Per frame (issue #68): live restores after an undo or redo, the preset operations under
    /// way, the clicks in the header strips, and each open hosted editor's header.
    pub(crate) fn drive_plugin_headers(&mut self) {
        if self.player.is_none() {
            self.plugin_header.ops.clear();
            let _ = self.engine.take_state_restores();
            return;
        }
        for key in self.engine.take_state_restores() {
            let Some(state) = self.hosted_insert(key).and_then(|i| i.state_bytes()) else { continue };
            let plugin = self.hosted_insert(key).map(|i| i.plugin).unwrap_or_default();
            if let Some(p) = &self.player
                && let Ok(job) = p.begin_load_state(key.0, key.1, state)
            {
                self.plugin_header.ops.push(Op { key, plugin, step: Step::Restore { job } });
            }
        }
        for mut op in std::mem::take(&mut self.plugin_header.ops) {
            let key = op.key;
            match op.step {
                Step::Read { ref mut job, .. } => match job.poll() {
                    JobPoll::Waiting => self.plugin_header.ops.push(op),
                    JobPoll::Ready(state) => {
                        let Step::Read { then, .. } = op.step else { continue };
                        self.read_done(key, op.plugin, state, then);
                    }
                    JobPoll::Failed(e) => self.status = format!("Preset: {e}"),
                },
                Step::Default { ref mut job } => match job.poll() {
                    JobPoll::Waiting => self.plugin_header.ops.push(op),
                    JobPoll::Ready(Some(state)) => self.start_load(key, &op.plugin, Source::State { state, name: "Default".into() }),
                    JobPoll::Ready(None) => self.status = "The plug-in has no default state to load".into(),
                    JobPoll::Failed(e) => self.status = format!("Reset: {e}"),
                },
                Step::Load { ref mut job, .. } => match job.poll() {
                    JobPoll::Waiting => self.plugin_header.ops.push(op),
                    JobPoll::Ready(values) => {
                        let Step::Load { source, .. } = op.step else { continue };
                        self.load_done(key, values, source);
                    }
                    JobPoll::Failed(e) => self.status = format!("Preset: {e}"),
                },
                Step::Restore { ref mut job } => match job.poll() {
                    JobPoll::Waiting => self.plugin_header.ops.push(op),
                    JobPoll::Ready(Some(values)) => {
                        // The undone (or redone) step's parameters may not match the state it
                        // stored; the plug-in's own values after the load are the ones to keep.
                        let p = json!({"track": key.0.0, "slot": slot_json(key.1), "params": values_json(&values)});
                        if let Err(e) = self.engine.execute("plugin.state_values", &p) {
                            log::warn!("cannot store the plug-in's values after a restore: {e}");
                        }
                    }
                    JobPoll::Ready(None) => self.status = "The plug-in refused its earlier state".into(),
                    JobPoll::Failed(e) => self.status = format!("Could not give the plug-in its earlier state: {e}"),
                },
            }
        }
        let actions = self.player.as_ref().map(soundcraft_playback::Player::take_header_actions).unwrap_or_default();
        for (t, slot, a) in actions {
            self.header_action(t, slot, a);
        }
        for key in self.ui.plugin_windows.clone() {
            if !self.player.as_ref().is_some_and(|p| p.editor_open(key.0, key.1)) {
                continue;
            }
            if let Some(header) = self.editor_header(key)
                && let Some(p) = &self.player
            {
                let _ = p.set_editor_header(key.0, key.1, &header);
            }
        }
    }
}
