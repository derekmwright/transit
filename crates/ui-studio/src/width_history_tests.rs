//! Recording and device-boundary regressions. Stand-in input counts, synthetic egui events,
//! and pure capture planning exercise the real dispatch paths without opening audio devices.

use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::{ChannelFormat, Route, TrackId};

const HISTORY_REASON: &str = "Stop recording before using Undo or Redo.";

fn app(inputs: usize) -> StudioApp {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.audio_devices.fake = crate::audio_devices::fake_from_json(&json!({"systems": [{
        "name": "Test", "is_default": true,
        "inputs": [{"name": "Synthetic input", "is_default": true, "max_channels": inputs}]
    }]}))
    .unwrap();
    assert_eq!(app.device_inputs(), inputs);
    app
}

fn track(app: &mut StudioApp, format: &str, input: &str) -> TrackId {
    let id = TrackId(app.add_audio_track(format).unwrap()["tracks"][0].as_u64().unwrap());
    app.run("track.input", json!({"tracks": [id.0], "input": input})).unwrap();
    app.run("mix.record_arm", json!({"tracks": [id.0], "value": true})).unwrap();
    id
}

fn width(app: &mut StudioApp, ids: &[TrackId], format: &str) -> Result<Value, String> {
    app.run("track.change_width", json!({"tracks": ids.iter().map(|id| id.0).collect::<Vec<_>>(), "format": format}))
}

fn history(app: &mut StudioApp) -> Value {
    app.engine.execute("engine.history", &json!({})).unwrap()
}

fn capture_inputs(app: &StudioApp) -> Vec<Vec<usize>> {
    soundcraft_engine::io::plan_takes(&app.engine, std::path::Path::new(env!("CARGO_MANIFEST_DIR")), app.device_inputs())
        .into_iter()
        .map(|(_, inputs, _)| inputs)
        .collect()
}

#[test]
fn stereo_conversion_refuses_unpaired_or_missing_inputs_atomically() {
    for (inputs, input) in [(3, 3), (3, 4), (2, 3), (2, 8), (1, 1), (0, 1), (4, 5)] {
        let mut app = app(inputs);
        // A valid track before the invalid one must not be partially converted either.
        let valid = track(&mut app, "mono", "In 1");
        let invalid = track(&mut app, "mono", &format!("In {input}"));
        let ids = if inputs >= 2 { vec![valid, invalid] } else { vec![invalid] };
        let before = app.engine.session_arc();
        let steps = history(&mut app);
        let error = width(&mut app, &ids, "stereo").unwrap_err();
        assert!(error.contains(&format!("Input {input} has no stereo pair on this device; pick a stereo input first")), "{error}");
        assert_eq!(app.status, error);
        assert!(std::sync::Arc::ptr_eq(&before, &app.engine.session_arc()), "{inputs} inputs, In {input}");
        assert_eq!(history(&mut app), steps);
        if inputs == 3 && input == 3 {
            assert_eq!(capture_inputs(&app), [vec![0], vec![2]], "capture must still use only input 3 for the rejected track");
        }
        if inputs >= 2 {
            app.set_input(invalid, "In 2").unwrap();
            width(&mut app, &ids, "stereo").unwrap();
            assert_eq!(app.engine.session().track(invalid).unwrap().mixer.input, Route::Hardware("In 1-2".into()));
        }
    }
}

#[test]
fn width_conversion_and_input_choices_use_only_available_channels() {
    for (inputs, first, last) in [(2, 1, 2), (3, 2, 2), (4, 3, 4), (4, 4, 4), (6, 6, 6)] {
        let mut app = app(inputs);
        let t = track(&mut app, "mono", &format!("In {first}"));
        width(&mut app, &[t], "stereo").unwrap();
        let tr = app.engine.session().track(t).unwrap();
        let mapped = format!("In {}-{last}", last - 1);
        assert_eq!(tr.mixer.input, Route::Hardware(mapped.clone()));
        assert!(crate::mixer::input_choices(app.engine.session(), tr, inputs).iter().any(|(_, value)| value == &mapped));
        assert_eq!(capture_inputs(&app), [vec![last - 2, last - 1]]);
    }
    for inputs in [0, 1] {
        let mut app = app(inputs);
        let t = track(&mut app, "stereo", "In 1-2");
        assert!(crate::mixer::input_choices(app.engine.session(), app.engine.session().track(t).unwrap(), inputs).is_empty());
        // Default (implicit input 1) routes must obey the same boundary.
        app.engine.session_mut().track_mut(t).unwrap().mixer.input = Route::None;
        assert!(width(&mut app, &[t], "stereo").is_err());
    }
    let mut app = app(2);
    let t = track(&mut app, "stereo", "In 3-4");
    let before = app.engine.session_arc();
    assert!(width(&mut app, &[t], "mono").unwrap_err().contains("Input 3 is unavailable on this device"));
    assert!(std::sync::Arc::ptr_eq(&before, &app.engine.session_arc()));
}

