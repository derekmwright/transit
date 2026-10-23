//! The arrangement window: a track list on the left, a bar/beat ruler, and the timeline with
//! clips as colored regions. Clicking a clip selects it; dragging moves it (Ctrl-drag, Cmd on a
//! Mac, or Alt-drag copies; clip drags snap to the visible grid, Shift bypasses it); dragging an edge trims it; clicking empty timeline sets the
//! cursor. Every edit goes through engine commands (see `actions`).

use crate::StudioApp;
use crate::theme::{self, CLIP_NAME_H, RULER_H, TOKENS};
use egui::{Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde_json::json;
use soundcraft_model::{Clip, ClipContent, ClipId, ClipLoop, Session, Track, TrackId, TrackKind};
use soundcraft_time::{BarBeat, Samples};

/// How close (points) to a clip edge a press trims instead of moving.
const EDGE: f32 = 6.0;

/// Width (points) of the loop handle's hot zone, measured back from a clip's right edge.
const LOOP_ZONE_W: f32 = 20.0;

/// Most repeats one loop drag shows or lays down (the engine caps `clip.loop_to` at the same).
const MAX_LOOP_REPEATS: usize = 1000;

/// What a drag on a clip does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DragKind {
    Move,
    TrimStart,
    TrimEnd,
    /// The loop handle: the upper right corner of the header strip (Logic's loop corner).
    Loop,
}

/// What a press at `pos` on a clip drawn in `rect` (`[x0, y0, x1, y1]`) would do. The edges trim
/// (`EDGE` wide, as before); the loop handle is the header strip's part of the last
/// `LOOP_ZONE_W` points before the right-edge trim handle; everything else moves. Narrow clips
/// shrink the edges and the loop zone so the middle still moves.
pub fn zone(rect: R4, pos: Pos2) -> DragKind {
    let w = rect[2] - rect[0];
    let edge = EDGE.min(w / 4.0);
    if pos.x - rect[0] <= edge {
        DragKind::TrimStart
    } else if rect[2] - pos.x <= edge {
        DragKind::TrimEnd
    } else if pos.y - rect[1] <= CLIP_NAME_H && rect[2] - pos.x <= LOOP_ZONE_W.min(w / 3.0) {
        DragKind::Loop
    } else {
        DragKind::Move
    }
}

/// The repeats a loop drag lays down: back-to-back copies of length `len` from `from` up to `to`,
/// the last one trimmed at `to`. Empty when `to` is not past `from`.
pub fn loop_ghosts(from: Samples, len: Samples, to: Samples) -> Vec<(Samples, Samples)> {
    let len = len.max(1);
    let mut out = Vec::new();
    let mut a = from;
    while a < to && out.len() < MAX_LOOP_REPEATS {
        out.push((a, a.saturating_add(len).min(to)));
        a = a.saturating_add(len);
    }
    out
}

/// A mouse gesture in progress.
#[derive(Debug, Clone)]
pub enum Gesture {
    Clip { clip: ClipId, track: TrackId, kind: DragKind, press: Pos2, start: Samples, end: Samples },
    Volume { track: TrackId, start_db: f32 },
}

/// Rects as `[x0, y0, x1, y1]`, for `ui.inspect` (scripts drive the UI with them).
pub type R4 = [f32; 4];

pub fn r4(r: Rect) -> R4 {
    [r.min.x, r.min.y, r.max.x, r.max.y]
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TrackRow {
    pub id: u64,
    pub name: String,
    pub kind: TrackKind,
    /// The line under the name (an instrument track: the instrument).
    pub subtitle: String,
    pub header: R4,
    /// The name in the header (double-click renames, slice 6).
    pub name_rect: R4,
    pub lane: R4,
    pub mute: R4,
    pub solo: R4,
    pub arm: R4,
    pub monitor: R4,
    pub volume: R4,
    /// The group this track sits in (slice 2c), its depth, and for a group: collapsed or not,
    /// its disclosure triangle, and how many member clips its collapsed lane summarizes.
    pub folder: Option<u64>,
    pub depth: usize,
    pub collapsed: bool,
    pub disclosure: R4,
    pub summary_bars: usize,
    /// Slice 7: the A toggle (show the track's automation lanes) and whether they are shown.
    pub automation: R4,
    pub automation_shown: bool,
    /// The track is frozen: its lane is tinted cold; the snowflake button unfreezes it.
    pub frozen: bool,
    pub freeze_button: R4,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ClipRect {
    pub id: u64,
    pub track: u64,
    pub rect: R4,
    pub start: Samples,
    pub end: Samples,
}

/// A MIDI clip being recorded on a track's lane.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RecordingLane {
    pub track: u64,
    pub rect: R4,
    /// The notes played so far, as rectangles in the lane.
    pub notes: Vec<R4>,
}

/// Last frame's arrangement geometry.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Layout {
    pub track_list: R4,
    pub ruler: R4,
    /// The loop strip along the top of the ruler.
    pub loop_strip: R4,
    pub timeline: R4,
    pub samples_per_px: f64,
    /// The grid as the ruler corner reads it ("Grid: 1/16", "Adaptive 1/32", "Snap off").
    pub grid_text: String,
    pub scroll: Samples,
    pub tracks: Vec<TrackRow>,
    pub clips: Vec<ClipRect>,
    /// MIDI passes being recorded: the growing clip outline per track with its provisional notes.
    pub recording: Vec<RecordingLane>,
    pub add_audio: R4,
    pub add_instrument: R4,
    pub add_midi: R4,
    /// Transport bar buttons (`play`, `stop`, `record`, `rtz`, `cycle`).
    pub transport: std::collections::BTreeMap<String, R4>,
    /// The icon each transport button shows (its Lucide name, e.g. `play`).
    pub transport_icons: std::collections::BTreeMap<String, String>,
    /// Where a dragged track header would land: a line between rows or a group header's outline.
    pub drop: Option<R4>,
}

#[derive(Debug, Default)]
pub struct ArrangeState {
    /// Grid mode, triplets and snap (view state; not saved in the session).
    pub grid: crate::grid::GridView,
    pub gesture: Option<Gesture>,
    /// The loop strip gesture in progress (the loop region itself lives in the session).
    pub loop_drag: Option<crate::loopbar::LoopDrag>,
    /// Vertical scroll of the track rows (points).
    pub vscroll: f32,
    pub layout: Layout,
    /// The track menu's open "Instrument" submenu entries (`value` is the plug-in id).
    pub menu: Vec<crate::mixer::MenuItem>,
    /// A track header being dragged (slice 2c).
    pub header_drag: Option<crate::groups::HeaderDrag>,
    /// The track rows in display order (Shift-click ranges over them).
    pub order: Vec<TrackId>,
    /// The track a plain or Ctrl click on a header last picked (Shift-click ranges from it).
    pub anchor: Option<TrackId>,
    /// Composite gestures started, to give each its own merge key.
    pub serial: u64,
    /// The menu bar's Track menu and its group entries as last drawn (slice 2c).
    pub bar_menu: Vec<crate::mixer::MenuItem>,
}

/// Timeline mapping: x ↔ samples, from the session's shared zoom (`edit.zoom`).
#[derive(Clone, Copy)]
pub(crate) struct Map {
    pub(crate) x0: f32,
    pub(crate) scroll: Samples,
    pub(crate) spp: f64,
    /// The grid settings, so every consumer of a map snaps to the lines drawn.
    pub(crate) grid: crate::grid::GridView,
}

impl Map {
    pub(crate) fn x(&self, at: Samples) -> f32 {
        self.x0 + ((at - self.scroll) as f64 / self.spp) as f32
    }
    pub(crate) fn at(&self, x: f32) -> Samples {
        self.scroll + (f64::from(x - self.x0) * self.spp).round() as Samples
    }
}

/// Height of one take lane (slice 6).
pub const TAKE_H: f32 = 22.0;

/// Row geometry: each row's top (content coordinates, before scrolling) and full height. A row is
/// the track's lane (`rh`) plus its take lanes when they are shown, then its automation lanes.
pub(crate) struct RowGeo {
    pub tops: Vec<f32>,
    pub heights: Vec<f32>,
    pub rh: f32,
}

impl RowGeo {
    fn new(app: &StudioApp, rows: &[&Track], rh: f32) -> RowGeo {
        let mut tops = Vec::with_capacity(rows.len());
        let mut heights = Vec::with_capacity(rows.len());
        let mut acc = 0.0;
        for t in rows {
            let h = rh + takes_shown(app, t) as f32 * TAKE_H + crate::automation::lanes_height(app, t);
            tops.push(acc);
            heights.push(h);
            acc += h;
        }
        RowGeo { tops, heights, rh }
    }
    fn total(&self) -> f32 {
        self.tops.last().zip(self.heights.last()).map_or(0.0, |(t, h)| t + h)
    }
    /// The row at content height `y`.
    fn index_at(&self, y: f32) -> Option<usize> {
        if y < 0.0 {
            return None;
        }
        (0..self.tops.len()).find(|i| y >= self.tops[*i] && y < self.tops[*i] + self.heights[*i])
    }
}

/// How many take lanes a track shows: all its playlists when it has several and they are open.
pub(crate) fn takes_shown(app: &StudioApp, t: &Track) -> usize {
    if t.playlists.len() > 1 && app.ui.takes_open.contains(&t.id) { t.playlists.len() } else { 0 }
}

/// A row in the track list: not hidden, and not in a collapsed group (view state, slice 2c).
fn shown(app: &StudioApp, s: &Session, t: &Track) -> bool {
    !t.hidden && !app.tucked(s, t)
}

pub fn show(app: &mut StudioApp, ui: &mut Ui) {
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(TOKENS.bg)).show(ui, |ui| arrangement(app, ui));
}

