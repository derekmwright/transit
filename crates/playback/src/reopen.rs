//! Automatic reopen of the audio device after its driver asked for it (native ASIO slice d1).
//!
//! An ASIO driver whose settings moved under a running stream (a buffer size or a rate changed in
//! its panel) asks the host to close and reopen it (`kAsioResetRequest`); a driver that stopped
//! calling back is caught by the backend's watchdog and reopened the same way. The Player reports that
//! ([`crate::Player::reopen_cause`]); the app stops the transport (a take in progress ends there and
//! is kept, never spliced onto the reopened stream), then reopens the device with the same
//! configuration through its one reopen path. [`Reopener`] decides when: a short wait so the
//! driver settles, at most [`REOPEN_ATTEMPTS`] attempts with growing waits ([`REOPEN_BACKOFF`]),
//! nothing while the driver's settings panel holds the driver, and after the last failure the
//! user reconnects by hand, as before. It is plain logic over a caller-supplied clock (seconds),
//! so its tests use a fake one.

/// Why the device must be reopened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReopenCause {
    /// The driver asked to be reset (ASIO `kAsioResetRequest`, or its rate changed under the stream).
    DriverReset,
    /// The driver stopped calling back while the stream should run (the ASIO watchdog).
    DriverStalled,
}

impl ReopenCause {
    /// What happened, for the status line: "driver reset", "driver stopped calling back".
    pub fn text(self) -> &'static str {
        match self {
            Self::DriverReset => "driver reset",
            Self::DriverStalled => "driver stopped calling back",
        }
    }
}

/// Attempts before the user is asked to reconnect.
pub const REOPEN_ATTEMPTS: u32 = 3;
/// Seconds to wait before each attempt: the first lets the driver settle after its change.
pub const REOPEN_BACKOFF: [f64; REOPEN_ATTEMPTS as usize] = [0.25, 1.0, 3.0];
/// Seconds a reopened device must run without another request before the attempts count afresh;
/// a driver that keeps asking within this time runs out of attempts instead of looping.
pub const REOPEN_SETTLE: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// The next attempt is due at this time.
    Waiting(f64),
    /// The attempts ran out; the user reconnects.
    GaveUp,
}

/// When to reopen the device after a driver reset, and what to say about it.
#[derive(Debug, Clone)]
pub struct Reopener {
    phase: Phase,
    /// Attempts since the device last ran [`REOPEN_SETTLE`] seconds without a request.
    streak: u32,
    last_success: Option<f64>,
    cause: Option<ReopenCause>,
    resets: u64,
    stalls: u64,
    reopened: u64,
}

impl Default for Reopener {
    fn default() -> Self {
        Self { phase: Phase::Idle, streak: 0, last_success: None, cause: None, resets: 0, stalls: 0, reopened: 0 }
    }
}

impl Reopener {
    /// The running device reported `cause` at `now`. True when this starts a new incident (an
    /// attempt is scheduled, or the attempts already ran out); false while one is under way.
    pub fn trouble(&mut self, cause: ReopenCause, now: f64) -> bool {
        if matches!(self.phase, Phase::Waiting(_) | Phase::GaveUp) {
            return false;
        }
        if self.last_success.is_some_and(|at| now - at >= REOPEN_SETTLE) {
            self.streak = 0;
        }
        self.cause = Some(cause);
        match cause {
            ReopenCause::DriverReset => self.resets += 1,
            ReopenCause::DriverStalled => self.stalls += 1,
        }
        self.phase = self.next(now);
        true
    }

    fn next(&self, now: f64) -> Phase {
        match REOPEN_BACKOFF.get(self.streak as usize) {
            Some(wait) => Phase::Waiting(now + wait),
            None => Phase::GaveUp,
        }
    }

    /// Whether an attempt is due at `now`. Never while `blocked` (the driver's settings panel is
    /// open and holds the driver): the attempt waits for it, and is not used up.
    pub fn due(&self, now: f64, blocked: bool) -> bool {
        matches!(self.phase, Phase::Waiting(at) if now >= at) && !blocked
    }

    /// The attempt that was due ran at `now`; `opened` says whether the device runs again.
    pub fn attempted(&mut self, opened: bool, now: f64) {
        if !matches!(self.phase, Phase::Waiting(_)) {
            return;
        }
        self.streak += 1;
        if opened {
            self.phase = Phase::Idle;
            self.reopened += 1;
            self.last_success = Some(now);
        } else {
            self.phase = self.next(now);
        }
    }

    /// The user reopened the device (Reconnect, Apply): any incident is over and the attempts
    /// count afresh.
    pub fn reconnected(&mut self) {
        self.phase = Phase::Idle;
        self.streak = 0;
        self.last_success = None;
    }

    /// Whether an incident is under way (an attempt is scheduled).
    pub fn pending(&self) -> bool {
        matches!(self.phase, Phase::Waiting(_))
    }

    /// Whether the attempts ran out and the user must reconnect.
    pub fn gave_up(&self) -> bool {
        self.phase == Phase::GaveUp
    }

    /// The attempt that runs next (1-based), while one is scheduled.
    pub fn attempt(&self) -> Option<u32> {
        self.pending().then_some(self.streak + 1)
    }

    /// Driver reset requests seen.
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// Stalls the backend's watchdog reported (counted apart from resets).
    pub fn stalls(&self) -> u64 {
        self.stalls
    }

