#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use soundcraft_mix::DeviceGeneration;
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use soundcraft_rt::testing::measure;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512 }
}
fn session(name: &str) -> Arc<Session> {
    Arc::new(Session { name: name.into(), ..Session::default() })
}
fn factory(spec: &InstanceSpec, _: SlotGen) -> Option<NewInstance> {
    soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch)
}
fn adopt(audio: &mut plan_bridge::Audio) {
    let (result, stats) = measure(|| audio.service(&mut plan_bridge::callback_scope(), true));
    assert_eq!(result, crate::publication::Service::Returned);
    assert!(stats.is_zero());
}

#[test]
fn requested_identity_survives_coalescing_without_becoming_a_premature_plan() {
    let (mut control, mut audio) = session_pair(target(), PlanLimits::default());
    let first = control.request(session("first"), None).unwrap();
    assert_eq!(control.state(first), RequestState::Preparing);
    control.service(&mut factory);
    let RequestState::Published(generation) = control.state(first) else { panic!("not published") };
    let obsolete = session("obsolete");
    let weak = Arc::downgrade(&obsolete);
    let second = control.request(obsolete, None).unwrap();
    let third = control.request(session("third"), None).unwrap();
    assert!(weak.upgrade().is_none(), "coalesced owners must leave on control");
    assert_eq!(control.state(second), RequestState::Finished(SessionOutcome::Superseded));
    control.service(&mut |_, _| panic!("no speculative factory while a plan is in flight"));
    assert_eq!(control.state(third), RequestState::Preparing);
    adopt(&mut audio);
    assert_eq!(audio.engine().session().name, "first");
    control.service(&mut factory);
    assert_eq!(control.state(first), RequestState::Finished(SessionOutcome::Applied { generation }));
    assert!(matches!(control.state(third), RequestState::Published(g) if g > generation));
    adopt(&mut audio);
    control.service(&mut factory);
    assert_eq!(audio.engine().session().name, "third");
    let (notices, _) = control.retire(audio);
    assert_eq!(notices.len(), 3);
    assert_eq!(notices.iter().filter(|n| matches!(n.outcome, SessionOutcome::Applied { .. })).count(), 2);
}

#[test]
fn result_backpressure_never_silently_overwrites_an_admitted_request() {
    let (mut control, audio) = session_pair(target(), PlanLimits::default());
    let mut last = None;
    for _ in 0..SESSION_REQUESTS {
        last = Some(control.request(session("queued"), None).unwrap());
    }
    let last = last.unwrap();
    assert_eq!(control.request(session("refused"), None), Err(PlayerError::SessionBackpressure));
    assert_eq!(control.state(last), RequestState::Preparing);
    let mut out = [None; 8];
    let (count, stats) = measure(|| control.poll_notices(&mut out));
    assert!(stats.is_zero());
    assert_eq!(count, 8);
    assert!(out.iter().flatten().all(|n| n.outcome == SessionOutcome::Superseded));
    control.request(session("accepted after collection"), None).unwrap();
    let (notices, _) = control.retire(audio);
    assert_eq!(notices.len(), SESSION_REQUESTS + 1 - 8);
    assert_eq!(notices.iter().filter(|n| n.outcome == SessionOutcome::Rejected(SessionFailure::Shutdown)).count(), 1);
}

#[test]
fn rate_mismatch_and_exhaustion_leave_the_existing_request_intact() {
    let (mut control, audio) = session_pair(target(), PlanLimits::default());
    control.next = u64::MAX - 1;
    let last = control.request(session("last"), None).unwrap();
    let wrong_rate = Session { sample_rate: soundcraft_time::SampleRate::new(44_100).unwrap(), ..Session::default() };
    assert_eq!(control.request(Arc::new(wrong_rate), None), Err(PlayerError::NeedsReopen { current_rate: 48_000, requested_rate: 44_100 }));
    assert_eq!(control.request(session("wrap"), None), Err(PlayerError::SequenceExhausted));
    assert_eq!(control.state(last), RequestState::Preparing);
    assert_eq!(control.retire(audio).0.len(), 1);
}

#[test]
fn a_compile_refusal_has_a_request_result_and_does_not_consume_a_plan_identity() {
    let (mut control, mut audio) = session_pair(target(), PlanLimits { tracks: 0, ..PlanLimits::default() });
    let mut too_large = Session::default();
    too_large.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let refused = control.request(Arc::new(too_large), None).unwrap();
    control.service(&mut |_, _| panic!("resource validation precedes factory"));
    assert!(matches!(
        control.state(refused),
        RequestState::Finished(SessionOutcome::Rejected(SessionFailure::Compile(CompileError::ResourceLimit { .. })))
    ));
    let next = control.request(session("fits"), None).unwrap();
    control.service(&mut factory);
    assert_eq!(control.state(next), RequestState::Published(PlanGeneration::from_raw(1)));
    adopt(&mut audio);
    control.service(&mut factory);
    assert_eq!(control.retire(audio).0.len(), 2);
}

