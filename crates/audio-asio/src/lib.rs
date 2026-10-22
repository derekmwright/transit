//! The native ASIO backend (ADR 0016 amendment, draft "native ASIO"), slice (a): the driver list,
//! and opening and closing a driver without streaming. 64-bit Windows only: on every other target
//! the crate is empty.
//!
//! - The bindings are our own ([`abi`]): `#[repr(C)]` structures and the `IASIO` vtable with
//!   compile-time layout assertions. No ASIO SDK download, no C++ and no bindgen in the build.
//! - Every driver call happens on one dedicated STA host thread ([`host`]) that owns the driver
//!   object and a hidden message-only window passed as `sysRef`.
//! - [`AsioBackend::endpoints`] loads, initializes, queries and unloads each installed driver in
//!   turn (one at a time: ASIO allows one loaded driver per process). A driver that cannot be
//!   listed is reported by [`AsioBackend::diagnostics`] and a `log::warn!`, never dropped silently.
//! - [`AsioBackend::load`] keeps one driver loaded and initialized, with no buffers created, for
//!   queries, the sample rate and the control panel; [`Loaded::close`] (or dropping it) releases it.
//!
//! Streaming (`open_render`, `open_capture`) is slice (b): those calls return an error today.
//!
//! This is a sanctioned `unsafe` crate (ADR 0013 and the draft amendment): all `unsafe` is in
//! [`com`], each block with a `// SAFETY:` comment.
#![cfg(all(windows, target_pointer_width = "64"))]
// No audio callback exists in this slice; control-thread code may allocate and format.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

pub mod abi;
mod com;
pub mod driver;
mod host;

pub use driver::{BufferSize, DirectionType, DriverDiagnostic, DriverEntry, DriverInfo, PROBE_RATES, SampleType};

use host::{Host, Loader, Slot};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use transit_audio_backend as ab;

/// The system name every endpoint of this backend carries (the one saved configurations use).
pub const SYSTEM: &str = "ASIO";
/// How long a caller waits for the host thread before a request fails (a blocking driver).
pub const HOST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a caller waits for a control panel call (some panels are modal).
const PANEL_TIMEOUT: Duration = Duration::from_secs(600);
/// An enumeration is reused for this long, so listing both directions loads each driver once.
const ENUMERATION_REUSE: Duration = Duration::from_secs(5);
/// What `open_render` and `open_capture` answer until slice (b).
const NOT_YET: &str = "the native ASIO backend does not stream yet (ADR 0016 amendment, ASIO slice (b))";

/// Creates the native ASIO backend for the system named "ASIO".
#[derive(Debug, Clone, Copy, Default)]
pub struct AsioFactory;

impl ab::BackendFactory for AsioFactory {
    fn backend(&self, system: Option<&str>) -> Option<Box<dyn ab::Backend>> {
        if system.is_some_and(|s| s != SYSTEM) {
            return None;
        }
        match AsioBackend::new() {
            Ok(backend) => Some(Box::new(backend)),
            Err(e) => {
                log::warn!("ASIO: the native backend did not start: {e}");
                None
            }
        }
    }
}

/// The device reference an ASIO endpoint carries: the registry entry and what the driver
/// reported when it was probed (sample types per direction included, supported or not).
#[derive(Debug, Clone)]
pub struct AsioEndpoint {
    pub entry: DriverEntry,
    pub info: DriverInfo,
}

#[derive(Clone, Default)]
struct Enumeration {
    drivers: Vec<(DriverEntry, DriverInfo)>,
    diagnostics: Vec<DriverDiagnostic>,
}

/// The native ASIO audio system. `Send` and `Sync`: every driver call is a request to the host thread.
pub struct AsioBackend {
    host: Arc<Host>,
    last: Mutex<Option<(Instant, Enumeration)>>,
}

