//! Hosted plug-ins' user preset files (issue #68; formats and folders in [`crate::presets`]),
//! read, written, listed and deleted through the checked file access (`crate::file_access`), like
//! every other file the engine touches.

use crate::presets::{decode, encode, extension, folder};
use std::path::Path;

/// Largest preset file read (two 64 MiB states and the container).
pub const MAX_FILE_BYTES: u64 = 130 << 20;

/// The user presets of plug-in `id` under the settings folder `root`, sorted by name
/// (case-insensitive). Empty when there are none, or the folder may not be read.
#[must_use]
pub fn list(root: &Path, id: &str) -> Vec<String> {
    let Some(ext) = extension(id) else { return Vec::new() };
    let dir = folder(root, id);
    if crate::file_access::check_read(&dir).is_err() {
        return Vec::new();
    }
    let mut v: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let ok = p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case(ext)) && e.file_type().is_ok_and(|t| t.is_file());
            ok.then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned())).flatten()
        })
        .collect();
    v.sort_by_key(|s| s.to_lowercase());
    v
}

/// Writes plug-in `id`'s state to `path` as its preset file, creating the folder. The file is
/// written beside it first and then moved into place, so a failed write never leaves half a
/// preset.
///
/// # Errors
/// A state the format cannot hold, a path outside the automation write root, or a failed write.
pub fn write(path: &Path, id: &str, state: &[u8]) -> Result<(), String> {
    crate::file_access::check_write(path)?;
    let bytes = encode(id, state)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp-preset");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {e}", path.display())
    })
}

/// The state of plug-in `id` in the preset file at `path`.
///
/// # Errors
/// The file cannot be read (or is outside the automation read root), is too large, or is not
/// this plug-in's preset.
pub fn read(path: &Path, id: &str) -> Result<Vec<u8>, String> {
    crate::file_access::check_read(path)?;
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(format!("{} is not a preset file of a size this build reads", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    decode(id, &bytes)
}

/// Deletes a preset file; anything that is not a `.vstpreset` or `.clappreset` is refused.
///
/// # Errors
/// Not a preset file, outside the automation write root, or the delete failed.
pub fn delete(path: &Path) -> Result<(), String> {
    if !crate::presets::is_preset_file(path) {
        return Err("only .vstpreset and .clappreset files are deleted".into());
    }
    crate::file_access::check_write(path)?;
    std::fs::remove_file(path).map_err(|e| format!("cannot delete {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::presets::path;

    const VST3: &str = "vst3:0123456789ABCDEFFEDCBA9876543210";
    const CLAP: &str = "clap:com.example.gain";

    #[test]
    fn write_list_read_and_delete_in_a_temp_folder() {
        let root = std::env::temp_dir().join(format!("transit-presets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = soundcraft_vst3_host::preset::join_state(b"comp", b"ctrl").unwrap();
        for name in ["b kit", "A Kit"] {
            write(&path(&root, VST3, name).unwrap(), VST3, &state).unwrap();
        }
        std::fs::write(folder(&root, VST3).join("stray.txt"), b"x").unwrap();
        assert_eq!(list(&root, VST3), vec!["A Kit".to_string(), "b kit".to_string()], "sorted, presets only");
        let a = path(&root, VST3, "A Kit").unwrap();
        assert_eq!(read(&a, VST3).unwrap(), state);
        assert!(read(&a, "vst3:00000000000000000000000000000000").is_err(), "another plug-in");
        assert!(list(&root, CLAP).is_empty());
        assert!(delete(&folder(&root, VST3).join("stray.txt")).is_err(), "not a preset");
        delete(&a).unwrap();
        assert_eq!(list(&root, VST3), vec!["b kit".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
