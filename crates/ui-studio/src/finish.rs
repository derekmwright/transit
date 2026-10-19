//! Slice 6, finishing a track: rename in place (tracks and clips), takes (track playlists shown as
//! take lanes), File > Bounce with progress and cancel, Track > Bounce in Place, and the undo
//! history window. Everything that changes the document runs an engine command.
//!
//! Takes are the engine's track playlists (plan 06, L8): a take is a whole playlist, not a lane
//! inside one clip. Recording over clips duplicates the active playlist first
//! (`StudioApp::place_takes`), so the previous take keeps the old clips.

use crate::arrange::R4;
use crate::mixer::MenuItem;
use crate::shortcuts::{Action as A, hit, label, pressed};
use crate::theme::TOKENS;
use crate::{Dialog, StudioApp};
use egui::{FontId, Rect, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_model::{ClipId, TrackId, TrackKind};

/// What a name being typed renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RenameTarget {
    Track(TrackId),
    Clip(ClipId),
}

/// A name being typed in place.
#[derive(Debug, Clone)]
pub struct Rename {
    pub target: RenameTarget,
    pub text: String,
    /// Where the field sits (the track's name, the strip's name or the clip's name strip).
    pub rect: R4,
    fresh: bool,
    /// A refused name keeps the field open: focus it again, keeping the typed text.
    refocus: bool,
    /// Escape or Enter this frame, taken before any other handler sees them ([`rename_keys`]).
    keys: (bool, bool),
}

/// File > Bounce: which part of the song.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum BounceRange {
    /// The selection, which is also the cycle range in this UI (plan 06, L6).
    Selection,
    Whole,
}

/// The File > Bounce form.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BounceForm {
    pub range: BounceRange,
    /// `main`, or the name of a bus (an FX channel or a group).
    pub source: String,
    pub format: String,
    pub bit_depth: String,
    pub normalize: bool,
    pub path: String,
}

/// One take lane as last drawn.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TakeRow {
    pub track: u64,
    pub index: usize,
    pub active: bool,
    pub clips: usize,
    /// The lane in the timeline (click: make this take active).
    pub lane: R4,
    /// Its label in the track list (click: the same).
    pub label: R4,
    pub promote: R4,
    pub delete: R4,
    pub delete_enabled: bool,
}

/// A clip's take disclosure chip ("2 takes"), as last drawn.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TakeChip {
    pub track: u64,
    pub clip: u64,
    pub takes: usize,
    pub open: bool,
    pub rect: R4,
}

/// The bounce form as last drawn.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BounceLayout {
    pub window: R4,
    pub ranges: Vec<MenuItem>,
    pub sources: Vec<MenuItem>,
    pub formats: Vec<MenuItem>,
    pub depths: Vec<MenuItem>,
    pub normalize: R4,
    pub path: R4,
    pub choose: R4,
    pub ok: R4,
    pub cancel: R4,
}

/// One row of Edit > Undo History.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HistoryRow {
    pub label: String,
    /// How many undo steps stay done after a click on this row.
    pub keep: usize,
    pub current: bool,
    pub rect: R4,
}

/// Slice 6 geometry and state for `ui.inspect`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FinishLayout {
    pub takes: Vec<TakeRow>,
    pub chips: Vec<TakeChip>,
    pub bounce: Option<BounceLayout>,
    pub history_open: bool,
    pub history: Vec<HistoryRow>,
    pub history_window: R4,
    /// The last finished bounce: path, seconds, bytes.
    pub last_bounce: Option<Value>,
}

#[derive(Default)]
pub struct FinishState {
    pub rename: Option<Rename>,
    pub bounce_form: Option<BounceForm>,
    /// The bounce being rendered, a slice per frame.
    pub job: Option<soundcraft_engine::io::Bounce>,
    /// Gestures started (each composite gets its own merge key).
    serial: u64,
    pub layout: FinishLayout,
    /// The progress window's Cancel button as last drawn.
    pub cancel: R4,
    /// A typed destination that exists is replaced only after asking.
    pub replace: Replace,
    /// The "Replace existing file?" question's buttons as last drawn: replace, cancel.
    pub replace_buttons: (R4, R4),
}

/// Where the "Replace existing file?" question stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Replace {
    #[default]
    No,
    Asking,
    Yes,
}

