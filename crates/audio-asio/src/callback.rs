//! The driver's four callbacks (`ASIOCallbacks`) and the process-wide state they reach.
//!
//! **Why process-wide.** The ASIO callbacks are plain C functions without a user-data pointer, and
//! ASIO allows one loaded driver per process (the host's `Slot`), so one callback table
//! ([`table`]) and one installed [`CallbackState`] (behind the atomic pointer `CURRENT`) serve it.
//! A state is installed before `createBuffers` and uninstalled after `disposeBuffers`; a second
//! install while one is in place fails.
//!
//! **Which thread.** `bufferSwitch`/`bufferSwitchTimeInfo` run on a thread the driver owns (not the
//! STA host thread), `asioMessage` and `sampleRateDidChange` on whatever thread the driver uses,
//! sometimes the host thread from inside a call it made. Nothing here blocks, allocates or takes a
//! lock: the [`Engine`] and the buffer addresses sit in `UnsafeCell`s guarded by one `busy` flag
//! that the callbacks only ever *try* to take (a callback that finds it taken does nothing and is
//! counted in `busy_skips`); the control side takes it, spinning, only while the driver is stopped or before it
//! starts (attach, restart), when no buffer switch runs. Driver messages that must reach the
//! owner's error callback set a pending bit and deliver it under the same flag, from whichever
//! thread gets it next; the holder rechecks the bit after releasing, so none is lost.
//!
//! **Lifetime.** The QPC read at entry and the in-flight count let [`Installed`]'s drop clear the
//! pointer and wait until no callback is inside the state before freeing it (and the user
//! callbacks with it), so the state outlives every callback that saw it.
//!
//! Every `unsafe` block has a `// SAFETY:` comment; the logic is in the safe [`crate::engine`].
#![deny(clippy::disallowed_macros, clippy::disallowed_methods)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use crate::abi::{self, AsioBool, AsioCallbacks, AsioLong, AsioSampleRate, AsioTime};
use crate::com::{CallbackDriver, Event};
use crate::convert::{InputPlanes, OutputPlanes};
use crate::engine::{Engine, Time};
use crate::mmcss::Mmcss;
use crate::stats::DriverEvents;
use std::cell::{Cell, UnsafeCell};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use transit_audio_backend::{HostOrigin, StreamError};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// The installed state, or null.
static CURRENT: AtomicPtr<CallbackState> = AtomicPtr::new(std::ptr::null_mut());
/// Callbacks between their entry and their exit, counted before `CURRENT` is read.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    /// How many of this crate's driver callbacks the calling thread is inside (a const-initialized
    /// `Cell`: no allocation and no destructor).
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Whether the calling thread is inside a driver callback (a buffer switch, `asioMessage`,
/// `sampleRateDidChange`): a control call made there must not wait for the host thread or call
/// the driver ([`crate::AsioStream`]'s `stop` defers to the host thread instead).
pub(crate) fn in_driver_callback() -> bool {
    DEPTH.with(Cell::get) > 0
}

/// How long [`Installed`]'s drop waits for callbacks still inside the state before it leaks it.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The pending bit of a reset request (or a rate change) not yet reported.
const PENDING_RESET: u8 = 1;
/// The pending bit of a stall the watchdog found, not yet reported.
const PENDING_STALL: u8 = 2;

/// The callback table handed to every `createBuffers`, in writable static memory: the SDK passes
/// it as `ASIOCallbacks *` and the driver keeps the pointer until `disposeBuffers`.
struct Table(UnsafeCell<AsioCallbacks>);

// SAFETY: Rust never reads or writes the table after initialization; only drivers read it (and
// the SDK lets them keep the pointer), through the raw pointer `table` returns.
unsafe impl Sync for Table {}

static TABLE: Table = Table(UnsafeCell::new(AsioCallbacks {
    bufferSwitch: Some(buffer_switch),
    sampleRateDidChange: Some(sample_rate_did_change),
    asioMessage: Some(asio_message),
    bufferSwitchTimeInfo: Some(buffer_switch_time_info),
}));

/// The `ASIOCallbacks *` for `createBuffers`; valid for the life of the process.
pub(crate) fn table() -> *mut AsioCallbacks {
    TABLE.0.get()
}

/// The QPC now, in 100 ns units; allocation-free (the `transit-audio-wasapi` reading).
pub(crate) fn qpc_100ns() -> u64 {
    let (mut ticks, mut frequency) = (0i64, 0i64);
    // SAFETY: QueryPerformanceCounter and QueryPerformanceFrequency fill live i64s and cannot
    // fail on Windows XP and later; a failure would leave zeros, which gives a zero time.
    unsafe {
        let _ = QueryPerformanceCounter(&mut ticks);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    (ticks.unsigned_abs() as u128 * 10_000_000 / frequency.unsigned_abs().max(1) as u128) as u64
}

/// The `(QPC in 100 ns units, host-origin nanoseconds)` pair of one instant (`origin_pair` of
/// `transit-audio-wasapi`): each stream takes its own at `start()` against the Player's origin.
pub(crate) fn origin_pair(origin: HostOrigin) -> (u64, i64) {
    let qpc = qpc_100ns();
    let instant = Instant::now();
    let origin_ns = match instant.checked_duration_since(origin.instant()) {
        Some(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        None => -i64::try_from(origin.instant().duration_since(instant).as_nanos()).unwrap_or(i64::MAX),
    };
    (qpc, origin_ns)
}

/// The two buffer addresses of each created channel and the byte length of one buffer.
#[derive(Debug, Default)]
pub(crate) struct PlaneTable {
    pub(crate) outputs: Vec<[usize; 2]>,
    pub(crate) inputs: Vec<[usize; 2]>,
    pub(crate) output_bytes: usize,
    pub(crate) input_bytes: usize,
}

impl PlaneTable {
    /// Zeroes both halves of every output buffer (drivers hand them over uninitialized).
    pub(crate) fn silence_outputs(&self) {
        for half in 0..2 {
            crate::convert::silence(&mut RawOutputs { table: &self.outputs, half, bytes: self.output_bytes });
        }
    }
}

/// The output buffers of one half, from the addresses `createBuffers` returned.
struct RawOutputs<'a> {
    table: &'a [[usize; 2]],
    half: usize,
    bytes: usize,
}

impl OutputPlanes for RawOutputs<'_> {
    fn count(&self) -> usize {
        self.table.len()
    }

    fn plane(&mut self, channel: usize) -> &mut [u8] {
        let Some(address) = self.table.get(channel).and_then(|a| a.get(self.half)).copied().filter(|a| *a != 0) else { return &mut [] };
        // SAFETY: `address` is one of the two buffers `createBuffers` returned for this output
        // channel, `bytes` long (frames times the sample width), valid until `disposeBuffers`
        // ("buffers are valid until ASIODisposeBuffers"). It is reached only under the state's
        // `busy` flag (or during attach, before the driver starts), so no other Rust reference to
        // it exists, and `&mut self` keeps this one unique until it ends. During the buffer switch
        // for this half the driver works on the other half.
        unsafe { std::slice::from_raw_parts_mut(address as *mut u8, self.bytes) }
    }
}

/// The input buffers of one half.
struct RawInputs<'a> {
    table: &'a [[usize; 2]],
    half: usize,
    bytes: usize,
}

