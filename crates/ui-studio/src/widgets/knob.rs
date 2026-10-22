//! A rotary knob for a continuous plug-in parameter: a 270 degree arc, the value arc in the
//! accent token, the label below (the value while the pointer is over it or dragging).
//!
//! Drag up or down to turn (Shift for fine), double-click to reset. The widget only reports the
//! new value; the caller writes it, through the merged-command path, so a drag is one undo step.

use crate::theme::{RADIUS_SM, TOKENS};
use egui::{Align2, FontId, Pos2, Sense, Shape, Stroke, Ui, vec2};
use soundcraft_dsp::{ParamInfo, Unit};
use std::f32::consts::PI;

/// The arc starts at the lower left (135 degrees, y down) and sweeps clockwise.
const START: f32 = 0.75 * PI;
const SWEEP: f32 = 1.5 * PI;
/// Pixels of vertical drag for the full travel, and the divisor Shift applies.
const DRAG_PX: f32 = 180.0;
const FINE: f32 = 10.0;
/// Width of one knob cell and its dial diameter.
pub const CELL_W: f32 = 62.0;
const DIAL: f32 = 40.0;

/// The pointer angle of a normalized position (0..1).
pub fn angle(norm: f32) -> f32 {
    START + SWEEP * norm.clamp(0.0, 1.0)
}

/// The normalized position after dragging `dy` pixels (up is negative) from `norm`.
pub fn dragged(norm: f32, dy: f32, fine: bool) -> f32 {
    (norm - dy / DRAG_PX / if fine { FINE } else { 1.0 }).clamp(0.0, 1.0)
}

/// The parameter value at a normalized position: the taper honored, semitones and cents whole.
pub fn value_at(p: &ParamInfo, norm: f32) -> f32 {
    let v = p.from_normalized(norm);
    if matches!(p.unit, Unit::Semitones | Unit::Cents) { p.clamp(v.round()) } else { v }
}

/// Whether the dial's arc starts at its center (a range that spans zero, like a gain).
fn bipolar(p: &ParamInfo) -> bool {
    p.min < 0.0 && p.max > 0.0 && p.taper == soundcraft_dsp::Taper::Linear
}