impl AsioBackend {
    /// Starts the host thread over the registry and the real drivers.
    pub fn new() -> Result<Self, ab::OpenError> {
        Self::with_loader(Box::new(com::ComLoader), Slot::process(), HOST_TIMEOUT)
    }

    fn with_loader(loader: Box<dyn Loader>, slot: Slot, timeout: Duration) -> Result<Self, ab::OpenError> {
        Ok(Self { host: Arc::new(Host::spawn(loader, slot, timeout)?), last: Mutex::new(None) })
    }

    /// The installed drivers from the registry, nothing loaded.
    pub fn drivers(&self) -> Result<Vec<DriverEntry>, ab::OpenError> {
        self.host.call("list the ASIO drivers", |s| s.list())
    }

    /// Loads, queries and unloads every installed driver, one at a time (a driver this backend
    /// holds open is queried in place). Failures become [`AsioBackend::diagnostics`].
    pub fn probe_all(&self) -> Result<Vec<(DriverEntry, DriverInfo)>, ab::OpenError> {
        let enumeration = self.enumerate()?;
        let drivers = enumeration.drivers.clone();
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), enumeration));
        Ok(drivers)
    }

    /// The drivers the last enumeration could not list, with why.
    pub fn diagnostics(&self) -> Vec<DriverDiagnostic> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|(_, e)| e.diagnostics.clone()).unwrap_or_default()
    }

    fn enumerate(&self) -> Result<Enumeration, ab::OpenError> {
        let mut out = Enumeration::default();
        let entries = self.drivers()?;
        let mut blocked: Option<String> = None;
        for entry in entries {
            let problem = if let Some(stuck) = &blocked {
                Some(format!("not probed: the host thread is blocked in {stuck}"))
            } else if !entry.dll_exists {
                Some(match &entry.dll {
                    Some(dll) => format!("driver DLL not found: {dll}"),
                    None => format!("no InprocServer32 registered for CLSID {}", entry.clsid_text),
                })
            } else {
                let probe = entry.clone();
                match self.host.call(&format!("probe of \"{}\"", entry.name), move |s| s.probe(&probe)) {
                    Ok(info) => {
                        for direction in [ab::Direction::Input, ab::Direction::Output] {
                            let t = info.sample_type(direction);
                            if info.channels(direction) > 0 && t.format().is_none() {
                                log::info!("ASIO: \"{}\" {direction:?} sample type {} is not supported", entry.name, t.describe());
                            }
                        }
                        out.drivers.push((entry.clone(), info));
                        None
                    }
                    Err(e) => {
                        if self.host.is_stuck() {
                            blocked = Some(format!("the probe of \"{}\"", entry.name));
                        }
                        Some(e.to_string())
                    }
                }
            };
            if let Some(problem) = problem {
                let diagnostic = DriverDiagnostic { driver: entry.name.clone(), problem };
                log::warn!("ASIO: {diagnostic}");
                out.diagnostics.push(diagnostic);
            }
        }
        Ok(out)
    }

    /// The last enumeration if it is recent, else a new one.
    fn recent(&self) -> Result<Enumeration, ab::OpenError> {
        if let Some((at, e)) = self.last.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
            && at.elapsed() < ENUMERATION_REUSE
        {
            return Ok(e.clone());
        }
        let e = self.enumerate()?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), e.clone()));
        Ok(e)
    }

    /// Loads and initializes the driver registered as `name` and keeps it loaded (no buffers)
    /// until [`Loaded::close`] or drop. Fails with [`ab::OpenError::InUse`] while another driver
    /// is loaded in this process.
    pub fn load(&self, name: &str) -> Result<Loaded, ab::OpenError> {
        let entry = self
            .drivers()?
            .into_iter()
            .find(|e| e.name == name)
            .ok_or_else(|| ab::OpenError::DeviceNotAvailable(format!("no ASIO driver named \"{name}\" is installed")))?;
        if !entry.dll_exists {
            return Err(ab::OpenError::DeviceNotAvailable(format!(
                "ASIO driver \"{name}\": DLL not found ({})",
                entry.dll.as_deref().unwrap_or("no InprocServer32 registered")
            )));
        }
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let load = entry.clone();
        let info = self.host.call(&format!("load of \"{name}\""), move |s| s.load(&load))?;
        Ok(Loaded { host: Arc::clone(&self.host), name: name.to_owned(), info, open: true })
    }

    fn endpoint(entry: &DriverEntry, info: &DriverInfo, direction: ab::Direction) -> ab::Endpoint {
        ab::Endpoint::new(
            entry.name.clone(),
            entry.name.clone(),
            SYSTEM.into(),
            info.inputs,
            info.outputs,
            info.format_ranges(direction),
            Arc::new(AsioEndpoint { entry: entry.clone(), info: info.clone() }),
        )
    }

    fn find(&self, direction: ab::Direction, device: Option<&str>) -> Result<ab::Endpoint, String> {
        let endpoints = ab::Backend::endpoints(self, direction).map_err(|e| e.to_string())?;
        let chosen = device.and_then(|d| endpoints.iter().find(|e| e.name == d || e.id == d)).or_else(|| endpoints.first());
        chosen.cloned().ok_or_else(|| format!("no ASIO driver offers {direction:?} channels"))
    }
}

