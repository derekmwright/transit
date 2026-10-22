//! ADR 0015 slice (e): stems by output path. One pass of the plan renderer fills Main and every
//! output path; each goes to its own file, all padded to the one device latency, so the files line
//! up with each other and with a plain main bounce.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use soundcraft_engine::Engine;
use soundcraft_engine::io::{Bounce, PathBounce, PathBounced, PathNormalize, PathStem, bounce_paths, routed_paths};
use soundcraft_model::{ChannelFormat, Clip, Insert, IoMap, Route, SendSlot, Session, SourceAudio, SourceId, TrackKind};
use soundcraft_time::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SR: u32 = 48_000;
const AT: usize = 2_400;
const LEN: usize = 8_192;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soundcraft-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn opts() -> EncodeOptions {
    EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None }
}

fn io() -> IoMap {
    IoMap::for_device("test", "test", 2, 10)
}

fn range() -> Range {
    Range::new(0, LEN as i64)
}

/// Three stereo tracks, each playing one impulse at `AT`, hard-panned. "Latent" (0.5) has a
/// lookahead limiter, so the mix has a device latency; "Stem" (`stem_gain`) and "Cue" (0.25) have
/// none. `enabled` picks which of the three exist.
struct Rig {
    latent: Route,
    stem: Route,
    cue: Route,
    cue_send: Option<Route>,
    stem_gain: f32,
    enabled: [bool; 3],
}

impl Rig {
    fn new() -> Rig {
        Rig {
            latent: Route::Main,
            stem: Route::Hardware("Out 3-4".into()),
            cue: Route::Main,
            cue_send: Some(Route::Hardware("Out 5-6".into())),
            stem_gain: 0.125,
            enabled: [true; 3],
        }
    }

    fn build(&self) -> Session {
        let mut s = Session::default();
        s.edit.delay_compensation = true;
        s.edit.click = false;
        let spec = [
            ("Latent", 0.5f32, self.latent.clone(), true),
            ("Stem", self.stem_gain, self.stem.clone(), false),
            ("Cue", 0.25, self.cue.clone(), false),
        ];
        for (k, (name, level, out, latent)) in spec.into_iter().enumerate() {
            if !self.enabled[k] {
                continue;
            }
            let mut sig = vec![0.0f32; LEN];
            sig[AT] = level;
            let src = SourceId(s.next_id + 10_000);
            s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![sig.clone(), sig] })));
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
            let cid = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "imp", src, 0, 0, LEN as i64));
            tr.mixer.pan = vec![-1.0, 1.0];
            tr.mixer.output = out;
            if latent {
                tr.mixer.inserts[0] = Some(Insert::new("maximizer"));
            }
            if name == "Cue"
                && let Some(target) = self.cue_send.clone()
            {
                tr.mixer.sends[0] =
                    Some(SendSlot { target, level_db: 0.0, pan: 0.0, mute: false, pre_fader: false, follow_main_pan: true, surround: None });
            }
        }
        s
    }
}

fn read(path: &Path) -> Vec<Vec<f32>> {
    let (_, buf) = soundcraft_audio_io::decode(&std::fs::read(path).unwrap(), None).unwrap();
    buf.channels
}

fn stem<'a>(done: &'a PathBounced, name: &str) -> &'a PathStem {
    done.stems.iter().find(|s| s.path == name).unwrap_or_else(|| panic!("no stem {name}: {:?}", done.stems))
}

/// The sample of each channel's largest value, for channels that have sound.
fn peak_at(ch: &[Vec<f32>]) -> Vec<usize> {
    let mut hits = Vec::new();
    for c in ch {
        let m = c.iter().fold(0.0f32, |a, v| a.max(v.abs()));
        if m > 0.0 {
            hits.push(c.iter().position(|v| v.abs() == m).unwrap());
        }
    }
    hits
}

fn run(s: &Session, dir: &Path, paths: &[&str], normalize: PathNormalize) -> PathBounced {
    let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
    bounce_paths(Arc::new(s.clone()), &io(), dir, range(), &opts(), &paths, normalize).unwrap()
}

fn close(a: &[Vec<f32>], b: &[Vec<f32>], what: &str) {
    assert_eq!((a.len(), a[0].len()), (b.len(), b[0].len()), "{what}: shape");
    for (x, y) in a.iter().flatten().zip(b.iter().flatten()) {
        assert!((x - y).abs() < 1e-6, "{what}: {x} vs {y}");
    }
}

