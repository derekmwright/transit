//! Crash-recovery copies of open sessions, one folder each, written in the background, and the
//! recovery of the ones left behind with unsaved work.

use crate::io::{Recording, open_session_linked, sanitize_name};
use crate::{Engine, EngineError, Result};
use soundcraft_audio_io::{BitDepth, EncodeOptions, FileFormat};
use soundcraft_model::{Session, SourceAudio, SourceId};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

const SESSION: &str = "session.transit";
const ORIGIN: &str = "origin";
const LOCK: &str = "lock";
/// The plan of a recording still running when the folder was written.
const RECORDING: &str = "recording.json";
/// While changes keep coming, the newest is written at most this often.
const INTERVAL: Duration = Duration::from_secs(1);
const STOPPED: &str = "the autosave thread stopped";

/// Writes autosaves on a thread of its own, so the UI never waits on the disk.
pub struct Autosaver {
    jobs: Option<Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
    /// Writes handed over so far.
    queued: AtomicU64,
    /// The newest write that reached the disk.
    landed: Arc<AtomicU64>,
}

/// A write handed to the autosaver, to ask [`Autosaver::landed`] about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

enum Job {
    Write(Arc<Session>, Option<String>, Option<Recording>, u64),
    /// Done with the session's folder: keep it for the next launch, or delete it.
    Close(bool),
    Flush(Sender<std::result::Result<(), String>>),
}

impl Autosaver {
    /// A writer for autosave folders under `root`.
    pub fn new(root: PathBuf) -> Autosaver {
        let (tx, rx) = channel();
        let failure = Arc::new(Mutex::new(None));
        let landed = Arc::new(AtomicU64::new(0));
        let writer =
            Writer { root, failure: Arc::clone(&failure), landed: Arc::clone(&landed), folder: None, pending: None, last: None, written: Ok(()) };
        let thread = std::thread::Builder::new().name("soundcraft-autosave".into()).spawn(move || writer.run(&rx));
        if let Err(e) = &thread {
            report(&failure, format!("{STOPPED}: {e}"));
        }
        Autosaver { jobs: Some(tx), thread: thread.ok(), failure, queued: AtomicU64::new(0), landed }
    }

    /// Autosave `session`, opened from the session file `origin`, soon, with the plan of the
    /// recording running in it. The ticket says when it is on disk (see [`Autosaver::landed`]).
    pub fn write(&self, session: Arc<Session>, origin: Option<String>, recording: Option<Recording>) -> Ticket {
        let n = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(Job::Write(session, origin, recording, n));
        Ticket(n)
    }

    /// Has the write of `ticket`, or a newer one, reached the disk? Writes handed over while an
    /// older one waits replace it, so a newer write landing covers the older ticket too.
    pub fn landed(&self, ticket: Ticket) -> bool {
        self.landed.load(Ordering::Acquire) >= ticket.0
    }

    /// Done with the open session's folder: keep it for the next launch, or delete it.
    pub fn close(&self, keep: bool) {
        self.send(Job::Close(keep));
    }

    /// Wait until everything handed over so far is done. A failure returned here isn't reported
    /// again by `failure`.
    pub fn flush(&self) -> Result<()> {
        let (tx, rx) = channel();
        self.send(Job::Flush(tx));
        let done = rx.recv().unwrap_or_else(|_| Err(STOPPED.into()));
        if done.is_err() {
            self.failure();
        }
        done.map_err(EngineError::Io)
    }

    /// The newest failure since the last call.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn send(&self, job: Job) {
        if self.jobs.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
            report(&self.failure, STOPPED.into());
        }
    }
}

