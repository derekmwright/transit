//! The piano roll (key editor, plan 06 slice 3): a MIDI clip opened by a double-click in the
//! arrangement, shown in the bottom pane instead of the mixer and device chain (Ableton's clip
//! view). A tab strip in its header switches back; Ctrl+Alt+K toggles it; Escape closes it.
//!
//! No draw tool (Ableton): a double-click on empty grid adds a note, a double-click on a note
//! deletes it; a click selects, Shift-click adds, a drag on empty grid selects with a rubber band;
//! dragging notes moves them (Ctrl copies), their right edge resizes, Alt+drag or the velocity
//! lane sets velocity. Every edit is a `midi.*` / `event.*` command, one undo step per gesture.
//! Notes are addressed by index and indices shift after each edit (plan 06, section 1), so the
//! selection is kept as note values and resolved to indices again before each command.
//! Zoom, scroll, snap, pane height, new-note velocity and the audition switch are view state: [`KeysPrefs`] in
//! [`crate::UiState`] and [`KeysState`] here, never the document.

mod step;
pub use step::StepState;

use crate::StudioApp;
use crate::arrange::R4;
use crate::mixer::MenuItem;
use crate::theme::{self, TOKENS};
use egui::{Align2, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_midi::Note;
use soundcraft_model::{ClipContent, ClipId, Session, TrackId};
use soundcraft_time::{Samples, TICKS_PER_QUARTER as PPQ};

/// Width of the keyboard column, heights of the pane's rows (points).
pub const KEY_W: f32 = 52.0;
const HANDLE_H: f32 = 5.0;
const HEADER_H: f32 = 28.0;
const RULER_H: f32 = 20.0;
const VEL_H: f32 = 52.0;
/// How close (points) to a note's right edge a press resizes instead of moving.
const EDGE: f32 = 6.0;
/// Pane height limits.
const MIN_H: f32 = 160.0;

/// The grid: note values plus off. Serialized as the label ("1/16", "1/8T", "off").
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Snap {
    #[serde(rename = "1/4")]
    Quarter,
    #[serde(rename = "1/8")]
    Eighth,
    #[serde(rename = "1/16")]
    Sixteenth,
    #[serde(rename = "1/32")]
    ThirtySecond,
    #[serde(rename = "1/8T")]
    EighthTriplet,
    #[serde(rename = "1/16T")]
    SixteenthTriplet,
    #[serde(rename = "off")]
    Off,
}

impl Snap {
    pub const ALL: [Snap; 7] =
        [Snap::Quarter, Snap::Eighth, Snap::Sixteenth, Snap::ThirtySecond, Snap::EighthTriplet, Snap::SixteenthTriplet, Snap::Off];

    pub fn label(self) -> &'static str {
        match self {
            Snap::Quarter => "1/4",
            Snap::Eighth => "1/8",
            Snap::Sixteenth => "1/16",
            Snap::ThirtySecond => "1/32",
            Snap::EighthTriplet => "1/8T",
            Snap::SixteenthTriplet => "1/16T",
            Snap::Off => "Off",
        }
    }

    /// Grid spacing in ticks; `None` when off.
    pub fn ticks(self) -> Option<i64> {
        match self {
            Snap::Quarter => Some(PPQ),
            Snap::Eighth => Some(PPQ / 2),
            Snap::Sixteenth => Some(PPQ / 4),
            Snap::ThirtySecond => Some(PPQ / 8),
            Snap::EighthTriplet => Some(PPQ / 3),
            Snap::SixteenthTriplet => Some(PPQ / 6),
            Snap::Off => None,
        }
    }

    /// The length of a new note and of a nudge: the grid, or a sixteenth when off.
    pub fn step(self) -> i64 {
        self.ticks().unwrap_or(PPQ / 4)
    }
}

/// The piano roll's view preferences (saved with the UI state; never in the document).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct KeysPrefs {
    /// The piano roll replaces the mixer and device chain panes (when a clip is open).
    pub show: bool,
    /// Pane height in points.
    pub height: f32,
    /// Horizontal zoom: points per quarter note (60 = a 4/4 bar in 240 points).
    pub px_per_quarter: f32,
    /// Vertical zoom: key row height in points.
    pub key_h: f32,
    /// Play notes on the clip's instrument as they are placed, clicked or dragged.
    pub audition: bool,
    pub snap: Snap,
    /// Explicit velocity for new notes and audition; note edits never change it.
    #[serde(deserialize_with = "read_velocity")]
    pub velocity: u8,
}

fn read_velocity<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    let value: i64 = serde::Deserialize::deserialize(d)?;
    Ok(value.clamp(1, 127) as u8)
}

impl Default for KeysPrefs {
    fn default() -> Self {
        KeysPrefs { show: false, height: 340.0, px_per_quarter: 60.0, key_h: 16.0, audition: true, snap: Snap::Sixteenth, velocity: 100 }
    }
}

/// What a drag on notes does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum NoteDrag {
    Move,
    Resize,
    /// Alt+drag on a note: velocity by vertical distance.
    Velocity,
    /// A drag in the velocity lane: velocity from the pointer's height.
    Lane,
}

/// A gesture in progress in the pane.
#[derive(Debug, Clone)]
pub enum KeysGesture {
    Notes {
        kind: NoteDrag,
        anchor: Note,
        press: Pos2,
        copy: bool,
        sounding: u8,
    },
    Band {
        press: Pos2,
        additive: bool,
    },
    Height {
        start: f32,
        press: Pos2,
    },
    Ruler {
        ppq: f32,
        anchor: f64,
        press: Pos2,
    },
    Keyboard {
        key_h: f32,
        anchor: f32,
        press: Pos2,
    },
    /// Dragging the clip's end in the ruler (slice 6).
    ClipEnd,
}

/// One note as last drawn (for `ui.inspect`; scripts click and drag them).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NoteRect {
    pub index: usize,
    pub pitch: u8,
    pub start: i64,
    pub length: i64,
    pub velocity: u8,
    pub selected: bool,
    /// Starts at or past the clip's end: it never sounds (drawn dimmed and hatched).
    pub outside: bool,
    pub rect: R4,
    /// The note's stem in the velocity lane.
    pub vel_rect: R4,
}

/// Last frame's piano roll geometry and state.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct KeysLayout {
    pub visible: bool,
    pub clip: Option<u64>,
    pub track: Option<u64>,
    /// The pane takes the keyboard (Delete, arrows, Ctrl+A/D/U, Escape).
    pub active: bool,
    pub pane: R4,
    /// The pane's top edge: drag to resize.
    pub handle: R4,
    pub tab_keys: R4,
    pub tab_mixer: R4,
    pub tab_devices: R4,
    pub snap: R4,
    pub snap_menu: Vec<MenuItem>,
    pub quantize: R4,
    pub audition: R4,
    /// The Step toggle, whether Step entry is on, and its cursor and step length in ticks.
    pub step: R4,
    pub step_on: bool,
    pub step_cursor: i64,
    pub step_ticks: i64,
    pub new_note_velocity: R4,
    pub use_selected_velocity: R4,
    pub ruler: R4,
    pub keyboard: R4,
    pub grid: R4,
    pub velocity: R4,
    /// Mapping: x = grid[0] + (tick - scroll_ticks) * px_per_tick, ticks relative to the clip;
    /// the key `p` spans y = grid[1] + (top_pitch - p) * key_h downwards for key_h points.
    pub px_per_tick: f64,
    pub scroll_ticks: f64,
    pub top_pitch: f32,
    pub key_h: f32,
    /// The clip's start (absolute ticks) and length in ticks.
    pub clip_start_ticks: i64,
    pub clip_ticks: i64,
    /// The clip-end handle in the ruler (slice 6): drag to lengthen or shorten the clip.
    pub end_handle: R4,
    pub grid_ticks: Option<i64>,
    pub notes: Vec<NoteRect>,
    /// Instrument/MIDI tracks with no clips, which show the "create a clip" hint (set by the arrangement).
    pub hint_tracks: Vec<u64>,
    pub selected: usize,
    /// Notes auditioned so far, and the last one as (pitch, velocity): with or without a player.
    pub auditions: u64,
    pub last_audition: Option<(u8, u8)>,
    /// Auditioned pitches still sounding.
    pub sounding: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct KeysState {
    /// The clip being edited.
    pub clip: Option<ClipId>,
    /// Selected notes, by value (indices shift after every edit).
    pub selection: Vec<Note>,
    pub gesture: Option<KeysGesture>,
    /// Left edge of the grid in ticks from the clip start.
    pub scroll: f64,
    /// The pitch whose row is at the grid's top edge (fractional when scrolled).
    pub top: f32,
    /// Center the view on the notes on the next frame (after opening a clip).
    fit: bool,
    pub active: bool,
    /// Auditioned notes still sounding: (track, pitch, earliest release time).
    held: Vec<(TrackId, u8, f64)>,
    pub auditions: u64,
    pub last_audition: Option<(u8, u8)>,
    /// Step entry (session state, never saved).
    pub step: StepState,
    serial: u64,
    /// The last click on the grid (time, position), for double-clicks.
    last_click: Option<(f64, Pos2)>,
    pub layout: KeysLayout,
}

fn r4(r: Rect) -> R4 {
    [r.min.x, r.min.y, r.max.x, r.max.y]
}

