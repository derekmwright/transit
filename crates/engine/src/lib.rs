//! The SoundCraft command engine.
//!
//! Every user-visible action is a registered command ([`cmd::CommandSpec`]): menus, shortcuts, the
//! CLI, the JSON control channel and MCP all dispatch the same commands by id with JSON params.
//! The engine owns the [`Session`] behind an `Arc` (copy-on-write), so undo snapshots are cheap.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod autosave;
pub mod catalog;
pub mod catalog_more;
pub mod clip_group_file;
pub mod cmd;
pub mod demo;
pub mod edit;
pub mod file_access;
pub mod import;
pub mod inspect;
pub mod io;
pub mod retime;
pub mod score;

use serde_json::{Value, json};
use soundcraft_model::{Clip, Session, TrackId, ZoomState};
use soundcraft_time::Samples;
use std::cell::Cell;
use std::sync::Arc;

pub use cmd::{CommandInfo, CommandSpec, command_specs, find_command};
pub use soundcraft_model as model;

/// Shuts the third-party plugin hosts down, running the module exit of every plugin binary they
/// loaded. Call it at the end of `main`, once every plugin instance (mixer, player) is gone: a
/// plugin that has been used can crash when its module exit only runs from an exit handler.
pub fn shutdown_plugin_hosts() {
    soundcraft_vst3_host::shutdown();
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    #[error("unknown command `{0}`")]
    UnknownCommand(String),
    #[error("`{0}` is not available: {1}")]
    Disabled(String, String),
    #[error("bad parameters for `{0}`: {1}")]
    BadParams(String, String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Io(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("internal error in `{0}`: {1}")]
    Internal(String, String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Requests for the transport/audio engine, drained by the host app each frame.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum TransportRequest {
    /// Stop the old session's transport without applying stop-time edits to the new document.
    ResetSession,
    Play,
    Stop,
    TogglePlay,
    Record,
    Locate(Samples),
    PlaySelection,
    Pause,
    HalfSpeed,
    Scrub(Samples),
    AllNotesOff,
}

/// Snapshot of the transport reported back by the host (playback position etc.).
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct TransportStatus {
    pub playing: bool,
    pub recording: bool,
    pub position: Samples,
}

/// Clipboard contents: per-track clip copies relative to the copied range start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Clipboard {
    pub length: Samples,
    pub tracks: Vec<Vec<Clip>>,
    pub automation: Vec<Vec<soundcraft_model::AutomationLane>>,
    /// Copy Special › Clip Gain: per track, (offset from the copied range start, dB) breakpoints.
    pub clip_gain: Vec<Vec<(Samples, f32)>>,
    /// Copy Special › Clip Effects: clip-effect settings (`param` → value) of the first copied clip.
    pub clip_effects: Vec<(String, f64)>,
    /// Markers inside the copied range (starts relative to it), for Paste Special › Merge Markers.
    pub markers: Vec<soundcraft_model::MemoryLocation>,
    /// How far the copied range started after a grid line; Relative Grid pastes keep it.
    pub grid_offset: Samples,
    /// Copy Special › Automation: pasting it leaves the clips alone, in Shuffle mode too.
    pub automation_only: bool,
}

impl Clipboard {
    /// An empty clipboard for material copied from `range`.
    pub fn of_range(s: &Session, range: soundcraft_time::Range) -> Clipboard {
        let line = s.edit.grid.line_at_or_before(range.start, s.sample_rate, &s.tempo, s.frame_rate);
        Clipboard { length: range.len(), grid_offset: range.start.saturating_sub(line), ..Clipboard::default() }
    }
}

/// A tempo change being dragged (see `execute_merged`): the document the gesture started from,
/// the document its last step left, and the journal length after that step.
struct TempoGesture {
    key: String,
    source: Arc<Session>,
    result: Arc<Session>,
    journal_len: usize,
}

#[derive(Debug, Clone)]
struct HistoryEntry {
    label: String,
    doc: Arc<Session>,
}

/// Engine state: the document, history, clipboard and transport plumbing.
pub struct Engine {
    doc: Arc<Session>,
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    /// Undo steps pushed since creation; unlike the history length it still counts at `history_limit`.
    undo_pushes: u64,
    pub history_limit: usize,
    pub clipboard: Clipboard,
    pub transport_requests: Vec<TransportRequest>,
    pub transport: TransportStatus,
    /// Command journal (id, params) for macro recording / agents.
    pub journal: Vec<(String, Value)>,
    /// Path of the session file, if saved.
    pub path: Option<String>,
    /// Revision counter; bumps on every document change but zoom/scroll (UI caches key on it).
    pub revision: u64,
    dirty: bool,
    /// The document as last opened or saved, for `is_dirty`.
    saved: Arc<Session>,
    /// `is_dirty` as of a revision.
    dirty_at: Cell<(u64, bool)>,
    /// Gesture key for undo coalescing (see `execute_merged`).
    merge_key: Option<String>,
    /// The key every merged gesture takes while held (see `hold_merge`).
    merge_hold: Option<String>,
    /// The tempo gesture in progress, so each step retimes from the gesture's start.
    tempo_gesture: Option<TempoGesture>,
    /// While a gesture step runs: the document its retime converts from.
    retime_base: Option<Arc<Session>>,
    /// Set by a command that retimed material for a tempo change.
    retimed: bool,
    /// Messages for the UI status line / agents.
    pub messages: Vec<String>,
    /// The edit selection (range, tracks) before the last change, for Restore Last Selection.
    /// Kept out of the document so selecting never marks the session modified.
    pub last_selection: Option<(soundcraft_time::Range, Vec<TrackId>)>,
    /// Audio imports being prepared on worker threads (L30); emptied when the session is replaced.
    pub imports: import::Imports,
}

/// Logs the routing loops a document holds as it is opened (an older file: the mix renders it
/// with each loop's feedback edge dropped). Control side only, never the audio thread.
fn log_route_loops(s: &Session) {
    let g = s.route_graph();
    for l in g.layering().loops {
        log::warn!("session `{}`: routing loop {}; it plays without its feedback edge", s.name, l.describe(s));
    }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new(Session::default())
    }
}

impl Engine {
    pub fn new(session: Session) -> Self {
        install_panic_hook();
        log_route_loops(&session);
        let doc = Arc::new(session);
        Engine {
            saved: Arc::clone(&doc),
            doc,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_pushes: 0,
            history_limit: 200,
            clipboard: Clipboard::default(),
            transport_requests: Vec::new(),
            transport: TransportStatus::default(),
            journal: Vec::new(),
            path: None,
            revision: 1,
            dirty: false,
            dirty_at: Cell::new((0, false)),
            merge_key: None,
            merge_hold: None,
            tempo_gesture: None,
            retime_base: None,
            retimed: false,
            messages: Vec::new(),
            last_selection: None,
            imports: import::Imports::default(),
        }
    }

    pub fn session(&self) -> &Session {
        &self.doc
    }

    pub fn session_arc(&self) -> Arc<Session> {
        Arc::clone(&self.doc)
    }

    /// Mutable access (copy-on-write). Callers outside commands should prefer `execute`.
    pub fn session_mut(&mut self) -> &mut Session {
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
        Arc::make_mut(&mut self.doc)
    }

    /// Like `session_mut`, for `edit.zoom` only and without bumping `revision`.
    pub fn zoom_mut(&mut self) -> &mut ZoomState {
        self.dirty = true;
        &mut Arc::make_mut(&mut self.doc).edit.zoom
    }

    /// Replace the whole document (open / new); clears history.
    pub fn replace_session(&mut self, s: Session) {
        log_route_loops(&s);
        self.transport = TransportStatus { position: s.edit.selection.start.max(0), ..TransportStatus::default() };
        self.transport_requests.clear();
        self.transport_requests.push(TransportRequest::ResetSession);
        self.doc = Arc::new(s);
        self.saved = Arc::clone(&self.doc);
        self.undo.clear();
        self.redo.clear();
        self.last_selection = None;
        self.imports.clear();
        self.revision = self.revision.wrapping_add(1);
        self.dirty = false;
    }

    /// Whether the document changed since it was opened or saved. Selecting, locating and zooming
    /// don't count, and undoing back to the saved state makes it clean again.
    pub fn is_dirty(&self) -> bool {
        if !self.dirty {
            return false;
        }
        let (rev, dirty) = self.dirty_at.get();
        if rev == self.revision {
            return dirty;
        }
        let dirty = !same_document(&self.saved, &self.doc);
        self.dirty_at.set((self.revision, dirty));
        dirty
    }
    pub fn mark_clean(&mut self) {
        self.dirty = false;
        self.saved = Arc::clone(&self.doc);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|h| h.label.as_str())
    }
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|h| h.label.as_str())
    }
    /// How many undo steps have been pushed so far (a counter to diff around a composite edit).
    pub fn undo_pushes(&self) -> u64 {
        self.undo_pushes
    }
    pub fn undo_history(&self) -> Vec<String> {
        self.undo.iter().map(|h| h.label.clone()).collect()
    }

    pub fn undo(&mut self) -> bool {
        let Some(h) = self.undo.pop() else { return false };
        let cur = std::mem::replace(&mut self.doc, h.doc);
        self.keep_edit_mode(&cur);
        self.keep_runtime_state(&cur);
        self.redo.push(HistoryEntry { label: h.label, doc: cur });
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(h) = self.redo.pop() else { return false };
        let cur = std::mem::replace(&mut self.doc, h.doc);
        self.keep_edit_mode(&cur);
        self.keep_runtime_state(&cur);
        self.undo.push(HistoryEntry { label: h.label, doc: cur });
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
        true
    }

    /// Undo and Redo never change the Edit mode or its settings (Shuffle Lock above all).
    fn keep_edit_mode(&mut self, from: &Session) {
        use edit::{RELATIVE_GRID, SHUFFLE_LOCK, SNAP_TO_GRID};
        let flags = [SNAP_TO_GRID, SHUFFLE_LOCK, RELATIVE_GRID];
        let ed = &self.doc.edit;
        if ed.edit_mode != from.edit.edit_mode || flags.iter().any(|f| ed.flag(f) != from.edit.flag(f)) {
            let ed = &mut Arc::make_mut(&mut self.doc).edit;
            ed.edit_mode = from.edit.edit_mode;
            for f in flags {
                ed.set_flag(f, from.edit.flag(f));
            }
        }
    }

    /// Undo and Redo never change record-arm or input-monitor, which are runtime state: tracks keep
    /// what they have now, whatever the restored snapshot held.
    fn keep_runtime_state(&mut self, from: &Session) {
        let differs = |t: &soundcraft_model::Track| {
            from.track(t.id).is_some_and(|f| f.mixer.record_arm != t.mixer.record_arm || f.mixer.input_monitor != t.mixer.input_monitor)
        };
        if self.doc.tracks.iter().any(differs) {
            for t in &mut Arc::make_mut(&mut self.doc).tracks {
                if let Some(f) = from.track(t.id) {
                    t.mixer.record_arm = f.mixer.record_arm;
                    t.mixer.input_monitor = f.mixer.input_monitor;
                }
            }
        }
    }

    /// Record an undo point manually (for interactive gestures committed by the UI).
    pub fn push_undo(&mut self, label: &str, before: Arc<Session>) {
        if Arc::ptr_eq(&before, &self.doc) {
            return;
        }
        self.undo.push(HistoryEntry { label: label.to_string(), doc: before });
        self.undo_pushes += 1;
        self.redo.clear();
        if self.undo.len() > self.history_limit {
            let excess = self.undo.len() - self.history_limit;
            self.undo.drain(..excess);
        }
    }

    /// Fold the last `n` undo steps into one labeled `label`, for a composite built from several
    /// commands (the oldest step's snapshot is kept, so one undo returns to before all of them).
    /// Clears redo as any new edit would; the document is unchanged.
    pub fn squash_undo(&mut self, n: usize, label: &str) -> Result<()> {
        let bad = |m: String| EngineError::BadParams("edit.squash_undo".into(), m);
        if n == 0 {
            return Err(bad("n must be at least 1".into()));
        }
        if n > self.undo.len() {
            return Err(bad(format!("n is {n} but the history holds {} step(s)", self.undo.len())));
        }
        let keep = self.undo.len() - n;
        self.undo.truncate(keep + 1);
        self.undo[keep].label = label.to_string();
        self.redo.clear();
        self.merge_key = None;
        Ok(())
    }

    /// Whether command `id` must wait for the take being recorded: nothing may replace the
    /// session or quit under it.
    pub fn blocked_while_recording(&self, id: &str) -> bool {
        self.transport.recording && matches!(id, "session.new" | "session.open" | "session.close" | "session.revert" | "app.quit")
    }

    /// Run a command by id. Never panics: escaped panics become `EngineError::Internal` and the
    /// document is restored.
    pub fn execute(&mut self, id: &str, params: &Value) -> Result<Value> {
        let spec = find_command(id).ok_or_else(|| EngineError::UnknownCommand(id.to_string()))?;
        if self.blocked_while_recording(id) {
            return Err(EngineError::Disabled(id.to_string(), "stop recording first".into()));
        }
        if let Err(reason) = (spec.enabled)(self) {
            // A call that names its targets (tracks, clips, a range) does not need a selection;
            // the command itself validates them.
            let names_targets = ["track", "tracks", "clip", "clips", "sources", "start", "end", "at"].iter().any(|k| params.get(k).is_some());
            if !names_targets {
                return Err(EngineError::Disabled(id.to_string(), reason));
            }
        }
        if spec.view {
            return self.execute_view(spec, params);
        }
        let before = Arc::clone(&self.doc);
        let saved = Arc::clone(&self.saved);
        let rev = self.revision;
        let dirty = self.dirty;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (spec.run)(self, params)));
        // L22: no edit may close a routing loop, whichever command makes it (folder moves, track
        // deletes that promote members, presets, imported session data, ...). Opening a document
        // (it replaces the saved one too) may hold one: the mix breaks it and it was logged.
        let result = match result {
            Ok(Ok(v)) if !Arc::ptr_eq(&before, &self.doc) && Arc::ptr_eq(&saved, &self.saved) => {
                match soundcraft_model::new_cycle(&before, &self.doc) {
                    Some(path) => Ok(Err(cmd::bad(
                        id,
                        format!("that route would create a routing loop: {}", soundcraft_model::describe_cycle(&self.doc, &path)),
                    ))),
                    None => Ok(Ok(v)),
                }
            }
            other => other,
        };
        let result = match result {
            Ok(r) => r,
            Err(p) => {
                self.doc = before;
                self.revision = rev.wrapping_add(1);
                self.dirty = dirty;
                return Err(EngineError::Internal(id.to_string(), panic_message(&*p)));
            }
        };
        match result {
            Ok(v) => {
                if !Arc::ptr_eq(&before, &self.doc) {
                    // Only a change to the document ends a gesture, so Stop doesn't split the pass
                    // it ends. A refused command is rolled back below and ends nothing.
                    self.merge_key = None;
                    self.tempo_gesture = None;
                }
                cmd::remember_selection(self, &before);
                if spec.undoable && !Arc::ptr_eq(&before, &self.doc) {
                    self.push_undo(spec.label, before);
                }
                if spec.journal {
                    self.journal.push((id.to_string(), params.clone()));
                }
                Ok(v)
            }
            Err(e) => {
                // Commands must not leave half-applied edits behind.
                if !Arc::ptr_eq(&before, &self.doc) {
                    self.doc = before;
                    self.revision = self.revision.wrapping_add(1);
                    self.dirty = dirty;
                }
                Err(e)
            }
        }
    }

    fn execute_view(&mut self, spec: &CommandSpec, params: &Value) -> Result<Value> {
        let zoom = self.doc.edit.zoom;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (spec.run)(self, params)))
            .unwrap_or_else(|p| Err(EngineError::Internal(spec.id.to_string(), panic_message(&*p))));
        if result.is_err() && self.doc.edit.zoom != zoom {
            Arc::make_mut(&mut self.doc).edit.zoom = zoom;
        }
        result
    }

    /// Execute as part of a continuous gesture (fader drag, knob turn, slider): consecutive calls
    /// with the same `key` collapse into a single undo step.
    pub fn execute_merged(&mut self, id: &str, params: &Value, key: &str) -> Result<Value> {
        let key = self.merge_hold.clone().unwrap_or_else(|| key.to_string());
        let continuing = self.merge_key.as_ref() == Some(&key);
        let before_len = self.undo.len();
        let pushes = self.undo_pushes;
        // A continuing command pushes a step only to have it popped below, so keep room for it:
        // at `history_limit` the push would otherwise drop the oldest step for nothing.
        let limit = self.history_limit;
        if continuing {
            self.history_limit = limit.saturating_add(1);
        }
        // A tempo step converts from the gesture's start while nothing else has moved the material
        // since the last step, so dragging never trims audio for good and rounding never compounds.
        let gesture = self
            .tempo_gesture
            .take()
            .filter(|g| continuing && g.key == key && (Arc::ptr_eq(&g.result, &self.doc) || retime::same_material(&g.result, &self.doc)));
        let before = Arc::clone(&self.doc);
        self.retime_base = gesture.as_ref().map(|g| Arc::clone(&g.source));
        self.retimed = false;
        let r = self.execute(id, params);
        self.retime_base = None;
        self.history_limit = limit;
        let Ok(r) = r else {
            self.tempo_gesture = gesture;
            return r;
        };
        if std::mem::take(&mut self.retimed) {
            let journal_len = self.journal.len();
            // The journal keeps one step per gesture, which replays from the gesture's start.
            if let Some(g) = &gesture
                && journal_len == g.journal_len + 1
                && g.journal_len > 0
                && self.journal.get(g.journal_len - 1).is_some_and(|j| j.0 == id)
            {
                self.journal.remove(g.journal_len - 1);
            }
            let source = gesture.map_or(before, |g| g.source);
            self.tempo_gesture = Some(TempoGesture { key: key.clone(), source, result: Arc::clone(&self.doc), journal_len: self.journal.len() });
        } else if let Some(g) = gesture.filter(|g| retime::same_material(&g.result, &self.doc)) {
            // A step that left the musical material alone (an automation write on a
            // samples-timebase track during a pass, say) keeps the tempo gesture going.
            self.tempo_gesture = Some(TempoGesture { result: Arc::clone(&self.doc), ..g });
        }
        // Whether a step was pushed comes from the push counter, which still moves at the cap
        // where the stack length stays put.
        if continuing && self.undo_pushes == pushes + 1 && before_len > 0 {
            // Drop the step just pushed; the earlier one still holds the pre-gesture document.
            self.undo.pop();
        }
        if self.undo.len() > limit {
            let excess = self.undo.len() - limit;
            self.undo.drain(..excess);
        }
        self.merge_key = Some(key);
        Ok(r)
    }

    /// While `key` is held, every `execute_merged` gesture takes it instead of its own, so they
    /// all join one undo step (an automation pass and whatever is dragged during it).
    pub fn hold_merge(&mut self, key: Option<&str>) {
        if self.merge_hold.as_deref() != key {
            self.merge_hold = key.map(str::to_string);
        }
    }

    /// End a gesture started with `execute_merged`.
    pub fn end_merge(&mut self) {
        self.merge_key = None;
        self.tempo_gesture = None;
    }

    /// For commands that changed the tempo map from `old`: keep musical material on its bars
    /// (see [`retime`]). Within a merged gesture it converts from the gesture's start.
    pub(crate) fn retime_after_tempo(&mut self, id: &str, old: &soundcraft_time::TempoMap) -> Result<()> {
        self.retimed = true;
        let base = self.retime_base.clone();
        let s = self.session_mut();
        let r = match &base {
            Some(b) => {
                retime::restore_material(s, b);
                retime::retime_to_tempo(s, &b.tempo)
            }
            None => retime::retime_to_tempo(s, old),
        };
        r.map_err(|m| EngineError::BadParams(id.to_string(), m))
    }

    /// Convenience: execute with `{}` params.
    pub fn run(&mut self, id: &str) -> Result<Value> {
        self.execute(id, &json!({}))
    }

    pub fn message(&mut self, m: impl Into<String>) {
        let m = m.into();
        log::info!("{m}");
        self.messages.push(m);
        if self.messages.len() > 100 {
            self.messages.remove(0);
        }
    }

    /// Tracks targeted by an edit: the selected tracks, or all tracks with playlists if none.
    pub fn target_tracks(&self) -> Vec<TrackId> {
        let s = self.session();
        if s.edit.selected_tracks.is_empty() {
            Vec::new()
        } else {
            s.tracks.iter().filter(|t| s.edit.selected_tracks.contains(&t.id)).map(|t| t.id).collect()
        }
    }
}

