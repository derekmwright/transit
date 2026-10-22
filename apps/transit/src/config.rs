//! Settings folders. Adapted from `apps/soundcraft/src/config.rs` at `transit` 2af868e (file last
//! changed 0434bb1): our own folder for `ui.json`, `Autosave/` and `logs/` (`TRANSIT_CONFIG_DIR`),
//! and SoundCraft's folder for the shared plug-in scan cache only (plan 06, section 3).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum Platform {
    Mac,
    Windows,
    Other,
}

fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Other
    }
}

/// A nonempty `$<var>` is the complete directory; an explicitly empty one disables it.
fn dir_with(platform: Platform, var: &str, app: (&str, &str), env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = env(var) {
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    let (title, lower) = app;
    match platform {
        Platform::Mac => env("HOME").map(|h| PathBuf::from(h).join("Library/Application Support").join(title)),
        Platform::Windows => env("APPDATA").map(|a| PathBuf::from(a).join(title)),
        Platform::Other => {
            env("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config"))).map(|c| c.join(lower))
        }
    }
}

/// transit's settings folder (`TRANSIT_CONFIG_DIR` overrides).
pub fn config_dir() -> Option<PathBuf> {
    dir_with(platform(), "TRANSIT_CONFIG_DIR", ("transit", "transit"), |n| std::env::var_os(n))
}

/// The folder used before the product was named transit. `None` when `TRANSIT_CONFIG_DIR` is set:
/// an explicit folder is never migrated into.
pub fn legacy_config_dir() -> Option<PathBuf> {
    legacy_dir_with(platform(), |n| std::env::var_os(n))
}

fn legacy_dir_with(platform: Platform, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if env("TRANSIT_CONFIG_DIR").is_some() {
        return None;
    }
    dir_with(platform, "TRANSIT_CONFIG_DIR", ("transitDAW", "transitdaw"), env)
}

/// What the one-time settings-folder migration did.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// Nothing to do: the new folder exists, or there is no old one.
    None,
    /// The old folder was renamed to the new one.
    Moved,
    /// The rename failed; the files were copied and the old folder was left in place.
    Copied,
    /// Neither the rename nor the copy worked.
    Failed(String),
}