fn is_black(p: u8) -> bool {
    matches!(p % 12, 1 | 3 | 6 | 8 | 10)
}

/// The clip's track, start tick, length in ticks, notes and name.
struct ClipView {
    track: TrackId,
    c0: i64,
    len: i64,
    notes: Vec<Note>,
    name: String,
}

fn clip_view(s: &Session, clip: ClipId) -> Option<ClipView> {
    let (track, c) = s.find_clip(clip)?;
    let ClipContent::Midi { sequence } = &c.content else { return None };
    let c0 = s.tempo.samples_to_ticks(c.start, s.sample_rate);
    let len = s.tempo.samples_to_ticks(c.end(), s.sample_rate) - c0;
    Some(ClipView { track, c0, len, notes: sequence.notes.clone(), name: c.name.clone() })
}

/// Indices of `want` in `notes`, each note matched once (identical notes are interchangeable).
pub fn resolve(notes: &[Note], want: &[Note]) -> Vec<usize> {
    let mut used = vec![false; notes.len()];
    let mut out = Vec::new();
    for w in want {
        if let Some(i) = (0..notes.len()).find(|i| !used[*i] && notes[*i] == *w) {
            used[i] = true;
            out.push(i);
        }
    }
    out
}

/// Grid mapping for one frame.
#[derive(Clone, Copy)]
struct Map {
    gx0: f32,
    gy0: f32,
    ppt: f64,
    scroll: f64,
    top: f32,
    kh: f32,
    c0: i64,
    grid: Option<i64>,
}

impl Map {
    fn x(&self, t: i64) -> f32 {
        self.gx0 + ((t as f64 - self.scroll) * self.ppt) as f32
    }
    fn tick(&self, x: f32) -> f64 {
        self.scroll + f64::from(x - self.gx0) / self.ppt
    }
    fn y(&self, p: u8) -> f32 {
        self.gy0 + (self.top - f32::from(p)) * self.kh
    }
    fn pitch(&self, y: f32) -> i32 {
        (self.top - (y - self.gy0) / self.kh).ceil() as i32
    }
    /// The grid line at or before `rel` (ticks from the clip start; the grid is the session's).
    fn floor(&self, rel: f64) -> i64 {
        match self.grid {
            Some(g) => ((self.c0 as f64 + rel) / g as f64).floor() as i64 * g - self.c0,
            None => rel.floor() as i64,
        }
    }
    /// The nearest grid line.
    fn round(&self, rel: f64) -> i64 {
        match self.grid {
            Some(g) => ((self.c0 as f64 + rel) / g as f64).round() as i64 * g - self.c0,
            None => rel.round() as i64,
        }
    }
    fn note_rect(&self, n: &Note) -> Rect {
        let x0 = self.x(n.start);
        let y = self.y(n.pitch);
        Rect::from_min_max(pos2(x0, y + 1.0), pos2(self.x(n.end()).max(x0 + 3.0), y + self.kh - 1.0))
    }
}

impl KeysState {
    /// Pitches of auditioned notes still held.
    #[cfg(test)]
    pub fn held_pitches(&self) -> Vec<u8> {
        self.held.iter().map(|h| h.1).collect()
    }
}

impl StudioApp {
    /// The piano roll is on screen.
    pub fn keys_shown(&self) -> bool {
        self.ui.keys.show && self.keys.clip.is_some() && !self.ui.mixer_full
    }

    /// Open `clip` in the piano roll (MIDI clips only). View state only.
    pub fn open_keys(&mut self, clip: ClipId) -> bool {
        if clip_view(self.engine.session(), clip).is_none() {
            return false;
        }
        if self.keys.clip != Some(clip) {
            self.keys.selection.clear();
            self.keys.fit = true;
        }
        self.keys.clip = Some(clip);
        self.keys.gesture = None;
        self.keys.active = true;
        self.ui.keys.show = true;
        true
    }

    /// Ctrl+Alt+K: show or hide the piano roll; with no clip open, open the selected MIDI clip.
    pub fn toggle_keys(&mut self) {
        if self.keys_shown() {
            self.close_keys();
            return;
        }
        let s = self.engine.session();
        let open = self.keys.clip.filter(|c| clip_view(s, *c).is_some());
        let pick = open.or_else(|| s.edit.selected_clips.iter().copied().find(|c| clip_view(s, *c).is_some()));
        match pick {
            Some(c) => {
                self.open_keys(c);
            }
            None => self.status = "Piano roll: double-click a MIDI clip to edit it.".into(),
        }
    }

    /// Hide the piano roll; the panes shown before it come back.
    pub fn close_keys(&mut self) {
        self.ui.keys.show = false;
        self.keys.gesture = None;
        self.keys.active = false;
        self.release_auditions(f64::INFINITY, false);
    }

    fn keys_track(&self) -> Option<TrackId> {
        self.engine.session().find_clip(self.keys.clip?).map(|(t, _)| t)
    }

    /// Start a gesture's undo step: a key of its own, so it never merges with the one before.
    fn keys_merge_key(&mut self) -> String {
        self.engine.end_merge();
        self.keys.serial += 1;
        format!("keys:{}", self.keys.serial)
    }

    fn keys_exec(&mut self, id: &str, params: Value, key: &str) -> Result<Value, String> {
        self.engine.execute_merged(id, &params, key).map_err(|e| {
            self.status = e.to_string();
            e.to_string()
        })
    }

    fn keys_notes(&self) -> Vec<Note> {
        self.keys.clip.and_then(|c| clip_view(self.engine.session(), c)).map(|v| v.notes).unwrap_or_default()
    }

    /// Play `pitch` at the toolbar velocity (when the audition switch is on).
    pub fn audition(&mut self, pitch: u8, now: f64) {
        if !self.ui.keys.audition {
            return;
        }
        let Some(track) = self.keys_track() else { return };
        let velocity = self.ui.keys.velocity.clamp(1, 127);
        self.keys.auditions += 1;
        self.keys.last_audition = Some((pitch, velocity));
        if let Some(p) = &self.player
            && self.player_requests.result(&mut self.status, p.note(track, pitch, velocity)).is_none()
        {
            return;
        }
        self.keys.held.push((track, pitch, now + 0.15));
    }

    /// Release auditioned notes that have sounded long enough, once the pointer is up.
    fn release_auditions(&mut self, now: f64, pointer_down: bool) {
        let mut keep = Vec::new();
        for (t, pitch, at) in std::mem::take(&mut self.keys.held) {
            if pointer_down || now < at {
                keep.push((t, pitch, at));
            } else if let Some(p) = &self.player
                && self.player_requests.result(&mut self.status, p.note(t, pitch, 0)).is_none()
            {
                keep.push((t, pitch, at));
            }
        }
        self.keys.held = keep;
    }

    /// Add notes to the open clip as one undo step; returns them as stored (the new selection).
    pub fn keys_add(&mut self, notes: &[Note]) -> Result<Vec<Note>, String> {
        let Some(clip) = self.keys.clip else { return Err("no clip".into()) };
        let key = self.keys_merge_key();
        let mut out = Vec::new();
        for n in notes {
            let stored = Note { release_velocity: 64, ..*n };
            self.keys_exec(
                "midi.note_add",
                json!({"clip": clip.0, "pitch": n.pitch, "start_ticks": n.start, "length_ticks": n.length, "velocity": n.velocity, "channel": n.channel}),
                &key,
            )?;
            out.push(stored);
        }
        Ok(out)
    }

    /// Change notes (old value, new value) as one undo step, each found again before its edit.
    pub fn keys_edit(&mut self, pairs: &[(Note, Note)]) -> Result<Vec<Note>, String> {
        let Some(clip) = self.keys.clip else { return Err("no clip".into()) };
        let key = self.keys_merge_key();
        let mut out = Vec::new();
        for (old, new) in pairs {
            if old == new {
                out.push(*new);
                continue;
            }
            let Some(i) = resolve(&self.keys_notes(), &[*old]).first().copied() else { continue };
            self.keys_exec(
                "midi.note_edit",
                json!({"clip": clip.0, "index": i, "pitch": new.pitch, "start_ticks": new.start, "length_ticks": new.length, "velocity": new.velocity}),
                &key,
            )?;
            out.push(*new);
        }
        Ok(out)
    }

    /// Move the selection by `ticks` and `semitones` (clamped so no note leaves the range).
    pub fn keys_move(&mut self, ticks: i64, semitones: i64) -> Result<(), String> {
        let Some(clip) = self.keys.clip else { return Err("no clip".into()) };
        let sel = self.keys.selection.clone();
        let (dt, ds) = clamp_move(&sel, ticks, semitones);
        let idx = resolve(&self.keys_notes(), &sel);
        if idx.is_empty() || (dt == 0 && ds == 0) {
            return Ok(());
        }
        let key = self.keys_merge_key();
        self.keys_exec("midi.notes_move", json!({"clip": clip.0, "indices": idx, "ticks": dt, "semitones": ds}), &key)?;
        self.keys.selection = sel.iter().map(|n| shifted(n, dt, ds)).collect();
        Ok(())
    }

    /// Delete the selected notes (one undo step).
    pub fn keys_delete(&mut self) -> Result<(), String> {
        let Some(clip) = self.keys.clip else { return Err("no clip".into()) };
        let idx = resolve(&self.keys_notes(), &self.keys.selection);
        self.keys.selection.clear();
        if idx.is_empty() {
            return Ok(());
        }
        let key = self.keys_merge_key();
        self.keys_exec("midi.note_delete", json!({"clip": clip.0, "indices": idx}), &key).map(|_| ())
    }

