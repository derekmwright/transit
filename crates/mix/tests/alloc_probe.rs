//! Allocation probe (F01b-1, ADR 0009 G9): a realtime engine fed the way the playback callback
//! feeds it allocates and frees nothing on the audio thread when a document edit adds a built-in
//! insert or a clip effect. The plug-in instances are made on the control thread, as playback's
//! `Side::prepare` does, and reach the engine through `MixEngine::adopt`; the callback then only
//! adopts, syncs, ships what the engine retired, and renders.
//!
//! The binary installs `soundcraft_rt::testing::CountingAlloc` and counts the test thread only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{MixEngine, PreparedInstance, mark_audio_thread};
use soundcraft_model::{ChannelFormat, Clip, ClipId, Insert, Session, SourceAudio, SourceId, TrackId, TrackKind};
use soundcraft_rt::testing::{AllocStats, CountingAlloc, measure};
use std::collections::HashMap;
use std::sync::Arc;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const SR: f32 = 48_000.0;
const BLOCK: usize = 256;
const BLOCKS: i64 = 64;

/// The control thread's side: makes the instances a session needs that the engine does not have
/// yet (the same diff playback's `Side::prepare` keeps).
#[derive(Default)]
struct Control {
    shadow: HashMap<(TrackId, usize), (String, usize)>,
}

impl Control {
    fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        let specs = soundcraft_mix::realtime_instance_specs(s);
        self.shadow.retain(|k, _| specs.iter().any(|x| (x.track, x.slot) == *k));
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            self.shadow.insert(key, (spec.id.clone(), spec.ch));
            if let Some(n) = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), SR, BLOCK, spec.ch) {
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin: n.plugin });
            }
        }
        out
    }
}

/// What reaches the UI thread from one callback: the engine's retired instances (and any buffer
/// it hands back), dropped there.
type Garbage = Vec<Box<dyn soundcraft_dsp::Plugin>>;

struct Rig {
    mix: MixEngine,
    control: Control,
    out: Vec<Vec<f32>>,
    pos: i64,
}

impl Rig {
    fn new(s: &Session) -> Rig {
        MixEngine::force_serial(true);
        mark_audio_thread();
        let mut mix = MixEngine::new(SR, BLOCK);
        mix.set_external_instances(true);
        mix.metronome = true;
        let mut rig = Rig { mix, control: Control::default(), out: vec![vec![0.0; BLOCK]; 2], pos: 0 };
        // Warm up (not measured): first session, strips and lazy per-strip storage, meters.
        let instances = rig.control.prepare(s);
        let g = rig.callback(s, instances);
        drop(g);
        rig
    }

    /// One document edit as the playback callback handles it, then `BLOCKS` blocks of playback.
    /// Returns what the callback ships to the UI thread. Playback sends every returned buffer
    /// that holds memory (an empty one with capacity too) to be dropped on the UI thread.
    fn callback(&mut self, s: &Session, instances: Vec<PreparedInstance>) -> Option<Garbage> {
        self.mix.adopt(instances);
        self.mix.ensure_synced(s);
        let g = self.mix.take_retired();
        let ship = (g.capacity() > 0).then_some(g);
        for _ in 0..BLOCKS {
            self.mix.render(s, self.pos, BLOCK, &mut self.out);
            self.pos += BLOCK as i64;
        }
        ship
    }

    /// The edit, measured on the audio thread (this thread) only. The control-thread work
    /// (`prepare`) and the UI thread's drop of the shipped garbage are outside the measurement.
    fn edit(&mut self, s: &Session) -> AllocStats {
        self.edit_retiring(s).0
    }

    /// [`Rig::edit`], also saying how many instances the edit retired (shipped to the UI thread).
    fn edit_retiring(&mut self, s: &Session) -> (AllocStats, usize) {
        let instances = self.control.prepare(s);
        let (garbage, stats) = measure(|| self.callback(s, instances));
        (stats, garbage.map_or(0, |g| g.len()))
    }
}

fn session_with_clip() -> (Session, TrackId, ClipId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let len = 48_000usize * 4;
    let signal: Vec<f32> = (0..len).map(|i| (i as f32 * 0.01).sin() * 0.25).collect();
    let src = SourceId(1);
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![signal.clone(), signal] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", src, 0, 0, len as i64));
    (s, t, id)
}

/// A built-in insert as the engine's `mix.insert` command adds it: its parameter map holds the
/// plug-in's defaults.
fn builtin(id: &str) -> Insert {
    let mut ins = Insert::new(id);
    for p in soundcraft_dsp::plugin_info(id).expect("built-in").params {
        ins.params.insert(p.id.to_string(), p.default);
    }
    ins
}

