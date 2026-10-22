//! Plug-in scan bootstrap. **Copied** from `crates/ui-egui/src/plugin_folders.rs` (`PluginDirs`,
//! `start`, `scan_child`) at `transit` 2af868e (file last changed da11565). The app must answer
//! [`scan_child`] first thing in `main`, or every scan child starts a whole app (E1/E2).

use std::path::{Path, PathBuf};

/// The user's own plugin folders, scanned besides the standard ones (kept with the preferences).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PluginDirs {
    pub vst3: Vec<String>,
    pub clap: Vec<String>,
}

/// At launch: remembers scans in `settings_dir` (when there is one), adds the user's folders and
/// starts scanning on a background thread. Plugin files are loaded for the scan by `scanner`
/// (this app's own program, run with [`scan_child`]'s arguments) in a child process.
pub fn start(dirs: &PluginDirs, settings_dir: Option<&Path>, scanner: Option<PathBuf>) {
    soundcraft_vst3_host::set_extra_dirs(dirs.vst3.iter().map(PathBuf::from).collect());
    soundcraft_clap_host::set_extra_dirs(dirs.clap.iter().map(PathBuf::from).collect());
    soundcraft_vst3_host::set_scanner(scanner.clone());
    soundcraft_clap_host::set_scanner(scanner);
    if let Some(dir) = settings_dir {
        soundcraft_vst3_host::set_cache_file(Some(dir.join("plugins-vst3.json")));
        soundcraft_clap_host::set_cache_file(Some(dir.join("plugins-clap.json")));
        STARTED.store(true, std::sync::atomic::Ordering::Release);
        STARTED.store(true, std::sync::atomic::Ordering::Release);
        soundcraft_vst3_host::scan_in_background();
        soundcraft_clap_host::scan_in_background();
    }
}

/// The scan child process's work: with `<flag> <file>` arguments (see [`start`]), loads that one
/// plugin file and returns the line to print; `None` for any other arguments.
pub fn scan_child(args: &[String]) -> Option<String> {
    let [flag, file] = args else { return None };
    let file = Path::new(file);
    if flag == soundcraft_vst3_host::SCAN_FLAG {
        Some(soundcraft_vst3_host::scan_reply(file))
    } else if flag == soundcraft_clap_host::SCAN_FLAG {
        Some(soundcraft_clap_host::scan_reply(file))
    } else {
        None
    }
}

/// Set once a scan has been started (at launch with a settings folder, or by [`rescan`]).
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Scans again on background threads, reusing what the cache remembers about unchanged files.
/// A scan already running keeps running. Works without a settings folder (nothing is remembered).
pub fn rescan() {
    STARTED.store(true, std::sync::atomic::Ordering::Release);
    soundcraft_vst3_host::rescan_in_background(false);
    soundcraft_clap_host::rescan_in_background(false);
}

/// Where the scan is: `"idle"` (none started), `"running"`, `"done"`, or `"failed: <why>"`
/// (a scan thread ended without a result).
pub fn state() -> String {
    use std::sync::atomic::Ordering;
    if soundcraft_vst3_host::scanning() || soundcraft_clap_host::scanning() {
        "running".to_string()
    } else if !STARTED.load(Ordering::Acquire) {
        "idle".to_string()
    } else if soundcraft_vst3_host::cached().is_none() {
        "failed: the VST3 scan ended without a result".to_string()
    } else if soundcraft_clap_host::cached().is_none() {
        "failed: the CLAP scan ended without a result".to_string()
    } else {
        "done".to_string()
    }
}

/// "Scanning…" for menus that list scanned plug-ins, with the file the scan is on when it says
/// (`progress`); `None` when no scan runs. The host crates expose no count of plug-ins found so far
/// (`cached()` is empty until a format's scan ends), so that is all a menu can show.
pub fn scanning_label() -> Option<String> {
    if state() != "running" {
        return None;
    }
    let at = soundcraft_vst3_host::progress().map(|p| ("VST3", p)).or_else(|| soundcraft_clap_host::progress().map(|p| ("CLAP", p)));
    Some(match at {
        Some((format, (i, n, _))) => format!("Scanning… ({format} file {} of {n})", (i + 1).min(n)),
        None => "Scanning…".to_string(),
    })
}

/// `plugin_scan` for `ui.inspect`: the [`state`] and the counts found so far.
pub fn inspect() -> serde_json::Value {
    let count = |n: Option<usize>| n.map_or(serde_json::Value::Null, Into::into);
    serde_json::json!({
        "state": state(),
        "vst3": count(soundcraft_vst3_host::cached().map(|l| l.len())),
        "clap": count(soundcraft_clap_host::cached().map(|l| l.len())),
    })
}

/// Where a running scan is, for the launch splash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanProgress {
    /// The format being scanned ("VST3" or "CLAP").
    pub format: &'static str,
    /// Files done so far (the one loading counts as in progress, so this is its 1-based index).
    pub index: usize,
    /// Plug-in files found to scan.
    pub total: usize,
    /// The folder holding the file being loaded.
    pub folder: String,
}

/// The running scan's counts; `None` when no scan runs or it only reuses the cache file.
pub fn progress() -> Option<ScanProgress> {
    if state() != "running" {
        return None;
    }
    let at = soundcraft_vst3_host::progress().map(|p| ("VST3", p)).or_else(|| soundcraft_clap_host::progress().map(|p| ("CLAP", p)))?;
    let (format, (i, n, file)) = at;
    let folder = file.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Some(ScanProgress { format, index: (i + 1).min(n), total: n, folder })
}