    /// Ctrl+D: copies of the selection right after it (its span rounded up to the grid).
    pub fn keys_duplicate(&mut self) -> Result<(), String> {
        let sel = self.keys.selection.clone();
        let (Some(a), Some(b)) = (sel.iter().map(|n| n.start).min(), sel.iter().map(Note::end).max()) else { return Ok(()) };
        let g = self.ui.keys.snap.step();
        let by = ((b - a + g - 1) / g).max(1) * g;
        let copies: Vec<Note> = sel.iter().map(|n| shifted(n, by, 0)).collect();
        self.keys.selection = self.keys_add(&copies)?;
        Ok(())
    }

    /// Quantize the selection to the grid (all notes when none or all are selected:
    /// `event.quantize`; otherwise the same operation note by note, see leak L23).
    pub fn keys_quantize(&mut self) -> Result<(), String> {
        let Some(clip) = self.keys.clip else { return Err("no clip".into()) };
        let notes = self.keys_notes();
        let g = self.ui.keys.snap.step();
        let opts = soundcraft_midi::ops::QuantizeOptions { grid_ticks: g, ..Default::default() };
        let sel = self.keys.selection.clone();
        let whole = sel.is_empty() || resolve(&notes, &sel).len() == notes.len();
        if whole {
            let key = self.keys_merge_key();
            self.ensure_clip_selected(clip);
            self.keys_exec("event.quantize", json!({"clips": [clip.0], "grid": g}), &key)?;
            if !sel.is_empty() {
                self.keys.selection = self.keys_notes();
            }
            return Ok(());
        }
        let mut q = sel.clone();
        soundcraft_midi::ops::quantize(&mut q, &opts);
        let pairs: Vec<(Note, Note)> = sel.into_iter().zip(q).collect();
        self.keys.selection = self.keys_edit(&pairs)?;
        Ok(())
    }

    /// Ctrl+Up/Down: transpose the selection (all notes when none or all are selected:
    /// `event.transpose`; otherwise `midi.notes_move`).
    pub fn keys_transpose(&mut self, semitones: i64) -> Result<(), String> {
        let Some(clip) = self.keys.clip else { return Err("no clip".into()) };
        let notes = self.keys_notes();
        let sel = self.keys.selection.clone();
        let whole = sel.is_empty() || resolve(&notes, &sel).len() == notes.len();
        if !whole {
            return self.keys_move(0, semitones);
        }
        let key = self.keys_merge_key();
        self.ensure_clip_selected(clip);
        self.keys_exec("event.transpose", json!({"clips": [clip.0], "semitones": semitones}), &key)?;
        if !sel.is_empty() {
            self.keys.selection = self.keys_notes();
        }
        Ok(())
    }

    /// `event.*` commands need a selection to be enabled; the clip being edited is it.
    fn ensure_clip_selected(&mut self, clip: ClipId) {
        let s = self.engine.session();
        if s.edit.selected_clips.is_empty() && s.edit.selected_tracks.is_empty() {
            let _ = self.engine.execute("edit.select", &json!({"clips": [clip.0]}));
        }
    }
}

fn shifted(n: &Note, dt: i64, ds: i64) -> Note {
    Note { start: (n.start + dt).max(0), pitch: (i64::from(n.pitch) + ds).clamp(0, 127) as u8, ..*n }
}

/// Clamp a move so every note stays at or after the clip start and inside 0..=127.
fn clamp_move(sel: &[Note], dt: i64, ds: i64) -> (i64, i64) {
    let min_start = sel.iter().map(|n| n.start).min().unwrap_or(0);
    let lo = sel.iter().map(|n| i64::from(n.pitch)).min().unwrap_or(0);
    let hi = sel.iter().map(|n| i64::from(n.pitch)).max().unwrap_or(127);
    (dt.max(-min_start), ds.clamp(-lo, 127 - hi))
}

/// The piano roll's keys. Runs before the arrangement's shortcuts, so Delete, Ctrl+A/D, the
/// arrows and Escape go to the notes while the pane has the keyboard.
pub fn shortcuts(app: &mut StudioApp, ctx: &egui::Context) {
    use crate::shortcuts::{Action as A, pressed};
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() {
        return;
    }
    if pressed(ctx, A::TogglePianoRoll) {
        app.toggle_keys();
    }
    if !app.keys_shown() || !app.keys.active {
        return;
    }
    if pressed(ctx, A::NotesClose) {
        if app.keys.gesture.take().is_none() {
            app.close_keys();
        }
        return;
    }
    if pressed(ctx, A::StepToggle) {
        app.step_toggle();
    }
    if app.step_active() {
        if pressed(ctx, A::StepBack) {
            app.step_back();
            return;
        }
        if pressed(ctx, A::StepRest) {
            app.step_rest();
        }
        for (action, ticks) in [
            (A::StepWhole, PPQ * 4),
            (A::StepHalf, PPQ * 2),
            (A::StepQuarter, PPQ),
            (A::StepEighth, PPQ / 2),
            (A::StepSixteenth, PPQ / 4),
            (A::StepThirtySecond, PPQ / 8),
        ] {
            if pressed(ctx, action) {
                app.step_set_len(ticks);
            }
        }
    }
    if pressed(ctx, A::NotesSelectAll) {
        app.keys.selection = app.keys_notes();
    }
    if pressed(ctx, A::NotesDuplicate) {
        let _ = app.keys_duplicate();
    }
    if pressed(ctx, A::NotesQuantize) {
        let _ = app.keys_quantize();
    }
    if pressed(ctx, A::NotesTransposeUp) {
        let _ = app.keys_transpose(12);
    }
    if pressed(ctx, A::NotesTransposeDown) {
        let _ = app.keys_transpose(-12);
    }
    let step = app.ui.keys.snap.step();
    let nudges = [
        (A::NotesOctaveUp, 0, 12),
        (A::NotesOctaveDown, 0, -12),
        (A::NotesUp, 0, 1),
        (A::NotesDown, 0, -1),
        (A::NotesEarlier, -step, 0),
        (A::NotesLater, step, 0),
    ];
    for (action, dt, ds) in nudges {
        if pressed(ctx, action) {
            let _ = app.keys_move(dt, ds);
        }
    }
    if pressed(ctx, A::NotesDelete) {
        let _ = app.keys_delete();
    }
}

/// The piano roll pane at the bottom of the window. Returns true when it is shown (it then
/// replaces the mixer and device chain panes). Call before the central panel.
pub fn pane(app: &mut StudioApp, ui: &mut Ui) -> bool {
    // A deleted clip (or a new session) closes the editor.
    if let Some(c) = app.keys.clip
        && clip_view(app.engine.session(), c).is_none()
    {
        app.keys.clip = None;
        app.keys.selection.clear();
        app.keys.gesture = None;
    }
    let now = ui.input(|i| i.time);
    let down = ui.input(|i| i.pointer.any_down());
    // Focus lost: nothing may keep sounding.
    let blurred = ui.input(|i| i.viewport().focused == Some(false));
    app.release_auditions(if blurred { f64::INFINITY } else { now }, down && !blurred);
    if !app.keys_shown() {
        // Hidden by a tab switch or a closed pane: let go of everything.
        app.release_auditions(f64::INFINITY, false);
        app.keys.layout = KeysLayout { auditions: app.keys.auditions, last_audition: app.keys.last_audition, ..KeysLayout::default() };
        return false;
    }
    app.mixer.layout.visible = false;
    app.mixer.devices.visible = false;
    let max_h = (ui.ctx().content_rect().height() - 200.0).max(MIN_H);
    let h = app.ui.keys.height.clamp(MIN_H, max_h);
    egui::Panel::bottom("keys").exact_size(h).frame(egui::Frame::NONE.fill(TOKENS.panel)).show(ui, |ui| editor(app, ui));
    // A pending note-off needs a frame to happen on, even with the mouse still.
    if !app.keys.held.is_empty() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
    }
    true
}

