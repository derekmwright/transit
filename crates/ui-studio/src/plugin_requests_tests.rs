//! Gate G7 (ADR 0012, audit F08; review R2): the app's frame loop services what hosted plug-ins
//! ask of the host, with no editor open and no audio running.

use crate::{Services, StudioApp};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const REQUESTS: &str = "clap:org.soundcraft.test.requests";

/// Builds soundcraft-clap-host's fixture with its `requests` feature (a GUI-less CLAP effect)
/// once and adds its folder to the CLAP search path.
fn requests_fixture() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp/clap-requests-fixture-target");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let st = Command::new(cargo)
            .args(["build", "--quiet", "--manifest-path"])
            .arg(&manifest)
            .args(["--features", "requests"])
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .expect("run cargo");
        assert!(st.success(), "building the CLAP fixture failed");
        let lib = if cfg!(target_os = "macos") {
            "libclap_fixture.dylib"
        } else if cfg!(windows) {
            "clap_fixture.dll"
        } else {
            "libclap_fixture.so"
        };
        let dir = std::env::temp_dir().join(format!("studio-clap-requests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(target.join("debug").join(lib), dir.join("Requests.clap")).unwrap();
        assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 1);
    });
}

/// One frame of the app's per-frame logic.
fn frame(app: &mut StudioApp, ctx: &egui::Context) {
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| app.logic(ui.ctx()));
    out.textures_delta.clear();
}

#[test]
fn the_frame_loop_restarts_a_gui_less_clap_plugin_with_no_audio_running() {
    requests_fixture();
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let ctx = egui::Context::default();
    // Created on this (the app's) thread, so this is its main thread.
    let mut p = soundcraft_clap_host::create(REQUESTS).expect("requests fixture");
    p.prepare(48_000.0, 64, 2);
    assert!(p.editor().is_none(), "no editor: nothing but the frame loop services it");
    p.process(&mut vec![vec![0.5f32; 64]; 2], 64); // in the processing state
    // Its IO thread asks for a restart (to 32 samples); then no audio callback comes.
    assert!(p.load_state(b"CMD:restart-now:32"));
    assert_eq!(p.latency(), 5);
    let deadline = Instant::now() + Duration::from_secs(10);
    while p.latency() != 32 && Instant::now() < deadline {
        frame(&mut app, &ctx);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(p.latency(), 32, "serviced by the app's frames");
}
