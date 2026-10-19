//! Gate G7 (ADR 0012, audit F08), VST3 side: `IComponentHandler::restartComponent` is serviced on
//! the main thread. `kMidiCCAssignmentChanged` re-reads the `IMidiMapping` table before the next
//! controller is routed; `kLatencyChanged` (and `kIoChanged`) deactivate and reactivate the
//! component on the main thread and report the new latency; `kReloadComponent` is refused clearly.
//!
//! The fixture synth takes commands as a component state and reports, as its component state,
//! the OS thread of each call (see `tests/fixture`). The thread that creates an instance is its
//! main (UI) thread here; the report's `me=` is the thread reading it, so each test compares the
//! threads of main-thread calls with its own, as recorded identities, not thread names.

mod common;

use soundcraft_dsp::{Plugin, PluginStateReader};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BLOCK: usize = 64;

fn synth() -> (Box<dyn Plugin>, Box<dyn PluginStateReader>) {
    let found = soundcraft_vst3_host::scan_paths(&[common::fixture_dir().to_path_buf()]);
    let id = found.into_iter().find(|d| d.name == "Fixture Synth").expect("fixture synth").id;
    let mut p = soundcraft_vst3_host::create(&id).expect("instantiate");
    p.prepare(48_000.0, BLOCK, 2);
    let reader = p.state_reader().expect("state reader");
    (p, reader)
}

/// The synth's report: `me=` and the threads of each call.
struct Report(String);

impl Report {
    fn read(reader: &mut Box<dyn PluginStateReader>) -> Report {
        let blob = reader.save_state().expect("state");
        Report(String::from_utf8_lossy(&blob).into_owned())
    }

    fn field(&self, key: &str) -> String {
        let at = self.0.find(&format!("{key}=")).unwrap_or_else(|| panic!("{key} in {}", self.0)) + key.len() + 1;
        self.0[at..].chars().take_while(|c| c.is_ascii_digit() || *c == ',').collect()
    }

    fn me(&self) -> usize {
        self.field("me").parse().unwrap()
    }

    /// The OS threads that made `call` (`map`, `active`, `process`, `get_latency`).
    fn threads(&self, call: &str) -> Vec<usize> {
        self.field(call).split(',').filter(|t| !t.is_empty()).map(|t| t.parse().unwrap()).collect()
    }
}

/// One block with a controller at frame 10; the level at the end of the block.
fn cc_block(p: &mut Box<dyn Plugin>, cc: u8, value: u8) -> f32 {
    let mut io = vec![vec![0.0f32; BLOCK]; 2];
    p.midi(10, [0xB0, cc, value]);
    p.process(&mut io, BLOCK);
    io[0][BLOCK - 1]
}

#[test]
fn a_learn_during_a_state_load_routes_the_next_cc_by_the_new_table() {
    let (mut p, mut reader) = synth();
    assert!((cc_block(&mut p, 7, 64) - 64.0 / 127.0).abs() < 1e-6, "CC 7 drives the volume");
    assert_eq!(cc_block(&mut p, 11, 127), 64.0 / 127.0, "CC 11 is not mapped");
    // The plug-in learns CC 11 inside `setState` and calls restartComponent(kMidiCCAssignmentChanged).
    assert!(p.load_state(b"LEARN:11"));
    assert!((cc_block(&mut p, 11, 100) - 100.0 / 127.0).abs() < 1e-6, "the very next CC 11 routes by the new table");
    assert!((cc_block(&mut p, 7, 1) - 100.0 / 127.0).abs() < 1e-6, "CC 7 is no longer mapped");
    let r = Report::read(&mut reader);
    assert!(r.threads("map").iter().all(|&t| t == r.me()), "IMidiMapping only on this thread: {}", r.0);
}

