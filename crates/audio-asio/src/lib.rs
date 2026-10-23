//! The native ASIO backend (ADR 0016 amendment, draft "native ASIO"), slices (a) and (b): the
//! driver list, opening and closing a driver, and render and capture streams through the driver's
//! buffer-switch callback. 64-bit Windows only: on every other target the crate is empty.
//!
//! - The bindings are our own ([`abi`]): `#[repr(C)]` structures and the `IASIO` vtable with
//!   compile-time layout assertions. No ASIO SDK download, no C++ and no bindgen in the build.
//! - Every driver call happens on one dedicated STA host thread ([`host`]) that owns the driver
//!   object and a hidden message-only window passed as `sysRef`.
//! - [`AsioBackend::drivers`] lists the installed drivers from the registry without loading any;
//!   [`AsioBackend::probe`] loads, initializes, queries and unloads one (ASIO allows one loaded
//!   driver per process), the one the app selects, and keeps what it found for the listing. A
//!   driver that cannot be probed is reported by [`AsioBackend::diagnostics`] and a `log::warn!`.
//! - [`AsioBackend::load`] keeps one driver loaded and initialized, with no buffers created, for
//!   queries, the sample rate and the control panel; [`Loaded::close`] (or dropping it) releases it.
//! - `open_render` and `open_capture` open the two directions of one driver's [`session`]: one
//!   `createBuffers` for both, the driver started by the first stream's `start`. Open both before
//!   starting either (the open-order rule, [`session`]). The callback runs on the driver's thread
//!   ([`callback`]); its work ([`engine`]) converts ([`convert`]), stamps and counts ([`stats`]).
//!   A driver that stops calling back while a stream should run is caught by the host thread's
//!   [`watchdog`] and reported as `StreamError::Stalled`.
//! - [`AsioFactory`] hands out one process-wide backend: one host thread and one enumeration cache.
//!
//! - The buffer-switch thread joins the MMCSS task "Pro Audio" at its first switch after each
//!   driver start and leaves it at the stop ([`mmcss`]); the host thread runs above normal priority.
//!
//! This is a sanctioned `unsafe` crate (ADR 0013 and the draft amendment): all `unsafe` is in
//! [`com`] (COM, registry, window, vtable, MMCSS and thread-priority calls) and [`callback`] (the
//! driver's callbacks and the buffers they touch), each block with a `// SAFETY:` comment.
#![cfg(all(windows, target_pointer_width = "64"))]
// The callback modules (`callback`, `engine`, `convert`, `stats`) deny these lints; control-thread
// code may allocate and format.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

pub mod abi;
mod callback;
mod com;
mod convert;
pub mod driver;
mod engine;
mod host;
mod mmcss;
mod session;
mod stats;
#[cfg(test)]
mod stream_tests;
mod watchdog;

pub use driver::{
    BufferSize, DirectionType, DriverDiagnostic, DriverEntry, DriverInfo, PROBE_RATES, ResolvedDriver, SampleType, resolve_driver_name,
};

use host::{Host, Loader, Slot};
use session::{Callback, SideOwner};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use transit_audio_backend as ab;

/// The system name every endpoint of this backend carries (the one saved configurations use).
pub const SYSTEM: &str = "ASIO";
/// How long a caller waits for the host thread before a request fails (a blocking driver).
pub const HOST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a caller waits for a control panel call (some panels are modal).
const PANEL_TIMEOUT: Duration = Duration::from_secs(600);
/// A driver's probe is reused this long, so a plan right after the listing loads it once.
const ENUMERATION_REUSE: Duration = Duration::from_secs(5);

/// Creates the native ASIO backend for the system named "ASIO": every call hands out the same
/// process-wide backend ([`AsioBackend::shared`]), so repeated calls share one host thread and one
/// per-driver probe cache.
#[derive(Debug, Clone, Copy, Default)]
pub struct AsioFactory;