#[test]
fn adding_room_reverb_during_render_allocates_nothing_on_the_audio_thread() {
    let (mut s, t, _) = session_with_clip();
    let mut rig = Rig::new(&s);
    let unchanged = rig.edit(&s);
    println!("edit without changes: {unchanged:?}");

    s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("room_reverb"));
    let added = rig.edit(&s);
    println!("add room_reverb: {added:?}");
    assert!(rig.out.iter().any(|c| c.iter().any(|x| *x != 0.0)), "the track plays");

    // A second edit returns the buffer the first one brought (the engine hands it back with
    // `take_retired`), so repeated edits stay allocation-free too.
    s.track_mut(t).unwrap().mixer.inserts[1] = Some(builtin("compressor"));
    let again = rig.edit(&s);
    println!("add compressor: {again:?}");

    assert_eq!(unchanged, AllocStats::default(), "an edit that changes nothing, on the audio thread");
    assert_eq!(added, AllocStats::default(), "adding room_reverb, on the audio thread");
    assert_eq!(again, AllocStats::default(), "adding a second built-in, on the audio thread");

    // Both were in service: removing them retires both, to be dropped on the UI thread.
    let ins = &mut s.track_mut(t).unwrap().mixer.inserts;
    ins[0] = None;
    ins[1] = None;
    assert_eq!(rig.edit_retiring(&s).1, 2, "the room_reverb and compressor instances were placed");
}

#[test]
fn adding_a_clip_effect_during_render_allocates_nothing_on_the_audio_thread() {
    let (mut s, _, clip) = session_with_clip();
    let mut rig = Rig::new(&s);
    let unchanged = rig.edit(&s);

    let eq = soundcraft_dsp::plugin_info("eq_7band").expect("eq").params.first().expect("an eq parameter");
    let comp = soundcraft_dsp::plugin_info("compressor").expect("compressor").params.first().expect("a compressor parameter");
    s.edit.values.insert(format!("clip_fx.{}.eq.{}", clip.0, eq.id), f64::from(eq.default));
    s.edit.values.insert(format!("clip_fx.{}.comp.{}", clip.0, comp.id), f64::from(comp.default));
    let added = rig.edit(&s);
    println!("add clip eq + compressor: {added:?}");
    assert_eq!(unchanged, AllocStats::default(), "an edit that changes nothing, on the audio thread");
    assert_eq!(added, AllocStats::default(), "adding clip effects, on the audio thread");

    // Both modules were in service: removing the clip's effects retires both.
    s.edit.values.clear();
    assert_eq!(rig.edit_retiring(&s).1, 2, "the clip EQ and compressor were placed");
}

// ---- F05 refutation review: delay compensation on the legacy path ----

/// A latent insert whose latency a test turns up while it plays: it delays its input by that
/// much, from storage made up front (it allocates nothing itself).
struct VarDelay {
    knob: Arc<std::sync::atomic::AtomicUsize>,
    ring: Vec<Vec<f32>>,
    w: usize,
}

impl VarDelay {
    fn new(knob: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        VarDelay { knob, ring: vec![vec![0.0; 400_000]; 2], w: 0 }
    }
}

impl soundcraft_dsp::Plugin for VarDelay {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        soundcraft_dsp::plugin_info("gain").unwrap()
    }
    fn prepare(&mut self, _sr: f32, _mb: usize, _ch: usize) {}
    fn reset(&mut self) {}
    fn set_param(&mut self, _id: &str, _v: f32) -> bool {
        true
    }
    fn param(&self, _id: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let d = self.latency();
        let n = self.ring[0].len();
        for (c, ch) in io.iter_mut().enumerate().take(2) {
            for (i, x) in ch.iter_mut().take(frames).enumerate() {
                let w = (self.w + i) % n;
                self.ring[c][w] = *x;
                *x = self.ring[c][(w + n - d) % n];
            }
        }
        self.w = (self.w + frames) % n;
    }
    fn latency(&self) -> usize {
        self.knob.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Every `PERIOD` samples an impulse of `level`, on a stereo track routed to the main mix.
const PERIOD: usize = 4096;

fn pulse_track(s: &mut Session, name: &str, level: f32, src: u64) -> TrackId {
    let len = 48_000usize * 30;
    let sig: Vec<f32> = (0..len).map(|i| if i % PERIOD == 0 { level } else { 0.0 }).collect();
    s.pool.insert(SourceId(src), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![sig.clone(), sig] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "p", SourceId(src), 0, 0, len as i64));
    t
}

