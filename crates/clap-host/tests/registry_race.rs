//! A folder added while a scan is loading must not be lost when that scan publishes its list.

mod common;

use soundcraft_clap_host as host;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn a_folder_added_during_a_scan_is_in_the_scan_that_publishes_after_it() {
    let dir = common::fixture_dir();
    // The fixture helper registered the folder; start from none, as if it came later.
    host::set_extra_dirs(Vec::new());
    let wanted: Vec<String> = host::scan_paths(&[dir.to_path_buf()]).into_iter().map(|d| d.id).collect();
    assert_eq!(wanted.len(), 2);
    let (loaded_tx, loaded_rx) = channel::<()>();
    let (go_tx, go_rx) = channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let loaded_tx = Mutex::new(loaded_tx);
    host::set_scan_hook(Some(Arc::new(move || {
        let _ = loaded_tx.lock().unwrap().send(());
        let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60));
    })));
    let scanner = std::thread::spawn(host::scan);
    // The scan has taken its folders and loaded its files; it has not published yet.
    loaded_rx.recv_timeout(Duration::from_secs(60)).expect("the scan reached the hook");
    host::set_scan_hook(None);
    assert_eq!(host::add_search_dir(dir), 2);
    go_tx.send(()).unwrap();
    let published = scanner.join().unwrap();
    let missing = |list: &[host::ClapDescriptor]| wanted.iter().filter(|id| !list.iter().any(|d| &d.id == *id)).count();
    assert_eq!(missing(&published), 0, "the scan published a list without the folder added during it");
    assert_eq!(missing(&host::scan()), 0, "a later scan() returns the stale cache");
}
