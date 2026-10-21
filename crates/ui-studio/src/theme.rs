//! Theme tokens (plan 06 slice 5): a dark and a light set with the same accent hue, picked by
//! View > Appearance (System follows the OS). Flat: one accent, no gradients, no shadows beyond
//! a 1 px separator; record, mute and solo keep their conventional state colors.
//!
//! Every color the studio UI paints comes from [`TOKENS`], which reads the theme of the frame
//! being drawn ([`set_current`], per thread, so tests on parallel threads stay hermetic).

use egui::Color32;
use std::cell::Cell;

/// View > Appearance: follow the OS, or pin dark or light. Kept in `UiState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Dark,
    Light,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Dark, Appearance::Light];

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Dark => "Dark",
            Appearance::Light => "Light",
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            Appearance::System => egui::ThemePreference::System,
            Appearance::Dark => egui::ThemePreference::Dark,
            Appearance::Light => egui::ThemePreference::Light,
        }
    }
}

/// Color tokens. Everything the studio UI paints takes its colors from here.
pub struct Tokens {
    pub bg: Color32,
    pub panel: Color32,
    pub panel_raised: Color32,
    pub header: Color32,
    pub header_selected: Color32,
    pub lane: Color32,
    pub lane_alt: Color32,
    pub ruler: Color32,
    /// The 1 px separator between areas (the only "shadow" there is).
    pub line: Color32,
    pub grid_bar: Color32,
    pub grid_beat: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    /// The one accent: selection, focus, active values. Same hue in both themes (darker in light
    /// so it keeps 3:1 against the light panels).
    pub accent: Color32,
    pub cursor: Color32,
    pub record: Color32,
    pub mute: Color32,
    pub solo: Color32,
    pub monitor: Color32,
    pub button: Color32,
    /// The hover token: fill for hovered buttons and rows.
    pub button_hover: Color32,
    pub selection_outline: Color32,
    /// Keyboard focus ring (and the active pane's outline).
    pub focus_ring: Color32,
    /// Dims what lies outside a range (piano roll past the clip end).
    pub overlay: Color32,
    /// Text on light fills and on dark fills ([`text_on`]).
    pub ink_dark: Color32,
    pub ink_light: Color32,
    /// Light theme: clip bodies are the track color lightened, the name strip the track color.
    pub light_clips: bool,
}

pub const DARK: Tokens = Tokens {
    bg: Color32::from_rgb(0x1b, 0x1c, 0x1e),
    panel: Color32::from_rgb(0x24, 0x25, 0x28),
    panel_raised: Color32::from_rgb(0x2d, 0x2e, 0x32),
    header: Color32::from_rgb(0x2a, 0x2b, 0x2f),
    header_selected: Color32::from_rgb(0x3a, 0x3c, 0x42),
    lane: Color32::from_rgb(0x1f, 0x20, 0x23),
    lane_alt: Color32::from_rgb(0x22, 0x23, 0x26),
    ruler: Color32::from_rgb(0x2b, 0x2c, 0x30),
    line: Color32::from_rgb(0x14, 0x14, 0x16),
    grid_bar: Color32::from_rgb(0x38, 0x39, 0x3e),
    grid_beat: Color32::from_rgb(0x2a, 0x2b, 0x2f),
    text: Color32::from_rgb(0xdc, 0xdd, 0xe0),
    // Lifted from 0x8c8e94 (slice 5) so secondary labels keep 4.5:1 on the raised panels.
    text_dim: Color32::from_rgb(0xb6, 0xb8, 0xbe),
    accent: Color32::from_rgb(0x4c, 0xb1, 0xf0),
    cursor: Color32::from_rgb(0xe8, 0xe8, 0xea),
    record: Color32::from_rgb(0xe5, 0x48, 0x48),
    mute: Color32::from_rgb(0xe0, 0xa2, 0x36),
    solo: Color32::from_rgb(0xe6, 0xd2, 0x4a),
    monitor: Color32::from_rgb(0x6f, 0xc2, 0x76),
    button: Color32::from_rgb(0x36, 0x37, 0x3c),
    button_hover: Color32::from_rgb(0x42, 0x44, 0x4a),
    selection_outline: Color32::from_rgb(0xf4, 0xf4, 0xf6),
    focus_ring: Color32::from_rgb(0x4c, 0xb1, 0xf0),
    overlay: Color32::from_rgba_premultiplied(0, 0, 0, 70),
    ink_dark: Color32::from_rgb(0x14, 0x14, 0x16),
    ink_light: Color32::from_rgb(0xf6, 0xf6, 0xf8),
    light_clips: false,
};

