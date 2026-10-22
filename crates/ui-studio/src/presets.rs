//! Presets of the built-in plug-ins. A preset is a small JSON file:
//!
//! ```json
//! {"plugin": "compressor", "version": 1, "params": {"threshold": -18.0, "ratio": 3.0}}
//! ```
//!
//! `params` maps parameter ids to values. Recalling a preset sets every parameter of the plug-in:
//! the ones it lists to its values, the rest to their defaults, so a preset sounds the same
//! whatever came before. Ids the plug-in does not have are ignored. Factory presets live in
//! `assets/presets/<plugin-id>/<name>.json` and are compiled in; user presets are the same files
//! in `<config dir>/presets/<plugin-id>/`.

use crate::StudioApp;
use crate::theme::TOKENS;
use egui::Ui;
use serde::{Deserialize, Serialize};
use serde_json::json;
use soundcraft_dsp::PluginInfo;
use soundcraft_model::{Insert, TrackId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub plugin: String,
    pub version: u32,
    pub params: BTreeMap<String, f32>,
}

macro_rules! factory {
    ($($plugin:literal / $name:literal),* $(,)?) => {
        &[$(($plugin, $name, include_str!(concat!("../../../assets/presets/", $plugin, "/", $name, ".json")))),*]
    };
}

/// Every factory preset: plug-in id, name, file text.
pub static FACTORY: &[(&str, &str, &str)] = factory![
    "compressor" / "Vocal",
    "compressor" / "Drum Bus",
    "compressor" / "Gentle",
    "expander_gate" / "Tight Drums",
    "expander_gate" / "Light Gate",
    "eq_7band" / "Flat",
    "eq_7band" / "Low Cut",
    "subtractive_synth" / "Init",
    "subtractive_synth" / "Soft Pad",
    "subtractive_synth" / "Bass",
    "plate_reverb" / "Vocal Plate",
    "plate_reverb" / "Bright Plate",
];

/// The factory presets of one plug-in, in table order.
pub fn factory_for(plugin: &str) -> Vec<(&'static str, Preset)> {
    FACTORY.iter().filter(|(p, ..)| *p == plugin).filter_map(|(_, name, text)| parse(text).ok().map(|p| (*name, p))).collect()
}

pub fn parse(text: &str) -> Result<Preset, String> {
    let p: Preset = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if p.version > VERSION {
        return Err(format!("preset version {} is newer than this build reads ({VERSION})", p.version));
    }
    Ok(p)
}

/// The values a recall writes: every parameter of `info`, from the preset or else its default.
pub fn recall_values(info: &PluginInfo, preset: &Preset) -> BTreeMap<String, f32> {
    info.params.iter().map(|p| (p.id.to_string(), p.clamp(preset.params.get(p.id).copied().unwrap_or(p.default)))).collect()
}

/// A file-safe name: path separators and other trouble characters become spaces.
pub fn file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || " -_().".contains(c) { c } else { ' ' }).collect();
    s.trim().trim_matches('.').to_string()
}

fn dir_of(root: &Path, plugin: &str) -> PathBuf {
    root.join("presets").join(plugin)
}

/// The names of the user presets of `plugin` under the config dir `root`, sorted.
pub fn user_names(root: &Path, plugin: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir_of(root, plugin))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.path().file_stem().filter(|_| e.path().extension().is_some_and(|x| x == "json")).map(|s| s.to_string_lossy().into_owned()))
        .collect();
    v.sort_by_key(|s| s.to_lowercase());
    v
}