fn arrangement(app: &mut StudioApp, ui: &mut Ui) {
    let full = ui.max_rect();
    let hw = app.ui.header_width.clamp(160.0, 480.0);
    let rh = app.ui.track_height.clamp(24.0, 200.0);
    let list = Rect::from_min_max(pos2(full.min.x, full.min.y + RULER_H), pos2(full.min.x + hw, full.max.y));
    let ruler = Rect::from_min_max(pos2(full.min.x + hw, full.min.y), pos2(full.max.x, full.min.y + RULER_H));
    let tl = Rect::from_min_max(pos2(full.min.x + hw, full.min.y + RULER_H), full.max);
    let corner = Rect::from_min_max(full.min, pos2(full.min.x + hw, full.min.y + RULER_H));
    let s = app.engine.session_arc();
    let map = Map { x0: tl.min.x, scroll: s.edit.zoom.scroll, spp: s.edit.zoom.samples_per_px.max(0.01), grid: app.arrange.grid };
    let p = ui.painter();
    p.rect_filled(corner, 0.0, TOKENS.ruler);
    p.with_clip_rect(corner).text(
        pos2(corner.max.x - 8.0, corner.max.y - 9.0),
        Align2::RIGHT_CENTER,
        crate::grid::label(&s, map.spp, &map.grid),
        FontId::proportional(11.0),
        TOKENS.text_dim,
    );
    p.rect_filled(list, 0.0, TOKENS.panel);

    // Rows.
    let rows: Vec<&Track> = s.tracks.iter().filter(|t| shown(app, &s, t)).collect();
    app.arrange.order = rows.iter().map(|t| t.id).collect();
    let geo = RowGeo::new(app, &rows, rh);
    let content_h = geo.total() + 60.0;
    app.finish.layout.takes.clear();
    app.finish.layout.chips.clear();
    app.arrange.vscroll = app.arrange.vscroll.clamp(0.0, (content_h - tl.height()).max(0.0));
    let mut layout = Layout {
        track_list: r4(list),
        ruler: r4(ruler),
        timeline: r4(tl),
        samples_per_px: map.spp,
        grid_text: crate::grid::label(&s, map.spp, &map.grid),
        scroll: map.scroll,
        transport: std::mem::take(&mut app.arrange.layout.transport),
        transport_icons: std::mem::take(&mut app.arrange.layout.transport_icons),
        ..Layout::default()
    };
    let selected_tracks = s.edit.selected_tracks.clone();
    let focus = app.focus_track();
    app.arrange.menu.clear();
    for (i, t) in rows.iter().enumerate() {
        let y0 = tl.min.y + geo.tops[i] - app.arrange.vscroll;
        let header = Rect::from_min_max(pos2(list.min.x, y0), pos2(list.max.x, y0 + geo.heights[i]));
        let lane = Rect::from_min_max(pos2(tl.min.x, y0), pos2(tl.max.x, y0 + rh));
        let mut row = TrackRow {
            id: t.id.0,
            name: t.name.clone(),
            kind: t.kind,
            header: r4(header),
            lane: r4(lane),
            folder: crate::groups::parent_of(&s, t).map(|f| f.0),
            depth: crate::groups::depth(&s, t),
            collapsed: t.is_folder() && app.collapsed(t.id),
            frozen: t.freeze.is_some(),
            ..TrackRow::default()
        };
        if header.max.y >= list.min.y && header.min.y <= list.max.y {
            let selected = selected_tracks.contains(&t.id) || focus == Some(t.id);
            track_header(app, ui, t, header, list, selected, &mut row);
        }
        let lane_fill = if i % 2 == 0 { TOKENS.lane } else { TOKENS.lane_alt };
        let row_rect = Rect::from_min_max(lane.min, pos2(lane.max.x, y0 + geo.heights[i]));
        ui.painter().with_clip_rect(tl).rect_filled(row_rect, 0.0, lane_fill);
        if row.frozen {
            // A frozen lane reads cold: the accent, faint, over the lane.
            ui.painter().with_clip_rect(tl).rect_filled(row_rect, 0.0, TOKENS.accent.gamma_multiply(0.12));
        }
        layout.tracks.push(row);
    }
    // Add Track buttons below the rows.
    let below = tl.min.y + geo.total() - app.arrange.vscroll + 8.0;
    let add_a = Rect::from_min_size(pos2(list.min.x + 8.0, below), vec2((hw - 24.0) / 2.0, 22.0));
    let add_i = Rect::from_min_size(pos2(add_a.max.x + 8.0, below), add_a.size());
    let add_m = Rect::from_min_size(pos2(add_a.min.x, below + 30.0), add_a.size());
    if add_a.min.y < list.max.y {
        let clip = ui.painter().with_clip_rect(list);
        // Audio adds a mono track (the common case: one guitar or mic); its chevron picks mono or stereo.
        let chev_w = 18.0;
        let main = Rect::from_min_max(add_a.min, pos2(add_a.max.x - chev_w, add_a.max.y));
        let chev = Rect::from_min_max(pos2(add_a.max.x - chev_w, add_a.min.y), add_a.max);
        let main_resp = ui.interact(main.intersect(list), ui.id().with("add_audio"), Sense::click());
        let chev_resp = ui.interact(chev.intersect(list), ui.id().with("add_audio_format"), Sense::click());
        let hot = main_resp.hovered() || chev_resp.hovered();
        clip.rect_filled(add_a, crate::theme::RADIUS, if hot { TOKENS.button_hover } else { TOKENS.button });
        crate::icons::paint_labeled(&clip, main, false, crate::icons::Icon::Plus, 12.0, "Audio", FontId::proportional(12.0), TOKENS.text);
        crate::icons::paint(&clip, chev.center(), crate::icons::Icon::ChevronDown, 10.0, TOKENS.text_dim);
        if main_resp.clicked() {
            let _ = app.add_audio_track("mono");
        }
        egui::Popup::menu(&chev_resp).width(110.0).show(|ui| {
            if ui.button("Mono").clicked() {
                let _ = app.add_audio_track("mono");
                ui.close();
            }
            if ui.button("Stereo").clicked() {
                let _ = app.add_audio_track("stereo");
                ui.close();
            }
        });
        let resp = ui.interact(add_i.intersect(list), ui.id().with("add_instrument"), Sense::click());
        clip.rect_filled(add_i, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
        crate::icons::paint_labeled(&clip, add_i, false, crate::icons::Icon::Plus, 12.0, "Instrument", FontId::proportional(12.0), TOKENS.text);
        if resp.clicked() {
            let _ = app.add_track("instrument");
        }
        let resp = ui.interact(add_m.intersect(list), ui.id().with("add_midi"), Sense::click());
        clip.rect_filled(add_m, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
        crate::icons::paint_labeled(&clip, add_m, false, crate::icons::Icon::Plus, 12.0, "MIDI", FontId::proportional(12.0), TOKENS.text);
        if resp.clicked() {
            let _ = app.add_track("midi");
        }
    }
    layout.add_audio = r4(add_a);
    layout.add_instrument = r4(add_i);
    layout.add_midi = r4(add_m);
    ui.painter().line_segment([pos2(list.max.x, full.min.y), pos2(list.max.x, full.max.y)], Stroke::new(1.0, TOKENS.line));

    grid_and_ruler(app, ui, &s, map, ruler, tl);
    clips(app, ui, &s, map, tl, &rows, &geo, &mut layout);
    group_summaries(app, ui, &s, map, tl, &mut layout);
    header_drag(app, ui, &s, &mut layout);
    crate::keys::lane_hints(app, ui, tl, &layout.tracks);
    timeline_input(app, ui, &s, map, tl, &rows, &geo, &layout);
    takes(app, ui, &s, map, tl, list, &rows, &geo, &layout);
    let vscroll = app.arrange.vscroll;
    crate::automation::lanes(app, ui, &s, map, tl, list, &rows, &geo, vscroll);
    let strip = Rect::from_min_max(ruler.min, pos2(ruler.max.x, ruler.min.y + crate::loopbar::STRIP_H));
    crate::loopbar::strip(app, ui, &s, map, strip);
    layout.loop_strip = r4(strip);
    ruler_input(app, ui, map, Rect::from_min_max(pos2(ruler.min.x, strip.max.y), ruler.max));
    overlays(app, ui, &s, map, ruler, tl, &rows, &geo, &mut layout);
    scroll_and_zoom(app, ui, map, full, tl, content_h);
    follow_playhead(app, map, tl);
    app.arrange.layout = layout;
}

/// One track's header: color strip, name, M S R I toggles, volume readout.
fn track_header(app: &mut StudioApp, ui: &mut Ui, t: &Track, r: Rect, list: Rect, selected: bool, row: &mut TrackRow) {
    let p = ui.painter().with_clip_rect(list);
    let bg = if selected { TOKENS.header_selected } else { TOKENS.header };
    p.rect_filled(r.shrink2(vec2(0.0, 0.5)), 0.0, bg);
    // Group members sit indented under their group (slice 2c); a group has a disclosure triangle.
    let indent = row.depth as f32 * crate::groups::INDENT;
    let tri = if t.is_folder() { 14.0 } else { 0.0 };
    let tx = r.min.x + indent + tri;
    p.rect_filled(Rect::from_min_size(pos2(r.min.x + indent, r.min.y), vec2(5.0, r.height())), 0.0, theme::rgb(t.color));
    p.line_segment([pos2(r.min.x, r.max.y), pos2(r.max.x, r.max.y)], Stroke::new(1.0, TOKENS.line));
    let name_rect = Rect::from_min_max(pos2(tx + 12.0, r.min.y + 4.0), pos2(r.max.x - 114.0, r.min.y + 20.0));
    row.name_rect = r4(Rect::from_min_max(pos2(name_rect.min.x - 4.0, name_rect.min.y - 2.0), pos2(name_rect.max.x, name_rect.max.y + 2.0)));
    p.with_clip_rect(name_rect.intersect(list)).text(name_rect.left_center(), Align2::LEFT_CENTER, &t.name, FontId::proportional(13.0), TOKENS.text);
    let kind = match t.kind {
        TrackKind::Instrument => t.instrument.as_ref().map_or("Instrument".to_string(), |i| short_plugin(&app.plugin_name(&i.plugin, true))),
        TrackKind::Audio => format!("Audio · {}", t.format.label()),
        TrackKind::Folder => {
            let n = app.engine.session().tracks.iter().filter(|x| x.folder == Some(t.id) && x.id != t.id).count();
            format!("Group · {n} track{}", if n == 1 { "" } else { "s" })
        }
        k => format!("{k:?}"),
    };
    row.subtitle = kind.clone();
    p.text(pos2(tx + 12.0, r.min.y + 30.0), Align2::LEFT_CENTER, kind, FontId::proportional(10.5), TOKENS.text_dim);
    // Toggles, right-aligned on the first line.
    let m = &t.mixer;
    let mut x = r.max.x - 6.0;
    let mut toggle = |app: &mut StudioApp, label: &str, on: bool, color: Color32, cmd: &str, enabled: bool| -> R4 {
        let b = Rect::from_min_size(pos2(x - 20.0, r.min.y + 5.0), vec2(19.0, 16.0));
        x -= 22.0;
        let resp = ui.interact(b.intersect(list), ui.id().with((cmd, t.id.0)), Sense::click());
        let fill = if on {
            color
        } else if resp.hovered() && enabled {
            TOKENS.button_hover
        } else {
            TOKENS.button
        };
        p.rect_filled(b, crate::theme::RADIUS_SM, fill);
        let fg = if on {
            theme::text_on(color)
        } else if enabled {
            TOKENS.text
        } else {
            TOKENS.text_dim.gamma_multiply(0.5)
        };
        p.text(b.center(), Align2::CENTER_CENTER, label, FontId::proportional(11.0), fg);
        if resp.clicked() && enabled {
            let _ = app.run(cmd, json!({"tracks": [t.id.0], "value": !on}));
        }
        r4(b)
    };
    let records = t.kind.has_playlist();
    row.monitor = toggle(app, "I", m.input_monitor, TOKENS.monitor, "mix.input_monitor", t.kind == TrackKind::Audio);
    row.arm = toggle(app, "R", m.record_arm, TOKENS.record, "mix.record_arm", records);
    row.solo = toggle(app, "S", m.solo, TOKENS.solo, "mix.solo", true);
    row.mute = toggle(app, "M", m.mute, TOKENS.mute, "mix.mute", true);
    // Slice 7: A shows the track's automation lanes (Ableton's automation toggle). View state.
    let shown = app.ui.auto_shown.contains(&t.id);
    let b = Rect::from_min_size(pos2(x - 20.0, r.min.y + 5.0), vec2(19.0, 16.0));
    let resp = ui.interact(b.intersect(list), ui.id().with(("auto_toggle", t.id.0)), Sense::click());
    let fill = if shown {
        TOKENS.accent
    } else if resp.hovered() {
        TOKENS.button_hover
    } else {
        TOKENS.button
    };
    p.rect_filled(b, crate::theme::RADIUS_SM, fill);
    p.text(b.center(), Align2::CENTER_CENTER, "A", FontId::proportional(11.0), if shown { theme::text_on(TOKENS.accent) } else { TOKENS.text });
    if resp.on_hover_text(if shown { "Hide automation" } else { "Show automation" }).clicked() {
        app.toggle_automation(t.id);
    }
    row.automation = r4(b);
    row.automation_shown = shown;
    // Volume readout: drag sideways to change, double-click for 0 dB.
    let vol = Rect::from_min_size(pos2(r.max.x - 72.0, r.min.y + 23.0), vec2(66.0, 15.0));
    row.volume = r4(vol);
    let resp = ui.interact(vol.intersect(list), ui.id().with(("vol", t.id.0)), Sense::click_and_drag());
    p.rect_filled(vol, crate::theme::RADIUS_SM, TOKENS.bg);
    let db = app.followed(t, &crate::automation::Lane::Volume).unwrap_or(m.volume_db);
    let text = if db <= -144.0 { "-inf dB".to_string() } else { format!("{db:+.1} dB") };
    p.text(vol.center(), Align2::CENTER_CENTER, text, crate::theme::mono(ui.ctx(), 10.5), if resp.hovered() { TOKENS.accent } else { TOKENS.text });
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }
    if resp.drag_started() {
        app.arrange.gesture = Some(Gesture::Volume { track: t.id, start_db: db });
    }
    if resp.dragged()
        && let Some(Gesture::Volume { track, start_db }) = app.arrange.gesture.clone()
        && track == t.id
    {
        let dx = ui.input(|i| i.pointer.press_origin().zip(i.pointer.latest_pos()).map_or(0.0, |(a, b)| b.x - a.x));
        let fine = ui.input(|i| i.modifiers.shift);
        let target = (start_db + dx * if fine { 0.02 } else { 0.1 }).clamp(-80.0, 12.0);
        let _ = app.engine.execute_merged("mix.volume", &json!({"tracks": [t.id.0], "db": target}), &crate::mixer::fader_key(t.id));
        crate::automation::note_pass_edit(app);
    }
    if resp.drag_stopped() {
        app.arrange.gesture = None;
    }
    if resp.double_clicked() {
        let _ = app.run("mix.volume", json!({"tracks": [t.id.0], "db": 0.0}));
    }
    // Freeze: the snowflake left of the volume readout (Unfreeze when frozen).
    if matches!(t.kind, TrackKind::Audio | TrackKind::Instrument) {
        let frozen = t.freeze.is_some();
        let b = Rect::from_min_size(pos2(vol.min.x - 24.0, r.min.y + 22.0), vec2(19.0, 16.0));
        let resp = ui.interact(b.intersect(list), ui.id().with(("freeze", t.id.0)), Sense::click());
        let fill = if frozen {
            TOKENS.accent
        } else if resp.hovered() {
            TOKENS.button_hover
        } else {
            TOKENS.button
        };
        p.rect_filled(b, crate::theme::RADIUS_SM, fill);
        crate::icons::paint(&p, b.center(), crate::icons::Icon::Snowflake, 13.0, if frozen { theme::text_on(TOKENS.accent) } else { TOKENS.text });
        if resp.on_hover_text(if frozen { "Unfreeze" } else { "Freeze: render to audio and turn the plug-ins off" }).clicked() {
            let _ = app.toggle_freeze(t.id);
        }
        row.freeze_button = r4(b);
    }
    // Meter: a thin bar at the right edge.
    if let Some(d) = app.meters.get(&t.id) {
        let lvl = d.level[0].max(d.level[1]).clamp(0.0, 1.0);
        let h = (r.height() - 4.0) * meter_pos(lvl);
        let bar = Rect::from_min_max(pos2(r.max.x - 3.0, r.max.y - 2.0 - h), pos2(r.max.x - 1.0, r.max.y - 2.0));
        p.rect_filled(bar, 0.0, if d.clip { TOKENS.record } else { TOKENS.monitor });
    }
    // Name area: click selects the track (Shift ranges, Ctrl adds); a drag moves it (slice 2c);
    // right-click opens the track menu.
    let name_area = Rect::from_min_max(r.min, pos2(r.max.x - 114.0, r.max.y));
    let resp = ui.interact(name_area.intersect(list), ui.id().with(("hdr", t.id.0)), Sense::click_and_drag());
    if resp.clicked() {
        let m = ui.input(|i| i.modifiers);
        app.click_header(t.id, m.shift, m.command || m.ctrl);
    }
    // Slice 6: double-click the name to rename the track in place.
    // Only on the name itself; a double-click elsewhere on the header keeps selecting.
    if resp.double_clicked() && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| crate::finish::rect(row.name_rect).contains(p)) {
        app.start_rename(crate::finish::RenameTarget::Track(t.id), row.name_rect);
    }
    if resp.drag_started() && t.kind != TrackKind::Master {
        app.arrange.header_drag = Some(crate::groups::HeaderDrag { track: t.id, drop: None });
    }
    if t.is_folder() {
        disclosure(app, ui, t, Rect::from_min_size(pos2(r.min.x + indent + 8.0, r.min.y + 5.0), vec2(14.0, 14.0)), list, row);
    }
    let id = t.id;
    let kind = t.kind;
    let has_instrument = t.instrument.is_some();
    resp.context_menu(|ui| track_menu(app, ui, id, kind, has_instrument));
}