    /// The cause of the last incident.
    pub fn cause(&self) -> Option<ReopenCause> {
        self.cause
    }

    /// Automatic reopens that succeeded.
    pub fn reopened(&self) -> u64 {
        self.reopened
    }

    /// The line for the Audio Devices window: what happened and what happens next; `None` before
    /// any request. `blocked` as in [`Self::due`].
    pub fn status(&self, blocked: bool) -> Option<String> {
        let what = self.cause?.text();
        let counts = format!("resets {}, stalls {}, reopened {}", self.resets, self.stalls, self.reopened);
        let state = match self.phase {
            Phase::Waiting(_) if blocked => "waiting for the driver settings panel to close, then reopening".to_owned(),
            Phase::Waiting(_) => format!("reopening (attempt {} of {REOPEN_ATTEMPTS})", self.streak + 1),
            Phase::GaveUp => format!("the device did not reopen after {REOPEN_ATTEMPTS} attempts; use Reconnect"),
            Phase::Idle if self.reopened > 0 => "reopened automatically".to_owned(),
            Phase::Idle => "reconnected".to_owned(),
        };
        Some(format!("Recovery: {what}, {state} ({counts})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESET: ReopenCause = ReopenCause::DriverReset;

    #[test]
    fn a_reset_waits_for_the_driver_then_reopens_once() {
        let mut r = Reopener::default();
        assert_eq!(r.status(false), None);
        assert!(r.trouble(RESET, 10.0));
        assert!(!r.trouble(RESET, 10.01), "the other direction's report is the same incident");
        assert_eq!(r.resets(), 1);
        assert!(!r.due(10.2, false), "the driver settles first");
        assert!(r.due(10.25, false));
        assert_eq!(r.attempt(), Some(1));
        assert_eq!(r.status(false).unwrap(), "Recovery: driver reset, reopening (attempt 1 of 3) (resets 1, stalls 0, reopened 0)");
        r.attempted(true, 10.3);
        assert!(!r.pending() && !r.due(20.0, false));
        assert_eq!(r.reopened(), 1);
        assert_eq!(r.status(false).unwrap(), "Recovery: driver reset, reopened automatically (resets 1, stalls 0, reopened 1)");
    }

    #[test]
    fn failed_attempts_back_off_then_give_up_after_three() {
        let mut r = Reopener::default();
        r.trouble(RESET, 0.0);
        assert!(r.due(0.25, false));
        r.attempted(false, 0.25);
        assert!(!r.due(1.0, false) && r.due(1.25, false), "one second after the first failure");
        assert_eq!(r.attempt(), Some(2));
        r.attempted(false, 1.25);
        assert!(!r.due(4.0, false) && r.due(4.25, false), "three seconds after the second");
        r.attempted(false, 4.25);
        assert!(r.gave_up() && !r.due(100.0, false));
        assert!(!r.trouble(RESET, 5.0), "nothing more until the user reconnects");
        assert_eq!(
            r.status(false).unwrap(),
            "Recovery: driver reset, the device did not reopen after 3 attempts; use Reconnect (resets 1, stalls 0, reopened 0)"
        );
        r.reconnected();
        assert!(r.trouble(RESET, 50.0) && r.due(50.25, false), "a reconnect starts the count afresh");
    }

    #[test]
    fn an_open_settings_panel_holds_the_attempt_without_using_it() {
        let mut r = Reopener::default();
        r.trouble(RESET, 0.0);
        assert!(!r.due(5.0, true));
        assert_eq!(
            r.status(true).unwrap(),
            "Recovery: driver reset, waiting for the driver settings panel to close, then reopening (resets 1, stalls 0, reopened 0)"
        );
        assert!(r.due(5.0, false));
        assert_eq!(r.attempt(), Some(1), "still the first attempt");
    }

    #[test]
    fn a_driver_that_keeps_asking_runs_out_of_attempts_instead_of_looping() {
        let mut r = Reopener::default();
        for (i, at) in [0.0, 2.0, 6.0].into_iter().enumerate() {
            assert!(r.trouble(RESET, at));
            assert_eq!(r.attempt(), Some(i as u32 + 1));
            let due = at + REOPEN_BACKOFF[i];
            assert!(r.due(due, false));
            r.attempted(true, due);
        }
        assert!(r.trouble(RESET, 12.0));
        assert!(r.gave_up(), "a fourth request within the settle time is not retried");
        assert_eq!(r.reopened(), 3);
    }

    #[test]
    fn a_stall_takes_the_same_path_and_is_counted_apart() {
        let mut r = Reopener::default();
        assert!(r.trouble(ReopenCause::DriverStalled, 0.0));
        assert_eq!(r.cause(), Some(ReopenCause::DriverStalled));
        assert!(r.due(0.25, false));
        r.attempted(true, 0.25);
        assert_eq!((r.resets(), r.stalls(), r.reopened()), (0, 1, 1));
        assert_eq!(r.status(false).unwrap(), "Recovery: driver stopped calling back, reopened automatically (resets 0, stalls 1, reopened 1)");
    }

    #[test]
    fn a_device_that_ran_long_enough_starts_a_new_count() {
        let mut r = Reopener::default();
        for at in [0.0, 100.0, 200.0, 300.0] {
            r.trouble(RESET, at);
            assert_eq!(r.attempt(), Some(1));
            r.attempted(true, at + 0.25);
        }
        assert_eq!((r.resets(), r.reopened()), (4, 4));
    }
}
