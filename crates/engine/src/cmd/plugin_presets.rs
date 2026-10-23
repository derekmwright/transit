//! User presets of hosted plug-ins (issue #68): the plug-in's full state as the format's preset
//! file ([`crate::presets`]). The UI reads the live state (on the plug-in host thread) before a
//! save and loads a preset's state into the live instance; these commands keep the session in
//! step: a load is one undo step (the stored state, the preset name and the stored parameter
//! values), a save is not undoable.

use super::plugin_state::slot_param_for;
use super::*;
use serde_json::json;
use soundcraft_model::{Insert, b64};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "plugin.preset_save",
            label: "Save Plug-in Preset",
            menu: &[],
            shortcut: None,
            params: "{track, slot: 0..9 | \"a\"..\"j\" | \"instrument\", path, state: base64} → {path, name, bytes}. Writes the hosted plug-in's state (read live by the caller) as its preset file (.vstpreset for VST3, .clappreset for CLAP), stores it on the insert and names the preset. Not undoable and not journaled (states can be large).",
            enabled: has_tracks,
            run: preset_save,
            journal: false,
            undoable: false,
            view: false,
        },
        CommandSpec {
            id: "plugin.preset_load",
            label: "Load Preset",
            menu: &[],
            shortcut: None,
            params: "{track, slot, path, params?: {id: value}} → {name, state: base64}. Reads a preset file of the slot's plug-in into the session (stored state and preset name; `params`, the values the plug-in reported after loading it live, update the parameters the session stores). One undo step; the caller loads the state into the live instance.",
            enabled: has_tracks,
            run: preset_load,
            journal: true,
            undoable: true,
            view: false,
        },
        CommandSpec {
            id: "plugin.state_load",
            label: "Load Plug-in State",
            menu: &[],
            shortcut: None,
            params: "{track, slot, state: base64, name?, params?: {id: value}} → {bytes}. As plugin.preset_load from a state in hand (a pasted state, the plug-in's defaults). One undo step; not journaled (states can be large).",
            enabled: has_tracks,
            run: state_load,
            journal: false,
            undoable: true,
            view: false,
        },
        CommandSpec {
            id: "plugin.state_values",
            label: "Store Plug-in Values",
            menu: &[],
            shortcut: None,
            params: "{track, slot, params: {id: value}} → {stored}. The values a hosted plug-in reports after its stored state was loaded into it live again (an undo or redo of a preset load): the parameters the session stores take them, so the session never pushes other values over that state. Not undoable and not journaled.",
            enabled: has_tracks,
            run: state_values,
            journal: false,
            undoable: false,
            view: false,
        },
        CommandSpec {
            id: "plugin.preset_list",
            label: "List Plug-in Presets",
            menu: &[],
            shortcut: None,
            params: "{plugin, root} → {folder, presets: [name]}. The user presets of a hosted plug-in type under the settings folder `root`.",
            enabled: always,
            run: preset_list,
            journal: false,
            undoable: false,
            view: false,
        },
        CommandSpec {
            id: "plugin.preset_delete",
            label: "Delete Plug-in Preset",
            menu: &[],
            shortcut: None,
            params: "{path} → {deleted}. Deletes a preset file (.vstpreset or .clappreset only). Not undoable.",
            enabled: always,
            run: preset_delete,
            journal: true,
            undoable: false,
            view: false,
        },
    ]
}

/// The hosted plug-in insert (or instrument) at `track`/`slot`, with its track and slot.
fn hosted<'a>(e: &'a mut Engine, cmd: &str, p: &Value) -> Result<&'a mut Insert> {
    let t = track_param(e, cmd, p, "track")?.ok_or_else(|| bad(cmd, "`track` required"))?;
    let slot = slot_param_for(cmd, p)?;
    let s = e.session_mut();
    let tr = s.track_mut(t).ok_or_else(|| bad(cmd, "no track"))?;
    let ins = match slot {
        Some(i) => tr.mixer.inserts.get_mut(i).and_then(Option::as_mut),
        None => tr.instrument.as_mut(),
    }
    .ok_or_else(|| bad(cmd, "empty slot"))?;
    if crate::presets::format_of(&ins.plugin).is_none() {
        return Err(bad(cmd, format!("`{}` has no preset files here (VST3 and CLAP plug-ins do)", ins.plugin)));
    }
    Ok(ins)
}

