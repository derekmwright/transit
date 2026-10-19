#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use soundcraft_dsp::{Plugin, PluginInfo};
use soundcraft_mix::DeviceGeneration;
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use soundcraft_rt::testing::measure;
use std::sync::atomic::{AtomicUsize, Ordering};

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512 }
}
fn session() -> Arc<Session> {
    let mut session = Session::default();
    session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("dry"));
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("latency report"));
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    Arc::new(session)
}
// A mutable latency REPORT fixture, not a signal-alignment/calibration oracle.
struct Latency {
    inner: Box<dyn Plugin>,
    value: Arc<AtomicUsize>,
    state: u8,
}
impl Plugin for Latency {
    fn info(&self) -> &'static PluginInfo {
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
    fn process(&mut self, data: &mut [Vec<f32>], frames: usize) {
        self.inner.process(data, frames);
    }
    fn latency(&self) -> usize {
        self.value.load(Ordering::Relaxed)
    }
    fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
        let state = self.state;
        Some(Box::new(move || Some(vec![state])))
    }
}
fn setup(limits: PlanLimits) -> (RuntimeControl<u8, u8>, AudioPorts<u8, u8>, Arc<AtomicUsize>) {
    let (mut runtime, mut ports) = RuntimeControl::new(session(), None, target(), limits).unwrap();
    let value = Arc::new(AtomicUsize::new(100));
    let handles = &mut runtime.handles;
    runtime.control.service(&mut |spec, generation| {
        let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch).unwrap();
        handles.capture(
            (spec.track, spec.slot, generation),
            NewInstance { plugin: Box::new(Latency { inner: made.plugin, value: Arc::clone(&value), state: 0xa1 }), refused_state: false },
        )
    });
    runtime.handles.refresh(&mut runtime.control.sessions);
    adopt(&mut ports.plans);
    runtime.service();
    (runtime, ports, value)
}
fn adopt(plans: &mut plan_bridge::Audio) {
    let (result, heap) = measure(|| plans.service(&mut plan_bridge::callback_scope(), true));
    assert_eq!(result, crate::publication::Service::Returned);
    assert!(heap.is_zero(), "{heap:?}");
}
fn render(plans: &mut plan_bridge::Audio) {
    let ((), heap) = measure(|| plans.render(0, 64));
    assert!(heap.is_zero(), "{heap:?}");
}

#[test]
fn latency_growth_requests_one_control_preparation_and_realigns_without_a_document_edit() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits::default());
    let first = ports.plans.engine().active().generation;
    let initial = runtime.requested_state().0;
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    assert!(ports.plans.engine().status().capacity_exceeded > 0);
    let refresh = runtime.service().latency_refresh.unwrap();
    assert!(refresh > initial);
    assert_eq!(runtime.requested_state(), (refresh, RequestState::Preparing));
    assert_eq!(ports.plans.engine().active().generation, first);
    assert!(runtime.control.sessions.published.is_none(), "refresh was admitted, not compiled in a second pass");
    assert!(runtime.service().latency_refresh.is_none());
    let RequestState::Published(next) = runtime.requested_state().1 else { panic!("refresh publication") };
    for _ in 0..8 {
        assert!(runtime.service().latency_refresh.is_none());
    }
    adopt(&mut ports.plans);
    render(&mut ports.plans);
    assert_eq!(ports.plans.engine().status().capacity_exceeded, 0);
    assert_eq!(ports.plans.engine().active().generation, next);
    assert!(!runtime.service().preparing);
    for _ in 0..8 {
        render(&mut ports.plans);
        assert!(runtime.service().latency_refresh.is_none());
    }
    assert_eq!(runtime.requested_state().0, refresh);
    let notices = runtime.retire(ports).notices;
    assert_eq!(notices.iter().filter(|n| matches!(n, Notice::Session(SessionNotice { outcome: SessionOutcome::Applied { .. }, .. }))).count(), 2);
}