fn editor(app: &mut StudioApp, ui: &mut Ui) {
    let area = ui.max_rect();
    let Some(clip) = app.keys.clip else { return };
    let s = app.engine.session_arc();
    let Some(cv) = clip_view(&s, clip) else { return };
    let now = ui.input(|i| i.time);
    // The pane takes the keyboard after a press inside it, and gives it up after one outside.
    if let Some(p) = ui.input(|i| if i.pointer.any_pressed() { i.pointer.interact_pos() } else { None }) {
        app.keys.active = area.contains(p);
    }
    let mut layout =
        KeysLayout { visible: true, clip: Some(clip.0), track: Some(cv.track.0), pane: r4(area), notes: Vec::new(), ..KeysLayout::default() };
    let handle = Rect::from_min_max(area.min, pos2(area.max.x, area.min.y + HANDLE_H));
    let header = Rect::from_min_max(pos2(area.min.x, handle.max.y), pos2(area.max.x, handle.max.y + HEADER_H));
    let body_top = header.max.y;
    let ruler = Rect::from_min_max(pos2(area.min.x + KEY_W, body_top), pos2(area.max.x, body_top + RULER_H));
    let vel = Rect::from_min_max(pos2(area.min.x + KEY_W, area.max.y - VEL_H), area.max);
    let grid = Rect::from_min_max(pos2(area.min.x + KEY_W, ruler.max.y), pos2(area.max.x, vel.min.y - 1.0));
    let keyboard = Rect::from_min_max(pos2(area.min.x, ruler.max.y), pos2(area.min.x + KEY_W, grid.max.y));
    let corner = Rect::from_min_max(pos2(area.min.x, body_top), pos2(area.min.x + KEY_W, ruler.max.y));
    let vel_label = Rect::from_min_max(pos2(area.min.x, vel.min.y), pos2(area.min.x + KEY_W, area.max.y));

    let prefs = app.ui.keys.clone();
    let kh = prefs.key_h.clamp(6.0, 40.0);
    let ppt = f64::from(prefs.px_per_quarter.clamp(8.0, 800.0)) / PPQ as f64;
    let rows = grid.height() / kh;
    if app.keys.fit {
        app.keys.fit = false;
        let (lo, hi) = (cv.notes.iter().map(|n| n.pitch).min(), cv.notes.iter().map(|n| n.pitch).max());
        let center = match (lo, hi) {
            (Some(a), Some(b)) => (f32::from(a) + f32::from(b)) / 2.0,
            _ => 60.0,
        };
        app.keys.top = (center + rows / 2.0).round();
        app.keys.scroll = 0.0;
    }
    app.keys.top = app.keys.top.clamp((rows - 1.0).min(127.0), 127.0);
    app.keys.scroll = app.keys.scroll.max(-(cv.c0 as f64));
    let step_cur = app.step_cursor(ppt, grid.width());
    let map = Map { gx0: grid.min.x, gy0: grid.min.y, ppt, scroll: app.keys.scroll, top: app.keys.top, kh, c0: cv.c0, grid: prefs.snap.ticks() };
    // Drop selected notes that no longer exist (undo, another view).
    let present = resolve(&cv.notes, &app.keys.selection);
    if present.len() != app.keys.selection.len() {
        app.keys.selection = present.iter().map(|i| cv.notes[*i]).collect();
    }

    header_row(app, ui, header, &cv, &mut layout);
    pane_handle(app, ui, handle, &mut layout);
    paint_grid(ui, &s, &map, grid, ruler, &cv);
    let preview = preview(app, ui, &map, vel, &cv.notes);
    paint_notes(app, ui, &map, grid, vel, &cv.notes, cv.len, preview.as_ref(), &mut layout);
    grid_input(app, ui, &map, grid, vel, &cv, now, &mut layout);
    lane_input(app, ui, &map, vel, &cv, &layout);
    paint_keyboard(app, ui, &map, keyboard);
    keyboard_input(app, ui, &map, keyboard, now);
    ruler_input(app, ui, &map, ruler, &s);
    clip_end_handle(app, ui, &map, ruler, grid, &cv, clip, &mut layout);
    wheel(app, ui, &map, Rect::from_min_max(pos2(area.min.x, body_top), area.max));
    // Overlays: rubber band, cursor and playhead.
    let p = ui.painter().with_clip_rect(Rect::from_min_max(ruler.min, vel.max));
    if let Some(KeysGesture::Band { press, .. }) = app.keys.gesture
        && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
    {
        let band = Rect::from_two_pos(press, pos);
        p.rect_filled(band, 0.0, TOKENS.accent.gamma_multiply(0.15));
        p.rect_stroke(band, 0.0, Stroke::new(1.0, TOKENS.accent), StrokeKind::Inside);
    }
    let to_rel = |at: Samples| s.tempo.samples_to_ticks(at, s.sample_rate) - cv.c0;
    let cx = map.x(to_rel(app.cursor()));
    p.line_segment([pos2(cx, ruler.min.y), pos2(cx, vel.max.y)], Stroke::new(1.0, TOKENS.cursor.gamma_multiply(0.6)));
    if let Some((at, len)) = step_cur {
        let (x0, x1) = (map.x(at), map.x(at + len));
        let band = Rect::from_min_max(pos2(x0, ruler.min.y), pos2(x1.max(x0 + 2.0), vel.max.y));
        p.rect_filled(band, 0.0, TOKENS.accent.gamma_multiply(0.18));
        p.line_segment([pos2(x0, ruler.min.y), pos2(x0, vel.max.y)], Stroke::new(2.0, TOKENS.accent));
        p.add(egui::Shape::convex_polygon(
            vec![pos2(x0 - 5.0, ruler.min.y), pos2(x0 + 5.0, ruler.min.y), pos2(x0, ruler.min.y + 8.0)],
            TOKENS.accent,
            Stroke::NONE,
        ));
        layout.step_cursor = at;
        layout.step_ticks = len;
        ui.ctx().request_repaint();
    }
    layout.step_on = step_cur.is_some();
    if app.is_playing() {
        let rel = to_rel(app.engine.transport.position);
        let px = map.x(rel);
        p.line_segment([pos2(px, ruler.min.y), pos2(px, vel.max.y)], Stroke::new(1.5, TOKENS.accent));
        // Page the view so the playhead stays on screen; the editor stays on its clip.
        let w = f64::from(grid.width()) / ppt;
        if app.keys.gesture.is_none() && ((rel as f64) > app.keys.scroll + w * 0.97 || (rel as f64) < app.keys.scroll) {
            app.keys.scroll = rel as f64 - w * 0.05;
        }
        ui.ctx().request_repaint();
    }
    let lp = ui.painter();
    lp.rect_filled(corner, 0.0, TOKENS.ruler);
    lp.rect_filled(vel_label, 0.0, TOKENS.panel);
    lp.text(pos2(vel_label.min.x + 8.0, vel_label.min.y + 10.0), Align2::LEFT_CENTER, "Velocity", FontId::proportional(10.5), TOKENS.text_dim);
    lp.line_segment([pos2(area.min.x, vel.min.y - 0.5), pos2(area.max.x, vel.min.y - 0.5)], Stroke::new(1.0, TOKENS.line));

    layout.active = app.keys.active;
    layout.ruler = r4(ruler);
    layout.keyboard = r4(keyboard);
    layout.grid = r4(grid);
    layout.velocity = r4(vel);
    layout.px_per_tick = ppt;
    layout.scroll_ticks = app.keys.scroll;
    layout.top_pitch = app.keys.top;
    layout.key_h = kh;
    layout.clip_start_ticks = cv.c0;
    layout.clip_ticks = cv.len;
    layout.grid_ticks = prefs.snap.ticks();
    layout.selected = app.keys.selection.len();
    layout.auditions = app.keys.auditions;
    layout.last_audition = app.keys.last_audition;
    layout.sounding = app.keys.held.iter().map(|h| h.1).collect();
    app.keys.layout = layout;
}

