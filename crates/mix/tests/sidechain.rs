//! ADR 0015 slice (c): sidechain keys. A built-in compressor or gate detects on another strip's
//! signal: a track (kick-to-bass) or a bus. The keyed strip renders in a later layer than its
//! source whatever the track order, serially and in parallel, with the same output.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, NewInstance, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, create_instance,
};
use soundcraft_model::{Bus, BusId, ChannelFormat, Clip, Insert, KeyFrom, KeySource, Route, Session, SourceAudio, SourceId, Tap, TrackId, TrackKind};
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;
const LEN: usize = 24_000;
/// Where each kick burst starts, and how long it lasts.
const KICKS: [usize; 3] = [3_000, 10_000, 17_000];
const BURST: usize = 600;
const BASS: f32 = 0.05;

fn session() -> Session {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    s.edit.click = false;
    s
}

fn audio(s: &mut Session, name: &str, sig: Vec<f32>, output: Route) -> TrackId {
    let src = SourceId(s.next_id + 10_000);
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![sig.clone(), sig] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "c", src, 0, 0, LEN as i64));
    tr.mixer.pan = vec![-1.0, 1.0];
    tr.mixer.output = output;
    t
}

/// A kick: a 0.9 burst at each of `KICKS`.
fn kick_signal() -> Vec<f32> {
    let mut v = vec![0.0; LEN];
    for k in KICKS {
        v[k..k + BURST].fill(0.9);
    }
    v
}

/// A sustained bass tone well under the compressor's threshold.
fn bass_signal() -> Vec<f32> {
    (0..LEN).map(|i| BASS * (0.2 + 0.8 * (i as f32 * 0.0131).sin().abs())).collect()
}

/// A compressor that only a loud signal moves: threshold -20 dB, ratio 20, fast attack.
fn ducker(key: Option<KeySource>) -> Insert {
    let mut c = Insert::new("compressor");
    for p in soundcraft_dsp::plugin_info("compressor").unwrap().params {
        c.params.insert(p.id.to_string(), p.default);
    }
    for (k, v) in [("threshold", -20.0), ("ratio", 20.0), ("attack", 0.5), ("release", 10.0), ("knee", 0.0)] {
        c.params.insert(k.into(), v);
    }
    c.key = key;
    c
}

fn bus(s: &mut Session, name: &str) -> BusId {
    let id = BusId(s.alloc());
    s.busses.push(Bus { id, name: name.into(), format: ChannelFormat::Stereo });
    id
}

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 }
}

fn instance(spec: &InstanceSpec) -> Option<NewInstance> {
    create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch).map(|n| NewInstance { plugin: n.plugin, refused_state: false })
}

/// The main mix (left channel) through the plan renderer. `serial` renders on a thread marked as
/// the audio thread with serial rendering forced; otherwise the strips of a layer run in parallel
/// on the global pool.
fn render(s: &Session, serial: bool) -> Vec<f32> {
    let s = s.clone();
    let run = move || {
        let mut mix = MixEngine::realtime(target());
        let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
        let mut make = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| instance(spec);
        let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut make).unwrap();
        assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
        compiler.reclaim(env);
        let mut out = vec![vec![0.0f32; BLOCK]; 2];
        let mut all = Vec::new();
        let mut pos = 0i64;
        while all.len() < LEN {
            mix.render_active(pos, BLOCK, &mut out);
            all.extend_from_slice(&out[0]);
            pos += BLOCK as i64;
        }
        all
    };
    if serial {
        std::thread::spawn(move || {
            MixEngine::force_serial(true);
            soundcraft_mix::mark_audio_thread();
            run()
        })
        .join()
        .unwrap()
    } else {
        run()
    }
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

/// The bass is full level before each kick and ducked well down inside the burst; with `ducks`
/// false it plays through unchanged.
#[track_caller]
fn assert_ducked(out: &[f32], ducks: bool) {
    for k in KICKS {
        let before = peak(&out[k - 800..k - 100]);
        assert!(before > BASS * 0.9 && before < BASS * 1.01, "bass before the kick at {k}: {before}");
        let inside = peak(&out[k + 150..k + BURST]);
        if ducks {
            assert!(inside < BASS * 0.25, "bass ducked inside the kick at {k}: {inside}");
        } else {
            assert!(inside > BASS * 0.2, "bass untouched at {k}: {inside}");
        }
    }
}

