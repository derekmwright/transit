#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use std::sync::atomic::AtomicUsize;
use transit_rt::testing::measure;

fn prepared() -> (Player, DeviceOutput) {
    // Allocates clocks/queues only. No device discovery, stream or fallback worker.
    let (player, access, _clock) = Player::prepare(Arc::new(Session::default()), 48_000, soundcraft_model::IoMap::default()).unwrap();
    let output = DeviceOutput::new(access, &player, 2);
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
            measure(|| error(ab::StreamError::DeviceNotAvailable)).1
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
        let mut callback = DeviceOutput::new(access, &player, 2);
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
    let mut callback = DeviceOutput::new(access, &player, 2);
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
    let mut callback = DeviceOutput::new(access, &player, 2);
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
    let mut callback = DeviceOutput::new(access, &player, 2);
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
    // Main may hold control while audio renders: it is not part of the prepared callback access path.
    let _owner = player.owner().unwrap();
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
    let mut callback = DeviceOutput::new(access, &player, 2);
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
        let DeviceOutput { access, channels: _, timing, shared, fault, loop_probe, convert } = callback;
        // This is backend closure teardown, not timed rendering. The empty
        // allocation is permitted to be freed here; Audio was already reclaimed.
        let ((), heap) = measure(|| drop(access));
        assert_eq!(heap.allocs, 0);
        assert_eq!(heap.frees, 1);
        drop((timing, shared, fault, loop_probe, convert));
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

fn fake_factory(failing: bool) -> ab::fake::FakeBackend {
    let fake = ab::fake::FakeBackend::new(ab::Format::new(48_000, 2, ab::SampleFormat::F32));
    if failing {
        fake.fail_next_open(ab::OpenError::DeviceNotAvailable("scripted".into()));
    }
    fake
}

#[test]
fn a_native_backend_that_opens_is_marked_and_says_nothing() {
    let (native, fallback) = (fake_factory(false), fake_factory(false));
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &native, &fallback).unwrap();
    assert!(player.native_backend && player.backend_note.is_none() && !player.silent);
    assert_eq!((native.opens().len(), fallback.opens().len()), (1, 0), "cpal was not touched");
}

#[test]
fn a_native_backend_that_fails_to_open_falls_back_and_says_why() {
    let (native, fallback) = (fake_factory(true), fake_factory(false));
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &native, &fallback).unwrap();
    assert!(!player.native_backend && !player.silent, "the fallback backend serves the device");
    assert_eq!(player.backend_note.as_deref(), Some("Native WASAPI failed: scripted; using the cpal backend"));
    assert_eq!(fallback.opens().len(), 1);
}

/// The fake's `set_host_blocked` is the hook for a stuck ASIO host thread (a driver settings panel
/// that is still open): the open fails and the same driver must not be loaded again through cpal.
#[test]
fn a_native_open_refused_by_a_stuck_host_does_not_fall_back_to_cpal() {
    let (native, fallback) = (fake_factory(true), fake_factory(false));
    native.set_host_blocked(true);
    let error = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &native, &fallback).err();
    assert_eq!(error.as_deref(), Some("the ASIO driver's settings panel is open; close it and reconnect"));
    assert!(fallback.opens().is_empty(), "cpal was not asked to open the driver again");
}

#[test]
fn both_backends_failing_reports_the_fallbacks_error() {
    let (native, fallback) = (fake_factory(true), fake_factory(true));
    let error = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &native, &fallback).err();
    assert_eq!(error.as_deref(), Some("scripted"));
}

fn modes(fake: &ab::fake::FakeBackend) -> Vec<(ab::Direction, ab::Mode, ab::Period)> {
    fake.opens().iter().map(|(direction, request)| (*direction, request.mode, request.period)).collect()
}

/// A native fake whose device asks for a 128 frame period, the chosen buffer size.
fn fake_at_128() -> ab::fake::FakeBackend {
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    let request =
        ab::OpenRequest { format, period: ab::Period::Frames(128), mode: ab::Mode::Shared, origin: ab::HostOrigin::default(), calibration: None };
    ab::fake::FakeBackend::new(format).with_attempts(vec![request])
}

#[test]
fn the_native_backend_is_asked_for_exclusive_unless_the_user_shares_the_device() {
    for (share_device, mode) in [(false, ab::Mode::Exclusive), (true, ab::Mode::Shared)] {
        let native = fake_factory(false);
        let cfg = AudioConfig { share_device, ..Default::default() };
        let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &native, &fake_factory(false)).unwrap();
        assert!(player.native_backend && player.backend_note.is_none());
        assert_eq!(modes(&native), [(ab::Direction::Output, mode, ab::Period::Default)], "share_device {share_device}");
        assert_eq!(player.device_exclusive, mode == ab::Mode::Exclusive);
    }
    // The cpal backend has only shared streams: it is never asked for anything else.
    let fallback = fake_factory(false);
    let _ = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &fake_factory(true), &fallback).unwrap();
    assert_eq!(modes(&fallback), [(ab::Direction::Output, ab::Mode::Shared, ab::Period::Default)]);
}

#[test]
fn the_telemetry_carries_the_backend_snapshot_of_the_output_stream() {
    let native = fake_factory(false);
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &native, &fake_factory(false)).unwrap();
    assert_eq!(player.callback_telemetry().backend, Some(ab::StreamStats::default()), "the fake backend keeps counters");
    let scripted = ab::StreamStats { wakes: 9, skipped_wakes: 2, max_wake_late_us: 700, ..Default::default() };
    native.set_stream_stats(ab::Direction::Output, scripted);
    assert_eq!(player.callback_telemetry().backend, Some(scripted));
    assert_eq!(player.backend_stats(), Some(scripted));
}

#[test]
fn a_refused_period_retries_exclusive_at_the_device_period_and_says_so() {
    let native = fake_at_128();
    native.fail_next_open(ab::OpenError::PeriodRefused {
        requested_frames: 128,
        min_frames: Some(144),
        default_frames: Some(480),
        message: "Initialize(exclusive): the period is outside the range the device accepts".into(),
    });
    let cfg = AudioConfig { buffer_size: Some(128), share_device: false, ..Default::default() };
    let fallback = fake_factory(false);
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &native, &fallback).unwrap();
    assert_eq!(
        modes(&native),
        [(ab::Direction::Output, ab::Mode::Exclusive, ab::Period::Frames(128)), (ab::Direction::Output, ab::Mode::Exclusive, ab::Period::Default)]
    );
    assert!(player.native_backend && player.device_exclusive && fallback.opens().is_empty());
    assert_eq!(player.backend_note.as_deref(), Some("Exclusive mode could not use 128 frames; using the device period"));
}

