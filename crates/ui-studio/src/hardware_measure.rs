//! `io.measure_hardware_latency` and its result (ADR 0015 section 5, slice d part 2). The engine
//! checks the request (known paths, stopped transport) and keeps the table; the live Player plays
//! the click and records the return, so the app runs the command itself. Over the command channel
//! the call waits for the measurement, about two and a half seconds, because a command answers when
//! it is done. The Audio Devices window starts the same measurement without waiting
//! ([`StudioApp::start_measure`]): it shows the clicks sent so far and a per-frame step collects
//! the result ([`StudioApp::step_measure`]). Both store the result through one function.

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

/// A measurement the window started and has not collected yet.
#[derive(Debug, Clone)]
pub struct Pending {
    send: String,
    ret: String,
    rate: u32,
    began: Instant,
}

/// A stand-in for the Player's probe, driven by tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeProbe {
    pub progress: (u32, u32),
    pub result: Option<Result<Measured, ProbeError>>,
    pub started: Vec<(Vec<u16>, Vec<u16>)>,
    pub cancelled: usize,
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
        let pending = self.begin_measure(send, ret)?;
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
        let pending = self.begin_measure(send, ret)?;
        self.audio_devices.measuring = Some(pending);
        Ok(())
    }

    /// Clicks sent and in all, while the window's measurement runs.
    pub(crate) fn measure_progress(&self) -> Option<(u32, u32)> {
        self.audio_devices.measuring.as_ref()?;
        #[cfg(test)]
        if let Some(f) = &self.audio_devices.fake_probe {
            return Some(f.progress);
        }
        Some(self.player.as_ref().map_or((0, 0), |p| p.hardware_probe_progress()))
    }

    /// Per frame: collect the window's measurement when it is done. Stores the result like the
    /// command does and says it in the window and the status line.
    pub(crate) fn step_measure(&mut self) {
        let Some(pending) = self.audio_devices.measuring.clone() else { return };
        let said = match self.poll_measure(&pending) {
            Ok(None) => return,
            Ok(Some(done)) => self.finish_measure(&pending, done).map(|v| {
                let n = |k: &str| v.get(k).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                crate::audio_devices_io::measured_text(n("round_trip_frames"), n("spread_frames"), pending.rate)
            }),
            Err(why) => Err(why),
        };
        self.audio_devices.measuring = None;
        self.audio_devices.measure_note = Some(said.unwrap_or_else(|why| {
            self.status.clone_from(&why);
            why
        }));
    }

    /// Stop the window's measurement (Cancel, the window closing) and leave `say` in the window.
    /// Nothing is stored.
    pub(crate) fn cancel_measure(&mut self, say: &str) {
        if self.audio_devices.measuring.take().is_none() {
            return;
        }
        self.cancel_probe();
        self.audio_devices.measure_note = Some(say.into());
    }

    /// Check the request, make the plan know the table and start the click train.
    fn begin_measure(&mut self, send: &str, ret: &str) -> Result<Pending, String> {
        if self.audio_devices.measuring.is_some() {
            return Err("a measurement is already running".into());
        }
        let (out, back) = self.engine.hardware_measurement(send, ret).map_err(|e| message(&e))?;
        // The plan must know the table (and so the pair's sinks) as it is now.
        self.sync_io();
        let rate = self.engine.session().sample_rate.hz();
        let pending = Pending { send: send.into(), ret: ret.into(), rate, began: Instant::now() };
        #[cfg(test)]
        if let Some(f) = self.audio_devices.fake_probe.as_mut() {
            f.started.push((out, back));
            return Ok(pending);
        }
        let player = self.player.as_ref().filter(|p| !p.silent).ok_or("measuring needs a live audio device")?;
        if player.is_playing() {
            return Err("stop the transport before measuring".into());
        }
        player.start_hardware_probe(&out, &back)?;
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
        if let Some(done) = player.hardware_probe_result() {
            return Ok(Some(done));
        }
        if pending.began.elapsed() > WAIT {
            player.cancel_hardware_probe();
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
        }
    }

    /// Store a finished measurement in the table and say it: the one place both paths end.
    fn finish_measure(&mut self, pending: &Pending, outcome: Result<Measured, ProbeError>) -> Result<Value, String> {
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
}
