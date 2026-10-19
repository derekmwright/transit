//! Deterministic scan/publication races.
mod common;

use soundcraft_clap_host as host;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

static SERIAL: Mutex<()> = Mutex::new(());
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Test {
    _serial: MutexGuard<'static, ()>,
    dir: PathBuf,
    fixture: PathBuf,
}

impl Test {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = common::fixture_dir().to_path_buf();
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("clap-registry-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        host::set_scan_extra_dirs_only(true);
        host::set_scan_hook(None);
        host::set_extra_dirs(Vec::new());
        host::set_scanner(None);
        host::set_cache_file(Some(dir.join("cache.json")));
        Self { _serial: serial, dir, fixture }
    }

    fn cache(&self) -> Vec<serde_json::Value> {
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(self.dir.join("cache.json")).unwrap()).unwrap();
        json["files"].as_array().unwrap().clone()
    }

    fn assert_fixture(&self, list: &[host::ClapDescriptor]) {
        let wanted = host::scan_paths(std::slice::from_ref(&self.fixture));
        assert_eq!(wanted.len(), 2);
        for d in wanted {
            assert!(list.iter().any(|p| p.id == d.id), "missing {}", d.id);
        }
    }
}

impl Drop for Test {
    fn drop(&mut self) {
        host::set_scan_hook(None);
        host::set_scan_extra_dirs_only(false);
        host::set_scanner(None);
        host::set_cache_file(None);
        host::set_extra_dirs(Vec::new());
    }
}

fn paused_scan(during: impl FnOnce()) -> Vec<host::ClapDescriptor> {
    let (loaded_tx, loaded_rx) = channel();
    let (go_tx, go_rx) = channel();
    let go_rx = Mutex::new(go_rx);
    host::set_scan_hook(Some(Arc::new(move || {
        loaded_tx.send(()).unwrap();
        go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60)).unwrap();
    })));
    // Release the worker even if an assertion in the caller fails.
    struct Resume(std::sync::mpsc::Sender<()>);
    impl Drop for Resume {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let resume = Resume(go_tx);
    let scanner = std::thread::spawn(host::rescan);
    loaded_rx.recv_timeout(Duration::from_secs(60)).expect("scan reached publication hook");
    host::set_scan_hook(None);
    during();
    drop(resume);
    scanner.join().unwrap()
}

#[test]
fn a_folder_added_during_a_scan_is_in_the_scan_that_publishes_after_it() {
    let t = Test::new();
    let published = paused_scan(|| {
        assert_eq!(host::add_search_dir(&t.fixture), 2);
        assert!(host::cached().is_none(), "registry reads do not wait for publication");
    });
    t.assert_fixture(&published);
    t.assert_fixture(&host::scan());
}

#[test]
fn late_folder_uses_configured_child_scanner_and_records_failure() {
    let t = Test::new();
    host::set_scanner(Some(t.dir.join("unavailable-scanner")));
    let published = paused_scan(|| host::set_extra_dirs(vec![t.fixture.clone()]));
    assert!(published.is_empty(), "late files bypassed the configured child scanner");
    let problems = host::problems();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].1.contains("scan process did not start"), "{problems:?}");
    let files = t.cache();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["error"].as_str(), Some(problems[0].1.as_str()));
}

#[test]
fn skip_before_late_round_leaves_files_unloaded_and_uncached() {
    let t = Test::new();
    let published = paused_scan(|| {
        host::set_extra_dirs(vec![t.fixture.clone()]);
        host::skip_scan();
    });
    assert!(published.is_empty(), "Skip must apply to late files");
    assert!(t.cache().is_empty(), "skipped files must be retried");
    assert!(host::problems().is_empty(), "Skip is not a load failure");
    assert!(host::progress().is_none());
    t.assert_fixture(&host::rescan());
}

#[test]
fn late_plugins_join_the_main_scan_cache() {
    let t = Test::new();
    let initial = t.dir.join("initial.clap");
    std::fs::write(&initial, b"not a plugin").unwrap();
    host::set_extra_dirs(vec![initial.clone()]);
    let published = paused_scan(|| host::set_extra_dirs(vec![initial.clone(), t.fixture.clone()]));
    t.assert_fixture(&published);
    let files = t.cache();
    assert_eq!(files.len(), 2, "both initial and late results are persisted");
    let ids: Vec<_> = files.iter().flat_map(|f| f["plugins"].as_array().unwrap()).map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    assert!(published.iter().all(|d| ids.contains(&d.id.as_str())));
    assert!(files.iter().any(|f| f["error"].is_string()), "initial failure is retained");
}

#[test]
fn late_malformed_files_join_problems_and_cache() {
    let t = Test::new();
    let bad = t.dir.join("bad.clap");
    std::fs::write(&bad, b"not a plugin").unwrap();
    assert!(paused_scan(|| host::set_extra_dirs(vec![bad.clone()])).is_empty());
    let problems = host::problems();
    assert_eq!(problems.len(), 1, "late failures must be reported");
    assert_eq!(problems[0].0, bad);
    let files = t.cache();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["error"].as_str(), Some(problems[0].1.as_str()));
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
fn installing_into_an_already_snapshotted_folder_is_published() {
    let t = Test::new();
    let installed = t.dir.join("install");
    std::fs::create_dir_all(&installed).unwrap();
    host::set_extra_dirs(vec![installed.clone()]);
    let published = paused_scan(|| {
        copy_tree(&t.fixture, &installed);
        assert_eq!(host::add_search_dir(&installed), 2);
    });
    t.assert_fixture(&published);
    t.assert_fixture(&host::cached().unwrap());
    assert_eq!(t.cache().iter().flat_map(|f| f["plugins"].as_array().unwrap()).count(), 2);
}

#[test]
fn refreshing_an_overlapping_folder_preserves_the_first_id_winner() {
    let t = Test::new();
    let first = t.dir.join("a");
    let second = t.dir.join("b");
    copy_tree(&t.fixture, &first);
    copy_tree(&t.fixture, &second);
    host::set_extra_dirs(vec![first.clone(), second]);
    let published = paused_scan(|| {
        assert_eq!(host::add_search_dir(&first), 2);
    });
    t.assert_fixture(&published);
    assert!(published.iter().all(|d| d.path.starts_with(&first)), "refresh reordered duplicate IDs");
    assert_eq!(t.cache().len(), 2, "one cache row per file");
}

#[test]
fn continuous_additions_publish_after_four_late_rounds_and_defer_the_rest() {
    let t = Test::new();
    let dirs: Vec<_> = (0..8)
        .map(|i| {
            let dir = t.dir.join(format!("late-{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("bad.clap"), b"not a plugin").unwrap();
            dir
        })
        .collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let additions = dirs.clone();
    host::set_scan_hook(Some(Arc::new(move || {
        let i = count.fetch_add(1, Ordering::Relaxed);
        if let Some(dir) = additions.get(i) {
            let mut extra = host::search_dirs().1;
            extra.push(dir.clone());
            host::set_extra_dirs(extra);
        }
    })));
    assert!(host::rescan().is_empty());
    host::set_scan_hook(None);
    assert_eq!(calls.load(Ordering::Relaxed), 5, "one initial round plus four late rounds");
    assert_eq!(host::problems().len(), 4);
    assert_eq!(t.cache().len(), 4);
    assert!(host::search_dirs().1.contains(&dirs[4]), "deferred folder stays registered");
    assert!(host::rescan().is_empty());
    assert_eq!(host::problems().len(), 5, "the next rescan covers the deferred folder");
    assert_eq!(t.cache().len(), 5);
}
