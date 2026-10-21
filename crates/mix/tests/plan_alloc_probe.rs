//! Allocation probe for the prepared render plan (F01b, ADR 0009 G9): a realtime engine fed the
//! way the playback callback will feed it — `PlanCompiler::compile` on the control thread,
//! `MixEngine::adopt_plan` plus 64 `render_active` blocks on the audio thread, `reclaim` back on
//! the control thread — allocates and frees nothing on the audio thread for every edit class the
//! spec lists. Only the audio part is measured.
//!
//! Keeps the probe cases of the held `fix/retire-path-no-free` branch (removal, replacement,
//! consecutive removals, track delete, a 70-entry map, 40 clip-effect chains, drop thread),
//! now on the plan API.
//!
//! The binary installs `soundcraft_rt::testing::CountingAlloc` and counts the test thread only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::{Plugin, ProcessStatus, SampleSource, Transport};
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, NewInstance, PlanCompiler, PlanEnvelope, PlanGeneration, PlanLimits, PlanStatus,
    RenderTarget, mark_audio_thread,
};
use soundcraft_model::{Bus, BusId, ChannelFormat, Clip, ClipId, Insert, OutputPath, Route, Session, SourceAudio, SourceId, TrackId, TrackKind};
use soundcraft_rt::testing::{AllocStats, CountingAlloc, measure};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;
const BLOCKS: i64 = 64;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 }
}

/// Shared by every fake instance: drop threads, and the latency `clap:test.latent` reports.
#[derive(Default)]
struct Shared {
    drops: Mutex<Vec<std::thread::ThreadId>>,
    latency: AtomicUsize,
}

/// A hosted plug-in stand-in (`clap:test.*`): a gain that records its drop thread.
struct Fake {
    inner: Box<dyn Plugin>,
    shared: Arc<Shared>,
    latent: bool,
}

impl Plugin for Fake {
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
        self.inner.process(io, frames);
    }
    fn latency(&self) -> usize {
        if self.latent { self.shared.latency.load(Ordering::Relaxed) } else { 0 }
    }
    fn process_status(&self) -> ProcessStatus {
        ProcessStatus::Continue
    }
    fn set_transport(&mut self, t: &Transport) {
        self.inner.set_transport(t);
    }
    fn set_sample(&mut self, s: Option<Arc<dyn SampleSource>>) -> bool {
        self.inner.set_sample(s)
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.shared.drops.lock().unwrap().push(std::thread::current().id());
    }
}

/// The control thread's instance factory (playback's `Side::prepare` becomes this).
fn make(shared: &Arc<Shared>, spec: &InstanceSpec) -> Option<NewInstance> {
    if let Some(rest) = spec.id.strip_prefix("clap:test.") {
        let mut inner = soundcraft_dsp::create("gain")?;
        inner.prepare(SR as f32, BLOCK, spec.ch);
        let plugin: Box<dyn Plugin> = Box::new(Fake { inner, shared: Arc::clone(shared), latent: rest == "latent" });
        return Some(NewInstance { plugin, refused_state: false });
    }
    soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch)
}

struct Rig {
    mix: MixEngine,
    compiler: PlanCompiler,
    shared: Arc<Shared>,
    out: Vec<Vec<f32>>,
    pos: i64,
    generation: PlanGeneration,
}

/// What one measured edit did on the audio thread.
struct Edit {
    stats: AllocStats,
    adoption: Adoption,
    /// Plug-in instances the returned envelope held (dropped at reclaim, on the control side).
    retired: usize,
    /// Fake instances dropped during the measured callback (must be none).
    dropped_in_callback: usize,
    status: PlanStatus,
}

