//! MIDI recording takes: live events captured during a recording pass become MIDI clips.
//!
//! The captured events arrive with the samples since the record-start latch (the same origin the
//! audio take is placed from), so a MIDI take lines up with an audio take of the same pass. This
//! module pairs note-ons with note-offs, converts to ticks through the session's tempo map, and
//! places the result as one undo step ("Record"):
//!
//! - **Overdub:** if exactly one MIDI clip already covers the recorded range on the track, the
//!   new notes are merged into it (Logic's default).
//! - **Several clips in the way:** the track's playlist is duplicated first, so the old clips stay
//!   as the previous take, as audio recording does.
//! - **Loop record:** each pass is its own clip on its own playlist (a take lane), as audio does.
//! - **Nothing played:** no clip.

use crate::Engine;
use crate::io::Recording;
use serde_json::json;
use soundcraft_midi::{CtrlEvent, CtrlKind, Note, Sequence};
use soundcraft_model::{Clip, ClipContent, ClipId, TrackId};
use soundcraft_time::{Range, SampleRate, Samples, TempoMap};

/// The most loop passes one recording turns into clips (as for audio takes).
const MAX_PASSES: i64 = 1_000;

/// One captured channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Msg {
    NoteOn {
        pitch: u8,
        velocity: u8,
    },
    NoteOff {
        pitch: u8,
        velocity: u8,
    },
    Cc {
        cc: u8,
        value: u8,
    },
    /// 0..=16383, 8192 is the center.
    PitchBend {
        value: u16,
    },
    ChannelPressure {
        value: u8,
    },
    PolyPressure {
        pitch: u8,
        value: u8,
    },
    Program {
        program: u8,
    },
}

/// A captured event: where it happened, in session samples since the record-start latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecEvent {
    pub track: TrackId,
    pub channel: u8,
    pub offset: Samples,
    pub msg: Msg,
}

/// What one recording pass is cut to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    /// Session sample of the latch (the take's start; the loop start when loop recording).
    pub start: Samples,
    /// Loop length when loop recording: each pass is a take.
    pub pass: Option<Samples>,
    /// The range a punch keeps.
    pub punch: Option<Range>,
    /// Samples from the latch to Stop. Notes still held then end here (ignored when looping,
    /// where a pass ends at its length).
    pub stop: Samples,
}

/// One clip's worth of recorded material.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub track: TrackId,
    /// 0 for the first (or only) pass.
    pub pass: usize,
    /// Where the clip lies on the timeline.
    pub range: Range,
    /// Ticks relative to the clip start.
    pub sequence: Sequence,
}

