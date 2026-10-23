use serde_json::{Value, json};
use soundcraft_engine::Engine;

fn setup() -> (Engine, Value) {
    let mut e = Engine::default();
    e.execute("track.new", &json!({"kind": "instrument", "name": "Keys"})).unwrap();
    let clip = e.execute("midi.new_clip", &json!({"track": "Keys", "start": "1|1", "length": "5|1"})).unwrap()["clip"].clone();
    (e, clip)
}

fn notes(e: &mut Engine, clip: &Value) -> Value {
    e.execute("midi.notes", &json!({"clip": clip})).unwrap()["notes"].clone()
}

#[test]
fn chord_qualities_and_inversions_produce_exact_voicings_and_one_undo_step() {
    let qualities: [(&str, &[u8]); 9] = [
        ("maj", &[0, 4, 7]),
        ("min", &[0, 3, 7]),
        ("dim", &[0, 3, 6]),
        ("aug", &[0, 4, 8]),
        ("maj7", &[0, 4, 7, 11]),
        ("min7", &[0, 3, 7, 10]),
        ("dom7", &[0, 4, 7, 10]),
        ("sus2", &[0, 2, 7]),
        ("sus4", &[0, 5, 7]),
    ];
    for (quality, intervals) in qualities {
        for inversion in 0..intervals.len() {
            let (mut e, clip) = setup();
            let result = e.execute("midi.chord_add", &json!({"clip": clip, "root": "G3", "quality": quality, "inversion": inversion, "start": "2|3|120", "length": "1|2", "velocity": 80})).unwrap();
            let mut expected: Vec<_> = intervals.iter().enumerate().map(|(i, n)| 67 + n + if i < inversion { 12 } else { 0 }).collect();
            expected.sort_unstable();
            let actual = notes(&mut e, &clip);
            assert_eq!(actual, result["notes"]);
            assert_eq!(
                actual.as_array().unwrap().iter().map(|n| n["pitch"].as_u64().unwrap() as u8).collect::<Vec<_>>(),
                expected,
                "{quality} inversion {inversion}"
            );
            for n in actual.as_array().unwrap() {
                assert_eq!(n["start"], 5880);
                assert_eq!(n["length"], 960);
                assert_eq!(n["velocity"], 80);
            }
            e.execute("edit.undo", &json!({})).unwrap();
            assert_eq!(notes(&mut e, &clip), json!([]));
            e.execute("edit.redo", &json!({})).unwrap();
            assert_eq!(notes(&mut e, &clip), actual);
        }
    }
}

#[test]
fn named_numeric_and_custom_roots_follow_transit_octaves() {
    for (root, expected) in [(json!(60), 60), (json!("C3"), 60), (json!("Bb2"), 58), (json!("F#3"), 66), (json!("G"), 67), (json!("C-2"), 0)] {
        let (mut e, clip) = setup();
        e.execute("midi.chord_add", &json!({"clip": clip, "root": root, "notes": [7, 0, 3], "start": "1|1", "length": "2|1"})).unwrap();
        let n = notes(&mut e, &clip);
        assert_eq!(n[0]["pitch"], expected);
        assert_eq!(n[1]["pitch"], expected + 3);
        assert_eq!(n[2]["pitch"], expected + 7);
        assert_eq!(n[0]["length"], 3840);
    }
}

