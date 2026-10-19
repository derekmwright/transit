//! Slice 5 tests (icons, theme, shortcut map). Driven like a user: synthetic clicks and keys
//! through the control channel's input queue, one frame at a time. No player, no files.

use crate::{Services, StudioApp};
use serde_json::{Value, json};

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

impl T {
    fn new() -> T {
        let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        app.run("track.new", json!({"kind": "instrument"})).unwrap();
        let mut t = T { app, ctx: egui::Context::default(), time: 0.0 };
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
        self.input("ui.click", json!({"x": at.0, "y": at.1}));
    }

    fn inspect(&self) -> Value {
        self.app.inspect(&self.ctx)
    }
}

#[test]
fn transport_buttons_show_lucide_icons_and_still_work() {
    let mut t = T::new();
    let icons = t.inspect()["layout"]["transport_icons"].clone();
    assert_eq!(icons, json!({"rtz": "skip-back", "stop": "square", "play": "play", "record": "circle", "cycle": "repeat"}));
    let play = t.app.arrange.layout.transport["play"];
    t.click(mid(play));
    assert!(t.app.is_playing(), "the Play icon button plays");
    let stop = t.app.arrange.layout.transport["stop"];
    t.click(mid(stop));
    assert!(!t.app.is_playing());
    let cycle = t.app.arrange.layout.transport["cycle"];
    t.click(mid(cycle));
    assert!(t.app.engine.session().edit.loop_playback, "the Loop icon button turns cycle on");
}

#[test]
fn icons_are_rasterized_once_not_every_frame() {
    let mut t = T::new();
    let first = crate::icons::rasterized(&t.ctx);
    assert!(first >= 5, "the transport's five icons at least: {first}");
    t.frames(30);
    assert_eq!(crate::icons::rasterized(&t.ctx), first, "no per-frame parsing");
}

/// The View menu's entry `label` (open the View menu first).
fn view_item(t: &T, label: &str) -> [f32; 4] {
    t.app.arrange.bar_menu.iter().find(|m| m.label == label).unwrap_or_else(|| panic!("no {label}: {:?}", t.app.arrange.bar_menu)).rect
}

/// View > Appearance > `label`, by clicks.
fn pick_appearance(t: &mut T, label: &str) {
    t.click(mid(view_item(t, "View")));
    t.click(mid(view_item(t, "Appearance")));
    t.click(mid(view_item(t, label)));
}

#[test]
fn view_appearance_switches_the_theme_by_clicks_and_never_dirties() {
    let mut t = T::new();
    assert_eq!(t.app.ui.appearance, crate::theme::Appearance::System);
    assert_eq!(t.inspect()["theme"], "dark", "no OS theme in a test: egui falls back to dark");
    let dirty = t.app.engine.is_dirty();
    let rev = t.app.engine.revision;
    pick_appearance(&mut t, "Light");
    assert_eq!(t.app.ui.appearance, crate::theme::Appearance::Light);
    assert_eq!(t.inspect()["theme"], "light");
    assert_eq!(t.inspect()["ui"]["appearance"], "light", "kept in UiState");
    assert_eq!(t.ctx.theme(), egui::Theme::Light, "egui's own widgets follow");
    assert_eq!(t.ctx.global_style().visuals.panel_fill, crate::theme::LIGHT.panel);
    pick_appearance(&mut t, "Dark");
    assert_eq!(t.inspect()["theme"], "dark");
    assert_eq!(t.ctx.global_style().visuals.panel_fill, crate::theme::DARK.panel);
    assert_eq!((t.app.engine.is_dirty(), t.app.engine.revision), (dirty, rev), "appearance is view state, not the document");
}

#[test]
fn system_appearance_follows_the_os_theme() {
    let mut t = T::new();
    t.ctx.options_mut(|o| o.fallback_theme = egui::Theme::Light);
    t.frames(2);
    assert_eq!(t.inspect()["theme"], "light", "System follows what egui resolves for the OS");
    pick_appearance(&mut t, "Dark");
    assert_eq!(t.inspect()["theme"], "dark", "a pinned theme ignores the OS");
}