impl Rig {
    fn new(s: &Session) -> Rig {
        MixEngine::force_serial(true);
        mark_audio_thread();
        let mut mix = MixEngine::realtime(target());
        mix.metronome = true;
        let mut rig = Rig {
            mix,
            compiler: PlanCompiler::new(target(), PlanLimits::default()),
            shared: Arc::default(),
            out: vec![vec![0.0; BLOCK]; 2],
            pos: 0,
            generation: PlanGeneration::EMPTY,
        };
        // Warm up (not measured): the first plan and the first blocks.
        let e = rig.edit(s);
        assert!(matches!(e.adoption, Adoption::Adopted));
        rig
    }

    fn compile(&mut self, s: &Session) -> PlanEnvelope {
        self.generation = self.generation.checked_next().unwrap();
        let shared = Arc::clone(&self.shared);
        let mut f = move |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| make(&shared, spec);
        self.compiler.compile(Arc::new(s.clone()), self.compiler.active(), self.generation, &mut f).unwrap()
    }

    /// The audio thread's part: adopt the envelope, then render `BLOCKS` blocks.
    fn callback(&mut self, env: &mut PlanEnvelope) -> Adoption {
        let a = self.mix.adopt_plan(env);
        for _ in 0..BLOCKS {
            self.mix.render_active(self.pos, BLOCK, &mut self.out);
            self.pos += BLOCK as i64;
        }
        a
    }

    /// One document edit: compile (control), callback (audio, measured), reclaim (control).
    fn edit(&mut self, s: &Session) -> Edit {
        let mut env = self.compile(s);
        let before = self.shared.drops.lock().unwrap().len();
        let (adoption, stats) = measure(|| self.callback(&mut env));
        let dropped_in_callback = self.shared.drops.lock().unwrap().len() - before;
        let status = self.mix.status();
        let retired = env.plan().instances();
        self.compiler.reclaim(env);
        Edit { stats, adoption, retired, dropped_in_callback, status }
    }
}

fn no_cost(what: &str, e: &Edit) {
    println!("{what}: {:?}, {:?}, {} retired", e.stats, e.adoption, e.retired);
    assert!(matches!(e.adoption, Adoption::Adopted), "{what}: adopted");
    assert_eq!(e.stats, AllocStats::default(), "{what}, on the audio thread");
    assert_eq!(e.dropped_in_callback, 0, "{what}: nothing dropped in the callback");
}

#[test]
fn new_document_replaces_every_slot_kind_without_callback_allocation() {
    let (mut s, t, clip) = session_with_clip();
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.gain"));
    let instrument = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    s.track_mut(instrument).unwrap().instrument = Some(builtin("sampler"));
    for (module, id) in [("eq", "eq_7band"), ("comp", "compressor")] {
        let param = soundcraft_dsp::plugin_info(id).unwrap().params.first().unwrap();
        s.edit.values.insert(format!("clip_fx.{}.{}.{}", clip.0, module, param.id), f64::from(param.default));
    }
    let mut rig = Rig::new(&s);
    let original = rig.compiler.active_bindings();
    assert_eq!(original.len(), 4, "hosted insert, instrument, clip EQ and compressor");
    rig.generation = rig.generation.checked_next().unwrap();
    let shared = Arc::clone(&rig.shared);
    let mut env = rig
        .compiler
        .compile_with_mode(Arc::new(s), rig.compiler.active(), rig.generation, soundcraft_mix::CompileMode::NewDocument, &mut |spec, _| {
            make(&shared, spec)
        })
        .unwrap();
    let before = rig.shared.drops.lock().unwrap().len();
    let (adoption, stats) = measure(|| rig.callback(&mut env));
    assert_eq!(adoption, Adoption::Adopted);
    assert_eq!(stats, AllocStats::default());
    assert_eq!(rig.shared.drops.lock().unwrap().len(), before);
    assert_eq!(env.plan().instances(), original.len());
    rig.compiler.reclaim(env);
    let replacement = rig.compiler.active_bindings();
    assert_eq!(replacement.len(), original.len());
    for (old, new) in original.iter().zip(&replacement) {
        assert_eq!((old.0, old.1), (new.0, new.1));
        assert_ne!(old.2, new.2);
    }
    assert_eq!(rig.compiler.take_released(), original);
}

