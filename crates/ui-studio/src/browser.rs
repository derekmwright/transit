//! The browser (slice 4): a panel on the left with two tabs.
//!
//! **Files** lists the user's locations (view preferences in [`BrowserPrefs`], never in the
//! document) and, once the session is saved, its Audio Files folder, as a lazy tree. A folder's
//! entries are read only when it is expanded, on a background thread; nothing is read ahead and
//! nothing is indexed. Audio files show their length and channel count, read from the header on
//! a background thread for the rows on screen and kept in memory for the session. The UI thread
//! never touches the file system. Maschine and Kontakt files are listed greyed and refused.
//!
//! **Plug-ins** lists the built-in plug-ins and the VST3 and CLAP plug-ins the background scan
//! found (the scan caches only; never a scan), by type and then vendor, with favorites.
//!
//! A drag onto the arrangement, a mixer strip or the device chain, or a double-click, runs engine
//! commands: audio is imported in the background (`crate::import`, one undo step per file),
//! plug-ins load with `mix.instrument` or `mix.insert` (one undo step per drop).
//! Everything else here (tabs, width, expanding, filters, favorites) is view state.
//!
//! Audition is not built: the engine and `Player` have no way to play a file outside the
//! document (plan 06, L29; proposal: an additive `Player::audition`).

use crate::StudioApp;
use crate::arrange::R4;
use crate::icons::Icon;
use crate::shortcuts::{Action as A, pressed};
use crate::theme::TOKENS;
use egui::{Align2, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde_json::json;
use soundcraft_model::{TrackId, TrackKind};
use soundcraft_time::Samples;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

/// Height of a tree or list row.
pub const ROW_H: f32 = 20.0;
const MIN_W: f32 = 200.0;
const MAX_W: f32 = 560.0;
const INDENT: f32 = 14.0;
/// How much of an audio file is read for its header.
const HEADER_BYTES: u64 = 64 * 1024;

/// Extensions `file.import_audio` decodes (the engine's decoders; the same list as the app's
/// Open dialog).
pub const AUDIO_EXT: &[&str] = &["wav", "wave", "bwf", "aif", "aiff", "aifc", "flac", "mp3", "ogg"];
/// Native Instruments formats (Maschine, Kontakt): closed, so listed greyed and never parsed.
pub const CLOSED_EXT: &[&str] =
    &["mxgrp", "mxsnd", "mxprj", "mxinst", "mxfx", "mxkit", "nki", "nkm", "nkx", "nkc", "nkr", "nkb", "ncw", "nksn", "nksf"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Tab {
    #[default]
    Files,
    Plugins,
}

/// The browser's view preferences, kept with [`crate::UiState`] (saved in `ui.json`, never in
/// the document).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BrowserPrefs {
    pub show: bool,
    pub width: f32,
    pub tab: Tab,
    /// Folders the user added (Add Folder).
    pub locations: Vec<String>,
    /// Starred plug-ins, by id.
    pub favorites: Vec<String>,
}

impl Default for BrowserPrefs {
    fn default() -> Self {
        BrowserPrefs { show: false, width: 280.0, tab: Tab::Files, locations: Vec::new(), favorites: Vec::new() }
    }
}

/// What a tree row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Kind {
    Folder,
    Audio,
    /// Maschine or Kontakt: shown greyed, refused on drop.
    Closed,
    /// "Listing…", "Empty folder", a read error.
    Note,
}

/// The kind of a directory entry, or `None` for files the browser leaves out.
pub fn kind_of(name: &str, is_dir: bool) -> Option<Kind> {
    if is_dir {
        return Some(Kind::Folder);
    }
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase)?;
    if AUDIO_EXT.contains(&ext.as_str()) {
        Some(Kind::Audio)
    } else if CLOSED_EXT.contains(&ext.as_str()) {
        Some(Kind::Closed)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    lower: String,
    pub kind: Kind,
}

/// One directory read by the background lister.
#[derive(Debug, Clone, Default)]
pub struct ListResult {
    /// Folders first, then by name (case-insensitive).
    pub entries: Vec<Entry>,
    /// Everything `read_dir` returned.
    pub raw: usize,
    /// Every folder, hidden ones included.
    pub folders: usize,
    /// Hidden and system entries (left out).
    pub hidden: usize,
    /// Files that are neither audio nor Maschine/Kontakt (left out).
    pub other: usize,
}

#[derive(Debug, Clone)]
pub enum Listing {
    Pending { since: Instant },
    Ready { list: ListResult, ms: f32, ui_ms: f32 },
    Failed(String),
}

/// What an audio file's header says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FileInfo {
    pub secs: f64,
    pub channels: u16,
    pub rate: u32,
}

impl FileInfo {
    pub fn label(&self) -> String {
        let len = if self.secs < 60.0 {
            format!("{:.1} s", self.secs)
        } else {
            let s = self.secs.round() as u64;
            format!("{}:{:02}", s / 60, s % 60)
        };
        let ch = match self.channels {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{n} ch"),
        };
        format!("{len} · {ch}")
    }
}

enum Msg {
    Listed { dir: PathBuf, result: Result<ListResult, String>, ms: f32 },
    Info { path: PathBuf, info: Option<FileInfo> },
}

/// What is being dragged out of the browser.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Files(Vec<PathBuf>),
    Plugin { id: String, name: String, instrument: bool },
}

/// Where a drop would land.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum Target {
    /// A track: its lane at a time, or its header, mixer strip or device chain (no time: the cursor).
    Track { track: TrackId, at: Option<Samples> },
    /// Below the tracks: a new track.
    Below { at: Option<Samples> },
}

#[derive(Debug, Clone)]
pub struct Drag {
    pub payload: Payload,
    pub target: Option<(Target, R4)>,
}

/// A tree row as last drawn, for `ui.inspect` and tests. `rect` is `None` when scrolled out.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RowLayout {
    pub path: String,
    pub name: String,
    /// The name as drawn: elided with "…" (extension kept) when the row is too narrow.
    pub shown: String,
    /// The type icon drawn before the name (its Lucide name), empty for notes.
    pub icon: String,
    pub kind: Option<Kind>,
    pub depth: usize,
    pub root: bool,
    pub expanded: bool,
    pub selected: bool,
    pub rect: Option<R4>,
    pub info: Option<String>,
    /// A user location's remove button.
    pub remove: Option<R4>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PluginRowLayout {
    /// "heading", "vendor", "note" or "plugin".
    pub row: &'static str,
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub format: String,
    pub instrument: bool,
    pub favorite: bool,
    pub selected: bool,
    pub rect: Option<R4>,
    pub star: Option<R4>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ListingInfo {
    pub state: &'static str,
    pub ms: f32,
    pub ui_ms: f32,
    pub entries: usize,
    pub raw: usize,
    pub folders: usize,
    pub hidden: usize,
    pub other: usize,
}

/// Last frame's browser geometry and state.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BrowserLayout {
    pub visible: bool,
    pub panel: R4,
    pub handle: R4,
    pub tab: Tab,
    pub tab_files: R4,
    pub tab_plugins: R4,
    pub filter: R4,
    pub add_folder: R4,
    pub add_field: R4,
    pub add_ok: R4,
    pub add_browse: R4,
    pub list: R4,
    pub rows: Vec<RowLayout>,
    pub plugins: Vec<PluginRowLayout>,
    pub scan: String,
    pub listings: BTreeMap<String, ListingInfo>,
    pub pending: usize,
    /// Directories read since launch (one per expand; never ahead of time).
    pub dirs_read: u64,
    pub headers_read: u64,
    /// The browser's own work per frame, and the gap between frames, at most, while a listing ran.
    pub max_panel_ms_while_listing: f32,
    pub max_frame_gap_ms_while_listing: f32,
    /// The panel asked for a repaint every frame (only while a drag is held).
    pub per_frame_repaint: bool,
    /// The panel asked for a repaint in 100 ms (a listing is still pending).
    pub repaint_after_ms: Option<u64>,
    pub dragging: Option<String>,
    pub drop: Option<R4>,
    pub drop_target: Option<Target>,
    pub audition: &'static str,
}

/// A plug-in the Plug-ins tab offers.
#[derive(Debug, Clone)]
struct PluginItem {
    id: String,
    name: String,
    vendor: String,
    format: &'static str,
    instrument: bool,
    category: String,
}

