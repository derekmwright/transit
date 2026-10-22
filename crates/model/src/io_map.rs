//! The machine's I/O map (ADR 0015): how many channels the open audio device has and the named
//! paths laid over them. Per machine, not per session: the app keeps it with its preferences
//! (keyed by audio system plus device name) and hands it to the engine. A session refers to
//! paths by name (`Route::Hardware`), so the same session opens on another interface.

use crate::ChannelFormat;

/// Most device channels opened in either direction. A path or bus is narrower
/// (`MAX_PATH_WIDTH`); this is the width of the device.
pub const MAX_DEVICE_CHANNELS: usize = 32;
/// Widest path (the mix's `MAX_CHANNELS`).
pub const MAX_PATH_WIDTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IoDirection {
    Input,
    Output,
}

/// A name, a width and a channel set; `channels` may be non-contiguous. 0-based device channels.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IoPath {
    pub name: String,
    pub direction: IoDirection,
    pub channels: Vec<u16>,
    pub format: ChannelFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoError {
    EmptyName,
    Duplicate(String),
    Width(usize),
    OutOfRange {
        channel: u16,
        opened: u16,
    },
    Missing(String),
    Repeated(u16),
    /// Main plays on this path.
    MainPath,
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoError::EmptyName => write!(f, "a path needs a name"),
            IoError::Duplicate(n) => write!(f, "a path named `{n}` exists"),
            IoError::Width(w) => write!(f, "a path is 1 to {MAX_PATH_WIDTH} channels wide, not {w}"),
            IoError::OutOfRange { channel, opened } => write!(f, "channel {} is not open (the device opened {opened})", channel + 1),
            IoError::Repeated(c) => write!(f, "channel {} is listed twice", c + 1),
            IoError::Missing(n) => write!(f, "no path named `{n}`"),
            IoError::MainPath => write!(f, "Main plays on this path; choose another for Main first"),
        }
    }
}

impl std::error::Error for IoError {}

/// A measured round trip through a hardware insert (ADR 0015 section 5): the frames between a
/// sample written to the `send` output path and the same sample arriving on the `ret` input path,
/// at one session `rate`. Stored per machine with the path table, since it is a fact about this
/// interface and its cabling; a session names the paths only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HardwareLatency {
    pub send: String,
    pub ret: String,
    pub round_trip_frames: u32,
    pub rate: u32,
    /// Seconds since the Unix epoch when it was measured (0 when unknown).
    #[serde(default)]
    pub measured_at: u64,
}

/// One device's channel counts and path table. `inputs` and `outputs` are the counts the stream
/// opened (stored here, so one device in two USB modes is one entry that adapts).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IoMap {
    pub system: String,
    pub device: String,
    pub inputs: u16,
    pub outputs: u16,
    pub paths: Vec<IoPath>,
    /// The output path Main plays on.
    pub main: String,
    /// Measured hardware-insert round trips (see [`HardwareLatency`]).
    pub hardware_latency: Vec<HardwareLatency>,
}

impl Default for IoMap {
    fn default() -> Self {
        Self::for_device("", "", 0, 2)
    }
}

fn format_of(n: usize) -> ChannelFormat {
    ChannelFormat::for_channels(n.clamp(1, MAX_PATH_WIDTH))
}

/// "Out 3-4" for two channels from 2 (1-based names, today's labels).
fn label(prefix: &str, first: u16, width: u16) -> String {
    if width <= 1 { format!("{prefix} {}", first + 1) } else { format!("{prefix} {}-{}", first + 1, first + width) }
}

impl IoPath {
    /// The device channel strip channel `k` of a route through this path uses: the path's own
    /// channels in order, then (a strip wider than the path) the channels after the last one,
    /// which is what "In 3" on a stereo track always meant.
    pub fn source_channel(&self, k: usize) -> usize {
        match self.channels.get(k) {
            Some(&c) => usize::from(c),
            None => self.channels.last().map_or(k, |&l| usize::from(l).saturating_add(k + 1 - self.channels.len())),
        }
    }
}

