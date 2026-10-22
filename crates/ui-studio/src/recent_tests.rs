//! File > Open Recent: the list, the submenu, the missing-file case and the persisted field.

use crate::{Services, StudioApp, UiState};
use serde_json::{Value, json};
use std::path::PathBuf;

struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("transit-recent-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
    dir: Dir,
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

impl T {
    fn new() -> T {
        let mut t = T {
            app: StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default()),
            ctx: egui::Context::default(),
            time: 0.0,
            dir: Dir::new(),
        };
        t.frames(3);
        t
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.time += 1.0 / 60.0;
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 800.0))),
                time: Some(self.time),
                ..Default::default()
            };
            self.app.raw_input_hook(&mut raw);
            let app = &mut self.app;
            let mut out = self.ctx.run_ui(raw, |ui| {
                app.logic(ui.ctx());
                app.ui(ui);
            });
            out.textures_delta.clear();
        }
    }

    fn input(&mut self, method: &str, p: Value) {
        let (req, _rx) = crate::ControlRequest::new(method, p);
        let ctx = egui::Context::default();
        let _ = crate::control::handle_for_test(&mut self.app, &ctx, &req);
        self.frames(30);
    }

    fn click(&mut self, at: (f32, f32)) {
        self.input("ui.click", json!({"x": at.0, "y": at.1, "count": 1}));
    }

    /// Save a new session named `name` in the fixture folder and return its path.
    fn save_new(&mut self, name: &str) -> String {
        self.app.run("session.new", json!({"name": name})).unwrap();
        let path = self.dir.path(&format!("{name}.transit"));
        self.app.run("session.save_as", json!({"path": path, "replace": true})).unwrap();
        path
    }

    fn open(&mut self, path: &str) {
        self.app.run("session.open", json!({"path": path})).unwrap();
    }

    fn recent(&self) -> Vec<String> {
        self.app.ui.recent_files.iter().map(|r| r.path.to_string_lossy().into_owned()).collect()
    }

    /// File > Open Recent, opened by clicks; the entries as drawn.
    fn show_menu(&mut self) -> Vec<crate::recent::RecentItemLayout> {
        let file = self.app.arrange.bar_menu.iter().find(|m| m.label == "File").cloned().expect("the File menu");
        self.click(mid(file.rect));
        let sub = self.app.arrange.bar_menu.iter().find(|m| m.label == "Open Recent").cloned().expect("Open Recent in the File menu");
        self.click(mid(sub.rect));
        self.app.recent_menu.clone()
    }

    fn click_clear(&mut self) {
        let clear = self.app.arrange.bar_menu.iter().find(|m| m.label == "Clear Menu").cloned().expect("Clear Menu");
        self.click(mid(clear.rect));
    }
}

fn abs(p: &str) -> String {
    std::path::absolute(p).unwrap().to_string_lossy().into_owned()
}

#[test]
fn opened_and_saved_sessions_list_most_recent_first_without_duplicates() {
    let mut t = T::new();
    let a = t.save_new("Alpha");
    let b = t.save_new("Bravo");
    assert_eq!(t.recent(), [abs(&b), abs(&a)], "a save makes the file the most recent");
    t.open(&a);
    t.open(&b);
    t.open(&a);
    assert_eq!(t.recent(), [abs(&a), abs(&b)], "opening one again moves it to the top, no duplicate");
    let items = t.show_menu();
    assert_eq!(items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["Alpha.transit", "Bravo.transit"]);
    assert_eq!(items[0].folder, t.dir.0.file_name().unwrap().to_string_lossy());
    assert!(items.iter().all(|i| !i.missing));
    assert!(!t.app.engine.is_dirty(), "the list is not part of the document");
    assert!(t.app.engine.undo_history().is_empty());
}

