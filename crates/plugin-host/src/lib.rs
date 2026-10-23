//! The plug-in host thread (ADR 0018).
//!
//! Hosted plug-ins (CLAP, VST3) are created, restored, edited and destroyed on a thread of their
//! own, which is their main thread in every SDK's sense. The UI submits a job ([`Host::submit`])
//! and polls the returned [`Pending`] once per frame; it never waits for plug-in code for more
//! than a frame. The host thread pumps the OS message queue (`transit_rt::gui_thread`) between
//! jobs, and every [`Config::tick`] runs the formats' request servicing and each editor's `idle`.
//!
//! An instance made in a job goes through [`Ctx::adopt`]: it is wrapped in a forwarder that the
//! render graph holds like any other plug-in (no allocation, one extra virtual call per method)
//! and whose drop sends the instance back to its host thread to be destroyed there. Its editor
//! and state reader stay on the host thread; the wrapper hands out proxies that send requests.
//!
//! **Stuck plug-ins.** A job or tick that runs longer than [`Config::timeout`], or a window
//! message (a plug-in's modal dialog runs inside one) that runs longer than
//! [`Config::dispatch_timeout`], wedges its thread: [`Host::check`] marks it, starts a fresh host
//! thread for new creations, and moves the creations that had not started to it. A wedged
//! thread is never joined. **When the call returns, the thread is responsive again** and goes on
//! serving its own instances (state reads, editors, destruction); only new creations stay on the
//! fresh thread. A thread that never returns keeps its instances, leaked rather than destroyed
//! elsewhere.
//!
//! **Inline mode.** On macOS (AppKit wants editors on the process main thread, and both SDKs want
//! the editor and the instance on one thread) and on `wasm32` the host runs every job at once on
//! the calling thread and [`Ctx::adopt`] returns the instance as it is: the behavior before
//! ADR 0018.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod hosted;

use soundcraft_dsp::{Plugin, PluginEditor, PluginStateReader};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};
use transit_rt::gui_thread::{MessageQueue, QueueWaker};

use hosted::EditorShared;

/// How long a request may run before its host thread counts as wedged (ADR 0018).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one window-message dispatch (a modal dialog) may run before its thread counts as
/// wedged.
pub const DEFAULT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the host thread services plug-in requests and editor `idle` calls.
pub const DEFAULT_TICK: Duration = Duration::from_millis(15);

/// Code the host thread runs at its start, every tick and at shutdown (the format hosts' hooks:
/// [`Hooks::formats`]).
#[derive(Clone, Default)]
pub struct Hooks {
    /// On each host thread, before its first job.
    pub start: Option<Arc<dyn Fn() + Send + Sync>>,
    /// On each host thread, every [`Config::tick`].
    pub tick: Option<Arc<dyn Fn() + Send + Sync>>,
    /// On each host thread at [`Host::shutdown`], after its instances' handles are gone.
    pub shutdown: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Hooks {
    /// The CLAP and VST3 hosts: the thread adopts the VST3 main-thread role, services both
    /// formats' requests every tick and runs the VST3 module exits at shutdown.
    #[must_use]
    pub fn formats() -> Hooks {
        Hooks {
            start: Some(Arc::new(soundcraft_vst3_host::adopt_main_thread)),
            tick: Some(Arc::new(|| {
                soundcraft_clap_host::service_requests();
                soundcraft_vst3_host::service_requests();
            })),
            shutdown: Some(Arc::new(soundcraft_vst3_host::shutdown)),
        }
    }
}

/// Whether hosts run inline unless told otherwise ([`Config::inline`]): on macOS (AppKit wants
/// editors on the process main thread) and `wasm32`. Tests of host-thread behavior through the
/// default host ([`shared`]) branch on this, so they follow the runtime's own choice.
pub const INLINE_BY_DEFAULT: bool = cfg!(any(target_os = "macos", target_arch = "wasm32"));

/// How a [`Host`] runs.
#[derive(Clone)]
pub struct Config {
    /// Longest a job or tick may run before the thread counts as wedged.
    pub timeout: Duration,
    /// Longest one window-message dispatch may run before the thread counts as wedged. Longer
    /// than `timeout`: a plug-in's modal file dialog or message box runs inside a dispatch for as
    /// long as the user leaves it open.
    pub dispatch_timeout: Duration,
    /// Period of the request servicing and editor `idle` calls.
    pub tick: Duration,
    pub hooks: Hooks,
    /// Run every job on the calling thread (macOS, `wasm32`, or a failed thread start).
    pub inline: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            timeout: DEFAULT_TIMEOUT,
            dispatch_timeout: DEFAULT_DISPATCH_TIMEOUT,
            tick: DEFAULT_TICK,
            hooks: Hooks::default(),
            inline: INLINE_BY_DEFAULT,
        }
    }
}

