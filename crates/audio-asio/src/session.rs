//! A driver's streaming session, on the host thread: the rate and buffer size both directions run
//! at, the one set of buffers `createBuffers` made for them, the installed callback state, and
//! `start`/`stop` of the whole driver as the streams of its directions start and stop.
//!
//! **The open-order rule.** ASIO has one buffer set and one callback per driver, made by one
//! `createBuffers` call for every channel of both directions. So both streams of a driver are
//! opened before either starts, in either order: the first `open_*` creates the session and its
//! buffers, a second `open_*` of the other direction (while nothing has started) disposes them and
//! creates them again for both directions. Once a stream has started, opening the other direction
//! fails with a message saying so; close both and open them again. `start` of the first stream
//! starts the driver (the other direction's callback is not called, and its output plays silence,
//! until its own `start`); the driver stops when no stream of it is running any more, and the
//! session ends (buffers disposed, callback state freed, the driver unloaded unless a
//! [`crate::Loaded`] handle holds it) when both streams are dropped.
//!
//! **MMCSS.** Each driver start arms the session's [`Mmcss`]: the first buffer switch registers its
//! thread with "Pro Audio"; before the driver's `stop` the registration is reverted (see
//! [`crate::mmcss`]).
//!
//! **The watchdog.** While the driver runs, the host thread checks between requests whether its
//! buffer switches still come ([`Session::tend`], [`crate::watchdog`]); a driver silent for longer
//! than the limit gets each open stream a `StreamError::Stalled`, once per start.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use crate::callback::{CallbackState, Installed, PlaneTable};
use crate::com::{Buffers, ChannelCount, Driver, Event, Running};
use crate::convert::Wire;
use crate::driver::{BufferSize, DriverInfo, asio_error};
use crate::engine::{DETACHED, Engine, Latencies, OPENED, STARTED, STOPPED, Side, SideShared};
use crate::mmcss::{Mmcss, Released};
use crate::stats::DriverEvents;
use crate::watchdog::Watchdog;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use transit_audio_backend as ab;

/// The callback a stream is opened with.
pub(crate) enum Callback {
    Render(ab::RenderCallback),
    Capture(ab::CaptureCallback),
}

impl Callback {
    fn direction(&self) -> ab::Direction {
        match self {
            Self::Render(_) => ab::Direction::Output,
            Self::Capture(_) => ab::Direction::Input,
        }
    }
}

/// Marks a direction detached when the stream that owns it goes, including a stream whose open
/// answer its caller never received (the host then sweeps the session).
pub(crate) struct SideOwner(pub(crate) Arc<SideShared>);

impl Drop for SideOwner {
    fn drop(&mut self) {
        self.0.state.store(DETACHED, Ordering::Release);
    }
}

/// What an open hands the stream.
pub(crate) struct Opened {
    pub(crate) owner: SideOwner,
    pub(crate) achieved: ab::Achieved,
    pub(crate) generation: u64,
    /// The smallest step of the driver's `systemTime` seen (ns; `u64::MAX` before two readings).
    pub(crate) system_step: Arc<AtomicU64>,
    /// The MMCSS registration of the buffer-switch thread (the stream's `thread_priority`).
    pub(crate) mmcss: Arc<Mmcss>,
}

enum BufferState {
    /// `createBuffers` failed on a second open; nothing can start.
    Missing,
    Prepared(Buffers),
    Running(Running),
}

/// One driver's streams.
pub(crate) struct Session {
    generation: u64,
    rate: u32,
    frames: usize,
    min_frames: Option<u32>,
    render: Option<Arc<SideShared>>,
    capture: Option<Arc<SideShared>>,
    buffers: BufferState,
    latencies: Arc<Latencies>,
    events: Arc<DriverEvents>,
    system_step: Arc<AtomicU64>,
    watchdog: Watchdog,
    mmcss: Arc<Mmcss>,
    installed: Installed,
}

