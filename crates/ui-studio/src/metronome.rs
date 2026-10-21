//! The transport's metronome: the click toggle with its beat flash, the right-click popup (level
//! and count-in bars), and the record count-in's plan and counter.
//!
//! The level and the count-in length are UI preferences, not the document: they never touch the
//! undo history. The level reaches the mixer through `Player::set_metronome_db` (one atomic store
//! per frame); the count-in is a phase of the audio thread (`Player::record_with_count_in`).

use crate::StudioApp;
use crate::icons::{self, Icon};
use crate::tempo_edit::{register, small_button};
use crate::theme::{TOKENS, mono};
use egui::{Align2, Rect, Sense, Ui, pos2, vec2};
use serde_json::json;
use soundcraft_playback::CountIn;
use soundcraft_time::Samples;

/// The popup's level range, in dB.
const DB_RANGE: (f32, f32) = (-24.0, 12.0);

impl StudioApp {
    /// The count-in Record plays first from `at`: `ui.count_in_bars` bars at the tempo and meter
    /// of the bar containing `at`, measured here in project samples. `None` when it is off.
    pub(crate) fn count_in_for(&self, at: Samples) -> Option<CountIn> {
        if !self.ui.count_in {
            return None;
        }
        let bars = i64::from(self.ui.count_in_bars.clamp(1, 16));
        let s = self.engine.session();
        let rate = s.sample_rate;
        let tick = s.tempo.samples_to_ticks(at.max(0), rate);
        let bar_tick = s.tempo.bar_start_tick(s.tempo.bar_beat_at_tick(tick).bar);
        let meter = s.tempo.meter_at_tick(bar_tick);
        let beat = (s.tempo.tick_to_samples(bar_tick + meter.ticks_per_beat(), rate) - s.tempo.tick_to_samples(bar_tick, rate)).max(1);
        let per_bar = meter.numerator.max(1);
        Some(CountIn { frames: beat.saturating_mul(i64::from(per_bar)).saturating_mul(bars), beat, per_bar })
    }

    /// A count-in under way: (samples left, whole length) in project samples.
    pub fn count_in_state(&self) -> Option<(Samples, Samples)> {
        match &self.player {
            Some(p) => p.count_in(),
            None => self.sim_count_in.map(|(left, _)| (left.max(1), self.count_in_plan.map_or(left, |p| p.frames))),
        }
    }

    /// `-2| 3`: bars to go and the beat being counted, while a count-in runs.
    pub fn count_in_label(&self) -> Option<String> {
        let (left, total) = self.count_in_state()?;
        let plan = self.count_in_plan?;
        let per_bar = i64::from(plan.per_bar);
        let beats = (total / plan.beat.max(1)).max(1);
        let index = ((total - left) / plan.beat.max(1)).clamp(0, beats - 1);
        Some(format!("-{}| {}", (beats - index + per_bar - 1) / per_bar, index % per_bar + 1))
    }

    /// Where the click is in its beat, 0 at the click to 1 at the next, and whether it is the bar's
    /// first beat: from the count-in's counter, else from the playhead while playing.
    pub fn beat_phase(&self) -> Option<(f32, bool)> {
        if let (Some((left, total)), Some(plan)) = (self.count_in_state(), self.count_in_plan) {
            let beat = plan.beat.max(1);
            let elapsed = (total - left).max(0);
            let first = (elapsed / beat) % i64::from(plan.per_bar.max(1)) == 0;
            return Some(((elapsed % beat) as f32 / beat as f32, first));
        }
        if !self.is_playing() {
            return None;
        }
        let s = self.engine.session();
        let tick = s.tempo.samples_to_ticks(self.position().max(0), s.sample_rate);
        let bb = s.tempo.bar_beat_at_tick(tick);
        let per_beat = s.tempo.meter_at_tick(tick).ticks_per_beat().max(1);
        Some((bb.tick as f32 / per_beat as f32, bb.beat == 1))
    }

    /// Hand the level to the player. One atomic store; safe to call every frame.
    pub(crate) fn push_metronome_level(&self) {
        if let Some(p) = &self.player {
            p.set_metronome_db(self.ui.metronome_db);
        }
    }

