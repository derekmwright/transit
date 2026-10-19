use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_model::{AutoParam, TrackId};

fn setup() -> (Engine, TrackId) {
    let mut e = Engine::default();
    let t = TrackId(e.execute("track.new", &json!({"count": 1})).unwrap()["tracks"][0].as_u64().unwrap());
    for at in (0..100).map(|i| i * 10) {
        e.execute("automation.set_point", &json!({"track": t.0, "param": "volume", "at": at, "value": -6.0})).unwrap();
        e.execute("automation.set_point", &json!({"track": t.0, "param": "pan", "at": at, "value": 0.25})).unwrap();
    }
    (e, t)
}

fn count(e: &Engine, t: TrackId, p: &AutoParam) -> usize {
    e.session().track(t).unwrap().lane(p).map_or(0, |l| l.points.len())
}

fn snapshot(e: &Engine, t: TrackId) -> String {
    format!("{:?}", e.session().track(t).unwrap().automation)
}

#[test]
fn param_thins_only_that_lane() {
    let (mut e, t) = setup();
    let pan = AutoParam::Pan(0);
    assert_eq!((count(&e, t, &AutoParam::Volume), count(&e, t, &pan)), (100, 100));
    let r = e.execute("automation.thin", &json!({"tracks": [t.0], "param": "volume"})).unwrap();
    assert_eq!(r["removed"], 98);
    assert_eq!(count(&e, t, &AutoParam::Volume), 2);
    assert_eq!(count(&e, t, &pan), 100);
}

#[test]
fn without_param_thins_every_lane() {
    let (mut e, t) = setup();
    let r = e.execute("automation.thin", &json!({"tracks": [t.0]})).unwrap();
    assert_eq!(r["removed"], 196);
    assert_eq!((count(&e, t, &AutoParam::Volume), count(&e, t, &AutoParam::Pan(0))), (2, 2));
}

#[test]
fn param_naming_an_absent_lane_is_a_no_op() {
    let (mut e, t) = setup();
    let before = snapshot(&e, t);
    let r = e.execute("automation.thin", &json!({"tracks": [t.0], "param": "mute"})).unwrap();
    assert_eq!(r["removed"], 0);
    assert_eq!(snapshot(&e, t), before);
}

#[test]
fn unknown_param_is_bad_params() {
    let (mut e, t) = setup();
    assert!(e.execute("automation.thin", &json!({"tracks": [t.0], "param": "bogus"})).is_err());
}

#[test]
fn one_undo_restores_both_lanes() {
    let (mut e, t) = setup();
    let before = snapshot(&e, t);
    e.execute("automation.thin", &json!({"tracks": [t.0]})).unwrap();
    assert_ne!(snapshot(&e, t), before);
    assert!(e.undo());
    assert_eq!(snapshot(&e, t), before);
    e.execute("automation.thin", &json!({"tracks": [t.0], "param": "pan"})).unwrap();
    assert!(e.undo());
    assert_eq!(snapshot(&e, t), before);
}