/// The buffer size to request: `Period::Default` is the driver's preferred size; a requested size
/// snaps to the nearest one the driver allows (ties go to the larger). Granularity -1 means powers
/// of two between `min` and `max`, 0 one fixed size (the preferred one), a positive value steps
/// from `min` (`asio.h`, `ASIOGetBufferSize`).
pub(crate) fn negotiate(buffer: BufferSize, period: ab::Period) -> Result<u32, String> {
    let BufferSize { min, max, preferred, granularity } = buffer;
    let fallback = if preferred > 0 { preferred } else { min };
    if fallback <= 0 {
        return Err(format!("the driver reports no usable buffer size (min {min}, max {max}, preferred {preferred}, granularity {granularity})"));
    }
    let Some(want) = period.frames() else { return Ok(fallback as u32) };
    let want = i64::from(want);
    if min <= 0 || max < min || granularity == 0 || granularity < -1 {
        return Ok(fallback as u32);
    }
    let nearest = |candidates: &mut dyn Iterator<Item = i64>| -> Option<i64> { candidates.min_by_key(|c| ((c - want).abs(), std::cmp::Reverse(*c))) };
    let chosen = if granularity == -1 {
        let mut powers = (0..31).map(|shift| 1i64 << shift).filter(|p| *p >= i64::from(min) && *p <= i64::from(max));
        nearest(&mut powers)
    } else {
        let step = i64::from(granularity);
        let steps = (i64::from(max) - i64::from(min)) / step;
        let below = ((want - i64::from(min)) / step).clamp(0, steps);
        let mut around = [below, (below + 1).min(steps)].into_iter().map(|k| i64::from(min) + k * step);
        nearest(&mut around)
    };
    Ok(chosen.and_then(|c| u32::try_from(c).ok()).unwrap_or(fallback as u32))
}

/// How long `stop` and a drop wait for a callback still inside the direction they end.
const QUIESCE_LIMIT: Duration = Duration::from_secs(2);

fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn other(name: &str, text: impl std::fmt::Display) -> ab::OpenError {
    ab::OpenError::Other(format!("ASIO driver \"{name}\": {text}"))
}

/// The direction's wire, sample format and the channel count to open (the request's, at most the
/// driver's).
fn direction_format(name: &str, info: &DriverInfo, direction: ab::Direction, request: &ab::OpenRequest) -> Result<(Wire, ab::Format), ab::OpenError> {
    let available = info.channels(direction);
    if available == 0 {
        return Err(ab::OpenError::DeviceNotAvailable(format!("ASIO driver \"{name}\" has no {direction:?} channels")));
    }
    let crate::driver::DirectionType::Uniform(sample) = info.sample_type(direction) else {
        return Err(other(name, format_args!("{direction:?} sample type {} is not supported", info.sample_type(direction).describe())));
    };
    let (Some(wire), Some((format, valid_bits))) = (Wire::of(*sample), sample.format()) else {
        return Err(other(name, format_args!("{direction:?} sample type {} is not supported", sample.name())));
    };
    let channels = request.format.channels.clamp(1, available);
    Ok((wire, ab::Format { rate: request.format.rate, channels, sample: format, valid_bits, channel_mask: None }))
}

/// Sets the driver to `rate` when it runs at another one.
fn set_rate(name: &str, driver: &mut Driver, rate: u32) -> Result<(), ab::OpenError> {
    let current = driver.sample_rate().map_err(|code| asio_error(name, "getSampleRate", code))?;
    if (current - f64::from(rate)).abs() < 0.5 {
        return Ok(());
    }
    driver.can_sample_rate(f64::from(rate)).map_err(|code| asio_error(name, &format!("canSampleRate({rate})"), code))?;
    driver.set_sample_rate(f64::from(rate)).map_err(|code| asio_error(name, &format!("setSampleRate({rate})"), code))?;
    let now = driver.sample_rate().map_err(|code| asio_error(name, "getSampleRate", code))?;
    if (now - f64::from(rate)).abs() >= 0.5 {
        return Err(other(name, format_args!("asked for {rate} Hz, the driver runs at {now} Hz")));
    }
    Ok(())
}

