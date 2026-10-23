//! Loopback calibration of the open device over native ASIO (#20, known issue K31): the Player's
//! side of the Calibration line's Measure. A click plays on an output path and is found again on
//! an input path by sample position ([`crate::loop_probe`]); the app stores the result under
//! [`Player::calibration_key`] and hands it to the running streams ([`Player::apply_calibration`]).

use super::Player;
use crate::devices;
use crate::hardware_probe::{Measured, ProbeError};
use transit_audio_backend as ab;

impl Player {
    /// Start measuring the loop of the open native ASIO driver: a click train plays on the device
    /// channels `send` (0-based) and the capture records `ret` for about two seconds and a third.
    /// Poll [`Self::loop_calibration_result`]. Refuses when the device is not a duplex ASIO
    /// driver, the transport is playing, a channel is not open, or a measurement (this one or a
    /// hardware insert's) is running. Control side only; the callbacks allocate nothing.
    pub fn start_loop_calibration(&self, send: &[u16], ret: &[u16]) -> Result<(), String> {
        let probe = self.loop_probe.as_ref().filter(|_| !self.silent).ok_or("there is no audio device to measure through")?;
        if !self.device_system.eq_ignore_ascii_case(devices::ASIO_SYSTEM) || !self.native_backend {
            return Err("measuring the loop needs the native ASIO backend".into());
        }
        if !self.holds_input() || self.calibration_key.is_none() {
            return Err("the driver's input is not open with its output: no loop to measure".into());
        }
        if self.is_playing() {
            return Err("stop the transport before measuring".into());
        }
        if self.hardware_probe_running() {
            return Err("a measurement is already running".into());
        }
        let inputs = self.with_held_input(|recorder| recorder.channels).unwrap_or(0);
        if send.iter().any(|c| *c >= self.device_channels) || ret.iter().any(|c| usize::from(*c) >= inputs) {
            return Err("a path names a channel the device did not open".into());
        }
        if !probe.arm(send, ret) {
            return Err("a measurement is already running".into());
        }
        Ok(())
    }

    /// The loop measured by [`Self::start_loop_calibration`] once it is done, in device frames
    /// (`None` while it runs, and after it has been taken).
    pub fn loop_calibration_result(&self) -> Option<Result<Measured, ProbeError>> {
        self.loop_probe.as_ref()?.take_result()
    }

    /// Whether a loop measurement is running.
    pub fn loop_calibration_running(&self) -> bool {
        self.loop_probe.as_ref().is_some_and(|p| p.armed())
    }

    /// Clicks sent so far and in all, for a running loop measurement.
    pub fn loop_calibration_progress(&self) -> (u32, u32) {
        self.loop_probe.as_ref().map_or((0, 0), |p| p.progress())
    }

    /// Whether the driver delivered a block without a sample position during the measurement (it
    /// then never finishes: the caller says why).
    pub fn loop_calibration_lacks_position(&self) -> bool {
        self.loop_probe.as_ref().is_some_and(|p| p.lacks_position())
    }

    /// Stop a loop measurement in progress; nothing is kept.
    pub fn cancel_loop_calibration(&self) {
        if let Some(p) = &self.loop_probe {
            p.cancel();
        }
    }

    /// The record of `measured` under the open pair's key, measured today: the median loop, and
    /// its spread plus one frame (positions are whole frames) as the bound. `None` without a key.
    pub fn calibration_record(&self, measured: &Measured) -> Option<devices::LoopbackCalibration> {
        let key = self.calibration_key.as_ref()?;
        Some(key.record(measured.round_trip_frames, measured.spread_frames.saturating_add(1), &devices::utc_today()))
    }

    /// The loop the driver's own figures give (`getLatencies` output plus input, in frames at the
    /// device rate), for comparing with a measured one; `None` where the backend reports none.
    pub fn driver_loop_frames(&self) -> Option<u32> {
        let input = self.with_held_input(|recorder| recorder.latency).flatten()?;
        let total = self.output_latency? + input;
        u32::try_from((total.as_secs_f64() * f64::from(self.device_rate)).round() as u64).ok()
    }

    /// Hand `calibration` to the open output stream and the held input, so the stamps (and the
    /// placement of recordings) use it from the next callback on. False when a stream could not
    /// take it while running: it then applies at the next open.
    pub fn apply_calibration(&mut self, calibration: Option<ab::Calibration>) -> bool {
        let output = self.stream.as_ref().is_some_and(|stream| stream.set_calibration(calibration));
        let input = self.with_held_input(|recorder| recorder.set_calibration(calibration)).unwrap_or(false);
        if output && input {
            self.calibration = calibration;
        }
        output && input
    }
}
