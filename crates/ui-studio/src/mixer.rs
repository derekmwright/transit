//! The mixer: one channel strip per track plus the master, as a pane under the arrangement
//! (Ableton's Ctrl+Alt+M) or filling the window (Cubase's F3). A strip has the track's name and
//! color, input and output dropdowns, a balance knob, Mute/Solo/Record/Monitor, a
//! fader and a meter. Flat, like the arrangement: fills, no borders or gradients, one accent.
//!
//! Everything a strip changes goes through engine commands. A fader or knob drag is one undo step
//! (`Engine::execute_merged` with a per-track key). Which strip is focused, the panes and the
//! scroll are view state in [`crate::UiState`] and [`MixerState`], never in the document.
//!
//! Model leaks handled here (plan 06, section 2):
//! - L10: stereo tracks pan dual-mono. The knob is a balance control that drives both panners
//!   through [`pans_for`] (two `mix.pan` calls under one merge key).
//! - L5: busses are named paths. Outputs name the FX/group channel (the aux track
//!   whose input is the bus), not the bus. "Add FX Channel" is four commands, one undo step.
//! - L12: the master is an optional track. Without one the main-out strip shows the main meter
//!   and offers to add a master fader.
//! - L4: sends are ten fixed slots. A strip lists the occupied ones as rows (target, level,
//!   remove) under its input, Logic style, and "+ Send" fills the first free slot.
//!
//! Routing never offers a target that would close a loop of any length: the engine accepts
//! loops (`track.output`, `track.input` and `mix.send` don't check), and the mix only bounds the
//! aux depth, so the menus walk the graph themselves ([`feeding_loops`], [`listening_loops`]).

use crate::StudioApp;
use crate::arrange::R4;
use crate::fine::{self, Target};
use crate::theme::{self, TOKENS};
use egui::{Align2, Color32, CursorIcon, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_model::{BusId, Route, SendSlot, Session, Track, TrackId, TrackKind};

/// Strip width and gap (points).
pub const STRIP_W: f32 = 88.0;
const GAP: f32 = 2.0;
/// The fader's top, in dB.
pub const FADER_MAX_DB: f32 = 12.0;

/// A drag in progress on a fader, knob or send level. `raw` is where the pointer has taken the
/// control (fader position 0..1 for a fader or send, balance -1..1 for a knob), before snapping,
/// so the snap is a detent the drag leaves again rather than a place it sticks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixGesture {
    pub target: Target,
    pub raw: f32,
    /// The fader's height in points (pointer travel to fader travel).
    pub height: f32,
}

/// The value a "+ Send" menu entry carries for "New FX Channel…".
pub const NEW_FX: &str = "\u{1}new-fx-channel";
/// The value of the "Remove unused buses (n)" entry that ends the send, output and input menus.
pub const REMOVE_UNUSED: &str = "\u{1}remove-unused-busses";

/// One send row as last drawn (a strip's or the device chain's).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SendRow {
    pub slot: usize,
    /// What the target reads as: the FX channel's name, or "Bus: name".
    pub target: String,
    pub level_db: f32,
    pub pre_fader: bool,
    pub rect: R4,
    /// Click: a menu to send somewhere else.
    pub target_rect: R4,
    /// The thin level bar along the row's bottom: drag changes the level (one undo step),
    /// double-click resets it to 0 dB.
    pub level_rect: R4,
    /// The level as text: wheel nudges it, double-click types a value, drag changes it.
    pub readout_rect: R4,
    /// What the readout shows.
    pub level_text: String,
    pub remove: R4,
    /// The pre/post toggle (device chain only; strips have it in the row's right-click menu).
    pub pre: R4,
    /// Slice 7: the send's mute toggle and whether it is muted.
    pub mute: R4,
    pub muted: bool,
    /// Slice 7: the send's pan (device chain only) and what it shows.
    pub pan: R4,
    pub pan_text: String,
}

/// One dropdown or menu entry as last drawn (for `ui.inspect`; scripts click them).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MenuItem {
    pub label: String,
    pub value: String,
    pub rect: R4,
    /// False while the entry is scrolled out of its menu's view (a click there would miss).
    pub visible: bool,
}

impl MenuItem {
    /// An entry drawn in `ui` at `rect`, visible when its center is inside the ui's clip.
    pub fn at(ui: &Ui, label: String, value: String, rect: Rect) -> Self {
        MenuItem { label, value, rect: r4(rect), visible: ui.clip_rect().contains(rect.center()) }
    }
}

/// One strip as last drawn, with the values it shows.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StripLayout {
    pub track: u64,
    pub name: String,
    pub kind: Option<TrackKind>,
    pub rect: R4,
    pub name_rect: R4,
    pub input: R4,
    pub output: R4,
    pub input_label: String,
    pub output_label: String,
    pub knob: R4,
    pub mute: R4,
    pub solo: R4,
    pub arm: R4,
    pub monitor: R4,
    pub fader: R4,
    /// The y of the line through the fader cap, and of the 0 dB tick, as painted last frame.
    /// Both are on physical pixel centers, so at 0 dB they are one y.
    pub cap_line_y: Option<f32>,
    pub zero_tick_y: Option<f32>,
    /// The physical pixel rows [top, bottom) each of the two covers; the cap line is as thick as the
    /// tick and covers exactly its rows at 0 dB.
    pub cap_line_span: Option<[i32; 2]>,
    pub zero_tick_span: Option<[i32; 2]>,
    pub meter: R4,
    /// The dB readout under the fader (double-click types a level) and its text.
    pub readout_rect: R4,
    pub volume_text: String,
    /// The held peak above the meter ("-3.2", "+1.4"; click resets, Ctrl-click resets all).
    pub peak_rect: R4,
    pub peak_text: String,
    /// The pan readout under the knob and its text ("C", "L20", "R35").
    pub pan_readout_rect: R4,
    pub pan_text: String,
    pub volume_db: f32,
    /// The knob's value, -1 (left) .. 1 (right), read back from the track's panners.
    pub balance: f32,
    /// The track's panners as the engine holds them.
    pub pan: Vec<f32>,
    pub level: [f32; 2],
    /// An FX (aux) channel: tagged "FX", its name muted.
    pub fx: bool,
    pub sends: Vec<SendRow>,
    /// "+ Send" (zero when the strip takes no sends or all ten are in use).
    pub add_send: R4,
    /// The occupied insert slots listed under the input (at most three rows).
    pub inserts: Vec<InsertRow>,
    /// The "+" row that adds an effect to this track (zero when all ten slots are in use).
    pub add_insert: R4,
    /// The strip is the focused track's (the device chain follows it).
    pub focused: bool,
    /// The group this track sits in (slice 2c).
    pub folder: Option<u64>,
}

/// One insert row of a strip.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct InsertRow {
    pub slot: usize,
    pub name: String,
    pub bypass: bool,
    pub rect: R4,
}

/// Last frame's mixer geometry.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MixerLayout {
    pub visible: bool,
    pub area: R4,
    pub strips: Vec<StripLayout>,
    /// The master strip (`track` 0 when there is no master track yet).
    pub master: Option<StripLayout>,
    pub add_master: R4,
    pub add_fx: R4,
    /// The open dropdown's entries.
    pub menu: Vec<MenuItem>,
    /// The bars above a group's strip and its members' strips (slice 2c).
    pub groups: Vec<GroupLink>,
}

/// The thin accent bar that links a group's strip to its members' strips.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct GroupLink {
    pub folder: u64,
    pub members: Vec<u64>,
    pub rect: R4,
}

/// One device card as last drawn.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DeviceCard {
    /// Insert slot, or `None` for the instrument.
    pub slot: Option<usize>,
    pub plugin: String,
    pub name: String,
    pub bypass: bool,
    pub rect: R4,
    pub bypass_button: R4,
    pub remove_button: R4,
    pub editor_button: R4,
    pub editor_enabled: bool,
    /// The plug-in's editor window is open or wanted (the button then reads "Close editor").
    pub editor_open: bool,
    /// The instrument card's name (or "Choose…" when there is none): opens the instrument menu.
    pub pick_button: R4,
}

/// Last frame's device chain geometry.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DevicesLayout {
    pub visible: bool,
    pub track: Option<u64>,
    pub area: R4,
    pub cards: Vec<DeviceCard>,
    pub add: R4,
    /// The open menu's entries: "+" or the instrument menu (`value` is the plug-in id), or a
    /// send menu (`value` is the bus name, or [`NEW_FX`]).
    pub menu: Vec<MenuItem>,
    /// The focused track's sends.
    pub sends: Vec<SendRow>,
    pub add_send: R4,
}

/// A plug-in the "+" menu offers.
#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub id: String,
    pub name: String,
    pub category: String,
    pub instrument: bool,
}

#[derive(Debug, Default)]
pub struct MixerState {
    pub gesture: Option<MixGesture>,
    /// Horizontal scroll of the strips (points).
    pub hscroll: f32,
    pub layout: MixerLayout,
    pub devices: DevicesLayout,
    /// Built-in plug-ins (from `engine.plugins`), read once.
    pub(crate) builtin: Option<Vec<PluginEntry>>,
    /// Display names of hosted plug-ins by id, filled from the scan caches as cards need them.
    pub(crate) names: std::collections::HashMap<String, String>,
    /// The instrument menu's filter text (shown when the list is long).
    pub instrument_filter: String,
    /// The "+" effects menu's filter text.
    pub effect_filter: String,
    /// Where the instrument menu's filter box was last drawn (for `ui.inspect`).
    pub filter_rect: R4,
    /// Gestures started, to give each composite its own merge key.
    pub(crate) serial: u64,
    /// A readout being edited in place.
    pub edit: Option<fine::ReadoutEdit>,
    /// Where the editing field was last drawn (for `ui.inspect`).
    pub edit_rect: R4,
    /// The wheel gesture in progress (its nudges are one undo step).
    pub wheel: Option<fine::WheelMerge>,
    /// The control under the pointer as last drawn (Delete resets it).
    pub hover: Option<Target>,
    /// A control took the wheel this frame, so the strips don't scroll.
    wheel_taken: bool,
}

/// Fader position 0..1 for a gain in dB: the fourth root of the amplitude, scaled so +12 dB is
/// the top. 0 dB sits at about 70 %, -60 dB at about 13 %; -144 dB (silence) is the bottom.
pub fn db_to_k(db: f32) -> f32 {
    if db <= -144.0 {
        return 0.0;
    }
    let top = 10f32.powf(FADER_MAX_DB / 80.0);
    (10f32.powf(db / 80.0) / top).clamp(0.0, 1.0)
}

/// The inverse of [`db_to_k`].
pub fn k_to_db(k: f32) -> f32 {
    if k <= 0.0005 {
        return -144.0;
    }
    let top = 10f32.powf(FADER_MAX_DB / 80.0);
    (80.0 * (k.min(1.0) * top).log10()).clamp(-144.0, FADER_MAX_DB)
}

/// The panners for balance `b` (-1..1) on a track with `n` of them (plan 06, L10). Mono: the one
/// panner is `b`. Stereo: center keeps L hard left and R hard right; turning toward a side moves
/// the other channel's panner across, so at the end both sit on that side.
pub fn pans_for(b: f32, n: usize) -> Vec<f32> {
    let b = b.clamp(-1.0, 1.0);
    match n {
        0 => Vec::new(),
        1 => vec![b],
        _ => vec![-1.0 + 2.0 * b.max(0.0), 1.0 + 2.0 * b.min(0.0)],
    }
}

/// The balance a set of panners stands for (the inverse of [`pans_for`]).
pub fn balance_of(pan: &[f32]) -> f32 {
    match pan {
        [] => 0.0,
        [p] => *p,
        [l, r, ..] => ((l + r) / 2.0).clamp(-1.0, 1.0),
    }
}

