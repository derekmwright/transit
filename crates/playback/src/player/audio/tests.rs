#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::{
    MeterSnapshot, OutputCallback,
    device_clock::{ClockKey, Direction},
};
use soundcraft_mix::{DeviceGeneration, InstanceSpec, NewInstance, SlotGen};
use soundcraft_model::Session;
use soundcraft_rt::testing::measure;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicI64, AtomicU32},
};

fn fixture(device_rate: u32) -> (Control<Command, ActionResult>, Audio, Arc<Shared>) {
    fixture_session(device_rate, Arc::new(Session::default()))
}
fn fixture_session(device_rate: u32, session: Arc<Session>) -> (Control<Command, ActionResult>, Audio, Arc<Shared>) {
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicU32::new(1000),
        meters: Mutex::new(MeterSnapshot::default()),
        editors_open: AtomicBool::new(false),
    });
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512 };
    let key = ClockKey { generation: 1, domain: 1, direction: Direction::Output };
    let (mut control, audio) = pair(target, PlanLimits::default(), key, device_rate, Arc::clone(&shared)).unwrap();
    control.request(session, None).unwrap();
    control.service(&mut make);
    (control, audio, shared)
}
fn make(spec: &InstanceSpec, _: SlotGen) -> Option<NewInstance> {
    soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch)
}
fn fill(audio: &mut Audio, out: &mut [f32]) {
    audio.fill(out, 2, &mut plan_bridge::callback_scope(), &mut action_lane::callback_scope()).unwrap();
}

#[test]
fn consumed_playhead_is_independent_of_render_ahead_and_callback_partition() {
    for partitions in [&[1usize, 19, 3, 127, 64, 2][..], &[216][..]] {
        let (mut control, mut audio, shared) = fixture(44_100);
        control.send_action(Command::Play { from: 0, end: None, looped: None }, true).unwrap();
        let mut frames = 0;
        let mut saw_render_ahead = false;
        for &count in partitions {
            let mut out = vec![1.0; count * 2];
            let ((), heap) = measure(|| fill(&mut audio, &mut out));
            assert!(heap.is_zero(), "{heap:?}");
            frames += count;
            let expected = (frames as u64 * 48_000 / 44_100) as i64;
            assert_eq!(shared.position.load(Ordering::Relaxed), expected);
            saw_render_ahead |= audio.transport.render_position > expected;
            assert_eq!(audio.transport.timeline.next_output(), frames as u64);
        }
        assert!(saw_render_ahead);
        control.retire(audio.plans, audio.actions);
    }
}

#[test]
fn seek_past_loop_end_continues_linearly_and_a_seek_back_engages_the_loop() {
    let (mut control, mut audio, shared) = fixture(48_000);
    let looped = Some(Range { start: 0, end: 256 });
    control.send_action(Command::Play { from: 1000, end: None, looped }, true).unwrap();
    let mut out = [0.0; 128];
    fill(&mut audio, &mut out);
    assert_eq!(shared.position.load(Ordering::Relaxed), 1064);
    control.send_action(Command::Locate(240), true).unwrap();
    fill(&mut audio, &mut out);
    assert_eq!(shared.position.load(Ordering::Relaxed), 48);
    assert!(audio.transport.render_position > 48, "the unconsumed loop buffer must not advance the displayed playhead");
    control.retire(audio.plans, audio.actions);
}

#[test]
fn conversion_chunks_share_the_actual_device_callback_action_budget() {
    let (mut control, mut audio, _) = fixture(48_000);
    for _ in 0..64 {
        control.send_action(Command::Recording(true), false).unwrap();
    }
    let mut device = OutputCallback::new(2, 48_000);
    let mut out = vec![1.0; 20_000 * 2];
    let ((), heap) = measure(|| device.fill_with(&mut out, |buffer, plans, actions| audio.fill(buffer, 2, plans, actions).unwrap()));
    assert!(heap.is_zero(), "{heap:?}");
    let mut notices = [None; 128];
    assert_eq!(control.poll_notices(&mut notices), action_lane::CALLBACK_ACTIONS);
    let ((), heap) = measure(|| device.fill_with(&mut out, |buffer, plans, actions| audio.fill(buffer, 2, plans, actions).unwrap()));
    assert!(heap.is_zero(), "{heap:?}");
    assert_eq!(control.poll_notices(&mut notices), action_lane::CALLBACK_ACTIONS);
    control.retire(audio.plans, audio.actions);
}

