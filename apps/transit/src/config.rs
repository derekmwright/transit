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

/// transitDAW's settings folder (`TRANSIT_CONFIG_DIR` overrides).
pub fn config_dir() -> Option<PathBuf> {
    dir_with(platform(), "TRANSIT_CONFIG_DIR", ("transitDAW", "transitdaw"), |n| std::env::var_os(n))
}

/// SoundCraft's settings folder (`SOUNDCRAFT_CONFIG_DIR` overrides), for the shared plug-in scan
/// cache, so a large library is scanned once for both apps.
pub fn soundcraft_dir() -> Option<PathBuf> {
    dir_with(platform(), "SOUNDCRAFT_CONFIG_DIR", ("SoundCraft", "soundcraft"), |n| std::env::var_os(n))
}

const MAX_UI_BYTES: u64 = 4 * 1024 * 1024;

/// Read bounded settings; `Ok(None)` when there are none yet.
pub fn load_ui(path: &Path) -> Result<Option<soundcraft_ui_studio::UiState>, String> {
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
pub fn save_ui(path: &Path, ui: &soundcraft_ui_studio::UiState) -> Result<(), String> {
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
            dir_with(Platform::Windows, "TRANSIT_CONFIG_DIR", ("transitDAW", "transitdaw"), env),
            Some(PathBuf::from("C:/Users/x/AppData/Roaming/transitDAW"))
        );
        assert_eq!(
            dir_with(Platform::Windows, "SOUNDCRAFT_CONFIG_DIR", ("SoundCraft", "soundcraft"), env),
            Some(PathBuf::from("C:/Users/x/AppData/Roaming/SoundCraft"))
        );
        let empty = |n: &str| (n == "TRANSIT_CONFIG_DIR").then(OsString::new);
        assert_eq!(dir_with(Platform::Other, "TRANSIT_CONFIG_DIR", ("transitDAW", "transitdaw"), empty), None);
    }

    #[test]
    fn the_io_map_round_trips_through_ui_json_and_old_files_load_without_it() {
        let dir = std::env::temp_dir().join(format!("transit-io-map-{}", std::process::id()));
        let path = dir.join("ui.json");
        let mut ui = soundcraft_ui_studio::UiState::default();
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
    fn settings_round_trip() {
        let dir = std::env::temp_dir().join(format!("transit-ui-prefs-{}", std::process::id()));
        let path = dir.join("ui.json");
        let mut ui = soundcraft_ui_studio::UiState { track_height: 60.0, ..Default::default() };
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
