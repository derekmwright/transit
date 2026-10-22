//! Recording file planning and take placement.

use super::*;

/// The record-enabled audio tracks, each with the input channels it records (those its input
/// route names, "In 3" → 2, wrapped to the device's `input_channels`) and a new take file
/// `<track>_<NN>.wav` in the `Audio Files` of the session's folder (`unsaved_dir` until it is saved).
pub fn plan_takes(e: &Engine, unsaved_dir: &Path, input_channels: usize) -> Vec<(TrackId, Vec<usize>, PathBuf)> {
    let dir = e.path.as_deref().and_then(|p| Path::new(p).parent()).unwrap_or(unsaved_dir);
    let s = e.session();
    let take_no = s.sources.len() + 1;
    s.tracks
        .iter()
        .filter(|t| t.mixer.record_arm && t.kind == TrackKind::Audio && !t.inactive)
        .map(|t| {
            // The same resolution as the mix's input monitoring: a path by its name in this
            // machine's table, else the number in the label.
            let inputs = (0..t.channels())
                .map(|k| {
                    let channel = match &t.mixer.input {
                        soundcraft_model::Route::Hardware(h) => soundcraft_model::input_channel(Some(e.io_map()), h, k),
                        _ => k,
                    };
                    channel % input_channels.max(1)
                })
                .collect();
            let file = |n: usize| dir.join("Audio Files").join(format!("{}_{n:02}.wav", sanitize_name(&t.name)));
            let path = (take_no..take_no + 1_000).map(file).find(|p| !p.exists()).unwrap_or_else(|| file(take_no));
            (t.id, inputs, path)
        })
        .collect()
}

/// A recording's plan, made when it starts: each record-enabled track's take file, where the
/// takes start on the timeline, the loop length they split into when loop recording, and the
/// punch range they're trimmed to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Recording {
    pub start: Samples,
    pub pass: Option<Samples>,
    pub punch: Option<Range>,
    /// The input's sample rate, which the take files are written at.
    pub rate: u32,
    pub takes: Vec<Take>,
    /// The MIDI take journal of the pass, if MIDI is being recorded (crash recovery rebuilds the
    /// clip from it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_journal: Option<PathBuf>,
    /// The pass's MIDI clips are already in the session this plan was saved with, so recovery
    /// discards the journal instead of rebuilding them (set once Stop has placed the clips).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub midi_placed: bool,
}

/// One track's take file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Take {
    pub track: TrackId,
    pub path: PathBuf,
    pub channels: usize,
}

/// Place recorded take files as `plan` says, as one undo step ("Record").
pub fn place_recording(e: &mut Engine, plan: &Recording, takes: Vec<(TrackId, PathBuf)>) -> Vec<soundcraft_model::ClipId> {
    let ids = add_recording(e, plan.start, takes, plan.pass);
    if let Some(p) = plan.punch {
        let clips: Vec<u64> = ids.iter().map(|c| c.0).collect();
        let _ = e.execute_merged("edit.trim_to_fill_selection", &json!({"clips": clips, "start": p.start, "end": p.end}), "record");
    }
    ids
}

/// `place_recording` that is always exactly one undo step labeled "Record", whatever it does:
/// with `new_take`, a track that already has clips under the take (within the punch range)
/// first gets a duplicate of its playlist, so the old clips stay as the previous take; the punch
/// trim is folded in too. Loop recordings never duplicate.
pub fn place_recording_take(e: &mut Engine, plan: &Recording, takes: Vec<(TrackId, PathBuf)>, new_take: bool) -> Vec<soundcraft_model::ClipId> {
    let ids = add_recording_take(e, plan.start, takes, plan.pass, new_take, plan.punch);
    let n0 = e.undo_pushes();
    if !ids.is_empty() {
        if let Some(p) = plan.punch {
            let clips: Vec<u64> = ids.iter().map(|c| c.0).collect();
            let _ = e.execute("edit.trim_to_fill_selection", &json!({"clips": clips, "start": p.start, "end": p.end}));
        }
        // The add above is already one "Record" step; fold in the trim pushed since.
        let added = usize::try_from(e.undo_pushes() - n0).unwrap_or(0);
        if added > 0 {
            let _ = e.squash_undo(added + 1, "Record");
        }
    }
    ids
}

