//! Asynchronous import (L30): `file.prepare_import` on a worker, `file.commit_import` as one undo
//! step. Files live in a temp folder each test makes.

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A 16-bit PCM WAV.
fn wav(channels: u16, rate: u32, frames: u32) -> Vec<u8> {
    let data = frames * u32::from(channels) * 2;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    b.extend_from_slice(&(channels * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..frames * u32::from(channels) {
        let v = ((i % 64) as i16 - 32) * 300;
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("sc-async-import-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn engine_with_track() -> (Engine, u64) {
    let mut e = Engine::default();
    let t = e.execute("track.new", &json!({"kind": "audio", "format": "stereo", "name": "Audio 1"})).unwrap();
    (e, t["tracks"][0].as_u64().unwrap())
}

fn prepare(e: &mut Engine, path: &str) -> u64 {
    e.execute("file.prepare_import", &json!({"path": path})).unwrap()["id"].as_u64().unwrap()
}

/// Poll until the job is done or failed; returns the last progress report and every percentage seen.
fn wait(e: &mut Engine, id: u64) -> (Value, Vec<u64>) {
    let t0 = Instant::now();
    let mut seen = Vec::new();
    loop {
        let v = e.execute("file.import_progress", &json!({"id": id})).unwrap();
        seen.push(v["progress"].as_u64().unwrap());
        if matches!(v["state"].as_str(), Some("done" | "failed")) {
            return (v, seen);
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "import {id} never finished: {v}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn clips(e: &Engine, track: u64) -> Vec<(i64, i64, String)> {
    let s = e.session();
    s.track(soundcraft_engine::model::TrackId(track)).unwrap().clips().iter().map(|c| (c.start, c.end() - c.start, c.name.clone())).collect()
}

#[test]
fn prepare_reaches_100_then_commit_places_the_clip_with_its_length_in_one_undo_step() {
    let d = Dir::new("ok");
    let (mut e, track) = engine_with_track();
    let path = d.file("tone.wav", &wav(2, 48_000, 24_000));
    let rev = e.revision;
    let undo = e.undo_history().len();
    let id = prepare(&mut e, &path);
    let (v, seen) = wait(&mut e, id);
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["progress"], 100);
    assert_eq!((v["frames"].as_u64(), v["channels"].as_u64()), (Some(24_000), Some(2)));
    assert!((v["duration"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "progress never goes back: {seen:?}");
    assert_eq!((e.revision, e.undo_history().len(), e.session().sources.len()), (rev, undo, 0), "preparing changes nothing");
    let r = e.execute("file.commit_import", &json!({"id": id, "track": track, "at": 96_000})).unwrap();
    assert_eq!(r["frames"], 24_000);
    assert_eq!(clips(&e, track), vec![(96_000, 24_000, "tone".to_string())]);
    assert_eq!(e.undo_history().len(), undo + 1);
    assert_eq!(e.undo_label(), Some("Import tone"));
    assert_eq!(e.journal.iter().filter(|j| j.0 == "file.commit_import").count(), 0, "a prepare id cannot be replayed");
    let j = e.journal.last().expect("a journal entry");
    assert_eq!(j.0, "file.import_audio");
    assert_eq!((j.1["path"].as_str(), j.1["track"].as_u64(), j.1["at"].as_i64()), (Some(path.as_str()), Some(track), Some(96_000)));
    assert!(e.session().pool.get(e.session().sources[0].id).is_some(), "the media is in the pool");
    assert!(e.execute("file.commit_import", &json!({"id": id, "track": track})).is_err(), "a commit takes the item");
    e.undo();
    assert!(clips(&e, track).is_empty() && e.session().sources.is_empty(), "one undo takes the import back");
}

#[test]
fn cancel_leaves_no_media_and_no_document_change() {
    let d = Dir::new("cancel");
    let (mut e, track) = engine_with_track();
    let path = d.file("long.wav", &wav(2, 44_100, 44_100 * 20));
    let rev = e.revision;
    let undo = e.undo_history().len();
    let id = prepare(&mut e, &path);
    assert_eq!(e.execute("file.import_cancel", &json!({"id": id})).unwrap()["canceled"], true);
    assert_eq!(e.execute("file.import_cancel", &json!({"id": id})).unwrap()["canceled"], false);
    assert!(e.execute("file.import_progress", &json!({"id": id})).is_err(), "the job is gone");
    assert!(e.execute("file.commit_import", &json!({"id": id, "track": track})).is_err());
    // The worker stops at its next step and drops what it made.
    std::thread::sleep(Duration::from_millis(200));
    assert!(e.imports.is_empty());
    assert_eq!((e.revision, e.undo_history().len(), e.session().sources.len()), (rev, undo, 0));
    assert!(clips(&e, track).is_empty());
}

#[test]
fn commit_after_session_new_is_refused() {
    let d = Dir::new("new");
    let (mut e, _) = engine_with_track();
    let path = d.file("tone.wav", &wav(1, 48_000, 4_800));
    let id = prepare(&mut e, &path);
    wait(&mut e, id);
    e.execute("session.new", &json!({})).unwrap();
    assert!(e.imports.is_empty(), "a new session drops the staging map");
    let t = e.execute("track.new", &json!({"kind": "audio"})).unwrap()["tracks"][0].as_u64().unwrap();
    let err = e.execute("file.commit_import", &json!({"id": id, "track": t})).unwrap_err().to_string();
    assert!(err.contains("no import"), "{err}");
    assert!(e.session().sources.is_empty() && clips(&e, t).is_empty());
}

#[test]
fn a_corrupt_file_reports_an_error_not_a_panic() {
    let d = Dir::new("corrupt");
    let (mut e, track) = engine_with_track();
    let mut bytes = wav(2, 48_000, 4_800);
    bytes.truncate(30);
    let path = d.file("broken.wav", &bytes);
    let junk = d.file("junk.flac", b"fLaC but not really");
    for p in [path, junk] {
        let id = prepare(&mut e, &p);
        let (v, _) = wait(&mut e, id);
        assert_eq!(v["state"], "failed", "{v}");
        assert!(!v["error"].as_str().unwrap_or("").is_empty(), "{v}");
        assert!(e.execute("file.commit_import", &json!({"id": id, "track": track})).is_err());
        assert!(e.execute("file.import_cancel", &json!({"id": id})).unwrap()["canceled"].as_bool().unwrap());
    }
    let missing = d.0.join("nope.wav").to_string_lossy().into_owned();
    let id = prepare(&mut e, &missing);
    assert_eq!(wait(&mut e, id).0["state"], "failed");
    assert!(e.session().sources.is_empty());
}

#[test]
fn resampling_44_1_to_48_gives_the_expected_frame_count() {
    let d = Dir::new("rate");
    let (mut e, track) = engine_with_track();
    assert_eq!(e.session().sample_rate.hz(), 48_000);
    let path = d.file("kick.wav", &wav(1, 44_100, 22_050));
    let id = prepare(&mut e, &path);
    let (v, _) = wait(&mut e, id);
    assert_eq!(v["frames"], 24_000, "{v}");
    e.execute("file.commit_import", &json!({"id": id, "track": track, "at": 0})).unwrap();
    assert_eq!(clips(&e, track), vec![(0, 24_000, "kick".to_string())]);
    let src = &e.session().sources[0];
    assert_eq!((src.frames, src.sample_rate), (24_000, 48_000));
    // The synchronous path gives the same result.
    let r = e.execute("file.import_audio", &json!({"path": path, "track": track, "at": 48_000})).unwrap();
    assert_eq!(r["imported"][0]["frames"], 24_000);
}

#[test]
fn more_files_than_workers_all_finish_and_a_missing_track_keeps_the_item() {
    let d = Dir::new("queue");
    let (mut e, track) = engine_with_track();
    let ids: Vec<u64> = (0..7).map(|i| d.file(&format!("f{i}.wav"), &wav(1, 48_000, 1_000 * (i + 1)))).map(|p| prepare(&mut e, &p)).collect();
    let mut at = 0;
    for (i, id) in ids.iter().enumerate() {
        let (v, _) = wait(&mut e, *id);
        assert_eq!(v["frames"], 1_000 * (i as u64 + 1));
        let r = e.execute("file.commit_import", &json!({"id": id, "track": track, "at": at})).unwrap();
        at += r["frames"].as_i64().unwrap();
    }
    assert_eq!(clips(&e, track).len(), 7);
    let id = prepare(&mut e, &d.file("late.wav", &wav(1, 48_000, 480)));
    wait(&mut e, id);
    e.execute("track.delete", &json!({"tracks": [track]})).unwrap();
    assert!(e.execute("file.commit_import", &json!({"id": id, "track": track})).is_err());
    assert_eq!(e.execute("file.import_progress", &json!({"id": id})).unwrap()["state"], "done", "a refused commit keeps the item");
    let r = e.execute("file.commit_import", &json!({"id": id, "name": "Late Kick"})).unwrap();
    let t = e.session().track(soundcraft_engine::model::TrackId(r["track"].as_u64().unwrap())).unwrap();
    assert_eq!(t.name, "Late Kick", "no track: a new one, named as asked");
}

#[test]
fn the_journaled_commit_replays_to_the_same_clip() {
    let d = Dir::new("replay");
    let (mut e, track) = engine_with_track();
    let path = d.file("tone.wav", &wav(2, 48_000, 24_000));
    let id = prepare(&mut e, &path);
    wait(&mut e, id);
    e.execute("file.commit_import", &json!({"id": id, "track": track, "at": 1_000})).unwrap();
    let (cmd, params) = e.journal.last().cloned().unwrap();
    let (mut f, track2) = engine_with_track();
    assert_eq!(track, track2);
    f.execute(&cmd, &params).unwrap();
    assert_eq!(clips(&f, track), clips(&e, track));
}

#[test]
fn finished_items_a_polling_caller_left_uncommitted_expire_past_the_count_limit() {
    let d = Dir::new("expire");
    let (mut e, _) = engine_with_track();
    let path = d.file("tone.wav", &wav(1, 48_000, 480));
    e.imports.set_limits(Duration::from_secs(600), 3, Duration::ZERO);
    let ids: Vec<u64> = (0..5).map(|_| prepare(&mut e, &path)).collect();
    let t0 = Instant::now();
    while e.imports.all().iter().any(|v| !matches!(v["state"].as_str(), Some("done" | "expired"))) {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(2));
    }
    let states: Vec<String> = e.imports.all().iter().map(|v| v["state"].as_str().unwrap().to_string()).collect();
    assert_eq!(states.iter().filter(|s| *s == "done").count(), 3, "{states:?}");
    assert_eq!(states.iter().filter(|s| *s == "expired").count(), 2, "{states:?}");
    assert_eq!(ids.len(), states.len(), "expired items keep their ids");
}

#[test]
fn a_caller_that_was_away_finds_all_its_items_when_it_comes_back() {
    let d = Dir::new("away");
    let (mut e, track) = engine_with_track();
    let path = d.file("tone.wav", &wav(1, 48_000, 480));
    e.imports.set_limits(Duration::from_secs(600), 3, Duration::from_millis(50));
    let ids: Vec<u64> = (0..5).map(|_| prepare(&mut e, &path)).collect();
    // A modal dialog on the UI thread: nobody polls while the workers finish.
    std::thread::sleep(Duration::from_millis(500));
    for (i, id) in ids.iter().enumerate() {
        wait(&mut e, *id);
        let r = e.execute("file.commit_import", &json!({"id": id, "track": track, "at": i as i64 * 480}));
        assert!(r.is_ok(), "item {i}: {r:?}");
    }
    assert_eq!(clips(&e, track).len(), 5);
}

#[test]
fn an_item_left_uncommitted_while_the_caller_polls_others_goes_to_expired() {
    let d = Dir::new("expired-state");
    let (mut e, _) = engine_with_track();
    let path = d.file("tone.wav", &wav(1, 48_000, 480));
    e.imports.set_limits(Duration::from_millis(100), 32, Duration::from_secs(30));
    let (a, b) = (prepare(&mut e, &path), prepare(&mut e, &path));
    wait(&mut e, a);
    wait(&mut e, b);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(400) {
        e.imports.progress(b);
        std::thread::sleep(Duration::from_millis(5));
    }
    let v = e.imports.progress(a).expect("the id is kept");
    assert_eq!(v["state"], "expired", "{v}");
    assert!(v["error"].as_str().unwrap_or("").contains("expired"), "{v}");
    assert!(e.execute("file.commit_import", &json!({"id": a})).unwrap_err().to_string().contains("expired"));
    assert_eq!(e.imports.progress(b).unwrap()["state"], "done", "the polled item stays");
    assert!(e.execute("file.import_cancel", &json!({"id": a})).unwrap()["canceled"].as_bool().unwrap());
}

#[test]
fn name_with_several_paths_is_refused() {
    let d = Dir::new("name-many");
    let (mut e, _) = engine_with_track();
    let (a, b) = (d.file("a.wav", &wav(1, 48_000, 480)), d.file("b.wav", &wav(1, 48_000, 480)));
    let err = e.execute("file.import_audio", &json!({"paths": [a, b], "name": "Same"})).unwrap_err().to_string();
    assert!(err.contains("name") && err.contains("one"), "{err}");
    assert!(e.session().sources.is_empty());
    assert!(e.execute("file.import_audio", &json!({"paths": [a], "name": "Solo"})).is_ok());
}

#[test]
fn a_commit_after_a_session_rate_change_asks_for_a_re_import() {
    let d = Dir::new("rate-change");
    let (mut e, track) = engine_with_track();
    let path = d.file("kick.wav", &wav(1, 44_100, 22_050));
    let id = prepare(&mut e, &path);
    wait(&mut e, id);
    e.session_mut().sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
    let err = e.execute("file.commit_import", &json!({"id": id, "track": track})).unwrap_err().to_string();
    assert!(err.contains("prepared at 48000 Hz; the session is now 44100 Hz: re-import"), "{err}");
    assert!(e.session().sources.is_empty() && clips(&e, track).is_empty());
    assert_eq!(e.execute("file.import_progress", &json!({"id": id})).unwrap()["state"], "done", "the item stays");
}

#[test]
fn items_the_caller_keeps_polling_are_never_dropped_for_the_count_limit() {
    let d = Dir::new("keep");
    let (mut e, track) = engine_with_track();
    let path = d.file("tone.wav", &wav(1, 48_000, 480));
    e.imports.set_limits(Duration::from_secs(600), 3, Duration::from_secs(30));
    let ids: Vec<u64> = (0..6).map(|_| prepare(&mut e, &path)).collect();
    let t0 = Instant::now();
    while e.imports.all().iter().any(|v| v["state"] != "done") {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(2));
    }
    for (i, id) in ids.iter().enumerate() {
        assert!(e.execute("file.commit_import", &json!({"id": id, "track": track, "at": i as i64 * 480})).is_ok(), "item {i} committed");
    }
    assert_eq!(clips(&e, track).len(), 6);
}

#[test]
fn a_commit_without_a_track_replays_on_a_fresh_engine_with_the_same_track_clip_and_names() {
    let d = Dir::new("replay-new-track");
    let mut e = Engine::default();
    let path = d.file("tone.wav", &wav(2, 48_000, 24_000));
    let id = prepare(&mut e, &path);
    wait(&mut e, id);
    let r = e.execute("file.commit_import", &json!({"id": id, "at": 480, "name": "Lead Vox"})).unwrap();
    let (cmd, params) = e.journal.last().cloned().unwrap();
    assert_eq!(cmd, "file.import_audio");
    assert!(params.get("track").is_none(), "{params}");
    let mut f = Engine::default();
    f.execute(&cmd, &params).unwrap();
    let names = |x: &Engine| x.session().tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&f), names(&e));
    assert_eq!(names(&f), vec!["Lead Vox".to_string()]);
    let tid = r["track"].as_u64().unwrap();
    let ftid = f.session().tracks[0].id.0;
    assert_eq!(clips(&f, ftid), clips(&e, tid));
    assert_eq!(clips(&f, ftid), vec![(480, 24_000, "Lead Vox".to_string())]);
}

#[test]
fn a_caller_away_longer_than_the_limit_loses_nothing_when_it_returns() {
    let d = Dir::new("away-reset");
    let (mut e, _) = engine_with_track();
    let path = d.file("tone.wav", &wav(1, 48_000, 480));
    e.imports.set_limits(Duration::from_millis(100), 32, Duration::from_secs(30));
    let (a, b) = (prepare(&mut e, &path), prepare(&mut e, &path));
    wait(&mut e, a);
    wait(&mut e, b);
    e.imports.all();
    // Away for longer than the engine's `AWAY` (5 s), then back, polling only the first item.
    std::thread::sleep(Duration::from_millis(5_300));
    e.imports.progress(a);
    e.imports.progress(a);
    assert_eq!(e.imports.progress(b).unwrap()["state"], "done", "the idle clocks restarted on return");
}
