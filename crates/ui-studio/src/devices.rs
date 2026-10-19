//! The device chain of the focused track, under the arrangement (Ableton's detail view): the
//! instrument first on instrument tracks (its name opens the instrument menu; "Choose…" when
//! there is none), then the occupied insert slots as cards in slot order, then "+", which lists
//! the built-in effects and the scanned VST3 and CLAP effects, then the track's sends.
//!
//! Plan 06 leaks: inserts are ten fixed slots (L1), so the chain shows only the occupied ones and
//! "+" puts the new device in the slot after the last occupied one, so it lands at the end of the
//! chain; an eleventh device is refused. There is no drag-reorder: `mix.insert_move` is a swap
//! that leaves slot-keyed automation (L2) and live instances (L3) behind.

use crate::StudioApp;
use crate::icons::Icon;
use crate::mixer::{DeviceCard, DevicesLayout, MenuItem, PluginEntry};
use crate::theme::{self, TOKENS};
use egui::{Align2, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde_json::Value;
use soundcraft_model::{Insert, TrackId, TrackKind};

/// Height of the device pane.
pub const PANE_H: f32 = 132.0;
const CARD_W: f32 = 168.0;

fn r4(r: Rect) -> crate::arrange::R4 {
    [r.min.x, r.min.y, r.max.x, r.max.y]
}

/// Whether `id` names a hosted (third-party) plug-in.
pub(crate) fn hosted(id: &str) -> bool {
    id.starts_with("vst3:") || id.starts_with("clap:") || id.starts_with("au:")
}

impl StudioApp {
    /// The built-in plug-ins (`engine.plugins`, a query: no undo step, no document change).
    pub fn builtin_plugins(&mut self) -> Vec<PluginEntry> {
        if self.mixer.builtin.is_none() {
            let list = self.engine.execute("engine.plugins", &serde_json::json!({})).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
            let entries = list
                .iter()
                .map(|p| PluginEntry {
                    id: p.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                    name: p.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    category: p.get("category").map(|c| c.as_str().map_or_else(|| c.to_string(), str::to_string)).unwrap_or_default(),
                    instrument: p.get("instrument").and_then(Value::as_bool).unwrap_or(false),
                })
                .filter(|p| !p.id.is_empty())
                .collect();
            self.mixer.builtin = Some(entries);
        }
        self.mixer.builtin.clone().unwrap_or_default()
    }

    /// A plug-in's display name: built-in names, hosted names from the scan caches, and for a
    /// hosted plug-in the cache does not know, "Scanning…" while a scan runs or a generic label
    /// ([`hosted_label`]). Reads the caches only; never scans or loads anything.
    pub fn plugin_name(&mut self, id: &str, instrument: bool) -> String {
        if self.mixer.builtin.is_none() {
            self.builtin_plugins();
        }
        if let Some(n) = self.mixer.builtin.iter().flatten().find(|p| p.id == id).map(|p| p.name.clone()) {
            return n;
        }
        if let Some(n) = self.mixer.names.get(id).cloned() {
            return n;
        }
        if hosted(id) {
            let found = soundcraft_vst3_host::cached()
                .into_iter()
                .flatten()
                .map(|d| (d.id, d.name))
                .chain(soundcraft_clap_host::cached().into_iter().flatten().map(|d| (d.id, d.name)))
                .find(|(i, _)| i == id)
                .map(|(_, n)| n);
            if let Some(n) = &found {
                self.mixer.names.insert(id.to_string(), n.clone());
            }
            return hosted_label(id, instrument, found.as_deref(), crate::plugin_scan::state() == "running");
        }
        id.rsplit(':').next().unwrap_or(id).to_string()
    }
}

/// What a hosted plug-in reads as: its cached name, else "Scanning…" while a scan runs, else
/// "VST3 instrument" / "CLAP effect" and so on (never the raw class id).
pub fn hosted_label(id: &str, instrument: bool, cached_name: Option<&str>, scanning: bool) -> String {
    if let Some(n) = cached_name {
        return n.to_string();
    }
    if scanning {
        return "Scanning…".to_string();
    }
    let format = match id.split(':').next() {
        Some("vst3") => "VST3",
        Some("clap") => "CLAP",
        _ => "Audio Unit",
    };
    format!("{format} {}", if instrument { "instrument" } else { "effect" })
}

/// The "+" menu of a device list, on `resp`: the same shape as the instrument menu, with
/// built-in effects, then the scanned VST3 and CLAP effects grouped by vendor, a filter box above
/// [`FILTER_OVER`] entries, and every row and the filter rect published for `ui.inspect`. A pick
/// adds the device to the end of `tid`'s chain. Shared by the device chain and the mixer strips,
/// so a strip adds to its own track whichever one is focused.
pub(crate) fn add_device_menu(app: &mut StudioApp, resp: &egui::Response, tid: TrackId, menu: &mut Vec<MenuItem>) {
    let mut picked: Option<String> = None;
    let mut filter = std::mem::take(&mut app.mixer.effect_filter);
    let mut filter_rect = app.mixer.filter_rect;
    egui::Popup::menu(resp).width(260.0).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        let lists = app.effect_lists();
        picked = plugin_menu(ui, &lists, "effects", None, &mut filter, &mut filter_rect, menu);
    });
    app.mixer.effect_filter = filter;
    app.mixer.filter_rect = filter_rect;
    if let Some(id) = picked {
        app.mixer.effect_filter.clear();
        let _ = app.add_device(tid, &id);
    }
}

