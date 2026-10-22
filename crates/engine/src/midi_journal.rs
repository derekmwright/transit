//! The MIDI take journal: the events of a MIDI recording pass, appended to a file while the pass
//! runs, so a crash does not lose them (the audio take is a file for the same reason).
//!
//! Format: newline-delimited JSON. The first line is the [`Header`] (the record-start latch and
//! what is needed to rebuild the clip); every other line is one [`Record`]. A record cut off by a
//! crash fails to parse and ends the journal; everything before it is kept.
//!
//! The writer runs on the control thread (it is fed from the drained capture queue); the audio
//! thread never touches it.

use crate::io::{Recording, sanitize_name};
use crate::midi_take::{Msg, RecEvent};
use crate::{Engine, EngineError, Result};
use soundcraft_model::{ClipId, TrackId};
use soundcraft_time::{Range, Samples};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The file extension of a journal.
pub const EXTENSION: &str = "midijournal";
/// The journal is flushed to the file at least this often while events arrive.
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);
const VERSION: u32 = 1;

/// The latch and the plan of the pass, written once at the top of the journal.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Header {
    pub version: u32,
    /// The output-device frame of the record-start latch.
    pub output_frame: u64,
    /// The session position of the latch, in samples.
    pub position: Samples,
    pub device_rate: u32,
    pub session_rate: u32,
    /// Where the take starts on the timeline (the latch position).
    pub plan_start: Samples,
    /// The loop length when loop recording.
    pub pass: Option<Samples>,
    pub punch: Option<Range>,
    pub loop_range: Option<Range>,
    /// The armed tracks of the pass.
    pub tracks: Vec<TrackId>,
}

/// The device and session side of a [`Header`].
#[derive(Debug, Clone, Copy)]
pub struct Latch {
    pub output_frame: u64,
    pub position: Samples,
    pub device_rate: u32,
    pub session_rate: u32,
}

impl Header {
    /// A header for the current format.
    pub fn new(latch: Latch, plan: &Recording, loop_range: Option<Range>, tracks: Vec<TrackId>) -> Header {
        Header {
            version: VERSION,
            output_frame: latch.output_frame,
            position: latch.position,
            device_rate: latch.device_rate,
            session_rate: latch.session_rate,
            plan_start: plan.start,
            pass: plan.pass,
            punch: plan.punch,
            loop_range,
            tracks,
        }
    }
}

/// One captured event, as the audio thread placed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// The output-device frame.
    pub frame: u64,
    pub track: TrackId,
    pub channel: u8,
    pub approximate: bool,
    pub msg: Msg,
}

/// Where a journal for the session called `name` goes: next to the audio takes, in `dir`
/// (`<dir>/<name>_<timestamp>.midijournal`).
pub fn journal_path(dir: &Path, name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let base = sanitize_name(name);
    let mut path = dir.join(format!("{base}_{stamp}.{EXTENSION}"));
    for n in 2..1_000 {
        if !path.exists() {
            break;
        }
        path = dir.join(format!("{base}_{stamp}-{n}.{EXTENSION}"));
    }
    path
}

/// Appends a pass's events to its journal file. The file is created with the first event.
pub struct Writer {
    path: PathBuf,
    file: Option<BufWriter<std::fs::File>>,
    /// How many of the pass's captured events are accounted for (written or skipped).
    consumed: usize,
    flushed: Instant,
    failed: bool,
}

impl Writer {
    pub fn new(path: PathBuf) -> Writer {
        Writer { path, file: None, consumed: 0, flushed: Instant::now(), failed: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many of the pass's captured events (oldest first) the journal has taken: the rest are
    /// for [`Writer::append`].
    pub fn consumed(&self) -> usize {
        self.consumed
    }

    /// Append `records` (from the pass's events up to `consumed`), creating the file with
    /// `header` first. Flushes when [`FLUSH_EVERY`] has passed. A failure is returned once; later
    /// calls do nothing, so a full disk does not stop the recording.
    pub fn append(&mut self, header: &Header, records: &[Record], consumed: usize) -> std::io::Result<()> {
        self.consumed = self.consumed.max(consumed);
        if self.failed || (self.file.is_none() && records.is_empty()) {
            return Ok(());
        }
        let written = self.write(header, records).and_then(|()| if self.flushed.elapsed() >= FLUSH_EVERY { self.flush() } else { Ok(()) });
        if written.is_err() {
            self.failed = true;
        }
        written
    }

    fn write(&mut self, header: &Header, records: &[Record]) -> std::io::Result<()> {
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = BufWriter::new(std::fs::File::create(&self.path)?);
            line(&mut file, header)?;
            self.file = Some(file);
        }
        let Some(file) = self.file.as_mut() else { return Ok(()) };
        records.iter().try_for_each(|r| line(file, r))
    }

    /// Hand everything written so far to the file.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.flushed = Instant::now();
        self.file.as_mut().map_or(Ok(()), Write::flush)
    }