enum PRow {
    Heading(String),
    Vendor(String),
    Note(String),
    Item(usize),
}

/// Runtime state of the browser (never saved).
pub struct BrowserState {
    pub listings: HashMap<PathBuf, Listing>,
    pub expanded: BTreeSet<PathBuf>,
    pub filter: String,
    pub plugin_filter: String,
    pub selected: Vec<PathBuf>,
    anchor: Option<PathBuf>,
    pub plugin_selected: Option<String>,
    info: HashMap<PathBuf, Option<FileInfo>>,
    info_pending: HashSet<PathBuf>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    probe_tx: Option<Sender<PathBuf>>,
    pub drag: Option<Drag>,
    /// The Add Folder field while it is open.
    pub adding: Option<String>,
    /// The Add Folder field had the keyboard last frame (Escape then closes it).
    add_focused: bool,
    scroll: f32,
    plugin_scroll: f32,
    catalog: Option<(String, Vec<PluginItem>)>,
    pub layout: BrowserLayout,
    dirs_read: u64,
    headers_read: u64,
    max_panel_ms: f32,
    max_gap_ms: f32,
    gap_armed: bool,
    serial: u64,
    pub(crate) ctx: Option<egui::Context>,
    /// The last "Importing …" notice, set when a drop or double-click queues an import.
    pub import_notice: String,
}

impl Default for BrowserState {
    fn default() -> Self {
        let (tx, rx) = channel();
        BrowserState {
            listings: HashMap::new(),
            expanded: BTreeSet::new(),
            filter: String::new(),
            plugin_filter: String::new(),
            selected: Vec::new(),
            anchor: None,
            plugin_selected: None,
            info: HashMap::new(),
            info_pending: HashSet::new(),
            tx,
            rx,
            probe_tx: None,
            drag: None,
            adding: None,
            add_focused: false,
            scroll: 0.0,
            plugin_scroll: 0.0,
            catalog: None,
            layout: BrowserLayout::default(),
            dirs_read: 0,
            headers_read: 0,
            max_panel_ms: 0.0,
            max_gap_ms: 0.0,
            gap_armed: false,
            serial: 0,
            ctx: None,
            import_notice: String::new(),
        }
    }
}

impl BrowserState {
    /// A file's length in seconds, when its header has been read for the list.
    pub(crate) fn header_secs(&self, path: &Path) -> Option<f64> {
        self.info.get(path).and_then(|i| i.as_ref()).map(|i| i.secs)
    }

    /// Read `dir` on a background thread (once; a collapse forgets it so the next expand reads it
    /// fresh).
    fn request_listing(&mut self, dir: &Path, ctx: &egui::Context) {
        if matches!(self.listings.get(dir), Some(Listing::Pending { .. } | Listing::Ready { .. })) {
            return;
        }
        let since = Instant::now();
        self.listings.insert(dir.to_path_buf(), Listing::Pending { since });
        self.dirs_read += 1;
        let key = dir.to_path_buf();
        let (tx, dir, ctx) = (self.tx.clone(), dir.to_path_buf(), ctx.clone());
        let spawned = std::thread::Builder::new().name("browser-list".into()).spawn(move || {
            let result = list_dir(&dir);
            let ms = since.elapsed().as_secs_f32() * 1000.0;
            let _ = tx.send(Msg::Listed { dir, result, ms });
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            self.listings.insert(key, Listing::Failed(e.to_string()));
        }
    }

    /// Ask the header reader for `path`'s length and channels.
    fn request_info(&mut self, path: &Path, ctx: &egui::Context) {
        if self.info.contains_key(path) || self.info_pending.contains(path) {
            return;
        }
        if self.probe_tx.is_none() {
            let (ptx, prx) = channel::<PathBuf>();
            let (out, ctx) = (self.tx.clone(), ctx.clone());
            let spawned = std::thread::Builder::new().name("browser-headers".into()).spawn(move || {
                while let Ok(p) = prx.recv() {
                    let info = read_info(&p);
                    if out.send(Msg::Info { path: p, info }).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
            if spawned.is_err() {
                return;
            }
            self.probe_tx = Some(ptx);
        }
        if let Some(tx) = &self.probe_tx
            && tx.send(path.to_path_buf()).is_ok()
        {
            self.info_pending.insert(path.to_path_buf());
            self.headers_read += 1;
        }
    }

    /// Take what the background threads sent.
    fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Listed { dir, result, ms } => {
                    // A listing collapsed (forgotten) while it ran is dropped.
                    let Some(Listing::Pending { since }) = self.listings.get(&dir) else { continue };
                    let ui_ms = since.elapsed().as_secs_f32() * 1000.0;
                    let l = match result {
                        Ok(list) => Listing::Ready { list, ms, ui_ms },
                        Err(e) => Listing::Failed(e),
                    };
                    self.listings.insert(dir, l);
                }
                Msg::Info { path, info } => {
                    self.info_pending.remove(&path);
                    self.info.insert(path, info);
                }
            }
        }
    }

    /// Forget the listings (a pending one included: its late result is discarded), expansion and
    /// selection under `dir`. With `include_dir` the folder's own entries go too. The header
    /// cache stays: it is keyed by file path and never goes stale the way a listing does.
    fn forget_subtree(&mut self, dir: &Path, include_dir: bool) {
        let under = |p: &Path| p.starts_with(dir) && (include_dir || p != dir);
        self.listings.retain(|p, _| !under(p));
        self.expanded.retain(|p| !under(p));
        self.selected.retain(|p| !under(p));
        if self.anchor.as_ref().is_some_and(|a| under(a)) {
            self.anchor = None;
        }
    }

    fn pending(&self) -> usize {
        self.listings.values().filter(|l| matches!(l, Listing::Pending { .. })).count()
    }

    /// The header facts of `path`, if read.
    pub fn info(&self, path: &Path) -> Option<FileInfo> {
        self.info.get(path).copied().flatten()
    }
}

/// Read one directory: no recursion, no file contents. Hidden and system entries are left out.
pub fn list_dir(dir: &Path) -> Result<ListResult, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = ListResult::default();
    for e in rd.flatten() {
        out.raw += 1;
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        let is_dir = ft.is_dir() || (ft.is_symlink() && e.path().is_dir());
        out.folders += usize::from(is_dir);
        if name.starts_with('.') || hidden(&e) {
            out.hidden += 1;
            continue;
        }
        match kind_of(&name, is_dir) {
            Some(kind) => out.entries.push(Entry { lower: name.to_lowercase(), name, kind }),
            None => out.other += 1,
        }
    }
    out.entries.sort_by(|a, b| (a.kind != Kind::Folder).cmp(&(b.kind != Kind::Folder)).then_with(|| a.lower.cmp(&b.lower)));
    Ok(out)
}

#[cfg(windows)]
fn hidden(e: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM; on Windows the entry's metadata comes with
    // the directory listing (no extra read).
    e.metadata().is_ok_and(|m| m.file_attributes() & 0x6 != 0)
}

#[cfg(not(windows))]
fn hidden(_: &std::fs::DirEntry) -> bool {
    false
}

/// The most RIFF chunk headers walked, and the furthest file offset looked at, for one WAV.
const MAX_CHUNKS: usize = 32;
const MAX_WALK_BYTES: u64 = 4 * 1024 * 1024;

/// A file's header, parsed. A WAV is walked chunk by chunk (headers read, bodies skipped by
/// seeking, bounded in count and offset) so a large bext, iXML or artwork chunk before `fmt `
/// does not hide the format; anything else is parsed from its first [`HEADER_BYTES`].
fn read_info(p: &Path) -> Option<FileInfo> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut head = [0u8; 12];
    f.read_exact(&mut head).ok()?;
    if matches!(&head[..4], b"RIFF" | b"RF64") && &head[8..12] == b"WAVE" {
        return read_wav_info(&mut f, head);
    }
    let mut b = head.to_vec();
    f.take(HEADER_BYTES - 12).read_to_end(&mut b).ok()?;
    parse_header(&b)
}

