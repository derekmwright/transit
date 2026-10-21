//! Independent ADR 0009 probes. No audio devices or external plug-ins are opened.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, NewInstance, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, SlotGen,
    mark_audio_thread,
};
use soundcraft_model::{ChannelFormat, Insert, Session, TrackKind};
use soundcraft_rt::testing::{CountingAlloc, measure};
use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::thread::ThreadId;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;
const BLOCK: usize = 64;
const N: usize = 6;

struct State {
    id: usize,
    main: ThreadId,
    armed: AtomicBool,
    calls: AtomicUsize,
    raised: AtomicUsize,
    dropped: AtomicUsize,
    wrong_thread: AtomicBool,
    drop_panics: AtomicBool,
}

impl State {
    fn new(id: usize) -> Arc<Self> {
        Arc::new(Self {
            id,
            main: std::thread::current().id(),
            armed: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            raised: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            wrong_thread: AtomicBool::new(false),
            drop_panics: AtomicBool::new(false),
        })
    }
}

struct Payload(Arc<State>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.wrong_thread.fetch_or(self.0.main != std::thread::current().id(), Relaxed);
        self.0.dropped.fetch_add(1, Relaxed);
        if self.0.drop_panics.swap(false, Relaxed) {
            panic!("injected payload destructor panic");
        }
    }
}

struct Probe(Arc<State>);
impl Plugin for Probe {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        soundcraft_dsp::plugin_info("gain").unwrap()
    }
    fn prepare(&mut self, _: f32, _: usize, _: usize) {}
    fn reset(&mut self) {}
    fn set_param(&mut self, _: &str, _: f32) -> bool {
        true
    }
    fn param(&self, _: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        self.0.calls.fetch_add(1, Relaxed);
        if self.0.armed.load(Relaxed) {
            self.0.raised.fetch_add(1, Relaxed);
            std::panic::panic_any(Payload(Arc::clone(&self.0)));
        }
        for ch in io {
            ch.iter_mut().take(frames).for_each(|x| *x = 0.125);
        }
    }
}

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 }
}

fn setup(kinds: &[TrackKind]) -> (MixEngine, PlanCompiler, Session, Vec<Arc<State>>) {
    let mut s = Session::default();
    s.edit.click = false;
    let states: Vec<_> = (0..kinds.len()).map(State::new).collect();
    for (i, &kind) in kinds.iter().enumerate() {
        let id = s.add_track(kind, ChannelFormat::Stereo, None);
        let t = s.track_mut(id).unwrap();
        let insert = Insert::new(format!("clap:refute.{i}"));
        if kind == TrackKind::Instrument {
            t.instrument = Some(insert);
        } else {
            t.mixer.inserts[0] = Some(insert);
        }
    }
    let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
    let mut make = |spec: &InstanceSpec, _: SlotGen| {
        let id: usize = spec.id.strip_prefix("clap:refute.").unwrap().parse().unwrap();
        Some(NewInstance { plugin: Box::new(Probe(Arc::clone(&states[id]))), refused_state: false })
    };
    let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::EMPTY.checked_next().unwrap(), &mut make).unwrap();
    let mut mix = MixEngine::realtime(target());
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    (mix, compiler, s, states)
}

fn render(mix: &mut MixEngine, pos: i64) -> Result<(), Box<dyn Any + Send>> {
    let mut out = vec![vec![0.0; BLOCK]; 2];
    catch_unwind(AssertUnwindSafe(|| mix.render_active(pos, BLOCK, &mut out)))
}

fn assert_retained(states: &[Arc<State>]) {
    assert!(states.iter().all(|s| s.dropped.load(Relaxed) == 0));
}

fn assert_reclaimed(states: &[Arc<State>]) {
    for s in states {
        assert_eq!(s.dropped.load(Relaxed), s.raised.load(Relaxed), "payload {} lost", s.id);
        assert!(!s.wrong_thread.load(Relaxed), "payload {} dropped off main", s.id);
    }
}

