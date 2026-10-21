//! A panic out of a strip's processing (a plug-in's) never has its payload dropped on the thread
//! that rendered the strip (every last-owner drop happens on the permitted thread). The
//! render keeps each strip's payload in the strip, re-raises the first one to its caller, and the
//! rest go back to the control side with the strips. Before this, Rayon re-raised one panic of a
//! parallel render and dropped the others' payloads on the render pool's workers.
//!
//! Both paths: the plan on the render pool and serially. The serial run also counts the audio thread's heap operations: the
//! render adds none to what the panics themselves cost (each panic's payload box and the unwind's
//! own exception box, plus one more exception box for the re-raise).
//!
//! The binary installs `soundcraft_rt::testing::CountingAlloc` and a panic hook silent for the
//! instruments' panics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_mix::{
    DeviceGeneration, INSTRUMENT_SLOT, InstanceSpec, MixEngine, NewInstance, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget,
    mark_audio_thread,
};
use soundcraft_model::{ChannelFormat, Insert, Session, TrackId, TrackKind};
use soundcraft_rt::testing::{AllocStats, CountingAlloc, measure};
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;
/// More than `PARALLEL_STRIPS` (4), so the strips render in parallel where a pool exists.
const TRACKS: usize = 6;

/// `MixEngine::force_serial` is process-wide: one test at a time.
static SERIAL: Mutex<()> = Mutex::new(());

struct Shared {
    /// Where each panic payload was dropped.
    payload_drops: Mutex<Vec<ThreadId>>,
    /// Where each panic was raised (reserved: recording one allocates nothing).
    raised: Mutex<Vec<ThreadId>>,
    armed: AtomicBool,
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared { payload_drops: Mutex::default(), raised: Mutex::new(Vec::with_capacity(64)), armed: AtomicBool::new(false) })
    }
}

/// A panic payload that owns something and records the thread it is dropped on.
struct Payload(Arc<Shared>);

impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payload_drops.lock().unwrap().push(std::thread::current().id());
    }
}

/// An instrument that panics in `process` with a [`Payload`] while armed.
struct Panics {
    inner: Box<dyn Plugin>,
    shared: Arc<Shared>,
}

impl Plugin for Panics {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
        self.inner.prepare(sr, mb, ch);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn set_param(&mut self, id: &str, v: f32) -> bool {
        self.inner.set_param(id, v)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if self.shared.armed.load(Ordering::Relaxed) {
            self.shared.raised.lock().unwrap().push(std::thread::current().id());
            std::panic::panic_any(Payload(Arc::clone(&self.shared)));
        }
        self.inner.process(io, frames);
    }
}

fn panics(shared: &Arc<Shared>, ch: usize) -> Box<dyn Plugin> {
    let mut inner = soundcraft_dsp::create("gain").unwrap();
    inner.prepare(SR as f32, BLOCK, ch);
    Box::new(Panics { inner, shared: Arc::clone(shared) })
}

fn session(tracks: usize) -> (Session, Vec<TrackId>) {
    let mut s = Session::default();
    s.edit.click = false;
    let ids = (0..tracks)
        .map(|_| {
            let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
            s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.panics"));
            t
        })
        .collect();
    (s, ids)
}

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 }
}

/// The instruments' panics report nothing (the default hook would format and print them, which
/// allocates on the measured thread); any other panic, a failed assertion, reports as usual.
fn silence_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !info.payload().is::<Payload>() {
                prev(info);
            }
        }));
    });
}

/// A realtime engine with the panicking instruments adopted from a compiled plan (control side),
/// warmed up with a few blocks while disarmed.
fn plan_engine(shared: &Arc<Shared>) -> MixEngine {
    let (s, _) = session(TRACKS);
    let mut mix = MixEngine::realtime(target());
    let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
    let sh = Arc::clone(shared);
    let mut make = move |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| {
        assert!(spec.id == "clap:test.panics" && spec.slot == INSTRUMENT_SLOT, "{}", spec.id);
        Some(NewInstance { plugin: panics(&sh, spec.ch), refused_state: false })
    };
    let mut env = compiler.compile(Arc::new(s), compiler.active(), PlanGeneration::EMPTY.checked_next().unwrap(), &mut make).unwrap();
    assert!(matches!(mix.adopt_plan(&mut env), soundcraft_mix::Adoption::Adopted));
    compiler.reclaim(env);
    let mut out = vec![vec![0.0; BLOCK]; 2];
    for b in 0..4 {
        mix.render_active(b * BLOCK as i64, BLOCK, &mut out);
    }
    mix
}

