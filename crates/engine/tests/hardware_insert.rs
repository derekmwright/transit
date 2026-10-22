//! ADR 0015 slice (d): `mix.insert` with `hardware` makes a hardware insert slot; the engine refuses
//! a second insert on a send path, a copy of one (same path), and a route that would feed an
//! insert's return back into its own send; bypass, move and remove work like any insert's.

use serde_json::json;
use soundcraft_engine::{Engine, EngineError};
use soundcraft_model::{IoMap, Route};

fn engine(names: &[(&str, &str)]) -> Engine {
    let mut e = Engine::default();
    e.set_io_map(IoMap::for_device("test", "test", 4, 6));
    for (kind, name) in names {
        e.execute("track.new", &json!({"kind": kind, "name": name})).unwrap();
    }
    e
}

fn hardware(e: &mut Engine, track: &str, slot: usize, send: &str) -> Result<serde_json::Value, EngineError> {
    e.execute("mix.insert", &json!({"track": track, "slot": slot, "hardware": {"send": send, "return": "In 3-4"}}))
}

#[track_caller]
fn refused(r: Result<serde_json::Value, EngineError>, what: &str) {
    let Err(EngineError::BadParams(_, msg)) = r else { panic!("expected a refusal, got {r:?}") };
    assert!(msg.contains(what), "`{msg}` should mention `{what}`");
}

#[test]
fn a_hardware_insert_is_an_insert_kind_that_moves_bypasses_and_is_removed() {
    let mut e = engine(&[("audio", "Gtr")]);
    hardware(&mut e, "Gtr", 1, "Out 3-4").unwrap();
    let ins = |e: &Engine, slot: usize| e.session().tracks[0].mixer.inserts[slot].clone();
    let h = ins(&e, 1).unwrap();
    assert_eq!(h.hardware.as_ref().map(|h| (h.send.as_str(), h.ret.as_str())), Some(("Out 3-4", "In 3-4")));
    e.execute("mix.insert_bypass", &json!({"track": "Gtr", "slot": 1})).unwrap();
    assert!(ins(&e, 1).unwrap().bypass);
    e.execute("mix.insert_move", &json!({"track": "Gtr", "from": 1, "to": 3})).unwrap();
    assert!(ins(&e, 1).is_none() && ins(&e, 3).unwrap().hardware.is_some(), "the slot carries its pair when moved");
    e.execute("mix.insert_remove", &json!({"track": "Gtr", "slot": 3})).unwrap();
    assert!(ins(&e, 3).is_none());
    refused(e.execute("mix.insert", &json!({"track": "Gtr", "hardware": {"send": "Out 3-4"}})), "return");
}

#[test]
fn a_send_path_serves_one_hardware_insert() {
    let mut e = engine(&[("audio", "A"), ("audio", "B")]);
    hardware(&mut e, "A", 0, "Out 3-4").unwrap();
    refused(hardware(&mut e, "B", 0, "Out 3-4"), "send");
    assert!(e.session().tracks[1].mixer.inserts[0].is_none(), "the refused insert is not left behind");
    hardware(&mut e, "B", 0, "Out 5-6").unwrap();
    // A copy would send on the same path.
    refused(e.execute("mix.insert_copy", &json!({"track": "A", "from": 0, "to": 1})), "send");
}

#[test]
fn a_route_that_feeds_the_return_into_its_own_send_is_refused() {
    let mut e = engine(&[("audio", "Src"), ("aux", "Sub")]);
    e.execute("track.input", &json!({"tracks": ["Sub"], "input": "Sub In"})).unwrap();
    hardware(&mut e, "Src", 0, "Out 3-4").unwrap();
    // Sub is not downstream of Src yet: its output on the send path is only a sum on that path.
    e.execute("track.output", &json!({"tracks": ["Src"], "output": "Sub In"})).unwrap();
    refused(e.execute("track.output", &json!({"tracks": ["Sub"], "output": "Out 3-4"})), "own send");
    assert_eq!(e.session().tracks[1].mixer.output, Route::Main);
    // The strip's own output on its send path is the same loop.
    refused(e.execute("track.output", &json!({"tracks": ["Src"], "output": "Out 3-4"})), "own send");
    // Another path is fine, and so is an insert made after the routing is in place being refused.
    e.execute("track.output", &json!({"tracks": ["Sub"], "output": "Out 5-6"})).unwrap();
    refused(hardware(&mut e, "Src", 1, "Out 5-6"), "own send");
}