#[test]
fn refused_chords_and_edits_leave_the_notes_and_history_unchanged() {
    let (mut e, clip) = setup();
    e.execute("midi.note_add", &json!({"clip": clip, "pitch": 60, "start_ticks": 0})).unwrap();
    let before = notes(&mut e, &clip);
    let history = e.execute("engine.history", &json!({})).unwrap();
    for overrides in [
        json!({"root": "H3"}),
        json!({"root": "C9223372036854775807"}),
        json!({"root": 128}),
        json!({"root": 127}),
        json!({"root": 0, "quality": null, "notes": [-1, 0]}),
        json!({"quality": "nope"}),
        json!({"notes": [0, 4, 7]}),
        json!({"quality": null}),
        json!({"inversion": -1}),
        json!({"inversion": 3}),
        json!({"velocity": 0}),
        json!({"start": "1|5"}),
        json!({"start": "-1|1"}),
        json!({"length": "1|1"}),
        json!({"start_ticks": 0}),
        json!({"length_ticks": 960}),
        json!({"length": "9223372036854775807|1"}),
    ] {
        let mut p = json!({"clip": clip, "root": 60, "quality": "min", "start": "1|1", "length": "1|2"});
        for (key, value) in overrides.as_object().unwrap() {
            p[key] = value.clone();
        }
        assert!(e.execute("midi.chord_add", &p).is_err(), "{p}");
        assert_eq!(notes(&mut e, &clip), before);
        assert_eq!(e.execute("engine.history", &json!({})).unwrap(), history);
    }
    for command in ["midi.note_add", "midi.note_edit", "event.midi_note"] {
        assert!(e.execute(command, &json!({"clip": clip, "index": 0, "pitch": 72, "start": "1|2", "length": "1|0"})).is_err());
        assert_eq!(notes(&mut e, &clip), before);
    }
    assert!(e.execute("midi.chord_add", &json!({"clip": clip, "root": 60, "notes": vec![0; 33]})).is_err());
    assert!(e.execute("midi.chord_add", &json!({"clip": clip, "root": 60, "notes": []})).is_err());
    assert!(e.execute("midi.chord_add", &json!({"clip": clip, "root": 60, "notes": [0, i64::MAX]})).is_err());
    assert!(e.execute("midi.chord_add", &json!({"clip": clip, "root": 0, "notes": [-1, 0]})).is_err());
}

#[test]
fn retrospective_record_rejects_extreme_addresses_without_an_internal_panic() {
    let (mut e, _) = setup();
    let before = serde_json::to_value(e.session()).unwrap();
    for start in [json!({"start": "2401919801264265|3|127"}), json!({"start_ticks": i64::MAX}), json!({"start_ticks": i64::MAX - 240})] {
        let result = e.execute("event.retrospective_record", &json!({"track": "Keys", "notes": [start]}));
        let error = result.unwrap_err();
        assert!(!matches!(error, soundcraft_engine::EngineError::Internal(..)), "{error}");
        assert_eq!(serde_json::to_value(e.session()).unwrap(), before);
    }
}

#[test]
fn every_tick_note_command_accepts_musical_addresses() {
    let (mut e, clip) = setup();
    e.execute("midi.note_add", &json!({"clip": clip, "start": "1|1", "length": "1|2"})).unwrap();
    e.execute("midi.note_edit", &json!({"clip": clip, "index": 0, "start": "2|3|120", "length": "1|3"})).unwrap();
    let n = notes(&mut e, &clip);
    assert_eq!(n[0]["start"], 5880);
    assert_eq!(n[0]["length"], 1920);
    e.execute("event.midi_note", &json!({"clip": clip, "start": "3|1", "length": "2|1"})).unwrap();
    let n = notes(&mut e, &clip);
    assert_eq!(n[1]["start"], 7680);
    assert_eq!(n[1]["length"], 3840);
    let r = e.execute("event.retrospective_record", &json!({"track": "Keys", "notes": [{"pitch": 64, "start": "2|2", "length": "1|3"}]})).unwrap();
    let n = notes(&mut e, &r["clip"]);
    assert_eq!(n[0]["start"], 960);
    assert_eq!(n[0]["length"], 1920);
}

#[test]
fn clip_lengths_integrate_tempo_and_notes_use_the_meter_at_their_origin() {
    let mut e = Engine::default();
    e.execute("track.new", &json!({"kind": "instrument", "name": "Keys"})).unwrap();
    e.session_mut().tempo.set_tempo(3840, 60.0).unwrap();
    e.session_mut().tempo.set_meter(7680, 6, 8).unwrap();
    let r = e.execute("midi.new_clip", &json!({"track": "Keys", "start": "2|1", "length": "3|1"})).unwrap();
    let sr = e.session().sample_rate.as_f64();
    assert_eq!(r["start"], (sr * 2.0) as i64);
    assert_eq!(r["end"], (sr * 9.0) as i64);
    e.execute("midi.note_add", &json!({"clip": r["clip"], "start": "2|6", "length": "1|2"})).unwrap();
    let n = notes(&mut e, &r["clip"]);
    assert_eq!(n[0]["start"], 6240);
    assert_eq!(n[0]["length"], 480);
}

