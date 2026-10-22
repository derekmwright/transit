//! Tracks and playlists.

use crate::{AutoParam, AutomationLane, ChannelFormat, Clip, ClipId, Insert, Mixer, SourceId, TrackId};
use soundcraft_time::Samples;

/// Track types (Track › New).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum TrackKind {
    #[default]
    Audio,
    Aux,
    Master,
    Midi,
    Instrument,
    Vca,
    Folder,
    /// Picture: clips reference a movie ([`crate::VideoSource`]); no audio path.
    Video,
}

impl TrackKind {
    pub const ALL: [TrackKind; 8] = [
        TrackKind::Audio,
        TrackKind::Aux,
        TrackKind::Master,
        TrackKind::Midi,
        TrackKind::Instrument,
        TrackKind::Vca,
        TrackKind::Folder,
        TrackKind::Video,
    ];
    pub fn label(self) -> &'static str {
        match self {
            TrackKind::Audio => "Audio Track",
            TrackKind::Aux => "Aux Input",
            TrackKind::Master => "Master Fader",
            TrackKind::Midi => "MIDI Track",
            TrackKind::Instrument => "Instrument Track",
            TrackKind::Vca => "VCA Master",
            TrackKind::Folder => "Routing Folder Track",
            TrackKind::Video => "Video Track",
        }
    }
    pub fn default_name(self) -> &'static str {
        match self {
            TrackKind::Audio => "Audio",
            TrackKind::Aux => "Aux",
            TrackKind::Master => "Master",
            TrackKind::Midi => "MIDI",
            TrackKind::Instrument => "Inst",
            TrackKind::Vca => "VCA",
            TrackKind::Folder => "Folder",
            TrackKind::Video => "Video",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            TrackKind::Audio => "audio",
            TrackKind::Aux => "aux",
            TrackKind::Master => "master",
            TrackKind::Midi => "midi",
            TrackKind::Instrument => "instrument",
            TrackKind::Vca => "vca",
            TrackKind::Folder => "folder",
            TrackKind::Video => "video",
        }
    }
    pub fn from_id(s: &str) -> Option<TrackKind> {
        let s = s.to_ascii_lowercase();
        TrackKind::ALL.into_iter().find(|k| k.id() == s || k.label().eq_ignore_ascii_case(&s))
    }
    /// Track kinds that hold clips on a timeline.
    pub fn has_playlist(self) -> bool {
        matches!(self, TrackKind::Audio | TrackKind::Midi | TrackKind::Instrument | TrackKind::Video)
    }
    pub fn is_midi(self) -> bool {
        matches!(self, TrackKind::Midi | TrackKind::Instrument)
    }
    /// Kinds that pass audio through the mixer.
    pub fn is_audio_path(self) -> bool {
        matches!(self, TrackKind::Audio | TrackKind::Aux | TrackKind::Master | TrackKind::Instrument | TrackKind::Folder)
    }
}

/// Track heights (View › Track Height presets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum TrackHeight {
    /// Any height from Micro to Extreme, in points.
    Custom(u16),
    Micro,
    Mini,
    Small,
    #[default]
    Medium,
    Large,
    Jumbo,
    Extreme,
    /// Fit To Window: this many points.
    Fit(u16),
}

