//! The new-version notice. The check itself (one anonymous HTTPS request) lives in the app and is
//! handed in as [`Services::update_check`]; this module owns the state, the schedule (at most once
//! per 24 hours, after the splash), the result handling and the one-line notice.
use crate::StudioApp;
use std::sync::mpsc::{Receiver, TryRecvError};

/// How long a check result is good for.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Why the privacy sentence exists: shown by the Help menu entry and About.
pub const PRIVACY: &str = "The check is a single anonymous request to GitHub's public API; nothing else is sent.";
/// The tooltip of the disabled Help entry when the check is turned off by the environment.
pub const DISABLED_BY_ENV: &str = "Turned off by TRANSIT_NO_UPDATE_CHECK.";
/// The tooltip when this build has no update check.
pub const NOT_IN_BUILD: &str = "This build was made without the update-check feature.";

/// A release newer than the running version.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UpdateNotice {
    /// The version without the leading "v", for example `0.2.0-alpha.2`.
    pub version: String,
    /// The release page.
    pub url: String,
}

impl UpdateNotice {
    /// "transit v0.2.0-alpha.2 is available".
    pub fn line(&self) -> String {
        format!("transit v{} is available", self.version)
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Available(UpdateNotice),
    /// Nothing newer; the running version.
    Current(String),
    /// Offline, a rate limit, or an answer that was not release JSON.
    Failed,
}

/// Starts a check on its own thread and returns where the outcome arrives. Built by the app.
pub type UpdateCheck = Box<dyn Fn() -> Receiver<UpdateOutcome>>;

/// The state of the notice and of a check under way.
#[derive(Default)]
pub struct UpdateState {
    /// A newer release is known (from a check this run, or the last one).
    pub notice: Option<UpdateNotice>,
    rx: Option<Receiver<UpdateOutcome>>,
    /// The check under way was asked for from the menu (it reports every result).
    manual: bool,
    /// The launch check has been decided for this run.
    auto_done: bool,
}

impl UpdateState {
    pub fn checking(&self) -> bool {
        self.rx.is_some()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A launch check is due when none is recorded, the record is in the future (a clock that was set
/// forward and corrected, or a damaged file), or the last one is a day old.
pub fn check_due(last: u64, now: u64) -> bool {
    last == 0 || last > now || now - last >= CHECK_INTERVAL_SECS
}

impl StudioApp {
    /// Why Help > Check for Updates is unavailable, or `None` when it works.
    pub fn update_check_unavailable(&self) -> Option<&'static str> {
        if self.services.update_check.is_some() {
            None
        } else if self.services.update_off_by_env {
            Some(DISABLED_BY_ENV)
        } else {
            Some(NOT_IN_BUILD)
        }
    }

    /// Help > Check for Updates (and `help.check_updates`): runs now, whatever the preference says.
    pub fn check_for_updates_now(&mut self) -> Result<(), String> {
        if let Some(why) = self.update_check_unavailable() {
            self.status = why.into();
            return Err(why.into());
        }
        if self.update.checking() {
            return Ok(());
        }
        self.start_update_check(true);
        self.status = "Checking for updates…".into();
        Ok(())
    }

    fn start_update_check(&mut self, manual: bool) {
        let Some(check) = self.services.update_check.as_ref() else { return };
        self.update.rx = Some(check());
        self.update.manual = manual;
        self.ui.last_update_check = now();
    }

    /// Once a frame: start the launch check when it is due (after the splash, the preference on, no
    /// check in the last 24 hours) and take in an outcome that has arrived.
    pub fn update_tick(&mut self, ctx: &egui::Context) {
        if !self.update.auto_done && !self.splash.visible && self.services.update_check.is_some() {
            self.update.auto_done = true;
            if self.ui.check_for_updates && check_due(self.ui.last_update_check, now()) {
                self.start_update_check(false);
            }
        }
        if self.update.checking() {
            self.poll_update();
            if self.update.checking() {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
        }
    }

    /// Take in the outcome of a check if it has arrived. Offline and API errors stay quiet unless
    /// the check was asked for from the menu.
    pub fn poll_update(&mut self) {
        let Some(rx) = self.update.rx.as_ref() else { return };
        let outcome = match rx.try_recv() {
            Ok(o) => o,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => UpdateOutcome::Failed,
        };
        self.update.rx = None;
        let manual = std::mem::take(&mut self.update.manual);
        match outcome {
            UpdateOutcome::Available(n) => {
                self.status = n.line();
                self.update.notice = Some(n);
            }
            UpdateOutcome::Current(v) => {
                self.update.notice = None;
                if manual {
                    self.status = format!("You are on the latest version, v{v}");
                }
            }
            UpdateOutcome::Failed => {
                log::debug!("update check failed");
                if manual {
                    self.status = "Could not reach GitHub".into();
                }
            }
        }
    }
}

/// The one-line notice with its action; used by the status bar and About.
pub fn notice_line(ui: &mut egui::Ui, n: &UpdateNotice) {
    ui.label(egui::RichText::new(n.line()).color(crate::theme::TOKENS.text).size(11.5));
    ui.hyperlink_to("Open release page", &n.url);
}