/// Collect `fmt `, `ds64` and the `data` header of a WAV into a small synthetic header and parse
/// that, so the format rules live in [`parse_header`] only.
fn read_wav_info<F: Read + std::io::Seek>(f: &mut F, head: [u8; 12]) -> Option<FileInfo> {
    let mut b = head.to_vec();
    let mut off = 12u64;
    for _ in 0..MAX_CHUNKS {
        if off > MAX_WALK_BYTES {
            return None;
        }
        let mut h = [0u8; 8];
        f.read_exact(&mut h).ok()?;
        let size = u64::from(u32::from_le_bytes([h[4], h[5], h[6], h[7]]));
        let keep = match &h[..4] {
            b"fmt " | b"ds64" => size.min(64) as usize,
            b"data" => {
                b.extend_from_slice(&h);
                return parse_header(&b);
            }
            _ => 0,
        };
        if keep > 0 {
            b.extend_from_slice(&h[..4]);
            b.extend_from_slice(&(keep as u32).to_le_bytes());
            let at = b.len();
            b.resize(at + keep, 0);
            f.read_exact(&mut b[at..]).ok()?;
        }
        let next = off.checked_add(8)?.checked_add(size)?.checked_add(size & 1)?;
        f.seek(std::io::SeekFrom::Start(next)).ok()?;
        // The synthetic chunk is `keep` bytes long (padded to even), whatever the real one was.
        if keep & 1 == 1 {
            b.push(0);
        }
        off = next;
    }
    None
}

fn u16le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}
fn u32be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Length, channels and rate from the start of a WAV (RIFF, RF64), AIFF/AIFF-C or FLAC file;
/// `None` for anything else (MP3 and Ogg have no length in their header).
pub fn parse_header(b: &[u8]) -> Option<FileInfo> {
    match b.get(..4)? {
        b"RIFF" | b"RF64" if b.get(8..12)? == b"WAVE" => {
            let (mut at, mut fmt, mut ds64, mut data) = (12usize, None, None, None);
            while at + 8 <= b.len() {
                let id = b.get(at..at + 4)?;
                let size = u32le(b, at + 4)? as usize;
                let body = at + 8;
                match id {
                    b"fmt " => fmt = Some((u16le(b, body + 2)?, u32le(b, body + 4)?, u16le(b, body + 12)?)),
                    b"ds64" => ds64 = u64le(b, body + 8),
                    b"data" => {
                        data = Some(if size == u32::MAX as usize { ds64? } else { size as u64 });
                        break;
                    }
                    _ => {}
                }
                at = body.checked_add(size)?.checked_add(size & 1)?;
            }
            let ((channels, rate, align), data) = (fmt?, data?);
            (channels > 0 && rate > 0 && align > 0).then(|| FileInfo { secs: (data / u64::from(align)) as f64 / f64::from(rate), channels, rate })
        }
        b"FORM" if matches!(b.get(8..12)?, b"AIFF" | b"AIFC") => {
            let mut at = 12usize;
            while at + 8 <= b.len() {
                let id = b.get(at..at + 4)?;
                let size = u32be(b, at + 4)? as usize;
                let body = at + 8;
                if id == b"COMM" {
                    let channels = u16::from_be_bytes(b.get(body..body + 2)?.try_into().ok()?);
                    let frames = u32be(b, body + 2)?;
                    let rate = extended(b.get(body + 8..body + 18)?)?;
                    return (channels > 0 && rate > 0.0).then(|| FileInfo { secs: f64::from(frames) / rate, channels, rate: rate.round() as u32 });
                }
                at = body.checked_add(size)?.checked_add(size & 1)?;
            }
            None
        }
        b"fLaC" => {
            // The first metadata block is STREAMINFO: rate (20 bits), channels-1 (3), bits-1 (5),
            // total samples (36), from byte 10 of its body.
            let s = b.get(8 + 10..8 + 18)?;
            let rate = (u32::from(s[0]) << 12) | (u32::from(s[1]) << 4) | (u32::from(s[2]) >> 4);
            let channels = u16::from((s[2] >> 1) & 0x7) + 1;
            let total = (u64::from(s[3] & 0x0F) << 32) | u64::from(u32::from_be_bytes([s[4], s[5], s[6], s[7]]));
            (rate > 0).then(|| FileInfo { secs: total as f64 / f64::from(rate), channels, rate })
        }
        _ => None,
    }
}

/// An 80-bit IEEE extended float (AIFF's sample rate).
fn extended(b: &[u8]) -> Option<f64> {
    let exp = i32::from(u16::from_be_bytes([b[0], b[1]]) & 0x7FFF);
    let mant = u64::from_be_bytes(b.get(2..10)?.try_into().ok()?);
    if exp == 0 && mant == 0 {
        return Some(0.0);
    }
    Some(mant as f64 * 2f64.powi(exp - 16383 - 63))
}

/// What a user-typed folder becomes: trimmed, unquoted, and `D:` read as `D:\`.
pub fn normalize_location(s: &str) -> Option<String> {
    let t = s.trim().trim_matches('"').trim();
    if t.is_empty() {
        return None;
    }
    let t = if t.len() == 2 && t.ends_with(':') { format!("{t}\\") } else { t.to_string() };
    Some(t)
}

fn r4(r: Rect) -> R4 {
    [r.min.x, r.min.y, r.max.x, r.max.y]
}

fn rect(r: R4) -> Rect {
    Rect::from_min_max(pos2(r[0], r[1]), pos2(r[2], r[3]))
}

