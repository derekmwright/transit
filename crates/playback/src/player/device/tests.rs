#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use soundcraft_rt::testing::measure;
use std::sync::atomic::AtomicUsize;

fn prepared() -> (Player, DeviceOutput) {
    // Allocates clocks/queues only. No device discovery, stream or fallback worker.
    let (player, access, _clock) = Player::prepare(Arc::new(Session::default()), 48_000, soundcraft_model::IoMap::default()).unwrap();
    let output = DeviceOutput::new(access, &player, 2, 48_000);
    (player, output)
}

#[test]
fn public_transport_reports_consumption_and_returns_exact_action_receipts() {
    let (player, mut callback) = prepared();
    let _: &crate::Player = &player; // The public facade is this prepared runtime.
    callback.fill(&mut [0.0f32; 128]);
    player.service().unwrap();
    let play = player.play(12_345, None, None).unwrap();
    assert!(!player.is_playing());
    assert_eq!(player.position(), 0, "request admission is not device consumption");
    let mut out = [1i16; 128];
    let ((), heap) = measure(|| callback.fill(&mut out));
    assert!(heap.is_zero(), "{heap:?}");
    assert!(player.is_playing());
    assert_eq!(player.position(), 12_409);
    let stop = player.stop().unwrap();
    assert!(player.is_playing());
    assert_ne!(player.recovery_progress().unwrap().0, stop);
    callback.fill(&mut out);
    assert!(!player.is_playing());
    assert_eq!(player.position(), 12_409);
    let mut notices = [None; 16];
    let count = player.poll_notices(&mut notices).unwrap();
    assert_eq!(player.recovery_progress().unwrap().0, stop);
    // Urgent requests use acknowledgment frontiers, not ordinary lane receipts.
    let receipts: Vec<_> = notices[..count]
        .iter()
        .flatten()
        .filter_map(|notice| match notice {
            Notice::Action(receipt) if receipt.sequence == play => Some(receipt),
            _ => None,
        })
        .collect();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].outcome, action_lane::Outcome::Applied);
    assert_eq!(receipts[0].handler_result, Some(ActionResult::Applied));
}

#[test]
fn device_loss_closes_sources_and_actions_while_main_and_audio_are_busy() {
    let (player, mut callback) = prepared();
    callback.fill(&mut [0.0f32; 128]);
    player.service().unwrap();
    let action = player.play(0, None, None).unwrap();
    let token = player.audition.unwrap();
    let mut error = error_callback(Arc::clone(&player.fault), Arc::clone(&player.shared), player.failure_signal.clone());
    {
        let owner = player.owner().unwrap();
        let held = callback.access.enter().unwrap();
        let heap = std::thread::spawn(move || {
            crate::mark_audio_thread();
            measure(|| error(cpal::StreamError::DeviceNotAvailable)).1
        })
        .join()
        .unwrap();
        assert!(heap.is_zero(), "error callback: {heap:?}");
        assert!(owner.faulted());
        assert!(!owner.source_admitted(token));
        let audio = &held;
        assert!(audio.plans.faulted());
        assert!(!audio.sources.permits(token.port(), token.generation()));
    }
    assert_eq!(player.stop(), Err(PlayerError::Faulted));
    let mut out = [0.75f32; 128];
    let ((), heap) = measure(|| callback.fill(&mut out));
    assert!(heap.is_zero());
    assert_eq!(out, [0.0; 128]);
    let mut notices = [None; 16];
    let count = player.poll_notices(&mut notices).unwrap();
    assert!(notices[..count].iter().flatten().any(|notice| matches!(notice,
        Notice::Action(receipt) if receipt.sequence == action && receipt.outcome == action_lane::Outcome::Faulted
    )));
    assert_eq!(player.callback_telemetry().device_lost_errors, 1);
}

#[test]
fn closed_admission_yields_silence_without_locks_waiting_or_allocating() {
    let (player, mut callback) = prepared();
    let mut out = [0.75f32; 128];
    player.state.as_ref().unwrap().close();
    let ((), heap) = measure(|| callback.fill(&mut out));
    assert!(heap.is_zero());
    assert_eq!(out, [0.0; 128]);
    assert_eq!(player.callback_telemetry().unavailable_silence_frames, 64);
    assert_eq!(player.callback_telemetry().contention_silence_frames, 0);
}