impl TrackHeight {
    pub const ALL: [TrackHeight; 7] = [
        TrackHeight::Micro,
        TrackHeight::Mini,
        TrackHeight::Small,
        TrackHeight::Medium,
        TrackHeight::Large,
        TrackHeight::Jumbo,
        TrackHeight::Extreme,
    ];
    pub fn points(self) -> f32 {
        match self {
            TrackHeight::Custom(points) => f32::from(points).clamp(TrackHeight::Micro.points(), TrackHeight::Extreme.points()),
            TrackHeight::Micro => 16.0,
            TrackHeight::Mini => 24.0,
            TrackHeight::Small => 46.0,
            TrackHeight::Medium => 88.0,
            TrackHeight::Large => 132.0,
            TrackHeight::Jumbo => 220.0,
            TrackHeight::Extreme => 440.0,
            TrackHeight::Fit(points) => f32::from(points.max(16)),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            TrackHeight::Custom(_) => "custom",
            TrackHeight::Micro => "micro",
            TrackHeight::Mini => "mini",
            TrackHeight::Small => "small",
            TrackHeight::Medium => "medium",
            TrackHeight::Large => "large",
            TrackHeight::Jumbo => "jumbo",
            TrackHeight::Extreme => "extreme",
            TrackHeight::Fit(_) => "fit to window",
        }
    }
    pub fn from_id(s: &str) -> Option<TrackHeight> {
        TrackHeight::ALL.into_iter().find(|h| h.label().eq_ignore_ascii_case(s))
    }
    /// The preset this many points tall, else a custom height clamped to Micro..Extreme.
    pub fn from_points(points: f32) -> TrackHeight {
        let p = points.max(TrackHeight::Micro.points()).min(TrackHeight::Extreme.points()).round() as u16;
        TrackHeight::ALL.into_iter().find(|h| h.points() == f32::from(p)).unwrap_or(TrackHeight::Custom(p))
    }
    /// The next preset taller (or shorter) than this height; Extreme (or Micro) stays.
    pub fn step(self, taller: bool) -> TrackHeight {
        let p = self.points();
        let next =
            if taller { TrackHeight::ALL.into_iter().find(|h| h.points() > p) } else { TrackHeight::ALL.into_iter().rev().find(|h| h.points() < p) };
        next.unwrap_or(self)
    }
}

/// The default track colour palette (original, chosen for SoundCraft).
pub const TRACK_COLORS: [[u8; 3]; 16] = [
    [86, 132, 214],
    [94, 170, 214],
    [80, 190, 168],
    [104, 186, 98],
    [168, 196, 76],
    [222, 196, 72],
    [230, 152, 62],
    [222, 104, 72],
    [214, 82, 110],
    [196, 86, 168],
    [150, 98, 206],
    [110, 104, 214],
    [130, 150, 170],
    [178, 140, 110],
    [120, 170, 140],
    [190, 120, 150],
];

/// A playlist: an alternative take of a track's clips.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Playlist {
    pub name: String,
    /// Clips sorted by start; non-overlapping except at crossfades.
    pub clips: Vec<Clip>,
}

impl Playlist {
    pub fn new(name: impl Into<String>) -> Self {
        Playlist { name: name.into(), clips: Vec::new() }
    }
    pub fn sort(&mut self) {
        self.clips.sort_by_key(|c| (c.start, c.id.0));
    }
    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == id)
    }
    pub fn clip_mut(&mut self, id: ClipId) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == id)
    }
    pub fn clip_at(&self, at: Samples) -> Option<&Clip> {
        self.clips.iter().rev().find(|c| c.range().contains(at))
    }
    pub fn end(&self) -> Samples {
        self.clips.iter().map(Clip::end).max().unwrap_or(0)
    }
}

/// A track rendered to audio (Track > Freeze): the post-insert, pre-fader output over `start` for
/// `length` samples, kept in the source pool. The instrument, inserts and clips stay in the track.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Frozen {
    pub source: SourceId,
    /// Where the render starts on the timeline, latency removed (as a bounce has it).
    pub start: Samples,
    pub length: Samples,
    /// [`Track::freeze_hash`] of the track when it was rendered.
    pub hash: u64,
    /// The engine's edit revision at the render (informational).
    #[serde(default)]
    pub revision: u64,
    /// The latency in samples of the chain the render replaces: the frozen track reports it, so
    /// delay compensation stays what it was live.
    #[serde(default)]
    pub latency: u32,
}

/// What a frozen render depends on: an edit to any of it makes the render stale.
#[derive(PartialEq, serde::Serialize)]
struct FreezeInputs<'a> {
    kind: TrackKind,
    format: ChannelFormat,
    ticks_timebase: bool,
    elastic: &'a Option<String>,
    clips: &'a [Clip],
    instrument: &'a Option<Insert>,
    inserts: &'a [Option<Insert>],
    plugin_lanes: Vec<&'a AutomationLane>,
}