#[test]
fn a_period_error_at_the_device_period_has_no_step_left_and_falls_back_to_cpal() {
    let native = fake_factory(false);
    native.fail_next_open(ab::OpenError::PeriodRefused {
        requested_frames: 128,
        min_frames: Some(144),
        default_frames: Some(480),
        message: "Initialize(exclusive): the period is outside the range the device accepts".into(),
    });
    let fallback = fake_factory(false);
    let player =
        Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig { share_device: false, ..Default::default() }, &native, &fallback)
            .unwrap();
    assert!(!player.native_backend);
    assert!(player.backend_note.as_deref().is_some_and(|n| n.starts_with("Native WASAPI failed: ") && n.ends_with("; using the cpal backend")));
}

#[test]
fn a_device_held_by_another_app_or_closed_to_exclusive_use_opens_shared_and_says_so() {
    let held = ab::OpenError::InUse("Initialize(exclusive): in use".into());
    let off = ab::OpenError::ExclusiveDisallowed("Initialize(exclusive): not allowed".into());
    for (error, note) in [
        (held, "Exclusive mode unavailable: another app holds the device; opened shared (higher latency)"),
        (off, "Exclusive mode is turned off for this device in Windows; opened shared"),
    ] {
        let native = fake_at_128();
        native.fail_next_open(error);
        let cfg = AudioConfig { buffer_size: Some(128), share_device: false, ..Default::default() };
        let fallback = fake_factory(false);
        let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &native, &fallback).unwrap();
        assert_eq!(
            modes(&native),
            [
                (ab::Direction::Output, ab::Mode::Exclusive, ab::Period::Frames(128)),
                (ab::Direction::Output, ab::Mode::Shared, ab::Period::Frames(128))
            ],
            "shared keeps the chosen buffer size"
        );
        assert!(player.native_backend && !player.device_exclusive && fallback.opens().is_empty());
        assert_eq!(player.backend_note.as_deref(), Some(note));
    }
}

#[test]
fn any_other_exclusive_error_and_any_shared_error_fall_back_to_cpal() {
    for (share_device, error) in [
        (false, ab::OpenError::DeviceNotAvailable("gone".into())),
        (false, ab::OpenError::Other("the device takes no 48000 Hz, 2-channel format in exclusive mode".into())),
        (true, ab::OpenError::InUse("busy".into())),
    ] {
        let native = fake_factory(false);
        native.fail_next_open(error.clone());
        let fallback = fake_factory(false);
        let cfg = AudioConfig { share_device, ..Default::default() };
        let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &native, &fallback).unwrap();
        assert_eq!((native.opens().len(), fallback.opens().len()), (1, 1), "{error}");
        assert!(!player.native_backend);
        assert_eq!(player.backend_note, Some(NativeKind::Wasapi.fallback_note(&error.to_string())));
    }
}

#[test]
fn a_duplex_input_that_cannot_open_exclusive_reopens_both_streams_in_the_fallback_mode() {
    let native = fake_factory(false).with_capture(2, true);
    native.fail_next_open(ab::OpenError::InUse("in use".into()));
    let player = Player::open_native_or_cpal(
        Arc::new(Session::default()),
        &AudioConfig { share_device: false, ..Default::default() },
        &native,
        &fake_factory(false),
    )
    .unwrap();
    assert_eq!(
        modes(&native),
        [
            (ab::Direction::Input, ab::Mode::Exclusive, ab::Period::Default),
            (ab::Direction::Input, ab::Mode::Shared, ab::Period::Default),
            (ab::Direction::Output, ab::Mode::Shared, ab::Period::Default),
        ],
        "the output never opened exclusive beside a shared input"
    );
    assert!(player.native_backend && !player.device_exclusive && !player.silent);
    assert_eq!(player.backend_note.as_deref(), Some("Exclusive mode unavailable: another app holds the device; opened shared (higher latency)"));
    assert!(native.running(ab::Direction::Input) && native.running(ab::Direction::Output));
}

#[test]
fn a_duplex_input_failure_the_ladder_does_not_answer_keeps_the_output_exclusive() {
    let cfg = AudioConfig { share_device: false, ..Default::default() };
    let held = fake_factory(false).with_capture(2, true);
    held.fail_next_open(ab::OpenError::Other("not an exclusive-mode error".into()));
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &held, &fake_factory(false)).unwrap();
    assert!(player.native_backend && player.device_exclusive, "an input failure the ladder does not answer leaves the output exclusive");
    assert_eq!(held.opens().len(), 2);
}

/// Opens the owner's default endpoint through the whole Player path with the native backend.
/// Run it with `cargo test -p soundcraft-playback --features native-wasapi -- --ignored hardware_ --nocapture`.
#[cfg(all(windows, feature = "native-wasapi"))]
#[test]
#[ignore = "opens the default audio endpoint"]
fn hardware_player_opens_native_wasapi() {
    let cfg = AudioConfig { backend: crate::Backend::NativeWasapi, system: Some("WASAPI".into()), ..Default::default() };
    let player = Player::with_config(Arc::new(Session::default()), &cfg);
    println!(
        "device {:?} system {:?} rate {} buffer {:?} native {} note {:?}",
        player.device_name, player.device_system, player.device_rate, player.device_buffer, player.native_backend, player.backend_note
    );
    assert!(!player.silent, "the device did not open: {:?}", player.initial_error());
    assert!(player.native_backend, "the native backend did not serve the device: {:?}", player.backend_note);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let telemetry = player.callback_telemetry();
    println!("{telemetry:?}");
    assert!(telemetry.frames > 0, "no callbacks in two seconds");
    assert_eq!((telemetry.backend_errors, telemetry.device_lost_errors), (0, 0));
    drop(player);
}

