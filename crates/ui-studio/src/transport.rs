//! The transport bar: Return to Zero, Stop, Play, Record, Cycle; the position in bars|beats and
//! min:sec; the tempo and meter; the main meter. Icons are Lucide SVGs ([`crate::icons`]).
//!
//! Stop returns to the existing cursor (normally the play-start position); Stop again while
//! stopped, or a double-click, returns to zero. Space remains Play/Stop, without this second-stop action.

use crate::StudioApp;
use crate::icons::{self, Icon};
use crate::theme::{TOKENS, TRANSPORT_H};
use egui::{Align2, Color32, Rect, Sense, Ui, pos2, vec2};
use serde_json::json;
use soundcraft_time::Samples;

/// A square transport button, including its click count.
fn button(app: &mut StudioApp, ui: &mut Ui, name: &str, which: Icon, on: bool, on_color: Color32) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(34.0, 28.0), Sense::click());
    let p = ui.painter();
    let bg = if on {
        on_color.gamma_multiply(0.35)
    } else if resp.hovered() {
        TOKENS.button_hover
    } else {
        TOKENS.button
    };
    p.rect_filled(r, crate::theme::RADIUS, bg);
    icons::paint_in(p, r, which, if on { on_color } else { TOKENS.text });
    app.arrange.layout.transport.insert(name.to_string(), [r.min.x, r.min.y, r.max.x, r.max.y]);
    app.arrange.layout.transport_icons.insert(name.to_string(), which.name().to_string());
    let tooltip = if name == "stop" { "Stop: back to start position; again: to the beginning" } else { name };
    resp.on_hover_text(tooltip)
}

/// `1. 1. 1. 000` (bar, beat, sixteenth, tick within the sixteenth).
pub fn bars_beats(app: &StudioApp, at: Samples) -> String {
    let s = app.engine.session();
    let bb = s.tempo.bar_beat_at(at.max(0), s.sample_rate);
    let sixteenth = soundcraft_time::TICKS_PER_QUARTER / 4;
    format!("{:>3}. {}. {}. {:03}", bb.bar, bb.beat, bb.tick / sixteenth + 1, bb.tick % sixteenth)
}

/// `0:00.000`.
pub fn min_sec(app: &StudioApp, at: Samples) -> String {
    let secs = at.max(0) as f64 / app.engine.session().sample_rate.as_f64().max(1.0);
    let m = (secs / 60.0).floor();
    format!("{}:{:06.3}", m as i64, secs - m * 60.0)
}

pub fn bar(app: &mut StudioApp, ui: &mut Ui) {
    egui::Panel::top("transport")
        .exact_size(TRANSPORT_H)
        .frame(egui::Frame::NONE.fill(TOKENS.panel_raised).inner_margin(egui::Margin::symmetric(crate::theme::SPACE as i8, 6)))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = crate::theme::SPACE / 2.0;
                let playing = app.is_playing();
                let recording = app.engine.transport.recording;
                let cycle = app.engine.session().edit.loop_playback;
                if button(app, ui, "rtz", Icon::Rewind, false, TOKENS.accent).clicked() {
                    let _ = app.run("transport.rtz", json!({}));
                }
                let stop = button(app, ui, "stop", Icon::Stop, false, TOKENS.accent);
                if stop.clicked() {
                    let _ = app.run("transport.stop", json!({}));
                    // Also works when both clicks arrive in one frame: the first Stop finishes
                    // any take before the second Stop requests Return to Zero.
                    if stop.double_clicked() && (playing || recording) {
                        let _ = app.run("transport.stop", json!({}));
                    }
                }
                if button(app, ui, "play", Icon::Play, playing && !recording, TOKENS.accent).clicked() {
                    let _ = app.run("transport.play", json!({}));
                }
                // A record count-in pulses the button on each beat instead of lighting it.
                let counting = app.count_in_label();
                let pulse = counting.is_some() && app.beat_phase().is_some_and(|(phase, _)| phase < 0.5);
                if button(app, ui, "record", Icon::Record, (recording && counting.is_none()) || pulse, TOKENS.record).clicked() {
                    let _ = app.run("transport.record", json!({}));
                }
                if button(app, ui, "cycle", Icon::Loop, cycle, TOKENS.loop_region).clicked() {
                    let _ = app.run("options.loop_playback", json!({"value": !cycle}));
                }
                crate::metronome::button(app, ui);
                ui.add_space(2.0 * crate::theme::SPACE);
                let at = app.position();
                match &counting {
                    Some(label) => readout(ui, &format!("Count-in {label}"), 128.0, TOKENS.record),
                    None => readout(ui, &bars_beats(app, at), 128.0, TOKENS.text),
                }
                readout(ui, &min_sec(app, at), 92.0, TOKENS.text_dim);
                ui.add_space(2.0 * crate::theme::SPACE);
                crate::tempo_edit::tempo_field(app, ui);
                crate::tempo_edit::meter_field(app, ui);
                ui.add_space(2.0 * crate::theme::SPACE);
                main_meter(app, ui);
                crate::load_readout::readout(app, ui);
                if let Some(hint) = app.record_hint() {
                    ui.add_space(crate::theme::SPACE);
                    ui.label(egui::RichText::new(hint).color(TOKENS.record));
                }
            });
        });
}

fn readout(ui: &mut Ui, text: &str, w: f32, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(w, 26.0), Sense::hover());
    ui.painter().rect_filled(r, crate::theme::RADIUS, TOKENS.bg);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, text, crate::theme::mono(ui.ctx(), 13.0), color);
}

fn main_meter(app: &mut StudioApp, ui: &mut Ui) {
    let (r, resp) = ui.allocate_exact_size(vec2(120.0, 16.0), Sense::click());
    if resp.on_hover_text("Click to clear the clip latch and the peak hold, Ctrl-click for every meter").clicked() {
        if ui.input(|i| i.modifiers.command || i.modifiers.ctrl) {
            app.reset_meters(None);
        } else {
            app.reset_main_meter();
        }
    }
    let p = ui.painter();
    p.rect_filled(r, crate::theme::RADIUS_SM, TOKENS.bg);
    for (i, lvl) in app.main_meter.level.iter().enumerate() {
        let h = (r.height() - 3.0) / 2.0;
        let y = r.min.y + 1.0 + i as f32 * (h + 1.0);
        let db = if *lvl > 0.0 { 20.0 * lvl.log10() } else { -90.0 };
        let k = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let bar = Rect::from_min_size(pos2(r.min.x + 1.0, y), vec2((r.width() - 2.0) * k, h));
        p.rect_filled(bar, 0.0, TOKENS.monitor);
    }
    // A clip latch is a small red cap at the right end of the bar, not a red bar.
    if app.main_meter.clip {
        p.rect_filled(Rect::from_min_max(pos2(r.max.x - crate::mixer::CLIP_CAP, r.min.y), r.max), 0.0, TOKENS.record);
    }
}