/// `name` fitted to `width`: when too wide, the stem is cut and "…" put before the extension
/// ("Kick Automa….wav"), so the file type stays visible.
fn elide(p: &egui::Painter, name: &str, font: &FontId, width: f32) -> String {
    let w = |s: &str| p.layout_no_wrap(s.to_string(), font.clone(), TOKENS.text).size().x;
    if w(name) <= width {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let chars: Vec<char> = stem.chars().collect();
    for n in (0..chars.len()).rev() {
        let cand = format!("{}…{ext}", chars[..n].iter().collect::<String>().trim_end());
        if w(&cand) <= width {
            return cand;
        }
    }
    format!("…{ext}")
}

pub(crate) fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

/// A row of the Files tree.
struct Row {
    path: PathBuf,
    name: String,
    kind: Kind,
    depth: usize,
    /// A location: `Some(Some(i))` for the user's i-th, `Some(None)` for the session's folder.
    root: Option<Option<usize>>,
}

impl StudioApp {
    /// The session's Audio Files folder, once the session has been saved.
    pub fn session_audio_folder(&self) -> Option<PathBuf> {
        self.engine.path.as_ref()?;
        self.engine.session().folder.as_ref().map(|f| f.join("Audio Files"))
    }

    /// Show or hide the browser (Ctrl+Alt+B, View › Browser).
    pub fn toggle_browser(&mut self) {
        self.ui.browser.show = !self.ui.browser.show;
        if !self.ui.browser.show {
            self.browser.drag = None;
        }
    }

    /// Add a folder to Locations (a view preference).
    pub fn add_location(&mut self, typed: &str) -> bool {
        let Some(loc) = normalize_location(typed) else { return false };
        if self.ui.browser.locations.iter().any(|l| l.eq_ignore_ascii_case(&loc)) {
            self.status = format!("{loc} is already in Locations");
            return false;
        }
        self.ui.browser.locations.push(loc.clone());
        self.status = format!("Added {loc} to Locations");
        true
    }

    pub fn remove_location(&mut self, i: usize) {
        if i < self.ui.browser.locations.len() {
            let loc = self.ui.browser.locations.remove(i);
            let p = PathBuf::from(&loc);
            self.browser.forget_subtree(&p, true);
            self.status = format!("Removed {loc} from Locations");
        }
    }

    /// Expand or collapse a folder. Expanding reads it on a background thread; collapsing
    /// forgets its entries, so the next expand reads it fresh.
    pub fn toggle_folder(&mut self, dir: &Path, ctx: &egui::Context) {
        if self.browser.expanded.contains(dir) {
            self.browser.forget_subtree(dir, true);
        } else {
            self.browser.expanded.insert(dir.to_path_buf());
            self.browser.request_listing(dir, ctx);
        }
    }

    /// Import audio files at `at`: onto `track` (an audio track), or onto a new track when
    /// `track` is `None`. Several files go one after another on the same track. The files are
    /// prepared in the background and committed in order, one undo step each (see `import`).
    pub fn import_files(&mut self, files: &[PathBuf], track: Option<TrackId>, at: Samples) -> Result<(), String> {
        let (ok, closed): (Vec<&PathBuf>, Vec<&PathBuf>) = files.iter().partition(|p| kind_of(&p.to_string_lossy(), false) == Some(Kind::Audio));
        if ok.is_empty() {
            let what = closed.first().map_or("Nothing to import".to_string(), |p| refusal(p));
            self.status = what.clone();
            return Err(what);
        }
        if let Some(t) = track {
            let s = self.engine.session();
            let Some(tr) = s.track(t) else { return Err("no such track".into()) };
            if tr.kind != TrackKind::Audio {
                self.status = format!("{} is not an audio track: drop audio files on an audio track or below the tracks", tr.name);
                return Err(self.status.clone());
            }
        }
        let what = if ok.len() == 1 { file_name(ok[0]) } else { format!("{} files", ok.len()) };
        let files: Vec<PathBuf> = ok.into_iter().cloned().collect();
        self.queue_imports(&files, track, at.max(0), closed.len())?;
        self.status = format!("Importing {what}…");
        self.browser.import_notice = self.status.clone();
        Ok(())
    }

    /// Double-click on a file: import at the cursor on the focused (or cursor's) audio track, or
    /// on a new track when neither is an audio track.
    pub fn import_at_cursor(&mut self, files: &[PathBuf]) -> Result<(), String> {
        let s = self.engine.session_arc();
        let audio = |t: &TrackId| s.track(*t).is_some_and(|t| t.kind == TrackKind::Audio);
        let track = [self.focus_track(), self.cursor_track()].into_iter().flatten().find(audio);
        self.import_files(files, track, self.cursor())
    }

    /// Put a plug-in where it was dropped: an instrument on an instrument track (or a new
    /// instrument track below the tracks), an effect at the end of a track's chain.
    pub fn drop_plugin(&mut self, id: &str, name: &str, instrument: bool, target: Target) -> Result<(), String> {
        let track = match target {
            Target::Track { track, .. } => track,
            Target::Below { .. } if instrument => return self.new_instrument_track(id, name),
            Target::Below { .. } => {
                self.status = format!("Drop {name} on a track, a mixer strip or the device chain");
                return Err(self.status.clone());
            }
        };
        let Some(t) = self.engine.session().track(track).map(|t| (t.kind, t.name.clone())) else { return Err("no such track".into()) };
        if instrument {
            if t.0 != TrackKind::Instrument {
                self.status = format!("{name} is an instrument: drop it on an instrument track (or below the tracks for a new one)");
                return Err(self.status.clone());
            }
            self.set_instrument(track, id)?;
            self.status = format!("Loaded {name} on {}", t.1);
        } else {
            let before = self.status.clone();
            self.add_device(track, id)?;
            if self.status == before {
                self.status = format!("Added {name} to {}", t.1);
            }
        }
        self.ui.focus_track = Some(track);
        Ok(())
    }

    /// Double-click on a plug-in: the same as a drop on the focused track.
    pub fn plugin_to_focus(&mut self, id: &str, name: &str, instrument: bool) -> Result<(), String> {
        let Some(track) = self.focus_track() else {
            self.status = format!("Click a track first, then double-click {name}");
            return Err(self.status.clone());
        };
        self.drop_plugin(id, name, instrument, Target::Track { track, at: None })
    }

    /// A new instrument track playing `id`: `track.new` and `mix.instrument` as one undo step.
    fn new_instrument_track(&mut self, id: &str, name: &str) -> Result<(), String> {
        let before = self.engine.session_arc();
        self.browser.serial += 1;
        let key = format!("browser:instrument:{}", self.browser.serial);
        self.engine.end_merge();
        let r = self.engine.execute_merged("track.new", &json!({"kind": "instrument"}), &key).and_then(|v| {
            let t = v["tracks"][0].as_u64().unwrap_or_default();
            self.engine.execute_merged("mix.instrument", &json!({"track": t, "plugin": id}), &key).map(|_| TrackId(t))
        });
        self.engine.end_merge();
        match r {
            Ok(t) => {
                let tn = self.engine.session().track(t).map_or_else(String::new, |t| t.name.clone());
                self.status = format!("Loaded {name} on a new track, {tn}");
                self.ui.focus_track = Some(t);
                Ok(())
            }
            Err(e) => {
                if !std::sync::Arc::ptr_eq(&before, &self.engine.session_arc()) {
                    self.engine.undo();
                }
                self.status = format!("{name}: {e}");
                Err(self.status.clone())
            }
        }
    }

    /// What lies under `pos` for a drop, from last frame's arrangement, mixer and device layouts.
    fn drop_target(&self, pos: Pos2, snap: bool) -> Option<(Target, R4)> {
        let m = &self.mixer.layout;
        if m.visible {
            for s in m.strips.iter().chain(m.master.iter()) {
                if rect(s.rect).contains(pos) {
                    return Some((Target::Track { track: TrackId(s.track), at: None }, s.rect));
                }
            }
            if rect(m.area).contains(pos) {
                return None;
            }
        }
        let d = &self.mixer.devices;
        if d.visible && rect(d.area).contains(pos) {
            return d.track.map(|t| (Target::Track { track: TrackId(t), at: None }, d.area));
        }
        if self.ui.mixer_full || self.keys.layout.visible && rect(self.keys.layout.pane).contains(pos) {
            return None;
        }
        let l = &self.arrange.layout;
        let (tl, list) = (rect(l.timeline), rect(l.track_list));
        let time = |x: f32| {
            let at = (l.scroll + (f64::from(x - tl.min.x) * l.samples_per_px).round() as Samples).max(0);
            if snap { snap_to_beat(self, at) } else { at }
        };
        for row in &l.tracks {
            if tl.contains(pos) && rect(row.lane).contains(pos) {
                return Some((Target::Track { track: TrackId(row.id), at: Some(time(pos.x)) }, row.lane));
            }
            if list.contains(pos) && rect(row.header).contains(pos) {
                return Some((Target::Track { track: TrackId(row.id), at: None }, row.header));
            }
        }
        let bottom = l.tracks.last().map_or(tl.min.y, |r| r.lane[3]);
        if pos.y >= bottom && (tl.contains(pos) || list.contains(pos)) {
            let band = [list.min.x, bottom, tl.max.x, (bottom + 40.0).min(tl.max.y)];
            return Some((Target::Below { at: tl.contains(pos).then(|| time(pos.x)) }, band));
        }
        None
    }

    /// Run a finished drag.
    fn drop_payload(&mut self, payload: Payload, target: Target) {
        match payload {
            Payload::Files(files) => {
                let (track, at) = match target {
                    Target::Track { track, at } => (Some(track), at),
                    Target::Below { at } => (None, at),
                };
                let _ = self.import_files(&files, track, at.unwrap_or_else(|| self.cursor()));
            }
            Payload::Plugin { id, name, instrument } => {
                let _ = self.drop_plugin(&id, &name, instrument, target);
            }
        }
    }
}

/// Why a file can't be imported.
fn refusal(p: &Path) -> String {
    match kind_of(&p.to_string_lossy(), false) {
        Some(Kind::Closed) => {
            format!("{}: Maschine/Kontakt files are not supported (their samples are WAV files you can drag instead)", file_name(p))
        }
        _ => format!("{}: not an audio file this app imports", file_name(p)),
    }
}

/// The nearest beat to `at`.
fn snap_to_beat(app: &StudioApp, at: Samples) -> Samples {
    let s = app.engine.session();
    let q = soundcraft_time::TICKS_PER_QUARTER;
    let t = s.tempo.samples_to_ticks(at, s.sample_rate);
    let beat = ((t as f64 / q as f64).round() as i64) * q;
    s.tempo.tick_to_samples(beat.max(0), s.sample_rate)
}

/// `2.1.1`: bar, beat, sixteenth.
pub fn bbs(app: &StudioApp, at: Samples) -> String {
    let s = app.engine.session();
    let bb = s.tempo.bar_beat_at(at.max(0), s.sample_rate);
    format!("{}.{}.{}", bb.bar, bb.beat, bb.tick / (soundcraft_time::TICKS_PER_QUARTER / 4) + 1)
}

/// Ctrl+Alt+B shows and hides the browser; Escape closes the Add Folder field it is typing in.
pub fn shortcuts(app: &mut StudioApp, ctx: &egui::Context) {
    // egui drops the field's focus on Escape before the frame runs, so this goes first.
    if app.browser.adding.is_some() && app.browser.add_focused && pressed(ctx, A::BrowserCancel) {
        app.browser.adding = None;
        app.browser.add_focused = false;
        return;
    }
    // Escape during a drag cancels it: no drop, no message (and no other Escape action).
    if app.browser.drag.is_some() && pressed(ctx, A::BrowserCancel) {
        app.browser.drag = None;
        return;
    }
    // Escape while files are being imported cancels them (L30).
    if app.imports.active() && !ctx.egui_wants_keyboard_input() && app.dialog.is_none() && pressed(ctx, A::BrowserCancel) {
        app.cancel_imports(None);
        return;
    }
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() {
        return;
    }
    if pressed(ctx, A::ToggleBrowser) {
        app.toggle_browser();
    }
}

/// The browser panel, on the left between the transport and the status bar. Call before the
/// bottom panes and the arrangement.
pub fn panel(app: &mut StudioApp, ui: &mut Ui) {
    let t0 = Instant::now();
    let ctx = ui.ctx().clone();
    app.browser.poll();
    drive_drag(app, &ctx);
    let listing = app.browser.pending() > 0;
    if listing {
        // The first frame after the request follows whatever idle time came before it.
        let gap = ctx.input(|i| i.unstable_dt) * 1000.0;
        if app.browser.gap_armed {
            app.browser.max_gap_ms = app.browser.max_gap_ms.max(gap);
        }
        app.browser.gap_armed = true;
        // Poll, don't spin: the lister wakes the UI itself when it answers.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    } else {
        app.browser.gap_armed = false;
    }
    if app.ui.browser.show {
        let w = app.ui.browser.width.clamp(MIN_W, MAX_W);
        egui::Panel::left("browser").exact_size(w).resizable(false).frame(egui::Frame::NONE.fill(TOKENS.panel)).show(ui, |ui| body(app, ui));
    } else {
        app.browser.layout.visible = false;
        app.browser.layout.rows.clear();
        app.browser.layout.plugins.clear();
    }
    let b = &mut app.browser;
    b.ctx = Some(ctx.clone());
    b.layout.per_frame_repaint = b.drag.is_some();
    b.layout.repaint_after_ms = listing.then_some(100);
    let mut listings = BTreeMap::new();
    for (p, l) in &b.listings {
        let info = match l {
            Listing::Pending { .. } => ListingInfo { state: "pending", ..ListingInfo::default() },
            Listing::Failed(_) => ListingInfo { state: "failed", ..ListingInfo::default() },
            Listing::Ready { list, ms, ui_ms } => ListingInfo {
                state: "ready",
                ms: *ms,
                ui_ms: *ui_ms,
                entries: list.entries.len(),
                raw: list.raw,
                folders: list.folders,
                hidden: list.hidden,
                other: list.other,
            },
        };
        listings.insert(p.to_string_lossy().into_owned(), info);
    }
    b.layout.listings = listings;
    b.layout.pending = b.pending();
    b.layout.dirs_read = b.dirs_read;
    b.layout.headers_read = b.headers_read;
    b.layout.max_frame_gap_ms_while_listing = b.max_gap_ms;
    b.layout.dragging = b.drag.as_ref().map(|d| match &d.payload {
        Payload::Files(f) if f.len() == 1 => file_name(&f[0]),
        Payload::Files(f) => format!("{} files", f.len()),
        Payload::Plugin { name, .. } => name.clone(),
    });
    b.layout.drop = b.drag.as_ref().and_then(|d| d.target.map(|t| t.1));
    b.layout.drop_target = b.drag.as_ref().and_then(|d| d.target.map(|t| t.0));
    b.layout.audition = "not built (L29)";
    if listing {
        b.max_panel_ms = b.max_panel_ms.max(t0.elapsed().as_secs_f32() * 1000.0);
    }
    b.layout.max_panel_ms_while_listing = b.max_panel_ms;
}

/// A drag out of the browser: while held, show where it would land; on release, drop it there.
fn drive_drag(app: &mut StudioApp, ctx: &egui::Context) {
    let Some(drag) = app.browser.drag.clone() else { return };
    let (down, pos, shift) = ctx.input(|i| (i.pointer.primary_down(), i.pointer.latest_pos(), i.modifiers.shift));
    let target = pos.and_then(|p| app.drop_target(p, !shift));
    if down {
        if let Some(d) = &mut app.browser.drag {
            d.target = target;
        }
        let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("browser-drag")));
        if let Some((_, r)) = target {
            p.rect_stroke(rect(r).shrink(1.0), crate::theme::RADIUS_SM, Stroke::new(2.0, TOKENS.accent), StrokeKind::Inside);
        }
        if let Some(pos) = pos {
            let label = match &drag.payload {
                Payload::Files(f) if f.len() == 1 => file_name(&f[0]),
                Payload::Files(f) => format!("{} files", f.len()),
                Payload::Plugin { name, .. } => name.clone(),
            };
            let at = pos + vec2(14.0, 10.0);
            let galley = p.layout_no_wrap(label, FontId::proportional(12.0), TOKENS.text);
            let r = Rect::from_min_size(at, galley.size() + vec2(12.0, 6.0));
            p.rect_filled(r, crate::theme::RADIUS, TOKENS.panel_raised);
            p.galley(at + vec2(6.0, 3.0), galley, TOKENS.text);
        }
        ctx.set_cursor_icon(if target.is_some() { CursorIcon::Grabbing } else { CursorIcon::NoDrop });
        ctx.request_repaint();
        return;
    }
    app.browser.drag = None;
    if let Some((t, _)) = target {
        app.drop_payload(drag.payload, t);
    } else {
        app.status = "Drop onto a track lane, header, strip or the device chain".to_string();
    }
}

