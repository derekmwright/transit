//! Placement against the grid (`--placement`, issue #71): does a stored compensation put a hardware
//! synth's notes where they were meant to sound?
//!
//! The bench schedules a pattern of note-ons at intended device sample positions P_i (a grid
//! starting [`Settings::lead_ms`] after the scheduling instant), sends each at P_i minus the
//! compensation, the deadline being the host time of that position by the position clock
//! ([`crate::fit`]: device sample position against capture-callback arrival time, the same reference as the app's ClockFitter input map; the app's MIDI output thread schedules on the output presentation map, which slice 3 of #71 has to reconcile) and the wait the
//! output thread's (sleep until [`SPIN_NS`] before the deadline, then yield). The return is
//! captured by sample position, each onset O_i found as in `--midi-roundtrip`, and the placement
//! error is E_i = O_i - P_i. With `--compensation` it runs the pattern twice: uncompensated (E is
//! the round trip) and compensated (E is what is left: synth and clock noise, or a wrong offset).
//!
//! Note safety is the round trip's: one guard releases the note on every path, Ctrl+C included.

use crate::midi_roundtrip::{
    self as rt, Driver, FLOOR_MS, Interrupt, MidiSink, Notes, POLL_NS, RING_SECONDS, STALL_MS, SendFit, Started, Streams, frames_of, pass,
};
use crate::onset;
use crate::session::{self, Request};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use transit_audio_backend as ab;

/// The MIDI output thread sleeps until this long before a deadline and yields for the rest.
pub const SPIN_NS: u64 = 300_000;
pub const DEFAULT_PATTERN_COUNT: u32 = 8;
pub const DEFAULT_PATTERN_SPACING_MS: u32 = 500;
/// The first intended position lies this far after the scheduling instant; |compensation| must be
/// below it.
pub const DEFAULT_LEAD_MS: u32 = 250;
/// Notes of a pattern at least this far apart.
pub const MIN_SPACING_MS: u32 = 50;
/// A note is held this long (at most half the pattern's smallest spacing).
pub const NOTE_HOLD_MS: u32 = 100;

/// The compensation as given: samples, or milliseconds (converted at the device's rate).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Compensation {
    Samples(i64),
    Ms(f64),
}

impl Compensation {
    /// `1070` (samples) or `24.3ms`; negative sends later.
    pub fn parse(value: &str) -> Result<Self, String> {
        let bad = || format!("--compensation: `{value}` is not a number of samples (`1070`) or milliseconds (`24.3ms`)");
        let trimmed = value.trim();
        if let Some(ms) = trimmed.strip_suffix("ms") {
            let ms: f64 = ms.trim().parse().map_err(|_| bad())?;
            return if ms.is_finite() { Ok(Self::Ms(ms)) } else { Err(bad()) };
        }
        trimmed.strip_suffix("samples").unwrap_or(trimmed).trim().parse().map(Self::Samples).map_err(|_| bad())
    }

    /// In samples at `rate`.
    pub fn samples(self, rate: u32) -> i64 {
        match self {
            Self::Samples(n) => n,
            Self::Ms(ms) => (ms * f64::from(rate) / 1000.0).round() as i64,
        }
    }
}

/// The `--placement` command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub port: String,
    pub settings: Settings,
    pub json: Option<std::path::PathBuf>,
}

/// What a placement plays. `base.trials` is not used.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub base: rt::Settings,
    pub compensation: Option<Compensation>,
    /// Intended onsets, in ms from the pattern's start, increasing.
    pub pattern_ms: Vec<u32>,
    pub lead_ms: u32,
}

impl Settings {
    /// The defaults for `channel` and `returns` (0-based): 8 notes 500 ms apart, no compensation.
    pub fn new(channel: u8, returns: Vec<u16>) -> Self {
        Self {
            base: rt::Settings::new(channel, returns),
            compensation: None,
            pattern_ms: grid(DEFAULT_PATTERN_COUNT, DEFAULT_PATTERN_SPACING_MS),
            lead_ms: DEFAULT_LEAD_MS,
        }
    }

