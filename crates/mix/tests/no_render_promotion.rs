//! `MixEngine::render_active` makes no OS priority call (MMCSS registration is a service call that can
//! block inside the first timed callback). The audio backend promotes the callback thread before
//! its first data callback. Its own test binary: the promotion counter is process-wide.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, SlotGen, create_instance,
    mark_audio_thread,
};
use soundcraft_model::{ChannelFormat, Clip, Session, SourceAudio, SourceId, TrackKind};
use std::sync::Arc;
use std::time::{Duration, Instant};
use transit_rt::testing::promotion_calls;

const FRAMES: usize = 256;

/// An engine that has adopted the plan of `s`.
fn planned_engine(s: &Session) -> MixEngine {
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: FRAMES as u32, out_channels: 0, in_channels: 0 };
    let mut compiler = PlanCompiler::new(target, PlanLimits::default());
    let mut plan = compiler
        .compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut |spec: &InstanceSpec, _: SlotGen| {
            create_instance(&spec.id, spec.state.as_deref(), 48_000.0, FRAMES, spec.ch)
        })
        .unwrap();
    let mut engine = MixEngine::realtime(target);
    assert_eq!(engine.adopt_plan(&mut plan), Adoption::Adopted);
    compiler.reclaim(plan);
    engine
}

fn eight_strips() -> Session {
    let mut s = Session::default();
    let signal: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin() * 0.25).collect();
    s.pool.insert(SourceId(960), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![signal] })));
    for _ in 0..8 {
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "sine", SourceId(960), 0, 0, 48_000));
    }
    s
}

/// Wait until the pool workers' start handlers (which promote them, off the callback) are done:
/// the counter has not moved for a while.
fn settle() -> usize {
    let mut last = promotion_calls();
    let mut since = Instant::now();
    while since.elapsed() < Duration::from_millis(300) {
        std::thread::sleep(Duration::from_millis(20));
        let now = promotion_calls();
        if now != last {
            last = now;
            since = Instant::now();
        }
    }
    last
}

fn render_on_audio_thread(session: Session) -> (usize, usize) {
    let mut engine = planned_engine(&session);
    let before = settle();
    let after = std::thread::spawn(move || {
        mark_audio_thread();
        let mut out = vec![vec![0.0f32; FRAMES]; 2];
        for b in 0..50 {
            engine.render_active((b * FRAMES) as i64, FRAMES, &mut out);
        }
        promotion_calls()
    })
    .join()
    .unwrap();
    (before, after)
}

#[test]
fn render_makes_no_os_promotion_call_serial_and_pool() {
    for serial in [true, false] {
        MixEngine::force_serial(serial);
        let (before, after) = render_on_audio_thread(eight_strips());
        assert_eq!(before, after, "render promoted a thread (force_serial = {serial})");
    }
}
