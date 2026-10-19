//! MIDI input routing. One command changes any number of selected MIDI tracks atomically.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::MidiPort;

const ID: &str = "midi.input";

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "midi.input",
        "MIDI Input",
        [],
        None,
        "{track?|tracks?, port?: all|none|{backend,name,ordinal,id?}, channel?: all|1..16} — omitted fields keep their current values",
        has_tracks,
        set_input
    )]
}

fn device_text(p: &Value, key: &str, max: usize, allow_empty: bool) -> Result<String> {
    let value = p.get(key).and_then(Value::as_str).ok_or_else(|| bad(ID, format!("device `{key}` must be a string")))?;
    if (!allow_empty && value.is_empty()) || value.len() > max {
        return Err(bad(ID, format!("device `{key}` has an invalid length")));
    }
    Ok(value.to_owned())
}

fn port_param(value: &Value) -> Result<MidiPort> {
    match value.as_str() {
        Some("all") => Ok(MidiPort::All),
        Some("none") => Ok(MidiPort::None),
        _ if value.is_object() => {
            let backend = device_text(value, "backend", 64, false)?;
            let name = device_text(value, "name", 1024, false)?;
            let ordinal = value
                .get("ordinal")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| bad(ID, "device `ordinal` must be an integer in 0..4294967295"))?;
            let id = if value.get("id").is_some() { device_text(value, "id", 4096, true)? } else { String::new() };
            Ok(MidiPort::Device { backend, name, ordinal, id })
        }
        _ => Err(bad(ID, "`port` must be all, none, or a device object")),
    }
}

fn set_input(e: &mut Engine, p: &Value) -> Result<Value> {
    if p.get("tracks").is_some_and(|v| !v.is_array()) {
        return Err(bad(ID, "`tracks` must be an array of track ids or names"));
    }
    let mut tracks = tracks_required(e, ID, p)?;
    tracks.sort_by_key(|t| t.0);
    tracks.dedup();
    if tracks.iter().any(|id| e.session().track(*id).is_none_or(|t| !t.kind.is_midi())) {
        return Err(bad(ID, "MIDI input is available only on MIDI and instrument tracks"));
    }
    let port = p.get("port").map(port_param).transpose()?;
    let channel = p
        .get("channel")
        .map(|value| {
            if value.as_str() == Some("all") {
                Ok(None)
            } else {
                value
                    .as_u64()
                    .filter(|v| (1..=16).contains(v))
                    .map(|v| Some((v - 1) as u8))
                    .ok_or_else(|| bad(ID, "`channel` must be all or an integer in 1..16"))
            }
        })
        .transpose()?;
    if port.is_none() && channel.is_none() {
        return Err(bad(ID, "supply `port` or `channel`"));
    }
    for id in &tracks {
        if let Some(track) = e.session_mut().track_mut(*id) {
            if let Some(port) = &port {
                track.midi_input.port = port.clone();
            }
            if let Some(channel) = channel {
                track.midi_input.channel = channel;
            }
        }
    }
    Ok(json!({"tracks": tracks}))
}
