//! Allocation probe for the plug-in hosts' event paths (F10, ADR 0012 G5): with the VST3 and CLAP
//! fixtures' events plug-ins (which allocate nothing themselves), saturating the note and
//! controller queues and the timed parameter points, then processing the block (the VST3
//! parameter point slab included), allocates and frees nothing on the calling thread.
//!
//! The binary installs `soundcraft_rt::testing::CountingAlloc` and counts the test thread only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_dsp::event::{Message, NoteId, PluginEvent};
use soundcraft_rt::testing::{AllocStats, CountingAlloc, measure};
use std::path::{Path, PathBuf};
use std::process::Command;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const BLOCK: usize = 1024;

/// Builds a fixture crate (outside the workspace) and returns its library.
fn build(fixture: &str, features: &[&str], target_dir: &str, lib: [&str; 3]) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../{fixture}/Cargo.toml"));
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join(target_dir);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .args(features)
        .arg("--target-dir")
        .arg(&target)
        .env_remove("RUSTFLAGS")
        .status()
        .expect("run cargo");
    assert!(st.success(), "building {fixture} failed");
    let name = if cfg!(target_os = "macos") {
        lib[0]
    } else if cfg!(windows) {
        lib[1]
    } else {
        lib[2]
    };
    target.join("debug").join(name)
}

fn vst3_events() -> Box<dyn Plugin> {
    let built = build("vst3-host/tests/fixture", &[], "mix-vst3-fixture-target", ["libvst3_fixture.dylib", "vst3_fixture.dll", "libvst3_fixture.so"]);
    let (folder, exe) = if cfg!(target_os = "macos") {
        ("MacOS", "Fixture")
    } else if cfg!(windows) {
        ("x86_64-win", "Fixture.vst3")
    } else {
        (if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, "Fixture.so")
    };
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mix-vst3-events-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("vendor/Fixture.vst3/Contents").join(folder);
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(&built, bin.join(exe)).unwrap();
    // Registered first: an id the registry does not know would make `create` scan every
    // plug-in on the machine.
    assert_eq!(soundcraft_vst3_host::add_search_dir(&dir), 3);
    let found = soundcraft_vst3_host::scan_paths(&[dir]);
    let id = found.iter().find(|d| d.name == "Fixture Events").unwrap_or_else(|| panic!("{found:?}")).id.clone();
    soundcraft_vst3_host::create(&id).unwrap()
}

fn clap_events() -> Box<dyn Plugin> {
    let built = build(
        "clap-host/tests/fixture",
        &["--features", "events"],
        "mix-clap-events-target",
        ["libclap_fixture.dylib", "clap_fixture.dll", "libclap_fixture.so"],
    );
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mix-clap-events-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if cfg!(target_os = "macos") {
        let exe = dir.join("Events.clap/Contents/MacOS");
        std::fs::create_dir_all(&exe).unwrap();
        std::fs::copy(&built, exe.join("Events")).unwrap();
    } else {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&built, dir.join("Events.clap")).unwrap();
    }
    assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 1);
    soundcraft_clap_host::create("clap:org.soundcraft.test.events").unwrap()
}

/// Saturates every queue of `p`, then processes one block; returns what was refused.
fn saturate_and_process(p: &mut dyn Plugin, io: &mut [Vec<f32>]) -> (usize, usize) {
    let n = NoteId::NONE;
    let mut refused = (0, 0);
    let mut send = |p: &mut dyn Plugin, offset: u32, ev: PluginEvent| {
        if !p.event(offset, &ev) {
            if ev.is_release() {
                refused.1 += 1;
            } else {
                refused.0 += 1;
            }
        }
    };
    for i in 0..1200u32 {
        let (channel, key) = ((i % 16) as u8, (i % 128) as u8);
        send(p, i % BLOCK as u32, PluginEvent { channel, note_id: i as i32, message: Message::NoteOn { key, velocity: 90, id: n } });
        send(p, i % BLOCK as u32, PluginEvent { channel, note_id: -1, message: Message::Control { cc: 64, value: 127 } });
        p.set_param_at(i % BLOCK as u32, "5", (i % 100) as f32 / 100.0);
    }
    for i in 0..256u32 {
        let (channel, key) = ((i % 16) as u8, (i % 128) as u8);
        send(p, BLOCK as u32 - 1, PluginEvent { channel, note_id: i as i32, message: Message::NoteOff { key, velocity: 0, id: n } });
    }
    p.process(io, BLOCK);
    refused
}

#[test]
fn host_event_queues_and_the_vst3_point_slab_allocate_nothing_at_saturation() {
    for name in ["vst3", "clap"] {
        let mut p = if name == "vst3" { vst3_events() } else { clap_events() };
        p.prepare(48_000.0, BLOCK, 2);
        let mut io = vec![vec![0.0f32; BLOCK]; 2];
        // Warm up (not measured): the first blocks.
        for _ in 0..2 {
            saturate_and_process(&mut *p, &mut io);
        }
        let (refused, stats) = measure(|| saturate_and_process(&mut *p, &mut io));
        println!("{name}: {stats:?}, refused (non-release, release) {refused:?}");
        assert_eq!(stats, AllocStats::default(), "{name}: on the audio thread");
        assert!(refused.0 > 0, "{name}: saturated");
        assert_eq!(refused.1, 0, "{name}: every release fits the reserve");
    }
}