fn history_setup(redo: bool) -> (StudioApp, TrackId) {
    let mut app = app(2);
    let t = track(&mut app, "mono", "In 2");
    width(&mut app, &[t], "stereo").unwrap();
    if redo {
        app.run("edit.undo", json!({})).unwrap();
    }
    app.tracks_selected_by_header = true;
    app.engine.transport.recording = true;
    (app, t)
}

#[test]
fn history_commands_and_control_refuse_recording_without_mutation() {
    for redo in [false, true] {
        let (mut app, t) = history_setup(redo);
        let command = if redo { "edit.redo" } else { "edit.undo" };
        let before = app.engine.session_arc();
        let steps = history(&mut app);
        let writer_inputs = capture_inputs(&app);
        assert_eq!(app.run(command, json!({})).unwrap_err(), HISTORY_REASON);
        for method in ["engine.execute", "command"] {
            let (req, _) = crate::ControlRequest::new(method, json!({"command": command, "params": {}}));
            let result = crate::control::handle_for_test(&mut app, &egui::Context::default(), &req).unwrap();
            assert_eq!(result, json!({"ok": false, "error": HISTORY_REASON}));
        }
        assert_eq!(app.undo_to(0), 0, "Undo History cannot bypass the guard");
        assert_eq!(app.status, HISTORY_REASON);
        assert!(std::sync::Arc::ptr_eq(&before, &app.engine.session_arc()));
        assert_eq!(history(&mut app), steps, "both history stacks must be preserved");
        assert_eq!(capture_inputs(&app), writer_inputs);
        assert!(app.engine.transport.recording);
        assert!(app.tracks_selected_by_header);
        app.engine.transport.recording = false;
        app.run(command, json!({})).unwrap();
        let tr = app.engine.session().track(t).unwrap();
        assert_eq!(tr.format, if redo { ChannelFormat::Stereo } else { ChannelFormat::Mono });
        assert_eq!(tr.mixer.input, Route::Hardware(if redo { "In 1-2" } else { "In 2" }.into()));
    }
}

#[test]
fn history_shortcuts_are_no_ops_while_recording() {
    use crate::shortcuts::Action;
    for key in crate::shortcuts::TABLE.iter().filter(|key| matches!(key.action, Action::Undo | Action::Redo)) {
        let (mut app, _) = history_setup(key.action == Action::Redo);
        let before = app.engine.session_arc();
        let steps = history(&mut app);
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(key.mods),
                egui::Event::Key { key: key.key, physical_key: None, pressed: true, repeat: false, modifiers: key.mods },
            ],
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| crate::menus::shortcuts(&mut app, ui.ctx()));
        out.textures_delta.clear();
        assert_eq!(app.status, HISTORY_REASON, "{}", key.label);
        assert!(std::sync::Arc::ptr_eq(&before, &app.engine.session_arc()));
        assert_eq!(history(&mut app), steps);
    }
}

fn shape_has_text(shape: &egui::Shape, expected: &str) -> bool {
    match shape {
        egui::Shape::Text(text) => text.galley.text().contains(expected),
        egui::Shape::Vec(shapes) => shapes.iter().any(|s| shape_has_text(s, expected)),
        _ => false,
    }
}

/// Draw the production menu item, hover it, and check the actual rendered tooltip text.
fn disabled_tooltip(app: &mut StudioApp, reason: &str, mut item: impl FnMut(&mut StudioApp, &mut egui::Ui) -> egui::Response) {
    let ctx = egui::Context::default();
    ctx.all_styles_mut(|s| s.interaction.tooltip_delay = 0.0);
    let mut at = egui::Pos2::ZERO;
    let mut explained = false;
    for frame in 0..5 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 200.0))),
            time: Some(f64::from(frame)),
            events: vec![egui::Event::PointerMoved(at)],
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            let response = item(app, ui);
            assert!(!response.enabled());
            at = response.rect.center();
        });
        explained |= out.shapes.iter().any(|s| shape_has_text(&s.shape, reason));
        out.textures_delta.clear();
    }
    assert!(explained, "missing tooltip: {reason}");
}

#[test]
fn width_menu_explains_missing_stereo_pairs() {
    for (inputs, first) in [(3, 3), (2, 3), (1, 1), (0, 1)] {
        let mut app = app(inputs);
        let t = track(&mut app, "mono", &format!("In {first}"));
        let reason = format!("Input {first} has no stereo pair on this device; pick a stereo input first");
        disabled_tooltip(&mut app, &reason, |app, ui| crate::arrange::width_item(app, ui, t).unwrap());
    }
}

#[test]
fn history_menu_is_disabled_with_a_reason_and_recovers_after_recording() {
    for redo in [false, true] {
        let (mut app, _) = history_setup(redo);
        disabled_tooltip(&mut app, HISTORY_REASON, |app, ui| crate::menus::history_item(app, ui, redo));
        app.engine.transport.recording = false;
        let mut out = egui::Context::default().run_ui(egui::RawInput::default(), |ui| {
            assert!(crate::menus::history_item(&mut app, ui, redo).enabled());
        });
        out.textures_delta.clear();
    }
}
