//! The ASIO host thread: one dedicated thread that owns the driver object and makes every `IASIO`
//! call. Other threads send it requests (list, load and init, query, set the rate, open the
//! panel, unload) and wait for the answer with a timeout.
//!
//! Why one thread: drivers register as apartment-threaded in-process COM servers, so their object
//! belongs to the single-threaded apartment that created it, and the ASIO SDK expects every call
//! on the thread that called `init`. Drivers also post window messages to the `sysRef` handle
//! given to `init` and create windows (control panels) on the calling thread, so that thread must
//! own a window and pump messages while idle. The host thread initializes COM as STA, creates a
//! hidden message-only window (`HWND_MESSAGE`) as `sysRef`, and between requests dispatches its
//! messages and sleeps on an event that callers set after queuing a request.
//!
//! One driver per process: the ASIO model allows one loaded driver at a time, so every load
//! claims a process-wide [`Slot`]; a second load (on this host or another) fails with
//! [`ab::OpenError::InUse`] naming the driver that holds it.
//!
//! A driver call that never returns (a driver blocking in `init`) wedges the host thread; the
//! caller's request fails after the timeout instead of hanging, later requests fail at once while
//! the thread is still blocked, and a load whose caller gave up is undone when the call finally
//! returns.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use crate::com::{Driver, Event, MessageWindow, StaGuard};
use crate::driver::{self, BufferSize, DriverEntry, DriverInfo, asio_error};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use transit_audio_backend as ab;

/// Where drivers come from: the registry and `CoCreateInstance` ([`crate::com::ComLoader`]), or
/// fakes in tests. Called on the host thread only.
pub(crate) trait Loader: Send + 'static {
    fn list(&self) -> Result<Vec<DriverEntry>, ab::OpenError>;
    fn create(&self, entry: &DriverEntry) -> Result<Driver, ab::OpenError>;
}

/// The one-loaded-driver rule: holds the name of the driver loaded in this process.
#[derive(Clone, Default)]
pub(crate) struct Slot(Arc<Mutex<Option<String>>>);

impl Slot {
    /// The process-wide slot every real host shares.
    pub(crate) fn process() -> Self {
        static PROCESS: OnceLock<Slot> = OnceLock::new();
        PROCESS.get_or_init(Slot::default).clone()
    }

    fn claim(&self, name: &str) -> Result<SlotGuard, String> {
        let mut holder = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = holder.as_ref() {
            return Err(held.clone());
        }
        *holder = Some(name.to_owned());
        Ok(SlotGuard(self.clone()))
    }
}

/// Frees the slot when the driver is released.
struct SlotGuard(Slot);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// The loaded driver. Field order matters: the driver is released before the slot is freed.
struct Loaded {
    entry: DriverEntry,
    driver: Driver,
    /// Whether `createBuffers` succeeded (never in slice a); `unload` then stops and disposes.
    buffers_created: bool,
    _slot: SlotGuard,
}

/// The state of the host thread; requests run against it on that thread.
pub(crate) struct HostState {
    loader: Box<dyn Loader>,
    slot: Slot,
    sys_ref: *mut c_void,
    loaded: Option<Loaded>,
}

impl HostState {
    pub(crate) fn list(&self) -> Result<Vec<DriverEntry>, ab::OpenError> {
        self.loader.list()
    }

    fn loaded_name(&self) -> Option<String> {
        self.loaded.as_ref().map(|l| l.entry.name.clone())
    }

    /// Loads and initializes `entry` (no buffers) and queries it.
    pub(crate) fn load(&mut self, entry: &DriverEntry) -> Result<DriverInfo, ab::OpenError> {
        if let Some(open) = self.loaded_name() {
            return Err(ab::OpenError::InUse(format!(
                "ASIO driver \"{open}\" is already open; ASIO allows one driver per process, so \"{}\" cannot open until it is closed",
                entry.name
            )));
        }
        let guard = self.slot.claim(&entry.name).map_err(|held| {
            ab::OpenError::InUse(format!(
                "ASIO driver \"{held}\" is open elsewhere in this process; ASIO allows one driver per process, so \"{}\" cannot open",
                entry.name
            ))
        })?;
        let mut driver = self.loader.create(entry)?;
        if !driver.init(self.sys_ref) {
            let message = driver.error_message();
            drop(driver);
            let detail = if message.is_empty() { String::new() } else { format!(": {message}") };
            return Err(ab::OpenError::DeviceNotAvailable(format!("ASIO driver \"{}\": init failed{detail}", entry.name)));
        }
        let info = driver::query(&entry.name, &mut driver)?;
        self.loaded = Some(Loaded { entry: entry.clone(), driver, buffers_created: false, _slot: guard });
        Ok(info)
    }