/// Four silent tracks, so a layer is wide enough for the parallel path.
fn fillers(s: &mut Session) {
    for i in 0..4 {
        audio(s, &format!("Filler {i}"), vec![0.0; LEN], Route::Main);
    }
}

fn key_from_track(t: TrackId, tap: Tap) -> Option<KeySource> {
    Some(KeySource { from: KeyFrom::Track(t), tap })
}

/// Kick (not in the mix) and bass; `bass_first` declares the bass before the kick.
fn kick_to_bass(bass_first: bool, tap: Tap) -> Session {
    let mut s = session();
    fillers(&mut s);
    let (bass, kick);
    if bass_first {
        bass = audio(&mut s, "Bass", bass_signal(), Route::Main);
        kick = audio(&mut s, "Kick", kick_signal(), Route::None);
    } else {
        kick = audio(&mut s, "Kick", kick_signal(), Route::None);
        bass = audio(&mut s, "Bass", bass_signal(), Route::Main);
    }
    s.track_mut(bass).unwrap().mixer.inserts[0] = Some(ducker(key_from_track(kick, tap)));
    s
}

#[test]
fn a_kick_ducks_the_bass_serially_and_in_parallel() {
    let s = kick_to_bass(false, Tap::Post);
    let parallel = render(&s, false);
    assert_ducked(&parallel, true);
    let serial = render(&s, true);
    assert!(parallel.iter().zip(&serial).all(|(a, b)| a.to_bits() == b.to_bits()), "serial and parallel are bit-identical");
}

#[test]
fn without_a_key_the_compressor_detects_on_its_own_input() {
    let mut s = kick_to_bass(false, Tap::Post);
    for t in s.tracks.iter_mut() {
        if let Some(Some(i)) = t.mixer.inserts.first_mut() {
            i.key = None;
        }
    }
    assert_ducked(&render(&s, false), false);
}

#[test]
fn layering_not_track_order_decides_the_key() {
    let early = render(&kick_to_bass(false, Tap::Post), false);
    let late = render(&kick_to_bass(true, Tap::Post), false);
    assert_ducked(&late, true);
    assert!(early.iter().zip(&late).all(|(a, b)| a.to_bits() == b.to_bits()), "a bass declared before its kick sounds the same");
}

#[test]
fn a_pre_fader_key_ignores_the_source_fader_and_a_post_fader_key_follows_it() {
    for (tap, ducks) in [(Tap::Pre, true), (Tap::Post, false)] {
        let mut s = kick_to_bass(false, tap);
        // The kick's fader is all the way down: nothing of it is heard after the fader.
        s.tracks.iter_mut().filter(|t| t.name == "Kick").for_each(|t| t.mixer.volume_db = -80.0);
        assert_ducked(&render(&s, false), ducks);
    }
}

#[test]
fn a_kick_on_a_bus_ducks_an_aux() {
    let mut s = session();
    fillers(&mut s);
    let (kbus, bbus) = (bus(&mut s, "Kick Bus"), bus(&mut s, "Bass Bus"));
    audio(&mut s, "Kick", kick_signal(), Route::Bus(kbus));
    audio(&mut s, "Bass", bass_signal(), Route::Bus(bbus));
    let a = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Bass Aux"));
    let t = s.track_mut(a).unwrap();
    t.mixer.input = Route::Bus(bbus);
    t.mixer.output = Route::Main;
    t.mixer.pan = vec![-1.0, 1.0];
    t.mixer.inserts[0] = Some(ducker(Some(KeySource { from: KeyFrom::Bus(kbus), tap: Tap::Post })));
    let parallel = render(&s, false);
    assert_ducked(&parallel, true);
    assert_eq!(parallel, render(&s, true));
}

