//! Issue #68: a user preset is the plug-in's full state, read and loaded through the plug-in host
//! thread while the instance is live, and written as the format's preset file (`.vstpreset` for
//! VST3, transit's CLAP container for CLAP). Real fixture plug-ins of both formats: save from one
//! instance, write the file, read it back, load it into a second live instance, and compare.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::{JobPoll, Plugin, StateJob};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use transit_plugin_host::{Config, Hooks, Host};

/// Builds a fixture crate of a host crate (a cdylib outside the workspace) into the shared target.
fn build(host_crate: &str, target_dir: &str) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../{host_crate}/tests/fixture/Cargo.toml"));
    let tmp = PathBuf::from(option_env!("CARGO_TARGET_TMPDIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp")));
    let target = tmp.join(target_dir);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .env_remove("RUSTFLAGS")
        .status()
        .expect("run cargo");
    assert!(st.success(), "building the {host_crate} fixture failed");
    target.join("debug")
}

fn install_dir(name: &str) -> PathBuf {
    let tmp = PathBuf::from(option_env!("CARGO_TARGET_TMPDIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp")));
    let dir = tmp.join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn clap_fixture() {
    soundcraft_clap_host::set_scan_extra_dirs_only(true);
    let built = build("clap-host", "clap-fixture-target");
    let lib = if cfg!(target_os = "macos") {
        "libclap_fixture.dylib"
    } else if cfg!(windows) {
        "clap_fixture.dll"
    } else {
        "libclap_fixture.so"
    };
    let dir = install_dir("plugin-host-clap-presets");
    if cfg!(target_os = "macos") {
        let exe = dir.join("vendor/Fixture.clap/Contents/MacOS");
        std::fs::create_dir_all(&exe).unwrap();
        std::fs::copy(built.join(lib), exe.join("Fixture")).unwrap();
    } else {
        std::fs::create_dir_all(dir.join("vendor")).unwrap();
        std::fs::copy(built.join(lib), dir.join("vendor/Fixture.clap")).unwrap();
    }
    assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 2);
}

fn vst3_fixture() -> String {
    soundcraft_vst3_host::set_scan_extra_dirs_only(true);
    let built = build("vst3-host", "vst3-fixture-target");
    let (lib, folder, exe) = if cfg!(target_os = "macos") {
        ("libvst3_fixture.dylib", "MacOS", "Fixture")
    } else if cfg!(windows) {
        ("vst3_fixture.dll", "x86_64-win", "Fixture.vst3")
    } else {
        ("libvst3_fixture.so", if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, "Fixture.so")
    };
    let dir = install_dir("plugin-host-vst3-presets");
    let bin = dir.join("vendor/Fixture.vst3/Contents").join(folder);
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(built.join(lib), bin.join(exe)).unwrap();
    assert_eq!(soundcraft_vst3_host::add_search_dir(&dir), 3);
    soundcraft_vst3_host::scan_paths(&[dir]).into_iter().find(|d| d.name == "Fixture Gain").unwrap().id
}

fn finish<T>(mut job: StateJob<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match job.poll() {
            JobPoll::Ready(v) => return v,
            JobPoll::Failed(e) => panic!("the state job failed: {e}"),
            JobPoll::Waiting if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            JobPoll::Waiting => panic!("the state job did not finish"),
        }
    }
}

/// Makes an instance on the host thread, as the Player does (adopted there).
fn make(host: &Host, id: &'static str, create: fn(&str) -> Option<Box<dyn Plugin>>) -> Box<dyn Plugin> {
    host.submit(move |ctx| {
        let mut p = create(id).expect("the fixture plug-in");
        p.prepare(48_000.0, 64, 2);
        ctx.adopt(p)
    })
    .wait(Duration::from_secs(10))
    .expect("made on the host thread")
}

/// Sets a parameter the way the session does (the audio side), and runs a block so it lands.
fn set(p: &mut Box<dyn Plugin>, id: &str, value: f32) {
    assert!(p.set_param(id, value));
    let mut io = vec![vec![0.25f32; 64]; 2];
    p.process(&mut io, 64);
}