fn body(app: &mut StudioApp, ui: &mut Ui) {
    let area = ui.max_rect();
    let mut layout = BrowserLayout { visible: true, panel: r4(area), tab: app.ui.browser.tab, ..BrowserLayout::default() };
    let p = ui.painter().clone();
    p.line_segment([pos2(area.max.x - 0.5, area.min.y), pos2(area.max.x - 0.5, area.max.y)], Stroke::new(1.0, TOKENS.line));
    // Tabs.
    let tabs_r = Rect::from_min_size(area.min, vec2(area.width(), 28.0));
    let half = (tabs_r.width() - 6.0) / 2.0;
    for (i, (label, tab)) in [("Files", Tab::Files), ("Plug-ins", Tab::Plugins)].into_iter().enumerate() {
        let r = Rect::from_min_size(pos2(tabs_r.min.x + i as f32 * half, tabs_r.min.y), vec2(half, tabs_r.height()));
        let resp = ui.interact(r, ui.id().with(("browser_tab", i)), Sense::click());
        let on = app.ui.browser.tab == tab;
        let ink = if on || resp.hovered() { TOKENS.text } else { TOKENS.text_dim };
        p.text(r.center(), Align2::CENTER_CENTER, label, FontId::proportional(12.5), ink);
        if on {
            p.line_segment([pos2(r.min.x + 10.0, r.max.y - 1.5), pos2(r.max.x - 10.0, r.max.y - 1.5)], Stroke::new(2.0, TOKENS.accent));
        }
        if resp.clicked() {
            app.ui.browser.tab = tab;
        }
        if i == 0 {
            layout.tab_files = r4(r);
        } else {
            layout.tab_plugins = r4(r);
        }
    }
    p.line_segment([pos2(area.min.x, tabs_r.max.y), pos2(area.max.x, tabs_r.max.y)], Stroke::new(1.0, TOKENS.line));
    // Filter box (one per tab).
    let fr = Rect::from_min_max(pos2(area.min.x + 8.0, tabs_r.max.y + 6.0), pos2(area.max.x - 10.0, tabs_r.max.y + 28.0));
    let files = app.ui.browser.tab == Tab::Files;
    {
        let text = if files { &mut app.browser.filter } else { &mut app.browser.plugin_filter };
        let id = ui.id().with(if files { "browser_filter" } else { "browser_plugin_filter" });
        let margin = egui::Margin { left: 24, right: 4, top: 2, bottom: 2 };
        ui.put(fr, egui::TextEdit::singleline(text).id(id).hint_text("Filter").margin(margin).desired_width(fr.width()));
        crate::icons::paint(&p, pos2(fr.min.x + 12.0, fr.center().y), Icon::Filter, 13.0, TOKENS.text_dim);
    }
    layout.filter = r4(fr);
    let mut top = fr.max.y + 6.0;
    if files {
        top = locations_bar(app, ui, &p, area, top, &mut layout);
    } else {
        layout.scan = scan_note().unwrap_or_default();
    }
    let list = Rect::from_min_max(pos2(area.min.x, top), pos2(area.max.x - 4.0, area.max.y));
    layout.list = r4(list);
    if files {
        tree(app, ui, list, &mut layout);
    } else {
        plugins(app, ui, list, &mut layout);
    }
    // Width: drag the right edge.
    let handle = Rect::from_min_max(pos2(area.max.x - 5.0, area.min.y), pos2(area.max.x + 1.0, area.max.y));
    let resp = ui.interact(handle, ui.id().with("browser_width"), Sense::drag());
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        p.line_segment([pos2(area.max.x - 1.0, area.min.y), pos2(area.max.x - 1.0, area.max.y)], Stroke::new(2.0, TOKENS.accent));
    }
    if resp.dragged() {
        let w = app.ui.browser.width.clamp(MIN_W, MAX_W);
        app.ui.browser.width = (w + resp.drag_delta().x).clamp(MIN_W, MAX_W);
    }
    layout.handle = r4(handle);
    let keep = std::mem::take(&mut app.browser.layout);
    app.browser.layout = BrowserLayout { listings: keep.listings, ..layout };
}