#[test]
fn stop_cuts_pending_audio_and_invalid_play_does_not_move_the_playhead() {
    let (mut control, mut audio, shared) = fixture(44_100);
    control.send_action(Command::Play { from: 500, end: None, looped: None }, true).unwrap();
    let mut out = [0.0; 2];
    fill(&mut audio, &mut out);
    assert!(audio.pending as f64 > audio.read);
    let stop = control.stop().unwrap();
    let stopped_at = shared.position.load(Ordering::Relaxed);
    fill(&mut audio, &mut out);
    assert_eq!(control.recovery_progress().0, stop);
    assert!(!shared.playing.load(Ordering::Relaxed));
    assert_eq!(shared.position.load(Ordering::Relaxed), stopped_at);
    let invalid = control.send_action(Command::Play { from: MAX_POSITION + 1, end: None, looped: None }, true).unwrap();
    fill(&mut audio, &mut out);
    assert_eq!(shared.position.load(Ordering::Relaxed), stopped_at);
    let mut notices = [None; 8];
    control.poll_notices(&mut notices);
    assert!(
        notices.iter().flatten().any(
            |n| matches!(n, crate::player::Notice::Action(r) if r.sequence == invalid && r.handler_result == Some(ActionResult::InvalidPosition))
        )
    );
    control.retire(audio.plans, audio.actions);
}

#[test]
fn timeline_exhaustion_still_executes_stop_and_faults_the_runtime() {
    let (mut control, mut audio, shared) = fixture(48_000);
    control.send_action(Command::Play { from: 0, end: None, looped: None }, true).unwrap();
    fill(&mut audio, &mut [0.0; 128]);
    audio.transport.epoch = u64::MAX;
    let stop = control.stop().unwrap();
    assert_eq!(
        audio.fill(&mut [0.0; 128], 2, &mut plan_bridge::callback_scope(), &mut action_lane::callback_scope()),
        Err(RenderError::Timeline(timeline_map::MapError::Arithmetic))
    );
    assert!(audio.faulted);
    assert!(!audio.transport.playing);
    assert!(!shared.playing.load(Ordering::Relaxed));
    assert_eq!(control.recovery_progress().0, stop);
    control.retire(audio.plans, audio.actions);
}

#[test]
fn end_is_acknowledged_at_the_consumed_boundary_even_without_another_callback() {
    for (rate, frames) in [(48_000, 64), (44_100, 59)] {
        let (mut control, mut audio, shared) = fixture(rate);
        control.send_action(Command::Play { from: 0, end: Some(64), looped: None }, true).unwrap();
        let mut out = vec![0.0; frames * 2];
        let ((), heap) = measure(|| fill(&mut audio, &mut out));
        assert!(heap.is_zero(), "{heap:?}");
        assert_eq!(shared.position.load(Ordering::Relaxed), 64);
        assert!(!shared.playing.load(Ordering::Relaxed));
        assert!(!audio.transport.flush_releases);
        control.retire(audio.plans, audio.actions);
    }
}

#[test]
fn changed_layout_waits_for_old_render_ahead_and_releases_it_on_control() {
    let (mut control, mut audio, _) = fixture(44_100);
    control.send_action(Command::Play { from: 0, end: None, looped: None }, true).unwrap();
    fill(&mut audio, &mut [0.0; 2]);
    let old = audio.plans.engine().active().generation;
    control.service(&mut make);
    let mut surround = Session::default();
    surround.outputs[0].format = soundcraft_model::ChannelFormat::Surround51;
    control.request(Arc::new(surround), None).unwrap();
    control.service(&mut make);
    assert!(audio.read < audio.pending as f64);
    let ((), heap) = measure(|| fill(&mut audio, &mut [0.0; 2]));
    assert!(heap.is_zero(), "{heap:?}");
    assert_eq!(audio.plans.engine().active().generation, old);
    let ((), heap) = measure(|| fill(&mut audio, &mut [0.0; 2]));
    assert!(heap.is_zero(), "{heap:?}");
    assert!(audio.plans.engine().active().generation > old);
    assert_eq!(audio.plans.output().len(), 6);
    control.service(&mut make);
    control.retire(audio.plans, audio.actions);
}