#[test]
fn a_bus_key_into_an_audio_track_is_inert_and_a_missing_source_leaves_the_input() {
    let mut s = session();
    let kbus = bus(&mut s, "Kick Bus");
    audio(&mut s, "Kick", kick_signal(), Route::Bus(kbus));
    let bass = audio(&mut s, "Bass", bass_signal(), Route::Main);
    // A bus fills after audio tracks render: this key reaches nothing, the insert keeps its input.
    s.track_mut(bass).unwrap().mixer.inserts[0] = Some(ducker(Some(KeySource { from: KeyFrom::Bus(kbus), tap: Tap::Post })));
    assert_ducked(&render(&s, false), false);
    s.track_mut(bass).unwrap().mixer.inserts[0] = Some(ducker(key_from_track(TrackId(9_999), Tap::Post)));
    assert_ducked(&render(&s, false), false);
}

// ---- Delay compensation of key edges (ADR 0015 section 4) ----

/// A pure delay of `n` samples that reports `n` as its latency. Stands in for any latent insert.
struct Delay {
    n: usize,
    ring: Vec<Vec<f32>>,
    w: usize,
}

impl soundcraft_dsp::Plugin for Delay {
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

fn delay(n: usize) -> Option<Insert> {
    let mut i = Insert::new("gain");
    i.preset = format!("delay:{n}");
    Some(i)
}

fn make(s: &Session, spec: &InstanceSpec) -> Option<NewInstance> {
    let delay =
        s.track(spec.track).and_then(|t| t.mixer.inserts.get(spec.slot)?.as_ref()).and_then(|i| i.preset.strip_prefix("delay:")?.parse().ok());
    match delay {
        Some(n) => {
            let mut d = Delay { n, ring: Vec::new(), w: 0 };
            soundcraft_dsp::Plugin::prepare(&mut d, SR as f32, BLOCK, spec.ch);
            Some(NewInstance { plugin: Box::new(d), refused_state: false })
        }
        None => instance(spec),
    }
}

/// The main mix (left channel) through the plan renderer, and the latency the engine settled on.
fn render_latent(s: &Session) -> (Vec<f32>, usize) {
    let s = s.clone();
    std::thread::spawn(move || {
        MixEngine::force_serial(true);
        soundcraft_mix::mark_audio_thread();
        let mut mix = MixEngine::realtime(target());
        let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
        let mut maker = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| make(&s, spec);
        let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut maker).unwrap();
        assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
        compiler.reclaim(env);
        let mut out = vec![vec![0.0f32; BLOCK]; 2];
        let mut all = Vec::new();
        let mut pos = 0i64;
        while all.len() < LEN {
            mix.render_active(pos, BLOCK, &mut out);
            all.extend_from_slice(&out[0]);
            pos += BLOCK as i64;
        }
        assert_eq!(mix.status().capacity_exceeded, 0, "the plan's delay lines hold every key's delay");
        (all, mix.latency)
    })
    .join()
    .unwrap()
}

/// A constant bass, so the first sample below half of it is where the ducking starts.
fn steady_bass() -> Vec<f32> {
    vec![BASS; LEN]
}

/// The first kick's duck onset in the device's output, relative to the device latency: the first
/// sample after the latency (plus a margin) where the bass fell below half.
fn onset(out: &[f32], latency: usize) -> usize {
    let from = latency + 1_000;
    let at = out[from..].iter().position(|v| v.abs() < BASS * 0.5).expect("the bass ducks") + from;
    at - latency
}

/// A kick (outside the mix, with `kick` samples of latency) keying a compressor on a bass in the
/// mix: `before` samples of latency ahead of the compressor, `after` behind it.
fn latent_kick_to_bass(kick: usize, before: usize, after: usize, tap: Tap, kick_db: f32) -> Session {
    let mut s = session();
    let k = audio(&mut s, "Kick", kick_signal(), Route::None);
    let b = audio(&mut s, "Bass", steady_bass(), Route::Main);
    let kt = s.track_mut(k).unwrap();
    kt.mixer.volume_db = kick_db;
    if kick > 0 {
        kt.mixer.inserts[0] = delay(kick);
    }
    let bt = s.track_mut(b).unwrap();
    if before > 0 {
        bt.mixer.inserts[0] = delay(before);
    }
    bt.mixer.inserts[1] = Some(ducker(key_from_track(k, tap)));
    if after > 0 {
        bt.mixer.inserts[2] = delay(after);
    }
    s
}

