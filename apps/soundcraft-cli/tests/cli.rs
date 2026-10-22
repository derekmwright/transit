//! `soundcraft-cli mcp` with the automation root flags, run as the real binary.
// Integration tests: unwrapping and panicking on failure is fine here, unlike in shipped code (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_soundcraft-cli"));
    cmd.env("SOUNDCRAFT_NO_PREFS", "1");
    cmd
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().unwrap()
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-cli-roots-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Send `calls` (tool name, arguments) to `soundcraft-cli mcp <args>`; returns whether it exited
/// cleanly, its stderr, and the stdout lines after `initialize`, one per call.
fn mcp_session(args: &[&str], calls: &[(&str, Value)]) -> (bool, String, Vec<Value>) {
    let mut child = bin().arg("mcp").args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#).unwrap();
        for (i, (name, arguments)) in calls.iter().enumerate() {
            let call = serde_json::json!({"jsonrpc": "2.0", "id": i + 2, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
            writeln!(stdin, "{call}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let replies = stdout.lines().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned(), replies)
}

fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().unwrap()
}

/// A short WAV made by the binary itself.
fn make_wav(path: &std::path::Path) {
    let o = run(&["run", "--demo", "--bounce", path.to_str().unwrap(), "--start", "0", "--end", "0.1"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn mcp_automation_roots_confine_the_server() {
    let base = tmpdir("roots");
    let (work, elsewhere) = (base.join("work"), base.join("elsewhere"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    make_wav(&work.join("in.wav"));
    make_wav(&elsewhere.join("secret.wav"));
    let work_s = work.to_str().unwrap();
    let secret = elsewhere.join("secret.wav").to_str().unwrap().to_string();

    // Both spellings of the flags; inside works, outside is refused with a readable message.
    let read_flag = format!("--automation-read-root={work_s}");
    let calls = [
        ("import_audio", serde_json::json!({"path": work.join("in.wav").to_str().unwrap()})),
        ("import_audio", serde_json::json!({"path": secret})),
        ("save_session", serde_json::json!({"path": work.join("saved.transit").to_str().unwrap()})),
        ("save_session", serde_json::json!({"path": elsewhere.join("saved.transit").to_str().unwrap()})),
        ("bounce_mix", serde_json::json!({"path": elsewhere.join("mix.wav").to_str().unwrap()})),
    ];
    let (success, stderr, replies) = mcp_session(&[&read_flag, "--automation-write-root", work_s], &calls);
    assert!(success, "{stderr}");
    assert!(stderr.contains("files confined"), "{stderr}");
    assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
    assert_eq!(replies[1]["result"]["isError"], true);
    assert!(text(&replies[1]).contains("outside the read root"), "{}", text(&replies[1]));
    assert_eq!(replies[2]["result"]["isError"], false, "{}", replies[2]);
    assert!(work.join("saved.transit").exists());
    assert_eq!(replies[3]["result"]["isError"], true);
    assert!(text(&replies[3]).contains("outside the write root"), "{}", text(&replies[3]));
    assert_eq!(replies[4]["result"]["isError"], true);
    assert!(text(&replies[4]).contains("outside the write root"), "{}", text(&replies[4]));
    assert!(!elsewhere.join("saved.transit").exists() && !elsewhere.join("mix.wav").exists());

    // A root left out grants none of its access.
    let (_, _, replies) = mcp_session(&[&read_flag], &calls[2..3]);
    assert_eq!(replies[0]["result"]["isError"], true);
    assert!(text(&replies[0]).contains("write authority is absent"), "{}", text(&replies[0]));
    let (_, _, replies) = mcp_session(&["--automation-write-root", work_s], &calls[0..1]);
    assert_eq!(replies[0]["result"]["isError"], true);
    assert!(text(&replies[0]).contains("read authority is absent"), "{}", text(&replies[0]));

    // No flags: nothing is confined.
    let (success, _, replies) = mcp_session(&[], &calls[1..2]);
    assert!(success);
    assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn mcp_automation_root_flags_are_checked_up_front() {
    let base = tmpdir("roots-args");
    let dir = base.to_str().unwrap();
    let missing = base.join("missing");
    let fails = |args: &[&str], needle: &str| {
        let mut full = vec!["mcp"];
        full.extend_from_slice(args);
        let o = run(&full);
        assert!(!o.status.success(), "{args:?}");
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        assert!(err.contains(needle), "{args:?}: {err}");
    };
    // The flags confine the in-process server, so they cannot be combined with --connect.
    fails(&["--connect", "7979", "--automation-read-root", dir], "--connect");
    fails(&["--automation-write-root", dir, "--connect", "7979"], "--connect");
    fails(&["--automation-read-root", dir, "--automation-read-root", dir], "given twice");
    fails(&["--automation-write-root", dir, "--automation-write-root", dir], "given twice");
    fails(&["--automation-read-root"], "needs a folder");
    fails(&["--automation-write-root", "--demo"], "needs a folder");
    fails(&["--automation-read-root", missing.to_str().unwrap()], "cannot open the automation read root");
    fails(&["--automation-write-root="], "needs a non-empty folder");
    // A misspelled root flag must not start an unconfined server.
    fails(&["--automation-read-roots", dir], "unknown mcp option");
    // A file is not a folder.
    std::fs::write(base.join("f.txt"), "x").unwrap();
    fails(&["--automation-write-root", base.join("f.txt").to_str().unwrap()], "cannot open the automation write root");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn usage_lists_the_root_flags() {
    let o = run(&["--help"]);
    let help = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(help.contains("--automation-read-root DIR") && help.contains("--automation-write-root DIR"), "{help}");
}