fn session_with_clip() -> (Session, TrackId, ClipId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let len = 48_000usize * 4;
    let signal: Vec<f32> = (0..len).map(|i| (i as f32 * 0.01).sin() * 0.25).collect();
    let src = SourceId(1);
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![signal.clone(), signal] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", src, 0, 0, len as i64));
    (s, t, id)
}

fn builtin(id: &str) -> Insert {
    let mut ins = Insert::new(id);
    for p in soundcraft_dsp::plugin_info(id).expect("built-in").params {
        ins.params.insert(p.id.to_string(), p.default);
    }
    ins
}

#[test]
fn adding_replacing_and_removing_inserts_allocates_nothing_on_the_audio_thread() {
    let (mut s, t, _) = session_with_clip();
    let mut rig = Rig::new(&s);
    no_cost("unchanged", &rig.edit(&s));
    for (i, id) in ["room_reverb", "compressor", "eq_7band"].into_iter().enumerate() {
        s.track_mut(t).unwrap().mixer.inserts[i] = Some(builtin(id));
        no_cost(&format!("add {id}"), &rig.edit(&s));
    }
    assert!(rig.out.iter().any(|c| c.iter().any(|x| *x != 0.0)), "the track plays");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("compressor"));
    let e = rig.edit(&s);
    no_cost("replace a built-in", &e);
    assert_eq!(e.retired, 1, "the replaced instance comes back in the envelope");
    s.track_mut(t).unwrap().mixer.inserts[3] = Some(Insert::new("clap:test.fake"));
    no_cost("add a hosted fake", &rig.edit(&s));
    s.track_mut(t).unwrap().mixer.inserts[3] = Some(Insert::new("clap:test.other"));
    let e = rig.edit(&s);
    no_cost("replace a hosted fake", &e);
    assert_eq!(e.retired, 1);
    for i in 0..4 {
        s.track_mut(t).unwrap().mixer.inserts[i] = None;
        let e = rig.edit(&s);
        no_cost(&format!("removal {}", i + 1), &e);
        assert_eq!(e.retired, 1, "consecutive removals each return their instance");
    }
    assert_eq!(rig.shared.drops.lock().unwrap().len(), 2, "both hosted fakes were dropped, at reclaim");
}

#[test]
fn adding_a_track_and_deleting_one_with_six_inserts_allocates_nothing() {
    let (mut s, _, _) = session_with_clip();
    let mut rig = Rig::new(&s);
    let doomed = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    for (i, id) in ["room_reverb", "compressor", "eq_7band", "room_reverb", "compressor", "clap:test.fake"].into_iter().enumerate() {
        s.track_mut(doomed).unwrap().mixer.inserts[i] = Some(if id.starts_with("clap:") { Insert::new(id) } else { builtin(id) });
    }
    no_cost("add a track with six inserts", &rig.edit(&s));
    s.tracks.retain(|t| t.id != doomed);
    let e = rig.edit(&s);
    no_cost("delete it", &e);
    assert_eq!(e.retired, 6, "the strip leaves whole, its six instances inside");
    assert_eq!(rig.shared.drops.lock().unwrap().len(), 1, "the strip left whole, its hosted fake dropped at reclaim");
}