pub fn chain(app: &mut StudioApp, ui: &mut Ui) {
    let area = ui.max_rect();
    let p = ui.painter().clone();
    p.line_segment([area.left_top(), area.right_top()], Stroke::new(1.0, TOKENS.line));
    let mut layout = DevicesLayout { visible: true, area: r4(area), menu: std::mem::take(&mut app.mixer.devices.menu), ..DevicesLayout::default() };
    let Some(tid) = app.focus_track() else {
        p.text(area.center(), Align2::CENTER_CENTER, "Click a track to see its devices", FontId::proportional(12.0), TOKENS.text_dim);
        app.mixer.devices = layout;
        return;
    };
    let s = app.engine.session_arc();
    let Some(t) = s.track(tid) else { return };
    layout.track = Some(tid.0);
    // Track tab on the left: color and name.
    let tab = Rect::from_min_max(area.min, pos2(area.min.x + 120.0, area.max.y));
    p.rect_filled(tab, 0.0, TOKENS.panel_raised);
    p.rect_filled(Rect::from_min_size(tab.min, vec2(5.0, tab.height())), 0.0, theme::rgb(t.color));
    p.with_clip_rect(tab).text(pos2(tab.min.x + 12.0, tab.min.y + 16.0), Align2::LEFT_CENTER, &t.name, FontId::proportional(13.0), TOKENS.text);
    let used = t.mixer.inserts.iter().filter(|i| i.is_some()).count();
    let lane = Rect::from_min_max(pos2(tab.max.x + 8.0, area.min.y + 8.0), pos2(area.max.x - 8.0, area.max.y - 8.0));
    let mut x = lane.min.x;
    // An instrument track's instrument comes first, even when it has none yet ("Choose…").
    let mut cards: Vec<(Option<usize>, Option<Insert>)> = Vec::new();
    if t.kind == TrackKind::Instrument {
        cards.push((None, t.instrument.clone()));
    }
    for (slot, ins) in t.mixer.inserts.iter().enumerate() {
        if let Some(i) = ins {
            cards.push((Some(slot), Some(i.clone())));
        }
    }
    for (slot, ins) in cards {
        let r = Rect::from_min_size(pos2(x, lane.min.y), vec2(CARD_W, lane.height()));
        if r.min.x < lane.max.x {
            let c = card(app, ui, &p, lane, tid, slot, ins.as_ref(), r, &mut layout.menu);
            layout.cards.push(c);
        }
        // An arrow between devices: the signal flows left to right.
        let ax = r.max.x + 7.0;
        crate::icons::paint(&p.with_clip_rect(lane), pos2(ax, r.center().y), Icon::ChevronRight, 14.0, TOKENS.text_dim);
        x += CARD_W + 14.0;
    }
    // "+": add a device.
    let add = Rect::from_min_size(pos2(x, lane.min.y), vec2(40.0, lane.height()));
    layout.add = r4(add);
    let full = used >= t.mixer.inserts.len();
    let resp = ui.interact(add.intersect(lane), ui.id().with(("add_device", tid.0)), Sense::click());
    let pc = p.with_clip_rect(lane);
    pc.rect_filled(add, crate::theme::RADIUS, if resp.hovered() && !full { TOKENS.button_hover } else { TOKENS.button });
    crate::icons::paint(&pc, add.center(), Icon::Plus, 20.0, if full { TOKENS.text_dim } else { TOKENS.text });
    let resp = resp.on_hover_text(if full { "All ten device slots are in use" } else { "Add an effect" });
    if !full {
        add_device_menu(app, &resp, tid, &mut layout.menu);
    }
    // Sends of the focused track, after the chain.
    if crate::mixer::takes_sends(t.kind) {
        let sx = add.max.x + 24.0;
        if sx < lane.max.x {
            sends_section(app, ui, &p, &s, t, sx, lane, &mut layout);
        }
    }
    app.mixer.devices = layout;
}