/// Pair and convert `events` into per-track, per-pass sequences. Events outside the cut are
/// dropped, a note released after its pass is closed at the pass end, and a note still held at
/// Stop ends at the stop sample. Passes (or tracks) with no notes produce nothing.
pub fn build(tempo: &TempoMap, sr: SampleRate, events: &[RecEvent], cut: &Cut) -> Vec<Built> {
    let mut tracks: Vec<TrackId> = Vec::new();
    for e in events {
        if !tracks.contains(&e.track) {
            tracks.push(e.track);
        }
    }
    let mut out = Vec::new();
    for track in tracks {
        let mut mine: Vec<&RecEvent> = events.iter().filter(|e| e.track == track && e.offset >= 0).collect();
        mine.sort_by_key(|e| e.offset);
        let pass_len = cut.pass.filter(|p| *p > 0);
        let passes = match pass_len {
            Some(len) => mine.last().map_or(0, |e| (e.offset / len + 1).min(MAX_PASSES)),
            None => 1,
        };
        for k in 0..passes {
            let (window, range) = match pass_len {
                Some(len) => (Range::new(k * len, (k + 1) * len), Range::new(cut.start, cut.start.saturating_add(len))),
                None => {
                    let end = cut.start.saturating_add(cut.stop.max(0));
                    let range = cut.punch.map_or(Range::new(cut.start, end), |p| Range::new(p.start.max(cut.start), p.end.min(end)));
                    (Range::new(0, cut.stop.max(0).saturating_add(1)), range)
                }
            };
            if range.is_empty() {
                continue;
            }
            let at = |offset: Samples| match pass_len {
                Some(_) => cut.start.saturating_add(offset - window.start),
                None => cut.start.saturating_add(offset),
            };
            let base = tempo.samples_to_ticks(range.start, sr);
            let tick = |sample: Samples| tempo.samples_to_ticks(sample.clamp(range.start, range.end), sr) - base;
            let mut sequence = Sequence::default();
            // Held notes: (channel, pitch, start sample, velocity); the earliest matching on wins.
            let mut held: Vec<(u8, u8, Samples, u8)> = Vec::new();
            let close = |sequence: &mut Sequence, held_note: (u8, u8, Samples, u8), end: Samples, release: u8| {
                let (channel, pitch, started, velocity) = held_note;
                let (a, b) = (tick(started), tick(end));
                sequence.notes.push(Note { pitch, velocity, release_velocity: release, channel, start: a, length: (b - a).max(1) });
            };
            for e in mine.iter().filter(|e| window.contains(e.offset)) {
                let sample = at(e.offset);
                if !range.contains(sample) && !matches!(e.msg, Msg::NoteOff { .. }) {
                    continue;
                }
                match e.msg {
                    Msg::NoteOn { pitch, velocity } => held.push((e.channel, pitch, sample, velocity.clamp(1, 127))),
                    Msg::NoteOff { pitch, velocity } => {
                        if let Some(i) = held.iter().position(|h| h.0 == e.channel && h.1 == pitch) {
                            let note = held.remove(i);
                            close(&mut sequence, note, sample.min(range.end), if velocity == 0 { 64 } else { velocity.min(127) });
                        }
                    }
                    Msg::Cc { cc, value } if cc < 120 => {
                        sequence.ctrls.push(CtrlEvent { tick: tick(sample), channel: e.channel, kind: CtrlKind::Cc(cc), value: i32::from(value) });
                    }
                    Msg::Cc { .. } => {}
                    Msg::PitchBend { value } => {
                        sequence.ctrls.push(CtrlEvent {
                            tick: tick(sample),
                            channel: e.channel,
                            kind: CtrlKind::PitchBend,
                            value: i32::from(value.min(16383)) - 8192,
                        });
                    }
                    Msg::ChannelPressure { value } => {
                        sequence.ctrls.push(CtrlEvent {
                            tick: tick(sample),
                            channel: e.channel,
                            kind: CtrlKind::ChannelPressure,
                            value: i32::from(value),
                        });
                    }
                    Msg::PolyPressure { pitch, value } => {
                        sequence.ctrls.push(CtrlEvent {
                            tick: tick(sample),
                            channel: e.channel,
                            kind: CtrlKind::PolyPressure(pitch),
                            value: i32::from(value),
                        });
                    }
                    Msg::Program { program } => {
                        sequence.ctrls.push(CtrlEvent { tick: tick(sample), channel: e.channel, kind: CtrlKind::Program, value: i32::from(program) });
                    }
                }
            }
            // Still held at Stop (or at the pass end): no note is left open.
            for note in held {
                close(&mut sequence, note, range.end, 64);
            }
            if sequence.notes.is_empty() {
                continue;
            }
            sequence.sort();
            out.push(Built { track, pass: usize::try_from(k).unwrap_or(0), range, sequence });
        }
    }
    out
}

