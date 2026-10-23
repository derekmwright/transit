//! Plugin editors must be driven from the process main thread, which the standard test harness
//! never uses, so this test binary has its own `main` (`harness = false`). The fixture's GUI is
//! window-less: nothing appears on screen.

mod common;

use soundcraft_dsp::Plugin;

const GAIN: &str = "clap:org.soundcraft.test.gain";

fn block(p: &mut Box<dyn Plugin>) -> Vec<Vec<f32>> {
    let mut io = vec![vec![1.0f32; 64]; 2];
    p.process(&mut io, 64);
    io
}

fn main() {
    common::fixture_dir();
    let mut p = soundcraft_clap_host::create(GAIN).expect("fixture");
    p.prepare(48_000.0, 64, 2);
    let mut ed = p.editor().expect("editor handle");
    assert!(!ed.is_open());
    ed.open().expect("open on the main thread");
    assert!(ed.is_open());
    ed.open().expect("opening again just raises it");
    // The fixture "edits" the gain in its GUI; the edit arrives as an output event of the next
    // block and is reported (in SoundCraft units) by idle.
    let io = block(&mut p);
    assert!((io[0][10] - 0.25).abs() < 1e-6, "{}", io[0][10]);
    assert_eq!(ed.idle(), vec![("7".to_string(), 0.25)]);
    assert!(ed.idle().is_empty(), "drained");
    ed.close();
    assert!(!ed.is_open());
    // Through the Plugin trait as well.
    p.open_editor().expect("open via the trait");
    assert!(ed.is_open(), "the handle sees the same editor");
    p.close_editor();
    assert!(!ed.is_open());
    // Dropping the instance with its editor open destroys the GUI first (the fixture asserts
    // that); the handle then refuses politely.
    ed.open().expect("reopen");
    drop(p);
    assert!(!ed.is_open());
    assert!(ed.open().is_err());
    assert!(ed.idle().is_empty());
    ed.close();

    // Issue #68: embedded first where this host has its own window (with the header strip);
    // a plug-in whose embedded editor fails still gets its floating one.
    let mut p = soundcraft_clap_host::create(GAIN).expect("fixture");
    p.prepare(48_000.0, 64, 2);
    assert!(p.load_state(b"CMD:gui-embedded-fails"));
    let mut ed = p.editor().expect("editor handle");
    ed.open().expect("the embedded editor fails: the floating one opens");
    assert!(ed.is_open());
    assert!(p.load_state(b"CMD:gui-was-floating"), "the fallback is the floating editor");
    ed.close();
    drop((ed, p));
    let mut p = soundcraft_clap_host::create(GAIN).expect("fixture");
    p.prepare(48_000.0, 64, 2);
    assert!(p.load_state(b"CMD:gui-embedded"));
    let mut ed = p.editor().expect("editor handle");
    ed.open().expect("the embedded editor opens");
    assert!(ed.is_open());
    let _ = ed.idle(); // the fixture's resize request reaches the window
    assert!(ed.is_open());
    assert_eq!(p.load_state(b"CMD:gui-was-floating"), !cfg!(windows), "embedded on Windows, floating elsewhere");
    ed.close();
    assert!(!ed.is_open());
    drop((ed, p));
    println!("editor_main_thread: ok");
}