/// One device card: bypass (power), name, remove; kind; "Open editor". The instrument card has
/// no bypass or remove (plan 06, L17); its name opens the instrument menu, and with no
/// instrument it offers "Choose…".
#[allow(clippy::too_many_arguments)]
fn card(
    app: &mut StudioApp,
    ui: &mut Ui,
    p: &egui::Painter,
    lane: Rect,
    t: TrackId,
    slot: Option<usize>,
    ins: Option<&Insert>,
    r: Rect,
    menu: &mut Vec<MenuItem>,
) -> DeviceCard {
    let pc = p.with_clip_rect(lane);
    let on = ins.is_some_and(|i| !i.bypass);
    pc.rect_filled(r, crate::theme::RADIUS, TOKENS.panel_raised);
    let title = Rect::from_min_size(r.min, vec2(r.width(), 22.0));
    pc.rect_filled(title, crate::theme::top_corners(), if on { TOKENS.button } else { TOKENS.bg });
    let plugin = ins.map(|i| i.plugin.clone()).unwrap_or_default();
    let name = if plugin.is_empty() { "No instrument".to_string() } else { app.plugin_name(&plugin, slot.is_none()) };
    let mut c =
        DeviceCard { slot, plugin: plugin.clone(), name: name.clone(), bypass: ins.is_some_and(|i| i.bypass), rect: r4(r), ..DeviceCard::default() };
    let salt = slot.map_or(u64::MAX, |s| s as u64);
    // Power button (bypass) on inserts; the instrument has none (no instrument bypass command).
    let mut name_x = r.min.x + 8.0;
    if let Some(slot) = slot {
        let pb = Rect::from_min_size(pos2(r.min.x + 4.0, r.min.y + 3.0), vec2(16.0, 16.0));
        let resp = ui.interact(pb.intersect(lane), ui.id().with(("bypass", t.0, salt)), Sense::click());
        let ink = if on {
            TOKENS.accent
        } else if resp.hovered() {
            TOKENS.text
        } else {
            TOKENS.text_dim
        };
        crate::icons::paint(&pc, pb.center(), Icon::Power, 14.0, ink);
        if resp.on_hover_text(if on { "Bypass" } else { "Turn on" }).clicked() {
            let _ = app.toggle_bypass(t, slot);
        }
        c.bypass_button = r4(pb);
        name_x = pb.max.x + 5.0;
        let xb = Rect::from_min_size(pos2(r.max.x - 20.0, r.min.y + 3.0), vec2(16.0, 16.0));
        let resp = ui.interact(xb.intersect(lane), ui.id().with(("remove", t.0, salt)), Sense::click());
        crate::icons::paint(&pc, xb.center(), Icon::Remove, 14.0, if resp.hovered() { TOKENS.record } else { TOKENS.text_dim });
        if resp.on_hover_text("Remove").clicked() {
            let _ = app.remove_device(t, slot);
        }
        c.remove_button = r4(xb);
    }
    let name_r = Rect::from_min_max(pos2(name_x, title.min.y), pos2(r.max.x - 22.0, title.max.y));
    pc.with_clip_rect(name_r.intersect(lane)).text(
        name_r.left_center(),
        Align2::LEFT_CENTER,
        &name,
        FontId::proportional(13.0),
        if on { TOKENS.text } else { TOKENS.text_dim },
    );
    let kind = match (slot, plugin.split(':').next()) {
        (None, Some("vst3")) => "VST3 instrument".to_string(),
        (None, Some("clap")) => "CLAP instrument".into(),
        (None, _) => "Instrument".into(),
        (_, Some("vst3")) => "VST3 effect".into(),
        (_, Some("clap")) => "CLAP effect".into(),
        (_, Some("au")) => "Audio Unit".into(),
        _ => app.builtin_plugins().iter().find(|p| p.id == plugin).map_or("Built-in".into(), |p| format!("Built-in · {}", p.category)),
    };
    pc.text(pos2(r.min.x + 8.0, title.max.y + 12.0), Align2::LEFT_CENTER, kind, FontId::proportional(10.0), TOKENS.text_dim);
    if c.bypass {
        pc.text(pos2(r.min.x + 8.0, title.max.y + 28.0), Align2::LEFT_CENTER, "Bypassed", FontId::proportional(10.0), TOKENS.mute);
    }
    let eb = Rect::from_min_size(pos2(r.min.x + 8.0, r.max.y - 26.0), vec2(r.width() - 16.0, 20.0));
    if slot.is_none() {
        // The instrument menu: from the name, or from "Choose…" when there is no instrument.
        let pick = if ins.is_some() { Rect::from_min_max(title.min, pos2(r.max.x, title.max.y)) } else { eb };
        let resp = ui.interact(pick.intersect(lane), ui.id().with(("pick_instrument", t.0)), Sense::click());
        if ins.is_some() {
            if resp.hovered() {
                pc.rect_filled(pick, crate::theme::top_corners(), TOKENS.button_hover);
                pc.with_clip_rect(name_r.intersect(lane)).text(
                    name_r.left_center(),
                    Align2::LEFT_CENTER,
                    &name,
                    FontId::proportional(13.0),
                    TOKENS.text,
                );
            }
            crate::icons::paint(&pc, pos2(pick.max.x - 10.0, pick.center().y), Icon::ChevronDown, 12.0, TOKENS.text_dim);
        } else {
            pc.rect_filled(eb, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
            pc.text(eb.center(), Align2::CENTER_CENTER, "Choose…", FontId::proportional(10.5), TOKENS.text);
        }
        c.pick_button = r4(pick);
        let resp = resp.on_hover_text(if ins.is_some() { "Replace the instrument" } else { "Choose an instrument" });
        let mut filter = std::mem::take(&mut app.mixer.instrument_filter);
        let mut filter_rect = app.mixer.filter_rect;
        let mut picked = None;
        egui::Popup::menu(&resp).width(260.0).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            let lists = app.instrument_lists();
            picked = instrument_menu(ui, &lists, ins.map(|i| i.plugin.as_str()), &mut filter, &mut filter_rect, menu);
        });
        app.mixer.instrument_filter = filter;
        app.mixer.filter_rect = filter_rect;
        if let Some(id) = picked {
            app.mixer.instrument_filter.clear();
            let _ = app.set_instrument(t, &id);
        }
        if ins.is_none() {
            return c;
        }
    }
    // Open editor: the plug-in's own window, through the same path as `ui.set {plugin_windows}`.
    let key_slot = slot.unwrap_or(soundcraft_mix::INSTRUMENT_SLOT);
    let enabled = app.player.as_ref().is_some_and(|pl| hosted(&plugin) || pl.has_editor(t, key_slot));
    let open = app.ui.plugin_windows.contains(&(t, key_slot));
    let resp = ui.interact(eb.intersect(lane), ui.id().with(("editor", t.0, salt)), Sense::click());
    pc.rect_filled(eb, crate::theme::RADIUS, if resp.hovered() && enabled { TOKENS.button_hover } else { TOKENS.button });
    let label = if open { "Close editor" } else { "Open editor" };
    let ink = if enabled { TOKENS.text } else { TOKENS.text_dim.gamma_multiply(0.6) };
    let font = FontId::proportional(10.5);
    let w = pc.layout_no_wrap(label.into(), font.clone(), ink).size().x + 12.0 + 4.0;
    let x0 = eb.center().x - w / 2.0;
    crate::icons::paint(&pc, pos2(x0 + 6.0, eb.center().y), Icon::OpenEditor, 12.0, ink);
    pc.text(pos2(x0 + 16.0, eb.center().y), Align2::LEFT_CENTER, label, font, ink);
    let tip = if !enabled {
        "No editor window (built-in, or no audio device)"
    } else if open {
        "Close the plug-in's window"
    } else {
        "Open the plug-in's window"
    };
    if resp.on_hover_text(tip).clicked() && enabled {
        app.toggle_editor((t, key_slot));
    }
    c.editor_button = r4(eb);
    c.editor_open = open;
    c.editor_enabled = enabled;
    c
}

