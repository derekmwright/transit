//! Parity (F01b): for representative sessions and edit sequences, a realtime engine fed through
//! `PlanCompiler::compile` + `MixEngine::adopt_plan` + `render_active` puts out what the golden
//! outputs in `tests/golden/plan_parity` hold. They were generated from the pre-plan renderer,
//! which these sessions matched bit for bit; they are the oracle now (`UPDATE_GOLDEN=1` rewrites
//! them from the plan path, which a change in the mix's output must justify).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_midi::{Note, Sequence};
use soundcraft_mix::{Adoption, DeviceGeneration, InstanceSpec, MixEngine, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, create_instance};
use soundcraft_model::{
    Bus, BusId, ChannelFormat, Clip, Insert, OutputPath, Route, SendSlot, Session, SourceAudio, SourceId, SurroundPan, TrackId, TrackKind,
};
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 }
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

/// Tolerance of a golden comparison: `|plan - golden| <= ABS + REL * |golden|`. The goldens were made
/// on Windows (MSVC CRT libm, x86-64 FMA contraction); Linux/glibc and macOS (aarch64 fuses
/// multiply-adds differently, Apple libm) round the transcendental functions in the filters and the
/// plate reverb differently. In the feedback paths that drift is absolute, not proportional: CI saw
/// 8.2e-5 at a level of 0.045 (Linux) and 5.0e-5 at 0.0023 (macOS) in the aux chains session.
const GOLDEN_ABS_TOLERANCE: f32 = 3e-4;
const GOLDEN_REL_TOLERANCE: f32 = 2e-3;

/// What a plan render put out, thinned to every eighth frame (a different phase each block) so the
/// fixtures stay small, plus each step's delay compensation; compared with a checked-in file.
struct Golden {
    name: String,
    values: Vec<f32>,
}

impl Golden {
    fn new(name: &str) -> Self {
        let slug: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
        Golden { name: slug, values: Vec::new() }
    }

    fn block(&mut self, k: usize, block: &[Vec<f32>]) {
        for ch in block {
            self.values.extend(ch.iter().skip(k % 8).step_by(8).copied());
        }
    }

    fn value(&mut self, v: usize) {
        self.values.push(v as f32);
    }

    fn check(self) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/plan_parity").join(format!("{}.f32", self.name));
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes: Vec<u8> = self.values.iter().flat_map(|v| v.to_le_bytes()).collect();
            std::fs::write(&path, bytes).unwrap();
            return;
        }
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e} (UPDATE_GOLDEN=1 writes it)", path.display()));
        let want: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        assert_eq!(want.len(), self.values.len(), "{}: golden length", self.name);
        let bad = want
            .iter()
            .zip(&self.values)
            .position(|(w, g)| (w - g).abs().partial_cmp(&(GOLDEN_ABS_TOLERANCE + GOLDEN_REL_TOLERANCE * w.abs())).is_none_or(|o| o.is_gt()));
        assert!(bad.is_none(), "{}: value {bad:?}: golden {:?}, plan {:?}", self.name, bad.map(|i| want[i]), bad.map(|i| self.values[i]));
    }
}

