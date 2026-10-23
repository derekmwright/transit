//! Fade and crossfade handles on audio clips (#63, Logic style).
//!
//! The corner hot zones live in [`crate::arrange::zone`] with the trim and loop zones. This module
//! holds what a fade gesture does: the pure mapping from the pointer to a fade length
//! ([`fade_len`]), from a vertical drag to a curvature ([`curvature_after`]), the commands one
//! gesture turns into ([`plan`]), the curve the clip draws ([`curve_points`], the same
//! `Fade::gain` the engine renders with), and the shape menu.
//!
//! Every gesture is one undo step: the commands run under one merge key (`composite`). Fades are
//! set through the engine's clip-targeted `edit.fades_create` (`clip`, `side`, `length`, `shape`,
//! `curvature`); a crossfade across an abutting boundary uses its range form (`ops::create_fades`).

use crate::StudioApp;
use crate::arrange::{DragKind, Gesture, Map, R4};
use crate::theme::CLIP_NAME_H;
use egui::{Color32, Pos2, Stroke, Ui, pos2};
use serde_json::{Value, json};
use soundcraft_model::{Clip, ClipContent, ClipId, Fade, FadeShape, Session, TrackId};
use soundcraft_time::Samples;

/// Vertical drag that bends a fade's curvature across its whole range (points).
pub const CURVE_RANGE_PX: f32 = 120.0;

/// A fade at a clip's start (in) or end (out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Side {
    In,
    Out,
}

impl Side {
    fn id(self) -> &'static str {
        match self {
            Side::In => "in",
            Side::Out => "out",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::In => "Fade In",
            Side::Out => "Fade Out",
        }
    }

    /// The fade of `c` on this side.
    pub fn of(self, c: &Clip) -> Fade {
        match self {
            Side::In => c.fade_in,
            Side::Out => c.fade_out,
        }
    }

    fn other(self) -> Side {
        match self {
            Side::In => Side::Out,
            Side::Out => Side::In,
        }
    }
}

/// The fade length the pointer at `at` asks for on a clip spanning `start..end`: from the clip
/// edge to the pointer. Negative when the pointer is past the clip's edge (outward).
pub fn fade_len(side: Side, start: Samples, end: Samples, at: Samples) -> Samples {
    match side {
        Side::In => at.saturating_sub(start),
        Side::Out => end.saturating_sub(at),
    }
}

/// The curvature after dragging `dy` points down from `start` (up bends the fade up, towards
/// full level sooner), clamped to the engine's -1..1.
pub fn curvature_after(start: f32, dy: f32) -> f32 {
    (start - dy / CURVE_RANGE_PX * 2.0).clamp(-1.0, 1.0)
}

/// Which fade's region (`in_w` / `out_w` points wide from the clip edges, below the name strip)
/// the point is in.
pub fn region_at(rect: R4, in_w: f32, out_w: f32, pos: Pos2) -> Option<Side> {
    if pos.y <= rect[1] + CLIP_NAME_H || pos.y > rect[3] || pos.x < rect[0] || pos.x > rect[2] {
        return None;
    }
    if in_w > 0.0 && pos.x - rect[0] <= in_w {
        Some(Side::In)
    } else if out_w > 0.0 && rect[2] - pos.x <= out_w {
        Some(Side::Out)
    } else {
        None
    }
}

/// The points of a fade's curve across `x0..x1` (the fade's extent on screen), between `bottom`
/// (silence) and `top` (unity): `n + 1` points, from the clip edge inward... for a fade-in that is
/// left to right with the gain rising; for a fade-out left to right with the gain falling.
pub fn curve_points(fade: Fade, side: Side, x0: f32, x1: f32, top: f32, bottom: f32, n: usize) -> Vec<Pos2> {
    let n = n.max(1);
    (0..=n)
        .map(|i| {
            let f = i as f32 / n as f32;
            let t = match side {
                Side::In => f,
                Side::Out => 1.0 - f,
            };
            pos2(x0 + (x1 - x0) * f, bottom - (bottom - top) * fade.gain(t))
        })
        .collect()
}

/// Two audio clips of one track that overlap: the left one (earlier start), the right one, and
/// the overlap `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlap {
    pub left: ClipId,
    pub right: ClipId,
    pub start: Samples,
    pub end: Samples,
}

fn is_audio(c: &Clip) -> bool {
    matches!(c.content, ClipContent::Audio { .. })
}