/// One format's round trip: A's state → preset file → B, live, through the host thread.
fn round_trip(host: &Host, a: &mut Box<dyn Plugin>, b: &mut Box<dyn Plugin>, encode: impl Fn(&[u8]) -> Vec<u8>, decode: impl Fn(&[u8]) -> Vec<u8>) {
    let mut ra = a.state_reader().unwrap();
    let mut rb = b.state_reader().unwrap();
    let saved = finish(ra.begin_save()).expect("A has a state");
    let file = encode(&saved);
    let path = install_dir("plugin-host-preset-file").with_extension("preset");
    std::fs::write(&path, &file).unwrap();
    let back = decode(&std::fs::read(&path).unwrap());
    assert_eq!(back, saved, "the file holds the state exactly");
    let before = finish(rb.begin_save()).unwrap();
    assert_ne!(before, saved, "B starts elsewhere");
    let values = finish(rb.begin_load(back)).expect("B takes the preset");
    assert_eq!(finish(rb.begin_save()).unwrap(), saved, "B's state is A's now");
    assert!(!values.is_empty(), "the values after the load come back");
    let _ = std::fs::remove_file(&path);
    let _ = host;
}

#[test]
fn clap_and_vst3_presets_round_trip_through_the_host_thread() {
    let host = Host::new(Config { inline: false, hooks: Hooks::formats(), ..Config::default() });
    // CLAP: the gain fixture keeps gain (param 7) and mode in its state.
    clap_fixture();
    let clap_create: fn(&str) -> Option<Box<dyn Plugin>> = soundcraft_clap_host::create;
    let mut a = make(&host, "clap:org.soundcraft.test.gain", clap_create);
    let mut b = make(&host, "clap:org.soundcraft.test.gain", clap_create);
    set(&mut a, "7", 0.5);
    round_trip(
        &host,
        &mut a,
        &mut b,
        |s| soundcraft_clap_host::preset::file_from_state("clap:org.soundcraft.test.gain", s).unwrap(),
        |f| soundcraft_clap_host::preset::state_from_file("clap:org.soundcraft.test.gain", f).unwrap(),
    );
    let values = finish(b.state_reader().unwrap().begin_load(finish(a.state_reader().unwrap().begin_save()).unwrap())).unwrap();
    assert!(values.iter().any(|(id, v)| id == "7" && (v - 0.5).abs() < 1e-6), "the loaded gain is reported: {values:?}");
    drop((a, b));

    // VST3: component plus controller state, in a `.vstpreset`.
    let id: &'static str = Box::leak(vst3_fixture().into_boxed_str());
    let vst3_create: fn(&str) -> Option<Box<dyn Plugin>> = soundcraft_vst3_host::create;
    let mut a = make(&host, id, vst3_create);
    let mut b = make(&host, id, vst3_create);
    // A's own state, loaded live: gain 0.75 (component) and frequency 0.3 (controller).
    let state = soundcraft_vst3_host::preset::join_state(&0.75f64.to_le_bytes(), &0.3f64.to_le_bytes()).unwrap();
    let values = finish(a.state_reader().unwrap().begin_load(state)).expect("A takes a state");
    assert!(values.iter().any(|(id, v)| id == "7" && (v - 1.5).abs() < 1e-4), "gain 0.75 normalized is 1.5 plain: {values:?}");
    round_trip(
        &host,
        &mut a,
        &mut b,
        |s| soundcraft_vst3_host::preset::file_from_state(id, s).unwrap(),
        |f| soundcraft_vst3_host::preset::state_from_file(id, f).unwrap(),
    );
    let file = soundcraft_vst3_host::preset::file_from_state(id, &finish(a.state_reader().unwrap().begin_save()).unwrap()).unwrap();
    assert_eq!(&file[..4], b"VST3", "a standard .vstpreset");
    drop((a, b));
    assert!(host.shutdown(Duration::from_secs(10)));
}