fn signal() -> Arc<Session> {
    use soundcraft_model::{ChannelFormat, Clip, Insert, SourceAudio, SourceId, TrackKind};
    let mut s = Session::default();
    s.edit.click = false;
    let track = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let channels = (0..2).map(|ch| (0..4096).map(|frame| (frame % 31 + 1) as f32 * (ch + 1) as f32 / 128.0).collect()).collect();
    s.pool.insert(SourceId(1), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels })));
    let clip = s.new_clip_id();
    let t = s.track_mut(track).unwrap();
    t.playlist_mut().unwrap().clips.push(Clip::audio(clip, "pattern", SourceId(1), 0, 0, 4096));
    t.mixer.inserts[0] = Some(Insert::new("gain"));
    Arc::new(s)
}

#[test]
fn prepared_audio_matches_source_across_arbitrary_equal_rate_callbacks_and_loop_boundaries() {
    let (mut control, mut audio, shared) = fixture_session(48_000, signal());
    control.send_action(Command::Play { from: 17, end: None, looped: Some(Range::new(17, 273)) }, true).unwrap();
    let mut index = 17usize;
    for frames in [1, 127, 1027, 3, 511, 128, 2000] {
        let mut out = vec![0.0; frames * 2];
        let ((), heap) = measure(|| fill(&mut audio, &mut out));
        assert!(heap.is_zero(), "{heap:?}");
        for frame in out.as_chunks::<2>().0 {
            if index >= 273 {
                index = 17;
            }
            for (channel, &sample) in frame.iter().enumerate() {
                let expected = (index % 31 + 1) as f32 * (channel + 1) as f32 / 128.0;
                assert!((sample - expected).abs() < 0.00001, "sample {index} channel {channel}: {sample} != {expected}");
            }
            index += 1;
        }
        assert_eq!(shared.position.load(Ordering::Relaxed), if index == 273 { 17 } else { index as i64 });
    }
    control.retire(audio.plans, audio.actions);
}

#[test]
fn a_seek_discards_real_pending_audio_and_session_retirement_runs_off_callback() {
    let session = signal();
    let weak = Arc::downgrade(&session);
    let (mut control, mut audio, shared) = fixture_session(44_100, session);
    control.send_action(Command::Play { from: 17, end: None, looped: None }, true).unwrap();
    let mut out = [0.0; 2];
    fill(&mut audio, &mut out);
    assert!(out.iter().any(|sample| *sample != 0.0));
    assert!(audio.read < audio.pending as f64);
    control.send_action(Command::Locate(10_000), true).unwrap();
    let ((), heap) = measure(|| fill(&mut audio, &mut out));
    assert!(heap.is_zero(), "{heap:?}");
    assert_eq!(out, [0.0, 0.0]);
    assert_eq!(shared.position.load(Ordering::Relaxed), 10_001);
    control.service(&mut make);
    control.request(Arc::new(Session::default()), None).unwrap();
    control.service(&mut make);
    let ((), heap) = measure(|| fill(&mut audio, &mut [0.0; 128]));
    assert!(heap.is_zero(), "{heap:?}");
    assert!(weak.upgrade().is_some(), "retired session still belongs to the return envelope");
    control.service(&mut make);
    assert!(weak.upgrade().is_none(), "control reclaims its final owner");
    control.retire(audio.plans, audio.actions);
}

