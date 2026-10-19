//! Merged gestures and `squash_undo` at and below `history_limit`.

use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_model::{ChannelFormat, TrackId, TrackKind};

fn setup(fill: usize) -> (Engine, TrackId) {
    let mut e = Engine::default();
    let t = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    for i in 0..fill {
        let pan = if i % 2 == 0 { 0.5 } else { -0.5 };
        e.execute("mix.pan", &json!({"track": t.0, "pan": pan})).unwrap();
    }
    (e, t)
}

fn vol(e: &Engine, t: TrackId) -> f32 {
    e.session().track(t).unwrap().mixer.volume_db
}

fn gesture(e: &mut Engine, t: TrackId, n: usize) {
    for i in 0..n {
        e.execute_merged("mix.volume", &json!({"track": t.0, "db": -0.1 * (i as f64 + 1.0)}), "fader").unwrap();
    }
    e.end_merge();
}

fn check(fill: usize, n: usize, limit: usize) {
    let (mut e, t) = setup(0);
    e.history_limit = limit;
    for i in 0..fill {
        e.execute("mix.pan", &json!({"track": t.0, "pan": if i % 2 == 0 { 0.5 } else { -0.5 }})).unwrap();
    }
    let before = e.undo_history().len();
    let last = (-0.1 * n as f64) as f32;
    gesture(&mut e, t, n);
    assert_eq!(e.undo_history().len(), (before + 1).min(limit), "fill {fill} n {n}");
    assert!((vol(&e, t) - last).abs() < 1e-4);
    assert!(e.undo());
    assert_eq!(vol(&e, t), 0.0, "one undo restores the pre-gesture state");
    assert!(e.redo());
    assert!((vol(&e, t) - last).abs() < 1e-4);
}

#[test]
fn gesture_at_the_cap_is_one_step() {
    check(200, 50, 200);
}

#[test]
fn gesture_below_the_cap_is_one_step() {
    check(10, 50, 200);
}

#[test]
fn gesture_longer_than_the_limit_is_still_one_step() {
    // The first command of a gesture may push the oldest step off a full history, but the merged
    // commands after it never do, so the pre-gesture snapshot survives however long the drag runs.
    check(200, 250, 200);
}

#[test]
fn squash_undo_at_the_cap_folds_the_right_steps() {
    let (mut e, t) = setup(200);
    assert_eq!(e.undo_history().len(), 200);
    for db in [-1.0, -2.0, -3.0] {
        e.execute("mix.volume", &json!({"track": t.0, "db": db})).unwrap();
    }
    assert_eq!(e.undo_history().len(), 200);
    e.execute("edit.squash_undo", &json!({"n": 3, "label": "Take"})).unwrap();
    assert_eq!((e.undo_history().len(), e.undo_label()), (198, Some("Take")));
    e.undo();
    assert_eq!(vol(&e, t), 0.0);
}
