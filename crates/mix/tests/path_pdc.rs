//! G8 / F05: path-based delay compensation. An impulse placed at the same timeline position on
//! every source arrives at the main mix at the same output sample, within one sample, whatever
//! path it takes: latent inserts, a latent instrument, pre- and post-fader sends, aux → bus → aux
//! cascades, and chains deeper than eight hops. Checked in the legacy realtime path (playback
//! today), the prepared-plan path, and the two are bit-identical. A session loaded with a routing
//! loop renders without hanging, deterministically, with the loop broken.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::Plugin;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, NewInstance, PlanCompiler, PlanGeneration, PlanLimits, PreparedInstance, RenderTarget,
    create_instance, render_range,
};
use soundcraft_model::{
    Bus, BusId, ChannelFormat, Clip, EdgeKind, Insert, Route, RouteEdge, RouteNode, SendSlot, Session, SourceAudio, SourceId, TrackId, TrackKind,
};
use soundcraft_time::Range;
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;
/// Timeline position of every impulse (a whole number of MIDI ticks at the default tempo).
const AT: usize = 2400;
const LEN: usize = 16_384;

/// A pure delay of `n` samples that reports `n` as its latency. Stands in for any latent insert.
struct Delay {
    n: usize,
    ring: Vec<Vec<f32>>,
    w: usize,
}

impl Plugin for Delay {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        soundcraft_dsp::plugin_info("gain").unwrap()
    }
    fn prepare(&mut self, _sr: f32, _mb: usize, ch: usize) {
        self.ring = vec![vec![0.0; self.n.max(1)]; ch.max(2)];
        self.w = 0;
    }
    fn reset(&mut self) {
        self.ring.iter_mut().for_each(|r| r.fill(0.0));
    }
    fn set_param(&mut self, _id: &str, _v: f32) -> bool {
        true
    }
    fn param(&self, _id: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if self.n == 0 {
            return;
        }
        let w0 = self.w;
        for (c, ch) in io.iter_mut().enumerate() {
            let Some(r) = self.ring.get_mut(c) else { continue };
            let mut w = w0;
            for x in ch.iter_mut().take(frames) {
                std::mem::swap(&mut r[w], x);
                w = (w + 1) % self.n;
            }
        }
        self.w = (w0 + frames) % self.n;
    }
    fn latency(&self) -> usize {
        self.n
    }
}

/// An instrument that answers each note-on with an impulse `n` samples later, and reports `n` as
/// its latency (a look-ahead synth or a sampler with a pre-delay).
struct LatentSynth {
    n: usize,
    /// Samples until each pending impulse, counted from the start of the next block.
    pending: Vec<usize>,
}

impl Plugin for LatentSynth {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        soundcraft_dsp::plugin_info("subtractive_synth").unwrap()
    }
    fn prepare(&mut self, _sr: f32, _mb: usize, _ch: usize) {}
    fn reset(&mut self) {
        self.pending.clear();
    }
    fn set_param(&mut self, _id: &str, _v: f32) -> bool {
        true
    }
    fn param(&self, _id: &str) -> Option<f32> {
        None
    }
    fn note_on(&mut self, offset: usize, _note: u8, _velocity: u8) {
        self.pending.push(offset + self.n);
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        for ch in io.iter_mut() {
            ch.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
        for p in &mut self.pending {
            if *p < frames {
                for ch in io.iter_mut() {
                    ch[*p] = 1.0;
                }
            }
            *p = p.wrapping_sub(frames);
        }
        self.pending.retain(|p| *p < usize::MAX / 2);
    }
    fn latency(&self) -> usize {
        self.n
    }
}

/// A latent insert that breaks: it puts out NaN, so the mix contains it (the slot fails and is
/// skipped from then on) while it still reports its latency.
struct Failing(usize);

impl Plugin for Failing {
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
        for ch in io.iter_mut() {
            ch.iter_mut().take(frames).for_each(|x| *x = f32::NAN);
        }
    }
    fn latency(&self) -> usize {
        self.0
    }
}