impl ab::BackendFactory for AsioFactory {
    fn backend(&self, system: Option<&str>) -> Option<Box<dyn ab::Backend>> {
        if system.is_some_and(|s| s != SYSTEM) {
            return None;
        }
        match AsioBackend::shared() {
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

/// What the last probe of one driver found ([`AsioBackend::probe`]).
#[derive(Clone)]
struct Probed {
    at: Instant,
    entry: DriverEntry,
    result: Result<DriverInfo, DriverDiagnostic>,
}

/// The native ASIO audio system. `Send` and `Sync`: every driver call is a request to the host
/// thread. Clones share the host thread and the per-driver probe cache.
///
/// Listing instantiates nothing: the registry names come from [`AsioBackend::drivers`], and a
/// driver is loaded, queried and unloaded only when it is probed ([`AsioBackend::probe`], the
/// driver the app selects) or planned (`plan_render`/`plan_capture` of it, which reuse a probe
/// younger than five seconds). [`ab::Backend::endpoints`] lists the probed drivers.
#[derive(Clone)]
pub struct AsioBackend {
    host: Arc<Host>,
    /// The last probe of each driver, in the order they were first probed.
    probes: Arc<Mutex<Vec<Probed>>>,
}

impl AsioBackend {
    /// Starts a host thread of its own over the registry and the real drivers. The app uses
    /// [`AsioBackend::shared`] (through [`AsioFactory`]).
    pub fn new() -> Result<Self, ab::OpenError> {
        Self::with_loader(Box::new(com::ComLoader), Slot::process(), HOST_TIMEOUT)
    }

    /// The process's one backend, started on first use and kept for the life of the process (a
    /// failed start is retried by the next call). Dropping a host can block for up to
    /// [`HOST_TIMEOUT`], so the shared one is never dropped.
    pub fn shared() -> Result<Self, ab::OpenError> {
        static SHARED: Mutex<Option<AsioBackend>> = Mutex::new(None);
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(backend) = shared.as_ref() {
            return Ok(backend.clone());
        }
        let backend = Self::new()?;
        *shared = Some(backend.clone());
        Ok(backend)
    }

    fn with_loader(loader: Box<dyn Loader>, slot: Slot, timeout: Duration) -> Result<Self, ab::OpenError> {
        Self::with_loader_watching(loader, slot, timeout, watchdog::STALL_FLOOR)
    }

    /// [`Self::with_loader`] with `watchdog_floor` as the shortest stall limit (tests that drive
    /// the callbacks by hand use a long one).
    fn with_loader_watching(loader: Box<dyn Loader>, slot: Slot, timeout: Duration, watchdog_floor: Duration) -> Result<Self, ab::OpenError> {
        Ok(Self { host: Arc::new(Host::spawn_watching(loader, slot, timeout, watchdog_floor)?), probes: Arc::default() })
    }

    /// The installed drivers from the registry, nothing loaded.
    pub fn drivers(&self) -> Result<Vec<DriverEntry>, ab::OpenError> {
        self.host.call("list the ASIO drivers", |s| s.list())
    }

    /// Probes the driver registered as `name`, and only it: loads, queries and unloads it (the
    /// driver this process holds open is queried in place), and keeps what it found for the
    /// listing ([`ab::Backend::endpoints`], [`AsioBackend::diagnostics`]) and the plans. A name
    /// that is not installed is an error listing the installed drivers.
    pub fn probe(&self, name: &str) -> Result<DriverInfo, ab::OpenError> {
        let installed = self.drivers()?;
        let Some(entry) = installed.iter().find(|e| e.name == name).cloned() else {
            let names: Vec<String> = installed.into_iter().map(|e| e.name).collect();
            return Err(ab::OpenError::DeviceNotAvailable(driver::not_installed(name, &names)));
        };
        self.probe_entry(&entry)
            .map_err(|d| if d.blocked { ab::OpenError::InUse(d.problem) } else { ab::OpenError::DeviceNotAvailable(d.to_string()) })
    }

    /// What the last probe of `name` found, if it was probed: its report, or why it failed.
    pub fn probed(&self, name: &str) -> Option<Result<DriverInfo, DriverDiagnostic>> {
        self.probes.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|p| p.entry.name == name).map(|p| p.result.clone())
    }

    /// Loads, queries and unloads every installed driver, one at a time (a driver this backend
    /// holds open is queried in place). Failures become [`AsioBackend::diagnostics`]. The app
    /// never does this (it probes the selected driver only); the hardware listing test does.
    pub fn probe_all(&self) -> Result<Vec<(DriverEntry, DriverInfo)>, ab::OpenError> {
        let mut found = Vec::new();
        let mut blocked: Option<String> = None;
        for entry in self.drivers()? {
            let result = match &blocked {
                Some(stuck) => {
                    let problem = format!("not probed: the host thread is blocked in {stuck}");
                    let diagnostic = DriverDiagnostic { driver: entry.name.clone(), problem, blocked: false };
                    log::warn!("ASIO: {diagnostic}");
                    self.remember(&entry, Err(diagnostic.clone()));
                    Err(diagnostic)
                }
                None => self.probe_entry(&entry),
            };
            match result {
                Ok(info) => found.push((entry, info)),
                Err(_) if blocked.is_none() && self.host.is_stuck() => blocked = Some(format!("the probe of \"{}\"", entry.name)),
                Err(_) => {}
            }
        }
        Ok(found)
    }

    /// The probed drivers that could not be listed, with why.
    pub fn diagnostics(&self) -> Vec<DriverDiagnostic> {
        self.probes.lock().unwrap_or_else(PoisonError::into_inner).iter().filter_map(|p| p.result.clone().err()).collect()
    }

    /// Probes `entry` (see [`AsioBackend::probe`]) and keeps the result.
    fn probe_entry(&self, entry: &DriverEntry) -> Result<DriverInfo, DriverDiagnostic> {
        let result = if entry.dll_exists {
            let probe = entry.clone();
            match self.host.call(&format!("probe of \"{}\"", entry.name), move |s| s.probe(&probe)) {
                Ok(info) => {
                    for direction in [ab::Direction::Input, ab::Direction::Output] {
                        let t = info.sample_type(direction);
                        if info.channels(direction) > 0 && t.format().is_none() {
                            log::info!("ASIO: \"{}\" {direction:?} sample type {} is not supported", entry.name, t.describe());
                        }
                    }
                    Ok(info)
                }
                Err(e) => Err((e.to_string(), matches!(e, ab::OpenError::InUse(_)))),
            }
        } else {
            let problem = match &entry.dll {
                Some(dll) => format!("driver DLL not found: {dll}"),
                None => format!("no InprocServer32 registered for CLSID {}", entry.clsid_text),
            };
            Err((problem, false))
        };
        let result = result.map_err(|(problem, blocked)| {
            let diagnostic = DriverDiagnostic { driver: entry.name.clone(), problem, blocked };
            if blocked {
                log::info!("ASIO: {diagnostic}");
            } else {
                log::warn!("ASIO: {diagnostic}");
            }
            diagnostic
        });
        self.remember(entry, result.clone());
        result
    }

    fn remember(&self, entry: &DriverEntry, result: Result<DriverInfo, DriverDiagnostic>) {
        let probed = Probed { at: Instant::now(), entry: entry.clone(), result };
        let mut probes = self.probes.lock().unwrap_or_else(PoisonError::into_inner);
        match probes.iter_mut().find(|p| p.entry.name == entry.name) {
            Some(slot) => *slot = probed,
            None => probes.push(probed),
        }
    }

    /// A successful probe of `entry` younger than [`ENUMERATION_REUSE`] (a plan right after the
    /// listing probed it), else a new probe.
    fn recent(&self, entry: &DriverEntry) -> Result<DriverInfo, DriverDiagnostic> {
        let reused = self
            .probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|p| p.entry.name == entry.name && p.at.elapsed() < ENUMERATION_REUSE)
            .and_then(|p| p.result.clone().ok());
        match reused {
            Some(info) => Ok(info),
            None => self.probe_entry(entry),
        }
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

    /// The registry names of the installed drivers.
    pub fn driver_names(&self) -> Result<Vec<String>, ab::OpenError> {
        Ok(self.drivers()?.into_iter().map(|d| d.name).collect())
    }

    /// Why `name` cannot be an ASIO driver of this machine (it is not installed), listing the
    /// installed ones; `None` when it is installed or the registry could not be read.
    pub fn missing_driver(&self, name: &str) -> Option<String> {
        let installed = self.driver_names().ok()?;
        (!installed.iter().any(|n| n == name)).then(|| driver::not_installed(name, &installed))
    }

    /// The endpoint of the driver named `device` (its registry name, exactly), else the first
    /// driver in registry order that loads with `direction` channels. Only the drivers asked about
    /// are probed (a probe younger than five seconds is reused). A named driver that is not
    /// installed, failed to load or has no `direction` channels is an error saying so: never
    /// another driver.
    fn find(&self, direction: ab::Direction, device: Option<&str>) -> Result<ab::Endpoint, String> {
        let installed = self.drivers().map_err(|e| e.to_string())?;
        let usable = |entry: &DriverEntry| -> Result<ab::Endpoint, String> {
            let info = self.recent(entry).map_err(|d| format!("ASIO driver \"{}\" could not be opened: {}", d.driver, d.problem))?;
            if info.channels(direction) == 0 {
                return Err(format!("ASIO driver \"{}\" has no {direction:?} channels", entry.name));
            }
            Ok(Self::endpoint(entry, &info, direction))
        };
        if let Some(wanted) = device {
            let names = || installed.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
            let entry = installed.iter().find(|e| e.name == wanted).ok_or_else(|| driver::not_installed(wanted, &names()))?;
            return usable(entry);
        }
        installed.iter().find_map(|entry| usable(entry).ok()).ok_or_else(|| format!("no ASIO driver offers {direction:?} channels"))
    }

    /// Opens one direction of the endpoint's driver (see [`session`] for the open-order rule).
    fn open_stream(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        callback: Callback,
        error: ab::ErrorCallback,
    ) -> Result<(AsioStream, ab::Achieved), ab::OpenError> {
        let device = endpoint.handle::<AsioEndpoint>().ok_or_else(|| ab::OpenError::Other("the endpoint is not an ASIO driver".into()))?;
        let entry = device.entry.clone();
        let name = entry.name.clone();
        let direction = match &callback {
            Callback::Render(_) => ab::Direction::Output,
            Callback::Capture(_) => ab::Direction::Input,
        };
        let what = if direction == ab::Direction::Output { "render" } else { "capture" };
        let request_copy = *request;
        let opened = self.host.call(&format!("open {what} of \"{name}\""), move |s| s.open_stream(&entry, &request_copy, callback, error))?;
        let latency = opened.achieved.latency;
        let achieved = opened.achieved;
        let stream = AsioStream {
            host: Arc::clone(&self.host),
            name,
            generation: opened.generation,
            direction,
            origin: request.origin,
            latency,
            system_step: opened.system_step,
            mmcss: opened.mmcss,
            owner: opened.owner,
        };
        Ok((stream, achieved))
    }
}

/// One direction of a driver's session. Every control call is a request to the host thread;
/// dropping it detaches the direction (and ends the session with its last stream), which can block
/// for up to [`HOST_TIMEOUT`]: drop it on a control thread, never in a callback.
pub(crate) struct AsioStream {
    host: Arc<Host>,
    name: String,
    generation: u64,
    direction: ab::Direction,
    origin: ab::HostOrigin,
    latency: Option<Duration>,
    system_step: Arc<AtomicU64>,
    mmcss: Arc<mmcss::Mmcss>,
    owner: SideOwner,
}

impl AsioStream {
    fn what(&self) -> &'static str {
        if self.direction == ab::Direction::Output { "render" } else { "capture" }
    }

    /// The smallest step between consecutive callbacks' `systemTime` seen so far, in ns (an upper
    /// bound on the grain of the driver's clock), `None` before two valid readings.
    pub(crate) fn system_time_step_ns(&self) -> Option<u64> {
        Some(self.system_step.load(Ordering::Relaxed)).filter(|step| *step != u64::MAX)
    }
}

impl ab::Stream for AsioStream {
    /// Starts this direction, and the driver with the first one. A stream starts once.
    fn start(&self) -> Result<(), ab::OpenError> {
        let pair = callback::origin_pair(self.origin);
        let (name, generation, direction) = (self.name.clone(), self.generation, self.direction);
        self.host.call(&format!("start {} of \"{}\"", self.what(), self.name), move |s| s.start_stream(&name, generation, direction, pair))
    }

