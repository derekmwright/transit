use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_model::{ChannelFormat, MidiInput, MidiPort, TrackId, TrackKind};

fn tracks(e: &mut Engine) -> (TrackId, TrackId, TrackId) {
    let s = e.session_mut();
    (
        s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None),
        s.add_track(TrackKind::Midi, ChannelFormat::Mono, None),
        s.add_track(TrackKind::Audio, ChannelFormat::Mono, None),
    )
}

#[test]
fn one_undo_changes_multiple_tracks_and_redo_restores_the_device() {
    let mut e = Engine::default();
    let (a, b, _) = tracks(&mut e);
    let before = e.undo_pushes();
    e.execute(
        "midi.input",
        &json!({"tracks": [a.0, b.0, a.0], "port": {"backend": "winmm", "name": "Keys", "ordinal": 0, "id": "usb-keys"}, "channel": 16}),
    )
    .unwrap();
    let expected =
        MidiInput { port: MidiPort::Device { backend: "winmm".into(), name: "Keys".into(), ordinal: 0, id: "usb-keys".into() }, channel: Some(15) };
    assert_eq!(e.undo_pushes(), before + 1);
    for t in [a, b] {
        assert_eq!(e.session().track(t).unwrap().midi_input, expected);
    }
    assert!(e.undo());
    for t in [a, b] {
        assert_eq!(e.session().track(t).unwrap().midi_input, MidiInput::default());
    }
    assert!(e.redo());
    for t in [a, b] {
        assert_eq!(e.session().track(t).unwrap().midi_input, expected);
    }
    let serialized = serde_json::to_string(e.session()).unwrap();
    let restored: soundcraft_model::Session = serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored.track(a).unwrap().midi_input, expected);
}

#[test]
fn partial_changes_preserve_the_other_filter_and_selection_works() {
    let mut e = Engine::default();
    let (a, b, _) = tracks(&mut e);
    e.session_mut().edit.selected_tracks = vec![a, b];
    e.execute("midi.input", &json!({"channel": 1})).unwrap();
    e.execute("midi.input", &json!({"track": a.0, "port": "none"})).unwrap();
    assert_eq!(e.session().track(a).unwrap().midi_input, MidiInput { port: MidiPort::None, channel: Some(0) });
    e.execute("midi.input", &json!({"track": a.0, "channel": "all"})).unwrap();
    assert_eq!(e.session().track(a).unwrap().midi_input, MidiInput { port: MidiPort::None, channel: None });
    assert_eq!(e.session().track(b).unwrap().midi_input.channel, Some(0));
    e.execute("midi.input", &json!({"track": a.0, "port": "all"})).unwrap();
    assert_eq!(e.session().track(a).unwrap().midi_input, MidiInput::default());
}

#[test]
fn hostile_channels_ports_and_mixed_track_sets_do_not_mutate() {
    let mut e = Engine::default();
    let (a, _, audio) = tracks(&mut e);
    let before = serde_json::to_value(e.session()).unwrap();
    let undo = e.undo_pushes();
    let mut invalid: Vec<Value> = [-1, 0, 17, 255, 256, i64::MAX].into_iter().map(|v| json!({"track": a.0, "channel": v})).collect();
    for value in [Value::Null, json!(true), json!(1.5), json!("16"), json!([]), json!(18446744073709551615u64)] {
        invalid.push(json!({"track": a.0, "channel": value}));
    }
    for port in [
        json!("unknown"),
        json!(null),
        json!({}),
        json!({"backend": "winmm", "name": "Keys", "ordinal": -1}),
        json!({"backend": "winmm", "name": "Keys", "ordinal": 4294967296u64}),
        json!({"backend": "", "name": "Keys", "ordinal": 0}),
        json!({"backend": "winmm", "name": "Keys", "ordinal": 0, "id": 2}),
    ] {
        invalid.push(json!({"track": a.0, "port": port}));
    }
    invalid.extend([json!({"tracks": [a.0, audio.0], "port": "none"}), json!({"tracks": "bad", "port": "none"}), json!({"track": a.0})]);
    for params in invalid {
        assert!(e.execute("midi.input", &params).is_err(), "{params}");
        assert_eq!(serde_json::to_value(e.session()).unwrap(), before, "{params}");
        assert_eq!(e.undo_pushes(), undo, "{params}");
    }
}
