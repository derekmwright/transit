//! The editor window of a built-in plug-in (an instrument or an insert effect). Hosted plug-ins
//! open their own native windows from the device card's editor button; a built-in has none, so the
//! same button (and `ui.set {plugin_windows}`) opens this one instead: a bypass toggle, one row
//! per parameter from the plug-in's descriptor, and for the Sampler a sample picker.
//!
//! Values go through `mix.insert_param` (inserts) or `mix.instrument_param` (the instrument),
//! merged while the pointer is held so a drag is one undo step. Double-clicking a control resets
//! it to its default. The window is keyed by `Insert.uid`, so it follows its insert when the
//! chain is reordered (the `plugin_windows` entry follows too, see `StudioApp::reorder_device`).
//! Parameters of a plug-in that groups them per pad (the Drum Synth) are listed under a heading
//! for each pad.

use crate::StudioApp;
use crate::theme::TOKENS;
use egui::{RichText, Ui};
use serde_json::json;
use soundcraft_dsp::{ParamInfo, PluginInfo, Taper, Unit};
use soundcraft_model::{Insert, TrackId};

const SAMPLER: &str = "sampler";
const DRUM_SYNTH: &str = "drum_synth";
/// Sampler parameters that are MIDI notes, and the ones that are whole numbers besides.
const NOTE_PARAMS: [&str; 3] = ["root_note", "key_low", "key_high"];
const WHOLE_PARAMS: [&str; 2] = ["vel_low", "vel_high"];
/// What the sample picker offers and a dropped file may be.
const AUDIO_EXTS: &[&str] = &["wav", "wave", "bwf", "aif", "aiff", "aifc", "flac", "mp3", "ogg", "oga", "m4a", "aac", "mp4", "caf"];
/// The Drum Synth's pads, in the order their parameters are listed (a parameter's id starts with its pad).
const PADS: [(&str, &str); 6] =
    [("kick", "Kick"), ("snare", "Snare"), ("open_hat", "Open Hat"), ("hat", "Closed Hat"), ("tom", "Toms"), ("cymbal", "Cymbals")];

/// One parameter row as last drawn.
#[derive(Debug, Clone)]
pub struct RowLayout {
    pub param: String,
    /// The slider, combo box or check box.
    pub control: [f32; 4],
    /// The value as the row reads it.
    pub value: String,
}

/// One editor window as last drawn, for `ui.inspect`-style checks and tests.
#[derive(Debug, Clone)]
pub struct EditorLayout {
    pub key: (TrackId, usize),
    pub uid: u64,
    pub title: String,
    pub rect: [f32; 4],
    /// The bypass toggle (inserts only), the Sampler's "Load Sample…" button and its sample line.
    pub bypass: [f32; 4],
    /// The preset dropdown.
    pub preset: [f32; 4],
    pub picker: [f32; 4],
    pub sample: String,
    pub groups: Vec<String>,
    pub rows: Vec<RowLayout>,
}

#[derive(Debug, Default)]
pub struct EditorState {
    pub windows: Vec<EditorLayout>,
    /// A control changed since the pointer went down: its undo step ends on release.
    gesture: bool,
}

fn rect4(r: egui::Rect) -> [f32; 4] {
    [r.min.x, r.min.y, r.max.x, r.max.y]
}

/// Whether a built-in instrument's parameter only takes whole numbers.
fn whole_number(plugin: &str, param: &str) -> bool {
    plugin == SAMPLER && (NOTE_PARAMS.contains(&param) || WHOLE_PARAMS.contains(&param))
}

/// A parameter's value as its row shows it: the Sampler's notes by name.
fn format_param(plugin: &str, p: &ParamInfo, value: f32) -> String {
    if plugin == SAMPLER && NOTE_PARAMS.contains(&p.id) {
        let n = value.round().clamp(0.0, 127.0) as u8;
        return format!("{} ({n})", soundcraft_midi::note_name(n));
    }
    if whole_number(plugin, p.id) {
        return format!("{:.0}", value.round());
    }
    p.format(value)
}

/// The heading a parameter is listed under, if its plug-in groups parameters.
fn group_of(plugin: &str, param: &str) -> Option<&'static str> {
    if plugin != DRUM_SYNTH {
        return None;
    }
    Some(PADS.iter().find(|(prefix, _)| param.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('_'))).map_or("Output", |(_, name)| name))
}

fn is_audio_name(name: &str) -> bool {
    std::path::Path::new(name).extension().and_then(|x| x.to_str()).is_some_and(|x| AUDIO_EXTS.contains(&x.to_ascii_lowercase().as_str()))
}

