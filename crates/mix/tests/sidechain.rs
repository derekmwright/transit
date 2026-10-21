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