/// Opens `driver` through the whole Player path with the native ASIO backend, takes the duplex input
/// as the recorder does, runs two seconds and checks both streams' counts. Run with
/// `cargo test -p soundcraft-playback --features native-asio --lib -- --ignored hardware_player_opens_native_asio --nocapture --test-threads 1`.
#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
fn player_on_native_asio(driver: &str, rate: u32, buffer: Option<u32>) -> (u16, usize) {
    let cfg = AudioConfig {
        backend: crate::Backend::NativeAsio,
        system: Some("ASIO".into()),
        output: Some(driver.into()),
        sample_rate: Some(rate),
        buffer_size: buffer,
        ..Default::default()
    };
    let player = Player::with_config(Arc::new(Session::default()), &cfg);
    println!(
        "{driver}: device {:?} system {:?} rate {} buffer {:?} channels {} latency {:?} sample {:?} native {} note {:?}",
        player.device_name,
        player.device_system,
        player.device_rate,
        player.device_buffer,
        player.device_channels,
        player.output_latency,
        player.output_sample,
        player.native_backend,
        player.backend_note
    );
    println!("{driver}: outputs {:?}; inputs {:?}", player.output_channel_names, player.input_channel_names);
    assert!(!player.silent, "the device did not open: {:?}", player.initial_error());
    assert!(player.native_backend && player.device_system == "ASIO", "the native backend did not serve the device: {:?}", player.backend_note);
    let recorder = player.open_input(&cfg).expect("the duplex input is held");
    println!(
        "{driver}: input {} ch at {} Hz, buffer {:?}, latency {:?}",
        recorder.channels, recorder.sample_rate, recorder.buffer_frames, recorder.latency
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    let telemetry = player.callback_telemetry();
    let input = recorder.backend_stats().expect("input counters");
    println!("{driver}: {telemetry:?}");
    println!("{driver}: input {}", input.line(true));
    assert!(telemetry.frames > u64::from(rate), "fewer than a second of output frames in two seconds");
    assert!(input.wakes > 0, "the capture callback never ran");
    assert_eq!((telemetry.backend_errors, telemetry.device_lost_errors), (0, 0));
    let shape = (player.device_channels, recorder.channels);
    drop(recorder);
    drop(player);
    shape
}

/// The ADR 0015 hardware-insert measurement (the click train) through the Player on native ASIO, over
/// the bench's loopback cable: output `send` to input `ret` (0-based). Returns the measured frames.
#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
fn hardware_ping_on_native_asio(driver: &str, rate: u32, buffer: Option<u32>, send: u16, ret: u16) -> u32 {
    let cfg = AudioConfig {
        backend: crate::Backend::NativeAsio,
        system: Some("ASIO".into()),
        output: Some(driver.into()),
        sample_rate: Some(rate),
        buffer_size: buffer,
        ..Default::default()
    };
    let session = Session { sample_rate: soundcraft_time::SampleRate::new(rate).expect("a rate"), ..Session::default() };
    let player = Player::with_config(Arc::new(session), &cfg);
    assert!(!player.silent && player.native_backend, "{:?} {:?}", player.initial_error(), player.backend_note);
    let recorder = player.open_input(&cfg).expect("the duplex input is held");
    player.set_input(Arc::clone(&recorder.monitor)).expect("input");
    let settle = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < settle {
        player.service().expect("service");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The plan adopts the live input a few blocks late; start from an empty ring (K22).
    recorder.monitor.clear_monitor();
    player.start_hardware_probe(&[send], &[ret]).unwrap_or_else(|e| panic!("{e}"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let result = loop {
        player.service().expect("service");
        if let Some(result) = player.hardware_probe_result() {
            break result;
        }
        assert!(std::time::Instant::now() < deadline, "no result in 10 s");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let measured = result.unwrap_or_else(|e| panic!("{driver}: {e}"));
    println!(
        "{driver}: out {} -> in {}: {measured:?} ({:.3} ms at {} Hz, buffer {:?})",
        send + 1,
        ret + 1,
        f64::from(measured.round_trip_frames) * 1000.0 / f64::from(rate),
        player.device_rate,
        player.device_buffer
    );
    measured.round_trip_frames
}

#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
#[test]
#[ignore = "plays a click train out of the Scarlett's output 2 (cabled to input 2) through the Player"]
fn hardware_player_measures_a_hardware_insert_on_the_scarlett_over_native_asio() {
    let frames = hardware_ping_on_native_asio("Focusrite USB ASIO", 48_000, Some(64), 1, 1);
    assert!((250..400).contains(&frames), "{frames} frames: the bench measures about 300 at 64");
}

#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
#[test]
#[ignore = "plays a click train out of the Model 12's output 2 (cabled to input 1) through the Player"]
fn hardware_player_measures_a_hardware_insert_on_the_model12_over_native_asio() {
    let frames = hardware_ping_on_native_asio("Model Mixer ASIO Driver", 44_100, None, 1, 0);
    assert!((900..1_300).contains(&frames), "{frames} frames: the bench measures 974 to 1239");
}

#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
#[test]
#[ignore = "opens the Focusrite ASIO driver through the Player for two seconds"]
fn hardware_player_opens_native_asio_on_the_scarlett() {
    assert_eq!(player_on_native_asio("Focusrite USB ASIO", 48_000, Some(64)), (2, 2));
}

#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
#[test]
#[ignore = "opens the Model 12 ASIO driver through the Player for two seconds"]
fn hardware_player_opens_native_asio_on_the_model12() {
    assert_eq!(player_on_native_asio("Model Mixer ASIO Driver", 44_100, None), (10, 12));
}

/// The duplex open of the native path on the scripted backend: the output and the paired input are
/// opened as two streams, each with its own clock key, and the stamps of both are in the one host
/// origin, so an output frame maps to the input frame that was captured when it was presented.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_native_duplex_open_gives_both_streams_clocks_in_one_host_origin() {
    use ab::fake::{SliceSink, SliceSource};
    const BLOCK: usize = 192;
    const PERIOD_NS: u64 = 4_000_000;
    // The output is presented 6 ms after its callback; the input frame was captured 3 ms before its.
    const PRESENT_NS: i64 = 6_000_000;
    const CAPTURED_NS: i64 = 3_000_000;
    let fake = fake_factory(false).with_capture(2, true);
    let cfg = AudioConfig::default();
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &fake, &fake_factory(false)).unwrap();
    assert!(player.native_backend && !player.silent);
    let mut recorder = player.open_input(&cfg).expect("the duplex input is held for the recorder");
    assert_eq!(fake.opens().iter().map(|(direction, _)| *direction).collect::<Vec<_>>(), [ab::Direction::Input, ab::Direction::Output]);
    assert!(fake.running(ab::Direction::Input) && fake.running(ab::Direction::Output));

    let origin = player.origin;
    let t0 = origin.now_ns().unwrap();
    let stamp = |i: u64, direction: ab::Direction| {
        let callback = (i * PERIOD_NS) as i64;
        let frame = if direction == ab::Direction::Output { callback + PRESENT_NS } else { callback - CAPTURED_NS };
        ab::Stamp {
            host_callback_ns: t0 + i * PERIOD_NS,
            backend_callback_ns: callback,
            backend_frame_ns: frame,
            quality: ab::Quality::BackendEstimate,
            offset_uncertainty_ns: None,
        }
    };
    let (mut output, mut input) = ([0.0f32; BLOCK * 2], [0.25f32; BLOCK * 2]);
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    // A device that keeps pace with the real clock: block `i` is pumped once its host time has come.
    let mut maps = None;
    for i in 0..2_000u64 {
        while origin.now_ns().unwrap() < t0 + i * PERIOD_NS + 1_000_000 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(fake.pump_capture(BLOCK, stamp(i, ab::Direction::Input), format, &mut SliceSource { data: &input, channels: 2 }));
        assert!(fake.pump_render(BLOCK, stamp(i, ab::Direction::Output), &mut SliceSink { data: &mut output, channels: 2 }));
        if i % 4 == 3
            && let (Some(out), Some(inp)) = (player.clock_report().and_then(|r| r.map), recorder.clock_report().and_then(|r| r.map))
        {
            maps = Some((out, inp));
            break;
        }
    }
    let (out, inp) = maps.expect("both streams' clocks fitted");
    input.fill(0.0);
    assert_eq!((out.key.direction, inp.key.direction), (device_clock::Direction::Output, device_clock::Direction::Input));
    assert_ne!((out.key.generation, out.key.domain), (inp.key.generation, inp.key.domain), "each stream has its own clock domain");
    assert_eq!((out.quality, inp.quality), (ab::Quality::BackendEstimate, ab::Quality::BackendEstimate));
    // The recorder's mapping: the host time at which output frame F is presented, in the input's fit.
    let now = origin.now_ns().unwrap();
    let frame = out.frame_range().start() + 4_800;
    let host = out.frame_to_host(out.key, frame, now).expect("an output frame in the window");
    let captured = inp.host_to_frame(inp.key, host, now).expect("the input frame of that host time");
    let expected = frame + ((PRESENT_NS + CAPTURED_NS) as u64) * 48_000 / 1_000_000_000;
    assert!(captured.abs_diff(expected) <= 1, "input frame {captured}, expected {expected}");
}

/// A saved exclusive-mode loopback (the default mode) of the fake device at its rate and default
/// period, 480 frames (10 ms) with a 25 frame (about 0.52 ms) bound.
fn fake_calibration(rate: u32) -> crate::LoopbackCalibration {
    crate::LoopbackCalibration {
        device: "Fake device".into(),
        input: None,
        mode: crate::devices::MODE_EXCLUSIVE.into(),
        rate,
        period: 0,
        round_trip_frames: 480,
        spread_frames: 25,
        measured_at: "2026-10-21".into(),
    }
}

#[test]
fn a_matching_calibration_reaches_both_streams_and_a_mismatched_one_does_not() {
    for (calibration_rate, expected) in [(48_000, Some(ab::Calibration { round_trip_ns: 10_000_000, uncertainty_ns: 520_833 })), (44_100, None)] {
        let fake = fake_factory(false).with_capture(2, true);
        let cfg = AudioConfig { calibrations: vec![fake_calibration(calibration_rate)], share_device: false, ..Default::default() };
        let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &fake, &fake_factory(false)).unwrap();
        assert!(player.native_backend && !player.silent);
        let opens = fake.opens();
        assert_eq!(opens.iter().map(|(direction, _)| *direction).collect::<Vec<_>>(), [ab::Direction::Input, ab::Direction::Output]);
        for (direction, request) in opens {
            assert_eq!(request.calibration, expected, "{direction:?} stream at calibration rate {calibration_rate}");
        }
    }
    // An exclusive calibration is not the shared device's.
    let fake = fake_factory(false).with_capture(2, true);
    let cfg = AudioConfig { calibrations: vec![fake_calibration(48_000)], share_device: true, ..Default::default() };
    let _player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &fake, &fake_factory(false)).unwrap();
    assert!(fake.opens().iter().all(|(_, request)| request.mode == ab::Mode::Shared && request.calibration.is_none()));
    // Without a paired input there is no loopback to apply: the output stays uncalibrated.
    let fake = fake_factory(false);
    let cfg = AudioConfig { calibrations: vec![fake_calibration(48_000)], ..Default::default() };
    let _player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &fake, &fake_factory(false)).unwrap();
    assert!(fake.opens().iter().all(|(_, request)| request.calibration.is_none()));
}

