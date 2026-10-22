//! Measured headroom, serial vs render pool, under CPU load (audit F01: "serial is the baseline
//! until the pool beats it in measured headroom"). Ignored by default; a measurement, not a gate:
//!
//! ```text
//! cargo test --release -p soundcraft-mix --test render_headroom -- --ignored --nocapture
//! SOUNDCRAFT_RENDER_NO_PRIORITY=1 cargo test --release -p soundcraft-mix --test render_headroom -- --ignored --nocapture
//! ```
//!
//! The first run promotes the pool workers and the callback thread (as a realtime driver thread
//! would be); the second leaves every thread at the default priority. Each run renders 16 strips
//! (compressor or reverb insert on each) at 128 frames for 2000 blocks, serial and pool, idle and
//! with one busy-spinning normal-priority thread per logical core, one block per 2.67 ms period
//! as a device would call, and prints worst, p99 and median block times against that deadline.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, SlotGen, create_instance,
    mark_audio_thread,
};
use soundcraft_model::{ChannelFormat, Clip, Insert, Session, SourceAudio, SourceId, TrackKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FRAMES: usize = 128;
const BLOCKS: usize = 2000;
const DEADLINE: Duration = Duration::from_micros(2667);

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

fn sixteen_strips() -> Session {
    let mut s = Session::default();
    let len = 48_000usize * 8;
    for k in 0..16u32 {
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let f = 0.003 + 0.0007 * k as f32;
        let signal: Vec<f32> = (0..len).map(|i| (i as f32 * f).sin() * 0.2).collect();
        let src = SourceId(2000 + u64::from(k));
        s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![signal] })));
        let id = s.new_clip_id();
        let tr = s.track_mut(t).unwrap();
        tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "x", src, 0, 0, len as i64));
        tr.mixer.inserts[0] = Some(Insert::new(if k % 2 == 0 { "compressor" } else { "room_reverb" }));
    }
    s
}

/// One normal-priority thread per logical core, spinning until dropped.
struct Load(Arc<AtomicBool>, Vec<std::thread::JoinHandle<()>>);

impl Load {
    fn new() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let handles = (0..n)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 1u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
                    }
                })
            })
            .collect();
        Load(stop, handles)
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
        for h in self.1.drain(..) {
            let _ = h.join();
        }
    }
}

struct Stats {
    worst: Duration,
    p99: Duration,
    median: Duration,
    late: usize,
}

fn run(s: &Session, serial: bool, promote: bool) -> Stats {
    MixEngine::force_serial(serial);
    let s = s.clone();
    let mut eng = planned_engine(&s);
    let mut times = std::thread::spawn(move || {
        let p = if promote { transit_rt::promote_audio_thread() } else { transit_rt::Promotion::Unsupported("disabled") };
        mark_audio_thread();
        let mut out = vec![vec![0.0f32; FRAMES]; 2];
        for b in 0..64 {
            eng.render_active(b * FRAMES as i64, FRAMES, &mut out);
        }
        let mut times = Vec::with_capacity(BLOCKS);
        // Paced like a device callback: one block per period, idle in between (a back-to-back
        // loop would be a 100% duty cycle, which MMCSS throttles and no driver produces).
        let first = Instant::now();
        for (k, b) in (64..64 + BLOCKS).enumerate() {
            if let Some(wait) = (first + DEADLINE * k as u32).checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let t = Instant::now();
            eng.render_active(b as i64 * FRAMES as i64, FRAMES, &mut out);
            times.push(t.elapsed());
        }
        transit_rt::demote(p);
        times
    })
    .join()
    .unwrap();
    MixEngine::force_serial(true); // the default
    times.sort();
    Stats {
        worst: times[times.len() - 1],
        p99: times[times.len() * 99 / 100],
        median: times[times.len() / 2],
        late: times.iter().filter(|t| **t > DEADLINE).count(),
    }
}

#[test]
#[ignore = "measurement: run in release with --ignored --nocapture"]
fn headroom_serial_vs_pool_under_load() {
    let promote = !std::env::var(soundcraft_mix::RENDER_NO_PRIORITY_ENV).is_ok_and(|v| v.trim() == "1");
    let s = sixteen_strips();
    let _warm = planned_engine(&s);
    std::thread::sleep(Duration::from_millis(100));
    println!("priority: {}; workers: {:?}", if promote { "promoted" } else { "default" }, soundcraft_mix::render_worker_promotions());
    println!("{:<6} {:<6} {:>10} {:>10} {:>10} {:>6}", "path", "load", "worst_us", "p99_us", "median_us", "late");
    for loaded in [false, true] {
        for serial in [true, false] {
            let load = loaded.then(Load::new);
            let st = run(&s, serial, promote);
            drop(load);
            println!(
                "{:<6} {:<6} {:>10} {:>10} {:>10} {:>6}",
                if serial { "serial" } else { "pool" },
                if loaded { "busy" } else { "idle" },
                st.worst.as_micros(),
                st.p99.as_micros(),
                st.median.as_micros(),
                st.late
            );
        }
    }
}