    fn set_count_in_bars(&mut self, bars: Option<u8>) {
        match bars {
            Some(b) => {
                self.ui.count_in = true;
                self.ui.count_in_bars = b;
            }
            None => self.ui.count_in = false,
        }
    }
}

/// The metronome button: left-click toggles Options > Click, right-click opens the popup. It
/// flashes on every beat while playing and counting in.
pub fn button(app: &mut StudioApp, ui: &mut Ui) {
    let on = app.engine.session().edit.click;
    let (r, resp) = ui.allocate_exact_size(vec2(34.0, 28.0), Sense::click());
    let bg = if on {
        TOKENS.accent.gamma_multiply(0.35)
    } else if resp.hovered() {
        TOKENS.button_hover
    } else {
        TOKENS.button
    };
    ui.painter().rect_filled(r, crate::theme::RADIUS, bg);
    if let Some((phase, first)) = app.beat_phase() {
        // A short decay from each beat; the bar's first beat is brighter.
        let glow = (1.0 - phase / 0.3).max(0.0) * if first { 0.9 } else { 0.5 };
        ui.painter().rect_filled(r, crate::theme::RADIUS, TOKENS.accent.gamma_multiply(glow));
    }
    icons::paint_in(ui.painter(), r, Icon::Metronome, if on { TOKENS.accent } else { TOKENS.text });
    app.arrange.layout.transport.insert("metronome".into(), [r.min.x, r.min.y, r.max.x, r.max.y]);
    app.arrange.layout.transport_icons.insert("metronome".into(), Icon::Metronome.name().into());
    if resp.clicked() {
        let _ = app.run("options.click", json!({"value": !on}));
    }
    if resp.secondary_clicked() {
        app.metro_popup = !app.metro_popup;
    }
    resp.on_hover_text("Metronome: click to turn the click on or off, right-click for level and count-in");
    if app.metro_popup {
        popup(app, ui.ctx(), r);
    }
}

fn popup(app: &mut StudioApp, ctx: &egui::Context, field: Rect) {
    let mut rects: Vec<(String, Rect)> = Vec::new();
    let (mut db, bars) = (app.ui.metronome_db, app.ui.count_in.then_some(app.ui.count_in_bars));
    let mut pick = None;
    let area = egui::Area::new(egui::Id::new("metronome_popup")).order(egui::Order::Foreground).fixed_pos(pos2(field.min.x, field.max.y + 4.0)).show(
        ctx,
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                ui.horizontal(|ui| {
                    ui.label("Level");
                    let (r, hit) = small_button(ui, "", Some(Icon::Minus), 24.0, false);
                    rects.push(("metro_level_dec".into(), r));
                    if hit {
                        db = (db - 1.0).max(DB_RANGE.0);
                    }
                    let (r, _) = ui.allocate_exact_size(vec2(58.0, 24.0), Sense::hover());
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, format!("{db:+.0} dB"), mono(ui.ctx(), 13.0), TOKENS.text);
                    let (r, hit) = small_button(ui, "", Some(Icon::Plus), 24.0, false);
                    rects.push(("metro_level_inc".into(), r));
                    if hit {
                        db = (db + 1.0).min(DB_RANGE.1);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Count-in");
                    for (name, label, value) in [("off", "Off", None), ("1", "1", Some(1u8)), ("2", "2", Some(2)), ("4", "4", Some(4))] {
                        let (r, hit) = small_button(ui, label, None, 34.0, bars == value);
                        rects.push((format!("metro_bars_{name}"), r));
                        if hit {
                            pick = Some(value);
                        }
                    }
                });
            });
        },
    );
    for (name, r) in rects {
        register(app, &name, r);
    }
    if db != app.ui.metronome_db {
        app.ui.metronome_db = db;
        app.push_metronome_level();
    }
    if let Some(value) = pick {
        app.set_count_in_bars(value);
    }
    // A press outside the popup and the button, or Escape, closes it.
    let outside =
        ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !field.contains(p)));
    if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.metro_popup = false;
    }
}