/// A loaded and initialized driver (no buffers). Every call goes to the host thread.
pub struct Loaded {
    host: Arc<Host>,
    name: String,
    info: DriverInfo,
    open: bool,
}

impl Loaded {
    /// The registry name of the driver.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the driver reported when it was loaded.
    pub fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn call<R: Send + 'static>(
        &self,
        what: &str,
        call: impl FnOnce(&mut com::Driver) -> Result<R, i32> + Send + 'static,
    ) -> Result<R, ab::OpenError> {
        let (name, method) = (self.name.clone(), what.to_owned());
        self.host.call(&format!("{what} of \"{}\"", self.name), move |s| s.with_driver(&name, &method, call))
    }

    /// `getChannels`: (inputs, outputs).
    pub fn channels(&self) -> Result<(u16, u16), ab::OpenError> {
        let (i, o) = self.call("getChannels", com::Driver::channels)?;
        let clamp = |n: i32| u16::try_from(n.max(0)).unwrap_or(u16::MAX);
        Ok((clamp(i), clamp(o)))
    }

    /// `getLatencies`: (input, output) in frames.
    pub fn latencies(&self) -> Result<(u32, u32), ab::OpenError> {
        let (i, o) = self.call("getLatencies", com::Driver::latencies)?;
        Ok((i.max(0) as u32, o.max(0) as u32))
    }

    /// `getBufferSize`.
    pub fn buffer_size(&self) -> Result<BufferSize, ab::OpenError> {
        let name = self.name.clone();
        self.host.call(&format!("getBufferSize of \"{}\"", self.name), move |s| s.buffer_size(&name))
    }

    /// `getSampleRate`.
    pub fn sample_rate(&self) -> Result<f64, ab::OpenError> {
        self.call("getSampleRate", com::Driver::sample_rate)
    }

    /// `canSampleRate`.
    pub fn can_sample_rate(&self, rate: u32) -> Result<bool, ab::OpenError> {
        self.call("canSampleRate", move |d| Ok(d.can_sample_rate(f64::from(rate)).is_ok()))
    }

    /// `setSampleRate`, then `getSampleRate`: the rate the driver runs at afterwards.
    pub fn set_sample_rate(&self, rate: u32) -> Result<f64, ab::OpenError> {
        self.call("setSampleRate", move |d| {
            d.set_sample_rate(f64::from(rate))?;
            d.sample_rate()
        })
    }

    /// `controlPanel`: opens the driver's own settings (modal or a window of its own).
    pub fn control_panel(&self) -> Result<(), ab::OpenError> {
        let name = self.name.clone();
        self.host.call_within(&format!("controlPanel of \"{}\"", self.name), PANEL_TIMEOUT, move |s| {
            s.with_driver(&name, "controlPanel", com::Driver::control_panel)
        })
    }

    /// Queries everything again (as at load).
    pub fn refresh(&mut self) -> Result<&DriverInfo, ab::OpenError> {
        let name = self.name.clone();
        self.info = self.host.call(&format!("query of \"{}\"", self.name), move |s| s.query(&name))?;
        Ok(&self.info)
    }

    /// Releases the driver on the host thread (disposing buffers first, had any been created).
    pub fn close(mut self) -> Result<(), ab::OpenError> {
        self.unload()
    }

    fn unload(&mut self) -> Result<(), ab::OpenError> {
        if !std::mem::replace(&mut self.open, false) {
            return Ok(());
        }
        let name = self.name.clone();
        self.host.call(&format!("unload of \"{}\"", self.name), move |s| s.unload(&name))
    }
}