#[test]
fn appearance_round_trips_through_ui_state_json() {
    let ui = crate::UiState { appearance: crate::theme::Appearance::Light, ..Default::default() };
    let back: crate::UiState = serde_json::from_value(serde_json::to_value(&ui).unwrap()).unwrap();
    assert_eq!(back.appearance, crate::theme::Appearance::Light);
    let old: crate::UiState = serde_json::from_value(json!({"track_height": 50.0})).unwrap();
    assert_eq!(old.appearance, crate::theme::Appearance::System, "an older ui.json follows the OS");
}

#[test]
fn both_themes_keep_text_meters_and_accent_legible() {
    use crate::theme::{DARK, LIGHT, Tokens, contrast};
    for (name, t) in [("dark", &DARK), ("light", &LIGHT)] {
        let t: &Tokens = t;
        for (bg_name, bg) in
            [("bg", t.bg), ("panel", t.panel), ("raised", t.panel_raised), ("header", t.header), ("lane", t.lane), ("button", t.button)]
        {
            assert!(contrast(t.text, bg) >= 7.0, "{name}: text on {bg_name}");
            assert!(contrast(t.text_dim, bg) >= 4.5, "{name}: dim text on {bg_name}: {}", contrast(t.text_dim, bg));
        }
        assert!(contrast(t.text, t.header_selected) >= 7.0, "{name}: text on a selected header");
        assert!(contrast(t.text_dim, t.header_selected) >= 4.5, "{name}: dim text on a selected header: {}", contrast(t.text_dim, t.header_selected));
        assert!(contrast(t.text, t.button_hover) >= 7.0, "{name}: text on a hovered button");
        assert!(contrast(t.text_dim, t.button_hover) >= 4.5, "{name}: dim text on a hovered button: {}", contrast(t.text_dim, t.button_hover));
        // The track menu's color swatches are UI components on the popup's panel: each is either
        // 3:1 on its own or bounded by the dim-text outline the menu draws, which is.
        for c in crate::arrange::PALETTE {
            let c = crate::theme::rgb(c);
            assert!(
                contrast(c, t.panel) >= 3.0 || contrast(t.text_dim, t.panel) >= 3.0,
                "{name}: menu swatch {c:?} on the panel: {}",
                contrast(c, t.panel)
            );
        }
        for bg in [t.bg, t.panel, t.panel_raised, t.lane] {
            assert!(contrast(t.accent, bg) >= 3.0, "{name}: accent {}", contrast(t.accent, bg));
            assert!(contrast(t.focus_ring, bg) >= 3.0, "{name}: focus ring");
            assert!(contrast(t.cursor, bg) >= 3.0, "{name}: playhead");
        }
        // Meters: level and clip cap on the meter's background.
        assert!(contrast(t.monitor, t.bg) >= 3.0, "{name}: meter level");
        assert!(contrast(t.record, t.bg) >= 3.0, "{name}: clip cap");
        // State fills carry their letter at 4.5:1.
        crate::theme::set_current(if t.light_clips { egui::Theme::Light } else { egui::Theme::Dark });
        for c in [t.mute, t.solo, t.record, t.monitor, t.accent] {
            assert!(contrast(crate::theme::text_on(c), c) >= 4.5, "{name}: letter on {c:?}");
        }
    }
    crate::theme::set_current(egui::Theme::Dark);
}

#[test]
fn clip_names_read_on_every_track_color_in_both_themes() {
    use crate::theme::{clip_colors, contrast, rgb, set_current};
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        set_current(theme);
        for c in soundcraft_model::TRACK_COLORS {
            for selected in [false, true] {
                for base in [rgb(c), crate::theme::darken(rgb(c), 0.5)] {
                    let (_, strip, ink) = clip_colors(base, selected);
                    assert!(contrast(ink, strip) >= 4.5, "{theme:?} {c:?} selected={selected}: {}", contrast(ink, strip));
                }
            }
        }
    }
    set_current(egui::Theme::Dark);
}

