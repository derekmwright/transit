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

/// Write settings atomically (temporary file, then rename).
pub fn save_ui(path: &Path, ui: &transit_ui_studio::UiState) -> Result<(), String> {
    use std::io::Write as _;
    let mut saved = ui.clone();
    saved.plugin_windows.clear();
    let bytes = serde_json::to_vec_pretty(&saved).map_err(|error| error.to_string())?;
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
