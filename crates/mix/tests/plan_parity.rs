//! Parity (F01b): for representative sessions and edit sequences, a realtime engine fed through
//! `PlanCompiler::compile` + `MixEngine::adopt_plan` + `render_active` puts out exactly what
//! today's realtime path (`MixEngine::adopt` + `render`, instances from
//! `realtime_instance_specs`) puts out, sample for sample, on every channel.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_midi::{Note, Sequence};
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, PlanCompiler, PlanGeneration, PlanLimits, PreparedInstance, RenderTarget, create_instance,
};
use soundcraft_model::{
    Bus, BusId, ChannelFormat, Clip, Insert, OutputPath, Route, SendSlot, Session, SourceAudio, SourceId, SurroundPan, TrackId, TrackKind,
};
use std::collections::HashMap;
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32 }
}

/// Today's realtime path, as `playback` drives it.
struct Legacy {
    mix: MixEngine,
    shadow: HashMap<(TrackId, usize), (String, usize)>,
}

impl Legacy {
    fn new() -> Self {
        let mut mix = MixEngine::new(SR as f32, BLOCK);
        mix.set_external_instances(true);
        Legacy { mix, shadow: HashMap::new() }
    }

    fn update(&mut self, s: &Session) {
        let specs = soundcraft_mix::realtime_instance_specs(s);
        self.shadow.retain(|k, _| specs.iter().any(|x| (x.track, x.slot) == *k));
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            self.shadow.insert(key, (spec.id.clone(), spec.ch));
            if let Some(n) = create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch) {
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin: n.plugin });
            }
        }
        self.mix.adopt(out);
        self.mix.ensure_synced(s);
        drop(self.mix.take_retired());
    }
}

/// The plan path.
struct Planned {
    mix: MixEngine,
    compiler: PlanCompiler,
    generation: PlanGeneration,
}

impl Planned {
    fn new() -> Self {
        Planned {
            mix: MixEngine::realtime(target()),
            compiler: PlanCompiler::new(target(), PlanLimits::default()),
            generation: PlanGeneration::EMPTY,
        }
    }

    fn update(&mut self, s: &Session) {
        self.generation = self.generation.checked_next().unwrap();
        let mut make = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch);
        let mut env = self.compiler.compile(Arc::new(s.clone()), self.compiler.active(), self.generation, &mut make).unwrap();
        assert_eq!(self.mix.adopt_plan(&mut env), Adoption::Adopted);
        self.compiler.reclaim(env);
    }
}

/// Renders `sessions` one after the other (each one an edit of the one before), `blocks` blocks
/// each, through both paths, and compares every channel of every block.
fn assert_parity(name: &str, sessions: &[Session], blocks: usize) {
    MixEngine::force_serial(true);
    let mut legacy = Legacy::new();
    let mut planned = Planned::new();
    let mut pos = 0i64;
    let mut heard = false;
    for (step, s) in sessions.iter().enumerate() {
        legacy.update(s);
        planned.update(s);
        let ch = soundcraft_mix::main_channels(s);
        let mut a = vec![vec![0.0f32; BLOCK]; ch];
        let mut b = vec![vec![0.0f32; BLOCK]; ch];
        for k in 0..blocks {
            legacy.mix.render(s, pos, BLOCK, &mut a);
            planned.mix.render_active(pos, BLOCK, &mut b);
            for c in 0..ch {
                let diff = a[c].iter().zip(&b[c]).position(|(x, y)| x.to_bits() != y.to_bits());
                assert!(
                    diff.is_none(),
                    "{name}: step {step}, block {k}, channel {c}, frame {diff:?}: {:?} vs {:?}",
                    a[c][diff.unwrap()],
                    b[c][diff.unwrap()]
                );
            }
            heard |= a.iter().flatten().any(|x| *x != 0.0);
            pos += BLOCK as i64;
        }
        assert_eq!(legacy.mix.latency, planned.mix.latency, "{name}: step {step}: delay compensation");
    }
    assert!(heard, "{name}: the sessions make sound");
}

fn add_audio(s: &mut Session, fmt: ChannelFormat, src: u64, freq: f32) -> TrackId {
    let t = s.add_track(TrackKind::Audio, fmt, None);
    let len = 48_000usize;
    let ch = fmt.channels();
    let signal: Vec<f32> = (0..len).map(|i| if i < 12_000 { (i as f32 * freq).sin() * 0.3 } else { 0.0 }).collect();
    s.pool.insert(SourceId(src), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![signal; ch] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(src), 0, 0, len as i64));
    t
}

fn builtin(id: &str) -> Insert {
    let mut ins = Insert::new(id);
    for p in soundcraft_dsp::plugin_info(id).unwrap().params {
        ins.params.insert(p.id.to_string(), p.default);
    }
    ins
}

fn send(bus: BusId, pre: bool) -> Option<SendSlot> {
    let x = SendSlot { target: Route::Bus(bus), level_db: -6.0, pan: 0.0, mute: false, pre_fader: pre, follow_main_pan: true, surround: None };
    Some(x)
}

