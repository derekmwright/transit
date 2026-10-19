//! Asynchronous audio import (plan 06, L30), as the browser drives it.
//!
//! A drop or a double-click asks the engine to prepare each file (`file.prepare_import`: read,
//! decode and resample on the engine's worker threads) and returns at once. While the files are
//! prepared, the status line says "Importing <name>… 42%" and an outline with an × stands where
//! each clip will land (view state only). Progress is polled at most ten times a second. Files
//! are committed in the order they were dropped (`file.commit_import`, one undo step each) at the
//! track and time the drop asked for; the files of one drop follow each other on one track.
//! Escape, or the × on an outline, cancels. A drop whose track was deleted meanwhile is refused.

use crate::StudioApp;
use crate::arrange::R4;
use crate::theme::TOKENS;
use egui::{Align2, FontId, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_model::TrackId;
use soundcraft_time::Samples;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Seconds between progress polls.
const POLL_S: f64 = 0.1;

/// One file being imported.
#[derive(Debug, Clone)]
pub struct Item {
    /// The engine's staging id.
    pub id: u64,
    pub name: String,
    pub batch: u64,
    /// Engine state: queued, running, done or failed.
    pub state: String,
    pub percent: u32,
    pub error: Option<String>,
    /// Length at the session rate: from the header until the engine knows.
    pub frames: Samples,
    /// Frames since it was queued (the test pacing counts them).
    frames_seen: u32,
}

/// The files of one drop: where the next one goes.
#[derive(Debug, Clone)]
struct Batch {
    track: Option<TrackId>,
    at: Samples,
    next: Samples,
    files: usize,
    committed: usize,
    closed: usize,
    /// Files the engine refused to prepare, and how many were asked for.
    refused: Vec<String>,
    asked: usize,
}

/// One outline, as last drawn (for `ui.inspect`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ItemLayout {
    pub id: u64,
    pub name: String,
    pub state: String,
    pub percent: u32,
    pub track: Option<u64>,
    pub at: Samples,
    pub rect: Option<R4>,
    pub cancel: Option<R4>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ImportLayout {
    pub items: Vec<ItemLayout>,
    /// The longest gap between frames while importing (ms), and the longest import step (poll
    /// and commit) on the UI thread.
    pub max_frame_gap_ms: f32,
    pub max_step_ms: f32,
    pub polls: u64,
}

/// The imports in flight, oldest first.
#[derive(Debug, Default)]
pub struct ImportQueue {
    pub items: Vec<Item>,
    batches: BTreeMap<u64, Batch>,
    next_batch: u64,
    last_poll: Option<f64>,
    gap_armed: bool,
    /// Test seam, a fake prepare: progress is shown as frames seen over this many frames, and a
    /// file commits no sooner than that (and never before the engine has it ready).
    pub pace: Option<u32>,
    pub layout: ImportLayout,
}

impl ImportQueue {
    pub fn active(&self) -> bool {
        !self.items.is_empty()
    }
}

impl StudioApp {
    /// Queue `files` (audio, already checked) for import at `at` on `track`, or on a new track
    /// when `track` is `None`. `closed` counts the Maschine/Kontakt files left out.
    pub(crate) fn queue_imports(&mut self, files: &[PathBuf], track: Option<TrackId>, at: Samples, closed: usize) -> Result<(), String> {
        let rate = f64::from(self.engine.session().sample_rate.hz());
        let batch = self.imports.next_batch;
        self.imports.next_batch += 1;
        let mut queued = 0;
        let mut last_err = None;
        let mut refused = Vec::new();
        for f in files {
            let name = crate::browser::file_name(f);
            match self.engine.execute("file.prepare_import", &json!({"path": f.to_string_lossy()})) {
                Ok(v) => {
                    let frames = self.browser.header_secs(f).map_or((rate * 2.0) as Samples, |s| (s * rate).round() as Samples).max(1);
                    let id = v["id"].as_u64().unwrap_or_default();
                    self.imports.items.push(Item { id, name, batch, state: "queued".into(), percent: 0, error: None, frames, frames_seen: 0 });
                    queued += 1;
                }
                Err(e) => {
                    last_err = Some(format!("Couldn't import {name}: {e}"));
                    refused.push(name);
                }
            }
        }
        if queued == 0 {
            self.status = last_err.unwrap_or_else(|| "Nothing to import".into());
            return Err(self.status.clone());
        }
        let asked = files.len();
        if !refused.is_empty() {
            self.status = not_prepared(&refused, asked);
        }
        self.imports.batches.insert(batch, Batch { track, at, next: at, files: queued, committed: 0, closed, refused, asked });
        if let Some(ctx) = &self.browser.ctx {
            ctx.request_repaint();
        }
        Ok(())
    }

    /// Cancel one import (`Some(id)`) or all of them.
    pub fn cancel_imports(&mut self, id: Option<u64>) {
        let gone: Vec<Item> = match id {
            Some(id) => {
                let (gone, keep) = std::mem::take(&mut self.imports.items).into_iter().partition(|i| i.id == id);
                self.imports.items = keep;
                gone
            }
            None => std::mem::take(&mut self.imports.items),
        };
        for i in &gone {
            let _ = self.engine.execute("file.import_cancel", &json!({"id": i.id}));
        }
        self.forget_batches();
        self.status = match gone.as_slice() {
            [] => return,
            [one] => format!("Canceled the import of {}", one.name),
            many => format!("Canceled {} imports", many.len()),
        };
    }

    /// Drop the batches no item belongs to any more.
    fn forget_batches(&mut self) {
        let items = &self.imports.items;
        self.imports.batches.retain(|b, _| items.iter().any(|i| i.batch == *b));
    }

    /// Drop an item (and its staging entry) with a status.
    fn drop_item(&mut self, idx: usize, status: String) {
        let it = self.imports.items.remove(idx);
        let _ = self.engine.execute("file.import_cancel", &json!({"id": it.id}));
        self.status = status;
        self.forget_batches();
    }

    /// Commit the oldest item: at its drop's track and next position.
    fn commit_front(&mut self) {
        let it = self.imports.items[0].clone();
        let Some(b) = self.imports.batches.get(&it.batch).cloned() else {
            self.drop_item(0, format!("Couldn't import {}", it.name));
            return;
        };
        if let Some(t) = b.track
            && self.engine.session().track(t).is_none()
        {
            // The drop's track is gone: refuse this file and the rest of its drop.
            let rest: Vec<u64> = self.imports.items.iter().filter(|i| i.batch == it.batch).map(|i| i.id).collect();
            for id in rest {
                let _ = self.engine.execute("file.import_cancel", &json!({"id": id}));
            }
            self.imports.items.retain(|i| i.batch != it.batch);
            self.forget_batches();
            self.status = format!("Couldn't import {}: its track was deleted", it.name);
            return;
        }
        let mut p = json!({"id": it.id, "at": b.next});
        if let Some(t) = b.track {
            p["track"] = json!(t.0);
        }
        self.engine.end_merge();
        match self.engine.execute("file.commit_import", &p) {
            Ok(v) => {
                self.imports.items.remove(0);
                let track = v.get("track").and_then(Value::as_u64).map(TrackId);
                let frames = v.get("frames").and_then(Value::as_i64).unwrap_or(0);
                let mut b = b;
                b.track = track.or(b.track);
                b.next += frames;
                b.committed += 1;
                self.imports.batches.insert(it.batch, b.clone());
                let left = self.imports.items.iter().any(|i| i.batch == it.batch);
                if !left {
                    let what = match (b.files, b.committed) {
                        (1, _) => it.name.clone(),
                        (f, c) if f == c => format!("{c} files"),
                        (f, c) => format!("{c} of {f} files"),
                    };
                    let name = b.track.and_then(|t| self.engine.session().track(t)).map_or_else(String::new, |t| t.name.clone());
                    let mut msg = format!("Imported {what} to {name} at {}", crate::browser::bbs(self, b.at));
                    if b.closed > 0 {
                        msg.push_str(&format!(
                            " ({} Maschine/Kontakt file{} skipped: not supported)",
                            b.closed,
                            if b.closed == 1 { "" } else { "s" }
                        ));
                    }
                    if !b.refused.is_empty() {
                        msg.push_str(&format!("; {}", not_prepared(&b.refused, b.asked)));
                    }
                    self.status = msg;
                    self.forget_batches();
                }
                if let Some(t) = b.track {
                    self.ui.focus_track = Some(t);
                }
            }
            Err(e) => self.drop_item(0, format!("Couldn't import {}: {e}", it.name)),
        }
    }
}

/// "2 of 18 could not be prepared: a.wav, b.wav".
fn not_prepared(names: &[String], asked: usize) -> String {
    format!("{} of {asked} could not be prepared: {}", names.len(), names.join(", "))
}

/// Once a frame (from `logic`): poll progress (at most ten times a second), commit what is ready
/// in order, and keep the status line current.
pub fn step(app: &mut StudioApp, ctx: &egui::Context) {
    if !app.imports.active() {
        app.imports.gap_armed = false;
        app.imports.layout.items.clear();
        return;
    }
    let t0 = std::time::Instant::now();
    if app.imports.gap_armed {
        let gap = ctx.input(|i| i.unstable_dt) * 1000.0;
        app.imports.layout.max_frame_gap_ms = app.imports.layout.max_frame_gap_ms.max(gap);
    }
    app.imports.gap_armed = true;
    for it in &mut app.imports.items {
        it.frames_seen = it.frames_seen.saturating_add(1);
    }
    let now = ctx.input(|i| i.time);
    if app.imports.last_poll.is_none_or(|t| now - t >= POLL_S || now < t) {
        app.imports.last_poll = Some(now);
        app.imports.layout.polls += 1;
        poll(app);
        // Commit in order: only the oldest item may go, then the next.
        while let Some(front) = app.imports.items.first() {
            let paced = app.imports.pace.is_none_or(|n| front.frames_seen >= n);
            match front.state.as_str() {
                "done" if paced => app.commit_front(),
                "failed" => {
                    let msg = format!("Couldn't import {}: {}", front.name, front.error.clone().unwrap_or_default());
                    app.drop_item(0, msg);
                }
                _ => break,
            }
        }
        if let Some(front) = app.imports.items.first() {
            let more = app.imports.items.len() - 1;
            let more = if more > 0 { format!(" ({more} more)") } else { String::new() };
            app.status = format!("Importing {}… {}%{more}", front.name, front.percent);
        }
    }
    let ms = t0.elapsed().as_secs_f32() * 1000.0;
    app.imports.layout.max_step_ms = app.imports.layout.max_step_ms.max(ms);
    // Keep frames coming while importing: the outlines animate and the frame time stays honest.
    ctx.request_repaint();
}

fn poll(app: &mut StudioApp) {
    let pace = app.imports.pace;
    let mut lost = Vec::new();
    for (i, it) in app.imports.items.iter_mut().enumerate() {
        match app.engine.execute("file.import_progress", &json!({"id": it.id})) {
            Ok(v) => {
                it.state = v["state"].as_str().unwrap_or("queued").to_string();
                let real = v["progress"].as_u64().unwrap_or(0) as u32;
                it.percent = match pace {
                    Some(n) if it.frames_seen < n => (it.frames_seen * 100 / n.max(1)).min(99),
                    Some(_) if it.state == "done" => 100,
                    Some(_) => 99,
                    None => real,
                };
                it.error = v["error"].as_str().map(str::to_string);
                // The header's estimate stands until the engine knows the length.
                if let Some(f) = v["frames"].as_i64() {
                    it.frames = f;
                }
            }
            Err(_) => lost.push(i),
        }
    }
    // Gone from the engine: the session was replaced.
    if let Some(&first) = lost.first() {
        let name = app.imports.items[first].name.clone();
        for i in lost.into_iter().rev() {
            app.imports.items.remove(i);
        }
        app.forget_batches();
        app.status = format!("The import of {name} stopped: the session changed");
    }
}

/// Where the label and the × of an outline at `r` go: at the visible part of it, so a file longer
/// than the view still shows both. Returns the label's top-left and the × rect.
pub(crate) fn pinned(r: Rect, tl: Rect) -> (egui::Pos2, Rect) {
    let vis = r.intersect(tl);
    let x = Rect::from_center_size(pos2((vis.max.x - 10.0).max(vis.min.x + 10.0), r.min.y + 10.0), vec2(16.0, 16.0));
    (pos2(vis.min.x + 6.0, r.min.y + 4.0), x)
}

/// The outlines where the clips will land, with their progress and an × that cancels. Call after
/// the arrangement is drawn.
pub fn paint(app: &mut StudioApp, ctx: &egui::Context) {
    if !app.imports.active() {
        return;
    }
    let l = &app.arrange.layout;
    let tl = Rect::from_min_max(pos2(l.timeline[0], l.timeline[1]), pos2(l.timeline[2], l.timeline[3]));
    let x_of = |at: Samples| tl.min.x + ((at - l.scroll) as f64 / l.samples_per_px.max(1e-9)) as f32;
    let below = l.tracks.last().map_or(tl.min.y, |r| r.lane[3]);
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("import-outlines"))).with_clip_rect(tl);
    let t = ctx.input(|i| i.time) as f32;
    let mut pos: BTreeMap<u64, Samples> = BTreeMap::new();
    let mut out = Vec::new();
    let mut cancel = None;
    for it in &app.imports.items {
        let Some(b) = app.imports.batches.get(&it.batch) else { continue };
        let at = *pos.get(&it.batch).unwrap_or(&b.next);
        pos.insert(it.batch, at + it.frames);
        let lane = match b.track {
            Some(t) => l.tracks.iter().find(|r| r.id == t.0).map(|r| (r.lane[1], r.lane[3])),
            None => Some((below + 2.0, below + 40.0)),
        };
        let mut row = ItemLayout {
            id: it.id,
            name: it.name.clone(),
            state: it.state.clone(),
            percent: it.percent,
            track: b.track.map(|t| t.0),
            at,
            rect: None,
            cancel: None,
        };
        if let Some((y0, y1)) = lane {
            let r = Rect::from_min_max(pos2(x_of(at), y0 + 2.0), pos2(x_of(at + it.frames).max(x_of(at) + 24.0), y1 - 2.0));
            if r.intersects(tl) {
                // A dashed look: the outline breathes while the file is prepared.
                let a = 0.55 + 0.45 * (t * 4.0).sin().abs();
                painter.rect_filled(r, crate::theme::RADIUS_SM, TOKENS.accent.gamma_multiply(0.12));
                painter.rect_stroke(r, crate::theme::RADIUS_SM, Stroke::new(1.5, TOKENS.accent.gamma_multiply(a)), StrokeKind::Inside);
                let fill = Rect::from_min_max(pos2(r.min.x, r.max.y - 3.0), pos2(r.min.x + r.width() * it.percent as f32 / 100.0, r.max.y));
                painter.rect_filled(fill, 0.0, TOKENS.accent);
                let label = format!("{} {}%", it.name, it.percent);
                let (text_at, x) = pinned(r, tl);
                painter.text(text_at, Align2::LEFT_TOP, label, FontId::proportional(11.5), TOKENS.text);
                let id = egui::Id::new(("import-cancel", it.id));
                let resp = egui::Area::new(id)
                    .order(egui::Order::Foreground)
                    .fixed_pos(x.min)
                    .show(ctx, |ui| ui.allocate_rect(Rect::from_min_size(x.min, x.size()), Sense::click()));
                let resp = resp.inner;
                let ink = if resp.hovered() { TOKENS.text } else { TOKENS.text_dim };
                painter.text(x.center(), Align2::CENTER_CENTER, "×", FontId::proportional(14.0), ink);
                if resp.clicked() {
                    cancel = Some(it.id);
                }
                row.rect = Some(crate::arrange::r4(r));
                row.cancel = Some(crate::arrange::r4(x));
            }
        }
        out.push(row);
    }
    app.imports.layout.items = out;
    if let Some(id) = cancel {
        app.cancel_imports(Some(id));
    }
}
