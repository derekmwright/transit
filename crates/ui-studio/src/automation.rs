//! Slice 7: automation lanes under a track, the track's automation mode, and the write pass.
//!
//! The "A" toggle on a track header (Ableton's automation toggle; the A key does the same for the
//! focused track) shows the track's lanes under its row, opening Volume the first time. Each lane
//! has a header in the track list (its parameter, picked from Volume, Pan, Mute, each send's
//! level, pan and mute, and every device parameter; "+" adds a lane, "x" closes it; the first lane
//! also has the track's mode: Off, Read, Touch, Latch, Write) and draws its points on the
//! arrangement's time axis at the arrangement's zoom. Double-click adds a point (snapped to the
//! visible grid), dragging a point moves it (vertical: value, horizontal: time; Shift: ten times
//! finer and no snap), a drag on empty lane rubber-band selects, Delete removes the selected
//! points. Each gesture is one undo step. Which lanes are open and their heights are view state
//! in [`crate::UiState`], never in the document.
//!
//! While playing, tracks in Write record the fader, and tracks in Touch or Latch record the control
//! being moved (Latch keeps writing it after release until Stop), through `automation.set_point`
//! with `from` (the write pass `soundcraft-ui-egui` runs, plan 06 section 3). A pass is one undo
//! step. Clicking a fader, knob or send highlights its lane when that lane is open.
//!
//! Model and engine gaps (plan 06 leaks): lanes are linear only (no curve shapes, L38); the mix
//! does not read send pan or send mute lanes (L39); hosted plug-ins list only the parameters the
//! document already holds (no Player query for a plug-in's parameters, L40). Stereo pan is two
//! panners (L10): the Pan lane is a balance lane that writes both panners' lanes.

use crate::StudioApp;
use crate::arrange::{Map, R4, RowGeo, r4};
use crate::fine::{self, Target};
use crate::grid::GridView;
use crate::mixer::{MenuItem, balance_of, db_to_k, k_to_db, pans_for};
use crate::theme::{self, TOKENS};
use egui::{Align2, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_model::{AutoParam, AutomationMode, Route, Session, Track, TrackId};
use soundcraft_time::Samples;
use std::collections::HashMap;

/// A new lane's height, and the range a drag on its bottom edge keeps it in (points).
pub const LANE_H: f32 = 56.0;
pub const LANE_MIN: f32 = 40.0;
pub const LANE_MAX: f32 = 240.0;
/// Space above and below the curve inside a lane.
const PAD: f32 = 7.0;
/// How close (points) a press must be to a point to grab it.
const GRAB: f32 = 7.0;
/// The write pass writes a point at most this often (seconds).
const WRITE_STEP_S: f64 = 0.02;
/// A point drag moves in time only after the pointer travels this far sideways (points).
const DRAG_SLOP: f32 = 3.0;
/// A gap of more than this many write steps is a locate: the pass starts a new segment.
const WRITE_GAP_STEPS: i64 = 4;
/// The merge key of a write pass.
const PASS: &str = "automation_pass";
/// The modes the mode menu offers (Logic's terms; the engine's Touch/Latch and Trim are left out).
pub const MODES: [AutomationMode; 5] =
    [AutomationMode::Off, AutomationMode::Read, AutomationMode::Touch, AutomationMode::Latch, AutomationMode::Write];

/// An open lane: which track, which parameter (a [`Lane`] id) and how tall. View state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LaneView {
    pub track: TrackId,
    pub lane: String,
    pub height: f32,
}

/// What a lane automates, as the UI sees it. Ids are the engine's `AutoParam` ids, except that
/// `pan` is the balance of all the track's panners (L10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lane {
    Volume,
    Pan,
    Mute,
    SendLevel(u8),
    SendPan(u8),
    SendMute(u8),
    Plugin { slot: u8, param: String },
}

/// A slot's letter, A..J (the model's own helper is private to it).
fn slot_letter(i: u8) -> char {
    char::from(b'A'.saturating_add(i.min(25)))
}

fn letter(i: u8) -> char {
    slot_letter(i).to_ascii_lowercase()
}

impl Lane {
    pub fn parse(id: &str) -> Option<Lane> {
        Some(match AutoParam::parse(id)? {
            AutoParam::Volume => Lane::Volume,
            AutoParam::Pan(_) => Lane::Pan,
            AutoParam::Mute => Lane::Mute,
            AutoParam::SendLevel(i) => Lane::SendLevel(i),
            AutoParam::SendPan(i) => Lane::SendPan(i),
            AutoParam::SendMute(i) => Lane::SendMute(i),
            AutoParam::Plugin { slot, param } => Lane::Plugin { slot, param },
        })
    }

    pub fn id(&self) -> String {
        match self {
            Lane::Volume => "volume".into(),
            Lane::Pan => "pan".into(),
            Lane::Mute => "mute".into(),
            Lane::SendLevel(i) => format!("send_{}_level", letter(*i)),
            Lane::SendPan(i) => format!("send_{}_pan", letter(*i)),
            Lane::SendMute(i) => format!("send_{}_mute", letter(*i)),
            Lane::Plugin { slot, param } if *slot == AutoParam::INSTRUMENT_SLOT => format!("plugin:instrument:{param}"),
            Lane::Plugin { slot, param } => format!("plugin:{}:{param}", letter(*slot)),
        }
    }

    /// The lane of a plug-in parameter: an insert's (`slot` the insert index) or, for
    /// [`soundcraft_mix::INSTRUMENT_SLOT`], the instrument's. (`fine::Target` is `Copy`, which a
    /// parameter id is not, so plug-in gestures reach the write pass as [`ParamTouch`]es.)
    pub fn of_param(track: TrackId, slot: usize, param: &str) -> Option<(TrackId, Lane)> {
        let slot = if slot == soundcraft_mix::INSTRUMENT_SLOT {
            AutoParam::INSTRUMENT_SLOT
        } else {
            u8::try_from(slot).ok().filter(|s| *s < u8::MAX - 1)?
        };
        Some((track, Lane::Plugin { slot, param: param.to_string() }))
    }

