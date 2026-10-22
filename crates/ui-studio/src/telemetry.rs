//! The output-callback telemetry readout in the transport bar, and its details window.
//!
//! Fed by `Player::callback_telemetry()` (a handful of relaxed atomic loads), read at most
//! [`POLL_SECS`] apart, never per frame. The readout is absent without a player (`--no-audio`).
//! The snapshot is approximate: fields are loaded independently, so nothing here derives an exact
//! average or rate from it, and every cross-field subtraction saturates. The teardown-only
//! counters (`contention_silence_frames`, `unavailable_silence_frames`) are never shown.

use crate::StudioApp;

use crate::theme::{SPACE, TOKENS};
use soundcraft_playback::telemetry::{CallbackTelemetry, DURATION_LIMITS_NS, StreamStats};

/// Minimum time between two reads of the counters (about four a second).
pub const POLL_SECS: f64 = 0.25;

/// How long the readout keeps its overload color after an over-budget callback.
pub const OVERLOAD_HOLD_SECS: f64 = 2.0;

/// What the two numbers are, and why "nominal" matters (the playback module's wording).
pub const EXPLAIN: &str = "Load now: audio thread time per callback divided by the callback budget, the highest over about the last second.
\nPeak callback load (since open): the longest callback's work time as a share of its nominal budget, \
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
    /// How loaded the readout looks (its color): ok, warning or alert.
    pub level: crate::load_readout::Level,
    pub open: bool,
    pub window: crate::arrange::R4,
    /// The window's lines: histogram rows, then raw counts, as `label: value`.
    pub rows: Vec<String>,
    /// The details window's "Reset peak" button.
    pub reset_peak: crate::arrange::R4,
}

#[derive(Default)]
pub struct TelemetryState {
    pub open: bool,
    pub snap: CallbackTelemetry,
    last_read: Option<f64>,
    /// How many times the counters were read (tests check it follows the poll interval).
    pub reads: u64,
    /// A snapshot standing in for a player, so tests can drive the readout without audio.
    pub injected: Option<CallbackTelemetry>,
    /// Host name, callback frames and rate behind the readout, from the latest read.
    pub host: String,
    pub frames: u32,
    pub rate: u32,
    /// Time (egui seconds) until which the readout keeps flashing its overload color.
    pub alert_until: f64,
    /// Injected host name for tests, next to `injected`.
    pub injected_host: Option<String>,
    /// Peak resets requested (the details window button).
    pub peak_resets: u64,
    /// Audio takes recorded without a validated device clock (aligned approximately) this run.
    pub approximate_takes: u64,
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
        format!("Load now (about the last second): {}", load_text(t.current_load_ppm)),
        format!("Over-budget callbacks in that window: {}", t.recent_over_budget),
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

/// The Audio Devices line for the device loop of a native stream (see [`StreamStats::line`]).
/// `wakes_per_second` is the rate over the last panel samples; `None` until there are two.
pub fn backend_line(s: &StreamStats, capture: bool, wakes_per_second: Option<u64>) -> String {
    let wakes = wakes_per_second.map_or_else(|| "-".to_owned(), |rate| format!("{rate}/s"));
    format!("Backend: {}", s.line_with_wakes(capture, &wakes))
}

/// The host shown in the load readout: the audio system, starred when the native backend serves it
/// (`WASAPI*`; see `docs/contributing/knowledge/clocks.md`), with the mode it opened in
/// (`WASAPI* exclusive`, `WASAPI* shared`).
pub(crate) fn host_name(player: &soundcraft_playback::Player) -> String {
    if player.native_backend {
        format!("{}* {}", player.device_system, if player.device_exclusive { "exclusive" } else { "shared" })
    } else {
        player.device_system.clone()
    }
}

/// Read the counters when due (about four times a second) and keep the overload hold running.
/// Returns false, with the readout and window closed, when there is no player and no test snapshot.
pub fn poll(app: &mut StudioApp, ctx: &egui::Context) -> bool {
    let now = ctx.input(|i| i.time);
    if app.player.is_none() && app.telemetry.injected.is_none() {
        app.telemetry.layout.shown = false;
        app.telemetry.layout.warning = None;
        app.telemetry.open = false;
        return false;
    }
    if app.telemetry.due(now) {
        let (snap, host, rate, buffer) = match (&app.player, app.telemetry.injected) {
            (Some(p), _) => (p.callback_telemetry(), host_name(p), p.device_rate, p.device_buffer.unwrap_or(0)),
            (None, Some(s)) => (s, app.telemetry.injected_host.clone().unwrap_or_default(), s.sample_rate, s.callback_frames),
            (None, None) => Default::default(),
        };
        let t = &mut app.telemetry;
        // A new over-budget callback, whether or not it is still inside the recent window.
        if snap.recent_over_budget > 0 || snap.over_budget_callbacks > t.snap.over_budget_callbacks {
            t.alert_until = now + OVERLOAD_HOLD_SECS;
        }
        t.snap = snap;
        t.host = host;
        // Before the first callback the device's own figures stand in.
        t.rate = if snap.sample_rate > 0 { snap.sample_rate } else { rate };
        t.frames = if snap.callback_frames > 0 { snap.callback_frames } else { buffer };
    }
    // Keep the poll alive on an idle UI.
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(POLL_SECS));
    true
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
    let mut reset_clicked = false;
    let mut reset_rect = [0.0; 4];
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
            ui.horizontal(|ui| {
                let reset = ui.button("Reset peak").on_hover_text("Start the since-open peak load and longest callback over");
                reset_rect = crate::arrange::r4(reset.rect);
                if reset.clicked() {
                    reset_clicked = true;
                }
                ui.label(egui::RichText::new(format!("Now {} (last second)", load_text(snap.current_load_ppm))).color(TOKENS.text_dim));
            });
            ui.label(egui::RichText::new(EXPLAIN).color(TOKENS.text_dim).size(11.0));
        });
    if let Some(r) = r {
        app.telemetry.layout.window = crate::arrange::r4(r.response.rect);
    }
    app.telemetry.layout.rows = rows;
    app.telemetry.layout.reset_peak = reset_rect;
    if reset_clicked {
        app.reset_peak_load();
    }
    if !open {
        app.telemetry.open = false;
    }
    app.telemetry.layout.open = app.telemetry.open;
}

impl StudioApp {
    /// Start the since-open peak over: the audio side zeroes its maxima at its next block, and the
    /// figures shown drop at once.
    pub fn reset_peak_load(&mut self) {
        self.telemetry.peak_resets += 1;
        if let Some(p) = &self.player {
            p.reset_peak_load();
        }
        let clear = |t: &mut CallbackTelemetry| {
            t.max_load_ppm = 0;
            t.max_duration_ns = 0;
        };
        clear(&mut self.telemetry.snap);
        if let Some(s) = &mut self.telemetry.injected {
            clear(s);
        }
    }
}