impl StudioApp {
    /// The built-in plug-in an editor key names (`None` for a hosted one, or for nothing).
    pub(crate) fn builtin_at(&self, key: (TrackId, usize)) -> Option<Insert> {
        let t = self.engine.session().track(key.0)?;
        let ins = if key.1 == soundcraft_mix::INSTRUMENT_SLOT { t.instrument.as_ref() } else { t.mixer.inserts.get(key.1)?.as_ref() }?;
        (!soundcraft_mix::is_third_party(&ins.plugin) && soundcraft_dsp::plugin_info(&ins.plugin).is_some()).then(|| ins.clone())
    }

    /// Writes one built-in parameter: merged into the gesture in progress.
    pub(crate) fn set_builtin_param(&mut self, key: (TrackId, usize), param: &str, value: f32) {
        let (t, slot) = key;
        self.builtin_editor.gesture = true;
        let r = if slot == soundcraft_mix::INSTRUMENT_SLOT {
            self.engine.execute_merged(
                "mix.instrument_param",
                &json!({"track": t.0, "param": param, "value": value}),
                &format!("instrument_param:{}:{param}", t.0),
            )
        } else {
            self.engine.execute_merged(
                "mix.insert_param",
                &json!({"track": t.0, "slot": slot, "param": param, "value": value}),
                &format!("param:{}:{slot}:{param}", t.0),
            )
        };
        if let Err(e) = r {
            self.status = e.to_string();
        }
    }
}

/// Draws the editor window of every open built-in plug-in. Called every frame.
pub fn windows(app: &mut StudioApp, ctx: &egui::Context) {
    app.builtin_editor.windows.clear();
    let keys = app.ui.plugin_windows.clone();
    let mut closed = Vec::new();
    for (n, key) in keys.into_iter().enumerate() {
        let Some(ins) = app.builtin_at(key) else { continue };
        let Some(info) = soundcraft_dsp::plugin_info(&ins.plugin) else { continue };
        let instrument = key.1 == soundcraft_mix::INSTRUMENT_SLOT;
        let track_name = app.engine.session().track(key.0).map(|t| t.name.clone()).unwrap_or_default();
        let title = format!("{track_name} · {}", info.name);
        let mut layout = EditorLayout {
            key,
            uid: ins.uid.get(),
            title: title.clone(),
            rect: [0.0; 4],
            bypass: [0.0; 4],
            preset: [0.0; 4],
            picker: [0.0; 4],
            sample: String::new(),
            groups: Vec::new(),
            rows: Vec::new(),
        };
        let mut open = true;
        let shown = egui::Window::new(title)
            .id(egui::Id::new(("builtin_editor", ins.uid.get())))
            .open(&mut open)
            .default_width(if crate::panel::layout(&ins.plugin).is_some() { 0.0 } else { 360.0 })
            .default_pos(egui::pos2(ctx.content_rect().width() - 400.0 - 24.0 * n as f32, 90.0 + 24.0 * n as f32))
            .show(ctx, |ui| body(app, ui, key, &ins, info, instrument, &mut layout));
        if let Some(w) = shown {
            layout.rect = rect4(w.response.rect);
            if ins.plugin == SAMPLER {
                dropped_sample(app, ctx, key.0, w.response.rect);
            }
        }
        if !open {
            closed.push(key);
        }
        app.builtin_editor.windows.push(layout);
    }
    for key in closed {
        app.close_editor(key);
    }
    if app.builtin_editor.gesture && ctx.input(|i| i.pointer.any_released()) {
        app.builtin_editor.gesture = false;
        app.engine.end_merge();
    }
}

/// An audio file dropped on a Sampler editor loads as its sample.
fn dropped_sample(app: &mut StudioApp, ctx: &egui::Context, t: TrackId, r: egui::Rect) {
    let (files, pos) = ctx.input(|i| (i.raw.dropped_files.clone(), i.pointer.latest_pos()));
    if files.is_empty() || pos.is_some_and(|p| !r.contains(p)) {
        return;
    }
    if let Some(path) = files.iter().map(|f| f.path()).find(|p| p.is_absolute() && is_audio_name(&p.to_string_lossy())) {
        load_sample(app, t, &path.to_string_lossy());
    }
}

fn load_sample(app: &mut StudioApp, t: TrackId, path: &str) {
    if let Err(e) = app.run("mix.sampler_load", json!({"track": t.0, "path": path})) {
        app.status = e;
    }
}

fn body(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize), ins: &Insert, info: &'static PluginInfo, instrument: bool, lay: &mut EditorLayout) {
    let (t, slot) = key;
    ui.horizontal(|ui| {
        if !instrument {
            let r = ui.selectable_label(ins.bypass, if ins.bypass { "Bypassed" } else { "Bypass" });
            lay.bypass = rect4(r.rect);
            if r.on_hover_text("Bypass this effect").clicked() {
                let _ = app.toggle_bypass(t, slot);
            }
        }
        if crate::panel::layout(&ins.plugin).is_some() {
            crate::presets::control(app, ui, key, ins, info, &mut lay.preset);
        }
    });
    if let Some(rows) = crate::panel::layout(&ins.plugin) {
        crate::panel::show(app, ui, key, ins, info, rows, lay);
        return;
    }
    if ins.plugin == SAMPLER {
        sample_picker(app, ui, t, ins, lay);
    }
    egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
        ui.spacing_mut().slider_width = 170.0;
        egui::Grid::new(("builtin_params", ins.uid.get())).num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
            let mut group: Option<&str> = None;
            for p in info.params {
                if let Some(g) = group_of(&ins.plugin, p.id).filter(|g| group != Some(*g)) {
                    group = Some(g);
                    lay.groups.push(g.to_string());
                    ui.label(RichText::new(g).strong().color(TOKENS.text));
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                }
                param_row(app, ui, key, ins, p, lay);
                ui.end_row();
            }
        });
    });
}