/// Renders `sessions` one after the other (each one an edit of the one before), `blocks` blocks
/// each, through the plan path and checks the golden output.
fn assert_parity(name: &str, sessions: &[Session], blocks: usize) {
    MixEngine::force_serial(true);
    let mut planned = Planned::new();
    let mut golden = Golden::new(name);
    let mut pos = 0i64;
    let mut heard = false;
    for s in sessions {
        planned.update(s);
        let ch = soundcraft_mix::main_channels(s);
        let mut b = vec![vec![0.0f32; BLOCK]; ch];
        for k in 0..blocks {
            planned.mix.render_active(pos, BLOCK, &mut b);
            golden.block(k, &b);
            heard |= b.iter().flatten().any(|x| *x != 0.0);
            pos += BLOCK as i64;
        }
        golden.value(planned.mix.latency);
    }
    assert!(heard, "{name}: the sessions make sound");
    golden.check();
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
fn inserts_sends_auxes_and_a_master_fader_match_the_golden() {
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
fn instruments_and_midi_match_the_golden() {
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
fn surround_and_clip_effects_match_the_golden() {
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
fn aux_chains_folders_inactive_tracks_and_two_masters_match_the_golden() {
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
fn seeks_with_held_notes_match_the_golden() {
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
    let mut planned = Planned::new();
    planned.update(&s);
    let mut golden = Golden::new("seeks with held notes");
    let mut b = vec![vec![0.0f32; BLOCK]; 2];
    for (k, pos) in [0i64, 256, 512, 768, 20_000, 20_256, 0, 256, 512].into_iter().enumerate() {
        if k == 5 {
            s.track_mut(t).unwrap().mixer.inserts[0] = Some(builtin("chorus"));
            planned.update(&s);
        }
        planned.mix.render_active(pos, BLOCK, &mut b);
        golden.block(k, &b);
    }
    golden.check();
}

#[test]
fn reordered_tracks_match_the_golden() {
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

#[test]
fn an_inactive_aux_keeps_its_latency_across_a_structural_plan() {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    let a = add_audio(&mut s, ChannelFormat::Stereo, 1, 0.02);
    s.busses.push(Bus { id: BusId(900), name: "V".into(), format: ChannelFormat::Stereo });
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(BusId(900));
    let mut la = builtin("compressor");
    la.params.insert("lookahead".into(), 5.0);
    s.track_mut(aux).unwrap().mixer.inserts[0] = Some(la);
    s.track_mut(a).unwrap().mixer.sends[0] = send(BusId(900), false);
    let mut steps = vec![s.clone()];
    s.track_mut(aux).unwrap().inactive = true;
    s.track_mut(a).unwrap().mixer.inserts[0] = Some(builtin("gain"));
    steps.push(s.clone());
    assert_parity("inactive aux latency", &steps, 6);
}

/// F05: path-based delay compensation in both paths, sample for sample: a lookahead cascade of
/// auxes, pre- and post-fader sends, a chain deeper than eight auxes declared in reverse, a
/// latent instrument, a lookahead change at runtime, and a routing loop (an older session) added
/// and removed.
#[test]
fn path_based_delay_compensation_matches_the_golden() {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    let a = add_audio(&mut s, ChannelFormat::Stereo, 1, 0.02);
    let b = add_audio(&mut s, ChannelFormat::Mono, 2, 0.05);
    let mut la = builtin("compressor");
    la.params.insert("lookahead".into(), 5.0);
    let chain: Vec<BusId> = (0..10).map(|i| BusId(920 + i)).collect();
    for (i, id) in chain.iter().enumerate() {
        s.busses.push(Bus { id: *id, name: format!("C{i}"), format: ChannelFormat::Stereo });
    }
    let mut auxes = Vec::new();
    for i in (0..10).rev() {
        let x = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
        let tr = s.track_mut(x).unwrap();
        tr.mixer.input = Route::Bus(chain[i]);
        tr.mixer.output = if i == 9 { Route::Main } else { Route::Bus(chain[i + 1]) };
        if i % 3 == 0 {
            tr.mixer.inserts[0] = Some(la.clone());
        }
        auxes.push(x);
    }
    auxes.reverse();
    s.track_mut(a).unwrap().mixer.sends[0] = send(chain[0], false);
    s.track_mut(a).unwrap().mixer.sends[1] = send(chain[4], true);
    s.track_mut(b).unwrap().mixer.inserts[0] = Some(builtin("maximizer"));
    s.track_mut(b).unwrap().mixer.sends[0] = send(chain[7], false);
    let inst = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let mut seq = Sequence::default();
    seq.notes.push(Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 4000 });
    let cid = s.new_clip_id();
    let tr = s.track_mut(inst).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 96_000, seq));
    tr.instrument = Some(builtin("subtractive_synth"));
    tr.mixer.inserts[0] = Some(la.clone());
    tr.mixer.output = Route::Bus(chain[2]);
    let mut steps = vec![s.clone()];
    // Longer lookahead deep in the chain (latency changes, structure does not).
    s.track_mut(auxes[6]).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("lookahead".into(), 9.0);
    steps.push(s.clone());
    // A loop, as an older file could hold: the last aux feeds the first one's bus.
    s.track_mut(auxes[9]).unwrap().mixer.sends[0] = send(chain[0], false);
    steps.push(s.clone());
    s.track_mut(auxes[9]).unwrap().mixer.sends[0] = None;
    s.track_mut(a).unwrap().mixer.sends[1].as_mut().unwrap().pre_fader = false;
    steps.push(s.clone());
    assert_parity("path-based delay compensation", &steps, 24);
}