/// One-time migration: when `new` does not exist and `old` does, rename `old` to `new`. If the
/// rename fails (another device, permissions), copy the files and leave `old`. `old` is never
/// deleted.
pub fn migrate_legacy_dir(new: &Path, old: &Path) -> Migration {
    if new.exists() || !old.is_dir() {
        return Migration::None;
    }
    if std::fs::rename(old, new).is_ok() {
        return Migration::Moved;
    }
    match copy_dir(old, new) {
        Ok(()) => Migration::Copied,
        Err(error) => Migration::Failed(error.to_string()),
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Run the migration for the real folders and log one line.
pub fn migrate_legacy_config() {
    let (Some(new), Some(old)) = (config_dir(), legacy_config_dir()) else { return };
    match migrate_legacy_dir(&new, &old) {
        Migration::None => {}
        Migration::Moved => log::info!("settings folder moved from {} to {}", old.display(), new.display()),
        Migration::Copied => log::info!("settings folder copied from {} to {} (the old folder was left in place)", old.display(), new.display()),
        Migration::Failed(error) => log::warn!("settings folder could not be moved or copied from {} to {}: {error}", old.display(), new.display()),
    }
}

/// SoundCraft's settings folder (`SOUNDCRAFT_CONFIG_DIR` overrides), for the shared plug-in scan
/// cache, so a large library is scanned once for both apps.
pub fn soundcraft_dir() -> Option<PathBuf> {
    dir_with(platform(), "SOUNDCRAFT_CONFIG_DIR", ("SoundCraft", "soundcraft"), |n| std::env::var_os(n))
}

const MAX_UI_BYTES: u64 = 4 * 1024 * 1024;

/// Read bounded settings; `Ok(None)` when there are none yet.
pub fn load_ui(path: &Path) -> Result<Option<transit_ui_studio::UiState>, String> {
    read_ui(path)?.map(decode_ui).transpose()
}

fn read_ui(path: &Path) -> Result<Option<serde_json::Value>, String> {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Settings could not be read: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_UI_BYTES + 1).read_to_end(&mut bytes).map_err(|error| format!("Settings could not be read: {error}"))?;
    if bytes.len() as u64 > MAX_UI_BYTES {
        return Err("Settings file exceeds the 4 MiB limit".into());
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|error| format!("Settings could not be restored: {error}"))
}

/// Recover independent preferences even if a newer version wrote an unsupported field value.
fn decode_ui(value: serde_json::Value) -> Result<transit_ui_studio::UiState, String> {
    let fields = value.as_object().ok_or("Settings must be a JSON object")?;
    let mut accepted = serde_json::Map::new();
    let mut ui = transit_ui_studio::UiState::default();
    let defaults = serde_json::to_value(&ui).map_err(|error| error.to_string())?;
    for (key, value) in fields {
        // Unknown fields stay in the raw file; do not repeatedly deserialize them.
        if defaults.get(key).is_none() {
            continue;
        }
        accepted.insert(key.clone(), value.clone());
        match serde_json::from_value(serde_json::Value::Object(accepted.clone())) {
            Ok(next) => ui = next,
            Err(error) => {
                accepted.remove(key);
                log::warn!("Settings field {key} could not be restored: {error}");
            }
        }
    }
    Ok(ui)
}

/// Keep unknown fields and unchanged raw values, including arrays from a newer version.
/// Only preferences actually changed by this version replace their saved representation.
fn merge_ui(raw: &mut serde_json::Value, previous: &serde_json::Value, current: &serde_json::Value) {
    if let (Some(raw), Some(previous), Some(current)) = (raw.as_object_mut(), previous.as_object(), current.as_object()) {
        for (key, value) in current {
            if previous.get(key) == Some(value) && raw.contains_key(key) {
                continue;
            }
            if let (Some(raw), Some(previous)) = (raw.get_mut(key), previous.get(key)) {
                if raw.is_object() && previous.is_object() && value.is_object() {
                    merge_ui(raw, previous, value);
                } else {
                    let identity: &[&str] = match key.as_str() {
                        "recent_files" => &["path"],
                        "io_maps" => &["system", "device"],
                        "paths" => &["name", "direction"],
                        "hardware_latency" => &["send", "ret", "rate"],
                        _ => &[],
                    };
                    *raw = merge_entries(raw, previous, value, identity);
                }
            } else {
                raw.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Preserve extra fields on retained entries even when a list is reordered or extended.
fn merge_entries(raw: &serde_json::Value, previous: &serde_json::Value, current: &serde_json::Value, identity: &[&str]) -> serde_json::Value {
    let (Some(raw), Some(previous), Some(current_entries)) = (raw.as_array(), previous.as_array(), current.as_array()) else {
        return current.clone();
    };
    if identity.is_empty() {
        return current.clone();
    }
    serde_json::Value::Array(
        current_entries
            .iter()
            .map(|entry| {
                let matches = |other: &&serde_json::Value| identity.iter().all(|key| entry.get(key).is_some() && entry.get(key) == other.get(key));
                if let (Some(raw), Some(previous)) = (raw.iter().find(matches), previous.iter().find(matches)) {
                    let mut kept = raw.clone();
                    merge_ui(&mut kept, previous, entry);
                    kept
                } else {
                    entry.clone()
                }
            })
            .collect(),
    )
}

/// Write settings atomically (temporary file, then rename). Unreadable files are left intact.
pub fn save_ui(path: &Path, ui: &transit_ui_studio::UiState) -> Result<(), String> {
    use std::io::Write as _;
    let mut saved = ui.clone();
    saved.plugin_windows.clear();
    let mut raw = read_ui(path)?.unwrap_or_else(|| serde_json::json!({}));
    let previous = serde_json::to_value(decode_ui(raw.clone())?).map_err(|error| error.to_string())?;
    let current = serde_json::to_value(saved).map_err(|error| error.to_string())?;
    let original = raw.clone();
    merge_ui(&mut raw, &previous, &current);
    if raw == original {
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(&raw).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_UI_BYTES {
        return Err("Settings file exceeds the 4 MiB limit".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_nanos();
    let temporary = path.with_extension(format!("json.{}.{nonce}.tmp", std::process::id()));
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary).map_err(|error| format!("Settings could not be saved: {error}"))?;
    let written = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(|error| format!("Settings could not be saved: {error}"))
}

/// Owns only a fresh temporary directory, never an earlier run's recovery files.
pub struct TemporaryAutosave(PathBuf);

impl TemporaryAutosave {
    pub fn new() -> std::io::Result<Self> {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(std::io::Error::other)?.as_nanos();
        let path = std::env::temp_dir().join(format!("transit-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryAutosave {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            log::warn!("Temporary autosave folder could not be removed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_folder_and_soundcrafts_are_separate() {
        let env = |n: &str| match n {
            "APPDATA" => Some(OsString::from("C:/Users/x/AppData/Roaming")),
            _ => None,
        };
        assert_eq!(
            dir_with(Platform::Windows, "TRANSIT_CONFIG_DIR", ("transit", "transit"), env),
            Some(PathBuf::from("C:/Users/x/AppData/Roaming/transit"))
        );
        assert_eq!(
            dir_with(Platform::Windows, "SOUNDCRAFT_CONFIG_DIR", ("SoundCraft", "soundcraft"), env),
            Some(PathBuf::from("C:/Users/x/AppData/Roaming/SoundCraft"))
        );
        let empty = |n: &str| (n == "TRANSIT_CONFIG_DIR").then(OsString::new);
        assert_eq!(dir_with(Platform::Other, "TRANSIT_CONFIG_DIR", ("transit", "transit"), empty), None);
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("transit-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn legacy_folder_is_the_old_name_and_never_used_with_an_override() {
        let env = |n: &str| match n {
            "APPDATA" => Some(OsString::from("C:/Users/x/AppData/Roaming")),
            _ => None,
        };
        assert_eq!(legacy_dir_with(Platform::Windows, env), Some(PathBuf::from("C:/Users/x/AppData/Roaming/transitDAW")));
        let over = |n: &str| (n == "TRANSIT_CONFIG_DIR").then(|| OsString::from("D:/x"));
        assert_eq!(legacy_dir_with(Platform::Windows, over), None);
    }

    #[test]
    fn migration_moves_the_old_folder_when_the_new_one_is_absent() {
        let root = temp_root("move");
        let (old, new) = (root.join("transitDAW"), root.join("transit"));
        std::fs::create_dir_all(old.join("Autosave")).unwrap();
        std::fs::write(old.join("ui.json"), b"{}").unwrap();
        std::fs::write(old.join("Autosave/a.transit"), b"a").unwrap();
        assert_eq!(migrate_legacy_dir(&new, &old), Migration::Moved);
        assert!(new.join("ui.json").is_file() && new.join("Autosave/a.transit").is_file());
        assert!(!old.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migration_leaves_both_folders_alone_when_the_new_one_exists() {
        let root = temp_root("both");
        let (old, new) = (root.join("transitDAW"), root.join("transit"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("ui.json"), b"old").unwrap();
        std::fs::write(new.join("ui.json"), b"new").unwrap();
        assert_eq!(migrate_legacy_dir(&new, &old), Migration::None);
        assert_eq!(std::fs::read(old.join("ui.json")).unwrap(), b"old");
        assert_eq!(std::fs::read(new.join("ui.json")).unwrap(), b"new");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migration_with_neither_folder_is_a_fresh_start() {
        let root = temp_root("neither");
        let (old, new) = (root.join("transitDAW"), root.join("transit"));
        assert_eq!(migrate_legacy_dir(&new, &old), Migration::None);
        assert!(!old.exists() && !new.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_fallback_copies_the_tree_and_keeps_the_old_folder() {
        let root = temp_root("copy");
        let (old, new) = (root.join("transitDAW"), root.join("transit"));
        std::fs::create_dir_all(old.join("logs")).unwrap();
        std::fs::write(old.join("ui.json"), b"{}").unwrap();
        std::fs::write(old.join("logs/x.log"), b"l").unwrap();
        copy_dir(&old, &new).unwrap();
        assert!(new.join("ui.json").is_file() && new.join("logs/x.log").is_file());
        assert!(old.join("ui.json").is_file(), "the old folder stays");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_io_map_round_trips_through_ui_json_and_old_files_load_without_it() {
        let dir = std::env::temp_dir().join(format!("transit-io-map-{}", std::process::id()));
        let path = dir.join("ui.json");
        let mut ui = transit_ui_studio::UiState::default();
        let mut map = soundcraft_engine::model::IoMap::for_device("ASIO", "TASCAM Model 12", 12, 10);
        map.rename_path("Out 3-4", "Cue").unwrap();
        ui.io_maps.push(map.clone());
        save_ui(&path, &ui).unwrap();
        let back = load_ui(&path).unwrap().unwrap();
        assert_eq!(back.io_maps, [map]);
        assert_eq!((back.io_maps[0].inputs, back.io_maps[0].outputs), (12, 10), "the opened channel counts are stored inside");
        // A file written before the table existed.
        let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("io_maps");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(load_ui(&path).unwrap().unwrap().io_maps.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recent_files_round_trip_through_ui_json_and_old_files_load_without_them() {
        let dir = std::env::temp_dir().join(format!("transit-recent-files-{}", std::process::id()));
        let path = dir.join("ui.json");
        let mut ui = transit_ui_studio::UiState::default();
        ui.remember_recent(Path::new("/music/a.transit"), 100);
        ui.remember_recent(Path::new("/music/b.transit"), 200);
        save_ui(&path, &ui).unwrap();
        assert_eq!(load_ui(&path).unwrap().unwrap().recent_files, ui.recent_files);
        let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("recent_files");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(load_ui(&path).unwrap().unwrap().recent_files.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_round_trip() {
        let dir = std::env::temp_dir().join(format!("transit-ui-prefs-{}", std::process::id()));
        let path = dir.join("ui.json");
        let mut ui = transit_ui_studio::UiState { track_height: 60.0, ..Default::default() };
        ui.keys.velocity = 87;
        ui.plugin_windows.push((soundcraft_engine::model::TrackId(1), 0));
        save_ui(&path, &ui).unwrap();
        let back = load_ui(&path).unwrap().unwrap();
        assert_eq!(back.track_height, 60.0);
        assert_eq!(back.keys.velocity, 87, "new-note velocity is saved in ui.json");
        assert!(back.plugin_windows.is_empty(), "open editors are not remembered");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod preference_regressions {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_bad_field_does_not_discard_other_preferences_or_future_data() {
        let dir = TemporaryAutosave::new().unwrap();
        let path = dir.path().join("ui.json");
        let original = json!({
            "track_height": "future-size",
            "window_size": [1234, 789],
            "audio": {"backend": "future-driver", "future_calibration": [1, 2, 3]},
            "io_maps": [{"future_map": true}],
            "future_preference": {"keep": true}
        });
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        let mut ui = load_ui(&path).unwrap().unwrap();
        assert_eq!(ui.window_size, [1234.0, 789.0]);
        assert_eq!(ui.track_height, transit_ui_studio::UiState::default().track_height);
        ui.show_mixer = true;
        save_ui(&path, &ui).unwrap();
        let saved = read_ui(&path).unwrap().unwrap();
        for key in ["track_height", "audio", "io_maps", "future_preference"] {
            assert_eq!(saved[key], original[key], "preserve {key}");
        }
        assert_eq!(saved["show_mixer"], true);
    }

    #[test]
    fn changed_nested_preferences_keep_unknown_siblings() {
        let dir = TemporaryAutosave::new().unwrap();
        let path = dir.path().join("ui.json");
        std::fs::write(&path, br#"{"keys":{"velocity":80,"future":17}}"#).unwrap();
        let mut ui = load_ui(&path).unwrap().unwrap();
        ui.keys.velocity = 90;
        save_ui(&path, &ui).unwrap();
        let saved = read_ui(&path).unwrap().unwrap();
        assert_eq!(saved["keys"]["velocity"], 90);
        assert_eq!(saved["keys"]["future"], 17);
    }

    #[test]
    fn unreadable_settings_are_not_overwritten() {
        let dir = TemporaryAutosave::new().unwrap();
        let path = dir.path().join("ui.json");
        for bytes in [b"{broken".to_vec(), b"[]".to_vec(), vec![b' '; MAX_UI_BYTES as usize + 1]] {
            std::fs::write(&path, &bytes).unwrap();
            assert!(save_ui(&path, &Default::default()).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn temporary_autosaves_remove_only_the_owned_directory_on_drop() {
        let first = TemporaryAutosave::new().unwrap();
        let second = TemporaryAutosave::new().unwrap();
        let path = first.path().to_path_buf();
        std::fs::create_dir(path.join("Autosave")).unwrap();
        std::fs::write(path.join("Autosave/session.transit"), b"test").unwrap();
        drop(first);
        assert!(!path.exists());
        assert!(second.path().exists());
    }
}

#[cfg(test)]
mod array_preservation_tests {
    use super::*;

    #[test]
    fn changing_lists_keeps_future_fields_on_retained_entries() {
        let dir = TemporaryAutosave::new().unwrap();
        let path = dir.path().join("ui.json");
        let mut ui = transit_ui_studio::UiState::default();
        ui.remember_recent(Path::new("old.transit"), 1);
        ui.io_maps.push(soundcraft_engine::model::IoMap::for_device("Test", "Device", 2, 2));
        let mut raw = serde_json::to_value(&ui).unwrap();
        raw["recent_files"][0]["future"] = 17.into();
        raw["io_maps"][0]["future"] = 18.into();
        raw["io_maps"][0]["paths"][0]["future"] = 19.into();
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        ui.remember_recent(Path::new("new.transit"), 2);
        ui.io_maps[0].outputs = 4;
        ui.io_maps[0].paths[0].channels = vec![1];
        save_ui(&path, &ui).unwrap();
        let saved = read_ui(&path).unwrap().unwrap();
        assert_eq!(saved["recent_files"][1]["future"], 17);
        assert_eq!(saved["io_maps"][0]["future"], 18);
        assert_eq!(saved["io_maps"][0]["paths"][0]["future"], 19);
        assert_eq!(saved["io_maps"][0]["outputs"], 4);
        assert_eq!(saved["io_maps"][0]["paths"][0]["channels"], serde_json::json!([1]));
    }
}
