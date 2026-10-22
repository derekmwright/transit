//! Browser (slice 4) tests. Every gesture is driven the way a user drives it: synthetic clicks,
//! drags, wheel, keys and text through the control channel's input queue, one frame at a time.
//! Files live in a temp folder each test makes; no player, no plug-ins, no scan.

use crate::browser::{Kind, Tab, parse_header};
use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::{ClipContent, TrackId, TrackKind};
use soundcraft_time::{BarBeat, Samples};
use std::path::{Path, PathBuf};

/// A 16-bit PCM WAV.
fn wav(channels: u16, rate: u32, frames: u32) -> Vec<u8> {
    let data = frames * u32::from(channels) * 2;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    b.extend_from_slice(&(channels * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..frames * u32::from(channels) {
        let v = ((i % 64) as i16 - 32) * 300;
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

/// A folder of its own under the temp dir, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("transit-browser-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
    fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }
    fn s(&self) -> String {
        self.0.to_string_lossy().into_owned()
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
    rate: u32,
    dir: Dir,
}

impl T {
    /// An audio track and an instrument track, the browser shown, a sample folder with
    /// `Kicks/` (two WAVs), `Snares/` (one), a stereo loop, a Kontakt file and a text file.
    fn new() -> T {
        let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        app.run("track.new", json!({"kind": "audio", "format": "stereo"})).unwrap();
        app.run("track.new", json!({"kind": "instrument"})).unwrap();
        app.ui.show_mixer = false;
        app.ui.show_devices = false;
        app.ui.browser.show = true;
        let rate = app.engine.session().sample_rate.hz();
        let dir = Dir::new("t");
        dir.file("Kicks/Kick 01.wav", &wav(1, rate, rate / 2));
        dir.file("Kicks/Kick 02.wav", &wav(1, rate, rate / 4));
        dir.file("Snares/Snare 01.wav", &wav(1, rate, rate / 4));
        dir.file("loop 90.wav", &wav(2, rate, rate * 2));
        dir.file("Kit.nki", b"not a wav");
        dir.file("readme.txt", b"hello");
        let mut t = T { app, ctx: egui::Context::default(), time: 0.0, rate, dir };
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

    /// Frames until the background threads have answered (or 5 s).
    fn settle(&mut self) {
        let t0 = std::time::Instant::now();
        loop {
            self.frames(2);
            let b = &self.app.browser.layout;
            let waiting = b.pending > 0 || b.rows.iter().any(|r| r.kind == Some(Kind::Audio) && r.rect.is_some() && r.info.is_none());
            if !waiting || t0.elapsed().as_secs() > 5 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        self.frames(2);
    }

    /// Frames until every import has been committed or dropped (or 10 s).
    fn settle_imports(&mut self) {
        let t0 = std::time::Instant::now();
        while self.app.imports.active() && t0.elapsed().as_secs() < 10 {
            self.frames(6);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!self.app.imports.active(), "imports never finished: {}", self.app.status);
        self.frames(2);
    }

    fn input(&mut self, method: &str, p: Value) {
        let (req, _rx) = crate::ControlRequest::new(method, p);
        let ctx = egui::Context::default();
        let _ = crate::control::handle_for_test(&mut self.app, &ctx, &req);
        self.frames(30);
    }

    /// A double-click after a pause (egui counts a third click within 0.6 s as a triple click).
    fn double_click(&mut self, at: (f32, f32)) {
        self.frames(40);
        self.click_n(at, 2, json!({}));
    }

    fn click_n(&mut self, at: (f32, f32), count: u64, mods: Value) {
        self.input("ui.click", merged(json!({"x": at.0, "y": at.1, "count": count}), mods));
    }

    fn click(&mut self, at: (f32, f32)) {
        self.click_n(at, 1, json!({}));
    }

    fn drag(&mut self, from: (f32, f32), to: (f32, f32), mods: Value) {
        self.input("ui.drag", merged(json!({"x": from.0, "y": from.1, "to_x": to.0, "to_y": to.1}), mods));
    }

    fn key(&mut self, key: &str, mods: Value) {
        self.input("ui.key", merged(json!({"key": key}), mods));
    }

    fn text(&mut self, s: &str) {
        self.input("ui.text", json!({"text": s}));
    }

    /// Add the sample folder through "+", the path field and Enter, then expand it.
    fn add_and_expand(&mut self) {
        let plus = self.app.browser.layout.add_folder;
        self.click(mid(plus));
        let field = self.app.browser.layout.add_field;
        self.click(mid(field));
        let s = self.dir.s();
        self.text(&s);
        self.key("enter", json!({}));
        assert_eq!(self.app.ui.browser.locations, vec![s.clone()], "{}", self.app.status);
        self.click(mid(self.row(&s).rect.unwrap()));
        self.settle();
    }

    fn row(&self, path: &str) -> crate::browser::RowLayout {
        self.app.browser.layout.rows.iter().find(|r| r.path == path).cloned().unwrap_or_else(|| panic!("no row {path}: {:?}", self.names()))
    }

    fn row_named(&self, name: &str) -> crate::browser::RowLayout {
        self.app.browser.layout.rows.iter().find(|r| r.name == name).cloned().unwrap_or_else(|| panic!("no row {name}: {:?}", self.names()))
    }

    fn names(&self) -> Vec<String> {
        self.app.browser.layout.rows.iter().map(|r| r.name.clone()).collect()
    }

    fn path(&self, rel: &str) -> String {
        self.dir.0.join(rel).to_string_lossy().into_owned()
    }

    fn undo_len(&self) -> usize {
        self.app.engine.undo_history().len()
    }

    fn track(&self, kind: TrackKind) -> TrackId {
        self.app.engine.session().tracks.iter().find(|t| t.kind == kind).unwrap().id
    }

    fn lane(&self, t: TrackId) -> [f32; 4] {
        self.app.arrange.layout.tracks.iter().find(|r| r.id == t.0).unwrap().lane
    }

    /// The x of `at` on the timeline.
    fn x_of(&self, at: Samples) -> f32 {
        let l = &self.app.arrange.layout;
        l.timeline[0] + ((at - l.scroll) as f64 / l.samples_per_px) as f32
    }

    fn bar(&self, bar: i64) -> Samples {
        let s = self.app.engine.session();
        s.tempo.samples_at_bar_beat(BarBeat { bar, beat: 1, tick: 0 }, s.sample_rate)
    }

    /// (start, length, name) of the audio clips on `t`.
    fn clips(&self, t: TrackId) -> Vec<(Samples, Samples, String)> {
        let s = self.app.engine.session();
        s.track(t)
            .unwrap()
            .clips()
            .iter()
            .filter(|c| matches!(c.content, ClipContent::Audio { .. }))
            .map(|c| (c.start, c.end() - c.start, c.name.clone()))
            .collect()
    }

    fn plugin(&self, name: &str) -> crate::browser::PluginRowLayout {
        self.app.browser.layout.plugins.iter().find(|p| p.row == "plugin" && p.name == name).cloned().unwrap_or_else(|| panic!("no plug-in {name}"))
    }
}

fn merged(mut a: Value, b: Value) -> Value {
    if let (Some(o), Some(m)) = (a.as_object_mut(), b.as_object()) {
        o.extend(m.clone());
    }
    a
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

#[test]
fn ctrl_alt_b_and_the_view_menu_show_and_hide_the_browser_and_never_dirty_the_document() {
    let mut t = T::new();
    let u0 = t.undo_len();
    let dirty = t.app.engine.is_dirty();
    assert!(t.app.browser.layout.visible);
    let list_x = t.app.arrange.layout.track_list[0];
    assert!(list_x >= t.app.browser.layout.panel[2] - 1.0, "the arrangement sits right of the browser");
    t.key("B", json!({"cmd": true, "alt": true}));
    assert!(!t.app.ui.browser.show && !t.app.browser.layout.visible);
    assert!(t.app.arrange.layout.track_list[0] < 1.0, "the arrangement takes the room back");
    // View › Browser.
    let view = t.app.arrange.bar_menu.iter().find(|m| m.label == "View").cloned().expect("the View menu");
    t.click(mid(view.rect));
    let item = t.app.arrange.bar_menu.iter().find(|m| m.label == "Browser").cloned().expect("Browser in the View menu");
    t.click(mid(item.rect));
    assert!(t.app.ui.browser.show && t.app.browser.layout.visible);
    assert_eq!((t.undo_len(), t.app.engine.is_dirty()), (u0, dirty));
}

#[test]
fn tabs_switch_between_files_and_plugins() {
    let mut t = T::new();
    assert_eq!(t.app.ui.browser.tab, Tab::Files);
    t.click(mid(t.app.browser.layout.tab_plugins));
    assert_eq!(t.app.ui.browser.tab, Tab::Plugins);
    assert!(t.app.browser.layout.plugins.iter().any(|p| p.row == "heading" && p.name == "Instruments"));
    assert!(t.app.browser.layout.scan.contains("not scanned"), "the scan state shows: {}", t.app.browser.layout.scan);
    t.click(mid(t.app.browser.layout.tab_files));
    assert_eq!(t.app.ui.browser.tab, Tab::Files);
}

#[test]
fn the_right_edge_drags_the_width_kept_in_ui_state_and_clamped() {
    let mut t = T::new();
    let u0 = t.undo_len();
    let at = mid(t.app.browser.layout.handle);
    t.drag(at, (at.0 + 60.0, at.1), json!({}));
    assert!((t.app.ui.browser.width - 340.0).abs() < 8.0, "{}", t.app.ui.browser.width);
    assert!((t.app.browser.layout.panel[2] - t.app.ui.browser.width).abs() < 2.0);
    let at = mid(t.app.browser.layout.handle);
    t.drag(at, (at.0 - 400.0, at.1), json!({}));
    assert_eq!(t.app.ui.browser.width, 200.0, "never narrower than 200");
    assert_eq!(t.undo_len(), u0);
    let saved = serde_json::to_value(&t.app.ui).unwrap();
    assert_eq!(saved["browser"]["width"], json!(200.0), "kept with the preferences");
}

#[test]
fn add_folder_by_typing_a_path_then_x_removes_the_location() {
    let mut t = T::new();
    let u0 = t.undo_len();
    let dirty = t.app.engine.is_dirty();
    t.add_and_expand();
    assert!(t.app.status.contains("Added"), "{}", t.app.status);
    // The same folder again is refused.
    t.click(mid(t.app.browser.layout.add_folder));
    t.click(mid(t.app.browser.layout.add_field));
    let s = t.dir.s();
    t.text(&s);
    t.click(mid(t.app.browser.layout.add_ok));
    assert!(t.app.status.contains("already"), "{}", t.app.status);
    assert_eq!(t.app.ui.browser.locations.len(), 1);
    // Escape closes the field.
    t.click(mid(t.app.browser.layout.add_field));
    t.key("escape", json!({}));
    assert!(t.app.browser.adding.is_none());
    // Hover the location, then its ×.
    let root = t.row(&s);
    let c = mid(root.rect.unwrap());
    t.input("ui.move", json!({"x": c.0, "y": c.1}));
    t.click(mid(root.remove.unwrap()));
    assert!(t.app.ui.browser.locations.is_empty());
    assert!(t.app.browser.layout.rows.is_empty());
    assert_eq!((t.undo_len(), t.app.engine.is_dirty()), (u0, dirty));
}

#[test]
fn browse_adds_the_folder_the_dialog_picks() {
    let mut t = T::new();
    let picked = t.dir.s();
    t.app.services.pick_open = Some(Box::new(move |purpose: &str| (purpose == "folder").then(|| picked.clone())));
    t.click(mid(t.app.browser.layout.add_folder));
    let browse = t.app.browser.layout.add_browse;
    assert!(browse[2] > browse[0], "Browse… shows when the app has a dialog");
    t.click(mid(browse));
    assert_eq!(t.app.ui.browser.locations, vec![t.dir.s()]);
    assert!(t.app.browser.adding.is_none(), "the field closes");
}

#[test]
fn expanding_lists_folders_first_in_the_background_and_reads_no_subfolder() {
    let mut t = T::new();
    let u0 = t.undo_len();
    let dirty = t.app.engine.is_dirty();
    t.add_and_expand();
    let root = t.dir.s();
    let names: Vec<String> = t.app.browser.layout.rows.iter().filter(|r| r.depth == 1).map(|r| r.name.clone()).collect();
    assert_eq!(names, vec!["Kicks", "Snares", "Kit.nki", "loop 90.wav"], "folders first, then by name; the text file is left out");
    let l = &t.app.browser.layout.listings[&root];
    assert_eq!((l.state, l.entries, l.raw, l.folders, l.other), ("ready", 4, 5, 2, 1));
    assert_eq!(t.app.browser.layout.dirs_read, 1, "Kicks and Snares were not read");
    assert!(!t.app.browser.layout.listings.contains_key(&t.path("Kicks")));
    let nki = t.row_named("Kit.nki");
    assert_eq!(nki.kind, Some(Kind::Closed), "Kontakt files are listed greyed");
    // Expand Kicks: one more directory read; collapse it again.
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    t.settle();
    assert_eq!(t.app.browser.layout.dirs_read, 2);
    assert!(t.row(&t.path("Kicks")).expanded);
    assert!(t.names().contains(&"Kick 02.wav".to_string()));
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    assert!(!t.names().contains(&"Kick 02.wav".to_string()), "collapsed");
    assert!(!t.app.browser.layout.listings.contains_key(&t.path("Kicks")), "forgotten, so the next expand reads it fresh");
    // Collapse the location itself.
    t.click(mid(t.row(&root).rect.unwrap()));
    assert_eq!(t.app.browser.layout.rows.len(), 1);
    assert_eq!((t.undo_len(), t.app.engine.is_dirty()), (u0, dirty), "expanding and collapsing are view state");
}

#[test]
fn audio_rows_show_length_and_channels_read_from_the_header() {
    let mut t = T::new();
    t.add_and_expand();
    let r = t.row_named("loop 90.wav");
    assert_eq!(r.info.as_deref(), Some("2.0 s · stereo"), "{r:?}");
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    t.settle();
    assert_eq!(t.row_named("Kick 01.wav").info.as_deref(), Some("0.5 s · mono"));
    assert!(t.app.browser.layout.headers_read >= 3);
}

#[test]
fn parse_header_reads_wav_aiff_and_flac_lengths() {
    let w = parse_header(&wav(2, 44_100, 22_050)).unwrap();
    assert_eq!((w.channels, w.rate, w.secs), (2, 44_100, 0.5));
    // Only the header: the data chunk is cut off.
    let cut = wav(1, 48_000, 96_000);
    assert_eq!(parse_header(&cut[..200]).unwrap().secs, 2.0);
    // AIFF: COMM with 48 kHz as an 80-bit float.
    let mut a = b"FORM\0\0\0\x2eAIFFCOMM\0\0\0\x12".to_vec();
    a.extend_from_slice(&1u16.to_be_bytes());
    a.extend_from_slice(&24_000u32.to_be_bytes());
    a.extend_from_slice(&16u16.to_be_bytes());
    a.extend_from_slice(&[0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]);
    let i = parse_header(&a).unwrap();
    assert_eq!((i.channels, i.rate, i.secs), (1, 48_000, 0.5));
    // FLAC STREAMINFO: 44.1 kHz, 2 channels, 88 200 samples.
    let mut f = b"fLaC\0\0\0\x22".to_vec();
    let mut si = [0u8; 34];
    let rate = 44_100u32;
    si[10] = (rate >> 12) as u8;
    si[11] = (rate >> 4) as u8;
    si[12] = ((rate & 0xF) << 4) as u8 | (1 << 1);
    si[14..18].copy_from_slice(&88_200u32.to_be_bytes());
    f.extend_from_slice(&si);
    let i = parse_header(&f).unwrap();
    assert_eq!((i.channels, i.rate, i.secs), (2, 44_100, 2.0));
    assert!(parse_header(b"ID3 not a header").is_none());
}

#[test]
fn the_filter_shows_matches_and_the_folders_that_hold_them() {
    let mut t = T::new();
    t.add_and_expand();
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    t.settle();
    let u0 = t.undo_len();
    t.click(mid(t.app.browser.layout.filter));
    t.text("02");
    let names = t.names();
    assert!(names.contains(&"Kick 02.wav".to_string()) && names.contains(&"Kicks".to_string()), "{names:?}");
    assert!(!names.contains(&"Kick 01.wav".to_string()) && !names.contains(&"Snares".to_string()), "{names:?}");
    // A folder that matches shows what is expanded under it.
    t.app.browser.filter.clear();
    t.click(mid(t.app.browser.layout.filter));
    t.text("kicks");
    let names = t.names();
    assert!(names.contains(&"Kick 01.wav".to_string()) && !names.contains(&"loop 90.wav".to_string()), "{names:?}");
    assert_eq!(t.undo_len(), u0);
}

#[test]
fn click_selects_ctrl_click_adds_and_shift_click_ranges() {
    let mut t = T::new();
    t.add_and_expand();
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    t.settle();
    let k1 = t.row_named("Kick 01.wav").rect.unwrap();
    let k2 = t.row_named("Kick 02.wav").rect.unwrap();
    let lp = t.row_named("loop 90.wav").rect.unwrap();
    t.click(mid(k1));
    assert_eq!(t.app.browser.selected, vec![PathBuf::from(t.path("Kicks/Kick 01.wav"))]);
    t.click_n(mid(lp), 1, json!({"ctrl": true}));
    assert_eq!(t.app.browser.selected.len(), 2);
    t.click_n(mid(lp), 1, json!({"ctrl": true}));
    assert_eq!(t.app.browser.selected.len(), 1, "Ctrl-click again takes it out");
    t.click(mid(k1));
    t.click_n(mid(lp), 1, json!({"shift": true}));
    let sel: Vec<String> = t.app.browser.selected.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(sel, vec!["Kick 01.wav", "Kick 02.wav", "Kit.nki", "loop 90.wav"], "the files between, in display order");
    t.click(mid(k2));
    assert!(t.row_named("Kick 02.wav").selected && !t.row_named("Kick 01.wav").selected);
}

#[test]
fn dragging_a_wav_onto_an_audio_lane_at_bar_2_imports_it_there_in_one_undo_step() {
    let mut t = T::new();
    t.add_and_expand();
    let audio = t.track(TrackKind::Audio);
    let u0 = t.undo_len();
    let from = mid(t.row_named("loop 90.wav").rect.unwrap());
    let lane = t.lane(audio);
    // A little after bar 2: snapped to the beat.
    let to = (t.x_of(t.bar(2)) + 2.0, (lane[1] + lane[3]) / 2.0);
    t.drag(from, to, json!({}));
    t.settle_imports();
    let clips = t.clips(audio);
    assert_eq!(clips, vec![(t.bar(2), Samples::from(t.rate * 2), "loop 90".to_string())], "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 1, "one undo step");
    assert!(t.app.status.contains("Imported loop 90.wav to") && t.app.status.ends_with("at 2.1.1"), "{}", t.app.status);
    assert!(t.app.browser.drag.is_none());
    t.app.run("edit.undo", json!({})).unwrap();
    assert!(t.clips(audio).is_empty(), "undo takes the whole drop back");
}

#[test]
fn shift_turns_the_beat_snap_off() {
    let mut t = T::new();
    t.add_and_expand();
    let audio = t.track(TrackKind::Audio);
    let from = mid(t.row_named("loop 90.wav").rect.unwrap());
    let lane = t.lane(audio);
    let to = (t.x_of(t.bar(2)) + 9.0, (lane[1] + lane[3]) / 2.0);
    t.drag(from, to, json!({"shift": true}));
    t.settle_imports();
    let start = t.clips(audio)[0].0;
    assert!(start > t.bar(2), "unsnapped: {start}");
}

#[test]
fn dragging_below_the_tracks_makes_a_new_audio_track() {
    let mut t = T::new();
    t.add_and_expand();
    let n = t.app.engine.session().tracks.len();
    let u0 = t.undo_len();
    let from = mid(t.row_named("loop 90.wav").rect.unwrap());
    let last = t.app.arrange.layout.tracks.last().unwrap().lane;
    t.drag(from, (t.x_of(t.bar(3)), last[3] + 30.0), json!({}));
    t.settle_imports();
    let s = t.app.engine.session();
    assert_eq!(s.tracks.len(), n + 1, "{}", t.app.status);
    let new = s.tracks.iter().find(|tr| tr.name.starts_with("loop 90")).expect("the new track");
    assert_eq!(new.kind, TrackKind::Audio);
    assert_eq!(new.clips()[0].start, t.bar(3));
    assert_eq!(t.undo_len(), u0 + 1);
}

#[test]
fn three_selected_files_go_one_after_another_on_one_track_one_undo_step_each() {
    let mut t = T::new();
    t.add_and_expand();
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    t.settle();
    t.click(mid(t.row(&t.path("Snares")).rect.unwrap()));
    t.settle();
    let k1 = t.row_named("Kick 01.wav").rect.unwrap();
    let k2 = t.row_named("Kick 02.wav").rect.unwrap();
    let s1 = t.row_named("Snare 01.wav").rect.unwrap();
    t.click(mid(k1));
    t.click_n(mid(k2), 1, json!({"ctrl": true}));
    t.click_n(mid(s1), 1, json!({"ctrl": true}));
    let audio = t.track(TrackKind::Audio);
    let u0 = t.undo_len();
    let lane = t.lane(audio);
    t.drag(mid(k2), (t.x_of(t.bar(1)) + 1.0, (lane[1] + lane[3]) / 2.0), json!({}));
    t.settle_imports();
    let half = Samples::from(t.rate / 2);
    let q = Samples::from(t.rate / 4);
    let clips = t.clips(audio);
    assert_eq!(
        clips,
        vec![(0, half, "Kick 01".to_string()), (half, q, "Kick 02".to_string()), (half + q, q, "Snare 01".to_string())],
        "{}",
        t.app.status
    );
    assert_eq!(t.undo_len(), u0 + 3, "one undo step per file");
    assert!(t.app.status.starts_with("Imported 3 files"), "{}", t.app.status);
}

#[test]
fn a_double_click_imports_at_the_cursor_on_the_selected_track() {
    let mut t = T::new();
    t.add_and_expand();
    let audio = t.track(TrackKind::Audio);
    // Click the audio lane at bar 3: the cursor and the selected track.
    let lane = t.lane(audio);
    t.click((t.x_of(t.bar(3)), (lane[1] + lane[3]) / 2.0));
    let cursor = t.app.cursor();
    let u0 = t.undo_len();
    t.double_click(mid(t.row_named("loop 90.wav").rect.unwrap()));
    t.settle_imports();
    assert_eq!(t.clips(audio), vec![(cursor, Samples::from(t.rate * 2), "loop 90".to_string())], "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 1);
}

#[test]
fn a_kontakt_file_is_refused_with_a_status_and_no_change() {
    let mut t = T::new();
    t.add_and_expand();
    let audio = t.track(TrackKind::Audio);
    let u0 = t.undo_len();
    let lane = t.lane(audio);
    t.drag(mid(t.row_named("Kit.nki").rect.unwrap()), (t.x_of(t.bar(2)), (lane[1] + lane[3]) / 2.0), json!({}));
    assert!(t.app.status.contains("not supported"), "{}", t.app.status);
    assert_eq!(t.undo_len(), u0);
    assert!(t.clips(audio).is_empty());
}

#[test]
fn audio_dropped_on_an_instrument_track_is_refused() {
    let mut t = T::new();
    t.add_and_expand();
    let inst = t.track(TrackKind::Instrument);
    let u0 = t.undo_len();
    let lane = t.lane(inst);
    t.drag(mid(t.row_named("loop 90.wav").rect.unwrap()), (t.x_of(t.bar(2)), (lane[1] + lane[3]) / 2.0), json!({}));
    assert!(t.app.status.contains("not an audio track"), "{}", t.app.status);
    assert_eq!(t.undo_len(), u0);
}

#[test]
fn a_drag_released_over_the_browser_does_nothing() {
    let mut t = T::new();
    t.add_and_expand();
    let u0 = t.undo_len();
    let from = mid(t.row_named("loop 90.wav").rect.unwrap());
    t.drag(from, (from.0, from.1 + 60.0), json!({}));
    assert_eq!(t.undo_len(), u0);
    assert!(t.app.browser.drag.is_none());
}

#[test]
fn the_wheel_scrolls_the_tree() {
    let mut t = T::new();
    for i in 0..60 {
        t.dir.file(&format!("many/s{i:02}.wav"), &wav(1, t.rate, 10));
    }
    t.add_and_expand();
    t.click(mid(t.row(&t.path("many")).rect.unwrap()));
    t.settle();
    let first = t.row_named("s00.wav").rect.unwrap()[1];
    let list = t.app.browser.layout.list;
    t.input("ui.scroll", json!({"x": (list[0] + list[2]) / 2.0, "y": (list[1] + list[3]) / 2.0, "dy": -200.0}));
    let after = t.row_named("s00.wav").rect.map_or(f32::NEG_INFINITY, |r| r[1]);
    assert!(after < first - 100.0, "{first} -> {after}");
}

#[test]
fn the_session_audio_files_folder_is_a_location_once_saved() {
    let mut t = T::new();
    assert!(!t.names().iter().any(|n| n.starts_with("Audio Files")));
    let path = t.dir.0.join("Song.transit");
    t.app.run("session.save_as", json!({"path": path.to_string_lossy(), "replace": true})).unwrap();
    t.frames(3);
    let row = t.row_named("Audio Files (this session)");
    assert!(row.root && row.remove.is_none(), "not removable");
    assert_eq!(Path::new(&row.path), t.dir.0.join("Audio Files"));
}

/// Built-in instrument and effect names from `engine.plugins`: an instrument the test's
/// instrument track does not already play.
fn builtins(t: &mut T) -> (String, String) {
    let b = t.app.builtin_plugins();
    let inst = t.track(TrackKind::Instrument);
    let current = t.app.engine.session().track(inst).and_then(|tr| tr.instrument.as_ref()).map(|i| i.plugin.clone());
    let i = b.iter().find(|p| p.instrument && Some(&p.id) != current.as_ref()).unwrap();
    (i.name.clone(), b.iter().find(|p| !p.instrument).unwrap().name.clone())
}

#[test]
fn an_instrument_dragged_onto_an_instrument_track_loads_it() {
    let mut t = T::new();
    let (inst_name, _) = builtins(&mut t);
    t.click(mid(t.app.browser.layout.tab_plugins));
    let inst = t.track(TrackKind::Instrument);
    let u0 = t.undo_len();
    let row = t.plugin(&inst_name);
    assert!(row.instrument && row.format == "Built-in");
    let lane = t.lane(inst);
    t.drag(mid(row.rect.unwrap()), (t.x_of(t.bar(2)), (lane[1] + lane[3]) / 2.0), json!({}));
    let got = t.app.engine.session().track(inst).unwrap().instrument.as_ref().map(|i| i.plugin.clone());
    assert_eq!(got, Some(row.id.clone()), "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 1);
    assert!(t.app.status.starts_with("Loaded"), "{}", t.app.status);
    // An instrument onto an audio track is refused.
    let audio = t.track(TrackKind::Audio);
    let lane = t.lane(audio);
    t.drag(mid(t.plugin(&inst_name).rect.unwrap()), (t.x_of(t.bar(2)), (lane[1] + lane[3]) / 2.0), json!({}));
    assert!(t.app.status.contains("is an instrument"), "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 1);
}

#[test]
fn an_instrument_dragged_below_the_tracks_makes_an_instrument_track_in_one_undo_step() {
    let mut t = T::new();
    let (inst_name, _) = builtins(&mut t);
    t.click(mid(t.app.browser.layout.tab_plugins));
    let n = t.app.engine.session().tracks.len();
    let u0 = t.undo_len();
    let last = t.app.arrange.layout.tracks.last().unwrap().lane;
    let row = t.plugin(&inst_name);
    t.drag(mid(row.rect.unwrap()), (t.x_of(t.bar(2)), last[3] + 30.0), json!({}));
    let s = t.app.engine.session();
    assert_eq!(s.tracks.len(), n + 1, "{}", t.app.status);
    let new = s.tracks.last().unwrap();
    assert_eq!(new.kind, TrackKind::Instrument);
    assert_eq!(new.instrument.as_ref().map(|i| i.plugin.clone()), Some(row.id));
    assert_eq!(t.undo_len(), u0 + 1);
}

#[test]
fn an_effect_dragged_onto_a_mixer_strip_or_a_lane_is_inserted() {
    let mut t = T::new();
    let (_, fx_name) = builtins(&mut t);
    t.app.ui.show_mixer = true;
    t.frames(3);
    t.click(mid(t.app.browser.layout.tab_plugins));
    let audio = t.track(TrackKind::Audio);
    let u0 = t.undo_len();
    let strip = t.app.mixer.layout.strips.iter().find(|s| s.track == audio.0).unwrap().rect;
    let row = t.plugin(&fx_name);
    assert!(!row.instrument);
    t.drag(mid(row.rect.unwrap()), mid(strip), json!({}));
    let ins = |t: &T| t.app.engine.session().track(audio).unwrap().mixer.inserts.iter().flatten().map(|i| i.plugin.clone()).collect::<Vec<_>>();
    assert_eq!(ins(&t), vec![row.id.clone()], "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 1);
    // Onto the lane: the end of the chain.
    let lane = t.lane(audio);
    t.drag(mid(t.plugin(&fx_name).rect.unwrap()), (t.x_of(t.bar(2)), (lane[1] + lane[3]) / 2.0), json!({}));
    assert_eq!(ins(&t).len(), 2);
    assert_eq!(t.undo_len(), u0 + 2);
    // Below the tracks: refused.
    t.app.ui.show_mixer = false;
    t.frames(3);
    let last = t.app.arrange.layout.tracks.last().unwrap().lane;
    t.drag(mid(t.plugin(&fx_name).rect.unwrap()), (t.x_of(t.bar(2)), last[3] + 30.0), json!({}));
    assert!(t.app.status.contains("Drop"), "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 2);
}

#[test]
fn a_double_click_on_a_plugin_acts_on_the_focused_track() {
    let mut t = T::new();
    let (_, fx_name) = builtins(&mut t);
    t.click(mid(t.app.browser.layout.tab_plugins));
    let audio = t.track(TrackKind::Audio);
    // Focus the audio track by clicking its header.
    let hdr = t.app.arrange.layout.tracks.iter().find(|r| r.id == audio.0).unwrap().header;
    t.click((hdr[0] + 30.0, (hdr[1] + hdr[3]) / 2.0));
    let u0 = t.undo_len();
    t.double_click(mid(t.plugin(&fx_name).rect.unwrap()));
    assert_eq!(t.app.engine.session().track(audio).unwrap().mixer.inserts.iter().flatten().count(), 1, "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 1);
    assert!(t.plugin(&fx_name).selected);
}

#[test]
fn the_star_keeps_favorites_in_ui_state_and_the_filter_narrows_the_list() {
    let mut t = T::new();
    let (inst_name, fx_name) = builtins(&mut t);
    t.click(mid(t.app.browser.layout.tab_plugins));
    let u0 = t.undo_len();
    let dirty = t.app.engine.is_dirty();
    let row = t.plugin(&fx_name);
    t.click(mid(row.star.unwrap()));
    assert_eq!(t.app.ui.browser.favorites, vec![row.id.clone()]);
    // After the scan note: Favorites first.
    let l: Vec<_> = t.app.browser.layout.plugins.iter().filter(|p| p.row != "note").cloned().collect();
    assert_eq!((l[0].row, l[0].name.as_str()), ("heading", "Favorites"), "favorites first");
    assert_eq!(l[1].name, fx_name);
    assert!(l[1].favorite);
    // Unstar from the Favorites row.
    t.click(mid(l[1].star.unwrap()));
    assert!(t.app.ui.browser.favorites.is_empty());
    // Filter.
    t.click(mid(t.app.browser.layout.filter));
    t.text(&inst_name);
    let names: Vec<String> = t.app.browser.layout.plugins.iter().filter(|p| p.row == "plugin").map(|p| p.name.clone()).collect();
    assert!(names.contains(&inst_name) && !names.contains(&fx_name), "{names:?}");
    assert_eq!((t.undo_len(), t.app.engine.is_dirty()), (u0, dirty), "view state only");
    assert_eq!(serde_json::to_value(&t.app.ui).unwrap()["browser"]["favorites"], json!([]));
}

#[test]
fn a_pending_listing_dies_with_its_location_and_never_spins_the_frame_loop() {
    let mut t = T::new();
    t.add_and_expand();
    let root = t.dir.s();
    // A listing that never answers (a dead network share).
    let dead = t.dir.0.join("Dead");
    t.app.browser.listings.insert(dead.clone(), crate::browser::Listing::Pending { since: std::time::Instant::now() });
    t.app.browser.expanded.insert(dead);
    t.frames(3);
    let l = &t.app.browser.layout;
    assert_eq!(l.pending, 1);
    assert!(!l.per_frame_repaint, "polls, does not repaint every frame");
    assert_eq!(l.repaint_after_ms, Some(100));
    // Hide the panel: still no per-frame repaint.
    t.app.ui.browser.show = false;
    t.frames(3);
    assert!(!t.app.browser.layout.per_frame_repaint);
    t.app.ui.browser.show = true;
    t.frames(3);
    let c = mid(t.row(&root).rect.unwrap());
    t.input("ui.move", json!({"x": c.0, "y": c.1}));
    t.click(mid(t.row(&root).remove.unwrap()));
    assert_eq!(t.app.browser.layout.pending, 0);
    assert_eq!(t.app.browser.layout.repaint_after_ms, None);
    assert!(t.app.browser.listings.is_empty() && t.app.browser.expanded.is_empty());
}

#[test]
fn collapse_and_remove_forget_the_subtree_so_nothing_stale_shows_again() {
    let mut t = T::new();
    t.add_and_expand();
    let root = t.dir.s();
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    t.settle();
    assert!(t.names().contains(&"Kick 02.wav".to_string()));
    // Collapse the location: the nested Kicks listing and expansion go with it.
    t.click(mid(t.row(&root).rect.unwrap()));
    assert!(t.app.browser.listings.is_empty(), "{:?}", t.app.browser.listings.keys());
    assert!(t.app.browser.expanded.is_empty());
    // A file added meanwhile shows on the next expand (no stale listing).
    t.dir.file("Kicks/Kick 03.wav", &wav(1, t.rate, 100));
    t.click(mid(t.row(&root).rect.unwrap()));
    t.settle();
    t.click(mid(t.row(&t.path("Kicks")).rect.unwrap()));
    t.settle();
    assert!(t.names().contains(&"Kick 03.wav".to_string()), "{:?}", t.names());
    // Remove the location and add it back: a fresh read, not the old listing.
    let c = mid(t.row(&root).rect.unwrap());
    t.input("ui.move", json!({"x": c.0, "y": c.1}));
    t.click(mid(t.row(&root).remove.unwrap()));
    assert!(t.app.browser.listings.is_empty() && t.app.browser.expanded.is_empty());
    assert!(t.app.add_location(&root));
    t.frames(3);
    assert_eq!(t.app.browser.layout.rows.len(), 1, "collapsed, with nothing cached");
}

#[test]
fn a_drop_on_no_target_says_where_drops_go_and_escape_cancels_a_drag() {
    let mut t = T::new();
    t.add_and_expand();
    let u0 = t.undo_len();
    let from = mid(t.row_named("loop 90.wav").rect.unwrap());
    // Released over the transport: no target.
    t.drag(from, (700.0, 8.0), json!({}));
    assert_eq!(t.app.status, "Drop onto a track lane, header, strip or the device chain");
    assert_eq!(t.undo_len(), u0);
    // Escape mid-drag: no action, no message.
    t.app.status.clear();
    let l = t.lane(t.track(TrackKind::Audio));
    let over = mid(l);
    let mut ev = |e| t.app.synthetic.push(e);
    ev(egui::Event::PointerMoved(egui::pos2(from.0, from.1)));
    ev(egui::Event::PointerButton {
        pos: egui::pos2(from.0, from.1),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    for k in 1..=10 {
        let f = k as f32 / 10.0;
        ev(egui::Event::PointerMoved(egui::pos2(from.0 + (over.0 - from.0) * f, from.1 + (over.1 - from.1) * f)));
    }
    t.frames(5);
    assert!(t.app.browser.drag.is_some() && t.app.browser.layout.drop.is_some());
    t.key("escape", json!({}));
    assert!(t.app.browser.drag.is_none());
    let up = egui::pos2(over.0, over.1);
    t.app.synthetic.push(egui::Event::PointerButton { pos: up, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    t.frames(5);
    assert_eq!(t.undo_len(), u0, "nothing imported");
    assert!(t.app.status.is_empty(), "{}", t.app.status);
}

/// A WAV with `pad` bytes of LIST chunk before `fmt `.
fn wav_with_list(pad: usize) -> Vec<u8> {
    let plain = wav(2, 48_000, 24_000);
    let mut b = plain[..12].to_vec();
    b.extend_from_slice(b"LIST");
    b.extend_from_slice(&(pad as u32).to_le_bytes());
    b.extend(std::iter::repeat_n(7u8, pad));
    b.extend_from_slice(&plain[12..]);
    b
}

#[test]
fn a_wav_with_a_200_kb_chunk_before_fmt_still_shows_its_length() {
    let mut t = T::new();
    t.dir.file("Big Header.wav", &wav_with_list(200 * 1024));
    t.dir.file("Huge Header.wav", &wav_with_list(5 * 1024 * 1024));
    t.dir.file("Cut.wav", &wav_with_list(200 * 1024)[..1000]);
    t.add_and_expand();
    assert_eq!(t.row_named("Big Header.wav").info.as_deref(), Some("0.5 s · stereo"));
    assert_eq!(t.row_named("Huge Header.wav").info.as_deref(), Some("–"), "beyond the 4 MB walk bound");
    assert_eq!(t.row_named("Cut.wav").info.as_deref(), Some("–"), "truncated files are safe");
    assert_eq!(t.row_named("loop 90.wav").info.as_deref(), Some("2.0 s · stereo"));
}

#[test]
fn a_long_name_is_elided_with_the_extension_kept() {
    let mut t = T::new();
    t.dir.file("Kick Automation Take Seven Final Mix.wav", &wav(1, t.rate, 100));
    t.add_and_expand();
    let long = t.row_named("Kick Automation Take Seven Final Mix.wav");
    assert!(long.shown.contains('…') && long.shown.ends_with(".wav") && long.shown.len() < long.name.len(), "{}", long.shown);
    assert!(long.shown.starts_with("Kick"), "{}", long.shown);
    let short = t.row_named("loop 90.wav");
    assert_eq!(short.shown, "loop 90.wav");
}

#[test]
fn importing_announces_itself_and_returns_before_the_file_is_placed() {
    let mut t = T::new();
    t.add_and_expand();
    let track = t.track(TrackKind::Audio);
    t.app.import_files(&[t.dir.0.join("loop 90.wav")], Some(track), 0).unwrap();
    assert_eq!(t.app.browser.import_notice, "Importing loop 90.wav…");
    assert!(t.clips(track).is_empty(), "the call only queues the file");
    t.settle_imports();
    assert!(t.app.status.starts_with("Imported loop 90.wav"), "{}", t.app.status);
    assert_eq!(t.clips(track).len(), 1);
}

#[test]
fn rows_show_their_type_icons_and_a_click_on_the_chevron_row_expands() {
    let mut t = T::new();
    t.add_and_expand();
    let kicks = t.row(&t.path("Kicks"));
    assert_eq!(kicks.icon, "folder");
    t.click(mid(kicks.rect.unwrap()));
    t.settle();
    assert_eq!(t.row(&t.path("Kicks")).icon, "folder-open", "an expanded folder shows the open folder");
    assert_eq!(t.row_named("Kick 01.wav").icon, "file-audio");
    assert_eq!(t.row_named("Kit.nki").icon, "file");
    // The name starts after the icon, still elided to fit.
    assert_eq!(t.row_named("loop 90.wav").shown, "loop 90.wav");
}

// ---- Asynchronous import (L30). `imports.pace` is the fake prepare: progress over N frames. ----

/// Drag `name` onto `track`'s lane a little after bar 2 (snapped to bar 2).
fn drop_on_lane(t: &mut T, name: &str, track: TrackId) {
    let from = mid(t.row_named(name).rect.unwrap());
    let lane = t.lane(track);
    let to = (t.x_of(t.bar(2)) + 2.0, (lane[1] + lane[3]) / 2.0);
    t.drag(from, to, json!({}));
}

#[test]
fn a_drop_shows_an_outline_and_progress_then_commits_at_the_drop_in_one_undo_step() {
    let mut t = T::new();
    t.add_and_expand();
    t.app.imports.pace = Some(120);
    let audio = t.track(TrackKind::Audio);
    let u0 = t.undo_len();
    drop_on_lane(&mut t, "loop 90.wav", audio);
    assert!(t.app.imports.active(), "{}", t.app.status);
    assert!(t.clips(audio).is_empty(), "nothing placed while preparing");
    assert_eq!(t.undo_len(), u0);
    let pct = t.app.status.strip_prefix("Importing loop 90.wav… ").and_then(|r| r.strip_suffix('%')).and_then(|n| n.parse::<u32>().ok());
    assert!(pct.is_some_and(|p| (1..100).contains(&p)), "{}", t.app.status);
    let item = t.app.imports.layout.items[0].clone();
    let (r, lane) = (item.rect.expect("an outline on screen"), t.lane(audio));
    assert_eq!(item.at, t.bar(2));
    assert!((r[0] - t.x_of(t.bar(2))).abs() < 1.5 && r[1] >= lane[1] && r[3] <= lane[3], "{r:?} on lane {lane:?}");
    assert!((r[2] - t.x_of(t.bar(2) + Samples::from(t.rate * 2))).abs() < 1.5, "as long as the header says: {r:?}");
    assert!(item.cancel.is_some());
    // The UI keeps running while the file is prepared.
    t.frames(12);
    assert!(t.app.imports.layout.max_step_ms < 40.0, "{}", t.app.imports.layout.max_step_ms);
    t.settle_imports();
    assert_eq!(t.clips(audio), vec![(t.bar(2), Samples::from(t.rate * 2), "loop 90".to_string())], "{}", t.app.status);
    assert_eq!(t.undo_len(), u0 + 1);
    assert_eq!(t.app.engine.undo_label(), Some("Import loop 90"));
    assert!(t.app.status.starts_with("Imported loop 90.wav to") && t.app.status.ends_with("at 2.1.1"), "{}", t.app.status);
    assert!(t.app.imports.layout.items.is_empty() && t.app.engine.imports.is_empty());
}

#[test]
fn escape_cancels_the_imports_in_flight_and_changes_nothing() {
    let mut t = T::new();
    t.add_and_expand();
    t.app.imports.pace = Some(1_000_000);
    let audio = t.track(TrackKind::Audio);
    let (u0, rev) = (t.undo_len(), t.app.engine.revision);
    drop_on_lane(&mut t, "loop 90.wav", audio);
    assert!(t.app.imports.active());
    t.key("escape", json!({}));
    assert!(!t.app.imports.active());
    assert_eq!(t.app.status, "Canceled the import of loop 90.wav");
    assert!(t.app.engine.imports.is_empty(), "the staging item is dropped");
    assert!(t.clips(audio).is_empty());
    assert_eq!((t.undo_len(), t.app.engine.revision), (u0, rev));
    assert!(t.app.engine.session().sources.is_empty());
}

#[test]
fn the_x_on_an_outline_cancels_that_file() {
    let mut t = T::new();
    t.add_and_expand();
    t.app.imports.pace = Some(1_000_000);
    let audio = t.track(TrackKind::Audio);
    let u0 = t.undo_len();
    drop_on_lane(&mut t, "loop 90.wav", audio);
    let x = t.app.imports.layout.items[0].cancel.expect("an x");
    let cursor = t.app.cursor();
    t.click(mid(x));
    assert!(!t.app.imports.active(), "{}", t.app.status);
    assert_eq!(t.app.status, "Canceled the import of loop 90.wav");
    assert_eq!(t.app.cursor(), cursor, "the click went to the x, not the lane");
    assert!(t.clips(audio).is_empty() && t.app.engine.imports.is_empty());
    assert_eq!(t.undo_len(), u0);
}

#[test]
fn an_import_whose_track_was_deleted_meanwhile_is_refused() {
    let mut t = T::new();
    t.add_and_expand();
    t.app.imports.pace = Some(1_000_000);
    let audio = t.track(TrackKind::Audio);
    drop_on_lane(&mut t, "loop 90.wav", audio);
    t.app.run("track.delete", json!({"tracks": [audio.0]})).unwrap();
    let u0 = t.undo_len();
    t.app.imports.pace = None;
    t.settle_imports();
    assert_eq!(t.app.status, "Couldn't import loop 90.wav: its track was deleted");
    assert!(t.app.engine.imports.is_empty(), "the staging item is dropped");
    assert!(t.app.engine.session().sources.is_empty());
    assert_eq!(t.undo_len(), u0);
}

#[test]
fn a_new_session_while_importing_stops_the_import() {
    let mut t = T::new();
    t.add_and_expand();
    t.app.imports.pace = Some(1_000_000);
    let audio = t.track(TrackKind::Audio);
    drop_on_lane(&mut t, "loop 90.wav", audio);
    t.app.engine.execute("session.new", &json!({})).unwrap();
    t.frames(12);
    assert!(!t.app.imports.active());
    assert_eq!(t.app.status, "The import of loop 90.wav stopped: the session changed");
}

#[test]
fn the_x_of_a_clip_longer_than_the_view_stays_inside_the_viewport() {
    let mut t = T::new();
    t.add_and_expand();
    t.app.imports.pace = Some(1_000_000);
    let audio = t.track(TrackKind::Audio);
    drop_on_lane(&mut t, "loop 90.wav", audio);
    let tl = t.app.arrange.layout.timeline;
    // A 19-minute file: the engine's own length report would replace this, so retry across a poll.
    let mut x = None;
    for _ in 0..8 {
        t.app.imports.items[0].frames = Samples::from(t.rate) * 60 * 19;
        t.frames(1);
        let item = &t.app.imports.layout.items[0];
        if item.rect.is_some_and(|r| r[2] > tl[2] + 1_000.0) {
            x = item.cancel;
            break;
        }
    }
    let x = x.expect("an x on a long outline");
    assert!(x[0] >= tl[0] && x[2] <= tl[2] && x[1] >= tl[1] && x[3] <= tl[3], "x {x:?} inside the timeline {tl:?}");
}

#[test]
fn pinned_keeps_the_label_and_x_on_the_visible_part() {
    let tl = egui::Rect::from_min_max(egui::pos2(100.0, 50.0), egui::pos2(900.0, 400.0));
    for r in [
        egui::Rect::from_min_max(egui::pos2(300.0, 60.0), egui::pos2(53_707.0, 100.0)),
        egui::Rect::from_min_max(egui::pos2(-5_000.0, 60.0), egui::pos2(53_707.0, 100.0)),
        egui::Rect::from_min_max(egui::pos2(300.0, 60.0), egui::pos2(324.0, 100.0)),
        egui::Rect::from_min_max(egui::pos2(-5_000.0, 60.0), egui::pos2(104.0, 100.0)),
        egui::Rect::from_min_max(egui::pos2(895.0, 40.0), egui::pos2(53_707.0, 100.0)),
    ] {
        let (text, x) = crate::import::pinned(r, tl);
        assert!(tl.contains_rect(x), "{x:?} in {tl:?}");
        assert!(text.x >= tl.min.x, "{text:?}");
    }
}

#[test]
fn files_the_engine_would_not_prepare_are_named_in_the_status() {
    let mut t = T::new();
    t.add_and_expand();
    let audio = t.track(TrackKind::Audio);
    let ok = t.dir.0.join("loop 90.wav");
    let outside = std::env::temp_dir().join(format!("transit-outside-{}", std::process::id())).join("elsewhere.wav");
    let roots = soundcraft_engine::file_access::AutomationRoots::new(Some(&t.dir.0), None).unwrap();
    let files = vec![ok, outside];
    let app = &mut t.app;
    soundcraft_engine::file_access::confine(roots.as_ref(), || app.queue_imports(&files, Some(audio), 0, 0)).unwrap();
    assert!(t.app.status.starts_with("1 of 2 could not be prepared: elsewhere.wav ("), "{}", t.app.status);
    t.settle_imports();
    assert!(
        t.app.status.starts_with("Imported loop 90.wav to") && t.app.status.contains("; 1 of 2 could not be prepared: elsewhere.wav ("),
        "{}",
        t.app.status
    );
}

#[test]
fn many_refused_files_show_three_names_the_first_reason_and_a_count() {
    let refused: Vec<(String, String)> = (1..=7).map(|i| (format!("f{i}.wav"), format!("why {i}"))).collect();
    assert_eq!(crate::import::not_prepared(&refused, 9), "7 of 9 could not be prepared: f1.wav (why 1), f2.wav, f3.wav and 4 more");
    assert_eq!(crate::import::not_prepared(&refused[..2], 2), "2 of 2 could not be prepared: f1.wav (why 1), f2.wav");
}

#[test]
fn imports_pace_is_bounded_and_a_rejected_ui_set_leaves_it_alone() {
    let mut t = T::new();
    assert!(t.app.set_ui(&json!({"imports_pace": 90})).is_ok());
    assert_eq!(t.app.imports.pace, Some(90));
    assert!(t.app.set_ui(&json!({"imports_pace": 601})).is_err());
    assert!(t.app.set_ui(&json!({"imports_pace": u32::MAX})).is_err());
    assert!(t.app.set_ui(&json!({"imports_pace": -1})).is_err());
    assert_eq!(t.app.imports.pace, Some(90), "a rejected value changes nothing");
    assert!(t.app.set_ui(&json!({"imports_pace": 5, "plugin_dirs": ["Z:/elsewhere"]})).is_err());
    assert_eq!(t.app.imports.pace, Some(90), "a rejected ui.set changes nothing");
    assert!(t.app.set_ui(&json!({"imports_pace": null})).is_ok());
    assert_eq!(t.app.imports.pace, None);
}

#[test]
fn an_import_the_engine_let_expire_says_so_and_is_removed() {
    let mut t = T::new();
    t.add_and_expand();
    t.app.imports.pace = Some(1_000_000);
    let audio = t.track(TrackKind::Audio);
    drop_on_lane(&mut t, "loop 90.wav", audio);
    t.app.engine.imports.set_limits(std::time::Duration::ZERO, 32, std::time::Duration::from_secs(30));
    for _ in 0..40 {
        t.frames(6);
        std::thread::sleep(std::time::Duration::from_millis(2));
        if !t.app.imports.active() {
            break;
        }
    }
    assert!(!t.app.imports.active(), "{}", t.app.status);
    assert_eq!(t.app.status, "The import of loop 90.wav expired: re-import");
    assert!(t.app.engine.imports.is_empty(), "the staging entry is canceled");
}
