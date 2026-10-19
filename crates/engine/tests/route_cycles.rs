//! L22: `track.output`, `track.input` and `mix.send` refuse a route that closes a loop, name the
//! loop in the error, and leave the session and the undo history as they were.

use serde_json::json;
use soundcraft_engine::{Engine, EngineError};
use soundcraft_model::Route;

/// Auxes `names`, each reading its own bus "<name> In".
fn auxes(names: &[&str]) -> Engine {
    let mut e = Engine::default();
    for n in names {
        e.execute("track.new", &json!({"kind": "aux", "name": n})).unwrap();
        e.execute("track.input", &json!({"tracks": [n], "input": format!("{n} In")})).unwrap();
    }
    e
}

/// Chain: each aux outputs to the next one's input bus.
fn chain(e: &mut Engine, names: &[&str]) {
    for w in names.windows(2) {
        e.execute("track.output", &json!({"tracks": [w[0]], "output": format!("{} In", w[1])})).unwrap();
    }
}

/// What a refused command must not change: every track's routes, the busses, the undo history.
fn snapshot(e: &Engine) -> (Vec<(Route, Route, Vec<Option<Route>>)>, usize, Vec<String>, bool) {
    let s = e.session();
    let routes = s
        .tracks
        .iter()
        .map(|t| (t.mixer.input.clone(), t.mixer.output.clone(), t.mixer.sends.iter().map(|x| x.as_ref().map(|x| x.target.clone())).collect()))
        .collect();
    (routes, s.busses.len(), e.undo_history(), e.can_redo())
}

#[track_caller]
fn refused(e: &mut Engine, cmd: &str, params: serde_json::Value, path: &str) {
    let before = snapshot(e);
    let err = e.execute(cmd, &params).expect_err("a routing loop must be refused");
    let EngineError::BadParams(id, msg) = &err else { panic!("expected BadParams, got {err:?}") };
    assert_eq!(id, cmd);
    assert!(msg.contains("routing loop") && msg.contains(path), "message `{msg}` should name the loop `{path}`");
    assert_eq!(snapshot(e), before, "a refused route leaves the session and history untouched");
}

#[test]
fn track_output_refuses_a_direct_loop() {
    let mut e = auxes(&["A", "B"]);
    chain(&mut e, &["A", "B"]);
    refused(&mut e, "track.output", json!({"tracks": ["B"], "output": "A In"}), "B → A → B");
}

#[test]
fn track_output_refuses_an_indirect_loop() {
    let mut e = auxes(&["A", "B", "C"]);
    chain(&mut e, &["A", "B", "C"]);
    refused(&mut e, "track.output", json!({"tracks": ["C"], "output": "A In"}), "C → A → B → C");
}

#[test]
fn track_output_refuses_an_aux_into_its_own_input() {
    let mut e = auxes(&["A"]);
    refused(&mut e, "track.output", json!({"tracks": ["A"], "output": "A In"}), "A → A");
}

#[test]
fn track_input_refuses_a_direct_loop() {
    let mut e = auxes(&["A", "B"]);
    chain(&mut e, &["A", "B"]);
    e.execute("track.output", &json!({"tracks": ["B"], "output": "Ret"})).unwrap();
    refused(&mut e, "track.input", json!({"tracks": ["A"], "input": "Ret"}), "A → B → A");
}

#[test]
fn track_input_refuses_an_indirect_loop() {
    let mut e = auxes(&["A", "B", "C"]);
    chain(&mut e, &["A", "B", "C"]);
    e.execute("track.output", &json!({"tracks": ["C"], "output": "Ret"})).unwrap();
    refused(&mut e, "track.input", json!({"tracks": ["A"], "input": "Ret"}), "A → B → C → A");
}

#[test]
fn mix_send_refuses_a_direct_loop() {
    let mut e = auxes(&["A", "B"]);
    chain(&mut e, &["A", "B"]);
    refused(&mut e, "mix.send", json!({"track": "B", "slot": 0, "bus": "A In"}), "B → A → B");
}

#[test]
fn mix_send_refuses_an_indirect_loop() {
    let mut e = auxes(&["A", "B", "C"]);
    chain(&mut e, &["A", "B", "C"]);
    refused(&mut e, "mix.send", json!({"track": "C", "slot": 3, "bus": "A In"}), "C → A → B → C");
}

#[test]
fn retargeting_a_send_into_a_loop_is_refused_and_the_old_send_stays() {
    let mut e = auxes(&["A", "B"]);
    chain(&mut e, &["A", "B"]);
    e.execute("mix.send", &json!({"track": "B", "slot": 1, "bus": "Verb"})).unwrap();
    refused(&mut e, "mix.send", json!({"track": "B", "slot": 1, "bus": "A In"}), "B → A → B");
    let verb = e.session().bus_by_name("Verb").unwrap().id;
    assert_eq!(e.session().track_by_name("B").unwrap().mixer.sends[1].as_ref().unwrap().target, Route::Bus(verb));
}

#[test]
fn a_chain_deeper_than_eight_hops_is_accepted() {
    let names: Vec<String> = (0..12).map(|i| format!("Aux {i}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut e = auxes(&names);
    e.execute("track.new", &json!({"count": 1, "name": "Src"})).unwrap();
    e.execute("track.output", &json!({"tracks": ["Src"], "output": "Aux 0 In"})).unwrap();
    chain(&mut e, &names);
    // Sends along the chain, skipping hops, are legal too (still feed forward).
    e.execute("mix.send", &json!({"track": "Aux 2", "slot": 0, "bus": "Aux 9 In"})).unwrap();
    e.execute("track.output", &json!({"tracks": ["Aux 11"], "output": "main"})).unwrap();
    let s = e.session();
    assert!(s.route_graph().topological_order().is_ok());
    // ...and closing it from the far end is still refused.
    refused(&mut e, "mix.send", json!({"track": "Aux 11", "slot": 0, "bus": "Aux 0 In"}), "Aux 11 → Aux 0 → Aux 1");
}

#[test]
fn routing_several_tracks_at_once_checks_them_together() {
    let mut e = auxes(&["A", "B"]);
    e.execute("mix.new_bus", &json!({"name": "X"})).unwrap();
    e.execute("track.output", &json!({"tracks": ["A"], "output": "B In"})).unwrap();
    // B → X is fine alone; A's input from X then closes A → B → A.
    e.execute("track.output", &json!({"tracks": ["B"], "output": "X"})).unwrap();
    refused(&mut e, "track.input", json!({"tracks": ["A"], "input": "X"}), "A → B → A");
}

#[test]
fn a_refused_command_after_other_edits_keeps_undo_and_redo() {
    let mut e = auxes(&["A", "B"]);
    chain(&mut e, &["A", "B"]);
    e.execute("track.output", &json!({"tracks": ["B"], "output": "main"})).unwrap();
    assert!(e.undo());
    assert!(e.can_redo());
    let history = e.undo_history();
    refused(&mut e, "track.output", json!({"tracks": ["B"], "output": "A In"}), "B → A → B");
    assert_eq!(e.undo_history(), history);
    assert!(e.can_redo(), "a refused command does not clear redo");
}
