//! The documented four-bar progression is reachable and verifiable through MCP alone.

use serde_json::{Value, json};
use soundcraft_automation::{Headless, mcp::Server};
use soundcraft_engine::Engine;

fn tool(server: &mut Server<Headless>, name: &str, args: Value) -> Value {
    let reply = server.call_tool(name, &args);
    assert_eq!(reply["isError"], false, "{reply}");
    serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn execute(server: &mut Server<Headless>, command: &str, params: Value) -> Value {
    tool(server, "execute", json!({"command": command, "params": params}))
}

#[test]
fn assistant_composes_four_bars_in_g_minor_with_movement_inside_a_bar() {
    let mut server = Server::new(Headless::new(Engine::default()));
    // Fixture: an existing instrument track. No audio device or plug-in is opened.
    execute(&mut server, "track.new", json!({"kind": "instrument", "name": "Keys"}));
    let session = tool(&mut server, "inspect_session", json!({}));
    assert_eq!(session["tempo"][0]["bpm"], 120.0);
    assert_eq!(session["meter"][0]["numerator"], 4);
    assert_eq!(session["meter"][0]["denominator"], 4);
    assert_eq!(session["ticks_per_quarter"], 960);
    assert!(session["key"].is_null());
    let commands = tool(&mut server, "list_commands", json!({"filter": "midi.chord_add"}));
    assert_eq!(commands.as_array().unwrap().len(), 1);
    execute(&mut server, "event.add_key_change", json!({"key": "G minor", "at": 0}));
    let created = execute(&mut server, "midi.new_clip", json!({"track": "Keys", "start": "1|1", "length": "5|1"}));
    let clip = &created["clip"];
    let result = tool(
        &mut server,
        "batch",
        json!({"calls": [
            {"command": "midi.chord_add", "params": {"clip": clip, "root": "G3", "quality": "min", "start": "1|1", "length": "2|1", "velocity": 90}},
            {"command": "midi.chord_add", "params": {"clip": clip, "root": "Eb3", "quality": "maj", "inversion": 1, "start": "2|1", "length": "2|1", "velocity": 86}},
            {"command": "midi.chord_add", "params": {"clip": clip, "root": "Bb2", "quality": "maj", "inversion": 2, "start": "3|1", "length": "2|1", "velocity": 88}},
            {"command": "midi.chord_add", "params": {"clip": clip, "root": "F3", "quality": "maj", "start": "4|1", "length": "2|1", "velocity": 86}}
        ]}),
    );
    assert!(!result.to_string().contains("error"));
    let initial = execute(&mut server, "midi.notes", json!({"clip": clip}));
    assert_eq!(initial["notes"][2]["pitch"], 74);
    execute(&mut server, "midi.note_edit", json!({"clip": clip, "index": 2, "length": "1|3"}));
    execute(&mut server, "midi.chord_add", json!({"clip": clip, "root": "Eb4", "notes": [0], "start": "1|3", "length": "1|3", "velocity": 85}));
    let actual = execute(&mut server, "midi.notes", json!({"clip": clip}));
    let pitches: Vec<_> = actual["notes"].as_array().unwrap().iter().map(|n| n["pitch"].as_u64().unwrap()).collect();
    assert_eq!(pitches, [67, 70, 74, 75, 67, 70, 75, 65, 70, 74, 65, 69, 72]);
    assert_eq!(actual["notes"][2]["length"], 1920);
    assert_eq!(actual["notes"][3]["start"], 1920);
    assert_eq!(actual["notes"][12]["start"], 11520);
    for detail in ["summary", "full"] {
        let session = tool(&mut server, "inspect_session", json!({"detail": detail}));
        assert_eq!(session["key"], "G minor");
        assert_eq!(session["key_signatures"], json!([{"at": 0, "key": "G minor"}]));
        assert_eq!(session["ticks_per_quarter"], 960);
        assert_eq!(session["tracks"][0]["clips"][0]["notes"], 13);
        assert_eq!(session["tracks"][0]["clips"][0]["length"].as_u64().unwrap(), session["sample_rate"].as_u64().unwrap() * 8);
    }
}