/// A track.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    pub format: ChannelFormat,
    pub color: [u8; 3],
    pub height: TrackHeight,
    pub playlists: Vec<Playlist>,
    pub active_playlist: usize,
    pub mixer: Mixer,
    /// MIDI hardware input and channel filter (old documents accept every input).
    #[serde(default)]
    pub midi_input: crate::MidiInput,
    pub automation: Vec<AutomationLane>,
    /// Which lane the track shows: `waveform`/`blocks`/`notes`/`regions` or an automation param id.
    pub view: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub inactive: bool,
    /// The track rendered to audio: while set, the mix plays the render instead of the instrument
    /// and inserts, which make no instances and keep their settings (see [`Frozen`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeze: Option<Frozen>,
    #[serde(default)]
    pub comments: String,
    /// Folder that contains this track.
    #[serde(default)]
    pub folder: Option<TrackId>,
    #[serde(default = "yes")]
    pub folder_open: bool,
    /// Instrument plugin (instrument tracks) — registry id.
    #[serde(default)]
    pub instrument: Option<crate::Insert>,
    /// Ticks (musical) or samples timebase for clips.
    #[serde(default)]
    pub ticks_timebase: bool,
    /// Elastic Audio algorithm, if enabled.
    #[serde(default)]
    pub elastic: Option<String>,
    /// Pinned to top in the Edit window.
    #[serde(default)]
    pub pinned: bool,
}

fn yes() -> bool {
    true
}

impl Track {
    pub fn new(id: TrackId, name: impl Into<String>, kind: TrackKind, format: ChannelFormat, color: [u8; 3]) -> Self {
        let name = name.into();
        let format = if kind.is_midi() && kind == TrackKind::Midi { ChannelFormat::Mono } else { format };
        let view = if kind.is_midi() {
            "notes"
        } else if kind.has_playlist() {
            "waveform"
        } else {
            "volume"
        };
        Track {
            id,
            playlists: vec![Playlist::new(name.clone())],
            name,
            kind,
            format,
            color,
            height: TrackHeight::Medium,
            active_playlist: 0,
            mixer: Mixer::new(format),
            midi_input: crate::MidiInput::default(),
            automation: Vec::new(),
            view: view.into(),
            hidden: false,
            inactive: false,
            freeze: None,
            comments: String::new(),
            folder: None,
            folder_open: true,
            instrument: None,
            ticks_timebase: kind.is_midi(),
            elastic: None,
            pinned: false,
        }
    }

    pub fn channels(&self) -> usize {
        self.format.channels()
    }

