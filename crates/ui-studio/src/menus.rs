//! The minimal menus (File, Edit, Track, Transport, Audio), keyboard shortcuts, the status bar,
//! the save-changes question and the recovery offer.
//!
//! Shortcuts come from one table, [`crate::shortcuts`] (Ableton where Ableton and Cubase differ).

use crate::shortcuts::{Action as A, label, pressed};
use crate::theme::{STATUS_H, TOKENS};
use crate::{Dialog, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::TrackId;

impl StudioApp {
    /// Run a session-replacing command or Quit, asking to save unsaved changes first.
    pub fn run_or_ask(&mut self, id: &str, params: Value) {
        if self.engine.transport.recording && matches!(id, "session.new" | "session.open" | "app.quit") {
            self.stop_play();
        }
        if self.engine.is_dirty() && matches!(id, "session.new" | "session.open" | "app.quit") {
            self.dialog = Some(Dialog::SaveChanges { then: (id.into(), params) });
            return;
        }
        let _ = self.run(id, params);
    }

    /// File › Save: to the session's file, or Save As when it has none. False when cancelled or failed.
    pub fn save(&mut self) -> bool {
        if self.engine.path.is_some() {
            return self.run("session.save", json!({})).is_ok();
        }
        self.save_as()
    }

    pub fn save_as(&mut self) -> bool {
        let name = format!("{}.scraft", self.engine.session().name);
        let Some(path) = self.services.pick_save.as_ref().and_then(|f| f(&name)) else { return false };
        self.run("session.save_as", json!({"path": path, "replace": true})).is_ok()
    }

    pub fn open_dialog(&mut self) {
        let Some(path) = self.services.pick_open.as_ref().and_then(|f| f("session.open")) else { return };
        self.run_or_ask("session.open", json!({"path": path}));
    }
}

pub fn menu_bar(app: &mut StudioApp, ui: &mut egui::Ui) {
    app.arrange.bar_menu.clear();
    egui::Panel::top("menu_bar")
        .exact_size(24.0)
        .frame(egui::Frame::NONE.fill(TOKENS.panel).inner_margin(egui::Margin::symmetric(crate::theme::SPACE as i8, 2)))
        .show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let file_menu = ui.menu_button("File", |ui| {
                    if ui.button("New").clicked() {
                        app.run_or_ask("session.new", json!({}));
                        ui.close();
                    }
                    if ui.button("Open…").clicked() {
                        ui.close();
                        app.open_dialog();
                    }
                    if ui.button("Save").clicked() {
                        ui.close();
                        app.save();
                    }
                    if ui.button("Save As…").clicked() {
                        ui.close();
                        app.save_as();
                    }
                    ui.separator();
                    let b = ui.add(egui::Button::new("Bounce…").shortcut_text(label(A::Bounce)));
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Bounce…".into(), String::new(), b.rect));
                    if b.clicked() {
                        ui.close();
                        app.open_bounce();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ui.close();
                        app.run_or_ask("app.quit", json!({}));
                    }
                });
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "File".into(), String::new(), file_menu.response.rect));
                let edit_menu = ui.menu_button("Edit", |ui| {
                    history_item(app, ui, false);
                    history_item(app, ui, true);
                    let items: [(&str, A, fn(&mut StudioApp)); 6] = [
                        ("Cut", A::Cut, |a| drop(a.copy_selected(true))),
                        ("Copy", A::Copy, |a| drop(a.copy_selected(false))),
                        ("Paste at Cursor", A::Paste, |a| drop(a.paste_at_cursor())),
                        ("Duplicate", A::Duplicate, |a| drop(a.duplicate_selected())),
                        ("Split at Cursor", A::Split, |a| drop(a.split_at_cursor())),
                        ("Delete", A::Delete, |a| drop(a.delete_selected())),
                    ];
                    for (text, action, f) in items {
                        if ui.add(egui::Button::new(text).shortcut_text(label(action))).clicked() {
                            f(app);
                            ui.close();
                        }
                    }
                    ui.separator();
                    let b = ui.add(egui::Button::new("Undo History").shortcut_text(label(A::UndoHistory)));
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Undo History".into(), String::new(), b.rect));
                    if b.clicked() {
                        app.ui.show_history = true;
                        ui.close();
                    }
                });
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Edit".into(), String::new(), edit_menu.response.rect));
                let track_menu = ui.menu_button("Track", |ui| {
                    if ui.add(egui::Button::new("Add Mono Audio Track").shortcut_text(label(A::NewAudioTrack))).clicked() {
                        let _ = app.add_audio_track("mono");
                        ui.close();
                    }
                    if ui.button("Add Stereo Audio Track").clicked() {
                        let _ = app.add_audio_track("stereo");
                        ui.close();
                    }
                    if ui.add(egui::Button::new("Add Instrument Track").shortcut_text(label(A::NewInstrumentTrack))).clicked() {
                        let _ = app.add_track("instrument");
                        ui.close();
                    }
                    if ui.button("Add FX Channel").clicked() {
                        let _ = app.add_fx_channel();
                        ui.close();
                    }
                    if ui.button("New MIDI Clip at Cursor").clicked() {
                        let _ = app.new_midi_clip(None);
                        ui.close();
                    }
                    let unused = crate::mixer::orphan_busses(&app.engine.session_arc()).len();
                    if unused > 0 && ui.button(format!("Remove unused buses ({unused})")).clicked() {
                        let _ = app.remove_unused_busses();
                        ui.close();
                    }
                    ui.separator();
                    let b = ui.add(egui::Button::new("Rename").shortcut_text(label(A::Rename)));
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Rename".into(), String::new(), b.rect));
                    if b.clicked() {
                        ui.close();
                        app.rename_selected();
                    }
                    let b = ui.button("Bounce in Place");
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Bounce in Place".into(), String::new(), b.rect));
                    if b.clicked() {
                        ui.close();
                        let _ = app.bounce_in_place();
                    }
                    ui.separator();
                    crate::groups::menu_items(app, ui, None);
                });
                // Where the Track menu sits, for `ui.inspect` and tests (slice 2c).
                app.arrange.bar_menu.retain(|m| m.label != "Track");
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Track".into(), String::new(), track_menu.response.rect));
                let mut view_items = Vec::new();
                let auto_menu = ui.menu_button("Automation", |ui| {
                    // Follow-ups: a preference, not the document.
                    let on = app.ui.thin_after_write;
                    let b = check_button(ui, on, "Thin after write");
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Thin after write".into(), String::new(), b.rect));
                    if b.clicked() {
                        app.ui.thin_after_write = !on;
                        ui.close();
                    }
                    let b = ui.add_enabled(app.focus_track().is_some_and(|t| !app.pass_on(t)), menu_button("Thin Focused Track"));
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Thin Focused Track".into(), String::new(), b.rect));
                    if b.clicked() {
                        let _ = app.thin_focused();
                        ui.close();
                    }
                });
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Automation".into(), String::new(), auto_menu.response.rect));
                let view_menu = ui.menu_button("View", |ui| {
                    let u = &mut app.ui;
                    for (name, action, flag) in [
                        ("Browser", A::ToggleBrowser, &mut u.browser.show),
                        ("Mixer", A::ToggleMixer, &mut u.show_mixer),
                        ("Devices", A::ToggleDevices, &mut u.show_devices),
                        ("Full Mixer Window", A::FullMixer, &mut u.mixer_full),
                    ] {
                        let b = check_button_with(ui, *flag, name, Some(label(action)));
                        view_items.push(crate::mixer::MenuItem::at(ui, name.into(), String::new(), b.rect));
                        if b.clicked() {
                            *flag = !*flag;
                            ui.close();
                        }
                    }
                    ui.separator();
                    // Appearance: System follows the OS; kept in `UiState` (slice 5).
                    let sub = ui.menu_button(menu_contents("Appearance"), |ui| {
                        for a in crate::theme::Appearance::ALL {
                            let on = app.ui.appearance == a;
                            let b = check_button(ui, on, a.label());
                            view_items.push(crate::mixer::MenuItem::at(ui, a.label().into(), "appearance".into(), b.rect));
                            if b.clicked() {
                                app.ui.appearance = a;
                                ui.close();
                            }
                        }
                    });
                    view_items.push(crate::mixer::MenuItem::at(ui, "Appearance".into(), String::new(), sub.response.rect));
                });
                // Where the View menu and its entries sit, for `ui.inspect` and tests (slice 4).
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "View".into(), String::new(), view_menu.response.rect));
                app.arrange.bar_menu.extend(view_items);
                let transport_menu = ui.menu_button("Transport", |ui| {
                    let cycle = app.engine.session().edit.loop_playback;
                    let items: [(&str, A, &str, Value); 4] = [
                        ("Play / Stop", A::PlayStop, "transport.toggle", json!({})),
                        ("Record", A::Record, "transport.record", json!({})),
                        ("Return to Zero", A::ReturnToZero, "transport.rtz", json!({})),
                        (if cycle { "Cycle Off" } else { "Cycle On" }, A::Cycle, "options.loop_playback", json!({"value": !cycle})),
                    ];
                    for (text, action, id, p) in items {
                        if ui.add(menu_button(text).shortcut_text(label(action))).clicked() {
                            let _ = app.run(id, p);
                            ui.close();
                        }
                    }
                    ui.separator();
                    // Slice 6: a preference, not the document.
                    let on = app.ui.count_in;
                    let b = check_button(ui, on, "Count-in (1 bar)");
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Count-in (1 bar)".into(), String::new(), b.rect));
                    if b.clicked() {
                        app.ui.count_in = !on;
                        ui.close();
                    }
                });
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Transport".into(), String::new(), transport_menu.response.rect));
                let mut audio_items = Vec::new();
                let audio_menu = ui.menu_button("Audio", |ui| {
                    let name = app.player.as_ref().map_or("No audio device (--no-audio)".to_string(), |p| p.device_name.clone());
                    ui.label(egui::RichText::new(name).color(TOKENS.text_dim));
                    let b = ui.button("Audio Devices…");
                    audio_items.push(crate::mixer::MenuItem::at(ui, "Audio Devices…".into(), String::new(), b.rect));
                    if b.clicked() {
                        app.open_audio_devices();
                        ui.close();
                    }
                    let b = ui.add_enabled(
                        !app.engine.transport.recording && (app.player.is_some() || app.audio_devices.fake.is_some()),
                        egui::Button::new("Reconnect Device"),
                    );
                    audio_items.push(crate::mixer::MenuItem::at(ui, "Reconnect Device".into(), String::new(), b.rect));
                    if b.clicked() {
                        app.reconnect_audio_devices();
                        ui.close();
                    }
                    if ui.button("Scan Plug-ins").clicked() {
                        crate::plugin_scan::rescan();
                        ui.close();
                    }
                });
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Audio".into(), String::new(), audio_menu.response.rect));
                app.arrange.bar_menu.extend(audio_items);
                let help = ui.menu_button("Help", |ui| {
                    let b = ui.button("Keyboard Shortcuts");
                    app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Keyboard Shortcuts".into(), String::new(), b.rect));
                    if b.clicked() {
                        app.ui.show_shortcuts = true;
                        ui.close();
                    }
                });
                app.arrange.bar_menu.push(crate::mixer::MenuItem::at(ui, "Help".into(), String::new(), help.response.rect));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let s = app.engine.session();
                    let dirty = if app.engine.is_dirty() { " •" } else { "" };
                    ui.label(egui::RichText::new(format!("{}{dirty}", s.name)).color(TOKENS.text_dim));
                });
            });
        });
}

