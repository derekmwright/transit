//! Plan contract tests that need the crate's internals: forged (malformed) plans for every
//! reject reason, atomic rejection, injected commit faults, identity and limits.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::active::FAULT_AT;
use super::plan::{Attach, CellKind, CellMove};
use super::*;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_model::{Insert, SourceId};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Weak};
use std::thread::ThreadId;

/// Counts heap operations on threads a test measures (all other threads are only forwarded).
#[global_allocator]
static ALLOC: soundcraft_rt::testing::CountingAlloc = soundcraft_rt::testing::CountingAlloc;

/// Adopts on this thread, measured: the audio side allocates and frees nothing.
fn adopt_measured(eng: &mut MixEngine, env: &mut PlanEnvelope) -> Adoption {
    let (a, stats) = soundcraft_rt::testing::measure(|| eng.adopt_plan(env));
    assert!(stats.is_zero(), "adopt_plan allocated or freed: {stats:?}");
    a
}

const SR: u32 = 48_000;
const BLOCK: usize = 256;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(7), sample_rate: SR, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 }
}

/// Where and on which thread test instances were dropped.
#[derive(Default)]
struct Log {
    drops: Mutex<Vec<ThreadId>>,
    made: AtomicUsize,
    /// Latency every `clap:test.latent` instance reports.
    latency: AtomicUsize,
    /// `set_sample` unwinds (armed after the instances are made).
    panic_on_sample: AtomicBool,
    panic_on_drop: AtomicBool,
    panic_on_process: AtomicBool,
}

type SharedLog = Arc<Log>;

/// A test instance: a built-in (or a gain for `clap:test.*` ids) that records its drop.
struct Probe {
    inner: Box<dyn Plugin>,
    log: SharedLog,
    latent: bool,
}

impl Plugin for Probe {
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
        if self.log.panic_on_process.load(Ordering::Relaxed) {
            panic!("plug-in panics in process");
        }
        self.inner.process(io, frames);
    }
    fn latency(&self) -> usize {
        if self.latent { self.log.latency.load(Ordering::Relaxed) } else { self.inner.latency() }
    }
    fn tail_samples(&self) -> usize {
        self.inner.tail_samples()
    }
    fn process_status(&self) -> ProcessStatus {
        self.inner.process_status()
    }
    fn gain_reduction_db(&self) -> f32 {
        self.inner.gain_reduction_db()
    }
    fn set_transport(&mut self, t: &Transport) {
        self.inner.set_transport(t);
    }
    fn note_on(&mut self, o: usize, n: u8, v: u8) {
        self.inner.note_on(o, n, v);
    }
    fn note_off(&mut self, o: usize, n: u8) {
        self.inner.note_off(o, n);
    }
    fn midi(&mut self, o: usize, m: [u8; 3]) {
        self.inner.midi(o, m);
    }
    fn all_notes_off(&mut self) {
        self.inner.all_notes_off();
    }
    fn set_sample(&mut self, s: Option<Arc<dyn SampleSource>>) -> bool {
        if self.log.panic_on_sample.load(Ordering::Relaxed) {
            panic!("test sampler refuses its sample");
        }
        self.inner.set_sample(s)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.log.drops.lock().unwrap().push(std::thread::current().id());
        if self.log.panic_on_drop.swap(false, Ordering::Relaxed) {
            panic!("plug-in panics in drop");
        }
    }
}

fn factory(log: &SharedLog) -> impl FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance> + use<> {
    let log = Arc::clone(log);
    move |spec: &InstanceSpec, _generation: SlotGen| {
        let base = if spec.id.starts_with("clap:test.") { "gain" } else { spec.id.as_str() };
        if spec.id == "clap:test.missing" {
            return None;
        }
        let n = create_instance(base, None, SR as f32, BLOCK, spec.ch)?;
        log.made.fetch_add(1, Ordering::Relaxed);
        let latent = spec.id == "clap:test.latent";
        Some(NewInstance { plugin: Box::new(Probe { inner: n.plugin, log: Arc::clone(&log), latent }), refused_state: false })
    }
}

/// A realtime engine with its compiler, fed the way playback feeds them (N = 1).
struct Rig {
    eng: MixEngine,
    comp: PlanCompiler,
    log: SharedLog,
    pos: i64,
    out: Vec<Vec<f32>>,
    issued: u64,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            eng: MixEngine::realtime(target()),
            comp: PlanCompiler::new(target(), PlanLimits::default()),
            log: SharedLog::default(),
            pos: 0,
            out: vec![vec![0.0; BLOCK]; 2],
            issued: 0,
        }
    }

    fn compile(&mut self, s: &Session) -> PlanEnvelope {
        self.issued += 1;
        let mut make = factory(&self.log);
        self.comp.compile(Arc::new(s.clone()), self.comp.active(), PlanGeneration::from_raw(self.issued), &mut make).unwrap()
    }

    fn publish(&mut self, s: &Session) -> Adoption {
        let mut env = self.compile(s);
        let a = self.eng.adopt_plan(&mut env);
        self.comp.reclaim(env);
        a
    }

    /// Renders `blocks` blocks; returns the left channel.
    fn render(&mut self, blocks: usize) -> Vec<f32> {
        let mut left = Vec::new();
        for _ in 0..blocks {
            self.eng.render_active(self.pos, BLOCK, &mut self.out);
            left.extend_from_slice(&self.out[0]);
            self.pos += BLOCK as i64;
        }
        left
    }

    fn drops(&self) -> usize {
        self.log.drops.lock().unwrap().len()
    }
}

/// Everything observable about a realtime engine's plan, pointers included.
fn digest(eng: &MixEngine) -> Vec<u64> {
    let mut d = Vec::new();
    let p = eng.plan.as_deref().unwrap();
    d.push(p.id.generation.get());
    d.push(p.id.base.get());
    d.push(Arc::as_ptr(&p.session) as usize as u64);
    d.push(u64::from(p.faulted));
    d.push(p.tables.cells.len() as u64);
    for st in &p.tables.strips {
        d.extend([st.generation.get(), st.id.0, st.buf.as_ptr() as usize as u64, st.latency as u64]);
        d.extend([st.tap.len as u64, st.tap.idx as u64, st.tap.bufs.as_ptr() as usize as u64, u64::from(st.tap.live)]);
        let slots = st.plugins.iter().chain(std::iter::once(&st.instrument)).chain(st.clip_fx.0.iter().flat_map(|(_, fx)| [&fx.eq, &fx.comp]));
        for s in slots {
            match s {
                Some(s) => d.extend([
                    s.generation.get(),
                    &*s.plugin as *const dyn Plugin as *const () as usize as u64,
                    s.applied.heap.as_ptr() as usize as u64,
                    s.applied.heap.len() as u64,
                    s.sample_audio.as_ref().map_or(0, |a| Arc::as_ptr(a) as usize as u64),
                    s.events.as_deref().map_or(0, |e| e as *const _ as usize as u64),
                ]),
                None => d.push(u64::MAX),
            }
        }
    }
    d
}

/// What an envelope owns: its Session, every instance in its tables, its moves and attachments.
fn holdings(env: &PlanEnvelope) -> (usize, usize, usize, usize, usize) {
    let p = &env.plan;
    (
        Arc::as_ptr(&p.session) as usize,
        p.tables.as_ref().map_or(0, |t| t.plugins().count()),
        p.tables.as_ref().map_or(0, |t| t.strips.len()),
        p.moves.len(),
        p.attach.len(),
    )
}

fn audio_session() -> (Session, TrackId, TrackId) {
    let mut s = Session::default();
    let a = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let b = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let len = 48_000usize;
    // A burst, then silence: inserts ring into the tail.
    let signal: Vec<f32> = (0..len).map(|i| if i < 4_000 { (i as f32 * 0.03).sin() * 0.5 } else { 0.0 }).collect();
    s.pool.insert(SourceId(1), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![signal.clone(), signal] })));
    let id = s.new_clip_id();
    s.track_mut(a).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(1), 0, 0, len as i64));
    (s, a, b)
}

fn builtin(id: &str) -> Insert {
    let mut ins = Insert::new(id);
    for p in soundcraft_dsp::plugin_info(id).unwrap().params {
        ins.params.insert(p.id.to_string(), p.default);
    }
    ins
}