    /// The device a plug-in lane addresses.
    fn device<'a>(&self, t: &'a Track) -> Option<&'a soundcraft_model::Insert> {
        let Lane::Plugin { slot, .. } = self else { return None };
        if *slot == AutoParam::INSTRUMENT_SLOT { t.instrument.as_ref() } else { t.mixer.inserts.get(usize::from(*slot))?.as_ref() }
    }

    /// The control a lane belongs to, for the write pass and the mixer link.
    pub fn of_target(t: Target) -> (TrackId, Lane) {
        match t {
            Target::Fader(id) => (id, Lane::Volume),
            Target::Pan(id) => (id, Lane::Pan),
            Target::Send(id, s) => (id, Lane::SendLevel(u8::try_from(s).unwrap_or(0))),
            Target::SendPan(id, s) => (id, Lane::SendPan(u8::try_from(s).unwrap_or(0))),
        }
    }

    /// The engine parameter ids this lane writes: the Pan lane writes every panner.
    fn param_ids(&self, t: &Track) -> Vec<String> {
        match self {
            Lane::Pan => (0..t.mixer.pan.len().max(1)).map(|i| if i == 0 { "pan".to_string() } else { format!("pan{}", i + 1) }).collect(),
            other => vec![other.id()],
        }
    }

    fn params(&self, t: &Track) -> Vec<AutoParam> {
        self.param_ids(t).iter().filter_map(|p| AutoParam::parse(p)).collect()
    }

    /// Whether the lane still has something to automate (a removed send or device has not).
    pub fn valid(&self, t: &Track) -> bool {
        match self {
            Lane::SendLevel(i) | Lane::SendPan(i) | Lane::SendMute(i) => t.mixer.sends.get(usize::from(*i)).is_some_and(Option::is_some),
            Lane::Plugin { .. } => self.device(t).is_some(),
            _ => true,
        }
    }

    fn is_step(&self) -> bool {
        matches!(self, Lane::Mute | Lane::SendMute(_))
    }

    fn info(&self, t: &Track) -> Option<&'static soundcraft_dsp::ParamInfo> {
        let Lane::Plugin { param, .. } = self else { return None };
        device_info(&self.device(t)?.plugin)?.params.iter().find(|p| p.id == param)
    }

    /// The value the parameter has without automation.
    pub fn static_value(&self, t: &Track) -> f32 {
        let send = |i: &u8| t.mixer.sends.get(usize::from(*i)).and_then(Option::as_ref);
        match self {
            Lane::Volume => t.mixer.volume_db,
            Lane::Pan => balance_of(&t.mixer.pan),
            Lane::Mute => f32::from(u8::from(t.mixer.mute)),
            Lane::SendLevel(i) => send(i).map_or(fine::DB_MIN, |s| s.level_db),
            Lane::SendPan(i) => send(i).map_or(0.0, |s| s.pan),
            Lane::SendMute(i) => send(i).map_or(0.0, |s| f32::from(u8::from(s.mute))),
            Lane::Plugin { param, .. } => {
                self.device(t).and_then(|i| i.params.get(param).copied()).or_else(|| self.info(t).map(|i| i.default)).unwrap_or(0.0)
            }
        }
    }

    /// The lane's value at `at`: its curve where it has points, else the static value.
    pub fn value_at(&self, t: &Track, at: Samples) -> f32 {
        match self {
            Lane::Pan => {
                let pans: Vec<f32> = self
                    .params(t)
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let base = t.mixer.pan.get(i).copied().unwrap_or(0.0);
                        t.lane(p).filter(|l| !l.points.is_empty()).map_or(base, |l| l.value_at(at, base))
                    })
                    .collect();
                balance_of(&pans)
            }
            other => {
                let base = other.static_value(t);
                other.params(t).first().and_then(|p| t.lane(p)).filter(|l| !l.points.is_empty()).map_or(base, |l| l.value_at(at, base))
            }
        }
    }

    /// The lane's points (the first panner's times for the Pan lane), with the lane's value there.
    pub fn points(&self, t: &Track) -> Vec<(Samples, f32)> {
        let Some(l) = self.params(t).first().and_then(|p| t.lane(p)) else { return Vec::new() };
        l.points.iter().map(|p| (p.at, if *self == Lane::Pan { self.value_at(t, p.at) } else { p.value })).collect()
    }

    /// Value to lane height, 0 (bottom) .. 1 (top): the fader law for levels.
    pub fn norm(&self, t: &Track, v: f32) -> f32 {
        match self {
            Lane::Volume | Lane::SendLevel(_) => db_to_k(v),
            Lane::Pan | Lane::SendPan(_) => (v + 1.0) / 2.0,
            Lane::Mute | Lane::SendMute(_) => v.clamp(0.0, 1.0),
            Lane::Plugin { .. } => self.info(t).map_or(v.clamp(0.0, 1.0), |i| i.to_normalized(v)),
        }
    }

    pub fn denorm(&self, t: &Track, k: f32) -> f32 {
        let k = k.clamp(0.0, 1.0);
        match self {
            Lane::Volume | Lane::SendLevel(_) => k_to_db(k),
            Lane::Pan | Lane::SendPan(_) => k * 2.0 - 1.0,
            Lane::Mute | Lane::SendMute(_) => k.round(),
            Lane::Plugin { .. } => self.info(t).map_or(k, |i| i.from_normalized(k)),
        }
    }

    /// A value in the parameter's unit: dB, L/C/R, Muted, the device's own format.
    pub fn text(&self, t: &Track, v: f32) -> String {
        match self {
            Lane::Volume | Lane::SendLevel(_) => format!("{} dB", fine::db_text(v)),
            Lane::Pan | Lane::SendPan(_) => fine::pan_text(v),
            Lane::Mute | Lane::SendMute(_) => if v >= 0.5 { "Muted" } else { "On" }.into(),
            Lane::Plugin { .. } => self.info(t).map_or_else(|| format!("{:.0} %", v * 100.0), |i| i.format(v)),
        }
    }

    /// What the lane's header says.
    pub fn label(&self, app: &mut StudioApp, s: &Session, t: &Track) -> String {
        let send = |i: &u8| {
            let target = t.mixer.sends.get(usize::from(*i)).and_then(Option::as_ref).map(|x| match &x.target {
                Route::Bus(b) => crate::mixer::bus_label(s, *b),
                other => crate::mixer::route_label(s, other, false, 0),
            });
            format!("Send {} · {}", slot_letter(*i), target.unwrap_or_default())
        };
        match self {
            Lane::Volume => "Volume".into(),
            Lane::Pan => "Pan".into(),
            Lane::Mute => "Mute".into(),
            Lane::SendLevel(i) => format!("{} level", send(i)),
            Lane::SendPan(i) => format!("{} pan", send(i)),
            Lane::SendMute(i) => format!("{} mute", send(i)),
            Lane::Plugin { param, .. } => {
                let dev = self.device(t).map(|i| i.plugin.clone()).unwrap_or_default();
                let name = self.info(t).map_or_else(|| param.clone(), |i| i.name.to_string());
                format!("{} · {name}", app.plugin_name(&dev, false))
            }
        }
    }

    /// `automation.set_point` parameters for a point at `at` (one per panner for Pan).
    fn set_cmds(&self, t: &Track, at: Samples, v: f32, from: Option<Samples>) -> Vec<(&'static str, Value)> {
        let values = if *self == Lane::Pan { pans_for(v, t.mixer.pan.len().max(1)) } else { vec![v] };
        self.param_ids(t)
            .into_iter()
            .zip(values)
            .map(|(param, value)| {
                let mut p = json!({"track": t.id.0, "param": param, "at": at, "value": value});
                if let Some(f) = from {
                    p["from"] = f.into();
                }
                ("automation.set_point", p)
            })
            .collect()
    }

    /// `automation.clear` parameters removing exactly the point at `at` (never an empty range,
    /// which would clear the whole lane).
    fn clear_cmds(&self, t: &Track, at: Samples) -> Vec<(&'static str, Value)> {
        self.param_ids(t)
            .into_iter()
            .map(|param| ("automation.clear", json!({"track": t.id.0, "param": param, "start": at, "end": at.saturating_add(1)})))
            .collect()
    }
}

/// A device's parameter table: a built-in's, or a hosted plug-in's from the host registries'
/// cache. The cache is read only (no instantiation), so this never blocks the UI thread; a type
/// is cached once any instance of it has loaded (a project's plug-ins do on open).
pub fn device_info(plugin: &str) -> Option<&'static soundcraft_dsp::PluginInfo> {
    soundcraft_dsp::plugin_info(plugin)
        .or_else(|| plugin.starts_with(soundcraft_clap_host::ID_PREFIX).then(|| soundcraft_clap_host::cached_info(plugin)).flatten())
        .or_else(|| plugin.starts_with(soundcraft_vst3_host::ID_PREFIX).then(|| soundcraft_vst3_host::cached_info(plugin)).flatten())
}

/// The mixer's and the sends' lanes of `t`, as (section, label, lane id).
fn base_choices(app: &mut StudioApp, s: &Session, t: &Track) -> Vec<(String, String, String)> {
    let mut v = Vec::new();
    for l in [Lane::Volume, Lane::Pan, Lane::Mute] {
        v.push(("Mixer".to_string(), l.label(app, s, t), l.id()));
    }
    for (i, snd) in t.mixer.sends.iter().enumerate() {
        if snd.is_none() {
            continue;
        }
        let i = u8::try_from(i).unwrap_or(0);
        for l in [Lane::SendLevel(i), Lane::SendPan(i), Lane::SendMute(i)] {
            v.push(("Sends".to_string(), l.label(app, s, t), l.id()));
        }
    }
    v
}

/// The devices of `t` (the instrument, then the inserts) as (slot, device).
fn devices_of(t: &Track) -> impl Iterator<Item = (u8, &soundcraft_model::Insert)> {
    t.instrument
        .iter()
        .map(|i| (AutoParam::INSTRUMENT_SLOT, i))
        .chain(t.mixer.inserts.iter().enumerate().filter_map(|(slot, ins)| Some((u8::try_from(slot).unwrap_or(0), ins.as_ref()?))))
}

/// The lanes `t` can show, as (section, label, lane id): the mixer's, each send's, each device's.
pub fn choices(app: &mut StudioApp, s: &Session, t: &Track) -> Vec<(String, String, String)> {
    let mut v = base_choices(app, s, t);
    for (slot, ins) in devices_of(t) {
        let dev = app.plugin_name(&ins.plugin, false);
        let ids: Vec<(String, String)> = match device_info(&ins.plugin) {
            Some(info) => info.params.iter().map(|p| (p.id.to_string(), p.name.to_string())).collect(),
            // A hosted plug-in whose table is not cached yet (never loaded this run): the
            // parameters the document holds, by id.
            None => ins.params.keys().map(|k| (k.clone(), format!("param {k}"))).collect(),
        };
        for (id, name) in ids {
            v.push((dev.clone(), name, Lane::Plugin { slot, param: id }.id()));
        }
    }
    v
}

/// One device's parameter model for the picker: from the cached table (built once per table), or,
/// for a plug-in whose table is not cached yet, the parameters the document holds.
fn device_model(app: &mut StudioApp, ins: &soundcraft_model::Insert) -> std::sync::Arc<crate::automation_picker::DeviceModel> {
    match device_info(&ins.plugin) {
        Some(info) => app.auto.picker.model(&ins.plugin, info),
        None => {
            let names: Vec<String> = ins.params.keys().map(|k| format!("param {k}")).collect();
            std::sync::Arc::new(crate::automation_picker::DeviceModel::build(
                ins.params.keys().zip(&names).map(|(k, n)| (k.as_str(), n.as_str(), "")),
            ))
        }
    }
}

