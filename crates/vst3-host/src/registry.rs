//! The process-wide VST3 registry: scan cache, leaked `PluginInfo`s, instantiation.

use crate::ffi::{Bundle, Instance, PARAM_BYPASS, PARAM_HIDDEN, PARAM_READ_ONLY, RawParam};
use crate::plugin::Vst3Plugin;
use crate::scan as paths;
use crate::scan_cache;
use crate::{AUDIO_MODULE_CLASS, Vst3Descriptor, Vst3Error, parse_id};
use soundcraft_dsp::{ParamInfo, Plugin, PluginInfo, Taper, Unit};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Most labels read for a stepped parameter (more steps are shown as a plain range).
const MAX_CHOICES: i32 = 128;

#[derive(Default)]
struct Registry {
    scanned: Option<Vec<Vst3Descriptor>>,
    /// While a scan runs: the plugins of the cache file or of the scan before, so that a plugin
    /// can be created without waiting for the scan.
    remembered: Vec<Vst3Descriptor>,
    extra_dirs: Vec<PathBuf>,
    /// Folders whose add_search_dir completed while a scan had not published yet.
    refresh_dirs: HashSet<PathBuf>,
    /// Where scans are remembered across runs (see [`scan_cache`]); `None` keeps them in memory only.
    cache_file: Option<PathBuf>,
    /// The program that loads files for scans in a child process (see [`set_scanner`]).
    scanner: Option<PathBuf>,
    /// The files the last scan could not load, with the reason.
    problems: Vec<(PathBuf, String)>,
    /// One leaked `PluginInfo` per plugin type that was actually instantiated (bounded by the
    /// number of distinct installed plugins).
    infos: HashMap<String, &'static PluginInfo>,
    /// Ids that failed to instantiate (not retried every UI frame).
    failed: HashMap<String, Vst3Error>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock() -> MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Held for a whole scan, so that scans run one at a time. The registry itself is not locked
/// while plugin files load, so that the UI can ask for [`cached`] and plugin infos meanwhile.
fn scan_lock() -> MutexGuard<'static, ()> {
    static S: Mutex<()> = Mutex::new(());
    S.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Set while [`scan_in_background`]'s thread runs.
static BACKGROUND: AtomicBool = AtomicBool::new(false);

/// Set by [`skip_scan`]: the running scan loads no more files.
static SKIP: AtomicBool = AtomicBool::new(false);

/// The file a scan is loading: its index, the number of files, its path.
fn loading() -> MutexGuard<'static, Option<(usize, usize, PathBuf)>> {
    static L: Mutex<Option<(usize, usize, PathBuf)>> = Mutex::new(None);
    L.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Descriptors of every audio module class in one `.vst3`.
pub fn load_bundle(path: &Path) -> Result<Vec<Vst3Descriptor>, Vst3Error> {
    let b = Bundle::load(path)?;
    Ok(b.classes()
        .into_iter()
        .filter(|c| c.category == AUDIO_MODULE_CLASS)
        .map(|c| {
            let subs = paths::split_sub_categories(&c.sub_categories);
            let class_id = crate::ffi::hex(&c.cid);
            Vst3Descriptor {
                id: format!("{}{class_id}", crate::ID_PREFIX),
                category: paths::category_from_sub_categories(&subs),
                is_instrument: paths::is_instrument(&subs),
                class_id,
                name: if c.name.is_empty() { "VST3 plugin".to_string() } else { c.name },
                vendor: c.vendor,
                version: c.version,
                sdk_version: c.sdk_version,
                sub_categories: subs,
                path: path.to_path_buf(),
            }
        })
        .collect())
}

/// Scans `dirs` (uncached). Unloadable files are logged and skipped; the first plugin with a
/// given id wins.
pub fn scan_paths(dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
    merge(scan_cache::scan(paths::find_bundles(dirs), &[], paths::bundle_binary, |p| Some(load_bundle(p).map_err(|e| e.to_string())), |_, _, _| true))
}

/// The plugins of every file, the first plugin with a given id winning, sorted by name.
fn merge(files: Vec<scan_cache::Entry<Vst3Descriptor>>) -> Vec<Vst3Descriptor> {
    let mut out: Vec<Vst3Descriptor> = Vec::new();
    for d in files.into_iter().flat_map(|f| f.plugins) {
        if !out.iter().any(|o| o.id == d.id) {
            out.push(d);
        }
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// Scans the search folders and keeps the result. With `reuse`, files the cache file knows are
/// not loaded again unless they changed; either way the cache file is rewritten.
fn scan_now(reuse: bool) -> Vec<Vst3Descriptor> {
    let (dirs, mut taken, cache_file, scanner) = {
        let mut r = lock();
        // Unit tests do not load installed plugins; integration tests use an explicit opt-in.
        #[cfg(test)]
        let mut dirs = Vec::new();
        #[cfg(not(test))]
        let mut dirs = paths::default_search_paths();
        #[cfg(feature = "test-hooks")]
        if EXTRA_DIRS_ONLY.load(Ordering::Acquire) {
            dirs.clear();
        }
        dirs.extend(r.extra_dirs.iter().cloned());
        // Anything added before this snapshot is covered by the initial scan.
        r.refresh_dirs.clear();
        (dirs, r.extra_dirs.clone(), r.cache_file.clone(), r.scanner.clone())
    };
    let load = |p: &Path| match &scanner {
        // Skip kills a child that is loading; a file loading here has to finish.
        Some(program) => scan_cache::load_in_child(program, SCAN_FLAG, p, || SKIP.load(Ordering::Acquire)),
        None => Some(load_bundle(p).map_err(|e| e.to_string())),
    };
    let known = match &cache_file {
        Some(f) if reuse => scan_cache::read(f),
        _ => Vec::new(),
    };
    if !known.is_empty() {
        lock().remembered = merge(known.clone());
    }
    SKIP.store(false, Ordering::Release);
    let ahead = |i: usize, n: usize, p: &Path| {
        *loading() = Some((i, n, p.to_path_buf()));
        !SKIP.load(Ordering::Acquire)
    };
    let mut files = scan_cache::scan(paths::find_bundles(&dirs), &known, paths::bundle_binary, load, ahead);
    let mut rounds = 0;
    // Every round uses the same loader, cancellation, progress and file-level cache entries.
    // Publish after at most four late rounds even if additions continue. Remaining folders stay
    // registered and are covered by the next explicit rescan (scan() returns this publication).
    loop {
        *loading() = None;
        #[cfg(feature = "test-hooks")]
        run_after_load_hook();
        let mut r = lock();
        let mut late: Vec<PathBuf> = r.extra_dirs.iter().filter(|d| !taken.contains(d) || r.refresh_dirs.contains(*d)).cloned().collect();
        if late.is_empty() || rounds == MAX_LATE_SCAN_ROUNDS {
            let problems = files.iter().filter_map(|f| Some((f.path.clone(), f.error.clone()?))).collect();
            let list = merge(files.clone());
            r.scanned = Some(list.clone());
            r.problems = problems;
            // The check and publication share the registry lock: a later add merges into scanned.
            drop(r);
            if let Some(f) = &cache_file {
                scan_cache::write(f, &files);
            }
            return list;
        }
        for dir in &late {
            r.refresh_dirs.remove(dir);
        }
        drop(r);
        late.sort();
        late.dedup();
        // Reuse this scan's entries first, then the previous cache, including failed files.
        let mut late_known = files.clone();
        late_known.extend(known.iter().cloned());
        let bundles = paths::find_bundles(&late);
        let added = scan_cache::scan(bundles.clone(), &late_known, paths::bundle_binary, load, ahead);
        // Replace file entries, including failures and skipped files, rather than just merging
        // descriptors: otherwise an updated file can leave stale plugins or duplicate cache rows.
        files.retain(|f| !bundles.contains(&f.path) || added.iter().any(|e| e.path == f.path));
        for entry in added {
            if let Some(previous) = files.iter_mut().find(|f| f.path == entry.path) {
                *previous = entry;
            } else {
                files.push(entry);
            }
        }
        taken.extend(late);
        rounds += 1;
    }
}

/// A stream of folder additions must not keep the scan lock forever.
const MAX_LATE_SCAN_ROUNDS: usize = 4;

#[cfg(feature = "test-hooks")]
static EXTRA_DIRS_ONLY: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "test-hooks")]
pub fn set_scan_extra_dirs_only(only: bool) {
    EXTRA_DIRS_ONLY.store(only, Ordering::Release);
}

/// Test hook: runs after each scan round loads its files and before publication.
#[cfg(feature = "test-hooks")]
static AFTER_LOAD: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

#[cfg(feature = "test-hooks")]
pub fn set_after_load_hook(hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
    *AFTER_LOAD.lock().unwrap_or_else(PoisonError::into_inner) = hook;
}

#[cfg(feature = "test-hooks")]
fn run_after_load_hook() {
    let hook = AFTER_LOAD.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if let Some(h) = hook {
        h();
    }
}

pub fn scan() -> Vec<Vst3Descriptor> {
    let _one_at_a_time = scan_lock();
    cached().unwrap_or_else(|| scan_now(true))
}

pub fn cached() -> Option<Vec<Vst3Descriptor>> {
    lock().scanned.clone()
}

pub fn rescan() -> Vec<Vst3Descriptor> {
    rescan_with(false)
}

/// Forgets the scan and the failed instances and scans again, reusing the cache file with `reuse`.
fn rescan_with(reuse: bool) -> Vec<Vst3Descriptor> {
    let _one_at_a_time = scan_lock();
    {
        let mut r = lock();
        if let Some(before) = r.scanned.take() {
            r.remembered = before;
        }
        r.failed.clear();
    }
    scan_now(reuse)
}

pub fn set_cache_file(file: Option<PathBuf>) {
    lock().cache_file = file;
}

pub fn scan_in_background() {
    if cached().is_none() {
        in_background(|| {
            scan();
        });
    }
}

pub fn rescan_in_background(clear_cache: bool) {
    in_background(if clear_cache { || drop(rescan_with(false)) } else { || drop(rescan_with(true)) });
}

/// Runs `job` on a scan thread, unless one is running already.
fn in_background(job: fn()) {
    if BACKGROUND.swap(true, Ordering::AcqRel) {
        return;
    }
    /// Clears [`BACKGROUND`] however the job ends.
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            BACKGROUND.store(false, Ordering::Release);
        }
    }
    let done = Done;
    if let Err(e) = std::thread::Builder::new().name("vst3-scan".into()).spawn(move || {
        let _done = done;
        job();
    }) {
        log::warn!("vst3: no scan thread: {e}");
    }
}

