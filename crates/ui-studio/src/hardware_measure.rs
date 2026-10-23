//! `io.measure_hardware_latency` and its result (ADR 0015 section 5, slice d part 2). The engine
//! checks the request (known paths, stopped transport) and keeps the table; the live Player plays
//! the click and records the return, so the app runs the command itself. Over the command channel
//! the call waits for the measurement, about two and a half seconds, because a command answers when
//! it is done. The Audio Devices window starts the same measurement without waiting
//! ([`StudioApp::start_measure`]): it shows the clicks sent so far and a per-frame step collects
//! the result ([`StudioApp::step_measure`]). Both store the result through one function.
//!
//! The Calibration line's Measure over native ASIO (#20, known issue K31) runs through the same
//! flow ([`MeasureKind::Loop`]): the Player's loop probe times the click by sample position, and
//! the result is stored as the driver's loopback calibration under the open key and handed to the
//! running streams, so recordings are placed from it at once.

use crate::StudioApp;
use serde_json::{Value, json};
use soundcraft_engine::EngineError;
use soundcraft_playback::hardware_probe::{Measured, ProbeError};
use std::time::{Duration, Instant};

/// How long to wait for the audio thread to finish a measurement (it needs about 2.3 s of audio).
const WAIT: Duration = Duration::from_secs(8);

/// The refusal of a transport start while the window's measurement runs.
pub(crate) const REFUSED_TRANSPORT: &str = "Stop the hardware insert measurement first";

/// Why a measurement ended early when the audio device was re-opened under it.
pub(crate) const CANCELLED_REOPEN: &str = "Measurement cancelled: the audio device was re-opened";

fn message(error: &EngineError) -> String {
    match error {
        EngineError::BadParams(_, why) | EngineError::Disabled(_, why) => why.clone(),
        other => other.to_string(),
    }
}

/// What a measurement times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MeasureKind {
    /// A hardware insert's pair, in the plan's frames (the delay compensation's view).
    #[default]
    Insert,
    /// The open native ASIO driver's loop, in sample positions: its loopback calibration.
    Loop,
}

/// How long a loop measurement waits for a sample position before it says the driver gives none.
const POSITION_GRACE: Duration = Duration::from_secs(1);

/// A measurement the window started and has not collected yet.
#[derive(Debug, Clone)]
pub struct Pending {
    send: String,
    ret: String,
    rate: u32,
    began: Instant,
    pub(crate) kind: MeasureKind,
}

/// A stand-in for the Player's probe, driven by tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeProbe {
    pub progress: (u32, u32),
    pub result: Option<Result<Measured, ProbeError>>,
    pub started: Vec<(Vec<u16>, Vec<u16>)>,
    pub cancelled: usize,
    /// The key a loop measurement stores under (the open Player's), `None` for none.
    pub key: Option<soundcraft_playback::devices::CalibrationKey>,
    /// The loop the driver's figures give, `None` for none.
    pub driver_loop: Option<u32>,
}

impl StudioApp {
    /// The command channel's call: waits for the measurement and answers with the result.
    pub(crate) fn measure_hardware_latency(&mut self, p: &Value) -> Result<Value, String> {
        let name = |keys: &[&str]| keys.iter().find_map(|k| p.get(k).and_then(Value::as_str)).map(str::trim).filter(|n| !n.is_empty());
        let send = name(&["send"]).ok_or("`send` (an output path) required")?.to_string();
        let ret = name(&["ret", "return"]).ok_or("`ret` (an input path) required")?.to_string();
        let result = self.run_hardware_probe(&send, &ret);
        if let Err(why) = &result {
            self.status.clone_from(why);
        }
        result
    }

