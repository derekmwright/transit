//! Port identity: which backend, which name, which of several ports with that name.

use std::fmt;

/// The MIDI API a port belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BackendId {
    /// Windows Multimedia (WinMM) MIDI, midir's Windows backend.
    WinMm,
    /// ALSA sequencer (Linux).
    Alsa,
    /// CoreMIDI (macOS, iOS).
    CoreMidi,
    /// Web MIDI (browser).
    WebMidi,
    /// The in-process fake used by tests.
    Fake,
}

impl BackendId {
    /// The backend midir uses on this platform.
    #[must_use]
    pub const fn system() -> Self {
        if cfg!(target_os = "windows") {
            Self::WinMm
        } else if cfg!(any(target_os = "macos", target_os = "ios")) {
            Self::CoreMidi
        } else if cfg!(target_arch = "wasm32") {
            Self::WebMidi
        } else {
            Self::Alsa
        }
    }

    /// A short stable name, for persisted port references and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WinMm => "winmm",
            Self::Alsa => "alsa",
            Self::CoreMidi => "coremidi",
            Self::WebMidi => "webmidi",
            Self::Fake => "fake",
        }
    }
}

/// Input or output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// A port MIDI arrives from.
    Input,
    /// A port MIDI is sent to.
    Output,
}

/// A stable port reference: backend, the backend's port name, and the ordinal of this port among
/// the ports of the same direction with exactly that name, in enumeration order (0 for the first
/// or only one).
///
/// It survives unplug and replug of the same device and process restarts, so it is what a session
/// stores. With two identical devices the ordinal follows enumeration order, which can change when
/// the first is unplugged; that ambiguity is inherent in name-based identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortKey {
    /// The backend that enumerates the port.
    pub backend: BackendId,
    /// The backend's port name (on WinMM `szPname`, at most 31 characters).
    pub name: String,
    /// Which port with this name, in enumeration order.
    pub ordinal: u32,
}

impl fmt::Display for PortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ordinal == 0 { write!(f, "{}", self.name) } else { write!(f, "{} #{}", self.name, self.ordinal + 1) }
    }
}

/// One enumerated port.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PortInfo {
    /// Stable reference to open it with.
    pub key: PortKey,
    /// Input or output.
    pub direction: Direction,
    /// Name for the UI: the port name, with " #2", " #3" ... for duplicates.
    pub display_name: String,
}

/// Turns names in enumeration order into infos, numbering duplicates.
pub(crate) fn infos(backend: BackendId, direction: Direction, names: Vec<String>) -> Vec<PortInfo> {
    let mut out: Vec<PortInfo> = Vec::with_capacity(names.len());
    for name in names {
        let ordinal = u32::try_from(out.iter().filter(|p| p.key.name == name).count()).unwrap_or(u32::MAX);
        let key = PortKey { backend, name, ordinal };
        out.push(PortInfo { display_name: key.to_string(), key, direction });
    }
    out
}

/// The enumeration index of the port `key` names, if present in `names`.
pub(crate) fn index_of(names: &[String], key: &PortKey) -> Option<usize> {
    names.iter().enumerate().filter(|(_, n)| **n == key.name).nth(usize::try_from(key.ordinal).ok()?).map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_get_ordinals_and_resolve_back() {
        let names: Vec<String> = ["A", "B", "A", "A"].iter().map(|s| (*s).to_string()).collect();
        let ports = infos(BackendId::Fake, Direction::Input, names.clone());
        let shown: Vec<_> = ports.iter().map(|p| p.display_name.as_str()).collect();
        assert_eq!(shown, ["A", "B", "A #2", "A #3"]);
        for (i, p) in ports.iter().enumerate() {
            assert_eq!(index_of(&names, &p.key), Some(i));
        }
        let missing = PortKey { backend: BackendId::Fake, name: "A".into(), ordinal: 3 };
        assert_eq!(index_of(&names, &missing), None);
    }
}
