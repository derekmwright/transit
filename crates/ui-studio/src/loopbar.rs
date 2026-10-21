//! The loop strip: a thin band at the top of the arrangement ruler. Dragging empty strip makes a
//! loop region, dragging an edge resizes it, dragging the body moves it, double-clicking turns
//! looping on or off. The region lives in the session (`edit.loop_range`, `edit.loop_playback`);
//! a gesture is only held here while it runs and lands as one `transport.loop` command on release.

use crate::StudioApp;
use crate::arrange::Map;
use crate::theme::TOKENS;
use egui::{CursorIcon, Rect, Sense, Stroke, StrokeKind, Ui, pos2};
use serde_json::json;
use soundcraft_model::Session;
use soundcraft_time::{Range, Samples};

/// Height of the strip, at the top of the ruler (points).
pub const STRIP_H: f32 = 8.0;
/// How close (points) to a region edge a press grabs the edge instead of the body.
pub const HANDLE: f32 = 6.0;

/// What a press on the strip took hold of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    /// Empty strip: a new region from `anchor` to the pointer.
    Create { anchor: Samples },
    /// The left edge; `end` stays put.
    Left { end: Samples },
    /// The right edge; `start` stays put.
    Right { start: Samples },
    /// The body, pressed at `press`; the region keeps its length.
    Body { start: Samples, end: Samples, press: Samples },
}

/// A gesture in progress: the grab and the region it currently describes (empty: none yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopDrag {
    pub grab: Grab,
    pub range: Range,
}

/// The part of a region under a press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Left,
    Right,
    Body,
    Outside,
}

/// Which part of a region spanning `xl..xr` the point `x` is on. Edges win within `handle` points
/// (the nearer one when the region is narrow).
pub fn hit(xl: f32, xr: f32, x: f32, handle: f32) -> Part {
    let (dl, dr) = ((x - xl).abs(), (x - xr).abs());
    if dl <= handle && dl <= dr {
        Part::Left
    } else if dr <= handle {
        Part::Right
    } else if x > xl && x < xr {
        Part::Body
    } else {
        Part::Outside
    }
}

/// The region a gesture describes with the pointer at `at`; `snap` rounds a position to the grid.
/// Dragging an edge past the other one swaps them; a body never moves before zero.
pub fn drag_range(grab: Grab, at: Samples, snap: impl Fn(Samples) -> Samples) -> Range {
    let r = match grab {
        Grab::Create { anchor } => Range::new(snap(anchor), snap(at)),
        Grab::Left { end } => Range::new(snap(at), end),
        Grab::Right { start } => Range::new(start, snap(at)),
        Grab::Body { start, end, press } => {
            let s = snap(start.saturating_add(at.saturating_sub(press))).max(0);
            Range { start: s, end: s.saturating_add(end.saturating_sub(start)) }
        }
    };
    Range { start: r.start.max(0), end: r.end.max(0) }
}

/// Remove the loop region (an empty pair clears it); one undo step.
pub(crate) fn clear(app: &mut StudioApp) {
    let _ = app.run("transport.loop", json!({"start": 0, "end": 0}));
}

