//! Gate G7 (ADR 0012, audit F08) in the mixer: a hosted CLAP effect that sleeps through silence
//! is processed again when it asks (`request_process`), with no audio and no editor, and the
//! latency it reports after a restart reaches delay compensation.
//!
//! Uses soundcraft-clap-host's fixture built with its `requests` feature (from
//! `crates/clap-host/tests/fixture`); the requests plug-in reports its lifecycle log as its
//! "state" (see that crate's `tests/requests.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::PluginStateReader;
use soundcraft_mix::{MixEngine, PreparedInstance, realtime_instance_specs};
use soundcraft_model::{ChannelFormat, Insert, Route, Session, TrackKind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const REQUESTS: &str = "clap:org.soundcraft.test.requests";
const BLOCK: usize = 512;

/// Builds the CLAP fixture's requests plug-in once and adds its folder to the CLAP search path.
fn fixture() {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clap-requests-fixture-target");
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
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mix-clap-requests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if cfg!(target_os = "macos") {
            let exe = dir.join("Requests.clap/Contents/MacOS");
            std::fs::create_dir_all(&exe).unwrap();
            std::fs::copy(target.join("debug").join(lib), exe.join("Requests")).unwrap();
        } else {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(target.join("debug").join(lib), dir.join("Requests.clap")).unwrap();
        }
        assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 1);
        dir
    });
}

/// One silent audio track (no clips) with the requests plug-in as its first insert.
fn session() -> Session {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("FX"));
    let tr = s.track_mut(t).unwrap();
    tr.mixer.inserts[0] = Some(Insert::new(REQUESTS));
    tr.mixer.output = Route::Main;
    s
}

/// A realtime-mode engine holding a requests instance made and prepared here, then given
/// `command` (a fixture `CMD:`), with that instance's state reader.
fn engine(s: &Session, command: &[u8]) -> (MixEngine, Box<dyn PluginStateReader>) {
    fixture();
    let spec = realtime_instance_specs(s).into_iter().find(|x| x.id == REQUESTS).unwrap();
    let mut p = soundcraft_clap_host::create(REQUESTS).unwrap();
    p.prepare(48_000.0, BLOCK, spec.ch);
    assert!(p.load_state(command));
    let reader = p.state_reader().unwrap();
    let mut eng = MixEngine::new(48_000.0, BLOCK);
    eng.set_external_instances(true);
    eng.adopt(vec![PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin: p }]);
    (eng, reader)
}

/// The fixture's report as `key=value` fields and its log of `(call, thread token)`.
fn report(reader: &mut Box<dyn PluginStateReader>) -> (u64, Vec<String>) {
    let text = String::from_utf8(reader.save_state().unwrap()).unwrap();
    let field = |k: &str| text.split(';').find_map(|kv| kv.strip_prefix(k)).unwrap_or_default().to_string();
    let log = field("log=").split(',').filter(|e| !e.is_empty()).map(|e| e.rsplit_once('@').unwrap().0.to_string()).collect();
    (field("processes=").parse().unwrap(), log)
}

#[test]
fn a_sleeping_clap_effect_that_requests_process_is_woken_by_the_mixer() {
    let s = session();
    // The plug-in's IO thread asks to be processed 300 ms from now.
    let (mut eng, mut reader) = engine(&s, b"CMD:process-after:300");
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let mut pos = 0;
    let mut blocks = 0;
    let mut render = |eng: &mut MixEngine| {
        eng.render(&s, pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        blocks += 1;
    };
    render(&mut eng);
    assert_eq!(report(&mut reader).0, 1, "the first block processes it; then it sleeps");
    let deadline = Instant::now() + Duration::from_secs(20);
    while report(&mut reader).0 < 2 && Instant::now() < deadline {
        render(&mut eng);
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..50 {
        render(&mut eng);
    }
    let (processes, log) = report(&mut reader);
    assert_eq!(processes, 2, "silent input, no editor: processed once at the start and once when it asked, over {blocks} blocks");
    let calls: Vec<&str> = log.iter().map(String::as_str).filter(|c| *c == "process" || *c == "request_process").collect();
    assert_eq!(calls, ["process", "request_process", "process"]);
}

#[test]
fn the_latency_after_a_clap_restart_reaches_delay_compensation() {
    let s = session();
    // Asked before it ever processed; after the restart it reports 64 samples.
    let (mut eng, mut reader) = engine(&s, b"CMD:restart-now:64");
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    eng.render(&s, 0, BLOCK, &mut out);
    assert_eq!(eng.latency, 5, "the fixture starts at 5 samples");
    let mut pos = BLOCK as i64;
    for _ in 0..10 {
        soundcraft_clap_host::service_requests();
        eng.render(&s, pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        if eng.latency == 64 {
            break;
        }
    }
    assert_eq!(eng.latency, 64, "the restarted plug-in's latency is compensated");
    let (_, log) = report(&mut reader);
    let tail: Vec<&str> = log.iter().map(String::as_str).skip_while(|c| *c != "stop_processing").collect();
    assert_eq!(&tail[..4], ["stop_processing", "deactivate", "activate", "latency.changed"], "{log:?}");
}
