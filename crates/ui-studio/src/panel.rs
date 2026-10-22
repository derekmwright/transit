//! Bespoke panels for the built-in plug-ins that deserve more than a list of sliders: a small
//! layout description (rows of sections, each a titled group of knobs, toggles and choices) and
//! the code that draws it. A plug-in without a layout here keeps the generic rows.
//!
//! Every parameter of a bespoke plug-in appears in its layout (a test holds that), so nothing
//! becomes unreachable. Values go through `StudioApp::set_builtin_param`, like the sliders.

use crate::StudioApp;
use crate::builtin_editor::{EditorLayout, RowLayout};
use crate::theme::{RADIUS, TOKENS};
use crate::widgets::knob;
use egui::{RichText, Ui};
use soundcraft_dsp::{ParamInfo, PluginInfo};
use soundcraft_model::{Insert, KeyFrom, TrackId};

/// One control in a section: parameter id and the label under it.
#[derive(Debug, Clone, Copy)]
pub enum Item {
    Knob(&'static str, &'static str),
    Toggle(&'static str, &'static str),
    Choice(&'static str, &'static str),
    /// The sidechain key dropdown (compressor and gate).
    Key,
}

/// A titled group; `cols` items per line.
#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub title: &'static str,
    pub cols: usize,
    pub items: &'static [Item],
}

pub type Row = &'static [Section];

use Item::{Choice, Key, Knob, Toggle};

const fn sec(title: &'static str, cols: usize, items: &'static [Item]) -> Section {
    Section { title, cols, items }
}

static COMPRESSOR: [Row; 2] = [
    &[
        sec(
            "Dynamics",
            3,
            &[
                Knob("threshold", "Threshold"),
                Knob("ratio", "Ratio"),
                Knob("attack", "Attack"),
                Knob("release", "Release"),
                Knob("knee", "Knee"),
                Knob("makeup", "Makeup"),
            ],
        ),
        sec("Output", 3, &[Toggle("limiter", "Limit"), Knob("lookahead", "Lookahead"), Knob("mix", "Mix")]),
    ],
    &[sec("Key", 2, &[Key, Toggle("sc_hpf_on", "SC HPF"), Knob("sc_hpf_freq", "HPF Freq")])],
];

static GATE: [Row; 2] = [
    &[
        sec("Gate", 3, &[Knob("threshold", "Threshold"), Knob("ratio", "Ratio"), Knob("range", "Range")]),
        sec("Envelope", 3, &[Knob("attack", "Attack"), Knob("hold", "Hold"), Knob("release", "Release")]),
    ],
    &[sec("Key", 1, &[Key])],
];

static EQ7: [Row; 1] = [&[
    sec("In", 1, &[Knob("input_gain", "Input")]),
    sec("HPF", 1, &[Toggle("hpf_on", "On"), Knob("hpf_freq", "Freq"), Choice("hpf_slope", "Slope")]),
    sec("Low", 1, &[Knob("low_shelf_freq", "Freq"), Knob("low_shelf_gain", "Gain"), Knob("low_shelf_q", "Q")]),
    sec("Low Mid", 1, &[Knob("low_mid_freq", "Freq"), Knob("low_mid_gain", "Gain"), Knob("low_mid_q", "Q")]),
    sec("Mid", 1, &[Knob("mid_freq", "Freq"), Knob("mid_gain", "Gain"), Knob("mid_q", "Q")]),
    sec("High Mid", 1, &[Knob("high_mid_freq", "Freq"), Knob("high_mid_gain", "Gain"), Knob("high_mid_q", "Q")]),
    sec("High", 1, &[Knob("high_shelf_freq", "Freq"), Knob("high_shelf_gain", "Gain"), Knob("high_shelf_q", "Q")]),
    sec("LPF", 1, &[Toggle("lpf_on", "On"), Knob("lpf_freq", "Freq"), Choice("lpf_slope", "Slope")]),
    sec("Out", 1, &[Knob("output_gain", "Output")]),
]];