/// An active plan with two reused inserts on track `a`, and a structural plan that keeps both
/// and adds a fresh third (so it has slot moves, a strip move, and a fresh slot).
fn rig_with_pending() -> (Rig, Session, PlanEnvelope) {
    let (mut s, a, _) = audio_session();
    let ins = &mut s.track_mut(a).unwrap().mixer.inserts;
    ins[0] = Some(builtin("room_reverb"));
    ins[1] = Some(builtin("compressor"));
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    rig.render(4);
    let mut s2 = s.clone();
    let ins = &mut s2.track_mut(a).unwrap().mixer.inserts;
    ins[2] = Some(builtin("compressor"));
    // The reverb's parameter map grows past the inline storage: an `Attach::Applied`.
    for i in 0..70 {
        ins[0].as_mut().unwrap().params.insert(format!("extra{i:02}"), 0.5);
    }
    let env = rig.compile(&s2);
    assert_eq!(env.plan.attach.len(), 1);
    (rig, s2, env)
}

fn slot_moves(env: &PlanEnvelope) -> Vec<usize> {
    env.plan.moves.iter().enumerate().filter(|(_, m)| m.kind == CellKind::Insert).map(|(i, _)| i).collect()
}

/// Adopting `env` is refused for `reason`; the engine is exactly as before, and the envelope
/// still holds its entire input. Then a twin that never saw the plan renders the same.
fn assert_rejected(mut rig: Rig, mut env: PlanEnvelope, reason: RejectReason) {
    let before = digest(&rig.eng);
    let held = holdings(&env);
    let drops = rig.drops();
    assert_eq!(adopt_measured(&mut rig.eng, &mut env), Adoption::Rejected(reason));
    assert_eq!(env.state(), EnvelopeState::Rejected { reason, active: rig.eng.active().generation });
    assert_eq!(digest(&rig.eng), before, "the engine is unchanged");
    assert_eq!(holdings(&env), held, "the envelope holds its entire input");
    assert_eq!(rig.drops(), drops, "nothing dropped while refusing");
    assert_eq!(rig.eng.adopt_plan(&mut env), Adoption::AlreadyServiced);
    assert_eq!(digest(&rig.eng), before);
    rig.comp.reclaim(env);
    assert_eq!(rig.comp.active(), rig.eng.active().generation, "the compiler's shadow stays at the active generation");
}

/// The rejected engine and an engine that never saw the plan render the same afterwards.
fn twin_renders_match(corrupt: impl Fn(&mut PlanEnvelope)) {
    let (mut rig, _, mut env) = rig_with_pending();
    let (mut twin, _, twin_env) = rig_with_pending();
    corrupt(&mut env);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    twin.comp.reclaim(twin_env);
    assert_eq!(rig.render(8), twin.render(8), "bit-identical to an engine that never saw the plan");
}

#[test]
fn a_plan_with_moves_adopts_and_returns_what_it_displaced() {
    let (mut rig, _, mut env) = rig_with_pending();
    assert_eq!(slot_moves(&env).len(), 2, "both inserts are reused");
    assert!(env.plan.moves.iter().any(|m| m.kind == CellKind::Strip), "the strip's state moves");
    assert_eq!(adopt_measured(&mut rig.eng, &mut env), Adoption::Adopted);
    assert_eq!(env.state(), EnvelopeState::Returned { replaced: PlanGeneration::from_raw(1) });
    assert_eq!(rig.eng.adopt_plan(&mut env), Adoption::AlreadyServiced);
    // The returned tables hold no instance: both moved, none was replaced.
    assert_eq!(holdings(&env).1, 0);
    rig.comp.reclaim(env);
    assert_eq!(rig.comp.active(), rig.eng.active().generation);
    assert_eq!(rig.eng.plan.as_deref().unwrap().tables.plugins().count(), 3);
}

#[test]
fn rejects_a_stale_base() {
    let (rig, _, mut env) = rig_with_pending();
    env.plan.id.base = PlanGeneration::EMPTY;
    assert_rejected(rig, env, RejectReason::BaseMismatch);
}

#[test]
fn rejects_a_generation_that_does_not_advance() {
    let (rig, _, mut env) = rig_with_pending();
    env.plan.id.generation = env.plan.id.base;
    assert_rejected(rig, env, RejectReason::BaseMismatch);
}

#[test]
fn rejects_each_target_field() {
    for f in 0..3 {
        let (rig, _, mut env) = rig_with_pending();
        let t = &mut env.plan.id.target;
        match f {
            0 => t.device = DeviceGeneration(8),
            1 => t.sample_rate = 44_100,
            _ => t.max_block = 512,
        }
        assert_rejected(rig, env, RejectReason::TargetMismatch);
    }
}

#[test]
fn rejects_a_move_out_of_range() {
    let (rig, _, mut env) = rig_with_pending();
    let i = slot_moves(&env)[0];
    env.plan.moves[i].from = 9_999;
    assert_rejected(rig, env, RejectReason::MoveOutOfRange);
    let (rig, _, mut env) = rig_with_pending();
    env.plan.moves[i].to = 9_999;
    assert_rejected(rig, env, RejectReason::MoveOutOfRange);
}

#[test]
fn rejects_duplicate_sources() {
    let (rig, _, mut env) = rig_with_pending();
    let [a, b] = slot_moves(&env)[..] else { panic!() };
    let mut moves = env.plan.moves.to_vec();
    moves[b].from = moves[a].from;
    env.plan.moves = moves.into_boxed_slice();
    assert_rejected(rig, env, RejectReason::DuplicateSource);
}

#[test]
fn rejects_two_moves_into_one_vacant_destination() {
    let (rig, _, mut env) = rig_with_pending();
    let [a, b] = slot_moves(&env)[..] else { panic!() };
    let to = env.plan.moves[a].to;
    env.plan.moves[b].to = to;
    assert_rejected(rig, env, RejectReason::DuplicateDestination);
    twin_renders_match(|env| {
        let [a, b] = slot_moves(env)[..] else { panic!() };
        env.plan.moves[b].to = env.plan.moves[a].to;
    });
}

#[test]
fn rejects_a_source_that_holds_nothing() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.missing"));
    s.track_mut(a).unwrap().mixer.inserts[1] = Some(builtin("compressor"));
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    let mut s2 = s.clone();
    s2.track_mut(a).unwrap().mixer.inserts[2] = Some(builtin("gain"));
    let mut env = rig.compile(&s2);
    // The missing instance's cell is in the active plan, empty: point the compressor's move at it.
    let old = &rig.eng.plan.as_deref().unwrap().tables;
    let empty = old.cells.iter().position(|c| c.shape.kind == CellKind::Insert && old.slot(c).is_some_and(Option::is_none)).unwrap();
    let i = slot_moves(&env)[0];
    env.plan.moves[i].from = empty as u16;
    assert_rejected(rig, env, RejectReason::SourceNotLive);
}

#[test]
fn rejects_a_stale_slot_generation() {
    let (rig, _, mut env) = rig_with_pending();
    let i = slot_moves(&env)[0];
    env.plan.moves[i].expect = SlotGen(9_999);
    assert_rejected(rig, env, RejectReason::SlotGenMismatch);
    let (rig, _, mut env) = rig_with_pending();
    let i = env.plan.moves.iter().position(|m| m.kind == CellKind::Strip).unwrap();
    env.plan.moves[i].expect = SlotGen(9_999);
    assert_rejected(rig, env, RejectReason::SlotGenMismatch);
}

#[test]
fn rejects_a_destination_that_is_filled() {
    let (rig, _, mut env) = rig_with_pending();
    let new = env.plan.tables.as_ref().unwrap();
    let fresh = new.cells.iter().position(|c| c.shape.kind == CellKind::Insert && new.slot(c).is_some_and(Option::is_some)).unwrap();
    let i = slot_moves(&env)[0];
    env.plan.moves[i].to = fresh as u16;
    assert_rejected(rig, env, RejectReason::DestinationNotVacant);
}

#[test]
fn rejects_a_destination_of_another_kind_channel_count_or_layout() {
    for f in 0..4 {
        let (rig, _, mut env) = rig_with_pending();
        let i = slot_moves(&env)[0];
        let to = usize::from(env.plan.moves[i].to);
        let cells = &mut env.plan.tables.as_mut().unwrap().cells;
        match f {
            0 => env.plan.moves[i].kind = CellKind::Instrument,
            1 => cells[to].shape.kind = CellKind::Instrument,
            2 => cells[to].shape.ch = 1,
            _ => cells[to].shape.layout ^= 0x40,
        }
        assert_rejected(rig, env, RejectReason::ShapeMismatch);
    }
    // A strip shell of another width.
    let (rig, _, mut env) = rig_with_pending();
    let i = env.plan.moves.iter().position(|m| m.kind == CellKind::Strip).unwrap();
    let to = usize::from(env.plan.moves[i].to);
    env.plan.tables.as_mut().unwrap().cells[to].shape.layout ^= 0x40;
    assert_rejected(rig, env, RejectReason::ShapeMismatch);
}