pub fn status_bar(app: &mut StudioApp, ui: &mut egui::Ui) {
    egui::Panel::bottom("status")
        .exact_size(STATUS_H)
        .frame(egui::Frame::NONE.fill(TOKENS.panel).inner_margin(egui::Margin::symmetric(crate::theme::SPACE as i8, 2)))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(egui::RichText::new(&app.status).color(TOKENS.text_dim).size(11.5));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let c = app.cursor();
                    let text = format!("Cursor {}  ·  {}", crate::transport::bars_beats(app, c).trim(), crate::transport::min_sec(app, c));
                    ui.label(egui::RichText::new(text).color(TOKENS.text_dim).size(11.5).monospace());
                });
            });
        });
}

/// Both history actions share the recording refusal used by shortcuts and control requests.
pub(crate) fn history_item(app: &mut StudioApp, ui: &mut egui::Ui, redo: bool) -> egui::Response {
    let (text, action, command) = if redo { ("Redo", A::Redo, "edit.redo") } else { ("Undo", A::Undo, "edit.undo") };
    let reason = app.history_disabled_reason();
    let response =
        ui.add_enabled(reason.is_none(), egui::Button::new(text).shortcut_text(label(action))).on_disabled_hover_text(reason.unwrap_or_default());
    if response.clicked() {
        let _ = app.run(command, json!({}));
        ui.close();
    }
    response
}