pub const LIGHT: Tokens = Tokens {
    bg: Color32::from_rgb(0xff, 0xff, 0xff),
    panel: Color32::from_rgb(0xee, 0xee, 0xf0),
    panel_raised: Color32::from_rgb(0xf7, 0xf7, 0xf8),
    header: Color32::from_rgb(0xe6, 0xe6, 0xe9),
    header_selected: Color32::from_rgb(0xd5, 0xdb, 0xe6),
    lane: Color32::from_rgb(0xf8, 0xf8, 0xf9),
    lane_alt: Color32::from_rgb(0xf1, 0xf1, 0xf3),
    ruler: Color32::from_rgb(0xe3, 0xe3, 0xe7),
    line: Color32::from_rgb(0xc9, 0xca, 0xcf),
    grid_bar: Color32::from_rgb(0xc6, 0xc7, 0xcc),
    grid_beat: Color32::from_rgb(0xe2, 0xe3, 0xe6),
    text: Color32::from_rgb(0x1c, 0x1d, 0x20),
    text_dim: Color32::from_rgb(0x50, 0x53, 0x5b),
    accent: Color32::from_rgb(0x18, 0x73, 0xb8),
    cursor: Color32::from_rgb(0x1c, 0x1d, 0x20),
    record: Color32::from_rgb(0xc6, 0x2f, 0x2f),
    mute: Color32::from_rgb(0xe0, 0xa2, 0x36),
    solo: Color32::from_rgb(0xe6, 0xd2, 0x4a),
    monitor: Color32::from_rgb(0x2f, 0x9a, 0x3c),
    button: Color32::from_rgb(0xde, 0xdf, 0xe3),
    button_hover: Color32::from_rgb(0xd0, 0xd2, 0xd7),
    selection_outline: Color32::from_rgb(0x1c, 0x1d, 0x20),
    focus_ring: Color32::from_rgb(0x18, 0x73, 0xb8),
    overlay: Color32::from_rgba_premultiplied(0, 0, 0, 28),
    ink_dark: Color32::from_rgb(0x14, 0x14, 0x16),
    ink_light: Color32::from_rgb(0xf6, 0xf6, 0xf8),
    light_clips: true,
};

thread_local! {
    static LIGHT_NOW: Cell<bool> = const { Cell::new(false) };
}

/// The tokens of the theme being drawn: `TOKENS.accent` reads dark or light.
pub struct Current;

impl std::ops::Deref for Current {
    type Target = Tokens;
    fn deref(&self) -> &Tokens {
        if LIGHT_NOW.with(Cell::get) { &LIGHT } else { &DARK }
    }
}

pub static TOKENS: Current = Current;

/// Draw with `theme`'s tokens from now on (this thread).
pub fn set_current(theme: egui::Theme) {
    LIGHT_NOW.with(|c| c.set(theme == egui::Theme::Light));
}

/// The theme the tokens currently read.
pub fn current() -> egui::Theme {
    if LIGHT_NOW.with(Cell::get) { egui::Theme::Light } else { egui::Theme::Dark }
}

/// The spacing grid: margins and gaps are multiples of 8 px (4 px half steps inside dense rows).
pub const SPACE: f32 = 8.0;
/// Corner radius of buttons, cards, clips, menus and windows.
pub const RADIUS: f32 = 4.0;
/// Corner radius of small controls (header toggles, readouts, meters, send rows).
pub const RADIUS_SM: f32 = 2.0;
/// Keyboard focus ring width.
pub const FOCUS_W: f32 = 1.5;

/// Rounded top corners only (a card's title, a clip's name strip).
pub fn top_corners() -> egui::CornerRadius {
    let r = RADIUS as u8;
    egui::CornerRadius { nw: r, ne: r, sw: 0, se: 0 }
}

/// Track list row metrics (Cubase-like density).
pub const RULER_H: f32 = 34.0;
pub const TRANSPORT_H: f32 = 40.0;
pub const STATUS_H: f32 = 22.0;
pub const CLIP_NAME_H: f32 = 14.0;

/// egui's own widgets (menus, buttons, windows) in `t`'s colors.
fn visuals(t: &Tokens, light: bool) -> egui::Visuals {
    let mut v = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.panel_fill = t.panel;
    v.window_fill = t.panel_raised;
    v.extreme_bg_color = t.bg;
    v.faint_bg_color = t.lane_alt;
    v.selection.bg_fill = t.accent.gamma_multiply(if light { 0.35 } else { 0.6 });
    v.selection.stroke = egui::Stroke::new(FOCUS_W, t.focus_ring);
    v.hyperlink_color = t.accent;
    v.window_stroke = egui::Stroke::new(1.0, t.line);
    v.widgets.noninteractive.bg_fill = t.panel;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, t.line);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, t.text);
    v.widgets.inactive.bg_fill = t.button;
    v.widgets.inactive.weak_bg_fill = t.button;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, t.text);
    v.widgets.hovered.bg_fill = t.button_hover;
    v.widgets.hovered.weak_bg_fill = t.button_hover;
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, t.text);
    v.widgets.active.bg_fill = t.accent.gamma_multiply(0.7);
    v.widgets.active.weak_bg_fill = t.accent.gamma_multiply(0.7);
    v.widgets.open.weak_bg_fill = t.button_hover;
    // Flat: no shadows (the 1 px separator is the only edge), one corner radius.
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.window_corner_radius = egui::CornerRadius::same(RADIUS as u8);
    v.menu_corner_radius = egui::CornerRadius::same(RADIUS as u8);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = egui::CornerRadius::same(RADIUS_SM as u8);
        w.expansion = 0.0;
    }
    v
}