#[test]
fn reentrant_control_and_input_requests_refuse_without_losing_ownership() {
    let (player, _callback) = prepared();
    let owner = player.owner().unwrap();
    assert_eq!(player.play(0, None, None), Err(PlayerError::Busy));
    drop(owner);
    let mut recorder = Some(record::Recorder::detached("caller input"));
    let input = player.input.borrow_mut();
    assert_eq!(player.close_input(&mut recorder), Err(PlayerError::Busy));
    assert_eq!(recorder.as_ref().unwrap().device_name, "caller input");
    drop(input);
    *player.input.borrow_mut() = HeldInput::Held(Some(record::Recorder::detached("held input")));
    assert_eq!(player.close_input(&mut recorder), Err(PlayerError::Busy));
    assert!(recorder.is_some());
    *player.input.borrow_mut() = HeldInput::Held(None);
    player.close_input(&mut recorder).unwrap();
    assert!(recorder.is_none());
    assert_eq!(player.open_input(&AudioConfig::default()).unwrap().device_name, "caller input");
}

#[test]
fn returning_input_cannot_finalize_or_discard_an_uncollected_take() {
    let (player, _callback) = prepared();
    *player.input.borrow_mut() = HeldInput::Held(None);
    let mut recorder = record::Recorder::detached("recording input");
    // Empty take list still owns a writer/capture operation; no files or devices.
    recorder.arm(Vec::new()).unwrap();
    let mut recorder = Some(recorder);
    assert_eq!(player.close_input(&mut recorder), Err(PlayerError::RecordingActive));
    assert!(recorder.as_ref().unwrap().has_pending_take());
    assert_eq!(recorder.as_mut().unwrap().finish(), (Vec::new(), None));
    player.close_input(&mut recorder).unwrap();
    assert!(recorder.is_none());
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn virtual_fallback_conserves_fractional_time_across_irregular_wakes() {
    use std::time::Duration;
    for rate in [44_100, 48_000, 96_000] {
        let mut pacer = SilentPacer::default();
        let (mut elapsed, mut rendered) = (0u128, 0u128);
        for index in 0..10_000u64 {
            let nanos = 1_000_001 + (index % 17) * 531;
            elapsed += u128::from(nanos);
            rendered += pacer.advance(Duration::from_nanos(nanos), rate, 8192).unwrap() as u128;
            assert_eq!(rendered, elapsed * u128::from(rate) / 1_000_000_000);
        }
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn virtual_fallback_bounds_each_pass_without_discarding_stall_backlog() {
    use std::time::Duration;
    let mut pacer = SilentPacer::default();
    let mut rendered = pacer.advance(Duration::from_secs(1), 48_000, 64).unwrap();
    assert_eq!(rendered, 64);
    assert_eq!(pacer.advance(Duration::ZERO, 48_000, 0), Some(0));
    for _ in 1..750 {
        assert_eq!(pacer.advance(Duration::ZERO, 48_000, 64), Some(64));
        rendered += 64;
    }
    assert_eq!(rendered, 48_000);
    assert_eq!(pacer.advance(Duration::ZERO, 48_000, 64), Some(0));
}

struct DropTrace {
    main: std::thread::ThreadId,
    drops: AtomicUsize,
    wrong_thread: AtomicBool,
}

impl DropTrace {
    fn new() -> Arc<Self> {
        Arc::new(Self { main: std::thread::current().id(), drops: AtomicUsize::new(0), wrong_thread: AtomicBool::new(false) })
    }

    fn record(&self) {
        if std::thread::current().id() != self.main {
            self.wrong_thread.store(true, Ordering::Relaxed);
        }
        self.drops.fetch_add(1, Ordering::Relaxed);
    }

    fn assert_main_drop(&self) {
        assert_eq!(self.drops.load(Ordering::Relaxed), 1);
        assert!(!self.wrong_thread.load(Ordering::Relaxed));
    }
}

struct PanicPayload(Arc<DropTrace>);

impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.record();
    }
}

struct ProbePlugin {
    inner: Box<dyn soundcraft_dsp::Plugin>,
    drops: Arc<DropTrace>,
    panic: Option<Arc<DropTrace>>,
    gate: Option<(std::sync::mpsc::SyncSender<()>, Arc<AtomicBool>)>,
}

impl soundcraft_dsp::Plugin for ProbePlugin {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, rate: f32, frames: usize, channels: usize) {
        self.inner.prepare(rate, frames, channels);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        self.inner.set_param(id, value)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if let Some((entered, stop)) = &self.gate {
            let _ = entered.try_send(());
            while !stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        if let Some(trace) = &self.panic {
            std::panic::panic_any(PanicPayload(Arc::clone(trace)));
        }
        self.inner.process(io, frames);
    }
}

impl Drop for ProbePlugin {
    fn drop(&mut self) {
        self.drops.record();
    }
}

type ProbeControl = super::super::Control<Command, ActionResult>;

/// A real prepared Audio and production DeviceOutput, with only the device and
/// plugin factory replaced. The standalone compiler is retained on Main. Player
/// owns the same anchored Audio used by its normal RuntimeControl path.
fn probe_player(
    panic: Option<Arc<DropTrace>>,
    entered: Option<std::sync::mpsc::SyncSender<()>>,
) -> (Player, AudioAccess<Audio>, ProbeControl, Arc<DropTrace>) {
    use soundcraft_model::{ChannelFormat, Insert, TrackKind};
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    session.track_mut(track).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    let mut player = Player::empty(48_000);
    let key = device_clock::ClockKey { generation: 1, domain: 1, direction: device_clock::Direction::Output };
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 };
    let (mut control, audio) = super::super::audio::pair(target, PlanLimits::default(), key, 48_000, Arc::clone(&player.shared)).unwrap();
    control.request(Arc::new(session), None).unwrap();
    let drops = DropTrace::new();
    #[cfg(not(target_arch = "wasm32"))]
    let stop = Arc::clone(&player.silent_stop);
    #[cfg(target_arch = "wasm32")]
    let stop = Arc::new(AtomicBool::new(true));
    control.service(&mut |spec, _generation| {
        let inner = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch).unwrap().plugin;
        Some(soundcraft_mix::NewInstance {
            plugin: Box::new(ProbePlugin {
                inner,
                drops: Arc::clone(&drops),
                panic: panic.clone(),
                gate: entered.as_ref().map(|tx| (tx.clone(), Arc::clone(&stop))),
            }),
            refused_state: false,
        })
    });
    let (anchor, access) = Anchor::new(audio);
    player.state = Some(anchor);
    (player, access, control, drops)
}