/// Keyboard shortcuts, unless a text field has the keyboard. Keys come from [`crate::shortcuts`].
pub fn shortcuts(app: &mut StudioApp, ctx: &egui::Context) {
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() {
        return;
    }
    // Alt+C before C: clear every clip latch (`track.clear_clip_indicators`).
    if pressed(ctx, A::ClearClipIndicators) {
        let _ = app.run("track.clear_clip_indicators", json!({}));
    }
    // egui turns the platform's copy/cut/paste keys into these events.
    // Only with Ctrl alone: Ctrl+Alt+C is not Copy.
    let (copy, cut, paste) = ctx.input(|i| {
        if !i.modifiers.matches_exact(egui::Modifiers::COMMAND) {
            return (false, false, false);
        }
        let has = |f: fn(&egui::Event) -> bool| i.events.iter().any(f);
        (has(|e| matches!(e, egui::Event::Copy)), has(|e| matches!(e, egui::Event::Cut)), has(|e| matches!(e, egui::Event::Paste(_))))
    });
    if copy || pressed(ctx, A::Copy) {
        let _ = app.copy_selected(false);
    }
    if cut || pressed(ctx, A::Cut) {
        let _ = app.copy_selected(true);
    }
    if paste || pressed(ctx, A::Paste) {
        let _ = app.paste_at_cursor();
    }
    if pressed(ctx, A::Redo) {
        let _ = app.run("edit.redo", json!({}));
    }
    if pressed(ctx, A::Undo) {
        let _ = app.run("edit.undo", json!({}));
    }
    if pressed(ctx, A::Duplicate) {
        let _ = app.duplicate_selected();
    }
    if pressed(ctx, A::Split) {
        let _ = app.split_at_cursor();
    }
    if pressed(ctx, A::NewInstrumentTrack) {
        let _ = app.add_track("instrument");
    }
    if pressed(ctx, A::NewAudioTrack) {
        let _ = app.add_track("audio");
    }
    if pressed(ctx, A::SelectAll) {
        app.select_all_clips();
    }
    if pressed(ctx, A::SaveAs) {
        app.save_as();
    }
    if pressed(ctx, A::Save) {
        app.save();
    }
    if pressed(ctx, A::Open) {
        app.open_dialog();
    }
    if pressed(ctx, A::New) {
        app.run_or_ask("session.new", json!({}));
    }
    if pressed(ctx, A::Record) {
        let _ = app.run("transport.record", json!({}));
    }
    if pressed(ctx, A::PlayStop) {
        let _ = app.run("transport.toggle", json!({}));
    }
    if pressed(ctx, A::ReturnToZero) {
        let _ = app.run("transport.rtz", json!({}));
    }
    if pressed(ctx, A::Cycle) {
        let on = app.engine.session().edit.loop_playback;
        let _ = app.run("options.loop_playback", json!({"value": !on}));
    }
    // Delete over a fader, knob or send level resets it (Ableton); anywhere else it deletes.
    // Backspace does the same. (A readout being edited has the keyboard, so both edit text.)
    match app.mixer.hover {
        Some(t) => {
            if pressed(ctx, A::ResetControl) {
                app.reset_control(t);
            }
        }
        None => {
            if pressed(ctx, A::Delete) {
                let _ = app.delete_selected();
            }
        }
    }
    // Views (Ableton where it has a key; Cubase's F3 for the full mixer).
    if pressed(ctx, A::ToggleMixer) {
        app.ui.show_mixer = !app.ui.show_mixer;
    }
    if pressed(ctx, A::ToggleDevices) {
        app.ui.show_devices = !app.ui.show_devices;
    }
    if pressed(ctx, A::FullMixer) {
        app.ui.mixer_full = !app.ui.mixer_full;
    }
    if pressed(ctx, A::SelectNone) {
        let _ = app.run("edit.select_none", json!({}));
    }
    if pressed(ctx, A::ZoomIn) {
        let _ = app.engine.execute("view.zoom_in", &json!({}));
    }
    if pressed(ctx, A::ZoomOut) {
        let _ = app.engine.execute("view.zoom_out", &json!({}));
    }
}

