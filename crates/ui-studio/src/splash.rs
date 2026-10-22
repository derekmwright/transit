//! The launch splash: while plug-ins are being scanned (or the first audio device is opening) the
//! main window shows the wordmark, a one-line status and a thin progress bar instead of an empty
//! arrangement. It is a panel drawn by the frame loop, never a modal wait: input keeps flowing,
//! the scan and the device open run on their own threads, and a click anywhere (or Skip) hides it.

use crate::StudioApp;
use crate::arrange::{R4, r4};
use crate::plugin_scan::ScanProgress;
use crate::theme::{SPACE, TOKENS};

const WORDMARK_W: f32 = 300.0;
const BAR_W: f32 = 320.0;
const BAR_H: f32 = 4.0;

/// What the splash watches this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Watch {
    /// A plug-in scan is running; with counts when the host crates report them.
    Scan(Option<ScanProgress>),
    /// The named audio device is being opened.
    Device(String),
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SplashState {
    /// Armed by the app at launch; an unarmed splash (tests, agents) never shows.
    armed: bool,
    /// Latched once the splash has gone: it does not come back for a later rescan.
    pub finished: bool,
    /// The device being opened right now, when the app is opening one.
    pub opening_device: Option<String>,
    /// Test hook: replaces the global scan state (`Some(None)` is a scan without counts).
    #[serde(skip)]
    pub scan_override: Option<Option<ScanProgress>>,
    pub visible: bool,
    pub status: String,
    pub fraction: Option<f32>,
    pub wordmark: R4,
    pub bar: R4,
    pub skip: R4,
}

impl SplashState {
    /// Show the splash from the next frame until the work it watches is done.
    pub fn arm(&mut self) {
        self.armed = true;
    }

    /// Hide it for good (Skip, a click, or the work finishing).
    pub fn dismiss(&mut self) {
        self.finished = true;
        self.visible = false;
    }

    fn watching(&self) -> Option<Watch> {
        if let Some(d) = &self.opening_device {
            return Some(Watch::Device(d.clone()));
        }
        if let Some(fake) = &self.scan_override {
            return Some(Watch::Scan(fake.clone()));
        }
        (crate::plugin_scan::state() == "running").then(|| Watch::Scan(crate::plugin_scan::progress()))
    }
}

/// The status line and progress fraction for what is being watched.
pub fn describe(w: &Watch) -> (String, Option<f32>) {
    match w {
        Watch::Scan(Some(p)) => {
            let at = if p.folder.is_empty() { String::new() } else { format!(" ({})", p.folder) };
            (format!("Scanning plug-ins… {} of {}{at}", p.index, p.total), (p.total > 0).then(|| p.index as f32 / p.total as f32))
        }
        Watch::Scan(None) => ("Scanning plug-ins…".to_string(), None),
        Watch::Device(name) => (format!("Opening {name}…"), None),
    }
}

/// Decide, once per frame before the panels are laid out, whether the splash shows. Returns it.
pub fn update(app: &mut StudioApp, ctx: &egui::Context) -> bool {
    let s = &mut app.splash;
    if !s.armed || s.finished {
        s.visible = false;
        return false;
    }
    // A press anywhere hides it (the press still reaches whatever it landed on).
    if ctx.input(|i| i.pointer.any_pressed()) {
        s.dismiss();
        return false;
    }
    match s.watching() {
        Some(w) => {
            (s.status, s.fraction) = describe(&w);
            s.visible = true;
        }
        None => s.dismiss(),
    }
    s.visible
}

/// Draw the splash panel in the space the arrangement would take.
pub fn show(app: &mut StudioApp, ui: &mut egui::Ui) {
    let device = app.player.as_ref().filter(|p| !p.silent).map(|p| p.device_name.clone());
    let s = &mut app.splash;
    let mut skip = false;
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(TOKENS.bg)).show(ui, |ui| {
        let area = ui.max_rect();
        let h = WORDMARK_W / crate::icons::WORDMARK_ASPECT + 5.0 * SPACE + 40.0;
        let top = (area.center().y - h / 2.0).max(area.min.y);
        let x = area.center().x;
        let mut y = top;
        let wm = crate::icons::paint_wordmark(ui.painter(), x, y, WORDMARK_W);
        s.wordmark = r4(wm);
        y = wm.max.y + 2.0 * SPACE;
        let p = ui.painter();
        let line = p.layout_no_wrap(s.status.clone(), egui::FontId::proportional(13.0), TOKENS.text);
        let line_h = line.size().y;
        p.galley(egui::pos2(x - line.size().x / 2.0, y), line, TOKENS.text);
        y += line_h + SPACE;
        let bar = egui::Rect::from_center_size(egui::pos2(x, y + BAR_H / 2.0), egui::vec2(BAR_W, BAR_H));
        let r = egui::CornerRadius::same(crate::theme::RADIUS_SM as u8);
        p.rect_filled(bar, r, TOKENS.panel_raised);
        match s.fraction {
            Some(f) => {
                p.rect_filled(egui::Rect::from_min_size(bar.min, egui::vec2(BAR_W * f.clamp(0.0, 1.0), BAR_H)), r, TOKENS.accent);
            }
            None => {
                // No counts yet: a short segment sliding along the track.
                let t = ui.input(|i| i.time) as f32;
                let seg = BAR_W / 4.0;
                let at = (t * 0.8).fract() * (BAR_W - seg);
                p.rect_filled(egui::Rect::from_min_size(bar.min + egui::vec2(at, 0.0), egui::vec2(seg, BAR_H)), r, TOKENS.accent);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
            }
        }
        s.bar = r4(bar);
        y = bar.max.y + SPACE;
        if let Some(d) = &device {
            let g = p.layout_no_wrap(format!("Audio: {d}"), egui::FontId::proportional(11.0), TOKENS.text_dim);
            let gh = g.size().y;
            p.galley(egui::pos2(x - g.size().x / 2.0, y), g, TOKENS.text_dim);
            y += gh + SPACE / 2.0;
        }
        let skip_rect = egui::Rect::from_center_size(egui::pos2(x, y + 10.0), egui::vec2(48.0, 20.0));
        let resp = ui.put(skip_rect, egui::Label::new(egui::RichText::new("Skip").color(TOKENS.accent).underline()).sense(egui::Sense::click()));
        s.skip = r4(resp.rect);
        skip = resp.clicked();
    });
    if skip {
        s.dismiss();
    }
}