#[test]
fn bounded_results_backpressure_a_refresh_without_losing_the_current_request() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits::default());
    let session = Arc::clone(runtime.requested_session());
    for _ in 1..SESSION_REQUESTS {
        runtime.update_session(Arc::clone(&session)).unwrap();
    }
    runtime.service();
    adopt(&mut ports.plans);
    runtime.service();
    let current = runtime.requested_state();
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    let refused = runtime.service();
    assert_eq!(refused.refresh_error, Some(PlayerError::SessionBackpressure));
    assert_eq!(runtime.requested_state(), current);
    assert!(Arc::ptr_eq(runtime.requested_session(), &session));
    let mut result = [None];
    assert_eq!(runtime.poll_notices(&mut result), 1);
    assert!(runtime.service().latency_refresh.is_some());
    runtime.retire(ports);
}

#[test]
fn unavailable_capacity_does_not_cause_an_endless_recompile_loop() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits { delay_samples: 1024, ..PlanLimits::default() });
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    let refresh = runtime.service().latency_refresh.unwrap();
    runtime.service();
    if matches!(runtime.requested_state().1, RequestState::Published(_)) {
        adopt(&mut ports.plans);
    }
    for _ in 0..16 {
        render(&mut ports.plans);
        assert!(runtime.service().latency_refresh.is_none());
    }
    assert_eq!(runtime.requested_state().0, refresh);
    runtime.retire(ports);
}

#[test]
fn a_failed_internal_refresh_can_retry_once_for_a_new_capacity_need() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits { delay_samples: 1024, ..PlanLimits::default() });
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    let first = runtime.service().latency_refresh.unwrap();
    runtime.service();
    // Exercise an explicitly returned, unadopted internal refresh. The compiler
    // can publish a capacity-limited plan, so low limits alone do not imply refusal.
    assert_eq!(ports.plans.return_pending(&mut plan_bridge::callback_scope()), crate::publication::Service::Returned);
    runtime.service();
    assert!(matches!(runtime.requested_state().1, RequestState::Finished(SessionOutcome::Rejected(_))));
    for _ in 0..8 {
        assert!(runtime.service().latency_refresh.is_none());
    }
    latency.store(30_000, Ordering::Relaxed);
    render(&mut ports.plans);
    let second = runtime.service().latency_refresh.unwrap();
    assert!(second > first);
    runtime.service();
    assert_eq!(ports.plans.return_pending(&mut plan_bridge::callback_scope()), crate::publication::Service::Returned);
    runtime.service();
    for _ in 0..8 {
        assert!(runtime.service().latency_refresh.is_none());
    }
    assert_eq!(runtime.requested_state().0, second);
    runtime.retire(ports);
}

#[test]
fn a_newer_document_is_never_overwritten_by_refreshing_an_older_active_one() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits::default());
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    let newer = Session { name: "user document".into(), ..Session::default() };
    let latest = runtime.update_session(Arc::new(newer)).unwrap();
    let report = runtime.service();
    assert!(report.latency_refresh.is_none());
    assert_eq!(runtime.requested_state().0, latest);
    adopt(&mut ports.plans);
    runtime.service();
    assert_eq!(ports.plans.engine().session().name, "user document");
    assert_eq!(runtime.requested_session().name, "user document");
    runtime.retire(ports);
}

#[test]
fn an_older_plans_latency_cannot_retry_a_refused_new_user_request() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits::default());
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    let newer = Arc::new(Session { name: "refused user document".into(), ..Session::default() });
    let request = runtime.update_session(Arc::clone(&newer)).unwrap();
    runtime.service();
    assert_eq!(ports.plans.return_pending(&mut plan_bridge::callback_scope()), crate::publication::Service::Returned);
    for _ in 0..8 {
        assert!(runtime.service().latency_refresh.is_none());
    }
    assert_eq!(runtime.requested_state().0, request);
    assert!(matches!(runtime.requested_state().1, RequestState::Finished(SessionOutcome::Rejected(_))));
    assert!(Arc::ptr_eq(runtime.requested_session(), &newer));
    runtime.retire(ports);
}

