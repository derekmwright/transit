//! Transport commands use the same cursor as timeline/ruler clicks, without audio hardware.

use serde_json::json;
use transit_ui_studio::{Services, StudioApp};

fn command(app: &mut StudioApp, ctx: &egui::Context, id: &str, params: serde_json::Value) {
    app.run(id, params).unwrap();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        app.logic(ui.ctx());
        app.ui(ui);
    });
    output.textures_delta.clear();
}

#[test]
fn stop_returns_to_each_user_set_position_then_zero() {
    for source in ["timeline", "ruler", "locate"] {
        let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let ctx = egui::Context::default();
        let start = 96_000;
        match source {
            "timeline" => app.click_empty(None, start),
            "ruler" => app.click_ruler(start),
            _ => command(&mut app, &ctx, "transport.locate", json!({"at": start})),
        }
        let history = app.run("engine.history", json!({})).unwrap();
        command(&mut app, &ctx, "transport.play", json!({}));
        assert!(app.is_playing());
        assert!(app.position() > start, "playback advances from {source}");
        command(&mut app, &ctx, "transport.stop", json!({}));
        assert!(!app.is_playing());
        assert_eq!(app.position(), start, "first Stop returns to {source}");
        command(&mut app, &ctx, "transport.stop", json!({}));
        assert_eq!(app.position(), 0, "second Stop after {source} returns to zero");
        assert_eq!(app.cursor(), 0);
        assert_eq!(app.engine.session().edit.playhead, 0);
        assert_eq!(app.run("engine.history", json!({})).unwrap(), history);
    }
}

#[test]
fn stop_returns_to_zero_when_no_cursor_was_set() {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let ctx = egui::Context::default();
    command(&mut app, &ctx, "transport.play", json!({}));
    assert!(app.is_playing());
    assert!(app.position() > 0);
    for _ in 0..2 {
        command(&mut app, &ctx, "transport.stop", json!({}));
        assert!(!app.is_playing());
        assert_eq!(app.position(), 0);
        assert_eq!(app.cursor(), 0);
    }
    assert!(app.engine.undo_history().is_empty());
}

fn ui_frame(app: &mut StudioApp, ctx: &egui::Context, time: f64, events: Vec<egui::Event>) -> egui::FullOutput {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 800.0))),
        time: Some(time),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        app.logic(ui.ctx());
        app.ui(ui);
    });
    output.textures_delta.clear();
    output
}

#[test]
fn double_click_in_one_frame_stops_then_returns_to_zero() {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let ctx = egui::Context::default();
    for frame in 0..3 {
        ui_frame(&mut app, &ctx, f64::from(frame) / 60.0, Vec::new());
    }
    app.click_ruler(96_000);
    command(&mut app, &ctx, "transport.play", json!({}));
    assert!(app.is_playing());
    let r = app.arrange.layout.transport["stop"];
    let pos = egui::pos2((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
    let mut events = vec![egui::Event::PointerMoved(pos)];
    for pressed in [true, false, true, false] {
        events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
    }
    ui_frame(&mut app, &ctx, 1.0, events);
    ui_frame(&mut app, &ctx, 1.1, Vec::new());
    assert!(!app.is_playing());
    assert_eq!(app.position(), 0);
    assert!(app.engine.transport_requests.is_empty());
}

#[test]
fn stop_tooltip_explains_both_returns() {
    fn explains(shape: &egui::Shape) -> bool {
        match shape {
            egui::Shape::Text(text) => text.galley.text().contains("Stop: back to start position; again: to the beginning"),
            egui::Shape::Vec(shapes) => shapes.iter().any(explains),
            _ => false,
        }
    }

    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let ctx = egui::Context::default();
    for frame in 0..3 {
        ui_frame(&mut app, &ctx, f64::from(frame), Vec::new());
    }
    ctx.all_styles_mut(|s| s.interaction.tooltip_delay = 0.0);
    let r = app.arrange.layout.transport["stop"];
    let pos = egui::pos2((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
    let mut found = false;
    for frame in 3..8 {
        let out = ui_frame(&mut app, &ctx, f64::from(frame), vec![egui::Event::PointerMoved(pos)]);
        found |= out.shapes.iter().any(|s| explains(&s.shape));
    }
    assert!(found, "the rendered Stop tooltip must explain both actions");
}