    fn run_hardware_probe(&mut self, send: &str, ret: &str) -> Result<Value, String> {
        let pending = self.begin_measure(send, ret, MeasureKind::Insert)?;
        loop {
            if let Some(done) = self.poll_measure(&pending)? {
                return self.finish_measure(&pending, done);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The window's Measure: start the measurement and return at once. Step it with
    /// [`Self::step_measure`] every frame. Errors say why it did not start.
    pub(crate) fn start_measure(&mut self, send: &str, ret: &str) -> Result<(), String> {
        let pending = self.begin_measure(send, ret, MeasureKind::Insert)?;
        self.audio_devices.measuring = Some(pending);
        Ok(())
    }

    /// The Calibration line's Measure over native ASIO: time the driver's loop from output path
    /// `send` to input path `ret` and return at once (collected like [`Self::start_measure`]).
    /// The pair is remembered as the next Measure's default.
    pub(crate) fn start_loop_measure(&mut self, send: &str, ret: &str) -> Result<(), String> {
        let pending = self.begin_measure(send, ret, MeasureKind::Loop)?;
        self.audio_devices.measuring = Some(pending);
        for cfg in [&mut self.ui.audio, &mut self.audio_devices.draft] {
            cfg.calibration_send = Some(send.to_owned());
            cfg.calibration_return = Some(ret.to_owned());
        }
        self.audio_devices.adopt_audio(&self.ui.audio);
        Ok(())
    }

    /// The kind of the measurement running, if one is.
    pub(crate) fn measuring_kind(&self) -> Option<MeasureKind> {
        self.audio_devices.measuring.as_ref().map(|p| p.kind)
    }

    /// Clicks sent and in all, while the window's measurement runs.
    pub(crate) fn measure_progress(&self) -> Option<(u32, u32)> {
        self.audio_devices.measuring.as_ref()?;
        #[cfg(test)]
        if let Some(f) = &self.audio_devices.fake_probe {
            return Some(f.progress);
        }
        let loop_kind = self.measuring_kind() == Some(MeasureKind::Loop);
        Some(self.player.as_ref().map_or((0, 0), |p| if loop_kind { p.loop_calibration_progress() } else { p.hardware_probe_progress() }))
    }

    /// Per frame: collect the window's measurement when it is done. Stores the result like the
    /// command does and says it in the window and the status line.
    pub(crate) fn step_measure(&mut self) {
        let Some(pending) = self.audio_devices.measuring.clone() else { return };
        let said = match self.poll_measure(&pending) {
            Ok(None) => return,
            Ok(Some(done)) => self.finish_measure(&pending, done).map(|v| {
                let n = |k: &str| v.get(k).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                match pending.kind {
                    MeasureKind::Insert => crate::audio_devices_io::measured_text(n("round_trip_frames"), n("spread_frames"), pending.rate),
                    MeasureKind::Loop => v.get("said").and_then(Value::as_str).unwrap_or_default().to_owned(),
                }
            }),
            Err(why) => Err(why),
        };
        self.audio_devices.measuring = None;
        self.audio_devices.measure_note = Some((
            pending.kind,
            said.unwrap_or_else(|why| {
                self.status.clone_from(&why);
                why
            }),
        ));
    }

    /// Stop the window's measurement (Cancel, the window closing) and leave `say` in the window.
    /// Nothing is stored.
    pub(crate) fn cancel_measure(&mut self, say: &str) {
        let Some(pending) = self.audio_devices.measuring.take() else { return };
        self.cancel_probe();
        self.audio_devices.measure_note = Some((pending.kind, say.into()));
    }

    /// Check the request, make the plan know the table and start the click train.
    fn begin_measure(&mut self, send: &str, ret: &str, kind: MeasureKind) -> Result<Pending, String> {
        if self.audio_devices.measuring.is_some() {
            return Err("a measurement is already running".into());
        }
        let (out, back) = self.engine.hardware_measurement(send, ret).map_err(|e| message(&e))?;
        // The plan must know the table (and so the pair's sinks) as it is now.
        self.sync_io();
        // An insert counts the session's frames; a loop the device's.
        let rate = match kind {
            MeasureKind::Insert => self.engine.session().sample_rate.hz(),
            MeasureKind::Loop => self.player.as_ref().map_or(self.engine.session().sample_rate.hz(), |p| p.device_rate),
        };
        let pending = Pending { send: send.into(), ret: ret.into(), rate, began: Instant::now(), kind };
        #[cfg(test)]
        if let Some(f) = self.audio_devices.fake_probe.as_mut() {
            f.started.push((out, back));
            return Ok(pending);
        }
        let player = self.player.as_ref().filter(|p| !p.silent).ok_or("measuring needs a live audio device")?;
        if player.is_playing() {
            return Err("stop the transport before measuring".into());
        }
        match kind {
            MeasureKind::Insert => player.start_hardware_probe(&out, &back)?,
            MeasureKind::Loop => player.start_loop_calibration(&out, &back)?,
        }
        Ok(pending)
    }

    /// `Ok(None)` while it runs; the outcome once done; an error when the device went away or did
    /// not answer.
    fn poll_measure(&mut self, pending: &Pending) -> Result<Option<Result<Measured, ProbeError>>, String> {
        #[cfg(test)]
        if let Some(f) = self.audio_devices.fake_probe.as_mut() {
            return Ok(f.result.take());
        }
        let Some(player) = self.player.as_ref() else { return Err("the audio device went away while measuring".into()) };
        let done = match pending.kind {
            MeasureKind::Insert => player.hardware_probe_result(),
            MeasureKind::Loop => player.loop_calibration_result(),
        };
        if let Some(done) = done {
            return Ok(Some(done));
        }
        if pending.kind == MeasureKind::Loop && player.loop_calibration_lacks_position() && pending.began.elapsed() > POSITION_GRACE {
            player.cancel_loop_calibration();
            return Err("the driver reports no sample position, so its loop cannot be measured: the driver's own figures stay in use".into());
        }
        if pending.began.elapsed() > WAIT {
            player.cancel_hardware_probe();
            player.cancel_loop_calibration();
            return Err("the audio device did not answer: no measurement".into());
        }
        Ok(None)
    }

    fn cancel_probe(&mut self) {
        #[cfg(test)]
        if let Some(f) = self.audio_devices.fake_probe.as_mut() {
            f.cancelled += 1;
            return;
        }
        if let Some(player) = &self.player {
            player.cancel_hardware_probe();
            player.cancel_loop_calibration();
        }
    }

    /// Store a finished measurement in the table and say it: the one place both paths end.
    fn finish_measure(&mut self, pending: &Pending, outcome: Result<Measured, ProbeError>) -> Result<Value, String> {
        if pending.kind == MeasureKind::Loop {
            return self.finish_loop(pending, outcome);
        }
        let (send, ret, rate) = (&pending.send, &pending.ret, pending.rate);
        match outcome {
            Ok(m) => {
                self.engine.store_hardware_latency(send, ret, m.round_trip_frames);
                self.sync_io();
                let ms = f64::from(m.round_trip_frames) * 1000.0 / f64::from(rate.max(1));
                self.status = format!("{send} \u{2192} {ret}: {} frames ({ms:.1} ms)", m.round_trip_frames);
                Ok(json!({"round_trip_frames": m.round_trip_frames, "spread_frames": m.spread_frames, "ms": ms}))
            }
            Err(ProbeError::NoSignal { .. }) => Err(format!("no signal returned on {ret}: check the cable and input gain")),
        }
    }

    /// A finished loop measurement: kept as the driver's calibration under the open key (in
    /// `ui.audio`, saved with the preferences), handed to the running streams, and said.
    fn finish_loop(&mut self, pending: &Pending, outcome: Result<Measured, ProbeError>) -> Result<Value, String> {
        let measured = match outcome {
            Ok(m) => m,
            Err(ProbeError::NoSignal { found, sent }) => {
                return Err(format!(
                    "the click came back on {found} of {sent} tries: check the cable from {} to {} and the input gain",
                    pending.send, pending.ret
                ));
            }
        };
        let key = self.loop_key().ok_or("the audio device changed while measuring: nothing stored")?;
        let record = key.record(measured.round_trip_frames, measured.spread_frames.saturating_add(1), &soundcraft_playback::devices::utc_today());
        let mut said = crate::audio_devices::calibrated_text(&record);
        // The driver's own figures beside it: a loop more than a buffer away from them is more
        // likely a wrong cable or path, or a loop that came up in another phase, than the driver.
        let driver = self.driver_loop_frames();
        let mut warning = None;
        if let Some(driver) = driver {
            let difference = i64::from(record.round_trip_frames) - i64::from(driver);
            said.push_str(&format!("; driver figures {driver} frames ({difference:+})"));
            if difference.unsigned_abs() > u64::from(key.period.max(1)) {
                warning = Some(format!(
                    "the measured loop is {} frames from the driver's figures, more than a buffer: check the cable and the paths, and Measure again",
                    difference.unsigned_abs()
                ));
            }
        }
        let calibration = record.to_backend();
        for cfg in [&mut self.ui.audio, &mut self.audio_devices.draft] {
            cfg.store_calibration(record.clone());
        }
        self.audio_devices.adopt_audio(&self.ui.audio);
        let live = self.player.as_mut().is_some_and(|p| p.apply_calibration(calibration));
        self.status = if live { format!("Calibrated: {said}") } else { format!("Calibrated: {said}; it applies when the device is next opened") };
        if let Some(warning) = &warning {
            said = format!("{said}. Warning: {warning}");
            self.status = format!("{}. Warning: {warning}", self.status);
        }
        self.save_audio_preferences();
        Ok(json!({"round_trip_frames": record.round_trip_frames, "spread_frames": record.spread_frames, "said": said, "live": live}))
    }

    /// The loop the open driver's own figures give, in frames (`None` without them).
    fn driver_loop_frames(&self) -> Option<u32> {
        #[cfg(test)]
        if let Some(f) = &self.audio_devices.fake_probe {
            return f.driver_loop;
        }
        self.player.as_ref().filter(|p| !p.silent)?.driver_loop_frames()
    }

    /// The key a loop measurement of the open device stores under.
    fn loop_key(&self) -> Option<soundcraft_playback::devices::CalibrationKey> {
        #[cfg(test)]
        if let Some(f) = &self.audio_devices.fake_probe {
            return f.key.clone();
        }
        self.player.as_ref().filter(|p| !p.silent)?.calibration_key.clone()
    }
}
