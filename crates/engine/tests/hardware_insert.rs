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

fn refused_any(r: Result<serde_json::Value, EngineError>) -> String {
    match r {
        Err(EngineError::BadParams(_, msg)) => msg,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("transit-hwins-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("s.scraft")
}

#[test]
fn an_ordinary_route_to_a_hardware_send_path_is_refused_and_so_is_the_insert_that_would_meet_one() {
    let mut e = engine(&[("audio", "Gtr"), ("audio", "Dr"), ("aux", "Fx")]);
    hardware(&mut e, "Gtr", 0, "Out 3-4").unwrap();
    // A track output and a send to the send path would sum into it unpadded.
    let why = refused_any(e.execute("track.output", &json!({"tracks": ["Dr"], "output": "Out 3-4"})));
    assert!(why.contains("send path") && why.contains("Out 3-4"), "{why}");
    assert_eq!(e.session().tracks[1].mixer.output, Route::Main);
    refused(e.execute("mix.send", &json!({"track": "Dr", "slot": 0, "bus": "Out 3-4"})), "send path");
    assert!(e.session().tracks[1].mixer.sends.iter().all(Option::is_none));
    // Other paths are fine; and a hardware insert on a path that is already a route target is not.
    e.execute("track.output", &json!({"tracks": ["Dr"], "output": "Out 5-6"})).unwrap();
    refused(hardware(&mut e, "Fx", 0, "Out 5-6"), "Out 5-6");
    assert!(e.session().tracks[2].mixer.inserts[0].is_none());
    // Moving the route away makes room.
    e.execute("track.output", &json!({"tracks": ["Dr"], "output": "Main"})).ok();
}

#[test]
fn an_insert_with_both_a_plugin_and_a_hardware_pair_is_refused() {
    let mut e = engine(&[("audio", "Gtr")]);
    let why = refused_any(e.execute("mix.insert", &json!({"track": "Gtr", "plugin": "gain", "hardware": {"send": "Out 3-4", "return": "In 3-4"}})));
    assert!(why.contains("not both"), "{why}");
    assert!(e.session().tracks[0].mixer.inserts.iter().all(Option::is_none));
}

#[test]
fn a_session_that_opens_with_a_conflicting_hardware_insert_opens_with_it_disabled_and_says_so() {
    let mut e = engine(&[("audio", "A"), ("audio", "B"), ("audio", "C")]);
    hardware(&mut e, "A", 0, "Out 3-4").unwrap();
    hardware(&mut e, "C", 0, "Out 5-6").unwrap();
    // A file edited outside the app: B sends on A's path, C's strip routes to its own send.
    e.session_mut().tracks[1].mixer.inserts[0] = Some(soundcraft_model::Insert::hardware("Out 3-4", "In 1-2"));
    e.session_mut().tracks[2].mixer.output = Route::Hardware("Out 5-6".into());
    let file = temp("open");
    e.execute("session.save", &json!({"path": file.to_string_lossy(), "replace": true})).unwrap();
    e.messages.clear();
    e.execute("session.open", &json!({"path": file.to_string_lossy()})).unwrap();
    let disabled = |e: &Engine, t: usize| e.session().tracks[t].mixer.inserts[0].as_ref().unwrap().hardware.as_ref().unwrap().disabled.clone();
    assert_eq!(disabled(&e, 0), None, "the first insert on a path keeps it");
    assert_eq!(disabled(&e, 1).as_deref(), Some("send path in use by A"));
    assert!(disabled(&e, 2).is_some(), "the strip that feeds its own send is disabled");
    assert!(e.messages.iter().any(|m| m.contains("`B`") && m.contains("send path in use by A")), "{:?}", e.messages);
    // Reverting runs the same check.
    e.session_mut().tracks[1].mixer.inserts[0] = Some(soundcraft_model::Insert::hardware("Out 3-4", "In 1-2"));
    e.execute("session.save", &json!({"path": file.to_string_lossy(), "replace": true})).unwrap();
    e.messages.clear();
    e.execute("session.revert", &json!({})).unwrap();
    assert!(disabled(&e, 1).is_some() && e.messages.iter().any(|m| m.contains("`B`")), "{:?}", e.messages);
    // The report shows it, and the plan treats it as dry.
    let report = soundcraft_mix::hardware_report(e.session(), e.io_map(), 6, 4, true, 48_000);
    let b = report.iter().find(|r| r.track == e.session().tracks[1].id).unwrap();
    assert!(matches!(&b.state, soundcraft_mix::HardwareState::Disabled(why) if why == "send path in use by A"), "{:?}", b.state);
    // Making the insert again clears the reason when nothing conflicts.
    e.execute("mix.insert_remove", &json!({"track": "A", "slot": 0})).unwrap();
    hardware(&mut e, "B", 0, "Out 3-4").unwrap();
    assert_eq!(disabled(&e, 1), None);
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn a_duplicated_track_keeps_its_hardware_insert_disabled() {
    let mut e = engine(&[("audio", "Gtr")]);
    hardware(&mut e, "Gtr", 0, "Out 3-4").unwrap();
    e.execute("track.duplicate", &json!({"track": "Gtr"})).unwrap();
    assert_eq!(e.session().tracks.len(), 2);
    let copy = e.session().tracks.iter().find(|t| t.name.ends_with(".dup")).unwrap();
    let h = copy.mixer.inserts[0].as_ref().unwrap().hardware.as_ref().unwrap();
    assert_eq!((h.send.as_str(), h.disabled.as_deref()), ("Out 3-4", Some("send path in use by Gtr")));
    let original = e.session().tracks[0].mixer.inserts[0].as_ref().unwrap().hardware.as_ref().unwrap();
    assert_eq!(original.disabled, None);
}

#[test]
fn measuring_is_refused_for_unknown_paths_a_running_transport_and_without_a_live_device() {
    let mut e = engine(&[("audio", "Gtr")]);
    let io = e.io_map().clone();
    assert!(io.path("Out 3-4").is_some() && io.path("In 3-4").is_some());
    refused(e.execute("io.measure_hardware_latency", &json!({"send": "Nowhere", "ret": "In 3-4"})), "Nowhere");
    refused(e.execute("io.measure_hardware_latency", &json!({"send": "Out 3-4", "ret": "Out 3-4"})), "input");
    refused(e.execute("io.measure_hardware_latency", &json!({"send": "Out 3-4"})), "ret");
    refused(e.execute("io.measure_hardware_latency", &json!({"send": "Out 3-4", "ret": "In 3-4"})), "live audio device");
    e.transport.playing = true;
    refused(e.execute("io.measure_hardware_latency", &json!({"send": "Out 3-4", "ret": "In 3-4"})), "stop the transport");
    e.transport.playing = false;
    assert_eq!(e.hardware_measurement("Out 3-4", "In 3-4").unwrap(), (vec![2, 3], vec![2, 3]));
}

#[test]
fn a_stored_measurement_is_per_machine_clears_by_command_and_leaves_the_session_clean() {
    let mut e = engine(&[("audio", "Gtr")]);
    let (revision, dirty, undo) = (e.io_revision(), e.is_dirty(), e.undo_history().len());
    e.store_hardware_latency("Out 3-4", "In 3-4", 612);
    assert_eq!(e.io_map().hardware_latency("out 3-4", "in 3-4", e.session().sample_rate.hz()), Some(612), "at the session rate");
    assert!(e.io_revision() > revision);
    assert_eq!((e.is_dirty(), e.undo_history().len()), (dirty, undo), "no undo step, document untouched");
    assert_eq!(e.execute("io.clear_hardware_latency", &json!({"send": "Out 3-4", "ret": "In 3-4"})).unwrap()["cleared"], true);
    assert_eq!(e.execute("io.clear_hardware_latency", &json!({"send": "Out 3-4", "ret": "In 3-4"})).unwrap()["cleared"], false);
    assert!(e.io_map().hardware_latency.is_empty());
}
