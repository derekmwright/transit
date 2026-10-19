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

/// A stable port reference: backend, name, ordinal, and the backend's own port id when available.
///
/// **Identity.** Within a backend and direction, a non-empty `id` identifies the port: opening
/// and hot-plug tracking match on it, so unplugging one identified device does not make another
/// look like it. On Windows `id` is the WinMM interface path (midir's `MidiInputPort::id`).
/// It survives enumeration changes, but persistence across replugs and restarts depends on the
/// device and driver. Devices without unique USB serial numbers can change ids when
/// moved between USB ports; identical devices swapped between ports can be misassociated.
/// See [Microsoft's USB MIDI serial-number guidance][usb-serial].
///
/// **Fallback.** When `id` is empty, identity uses `name` plus `ordinal` among only anonymous
/// ports of that name and direction, so identified ports cannot shift an anonymous identity.
/// Removing an anonymous same-name port can still shift the others; that ambiguity is inherent
/// in name-based identity. All backends except WinMM use this fallback here. Legacy empty-id
/// keys can also open identified ports by name and ordinal when no anonymous same-name ports
/// are present; in a mixed list, anonymous ports take precedence. Use an enumerated key to avoid
/// that legacy ambiguity. Identified keys retain a same-name ordinal for display, while
/// [`PortInfo::display_name`] numbers all same-name ports regardless of identity type.
///
/// [usb-serial]: https://devblogs.microsoft.com/windows-music-dev/the-importance-of-including-a-unique-iserialnumber-in-your-usb-midi-devices/
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortKey {
    /// The backend that enumerates the port.
    pub backend: BackendId,
    /// The backend's port name (on WinMM `szPname`, at most 31 characters).
    pub name: String,
    /// Zero-based enumeration order among same-name ports of this direction; when `id` is
    /// empty, only anonymous ports count. See the legacy-key exception above.
    pub ordinal: u32,
    /// The backend's stable port id, or empty when it has none.
    pub id: String,
}

impl fmt::Display for PortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ordinal == 0 { write!(f, "{}", self.name) } else { write!(f, "{} #{}", self.name, u64::from(self.ordinal) + 1) }
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
        let mut key = PortKey { backend, name, ordinal, id };
        let display_name = key.to_string();
        if key.id.is_empty() {
            key.ordinal = u32::try_from(out.iter().filter(|p| p.key.name == key.name && p.key.id.is_empty()).count()).unwrap_or(u32::MAX);
        }
        out.push(PortInfo { display_name, key, direction });
    }
    out
}

/// The enumeration index of the port `key` names, if present: by id when the key has one,
/// otherwise by name and anonymous ordinal. Legacy keys may select identified ports only when
/// there are no anonymous ports of that name; never fall through after an anonymous miss.
pub(crate) fn resolve(ports: &[RawPort], key: &PortKey) -> Option<usize> {
    if !key.id.is_empty() {
        return ports.iter().position(|p| p.id == key.id);
    }
    let anonymous = ports.iter().any(|p| p.name == key.name && p.id.is_empty());
    ports
        .iter()
        .enumerate()
        .filter(|(_, p)| p.name == key.name && (!anonymous || p.id.is_empty()))
        .nth(usize::try_from(key.ordinal).ok()?)
        .map(|(i, _)| i)
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
    fn mixed_ids_resolve_anonymous_ordinals_and_keep_display_names_distinct() {
        let raw = |id: &str| RawPort { name: "Keys".into(), id: id.into() };
        let ports = vec![raw("x"), raw(""), raw("y"), raw("")];
        let listed = infos(BackendId::Fake, Direction::Input, ports.clone());
        assert_eq!(listed.iter().map(|p| p.display_name.as_str()).collect::<Vec<_>>(), ["Keys", "Keys #2", "Keys #3", "Keys #4"]);
        for (i, p) in listed.iter().enumerate() {
            assert_eq!(resolve(&ports, &p.key), Some(i));
        }
        let anonymous: Vec<_> = listed.iter().filter(|p| p.key.id.is_empty()).collect();
        let after = vec![raw(""), raw("")];
        for (i, p) in anonymous.iter().enumerate() {
            assert_eq!(resolve(&after, &p.key), Some(i));
        }
        assert_eq!(anonymous.iter().map(|p| p.key.ordinal).collect::<Vec<_>>(), [0, 1]);
        let missing = PortKey { ordinal: 2, ..anonymous[0].key.clone() };
        assert_eq!(resolve(&ports, &missing), None, "anonymous ordinals must not fall through to identified ports");
    }

    #[test]
    fn legacy_keys_resolve_identified_ports_when_no_anonymous_names_match() {
        let ports = vec![
            RawPort { name: "Keys".into(), id: "x".into() },
            RawPort { name: "Other".into(), id: String::new() },
            RawPort { name: "Keys".into(), id: "y".into() },
        ];
        let key = |ordinal| PortKey { backend: BackendId::Fake, name: "Keys".into(), ordinal, id: String::new() };
        assert_eq!(resolve(&ports, &key(0)), Some(0));
        assert_eq!(resolve(&ports, &key(1)), Some(2));
        assert_eq!(resolve(&ports, &key(2)), None);
    }

    #[test]
    fn port_key_docs_qualify_usb_identity_persistence() {
        // Keep the persistence warning with the public key contract, not just in implementation comments.
        let source = include_str!("port.rs");
        let before_struct = source.split_once("pub struct PortKey {").unwrap().0;
        let docs = before_struct.rsplit_once("/// A stable port reference").unwrap().1;
        assert!(docs.contains("unique USB serial numbers"));
        assert!(docs.contains("moved between USB ports"));
        assert!(docs.contains("identical devices swapped between ports can be misassociated"));
        assert!(docs.contains(
            "https://devblogs.microsoft.com/windows-music-dev/the-importance-of-including-a-unique-iserialnumber-in-your-usb-midi-devices/"
        ));
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