#[test]
fn bus_add_and_delete_and_a_main_format_change_allocate_nothing() {
    let (mut s, t, _) = session_with_clip();
    let mut rig = Rig::new(&s);
    s.busses.push(Bus { id: BusId(500), name: "FX".into(), format: ChannelFormat::Stereo });
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(BusId(500));
    s.track_mut(aux).unwrap().mixer.inserts[0] = Some(builtin("plate_reverb"));
    s.track_mut(t).unwrap().mixer.output = Route::Bus(BusId(500));
    no_cost("bus and aux add", &rig.edit(&s));
    s.track_mut(t).unwrap().mixer.output = Route::Main;
    s.tracks.retain(|x| x.id != aux);
    s.busses.clear();
    no_cost("bus and aux delete", &rig.edit(&s));
    s.outputs = vec![OutputPath { name: "5.1".into(), first_channel: 0, format: ChannelFormat::Surround51 }];
    rig.out = vec![vec![0.0; BLOCK]; 6];
    no_cost("main format to 5.1", &rig.edit(&s));
    assert_eq!(rig.mix.main_channels(), 6);
}

#[test]
fn output_path_routes_allocate_nothing_when_added_moved_and_removed() {
    use soundcraft_model::{IoMap, SendSlot};
    let (mut s, t, _) = session_with_clip();
    let mut rig = Rig::new(&s);
    rig.compiler.set_io(Arc::new(IoMap::for_device("test", "test", 2, 10)));
    rig.out = vec![vec![0.0; BLOCK]; 6];
    s.track_mut(t).unwrap().mixer.output = Route::Hardware("Out 3-4".into());
    no_cost("output to a path", &rig.edit(&s));
    assert!(rig.out[2].iter().any(|x| *x != 0.0), "the path carries the track");
    assert!(rig.out[0].iter().all(|x| *x == 0.0), "and Main does not");
    s.track_mut(t).unwrap().mixer.sends[0] = Some(SendSlot {
        target: Route::Hardware("Out 5-6".into()),
        level_db: 0.0,
        pan: 0.0,
        mute: false,
        pre_fader: true,
        follow_main_pan: true,
        surround: None,
    });
    no_cost("a pre-fader send to a second path", &rig.edit(&s));
    assert!(rig.out[4].iter().any(|x| *x != 0.0));
    s.track_mut(t).unwrap().mixer.output = Route::Hardware("Missing".into());
    no_cost("output to a name the table lacks", &rig.edit(&s));
    s.track_mut(t).unwrap().mixer.output = Route::Main;
    s.track_mut(t).unwrap().mixer.sends[0] = None;
    no_cost("back to Main", &rig.edit(&s));
    // The table itself changing is a plan like any other.
    s.track_mut(t).unwrap().mixer.output = Route::Hardware("Out 3-4".into());
    no_cost("path again", &rig.edit(&s));
    rig.compiler.set_io(Arc::new(IoMap::for_device("test", "test", 2, 2)));
    no_cost("the path vanishes with the table", &rig.edit(&s));
}

#[test]
fn sidechain_keys_allocate_nothing_when_added_changed_and_removed() {
    use soundcraft_model::{Bus, BusId, KeyFrom, KeySource, Tap};
    let (mut s, bass, _) = session_with_clip();
    // The kick is declared after the bass, so the bass renders in a later layer.
    let kick = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let bus = BusId(s.alloc());
    s.busses.push(Bus { id: bus, name: "Drums".into(), format: ChannelFormat::Stereo });
    s.track_mut(kick).unwrap().mixer.output = Route::Bus(bus);
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
    let mut rig = Rig::new(&s);
    let key = |from, tap| Some(KeySource { from, tap });
    s.track_mut(bass).unwrap().mixer.inserts[0] = Some(builtin("compressor"));
    s.track_mut(aux).unwrap().mixer.inserts[0] = Some(builtin("expander_gate"));
    no_cost("inserts without keys", &rig.edit(&s));
    for (what, from, tap) in [("a post key", KeyFrom::Track(kick), Tap::Post), ("a pre key", KeyFrom::Track(kick), Tap::Pre)] {
        s.track_mut(bass).unwrap().mixer.inserts[0].as_mut().unwrap().key = key(from, tap);
        no_cost(what, &rig.edit(&s));
    }
    s.track_mut(aux).unwrap().mixer.inserts[0].as_mut().unwrap().key = key(KeyFrom::Bus(bus), Tap::Post);
    no_cost("a bus key into an aux", &rig.edit(&s));
    s.track_mut(bass).unwrap().mixer.inserts[0].as_mut().unwrap().key = None;
    no_cost("removing a key", &rig.edit(&s));
    s.track_mut(aux).unwrap().mixer.inserts[0].as_mut().unwrap().key = None;
    no_cost("removing the bus key", &rig.edit(&s));
}

