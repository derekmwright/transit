//! MIDI input menu, using only the Player's cached device status and engine commands.

use serde_json::{Value, json};
use soundcraft_model::{MidiInput, MidiPort, TrackId};
use soundcraft_playback::Player;

pub(crate) struct InputView {
    ports: Vec<(MidiPort, String)>,
    status: String,
    known: bool,
}

impl InputView {
    pub(crate) fn read(player: Option<&Player>) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(player) = player {
            return match player.midi_status() {
                Ok(status) => {
                    let ports = status
                        .inputs
                        .iter()
                        .map(|p| {
                            (
                                MidiPort::Device {
                                    backend: p.key.backend.as_str().into(),
                                    name: p.key.name.clone(),
                                    ordinal: p.key.ordinal,
                                    id: p.key.id.clone(),
                                },
                                p.display_name.clone(),
                            )
                        })
                        .collect();
                    let c = status.counts;
                    let mut detail = format!(
                        "{} open MIDI inputs · {} active routes\nEvents received: {} · delivered: {} · filtered: {}\nCanceled: {} · dropped: {} · refused: {} · recovered: {}",
                        status.open_inputs, status.routes, c.offered, c.admitted, c.filtered, c.cancelled, c.shed, c.refused, c.recovered
                    );
                    if let Some(error) = status.error {
                        detail.push_str(&format!("\nLast MIDI error: {error}"));
                    }
                    Self { ports, status: detail, known: true }
                }
                Err(error) => Self { ports: Vec::new(), status: format!("MIDI status unavailable: {error}"), known: false },
            };
        }
        let _ = player;
        let status = if cfg!(target_arch = "wasm32") {
            "Hardware MIDI input is not supported in this browser build."
        } else {
            "MIDI input is unavailable without an active audio device."
        };
        Self { ports: Vec::new(), status: status.into(), known: false }
    }

    fn same_port(&self, selected: &MidiPort, available: &MidiPort) -> bool {
        match (selected, available) {
            (MidiPort::Device { backend, name, ordinal, id }, MidiPort::Device { backend: b, name: n, ordinal: o, id: i }) => {
                backend == b
                    && if id.is_empty() {
                        let has_anonymous = self.ports.iter().any(|(port, _)| {
                            matches!(port,
                        MidiPort::Device { backend: candidate_backend, name: candidate_name, id: candidate_id, .. }
                        if candidate_backend == backend && candidate_name == name && candidate_id.is_empty())
                        });
                        name == n && ordinal == o && (i.is_empty() || !has_anonymous)
                    } else {
                        id == i
                    }
            }
            _ => selected == available,
        }
    }

    fn port_label(&self, port: &MidiPort) -> String {
        match port {
            MidiPort::All => "All MIDI inputs".into(),
            MidiPort::None => "No MIDI input".into(),
            MidiPort::Device { name, ordinal, .. } => {
                if let Some((_, label)) = self.ports.iter().find(|(p, _)| self.same_port(port, p)) {
                    return label.clone();
                }
                let suffix = if *ordinal == 0 { String::new() } else { format!(" #{}", u64::from(*ordinal) + 1) };
                format!("{name}{suffix} ({})", if self.known { "disconnected" } else { "unavailable" })
            }
        }
    }

    pub(crate) fn label(&self, input: &MidiInput) -> String {
        let channel = match input.channel {
            None => "All ch".into(),
            Some(c) if c < 16 => format!("Ch {}", c + 1),
            Some(_) => "Invalid channel".into(),
        };
        format!("{} · {channel}", self.port_label(&input.port))
    }

    /// Returns only the changed field, so choosing a port preserves the channel and vice versa.
    pub(crate) fn popup(&self, response: &egui::Response, track: TrackId, input: &MidiInput) -> Option<Value> {
        response.clone().on_hover_text(format!("{}\n{}\nArm this track to receive MIDI input.", self.label(input), self.status));
        let mut change = None;
        egui::Popup::menu(response).width(240.0).show(|ui| {
            ui.label("MIDI input");
            for (port, label) in [(MidiPort::All, "All MIDI inputs"), (MidiPort::None, "No MIDI input")] {
                if ui.selectable_label(input.port == port, label).clicked() {
                    change = Some(json!({"track": track.0, "port": if port == MidiPort::All { "all" } else { "none" }}));
                    ui.close();
                }
            }
            ui.separator();
            if let MidiPort::Device { .. } = &input.port
                && !self.ports.iter().any(|(p, _)| self.same_port(&input.port, p))
            {
                ui.add_enabled(false, egui::Button::new(self.port_label(&input.port)).selected(true));
            }
            egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                for (port, label) in &self.ports {
                    if ui.selectable_label(self.same_port(&input.port, port), label).clicked() {
                        if let MidiPort::Device { backend, name, ordinal, id } = port {
                            change = Some(json!({"track": track.0, "port": {"backend": backend, "name": name, "ordinal": ordinal, "id": id}}));
                        }
                        ui.close();
                    }
                }
            });
            if self.ports.is_empty() {
                ui.weak("No MIDI inputs available");
            }
            ui.separator();
            ui.menu_button("Channel", |ui| {
                if ui.selectable_label(input.channel.is_none(), "All channels").clicked() {
                    change = Some(json!({"track": track.0, "channel": "all"}));
                    ui.close();
                }
                for channel in 1u8..=16 {
                    if ui.selectable_label(input.channel == Some(channel - 1), format!("Channel {channel}")).clicked() {
                        change = Some(json!({"track": track.0, "channel": channel}));
                        ui.close();
                    }
                }
            });
            ui.separator();
            ui.add(egui::Label::new(egui::RichText::new(&self.status).weak()).wrap());
        });
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_selection_and_channel_remain_visible() {
        let view = InputView { ports: Vec::new(), status: String::new(), known: true };
        let input = MidiInput {
            port: MidiPort::Device { backend: "winmm".into(), name: "Keyboard".into(), ordinal: 1, id: "usb".into() },
            channel: Some(15),
        };
        assert_eq!(view.label(&input), "Keyboard #2 (disconnected) · Ch 16");
        let online = InputView {
            ports: vec![(MidiPort::Device { backend: "winmm".into(), name: "Renamed".into(), ordinal: 0, id: "usb".into() }, "Renamed".into())],
            ..view
        };
        assert_eq!(online.label(&input), "Renamed · Ch 16");
        assert!(InputView::read(None).ports.is_empty());
    }

    #[test]
    fn menu_picks_an_online_device_by_identity_and_keeps_the_channel() {
        let device = MidiPort::Device { backend: "fake".into(), name: "Keyboard".into(), ordinal: 1, id: "device-two".into() };
        let view = InputView { ports: vec![(device.clone(), "Keyboard #2".into())], status: "Test input".into(), known: true };
        let input = MidiInput { channel: Some(9), ..MidiInput::default() };
        let ctx = egui::Context::default();
        let mut frame = 0u32;
        let mut draw = |events| {
            frame += 1;
            let mut changed = None;
            let mut button = egui::Rect::NOTHING;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 700.0))),
                    time: Some(f64::from(frame) / 4.0),
                    events,
                    ..Default::default()
                },
                |ui| {
                    let response = ui.button("Input");
                    button = response.rect;
                    changed = view.popup(&response, TrackId(1), &input);
                },
            );
            output.textures_delta.clear();
            (button, changed, output)
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
        let (_, _, output) = draw(Vec::new());
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Keyboard #2" => Some(text.pos + text.galley.rect.center().to_vec2()),
                _ => None,
            })
            .expect("device menu item");
        let (_, changed, _) = draw(click(pos));
        let params = changed.expect("device selection");
        assert_eq!(params["port"]["id"], "device-two");
        assert!(params.get("channel").is_none());
        let mut engine = soundcraft_engine::Engine::default();
        let track = engine.session_mut().add_track(soundcraft_model::TrackKind::Instrument, soundcraft_model::ChannelFormat::Stereo, None);
        engine.session_mut().track_mut(track).unwrap().midi_input = input.clone();
        let mut params = params;
        params["track"] = json!(track.0);
        engine.execute("midi.input", &params).unwrap();
        assert_eq!(engine.session().track(track).unwrap().midi_input, MidiInput { port: device.clone(), channel: Some(9) });
        let (button, _, _) = draw(Vec::new());
        draw(click(button.center()));
        let (_, _, output) = draw(Vec::new());
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().starts_with("Channel") => Some(text.pos + text.galley.rect.center().to_vec2()),
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "channel submenu; visible text: {:?}",
                    output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) => Some(text.galley.text()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                )
            });
        draw(click(pos));
        let (_, _, output) = draw(Vec::new());
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Channel 16" => Some(text.pos + text.galley.rect.center().to_vec2()),
                _ => None,
            })
            .expect("last channel");
        let (_, changed, _) = draw(click(pos));
        let mut params = changed.expect("channel selection");
        assert_eq!(params["channel"], 16);
        assert!(params.get("port").is_none());
        params["track"] = json!(track.0);
        engine.execute("midi.input", &params).unwrap();
        assert_eq!(engine.session().track(track).unwrap().midi_input, MidiInput { port: device.clone(), channel: Some(15) });
        assert!(engine.undo());
        assert_eq!(engine.session().track(track).unwrap().midi_input, MidiInput { port: device, channel: Some(9) });
    }
}