#[test]
fn tokens_are_the_only_colors() {
    // Every literal color lives in theme.rs (transparent mesh fringes aside).
    for (file, src) in [
        ("arrange.rs", include_str!("arrange.rs")),
        ("browser.rs", include_str!("browser.rs")),
        ("devices.rs", include_str!("devices.rs")),
        ("groups.rs", include_str!("groups.rs")),
        ("keys.rs", include_str!("keys.rs")),
        ("menus.rs", include_str!("menus.rs")),
        ("mixer.rs", include_str!("mixer.rs")),
        ("transport.rs", include_str!("transport.rs")),
        ("telemetry.rs", include_str!("telemetry.rs")),
        ("waveform.rs", include_str!("waveform.rs")),
        ("icons.rs", include_str!("icons.rs")),
        ("shortcuts.rs", include_str!("shortcuts.rs")),
    ] {
        for pat in ["Color32::from_", "Color32::WHITE", "Color32::BLACK", "Color32::RED", "Color32::GRAY", "Color32::LIGHT", "Color32::DARK"] {
            assert!(!src.contains(pat), "{file} paints a literal color ({pat})");
        }
    }
}

#[test]
fn no_two_shortcuts_share_a_key_in_a_scope() {
    use crate::shortcuts::{Scope, TABLE, text};
    let mut seen = std::collections::HashMap::new();
    for s in TABLE {
        let k = (s.scope, text(s));
        assert!(seen.insert(k.clone(), s.label).is_none(), "{k:?}: {} and {}", seen[&k], s.label);
        assert!(!s.note.is_empty(), "{}: every row says where its key comes from", s.label);
    }
    // A global key must not also mean something in the arrangement or the piano roll.
    for g in TABLE.iter().filter(|s| s.scope == Scope::Global) {
        for o in TABLE.iter().filter(|s| matches!(s.scope, Scope::Arrangement | Scope::PianoRoll)) {
            assert_ne!(text(g), text(o), "{} (global) and {} ({:?})", g.label, o.label, o.scope);
        }
    }
}

#[test]
fn every_handler_takes_its_keys_from_the_table() {
    let sources = [
        include_str!("menus.rs"),
        include_str!("groups.rs"),
        include_str!("browser.rs"),
        include_str!("keys.rs"),
        include_str!("mixer.rs"),
        include_str!("arrange.rs"),
        include_str!("devices.rs"),
        include_str!("transport.rs"),
        include_str!("telemetry.rs"),
        include_str!("lib.rs"),
        include_str!("finish.rs"),
    ];
    for src in sources {
        for raw in ["consume_key(", "key_pressed(", "shortcut_text(\"", "Key::"] {
            assert!(!src.contains(raw), "a handler reads keys directly ({raw})");
        }
    }
    // And every action in the table has a handler.
    let all: String = sources.concat();
    for s in crate::shortcuts::TABLE {
        let name = format!("{:?}", s.action);
        assert!(all.contains(&format!("A::{name}")) || all.contains(&format!("Action::{name}")), "{name} has no handler");
    }
}

#[test]
fn menus_show_the_table_keys() {
    use crate::shortcuts::{Action, label};
    assert_eq!(label(Action::Redo), "Ctrl+Shift+Z");
    assert_eq!(label(Action::Split), "Ctrl+E");
    assert_eq!(label(Action::ToggleBrowser), "Ctrl+Alt+B");
    assert_eq!(label(Action::Record), "R");
    assert_eq!(label(Action::NotesTransposeUp), "Ctrl+Up");
}