/// A group's disclosure chevron: collapses or expands its members (view state only).
fn disclosure(app: &mut StudioApp, ui: &mut Ui, t: &Track, b: Rect, list: Rect, row: &mut TrackRow) {
    row.disclosure = r4(b);
    let resp = ui.interact(b.intersect(list), ui.id().with(("disclose", t.id.0)), Sense::click());
    let ink = if resp.hovered() { TOKENS.text } else { TOKENS.text_dim };
    let icon = if row.collapsed { crate::icons::Icon::ChevronRight } else { crate::icons::Icon::ChevronDown };
    crate::icons::paint(&ui.painter().with_clip_rect(list), b.center(), icon, 14.0, ink);
    if resp.clicked() {
        app.toggle_group(t.id);
    }
}

/// A collapsed group's lane: its members' clips as thin bars in their colors, one row of bars per
/// member, read-only (they are not in `layout.clips`, so a click there is a click on the lane).
fn group_summaries(app: &StudioApp, ui: &Ui, s: &Session, map: Map, tl: Rect, layout: &mut Layout) {
    let p = ui.painter().with_clip_rect(tl);
    for row in layout.tracks.iter_mut().filter(|r| r.collapsed) {
        let g = TrackId(row.id);
        let members: Vec<&Track> = s.tracks.iter().filter(|t| t.id != g && crate::groups::within(s, t.id, g) && t.kind.has_playlist()).collect();
        let lane = Rect::from_min_max(pos2(row.lane[0], row.lane[1]), pos2(row.lane[2], row.lane[3]));
        let n = members.len().max(1) as f32;
        let h = ((lane.height() - 8.0) / n).clamp(1.0, 5.0);
        let pitch = ((lane.height() - 8.0) / n).max(h);
        for (k, t) in members.iter().enumerate() {
            let y = lane.min.y + 4.0 + k as f32 * pitch;
            for c in t.clips() {
                let bar = Rect::from_min_max(pos2(map.x(c.start), y), pos2(map.x(c.end()).max(map.x(c.start) + 2.0), y + h));
                row.summary_bars += 1;
                if bar.max.x >= tl.min.x && bar.min.x <= tl.max.x {
                    p.rect_filled(bar, crate::theme::RADIUS_SM, theme::rgb(c.color.unwrap_or(t.color)));
                }
            }
        }
    }
    let _ = app;
}

