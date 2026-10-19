//! Instrument events as a plug-in receives them (ADR 0012 G5, F10): one note port, a MIDI 1.0
//! channel, and a host-assigned voice id. The mix engine wraps these with routing and timing
//! (`soundcraft_mix::InstrumentEvent`) and re-exports them; plug-in hosts encode them for their
//! format.

/// A sender's note identity: the id an ingress protocol gives a note (MIDI 2.0, OSC, a clip), so
/// its release finds it again. MIDI 1.0 input has none ([`NoteId::NONE`]); notes are then matched
/// by key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoteId(pub u32);

impl NoteId {
    /// No sender identity (MIDI 1.0).
    pub const NONE: Self = NoteId(u32::MAX);
}

/// The sustain pedal (CC 64).
pub const CC_SUSTAIN: u8 = 64;
/// The sostenuto pedal (CC 66).
pub const CC_SOSTENUTO: u8 = 66;
/// The soft pedal (CC 67): not tracked per source, but its release survives a host reset.
pub const CC_SOFT: u8 = 67;
/// Hold 2 (CC 69).
pub const CC_HOLD2: u8 = 69;
/// All sound off (CC 120).
pub const CC_ALL_SOUND_OFF: u8 = 120;
/// Reset all controllers (CC 121).
pub const CC_RESET_CONTROLLERS: u8 = 121;
/// All notes off (CC 123).
pub const CC_ALL_NOTES_OFF: u8 = 123;

/// One channel message, MIDI 1.0 values. A note-on has velocity 1..=127 (ingress turns a 9n with
/// velocity 0 into a [`Message::NoteOff`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    NoteOn {
        key: u8,
        velocity: u8,
        id: NoteId,
    },
    NoteOff {
        key: u8,
        velocity: u8,
        id: NoteId,
    },
    PolyPressure {
        key: u8,
        value: u8,
        id: NoteId,
    },
    ChannelPressure {
        value: u8,
    },
    Control {
        cc: u8,
        value: u8,
    },
    /// A decoded 14-bit controller pair: `msb` is the MSB controller (0..=31), `value` 0..=16383.
    Control14 {
        msb: u8,
        value: u16,
    },
    /// 0..=16383, 8192 is the center.
    PitchBend {
        value: u16,
    },
    Program {
        program: u8,
    },
}

/// Is `cc` one of the pedals the mix tracks per source (sustain, sostenuto, hold 2)?
#[must_use]
pub const fn is_pedal(cc: u8) -> bool {
    matches!(cc, CC_SUSTAIN | CC_SOSTENUTO | CC_HOLD2)
}

impl Message {
    /// The release class: what must reach a plug-in even when its queue is full. A note-off, a
    /// pedal going up (value below 64), reset all controllers, all sound off and all notes off.
    #[must_use]
    pub const fn is_release(&self) -> bool {
        match *self {
            Message::NoteOff { .. } => true,
            Message::Control { cc, value } => {
                (is_pedal(cc) && value < 64) || matches!(cc, CC_ALL_SOUND_OFF | CC_RESET_CONTROLLERS | CC_ALL_NOTES_OFF)
            }
            _ => false,
        }
    }

    /// What a plug-in host keeps through its own `reset` (and delivers on the next `process`):
    /// the release class, a note-on with velocity 0, and the soft pedal (CC 67) going up. A reset
    /// that dropped these would leave a sustain or note hanging at the plug-in, since the mix has
    /// already freed what it held.
    #[must_use]
    pub const fn survives_reset(&self) -> bool {
        match *self {
            Message::NoteOn { velocity: 0, .. } => true,
            Message::Control { cc: CC_SOFT, value } => value < 64,
            _ => self.is_release(),
        }
    }

    /// [`Message::survives_reset`] for raw MIDI 1.0 bytes (any status byte; non-channel
    /// messages do not survive).
    #[must_use]
    pub const fn midi1_survives_reset(msg: [u8; 3]) -> bool {
        let [status, d1, d2] = msg;
        match status & 0xF0 {
            0x80 => true,
            0x90 => d2 & 0x7F == 0,
            0xB0 => Message::Control { cc: d1 & 0x7F, value: d2 & 0x7F }.survives_reset(),
            _ => false,
        }
    }

