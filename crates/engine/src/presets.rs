//! User presets of hosted plug-ins (issue #68): a preset is the plug-in's full state, the same
//! state a session stores, written in the format's own preset file so other hosts read it:
//! `.vstpreset` for VST3 (Steinberg's container, component and controller state) and transit's
//! documented container for CLAP (`soundcraft_clap_host::preset`). They live in the settings
//! folder's `presets/` (beside the built-in devices' presets), one folder per plug-in type
//! ([`folder`]). Reading, writing and listing them go through the checked file access
//! ([`crate::io::presets`]).

use std::path::{Path, PathBuf};

/// A hosted plug-in format with preset files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Vst3,
    Clap,
}

/// The preset format of plug-in `id` (`None`: built-in devices and Audio Units, which have none
/// here).
#[must_use]
pub fn format_of(id: &str) -> Option<Format> {
    if soundcraft_vst3_host::parse_id(id).is_some() {
        Some(Format::Vst3)
    } else if soundcraft_clap_host::parse_id(id).is_some() {
        Some(Format::Clap)
    } else {
        None
    }
}

/// The file extension of plug-in `id`'s presets.
#[must_use]
pub fn extension(id: &str) -> Option<&'static str> {
    Some(match format_of(id)? {
        Format::Vst3 => soundcraft_vst3_host::preset::EXTENSION,
        Format::Clap => soundcraft_clap_host::preset::EXTENSION,
    })
}

/// Whether `path` names a preset file of either format (the only files a delete touches).
#[must_use]
pub fn is_preset_file(path: &Path) -> bool {
    path.extension().and_then(|x| x.to_str()).is_some_and(|x| {
        x.eq_ignore_ascii_case(soundcraft_vst3_host::preset::EXTENSION) || x.eq_ignore_ascii_case(soundcraft_clap_host::preset::EXTENSION)
    })
}

/// A file-safe name: path separators and other trouble characters become spaces.
#[must_use]
pub fn file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || " -_().,'&+!#".contains(c) { c } else { ' ' }).collect();
    s.trim().trim_matches('.').trim().to_string()
}

/// The folder name of plug-in `id`'s presets: its id made file-safe (`vst3-<class id>`,
/// `clap-<id>`).
#[must_use]
pub fn folder_name(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect()
}

/// Where plug-in `id`'s user presets live under the settings folder `root`.
#[must_use]
pub fn folder(root: &Path, id: &str) -> PathBuf {
    root.join("presets").join(folder_name(id))
}

/// The file of user preset `name` of plug-in `id` (`None`: no preset format, or an empty name).
#[must_use]
pub fn path(root: &Path, id: &str, name: &str) -> Option<PathBuf> {
    let name = file_name(name);
    if name.is_empty() {
        return None;
    }
    let ext = extension(id)?;
    Some(folder(root, id).join(format!("{name}.{ext}")))
}

/// The preset name of a file: its stem.
#[must_use]
pub fn name_of(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Plug-in `id`'s state as its preset file.
///
/// # Errors
/// A plug-in without a preset format, or a state the format cannot hold.
pub fn encode(id: &str, state: &[u8]) -> Result<Vec<u8>, String> {
    match format_of(id).ok_or_else(|| format!("`{id}` has no preset file format"))? {
        Format::Vst3 => soundcraft_vst3_host::preset::file_from_state(id, state),
        Format::Clap => soundcraft_clap_host::preset::file_from_state(id, state),
    }
}

/// The state a preset file holds for plug-in `id`.
///
/// # Errors
/// A plug-in without a preset format, a file that is not its preset, or one for another plug-in.
pub fn decode(id: &str, file: &[u8]) -> Result<Vec<u8>, String> {
    match format_of(id).ok_or_else(|| format!("`{id}` has no preset file format"))? {
        Format::Vst3 => soundcraft_vst3_host::preset::state_from_file(id, file),
        Format::Clap => soundcraft_clap_host::preset::state_from_file(id, file),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const VST3: &str = "vst3:0123456789ABCDEFFEDCBA9876543210";
    const CLAP: &str = "clap:com.example.gain";

    #[test]
    fn folders_names_and_formats() {
        assert_eq!(format_of(VST3), Some(Format::Vst3));
        assert_eq!(format_of(CLAP), Some(Format::Clap));
        assert_eq!(format_of("compressor"), None);
        assert_eq!(extension(VST3), Some("vstpreset"));
        assert_eq!(extension(CLAP), Some("clappreset"));
        assert_eq!(folder_name(VST3), "vst3-0123456789ABCDEFFEDCBA9876543210");
        assert_eq!(folder_name(CLAP), "clap-com.example.gain");
        assert_eq!(file_name("../My Kit: 2/"), "My Kit  2");
        let root = Path::new("cfg");
        assert_eq!(path(root, CLAP, "My Kit").unwrap(), Path::new("cfg/presets/clap-com.example.gain/My Kit.clappreset"));
        assert_eq!(path(root, CLAP, "///"), None);
        assert_eq!(path(root, "compressor", "x"), None);
        assert!(is_preset_file(Path::new("a/b.VSTPRESET")) && is_preset_file(Path::new("x.clappreset")));
        assert!(!is_preset_file(Path::new("session.transit")) && !is_preset_file(Path::new("noext")));
    }
}