    /// Loads, queries and unloads `entry`, or queries it in place when it is the loaded driver.
    pub(crate) fn probe(&mut self, entry: &DriverEntry) -> Result<DriverInfo, ab::OpenError> {
        if let Some(loaded) = self.loaded.as_mut()
            && loaded.entry.name == entry.name
        {
            return driver::query(&entry.name, &mut loaded.driver);
        }
        let info = self.load(entry)?;
        self.unload(&entry.name)?;
        Ok(info)
    }

    /// Releases the loaded driver `name` (stopping and disposing buffers first if any existed).
    pub(crate) fn unload(&mut self, name: &str) -> Result<(), ab::OpenError> {
        match self.loaded_name() {
            Some(open) if open == name => {
                self.release();
                Ok(())
            }
            _ => Err(not_open(name)),
        }
    }

    fn release(&mut self) {
        if let Some(mut loaded) = self.loaded.take() {
            if loaded.buffers_created {
                if let Err(code) = loaded.driver.stop() {
                    log::warn!("{}", asio_error(&loaded.entry.name, "stop", code));
                }
                if let Err(code) = loaded.driver.dispose_buffers() {
                    log::warn!("{}", asio_error(&loaded.entry.name, "disposeBuffers", code));
                }
            }
            drop(loaded);
        }
    }

    /// Runs `call` on the loaded driver `name`; `what` names the method for errors.
    pub(crate) fn with_driver<R>(&mut self, name: &str, what: &str, call: impl FnOnce(&mut Driver) -> Result<R, i32>) -> Result<R, ab::OpenError> {
        match self.loaded.as_mut() {
            Some(loaded) if loaded.entry.name == name => call(&mut loaded.driver).map_err(|code| asio_error(name, what, code)),
            _ => Err(not_open(name)),
        }
    }

    pub(crate) fn query(&mut self, name: &str) -> Result<DriverInfo, ab::OpenError> {
        match self.loaded.as_mut() {
            Some(loaded) if loaded.entry.name == name => driver::query(name, &mut loaded.driver),
            _ => Err(not_open(name)),
        }
    }

    pub(crate) fn buffer_size(&mut self, name: &str) -> Result<BufferSize, ab::OpenError> {
        self.with_driver(name, "getBufferSize", Driver::buffer_size)
    }
}

fn not_open(name: &str) -> ab::OpenError {
    ab::OpenError::Other(format!("ASIO driver \"{name}\" is not open on this host"))
}

/// A queued request: runs on the host thread and returns true when its caller had given up.
type Job = Box<dyn FnOnce(&mut HostState) -> bool + Send>;
/// A request and its label (for errors and the "still blocked in" message).
type Envelope = (String, Job);

struct Shared {
    quit: AtomicBool,
    /// Set by a caller whose request timed out, cleared when the host finishes that request.
    stuck: AtomicBool,
    /// The label of the request the host is serving.
    busy: Mutex<Option<String>>,
}

/// The answer cell of one request: the caller waits on it, the host fills it unless the caller
/// gave up first (decided under the lock, so exactly one side wins).
struct Answer<R> {
    state: Mutex<(Option<R>, bool)>,
    ready: Condvar,
}

/// A handle to a running host thread. Dropping it unloads any driver and ends the thread.
pub(crate) struct Host {
    tx: Option<Sender<Envelope>>,
    wake: Arc<Event>,
    shared: Arc<Shared>,
    timeout: Duration,
    /// Behind a mutex so the handle is `Sync` (a `Receiver` is not).
    exited: Mutex<Receiver<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Host {
    /// Starts the host thread: STA, message window, then the request loop.
    pub(crate) fn spawn(loader: Box<dyn Loader>, slot: Slot, timeout: Duration) -> Result<Self, ab::OpenError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let (started_tx, started_rx) = mpsc::channel();
        let (exited_tx, exited) = mpsc::channel();
        let wake = Arc::new(Event::new()?);
        let shared = Arc::new(Shared { quit: AtomicBool::new(false), stuck: AtomicBool::new(false), busy: Mutex::new(None) });
        let (thread_wake, thread_shared) = (Arc::clone(&wake), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("transit-asio-host".into())
            .spawn(move || {
                run(loader, slot, &rx, &thread_wake, &thread_shared, &started_tx);
                let _ = exited_tx.send(());
            })
            .map_err(|e| ab::OpenError::Other(format!("could not start the ASIO host thread: {e}")))?;
        match started_rx.recv_timeout(timeout) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err(ab::OpenError::Other("the ASIO host thread did not start".into())),
        }
        Ok(Self { tx: Some(tx), wake, shared, timeout, exited: Mutex::new(exited), thread: Some(thread) })
    }

