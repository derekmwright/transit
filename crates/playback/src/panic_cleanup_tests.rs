//! Regression tests derived from independent ADR 0009 fault-cleanup probes.
//! No test opens an audio device.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use soundcraft_model::{ChannelFormat, Insert};
use soundcraft_rt::testing::{AllocStats, measure};
use std::sync::atomic::AtomicUsize;
use std::thread::ThreadId;

type Log = Arc<Mutex<[Option<ThreadId>; 3]>>;

struct Payload {
    log: Log,
    index: usize,
    panics: bool,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.log.lock().unwrap()[self.index] = Some(std::thread::current().id());
        if self.panics {
            panic!("review payload destructor panic");
        }
    }
}

struct Adversary {
    inner: Box<dyn Plugin>,
    first: Option<Box<dyn std::any::Any + Send>>,
    second: Option<Box<dyn std::any::Any + Send>>,
    calls: Arc<AtomicUsize>,
    log: Log,
    drop_panics: bool,
}

impl Plugin for Adversary {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
        self.inner.prepare(sr, mb, ch);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn set_param(&mut self, id: &str, v: f32) -> bool {
        self.inner.set_param(id, v)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, _: &mut [Vec<f32>], _: usize) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let payload = self.first.take().or_else(|| self.second.take()).expect("unexpected third process");
        std::panic::resume_unwind(payload);
    }
}

impl Drop for Adversary {
    fn drop(&mut self) {
        self.log.lock().unwrap()[2] = Some(std::thread::current().id());
        if self.drop_panics {
            panic!("review plugin destructor panic");
        }
    }
}

fn rig(payload_panics: bool, plugin_panics: bool) -> (LegacyPlayer, Log, Arc<AtomicUsize>) {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    session.track_mut(track).unwrap().instrument = Some(Insert::new("clap:review.panic"));
    let session = Arc::new(session);
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicU32::new(1000),
        meters: Mutex::new(MeterSnapshot::default()),
        editors_open: AtomicBool::new(false),
        metro: crate::Metro::default(),
    });
    let (tx, rx) = channel();
    let (gtx, grx) = channel();
    let state = Arc::new(Mutex::new(Some(AudioState::new(rx, Arc::clone(&shared), Arc::clone(&session), 48_000.0, gtx))));
    let log: Log = Arc::new(Mutex::new([None; 3]));
    let calls = Arc::new(AtomicUsize::new(0));
    let plugin = Adversary {
        inner: soundcraft_dsp::create("gain").unwrap(),
        first: Some(Box::new(Payload { log: Arc::clone(&log), index: 0, panics: payload_panics })),
        second: Some(Box::new(Payload { log: Arc::clone(&log), index: 1, panics: false })),
        calls: Arc::clone(&calls),
        log: Arc::clone(&log),
        drop_panics: plugin_panics,
    };
    tx.send(Cmd::Session(
        session,
        vec![PreparedInstance { track, slot: soundcraft_mix::INSTRUMENT_SLOT, id: "clap:review.panic".into(), ch: 2, plugin: Box::new(plugin) }],
    ))
    .unwrap();
    tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    state.lock().unwrap().as_mut().unwrap().drain();
    let player = LegacyPlayer::silent(shared, Arc::new(Fault::default()), tx, Mutex::new(Side::new(48_000.0, grx)), state, 48_000);
    (player, log, calls)
}

fn callbacks(player: &LegacyPlayer, count: usize, device_error: bool) -> AllocStats {
    let (held, fault, shared) = (Arc::clone(&player.state), Arc::clone(&player.fault), Arc::clone(&player.shared));
    std::thread::spawn(move || {
        let mut output = OutputCallback::new(2, 48_000);
        let mut out = [0.75f32; 32];
        output.fill(&mut out, &held, &fault, &shared, || {});
        assert_eq!(out, [0.0; 32], "the faulting callback must be silent");
        assert!(fault.failed.load(Ordering::Relaxed));
        {
            let locked = held.lock().unwrap();
            assert!(locked.as_ref().unwrap().panic_payload.is_some(), "first rethrown payload must stay with AudioState");
        }
        let ((), stats) = measure(|| {
            for _ in 1..count {
                out.fill(0.75);
                output.fill(&mut out, &held, &fault, &shared, || {});
                assert_eq!(out, [0.0; 32]);
            }
        });
        if device_error {
            output_error_callback(Arc::clone(&fault))(cpal::StreamError::DeviceNotAvailable);
            assert!(fault.gone.load(Ordering::Relaxed));
        }
        stats
    })
    .join()
    .unwrap()
}

#[test]
fn successive_callbacks_keep_first_payload_and_never_throw_second() {
    let (player, log, calls) = rig(false, false);
    let stats = callbacks(&player, 100, false);
    assert_eq!(calls.load(Ordering::Relaxed), 1, "second panic must never be reached");
    assert_eq!(*log.lock().unwrap(), [None; 3]);
    assert!(stats.is_zero(), "idle fault callbacks: {stats:?}");
    assert!(!player.shared.playing.load(Ordering::Relaxed));
    drop(player);
    assert_eq!(*log.lock().unwrap(), [Some(std::thread::current().id()); 3]);
}