/// The picker's popup body: the search box, the mixer and send lanes, then each device's
/// parameters grouped (or matched, with a search). Returns the chosen lane id.
fn picker_body(app: &mut StudioApp, ui: &mut Ui, s: &Session, t: &Track, current: &str, menu: &mut Vec<MenuItem>) -> Option<String> {
    use crate::automation_picker::{DeviceView, device};
    let mut query = std::mem::take(&mut app.auto.picker.query);
    let focus = std::mem::take(&mut app.auto.picker.focus);
    let te = ui.add(egui::TextEdit::singleline(&mut query).hint_text("Search parameters").desired_width(f32::INFINITY));
    if focus {
        te.request_focus();
    }
    let enter = te.lost_focus() && ui.input(|i| crate::shortcuts::hit(i, crate::shortcuts::Action::PickerPickFirst));
    let searching = !query.trim().is_empty();
    let mut picked = None;
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
    let devs: Vec<_> =
        devices_of(t).map(|(slot, ins)| (slot, app.plugin_name(&ins.plugin, false), ins.plugin.clone(), device_model(app, ins))).collect();
    let base = base_choices(app, s, t);
    egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
        let mut section = String::new();
        for (sec, label, id) in base {
            if searching && !words.iter().all(|w| label.to_lowercase().contains(w)) {
                continue;
            }
            if sec != section {
                ui.label(egui::RichText::new(&sec).small().color(TOKENS.text_dim));
                section = sec;
            }
            let b = crate::menus::check_button(ui, id == current, label.as_str());
            menu.push(MenuItem::at(ui, label.clone(), id.clone(), b.rect));
            if b.clicked() {
                picked = Some(id);
            }
        }
        for (slot, name, plugin, model) in &devs {
            let recent = app.auto.picker.recent.get(plugin).map_or(&[][..], Vec::as_slice);
            let v = DeviceView { name, slot: *slot, model, recent };
            let found = searching.then(|| model.search(&query));
            if let Some(id) = device(ui, &v, found.as_deref(), current, menu) {
                picked = picked.take().or(Some(id));
            }
            if enter
                && picked.is_none()
                && let Some(&i) = found.as_ref().and_then(|f| f.first())
            {
                picked = Some(Lane::Plugin { slot: *slot, param: model.entries[i].id.clone() }.id());
            }
        }
    });
    app.auto.picker.query = query;
    if picked.is_some() {
        ui.close();
    }
    picked
}

/// A selected point.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PointRef {
    pub track: u64,
    pub lane: String,
    pub at: Samples,
}

/// A point drag. The pressed point is the anchor; every selected point of the lane moves by the
/// anchor's delta in time (samples) and in height (the lane's 0..1 scale).
#[derive(Debug, Clone)]
pub struct PointDrag {
    pub track: TrackId,
    pub lane: String,
    pub orig_at: Samples,
    pub orig_v: f32,
    /// The anchor's current time and value, and the delta applied so far.
    pub cur_at: Samples,
    pub cur_v: f32,
    pub cur_dk: f32,
    pub press: Pos2,
    pub key: String,
    /// The moving points as they were when the drag began, by time.
    pub group: Vec<(Samples, f32)>,
    /// The lane's other points: the moving ones never cross or replace them.
    pub fixed: Vec<Samples>,
}

impl PointDrag {
    /// The time delta nearest `dt` that keeps every moving point between its fixed neighbors
    /// and at or after zero.
    fn clamp_dt(&self, dt: Samples) -> Samples {
        let (mut lo, mut hi) = (Samples::MIN, Samples::MAX);
        for (at, _) in &self.group {
            lo = lo.max(self.fixed.iter().copied().filter(|f| f < at).max().map_or(0, |f| f + 1) - at);
            hi = hi.min(self.fixed.iter().copied().filter(|f| f > at).min().map_or(Samples::MAX, |f| f - 1).saturating_sub(*at));
        }
        dt.clamp(lo, hi.max(lo))
    }

    /// The height delta nearest `dk` that keeps every moving point inside 0..1.
    fn clamp_dk(&self, dk: f32, norms: &[f32]) -> f32 {
        let lo = norms.iter().map(|k| -k).fold(f32::MIN, f32::max);
        let hi = norms.iter().map(|k| 1.0 - k).fold(f32::MAX, f32::min);
        dk.clamp(lo, hi.max(lo))
    }
}

/// A gesture on a lane.
#[derive(Debug, Clone)]
pub enum AutoGesture {
    Point(Box<PointDrag>),
    Band { track: TrackId, lane: String, press: Pos2 },
    Height { track: TrackId, lane: String, start_h: f32, press_y: f32 },
}

/// One point as last drawn.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PointRow {
    pub at: Samples,
    pub value: f32,
    pub x: f32,
    pub y: f32,
    pub selected: bool,
}

/// One lane as last drawn (for `ui.inspect`; scripts drive the lanes with these rects).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LaneRow {
    pub track: u64,
    pub lane: String,
    pub label: String,
    /// The lane's header in the track list, and its area on the timeline.
    pub header: R4,
    pub area: R4,
    /// The parameter picker, "+" (another lane), "x" (close the lane).
    pub picker: R4,
    pub add: R4,
    pub close: R4,
    /// The mode dropdown (the track's first lane only; zero otherwise) and what it shows.
    pub mode: R4,
    pub mode_text: String,
    /// The bottom edge of the header: drag to change the lane's height.
    pub height_handle: R4,
    pub height: f32,
    /// The value at the playhead (or cursor) in the parameter's unit, and as a number.
    pub readout: String,
    pub value: f32,
    /// The marker on the curve at the playhead (or cursor).
    pub marker: [f32; 2],
    pub highlighted: bool,
    pub points: Vec<PointRow>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AutoLayout {
    pub lanes: Vec<LaneRow>,
    /// The open picker's or mode menu's entries.
    pub menu: Vec<MenuItem>,
    /// The rubber band being dragged.
    pub band: Option<R4>,
    pub selected: Vec<PointRef>,
    /// A write pass is recording.
    pub writing: bool,
    pub points_written: usize,
    /// Points the last thinning removed, and whether a pass ending thins (Automation menu).
    pub thinned: usize,
    pub thin_after_write: bool,
}

/// How long after the last value a hosted editor sent its parameter counts as still held (seconds).
/// The hosts forward values only, not the plug-in's begin/end-edit gestures, so a run of changes
/// less than this far apart is one gesture.
pub const HOSTED_HOLD_S: f64 = 0.15;

/// How long a moved plug-in parameter counts as held.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Hold {
    /// While the pointer is down (a built-in panel or window).
    Pointer,
    /// Until this time (a hosted editor, which egui cannot see the pointer in).
    Until(f64),
}

/// A plug-in parameter being moved: the plug-in counterpart of a mixer fader's `MixGesture`.
/// `fresh` holds until a write pass has seen it, so a click (toggle, choice, double-click reset,
/// preset recall: the pointer is already up) still writes its one point.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamTouch {
    pub track: TrackId,
    pub lane: Lane,
    hold: Hold,
    fresh: bool,
}

#[derive(Debug, Default)]
pub struct AutoState {
    pub gesture: Option<AutoGesture>,
    /// The plug-in parameters being moved (see [`StudioApp::touch_param`]).
    pub(crate) touches: Vec<ParamTouch>,
    /// The picker's grouped models, search text and recently touched parameters (#65).
    pub(crate) picker: crate::automation_picker::PickerState,
    pub selection: Vec<PointRef>,
    /// The lane of the mixer control last clicked (the mixer link).
    pub highlight: Option<(TrackId, String)>,
    pub layout: AutoLayout,
    serial: u64,
    /// The write pass: where each lane last wrote, the playhead at the last write, the latched
    /// lanes of Latch tracks, and how many points it wrote.
    last: HashMap<(TrackId, String), Samples>,
    last_at: Option<Samples>,
    /// Per lane: the value last written, and where a still value skipped its latest step.
    last_v: HashMap<(TrackId, String), f32>,
    skipped: HashMap<(TrackId, String), Samples>,
    latched: Vec<(TrackId, String)>,
    writing: bool,
    pub points_written: usize,
    /// The tracks the running pass wrote to (thinned when the pass or their Write ends).
    passed: Vec<TrackId>,
    /// Per written lane, the time spans the pass wrote (a locate starts a new span), and the undo
    /// counter after the pass's latest write: if it still holds at the end, the pass is the newest step.
    spans: HashMap<(TrackId, String), Vec<(Samples, Samples)>>,
    pass_mark: u64,
    /// What the last thinning removed, for `ui.inspect`.
    pub thinned: usize,
}

impl StudioApp {
    /// The track's open lanes, in order (none while its automation is hidden).
    pub fn lane_views(&self, t: &Track) -> Vec<LaneView> {
        if !self.ui.auto_shown.contains(&t.id) {
            return Vec::new();
        }
        self.ui.auto_lanes.iter().filter(|v| v.track == t.id && Lane::parse(&v.lane).is_some_and(|l| l.valid(t))).cloned().collect()
    }

    /// The value a mixer control shows while its track reads automation: the lane's value at the
    /// playhead. `None` when the track does not read (Off, Write, Trim), the lane has no points,
    /// or the user holds the control. It reads the same source as the lane readout and issues
    /// no command.
    pub fn followed(&self, t: &Track, lane: &Lane) -> Option<f32> {
        use AutomationMode as M;
        if !matches!(t.mixer.automation_mode, M::Read | M::Touch | M::Latch | M::TouchLatch) {
            return None;
        }
        if self.mixer.gesture.is_some_and(|g| Lane::of_target(g.target) == (t.id, lane.clone())) {
            return None;
        }
        if self.auto.touches.iter().any(|x| x.track == t.id && x.lane == *lane) {
            return None;
        }
        if *lane == Lane::Volume && matches!(&self.arrange.gesture, Some(crate::arrange::Gesture::Volume { track, .. }) if *track == t.id) {
            return None;
        }
        lane.params(t).first().and_then(|p| t.lane(p)).filter(|l| !l.points.is_empty())?;
        Some(lane.value_at(t, self.position()))
    }