/// "Locations" with "+" (Add Folder); while adding, a path field with Add and Browse….
fn locations_bar(app: &mut StudioApp, ui: &mut Ui, p: &egui::Painter, area: Rect, top: f32, layout: &mut BrowserLayout) -> f32 {
    let bar = Rect::from_min_max(pos2(area.min.x + 8.0, top), pos2(area.max.x - 10.0, top + 20.0));
    p.text(bar.left_center(), Align2::LEFT_CENTER, "Locations", FontId::proportional(10.5), TOKENS.text_dim);
    let plus = Rect::from_min_size(pos2(bar.max.x - 20.0, bar.min.y + 1.0), vec2(20.0, 18.0));
    let resp = ui.interact(plus, ui.id().with("browser_add_folder"), Sense::click());
    p.rect_filled(plus, crate::theme::RADIUS, if resp.hovered() { TOKENS.button_hover } else { TOKENS.button });
    crate::icons::paint(p, plus.center(), Icon::Plus, 14.0, TOKENS.text);
    if resp.on_hover_text("Add Folder…").clicked() {
        app.browser.adding = if app.browser.adding.is_some() { None } else { Some(String::new()) };
    }
    layout.add_folder = r4(plus);
    let mut y = bar.max.y + 4.0;
    if app.browser.adding.is_some() {
        let browse_w = if app.services.pick_open.is_some() { 64.0 } else { 0.0 };
        let field = Rect::from_min_max(pos2(bar.min.x, y), pos2(bar.max.x - 44.0 - browse_w, y + 22.0));
        let ok = Rect::from_min_size(pos2(field.max.x + 4.0, y), vec2(40.0, 22.0));
        let id = ui.id().with("browser_add_field");
        let mut text = app.browser.adding.take().unwrap_or_default();
        let fresp = ui.put(field, egui::TextEdit::singleline(&mut text).id(id).hint_text("Folder path, e.g. D:\\").desired_width(field.width()));
        let enter = fresp.lost_focus() && ui.input(|i| crate::shortcuts::hit(i, A::BrowserAddFolder));
        let oresp = ui.interact(ok, ui.id().with("browser_add_ok"), Sense::click());
        p.rect_filled(ok, crate::theme::RADIUS, if oresp.hovered() { TOKENS.button_hover } else { TOKENS.button });
        p.text(ok.center(), Align2::CENTER_CENTER, "Add", FontId::proportional(11.5), TOKENS.text);
        let mut done = false;
        if (enter || oresp.clicked()) && app.add_location(&text) {
            done = true;
        }
        if browse_w > 0.0 {
            let br = Rect::from_min_size(pos2(ok.max.x + 4.0, y), vec2(browse_w - 4.0, 22.0));
            let bresp = ui.interact(br, ui.id().with("browser_add_browse"), Sense::click());
            p.rect_filled(br, crate::theme::RADIUS, if bresp.hovered() { TOKENS.button_hover } else { TOKENS.button });
            p.text(br.center(), Align2::CENTER_CENTER, "Browse…", FontId::proportional(11.5), TOKENS.text);
            if bresp.clicked()
                && let Some(dir) = app.services.pick_open.as_ref().and_then(|f| f("folder"))
                && app.add_location(&dir)
            {
                done = true;
            }
            layout.add_browse = r4(br);
        }
        app.browser.add_focused = fresp.has_focus();
        if !done {
            app.browser.adding = Some(text);
        }
        layout.add_field = r4(field);
        layout.add_ok = r4(ok);
        y = field.max.y + 4.0;
    }
    y
}

/// The rows of the Files tree, in display order.
fn tree_rows(app: &StudioApp) -> Vec<Row> {
    let b = &app.browser;
    let f = b.filter.trim().to_lowercase();
    let mut out = Vec::new();
    let roots = app.ui.browser.locations.iter().enumerate().map(|(i, l)| (PathBuf::from(l), l.clone(), Some(i)));
    let session = app.session_audio_folder().map(|p| (p, "Audio Files (this session)".to_string(), None));
    for (path, name, user) in roots.chain(session) {
        let expanded = b.expanded.contains(&path);
        out.push(Row { path: path.clone(), name, kind: Kind::Folder, depth: 0, root: Some(user) });
        if expanded {
            walk(b, &path, 1, &f, false, &mut out);
        }
    }
    out
}

/// The rows under `dir` that pass the filter: an entry whose name matches shows with everything
/// expanded under it; a folder also shows when something under it matches.
fn walk(b: &BrowserState, dir: &Path, depth: usize, f: &str, forced: bool, out: &mut Vec<Row>) -> bool {
    let all = f.is_empty() || forced;
    let note = |out: &mut Vec<Row>, text: String| {
        if all {
            out.push(Row { path: dir.join(format!("<{text}>")), name: text, kind: Kind::Note, depth, root: None });
        }
    };
    match b.listings.get(dir) {
        None | Some(Listing::Pending { .. }) => {
            note(out, "Listing…".into());
            false
        }
        Some(Listing::Failed(e)) => {
            note(out, format!("Can't read this folder ({e})"));
            false
        }
        Some(Listing::Ready { list, .. }) => {
            if list.entries.is_empty() {
                note(out, "No folders or audio files".into());
            }
            let mut any = false;
            for e in &list.entries {
                let path = dir.join(&e.name);
                let m = all || e.lower.contains(f);
                if e.kind == Kind::Folder {
                    let mut sub = Vec::new();
                    let below = b.expanded.contains(&path) && walk(b, &path, depth + 1, f, m, &mut sub);
                    if m || below {
                        out.push(Row { path, name: e.name.clone(), kind: e.kind, depth, root: None });
                        out.append(&mut sub);
                        any = true;
                    }
                } else if m {
                    out.push(Row { path, name: e.name.clone(), kind: e.kind, depth, root: None });
                    any = true;
                }
            }
            any
        }
    }
}

/// Wheel scroll of a list: returns the new offset.
fn wheel(ui: &Ui, area: Rect, scroll: f32, content_h: f32) -> f32 {
    let hovered = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| area.contains(p));
    let dy = if hovered { ui.input(|i| i.smooth_scroll_delta.y) } else { 0.0 };
    (scroll - dy).clamp(0.0, (content_h - area.height()).max(0.0))
}