/// A hosted plug-in the instrument menu offers.
#[derive(Debug, Clone)]
pub struct HostedEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
}

/// What the instrument menu lists: built-in instruments, then the scanned VST3 and CLAP
/// instruments (`None` while a format has no scan result).
#[derive(Debug, Clone, Default)]
pub struct InstrumentLists {
    pub builtin: Vec<PluginEntry>,
    pub vst3: Option<Vec<HostedEntry>>,
    pub clap: Option<Vec<HostedEntry>>,
}

/// More entries than this and the instrument menu gets a filter box.
pub(crate) const FILTER_OVER: usize = 15;

impl StudioApp {
    /// The instruments to offer, read from `engine.plugins` and the scan caches (never a scan).
    pub fn instrument_lists(&mut self) -> InstrumentLists {
        let hosted = |list: Vec<(String, String, String, bool)>| -> Vec<HostedEntry> {
            let mut v: Vec<HostedEntry> = list.into_iter().filter(|d| d.3).map(|(id, name, vendor, _)| HostedEntry { id, name, vendor }).collect();
            v.sort_by_key(|e| (e.vendor.to_lowercase(), e.name.to_lowercase()));
            v
        };
        InstrumentLists {
            builtin: self.builtin_plugins().into_iter().filter(|p| p.instrument).collect(),
            vst3: soundcraft_vst3_host::cached().map(|l| hosted(l.into_iter().map(|d| (d.id, d.name, d.vendor, d.is_instrument)).collect())),
            clap: soundcraft_clap_host::cached().map(|l| hosted(l.into_iter().map(|d| (d.id, d.name, d.vendor, d.is_instrument)).collect())),
        }
    }
}