    /// The smallest distance between two notes of the pattern, in ms.
    fn min_spacing_ms(&self) -> u32 {
        self.pattern_ms.windows(2).map(|w| w[1].saturating_sub(w[0])).min().unwrap_or(u32::MAX)
    }

    fn hold_ms(&self) -> u32 {
        NOTE_HOLD_MS.min(self.min_spacing_ms() / 2)
    }

    /// The pattern, the lead and, at `rate`, the compensation against the lead.
    pub fn check(&self, rate: u32) -> Result<(), String> {
        let mut base = self.base.clone();
        base.trials = 1;
        base.check()?;
        if self.pattern_ms.is_empty() {
            return Err("the pattern needs at least one note".into());
        }
        if self.min_spacing_ms() < MIN_SPACING_MS {
            return Err(format!("the pattern's notes must be in increasing order, at least {MIN_SPACING_MS} ms apart"));
        }
        if self.lead_ms == 0 {
            return Err("--lead-ms must be positive".into());
        }
        if let Some(c) = self.compensation {
            let samples = c.samples(rate);
            let lead = frames_of(rate, u64::from(self.lead_ms)) as i64;
            let ms = samples as f64 * 1000.0 / f64::from(rate.max(1));
            if samples <= -lead {
                return Err(format!(
                    "--compensation {samples} samples ({ms:.2} ms) is negative by more than the lead ({} ms): each note would be sent that long after its intended position; a negative compensation must stay within --lead-ms",
                    self.lead_ms
                ));
            }
            if samples >= lead {
                return Err(format!(
                    "--compensation {samples} samples ({ms:.2} ms) is not below the lead ({} ms): the first note would be due before it was scheduled; raise --lead-ms",
                    self.lead_ms
                ));
            }
        }
        Ok(())
    }
}

/// `count` onsets `spacing_ms` apart from 0.
pub fn grid(count: u32, spacing_ms: u32) -> Vec<u32> {
    (0..count).map(|k| k.saturating_mul(spacing_ms)).collect()
}

/// One note of a run, in sample positions.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// P: where the note should sound.
    pub intended: u64,
    /// P minus the compensation: the send's deadline position.
    pub target: u64,
    /// The send instant's position.
    pub send: u64,
    /// O: the onset (the refined crossing), and the threshold crossing.
    pub onset: u64,
    pub crossing: u64,
    pub pre_send: f32,
    pub threshold: f32,
    pub peak: f32,
    pub send_call_ns: u64,
    pub fit: SendFit,
}

/// Whether the fit's newest block sat further from the line than `limit_ns`: a change in the
/// driver's dispatch delay the 2 s line has not followed yet. A warning only.
pub(crate) fn fit_drift(fit: &SendFit, limit_ns: u64) -> bool {
    fit.leave_last_out_ns.abs() > limit_ns as f64
}

impl Note {
    /// E = O - P, in samples.
    pub fn error(&self) -> i64 {
        self.onset as i64 - self.intended as i64
    }
    /// O minus the send: the round trip of this note.
    pub fn latency(&self) -> i64 {
        self.onset as i64 - self.send as i64
    }
    /// The send against its deadline, in samples (positive: late).
    pub fn schedule_error(&self) -> i64 {
        self.send as i64 - self.target as i64
    }
    /// The detector's late-side uncertainty, in samples.
    pub fn uncertainty(&self) -> u64 {
        self.onset.abs_diff(self.crossing)
    }
}

/// One pass of the pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Samples each send went early (negative: late).
    pub compensation: i64,
    pub notes: Vec<Note>,
}

impl Run {
    pub fn summary(&self) -> Option<SignedSummary> {
        summarize_signed(&self.notes.iter().map(Note::error).collect::<Vec<_>>())
    }
}