    /// The value a built-in plug-in control shows while its track reads automation (see
    /// [`StudioApp::followed`]).
    pub(crate) fn followed_param(&self, key: (TrackId, usize), param: &str) -> Option<f32> {
        let (t, lane) = Lane::of_param(key.0, key.1, param)?;
        self.followed(self.engine.session().track(t)?, &lane)
    }

    /// A plug-in parameter was moved by the user: a built-in panel's knob or slider (the pointer
    /// holds it) or a hosted editor (it holds it `HOSTED_HOLD_S` after its latest value). The write
    /// pass records it in Write, Touch and Latch like a fader move; Read only shows the lane's
    /// value again afterwards.
    pub(crate) fn touch_param(&mut self, track: TrackId, slot: usize, param: &str, hosted: bool) {
        let Some((track, lane)) = Lane::of_param(track, slot, param) else { return };
        if hosted && let Some(plugin) = self.engine.session().track(track).and_then(|t| lane.device(t)).map(|i| i.plugin.clone()) {
            self.auto.picker.touched(&plugin, param);
        }
        let hold = if hosted { Hold::Until(self.last_frame.unwrap_or_default() + HOSTED_HOLD_S) } else { Hold::Pointer };
        match self.auto.touches.iter_mut().find(|x| x.track == track && x.lane == lane) {
            Some(x) => x.hold = hold,
            None => self.auto.touches.push(ParamTouch { track, lane, hold, fresh: true }),
        }
    }

    /// The A toggle: show or hide a track's lanes (Volume the first time). View state only.
    pub fn toggle_automation(&mut self, t: TrackId) {
        if let Some(i) = self.ui.auto_shown.iter().position(|x| *x == t) {
            self.ui.auto_shown.remove(i);
            self.auto.selection.retain(|p| p.track != t.0);
            return;
        }
        self.ui.auto_shown.push(t);
        let has = self.engine.session().track(t).is_some_and(|tr| !self.lane_views(tr).is_empty());
        if !has {
            self.ui.auto_lanes.push(LaneView { track: t, lane: Lane::Volume.id(), height: LANE_H });
        }
    }

    /// "+" on a lane: another lane under it, for the first parameter not open yet.
    pub fn add_lane(&mut self, t: TrackId, after: &str) {
        let s = self.engine.session_arc();
        let Some(tr) = s.track(t) else { return };
        let open: Vec<String> = self.ui.auto_lanes.iter().filter(|v| v.track == t).map(|v| v.lane.clone()).collect();
        let Some((_, _, id)) = choices(self, &s, tr).into_iter().find(|c| !open.contains(&c.2)) else {
            self.status = "Every parameter of this track has a lane".into();
            return;
        };
        let at = self.ui.auto_lanes.iter().position(|v| v.track == t && v.lane == after).map_or(self.ui.auto_lanes.len(), |i| i + 1);
        self.ui.auto_lanes.insert(at, LaneView { track: t, lane: id, height: LANE_H });
    }

    /// "x" on a lane: close it; closing the last one hides the track's automation.
    pub fn close_lane(&mut self, t: TrackId, lane: &str) {
        self.ui.auto_lanes.retain(|v| !(v.track == t && v.lane == lane));
        self.auto.selection.retain(|p| !(p.track == t.0 && p.lane == lane));
        let s = self.engine.session_arc();
        if s.track(t).is_some_and(|tr| self.lane_views(tr).is_empty()) {
            self.ui.auto_shown.retain(|x| *x != t);
        }
    }

    /// The picker: show `to` in place of `from` (or bring an open `to` here).
    pub fn pick_lane(&mut self, t: TrackId, from: &str, to: &str) {
        if from == to {
            return;
        }
        self.ui.auto_lanes.retain(|v| !(v.track == t && v.lane == to));
        if let Some(v) = self.ui.auto_lanes.iter_mut().find(|v| v.track == t && v.lane == from) {
            v.lane = to.to_string();
        }
        self.auto.selection.retain(|p| !(p.track == t.0 && p.lane == from));
    }

    /// The track's automation mode (`mix.automation_mode`: a document change, one undo step).
    pub fn set_automation_mode(&mut self, t: TrackId, mode: AutomationMode) -> Result<Value, String> {
        let params = json!({"tracks": [t.0], "mode": mode.label()});
        let writes =
            |m: AutomationMode| matches!(m, AutomationMode::Write | AutomationMode::Touch | AutomationMode::Latch | AutomationMode::TouchLatch);
        if self.auto.writing && self.auto.passed.contains(&t) && !writes(mode) {
            // Leaving Write mid-pass: thin what it wrote, then change the mode, all in the pass's step.
            self.auto.passed.retain(|x| *x != t);
            thin_after_pass(self, &[t], false);
            return self.engine.execute_merged("mix.automation_mode", &params, PASS).map_err(|e| e.to_string());
        }
        self.run("mix.automation_mode", params)
    }

