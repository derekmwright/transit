//! Promotion on real threads. Each test promotes a thread it spawned, so the test harness's own
//! threads are never left raised.

use transit_rt::{Promotion, current_level, demote, promote_audio_thread};

fn on_new_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().expect("test thread panicked")
}

/// On every platform: promote then demote leaves the thread as the OS reported it before, and
/// nothing panics, whatever the result.
#[test]
fn demote_restores_the_level_the_thread_had() {
    let (before, p, after) = on_new_thread(|| {
        let before = current_level();
        let p = promote_audio_thread();
        demote(p);
        (before, p, current_level())
    });
    assert_eq!(before, after, "promotion {p:?} was not undone");
}

/// A second promotion on the same thread returns the first result and makes no new OS call;
/// one demote undoes it.
#[test]
fn promote_twice_is_idempotent() {
    let (first, second, during, after, before) = on_new_thread(|| {
        let before = current_level();
        let first = promote_audio_thread();
        let second = promote_audio_thread();
        let during = current_level();
        demote(second);
        (first, second, during, current_level(), before)
    });
    assert_eq!(first, second);
    if first.is_promoted() {
        assert!(during.is_some_and(|l| l.realtime), "{first:?} but level {during:?}");
    }
    assert_eq!(before, after);
}

/// Demoting with a value that is not a promotion, or twice, does nothing.
#[test]
fn demote_without_promotion_is_a_no_op() {
    let (before, after) = on_new_thread(|| {
        let before = current_level();
        demote(Promotion::Unsupported("test"));
        demote(Promotion::Denied(5));
        let p = promote_audio_thread();
        demote(p);
        demote(p);
        (before, current_level())
    });
    assert_eq!(before, after);
}

/// Promotion is per thread: another thread's level does not change.
#[test]
fn promotion_does_not_touch_other_threads() {
    let mine = current_level();
    let p = on_new_thread(promote_audio_thread); // the thread exits promoted; the OS drops it
    assert_eq!(current_level(), mine, "{p:?} leaked to the calling thread");
}

#[cfg(all(windows, not(feature = "force-fallback")))]
mod windows {
    use super::*;

    /// MMCSS (or, without the service, time-critical priority) is available to any user on
    /// Windows, so promotion must succeed and the level must show it.
    #[test]
    fn windows_promotes_and_reports_realtime() {
        let (before, p, during, after) = on_new_thread(|| {
            let before = current_level();
            let p = promote_audio_thread();
            let during = current_level();
            demote(p);
            (before, p, during, current_level())
        });
        assert!(matches!(p, Promotion::Mmcss { .. } | Promotion::Priority(15)), "{p:?}");
        let before = before.expect("GetThreadPriority");
        let during = during.expect("GetThreadPriority");
        assert!(!before.realtime, "{before:?}");
        assert!(during.realtime, "{p:?} but {during:?}");
        // The OS itself reports the raise (MMCSS shows 16-26, time critical 15), not only our
        // own bookkeeping.
        assert!(during.priority >= 15 && during.priority > before.priority, "{p:?} but {during:?}");
        assert_eq!(after, Some(before));
    }
}

#[cfg(feature = "force-fallback")]
#[test]
fn forced_fallback_is_unsupported() {
    let p = on_new_thread(promote_audio_thread);
    assert_eq!(p, Promotion::Unsupported("platform"));
    assert_eq!(current_level(), None);
}

/// Linux without `CAP_SYS_NICE` or an `RLIMIT_RTPRIO` gets `Denied(EPERM)`; with them (CI runs
/// this suite again under `sudo -E` with `SOUNDCRAFT_RT_EXPECT_PROMOTED=1`) it gets `SCHED_FIFO`.
/// The test asserts the variant set, and success only when the environment says it must succeed.
#[cfg(all(target_os = "linux", not(feature = "force-fallback")))]
mod linux {
    use super::*;

    const EPERM: i32 = 1;

    #[test]
    fn linux_promotes_or_is_denied_never_anything_else() {
        let (p, during) = on_new_thread(|| {
            let p = promote_audio_thread();
            let during = current_level();
            demote(p);
            (p, during)
        });
        assert!(matches!(p, Promotion::Priority(_) | Promotion::Denied(EPERM) | Promotion::Unsupported(_)), "{p:?}");
        if p.is_promoted() {
            assert!(during.is_some_and(|l| l.realtime), "{p:?} but level {during:?}");
        }
        if std::env::var("SOUNDCRAFT_RT_EXPECT_PROMOTED").is_ok_and(|v| v == "1") {
            assert!(matches!(p, Promotion::Priority(_)), "expected SCHED_FIFO under sudo, got {p:?}");
        }
    }
}

/// macOS needs no privilege for the time-constraint policy; a refusal is still a value.
#[cfg(all(target_os = "macos", not(feature = "force-fallback")))]
mod macos {
    use super::*;

    #[test]
    fn macos_promotes_or_reports_why() {
        let (p, during) = on_new_thread(|| {
            let p = promote_audio_thread();
            let during = current_level();
            demote(p);
            (p, during)
        });
        assert!(matches!(p, Promotion::TimeConstraint { .. } | Promotion::Denied(_) | Promotion::Unsupported(_)), "{p:?}");
        if p.is_promoted() {
            assert!(during.is_some_and(|l| l.realtime), "{p:?} but level {during:?}");
        }
    }
}