#[test]
fn keys_from_the_table_still_drive_the_transport_and_views() {
    let mut t = T::new();
    t.input("ui.key", json!({"key": "C"}));
    assert!(t.app.engine.session().edit.loop_playback, "C: cycle");
    t.input("ui.key", json!({"key": "Space"}));
    assert!(t.app.is_playing(), "Space: play");
    t.input("ui.key", json!({"key": "Space"}));
    assert!(!t.app.is_playing());
    let shown = t.app.ui.browser.show;
    t.input("ui.key", json!({"key": "B", "cmd": true, "alt": true}));
    assert_eq!(t.app.ui.browser.show, !shown, "Ctrl+Alt+B: browser");
    t.input("ui.key", json!({"key": "F3"}));
    assert!(t.app.ui.mixer_full, "F3: full mixer");
}

#[test]
fn help_keyboard_shortcuts_opens_groups_by_scope_and_filters() {
    let mut t = T::new();
    let rev = (t.app.engine.is_dirty(), t.app.engine.revision);
    t.click(mid(view_item(&t, "Help")));
    t.click(mid(view_item(&t, "Keyboard Shortcuts")));
    let w = t.inspect()["shortcuts"].clone();
    assert_eq!(w["open"], true);
    let scopes: Vec<String> = serde_json::from_value(w["scopes"].clone()).unwrap();
    assert_eq!(scopes, ["Global", "Arrangement", "Piano roll", "Browser", "Mixer", "Dialogs", "Name field"]);
    let all = w["rows"].as_array().unwrap().len();
    assert_eq!(all, crate::shortcuts::TABLE.len(), "every row shown");
    // Filter by typing in its box.
    let filter: [f32; 4] = serde_json::from_value(w["filter"].clone()).unwrap();
    let spp = t.app.arrange.layout.samples_per_px;
    t.click(mid(filter));
    t.input("ui.text", json!({"text": "zoom-="}));
    assert_eq!(t.app.arrange.layout.samples_per_px, spp, "typing - and = in the filter is not a shortcut");
    t.app.ui.shortcuts_filter = "zoom".into();
    t.frames(2);
    let rows: Vec<String> = serde_json::from_value(t.inspect()["shortcuts"]["rows"].clone()).unwrap();
    assert_eq!(rows, ["Arrangement · Zoom In · +", "Arrangement · Zoom In · =", "Arrangement · Zoom In · Shift+Plus", "Arrangement · Zoom Out · -"]);
    t.app.ui.shortcuts_filter.clear();
    t.click(mid(filter));
    t.input("ui.text", json!({"text": "ctrl+e"}));
    let rows: Vec<String> = serde_json::from_value(t.inspect()["shortcuts"]["rows"].clone()).unwrap();
    assert_eq!(rows, ["Arrangement · Split at Cursor · Ctrl+E"]);
    assert_eq!((t.app.engine.is_dirty(), t.app.engine.revision), rev, "the window never touches the document");
}

#[test]
fn the_shortcuts_window_closes_from_its_title_bar() {
    let mut t = T::new();
    t.click(mid(view_item(&t, "Help")));
    t.click(mid(view_item(&t, "Keyboard Shortcuts")));
    let w: [f32; 4] = serde_json::from_value(t.inspect()["shortcuts"]["window"].clone()).unwrap();
    t.click((w[2] - 14.0, w[1] + 13.0));
    assert_eq!(t.inspect()["shortcuts"]["open"], false);
    assert!(!t.app.ui.show_shortcuts);
}

#[test]
fn both_themes_are_flat_on_the_spacing_grid() {
    let t = T::new();
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let st = t.ctx.style_of(theme);
        let v = &st.visuals;
        assert_eq!(v.window_shadow, egui::Shadow::NONE, "{theme:?}: no window shadow");
        assert_eq!(v.popup_shadow, egui::Shadow::NONE, "{theme:?}: no popup shadow");
        assert_eq!(v.window_corner_radius, egui::CornerRadius::same(crate::theme::RADIUS as u8));
        assert_eq!(v.widgets.inactive.corner_radius, egui::CornerRadius::same(crate::theme::RADIUS_SM as u8));
        assert_eq!(v.selection.stroke.width, crate::theme::FOCUS_W, "{theme:?}: focus ring token");
        assert_eq!(st.spacing.item_spacing.x % (crate::theme::SPACE / 2.0), 0.0);
        assert_eq!(st.spacing.menu_margin, egui::Margin::same(crate::theme::SPACE as i8));
    }
}