/// A track header drag in progress (slice 2c): show where it would land; on release, move it
/// there (`StudioApp::drop_track`, one undo step).
fn header_drag(app: &mut StudioApp, ui: &Ui, s: &Session, layout: &mut Layout) {
    let Some(drag) = app.arrange.header_drag else { return };
    let (down, pos) = ui.input(|i| (i.pointer.primary_down(), i.pointer.latest_pos()));
    let at = pos.and_then(|p| crate::groups::drop_at(s, &layout.tracks, drag.track, p.y));
    if down {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        app.arrange.header_drag = Some(crate::groups::HeaderDrag { drop: at.map(|a| a.0), ..drag });
        if let Some((d, r)) = at {
            let rect = Rect::from_min_max(pos2(r[0], r[1]), pos2(r[2], r[3]));
            let p = ui.painter();
            match d {
                crate::groups::Drop::Gap { .. } => p.rect_filled(rect, 0.0, TOKENS.accent),
                crate::groups::Drop::Into(_) => {
                    p.rect_stroke(rect.shrink(1.0), crate::theme::RADIUS_SM, Stroke::new(2.0, TOKENS.accent), StrokeKind::Inside)
                }
            };
            layout.drop = Some(r);
        }
        return;
    }
    app.arrange.header_drag = None;
    if let Some((d, _)) = at {
        let _ = app.drop_track(drag.track, d);
    }
}

/// The track's right-click menu.
fn track_menu(app: &mut StudioApp, ui: &mut Ui, id: TrackId, kind: TrackKind, has_instrument: bool) {
    if kind == TrackKind::Instrument {
        // Instrument ▸: the device chain's instrument menu (a click in its filter box keeps it open).
        let config = egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        let mut picked = None;
        let (button, _) = egui::containers::menu::SubMenuButton::new("Instrument").config(config).ui(ui, |ui| {
            let lists = app.instrument_lists();
            let current = app.engine.session().track(id).and_then(|t| t.instrument.as_ref()).map(|i| i.plugin.clone());
            let mut filter = std::mem::take(&mut app.mixer.instrument_filter);
            let mut filter_rect = app.mixer.filter_rect;
            let mut items = Vec::new();
            picked = crate::devices::instrument_menu(ui, &lists, current.as_deref(), &mut filter, &mut filter_rect, &mut items);
            app.mixer.instrument_filter = filter;
            app.mixer.filter_rect = filter_rect;
            app.arrange.menu.extend(items);
        });
        app.arrange.menu.push(crate::mixer::MenuItem { label: "Instrument".into(), value: String::new(), rect: r4(button.rect), visible: true });
        if let Some(plugin) = picked {
            app.mixer.instrument_filter.clear();
            let _ = app.set_instrument(id, &plugin);
            ui.close();
        }
        if ui.add_enabled(has_instrument && app.player.is_some(), egui::Button::new("Open Instrument Editor")).clicked() {
            app.want_editor((id, soundcraft_mix::INSTRUMENT_SLOT));
            ui.close();
        }
        ui.separator();
    }
    if kind.is_midi() && ui.button("New MIDI Clip at Cursor").clicked() {
        let _ = app.new_midi_clip(Some(id));
        ui.close();
    }
    if kind == TrackKind::Audio {
        width_item(app, ui, id);
    }
    if matches!(kind, TrackKind::Audio | TrackKind::Instrument) {
        let frozen = app.engine.session().track(id).is_some_and(|t| t.freeze.is_some());
        let label = if frozen { "Unfreeze" } else { "Freeze" };
        let b = ui.button(label);
        app.arrange.menu.push(crate::mixer::MenuItem::at(ui, label.into(), String::new(), b.rect));
        if b.clicked() {
            let _ = app.toggle_freeze(id);
            ui.close();
        }
        let b = ui.button("Flatten");
        app.arrange.menu.push(crate::mixer::MenuItem::at(ui, "Flatten".into(), String::new(), b.rect));
        if b.clicked() {
            let _ = app.flatten_track(id);
            ui.close();
        }
    }
    if kind != TrackKind::Master {
        crate::groups::menu_items(app, ui, Some(id));
        ui.separator();
    }
    ui.menu_button("Color", |ui| {
        for (i, c) in PALETTE.iter().enumerate() {
            let (r, resp) = ui.allocate_exact_size(vec2(120.0, 16.0), Sense::click());
            // The outline carries the 3:1 boundary where a pale swatch meets the light popup.
            ui.painter().rect_filled(r.shrink(1.0), crate::theme::RADIUS_SM, theme::rgb(*c));
            ui.painter().rect_stroke(r.shrink(1.0), crate::theme::RADIUS_SM, Stroke::new(1.0, TOKENS.text_dim), StrokeKind::Inside);
            if resp.clicked() {
                let _ = app.run("track.color", json!({"tracks": [id.0], "color": c}));
                ui.close();
            }
            let _ = i;
        }
    });
    ui.separator();
    let del = ui.button("Delete Track");
    app.arrange.menu.push(crate::mixer::MenuItem::at(ui, "Delete Track".into(), String::new(), del.rect));
    if del.clicked() {
        let _ = app.request_delete_tracks(vec![id]);
        ui.close();
    }
}

/// "Make Stereo" on a mono audio track, "Make Mono" on a stereo one (`track.change_width`, one
/// undo step). Other widths (surround) are left to the engine command.
pub(crate) fn width_item(app: &mut StudioApp, ui: &mut Ui, id: TrackId) -> Option<egui::Response> {
    let channels = app.engine.session().track(id)?.channels();
    let (label, format) = match channels {
        1 => ("Make Stereo", "stereo"),
        2 => ("Make Mono", "mono"),
        _ => return None,
    };
    let reason = app
        .width_change_disabled_reason()
        .map(str::to_string)
        .or_else(|| app.engine.session().track(id).and_then(|track| app.width_input(track, format).err()));
    let response = ui.add_enabled(reason.is_none(), egui::Button::new(label)).on_disabled_hover_text(reason.unwrap_or_default());
    if response.clicked() {
        let _ = app.run("track.change_width", json!({"tracks": [id.0], "format": format}));
        ui.close();
    }
    Some(response)
}

/// Track colors offered in the track menu (muted, distinguishable on the dark lanes).
pub(crate) const PALETTE: [[u8; 3]; 10] = [
    [0xd9, 0x5f, 0x5f],
    [0xe0, 0x8c, 0x4a],
    [0xd6, 0xbe, 0x4f],
    [0x8f, 0xc0, 0x5a],
    [0x4f, 0xb8, 0x8c],
    [0x4a, 0xb4, 0xc8],
    [0x5b, 0x8f, 0xdb],
    [0x8e, 0x74, 0xd6],
    [0xc7, 0x6a, 0xc0],
    [0x9a, 0x9c, 0xa3],
];

fn short_plugin(id: &str) -> String {
    let name = id.rsplit(':').next().unwrap_or(id);
    if name.len() > 24 { format!("{}…", name.chars().take(22).collect::<String>()) } else { name.to_string() }
}