fn arc(center: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let n = (((to - from).abs() / SWEEP * 48.0).ceil() as usize).max(2);
    (0..=n)
        .map(|i| {
            let a = from + (to - from) * i as f32 / n as f32;
            center + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

/// What a knob did this frame.
pub struct KnobOut {
    pub rect: egui::Rect,
    /// The new value, when the user turned or reset it.
    pub value: Option<f32>,
    /// The text the label slot shows: the value while hovered or dragged.
    pub text: String,
}

/// Draws the knob for `p` at `value` with `label` under it.
pub fn show(ui: &mut Ui, id: egui::Id, label: &str, p: &ParamInfo, value: f32) -> KnobOut {
    let (rect, _) = ui.allocate_exact_size(vec2(CELL_W, DIAL + 16.0), Sense::hover());
    let dial = egui::Rect::from_center_size(rect.center_top() + vec2(0.0, DIAL / 2.0), vec2(DIAL, DIAL));
    let r = ui.interact(dial, id, Sense::click_and_drag());
    let norm = p.to_normalized(value);
    let mut new = None;
    if r.dragged() {
        let (dy, fine) = ui.input(|i| (i.pointer.delta().y, i.modifiers.shift));
        let n = dragged(norm, dy, fine);
        if (n - norm).abs() > f32::EPSILON {
            new = Some(p.clamp(value_at(p, n)));
        }
    }
    let double = crate::builtin_editor::double_clicked(ui, &r);
    if double {
        new = Some(p.default);
    }
    let shown = new.unwrap_or(value);
    let norm = p.to_normalized(shown);
    let active = r.hovered() || r.dragged();
    let c = dial.center();
    let rad = DIAL / 2.0 - 3.0;
    let painter = ui.painter();
    painter.circle_filled(c, rad - 4.0, if active { TOKENS.button_hover } else { TOKENS.button });
    painter.add(Shape::line(arc(c, rad, START, START + SWEEP), Stroke::new(3.0, TOKENS.grid_bar)));
    let origin = if bipolar(p) { p.to_normalized(0.0) } else { 0.0 };
    if (norm - origin).abs() > 0.002 {
        let (a, b) = (angle(origin.min(norm)), angle(origin.max(norm)));
        painter.add(Shape::line(arc(c, rad, a, b), Stroke::new(3.0, TOKENS.accent)));
    }
    let a = angle(norm);
    let dir = vec2(a.cos(), a.sin());
    painter.line_segment([c + dir * (rad - 12.0), c + dir * (rad - 5.0)], Stroke::new(2.0, TOKENS.text));
    let text = if active { p.format(shown) } else { label.to_string() };
    if active {
        let slot = egui::Rect::from_min_max(egui::pos2(rect.min.x, dial.max.y + 2.0), rect.max);
        painter.rect_filled(slot, RADIUS_SM, TOKENS.panel);
    }
    painter.text(
        egui::pos2(rect.center().x, dial.max.y + 9.0),
        Align2::CENTER_CENTER,
        &text,
        FontId::proportional(10.5),
        if active { TOKENS.text } else { TOKENS.text_dim },
    );
    KnobOut { rect: dial, value: new, text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_dsp::plugin_info;

    fn p(plugin: &str, id: &str) -> &'static ParamInfo {
        plugin_info(plugin).unwrap().param(id).unwrap()
    }

    #[test]
    fn the_arc_spans_270_degrees_from_lower_left_to_lower_right() {
        assert!((angle(0.0) - 0.75 * PI).abs() < 1e-6);
        assert!((angle(1.0) - angle(0.0) - 1.5 * PI).abs() < 1e-6);
        assert!((angle(0.5) - 1.5 * PI).abs() < 1e-6, "midway points straight up");
        assert_eq!(angle(7.0), angle(1.0), "clamped");
    }

    #[test]
    fn dragging_up_raises_and_shift_is_ten_times_finer() {
        assert!(dragged(0.5, -18.0, false) > 0.5);
        assert!(dragged(0.5, 18.0, false) < 0.5);
        assert!((dragged(0.5, -18.0, false) - 0.6).abs() < 1e-6);
        assert!((dragged(0.5, -18.0, true) - 0.51).abs() < 1e-6);
        assert_eq!((dragged(0.9, -500.0, false), dragged(0.1, 500.0, false)), (1.0, 0.0));
    }

    #[test]
    fn value_and_position_map_through_the_taper() {
        let cutoff = p("subtractive_synth", "cutoff");
        assert!((value_at(cutoff, 0.0) - 20.0).abs() < 1e-3);
        assert!((value_at(cutoff, 1.0) - 20_000.0).abs() < 1.0);
        let mid = value_at(cutoff, 0.5);
        assert!((mid - (20.0f32 * 20_000.0).sqrt()).abs() < 1.0, "log taper: the middle is the geometric mean, got {mid}");
        let thr = p("compressor", "threshold");
        assert!((value_at(thr, 0.5) - -30.0).abs() < 1e-4, "linear");
        for n in [0.0, 0.25, 0.5, 1.0] {
            assert!((cutoff.to_normalized(value_at(cutoff, n)) - n).abs() < 1e-3);
        }
        let semis = p("subtractive_synth", "osc2_semitones");
        assert_eq!(value_at(semis, 0.51).fract(), 0.0, "semitones are whole");
    }

    #[test]
    fn a_range_spanning_zero_is_bipolar() {
        assert!(bipolar(p("eq_7band", "mid_gain")));
        assert!(!bipolar(p("compressor", "threshold")));
        assert!(!bipolar(p("compressor", "attack")));
    }
}
