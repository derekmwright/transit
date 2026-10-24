use super::*;
use soundcraft_model::{ChannelFormat, TrackKind};

fn fixture() -> (Engine, u64, Vec<Note>) {
    let mut e = Engine::default();
    let track = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let clip = e.execute("midi.new_clip", &json!({"track": track.0})).unwrap()["clip"].as_u64().unwrap();
    for (i, pitch) in [0, 61, 64, 126, 127].into_iter().enumerate() {
        e.execute(
            "midi.note_add",
            &json!({"clip": clip, "pitch": pitch, "start_ticks": i * 240, "length_ticks": 120, "velocity": 40 + i, "channel": i}),
        )
        .unwrap();
    }
    let notes = seq_mut(&mut e, ClipId(clip), "test").unwrap().notes.clone();
    (e, clip, notes)
}

#[test]
fn transpose_selection_clamps_extreme_offsets_and_undo_restores_every_field() {
    for delta in [1, -1, 12, -12, i64::MAX, i64::MIN] {
        let (mut e, clip, original) = fixture();
        let undo = e.undo_history().len();
        e.execute("midi.notes_transpose", &json!({"clip": clip, "indices": [0, 1, 3, 4, 1], "semitones": delta})).unwrap();
        let edited = seq_mut(&mut e, ClipId(clip), "test").unwrap().notes.clone();
        for (i, (before, after)) in original.iter().zip(&edited).enumerate() {
            let expected = if i == 2 { before.pitch } else { i64::from(before.pitch).saturating_add(delta).clamp(0, 127) as u8 };
            assert_eq!(after.pitch, expected);
            assert_eq!(Note { pitch: before.pitch, ..*after }, *before);
        }
        assert_eq!(e.undo_history().len(), undo + 1);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(seq_mut(&mut e, ClipId(clip), "test").unwrap().notes, original);
        e.execute("edit.redo", &json!({})).unwrap();
        assert_eq!(seq_mut(&mut e, ClipId(clip), "test").unwrap().notes, edited);
    }
}

#[test]
fn constrain_selected_notes_major_minor_and_non_c_root_in_one_undo_step() {
    for (root, scale, expected) in [(0, "major", [0, 60, 64, 125, 127]), (0, "minor", [0, 60, 63, 125, 127]), (2, "major", [1, 61, 64, 126, 127])] {
        let (mut e, clip, original) = fixture();
        let undo = e.undo_history().len();
        e.execute("midi.notes_constrain", &json!({"clip": clip, "indices": [0, 1, 2, 3], "root": root, "scale": scale})).unwrap();
        let edited = seq_mut(&mut e, ClipId(clip), "test").unwrap().notes.clone();
        assert_eq!(edited.iter().map(|n| n.pitch).collect::<Vec<_>>(), expected);
        for (before, after) in original.iter().zip(&edited) {
            assert_eq!(Note { pitch: before.pitch, ..*after }, *before);
        }
        assert_eq!(e.undo_history().len(), undo + 1);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(seq_mut(&mut e, ClipId(clip), "test").unwrap().notes, original);
    }
}

#[test]
fn pitch_commands_reject_bad_parameters_atomically() {
    let (mut e, clip, original) = fixture();
    let undo = e.undo_history().len();
    for indices in [json!([0, 99]), json!([0, -1]), json!([0, 1.5]), json!(null), json!("all")] {
        assert!(e.execute("midi.notes_transpose", &json!({"clip": clip, "indices": indices, "semitones": 12})).is_err());
        assert!(e.execute("midi.notes_constrain", &json!({"clip": clip, "indices": indices, "root": 0, "scale": "minor"})).is_err());
    }
    for (root, scale) in [(json!(12), json!("major")), (json!(-1), json!("minor")), (json!(0), json!("unknown"))] {
        assert!(e.execute("midi.notes_constrain", &json!({"clip": clip, "indices": [0], "root": root, "scale": scale})).is_err());
    }
    assert!(e.execute("midi.notes_transpose", &json!({"clip": clip, "indices": [0], "semitones": 1.5})).is_err());
    assert_eq!(seq_mut(&mut e, ClipId(clip), "test").unwrap().notes, original);
    assert_eq!(e.undo_history().len(), undo);
}