/// The stamps a calibrated native pair reports are `Measured` with the calibration's bound, and the
/// recorder's mapping of an output frame to the input frame resolves from the fits (the exact path,
/// not the approximate placement by input frame ordinal).
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_calibrated_duplex_pair_fits_measured_clocks_and_maps_output_frames_to_input_frames() {
    use ab::fake::{SliceSink, SliceSource};
    const BLOCK: usize = 192;
    const PERIOD_NS: u64 = 4_000_000;
    const PRESENT_NS: i64 = 6_000_000;
    // The input is captured 3 ms before its callback; the calibration moves it earlier by the round trip (10 ms), so it
    // reports the presentation time of the output frame that produced it.
    const ROUND_TRIP_NS: i64 = 10_000_000;
    const BOUND_NS: u64 = 520_833;
    let fake = fake_factory(false).with_capture(2, true);
    let cfg = AudioConfig { calibrations: vec![fake_calibration(48_000)], ..Default::default() };
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &fake, &fake_factory(false)).unwrap();
    let mut recorder = player.open_input(&cfg).expect("the duplex input is held for the recorder");
    let origin = player.origin;
    let t0 = origin.now_ns().unwrap();
    let stamp = |i: u64, direction: ab::Direction| {
        let callback = (i * PERIOD_NS) as i64;
        let frame = if direction == ab::Direction::Output { callback + PRESENT_NS } else { callback - 3_000_000 - ROUND_TRIP_NS };
        ab::Stamp {
            host_callback_ns: t0 + i * PERIOD_NS,
            backend_callback_ns: callback,
            backend_frame_ns: frame,
            quality: ab::Quality::Measured,
            offset_uncertainty_ns: Some(BOUND_NS),
        }
    };
    let (mut output, input) = ([0.0f32; BLOCK * 2], [0.25f32; BLOCK * 2]);
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    let mut maps = None;
    for i in 0..2_000u64 {
        while origin.now_ns().unwrap() < t0 + i * PERIOD_NS + 1_000_000 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(fake.pump_capture(BLOCK, stamp(i, ab::Direction::Input), format, &mut SliceSource { data: &input, channels: 2 }));
        assert!(fake.pump_render(BLOCK, stamp(i, ab::Direction::Output), &mut SliceSink { data: &mut output, channels: 2 }));
        if i % 4 == 3
            && let (Some(out), Some(inp)) = (player.clock_report().and_then(|r| r.map), recorder.clock_report().and_then(|r| r.map))
        {
            maps = Some((out, inp));
            break;
        }
    }
    let (out, inp) = maps.expect("both streams' clocks fitted");
    assert_eq!((out.quality, inp.quality), (ab::Quality::Measured, ab::Quality::Measured));
    assert_eq!((out.offset_uncertainty_ns, inp.offset_uncertainty_ns), (Some(BOUND_NS), Some(BOUND_NS)));
    let now = origin.now_ns().unwrap();
    let frame = out.frame_range().start() + 4_800;
    let host = out.frame_to_host(out.key, frame, now).expect("an output frame in the window");
    let captured = inp.host_to_frame(inp.key, host, now).expect("the input frame of that host time resolves without the approximate path");
    let expected = frame + ((PRESENT_NS + 3_000_000 + ROUND_TRIP_NS) as u64) * 48_000 / 1_000_000_000;
    assert!(captured.abs_diff(expected) <= 1, "input frame {captured}, expected {expected}");
}