/// The duck lands on the same source sample as without any latency, `device` samples later.
#[track_caller]
fn assert_aligned(s: &Session, reference: &Session, device: usize) {
    let (base, base_latency) = render_latent(reference);
    assert_eq!(base_latency, 0);
    let want = onset(&base, 0);
    assert!((KICKS[0]..KICKS[0] + 200).contains(&want), "the reference ducks right at the kick: {want}");
    let (out, latency) = render_latent(s);
    assert_eq!(latency, device, "the device latency");
    assert_eq!(onset(&out, latency), want, "the duck starts on the kick's own sample");
}

#[test]
fn a_late_key_delays_the_keyed_strip_to_meet_it() {
    // The kick's insert delays it 128 samples; the bass has none. The bass waits for the key.
    let reference = latent_kick_to_bass(0, 0, 0, Tap::Post, 0.0);
    assert_aligned(&latent_kick_to_bass(128, 0, 0, Tap::Post, 0.0), &reference, 128);
}

#[test]
fn an_early_key_is_delayed_to_meet_the_keyed_signal() {
    // The bass has 128 samples of latency ahead of its compressor; the kick has none.
    let reference = latent_kick_to_bass(0, 0, 0, Tap::Post, 0.0);
    assert_aligned(&latent_kick_to_bass(0, 128, 0, Tap::Post, 0.0), &reference, 128);
    // A latent insert behind the compressor does not matter to the key: nothing to align.
    assert_aligned(&latent_kick_to_bass(0, 0, 128, Tap::Post, 0.0), &reference, 128);
    // Both latent: the bass ahead of the compressor by 128, the kick by 64: the key waits 64.
    assert_aligned(&latent_kick_to_bass(64, 128, 0, Tap::Post, 0.0), &reference, 128);
    // And the other way: the kick's 192 is later than the bass's 128: the bass waits 64 more.
    assert_aligned(&latent_kick_to_bass(192, 128, 32, Tap::Post, 0.0), &reference, 224);
}

#[test]
fn a_pre_fader_key_from_a_silent_fader_is_aligned_too() {
    let reference = latent_kick_to_bass(0, 0, 0, Tap::Pre, -80.0);
    assert_aligned(&latent_kick_to_bass(128, 0, 0, Tap::Pre, -80.0), &reference, 128);
    assert_aligned(&latent_kick_to_bass(0, 128, 0, Tap::Pre, -80.0), &reference, 128);
}

#[test]
fn a_key_through_a_latent_bus_chain_is_aligned() {
    // Kick (64) -> Kick Bus -> Drums aux (32) -> Drum Out bus, which keys the Bass Aux.
    let chain = |kick: usize, drums: usize| {
        let mut s = session();
        let (kb, ob, bb) = (bus(&mut s, "Kick Bus"), bus(&mut s, "Drum Out"), bus(&mut s, "Bass Bus"));
        let k = audio(&mut s, "Kick", kick_signal(), Route::Bus(kb));
        audio(&mut s, "Bass", steady_bass(), Route::Bus(bb));
        if kick > 0 {
            s.track_mut(k).unwrap().mixer.inserts[0] = delay(kick);
        }
        let d = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Drums"));
        let t = s.track_mut(d).unwrap();
        t.mixer.input = Route::Bus(kb);
        t.mixer.output = Route::Bus(ob);
        t.mixer.pan = vec![-1.0, 1.0];
        if drums > 0 {
            t.mixer.inserts[0] = delay(drums);
        }
        let a = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Bass Aux"));
        let t = s.track_mut(a).unwrap();
        t.mixer.input = Route::Bus(bb);
        t.mixer.output = Route::Main;
        t.mixer.pan = vec![-1.0, 1.0];
        t.mixer.inserts[0] = Some(ducker(Some(KeySource { from: KeyFrom::Bus(ob), tap: Tap::Post })));
        s
    };
    assert_aligned(&chain(64, 32), &chain(0, 0), 96);
}