/// The extensions of audio files a bounce can write or replace.
const AUDIO_EXTS: [&str; 6] = ["wav", "aif", "aiff", "flac", "mp3", "ogg"];

/// `path` with the extension `format` writes. A known audio extension is replaced; any other
/// (`mix.v2`) is part of the name, so the new one is appended (`mix.v2.wav`).
pub fn fit_extension(path: &std::path::Path, format: &str) -> std::path::PathBuf {
    let ext = if format == "aiff" { "aif" } else { format };
    let has = path.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    if has.as_deref().is_some_and(|h| h == ext || (ext == "aif" && h == "aiff")) {
        return path.to_path_buf();
    }
    let mut out = path.to_path_buf();
    if has.as_deref().is_some_and(|h| AUDIO_EXTS.contains(&h)) {
        out.set_extension(ext);
    } else {
        let mut name = out.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
        name.push(".");
        name.push(ext);
        out.set_file_name(name);
    }
    out
}

impl std::fmt::Debug for FinishState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FinishState").field("rename", &self.rename).field("bounce_form", &self.bounce_form).field("job", &self.job.is_some()).finish()
    }
}

impl FinishState {
    pub fn inspect(&self) -> Value {
        json!({
            "rename": self.rename.as_ref().map(|r| json!({"target": r.target, "text": r.text, "rect": r.rect})),
            "bounce_form": self.bounce_form,
            "bouncing": self.job.as_ref().map(|j| json!({"progress": j.progress(), "path": j.path(), "cancel": self.cancel})),
            "layout": self.layout,
        })
    }
}

pub fn rect(r: R4) -> Rect {
    Rect::from_min_max(pos2(r[0], r[1]), pos2(r[2], r[3]))
}

/// A session-replacing command while a bounce holds its own copy of the session is refused (two
/// large sessions could run out of memory); Quit cancels the bounce.
pub fn refuses(app: &mut StudioApp, id: &str) -> bool {
    if app.finish.job.is_none() {
        return false;
    }
    if id == "app.quit" {
        app.finish.job = None;
        app.dialog = None;
        return false;
    }
    let replacing = matches!(id, "session.new" | "session.open" | "session.close" | "session.revert");
    if replacing {
        app.status = "Finish or cancel the bounce first".into();
    }
    replacing
}

impl StudioApp {
    // ---- Rename ----

    /// Start typing a new name for `target` in a field over `rect`.
    pub fn start_rename(&mut self, target: RenameTarget, rect: R4) {
        let s = self.engine.session();
        let name = match target {
            RenameTarget::Track(t) => s.track(t).map(|t| t.name.clone()),
            RenameTarget::Clip(c) => s.find_clip(c).map(|(_, c)| c.name.clone()),
        };
        let Some(text) = name else { return };
        self.finish.rename = Some(Rename { target, text, rect, fresh: true, refocus: false, keys: (false, false) });
    }

    /// Ctrl+R or F2: the selected clip, unless tracks were picked by their headers; else the
    /// focused track. False when there is nothing to rename.
    pub fn rename_selected(&mut self) -> bool {
        let s = self.engine.session_arc();
        if !self.tracks_selected_by_header
            && let Some(c) = s.edit.selected_clips.first().copied()
            && s.find_clip(c).is_some()
        {
            let r = self.arrange.layout.clips.iter().find(|r| r.id == c.0).map(|r| r.rect);
            let r = r.map_or_else(
                || self.fallback_rect(),
                |r| [r[0].max(self.arrange.layout.timeline[0]), r[1], r[2].max(r[0] + 160.0), r[1] + crate::theme::CLIP_NAME_H + 6.0],
            );
            self.start_rename(RenameTarget::Clip(c), r);
            return true;
        }
        let Some(t) = self.focus_track() else {
            self.status = "Select a track or a clip to rename.".into();
            return false;
        };
        let strip = self.mixer.layout.strips.iter().find(|x| x.track == t.0).map(|x| x.name_rect);
        let row = self.arrange.layout.tracks.iter().find(|x| x.id == t.0).map(|x| x.name_rect);
        let r = if self.ui.mixer_full { strip.or(row) } else { row.or(strip) };
        let r = r.filter(|r| r[2] > r[0]).unwrap_or_else(|| self.fallback_rect());
        self.start_rename(RenameTarget::Track(t), r);
        true
    }