#[test]
fn clip_effects_on_and_off_and_forty_chains_allocate_nothing() {
    let (mut s, t, first) = session_with_clip();
    let mut rig = Rig::new(&s);
    let eq = soundcraft_dsp::plugin_info("eq_7band").unwrap().params.first().unwrap();
    let comp = soundcraft_dsp::plugin_info("compressor").unwrap().params.first().unwrap();
    s.edit.values.insert(format!("clip_fx.{}.eq.{}", first.0, eq.id), f64::from(eq.default));
    s.edit.values.insert(format!("clip_fx.{}.comp.{}", first.0, comp.id), f64::from(comp.default));
    no_cost("clip eq + compressor on", &rig.edit(&s));
    s.edit.values.clear();
    let e = rig.edit(&s);
    no_cost("clip effects off", &e);
    assert_eq!(e.retired, 2, "the clip EQ and compressor come back");
    let mut ids = vec![first];
    for i in 1..40 {
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(1), 0, i * 100, 4_000));
        ids.push(id);
    }
    for id in &ids {
        s.edit.values.insert(format!("clip_fx.{}.eq.{}", id.0, eq.id), f64::from(eq.default));
    }
    no_cost("40 clip effect chains", &rig.edit(&s));
    s.edit.values.clear();
    let e = rig.edit(&s);
    no_cost("remove them", &e);
    assert_eq!(e.retired, 40);
}

#[test]
fn a_seventy_entry_parameter_map_grows_through_prepared_storage() {
    let (mut s, t, _) = session_with_clip();
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    let mut rig = Rig::new(&s);
    let ins = s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap();
    for i in 0..70 {
        ins.params.insert(format!("extra{i:02}"), 0.5);
    }
    no_cost("grow the gain's map to 71 entries", &rig.edit(&s));
    assert_eq!(rig.mix.status().applied_mismatch, 0);
    s.track_mut(t).unwrap().mixer.inserts[0] = None;
    no_cost("remove it", &rig.edit(&s));
}

#[test]
fn a_sampler_sample_swap_allocates_and_frees_nothing() {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let audio = |v: f32| Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![vec![v; 4_800]] }));
    s.pool.insert(SourceId(1), audio(0.5));
    s.pool.insert(SourceId(2), audio(0.25));
    let mut ins = builtin("sampler");
    ins.sample = Some(SourceId(1));
    s.track_mut(t).unwrap().instrument = Some(ins);
    let mut rig = Rig::new(&s);
    s.track_mut(t).unwrap().instrument.as_mut().unwrap().sample = Some(SourceId(2));
    no_cost("swap the sample", &rig.edit(&s));
    s.track_mut(t).unwrap().instrument.as_mut().unwrap().sample = None;
    no_cost("clear the sample", &rig.edit(&s));
}

#[test]
fn delay_compensation_within_and_above_capacity_allocates_nothing() {
    let (mut s, t, _) = session_with_clip();
    s.edit.delay_compensation = true;
    let other = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let _ = other;
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("compressor"));
    s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.latent"));
    let mut rig = Rig::new(&s);
    // Within capacity: the compressor's lookahead (a parameter edit: a Session-only plan).
    s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("lookahead".into(), 5.0);
    let e = rig.edit(&s);
    no_cost("lookahead within capacity", &e);
    assert!(rig.mix.latency > 0, "the compressor delays the track");
    assert_eq!(e.status.capacity_exceeded, 0);
    // Above capacity: the hosted plug-in reports more delay than any line holds.
    rig.shared.latency.store(40_000, Ordering::Relaxed);
    s.track_mut(t).unwrap().mixer.volume_db = -1.0;
    let e = rig.edit(&s);
    no_cost("latency above capacity", &e);
    assert!(e.status.capacity_exceeded > 0, "clamped and reported: {:?}", e.status);
    assert!(e.status.required_latency >= 40_000);
}

