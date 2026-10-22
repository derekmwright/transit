//! Incremental mix and track-stem bounces.

use super::*;

/// A bounce of the main mix, or of one bus, over a range, written to its file block by block so no
/// bounce has to fit in memory, and rendered a slice at a time ([`Bounce::step`]) so a UI can show
/// progress. The file appears at its path only once complete; a bounce that fails, or is dropped
/// unfinished, leaves any older file there alone.
pub struct Bounce {
    /// Offline mode for hosted plugins: they sample their parameter curves at creation, since no
    /// editor `idle` runs during a render.
    _offline: soundcraft_vst3_host::OfflineScope,
    session: Arc<Session>,
    path: PathBuf,
    part: PathBuf,
    range: Range,
    bus: Option<BusId>,
    fold: Option<ChannelFormat>,
    normalize: bool,
    /// Measuring the peak to normalize to, the first of two passes.
    measuring: bool,
    render: soundcraft_mix::RangeRender,
    block: Vec<Vec<f32>>,
    done: usize,
    gain: f32,
    peak: f32,
    meter: soundcraft_dsp::meter::LoudnessMeter,
    enc: Option<soundcraft_audio_io::FileEncoder<Out>>,
}

/// A finished bounce: the size of its file and its levels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounced {
    pub bytes: u64,
    pub peak_db: f32,
    pub lufs: f32,
    pub true_peak_db: f32,
}

/// Rendering choices for a mix or bus bounce, independent of file encoding.
#[derive(Debug, Clone, Copy, Default)]
pub struct BounceOptions {
    pub bus: Option<BusId>,
    pub normalize: bool,
    pub fold_stereo: bool,
}

impl Bounce {
    /// Starts a bounce of `s`'s main mix, or of `bus`, over `r` to `path`: every channel (SMPTE/WAV
    /// order, with the WAV speaker mask of its format), or an ITU stereo fold-down when
    /// `fold_stereo`, peak-normalized to -0.1 dBFS when `normalize` (which renders twice).
    pub fn new(
        s: Arc<Session>,
        path: &Path,
        r: Range,
        opts: &EncodeOptions,
        bus: Option<BusId>,
        normalize: bool,
        fold_stereo: bool,
    ) -> Result<Bounce> {
        Self::with_options(s, path, r, opts, BounceOptions { bus, normalize, fold_stereo })
    }

