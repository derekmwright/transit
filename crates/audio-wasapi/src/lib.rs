//! The native WASAPI backend (ADR 0016, slice c): shared-mode, event-driven render through
//! `IAudioClient3` at the smallest engine period the endpoint allows, with the device position of
//! `IAudioClock2` as the stamp source. Windows only: on every other platform the crate is empty.
//!
//! This is the one crate besides `transit-rt` that contains `unsafe` (ADR 0013, second
//! amendment), for COM and WASAPI through the `windows` crate only. Each stream is owned by one
//! MTA thread; see [`com`] for the threading rules and [`render_loop`] for the audio-thread loop.
//! Capture and the pairing of capture with render endpoints are slice (d). Exclusive mode (slice e)
//! opens an event-driven exclusive client at a probed format and the driver's period (see
//! [`exclusive`]); enumeration stays shared-mode only and never probes.
#![cfg(windows)]
// The callback modules (`render_loop`, `convert`) deny these lints; control-thread code may
// allocate and format.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

mod capture_loop;
mod com;
mod convert;
mod exclusive;
mod lifecycle;
mod pairing;
mod render_loop;
mod stats;

use com::{EndpointInfo, NativeStream, OpenParams, Watcher};
use std::sync::{Mutex, PoisonError};
use transit_audio_backend as ab;

/// The system name every endpoint of this backend carries.
pub const SYSTEM: &str = "WASAPI";

/// Creates the native backend for the system named "WASAPI" (or the default one).
#[derive(Debug, Clone, Default)]
pub struct WasapiFactory {
    /// The capture endpoint (id or name) the user paired with the output, for devices whose
    /// endpoints share no container id. `None` pairs by container id alone.
    pub input_pairing: Option<String>,
}

impl ab::BackendFactory for WasapiFactory {
    fn backend(&self, system: Option<&str>) -> Option<Box<dyn ab::Backend>> {
        system
            .is_none_or(|s| s == SYSTEM)
            .then(|| Box::new(WasapiBackend { pairing: self.input_pairing.clone(), ..Default::default() }) as Box<dyn ab::Backend>)
    }
}

/// The device reference an endpoint carries: the endpoint id string, so no COM pointer crosses
/// threads. Each open resolves it again on the stream thread.
struct WasapiEndpoint {
    id: String,
    /// A render endpoint (a capture open on it goes to its paired capture endpoint).
    output: bool,
}

/// The native WASAPI audio system.
#[derive(Default)]
pub struct WasapiBackend {
    watcher: Mutex<Option<Watcher>>,
    pairing: Option<String>,
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
        std::sync::Arc::new(WasapiEndpoint { id: info.id.clone(), output }),
    )
}

fn range(format: &ab::Format, rate: u32, period: Option<(u32, u32)>) -> ab::FormatRange {
    ab::FormatRange { sample: format.sample, channels: format.channels, min_rate: rate, max_rate: rate, period }
}

/// The requests to try on `info`: the wished rates first, in the order given (the session's rate
/// wins; where it differs from the endpoint's mix rate the engine converts in shared mode), then the
/// endpoint's own mix rate as the fallback when no wished rate can be opened.
fn attempts(info: &EndpointInfo, wished: Vec<u32>, period: Option<u32>) -> Vec<ab::OpenRequest> {
    let mut rates = Vec::new();
    for rate in wished.into_iter().chain([info.mix.rate]) {
        if !rates.contains(&rate) {
            rates.push(rate);
        }
    }
    let period = period.map_or(ab::Period::Default, ab::Period::Frames);
    rates
        .into_iter()
        .map(|rate| ab::OpenRequest {
            format: ab::Format { rate, ..info.mix.format() },
            period,
            mode: ab::Mode::Shared,
            origin: ab::HostOrigin::default(),
            calibration: None,
        })
        .collect()
}

