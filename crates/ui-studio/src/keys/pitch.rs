//! Explicit selected-note pitch edits. Incoming MIDI and step entry are unaffected.

use super::*;
use soundcraft_midi::scale::Scale;

impl StudioApp {
    pub(super) fn keys_pitch_edit(&mut self, command: &str, mut params: Value) -> Result<(), String> {
        let Some(clip) = self.keys.clip else { return Err("no clip".into()) };
        let indices = resolve(&self.keys_notes(), &self.keys.selection);
        if indices.is_empty() {
            return Ok(());
        }
        params["clip"] = json!(clip.0);
        params["indices"] = json!(indices);
        let key = self.keys_merge_key();
        let result = self.keys_exec(command, params, &key)?;
        self.keys.selection = serde_json::from_value(result["notes"].clone()).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Snap only the selected notes to the chosen scale, in one undo step.
    pub fn keys_constrain(&mut self) -> Result<(), String> {
        let key = self.ui.keys.key_scale;
        self.keys_pitch_edit("midi.notes_constrain", json!({"root": key.root % 12, "scale": key.scale}))
    }
}

pub(super) fn menu(app: &mut StudioApp, ui: &mut Ui, layout: &mut KeysLayout) {
    let selected = !resolve(&app.keys_notes(), &app.keys.selection).is_empty();
    for (label, delta) in [("Up a semitone", 1), ("Down a semitone", -1), ("Up an octave", 12), ("Down an octave", -12)] {
        let response = ui.add_enabled(selected, egui::Button::new(label));
        layout.pitch_menu.push(MenuItem::at(ui, label.into(), label.into(), response.rect));
        if response.clicked() {
            let _ = app.keys_pitch_edit("midi.notes_transpose", json!({"semitones": delta}));
            ui.close();
        }
    }
    ui.separator();
    const ROOTS: [&str; 12] = ["C", "C# / Db", "D", "D# / Eb", "E", "F", "F# / Gb", "G", "G# / Ab", "A", "A# / Bb", "B"];
    let key = &mut app.ui.keys.key_scale;
    key.root %= 12;
    egui::ComboBox::from_id_salt("pitch_root").selected_text(ROOTS[usize::from(key.root)]).show_ui(ui, |ui| {
        for (root, label) in ROOTS.iter().enumerate() {
            ui.selectable_value(&mut key.root, root as u8, *label);
        }
    });
    ui.horizontal(|ui| {
        ui.selectable_value(&mut key.scale, Scale::Major, "Major");
        ui.selectable_value(&mut key.scale, Scale::Minor, "Natural minor");
    });
    let response = ui.add_enabled(selected, egui::Button::new("Constrain selected notes"));
    layout.pitch_menu.push(MenuItem::at(ui, "constrain".into(), "Constrain selected notes".into(), response.rect));
    if response.clicked() {
        let _ = app.keys_constrain();
        ui.close();
    }
    ui.small("Nearest scale tone; equal distances go down.");
}