/// Tabs (Piano Roll, Mixer, Devices), the clip's name, grid, Quantize and the audition switch.
fn header_row(app: &mut StudioApp, ui: &mut Ui, r: Rect, cv: &ClipView, layout: &mut KeysLayout) {
    let p = ui.painter();
    p.rect_filled(r, 0.0, TOKENS.panel);
    p.line_segment([pos2(r.min.x, r.max.y - 0.5), pos2(r.max.x, r.max.y - 0.5)], Stroke::new(1.0, TOKENS.line));
    let mut x = r.min.x + 8.0;
    let font = FontId::proportional(12.0);
    for (label, which) in [("Piano Roll", 0), ("Mixer", 1), ("Devices", 2)] {
        let w = ui.painter().layout_no_wrap(label.into(), font.clone(), TOKENS.text).size().x + 16.0;
        let b = Rect::from_min_size(pos2(x, r.min.y + 3.0), vec2(w, r.height() - 6.0));
        x += w + 2.0;
        let resp = ui.interact(b, ui.id().with(("keys_tab", which)), Sense::click());
        let on = which == 0;
        let p = ui.painter();
        if resp.hovered() && !on {
            p.rect_filled(b, crate::theme::RADIUS, TOKENS.button_hover);
        }
        p.text(b.center(), Align2::CENTER_CENTER, label, font.clone(), if on { TOKENS.text } else { TOKENS.text_dim });
        if on {
            p.line_segment([pos2(b.min.x + 6.0, b.max.y), pos2(b.max.x - 6.0, b.max.y)], Stroke::new(2.0, TOKENS.accent));
        }
        match which {
            0 => layout.tab_keys = r4(b),
            1 => layout.tab_mixer = r4(b),
            _ => layout.tab_devices = r4(b),
        }
        if resp.clicked() && !on {
            app.close_keys();
            app.ui.show_mixer = which == 1;
            app.ui.show_devices = which == 2;
        }
    }
    // The clip: a separator, a swatch in the track color, then its name (not a tab).
    let (track_name, color) = app.engine.session().track(cv.track).map(|t| (t.name.clone(), t.color)).unwrap_or_default();
    let title = if cv.name.is_empty() || cv.name == track_name { track_name } else { format!("{track_name} · {}", cv.name) };
    let p = ui.painter();
    p.line_segment([pos2(x + 8.0, r.min.y + 7.0), pos2(x + 8.0, r.max.y - 7.0)], Stroke::new(1.0, TOKENS.line));
    p.rect_filled(Rect::from_center_size(pos2(x + 22.0, r.center().y), vec2(8.0, 8.0)), crate::theme::RADIUS_SM, theme::rgb(color));

    // Right side, right to left: Audition, Quantize, Grid, velocity and its eyedropper.
    let mut rx = r.max.x - 8.0;
    let mut button = |ui: &mut Ui, w: f32, salt: &str| {
        let b = Rect::from_min_max(pos2(rx - w, r.min.y + 4.0), pos2(rx, r.max.y - 4.0));
        rx -= w + 6.0;
        (b, ui.interact(b, ui.id().with(("keys_btn", salt)), Sense::click()))
    };
    let small = FontId::proportional(11.5);
    let (b, resp) = button(ui, 96.0, "step");
    let on = app.keys.step.on;
    let p = ui.painter();
    p.rect_filled(
        b,
        crate::theme::RADIUS_SM,
        if on {
            TOKENS.accent
        } else if resp.hovered() {
            TOKENS.button_hover
        } else {
            TOKENS.button
        },
    );
    let label = if on { format!("Step {}", step::length_label(app.step_len())) } else { "Step".into() };
    p.text(b.center(), Align2::CENTER_CENTER, label, small.clone(), if on { theme::text_on(TOKENS.accent) } else { TOKENS.text_dim });
    layout.step = r4(b);
    if resp
        .on_hover_text(format!(
            "Step entry: notes played on the MIDI keyboard go in at the cursor ({}; rest {}, back {}, length 1-6)",
            crate::shortcuts::label(crate::shortcuts::Action::StepToggle),
            crate::shortcuts::label(crate::shortcuts::Action::StepRest),
            crate::shortcuts::label(crate::shortcuts::Action::StepBack)
        ))
        .clicked()
    {
        app.step_toggle();
    }
    let (b, resp) = button(ui, 72.0, "audition");
    let on = app.ui.keys.audition;
    let p = ui.painter();
    p.rect_filled(
        b,
        crate::theme::RADIUS_SM,
        if on {
            TOKENS.accent
        } else if resp.hovered() {
            TOKENS.button_hover
        } else {
            TOKENS.button
        },
    );
    p.text(b.center(), Align2::CENTER_CENTER, "Audition", small.clone(), if on { theme::text_on(TOKENS.accent) } else { TOKENS.text_dim });
    layout.audition = r4(b);
    if resp.on_hover_text("Play notes as you place, click or drag them").clicked() {
        app.ui.keys.audition = !on;
        app.release_auditions(f64::INFINITY, false);
    }
    let (b, resp) = button(ui, 72.0, "quantize");
    let p = ui.painter();
    p.rect_filled(b, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
    p.text(b.center(), Align2::CENTER_CENTER, "Quantize", small.clone(), TOKENS.text);
    layout.quantize = r4(b);
    if resp
        .on_hover_text(format!("Quantize the selection to the grid ({})", crate::shortcuts::label(crate::shortcuts::Action::NotesQuantize)))
        .clicked()
    {
        let _ = app.keys_quantize();
    }
    let (b, resp) = button(ui, 84.0, "snap");
    let p = ui.painter();
    p.rect_filled(b, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
    p.text(pos2(b.min.x + 8.0, b.center().y), Align2::LEFT_CENTER, format!("Grid {}", app.ui.keys.snap.label()), small, TOKENS.text);
    let c = pos2(b.max.x - 9.0, b.center().y);
    p.add(egui::Shape::convex_polygon(
        vec![pos2(c.x - 3.0, c.y - 1.5), pos2(c.x + 3.0, c.y - 1.5), pos2(c.x, c.y + 2.0)],
        TOKENS.text_dim,
        Stroke::NONE,
    ));
    layout.snap = r4(b);
    let current = app.ui.keys.snap;
    egui::Popup::menu(&resp).width(90.0).show(|ui| {
        for snap in Snap::ALL {
            let item = crate::menus::check_button(ui, snap == current, snap.label());
            layout.snap_menu.push(MenuItem::at(ui, snap.label().into(), snap.label().into(), item.rect));
            if item.clicked() {
                app.ui.keys.snap = snap;
                ui.close();
            }
        }
    });

    // Copy only a single, still-existing selection: a stale or mixed selection is ambiguous.
    let selected = match app.keys.selection.as_slice() {
        [note] if cv.notes.contains(note) => Some(note.velocity),
        _ => None,
    };
    let b = Rect::from_min_max(pos2(rx - 24.0, r.min.y + 4.0), pos2(rx, r.max.y - 4.0));
    rx -= 30.0;
    let resp = ui.add_enabled_ui(selected.is_some(), |ui| ui.put(b, egui::Button::new(""))).inner;
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, selected.is_some(), "Use selected note's velocity"));
    // Original code-drawn eyedropper; no image asset or font glyph.
    let p = ui.painter();
    let c = b.center();
    let ink = if selected.is_some() { TOKENS.text } else { TOKENS.text_dim };
    let stroke = Stroke::new(1.4, ink);
    p.line_segment([c + vec2(-6.0, 6.0), c + vec2(-4.0, 2.0)], stroke);
    p.add(egui::Shape::closed_line(vec![c + vec2(-4.0, 2.0), c + vec2(2.0, -4.0), c + vec2(4.0, -2.0), c + vec2(-2.0, 4.0)], stroke));
    p.line_segment([c + vec2(0.0, -6.0), c + vec2(6.0, 0.0)], stroke);
    p.line_segment([c + vec2(3.0, -3.0), c + vec2(6.0, -6.0)], Stroke::new(3.0, ink));
    layout.use_selected_velocity = r4(b);
    if resp.on_hover_text("Use selected note's velocity for new notes and audition (select one note)").clicked()
        && let Some(velocity) = selected
    {
        app.ui.keys.velocity = velocity.clamp(1, 127);
    }

    let field = Rect::from_min_max(pos2(rx - 44.0, r.min.y + 4.0), pos2(rx, r.max.y - 4.0));
    ui.push_id("keys_new_note_velocity", |ui| {
        let resp = ui.put(field, egui::DragValue::new(&mut app.ui.keys.velocity).range(1..=127).speed(1));
        layout.new_note_velocity = r4(resp.rect);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::DragValue, true, "New note velocity"));
        resp.on_hover_text("New note velocity (1–127): used for drawing and audition. Drag or click to type.");
    });
    rx -= 150.0;
    ui.painter().text(pos2(rx, r.center().y), Align2::LEFT_CENTER, "New note velocity", FontId::proportional(11.5), TOKENS.text);
    let title_rect = Rect::from_min_max(pos2(x + 32.0, r.min.y), pos2((rx - 8.0).max(x + 32.0), r.max.y));
    ui.painter().with_clip_rect(title_rect).text(title_rect.left_center(), Align2::LEFT_CENTER, title, FontId::proportional(11.5), TOKENS.text_dim);
}

/// The pane's top edge: drag to change its height.
fn pane_handle(app: &mut StudioApp, ui: &mut Ui, handle: Rect, layout: &mut KeysLayout) {
    let resp = ui.interact(handle, ui.id().with("keys_handle"), Sense::drag());
    ui.painter().line_segment([handle.left_top(), handle.right_top()], Stroke::new(1.0, TOKENS.line));
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        ui.painter().line_segment(
            [pos2(handle.min.x, handle.min.y + 1.0), pos2(handle.max.x, handle.min.y + 1.0)],
            Stroke::new(2.0, TOKENS.accent.gamma_multiply(0.6)),
        );
    }
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        app.keys.gesture = Some(KeysGesture::Height { start: app.ui.keys.height, press });
    }
    if resp.dragged()
        && let Some(KeysGesture::Height { start, press }) = app.keys.gesture.clone()
        && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
    {
        let max_h = (ui.ctx().content_rect().height() - 200.0).max(MIN_H);
        app.ui.keys.height = (start - (pos.y - press.y)).clamp(MIN_H, max_h);
    }
    if resp.drag_stopped() {
        app.keys.gesture = None;
    }
    layout.handle = r4(handle);
}