    fn fallback_rect(&self) -> R4 {
        let l = self.arrange.layout.track_list;
        [l[0] + 8.0, l[1] + 4.0, l[0] + 200.0, l[1] + 24.0]
    }

    /// Rename `target` to `text` (trimmed): `track.rename` or `clip.rename`. An empty or
    /// unchanged name changes nothing.
    pub fn apply_rename(&mut self, target: RenameTarget, text: &str) -> Result<Value, String> {
        let name = text.trim();
        let current = match target {
            RenameTarget::Track(t) => self.engine.session().track(t).map(|t| t.name.clone()),
            RenameTarget::Clip(c) => self.engine.session().find_clip(c).map(|(_, c)| c.name.clone()),
        };
        if name.is_empty() {
            self.status = "A name can't be empty; kept the old one.".into();
            return Err(self.status.clone());
        }
        if current.as_deref() == Some(name) {
            return Ok(json!({"unchanged": true}));
        }
        match target {
            RenameTarget::Track(t) => self.run("track.rename", json!({"track": t.0, "name": name})),
            RenameTarget::Clip(c) => self.run("clip.rename", json!({"clips": [c.0], "name": name})),
        }
    }

    // ---- Takes ----

    /// Show or hide a track's take lanes (view state only).
    pub fn toggle_takes(&mut self, t: TrackId) {
        if let Some(i) = self.ui.takes_open.iter().position(|x| *x == t) {
            self.ui.takes_open.remove(i);
        } else {
            self.ui.takes_open.push(t);
        }
    }

    fn take_count(&self, t: TrackId) -> usize {
        self.engine.session().track(t).map_or(0, |t| t.playlists.len())
    }

    fn track_name(&self, t: TrackId) -> String {
        self.engine.session().track(t).map_or_else(String::new, |t| t.name.clone())
    }

    /// Make take `k` the one that plays (`track.playlist_select`).
    pub fn select_take(&mut self, t: TrackId, k: usize) -> Result<Value, String> {
        let r = self.run("track.playlist_select", json!({"track": t.0, "index": k}))?;
        self.status = format!("Take {} of {} plays", k + 1, self.track_name(t));
        Ok(r)
    }

    /// Promote take `k`: it becomes the track's only take, the others are removed. One undo step
    /// (`track.playlist_select` then `track.playlist_delete_unused`, merged). The undo entry keeps
    /// the first command's label, "Select Playlist": the engine has no relabel for a merged step,
    /// the order is forced (delete-unused keeps the active take), and `track.playlist_promote`
    /// copies a range between playlists instead, so this is left documented (L20).
    pub fn promote_take(&mut self, t: TrackId, k: usize) -> Result<Value, String> {
        let n = self.take_count(t);
        if k >= n {
            return Err("no such take".into());
        }
        let active = self.engine.session().track(t).map_or(0, |t| t.active_playlist);
        let r = if active == k {
            self.run("track.playlist_delete_unused", json!({"track": t.0}))
        } else {
            self.finish.serial += 1;
            let key = format!("promote:{}", self.finish.serial);
            self.engine.end_merge();
            let first = self.engine.execute_merged("track.playlist_select", &json!({"track": t.0, "index": k}), &key);
            let r = first.and_then(|_| self.engine.execute_merged("track.playlist_delete_unused", &json!({"track": t.0}), &key));
            self.engine.end_merge();
            r.map_err(|e| {
                self.status = e.to_string();
                e.to_string()
            })
        }?;
        self.ui.takes_open.retain(|x| *x != t);
        self.status = format!("Kept take {} of {}; removed {} other take{}", k + 1, self.track_name(t), n - 1, if n == 2 { "" } else { "s" });
        Ok(r)
    }

    /// Delete take `k`. The engine removes playlists only as "all but the active one", so this
    /// works when the track has two takes (it keeps the other); with more it is refused (L33).
    pub fn delete_take(&mut self, t: TrackId, k: usize) -> Result<Value, String> {
        let n = self.take_count(t);
        if n != 2 || k >= n {
            self.status = "Deleting one of three or more takes needs an engine command (track.playlist_delete); promote a take instead.".into();
            return Err(self.status.clone());
        }
        let keep = 1 - k;
        self.promote_take(t, keep)?;
        self.status = format!("Deleted take {} of {}", k + 1, self.track_name(t));
        Ok(json!({"kept": keep}))
    }