/// Draw the strip and take its input.
pub(crate) fn strip(app: &mut StudioApp, ui: &mut Ui, s: &Session, map: Map, strip: Rect) {
    let resp = ui.interact(strip, ui.id().with("loop_strip"), Sense::click_and_drag());
    let region = s.edit.loop_range;
    let enabled = s.edit.loop_playback;
    // Edges land on the nearest grid line (the one automation uses); Shift drags freely.
    let free = ui.input(|i| i.modifiers.shift);
    let snap = |at: Samples| if free { at.max(0) } else { crate::automation::snap(s, map.spp, at) };
    let part_at = |x: f32| {
        if region.is_empty() { Part::Outside } else { hit(map.x(region.start), map.x(region.end), x, HANDLE) }
    };
    if resp.drag_started_by(egui::PointerButton::Primary)
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        let at = map.at(press.x);
        let grab = match part_at(press.x) {
            Part::Left => Grab::Left { end: region.end },
            Part::Right => Grab::Right { start: region.start },
            Part::Body => Grab::Body { start: region.start, end: region.end, press: at },
            Part::Outside => Grab::Create { anchor: at },
        };
        app.arrange.loop_drag = Some(LoopDrag { grab, range: region });
    }
    if let (Some(d), Some(pos)) = (app.arrange.loop_drag.as_mut(), resp.interact_pointer_pos())
        && resp.dragged()
    {
        d.range = drag_range(d.grab, map.at(pos.x.clamp(strip.min.x, strip.max.x)), snap);
    }
    if resp.drag_stopped()
        && let Some(d) = app.arrange.loop_drag.take()
        && !d.range.is_empty()
        && d.range != region
    {
        let mut p = json!({"start": d.range.start, "end": d.range.end});
        if matches!(d.grab, Grab::Create { .. }) {
            p["enabled"] = json!(true);
        }
        let _ = app.run("transport.loop", p);
    }
    if resp.double_clicked() {
        let _ = app.run("options.loop_playback", json!({"value": !enabled}));
    }
    resp.context_menu(|ui| {
        let item = ui.add_enabled(!region.is_empty(), egui::Button::new("Clear loop"));
        app.arrange.menu.push(crate::mixer::MenuItem::at(ui, "Clear loop".into(), String::new(), item.rect));
        if item.clicked() {
            clear(app);
            ui.close();
        }
    });
    if let Some(pos) = resp.hover_pos().filter(|_| app.arrange.loop_drag.is_none()) {
        match part_at(pos.x) {
            Part::Left | Part::Right => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
            Part::Body => ui.ctx().set_cursor_icon(CursorIcon::Grab),
            Part::Outside => {}
        }
    }

    // Paint from the session, except while a gesture runs: then its region.
    let shown = app.arrange.loop_drag.map_or(region, |d| d.range);
    let p = ui.painter().with_clip_rect(strip);
    p.rect_filled(strip, 0.0, TOKENS.lane);
    if !shown.is_empty() {
        let r =
            Rect::from_min_max(pos2(map.x(shown.start), strip.min.y + 1.0), pos2(map.x(shown.end).max(map.x(shown.start) + 1.0), strip.max.y - 1.0));
        let (fill, edge) = if enabled { (TOKENS.accent, TOKENS.text) } else { (TOKENS.accent.gamma_multiply(0.3), TOKENS.text_dim) };
        p.rect_filled(r, crate::theme::RADIUS_SM, fill);
        p.rect_stroke(r, crate::theme::RADIUS_SM, Stroke::new(1.0, edge.gamma_multiply(0.6)), StrokeKind::Inside);
        for x in [r.min.x, r.max.x] {
            p.line_segment([pos2(x, r.min.y), pos2(x, r.max.y)], Stroke::new(2.0, edge));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snap to multiples of 100.
    fn s(at: Samples) -> Samples {
        ((at as f64 / 100.0).round() as Samples * 100).max(0)
    }

    #[test]
    fn create_snaps_both_ends_and_orders_them() {
        assert_eq!(drag_range(Grab::Create { anchor: 130 }, 480, s), Range::new(100, 500));
        assert_eq!(drag_range(Grab::Create { anchor: 480 }, 130, s), Range::new(100, 500), "dragging backwards");
        assert_eq!(drag_range(Grab::Create { anchor: 130 }, -900, s), Range::new(0, 100), "clamped at zero");
        assert!(drag_range(Grab::Create { anchor: 130 }, 149, s).is_empty(), "within one step: no region");
    }

    #[test]
    fn resizing_an_edge_keeps_the_other_and_swaps_on_crossing() {
        let left = Grab::Left { end: 800 };
        assert_eq!(drag_range(left, 290, s), Range::new(300, 800));
        assert_eq!(drag_range(left, 1010, s), Range::new(800, 1000), "left edge dragged past the right end");
        assert_eq!(drag_range(left, -50, s), Range::new(0, 800));
        let right = Grab::Right { start: 300 };
        assert_eq!(drag_range(right, 910, s), Range::new(300, 900));
        assert_eq!(drag_range(right, 90, s), Range::new(100, 300), "right edge dragged past the left start");
        assert!(drag_range(right, 300, s).is_empty());
        // Shift: no snapping (the caller passes a plain clamp).
        assert_eq!(drag_range(right, 913, |at| at.max(0)), Range::new(300, 913));
    }

    #[test]
    fn moving_the_body_keeps_its_length_and_stops_at_zero() {
        let body = Grab::Body { start: 300, end: 800, press: 500 };
        assert_eq!(drag_range(body, 740, s), Range::new(500, 1000));
        assert_eq!(drag_range(body, 500, s), Range::new(300, 800), "no movement");
        assert_eq!(drag_range(body, 100, s), Range::new(0, 500), "clamped at zero, length kept");
        assert_eq!(drag_range(body, -5000, s), Range::new(0, 500));
    }

    #[test]
    fn a_press_picks_the_edge_the_body_or_empty_strip() {
        assert_eq!(hit(100.0, 300.0, 103.0, HANDLE), Part::Left);
        assert_eq!(hit(100.0, 300.0, 95.0, HANDLE), Part::Left, "just outside the edge still grabs it");
        assert_eq!(hit(100.0, 300.0, 297.0, HANDLE), Part::Right);
        assert_eq!(hit(100.0, 300.0, 200.0, HANDLE), Part::Body);
        assert_eq!(hit(100.0, 300.0, 50.0, HANDLE), Part::Outside);
        assert_eq!(hit(100.0, 300.0, 400.0, HANDLE), Part::Outside);
        assert_eq!(hit(100.0, 108.0, 106.0, HANDLE), Part::Right, "a narrow region: the nearer edge");
    }

    #[test]
    fn the_grid_snap_rounds_to_the_visible_grid_and_never_below_zero() {
        let sess = Session::default();
        let a = crate::automation::snap(&sess, 100.0, 123_457);
        assert_eq!(crate::automation::snap(&sess, 100.0, a), a, "snapping is idempotent");
        assert_eq!(crate::automation::snap(&sess, 100.0, -400), 0);
    }
}