/// The save-changes question.
pub fn dialogs(app: &mut StudioApp, ctx: &egui::Context) {
    if let Some(Dialog::DeleteTracks { tracks, clips }) = app.dialog.clone() {
        return delete_tracks_dialog(app, ctx, &tracks, clips);
    }
    if let Some(Dialog::DeleteTake { track, index }) = app.dialog.clone() {
        return delete_take_dialog(app, ctx, track, index);
    }
    let Some(Dialog::SaveChanges { then }) = app.dialog.clone() else { return };
    let mut choice = None;
    egui::Modal::new(egui::Id::new("save_changes")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.label(egui::RichText::new(format!("Save changes to “{}”?", app.engine.session().name)).strong());
        ui.label(egui::RichText::new("Your changes are lost if you don't save them.").color(TOKENS.text_dim));
        ui.add_space(crate::theme::SPACE);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                choice = Some(1);
            }
            if ui.button("Don't Save").clicked() {
                choice = Some(2);
            }
            if ui.button("Cancel").clicked() || ui.input(|i| crate::shortcuts::hit(i, A::DialogCancel)) {
                choice = Some(0);
            }
        });
    });
    match choice {
        Some(1) => {
            app.dialog = None;
            if app.save() {
                let _ = app.run(&then.0, then.1);
            }
        }
        Some(2) => {
            app.dialog = None;
            let _ = app.run(&then.0, then.1);
        }
        Some(_) => app.dialog = None,
        None => {}
    }
}

