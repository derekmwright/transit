//! The audio callback never waits on Rayon's global pool (ADR 0012 gate G1, audit finding F01a).
//! Its own test binary, so saturating the global pool here stalls no other test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, SlotGen, create_instance,
    mark_audio_thread,
};
use soundcraft_model::{ChannelFormat, Clip, Session, SourceAudio, SourceId, TrackKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const FRAMES: usize = 512;

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
/// A 512-frame block of sixteen plain strips takes well under a millisecond; this leaves room for
/// a loaded CI machine and is still far below the time the global pool stays busy.
const BOUND: Duration = Duration::from_millis(50);
/// How long the global pool stays busy unless the test releases it earlier.
const BUSY: Duration = Duration::from_secs(3);

fn sixteen_strips() -> Session {
    let mut s = Session::default();
    let signal: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin() * 0.25).collect();
    s.pool.insert(SourceId(950), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![signal] })));
    for _ in 0..16 {
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "sine", SourceId(950), 0, 0, 48_000));
    }
    s
}

/// Keeps every global-pool worker occupied until dropped (also when the test fails). They sleep
/// rather than spin: what is under test is queueing behind the global pool's jobs, not CPU
/// oversubscription (the render pool runs at the default priority; see `build_render_pool`).
struct Saturated(Arc<AtomicBool>);

impl Saturated {
    fn new() -> Self {
        let release = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicUsize::new(0));
        let n = rayon::current_num_threads();
        for _ in 0..n {
            let (release, running) = (release.clone(), running.clone());
            rayon::spawn(move || {
                running.fetch_add(1, Ordering::SeqCst);
                let start = Instant::now();
                while !release.load(Ordering::Relaxed) && start.elapsed() < BUSY {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        }
        let start = Instant::now();
        while running.load(Ordering::SeqCst) < n {
            assert!(start.elapsed() < Duration::from_secs(5), "the global pool never got busy");
            std::thread::yield_now();
        }
        Saturated(release)
    }
}

impl Drop for Saturated {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn audio_thread_render_does_not_wait_for_a_saturated_global_pool() {
    MixEngine::force_serial(false); // the pool is opt-in; this test is about the pool path
    let s = sixteen_strips();
    let mut eng = planned_engine(&s);
    let mut out = vec![vec![0.0f32; FRAMES]; 2];
    // Warm up before the global pool is taken.
    eng.render_active(0, FRAMES, &mut out);
    let busy = Saturated::new();
    let (worst, peak) = std::thread::spawn(move || {
        mark_audio_thread();
        let mut worst = Duration::ZERO;
        let mut peak = 0.0f32;
        for b in 1..=8 {
            let t = Instant::now();
            eng.render_active(b * FRAMES as i64, FRAMES, &mut out);
            worst = worst.max(t.elapsed());
            peak = out.iter().flatten().fold(peak, |m, x| m.max(x.abs()));
        }
        (worst, peak)
    })
    .join()
    .unwrap();
    drop(busy);
    assert!(peak > 0.01, "the strips should sound: {peak}");
    assert!(worst < BOUND, "a 512-frame block of 16 strips took {worst:?} with the global pool saturated");
    eprintln!("worst block with the global pool saturated: {worst:?}");
}

/// Each render pool worker promotes itself at start through `soundcraft-rt` (ADR 0013), and the
/// result is kept per worker. Windows grants MMCSS or time-critical priority to any user, so
/// there every worker must be promoted.
#[test]
fn render_pool_workers_promote_themselves_at_start() {
    MixEngine::force_serial(false); // the pool is opt-in; this test is about its workers
    let _eng = MixEngine::realtime(RenderTarget {
        device: DeviceGeneration(1),
        sample_rate: 48_000,
        max_block: FRAMES as u32,
        out_channels: 0,
        in_channels: 0,
    });
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let workers = cores.saturating_sub(1).min(soundcraft_mix::MAX_RENDER_WORKERS);
    if workers < 2 {
        return; // no pool on this machine
    }
    let start = Instant::now();
    let seen = loop {
        let seen = soundcraft_mix::render_worker_promotions();
        if seen.iter().take(workers).all(Option::is_some) {
            break seen;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "workers never started: {seen:?}");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(seen.iter().skip(workers).all(Option::is_none), "{seen:?}");
    let disabled = std::env::var(soundcraft_mix::RENDER_NO_PRIORITY_ENV).is_ok_and(|v| v.trim() == "1");
    for p in seen.iter().flatten() {
        if disabled {
            assert_eq!(*p, soundcraft_rt::Promotion::Unsupported("disabled"));
        } else if cfg!(windows) {
            assert!(p.is_promoted(), "{seen:?}");
        }
    }
}