/// Mean, median, spread and range of signed values.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedSummary {
    pub n: usize,
    pub mean: f64,
    pub median: f64,
    pub std_dev: f64,
    pub min: i64,
    pub max: i64,
}

pub fn summarize_signed(values: &[i64]) -> Option<SignedSummary> {
    let (&min, &max) = (values.iter().min()?, values.iter().max()?);
    let n = values.len();
    let mean = values.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let median = if n % 2 == 1 { sorted[n / 2] as f64 } else { (sorted[n / 2 - 1] + sorted[n / 2]) as f64 / 2.0 };
    let std_dev = if n > 1 { (values.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt() } else { 0.0 };
    Some(SignedSummary { n, mean, median, std_dev, min, max })
}

/// A placement's result.
#[derive(Debug, Clone)]
pub struct Report {
    pub device: String,
    pub midi_port: String,
    pub settings: Settings,
    pub rate: u32,
    pub buffer: Option<u32>,
    pub input_latency: Option<u64>,
    /// The lowest envelope peak of the reference window and every pre-send window.
    pub floor: f32,
    pub reference_floor: f32,
    /// Uncompensated, then compensated when `--compensation` was given.
    pub runs: Vec<Run>,
    pub envelope_window: usize,
    pub stream_errors: u64,
    pub lost_blocks: u64,
}

impl Report {
    pub fn ms(&self, samples: f64) -> f64 {
        samples * 1000.0 / f64::from(self.rate.max(1))
    }

    fn run_label(&self, run: &Run) -> String {
        if run.compensation == 0 {
            "uncompensated".to_string()
        } else {
            format!("compensated ({} samples, {:.2} ms)", run.compensation, self.ms(run.compensation as f64))
        }
    }

    fn summary_line(&self, run: &Run) -> Option<String> {
        let m = run.summary()?;
        Some(format!(
            "{} placement error: mean {:+.1} samples ({:+.2} ms), median {:+.1} ({:+.2} ms), std dev {:.1} samples ({:.2} ms), min {:+} ({:+.2} ms), max {:+} ({:+.2} ms), n = {}",
            self.run_label(run),
            m.mean,
            self.ms(m.mean),
            m.median,
            self.ms(m.median),
            m.std_dev,
            self.ms(m.std_dev),
            m.min,
            self.ms(m.min as f64),
            m.max,
            self.ms(m.max as f64),
            m.n
        ))
    }

    /// The largest |E| of the compensated run, in samples.
    pub fn compensated_within(&self) -> Option<u64> {
        let run = self.runs.iter().find(|r| r.compensation != 0)?;
        run.notes.iter().map(|n| n.error().unsigned_abs()).max()
    }

    /// The report as printed: the runs side by side, then their summaries and the reading.
    pub fn lines(&self) -> Vec<String> {
        let s = &self.settings;
        let b = &s.base;
        let buffer = self.buffer.map_or("the driver's".to_string(), |b| b.to_string());
        let pattern = s.pattern_ms.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
        let mut lines = vec![
            "MIDI placement against the grid (note-ons scheduled at intended device positions P, sent at P minus the compensation; error E = onset - P)".to_string(),
            format!("timing reference: {}", crate::fit::TIMING_REFERENCE),
            format!(
                "MIDI \"{}\" channel {} note {} velocity {} -> \"{}\" input {}, {} Hz, buffer {buffer}, floor {} (lowest of the reference window {} and the pre-send windows), warm-up note {}",
                self.midi_port,
                b.channel + 1,
                b.note,
                b.velocity,
                self.device,
                b.returns_label(),
                self.rate,
                onset::dbfs(self.floor),
                onset::dbfs(self.reference_floor),
                if b.warmup { "sent, not measured" } else { "off" }
            ),
            format!("pattern: {} notes at {pattern} ms, the first {} ms after scheduling; each held {} ms", s.pattern_ms.len(), s.lead_ms, s.hold_ms()),
        ];
        let header: Vec<String> = self.runs.iter().map(|r| self.run_label(r)).collect();
        lines.push(format!("note: {}", header.join(" | ")));
        for (i, at) in s.pattern_ms.iter().enumerate() {
            let cells: Vec<String> = self
                .runs
                .iter()
                .map(|r| r.notes.get(i).map_or("-".to_string(), |n| format!("E {:+} samples ({:+.2} ms)", n.error(), self.ms(n.error() as f64))))
                .collect();
            lines.push(format!("note {:>2} at +{at} ms: {}", i + 1, cells.join(" | ")));
        }
        for run in &self.runs {
            lines.extend(self.summary_line(run));
        }
        let uncompensated = self.runs.first().and_then(Run::summary);
        match (self.compensated_within(), uncompensated) {
            (Some(within), Some(u)) => lines.push(format!(
                "reading: compensated placement within +-{:.2} ms ({within} samples, the largest |E| over {} notes); uncompensated mean {:+.2} ms is the round trip the compensation removes",
                self.ms(within as f64),
                s.pattern_ms.len(),
                self.ms(u.mean)
            )),
            (None, Some(u)) => lines.push(format!(
                "reading: no --compensation: the errors are the round trip, mean {:+.1} samples ({:+.2} ms); pass --compensation {:.0} to check it",
                u.mean,
                self.ms(u.mean),
                u.mean
            )),
            _ => {}
        }
        let notes: Vec<&Note> = self.runs.iter().flat_map(|r| &r.notes).collect();
        let late: Vec<i64> = notes.iter().map(|n| n.schedule_error()).collect();
        if let Some(m) = summarize_signed(&late) {
            lines.push(format!(
                "sends against their deadlines (the MIDI output thread's wait: sleep, then yield for the last {} us): mean {:+.1} samples, max {:+} samples ({:+.2} ms); included in E",
                SPIN_NS / 1000,
                m.mean,
                m.max,
                self.ms(m.max as f64)
            ));
        }
        lines.extend(rt::detector_lines(self.rate, self.envelope_window, &notes.iter().map(|n| n.uncertainty()).collect::<Vec<_>>()));
        lines.extend(rt::fit_lines(self.rate, &notes.iter().map(|n| n.fit).collect::<Vec<_>>(), b.max_fit_error_ns));
        for run in &self.runs {
            for (i, n) in run.notes.iter().enumerate() {
                let what = format!("{} note {}", self.run_label(run), i + 1);
                if self.ms(n.uncertainty() as f64) > rt::SLOW_ATTACK_MS {
                    lines.push(format!(
                        "warning: {what}'s onset is {:.2} ms after its threshold crossing: a slow attack; use the synth's fastest attack",
                        self.ms(n.uncertainty() as f64)
                    ));
                }
                if fit_drift(&n.fit, b.max_fit_error_ns) {
                    lines.push(format!(
                        "warning: {what}: the clock fit's leave-last-out error is {:+.2} ms, above {:.2} ms: the driver's dispatch delay may have changed; E may carry the change",
                        n.fit.leave_last_out_ns / 1e6,
                        b.max_fit_error_ns as f64 / 1e6
                    ));
                }
                lines.extend(
                    rt::pre_send_warning(&what, n.pre_send, self.floor, false).map(|w| w.replace("raise --gap-ms", "raise --pattern-spacing-ms")),
                );
            }
        }
        if self.stream_errors > 0 || self.lost_blocks > 0 {
            lines.push(format!("notes: {} stream errors, {} capture blocks lost to the clock map", self.stream_errors, self.lost_blocks));
        }
        lines
    }

    /// The result as JSON, every note of every run included.
    pub fn json(&self, command: &str, date: &str, machine: &crate::report::Machine) -> serde_json::Value {
        let runs: Vec<_> = self
            .runs
            .iter()
            .map(|run| {
                let summary = run.summary().map(|m| {
                    serde_json::json!({
                        "n": m.n, "mean_samples": m.mean, "median_samples": m.median, "std_dev_samples": m.std_dev,
                        "min_samples": m.min, "max_samples": m.max, "mean_ms": self.ms(m.mean), "median_ms": self.ms(m.median),
                        "std_dev_ms": self.ms(m.std_dev), "min_ms": self.ms(m.min as f64), "max_ms": self.ms(m.max as f64),
                    })
                });
                let notes: Vec<_> = run
                    .notes
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let mut note = serde_json::json!({
                            "note": i + 1, "intended_position": n.intended, "send_target_position": n.target, "send_position": n.send,
                            "onset_position": n.onset, "threshold_crossing_position": n.crossing,
                            "error_samples": n.error(), "error_ms": self.ms(n.error() as f64),
                            "latency_samples": n.latency(), "schedule_error_samples": n.schedule_error(),
                            "detector_uncertainty_samples": n.uncertainty(), "pre_send_envelope_peak": n.pre_send,
                            "threshold": n.threshold, "attack_peak": n.peak, "send_call_ns": n.send_call_ns,
                        });
                        rt::merge(&mut note, rt::fit_json(&n.fit));
                        note["fit_drift_warning"] = serde_json::json!(fit_drift(&n.fit, self.settings.base.max_fit_error_ns));
                        note
                    })
                    .collect();
                serde_json::json!({
                    "compensation_samples": run.compensation, "compensation_ms": self.ms(run.compensation as f64),
                    "placement_error": summary, "notes": notes,
                })
            })
            .collect();
        let b = &self.settings.base;
        serde_json::json!({
            "measurement": "MIDI placement against the grid",
            "timing_reference": crate::fit::TIMING_REFERENCE,
            "error": "onset position minus intended position, in device samples",
            "command": command, "date": date, "cpu": machine.cpu, "os": machine.os,
            "device": self.device, "midi_port": self.midi_port,
            "midi_channel": b.channel + 1, "note": b.note, "velocity": b.velocity,
            "inputs": b.returns.iter().map(|c| c + 1).collect::<Vec<_>>(),
            "rate": self.rate, "buffer": self.buffer, "driver_input_latency_samples": self.input_latency,
            "pattern_ms": self.settings.pattern_ms, "lead_ms": self.settings.lead_ms, "hold_ms": self.settings.hold_ms(),
            "gap_ms": b.gap_ms, "max_ms": b.max_ms, "warmup_note": b.warmup,
            "compensation_samples": self.settings.compensation.map(|c| c.samples(self.rate)),
            "compensated_within_samples": self.compensated_within(),
            "compensated_within_ms": self.compensated_within().map(|s| self.ms(s as f64)),
            "noise_floor_envelope_peak": self.floor, "reference_floor_envelope_peak": self.reference_floor,
            "envelope_window_samples": self.envelope_window,
            "max_fit_error_ns": b.max_fit_error_ns,
            "fit_scatter_note": "clock-fit scatter, not a bound on the send offset",
            "runs": runs,
            "stream_errors": self.stream_errors, "lost_blocks": self.lost_blocks,
        })
    }
}