    /// The pass ended cleanly: delete the journal.
    pub fn discard(mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

fn line<T: serde::Serialize>(file: &mut impl Write, value: &T) -> std::io::Result<()> {
    serde_json::to_writer(&mut *file, value).map_err(std::io::Error::other)?;
    file.write_all(b"\n")
}

/// A journal as read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Journal {
    pub header: Header,
    pub records: Vec<Record>,
    /// The last record was cut off (a crash mid-write); it and anything after it is ignored.
    pub truncated: bool,
}

/// Read the journal at `path`. An unreadable header is an error; a damaged record ends the list.
pub fn read(path: &Path) -> Result<Journal> {
    let io = |e: std::io::Error| EngineError::Io(format!("{}: {e}", path.display()));
    let mut lines = BufReader::new(std::fs::File::open(path).map_err(io)?).split(b'\n');
    let first = lines.next().transpose().map_err(io)?.unwrap_or_default();
    let header: Header = serde_json::from_slice(&first).map_err(|e| EngineError::Io(format!("{}: not a MIDI journal: {e}", path.display())))?;
    if header.version != VERSION {
        return Err(EngineError::Io(format!("{}: unknown MIDI journal version {}", path.display(), header.version)));
    }
    let mut records = Vec::new();
    let mut truncated = false;
    for l in lines {
        match l.ok().and_then(|l| serde_json::from_slice::<Record>(&l).ok()) {
            Some(r) => records.push(r),
            None => {
                truncated = true;
                break;
            }
        }
    }
    Ok(Journal { header, records, truncated })
}

impl Journal {
    /// The events of the pass's tracks at or after the latch, each with the session samples since
    /// the latch (device frames converted at the session rate, as the live take is).
    pub fn events(&self) -> Vec<RecEvent> {
        let h = &self.header;
        let rate = u128::from(h.device_rate.max(1));
        self.records
            .iter()
            .filter(|r| h.tracks.contains(&r.track))
            .filter_map(|r| {
                let frames = r.frame.checked_sub(h.output_frame)?;
                let samples = (u128::from(frames) * u128::from(h.session_rate) + rate / 2) / rate;
                Some(RecEvent { track: r.track, channel: r.channel, offset: i64::try_from(samples).ok()?, msg: r.msg })
            })
            .collect()
    }
}

/// Rebuild the clips of the journal at `path` into `e` through the normal take path, as one
/// "Record" undo step. The new clips are named `<Track>_recovered`; notes merged into a clip that
/// was already there keep its name. Returns the clips touched; none when nothing was played.
pub fn recover(e: &mut Engine, path: &Path) -> Result<Vec<ClipId>> {
    let journal = read(path)?;
    let events = journal.events();
    let h = &journal.header;
    // Held notes end a little after the last event (the journal doesn't record when Stop came).
    let stop = events.iter().map(|ev| ev.offset).max().unwrap_or(0).saturating_add(i64::from(h.session_rate) / 4);
    let plan = Recording { start: h.plan_start, pass: h.pass, punch: h.punch, rate: 0, takes: Vec::new(), midi_journal: None };
    let before = e.session_arc();
    let ids = crate::midi_take::place_midi_recording(e, &plan, &events, stop);
    let mut named: Vec<(TrackId, usize)> = Vec::new();
    for id in &ids {
        if before.find_clip(*id).is_some() {
            continue;
        }
        let Some(track) = e.session().find_clip(*id).map(|(t, _)| t) else { continue };
        let n = match named.iter_mut().find(|(t, _)| *t == track) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                named.push((track, 1));
                1
            }
        };
        let base = e.session().track(track).map_or_else(|| "Take".to_string(), |t| sanitize_name(&t.name));
        let name = if n == 1 { format!("{base}_recovered") } else { format!("{base}_recovered_{n:02}") };
        if let Some(clip) = e.session_mut().find_clip_mut(*id) {
            clip.name = name;
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sc-midijournal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn header(tracks: Vec<TrackId>) -> Header {
        let plan = Recording { start: 48_000, pass: None, punch: None, rate: 0, takes: Vec::new(), midi_journal: None };
        let latch = Latch { output_frame: 10_000, position: 48_000, device_rate: 48_000, session_rate: 48_000 };
        Header::new(latch, &plan, None, tracks)
    }
    fn rec(frame: u64, msg: Msg) -> Record {
        Record { frame, track: TrackId(1), channel: 0, approximate: false, msg }
    }
    fn two_notes() -> Vec<Record> {
        vec![
            rec(10_000 + 24_000, Msg::NoteOn { pitch: 60, velocity: 90 }),
            rec(10_000 + 48_000, Msg::NoteOff { pitch: 60, velocity: 0 }),
            rec(10_000 + 72_000, Msg::NoteOn { pitch: 64, velocity: 80 }),
            rec(10_000 + 96_000, Msg::NoteOff { pitch: 64, velocity: 0 }),
        ]
    }

    #[test]
    fn a_pass_is_journaled_with_its_header_and_flushed() {
        let d = dir("write");
        let mut w = Writer::new(d.join("s.midijournal"));
        let h = header(vec![TrackId(1)]);
        w.append(&h, &[], 0).unwrap();
        assert!(!w.path().exists(), "no file until there is an event");
        let records = two_notes();
        w.append(&h, &records[..2], 2).unwrap();
        w.append(&h, &records[2..], 4).unwrap();
        w.flush().unwrap();
        assert_eq!(w.consumed(), 4);
        let j = read(w.path()).unwrap();
        assert_eq!((j.header.clone(), j.records.clone(), j.truncated), (h, records, false));
        let path = w.path().to_path_buf();
        w.discard();
        assert!(!path.exists(), "a clean stop deletes the journal");
    }

    #[test]
    fn a_journal_cut_off_mid_record_keeps_the_complete_records() {
        let d = dir("cut");
        let path = d.join("s.midijournal");
        let mut w = Writer::new(path.clone());
        w.append(&header(vec![TrackId(1)]), &two_notes(), 4).unwrap();
        w.flush().unwrap();
        let text = std::fs::read(&path).unwrap();
        std::fs::write(&path, &text[..text.len() - 9]).unwrap();
        let j = read(&path).unwrap();
        assert_eq!((j.records.len(), j.truncated), (3, true));
    }

    #[test]
    fn a_journal_with_two_notes_and_no_clip_rebuilds_the_clip_at_the_right_ticks() {
        let d = dir("recover");
        let path = d.join("s.midijournal");
        let mut e = Engine::default();
        let t = TrackId(e.execute("track.new", &serde_json::json!({"kind": "midi", "name": "Keys"})).unwrap()["tracks"][0].as_u64().unwrap());
        let mut w = Writer::new(path.clone());
        let mut h = header(vec![t]);
        h.plan_start = 0;
        let records: Vec<Record> = two_notes().into_iter().map(|r| Record { track: t, ..r }).collect();
        w.append(&h, &records, 4).unwrap();
        w.flush().unwrap();
        let ids = recover(&mut e, &path).unwrap();
        assert_eq!(ids.len(), 1);
        let s = e.session();
        let (_, clip) = s.find_clip(ids[0]).unwrap();
        assert_eq!(clip.name, "Keys_recovered");
        let soundcraft_model::ClipContent::Midi { sequence } = &clip.content else { panic!("not MIDI") };
        // 120 bpm at 48 kHz: a beat is 24,000 samples, 960 ticks.
        let notes: Vec<_> = sequence.notes.iter().map(|n| (n.pitch, n.start, n.length, n.velocity)).collect();
        assert_eq!(notes, vec![(60, 960, 960, 90), (64, 2880, 960, 80)]);
    }
}