    /// Commands as one undo step of their own, or as part of the gesture `key`.
    fn auto_exec(&mut self, cmds: Vec<(&'static str, Value)>, key: Option<&str>) -> Result<(), String> {
        let own = key.is_none();
        let key = key.map_or_else(
            || {
                self.auto.serial += 1;
                format!("auto:{}", self.auto.serial)
            },
            str::to_string,
        );
        if own {
            self.engine.end_merge();
        }
        let mut res = Ok(());
        for (id, p) in cmds {
            if let Err(e) = self.engine.execute_merged(id, &p, &key) {
                self.status = e.to_string();
                res = Err(e.to_string());
                break;
            }
        }
        if own {
            self.engine.end_merge();
        }
        res
    }

    /// Add a point (double-click on a lane): one undo step.
    pub fn add_point(&mut self, t: TrackId, lane: &str, at: Samples, v: f32) -> Result<(), String> {
        let s = self.engine.session_arc();
        let (Some(tr), Some(l)) = (s.track(t), Lane::parse(lane)) else { return Err("no such lane".into()) };
        let r = self.auto_exec(l.set_cmds(tr, at.max(0), v, None), None);
        if r.is_ok() {
            self.auto.selection = vec![PointRef { track: t.0, lane: lane.to_string(), at: at.max(0) }];
        }
        r
    }

    /// Delete the selected points: one undo step. Nothing selected: nothing happens.
    pub fn delete_points(&mut self) -> Result<usize, String> {
        let sel = std::mem::take(&mut self.auto.selection);
        let s = self.engine.session_arc();
        let mut cmds = Vec::new();
        for p in &sel {
            if let (Some(tr), Some(l)) = (s.track(TrackId(p.track)), Lane::parse(&p.lane)) {
                cmds.extend(l.clear_cmds(tr, p.at));
            }
        }
        if cmds.is_empty() {
            return Ok(0);
        }
        self.auto_exec(cmds, None)?;
        self.status = format!("Deleted {} automation point{}", sel.len(), if sel.len() == 1 { "" } else { "s" });
        Ok(sel.len())
    }

    /// Clear the point selection (a click elsewhere, Escape).
    pub fn clear_points(&mut self) {
        self.auto.selection.clear();
    }

    /// Drop selected points the document no longer has (after an undo, say).
    fn prune_selection(&mut self) {
        let s = self.engine.session_arc();
        self.auto
            .selection
            .retain(|p| s.track(TrackId(p.track)).zip(Lane::parse(&p.lane)).is_some_and(|(t, l)| l.points(t).iter().any(|(at, _)| *at == p.at)));
    }
}

/// Keys of the lanes: A shows the focused track's lanes; with points selected, Delete removes
/// them and Escape deselects them (before the arrangement's Delete takes clips).
pub fn shortcuts(app: &mut StudioApp, ctx: &egui::Context) {
    use crate::shortcuts::{Action as A, pressed};
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() {
        return;
    }
    if pressed(ctx, A::ToggleAutomation)
        && let Some(t) = app.focus_track()
    {
        app.toggle_automation(t);
    }
    if app.auto.selection.is_empty() || app.mixer.hover.is_some() {
        return;
    }
    if pressed(ctx, A::DeletePoints) {
        let _ = app.delete_points();
    }
    if pressed(ctx, A::DeselectPoints) {
        app.clear_points();
    }
}

/// The lane of the control the pointer just pressed in the mixer or the device chain, or the
/// header volume being dragged: it is highlighted when open (the mixer link).
fn follow_touch(app: &mut StudioApp, ui: &Ui) {
    let pressed = ui.input(|i| i.pointer.primary_pressed() || i.pointer.primary_down());
    let target = app.mixer.gesture.map(|g| g.target).or_else(|| app.mixer.hover.filter(|_| pressed));
    if let Some(t) = target {
        let (track, lane) = Lane::of_target(t);
        app.auto.highlight = Some((track, lane.id()));
    } else if let Some(crate::arrange::Gesture::Volume { track, .. }) = &app.arrange.gesture {
        app.auto.highlight = Some((*track, Lane::Volume.id()));
    }
}

/// The grid step in force, in ticks: the arrangement's grid settings ([`crate::grid`]) at this
/// zoom. The same step the arrangement draws its lines at.
pub fn grid_ticks(s: &Session, spp: f64, g: &GridView) -> i64 {
    crate::grid::ticks(crate::grid::px_per_tick(s, spp), crate::grid::bar_ticks(s), g)
}

/// `at` snapped to the grid (unchanged when snapping is off).
pub fn snap(s: &Session, spp: f64, g: &GridView, at: Samples) -> Samples {
    if !g.snap_enabled {
        return at;
    }
    let step = grid_ticks(s, spp, g).max(1);
    let tick = s.tempo.samples_to_ticks(at.max(0), s.sample_rate);
    let snapped = ((tick as f64 / step as f64).round() as i64).saturating_mul(step);
    s.tempo.tick_to_samples(snapped.max(0), s.sample_rate)
}

/// A drag of `dx` samples from `start`, quantized to the grid as a delta (Logic's relative
/// snap): a start on the grid lands on it, an off-grid start keeps its offset. Same grid as
/// [`snap`]; unchanged when snapping is off.
pub fn snap_delta(s: &Session, spp: f64, g: &GridView, start: Samples, dx: Samples) -> Samples {
    if !g.snap_enabled {
        return dx;
    }
    let step = grid_ticks(s, spp, g).max(1);
    let t0 = s.tempo.samples_to_ticks(start.max(0), s.sample_rate);
    let t1 = s.tempo.samples_to_ticks(start.saturating_add(dx).max(0), s.sample_rate);
    let q = ((t1 - t0) as f64 / step as f64).round() as i64;
    let to = s.tempo.tick_to_samples(t0.saturating_add(q.saturating_mul(step)).max(0), s.sample_rate);
    to - s.tempo.tick_to_samples(t0, s.sample_rate)
}

/// Total height of a track's open lanes.
pub(crate) fn lanes_height(app: &StudioApp, t: &Track) -> f32 {
    app.lane_views(t).iter().map(|v| v.height.clamp(LANE_MIN, LANE_MAX)).sum()
}

/// A lane's value-to-y mapping.
#[derive(Clone, Copy)]
struct Ys {
    top: f32,
    h: f32,
}

impl Ys {
    fn y(&self, k: f32) -> f32 {
        self.top + PAD + (1.0 - k.clamp(0.0, 1.0)) * (self.h - 2.0 * PAD).max(1.0)
    }
    fn k(&self, y: f32) -> f32 {
        1.0 - (y - self.top - PAD) / (self.h - 2.0 * PAD).max(1.0)
    }
}

/// Draw every open lane (header in the track list, curve on the timeline) and take its input.
/// Called after the timeline's own input, so a lane takes the clicks inside it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lanes(app: &mut StudioApp, ui: &mut Ui, s: &Session, map: Map, tl: Rect, list: Rect, rows: &[&Track], geo: &RowGeo, vscroll: f32) {
    follow_touch(app, ui);
    app.prune_selection();
    let mut layout = AutoLayout { menu: std::mem::take(&mut app.auto.layout.menu), ..AutoLayout::default() };
    layout.menu.clear();
    let pos = app.position();
    for (i, t) in rows.iter().enumerate() {
        let views = app.lane_views(t);
        if views.is_empty() {
            continue;
        }
        let mut y = tl.min.y + geo.tops[i] - vscroll + geo.rh + crate::arrange::takes_shown(app, t) as f32 * crate::arrange::TAKE_H;
        for (k, v) in views.iter().enumerate() {
            let h = v.height.clamp(LANE_MIN, LANE_MAX);
            let Some(lane) = Lane::parse(&v.lane) else { continue };
            let area = Rect::from_min_max(pos2(tl.min.x, y), pos2(tl.max.x, y + h));
            let header = Rect::from_min_max(pos2(list.min.x + 12.0, y), pos2(list.max.x - 4.0, y + h));
            let mut row = LaneRow {
                track: t.id.0,
                lane: v.lane.clone(),
                label: lane.label(app, s, t),
                header: r4(header),
                area: r4(area),
                height: h,
                ..LaneRow::default()
            };
            row.highlighted = app.auto.highlight.as_ref().is_some_and(|(ht, hl)| *ht == t.id && *hl == v.lane);
            if area.max.y >= tl.min.y && area.min.y <= tl.max.y {
                lane_area(app, ui, s, t, &lane, map, tl, area, pos, &mut row);
                lane_header(app, ui, t, &lane, k == 0, list, header, &mut row, &mut layout.menu);
            }
            layout.lanes.push(row);
            y += h;
        }
    }
    if let Some(AutoGesture::Band { press, .. }) = &app.auto.gesture
        && let Some(now) = ui.input(|i| i.pointer.latest_pos())
    {
        let r = Rect::from_two_pos(*press, now);
        let p = ui.painter().with_clip_rect(tl);
        p.rect_filled(r, 0.0, TOKENS.accent.gamma_multiply(0.15));
        p.rect_stroke(r, 0.0, Stroke::new(1.0, TOKENS.accent), StrokeKind::Inside);
        layout.band = Some(r4(r));
    }
    layout.selected = app.auto.selection.clone();
    layout.writing = app.auto.writing;
    layout.points_written = app.auto.points_written;
    layout.thinned = app.auto.thinned;
    layout.thin_after_write = app.ui.thin_after_write;
    app.auto.layout = layout;
}

/// The lane on the timeline: the curve, its points, the playhead's value marker, and the input.
#[allow(clippy::too_many_arguments)]
fn lane_area(app: &mut StudioApp, ui: &mut Ui, s: &Session, t: &Track, lane: &Lane, map: Map, tl: Rect, area: Rect, pos: Samples, row: &mut LaneRow) {
    let p = ui.painter().with_clip_rect(area.intersect(tl));
    let ys = Ys { top: area.min.y, h: area.height() };
    let color = theme::rgb(t.color);
    p.rect_filled(area, 0.0, TOKENS.bg);
    p.line_segment([area.left_top(), area.right_top()], Stroke::new(1.0, TOKENS.line));
    let pts = lane.points(t);
    let lid = lane.id();
    // The curve across the visible part: the value at each edge, the points between, and steps
    // for mute lanes.
    let x0 = tl.min.x;
    let x1 = tl.max.x;
    let at_y = |at: Samples| ys.y(lane.norm(t, lane.value_at(t, at)));
    let mut line = vec![pos2(x0, at_y(map.at(x0)))];
    let mut prev_y = line[0].y;
    for (at, v) in &pts {
        let x = map.x(*at);
        if x <= x0 || x >= x1 {
            continue;
        }
        let y = ys.y(lane.norm(t, *v));
        if lane.is_step() {
            line.push(pos2(x, prev_y));
        }
        line.push(pos2(x, y));
        prev_y = y;
    }
    line.push(pos2(x1, if lane.is_step() { prev_y } else { at_y(map.at(x1)) }));
    let ink = if pts.is_empty() { TOKENS.text_dim } else { color };
    p.add(egui::Shape::line(line, Stroke::new(if pts.is_empty() { 1.0 } else { 1.5 }, ink)));
    if pts.is_empty() {
        p.text(
            pos2(area.min.x + 8.0, area.min.y + 10.0),
            Align2::LEFT_CENTER,
            "Double-click to add a point",
            FontId::proportional(10.5),
            TOKENS.text_dim,
        );
    }
    for (at, v) in &pts {
        let c = pos2(map.x(*at), ys.y(lane.norm(t, *v)));
        let selected = app.auto.selection.iter().any(|q| q.track == t.id.0 && q.lane == lid && q.at == *at);
        row.points.push(PointRow { at: *at, value: *v, x: c.x, y: c.y, selected });
        if c.x < x0 - GRAB || c.x > x1 + GRAB {
            continue;
        }
        if selected {
            p.circle_filled(c, 4.5, TOKENS.selection_outline);
            p.circle_filled(c, 3.0, color);
        } else {
            p.circle_filled(c, 3.5, color);
            p.circle_stroke(c, 3.5, Stroke::new(1.0, TOKENS.bg));
        }
    }
    // The value at the playhead (or the cursor) follows playback.
    let v = lane.value_at(t, pos);
    let m = pos2(map.x(pos), ys.y(lane.norm(t, v)));
    row.value = v;
    row.readout = lane.text(t, v);
    row.marker = [m.x, m.y];
    if m.x >= x0 && m.x <= x1 {
        p.circle_stroke(m, 5.0, Stroke::new(1.5, TOKENS.accent));
    }
    if row.highlighted {
        p.rect_stroke(area.shrink(0.5), 0.0, Stroke::new(1.5, TOKENS.accent), StrokeKind::Inside);
    }
    lane_input(app, ui, s, t, lane, map, tl, area, ys, &pts);
}

/// The point under `pos` (within [`GRAB`]).
fn point_at(lane: &Lane, t: &Track, map: Map, ys: Ys, pts: &[(Samples, f32)], pos: Pos2) -> Option<(Samples, f32)> {
    pts.iter()
        .map(|(at, v)| ((*at, *v), pos2(map.x(*at), ys.y(lane.norm(t, *v))).distance(pos)))
        .filter(|(_, d)| *d <= GRAB)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(p, _)| p)
}