    /// Runs `request` on the host thread and waits up to the host's timeout for its answer.
    pub(crate) fn call<R: Send + 'static>(
        &self,
        label: &str,
        request: impl FnOnce(&mut HostState) -> Result<R, ab::OpenError> + Send + 'static,
    ) -> Result<R, ab::OpenError> {
        self.call_within(label, self.timeout, request)
    }

    /// [`Host::call`] with its own timeout (a modal control panel may stay open for minutes).
    pub(crate) fn call_within<R: Send + 'static>(
        &self,
        label: &str,
        timeout: Duration,
        request: impl FnOnce(&mut HostState) -> Result<R, ab::OpenError> + Send + 'static,
    ) -> Result<R, ab::OpenError> {
        if self.shared.stuck.load(Ordering::Acquire) {
            let busy = self.shared.busy.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_else(|| "a driver call".into());
            return Err(ab::OpenError::Other(format!("the ASIO host thread is still blocked in {busy}; {label} was not sent")));
        }
        let answer = Arc::new(Answer::<Result<R, ab::OpenError>> { state: Mutex::new((None, false)), ready: Condvar::new() });
        let host_answer = Arc::clone(&answer);
        let what = label.to_owned();
        let job: Job = Box::new(move |state| {
            let result = catch_unwind(AssertUnwindSafe(|| request(state)))
                .unwrap_or_else(|_| Err(ab::OpenError::Other(format!("the ASIO host thread panicked while serving {what}"))));
            let mut cell = host_answer.state.lock().unwrap_or_else(PoisonError::into_inner);
            if cell.1 {
                return true;
            }
            cell.0 = Some(result);
            host_answer.ready.notify_all();
            false
        });
        let Some(tx) = &self.tx else { return Err(exited()) };
        tx.send((label.to_owned(), job)).map_err(|_| exited())?;
        self.wake.set();
        let cell = answer.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut cell, _) = answer.ready.wait_timeout_while(cell, timeout, |c| c.0.is_none()).unwrap_or_else(PoisonError::into_inner);
        if let Some(result) = cell.0.take() {
            return result;
        }
        cell.1 = true;
        self.shared.stuck.store(true, Ordering::Release);
        Err(ab::OpenError::Other(format!(
            "the ASIO host thread did not answer {label} within {} s: a driver call is blocking",
            timeout.as_secs_f32()
        )))
    }

    /// Whether a request timed out and the host thread has not finished it yet.
    pub(crate) fn is_stuck(&self) -> bool {
        self.shared.stuck.load(Ordering::Acquire)
    }
}

fn exited() -> ab::OpenError {
    ab::OpenError::Other("the ASIO host thread has exited".into())
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        self.tx = None;
        self.wake.set();
        // A thread blocked in a driver call cannot be joined; it is left to finish on its own.
        let exited = self.exited.lock().unwrap_or_else(PoisonError::into_inner).recv_timeout(self.timeout).is_ok();
        if exited {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            log::warn!("ASIO: the host thread did not exit within {} s; left running", self.timeout.as_secs());
        }
    }
}

fn run(loader: Box<dyn Loader>, slot: Slot, jobs: &Receiver<Envelope>, wake: &Event, shared: &Shared, started: &Sender<Result<(), ab::OpenError>>) {
    let com = match StaGuard::new() {
        Ok(com) => com,
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    let window = match MessageWindow::new() {
        Ok(window) => window,
        Err(e) => {
            let _ = started.send(Err(e));
            drop(com);
            return;
        }
    };
    let mut state = HostState { loader, slot, sys_ref: window.sys_ref(), loaded: None };
    let _ = started.send(Ok(()));
    'serve: loop {
        loop {
            match jobs.try_recv() {
                Ok((label, job)) => serve(&mut state, job, label, shared),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break 'serve,
            }
        }
        if shared.quit.load(Ordering::Acquire) {
            break;
        }
        wake.pump_and_wait();
    }
    state.release();
    drop(state);
    drop(window);
    drop(com);
}