#[test]
fn a_fault_returns_pending_plans_and_actions_and_closes_control_admission() {
    let (mut control, mut audio, shared) = fixture(48_000);
    control.send_action(Command::Play { from: 0, end: None, looped: None }, true).unwrap();
    fill(&mut audio, &mut [0.0; 128]);
    control.service(&mut make);
    let mut replacement = Session::default();
    replacement.outputs[0].format = soundcraft_model::ChannelFormat::Surround51;
    let replacement = Arc::new(replacement);
    let weak = Arc::downgrade(&replacement);
    let request = control.request(replacement, None).unwrap();
    control.service(&mut make);
    let stop = control.stop().unwrap();
    let later = control.send_action(Command::Recording(true), false).unwrap();
    audio.transport.epoch = u64::MAX;
    let mut out = [1.0; 128];
    let (result, heap) = measure(|| audio.fill(&mut out, 2, &mut plan_bridge::callback_scope(), &mut action_lane::callback_scope()));
    assert_eq!(result, Err(RenderError::Timeline(timeline_map::MapError::Arithmetic)));
    assert!(heap.is_zero(), "{heap:?}");
    assert_eq!(out, [0.0; 128]);
    assert!(!shared.playing.load(Ordering::Relaxed));
    assert!(control.sessions.faulted(), "control observes failure before polling a return");
    assert!(weak.upgrade().is_some(), "the returned owner is still anchored until main polls");
    assert_eq!(control.request(Arc::new(Session::default()), None), Err(crate::player::PlayerError::Faulted));
    assert_eq!(control.send_action(Command::Recording(false), false), Err(crate::player::PlayerError::Faulted));
    assert_eq!(control.stop(), Err(crate::player::PlayerError::Faulted));
    assert_eq!(control.all_notes_off(), Err(crate::player::PlayerError::Faulted));
    for _ in 0..10 {
        fill(&mut audio, &mut out);
        control.service(&mut make);
    }
    assert!(weak.upgrade().is_none());
    assert_eq!(control.recovery_progress().0, stop, "only the Stop whose handler ran is acknowledged");
    let mut notices = [None; 16];
    control.poll_notices(&mut notices);
    assert!(notices.iter().flatten().any(|n| matches!(n, crate::player::Notice::Session(crate::player::SessionNotice { request: id, outcome: crate::player::SessionOutcome::Rejected(crate::player::SessionFailure::Faulted) }) if *id == request)));
    assert!(notices.iter().flatten().any(|n| matches!(n, crate::player::Notice::Action(r) if r.sequence == later && r.outcome == action_lane::Outcome::Faulted && r.handler_result.is_none())));
    control.retire(audio.plans, audio.actions);
}

#[test]
fn a_fault_drains_saturated_action_and_note_lanes_over_successive_callbacks() {
    let (mut control, mut audio, _) = fixture(48_000);
    fill(&mut audio, &mut [0.0; 128]);
    control.service(&mut make);
    control.poll_notices(&mut [None; 8]);
    control.stop().unwrap();
    let mut admitted = Vec::new();
    for _ in 0..action_lane::CAPACITY {
        admitted.push(control.send_action(Command::Recording(true), false).unwrap());
    }
    for _ in 0..action_lane::NOTE_CAPACITY {
        admitted.push(control.send_note(Command::Recording(true), action_lane::NoteKind::On).unwrap());
    }
    admitted.sort_unstable();
    audio.transport.epoch = u64::MAX;
    let mut collected = Vec::new();
    for callback in 0..8 {
        let mut out = [1.0; 128];
        let (result, heap) = measure(|| audio.fill(&mut out, 2, &mut plan_bridge::callback_scope(), &mut action_lane::callback_scope()));
        assert!(heap.is_zero(), "{heap:?}");
        assert_eq!(result, if callback == 0 { Err(RenderError::Timeline(timeline_map::MapError::Arithmetic)) } else { Ok(()) });
        assert_eq!(out, [0.0; 128]);
        assert!(!audio.transport.recording, "no faulted action may reach the handler");
        let mut notices = [None; action_lane::CALLBACK_ACTIONS + action_lane::CALLBACK_NOTES + 1];
        let count = control.poll_notices(&mut notices);
        assert_eq!(count, action_lane::CALLBACK_ACTIONS + action_lane::CALLBACK_NOTES);
        for notice in notices.into_iter().flatten() {
            let crate::player::Notice::Action(receipt) = notice else { panic!("unexpected session result") };
            assert_eq!(receipt.outcome, action_lane::Outcome::Faulted);
            assert_eq!(receipt.handler_result, None);
            collected.push(receipt.sequence);
        }
    }
    collected.sort_unstable();
    assert_eq!(collected, admitted, "every accepted action returns exactly once");
    assert!(control.retire(audio.plans, audio.actions).0.is_empty());
}