impl InputPlanes for RawInputs<'_> {
    fn count(&self) -> usize {
        self.table.len()
    }

    fn plane(&self, channel: usize) -> &[u8] {
        let Some(address) = self.table.get(channel).and_then(|a| a.get(self.half)).copied().filter(|a| *a != 0) else { return &[] };
        // SAFETY: as for `RawOutputs::plane`: a `createBuffers` input buffer of `bytes` bytes, valid
        // until `disposeBuffers`, read under `busy` while the driver fills the other half.
        unsafe { std::slice::from_raw_parts(address as *const u8, self.bytes) }
    }
}

/// Everything the callbacks of one driver session touch. Built on the host thread, installed for
/// the callbacks by [`Installed`].
pub(crate) struct CallbackState {
    busy: AtomicBool,
    engine: UnsafeCell<Engine>,
    planes: UnsafeCell<PlaneTable>,
    driver: CallbackDriver,
    output_ready: AtomicBool,
    pending: AtomicU8,
    resync: AtomicBool,
    /// The session's rate, as `f64` bits, for `sampleRateDidChange`.
    rate_bits: AtomicU64,
    pub(crate) events: Arc<DriverEvents>,
    /// Set by `kAsioLatenciesChanged`; the host thread refetches `getLatencies`.
    pub(crate) latencies_dirty: AtomicBool,
    wake: Arc<Event>,
    /// The QPC (100 ns) of the last buffer switch's entry, 0 before the first since the driver
    /// started: what the host thread's watchdog reads.
    last_switch: AtomicU64,
    /// The buffer-switch thread's MMCSS registration, tended at each switch.
    mmcss: Arc<Mmcss>,
}

