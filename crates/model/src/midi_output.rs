//! Persisted MIDI output destination of a MIDI or instrument track (#61), independent of the
//! native MIDI backend. The port is stored by name (plus the backend's stable id when it has one),
//! so a session opened where the device is absent keeps the choice and reports it missing.

/// The largest offset a destination accepts, in milliseconds either way.
pub const MAX_MIDI_OUT_OFFSET_MS: f32 = 500.0;

/// An output port, identified like a MIDI input device: backend, name, ordinal among same-name
/// ports, and the backend's stable id when it has one (it takes precedence when nonempty).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MidiOutPort {
    pub backend: String,
    pub name: String,
    #[serde(default)]
    pub ordinal: u32,
    #[serde(default)]
    pub id: String,
}

/// Which channel an external destination sends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiOutChannel {
    /// Each event keeps the channel it was recorded on.
    #[default]
    AsRecorded,
    /// Every event is sent on this channel, zero-based (0 is MIDI channel 1).
    Channel(u8),
}

impl MidiOutChannel {
    /// The status-byte channel nibble for an event recorded on `recorded` (zero-based).
    #[must_use]
    pub fn resolve(self, recorded: u8) -> u8 {
        match self {
            Self::AsRecorded => recorded & 0x0F,
            Self::Channel(c) => c & 0x0F,
        }
    }
}

/// Where a track's MIDI goes.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiOutput {
    /// The track's hosted instrument (the behavior of every session before #61).
    #[default]
    Internal,
    /// A hardware or virtual MIDI port. The hosted instrument, if any, receives nothing.
    External {
        port: MidiOutPort,
        #[serde(default)]
        channel: MidiOutChannel,
        /// Sends happen this much later (negative: earlier) than the audio they belong with.
        #[serde(default)]
        offset_ms: f32,
    },
}

impl MidiOutput {
    #[must_use]
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::Internal)
    }

    #[must_use]
    pub fn is_external(&self) -> bool {
        !self.is_internal()
    }

    /// The offset in milliseconds, finite and clamped to [`MAX_MIDI_OUT_OFFSET_MS`]; 0 when internal.
    /// Files are hostile: a huge value from a hand-edited session cannot schedule a send for
    /// next week.
    #[must_use]
    pub fn offset_ms(&self) -> f32 {
        match self {
            Self::External { offset_ms, .. } if offset_ms.is_finite() => offset_ms.clamp(-MAX_MIDI_OUT_OFFSET_MS, MAX_MIDI_OUT_OFFSET_MS),
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelFormat, Track, TrackId, TrackKind};

    #[test]
    fn old_tracks_load_with_the_internal_destination_and_internal_is_not_written() {
        let track = Track::new(TrackId(1), "Keys", TrackKind::Midi, ChannelFormat::Mono, [0; 3]);
        let value = serde_json::to_value(&track).unwrap();
        assert!(value.get("midi_output").is_none(), "the default destination is not written, so older builds read the file unchanged");
        let restored: Track = serde_json::from_value(value).unwrap();
        assert_eq!(restored.midi_output, MidiOutput::Internal);
    }

    #[test]
    fn external_destination_round_trips() {
        let mut track = Track::new(TrackId(1), "Synth", TrackKind::Midi, ChannelFormat::Mono, [0; 3]);
        track.midi_output = MidiOutput::External {
            port: MidiOutPort { backend: "winmm".into(), name: "KeyStep Pro MIDI OUT".into(), ordinal: 0, id: "usb-path".into() },
            channel: MidiOutChannel::Channel(9),
            offset_ms: -3.5,
        };
        let text = serde_json::to_string(&track).unwrap();
        let restored: Track = serde_json::from_str(&text).unwrap();
        assert_eq!(restored.midi_output, track.midi_output);
        let as_recorded = MidiOutput::External { port: MidiOutPort::default(), channel: MidiOutChannel::AsRecorded, offset_ms: 0.0 };
        assert_eq!(serde_json::from_str::<MidiOutput>(&serde_json::to_string(&as_recorded).unwrap()).unwrap(), as_recorded);
    }

    #[test]
    fn minimal_external_object_loads_with_defaults() {
        let v: MidiOutput = serde_json::from_value(serde_json::json!({"external": {"port": {"backend": "winmm", "name": "Synth"}}})).unwrap();
        assert_eq!(
            v,
            MidiOutput::External {
                port: MidiOutPort { backend: "winmm".into(), name: "Synth".into(), ordinal: 0, id: String::new() },
                channel: MidiOutChannel::AsRecorded,
                offset_ms: 0.0
            }
        );
    }

    #[test]
    fn hostile_offsets_are_clamped() {
        let out = |offset_ms| MidiOutput::External { port: MidiOutPort::default(), channel: MidiOutChannel::AsRecorded, offset_ms };
        assert_eq!(out(1e9).offset_ms(), MAX_MIDI_OUT_OFFSET_MS);
        assert_eq!(out(-1e9).offset_ms(), -MAX_MIDI_OUT_OFFSET_MS);
        assert_eq!(out(f32::NAN).offset_ms(), 0.0);
        assert_eq!(MidiOutput::Internal.offset_ms(), 0.0);
        assert_eq!(MidiOutChannel::Channel(200).resolve(3), 200 & 0x0F);
        assert_eq!(MidiOutChannel::AsRecorded.resolve(3), 3);
    }
}