#[test]
fn error_then_no_more_callbacks_keeps_both_owners_until_control_drop() {
    for gone in [false, true] {
        let (player, log, _) = rig(false, false);
        callbacks(&player, 1, gone);
        assert_eq!(*log.lock().unwrap(), [None; 3]);
        assert_eq!(player.trouble(), Some(OutputTrouble::Bug));
        drop(player);
        assert_eq!(*log.lock().unwrap(), [Some(std::thread::current().id()); 3]);
    }
}

#[test]
fn payload_drop_must_not_escape_player_teardown() {
    let (player, log, _) = rig(true, false);
    callbacks(&player, 2, false);
    assert_eq!(*log.lock().unwrap(), [None; 3]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(player)));
    assert_eq!(*log.lock().unwrap(), [Some(std::thread::current().id()); 3]);
    assert!(result.is_ok(), "retained payload destructor panic escaped LegacyPlayer::drop");
}

#[test]
fn queued_session_after_fault_must_not_allocate() {
    let (player, log, _) = rig(false, false);
    callbacks(&player, 1, false);
    let session = Arc::new(Session::default());
    player.tx.send(Cmd::Session(Arc::clone(&session), Vec::new())).unwrap();
    let (held, fault, shared) = (Arc::clone(&player.state), Arc::clone(&player.fault), Arc::clone(&player.shared));
    let stats = std::thread::spawn(move || {
        let mut out = [0.75; 32];
        let (_, stats) = measure(|| render_held(&held, &fault, &shared, &mut out, 2));
        assert_eq!(out, [0.0; 32]);
        stats
    })
    .join()
    .unwrap();
    assert_eq!(*log.lock().unwrap(), [None; 3]);
    drop(player);
    assert_eq!(*log.lock().unwrap(), [Some(std::thread::current().id()); 3]);
    assert!(stats.is_zero(), "postfault empty Session command: {stats:?}");
}

#[test]
fn teardown_unwind_child() {
    let Ok(case) = std::env::var("SOUNDCRAFT_LEGACY_CLEANUP_CHILD") else { return };
    let (player, _, _) = rig(true, case == "two-destructors");
    callbacks(&player, 2, false);
    std::panic::set_hook(Box::new(|p| eprintln!("child panic: {p}")));
    if case == "outer-unwind" {
        let _player = player;
        panic!("review unrelated control panic");
    }
    drop(player);
}

#[test]
fn payload_cleanup_does_not_double_panic_during_other_unwinds() {
    for case in ["outer-unwind", "two-destructors"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "panic_cleanup_tests::teardown_unwind_child", "--nocapture"])
            .env("SOUNDCRAFT_LEGACY_CLEANUP_CHILD", case)
            .env("RUST_BACKTRACE", "0")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!("{case}: status={}; {stderr}", out.status);
        assert_eq!(out.status.code(), Some(101), "child must report an ordinary test panic, never abort: {stderr}");
        assert!(stderr.contains("review payload destructor panic"), "{stderr}");
        assert!(
            stderr.contains(if case == "outer-unwind" { "review unrelated control panic" } else { "review plugin destructor panic" }),
            "{stderr}"
        );
    }
}

#[test]
fn postfault_commands_are_retained_until_control_services_or_destroys_them() {
    for service in [false, true] {
        let (player, _, _) = rig(false, false);
        callbacks(&player, 1, false);
        let session = Arc::new(Session::default());
        let weak = Arc::downgrade(&session);
        player.tx.send(Cmd::Session(session, Vec::new())).unwrap();
        let (held, fault, shared) = (Arc::clone(&player.state), Arc::clone(&player.fault), Arc::clone(&player.shared));
        let stats = std::thread::spawn(move || {
            let mut output = [1.0; 32];
            let (_, stats) = measure(|| render_held(&held, &fault, &shared, &mut output, 2));
            assert_eq!(output, [0.0; 32]);
            stats
        })
        .join()
        .unwrap();
        assert!(stats.is_zero(), "fault callback must not receive/free queued owners: {stats:?}");
        assert!(weak.upgrade().is_some());
        if service {
            player.idle();
            assert!(weak.upgrade().is_none());
        }
        drop(player);
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn secondary_payload_cleanup_is_not_recursively_invoked() {
    struct Secondary(Arc<AtomicUsize>);
    impl Drop for Secondary {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    struct Primary(Arc<AtomicUsize>);
    impl Drop for Primary {
        fn drop(&mut self) {
            std::panic::panic_any(Secondary(Arc::clone(&self.0)));
        }
    }
    let (player, _, _) = rig(false, false);
    let secondary_drops = Arc::new(AtomicUsize::new(0));
    player.state.lock().unwrap().as_mut().unwrap().panic_payload = Some(Box::new(Primary(Arc::clone(&secondary_drops))));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(player)));
    assert!(result.is_ok());
    assert_eq!(secondary_drops.load(Ordering::Relaxed), 0, "secondary foreign cleanup must not recurse");
}