// SAFETY: the `UnsafeCell`s are only accessed under `busy` (an acquire compare-exchange to take it,
// a release store to give it back), which makes those accesses mutually exclusive across threads;
// everything else is atomics, `Arc`s of atomics, the `Sync` `Event` and the `Sync` `CallbackDriver`.
// The engine's user callbacks are `Send`, so running them on whichever thread holds `busy` is fine.
unsafe impl Sync for CallbackState {}

impl CallbackState {
    pub(crate) fn new(engine: Engine, driver: CallbackDriver, events: Arc<DriverEvents>, wake: Arc<Event>, mmcss: Arc<Mmcss>) -> Box<Self> {
        let rate = f64::from(engine.rate());
        Box::new(Self {
            busy: AtomicBool::new(false),
            engine: UnsafeCell::new(engine),
            planes: UnsafeCell::new(PlaneTable::default()),
            driver,
            output_ready: AtomicBool::new(false),
            pending: AtomicU8::new(0),
            resync: AtomicBool::new(false),
            rate_bits: AtomicU64::new(rate.to_bits()),
            events,
            latencies_dirty: AtomicBool::new(false),
            wake,
            last_switch: AtomicU64::new(0),
            mmcss,
        })
    }

    fn try_take(&self) -> bool {
        self.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed).is_ok()
    }

    fn give_back(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }

    /// Runs `f` on the engine and the buffer addresses on the control side. Spins (yielding) for
    /// the flag; call it only when no buffer switch runs (before `start`, after `stop`), where it
    /// is free at once or after one `asioMessage` delivery.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Engine, &mut PlaneTable) -> R) -> R {
        while !self.try_take() {
            std::thread::yield_now();
        }
        // SAFETY: `busy` is held: no other access to the cells exists until `give_back`.
        let (engine, planes) = unsafe { (&mut *self.engine.get(), &mut *self.planes.get()) };
        let r = f(engine, planes);
        self.give_back();
        self.deliver_pending();
        r
    }

    pub(crate) fn set_output_ready(&self, supported: bool) {
        self.output_ready.store(supported, Ordering::Relaxed);
    }

    /// One buffer switch on the driver's thread.
    fn switch(&self, index: AsioLong, time: Time, arrival_100ns: u64) {
        self.last_switch.store(arrival_100ns, Ordering::Relaxed);
        self.mmcss.tend();
        if !self.try_take() {
            self.events.busy_skips.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: `busy` is held (taken above): the only access to the cells until `give_back`.
        let (engine, planes) = unsafe { (&mut *self.engine.get(), &*self.planes.get()) };
        if self.resync.swap(false, Ordering::Relaxed) {
            engine.resync();
        }
        let half = usize::from(index & 1 == 1);
        let mut outputs = RawOutputs { table: &planes.outputs, half, bytes: planes.output_bytes };
        let inputs = RawInputs { table: &planes.inputs, half, bytes: planes.input_bytes };
        engine.switch(time, arrival_100ns, &mut outputs, &inputs);
        if self.output_ready.load(Ordering::Relaxed) {
            self.driver.output_ready();
        }
        engine.finish(arrival_100ns, qpc_100ns());
        Self::deliver_locked(&self.pending, engine);
        self.give_back();
        self.deliver_pending();
    }

    fn deliver_locked(pending: &AtomicU8, engine: &mut Engine) {
        let bits = pending.swap(0, Ordering::SeqCst);
        if bits & PENDING_RESET != 0 {
            engine.report(StreamError::ResetRequested);
        }
        if bits & PENDING_STALL != 0 {
            engine.report(StreamError::Stalled);
        }
    }

    /// The QPC of the last buffer switch since the driver started, 0 before the first.
    pub(crate) fn last_switch(&self) -> u64 {
        self.last_switch.load(Ordering::Relaxed)
    }

    /// The driver (re)starts: the watchdog waits for its first buffer switch again.
    pub(crate) fn forget_last_switch(&self) {
        self.last_switch.store(0, Ordering::Relaxed);
    }

    /// The watchdog found the driver had stopped calling back (host thread): counted, and each
    /// open stream gets `StreamError::Stalled` once, delivered at once when no buffer switch
    /// holds the flag (else by its holder when it lets go).
    pub(crate) fn stalled(&self) {
        self.events.timeouts.fetch_add(1, Ordering::Relaxed);
        self.pending.fetch_or(PENDING_STALL, Ordering::SeqCst);
        self.deliver_pending();
    }

    /// Delivers pending errors if the flag is free; a holder delivers them before it lets go.
    fn deliver_pending(&self) {
        while self.pending.load(Ordering::SeqCst) != 0 {
            if !self.try_take() {
                return;
            }
            // SAFETY: `busy` is held (taken above).
            let engine = unsafe { &mut *self.engine.get() };
            Self::deliver_locked(&self.pending, engine);
            self.give_back();
        }
    }

    fn reset_requested(&self) {
        self.events.resets.fetch_add(1, Ordering::Relaxed);
        self.pending.fetch_or(PENDING_RESET, Ordering::SeqCst);
        self.deliver_pending();
    }

    fn message(&self, selector: AsioLong) -> AsioLong {
        match selector {
            abi::K_ASIO_RESET_REQUEST => {
                self.reset_requested();
                1
            }
            abi::K_ASIO_RESYNC_REQUEST => {
                self.events.resets.fetch_add(1, Ordering::Relaxed);
                self.resync.store(true, Ordering::Relaxed);
                1
            }
            abi::K_ASIO_LATENCIES_CHANGED => {
                self.latencies_dirty.store(true, Ordering::Relaxed);
                self.wake.set();
                1
            }
            abi::K_ASIO_OVERLOAD => {
                self.events.overloads.fetch_add(1, Ordering::Relaxed);
                0
            }
            _ => 0,
        }
    }

    fn rate_changed(&self, rate: f64) {
        let ours = f64::from_bits(self.rate_bits.load(Ordering::Relaxed));
        if (rate - ours).abs() > 0.5 {
            self.reset_requested();
        }
    }
}