impl Drop for Autosaver {
    /// What was handed over is written, and an open folder stays for the next launch.
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The writer thread.
struct Writer {
    root: PathBuf,
    failure: Arc<Mutex<Option<String>>>,
    landed: Arc<AtomicU64>,
    /// The open session's folder.
    folder: Option<Autosave>,
    /// The newest session not written yet.
    pending: Option<(Arc<Session>, Option<String>, Option<Recording>, u64)>,
    last: Option<Instant>,
    /// How the newest write into the open folder went, for `Flush`.
    written: std::result::Result<(), String>,
}

impl Writer {
    fn run(mut self, jobs: &Receiver<Job>) {
        loop {
            let wait = match (&self.pending, self.last) {
                (None, _) => Duration::MAX,
                (Some(_), last) => last.map_or(Duration::ZERO, |t| INTERVAL.saturating_sub(t.elapsed())),
            };
            match jobs.recv_timeout(wait) {
                Ok(Job::Write(session, origin, recording, n)) => self.pending = Some((session, origin, recording, n)),
                Ok(Job::Flush(reply)) => {
                    self.write_pending();
                    let _ = reply.send(self.written.clone());
                }
                Ok(Job::Close(keep)) => {
                    if keep {
                        self.write_pending();
                    }
                    self.pending = None;
                    self.written = Ok(());
                    if let Some(a) = self.folder.take()
                        && !keep
                    {
                        a.discard();
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.write_pending(),
                Err(RecvTimeoutError::Disconnected) => {
                    self.write_pending();
                    return;
                }
            }
        }
    }

    fn write_pending(&mut self) {
        let Some((session, origin, recording, ticket)) = self.pending.take() else { return };
        self.last = Some(Instant::now());
        let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            if self.folder.is_none() {
                self.folder = Some(Autosave::create(&self.root, &session.name)?);
            }
            self.folder.as_mut().map_or(Ok(()), |a| a.write(&session, origin.as_deref(), recording.as_ref()))
        }));
        self.written = match written {
            Ok(written) => written.map_err(|e| e.to_string()),
            Err(_) => Err("internal error (see the log)".into()),
        };
        match &self.written {
            Ok(()) => self.landed.store(ticket, Ordering::Release),
            Err(e) => report(&self.failure, e.clone()),
        }
    }
}

fn report(failure: &Mutex<Option<String>>, e: String) {
    log::warn!("autosave failed: {e}");
    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
}

/// An open session's autosave folder, locked while the session is open so a later launch can tell
/// it from one a crash left behind.
struct Autosave {
    dir: PathBuf,
    lock: Option<std::fs::File>,
    copies: BTreeMap<SourceId, (Weak<SourceAudio>, String)>,
    next: u64,
    /// A recording's plan is in the folder.
    recording: bool,
}