/// The overlap `clip` has with a neighbor on its track (audio clips only).
pub fn overlap(s: &Session, clip: ClipId) -> Option<Overlap> {
    let (track, c) = s.find_clip(clip)?;
    if !is_audio(c) {
        return None;
    }
    let t = s.track(track)?;
    t.clips().iter().filter(|o| o.id != c.id && is_audio(o)).find_map(|o| {
        let (l, r) = if (o.start, o.id.0) < (c.start, c.id.0) { (o, c) } else { (c, o) };
        let (start, end) = (r.start, l.end().min(r.end()));
        (start < end).then_some(Overlap { left: l.id, right: r.id, start, end })
    })
}

/// What a fade-handle drag turns into.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Nothing to do (the length is zero, or outward with no neighbor to cross into).
    None,
    /// Set the fades: (clip, side, fade) in order.
    Set(Vec<(ClipId, Side, Fade)>),
    /// Dragged outward across the boundary of an abutting neighbor: a crossfade over
    /// `start..end` on `track` (`edit.fades_create`'s range form).
    Cross { track: TrackId, start: Samples, end: Samples },
}

/// The commands for dragging `clip`'s `side` handle to ask for `len` (see [`fade_len`]). Dragging
/// a crossfade's edge (the clip overlaps a neighbor) sets both sides unless `alt`.
pub fn plan(s: &Session, clip: ClipId, side: Side, len: Samples, alt: bool) -> Plan {
    let Some((track, c)) = s.find_clip(clip) else { return Plan::None };
    if len > 0 {
        let fade = Fade { len: len.min(c.length), ..side.of(c) };
        let mut set = vec![(clip, side, fade)];
        if !alt && is_audio(c) {
            let partner = s.track(track).and_then(|t| {
                t.clips().iter().filter(|o| o.id != c.id && is_audio(o)).find(|o| match side {
                    Side::In => o.start < c.start && o.end() > c.start,
                    Side::Out => o.start > c.start && o.start < c.end(),
                })
            });
            if let Some(o) = partner {
                set.push((o.id, side.other(), Fade { len: len.min(o.length), ..side.other().of(o) }));
            }
        }
        return Plan::Set(set);
    }
    if len < 0 {
        let d = len.saturating_neg();
        let boundary = match side {
            Side::In => c.start,
            Side::Out => c.end(),
        };
        let abuts = s.track(track).is_some_and(|t| {
            t.clips().iter().any(|o| {
                o.id != c.id
                    && is_audio(o)
                    && match side {
                        Side::In => o.end() == boundary,
                        Side::Out => o.start == boundary,
                    }
            })
        });
        if abuts && is_audio(c) {
            return Plan::Cross { track, start: boundary.saturating_sub(d).max(0), end: boundary.saturating_add(d) };
        }
    }
    Plan::None
}

/// The `edit.fades_create` parameters that set `fade` on a clip's side.
fn set_params(clip: ClipId, side: Side, fade: Fade) -> Value {
    json!({"clip": clip.0, "side": side.id(), "length": fade.len, "shape": fade.shape.label(), "curvature": f64::from(fade.curvature.clamp(-1.0, 1.0))})
}

/// A fade gesture in progress, as it would land.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub clip: ClipId,
    pub side: Side,
    /// The fade the dragged clip would have (what the clip draws while dragging).
    pub fade: Fade,
    /// What releasing the pointer does.
    pub plan: Plan,
    /// A curvature drag (Alt) rather than a length drag.
    pub curve: bool,
}

/// The fade gesture from the pointer, if one is in progress.
pub(crate) fn drag(app: &StudioApp, ui: &Ui, map: Map) -> Option<Live> {
    let Some(Gesture::Clip { clip, kind, press, start, end, .. }) = app.arrange.gesture.clone() else { return None };
    let (pos, m) = ui.input(|i| (i.pointer.latest_pos(), i.modifiers));
    let pos = pos?;
    let s = app.engine.session();
    let c = s.find_clip(clip)?.1;
    let side = match kind {
        DragKind::FadeIn | DragKind::FadeCurveIn => Side::In,
        DragKind::FadeOut | DragKind::FadeCurveOut => Side::Out,
        _ => return None,
    };
    let base = side.of(c);
    if matches!(kind, DragKind::FadeCurveIn | DragKind::FadeCurveOut) {
        let fade = Fade { curvature: curvature_after(base.curvature, pos.y - press.y), ..base };
        return Some(Live { clip, side, fade, plan: Plan::Set(vec![(clip, side, fade)]), curve: true });
    }
    let at = map.at(pos.x);
    let at = if m.shift { at } else { crate::automation::snap_delta(s, map.spp, &map.grid, 0, at) };
    let len = fade_len(side, start, end, at);
    let plan = plan(s, clip, side, len, m.alt);
    let fade = match &plan {
        Plan::Set(v) => v.first().map_or(base, |x| x.2),
        _ => Fade { len: 0, ..base },
    };
    Some(Live { clip, side, fade, plan, curve: false })
}