/// Turn recorded take files into clips at `start`, as one undo step ("Record"); each file is the
/// source of its clips. With `pass` (loop recording), a take becomes one clip per pass (at most
/// 1000), each pass after the first on a new playlist.
pub fn add_recording(e: &mut Engine, start: Samples, takes: Vec<(TrackId, PathBuf)>, pass: Option<Samples>) -> Vec<soundcraft_model::ClipId> {
    add_recording_take(e, start, takes, pass, false, None)
}

/// `add_recording` with `new_take`: a track with clips under the take (clipped to `punch`) gets a
/// duplicate of its active playlist first, inside the same "Record" step.
pub fn add_recording_take(
    e: &mut Engine,
    start: Samples,
    takes: Vec<(TrackId, PathBuf)>,
    pass: Option<Samples>,
    new_take: bool,
    punch: Option<Range>,
) -> Vec<soundcraft_model::ClipId> {
    let before = e.session_arc();
    let n0 = e.undo_pushes();
    let mut out = Vec::new();
    for (tid, path) in takes {
        let read =
            read_file(&path).and_then(|b| soundcraft_audio_io::decode(&b, None).map_err(|err| EngineError::Io(format!("{}: {err}", path.display()))));
        let Ok((_, buf)) = read.inspect_err(|err| e.message(err.to_string())) else { continue };
        if new_take && pass.is_none() {
            let frames = i64::try_from(buf.frames()).unwrap_or(0);
            let from = punch.map_or(start, |p| p.start.max(start));
            let to = punch.map_or(start + frames, |p| p.end.min(start + frames));
            let over = to > from && e.session().track(tid).is_some_and(|t| t.clips().iter().any(|c| c.start < to && c.end() > from));
            if over && let Err(err) = e.execute("track.playlist_duplicate", &json!({"track": tid.0})) {
                e.message(format!("Take kept at {} but not placed: {err}", path.display()));
                continue;
            }
        }
        let s = e.session_mut();
        if s.track(tid).is_none() {
            continue;
        }
        let resampled = buf.sample_rate != s.sample_rate.hz();
        let name = path.file_stem().and_then(|x| x.to_str()).unwrap_or("Take");
        let src = add_source(s, name, buf, Some(&path.to_string_lossy()), FileFormat::Wav);
        let Some(x) = s.sources.last_mut() else { continue };
        x.unsaved = resampled;
        let len = i64::try_from(x.frames).unwrap_or(0);
        let pass = pass.unwrap_or(len).max(1);
        for offset in (0..len).step_by(usize::try_from(pass).unwrap_or(usize::MAX)).take(1_000) {
            let n = pass.min(len - offset);
            if offset > 0 && n < pass / 8 {
                break;
            }
            if offset > 0
                && let Some(t) = s.track_mut(tid)
            {
                t.playlists.push(soundcraft_model::Playlist::new(format!("{}.{:02}", t.name, t.playlists.len() + 1)));
                t.active_playlist = t.playlists.len() - 1;
            }
            let cid = s.new_clip_id();
            crate::edit::place_clip(s, tid, Clip::audio(cid, name, src, offset, start, n));
            out.push(cid);
        }
    }
    if !out.is_empty() {
        e.push_undo("Record", before);
        let added = usize::try_from(e.undo_pushes() - n0).unwrap_or(0);
        if new_take && added > 1 {
            let _ = e.squash_undo(added, "Record");
        }
    }
    out
}