/// The main mix (left channel) and the latency on a fake duplex device that plays the sinks back on
/// its input `delay` frames later, with `io`'s measured round trips.
fn render_duplex(s: &Session, io: &soundcraft_model::IoMap, delay: usize) -> (Vec<f32>, usize) {
    let (s, io) = (s.clone(), io.clone());
    std::thread::spawn(move || {
        MixEngine::force_serial(true);
        soundcraft_mix::mark_audio_thread();
        let t = RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: 4, in_channels: 4 };
        let mut mix = MixEngine::realtime(t);
        let mut compiler = PlanCompiler::new(t, PlanLimits::default());
        compiler.set_io(Arc::new(io.clone()));
        let mut maker = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| instance(spec);
        let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut maker).unwrap();
        assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
        compiler.reclaim(env);
        let sinks = soundcraft_mix::plan_sinks(&s, &io, 4);
        let dest = sinks.destinations();
        let main_w = soundcraft_mix::main_channels(&s);
        let mut out = vec![vec![0.0f32; BLOCK]; main_w + sinks.channels()];
        let mut device: Vec<Vec<f32>> = vec![Vec::new(); 4];
        let (mut all, mut pos) = (Vec::new(), 0usize);
        while all.len() < LEN {
            mix.input = (0..4)
                .map(|c| (0..BLOCK).map(|i| (pos + i).checked_sub(delay).and_then(|k| device[c].get(k)).copied().unwrap_or(0.0)).collect())
                .collect();
            mix.render_active(pos as i64, BLOCK, &mut out);
            all.extend_from_slice(&out[0]);
            for c in &mut device {
                c.extend(std::iter::repeat_n(0.0, BLOCK));
            }
            for (k, &d) in dest.iter().enumerate() {
                for i in 0..BLOCK {
                    device[usize::from(d)][pos + i] += out[main_w + k][i];
                }
            }
            pos += BLOCK;
        }
        assert_eq!(mix.status().capacity_exceeded, 0, "the plan's delay lines hold every key's delay");
        (all, mix.latency)
    })
    .join()
    .unwrap()
}

#[test]
fn a_key_after_a_hardware_insert_is_aligned() {
    // The bass goes out to an outboard unit and back (a measured 700 frames round trip) in slot 0,
    // then its compressor in slot 1 is keyed by a kick with no latency. The key waits for the bass.
    const D: usize = 700;
    let (send, ret) = ("Out 3-4", "In 3-4");
    let mut io = soundcraft_model::IoMap::for_device("test", "fake duplex", 4, 4);
    io.set_hardware_latency(soundcraft_model::HardwareLatency {
        send: send.into(),
        ret: ret.into(),
        round_trip_frames: D as u32,
        rate: SR,
        measured_at: 1,
    });
    let reference = latent_kick_to_bass(0, 0, 0, Tap::Post, 0.0);
    let (base, base_latency) = render_latent(&reference);
    assert_eq!(base_latency, 0);
    let want = onset(&base, 0);
    assert!((KICKS[0]..KICKS[0] + 200).contains(&want), "the reference ducks right at the kick: {want}");
    let mut s = reference.clone();
    let bass = s.tracks.iter().find(|t| t.name == "Bass").unwrap().id;
    s.track_mut(bass).unwrap().mixer.inserts[0] = Some(Insert::hardware(send, ret));
    let (out, latency) = render_duplex(&s, &io, D);
    assert_eq!(latency, D, "the device latency is the hardware round trip");
    assert_eq!(onset(&out, latency), want, "the duck starts on the kick's own sample");
}

#[test]
fn keys_do_not_change_a_session_without_latency_or_delay_compensation() {
    // No latency anywhere: the key edge adds no delay, the output is what part 1 rendered.
    let s = kick_to_bass(false, Tap::Post);
    let (out, latency) = render_latent(&s);
    assert_eq!(latency, 0);
    assert_eq!(out, render(&s, true));
    // Delay compensation off: no alignment, the engine reports no latency.
    let mut off = latent_kick_to_bass(128, 0, 0, Tap::Post, 0.0);
    off.edit.delay_compensation = false;
    assert_eq!(render_latent(&off).1, 0);
}