impl Drop for Loaded {
    fn drop(&mut self) {
        if let Err(e) = self.unload() {
            log::warn!("ASIO: closing \"{}\": {e}", self.name);
        }
    }
}

impl ab::Backend for AsioBackend {
    fn system(&self) -> &str {
        SYSTEM
    }

    /// The drivers with channels in `direction`, each probed (loaded, queried, unloaded) in turn.
    /// A direction whose sample type is unsupported lists no formats; the type is in the
    /// [`AsioEndpoint`] handle. Drivers that fail are in [`AsioBackend::diagnostics`].
    fn endpoints(&self, direction: ab::Direction) -> Result<Vec<ab::Endpoint>, ab::OpenError> {
        let e = self.recent()?;
        Ok(e.drivers.iter().filter(|(_, info)| info.channels(direction) > 0).map(|(entry, info)| Self::endpoint(entry, info, direction)).collect())
    }

    fn plan_render(&self, wish: &ab::OutputWish) -> Result<ab::Plan, String> {
        let endpoint = self.find(ab::Direction::Output, wish.device.as_deref())?;
        let attempts = attempts(&endpoint, ab::Direction::Output, &wish.rates, wish.period)?;
        Ok(ab::Plan { endpoint, attempts })
    }

    fn plan_capture(&self, wish: &ab::InputWish) -> Result<ab::Plan, String> {
        let endpoint = self.find(ab::Direction::Input, wish.device.as_deref())?;
        let attempts = attempts(&endpoint, ab::Direction::Input, &wish.rate.into_iter().collect::<Vec<_>>(), wish.period)?;
        Ok(ab::Plan { endpoint, attempts })
    }

    /// One driver, one set of buffers for both directions.
    fn capture_with_render(&self, render: &ab::Endpoint) -> bool {
        render.handle::<AsioEndpoint>().is_some()
    }

    fn capture_format(&self, endpoint: &ab::Endpoint, rate: Option<u32>) -> Result<ab::Format, String> {
        let device = endpoint.handle::<AsioEndpoint>().ok_or("the endpoint is not an ASIO driver")?;
        format_of(&device.info, ab::Direction::Input, rate)
    }

    fn open_render(
        &self,
        _endpoint: &ab::Endpoint,
        _request: &ab::OpenRequest,
        _render: ab::RenderCallback,
        _error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        Err(ab::OpenError::Other(NOT_YET.into()))
    }

    fn open_capture(
        &self,
        _endpoint: &ab::Endpoint,
        _request: &ab::OpenRequest,
        _capture: ab::CaptureCallback,
        _error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        Err(ab::OpenError::Other(NOT_YET.into()))
    }
}