type Job = Box<dyn FnOnce(&mut Ctx) + Send>;

/// One queued job. Taking it claims it, so a job that was moved to a fresh thread runs once.
struct JobCell {
    job: Mutex<Option<Job>>,
}

impl JobCell {
    fn take(&self) -> Option<Job> {
        lock(&self.job).take()
    }
    fn claimed(&self) -> bool {
        lock(&self.job).is_none()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One host thread.
pub(crate) struct Worker {
    tx: mpsc::Sender<Arc<JobCell>>,
    waker: QueueWaker,
    epoch: Instant,
    /// Milliseconds since `epoch` plus one when the current job, tick or dispatch began; 0 idle.
    busy_since: AtomicU64,
    /// The current busy section is a window-message dispatch (its own, longer budget).
    dispatching: AtomicBool,
    /// Busy past its budget and not yet back; cleared as soon as the call returns.
    wedged: AtomicBool,
    /// The thread's loop runs (false once it has shut down).
    running: AtomicBool,
    /// Jobs that may move to a fresh thread if this one wedges (creation jobs).
    movable: Mutex<Vec<Arc<JobCell>>>,
    thread: OnceLock<std::thread::ThreadId>,
}

impl Worker {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX - 1)
    }

    /// How long the current job has run (`None` while idle).
    fn busy_for(&self) -> Option<Duration> {
        match self.busy_since.load(Ordering::Acquire) {
            0 => None,
            t => Some(Duration::from_millis(self.now_ms().saturating_sub(t - 1))),
        }
    }

    fn busy<R>(&self, f: impl FnOnce() -> R) -> R {
        self.busy_as(false, f)
    }

    /// Runs `f` on the clock (`dispatch`: the window-message budget). A thread marked wedged
    /// meanwhile is responsive again once `f` returns: it was slow, not stuck.
    fn busy_as<R>(&self, dispatch: bool, f: impl FnOnce() -> R) -> R {
        self.dispatching.store(dispatch, Ordering::Release);
        self.busy_since.store(self.now_ms() + 1, Ordering::Release);
        let r = f();
        self.busy_since.store(0, Ordering::Release);
        if self.wedged.swap(false, Ordering::AcqRel) {
            log::warn!("plug-in host: a host thread that was not responding has returned; its plug-ins are reachable again");
        }
        r
    }

    /// The budget of the current busy section.
    fn budget(&self, config: &Config) -> Duration {
        if self.dispatching.load(Ordering::Acquire) { config.dispatch_timeout } else { config.timeout }
    }

    /// Queues a job; `Err` gives it back when the thread is gone.
    fn send(&self, cell: Arc<JobCell>) -> Result<(), Arc<JobCell>> {
        if !self.running.load(Ordering::Acquire) {
            return Err(cell);
        }
        self.tx.send(cell).map_err(|e| e.0)?;
        self.waker.wake();
        Ok(())
    }

    /// Queues `job`, or leaks it (and whatever it owns) when the thread is gone: plug-in objects
    /// are never destroyed on another thread than their own.
    pub(crate) fn send_or_leak(&self, job: impl FnOnce(&mut Ctx) + Send + 'static) {
        let cell = Arc::new(JobCell { job: Mutex::new(Some(Box::new(job))) });
        if let Err(cell) = self.send(cell) {
            log::debug!("plug-in host: the thread of an instance is gone; leaking what it owned");
            std::mem::forget(cell);
        }
    }

    pub(crate) fn is_current_thread(&self) -> bool {
        self.thread.get() == Some(&std::thread::current().id())
    }

    /// Whether the thread is inside one window-message dispatch (a plug-in's modal dialog) that
    /// has run longer than `limit`.
    pub(crate) fn in_dispatch_longer_than(&self, limit: Duration) -> bool {
        self.dispatching.load(Ordering::Acquire) && self.busy_for().is_some_and(|d| d >= limit)
    }