#[test]
fn input_coalescing_and_reopen_refusal_keep_final_owners_on_control() {
    let original = Arc::new(Session::default());
    let (mut runtime, ports) = RuntimeControl::<u8, u8>::new(Arc::clone(&original), None, target(), PlanLimits::default()).unwrap();
    let input = InputRing::new(2, 48_000);
    let weak = Arc::downgrade(&input);
    runtime.set_input(Some(input)).unwrap();
    runtime.set_input(None).unwrap();
    assert!(weak.upgrade().is_none(), "unpublished monitor owner reclaimed immediately on control");
    let wrong = Session { sample_rate: soundcraft_time::SampleRate::new(44_100).unwrap(), ..Session::default() };
    assert_eq!(runtime.update_session(Arc::new(wrong)), Err(PlayerError::NeedsReopen { current_rate: 48_000, requested_rate: 44_100 }));
    assert!(Arc::ptr_eq(runtime.requested_session(), &original));
    assert_eq!(runtime.retire(ports).notices.len(), 3);
}

#[test]
fn source_revocation_and_shutdown_return_with_the_same_runtime_owner() {
    let (mut runtime, mut ports) = RuntimeControl::<u8, u8>::new(Arc::new(Session::default()), None, target(), PlanLimits::default()).unwrap();
    let source = runtime.register_source(3).unwrap();
    let request = runtime.revoke_source(source).unwrap();
    assert!(!runtime.source_admitted(source));
    assert_eq!(runtime.service().source_revocations_pending, 1);
    let (report, heap) = measure(|| {
        ports.sources.service(&mut action_lane::callback_scope(), &mut ports.plans, &mut |port, generation, _| {
            assert_eq!((port, generation), (source.port(), source.generation()));
            assert_eq!(runtime.source_status(request), Ok(sources::State::Pending));
            Ok(())
        })
    });
    assert!(heap.is_zero());
    assert_eq!(report.revoked, 1);
    assert_eq!(runtime.source_status(request), Ok(sources::State::Revoked));
    let second = runtime.register_source(4).unwrap();
    let unprocessed = runtime.revoke_source(second).unwrap();
    let retirement = runtime.retire(ports);
    assert_eq!(
        retirement.sources,
        [
            sources::Receipt { request, outcome: sources::State::Revoked },
            sources::Receipt { request: unprocessed, outcome: sources::State::Shutdown },
        ]
    );
    assert_eq!(retirement.notices.len(), 1);
}

#[test]
fn a_failed_source_handler_closes_all_runtime_admission_without_claiming_success() {
    let (mut runtime, mut ports) = RuntimeControl::<u8, u8>::new(Arc::new(Session::default()), None, target(), PlanLimits::default()).unwrap();
    let source = runtime.register_source(3).unwrap();
    let request = runtime.revoke_source(source).unwrap();
    ports.sources.service(&mut action_lane::callback_scope(), &mut ports.plans, &mut |_, _, _| Err(sources::ApplyError::Refused));
    assert!(runtime.faulted());
    assert_eq!(runtime.source_status(request), Ok(sources::State::Faulted));
    assert_eq!(runtime.send_action(1, false), Err(PlayerError::Faulted));
    assert_eq!(runtime.send_note(1, action_lane::NoteKind::On), Err(PlayerError::Faulted));
    assert_eq!(runtime.update_session(Arc::new(Session::default())), Err(PlayerError::Faulted));
    assert_eq!(runtime.stop(), Err(PlayerError::Faulted));
    assert_eq!(runtime.register_source(3), Err(PlayerError::Faulted));
    let retirement = runtime.retire(ports);
    assert_eq!(retirement.sources, [sources::Receipt { request, outcome: sources::State::Faulted }]);
}