/// An insert that the factory replaces with a [`Delay`] of `n` samples.
fn delay(n: usize) -> Insert {
    let mut i = Insert::new("gain");
    i.preset = format!("delay:{n}");
    i
}

/// The plug-in for one slot: test stand-ins by preset, built-ins otherwise.
fn instance(s: &Session, spec: &InstanceSpec) -> Option<Box<dyn Plugin>> {
    let t = s.track(spec.track)?;
    let ins = if spec.slot == soundcraft_mix::INSTRUMENT_SLOT { t.instrument.as_ref() } else { t.mixer.inserts.get(spec.slot)?.as_ref() };
    let preset = ins.map(|i| i.preset.as_str()).unwrap_or("");
    if let Some(n) = preset.strip_prefix("delay:") {
        let mut d = Delay { n: n.parse().unwrap(), ring: Vec::new(), w: 0 };
        d.prepare(SR as f32, BLOCK, spec.ch);
        return Some(Box::new(d));
    }
    if let Some(n) = preset.strip_prefix("fail:") {
        return Some(Box::new(Failing(n.parse().unwrap())));
    }
    if let Some(n) = preset.strip_prefix("latent:") {
        return Some(Box::new(LatentSynth { n: n.parse().unwrap(), pending: Vec::new() }));
    }
    create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch).map(|n| n.plugin)
}

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32 }
}

/// Renders `s` from 0 for `LEN` samples plus the mix latency through one path, and returns the
/// main mix's first channel with the latency removed (index = timeline position), and the latency.
fn render_legacy(s: &Session) -> (Vec<f32>, usize) {
    MixEngine::force_serial(true);
    let mut mix = MixEngine::new(SR as f32, BLOCK);
    mix.set_external_instances(true);
    let specs = soundcraft_mix::realtime_instance_specs(s);
    let instances = specs
        .iter()
        .filter_map(|spec| {
            instance(s, spec).map(|plugin| PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id.clone(), ch: spec.ch, plugin })
        })
        .collect();
    mix.adopt(instances);
    mix.ensure_synced(s);
    run(s, |pos, out| {
        mix.render(s, pos, BLOCK, out);
        mix.latency
    })
}

fn render_planned(s: &Session) -> (Vec<f32>, usize) {
    MixEngine::force_serial(true);
    let mut mix = MixEngine::realtime(target());
    let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
    let mut make = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| instance(s, spec).map(|plugin| NewInstance { plugin, refused_state: false });
    let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut make).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    let r = run(s, |pos, out| {
        mix.render_active(pos, BLOCK, out);
        mix.latency
    });
    assert_eq!(mix.status().capacity_exceeded, 0, "the plan's delay lines hold every path's delay");
    r
}

fn run(s: &Session, mut block: impl FnMut(i64, &mut [Vec<f32>]) -> usize) -> (Vec<f32>, usize) {
    let ch = soundcraft_mix::main_channels(s);
    let mut out = vec![vec![0.0f32; BLOCK]; ch];
    let mut all = Vec::new();
    let mut latency = 0;
    let mut pos = 0i64;
    while all.len() < LEN + latency {
        latency = block(pos, &mut out);
        all.extend_from_slice(&out[0]);
        pos += BLOCK as i64;
    }
    (all.split_off(latency.min(all.len())), latency)
}

/// Timeline positions where the output is not silent.
fn arrivals(x: &[f32]) -> Vec<usize> {
    x.iter().enumerate().filter(|(_, v)| v.abs() > 1e-3).map(|(i, _)| i).collect()
}

/// Every arrival within one sample of `AT`, through both realtime paths, and both bit-identical.
/// Returns the arrivals.
#[track_caller]
fn assert_aligned(name: &str, s: &Session) -> Vec<usize> {
    let (a, la) = render_legacy(s);
    let (b, lb) = render_planned(s);
    assert_eq!(la, lb, "{name}: legacy and plan mix latency");
    let diff = a.iter().zip(&b).position(|(x, y)| x.to_bits() != y.to_bits());
    assert!(diff.is_none(), "{name}: legacy and plan differ at {diff:?}");
    let hits = arrivals(&a);
    assert!(!hits.is_empty(), "{name}: the impulses are heard");
    let off: Vec<usize> = hits.iter().copied().filter(|&i| i.abs_diff(AT) > 1).collect();
    assert!(off.is_empty(), "{name}: arrivals off {AT} by more than one sample: {off:?} (latency {la})");
    println!("{name}: arrivals {:?} at latency {la}", hits.iter().map(|&i| i as i64 - AT as i64).collect::<Vec<_>>());
    hits
}

