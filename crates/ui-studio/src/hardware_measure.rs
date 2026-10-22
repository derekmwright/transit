//! `io.measure_hardware_latency` and its result (ADR 0015 section 5, slice d part 2). The engine
//! checks the request (known paths, stopped transport) and keeps the table; the live Player plays
//! the click and records the return, so the app runs the command itself. The call waits for the
//! measurement, about two and a half seconds, because a command answers when it is done.

use crate::StudioApp;
use serde_json::{Value, json};
use soundcraft_engine::EngineError;
use soundcraft_playback::hardware_probe::ProbeError;
use std::time::{Duration, Instant};

/// How long to wait for the audio thread to finish a measurement (it needs about 2.3 s of audio).
const WAIT: Duration = Duration::from_secs(8);

fn message(error: &EngineError) -> String {
    match error {
        EngineError::BadParams(_, why) | EngineError::Disabled(_, why) => why.clone(),
        other => other.to_string(),
    }
}

impl StudioApp {
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
        let (out, back) = self.engine.hardware_measurement(send, ret).map_err(|e| message(&e))?;
        // The plan must know the table (and so the pair's sinks) as it is now.
        self.sync_io();
        let rate = self.engine.session().sample_rate.hz();
        {
            let player = self.player.as_ref().filter(|p| !p.silent).ok_or("measuring needs a live audio device")?;
            if player.is_playing() {
                return Err("stop the transport before measuring".into());
            }
            player.start_hardware_probe(&out, &back)?;
        }
        let began = Instant::now();
        let outcome = loop {
            let Some(player) = self.player.as_ref() else { return Err("the audio device went away while measuring".into()) };
            if let Some(done) = player.hardware_probe_result() {
                break done;
            }
            if began.elapsed() > WAIT {
                player.cancel_hardware_probe();
                return Err("the audio device did not answer: no measurement".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
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