#[test]
fn a_device_failure_can_fault_the_main_owner_without_borrowing_audio() {
    let (mut runtime, mut ports) = RuntimeControl::<u8, u8>::new(Arc::new(Session::default()), None, target(), PlanLimits::default()).unwrap();
    let source = runtime.register_source(0).unwrap();
    let revoke = runtime.revoke_source(source).unwrap();
    runtime.service();
    runtime.fault();
    assert!(runtime.faulted());
    assert_eq!(runtime.source_status(revoke), Ok(sources::State::Faulted));
    assert_eq!(runtime.send_action(1, false), Err(PlayerError::Faulted));
    assert!(ports.plans.faulted());
    let (returned, heap) = measure(|| ports.plans.service(&mut plan_bridge::callback_scope(), true));
    assert!(heap.is_zero());
    assert_eq!(returned, crate::publication::Service::Returned);
    assert_eq!(ports.plans.engine().active().generation, PlanGeneration::EMPTY);
    let retirement = runtime.retire(ports);
    assert_eq!(retirement.sources, [sources::Receipt { request: revoke, outcome: sources::State::Faulted }]);
}

#[test]
fn terminal_preparation_failure_reaches_audio_actions_and_sources_without_borrowing_them() {
    for already_observed in [false, true] {
        let (mut runtime, mut ports, _) = setup(PlanLimits::default());
        let source = runtime.register_source(3).unwrap();
        let request = runtime.revoke_source(source).unwrap();
        let active_source = runtime.register_source(4).unwrap();
        assert!(ports.sources.permits(active_source.port(), active_source.generation()));
        let action = runtime.send_action(42, false).unwrap();
        let note = runtime.send_note(43, NoteKind::On).unwrap();
        let mut replacement = (**runtime.requested_session()).clone();
        let track = replacement.tracks.last_mut().unwrap();
        track.mixer.inserts[0] = Some(Insert::new("factory-panic"));
        let replacement = runtime.update_session(Arc::new(replacement)).unwrap();
        // The same caught factory failure used by Handles::service. Injection is
        // before after_service for a new failure, or before the next public call
        // for an already-observed terminal failure.
        runtime.control.service(&mut |_, _| panic!("replacement factory fault"));
        assert!(runtime.control.sessions.faulted());
        let report = if already_observed { runtime.service() } else { runtime.after_service() };
        assert!(runtime.faulted());
        assert!(report.sources_faulted);
        assert!(report.latency_refresh.is_none());
        assert!(ports.plans.faulted());
        assert!(!ports.sources.permits(active_source.port(), active_source.generation()));
        assert_eq!(runtime.source_status(request), Ok(sources::State::Faulted));
        assert_eq!(runtime.stop(), Err(PlayerError::Faulted));
        let (mut action_calls, mut revoke_calls) = (0, 0);
        let ((), heap) = measure(|| {
            ports.actions.service(&mut action_lane::callback_scope(), &mut ports.plans, &mut |_, _| {
                action_calls += 1;
                0
            });
            ports.sources.service(&mut action_lane::callback_scope(), &mut ports.plans, &mut |_, _, _| {
                revoke_calls += 1;
                Ok(())
            });
        });
        assert!(heap.is_zero());
        assert_eq!((action_calls, revoke_calls), (0, 0));
        let notices = runtime.retire(ports).notices;
        for sequence in [action, note] {
            let matching: Vec<_> = notices
                .iter()
                .filter_map(|n| match n {
                    Notice::Action(receipt) if receipt.sequence == sequence => Some(receipt),
                    _ => None,
                })
                .collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0].outcome, action_lane::Outcome::Faulted);
            assert_eq!(matching[0].handler_result, None);
        }
        assert!(notices.iter().any(|n| matches!(n,
            Notice::Session(SessionNotice { request, outcome: SessionOutcome::Rejected(super::super::SessionFailure::FactoryPanicked) })
            if *request == replacement
        )));
    }
}

// Regression derived from the independent ADR0009 blocking-cleanup probe.

struct BlockingPanicPayload {
    entered: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}
impl Drop for BlockingPanicPayload {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.resume.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
    }
}