    /// Starts a bounce with named rendering choices. See [`Self::new`] for output semantics.
    pub fn with_options(s: Arc<Session>, path: &Path, r: Range, opts: &EncodeOptions, options: BounceOptions) -> Result<Bounce> {
        let BounceOptions { bus, normalize, fold_stereo } = options;
        check_bounce_range(&s, r, "file.bounce_mix")?;
        let (fmt, channels, fold) = bounce_layout(&s, bus, fold_stereo);
        let name = path.file_name().map_or_else(|| "bounce".into(), |n| n.to_string_lossy().into_owned());
        let part = path.with_file_name(format!("{name}.part"));
        crate::file_access::check_write(path).map_err(EngineError::Io)?;
        crate::file_access::check_write(&part).map_err(EngineError::Io)?;
        let enc = (|| -> std::result::Result<_, soundcraft_audio_io::AudioError> {
            let enc = soundcraft_audio_io::FileEncoder::new(Out::create(&part)?, opts, channels, s.sample_rate.hz(), Some(fmt.channel_mask()))?;
            enc.check_room(u64::try_from(r.len()).unwrap_or(0))?;
            Ok(enc)
        })()
        .map_err(|err| {
            let _ = std::fs::remove_file(&part);
            EngineError::Io(format!("{}: {err}", path.display()))
        })?;
        // Hosted plug-ins are created while the render is compiled: offline mode must already be on.
        let offline = soundcraft_vst3_host::offline_scope();
        let render = soundcraft_mix::RangeRender::try_new(&s, bus, r, 1024)
            .map_err(|err| EngineError::Io(format!("{}: the session could not be prepared for rendering: {err:?}", path.display())))?;
        let meter = soundcraft_dsp::meter::LoudnessMeter::new(s.sample_rate.as_f64() as f32, channels);
        Ok(Bounce {
            _offline: offline,
            session: s,
            path: path.to_path_buf(),
            part,
            range: r,
            bus,
            fold,
            normalize,
            measuring: normalize,
            render,
            block: Vec::new(),
            done: 0,
            gain: 1.0,
            peak: 0.0,
            meter,
            enc: Some(enc),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn range(&self) -> Range {
        self.range
    }

    /// How far along the bounce is, from 0 to 1.
    pub fn progress(&self) -> f32 {
        let pass = (self.done as f64 / self.range.len().max(1) as f64).min(1.0) as f32;
        match (self.normalize, self.measuring) {
            (false, _) => pass,
            (true, true) => pass / 2.0,
            (true, false) => 0.5 + pass / 2.0,
        }
    }

    /// Renders blocks until `more` returns false (at least one per call). Returns the finished
    /// bounce once its file is complete.
    pub fn step(&mut self, mut more: impl FnMut() -> bool) -> Result<Option<Bounced>> {
        loop {
            match self.render.next_block() {
                Some((chunk, n)) => {
                    fill(&mut self.block, chunk, n, self.fold, self.gain);
                    self.done += n;
                    self.peak = self.peak.max(soundcraft_dsp::offline::peak(&self.block));
                    if !self.measuring {
                        self.meter.process(&self.block, n);
                        if let Some(enc) = &mut self.enc {
                            enc.write(&self.block, n).map_err(|err| EngineError::Io(format!("{}: {err}", self.path.display())))?;
                        }
                    }
                }
                None if self.measuring => {
                    self.gain = if self.peak > 0.0 { soundcraft_dsp::db_to_gain(-0.1) / self.peak } else { 1.0 };
                    self.peak = 0.0;
                    self.done = 0;
                    self.measuring = false;
                    self.render = soundcraft_mix::RangeRender::try_new(&self.session, self.bus, self.range, 1024).map_err(|err| {
                        EngineError::Io(format!("{}: the session could not be prepared for rendering: {err:?}", self.path.display()))
                    })?;
                }
                None => return self.finish().map(Some),
            }
            if !more() {
                return Ok(None);
            }
        }
    }

    /// Renders the whole bounce (see [`Bounce::step`]).
    pub fn run(mut self) -> Result<Bounced> {
        loop {
            if let Some(done) = self.step(|| true)? {
                return Ok(done);
            }
        }
    }

    fn finish(&mut self) -> Result<Bounced> {
        let bytes = match self.enc.take() {
            Some(enc) => enc.finish().map_err(|err| EngineError::Io(format!("{}: {err}", self.path.display())))?.complete(&self.part, &self.path)?,
            None => 0,
        };
        Ok(Bounced {
            bytes,
            peak_db: soundcraft_dsp::gain_to_db(self.peak),
            lufs: self.meter.integrated_lufs(),
            true_peak_db: self.meter.true_peak_db(),
        })
    }
}

impl Drop for Bounce {
    /// Removes the part file unless the bounce completed (which renamed it).
    fn drop(&mut self) {
        drop(self.enc.take());
        let _ = std::fs::remove_file(&self.part);
    }
}

const MAX_BOUNCE_SECS: f64 = 4.0 * 3600.0;

pub(super) fn check_bounce_range(s: &Session, range: Range, command: &str) -> Result<()> {
    if range.len() > s.sample_rate.samples(MAX_BOUNCE_SECS) {
        return Err(crate::cmd::bad(command, "bounces are limited to 4 hours"));
    }
    Ok(())
}

/// The format, channel count and fold-down source of a bounce of the main mix or `bus`: every
/// channel (a mono or stereo main mix as stereo), or an ITU stereo fold-down of a surround source
/// when `fold_stereo`.
pub(super) fn bounce_layout(s: &Session, bus: Option<BusId>, fold_stereo: bool) -> (ChannelFormat, usize, Option<ChannelFormat>) {
    let nch = soundcraft_mix::source_channels(s, bus);
    let fmt = match bus {
        Some(id) => s.bus(id).map_or(ChannelFormat::Stereo, |b| b.format),
        None if nch <= 2 => ChannelFormat::Stereo,
        None => s.main_format(),
    };
    if fold_stereo && nch > 2 { (ChannelFormat::Stereo, 2, Some(fmt)) } else { (fmt, nch, None) }
}

/// The first `n` frames of `chunk`, folded down from `fold` when set and scaled by `gain`.
fn fill(block: &mut Vec<Vec<f32>>, chunk: &[Vec<f32>], n: usize, fold: Option<ChannelFormat>, gain: f32) {
    let folded = fold.map(|from| soundcraft_mix::fold_down_stereo(from, chunk));
    let src = folded.as_deref().unwrap_or(chunk);
    block.resize_with(src.len(), Vec::new);
    for (d, c) in block.iter_mut().zip(src) {
        d.clear();
        d.extend(c.iter().take(n).map(|v| v * gain));
    }
}

/// Bounce one file per audio/instrument track (each soloed, so its sends and auxes are included)
/// into `dir`. Returns the written paths.
pub fn bounce_stems(e: &Engine, dir: &str, r: Range, opts: &EncodeOptions) -> Result<Vec<String>> {
    bounce_stems_with(e, dir, r, opts, false)
}

/// [`bounce_stems`] in the main format, or folded down to stereo when `fold_stereo`.
pub fn bounce_stems_with(e: &Engine, dir: &str, r: Range, opts: &EncodeOptions, fold_stereo: bool) -> Result<Vec<String>> {
    let s = e.session();
    check_bounce_range(s, r, "file.bounce_stems")?;
    make_dir(dir)?;
    let ext = match opts.format {
        FileFormat::Aiff => "aif",
        FileFormat::Flac => "flac",
        _ => "wav",
    };
    let mut out = Vec::new();
    let stems: Vec<(TrackId, String)> = s
        .tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Audio | TrackKind::Instrument | TrackKind::Midi) && !t.inactive && !t.mixer.mute)
        .map(|t| (t.id, t.name.clone()))
        .collect();
    let mut solo = Arc::new(s.clone());
    for (id, name) in stems {
        for t in &mut Arc::make_mut(&mut solo).tracks {
            t.mixer.solo = t.id == id;
        }
        let path = Path::new(dir).join(format!("{} - {}.{ext}", sanitize_name(&s.name), sanitize_name(&name)));
        Bounce::with_options(Arc::clone(&solo), &path, r, opts, BounceOptions { fold_stereo, ..BounceOptions::default() })?.run()?;
        out.push(path.to_string_lossy().into_owned());
    }
    Ok(out)
}