/// The main mix at `AT` is `level` (every path summed there, none lost).
#[track_caller]
fn assert_heard_at(s: &Session, level: f32) {
    let (a, _) = render_legacy(s);
    assert!((a[AT] - level).abs() < 1e-4, "at {AT}: {} (expected {level})", a[AT]);
}

/// An audio track playing an impulse at `AT` (stereo, both channels), routed to the main mix.
fn impulse_track(s: &mut Session, name: &str) -> TrackId {
    let mut sig = vec![0.0f32; LEN];
    sig[AT] = 0.5;
    let src = SourceId(s.next_id + 10_000);
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![sig.clone(), sig] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "imp", src, 0, 0, LEN as i64));
    tr.mixer.pan = vec![-1.0, 1.0];
    t
}

fn bus(s: &mut Session, name: &str) -> BusId {
    let id = BusId(s.alloc());
    s.busses.push(Bus { id, name: name.into(), format: ChannelFormat::Stereo });
    id
}

/// An aux reading `input`, carrying `inserts`, out to `output`.
fn aux(s: &mut Session, name: &str, input: BusId, inserts: Vec<Insert>, output: Route) -> TrackId {
    let a = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some(name));
    let t = s.track_mut(a).unwrap();
    t.mixer.input = Route::Bus(input);
    t.mixer.output = output;
    t.mixer.pan = vec![-1.0, 1.0];
    for (i, ins) in inserts.into_iter().enumerate() {
        t.mixer.inserts[i] = Some(ins);
    }
    a
}

fn send(to: BusId, pre: bool) -> Option<SendSlot> {
    Some(SendSlot { target: Route::Bus(to), level_db: 0.0, pan: 0.0, mute: false, pre_fader: pre, follow_main_pan: true, surround: None })
}

fn session() -> Session {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    s.edit.click = false;
    s
}

fn lookahead_comp() -> Insert {
    let mut c = Insert::new("compressor");
    for p in soundcraft_dsp::plugin_info("compressor").unwrap().params {
        c.params.insert(p.id.to_string(), p.default);
    }
    c.params.insert("lookahead".into(), 5.0);
    c.params.insert("ratio".into(), 1.0);
    c
}

/// (a) Codex's two-aux probe (notes/08 F05): a dry path to main, and a path through two serial
/// auxes each carrying a 5 ms lookahead compressor at ratio 1. Before: the cascade landed 240
/// samples late.
fn two_aux_probe() -> Session {
    let mut s = session();
    let t = impulse_track(&mut s, "Src");
    let b1 = bus(&mut s, "B1");
    let b2 = bus(&mut s, "B2");
    aux(&mut s, "Aux 1", b1, vec![lookahead_comp()], Route::Bus(b2));
    aux(&mut s, "Aux 2", b2, vec![lookahead_comp()], Route::Main);
    s.track_mut(t).unwrap().mixer.sends[0] = send(b1, false);
    s
}

#[test]
fn a_two_aux_lookahead_cascade_lines_up_with_the_dry_path() {
    let s = two_aux_probe();
    assert_aligned("two-aux probe", &s);
    assert_heard_at(&s, 1.0);
    // The offline path (bounces) too.
    let out = render_range(&s, Range::new(0, LEN as i64), 512);
    let hits = arrivals(&out[0]);
    assert!(!hits.is_empty() && hits.iter().all(|&i| i.abs_diff(AT) <= 1), "offline two-aux probe: {hits:?}");
}