/// The spacing grid on egui's own widgets.
fn spacing(st: &mut egui::Style) {
    let sp = &mut st.spacing;
    sp.item_spacing = egui::vec2(SPACE, SPACE / 2.0);
    sp.button_padding = egui::vec2(SPACE, SPACE / 4.0);
    sp.menu_margin = egui::Margin::same(SPACE as i8);
    sp.window_margin = egui::Margin::same(SPACE as i8);
    sp.indent = 2.0 * SPACE;
}

/// Install both themes' visuals and follow `appearance` (System: the OS setting, through egui's
/// `system_theme`). Call when the appearance changes; [`follow`] runs every frame.
pub fn apply(ctx: &egui::Context, appearance: Appearance) {
    for (theme, tokens) in [(egui::Theme::Dark, &DARK), (egui::Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |st| {
            st.visuals = visuals(tokens, theme == egui::Theme::Light);
            spacing(st);
        });
    }
    ctx.set_theme(appearance.preference());
    set_current(ctx.theme());
}

/// Point the tokens at the theme egui resolved for this frame (System follows the OS live).
pub fn follow(ctx: &egui::Context) {
    set_current(ctx.theme());
}

/// WCAG relative luminance of an opaque color.
pub fn luminance(c: Color32) -> f32 {
    let lin = |x: u8| {
        let v = f32::from(x) / 255.0;
        if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio of two opaque colors (1 to 21).
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// A readable text color on `bg`: whichever of the dark and light inks contrasts more.
pub fn text_on(bg: Color32) -> Color32 {
    let t = &TOKENS;
    if contrast(t.ink_dark, bg) >= contrast(t.ink_light, bg) { t.ink_dark } else { t.ink_light }
}

/// A clip's colors from its track color: (body, name strip, name text). Dark theme: the track
/// color as the body under a darker strip; light theme: a lightened body under a strip in the
/// track color. The strip is nudged darker or lighter until its name reads at 4.5:1.
pub fn clip_colors(base: Color32, selected: bool) -> (Color32, Color32, Color32) {
    let t = &TOKENS;
    let (body, mut strip) = if t.light_clips {
        let body = lighten(base, if selected { 0.25 } else { 0.4 });
        (body, if selected { darken(base, 0.08) } else { base })
    } else {
        let body = if selected { lighten(base, 0.12) } else { base };
        (body, darken(body, 0.22))
    };
    for _ in 0..20 {
        let ink = text_on(strip);
        if contrast(ink, strip) >= 4.5 {
            break;
        }
        strip = if ink == t.ink_light { darken(strip, 0.06) } else { lighten(strip, 0.06) };
    }
    (body, strip, text_on(strip))
}

/// `c` mixed toward black by `k` (0..1).
pub fn darken(c: Color32, k: f32) -> Color32 {
    let f = (1.0 - k).clamp(0.0, 1.0);
    Color32::from_rgb((f32::from(c.r()) * f) as u8, (f32::from(c.g()) * f) as u8, (f32::from(c.b()) * f) as u8)
}

/// `c` mixed toward white by `k` (0..1).
pub fn lighten(c: Color32, k: f32) -> Color32 {
    let k = k.clamp(0.0, 1.0);
    let m = |x: u8| (f32::from(x) + (255.0 - f32::from(x)) * k) as u8;
    Color32::from_rgb(m(c.r()), m(c.g()), m(c.b()))
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// The family apps/transit installs for numbers (JetBrains Mono Medium).
const MONO_MEDIUM: &str = "mono-medium";

/// Notes, once a frame, whether the `mono-medium` family exists (an egui font id naming a family
/// that was not installed panics, and tests and `--no-fonts` runs have only the defaults).
pub fn probe_fonts(ctx: &egui::Context) {
    let has = ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name(MONO_MEDIUM.into())));
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("transit-mono-medium"), has));
}

/// Digits for clocks and readouts: `mono-medium` where the app installed it, else Monospace.
pub fn mono(ctx: &egui::Context, size: f32) -> egui::FontId {
    let has = ctx.data(|d| d.get_temp::<bool>(egui::Id::new("transit-mono-medium"))).unwrap_or(false);
    if has { egui::FontId::new(size, egui::FontFamily::Name(MONO_MEDIUM.into())) } else { egui::FontId::monospace(size) }
}