    pub(crate) fn responsive(&self) -> bool {
        self.running.load(Ordering::Acquire) && !self.wedged.load(Ordering::Acquire)
    }
}

/// What a job's slot holds.
const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const DONE: u8 = 2;
const FAILED: u8 = 3;

struct Slot<T> {
    state: AtomicU8,
    /// When the job started on its thread (an `Instant` taken there).
    started: Mutex<Option<Instant>>,
    value: Mutex<Option<T>>,
    done: Condvar,
}

struct Unrun<T>(Arc<Slot<T>>);

impl<T> Drop for Unrun<T> {
    fn drop(&mut self) {
        if self.0.state.compare_exchange(QUEUED, FAILED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.0.done.notify_all();
        }
    }
}

/// What [`Pending::poll`] found.
#[derive(Debug)]
pub enum Status<T> {
    /// Not finished; `running` once its thread has started it.
    Waiting {
        running: bool,
    },
    Ready(T),
    /// It has run longer than the timeout: its thread is wedged and abandoned (see the crate docs).
    Stuck,
    /// It panicked, was cancelled, or its thread is gone.
    Failed,
}

/// The result of a job, to poll once per frame. Dropping it before the job starts cancels the job;
/// a result that arrives after the drop is dropped on the host thread.
pub struct Pending<T> {
    slot: Arc<Slot<T>>,
    cell: Option<Arc<JobCell>>,
    host: Host,
}

impl<T> Pending<T> {
    /// Non-blocking. [`Status::Ready`] once (later polls report [`Status::Failed`]).
    pub fn poll(&self) -> Status<T> {
        match self.slot.state.load(Ordering::Acquire) {
            DONE => lock(&self.slot.value).take().map_or(Status::Failed, Status::Ready),
            FAILED => Status::Failed,
            RUNNING => {
                let ran = lock(&self.slot.started).map_or(Duration::ZERO, |t| t.elapsed());
                if ran >= self.host.inner.config.timeout {
                    self.host.check();
                    Status::Stuck
                } else {
                    Status::Waiting { running: true }
                }
            }
            _ => {
                self.host.check();
                Status::Waiting { running: false }
            }
        }
    }

    /// Blocks until the job finishes, at most `timeout` (`None`: timed out, failed or stuck).
    /// Only for the few callers that must have the answer now (a save reading plug-in states).
    pub fn wait(self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut value = lock(&self.slot.value);
        loop {
            match self.slot.state.load(Ordering::Acquire) {
                DONE => return value.take(),
                FAILED => return None,
                _ => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(value);
                self.host.check();
                return None;
            }
            value = self.slot.done.wait_timeout(value, left.min(Duration::from_millis(50))).map_or_else(|e| e.into_inner().0, |r| r.0);
        }
    }
}

impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        // Not started: cancel, dropping the job here (it made nothing yet).
        if let Some(cell) = self.cell.take()
            && self.slot.state.load(Ordering::Acquire) == QUEUED
        {
            drop(cell.take());
        }
    }
}

/// The host thread's view of its instances, given to every job.
pub struct Ctx {
    worker: Option<Arc<Worker>>,
    host: Weak<Inner>,
    timeout: Duration,
    entries: HashMap<u64, Local>,
    next_key: u64,
    exit: bool,
}

/// An instance's editor and state reader, kept on its host thread.
struct Local {
    editor: Option<Box<dyn PluginEditor>>,
    state: Option<Box<dyn PluginStateReader>>,
    shared: Arc<EditorShared>,
}

impl Ctx {
    /// Takes an instance made on this thread into the host: the returned plug-in forwards to it,
    /// its editor and state reader stay here (the returned one hands out proxies), and dropping it
    /// destroys the instance here. In inline mode it returns `plugin` unchanged. Call it in the
    /// job that made the instance.
    pub fn adopt(&mut self, mut plugin: Box<dyn Plugin>) -> Box<dyn Plugin> {
        let Some(worker) = self.worker.clone() else { return plugin };
        let editor = plugin.editor();
        let state = plugin.state_reader();
        let shared = Arc::new(EditorShared::default());
        let key = self.next_key;
        self.next_key += 1;
        let (has_editor, has_state) = (editor.is_some(), state.is_some());
        self.entries.insert(key, Local { editor, state, shared: Arc::clone(&shared) });
        let token = hosted::Token::new(key, worker, shared, self.timeout, self.host.clone());
        Box::new(hosted::Hosted::new(plugin, token, has_editor, has_state))
    }