impl Autosave {
    /// A new folder under `root` for the session called `name`.
    fn create(root: &Path, name: &str) -> Result<Autosave> {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis();
        let base = format!("{} {stamp}-{}", sanitize_name(name), std::process::id());
        std::fs::create_dir_all(root).map_err(|err| io_error(root, err))?;
        // A folder is this session's alone: another created in the same millisecond (a session
        // closed and reopened at once, or two in one process) gets a number, never the same folder.
        let mut n = 1;
        let dir = loop {
            let dir = root.join(if n == 1 { base.clone() } else { format!("{base}-{n}") });
            match std::fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(err) => return Err(io_error(&dir, err)),
            }
        };
        let lock = std::fs::File::create(dir.join(LOCK)).map_err(|err| io_error(&dir, err))?;
        lock.try_lock().map_err(|err| EngineError::Io(format!("{}: {err}", dir.display())))?;
        Ok(Autosave { dir, lock: Some(lock), copies: BTreeMap::new(), next: 1, recording: false })
    }

    /// Write `session`, opened from the session file `origin`, and the plan of the `recording`
    /// running in it. Audio saved with it is referenced where it is; audio that exists only in
    /// memory is copied, once per recording or render.
    fn write(&mut self, session: &Session, origin: Option<&str>, recording: Option<&Recording>) -> Result<()> {
        let base = origin.and_then(|p| Path::new(p).parent()).and_then(|p| std::path::absolute(p).ok());
        let audio_dir = self.dir.join("Audio Files");
        let mut s = session.clone();
        let mut copies = BTreeMap::new();
        for src in &mut s.sources {
            let file = if Path::new(&src.path).is_absolute() { Some(PathBuf::from(&src.path)) } else { base.as_ref().map(|b| b.join(&src.path)) };
            let audio = s.pool.get(src.id);
            if let Some(f) = &file
                && (audio.is_none() || !src.unsaved && f.is_file())
            {
                src.path = f.to_string_lossy().into_owned();
                continue;
            }
            let Some(audio) = audio else { continue };
            let name = match self.copies.remove(&src.id) {
                Some((copy, name)) if copy.upgrade().is_some_and(|a| Arc::ptr_eq(&a, audio)) => name,
                _ => {
                    let name = format!("{} {}.wav", sanitize_name(&src.name), self.next);
                    self.next += 1;
                    let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
                    let bytes = soundcraft_audio_io::encode(&audio.buffer, &opts).map_err(|err| EngineError::Io(err.to_string()))?;
                    std::fs::create_dir_all(&audio_dir)
                        .and_then(|()| write_synced(&audio_dir.join(&name), &bytes))
                        .map_err(|err| io_error(&audio_dir, err))?;
                    name
                }
            };
            src.path = audio_dir.join(&name).to_string_lossy().into_owned();
            src.unsaved = true;
            copies.insert(src.id, (Arc::downgrade(audio), name));
        }
        let text = s.to_json().map_err(|err| EngineError::Io(err.to_string()))?;
        write_atomically(&self.dir.join(ORIGIN), origin.unwrap_or_default().as_bytes())?;
        write_atomically(&self.dir.join(SESSION), text.as_bytes())?;
        for (_, name) in std::mem::replace(&mut self.copies, copies).into_values() {
            let _ = std::fs::remove_file(audio_dir.join(name));
        }
        if let Some(r) = recording {
            let plan = serde_json::to_string(r).map_err(|err| EngineError::Io(err.to_string()))?;
            write_atomically(&self.dir.join(RECORDING), plan.as_bytes())?;
        } else if self.recording {
            let _ = std::fs::remove_file(self.dir.join(RECORDING));
        }
        self.recording = recording.is_some();
        Ok(())
    }

    /// Delete the folder: the session closed with nothing left to recover.
    fn discard(mut self) {
        drop(self.lock.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Unsaved work left behind in an autosave folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Leftover {
    pub dir: PathBuf,
    /// The session's name.
    pub name: String,
    /// The session file it was autosaved from, if it had one.
    pub origin: Option<String>,
    pub saved: SystemTime,
    /// A recording was still running in it.
    pub recording: bool,
    /// The MIDI take journal of that recording, if it left one.
    pub midi_journal: Option<PathBuf>,
    /// Stop had already placed that recording's MIDI clips in the autosaved session.
    pub midi_placed: bool,
}

/// Leftover autosaves under `root`, newest first: folders no open session holds, written after the
/// session file they came from or holding a recording. Empty and outdated ones are deleted.
pub fn leftovers(root: &Path) -> Vec<Leftover> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten().map(|d| d.path()) {
        let Ok(lock) = std::fs::OpenOptions::new().write(true).open(dir.join(LOCK)) else { continue };
        if lock.try_lock().is_err() {
            continue;
        }
        let origin = std::fs::read_to_string(dir.join(ORIGIN)).ok().filter(|o| !o.is_empty());
        let origin_saved = origin.as_ref().and_then(|o| std::fs::metadata(o).and_then(|m| m.modified()).ok());
        let session_file = session_in(&dir);
        let saved = std::fs::metadata(&session_file).and_then(|m| m.modified());
        let recording = dir.join(RECORDING).is_file();
        drop(lock);
        match saved {
            Ok(saved) if recording || origin_saved.is_none_or(|o| saved > o) => {
                let text = std::fs::read_to_string(&session_file).unwrap_or_default();
                let named = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("name")?.as_str().map(str::to_string));
                let name = named.unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
                let plan = recording.then(|| plan_of(&dir)).flatten();
                let midi_placed = plan.as_ref().is_some_and(|p| p.midi_placed);
                let midi_journal = plan.and_then(|p| p.midi_journal).filter(|j| j.is_file());
                out.push(Leftover { dir, name, origin, saved, recording, midi_journal, midi_placed });
            }
            _ => {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
    out.sort_by_key(|l| std::cmp::Reverse(l.saved));
    out
}

/// The session file of an autosave folder.
fn session_in(dir: &Path) -> PathBuf {
    dir.join(SESSION)
}

/// Open `left` as an unsaved version of the session it came from, so Save writes it back there,
/// with the takes of a recording that never finished placed as Stop would have. Returns the
/// media that couldn't be loaded.
pub fn recover(e: &mut Engine, left: &Leftover) -> Result<Vec<String>> {
    let mut r = Engine::default();
    let session = session_in(&left.dir);
    let links = left.origin.as_deref().map_or(session.as_path(), Path::new);
    let mut missing = open_session_linked(&mut r, &session.to_string_lossy(), links)?;
    let mut s = r.session().clone();
    let unfinished = unfinished_takes(left, &s, &mut missing);
    if let Some(home) = left.origin.as_deref().and_then(|o| Path::new(o).parent()) {
        for src in s.sources.iter_mut().filter(|src| !src.unsaved) {
            if let Ok(rel) = Path::new(&src.path).strip_prefix(home) {
                src.path = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            }
        }
    }
    e.replace_session(Session::default());
    *e.session_mut() = s;
    e.path = left.origin.clone();
    if let Some((plan, takes)) = unfinished {
        crate::io::place_recording(e, &plan, takes);
    }
    Ok(missing)
}

/// The takes of the recording `left` holds the plan of, repaired, minus those already in `s`.
/// A take the plan names that isn't a take file transit recorded, in a folder it records into,
/// is left untouched and added to `skipped`.
fn unfinished_takes(left: &Leftover, s: &Session, skipped: &mut Vec<String>) -> Option<(Recording, Vec<(soundcraft_model::TrackId, PathBuf)>)> {
    let plan: Recording = serde_json::from_str(&std::fs::read_to_string(left.dir.join(RECORDING)).ok()?).ok()?;
    let known: Vec<PathBuf> =
        s.sources.iter().filter(|src| s.pool.get(src.id).is_some()).filter_map(|src| std::fs::canonicalize(&src.path).ok()).collect();
    let folders = take_folders(left);
    let mut takes = Vec::new();
    for t in &plan.takes {
        if std::fs::canonicalize(&t.path).is_ok_and(|p| known.contains(&p)) {
            continue;
        }
        match repair_take(&t.path, &folders, t.channels, plan.rate) {
            Ok(frames) if frames > 0 => takes.push((t.track, t.path.clone())),
            Ok(_) => {}
            Err(err) => {
                log::warn!("recovery skipped the take {}: {err}", t.path.display());
                skipped.push(format!("{}: {err}", t.path.display()));
            }
        }
    }
    Some((plan, takes))
}

/// The folders a recording in `left`'s session writes its takes into: the `Audio Files` next to
/// the session file, or, never saved, the one in its folder under the autosave root.
fn take_folders(left: &Leftover) -> Vec<PathBuf> {
    let saved = left.origin.as_deref().and_then(|o| Path::new(o).parent()).map(|d| d.join("Audio Files"));
    let unsaved = left.dir.parent().map(|root| root.join(sanitize_name(&left.name)).join("Audio Files"));
    saved.into_iter().chain(unsaved).filter_map(|d| std::fs::canonicalize(d).ok()).collect()
}

/// Make a take file's header cover all the audio in it; returns its frames. The recorder
/// rewrites the header only every second, so a crash leaves up to a second uncovered. Only a
/// regular file in one of `folders` that starts with the header the recorder writes is touched.
fn repair_take(path: &Path, folders: &[PathBuf], channels: usize, rate: u32) -> std::io::Result<u64> {
    let refuse = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string());
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(refuse("not a regular file"));
    }
    let folder = path.parent().and_then(|d| std::fs::canonicalize(d).ok());
    if !folder.is_some_and(|d| folders.contains(&d)) {
        return Err(refuse("not in a folder transit records into"));
    }
    let header = |frames| soundcraft_audio_io::wav_float_header(channels, rate, frames).map_err(std::io::Error::other);
    let expected = header(0)?;
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut head = vec![0u8; expected.len()];
    if std::io::Read::read_exact(&mut f, &mut head).is_err() || !is_take_header(&head, &expected) {
        return Err(refuse("not a take transit recorded"));
    }
    let audio = f.metadata()?.len().saturating_sub(expected.len() as u64);
    let frames = audio / (channels.max(1) as u64 * 4);
    std::io::Seek::rewind(&mut f)?;
    f.write_all(&header(frames)?)?;
    f.sync_all()?;
    Ok(frames)
}

/// `head` is a take header like `expected` (the recorder's header for no frames yet): RIFF or
/// RF64 WAVE, the same format chunk at the same place, and the data chunk right after it all.
/// Only the sizes and frame counts may differ.
fn is_take_header(head: &[u8], expected: &[u8]) -> bool {
    let magic = matches!(head.get(..4), Some(b"RIFF" | b"RF64")) && head.get(8..12) == Some(b"WAVE");
    let fmt = expected.windows(4).position(|w| w == b"fmt ");
    let fmt_len = fmt.and_then(|at| expected.get(at + 4..at + 8)).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes);
    let same_fmt = match (fmt, fmt_len) {
        (Some(at), Some(n)) => {
            let end = at.saturating_add(8).saturating_add(n as usize);
            expected.get(at..end).is_some_and(|chunk| head.get(at..end) == Some(chunk))
        }
        _ => false,
    };
    let data = expected.len().checked_sub(8).and_then(|at| head.get(at..at + 4)) == Some(b"data");
    magic && same_fmt && data
}