/// Key rows, the clip's range, bar, beat and grid lines, and the ruler with bar numbers.
fn paint_grid(ui: &Ui, s: &Session, map: &Map, grid: Rect, ruler: Rect, cv: &ClipView) {
    let p = ui.painter().with_clip_rect(grid);
    p.rect_filled(grid, 0.0, TOKENS.bg);
    let inside = Rect::from_min_max(pos2(map.x(0).max(grid.min.x), grid.min.y), pos2(map.x(cv.len).min(grid.max.x), grid.max.y));
    let lo = map.pitch(grid.max.y).clamp(0, 127);
    let hi = map.pitch(grid.min.y).clamp(0, 127);
    for pitch in lo..=hi {
        let pitch = pitch as u8;
        let y = map.y(pitch);
        let row = Rect::from_min_max(pos2(grid.min.x, y), pos2(grid.max.x, y + map.kh));
        let fill = if is_black(pitch) { TOKENS.lane } else { TOKENS.lane_alt };
        if inside.width() > 0.0 {
            p.rect_filled(row.intersect(inside), 0.0, fill);
        }
        if pitch.is_multiple_of(12) {
            p.line_segment([pos2(grid.min.x, y + map.kh), pos2(grid.max.x, y + map.kh)], Stroke::new(1.0, TOKENS.grid_bar));
        }
    }
    let rp = ui.painter().with_clip_rect(ruler);
    rp.rect_filled(ruler, 0.0, TOKENS.ruler);
    rp.rect_filled(Rect::from_min_max(pos2(map.x(0), ruler.max.y - 4.0), pos2(map.x(cv.len), ruler.max.y)), 0.0, TOKENS.text_dim.gamma_multiply(0.5));
    rp.line_segment([pos2(ruler.min.x, ruler.max.y - 0.5), pos2(ruler.max.x, ruler.max.y - 0.5)], Stroke::new(1.0, TOKENS.line));
    // Lines: grid subdivisions, beats, bars (absolute ticks; the clip starts at c0).
    let t_left = (map.tick(grid.min.x) as i64 + map.c0).max(0);
    let t_right = map.tick(grid.max.x) as i64 + map.c0;
    let first_bar = s.tempo.bar_beat_at_tick(t_left).bar.max(1);
    let bar_px = (map.x(s.tempo.bar_start_tick(first_bar + 1) - map.c0) - map.x(s.tempo.bar_start_tick(first_bar) - map.c0)).max(0.01);
    let every = [1, 2, 4, 8, 16, 32, 64].into_iter().find(|n| bar_px * *n as f32 >= 36.0).unwrap_or(128);
    for bar in (first_bar..).take(2000) {
        let b0 = s.tempo.bar_start_tick(bar);
        if b0 > t_right {
            break;
        }
        let b1 = s.tempo.bar_start_tick(bar + 1);
        let m = s.tempo.meter_at_tick(b0);
        let beat = (PPQ * 4 / i64::from(m.denominator.max(1))).max(1);
        let sub = map.grid.filter(|g| *g as f64 * map.ppt >= 7.0);
        let step = sub.unwrap_or(beat).min(beat);
        let mut t = b0 + step;
        while t < b1 {
            let x = map.x(t - map.c0);
            let color = if (t - b0) % beat == 0 { TOKENS.grid_bar.gamma_multiply(0.75) } else { TOKENS.grid_beat };
            if (t - b0) % beat == 0 || sub.is_some() {
                p.line_segment([pos2(x, grid.min.y), pos2(x, grid.max.y)], Stroke::new(1.0, color));
            }
            t += step;
        }
        let x = map.x(b0 - map.c0);
        p.line_segment([pos2(x, grid.min.y), pos2(x, grid.max.y)], Stroke::new(1.0, TOKENS.grid_bar));
        if (bar - 1) % every == 0 {
            rp.line_segment([pos2(x, ruler.min.y + 6.0), pos2(x, ruler.max.y)], Stroke::new(1.0, TOKENS.text_dim));
            rp.text(pos2(x + 4.0, ruler.min.y + 8.0), Align2::LEFT_CENTER, bar.to_string(), FontId::proportional(10.5), TOKENS.text);
        }
    }
    // Outside the clip: dimmed.
    let dim = TOKENS.overlay;
    if map.x(0) > grid.min.x {
        p.rect_filled(Rect::from_min_max(grid.min, pos2(map.x(0), grid.max.y)), 0.0, dim);
    }
    if map.x(cv.len) < grid.max.x {
        p.rect_filled(Rect::from_min_max(pos2(map.x(cv.len), grid.min.y), grid.max), 0.0, dim);
    }
}

/// Where the notes of the gesture in progress would land: (old, new) pairs, and whether they are copies.
fn preview(app: &StudioApp, ui: &Ui, map: &Map, vel: Rect, notes: &[Note]) -> Option<(Vec<(Note, Note)>, bool)> {
    let Some(KeysGesture::Notes { kind, anchor, press, copy, .. }) = app.keys.gesture.clone() else { return None };
    let pos = ui.input(|i| i.pointer.latest_pos())?;
    let sel: Vec<Note> = resolve(notes, &app.keys.selection).into_iter().map(|i| notes[i]).collect();
    let raw = f64::from(pos.x - press.x) / map.ppt;
    let pairs = match kind {
        NoteDrag::Move => {
            let dt = map.round(anchor.start as f64 + raw) - anchor.start;
            let ds = ((press.y - pos.y) / map.kh).round() as i64;
            let (dt, ds) = clamp_move(&sel, dt, ds);
            sel.iter().map(|n| (*n, shifted(n, dt, ds))).collect()
        }
        NoteDrag::Resize => {
            let min = map.grid.unwrap_or(1).max(1);
            let dl = map.round(anchor.end() as f64 + raw) - anchor.end();
            sel.iter().map(|n| (*n, Note { length: (n.length + dl).max(min.min(n.length)), ..*n })).collect()
        }
        NoteDrag::Velocity | NoteDrag::Lane => {
            let target = if kind == NoteDrag::Lane {
                ((vel.max.y - pos.y) / (vel.height() - 6.0) * 127.0).round() as i64
            } else {
                i64::from(anchor.velocity) + ((press.y - pos.y) / 1.5).round() as i64
            };
            let dv = target.clamp(1, 127) - i64::from(anchor.velocity);
            sel.iter().map(|n| (*n, Note { velocity: (i64::from(n.velocity) + dv).clamp(1, 127) as u8, ..*n })).collect()
        }
    };
    Some((pairs, copy && kind == NoteDrag::Move))
}

#[allow(clippy::too_many_arguments)]
fn paint_notes(
    app: &StudioApp,
    ui: &Ui,
    map: &Map,
    grid: Rect,
    vel: Rect,
    notes: &[Note],
    clip_len: i64,
    preview: Option<&(Vec<(Note, Note)>, bool)>,
    layout: &mut KeysLayout,
) {
    let p = ui.painter().with_clip_rect(grid);
    let vp = ui.painter().with_clip_rect(vel);
    vp.rect_filled(vel, 0.0, TOKENS.panel);
    let sel_idx = resolve(notes, &app.keys.selection);
    let moving: Vec<usize> = match preview {
        Some((pairs, false)) => resolve(notes, &pairs.iter().map(|(a, _)| *a).collect::<Vec<_>>()),
        _ => Vec::new(),
    };
    let stem = |n: &Note| {
        let x = map.x(n.start);
        let h = (vel.height() - 6.0) * f32::from(n.velocity) / 127.0;
        Rect::from_min_max(pos2(x, vel.max.y - h), pos2(x + 3.0, vel.max.y))
    };
    let draw = |n: &Note, selected: bool, alpha: f32| {
        let r = map.note_rect(n);
        let outside = n.start >= clip_len;
        let alpha = if outside { alpha * 0.4 } else { alpha };
        let a = 0.45 + 0.55 * f32::from(n.velocity) / 127.0;
        let fill = if selected { theme::lighten(TOKENS.accent, 0.25) } else { TOKENS.accent.gamma_multiply(a) };
        p.rect_filled(r, crate::theme::RADIUS_SM, fill.gamma_multiply(alpha));
        if selected {
            p.rect_stroke(r, crate::theme::RADIUS_SM, Stroke::new(1.0, TOKENS.selection_outline.gamma_multiply(alpha)), StrokeKind::Inside);
        }
        if outside {
            // Diagonal hatch: past the clip's end the note is silent.
            let hp = p.with_clip_rect(r.intersect(grid));
            let mut x = r.min.x - r.height();
            while x < r.max.x {
                hp.line_segment([pos2(x, r.max.y), pos2(x + r.height(), r.min.y)], Stroke::new(1.0, TOKENS.text_dim.gamma_multiply(0.7)));
                x += 5.0;
            }
        }
        let s = stem(n);
        let c = if selected { TOKENS.selection_outline } else { TOKENS.accent };
        vp.rect_filled(s, 0.0, c.gamma_multiply(alpha * 0.9));
        vp.rect_filled(Rect::from_min_size(pos2(s.min.x - 1.5, s.min.y - 1.5), vec2(6.0, 3.0)), crate::theme::RADIUS_SM, c.gamma_multiply(alpha));
    };
    for (i, n) in notes.iter().enumerate() {
        let selected = sel_idx.contains(&i);
        let r = map.note_rect(n);
        layout.notes.push(NoteRect {
            index: i,
            pitch: n.pitch,
            start: n.start,
            length: n.length,
            velocity: n.velocity,
            selected,
            outside: n.start >= clip_len,
            rect: r4(r),
            vel_rect: r4(stem(n)),
        });
        if r.max.x < grid.min.x || r.min.x > grid.max.x || r.max.y < grid.min.y || r.min.y > grid.max.y {
            continue;
        }
        draw(n, selected, if moving.contains(&i) { 0.3 } else { 1.0 });
    }
    if let Some((pairs, _)) = preview {
        for (_, n) in pairs {
            draw(n, true, 0.9);
        }
    }
}

/// The note under `pos`: the topmost, and whether the press is on its right edge.
fn hit(layout: &KeysLayout, pos: Pos2) -> Option<(usize, bool)> {
    layout.notes.iter().rev().find(|n| pos.x >= n.rect[0] && pos.x <= n.rect[2] && pos.y >= n.rect[1] && pos.y <= n.rect[3]).map(|n| {
        let w = n.rect[2] - n.rect[0];
        (n.index, w >= 8.0 && n.rect[2] - pos.x <= EDGE.min(w / 3.0))
    })
}

