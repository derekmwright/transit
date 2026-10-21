//! The transport bar's load readout: the audio thread's current load with a small bar, the
//! callback buffer and rate ("128 @ 48 kHz") and the audio system ("ASIO", "WASAPI").
//!
//! The load is the highest callback work time over about the last second as a share of the
//! callback's nominal budget (see `CallbackTelemetry::current_load_ppm`), not the since-open peak:
//! that stays in the details window, which a click opens. Above 80% the readout turns the warning
//! color; at 100% and over, or for [`OVERLOAD_HOLD_SECS`] after any over-budget callback, the alert
//! color. Counters are read by `telemetry::poll`, a few times a second.

use crate::StudioApp;
use crate::icons::{self, Icon};
use crate::telemetry::{self, error_count};
use crate::theme::{RADIUS, RADIUS_SM, SPACE, TOKENS};
use egui::{Align2, Color32, Rect, Sense, Ui, pos2, vec2};

/// How loaded the audio thread looks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Ok,
    Warning,
    Alert,
}

/// Above 80% of the budget.
pub const WARN_PPM: u64 = 800_000;
/// At or over the budget.
pub const ALERT_PPM: u64 = 1_000_000;

/// The level for a load in millionths, with `overloaded` for a recent over-budget callback.
pub fn level(ppm: u64, overloaded: bool) -> Level {
    if overloaded || ppm >= ALERT_PPM {
        Level::Alert
    } else if ppm > WARN_PPM {
        Level::Warning
    } else {
        Level::Ok
    }
}

/// `42%`: whole percent from millionths (1_000_000 = 100%).
pub fn percent_text(ppm: u64) -> String {
    format!("{}%", ppm / 10_000)
}

/// `128 @ 48 kHz`, `64 @ 44.1 kHz`; `--` for a figure not known yet.
pub fn format_text(frames: u32, rate: u32) -> String {
    let khz = match rate {
        0 => "--".to_string(),
        r if r % 1000 == 0 => (r / 1000).to_string(),
        r => format!("{:.1}", f64::from(r) / 1000.0),
    };
    let frames = if frames == 0 { "--".to_string() } else { frames.to_string() };
    format!("{frames} @ {khz} kHz")
}

/// Shown beside the figures; `device_system` already is the short cpal host name.
pub fn host_text(host: &str) -> &str {
    host.strip_prefix("Windows ").unwrap_or(host)
}

/// What the number means.
pub const TIP: &str = "Audio load: audio thread time per callback / callback budget (buffer frames / sample rate), the highest over \
about the last second.\nAbove 80% it turns amber; at 100% or after an over-budget callback it turns red for a couple of seconds.";

/// Draw the readout (a button that opens the telemetry details window).
pub fn readout(app: &mut StudioApp, ui: &mut Ui) {
    if !telemetry::poll(app, ui.ctx()) {
        return;
    }
    let now = ui.input(|i| i.time);
    let t = &app.telemetry;
    let snap = t.snap;
    let level = level(snap.current_load_ppm, now < t.alert_until);
    let load = percent_text(snap.current_load_ppm);
    let host = host_text(&t.host).to_string();
    let fmt = format_text(t.frames, t.rate);
    let errors = error_count(&snap);
    let text = if host.is_empty() { format!("{load}  {fmt}") } else { format!("{load}  {fmt}  {host}") };
    let tint: Color32 = match level {
        Level::Ok => TOKENS.monitor,
        Level::Warning => TOKENS.mute,
        Level::Alert => TOKENS.record,
    };

    ui.add_space(SPACE);
    let bar_w = 40.0;
    let text_w = 168.0;
    let warn_w = errors.map_or(0.0, |_| 44.0);
    let (r, resp) = ui.allocate_exact_size(vec2(bar_w + text_w + warn_w + SPACE, 26.0), Sense::click());
    let p = ui.painter();
    p.rect_filled(r, RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.bg });
    let track = Rect::from_min_size(pos2(r.min.x + 6.0, r.center().y - 4.0), vec2(bar_w, 8.0));
    p.rect_filled(track, RADIUS_SM, TOKENS.line);
    let k = (snap.current_load_ppm as f32 / ALERT_PPM as f32).clamp(0.0, 1.0);
    p.rect_filled(Rect::from_min_size(track.min, vec2(track.width() * k, track.height())), RADIUS_SM, tint);
    let ink = if level == Level::Ok { if resp.hovered() { TOKENS.text } else { TOKENS.text_dim } } else { tint };
    p.text(pos2(track.max.x + 8.0, r.center().y), Align2::LEFT_CENTER, &text, crate::theme::mono(ui.ctx(), 11.5), ink);
    if let Some(n) = errors {
        let c = pos2(r.max.x - warn_w + 6.0, r.center().y);
        icons::paint(p, c, Icon::Warning, 14.0, TOKENS.record);
        p.text(pos2(c.x + 12.0, c.y), Align2::LEFT_CENTER, n.to_string(), crate::theme::mono(ui.ctx(), 11.5), TOKENS.text);
    }
    if resp.on_hover_text(format!("{TIP}\nClick for details.")).clicked() {
        app.telemetry.open = !app.telemetry.open;
    }
    let l = &mut app.telemetry.layout;
    l.shown = true;
    l.text = text;
    l.rect = crate::arrange::r4(r);
    l.warning = errors;
    l.level = level;
}
