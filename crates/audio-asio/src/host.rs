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
//!
//! Messages are pumped between requests, not during a driver call. A driver whose `init`,
//! `createBuffers` or `controlPanel` waits for one of its own threads that `SendMessage`s to the
//! `sysRef` window (cross-thread, so the message is only delivered when this thread pumps) cannot
//! be served until the call returns: the caller's request times out (no hang of the caller), and
//! the sent message is dispatched as soon as the driver call returns. Running those calls on
//! another thread while this one pumps would break the SDK's rule that every call is made on the
//! thread that called `init`, so that limitation is kept and tested
//! (`a_driver_that_sends_to_sys_ref_from_its_own_thread_*`); drivers normally `PostMessage`.
//!
//! Streaming: the driver's buffers, callback state and `start`/`stop` belong to its
//! [`Session`], held by the loaded driver (see [`crate::session`] for the open-order rule).
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use crate::com::{Driver, Event, MessageWindow, StaGuard};
use crate::driver::{self, BufferSize, DriverEntry, DriverInfo, asio_error};
use crate::session::{Callback, Opened, Session};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// The MMCSS calls the sessions' buffer-switch threads make ([`crate::mmcss`]).
    fn mmcss(&self) -> crate::mmcss::Hooks {
        crate::com::MMCSS
    }
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

/// The loaded driver. Field order matters: a session is torn down explicitly (`HostState::release`)
/// before the driver is released, and the driver is released before the slot is freed.
struct Loaded {
    entry: DriverEntry,
    /// What the driver reported at load.
    info: DriverInfo,
    /// The streams of the driver, when any were opened.
    session: Option<Session>,
    /// Whether a [`crate::Loaded`] handle holds the driver (it stays loaded while either holds it).
    handle: bool,
    driver: Driver,
    _slot: SlotGuard,
}

/// The state of the host thread; requests run against it on that thread.
pub(crate) struct HostState {
    loader: Box<dyn Loader>,
    slot: Slot,
    sys_ref: *mut c_void,
    loaded: Option<Loaded>,
    /// The host's wake event, which `kAsioLatenciesChanged` sets so the thread refetches.
    wake: Arc<Event>,
    /// The shortest stall limit of the sessions' watchdogs ([`crate::watchdog::STALL_FLOOR`]).
    watchdog_floor: Duration,
}

impl HostState {
    pub(crate) fn list(&self) -> Result<Vec<DriverEntry>, ab::OpenError> {
        self.loader.list()
    }

    fn loaded_name(&self) -> Option<String> {
        self.loaded.as_ref().map(|l| l.entry.name.clone())
    }