#[test]
fn rejects_invalid_attachments() {
    for f in 0..3 {
        let (rig, _, mut env) = rig_with_pending();
        let to = env.plan.moves[slot_moves(&env)[0]].to;
        let strip_cell = env.plan.moves.iter().find(|m| m.kind == CellKind::Strip).unwrap().to;
        let a = match f {
            0 => Attach::Applied { cell: to, from_move: None, storage: vec![f32::NAN; 8] },
            1 => Attach::Applied { cell: strip_cell, from_move: None, storage: vec![f32::NAN; 80] },
            _ => Attach::Sample { cell: to, from_move: None, audio: None, wrapper: None },
        };
        env.plan.attach = vec![a].into_boxed_slice();
        assert_rejected(rig, env, RejectReason::AttachInvalid);
    }
}

#[test]
fn rejects_claim_scratch_of_the_wrong_size() {
    let (rig, _, mut env) = rig_with_pending();
    env.plan.src_claimed = vec![false; 1].into_boxed_slice();
    assert_rejected(rig, env, RejectReason::MoveOutOfRange);
}

#[test]
fn a_rejected_plans_owners_drop_on_the_control_thread() {
    let (mut rig, _, mut env) = rig_with_pending();
    env.plan.moves[0].expect = SlotGen(9_999);
    let control = std::thread::current().id();
    let drops = rig.drops();
    let audio = std::thread::scope(|sc| {
        sc.spawn(|| {
            assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
            std::thread::current().id()
        })
        .join()
        .unwrap()
    });
    assert_eq!(rig.drops(), drops, "nothing dropped on the audio thread");
    rig.comp.reclaim(env);
    assert_eq!(rig.comp.pooled(), 1, "the fresh instance waits in the pool");
    // The next compile does not need it: it drops at the end of that compile, here.
    let (s, _, _) = audio_session();
    let env = rig.compile(&s);
    assert_eq!(rig.comp.pooled(), 0);
    let log = rig.log.drops.lock().unwrap().clone();
    assert!(log.len() > drops);
    assert!(log.iter().all(|t| *t == control && *t != audio));
    drop(env);
}

#[test]
fn a_refused_plans_fresh_instance_is_reused_by_the_next_compile() {
    let (mut rig, s2, mut env) = rig_with_pending();
    env.plan.moves[0].expect = SlotGen(9_999);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    let made = rig.log.made.load(Ordering::Relaxed);
    assert_eq!(rig.publish(&s2), Adoption::Adopted);
    assert_eq!(rig.log.made.load(Ordering::Relaxed), made, "no new instance was made");
    assert_eq!(rig.comp.pooled(), 0);
}

#[test]
fn identity_and_generation_rules() {
    assert_eq!(PlanGeneration::from_raw(u64::MAX).checked_next(), None);
    assert_eq!(PlanGeneration::EMPTY.checked_next(), Some(PlanGeneration::from_raw(1)));
    let (s, _, _) = audio_session();
    let mut comp = PlanCompiler::new(target(), PlanLimits::default());
    let log = SharedLog::default();
    let mut make = factory(&log);
    let g = PlanGeneration::from_raw;
    let s = Arc::new(s);
    // Stale base.
    let e = comp.compile(Arc::clone(&s), g(3), g(4), &mut make).unwrap_err();
    assert_eq!(e, CompileError::BaseNotActive { base: g(3), active: PlanGeneration::EMPTY });
    // Not increasing.
    let e = comp.compile(Arc::clone(&s), PlanGeneration::EMPTY, PlanGeneration::EMPTY, &mut make).unwrap_err();
    assert!(matches!(e, CompileError::GenerationNotIncreasing { .. }));
    // Target rate.
    let mut other = (*s).clone();
    other.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
    let e = comp.compile(Arc::new(other), PlanGeneration::EMPTY, g(1), &mut make).unwrap_err();
    assert!(matches!(e, CompileError::TargetMismatch { .. }));
    // In flight (N = 1), then exhausted after the last generation.
    let env = comp.compile(Arc::clone(&s), PlanGeneration::EMPTY, g(u64::MAX), &mut make).unwrap();
    assert_eq!(comp.compile(Arc::clone(&s), PlanGeneration::EMPTY, g(5), &mut make).unwrap_err(), CompileError::InFlight { pending: g(u64::MAX) });
    let mut eng = MixEngine::realtime(target());
    let mut env = env;
    assert_eq!(eng.adopt_plan(&mut env), Adoption::Adopted);
    comp.reclaim(env);
    assert_eq!(comp.active(), g(u64::MAX));
    assert_eq!(comp.compile(Arc::clone(&s), g(u64::MAX), g(u64::MAX), &mut make).unwrap_err(), CompileError::GenerationExhausted);
}

#[test]
fn limits_are_checked_before_anything_is_allocated() {
    let (mut s, a, _) = audio_session();
    for i in 0..3 {
        s.track_mut(a).unwrap().mixer.inserts[i] = Some(builtin("gain"));
    }
    let s = Arc::new(s);
    let log = SharedLog::default();
    let cases: [(PlanLimits, &str); 5] = [
        (PlanLimits { tracks: 1, ..PlanLimits::default() }, "tracks"),
        (PlanLimits { slots_per_strip: 4, ..PlanLimits::default() }, "slots_per_strip"),
        (PlanLimits { prepared_bytes: 1 << 16, ..PlanLimits::default() }, "prepared_bytes"),
        (PlanLimits { buses: 0, ..PlanLimits::default() }, "buses"),
        (PlanLimits { clip_fx_per_strip: 0, ..PlanLimits::default() }, "clip_fx_per_strip"),
    ];
    for (limits, what) in cases {
        let mut ses = (*s).clone();
        if what == "buses" {
            ses.busses.push(soundcraft_model::Bus { id: BusId(77), name: "b".into(), format: ChannelFormat::Stereo });
        }
        if what == "clip_fx_per_strip" {
            let clip = ses.track(a).unwrap().clips()[0].id.0;
            ses.edit.values.insert(format!("clip_fx.{clip}.eq.gain1"), 1.0);
        }
        let ses = Arc::new(ses);
        let mut comp = PlanCompiler::new(target(), limits);
        let mut make = factory(&log);
        let (r, stats) =
            soundcraft_rt::testing::measure(|| comp.compile(Arc::clone(&ses), PlanGeneration::EMPTY, PlanGeneration::from_raw(1), &mut make));
        match r {
            Err(CompileError::ResourceLimit { what: w, need, limit }) => {
                assert_eq!(w, what);
                assert!(need > limit);
            }
            other => panic!("{what}: {other:?}"),
        }
        assert_eq!(stats.allocs, 0, "{what}: nothing allocated before the check");
        assert_eq!(log.made.load(Ordering::Relaxed), 0, "{what}: no instance made");
    }
    // Multiplication overflow is a limit, not a wrap.
    assert_eq!(
        compile::strip_bytes(16, 16_384, usize::MAX / 8, 4, 0),
        Err(CompileError::ResourceLimit { what: "prepared_bytes", need: usize::MAX, limit: usize::MAX })
    );
}

#[test]
fn a_volume_only_edit_publishes_no_tables() {
    let (s, a, _) = audio_session();
    let mut rig = Rig::new();
    rig.publish(&s);
    let mut s2 = s.clone();
    s2.track_mut(a).unwrap().mixer.volume_db = -6.0;
    let env = rig.compile(&s2);
    assert!(!env.plan().is_structural());
    rig.comp.reclaim(env);
}

/// Burst into a reverb and a compressor on track `a`; renders 40 blocks, applying `edit` (as a
/// published plan) after 6.
fn tail_with_edit(edit: Option<&dyn Fn(&mut Session)>) -> Vec<f32> {
    let (mut s, a, _) = audio_session();
    let ins = &mut s.track_mut(a).unwrap().mixer.inserts;
    ins[0] = Some(builtin("compressor"));
    ins[1] = Some(builtin("room_reverb"));
    let mut rig = Rig::new();
    rig.publish(&s);
    let mut out = rig.render(6);
    if let Some(edit) = edit {
        edit(&mut s);
        assert_eq!(rig.publish(&s), Adoption::Adopted);
    }
    out.extend(rig.render(34));
    out
}