impl Session {
    /// The first open of a driver: rate, buffer size, the callback state and the buffers.
    pub(crate) fn open(
        name: &str,
        driver: &mut Driver,
        info: &DriverInfo,
        wake: Arc<Event>,
        watchdog_floor: Duration,
        mmcss: crate::mmcss::Hooks,
        request: &ab::OpenRequest,
        callback: Callback,
        error: ab::ErrorCallback,
    ) -> Result<(Self, Opened), ab::OpenError> {
        let direction = callback.direction();
        let (wire, format) = direction_format(name, info, direction, request)?;
        set_rate(name, driver, format.rate)?;
        let buffer = driver.buffer_size().map_err(|code| asio_error(name, "getBufferSize", code))?;
        let frames = negotiate(buffer, request.period).map_err(|e| other(name, e))? as usize;
        let events = Arc::new(DriverEvents::default());
        let latencies = Arc::new(Latencies::default());
        let engine = Engine::new(frames, format.rate, Arc::clone(&events), Arc::clone(&latencies));
        let system_step = engine.system_time_step_ns();
        let mmcss = Arc::new(Mmcss::new(mmcss));
        let state = CallbackState::new(engine, driver.callback_driver(), Arc::clone(&events), wake, Arc::clone(&mmcss));
        let installed = Installed::install(state)
            .map_err(|_| ab::OpenError::InUse(format!("ASIO driver \"{name}\": another ASIO stream session is active in this process")))?;
        let mut session = Self {
            generation: next_generation(),
            rate: format.rate,
            frames,
            min_frames: u32::try_from(buffer.min).ok().filter(|m| *m > 0),
            render: None,
            capture: None,
            buffers: BufferState::Missing,
            latencies,
            events,
            system_step,
            watchdog: Watchdog::new(frames, format.rate, watchdog_floor),
            mmcss,
            installed,
        };
        let opened = session.add(name, driver, wire, format, request, callback, error)?;
        Ok((session, opened))
    }

    /// A second open on the same driver (the other direction), before anything started.
    pub(crate) fn attach(
        &mut self,
        name: &str,
        driver: &mut Driver,
        info: &DriverInfo,
        request: &ab::OpenRequest,
        callback: Callback,
        error: ab::ErrorCallback,
    ) -> Result<Opened, ab::OpenError> {
        let direction = callback.direction();
        if self.started() || matches!(self.buffers, BufferState::Running(_)) {
            return Err(other(
                name,
                format_args!(
                    "the {} stream must be opened before either stream of the driver starts (ASIO creates both directions' buffers in \
                     one call): open the capture and the render stream, then start them; close the running stream to reopen both",
                    if direction == ab::Direction::Output { "render" } else { "capture" }
                ),
            ));
        }
        let held = match direction {
            ab::Direction::Output => &self.render,
            ab::Direction::Input => &self.capture,
        };
        if held.as_ref().is_some_and(|s| s.state() != DETACHED) {
            return Err(ab::OpenError::InUse(format!("ASIO driver \"{name}\": its {direction:?} stream is already open")));
        }
        if request.format.rate != self.rate {
            return Err(other(
                name,
                format_args!("the {direction:?} stream asks for {} Hz but the driver's other stream runs at {} Hz", request.format.rate, self.rate),
            ));
        }
        let (wire, format) = direction_format(name, info, direction, request)?;
        // The period of a second open is the session's: one buffer size per driver.
        self.add(name, driver, wire, format, request, callback, error)
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        name: &str,
        driver: &mut Driver,
        wire: Wire,
        format: ab::Format,
        request: &ab::OpenRequest,
        callback: Callback,
        error: ab::ErrorCallback,
    ) -> Result<Opened, ab::OpenError> {
        let direction = callback.direction();
        let shared = Arc::new(SideShared::new(Arc::clone(&self.events)));
        let frames = self.frames;
        let calibration = request.calibration;
        // No buffer switch runs (nothing has started), so the engine is free.
        self.installed.state().with(|engine, _| match callback {
            Callback::Render(render) => {
                engine.render = Some(Side::new(Arc::clone(&shared), render, error, format, wire, calibration, frames));
            }
            Callback::Capture(capture) => {
                engine.capture = Some(Side::new(Arc::clone(&shared), capture, error, format, wire, calibration, frames));
            }
        });
        match direction {
            ab::Direction::Output => self.render = Some(Arc::clone(&shared)),
            ab::Direction::Input => self.capture = Some(Arc::clone(&shared)),
        }
        if let Err(e) = self.rebuild(name, driver) {
            self.forget(direction);
            // Leave the other direction's buffers as they were where possible.
            let _ = self.rebuild(name, driver);
            return Err(e);
        }
        let latency_frames = match direction {
            ab::Direction::Output => self.latencies.output.load(Ordering::Relaxed),
            ab::Direction::Input => self.latencies.input.load(Ordering::Relaxed),
        };
        let latency = Duration::from_secs_f64(f64::from(latency_frames) / f64::from(self.rate.max(1)));
        let frames = u32::try_from(self.frames).unwrap_or(u32::MAX);
        let achieved = ab::Achieved {
            format,
            period: ab::Period::Frames(frames),
            latency: Some(latency),
            device_position: None,
            min_period: self.min_frames,
            buffer_frames: Some(frames),
        };
        Ok(Opened {
            owner: SideOwner(shared),
            achieved,
            generation: self.generation,
            system_step: Arc::clone(&self.system_step),
            mmcss: Arc::clone(&self.mmcss),
        })
    }

