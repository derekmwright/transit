//! The transport's tempo and meter fields. The tempo drags (a third of a BPM per point, Shift
//! 0.01), the wheel nudges it by 1 BPM (Shift 0.01), double-click types it; the meter opens a small
//! popup. Both edit the event in effect at the playhead with `event.tempo` / `event.meter`, so a
//! session with one tempo edits that one and a later change edits that change. A whole drag or
//! wheel gesture is one undo step; a value that does not change touches nothing.

use crate::StudioApp;
use crate::icons::Icon;
use crate::theme::{RADIUS, TOKENS};
use egui::{Align2, Rect, Sense, Ui, pos2, vec2};
use serde_json::json;

pub const BPM_MIN: f64 = 20.0;
pub const BPM_MAX: f64 = 999.0;
/// Pointer travel for one BPM.
pub const PT_PER_BPM: f32 = 3.0;
pub const WHEEL_BPM: f64 = 1.0;
pub const WHEEL_BPM_FINE: f64 = 0.01;
pub const DENOMINATORS: [u32; 4] = [2, 4, 8, 16];
pub const NUMERATOR_MAX: u32 = 32;

#[derive(Debug, Clone)]
pub struct Edit {
    pub text: String,
    /// The tempo event it edits: the one in effect when the field opened.
    tick: i64,
    fresh: bool,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    tick: i64,
    raw: f64,
    /// The tempo and the undo length when the gesture began.
    start: f64,
    undo_len: usize,
}

#[derive(Debug, Clone, Copy)]
struct Wheel {
    tick: i64,
    rect: Rect,
    last: f64,
}

/// The meter popup: the draft is its text ("3/4").
#[derive(Debug, Clone)]
pub struct MeterPopup {
    pub text: String,
    /// The meter event it edits: the one in effect when the popup opened.
    tick: i64,
    fresh: bool,
}

#[derive(Debug, Default)]
pub struct TempoUi {
    pub edit: Option<Edit>,
    pub meter: Option<MeterPopup>,
    drag: Option<Drag>,
    wheel: Option<Wheel>,
    serial: u64,
    /// Escape and Enter this frame, taken before any other handler sees them ([`keys`]).
    keys: (bool, bool),
}

/// The tempo event in effect at the playhead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Governing {
    pub tick: i64,
    pub bpm: f64,
    pub bar: i64,
}

pub fn governing_tempo(app: &StudioApp) -> Governing {
    let s = app.engine.session();
    let tick = s.tempo.samples_to_ticks(app.position().max(0), s.sample_rate);
    let ev = s.tempo.tempos().iter().rev().find(|e| e.tick <= tick).or(s.tempo.tempos().first()).copied();
    let (t, bpm) = ev.map_or((0, 120.0), |e| (e.tick, e.bpm));
    Governing { tick: t, bpm, bar: s.tempo.bar_beat_at_tick(t).bar }
}

/// The meter event in effect at the playhead: (tick, numerator, denominator).
pub fn governing_meter(app: &StudioApp) -> (i64, u32, u32) {
    let s = app.engine.session();
    let tick = s.tempo.samples_to_ticks(app.position().max(0), s.sample_rate);
    let m = s.tempo.meter_at_tick(tick);
    (m.tick, m.numerator, m.denominator)
}

/// A typed tempo: "90", "93.5", "120 BPM"; clamped to 20..999 and rounded to two decimals.
pub fn parse_bpm(text: &str) -> Option<f64> {
    let lower = text.trim().to_ascii_lowercase();
    let t = lower.strip_suffix("bpm").unwrap_or(&lower).trim();
    let v: f64 = t.parse().ok()?;
    v.is_finite().then(|| round2(v.clamp(BPM_MIN, BPM_MAX)))
}

