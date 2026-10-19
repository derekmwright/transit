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