#[test]
fn inserts_sends_auxes_and_a_master_fader_match_todays_path() {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    let a = add_audio(&mut s, ChannelFormat::Stereo, 1, 0.02);
    let b = add_audio(&mut s, ChannelFormat::Mono, 2, 0.05);
    s.busses.push(Bus { id: BusId(900), name: "Verb".into(), format: ChannelFormat::Stereo });
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(BusId(900));
    s.track_mut(aux).unwrap().mixer.inserts[0] = Some(builtin("plate_reverb"));
    let master = s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
    s.track_mut(master).unwrap().mixer.inserts[0] = Some(builtin("maximizer"));
    let ta = s.track_mut(a).unwrap();
    ta.mixer.inserts[0] = Some(builtin("compressor"));
    ta.mixer.inserts[1] = Some(builtin("room_reverb"));
    ta.mixer.sends[0] = send(BusId(900), false);
    let tb = s.track_mut(b).unwrap();
    tb.mixer.sends[0] = send(BusId(900), true);
    tb.mixer.pan = vec![-0.4];
    let mut steps = vec![s.clone()];
    // Volume only.
    s.track_mut(a).unwrap().mixer.volume_db = -4.0;
    steps.push(s.clone());
    // Insert add on b, and a latent one (PDC changes).
    s.track_mut(b).unwrap().mixer.inserts[0] = Some(builtin("maximizer"));
    steps.push(s.clone());
    // Insert removal on a, solo on b.
    s.track_mut(a).unwrap().mixer.inserts[0] = None;
    s.track_mut(b).unwrap().mixer.solo = true;
    steps.push(s.clone());
    // Delete track a.
    s.tracks.retain(|t| t.id != a);
    steps.push(s.clone());
    assert_parity("inserts, sends, aux, master", &steps, 12);
}

#[test]
fn instruments_and_midi_match_todays_path() {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let mut seq = Sequence::default();
    for (i, p) in [60u8, 64, 67, 72].into_iter().enumerate() {
        seq.notes.push(Note { pitch: p, velocity: 100, release_velocity: 64, channel: 0, start: i as i64 * 480, length: 600 });
    }
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 96_000, seq));
    tr.instrument = Some(builtin("subtractive_synth"));
    tr.mixer.inserts[0] = Some(builtin("chorus"));
    let mut steps = vec![s.clone()];
    s.track_mut(t).unwrap().mixer.inserts[1] = Some(builtin("plate_reverb"));
    steps.push(s.clone());
    s.track_mut(t).unwrap().instrument.as_mut().unwrap().params.insert("level".into(), -12.0);
    steps.push(s.clone());
    assert_parity("instrument", &steps, 20);
}

#[test]
fn surround_and_clip_effects_match_todays_path() {
    let mut s =
        Session { outputs: vec![OutputPath { name: "5.1".into(), first_channel: 0, format: ChannelFormat::Surround51 }], ..Session::default() };
    let mono = add_audio(&mut s, ChannelFormat::Mono, 1, 0.03);
    s.track_mut(mono).unwrap().mixer.surround = Some(SurroundPan::default());
    let wide = add_audio(&mut s, ChannelFormat::Surround51, 2, 0.01);
    s.track_mut(wide).unwrap().mixer.inserts[0] = Some(builtin("eq_7band"));
    let clip = s.track(mono).unwrap().clips()[0].id.0;
    let eq = soundcraft_dsp::plugin_info("eq_7band").unwrap().params.first().unwrap();
    s.edit.values.insert(format!("clip_fx.{clip}.eq.{}", eq.id), f64::from(eq.default));
    s.edit.values.insert(format!("clip_fx.{clip}.comp.threshold"), -20.0);
    s.edit.values.insert(format!("clip_fx.{clip}.gain"), -3.0);
    let mut steps = vec![s.clone()];
    s.edit.values.retain(|k, _| !k.contains(".comp."));
    steps.push(s.clone());
    s.outputs = vec![OutputPath { name: "St".into(), first_channel: 0, format: ChannelFormat::Stereo }];
    steps.push(s.clone());
    assert_parity("surround and clip effects", &steps, 10);
}

