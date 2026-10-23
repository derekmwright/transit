#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use soundcraft_dsp::{Plugin, PluginInfo};
use soundcraft_mix::DeviceGeneration;
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use std::sync::atomic::{AtomicUsize, Ordering};
use transit_rt::testing::measure;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 }
}
fn session() -> Arc<Session> {
    let mut session = Session::default();
    session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("dry"));
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("latency report"));
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    Arc::new(session)
}
// A mutable latency REPORT fixture, not a signal-alignment/calibration oracle.
struct ConstantEditor(u8);
impl soundcraft_dsp::PluginEditor for ConstantEditor {
    fn open(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn close(&mut self) {}
    fn is_open(&self) -> bool {
        false
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        vec![("fixture".into(), f32::from(self.0))]
    }
}
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
    fn editor(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginEditor>> {
        Some(Box::new(ConstantEditor(self.state)))
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

#[test]
fn explicit_same_topology_restore_changes_capture_lineage_but_cow_update_keeps_it() {
    let (mut runtime, mut ports, value) = setup(PlanLimits::default());
    let original = Arc::clone(runtime.requested_session());
    let bindings = runtime.control.sessions.active_bindings();
    assert_eq!(runtime.idle_editors()[0].3, f32::from(0xa1u8));
    runtime.replace_session(Arc::clone(&original)).unwrap();
    assert!(runtime.idle_editors().is_empty(), "old-document editor edits must be discarded");
    assert_eq!(runtime.capture_states(&original), Err(PlayerError::SessionPending));
    let handles = &mut runtime.handles;
    runtime.control.service(&mut |spec, generation| {
        let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch).unwrap();
        handles.capture(
            (spec.track, spec.slot, generation),
            NewInstance { plugin: Box::new(Latency { inner: made.plugin, value: Arc::clone(&value), state: 0xb2 }), refused_state: false },
        )
    });
    adopt(&mut ports.plans);
    runtime.service();
    assert_ne!(runtime.control.sessions.active_bindings(), bindings);
    assert_eq!(runtime.capture_states(&original).unwrap()[0].2, [0xb2]);
    assert_eq!(runtime.idle_editors()[0].3, f32::from(0xb2u8));
    let restored = runtime.control.sessions.active_bindings();
    let mut view = Arc::clone(&original);
    Arc::make_mut(&mut view).edit.zoom.scroll = 64;
    assert_eq!(runtime.capture_states(&view), Err(PlayerError::SessionPending));
    runtime.update_session(Arc::clone(&view)).unwrap();
    runtime.service();
    adopt(&mut ports.plans);
    runtime.service();
    assert_eq!(runtime.control.sessions.active_bindings(), restored);
    assert_eq!(runtime.capture_states(&view).unwrap()[0].2, [0xb2]);
    assert_eq!(runtime.capture_states(&original), Err(PlayerError::SessionPending));
    runtime.retire(ports);
}

#[test]
fn reopen_capture_preserves_confirmed_fault_state_without_aliasing_pending_documents() {
    let (mut runtime, ports, _) = setup(PlanLimits::default());
    let original = Arc::clone(runtime.requested_session());
    let other = Arc::new((*original).clone());
    assert_eq!(runtime.capture_states_for_reopen(&other), Err(PlayerError::SessionPending));
    runtime.fault();
    assert_eq!(runtime.capture_states(&original), Err(PlayerError::Faulted));
    assert_eq!(runtime.capture_states_for_reopen(&original).unwrap().unwrap()[0].2, [0xa1]);
    assert_eq!(runtime.capture_states_for_reopen(&other).unwrap(), None);
    runtime.retire(ports);

    let (mut runtime, ports, _) = setup(PlanLimits::default());
    let replacement = Arc::new((**runtime.requested_session()).clone());
    runtime.replace_session(Arc::clone(&replacement)).unwrap();
    assert_eq!(runtime.capture_states_for_reopen(&replacement), Err(PlayerError::SessionPending));
    runtime.fault();
    assert_eq!(runtime.capture_states_for_reopen(&replacement).unwrap(), None);
    runtime.retire(ports);
}

#[test]
fn editor_handles_follow_a_moved_insert() {
    let (mut runtime, mut ports, value) = setup(PlanLimits::default());
    let original = Arc::clone(runtime.requested_session());
    let track = original.tracks.last().unwrap().id;
    assert!(runtime.has_editor(track, 0));
    let mut moved = (*original).clone();
    assert!(moved.track_mut(track).unwrap().move_insert(0, 2));
    let moved = Arc::new(moved);
    runtime.update_session(Arc::clone(&moved)).unwrap();
    let handles = &mut runtime.handles;
    runtime.control.service(&mut |spec, generation| {
        let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch).unwrap();
        handles.capture(
            (spec.track, spec.slot, generation),
            NewInstance { plugin: Box::new(Latency { inner: made.plugin, value: Arc::clone(&value), state: 0xb2 }), refused_state: false },
        )
    });
    adopt(&mut ports.plans);
    runtime.service();
    assert!(runtime.has_editor(track, 2), "moved insert's editor handle must be reachable at its new slot");
    assert_eq!(runtime.capture_states(&moved).unwrap().first().map(|s| s.1), Some(2));
    runtime.retire(ports);
}

#[test]
fn a_path_table_edit_is_a_plan_on_the_same_engine_and_the_callback_allocates_nothing() {
    let (mut runtime, mut ports, _) = setup(PlanLimits::default());
    let engine = ports.plans.engine().active().target;
    let first = ports.plans.engine().active().generation;
    let mut io = soundcraft_model::IoMap::for_device("test", "test", 0, 2);
    io.add_path(soundcraft_model::IoPath {
        name: "Cue".into(),
        direction: soundcraft_model::IoDirection::Output,
        channels: vec![0],
        format: ChannelFormat::Mono,
    })
    .unwrap();
    let request = runtime.set_io(Arc::new(io)).unwrap();
    assert_eq!(runtime.requested_state(), (request, RequestState::Preparing));
    runtime.service();
    let RequestState::Published(next) = runtime.requested_state().1 else { panic!("the table is published as a plan") };
    adopt(&mut ports.plans);
    render(&mut ports.plans);
    assert_eq!(ports.plans.engine().active().generation, next);
    assert!(next > first);
    assert_eq!(ports.plans.engine().active().target, engine, "same device generation: no reopen");
    runtime.retire(ports);
}

/// A factory standing in for a heavy third-party plug-in: it counts its calls and takes `delay`.
/// A factory that sleeps `delay` (a heavy plug-in's creation) and counts its calls and the thread
/// it ran on.
fn slow_factory(delay: std::time::Duration, made: &Arc<AtomicUsize>) -> super::handles::Factory {
    let made = Arc::clone(made);
    Arc::new(move |spec: &soundcraft_mix::InstanceSpec| {
        made.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(delay);
        soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch)
    })
}

