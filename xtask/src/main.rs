//! `cargo xtask <ci|layers|assets|parity|wasm|rt-targets|audio-bench|ico>` — repository gates.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod ico;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

fn cargo_command() -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(root());
    command
}

fn cargo(args: &[&str]) -> Result<(), String> {
    let st = cargo_command().args(args).status().map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("cargo {} failed ({st})", args.join(" "))) }
}

/// Full package names avoid treating a new transit crate as an inherited SoundCraft crate.
/// Same-layer edges are permitted (docs/contributing/knowledge/layering.md).
const LAYERS: &[(&str, u8)] = &[
    ("soundcraft-time", 0),
    ("transit-rt", 0),
    ("soundcraft-audio-io", 0),
    ("transit-audio-backend", 0),
    ("transit-audio-wasapi", 0),
    ("transit-audio-asio", 0),
    ("transit-midi-io", 0),
    ("soundcraft-midi", 0),
    ("soundcraft-video", 0),
    ("soundcraft-dsp", 1),
    ("soundcraft-clap-host", 1),
    ("soundcraft-vst3-host", 1),
    ("soundcraft-au-host", 1),
    ("soundcraft-asio-panel", 1),
    ("soundcraft-model", 2),
    ("soundcraft-mix", 3),
    ("soundcraft-engine", 4),
    ("soundcraft-playback", 4),
    ("soundcraft-automation", 5),
    ("soundcraft-ui-egui", 6),
    ("transit-ui-studio", 6),
];
/// Standalone crates have no workspace dependencies (dev-dependencies excepted).
const STANDALONE: &[&str] = &[
    "soundcraft-time",
    "soundcraft-audio-io",
    "transit-audio-backend",
    "transit-midi-io",
    "soundcraft-midi",
    "soundcraft-video",
    "soundcraft-dsp",
    "soundcraft-asio-panel",
];
// The image crate also decodes video frames in L0; it is not a UI-only dependency.
const UI_CRATES: &[&str] = &["craft-ui", "egui", "eframe", "winit", "egui_kittest", "rfd", "egui-wgpu", "egui_glow", "egui_extras", "resvg", "wgpu"];
// audio-bench is an app over playback and the native backends, not an engine layer.
const EXEMPT: &[&str] = &["soundcraft", "soundcraft-cli", "soundcraft-web", "transit", "xtask", "audio-bench"];
/// Manifest exceptions to the workspace unsafe forbid (ADR 0013 and its amendments).
const UNSAFE_ALLOWED: &[&str] = &[
    "transit-rt",
    "transit-audio-wasapi",
    "transit-audio-asio",
    "soundcraft-clap-host",
    "soundcraft-vst3-host",
    "soundcraft-au-host",
    "soundcraft-asio-panel",
];

/// Parse Cargo's string and table lint forms, including workspace inheritance.
fn opts_out_of_forbid(manifest: &str) -> Result<bool, String> {
    let manifest: toml::Value = toml::from_str(manifest).map_err(|e| format!("invalid manifest: {e}"))?;
    let lints = manifest.get("lints");
    if lints.and_then(|v| v.get("workspace")).and_then(toml::Value::as_bool) == Some(true) {
        return Ok(false);
    }
    let lint = lints.and_then(|v| v.get("rust")).and_then(|v| v.get("unsafe_code"));
    let level = lint.and_then(|v| v.as_str().or_else(|| v.get("level").and_then(toml::Value::as_str)));
    Ok(level != Some("forbid"))
}

#[derive(serde::Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(serde::Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<Dependency>,
}

#[derive(serde::Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
}