/// An installed [`CallbackState`]. Dropping it uninstalls the state, waits for callbacks still
/// inside it and frees it (with the user callbacks). Drop it only after `disposeBuffers`, or at
/// least after `stop`, so the driver has no reason to call again.
pub(crate) struct Installed {
    state: *mut CallbackState,
}

impl Installed {
    /// Makes `state` the one the callbacks reach. Fails, giving it back, while another is installed.
    pub(crate) fn install(state: Box<CallbackState>) -> Result<Self, Box<CallbackState>> {
        let raw = Box::into_raw(state);
        match CURRENT.compare_exchange(std::ptr::null_mut(), raw, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => Ok(Self { state: raw }),
            // SAFETY: `raw` came from `Box::into_raw` just above and was not published.
            Err(_) => Err(unsafe { Box::from_raw(raw) }),
        }
    }

    pub(crate) fn state(&self) -> &CallbackState {
        // SAFETY: the box is freed only in `Drop`, after which `self` is gone.
        unsafe { &*self.state }
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = CURRENT.compare_exchange(self.state, std::ptr::null_mut(), Ordering::SeqCst, Ordering::SeqCst);
        // A callback that read the pointer before the swap counted itself in first (SeqCst both
        // sides), so once the count is zero no callback can still see the state.
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while IN_FLIGHT.load(Ordering::SeqCst) != 0 {
            if Instant::now() > deadline {
                log::warn!("ASIO: a driver callback did not return within {} s; its state is leaked", DRAIN_TIMEOUT.as_secs());
                return;
            }
            std::thread::yield_now();
        }
        // SAFETY: `state` came from `Box::into_raw` in `install`, is no longer reachable through
        // `CURRENT` and no callback is inside it (above); it is freed once, here.
        drop(unsafe { Box::from_raw(self.state) });
    }
}