#[test]
fn a_reused_slot_keeps_its_state_across_plans() {
    let reference = tail_with_edit(None);
    assert!(reference[20 * BLOCK..].iter().any(|x| x.abs() > 1e-4), "the reverb tail rings past the burst");
    // A volume-only plan on the other track, and a structural plan that adds an insert there.
    let volume = tail_with_edit(Some(&|s: &mut Session| s.tracks[1].mixer.volume_db = -3.0));
    let insert = tail_with_edit(Some(&|s: &mut Session| s.tracks[1].mixer.inserts[0] = Some(builtin("plate_reverb"))));
    assert_eq!(volume, reference, "volume-only plan");
    assert_eq!(insert, reference, "unrelated insert-add plan");
}

/// Delay compensation holds track `a`'s audio in its strip's delay line (a lookahead insert on
/// track `b`); a structural plan arrives mid-burst. Renders 30 blocks.
fn delayed_with_edit(edit: bool) -> Vec<f32> {
    let (mut s, _, b) = audio_session();
    s.edit.delay_compensation = true;
    let mut lookahead = builtin("compressor");
    lookahead.params.insert("lookahead".into(), 10.0);
    s.track_mut(b).unwrap().mixer.inserts[0] = Some(lookahead);
    let mut rig = Rig::new();
    rig.publish(&s);
    let mut out = rig.render(3);
    assert!(rig.eng.latency > 400, "the lookahead delays the mix: {}", rig.eng.latency);
    if edit {
        s.track_mut(b).unwrap().mixer.inserts[1] = Some(builtin("plate_reverb"));
        assert_eq!(rig.publish(&s), Adoption::Adopted);
    }
    out.extend(rig.render(27));
    out
}

#[test]
fn strip_state_moves_across_a_structural_plan() {
    let reference = delayed_with_edit(false);
    assert_eq!(delayed_with_edit(true), reference, "the delay lines (and the rest of the strip state) carry over");
}

#[test]
fn a_mid_commit_unwind_loses_no_owner_and_stops_the_engine() {
    for step in 1..=4 {
        let (mut rig, _, mut env) = rig_with_pending();
        let session: Weak<Session> = Arc::downgrade(&env.plan.session);
        let total = rig.log.made.load(Ordering::Relaxed);
        FAULT_AT.set(step);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.eng.adopt_plan(&mut env)));
        FAULT_AT.set(0);
        assert!(r.is_err(), "step {step} unwound");
        assert_eq!(rig.drops(), 0, "step {step}: no owner dropped by the unwind");
        assert!(rig.eng.faulted());
        assert_eq!(env.state(), EnvelopeState::Pending, "the port keeps the envelope");
        assert_eq!(rig.render(1), vec![0.0; BLOCK], "the fault path renders silence");
        let mut again = rig.compile_after_fault_is_refused();
        assert_eq!(rig.eng.adopt_plan(&mut again), Adoption::Rejected(RejectReason::Faulted));
        drop(again);
        // Teardown after quiescence: both halves, on this (control) thread.
        let retired = std::mem::replace(&mut rig.eng, MixEngine::realtime(target())).into_retired();
        drop(retired);
        drop(env);
        assert_eq!(rig.drops(), total, "step {step}: every instance dropped exactly once, at teardown");
        assert!(session.upgrade().is_none(), "step {step}: the Session too");
    }
}

impl Rig {
    /// A plan forged against the active generation after a fault (the compiler still has one in
    /// flight, so it is built by hand from a fresh compiler).
    fn compile_after_fault_is_refused(&mut self) -> PlanEnvelope {
        let (s, _, _) = audio_session();
        let mut comp = PlanCompiler::new(target(), PlanLimits::default());
        let mut make = factory(&self.log);
        let mut env = comp.compile(Arc::new(s), PlanGeneration::EMPTY, PlanGeneration::from_raw(99), &mut make).unwrap();
        env.plan.id.base = self.eng.active().generation;
        env
    }
}

#[test]
fn a_sampler_sample_swap_keeps_both_owners_until_reclaim() {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let audio = |v: f32| Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![vec![v; 4_800]] }));
    s.pool.insert(SourceId(1), audio(0.5));
    s.pool.insert(SourceId(2), audio(0.25));
    let mut ins = Insert::new("sampler");
    ins.sample = Some(SourceId(1));
    s.track_mut(t).unwrap().instrument = Some(ins);
    let first: Weak<SourceAudio> = Arc::downgrade(s.pool.get(SourceId(1)).unwrap());
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    rig.render(2);
    // Only the sample changes, and source 1 leaves the pool.
    s.track_mut(t).unwrap().instrument.as_mut().unwrap().sample = Some(SourceId(2));
    s.pool = soundcraft_model::SourcePool::default();
    s.pool.insert(SourceId(2), audio(0.25));
    let mut env = rig.compile(&s);
    assert!(env.plan.attach.iter().any(|a| matches!(a, Attach::Sample { .. })), "the sampler is reused with a new sample");
    let made = rig.log.made.load(Ordering::Relaxed);
    assert_eq!(adopt_measured(&mut rig.eng, &mut env), Adoption::Adopted);
    assert!(first.upgrade().is_some(), "the old audio is still owned, by the envelope");
    rig.comp.reclaim(env);
    assert!(first.upgrade().is_none(), "dropped at reclaim, on the control thread");
    assert_eq!(rig.log.made.load(Ordering::Relaxed), made, "the instance was kept");
    assert_eq!(rig.drops(), 0);
}

#[test]
fn delay_beyond_capacity_is_clamped_and_reported() {
    let (mut s, a, b) = audio_session();
    s.edit.delay_compensation = true;
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.latent"));
    let mut rig = Rig::new();
    rig.publish(&s);
    rig.render(2);
    assert_eq!(rig.eng.status(), PlanStatus::default());
    rig.log.latency.store(20_000, Ordering::Relaxed);
    rig.render(2);
    let st = rig.eng.status();
    assert!(st.capacity_exceeded > 0);
    assert_eq!(st.required_latency, 20_000);
    assert_eq!(st.affected.map(|(t, _)| t), Some(b), "the strip that needed the longer delay");
    // The next plan prepares that much.
    rig.comp.observe(&st);
    s.track_mut(a).unwrap().mixer.volume_db = -1.0;
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    rig.render(2);
    assert_eq!(rig.eng.status().capacity_exceeded, 0);
}

#[test]
fn envelopes_are_send() {
    fn send<T: Send>() {}
    send::<PlanEnvelope>();
    send::<MixEngine>();
    send::<Retired>();
}

#[test]
fn applied_storage_grows_through_an_attachment() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    let mut rig = Rig::new();
    rig.publish(&s);
    rig.render(1);
    let ins = s.track_mut(a).unwrap().mixer.inserts[0].as_mut().unwrap();
    for i in 0..70 {
        ins.params.insert(format!("extra{i:02}"), 0.5);
    }
    let n = ins.params.len();
    let env = rig.compile(&s);
    assert!(env.plan.attach.iter().any(|a| matches!(a, Attach::Applied { storage, .. } if storage.len() == n)));
    let mut env = env;
    assert_eq!(adopt_measured(&mut rig.eng, &mut env), Adoption::Adopted);
    rig.comp.reclaim(env);
    rig.render(2);
    assert_eq!(rig.eng.status().applied_mismatch, 0);
}

/// A move list the compiler made is accepted only for the active plan it was made against.
#[test]
fn moves_name_cells_of_the_active_plan() {
    let (rig, _, env) = rig_with_pending();
    let old = &rig.eng.plan.as_deref().unwrap().tables;
    for CellMove { kind, from, .. } in env.plan.moves.iter() {
        assert_eq!(old.cells[usize::from(*from)].shape.kind, *kind);
    }
    assert_eq!(env.plan.src_claimed.len(), old.cells.len());
}

// Review repros (ADR 0009 refutation review of feat/prepared-plan).

/// Track `a` plays through PDC delay lines; plan 2 lowers the session's max latency, plan 3 is an
/// unrelated insert add. Returns the left channel.
fn shrink_with_edit(edit: bool) -> Vec<f32> {
    let (mut s, _, b) = audio_session();
    s.edit.delay_compensation = true;
    let mut rig = Rig::new();
    rig.log.latency.store(10_000, Ordering::Relaxed);
    s.track_mut(b).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.latent"));
    rig.publish(&s);
    let mut out = rig.render(2);
    rig.log.latency.store(480, Ordering::Relaxed);
    s.track_mut(b).unwrap().mixer.inserts[0] = None;
    s.track_mut(b).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.latent"));
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    out.extend(rig.render(4));
    if edit {
        s.track_mut(b).unwrap().mixer.inserts[2] = Some(builtin("gain"));
        assert_eq!(rig.publish(&s), Adoption::Adopted);
    }
    out.extend(rig.render(8));
    out
}