/// The recording plan in `dir`.
fn plan_of(dir: &Path) -> Option<Recording> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(RECORDING)).ok()?).ok()
}

/// `path`, which `left`'s plan names as its MIDI journal, is one transit wrote: a regular file
/// with the journal extension in a folder it records into (as [`repair_take`] checks a take).
/// The plan is only a file in a folder, so nothing it names is trusted further than that.
fn trusted_journal(left: &Leftover, path: &Path) -> std::io::Result<()> {
    let refuse = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {why}", path.display()));
    if path.extension().and_then(|e| e.to_str()) != Some(crate::midi_journal::EXTENSION) {
        return Err(refuse("not a MIDI journal file name"));
    }
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(refuse("not a regular file"));
    }
    let folder = path.parent().and_then(|d| std::fs::canonicalize(d).ok());
    if !folder.is_some_and(|d| take_folders(left).contains(&d)) {
        return Err(refuse("not in a folder transit records into"));
    }
    Ok(())
}

/// Is `left`'s MIDI journal still there and one [`recover_midi`] may use? After a failed
/// recovery this says the failure may pass (a locked file) and the leftover is worth keeping.
pub fn midi_journal_usable(left: &Leftover) -> bool {
    left.midi_journal.as_deref().is_some_and(|j| trusted_journal(left, j).is_ok())
}

