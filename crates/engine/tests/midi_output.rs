use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_model::{ChannelFormat, MidiOutChannel, MidiOutPort, MidiOutput, TrackId, TrackKind};

fn tracks(e: &mut Engine) -> (TrackId, TrackId, TrackId) {
    let s = e.session_mut();
    (
        s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None),
        s.add_track(TrackKind::Midi, ChannelFormat::Mono, None),
        s.add_track(TrackKind::Audio, ChannelFormat::Mono, None),
    )
}

fn synth() -> MidiOutPort {
    MidiOutPort { backend: "winmm".into(), name: "Synth OUT".into(), ordinal: 0, id: "usb-synth".into() }
}

#[test]
fn one_undo_step_sets_the_destination_on_several_tracks_and_it_round_trips() {
    let mut e = Engine::default();
    let (a, b, _) = tracks(&mut e);
    let before = e.undo_pushes();
    e.execute("midi.output", &json!({"tracks": [a.0, b.0], "port": {"backend": "winmm", "name": "Synth OUT", "id": "usb-synth"}, "channel": 10, "offset_ms": -4.5}))
        .unwrap();
    let expected = MidiOutput::External { port: synth(), channel: MidiOutChannel::Channel(9), offset_ms: -4.5 };
    assert_eq!(e.undo_pushes(), before + 1);
    for t in [a, b] {
        assert_eq!(e.session().track(t).unwrap().midi_output, expected);
    }
    let text = serde_json::to_string(e.session()).unwrap();
    let restored: soundcraft_model::Session = serde_json::from_str(&text).unwrap();
    assert_eq!(restored.track(b).unwrap().midi_output, expected);
    assert!(e.undo());
    for t in [a, b] {
        assert_eq!(e.session().track(t).unwrap().midi_output, MidiOutput::Internal);
    }
    assert!(e.redo());
    assert_eq!(e.session().track(a).unwrap().midi_output, expected);
}

#[test]
fn partial_changes_keep_the_other_fields_and_internal_resets() {
    let mut e = Engine::default();
    let (_, b, _) = tracks(&mut e);
    e.execute("midi.output", &json!({"track": b.0, "port": {"backend": "winmm", "name": "Synth OUT", "id": "usb-synth"}})).unwrap();
    assert_eq!(e.session().track(b).unwrap().midi_output, MidiOutput::External { port: synth(), channel: MidiOutChannel::AsRecorded, offset_ms: 0.0 });
    e.execute("midi.output", &json!({"track": b.0, "channel": 2})).unwrap();
    e.execute("midi.output", &json!({"track": b.0, "offset_ms": 12})).unwrap();
    assert_eq!(e.session().track(b).unwrap().midi_output, MidiOutput::External { port: synth(), channel: MidiOutChannel::Channel(1), offset_ms: 12.0 });
    let other = json!({"backend": "winmm", "name": "Other"});
    e.execute("midi.output", &json!({"track": b.0, "port": other})).unwrap();
    assert!(matches!(
        &e.session().track(b).unwrap().midi_output,
        MidiOutput::External { port, channel: MidiOutChannel::Channel(1), offset_ms } if port.name == "Other" && *offset_ms == 12.0
    ));
    e.execute("midi.output", &json!({"track": b.0, "channel": "as_recorded"})).unwrap();
    e.execute("midi.output", &json!({"track": b.0, "port": "internal"})).unwrap();
    assert_eq!(e.session().track(b).unwrap().midi_output, MidiOutput::Internal);
    let inspected = soundcraft_engine::inspect::session(&e, false);
    let track = inspected["tracks"].as_array().unwrap().iter().find(|t| t["id"] == json!(b.0)).unwrap();
    assert_eq!(track["midi_output"], json!("internal"));
}

#[test]
fn hostile_params_and_mixed_track_sets_change_nothing() {
    let mut e = Engine::default();
    let (a, b, audio) = tracks(&mut e);
    let port = json!({"backend": "winmm", "name": "Synth OUT"});
    let cases: Vec<Value> = vec![
        json!({"track": audio.0, "port": port}),
        json!({"tracks": [a.0, audio.0], "port": port}),
        json!({"track": a.0}),
        json!({"track": a.0, "channel": 3}),
        json!({"track": a.0, "offset_ms": 3}),
        json!({"track": a.0, "port": "internal", "channel": 3}),
        json!({"track": a.0, "port": {"backend": "winmm", "name": ""}}),
        json!({"track": a.0, "port": {"backend": "", "name": "x"}}),
        json!({"track": a.0, "port": {"backend": "winmm", "name": "x", "ordinal": -1}}),
        json!({"track": a.0, "port": {"backend": "winmm", "name": "x", "ordinal": 1u64 << 40}}),
        json!({"track": a.0, "port": {"backend": "winmm", "name": "x".repeat(2000)}}),
        json!({"track": a.0, "port": 7}),
        json!({"track": a.0, "port": port, "channel": 0}),
        json!({"track": a.0, "port": port, "channel": 17}),
        json!({"track": a.0, "port": port, "channel": "all"}),
        json!({"track": a.0, "port": port, "offset_ms": 501}),
        json!({"track": a.0, "port": port, "offset_ms": "5"}),
        json!({"tracks": "1", "port": port}),
    ];
    for params in cases {
        assert!(e.execute("midi.output", &params).is_err(), "{params}");
        for t in [a, b] {
            assert_eq!(e.session().track(t).unwrap().midi_output, MidiOutput::Internal, "{params}");
        }
    }
}
