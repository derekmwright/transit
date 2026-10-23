//! ADR 0018: the caller (standing in for the UI frame loop) never waits for plug-in code. Each test
//! polls once per "frame" and checks that no frame takes long while a fake plug-in sleeps in its
//! creation, destruction, or forever.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Fake, Log};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use transit_plugin_host::{Config, Host, Status};

/// A frame may take this long at most. Load-tolerant: a starved runner can stall a poll for a
/// while, but a frame that waited for the plug-in takes its full 1 s sleep.
const FRAME_BUDGET: Duration = Duration::from_millis(400);
/// A fake plug-in's slow creation or destruction: well past the frame budget.
const SLOW: Duration = Duration::from_millis(1000);

fn host(timeout: Duration) -> Host {
    Host::new(Config { timeout, inline: false, ..Config::default() })
}

/// Polls `pending` once per 5 ms frame until it is not waiting; returns the status, the frames
/// that ran meanwhile and the longest frame.
fn frames<T>(pending: &transit_plugin_host::Pending<T>, limit: Duration) -> (Status<T>, u32, Duration) {
    let (start, mut n, mut worst) = (Instant::now(), 0u32, Duration::ZERO);
    loop {
        let t = Instant::now();
        let status = pending.poll();
        worst = worst.max(t.elapsed());
        n += 1;
        if !matches!(status, Status::Waiting { .. }) || start.elapsed() > limit {
            return (status, n, worst);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn frames_advance_while_a_slow_instance_is_created_on_the_host_thread() {
    let host = host(Duration::from_secs(10));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let pending = host.submit(move |ctx| {
        let p = Fake::make(&l, SLOW, Duration::ZERO);
        ctx.adopt(p)
    });
    assert!(matches!(pending.poll(), Status::Waiting { .. }), "the first frame sees it loading");
    let (status, n, worst) = frames(&pending, Duration::from_secs(5));
    let Status::Ready(plugin) = status else { panic!("not made") };
    assert!(n >= 10, "only {n} frames ran during a 1 s creation");
    assert!(worst < FRAME_BUDGET, "a frame took {worst:?}");
    assert_ne!(log.thread_of("create"), Some(std::thread::current().id()), "made on the host thread");
    drop(plugin);
    assert!(host.shutdown(Duration::from_secs(5)));
}

#[test]
fn a_slow_destruction_runs_on_the_host_thread_and_the_drop_returns_at_once() {
    let host = host(Duration::from_secs(10));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let plugin = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, SLOW))).wait(Duration::from_secs(5)).unwrap();
    let t = Instant::now();
    drop(plugin);
    assert!(t.elapsed() < FRAME_BUDGET, "the drop waited {:?} for the plug-in", t.elapsed());
    let deadline = Instant::now() + Duration::from_secs(5);
    while log.thread_of("destroy").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(log.thread_of("destroy"), log.thread_of("create"), "destroyed on the thread that made it");
    assert!(host.shutdown(Duration::from_secs(5)));
}

#[test]
fn a_creation_that_never_returns_is_stuck_after_the_timeout_and_never_blocks_a_frame() {
    let host = host(Duration::from_millis(300));
    let (release, hang) = mpsc::channel::<()>();
    let stuck = host.submit(move |_| {
        let _ = hang.recv(); // "never" returns: until the test ends
    });
    // Queued behind the stuck one before anyone knew: it moves to the fresh thread.
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let behind = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, Duration::ZERO)));
    let (status, n, worst) = frames(&stuck, Duration::from_secs(5));
    assert!(matches!(status, Status::Stuck), "the hung creation reads as stuck, got {status:?}");
    assert!(n >= 10, "frames ran meanwhile ({n})");
    assert!(worst < FRAME_BUDGET, "a frame took {worst:?}");
    assert_eq!(host.wedged(), 1);
    let (status, _, _) = frames(&behind, Duration::from_secs(5));
    assert!(matches!(status, Status::Ready(_)), "the queued request ran on the fresh thread");
    // New requests go to the fresh thread too.
    let fresh = host.submit(|_| std::thread::current().id()).wait(Duration::from_secs(5)).unwrap();
    assert_eq!(Some(fresh), log.thread_of("create"));
    // Shutdown does not wait for the abandoned thread.
    let t = Instant::now();
    assert!(host.shutdown(Duration::from_secs(2)));
    assert!(t.elapsed() < Duration::from_secs(2));
    drop(release);
}