#[test]
fn lost_or_rejected_device_callback_never_owns_plugin_destruction() {
    // Before any callback models build failure; after one callback models start
    // failure or device loss without another render callback to return ownership.
    for rendered in [false, true] {
        let (player, access, control, drops) = probe_player(None, None);
        let mut callback = DeviceOutput::new(access, &player, 2, 48_000);
        std::thread::spawn(move || {
            if rendered {
                callback.fill(&mut [0.0f32; 128]);
            }
            drop(callback);
        })
        .join()
        .unwrap();
        assert_eq!(drops.drops.load(Ordering::Relaxed), 0);
        drop(player);
        drop(control);
        drops.assert_main_drop();
    }
}

#[test]
fn escaped_plugin_panic_and_endpoint_loss_reclaim_payload_and_plugin_on_main() {
    let payload = DropTrace::new();
    let (player, access, control, drops) = probe_player(Some(Arc::clone(&payload)), None);
    let mut callback = DeviceOutput::new(access, &player, 2, 48_000);
    std::thread::spawn(move || {
        let mut out = [0.75f32; 128];
        callback.fill(&mut out);
        assert_eq!(out, [0.0; 128]);
        assert!(callback.fault.failed.load(Ordering::Acquire));
        let ((), heap) = measure(|| callback.fill(&mut out));
        assert!(heap.is_zero(), "faulted callback: {heap:?}");
        assert_eq!(out, [0.0; 128]);
        drop(callback);
    })
    .join()
    .unwrap();
    assert_eq!(drops.drops.load(Ordering::Relaxed), 0);
    assert_eq!(payload.drops.load(Ordering::Relaxed), 0);
    drop(player);
    drop(control);
    drops.assert_main_drop();
    payload.assert_main_drop();
}