/// Place a finished MIDI recording as one undo step ("Record") and return the clips it created
/// or merged into. `stop` is the samples from the latch to Stop.
pub fn place_midi_recording(e: &mut Engine, plan: &Recording, events: &[RecEvent], stop: Samples) -> Vec<ClipId> {
    let cut = Cut { start: plan.start, pass: plan.pass, punch: plan.punch, stop };
    let built = {
        let s = e.session();
        build(&s.tempo, s.sample_rate, events, &cut)
    };
    if built.is_empty() {
        return Vec::new();
    }
    let before = e.session_arc();
    let n0 = e.undo_pushes();
    let mut ids = Vec::new();
    let mut tracks: Vec<TrackId> = Vec::new();
    for b in &built {
        if !tracks.contains(&b.track) {
            tracks.push(b.track);
        }
    }
    for track in tracks {
        let mine: Vec<&Built> = built.iter().filter(|b| b.track == track).collect();
        let overlapping = |e: &Engine, range: Range| -> Vec<ClipId> {
            e.session()
                .track(track)
                .map(|t| {
                    t.clips()
                        .iter()
                        .filter(|c| matches!(c.content, ClipContent::Midi { .. }) && c.range().intersect(&range).is_some())
                        .map(|c| c.id)
                        .collect()
                })
                .unwrap_or_default()
        };
        let looped = plan.pass.is_some();
        let span = mine.iter().map(|b| b.range).reduce(|a, b| Range::new(a.start.min(b.start), a.end.max(b.end)));
        let Some(span) = span else { continue };
        let covered = overlapping(e, span);
        // One clip in the way merges (overdub); several, or a loop, keep the old material as the
        // previous take on a duplicated playlist.
        let merge_into = match (looped, covered.as_slice()) {
            (false, [only]) => Some(*only),
            _ => None,
        };
        if merge_into.is_none() && !covered.is_empty() {
            let _ = e.execute("track.playlist_duplicate", &json!({"track": track.0}));
        }
        for b in mine {
            if b.pass > 0
                && let Some(t) = e.session_mut().track_mut(track)
            {
                t.playlists.push(soundcraft_model::Playlist::new(format!("{}.{:02}", t.name, t.playlists.len() + 1)));
                t.active_playlist = t.playlists.len() - 1;
            }
            let s = e.session_mut();
            let (sr, tempo) = (s.sample_rate, s.tempo.clone());
            if let Some(id) = merge_into {
                merge(s, id, b, &tempo, sr);
                ids.push(id);
            } else {
                let cid = s.new_clip_id();
                let n = s.track(track).map_or(0, |t| t.playlists.iter().map(|p| p.clips.len()).sum::<usize>()) + 1;
                let name = format!("{}_{n:02}", s.track(track).map_or_else(|| "Take".to_string(), |t| crate::io::sanitize_name(&t.name)));
                crate::edit::place_clip(s, track, Clip::midi(cid, name, b.range.start, b.range.len(), b.sequence.clone()));
                ids.push(cid);
            }
        }
    }
    if !ids.is_empty() {
        e.push_undo("Record", before);
        let added = usize::try_from(e.undo_pushes() - n0).unwrap_or(0);
        if added > 1 {
            let _ = e.squash_undo(added, "Record");
        }
    }
    ids
}

