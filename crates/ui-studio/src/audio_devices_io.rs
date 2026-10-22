//! Audio › Audio Devices, I/O paths section (ADR 0015 slice a): the channels the open device
//! has, and this machine's named paths over them (name, channels, width) for each direction, with
//! Main's path. The table is the engine's (`Engine::io_map`), read again every frame; every edit
//! is one `setup.io_map` command (no undo entry, session untouched) that the window runs through
//! `StudioApp::run`, so its refusals reach the status line. The only state kept here is what the
//! user is in the middle of typing or editing.

use crate::icons::{self, Icon};
use crate::mixer::MenuItem;
use crate::theme::{SPACE, TOKENS};
use serde_json::{Value, json};
use soundcraft_model::{IoDirection, IoMap, IoPath, MAX_PATH_WIDTH};

/// One edit of the path table: one `setup.io_map` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoEdit {
    Add { direction: IoDirection, name: String, channels: Vec<u16> },
    Rename { name: String, new_name: String },
    Remove { name: String },
    SetChannels { name: String, channels: Vec<u16> },
    SetMain { name: String },
}

impl IoEdit {
    /// The `setup.io_map` parameters (0-based device channels).
    pub fn params(&self) -> Value {
        match self {
            IoEdit::Add { direction, name, channels } => {
                let direction = if *direction == IoDirection::Input { "input" } else { "output" };
                json!({"action": "add_path", "direction": direction, "name": name, "channels": channels})
            }
            IoEdit::Rename { name, new_name } => json!({"action": "rename_path", "name": name, "new_name": new_name}),
            IoEdit::Remove { name } => json!({"action": "remove_path", "name": name}),
            IoEdit::SetChannels { name, channels } => json!({"action": "set_channels", "name": name, "channels": channels}),
            IoEdit::SetMain { name } => json!({"action": "set_main", "name": name}),
        }
    }
}

/// Device channels as people count them (1-based): "1", "3-4", "1-6", and "1-2, 5" when the path
/// is not contiguous.
pub fn channel_label(channels: &[u16]) -> String {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for &c in channels {
        match runs.last_mut() {
            Some((_, last)) if c == *last + 1 => *last = c,
            _ => runs.push((c, c)),
        }
    }
    let part = |(a, b): (u16, u16)| if a == b { format!("{}", a + 1) } else { format!("{}-{}", a + 1, b + 1) };
    runs.into_iter().map(part).collect::<Vec<_>>().join(", ")
}

/// `channels` with `c` added (kept in order) or removed.
pub fn toggled(channels: &[u16], c: u16) -> Vec<u16> {
    let mut next: Vec<u16> = channels.to_vec();
    if let Some(i) = next.iter().position(|&x| x == c) {
        next.remove(i);
    } else {
        next.push(c);
        next.sort_unstable();
    }
    next
}

/// The first channel no path of `direction` uses (0 when all are taken).
fn first_free(io: &IoMap, direction: IoDirection) -> u16 {
    let opened = if direction == IoDirection::Input { io.inputs } else { io.outputs };
    (0..opened).find(|c| !io.paths_of(direction).any(|p| p.channels.contains(c))).unwrap_or(0)
}

/// A sentence for each pair of output paths that share a device channel.
pub fn overlap_lines(io: &IoMap) -> Vec<String> {
    io.overlaps().into_iter().map(|(a, b, c)| format!("{a} and {b} share channel {} and add together there.", c + 1)).collect()
}