#[test]
fn picking_an_entry_opens_it_through_file_open() {
    let mut t = T::new();
    let a = t.save_new("Alpha");
    t.save_new("Bravo");
    assert_eq!(t.app.engine.session().name, "Bravo");
    let items = t.show_menu();
    let alpha = items.iter().find(|i| i.name == "Alpha.transit").unwrap();
    t.click(mid(alpha.rect));
    assert_eq!(t.app.engine.session().name, "Alpha");
    assert_eq!(t.app.engine.path.as_deref(), Some(a.as_str()));
    assert_eq!(t.app.ui.recent_files[0].path, PathBuf::from(abs(&a)));
}

#[test]
fn picking_an_entry_with_unsaved_changes_asks_first() {
    let mut t = T::new();
    let a = t.save_new("Alpha");
    t.save_new("Bravo");
    t.app.run("track.new", json!({"kind": "audio"})).unwrap();
    assert!(t.app.engine.is_dirty());
    t.app.open_recent(std::path::Path::new(&a));
    assert!(matches!(t.app.dialog, Some(crate::Dialog::SaveChanges { .. })));
    assert_eq!(t.app.engine.session().name, "Bravo", "nothing opened yet");
}

#[test]
fn the_list_holds_ten() {
    let mut t = T::new();
    let paths: Vec<String> = (0..12).map(|i| t.save_new(&format!("S{i:02}"))).collect();
    let want: Vec<String> = paths.iter().rev().take(10).map(|p| abs(p)).collect();
    assert_eq!(t.recent(), want);
}

#[test]
fn a_missing_file_is_dimmed_and_picking_it_removes_it() {
    let mut t = T::new();
    let a = t.save_new("Alpha");
    let b = t.save_new("Bravo");
    std::fs::remove_file(&a).unwrap();
    let items = t.show_menu();
    assert_eq!(items.iter().map(|i| (i.name.as_str(), i.missing)).collect::<Vec<_>>(), [("Bravo.transit", false), ("Alpha.transit", true)]);
    let gone = items[1].rect;
    t.click(mid(gone));
    assert_eq!(t.app.status, "File not found");
    assert_eq!(t.recent(), [abs(&b)]);
    assert_eq!(t.app.engine.session().name, "Bravo", "the open session is untouched");
}

#[test]
fn clear_menu_empties_the_list() {
    let mut t = T::new();
    t.save_new("Alpha");
    t.save_new("Bravo");
    t.show_menu();
    t.click_clear();
    assert!(t.app.ui.recent_files.is_empty());
}

#[test]
fn ctrl_shift_o_opens_the_most_recent() {
    let mut t = T::new();
    let a = t.save_new("Alpha");
    t.save_new("Bravo");
    t.app.ui.recent_files.reverse();
    t.input("ui.key", json!({"key": "O", "cmd": true, "shift": true}));
    assert_eq!(t.app.engine.path.as_deref(), Some(a.as_str()));
}

#[test]
fn autosave_files_are_never_listed() {
    let mut t = T::new();
    let auto = t.dir.0.join("Autosave");
    std::fs::create_dir_all(&auto).unwrap();
    t.app.autosave_dir = Some(auto.clone());
    let p = auto.join("x.transit").to_string_lossy().into_owned();
    t.app.run("session.save_as", json!({"path": p, "replace": true})).unwrap();
    assert!(t.recent().is_empty());
}

#[test]
fn ui_json_round_trips_the_list_and_an_old_file_loads() {
    let mut ui = UiState::default();
    ui.remember_recent(std::path::Path::new("/music/a.transit"), 100);
    ui.remember_recent(std::path::Path::new("/music/b.transit"), 200);
    let text = serde_json::to_string(&ui).unwrap();
    let back: UiState = serde_json::from_str(&text).unwrap();
    assert_eq!(back.recent_files, ui.recent_files);
    assert_eq!(back.recent_files[0].opened_at, 200);
    let mut old: Value = serde_json::to_value(UiState::default()).unwrap();
    old.as_object_mut().unwrap().remove("recent_files");
    assert!(serde_json::from_value::<UiState>(old).unwrap().recent_files.is_empty());
}
