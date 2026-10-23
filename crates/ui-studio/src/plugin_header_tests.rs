//! Issue #68: the hosted plug-in window's header strip, through the real Player and plug-in host
//! thread with the VST3 test fixture's gain as an insert: Save As writes a `.vstpreset` to the
//! settings folder, the dropdown lists it, picking it loads it into the live instance as one undo
//! step, undo gives the plug-in its earlier state back, previous/next step through the presets,
//! Copy/Paste carries a state to another instance, and the strip is disabled while an operation
//! runs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::{Services, StudioApp};
use soundcraft_dsp::{HeaderAction, JobPoll, StateJob};
use soundcraft_model::TrackId;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Builds the VST3 fixture, installs it as a bundle and adds its folder to the search path;
/// returns the gain's id.
fn fixture_gain() -> String {
    soundcraft_vst3_host::set_scan_extra_dirs_only(true);
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../vst3-host/tests/fixture/Cargo.toml");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
        .join("tmp/studio-vst3-header-fixture-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .env_remove("RUSTFLAGS")
        .status()
        .expect("run cargo");
    assert!(st.success(), "building the VST3 fixture failed");
    let (lib, folder, exe) = if cfg!(target_os = "macos") {
        ("libvst3_fixture.dylib", "MacOS", "Fixture")
    } else if cfg!(windows) {
        ("vst3_fixture.dll", "x86_64-win", "Fixture.vst3")
    } else {
        ("libvst3_fixture.so", if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, "Fixture.so")
    };
    let dir = std::env::temp_dir().join(format!("studio-vst3-header-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("Fixture.vst3/Contents").join(folder);
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(target.join("debug").join(lib), bin.join(exe)).unwrap();
    assert_eq!(soundcraft_vst3_host::add_search_dir(&dir), 3, "the fixture's plug-ins");
    soundcraft_vst3_host::scan_paths(&[dir]).into_iter().find(|d| d.name == "Fixture Gain").expect("the fixture's gain").id
}

/// One frame of the app, with the device-free player's "audio callback" first.
fn frame(app: &mut StudioApp, ctx: &egui::Context) {
    app.player.as_ref().unwrap().step_device_free();
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| app.logic(ui.ctx()));
    out.textures_delta.clear();
}

/// Frames until `done`, at most 20 s.
fn frames_until(app: &mut StudioApp, ctx: &egui::Context, what: &str, mut done: impl FnMut(&mut StudioApp) -> bool) {
    let start = Instant::now();
    while !done(app) {
        assert!(start.elapsed() < Duration::from_secs(20), "{what}: never happened (status {:?})", app.status);
        frame(app, ctx);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn finish<T>(app: &mut StudioApp, ctx: &egui::Context, mut job: StateJob<T>) -> T {
    let start = Instant::now();
    loop {
        match job.poll() {
            JobPoll::Ready(v) => return v,
            JobPoll::Failed(e) => panic!("state job failed: {e}"),
            JobPoll::Waiting => {
                assert!(start.elapsed() < Duration::from_secs(20), "state job never finished");
                frame(app, ctx);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// The fixture gain's state: component gain (normalized) and controller frequency.
fn state(gain: f64) -> Vec<u8> {
    soundcraft_vst3_host::preset::join_state(&gain.to_le_bytes(), &0.5f64.to_le_bytes()).unwrap()
}

fn live(app: &mut StudioApp, ctx: &egui::Context, t: TrackId, slot: usize) -> Vec<u8> {
    let job = app.player.as_ref().unwrap().begin_save_state(t, slot).unwrap();
    finish(app, ctx, job).expect("a state")
}

fn set_live(app: &mut StudioApp, ctx: &egui::Context, t: TrackId, slot: usize, data: Vec<u8>) {
    let job = app.player.as_ref().unwrap().begin_load_state(t, slot, data).unwrap();
    assert!(finish(app, ctx, job).is_some(), "the fixture takes the state");
}

/// Runs frames until the live state is `want` (the session's parameters reach the plug-in a few
/// blocks after a document change), at most 10 s.
fn live_becomes(app: &mut StudioApp, ctx: &egui::Context, t: TrackId, slot: usize, want: &[u8], what: &str) {
    let start = Instant::now();
    loop {
        let got = live(app, ctx, t, slot);
        if got == want {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{what}: live state {got:?}, want {want:?} (status {:?})", app.status);
        for _ in 0..3 {
            frame(app, ctx);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until no preset operation runs.
fn settle(app: &mut StudioApp, ctx: &egui::Context) {
    frames_until(app, ctx, "the preset operation", |a| !a.plugin_header.any_busy());
}

#[test]
fn header_presets_save_load_undo_step_and_paste_through_the_plug_in_host() {
    // The host registry is process-global: run in a child so no other test sees the fixture.
    if std::env::var_os("TRANSIT_HEADER_FIXTURE_CHILD").is_none() {
        let name = "plugin_header_tests::header_presets_save_load_undo_step_and_paste_through_the_plug_in_host";
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("TRANSIT_HEADER_FIXTURE_CHILD", "1")
            .output()
            .unwrap();
        let (out, err) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "{out}{err}");
        assert!(out.contains("1 passed"), "the child ran no test: {out}");
        return;
    }
    let gain = fixture_gain();
    let config = std::env::temp_dir().join(format!("studio-header-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    let mut engine = soundcraft_engine::Engine::default();
    let t = engine.session_mut().add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Stereo, Some("Drums"));
    let player = soundcraft_playback::Player::prepare_device_free(engine.session_arc()).unwrap();
    // Save As picks a name the way the dialog would: "My Kit" in the folder offered.
    // What the dialogs answer: Save As's file name, and the confirmations (and what they asked).
    let name = std::sync::Arc::new(std::sync::Mutex::new("My Kit.vstpreset".to_string()));
    let yes = std::sync::Arc::new(std::sync::Mutex::new(false));
    let questions = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (picked, answer, asked) = (std::sync::Arc::clone(&name), std::sync::Arc::clone(&yes), std::sync::Arc::clone(&questions));
    let services = Services {
        config_dir: Some(config.clone()),
        pick_save: Some(Box::new(move |default: &str| {
            let p = Path::new(default);
            Some(p.with_file_name(picked.lock().unwrap().as_str()).to_string_lossy().into_owned())
        })),
        confirm: Some(Box::new(move |question: &str| {
            asked.lock().unwrap().push(question.to_owned());
            *answer.lock().unwrap()
        })),
        ..Services::default()
    };
    let mut app = StudioApp::new(engine, Some(player), services);
    let ctx = egui::Context::default();
    for slot in 0..2 {
        app.run("mix.insert", serde_json::json!({"track": t.0, "plugin": gain, "slot": slot})).unwrap();
    }
    frames_until(&mut app, &ctx, "both instances live", |a| {
        let p = a.player.as_ref().unwrap();
        (0..2).all(|s| p.begin_save_state(t, s).is_ok())
    });
    // The fixture's "GUI" makes an edit as it is set up (gain 0.9 normalized); once the session
    // has recorded it, the session stores the gain, and a preset load must update that value too
    // (or the session would push the old one over the preset).
    frames_until(&mut app, &ctx, "the fixture's own edit recorded", |a| {
        a.engine.session().tracks[0].mixer.inserts.iter().take(2).all(|i| i.as_ref().is_some_and(|i| i.params.contains_key("7")))
    });

    let header = app.editor_header((t, 0)).expect("a header for the hosted insert");
    assert_eq!(header.title, "Drums \u{b7} Insert A \u{b7} Fixture Gain");
    assert!(header.user.is_empty() && header.preset.is_empty() && header.can_bypass && !header.can_paste);
    assert_eq!(header.disabled, None);

    // Save As: the live state, read on the host thread, as a .vstpreset in the settings folder.
    set_live(&mut app, &ctx, t, 0, state(0.75));
    let undo_before = app.engine.undo_history().len();
    app.header_action(t, 0, HeaderAction::SaveAs);
    assert_eq!(app.editor_header((t, 0)).unwrap().disabled.as_deref(), Some("Saving preset\u{2026}"), "disabled while it runs");
    settle(&mut app, &ctx);
    let file = soundcraft_engine::presets::path(&config, &gain, "My Kit").unwrap();
    assert_eq!(&std::fs::read(&file).unwrap()[..4], b"VST3", "{}", app.status);
    assert_eq!(app.engine.undo_history().len(), undo_before, "a save is not undoable");
    let header = app.editor_header((t, 0)).unwrap();
    assert_eq!((header.preset.as_str(), header.user.clone(), header.can_delete), ("My Kit", vec!["My Kit".to_string()], true));

    // The plug-in moves on; picking "My Kit" in the dropdown loads it live, as one undo step.
    set_live(&mut app, &ctx, t, 0, state(0.25));
    app.header_action(t, 0, HeaderAction::User(0));
    assert_eq!(app.editor_header((t, 0)).unwrap().disabled.as_deref(), Some("Loading preset\u{2026}"));
    settle(&mut app, &ctx);
    assert_eq!(live(&mut app, &ctx, t, 0), state(0.75), "the preset is live: {}", app.status);
    assert_eq!(app.engine.undo_label(), Some("Load Preset"));
    let ins = app.engine.session().tracks[0].mixer.inserts[0].clone().unwrap();
    assert_eq!((ins.preset.as_str(), ins.state_bytes().unwrap()), ("My Kit", state(0.75)), "the session holds it too");
    let stored = ins.params["7"];
    assert!((stored - 1.5).abs() < 1e-3, "the stored gain is the preset's (0.75 of 0..2), not the old edit's: {stored}");
    // More blocks: the session's parameters do not undo the preset.
    for _ in 0..10 {
        frame(&mut app, &ctx);
    }
    assert_eq!(live(&mut app, &ctx, t, 0), state(0.75), "still the preset after the session's parameters were applied");

    // Undo: the session and the live plug-in go back to the state before the load.
    app.run("edit.undo", serde_json::json!({})).unwrap();
    frame(&mut app, &ctx);
    settle(&mut app, &ctx);
    live_becomes(&mut app, &ctx, t, 0, &state(0.25), "undo gives the plug-in its earlier state back");
    let stored = app.engine.session().tracks[0].mixer.inserts[0].as_ref().unwrap().params["7"];
    assert!((stored - 0.5).abs() < 1e-3, "the session stores the restored state's gain, not the step's stale value: {stored}");
    app.run("edit.redo", serde_json::json!({})).unwrap();
    frame(&mut app, &ctx);
    settle(&mut app, &ctx);
    live_becomes(&mut app, &ctx, t, 0, &state(0.75), "redo loads the preset again");

    // Previous/next step through the user presets in name order.
    set_live(&mut app, &ctx, t, 0, state(0.5));
    soundcraft_engine::io::presets::write(&soundcraft_engine::presets::path(&config, &gain, "A Kit").unwrap(), &gain, &state(0.5)).unwrap();
    app.plugin_header = crate::plugin_header::HeaderState::default(); // forget the listing
    assert_eq!(app.editor_header((t, 0)).unwrap().user, vec!["A Kit".to_string(), "My Kit".to_string()]);
    app.header_action(t, 0, HeaderAction::Previous);
    settle(&mut app, &ctx);
    assert_eq!(app.engine.session().tracks[0].mixer.inserts[0].as_ref().unwrap().preset, "A Kit");
    assert_eq!(live(&mut app, &ctx, t, 0), state(0.5));
    app.header_action(t, 0, HeaderAction::Next);
    settle(&mut app, &ctx);
    assert_eq!(app.engine.session().tracks[0].mixer.inserts[0].as_ref().unwrap().preset, "My Kit");
    assert_eq!(live(&mut app, &ctx, t, 0), state(0.75));

    // Copy here, Paste into the other instance of the same plug-in.
    app.header_action(t, 0, HeaderAction::Copy);
    settle(&mut app, &ctx);
    assert!(app.editor_header((t, 1)).unwrap().can_paste);
    app.header_action(t, 1, HeaderAction::Paste);
    settle(&mut app, &ctx);
    assert_eq!(live(&mut app, &ctx, t, 1), state(0.75), "pasted: {}", app.status);
    assert_eq!(app.engine.session().tracks[0].mixer.inserts[1].as_ref().unwrap().preset, "My Kit");

    // A load whose slot is given another instance meanwhile: the result is dropped, not stored on
    // the new insert (review note 8).
    set_live(&mut app, &ctx, t, 1, state(0.25));
    let mine = app.editor_header((t, 1)).unwrap().user.iter().position(|n| n == "My Kit").unwrap();
    app.header_action(t, 1, HeaderAction::User(mine));
    app.run("mix.insert_remove", serde_json::json!({"track": t.0, "slot": 1})).unwrap();
    app.run("mix.insert", serde_json::json!({"track": t.0, "plugin": gain, "slot": 1})).unwrap();
    let steps = app.engine.undo_history().len();
    settle(&mut app, &ctx);
    assert_eq!(app.status, "The plug-in in that slot changed while its preset loaded; nothing was stored");
    assert_eq!(app.engine.undo_history().len(), steps, "no load step for the new insert");
    let ins = app.engine.session().tracks[0].mixer.inserts[1].clone().unwrap();
    assert_ne!(ins.preset, "My Kit", "the new insert was not given the old one's preset");
    assert!(ins.state_bytes().is_none_or(|s| s != state(0.75)));

    // Save As to a name the dialog showed without the extension: an existing preset under the
    // extended name is not replaced; the save takes a numbered name (review note 10).
    *name.lock().unwrap() = "My Kit".into();
    let kit = std::fs::read(&file).unwrap();
    app.header_action(t, 0, HeaderAction::SaveAs);
    settle(&mut app, &ctx);
    assert_eq!(std::fs::read(&file).unwrap(), kit, "My Kit untouched");
    let second = soundcraft_engine::presets::path(&config, &gain, "My Kit 2").unwrap();
    assert!(second.exists(), "saved as My Kit 2: {}", app.status);
    std::fs::remove_file(&second).unwrap();
    app.plugin_header = crate::plugin_header::HeaderState::default();
    app.header_action(t, 0, HeaderAction::User(mine));
    settle(&mut app, &ctx);

    // Bypass from the strip; Delete asks first, and only a yes removes the preset file.
    app.header_action(t, 0, HeaderAction::Bypass);
    assert!(app.engine.session().tracks[0].mixer.inserts[0].as_ref().unwrap().bypass);
    assert!(app.editor_header((t, 0)).unwrap().bypassed);
    app.header_action(t, 0, HeaderAction::Delete);
    assert!(file.exists(), "answered no: kept");
    assert!(questions.lock().unwrap()[0].contains("My Kit"), "{:?}", questions.lock().unwrap());
    *yes.lock().unwrap() = true;
    app.header_action(t, 0, HeaderAction::Delete);
    assert!(!file.exists());
    let _ = std::fs::remove_dir_all(&config);
}