/// The status line for a path table that needs a look: routes naming paths this machine lacks,
/// and output paths that share a channel. `None` when both are fine.
pub fn warning_text(unresolved: &[String], overlaps: &[(String, String, u16)]) -> Option<String> {
    let mut parts = Vec::new();
    match unresolved.len() {
        0 => {}
        1 => parts.push("1 route not on this device".to_string()),
        n => parts.push(format!("{n} routes not on this device")),
    }
    if !overlaps.is_empty() {
        let mut names: Vec<&str> = Vec::new();
        for (a, b, _) in overlaps {
            for n in [a.as_str(), b.as_str()] {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        parts.push(format!("Output paths overlap: {}", names.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("  \u{b7}  "))
}

/// The note under the counts: WASAPI shared mode offers one channel count.
pub const WASAPI_NOTE: &str = "WASAPI shared mode offers only the system mix format's channel count; more channels need ASIO.";

pub fn is_wasapi(system: &str) -> bool {
    system.eq_ignore_ascii_case("WASAPI")
}

/// What the Hardware inserts subsection asks for this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardwareAction {
    /// Run `io.measure_hardware_latency` (Measure and Remeasure).
    Measure { send: String, ret: String },
    /// Stop the measurement that is running.
    Cancel,
    /// Run `io.clear_hardware_latency`.
    Clear { send: String, ret: String },
}

/// What the subsection showed, for `ui.inspect` and tests.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HardwareLayout {
    /// Why Measure is off; empty when it can run.
    pub blocked: String,
    pub send: String,
    pub ret: String,
    /// "Measuring… <n>/<total> clicks" while a measurement runs; empty otherwise.
    pub progress: String,
    /// "<n> frames (<x> ms) ±<m>" or the refusal of the last run.
    pub result: String,
    /// The measured pairs listed: "Out 3-4 → In 3-4: 612 frames (12.8 ms)".
    pub rows: Vec<String>,
}

/// What the user is typing or editing now (the table itself is the engine's).
#[derive(Default)]
pub struct IoEditor {
    /// The pair picked for Measure.
    hw_send: Option<String>,
    hw_ret: Option<String>,
    /// What the last measurement said.
    pub hw_result: String,
    /// A path being renamed: (its name, the text so far).
    rename: Option<(String, String)>,
    /// The path whose channels are being picked.
    editing: Option<String>,
    new_output: String,
    new_input: String,
}

impl IoEditor {
    /// An edit was accepted: drop the text it came from.
    pub fn done(&mut self, edit: &IoEdit) {
        match edit {
            IoEdit::Add { direction: IoDirection::Output, .. } => self.new_output.clear(),
            IoEdit::Add { direction: IoDirection::Input, .. } => self.new_input.clear(),
            IoEdit::Rename { .. } => self.rename = None,
            IoEdit::Remove { name } if self.editing.as_deref() == Some(name) => self.editing = None,
            _ => {}
        }
    }
}

fn dim(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(egui::RichText::new(text.into()).color(TOKENS.text_dim).size(11.5));
}

/// The names the open device gives its channels (ASIO `getChannelInfo`), in channel order; empty
/// where the backend does not report them.
#[derive(Debug, Clone, Default)]
pub struct ChannelNames {
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
}

impl ChannelNames {
    fn of(&self, direction: IoDirection) -> &[String] {
        match direction {
            IoDirection::Output => &self.outputs,
            IoDirection::Input => &self.inputs,
        }
    }
}

/// How the channel picker shows device channel `c` (0-based): its 1-based number, with the
/// device's name for it where there is one ("Analog In 12" as it is when it already ends in the
/// number, "1 Mic/Line" otherwise).
pub fn channel_text(c: u16, names: &[String]) -> String {
    let number = (c + 1).to_string();
    match names.get(usize::from(c)).map(|n| n.trim()).filter(|n| !n.is_empty()) {
        Some(name) if name.rsplit(' ').next() == Some(number.as_str()) => name.to_string(),
        Some(name) => format!("{number} {name}"),
        None => number,
    }
}

/// The section. Returns the edit the user made this frame, if any; controls are listed in `items`
/// (label shown, value the control) for `ui.inspect` and tests. `names` labels the channels.
pub fn section(ui: &mut egui::Ui, io: &IoMap, wasapi: bool, names: &ChannelNames, ed: &mut IoEditor, items: &mut Vec<MenuItem>) -> Option<IoEdit> {
    let mut edit = None;
    ui.separator();
    ui.label(egui::RichText::new("I/O paths").strong());
    dim(ui, format!("Channels open on this device: {} in, {} out.", io.inputs, io.outputs));
    if wasapi {
        let r = ui.label(egui::RichText::new(WASAPI_NOTE).color(TOKENS.text_dim).size(11.5));
        items.push(MenuItem::at(ui, WASAPI_NOTE.into(), "io_wasapi_note".into(), r.rect));
    }
    let outputs: Vec<&IoPath> = io.paths_of(IoDirection::Output).collect();
    ui.horizontal(|ui| {
        ui.label("Main plays on");
        let combo = egui::ComboBox::from_id_salt("io_main").selected_text(&io.main).show_ui(ui, |ui| {
            for p in &outputs {
                let b = crate::menus::check_button(ui, p.name.eq_ignore_ascii_case(&io.main), &p.name);
                items.push(MenuItem::at(ui, p.name.clone(), "io_main_entry".into(), b.rect));
                if b.clicked() {
                    edit = Some(IoEdit::SetMain { name: p.name.clone() });
                }
            }
        });
        items.push(MenuItem::at(ui, io.main.clone(), "io_main".into(), combo.response.rect));
    });
    for line in overlap_lines(io) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(icons::ICON, icons::ICON), egui::Sense::hover());
            icons::paint(ui.painter(), r.center(), Icon::Warning, 14.0, TOKENS.record);
            ui.label(egui::RichText::new(line).color(TOKENS.text).size(11.5));
        });
    }
    for (direction, title) in [(IoDirection::Output, "Outputs"), (IoDirection::Input, "Inputs")] {
        let opened = if direction == IoDirection::Input { io.inputs } else { io.outputs };
        ui.add_space(SPACE);
        ui.label(egui::RichText::new(title).color(TOKENS.text_dim));
        egui::ScrollArea::vertical().id_salt(("io_paths", title)).max_height(100.0).show(ui, |ui| {
            egui::Grid::new(("io_paths_grid", title)).num_columns(4).spacing([SPACE, SPACE / 2.0]).show(ui, |ui| {
                for p in io.paths_of(direction) {
                    path_row(ui, io, p, ed, items, &mut edit);
                    ui.end_row();
                }
            });
        });
        if let Some(name) = ed.editing.clone()
            && let Some(p) = io.path(&name).filter(|p| p.direction == direction)
        {
            ui.horizontal_wrapped(|ui| {
                dim(ui, format!("{} channels (1 to {MAX_PATH_WIDTH}):", p.name));
                for c in 0..opened {
                    let text = channel_text(c, names.of(direction));
                    let b = ui.selectable_label(p.channels.contains(&c), &text);
                    items.push(MenuItem::at(ui, text, format!("io_channel:{}", p.name), b.rect));
                    if b.clicked() {
                        edit = Some(IoEdit::SetChannels { name: p.name.clone(), channels: toggled(&p.channels, c) });
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            let buf = if direction == IoDirection::Input { &mut ed.new_input } else { &mut ed.new_output };
            ui.add(egui::TextEdit::singleline(buf).hint_text("New path name").desired_width(160.0));
            let b = ui.add_enabled(opened > 0, egui::Button::new("Add"));
            let key = if direction == IoDirection::Input { "io_add:input" } else { "io_add:output" };
            items.push(MenuItem::at(ui, "Add".into(), key.into(), b.rect));
            if b.on_hover_text("A path on the first free channel; pick its channels with Channels.").clicked() {
                edit = Some(IoEdit::Add { direction, name: buf.clone(), channels: vec![first_free(io, direction)] });
            }
        });
    }
    edit
}

/// The "Hardware inserts" subsection: pick a send path and a return path and Measure, then the
/// measured table with Remeasure and Clear. `blocked` is why Measure is off (no duplex device, the
/// transport running), when it is. Controls go to `items`; the layout says what was shown.
pub fn hardware_section(
    ui: &mut egui::Ui,
    io: &IoMap,
    rate: u32,
    blocked: Option<&str>,
    progress: Option<(u32, u32)>,
    ed: &mut IoEditor,
    items: &mut Vec<MenuItem>,
) -> (Option<HardwareAction>, HardwareLayout) {
    use crate::hardware_ui::frames_text;
    let mut action = None;
    ui.separator();
    ui.label(egui::RichText::new("Hardware inserts").strong());
    dim(ui, "Time the round trip through outboard gear: a click plays on the send path and is found again on the return path.");
    let outputs: Vec<&str> = io.paths_of(IoDirection::Output).map(|p| p.name.as_str()).collect();
    let inputs: Vec<&str> = io.paths_of(IoDirection::Input).map(|p| p.name.as_str()).collect();
    // A pick that no longer names a path is dropped.
    if ed.hw_send.as_deref().is_some_and(|n| !outputs.contains(&n)) {
        ed.hw_send = None;
    }
    if ed.hw_ret.as_deref().is_some_and(|n| !inputs.contains(&n)) {
        ed.hw_ret = None;
    }
    let running = progress.is_some();
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!running, |ui| {
            for (label, key, names, pick) in [("Send", "hw_send", &outputs, &mut ed.hw_send), ("Return", "hw_ret", &inputs, &mut ed.hw_ret)] {
                ui.label(label);
                let combo = egui::ComboBox::from_id_salt(key).selected_text(pick.as_deref().unwrap_or("Choose…")).show_ui(ui, |ui| {
                    for n in names {
                        let b = crate::menus::check_button(ui, pick.as_deref() == Some(*n), *n);
                        items.push(MenuItem::at(ui, (*n).to_string(), format!("{key}_entry"), b.rect));
                        if b.clicked() {
                            *pick = Some((*n).to_string());
                        }
                    }
                });
                items.push(MenuItem::at(ui, pick.clone().unwrap_or_default(), key.into(), combo.response.rect));
            }
        });
        let ready = ed.hw_send.is_some() && ed.hw_ret.is_some();
        let b = ui.add_enabled(ready && blocked.is_none() && !running, egui::Button::new("Measure"));
        items.push(MenuItem::at(ui, "Measure".into(), "hw_measure".into(), b.rect));
        let tip = blocked.unwrap_or(if running {
            "A measurement is running."
        } else if ready {
            "Play a click out the send path and time it on the return path."
        } else {
            "Choose a send path and a return path."
        });
        if b.on_hover_text(tip).on_disabled_hover_text(tip).clicked()
            && let (Some(send), Some(ret)) = (ed.hw_send.clone(), ed.hw_ret.clone())
        {
            action = Some(HardwareAction::Measure { send, ret });
        }
    });
    let mut progress_text = String::new();
    if let Some((sent, total)) = progress {
        progress_text = format!("Measuring\u{2026} {sent}/{total} clicks");
        ui.horizontal(|ui| {
            let r = ui.label(egui::RichText::new(&progress_text).color(TOKENS.text).size(11.5));
            items.push(MenuItem::at(ui, progress_text.clone(), "hw_progress".into(), r.rect));
            let b = ui.button("Cancel");
            items.push(MenuItem::at(ui, "Cancel".into(), "hw_cancel".into(), b.rect));
            if b.on_hover_text("Stop measuring; nothing is stored.").clicked() {
                action = Some(HardwareAction::Cancel);
            }
        });
    }
    if let Some(why) = blocked {
        dim(ui, why);
    }
    if !ed.hw_result.is_empty() {
        let r = ui.label(egui::RichText::new(&ed.hw_result).color(TOKENS.text).size(11.5));
        items.push(MenuItem::at(ui, ed.hw_result.clone(), "hw_result".into(), r.rect));
    }
    let mut layout = HardwareLayout {
        blocked: blocked.unwrap_or_default().to_string(),
        progress: progress_text,
        send: ed.hw_send.clone().unwrap_or_default(),
        ret: ed.hw_ret.clone().unwrap_or_default(),
        result: ed.hw_result.clone(),
        rows: Vec::new(),
    };
    let measured: Vec<_> = io.hardware_latency.iter().filter(|m| m.rate == rate).collect();
    if measured.is_empty() {
        dim(ui, "Nothing measured yet.");
    } else {
        egui::Grid::new("hw_latency_grid").num_columns(3).spacing([SPACE, SPACE / 2.0]).show(ui, |ui| {
            for m in measured {
                let pair = format!("{} → {}", m.send, m.ret);
                ui.label(&pair);
                ui.label(frames_text(m.round_trip_frames, rate));
                layout.rows.push(format!("{pair}: {}", frames_text(m.round_trip_frames, rate)));
                ui.horizontal(|ui| {
                    let b = ui.add_enabled(blocked.is_none() && !running, egui::Button::new("Remeasure").small());
                    items.push(MenuItem::at(ui, "Remeasure".into(), format!("hw_remeasure:{pair}"), b.rect));
                    if b.clicked() {
                        action = Some(HardwareAction::Measure { send: m.send.clone(), ret: m.ret.clone() });
                    }
                    let b = ui.small_button("Clear");
                    items.push(MenuItem::at(ui, "Clear".into(), format!("hw_clear:{pair}"), b.rect));
                    if b.clicked() {
                        action = Some(HardwareAction::Clear { send: m.send.clone(), ret: m.ret.clone() });
                    }
                });
                ui.end_row();
            }
        });
    }
    (action, layout)
}