/// The format of one direction at `rate` (else the driver's current rate).
fn format_of(info: &DriverInfo, direction: ab::Direction, rate: Option<u32>) -> Result<ab::Format, String> {
    let (sample, valid_bits) = info.sample_type(direction).format().ok_or_else(|| {
        format!("ASIO driver \"{}\": {direction:?} sample type {} is not supported", info.name, info.sample_type(direction).describe())
    })?;
    let rate = rate
        .or_else(|| info.current_rate.filter(|r| r.is_finite() && *r >= 1.0).map(|r| r.round() as u32))
        .or_else(|| info.rates.first().copied())
        .ok_or_else(|| format!("ASIO driver \"{}\" reports no sample rate", info.name))?;
    Ok(ab::Format { rate, channels: info.channels(direction), sample, valid_bits, channel_mask: None })
}

/// The requests to try: the wished rates the driver accepts, then its current rate.
fn attempts(endpoint: &ab::Endpoint, direction: ab::Direction, wished: &[u32], period: Option<u32>) -> Result<Vec<ab::OpenRequest>, String> {
    let device = endpoint.handle::<AsioEndpoint>().ok_or("the endpoint is not an ASIO driver")?;
    let mut rates: Vec<u32> = wished.iter().copied().filter(|r| device.info.rates.contains(r)).collect();
    let current = format_of(&device.info, direction, None)?.rate;
    if !rates.contains(&current) {
        rates.push(current);
    }
    let period = period.map_or(ab::Period::Default, ab::Period::Frames);
    rates
        .into_iter()
        .map(|rate| {
            Ok(ab::OpenRequest {
                format: format_of(&device.info, direction, Some(rate))?,
                period,
                // ASIO has one mode: the driver owns the device and shares it (or not) itself.
                mode: ab::Mode::Shared,
                origin: ab::HostOrigin::default(),
                calibration: None,
            })
        })
        .collect()
}

