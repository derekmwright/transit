//! The scope and level strip at the top of a bespoke built-in panel: the last few milliseconds of
//! the insert's output as an envelope, its peak level in dB, and for the Compressor and the
//! Expander/Gate the gain reduction. The render fills a tap per insert (`soundcraft_mix::ScopeTap`)
//! while the window is open; this reads it through the player (see `StudioApp::scope_hub`).

use crate::StudioApp;
use crate::builtin_editor::EditorLayout;
use crate::theme::{RADIUS_SM, TOKENS};
use egui::{Pos2, Rect, Sense, Shape, Stroke, Ui, vec2};
use soundcraft_mix::{INSTRUMENT_SLOT, INSTRUMENT_UID, SCOPE_FRAMES, ScopeHub};
use soundcraft_model::{Insert, TrackId};
use std::sync::Arc;

/// Size of the scope and of the readout column beside it.
pub const SCOPE_W: f32 = 300.0;
pub const SCOPE_H: f32 = 48.0;
const READOUT_W: f32 = 96.0;
/// The floor of the scope's dB scale, and the gain reduction a full bar stands for.
const FLOOR_DB: f32 = -60.0;
const GR_FULL_DB: f32 = 24.0;

/// Whether a plug-in's strip shows gain reduction.
pub fn has_gain_reduction(plugin: &str) -> bool {
    matches!(plugin, "compressor" | "expander_gate")
}

/// The hub key of an editor's insert: its uid, or the instrument's.
pub fn hub_uid(key: (TrackId, usize), ins: &Insert) -> u64 {
    if key.1 == INSTRUMENT_SLOT { INSTRUMENT_UID } else { ins.uid.get() }
}

/// Min and max of `frames` per pixel column: column `x` of `px` covers its share of the frames.
/// Empty for no frames or no columns; a column never reads past the data.
#[must_use]
pub fn reduce(frames: &[f32], px: usize) -> Vec<(f32, f32)> {
    if frames.is_empty() || px == 0 {
        return Vec::new();
    }
    let n = frames.len();
    (0..px)
        .map(|x| {
            let a = x * n / px;
            let b = ((x + 1) * n / px).clamp(a + 1, n);
            frames.get(a..b).unwrap_or(&[]).iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| (lo.min(*v), hi.max(*v)))
        })
        .collect()
}

/// Amplitude to dB, floored at `FLOOR_DB`.
fn db(a: f32) -> f32 {
    if a <= 0.0 { FLOOR_DB } else { (20.0 * a.log10()).max(FLOOR_DB) }
}

/// 0..1 height of an amplitude on the scope's dB scale.
#[must_use]
pub fn height(a: f32) -> f32 {
    ((db(a) - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// The peak readout: "-inf dB" for silence.
#[must_use]
pub fn level_text(peak: f32) -> String {
    if peak < 1e-5 { "-inf dB".to_string() } else { format!("{:.1} dB", 20.0 * peak.log10()) }
}

/// The gain reduction readout.
#[must_use]
pub fn gr_text(gr_db: f32) -> String {
    format!("GR {:.1} dB", gr_db.max(0.0))
}

impl StudioApp {
    /// The scope taps to read and switch: the player's, or the headless one (tests, `--no-audio`).
    pub(crate) fn scope_hub(&self) -> Option<Arc<ScopeHub>> {
        match &self.player {
            Some(p) => p.scope_hub(),
            None => self.builtin_editor.hub.clone(),
        }
    }
}

/// Draws the strip into `ui` and records its rects in `lay`.
pub fn show(app: &mut StudioApp, ui: &mut Ui, key: (TrackId, usize), ins: &Insert, lay: &mut EditorLayout) {
    let uid = hub_uid(key, ins);
    let hub = app.scope_hub();
    let mut frames = std::mem::take(&mut app.builtin_editor.frames);
    frames.resize(SCOPE_FRAMES, 0.0);
    if let Some(h) = hub.as_ref() {
        h.frames(key.0, uid, &mut frames);
    }
    let gr = hub.as_ref().map_or(0.0, |h| h.gain_reduction_db(key.0, uid));
    let peak = frames.iter().fold(0.0f32, |m, v| m.max(*v));
    let with_gr = has_gain_reduction(&ins.plugin);
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(SCOPE_W, SCOPE_H), Sense::hover());
        lay.scope = [rect.min.x, rect.min.y, rect.max.x, rect.max.y];
        draw_scope(ui, rect, &frames);
        ui.vertical(|ui| {
            ui.set_width(READOUT_W);
            lay.level = level_text(peak);
            ui.label(egui::RichText::new(&lay.level).monospace().color(TOKENS.text));
            if with_gr {
                let (bar, _) = ui.allocate_exact_size(vec2(READOUT_W, 8.0), Sense::hover());
                lay.gr_bar = [bar.min.x, bar.min.y, bar.max.x, bar.max.y];
                draw_gr_bar(ui, bar, gr);
                lay.gr = gr_text(gr);
                ui.label(egui::RichText::new(&lay.gr).monospace().color(TOKENS.text_dim));
            }
        });
    });
    app.builtin_editor.frames = frames;
}