// The argument list of the call opening at `open` (just after its `(`), split at top-level
// commas, however many lines it spans.
fn args(src: &str, open: usize) -> Vec<String> {
    let (mut depth, mut cur, mut out) = (0usize, String::new(), Vec::new());
    for ch in src[open..].chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out.into_iter().map(|a| a.split_whitespace().collect::<Vec<_>>().join(" ")).collect()
}
// A bare number (other than 0) or `CornerRadius::same(<number>)` is a literal radius.
fn literal(arg: &str) -> bool {
    let a = arg.trim();
    let a = a.strip_prefix("egui::CornerRadius::same(").or_else(|| a.strip_prefix("CornerRadius::same(")).map_or(a, |r| r.trim_end_matches(')'));
    let a = a.trim_end_matches("u8").trim_end_matches("f32").trim_end_matches('_');
    a.parse::<f32>().is_ok_and(|v| v != 0.0)
}
#[test]
fn painted_corners_use_the_radius_tokens() {
    let mut checked = 0;
    for (file, src) in [
        ("arrange.rs", include_str!("arrange.rs")),
        ("browser.rs", include_str!("browser.rs")),
        ("devices.rs", include_str!("devices.rs")),
        ("keys.rs", include_str!("keys.rs")),
        ("menus.rs", include_str!("menus.rs")),
        ("mixer.rs", include_str!("mixer.rs")),
        ("transport.rs", include_str!("transport.rs")),
        ("telemetry.rs", include_str!("telemetry.rs")),
    ] {
        // (call, index of the radius argument)
        for (call, at) in
            [("rect_filled(", 1), ("rect_stroke(", 1), (".rect(", 1), ("CornerRadius::same(", 0), (".corner_radius(", 0), (".rounding(", 0)]
        {
            for (i, _) in src.match_indices(call) {
                let a = args(src, i + call.len());
                checked += 1;
                if let Some(radius) = a.get(at) {
                    assert!(!literal(radius), "{file}: literal corner radius in {call}{}", a.join(", "));
                }
            }
        }
        assert!(!src.contains("CornerRadius { nw"), "top corners go through theme::top_corners");
    }
    assert!(checked > 20, "the scan found the calls: {checked}");
}

#[test]
fn the_radius_scan_catches_multi_line_literals() {
    let src = "p.rect_filled(
    b,
    3.0,
    if on { a } else { c },
);";
    let a = args(src, src.find('(').unwrap() + 1);
    assert_eq!(a, ["b", "3.0", "if on { a } else { c }", ""]);
    assert!(literal(&a[1]));
    assert!(literal("egui::CornerRadius::same(4)"));
    assert!(!literal("crate::theme::RADIUS_SM"));
    assert!(!literal("0.0"));
}