/// Overdub: the new notes join the clip, which grows to cover the take (nothing else is in the
/// way: the caller found this the only clip in the range).
fn merge(s: &mut soundcraft_model::Session, id: ClipId, built: &Built, tempo: &TempoMap, sr: SampleRate) {
    let Some(clip) = s.find_clip_mut(id) else { return };
    let old_start = clip.start;
    let start = old_start.min(built.range.start);
    let end = clip.end().max(built.range.end);
    let (old_base, new_base) = (tempo.samples_to_ticks(old_start, sr), tempo.samples_to_ticks(built.range.start, sr));
    let ClipContent::Midi { sequence } = &mut clip.content else { return };
    // Existing material moves with the clip start; the new material is relative to its own start.
    let shift = (old_base - tempo.samples_to_ticks(start, sr)).max(0);
    for n in &mut sequence.notes {
        n.start += shift;
    }
    for c in &mut sequence.ctrls {
        c.tick += shift;
    }
    let delta = new_base - tempo.samples_to_ticks(start, sr);
    for n in &built.sequence.notes {
        sequence.notes.push(Note { start: n.start + delta, ..*n });
    }
    for c in &built.sequence.ctrls {
        sequence.ctrls.push(CtrlEvent { tick: c.tick + delta, ..*c });
    }
    sequence.sort();
    clip.start = start;
    clip.length = end - start;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;
    use soundcraft_time::TICKS_PER_QUARTER;

    const T: TrackId = TrackId(1);

    // The default session: 120 bpm, 4/4, 48 kHz, so a quarter note is 24,000 samples and
    // 960 ticks.
    fn ev(offset: Samples, msg: Msg) -> RecEvent {
        RecEvent { track: T, channel: 0, offset, msg }
    }
    fn on(offset: Samples, pitch: u8) -> RecEvent {
        ev(offset, Msg::NoteOn { pitch, velocity: 90 })
    }
    fn off(offset: Samples, pitch: u8) -> RecEvent {
        ev(offset, Msg::NoteOff { pitch, velocity: 0 })
    }
    fn tempo() -> (TempoMap, SampleRate) {
        let s = soundcraft_model::Session::default();
        (s.tempo.clone(), s.sample_rate)
    }
    fn cut(start: Samples, stop: Samples) -> Cut {
        Cut { start, pass: None, punch: None, stop }
    }

    #[test]
    fn notes_pair_and_convert_to_ticks_from_the_latch() {
        let (tempo, sr) = tempo();
        let events = [on(0, 60), off(24_000, 60), on(24_000, 64), off(60_000, 64)];
        let built = build(&tempo, sr, &events, &cut(96_000, 96_000));
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].range, Range::new(96_000, 192_000));
        let q = TICKS_PER_QUARTER;
        let notes: Vec<_> = built[0].sequence.notes.iter().map(|n| (n.pitch, n.start, n.length, n.velocity, n.release_velocity)).collect();
        assert_eq!(notes, vec![(60, 0, q, 90, 64), (64, q, q * 3 / 2, 90, 64)]);
    }

    #[test]
    fn a_note_held_at_stop_ends_at_the_stop_sample_and_a_stray_release_is_ignored() {
        let (tempo, sr) = tempo();
        let events = [off(100, 50), on(48_000, 60)];
        let built = build(&tempo, sr, &events, &cut(0, 72_000));
        let n = built[0].sequence.notes[0];
        assert_eq!((n.start, n.length), (TICKS_PER_QUARTER * 2, TICKS_PER_QUARTER));
        assert_eq!(built[0].sequence.notes.len(), 1);
    }

    #[test]
    fn controllers_are_kept_with_notes_and_nothing_played_makes_nothing() {
        let (tempo, sr) = tempo();
        let events = [
            ev(0, Msg::Cc { cc: 64, value: 127 }),
            on(12_000, 60),
            ev(18_000, Msg::PitchBend { value: 8192 + 100 }),
            ev(20_000, Msg::Cc { cc: 123, value: 0 }),
            off(24_000, 60),
            ev(24_000, Msg::Cc { cc: 64, value: 0 }),
        ];
        let built = build(&tempo, sr, &events, &cut(0, 48_000));
        let kinds: Vec<_> = built[0].sequence.ctrls.iter().map(|c| (c.tick, c.kind, c.value)).collect();
        assert_eq!(
            kinds,
            vec![(0, CtrlKind::Cc(64), 127), (TICKS_PER_QUARTER * 3 / 4, CtrlKind::PitchBend, 100), (TICKS_PER_QUARTER, CtrlKind::Cc(64), 0)]
        );
        assert!(build(&tempo, sr, &[ev(0, Msg::Cc { cc: 64, value: 127 })], &cut(0, 48_000)).is_empty(), "no notes, no clip");
        assert!(build(&tempo, sr, &[], &cut(0, 48_000)).is_empty());
    }

    #[test]
    fn loop_passes_split_by_the_loop_length_and_a_note_crossing_the_wrap_ends_at_it() {
        let (tempo, sr) = tempo();
        // Loop of two beats (48,000 samples) starting at 96,000; two passes.
        let events = [on(0, 60), off(24_000, 60), on(40_000, 62), off(60_000, 62), on(60_000, 64), off(72_000, 64)];
        let c = Cut { start: 96_000, pass: Some(48_000), punch: None, stop: 0 };
        let built = build(&tempo, sr, &events, &c);
        assert_eq!(
            built.iter().map(|b| (b.pass, b.range)).collect::<Vec<_>>(),
            vec![(0, Range::new(96_000, 144_000)), (1, Range::new(96_000, 144_000))]
        );
        let first: Vec<_> = built[0].sequence.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect();
        assert_eq!(
            first,
            vec![(60, 0, TICKS_PER_QUARTER), (62, TICKS_PER_QUARTER * 40_000 / 24_000, TICKS_PER_QUARTER * 2 - TICKS_PER_QUARTER * 40_000 / 24_000)]
        );
        let second: Vec<_> = built[1].sequence.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect();
        assert_eq!(second, vec![(64, TICKS_PER_QUARTER / 2, TICKS_PER_QUARTER / 2)]);
    }

    #[test]
    fn a_punch_keeps_only_its_range() {
        let (tempo, sr) = tempo();
        let events = [on(0, 60), off(24_000, 60), on(30_000, 62), off(40_000, 62), on(60_000, 64), off(66_000, 64)];
        let c = Cut { start: 0, pass: None, punch: Some(Range::new(24_000, 48_000)), stop: 72_000 };
        let built = build(&tempo, sr, &events, &c);
        assert_eq!(built[0].range, Range::new(24_000, 48_000));
        assert_eq!(built[0].sequence.notes.iter().map(|n| n.pitch).collect::<Vec<_>>(), vec![62]);
    }

    fn midi_engine() -> (Engine, TrackId) {
        let mut e = Engine::default();
        e.execute("track.new", &json!({"kind": "midi"})).unwrap();
        let t = e.session().tracks[0].id;
        (e, t)
    }
    fn plan(start: Samples, pass: Option<Samples>) -> Recording {
        Recording { start, pass, punch: None, rate: 48_000, takes: Vec::new(), midi_journal: None }
    }
    fn two_notes(t: TrackId) -> Vec<RecEvent> {
        let mut v = vec![on(0, 60), off(24_000, 60), on(24_000, 64), off(48_000, 64)];
        for e in &mut v {
            e.track = t;
        }
        v
    }
    fn notes_of(e: &Engine, id: ClipId) -> Vec<(u8, i64, i64)> {
        let clip = e.session().tracks.iter().flat_map(|t| t.playlists.iter()).flat_map(|p| p.clips.iter()).find(|c| c.id == id).unwrap();
        match &clip.content {
            ClipContent::Midi { sequence } => sequence.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect(),
            _ => panic!("not a MIDI clip"),
        }
    }

    #[test]
    fn two_played_notes_become_one_clip_in_one_undo_step_that_survives_undo_redo_and_save() {
        let (mut e, t) = midi_engine();
        let undo = e.undo_history().len();
        let ids = place_midi_recording(&mut e, &plan(96_000, None), &two_notes(t), 96_000);
        assert_eq!(ids.len(), 1);
        let clip = e.session().find_clip(ids[0]).unwrap().1.clone();
        assert_eq!((clip.start, clip.length), (96_000, 96_000));
        assert!(clip.name.ends_with("_01"), "{}", clip.name);
        let q = TICKS_PER_QUARTER;
        assert_eq!(notes_of(&e, ids[0]), vec![(60, 0, q), (64, q, q)]);
        assert_eq!(e.undo_history().len(), undo + 1);
        assert_eq!(e.undo_history().last().map(String::as_str), Some("Record"));
        // Saved and loaded again, the clip is the same.
        let json = serde_json::to_string(e.session()).unwrap();
        let loaded: soundcraft_model::Session = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.find_clip(ids[0]).unwrap().1, &clip);
        e.undo();
        assert!(e.session().track(t).unwrap().clips().is_empty(), "undo removes the clip");
        e.redo();
        assert_eq!(notes_of(&e, ids[0]).len(), 2);
    }

    #[test]
    fn nothing_played_places_nothing_and_adds_no_undo_step() {
        let (mut e, t) = midi_engine();
        let undo = e.undo_history().len();
        assert!(place_midi_recording(&mut e, &plan(0, None), &[], 96_000).is_empty());
        let pedal_only = [RecEvent { track: t, ..ev(0, Msg::Cc { cc: 64, value: 127 }) }];
        assert!(place_midi_recording(&mut e, &plan(0, None), &pedal_only, 96_000).is_empty());
        assert_eq!(e.undo_history().len(), undo);
        assert!(e.session().track(t).unwrap().clips().is_empty());
    }

    #[test]
    fn overdub_merges_into_the_clip_under_the_take_as_one_undo_step() {
        let (mut e, t) = midi_engine();
        let first = place_midi_recording(&mut e, &plan(0, None), &two_notes(t), 192_000)[0];
        let undo = e.undo_history().len();
        // Played again from the third beat, inside the same clip.
        let ids = place_midi_recording(&mut e, &plan(48_000, None), &two_notes(t), 96_000);
        assert_eq!(ids, vec![first]);
        assert_eq!(e.session().track(t).unwrap().clips().len(), 1);
        let q = TICKS_PER_QUARTER;
        assert_eq!(notes_of(&e, first), vec![(60, 0, q), (64, q, q), (60, 2 * q, q), (64, 3 * q, q)]);
        assert_eq!(e.undo_history().len(), undo + 1);
        e.undo();
        assert_eq!(notes_of(&e, first).len(), 2, "undo takes only the overdub back");
    }

    #[test]
    fn overdub_before_and_past_the_clip_grows_it_and_keeps_the_old_notes_in_place() {
        let (mut e, t) = midi_engine();
        let first = place_midi_recording(&mut e, &plan(48_000, None), &two_notes(t), 48_000)[0];
        // From one beat before the clip, running a beat past its end.
        let ids = place_midi_recording(&mut e, &plan(24_000, None), &two_notes(t), 96_000);
        assert_eq!(ids, vec![first]);
        let clip = e.session().find_clip(first).unwrap().1.clone();
        assert_eq!((clip.start, clip.end()), (24_000, 120_000));
        let q = TICKS_PER_QUARTER;
        // The old notes sit a beat later in the grown clip; the new ones start at its start.
        assert_eq!(notes_of(&e, first), vec![(60, 0, q), (60, q, q), (64, q, q), (64, 2 * q, q)]);
    }

    #[test]
    fn a_take_across_several_clips_keeps_them_as_the_previous_take_on_a_duplicated_playlist() {
        let (mut e, t) = midi_engine();
        let a = place_midi_recording(&mut e, &plan(0, None), &two_notes(t), 48_000)[0];
        let b = place_midi_recording(&mut e, &plan(96_000, None), &two_notes(t), 48_000)[0];
        let undo = e.undo_history().len();
        let ids = place_midi_recording(&mut e, &plan(0, None), &two_notes(t), 144_000);
        assert_eq!(ids.len(), 1);
        assert!(ids[0] != a && ids[0] != b);
        assert_eq!(e.undo_history().len(), undo + 1, "the duplicate and the placement are one step");
        let tr = e.session().track(t).unwrap();
        assert_eq!(tr.playlists.len(), 2);
        assert_eq!(tr.clips().len(), 1);
        assert_eq!(tr.playlists[0].clips.len(), 2, "the old clips stay as the previous take");
    }

    #[test]
    fn loop_recording_makes_each_pass_a_take_on_its_own_playlist() {
        let (mut e, t) = midi_engine();
        let mut events = vec![on(0, 60), off(12_000, 60), on(48_000 + 24_000, 62), off(48_000 + 36_000, 62)];
        for ev in &mut events {
            ev.track = t;
        }
        let undo = e.undo_history().len();
        let ids = place_midi_recording(&mut e, &plan(96_000, Some(48_000)), &events, 0);
        assert_eq!(ids.len(), 2);
        let tr = e.session().track(t).unwrap();
        assert_eq!(tr.playlists.len(), 2);
        assert_eq!(tr.active_playlist, 1);
        assert_eq!(tr.playlists[0].clips[0].range(), Range::new(96_000, 144_000));
        assert_eq!(tr.playlists[1].clips[0].range(), Range::new(96_000, 144_000));
        assert_eq!(e.undo_history().len(), undo + 1);
        assert_eq!(notes_of(&e, ids[0]), vec![(60, 0, TICKS_PER_QUARTER / 2)]);
        assert_eq!(notes_of(&e, ids[1]), vec![(62, TICKS_PER_QUARTER, TICKS_PER_QUARTER / 2)]);
    }
}