#[allow(clippy::too_many_arguments)]
fn lane_input(app: &mut StudioApp, ui: &mut Ui, s: &Session, t: &Track, lane: &Lane, map: Map, tl: Rect, area: Rect, ys: Ys, pts: &[(Samples, f32)]) {
    let lid = lane.id();
    let resp = ui.interact(area.intersect(tl), ui.id().with(("auto_lane", t.id.0, &lid)), Sense::click_and_drag());
    let mods = ui.input(|i| i.modifiers);
    if let Some(h) = resp.hover_pos()
        && app.auto.gesture.is_none()
        && point_at(lane, t, map, ys, pts, h).is_some()
    {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    }
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        match point_at(lane, t, map, ys, pts, press) {
            Some((at, v)) => {
                let me = PointRef { track: t.id.0, lane: lid.clone(), at };
                if !app.auto.selection.contains(&me) {
                    app.auto.selection = vec![me];
                }
                // Every selected point of the lane moves with the one pressed.
                let moving = |a: &Samples| app.auto.selection.iter().any(|q| q.track == t.id.0 && q.lane == lid && q.at == *a);
                let group: Vec<(Samples, f32)> = pts.iter().filter(|p| moving(&p.0)).copied().collect();
                let fixed: Vec<Samples> = pts.iter().map(|p| p.0).filter(|a| !moving(a)).collect();
                app.engine.end_merge();
                app.auto.serial += 1;
                let key = format!("autodrag:{}", app.auto.serial);
                app.auto.gesture = Some(AutoGesture::Point(Box::new(PointDrag {
                    track: t.id,
                    lane: lid.clone(),
                    orig_at: at,
                    orig_v: v,
                    cur_at: at,
                    cur_v: v,
                    cur_dk: 0.0,
                    press,
                    key,
                    group,
                    fixed,
                })));
            }
            None if mods.alt => {
                // Alt-drag on a segment bends it in Ableton; the model has no curve shapes (L38).
                app.status = "Lanes are linear: curved segments need a curve shape in the model".into();
            }
            None => app.auto.gesture = Some(AutoGesture::Band { track: t.id, lane: lid.clone(), press }),
        }
    }
    if resp.dragged()
        && let Some(AutoGesture::Point(g)) = app.auto.gesture.clone()
        && g.track == t.id
        && g.lane == lid
        && let Some(now) = ui.input(|i| i.pointer.latest_pos())
    {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        let fine_drag = mods.shift;
        let ratio = if fine_drag { fine::FINE } else { 1.0 };
        let d = now - g.press;
        // The height delta, kept so that no moving point leaves the range.
        let norms: Vec<f32> = g.group.iter().map(|(_, v)| lane.norm(t, *v)).collect();
        let dk = g.clamp_dk(-d.y / (ys.h - 2.0 * PAD).max(1.0) * ratio, &norms);
        let v = lane.denorm(t, lane.norm(t, g.orig_v) + dk);
        // A vertical drag changes the value only: off-grid points (a write pass makes them all)
        // keep their time until the pointer moves sideways past a small threshold.
        let raw = g.orig_at + (f64::from(d.x * ratio) * map.spp).round() as Samples;
        let want = if d.x.abs() < DRAG_SLOP {
            g.orig_at
        } else if fine_drag {
            raw
        } else {
            snap(s, map.spp, &map.grid, raw)
        };
        // The moving points stay between their fixed neighbors (moving past one would replace it).
        let dt = g.clamp_dt(want - g.orig_at);
        let at = g.orig_at + dt;
        let cur_dt = g.cur_at - g.orig_at;
        if dt != cur_dt || (dk - g.cur_dk).abs() > f32::EPSILON {
            let mut cmds = Vec::new();
            if dt != cur_dt {
                for (a, _) in &g.group {
                    cmds.extend(lane.clear_cmds(t, a + cur_dt));
                }
            }
            for (a, v0) in &g.group {
                cmds.extend(lane.set_cmds(t, a + dt, lane.denorm(t, lane.norm(t, *v0) + dk), None));
            }
            if app.auto_exec(cmds, Some(&g.key)).is_ok() {
                app.auto.selection.retain(|q| !(q.track == t.id.0 && q.lane == lid));
                app.auto.selection.extend(g.group.iter().map(|(a, _)| PointRef { track: t.id.0, lane: lid.clone(), at: a + dt }));
                app.auto.gesture = Some(AutoGesture::Point(Box::new(PointDrag { cur_at: at, cur_v: v, cur_dk: dk, ..*g })));
            }
        }
        // The value as the drag sets it, beside the point.
        let c = pos2(map.x(at), ys.y(lane.norm(t, v)));
        let text = format!("{} · {}", lane.text(t, v), s.format_position(at, s.edit.main_counter));
        let p = ui.painter().with_clip_rect(tl);
        let g = p.layout_no_wrap(text, FontId::proportional(10.5), TOKENS.text);
        let tag = Rect::from_min_size(pos2(c.x + 8.0, (c.y - 18.0).max(area.min.y)), g.size() + vec2(8.0, 4.0));
        p.rect_filled(tag, crate::theme::RADIUS_SM, TOKENS.panel_raised);
        p.galley(tag.min + vec2(4.0, 2.0), g, TOKENS.text);
    }
    if resp.drag_stopped() {
        match app.auto.gesture.take() {
            Some(AutoGesture::Band { track, lane: gl, press }) if track == t.id && gl == lid => {
                if let Some(end) = ui.input(|i| i.pointer.latest_pos()) {
                    let r = Rect::from_two_pos(press, end);
                    if !mods.shift && !mods.command {
                        app.auto.selection.clear();
                    }
                    for (at, v) in pts {
                        let c = pos2(map.x(*at), ys.y(lane.norm(t, *v)));
                        let me = PointRef { track: t.id.0, lane: lid.clone(), at: *at };
                        if r.contains(c) && !app.auto.selection.contains(&me) {
                            app.auto.selection.push(me);
                        }
                    }
                }
            }
            Some(AutoGesture::Point(_)) => app.engine.end_merge(),
            other => app.auto.gesture = other,
        }
    }
    if resp.clicked()
        && let Some(at_pos) = resp.interact_pointer_pos()
    {
        match point_at(lane, t, map, ys, pts, at_pos) {
            Some((at, _)) => {
                let me = PointRef { track: t.id.0, lane: lid.clone(), at };
                if mods.shift || mods.command {
                    if let Some(i) = app.auto.selection.iter().position(|q| *q == me) {
                        app.auto.selection.remove(i);
                    } else {
                        app.auto.selection.push(me);
                    }
                } else {
                    app.auto.selection = vec![me];
                }
                app.ui.focus_track = Some(t.id);
            }
            None => {
                app.click_empty(Some(t.id), map.at(at_pos.x));
            }
        }
    }
    if resp.double_clicked()
        && let Some(at_pos) = resp.interact_pointer_pos()
        && point_at(lane, t, map, ys, pts, at_pos).is_none()
    {
        let at = snap(s, map.spp, &map.grid, map.at(at_pos.x));
        let v = lane.denorm(t, ys.k(at_pos.y));
        let _ = app.add_point(t.id, &lid, at, v);
    }
}