/// A double-click on the control. (A slider senses drags only, so its response never reports one.)
pub(crate) fn double_clicked(ui: &Ui, r: &egui::Response) -> bool {
    ui.input(|i| {
        (i.pointer.button_double_clicked(egui::PointerButton::Primary) || i.pointer.button_triple_clicked(egui::PointerButton::Primary))
            && i.pointer.interact_pos().is_some_and(|p| r.rect.contains(p))
    })
}

fn param_row(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize), ins: &Insert, p: &ParamInfo, lay: &mut EditorLayout) {
    let v = p.clamp(ins.params.get(p.id).copied().unwrap_or(p.default));
    ui.label(RichText::new(p.name).color(TOKENS.text_dim));
    let mut x = v;
    let (resp, reset) = if p.unit == Unit::Choice && !p.choices.is_empty() {
        let mut idx = (x.round().max(0.0) as usize).min(p.choices.len() - 1);
        let r = egui::ComboBox::from_id_salt(("builtin_choice", ins.uid.get(), p.id)).selected_text(p.choices[idx]).show_ui(ui, |ui| {
            for (i, c) in p.choices.iter().enumerate() {
                ui.selectable_value(&mut idx, i, *c);
            }
        });
        x = idx as f32;
        let double = double_clicked(ui, &r.response);
        (r.response, double)
    } else if p.unit == Unit::Toggle {
        let mut on = x >= 0.5;
        let r = ui.checkbox(&mut on, "");
        x = f32::from(u8::from(on));
        let double = double_clicked(ui, &r);
        (r, double)
    } else {
        let mut sl = egui::Slider::new(&mut x, p.min..=p.max).show_value(false);
        if whole_number(&ins.plugin, p.id) {
            sl = sl.integer();
        }
        if p.taper == Taper::Log && p.min > 0.0 {
            sl = sl.logarithmic(true);
        }
        let r = ui.add(sl);
        let double = double_clicked(ui, &r);
        (r, double)
    };
    if reset {
        app.set_builtin_param(key, p.id, p.default);
        x = p.default;
    } else if (x - v).abs() > f32::EPSILON {
        let x = p.clamp(x);
        app.set_builtin_param(key, p.id, x);
    }
    let shown = format_param(&ins.plugin, p, x);
    ui.label(RichText::new(&shown).monospace().color(TOKENS.text_dim));
    lay.rows.push(RowLayout { param: p.id.to_string(), control: rect4(resp.rect), value: shown });
}

/// The Sampler's sample line, "Load Sample…" and Clear.
fn sample_picker(app: &mut StudioApp, ui: &mut Ui, t: TrackId, ins: &Insert, lay: &mut EditorLayout) {
    let name = ins.sample.and_then(|id| app.engine.session().source(id)).map(|s| s.name.clone());
    let secs = ins.sample.and_then(|id| app.engine.session().pool.get(id)).map(|a| a.frames() as f64 / f64::from(a.buffer.sample_rate.max(1)));
    lay.sample = match (name, secs) {
        (Some(n), Some(s)) => format!("{n} · {s:.2} s"),
        (Some(n), None) => format!("{n}: missing"),
        _ => "No sample".to_string(),
    };
    ui.horizontal(|ui| {
        let can_pick = app.services.pick_open.is_some();
        let load = ui.add_enabled(can_pick, egui::Button::new("Load Sample…"));
        lay.picker = rect4(load.rect);
        let load = if can_pick { load } else { load.on_disabled_hover_text("Drop an audio file on this window") };
        if load.clicked()
            && let Some(path) = app.services.pick_open.as_ref().and_then(|pick| pick("mix.sampler_load"))
        {
            load_sample(app, t, &path);
        }
        if ins.sample.is_some() && ui.button("Clear").clicked() {
            let _ = app.run("mix.sampler_clear", json!({"track": t.0}));
        }
    });
    ui.label(RichText::new(&lay.sample).monospace().color(TOKENS.text_dim));
    if ins.sample.is_none() {
        ui.label(RichText::new("The Sampler is silent until it has a sample: load one or drop an audio file here.").color(TOKENS.text_dim));
    }
    ui.add_space(4.0);
}