/// "Delete 3 tracks and their clips?" [Delete] [Cancel]; Enter confirms, Escape cancels.
fn delete_tracks_dialog(app: &mut StudioApp, ctx: &egui::Context, tracks: &[TrackId], clips: bool) {
    let mut choice = None;
    egui::Modal::new(egui::Id::new("delete_tracks")).show(ctx, |ui| {
        ui.set_width(340.0);
        let group = tracks.first().filter(|_| tracks.len() == 1).and_then(|t| app.engine.session().track(*t)).filter(|t| t.is_folder());
        let kept = group.map(|g| {
            let s = app.engine.session();
            let n = s.tracks.iter().filter(|t| crate::groups::parent_of(s, t) == Some(g.id)).count();
            format!("Delete {}? Its {n} track{} stay{}.", g.name, if n == 1 { "" } else { "s" }, if n == 1 { "s" } else { "" })
        });
        let what = match (tracks.len(), clips) {
            _ if kept.is_some() => kept.clone().unwrap_or_default(),
            (1, true) => {
                let name = app.engine.session().track(tracks[0]).map_or("the track".to_string(), |t| format!("“{}”", t.name));
                format!("Delete {name} and its clips?")
            }
            (1, false) => "Delete the track?".to_string(),
            (n, true) => format!("Delete {n} tracks and their clips?"),
            (n, false) => format!("Delete {n} tracks?"),
        };
        ui.label(egui::RichText::new(what).strong());
        ui.add_space(crate::theme::SPACE);
        ui.horizontal(|ui| {
            if ui.button("Delete").clicked() || ui.input(|i| crate::shortcuts::hit(i, A::DialogConfirm)) {
                choice = Some(true);
            }
            if ui.button("Cancel").clicked() || ui.input(|i| crate::shortcuts::hit(i, A::DialogCancel)) {
                choice = Some(false);
            }
        });
    });
    if let Some(yes) = choice {
        app.dialog = None;
        if yes {
            let _ = app.delete_tracks(tracks);
        }
    }
}