/// (b) A latent instrument on an instrument track against a dry audio track.
#[test]
fn a_latent_instrument_lines_up_with_a_dry_audio_track() {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let inst = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Synth"));
    let mut seq = soundcraft_midi::Sequence::default();
    let tempo_ticks = s.tempo.samples_to_ticks(AT as i64, s.sample_rate);
    seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: tempo_ticks, length: 120 });
    let cid = s.new_clip_id();
    let tr = s.track_mut(inst).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, LEN as i64, seq));
    let mut synth = Insert::new("subtractive_synth");
    synth.preset = "latent:300".into();
    tr.instrument = Some(synth);
    tr.mixer.pan = vec![-1.0, 1.0];
    tr.mixer.volume_db = -6.0;
    let hits = assert_aligned("latent instrument", &s);
    assert!(hits.contains(&AT), "the note sounds at {AT}");
}

/// (c) A pre-fader and a post-fader send to auxes with latent inserts, a latent insert on the
/// source itself, and a dry track.
#[test]
fn pre_and_post_fader_sends_through_latent_auxes_line_up() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    impulse_track(&mut s, "Dry");
    let pre = bus(&mut s, "Pre");
    let post = bus(&mut s, "Post");
    aux(&mut s, "Pre Aux", pre, vec![delay(200)], Route::Main);
    aux(&mut s, "Post Aux", post, vec![delay(50), delay(7)], Route::Main);
    let t = s.track_mut(src).unwrap();
    t.mixer.inserts[0] = Some(delay(30));
    t.mixer.sends[0] = send(pre, true);
    t.mixer.sends[1] = send(post, false);
    assert_aligned("pre/post sends", &s);
    assert_heard_at(&s, 2.0);
}

/// (d) aux → bus → aux cascades, with a direct send into the middle bus and a slow track on it.
#[test]
fn aux_bus_aux_cascades_line_up() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let slow = impulse_track(&mut s, "Slow");
    let b1 = bus(&mut s, "B1");
    let b2 = bus(&mut s, "B2");
    let b3 = bus(&mut s, "B3");
    // Declared out of dependency order on purpose.
    aux(&mut s, "Aux 3", b3, vec![delay(11)], Route::Main);
    aux(&mut s, "Aux 2", b2, vec![delay(70)], Route::Bus(b3));
    aux(&mut s, "Aux 1", b1, vec![delay(100)], Route::Bus(b2));
    let t = s.track_mut(src).unwrap();
    t.mixer.sends[0] = send(b1, false);
    t.mixer.sends[1] = send(b2, true);
    let t = s.track_mut(slow).unwrap();
    t.mixer.inserts[0] = Some(delay(500));
    t.mixer.output = Route::Bus(b2);
    assert_aligned("aux-bus-aux cascade", &s);
    assert_heard_at(&s, 2.0);
}

/// (e) A chain of twelve auxes (deeper than the old eight-hop order), declared in reverse.
#[test]
fn a_chain_deeper_than_eight_hops_lines_up() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let busses: Vec<BusId> = (0..12).map(|i| bus(&mut s, &format!("C{i}"))).collect();
    for i in (0..12).rev() {
        let out = if i == 11 { Route::Main } else { Route::Bus(busses[i + 1]) };
        aux(&mut s, &format!("Aux {i}"), busses[i], vec![delay(10 + i)], out);
    }
    s.track_mut(src).unwrap().mixer.sends[0] = send(busses[0], false);
    let hits = assert_aligned("deep chain", &s);
    assert_eq!(hits, vec![AT], "dry path and twelve-aux path sum at one sample");
    assert_heard_at(&s, 1.0);
}

/// A session from an older file with a loop (A → B → A) still renders: deterministically, without
/// hanging, with the loop's feedback edge dropped, and the dry and wet paths aligned.
#[test]
fn a_loaded_routing_loop_renders_without_the_feedback_edge() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let ba = bus(&mut s, "A In");
    let bb = bus(&mut s, "B In");
    aux(&mut s, "A", ba, vec![delay(40)], Route::Bus(bb));
    let b = aux(&mut s, "B", bb, vec![delay(25)], Route::Main);
    s.track_mut(b).unwrap().mixer.sends[0] = send(ba, false);
    s.track_mut(src).unwrap().mixer.sends[0] = send(ba, false);
    assert!(s.route_graph().topological_order().is_err(), "the fixture holds a loop");
    let first = assert_aligned("loaded loop", &s);
    assert_eq!(first, vec![AT], "the feedback edge is dropped, not delayed by a block");
    assert_heard_at(&s, 1.0);
    assert_eq!(assert_aligned("loaded loop, again", &s), first, "deterministic");
}