    /// The number of adopted instances whose handles are still alive here.
    #[must_use]
    pub fn adopted(&self) -> usize {
        self.entries.len()
    }

    fn local(&mut self, key: u64) -> Option<&mut Local> {
        self.entries.get_mut(&key)
    }

    /// Every editor's `idle`: their edits and open state go to the shared side.
    fn idle(&mut self) {
        for local in self.entries.values_mut() {
            if let Some(editor) = &mut local.editor {
                let edits = editor.idle();
                local.shared.publish(edits, editor.is_open());
            }
        }
    }
}

struct Inner {
    config: Config,
    current: Mutex<Option<Arc<Worker>>>,
    workers: Mutex<Vec<Arc<Worker>>>,
    /// Inline mode: the one context, used on the calling thread.
    inline: Mutex<Option<Ctx>>,
    closed: AtomicBool,
    wedged: AtomicUsize,
}

/// The plug-in host (see the crate docs). Cheap to clone; every clone is the same host.
#[derive(Clone)]
pub struct Host {
    inner: Arc<Inner>,
}

impl Host {
    /// A host with its first thread started (unless `config.inline`). If the thread cannot be
    /// started, the host runs inline.
    #[must_use]
    pub fn new(config: Config) -> Host {
        let inline = config.inline;
        let host = Host {
            inner: Arc::new(Inner {
                config,
                current: Mutex::new(None),
                workers: Mutex::new(Vec::new()),
                inline: Mutex::new(None),
                closed: AtomicBool::new(false),
                wedged: AtomicUsize::new(0),
            }),
        };
        let started = !inline && host.start_worker().is_some();
        if !started {
            if !inline {
                log::error!("plug-in host: cannot start the host thread; plug-ins run on the UI thread");
            }
            *lock(&host.inner.inline) = Some(Ctx {
                worker: None,
                host: Arc::downgrade(&host.inner),
                timeout: host.inner.config.timeout,
                entries: HashMap::new(),
                next_key: 0,
                exit: false,
            });
        }
        host
    }

    /// Whether jobs run on the calling thread (see the crate docs).
    #[must_use]
    pub fn is_inline(&self) -> bool {
        lock(&self.inner.inline).is_some()
    }

