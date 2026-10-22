//! Stems by output path (ADR 0015 slice e): one file per output path, rendered together in one
//! pass of the plan renderer. Offline rendering is control-side, so this allocates freely; the
//! per-block render ([`soundcraft_mix::RangeRender::next_block`]) does not.
//!
//! Alignment: the plan pads every sink (Main and each output path) to the one device latency L
//! (ADR 0015 section 6), and `RangeRender` drops the first L samples from all of them together.
//! Every file therefore starts at the range start, sample-aligned with the others and with a
//! plain main bounce of the same session.

use super::{Out, bounce_layout, check_bounce_range, make_dir, sanitize_name};
use crate::{EngineError, Result};
use soundcraft_audio_io::{EncodeOptions, FileEncoder, FileFormat};
use soundcraft_model::{ChannelFormat, IoDirection, IoMap, Route, Session, TrackKind};
use soundcraft_time::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How the peaks of a stem set are normalized to -0.1 dBFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathNormalize {
    #[default]
    Off,
    /// One gain from the loudest file, so the stems keep their relative levels.
    Joint,
    /// Each file to its own peak.
    Each,
}

/// One finished stem file.
#[derive(Debug, Clone, PartialEq)]
pub struct PathStem {
    /// The output path's name.
    pub path: String,
    pub file: PathBuf,
    pub bytes: u64,
    pub channels: usize,
    /// Peak level of the file as written (after any normalizing gain).
    pub peak_db: f32,
    /// Some track, bus or send routes to the path (Main always counts).
    pub routed: bool,
    /// The file is digital silence: nothing routed to the path, or what did made no sound.
    pub empty: bool,
}

/// A finished stem render.
#[derive(Debug, Clone, PartialEq)]
pub struct PathBounced {
    pub stems: Vec<PathStem>,
    /// The gain applied to every file by [`PathNormalize::Joint`], in dB.
    pub joint_gain_db: Option<f32>,
}

/// The output paths of `io` that `s` routes to: Main's path first when the table lists it, then
/// every other path some track or send names with `Route::Hardware`, in table order.
#[must_use]
pub fn routed_paths(s: &Session, io: &IoMap) -> Vec<String> {
    io.paths_of(IoDirection::Output).filter(|p| p.name.eq_ignore_ascii_case(&io.main) || is_routed(s, &p.name)).map(|p| p.name.clone()).collect()
}

/// Whether some track's output or send names the output path `name`.
#[must_use]
pub fn is_routed(s: &Session, name: &str) -> bool {
    let hits = |r: &Route| matches!(r, Route::Hardware(h) if h.eq_ignore_ascii_case(name));
    s.tracks
        .iter()
        .filter(|t| !matches!(t.kind, TrackKind::Master | TrackKind::Vca))
        .any(|t| hits(&t.mixer.output) || t.mixer.sends.iter().flatten().any(|x| hits(&x.target)))
}

/// One file being written and where its channels come from in a render block.
struct Stem {
    name: String,
    file: PathBuf,
    part: PathBuf,
    routed: bool,
    /// `(first channel, count)` in the block; `None` renders silence.
    at: Option<(usize, usize)>,
    /// A sink with Main's path name also plays on Main's device channels: it sums into Main's file.
    extra: Option<(usize, usize)>,
    channels: usize,
    block: Vec<Vec<f32>>,
    peak: f32,
    gain: f32,
    enc: Option<FileEncoder<Out>>,
    bytes: u64,
}

/// A bounce of several output paths in one pass, a slice at a time ([`PathBounce::step`]) so a UI
/// can show progress. The files appear only once the whole render is complete; a bounce that
/// fails, or is dropped unfinished, leaves any older files alone.
pub struct PathBounce {
    _offline: soundcraft_vst3_host::OfflineScope,
    session: Arc<Session>,
    io: IoMap,
    range: Range,
    normalize: PathNormalize,
    /// Measuring the peaks to normalize to, the first of two passes.
    measuring: bool,
    render: soundcraft_mix::RangeRender,
    stems: Vec<Stem>,
    done: usize,
    joint_gain: f32,
    dir: PathBuf,
}