#[allow(clippy::too_many_arguments)]
fn grid_input(app: &mut StudioApp, ui: &mut Ui, map: &Map, grid: Rect, vel: Rect, cv: &ClipView, now: f64, layout: &mut KeysLayout) {
    let resp = ui.interact(grid, ui.id().with("keys_grid"), Sense::click_and_drag());
    let mods = ui.input(|i| i.modifiers);
    if let Some(pos) = resp.hover_pos()
        && app.keys.gesture.is_none()
    {
        match hit(layout, pos) {
            Some((_, true)) => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
            Some(_) if mods.alt => ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical),
            Some(_) => ui.ctx().set_cursor_icon(CursorIcon::Grab),
            None => {}
        }
    }
    let notes = &cv.notes;
    // Our own double-click: egui counts a quick second double-click elsewhere as a triple click.
    let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
    let double = click.is_some_and(|pos| {
        let d = app.keys.last_click.is_some_and(|(t, p)| now - t < 0.35 && p.distance(pos) < 6.0);
        app.keys.last_click = if d { None } else { Some((now, pos)) };
        d
    });
    if double && let Some(pos) = click {
        match hit(layout, pos) {
            // Double-click a note: delete it (Ableton).
            Some((i, _)) => {
                app.keys.selection = vec![notes[i]];
                let _ = app.keys_delete();
            }
            // Double-click empty grid: a note in that cell, one grid long, at the toolbar velocity.
            None => {
                let pitch = map.pitch(pos.y);
                let start = map.floor(map.tick(pos.x));
                if start >= cv.len {
                    app.status = "Outside the clip: drag the clip end to extend".into();
                } else if (0..=127).contains(&pitch) && start >= 0 {
                    let n = Note {
                        pitch: pitch as u8,
                        velocity: app.ui.keys.velocity.clamp(1, 127),
                        release_velocity: 64,
                        channel: 0,
                        start,
                        length: app.ui.keys.snap.step(),
                    };
                    if let Ok(sel) = app.keys_add(&[n]) {
                        app.keys.selection = sel;
                        app.audition(n.pitch, now);
                    }
                }
            }
        }
    } else if let Some(pos) = click {
        match hit(layout, pos) {
            Some((i, _)) => {
                let n = notes[i];
                if mods.shift {
                    if let Some(k) = app.keys.selection.iter().position(|x| *x == n) {
                        app.keys.selection.remove(k);
                    } else {
                        app.keys.selection.push(n);
                    }
                } else {
                    app.keys.selection = vec![n];
                }
                app.audition(n.pitch, now);
            }
            None => {
                if !mods.shift {
                    app.keys.selection.clear();
                }
                // The cursor (where Play starts), as in the arrangement.
                let s = app.engine.session();
                let at = s.tempo.tick_to_samples(cv.c0 + map.tick(pos.x).round().max(-(cv.c0 as f64)) as i64, s.sample_rate);
                app.set_cursor(at, None);
            }
        }
    }
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        match hit(layout, press) {
            Some((i, edge)) => {
                let n = notes[i];
                if !app.keys.selection.contains(&n) {
                    if mods.shift {
                        app.keys.selection.push(n);
                    } else {
                        app.keys.selection = vec![n];
                    }
                }
                let kind = if mods.alt {
                    NoteDrag::Velocity
                } else if edge {
                    NoteDrag::Resize
                } else {
                    NoteDrag::Move
                };
                if kind == NoteDrag::Move {
                    app.audition(n.pitch, now);
                }
                app.keys.gesture = Some(KeysGesture::Notes { kind, anchor: n, press, copy: mods.command, sounding: n.pitch });
            }
            None => app.keys.gesture = Some(KeysGesture::Band { press, additive: mods.shift }),
        }
    }
    if resp.dragged() {
        drag_update(app, ui, map, now);
    }
    if resp.drag_stopped() {
        finish(app, ui, map, vel, notes, layout);
    }
}

/// While dragging notes: Ctrl makes a copy; a pitch change auditions the new pitch.
fn drag_update(app: &mut StudioApp, ui: &Ui, map: &Map, now: f64) {
    let mods = ui.input(|i| i.modifiers);
    let pos = ui.input(|i| i.pointer.latest_pos());
    let mut play = None;
    if let Some(KeysGesture::Notes { kind, anchor, press, copy, sounding }) = &mut app.keys.gesture
        && *kind == NoteDrag::Move
    {
        *copy = mods.command;
        ui.ctx().set_cursor_icon(if *copy { CursorIcon::Copy } else { CursorIcon::Grabbing });
        if let Some(pos) = pos {
            let ds = ((press.y - pos.y) / map.kh).round() as i64;
            let pitch = (i64::from(anchor.pitch) + ds).clamp(0, 127) as u8;
            if pitch != *sounding {
                play = Some((*sounding, pitch));
                *sounding = pitch;
            }
        }
    }
    if let Some((old, new)) = play {
        if let (Some(p), Some(t)) = (&app.player, app.keys_track()) {
            app.player_requests.result(&mut app.status, p.note(t, old, 0));
        }
        app.keys.held.retain(|h| h.1 != old);
        app.audition(new, now);
    }
}

/// The end of a drag on notes or a rubber band: one command (or one merged step).
fn finish(app: &mut StudioApp, ui: &Ui, map: &Map, vel: Rect, notes: &[Note], layout: &KeysLayout) {
    let preview = preview(app, ui, map, vel, notes);
    let Some(g) = app.keys.gesture.take() else { return };
    match g {
        KeysGesture::Notes { kind, .. } => {
            let Some((pairs, copy)) = preview else { return };
            match kind {
                NoteDrag::Move => {
                    let (dt, ds) = pairs.first().map_or((0, 0), |(a, b)| (b.start - a.start, i64::from(b.pitch) - i64::from(a.pitch)));
                    if dt == 0 && ds == 0 {
                        return;
                    }
                    if copy {
                        let new: Vec<Note> = pairs.iter().map(|(_, b)| *b).collect();
                        if let Ok(sel) = app.keys_add(&new) {
                            app.keys.selection = sel;
                        }
                    } else {
                        app.keys.selection = pairs.iter().map(|(a, _)| *a).collect();
                        let _ = app.keys_move(dt, ds);
                    }
                }
                NoteDrag::Resize | NoteDrag::Velocity | NoteDrag::Lane => {
                    if let Ok(sel) = app.keys_edit(&pairs) {
                        app.keys.selection = sel;
                    }
                }
            }
        }
        KeysGesture::Band { press, additive } => {
            let Some(pos) = ui.input(|i| i.pointer.latest_pos()) else { return };
            let band = Rect::from_two_pos(press, pos);
            let inside: Vec<Note> = layout
                .notes
                .iter()
                .filter(|n| band.intersects(Rect::from_min_max(pos2(n.rect[0], n.rect[1]), pos2(n.rect[2], n.rect[3]))))
                .map(|n| notes[n.index])
                .collect();
            if !additive {
                app.keys.selection.clear();
            }
            for n in inside {
                if !app.keys.selection.contains(&n) {
                    app.keys.selection.push(n);
                }
            }
        }
        _ => {}
    }
}

/// The velocity lane: drag a note's stem (or click at a height) to set its velocity.
fn lane_input(app: &mut StudioApp, ui: &mut Ui, map: &Map, vel: Rect, cv: &ClipView, layout: &KeysLayout) {
    let resp = ui.interact(vel, ui.id().with("keys_vel"), Sense::click_and_drag());
    let notes = &cv.notes;
    // The note whose stem is nearest the press (within 6 points).
    let near = |x: f32| -> Option<usize> {
        layout
            .notes
            .iter()
            .map(|n| (n.index, (n.vel_rect[0] + 1.5 - x).abs()))
            .filter(|(_, d)| *d <= 6.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    };
    let begin = |app: &mut StudioApp, press: Pos2| -> bool {
        let Some(i) = near(press.x) else { return false };
        let n = notes[i];
        if !app.keys.selection.contains(&n) {
            app.keys.selection = vec![n];
        }
        app.keys.gesture = Some(KeysGesture::Notes { kind: NoteDrag::Lane, anchor: n, press, copy: false, sounding: n.pitch });
        true
    };
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        begin(app, press);
    }
    if resp.drag_stopped() && matches!(app.keys.gesture, Some(KeysGesture::Notes { kind: NoteDrag::Lane, .. })) {
        finish(app, ui, map, vel, notes, layout);
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && begin(app, pos)
    {
        finish(app, ui, map, vel, notes, layout);
    }
}

fn paint_keyboard(app: &StudioApp, ui: &Ui, map: &Map, kb: Rect) {
    let p = ui.painter().with_clip_rect(kb);
    p.rect_filled(kb, 0.0, TOKENS.panel_raised);
    let lo = map.pitch(kb.max.y).clamp(0, 127);
    let hi = map.pitch(kb.min.y).clamp(0, 127);
    let sounding: Vec<u8> = app.keys.held.iter().map(|h| h.1).collect();
    for pitch in lo..=hi {
        let pitch = pitch as u8;
        let y = map.y(pitch);
        let row = Rect::from_min_max(pos2(kb.min.x, y), pos2(kb.max.x, y + map.kh));
        let fill = if sounding.contains(&pitch) {
            TOKENS.accent
        } else if is_black(pitch) {
            TOKENS.bg
        } else {
            TOKENS.panel_raised
        };
        p.rect_filled(row, 0.0, fill);
        p.line_segment(
            [pos2(kb.min.x, row.max.y), pos2(kb.max.x, row.max.y)],
            Stroke::new(1.0, TOKENS.line.gamma_multiply(if pitch.is_multiple_of(12) { 1.0 } else { 0.5 })),
        );
        let c = pitch.is_multiple_of(12);
        if c || (map.kh >= 14.0 && !is_black(pitch)) {
            let color = if c { TOKENS.text } else { TOKENS.text_dim };
            p.text(
                pos2(kb.max.x - 6.0, row.center().y),
                Align2::RIGHT_CENTER,
                soundcraft_midi::note_name(pitch),
                FontId::proportional((map.kh - 4.0).clamp(8.0, 11.0)),
                color,
            );
        }
    }
    p.line_segment([pos2(kb.max.x - 0.5, kb.min.y), pos2(kb.max.x - 0.5, kb.max.y)], Stroke::new(1.0, TOKENS.line));
}

/// The keyboard column: a press plays the key; dragging sideways zooms the rows, up and down scrolls.
fn keyboard_input(app: &mut StudioApp, ui: &mut Ui, map: &Map, kb: Rect, now: f64) {
    let resp = ui.interact(kb, ui.id().with("keys_keyboard"), Sense::click_and_drag());
    if resp.contains_pointer()
        && ui.input(|i| i.pointer.primary_pressed())
        && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
    {
        let pitch = map.pitch(pos.y);
        if (0..=127).contains(&pitch) {
            // Played even with the audition switch off: the keyboard is an instrument.
            let on = app.ui.keys.audition;
            app.ui.keys.audition = true;
            app.audition(pitch as u8, now);
            app.ui.keys.audition = on;
        }
    }
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        let anchor = map.top - (press.y - map.gy0) / map.kh;
        app.keys.gesture = Some(KeysGesture::Keyboard { key_h: app.ui.keys.key_h, anchor, press });
    }
    if resp.dragged()
        && let Some(KeysGesture::Keyboard { key_h, anchor, press }) = app.keys.gesture.clone()
        && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
    {
        let kh = (key_h * ((pos.x - press.x) * 0.01).exp()).clamp(6.0, 40.0);
        app.ui.keys.key_h = kh;
        // The pitch grabbed stays under the pointer.
        app.keys.top = anchor + (pos.y - map.gy0) / kh;
    }
    if resp.drag_stopped() {
        app.keys.gesture = None;
    }
}