/// After the render that panicked: nothing was dropped yet; the re-raised payload and the
/// engine's teardown (the control side) drop every payload, all on this thread.
fn assert_all_dropped_here(what: &str, shared: &Shared, panic: Box<dyn Any + Send>, engine: MixEngine) {
    let here = std::thread::current().id();
    let early = shared.payload_drops.lock().unwrap().len();
    assert_eq!(early, 0, "{what}: {early} of {TRACKS} panic payloads dropped during the render");
    assert!(panic.is::<Payload>(), "{what}: the strip's own payload comes out of the render");
    drop(panic);
    drop(engine.into_retired());
    let drops = shared.payload_drops.lock().unwrap().clone();
    assert_eq!(drops.len(), TRACKS, "{what}: every strip's payload was kept and handed back");
    assert!(drops.iter().all(|t| *t == here), "{what}: payloads dropped off the control thread: {drops:?}");
}

fn render_active_panic(mix: &mut MixEngine, pos: i64, out: &mut [Vec<f32>]) -> Box<dyn Any + Send> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mix.render_active(pos, BLOCK, out))).expect_err("the render panics")
}

#[test]
fn parallel_panicking_instruments_drop_no_payload_on_a_render_worker() {
    let _one = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    silence_panics();
    let shared = Shared::new();
    // The pool is built when the first engine is made, off the audio thread.
    let mut mix = plan_engine(&shared);
    let mark = std::thread::spawn(move || {
        MixEngine::force_serial(false);
        mark_audio_thread();
        shared.armed.store(true, Ordering::Relaxed);
        let panic = render_active_panic(&mut mix, 4 * BLOCK as i64, &mut vec![vec![0.0; BLOCK]; 2]);
        (mix, shared, panic)
    });
    let callback = mark.thread().id();
    let (mix, shared, panic) = mark.join().unwrap();
    MixEngine::force_serial(true);
    let workers = soundcraft_mix::render_worker_promotions().iter().filter(|p| p.is_some()).count();
    let raised = shared.raised.lock().unwrap().clone();
    let on_workers = raised.iter().filter(|t| **t != callback).count();
    println!("render pool workers: {workers}; {on_workers} of {} panics raised on a worker", raised.len());
    assert_eq!(raised.len(), TRACKS);
    if workers >= 2 {
        assert_eq!(on_workers, TRACKS, "the strips rendered on the pool");
    }
    assert_all_dropped_here("render pool", &shared, panic, mix);
}

#[test]
fn serial_panicking_instruments_keep_their_payloads_and_allocate_nothing_more() {
    let _one = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    silence_panics();
    let shared = Shared::new();
    let mut mix = plan_engine(&shared);
    MixEngine::force_serial(true);
    let here = std::thread::current().id();
    // What the panics themselves cost on this thread: one strip's panic caught, and a re-raise.
    let calibrate = || {
        let (p, raise) = measure(|| std::panic::catch_unwind(|| std::panic::panic_any(Payload(Arc::clone(&shared)))).unwrap_err());
        let (p, reraise) = measure(|| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| std::panic::resume_unwind(p))).unwrap_err());
        drop(p);
        (raise, reraise)
    };
    let _ = calibrate();
    let (raise, reraise) = calibrate();
    shared.payload_drops.lock().unwrap().clear();
    let n = TRACKS as u64;
    let expected =
        AllocStats { allocs: raise.allocs * n + reraise.allocs, frees: raise.frees * n + reraise.frees, bytes: raise.bytes * n + reraise.bytes };
    shared.armed.store(true, Ordering::Relaxed);
    mark_audio_thread();
    let mut out = vec![vec![0.0; BLOCK]; 2];
    let (panic, stats) = measure(|| render_active_panic(&mut mix, 4 * BLOCK as i64, &mut out));
    println!("one panic {raise:?}, re-raise {reraise:?}; the render {stats:?}");
    assert_eq!(stats, expected, "the render adds no heap operation to the panics' own");
    assert!(shared.raised.lock().unwrap().iter().all(|t| *t == here), "serial: every strip rendered on this thread");
    assert_all_dropped_here("serial", &shared, panic, mix);
}
