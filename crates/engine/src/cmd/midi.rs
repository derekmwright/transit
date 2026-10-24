//! MIDI note editing (MIDI Editor, pencil on MIDI tracks).

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_midi::{Note, Sequence};
use soundcraft_model::{Clip, ClipContent, ClipId};
use soundcraft_time::TICKS_PER_QUARTER;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "midi.notes", "List MIDI Notes", [], None, "{clip}", always, notes),
        cmd!(
            "midi.new_clip",
            "New MIDI Clip",
            ["Clip"],
            None,
            "{track, start?, end?|length?}; bar|beat[|tick] strings (1-based session positions; length 1|2 = 1 beat and 2|1 = 1 bar at start), or sample/time values; default: selection or 1 bar",
            has_tracks,
            new_clip
        ),
        cmd!(
            "midi.note_add",
            "Add Note",
            [],
            None,
            "{clip, pitch, start_ticks?|start?, length_ticks?: 480|length?, velocity?: 100, channel?: 0}; start/length: bar|beat[|tick] strings only, 1-based, start relative to clip, length 1|2 = 1 beat and 2|1 = 1 bar at note start; numeric values require start_ticks/length_ticks; aliases exclusive",
            always,
            note_add
        ),
        cmd!(
            "midi.note_edit",
            "Edit Note",
            [],
            None,
            "{clip, index, pitch?, start_ticks?|start?, length_ticks?|length?, velocity?}; start/length: bar|beat[|tick] strings only, 1-based, start relative to clip, length 1|2 = 1 beat and 2|1 = 1 bar at note start; numeric values require start_ticks/length_ticks; aliases exclusive",
            always,
            note_edit
        ),
        cmd!("midi.note_delete", "Delete Notes", [], None, "{clip, indices: [n]}", always, note_delete),
        cmd!(
            "midi.chord_add",
            "Add Chord",
            [],
            None,
            "{clip, root: MIDI number or note name (C3=60, bare G=G3), quality: maj|min|dim|aug|maj7|min7|dom7|sus2|sus4 OR notes: [1..32 semitone intervals], inversion?: 0, start?|start_ticks?, length?|length_ticks?: 480, velocity?: 100}; start/length: bar|beat[|tick] strings only, 1-based, start relative to clip, length 1|2 = 1 beat and 2|1 = 1 bar at note start; numeric values require start_ticks/length_ticks; aliases exclusive; inversion raises lowest N tones one octave",
            always,
            chord_add
        ),
        cmd!("midi.notes_move", "Move Notes", [], None, "{clip, indices: [n], ticks?: 0, semitones?: 0}", always, notes_move),
        cmd!(
            "midi.notes_transpose",
            "Transpose Notes",
            [],
            None,
            "{clip, indices: [n], semitones}; pitches clamp to 0..127",
            always,
            notes_transpose
        ),
        cmd!(
            "midi.notes_constrain",
            "Constrain Notes to Scale",
            [],
            None,
            "{clip, indices: [n], root: 0..11 (C=0), scale: major|minor}; nearest tone, ties downward; minor is natural minor",
            always,
            notes_constrain
        ),
    ]
}

fn seq_mut<'a>(e: &'a mut Engine, id: ClipId, cmd: &str) -> Result<&'a mut Sequence> {
    match e.session_mut().find_clip_mut(id).map(|c| &mut c.content) {
        Some(ClipContent::Midi { sequence }) => Ok(sequence),
        Some(_) => Err(bad(cmd, "not a MIDI clip")),
        None => Err(bad(cmd, "no such clip")),
    }
}

fn notes(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.notes")?;
    match e.session().find_clip(id).map(|(_, c)| &c.content) {
        Some(ClipContent::Midi { sequence }) => Ok(json!({"notes": sequence.notes})),
        _ => Err(bad("midi.notes", "not a MIDI clip")),
    }
}

