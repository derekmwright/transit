//! Hardware inserts in the studio UI (ADR 0015 section 5, slice d part 2): the pairs a machine can
//! offer (measured ones first), the words a card and the I/O table use for a pair's state, and the
//! small picker the device chain's "+" menu opens for "Hardware Insert…".

use crate::StudioApp;
use crate::mixer::MenuItem;
use crate::theme::{SPACE, TOKENS};
use serde_json::json;
use soundcraft_mix::{HardwareReport, HardwareState};
use soundcraft_model::{IoDirection, IoMap, TrackId};

/// A send and a return path, with the round trip measured for them at the session rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub send: String,
    pub ret: String,
    pub frames: Option<u32>,
}

/// Milliseconds of `frames` at `rate`.
pub fn ms(frames: u32, rate: u32) -> f64 {
    f64::from(frames) * 1000.0 / f64::from(rate.max(1))
}

/// "612 frames (12.8 ms)".
pub fn frames_text(frames: u32, rate: u32) -> String {
    format!("{frames} frames ({:.1} ms)", ms(frames, rate))
}

/// Every send/return pair this machine's table can make: the measured ones first (as the table
/// lists them), then each remaining output path with each input path.
pub fn pairs(io: &IoMap, rate: u32) -> Vec<Pair> {
    let is = |name: &str, direction| io.path(name).is_some_and(|p| p.direction == direction);
    let mut out: Vec<Pair> = io
        .hardware_latency
        .iter()
        .filter(|m| m.rate == rate && is(&m.send, IoDirection::Output) && is(&m.ret, IoDirection::Input))
        .map(|m| Pair { send: m.send.clone(), ret: m.ret.clone(), frames: Some(m.round_trip_frames) })
        .collect();
    for s in io.paths_of(IoDirection::Output) {
        for r in io.paths_of(IoDirection::Input) {
            if !out.iter().any(|p| p.send.eq_ignore_ascii_case(&s.name) && p.ret.eq_ignore_ascii_case(&r.name)) {
                out.push(Pair { send: s.name.clone(), ret: r.name.clone(), frames: None });
            }
        }
    }
    out
}

/// What a device card says in place of the editor button.
pub fn state_text(state: &HardwareState, latency: Option<u32>, rate: u32) -> String {
    match state {
        HardwareState::Ready => latency.map_or_else(|| "Ready".to_string(), |f| format!("{:.1} ms", ms(f, rate))),
        HardwareState::Unmeasured => "Unmeasured".into(),
        HardwareState::NoInput => "No input".into(),
        HardwareState::MissingSend => "Missing send".into(),
        HardwareState::MissingReturn => "Missing return".into(),
        HardwareState::Disabled(why) => format!("Disabled: {why}"),
    }
}

/// "HW: Out 3-4 → In 3-4".
pub fn card_line(send: &str, ret: &str) -> String {
    format!("HW: {send} \u{2192} {ret}")
}

/// Why Measure is off, `None` when it can run.
pub fn measure_blocker(live: bool, duplex: bool, playing: bool) -> Option<&'static str> {
    if !live {
        Some("Measuring needs an open audio device.")
    } else if !duplex {
        Some("This device has no live input open: hardware inserts need a duplex device.")
    } else if playing {
        Some("Stop the transport to measure.")
    } else {
        None
    }
}

/// The picker as last drawn, for tests and `ui.inspect`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PickerLayout {
    pub open: bool,
    pub track: Option<u64>,
    /// One entry per pair: label as shown, value `send|ret`.
    pub rows: Vec<MenuItem>,
}

impl StudioApp {
    /// Every hardware insert of the session as this machine and device see it.
    pub fn hardware_reports(&self) -> Vec<HardwareReport> {
        let io = self.engine.io_map();
        let duplex = self.player.as_ref().is_some_and(|p| p.duplex());
        soundcraft_mix::hardware_report(self.engine.session(), io, io.outputs, io.inputs, duplex, self.engine.session().sample_rate.hz())
    }

    /// Why Measure is off right now.
    pub fn measure_blocker(&self) -> Option<&'static str> {
        let live = self.player.as_ref().is_some_and(|p| !p.silent);
        let duplex = self.player.as_ref().is_some_and(|p| p.duplex());
        #[cfg(test)]
        if self.audio_devices.fake_probe.is_some() {
            return measure_blocker(true, true, false);
        }
        measure_blocker(live, duplex, self.is_playing())
    }

    /// Put a hardware insert on `t`'s chain: after the last device, like any other.
    pub fn add_hardware_device(&mut self, t: TrackId, send: &str, ret: &str) -> Result<serde_json::Value, String> {
        let ins = self.engine.session().track(t).map(|tr| tr.mixer.inserts.clone()).unwrap_or_default();
        let next = ins.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
        let mut params = json!({"track": t.0, "hardware": {"send": send, "return": ret}});
        if next < ins.len()
            && let Some(p) = params.as_object_mut()
        {
            p.insert("slot".into(), json!(next));
        }
        self.run("mix.insert", params)
    }
}

impl StudioApp {
    /// Make the hardware insert in `slot` of `t` again (switching a disabled one on when its path is free).
    pub fn add_hardware_device_at(&mut self, t: TrackId, slot: usize, send: &str, ret: &str) -> Result<serde_json::Value, String> {
        self.run("mix.insert", json!({"track": t.0, "slot": slot, "hardware": {"send": send, "return": ret}}))
    }
}

/// The picker window of the track `app.mixer.hw_picker` names. A pick runs one `mix.insert` with
/// `hardware`; the engine's refusal (a path in use) reaches the status line.
pub(crate) fn picker(app: &mut StudioApp, ctx: &egui::Context) {
    let mut layout = PickerLayout::default();
    if let Some(tid) = app.mixer.hw_picker {
        let io = app.engine.io_map().clone();
        let rate = app.engine.session().sample_rate.hz();
        let list = pairs(&io, rate);
        let mut open = true;
        let mut pick: Option<Pair> = None;
        layout.open = true;
        layout.track = Some(tid.0);
        egui::Window::new("Hardware Insert").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.label(egui::RichText::new("Send the signal out one path and take it back on another.").color(TOKENS.text_dim).size(11.5));
            ui.add_space(SPACE);
            if list.is_empty() {
                ui.label("This device has no output and input paths to pair (Audio Devices \u{203a} I/O paths).");
            }
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                for p in &list {
                    let label = match p.frames {
                        Some(f) => format!("{} \u{2192} {}   {}", p.send, p.ret, frames_text(f, rate)),
                        None => format!("{} \u{2192} {}   not measured", p.send, p.ret),
                    };
                    let b = ui.button(&label);
                    layout.rows.push(MenuItem::at(ui, label, format!("{}|{}", p.send, p.ret), b.rect));
                    if b.clicked() {
                        pick = Some(p.clone());
                    }
                }
            });
        });
        if let Some(p) = pick {
            let _ = app.add_hardware_device(tid, &p.send, &p.ret);
            app.mixer.hw_picker = None;
        } else if !open {
            app.mixer.hw_picker = None;
        }
    }
    app.mixer.hw_picker_layout = layout;
}