pub fn save_user(root: &Path, info: &PluginInfo, name: &str, params: &BTreeMap<String, f32>) -> Result<String, String> {
    let name = file_name(name);
    if name.is_empty() {
        return Err("Name the preset first".into());
    }
    let dir = dir_of(root, info.id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let preset = Preset { plugin: info.id.to_string(), version: VERSION, params: params.clone() };
    let text = serde_json::to_string_pretty(&preset).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{name}.json")), text).map_err(|e| e.to_string())?;
    Ok(name)
}

pub fn load_user(root: &Path, plugin: &str, name: &str) -> Result<Preset, String> {
    let text = std::fs::read_to_string(dir_of(root, plugin).join(format!("{}.json", file_name(name)))).map_err(|e| e.to_string())?;
    let p = parse(&text)?;
    if p.plugin != plugin {
        return Err(format!("that preset is for `{}`", p.plugin));
    }
    Ok(p)
}

pub fn delete_user(root: &Path, plugin: &str, name: &str) -> Result<(), String> {
    std::fs::remove_file(dir_of(root, plugin).join(format!("{}.json", file_name(name)))).map_err(|e| e.to_string())
}

impl StudioApp {
    /// Recalls `preset` on the effect or instrument at `key` as one undo step.
    pub(crate) fn apply_preset(&mut self, key: (TrackId, usize), info: &PluginInfo, name: &str, preset: &Preset) {
        let values = recall_values(info, preset);
        let (t, slot) = key;
        // A recall is one instantaneous gesture on each parameter it changes (automation capture).
        let current = self.builtin_at(key).map(|i| i.params).unwrap_or_default();
        for (id, v) in &values {
            let was = current.get(id).copied().or_else(|| info.param(id).map(|p| p.default));
            if was.is_none_or(|c| (c - v).abs() > f32::EPSILON) {
                self.touch_param(t, slot, id, false);
            }
        }
        let r = if slot == soundcraft_mix::INSTRUMENT_SLOT {
            // No batch command for the instrument: one merge key makes the writes one step.
            self.engine.end_merge();
            let mut r = Ok(());
            for (id, v) in &values {
                r = r.and(
                    self.engine
                        .execute_merged("mix.instrument_param", &json!({"track": t.0, "param": id, "value": v}), &format!("preset:{}", t.0))
                        .map(|_| ()),
                );
            }
            self.engine.end_merge();
            r
        } else {
            self.engine.execute("mix.insert_params", &json!({"track": t.0, "slot": slot, "params": values, "preset": name})).map(|_| ())
        };
        if let Err(e) = r {
            self.status = e.to_string();
        }
    }
}

/// The preset dropdown in an editor window: factory and user presets, Save As and Delete.
pub fn control(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize), ins: &Insert, info: &'static PluginInfo, out: &mut [f32; 4]) {
    let root = app.services.config_dir.clone();
    let user = root.as_deref().map(|r| user_names(r, info.id)).unwrap_or_default();
    let current = if ins.preset.is_empty() { "Preset" } else { ins.preset.as_str() };
    let name_id = ui.id().with(("preset_name", ins.uid.get()));
    let mut name: String = ui.data_mut(|d| d.get_temp(name_id)).unwrap_or_default();
    let mut pick: Option<(String, Result<Preset, String>)> = None;
    let mut delete = None;
    let mut save = None;
    let r = egui::ComboBox::from_id_salt(("builtin_preset", ins.uid.get())).selected_text(current).width(150.0).show_ui(ui, |ui| {
        let factory = factory_for(info.id);
        if !factory.is_empty() {
            ui.label(egui::RichText::new("Factory").size(10.5).color(TOKENS.text_dim));
        }
        for (n, p) in factory {
            if ui.selectable_label(ins.preset == n, n).clicked() {
                pick = Some((n.to_string(), Ok(p)));
            }
        }
        if !user.is_empty() {
            ui.separator();
            ui.label(egui::RichText::new("User").size(10.5).color(TOKENS.text_dim));
        }
        for n in &user {
            if ui.selectable_label(ins.preset == *n, n).clicked() {
                pick = Some((n.clone(), root.as_deref().map_or(Err("no config folder".into()), |r| load_user(r, info.id, n))));
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut name).hint_text("Save As…").desired_width(100.0));
            if ui.add_enabled(root.is_some() && !file_name(&name).is_empty(), egui::Button::new("Save")).clicked() {
                save = Some(name.clone());
            }
        });
        if user.contains(&ins.preset) && ui.button(format!("Delete \"{}\"", ins.preset)).clicked() {
            delete = Some(ins.preset.clone());
        }
    });
    *out = [r.response.rect.min.x, r.response.rect.min.y, r.response.rect.max.x, r.response.rect.max.y];
    ui.data_mut(|d| d.insert_temp(name_id, name));
    if let Some((n, p)) = pick {
        match p {
            Ok(p) => app.apply_preset(key, info, &n, &p),
            Err(e) => app.status = format!("Preset: {e}"),
        }
    }
    let Some(root) = root else { return };
    if let Some(n) = save {
        let values: BTreeMap<String, f32> =
            info.params.iter().map(|p| (p.id.to_string(), p.clamp(ins.params.get(p.id).copied().unwrap_or(p.default)))).collect();
        match save_user(&root, info, &n, &values) {
            Ok(n) => {
                // The name shows in the header; a note-only recall of the same values.
                if key.1 != soundcraft_mix::INSTRUMENT_SLOT {
                    let _ = app.engine.execute("mix.insert_params", &json!({"track": key.0.0, "slot": key.1, "params": {}, "preset": n}));
                }
                ui.data_mut(|d| d.insert_temp(name_id, String::new()));
            }
            Err(e) => app.status = format!("Preset: {e}"),
        }
    }
    if let Some(n) = delete
        && let Err(e) = delete_user(&root, info.id, &n)
    {
        app.status = format!("Preset: {e}");
    }
}
