//! Every other platform (and the `force-fallback` feature): no realtime mechanism.
//!
//! The thread is never changed. [`promote`] returns [`Promotion::Unsupported`] and logs that once
//! per process, at info level. No `unsafe` here. Compiled on every platform under `cfg(test)`,
//! so its tests run everywhere.

use crate::{Level, Promotion};
use std::sync::atomic::{AtomicBool, Ordering};

/// The reason [`promote`] gives.
pub(crate) const REASON: &str = "platform";

static LOGGED: AtomicBool = AtomicBool::new(false);

pub(crate) fn promote() -> Promotion {
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::info!("transit-rt: no realtime thread promotion on this platform; audio workers keep the default priority");
    }
    Promotion::Unsupported(REASON)
}

pub(crate) fn demote() {}

pub(crate) fn current_level() -> Option<Level> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_unsupported_and_unchanged() {
        assert_eq!(promote(), Promotion::Unsupported("platform"));
        assert_eq!(promote(), Promotion::Unsupported("platform"));
        assert!(LOGGED.load(Ordering::Relaxed));
        demote();
        assert_eq!(current_level(), None);
        assert!(!promote().is_promoted());
    }
}