const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<AsioBackend>();
    shareable::<Loaded>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::fake::{FakeLoader, FakeSpec, entry};
    use ab::Backend;
    use std::sync::atomic::Ordering;

    fn backend(loader: FakeLoader) -> AsioBackend {
        AsioBackend::with_loader(Box::new(loader), Slot::default(), Duration::from_secs(10)).expect("backend")
    }

    fn two_interfaces() -> Vec<(DriverEntry, FakeSpec)> {
        let mut model12 = FakeSpec::new("Model Mixer ASIO", 12, 10, abi::ASIOST_INT24_LSB);
        model12.rates = vec![44_100.0, 48_000.0];
        model12.current_rate = 44_100.0;
        let mut scarlett = FakeSpec::new("Focusrite USB ASIO", 2, 2, abi::ASIOST_INT32_LSB);
        scarlett.rates = vec![44_100.0, 48_000.0, 88_200.0, 96_000.0];
        vec![(entry("Model Mixer ASIO Driver", 10), model12), (entry("Focusrite USB ASIO", 11), scarlett)]
    }

    #[test]
    fn enumeration_maps_drivers_to_endpoints_per_direction() {
        let (loader, observed) = FakeLoader::new(two_interfaces());
        let backend = backend(loader);
        let outputs = backend.endpoints(ab::Direction::Output).expect("outputs");
        let inputs = backend.endpoints(ab::Direction::Input).expect("inputs");
        assert_eq!(observed.created.load(Ordering::Acquire), 2, "both directions came from one probe pass");
        assert_eq!(observed.max_live.load(Ordering::Acquire), 1, "one driver loaded at a time");
        assert_eq!(observed.live.load(Ordering::Acquire), 0, "each unloaded after its probe");
        assert_eq!(outputs.len(), 2);
        let m = &outputs[0];
        assert_eq!((m.id.as_str(), m.name.as_str(), m.system.as_str()), ("Model Mixer ASIO Driver", "Model Mixer ASIO Driver", "ASIO"));
        assert_eq!((m.inputs, m.outputs), (12, 10));
        assert_eq!(m.formats.len(), 2);
        assert!(m.formats.iter().all(|f| f.sample == ab::SampleFormat::I24 && f.channels == 10 && f.period == Some((32, 2048))));
        assert_eq!(m.formats.iter().map(|f| f.min_rate).collect::<Vec<_>>(), [44_100, 48_000]);
        let handle = m.handle::<AsioEndpoint>().expect("an ASIO handle");
        assert_eq!(handle.info.driver_name, "Model Mixer ASIO");
        assert_eq!(handle.info.output_type, DirectionType::Uniform(SampleType::Int24Lsb));
        assert_eq!(handle.info.latencies, Some((300, 400)));
        assert_eq!(handle.info.buffer, BufferSize { min: 32, max: 2048, preferred: 256, granularity: -1 });
        assert_eq!(inputs[0].formats[0].channels, 12);
        assert_eq!(inputs[1].formats.len(), 4, "the Scarlett accepts all four probed rates");
        assert!(backend.diagnostics().is_empty());
        let format = backend.capture_format(m, Some(48_000)).expect("a capture format");
        assert_eq!(format, ab::Format { rate: 48_000, channels: 12, sample: ab::SampleFormat::I24, valid_bits: None, channel_mask: None });
        assert!(backend.capture_with_render(m));
    }

    #[test]
    fn failing_and_unsupported_drivers_are_reported_not_dropped() {
        let mut drivers = two_interfaces();
        let mut absent = FakeSpec::new("Absent", 2, 2, abi::ASIOST_INT32_LSB);
        absent.init_ok = false;
        absent.init_message = "hardware not found".into();
        drivers.push((entry("Absent", 12), absent));
        let mut missing = entry("Missing DLL", 13);
        missing.dll_exists = false;
        drivers.push((missing, FakeSpec::new("Missing", 1, 1, abi::ASIOST_INT32_LSB)));
        drivers.push((entry("Unregistered", 14), FakeSpec::new("Unregistered", 1, 1, abi::ASIOST_INT32_LSB)));
        let mut msb = FakeSpec::new("Big Endian", 2, 2, abi::ASIOST_INT32_MSB);
        msb.output_type = abi::ASIOST_FLOAT32_LSB;
        drivers.push((entry("Big Endian", 15), msb));
        let (mut loader, _) = FakeLoader::new(drivers);
        loader.create_fails.push("Unregistered".into());
        let backend = backend(loader);
        let inputs = backend.endpoints(ab::Direction::Input).expect("enumeration does not fail");
        let names: Vec<&str> = inputs.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Model Mixer ASIO Driver", "Focusrite USB ASIO", "Big Endian"]);
        let big = &inputs[2];
        assert!(big.formats.is_empty(), "an unsupported sample type lists no format");
        let handle = big.handle::<AsioEndpoint>().expect("handle");
        assert_eq!(handle.info.input_type, DirectionType::Uniform(SampleType::Unsupported(abi::ASIOST_INT32_MSB)));
        assert!(backend.capture_format(big, None).unwrap_err().contains("ASIOSTInt32MSB"));
        let outputs = backend.endpoints(ab::Direction::Output).expect("outputs");
        assert_eq!(outputs[2].formats[0].sample, ab::SampleFormat::F32, "the other direction is still usable");
        let diagnostics = backend.diagnostics();
        let text: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        assert_eq!(diagnostics.len(), 3, "{text:?}");
        assert!(text[0].starts_with("Absent: ") && text[0].contains("init failed: hardware not found"), "{text:?}");
        assert!(text[1].starts_with("Missing DLL: ") && text[1].contains("DLL not found"), "{text:?}");
        assert!(text[2].starts_with("Unregistered: ") && text[2].contains("CoCreateInstance"), "{text:?}");
    }

    #[test]
    fn load_keeps_the_driver_open_until_close_and_blocks_others() {
        let (loader, observed) = FakeLoader::new(two_interfaces());
        let backend = backend(loader);
        let loaded = backend.load("Model Mixer ASIO Driver").expect("loads");
        assert_eq!(loaded.info().inputs, 12);
        assert_eq!(loaded.channels().expect("channels"), (12, 10));
        assert_eq!(loaded.latencies().expect("latencies"), (300, 400));
        assert_eq!(loaded.buffer_size().expect("buffer").preferred, 256);
        assert_eq!(loaded.sample_rate().expect("rate"), 44_100.0);
        assert!(loaded.can_sample_rate(48_000).expect("can"));
        assert!(!loaded.can_sample_rate(96_000).expect("cannot"));
        assert_eq!(loaded.set_sample_rate(48_000).expect("set"), 48_000.0);
        let refused = loaded.set_sample_rate(96_000).unwrap_err();
        assert!(matches!(&refused, ab::OpenError::Other(t) if t.contains("setSampleRate") && t.contains("ASE_NoClock")), "{refused:?}");
        loaded.control_panel().expect("panel");
        assert_eq!(observed.panels.load(Ordering::Acquire), 1);
        let other = backend.load("Focusrite USB ASIO").err().expect("a second driver is refused");
        assert!(matches!(&other, ab::OpenError::InUse(t) if t.contains("Model Mixer ASIO Driver")), "{other:?}");
        let outputs = backend.endpoints(ab::Direction::Output).expect("enumeration while one is open");
        assert_eq!(outputs.len(), 1, "the open driver is listed, the other is not probed");
        assert!(backend.diagnostics()[0].problem.contains("one driver per process"));
        assert_eq!(observed.live.load(Ordering::Acquire), 1);
        loaded.close().expect("closes");
        assert_eq!(observed.live.load(Ordering::Acquire), 0);
        let again = backend.load("Focusrite USB ASIO").expect("the other loads after close");
        drop(again);
        assert_eq!(observed.live.load(Ordering::Acquire), 0, "drop closes too");
        let missing = backend.load("Nope").err().expect("unknown name");
        assert!(matches!(missing, ab::OpenError::DeviceNotAvailable(_)));
    }

    #[test]
    fn plans_offer_the_wished_rates_the_driver_accepts_then_its_current_rate() {
        let (loader, _) = FakeLoader::new(two_interfaces());
        let backend = backend(loader);
        let wish = ab::OutputWish { device: Some("Focusrite USB ASIO".into()), rates: vec![96_000, 22_050], channels: 2, period: Some(64) };
        let plan = backend.plan_render(&wish).expect("plan");
        assert_eq!(plan.endpoint.name, "Focusrite USB ASIO");
        let rates: Vec<u32> = plan.attempts.iter().map(|a| a.format.rate).collect();
        assert_eq!(rates, [96_000, 48_000]);
        assert!(plan.attempts.iter().all(|a| a.period == ab::Period::Frames(64) && a.format.sample == ab::SampleFormat::I32));
        let capture = backend.plan_capture(&ab::InputWish::default()).expect("capture plan");
        assert_eq!(capture.endpoint.name, "Model Mixer ASIO Driver");
        assert_eq!(capture.attempts[0].format.channels, 12);
        let request = plan.attempts[0];
        let open = backend.open_render(&plan.endpoint, &request, Box::new(|_| {}), Box::new(|_| {}));
        assert!(matches!(open.err(), Some(ab::OpenError::Other(t)) if t.contains("slice (b)")));
    }
}

