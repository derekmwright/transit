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
    fill(&mut audio, &mut [0.0; 128]);
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