/// Commit a fade gesture: one undo step.
pub(crate) fn commit(app: &mut StudioApp, live: &Live) {
    let plan = live.plan.clone();
    let changes = match &plan {
        Plan::None => false,
        Plan::Set(v) => {
            let s = app.engine.session();
            v.iter().any(|(c, side, f)| s.find_clip(*c).is_some_and(|x| side.of(x.1) != *f))
        }
        Plan::Cross { .. } => true,
    };
    if !changes {
        return;
    }
    let _ = app.composite("Fade", |app, key| {
        match plan {
            Plan::None => {}
            Plan::Set(v) => {
                for (clip, side, fade) in v {
                    app.engine.execute_merged("edit.fades_create", &set_params(clip, side, fade), key)?;
                }
            }
            Plan::Cross { track, start, end } => {
                app.engine.execute_merged(
                    "edit.fades_create",
                    &json!({"tracks": [track.0], "start": start, "end": end, "shape": FadeShape::EqualPower.label()}),
                    key,
                )?;
            }
        }
        Ok(())
    });
}

/// Which fade region of an audio clip the point is in (and the clip), for clicks and Alt-drags.
pub(crate) fn hit(s: &Session, map: Map, layout: &crate::arrange::Layout, pos: Pos2) -> Option<(ClipId, Side)> {
    let r = layout.clips.iter().rev().find(|c| pos.x >= c.rect[0] && pos.x <= c.rect[2] && pos.y >= c.rect[1] && pos.y <= c.rect[3])?;
    let c = s.find_clip(ClipId(r.id))?.1;
    if !is_audio(c) {
        return None;
    }
    let w = |f: Fade, from: Samples| (map.x(from.saturating_add(f.len)) - map.x(from)).max(0.0);
    let (in_w, out_w) = (w(c.fade_in, c.start), w(c.fade_out, c.end().saturating_sub(c.fade_out.len)));
    region_at(r.rect, in_w, out_w, pos).map(|side| (ClipId(r.id), side))
}

/// Draw `c`'s fades over its body: the shaded part that is attenuated, the curve (the engine's
/// own `Fade::gain`), and small handles at the corners of a selected clip.
pub(crate) fn draw(p: &egui::Painter, c: &Clip, map: Map, rect: egui::Rect, body: egui::Rect, ink: Color32, selected: bool) {
    if !is_audio(c) || body.height() < 6.0 {
        return;
    }
    for side in [Side::In, Side::Out] {
        let f = side.of(c);
        if f.len <= 0 {
            continue;
        }
        let w = match side {
            Side::In => map.x(c.start + f.len) - map.x(c.start),
            Side::Out => map.x(c.end()) - map.x(c.end() - f.len),
        }
        .clamp(1.0, rect.width());
        let (x0, x1) = match side {
            Side::In => (rect.min.x, rect.min.x + w),
            Side::Out => (rect.max.x - w, rect.max.x),
        };
        let pts = curve_points(f, side, x0, x1, body.min.y, body.max.y, 24);
        let mut mesh = egui::Mesh::default();
        let shade = ink.gamma_multiply(0.3);
        for pair in pts.windows(2) {
            let i = mesh.vertices.len() as u32;
            for q in [pos2(pair[0].x, body.min.y), pos2(pair[1].x, body.min.y), pair[1], pair[0]] {
                mesh.colored_vertex(q, shade);
            }
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i, i + 2, i + 3);
        }
        p.add(egui::Shape::mesh(mesh));
        p.add(egui::Shape::line(pts, Stroke::new(1.5, ink)));
    }
    if selected {
        // Corner handles: small right triangles where a drag starts.
        let y = body.min.y + 1.0;
        let a = 9.0;
        p.add(egui::Shape::convex_polygon(
            vec![pos2(rect.min.x + 6.0, y), pos2(rect.min.x + 6.0 + a, y), pos2(rect.min.x + 6.0, y + a)],
            ink,
            Stroke::NONE,
        ));
        p.add(egui::Shape::convex_polygon(
            vec![pos2(rect.max.x - 6.0, y), pos2(rect.max.x - 6.0 - a, y), pos2(rect.max.x - 6.0, y + a)],
            ink,
            Stroke::NONE,
        ));
    }
}