fn synth_session(with_instrument: bool) -> (Arc<Session>, soundcraft_model::TrackId) {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("keys"));
    if with_instrument {
        session.track_mut(track).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    }
    (Arc::new(session), track)
}

fn energy(plans: &plan_bridge::Audio) -> f32 {
    plans.output().iter().flatten().map(|s| s * s).sum()
}

/// Plays a note for a few blocks and returns the output energy.
fn play(plans: &mut plan_bridge::Audio, track: soundcraft_model::TrackId) -> f32 {
    plans.live_note(track, 60, 100);
    let mut total = 0.0;
    for _ in 0..8 {
        plans.render(0, 64);
        total += energy(plans);
    }
    total
}

/// A frame may take this long at most while a plug-in loads. Load-tolerant: a starved runner can
/// stall a frame, but one that waited for the plug-in takes its full 500 ms.
const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// Runs 5 ms "frames" of `service_with` until nothing is loading; returns the frames and the
/// longest one.
fn frames_until_loaded(
    runtime: &mut RuntimeControl<u8, u8>,
    third_party: &dyn Fn(&str) -> bool,
    factory: &super::handles::Factory,
) -> (u32, std::time::Duration) {
    let (start, mut n, mut worst) = (std::time::Instant::now(), 0u32, std::time::Duration::ZERO);
    while runtime.any_loading() && start.elapsed() < std::time::Duration::from_secs(20) {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let t = std::time::Instant::now();
        runtime.handles.service_with(&mut runtime.control, third_party, factory);
        worst = worst.max(t.elapsed());
        n += 1;
    }
    (n, worst)
}