/// A duplex fake device whose capture is the render's channel 0 delayed by `delay` frames: the
/// plan's own view of a hardware loop. Pumps `blocks` blocks, counting the probe's frames.
#[cfg(not(target_arch = "wasm32"))]
fn loopback_player(delay: usize, send: &[u16], ret: &[u16], blocks: usize) -> (Player, Result<crate::hardware_probe::Measured, String>) {
    use ab::fake::{SliceSink, SliceSource};
    const BLOCK: usize = 192;
    let fake = fake_factory(false).with_capture(2, true);
    let cfg = AudioConfig::default();
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &fake, &fake_factory(false)).unwrap();
    let recorder = player.open_input(&cfg).expect("the duplex input");
    player.set_input(Arc::clone(&recorder.monitor)).unwrap();
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    let (mut output, mut input) = (vec![0.0f32; BLOCK * 2], vec![0.0f32; BLOCK * 2]);
    let mut sent: Vec<f32> = Vec::new();
    let mut started = false;
    // The plan adopts its live input on the first blocks after `set_input`; until then the ring
    // would pile up a backlog the insert would also see. Settle first, then start from empty.
    for _ in 0..4 {
        assert!(fake.pump_render(BLOCK, ab::Stamp::INVALID, &mut SliceSink { data: &mut output, channels: 2 }));
        player.service().unwrap();
    }
    recorder.monitor.clear_monitor();
    let mut result = Err("no result".to_string());
    for i in 0..blocks {
        // Let the plan and the input ring settle for a few blocks before arming.
        if i == 20 && !started {
            player.start_hardware_probe(send, ret).unwrap_or_else(|e| panic!("{e}"));
            started = true;
        }
        for f in 0..BLOCK {
            let n = i * BLOCK + f;
            let x = n.checked_sub(delay).and_then(|k| sent.get(k)).copied().unwrap_or(0.0);
            input[f * 2] = x;
            input[f * 2 + 1] = 0.0;
        }
        assert!(fake.pump_capture(BLOCK, ab::Stamp::INVALID, format, &mut SliceSource { data: &input, channels: 2 }));
        assert!(fake.pump_render(BLOCK, ab::Stamp::INVALID, &mut SliceSink { data: &mut output, channels: 2 }));
        sent.extend(output.as_chunks::<2>().0.iter().map(|f| f[0]));
        player.service().unwrap();
        if let Some(r) = player.hardware_probe_result() {
            result = r.map_err(|e| e.to_string());
            break;
        }
    }
    (player, result)
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_scripted_loopback_delay_is_measured_within_a_frame() {
    for delay in [300usize, 1_000, 2_400] {
        let (player, result) = loopback_player(delay, &[0], &[0], 700);
        let m = result.unwrap_or_else(|e| panic!("delay {delay}: {e}"));
        assert!(m.round_trip_frames.abs_diff(delay as u32) <= 1, "delay {delay}: measured {m:?}");
        assert!(m.spread_frames <= 1 && m.found >= 6, "{m:?}");
        assert!(!player.hardware_probe_running());
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn the_click_plays_on_the_send_channels_only() {
    use ab::fake::{SliceSink, SliceSource};
    let fake = fake_factory(false).with_capture(2, true);
    let cfg = AudioConfig::default();
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &cfg, &fake, &fake_factory(false)).unwrap();
    let recorder = player.open_input(&cfg).unwrap();
    player.set_input(Arc::clone(&recorder.monitor)).unwrap();
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    let (mut output, input) = (vec![0.0f32; 384], vec![0.0f32; 384]);
    for _ in 0..4 {
        fake.pump_capture(192, ab::Stamp::INVALID, format, &mut SliceSource { data: &input, channels: 2 });
        fake.pump_render(192, ab::Stamp::INVALID, &mut SliceSink { data: &mut output, channels: 2 });
        player.service().unwrap();
    }
    player.start_hardware_probe(&[1], &[0]).unwrap();
    let (mut left, mut right) = (0.0f32, 0.0f32);
    for _ in 0..60 {
        fake.pump_capture(192, ab::Stamp::INVALID, format, &mut SliceSource { data: &input, channels: 2 });
        fake.pump_render(192, ab::Stamp::INVALID, &mut SliceSink { data: &mut output, channels: 2 });
        left = left.max(output.as_chunks::<2>().0.iter().map(|f| f[0].abs()).fold(0.0, f32::max));
        right = right.max(output.as_chunks::<2>().0.iter().map(|f| f[1].abs()).fold(0.0, f32::max));
    }
    assert_eq!(left, 0.0, "the other channel stays silent");
    assert!(right > 0.2, "the click is on the send channel: {right}");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_measurement_is_refused_without_a_live_input_or_on_an_unknown_channel_and_silence_finds_nothing() {
    let player =
        Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &fake_factory(false), &fake_factory(false)).unwrap();
    let why = player.start_hardware_probe(&[0], &[0]).unwrap_err();
    assert!(why.contains("duplex"), "{why}");
    let (player, _) = loopback_player(300, &[0], &[0], 3);
    assert!(player.start_hardware_probe(&[0], &[7]).unwrap_err().contains("channel"));
    assert!(player.start_hardware_probe(&[9], &[0]).unwrap_err().contains("channel"));
    // A return that carries nothing: no click is found.
    let (_player, result) = loopback_player(300, &[0], &[1], 700);
    assert!(result.unwrap_err().contains("0 of"), "silence on the return");
}

/// A TASCAM Model 12 as the native ASIO backend presents it: one duplex driver with 12 inputs and
/// 10 outputs, packed 24-bit, its channels named by `getChannelInfo`, reporting its latency.
fn model12() -> ab::fake::FakeBackend {
    let names = |prefix: &str, n: u16| (1..=n).map(|c| format!("{prefix} {c}")).collect();
    ab::fake::FakeBackend::new(ab::Format::new(48_000, 10, ab::SampleFormat::I24))
        .named("ASIO", "Model Mixer ASIO Driver")
        .with_capture(12, true)
        .with_channel_names(names("Analog In", 12), names("Analog Out", 10))
        .with_latency(std::time::Duration::from_micros(15_760))
}

fn asio_config() -> AudioConfig {
    // Exclusive is asked for: ASIO has no such mode, so it must be ignored, not stepped down from.
    AudioConfig { backend: crate::Backend::NativeAsio, system: Some("ASIO".into()), share_device: false, ..Default::default() }
}

fn open_asio(fake: &ab::fake::FakeBackend, cfg: &AudioConfig) -> Player {
    Player::open_native(Arc::new(Session::default()), cfg, Box::new(fake.clone()), &fake_factory(false), NativeKind::Asio).unwrap()
}

#[test]
fn a_saved_asio_driver_that_is_not_installed_opens_nothing_else_and_says_so() {
    let fake = model12().strict_names();
    let fallback = fake_factory(false);
    let cfg = AudioConfig { output: Some("TASCAM Model 12 ASIO".into()), ..asio_config() };
    let refused = Player::open_native(Arc::new(Session::default()), &cfg, Box::new(fake.clone()), &fallback, NativeKind::Asio).err();
    let why = refused.expect("no other driver and no cpal fallback");
    assert!(why.contains("\"TASCAM Model 12 ASIO\"") && why.contains("\"Model Mixer ASIO Driver\""), "{why}");
    assert!(fake.opens().is_empty() && fallback.opens().is_empty(), "nothing opened");
    // The installed driver still opens by its name.
    let named = AudioConfig { output: Some("Model Mixer ASIO Driver".into()), ..asio_config() };
    assert!(!open_asio(&fake, &named).silent);
}

#[test]
fn a_native_asio_driver_opens_every_channel_capture_first_in_its_one_mode() {
    let fake = model12();
    let calibration = crate::LoopbackCalibration {
        device: "Model Mixer ASIO Driver".into(),
        input: None,
        mode: crate::devices::MODE_ASIO.into(),
        rate: 48_000,
        period: 0,
        round_trip_frames: 1_059,
        spread_frames: 2,
        measured_at: "2026-10-22".into(),
    };
    let cfg = AudioConfig { calibrations: vec![calibration], ..asio_config() };
    let player = open_asio(&fake, &cfg);
    assert!(player.native_backend && !player.silent && player.backend_note.is_none());
    assert_eq!(
        modes(&fake),
        [(ab::Direction::Input, ab::Mode::Shared, ab::Period::Default), (ab::Direction::Output, ab::Mode::Shared, ab::Period::Default)],
        "both directions open before either starts, capture first, as planned (no exclusive step)"
    );
    assert!(fake.opens().iter().all(|(_, request)| request.calibration.is_none()), "on ASIO it is not looked up by the request");
    let live = fake.live_calibrations();
    assert!(
        live.len() == 2 && live.iter().all(|(_, c)| c.is_some()),
        "a calibration keyed `asio` reaches both streams once the buffer is known: {live:?}"
    );
    assert!(fake.running(ab::Direction::Input) && fake.running(ab::Direction::Output));
    assert_eq!((player.device_system.as_str(), player.device_name.as_str(), player.device_channels), ("ASIO", "Model Mixer ASIO Driver", 10));
    assert!(!player.device_exclusive, "ASIO is never reported as WASAPI exclusive");
    assert_eq!((player.output_latency, player.output_sample), (Some(std::time::Duration::from_micros(15_760)), Some(ab::SampleFormat::I24)));
    let io = player.io_map();
    assert_eq!((io.system.as_str(), io.device.as_str(), io.inputs, io.outputs), ("ASIO", "Model Mixer ASIO Driver", 12, 10));
    assert!(io.path("Out 9-10").is_some() && io.path("In 12").is_some() && io.path("In 11-12").is_some(), "every channel has a path");
    assert_eq!((player.output_channel_names.len(), player.input_channel_names.len()), (10, 12));
    assert_eq!((player.output_channel_names[9].as_str(), player.input_channel_names[11].as_str()), ("Analog Out 10", "Analog In 12"));
    assert!(player.holds_input() && player.duplex(), "the input opened with the output is held from the start, before any track arms");
    let held = player
        .with_held_input(|r| (r.device_name.clone(), r.sample_rate, r.channels, r.buffer_frames, r.latency))
        .expect("the held input is readable");
    assert_eq!((held.0.as_str(), held.1, held.2, held.4), ("Model Mixer ASIO Driver", 48_000, 12, Some(std::time::Duration::from_micros(15_760))));
    let recorder = player.open_input(&cfg).expect("the duplex input is held for the recorder");
    assert_eq!((recorder.channels, recorder.sample_rate, recorder.latency), (12, 48_000, Some(std::time::Duration::from_micros(15_760))));
    assert_eq!(fake.opens().len(), 2, "taking the held input opens nothing");
    assert!(!player.holds_input() && player.with_held_input(|_| ()).is_none(), "the recorder owns it once taken");
}

/// The ADR 0015 hardware-insert measurement on the Model 12's last channels: the click leaves on
/// output 9 only and comes back on input 11 a known number of frames later.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_hardware_insert_is_measured_through_the_model12s_last_channels() {
    use ab::fake::{SliceSink, SliceSource};
    const BLOCK: usize = 256;
    const DELAY: usize = 1_059;
    let fake = model12();
    let cfg = asio_config();
    let player = open_asio(&fake, &cfg);
    let recorder = player.open_input(&cfg).unwrap();
    player.set_input(Arc::clone(&recorder.monitor)).unwrap();
    let format = ab::Format::new(48_000, 12, ab::SampleFormat::F32);
    let (mut output, mut input) = (vec![0.0f32; BLOCK * 10], vec![0.0f32; BLOCK * 12]);
    for _ in 0..4 {
        assert!(fake.pump_render(BLOCK, ab::Stamp::INVALID, &mut SliceSink { data: &mut output, channels: 10 }));
        player.service().unwrap();
    }
    recorder.monitor.clear_monitor();
    let (mut sent, mut elsewhere) = (Vec::<f32>::new(), 0.0f32);
    let mut result = Err("no result".to_string());
    for i in 0..700 {
        if i == 20 {
            player.start_hardware_probe(&[8], &[10]).unwrap_or_else(|e| panic!("{e}"));
        }
        input.fill(0.0);
        for f in 0..BLOCK {
            input[f * 12 + 10] = (i * BLOCK + f).checked_sub(DELAY).and_then(|k| sent.get(k)).copied().unwrap_or(0.0);
        }
        assert!(fake.pump_capture(BLOCK, ab::Stamp::INVALID, format, &mut SliceSource { data: &input, channels: 12 }));
        assert!(fake.pump_render(BLOCK, ab::Stamp::INVALID, &mut SliceSink { data: &mut output, channels: 10 }));
        for frame in output.chunks(10) {
            sent.push(frame[8]);
            elsewhere = frame.iter().enumerate().filter(|(c, _)| *c != 8).fold(elsewhere, |m, (_, x)| m.max(x.abs()));
        }
        player.service().unwrap();
        if let Some(r) = player.hardware_probe_result() {
            result = r.map_err(|e| e.to_string());
            break;
        }
    }
    let measured = result.unwrap_or_else(|e| panic!("{e}"));
    assert!(measured.round_trip_frames.abs_diff(DELAY as u32) <= 1, "{measured:?}");
    assert_eq!(elsewhere, 0.0, "the click played on output 9 only");
    assert!(player.start_hardware_probe(&[10], &[0]).unwrap_err().contains("channel"), "output 11 does not exist on a 10-output driver");
}