#[test]
fn seek_stop_and_a_volume_only_plan_allocate_nothing() {
    let (mut s, t, _) = session_with_clip();
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("room_reverb"));
    let mut rig = Rig::new(&s);
    s.track_mut(t).unwrap().mixer.volume_db = -6.0;
    no_cost("volume only", &rig.edit(&s));
    let (_, seek) = measure(|| {
        rig.pos = 96_000;
        for _ in 0..BLOCKS {
            rig.mix.render_active(rig.pos, BLOCK, &mut rig.out);
            rig.pos += BLOCK as i64;
        }
    });
    assert_eq!(seek, AllocStats::default(), "seek");
    let (_, stop) = measure(|| {
        rig.mix.reset();
        rig.mix.all_notes_off();
        rig.mix.monitor_only = true;
        rig.mix.flush_releases(BLOCK);
        for _ in 0..BLOCKS {
            rig.mix.render_active(rig.pos, BLOCK, &mut rig.out);
        }
        rig.mix.live_note(t, 60, 100);
    });
    assert_eq!(stop, AllocStats::default(), "stop");
}

/// F05: path-based delay compensation reads every edge from the strip's prepared delay lines.
/// Latent auxes in a cascade, pre- and post-fader sends, a lookahead change (more delay at
/// runtime), a send flipped pre/post and a reroute that keeps the render order all render with
/// no allocation on the audio thread, and the lines hold the delay (nothing clamped).
#[test]
fn path_based_delay_compensation_allocates_nothing() {
    let (mut s, t, _) = session_with_clip();
    s.edit.delay_compensation = true;
    let mut comp = builtin("compressor");
    comp.params.insert("lookahead".into(), 5.0);
    let (b1, b2) = (BusId(601), BusId(602));
    for (id, n) in [(b1, "B1"), (b2, "B2")] {
        s.busses.push(Bus { id, name: n.into(), format: ChannelFormat::Stereo });
    }
    let a1 = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    let a2 = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    for (a, input, out) in [(a1, b1, Route::Bus(b2)), (a2, b2, Route::Main)] {
        let tr = s.track_mut(a).unwrap();
        tr.mixer.input = Route::Bus(input);
        tr.mixer.output = out;
        tr.mixer.inserts[0] = Some(comp.clone());
    }
    let send = |to: BusId, pre: bool| {
        let mut x = soundcraft_model::SendSlot::new(Route::Bus(to));
        x.pre_fader = pre;
        Some(x)
    };
    s.track_mut(t).unwrap().mixer.sends[0] = send(b1, false);
    s.track_mut(t).unwrap().mixer.sends[1] = send(b2, true);
    let mut rig = Rig::new(&s);
    no_cost("unchanged", &rig.edit(&s));
    s.track_mut(a2).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("lookahead".into(), 10.0);
    no_cost("longer lookahead", &rig.edit(&s));
    s.track_mut(t).unwrap().mixer.sends[1].as_mut().unwrap().pre_fader = false;
    no_cost("send to post-fader", &rig.edit(&s));
    s.track_mut(t).unwrap().mixer.output = Route::Bus(b2);
    let e = rig.edit(&s);
    no_cost("reroute the dry path into the cascade", &e);
    assert_eq!(e.status.capacity_exceeded, 0, "the prepared lines hold every path's delay");
    assert!(rig.out.iter().any(|c| c.iter().any(|x| *x != 0.0)), "the session plays");
}