    /// Stops this direction for good (a stopped stream is reopened, not restarted); the driver
    /// stops when no stream of it runs.
    ///
    /// From inside a driver callback (this stream's own, say) or on the host thread, the stop
    /// cannot wait for the host thread, and the driver's `stop` must not be called from inside its
    /// own buffer switch: the direction stops at once (no later buffer switch calls it) and the
    /// host thread finishes the stop when the callback has returned. It never blocks there.
    fn stop(&self) -> Result<(), ab::OpenError> {
        if callback::in_driver_callback() || self.host.on_host_thread() {
            self.owner.0.request_stop();
            self.host.wake();
            return Ok(());
        }
        let (name, generation, direction) = (self.name.clone(), self.generation, self.direction);
        self.host.call(&format!("stop {} of \"{}\"", self.what(), self.name), move |s| s.stop_stream(&name, generation, direction))
    }

    fn latency(&self) -> Option<Duration> {
        self.latency
    }

    /// The callback runs on the driver's own thread, which its first buffer switch after a start
    /// registers with MMCSS "Pro Audio" ([`mmcss`]): `Pending` until then, `MmcssProAudio` (or
    /// `PromotionFailed`) while it runs, `Released` after the stop; `DriverRegistered` (before and
    /// after the stop) when the driver had registered that thread itself (error 1552);
    /// `Unobserved` when the driver calls back on the host thread.
    fn thread_priority(&self) -> ab::WorkerPriority {
        self.mmcss.priority()
    }