/// The device input channel (not yet wrapped to the channels the device opened) strip channel `k`
/// listens to or records for an input route named `name`. One function for the mix's monitoring
/// and the recorder, so they cannot disagree (ADR 0015 section 1.4). A name in `io`'s table
/// resolves through it, so a path renamed "Guitar" on channel 5 is channel 5. A name that is not
/// there (an old session, or one made on another machine) keeps the label meaning it always had:
/// the number in "In 3" or "In 3-4" is the first channel.
pub fn input_channel(io: Option<&IoMap>, name: &str, k: usize) -> usize {
    if let Some(p) = io.and_then(|m| m.path(name)).filter(|p| p.direction == IoDirection::Input) {
        return p.source_channel(k);
    }
    let first = name.trim_start_matches("In ").split(['-', ' ']).next().and_then(|n| n.parse::<usize>().ok()).map_or(0, |n| n.saturating_sub(1));
    first.saturating_add(k)
}

impl IoMap {
    /// The default table for an opened device (counts capped at [`MAX_DEVICE_CHANNELS`]): outputs
    /// "Out 1-2" (Main) and one stereo path per remaining pair, a lone trailing channel mono;
    /// inputs "In 1" per channel plus stereo pairs "In 1-2" (the labels sessions already use).
    pub fn for_device(system: &str, device: &str, inputs: u16, outputs: u16) -> Self {
        let cap = MAX_DEVICE_CHANNELS as u16;
        let (inputs, outputs) = (inputs.min(cap), outputs.min(cap));
        let mut paths = Vec::new();
        let mut c = 0;
        while c < outputs {
            let width = (outputs - c).min(2);
            paths.push(IoPath {
                name: label("Out", c, width),
                direction: IoDirection::Output,
                channels: (c..c + width).collect(),
                format: format_of(usize::from(width)),
            });
            c += width;
        }
        for c in 0..inputs {
            paths.push(IoPath { name: label("In", c, 1), direction: IoDirection::Input, channels: vec![c], format: ChannelFormat::Mono });
        }
        let mut c = 0;
        while c + 1 < inputs {
            paths.push(IoPath { name: label("In", c, 2), direction: IoDirection::Input, channels: vec![c, c + 1], format: ChannelFormat::Stereo });
            c += 2;
        }
        let main = paths.iter().find(|p| p.direction == IoDirection::Output).map_or_else(|| "Out 1-2".to_string(), |p| p.name.clone());
        IoMap { system: system.into(), device: device.into(), inputs, outputs, paths, main, hardware_latency: Vec::new() }
    }

    /// The key the app stores this table under.
    pub fn key(&self) -> (&str, &str) {
        (&self.system, &self.device)
    }