/// Open `request`'s device duplex through `backend`, then play the pattern into `midi`,
/// uncompensated and, with a compensation, compensated, and find each note's onset on the return.
/// The note is released on every path; a stop requested through `interrupt` ends the run.
pub fn measure(
    backend: &dyn ab::Backend,
    request: &Request,
    settings: &Settings,
    midi: &mut dyn MidiSink,
    driver: &mut dyn Driver,
    interrupt: &Interrupt,
) -> Result<Report, String> {
    settings.check(request.rate)?;
    let span = u64::from(settings.pattern_ms.last().copied().unwrap_or(0));
    let window_ms = u64::from(settings.lead_ms) + span + u64::from(settings.base.max_ms) + u64::from(onset::PEAK_MS) + u64::from(FLOOR_MS);
    let ring_ms = u64::from(RING_SECONDS) * 1000 + window_ms;
    let streams = Arc::new(Streams::new(request.rate, &settings.base.returns, ring_ms));
    let highest = settings.base.returns.iter().copied().max().unwrap_or(0);
    let request = Request { loopback: true, in_ch: highest.saturating_add(1), out_ch: 1, ..request.clone() };
    let midi_port = midi.name();
    let mut outcome = None;
    let opened = session::run_with(backend, &request, &streams, &mut || {
        outcome = Some(runs(&streams, settings, window_ms, &midi_port, &mut *midi, &mut *driver, interrupt)?);
        Ok(())
    })?;
    let (floor, reference_floor, runs, lost_blocks) = outcome.ok_or("the measurement did not run")?;
    let rate = opened.render_rate;
    Ok(Report {
        device: opened.device,
        midi_port,
        settings: settings.clone(),
        rate,
        buffer: opened.render.period.frames(),
        input_latency: opened.capture.and_then(|c| c.latency).map(|d| (d.as_secs_f64() * f64::from(rate)).round() as u64),
        floor,
        reference_floor,
        runs,
        envelope_window: onset::window(rate),
        stream_errors: streams.errors.load(Ordering::Relaxed),
        lost_blocks,
    })
}

