//! Shared by the integration tests: builds the fixture plugins (a cdylib outside the workspace)
//! and installs them in temp folders.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Builds the fixture once and returns a folder containing `Fixture.clap` (gain and synth).
#[allow(dead_code)]
pub fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| install(&[], "clap-fixture-target", "clap-fixture-install", "Fixture", 2))
}

/// Builds the fixture with its `requests` feature once and returns a folder containing
/// `Requests.clap` (only the requests plug-in).
#[allow(dead_code)]
pub fn requests_fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| install(&["--features", "requests"], "clap-requests-fixture-target", "clap-requests-install", "Requests", 1))
}

fn install(features: &[&str], target_dir: &str, install_dir: &str, name: &str, plugins: usize) -> PathBuf {
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
    assert!(st.success(), "building the CLAP fixture failed");
    let lib = if cfg!(target_os = "macos") {
        "libclap_fixture.dylib"
    } else if cfg!(windows) {
        "clap_fixture.dll"
    } else {
        "libclap_fixture.so"
    };
    let built = target.join("debug").join(lib);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{install_dir}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if cfg!(target_os = "macos") {
        // Install as a real macOS bundle to exercise bundle resolution.
        let exe = dir.join(format!("vendor/{name}.clap/Contents/MacOS"));
        std::fs::create_dir_all(&exe).unwrap();
        std::fs::copy(&built, exe.join(name)).unwrap();
    } else {
        std::fs::create_dir_all(dir.join("vendor")).unwrap();
        std::fs::copy(&built, dir.join(format!("vendor/{name}.clap"))).unwrap();
    }
    assert_eq!(soundcraft_clap_host::add_search_dir(&dir), plugins);
    dir
}
