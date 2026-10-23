//! MIDI output destination picker (#61), using only the Player's cached output status and the
//! `midi.output` command. Drawn as the small "Out" button at the end of a MIDI strip's input row;
//! it turns red with a warning when the destination's device is missing.
//!
//! TODO(#61): header picker. Once `arrange.rs` is free, call `OutputView::header_badge` from
//! `arrange::track_header` next to the kind subtitle, so "device missing" shows on the track
//! header too. Until then the mixer strip's Out button and its tooltip carry it.

use serde_json::{Value, json};
use soundcraft_model::{MAX_MIDI_OUT_OFFSET_MS, MidiOutChannel, MidiOutPort, MidiOutput, TrackId};
use soundcraft_playback::Player;
#[cfg(not(target_arch = "wasm32"))]
use soundcraft_playback::player::midi_out::DestinationState;

/// What the picker knows about one track's destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    /// Internal, or the output thread has not reported it yet.
    Unknown,
    Connected,
    Missing,
    Failed(String),
}

pub(crate) struct OutputView {
    ports: Vec<(MidiOutPort, String)>,
    states: Vec<(TrackId, State)>,
    status: String,
}

impl OutputView {
    pub(crate) fn read(player: Option<&Player>) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(player) = player {
            return match player.midi_status() {
                Ok(status) => {
                    let out = status.output;
                    let ports = out
                        .ports
                        .iter()
                        .map(|p| {
                            (
                                MidiOutPort {
                                    backend: p.key.backend.as_str().into(),
                                    name: p.key.name.clone(),
                                    ordinal: p.key.ordinal,
                                    id: p.key.id.clone(),
                                },
                                p.display_name.clone(),
                            )
                        })
                        .collect();
                    let states = out
                        .tracks
                        .iter()
                        .map(|t| {
                            let state = match &t.state {
                                DestinationState::Pending => State::Unknown,
                                DestinationState::Connected => State::Connected,
                                DestinationState::Missing => State::Missing,
                                DestinationState::Failed(why) => State::Failed(why.clone()),
                            };
                            (t.track, state)
                        })
                        .collect();
                    let mut detail = format!(
                        "MIDI output: {} sent · {} late (max {:.1} ms) · {} dropped · {} not sent (device missing)",
                        out.sent,
                        out.late,
                        out.max_late_ns as f64 / 1e6,
                        out.dropped,
                        out.unsent
                    );
                    if let Some(error) = out.error {
                        detail.push_str(&format!("\nLast MIDI output error: {error}"));
                    }
                    Self { ports, states, status: detail }
                }
                Err(error) => Self { ports: Vec::new(), states: Vec::new(), status: format!("MIDI status unavailable: {error}") },
            };
        }
        let _ = player;
        let status = if cfg!(target_arch = "wasm32") {
            "Hardware MIDI output is not supported in this browser build."
        } else {
            "MIDI output is unavailable without an active audio device."
        };
        Self { ports: Vec::new(), states: Vec::new(), status: status.into() }
    }

    pub(crate) fn state(&self, track: TrackId) -> State {
        self.states.iter().find(|(t, _)| *t == track).map_or(State::Unknown, |(_, s)| s.clone())
    }

    /// The destination is external and its device is not usable now.
    pub(crate) fn missing(&self, track: TrackId, output: &MidiOutput) -> bool {
        output.is_external() && matches!(self.state(track), State::Missing | State::Failed(_))
    }

    /// The status text for `track` when its device is missing (the strip button's label now,
    /// the track header's once the TODO above lands); `None` when there is nothing to say.
    pub(crate) fn header_badge(&self, track: TrackId, output: &MidiOutput) -> Option<String> {
        self.missing(track, output).then(|| format!("{} (device missing)", self.port_label(output)))
    }

    fn port_label(&self, output: &MidiOutput) -> String {
        match output {
            MidiOutput::Internal => "Instrument".into(),
            MidiOutput::External { port, .. } => {
                if let Some((_, label)) = self.ports.iter().find(|(p, _)| same_port(port, p, &self.ports)) {
                    return label.clone();
                }
                if port.ordinal == 0 { port.name.clone() } else { format!("{} #{}", port.name, u64::from(port.ordinal) + 1) }
            }
        }
    }

    pub(crate) fn label(&self, track: TrackId, output: &MidiOutput) -> String {
        match output {
            MidiOutput::Internal => "MIDI to the track's instrument".into(),
            MidiOutput::External { channel, .. } => {
                let channel = match channel {
                    MidiOutChannel::AsRecorded => "channels as recorded".to_string(),
                    MidiOutChannel::Channel(c) => format!("Ch {}", c + 1),
                };
                let offset = output.offset_ms();
                let offset = if offset == 0.0 { String::new() } else { format!(" · {offset:+.1} ms") };
                let state = match self.state(track) {
                    State::Missing => " (device missing)".to_string(),
                    State::Failed(why) => format!(" (cannot open: {why})"),
                    State::Connected | State::Unknown => String::new(),
                };
                format!("MIDI to {} · {channel}{offset}{state}", self.port_label(output))
            }
        }
    }

    /// Returns only the changed field, so choosing a port keeps the channel and offset.
    pub(crate) fn popup(&self, response: &egui::Response, track: TrackId, output: &MidiOutput) -> Option<Value> {
        response.clone().on_hover_text(format!("{}\n{}", self.label(track, output), self.status));
        let mut change = None;
        egui::Popup::menu(response).width(260.0).show(|ui| {
            ui.label("MIDI output");
            if ui.selectable_label(output.is_internal(), "Instrument (internal)").clicked() {
                change = Some(json!({"track": track.0, "port": "internal"}));
                ui.close();
            }
            ui.separator();
            if let MidiOutput::External { port, .. } = output
                && !self.ports.iter().any(|(p, _)| same_port(port, p, &self.ports))
            {
                ui.add_enabled(false, egui::Button::new(format!("{} (device missing)", self.port_label(output))).selected(true));
            }
            egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                for (port, label) in &self.ports {
                    let selected = matches!(output, MidiOutput::External { port: current, .. } if same_port(current, port, &self.ports));
                    if ui.selectable_label(selected, label).clicked() {
                        change = Some(
                            json!({"track": track.0, "port": {"backend": port.backend, "name": port.name, "ordinal": port.ordinal, "id": port.id}}),
                        );
                        ui.close();
                    }
                }
            });
            if self.ports.is_empty() {
                ui.weak("No MIDI outputs available");
            }
            if let MidiOutput::External { channel, .. } = output {
                ui.separator();
                ui.menu_button("Channel", |ui| {
                    if ui.selectable_label(*channel == MidiOutChannel::AsRecorded, "As recorded").clicked() {
                        change = Some(json!({"track": track.0, "channel": "as_recorded"}));
                        ui.close();
                    }
                    for c in 1u8..=16 {
                        if ui.selectable_label(*channel == MidiOutChannel::Channel(c - 1), format!("Channel {c}")).clicked() {
                            change = Some(json!({"track": track.0, "channel": c}));
                            ui.close();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Offset");
                    // Edited locally; one command (one undo step) when the drag or typing ends.
                    let id = ui.id().with(("midi-out-offset", track.0));
                    let mut value = ui.data(|d| d.get_temp::<f32>(id)).unwrap_or_else(|| output.offset_ms());
                    let response = ui.add(
                        egui::DragValue::new(&mut value)
                            .range(-MAX_MIDI_OUT_OFFSET_MS..=MAX_MIDI_OUT_OFFSET_MS)
                            .speed(0.1)
                            .fixed_decimals(1)
                            .suffix(" ms"),
                    );
                    if response.changed() {
                        ui.data_mut(|d| d.insert_temp(id, value));
                    }
                    if (response.drag_stopped() || response.lost_focus()) && ui.data(|d| d.get_temp::<f32>(id)).is_some() {
                        ui.data_mut(|d| d.remove::<f32>(id));
                        if (value - output.offset_ms()).abs() > f32::EPSILON {
                            change = Some(json!({"track": track.0, "offset_ms": value}));
                        }
                    }
                });
                ui.weak("Positive sends later, negative earlier, than the audio.");
            }
            ui.separator();
            ui.add(egui::Label::new(egui::RichText::new(&self.status).weak()).wrap());
        });
        change
    }
}