    /// The request timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.inner.config.timeout
    }

    fn start_worker(&self) -> Option<Arc<Worker>> {
        let (tx, rx) = mpsc::channel::<Arc<JobCell>>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<QueueWaker>(1);
        let n = lock(&self.inner.workers).len();
        let config = self.inner.config.clone();
        let (worker_tx, worker_rx) = mpsc::sync_channel::<Arc<Worker>>(1);
        let weak = Arc::downgrade(&self.inner);
        let spawned = std::thread::Builder::new().name(format!("plugin-host-{n}")).spawn(move || {
            let queue = MessageQueue::attach();
            if ready_tx.send(queue.waker()).is_err() {
                return;
            }
            let Ok(worker) = worker_rx.recv() else { return };
            let _ = worker.thread.set(std::thread::current().id());
            run(&worker, &rx, &queue, &config, weak);
        });
        if spawned.is_err() {
            return None;
        }
        let waker = ready_rx.recv().ok()?;
        let worker = Arc::new(Worker {
            tx,
            waker,
            epoch: Instant::now(),
            busy_since: AtomicU64::new(0),
            dispatching: AtomicBool::new(false),
            wedged: AtomicBool::new(false),
            running: AtomicBool::new(true),
            movable: Mutex::new(Vec::new()),
            thread: OnceLock::new(),
        });
        worker_tx.send(Arc::clone(&worker)).ok()?;
        lock(&self.inner.workers).push(Arc::clone(&worker));
        *lock(&self.inner.current) = Some(Arc::clone(&worker));
        Some(worker)
    }

    /// Runs `job` on the host thread (at once, in inline mode) and returns its result to poll.
    /// Creation jobs (`movable`) move to a fresh thread if theirs wedges before starting them;
    /// jobs for an existing instance stay on the instance's thread ([`Host::submit_to`]).
    pub fn submit<T: Send + 'static>(&self, job: impl FnOnce(&mut Ctx) -> T + Send + 'static) -> Pending<T> {
        self.check();
        let worker = lock(&self.inner.current).clone();
        self.submit_on(worker, true, job)
    }

    pub(crate) fn submit_to<T: Send + 'static>(&self, worker: &Arc<Worker>, job: impl FnOnce(&mut Ctx) -> T + Send + 'static) -> Pending<T> {
        self.submit_on(Some(Arc::clone(worker)), false, job)
    }

    fn submit_on<T: Send + 'static>(
        &self,
        worker: Option<Arc<Worker>>,
        movable: bool,
        job: impl FnOnce(&mut Ctx) -> T + Send + 'static,
    ) -> Pending<T> {
        let slot = Arc::new(Slot { state: AtomicU8::new(QUEUED), started: Mutex::new(None), value: Mutex::new(None), done: Condvar::new() });
        let theirs = Arc::clone(&slot);
        // Marks the job failed if it is dropped without running (cancelled, or its thread gone).
        let unrun = Unrun(Arc::clone(&slot));
        let wrapped = move |ctx: &mut Ctx| {
            let _unrun = unrun;
            *lock(&theirs.started) = Some(Instant::now());
            theirs.state.store(RUNNING, Ordering::Release);
            let outcome = catch_unwind(AssertUnwindSafe(|| job(ctx)));
            let mut value = lock(&theirs.value);
            match outcome {
                Ok(v) => {
                    *value = Some(v);
                    theirs.state.store(DONE, Ordering::Release);
                }
                Err(payload) => {
                    theirs.state.store(FAILED, Ordering::Release);
                    log::error!("plug-in host: a request panicked");
                    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                        std::mem::forget(secondary);
                    }
                }
            }
            drop(value);
            theirs.done.notify_all();
        };
        let failed = |slot: &Arc<Slot<T>>| slot.state.store(FAILED, Ordering::Release);
        if self.inner.closed.load(Ordering::Acquire) {
            failed(&slot);
            return Pending { slot, cell: None, host: self.clone() };
        }
        if let Some(ctx) = lock(&self.inner.inline).as_mut() {
            wrapped(ctx);
            return Pending { slot, cell: None, host: self.clone() };
        }
        let Some(worker) = worker else {
            failed(&slot);
            return Pending { slot, cell: None, host: self.clone() };
        };
        let cell = Arc::new(JobCell { job: Mutex::new(Some(Box::new(wrapped))) });
        if movable {
            let mut list = lock(&worker.movable);
            list.retain(|c| !c.claimed());
            list.push(Arc::clone(&cell));
        }
        if worker.send(Arc::clone(&cell)).is_err() {
            drop(cell.take());
            failed(&slot);
        }
        Pending { slot, cell: Some(cell), host: self.clone() }
    }

    /// Notices a wedged host thread: if the current one has been busy for longer than the
    /// timeout, it is marked, a fresh thread takes over, and the creation jobs it had not started
    /// move there. Cheap; the UI calls it every frame (polling a [`Pending`] does too).
    pub fn check(&self) {
        let current = lock(&self.inner.current).clone();
        // A thread that is no longer current (it wedged once and came back) is only marked: its
        // state reads and editors answer at once while it is stuck again.
        for w in lock(&self.inner.workers).iter() {
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, w)) || w.wedged.load(Ordering::Acquire) {
                continue;
            }
            if w.busy_for().is_some_and(|d| d >= w.budget(&self.inner.config)) {
                w.wedged.store(true, Ordering::Release);
                self.inner.wedged.fetch_add(1, Ordering::AcqRel);
                log::error!("plug-in host: an older host thread stopped responding; its plug-ins are unreachable until it returns");
            }
        }
        let Some(current) = current else { return };
        let budget = current.budget(&self.inner.config);
        if current.wedged.load(Ordering::Acquire) || current.busy_for().is_none_or(|d| d < budget) {
            return;
        }
        current.wedged.store(true, Ordering::Release);
        self.inner.wedged.fetch_add(1, Ordering::AcqRel);
        log::error!(
            "plug-in host: a plug-in has not returned for {budget:?}; a new host thread takes over new plug-ins, and the old one's plug-ins are unreachable until it returns"
        );
        if self.inner.closed.load(Ordering::Acquire) {
            return;
        }
        let Some(fresh) = self.start_worker() else {
            *lock(&self.inner.current) = None;
            return;
        };
        let waiting: Vec<_> = std::mem::take(&mut *lock(&current.movable));
        for cell in waiting.into_iter().filter(|c| !c.claimed()) {
            lock(&fresh.movable).push(Arc::clone(&cell));
            let _ = fresh.send(cell);
        }
    }

    /// How many host threads are wedged now (busy past their budget and not yet returned). The
    /// status line says so while it is not 0; it goes back down when a thread returns.
    #[must_use]
    pub fn wedged(&self) -> usize {
        lock(&self.inner.workers).iter().filter(|w| w.running.load(Ordering::Acquire) && w.wedged.load(Ordering::Acquire)).count()
    }

    /// How many times a host thread has wedged in this process (returned or not).
    #[must_use]
    pub fn wedges(&self) -> usize {
        self.inner.wedged.load(Ordering::Acquire)
    }

    /// Ends hosting: no new jobs; each live, responsive host thread drops its instances'
    /// handles, runs the shutdown hook and ends. Waits at most `timeout` in all; a thread still
    /// inside a plug-in is left behind. True when every thread finished in time.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let workers: Vec<_> = lock(&self.inner.workers).clone();
        let mut pending = Vec::new();
        for w in workers.iter().filter(|w| w.responsive()) {
            let hook = self.inner.config.hooks.shutdown.clone();
            pending.push(self.submit_to(w, move |ctx| {
                ctx.entries.clear();
                if let Some(h) = hook {
                    h();
                }
                ctx.exit = true;
            }));
        }
        self.inner.closed.store(true, Ordering::Release);
        let mut all = true;
        for p in pending {
            all &= p.wait(deadline.saturating_duration_since(Instant::now())).is_some();
        }
        all
    }
}

