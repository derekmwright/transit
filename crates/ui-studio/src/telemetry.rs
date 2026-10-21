//! The output-callback telemetry readout in the transport bar, and its details window.
//!
//! Fed by `Player::callback_telemetry()` (a handful of relaxed atomic loads), read at most
//! [`POLL_SECS`] apart, never per frame. The readout is absent without a player (`--no-audio`).
//! The snapshot is approximate: fields are loaded independently, so nothing here derives an exact
//! average or rate from it, and every cross-field subtraction saturates. The teardown-only
//! counters (`contention_silence_frames`, `unavailable_silence_frames`) are never shown.

use crate::StudioApp;
use crate::icons::{self, Icon};
use crate::theme::{RADIUS, SPACE, TOKENS};
use egui::{Align2, Rect, Sense, Ui, vec2};
use soundcraft_playback::telemetry::{CallbackTelemetry, DURATION_LIMITS_NS};

/// Minimum time between two reads of the counters (about four a second).
pub const POLL_SECS: f64 = 0.25;

/// What the two numbers are, and why "nominal" matters (the playback module's wording).
pub const EXPLAIN: &str = "Peak callback load (since open): the longest callback's work time as a share of its nominal budget, \
startup included.\n\
Over-budget callbacks (nominal): callbacks whose work time exceeded that budget.\n\
Nominal: the budget is frames / device rate, not the time left before the hardware deadline. A callback can miss that \
deadline without exceeding the nominal budget, so zero does not prove glitch-free audio.";

/// Where the readout and its window were last drawn, for `ui.inspect` and tests.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TelemetryLayout {
    /// The readout is drawn (there is a player, or a test snapshot).
    pub shown: bool,
    pub text: String,
    pub rect: crate::arrange::R4,
    /// The warning icon is drawn (backend or device-loss errors), with its count.
    pub warning: Option<u64>,
    pub open: bool,
    pub window: crate::arrange::R4,
    /// The window's lines: histogram rows, then raw counts, as `label: value`.
    pub rows: Vec<String>,
}

#[derive(Default)]
pub struct TelemetryState {
    pub open: bool,
    snap: CallbackTelemetry,
    last_read: Option<f64>,
    /// How many times the counters were read (tests check it follows the poll interval).
    pub reads: u64,
    /// A snapshot standing in for a player, so tests can drive the readout without audio.
    pub injected: Option<CallbackTelemetry>,
    pub layout: TelemetryLayout,
}

impl TelemetryState {
    /// Whether a read is due at `now` (seconds); records it when so.
    fn due(&mut self, now: f64) -> bool {
        if self.last_read.is_some_and(|t| now >= t && now - t < POLL_SECS) {
            return false;
        }
        self.last_read = Some(now);
        self.reads += 1;
        true
    }
}