    /// Drops a direction that failed to open from the engine and the session.
    fn forget(&mut self, direction: ab::Direction) {
        match direction {
            ab::Direction::Output => self.render = None,
            ab::Direction::Input => self.capture = None,
        }
        self.installed.state().with(|engine, _| match direction {
            ab::Direction::Output => engine.render = None,
            ab::Direction::Input => engine.capture = None,
        });
    }

    /// (Re)creates the buffers for the directions the engine holds, then refetches the latencies
    /// (they depend on the buffer size) and whether `outputReady` is supported.
    fn rebuild(&mut self, name: &str, driver: &mut Driver) -> Result<(), ab::OpenError> {
        match std::mem::replace(&mut self.buffers, BufferState::Missing) {
            BufferState::Prepared(buffers) => {
                if let Err(code) = driver.dispose_buffers(buffers) {
                    log::warn!("{}", asio_error(name, "disposeBuffers", code));
                }
            }
            // Not reached: a session rebuilds only before anything started.
            running @ BufferState::Running(_) => {
                self.buffers = running;
                return Err(other(name, "its buffers are in use by a running stream"));
            }
            BufferState::Missing => {}
        }
        let count: ChannelCount = driver.channels().map_err(|code| asio_error(name, "getChannels", code))?;
        let (inputs, outputs, input_wire, output_wire) = self.installed.state().with(|engine, _| {
            let input = engine.capture.as_ref().map(|s| (s.channels(), s.wire()));
            let output = engine.render.as_ref().map(|s| (s.channels(), s.wire()));
            (input.map_or(0, |i| i.0), output.map_or(0, |o| o.0), input.map(|i| i.1), output.map(|o| o.1))
        });
        if inputs + outputs == 0 {
            return Ok(());
        }
        let channels: Vec<_> = count.first(true, inputs).chain(count.first(false, outputs)).collect();
        let frames = i32::try_from(self.frames).map_err(|_| other(name, "buffer size out of range"))?;
        let (buffers, pointers) = driver.create_buffers(&channels, frames).map_err(|code| asio_error(name, "createBuffers", code))?;
        let (made_inputs, made_outputs) = pointers.split_at(count.first(true, inputs).count().min(pointers.len()));
        let table = PlaneTable {
            inputs: made_inputs.to_vec(),
            outputs: made_outputs.to_vec(),
            input_bytes: self.frames * input_wire.map_or(0, Wire::bytes),
            output_bytes: self.frames * output_wire.map_or(0, Wire::bytes),
        };
        table.silence_outputs();
        let ready = driver.supports_output_ready(&buffers);
        self.installed.state().set_output_ready(ready);
        self.installed.state().with(|_, planes| *planes = table);
        self.buffers = BufferState::Prepared(buffers);
        self.refresh_latencies(name, driver);
        Ok(())
    }

    /// `getLatencies` into the atomics the stamps read.
    pub(crate) fn refresh_latencies(&mut self, name: &str, driver: &mut Driver) {
        match driver.latencies() {
            Ok((input, output)) => {
                self.latencies.input.store(u32::try_from(input.max(0)).unwrap_or(0), Ordering::Relaxed);
                self.latencies.output.store(u32::try_from(output.max(0)).unwrap_or(0), Ordering::Relaxed);
            }
            Err(code) => log::warn!("{}", asio_error(name, "getLatencies", code)),
        }
    }

    /// The host thread's periodic duty: the stops asked for from inside a driver callback, a
    /// `kAsioLatenciesChanged` refetch, and the watchdog.
    pub(crate) fn tend(&mut self, name: &str, driver: &mut Driver) {
        for direction in [ab::Direction::Output, ab::Direction::Input] {
            if self.side(direction).is_some_and(|side| side.take_stop_request()) {
                self.stop(name, driver, direction);
            }
        }
        if self.installed.state().latencies_dirty.swap(false, Ordering::Relaxed) {
            self.refresh_latencies(name, driver);
        }
        self.watch(name, crate::callback::qpc_100ns());
    }