    // ---- Bounce ----

    /// File > Bounce…: open the form with the selection (or the whole song) and the main mix.
    pub fn open_bounce(&mut self) {
        if self.finish.job.is_some() || self.dialog.is_some() {
            return;
        }
        let s = self.engine.session();
        let range = if s.edit.selection.is_empty() { BounceRange::Whole } else { BounceRange::Selection };
        let name = format!("{}.wav", soundcraft_engine::io::sanitize_name(&s.name));
        let dir =
            self.engine.path.as_ref().and_then(|p| std::path::Path::new(p).parent().map(std::path::Path::to_path_buf)).unwrap_or_else(default_dir);
        self.finish.bounce_form = Some(BounceForm {
            range,
            source: "main".into(),
            format: "wav".into(),
            bit_depth: "24".into(),
            normalize: false,
            path: dir.join(name).to_string_lossy().into_owned(),
        });
        self.dialog = Some(Dialog::Bounce);
    }

    /// Start the bounce the form describes: `file.bounce_mix`'s job, rendered a slice per frame.
    pub fn start_bounce(&mut self) -> Result<(), String> {
        let Some(f) = self.finish.bounce_form.clone() else { return Err("no bounce form".into()) };
        let s = self.engine.session();
        let (start, end) = match f.range {
            BounceRange::Selection if !s.edit.selection.is_empty() => (s.edit.selection.start, s.edit.selection.end),
            BounceRange::Selection => {
                self.status = "Nothing is selected: pick a range, or bounce the whole song.".into();
                return Err(self.status.clone());
            }
            BounceRange::Whole => (0, s.content_end().max(1)),
        };
        let mut path = std::path::PathBuf::from(f.path.trim());
        if f.path.trim().is_empty() || path.file_name().is_none() {
            self.status = "Choose where to write the bounce.".into();
            return Err(self.status.clone());
        }
        path = fit_extension(&path, &f.format);
        if path.exists() && self.finish.replace != Replace::Yes {
            self.finish.replace = Replace::Asking;
            self.status = format!("{} exists", path.display());
            return Err(self.status.clone());
        }
        self.finish.replace = Replace::No;
        let mut p = json!({
            "path": path.to_string_lossy(),
            "format": f.format,
            "bit_depth": f.bit_depth,
            "normalize": f.normalize,
            "start": start,
            "end": end,
        });
        if f.source != "main" {
            p["source"] = json!(f.source);
        }
        self.capture_plugin_states();
        match soundcraft_engine::cmd::bounce_job(&self.engine, &p) {
            Ok(job) => {
                self.finish.job = Some(job);
                self.finish.bounce_form = None;
                self.status = format!("Bouncing to {}…", path.display());
                Ok(())
            }
            Err(e) => {
                self.status = e.to_string();
                Err(e.to_string())
            }
        }
    }

    /// Cancel the bounce form or the bounce being rendered (its part file is removed).
    pub fn cancel_bounce(&mut self) {
        if self.finish.job.take().is_some() {
            self.status = "Bounce cancelled".into();
        }
        self.finish.bounce_form = None;
        self.finish.replace = Replace::No;
        if matches!(self.dialog, Some(Dialog::Bounce)) {
            self.dialog = None;
        }
    }

