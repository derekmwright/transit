//! Shared by the integration tests: builds the fixture plugin (a cdylib outside the workspace)
//! and installs it as a `.vst3` bundle in a temp folder.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Builds the fixture once and returns a folder containing `Fixture.vst3`.
#[allow(dead_code)]
pub fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| install(&[], "vst3-fixture-target", "vst3-fixture-install", 3))
}

/// Builds the fixture with its `sidechain` feature once and returns a folder containing
/// `Fixture.vst3` with only the sidechain plug-ins (ADR 0015 slice c).
#[allow(dead_code)]
pub fn sidechain_fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| install(&["--features", "sidechain"], "vst3-sidechain-fixture-target", "vst3-sidechain-fixture-install", 2))
}

/// Builds the fixture with its `slow-create` feature once (its synth takes 500 ms to create) and
/// returns a folder containing `Fixture.vst3` with all of the fixture's plug-ins.
#[allow(dead_code)]
pub fn slow_fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| install(&["--features", "slow-create"], "vst3-slow-fixture-target", "vst3-slow-fixture-install", 3))
}

fn install(features: &[&str], target_dir: &str, install_dir: &str, plugins: usize) -> PathBuf {
    {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture/Cargo.toml");
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
        assert!(st.success(), "building the VST3 fixture failed");
        let (lib, folder, exe) = if cfg!(target_os = "macos") {
            ("libvst3_fixture.dylib", "MacOS", "Fixture")
        } else if cfg!(windows) {
            ("vst3_fixture.dll", "x86_64-win", "Fixture.vst3")
        } else {
            ("libvst3_fixture.so", if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, "Fixture.so")
        };
        let built = target.join("debug").join(lib);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{install_dir}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Install as a real bundle to exercise bundle resolution and the module entry.
        let bin = dir.join("vendor/Fixture.vst3/Contents").join(folder);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::copy(&built, bin.join(exe)).unwrap();
        assert_eq!(soundcraft_vst3_host::add_search_dir(&dir), plugins, "the fixture's plug-ins");
        dir
    }
}
