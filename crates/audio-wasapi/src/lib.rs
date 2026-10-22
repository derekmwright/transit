//! The native WASAPI backend (ADR 0016, slice c): shared-mode, event-driven render through
//! `IAudioClient3` at the smallest engine period the endpoint allows, with the device position of
//! `IAudioClock2` as the stamp source. Windows only: on every other platform the crate is empty.
//!
//! This is the one crate besides `soundcraft-rt` that contains `unsafe` (ADR 0013, second
//! amendment), for COM and WASAPI through the `windows` crate only. Each stream is owned by one
//! MTA thread; see [`com`] for the threading rules and [`render_loop`] for the audio-thread loop.
//! Capture arrives with slice (d); exclusive mode with slice (e).
#![cfg(windows)]
// The callback modules (`render_loop`, `convert`) deny these lints; control-thread code may
// allocate and format.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

mod com;
mod convert;
mod lifecycle;
mod render_loop;

use com::{EndpointInfo, OpenParams, RenderStream, Watcher};
use soundcraft_audio_backend as ab;
use std::sync::{Mutex, PoisonError};

/// The system name every endpoint of this backend carries.
pub const SYSTEM: &str = "WASAPI";

/// Creates the native backend for the system named "WASAPI" (or the default one).
pub struct WasapiFactory;

impl ab::BackendFactory for WasapiFactory {
    fn backend(&self, system: Option<&str>) -> Option<Box<dyn ab::Backend>> {
        system.is_none_or(|s| s == SYSTEM).then(|| Box::new(WasapiBackend::default()) as Box<dyn ab::Backend>)
    }
}

/// The device reference an endpoint carries: the endpoint id string, so no COM pointer crosses
/// threads. Each open resolves it again on the stream thread.
struct WasapiEndpoint {
    id: String,
}

/// The native WASAPI audio system.
#[derive(Default)]
pub struct WasapiBackend {
    watcher: Mutex<Option<Watcher>>,
}

fn endpoint_of(info: &EndpointInfo, output: bool) -> ab::Endpoint {
    let formats = vec![range(&info.mix.format(), info.mix.rate, info.periods)];
    let channels = info.mix.channels;
    let (inputs, outputs) = if output { (0, channels) } else { (channels, 0) };
    ab::Endpoint::new(
        info.id.clone(),
        info.name.clone(),
        SYSTEM.into(),
        inputs,
        outputs,
        formats,
        std::sync::Arc::new(WasapiEndpoint { id: info.id.clone() }),
    )
}

fn range(format: &ab::Format, rate: u32, period: Option<(u32, u32)>) -> ab::FormatRange {
    ab::FormatRange { sample: format.sample, channels: format.channels, min_rate: rate, max_rate: rate, period }
}

impl ab::Backend for WasapiBackend {
    fn system(&self) -> &str {
        SYSTEM
    }

    fn endpoints(&self, direction: ab::Direction) -> Result<Vec<ab::Endpoint>, ab::OpenError> {
        let output = direction == ab::Direction::Output;
        Ok(com::list_endpoints(output)?.iter().map(|info| endpoint_of(info, output)).collect())
    }

    fn plan_render(&self, wish: &ab::OutputWish) -> Result<ab::Plan, String> {
        let endpoints = com::list_endpoints(true).map_err(|e| e.to_string())?;
        let info = wish
            .device
            .as_deref()
            .and_then(|name| endpoints.iter().find(|e| e.name == name || e.id == name))
            .or_else(|| endpoints.first())
            .ok_or("no output device")?;
        // The endpoint's own rate first (no conversion in the engine); the wished rates after it
        // are tried with the engine converting.
        let mut rates = vec![info.mix.rate];
        rates.extend(wish.rates.iter().copied().filter(|r| !rates.contains(r)).collect::<Vec<_>>());
        rates.dedup();
        let period = wish.period.map_or(ab::Period::Default, ab::Period::Frames);
        let attempts = rates
            .into_iter()
            .map(|rate| ab::OpenRequest {
                format: ab::Format { rate, ..info.mix.format() },
                period,
                mode: ab::Mode::Shared,
                origin: ab::HostOrigin::default(),
            })
            .collect();
        Ok(ab::Plan { endpoint: endpoint_of(info, true), attempts })
    }

    fn plan_capture(&self, _wish: &ab::InputWish) -> Result<ab::Plan, String> {
        Err(NO_CAPTURE.into())
    }

    fn capture_with_render(&self, _render: &ab::Endpoint) -> bool {
        false
    }

    fn capture_format(&self, _endpoint: &ab::Endpoint, _rate: Option<u32>) -> Result<ab::Format, String> {
        Err(NO_CAPTURE.into())
    }