impl WasapiBackend {
    /// The exclusive-mode formats `endpoint` accepts among [`ab::exclusive_candidates`] for
    /// `rate_first` and `channels`, for the device report. It probes the device (a control-thread
    /// call that fails while another app holds it); enumeration never calls it.
    pub fn exclusive_formats(&self, endpoint: &ab::Endpoint, rate_first: u32, channels: &[u16]) -> Result<Vec<ab::Format>, ab::OpenError> {
        ab::Backend::probe_exclusive(self, endpoint, &ab::exclusive_candidates(rate_first, channels))
    }

    /// The capture endpoint to use for `endpoint`: itself when it is a capture endpoint, else the
    /// one paired with the render endpoint.
    fn paired_input(&self, endpoint: &ab::Endpoint) -> Result<EndpointInfo, ab::OpenError> {
        let device = endpoint.handle::<WasapiEndpoint>().ok_or_else(|| ab::OpenError::Other("the endpoint is not a WASAPI device".into()))?;
        let inputs = com::list_endpoints(false)?;
        if !device.output {
            return inputs
                .into_iter()
                .find(|i| i.id == device.id)
                .ok_or_else(|| ab::OpenError::DeviceNotAvailable("the input endpoint is gone".into()));
        }
        let outputs = com::list_endpoints(true)?;
        let render =
            outputs.iter().find(|o| o.id == device.id).ok_or_else(|| ab::OpenError::DeviceNotAvailable("the output endpoint is gone".into()))?;
        let at = com::paired_input(render, &inputs, self.pairing.as_deref())
            .ok_or_else(|| ab::OpenError::Other(format!("no input shares the container of {}; pair one in Audio Devices", render.name)))?;
        inputs.into_iter().nth(at).ok_or_else(|| ab::OpenError::Other("the paired input vanished".into()))
    }
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
        Ok(ab::Plan { endpoint: endpoint_of(info, true), attempts: attempts(info, wish.rates.clone(), wish.period) })
    }

    fn plan_capture(&self, wish: &ab::InputWish) -> Result<ab::Plan, String> {
        let endpoints = com::list_endpoints(false).map_err(|e| e.to_string())?;
        let info = wish
            .device
            .as_deref()
            .and_then(|name| endpoints.iter().find(|e| e.name == name || e.id == name))
            .or_else(|| endpoints.first())
            .ok_or("no input device")?;
        Ok(ab::Plan { endpoint: endpoint_of(info, false), attempts: attempts(info, wish.rate.into_iter().collect(), wish.period) })
    }

    /// A capture endpoint belongs with `render` when the user paired it or it has the same
    /// container id (see [`pairing`]).
    fn capture_with_render(&self, render: &ab::Endpoint) -> bool {
        self.paired_input(render).is_ok()
    }

    fn capture_format(&self, endpoint: &ab::Endpoint, rate: Option<u32>) -> Result<ab::Format, String> {
        let info = self.paired_input(endpoint).map_err(|e| e.to_string())?;
        Ok(ab::Format { rate: rate.unwrap_or(info.mix.rate), ..info.mix.format() })
    }

    fn open_render(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        render: ab::RenderCallback,
        error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        let device = endpoint.handle::<WasapiEndpoint>().ok_or_else(|| ab::OpenError::Other("the endpoint is not a WASAPI device".into()))?;
        let (stream, achieved) = NativeStream::open_render(OpenParams { id: device.id.clone(), request: *request }, render, error)?;
        Ok((Box::new(stream), achieved))
    }

    /// Open a capture stream on `endpoint`, or, for a render endpoint, on the capture endpoint
    /// paired with it: the duplex open of the Player and the harness.
    fn open_capture(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        capture: ab::CaptureCallback,
        error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        let device = endpoint.handle::<WasapiEndpoint>().ok_or_else(|| ab::OpenError::Other("the endpoint is not a WASAPI device".into()))?;
        let id = if device.output { self.paired_input(endpoint)?.id } else { device.id.clone() };
        let (stream, achieved) = NativeStream::open_capture(OpenParams { id, request: *request }, capture, error)?;
        Ok((Box::new(stream), achieved))
    }

    /// Probes the endpoint itself (a render endpoint's render side, a capture endpoint's capture side).
    fn probe_exclusive(&self, endpoint: &ab::Endpoint, candidates: &[ab::Format]) -> Result<Vec<ab::Format>, ab::OpenError> {
        let device = endpoint.handle::<WasapiEndpoint>().ok_or_else(|| ab::OpenError::Other("the endpoint is not a WASAPI device".into()))?;
        com::probe_exclusive(&device.id, candidates)
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

#[cfg(test)]
mod attempt_order {
    use super::*;
    use crate::convert::Wire;

    fn endpoint(rate: u32) -> EndpointInfo {
        let mix = com::Fmt { rate, channels: 2, sample: ab::SampleFormat::F32, wire: Wire::F32, valid_bits: 32, mask: 3 };
        EndpointInfo { id: "id".into(), name: "name".into(), mix, periods: None, container: None }
    }

    fn rates(info: &EndpointInfo, wished: Vec<u32>) -> Vec<u32> {
        attempts(info, wished, None).iter().map(|r| r.format.rate).collect()
    }

    #[test]
    fn the_wished_rate_comes_before_the_endpoint_mix_rate() {
        assert_eq!(rates(&endpoint(48_000), vec![44_100]), [44_100, 48_000]);
    }

    #[test]
    fn wished_rates_keep_their_order_and_the_mix_rate_is_not_repeated() {
        assert_eq!(rates(&endpoint(48_000), vec![96_000, 48_000, 44_100]), [96_000, 48_000, 44_100]);
        assert_eq!(rates(&endpoint(48_000), vec![]), [48_000]);
    }
}

#[cfg(test)]
mod hardware {
    //! Tests that open the owner's real default endpoint. They are `#[ignore]`d: the owner runs
    //! them with `cargo test -p transit-audio-wasapi -- --ignored hardware_ --nocapture`.
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

    #[derive(Default)]
    struct Tally {
        blocks: AtomicU64,
        frames: AtomicU64,
        invalid: AtomicU64,
        faults: AtomicU64,
        /// `host_callback_ns - backend_callback_ns` of each valid block: the host/backend offset.
        offsets: std::sync::Mutex<Vec<i64>>,
        /// Per capture block, in order: `GetBuffer` device position, frames, and whether the stamp was invalid.
        positions: std::sync::Mutex<Vec<(Option<u64>, usize, bool)>>,
    }

    /// The gap fill sizes a dropout from the `GetBuffer` device position, so it must advance with the frames.
    fn assert_position_advances(tally: &Tally) {
        let blocks = tally.positions.lock().expect("positions").clone();
        assert!(!blocks.is_empty(), "no capture blocks were seen");
        let zeros = blocks.iter().filter(|b| b.0 == Some(0)).count();
        let good: Vec<(u64, usize)> =
            blocks.iter().filter(|b| !b.2).map(|b| (b.0.expect("a block without a timestamp error has no device position"), b.1)).collect();
        assert!(!good.is_empty(), "every block had a timestamp error");
        let (first, last) = (good[0].0, good[good.len() - 1].0);
        // Frames delivered from the first block's start to the last block's start.
        let delivered: u64 = good[..good.len() - 1].iter().map(|b| b.1 as u64).sum();
        eprintln!("device_position: first {first}, last {last}, frames delivered {delivered}, blocks {}, blocks at Some(0) {zeros}", blocks.len());
        assert!(good.windows(2).all(|w| w[0].0 <= w[1].0), "device position went backwards");
        assert!(
            (last - first) as f64 >= delivered as f64 * 0.8,
            "device position did not advance with the frames: {first} to {last} over {delivered} frames"
        );
    }

    fn median(values: &mut [i64]) -> i64 {
        values.sort_unstable();
        values.get(values.len() / 2).copied().unwrap_or(0)
    }

    fn capture_callback(tally: &Arc<Tally>) -> ab::CaptureCallback {
        let tally = Arc::clone(tally);
        Box::new(move |block| {
            tally.blocks.fetch_add(1, Ordering::Relaxed);
            tally.frames.fetch_add(block.frames as u64, Ordering::Relaxed);
            if let Ok(mut positions) = tally.positions.lock() {
                positions.push((block.device_position, block.frames, block.stamp.quality == ab::Quality::Invalid));
            }
            if block.stamp.quality == ab::Quality::Invalid {
                tally.invalid.fetch_add(1, Ordering::Relaxed);
            } else if let Ok(mut offsets) = tally.offsets.lock() {
                offsets.push(block.stamp.host_callback_ns as i64 - block.stamp.backend_callback_ns);
            }
            block.chunks(&mut |_, _| {});
        })
    }

    fn render_callback(tally: &Arc<Tally>) -> ab::RenderCallback {
        let tally = Arc::clone(tally);
        Box::new(move |block| {
            tally.blocks.fetch_add(1, Ordering::Relaxed);
            tally.frames.fetch_add(block.frames as u64, Ordering::Relaxed);
            if block.stamp.quality == ab::Quality::Invalid {
                tally.invalid.fetch_add(1, Ordering::Relaxed);
            } else if let Ok(mut offsets) = tally.offsets.lock() {
                offsets.push(block.stamp.host_callback_ns as i64 - block.stamp.backend_callback_ns);
            }
            block.render(&mut |chunk| chunk.fill(0.0));
        })
    }

    fn fault_callback(tally: &Arc<Tally>) -> ab::ErrorCallback {
        let tally = Arc::clone(tally);
        Box::new(move |_| {
            tally.faults.fetch_add(1, Ordering::Relaxed);
        })
    }

    #[test]
    #[ignore = "opens the default audio capture endpoint"]
    fn hardware_capture_default_endpoint_runs_two_seconds() {
        let backend = WasapiBackend::default();
        let plan = backend.plan_capture(&ab::InputWish::default()).expect("a default capture endpoint");
        let tally = Arc::new(Tally::default());
        let (stream, achieved) =
            backend.open_capture(&plan.endpoint, &plan.attempts[0], capture_callback(&tally), fault_callback(&tally)).expect("open");
        println!("achieved {achieved:?}, priority {:?}", stream.thread_priority());
        stream.start().expect("start");
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let (blocks, frames) = (tally.blocks.load(Ordering::Relaxed), tally.frames.load(Ordering::Relaxed));
        drop(stream);
        let rate = f64::from(achieved.format.rate);
        let expected = started.elapsed().as_secs_f64() * rate;
        println!("{blocks} blocks, {frames} frames in 2 s; expected about {expected:.0} frames");
        assert!(blocks > 0 && tally.faults.load(Ordering::Relaxed) == 0);
        assert!((frames as f64 - expected).abs() < expected * 0.1, "captured frames are not the elapsed time at the rate");
        assert_eq!(tally.invalid.load(Ordering::Relaxed), 0, "stamps with an unreadable clock");
        assert_position_advances(&tally);
    }

    #[test]
    #[ignore = "opens the default audio render and capture endpoints"]
    fn hardware_duplex_scarlett_pair_by_container() {
        let backend = WasapiBackend::default();
        let render_plan = backend.plan_render(&ab::OutputWish { channels: 2, ..Default::default() }).expect("a default render endpoint");
        // The default devices pair by container id when they are one interface (the Scarlett);
        // otherwise this opens the default capture endpoint next to the default render one.
        let paired = backend.capture_with_render(&render_plan.endpoint);
        println!("default render {} pairs with a capture endpoint by container: {paired}", render_plan.endpoint.name);
        let capture_plan = backend.plan_capture(&ab::InputWish::default()).expect("a default capture endpoint");
        let origin = ab::HostOrigin::default();
        let (render, capture) = (Arc::new(Tally::default()), Arc::new(Tally::default()));
        let mut capture_request = capture_plan.attempts[0];
        capture_request.origin = origin;
        let (capture_stream, _) = if paired {
            let request = ab::OpenRequest { format: backend.capture_format(&render_plan.endpoint, None).expect("format"), ..capture_request };
            backend.open_capture(&render_plan.endpoint, &request, capture_callback(&capture), fault_callback(&capture)).expect("open paired")
        } else {
            backend.open_capture(&capture_plan.endpoint, &capture_request, capture_callback(&capture), fault_callback(&capture)).expect("open")
        };
        let mut render_request = render_plan.attempts[0];
        render_request.origin = origin;
        let (render_stream, _) =
            backend.open_render(&render_plan.endpoint, &render_request, render_callback(&render), fault_callback(&render)).expect("open render");
        capture_stream.start().expect("start capture");
        render_stream.start().expect("start render");
        std::thread::sleep(Duration::from_secs(2));
        drop((render_stream, capture_stream));
        let (rb, cb) = (render.blocks.load(Ordering::Relaxed), capture.blocks.load(Ordering::Relaxed));
        assert!(rb > 0 && cb > 0, "both streams ran ({rb} render, {cb} capture blocks)");
        assert_eq!(render.faults.load(Ordering::Relaxed) + capture.faults.load(Ordering::Relaxed), 0);
        assert_eq!(render.invalid.load(Ordering::Relaxed) + capture.invalid.load(Ordering::Relaxed), 0, "stamps with an unreadable clock");
        // Both stamp against the one host origin through QPC: the offset between host time and
        // each stream's own backend time is the same number for both, within callback jitter.
        let (render_offset, capture_offset) = (median(&mut render.offsets.lock().unwrap()), median(&mut capture.offsets.lock().unwrap()));
        println!("host - backend offset: render {render_offset} ns, capture {capture_offset} ns");
        assert!((render_offset - capture_offset).abs() < 5_000_000, "the two streams' stamps are not comparable in one host origin");
    }

    /// `result`, or a test failure whose message starts "in use" when another app holds the device.
    fn or_in_use<T>(result: Result<T, ab::OpenError>, what: &str) -> T {
        match result {
            Ok(value) => value,
            Err(ab::OpenError::InUse(text)) => {
                println!("in use: {what}: {text}");
                panic!("in use: {what}: {text}");
            }
            Err(e) => panic!("{what}: {e:?}"),
        }
    }

    /// An exclusive request at the first probed format of `endpoint` (its shared rate and channels
    /// first), at the device's default period, stamped against `origin`.
    fn exclusive_request(backend: &WasapiBackend, endpoint: &ab::Endpoint, like: ab::Format, origin: ab::HostOrigin) -> ab::OpenRequest {
        let formats = or_in_use(backend.exclusive_formats(endpoint, like.rate, &[like.channels]), "probe");
        println!("{} accepts in exclusive mode: {formats:?}", endpoint.name);
        let format = *formats.first().unwrap_or_else(|| panic!("{} accepts no exclusive format at {} channels", endpoint.name, like.channels));
        ab::OpenRequest { format, period: ab::Period::Default, mode: ab::Mode::Exclusive, origin, calibration: None }
    }

    fn print_achieved(what: &str, achieved: &ab::Achieved) {
        let f = achieved.format;
        let ms = achieved.period.frames().map(|n| f64::from(n) * 1000.0 / f64::from(f.rate));
        println!(
            "{what}: {:?} {} valid bits, {} ch, mask {:?}, {} Hz; period {:?} ({ms:.2?} ms), device minimum {:?} frames; latency {:?}",
            f.sample,
            f.valid_bits.map_or("all".to_string(), |b| b.to_string()),
            f.channels,
            f.channel_mask,
            f.rate,
            achieved.period,
            achieved.min_period,
            achieved.latency
        );
    }

    #[test]
    #[ignore = "opens the default audio endpoint exclusively (other apps are silenced)"]
    fn hardware_exclusive_render_default_endpoint_two_seconds() {
        let backend = WasapiBackend::default();
        let plan = backend.plan_render(&ab::OutputWish { channels: 2, ..Default::default() }).expect("a default endpoint");
        let request = exclusive_request(&backend, &plan.endpoint, plan.attempts[0].format, ab::HostOrigin::default());
        let tally = Arc::new(Tally::default());
        let (stream, achieved) =
            or_in_use(backend.open_render(&plan.endpoint, &request, render_callback(&tally), fault_callback(&tally)), "open exclusive render");
        print_achieved("exclusive render", &achieved);
        or_in_use(stream.start(), "start exclusive render");
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let (blocks, frames) = (tally.blocks.load(Ordering::Relaxed), tally.frames.load(Ordering::Relaxed));
        drop(stream);
        let period = achieved.period.frames().expect("the real period");
        let expected = started.elapsed().as_secs_f64() * f64::from(achieved.format.rate) / f64::from(period);
        println!("{blocks} blocks, {frames} frames in 2 s; expected about {expected:.0} blocks of {period} frames");
        assert!(blocks > 0, "no callbacks");
        assert_eq!(tally.faults.load(Ordering::Relaxed), 0, "stream faults");
        assert_eq!(frames, blocks * u64::from(period), "every exclusive wake fills the whole buffer");
        assert!(blocks as f64 > expected * 0.5, "far fewer callbacks than the period implies");
        assert_eq!(tally.invalid.load(Ordering::Relaxed), 0, "stamps with an unreadable clock");
    }

    #[test]
    #[ignore = "opens the capture endpoint paired with the default render endpoint exclusively"]
    fn hardware_exclusive_capture_default_endpoint_two_seconds() {
        // The capture endpoint of the default render device (paired by container id): the system
        // default capture endpoint may be another device (a webcam mic) with no exclusive format.
        let backend = WasapiBackend::default();
        let render = backend.plan_render(&ab::OutputWish { channels: 2, ..Default::default() }).expect("a default render endpoint").endpoint;
        let input = match backend.paired_input(&render) {
            Ok(input) => input,
            Err(why) => {
                println!("skipped: the default render endpoint {} has no paired capture endpoint ({why})", render.name);
                return;
            }
        };
        let Some(endpoint) = backend.endpoints(ab::Direction::Input).expect("inputs").into_iter().find(|e| e.id == input.id) else {
            println!("skipped: the paired capture endpoint {} is gone", input.name);
            return;
        };
        let like = backend.capture_format(&render, None).expect("format");
        let formats = or_in_use(backend.exclusive_formats(&endpoint, like.rate, &[like.channels]), "probe");
        println!("{} accepts in exclusive mode: {formats:?}", endpoint.name);
        let Some(&format) = formats.first() else {
            println!("skipped: {} accepts no exclusive format at {} channels", endpoint.name, like.channels);
            return;
        };
        let request =
            ab::OpenRequest { format, period: ab::Period::Default, mode: ab::Mode::Exclusive, origin: ab::HostOrigin::default(), calibration: None };
        let tally = Arc::new(Tally::default());
        let (stream, achieved) =
            or_in_use(backend.open_capture(&endpoint, &request, capture_callback(&tally), fault_callback(&tally)), "open exclusive capture");
        print_achieved("exclusive capture", &achieved);
        or_in_use(stream.start(), "start exclusive capture");
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let (blocks, frames) = (tally.blocks.load(Ordering::Relaxed), tally.frames.load(Ordering::Relaxed));
        drop(stream);
        let expected = started.elapsed().as_secs_f64() * f64::from(achieved.format.rate);
        println!("{blocks} blocks, {frames} frames in 2 s; expected about {expected:.0} frames");
        assert!(blocks > 0, "no callbacks");
        assert_eq!(tally.faults.load(Ordering::Relaxed), 0, "stream faults");
        assert!((frames as f64 - expected).abs() < expected * 0.1, "captured frames are not the elapsed time at the rate");
        assert_eq!(tally.invalid.load(Ordering::Relaxed), 0, "stamps with an unreadable clock");
        assert_position_advances(&tally);
    }

    #[test]
    #[ignore = "opens the default audio render and capture endpoints exclusively"]
    fn hardware_exclusive_duplex_scarlett_pair_by_container() {
        let backend = WasapiBackend::default();
        let render_plan = backend.plan_render(&ab::OutputWish { channels: 2, ..Default::default() }).expect("a default render endpoint");
        let paired = backend.capture_with_render(&render_plan.endpoint);
        println!("default render {} pairs with a capture endpoint by container: {paired}", render_plan.endpoint.name);
        let capture_plan = backend.plan_capture(&ab::InputWish::default()).expect("a default capture endpoint");
        let origin = ab::HostOrigin::default();
        let (render, capture) = (Arc::new(Tally::default()), Arc::new(Tally::default()));
        // The capture side is probed on its own endpoint: the paired one, else the default one.
        let capture_endpoint = if paired {
            let id = backend.paired_input(&render_plan.endpoint).expect("the paired input").id;
            backend.endpoints(ab::Direction::Input).expect("inputs").into_iter().find(|e| e.id == id).expect("the paired input endpoint")
        } else {
            capture_plan.endpoint.clone()
        };
        let like = if paired { backend.capture_format(&render_plan.endpoint, None).expect("format") } else { capture_plan.attempts[0].format };
        let capture_request = exclusive_request(&backend, &capture_endpoint, like, origin);
        let (capture_stream, capture_achieved) = or_in_use(
            backend.open_capture(&capture_endpoint, &capture_request, capture_callback(&capture), fault_callback(&capture)),
            "open exclusive capture",
        );
        let render_request = exclusive_request(&backend, &render_plan.endpoint, render_plan.attempts[0].format, origin);
        let (render_stream, render_achieved) = or_in_use(
            backend.open_render(&render_plan.endpoint, &render_request, render_callback(&render), fault_callback(&render)),
            "open exclusive render",
        );
        print_achieved("exclusive capture", &capture_achieved);
        print_achieved("exclusive render", &render_achieved);
        or_in_use(capture_stream.start(), "start exclusive capture");
        or_in_use(render_stream.start(), "start exclusive render");
        std::thread::sleep(Duration::from_secs(2));
        drop((render_stream, capture_stream));
        let (rb, cb) = (render.blocks.load(Ordering::Relaxed), capture.blocks.load(Ordering::Relaxed));
        assert!(rb > 0 && cb > 0, "both streams ran ({rb} render, {cb} capture blocks)");
        assert_eq!(render.faults.load(Ordering::Relaxed) + capture.faults.load(Ordering::Relaxed), 0);
        assert_eq!(render.invalid.load(Ordering::Relaxed) + capture.invalid.load(Ordering::Relaxed), 0, "stamps with an unreadable clock");
        let (render_offset, capture_offset) = (median(&mut render.offsets.lock().unwrap()), median(&mut capture.offsets.lock().unwrap()));
        println!("host - backend offset: render {render_offset} ns, capture {capture_offset} ns");
        assert!((render_offset - capture_offset).abs() < 5_000_000, "the two streams' stamps are not comparable in one host origin");
    }

    #[test]
    #[ignore = "probes the default audio endpoints in exclusive mode"]
    fn hardware_exclusive_probe_lists_formats() {
        let backend = WasapiBackend::default();
        let render = backend.plan_render(&ab::OutputWish { channels: 2, ..Default::default() }).expect("a default render endpoint").endpoint;
        let capture = backend.plan_capture(&ab::InputWish::default()).expect("a default capture endpoint").endpoint;
        for endpoint in [render, capture] {
            let own = endpoint.outputs.max(endpoint.inputs);
            for channels in [2, own] {
                let formats = or_in_use(backend.exclusive_formats(&endpoint, 48_000, &[channels]), "probe");
                println!("{} [{}] at 48 kHz first, {channels} ch: {} formats", endpoint.name, endpoint.id, formats.len());
                for f in &formats {
                    println!("  {} Hz {:?} valid {:?} mask {:?}", f.rate, f.sample, f.valid_bits, f.channel_mask);
                }
            }
        }
    }

    #[test]
    #[ignore = "enumerates the audio endpoints"]
    fn hardware_endpoints_list_names_formats_and_the_period_floor() {
        for direction in [ab::Direction::Output, ab::Direction::Input] {
            for endpoint in WasapiBackend::default().endpoints(direction).expect("endpoints") {
                println!(
                    "{direction:?} {} [{}]: {} ch, formats {:?}",
                    endpoint.name,
                    endpoint.id,
                    endpoint.outputs.max(endpoint.inputs),
                    endpoint.formats
                );
            }
        }
    }
}
