//! Persisted MIDI input choices, independent of the native MIDI backend.

/// A MIDI input port. A disconnected device remains selected until explicitly changed.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiPort {
    #[default]
    All,
    None,
    /// A backend id takes precedence over the name/ordinal fallback when nonempty.
    Device {
        backend: String,
        name: String,
        ordinal: u32,
        #[serde(default)]
        id: String,
    },
}

/// Track input filter. Channels are zero-based internally; `None` accepts every channel.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MidiInput {
    pub port: MidiPort,
    pub channel: Option<u8>,
}

impl MidiInput {
    /// Invalid saved channels disable input rather than silently selecting another channel.
    pub fn accepts_channel(&self, channel: u8) -> bool {
        channel < 16 && self.channel.is_none_or(|selected| selected < 16 && selected == channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelFormat, Track, TrackId, TrackKind};

    #[test]
    fn old_tracks_default_to_all_ports_and_channels() {
        let track = Track::new(TrackId(1), "Keys", TrackKind::Instrument, ChannelFormat::Stereo, [0; 3]);
        let mut old = serde_json::to_value(&track).unwrap();
        old.as_object_mut().unwrap().remove("midi_input");
        let restored: Track = serde_json::from_value(old).unwrap();
        assert_eq!(restored.midi_input, MidiInput::default());
        assert_eq!(serde_json::from_str::<MidiInput>("{}").unwrap(), MidiInput::default());
    }

    #[test]
    fn device_and_channel_round_trip_without_hardware() {
        let input = MidiInput {
            port: MidiPort::Device { backend: "winmm".into(), name: "Keyboard".into(), ordinal: 2, id: "persistent-id".into() },
            channel: Some(15),
        };
        assert_eq!(serde_json::from_str::<MidiInput>(&serde_json::to_string(&input).unwrap()).unwrap(), input);
        for selected in 0..16 {
            let input = MidiInput { channel: Some(selected), ..MidiInput::default() };
            for incoming in 0..16 {
                assert_eq!(input.accepts_channel(incoming), selected == incoming);
            }
        }
    }

    #[test]
    fn malformed_channels_never_wrap_or_match() {
        for channel in 16..=255 {
            let input: MidiInput = serde_json::from_value(serde_json::json!({"channel": channel})).unwrap();
            assert!((0..=255).all(|incoming| !input.accepts_channel(incoming)));
        }
        for channel in [serde_json::json!(-1), serde_json::json!(256), serde_json::json!(1.5), serde_json::json!("1")] {
            assert!(serde_json::from_value::<MidiInput>(serde_json::json!({"channel": channel})).is_err());
        }
        assert!(!MidiInput::default().accepts_channel(16));
    }
}