fn metadata_output(out: Output) -> Result<Metadata, String> {
    if !out.status.success() {
        return Err(format!("cargo metadata failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    let metadata: Metadata = serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    if metadata.packages.is_empty() {
        return Err("cargo metadata returned no packages".into());
    }
    Ok(metadata)
}

fn check_layers(metadata: &Metadata, read_manifest: impl Fn(&Path) -> Result<String, String>) -> Result<(), String> {
    let mut problems = Vec::new();
    for p in &metadata.packages {
        let name = p.name.as_str();
        if name.is_empty() || p.manifest_path.as_os_str().is_empty() {
            return Err("cargo metadata: empty package name or manifest path".into());
        }
        let manifest = read_manifest(&p.manifest_path).map_err(|e| format!("{}: {e}", p.manifest_path.display()))?;
        // Exempt crates are exempt from layering, not from the unsafe policy.
        if opts_out_of_forbid(&manifest).map_err(|e| format!("{name}: {e}"))? && !UNSAFE_ALLOWED.contains(&name) {
            problems.push(format!("{name}: opts out of unsafe_code = forbid but is not in the xtask unsafe allowlist"));
        }
        if EXEMPT.contains(&name) {
            continue;
        }
        let Some(&(_, layer)) = LAYERS.iter().find(|(n, _)| *n == name) else {
            problems.push(format!("{name}: not registered in xtask LAYERS"));
            continue;
        };
        for d in &p.dependencies {
            let dn = d.name.as_str();
            if dn.is_empty() || !matches!(d.kind.as_deref(), None | Some("normal" | "build" | "dev")) {
                return Err(format!("{name}: invalid dependency name or kind"));
            }
            if d.kind.as_deref() == Some("dev") {
                continue;
            }
            if UI_CRATES.contains(&dn) && layer < 6 {
                problems.push(format!("{name} (L{layer}) depends on UI crate {dn}"));
            }
            if metadata.packages.iter().any(|p| p.name == dn) {
                if STANDALONE.contains(&name) {
                    problems.push(format!("{name} is standalone but depends on {dn}"));
                }
                if let Some(&(_, dl)) = LAYERS.iter().find(|(n, _)| *n == dn) {
                    if dl > layer {
                        problems.push(format!("{name} (L{layer}) depends upward on {dn} (L{dl})"));
                    }
                } else {
                    problems.push(format!("{name} depends on workspace app or unregistered crate {dn}"));
                }
            }
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems.join("\n")) }
}

fn layers() -> Result<(), String> {
    let out = cargo_command().args(["metadata", "--locked", "--format-version", "1", "--no-deps"]).output().map_err(|e| e.to_string())?;
    let metadata = metadata_output(out)?;
    check_layers(&metadata, |path| std::fs::read_to_string(path).map_err(|e| e.to_string()))?;
    println!("layers: ok");
    Ok(())
}

const ASSET_EXT: &[&str] =
    &["png", "jpg", "jpeg", "svg", "ico", "icns", "ttf", "otf", "ttc", "woff", "woff2", "wav", "aif", "aiff", "flac", "mp3", "ogg", "gif", "webp"];

fn assets() -> Result<(), String> {
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root())
        .output()
        .map_err(|e| e.to_string())?;
    let files = String::from_utf8_lossy(&out.stdout).to_string();
    let attribution = std::fs::read_to_string(root().join("ATTRIBUTION.md")).map_err(|e| format!("ATTRIBUTION.md: {e}"))?;
    let mut missing = Vec::new();
    for f in files.lines() {
        let ext = Path::new(f).extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
        if !ASSET_EXT.contains(&ext.as_str()) {
            continue;
        }
        // Generated icon trees are covered by their directory row.
        let covered = attribution.contains(&format!("`{f}`"))
            || Path::new(f).ancestors().skip(1).any(|a| !a.as_os_str().is_empty() && attribution.contains(&format!("`{}/`", a.display())));
        if !covered {
            missing.push(f.to_string());
        }
    }
    if missing.is_empty() {
        println!("assets: ok");
        Ok(())
    } else {
        Err(format!("assets without an ATTRIBUTION.md row:\n  {}", missing.join("\n  ")))
    }
}

fn parity() -> Result<(), String> {
    cargo(&["run", "-q", "-p", "soundcraft-cli", "--", "parity", "--write", "docs/parity-checklist.md"])
}

/// Crates that must compile for the web.
const WASM: &[&str] = &[
    "soundcraft-time",
    "soundcraft-audio-io",
    "transit-audio-backend",
    "soundcraft-midi",
    "soundcraft-video",
    "soundcraft-dsp",
    "soundcraft-model",
    "soundcraft-mix",
    "soundcraft-engine",
    "soundcraft-playback",
    "soundcraft-automation",
    "soundcraft-ui-egui",
    "soundcraft-web",
];

fn wasm() -> Result<(), String> {
    let mut args = vec!["check", "--target", "wasm32-unknown-unknown"];
    for c in WASM {
        args.push("-p");
        args.push(c);
    }
    cargo(&args)
}

/// `transit-rt` type-checks and lints clean on every platform it has code for (ADR 0013, 5),
/// with and without the portable fallback. Needs `rustup target add` for each; no linker needed.
const RT_TARGETS: &[&str] = &["x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-apple-darwin"];

fn rt_targets() -> Result<(), String> {
    for t in RT_TARGETS {
        for features in [None, Some("force-fallback")] {
            let mut args = vec!["clippy", "--target", t, "-p", "transit-rt", "--all-targets"];
            if let Some(f) = features {
                args.extend(["--features", f]);
            }
            args.extend(["--", "-D", "warnings"]);
            cargo(&args)?;
        }
    }
    Ok(())
}

/// `cargo xtask audio-bench <options>`: the backend benchmark harness (ADR 0016), `tools/audio-bench`.
/// It lives in its own crate because it depends on playback (xtask has no workspace dependencies).
/// Release build: the figures are timings. See `docs/benchmarks/README.md`.
fn audio_bench(args: &[String]) -> Result<(), String> {
    let mut command = vec!["run".to_string(), "-q".into(), "--release".into(), "-p".into(), "audio-bench".into(), "--".into()];
    command.extend(args.iter().cloned());
    cargo(&command.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The device-free mode, for CI: opens no audio device.
fn audio_bench_fake() -> Result<(), String> {
    cargo(&["run", "-q", "-p", "audio-bench", "--", "--backend", "fake", "--seconds", "2", "--loopback"])
}

fn ci() -> Result<(), String> {
    let steps: Vec<(&str, Box<dyn Fn() -> Result<(), String>>)> = vec![
        ("fmt", Box::new(|| cargo(&["fmt", "--all", "--", "--check"]))),
        ("clippy", Box::new(|| cargo(&["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]))),
        ("test", Box::new(|| cargo(&["test", "--workspace"]))),
        ("assets", Box::new(assets)),
        ("layers", Box::new(layers)),
        ("wasm", Box::new(wasm)),
        ("rt-targets", Box::new(rt_targets)),
        ("audio-bench-fake", Box::new(audio_bench_fake)),
    ];
    let mut summary = Vec::new();
    for (name, f) in &steps {
        match f() {
            Ok(()) => summary.push(format!("ok    {name}")),
            Err(e) => {
                summary.push(format!("FAIL  {name}"));
                println!("{}", summary.join("\n"));
                return Err(e);
            }
        }
    }
    println!("{}", summary.join("\n"));
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("ci") => ci(),
        Some("layers") => layers(),
        Some("assets") => assets(),
        Some("parity") => parity(),
        Some("wasm") => wasm(),
        Some("rt-targets") => rt_targets(),
        Some("audio-bench") => audio_bench(args.get(1..).unwrap_or(&[])),
        Some("ico") => ico::run(args.get(1..).unwrap_or(&[])),
        _ => Err("usage: cargo xtask <ci|layers|assets|parity|wasm|rt-targets|audio-bench|ico>".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