/// A host thread's loop: jobs, OS messages, ticks.
fn run(worker: &Arc<Worker>, rx: &mpsc::Receiver<Arc<JobCell>>, queue: &MessageQueue, config: &Config, host: Weak<Inner>) {
    if let Some(start) = &config.hooks.start {
        worker.busy(|| catch_unwind(AssertUnwindSafe(|| start())).is_ok());
    }
    let mut ctx = Ctx { worker: Some(Arc::clone(worker)), host, timeout: config.timeout, entries: HashMap::new(), next_key: 0, exit: false };
    let mut last_tick = Instant::now();
    while !ctx.exit {
        while let Ok(cell) = rx.try_recv() {
            if let Some(job) = cell.take() {
                worker.busy(|| job(&mut ctx));
            }
            if ctx.exit {
                break;
            }
        }
        if ctx.exit {
            break;
        }
        worker.busy_as(true, || catch_unwind(AssertUnwindSafe(|| queue.dispatch(256))).is_ok());
        if last_tick.elapsed() >= config.tick {
            last_tick = Instant::now();
            worker.busy(|| {
                catch_unwind(AssertUnwindSafe(|| {
                    if let Some(tick) = &config.hooks.tick {
                        tick();
                    }
                    ctx.idle();
                }))
                .is_ok()
            });
        }
        queue.wait(config.tick.saturating_sub(last_tick.elapsed()).max(Duration::from_millis(1)));
    }
    worker.running.store(false, Ordering::Release);
    // Jobs queued behind the shutdown (an instance dropped late) are leaked: their plug-ins'
    // modules have exited, and the apartment ends with this thread.
    while let Ok(cell) = rx.try_recv() {
        std::mem::forget(cell.take());
    }
    drop(ctx);
}

/// The process's plug-in host, started on first use with [`Hooks::formats`] and the defaults.
pub fn shared() -> &'static Host {
    shared_cell().get_or_init(|| Host::new(Config { hooks: Hooks::formats(), ..Config::default() }))
}

fn shared_cell() -> &'static OnceLock<Host> {
    static HOST: OnceLock<Host> = OnceLock::new();
    &HOST
}

/// Shuts the process's plug-in host down if it was started (see [`Host::shutdown`]). Call it at
/// exit once the instances are gone, before the main thread's own module exits.
pub fn shutdown_shared(timeout: Duration) -> bool {
    shared_cell().get().is_none_or(|h| h.shutdown(timeout))
}