/// Where a take recording from `start` shows while it holds `len` samples, as it will be placed:
/// just the pass under way when loop recording in passes of `pass`, within the `punch` range.
/// Returns the timeline position of its first sample and the span it covers.
pub fn live_span(start: Samples, pass: Option<Samples>, punch: Option<Range>, len: Samples) -> Option<(Samples, Range)> {
    let origin = start.saturating_sub(pass.filter(|p| *p > 0).map_or(0, |p| len / p * p));
    let span = Range::new(start, origin.saturating_add(len));
    Some((origin, span.intersect(&punch.unwrap_or(span))?))
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_failed_playlist_duplicate_keeps_the_file_and_old_clips_issue_10() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let source =
            add_source(e.session_mut(), "Original", AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 480]] }, None, FileFormat::Wav);
        let clip = e.session_mut().new_clip_id();
        crate::edit::place_clip(e.session_mut(), t, Clip::audio(clip, "Original", source, 0, 0, 480));
        let tr = e.session_mut().track_mut(t).unwrap();
        tr.freeze = Some(soundcraft_model::Frozen { source, start: 0, length: 480, hash: tr.freeze_hash(), revision: 1, latency: 0 });
        let before = e.session_arc();
        let path = std::env::temp_dir().join(format!("failed-playlist-{}.wav", std::process::id()));
        write_take(&path, 480);
        assert!(add_recording_take(&mut e, 0, vec![(t, path.clone())], None, true, None).is_empty());
        assert_eq!(e.session(), before.as_ref());
        assert!(path.is_file());
        assert!(e.messages.last().is_some_and(|m| m.contains("not placed") && m.contains(path.to_string_lossy().as_ref())));
        assert!(e.undo_history().is_empty());
        std::fs::remove_file(path).unwrap();
    }

    fn armed(e: &mut Engine, name: &str, format: &str, input: &str) -> TrackId {
        let id = e.execute("track.new", &json!({"name": name, "format": format})).unwrap()["tracks"][0].as_u64().unwrap();
        e.execute("track.input", &json!({"tracks": [id], "input": input})).unwrap();
        e.execute("mix.record_arm", &json!({"tracks": [id], "value": true})).unwrap();
        TrackId(id)
    }

    fn write_take(path: &Path, frames: usize) {
        let mut bytes = soundcraft_audio_io::wav_float_header(1, 48_000, frames as u64).unwrap();
        bytes.extend((0..frames).flat_map(|i| (i as f32 / frames as f32 - 0.5).to_le_bytes()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn an_input_path_by_name_records_its_channel_and_old_labels_keep_their_meaning() {
        use soundcraft_model::{ChannelFormat, IoDirection, IoMap, IoPath};
        let dir = std::env::temp_dir().join(format!("soundcraft-takes-path-{}", std::process::id()));
        let mut e = Engine::default();
        let mut io = IoMap::for_device("ASIO", "x", 8, 2);
        io.add_path(IoPath { name: "Guitar".into(), direction: IoDirection::Input, channels: vec![4], format: ChannelFormat::Mono }).unwrap();
        io.add_path(IoPath { name: "Drum pair".into(), direction: IoDirection::Input, channels: vec![6, 7], format: ChannelFormat::Stereo }).unwrap();
        e.set_io_map(io);
        let gtr = armed(&mut e, "Gtr", "mono", "Guitar");
        let drums = armed(&mut e, "Drums", "stereo", "Drum pair");
        let old = armed(&mut e, "Old", "stereo", "In 3-4");
        let plan = plan_takes(&e, &dir, 8);
        let channels = |t: TrackId| plan.iter().find(|p| p.0 == t).map(|p| p.1.clone()).unwrap();
        assert_eq!(channels(gtr), [4], "Guitar is input channel 5");
        assert_eq!(channels(drums), [6, 7]);
        assert_eq!(channels(old), [2, 3]);
        // The same names, resolved by the mix: one function (`soundcraft_model::input_channel`).
        assert_eq!(soundcraft_model::input_channel(Some(e.io_map()), "Guitar", 0), 4);
    }

    #[test]
    fn armed_tracks_get_new_take_files_used_in_place() {
        let dir = std::env::temp_dir().join(format!("soundcraft-takes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::default();
        let vox = armed(&mut e, "Vox", "mono", "In 2");
        let gtr = armed(&mut e, "Gtr", "stereo", "In 3-4");
        write_take(&dir.join("Audio Files/Vox_01.wav"), 10);
        let plan = plan_takes(&e, &dir, 4);
        assert_eq!(plan, vec![(vox, vec![1], dir.join("Audio Files/Vox_02.wav")), (gtr, vec![2, 3], dir.join("Audio Files/Gtr_01.wav"))]);
        assert_eq!(plan_takes(&e, &dir, 2)[1].1, vec![0, 1]);

        let session = dir.join("Song.transit").to_string_lossy().into_owned();
        e.execute("session.save_as", &json!({"path": session})).unwrap();
        write_take(&plan[0].2, 4_800);
        let clips = add_recording(&mut e, 96_000, vec![(vox, plan[0].2.clone())], None);
        let clip = e.session().find_clip(clips[0]).unwrap().1.clone();
        assert_eq!((clip.start, clip.length), (96_000, 4_800));
        assert_eq!(save_session(&mut e, &session).unwrap(), 0);
        let src = e.session().sources.last().unwrap().clone();
        assert_eq!((src.name.as_str(), src.path.as_str(), src.unsaved, src.frames), ("Vox_02", "Audio Files/Vox_02.wav", false, 4_800));
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(e.session().find_clip(clips[0]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_loop_take_becomes_one_clip_per_pass_and_autosaves_in_place() {
        let dir = std::env::temp_dir().join(format!("soundcraft-loop-take-{}", std::process::id()));
        let path = dir.join("Audio Files/Vox_01.wav");
        write_take(&path, 1_050);
        let mut e = Engine::default();
        let vox = armed(&mut e, "Vox", "mono", "In 1");
        assert_eq!(add_recording(&mut e, 1_000, vec![(vox, path.clone())], Some(400)).len(), 3);
        assert_eq!(save_copy(&e, &dir.join("Untitled.transit").to_string_lossy()).unwrap(), 0);
        let t = e.session().track(vox).unwrap();
        let passes: Vec<_> = t.playlists.iter().flat_map(|p| p.clips.iter().map(|c| (c.source_offset(), c.start, c.length))).collect();
        assert_eq!(passes, vec![(0, 1_000, 400), (400, 1_000, 400), (800, 1_000, 250)]);
        assert_eq!((t.active_playlist, t.playlists[2].name.as_str()), (2, "Vox.03"));
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(e.session().track(vox).unwrap().playlists.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn over_a_clip(tag: &str) -> (Engine, TrackId, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("soundcraft-newtake-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (first, second) = (dir.join("Audio Files/Vox_01.wav"), dir.join("Audio Files/Vox_02.wav"));
        write_take(&first, 4_800);
        write_take(&second, 4_800);
        let mut e = Engine::default();
        let vox = armed(&mut e, "Vox", "mono", "In 1");
        add_recording(&mut e, 1_000, vec![(vox, first.clone())], None);
        (e, vox, first, second)
    }

    #[test]
    fn new_take_recording_over_a_clip_is_one_record_step() {
        let (mut e, vox, _, second) = over_a_clip("one");
        let before = e.undo_history();
        let plan = Recording { start: 2_000, pass: None, punch: None, rate: 48_000, takes: Vec::new(), midi_journal: None, midi_placed: false };
        let ids = place_recording_take(&mut e, &plan, vec![(vox, second)], true);
        assert_eq!(ids.len(), 1);
        let h = e.undo_history();
        assert_eq!((h.len(), h.last().map(String::as_str)), (before.len() + 1, Some("Record")));
        assert_eq!(e.session().track(vox).unwrap().playlists.len(), 2);
        e.execute("edit.undo", &json!({})).unwrap();
        let t = e.session().track(vox).unwrap();
        assert_eq!((t.playlists.len(), t.clips().len(), t.clips()[0].start), (1, 1, 1_000));
        e.execute("edit.redo", &json!({})).unwrap();
        assert_eq!(e.session().track(vox).unwrap().playlists.len(), 2);
    }

    #[test]
    fn new_take_with_punch_folds_the_trim_into_the_record_step() {
        let (mut e, vox, _, second) = over_a_clip("punch");
        let n = e.undo_history().len();
        let plan = Recording {
            start: 1_000,
            pass: None,
            punch: Some(Range::new(2_000, 3_000)),
            rate: 48_000,
            takes: Vec::new(),
            midi_journal: None,
            midi_placed: false,
        };
        let ids = place_recording_take(&mut e, &plan, vec![(vox, second)], true);
        let h = e.undo_history();
        assert_eq!((h.len(), h.last().map(String::as_str)), (n + 1, Some("Record")));
        let c = e.session().find_clip(ids[0]).unwrap().1.clone();
        assert_eq!((c.start, c.length), (2_000, 1_000));
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(e.session().track(vox).unwrap().playlists.len(), 1);
    }

    #[test]
    fn new_take_fold_still_works_when_the_history_is_full() {
        let (mut e, vox, _, second) = over_a_clip("full");
        e.history_limit = e.undo_history().len();
        let last = e.undo_history().pop();
        let plan = Recording { start: 2_000, pass: None, punch: None, rate: 48_000, takes: Vec::new(), midi_journal: None, midi_placed: false };
        place_recording_take(&mut e, &plan, vec![(vox, second)], true);
        let h = e.undo_history();
        // One new entry on top of the previous newest; the duplicate step is folded, not left behind.
        assert_eq!(h[h.len() - 2..], [last.unwrap(), "Record".to_string()]);
        assert!(!h.iter().any(|l| l == "Duplicate Playlist"));
        e.execute("edit.undo", &json!({})).unwrap();
        let t = e.session().track(vox).unwrap();
        assert_eq!((t.playlists.len(), t.clips().len(), t.clips()[0].start), (1, 1, 1_000));
    }

    #[test]
    fn new_take_across_two_tracks_with_clips_is_one_step() {
        let (mut e, vox, _, second) = over_a_clip("two");
        let gtr = armed(&mut e, "Gtr", "mono", "In 2");
        add_recording(&mut e, 1_000, vec![(gtr, second.clone())], None);
        let n = e.undo_history().len();
        let plan = Recording { start: 2_000, pass: None, punch: None, rate: 48_000, takes: Vec::new(), midi_journal: None, midi_placed: false };
        place_recording_take(&mut e, &plan, vec![(vox, second.clone()), (gtr, second)], true);
        let h = e.undo_history();
        assert_eq!((h.len(), h.last().map(String::as_str)), (n + 1, Some("Record")));
        assert_eq!([vox, gtr].map(|t| e.session().track(t).unwrap().playlists.len()), [2, 2]);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!([vox, gtr].map(|t| e.session().track(t).unwrap().playlists.len()), [1, 1]);
    }

    #[test]
    fn new_take_into_empty_space_does_not_duplicate_and_is_one_step() {
        let (mut e, vox, _, second) = over_a_clip("empty");
        let n = e.undo_history().len();
        let plan = Recording { start: 20_000, pass: None, punch: None, rate: 48_000, takes: Vec::new(), midi_journal: None, midi_placed: false };
        place_recording_take(&mut e, &plan, vec![(vox, second)], true);
        let h = e.undo_history();
        assert_eq!((h.len(), h.last().map(String::as_str)), (n + 1, Some("Record")));
        assert_eq!(e.session().track(vox).unwrap().playlists.len(), 1);
    }

    #[test]
    fn recording_without_new_take_keeps_the_old_steps() {
        let (mut e, vox, _, second) = over_a_clip("old");
        let n = e.undo_history().len();
        let plan = Recording {
            start: 1_000,
            pass: None,
            punch: Some(Range::new(2_000, 3_000)),
            rate: 48_000,
            takes: Vec::new(),
            midi_journal: None,
            midi_placed: false,
        };
        place_recording(&mut e, &plan, vec![(vox, second)]);
        assert_eq!(e.undo_history().len(), n + 2);
    }

    #[test]
    fn a_take_shows_the_pass_under_way_within_the_punch_while_it_records() {
        assert_eq!(live_span(1_000, None, None, 0), None);
        assert_eq!(live_span(1_000, None, None, 500), Some((1_000, Range::new(1_000, 1_500))));
        assert_eq!(live_span(1_000, Some(400), None, 900), Some((200, Range::new(1_000, 1_100))));
        assert_eq!(live_span(1_000, Some(400), None, 800), None);
        let punch = Some(Range::new(1_200, 1_400));
        assert_eq!(live_span(1_000, None, punch, 150), None);
        assert_eq!(live_span(1_000, None, punch, 500), Some((1_000, Range::new(1_200, 1_400))));
    }
}