pub fn search_dirs() -> (Vec<PathBuf>, Vec<PathBuf>) {
    (paths::default_search_paths(), lock().extra_dirs.clone())
}

pub fn set_extra_dirs(dirs: Vec<PathBuf>) {
    lock().extra_dirs = dirs;
}

/// The argument that makes the scan program load one file: `<program> --scan-vst3 <file>`.
pub const SCAN_FLAG: &str = "--scan-vst3";

pub fn set_scanner(program: Option<PathBuf>) {
    lock().scanner = program;
}

pub fn scan_reply(file: &Path) -> String {
    scan_cache::reply(load_bundle(file))
}

pub fn progress() -> Option<(usize, usize, PathBuf)> {
    loading().clone()
}

pub fn skip_scan() {
    SKIP.store(true, Ordering::Release);
}

pub fn problems() -> Vec<(PathBuf, String)> {
    lock().problems.clone()
}

pub fn scanning() -> bool {
    BACKGROUND.load(Ordering::Acquire)
}

pub fn add_search_dir(dir: &Path) -> usize {
    {
        let mut r = lock();
        if !r.extra_dirs.iter().any(|d| d == dir) {
            r.extra_dirs.push(dir.to_path_buf());
        }
    }
    let found = scan_paths(&[dir.to_path_buf()]);
    let n = found.len();
    // Not scanned yet: the next scan covers the folder along with the others. Meanwhile the
    // plug-ins are remembered, so that creating one by id does not scan every machine folder.
    let mut r = lock();
    if r.scanned.is_none() {
        r.refresh_dirs.insert(dir.to_path_buf());
    }
    let list = match r.scanned.as_mut() {
        Some(list) => list,
        None => &mut r.remembered,
    };
    for d in found {
        if !list.iter().any(|o| o.id == d.id) {
            list.push(d);
        }
    }
    n
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Maps a VST3 unit label to a SoundCraft display unit.
pub(crate) fn unit_from_label(units: &str) -> Unit {
    match units.trim().to_ascii_lowercase().as_str() {
        "db" => Unit::Db,
        "hz" => Unit::Hz,
        "ms" => Unit::Ms,
        "%" => Unit::Percent,
        "s" | "sec" | "secs" | "seconds" => Unit::Seconds,
        "st" | "semi" | "semitones" => Unit::Semitones,
        "ct" | "cent" | "cents" => Unit::Cents,
        _ => Unit::None,
    }
}

fn param_info(inst: &Instance, p: &RawParam) -> ParamInfo {
    let name = if p.title.is_empty() { format!("Param {}", p.id) } else { p.title.clone() };
    let base = ParamInfo {
        id: leak(p.id.to_string()),
        name: leak(name),
        min: 0.0,
        max: 1.0,
        default: 0.0,
        unit: Unit::None,
        taper: Taper::Linear,
        choices: &[],
    };
    if p.step_count == 1 {
        return ParamInfo { unit: Unit::Toggle, default: p.default_normalized.round() as f32, ..base };
    }
    if (2..=MAX_CHOICES).contains(&p.step_count) {
        let steps = f64::from(p.step_count);
        let labels: Vec<&'static str> =
            (0..=p.step_count).map(|i| leak(inst.value_text(p.id, f64::from(i) / steps).unwrap_or_else(|| i.to_string()))).collect();
        return ParamInfo {
            max: p.step_count as f32,
            default: (p.default_normalized * steps).round() as f32,
            unit: Unit::Choice,
            choices: Box::leak(labels.into_boxed_slice()),
            ..base
        };
    }
    if p.step_count > MAX_CHOICES {
        // Too many steps for a menu, but still stepped: the host value is the step index (exact
        // up to f32's 2^24 integers), see `Mapping::Stepped`. The cap is the UI's, not the
        // conversion's.
        let steps = f64::from(p.step_count);
        return ParamInfo {
            max: p.step_count as f32,
            default: (p.default_normalized * steps).round() as f32,
            unit: unit_from_label(&p.units),
            ..base
        };
    }
    // Continuous: plain range from the controller.
    let unit = unit_from_label(&p.units);
    match (inst.to_plain(p.id, 0.0), inst.to_plain(p.id, 1.0)) {
        (Some(lo), Some(hi)) if hi > lo && (lo as f32).is_finite() && (hi as f32).is_finite() && (hi as f32) > (lo as f32) => {
            let default = inst.to_plain(p.id, p.default_normalized).map_or(lo, |d| d.clamp(lo, hi));
            // A logarithmic plugin mapping (frequency knobs) shows its geometric mean at mid travel.
            let taper = match inst.to_plain(p.id, 0.5) {
                Some(mid)
                    if lo > 0.0
                        && ((mid - (lo * hi).sqrt()).abs() <= 0.01 * (lo * hi).sqrt())
                        && (mid - (lo + hi) / 2.0).abs() > 0.01 * (hi - lo) =>
                {
                    Taper::Log
                }
                _ => Taper::Linear,
            };
            ParamInfo { min: lo as f32, max: hi as f32, default: default as f32, unit, taper, ..base }
        }
        _ => ParamInfo { default: p.default_normalized as f32, unit, ..base },
    }
}

/// Builds and leaks the `PluginInfo` for one plugin type. Called at most once per id (cached).
fn build_info(d: &Vst3Descriptor, inst: &Instance) -> &'static PluginInfo {
    let params: Vec<ParamInfo> =
        inst.params().iter().filter(|p| p.flags & (PARAM_HIDDEN | PARAM_READ_ONLY | PARAM_BYPASS) == 0).map(|p| param_info(inst, p)).collect();
    let short: String = d.name.chars().take(8).collect();
    Box::leak(Box::new(PluginInfo {
        id: leak(d.id.clone()),
        name: leak(d.name.clone()),
        short_name: leak(short),
        category: d.category,
        params: Box::leak(params.into_boxed_slice()),
        is_instrument: d.is_instrument,
        audiosuite: false,
    }))
}