fn new_clip(e: &mut Engine, p: &Value) -> Result<Value> {
    let t = tracks_required(e, "midi.new_clip", p)?.first().copied().ok_or_else(|| bad("midi.new_clip", "no track"))?;
    if !e.session().track(t).is_some_and(|x| x.kind.is_midi()) {
        return Err(bad("midi.new_clip", "not a MIDI or instrument track"));
    }
    let mut r = midi_time::clip_range(e, p, "midi.new_clip")?;
    if r.is_empty() {
        let s = e.session();
        let bar = soundcraft_time::GridValue::Note { value: soundcraft_time::NoteValue::Bar, dotted: false, triplet: false };
        let start = bar.line_at_or_before(r.start, s.sample_rate, &s.tempo, s.frame_rate);
        r = soundcraft_time::Range::new(start, bar.next_line(start, s.sample_rate, &s.tempo, s.frame_rate));
    }
    let s = e.session_mut();
    let id = s.new_clip_id();
    let name = s.track(t).map(|x| x.name.clone()).unwrap_or_default();
    crate::edit::place_clip(s, t, Clip::midi(id, name, r.start, r.len(), Sequence::default()));
    Ok(json!({"clip": id, "start": r.start, "end": r.end}))
}

fn note_add(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.note_add")?;
    let (start, length) = midi_time::timing(e, p, id, TICKS_PER_QUARTER / 2, "midi.note_add")?;
    let n = Note {
        pitch: i64_or(p, "pitch", 60).clamp(0, 127) as u8,
        velocity: i64_or(p, "velocity", 100).clamp(1, 127) as u8,
        release_velocity: 64,
        channel: i64_or(p, "channel", 0).clamp(0, 15) as u8,
        start,
        length,
    };
    let seq = seq_mut(e, id, "midi.note_add")?;
    seq.notes.push(n);
    seq.sort();
    let index = seq.notes.iter().position(|x| *x == n).unwrap_or(0);
    Ok(json!({"index": index, "notes": seq.notes.len()}))
}

fn note_edit(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.note_edit")?;
    let idx = p.get("index").and_then(Value::as_u64).ok_or_else(|| bad("midi.note_edit", "`index` required"))? as usize;
    let original = *seq_mut(e, id, "midi.note_edit")?.notes.get(idx).ok_or_else(|| bad("midi.note_edit", "index out of range"))?;
    let origin = midi_time::origin(e, id, "midi.note_edit")?;
    let start = midi_time::param(e, p, "start", origin, "midi.note_edit")?.unwrap_or(original.start).max(0);
    let at = origin.checked_add(start).ok_or_else(|| bad("midi.note_edit", "note start is out of range"))?;
    let length = midi_time::param(e, p, "length", at, "midi.note_edit")?.unwrap_or(original.length).max(1);
    start.checked_add(length).ok_or_else(|| bad("midi.note_edit", "note end is out of range"))?;
    let seq = seq_mut(e, id, "midi.note_edit")?;
    let n = seq.notes.get_mut(idx).ok_or_else(|| bad("midi.note_edit", "index out of range"))?;
    if let Some(v) = p.get("pitch").and_then(Value::as_i64) {
        n.pitch = v.clamp(0, 127) as u8;
    }
    n.start = start;
    n.length = length;
    if let Some(v) = p.get("velocity").and_then(Value::as_i64) {
        n.velocity = v.clamp(1, 127) as u8;
    }
    let edited = *n;
    seq.sort();
    Ok(json!({"note": edited, "index": seq.notes.iter().position(|x| *x == edited)}))
}

fn chord_add(e: &mut Engine, p: &Value) -> Result<Value> {
    let cmd = "midi.chord_add";
    let id = clip_id(p, cmd)?;
    let pitches = midi_chord::pitches(p)?;
    let (start, length) = midi_time::timing(e, p, id, TICKS_PER_QUARTER / 2, cmd)?;
    let velocity = match p.get("velocity") {
        None => 100,
        Some(v) => v.as_u64().filter(|v| (1..=127).contains(v)).ok_or_else(|| bad(cmd, "velocity must be 1..127"))? as u8,
    };
    let notes: Vec<_> = pitches.into_iter().map(|pitch| Note { pitch, velocity, release_velocity: 64, channel: 0, start, length }).collect();
    let seq = seq_mut(e, id, cmd)?;
    seq.notes.extend_from_slice(&notes);
    seq.sort();
    Ok(json!({"added": notes.len(), "notes": notes}))
}

fn indices(p: &Value) -> Vec<usize> {
    p.get("indices").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as usize).collect()).unwrap_or_default()
}

fn note_delete(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.note_delete")?;
    let idx = indices(p);
    let seq = seq_mut(e, id, "midi.note_delete")?;
    let before = seq.notes.len();
    let mut i = 0;
    seq.notes.retain(|_| {
        let keep = !idx.contains(&i);
        i += 1;
        keep
    });
    Ok(json!({"deleted": before - seq.notes.len()}))
}

