//! Fine control for faders, pan knobs and send levels: marks and snapping, reset to default,
//! the editable readout's text, and the wheel's merged nudges. The widgets are in
//! [`crate::mixer`]; this holds the numbers and the state they share, so both stay testable
//! without a frame.

use soundcraft_model::TrackId;

/// The fader marks, in dB: 0 dB is the line across the track, the others are fainter.
pub const DB_MARKS: [f32; 4] = [0.0, -6.0, -12.0, -24.0];
/// A send level has a mark at 0 dB only.
pub const SEND_MARKS: [f32; 1] = [0.0];
/// While dragging slowly a value snaps to a mark this close (dB).
pub const SNAP_DB: f32 = 0.2;
/// The pan knob snaps to center within this (2 % of the travel).
pub const SNAP_PAN: f32 = 0.02;
/// A drag is slow when the pointer moves less than this many points in a frame.
pub const SLOW_PT: f32 = 2.0;
/// Shift-drag moves this fraction as far (10x slower).
pub const FINE: f32 = 0.1;
/// A wheel notch moves a level this many dB, and a pan this much of its travel; Shift is 10x finer.
pub const WHEEL_DB: f32 = 0.1;
pub const WHEEL_DB_FINE: f32 = 0.01;
pub const WHEEL_PAN: f32 = 0.01;
pub const WHEEL_PAN_FINE: f32 = 0.001;
/// Wheel nudges merge into one undo step until this many seconds pass without one.
pub const WHEEL_MERGE_S: f64 = 0.5;

/// The lowest level the engine takes (`mix.volume`, `mix.send_level`): silence.
pub const DB_MIN: f32 = -144.0;
/// The highest.
pub const DB_MAX: f32 = 12.0;

/// What a control changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Fader(TrackId),
    Pan(TrackId),
    Send(TrackId, usize),
    /// A send's pan (slice 7; the mix pans only a mono source's send).
    SendPan(TrackId, usize),
}

impl Target {
    /// A pan control (-1..1), as opposed to a level in dB.
    pub fn is_pan(self) -> bool {
        matches!(self, Target::Pan(_) | Target::SendPan(..))
    }

    /// Identifies the control (for widget ids).
    pub fn salt(self) -> (u8, u64, usize) {
        match self {
            Target::Fader(t) => (0, t.0, 0),
            Target::Pan(t) => (1, t.0, 0),
            Target::Send(t, s) => (2, t.0, s),
            Target::SendPan(t, s) => (3, t.0, s),
        }
    }

    /// The merge key of a wheel gesture on this control (its own step, apart from a drag's).
    pub fn wheel_key(self) -> String {
        match self {
            Target::Fader(t) => format!("wheel:fader:{}", t.0),
            Target::Pan(t) => format!("wheel:pan:{}", t.0),
            Target::Send(t, s) => format!("wheel:send:{}:{s}", t.0),
            Target::SendPan(t, s) => format!("wheel:sendpan:{}:{s}", t.0),
        }
    }
}

/// A readout being edited in place.
#[derive(Debug, Clone)]
pub struct ReadoutEdit {
    pub target: Target,
    /// Which of the control's readouts (a strip's, the device chain's).
    pub salt: u64,
    pub text: String,
    /// Not drawn yet: the field takes focus and selects its text on its first frame.
    pub fresh: bool,
}

/// A wheel gesture in progress: nudges to `target` merge until the pointer leaves `rect` or
/// [`WHEEL_MERGE_S`] pass since `last` (seconds of UI time).
#[derive(Debug, Clone, Copy)]
pub struct WheelMerge {
    pub target: Target,
    pub rect: egui::Rect,
    pub last: f64,
}

/// `db` snapped to the nearest of `marks` within [`SNAP_DB`], else unchanged.
pub fn snap_db(db: f32, marks: &[f32]) -> f32 {
    marks.iter().copied().filter(|m| (db - m).abs() <= SNAP_DB).min_by(|a, b| (db - a).abs().total_cmp(&(db - b).abs())).unwrap_or(db)
}

/// A balance (-1..1) snapped to center within [`SNAP_PAN`].
pub fn snap_pan(b: f32) -> f32 {
    if b.abs() <= SNAP_PAN { 0.0 } else { b }
}

/// Parses a level typed into a readout: "-6", "-6.0", "-6 dB", "+1.5", "-inf". Clamped to the
/// engine's range. `None` when it is not a number.
pub fn parse_db(text: &str) -> Option<f32> {
    // Windows IMEs and pasted text produce U+2212 for the minus sign.
    let lower = text.trim().replace('\u{2212}', "-").to_ascii_lowercase();
    let t = lower.strip_suffix("db").unwrap_or(&lower).trim();
    if t == "-inf" || t == "-infinity" {
        return Some(DB_MIN);
    }
    let v: f32 = t.parse().ok()?;
    v.is_finite().then(|| v.clamp(DB_MIN, DB_MAX))
}

/// Parses a pan typed into a readout: "L20", "R35", "C", "-20", "20", "+20" (positive is right),
/// with an optional "%". Returns the balance, -1..1, clamped. `None` when it is not a pan.
pub fn parse_pan(text: &str) -> Option<f32> {
    let t = text.trim().trim_end_matches('%').trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("c") || t.eq_ignore_ascii_case("center") {
        return Some(0.0);
    }
    let (sign, num) = match t.chars().next()? {
        'L' | 'l' => (-1.0, &t[1..]),
        'R' | 'r' => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    let v: f32 = num.trim().parse().ok().filter(|v: &f32| v.is_finite())?;
    Some((sign * v / 100.0).clamp(-1.0, 1.0))
}

/// How a balance reads: "C", "L20", "R35".
pub fn pan_text(b: f32) -> String {
    if b.abs() < 0.005 { "C".to_string() } else { format!("{}{:.0}", if b < 0.0 { "L" } else { "R" }, b.abs() * 100.0) }
}

/// How a fader's level reads: "+0.0", "-6.0", "-inf".
pub fn db_text(db: f32) -> String {
    if db <= DB_MIN { "-inf".to_string() } else { format!("{db:+.1}") }
}

/// What a readout starts as when its edit opens: the value with no sign on positives.
pub fn edit_text_db(db: f32) -> String {
    if db <= DB_MIN { "-inf".to_string() } else { format!("{db:.1}") }
}

/// A wheel nudge of `value` by `notches` (+1 up, -1 down) of `step`, rounded to a thousandth so
/// repeated nudges don't drift.
pub fn nudge(value: f32, notches: f32, step: f32) -> f32 {
    ((value + notches * step) * 1000.0).round() / 1000.0
}