fn tree(app: &mut StudioApp, ui: &mut Ui, area: Rect, layout: &mut BrowserLayout) {
    let ctx = ui.ctx().clone();
    let rows = tree_rows(app);
    let content_h = rows.len() as f32 * ROW_H + 8.0;
    app.browser.scroll = wheel(ui, area, app.browser.scroll, content_h);
    let p = ui.painter().with_clip_rect(area);
    if rows.is_empty() {
        p.text(
            area.min + vec2(12.0, 14.0),
            Align2::LEFT_CENTER,
            "Add a folder with + to browse its samples",
            FontId::proportional(11.5),
            TOKENS.text_dim,
        );
    }
    let mods = ui.input(|i| i.modifiers);
    let mut clicked_file: Option<(usize, bool, bool)> = None;
    let mut toggle: Option<PathBuf> = None;
    let mut remove: Option<usize> = None;
    let mut double: Option<PathBuf> = None;
    let mut drag_from: Option<PathBuf> = None;
    for (i, row) in rows.iter().enumerate() {
        let y = area.min.y + i as f32 * ROW_H - app.browser.scroll;
        let r = Rect::from_min_size(pos2(area.min.x, y), vec2(area.width(), ROW_H));
        let selected = app.browser.selected.contains(&row.path);
        let expanded = row.kind == Kind::Folder && app.browser.expanded.contains(&row.path);
        let mut out = RowLayout {
            path: row.path.to_string_lossy().into_owned(),
            name: row.name.clone(),
            shown: row.name.clone(),
            kind: Some(row.kind),
            depth: row.depth,
            root: row.root.is_some(),
            expanded,
            selected,
            ..RowLayout::default()
        };
        if row.kind == Kind::Audio
            && let Some(info) = app.browser.info.get(&row.path)
        {
            out.info = Some(info.map_or_else(|| "–".to_string(), |i| i.label()));
        }
        if r.max.y < area.min.y || r.min.y > area.max.y {
            layout.rows.push(out);
            continue;
        }
        out.rect = Some(r4(r));
        let resp = ui.interact(r.intersect(area), ui.id().with(("browser_row", &row.path)), Sense::click_and_drag());
        if selected {
            p.rect_filled(r, 0.0, TOKENS.accent.gamma_multiply(0.28));
        } else if resp.hovered() && row.kind != Kind::Note {
            p.rect_filled(r, 0.0, TOKENS.button);
        }
        let x = r.min.x + 8.0 + row.depth as f32 * INDENT;
        let c = pos2(x + 5.0, r.center().y);
        if row.kind == Kind::Folder {
            crate::icons::paint(&p, c, if expanded { Icon::ChevronDown } else { Icon::ChevronRight }, 12.0, TOKENS.text_dim);
        }
        let ink = match row.kind {
            Kind::Folder | Kind::Audio => TOKENS.text,
            Kind::Closed | Kind::Note => TOKENS.text_dim.gamma_multiply(0.75),
        };
        let right = match row.kind {
            Kind::Audio => {
                if !app.browser.info.contains_key(&row.path) {
                    app.browser.request_info(&row.path, &ctx);
                }
                out.info.clone().unwrap_or_default()
            }
            Kind::Closed => "Maschine/Kontakt (not supported)".to_string(),
            _ => String::new(),
        };
        let right_w = if right.is_empty() { 0.0 } else { (right.chars().count() as f32 * 5.6).min(r.width() * 0.55) };
        // The entry's type: folder (open or closed), audio file, or a file we can't use.
        let kind_icon = match row.kind {
            Kind::Folder if expanded => Some(Icon::FolderOpen),
            Kind::Folder => Some(Icon::Folder),
            Kind::Audio => Some(Icon::FileAudio),
            Kind::Closed => Some(Icon::File),
            Kind::Note => None,
        };
        if let Some(i) = kind_icon {
            crate::icons::paint(&p, pos2(x + 20.0, r.center().y), i, 13.0, ink);
            out.icon = i.name().to_string();
        }
        let name_x = if kind_icon.is_some() { x + 30.0 } else { x + 14.0 };
        let name_r = Rect::from_min_max(pos2(name_x, r.min.y), pos2(r.max.x - 10.0 - right_w, r.max.y));
        let size = if row.root.is_some() { 12.5 } else { 12.0 };
        let font = FontId::proportional(size);
        let shown = elide(&p, &row.name, &font, name_r.width());
        p.with_clip_rect(name_r.intersect(area)).text(name_r.left_center(), Align2::LEFT_CENTER, &shown, font, ink);
        out.shown = shown;
        if !right.is_empty() {
            p.with_clip_rect(r.intersect(area)).text(
                pos2(r.max.x - 8.0, r.center().y),
                Align2::RIGHT_CENTER,
                &right,
                FontId::proportional(10.5),
                TOKENS.text_dim.gamma_multiply(if row.kind == Kind::Closed { 0.7 } else { 1.0 }),
            );
        }
        if resp.clicked() {
            match row.kind {
                Kind::Folder => toggle = Some(row.path.clone()),
                Kind::Audio | Kind::Closed => clicked_file = Some((i, mods.command || mods.ctrl, mods.shift)),
                Kind::Note => {}
            }
        }
        if resp.double_clicked() && row.kind == Kind::Audio {
            double = Some(row.path.clone());
        }
        if resp.drag_started() && matches!(row.kind, Kind::Audio | Kind::Closed) {
            drag_from = Some(row.path.clone());
        }
        // A user location's remove button, on hover.
        if let Some(Some(li)) = row.root {
            let xb = Rect::from_min_size(pos2(r.max.x - 22.0, r.min.y + 2.0), vec2(16.0, 16.0));
            let hovering = ui.input(|i| i.pointer.hover_pos()).is_some_and(|h| r.contains(h));
            let xresp = ui.interact(xb, ui.id().with(("browser_remove", li)), Sense::click());
            if hovering || xresp.hovered() {
                crate::icons::paint(&p, xb.center(), Icon::Remove, 13.0, if xresp.hovered() { TOKENS.text } else { TOKENS.text_dim });
            }
            if xresp.on_hover_text("Remove from Locations").clicked() {
                remove = Some(li);
            }
            out.remove = Some(r4(xb));
        }
        layout.rows.push(out);
    }
    // Selection: click selects, Ctrl toggles, Shift ranges (in display order).
    if let Some((i, ctrl, shift)) = clicked_file {
        let path = rows[i].path.clone();
        let anchor = app.browser.anchor.as_ref().and_then(|a| rows.iter().position(|r| &r.path == a));
        if shift && let Some(a) = anchor {
            let (lo, hi) = (a.min(i), a.max(i));
            app.browser.selected = rows[lo..=hi].iter().filter(|r| matches!(r.kind, Kind::Audio | Kind::Closed)).map(|r| r.path.clone()).collect();
        } else if ctrl {
            if let Some(k) = app.browser.selected.iter().position(|p| *p == path) {
                app.browser.selected.remove(k);
            } else {
                app.browser.selected.push(path.clone());
            }
            app.browser.anchor = Some(path);
        } else {
            app.browser.selected = vec![path.clone()];
            app.browser.anchor = Some(path);
        }
    }
    if let Some(dir) = toggle {
        app.toggle_folder(&dir, &ctx);
    }
    if let Some(i) = remove {
        app.remove_location(i);
    }
    // Several selected files go in display order.
    let in_order = |sel: &[PathBuf]| rows.iter().filter(|r| sel.contains(&r.path)).map(|r| r.path.clone()).collect::<Vec<_>>();
    if let Some(path) = double {
        let files = if app.browser.selected.contains(&path) { in_order(&app.browser.selected) } else { vec![path] };
        let _ = app.import_at_cursor(&files);
    }
    if let Some(path) = drag_from {
        if !app.browser.selected.contains(&path) {
            app.browser.selected = vec![path.clone()];
            app.browser.anchor = Some(path);
        }
        let files = in_order(&app.browser.selected);
        app.browser.drag = Some(Drag { payload: Payload::Files(files), target: None });
    }
}

/// "Scanning…", "Not scanned yet…" or a failure, for the Plug-ins tab; `None` once scanned.
fn scan_note() -> Option<String> {
    if let Some(s) = crate::plugin_scan::scanning_label() {
        return Some(s);
    }
    match crate::plugin_scan::state().as_str() {
        "idle" => Some("VST3 and CLAP: not scanned yet (Audio › Scan Plug-ins)".into()),
        "done" => None,
        other => Some(format!("Scan {other}")),
    }
}

