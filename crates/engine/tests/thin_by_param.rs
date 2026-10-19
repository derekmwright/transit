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

#[test]
fn null_param_means_all_lanes_and_non_string_is_bad_params() {
    let (mut e, t) = setup();
    e.execute("automation.thin", &json!({"tracks": [t.0], "param": null})).unwrap();
    assert_eq!((count(&e, t, &AutoParam::Volume), count(&e, t, &AutoParam::Pan(0))), (2, 2));
    let (mut e, t) = setup();
    let before = snapshot(&e, t);
    assert!(e.execute("automation.thin", &json!({"tracks": [t.0], "param": 7})).is_err());
    assert_eq!(snapshot(&e, t), before);
}

fn pts(e: &Engine, t: TrackId, p: &AutoParam) -> Vec<(i64, f32)> {
    e.session().track(t).unwrap().lane(p).map(|l| l.points.iter().map(|x| (x.at, x.value)).collect()).unwrap_or_default()
}

fn ranged() -> (Engine, TrackId, Vec<(i64, f32)>) {
    let mut e = Engine::default();
    let t = TrackId(e.execute("track.new", &json!({"count": 1})).unwrap()["tracks"][0].as_u64().unwrap());
    let set = |e: &mut Engine, at: i64, v: f64| {
        e.execute("automation.set_point", &json!({"track": t.0, "param": "volume", "at": at, "value": v})).unwrap();
    };
    for (at, v) in [(400_000, -6.0), (450_000, -6.0), (500_000, -6.0), (550_000, -6.0)] {
        set(&mut e, at, v);
    }
    for at in (600_000..=900_000).step_by(10_000) {
        set(&mut e, at, -6.0);
    }
    let hand: Vec<_> = pts(&e, t, &AutoParam::Volume).into_iter().filter(|p| p.0 <= 550_000).collect();
    (e, t, hand)
}

#[test]
fn range_thins_only_the_pass_and_keeps_its_endpoints() {
    let (mut e, t, hand) = ranged();
    let r = e.execute("automation.thin", &json!({"tracks": [t.0], "param": "volume", "start": 600_000, "end": 900_000})).unwrap();
    assert_eq!(r["removed"], 29);
    let after = pts(&e, t, &AutoParam::Volume);
    assert_eq!(after[..4], hand[..]);
    assert_eq!(after[4..], [(600_000, -6.0), (900_000, -6.0)]);
}

#[test]
fn range_works_on_a_plugin_lane() {
    let mut e = Engine::default();
    let t = TrackId(e.execute("track.new", &json!({"count": 1})).unwrap()["tracks"][0].as_u64().unwrap());
    let lane = "plugin:a:threshold";
    for at in (0..=100).map(|i| i * 10) {
        e.execute("automation.set_point", &json!({"track": t.0, "param": lane, "at": at, "value": 0.5})).unwrap();
    }
    let p = AutoParam::parse(lane).unwrap();
    e.execute("automation.thin", &json!({"tracks": [t.0], "param": lane, "start": 200, "end": 800})).unwrap();
    let xs: Vec<i64> = pts(&e, t, &p).iter().map(|x| x.0).collect();
    assert_eq!(xs.len(), 2 + 20 + 20);
    assert!(xs.contains(&200) && xs.contains(&800) && !xs.contains(&500));
}

#[test]
fn range_undo_restores_exactly() {
    let (mut e, t, _) = ranged();
    let before = snapshot(&e, t);
    e.execute("automation.thin", &json!({"tracks": [t.0], "start": 600_000, "end": 900_000})).unwrap();
    assert_ne!(snapshot(&e, t), before);
    assert!(e.undo());
    assert_eq!(snapshot(&e, t), before);
}

#[test]
fn reversed_range_is_normalized_like_range_param() {
    let (mut e, t, _) = ranged();
    let r = e.execute("automation.thin", &json!({"tracks": [t.0], "start": 900_000, "end": 600_000})).unwrap();
    assert_eq!(r["removed"], 29);
}

#[test]
fn end_or_length_without_start_is_bad_params() {
    for extra in [json!({"end": 700_000}), json!({"length": 100}), json!({"start": null, "end": 700_000})] {
        let (mut e, t, _) = ranged();
        let before = snapshot(&e, t);
        let mut p = json!({"tracks": [t.0], "param": "volume"});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(e.execute("automation.thin", &p).is_err(), "{p}");
        assert_eq!(snapshot(&e, t), before);
    }
}

#[test]
fn null_bounds_mean_the_whole_lane() {
    let (mut e, t, _) = ranged();
    e.execute("automation.thin", &json!({"tracks": [t.0], "start": null, "end": null, "length": null})).unwrap();
    assert_eq!(pts(&e, t, &AutoParam::Volume).len(), 2);
}

#[test]
fn start_with_null_end_is_a_point_range() {
    let (mut e, t, _) = ranged();
    let r = e.execute("automation.thin", &json!({"tracks": [t.0], "start": 600_000, "end": null})).unwrap();
    assert_eq!(r["removed"], 0);
}