/// 0..1 meter position for a linear peak (-60 dB at the bottom).
fn meter_pos(lin: f32) -> f32 {
    if lin <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * lin.log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn bar_at(s: &Session, bar: i64) -> Samples {
    s.tempo.samples_at_bar_beat(BarBeat { bar, beat: 1, tick: 0 }, s.sample_rate)
}

/// Bar and beat lines across the timeline, and the ruler with bar numbers.
fn grid_and_ruler(app: &StudioApp, ui: &Ui, s: &Session, map: Map, ruler: Rect, tl: Rect) {
    let p = ui.painter();
    p.rect_filled(ruler, 0.0, TOKENS.ruler);
    p.line_segment([pos2(ruler.min.x, ruler.max.y), pos2(ruler.max.x, ruler.max.y)], Stroke::new(1.0, TOKENS.line));
    let first = s.tempo.bar_beat_at(map.at(tl.min.x).max(0), s.sample_rate).bar.max(1);
    let bar_px = (map.x(bar_at(s, first + 1)) - map.x(bar_at(s, first))).max(0.01);
    // Label every n bars so labels keep about 40 points apart.
    let every = [1, 2, 4, 8, 16, 32, 64, 128].into_iter().find(|n| bar_px * *n as f32 >= 40.0).unwrap_or(256);
    let gp = p.with_clip_rect(tl);
    let rp = p.with_clip_rect(ruler);
    let ppt = crate::grid::px_per_tick(s, map.spp);
    let step = crate::grid::ticks(ppt, crate::grid::bar_ticks(s), &map.grid);
    let sub_step = crate::grid::sub_line_step(step, ppt, soundcraft_time::TICKS_PER_QUARTER);
    for bar in (first..).take(4000) {
        let x = map.x(bar_at(s, bar));
        if x > tl.max.x {
            break;
        }
        let beats = s.tempo.meter_at_tick(s.tempo.bar_start_tick(bar)).numerator.max(1);
        let next = map.x(bar_at(s, bar + 1));
        if (next - x) / beats as f32 >= 10.0 {
            for b in 1..beats {
                let bx = map.x(s.tempo.samples_at_bar_beat(BarBeat { bar, beat: i64::from(b) + 1, tick: 0 }, s.sample_rate));
                gp.line_segment([pos2(bx, tl.min.y), pos2(bx, tl.max.y)], Stroke::new(1.0, TOKENS.grid_beat));
                rp.line_segment([pos2(bx, ruler.max.y - 5.0), pos2(bx, ruler.max.y)], Stroke::new(1.0, TOKENS.text_dim.gamma_multiply(0.5)));
            }
        }
        // Sub-beat lines at the grid step, fainter than the beats; skipped where they would be
        // denser than `MIN_DRAW_PX` (the snap still uses the step).
        if let Some(step) = sub_step {
            let (t0, t1) = (s.tempo.bar_start_tick(bar), s.tempo.bar_start_tick(bar + 1));
            let beat = s.tempo.meter_at_tick(t0).ticks_per_beat().max(1);
            // Multiples of the step from tick 0, as `snap` rounds to them.
            for tick in ((t0 + step - 1).div_euclid(step) * step..t1).step_by(step as usize) {
                if (tick - t0) % beat == 0 {
                    continue;
                }
                let sx = map.x(s.tempo.tick_to_samples(tick, s.sample_rate));
                if sx >= tl.min.x - 1.0 && sx <= tl.max.x {
                    gp.line_segment([pos2(sx, tl.min.y), pos2(sx, tl.max.y)], Stroke::new(1.0, TOKENS.grid_sub));
                }
            }
        }
        if (bar - 1) % every == 0 {
            gp.line_segment([pos2(x, tl.min.y), pos2(x, tl.max.y)], Stroke::new(1.0, TOKENS.grid_bar));
            rp.line_segment([pos2(x, ruler.min.y + crate::loopbar::STRIP_H + 10.0), pos2(x, ruler.max.y)], Stroke::new(1.0, TOKENS.text_dim));
            rp.text(
                pos2(x + 4.0, ruler.min.y + crate::loopbar::STRIP_H + 9.0),
                Align2::LEFT_CENTER,
                bar.to_string(),
                FontId::proportional(11.0),
                TOKENS.text,
            );
        } else if bar_px >= 6.0 {
            gp.line_segment([pos2(x, tl.min.y), pos2(x, tl.max.y)], Stroke::new(1.0, TOKENS.grid_beat));
        }
    }
    let _ = app;
}

/// Does a clip drag with these modifiers copy instead of move? Ctrl (Cmd on a Mac; egui's
/// `command`) is Ableton's copy-drag; Alt is Logic's. The shortcut table binds neither to a drag.
pub fn drag_copies(m: egui::Modifiers) -> bool {
    m.command || m.alt
}

/// Where a dragged clip's start lands: `start` moved by `dx` (through `snap`, the session grid's
/// snapped delta, unless Shift is held), never before zero. Moves and copies share this.
pub fn drop_start(start: Samples, dx: Samples, shift: bool, snap: impl Fn(Samples) -> Samples) -> Samples {
    start.saturating_add(if shift { dx } else { snap(dx) }).max(0)
}

/// The clip being dragged, as it would land: (track, start, end).
fn drag_preview(app: &StudioApp, ui: &Ui, map: Map, rows: &[&Track], tl: Rect, geo: &RowGeo) -> Option<(ClipId, TrackId, Samples, Samples, bool)> {
    let Some(Gesture::Clip { clip, track, kind, press, start, end }) = app.arrange.gesture.clone() else { return None };
    let (pos, m) = ui.input(|i| (i.pointer.latest_pos(), i.modifiers));
    let pos = pos?;
    let dx = f64::from(pos.x - press.x) * map.spp;
    let dx = dx.round() as Samples;
    Some(match kind {
        DragKind::Move => {
            let s = app.engine.session();
            let new_start = drop_start(start, dx, m.shift, |d| crate::automation::snap_delta(s, map.spp, &map.grid, start, d));
            let row = geo.index_at(pos.y - tl.min.y + app.arrange.vscroll);
            let dest = row.and_then(|i| rows.get(i)).map(|t| t.id).filter(|t| app.holds_clips(*t)).unwrap_or(track);
            (clip, dest, new_start, new_start + (end - start), drag_copies(m))
        }
        DragKind::TrimStart => (clip, track, (start + dx).clamp(0, end - 1), end, false),
        DragKind::TrimEnd => (clip, track, start, (end + dx).max(start + 1), false),
        DragKind::Loop => return None,
    })
}

/// Clips as solid regions in the track color, with a name strip and their content.
#[allow(clippy::too_many_arguments)]
fn clips(app: &StudioApp, ui: &Ui, s: &Session, map: Map, tl: Rect, rows: &[&Track], geo: &RowGeo, layout: &mut Layout) {
    let p = ui.painter().with_clip_rect(tl);
    let sel = &s.edit.selected_clips;
    let preview = drag_preview(app, ui, map, rows, tl, geo);
    let looping = loop_drag(app, ui, map);
    let rh = geo.rh;
    let row_y = |t: TrackId| rows.iter().position(|r| r.id == t).map(|i| tl.min.y + geo.tops[i] - app.arrange.vscroll);
    for t in rows {
        let Some(y0) = row_y(t.id) else { continue };
        for c in t.clips() {
            let rect = Rect::from_min_max(pos2(map.x(c.start), y0 + 1.0), pos2(map.x(c.end()).max(map.x(c.start) + 2.0), y0 + rh - 1.0));
            layout.clips.push(ClipRect { id: c.id.0, track: t.id.0, rect: r4(rect), start: c.start, end: c.end() });
            if rect.max.x < tl.min.x || rect.min.x > tl.max.x || rect.max.y < tl.min.y || rect.min.y > tl.max.y {
                continue;
            }
            let moving = preview.is_some_and(|(id, _, _, _, copy)| id == c.id && !copy);
            draw_clip(&p, s, map, tl, t, c, rect, sel.contains(&c.id), if moving { 0.35 } else { 1.0 });
        }
    }
    // A loop drag: the cycles (or copies) it would add as ghosts in the clip color at reduced alpha, and
    // the part it would cut off shaded.
    if let Some(l) = &looping
        && let Some((track, base)) = s.find_clip(l.clip).map(|(t, c)| (t, c.clone()))
        && let (Some(y0), Some(t)) = (row_y(track), s.track(track))
    {
        let span = |a: Samples, b: Samples| Rect::from_min_max(pos2(map.x(a), y0 + 1.0), pos2(map.x(b).max(map.x(a) + 2.0), y0 + rh - 1.0));
        for (a, b) in l.ghosts() {
            let mut g = base.clone();
            // The ghost plays the cycle as it would: its audio starts at the loop phase of `a`.
            let mut virt = base.clone();
            virt.looping = Some(base.looping.unwrap_or(ClipLoop { start: 0, end: base.length }));
            if let ClipContent::Audio { source, offset } = base.content {
                g.content = ClipContent::Audio { source, offset: offset.saturating_add(virt.content_pos(a - base.start)) };
            }
            g.start = a;
            g.length = b - a;
            g.looping = None;
            g.fade_in = Default::default();
            g.fade_out = Default::default();
            let rect = span(a, b);
            if rect.max.x >= tl.min.x && rect.min.x <= tl.max.x {
                draw_clip(&p, s, map, tl, t, &g, rect, false, 0.45);
            }
        }
        if let Some((a, b)) = l.cut() {
            p.rect_filled(span(a, b), crate::theme::RADIUS, TOKENS.header.gamma_multiply(0.6));
        }
    }
    // The dragged clip where it would land.
    if let Some((id, dest, a, b, _)) = preview
        && let (Some((_, c)), Some(y0)) = (s.find_clip(id), row_y(dest))
        && let Some(t) = s.track(dest)
    {
        let rect = Rect::from_min_max(pos2(map.x(a), y0 + 1.0), pos2(map.x(b).max(map.x(a) + 2.0), y0 + rh - 1.0));
        draw_clip(&p, s, map, tl, t, c, rect, true, 0.85);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_clip(p: &egui::Painter, s: &Session, map: Map, tl: Rect, t: &Track, c: &Clip, rect: Rect, selected: bool, alpha: f32) {
    let base = theme::rgb(c.color.unwrap_or(t.color));
    let base = if c.muted { theme::darken(base, 0.5) } else { base };
    let (body_col, strip_col, name_col) = theme::clip_colors(base, selected);
    let p = p.with_clip_rect(rect.intersect(tl));
    p.rect_filled(rect, crate::theme::RADIUS, body_col.gamma_multiply(alpha));
    let strip = Rect::from_min_max(rect.min, pos2(rect.max.x, (rect.min.y + CLIP_NAME_H).min(rect.max.y)));
    p.rect_filled(strip, crate::theme::top_corners(), strip_col.gamma_multiply(alpha));
    let name_x = rect.min.x.max(tl.min.x) + 4.0;
    p.with_clip_rect(strip.intersect(tl)).text(pos2(name_x, strip.center().y), Align2::LEFT_CENTER, &c.name, FontId::proportional(11.0), name_col);
    let body = Rect::from_min_max(pos2(rect.min.x, strip.max.y), rect.max).shrink2(vec2(0.0, 2.0));
    let ink = theme::darken(body_col, 0.62).gamma_multiply(alpha);
    if let Some(l) = c.looping.filter(|l| l.is_valid()) {
        draw_cycles(&p, s, map, tl, t, c, l, rect, strip, body, ink);
    } else if body.height() > 4.0 {
        match &c.content {
            ClipContent::Audio { source, .. } => {
                if let Some(audio) = s.pool.get(*source) {
                    let wp = p.with_clip_rect(body.intersect(tl));
                    crate::waveform::draw(&wp, s, tl, c, audio, t.channels().min(2), body, ink);
                }
            }
            ClipContent::Midi { sequence } => {
                let notes = &sequence.notes;
                if let (Some(lo), Some(hi)) = (notes.iter().map(|n| n.pitch).min(), notes.iter().map(|n| n.pitch).max()) {
                    let span = f32::from(hi.saturating_sub(lo)).max(11.0);
                    let clip_tick = s.tempo.samples_to_ticks(c.start, s.sample_rate);
                    for n in notes.iter().take(5000) {
                        let a = s.tempo.tick_to_samples(clip_tick.saturating_add(n.start), s.sample_rate);
                        let b = s.tempo.tick_to_samples(clip_tick.saturating_add(n.end()), s.sample_rate);
                        let fy = 1.0 - f32::from(n.pitch.saturating_sub(lo)) / span;
                        let y = body.min.y + 2.0 + fy * (body.height() - 6.0);
                        let r = Rect::from_min_max(pos2(map.x(a), y), pos2(map.x(b).max(map.x(a) + 2.0), y + 2.5));
                        p.rect_filled(r, 0.0, ink);
                    }
                }
            }
            ClipContent::Video { .. } => {}
        }
    }
    let stroke = if selected { Stroke::new(2.0, TOKENS.selection_outline) } else { Stroke::new(1.0, theme::darken(base, 0.45)) };
    p.rect_stroke(rect, crate::theme::RADIUS, stroke, StrokeKind::Inside);
}

/// A looped clip's cycles (ADR 0019): the first pass `0..loop.end`, then one segment per cycle,
/// each (clip-relative start, end, content position at its start), cut at the clip's length.
pub(crate) fn cycle_segments(c: &Clip, l: ClipLoop) -> impl Iterator<Item = (Samples, Samples, Samples)> {
    let (length, end, cycle) = (c.length, l.end, l.cycle());
    std::iter::once((0, end.min(length), 0)).chain(
        std::iter::successors(Some(end), move |x| x.checked_add(cycle)).take_while(move |x| *x < length).map(move |a| {
            let b = a.saturating_add(cycle).min(length);
            (a, b, l.start)
        }),
    )
}

/// Draw a looped clip's content once per cycle, with a divider tick at every seam.
#[allow(clippy::too_many_arguments)]
fn draw_cycles(
    p: &egui::Painter,
    s: &Session,
    map: Map,
    tl: Rect,
    t: &Track,
    c: &Clip,
    l: ClipLoop,
    rect: Rect,
    strip: Rect,
    body: Rect,
    ink: Color32,
) {
    let sx = |rel: Samples| rect.min.x + (rel as f64 / map.spp) as f32;
    for (i, (a, b, phase)) in cycle_segments(c, l).enumerate() {
        let (x0, x1) = (sx(a), sx(b).max(sx(a) + 1.0));
        if x1 < tl.min.x || x0 > tl.max.x {
            continue;
        }
        if i > 0 {
            // The seam: a tick through the header strip and a faint line down the body.
            p.line_segment([pos2(x0, strip.min.y), pos2(x0, strip.max.y)], Stroke::new(1.5, TOKENS.text_dim));
            p.line_segment([pos2(x0, strip.max.y), pos2(x0, rect.max.y)], Stroke::new(1.0, ink.gamma_multiply(0.6)));
        }
        if body.height() <= 4.0 {
            continue;
        }
        let seg = Rect::from_min_max(pos2(x0, body.min.y), pos2(x1, body.max.y));
        match &c.content {
            ClipContent::Audio { source, offset } => {
                if let Some(audio) = s.pool.get(*source) {
                    // The segment as a clip of its own that plays the file from its cycle phase.
                    let mut g = c.clone();
                    g.start = c.start.saturating_add(a);
                    g.length = (b - a).max(1);
                    g.looping = None;
                    g.content = ClipContent::Audio { source: *source, offset: offset.saturating_add(phase) };
                    if i > 0 {
                        g.fade_in = Default::default();
                    }
                    if b < c.length {
                        g.fade_out = Default::default();
                    }
                    let wp = p.with_clip_rect(seg.intersect(tl));
                    crate::waveform::draw(&wp, s, tl, &g, audio, t.channels().min(2), seg, ink);
                }
            }
            ClipContent::Midi { sequence } => {
                let notes = &sequence.notes;
                if let (Some(lo), Some(hi)) = (notes.iter().map(|n| n.pitch).min(), notes.iter().map(|n| n.pitch).max()) {
                    let span = f32::from(hi.saturating_sub(lo)).max(11.0);
                    let clip_tick = s.tempo.samples_to_ticks(c.start, s.sample_rate);
                    let wp = p.with_clip_rect(seg.intersect(tl));
                    for n in notes.iter().take(5000) {
                        let on = s.tempo.tick_to_samples(clip_tick.saturating_add(n.start), s.sample_rate) - c.start;
                        let off = s.tempo.tick_to_samples(clip_tick.saturating_add(n.end()), s.sample_rate) - c.start;
                        // Cycle 0 plays what starts before the loop end; later cycles what starts inside the range.
                        if on >= l.end || (i > 0 && on < l.start) || (i == 0 && on < 0) {
                            continue;
                        }
                        let (from, to) = (a + on - phase, a + off.min(l.end) - phase);
                        let fy = 1.0 - f32::from(n.pitch.saturating_sub(lo)) / span;
                        let y = body.min.y + 2.0 + fy * (body.height() - 6.0);
                        let r = Rect::from_min_max(pos2(sx(from), y), pos2(sx(to.min(b)).max(sx(from) + 2.0), y + 2.5));
                        wp.rect_filled(r, 0.0, ink);
                    }
                }
            }
            ClipContent::Video { .. } => {}
        }
    }
}

/// What is under a point: a clip (and whether near its start or end edge) and the row's track.
fn hit(layout: &Layout, pos: Pos2) -> (Option<(ClipId, TrackId, DragKind, Samples, Samples)>, Option<TrackId>) {
    let track = layout.tracks.iter().find(|r| pos.y >= r.lane[1] && pos.y < r.lane[3]).map(|r| TrackId(r.id));
    // Topmost (last drawn) first.
    let clip = layout
        .clips
        .iter()
        .rev()
        .find(|c| pos.x >= c.rect[0] && pos.x <= c.rect[2] && pos.y >= c.rect[1] && pos.y <= c.rect[3])
        .map(|c| (ClipId(c.id), TrackId(c.track), zone(c.rect, pos), c.start, c.end));
    (clip, track)
}

#[allow(clippy::too_many_arguments)]
fn timeline_input(app: &mut StudioApp, ui: &mut Ui, s: &Session, map: Map, tl: Rect, rows: &[&Track], geo: &RowGeo, layout: &Layout) {
    let resp = ui.interact(tl, ui.id().with("timeline"), Sense::click_and_drag());
    if let Some(pos) = resp.hover_pos()
        && app.arrange.gesture.is_none()
    {
        match hit(layout, pos).0 {
            Some((_, _, DragKind::TrimStart | DragKind::TrimEnd, _, _)) => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
            Some((_, _, DragKind::Loop, _, _)) => ui.ctx().set_cursor_icon(CursorIcon::Alias),
            Some(_) => ui.ctx().set_cursor_icon(CursorIcon::Grab),
            None => {}
        }
    }
    if app.arrange.gesture.is_none()
        && let Some(pos) = resp.hover_pos()
        && let Some((_, _, DragKind::Loop, _, _)) = hit(layout, pos).0
    {
        resp.clone().on_hover_text("Drag right to loop this clip (Shift: no snap, Alt: independent copies). Drag back to shorten or unloop.");
    }
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
        && let (Some((clip, track, kind, start, end)), _) = hit(layout, press)
    {
        if !s.edit.selected_clips.contains(&clip) {
            app.click_clip(clip, false);
        }
        app.arrange.gesture = Some(Gesture::Clip { clip, track, kind, press, start, end });
    }
    if resp.dragged() && matches!(app.arrange.gesture, Some(Gesture::Clip { kind: DragKind::Move, .. })) {
        let copy = ui.input(|i| drag_copies(i.modifiers));
        ui.ctx().set_cursor_icon(if copy { CursorIcon::Copy } else { CursorIcon::Grabbing });
    }
    if resp.dragged() && matches!(app.arrange.gesture, Some(Gesture::Clip { kind: DragKind::Loop, .. })) {
        ui.ctx().set_cursor_icon(CursorIcon::Alias);
    }
    if resp.drag_stopped() {
        let preview = drag_preview(app, ui, map, rows, tl, geo);
        let looped = loop_drag(app, ui, map);
        let gesture = app.arrange.gesture.take();
        if let Some(l) = looped {
            commit_loop(app, &l);
        }
        if let (Some(Gesture::Clip { kind, track, start, end, .. }), Some((clip, dest, a, b, copy))) = (gesture, preview) {
            commit_drag(app, kind, clip, track, dest, start, end, a, b, copy);
        }
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let additive = ui.input(|i| i.modifiers.shift || i.modifiers.command);
        match hit(layout, pos) {
            (Some((clip, ..)), _) => app.click_clip(clip, additive),
            (None, track) => app.click_empty(track, map.at(pos.x)),
        }
    }
    if resp.double_clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && let (None, Some(track)) = hit(layout, pos)
        && s.track(track).is_some_and(|t| t.kind.is_midi())
    {
        let _ = app.new_midi_clip(Some(track));
    }
    // Slice 3: double-click a MIDI clip to edit it in the piano roll; slice 6: double-click its
    // name strip to rename it.
    if resp.double_clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && let (Some((clip, ..)), _) = hit(layout, pos)
    {
        let strip = layout.clips.iter().find(|c| c.id == clip.0).map(|c| c.rect).filter(|r| pos.y <= r[1] + CLIP_NAME_H);
        match strip {
            Some(r) => {
                let x0 = r[0].max(tl.min.x);
                let field = [x0, r[1], r[2].min(tl.max.x).max(x0 + 160.0), r[1] + CLIP_NAME_H + 6.0];
                app.start_rename(crate::finish::RenameTarget::Clip(clip), field);
            }
            None => {
                app.open_keys(clip);
            }
        }
    }
    resp.context_menu(|ui| {
        if ui.button("Add Mono Audio Track").clicked() {
            let _ = app.add_audio_track("mono");
            ui.close();
        }
        if ui.button("Add Stereo Audio Track").clicked() {
            let _ = app.add_audio_track("stereo");
            ui.close();
        }
        if ui.button("Add Instrument Track").clicked() {
            let _ = app.add_track("instrument");
            ui.close();
        }
        if ui.button("Add MIDI Track").clicked() {
            let _ = app.add_track("midi");
            ui.close();
        }
        ui.separator();
        if ui.button("Paste at Cursor").clicked() {
            let _ = app.paste_at_cursor();
            ui.close();
        }
        if ui.button("Split at Cursor").clicked() {
            let _ = app.split_at_cursor();
            ui.close();
        }
    });
}

/// A loop-handle drag as it would land.
struct LoopDrag {
    clip: ClipId,
    /// The clip's end now, and the drop point (snapped unless Shift is held): the new end.
    from: Samples,
    to: Samples,
    /// A cycle's length, and the clip-time end of the first pass (where unlooping cuts to).
    cycle: Samples,
    first_end: Samples,
    /// The clip starts here.
    start: Samples,
    looped: bool,
    /// Alt: plain copies instead of cycles.
    independent: bool,
}

impl LoopDrag {
    /// The ghost spans, in timeline samples: beyond the clip's end the cycles (or copies) it
    /// would gain, the last one cut at the drop point.
    fn ghosts(&self) -> Vec<(Samples, Samples)> {
        if self.to <= self.from {
            return Vec::new();
        }
        if self.independent {
            return loop_ghosts(self.from, self.from - self.start, self.to);
        }
        // Cycle boundaries sit at first_end + k * cycle; the first ghost starts where the clip ends.
        let seam = self.start + self.first_end;
        let mut out = Vec::new();
        let mut a = self.from;
        while a < self.to && out.len() < MAX_LOOP_REPEATS {
            let next = seam + ((a - seam).div_euclid(self.cycle.max(1)) + 1) * self.cycle.max(1);
            out.push((a, next.min(self.to)));
            a = next;
        }
        out
    }
    /// The part of the clip the drop would cut off (dragging left).
    fn cut(&self) -> Option<(Samples, Samples)> {
        let to = if self.looped && !self.independent && self.to <= self.start + self.first_end { self.start + self.first_end } else { self.to };
        (!self.independent && self.looped && to < self.from).then_some((to, self.from))
    }
    /// Does dropping here change anything?
    fn changes(&self) -> bool {
        if self.independent {
            self.to > self.from
        } else if self.looped {
            self.to != self.from
        } else {
            self.to > self.from
        }
    }
}

/// The loop handle drag in progress, from the pointer: its travel moves the clip's end.
fn loop_drag(app: &StudioApp, ui: &Ui, map: Map) -> Option<LoopDrag> {
    let Some(Gesture::Clip { clip, kind: DragKind::Loop, press, end, .. }) = app.arrange.gesture.clone() else { return None };
    let (pos, m) = ui.input(|i| (i.pointer.latest_pos(), i.modifiers));
    let s = app.engine.session();
    let c = s.find_clip(clip)?.1;
    let dx = (f64::from(pos?.x - press.x) * map.spp).round() as Samples;
    let to = end.saturating_add(dx).max(0);
    let to = if m.shift { to } else { crate::automation::snap_delta(s, map.spp, &map.grid, 0, to) };
    let l = c.looping.filter(|l| l.is_valid());
    let (cycle, first_end) = l.map_or((c.length, c.length), |l| (l.cycle(), l.end));
    Some(LoopDrag { clip, from: c.end(), to, cycle, first_end, start: c.start, looped: l.is_some(), independent: m.alt })
}

/// Commit a loop drag: one `clip.loop_to`, so one undo step.
fn commit_loop(app: &mut StudioApp, l: &LoopDrag) {
    if !l.changes() {
        return;
    }
    let _ = app.run("clip.loop_to", json!({"clip": l.clip.0, "to": l.to, "independent": l.independent}));
}

/// Turn a finished drag into commands.
#[allow(clippy::too_many_arguments)]
fn commit_drag(
    app: &mut StudioApp,
    kind: DragKind,
    clip: ClipId,
    from: TrackId,
    dest: TrackId,
    start: Samples,
    end: Samples,
    a: Samples,
    b: Samples,
    copy: bool,
) {
    match kind {
        DragKind::Move => {
            let by = a - start;
            if by == 0 && dest == from {
                if copy {
                    app.status = "Ctrl-drag (Alt-drag) the clip somewhere else to copy it.".into();
                }
                return;
            }
            if copy {
                let _ = app.copy_clip_to(clip, a, dest);
                return;
            }
            let sel: Vec<ClipId> = app.selected_clips().into_iter().map(|(_, c, _)| c).collect();
            let set = if sel.contains(&clip) && sel.len() > 1 { sel } else { vec![clip] };
            // Several clips move in time only; one clip may change track.
            let to = (set.len() == 1 && dest != from).then_some(dest);
            let _ = app.move_clips(&set, by, to);
        }
        DragKind::TrimStart if a != start => {
            let _ = app.trim_clip(clip, true, a);
        }
        DragKind::TrimEnd if b != end => {
            let _ = app.trim_clip(clip, false, b);
        }
        _ => {}
    }
}

fn ruler_input(app: &mut StudioApp, ui: &mut Ui, map: Map, ruler: Rect) {
    let resp = ui.interact(ruler, ui.id().with("ruler"), Sense::click_and_drag());
    if (resp.clicked() || resp.dragged())
        && let Some(pos) = resp.interact_pointer_pos()
    {
        app.click_ruler(map.at(pos.x.clamp(ruler.min.x, ruler.max.x)));
    }
}

/// Cycle range, recording takes, cursor and playhead.
#[allow(clippy::too_many_arguments)]
fn overlays(app: &StudioApp, ui: &Ui, s: &Session, map: Map, ruler: Rect, tl: Rect, rows: &[&Track], geo: &RowGeo, layout: &mut Layout) {
    let p = ui.painter();
    let sel = s.edit.cycle_range();
    if s.edit.loop_playback && !sel.is_empty() {
        let r = Rect::from_min_max(pos2(map.x(sel.start), ruler.min.y + crate::loopbar::STRIP_H + 14.0), pos2(map.x(sel.end), ruler.max.y - 1.0))
            .intersect(ruler);
        p.rect_filled(r, crate::theme::RADIUS_SM, TOKENS.loop_region.gamma_multiply(0.55));
    }
    // The take being recorded grows on each armed track where it will land (slice 6: inside the
    // punch range, after a count-in), with a "Recording" name strip.
    let span = app.engine.transport.recording.then(|| {
        let pass = app.take_plan.as_ref().and_then(|p| p.pass);
        soundcraft_engine::io::live_span(app.record_start, pass, app.punch, (app.position() - app.record_start).max(0))
    });
    if let Some(Some((_, span))) = span {
        let rp = p.with_clip_rect(tl);
        for (i, t) in rows.iter().enumerate() {
            if t.mixer.record_arm && !app.midi_live.as_ref().is_some_and(|live| live.tracks.contains(&t.id)) {
                let y0 = tl.min.y + geo.tops[i] - app.arrange.vscroll;
                let r = Rect::from_min_max(pos2(map.x(span.start), y0 + 1.0), pos2(map.x(span.end).max(map.x(span.start) + 2.0), y0 + geo.rh - 1.0));
                rp.rect_filled(r, crate::theme::RADIUS, TOKENS.record.gamma_multiply(0.45));
                let strip = Rect::from_min_max(r.min, pos2(r.max.x, (r.min.y + CLIP_NAME_H).min(r.max.y)));
                rp.rect_filled(strip, crate::theme::top_corners(), TOKENS.record);
                let tp = rp.with_clip_rect(strip.intersect(tl));
                tp.text(
                    pos2(strip.min.x + 4.0, strip.center().y),
                    Align2::LEFT_CENTER,
                    "Recording",
                    FontId::proportional(11.0),
                    theme::text_on(TOKENS.record),
                );
            }
        }
    }
    midi_recording(app, ui, s, map, tl, rows, geo, layout);
    let full = Rect::from_min_max(ruler.min, tl.max);
    let lp = p.with_clip_rect(full);
    let cx = map.x(app.cursor());
    lp.line_segment([pos2(cx, ruler.min.y), pos2(cx, tl.max.y)], Stroke::new(1.0, TOKENS.cursor.gamma_multiply(0.8)));
    let tri = vec![pos2(cx - 5.0, ruler.max.y - 8.0), pos2(cx + 5.0, ruler.max.y - 8.0), pos2(cx, ruler.max.y - 1.0)];
    lp.add(egui::Shape::convex_polygon(tri, TOKENS.cursor, Stroke::NONE));
    if app.is_playing() {
        let px = map.x(app.engine.transport.position);
        lp.line_segment([pos2(px, ruler.min.y), pos2(px, tl.max.y)], Stroke::new(1.5, TOKENS.accent));
    }
}

/// The MIDI take being recorded grows on each armed instrument lane from the latch to the playhead,
/// with the notes played so far drawn inside it.
#[allow(clippy::too_many_arguments)]
fn midi_recording(app: &StudioApp, ui: &Ui, s: &Session, map: Map, tl: Rect, rows: &[&Track], geo: &RowGeo, layout: &mut Layout) {
    let Some(live) = &app.midi_live else { return };
    let rp = ui.painter().with_clip_rect(tl);
    for (i, t) in rows.iter().enumerate() {
        if !live.tracks.contains(&t.id) {
            continue;
        }
        let y0 = tl.min.y + geo.tops[i] - app.arrange.vscroll;
        let (x0, x1) = (map.x(live.start), map.x(live.end).max(map.x(live.start) + 2.0));
        let r = Rect::from_min_max(pos2(x0, y0 + 1.0), pos2(x1, y0 + geo.rh - 1.0));
        rp.rect_filled(r, crate::theme::RADIUS, TOKENS.record.gamma_multiply(0.45));
        let strip = Rect::from_min_max(r.min, pos2(r.max.x, (r.min.y + CLIP_NAME_H).min(r.max.y)));
        rp.rect_filled(strip, crate::theme::top_corners(), TOKENS.record);
        rp.with_clip_rect(strip.intersect(tl)).text(
            pos2(strip.min.x + 4.0, strip.center().y),
            Align2::LEFT_CENTER,
            "Recording",
            FontId::proportional(11.0),
            theme::text_on(TOKENS.record),
        );
        // Provisional notes: pitch maps onto the lane under the name strip.
        let mine: Vec<_> = live.built.iter().filter(|b| b.track == t.id).collect();
        let (lo, hi) = mine.iter().flat_map(|b| b.sequence.notes.iter().map(|n| n.pitch)).fold((127u8, 0u8), |(lo, hi), p| (lo.min(p), hi.max(p)));
        let body = Rect::from_min_max(pos2(r.min.x, strip.max.y + 2.0), pos2(r.max.x, r.max.y - 2.0));
        let span = f32::from(hi.saturating_sub(lo).max(11)) + 1.0;
        let mut notes = Vec::new();
        for b in mine {
            let base = s.tempo.samples_to_ticks(b.range.start, s.sample_rate);
            for n in &b.sequence.notes {
                let (a, e) = (s.tempo.tick_to_samples(base + n.start, s.sample_rate), s.tempo.tick_to_samples(base + n.end(), s.sample_rate));
                let h = (body.height() / span).clamp(1.5, 6.0);
                let y = body.max.y - (f32::from(n.pitch.saturating_sub(lo)) + 1.0) * body.height() / span;
                let nr = Rect::from_min_max(pos2(map.x(a), y), pos2(map.x(e).max(map.x(a) + 1.5), y + h)).intersect(body);
                if nr.is_positive() {
                    rp.rect_filled(nr, crate::theme::RADIUS_SM, theme::text_on(TOKENS.record));
                }
                notes.push(r4(nr));
            }
        }
        layout.recording.push(RecordingLane { track: t.id.0, rect: r4(r), notes });
    }
}

/// Fraction of the visible width one wheel notch pans.
const PAN_PER_NOTCH: f64 = 0.10;

/// A horizontal wheel delta in points as a fraction of the visible width (positive: content moves
/// right, so the view moves earlier). A notch is `line_scroll_speed` points, so the pan does not
/// depend on zoom or on `pixels_per_point`. egui's wheel state already folds Shift+vertical into
/// `x` (`egui::input_state::wheel_state::on_wheel_event`: `delta = vec2(delta.x + delta.y, 0.0)`),
/// so callers must not test Shift and read `y` themselves.
pub(crate) fn wheel_pan(ui: &Ui, dx: f32) -> f64 {
    let notch = ui.ctx().options(|o| o.input_options.line_scroll_speed).max(1.0);
    f64::from(dx / notch) * PAN_PER_NOTCH
}

/// Wheel scrolls the rows, Shift+wheel or a sideways swipe scrolls time, Ctrl+wheel or a pinch zooms.
fn scroll_and_zoom(app: &mut StudioApp, ui: &Ui, map: Map, full: Rect, tl: Rect, content_h: f32) {
    let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else { return };
    if !full.contains(pos) {
        return;
    }
    let (delta, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
    if (zoom - 1.0).abs() > 1e-4 {
        let _ = app.engine.execute("view.zoom_by", &json!({"factor": zoom, "x": (pos.x - tl.min.x).max(0.0)}));
        return;
    }
    // egui has already turned Shift+vertical wheel into a horizontal delta (see `wheel_pan`), so
    // time pans on `delta.x` alone, whether it came from Shift, a tilt wheel or a swipe.
    if delta.x.abs() > 0.1 {
        let by_px = -wheel_pan(ui, delta.x) * f64::from(tl.width());
        let _ = app.engine.execute("view.scroll", &json!({"by_px": by_px}));
    }
    if delta.y.abs() > 0.1 {
        app.arrange.vscroll = (app.arrange.vscroll - delta.y).clamp(0.0, (content_h - tl.height()).max(0.0));
    }
    let _ = map;
}

/// While playing, page the view so the playhead stays on screen.
fn follow_playhead(app: &mut StudioApp, map: Map, tl: Rect) {
    if !app.is_playing() || app.arrange.gesture.is_some() {
        return;
    }
    let x = map.x(app.engine.transport.position);
    if x > tl.max.x - 8.0 || x < tl.min.x {
        let to = app.engine.transport.position - (f64::from(tl.width()) * map.spp * 0.05) as Samples;
        let _ = app.engine.execute("view.scroll", &json!({"to": to.max(0)}));
    }
}

/// Slice 6: a chip on each clip that other takes overlap ("2 takes", click to show or hide the
/// track's take lanes), and the take lanes of tracks whose takes are shown: one lane per playlist
/// with its clips as bars (click: that take plays), and in the track list its label with Promote
/// (keep only this take) and Delete. Drawn after the timeline's input, so these take the clicks.
#[allow(clippy::too_many_arguments)]
fn takes(app: &mut StudioApp, ui: &mut Ui, s: &Session, map: Map, tl: Rect, list: Rect, rows: &[&Track], geo: &RowGeo, layout: &Layout) {
    let _ = s;
    for (i, t) in rows.iter().enumerate() {
        if t.playlists.len() < 2 {
            continue;
        }
        let y0 = tl.min.y + geo.tops[i] - app.arrange.vscroll;
        let open = app.ui.takes_open.contains(&t.id);
        for c in t.clips() {
            let others = t
                .playlists
                .iter()
                .enumerate()
                .filter(|(k, pl)| *k != t.active_playlist && pl.clips.iter().any(|o| o.start < c.end() && o.end() > c.start))
                .count();
            let Some(cr) = layout.clips.iter().find(|r| r.id == c.id.0).map(|r| r.rect) else { continue };
            let wide = cr[2] - cr[0] >= 64.0 + 8.0;
            if others == 0 {
                continue;
            }
            // Wide clips get the "2 takes" chip at their right end; narrower ones a 10 px glyph at
            // their left edge, so the lanes stay reachable at any zoom.
            let chip = if wide {
                Rect::from_min_max(pos2(cr[2] - 64.0 - 2.0, cr[1] + 1.5), pos2(cr[2] - 2.0, cr[1] + CLIP_NAME_H - 0.5))
            } else {
                Rect::from_min_max(pos2(cr[0].max(tl.min.x) + 1.0, cr[1] + 1.5), pos2(cr[0].max(tl.min.x) + 11.0, cr[1] + CLIP_NAME_H - 0.5))
            };
            if !tl.intersects(chip) {
                continue;
            }
            let resp = ui.interact(chip.intersect(tl), ui.id().with(("take_chip", c.id.0)), Sense::click());
            let p = ui.painter().with_clip_rect(tl);
            p.rect_filled(chip, crate::theme::RADIUS_SM, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
            let icon = if open { crate::icons::Icon::ChevronDown } else { crate::icons::Icon::ChevronRight };
            if wide {
                crate::icons::paint(&p, pos2(chip.min.x + 8.0, chip.center().y), icon, 12.0, TOKENS.text);
                p.text(
                    pos2(chip.min.x + 16.0, chip.center().y),
                    Align2::LEFT_CENTER,
                    format!("{} takes", others + 1),
                    FontId::proportional(10.5),
                    TOKENS.text,
                );
            } else {
                crate::icons::paint(&p, chip.center(), icon, 10.0, TOKENS.text);
            }
            if resp.on_hover_text(if open { "Hide the takes" } else { "Show the takes" }).clicked() {
                app.toggle_takes(t.id);
            }
            app.finish.layout.chips.push(crate::finish::TakeChip { track: t.id.0, clip: c.id.0, takes: others + 1, open, rect: r4(chip) });
        }
        if !open {
            continue;
        }
        let n = t.playlists.len();
        for (k, pl) in t.playlists.iter().enumerate() {
            let ly = y0 + geo.rh + k as f32 * TAKE_H;
            let lane = Rect::from_min_max(pos2(tl.min.x, ly), pos2(tl.max.x, ly + TAKE_H));
            let active = k == t.active_playlist;
            let p = ui.painter().with_clip_rect(tl);
            p.rect_filled(lane.shrink2(vec2(0.0, 1.0)), 0.0, if active { TOKENS.accent.gamma_multiply(0.2) } else { TOKENS.bg });
            for c in &pl.clips {
                let r = Rect::from_min_max(pos2(map.x(c.start), ly + 4.0), pos2(map.x(c.end()).max(map.x(c.start) + 2.0), ly + TAKE_H - 4.0));
                let col = theme::rgb(c.color.unwrap_or(t.color));
                p.rect_filled(r, crate::theme::RADIUS_SM, if active { col } else { col.gamma_multiply(0.5) });
            }
            let lresp = ui.interact(lane.intersect(tl), ui.id().with(("take_lane", t.id.0, k)), Sense::click());
            // In the track list: the take's label, Promote and Delete.
            let lp = ui.painter().with_clip_rect(list);
            let hl = Rect::from_min_max(pos2(list.min.x + 20.0, ly + 1.0), pos2(list.max.x - 6.0, ly + TAKE_H - 1.0));
            let del = Rect::from_min_size(pos2(hl.max.x - 20.0, hl.min.y + 1.0), vec2(20.0, hl.height() - 2.0));
            let pro = Rect::from_min_size(pos2(del.min.x - 70.0, hl.min.y + 1.0), vec2(66.0, hl.height() - 2.0));
            let label = Rect::from_min_max(hl.min, pos2(pro.min.x - 4.0, hl.max.y));
            let hresp = ui.interact(label.intersect(list), ui.id().with(("take_label", t.id.0, k)), Sense::click());
            if active {
                lp.rect_filled(label, crate::theme::RADIUS_SM, TOKENS.accent.gamma_multiply(0.2));
            }
            let text = if active { format!("Take {} · playing", k + 1) } else { format!("Take {}", k + 1) };
            lp.text(
                pos2(label.min.x + 6.0, label.center().y),
                Align2::LEFT_CENTER,
                text,
                FontId::proportional(11.0),
                if active { TOKENS.text } else { TOKENS.text_dim },
            );
            let presp = ui.interact(pro.intersect(list), ui.id().with(("take_promote", t.id.0, k)), Sense::click());
            lp.rect_filled(pro, crate::theme::RADIUS_SM, if presp.hovered() { TOKENS.button_hover } else { TOKENS.button });
            lp.text(pro.center(), Align2::CENTER_CENTER, "Promote", FontId::proportional(10.5), TOKENS.text);
            let can_delete = n == 2;
            let dresp = ui.interact(del.intersect(list), ui.id().with(("take_delete", t.id.0, k)), Sense::click());
            lp.rect_filled(del, crate::theme::RADIUS_SM, if dresp.hovered() && can_delete { TOKENS.button_hover } else { TOKENS.button });
            let ink = if can_delete { TOKENS.text } else { TOKENS.text_dim.gamma_multiply(0.5) };
            crate::icons::paint(&lp, del.center(), crate::icons::Icon::Remove, 12.0, ink);
            app.finish.layout.takes.push(crate::finish::TakeRow {
                track: t.id.0,
                index: k,
                active,
                clips: pl.clips.len(),
                lane: r4(lane),
                label: r4(label),
                promote: r4(pro),
                delete: r4(del),
                delete_enabled: can_delete,
            });
            if (lresp.clicked() || hresp.clicked()) && !active {
                let _ = app.select_take(t.id, k);
            }
            if presp.on_hover_text("Keep this take and remove the others").clicked() {
                let _ = app.promote_take(t.id, k);
            }
            let tip =
                if can_delete { "Delete this take" } else { "Deleting one of three or more takes needs an engine command; promote a take instead" };
            if dresp.on_hover_text(tip).clicked() && can_delete {
                app.dialog = Some(crate::Dialog::DeleteTake { track: t.id, index: k });
            }
        }
    }
}