#[test]
fn a_live_learn_is_re_read_on_main_and_routed_from_the_next_cc() {
    let (mut p, mut reader) = synth();
    // At its next getState (on this thread, while the processor runs on the audio thread) the
    // plug-in learns CC 1.
    assert!(p.load_state(b"ARM:LEARN:1"));
    let stop = Arc::new(AtomicBool::new(false));
    let levels = Arc::new(Mutex::new(Vec::new()));
    let (halt, out) = (Arc::clone(&stop), Arc::clone(&levels));
    let audio = std::thread::spawn(move || {
        while !halt.load(Ordering::SeqCst) {
            let level = cc_block(&mut p, 1, 100);
            out.lock().unwrap().push(level);
            std::thread::sleep(Duration::from_millis(1));
        }
        p
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while levels.lock().unwrap().len() < 5 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let learned = Report::read(&mut reader); // runs the learn: restartComponent on this thread
    let mut serviced_at = None;
    while serviced_at.is_none() && Instant::now() < deadline {
        let before = levels.lock().unwrap().len();
        if soundcraft_vst3_host::service_requests().midi_maps > 0 {
            serviced_at = Some(before);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let serviced_at = serviced_at.expect("the mapping change is serviced");
    while levels.lock().unwrap().len() < serviced_at + 10 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::SeqCst);
    let p = audio.join().unwrap();
    let levels = levels.lock().unwrap();
    assert!(levels[..serviced_at].iter().all(|&l| l == 0.0), "CC 1 unmapped until the service: {levels:?}");
    assert!((levels.last().unwrap() - 100.0 / 127.0).abs() < 1e-6, "CC 1 routed by the new table: {levels:?}");
    let r = Report::read(&mut reader);
    let main = learned.me();
    assert_eq!(r.me(), main);
    assert!(!r.threads("process").contains(&main), "the processor ran on the audio thread");
    assert!(r.threads("map").iter().all(|&t| t == main), "IMidiMapping is queried on the main thread only: {}", r.0);
    drop(p);
}

#[test]
fn a_latency_change_reactivates_on_main_and_reports_the_new_latency() {
    let (mut p, mut reader) = synth();
    assert_eq!(p.latency(), 0);
    assert!(p.load_state(b"ARM:LATENCY:32"));
    let stop = Arc::new(AtomicBool::new(false));
    let latency = Arc::new(AtomicUsize::new(0));
    let (halt, lat) = (Arc::clone(&stop), Arc::clone(&latency));
    let audio = std::thread::spawn(move || {
        while !halt.load(Ordering::SeqCst) {
            cc_block(&mut p, 7, 64);
            lat.store(p.latency(), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1));
        }
        p
    });
    std::thread::sleep(Duration::from_millis(20));
    let main = Report::read(&mut reader).me(); // runs the change: restartComponent(kLatencyChanged)
    let deadline = Instant::now() + Duration::from_secs(20);
    while latency.load(Ordering::SeqCst) != 32 && Instant::now() < deadline {
        soundcraft_vst3_host::service_requests();
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(20));
    stop.store(true, Ordering::SeqCst);
    let mut p = audio.join().unwrap();
    assert_eq!(p.latency(), 32, "the latency read after setActive(true) is reported (feeds delay compensation)");
    let r = Report::read(&mut reader);
    assert_eq!(r.threads("active"), vec![main], "setActive on the main thread only: {}", r.0);
    assert_eq!(r.threads("get_latency"), vec![main], "getLatencySamples on the main thread only: {}", r.0);
    assert!(!r.threads("process").contains(&main));
    // It still plays after the restart.
    assert!((cc_block(&mut p, 7, 127) - 1.0).abs() < 1e-6);
}

#[test]
fn io_changes_reactivate_and_a_reload_request_is_refused_clearly() {
    let (mut p, _reader) = synth();
    // kReloadComponent (1 << 0): the component would have to be destroyed and created again.
    assert!(p.load_state(b"RESTART:1"));
    let s = soundcraft_vst3_host::service_requests();
    assert_eq!((s.restarted, s.unsupported), (0, 1));
    // kIoChanged (1 << 1): deactivate, renegotiate the buses, reactivate.
    assert!(p.load_state(b"RESTART:2"));
    let off = std::thread::spawn(soundcraft_vst3_host::service_requests).join().unwrap();
    assert_eq!(off.restarted, 0, "not off the main thread");
    let s = soundcraft_vst3_host::service_requests();
    assert_eq!((s.restarted, s.unsupported), (1, 0));
    assert!((cc_block(&mut p, 7, 127) - 1.0).abs() < 1e-6, "it plays after the reactivation");
}

/// A state for the gain fixture whose controller part also carries a "preset" gain (normalized)
/// that the plug-in returns to at each `getState` (see `tests/fixture`).
fn gain_state_with_preset(preset: f64) -> Vec<u8> {
    let comp = 0.25f64.to_le_bytes();
    let ctrl: Vec<u8> = [0.5f64.to_le_bytes(), preset.to_le_bytes()].concat();
    let mut out = b"SCV3".to_vec();
    out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
    out.extend_from_slice(&comp);
    out.extend_from_slice(&(ctrl.len() as u32).to_le_bytes());
    out.extend_from_slice(&ctrl);
    out
}

/// Editor handles drive windows on the process main thread on macOS (see `editor_main_thread.rs`).
#[cfg(not(target_os = "macos"))]
#[test]
fn values_changed_by_the_plugin_are_re_read_on_main_and_reported_like_edits() {
    let found = soundcraft_vst3_host::scan_paths(&[common::fixture_dir().to_path_buf()]);
    let id = found.into_iter().find(|d| d.name == "Fixture Gain").expect("fixture gain").id;
    let mut p = soundcraft_vst3_host::create(&id).expect("instantiate");
    p.prepare(48_000.0, BLOCK, 2);
    let mut ed = p.editor().expect("editor handle");
    let mut reader = p.state_reader().expect("state reader");
    let _ = ed.idle();
    assert!(p.load_state(&gain_state_with_preset(0.8)));
    assert!(ed.idle().is_empty(), "a state the host loads is not reported back as edits");
    // The plug-in switches its gain to 0.8 (plain 1.6) on its own and calls
    // restartComponent(kParamValuesChanged).
    let _ = reader.save_state();
    let off = std::thread::spawn(soundcraft_vst3_host::service_requests).join().unwrap();
    assert_eq!(off.value_rescans, 0, "not off the main thread");
    let edits = ed.idle();
    assert_eq!(edits.len(), 1, "only the value that changed: {edits:?}");
    assert_eq!(edits[0].0, "7");
    assert!((edits[0].1 - 1.6).abs() < 1e-6, "{edits:?}");
    assert!(ed.idle().is_empty(), "once");
    // Review R3: the host then sets 2.0 (normalized 1.0) and the plug-in goes back to its 0.8.
    // The value read (0.8) is the one read last time, but not what the host believes: reported.
    assert!(p.set_param("7", 2.0));
    p.process(&mut vec![vec![0.5f32; BLOCK]; 2], BLOCK);
    let _ = reader.save_state(); // syncs the host's 2.0 to the controller; it returns to 0.8
    let edits = ed.idle();
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert!((edits[0].1 - 1.6).abs() < 1e-6, "differs from the host's 2.0: {edits:?}");
}

/// Mutant V4 of the review: a state the host loads, whose plug-in reports changed values while
/// loading it, is not reported back as edits (the host already holds those values).
#[cfg(not(target_os = "macos"))]
#[test]
fn values_changed_during_a_host_state_load_are_not_reported() {
    let found = soundcraft_vst3_host::scan_paths(&[common::fixture_dir().to_path_buf()]);
    let id = found.into_iter().find(|d| d.name == "Fixture Gain").expect("fixture gain").id;
    let mut p = soundcraft_vst3_host::create(&id).expect("instantiate");
    p.prepare(48_000.0, BLOCK, 2);
    let mut ed = p.editor().expect("editor handle");
    let _ = ed.idle();
    // Gain 0.7 (component) and Freq 0.3 (controller), both different from before; the controller
    // calls restartComponent(kParamValuesChanged) inside setState.
    let mut blob = b"SCV3".to_vec();
    blob.extend_from_slice(&8u32.to_le_bytes());
    blob.extend_from_slice(&0.7f64.to_le_bytes());
    blob.extend_from_slice(&8u32.to_le_bytes());
    blob.extend_from_slice(&0.3f64.to_le_bytes());
    assert!(p.load_state(&blob));
    assert!(ed.idle().is_empty(), "the host loaded these values itself");
    assert!(ed.idle().is_empty());
}

/// Review R5: a reactivation the plug-in refuses is tried once per request, logged, counted as
/// failed, and not retried every frame.
#[test]
fn a_refused_reactivation_fails_once_and_is_not_retried() {
    let (mut p, _reader) = synth();
    assert!(p.load_state(b"REFUSE"));
    assert!(p.load_state(b"RESTART:8")); // kLatencyChanged
    let s = soundcraft_vst3_host::service_requests();
    assert_eq!((s.restarted, s.failed), (0, 1), "{s:?}");
    for _ in 0..5 {
        let s = soundcraft_vst3_host::service_requests();
        assert_eq!((s.restarted, s.failed, s.restarts_waiting), (0, 0, 0), "not retried: {s:?}");
    }
    assert_eq!(p.latency(), 0);
}

/// Review R1: a block the audio side skips because the main thread holds the instance (here,
/// reactivations for kLatencyChanged over and over) is silence faded from the last output, not
/// the buffer as it was (for an instrument: whatever the strip held, here a stale 0.5).
#[test]
fn a_block_skipped_during_a_reactivation_is_a_fade_to_silence() {
    let (mut p, mut reader) = synth();
    assert!(p.load_state(b"EVERY:RESTART:8"));
    let stop = Arc::new(AtomicBool::new(false));
    let skipped = Arc::new(AtomicUsize::new(0));
    let (halt, skips) = (Arc::clone(&stop), Arc::clone(&skipped));
    let audio = std::thread::spawn(move || {
        p.note_on(0, 60, 127);
        let mut last = 0.0f32;
        let mut bad = None;
        while !halt.load(Ordering::SeqCst) {
            let mut io = vec![vec![0.5f32; BLOCK]; 2];
            p.process(&mut io, BLOCK);
            let block = &io[0];
            if block.iter().all(|&x| x == 1.0) {
                last = 1.0;
                continue;
            }
            skips.fetch_add(1, Ordering::SeqCst);
            // A fade out (never above where it was, never the stale 0.5 held flat), silence, or
            // a fade in (from silence, or continuing one from the block before) back to the note.
            let flat_stale = block.iter().all(|&x| x == 0.5);
            let out_ok = block.windows(2).all(|w| w[1] <= w[0]) && block[0] <= last + 1e-6;
            let in_ok = block.windows(2).all(|w| w[1] >= w[0]) && (block[0] <= 0.02 || block[0] >= last - 1e-6);
            if flat_stale || !(out_ok || in_ok) {
                bad.get_or_insert(block.clone());
            }
            last = block[BLOCK - 1];
        }
        (p, bad)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while skipped.load(Ordering::SeqCst) < 5 && Instant::now() < deadline {
        let _ = Report::read(&mut reader); // restartComponent(kLatencyChanged) on this thread
        soundcraft_vst3_host::service_requests(); // reactivates while holding the instance
    }
    stop.store(true, Ordering::SeqCst);
    let (_p, bad) = audio.join().unwrap();
    assert!(skipped.load(Ordering::SeqCst) >= 5, "the audio side met the main thread at work");
    assert_eq!(bad, None, "a skipped block passed something other than a fade");
}
