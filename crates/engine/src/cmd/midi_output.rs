//! MIDI output destination (#61). One command changes any number of selected MIDI tracks
//! atomically; it is a document edit (saved with the session, one undo step).

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{MAX_MIDI_OUT_OFFSET_MS, MidiOutChannel, MidiOutPort, MidiOutput};

const ID: &str = "midi.output";

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "midi.output",
        "MIDI Output",
        [],
        None,
        "{track?|tracks?, port?: internal|{backend,name,ordinal?,id?}, channel?: as_recorded|1..16, offset_ms?: -500..500} — omitted fields keep their current values; channel and offset need an external port",
        has_tracks,
        set_output
    )]
}

fn text(p: &Value, key: &str, max: usize, allow_empty: bool) -> Result<String> {
    let value = p.get(key).and_then(Value::as_str).ok_or_else(|| bad(ID, format!("port `{key}` must be a string")))?;
    if (!allow_empty && value.is_empty()) || value.len() > max {
        return Err(bad(ID, format!("port `{key}` has an invalid length")));
    }
    Ok(value.to_owned())
}

/// `None` is the internal destination.
fn port_param(value: &Value) -> Result<Option<MidiOutPort>> {
    match value.as_str() {
        Some("internal") => Ok(None),
        _ if value.is_object() => {
            let backend = text(value, "backend", 64, false)?;
            let name = text(value, "name", 1024, false)?;
            let ordinal = match value.get("ordinal") {
                None => 0,
                Some(v) => {
                    v.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| bad(ID, "port `ordinal` must be an integer in 0..4294967295"))?
                }
            };
            let id = if value.get("id").is_some() { text(value, "id", 4096, true)? } else { String::new() };
            Ok(Some(MidiOutPort { backend, name, ordinal, id }))
        }
        _ => Err(bad(ID, "`port` must be internal or a port object")),
    }
}

fn channel_param(value: &Value) -> Result<MidiOutChannel> {
    if value.as_str() == Some("as_recorded") {
        return Ok(MidiOutChannel::AsRecorded);
    }
    value
        .as_u64()
        .filter(|v| (1..=16).contains(v))
        .map(|v| MidiOutChannel::Channel((v - 1) as u8))
        .ok_or_else(|| bad(ID, "`channel` must be as_recorded or an integer in 1..16"))
}

fn offset_param(value: &Value) -> Result<f32> {
    value
        .as_f64()
        .filter(|v| v.is_finite() && v.abs() <= f64::from(MAX_MIDI_OUT_OFFSET_MS))
        .map(|v| v as f32)
        .ok_or_else(|| bad(ID, format!("`offset_ms` must be a number in -{MAX_MIDI_OUT_OFFSET_MS}..{MAX_MIDI_OUT_OFFSET_MS}")))
}

fn set_output(e: &mut Engine, p: &Value) -> Result<Value> {
    if p.get("tracks").is_some_and(|v| !v.is_array()) {
        return Err(bad(ID, "`tracks` must be an array of track ids or names"));
    }
    let mut tracks = tracks_required(e, ID, p)?;
    tracks.sort_by_key(|t| t.0);
    tracks.dedup();
    if tracks.iter().any(|id| e.session().track(*id).is_none_or(|t| !t.kind.is_midi())) {
        return Err(bad(ID, "MIDI output is available only on MIDI and instrument tracks"));
    }
    let port = p.get("port").map(port_param).transpose()?;
    let channel = p.get("channel").map(channel_param).transpose()?;
    let offset = p.get("offset_ms").map(offset_param).transpose()?;
    if port.is_none() && channel.is_none() && offset.is_none() {
        return Err(bad(ID, "supply `port`, `channel` or `offset_ms`"));
    }
    if matches!(port, Some(None)) && (channel.is_some() || offset.is_some()) {
        return Err(bad(ID, "`channel` and `offset_ms` apply to an external port, not internal"));
    }
    // Validate every track before changing any, so a refusal changes nothing.
    let mut next = Vec::with_capacity(tracks.len());
    for id in &tracks {
        let current = e.session().track(*id).map(|t| t.midi_output.clone()).unwrap_or_default();
        let output = match (&port, current) {
            (Some(None), _) => MidiOutput::Internal,
            (Some(Some(port)), MidiOutput::External { channel: c, offset_ms: o, .. }) => {
                MidiOutput::External { port: port.clone(), channel: channel.unwrap_or(c), offset_ms: offset.unwrap_or(o) }
            }
            (Some(Some(port)), MidiOutput::Internal) => {
                MidiOutput::External { port: port.clone(), channel: channel.unwrap_or_default(), offset_ms: offset.unwrap_or(0.0) }
            }
            (None, MidiOutput::External { port, channel: c, offset_ms: o }) => {
                MidiOutput::External { port, channel: channel.unwrap_or(c), offset_ms: offset.unwrap_or(o) }
            }
            (None, MidiOutput::Internal) => return Err(bad(ID, "choose an external port before its channel or offset")),
        };
        next.push((*id, output));
    }
    for (id, output) in next {
        if let Some(track) = e.session_mut().track_mut(id) {
            track.midi_output = output;
        }
    }
    Ok(json!({"tracks": tracks}))
}