/// Rebuild the MIDI clips of the recording `left` was running, from its journal, into `e` (call
/// after [`recover`]). Returns the clips made; none when it left no journal, nothing was played,
/// or the autosave already holds the clips (the journal is then deleted). A journal that cannot
/// be read is an error and is deleted with it, so recovery doesn't offer it again; one that isn't
/// a journal in a recording folder is an error and left alone.
pub fn recover_midi(e: &mut Engine, left: &Leftover) -> Result<Vec<soundcraft_model::ClipId>> {
    let Some(journal) = left.midi_journal.as_deref() else { return Ok(Vec::new()) };
    trusted_journal(left, journal).map_err(|err| EngineError::Io(format!("the MIDI journal was not used: {err}")))?;
    if left.midi_placed {
        let _ = std::fs::remove_file(journal);
        return Ok(Vec::new());
    }
    std::fs::File::open(journal).map_err(|err| io_error(journal, err))?;
    crate::midi_journal::recover(e, journal)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(journal);
        })
        .map_err(|err| EngineError::Io(format!("{err} (the journal was discarded)")))
}

/// Delete a leftover autosave, and the MIDI journal it names if it is one transit wrote.
pub fn discard(left: &Leftover) {
    if let Some(journal) = &left.midi_journal {
        match trusted_journal(left, journal) {
            Ok(()) => {
                let _ = std::fs::remove_file(journal);
            }
            Err(err) => log::warn!("autosave: left the MIDI journal alone: {err}"),
        }
    }
    let _ = std::fs::remove_dir_all(&left.dir);
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    write_synced(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        io_error(path, err)
    })
}