/// Finds the descriptor, loads its binary and creates an instance plus its (cached) info.
fn instantiate_raw(id: &str) -> Result<(Instance, &'static PluginInfo), Vst3Error> {
    let cid = parse_id(id).ok_or_else(|| Vst3Error::NotFound(id.to_string()))?;
    if let Some(error) = lock().failed.get(id) {
        return Err(error.clone());
    }
    let known = {
        let r = lock();
        r.scanned.iter().flatten().chain(&r.remembered).find(|d| d.id == id).cloned()
    };
    let desc = match known {
        Some(d) => d,
        None => scan().into_iter().find(|d| d.id == id).ok_or_else(|| Vst3Error::NotFound(id.to_string()))?,
    };
    let made = Bundle::load(&desc.path).and_then(|b| b.instantiate(&cid));
    let mut r = lock();
    match made {
        Ok(inst) => {
            let info = match r.infos.get(id) {
                Some(i) => *i,
                None => {
                    let i = build_info(&desc, &inst);
                    r.infos.insert(id.to_string(), i);
                    i
                }
            };
            Ok((inst, info))
        }
        Err(e) => {
            r.failed.insert(id.to_string(), e.clone());
            Err(e)
        }
    }
}

pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Vst3Error> {
    Ok(Box::new(instantiate_plugin(id)?))
}