/// The plug-in id at `track`/`slot`, without changing the document.
fn plugin_at(e: &Engine, cmd: &str, p: &Value) -> Result<String> {
    let t = track_param(e, cmd, p, "track")?.ok_or_else(|| bad(cmd, "`track` required"))?;
    let slot = slot_param_for(cmd, p)?;
    let tr = e.session().track(t).ok_or_else(|| bad(cmd, "no track"))?;
    let ins = match slot {
        Some(i) => tr.mixer.inserts.get(i).and_then(Option::as_ref),
        None => tr.instrument.as_ref(),
    }
    .ok_or_else(|| bad(cmd, "empty slot"))?;
    if crate::presets::format_of(&ins.plugin).is_none() {
        return Err(bad(cmd, format!("`{}` has no preset files here (VST3 and CLAP plug-ins do)", ins.plugin)));
    }
    Ok(ins.plugin.clone())
}

/// Marks the slot's state as loaded by this command: its undo and redo load the stored state into
/// the live instance.
fn mark(e: &mut Engine, cmd: &str, p: &Value) -> Result<()> {
    let t = track_param(e, cmd, p, "track")?.ok_or_else(|| bad(cmd, "`track` required"))?;
    let slot = slot_param_for(cmd, p)?.unwrap_or(soundcraft_mix::INSTRUMENT_SLOT);
    e.mark_state_load(t, slot);
    Ok(())
}

fn path_param(cmd: &str, p: &Value) -> Result<std::path::PathBuf> {
    let path = str_param(p, "path").filter(|s| !s.trim().is_empty()).ok_or_else(|| bad(cmd, "`path` required"))?;
    Ok(std::path::PathBuf::from(path))
}

fn state_param(cmd: &str, p: &Value) -> Result<Vec<u8>> {
    let s = str_param(p, "state").ok_or_else(|| bad(cmd, "`state` (base64) required"))?;
    b64::decode(s).ok_or_else(|| bad(cmd, "`state` is not valid base64 (or is too large)"))
}

/// The values to store: `params` entries for parameters the insert already stores (the session
/// only pushes those to the plug-in), finite ones only.
fn apply_params(cmd: &str, ins: &mut Insert, p: &Value) -> Result<usize> {
    let Some(v) = p.get("params") else { return Ok(0) };
    let map = v.as_object().ok_or_else(|| bad(cmd, "`params` must be an object of id → value"))?;
    let mut n = 0;
    for (id, value) in map {
        let Some(x) = value.as_f64().filter(|x| x.is_finite()) else { return Err(bad(cmd, format!("`params.{id}` must be a number"))) };
        if let Some(slot) = ins.params.get_mut(id) {
            *slot = x as f32;
            n += 1;
        }
    }
    Ok(n)
}

fn preset_save(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "plugin.preset_save";
    let path = path_param(CMD, p)?;
    let state = state_param(CMD, p)?;
    let plugin = plugin_at(e, CMD, p)?;
    crate::io::presets::write(&path, &plugin, &state).map_err(|m| bad(CMD, m))?;
    let name = crate::presets::name_of(&path);
    let ins = hosted(e, CMD, p)?;
    ins.set_state_bytes(&state);
    ins.preset.clone_from(&name);
    Ok(json!({"path": path.to_string_lossy(), "name": name, "bytes": state.len()}))
}

fn preset_load(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "plugin.preset_load";
    let path = path_param(CMD, p)?;
    let plugin = plugin_at(e, CMD, p)?;
    let state = crate::io::presets::read(&path, &plugin).map_err(|m| bad(CMD, m))?;
    let name = crate::presets::name_of(&path);
    let ins = hosted(e, CMD, p)?;
    apply_params(CMD, ins, p)?;
    ins.set_state_bytes(&state);
    ins.preset.clone_from(&name);
    mark(e, CMD, p)?;
    Ok(json!({"name": name, "state": b64::encode(&state)}))
}

fn state_load(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "plugin.state_load";
    let state = state_param(CMD, p)?;
    if state.is_empty() {
        return Err(bad(CMD, "`state` is empty"));
    }
    let name = str_param(p, "name").map(str::to_owned);
    let ins = hosted(e, CMD, p)?;
    apply_params(CMD, ins, p)?;
    ins.set_state_bytes(&state);
    if let Some(name) = name {
        ins.preset = name;
    }
    mark(e, CMD, p)?;
    Ok(json!({"bytes": state.len()}))
}