#[test]
fn a_delay_tail_survives_a_capacity_shrink() {
    let r = shrink_with_edit(false);
    let e = shrink_with_edit(true);
    let first = r.iter().zip(&e).position(|(x, y)| x.to_bits() != y.to_bits());
    assert_eq!(first, None, "an unrelated insert add changed the output (cap shrink drops the delay lines)");
}

#[test]
fn one_large_status_does_not_wedge_later_compiles() {
    let (s, _, _) = audio_session();
    let mut rig = Rig::new();
    rig.comp = PlanCompiler::new(target(), PlanLimits { prepared_bytes: 32 << 20, ..PlanLimits::default() });
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    rig.comp.observe(&PlanStatus { required_latency: 1 << 20, capacity_exceeded: 1, ..PlanStatus::default() });
    let mut make = factory(&rig.log);
    let mut s2 = s.clone();
    s2.tracks[0].mixer.inserts[0] = Some(builtin("gain"));
    let r = rig.comp.compile(Arc::new(s2), rig.comp.active(), PlanGeneration::from_raw(50), &mut make);
    assert!(r.is_ok(), "one transient status wedges every structural compile: {:?}", r.err());
}

#[test]
fn a_panicking_drop_in_the_pool_does_not_wedge_the_compiler() {
    let (mut rig, _, mut env) = rig_with_pending();
    env.plan.moves[0].expect = SlotGen(9_999);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    assert_eq!(rig.comp.pooled(), 1);
    rig.log.panic_on_drop.store(true, Ordering::Relaxed);
    let (s, _, _) = audio_session();
    let mut make = factory(&rig.log);
    let active = rig.comp.active();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.comp.compile(Arc::new(s.clone()), active, PlanGeneration::from_raw(40), &mut make).map(|_| ())
    }));
    assert!(r.is_err(), "the pooled instance's drop panicked inside compile");
    let again = rig.comp.compile(Arc::new(s), active, PlanGeneration::from_raw(41), &mut make);
    assert!(again.is_ok(), "compiler wedged after a panicking drop: {:?}", again.err());
}

#[test]
fn reclaiming_a_torn_envelope_pools_nothing_under_the_wrong_spec() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("room_reverb"));
    let mut rig = Rig::new();
    rig.publish(&s);
    rig.render(2);
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("chorus"));
    let mut env = rig.compile(&s);
    FAULT_AT.set(4);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.eng.adopt_plan(&mut env)));
    FAULT_AT.set(0);
    assert!(r.is_err());
    assert_eq!(env.state(), EnvelopeState::Pending);
    rig.comp.reclaim(env);
    // Nothing from the torn envelope waits in the pool to be installed under the wrong spec, and
    // the faulted compiler makes no further plan at all.
    assert_eq!(rig.comp.pooled(), 0);
    let _ = a;
    let mut make = factory(&rig.log);
    let r = rig.comp.compile(Arc::new(s.clone()), rig.comp.active(), PlanGeneration::from_raw(30), &mut make);
    assert_eq!(r.unwrap_err(), CompileError::Faulted);
}

#[test]
fn a_plugin_panic_in_render_drops_nothing_and_the_engine_continues() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.fx"));
    let mut rig = Rig::new();
    rig.publish(&s);
    rig.render(1);
    rig.log.panic_on_process.store(true, Ordering::Relaxed);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.render(1)));
    assert!(r.is_err());
    rig.log.panic_on_process.store(false, Ordering::Relaxed);
    assert_eq!(rig.drops(), 0);
    rig.render(2);
    s.track_mut(a).unwrap().mixer.inserts[1] = Some(builtin("gain"));
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    assert_eq!(rig.drops(), 0);
}

#[test]
fn empty_and_shrinking_sessions_and_the_track_limit() {
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&Session::default()), Adoption::Adopted);
    assert_eq!(rig.render(2), vec![0.0; 2 * BLOCK]);
    let (s, _, _) = audio_session();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    let mut one = s.clone();
    one.tracks.truncate(1);
    assert_eq!(rig.publish(&one), Adoption::Adopted);
    rig.render(2);
    for n in [2u16, 1] {
        let mut comp = PlanCompiler::new(target(), PlanLimits { tracks: n, ..PlanLimits::default() });
        let mut make = factory(&rig.log);
        let r = comp.compile(Arc::new(s.clone()), PlanGeneration::EMPTY, PlanGeneration::from_raw(1), &mut make);
        if n == 2 { assert!(r.is_ok()) } else { assert!(matches!(r, Err(CompileError::ResourceLimit { what: "tracks", .. }))) }
    }
}

#[test]
fn a_refused_replacement_dropped_from_the_pool_is_released_at_an_occupied_key() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.a"));
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    let _ = rig.comp.take_released();
    let mut s2 = s.clone();
    s2.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.b"));
    let mut env = rig.compile(&s2);
    let i = env.plan.moves.iter().position(|m| m.kind == CellKind::Strip).unwrap();
    env.plan.moves[i].expect = SlotGen(9_999);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    assert_eq!(rig.comp.pooled(), 1, "B waits in the pool");
    let drops = rig.drops();
    let env = rig.compile(&s);
    assert_eq!(rig.drops(), drops + 1, "B was destroyed by drop_pool");
    let released = rig.comp.take_released();
    drop(env);
    assert!(!released.is_empty(), "B's destruction is reported: released = {released:?}");
}

// Coverage for the review's mutants.

fn synth_track(s: &mut Session) -> TrackId {
    use soundcraft_midi::{Note, Sequence};
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let mut seq = Sequence::default();
    for (i, p) in [60u8, 64, 67].into_iter().enumerate() {
        seq.notes.push(Note { pitch: p, velocity: 100, release_velocity: 64, channel: 0, start: i as i64 * 10, length: 40_000 });
    }
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 96_000, seq));
    tr.instrument = Some(builtin("subtractive_synth"));
    t
}

#[test]
fn a_sample_attachment_on_an_instrument_that_is_not_the_sampler_is_rejected() {
    let mut s = Session::default();
    let t = synth_track(&mut s);
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    let mut env = rig.compile(&s);
    let (i, m) = env.plan.moves.iter().enumerate().find(|(_, m)| m.kind == CellKind::Instrument).unwrap();
    let (to, from_move) = (m.to, Some(i as u16));
    env.plan.attach = vec![Attach::Sample { cell: to, from_move, audio: None, wrapper: None }].into_boxed_slice();
    assert_rejected(rig, env, RejectReason::AttachInvalid);
}

#[test]
fn held_notes_move_with_the_instrument_and_note_chasing_with_the_strip() {
    let mut s = Session::default();
    let t = synth_track(&mut s);
    let mut rig = Rig::new();
    rig.publish(&s);
    rig.render(4);
    let strip = |rig: &Rig| {
        let p = rig.eng.plan.as_deref().unwrap();
        let st = &p.tables.strips[p.tables.strip_of(t).unwrap()];
        let keys: Vec<u8> = st.instrument.as_ref().unwrap().events.as_deref().unwrap().sounding().map(|(_, key, _)| key).collect();
        (keys, st.chase)
    };
    let before = strip(&rig);
    assert_eq!(before, (vec![60, 64, 67], false), "three notes sound; the controllers were chased");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    assert_eq!(strip(&rig), before, "a structural plan keeps the held notes and the chase state");
}

#[test]
fn a_format_change_of_the_same_width_makes_a_fresh_instance() {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Quad, None);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    let made = rig.log.made.load(Ordering::Relaxed);
    s.track_mut(t).unwrap().format = ChannelFormat::Ambisonics(1);
    assert_eq!(rig.publish(&s), Adoption::Adopted, "a new layout is a new cell, not a move");
    assert_eq!(rig.log.made.load(Ordering::Relaxed), made + 1);
}

#[test]
fn reset_zeroes_the_plan_meters() {
    let (s, _, _) = audio_session();
    let mut rig = Rig::new();
    rig.publish(&s);
    rig.render(2);
    assert!(rig.eng.meters().any(|(_, m)| m.peaks.iter().any(|x| *x > 0.0)));
    rig.eng.reset();
    assert!(rig.eng.meters().all(|(_, m)| m.peaks.iter().all(|x| *x == 0.0) && m.peak == [0.0; 2]));
    assert!(rig.eng.active_main_meter().peaks.iter().all(|x| *x == 0.0));
}

// Instance identity (F5): the factory gets each instance's generation, and every instance the
// compiler destroys comes back once from `take_released` with that generation.

fn compile_recording(rig: &mut Rig, s: &Session, made: &Arc<Mutex<Vec<SlotBinding>>>) -> PlanEnvelope {
    rig.issued += 1;
    let mut inner = factory(&rig.log);
    let made = Arc::clone(made);
    let mut make = move |spec: &InstanceSpec, generation: SlotGen| {
        made.lock().unwrap().push((spec.track, spec.slot, generation));
        inner(spec, generation)
    };
    rig.comp.compile(Arc::new(s.clone()), rig.comp.active(), PlanGeneration::from_raw(rig.issued), &mut make).unwrap()
}