    fn open_render(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        render: ab::RenderCallback,
        error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        let device = endpoint.handle::<WasapiEndpoint>().ok_or_else(|| ab::OpenError::Other("the endpoint is not a WASAPI device".into()))?;
        let (stream, achieved) = RenderStream::open(OpenParams { id: device.id.clone(), request: *request }, render, error)?;
        Ok((Box::new(stream), achieved))
    }

    fn open_capture(
        &self,
        _endpoint: &ab::Endpoint,
        _request: &ab::OpenRequest,
        _capture: ab::CaptureCallback,
        _error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        Err(ab::OpenError::Other(NO_CAPTURE.into()))
    }

    fn on_device_change(&self, handler: Box<dyn FnMut(ab::DeviceEvent) + Send>) -> bool {
        match Watcher::start(handler) {
            Ok(watcher) => {
                *self.watcher.lock().unwrap_or_else(PoisonError::into_inner) = Some(watcher);
                true
            }
            Err(e) => {
                log::warn!("WASAPI device notifications unavailable: {e}");
                false
            }
        }
    }
}

const NO_CAPTURE: &str = "capture arrives with slice (d)";

#[cfg(test)]
mod hardware {
    //! Tests that open the owner's real default endpoint. They are `#[ignore]`d: the owner runs
    //! them with `cargo test -p soundcraft-audio-wasapi -- --ignored hardware_ --nocapture`.
    use super::*;
    use ab::Backend;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "opens the default audio endpoint"]
    fn hardware_default_endpoint_runs_two_seconds_with_a_sane_period() {
        let backend = WasapiBackend::default();
        let plan = backend.plan_render(&ab::OutputWish { channels: 2, ..Default::default() }).expect("a default endpoint");
        let (blocks, frames, invalid) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (b, f, i) = (Arc::clone(&blocks), Arc::clone(&frames), Arc::clone(&invalid));
        let render: ab::RenderCallback = Box::new(move |block| {
            b.fetch_add(1, Ordering::Relaxed);
            f.fetch_add(block.frames as u64, Ordering::Relaxed);
            if block.stamp.quality == ab::Quality::Invalid {
                i.fetch_add(1, Ordering::Relaxed);
            }
            block.render(&mut |chunk| chunk.fill(0.0));
        });
        let faults = Arc::new(AtomicU64::new(0));
        let fault_count = Arc::clone(&faults);
        let error: ab::ErrorCallback = Box::new(move |_| {
            fault_count.fetch_add(1, Ordering::Relaxed);
        });
        let (stream, achieved) = backend.open_render(&plan.endpoint, &plan.attempts[0], render, error).expect("open");
        println!("achieved {achieved:?}, priority {:?}", stream.thread_priority());
        stream.start().expect("start");
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let (blocks, frames) = (blocks.load(Ordering::Relaxed), frames.load(Ordering::Relaxed));
        drop(stream);
        let period = achieved.period.frames().expect("the real period");
        let rate = u64::from(achieved.format.rate);
        let expected_blocks = started.elapsed().as_secs_f64() * rate as f64 / f64::from(period);
        println!("{blocks} blocks, {frames} frames in 2 s; expected about {expected_blocks:.0} blocks of {period} frames");
        assert!(blocks > 0 && faults.load(Ordering::Relaxed) == 0);
        assert!(f64::from(u32::try_from(blocks).unwrap()) > expected_blocks * 0.5, "far fewer callbacks than the period implies");
        assert!(frames as f64 / blocks as f64 <= f64::from(period) * 2.5, "blocks larger than the engine period allows");
        assert_eq!(invalid.load(Ordering::Relaxed), 0, "stamps with an unreadable clock");
    }

    #[test]
    #[ignore = "opens the default audio endpoint"]
    fn hardware_stop_and_drop_close_cleanly_before_and_after_start() {
        let backend = WasapiBackend::default();
        let plan = backend.plan_render(&ab::OutputWish { channels: 2, ..Default::default() }).expect("a default endpoint");
        let open = || {
            backend
                .open_render(&plan.endpoint, &plan.attempts[0], Box::new(|block| block.render(&mut |c| c.fill(0.0))), Box::new(|_| {}))
                .expect("open")
                .0
        };
        drop(open()); // never started
        let stream = open();
        stream.start().expect("start");
        std::thread::sleep(Duration::from_millis(200));
        stream.stop().expect("stop");
        assert!(stream.start().is_err(), "a stopped stream is reopened, not restarted");
    }

    #[test]
    #[ignore = "enumerates the audio endpoints"]
    fn hardware_endpoints_list_names_formats_and_the_period_floor() {
        for endpoint in WasapiBackend::default().endpoints(ab::Direction::Output).expect("endpoints") {
            println!("{} [{}]: {} ch, formats {:?}", endpoint.name, endpoint.id, endpoint.outputs, endpoint.formats);
        }
    }
}
