//! The start/stop state of one stream, kept apart from the COM objects so its rules are tested
//! without a device: a stream starts at most once and never after `stop` (a stopped stream is
//! reopened, not restarted). The stream thread reads exactly one `Start` command, so a second
//! command would otherwise wait for an answer that never comes.

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartError {
    AlreadyStarted,
    Stopped,
}

impl StartError {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::AlreadyStarted => "the WASAPI stream is already started",
            Self::Stopped => "the WASAPI stream has stopped; open it again",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Lifecycle {
    started: AtomicBool,
    stopped: AtomicBool,
}

impl Lifecycle {
    /// Claim the one start. Fails (immediately, without blocking) once started or stopped.
    pub(crate) fn begin_start(&self) -> Result<(), StartError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(StartError::Stopped);
        }
        if self.started.swap(true, Ordering::AcqRel) {
            return Err(StartError::AlreadyStarted);
        }
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_starts_once() {
        let life = Lifecycle::default();
        assert_eq!(life.begin_start(), Ok(()));
        assert_eq!(life.begin_start(), Err(StartError::AlreadyStarted));
    }

    #[test]
    fn start_after_stop_fails_whether_or_not_it_ever_started() {
        let never = Lifecycle::default();
        never.stop();
        assert_eq!(never.begin_start(), Err(StartError::Stopped));
        let running = Lifecycle::default();
        running.begin_start().unwrap();
        running.stop();
        assert_eq!(running.begin_start(), Err(StartError::Stopped));
    }
}
