use super::*;
use serde_json::json;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;

const INHERIT: &str = "[lints]\nworkspace = true\n";

fn output(status: u32, value: serde_json::Value) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(status as _),
        stdout: value.to_string().into_bytes(),
        stderr: b"metadata diagnostic".to_vec(),
    }
}

fn package(name: &str, dependencies: &[(&str, Option<&str>)]) -> Package {
    Package {
        name: name.into(),
        manifest_path: PathBuf::from(format!("{name}/Cargo.toml")),
        dependencies: dependencies.iter().map(|(name, kind)| Dependency { name: (*name).into(), kind: kind.map(str::to_string) }).collect(),
    }
}

#[test]
fn cargo_commands_use_the_cargo_executable_and_workspace_root() {
    let command = cargo_command();
    assert_eq!(command.get_program(), std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    assert_eq!(command.get_current_dir(), Some(root().as_path()));
}

#[test]
fn failed_metadata_is_rejected_even_with_valid_stdout() {
    let valid = json!({"packages": [{"name": "soundcraft-time", "manifest_path": "Cargo.toml", "dependencies": []}]});
    assert!(metadata_output(output(0, valid.clone())).is_ok());
    let Err(error) = metadata_output(output(1, valid)) else { panic!("accepted failed cargo metadata") };
    assert!(error.contains("cargo metadata failed"));
    assert!(error.contains("metadata diagnostic"));
}

#[test]
fn missing_or_malformed_metadata_never_passes_as_an_empty_workspace() {
    for value in [
        json!({}),
        json!({"packages": null}),
        json!({"packages": []}),
        json!({"packages": {}}),
        json!({"packages": [{"name": "soundcraft-time", "manifest_path": "Cargo.toml"}]}),
        json!({"packages": [{"name": "soundcraft-time", "manifest_path": "Cargo.toml", "dependencies": null}]}),
        json!({"packages": [{"name": "soundcraft-time", "dependencies": []}]}),
        json!({"packages": [{"manifest_path": "Cargo.toml", "dependencies": []}]}),
    ] {
        assert!(metadata_output(output(0, value.clone())).is_err(), "accepted {value}");
    }
    let mut out = output(0, json!({}));
    out.stdout = b"not JSON".to_vec();
    assert!(metadata_output(out).is_err());
}

#[test]
fn unreadable_or_invalid_manifests_fail_even_for_exempt_apps() {
    for name in ["soundcraft-time", "transit"] {
        let metadata = Metadata { packages: vec![package(name, &[])] };
        let error = check_layers(&metadata, |_| Err("permission denied".into())).unwrap_err();
        assert!(error.contains("permission denied"));
        assert!(error.contains("Cargo.toml"));
        assert!(check_layers(&metadata, |_| Ok("[invalid".into())).is_err());
    }
}

#[test]
fn unsafe_lints_are_parsed_as_toml_instead_of_matching_text() {
    for (manifest, opts_out) in [
        ("[lints.rust]\nunsafe_code = 'allow'", true),
        ("[lints.rust]\nunsafe_code='deny'", true),
        ("[lints.rust]\nunsafe_code = { level = 'allow', priority = 0 }", true),
        ("[lints.rust]\nunsafe_code = 'forbid'", false),
        ("[lints.rust]\nunsafe_code = { level = 'forbid', priority = 0 }", false),
        ("[lints.rust.unsafe_code]\nlevel = 'forbid'", false),
        ("lints.rust.unsafe_code = { level = 'forbid' }", false),
        ("[package.metadata]\nunsafe_code = 'forbid'", true),
        ("# unsafe_code = 'allow'\n[lints]\nworkspace = true", false),
        (INHERIT, false),
        ("", true),
    ] {
        assert_eq!(opts_out_of_forbid(manifest), Ok(opts_out), "{manifest}");
    }
    assert!(opts_out_of_forbid("[lints").is_err());
}

#[test]
fn newly_prefixed_crates_do_not_inherit_layer_or_unsafe_permissions() {
    for name in ["transit-time", "soundcraft-rt"] {
        let metadata = Metadata { packages: vec![package(name, &[])] };
        assert!(check_layers(&metadata, |_| Ok(INHERIT.into())).unwrap_err().contains("not registered"));
        let error = check_layers(&metadata, |_| Ok("[lints.rust]\nunsafe_code = 'allow'".into())).unwrap_err();
        assert!(error.contains("unsafe allowlist"));
    }
}

#[test]
fn standalone_and_ui_checks_cover_normal_and_build_dependencies_but_not_dev() {
    for name in ["transit-audio-backend", "soundcraft-asio-panel"] {
        for kind in [None, Some("build"), Some("dev")] {
            let metadata = Metadata { packages: vec![package(name, &[("soundcraft-time", kind)]), package("soundcraft-time", &[])] };
            let result = check_layers(&metadata, |_| Ok(INHERIT.into()));
            assert_eq!(result.is_ok(), kind == Some("dev"), "{name}: {kind:?}");
        }
    }
    for name in ["egui_extras", "resvg", "wgpu"] {
        for kind in [None, Some("build"), Some("dev")] {
            let metadata = Metadata { packages: vec![package("soundcraft-model", &[(name, kind)])] };
            assert_eq!(check_layers(&metadata, |_| Ok(INHERIT.into())).is_ok(), kind == Some("dev"), "{name}: {kind:?}");
        }
    }
}

#[test]
fn upward_and_app_edges_fail_while_documented_same_layer_edges_remain_valid() {
    for (source, dest, valid) in [
        ("soundcraft-model", "soundcraft-engine", false),
        ("soundcraft-model", "transit", false),
        ("soundcraft-clap-host", "soundcraft-dsp", true),
        ("transit-audio-wasapi", "transit-rt", true),
    ] {
        let metadata = Metadata { packages: vec![package(source, &[(dest, None)]), package(dest, &[])] };
        assert_eq!(check_layers(&metadata, |_| Ok(INHERIT.into())).is_ok(), valid, "{source} -> {dest}");
    }
}

#[test]
fn legitimate_video_decoding_and_benchmark_app_dependencies_remain_valid() {
    let metadata = Metadata {
        packages: vec![
            package("soundcraft-video", &[("image", None)]),
            package("audio-bench", &[("soundcraft-playback", None)]),
            package("soundcraft-playback", &[]),
        ],
    };
    assert!(check_layers(&metadata, |_| Ok(INHERIT.into())).is_ok());
}

#[test]
fn shipped_app_inherits_the_workspace_version() -> Result<(), Box<dyn std::error::Error>> {
    let workspace: toml::Value = toml::from_str(&std::fs::read_to_string(root().join("Cargo.toml"))?)?;
    let app: toml::Value = toml::from_str(&std::fs::read_to_string(root().join("apps/transit/Cargo.toml"))?)?;
    assert_eq!(app["package"]["version"]["workspace"].as_bool(), Some(true));
    assert_eq!(workspace["workspace"]["package"]["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
    Ok(())
}