#[cfg(test)]
mod hardware {
    //! Tests against the installed ASIO drivers. They are `#[ignore]`d: the owner runs them with
    //! `cargo test -p transit-audio-asio -- --ignored hardware_ --nocapture --test-threads 1`.
    //! A driver another app holds may report "in use" or `ASE_NotPresent`: that is recorded, not
    //! retried.
    use super::*;
    use ab::Backend;

    const FOCUSRITE: &str = "Focusrite USB ASIO";
    /// The Model 12's registry key is "Model Mixer ASIO Driver"; its driver name is "Model Mixer ASIO".
    const MODEL12: &str = "Model Mixer ASIO";

    fn reference_drivers(backend: &AsioBackend) -> Vec<DriverEntry> {
        let drivers = backend.drivers().expect("registry");
        drivers.into_iter().filter(|d| d.name == FOCUSRITE || d.name.starts_with(MODEL12)).collect()
    }

    #[test]
    #[ignore = "loads every installed ASIO driver"]
    fn hardware_lists_focusrite_and_tascam() {
        let backend = AsioBackend::new().expect("backend");
        let all = backend.probe_all().expect("enumeration");
        for (entry, info) in &all {
            println!(
                "{} [{}] \"{}\" v{}: {} in ({}), {} out ({}); buffer {:?}; rates {:?}; current {:?}; latencies {:?}",
                entry.name,
                entry.clsid_text,
                info.driver_name,
                info.driver_version,
                info.inputs,
                info.input_type.describe(),
                info.outputs,
                info.output_type.describe(),
                info.buffer,
                info.rates,
                info.current_rate,
                info.latencies
            );
        }
        for d in backend.diagnostics() {
            println!("diagnostic: {d}");
        }
        let find = |name: &str| all.iter().find(|(e, _)| e.name == name || e.name.starts_with(name)).map(|(_, i)| i.clone());
        let scarlett = find(FOCUSRITE).expect("the Focusrite driver is listed");
        assert_eq!((scarlett.inputs, scarlett.outputs), (2, 2));
        assert!(scarlett.rates.contains(&48_000) && scarlett.rates.contains(&44_100), "{:?}", scarlett.rates);
        let model12 = find(MODEL12).expect("the Model 12 driver is listed");
        assert_eq!((model12.inputs, model12.outputs), (12, 10));
        assert_eq!(model12.input_type, DirectionType::Uniform(SampleType::Int24Lsb));
        assert_eq!(model12.output_type, DirectionType::Uniform(SampleType::Int24Lsb));
        assert!(model12.rates.contains(&48_000) && model12.rates.contains(&44_100), "{:?}", model12.rates);
        let outputs = backend.endpoints(ab::Direction::Output).expect("endpoints");
        assert!(outputs.iter().any(|e| e.name.starts_with(MODEL12) && e.formats.iter().all(|f| f.sample == ab::SampleFormat::I24)));
    }