    /// Loads and initializes `entry` (no buffers) and queries it, for a [`crate::Loaded`] handle.
    /// The driver of a running stream session is shared with the handle.
    pub(crate) fn load(&mut self, entry: &DriverEntry) -> Result<DriverInfo, ab::OpenError> {
        if let Some(loaded) = self.loaded.as_mut()
            && loaded.entry.name == entry.name
            && !loaded.handle
            && loaded.session.is_some()
        {
            loaded.handle = true;
            return driver::query(&entry.name, &mut loaded.driver);
        }
        let info = self.load_driver(entry)?;
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.handle = true;
        }
        Ok(info)
    }

    fn in_use(open: &str, entry: &DriverEntry) -> ab::OpenError {
        ab::OpenError::InUse(format!(
            "ASIO driver \"{open}\" is already open; ASIO allows one driver per process, so \"{}\" cannot open until it is closed",
            entry.name
        ))
    }

    /// Loads and initializes `entry`, held by nothing yet.
    fn load_driver(&mut self, entry: &DriverEntry) -> Result<DriverInfo, ab::OpenError> {
        if let Some(open) = self.loaded_name() {
            return Err(Self::in_use(&open, entry));
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
        self.loaded = Some(Loaded { entry: entry.clone(), info: info.clone(), session: None, handle: false, driver, _slot: guard });
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

    /// The handle of the loaded driver `name` lets go; the driver is released unless a stream
    /// session still uses it.
    pub(crate) fn unload(&mut self, name: &str) -> Result<(), ab::OpenError> {
        match self.loaded.as_mut() {
            Some(loaded) if loaded.entry.name == name => {
                loaded.handle = false;
                if loaded.session.is_none() {
                    self.release();
                }
                Ok(())
            }
            _ => Err(not_open(name)),
        }
    }

    /// Releases the loaded driver: its session first (stop, disposeBuffers, callback state freed),
    /// then the object.
    fn release(&mut self) {
        if let Some(mut loaded) = self.loaded.take() {
            if let Some(session) = loaded.session.take() {
                session.teardown(&loaded.entry.name, &mut loaded.driver);
            }
            drop(loaded);
        }
    }

    /// Opens a stream of `entry`: loads the driver if needed, then opens its session or attaches
    /// the stream to it.
    pub(crate) fn open_stream(
        &mut self,
        entry: &DriverEntry,
        request: &ab::OpenRequest,
        callback: Callback,
        error: ab::ErrorCallback,
    ) -> Result<Opened, ab::OpenError> {
        // A session whose streams all went while the host was stuck (their detach was never sent)
        // would refuse this open; end it first.
        self.sweep();
        match self.loaded_name() {
            None => {
                self.load_driver(entry)?;
            }
            Some(open) if open == entry.name => {}
            Some(open) => return Err(Self::in_use(&open, entry)),
        }
        let (wake, floor, mmcss) = (Arc::clone(&self.wake), self.watchdog_floor, self.loader.mmcss());
        let Some(loaded) = self.loaded.as_mut() else { return Err(not_open(&entry.name)) };
        let Loaded { entry: open, info, session, driver, .. } = &mut *loaded;
        let result = match session.as_mut() {
            Some(session) => session.attach(&open.name, driver, info, request, callback, error),
            None => Session::open(&open.name, driver, info, wake, floor, mmcss, request, callback, error).map(|(s, opened)| {
                *session = Some(s);
                opened
            }),
        };
        if result.is_err() && loaded.session.is_none() && !loaded.handle {
            self.release();
        }
        result
    }

    /// The session of `name` with `generation`, with its driver.
    fn session(&mut self, name: &str, generation: u64) -> Result<(&mut Session, &mut Driver), ab::OpenError> {
        match self.loaded.as_mut() {
            Some(Loaded { entry, session: Some(session), driver, .. }) if entry.name == name && session.generation() == generation => {
                Ok((session, driver))
            }
            _ => Err(ab::OpenError::Other(format!("ASIO driver \"{name}\": the stream's session has ended; open it again"))),
        }
    }

    pub(crate) fn start_stream(&mut self, name: &str, generation: u64, direction: ab::Direction, pair: (u64, i64)) -> Result<(), ab::OpenError> {
        let (session, driver) = self.session(name, generation)?;
        session.start(name, driver, direction, pair)
    }

    pub(crate) fn stop_stream(&mut self, name: &str, generation: u64, direction: ab::Direction) -> Result<(), ab::OpenError> {
        let (session, driver) = self.session(name, generation)?;
        session.stop(name, driver, direction);
        Ok(())
    }

    /// The stream of `direction` is gone; the session ends with its last stream.
    pub(crate) fn detach_stream(&mut self, name: &str, generation: u64, direction: ab::Direction) -> Result<(), ab::OpenError> {
        let (session, driver) = self.session(name, generation)?;
        if session.detach(name, driver, direction) {
            self.end_session();
        }
        Ok(())
    }

    /// Tears down a session whose streams are all gone (a stream whose open answer was abandoned).
    fn sweep(&mut self) {
        if self.loaded.as_ref().and_then(|l| l.session.as_ref()).is_some_and(Session::abandoned) {
            self.end_session();
        }
    }

    fn end_session(&mut self) {
        let Some(loaded) = self.loaded.as_mut() else { return };
        if let Some(session) = loaded.session.take() {
            session.teardown(&loaded.entry.name, &mut loaded.driver);
        }
        if !loaded.handle {
            self.release();
        }
    }

    /// Work between requests: a session left without streams (a stream dropped while the host
    /// was stuck, whose detach could not be sent) is ended, a latency refetch the driver asked for
    /// is made, and the watchdog looks at a running driver. Returns how long the thread may sleep
    /// before it looks again (`None`: until a request or a message).
    fn tend(&mut self) -> Option<Duration> {
        self.sweep();
        let Some(Loaded { entry, session: Some(session), driver, .. }) = self.loaded.as_mut() else { return None };
        session.tend(&entry.name, driver);
        session.tick()
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
    /// The host thread's id, once it runs (0 before).
    thread: AtomicU32,
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
    #[cfg(test)]
    pub(crate) fn spawn(loader: Box<dyn Loader>, slot: Slot, timeout: Duration) -> Result<Self, ab::OpenError> {
        Self::spawn_watching(loader, slot, timeout, crate::watchdog::STALL_FLOOR)
    }

    /// [`Host::spawn`] with `watchdog_floor` as the sessions' shortest stall limit (tests that
    /// drive the callbacks by hand use a long one).
    pub(crate) fn spawn_watching(loader: Box<dyn Loader>, slot: Slot, timeout: Duration, watchdog_floor: Duration) -> Result<Self, ab::OpenError> {
        Self::spawn_after(loader, slot, timeout, watchdog_floor, Duration::ZERO)
    }

    /// [`Host::spawn`] with the thread pausing `delay` before it starts (tests of the start timeout).
    fn spawn_after(loader: Box<dyn Loader>, slot: Slot, timeout: Duration, watchdog_floor: Duration, delay: Duration) -> Result<Self, ab::OpenError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let (started_tx, started_rx) = mpsc::channel();
        let (exited_tx, exited) = mpsc::channel();
        let wake = Arc::new(Event::new()?);
        let shared =
            Arc::new(Shared { quit: AtomicBool::new(false), stuck: AtomicBool::new(false), busy: Mutex::new(None), thread: AtomicU32::new(0) });
        let (thread_wake, thread_shared) = (Arc::clone(&wake), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("transit-asio-host".into())
            .spawn(move || {
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                run(loader, slot, watchdog_floor, &rx, thread_wake, &thread_shared, &started_tx);
                let _ = exited_tx.send(());
            })
            .map_err(|e| ab::OpenError::Other(format!("could not start the ASIO host thread: {e}")))?;
        match started_rx.recv_timeout(timeout) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                // The thread may still start late: tell it to end and wake it, so it does not sleep
                // in its wait forever with a window and an apartment nobody will use.
                shared.quit.store(true, Ordering::Release);
                drop(tx);
                wake.set();
                return Err(ab::OpenError::Other("the ASIO host thread did not start".into()));
            }
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

    /// Wakes the host thread for its work between requests (a stop asked for from inside a
    /// driver callback); never blocks or allocates.
    pub(crate) fn wake(&self) {
        self.wake.set();
    }

    /// Whether the calling thread is the host thread (a request to it from there would wait for
    /// itself).
    pub(crate) fn on_host_thread(&self) -> bool {
        self.shared.thread.load(Ordering::Acquire) == crate::com::current_thread_id()
    }

    /// Whether a request timed out and the host thread has not finished it yet.
    pub(crate) fn is_stuck(&self) -> bool {
        self.shared.stuck.load(Ordering::Acquire)
    }

    /// Marks the host stuck (or not) without a blocking request: the window in which a stream's
    /// detach is refused (tests of the orphaned-session sweep).
    #[cfg(test)]
    pub(crate) fn set_stuck(&self, stuck: bool) {
        self.shared.stuck.store(stuck, Ordering::Release);
        self.wake.set();
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

fn run(
    loader: Box<dyn Loader>,
    slot: Slot,
    watchdog_floor: Duration,
    jobs: &Receiver<Envelope>,
    wake: Arc<Event>,
    shared: &Shared,
    started: &Sender<Result<(), ab::OpenError>>,
) {
    shared.thread.store(crate::com::current_thread_id(), Ordering::Release);
    let com = match StaGuard::new() {
        Ok(com) => com,
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    if let Err(e) = crate::com::raise_host_priority() {
        log::warn!("ASIO: {e}; the host thread runs at normal priority");
    }
    let window = match MessageWindow::new() {
        Ok(window) => window,
        Err(e) => {
            let _ = started.send(Err(e));
            drop(com);
            return;
        }
    };
    let mut state = HostState { loader, slot, sys_ref: window.sys_ref(), loaded: None, wake: Arc::clone(&wake), watchdog_floor };
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
        let tick = state.tend();
        wake.pump_and_wait(tick);
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
        } else {
            state.sweep();
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
    fn the_host_thread_runs_above_normal_priority() {
        let (host, _) = host(Vec::new(), Slot::default(), Duration::from_secs(10));
        let level = host.call("priority", |_| Ok(crate::com::current_thread_priority())).expect("answers");
        assert_eq!(level, windows::Win32::System::Threading::THREAD_PRIORITY_ABOVE_NORMAL.0);
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
    fn a_host_that_starts_after_the_spawn_timeout_ends_instead_of_sleeping_forever() {
        let (loader, observed) = FakeLoader::new(Vec::new());
        let err = Host::spawn_after(
            Box::new(loader),
            Slot::default(),
            Duration::from_millis(100),
            crate::watchdog::STALL_FLOOR,
            Duration::from_millis(400),
        )
        .err();
        assert!(matches!(&err, Some(ab::OpenError::Other(t)) if t.contains("did not start")), "{err:?}");
        // The late thread starts, finds the request channel closed and the event set, and exits,
        // dropping the loader (the only other holder of `observed`).
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&observed) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&observed), 1, "the late host thread exited");
    }

    #[test]
    fn init_is_read_by_its_low_byte() {
        assert!(crate::com::init_succeeded(abi::ASIO_TRUE));
        assert!(!crate::com::init_succeeded(abi::ASIO_FALSE));
        assert!(!crate::com::init_succeeded(0x5a5a_5a00), "a C++ bool false with garbage above AL");
        assert!(crate::com::init_succeeded(0x5a5a_5a01), "a C++ bool true with garbage above AL");
        let mut liar = FakeSpec::new("Liar", 2, 2, abi::ASIOST_INT32_LSB);
        liar.init_raw = Some(0x1234_5600);
        let mut honest = FakeSpec::new("Bool", 2, 2, abi::ASIOST_INT32_LSB);
        honest.init_raw = Some(0x7fff_ff01);
        let (a, b) = (entry("Liar", 20), entry("Bool", 21));
        let (host, observed) = host(vec![(a.clone(), liar), (b.clone(), honest)], Slot::default(), Duration::from_secs(10));
        let err = host.call("load Liar", move |s| s.load(&a)).unwrap_err();
        assert!(matches!(err, ab::OpenError::DeviceNotAvailable(_)), "{err:?}");
        host.call("load Bool", move |s| s.load(&b)).expect("a true low byte is a success");
        host.call("unload", |s| s.unload("Bool")).expect("unloads");
        assert_eq!(observed.live.load(Ordering::Acquire), 0);
    }

    /// The `sysRef` window `init` got.
    fn sys_ref(observed: &crate::com::fake::Observed) -> usize {
        *observed.sys_refs.lock().unwrap().last().expect("init ran")
    }

    #[test]
    fn a_driver_that_sends_to_sys_ref_from_its_own_thread_is_served_while_the_host_is_idle() {
        let a = entry("Poster", 22);
        let (host, observed) =
            host(vec![(a.clone(), FakeSpec::new("Poster", 2, 2, abi::ASIOST_INT32_LSB))], Slot::default(), Duration::from_secs(10));
        host.call("load", move |s| s.load(&a)).expect("loads");
        let window = sys_ref(&observed);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            crate::com::fake::send_null(window);
            let _ = done_tx.send(());
        });
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).is_ok(), "the idle host pumped the sent message");
        host.call("unload", |s| s.unload("Poster")).expect("unloads");
    }

    #[test]
    fn a_driver_that_sends_to_sys_ref_from_its_own_thread_during_init_times_out_the_caller_not_the_process() {
        // The documented limitation: no pumping inside a driver call. The caller gets an error at
        // the timeout, the sent message is dispatched once `init` returns, and the host recovers.
        let mut spec = FakeSpec::new("Sender", 2, 2, abi::ASIOST_INT32_LSB);
        spec.init_sends = Some(Duration::from_millis(800));
        let a = entry("Sender", 23);
        let (host, observed) = host(vec![(a.clone(), spec)], Slot::default(), Duration::from_millis(200));
        let started = Instant::now();
        let e = a.clone();
        let err = host.call("load Sender", move |s| s.load(&e)).unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(700), "the caller did not wait for the driver: {:?}", started.elapsed());
        assert!(matches!(&err, ab::OpenError::Other(t) if t.contains("did not answer")), "{err:?}");
        let deadline = Instant::now() + Duration::from_secs(5);
        while (host.is_stuck() || !observed.sent.load(Ordering::SeqCst)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(observed.sent.load(Ordering::SeqCst), "the SendMessage completed once the host pumped again");
        assert!(!host.is_stuck());
        assert_eq!(observed.live.load(Ordering::Acquire), 0, "the abandoned load was undone");
        drop(a);
        host.call("list", |s| s.list()).expect("the host serves again");
    }

    #[test]
    fn a_panicking_request_is_an_error_and_the_host_keeps_serving() {
        let (host, _) = host(Vec::new(), Slot::default(), Duration::from_secs(10));
        let err = host.call("boom", |_s| -> Result<(), ab::OpenError> { panic!("boom") }).unwrap_err();
        assert!(matches!(&err, ab::OpenError::Other(t) if t.contains("panicked while serving boom")), "{err:?}");
        assert_eq!(host.call("list", |s| s.list()).expect("still serving"), Vec::new());
    }
}