/// Built-in plug-ins plus the scan caches, refreshed when the scan state changes.
fn catalog(app: &mut StudioApp) -> Vec<PluginItem> {
    let state = crate::plugin_scan::state();
    if let Some((s, items)) = &app.browser.catalog
        && *s == state
    {
        return items.clone();
    }
    let mut items: Vec<PluginItem> = app
        .builtin_plugins()
        .into_iter()
        .map(|e| PluginItem { id: e.id, name: e.name, vendor: "Built-in".into(), format: "Built-in", instrument: e.instrument, category: e.category })
        .collect();
    for (format, list) in [("VST3", crate::plugin_scan::cached_vst3()), ("CLAP", crate::plugin_scan::cached_clap())] {
        for crate::plugin_scan::CachedPlugin { id, name, vendor, instrument } in list.unwrap_or_default() {
            let vendor = if vendor.trim().is_empty() { "Other".to_string() } else { vendor };
            items.push(PluginItem { id, name, vendor, format, instrument, category: String::new() });
        }
    }
    app.browser.catalog = Some((state, items.clone()));
    items
}

/// The Plug-ins tab's rows: favorites, then instruments, then effects, each by vendor
/// (built-in first).
fn plugin_rows(items: &[PluginItem], favorites: &[String], filter: &str) -> Vec<PRow> {
    let f = filter.trim().to_lowercase();
    let keep = |p: &PluginItem| {
        f.is_empty() || [p.name.as_str(), p.vendor.as_str(), p.format, p.category.as_str()].iter().any(|s| s.to_lowercase().contains(&f))
    };
    let mut out = Vec::new();
    let favs: Vec<usize> = favorites.iter().filter_map(|id| items.iter().position(|p| &p.id == id)).filter(|i| keep(&items[*i])).collect();
    if !favs.is_empty() {
        out.push(PRow::Heading("Favorites".into()));
        out.extend(favs.into_iter().map(PRow::Item));
    }
    for (heading, instrument) in [("Instruments", true), ("Effects", false)] {
        let mut idx: Vec<usize> = (0..items.len()).filter(|i| items[*i].instrument == instrument && keep(&items[*i])).collect();
        idx.sort_by_key(|i| {
            let p = &items[*i];
            (p.vendor != "Built-in", p.vendor.to_lowercase(), p.name.to_lowercase())
        });
        out.push(PRow::Heading(heading.into()));
        if idx.is_empty() {
            out.push(PRow::Note(if f.is_empty() { "None".into() } else { "No match".into() }));
        }
        let mut vendor: Option<&str> = None;
        for i in idx {
            let v = items[i].vendor.as_str();
            if vendor != Some(v) {
                out.push(PRow::Vendor(v.to_string()));
                vendor = Some(v);
            }
            out.push(PRow::Item(i));
        }
    }
    out
}

fn plugins(app: &mut StudioApp, ui: &mut Ui, area: Rect, layout: &mut BrowserLayout) {
    let items = catalog(app);
    let mut rows = Vec::new();
    if let Some(note) = scan_note() {
        rows.push(PRow::Note(note));
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
    rows.extend(plugin_rows(&items, &app.ui.browser.favorites, &app.browser.plugin_filter));
    let content_h = rows.len() as f32 * ROW_H + 8.0;
    app.browser.plugin_scroll = wheel(ui, area, app.browser.plugin_scroll, content_h);
    let p = ui.painter().with_clip_rect(area);
    let mut star_click: Option<String> = None;
    let mut pick: Option<(usize, bool)> = None;
    let mut drag_from: Option<usize> = None;
    for (i, row) in rows.iter().enumerate() {
        let y = area.min.y + i as f32 * ROW_H - app.browser.plugin_scroll;
        let r = Rect::from_min_size(pos2(area.min.x, y), vec2(area.width(), ROW_H));
        let shown = r.max.y >= area.min.y && r.min.y <= area.max.y;
        let mut out = PluginRowLayout { rect: shown.then(|| r4(r)), ..PluginRowLayout::default() };
        match row {
            PRow::Heading(t) => {
                out.row = "heading";
                out.name.clone_from(t);
                if shown {
                    p.text(pos2(r.min.x + 8.0, r.center().y + 2.0), Align2::LEFT_CENTER, t, FontId::proportional(12.0), TOKENS.text);
                }
            }
            PRow::Vendor(t) => {
                out.row = "vendor";
                out.name.clone_from(t);
                if shown {
                    p.text(pos2(r.min.x + 14.0, r.center().y), Align2::LEFT_CENTER, t, FontId::proportional(10.5), TOKENS.text_dim);
                }
            }
            PRow::Note(t) => {
                out.row = "note";
                out.name.clone_from(t);
                if shown {
                    p.text(pos2(r.min.x + 14.0, r.center().y), Align2::LEFT_CENTER, t, FontId::proportional(10.5), TOKENS.text_dim);
                }
            }
            PRow::Item(k) => {
                let it = &items[*k];
                let fav = app.ui.browser.favorites.contains(&it.id);
                let selected = app.browser.plugin_selected.as_deref() == Some(it.id.as_str());
                out = PluginRowLayout {
                    row: "plugin",
                    id: it.id.clone(),
                    name: it.name.clone(),
                    vendor: it.vendor.clone(),
                    format: it.format.to_string(),
                    instrument: it.instrument,
                    favorite: fav,
                    selected,
                    rect: out.rect,
                    star: None,
                };
                if shown {
                    let resp = ui.interact(r.intersect(area), ui.id().with(("browser_plugin", i, &it.id)), Sense::click_and_drag());
                    if selected {
                        p.rect_filled(r, 0.0, TOKENS.accent.gamma_multiply(0.28));
                    } else if resp.hovered() {
                        p.rect_filled(r, 0.0, TOKENS.button);
                    }
                    let sr = Rect::from_min_size(pos2(r.max.x - 24.0, r.min.y + 2.0), vec2(16.0, 16.0));
                    let tag = if it.format == "Built-in" { it.category.clone() } else { it.format.to_string() };
                    p.text(pos2(sr.min.x - 6.0, r.center().y), Align2::RIGHT_CENTER, &tag, FontId::proportional(10.0), TOKENS.text_dim);
                    let name_r = Rect::from_min_max(pos2(r.min.x + 24.0, r.min.y), pos2(sr.min.x - 60.0, r.max.y));
                    p.with_clip_rect(name_r.intersect(area)).text(
                        name_r.left_center(),
                        Align2::LEFT_CENTER,
                        &it.name,
                        FontId::proportional(12.0),
                        TOKENS.text,
                    );
                    let sresp = ui.interact(sr, ui.id().with(("browser_star", i, &it.id)), Sense::click());
                    let ink = if fav {
                        TOKENS.accent
                    } else if sresp.hovered() {
                        TOKENS.text
                    } else {
                        TOKENS.text_dim.gamma_multiply(0.6)
                    };
                    crate::icons::paint(&p, sr.center(), if fav { Icon::StarFilled } else { Icon::Star }, 13.0, ink);
                    if sresp.on_hover_text(if fav { "Remove from Favorites" } else { "Add to Favorites" }).clicked() {
                        star_click = Some(it.id.clone());
                    }
                    out.star = Some(r4(sr));
                    if resp.clicked() {
                        pick = Some((*k, false));
                    }
                    if resp.double_clicked() {
                        pick = Some((*k, true));
                    }
                    if resp.drag_started() {
                        drag_from = Some(*k);
                    }
                }
            }
        }
        layout.plugins.push(out);
    }
    if let Some(id) = star_click {
        let favs = &mut app.ui.browser.favorites;
        if let Some(k) = favs.iter().position(|f| *f == id) {
            favs.remove(k);
        } else {
            favs.push(id);
        }
    } else if let Some((k, double)) = pick {
        let it = &items[k];
        app.browser.plugin_selected = Some(it.id.clone());
        if double {
            let _ = app.plugin_to_focus(&it.id, &it.name, it.instrument);
        }
    }
    if let Some(k) = drag_from {
        let it = &items[k];
        app.browser.plugin_selected = Some(it.id.clone());
        app.browser.drag =
            Some(Drag { payload: Payload::Plugin { id: it.id.clone(), name: it.name.clone(), instrument: it.instrument }, target: None });
    }
}