#[test]
fn unexpected_callback_unwind_releases_guard_without_moving_destruction_to_worker() {
    let (player, access, control, drops) = probe_player(None, None);
    let mut callback = DeviceOutput::new(access, &player, 2, 48_000);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        callback.fill(&mut [0.0f32; 128]);
        let _guard = callback.access.enter().unwrap();
        entered_tx.send(()).unwrap();
        panic!("synthetic enclosing callback panic");
    });
    entered_rx.recv().unwrap();
    drop(player);
    assert!(worker.join().is_err());
    drop(control);
    drops.assert_main_drop();
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn shutdown_waits_for_active_render_without_requiring_backend_join() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (player, access, control, drops) = probe_player(None, Some(entered_tx));
    let mut callback = DeviceOutput::new(access, &player, 2, 48_000);
    // Deliberately do not put this handle in Player: fake a backend returning
    // from stream destruction while callback teardown still runs on its worker.
    let worker = std::thread::spawn(move || {
        callback.fill(&mut [0.0f32; 128]);
        assert!(callback.access.enter().is_none(), "shutdown must permanently close admission");
        drop(callback);
    });
    entered_rx.recv().unwrap();
    // Drop closes admission and publishes silent_stop; the plugin gate observes
    // that signal and finishes its active render. No sleep is used as a fence.
    drop(player);
    worker.join().unwrap();
    drop(control);
    drops.assert_main_drop();
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn failed_silent_worker_spawn_drops_endpoint_but_keeps_audio_on_main() {
    let (mut player, access, control, drops) = probe_player(None, None);
    player.start_silent(access, |task| {
        // Like Builder::spawn's error path, destroy the captured callback without
        // ever invoking it. No device, OS thread failure, or profile is required.
        drop(task);
        Err(std::io::Error::other("synthetic worker start failure"))
    });
    assert!(player.fault.failed.load(Ordering::Acquire));
    assert!(player.initial_error().unwrap().contains("synthetic worker start failure"));
    assert!(player.silent_worker.is_none());
    assert_eq!(drops.drops.load(Ordering::Relaxed), 0);
    drop(player);
    drop(control);
    drops.assert_main_drop();
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn silent_worker_shutdown_reclaims_audio_only_after_join() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (mut player, access, control, drops) = probe_player(None, Some(entered_tx));
    player.start_silent(access, |task| std::thread::Builder::new().spawn(task));
    entered_rx.recv().unwrap();
    assert_eq!(drops.drops.load(Ordering::Relaxed), 0);
    drop(player);
    drop(control);
    drops.assert_main_drop();
}

#[test]
fn multichunk_conversion_and_main_control_activity_do_not_lock_or_allocate_on_audio() {
    let (player, mut callback) = prepared();
    callback.fill(&mut [0.0f32; 128]);
    player.service().unwrap();
    player.play(0, None, None).unwrap();
    let mut out = vec![1i16; 65_536];
    // Main may hold both control and the legacy meter mutex while audio renders:
    // neither is part of the prepared callback access path.
    let _owner = player.owner().unwrap();
    let _meters = player.shared.meters.lock().unwrap();
    let ((), heap) = measure(|| callback.fill(&mut out));
    assert!(heap.is_zero(), "{heap:?}");
    assert!(player.is_playing());
    assert_eq!(player.position(), 32_768);
    assert_eq!(player.callback_telemetry().contention_silence_frames, 0);
}

#[test]
fn failed_open_retaining_callback_cannot_hold_shutdown_or_plugin_reclamation_hostage() {
    // ASIO/CoreAudio build failures may retain the registered closure without
    // returning a Stream handle; WebAudio also retains a closure after closing.
    // A retained endpoint must not make Player Drop wait for closure destruction.
    let (player, access, control, drops) = probe_player(None, None);
    let mut callback = DeviceOutput::new(access, &player, 2, 48_000);
    callback.fill(&mut [0.0f32; 128]);
    drop(player);
    drop(control);
    drops.assert_main_drop();
    std::thread::spawn(move || {
        assert!(callback.access.enter().is_none());
        let mut out = [0.75f32; 128];
        let ((), heap) = measure(|| callback.fill(&mut out));
        assert!(heap.is_zero(), "retained callback must render silence without heap work: {heap:?}");
        assert_eq!(out, [0.0; 128]);
        let DeviceOutput { access, output, shared, fault } = callback;
        // This is backend closure teardown, not timed rendering. The empty
        // allocation is permitted to be freed here; Audio was already reclaimed.
        let ((), heap) = measure(|| drop(access));
        assert_eq!(heap.allocs, 0);
        assert_eq!(heap.frees, 1);
        drop((output, shared, fault));
    })
    .join()
    .unwrap();
    drops.assert_main_drop();
}

#[test]
fn session_identity_tracks_view_cow_and_same_rate_replace_keeps_the_device_clock() {
    let (player, mut callback) = prepared();
    callback.fill(&mut [0.0f32; 128]);
    player.service().unwrap();
    let original = Arc::clone(player.owner().unwrap().requested_session());
    assert!(player.has_requested_session(&original));
    let epoch = player.epoch;
    let mut view = Arc::clone(&original);
    Arc::make_mut(&mut view).edit.zoom.scroll = 32;
    assert!(!player.has_requested_session(&view));
    player.update_session(Arc::clone(&view)).unwrap();
    assert!(player.has_requested_session(&view));
    player.replace_session(Arc::clone(&view)).unwrap();
    player.service().unwrap();
    callback.fill(&mut [0.0f32; 2048]);
    player.service().unwrap();
    assert_eq!(player.epoch, epoch);
    assert!(player.has_requested_session(&view));
    assert!(!player.has_requested_session(&original));
}