static SYNTH: [Row; 2] = [
    &[
        sec(
            "Oscillators",
            3,
            &[
                Choice("osc1_wave", "Osc 1"),
                Choice("osc2_wave", "Osc 2"),
                Knob("osc_mix", "Mix"),
                Knob("osc2_semitones", "Pitch"),
                Knob("osc2_detune", "Detune"),
            ],
        ),
        sec("Filter", 3, &[Knob("cutoff", "Cutoff"), Knob("resonance", "Reso"), Knob("filter_env_amount", "Env Amt")]),
        sec("Voice", 3, &[Knob("glide", "Glide"), Knob("velocity", "Velocity"), Knob("level", "Level")]),
    ],
    &[
        sec("Amp Env", 4, &[Knob("amp_attack", "A"), Knob("amp_decay", "D"), Knob("amp_sustain", "S"), Knob("amp_release", "R")]),
        sec("Filter Env", 4, &[Knob("filter_attack", "A"), Knob("filter_decay", "D"), Knob("filter_sustain", "S"), Knob("filter_release", "R")]),
    ],
];

static PLATE: [Row; 1] = [&[
    sec("Space", 2, &[Knob("decay", "Decay"), Knob("size", "Size"), Knob("pre_delay", "Pre-Delay"), Knob("diffusion", "Diffusion")]),
    sec("Tone", 3, &[Knob("damping", "Damping"), Knob("low_cut", "Low Cut"), Knob("width", "Width")]),
    sec("Mix", 1, &[Knob("mix", "Mix")]),
]];

/// The bespoke layout of a plug-in (`None`: it keeps the generic rows).
pub fn layout(plugin: &str) -> Option<&'static [Row]> {
    match plugin {
        "compressor" => Some(&COMPRESSOR),
        "expander_gate" => Some(&GATE),
        "eq_7band" => Some(&EQ7),
        "subtractive_synth" => Some(&SYNTH),
        "plate_reverb" => Some(&PLATE),
        _ => None,
    }
}

/// Draws a plug-in's panel into `ui`, one row of sections after another.
pub fn show(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize), ins: &Insert, info: &'static PluginInfo, rows: &[Row], lay: &mut EditorLayout) {
    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
    for row in rows {
        ui.horizontal_top(|ui| {
            for s in *row {
                section(app, ui, key, ins, info, s, lay);
            }
        });
    }
}

fn section(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize), ins: &Insert, info: &'static PluginInfo, s: &Section, lay: &mut EditorLayout) {
    egui::Frame::new().fill(TOKENS.panel_raised).corner_radius(RADIUS).inner_margin(8.0).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(s.title).size(10.5).strong().color(TOKENS.text_dim));
            for line in s.items.chunks(s.cols.max(1)) {
                ui.horizontal(|ui| {
                    for item in line {
                        draw(app, ui, key, ins, info, *item, lay);
                    }
                });
            }
        });
    });
}

fn draw(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize), ins: &Insert, info: &'static PluginInfo, item: Item, lay: &mut EditorLayout) {
    let (id, label) = match item {
        Knob(id, l) | Toggle(id, l) | Choice(id, l) => (id, l),
        Key => return key_dropdown(app, ui, key),
    };
    let Some(p) = info.param(id) else { return };
    let v = p.clamp(ins.params.get(id).copied().unwrap_or(p.default));
    let (rect, new, shown) = match item {
        Knob(..) => {
            let k = knob::show(ui, egui::Id::new(("knob", ins.uid.get(), id)), label, p, v);
            (k.rect, k.value, p.format(k.value.unwrap_or(v)))
        }
        Toggle(..) => {
            let r = ui.selectable_label(v >= 0.5, label);
            (r.rect, r.clicked().then_some(if v >= 0.5 { 0.0 } else { 1.0 }), p.format(v))
        }
        _ => choice(ui, ins, p, label, v),
    };
    if let Some(x) = new.filter(|x| (x - v).abs() > f32::EPSILON) {
        app.set_builtin_param(key, id, p.clamp(x));
    }
    lay.rows.push(RowLayout { param: id.to_string(), control: [rect.min.x, rect.min.y, rect.max.x, rect.max.y], value: shown });
}