/// The same identity rule as MIDI inputs: a nonempty id decides; otherwise name and ordinal,
/// where an anonymous port of that name takes precedence over identified namesakes.
fn same_port(selected: &MidiOutPort, available: &MidiOutPort, ports: &[(MidiOutPort, String)]) -> bool {
    if selected.backend != available.backend {
        return false;
    }
    if !selected.id.is_empty() {
        return selected.id == available.id;
    }
    let anonymous = ports.iter().any(|(p, _)| p.backend == selected.backend && p.name == selected.name && p.id.is_empty());
    selected.name == available.name && selected.ordinal == available.ordinal && (available.id.is_empty() || !anonymous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn external(id: &str) -> MidiOutput {
        MidiOutput::External {
            port: MidiOutPort { backend: "fake".into(), name: "Synth OUT".into(), ordinal: 0, id: id.into() },
            channel: MidiOutChannel::Channel(1),
            offset_ms: 2.5,
        }
    }

    #[test]
    fn a_missing_device_shows_in_the_label_and_the_header_badge() {
        let view = OutputView { ports: Vec::new(), states: vec![(TrackId(3), State::Missing)], status: String::new() };
        let out = external("usb");
        assert!(view.missing(TrackId(3), &out));
        assert_eq!(view.label(TrackId(3), &out), "MIDI to Synth OUT · Ch 2 · +2.5 ms (device missing)");
        assert_eq!(view.header_badge(TrackId(3), &out).as_deref(), Some("Synth OUT (device missing)"));
        assert!(!view.missing(TrackId(3), &MidiOutput::Internal));
        assert_eq!(view.header_badge(TrackId(4), &out), None);
        assert!(OutputView::read(None).ports.is_empty());
    }

    #[test]
    fn the_menu_picks_a_port_by_identity_and_the_command_keeps_channel_and_offset() {
        let port = MidiOutPort { backend: "fake".into(), name: "Synth OUT".into(), ordinal: 0, id: "two".into() };
        let view = OutputView { ports: vec![(port.clone(), "Synth OUT".into())], states: Vec::new(), status: "Test".into() };
        let output = external("one");
        let ctx = egui::Context::default();
        let mut frame = 0u32;
        let mut draw = |events| {
            frame += 1;
            let mut changed = None;
            let mut button = egui::Rect::NOTHING;
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 700.0))),
                    time: Some(f64::from(frame) / 4.0),
                    events,
                    ..Default::default()
                },
                |ui| {
                    let response = ui.button("Out");
                    button = response.rect;
                    changed = view.popup(&response, TrackId(1), &output);
                },
            );
            out.textures_delta.clear();
            (button, changed, out)
        };
        let click = |pos| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE },
            ]
        };
        let (button, _, _) = draw(Vec::new());
        draw(Vec::new());
        draw(click(button.center()));
        let (_, _, shown) = draw(Vec::new());
        let texts: Vec<String> = shown
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t == "Synth OUT (device missing)"), "the selected but absent port stays visible: {texts:?}");
        let pos = shown
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == "Synth OUT" => Some(t.pos + t.galley.rect.center().to_vec2()),
                _ => None,
            })
            .expect("the listed port");
        let (_, changed, _) = draw(click(pos));
        let mut params = changed.expect("a port choice");
        assert_eq!(params["port"]["id"], "two");
        assert!(params.get("channel").is_none() && params.get("offset_ms").is_none());
        let mut engine = soundcraft_engine::Engine::default();
        let track = engine.session_mut().add_track(soundcraft_model::TrackKind::Midi, soundcraft_model::ChannelFormat::Mono, None);
        engine.session_mut().track_mut(track).unwrap().midi_output = output.clone();
        params["track"] = json!(track.0);
        engine.execute("midi.output", &params).unwrap();
        assert_eq!(
            engine.session().track(track).unwrap().midi_output,
            MidiOutput::External { port, channel: MidiOutChannel::Channel(1), offset_ms: 2.5 }
        );
    }
}