/// Rising plug-in latency on the legacy path: rendering never allocates (the delay lines clamp
/// and report the shortfall); `ensure_synced`, the control-side step, grows them keeping their
/// history, after which the dry and the latent track line up again.
#[test]
fn rising_latency_allocates_nothing_while_rendering_and_realigns_after_the_control_side_grow() {
    MixEngine::force_serial(true);
    mark_audio_thread();
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    s.edit.click = false;
    pulse_track(&mut s, "Dry", 0.25, 31);
    let lat = pulse_track(&mut s, "Lat", 0.5, 32);
    s.track_mut(lat).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    let knob = Arc::new(std::sync::atomic::AtomicUsize::new(100));
    let mut mix = MixEngine::new(SR, BLOCK);
    mix.set_external_instances(true);
    mix.adopt(vec![PreparedInstance { track: lat, slot: 0, id: "gain".into(), ch: 2, plugin: Box::new(VarDelay::new(knob.clone())) }]);
    mix.ensure_synced(&s);
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let mut pos = 0i64;
    let mut render = |mix: &mut MixEngine, n: usize, keep: &mut Vec<f32>| {
        for _ in 0..n {
            mix.render(&s, pos, BLOCK, &mut out);
            keep.extend_from_slice(&out[0]);
            pos += BLOCK as i64;
        }
    };
    let mut tail = Vec::with_capacity(BLOCK * 4096);
    render(&mut mix, 20, &mut tail);
    assert_eq!(mix.latency, 100);
    for target in [5_000usize, 20_000, 100_000, 300_000] {
        knob.store(target, std::sync::atomic::Ordering::Relaxed);
        let ((), stats) = measure(|| render(&mut mix, 50, &mut tail));
        println!("latency -> {target}: mix latency {}, {stats:?}, status {:?}", mix.latency, mix.status());
        assert_eq!(stats, AllocStats::default(), "latency {target}: rendering allocates nothing");
    }
    knob.store(20_000, std::sync::atomic::Ordering::Relaxed);
    render(&mut mix, 4, &mut tail);
    let short = mix.status();
    assert!(short.capacity_exceeded > 0 && short.required_latency >= 20_000, "the shortfall is reported: {short:?}");
    // The control-side step (playback's session update) grows the lines.
    mix.ensure_synced(&s);
    tail.clear();
    render(&mut mix, 200, &mut tail);
    let late = &tail[tail.len() - 4 * PERIOD..];
    let hits: Vec<f32> = late.iter().copied().filter(|x| x.abs() > 1e-3).collect();
    assert!(!hits.is_empty(), "the pulses play");
    assert!(hits.iter().all(|x| (x - 0.75).abs() < 1e-4), "dry and latent pulses coincide again: {hits:?}");
    assert_eq!(mix.status().capacity_exceeded, 0, "nothing clamped after the grow");
}

/// A routing change (a send retargeted, a loop as an older file could hold) re-orders the strips
/// with no allocation on the audio thread.
#[test]
fn routing_changes_and_a_loop_allocate_nothing_on_the_audio_thread() {
    MixEngine::force_serial(true);
    mark_audio_thread();
    let (mut s, t, _) = session_with_clip();
    s.edit.delay_compensation = true;
    let b = s.add_bus("B", ChannelFormat::Stereo);
    let a = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("A"));
    s.track_mut(a).unwrap().mixer.input = soundcraft_model::Route::Bus(b);
    let b2 = s.add_bus("B2", ChannelFormat::Stereo);
    let a2 = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("A2"));
    s.track_mut(a2).unwrap().mixer.input = soundcraft_model::Route::Bus(b2);
    s.track_mut(t).unwrap().mixer.sends[0] = Some(soundcraft_model::SendSlot::new(soundcraft_model::Route::Bus(b)));
    let mut rig = Rig::new(&s);
    assert_eq!(rig.edit(&s), AllocStats::default(), "unchanged");
    s.track_mut(t).unwrap().mixer.sends[1] = Some(soundcraft_model::SendSlot::new(soundcraft_model::Route::Bus(b2)));
    assert_eq!(rig.edit(&s), AllocStats::default(), "a new send");
    s.track_mut(t).unwrap().mixer.sends[0].as_mut().unwrap().target = soundcraft_model::Route::Bus(b2);
    assert_eq!(rig.edit(&s), AllocStats::default(), "a send retargeted");
    // A2 feeds A, A feeds A2: a loop.
    s.track_mut(a2).unwrap().mixer.output = soundcraft_model::Route::Bus(b);
    s.track_mut(a).unwrap().mixer.output = soundcraft_model::Route::Bus(b2);
    assert_eq!(rig.edit(&s), AllocStats::default(), "a loop");
    assert!(rig.out.iter().any(|c| c.iter().any(|x| *x != 0.0)), "the track plays");
}
