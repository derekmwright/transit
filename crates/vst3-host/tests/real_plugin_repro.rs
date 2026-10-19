//! Repro harness against an installed (licensed) VST3 plug-in, on the process main thread
//! (`harness = false`, since editors need it). Skipped unless `SOUNDCRAFT_VST3_REPRO` names the
//! `.vst3` bundle, e.g.
//! `SOUNDCRAFT_VST3_REPRO="C:\Program Files\Common Files\VST3\iZotope\Ozone 11 Equalizer.vst3"`.
//! Mirrors what the app does when a user inserts the plug-in: instantiate, prepare at 48 kHz
//! stereo / 1024 frames, process, save and restore state, open the editor (attached to its host
//! window) and idle it for a few seconds while servicing requests and processing, close, drop,
//! shut the host down. No window messages are pumped (that needs a native message loop, which
//! is the app's). `SOUNDCRAFT_VST3_REPRO_NO_EDITOR=1` skips the editor;
//! `SOUNDCRAFT_VST3_REPRO_SECS` sets the idle time (default 5).
//!
//! Kept as the repro for the iZotope Ozone 11 crash on insert: the plug-in's component, when
//! connected to its controller, dereferenced the `IMessage` our `IHostApplication::createInstance`
//! did not make (access violation in `Ozone 11 Equalizer.vst3` at offset 0x278c6).

use soundcraft_dsp::Plugin;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn step(s: &str) {
    eprintln!("[repro] {s}");
    let _ = std::io::stderr().flush();
}

fn main() {
    let Some(path) = std::env::var_os("SOUNDCRAFT_VST3_REPRO").map(PathBuf::from) else {
        println!("real_plugin_repro: skipped (set SOUNDCRAFT_VST3_REPRO to a .vst3 bundle)");
        return;
    };
    step(&format!("load_bundle {}", path.display()));
    let ds = soundcraft_vst3_host::load_bundle(&path).expect("load bundle");
    let d = ds.into_iter().next().expect("an audio module class");
    step(&format!("instantiate {} ({})", d.name, d.id));
    let mut p = soundcraft_vst3_host::instantiate_plugin(&d.id).expect("instantiate");
    step("prepare 48000/1024/2");
    p.prepare(48_000.0, 1024, 2);
    step("process");
    for i in 0..64 {
        let mut io = vec![vec![((i as f32) * 0.01).sin() * 0.1; 1024]; 2];
        p.process(&mut io, 1024);
    }
    step("save_state");
    let blob = p.save_state().expect("state");
    step(&format!("load_state ({} bytes)", blob.len()));
    assert!(p.load_state(&blob));
    step("editor_size");
    eprintln!("[repro]   size {:?}", p.editor_size());
    let mut ed = p.editor().expect("editor handle");
    let secs: u64 = std::env::var("SOUNDCRAFT_VST3_REPRO_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    if std::env::var_os("SOUNDCRAFT_VST3_REPRO_NO_EDITOR").is_none() {
        step("editor open");
        ed.open().expect("open");
        step("idle");
        let until = Instant::now() + Duration::from_secs(secs);
        let mut n = 0u64;
        while Instant::now() < until {
            let _ = ed.idle();
            let _ = soundcraft_vst3_host::service_requests();
            if n.is_multiple_of(20) {
                let mut io = vec![vec![0.05f32; 1024]; 2];
                p.process(&mut io, 1024);
            }
            n += 1;
            std::thread::sleep(Duration::from_millis(15));
        }
        step("editor close");
        ed.close();
    }
    step("drop");
    drop(ed);
    drop(p);
    soundcraft_vst3_host::shutdown();
    step("done");
    println!("real_plugin_repro: ok");
}