/// `12.3%` from millionths (1_000_000 = 100%), in integers: no rate is derived from other fields.
pub fn load_text(ppm: u64) -> String {
    let tenths = ppm / 1000;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// The compact readout: peak load and over-budget count.
pub fn readout_text(t: &CallbackTelemetry) -> String {
    format!("Load {}  Over {}", load_text(t.max_load_ppm), t.over_budget_callbacks)
}

/// The error count behind the warning icon: CPAL error callbacks (device loss is a subset of them,
/// so the larger of the two counters is the count), if any.
pub fn error_count(t: &CallbackTelemetry) -> Option<u64> {
    let n = t.backend_errors.max(t.device_lost_errors);
    (n > 0).then_some(n)
}

/// Histogram bin labels, from [`DURATION_LIMITS_NS`] and the open last bin.
pub fn bin_label(i: usize) -> String {
    let unit = |ns: u64| {
        if ns >= 1_000_000 { format!("{} ms", ns as f64 / 1_000_000.0) } else { format!("{} \u{b5}s", ns as f64 / 1_000.0) }
    };
    match (DURATION_LIMITS_NS.get(i), i.checked_sub(1).and_then(|p| DURATION_LIMITS_NS.get(p))) {
        (Some(hi), _) => format!("up to {}", unit(*hi)),
        (None, Some(lo)) => format!("over {}", unit(*lo)),
        (None, None) => String::new(),
    }
}

/// The details window's lines (`label: value`): the histogram, then the raw counts. Nothing is
/// derived but a saturating "without a timing" and a saturating histogram total.
pub fn detail_rows(t: &CallbackTelemetry) -> Vec<String> {
    let mut rows: Vec<String> = t.duration_histogram.iter().enumerate().map(|(i, n)| format!("{}: {n}", bin_label(i))).collect();
    rows.push(format!("Histogram total: {}", t.duration_histogram.iter().fold(0u64, |a, n| a.saturating_add(*n))));
    rows.extend([
        format!("Peak callback load (since open): {}", load_text(t.max_load_ppm)),
        format!("Over-budget callbacks (nominal): {}", t.over_budget_callbacks),
        format!("Over-budget callbacks while a plug-in was loading: {}", t.over_budget_while_creating),
        format!("Callbacks: {}", t.callbacks),
        format!("Timed callbacks: {}", t.timed_callbacks),
        format!("Callbacks without a timing: {}", t.callbacks.saturating_sub(t.timed_callbacks)),
        format!("Frames: {}", t.frames),
        format!("Total work time (ns): {}", t.total_duration_ns),
        format!("Longest callback (ns): {}", t.max_duration_ns),
        format!("Backend errors: {}", t.backend_errors),
        format!("Device-lost errors: {}", t.device_lost_errors),
        format!("Frames silenced by a render fault: {}", t.fault_silence_frames),
    ]);
    rows
}

/// Read the counters when due, and draw the readout (a button that opens the window).
pub fn readout(app: &mut StudioApp, ui: &mut Ui) {
    let now = ui.input(|i| i.time);
    if app.player.is_none() && app.telemetry.injected.is_none() {
        app.telemetry.layout.shown = false;
        app.telemetry.layout.warning = None;
        app.telemetry.open = false;
        return;
    }
    if app.telemetry.due(now) {
        app.telemetry.snap = match (&app.player, app.telemetry.injected) {
            (Some(p), _) => p.callback_telemetry(),
            (None, Some(s)) => s,
            (None, None) => CallbackTelemetry::default(),
        };
    }
    // Keep the poll alive on an idle UI.
    ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(POLL_SECS));
    let snap = app.telemetry.snap;
    let text = readout_text(&snap);
    let errors = error_count(&snap);
    ui.add_space(SPACE);
    let text_w = 132.0;
    let (r, resp) = ui.allocate_exact_size(vec2(text_w + errors.map_or(0.0, |_| 44.0), 26.0), Sense::click());
    let p = ui.painter();
    p.rect_filled(r, RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.bg });
    let ink = if resp.hovered() { TOKENS.text } else { TOKENS.text_dim };
    let text_rect = Rect::from_min_size(r.min, vec2(text_w, r.height()));
    p.text(text_rect.center(), Align2::CENTER_CENTER, &text, crate::theme::mono(ui.ctx(), 11.5), ink);
    if let Some(n) = errors {
        let c = egui::pos2(r.min.x + text_w + 10.0, r.center().y);
        icons::paint(p, c, Icon::Warning, 14.0, TOKENS.record);
        p.text(egui::pos2(c.x + 12.0, c.y), Align2::LEFT_CENTER, n.to_string(), crate::theme::mono(ui.ctx(), 11.5), TOKENS.text);
    }
    if resp.on_hover_text(format!("{EXPLAIN}\nClick for details.")).clicked() {
        app.telemetry.open = !app.telemetry.open;
    }
    app.telemetry.layout.shown = true;
    app.telemetry.layout.text = text;
    app.telemetry.layout.rect = crate::arrange::r4(r);
    app.telemetry.layout.warning = errors;
}

/// The details window: the duration histogram as a text table and the raw counts.
pub fn window(app: &mut StudioApp, ctx: &egui::Context) {
    app.telemetry.layout.window = [0.0; 4];
    app.telemetry.layout.rows.clear();
    if !app.telemetry.open {
        app.telemetry.layout.open = false;
        return;
    }
    let snap = app.telemetry.snap;
    let rows = detail_rows(&snap);
    let mut open = true;
    let r = egui::Window::new("Callback Telemetry")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_pos(egui::pos2(ctx.content_rect().right() - 560.0, crate::theme::TRANSPORT_H + 40.0))
        .show(ctx, |ui| {
            ui.set_width(520.0);
            // Side by side, so the window stays short enough to sit under the transport bar.
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Callback work time (callbacks per bin)").strong().color(TOKENS.text));
                    egui::Grid::new("telemetry_hist").num_columns(2).spacing([2.0 * SPACE, SPACE / 2.0]).show(ui, |ui| {
                        for (i, n) in snap.duration_histogram.iter().enumerate() {
                            ui.label(egui::RichText::new(bin_label(i)).color(TOKENS.text_dim));
                            ui.label(egui::RichText::new(n.to_string()).font(crate::theme::mono(ui.ctx(), 12.0)).color(TOKENS.text));
                            ui.end_row();
                        }
                    });
                });
                ui.add_space(2.0 * SPACE);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Counts").strong().color(TOKENS.text));
                    egui::Grid::new("telemetry_counts").num_columns(2).spacing([2.0 * SPACE, SPACE / 2.0]).show(ui, |ui| {
                        for row in rows.iter().skip(snap.duration_histogram.len() + 1) {
                            let (k, v) = row.rsplit_once(": ").unwrap_or((row, ""));
                            ui.label(egui::RichText::new(k).color(TOKENS.text_dim));
                            ui.label(egui::RichText::new(v).font(crate::theme::mono(ui.ctx(), 12.0)).color(TOKENS.text));
                            ui.end_row();
                        }
                    });
                });
            });
            ui.add_space(SPACE);
            ui.label(egui::RichText::new(EXPLAIN).color(TOKENS.text_dim).size(11.0));
        });
    if let Some(r) = r {
        app.telemetry.layout.window = crate::arrange::r4(r.response.rect);
    }
    app.telemetry.layout.rows = rows;
    if !open {
        app.telemetry.open = false;
    }
    app.telemetry.layout.open = app.telemetry.open;
}