/// Delay compensation off: nothing is compensated, nothing dropped.
#[test]
fn without_delay_compensation_paths_keep_their_own_latency() {
    let mut s = two_aux_probe();
    s.edit.delay_compensation = false;
    let (a, la) = render_legacy(&s);
    assert_eq!(la, 0);
    assert_eq!(arrivals(&a), vec![AT, AT + 480]);
}

// ---- Refutation review: more topologies, and the gaps mutants survived ----

/// A track that outputs to a bus and also sends (pre- and post-fader, latent insert) into it.
#[test]
fn a_track_sending_into_the_bus_it_outputs_to_lines_up() {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let src = impulse_track(&mut s, "Src");
    let b = bus(&mut s, "B");
    aux(&mut s, "A", b, vec![delay(40)], Route::Main);
    let t = s.track_mut(src).unwrap();
    t.mixer.inserts[0] = Some(delay(30));
    t.mixer.output = Route::Bus(b);
    t.mixer.sends[0] = send(b, true);
    t.mixer.sends[1] = send(b, false);
    assert_aligned("send into own output bus", &s);
    assert_heard_at(&s, 2.0);
}

#[test]
fn a_folder_inside_a_folder_lines_up() {
    let mut s = session();
    let s1 = impulse_track(&mut s, "S1");
    let s2 = impulse_track(&mut s, "S2");
    impulse_track(&mut s, "S3");
    let b1 = bus(&mut s, "F1 Bus");
    let b2 = bus(&mut s, "F2 Bus");
    let f2 = s.add_track(TrackKind::Folder, ChannelFormat::Stereo, Some("F2"));
    let f1 = s.add_track(TrackKind::Folder, ChannelFormat::Stereo, Some("F1"));
    for (f, inp, out, d) in [(f1, b1, Route::Bus(b2), 100), (f2, b2, Route::Main, 50)] {
        let t = s.track_mut(f).unwrap();
        t.mixer.input = Route::Bus(inp);
        t.mixer.output = out;
        t.mixer.pan = vec![-1.0, 1.0];
        t.mixer.inserts[0] = Some(delay(d));
    }
    s.track_mut(s1).unwrap().mixer.output = Route::Bus(b1);
    s.track_mut(s1).unwrap().mixer.inserts[0] = Some(delay(7));
    s.track_mut(s2).unwrap().mixer.output = Route::Bus(b2);
    assert_aligned("folder in folder", &s);
    assert_heard_at(&s, 1.5);
}

#[test]
fn an_aux_returning_to_a_bus_that_feeds_two_auxes_lines_up() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let ba = bus(&mut s, "A In");
    let bb = bus(&mut s, "B In");
    aux(&mut s, "B1", bb, vec![delay(10)], Route::Main);
    aux(&mut s, "B2", bb, vec![delay(300)], Route::Main);
    aux(&mut s, "A", ba, vec![delay(40)], Route::Bus(bb));
    s.track_mut(src).unwrap().mixer.sends[0] = send(ba, false);
    s.track_mut(src).unwrap().mixer.sends[1] = send(bb, false);
    assert_aligned("bus feeding two auxes", &s);
    assert_heard_at(&s, 2.5);
}

#[test]
fn a_master_insert_and_a_track_with_no_output_line_up() {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let src = impulse_track(&mut s, "NoOut");
    let b = bus(&mut s, "B");
    aux(&mut s, "A", b, vec![delay(200)], Route::Main);
    let t = s.track_mut(src).unwrap();
    t.mixer.output = Route::None;
    t.mixer.sends[0] = send(b, false);
    let m = s.add_track(TrackKind::Master, ChannelFormat::Stereo, Some("Master"));
    s.track_mut(m).unwrap().mixer.inserts[0] = Some(delay(77));
    assert_aligned("output none + master insert", &s);
    assert_heard_at(&s, 1.0);
    assert_eq!(render_legacy(&s).1, 277);
}