#[test]
fn with_every_track_on_main_the_main_stem_is_the_main_bounce() {
    let rig = Rig { stem: Route::Main, cue_send: None, ..Rig::new() };
    let s = rig.build();
    let dir = scratch("paths-main");
    assert_eq!(routed_paths(&s, &io()), ["Out 1-2"]);
    let done = run(&s, &dir, &[], PathNormalize::Off);
    assert_eq!(done.stems.len(), 1, "only Main is routed");
    let main = stem(&done, "Out 1-2");
    assert!(main.routed && !main.empty);
    assert_eq!(main.file, dir.join("Untitled_Out 1-2.wav"));
    let plain = dir.join("plain.wav");
    Bounce::new(Arc::new(s.clone()), &plain, range(), &opts(), None, false, false).unwrap().run().unwrap();
    assert_eq!(read(&main.file), read(&plain), "bit for bit the plain main bounce");
    assert_eq!(read(&main.file), soundcraft_mix::render_range(&s, range(), 1024));
    assert_eq!(peak_at(&read(&main.file)), [AT, AT], "the limiter's latency is removed");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no part files left");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn each_path_stem_equals_a_solo_render_of_that_path_and_all_align() {
    let s = Rig::new().build();
    let dir = scratch("paths-solo");
    assert_eq!(routed_paths(&s, &io()), ["Out 1-2", "Out 3-4", "Out 5-6"]);
    let done = run(&s, &dir, &[], PathNormalize::Off);
    assert_eq!(done.stems.len(), 3);
    // The reference for a path: only the tracks that feed it, everything routed to Main.
    let solo = |enabled: [bool; 3]| {
        let rig = Rig { stem: Route::Main, cue: Route::Main, cue_send: None, enabled, ..Rig::new() };
        soundcraft_mix::render_range(&rig.build(), range(), 1024)
    };
    let (main, out34, out56) = (read(&stem(&done, "Out 1-2").file), read(&stem(&done, "Out 3-4").file), read(&stem(&done, "Out 5-6").file));
    close(&main, &solo([true, false, true]), "Main is Latent plus the dry Cue");
    assert_eq!(out34, solo([false, true, false]), "Out 3-4 is the Stem track alone");
    close(&out56, &solo([false, false, true]), "Out 5-6 is the Cue send");
    // The limiter's lookahead delays Main, and the dry paths are padded to it, so every file has
    // its impulse where a mix with no latency at all would have it.
    for ch in [&main, &out34, &out56] {
        assert_eq!(peak_at(ch), [AT, AT]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_impulse_on_a_path_lands_where_it_would_land_on_main() {
    let dir = scratch("paths-align");
    let routed = Rig { enabled: [true, true, false], ..Rig::new() }.build();
    let on_main = Rig { stem: Route::Main, enabled: [true, true, false], ..Rig::new() }.build();
    let path_stem = read(&stem(&run(&routed, &dir, &["Out 3-4"], PathNormalize::Off), "Out 3-4").file);
    let main_mix = soundcraft_mix::render_range(&on_main, range(), 1024);
    assert_eq!(peak_at(&path_stem), peak_at(&main_mix));
    assert_eq!(peak_at(&path_stem), [AT, AT]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unrouted_path_renders_a_silent_file_and_is_reported() {
    let s = Rig::new().build();
    let dir = scratch("paths-empty");
    let done = run(&s, &dir, &["Out 1-2", "Out 7-8"], PathNormalize::Off);
    let silent = stem(&done, "Out 7-8");
    assert!(!silent.routed && silent.empty);
    let file = read(&silent.file);
    assert_eq!((file.len(), file[0].len()), (2, LEN));
    assert!(file.iter().flatten().all(|v| *v == 0.0));
    assert!(!stem(&done, "Out 1-2").empty);
    let err = bounce_paths(Arc::new(s), &io(), &dir, range(), &opts(), &["Out 99".to_string()], PathNormalize::Off).unwrap_err();
    assert!(err.to_string().contains("unknown output path"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn joint_normalize_keeps_relative_levels_and_each_does_not() {
    let s = Rig { cue_send: None, enabled: [true, true, false], ..Rig::new() }.build();
    let target = soundcraft_dsp::db_to_gain(-0.1);
    let level = |done: &PathBounced, name: &str| read(&stem(done, name).file).iter().flatten().fold(0.0f32, |a, v| a.max(v.abs()));
    let dir = scratch("paths-norm");
    let off = run(&s, &dir, &[], PathNormalize::Off);
    let (m0, p0) = (level(&off, "Out 1-2"), level(&off, "Out 3-4"));
    assert!(m0 > p0 * 2.0, "the stems differ in level: {m0} vs {p0}");
    let joint = run(&s, &dir, &[], PathNormalize::Joint);
    let (m1, p1) = (level(&joint, "Out 1-2"), level(&joint, "Out 3-4"));
    assert!((m1 - target).abs() < 1e-4, "the loudest file peaks at -0.1 dBFS: {m1}");
    assert!((m1 / p1 - m0 / p0).abs() < 1e-3, "relative level held: {} vs {}", m1 / p1, m0 / p0);
    assert!(joint.joint_gain_db.is_some_and(|g| g > 0.0));
    let each = run(&s, &dir, &[], PathNormalize::Each);
    let (m2, p2) = (level(&each, "Out 1-2"), level(&each, "Out 3-4"));
    assert!((m2 - target).abs() < 1e-4 && (p2 - target).abs() < 1e-4, "{m2} {p2}");
    assert!(each.joint_gain_db.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dropped_bounce_leaves_no_files_and_the_command_reports_empty_paths() {
    let s = Rig::new().build();
    let dir = scratch("paths-cancel");
    let mut job = PathBounce::new(Arc::new(s.clone()), &io(), &dir, range(), &opts(), &[], PathNormalize::Joint).unwrap();
    assert!(job.step(|| false).unwrap().is_none());
    assert!(job.progress() > 0.0 && job.progress() < 0.5);
    drop(job);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    let mut e = Engine::new(s);
    e.set_io_map(io());
    let params = json!({"dir": dir.to_string_lossy(), "paths": ["main", "Out 7-8"], "start": 0, "end": LEN, "bit_depth": "32f"});
    let out = e.execute("file.bounce_paths", &params).unwrap();
    assert_eq!(out["empty"], json!(["Out 7-8"]));
    assert_eq!(out["files"].as_array().unwrap().len(), 2);
    assert_eq!(out["files"][1]["routed"], json!(false));
    assert!(e.execute("file.bounce_paths", &json!({"dir": dir.to_string_lossy(), "paths": []})).is_err());
    assert!(e.execute("file.bounce_paths", &json!({"dir": dir.to_string_lossy(), "normalize": "loud"})).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