fn r4(r: Rect) -> R4 {
    [r.min.x, r.min.y, r.max.x, r.max.y]
}

fn meter_k(lin: f32) -> f32 {
    if lin <= 0.0 {
        return 0.0;
    }
    ((20.0 * lin.log10() + 60.0) / 66.0).clamp(0.0, 1.0)
}

/// The busses nobody listens to (no track has one as its input): nobody hears them.
pub fn orphan_busses(s: &Session) -> Vec<BusId> {
    s.busses.iter().filter(|b| !heard(s, b.id)).map(|b| b.id).collect()
}

/// Whether any track listens to `bus`.
fn heard(s: &Session, bus: BusId) -> bool {
    s.tracks.iter().any(|t| t.mixer.input == Route::Bus(bus))
}

/// Ends a routing menu with "Remove unused buses (n)" when the session has orphans.
fn push_unused_entry(s: &Session, choices: &mut Vec<(String, String)>) {
    let n = orphan_busses(s).len();
    if n > 0 {
        choices.push((format!("Remove unused buses ({n})"), REMOVE_UNUSED.into()));
    }
}

/// The aux track fed by `bus`, if any: an FX or group channel.
fn channel_of_bus(s: &Session, bus: BusId) -> Option<&Track> {
    // A group's own bus reads as the group (slice 2c).
    s.tracks.iter().find(|t| matches!(t.kind, TrackKind::Aux | TrackKind::Folder) && t.mixer.input == Route::Bus(bus))
}

/// A bus as a routing target reads: the FX channel it feeds, else "Bus: name".
pub fn bus_label(s: &Session, bus: BusId) -> String {
    channel_of_bus(s, bus).map_or_else(|| format!("Bus: {}", s.bus(bus).map_or("?", |b| b.name.as_str())), |c| c.name.clone())
}

/// The busses a track feeds: its output and its sends' targets.
fn bus_targets(t: &Track) -> impl Iterator<Item = BusId> + '_ {
    std::iter::once(&t.mixer.output).chain(t.mixer.sends.iter().flatten().map(|s| &s.target)).filter_map(|r| match r {
        Route::Bus(b) => Some(*b),
        _ => None,
    })
}

/// Whether audio put on bus `from` reaches bus `to`: through every track listening to a bus and
/// on through its output and sends, any number of hops (`from == to` counts).
pub fn reaches(s: &Session, from: BusId, to: BusId) -> bool {
    let mut seen: Vec<BusId> = Vec::new();
    let mut todo = vec![from];
    while let Some(b) = todo.pop() {
        if b == to {
            return true;
        }
        if seen.contains(&b) {
            continue;
        }
        seen.push(b);
        for t in s.tracks.iter().filter(|t| t.mixer.input == Route::Bus(b)) {
            todo.extend(bus_targets(t));
        }
    }
    false
}

/// Whether `t` feeding bus `b` (output or send) would close a loop back to `t`'s own input.
pub fn feeding_loops(s: &Session, t: &Track, b: BusId) -> bool {
    matches!(t.mixer.input, Route::Bus(input) if reaches(s, b, input))
}

/// Whether `t` listening to bus `b` would close a loop: something `t` feeds reaches `b`.
pub fn listening_loops(s: &Session, t: &Track, b: BusId) -> bool {
    bus_targets(t).any(|o| reaches(s, o, b))
}

/// Whether a track kind has sends here: audio, instrument, FX channels and groups (a group sums
/// its members like an FX channel, and the mix runs its sends the same way).
pub fn takes_sends(kind: TrackKind) -> bool {
    matches!(kind, TrackKind::Audio | TrackKind::Instrument | TrackKind::Aux | TrackKind::Folder)
}

/// Where a send from `t` can go: `(label, bus name)` for every bus that is not already one of its
/// sends (`except` slot aside) and would not close a loop.
pub fn send_targets(s: &Session, t: &Track, except: Option<usize>) -> Vec<(String, String)> {
    let taken: Vec<&Route> =
        t.mixer.sends.iter().enumerate().filter(|(i, _)| Some(*i) != except).filter_map(|(_, x)| x.as_ref()).map(|x| &x.target).collect();
    s.busses
        .iter()
        .filter(|b| !taken.contains(&&Route::Bus(b.id)) && !feeding_loops(s, t, b.id) && heard(s, b.id))
        .map(|b| (bus_label(s, b.id), b.name.clone()))
        .collect()
}

/// Whether picking `value` from a route dropdown would leave the route as it is: the same route,
/// or the label the current one already reads as (an audio track's default input, L15).
fn same_route(s: &Session, cur: &Route, cur_label: &str, value: &str, label: &str) -> bool {
    if label == cur_label {
        return true;
    }
    let parsed = match value.to_ascii_lowercase().as_str() {
        "" | "none" | "no input" | "no output" => Route::None,
        "main" | "out 1-2" | "master" => Route::Main,
        _ => s.bus_by_name(value).map_or_else(|| Route::Hardware(value.to_string()), |b| Route::Bus(b.id)),
    };
    &parsed == cur
}

/// How a route reads in a dropdown: busses by the channel they feed (L5). An audio track whose
/// input is `None` or `Main` records and monitors from input 1 (`engine::io::plan_takes`, the mix
/// engine's monitoring), so it reads as that input, not "No input".
pub fn route_label(s: &Session, r: &Route, input: bool, format_channels: usize) -> String {
    match r {
        Route::None | Route::Main if input && format_channels > 0 => if format_channels >= 2 { "In 1-2" } else { "In 1" }.into(),
        Route::None => if input { "No input" } else { "No output" }.into(),
        Route::Main => "Main".into(),
        Route::Bus(b) if input => s.bus(*b).map_or_else(|| "Bus".into(), |b| format!("Bus: {}", b.name)),
        Route::Bus(b) => bus_label(s, *b),
        Route::Hardware(h) => h.clone(),
    }
}

/// The outputs a track can pick: `(label, value for track.output)`.
pub fn output_choices(s: &Session, t: &Track) -> Vec<(String, String)> {
    let mut v = vec![("Main".to_string(), "main".to_string())];
    for b in &s.busses {
        // Never a bus that comes back to this track's input, however many hops away.
        if feeding_loops(s, t, b.id) || (!heard(s, b.id) && t.mixer.output != Route::Bus(b.id)) {
            continue;
        }
        v.push((bus_label(s, b.id), b.name.clone()));
    }
    v.push(("No output".into(), "none".into()));
    v
}

/// The inputs a track can pick: hardware inputs (`In 1`.. or `In 1-2`..) and busses.
pub fn input_choices(s: &Session, t: &Track, device_inputs: usize) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let n = device_inputs.max(2);
    if t.kind == TrackKind::Audio || t.kind == TrackKind::Aux {
        if t.channels() >= 2 {
            for i in (1..n).step_by(2) {
                v.push((format!("In {i}-{}", i + 1), format!("In {i}-{}", i + 1)));
            }
        } else {
            for i in 1..=n {
                v.push((format!("In {i}"), format!("In {i}")));
            }
        }
    }
    for b in &s.busses {
        if listening_loops(s, t, b.id) || !heard(s, b.id) {
            continue;
        }
        v.push((format!("Bus: {}", b.name), b.name.clone()));
    }
    // An audio track with no input still records from input 1 (see `route_label`), so only an
    // FX or group channel is offered "No input".
    if t.kind == TrackKind::Aux {
        v.push(("No input".into(), "none".into()));
    }
    v
}

impl StudioApp {
    /// The focused track (device chain, highlight): the last clicked one, else the cursor's
    /// track, else the first selected track.
    pub fn focus_track(&self) -> Option<TrackId> {
        let s = self.engine.session();
        [self.ui.focus_track, self.ui.cursor_track, s.edit.selected_tracks.first().copied()].into_iter().flatten().find(|t| s.track(*t).is_some())
    }

    /// Focus a track from either view: it becomes the selected track (as a click on its header
    /// does) and the cursor's track. Selection is not part of the saved document.
    pub fn focus(&mut self, t: TrackId) {
        self.ui.focus_track = Some(t);
        if self.engine.session().edit.selected_tracks != [t] {
            let _ = self.run("edit.select", json!({"tracks": [t.0]}));
        }
        let at = self.cursor();
        self.set_cursor(at, Some(t));
    }

    /// A click on a track's header (arrangement) or channel strip (mixer): focus it, and let
    /// Delete take it. `additive` (Shift) adds it to the selected tracks, or removes it again.
    pub fn focus_header(&mut self, t: TrackId, additive: bool) {
        if !additive {
            self.focus(t);
            self.tracks_selected_by_header = true;
            return;
        }
        let mut tracks: Vec<u64> = self.engine.session().edit.selected_tracks.iter().map(|x| x.0).collect();
        if let Some(i) = tracks.iter().position(|x| *x == t.0) {
            tracks.remove(i);
        } else {
            tracks.push(t.0);
        }
        self.ui.focus_track = Some(t);
        let _ = self.run("edit.select", json!({"tracks": tracks}));
        self.tracks_selected_by_header = !tracks.is_empty();
    }

    /// Fader gesture: one undo step per drag (merge key `fader:<track>`).
    pub fn set_volume_merged(&mut self, t: TrackId, db: f32) -> Result<Value, String> {
        self.engine.execute_merged("mix.volume", &json!({"tracks": [t.0], "db": db}), &format!("fader:{}", t.0)).map_err(|e| e.to_string())
    }

    /// Balance knob (L10): both panners of a stereo track, one undo step per gesture.
    pub fn set_balance(&mut self, t: TrackId, b: f32) -> Result<Value, String> {
        self.set_balance_keyed(t, b, &format!("pan:{}", t.0))
    }

    fn set_balance_keyed(&mut self, t: TrackId, b: f32, key: &str) -> Result<Value, String> {
        let n = self.engine.session().track(t).map_or(0, |tr| tr.mixer.pan.len());
        let key = key.to_string();
        let mut last = json!({});
        for (i, p) in pans_for(b, n).into_iter().enumerate() {
            last = self.engine.execute_merged("mix.pan", &json!({"tracks": [t.0], "pan": p, "index": i}), &key).map_err(|e| e.to_string())?;
        }
        Ok(last)
    }

    /// What a control holds now: dB for a fader or send, balance for a knob.
    pub fn value_of(&self, t: Target) -> Option<f32> {
        let s = self.engine.session();
        match t {
            Target::Fader(id) => s.track(id).map(|tr| tr.mixer.volume_db),
            Target::Pan(id) => s.track(id).map(|tr| balance_of(&tr.mixer.pan)),
            Target::Send(id, slot) => s.track(id)?.mixer.sends.get(slot)?.as_ref().map(|x| x.level_db),
            Target::SendPan(id, slot) => s.track(id)?.mixer.sends.get(slot)?.as_ref().map(|x| x.pan),
        }
    }

    /// What a control shows: the automated value at the playhead under Read, else [`Self::value_of`].
    pub fn shown_value(&self, t: Target) -> Option<f32> {
        let (id, lane) = crate::automation::Lane::of_target(t);
        let s = self.engine.session_arc();
        s.track(id).and_then(|tr| self.followed(tr, &lane)).or_else(|| self.value_of(t))
    }

    /// Set a control (dB, or balance for a knob) as part of a merged gesture: `key` is the
    /// gesture's merge key, by default the control's own drag key.
    pub fn set_value_merged(&mut self, t: Target, v: f32, key: Option<&str>) -> Result<Value, String> {
        let r = self.set_value_keyed(t, v, key);
        // The touched control's own commands belong to a running pass.
        crate::automation::note_pass_edit(self);
        r
    }