    #[test]
    #[ignore = "loads the Focusrite and Model 12 ASIO drivers"]
    fn hardware_open_close_each_driver_twice() {
        let backend = AsioBackend::new().expect("backend");
        let drivers = reference_drivers(&backend);
        assert_eq!(drivers.len(), 2, "both reference drivers are installed: {drivers:?}");
        for round in 1..=2 {
            for entry in &drivers {
                let started = Instant::now();
                let loaded = backend.load(&entry.name).unwrap_or_else(|e| panic!("round {round}: {}: {e}", entry.name));
                let info = loaded.info().clone();
                println!(
                    "round {round}: {} loaded in {:?}: {} in / {} out, {}, buffer {:?}, rate {:?}, latencies {:?}",
                    entry.name,
                    started.elapsed(),
                    info.inputs,
                    info.outputs,
                    info.output_type.describe(),
                    loaded.buffer_size().expect("getBufferSize"),
                    loaded.sample_rate().expect("getSampleRate"),
                    loaded.latencies().expect("getLatencies")
                );
                let other = drivers.iter().find(|e| e.name != entry.name).expect("the other");
                let refused = backend.load(&other.name).err().expect("a second driver is refused");
                assert!(matches!(refused, ab::OpenError::InUse(_)), "{refused:?}");
                loaded.close().unwrap_or_else(|e| panic!("round {round}: close {}: {e}", entry.name));
            }
        }
    }

    #[test]
    #[ignore = "opens the Focusrite driver's control panel (a window the owner must close)"]
    fn hardware_control_panel_opens_and_returns() {
        let backend = AsioBackend::new().expect("backend");
        let loaded = backend.load(FOCUSRITE).expect("loads");
        loaded.control_panel().expect("controlPanel");
        loaded.close().expect("closes");
    }
}