/// The lane's header in the track list: picker, "+", "x", the mode (first lane), the readout,
/// and the bottom edge that sets the height.
#[allow(clippy::too_many_arguments)]
fn lane_header(
    app: &mut StudioApp,
    ui: &mut Ui,
    t: &Track,
    lane: &Lane,
    first: bool,
    list: Rect,
    r: Rect,
    row: &mut LaneRow,
    menu: &mut Vec<MenuItem>,
) {
    let p = ui.painter().with_clip_rect(list);
    p.rect_filled(r, 0.0, TOKENS.panel);
    p.line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, TOKENS.line));
    if row.highlighted {
        p.rect_filled(Rect::from_min_size(r.min, vec2(3.0, r.height())), 0.0, TOKENS.accent);
    }
    let lid = lane.id();
    let salt = (t.id.0, lid.clone());
    // "x" and "+" on the right of the first line.
    let close = Rect::from_min_size(pos2(r.max.x - 20.0, r.min.y + 4.0), vec2(18.0, 16.0));
    let add = Rect::from_min_size(pos2(close.min.x - 20.0, close.min.y), close.size());
    let picker = Rect::from_min_max(pos2(r.min.x + 6.0, r.min.y + 4.0), pos2(add.min.x - 4.0, r.min.y + 20.0));
    row.close = r4(close);
    row.add = r4(add);
    row.picker = r4(picker);
    let resp = ui.interact(close.intersect(list), ui.id().with(("lane_close", &salt)), Sense::click());
    crate::icons::paint(&p, close.center(), crate::icons::Icon::Remove, 12.0, if resp.hovered() { TOKENS.text } else { TOKENS.text_dim });
    if resp.on_hover_text("Close this lane").clicked() {
        app.close_lane(t.id, &lid);
    }
    let resp = ui.interact(add.intersect(list), ui.id().with(("lane_add", &salt)), Sense::click());
    crate::icons::paint(&p, add.center(), crate::icons::Icon::Plus, 12.0, if resp.hovered() { TOKENS.text } else { TOKENS.text_dim });
    if resp.on_hover_text("Show another parameter").clicked() {
        app.add_lane(t.id, &lid);
    }
    // The picker: every parameter of the track, by section.
    let resp = ui.interact(picker.intersect(list), ui.id().with(("lane_pick", &salt)), Sense::click());
    p.rect_filled(picker, crate::theme::RADIUS_SM, if resp.hovered() { TOKENS.button_hover } else { TOKENS.bg });
    let tr = picker.shrink2(vec2(5.0, 0.0));
    p.with_clip_rect(Rect::from_min_max(tr.min, pos2(tr.max.x - 12.0, tr.max.y)).intersect(list)).text(
        tr.left_center(),
        Align2::LEFT_CENTER,
        &row.label,
        FontId::proportional(11.0),
        TOKENS.text,
    );
    crate::icons::paint(&p, pos2(picker.max.x - 8.0, picker.center().y), crate::icons::Icon::ChevronDown, 10.0, TOKENS.text_dim);
    let s = app.engine.session_arc();
    let mut picked = None;
    let resp = resp.on_hover_text("Pick the parameter this lane shows");
    if resp.clicked() {
        app.auto.picker.query.clear();
        app.auto.picker.focus = true;
    }
    if let Some(id) = egui::Popup::menu(&resp).width(260.0).show(|ui| picker_body(app, ui, &s, t, &lid, menu)).and_then(|r| r.inner) {
        picked = Some(id);
    }
    if let Some(to) = picked {
        app.pick_lane(t.id, &lid, &to);
    }
    // Second line: the mode (first lane only) and the readout.
    let line2 = Rect::from_min_max(pos2(r.min.x + 6.0, r.min.y + 22.0), pos2(r.max.x - 4.0, r.min.y + 37.0));
    if first {
        let mode = t.mixer.automation_mode;
        let mr = Rect::from_min_size(line2.min, vec2(66.0, line2.height()));
        row.mode = r4(mr);
        row.mode_text = mode_label(mode).into();
        let resp = ui.interact(mr.intersect(list), ui.id().with(("lane_mode", t.id.0)), Sense::click());
        let writing = matches!(mode, AutomationMode::Write | AutomationMode::Touch | AutomationMode::Latch | AutomationMode::TouchLatch);
        let fill = if writing {
            TOKENS.record
        } else if resp.hovered() {
            TOKENS.button_hover
        } else {
            TOKENS.button
        };
        p.rect_filled(mr, crate::theme::RADIUS_SM, fill);
        let ink = if writing { theme::text_on(TOKENS.record) } else { TOKENS.text };
        p.text(pos2(mr.min.x + 6.0, mr.center().y), Align2::LEFT_CENTER, mode_label(mode), FontId::proportional(10.5), ink);
        crate::icons::paint(&p, pos2(mr.max.x - 8.0, mr.center().y), crate::icons::Icon::ChevronDown, 10.0, ink);
        let resp = resp.on_hover_text("Automation mode: Read plays the lanes; Touch, Latch and Write record while playing");
        let mut picked = None;
        egui::Popup::menu(&resp).width(120.0).show(|ui| {
            for m in MODES {
                let b = crate::menus::check_button(ui, m == mode, mode_label(m));
                menu.push(MenuItem::at(ui, mode_label(m).into(), m.label().into(), b.rect));
                if b.clicked() {
                    picked = Some(m);
                    ui.close();
                }
            }
        });
        if let Some(m) = picked
            && m != mode
        {
            let _ = app.set_automation_mode(t.id, m);
        }
    }
    p.text(line2.right_center(), Align2::RIGHT_CENTER, &row.readout, crate::theme::mono(ui.ctx(), 10.5), TOKENS.text);
    // The bottom edge: drag to change the lane's height (view state).
    let handle = Rect::from_min_max(pos2(r.min.x, r.max.y - 5.0), r.max);
    row.height_handle = r4(handle);
    let resp = ui.interact(handle.intersect(list), ui.id().with(("lane_height", &salt)), Sense::drag());
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        p.line_segment([pos2(r.min.x, r.max.y - 1.0), pos2(r.max.x, r.max.y - 1.0)], Stroke::new(2.0, TOKENS.accent));
    }
    if resp.drag_started() {
        let y = ui.input(|i| i.pointer.press_origin()).map_or(r.max.y, |p| p.y);
        app.auto.gesture = Some(AutoGesture::Height { track: t.id, lane: lid.clone(), start_h: row.height, press_y: y });
    }
    if resp.dragged()
        && let Some(AutoGesture::Height { track, lane: gl, start_h, press_y }) = app.auto.gesture.clone()
        && track == t.id
        && gl == lid
        && let Some(now) = ui.input(|i| i.pointer.latest_pos())
        && let Some(v) = app.ui.auto_lanes.iter_mut().find(|v| v.track == t.id && v.lane == lid)
    {
        v.height = (start_h + now.y - press_y).clamp(LANE_MIN, LANE_MAX);
    }
    if resp.drag_stopped() && matches!(app.auto.gesture, Some(AutoGesture::Height { .. })) {
        app.auto.gesture = None;
    }
}

/// A mode as the menu shows it (Logic's words).
pub fn mode_label(m: AutomationMode) -> &'static str {
    match m {
        AutomationMode::Off => "Off",
        AutomationMode::Read => "Read",
        AutomationMode::Touch => "Touch",
        AutomationMode::Latch | AutomationMode::TouchLatch => "Latch",
        AutomationMode::Write => "Write",
        AutomationMode::Trim => "Trim",
    }
}

/// The write pass, once per frame. While playing: tracks in Write write their volume and the
/// control being moved; Touch writes the control while it is held; Latch keeps writing it after
/// release until Stop. Points go in every [`WRITE_STEP_S`] with `from`, which replaces what the
/// pass runs over. The whole pass, and whatever is dragged during it, is one undo step.
pub(crate) fn write_pass(app: &mut StudioApp, ctx: &egui::Context) {
    use AutomationMode as M;
    let playing = app.is_playing();
    let held = ctx.input(|i| i.pointer.any_down());
    let now = ctx.input(|i| i.time);
    app.auto.touches.retain(|x| x.fresh || (x.hold == Hold::Pointer && held) || matches!(x.hold, Hold::Until(u) if now <= u));
    let any = app.engine.session().tracks.iter().any(|t| matches!(t.mixer.automation_mode, M::Write | M::Touch | M::Latch | M::TouchLatch));
    let may = playing && any;
    app.engine.hold_merge(may.then_some(PASS));
    if !may {
        // Stopped: nothing is written, and a click leaves no pulse for the next Play.
        app.auto.touches.iter_mut().for_each(|x| x.fresh = false);
        if app.auto.writing {
            app.auto.writing = false;
            let tracks = std::mem::take(&mut app.auto.passed);
            thin_after_pass(app, &tracks, true);
            app.engine.end_merge();
        }
        app.auto.passed.clear();
        app.auto.spans.clear();
        app.auto.last.clear();
        app.auto.last_v.clear();
        app.auto.skipped.clear();
        app.auto.latched.clear();
        app.auto.last_at = None;
        return;
    }
    // A track whose Write ended while others go on: thin what it wrote.
    let done: Vec<TrackId> = app
        .auto
        .passed
        .iter()
        .copied()
        .filter(|id| {
            !app.engine.session().track(*id).is_some_and(|t| matches!(t.mixer.automation_mode, M::Write | M::Touch | M::Latch | M::TouchLatch))
        })
        .collect();
    if !done.is_empty() {
        app.auto.passed.retain(|id| !done.contains(id));
        thin_after_pass(app, &done, true);
    }
    let pos = app.engine.transport.position;
    let step = app.engine.session().sample_rate.samples(WRITE_STEP_S);
    if let Some(last) = app.auto.last_at {
        if pos < last {
            // A loop or a locate: the next points start a new run.
            app.auto.last.clear();
            app.auto.last_v.clear();
            app.auto.skipped.clear();
        } else if pos - last < step {
            return;
        } else if pos - last > step * WRITE_GAP_STEPS {
            // A forward locate: a new segment, so the skipped range is not cleared.
            app.auto.last.clear();
            app.auto.last_v.clear();
            app.auto.skipped.clear();
        }
    }
    app.auto.last_at = Some(pos);
    let header = match &app.arrange.gesture {
        Some(crate::arrange::Gesture::Volume { track, .. }) => Some((*track, Lane::Volume)),
        _ => None,
    };
    let touched: Option<(TrackId, Lane)> = app.mixer.gesture.map(|g| Lane::of_target(g.target)).or(header);
    let mut touched: Vec<(TrackId, Lane)> = touched.filter(|_| held).into_iter().collect();
    touched.extend(app.auto.touches.iter().map(|x| (x.track, x.lane.clone())));
    for x in &mut app.auto.touches {
        x.fresh = false;
    }
    let s = app.engine.session_arc();
    // Touch stops writing on release: the next touch is a new segment, so the automation between
    // the touches is not cleared and the written span ends here.
    let released: Vec<(TrackId, String)> = app
        .auto
        .last
        .keys()
        .filter(|(id, lane)| {
            s.track(*id).is_some_and(|t| t.mixer.automation_mode == M::Touch) && !touched.iter().any(|(i, l)| i == id && l.id() == *lane)
        })
        .cloned()
        .collect();
    for k in released {
        app.auto.last.remove(&k);
        app.auto.last_v.remove(&k);
        app.auto.skipped.remove(&k);
    }
    let mut writes: Vec<(TrackId, Lane)> = Vec::new();
    for t in &s.tracks {
        let mine: Vec<Lane> = touched.iter().filter(|(id, _)| *id == t.id).map(|(_, l)| l.clone()).collect();
        match t.mixer.automation_mode {
            M::Write => {
                writes.push((t.id, Lane::Volume));
                writes.extend(mine.into_iter().filter(|l| *l != Lane::Volume).map(|l| (t.id, l)));
            }
            M::Touch => writes.extend(mine.into_iter().map(|l| (t.id, l))),
            M::Latch | M::TouchLatch => {
                for l in mine {
                    if !app.auto.latched.contains(&(t.id, l.id())) {
                        app.auto.latched.push((t.id, l.id()));
                    }
                }
                writes.extend(app.auto.latched.iter().filter(|(id, _)| *id == t.id).filter_map(|(id, l)| Lane::parse(l).map(|l| (*id, l))));
            }
            _ => {}
        }
    }
    let mut wrote = false;
    for (id, lane) in writes {
        let Some(t) = s.track(id) else { continue };
        if !lane.valid(t) {
            continue;
        }
        let key = (id, lane.id());
        let mut from = app.auto.last.get(&key).copied();
        let started = from.is_none();
        let v = lane.static_value(t);
        let held_lane = touched.iter().any(|(i, l)| *i == id && *l == lane);
        let mut ok = true;
        if let Some(old) = app.auto.last_v.get(&key).copied() {
            if !held_lane && (old - v).abs() <= f32::EPSILON {
                // Still: no point, but remember where the hold ran to.
                app.auto.skipped.insert(key, pos);
                continue;
            }
            if let Some(hold) = app.auto.skipped.remove(&key) {
                // The value moved after a hold: close the hold at its old value first.
                for (cmd, p) in lane.set_cmds(t, hold, old, from) {
                    ok &= app.engine.execute_merged(cmd, &p, PASS).is_ok();
                }
                from = Some(hold);
            }
        }
        app.auto.skipped.remove(&key);
        for (cmd, p) in lane.set_cmds(t, pos, v, from) {
            ok &= app.engine.execute_merged(cmd, &p, PASS).is_ok();
        }
        if ok {
            app.auto.last_v.insert(key.clone(), v);
            app.auto.writing = true;
            let segs = app.auto.spans.entry(key.clone()).or_default();
            match segs.last_mut() {
                Some(seg) if !started => seg.1 = pos,
                _ => segs.push((pos, pos)),
            }
            wrote = true;
            open_plugin_lane(app, id, &lane);
            if !app.auto.passed.contains(&id) {
                app.auto.passed.push(id);
            }
            app.auto.points_written += 1;
            app.auto.last.insert(key, pos);
        }
    }
    if wrote {
        app.auto.pass_mark = app.engine.revision;
    }
}