fn publish_recording(rig: &mut Rig, s: &Session, made: &Arc<Mutex<Vec<SlotBinding>>>) -> Adoption {
    let mut env = compile_recording(rig, s, made);
    let a = rig.eng.adopt_plan(&mut env);
    rig.comp.reclaim(env);
    a
}

fn sorted(mut v: Vec<SlotBinding>) -> Vec<SlotBinding> {
    v.sort_unstable();
    v
}

#[test]
fn the_factory_gets_the_generation_each_instance_carries() {
    let (mut s, a, b) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("room_reverb"));
    s.track_mut(b).unwrap().mixer.inserts[3] = Some(Insert::new("clap:test.fx"));
    let (mut rig, made) = (Rig::new(), Arc::default());
    let mut env = compile_recording(&mut rig, &s, &made);
    assert_eq!(rig.comp.pending_bindings(), Some(sorted(made.lock().unwrap().clone())), "bound before publication");
    assert_eq!(rig.eng.adopt_plan(&mut env), Adoption::Adopted);
    rig.comp.reclaim(env);
    assert_eq!(rig.comp.active_bindings(), sorted(made.lock().unwrap().clone()));
    assert!(rig.comp.take_released().is_empty());
}

#[test]
fn a_displaced_instance_is_released_with_its_generation() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.a"));
    let (mut rig, made) = (Rig::new(), Arc::default());
    publish_recording(&mut rig, &s, &made);
    let old = made.lock().unwrap()[0];
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.b"));
    assert_eq!(publish_recording(&mut rig, &s, &made), Adoption::Adopted);
    let new = made.lock().unwrap()[1];
    assert_eq!((new.0, new.1), (old.0, old.1), "the same slot");
    assert_eq!(rig.comp.take_released(), vec![old], "the displaced instance, by its generation");
    assert_eq!(rig.comp.active_bindings(), vec![new], "the new one lives at that slot");
}

#[test]
fn a_rejected_instance_is_released_when_the_pool_drops_it_and_not_when_it_is_reused() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.a"));
    let (mut rig, made) = (Rig::new(), Arc::default());
    publish_recording(&mut rig, &s, &made);
    let mut s2 = s.clone();
    s2.track_mut(a).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.b"));
    let mut env = compile_recording(&mut rig, &s2, &made);
    let b = *made.lock().unwrap().last().unwrap();
    env.plan.moves[0].expect = SlotGen(9_999);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    assert!(rig.comp.take_released().is_empty(), "rejected: B waits in the pool, alive");
    // Reused: same binding, nothing released.
    let mut env = compile_recording(&mut rig, &s2, &made);
    assert!(rig.comp.pending_bindings().unwrap().contains(&b), "the pooled instance keeps its generation");
    assert!(rig.comp.take_released().is_empty());
    env.plan.moves[0].expect = SlotGen(9_999);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    // Not wanted by the next compile: dropped from the pool, released.
    let env = compile_recording(&mut rig, &s, &made);
    assert_eq!(rig.comp.take_released(), vec![b]);
    drop(env);
}

#[test]
fn a_pool_drop_at_an_occupied_slot_is_released_by_generation() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.a"));
    let (mut rig, made) = (Rig::new(), Arc::default());
    publish_recording(&mut rig, &s, &made);
    let a_bound = made.lock().unwrap()[0];
    let mut s2 = s.clone();
    s2.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.b"));
    let mut env = compile_recording(&mut rig, &s2, &made);
    let b_bound = made.lock().unwrap()[1];
    env.plan.moves[0].expect = SlotGen(9_999);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    let env = compile_recording(&mut rig, &s, &made);
    assert_eq!(rig.comp.take_released(), vec![b_bound], "B is gone although A lives at the same slot");
    assert_eq!(rig.comp.pending_bindings(), Some(vec![a_bound]));
    drop(env);
}

#[test]
fn teardown_reports_every_instance_by_generation() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.a"));
    let (mut rig, made) = (Rig::new(), Arc::default());
    publish_recording(&mut rig, &s, &made);
    s.track_mut(a).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.b"));
    // A plan that never reaches the audio thread, then teardown.
    let env = compile_recording(&mut rig, &s, &made);
    rig.comp.reclaim(env);
    rig.comp.clear_pool();
    let b = made.lock().unwrap()[1];
    assert_eq!(rig.comp.take_released(), vec![b], "the unsent plan's instance");
    let active = rig.comp.active_bindings();
    let retired = std::mem::replace(&mut rig.eng, MixEngine::realtime(target())).into_retired();
    assert_eq!(sorted(retired.instances()), active, "the engine's instances, for playback to release at teardown");
    drop(retired);
    assert_eq!(rig.drops(), 2);
}

#[test]
fn slot_generations_never_wrap() {
    let (mut s, a, _) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    let mut rig = Rig::new();
    rig.comp.set_slot_generation_for_tests(u64::MAX - 1);
    let mut make = factory(&rig.log);
    let r = rig.comp.compile(Arc::new(s), PlanGeneration::EMPTY, PlanGeneration::from_raw(1), &mut make);
    assert_eq!(r.unwrap_err(), CompileError::GenerationExhausted);
    assert_eq!(rig.comp.pending(), None, "nothing in flight");
    assert_eq!(rig.log.made.load(Ordering::Relaxed), rig.drops(), "whatever was made was dropped");
    assert_eq!(rig.comp.take_released().len(), rig.drops(), "and reported");
}

#[test]
fn a_hint_sizes_every_strip_below_it_once_and_is_dropped_when_it_does_not_fit() {
    let (mut s, a, b) = audio_session();
    s.edit.delay_compensation = true;
    let mut rig = Rig::new();
    rig.publish(&s);
    let cap = |rig: &Rig, t: TrackId| {
        let p = rig.eng.plan.as_deref().unwrap();
        Some(p.tables.strips[p.tables.strip_of(t).unwrap()].tap.len)
    };
    // Status without a clamp changes nothing.
    rig.comp.observe(&PlanStatus { required_latency: 40_000, ..PlanStatus::default() });
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    assert_eq!((cap(&rig, a), cap(&rig, b)), (Some(MIN_DELAY_CAPACITY), Some(MIN_DELAY_CAPACITY)));
    rig.comp.observe(&PlanStatus { required_latency: 40_000, capacity_exceeded: 1, affected: Some((b, SlotGen(1))), ..PlanStatus::default() });
    s.track_mut(a).unwrap().mixer.inserts[2] = Some(builtin("gain"));
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    assert_eq!(cap(&rig, b), Some(65_536), "the strip that asked");
    assert_eq!(cap(&rig, a), Some(65_536), "and every other strip below it, in the same plan");
    // Satisfied hints are gone: the next structural plan keeps those lines and asks for no more.
    s.track_mut(a).unwrap().mixer.inserts[3] = Some(builtin("gain"));
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    assert_eq!((cap(&rig, a), cap(&rig, b)), (Some(65_536), Some(65_536)));
    // A hint beyond the byte limit is dropped instead of failing every later compile.
    rig.comp = PlanCompiler::new(target(), PlanLimits { prepared_bytes: 8 << 20, ..PlanLimits::default() });
    rig.eng = MixEngine::realtime(target());
    rig.issued = 0;
    rig.publish(&s);
    rig.comp.observe(&PlanStatus { required_latency: 1 << 20, capacity_exceeded: 1, affected: Some((b, SlotGen(1))), ..PlanStatus::default() });
    s.track_mut(a).unwrap().mixer.inserts[1] = Some(builtin("gain"));
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    assert_eq!(cap(&rig, b), Some(MIN_DELAY_CAPACITY));
}

// Second review repros.
#[test]
fn a_factory_panic_mid_compile_reports_the_instances_already_made() {
    let (mut s, a, _) = audio_session();
    let mut rig = Rig::new();
    rig.publish(&s);
    let _ = rig.comp.take_released();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.a"));
    s.track_mut(a).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.boom"));
    let handed = Arc::new(Mutex::new(Vec::new()));
    let mut inner = factory(&rig.log);
    let h2 = Arc::clone(&handed);
    let mut make = move |spec: &InstanceSpec, g: SlotGen| {
        if spec.id == "clap:test.boom" {
            panic!("factory unwinds");
        }
        let n = inner(spec, g);
        if n.is_some() {
            h2.lock().unwrap().push(g);
        }
        n
    };
    let active = rig.comp.active();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.comp.compile(Arc::new(s.clone()), active, PlanGeneration::from_raw(50), &mut make).map(|_| ())
    }));
    assert!(r.is_err());
    let made = handed.lock().unwrap().clone();
    assert_eq!(made.len(), 1);
    assert_eq!(rig.drops(), 1, "the made instance was destroyed by the unwind");
    let rel: Vec<SlotGen> = rig.comp.take_released().iter().map(|b| b.2).collect();
    assert!(rel.contains(&made[0]), "destroyed generation {:?} not reported; released {rel:?}", made[0]);
}

