use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use transit_ui_studio::{Services, StudioApp, update};

const RUNNING: &str = "0.2.0-alpha.1";

/// A one-thread HTTP server on an ephemeral port that answers every request with `body`.
struct Fake {
    url: String,
    hits: Arc<AtomicUsize>,
    /// The request heads the server received.
    heads: Arc<std::sync::Mutex<Vec<String>>>,
}

fn fake(status: &str, body: &str) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/releases", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (count, status, body, log) = (hits.clone(), status.to_string(), body.to_string(), heads.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            count.fetch_add(1, Ordering::SeqCst);
            let mut seen = Vec::new();
            let mut buf = [0u8; 512];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
            }
            log.lock().unwrap().push(String::from_utf8_lossy(&seen).into_owned());
            let reply =
                format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = s.write_all(reply.as_bytes());
        }
    });
    Fake { url, hits, heads }
}

const NEWER: &str = r#"[
  {"tag_name":"v0.2.0-alpha.2","html_url":"https://github.com/transit-audio/transit/releases/tag/v0.2.0-alpha.2","draft":false},
  {"tag_name":"v0.2.0-alpha.1","html_url":"https://example.invalid/old"},
  {"tag_name":"nightly","html_url":"https://example.invalid/nightly"}
]"#;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn releases(tags: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        tags.iter().map(|t| serde_json::json!({"tag_name": t, "html_url": format!("https://github.com/x/y/releases/tag/{t}")})).collect(),
    )
}

#[test]
fn older_and_equal_are_not_news() {
    assert_eq!(newest_newer(&v("0.2.0-alpha.1"), &releases(&["v0.1.0", "v0.2.0-alpha.1", "v0.2.0-alpha.0"])), None);
}

#[test]
fn newer_prerelease_and_release_order_by_semver() {
    let n = newest_newer(&v("0.2.0-alpha.1"), &releases(&["v0.2.0-alpha.2", "v0.2.0-alpha.10", "v0.2.0-alpha.9"])).unwrap();
    assert_eq!(n.version, "0.2.0-alpha.10");
    assert!(n.url.ends_with("v0.2.0-alpha.10"));
    // The release is newer than its own pre-releases; a pre-release of the next version is newer than the release.
    assert_eq!(newest_newer(&v("0.2.0-alpha.1"), &releases(&["v0.2.0"])).unwrap().version, "0.2.0");
    assert_eq!(newest_newer(&v("0.2.0"), &releases(&["v0.2.0-alpha.9"])), None);
    assert_eq!(newest_newer(&v("0.2.0"), &releases(&["v0.2.0", "v0.3.0-alpha.1"])).unwrap().version, "0.3.0-alpha.1");
}

#[test]
fn malformed_tags_drafts_and_non_lists_are_skipped() {
    let list = serde_json::json!([
        {"tag_name": "nightly"}, {"tag_name": "v1"}, {"tag_name": 7}, {"name": "no tag"},
        {"tag_name": "v9.0.0", "draft": true},
        {"tag_name": "v0.3.0", "html_url": "javascript:alert(1)"}
    ]);
    let n = newest_newer(&v("0.2.0"), &list).unwrap();
    assert_eq!(n.version, "0.3.0");
    assert_eq!(n.url, "https://github.com/transit-audio/transit/releases/tag/v0.3.0", "a non-https link is replaced by the tag page");
    assert_eq!(newest_newer(&v("0.2.0"), &serde_json::json!({"message": "rate limited"})), None);
}

#[test]
fn the_environment_switch() {
    assert!(disabled_by(Some("1")));
    assert!(disabled_by(Some("yes")));
    assert!(!disabled_by(Some("0")));
    assert!(!disabled_by(Some("")));
    assert!(!disabled_by(None));
}