/// "Delete take 2? Ctrl+Z undoes." [Delete] [Cancel]; Enter confirms, Escape cancels.
fn delete_take_dialog(app: &mut StudioApp, ctx: &egui::Context, track: TrackId, index: usize) {
    let mut choice = None;
    egui::Modal::new(egui::Id::new("delete_take")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.label(egui::RichText::new(format!("Delete take {}? {} undoes.", index + 1, label(A::Undo))).strong());
        ui.add_space(crate::theme::SPACE);
        ui.horizontal(|ui| {
            if ui.button("Delete").clicked() || ui.input(|i| crate::shortcuts::hit(i, A::DialogConfirm)) {
                choice = Some(true);
            }
            if ui.button("Cancel").clicked() || ui.input(|i| crate::shortcuts::hit(i, A::DialogCancel)) {
                choice = Some(false);
            }
        });
    });
    if let Some(yes) = choice {
        app.dialog = None;
        if yes {
            let _ = app.delete_take(track, index);
        }
    }
}

/// Unsaved work an earlier run left behind, offered once at launch.
pub fn recovery(app: &mut StudioApp, ctx: &egui::Context) {
    if app.leftovers.is_empty() {
        return;
    }
    let mut act: Option<(usize, bool)> = None;
    let mut later = false;
    egui::Window::new("Recover unsaved work").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.label("An earlier run left unsaved changes:");
        for (i, l) in app.leftovers.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(&l.name);
                if ui.button("Recover").clicked() {
                    act = Some((i, true));
                }
                if ui.button("Discard").clicked() {
                    act = Some((i, false));
                }
            });
        }
        if ui.button("Later").clicked() {
            later = true;
        }
    });
    if let Some((i, recover)) = act
        && i < app.leftovers.len()
    {
        let left = app.leftovers.remove(i);
        if recover {
            app.recover(&left);
        } else {
            soundcraft_engine::autosave::discard(&left);
        }
    }
    if later {
        app.leftovers.clear();
    }
}

/// Help > Keyboard Shortcuts as last drawn, for `ui.inspect` and tests.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ShortcutsLayout {
    pub open: bool,
    pub window: crate::arrange::R4,
    pub filter: crate::arrange::R4,
    /// The scopes shown, in order.
    pub scopes: Vec<String>,
    /// The rows shown: `Scope · Action · Keys`.
    pub rows: Vec<String>,
}