fn serve(state: &mut HostState, job: Job, label: String, shared: &Shared) {
    *shared.busy.lock().unwrap_or_else(PoisonError::into_inner) = Some(label.clone());
    let before = state.loaded_name();
    let abandoned = job(state);
    if abandoned {
        let now = state.loaded_name();
        if now.is_some() && now != before {
            log::warn!("ASIO: {label} finished after its caller gave up; unloading {}", now.unwrap_or_default());
            state.release();
        }
    }
    *shared.busy.lock().unwrap_or_else(PoisonError::into_inner) = None;
    shared.stuck.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi;
    use crate::com::fake::{FakeLoader, FakeSpec, Gate, entry};
    use std::time::Instant;

    fn host(drivers: Vec<(DriverEntry, FakeSpec)>, slot: Slot, timeout: Duration) -> (Host, Arc<crate::com::fake::Observed>) {
        let (loader, observed) = FakeLoader::new(drivers);
        (Host::spawn(Box::new(loader), slot, timeout).expect("host"), observed)
    }

    #[test]
    fn every_driver_call_happens_on_the_host_thread_with_a_window_as_sys_ref() {
        let a = entry("A", 1);
        let (host, observed) = host(vec![(a.clone(), FakeSpec::new("A", 2, 2, abi::ASIOST_INT32_LSB))], Slot::default(), Duration::from_secs(10));
        let e = a.clone();
        let info = host.call("load A", move |s| s.load(&e)).expect("loads");
        assert_eq!((info.inputs, info.outputs), (2, 2));
        host.call("unload A", |s| s.unload("A")).expect("unloads");
        let threads = observed.threads.lock().unwrap().clone();
        assert!(!threads.is_empty());
        assert!(threads.iter().all(|t| *t == threads[0]), "one thread");
        assert_ne!(threads[0], std::thread::current().id(), "not the caller's");
        let sys = observed.sys_refs.lock().unwrap().clone();
        assert!(sys.len() == 1 && sys[0] != 0, "init got a window handle: {sys:?}");
        assert_eq!(observed.live.load(Ordering::Acquire), 0);
    }

    #[test]
    fn one_driver_at_a_time_on_one_host_and_across_hosts() {
        let slot = Slot::default();
        let (a, b) = (entry("A", 1), entry("B", 2));
        let drivers =
            vec![(a.clone(), FakeSpec::new("A", 2, 2, abi::ASIOST_INT32_LSB)), (b.clone(), FakeSpec::new("B", 1, 1, abi::ASIOST_INT16_LSB))];
        let (first, observed) = host(drivers.clone(), slot.clone(), Duration::from_secs(10));
        let (second, _) = host(drivers, slot.clone(), Duration::from_secs(10));
        let ea = a.clone();
        first.call("load A", move |s| s.load(&ea)).expect("A loads");
        let eb = b.clone();
        let same_host = first.call("load B", move |s| s.load(&eb)).unwrap_err();
        assert!(matches!(&same_host, ab::OpenError::InUse(t) if t.contains("\"A\"") && t.contains("\"B\"")), "{same_host:?}");
        let eb = b.clone();
        let other_host = second.call("load B", move |s| s.load(&eb)).unwrap_err();
        assert!(matches!(&other_host, ab::OpenError::InUse(t) if t.contains("\"A\"")), "{other_host:?}");
        let eb = b.clone();
        let probe = second.call("probe B", move |s| s.probe(&eb)).unwrap_err();
        assert!(matches!(probe, ab::OpenError::InUse(_)));
        let ea = a.clone();
        assert_eq!(first.call("probe A", move |s| s.probe(&ea)).expect("the loaded driver is queried in place").inputs, 2);
        assert_eq!(observed.max_live.load(Ordering::Acquire), 1, "never two drivers alive");
        first.call("unload A", |s| s.unload("A")).expect("unloads");
        let eb = b.clone();
        second.call("load B", move |s| s.load(&eb)).expect("B loads once A is released");
        drop(second);
        let ea = a.clone();
        first.call("load A", move |s| s.load(&ea)).expect("dropping a host frees the slot");
    }

    #[test]
    fn a_driver_blocking_in_init_times_out_and_is_unloaded_when_it_returns() {
        let gate = Arc::new(Gate::default());
        let mut spec = FakeSpec::new("Slow", 2, 2, abi::ASIOST_INT32_LSB);
        spec.init_gate = Some(Arc::clone(&gate));
        let slow = entry("Slow", 3);
        let slot = Slot::default();
        let (host, observed) = host(vec![(slow.clone(), spec)], slot.clone(), Duration::from_millis(300));
        let started = Instant::now();
        let e = slow.clone();
        let err = host.call("load Slow", move |s| s.load(&e)).unwrap_err();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(5), "{waited:?}");
        assert!(matches!(&err, ab::OpenError::Other(t) if t.contains("did not answer load Slow")), "{err:?}");
        assert!(host.is_stuck());
        let quick = Instant::now();
        let err = host.call("list", |s| s.list()).unwrap_err();
        assert!(quick.elapsed() < Duration::from_millis(100), "fails at once while stuck");
        assert!(matches!(&err, ab::OpenError::Other(t) if t.contains("still blocked in load Slow")), "{err:?}");
        gate.open();
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.is_stuck() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!host.is_stuck());
        assert_eq!(observed.live.load(Ordering::Acquire), 0, "the abandoned load was undone");
        assert!(slot.claim("x").is_ok(), "and the slot freed");
    }

    #[test]
    fn the_default_timeout_is_ten_seconds() {
        assert_eq!(crate::HOST_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn init_failure_and_create_failure_are_errors_with_the_driver_text() {
        let mut spec = FakeSpec::new("Absent", 2, 2, abi::ASIOST_INT32_LSB);
        spec.init_ok = false;
        spec.init_message = "No device connected".into();
        let (absent, gone) = (entry("Absent", 4), entry("Gone", 5));
        let (mut loader, observed) =
            FakeLoader::new(vec![(absent.clone(), spec), (gone.clone(), FakeSpec::new("Gone", 1, 1, abi::ASIOST_INT32_LSB))]);
        loader.create_fails.push("Gone".into());
        let host = Host::spawn(Box::new(loader), Slot::default(), Duration::from_secs(10)).expect("host");
        let e = absent.clone();
        let err = host.call("load", move |s| s.load(&e)).unwrap_err();
        assert!(matches!(&err, ab::OpenError::DeviceNotAvailable(t) if t.contains("init failed: No device connected")), "{err:?}");
        let e = gone.clone();
        let err = host.call("load", move |s| s.load(&e)).unwrap_err();
        assert!(
            matches!(&err, ab::OpenError::Other(t) if t.contains("CoCreateInstance") && t.contains("0x80040154") && t.contains("C:\\fake\\5.dll")),
            "{err:?}"
        );
        assert_eq!(observed.live.load(Ordering::Acquire), 0, "the failed driver was released");
        let e = absent.clone();
        let err = host.call("load", move |s| s.load(&e)).unwrap_err();
        assert!(matches!(err, ab::OpenError::DeviceNotAvailable(_)), "the slot was freed after the failure, not InUse");
    }

    #[test]
    fn a_query_error_maps_through_the_asio_error_table() {
        let mut spec = FakeSpec::new("Broken", 2, 2, abi::ASIOST_INT32_LSB);
        spec.channels_error = Some(abi::ASE_HW_MALFUNCTION);
        let broken = entry("Broken", 6);
        let (host, observed) = host(vec![(broken.clone(), spec)], Slot::default(), Duration::from_secs(10));
        let err = host.call("probe", move |s| s.probe(&broken)).unwrap_err();
        assert!(matches!(&err, ab::OpenError::Other(t) if t.contains("getChannels") && t.contains("ASE_HWMalfunction")), "{err:?}");
        assert_eq!(observed.live.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_panicking_request_is_an_error_and_the_host_keeps_serving() {
        let (host, _) = host(Vec::new(), Slot::default(), Duration::from_secs(10));
        let err = host.call("boom", |_s| -> Result<(), ab::OpenError> { panic!("boom") }).unwrap_err();
        assert!(matches!(&err, ab::OpenError::Other(t) if t.contains("panicked while serving boom")), "{err:?}");
        assert_eq!(host.call("list", |s| s.list()).expect("still serving"), Vec::new());
    }
}
