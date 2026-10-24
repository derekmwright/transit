//! Issue #69, item 1: the arranger track header shows an external MIDI destination chip.

use crate::{Services, StudioApp};
use egui::{Pos2, Rect, vec2};
use serde_json::json;

fn rig(external: bool) -> (StudioApp, egui::Context) {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let t = app.run("track.new", json!({"kind": "midi", "name": "Lead"})).unwrap()["tracks"][0].as_u64().unwrap();
    if external {
        app.run("midi.output", json!({"track": t, "port": {"backend": "fake", "name": "Synth OUT", "ordinal": 0, "id": ""}, "channel": 2})).unwrap();
    }
    (app, egui::Context::default())
}

fn texts(app: &mut StudioApp, ctx: &egui::Context, header_width: f32) -> Vec<String> {
    app.ui.header_width = header_width;
    run(app, ctx, 0, 3, Vec::new()).into_iter().map(|(t, _)| t).collect()
}

/// Runs `n` frames from frame number `from`; `events` go in the first. Returns the last frame's
/// text with its center.
fn run(app: &mut StudioApp, ctx: &egui::Context, from: u32, n: u32, events: Vec<egui::Event>) -> Vec<(String, Pos2)> {
    let mut last = Vec::new();
    for i in from..from + n {
        let mut raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
            time: Some(f64::from(i) / 4.0),
            events: if i == from { events.clone() } else { Vec::new() },
            ..Default::default()
        };
        app.raw_input_hook(&mut raw);
        let mut out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        out.textures_delta.clear();
        last = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2())),
                _ => None,
            })
            .collect();
    }
    last
}

#[test]
fn an_external_track_header_shows_port_and_channel_beside_its_name() {
    let (mut app, ctx) = rig(true);
    let shown = texts(&mut app, &ctx, 300.0);
    assert!(shown.iter().any(|t| t == "Synth OUT · Ch 2"), "{shown:?}");
    assert!(shown.iter().any(|t| t == "Lead"), "the name is still drawn: {shown:?}");
}

#[test]
fn an_internal_track_header_shows_no_destination() {
    let (mut app, ctx) = rig(false);
    let shown = texts(&mut app, &ctx, 300.0);
    assert!(!shown.iter().any(|t| t.contains("Ch ")), "{shown:?}");
}

#[test]
fn a_narrow_header_hides_the_label_and_keeps_the_dot() {
    let (mut app, ctx) = rig(true);
    let shown = texts(&mut app, &ctx, 160.0);
    assert!(!shown.iter().any(|t| t.contains("Synth OUT")), "{shown:?}");
    assert!(shown.iter().any(|t| t == "Lead"), "{shown:?}");
}

#[test]
fn the_header_chip_opens_the_picker_and_sends_midi_output() {
    let (mut app, ctx) = rig(true);
    app.ui.header_width = 236.0;
    let shown = run(&mut app, &ctx, 0, 3, Vec::new());
    let chip = shown.iter().find(|(t, _)| t == "Synth OUT · Ch 2").expect("the chip").1;
    let click = |pos| {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE },
        ]
    };
    let shown = run(&mut app, &ctx, 3, 3, click(chip - vec2(20.0, 0.0)));
    let internal = shown.iter().find(|(t, _)| t == "Instrument (internal)").expect("the picker opened from the header").1;
    run(&mut app, &ctx, 6, 3, click(internal));
    let track = app.engine.session().tracks.iter().find(|t| t.name == "Lead").unwrap().id;
    assert!(app.engine.session().track(track).unwrap().midi_output.is_internal(), "the choice ran midi.output");
}