    /// Track > Bounce in Place: the focused track rendered to a new audio track (`track.bounce`);
    /// the original is hidden and inactive. One undo step.
    pub fn bounce_in_place(&mut self) -> Result<Value, String> {
        let Some(t) = self.focus_track() else {
            self.status = "Select a track to bounce in place.".into();
            return Err(self.status.clone());
        };
        let name = self.track_name(t);
        let r = self.run("track.bounce", json!({"tracks": [t.0]}))?;
        let made: Vec<u64> = r["tracks"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        match made.first() {
            Some(n) => {
                let new = self.track_name(TrackId(*n));
                self.focus(TrackId(*n));
                self.status = format!("Bounced {name} in place to {new}; the original is hidden and inactive (Undo brings it back)");
            }
            None => self.status = format!("Nothing to bounce on {name}: it has no clips"),
        }
        Ok(r)
    }

    // ---- Undo history ----

    /// Undo until `keep` steps of the history remain done (a click in Edit > Undo History).
    pub fn undo_to(&mut self, keep: usize) -> usize {
        let mut n = 0;
        while self.engine.undo_history().len() > keep && n < 10_000 {
            if self.run("edit.undo", json!({})).is_err() {
                break;
            }
            n += 1;
        }
        if n > 0 {
            self.status = format!("Undid {n} step{}", if n == 1 { "" } else { "s" });
        }
        n
    }
}

/// Where a bounce goes by default when the session was never saved: the user's Music folder.
fn default_dir() -> std::path::PathBuf {
    if cfg!(test) {
        // Tests never point at the user's folders.
        return std::env::temp_dir();
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(std::path::PathBuf::from);
    home.map(|h| h.join("Music")).filter(|m| m.is_dir()).unwrap_or_else(std::env::temp_dir)
}

/// Slice 6 keys: Rename (Ctrl+R, F2), Bounce (Ctrl+Shift+R), Undo History (Alt+Z).
pub fn shortcuts(app: &mut StudioApp, ctx: &egui::Context) {
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() || app.finish.rename.is_some() {
        return;
    }
    if pressed(ctx, A::Bounce) {
        app.open_bounce();
    }
    if pressed(ctx, A::Rename) {
        app.rename_selected();
    }
    if pressed(ctx, A::UndoHistory) {
        app.ui.show_history = !app.ui.show_history;
    }
}

/// While a name is typed, Escape and Enter belong to its field: taken first in the frame, before
/// the arrangement's Escape (Select None) and Enter (Return to Zero) can.
pub fn rename_keys(app: &mut StudioApp, ctx: &egui::Context) {
    if let Some(r) = &mut app.finish.rename {
        r.keys = (pressed(ctx, A::RenameCancel), pressed(ctx, A::RenameApply));
    }
}

/// The in-place name field: Enter or a click elsewhere applies, Escape cancels.
pub fn rename_field(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(mut r) = app.finish.rename.take() else { return };
    let id = egui::Id::new("rename_field");
    let area = rect(r.rect);
    let w = area.width().max(120.0);
    let (esc, enter) = std::mem::take(&mut r.keys);
    if r.fresh {
        // Asked until the field has it: a new area is laid out once before it takes input.
        let n = r.text.chars().count();
        let mut st = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
        st.store(ctx, id);
        ctx.memory_mut(|m| m.request_focus(id));
    } else if r.refocus {
        ctx.memory_mut(|m| m.request_focus(id));
    }
    let (mut lost, mut has) = (false, false);
    egui::Area::new(egui::Id::new("rename_area")).order(egui::Order::Foreground).fixed_pos(area.min).show(ctx, |ui| {
        let field = egui::TextEdit::singleline(&mut r.text).id(id).font(FontId::proportional(13.0)).margin(vec2(4.0, 2.0)).desired_width(w - 8.0);
        let resp = ui.add_sized(vec2(w, area.height().max(20.0)), field);
        (lost, has) = (resp.lost_focus(), resp.has_focus());
    });
    if esc {
        return;
    }
    if has {
        r.fresh = false;
        r.refocus = false;
    }
    if !r.fresh && !r.refocus && (lost || enter) {
        // A refused name (empty, or taken) keeps the field and the typed text, and the status
        // line says why; Escape still cancels.
        if app.apply_rename(r.target, &r.text).is_err() {
            r.refocus = true;
            app.finish.rename = Some(r);
        }
        return;
    }
    app.finish.rename = Some(r);
}

/// Render the running bounce for a moment (30 ms) each frame.
pub fn step_bounce(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(job) = &mut app.finish.job else { return };
    let until = std::time::Instant::now() + std::time::Duration::from_millis(30);
    let path = job.path().to_string_lossy().into_owned();
    let secs = app.engine.session().sample_rate.seconds(job.range().len());
    app.status = match job.step(|| std::time::Instant::now() < until) {
        Ok(None) => {
            ctx.request_repaint();
            return;
        }
        Ok(Some(done)) => {
            app.finish.layout.last_bounce = Some(json!({"path": path, "seconds": secs, "bytes": done.bytes, "peak_db": done.peak_db}));
            format!("Bounced to {path} ({secs:.1} s, peak {:.1} dBFS)", done.peak_db)
        }
        Err(e) => e.to_string(),
    };
    app.finish.job = None;
    if matches!(app.dialog, Some(Dialog::Bounce)) {
        app.dialog = None;
    }
}

/// A row of choices; returns the picked value.
fn choices(ui: &mut egui::Ui, current: &str, items: &[(String, String)], out: &mut Vec<MenuItem>) -> Option<String> {
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        for (text, value) in items {
            let b = ui.selectable_label(value == current, text);
            out.push(MenuItem::at(ui, text.clone(), value.clone(), b.rect));
            if b.clicked() {
                picked = Some(value.clone());
            }
        }
    });
    picked
}

