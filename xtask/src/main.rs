//! `cargo xtask <ci|layers|assets|parity|wasm|rt-targets>` — repository gates.

mod ico;
mod version;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

fn cargo(args: &[&str]) -> Result<(), String> {
    let st =
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())).args(args).current_dir(root()).status().map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("cargo {} failed", args.join(" "))) }
}

/// Layer table: crate name (minus `soundcraft-` or `transit-`) → layer. Apps and xtask are exempt.
const LAYERS: &[(&str, u8)] = &[
    ("time", 0),
    ("rt", 0),
    ("audio-io", 0),
    ("audio-backend", 0),
    ("audio-wasapi", 0),
    ("audio-asio", 0),
    ("midi-io", 0),
    ("midi", 0),
    ("video", 0),
    ("dsp", 1),
    ("clap-host", 1),
    ("vst3-host", 1),
    ("au-host", 1),
    ("asio-panel", 1),
    ("model", 2),
    ("mix", 3),
    ("engine", 4),
    ("playback", 4),
    ("automation", 5),
    ("ui-egui", 6),
    ("ui-studio", 6),
];
/// Standalone format crates: no workspace dependencies at all (publishable on their own).
const STANDALONE: &[&str] = &["time", "audio-io", "midi-io", "midi", "video", "dsp"];
const UI_CRATES: &[&str] = &["craft-ui", "egui", "eframe", "winit", "egui_kittest", "rfd", "egui-wgpu", "egui_glow"];
const EXEMPT: &[&str] = &["soundcraft", "soundcraft-cli", "soundcraft-web", "transit", "xtask", "audio-bench"];
/// The only crates whose manifest may opt out of the workspace `unsafe_code = "forbid"` (ADR 0013
/// and its amendments): `rt`, `audio-wasapi` and `audio-asio` set `allow`; the SDK and FFI crates set `deny`
/// and allow `unsafe` only on their FFI module. `au-host` and `asio-panel` are the two isolated
/// FFI helpers that predate the amendments and follow the same `deny` pattern.
const UNSAFE_ALLOWED: &[&str] = &["rt", "audio-wasapi", "audio-asio", "clap-host", "vst3-host", "au-host", "asio-panel"];

/// True when the manifest sets `unsafe_code` to anything but `forbid` (any spacing or quoting).
fn opts_out_of_forbid(manifest: &str) -> bool {
    manifest.lines().any(|line| {
        let line: String = line.chars().filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\'')).collect();
        line.strip_prefix("unsafe_code=").is_some_and(|level| !level.starts_with("forbid") && !level.starts_with('#'))
    })
}

fn layers() -> Result<(), String> {
    let out =
        Command::new("cargo").args(["metadata", "--format-version", "1", "--no-deps"]).current_dir(root()).output().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let mut problems = Vec::new();
    for p in v["packages"].as_array().cloned().unwrap_or_default() {
        let name = p["name"].as_str().unwrap_or("").to_string();
        let short = name.strip_prefix("transit-").or_else(|| name.strip_prefix("soundcraft-")).unwrap_or(&name);
        let manifest = p["manifest_path"].as_str().and_then(|m| std::fs::read_to_string(m).ok()).unwrap_or_default();
        // Exempt crates are exempt from layering, not from the unsafe policy.
        if opts_out_of_forbid(&manifest) && !UNSAFE_ALLOWED.contains(&short) {
            problems.push(format!("{name}: opts out of unsafe_code = forbid but is not in the xtask unsafe allowlist"));
        }
        if EXEMPT.contains(&name.as_str()) {
            continue;
        }
        let Some(&(_, layer)) = LAYERS.iter().find(|(n, _)| *n == short) else {
            problems.push(format!("{name}: not registered in xtask LAYERS"));
            continue;
        };
        for d in p["dependencies"].as_array().cloned().unwrap_or_default() {
            let dn = d["name"].as_str().unwrap_or("");
            let kind = d["kind"].as_str().unwrap_or("normal");
            if UI_CRATES.contains(&dn) && layer < 6 && kind != "dev" {
                problems.push(format!("{name} (L{layer}) depends on UI crate {dn}"));
            }
            if let Some(ds) = dn.strip_prefix("soundcraft-").or_else(|| dn.strip_prefix("transit-")) {
                if STANDALONE.contains(&short) && kind != "dev" {
                    problems.push(format!("{name} is standalone but depends on {dn}"));
                }
                if let Some(&(_, dl)) = LAYERS.iter().find(|(n, _)| *n == ds)
                    && dl > layer
                    && kind != "dev"
                {
                    problems.push(format!("{name} (L{layer}) depends upward on {dn} (L{dl})"));
                }
            }
        }
    }
    if problems.is_empty() {
        println!("layers: ok");
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
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
];

fn wasm() -> Result<(), String> {
    let mut args = vec!["check", "--target", "wasm32-unknown-unknown"];
    for c in WASM {
        args.push("-p");
        args.push(c);
    }
    if root().join("apps/soundcraft-web/Cargo.toml").exists() {
        args.push("-p");
        args.push("soundcraft-web");
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
        Some("version") => version::run(args.get(1..).unwrap_or(&[])),
        Some("ico") => ico::run(args.get(1..).unwrap_or(&[])),
        _ => Err("usage: cargo xtask <ci|layers|assets|parity|wasm|rt-targets|audio-bench|version>".into()),
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
mod tests {
    use super::opts_out_of_forbid;

    #[test]
    fn the_unsafe_check_sees_any_spelling_of_an_opt_out() {
        assert!(opts_out_of_forbid(
            "[lints.rust]
unsafe_code = \"allow\"
"
        ));
        assert!(opts_out_of_forbid("unsafe_code=\"allow\""));
        assert!(opts_out_of_forbid("unsafe_code = 'deny'"));
        assert!(!opts_out_of_forbid(
            "unsafe_code = \"forbid\"
"
        ));
        assert!(!opts_out_of_forbid(
            "# opts out of `unsafe_code = \"allow\"` for itself
[lints]
workspace = true
"
        ));
    }
}