/// A session whose second request adds the instrument; returns the runtime after the first plan.
fn with_instrument_requested() -> (RuntimeControl<u8, u8>, AudioPorts<u8, u8>, soundcraft_model::TrackId, PlanGeneration) {
    let (before, track) = synth_session(false);
    let (mut runtime, mut ports) = RuntimeControl::<u8, u8>::new(before, None, target(), PlanLimits::default()).unwrap();
    runtime.service();
    adopt(&mut ports.plans);
    runtime.service();
    let first = ports.plans.engine().active().generation;
    // Same track id: the second session adds only the instrument.
    let mut edited = (**runtime.requested_session()).clone();
    edited.track_mut(track).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    runtime.update_session(Arc::new(edited)).unwrap();
    (runtime, ports, track, first)
}

#[test]
fn a_slow_instance_is_made_on_the_host_thread_while_frames_advance_and_the_plan_waits_for_it() {
    let (mut runtime, mut ports, track, first) = with_instrument_requested();
    let made = Arc::new(AtomicUsize::new(0));
    let thread = Arc::new(std::sync::Mutex::new(None));
    let slow = slow_factory(std::time::Duration::from_millis(500), &made);
    let seen = Arc::clone(&thread);
    let factory: super::handles::Factory = Arc::new(move |spec: &soundcraft_mix::InstanceSpec| {
        *seen.lock().unwrap() = Some(std::thread::current().id());
        slow(spec)
    });
    let slot = soundcraft_mix::INSTRUMENT_SLOT;
    let third_party = |id: &str| id == "subtractive_synth";

    // Frame 1: announced, nothing made, nothing compiled, and the slot reads as loading.
    let started = std::time::Instant::now();
    runtime.handles.service_with(&mut runtime.control, &third_party, &factory);
    assert!(started.elapsed() < FRAME_BUDGET, "the announcing frame does not wait for the plug-in");
    assert_eq!(made.load(Ordering::SeqCst), 0);
    assert!(runtime.loading(track, slot) && runtime.any_loading());
    assert!(runtime.control.sessions.published.is_none(), "the request waits uncompiled");
    assert_eq!(runtime.requested_state().1, RequestState::Preparing);

    // The slot is silent, not garbage, while it loads: the active plan has no instrument.
    assert_eq!(play(&mut ports.plans, track), 0.0);
    assert_eq!(ports.plans.engine().active().generation, first);

    let (frames, worst) = frames_until_loaded(&mut runtime, &third_party, &factory);
    assert!(started.elapsed() >= std::time::Duration::from_millis(500));
    if transit_plugin_host::INLINE_BY_DEFAULT {
        // macOS (ADR 0018 section 7): the default host runs inline, so the frame that submits
        // the instance makes it on this thread and has it at once.
        assert_eq!(frames, 1, "made in the submitting frame");
        assert_eq!(*thread.lock().unwrap(), Some(std::thread::current().id()), "made on the calling thread");
    } else {
        // Later frames submit it to the host thread and poll: none of them waits for the 500 ms.
        assert!(frames >= 10, "only {frames} frames ran during a 500 ms creation");
        assert!(worst < FRAME_BUDGET, "a frame took {worst:?}");
        assert_ne!(*thread.lock().unwrap(), Some(std::thread::current().id()), "made on the plug-in host thread");
    }
    assert_eq!(made.load(Ordering::SeqCst), 1);
    assert!(!runtime.loading(track, slot) && !runtime.any_loading() && !runtime.stuck(track, slot));
    assert!(runtime.control.sessions.published.is_some(), "the plan took the instance and was published");

    // Bound: it sounds. Making it again for the compile would be a second call.
    adopt(&mut ports.plans);
    assert!(play(&mut ports.plans, track) > 0.0, "the instrument sounds once its plan is adopted");
    assert_eq!(made.load(Ordering::SeqCst), 1, "the compile used the staged instance");
    runtime.retire(ports);
}