#[test]
fn all_propagated_timeline_errors_permanently_fault_and_silence() {
    for speed_exhaustion in [false, true] {
        let (mut control, mut audio, shared) = fixture(48_000);
        let from = if speed_exhaustion { 0 } else { MAX_POSITION - 10 };
        control.send_action(Command::Play { from, end: None, looped: None }, true).unwrap();
        if speed_exhaustion {
            fill(&mut audio, &mut [0.0; 128]);
            audio.transport.epoch = u64::MAX;
            shared.speed.store(500, Ordering::Relaxed);
        }
        let mut out = [1.0; 128];
        let (result, heap) = measure(|| audio.fill(&mut out, 2, &mut plan_bridge::callback_scope(), &mut action_lane::callback_scope()));
        assert_eq!(result, Err(RenderError::Timeline(timeline_map::MapError::Arithmetic)));
        assert!(heap.is_zero());
        assert_eq!(out, [0.0; 128]);
        assert!(audio.faulted && control.sessions.faulted());
        assert!(!shared.playing.load(Ordering::Relaxed));
        let position = shared.position.load(Ordering::Relaxed);
        for _ in 0..3 {
            fill(&mut audio, &mut out);
        }
        assert_eq!(shared.position.load(Ordering::Relaxed), position);
        control.retire(audio.plans, audio.actions);
    }
}

#[test]
fn a_publication_finishing_after_a_concurrent_fault_is_returned_without_adoption() {
    let (mut control, mut audio, shared) = fixture(48_000);
    fill(&mut audio, &mut [0.0; 128]);
    control.service(&mut make);
    let holder = Arc::new(Mutex::new(Some(audio)));
    let held = Arc::clone(&holder);
    let (go, start) = std::sync::mpsc::channel();
    let (done, faulted) = std::sync::mpsc::channel();
    let (drain, finish) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        start.recv().unwrap();
        {
            let mut guard = held.try_lock().unwrap();
            let audio = guard.as_mut().unwrap();
            audio.transport.epoch = u64::MAX;
            shared.speed.store(500, Ordering::Relaxed);
            assert_eq!(
                audio.fill(&mut [1.0; 128], 2, &mut plan_bridge::callback_scope(), &mut action_lane::callback_scope()),
                Err(RenderError::Timeline(timeline_map::MapError::Arithmetic))
            );
        }
        done.send(()).unwrap();
        finish.recv().unwrap();
        let mut guard = held.try_lock().unwrap();
        let audio = guard.as_mut().unwrap();
        let ((), heap) = measure(|| fill(audio, &mut [1.0; 128]));
        assert!(heap.is_zero());
    });
    let late = signal();
    let weak = Arc::downgrade(&late);
    let request = control.request(late, None).unwrap();
    let main = std::thread::current().id();
    control.service(&mut |spec, generation| {
        assert_eq!(std::thread::current().id(), main);
        go.send(()).unwrap();
        faulted.recv().unwrap();
        make(spec, generation)
    });
    assert!(control.sessions.faulted());
    assert!(weak.upgrade().is_some());
    drain.send(()).unwrap();
    worker.join().unwrap();
    control.service(&mut make);
    assert!(weak.upgrade().is_none());
    let audio = holder.lock().unwrap().take().unwrap();
    let notices = control.retire(audio.plans, audio.actions).0;
    assert!(notices.iter().any(|notice| matches!(notice, crate::player::Notice::Session(crate::player::SessionNotice { request: id, outcome: crate::player::SessionOutcome::Rejected(crate::player::SessionFailure::Faulted) }) if *id == request)));
}

struct CountedInstrument {
    inner: Box<dyn soundcraft_dsp::Plugin>,
    resets: Arc<std::sync::atomic::AtomicUsize>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    panic: Option<(std::thread::ThreadId, Arc<std::sync::atomic::AtomicUsize>, Arc<AtomicBool>)>,
}
impl soundcraft_dsp::Plugin for CountedInstrument {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, rate: f32, frames: usize, channels: usize) {
        self.inner.prepare(rate, frames, channels);
    }
    fn reset(&mut self) {
        self.resets.fetch_add(1, Ordering::Relaxed);
        self.inner.reset();
    }
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        self.inner.set_param(id, value)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if let Some((main, drops, on_main)) = &self.panic {
            std::panic::panic_any(PanicOwner { main: *main, drops: Arc::clone(drops), on_main: Arc::clone(on_main) });
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.process(io, frames);
    }
    fn all_notes_off(&mut self) {
        self.inner.all_notes_off();
    }
}