#[test]
fn inactive_tracks_do_not_raise_latency_and_muted_ones_keep_it() {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let slow = impulse_track(&mut s, "Slow");
    s.track_mut(slow).unwrap().mixer.inserts[0] = Some(delay(500));
    s.track_mut(slow).unwrap().inactive = true;
    let b = bus(&mut s, "B");
    let a = aux(&mut s, "A", b, vec![delay(900)], Route::Main);
    s.track_mut(a).unwrap().inactive = true;
    assert_eq!((render_legacy(&s).1, render_planned(&s).1), (0, 0));
    s.track_mut(slow).unwrap().inactive = false;
    s.track_mut(slow).unwrap().mixer.mute = true;
    assert_eq!((render_legacy(&s).1, render_planned(&s).1), (500, 500));
}

/// An aux whose output and a send both feed its own input (a loaded self-loop).
#[test]
fn a_self_looped_aux_renders_without_feedback() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let b = bus(&mut s, "B");
    let a = aux(&mut s, "A", b, vec![delay(40)], Route::Bus(b));
    s.track_mut(a).unwrap().mixer.sends[0] = send(b, false);
    s.track_mut(src).unwrap().mixer.sends[0] = send(b, false);
    s.track_mut(src).unwrap().mixer.output = Route::None;
    let (x, _) = render_legacy(&s);
    let (y, _) = render_planned(&s);
    assert!(x.iter().zip(&y).all(|(p, q)| p.to_bits() == q.to_bits()));
    assert!(arrivals(&x).is_empty(), "the aux only feeds itself: dropped, nothing reaches main");
}

/// Two loops sharing edges, plus a chain into the shared bus: the edges the route graph reports
/// cut are exactly the ones the render drops.
#[test]
fn the_reported_cut_set_is_the_one_the_render_drops() {
    let mut s = session();
    let br = bus(&mut s, "R In");
    let by = bus(&mut s, "Y In");
    let bx = bus(&mut s, "X In");
    let r = aux(&mut s, "R", br, vec![delay(10)], Route::Main);
    let y = aux(&mut s, "Y", by, vec![delay(20)], Route::Bus(br));
    let x = aux(&mut s, "X", bx, vec![delay(30)], Route::Bus(by));
    s.track_mut(r).unwrap().mixer.sends[0] = send(bx, false);
    s.track_mut(y).unwrap().mixer.sends[0] = send(bx, false);
    let bw = bus(&mut s, "W In");
    aux(&mut s, "W", bw, vec![delay(5)], Route::Bus(br));
    let src = impulse_track(&mut s, "S");
    s.track_mut(src).unwrap().mixer.output = Route::Bus(bw);
    let s2 = impulse_track(&mut s, "S2");
    s.track_mut(s2).unwrap().mixer.output = Route::Bus(by);
    let l = s.route_graph().layering();
    let yi = s.track_index(y).unwrap();
    let y_to_r = RouteEdge { from: RouteNode::Track(yi), to: RouteNode::Bus(0), kind: EdgeKind::Output };
    let (a, _) = render_legacy(&s);
    let (b, _) = render_planned(&s);
    assert!(a.iter().zip(&b).all(|(p, q)| p.to_bits() == q.to_bits()), "legacy == plan");
    // S reaches main through W and R: 0.5. S2 does through Y and R only if Y to R In plays.
    let y_plays = (a[AT] - 1.0).abs() < 1e-4;
    assert!(y_plays || (a[AT] - 0.5).abs() < 1e-4, "level {}", a[AT]);
    assert_eq!(l.cut.contains(&y_to_r), !y_plays, "Y to R In is reported cut exactly when the render drops it: {:?}", l.cut);
    assert!(!l.cut.is_empty() && l.cut.len() == l.loops.len());
    let _ = x;
}