/// A callback's stay inside the installed state: counted in before `CURRENT` is read.
struct Flight;

impl Flight {
    fn enter() -> Self {
        DEPTH.with(|depth| depth.set(depth.get() + 1));
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Flight
    }

    fn state(&self) -> Option<&CallbackState> {
        let state = CURRENT.load(Ordering::SeqCst);
        // SAFETY: a non-null `CURRENT` points to a live installed state; `Installed`'s drop clears
        // it and then waits until `IN_FLIGHT` (which counts this `Flight`) is zero before freeing,
        // so the state outlives the borrow, which is tied to this `Flight`.
        (!state.is_null()).then(|| unsafe { &*state })
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// `bufferSwitch(index, directProcess)`: drivers that do not use time info; the position comes
/// from `getSamplePosition`.
unsafe extern "C" fn buffer_switch(index: AsioLong, _direct_process: AsioBool) {
    let arrival = qpc_100ns();
    let flight = Flight::enter();
    let Some(state) = flight.state() else { return };
    let time = state
        .driver
        .sample_position()
        .map_or_else(Time::default, |(position, system)| Time { position: Some(position), system_time_ns: (system != 0).then_some(system) });
    state.switch(index, time, arrival);
}

/// `bufferSwitchTimeInfo(params, index, directProcess)`: the time info carries the position.
unsafe extern "C" fn buffer_switch_time_info(params: *mut AsioTime, index: AsioLong, _direct_process: AsioBool) -> *mut AsioTime {
    let arrival = qpc_100ns();
    let flight = Flight::enter();
    let Some(state) = flight.state() else { return params };
    let time = if params.is_null() {
        Time::default()
    } else {
        // SAFETY: the driver passes an `ASIOTime` (148 bytes, layout asserted in `abi`) that is valid
        // for the duration of the call; it is 4-byte packed, so the time info is copied out with
        // an unaligned read and no reference into it is made.
        let info = unsafe { std::ptr::addr_of!((*params).timeInfo).read_unaligned() };
        Time {
            position: (info.flags & abi::K_SAMPLE_POSITION_VALID != 0).then(|| info.samplePosition.value()),
            system_time_ns: (info.flags & abi::K_SYSTEM_TIME_VALID != 0).then(|| info.systemTime.value()),
        }
    };
    state.switch(index, time, arrival);
    params
}

/// `sampleRateDidChange(rate)`: a change under a running stream is a reset of the stream.
unsafe extern "C" fn sample_rate_did_change(rate: AsioSampleRate) {
    let flight = Flight::enter();
    if let Some(state) = flight.state() {
        state.rate_changed(rate);
    }
}

/// `asioMessage(selector, value, message, opt)`.
unsafe extern "C" fn asio_message(selector: AsioLong, value: AsioLong, _message: *mut c_void, _opt: *mut f64) -> AsioLong {
    match selector {
        abi::K_ASIO_SELECTOR_SUPPORTED => AsioLong::from(matches!(
            value,
            abi::K_ASIO_ENGINE_VERSION
                | abi::K_ASIO_RESET_REQUEST
                | abi::K_ASIO_RESYNC_REQUEST
                | abi::K_ASIO_LATENCIES_CHANGED
                | abi::K_ASIO_SUPPORTS_TIME_INFO
                | abi::K_ASIO_SUPPORTS_TIME_CODE
                | abi::K_ASIO_OVERLOAD
        )),
        abi::K_ASIO_ENGINE_VERSION => 2,
        abi::K_ASIO_SUPPORTS_TIME_INFO => abi::ASIO_TRUE,
        // No time code; a buffer size change is not handled in place ("use kAsioResetRequest").
        abi::K_ASIO_SUPPORTS_TIME_CODE | abi::K_ASIO_BUFFER_SIZE_CHANGE => 0,
        other => {
            let flight = Flight::enter();
            match flight.state() {
                Some(state) => state.message(other),
                // Before a session or after it: acknowledge what needs no state.
                None => AsioLong::from(matches!(other, abi::K_ASIO_RESET_REQUEST | abi::K_ASIO_RESYNC_REQUEST | abi::K_ASIO_LATENCIES_CHANGED)),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Calls into the table the way a driver does, from the test thread.
    use super::*;

    fn callbacks() -> AsioCallbacks {
        // SAFETY: the table is never written after initialization; a copy of its `Copy` value.
        unsafe { table().read() }
    }

    /// `bufferSwitchTimeInfo` with a time info of `position` and `system_ns` (each marked valid when
    /// `Some`).
    pub(crate) fn switch_time_info(index: i32, position: Option<u64>, system_ns: Option<u64>) {
        let mut time: AsioTime = AsioTime {
            reserved: [0; 4],
            timeInfo: abi::AsioTimeInfo {
                speed: 1.0,
                systemTime: abi::AsioTimeStamp::from_value(system_ns.unwrap_or(0)),
                samplePosition: abi::AsioSamples::from_value(position.unwrap_or(0)),
                sampleRate: 48_000.0,
                flags: if position.is_some() { abi::K_SAMPLE_POSITION_VALID } else { 0 }
                    | if system_ns.is_some() { abi::K_SYSTEM_TIME_VALID } else { 0 },
                reserved: [0; 12],
            },
            timeCode: abi::AsioTimeCode { speed: 0.0, timeCodeSamples: abi::AsioSamples::default(), flags: 0, future: [0; 64] },
        };
        let f = callbacks().bufferSwitchTimeInfo.expect("bufferSwitchTimeInfo");
        // SAFETY: a valid `ASIOTime` for the duration of the call, as a driver passes it.
        let back = unsafe { f(&mut time, index, abi::ASIO_TRUE) };
        assert_eq!(back, std::ptr::addr_of_mut!(time));
    }

    /// `bufferSwitch` (the driver then answers `getSamplePosition`).
    pub(crate) fn switch_plain(index: i32) {
        let f = callbacks().bufferSwitch.expect("bufferSwitch");
        // SAFETY: plain values, as a driver calls it.
        unsafe { f(index, abi::ASIO_TRUE) }
    }

    pub(crate) fn message(selector: i32, value: i32) -> i32 {
        let f = callbacks().asioMessage.expect("asioMessage");
        // SAFETY: null message and opt pointers, which these selectors do not read.
        unsafe { f(selector, value, std::ptr::null_mut(), std::ptr::null_mut()) }
    }

    pub(crate) fn rate_changed(rate: f64) {
        let f = callbacks().sampleRateDidChange.expect("sampleRateDidChange");
        // SAFETY: a plain value.
        unsafe { f(rate) }
    }

    /// Whether a state is installed.
    pub(crate) fn installed() -> bool {
        !CURRENT.load(Ordering::SeqCst).is_null()
    }
}