/// Renders 512-frame blocks on a thread standing in for the audio callback while the plug-in host
/// thread sits in a 500 ms plug-in factory and the control thread polls. Returns the blocks that
/// began and ended inside the creation and the worst block time.
fn render_while_an_instance_is_made() -> (u32, std::time::Duration) {
    let (mut runtime, ports, _, _) = with_instrument_requested();
    let made = Arc::new(AtomicUsize::new(0));
    let slow = slow_factory(std::time::Duration::from_millis(500), &made);
    // True for the whole factory call, so blocks are counted across the sleep as well as the load.
    let in_factory = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&in_factory);
    let factory: super::handles::Factory = Arc::new(move |spec: &soundcraft_mix::InstanceSpec| {
        flag.store(true, Ordering::Release);
        let made = slow(spec);
        flag.store(false, Ordering::Release);
        made
    });
    let third_party = |id: &str| id == "subtractive_synth";
    runtime.handles.service_with(&mut runtime.control, &third_party, &factory);

    // 512 frames at 48 kHz is a 10.7 ms budget. The audio thread renders while the host thread
    // sits in the 500 ms factory; no lock is shared between them.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut ports = ports;
    let audio = {
        let stop = Arc::clone(&stop);
        let in_factory = Arc::clone(&in_factory);
        let mut plans = ports.plans;
        std::thread::spawn(move || {
            let (mut during, mut worst) = (0u32, std::time::Duration::ZERO);
            while !stop.load(Ordering::Acquire) {
                let creating_before = in_factory.load(Ordering::Acquire);
                let t = std::time::Instant::now();
                plans.service(&mut plan_bridge::callback_scope(), true);
                plans.render(0, 512);
                worst = worst.max(t.elapsed());
                during += u32::from(creating_before && in_factory.load(Ordering::Acquire));
                std::thread::sleep(std::time::Duration::from_micros(2_000));
            }
            (during, worst, plans)
        })
    };
    frames_until_loaded(&mut runtime, &third_party, &factory);
    std::thread::sleep(std::time::Duration::from_millis(50));
    stop.store(true, Ordering::Release);
    let (during, worst, plans) = audio.join().unwrap();
    ports.plans = plans;
    runtime.retire(ports);
    (during, worst)
}

/// The claim: the creation path shares no lock with the callback. Evidence that does not depend on
/// the scheduler: the callback kept completing blocks for the whole 500 ms creation (about 200 at
/// an ideal 2.5 ms per block; a block waiting on the creation would leave a handful). The strict
/// timing check is the ignored hardware-timing companion below.
#[test]
fn the_audio_callback_keeps_rendering_while_an_instance_is_made() {
    let (during, _) = render_while_an_instance_is_made();
    assert!(during >= 20, "the callback completed only {during} blocks during the 500 ms creation");
}

/// Wall-clock companion for quiet hardware (hardware-evidence convention: timing budgets are
/// checked by hand, not on loaded CI runners): no block over 10 ms against the 10.7 ms budget.
/// Run with `cargo test -p soundcraft-playback -- --ignored hardware_timing`.
#[test]
#[ignore = "wall-clock timing; run on an idle machine"]
fn hardware_timing_the_audio_callback_keeps_its_budget_while_an_instance_is_made() {
    let (during, worst) = render_while_an_instance_is_made();
    assert!(during >= 20, "the callback completed only {during} blocks during the 500 ms creation");
    assert!(worst < std::time::Duration::from_millis(10), "worst block {worst:?} against a 10.7 ms budget");
}

#[test]
fn a_request_that_no_longer_wants_a_staged_instance_drops_it() {
    let (before, track) = synth_session(false);
    let (mut runtime, mut ports) = RuntimeControl::<u8, u8>::new(before, None, target(), PlanLimits::default()).unwrap();
    runtime.service();
    adopt(&mut ports.plans);
    runtime.service();
    let base = Arc::clone(runtime.requested_session());
    let mut edited = (*base).clone();
    edited.track_mut(track).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    runtime.update_session(Arc::new(edited)).unwrap();
    let made = Arc::new(AtomicUsize::new(0));
    let factory = slow_factory(std::time::Duration::ZERO, &made);
    let third_party = |id: &str| id == "subtractive_synth";
    runtime.handles.service_with(&mut runtime.control, &third_party, &factory);
    assert!(runtime.loading(track, soundcraft_mix::INSTRUMENT_SLOT));
    // Undo before it is made: nothing is created and nothing is left loading.
    runtime.update_session(base).unwrap();
    runtime.handles.service_with(&mut runtime.control, &third_party, &factory);
    assert!(!runtime.any_loading());
    assert_eq!(made.load(Ordering::SeqCst), 0);
    runtime.retire(ports);
}