/// A typed meter: "3/4", "7/8".
pub fn parse_meter(text: &str) -> Option<(u32, u32)> {
    let (n, d) = text.trim().split_once('/')?;
    let (n, d): (u32, u32) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
    ((1..=NUMERATOR_MAX).contains(&n) && DENOMINATORS.contains(&d)).then_some((n, d))
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

impl StudioApp {
    /// Sample position of a tempo-map tick (the engine's `at`).
    fn tick_at(&self, tick: i64) -> i64 {
        if tick == 0 {
            return 0;
        }
        let s = self.engine.session();
        s.tempo.tick_to_samples(tick, s.sample_rate)
    }

    /// Set the tempo of the event at `tick` to `bpm`. `key` joins a gesture's undo step; without
    /// one the change is a step of its own. Returns whether anything changed.
    pub fn set_tempo_at(&mut self, tick: i64, bpm: f64, key: Option<&str>) -> bool {
        let bpm = round2(bpm.clamp(BPM_MIN, BPM_MAX));
        let cur = self.engine.session().tempo.tempo_at_tick(tick);
        if (bpm - cur).abs() < 0.005 {
            return false;
        }
        let params = json!({"bpm": bpm, "at": self.tick_at(tick)});
        let result = match key {
            Some(k) => self.engine.execute_merged("event.tempo", &params, k),
            None => {
                self.engine.end_merge();
                self.tempo_ui.serial += 1;
                let k = format!("tempo:set:{}", self.tempo_ui.serial);
                let result = self.engine.execute_merged("event.tempo", &params, &k);
                self.engine.end_merge();
                result
            }
        };
        // A refused change (a clip or loop the new tempo would squeeze away) says why.
        if let Err(e) = &result {
            self.status = format!("Tempo not changed: {e}");
        }
        result.is_ok()
    }

    /// Set the meter of the event at `tick`, as one undo step.
    pub fn set_meter_at(&mut self, tick: i64, num: u32, den: u32) -> bool {
        let m = self.engine.session().tempo.meter_at_tick(tick);
        if (m.numerator, m.denominator) == (num, den) {
            return false;
        }
        let params = json!({"numerator": num, "denominator": den, "at": self.tick_at(tick)});
        self.engine.end_merge();
        let ok = self.engine.execute("event.meter", &params).is_ok();
        self.engine.end_merge();
        ok
    }

    /// Ends a tempo wheel gesture when the pointer leaves or [`crate::fine::WHEEL_MERGE_S`] pass.
    /// True while one is still going.
    pub(crate) fn drive_tempo_wheel(&mut self, ctx: &egui::Context, now: f64) -> bool {
        let Some(w) = self.tempo_ui.wheel else { return false };
        let inside = ctx.input(|i| i.pointer.hover_pos()).is_some_and(|p| w.rect.contains(p));
        if inside && now - w.last <= crate::fine::WHEEL_MERGE_S {
            ctx.request_repaint_after_secs((crate::fine::WHEEL_MERGE_S - (now - w.last)).max(0.0) as f32 + 0.02);
            return true;
        }
        self.tempo_ui.wheel = None;
        self.engine.end_merge();
        false
    }

    /// A wheel nudge of the tempo in effect: `notches` of 1 BPM (Shift 0.01).
    pub fn wheel_tempo(&mut self, notches: f64, shift: bool, rect: Rect, now: f64) {
        let g = governing_tempo(self);
        let step = if shift { WHEEL_BPM_FINE } else { WHEEL_BPM };
        if self.tempo_ui.wheel.map(|w| w.tick) != Some(g.tick) {
            self.engine.end_merge();
        }
        self.tempo_ui.wheel = Some(Wheel { tick: g.tick, rect, last: now });
        self.set_tempo_at(g.tick, g.bpm + notches * step, Some(&format!("tempo:wheel:{}", g.tick)));
    }
}

/// While the tempo is typed or the meter popup is open, Escape and Enter belong to it: taken first
/// in the frame, before the arrangement's Escape (Select None) and Enter (Return to Zero) can.
pub fn keys(app: &mut StudioApp, ctx: &egui::Context) {
    use crate::shortcuts::{Action as A, pressed};
    if app.tempo_ui.edit.is_some() || app.tempo_ui.meter.is_some() {
        app.tempo_ui.keys = (pressed(ctx, A::ReadoutCancel), pressed(ctx, A::ReadoutApply));
    }
}

pub(crate) fn register(app: &mut StudioApp, name: &str, r: Rect) {
    app.arrange.layout.transport.insert(name.to_string(), [r.min.x, r.min.y, r.max.x, r.max.y]);
}

pub fn tempo_field(app: &mut StudioApp, ui: &mut Ui) {
    let g = governing_tempo(app);
    let editing = app.tempo_ui.edit.is_some();
    let sense = if editing { Sense::hover() } else { Sense::click_and_drag() };
    let (r, resp) = ui.allocate_exact_size(vec2(92.0, 26.0), sense);
    register(app, "tempo", r);
    if editing {
        edit_field(app, ui, r);
        return;
    }
    let hot = resp.hovered() || resp.dragged();
    ui.painter().rect_filled(r, RADIUS, if hot { TOKENS.button_hover } else { TOKENS.bg });
    let color = if hot { TOKENS.accent } else { TOKENS.text };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, format!("{:.2} BPM", g.bpm), crate::theme::mono(ui.ctx(), 13.0), color);
    let (mods, now) = ui.input(|i| (i.modifiers, i.time));
    if resp.drag_started() {
        app.engine.end_merge();
        app.tempo_ui.wheel = None;
        app.tempo_ui.drag = Some(Drag { tick: g.tick, raw: g.bpm, start: g.bpm, undo_len: app.engine.undo_history().len() });
    }
    if resp.dragged()
        && let Some(mut d) = app.tempo_ui.drag
    {
        let dy = ui.input(|i| {
            if resp.drag_started() {
                i.pointer.press_origin().zip(i.pointer.latest_pos()).map_or(0.0, |(a, b)| b.y - a.y)
            } else {
                i.pointer.delta().y
            }
        });
        let per_pt = if mods.shift { WHEEL_BPM_FINE } else { WHEEL_BPM } / f64::from(PT_PER_BPM);
        d.raw = (d.raw - f64::from(dy) * per_pt).clamp(BPM_MIN, BPM_MAX);
        app.tempo_ui.drag = Some(d);
        let v = if mods.shift { round2(d.raw) } else { d.raw.round() };
        app.set_tempo_at(d.tick, v, Some(&format!("tempo:drag:{}", d.tick)));
    }
    if resp.drag_stopped()
        && let Some(d) = app.tempo_ui.drag.take()
    {
        // A drag that ends where it began is no edit: take its step back.
        let now = app.engine.session().tempo.tempo_at_tick(d.tick);
        if (now - d.start).abs() < 0.005 && app.engine.undo_history().len() == d.undo_len + 1 {
            app.engine.undo();
        }
        app.engine.end_merge();
    }
    if resp.double_clicked() {
        app.engine.end_merge();
        app.tempo_ui.edit = Some(Edit { text: format!("{:.2}", g.bpm), tick: g.tick, fresh: true });
    }
    if resp.hovered() && app.tempo_ui.drag.is_none() {
        let (notches, shift) = ui.input(|i| {
            let mut n = 0.0;
            let mut shift = false;
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, modifiers, .. } = e {
                    let d = if delta.x.abs() > delta.y.abs() { delta.x } else { delta.y };
                    n += match unit {
                        egui::MouseWheelUnit::Point => d / 50.0,
                        _ => d,
                    };
                    shift |= modifiers.shift;
                }
            }
            (n, shift)
        });
        if notches != 0.0 {
            app.wheel_tempo(f64::from(notches), shift, r, now);
        }
    }
    let tip = if g.tick > 0 { format!("Tempo at bar {}", g.bar) } else { "Tempo".to_string() };
    resp.on_hover_text(format!("{tip}. Drag to change, Shift for fine, double-click to type, wheel nudges"));
}