fn draw_scope(ui: &Ui, rect: Rect, frames: &[f32]) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, RADIUS_SM, TOKENS.lane);
    let mid = rect.center().y;
    p.line_segment([Pos2::new(rect.min.x, mid), Pos2::new(rect.max.x, mid)], Stroke::new(1.0, TOKENS.grid_bar));
    let cols = reduce(frames, rect.width().max(1.0) as usize);
    let half = rect.height() * 0.5 - 1.0;
    let (mut top, mut bottom) = (Vec::with_capacity(cols.len()), Vec::with_capacity(cols.len()));
    for (x, (_, hi)) in cols.iter().enumerate() {
        let h = height(*hi) * half;
        let px = rect.min.x + x as f32 + 0.5;
        top.push(Pos2::new(px, mid - h));
        bottom.push(Pos2::new(px, mid + h));
    }
    let stroke = Stroke::new(1.0, TOKENS.accent);
    p.add(Shape::line(top, stroke));
    p.add(Shape::line(bottom, stroke));
}

fn draw_gr_bar(ui: &Ui, rect: Rect, gr_db: f32) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, RADIUS_SM, TOKENS.lane);
    let w = rect.width() * (gr_db / GR_FULL_DB).clamp(0.0, 1.0);
    if w > 0.0 {
        let fill = Rect::from_min_size(rect.min, vec2(w, rect.height()));
        p.rect_filled(fill, RADIUS_SM, TOKENS.accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_gives_min_and_max_per_column() {
        let f = [0.1, 0.5, 0.2, 0.9, 0.3, 0.0, 0.4, 0.4];
        assert_eq!(reduce(&f, 4), vec![(0.1, 0.5), (0.2, 0.9), (0.0, 0.3), (0.4, 0.4)]);
        assert_eq!(reduce(&f, 1), vec![(0.0, 0.9)]);
    }

    #[test]
    fn reduce_handles_edges() {
        assert!(reduce(&[], 10).is_empty());
        assert!(reduce(&[1.0], 0).is_empty());
        // More columns than frames: every column still has a frame.
        let r = reduce(&[0.25, 0.75], 5);
        assert_eq!(r.len(), 5);
        assert!(r.iter().all(|(lo, hi)| lo <= hi && *lo >= 0.25 && *hi <= 0.75));
        assert_eq!(r[0], (0.25, 0.25));
        assert_eq!(r[4], (0.75, 0.75));
    }

    #[test]
    fn heights_and_readouts() {
        assert_eq!(height(0.0), 0.0);
        assert!((height(1.0) - 1.0).abs() < 1e-6);
        assert!((height(0.001) - 0.0).abs() < 1e-6, "-60 dB is the floor");
        assert_eq!(level_text(0.0), "-inf dB");
        assert_eq!(level_text(0.5), "-6.0 dB");
        assert_eq!(gr_text(3.25), "GR 3.2 dB");
        assert_eq!(gr_text(-1.0), "GR 0.0 dB");
    }
}