/// The ruler: a click sets the cursor; a drag up or down zooms time around the pointer, sideways scrolls.
fn ruler_input(app: &mut StudioApp, ui: &mut Ui, map: &Map, ruler: Rect, s: &Session) {
    let resp = ui.interact(ruler, ui.id().with("keys_ruler"), Sense::click_and_drag());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let abs = (map.c0 as f64 + map.tick(pos.x)).max(0.0) as i64;
        app.click_ruler(s.tempo.tick_to_samples(abs, s.sample_rate));
    }
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        app.keys.gesture = Some(KeysGesture::Ruler { ppq: app.ui.keys.px_per_quarter, anchor: map.tick(press.x), press });
    }
    if resp.dragged()
        && let Some(KeysGesture::Ruler { ppq, anchor, press }) = app.keys.gesture.clone()
        && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
    {
        let z = (ppq * ((pos.y - press.y) * 0.01).exp()).clamp(8.0, 800.0);
        app.ui.keys.px_per_quarter = z;
        app.keys.scroll = anchor - f64::from(pos.x - map.gx0) / (f64::from(z) / PPQ as f64);
    }
    if resp.drag_stopped() {
        app.keys.gesture = None;
    }
}

/// Slice 6: the clip's end as a handle in the ruler. Dragging it moves the end, snapped to the
/// grid (one grid step at least); on release the clip is trimmed or extended there
/// (`edit.trim_clip`, one undo step), so notes placed past the old end now sound.
#[allow(clippy::too_many_arguments)]
fn clip_end_handle(app: &mut StudioApp, ui: &mut Ui, map: &Map, ruler: Rect, grid: Rect, cv: &ClipView, clip: ClipId, layout: &mut KeysLayout) {
    let x = map.x(cv.len);
    let h = Rect::from_min_max(pos2(x - 5.0, ruler.min.y), pos2(x + 5.0, ruler.max.y));
    layout.end_handle = r4(h);
    if h.max.x < ruler.min.x || h.min.x > ruler.max.x {
        return;
    }
    let resp = ui.interact(h.intersect(ruler), ui.id().with("keys_clip_end"), Sense::click_and_drag());
    let dragging = matches!(app.keys.gesture, Some(KeysGesture::ClipEnd));
    let step = map.grid.unwrap_or(PPQ / 4).max(1);
    let target = ui.input(|i| i.pointer.latest_pos()).map(|p| map.round(map.tick(p.x)).max(step));
    let p = ui.painter().with_clip_rect(Rect::from_min_max(ruler.min, grid.max));
    let ink = if resp.hovered() || dragging { TOKENS.accent } else { TOKENS.text_dim };
    p.add(egui::Shape::convex_polygon(
        vec![pos2(x - 5.0, ruler.min.y + 4.0), pos2(x + 1.0, ruler.min.y + 4.0), pos2(x + 1.0, ruler.max.y), pos2(x - 5.0, ruler.max.y)],
        ink,
        Stroke::NONE,
    ));
    if resp.hovered() || dragging {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }
    if resp.drag_started() {
        app.keys.gesture = Some(KeysGesture::ClipEnd);
    }
    if dragging && let Some(t) = target {
        let tx = map.x(t);
        p.line_segment([pos2(tx, ruler.min.y), pos2(tx, grid.max.y)], Stroke::new(2.0, TOKENS.accent));
    }
    let resp = resp.on_hover_text("Drag to change the clip's length");
    if resp.drag_stopped() && dragging {
        app.keys.gesture = None;
        if let Some(t) = target
            && t != cv.len
        {
            let s = app.engine.session();
            let end = s.tempo.tick_to_samples(cv.c0 + t, s.sample_rate);
            if app.trim_clip(clip, false, end).is_ok() {
                let bars = t as f64 / (PPQ * 4) as f64;
                app.status = format!("Clip length {bars:.2} bars");
            }
        }
    }
}

/// Wheel: scroll pitch (Shift: time); Ctrl+wheel zooms time, Ctrl+Shift+wheel or Alt+wheel zooms keys.
fn wheel(app: &mut StudioApp, ui: &Ui, map: &Map, body: Rect) {
    let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else { return };
    if !body.contains(pos) {
        return;
    }
    // The wheel event's own modifiers when there is one (synthetic input sends no key changes).
    let wheel_mods = |i: &egui::InputState| {
        i.events
            .iter()
            .rev()
            .find_map(|e| if let egui::Event::MouseWheel { modifiers, .. } = e { Some(*modifiers) } else { None })
            .unwrap_or(i.modifiers)
    };
    let (delta, zoom, mods) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), wheel_mods(i)));
    let zoom_keys = |app: &mut StudioApp, f: f32| {
        let anchor = map.top - (pos.y - map.gy0) / map.kh;
        let kh = (app.ui.keys.key_h * f).clamp(6.0, 40.0);
        app.ui.keys.key_h = kh;
        app.keys.top = anchor + (pos.y - map.gy0) / kh;
    };
    if (zoom - 1.0).abs() > 1e-4 {
        if mods.shift {
            zoom_keys(app, zoom);
        } else {
            let anchor = map.tick(pos.x);
            let z = (app.ui.keys.px_per_quarter * zoom).clamp(8.0, 800.0);
            app.ui.keys.px_per_quarter = z;
            app.keys.scroll = anchor - f64::from(pos.x - map.gx0) / (f64::from(z) / PPQ as f64);
        }
        return;
    }
    if mods.alt || (mods.command && mods.shift) {
        let d = if delta.y.abs() > delta.x.abs() { delta.y } else { delta.x };
        if d.abs() > 0.1 {
            zoom_keys(app, (d * 0.005).exp());
        }
        return;
    }
    // Shift+vertical arrives as a horizontal delta already (see `arrange::wheel_pan`).
    if delta.x.abs() > 0.1 {
        app.keys.scroll -= crate::arrange::wheel_pan(ui, delta.x) * f64::from(body.max.x - map.gx0) / map.ppt;
    }
    if delta.y.abs() > 0.1 {
        app.keys.top += delta.y / map.kh;
    }
}

/// The arrangement's empty-lane hint: instrument and MIDI tracks with no clip say how to make one.
pub fn lane_hints(app: &mut StudioApp, ui: &Ui, tl: Rect, tracks: &[crate::arrange::TrackRow]) {
    let s = app.engine.session_arc();
    let mut ids = Vec::new();
    for row in tracks {
        let Some(t) = s.track(TrackId(row.id)) else { continue };
        if !t.kind.is_midi() || !t.clips().is_empty() {
            continue;
        }
        ids.push(row.id);
        let lane = Rect::from_min_max(pos2(row.lane[0], row.lane[1]), pos2(row.lane[2], row.lane[3])).intersect(tl);
        if lane.height() > 12.0 {
            ui.painter().with_clip_rect(tl).text(
                pos2(lane.min.x + 12.0, lane.center().y),
                Align2::LEFT_CENTER,
                "Double-click a lane to create a MIDI clip",
                FontId::proportional(11.5),
                TOKENS.text_dim,
            );
        }
    }
    app.keys.layout.hint_tracks = ids;
}
