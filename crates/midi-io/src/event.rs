//! The fixed-size event that crosses the input ring.

/// One MIDI channel or system message as the backend delivered it.
///
/// Fixed size and `Copy`, with the bytes inline, so the input callback never allocates. SysEx is
/// never carried (see [`InputCounters::sysex_dropped`](crate::InputCounters::sysex_dropped)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEvent {
    /// The message bytes, status first; only `bytes[..len]` is meaningful, the rest is zero.
    /// Running status is already expanded: `bytes[0]` is always a status byte.
    pub bytes: [u8; 3],
    /// Message length, 1 to 3.
    pub len: u8,
    /// Backend time B in microseconds, as the backend passed it to midir's callback. Its origin
    /// is per open port and backend-specific. On Windows (WinMM) it is the `MIM_DATA` timestamp,
    /// milliseconds since `midiInStart`, multiplied by 1000 by midir: **millisecond granularity**,
    /// and a 32-bit millisecond counter that wraps after about 49.7 days. On ALSA and CoreMIDI it
    /// is the backend's own clock. It is an observation of when the backend saw the bytes, not
    /// of the keypress.
    pub backend_time_us: u64,
    /// Host time H in nanoseconds since the port's configured origin (by default
    /// [`host_origin`](crate::host_origin)), read once at entry to the backend callback. Every
    /// message of one callback shares it.
    pub host_arrival_ns: u64,
    /// Per-open sequence number, starting at 0, incremented for every message offered to the
    /// ring, including messages dropped because the ring was full. A gap seen by the consumer
    /// therefore marks where events were dropped. SysEx and filtered messages take no number.
    /// Never wraps: `u64::MAX` is reserved as the terminal exclusive cutoff, so the last valid
    /// sequence is `MAX - 1`. See [`InputBoundary`](crate::InputBoundary) for cancellation and
    /// exhaustion independently of ring consumption.
    pub seq: u64,
}

impl InputEvent {
    /// The meaningful bytes, `&bytes[..len]`.
    #[must_use]
    pub fn message(&self) -> &[u8] {
        let len = usize::from(self.len).min(self.bytes.len());
        self.bytes.get(..len).unwrap_or(&[])
    }

    /// Whether this is a release-type message (see [`is_release`]).
    #[must_use]
    pub fn is_release(&self) -> bool {
        is_release(self.message())
    }
}

/// Release-type messages: the ones that end sound or held state, which the input ring protects
/// with a reserved tail.
///
/// Note-off (`8n`), note-on with velocity 0, the pedals sustain (CC 64), sostenuto (CC 66), soft
/// (CC 67) and hold 2 (CC 69) with a value below 64, all sound off (CC 120), reset all
/// controllers (CC 121, which releases pedals) and all notes off (CC 123).
#[must_use]
pub fn is_release(message: &[u8]) -> bool {
    match *message {
        [status, _, _] if status & 0xF0 == 0x80 => true,
        [status, _, 0] if status & 0xF0 == 0x90 => true,
        [status, controller, value] if status & 0xF0 == 0xB0 => match controller {
            64 | 66 | 67 | 69 => value < 64,
            120 | 121 | 123 => true,
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_classification() {
        assert!(is_release(&[0x80, 60, 64]));
        assert!(is_release(&[0x9F, 60, 0]));
        assert!(!is_release(&[0x90, 60, 1]));
        assert!(is_release(&[0xB0, 64, 0]));
        assert!(is_release(&[0xB3, 66, 63]));
        assert!(!is_release(&[0xB3, 66, 64]));
        assert!(is_release(&[0xB0, 67, 10]));
        assert!(is_release(&[0xB0, 69, 0]));
        assert!(is_release(&[0xB0, 120, 0]));
        assert!(is_release(&[0xB0, 121, 0]));
        assert!(is_release(&[0xB0, 123, 0]));
        assert!(!is_release(&[0xB0, 1, 0]));
        assert!(!is_release(&[0xC0, 1]));
        assert!(!is_release(&[0xF8]));
        assert!(!is_release(&[]));
    }

    #[test]
    fn message_is_the_len_prefix() {
        let e = InputEvent { bytes: [0xC2, 7, 0], len: 2, ..InputEvent::default() };
        assert_eq!(e.message(), &[0xC2, 7]);
        let bad = InputEvent { bytes: [0xF8, 0, 0], len: 9, ..InputEvent::default() };
        assert_eq!(bad.message(), &[0xF8, 0, 0]);
    }
}