#[test]
fn a_driver_reset_request_stops_the_output_and_says_so_once() {
    let fake = model12();
    let cfg = asio_config();
    let player = open_asio(&fake, &cfg);
    let mut recorder = player.open_input(&cfg).unwrap();
    assert_eq!(player.reopen_cause(), None);
    assert!(fake.inject_error(ab::Direction::Output, ab::StreamError::ResetRequested));
    assert!(!player.is_playing());
    assert_eq!(player.trouble(), Some(OutputTrouble::ResetRequested));
    assert_eq!(player.trouble(), None, "said once");
    assert_eq!(player.reopen_cause(), Some(crate::reopen::ReopenCause::DriverReset), "the cause holds: the app reopens the device");
    assert_eq!(player.callback_telemetry().device_lost_errors, 0, "a reset is not a lost device");
    assert!(fake.inject_error(ab::Direction::Input, ab::StreamError::ResetRequested));
    assert_eq!(recorder.trouble(), Some(crate::record::Trouble::Device(crate::RESET_REQUESTED_TEXT.into())));
}

/// The data frames and block align of a WAV file.
fn wav_data_frames(path: &std::path::Path) -> u64 {
    let bytes = std::fs::read(path).unwrap();
    let (mut i, mut align) = (12, 0u16);
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        if id == b"fmt " {
            align = u16::from_le_bytes(bytes[i + 20..i + 22].try_into().unwrap());
        }
        if id == b"data" {
            return (len.min(bytes.len() - i - 8) as u64) / u64::from(align.max(1));
        }
        i += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

/// Escaped defect (review B1 of ASIO slice d1): blocks the driver delivered after its reset
/// request (or after a stall it resumed from) landed in the take until the UI's next frame
/// finished it. The error callback now closes the take's input at once.
#[test]
fn samples_after_a_reset_request_or_a_stall_never_land_in_the_take() {
    use ab::fake::SliceSource;
    const BLOCK: usize = 256;
    for error in [ab::StreamError::ResetRequested, ab::StreamError::Stalled] {
        let fake = model12();
        let cfg = asio_config();
        let player = open_asio(&fake, &cfg);
        let mut recorder = player.open_input(&cfg).unwrap();
        let dir = std::env::temp_dir().join(format!("transit-d1-b1-{}-{error:?}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("take.wav");
        recorder.arm(vec![(soundcraft_model::TrackId(1), vec![0], path.clone())]).unwrap();
        let format = ab::Format::new(48_000, 12, ab::SampleFormat::F32);
        let (before, after) = (vec![0.25f32; BLOCK * 12], vec![0.75f32; BLOCK * 12]);
        for _ in 0..4 {
            assert!(fake.pump_capture(BLOCK, ab::Stamp::INVALID, format, &mut SliceSource { data: &before, channels: 12 }));
        }
        assert!(fake.inject_error(ab::Direction::Input, error));
        assert!(fake.inject_error(ab::Direction::Output, error));
        for _ in 0..4 {
            // The driver keeps calling back until the host stops it (the UI does at its next frame).
            assert!(fake.pump_capture(BLOCK, ab::Stamp::INVALID, format, &mut SliceSource { data: &after, channels: 12 }));
        }
        let (takes, _) = recorder.finish();
        assert_eq!(wav_data_frames(&takes[0].1), 4 * BLOCK as u64, "{error:?}: the take ends at the request");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_driver_the_watchdog_found_stalled_stops_the_output_and_is_reopened_like_a_reset() {
    let fake = model12();
    let cfg = asio_config();
    let player = open_asio(&fake, &cfg);
    let mut recorder = player.open_input(&cfg).unwrap();
    assert!(fake.inject_error(ab::Direction::Output, ab::StreamError::Stalled));
    assert!(!player.is_playing());
    assert_eq!(player.trouble(), Some(OutputTrouble::Stalled));
    assert_eq!(player.reopen_cause(), Some(crate::reopen::ReopenCause::DriverStalled));
    assert_eq!(player.callback_telemetry().device_lost_errors, 0, "a stall is not a lost device");
    assert!(fake.inject_error(ab::Direction::Input, ab::StreamError::Stalled));
    assert_eq!(recorder.trouble(), Some(crate::record::Trouble::Device(crate::DRIVER_STALLED_TEXT.into())));
}

#[test]
fn an_input_opened_on_demand_goes_through_the_native_backend_that_serves_the_output() {
    // Inputs that do not open with the output (native WASAPI without a pairing).
    let native = fake_factory(false).with_capture(2, false);
    let fallback = fake_factory(false);
    let player = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &native, &fallback).unwrap();
    assert_eq!(modes(&native), [(ab::Direction::Output, ab::Mode::Shared, ab::Period::Default)]);
    let recorder = player.open_input(&AudioConfig::default()).expect("the native backend's input opens");
    assert_eq!(native.opens().last().map(|(direction, _)| *direction), Some(ab::Direction::Input));
    assert!(native.running(ab::Direction::Input), "started");
    assert!(fallback.opens().is_empty(), "cpal was not asked");
    assert_eq!((recorder.device_name.as_str(), recorder.channels), ("Fake device", 2));
}

/// #20, K31: the Calibration line's Measure over native ASIO. A click plays on output 2 and comes
/// back on input 2 a scripted number of device frames later; the probe counts sample positions,
/// so a backlog in the input ring (capture blocks with no render between them) does not enter
/// it, and the figure is stored under the open key and handed to both running streams.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_loop_over_native_asio_is_measured_in_sample_positions_and_applied_to_the_running_streams() {
    use ab::fake::{SliceSink, SliceSource};
    const BLOCK: usize = 256;
    const LOOP: usize = 1_065;
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    let attempt = ab::OpenRequest {
        format,
        period: ab::Period::Frames(BLOCK as u32),
        mode: ab::Mode::Shared,
        origin: ab::HostOrigin::default(),
        calibration: None,
    };
    let fake = ab::fake::FakeBackend::new(format).named("ASIO", "Focusrite USB ASIO").with_capture(2, true).with_attempts(vec![attempt]);
    let mut player = open_asio(&fake, &asio_config());
    let key = player.calibration_key.clone().expect("a duplex ASIO open has a calibration key");
    assert_eq!((key.device.as_str(), key.mode.as_str(), key.rate, key.period, key.input), ("Focusrite USB ASIO", "asio", 48_000, 256, None));
    assert_eq!(player.calibration, None, "nothing saved for this key yet");
    assert!(player.start_loop_calibration(&[2], &[1]).is_err(), "a channel the device did not open");
    let (mut output, mut input) = (vec![0.0f32; BLOCK * 2], vec![0.0f32; BLOCK * 2]);
    let mut position = 9_000u64;
    // A backlog in the input ring: capture blocks with no render between them.
    for _ in 0..3 {
        assert!(fake.pump_capture_at(BLOCK, ab::Stamp::INVALID, Some(position), format, &mut SliceSource { data: &input, channels: 2 }));
        position += BLOCK as u64;
    }
    player.start_loop_calibration(&[1], &[1]).unwrap_or_else(|e| panic!("{e}"));
    assert!(player.loop_calibration_running());
    assert!(player.start_hardware_probe(&[1], &[1]).is_err(), "one measurement at a time");
    // Output channel 1 by sample position, as the cable carries it back.
    let mut sent: std::collections::BTreeMap<u64, f32> = std::collections::BTreeMap::new();
    let mut result = None;
    for _ in 0..1_000 {
        for f in 0..BLOCK {
            let at = position + f as u64;
            input[f * 2] = 0.0;
            input[f * 2 + 1] = at.checked_sub(LOOP as u64).and_then(|k| sent.get(&k)).copied().unwrap_or(0.0);
        }
        assert!(fake.pump_capture_at(BLOCK, ab::Stamp::INVALID, Some(position), format, &mut SliceSource { data: &input, channels: 2 }));
        assert!(fake.pump_render_at(BLOCK, ab::Stamp::INVALID, Some(position), &mut SliceSink { data: &mut output, channels: 2 }));
        for (f, frame) in output.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(frame[0], 0.0, "the click plays on the send channel only");
            sent.insert(position + f as u64, frame[1]);
        }
        player.service().unwrap();
        position += BLOCK as u64;
        if let Some(done) = player.loop_calibration_result() {
            result = Some(done);
            break;
        }
    }
    let measured = result.expect("the measurement finished").expect("every click came back");
    assert_eq!((measured.round_trip_frames, measured.spread_frames, measured.found), (LOOP as u32, 0, measured.sent));
    assert!(!player.loop_calibration_running() && !player.loop_calibration_lacks_position());
    let record = player.calibration_record(&measured).expect("a key");
    assert_eq!((record.round_trip_frames, record.spread_frames, record.period, record.mode.as_str()), (1_065, 1, 256, "asio"));
    let calibration = record.to_backend();
    assert!(player.apply_calibration(calibration), "both running streams take it");
    let live = fake.live_calibrations();
    assert_eq!(live, [(ab::Direction::Output, calibration), (ab::Direction::Input, calibration)]);
    assert_eq!(player.calibration, calibration);
    // The next open finds it under the same key.
    let cfg = AudioConfig { calibrations: vec![record], ..asio_config() };
    drop(player);
    let reopened = open_asio(&fake, &cfg);
    assert_eq!(reopened.calibration, calibration);
}

/// A driver that reports no sample position cannot be measured this way; it is said, not hung.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_loop_measurement_without_sample_positions_says_so_and_is_refused_off_asio() {
    use ab::fake::{SliceSink, SliceSource};
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    let fake = ab::fake::FakeBackend::new(format).named("ASIO", "Focusrite USB ASIO").with_capture(2, true);
    let player = open_asio(&fake, &asio_config());
    player.start_loop_calibration(&[1], &[1]).unwrap();
    let (mut output, input) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    for _ in 0..4 {
        fake.pump_capture(256, ab::Stamp::INVALID, format, &mut SliceSource { data: &input, channels: 2 });
        fake.pump_render(256, ab::Stamp::INVALID, &mut SliceSink { data: &mut output, channels: 2 });
    }
    assert!(player.loop_calibration_lacks_position() && player.loop_calibration_result().is_none());
    player.cancel_loop_calibration();
    assert!(!player.loop_calibration_running());
    // Not a native ASIO device: refused.
    let wasapi = fake_factory(false).with_capture(2, true);
    let other = Player::open_native_or_cpal(Arc::new(Session::default()), &AudioConfig::default(), &wasapi, &fake_factory(false)).unwrap();
    assert!(other.start_loop_calibration(&[1], &[1]).unwrap_err().contains("native ASIO"));
}

/// #20 review B1: on ASIO a calibration belongs to the buffer the driver runs, not to the one asked
/// for. A fixed-size driver (or "driver buffer" with the panel's size) runs 256 frames whatever is
/// requested: the measurement is keyed 256, and after the panel moves to 512 the next open, which
/// asks exactly the same, does not apply it.
#[test]
fn an_asio_calibration_is_keyed_and_applied_by_the_buffer_the_driver_runs() {
    let format = ab::Format::new(48_000, 2, ab::SampleFormat::F32);
    let asked =
        ab::OpenRequest { format, period: ab::Period::Frames(128), mode: ab::Mode::Shared, origin: ab::HostOrigin::default(), calibration: None };
    let fake = ab::fake::FakeBackend::new(format)
        .named("ASIO", "Focusrite USB ASIO")
        .with_capture(2, true)
        .with_attempts(vec![asked])
        .with_fixed_buffer(256);
    let player = open_asio(&fake, &asio_config());
    let key = player.calibration_key.clone().expect("a key");
    assert_eq!((key.period, player.device_buffer), (256, Some(256)), "the run's buffer, not the request's 128");
    let measured = crate::hardware_probe::Measured { round_trip_frames: 1_196, spread_frames: 3, found: 8, sent: 8 };
    let record = player.calibration_record(&measured).unwrap();
    assert_eq!(record.period, 256);
    let cfg = AudioConfig { calibrations: vec![record.clone()], ..asio_config() };
    drop(player);
    let again = open_asio(&fake, &cfg);
    assert_eq!(again.calibration, record.to_backend(), "the same buffer: applied");
    drop(again);
    fake.set_fixed_buffer(512);
    let before = fake.live_calibrations().len();
    let moved = open_asio(&fake, &cfg);
    assert_eq!((moved.device_buffer, moved.calibration), (Some(512), None), "the panel moved: not calibrated at 512");
    assert_eq!(fake.live_calibrations().len(), before, "nothing handed to the streams");
    assert_eq!(moved.calibration_key.as_ref().map(|k| k.period), Some(512));
}