/// The in-place field: Enter applies, Escape and losing focus cancel.
fn edit_field(app: &mut StudioApp, ui: &mut Ui, r: Rect) {
    let Some(mut e) = app.tempo_ui.edit.take() else { return };
    let id = egui::Id::new("tempo_edit");
    let (esc, enter) = std::mem::take(&mut app.tempo_ui.keys);
    if e.fresh {
        e.fresh = false;
        select_all(ui.ctx(), id, &e.text);
    }
    let field = egui::TextEdit::singleline(&mut e.text)
        .id(id)
        .font(crate::theme::mono(ui.ctx(), 13.0))
        .margin(vec2(3.0, 1.0))
        .horizontal_align(egui::Align::Center)
        .desired_width(r.width());
    let resp = ui.put(r, field);
    if esc {
        return;
    }
    if enter {
        match parse_bpm(&e.text) {
            Some(v) => {
                app.set_tempo_at(e.tick, v, None);
            }
            None => app.status = format!("\"{}\" is not a tempo", e.text.trim()),
        }
        return;
    }
    if resp.lost_focus() {
        return;
    }
    app.tempo_ui.edit = Some(e);
}

/// Focus the field with all of its text selected, so typing replaces it.
fn select_all(ctx: &egui::Context, id: egui::Id, text: &str) {
    let n = text.chars().count();
    let mut st = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
    st.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

pub fn meter_field(app: &mut StudioApp, ui: &mut Ui) {
    let (mtick, n, d) = governing_meter(app);
    let (r, resp) = ui.allocate_exact_size(vec2(40.0, 26.0), Sense::click());
    register(app, "meter", r);
    let open = app.tempo_ui.meter.is_some();
    let hot = resp.hovered() || open;
    ui.painter().rect_filled(r, RADIUS, if hot { TOKENS.button_hover } else { TOKENS.bg });
    let color = if hot { TOKENS.accent } else { TOKENS.text };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, format!("{n}/{d}"), crate::theme::mono(ui.ctx(), 13.0), color);
    if resp.clicked() {
        app.tempo_ui.meter = if open { None } else { Some(MeterPopup { text: format!("{n}/{d}"), tick: mtick, fresh: true }) };
    }
    resp.on_hover_text("Meter. Click to change");
    if app.tempo_ui.meter.is_some() {
        popup(app, ui.ctx(), r);
    }
}