impl PathBounce {
    /// Starts stems of `paths` (names in `io`'s output table, or `main` for Main's path; empty:
    /// every path that has at least one route) over `r` into `dir`, as `<session>_<path>.<ext>`.
    pub fn new(
        s: Arc<Session>,
        io: &IoMap,
        dir: &Path,
        r: Range,
        opts: &EncodeOptions,
        paths: &[String],
        normalize: PathNormalize,
    ) -> Result<PathBounce> {
        const CMD: &str = "file.bounce_paths";
        check_bounce_range(&s, r, CMD)?;
        let mut names: Vec<String> = if paths.is_empty() { routed_paths(&s, io) } else { Vec::new() };
        for want in paths {
            let want = want.trim();
            let found = if want.eq_ignore_ascii_case("main") { io.path(&io.main) } else { io.path(want) };
            let Some(p) = found.filter(|p| p.direction == IoDirection::Output) else {
                return Err(EngineError::BadParams(CMD.into(), format!("unknown output path `{want}`")));
            };
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&p.name)) {
                names.push(p.name.clone());
            }
        }
        if names.is_empty() {
            return Err(EngineError::BadParams(CMD.into(), "no output paths to bounce".into()));
        }
        make_dir(dir)?;
        let ext = match opts.format {
            FileFormat::Aiff => "aif",
            FileFormat::Flac => "flac",
            _ => "wav",
        };
        // Hosted plug-ins are created while the render is compiled: offline mode must already be on.
        let offline = soundcraft_vst3_host::offline_scope();
        let render = soundcraft_mix::RangeRender::try_new_paths(&s, r, 1024, io)
            .map_err(|err| EngineError::Io(format!("the session could not be prepared for rendering: {err:?}")))?;
        let (main_fmt, main_n, _) = bounce_layout(&s, None, false);
        let mut offsets = Vec::new();
        let mut first = main_n;
        for k in render.sinks() {
            offsets.push((first, k.dest.len()));
            first += k.dest.len();
        }
        let mut stems: Vec<Stem> = Vec::new();
        for name in names {
            let is_main = name.eq_ignore_ascii_case(&io.main);
            let sink = render.sinks().iter().position(|k| k.name.eq_ignore_ascii_case(&name));
            let spec = io.path(&name);
            let width = spec.map_or(2, |p| p.channels.len().max(1));
            let (format, at, extra, routed) = match (is_main, sink) {
                (true, k) => (main_fmt, Some((0, main_n)), k.and_then(|k| offsets.get(k).copied()), true),
                (false, Some(k)) => {
                    let format = render.sinks().get(k).map_or_else(|| ChannelFormat::for_channels(width), |x| x.format);
                    (format, offsets.get(k).copied(), None, true)
                }
                (false, None) => (spec.map_or_else(|| ChannelFormat::for_channels(width), |p| p.format), None, None, is_routed(&s, &name)),
            };
            let channels = if is_main { main_n } else { at.map_or(width.min(format.channels()).max(1), |(_, n)| n) };
            let stem = format!("{}_{}", sanitize_name(&s.name), sanitize_name(&name));
            let mut file = dir.join(format!("{stem}.{ext}"));
            let mut n = 2;
            while stems.iter().any(|x| x.file == file) {
                file = dir.join(format!("{stem}_{n}.{ext}"));
                n += 1;
            }
            let part = file.with_file_name(format!("{}.part", file.file_name().map_or_else(String::new, |f| f.to_string_lossy().into_owned())));
            crate::file_access::check_write(&file).map_err(EngineError::Io)?;
            crate::file_access::check_write(&part).map_err(EngineError::Io)?;
            let enc = (|| -> std::result::Result<_, soundcraft_audio_io::AudioError> {
                let enc = FileEncoder::new(Out::create(&part)?, opts, channels, s.sample_rate.hz(), Some(format.channel_mask()))?;
                enc.check_room(u64::try_from(r.len()).unwrap_or(0))?;
                Ok(enc)
            })()
            .map_err(|err| {
                let _ = std::fs::remove_file(&part);
                EngineError::Io(format!("{}: {err}", file.display()))
            })?;
            stems.push(Stem { name, file, part, routed, at, extra, channels, block: Vec::new(), peak: 0.0, gain: 1.0, enc: Some(enc), bytes: 0 });
        }
        Ok(PathBounce {
            _offline: offline,
            session: s,
            io: io.clone(),
            range: r,
            normalize,
            measuring: normalize != PathNormalize::Off,
            render,
            stems,
            done: 0,
            joint_gain: 1.0,
            dir: dir.to_path_buf(),
        })
    }

    /// The folder the stems go to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn range(&self) -> Range {
        self.range
    }

    /// The files this bounce writes, in order.
    pub fn files(&self) -> Vec<PathBuf> {
        self.stems.iter().map(|s| s.file.clone()).collect()
    }

    /// How far along the bounce is, from 0 to 1.
    pub fn progress(&self) -> f32 {
        let pass = (self.done as f64 / self.range.len().max(1) as f64).min(1.0) as f32;
        match (self.normalize == PathNormalize::Off, self.measuring) {
            (true, _) => pass,
            (false, true) => pass / 2.0,
            (false, false) => 0.5 + pass / 2.0,
        }
    }

    /// Renders blocks until `more` returns false (at least one per call). Returns the finished
    /// set once every file is complete.
    pub fn step(&mut self, mut more: impl FnMut() -> bool) -> Result<Option<PathBounced>> {
        loop {
            match self.render.next_block() {
                Some((chunk, n)) => {
                    let measuring = self.measuring;
                    for st in &mut self.stems {
                        st.fill(chunk, n, if measuring { 1.0 } else { st.gain });
                        st.peak = st.peak.max(soundcraft_dsp::offline::peak(&st.block));
                        if !measuring && let Some(enc) = &mut st.enc {
                            enc.write(&st.block, n).map_err(|err| EngineError::Io(format!("{}: {err}", st.file.display())))?;
                        }
                    }
                    self.done += n;
                }
                None if self.measuring => {
                    let target = soundcraft_dsp::db_to_gain(-0.1);
                    let loudest = self.stems.iter().map(|s| s.peak).fold(0.0f32, f32::max);
                    self.joint_gain = if loudest > 0.0 { target / loudest } else { 1.0 };
                    for st in &mut self.stems {
                        st.gain = match self.normalize {
                            PathNormalize::Joint => self.joint_gain,
                            PathNormalize::Each if st.peak > 0.0 => target / st.peak,
                            _ => 1.0,
                        };
                        st.peak = 0.0;
                    }
                    self.done = 0;
                    self.measuring = false;
                    self.render = soundcraft_mix::RangeRender::try_new_paths(&self.session, self.range, 1024, &self.io)
                        .map_err(|err| EngineError::Io(format!("the session could not be prepared for rendering: {err:?}")))?;
                }
                None => return self.finish().map(Some),
            }
            if !more() {
                return Ok(None);
            }
        }
    }

    /// Renders the whole bounce (see [`PathBounce::step`]).
    pub fn run(mut self) -> Result<PathBounced> {
        loop {
            if let Some(done) = self.step(|| true)? {
                return Ok(done);
            }
        }
    }

    fn finish(&mut self) -> Result<PathBounced> {
        // Every encoder finishes before any part file replaces a file, so a failure keeps the older set.
        let mut finished = Vec::new();
        for st in &mut self.stems {
            let Some(enc) = st.enc.take() else { continue };
            let out = enc.finish().map_err(|err| EngineError::Io(format!("{}: {err}", st.file.display())))?;
            finished.push(out);
        }
        for (st, out) in self.stems.iter_mut().zip(finished) {
            st.bytes = out.complete(&st.part, &st.file)?;
        }
        let stems = self
            .stems
            .iter()
            .map(|st| PathStem {
                path: st.name.clone(),
                file: st.file.clone(),
                bytes: st.bytes,
                channels: st.channels,
                peak_db: soundcraft_dsp::gain_to_db(st.peak),
                routed: st.routed,
                empty: st.peak <= 0.0,
            })
            .collect();
        let joint_gain_db = (self.normalize == PathNormalize::Joint).then(|| soundcraft_dsp::gain_to_db(self.joint_gain));
        Ok(PathBounced { stems, joint_gain_db })
    }
}