impl StudioApp {
    /// The effects to offer in a "+" menu: the same lists as [`Self::instrument_lists`] but with
    /// the non-instruments (hosted names come from `cached()`, never a scan).
    pub fn effect_lists(&mut self) -> InstrumentLists {
        let hosted = |list: Vec<(String, String, String, bool)>| -> Vec<HostedEntry> {
            let mut v: Vec<HostedEntry> = list.into_iter().filter(|d| !d.3).map(|(id, name, vendor, _)| HostedEntry { id, name, vendor }).collect();
            v.sort_by_key(|e| (e.vendor.to_lowercase(), e.name.to_lowercase()));
            v
        };
        InstrumentLists {
            builtin: self.builtin_plugins().into_iter().filter(|p| !p.instrument).collect(),
            vst3: soundcraft_vst3_host::cached().map(|l| hosted(l.into_iter().map(|d| (d.id, d.name, d.vendor, d.is_instrument)).collect())),
            clap: soundcraft_clap_host::cached().map(|l| hosted(l.into_iter().map(|d| (d.id, d.name, d.vendor, d.is_instrument)).collect())),
        }
    }
}

/// The instrument menu's contents (see [`plugin_menu`]). The caller runs `mix.instrument` through
/// [`StudioApp::set_instrument`] for the id picked.
pub fn instrument_menu(
    ui: &mut Ui,
    lists: &InstrumentLists,
    current: Option<&str>,
    filter: &mut String,
    filter_rect: &mut crate::arrange::R4,
    menu: &mut Vec<MenuItem>,
) -> Option<String> {
    plugin_menu(ui, lists, "instruments", current, filter, filter_rect, menu)
}