fn state_values(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "plugin.state_values";
    if p.get("params").is_none() {
        return Err(bad(CMD, "`params` required"));
    }
    let ins = hosted(e, CMD, p)?;
    let n = apply_params(CMD, ins, p)?;
    Ok(json!({"stored": n}))
}

fn preset_list(_e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "plugin.preset_list";
    let plugin = str_param(p, "plugin").ok_or_else(|| bad(CMD, "`plugin` required"))?;
    let root = path_param_key(CMD, p, "root")?;
    if crate::presets::format_of(plugin).is_none() {
        return Err(bad(CMD, format!("`{plugin}` has no preset files here (VST3 and CLAP plug-ins do)")));
    }
    Ok(json!({"folder": crate::presets::folder(&root, plugin).to_string_lossy(), "presets": crate::io::presets::list(&root, plugin)}))
}

fn path_param_key(cmd: &str, p: &Value, key: &str) -> Result<std::path::PathBuf> {
    let path = str_param(p, key).filter(|s| !s.trim().is_empty()).ok_or_else(|| bad(cmd, format!("`{key}` required")))?;
    Ok(std::path::PathBuf::from(path))
}

fn preset_delete(_e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "plugin.preset_delete";
    let path = path_param(CMD, p)?;
    crate::io::presets::delete(&path).map_err(|m| bad(CMD, m))?;
    Ok(json!({"deleted": path.to_string_lossy()}))
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;
    use soundcraft_model::b64;

    const VST3: &str = "vst3:0123456789ABCDEFFEDCBA9876543210";

    /// A track with a VST3 insert in slot A (the plug-in need not be installed: the session only
    /// names it).
    fn engine() -> (Engine, u64) {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id;
        let mut ins = soundcraft_model::Insert::new(VST3);
        ins.params.insert("7".into(), 1.0);
        e.session_mut().track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        (e, t.0)
    }

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("transit-plugin-presets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_saved_preset_loads_as_one_undo_step_and_the_undo_asks_for_a_live_restore() {
        let (mut e, t) = engine();
        let root = dir("undo");
        let mine = soundcraft_vst3_host::preset::join_state(b"my kit", b"ctrl").unwrap();
        let path = crate::presets::path(&root, VST3, "My Kit").unwrap();
        let undo_before = e.undo_history().len();
        let r =
            e.execute("plugin.preset_save", &json!({"track": t, "slot": "a", "path": path.to_string_lossy(), "state": b64::encode(&mine)})).unwrap();
        assert_eq!(r["name"], "My Kit");
        assert_eq!(e.undo_history().len(), undo_before, "a save is not undoable");
        assert!(e.journal.iter().all(|(id, _)| id != "plugin.preset_save"), "not journaled (states can be large)");
        assert_eq!(&std::fs::read(&path).unwrap()[..4], b"VST3", "a .vstpreset");
        let listed = e.execute("plugin.preset_list", &json!({"plugin": VST3, "root": root.to_string_lossy()})).unwrap();
        assert_eq!(listed["presets"], json!(["My Kit"]));
        // The plug-in moves on (a capture stores its live state), then the preset is loaded back.
        let later = soundcraft_vst3_host::preset::join_state(b"tweaked", b"ctrl").unwrap();
        e.execute("mix.insert_state", &json!({"track": t, "slot": 0, "state": b64::encode(&later)})).unwrap();
        e.session_mut().tracks[0].mixer.inserts[0].as_mut().unwrap().preset = "Other".into();
        let r = e
            .execute("plugin.preset_load", &json!({"track": t, "slot": 0, "path": path.to_string_lossy(), "params": {"7": 0.5, "99": 3.0}}))
            .unwrap();
        assert_eq!(r["name"], "My Kit");
        assert_eq!(b64::decode(r["state"].as_str().unwrap()).unwrap(), mine);
        let ins = e.session().tracks[0].mixer.inserts[0].clone().unwrap();
        assert_eq!((ins.state_bytes().unwrap(), ins.preset.as_str(), ins.params.get("7").copied()), (mine.clone(), "My Kit", Some(0.5)));
        assert!(!ins.params.contains_key("99"), "only the parameters the session stores");
        assert_eq!(e.undo_label(), Some("Load Preset"));
        assert!(e.take_state_restores().is_empty(), "the load itself needs no restore: the caller loaded it live");
        // Undo: the session holds the earlier state, and the live instance must get it back.
        e.execute("edit.undo", &json!({})).unwrap();
        let ins = e.session().tracks[0].mixer.inserts[0].clone().unwrap();
        assert_eq!((ins.state_bytes().unwrap(), ins.preset.as_str(), ins.params.get("7").copied()), (later, "Other", Some(1.0)));
        assert_eq!(e.take_state_restores(), vec![(soundcraft_model::TrackId(t), 0)]);
        assert!(e.take_state_restores().is_empty(), "once");
        e.execute("edit.redo", &json!({})).unwrap();
        assert_eq!(e.take_state_restores(), vec![(soundcraft_model::TrackId(t), 0)], "a redo restores the preset");
        // The values the plug-in reports after the redo's live load: stored, without a step.
        let steps = e.undo_history().len();
        e.execute("plugin.state_values", &json!({"track": t, "slot": 0, "params": {"7": 0.25}})).unwrap();
        assert_eq!(e.session().tracks[0].mixer.inserts[0].as_ref().unwrap().params.get("7").copied(), Some(0.25));
        assert_eq!(e.undo_history().len(), steps, "not undoable");

        // Delete.
        e.execute("plugin.preset_delete", &json!({"path": path.to_string_lossy()})).unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pasted_state_is_one_undo_step_and_hostile_params_are_errors() {
        let (mut e, t) = engine();
        // The caller stores the live state before it loads another (the undo returns to it).
        e.execute("mix.insert_state", &json!({"track": t, "slot": 0, "state": b64::encode(b"live before")})).unwrap();
        let pasted = soundcraft_vst3_host::preset::join_state(b"pasted", b"").unwrap();
        e.execute("plugin.state_load", &json!({"track": t, "slot": 0, "state": b64::encode(&pasted), "name": "Kit (copy)"})).unwrap();
        assert_eq!(e.undo_label(), Some("Load Plug-in State"));
        // An unrelated step after it: its undo asks for no live restore (the plug-in may have
        // moved on since a capture stored its state, which must not be undone behind its back).
        e.execute("mix.insert", &json!({"track": t, "plugin": "gain", "slot": 2})).unwrap();
        e.execute("mix.insert_state", &json!({"track": t, "slot": 0, "state": b64::encode(b"captured later")})).unwrap();
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(e.take_state_restores().is_empty());
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(e.take_state_restores(), vec![(soundcraft_model::TrackId(t), 0)], "the paste's own undo does");
        e.execute("edit.redo", &json!({})).unwrap();
        let _ = e.take_state_restores();
        assert_eq!(e.session().tracks[0].mixer.inserts[0].as_ref().unwrap().preset, "Kit (copy)");
        let root = dir("hostile");
        let elsewhere = root.join("x.vstpreset");
        for p in [
            json!({"track": t, "slot": 0, "path": elsewhere.to_string_lossy()}),
            json!({"track": t, "slot": 1, "path": elsewhere.to_string_lossy()}),
            json!({"track": t, "slot": 0}),
            json!({"track": t, "slot": 0, "path": ""}),
        ] {
            assert!(e.execute("plugin.preset_load", &p).is_err(), "{p}");
        }
        for p in [
            json!({"track": t, "slot": 0, "state": "!!!"}),
            json!({"track": t, "slot": 0, "state": ""}),
            json!({"track": t, "slot": 0, "state": b64::encode(b"x"), "params": {"7": "loud"}}),
            json!({"track": t, "slot": 0, "state": b64::encode(b"x"), "params": [1]}),
        ] {
            assert!(e.execute("plugin.state_load", &p).is_err(), "{p}");
        }
        // A file of another plug-in, or not a preset at all.
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&elsewhere, soundcraft_vst3_host::preset::encode(&[9; 16], b"c", b"")).unwrap();
        assert!(e.execute("plugin.preset_load", &json!({"track": t, "slot": 0, "path": elsewhere.to_string_lossy()})).is_err());
        let text = root.join("notes.txt");
        std::fs::write(&text, b"keep me").unwrap();
        assert!(e.execute("plugin.preset_delete", &json!({"path": text.to_string_lossy()})).is_err(), "only preset files");
        assert!(text.exists());
        // A built-in device has no preset files of this kind.
        e.execute("mix.insert", &json!({"track": t, "plugin": "gain", "slot": 1})).unwrap();
        assert!(e.execute("plugin.state_load", &json!({"track": t, "slot": 1, "state": b64::encode(b"x")})).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