impl Drop for PathBounce {
    /// Removes the part files that did not complete.
    fn drop(&mut self) {
        for st in &mut self.stems {
            drop(st.enc.take());
            let _ = std::fs::remove_file(&st.part);
        }
    }
}

impl Stem {
    /// The first `n` frames of this stem's channels in `chunk`, scaled by `gain`.
    fn fill(&mut self, chunk: &[Vec<f32>], n: usize, gain: f32) {
        self.block.resize_with(self.channels, Vec::new);
        let (at, extra) = (self.at, self.extra);
        for (c, d) in self.block.iter_mut().enumerate() {
            d.clear();
            let src = |range: Option<(usize, usize)>| range.filter(|&(_, count)| c < count).and_then(|(first, _)| chunk.get(first + c));
            let (a, b) = (src(at), src(extra));
            d.extend((0..n).map(|i| {
                let v = a.and_then(|x| x.get(i)).copied().unwrap_or(0.0) + b.and_then(|x| x.get(i)).copied().unwrap_or(0.0);
                v * gain
            }));
        }
    }
}

/// Renders stems of `paths` (see [`PathBounce::new`]) to completion.
pub fn bounce_paths(
    s: Arc<Session>,
    io: &IoMap,
    dir: &Path,
    r: Range,
    opts: &EncodeOptions,
    paths: &[String],
    normalize: PathNormalize,
) -> Result<PathBounced> {
    PathBounce::new(s, io, dir, r, opts, paths, normalize)?.run()
}
