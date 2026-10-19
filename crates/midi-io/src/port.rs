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

/// A stable port reference: backend, the backend's port name, the ordinal of this port among the
/// ports of the same direction with exactly that name, and the backend's own port id when it has
/// a stable one.
///
/// **Identity.** When `id` is non-empty it alone identifies the port: opening and hot-plug
/// tracking match on it, so two identical devices stay distinguishable and unplugging the first
/// does not make the second look like the first. On Windows `id` is the WinMM interface path
/// (midir's `MidiInputPort::id`), stable across enumerations, replugs and restarts. When `id` is
/// empty (a backend whose ids are not stable across runs, which is every backend but WinMM here,
/// or a key written before ids existed) identity falls back to `name` plus `ordinal`, which
/// follows enumeration order and can shift when an identical device is unplugged; that ambiguity
/// is inherent in name-based identity. `name` and `ordinal` stay set either way, for display.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortKey {
    /// The backend that enumerates the port.
    pub backend: BackendId,
    /// The backend's port name (on WinMM `szPname`, at most 31 characters).
    pub name: String,
    /// Which port with this name, in enumeration order.
    pub ordinal: u32,
    /// The backend's stable port id, or empty when it has none.
    pub id: String,
}

impl fmt::Display for PortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ordinal == 0 { write!(f, "{}", self.name) } else { write!(f, "{} #{}", self.name, self.ordinal + 1) }
    }
}

/// What identifies a port across polls: its backend id, else name plus ordinal.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum PortIdentity {
    Id(String),
    Slot(String, u32),
}

impl PortKey {
    pub(crate) fn identity(&self) -> PortIdentity {
        if self.id.is_empty() { PortIdentity::Slot(self.name.clone(), self.ordinal) } else { PortIdentity::Id(self.id.clone()) }
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

/// A port as a backend reports it: name and stable id (empty when the backend has none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawPort {
    pub(crate) name: String,
    pub(crate) id: String,
}

/// Turns ports in enumeration order into infos, numbering duplicate names.
pub(crate) fn infos(backend: BackendId, direction: Direction, ports: Vec<RawPort>) -> Vec<PortInfo> {
    let mut out: Vec<PortInfo> = Vec::with_capacity(ports.len());
    for RawPort { name, id } in ports {
        let ordinal = u32::try_from(out.iter().filter(|p| p.key.name == name).count()).unwrap_or(u32::MAX);
        let key = PortKey { backend, name, ordinal, id };
        out.push(PortInfo { display_name: key.to_string(), key, direction });
    }
    out
}

/// The enumeration index of the port `key` names, if present: by id when the key has one,
/// otherwise by name and ordinal.
pub(crate) fn resolve(ports: &[RawPort], key: &PortKey) -> Option<usize> {
    if !key.id.is_empty() {
        return ports.iter().position(|p| p.id == key.id);
    }
    ports.iter().enumerate().filter(|(_, p)| p.name == key.name).nth(usize::try_from(key.ordinal).ok()?).map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_get_ordinals_and_resolve_back() {
        let names: Vec<RawPort> = ["A", "B", "A", "A"].iter().map(|s| RawPort { name: (*s).to_string(), id: String::new() }).collect();
        let ports = infos(BackendId::Fake, Direction::Input, names.clone());
        let shown: Vec<_> = ports.iter().map(|p| p.display_name.as_str()).collect();
        assert_eq!(shown, ["A", "B", "A #2", "A #3"]);
        for (i, p) in ports.iter().enumerate() {
            assert_eq!(resolve(&names, &p.key), Some(i));
        }
        let missing = PortKey { backend: BackendId::Fake, name: "A".into(), ordinal: 3, id: String::new() };
        assert_eq!(resolve(&names, &missing), None);
    }

    #[test]
    fn ids_win_over_names_and_ordinals() {
        let raw = |n: &str, id: &str| RawPort { name: n.into(), id: id.into() };
        let ports = vec![raw("A", "x"), raw("A", "y")];
        let infos = infos(BackendId::Fake, Direction::Input, ports.clone());
        assert_eq!(resolve(&ports, &infos[1].key), Some(1));
        // The first unplugs: the second is now at index 0 and ordinal 0, but its id still finds it.
        let after = vec![raw("A", "y")];
        assert_eq!(resolve(&after, &infos[1].key), Some(0));
        assert_eq!(resolve(&after, &infos[0].key), None);
        assert_ne!(infos[0].key.identity(), infos[1].key.identity());
    }
}