#[test]
fn factory_panic_is_reported_and_permanently_closes_control_admission() {
    let (mut control, audio) = session_pair(target(), PlanLimits::default());
    let mut s = Session::default();
    let track = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    s.track_mut(track).unwrap().mixer.inserts.push(Some(Insert::new("gain")));
    let request = control.request(Arc::new(s), None).unwrap();
    control.service(&mut |_, _| panic!("factory exception"));
    assert_eq!(control.state(request), RequestState::Finished(SessionOutcome::Rejected(SessionFailure::FactoryPanicked)));
    assert!(control.faulted());
    assert_eq!(control.request(session("refused"), None), Err(PlayerError::Faulted));
    assert_eq!(control.retire(audio).0.len(), 1);
}

#[test]
fn shutdown_reports_actual_adoption_and_never_needs_another_callback() {
    for applied in [false, true] {
        let (mut control, mut audio) = session_pair(target(), PlanLimits::default());
        let id = control.request(session("closing"), None).unwrap();
        control.service(&mut factory);
        if applied {
            adopt(&mut audio);
        }
        let (notices, _) = control.retire(audio);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].request, id);
        assert_eq!(
            notices[0].outcome,
            if applied {
                SessionOutcome::Applied { generation: PlanGeneration::from_raw(1) }
            } else {
                SessionOutcome::Rejected(SessionFailure::Shutdown)
            }
        );
    }
}

#[test]
fn player_staging_keeps_play_behind_its_snapshot_but_never_blocks_stop_or_release() {
    let (mut control, mut plans, mut actions) = endpoints::<u32, u32>(target(), PlanLimits::default());
    let first = control.request(session("first"), None).unwrap();
    control.service(&mut factory);
    let waiting_request = control.request(session("not compiled"), None).unwrap();
    let play = control.send_action(10, true).unwrap();
    let release = control.send_note(20, action_lane::NoteKind::Off).unwrap();
    let mut events = [None; 8];
    let mut index = 0;
    let (result, heap) = measure(|| {
        actions.service(&mut action_lane::callback_scope(), &mut plans, &mut |event, _| {
            events[index] = Some(event);
            index += 1;
            99
        })
    });
    assert!(heap.is_zero());
    assert!(!result.waiting_for_plan, "uncompiled transport remains on control");
    assert_eq!(index, 1);
    assert_eq!(events[0], Some(action_lane::Event::Action { sequence: release, payload: 20 }));
    let stop = control.stop().unwrap();
    assert_eq!(control.recovery_progress().0.get(), 0);
    adopt(&mut plans);
    control.service(&mut factory);
    assert!(matches!(control.sessions.state(waiting_request), RequestState::Published(_)));
    adopt(&mut plans);
    control.service(&mut factory);
    let (_, heap) = measure(|| {
        actions.service(&mut action_lane::callback_scope(), &mut plans, &mut |event, _| {
            assert_eq!(event, action_lane::Event::Stop { sequence: stop });
            100
        })
    });
    assert!(heap.is_zero());
    assert_eq!(control.recovery_progress().0, stop);
    let mut notices = [None; 8];
    let (count, heap) = measure(|| control.poll_notices(&mut notices));
    assert!(heap.is_zero());
    assert_eq!(count, 4);
    assert!(notices.contains(&Some(Notice::Action(action_lane::Receipt {
        sequence: play,
        payload: 10,
        outcome: action_lane::Outcome::Stopped,
        handler_result: None,
    }))));
    assert!(notices.contains(&Some(Notice::Action(action_lane::Receipt {
        sequence: release,
        payload: 20,
        outcome: action_lane::Outcome::Applied,
        handler_result: Some(99),
    }))));
    assert!(
        notices
            .iter()
            .flatten()
            .any(|n| matches!(n, Notice::Session(SessionNotice { request, outcome: SessionOutcome::Applied { .. } }) if *request == first))
    );
    assert!(control.retire(plans, actions).0.is_empty());
}