/// File > Bounce: the form, then the progress window with Cancel (or Escape).
pub fn bounce_dialog(app: &mut StudioApp, ctx: &egui::Context) {
    if let Some(job) = &app.finish.job {
        let (name, progress) = (job.path().file_name().unwrap_or_default().to_string_lossy().into_owned(), job.progress());
        let mut cancel = ctx.input(|i| hit(i, A::DialogCancel));
        egui::Modal::new(egui::Id::new("bounce_progress")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(egui::RichText::new(format!("Bouncing {name}")).strong());
            ui.add(egui::ProgressBar::new(progress));
            ui.horizontal(|ui| {
                let b = ui.button("Cancel");
                app.finish.cancel = crate::arrange::r4(b.rect);
                cancel |= b.clicked();
                ui.label(egui::RichText::new(format!("{:.0}%", progress * 100.0)).color(TOKENS.text_dim));
            });
        });
        if cancel {
            app.cancel_bounce();
        }
        app.finish.layout.bounce = None;
        return;
    }
    let Some(mut f) = app.finish.bounce_form.clone() else {
        app.finish.layout.bounce = None;
        return;
    };
    if app.finish.replace != Replace::No {
        replace_question(app, ctx, &f);
        return;
    }
    let s = app.engine.session_arc();
    let mut lay = BounceLayout::default();
    let (mut go, mut cancel, mut choose) = (false, ctx.input(|i| hit(i, A::DialogCancel)), false);
    let path_id = egui::Id::new("bounce_path");
    let resp = egui::Modal::new(egui::Id::new("bounce_form")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.spacing_mut().item_spacing = vec2(crate::theme::SPACE, crate::theme::SPACE);
        ui.label(egui::RichText::new("Bounce").strong().size(15.0));
        let sel = s.edit.selection;
        let sel_text = if sel.is_empty() {
            "Selection (none)".to_string()
        } else {
            let a = crate::transport::bars_beats(app, sel.start);
            let b = crate::transport::bars_beats(app, sel.end);
            let what = if s.edit.loop_playback { "Cycle range" } else { "Selection" };
            format!("{what} {} – {}", a.trim(), b.trim())
        };
        ui.label(egui::RichText::new("Range").color(TOKENS.text_dim));
        let cur = if f.range == BounceRange::Selection { "selection" } else { "whole" };
        let items = [(sel_text, "selection".to_string()), ("Whole song".to_string(), "whole".to_string())];
        if let Some(v) = choices(ui, cur, &items, &mut lay.ranges) {
            f.range = if v == "selection" { BounceRange::Selection } else { BounceRange::Whole };
        }
        ui.label(egui::RichText::new("Source").color(TOKENS.text_dim));
        let mut items = vec![("Main mix".to_string(), "main".to_string())];
        items.extend(s.busses.iter().map(|b| (crate::mixer::bus_label(&s, b.id), b.name.clone())));
        if let Some(v) = choices(ui, &f.source, &items, &mut lay.sources) {
            f.source = v;
        }
        ui.label(egui::RichText::new("Format").color(TOKENS.text_dim));
        let items: Vec<(String, String)> =
            [("WAV", "wav"), ("AIFF", "aiff"), ("FLAC", "flac")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        if let Some(v) = choices(ui, &f.format, &items, &mut lay.formats) {
            f.format = v;
        }
        let items: Vec<(String, String)> =
            [("16-bit", "16"), ("24-bit", "24"), ("32-bit float", "32f")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        if let Some(v) = choices(ui, &f.bit_depth, &items, &mut lay.depths) {
            f.bit_depth = v;
        }
        let n = ui.checkbox(&mut f.normalize, "Normalize to -0.1 dBFS");
        lay.normalize = crate::arrange::r4(n.rect);
        ui.label(egui::RichText::new("Destination").color(TOKENS.text_dim));
        ui.horizontal(|ui| {
            let field = ui.add(egui::TextEdit::singleline(&mut f.path).id(path_id).desired_width(320.0));
            lay.path = crate::arrange::r4(field.rect);
            let b = ui.button("Choose…");
            lay.choose = crate::arrange::r4(b.rect);
            choose = b.clicked();
        });
        ui.add_space(crate::theme::SPACE);
        ui.horizontal(|ui| {
            let b = ui.button(format!("Bounce   {}", label(A::Bounce)));
            lay.ok = crate::arrange::r4(b.rect);
            go = b.clicked();
            let c = ui.button("Cancel");
            lay.cancel = crate::arrange::r4(c.rect);
            cancel |= c.clicked();
        });
    });
    lay.window = crate::arrange::r4(resp.response.rect);
    app.finish.layout.bounce = Some(lay);
    if choose {
        let named = fit_extension(std::path::Path::new(&f.path), &f.format);
        let name = named.file_name().map_or_else(|| "bounce.wav".to_string(), |n| n.to_string_lossy().into_owned());
        if let Some(p) = app.services.pick_save.as_ref().and_then(|pick| pick(&name)) {
            f.path = p;
        }
    }
    app.finish.bounce_form = Some(f);
    if cancel {
        app.cancel_bounce();
    } else if go && app.start_bounce().is_err() {
        // The form stays open; the status line says why.
    }
}