    /// The MIDI 1.0 bytes of this message on `channel` (0..=15): one message, or two for a
    /// 14-bit controller (MSB, then LSB). Unused entries are `None`.
    #[must_use]
    pub fn midi1(&self, channel: u8) -> [Option<[u8; 3]>; 2] {
        let ch = channel & 0x0F;
        let one = |m: [u8; 3]| [Some(m), None];
        match *self {
            Message::NoteOn { key, velocity, .. } => one([0x90 | ch, key & 0x7F, velocity.clamp(1, 127)]),
            Message::NoteOff { key, velocity, .. } => one([0x80 | ch, key & 0x7F, velocity & 0x7F]),
            Message::PolyPressure { key, value, .. } => one([0xA0 | ch, key & 0x7F, value & 0x7F]),
            Message::ChannelPressure { value } => one([0xD0 | ch, value & 0x7F, 0]),
            Message::Control { cc, value } => one([0xB0 | ch, cc & 0x7F, value & 0x7F]),
            Message::Control14 { msb, value } => {
                let v = value.min(16383);
                let msb = msb & 0x1F;
                [Some([0xB0 | ch, msb, (v >> 7) as u8]), Some([0xB0 | ch, msb + 32, (v & 0x7F) as u8])]
            }
            Message::PitchBend { value } => {
                let v = value.min(16383);
                one([0xE0 | ch, (v & 0x7F) as u8, (v >> 7) as u8])
            }
            Message::Program { program } => one([0xC0 | ch, program & 0x7F, 0]),
        }
    }
}

/// What a plug-in receives: one note port, a MIDI 1.0 channel (0..=15) and the host-assigned
/// voice id (`-1`: none). The mix assigns `note_id` per instance; it is never the sender's id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PluginEvent {
    pub channel: u8,
    pub note_id: i32,
    pub message: Message,
}

impl PluginEvent {
    /// No voice id.
    pub const NO_ID: i32 = -1;

    /// See [`Message::is_release`].
    #[must_use]
    pub const fn is_release(&self) -> bool {
        self.message.is_release()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_class() {
        let n = NoteId::NONE;
        assert!(Message::NoteOff { key: 60, velocity: 0, id: n }.is_release());
        assert!(!Message::NoteOn { key: 60, velocity: 1, id: n }.is_release());
        for cc in [64, 66, 69] {
            assert!(Message::Control { cc, value: 0 }.is_release());
            assert!(Message::Control { cc, value: 63 }.is_release());
            assert!(!Message::Control { cc, value: 64 }.is_release());
        }
        for cc in [120, 121, 123] {
            assert!(Message::Control { cc, value: 0 }.is_release());
        }
        assert!(!Message::Control { cc: 1, value: 0 }.is_release());
        assert!(!Message::PitchBend { value: 0 }.is_release());
    }

    #[test]
    fn what_survives_a_host_reset() {
        let n = NoteId::NONE;
        assert!(Message::NoteOff { key: 60, velocity: 0, id: n }.survives_reset());
        assert!(Message::NoteOn { key: 60, velocity: 0, id: n }.survives_reset());
        assert!(!Message::NoteOn { key: 60, velocity: 1, id: n }.survives_reset());
        for cc in [64, 66, 67, 69] {
            assert!(Message::Control { cc, value: 0 }.survives_reset(), "pedal {cc} up");
            assert!(!Message::Control { cc, value: 64 }.survives_reset(), "pedal {cc} down");
        }
        for cc in [120, 121, 123] {
            assert!(Message::Control { cc, value: 0 }.survives_reset());
        }
        assert!(!Message::Control { cc: 1, value: 0 }.survives_reset());
        assert!(Message::midi1_survives_reset([0x83, 60, 0]));
        assert!(Message::midi1_survives_reset([0x93, 60, 0]));
        assert!(!Message::midi1_survives_reset([0x93, 60, 1]));
        assert!(Message::midi1_survives_reset([0xB5, 67, 0]));
        assert!(Message::midi1_survives_reset([0xB5, 123, 0]));
        assert!(!Message::midi1_survives_reset([0xB5, 64, 127]));
        assert!(!Message::midi1_survives_reset([0xE0, 0, 64]));
    }

    #[test]
    fn midi1_bytes_carry_the_channel() {
        assert_eq!(Message::NoteOn { key: 60, velocity: 100, id: NoteId(3) }.midi1(5), [Some([0x95, 60, 100]), None]);
        assert_eq!(Message::PitchBend { value: 8192 }.midi1(15), [Some([0xEF, 0, 64]), None]);
        assert_eq!(Message::Control14 { msb: 1, value: 16383 }.midi1(0), [Some([0xB0, 1, 127]), Some([0xB0, 33, 127])]);
        assert_eq!(Message::Program { program: 7 }.midi1(16), [Some([0xC0, 7, 0]), None], "channel wraps into the nibble");
    }

    #[test]
    fn an_instrument_event_fits_a_lane_slot() {
        assert!(std::mem::size_of::<Message>() <= 8);
        assert!(std::mem::size_of::<PluginEvent>() <= 16);
    }
}