#[test]
fn every_table_row_is_reachable_and_no_looser_row_shadows_it() {
    use crate::shortcuts::{Action, Scope, TABLE, pressed, text};
    let live = [Scope::Global, Scope::Arrangement, Scope::PianoRoll, Scope::Mixer];
    // Does `action` consume a press of `key` with exactly `mods` held? (A fresh frame each time,
    // so one action cannot eat the press another is asked about.)
    let fires = |key: egui::Key, mods: egui::Modifiers, action: Action| {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(mods),
                egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: mods },
            ],
            ..Default::default()
        };
        let mut hit = false;
        let mut out = ctx.run_ui(raw, |ui| hit = pressed(ui.ctx(), action));
        out.textures_delta.clear();
        hit
    };
    for row in TABLE.iter().filter(|r| live.contains(&r.scope)) {
        assert!(fires(row.key, row.mods, row.action), "{} ({}) does not fire", row.label, text(row));
        // Only rows with exactly these modifiers answer; a looser row of the same key does not.
        for o in TABLE.iter().filter(|o| o.key == row.key && o.mods != row.mods) {
            assert!(
                !fires(row.key, row.mods, o.action) || TABLE.iter().any(|x| x.action == o.action && x.key == row.key && x.mods == row.mods),
                "{} ({}) also fires on {}",
                o.label,
                text(o),
                text(row)
            );
        }
        // Extra Alt or Shift on top of the row's modifiers reaches none of the same-key rows.
        for extra in [egui::Modifiers { alt: true, ..row.mods }, egui::Modifiers { shift: true, ..row.mods }] {
            if extra == row.mods || TABLE.iter().any(|o| o.key == row.key && o.mods == extra) {
                continue;
            }
            for o in TABLE.iter().filter(|o| o.key == row.key) {
                assert!(!fires(row.key, extra, o.action), "{} fires with extra modifiers on {}", o.label, text(row));
            }
        }
    }
}

#[test]
fn the_help_window_lists_the_new_rows() {
    let mut t = T::new();
    t.click(mid(view_item(&t, "Help")));
    t.click(mid(view_item(&t, "Keyboard Shortcuts")));
    let rows: Vec<String> = serde_json::from_value(t.inspect()["shortcuts"]["rows"].clone()).unwrap();
    for want in [
        "Global · New Audio Track · Ctrl+T",
        "Global · New Instrument Track · Ctrl+Shift+T",
        "Global · Record · F9",
        "Arrangement · Select All Clips · Ctrl+A",
    ] {
        assert!(rows.iter().any(|r| r == want), "{want} missing from {rows:?}");
    }
    assert!(!rows.iter().any(|r| r.contains("Split at Cursor · Ctrl+T")));
}

#[test]
fn the_shortcut_filter_searches_notes_and_spells_out_shift_plus() {
    use crate::shortcuts::{TABLE, filtered, text};
    for s in TABLE {
        let hit = filtered(s.note).into_iter().flat_map(|(_, r)| r).any(|r| r.action == s.action && r.key == s.key && r.mods == s.mods);
        assert!(hit, "{}: its own note finds it", s.label);
    }
    assert!(TABLE.iter().any(|s| text(s) == "Shift+Plus"));
    assert!(!TABLE.iter().any(|s| text(s).ends_with("++")));
}

fn snap() -> soundcraft_playback::telemetry::CallbackTelemetry {
    let mut h = [0u64; 16];
    h[3] = 7;
    h[15] = 1;
    soundcraft_playback::telemetry::CallbackTelemetry {
        callbacks: 100,
        frames: 51_200,
        timed_callbacks: 100,
        max_duration_ns: 3_000_000,
        max_load_ppm: 123_456,
        over_budget_callbacks: 2,
        duration_histogram: h,
        backend_errors: 3,
        device_lost_errors: 1,
        contention_silence_frames: 999,
        unavailable_silence_frames: 888,
        ..Default::default()
    }
}

#[test]
fn the_telemetry_readout_is_absent_without_a_player() {
    let mut t = T::new();
    assert!(t.app.player.is_none());
    t.frames(30);
    let tel = t.inspect()["telemetry"].clone();
    assert_eq!(tel["shown"], false);
    assert_eq!(t.app.telemetry.reads, 0, "no player, nothing read");
    // Even a stale open flag cannot show a window without a readout.
    t.app.telemetry.open = true;
    t.frames(2);
    assert_eq!(t.inspect()["telemetry"]["open"], false);
}