/// "Replace existing file?" in place of the form: Enter replaces, Escape (or Cancel) goes back to
/// the form with nothing written.
fn replace_question(app: &mut StudioApp, ctx: &egui::Context, f: &BounceForm) {
    let path = fit_extension(std::path::Path::new(f.path.trim()), &f.format);
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let (mut yes, mut no) = (false, false);
    egui::Modal::new(egui::Id::new("bounce_replace")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(egui::RichText::new("Replace existing file?").strong());
        ui.label(egui::RichText::new(format!("{name} already exists.")).color(TOKENS.text_dim));
        ui.add_space(crate::theme::SPACE);
        ui.horizontal(|ui| {
            let r = ui.button("Replace");
            let c = ui.button("Cancel");
            app.finish.replace_buttons = (crate::arrange::r4(r.rect), crate::arrange::r4(c.rect));
            yes = r.clicked() || ui.input(|i| hit(i, A::DialogConfirm));
            no = c.clicked();
        });
    });
    no |= ctx.input(|i| hit(i, A::DialogCancel));
    if yes {
        app.finish.replace = Replace::Yes;
        let _ = app.start_bounce();
    } else if no {
        app.finish.replace = Replace::No;
        app.status = "Bounce not started".into();
    }
}

/// Edit > Undo History: the undo steps, oldest first; a click undoes back to that row.
pub fn history_window(app: &mut StudioApp, ctx: &egui::Context) {
    app.finish.layout.history_open = app.ui.show_history;
    app.finish.layout.history.clear();
    if !app.ui.show_history {
        return;
    }
    let labels = app.engine.undo_history();
    let redo = app.engine.redo_label().map(str::to_string);
    let mut open = true;
    let mut pick = None;
    let mut rows = Vec::new();
    let r = egui::Window::new("Undo History")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([300.0, 360.0])
        .default_pos(ctx.content_rect().right_top() + vec2(-320.0, 80.0))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let all = std::iter::once("Opened".to_string()).chain(labels.iter().cloned());
                for (keep, text) in all.enumerate() {
                    let current = keep == labels.len();
                    let b = ui.selectable_label(current, &text);
                    rows.push(HistoryRow { label: text, keep, current, rect: crate::arrange::r4(b.rect) });
                    if b.clicked() && !current {
                        pick = Some(keep);
                    }
                }
                if let Some(r) = &redo {
                    ui.label(egui::RichText::new(format!("Redo: {r}")).color(TOKENS.text_dim));
                }
            });
        });
    if let Some(r) = r {
        app.finish.layout.history_window = crate::arrange::r4(r.response.rect);
    }
    app.finish.layout.history = rows;
    if let Some(keep) = pick {
        app.undo_to(keep);
    }
    if !open {
        app.ui.show_history = false;
    }
}

/// The take lanes of `kind`'s tracks hold clips: audio and instrument tracks.
pub fn holds_takes(kind: TrackKind) -> bool {
    kind.has_playlist()
}
