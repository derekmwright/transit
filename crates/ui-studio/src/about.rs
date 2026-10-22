//! Help > About transit: the wordmark, the version, the credit to SoundCraft, the licenses and a
//! button that copies the facts a bug report needs.

use crate::StudioApp;
use crate::arrange::{R4, r4};
use crate::theme::{SPACE, TOKENS};

/// Where transit's code is based, and the credit link (moved here from the Help menu).
const CREDIT: &str = "Based on SoundCraft by storytold";
const CREDIT_LINK: &str = "github.com/storytold/soundcraft";
const CREDIT_URL: &str = "https://github.com/storytold/soundcraft";
const LICENSES: &str = "MIT OR Apache-2.0";
/// Wordmark width in points.
const WORDMARK_W: f32 = 260.0;

/// The About window's state and last frame's layout (for `ui.inspect` and tests).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AboutState {
    pub open: bool,
    /// The app's own version (the transit crate's), set at launch; empty falls back to this crate's.
    pub app_version: String,
    pub window: R4,
    pub wordmark: R4,
    /// The "transit <version>" line.
    pub version: String,
    pub credit: String,
    pub licenses: String,
    pub copy: R4,
    pub close: R4,
    /// What the Copy button put on the clipboard last.
    pub copied: String,
}

/// "transit 0.1.0-alpha.1" (no git hash is embedded in the build).
pub fn version_line(app: &StudioApp) -> String {
    let v = if app.about.app_version.is_empty() { env!("CARGO_PKG_VERSION") } else { app.about.app_version.as_str() };
    format!("transit {v}")
}

/// The text "Copy version info" copies: version, OS, and the audio device when one is open.
pub fn version_info(app: &StudioApp) -> String {
    let mut out = format!("{}\nOS: {} {}\n", version_line(app), std::env::consts::OS, std::env::consts::ARCH);
    match &app.player {
        Some(p) => {
            let frames = p.device_buffer.map_or("default".to_string(), |b| b.to_string());
            out += &format!(
                "Audio system: {}\nAudio device: {}\nSample rate: {} Hz\nCallback frames: {frames}\n",
                p.device_system, p.device_name, p.device_rate
            );
        }
        None => out += "Audio: no device open\n",
    }
    out
}

/// Draw the window while it is open.
pub fn window(app: &mut StudioApp, ctx: &egui::Context) {
    let info = version_info(app);
    let version = version_line(app);
    let mut s = std::mem::take(&mut app.about);
    s.window = [0.0; 4];
    if !s.open {
        app.about = s;
        return;
    }
    let mut open = true;
    let mut close = false;
    let r = egui::Window::new("About transit")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(SPACE);
                let (r, _) = ui.allocate_exact_size(egui::vec2(WORDMARK_W, WORDMARK_W / crate::icons::WORDMARK_ASPECT), egui::Sense::hover());
                s.wordmark = r4(crate::icons::paint_wordmark(ui.painter(), r.center().x, r.min.y, WORDMARK_W));
                ui.add_space(SPACE);
                s.version = version.clone();
                ui.label(egui::RichText::new(&s.version).strong().size(16.0).color(TOKENS.text));
                ui.add_space(SPACE / 2.0);
                s.credit = CREDIT.to_string();
                ui.label(egui::RichText::new(CREDIT).color(TOKENS.text_dim));
                ui.hyperlink_to(CREDIT_LINK, CREDIT_URL);
                ui.add_space(SPACE / 2.0);
                s.licenses = LICENSES.to_string();
                ui.label(egui::RichText::new(format!("Licensed {LICENSES}")).color(TOKENS.text_dim));
                ui.add_space(SPACE);
                ui.horizontal(|ui| {
                    let copy = ui.button("Copy version info");
                    s.copy = r4(copy.rect);
                    if copy.clicked() {
                        ui.ctx().copy_text(info.clone());
                        s.copied = info.clone();
                    }
                    let b = ui.button("Close");
                    s.close = r4(b.rect);
                    close = b.clicked();
                });
                ui.add_space(SPACE / 2.0);
            });
        });
    if let Some(r) = r {
        s.window = r4(r.response.rect);
    }
    s.open = open && !close;
    app.about = s;
}