fn position_formats() -> [(&'static str, i64); 7] {
    [
        ("0:12", 576_000),
        ("1:30", 4_320_000),
        ("00:00:10:00", 480_000),
        ("1:2", 2_976_000),
        ("1|2", 24_000),
        ("5|1|000", 384_000),
        ("bars_beats:5|1|000", 384_000),
    ]
}

#[test]
fn clip_position_formats_keep_clock_time_distinct_from_musical_time() {
    let mut e = Engine::default();
    e.execute("track.new", &json!({"kind": "instrument", "name": "Keys"})).unwrap();
    assert_eq!(e.session().sample_rate.hz(), 48_000);
    for (position, expected) in position_formats() {
        let result = e.execute("midi.new_clip", &json!({"track": "Keys", "start": position, "length": 1000})).unwrap();
        assert_eq!(result["start"], expected, "{position}");
        assert_eq!(result["end"], expected + 1000, "{position}");
        for field in ["end", "length"] {
            let mut params = json!({"track": "Keys", "start": 0});
            params[field] = json!(position);
            let result = e.execute("midi.new_clip", &params).unwrap();
            assert_eq!(result["end"], expected, "{field}: {position}");
        }
    }
}

#[test]
fn retrospective_position_formats_keep_clock_time_distinct_from_musical_time() {
    let mut e = Engine::default();
    e.execute("track.new", &json!({"kind": "instrument", "name": "Keys"})).unwrap();
    assert_eq!(e.session().sample_rate.hz(), 48_000);
    for (position, expected) in position_formats() {
        let result = e.execute("event.retrospective_record", &json!({"track": "Keys", "notes": [{"start": position, "length_ticks": 240}]})).unwrap();
        let clip_id = soundcraft_model::ClipId(result["clip"].as_u64().unwrap());
        let (_, clip) = e.session().find_clip(clip_id).unwrap();
        let origin = e.session().tempo.samples_to_ticks(clip.start, e.session().sample_rate);
        let n = notes(&mut e, &result["clip"]);
        let absolute_tick = origin + n[0]["start"].as_i64().unwrap();
        assert_eq!(e.session().tempo.tick_to_samples(absolute_tick, e.session().sample_rate), expected, "{position}");
        assert_eq!(n[0]["length"], 240);
    }
}

#[test]
fn note_alias_types_are_rejected_with_the_numeric_field_named() {
    let (mut e, clip) = setup();
    e.execute("midi.note_add", &json!({"clip": clip, "pitch": 60})).unwrap();
    let before = serde_json::to_value(e.session()).unwrap();
    for command in ["midi.note_add", "midi.note_edit", "event.midi_note", "midi.chord_add", "event.retrospective_record"] {
        for field in ["start", "length"] {
            // Retrospective start already accepts numeric samples and {seconds} positions.
            if command == "event.retrospective_record" && field == "start" {
                continue;
            }
            for value in [json!(960), json!(true), json!(null), json!([]), json!({})] {
                let mut p = json!({"clip": clip, "index": 0, "pitch": 60, "root": 60, "quality": "min"});
                p[field] = value;
                if command == "event.retrospective_record" {
                    p["start_ticks"] = json!(0);
                    p = json!({"track": "Keys", "notes": [p]});
                }
                let error = e.execute(command, &p).unwrap_err().to_string();
                assert!(error.contains(&format!("{field}_ticks")), "{command}: {p}: {error}");
                assert_eq!(serde_json::to_value(e.session()).unwrap(), before);
            }
        }
    }
}