#[test]
fn caught_factory_fault_is_published_before_panic_payload_cleanup() {
    let (mut runtime, ports, _) = setup(PlanLimits::default());
    let action = runtime.send_action(37, false).unwrap();
    let pending_source = runtime.register_source(0).unwrap();
    let revoke = runtime.revoke_source(pending_source).unwrap();
    let live_source = runtime.register_source(1).unwrap();
    let mut next = (**runtime.requested_session()).clone();
    next.tracks.last_mut().unwrap().mixer.inserts[0] = Some(Insert::new("invert"));
    runtime.update_session(Arc::new(next)).unwrap();
    let (entered, payload_dropping) = std::sync::mpsc::sync_channel(1);
    let (resume, resumed) = std::sync::mpsc::sync_channel(1);
    let holder = Arc::new(std::sync::Mutex::new(Some(ports)));
    let worker_holder = Arc::clone(&holder);
    let worker = std::thread::spawn(move || {
        payload_dropping.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        let mut guard = worker_holder.lock().unwrap();
        let ports = guard.as_mut().unwrap();
        let audio_faulted = ports.plans.faulted();
        let admits = ports.sources.permits(live_source.port(), live_source.generation());
        let mut calls = 0;
        ports.actions.service(&mut action_lane::callback_scope(), &mut ports.plans, &mut |_, _| {
            calls += 1;
            0
        });
        resume.send(()).unwrap();
        (audio_faulted, admits, calls)
    });
    let mut payload = Some(BlockingPanicPayload { entered, resume: resumed });
    // Same preparation/catch path that Handles::service invokes inside public service().
    runtime.control.service(&mut |_, _| std::panic::panic_any(payload.take().unwrap()));
    runtime.service();
    let observed = worker.join().unwrap();
    assert_eq!(runtime.source_status(revoke), Ok(sources::State::Faulted));
    let ports = holder.lock().unwrap().take().unwrap();
    let notices = runtime.retire(ports).notices;
    let outcome = notices.iter().find_map(|n| match n {
        Notice::Action(r) if r.sequence == action => Some(r.outcome),
        _ => None,
    });
    eprintln!(
        "during caught-panic payload cleanup: audio_faulted={}, source_admits={}, dispatched={}, queued_action_outcome={outcome:?}",
        observed.0, observed.1, observed.2
    );
    assert_eq!(observed, (true, false, 0), "caught terminal fault must publish before arbitrary panic-payload destruction");
}

#[test]
fn a_source_fault_is_published_before_entering_external_host_service() {
    let (mut runtime, mut ports, _) = setup(PlanLimits::default());
    runtime.sources.fault();
    assert!(!ports.plans.faulted(), "this fixture isolates a source-only failure");
    let report = runtime.service_with(|| {
        assert!(ports.plans.faulted(), "entry propagation must precede external host work");
        let mut dispatched = 0;
        ports.actions.service(&mut action_lane::callback_scope(), &mut ports.plans, &mut |_, _| {
            dispatched += 1;
            0
        });
        assert_eq!(dispatched, 0);
        (2, 3, 4)
    });
    assert_eq!((report.plugin_pending, report.plugin_failed, report.plugin_other_thread), (2, 3, 4));
    runtime.retire(ports);
}

#[test]
fn faulted_runtime_does_not_attempt_a_pending_latency_refresh() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits::default());
    let request = runtime.requested_state().0;
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    assert!(ports.plans.engine().status().capacity_exceeded > 0);
    // Collect that report without admitting an automatic refresh, then fault.
    runtime.handles.service(&mut runtime.control);
    assert!(runtime.control.sessions.bridge.status().unwrap().status.required_latency > 0);
    runtime.fault();
    let report = runtime.after_service();
    assert!(report.latency_refresh.is_none());
    assert!(report.refresh_error.is_none(), "a faulted runtime must not even attempt a refresh request");
    assert_eq!(runtime.requested_state().0, request);
    runtime.retire(ports);
}