/// A latent insert that fails is skipped, and stops counting toward delay compensation.
#[test]
fn a_failed_latent_aux_insert_stops_counting() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let b = bus(&mut s, "B");
    let mut f = Insert::new("gain");
    f.preset = "fail:500".into();
    aux(&mut s, "A", b, vec![f], Route::Main);
    s.track_mut(src).unwrap().mixer.sends[0] = send(b, false);
    let hits = assert_aligned("failed aux insert", &s);
    assert_eq!(hits, vec![AT]);
    assert_eq!(render_legacy(&s).1, 0, "the failed insert's latency no longer counts");
}

/// A pre-fader send with no compensation delay still takes the pre-fader signal.
#[test]
fn a_zero_delay_pre_fader_send_ignores_the_fader() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let b = bus(&mut s, "B");
    aux(&mut s, "A", b, vec![], Route::Main);
    let t = s.track_mut(src).unwrap();
    t.mixer.volume_db = -144.0;
    t.mixer.sends[0] = send(b, true);
    assert_aligned("pre-fader send, fader down", &s);
    assert_heard_at(&s, 0.5);
}

/// A plan sizes its delay lines for a fresh instance's latency above the smallest capacity.
#[test]
fn a_fresh_instance_longer_than_the_smallest_line_is_held() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let b = bus(&mut s, "B");
    aux(&mut s, "A", b, vec![delay(20_000)], Route::Main);
    s.track_mut(src).unwrap().mixer.sends[0] = send(b, false);
    let hits = assert_aligned("20,000-sample aux", &s);
    assert_eq!(hits, vec![AT]);
}

/// A latent insert whose latency a test turns up while it plays (a plug-in restart that reports
/// more latency): it delays its input by that much.
struct Knob {
    knob: Arc<std::sync::atomic::AtomicUsize>,
    ring: Vec<Vec<f32>>,
    w: usize,
}

impl Plugin for Knob {
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

/// Plan path: a confirmed latency increase past the plan's lines is reported, and recompiling the
/// same Session after `observe` gives a structural plan with longer lines, which realigns.
#[test]
fn a_plan_realigns_after_a_latency_increase_through_observe_and_a_recompile() {
    const PERIOD: usize = 4096;
    let mut s = session();
    let pulses = |s: &mut Session, name: &str, level: f32| {
        let len = 48_000usize * 10;
        let sig: Vec<f32> = (0..len).map(|i| if i % PERIOD == 0 { level } else { 0.0 }).collect();
        let src = SourceId(s.next_id + 20_000);
        s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![sig.clone(), sig] })));
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
        let cid = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "p", src, 0, 0, len as i64));
        t
    };
    pulses(&mut s, "Dry", 0.25);
    let lat = pulses(&mut s, "Lat", 0.5);
    s.track_mut(lat).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    let knob = Arc::new(std::sync::atomic::AtomicUsize::new(100));
    let mut make = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| {
        if spec.track == lat && spec.slot == 0 {
            return Some(NewInstance {
                plugin: Box::new(Knob { knob: knob.clone(), ring: vec![vec![0.0; 100_000]; 2], w: 0 }),
                refused_state: false,
            });
        }
        create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch)
    };
    MixEngine::force_serial(true);
    let mut mix = MixEngine::realtime(target());
    let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
    let shared = Arc::new(s.clone());
    let mut env = compiler.compile(Arc::clone(&shared), compiler.active(), PlanGeneration::from_raw(1), &mut make).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let mut pos = 0i64;
    let mut run = |mix: &mut MixEngine, n: usize| {
        let mut all = Vec::new();
        for _ in 0..n {
            mix.render_active(pos, BLOCK, &mut out);
            all.extend_from_slice(&out[0]);
            pos += BLOCK as i64;
        }
        all
    };
    run(&mut mix, 20);
    knob.store(20_000, std::sync::atomic::Ordering::Relaxed);
    run(&mut mix, 8);
    let status = mix.status();
    assert!(status.capacity_exceeded > 0 && status.required_latency >= 20_000, "{status:?}");
    compiler.observe(&status);
    let mut env = compiler.compile(Arc::clone(&shared), compiler.active(), PlanGeneration::from_raw(2), &mut make).unwrap();
    assert!(env.plan().is_structural(), "the hint makes the same Session a structural plan with longer lines");
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    let all = run(&mut mix, 200);
    let late = &all[all.len() - 4 * PERIOD..];
    let hits: Vec<f32> = late.iter().copied().filter(|x| x.abs() > 1e-3).collect();
    assert!(!hits.is_empty() && hits.iter().all(|x| (x - 0.75).abs() < 1e-4), "realigned: {hits:?}");
    assert_eq!(mix.status().capacity_exceeded, 0);
}