#[test]
fn pending_panics_drain_in_render_order_and_recovery_is_not_permanently_silent() {
    let (mut mix, _, _, states) = setup(&[TrackKind::Instrument; N]);
    states.iter().for_each(|s| s.armed.store(true, Relaxed));
    let worker = std::thread::spawn(move || {
        mark_audio_thread();
        let first = render(&mut mix, 0).unwrap_err();
        (mix, first)
    });
    let (mut mix, first) = worker.join().unwrap();
    assert_retained(&states);
    assert_eq!(first.downcast_ref::<Payload>().unwrap().0.id, 0);
    states.iter().for_each(|s| s.armed.store(false, Relaxed));
    drop(first);
    mix.reset();
    for id in 1..N {
        let payload = render(&mut mix, (id * BLOCK) as i64).unwrap_err();
        assert_eq!(payload.downcast_ref::<Payload>().unwrap().0.id, id);
        drop(payload);
    }
    render(&mut mix, (N * BLOCK) as i64).unwrap();
    assert!(states.iter().all(|s| s.calls.load(Relaxed) >= 2));
    drop(mix.into_retired());
    assert_reclaimed(&states);
}

#[test]
fn a_second_fault_never_overwrites_an_unreported_payload() {
    let (mut mix, _, _, states) = setup(&[TrackKind::Instrument; N]);
    states.iter().for_each(|s| s.armed.store(true, Relaxed));
    let worker = std::thread::spawn(move || {
        mark_audio_thread();
        let first = render(&mut mix, 0).unwrap_err();
        let second = render(&mut mix, BLOCK as i64).unwrap_err();
        (mix, first, second)
    });
    let (mix, first, second) = worker.join().unwrap();
    assert_retained(&states);
    assert_eq!(states[0].raised.load(Relaxed), 2);
    assert!(states.iter().skip(1).all(|s| s.raised.load(Relaxed) == 1));
    drop((first, second));
    drop(mix.into_retired());
    assert_reclaimed(&states);
}

#[test]
fn table_replacement_and_track_deletion_return_pending_payloads_without_audio_heap_operations() {
    for delete in [false, true] {
        let (mut mix, mut compiler, mut s, states) = setup(&[TrackKind::Instrument; N]);
        states.iter().for_each(|s| s.armed.store(true, Relaxed));
        let (mut mix, payload) = std::thread::spawn(move || {
            mark_audio_thread();
            let payload = render(&mut mix, 0).unwrap_err();
            (mix, payload)
        })
        .join()
        .unwrap();
        if delete {
            s.tracks.clear();
        } else {
            s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        }
        let generation = compiler.active().checked_next().unwrap();
        let mut env =
            compiler.compile(Arc::new(s), compiler.active(), generation, &mut |_, _| panic!("existing instruments must be reused")).unwrap();
        let (mix, env, stats) = std::thread::spawn(move || {
            mark_audio_thread();
            let (adoption, stats) = measure(|| mix.adopt_plan(&mut env));
            assert_eq!(adoption, Adoption::Adopted);
            (mix, env, stats)
        })
        .join()
        .unwrap();
        assert!(stats.is_zero(), "adoption with pending payloads: {stats:?}");
        assert_retained(&states);
        compiler.reclaim(env);
        assert!(states.iter().skip(1).all(|s| s.dropped.load(Relaxed) == 1));
        let released = compiler.take_released();
        assert_eq!(released.len(), if delete { N } else { 0 });
        drop(payload);
        drop(mix.into_retired());
        assert_reclaimed(&states);
    }
}

#[test]
fn aux_and_master_faults_keep_the_single_payload_for_the_caller() {
    for kind in [TrackKind::Aux, TrackKind::Master] {
        let (mut mix, _, _, states) = setup(&[kind]);
        states[0].armed.store(true, Relaxed);
        let (mix, payload) = std::thread::spawn(move || {
            mark_audio_thread();
            let payload = render(&mut mix, 0).unwrap_err();
            (mix, payload)
        })
        .join()
        .unwrap();
        assert_retained(&states);
        assert_eq!(states[0].raised.load(Relaxed), 1);
        drop(payload);
        drop(mix.into_retired());
        assert_reclaimed(&states);
    }
}

#[test]
fn a_retained_payload_with_a_panicking_destructor_does_not_run_until_control_teardown() {
    let (mut mix, _, _, states) = setup(&[TrackKind::Instrument; N]);
    states.iter().for_each(|s| s.armed.store(true, Relaxed));
    // Only one destructor panics: a second destructor panic during unwind aborts Rust.
    states[N - 1].drop_panics.store(true, Relaxed);
    let (mix, payload) = std::thread::spawn(move || {
        mark_audio_thread();
        let payload = render(&mut mix, 0).unwrap_err();
        (mix, payload)
    })
    .join()
    .unwrap();
    assert_retained(&states);
    drop(payload);
    let retired = mix.into_retired();
    assert!(catch_unwind(AssertUnwindSafe(|| drop(retired))).is_err());
    assert_reclaimed(&states);
}
