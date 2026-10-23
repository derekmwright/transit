//! ADR 0018: the plug-in host thread adopts the main-thread role. A module it loads is entered on
//! it, and its own `shutdown()` runs the module exit there, before the process exit tears the
//! module's state down (`harness = false`, see `module_exit.rs`). The fixture ends the process with
//! status 3 if the exit runs on another thread than the entry, or after the teardown; the case
//! runs in a child process with the fixture's late state on (`VST3_FIXTURE_LATE_STATE`).

mod common;

use soundcraft_dsp::Plugin;
use std::process::Command;

fn child() {
    // A thread that is not named `main` and outlives the shutdown, like the host thread.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let host = std::thread::Builder::new()
        .name("plugin-host".into())
        .spawn(move || {
            soundcraft_vst3_host::adopt_main_thread();
            // Built and scanned here: the scan's load is this thread's module entry.
            let dir = common::fixture_dir().to_path_buf();
            let gain = soundcraft_vst3_host::scan_paths(&[dir]).into_iter().find(|d| d.name == "Fixture Gain").expect("fixture").id;
            let mut p = soundcraft_vst3_host::instantiate_plugin(&gain).expect("instantiate");
            p.prepare(48_000.0, 64, 2);
            p.process(&mut vec![vec![0.5f32; 64]; 2], 64);
            drop(p);
            soundcraft_vst3_host::shutdown();
            let refused = soundcraft_vst3_host::instantiate_plugin(&gain).is_err();
            done_tx.send(refused).expect("report");
            // Stay alive past the main thread's own shutdown, as the host thread does.
            std::thread::park();
        })
        .expect("spawn");
    assert!(done_rx.recv().expect("host thread"), "loads fail after the host thread's shutdown");
    // The main thread's call finds nothing of its own to exit and must not touch the host's.
    soundcraft_vst3_host::shutdown();
    drop(host);
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("child") {
        return child();
    }
    let out =
        Command::new(std::env::current_exe().expect("test binary")).arg("child").env("VST3_FIXTURE_LATE_STATE", "1").output().expect("run the child");
    assert!(out.status.success(), "host-thread shutdown: {:?}\n{}", out.status, String::from_utf8_lossy(&out.stderr));
    println!("module_exit_host_thread: ok");
}