/// Equal apart from what changes without editing: the selection, the playhead and the zoom.
fn same_document(saved: &Session, doc: &Session) -> bool {
    let mut doc = doc.clone();
    for (t, f) in doc.tracks.iter_mut().zip(&saved.tracks) {
        (t.mixer.record_arm, t.mixer.input_monitor) = (f.mixer.record_arm, f.mixer.input_monitor);
    }
    let (from, to) = (&saved.edit, &mut doc.edit);
    to.selection = from.selection;
    to.timeline_selection = from.timeline_selection;
    to.selected_tracks.clone_from(&from.selected_tracks);
    to.selected_clips.clone_from(&from.selected_clips);
    to.playhead = from.playhead;
    to.zoom = from.zoom;
    *saved == doc
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log::error!("panic: {info}\n{}", std::backtrace::Backtrace::force_capture());
            prev(info);
        }));
    });
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn merged_gesture_is_one_undo_step() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Mono, None);
        for db in [-1.0, -2.0, -3.0, -4.0] {
            e.execute_merged("mix.volume", &json!({"track": t.0, "db": db}), "fader").unwrap();
        }
        e.end_merge();
        assert_eq!(e.undo_history().len(), 1);
        e.undo();
        assert_eq!(e.session().track(t).unwrap().mixer.volume_db, 0.0);
    }

    #[test]
    fn squash_undo_folds_the_last_steps_into_one_labeled_step() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Mono, None);
        e.execute("mix.pan", &json!({"track": t.0, "pan": 0.5})).unwrap();
        for db in [-1.0, -2.0, -3.0] {
            e.execute("mix.volume", &json!({"track": t.0, "db": db})).unwrap();
        }
        assert_eq!(e.undo_history().len(), 4);
        e.execute("edit.squash_undo", &json!({"n": 3, "label": "Take"})).unwrap();
        assert_eq!((e.undo_history().len(), e.undo_label()), (2, Some("Take")));
        assert_eq!(e.journal.last().map(|j| j.0.as_str()), Some("edit.squash_undo"));
        e.undo();
        let m = &e.session().track(t).unwrap().mixer;
        assert_eq!((m.volume_db, m.pan.clone()), (0.0, vec![0.5]));
        assert!(e.redo_label() == Some("Take"));
        e.redo();
        assert_eq!(e.session().track(t).unwrap().mixer.volume_db, -3.0);
        e.undo();
        e.execute("mix.pan", &json!({"track": t.0, "pan": 0.1})).unwrap();
        assert!(!e.can_redo(), "squashing or editing clears redo");
    }

    #[test]
    fn squash_undo_clears_redo_and_rejects_bad_counts() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Mono, None);
        assert!(e.squash_undo(1, "x").is_err());
        e.execute("mix.volume", &json!({"track": t.0, "db": -1.0})).unwrap();
        e.execute("mix.volume", &json!({"track": t.0, "db": -2.0})).unwrap();
        e.undo();
        assert!(e.can_redo());
        assert!(e.squash_undo(0, "x").is_err());
        assert!(e.squash_undo(2, "x").is_err());
        assert!(e.execute("edit.squash_undo", &json!({"n": 9, "label": "x"})).is_err());
        assert!(e.can_redo(), "a rejected squash changes nothing");
        e.squash_undo(1, "Solo").unwrap();
        assert_eq!((e.undo_label(), e.can_redo()), (Some("Solo"), false));
    }

    #[test]
    fn a_held_merge_joins_other_gestures_into_its_step() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Mono, None);
        e.hold_merge(Some("pass"));
        for db in [-1.0, -2.0, -3.0] {
            e.execute_merged("mix.volume", &json!({"track": t.0, "db": db}), "fader").unwrap();
            e.execute_merged("mix.pan", &json!({"track": t.0, "pan": db / 4.0}), "pan").unwrap();
        }
        assert_eq!(e.undo_history().len(), 1);
        e.hold_merge(None);
        e.execute_merged("mix.volume", &json!({"track": t.0, "db": -6.0}), "fader").unwrap();
        assert_eq!(e.undo_history().len(), 2);
    }

    #[test]
    fn only_a_change_to_the_document_ends_a_gesture() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Mono, None);
        e.execute_merged("mix.volume", &json!({"track": t.0, "db": -1.0}), "fader").unwrap();
        e.execute("transport.stop", &json!({})).unwrap();
        e.execute_merged("mix.volume", &json!({"track": t.0, "db": -2.0}), "fader").unwrap();
        assert_eq!(e.undo_history().len(), 1);
        e.execute("mix.mute", &json!({"track": t.0})).unwrap();
        e.execute_merged("mix.volume", &json!({"track": t.0, "db": -3.0}), "fader").unwrap();
        assert_eq!(e.undo_history().len(), 3);
    }
}

#[cfg(test)]
mod dirty_tests {
    use super::*;

    #[test]
    fn selecting_locating_and_zooming_leave_the_session_clean() {
        let mut e = crate::demo::demo_engine();
        for (id, p) in [
            ("edit.select", json!({"tracks": ["Kick"], "start": 0, "end": 48_000})),
            ("edit.select_none", json!({})),
            ("transport.locate", json!({"at": 96_000})),
            ("view.zoom_in", json!({})),
            ("view.scroll", json!({"to": 48_000})),
        ] {
            e.execute(id, &p).unwrap();
            assert!(!e.is_dirty(), "{id}");
        }
        e.execute("mix.mute", &json!({"track": "Kick"})).unwrap();
        assert!(e.is_dirty());
        e.undo();
        assert!(!e.is_dirty());
    }
}