/// ADR 0018 section 5: a plug-in that never returns from its creation costs its slot, not the
/// UI: no frame waits for it, the slot reads stuck after the host's timeout, and the session is
/// compiled without it. Releasing the plug-in later destroys the late instance on its thread.
#[test]
fn a_creation_that_never_returns_is_stuck_after_the_timeout_and_the_session_compiles_without_it() {
    let (mut runtime, mut ports, track, _) = with_instrument_requested();
    runtime.handles = Handles::with_host(transit_plugin_host::Host::new(transit_plugin_host::Config {
        timeout: std::time::Duration::from_millis(300),
        inline: false,
        ..transit_plugin_host::Config::default()
    }));
    let (release, hang) = std::sync::mpsc::channel::<()>();
    let hang = std::sync::Mutex::new(hang);
    let factory: super::handles::Factory = Arc::new(move |spec: &soundcraft_mix::InstanceSpec| {
        let _ = hang.lock().unwrap().recv(); // until the test lets go
        soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch)
    });
    let slot = soundcraft_mix::INSTRUMENT_SLOT;
    let third_party = |id: &str| id == "subtractive_synth";
    runtime.handles.service_with(&mut runtime.control, &third_party, &factory);
    assert!(runtime.loading(track, slot));
    let (frames, worst) = frames_until_loaded(&mut runtime, &third_party, &factory);
    assert!(frames >= 10, "frames ran meanwhile ({frames})");
    assert!(worst < FRAME_BUDGET, "a frame took {worst:?}");
    assert!(runtime.stuck(track, slot) && runtime.any_stuck(), "the slot reads as not responding");
    assert!(!runtime.loading(track, slot));
    assert_eq!(runtime.handles.host_wedged(), 1);
    assert!(runtime.control.sessions.published.is_some(), "the session compiled without the stuck instance");
    adopt(&mut ports.plans);
    assert_eq!(play(&mut ports.plans, track), 0.0, "the stuck slot is silent");
    // Another edit of the same document keeps the mark and does not try again.
    runtime.update_session(Arc::clone(runtime.requested_session())).unwrap();
    runtime.handles.service_with(&mut runtime.control, &third_party, &factory);
    assert!(runtime.stuck(track, slot) && !runtime.any_loading());
    drop(release);
    runtime.retire(ports);
}

/// B1 of the ADR 0018 review: after the host thread was slow past its timeout (a modal dialog, a
/// long state restore) and returned, a save reads every live state again; it does not fall back
/// to the stored ones.
#[test]
fn a_save_after_the_host_thread_returns_from_a_stall_carries_the_live_state() {
    let (mut runtime, mut ports, track, _) = with_instrument_requested();
    runtime.handles = Handles::with_host(transit_plugin_host::Host::new(transit_plugin_host::Config {
        timeout: std::time::Duration::from_millis(300),
        inline: false,
        ..transit_plugin_host::Config::default()
    }));
    let value = Arc::new(AtomicUsize::new(0));
    let factory: super::handles::Factory = Arc::new(move |spec: &soundcraft_mix::InstanceSpec| {
        let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch)?;
        Some(NewInstance { plugin: Box::new(Latency { inner: made.plugin, value: Arc::clone(&value), state: 0x5a }), refused_state: false })
    });
    let third_party = |id: &str| id == "subtractive_synth";
    runtime.handles.service_with(&mut runtime.control, &third_party, &factory);
    frames_until_loaded(&mut runtime, &third_party, &factory);
    adopt(&mut ports.plans);
    runtime.service();
    let session = Arc::clone(runtime.requested_session());
    let live = vec![(track, soundcraft_mix::INSTRUMENT_SLOT, vec![0x5a])];
    assert_eq!(runtime.capture_states(&session).unwrap(), live);
    // A 700 ms call on the instance's own thread, against a 300 ms timeout.
    let host = runtime.handles.host().clone();
    let stall = host.submit(|_| std::thread::sleep(std::time::Duration::from_millis(700)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while host.wedged() == 0 && std::time::Instant::now() < deadline {
        let _ = stall.poll();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(host.wedged(), 1, "past its timeout the thread reads wedged");
    while host.wedged() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(host.wedged(), 0, "it returned");
    assert_eq!(runtime.capture_states(&session).unwrap(), live, "the save carries the live state");
    runtime.retire(ports);
}