/// A plug-in lane that has just taken its first point opens under the track's other lanes, when
/// the track's automation is showing.
fn open_plugin_lane(app: &mut StudioApp, t: TrackId, lane: &Lane) {
    let id = lane.id();
    if matches!(lane, Lane::Plugin { .. }) && app.ui.auto_shown.contains(&t) && !app.ui.auto_lanes.iter().any(|v| v.track == t && v.lane == id) {
        app.ui.auto_lanes.push(LaneView { track: t, lane: id, height: LANE_H });
    }
}

/// The tolerance of every thinning (the engine's default, passed explicitly so the dry run agrees).
const THIN_TOLERANCE: f32 = 0.1;

/// How many points `automation.thin` would remove from `lane` (within `range`), without touching
/// the document: a thin that removes nothing must not enter the undo history.
fn removable(lane: Option<&soundcraft_model::AutomationLane>, range: Option<(Samples, Samples)>) -> usize {
    let Some(l) = lane else { return 0 };
    let mut part = l.clone();
    if let Some((a, b)) = range {
        part.points.retain(|p| p.at >= a && p.at <= b);
    }
    part.thin(THIN_TOLERANCE)
}

/// `automation.thin` with the default tolerance on exactly what a pass wrote: per lane, per written
/// time span, so hand-drawn points outside it and the track's other lanes stay as they are. It runs
/// under the pass's merge key. Nothing happens when it would remove nothing, or when an Undo has
/// taken the pass off the stack (a Thin step would wipe Redo). When an edit or a mode change has
/// already closed the pass's undo step it would be a step of its own, so (with `squash`) it is
/// folded into the pass if nothing came after the pass, and otherwise left as a separate step
/// named "Thin".
fn thin_after_pass(app: &mut StudioApp, tracks: &[TrackId], squash: bool) {
    let mut plan: Vec<(TrackId, String, Samples, Samples)> = Vec::new();
    let mut wrote = false;
    {
        let s = app.engine.session_arc();
        for id in tracks {
            let keys: Vec<(TrackId, String)> = app.auto.spans.keys().filter(|k| k.0 == *id).cloned().collect();
            for k in keys {
                let Some(segs) = app.auto.spans.remove(&k) else { continue };
                wrote = true;
                let (Some(t), Some(lane)) = (s.track(k.0), Lane::parse(&k.1)) else { continue };
                for (start, end) in segs.into_iter().filter(|(a, b)| b > a) {
                    for param in lane.param_ids(t) {
                        if removable(AutoParam::parse(&param).and_then(|p| t.lane(&p)), Some((start, end))) > 0 {
                            plan.push((k.0, param, start, end));
                        }
                    }
                }
            }
        }
    }
    // The session handle is dropped: holding it would make every command copy the document.
    if !wrote || app.engine.can_redo() {
        return;
    }
    let contiguous = app.engine.revision == app.auto.pass_mark;
    let top0 = app.engine.undo_label().map(str::to_string);
    let mut removed = 0;
    if app.ui.thin_after_write {
        for (id, param, start, end) in plan {
            let p = json!({"tracks": [id.0], "param": param, "start": start, "end": end, "tolerance": THIN_TOLERANCE});
            if let Ok(r) = app.engine.execute_merged("automation.thin", &p, PASS) {
                removed += r["removed"].as_u64().unwrap_or(0) as usize;
            }
        }
        app.auto.thinned = removed;
    }
    // The thins share the pass's merge key, so they add at most one step: a "Thin" on top of a
    // stack that did not end in one.
    let added = usize::from(app.engine.undo_label() == Some("Thin") && top0.as_deref() != Some("Thin"));
    let plural = if removed == 1 { "" } else { "s" };
    if squash && contiguous {
        // The pass is one step labeled for what it is, thinned or not.
        let n = (added + 1).min(app.engine.undo_history().len());
        if n > 0 {
            let _ = app.engine.squash_undo(n, "Write Automation");
        }
        if removed > 0 {
            app.status = format!("Thinned the write pass: {removed} point{plural} removed");
        }
    } else if added > 0 {
        app.status =
            format!("Thinned the write pass as its own step (Thin), since the pass is no longer the newest step: {removed} point{plural} removed");
    } else if removed > 0 {
        app.status = format!("Thinned the write pass: {removed} point{plural} removed");
    }
}

impl StudioApp {
    /// The time spans the running pass has written to `t`, over all its lanes (tests).
    #[cfg(test)]
    pub(crate) fn pass_spans(&self, t: TrackId) -> Vec<(Samples, Samples)> {
        let mut v: Vec<_> = self.auto.spans.iter().filter(|(k, _)| k.0 == t).flat_map(|(_, s)| s.iter().copied()).collect();
        v.sort_unstable();
        v
    }

    /// A write pass is recording on `t` (Thin Focused Track waits for it).
    pub fn pass_on(&self, t: TrackId) -> bool {
        self.auto.writing && self.auto.passed.contains(&t)
    }

    /// Automation > Thin Focused Track: thin the focused track's lanes now, one undo step.
    pub fn thin_focused(&mut self) -> Result<usize, String> {
        let Some(t) = self.focus_track() else { return Err("No focused track".into()) };
        if self.pass_on(t) {
            return Err("A write pass is running on this track; stop it first".into());
        }
        let s = self.engine.session_arc();
        let any = s.track(t).is_some_and(|tr| tr.automation.iter().any(|l| removable(Some(l), None) > 0));
        drop(s);
        if !any {
            self.status = "Nothing to thin".into();
            return Ok(0);
        }
        let r = self.run("automation.thin", json!({"tracks": [t.0], "tolerance": THIN_TOLERANCE}))?;
        let n = r["removed"].as_u64().unwrap_or(0) as usize;
        self.status = format!("Thinned the automation: {n} point{} removed", if n == 1 { "" } else { "s" });
        Ok(n)
    }
}

/// A control the user is moving ran a command under the pass's merge key: it is part of the pass,
/// so the pass is still the newest step.
pub(crate) fn note_pass_edit(app: &mut StudioApp) {
    if app.auto.writing {
        app.auto.pass_mark = app.engine.revision;
    }
}

/// Whether a write pass holds the undo merge (the frame must not end it).
pub(crate) fn passing(app: &StudioApp) -> bool {
    app.auto.writing
}
