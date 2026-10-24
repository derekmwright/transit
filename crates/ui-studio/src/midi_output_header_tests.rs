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

fn instrument_rig() -> (StudioApp, egui::Context) {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let t = app.run("track.new", json!({"kind": "instrument", "name": "Keys"})).unwrap()["tracks"][0].as_u64().unwrap();
    app.run("midi.output", json!({"track": t, "port": {"backend": "fake", "name": "Synth OUT", "ordinal": 0, "id": ""}, "channel": 2})).unwrap();
    (app, egui::Context::default())
}

/// The last frame's text rects and chip dots (the dot is the only 4 px radius circle).
fn shapes(app: &mut StudioApp, ctx: &egui::Context, header_width: f32) -> (Vec<(String, Rect)>, Vec<Rect>) {
    app.ui.header_width = header_width;
    let mut last = (Vec::new(), Vec::new());
    for i in 0..3u32 {
        let mut raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
            time: Some(f64::from(i) / 4.0),
            ..Default::default()
        };
        app.raw_input_hook(&mut raw);
        let mut out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        out.textures_delta.clear();
        let mut texts = Vec::new();
        let mut dots = Vec::new();
        for s in &out.shapes {
            match &s.shape {
                egui::Shape::Text(t) => texts.push((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                egui::Shape::Circle(c) if c.radius == 4.0 => dots.push(Rect::from_center_size(c.center, vec2(8.0, 8.0))),
                _ => {}
            }
        }
        last = (texts, dots);
    }
    last
}

#[test]
fn the_chip_never_lands_on_the_subtitle() {
    for width in [160.0, 200.0, 236.0, 300.0] {
        let (mut app, ctx) = instrument_rig();
        let (texts, dots) = shapes(&mut app, &ctx, width);
        let subtitle = texts.iter().find(|(t, _)| t == "Subtractive Synth").unwrap_or_else(|| panic!("subtitle at {width}: {texts:?}")).1;
        for d in &dots {
            assert!(!d.intersects(subtitle), "dot {d:?} overlaps subtitle {subtitle:?} at {width}");
        }
        for (t, r) in texts.iter().filter(|(t, _)| t.contains("Synth OUT")) {
            assert!(!r.intersects(subtitle), "{t} {r:?} overlaps subtitle {subtitle:?} at {width}");
        }
        if width >= 236.0 {
            assert_eq!(dots.len(), 1, "the dot is drawn when it fits, at {width}");
        }
    }
    // The narrow header draws no chip shape at all.
    let (mut app, ctx) = instrument_rig();
    assert!(shapes(&mut app, &ctx, 160.0).1.is_empty());
}

#[test]
fn right_click_on_the_chip_opens_the_track_menu() {
    let (mut app, ctx) = rig(true);
    app.ui.header_width = 236.0;
    let shown = run(&mut app, &ctx, 0, 3, Vec::new());
    let chip = shown.iter().find(|(t, _)| t == "Synth OUT · Ch 2").expect("the chip").1 - vec2(20.0, 0.0);
    let right = vec![
        egui::Event::PointerMoved(chip),
        egui::Event::PointerButton { pos: chip, button: egui::PointerButton::Secondary, pressed: true, modifiers: egui::Modifiers::NONE },
        egui::Event::PointerButton { pos: chip, button: egui::PointerButton::Secondary, pressed: false, modifiers: egui::Modifiers::NONE },
    ];
    let shown = run(&mut app, &ctx, 3, 3, right);
    assert!(shown.iter().any(|(t, _)| t == "Delete Track"), "the track menu opened: {shown:?}");
    assert!(!shown.iter().any(|(t, _)| t == "Instrument (internal)"), "the picker did not open: {shown:?}");
}

#[test]
fn only_the_external_track_in_a_mixed_session_draws_a_chip() {
    let (mut app, ctx) = rig(true);
    app.run("track.new", json!({"kind": "midi", "name": "Pad"})).unwrap();
    app.ui.header_width = 300.0;
    let shown: Vec<String> = run(&mut app, &ctx, 0, 3, Vec::new()).into_iter().map(|(t, _)| t).collect();
    assert_eq!(shown.iter().filter(|t| t.contains("Synth OUT")).count(), 1, "{shown:?}");
    assert!(shown.iter().any(|t| t == "Pad"), "{shown:?}");
    let (_, dots) = shapes(&mut app, &ctx, 300.0);
    assert_eq!(dots.len(), 1, "the Internal header draws no dot");
}