fn io_error(path: &Path, err: std::io::Error) -> EngineError {
    EngineError::Io(format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::open_session;
    use serde_json::json;
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::{ChannelFormat, Clip, TrackKind};

    #[test]
    fn folders_made_at_the_same_moment_are_never_shared() {
        let root = folder("autosave-unique");
        let all: Vec<Autosave> = (0..40).map(|_| Autosave::create(&root, "Untitled").unwrap()).collect();
        let dirs: std::collections::BTreeSet<_> = all.iter().map(|a| a.dir.clone()).collect();
        assert_eq!(dirs.len(), all.len());
        drop(all);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("soundcraft-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn record(e: &mut Engine, level: f32) {
        let before = e.session_arc();
        let s = e.session_mut();
        let t = match s.tracks.first() {
            Some(t) => t.id,
            None => s.add_track(TrackKind::Audio, ChannelFormat::Mono, None),
        };
        let src =
            crate::io::add_source(s, "Audio 1_01", AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; 4_800]] }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(s, t, Clip::audio(id, "Audio 1_01", src, 0, 0, 4_800));
        e.push_undo("Record", before);
    }

    fn levels(e: &Engine) -> Vec<f32> {
        let s = e.session();
        s.sources.iter().map(|src| s.pool.get(src.id).map_or(f32::NAN, |a| a.buffer.channels[0][0])).collect()
    }

    #[test]
    fn saved_audio_is_referenced_and_takes_are_copied_once() {
        let dir = folder("autosave-copies");
        let path = dir.join("Song/Song.transit");
        let mut e = Engine::default();
        record(&mut e, 0.5);
        crate::io::save_session(&mut e, &path.to_string_lossy()).unwrap();
        record(&mut e, -0.25);
        let mut a = Autosave::create(&dir.join("Autosave"), "Song").unwrap();
        a.write(e.session(), e.path.as_deref(), None).unwrap();
        a.write(e.session(), e.path.as_deref(), None).unwrap();
        assert_eq!(std::fs::read_dir(a.dir.join("Audio Files")).unwrap().count(), 1);
        drop(a);
        let left = leftovers(&dir.join("Autosave"));
        let mut r = Engine::default();
        assert_eq!(recover(&mut r, &left[0]).unwrap(), Vec::<String>::new());
        let paths: Vec<_> = r.session().sources.iter().map(|s| (s.path.starts_with("Audio Files/"), s.unsaved)).collect();
        assert_eq!(paths, [(true, false), (false, true)]);
        assert_eq!(levels(&r), [0.5, -0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_recorded_after_undo_gets_its_own_copy() {
        let dir = folder("autosave-undo");
        let mut e = Engine::default();
        record(&mut e, 0.5);
        let mut a = Autosave::create(&dir, "Untitled").unwrap();
        a.write(e.session(), None, None).unwrap();
        e.execute("edit.undo", &json!({})).unwrap();
        record(&mut e, -0.25);
        a.write(e.session(), None, None).unwrap();
        drop(a);
        let mut r = Engine::default();
        recover(&mut r, &leftovers(&dir)[0]).unwrap();
        assert_eq!(levels(&r), [-0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_writer_saves_the_newest_change_and_drops_the_folder_when_done() {
        let root = folder("autosave-writer");
        let w = Autosaver::new(root.clone());
        let mut e = Engine::default();
        for _ in 0..5 {
            record(&mut e, 0.5);
            w.write(e.session_arc(), None, None);
        }
        w.flush().unwrap();
        let file = std::fs::read_dir(&root).unwrap().flatten().map(|d| d.path().join(SESSION)).collect::<Vec<_>>();
        let mut r = Engine::default();
        open_session(&mut r, &file[0].to_string_lossy()).unwrap();
        assert_eq!((file.len(), r.session().sources.len()), (1, 5));
        w.close(false);
        w.flush().unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        record(&mut e, 0.5);
        w.write(e.session_arc(), None, None);
        drop(w);
        let mut r = Engine::default();
        recover(&mut r, &leftovers(&root)[0]).unwrap();
        assert_eq!(r.session().sources.len(), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_take_still_recording_at_a_crash_is_placed_on_recovery() {
        use crate::io::Take;
        let dir = folder("autosave-take");
        let root = dir.join("Autosave");
        let mut e = Engine::default();
        let track =
            soundcraft_model::TrackId(e.execute("track.new", &json!({"name": "Vox", "format": "mono"})).unwrap()["tracks"][0].as_u64().unwrap());
        let take = root.join(sanitize_name(&e.session().name)).join("Audio Files/Vox_01.wav");
        std::fs::create_dir_all(take.parent().unwrap()).unwrap();
        let audio: Vec<u8> = (0..480).flat_map(|i| (i as f32 / 1000.0).to_le_bytes()).collect();
        std::fs::write(&take, [soundcraft_audio_io::wav_float_header(1, 48_000, 100).unwrap(), audio].concat()).unwrap();
        let plan = Recording {
            start: 48_000,
            pass: None,
            punch: None,
            rate: 48_000,
            takes: vec![Take { track, path: take, channels: 1 }],
            midi_journal: None,
            midi_placed: false,
        };
        let clips = |e: &Engine| e.session().track(track).unwrap().clips().iter().map(|c| (c.start, c.length)).collect::<Vec<_>>();
        let w = Autosaver::new(root.clone());
        w.write(e.session_arc(), None, Some(plan.clone()));
        drop(w);
        let left = leftovers(&root);
        assert!(left[0].recording);
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert_eq!(clips(&r), [(48_000, 480)], "all of the take, its last second included");
        discard(&left[0]);
        let w = Autosaver::new(root.clone());
        w.write(r.session_arc(), None, Some(plan));
        drop(w);
        let mut again = Engine::default();
        recover(&mut again, &leftovers(&root)[0]).unwrap();
        assert_eq!((clips(&again), again.session().sources.len()), (vec![(48_000, 480)], 1), "placed once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovery_never_writes_a_file_that_isnt_a_take_in_a_take_folder() {
        use crate::io::Take;
        let dir = folder("autosave-hostile-take");
        let root = dir.join("Autosave");
        let mut e = Engine::default();
        let track =
            soundcraft_model::TrackId(e.execute("track.new", &json!({"name": "Vox", "format": "mono"})).unwrap()["tracks"][0].as_u64().unwrap());
        let takes = root.join(sanitize_name(&e.session().name)).join("Audio Files");
        std::fs::create_dir_all(&takes).unwrap();
        let wav = [soundcraft_audio_io::wav_float_header(1, 48_000, 0).unwrap(), vec![0; 4_000]].concat();
        let text = b"not audio at all, but long enough to be mistaken for a header if nobody looked at it ......".repeat(4);
        let files =
            [(takes.join("notes.wav"), text.clone()), (dir.join("Elsewhere/Vox_01.wav"), wav.clone()), (takes.join("../Vox_02.wav"), wav.clone())];
        for (path, bytes) in &files {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        // Only the symlink case below adds to it, and symlinks are a Unix-only part of the test.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
        #[cfg(unix)]
        {
            let link = takes.join("Vox_03.wav");
            std::os::unix::fs::symlink(&files[1].0, &link).unwrap();
            paths.push(link);
        }
        let plan = Recording {
            start: 0,
            pass: None,
            punch: None,
            rate: 48_000,
            takes: paths.iter().map(|p| Take { track, path: p.clone(), channels: 1 }).collect(),
            midi_journal: None,
            midi_placed: false,
        };
        let w = Autosaver::new(root.clone());
        w.write(e.session_arc(), None, Some(plan));
        drop(w);
        let mut r = Engine::default();
        let skipped = recover(&mut r, &leftovers(&root)[0]).unwrap();
        assert_eq!(skipped.len(), paths.len(), "{skipped:?}");
        assert!(r.session().sources.is_empty());
        for (path, bytes) in &files {
            assert_eq!(&std::fs::read(path).unwrap(), bytes, "{} untouched", path.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A leftover whose plan names `journal`, for an unsaved session under `root`.
    fn left_with_journal(root: &Path, journal: &Path, placed: bool) -> Leftover {
        let e = Engine::default();
        let plan = Recording {
            start: 0,
            pass: None,
            punch: None,
            rate: 48_000,
            takes: Vec::new(),
            midi_journal: Some(journal.to_path_buf()),
            midi_placed: placed,
        };
        let w = Autosaver::new(root.to_path_buf());
        w.write(e.session_arc(), None, Some(plan));
        drop(w);
        let mut left = leftovers(root);
        assert_eq!(left.len(), 1);
        left.remove(0)
    }

    fn takes_folder(root: &Path) -> PathBuf {
        let dir = root.join(sanitize_name(&Engine::default().session().name)).join("Audio Files");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn journal_with_a_note(path: &Path, track: soundcraft_model::TrackId) {
        use crate::midi_journal::{Header, Latch, Record, Writer};
        let plan = Recording { start: 0, pass: None, punch: None, rate: 0, takes: Vec::new(), midi_journal: None, midi_placed: false };
        let latch = Latch { output_frame: 0, position: 0, device_rate: 48_000, session_rate: 48_000 };
        let mut w = Writer::new(path.to_path_buf());
        let on = Record { frame: 100, track, channel: 0, approximate: false, msg: crate::midi_take::Msg::NoteOn { pitch: 60, velocity: 90 } };
        let off = Record { frame: 4_900, msg: crate::midi_take::Msg::NoteOff { pitch: 60, velocity: 0 }, ..on };
        w.append(&Header::new(latch, &plan, None, vec![track]), &[on, off], 2).unwrap();
        w.flush().unwrap();
    }

    #[test]
    fn a_journal_the_autosave_already_has_the_clips_of_is_discarded_without_rebuilding() {
        let dir = folder("autosave-journal-placed");
        let root = dir.join("Autosave");
        let journal = takes_folder(&root).join("Song_1.midijournal");
        journal_with_a_note(&journal, soundcraft_model::TrackId(1));
        let left = left_with_journal(&root, &journal, true);
        assert!(left.midi_placed && left.midi_journal.as_deref() == Some(journal.as_path()));
        let mut e = Engine::default();
        e.execute("track.new", &json!({"kind": "midi"})).unwrap();
        assert_eq!(recover_midi(&mut e, &left).unwrap(), Vec::new());
        assert!(e.session().tracks[0].clips().is_empty(), "nothing rebuilt");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_journal_for_a_clip_the_autosave_lacks_is_rebuilt() {
        let dir = folder("autosave-journal-unplaced");
        let root = dir.join("Autosave");
        let journal = takes_folder(&root).join("Song_1.midijournal");
        let mut e = Engine::default();
        let track = soundcraft_model::TrackId(e.execute("track.new", &json!({"kind": "midi"})).unwrap()["tracks"][0].as_u64().unwrap());
        journal_with_a_note(&journal, track);
        let left = left_with_journal(&root, &journal, false);
        assert_eq!(recover_midi(&mut e, &left).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_can_only_name_a_journal_in_a_recording_folder() {
        let dir = folder("autosave-journal-trust");
        let root = dir.join("Autosave");
        let inside = takes_folder(&root);
        let victims = [
            dir.join("Elsewhere/Song_1.midijournal"),
            inside.join("notes.txt"),
            inside.join("../Song_2.midijournal"),
            inside.join("sub/Song_3.midijournal"),
        ];
        for v in &victims {
            std::fs::create_dir_all(v.parent().unwrap()).unwrap();
            std::fs::write(v, b"precious").unwrap();
        }
        for v in &victims {
            let left = left_with_journal(&root, v, false);
            let mut e = Engine::default();
            let err = recover_midi(&mut e, &left).unwrap_err().to_string();
            assert!(err.contains("not used"), "{}: {err}", v.display());
            discard(&left);
            assert_eq!(std::fs::read(v).unwrap(), b"precious", "{} untouched", v.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_journal_with_a_damaged_header_is_reported_and_discarded_instead_of_offered_again() {
        let dir = folder("autosave-journal-corrupt");
        let root = dir.join("Autosave");
        let journal = takes_folder(&root).join("Song_1.midijournal");
        std::fs::write(&journal, b"{\"version\":1,\"output_fr").unwrap();
        let left = left_with_journal(&root, &journal, false);
        let mut e = Engine::default();
        let err = recover_midi(&mut e, &left).unwrap_err().to_string();
        assert!(err.contains("not a MIDI journal") && err.contains("discarded"), "{err}");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsaved_work_is_offered_once_its_session_closes() {
        let dir = folder("autosave-leftovers");
        let path = dir.join("Song.transit").to_string_lossy().into_owned();
        let root = dir.join("Autosave");
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = Engine::default();
        crate::io::save_session(&mut e, &path).unwrap();
        record(&mut e, 0.5);
        let mut a = Autosave::create(&root, "Song").unwrap();
        a.write(e.session(), e.path.as_deref(), None).unwrap();
        assert!(leftovers(&root).is_empty());
        drop(a);
        let left = leftovers(&root);
        assert_eq!((left.len(), left[0].name.as_str(), left[0].origin.as_deref()), (1, "Song", Some(path.as_str())));
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert!(r.is_dirty());
        r.execute("session.save", &json!({})).unwrap();
        assert!(leftovers(&root).is_empty());
        assert!(!left[0].dir.exists());
        let mut reopened = Engine::default();
        open_session(&mut reopened, &path).unwrap();
        assert_eq!(levels(&reopened), [0.5]);
        Autosave::create(&root, "Song").unwrap().discard();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