#[test]
fn coalesced_and_refused_snapshots_finish_their_own_actions_even_after_notices_are_collected() {
    let (mut control, mut plans, mut actions) = endpoints::<u32, u32>(target(), PlanLimits { tracks: 0, ..PlanLimits::default() });
    control.request(session("blocking publication"), None).unwrap();
    control.service(&mut factory);
    let second = control.request(session("coalesced"), None).unwrap();
    let obsolete = control.send_action(1, true).unwrap();
    let mut large = Session::default();
    large.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let third = control.request(Arc::new(large), None).unwrap();
    let refused = control.send_action(2, true).unwrap();
    let mut notices = [None; 8];
    assert_eq!(control.poll_notices(&mut notices), 2);
    assert!(
        notices
            .iter()
            .flatten()
            .any(|n| matches!(n, Notice::Session(SessionNotice { request, outcome: SessionOutcome::Superseded }) if *request == second))
    );
    assert!(
        notices
            .iter()
            .flatten()
            .any(|n| matches!(n, Notice::Action(r) if r.sequence == obsolete && r.outcome == action_lane::Outcome::PlanSuperseded))
    );
    adopt(&mut plans);
    control.service(&mut factory);
    assert_eq!(control.poll_notices(&mut notices), 3);
    assert!(notices.iter().flatten().any(|n| matches!(n, Notice::Session(SessionNotice { request, outcome: SessionOutcome::Rejected(SessionFailure::Compile(_)) }) if *request == third)));
    assert!(
        notices.iter().flatten().any(|n| matches!(n, Notice::Action(r) if r.sequence == refused && r.outcome == action_lane::Outcome::PlanRejected))
    );
    // The current document remains refused after its notice was collected. Never
    // silently apply a new Play to the older session that is still active in audio.
    let retry = control.send_action(3, true).unwrap();
    assert_eq!(control.poll_notices(&mut notices), 1);
    assert!(matches!(notices[0], Some(Notice::Action(r)) if r.sequence == retry && r.outcome == action_lane::Outcome::PlanRejected));
    actions.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _| panic!("refused transport must not reach the handler"));
    assert!(control.retire(plans, actions).0.is_empty());
}

#[test]
fn admitted_play_uses_exact_plan_and_preserves_sequence_across_compilation() {
    let (mut control, mut plans, mut actions) = endpoints::<u32, u32>(target(), PlanLimits::default());
    let request = control.request(session("play target"), None).unwrap();
    let play = control.send_action(7, true).unwrap();
    let recording = control.send_action(8, false).unwrap();
    control.service(&mut factory);
    let mut budget = action_lane::callback_scope();
    let result = actions.service(&mut budget, &mut plans, &mut |_, _| panic!("the requested plan is not active"));
    assert!(result.waiting_for_plan);
    adopt(&mut plans);
    let mut next = 7;
    let (_, heap) = measure(|| {
        actions.service(&mut budget, &mut plans, &mut |event, _| {
            let action_lane::Event::Action { sequence, payload } = event else { panic!("ordinary action") };
            assert_eq!(payload, next);
            assert_eq!(sequence, if next == 7 { play } else { recording });
            next += 1;
            payload * 10
        })
    });
    assert!(heap.is_zero());
    assert_eq!(next, 9);
    control.service(&mut factory);
    let mut notices = [None; 1];
    let mut sessions = 0;
    let mut receipts = 0;
    while control.poll_notices(&mut notices) != 0 {
        match notices[0].unwrap() {
            Notice::Session(n) => {
                assert_eq!(n.request, request);
                sessions += 1;
            }
            Notice::Action(r) => {
                assert_eq!(r.handler_result, Some(r.payload * 10));
                receipts += 1;
            }
        }
    }
    assert_eq!((sessions, receipts), (1, 2));
    assert!(control.retire(plans, actions).0.is_empty());
}

struct TestEditor {
    id: u8,
    open: bool,
    events: Arc<std::sync::Mutex<Vec<(u8, &'static str, std::thread::ThreadId)>>>,
}
impl soundcraft_dsp::PluginEditor for TestEditor {
    fn open(&mut self) -> Result<(), String> {
        self.open = true;
        Ok(())
    }
    fn close(&mut self) {
        self.open = false;
        self.events.lock().unwrap().push((self.id, "close", std::thread::current().id()));
    }
    fn is_open(&self) -> bool {
        self.open
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        vec![("id".into(), f32::from(self.id))]
    }
}
struct HandlePlugin {
    plugin: Box<dyn soundcraft_dsp::Plugin>,
    id: u8,
    events: Arc<std::sync::Mutex<Vec<(u8, &'static str, std::thread::ThreadId)>>>,
    panic_reader: bool,
}
impl Drop for HandlePlugin {
    fn drop(&mut self) {
        self.events.lock().unwrap().push((self.id, "drop", std::thread::current().id()));
    }
}
impl soundcraft_dsp::Plugin for HandlePlugin {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.plugin.info()
    }
    fn prepare(&mut self, rate: f32, block: usize, channels: usize) {
        self.plugin.prepare(rate, block, channels);
    }
    fn reset(&mut self) {
        self.plugin.reset();
    }
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        self.plugin.set_param(id, value)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.plugin.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        self.plugin.process(io, frames);
    }
    fn editor(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginEditor>> {
        Some(Box::new(TestEditor { id: self.id, open: false, events: Arc::clone(&self.events) }))
    }
    fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
        assert!(!self.panic_reader, "reader factory panic");
        let id = self.id;
        Some(Box::new(move || Some(vec![id])))
    }
}
fn with_effect(id: &str) -> Arc<Session> {
    let mut s = Session::default();
    let track = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    s.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new(id));
    Arc::new(s)
}