/// Wait for the host time of position `target` by the freshest clock fit, as the MIDI output
/// thread waits for a deadline: sleep (at most a poll at a time) until [`SPIN_NS`] before it, then
/// yield until it has come.
fn wait_for(driver: &mut dyn Driver, started: &mut Started, streams: &Streams, target: u64) -> Result<(), String> {
    loop {
        started.clock.drain(streams)?;
        let deadline = started.clock.frame_to_host(target)?;
        let now = driver.now_ns();
        if now >= deadline {
            return Ok(());
        }
        let left = deadline - now;
        driver.wait(if left > SPIN_NS { (left - SPIN_NS).min(POLL_NS) } else { left })?;
    }
}

/// The runs, on the control thread while the streams run.
fn runs(
    streams: &Streams,
    settings: &Settings,
    window_ms: u64,
    port: &str,
    midi: &mut dyn MidiSink,
    driver: &mut dyn Driver,
    interrupt: &Interrupt,
) -> Result<(f32, f32, Vec<Run>, u64), String> {
    let b = &settings.base;
    // First, so that every exit below releases the note.
    let mut notes = Notes { midi, interrupt, channel: b.channel, note: b.note };
    let mut started = rt::start(streams, b, window_ms, driver, &mut notes)?;
    let (rate, w) = (started.rate, started.w);
    settings.check(rate)?;
    let peak_len = usize::try_from(frames_of(rate, u64::from(onset::PEAK_MS))).unwrap_or(usize::MAX);
    let max_frames = frames_of(rate, u64::from(b.max_ms));
    let gap_ns = u64::from(b.gap_ms) * 1_000_000;
    let hold_ns = u64::from(settings.hold_ms()) * 1_000_000;
    let pre_frames = frames_of(rate, u64::from(FLOOR_MS).min(u64::from(settings.min_spacing_ms()) / 4)).max(w as u64);
    let lead = frames_of(rate, u64::from(settings.lead_ms));
    let mut floor = started.reference.peak;
    let compensations: Vec<i64> = std::iter::once(0).chain(settings.compensation.map(|c| c.samples(rate))).collect();
    // What each run captured: (compensation, per note (intended, target, send, call ns, fit), region start, region).
    let mut captured = Vec::new();
    for (r, &compensation) in compensations.iter().enumerate() {
        let name = if compensation == 0 { "uncompensated run" } else { "compensated run" };
        let fail = |e: String| format!("{name}: {e}");
        if r > 0 || b.warmup {
            pass(driver, &mut started.clock, streams, gap_ns)?;
        }
        started.clock.drain(streams)?;
        let mut error = started.clock.position_at(driver.now_ns()).map_err(fail)?.1.error_ns;
        if error > b.max_fit_error_ns as f64 {
            eprintln!("{name}: the clock fit's standard error is {:.3} ms; waiting another gap", error / 1e6);
            pass(driver, &mut started.clock, streams, gap_ns)?;
            error = started.clock.position_at(driver.now_ns()).map_err(fail)?.1.error_ns;
            if error > b.max_fit_error_ns as f64 {
                return Err(fail(format!(
                    "the position clock's standard error ({:.3} ms) is above --max-fit-error-ms {:.2}; the driver's arrival times are irregular",
                    error / 1e6,
                    b.max_fit_error_ns as f64 / 1e6
                )));
            }
        }
        rt::pre_send(streams, &mut started, &mut floor, Some(gap_ns), driver, name).map_err(fail)?;
        started.clock.drain(streams)?;
        let (now, _) = started.clock.position_at(driver.now_ns()).map_err(fail)?;
        let first = now + lead;
        let mut sends = Vec::new();
        for (i, &at) in settings.pattern_ms.iter().enumerate() {
            let intended = first + frames_of(rate, u64::from(at));
            let target = u64::try_from(intended as i64 - compensation).map_err(|_| fail("the send position is negative".into()))?;
            wait_for(driver, &mut started, streams, target)?;
            let sent = driver.now_ns();
            notes.on(b.velocity).map_err(|e| fail(format!("note {}: {e}", i + 1)))?;
            let call = driver.now_ns().saturating_sub(sent);
            let (send, fit) = started.clock.position_at(sent).map_err(fail)?;
            sends.push((intended, target, send, call, fit));
            pass(driver, &mut started.clock, streams, hold_ns)?;
            notes.off().map_err(|e| fail(format!("note {}: {e}", i + 1)))?;
        }
        // The last onset's window, then the whole run from the ring.
        let last = sends.last().map_or(first, |s| s.2);
        let end = last + max_frames + peak_len as u64;
        let deadline = driver.now_ns().saturating_add((u64::from(b.max_ms) + STALL_MS) * 1_000_000);
        while streams.captured_end.load(Ordering::Acquire) < end {
            if driver.now_ns() > deadline {
                return Err(fail("the input stopped delivering samples".into()));
            }
            pass(driver, &mut started.clock, streams, POLL_NS)?;
        }
        let from = sends.first().map_or(first, |s| s.2).saturating_sub(pre_frames + w as u64);
        let region = streams.read(from, end).map_err(fail)?;
        captured.push((compensation, sends, from, region));
    }
    drop(notes);
    if interrupt.release_failed() {
        return Err("releasing the note failed: check that the synth is silent".into());
    }
    // Every note's pre-send window lowers the floor before any onset is found against it.
    let index = |from: u64, p: u64| (p - from) as usize;
    for (_, sends, from, region) in &captured {
        for s in sends {
            let at = index(*from, s.2);
            let pre = onset::noise_floor(region.get(at.saturating_sub(pre_frames as usize)..at).unwrap_or(&[]), w);
            floor = floor.min(pre.peak);
        }
    }
    let mut out = Vec::new();
    for (compensation, sends, from, region) in captured {
        let name = if compensation == 0 { "uncompensated run" } else { "compensated run" };
        let mut notes_out = Vec::new();
        for (i, &(intended, target, send, send_call_ns, fit)) in sends.iter().enumerate() {
            let fail = |e: String| format!("{name}, note {}: {e}", i + 1);
            let at = index(from, send);
            let pre = onset::noise_floor(region.get(at.saturating_sub(pre_frames as usize)..at).unwrap_or(&[]), w);
            onset::not_sounding(pre).map_err(fail)?;
            let (threshold, guard) = rt::levels(floor, pre.peak).map_err(fail)?;
            let next = sends.get(i + 1).map_or(region.len(), |n| index(from, n.2));
            let stop = (at + max_frames as usize).min(next).min(region.len());
            let Some(crossing) = onset::first_crossing(region.get(..stop).unwrap_or(&[]), at, w, threshold) else {
                let heard = region.get(at..stop).map_or(0.0, |s| onset::noise_floor(s, w).peak);
                return Err(fail(format!(
                    "no onset within {} ms of the note-on (or before the next note): the return peaked at {} against a threshold of {} (floor {}). Check that the synth plays on MIDI channel {} from \"{port}\", that its audio reaches input {}, and its level",
                    b.max_ms,
                    onset::dbfs(heard),
                    onset::dbfs(threshold),
                    onset::dbfs(floor),
                    b.channel + 1,
                    b.returns_label()
                )));
            };
            let found = onset::refine(&region, at, crossing, w, peak_len, guard);
            notes_out.push(Note {
                intended,
                target,
                send,
                onset: from + found.at as u64,
                crossing: from + found.crossing as u64,
                pre_send: pre.peak,
                threshold,
                peak: found.peak,
                send_call_ns,
                fit,
            });
        }
        out.push(Run { compensation, notes: notes_out });
    }
    Ok((floor, started.reference.peak, out, started.clock.lost))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_compensation_is_samples_or_milliseconds() {
        assert_eq!(Compensation::parse("1070").unwrap(), Compensation::Samples(1070));
        assert_eq!(Compensation::parse("-12").unwrap(), Compensation::Samples(-12));
        assert_eq!(Compensation::parse("955samples").unwrap(), Compensation::Samples(955));
        assert_eq!(Compensation::parse("24.3ms").unwrap(), Compensation::Ms(24.3));
        assert_eq!(Compensation::Ms(24.3).samples(44_100), 1072);
        assert!(Compensation::parse("fast").unwrap_err().contains("--compensation"));
        assert!(Compensation::parse("infms").is_err());
    }

    #[test]
    fn the_lead_bounds_the_compensation_both_ways() {
        let mut s = Settings::new(0, vec![0]);
        assert_eq!(s.pattern_ms, vec![0, 500, 1000, 1500, 2000, 2500, 3000, 3500]);
        assert_eq!(s.hold_ms(), 100);
        s.compensation = Some(Compensation::Samples(11_999));
        s.check(48_000).unwrap();
        s.compensation = Some(Compensation::Samples(12_000));
        assert!(s.check(48_000).unwrap_err().contains("raise --lead-ms"));
        s.compensation = Some(Compensation::Ms(-300.0));
        let refused = s.check(48_000).unwrap_err();
        assert!(refused.contains("negative by more than the lead (250 ms)"), "{refused}");
        s.compensation = None;
        s.pattern_ms = vec![0, 40];
        assert!(s.check(48_000).unwrap_err().contains("at least 50 ms apart"));
        s.pattern_ms = vec![100, 0];
        assert!(s.check(48_000).is_err());
    }

    #[test]
    fn a_dispatch_step_trips_the_fit_drift_warning_and_steady_arrivals_do_not() {
        let at = |k: u64| 1_000_000_000 + k * 64 * 1_000_000_000 / 48_000;
        let send_fit = |step_ns: u64| {
            let mut pf = crate::fit::PositionFit::new(48_000);
            for k in 0..400u64 {
                pf.push(k * 64, 64, at(k) + if k == 399 { step_ns } else { 0 });
            }
            let line = pf.line().unwrap();
            SendFit {
                scatter_ns: line.scatter_ns,
                sigma_ns: line.sigma_ns,
                error_ns: 0.0,
                leave_last_out_ns: line.leave_last_out_ns,
                points: line.n,
                app_offset: None,
            }
        };
        assert!(fit_drift(&send_fit(2_000_000), 500_000), "{:?}", send_fit(2_000_000));
        assert!(!fit_drift(&send_fit(0), 500_000), "{:?}", send_fit(0));
    }

    #[test]
    fn a_signed_summary() {
        let m = summarize_signed(&[-2, 1, 0, 3]).unwrap();
        assert_eq!((m.n, m.min, m.max, m.median), (4, -2, 3, 0.5));
        assert!((m.mean - 0.5).abs() < 1e-9);
        assert!(summarize_signed(&[]).is_none());
    }
}
