//! K38 (issue #56): a module entry that does not return holds back only loads of its own binary.
//! The fixture's entry blocks on a gate folder (`VST3_FIXTURE_ENTRY_GATE`) while another copy of
//! the binary, at another path, loads on a second plug-in main thread. Before the fix the second
//! load waited for the loaded-module lock the first held across its entry.
//!
//! The case runs in a child process: the gate is an environment variable of the process, and
//! both threads exit their own modules before it ends (the fixture checks the pairing).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

const CHILD: &str = "VST3_ENTRY_GATE_CHILD";

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A plug-in main thread (as the host threads are) that scans `dir`, waits at `barrier` until
/// both loads are over, then exits its own modules.
fn loader(dir: PathBuf, barrier: Arc<Barrier>, done: mpsc::Sender<(usize, Duration)>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        soundcraft_vst3_host::adopt_main_thread();
        let t = Instant::now();
        let found = soundcraft_vst3_host::scan_paths(&[dir]).len();
        let _ = done.send((found, t.elapsed()));
        barrier.wait();
        soundcraft_vst3_host::shutdown();
    })
}

fn child(gate: &Path) {
    let src = common::unregistered_fixture_dir().join("vendor/Fixture.vst3");
    let root = gate.parent().unwrap().to_path_buf();
    let (a, b) = (root.join("a"), root.join("b"));
    copy_dir(&src, &a.join("Fixture.vst3"));
    copy_dir(&src, &b.join("Fixture.vst3"));
    std::fs::write(gate.join("hold"), b"").unwrap();
    // Opens the gate however the case ends, so the first loader always finishes.
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("hold"));
        }
    }
    let release = Release(gate.to_path_buf());
    let barrier = Arc::new(Barrier::new(2));
    let (a_tx, a_rx) = mpsc::channel();
    let first = loader(a, Arc::clone(&barrier), a_tx);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !gate.join("entered").exists() {
        assert!(Instant::now() < deadline, "the first module entry never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    let (b_tx, b_rx) = mpsc::channel();
    let second = loader(b, Arc::clone(&barrier), b_tx);
    let other = b_rx.recv_timeout(Duration::from_secs(10));
    drop(release);
    let (found, took) = other.expect("the second binary's load waited for the hung module entry of the first");
    assert_eq!(found, 3, "the second copy's plug-ins");
    assert!(a_rx.recv_timeout(Duration::from_secs(30)).is_ok(), "the first load finishes once its entry returns");
    assert!(a_rx.try_recv().is_err());
    first.join().unwrap();
    second.join().unwrap();
    println!("module_entry_hang: second load took {took:?} while the first entry was held");
}

#[test]
fn a_hung_module_entry_does_not_block_another_binarys_load() {
    if let Some(gate) = std::env::var_os("VST3_FIXTURE_ENTRY_GATE").filter(|_| std::env::var_os(CHILD).is_some()) {
        return child(Path::new(&gate));
    }
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vst3-entry-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let gate = root.join("gate");
    std::fs::create_dir_all(&gate).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "a_hung_module_entry_does_not_block_another_binarys_load", "--nocapture"])
        .env(CHILD, "1")
        .env("VST3_FIXTURE_ENTRY_GATE", &gate)
        .output()
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "child: {:?}\n{stdout}\n{stderr}", out.status);
    assert!(stdout.contains("1 passed"), "the child ran no test: {stdout}");
}