/// Help > Keyboard Shortcuts: the shortcut table grouped by scope, with a filter (read-only).
pub fn shortcuts_window(app: &mut StudioApp, ctx: &egui::Context) {
    let mut layout = ShortcutsLayout { open: app.ui.show_shortcuts, ..ShortcutsLayout::default() };
    if !app.ui.show_shortcuts {
        app.shortcuts_window = layout;
        return;
    }
    let mut open = true;
    let r = egui::Window::new("Keyboard Shortcuts")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([560.0, 520.0])
        .default_pos(ctx.content_rect().center() - egui::vec2(280.0, 260.0))
        .show(ctx, |ui| {
            let resp = ui
                .horizontal(|ui| {
                    crate::icons::icon(ui, crate::icons::Icon::Filter);
                    ui.add(egui::TextEdit::singleline(&mut app.ui.shortcuts_filter).hint_text("Filter by action or key").desired_width(f32::INFINITY))
                })
                .inner;
            layout.filter = crate::arrange::r4(resp.rect);
            ui.add_space(crate::theme::SPACE);
            let groups = crate::shortcuts::filtered(&app.ui.shortcuts_filter);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if groups.is_empty() {
                    ui.label(egui::RichText::new("No shortcut matches.").color(TOKENS.text_dim));
                }
                for (scope, rows) in &groups {
                    layout.scopes.push(scope.label().into());
                    ui.label(egui::RichText::new(scope.label()).strong().color(TOKENS.text));
                    egui::Grid::new(("shortcut_grid", scope.label()))
                        .num_columns(3)
                        .spacing([2.0 * crate::theme::SPACE, crate::theme::SPACE / 2.0])
                        .show(ui, |ui| {
                            for s in rows {
                                let keys = crate::shortcuts::text(s);
                                ui.label(egui::RichText::new(s.label).color(TOKENS.text));
                                ui.label(egui::RichText::new(&keys).font(crate::theme::mono(ui.ctx(), 12.0)).color(TOKENS.accent));
                                ui.label(egui::RichText::new(s.note).color(TOKENS.text_dim).size(11.0));
                                ui.end_row();
                                layout.rows.push(format!("{} · {} · {keys}", scope.label(), s.label));
                            }
                        });
                    ui.add_space(crate::theme::SPACE);
                }
            });
        });
    if let Some(r) = r {
        layout.window = crate::arrange::r4(r.response.rect);
    }
    if !open {
        app.ui.show_shortcuts = false;
    }
    layout.open = app.ui.show_shortcuts;
    app.shortcuts_window = layout;
}

/// A fixed point-sized state column, followed by egui's point-sized icon spacing.
/// Keep this slot even for unchecked items and ordinary commands in the same menu.
const MENU_CHECK_SIZE: f32 = 12.0;

fn menu_contents(text: impl Into<egui::WidgetText>) -> (egui::Atom<'static>, egui::WidgetText) {
    (egui::Atom::custom(egui::Id::new("menu_check"), egui::Vec2::splat(MENU_CHECK_SIZE)), text.into())
}

fn menu_button(text: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(menu_contents(text))
}

/// Menu and popover choices use an icon in the state column, never a text glyph.
pub(crate) fn check_button(ui: &mut egui::Ui, on: bool, text: impl Into<egui::WidgetText>) -> egui::Response {
    check_button_with(ui, on, text, None)
}

fn check_button_with(ui: &mut egui::Ui, on: bool, text: impl Into<egui::WidgetText>, shortcut: Option<String>) -> egui::Response {
    let mut b = menu_button(text);
    if let Some(s) = shortcut {
        b = b.shortcut_text(s);
    }
    let resp = b.atom_ui(ui);
    if on && let Some(gutter) = resp.rect(egui::Id::new("menu_check")) {
        crate::icons::paint(ui.painter(), gutter.center(), crate::icons::Icon::Check, MENU_CHECK_SIZE, ui.visuals().text_color());
    }
    resp.response
}