fn choice(ui: &mut Ui, ins: &Insert, p: &ParamInfo, label: &str, v: f32) -> (egui::Rect, Option<f32>, String) {
    let mut idx = (v.round().max(0.0) as usize).min(p.choices.len().saturating_sub(1));
    let r = ui
        .vertical(|ui| {
            let c = egui::ComboBox::from_id_salt(("builtin_choice", ins.uid.get(), p.id))
                .width(knob::CELL_W - 10.0)
                .selected_text(p.choices.get(idx).copied().unwrap_or(""))
                .show_ui(ui, |ui| {
                    for (i, name) in p.choices.iter().enumerate() {
                        ui.selectable_value(&mut idx, i, *name);
                    }
                });
            ui.label(RichText::new(label).size(10.5).color(TOKENS.text_dim));
            c.response.rect
        })
        .inner;
    (r, (idx as f32 != v).then_some(idx as f32), p.format(idx as f32))
}

/// The sidechain key: None, or another track (or a bus), at the tap the device card last used.
fn key_dropdown(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize)) {
    let (t, slot) = key;
    let s = app.engine.session_arc();
    let Some(ins) = s.track(t).and_then(|x| x.mixer.inserts.get(slot)).and_then(Option::as_ref) else { return };
    let label = crate::devices::key_label(&s, ins);
    let (tracks, busses) = crate::devices::key_sources(&s, t);
    let current = ins.key;
    let pre = current.map_or(app.mixer.key_pre, |k| k.tap == soundcraft_model::Tap::Pre);
    let mut pick: Option<Option<KeyFrom>> = None;
    ui.vertical(|ui| {
        egui::ComboBox::from_id_salt(("panel_key", t.0, slot)).width(150.0).selected_text(label).show_ui(ui, |ui| {
            if ui.selectable_label(current.is_none(), "None").clicked() {
                pick = Some(None);
            }
            for (id, name) in &tracks {
                if ui.selectable_label(current.is_some_and(|k| k.from == KeyFrom::Track(*id)), name).clicked() {
                    pick = Some(Some(KeyFrom::Track(*id)));
                }
            }
            for (id, name) in &busses {
                if ui.selectable_label(current.is_some_and(|k| k.from == KeyFrom::Bus(*id)), name).clicked() {
                    pick = Some(Some(KeyFrom::Bus(*id)));
                }
            }
        });
        ui.label(RichText::new("Sidechain key").size(10.5).color(TOKENS.text_dim));
    });
    if let Some(from) = pick {
        let _ = app.set_insert_key(t, slot, from, pre);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_id(i: Item) -> Option<&'static str> {
        match i {
            Knob(id, _) | Toggle(id, _) | Choice(id, _) => Some(id),
            Key => None,
        }
    }

    #[test]
    fn every_layout_places_each_parameter_of_its_plugin_exactly_once() {
        for plugin in ["compressor", "expander_gate", "eq_7band", "subtractive_synth", "plate_reverb"] {
            let rows = layout(plugin).unwrap_or_else(|| panic!("a layout for {plugin}"));
            let mut ids: Vec<&str> = rows.iter().flat_map(|r| r.iter()).flat_map(|s| s.items.iter()).filter_map(|i| item_id(*i)).collect();
            ids.sort_unstable();
            let mut want: Vec<&str> = soundcraft_dsp::plugin_info(plugin).unwrap().params.iter().map(|p| p.id).collect();
            want.sort_unstable();
            assert_eq!(ids, want, "{plugin}");
        }
    }
}