pub fn instantiate_plugin(id: &str) -> Result<Vst3Plugin, Vst3Error> {
    let (inst, info) = instantiate_raw(id)?;
    let mut p = Vst3Plugin::new(inst, info);
    p.prepare(48_000.0, 1024, 2);
    // An offline render never runs an editor handle's idle, which is what samples the curves.
    if crate::offline_active() {
        p.sample_all_curves_now();
    }
    Ok(p)
}

/// The description of `id` if it is already cached, without instantiating anything (never loads
/// a plug-in binary, so it is safe on the UI thread). `None` for a type not loaded yet.
pub fn cached_info(id: &str) -> Option<&'static PluginInfo> {
    lock().infos.get(id).copied()
}

pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    parse_id(id)?;
    {
        let r = lock();
        if let Some(i) = r.infos.get(id) {
            return Some(*i);
        }
        if r.failed.contains_key(id) {
            return None;
        }
    }
    // Instantiate once to learn the parameters (VST3 exposes them only on a live controller).
    instantiate_raw(id).ok().map(|(_, info)| info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_instantiation_retains_the_original_failure() {
        let id = "vst3:12121212121212121212121212121212";
        let error = Vst3Error::Instantiate(id.into(), "fixture initialization was refused".into());
        lock().failed.insert(id.into(), error.clone());
        let again = instantiate_raw(id).err();
        lock().failed.remove(id);
        assert_eq!(again, Some(error));
    }

    #[test]
    fn unit_labels_map() {
        assert_eq!(unit_from_label("dB"), Unit::Db);
        assert_eq!(unit_from_label(" Hz "), Unit::Hz);
        assert_eq!(unit_from_label("%"), Unit::Percent);
        assert_eq!(unit_from_label("ms"), Unit::Ms);
        assert_eq!(unit_from_label("sec"), Unit::Seconds);
        assert_eq!(unit_from_label("cents"), Unit::Cents);
        assert_eq!(unit_from_label("st"), Unit::Semitones);
        assert_eq!(unit_from_label("furlongs"), Unit::None);
    }
}