    fn stats(&self) -> Option<ab::StreamStats> {
        Some(self.owner.0.stats.snapshot())
    }

    /// Takes effect at the next buffer switch (an atomic hand-off; never blocks or calls the host
    /// thread). The next stamp of this direction is invalid. A loop shorter than the driver's output
    /// figure is not used (the driver's figures are, and the stream's close logs a warning).
    fn set_calibration(&self, calibration: Option<ab::Calibration>) -> bool {
        self.owner.0.set_calibration(calibration);
        true
    }
}

impl Drop for AsioStream {
    fn drop(&mut self) {
        if let Some(step) = self.system_time_step_ns() {
            log::debug!("ASIO: \"{}\" {}: the driver's systemTime moved in steps of at least {step} ns", self.name, self.what());
        }
        if self.owner.0.calibration_rejected() {
            log::warn!(
                "ASIO: \"{}\" {}: the calibration's loop was shorter than the driver's output latency, so it was not used (the driver's figures were); measure the loop again",
                self.name,
                self.what()
            );
        }
        let (name, generation, direction) = (self.name.clone(), self.generation, self.direction);
        let label = format!("close {} of \"{}\"", self.what(), self.name);
        if let Err(e) = self.host.call(&label, move |s| s.detach_stream(&name, generation, direction)) {
            log::warn!("ASIO: {label}: {e}");
        }
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
        let (i, o) = self.call("getChannels", |d| d.channels().map(|c| (c.inputs(), c.outputs())))?;
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

    /// The probed drivers ([`AsioBackend::probe`]) with channels in `direction`, in the order they
    /// were first probed; listing instantiates no driver. A direction whose sample type is
    /// unsupported lists no formats; the type is in the [`AsioEndpoint`] handle. Drivers whose
    /// probe failed are in [`AsioBackend::diagnostics`].
    fn endpoints(&self, direction: ab::Direction) -> Result<Vec<ab::Endpoint>, ab::OpenError> {
        let probes = self.probes.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(probes
            .iter()
            .filter_map(|p| p.result.as_ref().ok().map(|info| (&p.entry, info)))
            .filter(|(_, info)| info.channels(direction) > 0)
            .map(|(entry, info)| Self::endpoint(entry, info, direction))
            .collect())
    }

    fn host_blocked(&self) -> bool {
        self.host.is_stuck()
    }

    /// A driver name that is not in the registry: the app says so instead of opening another.
    fn missing_device(&self, name: &str) -> Option<String> {
        self.missing_driver(name)
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

    /// `getChannelInfo`'s names, as the driver reported them when it was probed.
    fn channel_names(&self, endpoint: &ab::Endpoint, direction: ab::Direction) -> Vec<String> {
        endpoint.handle::<AsioEndpoint>().map(|device| device.info.channel_names(direction).to_vec()).unwrap_or_default()
    }

    /// Opens the driver's output (its first `format.channels` channels). Open the capture stream
    /// of the same driver before either starts, if one is wanted (see [`session`]).
    fn open_render(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        render: ab::RenderCallback,
        error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        let (stream, achieved) = self.open_stream(endpoint, request, Callback::Render(render), error)?;
        Ok((Box::new(stream), achieved))
    }

    /// Opens the driver's input (its first `format.channels` channels), in either order with the
    /// render stream, before either starts.
    fn open_capture(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        capture: ab::CaptureCallback,
        error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        let (stream, achieved) = self.open_stream(endpoint, request, Callback::Capture(capture), error)?;
        Ok((Box::new(stream), achieved))
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
        assert_eq!(backend.driver_names().unwrap(), ["Model Mixer ASIO Driver", "Focusrite USB ASIO"]);
        assert!(backend.endpoints(ab::Direction::Output).expect("outputs").is_empty(), "nothing probed yet");
        assert_eq!(observed.created.load(Ordering::Acquire), 0, "listing the names instantiates no driver");
        backend.probe("Model Mixer ASIO Driver").expect("probe");
        assert_eq!(observed.created.load(Ordering::Acquire), 1, "only the probed driver");
        assert_eq!(backend.endpoints(ab::Direction::Output).expect("outputs").len(), 1, "the probed driver only");
        backend.probe("Focusrite USB ASIO").expect("probe");
        let outputs = backend.endpoints(ab::Direction::Output).expect("outputs");
        let inputs = backend.endpoints(ab::Direction::Input).expect("inputs");
        assert_eq!(observed.created.load(Ordering::Acquire), 2, "both directions came from the cache, no new probe");
        assert_eq!(observed.max_live.load(Ordering::Acquire), 1, "one driver loaded at a time");
        assert_eq!(observed.live.load(Ordering::Acquire), 0, "each unloaded after its probe");
        assert!(backend.probed("Focusrite USB ASIO").is_some_and(|p| p.is_ok()));
        assert!(backend.probed("Other").is_none());
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
        let (ins, outs) = (backend.channel_names(m, ab::Direction::Input), backend.channel_names(m, ab::Direction::Output));
        assert_eq!((ins.len(), outs.len()), (12, 10), "every channel is named");
        assert_eq!((ins[11].as_str(), outs[9].as_str()), ("Analog In 12", "Analog Out 10"), "getChannelInfo's names, in channel order");
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
        assert_eq!(backend.probe_all().expect("enumeration does not fail").len(), 3);
        let inputs = backend.endpoints(ab::Direction::Input).expect("listing");
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
        assert!(diagnostics.iter().all(|d| !d.blocked), "each of these failed on its own");
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
        backend.probe_all().expect("enumeration while one is open");
        let outputs = backend.endpoints(ab::Direction::Output).expect("listing");
        assert_eq!(outputs.len(), 1, "the open driver is listed, the other is not probed");
        assert!(backend.diagnostics()[0].problem.contains("one driver per process"));
        assert!(backend.diagnostics()[0].blocked, "not probed because another driver is open: not a failure of that driver");
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
        let (loader, observed) = FakeLoader::new(two_interfaces());
        let backend = backend(loader);
        let wish = ab::OutputWish { device: Some("Focusrite USB ASIO".into()), rates: vec![96_000, 22_050], channels: 2, period: Some(64) };
        let plan = backend.plan_render(&wish).expect("plan");
        assert_eq!(plan.endpoint.name, "Focusrite USB ASIO");
        assert_eq!(observed.created.load(Ordering::Acquire), 1, "a named plan probes that driver only");
        backend.plan_render(&wish).expect("again");
        assert_eq!(observed.created.load(Ordering::Acquire), 1, "a recent probe is reused");
        let rates: Vec<u32> = plan.attempts.iter().map(|a| a.format.rate).collect();
        assert_eq!(rates, [96_000, 48_000]);
        assert!(plan.attempts.iter().all(|a| a.period == ab::Period::Frames(64) && a.format.sample == ab::SampleFormat::I32));
        let capture = backend.plan_capture(&ab::InputWish::default()).expect("capture plan");
        assert_eq!(capture.endpoint.name, "Model Mixer ASIO Driver");
        assert_eq!(capture.attempts[0].format.channels, 12);
    }

    #[test]
    fn a_named_driver_that_is_not_installed_or_failed_is_an_error_never_another_driver() {
        let mut drivers = two_interfaces();
        let mut absent = FakeSpec::new("Absent", 2, 2, abi::ASIOST_INT32_LSB);
        absent.init_ok = false;
        absent.init_message = "hardware not found".into();
        drivers.push((entry("Absent", 12), absent));
        let (loader, _) = FakeLoader::new(drivers);
        let backend = backend(loader);
        let wish = |device: &str| ab::OutputWish { device: Some(device.into()), rates: vec![48_000], channels: 2, period: None };
        let Err(missing) = backend.plan_render(&wish("TASCAM Model 12 ASIO")) else { panic!("not installed") };
        assert_eq!(
            missing,
            "no ASIO driver named \"TASCAM Model 12 ASIO\" is installed; the installed ASIO drivers are: \"Model Mixer ASIO Driver\", \"Focusrite USB ASIO\", \"Absent\""
        );
        assert_eq!(backend.missing_driver("TASCAM Model 12 ASIO"), Some(missing));
        assert_eq!(ab::Backend::missing_device(&backend, "Focusrite USB ASIO"), None);
        let Err(failed) = backend.plan_capture(&ab::InputWish { device: Some("Absent".into()), rate: None, period: None }) else { panic!("failed") };
        assert!(failed.starts_with("ASIO driver \"Absent\" could not be opened: ") && failed.contains("hardware not found"), "{failed}");
        assert_eq!(backend.plan_render(&wish("Focusrite USB ASIO")).expect("installed").endpoint.name, "Focusrite USB ASIO");
        assert_eq!(backend.driver_names().unwrap(), ["Model Mixer ASIO Driver", "Focusrite USB ASIO", "Absent"]);
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

    /// What two seconds of duplex streaming saw.
    #[derive(Default)]
    struct Tally {
        render: std::sync::atomic::AtomicU64,
        capture: std::sync::atomic::AtomicU64,
        invalid: std::sync::atomic::AtomicU64,
        errors: std::sync::atomic::AtomicU64,
        /// The first render and capture stamps' (backend callback, frame) times.
        first: Mutex<Vec<(ab::Direction, ab::Stamp)>>,
    }

    /// Opens `driver`'s capture then render at `rate` and `period` (every channel), runs two
    /// seconds of silence and checks the callbacks, stamps and counters.
    fn stream_two_seconds(driver: &str, rate: u32, period: Option<u32>) {
        use std::sync::atomic::Ordering::Relaxed;
        let backend = AsioBackend::new().expect("backend");
        let name = resolve_driver_name(&backend.driver_names().expect("registry"), driver).unwrap_or_else(|e| panic!("{e}")).name;
        let endpoint = backend.find(ab::Direction::Output, Some(&name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let info = endpoint.handle::<AsioEndpoint>().expect("handle").info.clone();
        let wish = ab::OutputWish { device: Some(endpoint.name.clone()), rates: vec![rate], channels: info.outputs, period };
        let render_request = backend.plan_render(&wish).expect("plan").attempts[0];
        let capture_request =
            backend.plan_capture(&ab::InputWish { device: Some(endpoint.name.clone()), rate: Some(rate), period }).expect("plan").attempts[0];
        assert_eq!((render_request.format.rate, capture_request.format.rate), (rate, rate), "never another rate on this interface");
        let tally = Arc::new(Tally::default());
        let (t1, t2, t3) = (Arc::clone(&tally), Arc::clone(&tally), Arc::clone(&tally));
        let capture: ab::CaptureCallback = Box::new(move |block| {
            if t1.capture.fetch_add(1, Relaxed) == 0
                && let Ok(mut first) = t1.first.try_lock()
            {
                first.push((ab::Direction::Input, block.stamp));
            }
            if block.stamp.quality == ab::Quality::Invalid {
                t1.invalid.fetch_add(1, Relaxed);
            }
            block.chunks(&mut |_, _| {});
        });
        let render: ab::RenderCallback = Box::new(move |block| {
            if t2.render.fetch_add(1, Relaxed) == 0
                && let Ok(mut first) = t2.first.try_lock()
            {
                first.push((ab::Direction::Output, block.stamp));
            }
            if block.stamp.quality == ab::Quality::Invalid {
                t2.invalid.fetch_add(1, Relaxed);
            }
            block.render(&mut |chunk| chunk.fill(0.0));
        });
        let error = move || -> ab::ErrorCallback {
            let t = Arc::clone(&t3);
            Box::new(move |_| {
                t.errors.fetch_add(1, Relaxed);
            })
        };
        let (capture, cap) = backend.open_stream(&endpoint, &capture_request, Callback::Capture(capture), error()).expect("open capture");
        let (render, ren) = backend.open_stream(&endpoint, &render_request, Callback::Render(render), error()).expect("open render");
        println!("{driver}: capture {cap:?}");
        println!("{driver}: render {ren:?}");
        ab::Stream::start(&capture).expect("start capture");
        ab::Stream::start(&render).expect("start render");
        // Count from the first callback on: a driver's start can take a good part of a second
        // before its first buffer switch (the Model 12), which is not a stall.
        let waiting = Instant::now();
        while tally.render.load(Relaxed) == 0 && waiting.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(1));
        }
        println!("{driver}: first buffer switch {:?} after the start", waiting.elapsed());
        let (r0, c0, started) = (tally.render.load(Relaxed), tally.capture.load(Relaxed), Instant::now());
        std::thread::sleep(Duration::from_secs(2));
        let (r1, c1, elapsed) = (tally.render.load(Relaxed), tally.capture.load(Relaxed), started.elapsed());
        let (rs, cs) = (ab::Stream::stats(&render).expect("stats"), ab::Stream::stats(&capture).expect("stats"));
        let running = ab::Stream::thread_priority(&render);
        ab::Stream::stop(&render).expect("stop render");
        ab::Stream::stop(&capture).expect("stop capture");
        let stopped = ab::Stream::thread_priority(&render);
        println!("{driver}: buffer-switch thread MMCSS while running {running:?}, after the stop {stopped:?}");
        println!("{driver}: render backend: {}{}", rs.line(false), rs.events_tail());
        println!("{driver}: capture backend: {}{}", cs.line(true), cs.events_tail());
        println!("{driver}: systemTime step {:?} ns; first stamps {:?}", render.system_time_step_ns(), tally.first.lock().map(|f| f.clone()));
        drop(render);
        drop(capture);
        let expected = expected_callbacks(elapsed, ren.format.rate, ren.period.frames().expect("frames"));
        let (r, c) = (r1 - r0, c1 - c0);
        println!(
            "{driver}: {r} render and {c} capture callbacks in {elapsed:?} after the first; about {expected:.0} expected at {} Hz, {} frames",
            ren.format.rate,
            ren.period.frames().unwrap_or(0)
        );
        assert!(r as f64 > expected * 0.8 && (r as f64) < expected * 1.2, "render callbacks {r} against {expected:.0}");
        assert!(c as f64 > expected * 0.8, "capture callbacks {c}");
        assert_eq!((tally.invalid.load(Relaxed), tally.errors.load(Relaxed)), (0, 0), "invalid stamps, stream errors");
        assert_eq!(rs.overloads, 0);
        assert_mmcss(driver, running, stopped);
    }

    /// How many buffer switches `elapsed` holds at the stream's own rate and buffer size.
    fn expected_callbacks(elapsed: Duration, rate: u32, frames: u32) -> f64 {
        elapsed.as_secs_f64() * f64::from(rate) / f64::from(frames.max(1))
    }

    #[test]
    fn the_expected_callback_count_follows_the_streams_rate_and_buffer() {
        let two = Duration::from_secs(2);
        assert_eq!(expected_callbacks(two, 44_100, 256).round(), 345.0, "the Model 12");
        assert_eq!(expected_callbacks(two, 48_000, 64).round(), 1500.0, "the Scarlett at 64 frames");
        assert_eq!(expected_callbacks(two, 48_000, 256).round(), 375.0);
    }

    /// The buffer-switch thread either joined "Pro Audio" through us and left it at the stop, or
    /// the driver had registered it itself (error 1552) and keeps it: both are success. Prints
    /// which.
    fn assert_mmcss(driver: &str, running: ab::WorkerPriority, stopped: ab::WorkerPriority) {
        use ab::WorkerPriority as P;
        match (running, stopped) {
            (P::MmcssProAudio, P::Released) => println!("{driver}: MMCSS: registered by transit, released at the stop"),
            (P::DriverRegistered, P::DriverRegistered) => println!("{driver}: MMCSS: the driver registered its own thread (error 1552)"),
            (P::Unobserved, _) => panic!("{driver}: the driver called back on the host thread (no MMCSS attempt): {running:?}, {stopped:?}"),
            other => panic!("{driver}: MMCSS while running and after the stop: {other:?}"),
        }
    }

    #[test]
    #[ignore = "streams the Focusrite ASIO driver at 48 kHz, 64 frames, for two seconds"]
    fn hardware_scarlett_duplex_streams_two_seconds() {
        stream_two_seconds(FOCUSRITE, 48_000, Some(64));
    }

    #[test]
    #[ignore = "streams the Model 12 ASIO driver at 44.1 kHz (its pinned rate), fixed 256 frames, for two seconds"]
    fn hardware_model12_duplex_streams_two_seconds() {
        stream_two_seconds(MODEL12, 44_100, None);
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