    /// The watchdog at `now` (QPC, 100 ns): a running driver whose buffer switches stopped while a
    /// stream of it is started stalls each open stream, once per start.
    fn watch(&mut self, name: &str, now: u64) {
        let running = matches!(self.buffers, BufferState::Running(_)) && self.sides().any(|s| s.state() == STARTED);
        let state = self.installed.state();
        if self.watchdog.check(now, state.last_switch(), running) {
            log::warn!("ASIO driver \"{name}\": no buffer switch for more than {} ms; the streams stalled", self.watchdog.limit().as_millis());
            state.stalled();
        }
    }

    /// How long the host thread may sleep before it next looks at the watchdog: `None` while the
    /// driver does not run.
    pub(crate) fn tick(&self) -> Option<Duration> {
        self.watchdog.tick()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    fn sides(&self) -> impl Iterator<Item = &Arc<SideShared>> {
        self.render.iter().chain(self.capture.iter())
    }

    fn side(&self, direction: ab::Direction) -> Option<&Arc<SideShared>> {
        match direction {
            ab::Direction::Output => self.render.as_ref(),
            ab::Direction::Input => self.capture.as_ref(),
        }
    }

    fn started(&self) -> bool {
        self.sides().any(|s| matches!(s.state(), STARTED | STOPPED))
    }

    /// Whether every direction's stream is gone.
    pub(crate) fn abandoned(&self) -> bool {
        self.sides().all(|s| s.state() == DETACHED)
    }

    /// Starts `direction` (its origin pair already published by the caller is in `pair`), and the
    /// driver if it is not running yet.
    pub(crate) fn start(&mut self, name: &str, driver: &mut Driver, direction: ab::Direction, pair: (u64, i64)) -> Result<(), ab::OpenError> {
        let side = self.side(direction).cloned().ok_or_else(|| other(name, format_args!("no {direction:?} stream is open")))?;
        if side.state() != OPENED {
            return Err(ab::OpenError::Other("a stopped ASIO stream is reopened, not restarted".into()));
        }
        match std::mem::replace(&mut self.buffers, BufferState::Missing) {
            BufferState::Missing => Err(other(name, "its buffers could not be created; close the streams and open them again")),
            BufferState::Running(running) => {
                self.buffers = BufferState::Running(running);
                side.start(pair.0, pair.1);
                Ok(())
            }
            BufferState::Prepared(buffers) => {
                // A fresh driver start: forget the stamps of any earlier run.
                self.installed.state().with(|engine, planes| {
                    engine.restart();
                    planes.silence_outputs();
                });
                side.start(pair.0, pair.1);
                self.installed.state().forget_last_switch();
                self.mmcss.arm();
                match driver.start(buffers) {
                    Ok(running) => {
                        self.buffers = BufferState::Running(running);
                        self.watchdog.arm(crate::callback::qpc_100ns());
                        Ok(())
                    }
                    Err((buffers, code)) => {
                        self.buffers = BufferState::Prepared(buffers);
                        side.state.store(STOPPED, Ordering::Release);
                        Err(asio_error(name, "start", code))
                    }
                }
            }
        }
    }

    /// Stops `direction`; stops the driver when no stream of it runs any more. Returns once no
    /// buffer switch is inside this direction's callback (the other direction may keep the driver
    /// running), so the owner sees no callback of it after `stop()` returns.
    pub(crate) fn stop(&mut self, name: &str, driver: &mut Driver, direction: ab::Direction) {
        if let Some(side) = self.side(direction) {
            let _ = side.state.compare_exchange(OPENED, STOPPED, Ordering::SeqCst, Ordering::SeqCst);
            let _ = side.state.compare_exchange(STARTED, STOPPED, Ordering::SeqCst, Ordering::SeqCst);
        }
        self.quiesce(name, direction);
        self.stop_if_idle(name, driver);
    }

    /// Marks `direction` detached (waiting for a callback inside it, as [`Session::stop`]); true
    /// when the session has no stream left.
    pub(crate) fn detach(&mut self, name: &str, driver: &mut Driver, direction: ab::Direction) -> bool {
        if let Some(side) = self.side(direction) {
            side.state.store(DETACHED, Ordering::SeqCst);
        }
        self.quiesce(name, direction);
        self.stop_if_idle(name, driver);
        self.abandoned()
    }

    /// Waits while a buffer switch is inside `direction`'s callback, when the driver runs.
    fn quiesce(&self, name: &str, direction: ab::Direction) {
        if !matches!(self.buffers, BufferState::Running(_)) {
            return;
        }
        if let Some(side) = self.side(direction)
            && !side.quiesce(QUIESCE_LIMIT)
        {
            log::warn!("ASIO driver \"{name}\": a {direction:?} callback did not return within {} s of its stop", QUIESCE_LIMIT.as_secs());
        }
    }

    fn stop_if_idle(&mut self, name: &str, driver: &mut Driver) {
        if self.sides().any(|s| s.state() == STARTED) {
            return;
        }
        self.buffers = match std::mem::replace(&mut self.buffers, BufferState::Missing) {
            BufferState::Running(running) => {
                self.watchdog.disarm();
                self.release_mmcss(name);
                let (buffers, result) = driver.stop(running);
                if let Err(code) = result {
                    log::warn!("{}", asio_error(name, "stop", code));
                }
                BufferState::Prepared(buffers)
            }
            other => other,
        };
    }

    /// Reverts the buffer-switch thread's MMCSS registration before the driver stops: on that
    /// thread at its next switch, within three buffers (20 to 200 ms), else from here.
    fn release_mmcss(&self, name: &str) {
        let period = Duration::from_secs_f64(self.frames as f64 / f64::from(self.rate.max(1)));
        let wait = (period * 3).clamp(Duration::from_millis(20), Duration::from_millis(200));
        if self.mmcss.priority() == ab::WorkerPriority::PromotionFailed {
            log::warn!("ASIO driver \"{name}\": MMCSS refused the buffer-switch thread (error {})", self.mmcss.error());
        }
        match self.mmcss.release(wait) {
            Released::Nothing => {}
            Released::OnItsThread => log::debug!("ASIO driver \"{name}\": the buffer-switch thread left MMCSS at the stop"),
            Released::FromHost => log::info!("ASIO driver \"{name}\": no buffer switch at the stop; MMCSS reverted from the host thread"),
            Released::Failed(code) => log::warn!("ASIO driver \"{name}\": AvRevertMmThreadCharacteristics failed (error {code})"),
            Released::Abandoned => log::warn!("ASIO driver \"{name}\": a buffer switch was still registering with MMCSS at the stop"),
        }
    }

    /// Stops the driver, disposes the buffers and frees the callback state (and the callbacks).
    pub(crate) fn teardown(mut self, name: &str, driver: &mut Driver) {
        for side in self.sides() {
            side.state.store(DETACHED, Ordering::Release);
        }
        self.stop_if_idle(name, driver);
        if let BufferState::Prepared(buffers) = std::mem::replace(&mut self.buffers, BufferState::Missing)
            && let Err(code) = driver.dispose_buffers(buffers)
        {
            log::warn!("{}", asio_error(name, "disposeBuffers", code));
        }
        // `self.installed` drops here: uninstalled, drained, freed.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(min: i32, max: i32, preferred: i32, granularity: i32) -> BufferSize {
        BufferSize { min, max, preferred, granularity }
    }

    #[test]
    fn the_default_period_is_the_preferred_size() {
        assert_eq!(negotiate(size(16, 1024, 256, 16), ab::Period::Default), Ok(256));
        assert_eq!(negotiate(size(256, 256, 256, 0), ab::Period::Default), Ok(256));
        assert_eq!(negotiate(size(64, 2048, 0, -1), ab::Period::Default), Ok(64), "no preferred size: the minimum");
    }

    #[test]
    fn a_zero_buffer_size_is_an_error_not_a_panic() {
        let e = negotiate(size(0, 0, 0, 0), ab::Period::Frames(64)).unwrap_err();
        assert!(e.contains("no usable buffer size"), "{e}");
    }

    #[test]
    fn requests_snap_to_the_driver_steps() {
        let focusrite = size(16, 1024, 256, 16);
        for (want, got) in [(64, 64), (32, 32), (70, 64), (72, 80), (8, 16), (5000, 1024), (1, 16)] {
            assert_eq!(negotiate(focusrite, ab::Period::Frames(want)), Ok(got), "{want}");
        }
        let powers = size(32, 2048, 256, -1);
        for (want, got) in [(64, 64), (100, 128), (96, 128), (95, 64), (16, 32), (4096, 2048)] {
            assert_eq!(negotiate(powers, ab::Period::Frames(want)), Ok(got), "{want}");
        }
        let fixed = size(256, 256, 256, 0);
        assert_eq!(negotiate(fixed, ab::Period::Frames(64)), Ok(256), "the Model 12's one size");
        let odd = size(48, 480, 144, 48);
        assert_eq!(negotiate(odd, ab::Period::Frames(100)), Ok(96));
    }
}