#[test]
fn stop_and_end_reset_the_instrument_and_deliver_its_release_pass_while_unmonitored() {
    use soundcraft_model::{ChannelFormat, Insert, TrackKind};
    use std::sync::atomic::AtomicUsize;
    for end in [false, true] {
        let (mut control, mut audio, _) = fixture(48_000);
        fill(&mut audio, &mut [0.0; 128]);
        control.service(&mut make);
        let mut s = Session::default();
        let track = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(track).unwrap().instrument = Some(Insert::new("subtractive_synth"));
        control.request(Arc::new(s), None).unwrap();
        let resets = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        control.service(&mut |spec, generation| {
            let made = make(spec, generation).unwrap();
            Some(NewInstance {
                plugin: Box::new(CountedInstrument { inner: made.plugin, resets: Arc::clone(&resets), calls: Arc::clone(&calls), panic: None }),
                refused_state: false,
            })
        });
        fill(&mut audio, &mut [0.0; 128]);
        control.service(&mut make);
        let before_resets = resets.load(Ordering::Relaxed);
        let before_calls = calls.load(Ordering::Relaxed);
        if end {
            control.send_action(Command::Play { from: 0, end: Some(64), looped: None }, true).unwrap();
        } else {
            control.stop().unwrap();
        }
        let ((), heap) = measure(|| fill(&mut audio, &mut [0.0; 128]));
        assert!(heap.is_zero());
        assert_eq!(resets.load(Ordering::Relaxed) - before_resets, if end { 2 } else { 1 });
        assert_eq!(calls.load(Ordering::Relaxed) - before_calls, if end { 2 } else { 1 });
        control.retire(audio.plans, audio.actions);
    }
}

#[test]
fn the_linear_converter_never_interpolates_from_unrendered_storage_after_a_short_loop_block() {
    let (mut control, mut audio, _) = fixture_session(44_100, signal());
    control.send_action(Command::Play { from: 17, end: None, looped: Some(Range::new(17, 20)) }, true).unwrap();
    let mut out = [0.0; 6];
    fill(&mut audio, &mut out);
    // The third output sample is fractional but sits at the last valid frame of
    // this 3-frame render. Capacity beyond that frame is NOT a valid neighbor.
    assert!((out[4] - 20.0 / 128.0).abs() < 0.00001);
    assert!((out[5] - 40.0 / 128.0).abs() < 0.00001);
    control.retire(audio.plans, audio.actions);
}

struct PanicOwner {
    main: std::thread::ThreadId,
    drops: Arc<std::sync::atomic::AtomicUsize>,
    on_main: Arc<AtomicBool>,
}
impl Drop for PanicOwner {
    fn drop(&mut self) {
        self.on_main.store(std::thread::current().id() == self.main, Ordering::Relaxed);
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn escaped_processing_panic_stays_owned_until_main_reclaims_the_callback_holder() {
    use soundcraft_model::{ChannelFormat, Insert, TrackKind};
    use std::sync::atomic::AtomicUsize;
    let (mut control, mut audio, _) = fixture(48_000);
    fill(&mut audio, &mut [0.0; 128]);
    control.service(&mut make);
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    session.track_mut(track).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    control.request(Arc::new(session), None).unwrap();
    let main = std::thread::current().id();
    let drops = Arc::new(AtomicUsize::new(0));
    let on_main = Arc::new(AtomicBool::new(false));
    control.service(&mut |spec, generation| {
        let made = make(spec, generation).unwrap();
        Some(NewInstance {
            plugin: Box::new(CountedInstrument {
                inner: made.plugin,
                resets: Arc::new(AtomicUsize::new(0)),
                calls: Arc::new(AtomicUsize::new(0)),
                panic: Some((main, Arc::clone(&drops), Arc::clone(&on_main))),
            }),
            refused_state: false,
        })
    });
    control.send_action(Command::Play { from: 0, end: None, looped: None }, true).unwrap();
    let holder = Arc::new(Mutex::new(Some(audio)));
    let held = Arc::clone(&holder);
    std::thread::spawn(move || {
        let mut guard = held.try_lock().unwrap();
        let audio = guard.as_mut().unwrap();
        let mut out = [1.0; 128];
        assert_eq!(audio.fill(&mut out, 2, &mut plan_bridge::callback_scope(), &mut action_lane::callback_scope()), Err(RenderError::Panicked));
        assert_eq!(out, [0.0; 128]);
        assert!(audio.panic_payload.is_some());
    })
    .join()
    .unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert!(control.sessions.faulted());
    let mut audio = holder.lock().unwrap().take().unwrap();
    drop(audio.panic_payload.take());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(on_main.load(Ordering::Relaxed));
    control.retire(audio.plans, audio.actions);
}