    pub fn path(&self, name: &str) -> Option<&IoPath> {
        self.paths.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn paths_of(&self, direction: IoDirection) -> impl Iterator<Item = &IoPath> {
        self.paths.iter().filter(move |p| p.direction == direction)
    }

    fn opened(&self, direction: IoDirection) -> u16 {
        match direction {
            IoDirection::Input => self.inputs,
            IoDirection::Output => self.outputs,
        }
    }

    /// A name, width 1..=16, every channel open. Name clashes are the editing methods' check.
    pub fn validate(&self, path: &IoPath) -> Result<(), IoError> {
        if path.name.trim().is_empty() {
            return Err(IoError::EmptyName);
        }
        if path.channels.is_empty() || path.channels.len() > MAX_PATH_WIDTH {
            return Err(IoError::Width(path.channels.len()));
        }
        if let Some((_, &c)) = path.channels.iter().enumerate().find(|(i, c)| path.channels[..*i].contains(c)) {
            return Err(IoError::Repeated(c));
        }
        let opened = self.opened(path.direction);
        if let Some(&channel) = path.channels.iter().find(|&&c| c >= opened) {
            return Err(IoError::OutOfRange { channel, opened });
        }
        Ok(())
    }

    /// Names are unique across both directions.
    pub fn add_path(&mut self, mut path: IoPath) -> Result<(), IoError> {
        path.name = path.name.trim().to_string();
        self.validate(&path)?;
        if self.path(&path.name).is_some() {
            return Err(IoError::Duplicate(path.name));
        }
        self.paths.push(path);
        Ok(())
    }

    /// Rename a path. Sessions refer to paths by name and keep the names they have: nothing here or
    /// in the engine rewrites a `Route::Hardware`, so a route left naming the old name is an
    /// unresolved route (silent, listed by the engine), not a silent change of destination.
    pub fn rename_path(&mut self, name: &str, to: &str) -> Result<(), IoError> {
        let to = to.trim();
        if to.is_empty() {
            return Err(IoError::EmptyName);
        }
        if self.path(to).is_some_and(|p| !p.name.eq_ignore_ascii_case(name)) {
            return Err(IoError::Duplicate(to.into()));
        }
        let p = self.paths.iter_mut().find(|p| p.name.eq_ignore_ascii_case(name)).ok_or_else(|| IoError::Missing(name.into()))?;
        if self.main.eq_ignore_ascii_case(&p.name) {
            self.main = to.into();
        }
        p.name = to.into();
        Ok(())
    }

    /// Replace a path's channels (its format follows the new width).
    pub fn set_channels(&mut self, name: &str, channels: Vec<u16>) -> Result<(), IoError> {
        let i = self.paths.iter().position(|p| p.name.eq_ignore_ascii_case(name)).ok_or_else(|| IoError::Missing(name.into()))?;
        let mut next = self.paths[i].clone();
        next.format = format_of(channels.len());
        next.channels = channels;
        self.validate(&next)?;
        self.paths[i] = next;
        Ok(())
    }

    pub fn remove_path(&mut self, name: &str) -> Result<(), IoError> {
        if self.main.eq_ignore_ascii_case(name) {
            return Err(IoError::MainPath);
        }
        let before = self.paths.len();
        self.paths.retain(|p| !p.name.eq_ignore_ascii_case(name));
        if self.paths.len() == before {
            return Err(IoError::Missing(name.into()));
        }
        self.hardware_latency.retain(|m| !m.send.eq_ignore_ascii_case(name) && !m.ret.eq_ignore_ascii_case(name));
        Ok(())
    }

    /// The measured round trip, in frames, of a hardware insert sending on `send` and returning on
    /// `ret` at session rate `rate`; `None` when that pair was not measured at that rate.
    #[must_use]
    pub fn hardware_latency(&self, send: &str, ret: &str, rate: u32) -> Option<u32> {
        self.hardware_latency
            .iter()
            .find(|m| m.rate == rate && m.send.eq_ignore_ascii_case(send) && m.ret.eq_ignore_ascii_case(ret))
            .map(|m| m.round_trip_frames)
    }

    /// Store a measurement, replacing the one for the same pair and rate.
    pub fn set_hardware_latency(&mut self, m: HardwareLatency) {
        self.hardware_latency.retain(|o| !(o.rate == m.rate && o.send.eq_ignore_ascii_case(&m.send) && o.ret.eq_ignore_ascii_case(&m.ret)));
        self.hardware_latency.push(m);
    }

    /// Forget the measurements of the pair (every rate). Returns whether there were any.
    pub fn clear_hardware_latency(&mut self, send: &str, ret: &str) -> bool {
        let before = self.hardware_latency.len();
        self.hardware_latency.retain(|o| !(o.send.eq_ignore_ascii_case(send) && o.ret.eq_ignore_ascii_case(ret)));
        self.hardware_latency.len() != before
    }

    /// Choose the output path Main plays on.
    pub fn set_main(&mut self, name: &str) -> Result<(), IoError> {
        let p = self.path(name).filter(|p| p.direction == IoDirection::Output).ok_or_else(|| IoError::Missing(name.into()))?;
        self.main = p.name.clone();
        Ok(())
    }

    /// The device channels Main plays on: its path's, else channels 0 and 1 (0 on a mono device).
    pub fn main_channels(&self) -> Vec<u16> {
        match self.path(&self.main).filter(|p| p.direction == IoDirection::Output) {
            Some(p) => p.channels.clone(),
            None => (0..self.outputs.clamp(1, 2)).collect(),
        }
    }

    /// Adopt the counts a device opened with: paths that no longer fit are dropped; a table left
    /// without its Main path gets the defaults back. Returns whether anything changed.
    pub fn reconcile(&mut self, inputs: u16, outputs: u16) -> bool {
        let cap = MAX_DEVICE_CHANNELS as u16;
        let (inputs, outputs) = (inputs.min(cap), outputs.min(cap));
        if (inputs, outputs) == (self.inputs, self.outputs) {
            return false;
        }
        self.inputs = inputs;
        self.outputs = outputs;
        let fits: Vec<bool> = self.paths.iter().map(|p| p.channels.iter().all(|&c| c < self.opened(p.direction))).collect();
        let mut fits = fits.into_iter();
        self.paths.retain(|_| fits.next().unwrap_or(false));
        if self.path(&self.main).is_none() {
            let fresh = Self::for_device(&self.system, &self.device, inputs, outputs);
            for p in fresh.paths {
                if self.path(&p.name).is_none() {
                    self.paths.push(p);
                }
            }
            self.main = fresh.main;
        }
        true
    }

    /// Pairs of output paths that share a device channel (they sum there), with the first shared
    /// channel (0-based): "Out 3-4 shares channel 3 with Headphones". Inputs may overlap freely
    /// ("In 1" and "In 1-2" read the same channel), so they are not reported.
    pub fn overlaps(&self) -> Vec<(String, String, u16)> {
        let mut found = Vec::new();
        for (i, a) in self.paths.iter().enumerate() {
            for b in &self.paths[i + 1..] {
                if a.direction == IoDirection::Output
                    && b.direction == IoDirection::Output
                    && let Some(&c) = a.channels.iter().find(|c| b.channels.contains(c))
                {
                    found.push((a.name.clone(), b.name.clone(), c));
                }
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(m: &IoMap, d: IoDirection) -> Vec<&str> {
        m.paths_of(d).map(|p| p.name.as_str()).collect()
    }

    fn out(name: &str, channels: Vec<u16>) -> IoPath {
        IoPath { name: name.into(), direction: IoDirection::Output, channels, format: ChannelFormat::Stereo }
    }

    #[test]
    fn default_for_two_channels_is_one_main_pair() {
        let m = IoMap::for_device("ASIO", "Scarlett 2i2", 2, 2);
        assert_eq!(names(&m, IoDirection::Output), ["Out 1-2"]);
        assert_eq!(names(&m, IoDirection::Input), ["In 1", "In 2", "In 1-2"]);
        assert_eq!(m.main, "Out 1-2");
        assert_eq!(m.main_channels(), [0, 1]);
        assert_eq!(m.path("Out 1-2").unwrap().format, ChannelFormat::Stereo);
    }

    #[test]
    fn default_for_four_and_ten_channels_adds_a_pair_per_two_channels() {
        let m = IoMap::for_device("ASIO", "x", 4, 4);
        assert_eq!(names(&m, IoDirection::Output), ["Out 1-2", "Out 3-4"]);
        assert_eq!(m.path("Out 3-4").unwrap().channels, [2, 3]);
        assert_eq!(names(&m, IoDirection::Input), ["In 1", "In 2", "In 3", "In 4", "In 1-2", "In 3-4"]);
        let m = IoMap::for_device("ASIO", "TASCAM", 12, 10);
        assert_eq!(names(&m, IoDirection::Output), ["Out 1-2", "Out 3-4", "Out 5-6", "Out 7-8", "Out 9-10"]);
        assert_eq!(m.paths_of(IoDirection::Input).count(), 12 + 6);
    }

    #[test]
    fn default_for_a_mono_device_is_one_mono_main() {
        let m = IoMap::for_device("WASAPI", "x", 0, 1);
        assert_eq!(names(&m, IoDirection::Output), ["Out 1"]);
        assert_eq!(m.main_channels(), [0]);
        assert_eq!(m.path("Out 1").unwrap().format, ChannelFormat::Mono);
    }

    #[test]
    fn default_for_34_channels_applies_the_cap_and_every_path_validates() {
        let m = IoMap::for_device("ASIO", "big", 34, 34);
        assert_eq!((m.inputs, m.outputs), (32, 32));
        assert_eq!(m.paths_of(IoDirection::Output).count(), 16);
        assert_eq!(m.path("Out 31-32").unwrap().channels, [30, 31]);
        assert!(m.path("Out 33-34").is_none() && m.path("In 33").is_none());
        assert!(m.paths.iter().all(|p| m.validate(p).is_ok()));
    }

    #[test]
    fn an_odd_trailing_output_is_mono() {
        let m = IoMap::for_device("ASIO", "x", 0, 5);
        assert_eq!(names(&m, IoDirection::Output), ["Out 1-2", "Out 3-4", "Out 5"]);
    }

    #[test]
    fn validation_refuses_width_range_and_duplicate_names() {
        let mut m = IoMap::for_device("ASIO", "x", 4, 4);
        assert_eq!(m.add_path(out("Empty", vec![])), Err(IoError::Width(0)));
        assert_eq!(m.add_path(out("Wide", (0..17).collect())), Err(IoError::Width(17)));
        assert_eq!(m.add_path(out("Far", vec![0, 4])), Err(IoError::OutOfRange { channel: 4, opened: 4 }));
        assert_eq!(m.add_path(out(" ", vec![0])), Err(IoError::EmptyName));
        assert_eq!(m.add_path(out("Twice", vec![1, 1])), Err(IoError::Repeated(1)));
        assert_eq!(m.add_path(out("out 3-4", vec![2])), Err(IoError::Duplicate("out 3-4".into())));
        assert_eq!(m.add_path(out("In 1", vec![2])), Err(IoError::Duplicate("In 1".into())), "one namespace across directions");
        m.add_path(out("Phones", vec![3, 1])).unwrap();
        assert_eq!(m.path("Phones").unwrap().channels, [3, 1], "non-contiguous is allowed");
        let wide = IoMap::for_device("ASIO", "x", 0, 32);
        assert!(wide.validate(&out("All", (0..16).collect())).is_ok());
    }

    #[test]
    fn edits_rename_main_and_refuse_removing_it() {
        let mut m = IoMap::for_device("ASIO", "x", 0, 4);
        assert_eq!(m.remove_path("Out 1-2"), Err(IoError::MainPath));
        m.rename_path("Out 1-2", "Monitors").unwrap();
        assert_eq!(m.main, "Monitors");
        assert_eq!(m.rename_path("Out 3-4", "monitors"), Err(IoError::Duplicate("monitors".into())));
        m.set_channels("Out 3-4", vec![3]).unwrap();
        assert_eq!(m.path("Out 3-4").unwrap().format, ChannelFormat::Mono);
        m.set_main("Out 3-4").unwrap();
        assert_eq!(m.main_channels(), [3]);
        m.remove_path("Monitors").unwrap();
        assert_eq!(m.remove_path("Monitors"), Err(IoError::Missing("Monitors".into())));
    }

    #[test]
    fn overlapping_output_paths_are_reported() {
        let mut m = IoMap::for_device("ASIO", "x", 2, 4);
        assert!(m.overlaps().is_empty());
        m.add_path(out("Headphones", vec![2])).unwrap();
        assert_eq!(m.overlaps(), [("Out 3-4".to_string(), "Headphones".to_string(), 2)]);
    }

    #[test]
    fn a_new_channel_count_keeps_what_fits_and_rebuilds_a_missing_main() {
        let mut m = IoMap::for_device("ASIO", "TASCAM", 12, 10);
        m.add_path(out("Mine", vec![0, 1])).unwrap();
        assert!(!m.reconcile(12, 10));
        assert!(m.reconcile(2, 2));
        assert_eq!((m.inputs, m.outputs), (2, 2));
        assert!(m.path("Out 5-6").is_none() && m.path("In 7").is_none());
        assert!(m.path("Mine").is_some() && m.path("Out 1-2").is_some());
        let mut moved = IoMap::for_device("ASIO", "x", 0, 4);
        moved.set_main("Out 3-4").unwrap();
        moved.reconcile(0, 2);
        assert_eq!(moved.main, "Out 1-2");
    }

    #[test]
    fn input_names_resolve_through_the_table_and_fall_back_to_the_label() {
        let mut m = IoMap::for_device("ASIO", "x", 8, 2);
        m.add_path(IoPath { name: "Guitar".into(), direction: IoDirection::Input, channels: vec![4], format: ChannelFormat::Mono }).unwrap();
        m.add_path(IoPath { name: "Pair".into(), direction: IoDirection::Input, channels: vec![6, 1], format: ChannelFormat::Stereo }).unwrap();
        assert_eq!(input_channel(Some(&m), "Guitar", 0), 4);
        assert_eq!(input_channel(Some(&m), "guitar", 1), 5, "a stereo strip on a mono path continues after it, as In 3 always did");
        assert_eq!((input_channel(Some(&m), "Pair", 0), input_channel(Some(&m), "Pair", 1)), (6, 1));
        assert_eq!(input_channel(Some(&m), "In 3", 0), 2);
        assert_eq!(input_channel(Some(&m), "In 3-4", 1), 3);
        // Not in the table: the number in the label, as before.
        assert_eq!(input_channel(Some(&m), "In 12", 0), 11);
        assert_eq!(input_channel(None, "In 3-4", 1), 3);
        assert_eq!(input_channel(None, "Guitar", 0), 0);
        // An output path is not an input.
        assert_eq!(input_channel(Some(&m), "Out 1-2", 0), 0);
    }

    #[test]
    fn hardware_latency_is_per_pair_and_rate_and_follows_removed_paths() {
        let mut m = IoMap::for_device("ASIO", "x", 4, 4);
        assert_eq!(m.hardware_latency("Out 3-4", "In 3-4", 48_000), None);
        let at = |frames, rate| HardwareLatency { send: "Out 3-4".into(), ret: "In 3-4".into(), round_trip_frames: frames, rate, measured_at: 7 };
        m.set_hardware_latency(at(300, 48_000));
        m.set_hardware_latency(at(500, 44_100));
        m.set_hardware_latency(at(310, 48_000));
        assert_eq!(m.hardware_latency("out 3-4", "IN 3-4", 48_000), Some(310), "a new measurement replaces the old");
        assert_eq!(m.hardware_latency("Out 3-4", "In 3-4", 44_100), Some(500));
        assert_eq!(m.hardware_latency("Out 3-4", "In 1-2", 48_000), None);
        let back: IoMap = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        let mut gone = m.clone();
        gone.remove_path("In 3-4").unwrap();
        assert!(gone.hardware_latency.is_empty());
        assert!(m.clear_hardware_latency("Out 3-4", "In 3-4") && !m.clear_hardware_latency("Out 3-4", "In 3-4"));
    }

    #[test]
    fn serde_round_trips_and_old_files_default() {
        let m = IoMap::for_device("ASIO", "TASCAM", 12, 10);
        let back: IoMap = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        let empty: IoMap = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, IoMap::default());
    }
}