#[test]
fn runtime_latency_growth_is_absorbed_by_one_plan_for_every_strip() {
    let (mut s, a, b) = audio_session();
    s.edit.delay_compensation = true;
    let c = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let d = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let _ = (a, c, d);
    s.track_mut(b).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.latent"));
    let mut rig = Rig::new();
    rig.publish(&s);
    rig.render(2);
    rig.log.latency.store(20_000, Ordering::Relaxed);
    let mut rounds = 0;
    for i in 0..6 {
        rig.render(2);
        let st = rig.eng.status();
        if st.capacity_exceeded == 0 {
            break;
        }
        rounds += 1;
        rig.comp.observe(&st);
        s.tracks[0].mixer.volume_db = -(i as f32) - 1.0;
        assert_eq!(rig.publish(&s), Adoption::Adopted);
    }
    assert!(rounds <= 1, "needed {rounds} plans before no strip clamped");
}

#[test]
fn reclaim_after_clear_pool_releases_instead_of_pooling() {
    let (mut rig, s2, env) = rig_with_pending();
    let _ = s2;
    rig.comp.clear_pool();
    rig.comp.reclaim(env); // never serviced (teardown)
    assert_eq!(rig.comp.pooled(), 0, "teardown: reclaim after clear_pool pools instances that die unreported with the compiler");
}

#[test]
fn every_instance_is_released_exactly_once_across_pool_reuse_and_teardown() {
    use std::collections::BTreeSet;
    let (mut s1, a, _) = audio_session();
    s1.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.a"));
    let mut s2 = s1.clone();
    s2.track_mut(a).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.b"));
    s2.track_mut(a).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.c"));
    let mut rig = Rig::new();
    let mut released: Vec<SlotGen> = Vec::new();
    let mut seen: BTreeSet<SlotGen> = BTreeSet::new();
    let reject = |env: &mut PlanEnvelope| {
        if let Some(i) = env.plan.moves.iter().position(|m| m.kind == CellKind::Strip) {
            env.plan.moves[i].expect = SlotGen(9_999);
        }
    };
    let steps: [(u8, bool); 7] = [(1, true), (2, false), (2, false), (1, true), (2, false), (2, true), (1, true)];
    for (k, (which, ok)) in steps.into_iter().enumerate() {
        let s = if which == 1 { &s1 } else { &s2 };
        let mut env = rig.compile(s);
        for b in env.plan.tables.as_ref().map(|t| t.bindings()).unwrap_or_default() {
            seen.insert(b.2);
        }
        if !ok {
            reject(&mut env);
        }
        let r = rig.eng.adopt_plan(&mut env);
        assert_eq!(matches!(r, Adoption::Adopted), ok, "step {k}");
        rig.comp.reclaim(env);
        released.extend(rig.comp.take_released().iter().map(|b| b.2));
        rig.render(1);
        let live: Vec<SlotGen> = rig.comp.active_bindings().iter().map(|b| b.2).collect();
        let mut sorted = released.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), released.len(), "step {k}: double release {released:?}");
        assert!(live.iter().all(|g| !released.contains(g)), "step {k}: live {live:?} released {released:?}");
        let mut lv = live.clone();
        lv.sort();
        lv.dedup();
        assert_eq!(lv.len(), live.len(), "step {k}: aliased live generations");
    }
    rig.comp.clear_pool();
    released.extend(rig.comp.take_released().iter().map(|b| b.2));
    let retired = std::mem::replace(&mut rig.eng, MixEngine::realtime(target())).into_retired();
    released.extend(retired.instances().iter().map(|b| b.2));
    drop(retired);
    let all: BTreeSet<SlotGen> = released.iter().copied().collect();
    assert_eq!(all.len(), released.len(), "double release at teardown");
    assert_eq!(all, seen, "every instance ever made is released exactly once");
    assert_eq!(rig.drops(), seen.len());
}

/// A sampler track with a gain insert; the next plan removes the gain and changes the sample.
fn sampler_with_gain() -> (Session, Session) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let audio = |v: f32| Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![vec![v; 4_800]] }));
    s.pool.insert(SourceId(1), audio(0.5));
    s.pool.insert(SourceId(2), audio(0.25));
    let mut ins = builtin("sampler");
    ins.sample = Some(SourceId(1));
    s.track_mut(t).unwrap().instrument = Some(ins);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    let mut s2 = s.clone();
    s2.track_mut(t).unwrap().mixer.inserts[0] = None;
    s2.track_mut(t).unwrap().instrument.as_mut().unwrap().sample = Some(SourceId(2));
    (s, s2)
}

#[test]
fn a_torn_reclaim_never_leaves_a_released_instance_active() {
    let (s, s2) = sampler_with_gain();
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    let before = rig.comp.active_bindings();
    assert_eq!(before.len(), 2, "the sampler and the gain");
    let mut env = rig.compile(&s2);
    assert!(env.plan.attach.iter().any(|a| matches!(a, Attach::Sample { .. })));
    // The sampler refuses its new sample during the commit, after the tables swapped.
    rig.log.panic_on_sample.store(true, Ordering::Relaxed);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.eng.adopt_plan(&mut env)));
    rig.log.panic_on_sample.store(false, Ordering::Relaxed);
    assert!(r.is_err());
    assert!(env.torn(), "the envelope says torn");
    assert!(!rig.comp.faulted(), "nothing known yet: the envelope has not come back");
    rig.comp.reclaim(env);
    assert!(rig.comp.faulted(), "the compiler knows the active plan is torn, not merely pending");
    let released = rig.comp.take_released();
    let active = rig.comp.active_bindings();
    assert!(!released.is_empty(), "the gain, displaced into the envelope, is destroyed");
    assert!(active.iter().all(|b| !released.contains(b)), "released {released:?} must not be active {active:?}");
    // What is reported active is what the faulted engine still owns.
    let retired = std::mem::replace(&mut rig.eng, MixEngine::realtime(target())).into_retired();
    assert_eq!(sorted(retired.instances()), active);
    drop(retired);
    assert_eq!(rig.drops(), before.len(), "every instance dropped once, nothing leaked or doubled");
}

#[test]
fn a_plain_pending_return_is_not_torn() {
    let (s, s2) = sampler_with_gain();
    let mut rig = Rig::new();
    rig.publish(&s);
    let env = rig.compile(&s2);
    assert!(!env.torn());
    rig.comp.reclaim(env);
    assert!(!rig.comp.faulted());
    assert_eq!(rig.comp.active_bindings().len(), 2);
}

#[test]
fn after_a_torn_reclaim_at_any_commit_step_the_active_bindings_are_what_the_engine_owns() {
    for step in 1..=4 {
        let (mut rig, _, mut env) = rig_with_pending();
        let _ = rig.comp.take_released();
        FAULT_AT.set(step);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.eng.adopt_plan(&mut env)));
        FAULT_AT.set(0);
        assert!(r.is_err() && env.torn(), "step {step}");
        rig.comp.reclaim(env);
        assert!(rig.comp.faulted());
        let released = rig.comp.take_released();
        let active = rig.comp.active_bindings();
        assert!(active.iter().all(|b| !released.contains(b)), "step {step}: released {released:?} active {active:?}");
        let retired = std::mem::replace(&mut rig.eng, MixEngine::realtime(target())).into_retired();
        assert_eq!(sorted(retired.instances()), active, "step {step}");
        drop(retired);
        assert_eq!(rig.drops(), rig.log.made.load(Ordering::Relaxed), "step {step}: every instance dropped once");
    }
}