#[test]
fn aux_chains_folders_inactive_tracks_and_two_masters_match_todays_path() {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    let a = add_audio(&mut s, ChannelFormat::Stereo, 1, 0.02);
    let b = add_audio(&mut s, ChannelFormat::Mono, 2, 0.05);
    let c = add_audio(&mut s, ChannelFormat::Stereo, 3, 0.011);
    for (id, n) in [(901u64, "A"), (902, "B"), (903, "C")] {
        s.busses.push(Bus { id: BusId(id), name: n.into(), format: ChannelFormat::Stereo });
    }
    // aux2 (declared first) reads B, which aux1 writes: dependency order must put aux1 first.
    let aux2 = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux2).unwrap().mixer.input = Route::Bus(BusId(902));
    s.track_mut(aux2).unwrap().mixer.inserts[0] = Some(builtin("plate_reverb"));
    let aux1 = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux1).unwrap().mixer.input = Route::Bus(BusId(901));
    s.track_mut(aux1).unwrap().mixer.output = Route::Bus(BusId(902));
    let mut la = builtin("compressor");
    la.params.insert("lookahead".into(), 5.0);
    s.track_mut(aux1).unwrap().mixer.inserts[0] = Some(la.clone());
    s.track_mut(aux1).unwrap().mixer.sends[0] = send(BusId(903), true);
    let folder = s.add_track(TrackKind::Folder, ChannelFormat::Stereo, None);
    s.track_mut(folder).unwrap().mixer.input = Route::Bus(BusId(903));
    let dead = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(dead).unwrap().mixer.input = Route::Bus(BusId(901));
    s.track_mut(dead).unwrap().mixer.inserts[0] = Some(la.clone());
    s.track_mut(dead).unwrap().inactive = true;
    let m1 = s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
    s.track_mut(m1).unwrap().mixer.inserts[0] = Some(builtin("maximizer"));
    let m2 = s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
    s.track_mut(m2).unwrap().mixer.volume_db = -2.0;
    s.track_mut(a).unwrap().mixer.sends[0] = send(BusId(901), false);
    s.track_mut(a).unwrap().mixer.sends[1] = send(BusId(902), true);
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(la.clone());
    s.track_mut(b).unwrap().mixer.output = Route::Bus(BusId(902));
    s.track_mut(c).unwrap().mixer.sends[0] = send(BusId(903), false);
    let mut steps = vec![s.clone()];
    // Reroute: aux2 now feeds aux1's input (order flips), c goes inactive.
    s.track_mut(aux1).unwrap().mixer.output = Route::Main;
    s.track_mut(aux2).unwrap().mixer.input = Route::Bus(BusId(903));
    s.track_mut(aux2).unwrap().mixer.output = Route::Bus(BusId(901));
    steps.push(s.clone());
    s.track_mut(c).unwrap().inactive = true;
    steps.push(s.clone());
    s.track_mut(dead).unwrap().inactive = false;
    steps.push(s.clone());
    assert_parity("aux chains", &steps, 16);
}

#[test]
fn seeks_with_held_notes_match_todays_path() {
    MixEngine::force_serial(true);
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let mut seq = Sequence::default();
    for i in 0..130u8 {
        seq.notes.push(Note { pitch: i.min(127), velocity: 100, release_velocity: 64, channel: 0, start: i64::from(i) * 7, length: 40_000 });
    }
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 96_000, seq));
    tr.instrument = Some(builtin("subtractive_synth"));
    let mut legacy = Legacy::new();
    let mut planned = Planned::new();
    legacy.update(&s);
    planned.update(&s);
    let mut a = vec![vec![0.0f32; BLOCK]; 2];
    let mut b = vec![vec![0.0f32; BLOCK]; 2];
    for (k, pos) in [0i64, 256, 512, 768, 20_000, 20_256, 0, 256, 512].into_iter().enumerate() {
        if k == 5 {
            s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("chorus"));
            legacy.update(&s);
            planned.update(&s);
        }
        legacy.mix.render(&s, pos, BLOCK, &mut a);
        planned.mix.render_active(pos, BLOCK, &mut b);
        assert_eq!(a, b, "block {k} at {pos}");
    }
}

#[test]
fn reordered_tracks_match_todays_path() {
    let mut s = Session::default();
    let a = add_audio(&mut s, ChannelFormat::Stereo, 1, 0.02);
    let b = add_audio(&mut s, ChannelFormat::Stereo, 2, 0.07);
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("room_reverb"));
    s.track_mut(b).unwrap().mixer.pan = vec![-1.0, -1.0];
    s.track_mut(a).unwrap().mixer.pan = vec![1.0, 1.0];
    let mut steps = vec![s.clone()];
    // The same structure in another track order: the strips stay in the same render order.
    s.tracks.swap(0, 1);
    steps.push(s.clone());
    assert_parity("reordered tracks", &steps, 12);

    // An aux and an audio track trade places: audio strips render before auxes either way, so
    // only the track indices the strips read change.
    let mut s = Session::default();
    s.busses.push(Bus { id: BusId(910), name: "R".into(), format: ChannelFormat::Stereo });
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(BusId(910));
    s.track_mut(aux).unwrap().mixer.inserts[0] = Some(builtin("plate_reverb"));
    s.track_mut(aux).unwrap().mixer.pan = vec![1.0, 1.0];
    let dry = add_audio(&mut s, ChannelFormat::Stereo, 3, 0.03);
    s.track_mut(dry).unwrap().mixer.sends[0] = send(BusId(910), false);
    s.track_mut(dry).unwrap().mixer.pan = vec![-1.0, -1.0];
    let mut steps = vec![s.clone()];
    s.tracks.swap(0, 1);
    steps.push(s.clone());
    assert_parity("aux and audio trade places", &steps, 12);
}