#[test]
fn handles_follow_confirmed_instance_generation_and_returned_pool_releases() {
    let main = std::thread::current().id();
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut control, mut plans, actions) = endpoints::<u8, u8>(target(), PlanLimits::default());
    let mut handles = Handles::default();
    let original = with_effect("gain");
    let track = original.tracks[0].id;
    let mut identity = 0u8;
    let mut prepare = |control: &mut Control<u8, u8>, handles: &mut Handles| {
        control.service(&mut |spec, generation| {
            identity += 1;
            let made = factory(spec, generation).unwrap();
            handles.capture(
                (spec.track, spec.slot, generation),
                NewInstance {
                    plugin: Box::new(HandlePlugin { plugin: made.plugin, id: identity, events: Arc::clone(&events), panic_reader: false }),
                    refused_state: true,
                },
            )
        });
        handles.refresh(&mut control.sessions);
    };
    control.request(Arc::clone(&original), None).unwrap();
    prepare(&mut control, &mut handles);
    assert!(!handles.has_editor(track, 0), "a prepared instance is not yet active");
    adopt(&mut plans);
    prepare(&mut control, &mut handles);
    let binding = handles.binding(track, 0).unwrap();
    assert!(handles.capture_states().is_empty(), "retain refused stored state until the exact editor opens");
    handles.open_editor(track, 0).unwrap();
    assert_eq!(handles.capture_states(), vec![(track, 0, vec![1])]);
    let mut replacement = (*original).clone();
    replacement.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("invert"));
    control.request(Arc::new(replacement), None).unwrap();
    prepare(&mut control, &mut handles);
    assert_eq!(handles.binding(track, 0), Some(binding));
    assert_eq!(handles.idle(), vec![(track, 0, "id".into(), 1.0)]);
    // Return the replacement without adoption. It enters the compiler's reuse pool;
    // neither publication nor pool retention authorizes exposing its handles.
    let (_, heap) = measure(|| plans.return_pending(&mut plan_bridge::callback_scope()));
    assert!(heap.is_zero());
    prepare(&mut control, &mut handles);
    assert_eq!(handles.binding(track, 0), Some(binding));
    control.request(original, None).unwrap();
    prepare(&mut control, &mut handles);
    assert_eq!(handles.capture_states(), vec![(track, 0, vec![1])]);
    assert!(handles.editor_open(track, 0));
    assert!(events.lock().unwrap().iter().any(|&(id, event, _)| id == 2 && event == "close"));
    adopt(&mut plans);
    prepare(&mut control, &mut handles);
    let (_, released) = control.retire(plans, actions);
    handles.release(released);
    assert!(!handles.has_editor(track, 0));
    let events = events.lock().unwrap();
    assert_eq!(events.iter().filter(|(_, event, _)| *event == "drop").count(), 2);
    assert!(events.iter().all(|(_, _, thread)| *thread == main));
}

#[test]
fn handle_factory_unwind_leaves_no_partial_visible_instance() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut control, plans, actions) = endpoints::<u8, u8>(target(), PlanLimits::default());
    let mut handles = Handles::default();
    let s = with_effect("gain");
    let track = s.tracks[0].id;
    control.request(s, None).unwrap();
    control.service(&mut |spec, generation| {
        let made = factory(spec, generation).unwrap();
        handles.capture(
            (spec.track, spec.slot, generation),
            NewInstance {
                plugin: Box::new(HandlePlugin { plugin: made.plugin, id: 1, events: Arc::clone(&events), panic_reader: true }),
                refused_state: false,
            },
        )
    });
    handles.refresh(&mut control.sessions);
    assert!(!handles.has_editor(track, 0));
    assert!(handles.capture_states().is_empty());
    assert!(control.sessions.faulted());
    let (_, released) = control.retire(plans, actions);
    handles.release(released);
    assert_eq!(events.lock().unwrap().iter().filter(|(_, event, _)| *event == "drop").count(), 1);
}