    fn set_value_keyed(&mut self, t: Target, v: f32, key: Option<&str>) -> Result<Value, String> {
        match t {
            Target::Fader(id) => {
                let key = key.map_or_else(|| format!("fader:{}", id.0), str::to_string);
                self.engine.execute_merged("mix.volume", &json!({"tracks": [id.0], "db": v}), &key).map_err(|e| e.to_string())
            }
            Target::Send(id, slot) => {
                let key = key.map_or_else(|| format!("send:{}:{slot}", id.0), str::to_string);
                self.engine.execute_merged("mix.send_level", &json!({"track": id.0, "slot": slot, "db": v}), &key).map_err(|e| e.to_string())
            }
            Target::Pan(id) => {
                let key = key.map_or_else(|| format!("pan:{}", id.0), str::to_string);
                self.set_balance_keyed(id, v, &key)
            }
            Target::SendPan(id, slot) => {
                let key = key.map_or_else(|| format!("sendpan:{}:{slot}", id.0), str::to_string);
                self.engine
                    .execute_merged("mix.send_level", &json!({"track": id.0, "slot": slot, "pan": v.clamp(-1.0, 1.0)}), &key)
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// Set a control to `v` as one undo step of its own (a reset, or a typed value).
    pub fn set_value(&mut self, t: Target, v: f32) -> Result<Value, String> {
        self.engine.end_merge();
        self.mixer.wheel = None;
        self.mixer.serial += 1;
        let r = self.set_value_merged(t, v, Some(&format!("set:{}", self.mixer.serial)));
        self.engine.end_merge();
        r
    }

    /// Reset a control to its default: 0.0 dB, or center.
    pub fn reset_control(&mut self, t: Target) {
        let _ = self.set_value(t, 0.0);
    }

    /// A wheel notch or several on a control: 0.1 dB each (Shift: 0.01; a knob 1 %, Shift 0.1 %).
    /// The nudges of one wheel gesture are one undo step.
    fn wheel_nudge(&mut self, t: Target, notches: f32, shift: bool, rect: Rect, now: f64) {
        let Some(cur) = self.shown_value(t) else { return };
        let step = match (t.is_pan(), shift) {
            (true, false) => fine::WHEEL_PAN,
            (true, true) => fine::WHEEL_PAN_FINE,
            (false, false) => fine::WHEEL_DB,
            (false, true) => fine::WHEEL_DB_FINE,
        };
        let (lo, hi) = if t.is_pan() { (-1.0, 1.0) } else { (fine::DB_MIN, fine::DB_MAX) };
        let v = fine::nudge(cur, notches, step).clamp(lo, hi);
        if self.mixer.wheel.map(|w| w.target) != Some(t) && !crate::automation::passing(self) {
            self.engine.end_merge();
        }
        self.mixer.wheel = Some(fine::WheelMerge { target: t, rect, last: now });
        let _ = self.set_value_merged(t, v, Some(&t.wheel_key()));
    }

    /// Ends a wheel gesture when the pointer has left the control or 500 ms have passed since
    /// its last notch. Returns whether one is still going (its merge must not be ended yet).
    pub(crate) fn drive_wheel(&mut self, ctx: &egui::Context, now: f64) -> bool {
        let Some(w) = self.mixer.wheel else { return false };
        let inside = ctx.input(|i| i.pointer.hover_pos()).is_some_and(|p| w.rect.contains(p));
        if inside && now - w.last <= fine::WHEEL_MERGE_S {
            ctx.request_repaint_after_secs((fine::WHEEL_MERGE_S - (now - w.last)).max(0.0) as f32 + 0.02);
            return true;
        }
        self.mixer.wheel = None;
        if !crate::automation::passing(self) {
            self.engine.end_merge();
        }
        false
    }

    /// Route a track's output (`track.output`), by the value an output choice carries. Picking
    /// what the output already is changes nothing (no document edit, no undo step).
    pub fn set_output(&mut self, t: TrackId, value: &str) -> Result<Value, String> {
        let s = self.engine.session_arc();
        if let Some(tr) = s.track(t) {
            let cur = route_label(&s, &tr.mixer.output, false, tr.channels());
            let label = output_choices(&s, tr).into_iter().find(|(_, v)| v == value).map_or_else(|| value.to_string(), |(l, _)| l);
            if same_route(&s, &tr.mixer.output, &cur, value, &label) {
                return Ok(json!({"unchanged": true}));
            }
        }
        self.run("track.output", json!({"tracks": [t.0], "output": value}))
    }

    /// Route a track's input (`track.input`). Picking what the input already reads as (an audio
    /// track's default "In 1-2", L15) changes nothing.
    pub fn set_input(&mut self, t: TrackId, value: &str) -> Result<Value, String> {
        let s = self.engine.session_arc();
        if let Some(tr) = s.track(t) {
            let ch = if tr.kind == TrackKind::Audio { tr.channels() } else { 0 };
            let cur = route_label(&s, &tr.mixer.input, true, ch);
            let label =
                input_choices(&s, tr, self.device_inputs()).into_iter().find(|(_, v)| v == value).map_or_else(|| value.to_string(), |(l, _)| l);
            if same_route(&s, &tr.mixer.input, &cur, value, &label) {
                return Ok(json!({"unchanged": true}));
            }
        }
        self.run("track.input", json!({"tracks": [t.0], "input": value}))
    }

    /// Add a device at the end of the track's chain: the slot after the last occupied one, so it
    /// lands after every device already there. Ten slots at most (plan 06, L1): when the last
    /// slot is taken but an earlier one is free, the device goes there and the status line says so.
    pub fn add_device(&mut self, t: TrackId, plugin: &str) -> Result<Value, String> {
        let ins = self.engine.session().track(t).map(|tr| tr.mixer.inserts.clone()).unwrap_or_default();
        let next = ins.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
        if next < ins.len() {
            return self.run("mix.insert", json!({"track": t.0, "slot": next, "plugin": plugin}));
        }
        let r = self.run("mix.insert", json!({"track": t.0, "plugin": plugin}));
        if r.is_ok() {
            self.status =
                "The last device slot is in use, so the new device went into a free slot earlier in the chain (ten slots, plan 06 L1).".into();
        }
        r
    }

    pub fn toggle_bypass(&mut self, t: TrackId, slot: usize) -> Result<Value, String> {
        self.run("mix.insert_bypass", json!({"track": t.0, "slot": slot}))
    }

    /// Remove a device; its editor window closes first.
    pub fn remove_device(&mut self, t: TrackId, slot: usize) -> Result<Value, String> {
        if let Some(p) = &self.player {
            p.close_editor(t, slot);
        }
        self.ui.plugin_windows.retain(|k| *k != (t, slot));
        self.run("mix.insert_remove", json!({"track": t.0, "slot": slot}))
    }

    /// Put `plugin` on an instrument track as its instrument (`mix.instrument`, one undo step),
    /// replacing the one there. The old instrument's editor window closes first. Picking the
    /// instrument the track already has changes nothing.
    pub fn set_instrument(&mut self, t: TrackId, plugin: &str) -> Result<Value, String> {
        if self.engine.session().track(t).and_then(|tr| tr.instrument.as_ref()).is_some_and(|i| i.plugin == plugin) {
            return Ok(json!({"unchanged": true}));
        }
        let key = (t, soundcraft_mix::INSTRUMENT_SLOT);
        if let Some(p) = &self.player {
            p.close_editor(key.0, key.1);
        }
        self.ui.plugin_windows.retain(|k| *k != key);
        self.editors.retain(|e| e.key != key);
        self.run("mix.instrument", json!({"track": t.0, "plugin": plugin}))
    }

    /// "+ FX Channel" after the strips: a new FX channel with a send from the focused track
    /// (not from an FX channel or the master) at -12 dB.
    pub fn add_fx_channel(&mut self) -> Result<TrackId, String> {
        let s = self.engine.session();
        let from = self.focus_track().filter(|t| s.track(*t).is_some_and(|t| t.kind != TrackKind::Aux && takes_sends(t.kind)));
        self.new_fx_channel(from, -12.0)
    }

    /// New FX Channel (plan 06, L5): a bus, an FX (aux) channel listening to it and, from `from`,
    /// a send to it at `level_db`, as one undo step. Returns the new channel's track id.
    ///
    /// When `from` has no free send slot nothing is made. If a step fails anyway, the steps
    /// already run are undone, so no bus or channel is left behind. The undo step reads "New Bus"
    /// (its first command's label): the engine has no way to name a composite (plan 06, L20).
    pub fn new_fx_channel(&mut self, from: Option<TrackId>, level_db: f32) -> Result<TrackId, String> {
        let s = self.engine.session_arc();
        let name =
            (1..).map(|i| format!("FX {i}")).find(|n| s.bus_by_name(n).is_none() && s.track_by_name(n).is_none()).unwrap_or_else(|| "FX".into());
        let slot = from.and_then(|f| s.track(f)).map(|t| (t.name.clone(), t.mixer.sends.iter().position(Option::is_none)));
        if let Some((track, None)) = &slot {
            self.status = format!("New FX Channel: all ten sends of {track} are in use");
            return Err(self.status.clone());
        }
        let slot = slot.and_then(|(_, s)| s);
        self.mixer.serial += 1;
        let key = format!("fx-channel:{}", self.mixer.serial);
        self.engine.end_merge();
        let r = (|| {
            let e = &mut self.engine;
            e.execute_merged("mix.new_bus", &json!({"name": name}), &key)?;
            let r = e.execute_merged("track.new", &json!({"kind": "aux", "format": "stereo", "name": name}), &key)?;
            let id = r.get("tracks").and_then(|t| t.get(0)).and_then(Value::as_u64).unwrap_or(0);
            e.execute_merged("track.input", &json!({"tracks": [id], "input": name}), &key)?;
            if let Some(f) = from {
                let mut p = json!({"track": f.0, "bus": name, "level_db": level_db});
                if let Some(slot) = slot {
                    p["slot"] = json!(slot);
                }
                e.execute_merged("mix.send", &p, &key)?;
            }
            Ok::<u64, soundcraft_engine::EngineError>(id)
        })();
        self.engine.end_merge();
        match r {
            Ok(id) => {
                self.status = format!("Added {name}{}", from.map_or(String::new(), |_| " with a send to it".into()));
                Ok(TrackId(id))
            }
            Err(e) => {
                // Take back what the composite did so far (one merged undo step).
                if !std::sync::Arc::ptr_eq(&s, &self.engine.session_arc()) {
                    self.engine.undo();
                }
                self.status = format!("New FX Channel: {e}");
                Err(self.status.clone())
            }
        }
    }

    /// "+ Send": a send from `t` to bus `value` at 0 dB in the first free slot, or a new FX
    /// channel with its send for [`NEW_FX`].
    pub fn add_send(&mut self, t: TrackId, value: &str) -> Result<Value, String> {
        if value == NEW_FX {
            return self.new_fx_channel(Some(t), 0.0).map(|id| json!({"track": id.0}));
        }
        self.run("mix.send", json!({"track": t.0, "bus": value, "level_db": 0.0}))
    }

    /// Send slot `slot` somewhere else; its level and pre/post setting stay.
    pub fn retarget_send(&mut self, t: TrackId, slot: usize, bus: &str) -> Result<Value, String> {
        self.run("mix.send", json!({"track": t.0, "slot": slot, "bus": bus}))
    }

    /// Send level gesture: one undo step per drag (merge key `send:<track>:<slot>`).
    pub fn set_send_level_merged(&mut self, t: TrackId, slot: usize, db: f32) -> Result<Value, String> {
        self.engine
            .execute_merged("mix.send_level", &json!({"track": t.0, "slot": slot, "db": db}), &format!("send:{}:{slot}", t.0))
            .map_err(|e| e.to_string())
    }

    pub fn remove_send(&mut self, t: TrackId, slot: usize) -> Result<Value, String> {
        self.run("mix.send_remove", json!({"track": t.0, "slot": slot}))
    }

    /// A send's mute (slice 7): one undo step.
    pub fn set_send_mute(&mut self, t: TrackId, slot: usize, mute: bool) -> Result<Value, String> {
        self.run("mix.send_level", json!({"track": t.0, "slot": slot, "mute": mute}))
    }

    /// Pre-fader or post-fader (`mix.send_level {pre_fader}`).
    pub fn set_send_pre(&mut self, t: TrackId, slot: usize, pre: bool) -> Result<Value, String> {
        self.run("mix.send_level", json!({"track": t.0, "slot": slot, "pre_fader": pre}))
    }

    /// The number of audio inputs offered in input dropdowns: the open input's, else 8.
    fn device_inputs(&self) -> usize {
        self.recorder.as_ref().map_or(8, |r| r.channels)
    }
}

/// The bottom panes: device chain (lowest) and the mixer above it. Call before the central panel.
pub fn panes(app: &mut StudioApp, ui: &mut Ui) {
    app.mixer.layout.menu.clear();
    app.mixer.devices.menu.clear();
    app.mixer.filter_rect = Default::default();
    app.mixer.hover = None;
    app.mixer.wheel_taken = false;
    if app.mixer.edit.is_none() {
        app.mixer.edit_rect = [0.0; 4];
    }
    app.mixer.devices.visible = app.ui.show_devices;
    if app.ui.show_devices {
        egui::Panel::bottom("devices")
            .exact_size(crate::devices::PANE_H)
            .frame(egui::Frame::NONE.fill(TOKENS.panel))
            .show(ui, |ui| crate::devices::chain(app, ui));
    }
    app.mixer.layout.visible = app.ui.show_mixer || app.ui.mixer_full;
    if app.ui.show_mixer && !app.ui.mixer_full {
        let h = app.ui.mixer_height.clamp(260.0, 600.0);
        egui::Panel::bottom("mixer").exact_size(h).frame(egui::Frame::NONE.fill(TOKENS.bg)).show(ui, |ui| strips(app, ui));
    }
}

/// The mixer filling the window (F3).
pub fn show_full(app: &mut StudioApp, ui: &mut Ui) {
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(TOKENS.bg)).show(ui, |ui| strips(app, ui));
}

fn strips(app: &mut StudioApp, ui: &mut Ui) {
    let area = ui.max_rect();
    let p = ui.painter();
    p.line_segment([area.left_top(), area.right_top()], Stroke::new(1.0, TOKENS.line));
    let s = app.engine.session_arc();
    let focus = app.focus_track();
    let master = s.tracks.iter().find(|t| t.kind == TrackKind::Master && !t.hidden);
    // Session order, which is the arrangement's; members of a collapsed group are out (slice 2c).
    let channels: Vec<&Track> = s.tracks.iter().filter(|t| t.kind != TrackKind::Master && !t.hidden && !app.tucked(&s, t)).collect();
    // The master strip is pinned right; the others scroll under it.
    let master_rect = Rect::from_min_max(pos2(area.max.x - STRIP_W - 6.0, area.min.y + 4.0), pos2(area.max.x - 6.0, area.max.y - 4.0));
    let lane = Rect::from_min_max(pos2(area.min.x + 4.0, area.min.y + 4.0), pos2(master_rect.min.x - 10.0, area.max.y - 4.0));
    let content_w = channels.len() as f32 * (STRIP_W + GAP) + 110.0;
    app.mixer.hscroll = app.mixer.hscroll.clamp(0.0, (content_w - lane.width()).max(0.0));
    let mut layout = MixerLayout { visible: true, area: r4(area), menu: std::mem::take(&mut app.mixer.layout.menu), ..MixerLayout::default() };
    // The sends block is as tall on every strip (the most sends plus "+ Send"), so the faders
    // line up, and never so tall that a fader gets shorter than MIN_FADER.
    let wanted = channels.iter().filter(|t| takes_sends(t.kind)).map(|t| send_rows_of(t)).max().unwrap_or(0);
    // Insert rows come first (up to INSERT_ROWS, fewer in a short pane); sends get what is left.
    let free = lane.height() - STRIP_FIXED_H - MIN_FADER;
    let ins_rows = (((free - 2.0 * SEND_ROW_H) / INSERT_ROW_H).floor().max(1.0) as usize).min(INSERT_ROWS);
    let room = ((free - ins_rows as f32 * INSERT_ROW_H) / SEND_ROW_H).floor().max(0.0) as usize;
    let rows = wanted.min(room);
    for (i, t) in channels.iter().enumerate() {
        let x = lane.min.x + i as f32 * (STRIP_W + GAP) - app.mixer.hscroll;
        let r = Rect::from_min_size(pos2(x, lane.min.y), vec2(STRIP_W, lane.height()));
        if r.max.x < lane.min.x || r.min.x > lane.max.x {
            continue;
        }
        let sl = strip(app, ui, &s, t, r, lane, focus == Some(t.id), (rows, ins_rows), &mut layout.menu);
        layout.strips.push(sl);
    }
    layout.groups = group_links(ui, &s, &channels, lane, area, app.mixer.hscroll);
    // "Add FX Channel" after the last strip.
    let ax = lane.min.x + channels.len() as f32 * (STRIP_W + GAP) - app.mixer.hscroll + 6.0;
    let add = Rect::from_min_size(pos2(ax, lane.min.y + 4.0), vec2(96.0, 24.0));
    if add.min.x < lane.max.x {
        let resp = ui.interact(add.intersect(lane), ui.id().with("add_fx"), Sense::click());
        let p = ui.painter().with_clip_rect(lane);
        p.rect_filled(add, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
        p.text(add.center(), Align2::CENTER_CENTER, "+ FX Channel", FontId::proportional(11.5), TOKENS.text);
        if resp.on_hover_text("A return channel with a send from the focused track").clicked() {
            let _ = app.add_fx_channel();
        }
    }
    layout.add_fx = r4(add);
    // Master.
    ui.painter().line_segment([pos2(master_rect.min.x - 5.0, area.min.y), pos2(master_rect.min.x - 5.0, area.max.y)], Stroke::new(1.0, TOKENS.line));
    match master {
        Some(m) => {
            let sl = strip(app, ui, &s, m, master_rect, master_rect, focus == Some(m.id), (rows, ins_rows), &mut layout.menu);
            layout.master = Some(sl);
        }
        None => {
            let (sl, add) = main_out(app, ui, master_rect);
            layout.master = Some(sl);
            layout.add_master = add;
        }
    }
    // Wheel over the strips scrolls them sideways.
    if let Some(pos) = ui.input(|i| i.pointer.hover_pos())
        && lane.contains(pos)
        && app.mixer.gesture.is_none()
        && !app.mixer.wheel_taken
    {
        let d = ui.input(|i| i.smooth_scroll_delta);
        let d = if d.x.abs() > d.y.abs() { d.x } else { d.y };
        if d.abs() > 0.1 {
            app.mixer.hscroll = (app.mixer.hscroll - d).clamp(0.0, (content_w - lane.width()).max(0.0));
        }
    }
    app.mixer.layout = layout;
}

/// Above the strips, a thin accent bar from each group's strip over its members' strips.
fn group_links(ui: &Ui, s: &Session, channels: &[&Track], lane: Rect, area: Rect, hscroll: f32) -> Vec<GroupLink> {
    let x = |i: usize| lane.min.x + i as f32 * (STRIP_W + GAP) - hscroll;
    let p = ui.painter().with_clip_rect(Rect::from_min_max(pos2(lane.min.x, area.min.y), pos2(lane.max.x, lane.min.y)));
    let mut out = Vec::new();
    for (i, g) in channels.iter().enumerate().filter(|(_, t)| t.is_folder()) {
        let members: Vec<u64> = channels[i + 1..]
            .iter()
            .take_while(|t| crate::groups::within(s, t.id, g.id))
            .filter(|t| t.folder == Some(g.id))
            .map(|t| t.id.0)
            .collect();
        let last = channels[i + 1..].iter().take_while(|t| crate::groups::within(s, t.id, g.id)).count();
        if last == 0 {
            continue;
        }
        let r = Rect::from_min_max(pos2(x(i) + 2.0, area.min.y + 1.0), pos2(x(i + last) + STRIP_W - 2.0, area.min.y + 3.0));
        p.rect_filled(r, crate::theme::RADIUS_SM, TOKENS.accent);
        out.push(GroupLink { folder: g.id.0, members, rect: r4(r) });
    }
    out
}

/// The main-out strip when the session has no master track: the main meter and an offer.
fn main_out(app: &mut StudioApp, ui: &mut Ui, r: Rect) -> (StripLayout, R4) {
    let p = ui.painter();
    p.rect_filled(r, crate::theme::RADIUS, TOKENS.panel);
    p.text(pos2(r.center().x, r.min.y + 12.0), Align2::CENTER_CENTER, "Main", FontId::proportional(12.0), TOKENS.text);
    let add = Rect::from_min_size(pos2(r.min.x + 6.0, r.min.y + 28.0), vec2(r.width() - 12.0, 34.0));
    let resp = ui.interact(add, ui.id().with("add_master"), Sense::click());
    p.rect_filled(add, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
    p.text(add.center(), Align2::CENTER_CENTER, "+ Master\nfader", FontId::proportional(10.5), TOKENS.text);
    if resp.clicked() {
        let _ = app.run("track.new", json!({"kind": "master"}));
    }
    let meter = Rect::from_min_max(pos2(r.center().x - 8.0, r.min.y + 76.0), pos2(r.center().x + 8.0, r.max.y - 26.0));
    draw_meter(p, meter, app.main_meter.level, app.main_meter.clip);
    let peak_r = Rect::from_min_max(pos2(r.min.x + 6.0, r.min.y + 63.0), pos2(r.max.x - 6.0, r.min.y + 75.0));
    let (text, over) = peak_text(app.main_meter.peak);
    let resp = ui.interact(peak_r, ui.id().with("main_peak"), Sense::click());
    p.rect_filled(peak_r, crate::theme::RADIUS_SM, TOKENS.bg);
    let color = if over || app.main_meter.clip { TOKENS.record } else { TOKENS.text_dim };
    p.text(peak_r.center(), Align2::CENTER_CENTER, &text, FontId::proportional(10.0), color);
    let on_meter = ui.interact(meter, ui.id().with("main_meter_click"), Sense::click());
    if resp.clicked() || on_meter.clicked() {
        let all = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        if all {
            app.reset_meters(None);
        } else {
            app.reset_main_meter();
        }
    }
    let sl = StripLayout {
        peak_rect: r4(peak_r),
        peak_text: text,
        name: "Main".into(),
        rect: r4(r),
        meter: r4(meter),
        level: app.main_meter.level,
        ..StripLayout::default()
    };
    (sl, r4(add))
}

fn draw_meter(p: &egui::Painter, r: Rect, level: [f32; 2], clip: bool) {
    p.rect_filled(r, crate::theme::RADIUS_SM, TOKENS.bg);
    let w = (r.width() - 3.0) / 2.0;
    for (c, lvl) in level.iter().enumerate() {
        let x = r.min.x + 1.0 + c as f32 * (w + 1.0);
        let h = (r.height() - 2.0) * meter_k(*lvl);
        let bar = Rect::from_min_max(pos2(x, r.max.y - 1.0 - h), pos2(x + w, r.max.y - 1.0));
        p.rect_filled(bar, 0.0, if *lvl > 0.9 { TOKENS.solo } else { TOKENS.monitor });
    }
    // A clip latch is a small red cap at the top of the bar, not a red bar.
    if clip {
        p.rect_filled(Rect::from_min_size(r.min, vec2(r.width(), CLIP_CAP)), 0.0, TOKENS.record);
    }
}

/// Height of the clip latch's cap at the top of a meter.
pub const CLIP_CAP: f32 = 3.0;

impl StudioApp {
    /// Clear the clip latch and the peak hold of one strip, or of every strip and the main meter.
    pub fn reset_meters(&mut self, only: Option<TrackId>) {
        match only {
            Some(id) => {
                if let Some(d) = self.meters.get_mut(&id) {
                    (d.peak, d.clip) = (0.0, false);
                }
                if self.engine.session().track(id).is_some_and(|t| t.kind == TrackKind::Master) {
                    (self.main_meter.peak, self.main_meter.clip) = (0.0, false);
                }
            }
            None => {
                for d in self.meters.values_mut().chain(std::iter::once(&mut self.main_meter)) {
                    (d.peak, d.clip) = (0.0, false);
                }
            }
        }
    }

    /// Clear the main meter and, when there is one, the Master track's own meter: the master
    /// strip shows `meters[master]` and the transport shows `main_meter`.
    pub fn reset_main_meter(&mut self) {
        (self.main_meter.peak, self.main_meter.clip) = (0.0, false);
        let master = self.engine.session().tracks.iter().find(|t| t.kind == TrackKind::Master).map(|t| t.id);
        if let Some(d) = master.and_then(|id| self.meters.get_mut(&id)) {
            (d.peak, d.clip) = (0.0, false);
        }
    }

    /// `track.clear_clip_indicators` (Alt+C), and playback starting: every clip latch off. The
    /// peak holds stay until clicked.
    pub fn clear_clip_latches(&mut self) {
        for d in self.meters.values_mut().chain(std::iter::once(&mut self.main_meter)) {
            d.clip = false;
        }
    }
}

/// A small text button inside a strip; returns its response.
fn small_button(ui: &mut Ui, clip: Rect, r: Rect, id: egui::Id, label: &str, on: bool, color: Color32, enabled: bool) -> egui::Response {
    let resp = ui.interact(r.intersect(clip), id, Sense::click());
    let p = ui.painter().with_clip_rect(clip);
    let fill = if on {
        color
    } else if resp.hovered() && enabled {
        TOKENS.button_hover
    } else {
        TOKENS.button
    };
    p.rect_filled(r, crate::theme::RADIUS_SM, fill);
    let fg = if on {
        theme::text_on(color)
    } else if enabled {
        TOKENS.text
    } else {
        TOKENS.text_dim.gamma_multiply(0.5)
    };
    p.text(r.center(), Align2::CENTER_CENTER, label, FontId::proportional(11.0), fg);
    resp
}

/// A dropdown field: its current value, and a menu of `choices` when clicked. Returns the value
/// picked. The open menu's entries go to `menu` for `ui.inspect`.
fn dropdown(ui: &mut Ui, clip: Rect, r: Rect, id: egui::Id, current: &str, choices: &[(String, String)], menu: &mut Vec<MenuItem>) -> Option<String> {
    let resp = ui.interact(r.intersect(clip), id, Sense::click());
    let p = ui.painter().with_clip_rect(clip);
    p.rect_filled(r, crate::theme::RADIUS_SM, if resp.hovered() { TOKENS.button_hover } else { TOKENS.bg });
    let text_r = r.shrink2(vec2(4.0, 0.0));
    p.with_clip_rect(text_r.intersect(clip)).text(text_r.left_center(), Align2::LEFT_CENTER, current, FontId::proportional(10.5), TOKENS.text);
    crate::icons::paint(&p, pos2(r.max.x - 7.0, r.center().y), crate::icons::Icon::ChevronDown, 10.0, TOKENS.text_dim);
    choice_menu(&resp, current, choices, menu)
}

/// The menu of a dropdown-like field: `choices` as `(label, value)`, the `current` label marked.
/// A [`NEW_FX`] entry comes after a separator. Returns the value picked.
fn choice_menu(resp: &egui::Response, current: &str, choices: &[(String, String)], menu: &mut Vec<MenuItem>) -> Option<String> {
    let mut picked = None;
    egui::Popup::menu(resp).width(150.0).show(|ui| {
        if choices.is_empty() {
            ui.weak("Nothing to pick");
        }
        for (label, value) in choices {
            if value == NEW_FX || value == REMOVE_UNUSED {
                ui.separator();
            }
            let b = ui.add(egui::Button::selectable(label == current, label.as_str()));
            menu.push(MenuItem::at(ui, label.clone(), value.clone(), b.rect));
            if b.clicked() {
                picked = Some(value.clone());
                ui.close();
            }
        }
    });
    picked
}

/// Height of one send row, and of the strip without its sends block and fader.
pub const SEND_ROW_H: f32 = 17.0;
/// The send level bar along a row's bottom edge.
const BAR_H: f32 = 5.0;
const STRIP_FIXED_H: f32 = 207.0;
/// Height of an insert row, and the most rows a strip has for them: three inserts and "+", or
/// two, "n more…" and "+" in a short pane (one row: only "+").
const INSERT_ROW_H: f32 = 15.0;
const INSERT_ROWS: usize = 4;
const MIN_FADER: f32 = 80.0;

/// The rows a track's sends block wants: its sends plus "+ Send" while a slot is free.
fn send_rows_of(t: &Track) -> usize {
    let used = t.mixer.sends.iter().flatten().count();
    used + usize::from(used < t.mixer.sends.len())
}

/// How a send level reads: whole dB in a strip, tenths in the device chain.
fn level_text(db: f32, wide: bool) -> String {
    if db <= -144.0 {
        "-inf".into()
    } else if wide {
        format!("{db:.1} dB")
    } else if db.abs() < 0.5 {
        "0".into()
    } else {
        format!("{db:.0}")
    }
}

/// One send row: the target (click: send it somewhere else; right-click: pre/post and remove),
/// the level (drag like a fader, one undo step; double-click: 0 dB) and remove. `wide` (the
/// device chain) adds a Pre/Post toggle; a strip marks a pre-fader send with a "P".
#[allow(clippy::too_many_arguments)]
pub(crate) fn send_row(
    app: &mut StudioApp,
    ui: &mut Ui,
    s: &Session,
    t: &Track,
    slot: usize,
    snd: &SendSlot,
    r: Rect,
    clip: Rect,
    wide: bool,
    menu: &mut Vec<MenuItem>,
) -> SendRow {
    let id = t.id;
    let salt = (id.0, slot, wide);
    // Under Read automation the row shows the lanes' values at the playhead.
    let mut shown = snd.clone();
    if let Ok(i) = u8::try_from(slot) {
        shown.level_db = app.followed(t, &crate::automation::Lane::SendLevel(i)).unwrap_or(snd.level_db);
        shown.pan = app.followed(t, &crate::automation::Lane::SendPan(i)).unwrap_or(snd.pan);
    }
    let snd = &shown;
    let p = ui.painter().with_clip_rect(clip);
    let target = match &snd.target {
        Route::Bus(b) => bus_label(s, *b),
        other => route_label(s, other, false, 0),
    };
    let mut row = SendRow { slot, target: target.clone(), level_db: snd.level_db, pre_fader: snd.pre_fader, rect: r4(r), ..SendRow::default() };
    p.rect_filled(r, crate::theme::RADIUS_SM, TOKENS.bg);
    // The level as a thin line along the bottom, so the row reads as a small fader.
    let k = db_to_k(snd.level_db);
    p.rect_filled(
        Rect::from_min_size(pos2(r.min.x, r.max.y - 2.0), vec2(r.width() * k, 2.0)),
        0.0,
        if snd.mute { TOKENS.text_dim } else { TOKENS.accent },
    );
    let xb = Rect::from_min_max(pos2(r.max.x - 14.0, r.min.y), r.max);
    let lr = Rect::from_min_max(pos2(xb.min.x - if wide { 50.0 } else { 24.0 }, r.min.y), pos2(xb.min.x, r.max.y));
    // The bar along the bottom is its own control; the text hit rects stop above it.
    let above = |q: Rect| Rect::from_min_max(q.min, pos2(q.max.x, q.max.y - BAR_H));
    // Slice 7: the send's mute, left of the level, and in the device chain its pan.
    let mb = Rect::from_min_max(pos2(lr.min.x - 14.0, r.min.y + 1.0), pos2(lr.min.x - 1.0, r.max.y - BAR_H));
    let pr = Rect::from_min_max(
        pos2(mb.min.x - if wide { 34.0 } else { 0.0 }, r.min.y + 2.0),
        pos2(mb.min.x - if wide { 4.0 } else { 0.0 }, r.max.y - 3.0),
    );
    let pnr = Rect::from_min_max(pos2(pr.min.x - if wide { 32.0 } else { 0.0 }, r.min.y), pos2(pr.min.x - if wide { 2.0 } else { 0.0 }, r.max.y));
    let mut tr = Rect::from_min_max(r.min, pos2(pnr.min.x - 1.0, r.max.y));
    if !wide && snd.pre_fader {
        p.text(pos2(tr.min.x + 7.0, tr.center().y), Align2::CENTER_CENTER, "P", FontId::proportional(9.5), TOKENS.text_dim);
        tr.min.x += 12.0;
    }
    // Target: another bus, or right-click for pre/post and remove.
    let th = above(tr);
    let resp = ui.interact(th.intersect(clip), ui.id().with(("send_target", salt)), Sense::click());
    if resp.hovered() {
        p.rect_filled(th, crate::theme::RADIUS_SM, TOKENS.button_hover);
    }
    let text_r = tr.shrink2(vec2(4.0, 0.0));
    p.with_clip_rect(text_r.intersect(clip)).text(text_r.left_center(), Align2::LEFT_CENTER, &target, FontId::proportional(10.5), TOKENS.text);
    row.target_rect = r4(th);
    let mut choices = send_targets(s, t, Some(slot));
    if !choices.iter().any(|(l, _)| *l == target) {
        choices.insert(0, (target.clone(), String::new()));
    }
    push_unused_entry(s, &mut choices);
    if let Some(v) = choice_menu(&resp, &target, &choices, menu)
        && !v.is_empty()
        && v != target
    {
        if v == REMOVE_UNUSED {
            let _ = app.remove_unused_busses();
        } else {
            let _ = app.retarget_send(id, slot, &v);
        }
    }
    // Its own popup id: `Popup::context_menu` closes the popup on a primary click, which would
    // otherwise be the retarget menu's (both default to the response's id).
    egui::Popup::context_menu(&resp).id(resp.id.with("send_ctx")).show(|ui| {
        let label = if snd.pre_fader { "Post-fader" } else { "Pre-fader" };
        let b = ui.button(label);
        menu.push(MenuItem::at(ui, label.into(), "pre".into(), b.rect));
        if b.clicked() {
            let _ = app.set_send_pre(id, slot, !snd.pre_fader);
            ui.close();
        }
        let b = ui.button("Remove Send");
        menu.push(MenuItem::at(ui, "Remove Send".into(), "remove".into(), b.rect));
        if b.clicked() {
            let _ = app.remove_send(id, slot);
            ui.close();
        }
    });
    // Pre/Post (device chain).
    if wide {
        let resp = ui.interact(pr.intersect(clip), ui.id().with(("send_pre", salt)), Sense::click());
        p.rect_filled(pr, crate::theme::RADIUS_SM, if snd.pre_fader || resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
        p.text(
            pr.center(),
            Align2::CENTER_CENTER,
            if snd.pre_fader { "Pre" } else { "Post" },
            FontId::proportional(10.0),
            if snd.pre_fader { TOKENS.text } else { TOKENS.text_dim },
        );
        if resp.on_hover_text("Pre-fader or post-fader send").clicked() {
            let _ = app.set_send_pre(id, slot, !snd.pre_fader);
        }
        row.pre = r4(pr);
    }
    // Mute: one undo step (`mix.send_level {mute}`).
    let resp = ui.interact(mb.intersect(clip), ui.id().with(("send_mute", salt)), Sense::click());
    let fill = if snd.mute {
        TOKENS.mute
    } else if resp.hovered() {
        TOKENS.button_hover
    } else {
        TOKENS.button
    };
    p.rect_filled(mb, crate::theme::RADIUS_SM, fill);
    p.text(mb.center(), Align2::CENTER_CENTER, "M", FontId::proportional(9.0), if snd.mute { theme::text_on(TOKENS.mute) } else { TOKENS.text_dim });
    if resp.on_hover_text(if snd.mute { "Unmute this send" } else { "Mute this send" }).clicked() {
        let _ = app.set_send_mute(id, slot, !snd.mute);
    }
    row.mute = r4(mb);
    row.muted = snd.mute;
    // Pan (device chain): drag, wheel, double-click to type, Alt-click to center. The mix pans a
    // mono track's send only (a stereo source keeps its sides).
    if wide {
        let pt = Target::SendPan(id, slot);
        let text = fine::pan_text(snd.pan);
        let hot = app.mixer.gesture.is_some_and(|g| g.target == pt);
        p.rect_filled(above(pnr).shrink2(vec2(0.0, 1.0)), crate::theme::RADIUS_SM, TOKENS.panel_raised);
        readout(
            app,
            ui,
            &p,
            clip,
            Readout {
                target: pt,
                salt: 1,
                draw: above(pnr),
                hit: above(pnr),
                text: text.clone(),
                edit_text: text.clone(),
                font: theme::mono(ui.ctx(), 10.0),
                color: if hot { TOKENS.accent } else { TOKENS.text },
                align: Align2::CENTER_CENTER,
                drag: true,
            },
        );
        row.pan = r4(above(pnr));
        row.pan_text = text;
    }
    // Level. The thin bar along the bottom is the control: drag up or right to raise it, like the
    // fader (fourth-root law); double-click, Alt-click, Ctrl-click and Delete reset it to 0 dB;
    // a mark shows 0 dB. The number is a readout: double-click types a level; drag and wheel work
    // on it too.
    let me = Target::Send(id, slot);
    let bar = Rect::from_min_max(pos2(r.min.x, r.max.y - BAR_H), r.max);
    let resp = ui.interact(bar.intersect(clip), ui.id().with(("send_level", salt)), Sense::click_and_drag());
    let zero_x = r.min.x + r.width() * db_to_k(0.0);
    p.line_segment([pos2(zero_x, bar.min.y), pos2(zero_x, r.max.y)], Stroke::new(1.0, TOKENS.text_dim));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
    }
    control(app, ui, &resp, me, 1.0, true, true);
    let _ = resp.on_hover_text(format!(
        "{target}: {} ({}). Drag to change, double-click for 0 dB, wheel for 0.1 dB",
        level_text(snd.level_db, true),
        if snd.pre_fader { "pre-fader" } else { "post-fader" }
    ));
    row.level_rect = r4(bar);
    let text = level_text(snd.level_db, wide);
    let hot = app.mixer.gesture.is_some_and(|g| g.target == me);
    readout(
        app,
        ui,
        &p,
        clip,
        Readout {
            target: me,
            salt: wide.into(),
            draw: lr,
            hit: above(lr),
            text: text.clone(),
            edit_text: fine::edit_text_db(snd.level_db),
            font: theme::mono(ui.ctx(), 10.0),
            color: if hot { TOKENS.accent } else { TOKENS.text },
            align: Align2::RIGHT_CENTER,
            drag: true,
        },
    );
    row.readout_rect = r4(lr);
    row.level_text = text;
    // Remove.
    let resp = ui.interact(xb.intersect(clip), ui.id().with(("send_remove", salt)), Sense::click());
    crate::icons::paint(&p, xb.center(), crate::icons::Icon::Remove, 11.0, if resp.hovered() { TOKENS.record } else { TOKENS.text_dim });
    if resp.on_hover_text("Remove send").clicked() {
        let _ = app.remove_send(id, slot);
    }
    row.remove = r4(xb);
    row
}

/// "+ Send": the busses a send from `t` can go to, then "New FX Channel…". Returns its rect.
pub(crate) fn add_send_row(app: &mut StudioApp, ui: &mut Ui, s: &Session, t: &Track, r: Rect, clip: Rect, menu: &mut Vec<MenuItem>) -> R4 {
    let resp = ui.interact(r.intersect(clip), ui.id().with(("add_send", t.id.0, r.min.x as i32)), Sense::click());
    let p = ui.painter().with_clip_rect(clip);
    if resp.hovered() {
        p.rect_filled(r, crate::theme::RADIUS_SM, TOKENS.button_hover);
    }
    p.text(pos2(r.min.x + 4.0, r.center().y), Align2::LEFT_CENTER, "+ Send", FontId::proportional(10.5), TOKENS.text_dim);
    let mut choices = send_targets(s, t, None);
    choices.push(("New FX Channel…".into(), NEW_FX.into()));
    push_unused_entry(s, &mut choices);
    if let Some(v) = choice_menu(&resp, "", &choices, menu) {
        if v == REMOVE_UNUSED {
            let _ = app.remove_unused_busses();
        } else {
            let _ = app.add_send(t.id, &v);
        }
    }
    r4(r)
}

/// A strip's sends block: `rows` rows at `y`, the sends in slot order, then "+ Send". When the
/// sends don't all fit, the last row says how many more there are (the device chain lists all).
#[allow(clippy::too_many_arguments)]
fn sends_block(
    app: &mut StudioApp,
    ui: &mut Ui,
    s: &Session,
    t: &Track,
    x0: f32,
    w: f32,
    y: f32,
    rows: usize,
    clip: Rect,
    sl: &mut StripLayout,
    menu: &mut Vec<MenuItem>,
) {
    if rows == 0 || !takes_sends(t.kind) {
        return;
    }
    let sends: Vec<(usize, SendSlot)> = t.mixer.sends.iter().enumerate().filter_map(|(i, x)| x.clone().map(|x| (i, x))).collect();
    let free = sends.len() < t.mixer.sends.len();
    let row_r = |i: usize| Rect::from_min_size(pos2(x0, y + i as f32 * SEND_ROW_H), vec2(w, SEND_ROW_H - 2.0));
    let fits = sends.len() + usize::from(free) <= rows;
    let shown = if fits { sends.len() } else { rows.saturating_sub(1) };
    for (i, (slot, snd)) in sends.iter().take(shown).enumerate() {
        let row = send_row(app, ui, s, t, *slot, snd, row_r(i), clip, false, menu);
        sl.sends.push(row);
    }
    if fits {
        if free {
            sl.add_send = add_send_row(app, ui, s, t, row_r(shown), clip, menu);
        }
    } else {
        let r = row_r(shown);
        let resp = ui.interact(r.intersect(clip), ui.id().with(("more_sends", t.id.0)), Sense::click());
        ui.painter().with_clip_rect(clip).text(
            pos2(r.min.x + 4.0, r.center().y),
            Align2::LEFT_CENTER,
            format!("{} more…", sends.len() - shown),
            FontId::proportional(10.5),
            TOKENS.text_dim,
        );
        if resp.on_hover_text("Show all sends in the device chain").clicked() {
            app.focus(t.id);
            app.ui.show_devices = true;
        }
    }
}

/// A strip's insert rows at `y`: one per occupied slot (click: the plug-in's editor; right-click:
/// bypass and remove), then a "+" row that adds an effect to this track. `rows` rows in all, so
/// every strip's faders line up; with more inserts than fit, the last row says how many more.
#[allow(clippy::too_many_arguments)]
fn inserts_block(
    app: &mut StudioApp,
    ui: &mut Ui,
    t: &Track,
    x0: f32,
    w: f32,
    y: f32,
    rows: usize,
    clip: Rect,
    sl: &mut StripLayout,
    menu: &mut Vec<MenuItem>,
) {
    let list: Vec<(usize, soundcraft_model::Insert)> = t.mixer.inserts.iter().enumerate().filter_map(|(i, x)| x.clone().map(|x| (i, x))).collect();
    let free = list.len() < t.mixer.inserts.len();
    let row_r = |i: usize| Rect::from_min_size(pos2(x0, y + i as f32 * INSERT_ROW_H), vec2(w, INSERT_ROW_H - 2.0));
    // The last row is always "+" (while a slot is free); a list that does not fit above it gives
    // up its last visible row to "n more…". With a single row, only "+".
    let room = rows.saturating_sub(usize::from(free));
    let fits = list.len() <= room;
    let shown = if fits { list.len() } else { room.saturating_sub(1) };
    let p = ui.painter().with_clip_rect(clip);
    let id = t.id;
    for (i, (slot, ins)) in list.iter().take(shown).enumerate() {
        let rr = row_r(i);
        let name = app.plugin_name(&ins.plugin, false);
        let resp = ui.interact(rr.intersect(clip), ui.id().with(("insert", id.0, *slot)), Sense::click());
        p.rect_filled(rr, crate::theme::RADIUS_SM, if resp.hovered() { TOKENS.button_hover } else { TOKENS.bg });
        let text_r = rr.shrink2(vec2(4.0, 0.0));
        p.with_clip_rect(text_r.intersect(clip)).text(
            text_r.left_center(),
            Align2::LEFT_CENTER,
            &name,
            FontId::proportional(10.5),
            if ins.bypass { TOKENS.text_dim } else { TOKENS.text },
        );
        let enabled = app.player.as_ref().is_some_and(|pl| crate::devices::hosted(&ins.plugin) || pl.has_editor(id, *slot));
        if resp.clicked() && enabled {
            app.want_editor((id, *slot));
        }
        let state = if ins.bypass { " (bypassed)" } else { "" };
        let resp = resp.on_hover_text(format!("{name}{state}. Click for its window, right-click for more"));
        let slot = *slot;
        let bypass = ins.bypass;
        egui::Popup::context_menu(&resp).id(resp.id.with("insert_ctx")).show(|ui| {
            let label = if bypass { "Turn On" } else { "Bypass" };
            let b = ui.button(label);
            menu.push(MenuItem::at(ui, label.into(), "bypass".into(), b.rect));
            if b.clicked() {
                let _ = app.toggle_bypass(id, slot);
                ui.close();
            }
            let b = ui.button("Remove");
            menu.push(MenuItem::at(ui, "Remove".into(), "remove".into(), b.rect));
            if b.clicked() {
                let _ = app.remove_device(id, slot);
                ui.close();
            }
        });
        sl.inserts.push(InsertRow { slot, name, bypass, rect: r4(rr) });
    }
    if rows == 0 {
        return;
    }
    if !fits {
        let rr = row_r(shown);
        let resp = ui.interact(rr.intersect(clip), ui.id().with(("more_inserts", id.0)), Sense::click());
        p.text(
            pos2(rr.min.x + 4.0, rr.center().y),
            Align2::LEFT_CENTER,
            format!("{} more…", list.len() - shown),
            FontId::proportional(10.5),
            TOKENS.text_dim,
        );
        if resp.on_hover_text("Show all devices in the device chain").clicked() {
            app.focus(id);
            app.ui.show_devices = true;
        }
    }
    if free {
        let rr = row_r(rows - 1);
        let resp = ui.interact(rr.intersect(clip), ui.id().with(("add_insert", id.0)), Sense::click());
        if resp.hovered() {
            p.rect_filled(rr, crate::theme::RADIUS_SM, TOKENS.button_hover);
        }
        crate::icons::paint(&p, pos2(rr.min.x + 9.0, rr.center().y), crate::icons::Icon::Plus, 12.0, TOKENS.text_dim);
        let resp = resp.on_hover_text("Add an effect to this track");
        let before = list.len();
        crate::devices::add_device_menu(app, &resp, id, menu);
        if app.engine.session().track(id).is_some_and(|x| x.mixer.inserts.iter().flatten().count() > before) {
            app.focus(id);
        }
        sl.add_insert = r4(rr);
    }
}

/// One channel strip.
#[allow(clippy::too_many_arguments)]
fn strip(
    app: &mut StudioApp,
    ui: &mut Ui,
    s: &Session,
    t: &Track,
    r: Rect,
    clip: Rect,
    focused: bool,
    (rows, ins_rows): (usize, usize),
    menu: &mut Vec<MenuItem>,
) -> StripLayout {
    let id = t.id;
    let m = &t.mixer;
    // Under Read automation the controls show the lane's value at the playhead (no command, no edit).
    let vol_db = app.followed(t, &crate::automation::Lane::Volume).unwrap_or(m.volume_db);
    let bal = app.followed(t, &crate::automation::Lane::Pan).unwrap_or_else(|| balance_of(&m.pan));
    let p = ui.painter().with_clip_rect(clip);
    p.rect_filled(r, crate::theme::RADIUS, if focused { TOKENS.header_selected } else { TOKENS.panel });
    let mut sl = StripLayout {
        track: id.0,
        name: t.name.clone(),
        kind: Some(t.kind),
        rect: r4(r),
        volume_db: vol_db,
        balance: bal,
        pan: m.pan.clone(),
        level: app.meters.get(&id).map_or([0.0; 2], |d| d.level),
        fx: t.kind == TrackKind::Aux,
        focused,
        folder: t.folder.map(|f| f.0),
        ..StripLayout::default()
    };
    let x0 = r.min.x + 4.0;
    let w = r.width() - 8.0;
    // Name and color: click focuses the track.
    let name_r = Rect::from_min_size(pos2(r.min.x, r.min.y), vec2(r.width(), 22.0));
    p.rect_filled(Rect::from_min_size(name_r.min, vec2(r.width(), 4.0)), crate::theme::top_corners(), theme::rgb(t.color));
    let mut tr = name_r.shrink2(vec2(5.0, 0.0));
    if sl.fx {
        // FX channels: a small "FX" tag and a muted name.
        let tag = Rect::from_min_size(pos2(tr.max.x - 18.0, tr.center().y - 5.0), vec2(18.0, 13.0));
        p.rect_filled(tag, crate::theme::RADIUS_SM, TOKENS.button);
        p.text(tag.center(), Align2::CENTER_CENTER, "FX", FontId::proportional(9.5), TOKENS.text_dim);
        tr.max.x = tag.min.x - 3.0;
    }
    p.with_clip_rect(tr.intersect(clip)).text(
        pos2(tr.min.x, tr.center().y + 2.0),
        Align2::LEFT_CENTER,
        &t.name,
        FontId::proportional(13.0),
        if focused {
            TOKENS.accent
        } else if sl.fx {
            TOKENS.text_dim
        } else {
            TOKENS.text
        },
    );
    sl.name_rect = r4(name_r);
    // Anywhere on the strip focuses it; controls drawn after this take their own clicks.
    let resp = ui.interact(r.intersect(clip), ui.id().with(("strip_name", id.0)), Sense::click());
    if resp.clicked() {
        let shift = ui.input(|i| i.modifiers.shift);
        app.focus_header(id, shift);
    }
    // Slice 6: double-click the name to rename the track in place.
    if resp.double_clicked() && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| name_r.contains(p)) {
        app.start_rename(crate::finish::RenameTarget::Track(id), r4(name_r));
    }
    let mut y = r.min.y + 26.0;
    // Input (audio and aux tracks; instrument input is MIDI and there is no MIDI device layer).
    let in_r = Rect::from_min_size(pos2(x0, y), vec2(w, 16.0));
    if matches!(t.kind, TrackKind::Audio | TrackKind::Aux) {
        let cur = route_label(s, &m.input, true, if t.kind == TrackKind::Audio { t.channels() } else { 0 });
        let mut choices = input_choices(s, t, app.device_inputs());
        push_unused_entry(s, &mut choices);
        if let Some(v) = dropdown(ui, clip, in_r, ui.id().with(("input", id.0)), &cur, &choices, menu) {
            if v == REMOVE_UNUSED {
                let _ = app.remove_unused_busses();
            } else {
                let _ = app.set_input(id, &v);
            }
        }
        sl.input = r4(in_r);
        sl.input_label = cur;
    } else {
        let label = if t.kind.is_midi() { "MIDI in" } else { "" };
        p.text(in_r.left_center() + vec2(4.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(10.0), TOKENS.text_dim);
    }
    y += 19.0;
    // Inserts (Logic: under the input), then sends.
    inserts_block(app, ui, t, x0, w, y, ins_rows, clip, &mut sl, menu);
    y += ins_rows as f32 * INSERT_ROW_H;
    // Sends (Logic: between the input and the output).
    sends_block(app, ui, s, t, x0, w, y, rows, clip, &mut sl, menu);
    y += rows as f32 * SEND_ROW_H;
    // Output.
    let out_r = Rect::from_min_size(pos2(x0, y), vec2(w, 16.0));
    let cur = route_label(s, &m.output, false, t.channels());
    let mut choices = output_choices(s, t);
    push_unused_entry(s, &mut choices);
    if let Some(v) = dropdown(ui, clip, out_r, ui.id().with(("output", id.0)), &cur, &choices, menu) {
        if v == REMOVE_UNUSED {
            let _ = app.remove_unused_busses();
        } else {
            let _ = app.set_output(id, &v);
        }
    }
    sl.output = r4(out_r);
    sl.output_label = cur;
    y += 20.0;
    y += 4.0;
    // Balance knob.
    let knob = Rect::from_center_size(pos2(r.center().x, y + 18.0), vec2(34.0, 34.0));
    sl.knob = r4(knob);
    let b = bal;
    let has_pan = !m.pan.is_empty();
    let pan = Target::Pan(id);
    let resp = ui.interact(knob.intersect(clip), ui.id().with(("knob", id.0)), Sense::click_and_drag());
    let hot = resp.hovered() || app.mixer.gesture.is_some_and(|g| g.target == pan);
    draw_knob(&p, knob, b, hot);
    if has_pan {
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        }
        control(app, ui, &resp, pan, 1.0, true, true);
    }
    let pan_r = Rect::from_center_size(pos2(r.center().x, knob.max.y + 7.0), vec2(34.0, 12.0));
    let pan_text = fine::pan_text(b);
    if has_pan {
        readout(
            app,
            ui,
            &p,
            clip,
            Readout {
                target: pan,
                salt: 0,
                draw: pan_r,
                hit: pan_r,
                text: pan_text.clone(),
                edit_text: pan_text.clone(),
                font: theme::mono(ui.ctx(), 9.5),
                color: TOKENS.text_dim,
                align: Align2::CENTER_CENTER,
                drag: false,
            },
        );
    } else {
        p.text(pan_r.center(), Align2::CENTER_CENTER, &pan_text, theme::mono(ui.ctx(), 9.5), TOKENS.text_dim);
    }
    sl.pan_readout_rect = r4(pan_r);
    sl.pan_text = pan_text;
    y = knob.max.y + 16.0;
    // M S / R I.
    let bw = (w - 3.0) / 2.0;
    let cell = |i: usize, row: f32| Rect::from_min_size(pos2(x0 + (i as f32) * (bw + 3.0), row), vec2(bw, 16.0));
    let buttons: [(&str, bool, Color32, &str, bool, Rect); 4] = [
        ("M", m.mute, TOKENS.mute, "mix.mute", true, cell(0, y)),
        ("S", m.solo, TOKENS.solo, "mix.solo", true, cell(1, y)),
        ("R", m.record_arm, TOKENS.record, "mix.record_arm", t.kind.has_playlist(), cell(0, y + 19.0)),
        ("I", m.input_monitor, TOKENS.monitor, "mix.input_monitor", t.kind == TrackKind::Audio, cell(1, y + 19.0)),
    ];
    if t.kind == TrackKind::Master {
        // A master has no solo, record or input.
        let resp = small_button(ui, clip, cell(0, y), ui.id().with(("mix.mute", id.0)), "M", m.mute, TOKENS.mute, true);
        if resp.clicked() {
            let _ = app.run("mix.mute", json!({"tracks": [id.0], "value": !m.mute}));
        }
        sl.mute = r4(cell(0, y));
    } else {
        for (label, on, color, cmd, enabled, br) in buttons {
            let resp = small_button(ui, clip, br, ui.id().with((cmd, id.0, "strip")), label, on, color, enabled);
            if resp.clicked() && enabled {
                let _ = app.run(cmd, json!({"tracks": [id.0], "value": !on}));
            }
            match label {
                "M" => sl.mute = r4(br),
                "S" => sl.solo = r4(br),
                "R" => sl.arm = r4(br),
                _ => sl.monitor = r4(br),
            }
        }
    }
    y += 42.0;
    // The held peak above the meter.
    let peak_r = Rect::from_min_max(pos2(r.center().x + 2.0, y + 1.0), pos2(r.max.x - 6.0, y + 14.0));
    let held = app.meters.get(&id).map_or(0.0, |d| d.peak);
    let (peak_text, over) = peak_text(held);
    let over = over || app.meters.get(&id).is_some_and(|d| d.clip);
    let resp = ui.interact(peak_r.intersect(clip), ui.id().with(("peak", id.0)), Sense::click());
    p.rect_filled(peak_r, crate::theme::RADIUS_SM, TOKENS.bg);
    p.with_clip_rect(peak_r).text(
        peak_r.center(),
        Align2::CENTER_CENTER,
        &peak_text,
        FontId::proportional(10.0),
        if over {
            TOKENS.record
        } else if resp.hovered() {
            TOKENS.accent
        } else {
            TOKENS.text_dim
        },
    );
    if resp.on_hover_text("Peak hold. Click to reset, Ctrl-click to reset all").clicked() {
        let all = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        app.reset_meters(if all { None } else { Some(id) });
    }
    sl.peak_rect = r4(peak_r);
    sl.peak_text = peak_text;
    y += 12.0;
    // Fader and meter.
    let bottom = r.max.y - 22.0;
    let fader = Rect::from_min_max(pos2(x0 + 4.0, y + 6.0), pos2(r.center().x + 4.0, bottom - 6.0));
    let meter = Rect::from_min_max(pos2(r.center().x + 12.0, y + 6.0), pos2(r.max.x - 10.0, bottom - 6.0));
    sl.fader = r4(fader);
    sl.meter = r4(meter);
    let d = app.meters.get(&id).copied().unwrap_or_default();
    draw_meter(&p, meter, d.level, d.clip);
    let resp = ui.interact(meter.intersect(clip), ui.id().with(("meter", id.0)), Sense::click());
    if resp.on_hover_text("Click to clear the clip latch and the peak hold, Ctrl-click for every strip").clicked() {
        let all = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        app.reset_meters(if all { None } else { Some(id) });
    }
    let ((cy, cs), (zy, zs)) = fader_widget(app, ui, &p, clip, id, vol_db, fader);
    (sl.cap_line_y, sl.cap_line_span, sl.zero_tick_y, sl.zero_tick_span) = (cy, cs, zy, zs);
    let db = vol_db;
    let text = fine::db_text(db);
    let ro = Rect::from_min_max(pos2(x0, bottom), pos2(r.max.x - 4.0, r.max.y - 4.0));
    p.rect_filled(ro, crate::theme::RADIUS_SM, TOKENS.bg);
    let fader_id = Target::Fader(id);
    let hot = app.mixer.gesture.is_some_and(|g| g.target == fader_id);
    readout(
        app,
        ui,
        &p,
        clip,
        Readout {
            target: fader_id,
            salt: 0,
            draw: ro,
            hit: ro,
            text: text.clone(),
            edit_text: fine::edit_text_db(db),
            font: theme::mono(ui.ctx(), 10.5),
            color: if hot { TOKENS.accent } else { TOKENS.text },
            align: Align2::CENTER_CENTER,
            drag: false,
        },
    );
    sl.readout_rect = r4(ro);
    sl.volume_text = text;
    sl
}

/// A held peak (linear) as the readout shows it: "-3.2", "0.0", "+1.4" (red from 0 dBFS), "-inf".
pub fn peak_text(lin: f32) -> (String, bool) {
    if lin <= 0.0 {
        return ("-inf".into(), false);
    }
    let db = 20.0 * lin.log10();
    let t = format!("{db:.1}");
    let over = db >= -0.05;
    (
        if db > 0.05 {
            format!("+{t}")
        } else if t == "-0.0" {
            "0.0".into()
        } else {
            t
        },
        over,
    )
}

fn draw_knob(p: &egui::Painter, r: Rect, b: f32, hot: bool) {
    // Flat: a thin ring for the travel and the accent from the center to the value.
    let c = r.center();
    let rad = r.width() / 2.0 - 4.0;
    let ang = |v: f32| (-90.0 + v * 135.0_f32).to_radians();
    let arc = |from: f32, to: f32| -> Vec<egui::Pos2> {
        let n = 24;
        (0..=n).map(|i| from + (to - from) * i as f32 / n as f32).map(|v| c + vec2(ang(v).cos(), ang(v).sin()) * rad).collect()
    };
    p.add(egui::Shape::line(arc(-1.0, 1.0), Stroke::new(3.0, TOKENS.button)));
    // The center notch: a short line across the ring at the top.
    p.line_segment([c + vec2(0.0, -(rad - 3.5)), c + vec2(0.0, -(rad + 3.5))], Stroke::new(1.0, TOKENS.text_dim));
    p.add(egui::Shape::line(arc(0.0, b), Stroke::new(3.0, if hot { theme::lighten(TOKENS.accent, 0.15) } else { TOKENS.accent })));
    p.circle_filled(c + vec2(ang(b).cos(), ang(b).sin()) * rad, 2.5, TOKENS.text);
}

/// A one-point line at `y` as whole physical pixels: the y of its center, the rows it covers
/// ([top, bottom)) and its thickness in points, so the line is crisp at any scale.
fn snap_line(y: f32, ppp: f32) -> (f32, [i32; 2], f32) {
    let n = ppp.round().max(1.0);
    let top = (y * ppp - n / 2.0).round();
    ((top + n / 2.0) / ppp, [top as i32, (top + n) as i32], n / ppp)
}

type Mark = (Option<f32>, Option<[i32; 2]>);

/// Draws the fader; returns the (center y, pixel rows) of the cap's center line and of the 0 dB tick.
fn fader_widget(app: &mut StudioApp, ui: &mut Ui, p: &egui::Painter, clip: Rect, id: TrackId, db: f32, r: Rect) -> (Mark, Mark) {
    let ppp = ui.ctx().pixels_per_point();
    let mut zero = (None, None);
    let me = Target::Fader(id);
    let resp = ui.interact(r.expand2(vec2(4.0, 6.0)).intersect(clip), ui.id().with(("fader", id.0)), Sense::click_and_drag());
    // Track and marks: a line across the track at 0 dB, fainter ticks at -6, -12 and -24.
    let groove = Rect::from_center_size(r.center(), vec2(4.0, r.height()));
    p.rect_filled(groove, crate::theme::RADIUS_SM, TOKENS.bg);
    let y_of = |k: f32| r.max.y - k * r.height();
    for mark in fine::DB_MARKS {
        let (yy, rows, w) = snap_line(y_of(db_to_k(mark)), ppp);
        if mark == 0.0 {
            zero = (Some(yy), Some(rows));
            p.line_segment([pos2(r.min.x, yy), pos2(r.max.x, yy)], Stroke::new(w, TOKENS.text_dim));
        } else {
            p.line_segment([pos2(r.center().x - 8.0, yy), pos2(r.center().x + 8.0, yy)], Stroke::new(w, TOKENS.text_dim.gamma_multiply(0.5)));
        }
    }
    let k = db_to_k(db);
    let (cap_y, cap_rows, cap_w) = snap_line(y_of(k), ppp);
    let cap = Rect::from_center_size(pos2(r.center().x, cap_y), vec2(r.width().min(26.0), 12.0));
    let hot = resp.hovered() || app.mixer.gesture.is_some_and(|g| g.target == me);
    p.rect_filled(cap, crate::theme::RADIUS_SM, if hot { TOKENS.accent } else { TOKENS.text_dim });
    // The line through the cap marks the exact point the value sits at, to line up with the 0 dB line.
    p.line_segment([pos2(cap.min.x, cap_y), pos2(cap.max.x, cap_y)], Stroke::new(cap_w, TOKENS.bg));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
    }
    control(app, ui, &resp, me, r.height().max(1.0), true, true);
    ((Some(cap_y), Some(cap_rows)), zero)
}

/// A number under a control that is also a way in: double-click types a value in its place,
/// the wheel nudges it, Alt-click and Ctrl-click reset it.
struct Readout {
    target: Target,
    /// Tells apart two readouts of one control (a strip's and the device chain's).
    salt: u64,
    /// Where the text is laid out, and where the pointer hits it.
    draw: Rect,
    hit: Rect,
    text: String,
    /// What the field starts as when editing opens.
    edit_text: String,
    font: FontId,
    color: Color32,
    align: Align2,
    /// Also drag it like the control (a send's number).
    drag: bool,
}

fn readout(app: &mut StudioApp, ui: &mut Ui, p: &egui::Painter, clip: Rect, ro: Readout) {
    let id = ui.id().with(("readout", ro.target.salt(), ro.salt));
    if app.mixer.edit.as_ref().is_some_and(|e| e.target == ro.target && e.salt == ro.salt) {
        edit_field(app, ui, ro.draw, id.with("edit"), ro.font);
        return;
    }
    let sense = if ro.drag { Sense::click_and_drag() } else { Sense::click() };
    let resp = ui.interact(ro.hit.intersect(clip), id, sense);
    let color = if resp.hovered() { TOKENS.accent } else { ro.color };
    let at = ro.align.pos_in_rect(&ro.draw.shrink2(vec2(2.0, 0.0)));
    p.with_clip_rect(ro.draw.intersect(clip)).text(
        at + if ro.align == Align2::RIGHT_CENTER { vec2(0.0, -1.0) } else { vec2(0.0, 0.0) },
        ro.align,
        &ro.text,
        ro.font,
        color,
    );
    control(app, ui, &resp, ro.target, 1.0, ro.drag, false);
    if resp.double_clicked() {
        app.mixer.edit = Some(fine::ReadoutEdit { target: ro.target, salt: ro.salt, text: ro.edit_text, fresh: true });
    }
}

/// The in-place text field of a readout being edited: Enter applies, Escape and losing focus
/// cancel.
fn edit_field(app: &mut StudioApp, ui: &mut Ui, draw: Rect, id: egui::Id, font: FontId) {
    let Some(mut e) = app.mixer.edit.take() else { return };
    let rect = Rect::from_center_size(draw.center(), vec2(draw.width().max(46.0), draw.height().max(16.0)));
    app.mixer.edit_rect = r4(rect);
    let (esc, enter) = ui.input(|i| {
        use crate::shortcuts::{Action, hit};
        (hit(i, Action::ReadoutCancel), hit(i, Action::ReadoutApply))
    });
    if e.fresh {
        e.fresh = false;
        // Focused with the whole text selected, so typing replaces it.
        let n = e.text.chars().count();
        let mut st = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
        st.store(ui.ctx(), id);
        ui.memory_mut(|m| m.request_focus(id));
    }
    let field = egui::TextEdit::singleline(&mut e.text)
        .id(id)
        .font(font)
        .margin(vec2(3.0, 1.0))
        .horizontal_align(egui::Align::Center)
        .desired_width(rect.width());
    let resp = ui.put(rect, field);
    if esc {
        return;
    }
    if resp.lost_focus() {
        if enter {
            let v = if e.target.is_pan() { fine::parse_pan(&e.text) } else { fine::parse_db(&e.text) };
            match v {
                Some(v) => {
                    let _ = app.set_value(e.target, v);
                }
                None => app.status = format!("\"{}\" is not a {}", e.text.trim(), if e.target.is_pan() { "pan position" } else { "level" }),
            }
        }
        return;
    }
    app.mixer.edit = Some(e);
}

/// What a fader, knob or send level does with the pointer, the wheel and the keyboard:
/// - drag moves it (Shift: ten times finer); a slow drag snaps to a mark;
/// - double-click (`dbl_reset`), Alt-click, Ctrl-click and Delete reset it (0 dB, or center);
/// - the wheel nudges it, and the nudges of one gesture are one undo step.
fn control(app: &mut StudioApp, ui: &Ui, resp: &egui::Response, t: Target, height: f32, drag: bool, dbl_reset: bool) {
    let (mods, now) = ui.input(|i| (i.modifiers, i.time));
    if resp.hovered() || resp.dragged() {
        app.mixer.hover = Some(t);
        app.mixer.wheel_taken = true;
    }
    if (resp.clicked() && (mods.alt || mods.ctrl || mods.command)) || (dbl_reset && resp.double_clicked()) {
        app.reset_control(t);
        return;
    }
    if drag {
        if resp.drag_started() {
            // A write pass (slice 7) holds one undo step for the pass and the drags in it.
            if !crate::automation::passing(app) {
                app.engine.end_merge();
            }
            app.mixer.wheel = None;
            let raw = app.shown_value(t).map_or(0.0, |v| if t.is_pan() { v } else { db_to_k(v) });
            app.mixer.gesture = Some(MixGesture { target: t, raw, height });
        }
        if resp.dragged()
            && let Some(mut g) = app.mixer.gesture.filter(|g| g.target == t)
        {
            let (step, now_delta) = ui.input(|i| {
                let d = if resp.drag_started() {
                    i.pointer.press_origin().zip(i.pointer.latest_pos()).map_or(vec2(0.0, 0.0), |(a, b)| b - a)
                } else {
                    i.pointer.delta()
                };
                (d, i.pointer.delta().length())
            });
            let ratio = if mods.shift { fine::FINE } else { 1.0 };
            let per_px = match t {
                Target::Fader(_) => -step.y / g.height,
                Target::Send(..) => (step.x - step.y) / 150.0,
                Target::Pan(_) => -step.y / 100.0,
                Target::SendPan(..) => (step.x - step.y) / 100.0,
            };
            let (lo, hi) = if t.is_pan() { (-1.0, 1.0) } else { (0.0, 1.0) };
            g.raw = (g.raw + per_px * ratio).clamp(lo, hi);
            app.mixer.gesture = Some(g);
            let slow = now_delta < fine::SLOW_PT && !mods.shift;
            let v = match t {
                Target::Pan(_) | Target::SendPan(..) if slow => fine::snap_pan(g.raw),
                Target::Pan(_) | Target::SendPan(..) => g.raw,
                Target::Fader(_) => {
                    let db = k_to_db(g.raw);
                    if slow { fine::snap_db(db, &fine::DB_MARKS) } else { db }
                }
                Target::Send(..) => {
                    let db = k_to_db(g.raw);
                    if slow { fine::snap_db(db, &fine::SEND_MARKS) } else { db }
                }
            };
            let _ = app.set_value_merged(t, v, None);
        }
        if resp.drag_stopped() && app.mixer.gesture.is_some_and(|g| g.target == t) {
            app.mixer.gesture = None;
        }
    }
    if resp.hovered() && app.mixer.gesture.is_none() {
        let (notches, shift) = ui.input(|i| {
            let mut n = 0.0;
            let mut shift = false;
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, modifiers, .. } = e {
                    let d = if delta.x.abs() > delta.y.abs() { delta.x } else { delta.y };
                    n += match unit {
                        egui::MouseWheelUnit::Point => d / 50.0,
                        _ => d,
                    };
                    shift |= modifiers.shift;
                }
            }
            (n, shift)
        });
        if notches != 0.0 {
            app.wheel_nudge(t, notches, shift, resp.rect, now);
        }
    }
}