/// A shape menu opened by a click on a fade.
#[derive(Debug, Clone, Copy)]
pub struct Menu {
    pub clip: ClipId,
    pub side: Side,
    pub at: Pos2,
}

/// The five shapes and "Reset Fade", for `clip`'s `side`. True when one was picked.
pub(crate) fn shape_items(app: &mut StudioApp, ui: &mut Ui, clip: ClipId, side: Side) -> bool {
    let Some(f) = app.engine.session().find_clip(clip).map(|(_, c)| side.of(c)) else { return false };
    let mut picked = false;
    ui.label(side.label());
    for shape in FadeShape::ALL {
        let b = ui.selectable_label(f.shape == shape, shape.label());
        app.arrange.menu.retain(|m| !(m.label == shape.label() && m.value == side.id()));
        app.arrange.menu.push(crate::mixer::MenuItem::at(ui, shape.label().into(), side.id().into(), b.rect));
        if b.clicked() {
            set_shape(app, clip, side, shape);
            picked = true;
        }
    }
    ui.separator();
    let b = ui.button("Reset Fade");
    app.arrange.menu.retain(|m| !(m.label == "Reset Fade" && m.value == side.id()));
    app.arrange.menu.push(crate::mixer::MenuItem::at(ui, "Reset Fade".into(), side.id().into(), b.rect));
    if b.clicked() {
        reset(app, clip, side);
        picked = true;
    }
    picked
}

/// Switch a fade to `shape`, keeping its length and curvature.
pub fn set_shape(app: &mut StudioApp, clip: ClipId, side: Side, shape: FadeShape) {
    let Some(f) = app.engine.session().find_clip(clip).map(|(_, c)| side.of(c)).filter(|f| f.len > 0) else { return };
    let _ = app.run("edit.fades_create", set_params(clip, side, Fade { shape, ..f }));
}

/// Back to the default shape (equal power) and no bend, keeping the length (double-click).
pub fn reset(app: &mut StudioApp, clip: ClipId, side: Side) {
    let Some(f) = app.engine.session().find_clip(clip).map(|(_, c)| side.of(c)).filter(|f| f.len > 0) else { return };
    let _ = app.run("edit.fades_create", set_params(clip, side, Fade { len: f.len, shape: FadeShape::EqualPower, curvature: 0.0 }));
}

/// Context menu "Crossfade": equal-power fades over the overlap, both sides, one undo step.
pub fn crossfade(app: &mut StudioApp, clip: ClipId) {
    let Some(o) = overlap(app.engine.session(), clip) else { return };
    let len = o.end - o.start;
    let fade = Fade { len, shape: FadeShape::EqualPower, curvature: 0.0 };
    let _ = app.composite("Crossfade", |app, key| {
        app.engine.execute_merged("edit.fades_create", &set_params(o.left, Side::Out, fade), key)?;
        app.engine.execute_merged("edit.fades_create", &set_params(o.right, Side::In, fade), key)?;
        Ok(())
    });
}

/// The shape menu a click on a fade opens, drawn at the click until a pick or a click elsewhere.
pub(crate) fn click_menu(app: &mut StudioApp, ui: &mut Ui) {
    let Some(m) = app.arrange.fade_menu else { return };
    let mut picked = false;
    let area = egui::Area::new(egui::Id::new("fade_shape_menu")).order(egui::Order::Foreground).fixed_pos(m.at).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_min_width(120.0);
            picked = shape_items(app, ui, m.clip, m.side);
        });
    });
    if picked || (ui.input(|i| i.pointer.any_pressed()) && !area.response.contains_pointer()) {
        app.arrange.fade_menu = None;
        app.arrange.menu.retain(|x| !FadeShape::ALL.iter().any(|s| s.label() == x.label) && x.label != "Reset Fade");
    }
}