#[test]
fn state_restore_and_save_round_trip_through_the_host_thread() {
    let host = host(Duration::from_secs(10));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let mut plugin = host
        .submit(move |ctx| {
            let mut p = Fake::make(&l, Duration::ZERO, Duration::ZERO);
            assert!(p.load_state(b"stored"));
            ctx.adopt(p)
        })
        .wait(Duration::from_secs(5))
        .unwrap();
    let mut reader = plugin.state_reader().expect("a state reader");
    assert_eq!(reader.save_state().as_deref(), Some(&b"stored"[..]));
    let host_thread = log.thread_of("create");
    assert_eq!(log.thread_of("load_state"), host_thread);
    assert_eq!(log.thread_of("save_state"), host_thread, "the state was read on the host thread");
    assert_ne!(host_thread, Some(std::thread::current().id()));
    drop((reader, plugin));
    assert!(host.shutdown(Duration::from_secs(5)));
}

#[test]
fn a_state_read_from_a_wedged_thread_answers_at_once() {
    let host = host(Duration::from_millis(200));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let mut plugin = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, Duration::ZERO))).wait(Duration::from_secs(5)).unwrap();
    let (release, hang) = mpsc::channel::<()>();
    // A hang on the instance's own thread (another job, a window message, a tick).
    let stuck = host.submit(move |_| {
        let _ = hang.recv();
    });
    let (status, _, _) = frames(&stuck, Duration::from_secs(5));
    assert!(matches!(status, Status::Stuck));
    let t = Instant::now();
    assert_eq!(plugin.state_reader().unwrap().save_state(), None);
    assert!(t.elapsed() < FRAME_BUDGET, "the wedged thread was waited for");
    let mut editor = plugin.editor().unwrap();
    assert!(editor.open().unwrap_err().contains("not responding"));
    drop(release);
    drop((editor, plugin));
    assert!(host.shutdown(Duration::from_secs(5)));
}

#[test]
fn editor_requests_run_on_the_host_thread_and_report_back() {
    let host = host(Duration::from_secs(10));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let mut plugin = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, Duration::ZERO))).wait(Duration::from_secs(5)).unwrap();
    let mut editor = plugin.editor().expect("an editor");
    let t = Instant::now();
    editor.open().unwrap();
    assert!(t.elapsed() < FRAME_BUDGET);
    assert!(editor.is_open(), "reads open while the request is on its way");
    // The host thread's tick idles the editor; its edit comes back through `idle`.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut edits = Vec::new();
    while edits.is_empty() && Instant::now() < deadline {
        edits = editor.idle();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(edits, vec![("gain".to_string(), 0.25)]);
    editor.set_param("gain", 0.5);
    editor.close();
    assert!(!editor.is_open());
    let host_thread = log.thread_of("create");
    let deadline = Instant::now() + Duration::from_secs(5);
    while log.thread_of("editor_close").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    for call in ["editor_open", "editor_idle", "editor_set_param", "editor_close"] {
        assert_eq!(log.thread_of(call), host_thread, "{call} ran on the host thread");
    }
    assert_eq!(*log.editor_params.lock().unwrap(), vec![("gain".to_string(), 0.5)]);
    assert!(!editor.is_open(), "a tick does not reopen a closed editor");
    drop((editor, plugin));
    assert!(host.shutdown(Duration::from_secs(5)));
}

#[test]
fn an_editor_that_fails_to_open_reports_why_once() {
    let host = host(Duration::from_secs(10));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let mut plugin = host
        .submit(move |ctx| {
            let p = Box::new(Fake { log: l, destroy: Duration::ZERO, editor_fails: true });
            ctx.adopt(p)
        })
        .wait(Duration::from_secs(5))
        .unwrap();
    let mut editor = plugin.editor().unwrap();
    editor.open().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut error = None;
    while error.is_none() && Instant::now() < deadline {
        error = editor.take_error();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(error.as_deref(), Some("no window today"));
    assert!(!editor.is_open());
    assert_eq!(editor.take_error(), None, "reported once");
    drop((editor, plugin));
    assert!(host.shutdown(Duration::from_secs(5)));
}

#[test]
fn a_request_dropped_before_it_starts_never_runs() {
    let host = host(Duration::from_secs(10));
    let (release, hang) = mpsc::channel::<()>();
    let busy = host.submit(move |_| {
        let _ = hang.recv();
    });
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let cancelled = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, Duration::ZERO)));
    drop(cancelled);
    drop(release);
    assert!(matches!(frames(&busy, Duration::from_secs(5)).0, Status::Ready(())));
    host.submit(|_| ()).wait(Duration::from_secs(5)).unwrap();
    assert_eq!(log.thread_of("create"), None, "the cancelled creation never ran");
    assert!(host.shutdown(Duration::from_secs(5)));
}