fn notes_move(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.notes_move")?;
    let idx = indices(p);
    let dt = i64_or(p, "ticks", 0);
    let ds = i64_or(p, "semitones", 0);
    let seq = seq_mut(e, id, "midi.notes_move")?;
    for (i, n) in seq.notes.iter_mut().enumerate() {
        if idx.contains(&i) {
            n.start = (n.start + dt).max(0);
            n.pitch = (i64::from(n.pitch) + ds).clamp(0, 127) as u8;
        }
    }
    seq.sort();
    Ok(json!({"moved": idx.len()}))
}

/// Validate the complete selection before editing; repeated indices edit a note only once.
fn pitch_edit(e: &mut Engine, p: &Value, command: &str, pitch: impl Fn(u8) -> u8) -> Result<Value> {
    let id = clip_id(p, command)?;
    let raw = p.get("indices").and_then(Value::as_array).ok_or_else(|| bad(command, "indices array required"))?;
    let mut selected = raw
        .iter()
        .map(|v| v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad(command, "invalid note index")))
        .collect::<Result<Vec<_>>>()?;
    selected.sort_unstable();
    selected.dedup();
    let seq = seq_mut(e, id, command)?;
    if selected.iter().any(|&i| i >= seq.notes.len()) {
        return Err(bad(command, "index out of range"));
    }
    let mut edited = Vec::with_capacity(selected.len());
    for i in selected {
        let note = &mut seq.notes[i];
        note.pitch = pitch(note.pitch);
        edited.push(*note);
    }
    seq.sort();
    Ok(json!({"notes": edited}))
}

fn notes_transpose(e: &mut Engine, p: &Value) -> Result<Value> {
    let command = "midi.notes_transpose";
    let delta = p.get("semitones").and_then(Value::as_i64).ok_or_else(|| bad(command, "integer semitones required"))?;
    pitch_edit(e, p, command, |pitch| i64::from(pitch).saturating_add(delta).clamp(0, 127) as u8)
}

fn notes_constrain(e: &mut Engine, p: &Value) -> Result<Value> {
    use soundcraft_midi::scale::{KeyScale, Scale};
    let command = "midi.notes_constrain";
    let root = p.get("root").and_then(Value::as_u64).filter(|&n| n < 12).ok_or_else(|| bad(command, "root must be 0..11"))? as u8;
    let scale = match p.get("scale").and_then(Value::as_str) {
        Some("major") => Scale::Major,
        Some("minor") => Scale::Minor,
        _ => return Err(bad(command, "scale must be major or minor")),
    };
    let key = KeyScale { root, scale };
    pitch_edit(e, p, command, |pitch| key.nearest(pitch))
}

#[cfg(test)]
mod pitch_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, TrackKind};

    #[test]
    fn create_clip_add_edit_move_delete() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        let r = e.execute("midi.new_clip", &json!({"track": t.0})).unwrap();
        let clip = r["clip"].as_u64().unwrap();
        e.execute("midi.note_add", &json!({"clip": clip, "pitch": 60, "start_ticks": 0})).unwrap();
        e.execute("midi.note_add", &json!({"clip": clip, "pitch": 64, "start_ticks": 480})).unwrap();
        e.execute("midi.note_edit", &json!({"clip": clip, "index": 0, "velocity": 30})).unwrap();
        e.execute("midi.notes_move", &json!({"clip": clip, "indices": [1], "semitones": 3})).unwrap();
        let n = e.execute("midi.notes", &json!({"clip": clip})).unwrap();
        assert_eq!(n["notes"][0]["velocity"], 30);
        assert_eq!(n["notes"][1]["pitch"], 67);
        e.execute("midi.note_delete", &json!({"clip": clip, "indices": [0]})).unwrap();
        assert_eq!(e.execute("midi.notes", &json!({"clip": clip})).unwrap()["notes"].as_array().map(Vec::len), Some(1));
        assert!(e.execute("midi.note_edit", &json!({"clip": clip, "index": 99})).is_err());
    }

    #[test]
    fn rejects_audio_tracks() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        assert!(e.execute("midi.new_clip", &json!({"track": t.0})).is_err());
    }
}