    fn freeze_inputs(&self) -> FreezeInputs<'_> {
        FreezeInputs {
            kind: self.kind,
            format: self.format,
            ticks_timebase: self.ticks_timebase,
            elastic: &self.elastic,
            clips: self.clips(),
            instrument: &self.instrument,
            inserts: &self.mixer.inserts,
            plugin_lanes: self.automation.iter().filter(|l| matches!(l.param, AutoParam::Plugin { .. })).collect(),
        }
    }

    /// Whether `other` has the same material as this track: the same clips, instrument, inserts
    /// and plug-in automation. Fader, pan, sends and other automation are not material.
    #[must_use]
    pub fn same_freeze_inputs(&self, other: &Track) -> bool {
        self.freeze_inputs() == other.freeze_inputs()
    }

    /// A hash (FNV-1a) of the material a freeze renders; see [`Track::same_freeze_inputs`].
    #[must_use]
    pub fn freeze_hash(&self) -> u64 {
        let bytes = serde_json::to_vec(&self.freeze_inputs()).unwrap_or_default();
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
    }

    pub fn playlist(&self) -> Option<&Playlist> {
        self.playlists.get(self.active_playlist)
    }

    pub fn playlist_mut(&mut self) -> Option<&mut Playlist> {
        self.playlists.get_mut(self.active_playlist)
    }

    pub fn clips(&self) -> &[Clip] {
        self.playlist().map_or(&[], |p| p.clips.as_slice())
    }

    /// Moves the insert in slot `from` to slot `to`; the slots between shift by one toward the
    /// vacated place (empty slots shift too). Each insert keeps its settings, state and
    /// [`InsertUid`](crate::InsertUid), and every plug-in automation lane follows its insert to the
    /// new slot. Returns false, changing nothing, when a slot is out of range, `from == to`, or
    /// `from` is empty.
    pub fn move_insert(&mut self, from: usize, to: usize) -> bool {
        let n = self.mixer.inserts.len();
        if from == to || from >= n || to >= n || self.mixer.inserts.get(from).is_none_or(Option::is_none) {
            return false;
        }
        if from < to {
            if let Some(r) = self.mixer.inserts.get_mut(from..=to) {
                r.rotate_left(1);
            }
        } else if let Some(r) = self.mixer.inserts.get_mut(to..=from) {
            r.rotate_right(1);
        }
        let new_slot = |s: usize| {
            if s == from {
                to
            } else if from < to && s > from && s <= to {
                s - 1
            } else if to < from && s >= to && s < from {
                s + 1
            } else {
                s
            }
        };
        for l in &mut self.automation {
            // `u8::MAX` is the track trim lane, not an insert.
            if let AutoParam::Plugin { slot, .. } = &mut l.param
                && let Some(new) = u8::try_from(new_slot(usize::from(*slot))).ok().filter(|_| usize::from(*slot) < n)
            {
                *slot = new;
            }
        }
        true
    }

    /// Copies the insert in slot `from` into slot `to`: the copy lands exactly there, and the
    /// inserts from `to` up to the first empty slot at or after it shift up one (nothing shifts
    /// when `to` is empty). The copy keeps the settings, bypass, preset and state but gets a fresh
    /// [`InsertUid`](crate::InsertUid), and no automation. Plug-in automation lanes of the
    /// shifted inserts follow them. Returns false, changing nothing, when a slot is out of range,
    /// `from == to`, `from` is empty, or there is no empty slot at or after `to`.
    pub fn copy_insert(&mut self, from: usize, to: usize) -> bool {
        let n = self.mixer.inserts.len();
        if from == to || from >= n || to >= n {
            return false;
        }
        let Some(mut copy) = self.mixer.inserts.get(from).and_then(Option::clone) else { return false };
        let Some(free) = self.mixer.inserts.iter().enumerate().skip(to).find(|(_, i)| i.is_none()).map(|(i, _)| i) else { return false };
        copy.uid = crate::InsertUid::default();
        if let Some(r) = self.mixer.inserts.get_mut(to..=free) {
            r.rotate_right(1);
        }
        if let Some(slot) = self.mixer.inserts.get_mut(to) {
            *slot = Some(copy);
        }
        for l in &mut self.automation {
            if let AutoParam::Plugin { slot, .. } = &mut l.param
                && let Some(new) = u8::try_from(usize::from(*slot) + 1).ok().filter(|_| (to..free).contains(&usize::from(*slot)))
            {
                *slot = new;
            }
        }
        true
    }

    pub fn lane(&self, p: &AutoParam) -> Option<&AutomationLane> {
        self.automation.iter().find(|l| &l.param == p)
    }

    pub fn lane_mut(&mut self, p: &AutoParam) -> &mut AutomationLane {
        let idx = match self.automation.iter().position(|l| &l.param == p) {
            Some(i) => i,
            None => {
                self.automation.push(AutomationLane::new(p.clone()));
                self.automation.len() - 1
            }
        };
        let len = self.automation.len();
        // `idx` is in range: it came from `position` or is the index of the element just pushed.
        &mut self.automation[idx.min(len - 1)]
    }

    pub fn is_folder(&self) -> bool {
        self.kind == TrackKind::Folder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_track_defaults() {
        let t = Track::new(TrackId(1), "Gtr", TrackKind::Audio, ChannelFormat::Stereo, TRACK_COLORS[0]);
        assert_eq!(t.channels(), 2);
        assert_eq!(t.view, "waveform");
        assert_eq!(t.playlists.len(), 1);
        let m = Track::new(TrackId(2), "Keys", TrackKind::Midi, ChannelFormat::Stereo, TRACK_COLORS[0]);
        assert_eq!(m.format, ChannelFormat::Mono);
        assert_eq!(m.view, "notes");
    }

    #[test]
    fn lane_mut_creates_once() {
        let mut t = Track::new(TrackId(1), "Gtr", TrackKind::Audio, ChannelFormat::Mono, TRACK_COLORS[0]);
        t.lane_mut(&AutoParam::Volume).set_point(0, -3.0);
        t.lane_mut(&AutoParam::Volume).set_point(10, -6.0);
        assert_eq!(t.automation.len(), 1);
        assert_eq!(t.lane(&AutoParam::Volume).map(|l| l.points.len()), Some(2));
    }

    fn chain4() -> Track {
        let mut t = Track::new(TrackId(1), "Gtr", TrackKind::Audio, ChannelFormat::Mono, TRACK_COLORS[0]);
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            t.mixer.inserts[i] = Some(crate::Insert::new(*id));
        }
        t
    }

    fn order(t: &Track) -> String {
        t.mixer.inserts.iter().map(|i| i.as_ref().map_or("_", |i| i.plugin.as_str())).collect()
    }

    #[test]
    fn move_insert_forward_and_backward() {
        let mut t = chain4();
        let uids: Vec<_> = t.mixer.inserts.iter().flatten().map(|i| i.uid.get()).collect();
        assert!(t.move_insert(0, 2));
        assert_eq!(order(&t), "bcad______");
        assert!(t.move_insert(3, 0));
        assert_eq!(order(&t), "dbca______");
        // A move into an empty slot shifts the empties too.
        assert!(t.move_insert(1, 5));
        assert_eq!(order(&t), "dca__b____");
        // Identity travels with the insert.
        let a = t.mixer.inserts.iter().flatten().find(|i| i.plugin == "a").map(|i| i.uid.get());
        assert_eq!(a, Some(uids[0]));
    }

    #[test]
    fn move_insert_rejects_noops_and_out_of_range() {
        let mut t = chain4();
        let before = order(&t);
        assert!(!t.move_insert(1, 1));
        assert!(!t.move_insert(0, 10));
        assert!(!t.move_insert(10, 0));
        assert!(!t.move_insert(usize::MAX, 0));
        assert!(!t.move_insert(5, 0), "an empty slot has nothing to move");
        assert_eq!(order(&t), before);
    }

    #[test]
    fn move_insert_carries_flags_and_automation() {
        let mut t = chain4();
        {
            let b = t.mixer.inserts[1].as_mut().unwrap();
            b.bypass = true;
            b.active = false;
            b.params.insert("x".into(), 0.25);
            b.preset = "P".into();
        }
        let lane = |slot| AutoParam::Plugin { slot, param: "x".into() };
        for slot in [0u8, 1, 2, 3] {
            t.lane_mut(&lane(slot)).set_point(0, f32::from(slot));
        }
        let trim = AutoParam::Plugin { slot: u8::MAX, param: "trim".into() };
        t.lane_mut(&trim).set_point(0, -3.0);
        assert!(t.move_insert(1, 3));
        assert_eq!(order(&t), "acdb______");
        let b = t.mixer.inserts[3].as_ref().unwrap();
        assert!(b.bypass && !b.active && b.preset == "P" && b.params.get("x") == Some(&0.25));
        // The lane that targeted b (value 1.0) now targets slot 3; the others followed their inserts.
        let at = |t: &Track, slot| t.lane(&lane(slot)).map(|l| l.points[0].value);
        assert_eq!([at(&t, 0), at(&t, 1), at(&t, 2), at(&t, 3)], [Some(0.0), Some(2.0), Some(3.0), Some(1.0)]);
        assert_eq!(t.lane(&trim).map(|l| l.points[0].value), Some(-3.0), "the trim lane is not an insert");
        // And back.
        assert!(t.move_insert(3, 1));
        assert_eq!(order(&t), "abcd______");
        assert_eq!([at(&t, 0), at(&t, 1), at(&t, 2), at(&t, 3)], [Some(0.0), Some(1.0), Some(2.0), Some(3.0)]);
    }

    #[test]
    fn copy_insert_places_a_fresh_copy_and_shifts_the_rest() {
        let mut t = chain4();
        {
            let b = t.mixer.inserts[1].as_mut().unwrap();
            b.bypass = true;
            b.params.insert("x".into(), 0.25);
            b.preset = "P".into();
            b.state = Some("AAEC".into());
        }
        let uids: Vec<_> = t.mixer.inserts.iter().flatten().map(|i| i.uid.get()).collect();
        let lane = |slot| AutoParam::Plugin { slot, param: "x".into() };
        for slot in [0u8, 1, 2, 3] {
            t.lane_mut(&lane(slot)).set_point(0, f32::from(slot));
        }
        assert!(t.copy_insert(1, 3));
        assert_eq!(order(&t), "abcbd_____");
        let (orig, copy) = (t.mixer.inserts[1].as_ref().unwrap(), t.mixer.inserts[3].as_ref().unwrap());
        assert!(copy.bypass && copy.params.get("x") == Some(&0.25) && copy.preset == "P" && copy.state.as_deref() == Some("AAEC"));
        assert_ne!(copy.uid.get(), orig.uid.get(), "the copy is a new insert");
        assert_eq!(orig.uid.get(), uids[1]);
        assert_eq!(t.mixer.inserts[4].as_ref().unwrap().uid.get(), uids[3], "d shifted, keeping its identity");
        // Lanes: a, b, c keep their slots; d's lane moved with it; the copy has none.
        let at = |t: &Track, slot| t.lane(&lane(slot)).map(|l| l.points[0].value);
        assert_eq!([at(&t, 0), at(&t, 1), at(&t, 2), at(&t, 3), at(&t, 4)], [Some(0.0), Some(1.0), Some(2.0), None, Some(3.0)]);
        // Into an empty slot: nothing shifts.
        assert!(t.copy_insert(0, 8));
        assert_eq!(order(&t), "abcbd___a_");
    }

    #[test]
    fn copy_insert_rejects_noops_out_of_range_and_a_full_tail() {
        let mut t = chain4();
        let before = order(&t);
        assert!(!t.copy_insert(1, 1));
        assert!(!t.copy_insert(0, 10));
        assert!(!t.copy_insert(10, 0));
        assert!(!t.copy_insert(5, 0), "an empty slot has nothing to copy");
        for i in 4..10 {
            t.mixer.inserts[i] = Some(crate::Insert::new("z"));
        }
        let full = order(&t);
        assert!(!t.copy_insert(0, 2), "no empty slot to shift into");
        assert_eq!(order(&t), full);
        assert_ne!(before, full);
    }

    #[test]
    fn kind_ids() {
        for k in TrackKind::ALL {
            assert_eq!(TrackKind::from_id(k.id()), Some(k));
        }
    }

    #[test]
    fn custom_heights_round_to_presets_and_step_through_them() {
        assert_eq!(TrackHeight::from_points(88.0), TrackHeight::Medium);
        assert_eq!(TrackHeight::from_points(57.4), TrackHeight::Custom(57));
        assert_eq!(TrackHeight::from_points(9999.0), TrackHeight::Extreme);
        assert_eq!(TrackHeight::from_points(f32::NAN), TrackHeight::Micro);
        assert_eq!(TrackHeight::Custom(u16::MAX).points(), 440.0);
        assert_eq!(TrackHeight::Custom(57).step(true), TrackHeight::Medium);
        assert_eq!(TrackHeight::Custom(57).step(false), TrackHeight::Small);
        assert_eq!(TrackHeight::Extreme.step(true), TrackHeight::Extreme);
        assert_eq!(TrackHeight::Micro.step(false), TrackHeight::Micro);
        let json = serde_json::to_string(&TrackHeight::Custom(57)).unwrap();
        assert_eq!(serde_json::from_str::<TrackHeight>(&json).unwrap(), TrackHeight::Custom(57));
        assert_eq!(serde_json::from_str::<TrackHeight>("\"Large\"").unwrap(), TrackHeight::Large);
    }
}