#[test]
fn a_panicking_factory_payload_destructor_cannot_escape_control_service() {
    struct PanickingCleanup;
    impl Drop for PanickingCleanup {
        fn drop(&mut self) {
            panic!("payload cleanup failed");
        }
    }
    let (mut runtime, ports, _) = setup(PlanLimits::default());
    let mut replacement = (**runtime.requested_session()).clone();
    replacement.tracks.last_mut().unwrap().mixer.inserts[0] = Some(Insert::new("invert"));
    runtime.update_session(Arc::new(replacement)).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.control.service(&mut |_, _| std::panic::panic_any(PanickingCleanup));
    }));
    assert!(result.is_ok(), "payload cleanup must not escape the preparation boundary");
    assert!(ports.plans.faulted());
    assert!(runtime.sources.faulted());
    runtime.retire(ports);
}

#[test]
fn state_capture_tracks_the_callers_document_through_preparation_and_adoption() {
    let (mut runtime, mut ports, value) = setup(PlanLimits::default());
    let original = Arc::clone(runtime.requested_session());
    assert_eq!(runtime.capture_states(&original).unwrap().first().unwrap().2, [0xa1]);
    let mut replacement = (*original).clone();
    replacement.tracks.last_mut().unwrap().mixer.inserts[0] = Some(Insert::new("invert"));
    let replacement = Arc::new(replacement);
    // A UI edit can precede the next update_session call.
    assert_eq!(runtime.capture_states(&replacement), Err(PlayerError::SessionPending));
    runtime.update_session(Arc::clone(&replacement)).unwrap();
    assert_eq!(runtime.capture_states(&original), Err(PlayerError::SessionPending));
    assert_eq!(runtime.capture_states(&replacement), Err(PlayerError::SessionPending));
    let handles = &mut runtime.handles;
    runtime.control.service(&mut |spec, generation| {
        let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch).unwrap();
        handles.capture(
            (spec.track, spec.slot, generation),
            NewInstance { plugin: Box::new(Latency { inner: made.plugin, value: Arc::clone(&value), state: 0xb2 }), refused_state: false },
        )
    });
    assert_eq!(runtime.capture_states(&replacement), Err(PlayerError::SessionPending));
    adopt(&mut ports.plans);
    assert_eq!(runtime.capture_states(&replacement), Err(PlayerError::SessionPending), "wait for main to confirm bindings");
    runtime.service();
    assert_eq!(runtime.capture_states(&replacement).unwrap().first().unwrap().2, [0xb2]);
    assert_eq!(runtime.capture_states(&original), Err(PlayerError::SessionPending));
    runtime.fault();
    assert_eq!(runtime.capture_states(&replacement), Err(PlayerError::Faulted));
    runtime.retire(ports);
}

#[test]
fn rejected_document_cannot_capture_its_predecessor_but_a_failed_internal_refresh_can_save() {
    let (mut runtime, mut ports, latency) = setup(PlanLimits { delay_samples: 1024, ..PlanLimits::default() });
    let original = Arc::clone(runtime.requested_session());
    latency.store(20_000, Ordering::Relaxed);
    render(&mut ports.plans);
    assert!(runtime.service().latency_refresh.is_some());
    assert_eq!(runtime.capture_states(&original).unwrap().first().unwrap().2, [0xa1]);
    runtime.service();
    ports.plans.return_pending(&mut plan_bridge::callback_scope());
    runtime.service();
    assert!(matches!(runtime.requested_state().1, RequestState::Finished(SessionOutcome::Rejected(_))));
    assert_eq!(runtime.capture_states(&original).unwrap().first().unwrap().2, [0xa1]);
    let replacement = Arc::new((*original).clone());
    runtime.update_session(Arc::clone(&replacement)).unwrap();
    runtime.service();
    ports.plans.return_pending(&mut plan_bridge::callback_scope());
    runtime.service();
    assert!(matches!(runtime.requested_state().1, RequestState::Finished(SessionOutcome::Rejected(_))));
    assert_eq!(runtime.capture_states(&replacement), Err(PlayerError::SessionPending));
    runtime.retire(ports);
}