fn rich_torn(step: usize) -> (Vec<SlotBinding>, Vec<SlotBinding>, Vec<SlotBinding>, usize, usize, Rig, Session) {
    let (mut s, a, b) = audio_session();
    let inst = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let audio = |v: f32| Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![vec![v; 480]] }));
    s.pool.insert(SourceId(5), audio(0.5));
    s.pool.insert(SourceId(6), audio(0.25));
    let mut smp = Insert::new("sampler");
    smp.sample = Some(SourceId(5));
    s.track_mut(inst).unwrap().instrument = Some(smp);
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("room_reverb"));
    s.track_mut(a).unwrap().mixer.inserts[1] = Some(builtin("compressor"));
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    rig.render(2);
    // A refused plan leaves a fresh gain at (b, 0) in the pool.
    let mut s2 = s.clone();
    s2.track_mut(b).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.g"));
    let mut env = rig.compile(&s2);
    let i = env.plan.moves.iter().position(|m| m.kind == CellKind::Strip).unwrap();
    env.plan.moves[i].expect = SlotGen(9_999);
    assert!(matches!(rig.eng.adopt_plan(&mut env), Adoption::Rejected(_)));
    rig.comp.reclaim(env);
    assert_eq!(rig.comp.pooled(), 1);
    // The torn plan: reuse the pooled gain, replace the reverb (displaced), keep the compressor,
    // swap the sampler's sample.
    s2.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("chorus"));
    s2.track_mut(inst).unwrap().instrument.as_mut().unwrap().sample = Some(SourceId(6));
    let mut env = rig.compile(&s2);
    assert!(env.plan.attach.iter().any(|x| matches!(x, Attach::Sample { .. })));
    let mut released = rig.comp.take_released();
    FAULT_AT.set(step);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.eng.adopt_plan(&mut env)));
    FAULT_AT.set(0);
    assert!(r.is_err() && env.torn());
    rig.comp.reclaim(env);
    assert!(rig.comp.faulted());
    released.extend(rig.comp.take_released());
    let active = rig.comp.active_bindings();
    let retired = std::mem::replace(&mut rig.eng, MixEngine::realtime(target())).into_retired();
    let owned = sorted(retired.instances());
    drop(retired);
    rig.comp.clear_pool();
    released.extend(rig.comp.take_released());
    let made = rig.log.made.load(Ordering::Relaxed);
    let drops = rig.drops();
    (active, owned, released, made, drops, rig, s)
}

#[test]
fn a_torn_reclaim_that_displaced_reused_and_moved_instances_matches_the_engine_at_every_step() {
    for step in 1..=4 {
        let (active, owned, released, made, drops, _, _) = rich_torn(step);
        assert_eq!(active, owned, "step {step}");
        assert!(active.iter().all(|b| !released.contains(b)), "step {step}: overlap");
        let mut all: Vec<_> = released.iter().chain(owned.iter()).copied().collect();
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n, "step {step}: double release");
        assert_eq!(n, made, "step {step}: every instance accounted once");
        assert_eq!(drops, made, "step {step}");
        if step >= 3 {
            assert!(!released.is_empty(), "step {step}: the displaced reverb came back");
        }
    }
}

#[test]
fn compile_after_a_torn_reclaim_is_refused() {
    let (mut rig, _, mut env) = rig_with_pending();
    // replace a module so something is displaced
    FAULT_AT.set(4);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.eng.adopt_plan(&mut env)));
    FAULT_AT.set(0);
    rig.comp.reclaim(env);
    assert!(rig.comp.faulted());
    let released = rig.comp.take_released();
    let (s, _, _) = audio_session();
    let mut make = factory(&rig.log);
    let r = rig.comp.compile(Arc::new(s), rig.comp.active(), PlanGeneration::from_raw(77), &mut make);
    assert_eq!(r.unwrap_err(), CompileError::Faulted, "released: {released:?}");
    assert_eq!(rig.comp.pending_bindings(), None, "nothing in flight");
}

#[test]
fn a_session_only_compile_after_a_torn_reclaim_lists_no_released_binding_as_pending() {
    let (_, _, released, _, _, mut rig, s_old) = rich_torn(4);
    let mut make = factory(&rig.log);
    let mut s = s_old.clone();
    s.tracks[0].mixer.volume_db = -3.0;
    let r = rig.comp.compile(Arc::new(s), rig.comp.active(), PlanGeneration::from_raw(90), &mut make);
    assert_eq!(r.unwrap_err(), CompileError::Faulted);
    let pend = rig.comp.pending_bindings().unwrap_or_default();
    assert!(pend.iter().all(|b| !released.contains(b)), "pending {pend:?} includes released {released:?}");
}

/// Track `a` with a gain (+12 dB), a saturator and a compressor in slots 0..3.
fn chain_session() -> (Session, TrackId) {
    let (mut s, a, _) = audio_session();
    let mut gain = builtin("gain");
    gain.params.insert("gain".into(), 12.0);
    let ins = &mut s.track_mut(a).unwrap().mixer.inserts;
    ins[0] = Some(gain);
    ins[1] = Some(builtin("saturator"));
    ins[2] = Some(builtin("compressor"));
    (s, a)
}

#[test]
fn a_moved_insert_keeps_its_instance_and_the_order_changes_the_sound() {
    let (s, a) = chain_session();
    let mut rig = Rig::new();
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    let before = rig.comp.active_bindings();
    assert_eq!(before.len(), 3);
    let gen_at = |b: &[SlotBinding], slot| b.iter().find(|x| x.0 == a && x.1 == slot).map(|x| x.2);
    let made = rig.log.made.load(Ordering::Relaxed);
    // Gain to the end: [saturator, compressor, gain].
    let mut s2 = s.clone();
    assert!(s2.track_mut(a).unwrap().move_insert(0, 2));
    let mut env = rig.compile(&s2);
    assert_eq!(slot_moves(&env).len(), 3, "all three instances move");
    assert_eq!(adopt_measured(&mut rig.eng, &mut env), Adoption::Adopted);
    assert_eq!(holdings(&env).1, 0, "nothing was replaced");
    rig.comp.reclaim(env);
    let after = rig.comp.active_bindings();
    assert_eq!(gen_at(&after, 2), gen_at(&before, 0), "the gain's instance is now in slot 2");
    assert_eq!(gen_at(&after, 0), gen_at(&before, 1));
    assert_eq!(gen_at(&after, 1), gen_at(&before, 2));
    assert_eq!(rig.log.made.load(Ordering::Relaxed), made, "no instance was made");
    assert_eq!(rig.drops(), 0, "none was dropped");
    // Undo (the original session again) puts every instance back.
    assert_eq!(rig.publish(&s), Adoption::Adopted);
    assert_eq!(rig.comp.active_bindings(), before);
    assert_eq!(rig.drops(), 0);
    // A move before any audio rendered sounds like a fresh plan of the new order; the order matters.
    let mut moved = Rig::new();
    assert_eq!(moved.publish(&s), Adoption::Adopted);
    assert_eq!(moved.publish(&s2), Adoption::Adopted);
    let moved_out = moved.render(4);
    let mut fresh = Rig::new();
    assert_eq!(fresh.publish(&s2), Adoption::Adopted);
    assert_eq!(moved_out, fresh.render(4), "the moved chain renders as a fresh chain in the new order");
    let mut orig = Rig::new();
    assert_eq!(orig.publish(&s), Adoption::Adopted);
    assert_ne!(moved_out, orig.render(4), "gain then saturator is not saturator then gain");
}

/// How many plug-ins a compile of `s` asks the factory for.
fn instances_made(mut comp: PlanCompiler, s: &Session) -> usize {
    let made = std::cell::Cell::new(0usize);
    let mut make = |spec: &InstanceSpec, _: SlotGen| {
        made.set(made.get() + 1);
        create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch)
    };
    let next = comp.active().checked_next().unwrap();
    let env = comp.compile(Arc::new(s.clone()), comp.active(), next, &mut make).unwrap();
    comp.reclaim(env);
    made.get()
}

#[test]
fn an_inactive_tracks_plugins_are_made_by_a_live_compile_but_not_an_offline_one() {
    let (mut s, a, b) = audio_session();
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("maximizer"));
    s.track_mut(b).unwrap().mixer.inserts[0] = Some(builtin("maximizer"));
    s.track_mut(b).unwrap().mixer.inserts[1] = Some(builtin("maximizer"));
    let live = || PlanCompiler::new(target(), PlanLimits::default());
    assert_eq!(instances_made(live(), &s), 3);
    assert_eq!(instances_made(live().skipping_inactive_instances(), &s), 3, "active tracks keep every instance");
    s.track_mut(b).unwrap().inactive = true;
    assert_eq!(instances_made(live(), &s), 3, "the live engine keeps an inactive track warm");
    assert_eq!(instances_made(live().skipping_inactive_instances(), &s), 1, "an offline compile skips it");
    // A muted track can be unmuted by automation: it stays.
    s.track_mut(b).unwrap().inactive = false;
    s.track_mut(b).unwrap().mixer.mute = true;
    assert_eq!(instances_made(live().skipping_inactive_instances(), &s), 3);
}
