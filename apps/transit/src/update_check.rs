//! The new-version check: one anonymous GET of GitHub's public releases list, at most once a day
//! (the schedule is `transit_ui_studio::update`). Nothing is downloaded or installed. Offline and
//! API errors come back as `Failed` and are debug-logged only.
use semver::Version;
use std::sync::mpsc;
use std::time::Duration;
use transit_ui_studio::update::{UpdateCheck, UpdateNotice, UpdateOutcome};

/// The releases list (pre-releases included while the project is in alpha).
pub const RELEASES_URL: &str = "https://api.github.com/repos/transit-audio/transit/releases?per_page=5";

/// `TRANSIT_NO_UPDATE_CHECK=1` (any non-empty value other than `0`) turns the check off.
pub fn disabled_by_env() -> bool {
    disabled_by(std::env::var("TRANSIT_NO_UPDATE_CHECK").ok().as_deref())
}

fn disabled_by(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// "v0.2.0-alpha.2" or "0.2.0-alpha.2" as a version; `None` for anything else. Build metadata
/// (`+x`) is dropped: it never makes a release newer.
fn parse_tag(tag: &str) -> Option<Version> {
    let mut v = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    v.build = semver::BuildMetadata::EMPTY;
    Some(v)
}

/// The highest release newer than `current` in a releases-list answer. Drafts and malformed tags
/// are skipped; pre-release precedence is semver's.
pub fn newest_newer(current: &Version, releases: &serde_json::Value) -> Option<UpdateNotice> {
    let mut best: Option<(Version, UpdateNotice)> = None;
    for r in releases.as_array()? {
        if r.get("draft").and_then(|d| d.as_bool()) == Some(true) {
            continue;
        }
        let Some(tag) = r.get("tag_name").and_then(|t| t.as_str()) else { continue };
        let Some(v) = parse_tag(tag) else { continue };
        if v <= *current || best.as_ref().is_some_and(|(b, _)| *b >= v) {
            continue;
        }
        let url = r
            .get("html_url")
            .and_then(|u| u.as_str())
            .filter(|u| u.starts_with("https://"))
            .map_or_else(|| format!("https://github.com/transit-audio/transit/releases/tag/{tag}"), str::to_string);
        best = Some((v.clone(), UpdateNotice { version: v.to_string(), url }));
    }
    best.map(|(_, n)| n)
}

/// One request to `url`, judged against `running`.
pub fn check(url: &str, running: &str) -> UpdateOutcome {
    let Some(current) = parse_tag(running) else { return UpdateOutcome::Failed };
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("transit/", env!("CARGO_PKG_VERSION")))
        .build();
    let fetched = agent.get(url).set("Accept", "application/vnd.github+json").call().map_err(|e| e.to_string());
    let body = match fetched.and_then(|r| r.into_string().map_err(|e| e.to_string())) {
        Ok(b) => b,
        Err(e) => {
            log::debug!("update check: {e}");
            return UpdateOutcome::Failed;
        }
    };
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) if v.is_array() => match newest_newer(&current, &v) {
            Some(n) => UpdateOutcome::Available(n),
            None => UpdateOutcome::Current(current.to_string()),
        },
        _ => {
            log::debug!("update check: the answer was not a release list");
            UpdateOutcome::Failed
        }
    }
}

/// The service the studio calls: a thread per check, waking the UI when it is done.
pub fn service(url: String, running: &'static str, ctx: egui::Context) -> UpdateCheck {
    Box::new(move || {
        let (tx, rx) = mpsc::channel();
        let (url, ctx) = (url.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            let _ = tx.send(check(&url, running));
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            log::debug!("update check: {e}");
        }
        rx
    })
}

/// The studio's check for this run: `None` when the environment turns it off.
pub fn for_app(ctx: &egui::Context) -> Option<UpdateCheck> {
    (!disabled_by_env()).then(|| service(RELEASES_URL.to_string(), env!("CARGO_PKG_VERSION"), ctx.clone()))
}

#[cfg(test)]
mod tests;