#[test]
fn the_telemetry_readout_texts_and_click_open_the_details_window() {
    use crate::telemetry::{detail_rows, load_text, readout_text};
    let s = snap();
    assert_eq!(load_text(123_456), "12.3%");
    assert_eq!(load_text(1_000_000), "100.0%");
    assert_eq!(load_text(0), "0.0%");
    assert_eq!(readout_text(&s), "Load 12.3%  Over 2");
    let mut t = T::new();
    t.app.telemetry.injected = Some(s);
    t.frames(30);
    let tel = t.inspect()["telemetry"].clone();
    assert_eq!(tel["shown"], true);
    assert_eq!(tel["text"], "Load 12.3%  Over 2");
    assert_eq!(tel["warning"], 3, "the warning icon carries the backend error count");
    assert_eq!(tel["open"], false);
    let rect: [f32; 4] = serde_json::from_value(tel["rect"].clone()).unwrap();
    let rev = (t.app.engine.is_dirty(), t.app.engine.revision);
    t.click(mid(rect));
    let tel = t.inspect()["telemetry"].clone();
    assert_eq!(tel["open"], true, "clicking the readout opens the window");
    let rows: Vec<String> = serde_json::from_value(tel["rows"].clone()).unwrap();
    assert_eq!(rows, detail_rows(&s));
    assert!(rows.contains(&"up to 8 \u{b5}s: 7".to_string()), "{rows:?}");
    assert!(rows.contains(&"over 16.384 ms: 1".to_string()), "{rows:?}");
    assert!(rows.contains(&"Over-budget callbacks (nominal): 2".to_string()));
    assert!(rows.contains(&"Peak callback load (since open): 12.3%".to_string()));
    assert!(!rows.iter().any(|r| r.contains("999") || r.contains("888") || r.to_lowercase().contains("teardown")), "teardown counters stay hidden");
    // Clicking again closes it; so does the title bar's close button.
    t.click(mid(rect));
    assert_eq!(t.inspect()["telemetry"]["open"], false);
    t.click(mid(rect));
    let w: [f32; 4] = serde_json::from_value(t.inspect()["telemetry"]["window"].clone()).unwrap();
    t.click((w[2] - 14.0, w[1] + 13.0));
    assert_eq!(t.inspect()["telemetry"]["open"], false, "closed from its title bar");
    assert_eq!((t.app.engine.is_dirty(), t.app.engine.revision), rev, "the readout never touches the document");
}

#[test]
fn the_telemetry_readout_has_no_warning_without_errors() {
    let mut t = T::new();
    t.app.telemetry.injected = Some(soundcraft_playback::telemetry::CallbackTelemetry::default());
    t.frames(30);
    let tel = t.inspect()["telemetry"].clone();
    assert_eq!(tel["text"], "Load 0.0%  Over 0");
    assert!(tel["warning"].is_null());
}

#[test]
fn telemetry_math_saturates_when_the_snapshot_is_torn() {
    use crate::telemetry::{detail_rows, error_count};
    // timed_callbacks ahead of callbacks (a reader that straddled a publication) and a huge sum.
    let s = soundcraft_playback::telemetry::CallbackTelemetry {
        callbacks: 5,
        timed_callbacks: 9,
        duration_histogram: [u64::MAX; 16],
        device_lost_errors: 4,
        ..Default::default()
    };
    let rows = detail_rows(&s);
    assert!(rows.contains(&"Callbacks without a timing: 0".to_string()), "{rows:?}");
    assert!(rows.contains(&format!("Histogram total: {}", u64::MAX)), "saturated, not wrapped");
    assert_eq!(error_count(&s), Some(4), "device loss alone still warns");
    assert_eq!(error_count(&Default::default()), None);
}

#[test]
fn the_telemetry_is_read_a_few_times_a_second_not_every_frame() {
    let mut t = T::new();
    t.app.telemetry.injected = Some(snap());
    t.frames(120); // two seconds at 60 fps
    let reads = t.app.telemetry.reads;
    assert!((7..=9).contains(&reads), "about four reads a second, not 120: {reads}");
}