/// What a finished measurement says: "612 frames (12.8 ms) ±2".
pub fn measured_text(frames: u32, spread: u32, rate: u32) -> String {
    format!("{} ±{spread}", crate::hardware_ui::frames_text(frames, rate))
}

fn path_row(ui: &mut egui::Ui, io: &IoMap, p: &IoPath, ed: &mut IoEditor, items: &mut Vec<MenuItem>, edit: &mut Option<IoEdit>) {
    if let Some((_, buf)) = ed.rename.as_mut().filter(|(n, _)| *n == p.name) {
        let r = ui.add(egui::TextEdit::singleline(buf).desired_width(130.0));
        r.request_focus();
        if r.lost_focus() {
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) && *buf != p.name {
                *edit = Some(IoEdit::Rename { name: p.name.clone(), new_name: buf.clone() });
            } else {
                ed.rename = None;
            }
        }
    } else {
        let main = p.name.eq_ignore_ascii_case(&io.main);
        ui.label(if main { format!("{} (Main)", p.name) } else { p.name.clone() });
    }
    ui.label(channel_label(&p.channels));
    ui.label(format!("{} ch", p.channels.len()));
    ui.horizontal(|ui| {
        let b = ui.small_button("Channels");
        items.push(MenuItem::at(ui, "Channels".into(), format!("io_channels:{}", p.name), b.rect));
        if b.clicked() {
            ed.editing = if ed.editing.as_deref() == Some(p.name.as_str()) { None } else { Some(p.name.clone()) };
        }
        let b = ui.small_button("Rename");
        items.push(MenuItem::at(ui, "Rename".into(), format!("io_rename:{}", p.name), b.rect));
        if b.clicked() {
            ed.rename = Some((p.name.clone(), p.name.clone()));
        }
        let can = !p.name.eq_ignore_ascii_case(&io.main);
        let b = ui.add_enabled(can, egui::Button::new("Remove").small());
        items.push(MenuItem::at(ui, "Remove".into(), format!("io_remove:{}", p.name), b.rect));
        if b.on_disabled_hover_text("Main plays here: choose another path for Main first.").clicked() {
            *edit = Some(IoEdit::Remove { name: p.name.clone() });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_read_one_based_and_contiguous_runs_collapse() {
        assert_eq!(channel_label(&[0]), "1");
        assert_eq!(channel_label(&[2, 3]), "3-4");
        assert_eq!(channel_label(&[0, 1, 2, 3, 4, 5]), "1-6");
        assert_eq!(channel_label(&[0, 1, 4]), "1-2, 5");
        assert_eq!(channel_label(&[3, 1]), "4, 2", "the path's own order is kept");
        assert_eq!(channel_label(&[]), "");
    }

    #[test]
    fn toggling_a_channel_keeps_the_set_in_order() {
        assert_eq!(toggled(&[0, 1], 3), [0, 1, 3]);
        assert_eq!(toggled(&[0, 1, 3], 1), [0, 3]);
        assert_eq!(toggled(&[2], 0), [0, 2]);
    }

    #[test]
    fn each_edit_is_the_setup_io_map_command_it_names() {
        let add = IoEdit::Add { direction: IoDirection::Input, name: "Mic".into(), channels: vec![2] };
        assert_eq!(add.params(), json!({"action": "add_path", "direction": "input", "name": "Mic", "channels": [2]}));
        assert_eq!(IoEdit::SetMain { name: "Out 3-4".into() }.params(), json!({"action": "set_main", "name": "Out 3-4"}));
        assert_eq!(IoEdit::Remove { name: "Cue".into() }.params(), json!({"action": "remove_path", "name": "Cue"}));
    }

    #[test]
    fn a_new_path_starts_on_the_first_free_channel_and_overlaps_are_said() {
        let mut io = IoMap::for_device("ASIO", "x", 2, 4);
        assert_eq!(first_free(&io, IoDirection::Output), 0, "the defaults cover every channel");
        io.remove_path("Out 3-4").unwrap();
        assert_eq!(first_free(&io, IoDirection::Output), 2);
        assert!(overlap_lines(&io).is_empty());
        io.add_path(IoPath { name: "Cue".into(), direction: IoDirection::Output, channels: vec![1], format: soundcraft_model::ChannelFormat::Mono })
            .unwrap();
        assert_eq!(overlap_lines(&io), ["Out 1-2 and Cue share channel 2 and add together there."]);
        assert!(is_wasapi("wasapi") && !is_wasapi("ASIO"));
    }
}