#[test]
fn fetch_finds_a_newer_release() {
    let f = fake("200 OK", NEWER);
    match check(&f.url, RUNNING) {
        UpdateOutcome::Available(n) => {
            assert_eq!(n.version, "0.2.0-alpha.2");
            assert_eq!(n.line(), "transit v0.2.0-alpha.2 is available");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    let head = f.heads.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.starts_with("get /releases "), "{head}");
    let want = format!("user-agent: transit/{}", env!("CARGO_PKG_VERSION"));
    assert!(head.lines().any(|l| l.trim_end() == want), "{head}");
}

#[test]
fn ten_is_newer_than_nine_and_build_metadata_is_ignored() {
    assert_eq!(newest_newer(&v("0.9.9"), &releases(&["v0.10.0"])).unwrap().version, "0.10.0");
    assert_eq!(newest_newer(&v("0.10.0"), &releases(&["v0.9.9"])), None);
    assert_eq!(newest_newer(&v("0.2.0"), &releases(&["v0.2.0+x"])), None);
    assert_eq!(newest_newer(&v("0.2.0"), &releases(&["v0.2.0+x", "v0.2.1+y"])).unwrap().version, "0.2.1");
}

#[test]
fn a_launch_check_is_due_when_missing_in_the_future_or_a_day_old() {
    let day = update::CHECK_INTERVAL_SECS;
    assert!(update::check_due(0, 1_000_000));
    assert!(update::check_due(2_000_000, 1_000_000), "a timestamp in the future");
    assert!(update::check_due(1_000_000 - day, 1_000_000));
    assert!(!update::check_due(1_000_000 - day + 1, 1_000_000));
    assert!(!update::check_due(1_000_000, 1_000_000));
}

#[test]
fn a_future_timestamp_still_runs_the_launch_check() {
    let f = fake("200 OK", NEWER);
    let mut app = studio(&f.url);
    app.ui.last_update_check = u64::MAX / 2;
    app.update_tick(&egui::Context::default());
    settle(&mut app);
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    assert!(app.ui.last_update_check < u64::MAX / 2);
}

#[test]
fn fetch_with_nothing_newer_is_current() {
    let f = fake("200 OK", r#"[{"tag_name":"v0.2.0-alpha.1"}]"#);
    assert_eq!(check(&f.url, RUNNING), UpdateOutcome::Current(RUNNING.into()));
}

#[test]
fn garbage_and_errors_fail_quietly() {
    assert_eq!(check(&fake("200 OK", "<html>not json</html>").url, RUNNING), UpdateOutcome::Failed);
    assert_eq!(check(&fake("200 OK", r#"{"message":"hi"}"#).url, RUNNING), UpdateOutcome::Failed);
    assert_eq!(check(&fake("403 Forbidden", r#"{"message":"rate limit"}"#).url, RUNNING), UpdateOutcome::Failed);
    // Nothing listening at all (a port that was just free).
    let gone = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/releases", gone.local_addr().unwrap());
    drop(gone);
    assert_eq!(check(&url, RUNNING), UpdateOutcome::Failed);
}

fn studio(url: &str) -> StudioApp {
    let services = Services { update_check: Some(service(url.to_string(), RUNNING, egui::Context::default())), ..Services::default() };
    StudioApp::new(soundcraft_engine::Engine::default(), None, services)
}

fn settle(app: &mut StudioApp) {
    for _ in 0..500 {
        app.poll_update();
        if !app.update.checking() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the check did not finish");
}

#[test]
fn help_check_for_updates_reports_each_result() {
    let found = fake("200 OK", NEWER);
    let mut app = studio(&found.url);
    app.run("help.check_updates", serde_json::json!({})).unwrap();
    assert_eq!(app.status, "Checking for updates…");
    settle(&mut app);
    assert_eq!(app.status, "transit v0.2.0-alpha.2 is available");
    assert_eq!(app.update.notice.as_ref().unwrap().version, "0.2.0-alpha.2");
    assert!(app.ui.last_update_check > 0);

    let none = fake("200 OK", r#"[{"tag_name":"v0.2.0-alpha.1"}]"#);
    let mut app = studio(&none.url);
    app.run("help.check_updates", serde_json::json!({})).unwrap();
    settle(&mut app);
    assert_eq!(app.status, "You are on the latest version, v0.2.0-alpha.1");
    assert!(app.update.notice.is_none());

    let bad = fake("500 Internal Server Error", "oops");
    let mut app = studio(&bad.url);
    app.run("help.check_updates", serde_json::json!({})).unwrap();
    settle(&mut app);
    assert_eq!(app.status, "Could not reach GitHub");
}

#[test]
fn the_manual_check_ignores_the_preference() {
    let f = fake("200 OK", NEWER);
    let mut app = studio(&f.url);
    app.ui.check_for_updates = false;
    app.run("help.check_updates", serde_json::json!({})).unwrap();
    settle(&mut app);
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn the_launch_check_runs_once_when_due_and_stays_quiet_on_failure() {
    let ctx = egui::Context::default();
    let f = fake("200 OK", NEWER);
    let mut app = studio(&f.url);
    app.update_tick(&ctx);
    settle(&mut app);
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    assert!(app.update.notice.is_some());
    app.update_tick(&ctx);
    assert!(!app.update.checking(), "once per run");

    let bad = fake("200 OK", "garbage");
    let mut app = studio(&bad.url);
    app.update_tick(&ctx);
    settle(&mut app);
    assert!(app.status.is_empty(), "a failed launch check says nothing: {}", app.status);
}

#[test]
fn no_request_when_the_preference_is_off_or_it_ran_today_or_the_splash_shows() {
    let ctx = egui::Context::default();
    let f = fake("200 OK", NEWER);
    let mut off = studio(&f.url);
    off.ui.check_for_updates = false;
    off.update_tick(&ctx);
    let mut recent = studio(&f.url);
    recent.ui.last_update_check = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    recent.update_tick(&ctx);
    let mut splash = studio(&f.url);
    splash.splash.visible = true;
    splash.update_tick(&ctx);
    for app in [&off, &recent, &splash] {
        assert!(!app.update.checking());
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(f.hits.load(Ordering::SeqCst), 0, "the fake endpoint saw a connection");
}

#[test]
fn the_environment_switch_leaves_no_service_and_disables_the_command() {
    let services = Services { update_off_by_env: true, ..Services::default() };
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, services);
    assert_eq!(app.update_check_unavailable(), Some(update::DISABLED_BY_ENV));
    assert_eq!(app.run("help.check_updates", serde_json::json!({})), Err(update::DISABLED_BY_ENV.to_string()));
    app.update_tick(&egui::Context::default());
    assert!(!app.update.checking());
}