/// A popup button: its label as text, or an icon when `icon` is given.
pub(crate) fn small_button(ui: &mut Ui, label: &str, icon: Option<Icon>, w: f32, on: bool) -> (Rect, bool) {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
    let bg = if on {
        TOKENS.accent.gamma_multiply(0.35)
    } else if resp.hovered() {
        TOKENS.button_hover
    } else {
        TOKENS.button
    };
    ui.painter().rect_filled(r, RADIUS, bg);
    match icon {
        Some(i) => crate::icons::paint_in(ui.painter(), r, i, TOKENS.text),
        None => {
            ui.painter().text(r.center(), Align2::CENTER_CENTER, label, crate::theme::mono(ui.ctx(), 13.0), TOKENS.text);
        }
    }
    (r, resp.clicked())
}

fn popup(app: &mut StudioApp, ctx: &egui::Context, field: Rect) {
    let Some(mut m) = app.tempo_ui.meter.take() else { return };
    let (esc, enter) = std::mem::take(&mut app.tempo_ui.keys);
    if esc {
        return;
    }
    let current = {
        let e = app.engine.session().tempo.meter_at_tick(m.tick);
        (e.numerator, e.denominator)
    };
    let draft = parse_meter(&m.text).unwrap_or(current);
    let id = egui::Id::new("meter_edit");
    let mut apply = enter && parse_meter(&m.text).is_some();
    let mut bad = false;
    let mut rects: Vec<(String, Rect)> = Vec::new();
    let area = egui::Area::new(egui::Id::new("meter_popup")).order(egui::Order::Foreground).fixed_pos(pos2(field.min.x, field.max.y + 4.0)).show(
        ctx,
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                ui.horizontal(|ui| {
                    // A new area is laid out once before it takes input: ask until the field has it.
                    if m.fresh {
                        select_all(ui.ctx(), id, &m.text);
                    }
                    let resp = ui.add(egui::TextEdit::singleline(&mut m.text).id(id).font(crate::theme::mono(ui.ctx(), 13.0)).desired_width(56.0));
                    if resp.has_focus() {
                        m.fresh = false;
                    }
                    rects.push(("meter_text".into(), resp.rect));
                    bad = enter && parse_meter(&m.text).is_none();
                    let (r, hit) = small_button(ui, "", Some(Icon::Minus), 24.0, false);
                    rects.push(("meter_dec".into(), r));
                    if hit {
                        m.text = format!("{}/{}", draft.0.saturating_sub(1).max(1), draft.1);
                    }
                    let (r, hit) = small_button(ui, "", Some(Icon::Plus), 24.0, false);
                    rects.push(("meter_inc".into(), r));
                    if hit {
                        m.text = format!("{}/{}", (draft.0 + 1).min(NUMERATOR_MAX), draft.1);
                    }
                });
                ui.horizontal(|ui| {
                    for den in DENOMINATORS {
                        let (r, hit) = small_button(ui, &format!("/{den}"), None, 32.0, draft.1 == den);
                        rects.push((format!("meter_den_{den}"), r));
                        if hit {
                            m.text = format!("{}/{den}", draft.0);
                        }
                    }
                    let (r, hit) = small_button(ui, "Set", None, 36.0, false);
                    rects.push(("meter_set".into(), r));
                    apply |= hit && parse_meter(&m.text).is_some();
                });
            });
        },
    );
    for (name, r) in rects {
        register(app, &name, r);
    }
    if bad {
        app.status = format!("\"{}\" is not a meter (1-32 over 2, 4, 8 or 16)", m.text.trim());
    }
    if apply {
        if let Some((n, d)) = parse_meter(&m.text) {
            app.set_meter_at(m.tick, n, d);
        }
        return;
    }
    // A press outside the popup and the field closes it.
    let outside =
        ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !field.contains(p)));
    if outside {
        return;
    }
    app.tempo_ui.meter = Some(m);
}