/// A plug-in menu's contents: a filter box when the list is long, built-in entries, then VST3 and
/// CLAP entries grouped by vendor. Returns the id picked. Entries (with `visible` flags) and the
/// filter rect go to `menu` and `filter_rect` for `ui.inspect`; `noun` words the empty lists.
pub fn plugin_menu(
    ui: &mut Ui,
    lists: &InstrumentLists,
    noun: &str,
    current: Option<&str>,
    filter: &mut String,
    filter_rect: &mut crate::arrange::R4,
    menu: &mut Vec<MenuItem>,
) -> Option<String> {
    let mut picked = None;
    let total = lists.builtin.len() + lists.vst3.as_ref().map_or(0, Vec::len) + lists.clap.as_ref().map_or(0, Vec::len);
    if total > FILTER_OVER {
        *filter_rect = r4(ui
            .horizontal(|ui| {
                crate::icons::icon(ui, Icon::Filter);
                ui.add(egui::TextEdit::singleline(filter).hint_text("Filter").desired_width(f32::INFINITY)).rect
            })
            .inner);
    } else {
        *filter_rect = Default::default();
    }
    let f = filter.trim().to_lowercase();
    let keep = |name: &str, vendor: &str| f.is_empty() || name.to_lowercase().contains(&f) || vendor.to_lowercase().contains(&f);
    let heading = |ui: &mut Ui, text: &str| {
        ui.label(egui::RichText::new(text).color(TOKENS.text_dim).size(10.5));
    };
    let mut item = |ui: &mut Ui, id: &str, label: &str, picked: &mut Option<String>| {
        let b = ui.add(egui::Button::selectable(current == Some(id), label));
        menu.push(MenuItem::at(ui, label.to_string(), id.to_string(), b.rect));
        if b.clicked() {
            *picked = Some(id.to_string());
            ui.close();
        }
    };
    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
        heading(ui, "Built-in");
        for e in lists.builtin.iter().filter(|e| keep(&e.name, "")) {
            item(ui, &e.id, &e.name, &mut picked);
        }
        for (format, list) in [("VST3", &lists.vst3), ("CLAP", &lists.clap)] {
            ui.separator();
            heading(ui, format);
            match list {
                None => match crate::plugin_scan::scanning_label() {
                    Some(text) => {
                        ui.weak(text);
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                    }
                    None => {
                        ui.weak("Not scanned yet (Audio › Scan Plug-ins)");
                    }
                },
                Some(l) if l.is_empty() => {
                    ui.weak(format!("No {format} {noun} found"));
                }
                Some(l) => {
                    let mut vendor: Option<&str> = None;
                    let mut any = false;
                    for e in l.iter().filter(|e| keep(&e.name, &e.vendor)) {
                        let v = if e.vendor.is_empty() { "Other" } else { e.vendor.as_str() };
                        if vendor != Some(v) {
                            ui.label(egui::RichText::new(v).color(TOKENS.text_dim).size(10.0));
                            vendor = Some(v);
                        }
                        item(ui, &e.id, &e.name, &mut picked);
                        any = true;
                    }
                    if !any {
                        ui.weak("No match");
                    }
                }
            }
        }
    });
    picked
}

/// The device chain's "Sends" section for the focused track: rows in columns, then "+ Send".
#[allow(clippy::too_many_arguments)]
fn sends_section(
    app: &mut StudioApp,
    ui: &mut Ui,
    p: &egui::Painter,
    s: &soundcraft_model::Session,
    t: &soundcraft_model::Track,
    x: f32,
    lane: Rect,
    layout: &mut DevicesLayout,
) {
    const COL_W: f32 = 230.0;
    const ROW_H: f32 = 18.0;
    let pc = p.with_clip_rect(lane);
    pc.text(pos2(x, lane.min.y + 6.0), Align2::LEFT_CENTER, "Sends", FontId::proportional(10.5), TOKENS.text_dim);
    let top = lane.min.y + 16.0;
    let per_col = (((lane.max.y - top) / ROW_H).floor() as usize).max(1);
    let at =
        |i: usize| Rect::from_min_size(pos2(x + (i / per_col) as f32 * (COL_W + 10.0), top + (i % per_col) as f32 * ROW_H), vec2(COL_W, ROW_H - 2.0));
    let mut i = 0;
    let mut menu = std::mem::take(&mut layout.menu);
    for (slot, snd) in t.mixer.sends.iter().enumerate() {
        let Some(snd) = snd else { continue };
        let row = crate::mixer::send_row(app, ui, s, t, slot, snd, at(i), lane, true, &mut menu);
        layout.sends.push(row);
        i += 1;
    }
    if i < t.mixer.sends.len() {
        layout.add_send = crate::mixer::add_send_row(app, ui, s, t, at(i), lane, &mut menu);
    }
    layout.menu = menu;
}
