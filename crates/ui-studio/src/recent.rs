//! File > Open Recent: the sessions last opened or saved on this machine, most recent first.
//!
//! The list is a per-machine preference in `UiState` (`ui.json`): changing it never touches the
//! document, its undo history or its dirty flag.

use crate::StudioApp;
use crate::shortcuts::{Action as A, label};
use crate::theme::TOKENS;
use serde_json::json;
use std::path::{Path, PathBuf};

/// The most sessions the menu remembers.
pub const MAX_RECENT: usize = 10;

/// One remembered session file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecentFile {
    pub path: PathBuf,
    /// When it was last opened or saved, in unix seconds.
    pub opened_at: u64,
}

/// One entry of the Open Recent submenu as last drawn (for `ui.inspect` and the tests).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RecentItemLayout {
    pub name: String,
    /// The last component of the parent folder, shown dimmed.
    pub folder: String,
    pub path: String,
    /// The file is gone, so the entry is drawn dimmed.
    pub missing: bool,
    pub rect: crate::arrange::R4,
}

fn same(a: &Path, b: &Path) -> bool {
    if cfg!(windows) { a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()) } else { a == b }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl crate::UiState {
    /// Make `path` the most recent entry: deduplicated, newest first, capped at [`MAX_RECENT`].
    pub fn remember_recent(&mut self, path: &Path, opened_at: u64) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.recent_files.retain(|r| !same(&r.path, &path));
        self.recent_files.insert(0, RecentFile { path, opened_at });
        self.recent_files.truncate(MAX_RECENT);
    }

    /// Drop `path` from the list.
    pub fn forget_recent(&mut self, path: &Path) {
        self.recent_files.retain(|r| !same(&r.path, path));
    }
}

impl StudioApp {
    /// After a successful open or save: the session's file becomes the most recent. The autosave
    /// folder's files are never listed.
    pub(crate) fn note_recent(&mut self) {
        let Some(path) = self.engine.path.clone().map(PathBuf::from) else { return };
        let abs = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
        if self.autosave_dir.as_deref().is_some_and(|dir| abs.starts_with(dir)) {
            return;
        }
        self.ui.remember_recent(&path, now());
    }

    /// File > Open Recent > an entry: the same path as File > Open (including the save-changes
    /// question). A file that is gone says so and leaves the list.
    pub fn open_recent(&mut self, path: &Path) {
        if !path.exists() {
            self.status = "File not found".into();
            self.ui.forget_recent(path);
            return;
        }
        self.run_or_ask("session.open", json!({"path": path.to_string_lossy()}));
    }

    /// Ctrl+Shift+O: the most recent session.
    pub fn open_most_recent(&mut self) {
        if let Some(path) = self.ui.recent_files.first().map(|r| r.path.clone()) {
            self.open_recent(&path);
        }
    }
}

/// The Open Recent submenu, drawn inside the File menu.
pub fn menu(app: &mut StudioApp, ui: &mut egui::Ui) {
    let empty = app.ui.recent_files.is_empty();
    let sub = ui.add_enabled_ui(!empty, |ui| {
        ui.menu_button("Open Recent", |ui| {
            let mut picked = None;
            let entries: Vec<PathBuf> = app.ui.recent_files.iter().map(|r| r.path.clone()).collect();
            for path in entries {
                let name = path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
                let folder = path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let missing = !path.exists();
                let text = egui::RichText::new(&name);
                let text = if missing { text.color(TOKENS.text_dim) } else { text };
                let b = ui.add(egui::Button::new(text).shortcut_text(egui::RichText::new(&folder).color(TOKENS.text_dim)));
                app.recent_menu.push(RecentItemLayout {
                    name,
                    folder,
                    path: path.to_string_lossy().into_owned(),
                    missing,
                    rect: crate::mixer::r4(b.rect),
                });
                if b.clicked() {
                    picked = Some(path);
                }
            }
            ui.separator();
            let clear = ui.button("Clear Menu");
            app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Clear Menu".into(), String::new(), clear.rect));
            if clear.clicked() {
                app.ui.recent_files.clear();
                ui.close();
            }
            if let Some(path) = picked {
                ui.close();
                app.open_recent(&path);
            }
        })
    });
    let r = sub.inner.response.rect;
    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Open Recent".into(), label(A::OpenRecent), r));
}