/// A bounce of a looped bus holds its sources, not the loop's feedback: the dropped edge is not
/// written at all (it would land in the bus after its reader ran).
#[test]
fn a_bounce_of_a_looped_bus_holds_no_feedback() {
    let mut s = session();
    let src = impulse_track(&mut s, "Src");
    let ba = bus(&mut s, "A In");
    let bb = bus(&mut s, "B In");
    aux(&mut s, "A", ba, vec![delay(40)], Route::Bus(bb));
    let b = aux(&mut s, "B", bb, vec![delay(25)], Route::Main);
    s.track_mut(b).unwrap().mixer.sends[0] = send(ba, false);
    s.track_mut(src).unwrap().mixer.sends[0] = send(ba, false);
    let out = soundcraft_mix::render_bus(&s, ba, Range::new(0, LEN as i64), 512);
    // Offline, the stand-in inserts are plain gains: B's feedback would land on the same sample.
    assert_eq!(arrivals(&out[0]), vec![AT], "only the source's send, at its own time");
    assert!((out[0][AT] - 0.5).abs() < 1e-4, "no feedback summed in: {}", out[0][AT]);
}

/// Plan path: a tone through a latency rise, the clamp, `observe` and the recompiled plan with
/// longer lines: no gap and no step at adoption (the lines carry their history over; a longer
/// delay waits for its history, then crossfades).
#[test]
fn a_plan_with_longer_lines_takes_over_seamlessly() {
    let mut s = session();
    let len = 48_000usize * 30;
    let tone: Vec<f32> = (0..len).map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 100.0 / 48_000.0).sin() * 0.25).collect();
    s.pool.insert(SourceId(61), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![tone.clone(), tone] })));
    let dry = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Dry"));
    let cid = s.new_clip_id();
    s.track_mut(dry).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "t", SourceId(61), 0, 0, len as i64));
    let lat = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Lat"));
    s.track_mut(lat).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    let knob = Arc::new(std::sync::atomic::AtomicUsize::new(100));
    let mut make = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| {
        if spec.track == lat && spec.slot == 0 {
            return Some(NewInstance {
                plugin: Box::new(Knob { knob: knob.clone(), ring: vec![vec![0.0; 100_000]; 2], w: 0 }),
                refused_state: false,
            });
        }
        create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch)
    };
    MixEngine::force_serial(true);
    let mut mix = MixEngine::realtime(target());
    let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
    let shared = Arc::new(s.clone());
    let mut env = compiler.compile(Arc::clone(&shared), compiler.active(), PlanGeneration::from_raw(1), &mut make).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let mut all = Vec::new();
    for k in 0..500i64 {
        if k == 60 {
            knob.store(20_000, std::sync::atomic::Ordering::Relaxed);
        }
        if k == 80 {
            compiler.observe(&mix.status());
            let mut env = compiler.compile(Arc::clone(&shared), compiler.active(), PlanGeneration::from_raw(2), &mut make).unwrap();
            assert!(env.plan().is_structural());
            assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
            compiler.reclaim(env);
        }
        mix.render_active(k * BLOCK as i64, BLOCK, &mut out);
        all.extend_from_slice(&out[0]);
    }
    let x = &all[BLOCK * 2..];
    let step = x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
    assert!(step < 0.02, "a step of {step}");
    let mut quiet = 0usize;
    let mut longest = 0usize;
    for v in x {
        quiet = if v.abs() < 1e-3 { quiet + 1 } else { 0 };
        longest = longest.max(quiet);
    }
    assert!(longest < 16, "a gap of {longest} samples");
    assert_eq!(mix.status().capacity_exceeded, 0, "realigned with nothing clamped");
}