#[test]
fn inline_mode_runs_on_the_calling_thread_and_keeps_the_instance_unwrapped() {
    let host = Host::new(Config { inline: true, ..Config::default() });
    assert!(host.is_inline());
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let pending = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, Duration::ZERO)));
    let Status::Ready(mut plugin) = pending.poll() else { panic!("inline jobs finish at once") };
    assert_eq!(log.thread_of("create"), Some(std::thread::current().id()));
    plugin.editor().unwrap().open().unwrap();
    assert_eq!(log.thread_of("editor_open"), Some(std::thread::current().id()), "the editor is the plug-in's own");
    drop(plugin);
    assert_eq!(log.thread_of("destroy"), Some(std::thread::current().id()));
}

/// Issue #36: closing the app while a plug-in is being created does not hang. The creation is
/// not stuck yet (the timeout is long), and shutdown still returns within its own bound.
#[test]
fn shutdown_during_a_creation_returns_within_its_bound() {
    let host = host(Duration::from_secs(60));
    let (release, hang) = mpsc::channel::<()>();
    let creating = host.submit(move |_| {
        let _ = hang.recv();
    });
    while !matches!(creating.poll(), Status::Waiting { running: true }) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let t = Instant::now();
    assert!(!host.shutdown(Duration::from_millis(500)), "the busy thread did not finish");
    assert!(t.elapsed() < Duration::from_secs(2), "shutdown waited {:?}", t.elapsed());
    assert!(matches!(host.submit(|_| ()).poll(), Status::Failed), "no new requests after shutdown");
    drop(release);
}

/// B1 of the ADR 0018 review: a thread that was slow, not stuck (a 12 s sampler restore, a modal
/// dialog), is responsive again when its call returns. Its instances' live states are read and
/// their editors open; new creations stay on the thread that took over.
#[test]
fn a_thread_that_returns_after_the_timeout_serves_its_instances_again() {
    let host = host(Duration::from_millis(300));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let mut plugin = host
        .submit(move |ctx| {
            let mut p = Fake::make(&l, Duration::ZERO, Duration::ZERO);
            assert!(p.load_state(b"live"));
            ctx.adopt(p)
        })
        .wait(Duration::from_secs(5))
        .unwrap();
    let old = log.thread_of("create");
    let long = host.submit(|_| std::thread::sleep(Duration::from_millis(900)));
    let (status, _, _) = frames(&long, Duration::from_secs(5));
    assert!(matches!(status, Status::Stuck), "past the timeout it reads stuck, got {status:?}");
    assert_eq!(host.wedged(), 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.wedged() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(host.wedged(), 0, "the thread returned: it is no longer wedged");
    assert_eq!(host.wedges(), 1);
    // The live state, read on the old thread (a save after this carries it, not the stored one).
    *log.state.lock().unwrap() = b"edited after the stall".to_vec();
    assert_eq!(plugin.state_reader().unwrap().save_state().as_deref(), Some(&b"edited after the stall"[..]));
    assert_eq!(log.calls.lock().unwrap().iter().rev().find(|(c, _)| *c == "save_state").map(|(_, t)| *t), old);
    let mut editor = plugin.editor().unwrap();
    editor.open().expect("the editor opens again");
    let deadline = Instant::now() + Duration::from_secs(5);
    while log.thread_of("editor_open").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(log.thread_of("editor_open"), old);
    assert!(editor.is_open());
    // New creations go to the thread that took over.
    let fresh = host.submit(|_| std::thread::current().id()).wait(Duration::from_secs(5)).unwrap();
    assert_ne!(Some(fresh), old);
    drop((editor, plugin));
    assert!(host.shutdown(Duration::from_secs(5)));
}

/// An editor open queued behind a call that then wedges its thread does not read open forever;
/// it opens once the thread returns.
#[test]
fn an_open_queued_on_a_thread_that_wedges_is_not_reported_open_until_it_runs() {
    let host = host(Duration::from_millis(300));
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let mut plugin = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, Duration::ZERO))).wait(Duration::from_secs(5)).unwrap();
    let (release, hang) = mpsc::channel::<()>();
    let stuck = host.submit(move |_| {
        let _ = hang.recv();
    });
    while !matches!(stuck.poll(), Status::Waiting { running: true }) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut editor = plugin.editor().unwrap();
    editor.open().unwrap();
    assert!(editor.is_open(), "on its way");
    assert!(matches!(frames(&stuck, Duration::from_secs(5)).0, Status::Stuck));
    assert!(!editor.is_open(), "a wedged thread's pending open does not read open");
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !editor.is_open() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(editor.is_open(), "it opens when the thread returns");
    drop((editor, plugin));
    assert!(host.shutdown(Duration::from_secs(5)));
}
