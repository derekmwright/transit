//! `Vst3Plugin`: a loaded VST3 instance behind the safe `soundcraft_dsp::Plugin` trait.
//!
//! # `restartComponent` (audit F08, ADR 0012 G7)
//!
//! The edit controller calls `IComponentHandler::restartComponent` on the UI thread; the host
//! records the flags ([`HostContext`]) and services them on the instance's main thread in
//! [`Shared::service`], called by [`crate::service_requests`] (every instance) and by each editor
//! handle's `idle`. Nothing below runs on the audio thread:
//!
//! - `kMidiCCAssignmentChanged` ("the host has to rebuild the MIDI-CC => parameter mapping"):
//!   `getMidiControllerAssignment` again, and the table is published to the audio side as one
//!   unit ([`MidiMap`], a sequence lock). The audio side checks the table's generation before it
//!   routes each controller, so the first controller after the publication uses the new table.
//!   A learn during a state load (`Plugin::load_state`, which owns the instance on this thread)
//!   is re-read before `load_state` returns.
//! - `kLatencyChanged` ("the host has to deactivate and activate the plug-in, then afterwards
//!   ... ask for the current latency") and `kIoChanged` ("deactivate ..., ask for its wanted new
//!   bus configurations ... and reactivate"): `setProcessing(false)`, `setActive(false)`, bus
//!   negotiation, `setupProcessing`, `setActive(true)` and `getLatencySamples`, all on the main
//!   thread (VST3 allows `setProcessing` on either thread), while holding the instance so the audio
//!   side skips its blocks meanwhile; the values the host set are sent again, as after
//!   `prepare`. The latency reaches delay compensation through [`Plugin::latency`].
//! - `kParamValuesChanged` ("the host invalidates all caches of parameter values and asks the
//!   edit controller for the current values"): `getParamNormalized` for every parameter, 64 per
//!   call; the values that differ from the last ones read are reported like editor edits (the
//!   path a GUI edit takes into the session). A state load re-reads the values itself and
//!   reports nothing (that is the host's own change).
//! - `kParamTitlesChanged`: the conversion curves are invalidated (the editor handle re-samples
//!   them). Titles, ranges and step counts in `PluginInfo` stay as read at load (`'static`).
//! - `kReloadComponent` ("the host has to unload completely the component ... and reload it"):
//!   not supported here (the instance would have to be created again by its owner); logged as a
//!   warning and counted in [`Serviced::unsupported`]. Other flags need nothing from this host.
//!
//! The audio side only `try_lock`s the instance and the main thread only `try_lock`s it while it
//! may be processing, so neither ever waits for the other; a busy instance is serviced by a later
//! call. Servicing happens only on the thread that created the instance.

use crate::ffi::{AFTERTOUCH, BusBuffers, EVENT_CAP, EditorLink, HostContext, Instance, NoteEvent, PARAM_QUEUE_CAP, PITCH_BEND};
use crate::params::{ParamCurves, PendingParams};
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, ProcessStatus, Transport};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError, Weak};
use std::thread::ThreadId;
use std::time::Duration;
use vst3::ComWrapper;

/// Largest block we set a plugin up for.
const MAX_BLOCK: usize = 1 << 16;
/// Most channels a host strip may ask for.
const MAX_CHANNELS: usize = 64;
/// An `unsynced` slot with no value waiting (a NaN pattern, which `set_param` never stores).
const SYNCED: u64 = u64::MAX;
/// Most curves one editor `idle` samples, and the time after which it stops (it always samples
/// one when any is due).
const CURVE_BUDGET_TABLES: usize = 64;
const CURVE_BUDGET_TIME: Duration = Duration::from_millis(2);
/// Entries of the MIDI controller table: CCs 0–127, aftertouch and pitch bend.
const MAP_LEN: usize = PITCH_BEND + 1;
/// VST3's `kNoParamId`: no parameter.
const NO_PARAM: u32 = u32::MAX;
/// Parameter values one service call re-reads for `kParamValuesChanged`: it holds the instance
/// (the audio side skips a block it would start meanwhile), so the hold stays short.
const VALUES_CHUNK: usize = 64;

/// How a SoundCraft parameter value maps to the VST3 normalized value.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mapping {
    /// Any stepped parameter (`stepCount > 0`, however many steps): step index `0..=steps` ↔
    /// `index / steps`, exactly.
    Stepped(f64),
    /// Plain value through the controller's curve, sampled on the UI thread ([`ParamCurves`]).
    Plain,
    /// Linear over the info range (the controller gave no usable plain range).
    Linear,
}

/// The status a VST3 plugin implies through its tail: an infinite tail (`kInfiniteTail`) must
/// keep processing; a finite one (including none) may stop that many samples after the input
/// goes silent.
fn status_from_tail(tail: u32) -> ProcessStatus {
    if tail == u32::MAX { ProcessStatus::Continue } else { ProcessStatus::Tail }
}

/// What one [`crate::service_requests`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Serviced {
    /// Instances deactivated and reactivated (`kLatencyChanged`, `kIoChanged`).
    pub restarted: usize,
    /// Reactivations waiting because the instance was busy on the audio thread.
    pub restarts_waiting: usize,
    /// `IMidiMapping` tables re-read and published (`kMidiCCAssignmentChanged`).
    pub midi_maps: usize,
    /// Parameter value re-reads finished (`kParamValuesChanged`).
    pub value_rescans: usize,
    /// Requests this host cannot honor (`kReloadComponent`), logged.
    pub unsupported: usize,
    /// Instances skipped because this is not the thread that created them.
    pub other_thread: usize,
}

/// The MIDI controller → parameter table, written on the main thread and read on the audio
/// thread as one unit: a sequence lock over atomics (odd sequence: a write is in progress).
struct MidiMap {
    seq: AtomicU64,
    ids: [AtomicU32; MAP_LEN],
}

impl MidiMap {
    fn new() -> MidiMap {
        MidiMap { seq: AtomicU64::new(0), ids: std::array::from_fn(|_| AtomicU32::new(NO_PARAM)) }
    }

    /// Publishes a table (main thread only: one writer). Missing entries are unmapped.
    fn publish(&self, map: &[Option<u32>]) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (i, slot) in self.ids.iter().enumerate() {
            slot.store(map.get(i).copied().flatten().unwrap_or(NO_PARAM), Ordering::Relaxed);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    fn generation(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Copies the table into `out` (no allocation). The generation read, or `None` when a write
    /// was in progress (`out` is then partly overwritten: the caller keeps its other copy).
    fn read(&self, out: &mut [Option<u32>; MAP_LEN]) -> Option<u64> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return None;
        }
        for (o, slot) in out.iter_mut().zip(&self.ids) {
            let v = slot.load(Ordering::Relaxed);
            *o = (v != NO_PARAM).then_some(v);
        }
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == s1).then_some(s1)
    }
}

/// What the activation decides: the bus buffers, sized on the main thread.
struct Core {
    inst: Instance,
    inputs: BusBuffers,
    outputs: BusBuffers,
    has_input: bool,
    has_output: bool,
    /// The block size it is set up for, and 0 while it is not (failed or never prepared).
    max_block: usize,
    /// What `prepare` asked for, to set it up again the same way.
    block: usize,
    sample_rate: f64,
    mono: bool,
}

/// A `kParamValuesChanged` re-read (main thread only): the last values read and the next one.
struct Values {
    /// Normalized, per parameter (same order as `ids`; NaN: never read).
    known: Vec<f64>,
    next: usize,
    active: bool,
}

/// The part of an instance both its owner (the audio thread, once in the engine) and the main
/// thread reach (see the module docs).
pub(crate) struct Shared {
    core: Mutex<Core>,
    latency: AtomicUsize,
    tail: AtomicUsize,
    event_input: AtomicBool,
    /// Bumped by every (re)activation: the audio side then sends the values the host set again.
    activation: AtomicU64,
    map: MidiMap,
    /// The thread that created the instance: its main (UI) thread.
    main: ThreadId,
    host: ComWrapper<HostContext>,
    curves: Arc<ParamCurves>,
    info: &'static PluginInfo,
    ids: Vec<u32>,
    values: Mutex<Values>,
}

fn try_lock<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// For callers that own the plugin (`&mut Vst3Plugin`) or main-thread-only state: nobody else
/// can be holding it then.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every instance alive, for [`crate::service_requests`]. Weak: the list never keeps a plugin.
fn live() -> &'static Mutex<Vec<Weak<Shared>>> {
    static LIVE: OnceLock<Mutex<Vec<Weak<Shared>>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Services the `restartComponent` requests of every live instance created on this thread.
pub(crate) fn service_requests() -> Serviced {
    let mut out = Serviced::default();
    let all: Vec<Arc<Shared>> = {
        let mut l = lock(live());
        l.retain(|w| w.strong_count() > 0);
        l.iter().filter_map(Weak::upgrade).collect()
    };
    // The list lock is released before calling into plug-ins. An instance whose owner dropped it
    // meanwhile goes here, on the main thread.
    for s in all {
        s.service(&mut out);
    }
    out
}

impl Shared {
    /// (Re)activates on the main thread at the size `prepare` asked for, then publishes what the
    /// audio side reads: latency, tail, event input, the MIDI table and a new activation.
    fn activate(&self, core: &mut Core) {
        // Cleared first: a request made while it reactivates asks again.
        self.host.io_changed.store(false, Ordering::Relaxed);
        self.host.midi_cc_changed.store(false, Ordering::Relaxed);
        core.max_block = 0;
        match core.inst.configure(core.sample_rate, core.block, core.mono) {
            Ok(layout) => {
                core.inputs = BusBuffers::new(&layout.inputs, core.block);
                core.outputs = BusBuffers::new(&layout.outputs, core.block);
                core.has_input = layout.inputs.first().is_some_and(|&c| c > 0);
                core.has_output = layout.outputs.first().is_some_and(|&c| c > 0);
                core.max_block = core.block;
                self.event_input.store(layout.event_input, Ordering::Relaxed);
                // VST3: getLatencySamples returns the new latency after setActive(true).
                self.latency.store(core.inst.latency() as usize, Ordering::Release);
                self.tail.store(core.inst.tail() as usize, Ordering::Relaxed);
                self.map.publish(&core.inst.midi_mapping());
            }
            Err(e) => {
                log::warn!("{}: {e}", self.info.id);
                self.latency.store(0, Ordering::Release);
            }
        }
        self.activation.fetch_add(1, Ordering::Release);
    }

    /// Main-thread service of this instance's `restartComponent` flags (see the module docs).
    pub(crate) fn service(&self, out: &mut Serviced) {
        if std::thread::current().id() != self.main {
            out.other_thread += 1;
            return;
        }
        let h = &self.host;
        h.restart_requested.store(false, Ordering::Relaxed);
        if h.titles_changed.swap(false, Ordering::Relaxed) {
            self.curves.invalidate();
        }
        if h.reload_requested.swap(false, Ordering::Relaxed) {
            log::warn!(
                "{}: the plug-in asked to be reloaded (kReloadComponent); not supported, remove and re-add it (or reopen the session)",
                self.info.id
            );
            out.unsupported += 1;
        }
        let other = h.other_flags.swap(0, Ordering::Relaxed);
        if other != 0 {
            log::debug!("{}: restartComponent flags {other:#x} need nothing from this host", self.info.id);
        }
        if h.midi_cc_changed.load(Ordering::Acquire)
            && let Some(core) = try_lock(&self.core)
        {
            h.midi_cc_changed.store(false, Ordering::Relaxed);
            let map = core.inst.midi_mapping();
            drop(core);
            self.map.publish(&map);
            out.midi_maps += 1;
        }
        if self.rescan_values() {
            out.value_rescans += 1;
        }
        if h.latency_changed.load(Ordering::Acquire) || h.io_changed.load(Ordering::Acquire) {
            match try_lock(&self.core) {
                Some(mut core) => {
                    self.activate(&mut core);
                    out.restarted += 1;
                }
                None => out.restarts_waiting += 1,
            }
        }
    }

    /// `kParamValuesChanged`. True when a re-read finished in this call; a busy instance is
    /// retried by the next call.
    fn rescan_values(&self) -> bool {
        let mut st = lock(&self.values);
        if !st.active {
            if !self.host.values_changed.swap(false, Ordering::Acquire) {
                return false;
            }
            st.active = true;
            st.next = 0;
        }
        let Some(core) = try_lock(&self.core) else { return false };
        let start = st.next.min(self.ids.len());
        let end = (start + VALUES_CHUNK).min(self.ids.len());
        let read: Vec<(usize, f64)> = (start..end).filter_map(|i| core.inst.normalized_value(*self.ids.get(i)?).map(|v| (i, v))).collect();
        drop(core);
        let mut changed = Vec::new();
        for (i, v) in read {
            if let (Some(k), Some(&id)) = (st.known.get_mut(i), self.ids.get(i))
                && k.to_bits() != v.to_bits()
            {
                *k = v;
                changed.push((id, v));
            }
        }
        if !changed.is_empty() {
            // The values go where GUI edits go (the editor handle reports them to the session).
            let mut edits = lock(&self.host.edits);
            for (id, v) in changed {
                if let Some(e) = edits.iter_mut().find(|(i, _)| *i == id) {
                    e.1 = v;
                } else if edits.len() < edits.capacity() {
                    edits.push((id, v));
                }
            }
        }
        st.next = end;
        if end < self.ids.len() {
            return false;
        }
        st.active = false;
        true
    }

    /// The controller's values become the known ones (after a load the host made itself).
    fn remember_values(&self, inst: &Instance) {
        let mut st = lock(&self.values);
        for (k, &id) in st.known.iter_mut().zip(&self.ids) {
            *k = inst.normalized_value(id).unwrap_or(f64::NAN);
        }
        st.active = false;
    }
}

/// A hosted VST3 plugin. Parameter ids are VST3 `ParamID`s as decimal strings ("0", "17", …);
/// values are plain (display) values with their names and ranges in `info().params`.
pub struct Vst3Plugin {
    shared: Arc<Shared>,
    info: &'static PluginInfo,
    /// VST3 id of each `info.params` entry (same order).
    ids: Vec<u32>,
    maps: Vec<Mapping>,
    values: Vec<f32>,
    /// Which values the host set since creation or the last state load (same order as `ids`):
    /// only these are re-sent on activation; values read from the plugin never are. `unsynced`
    /// cannot tell, since the state reader clears it once the controller has a value.
    host_set: Vec<bool>,
    /// The controller's curves of the `Mapping::Plain` parameters, shared with the editor handle,
    /// which re-samples them on the UI thread when the plugin asks for a restart.
    curves: Arc<ParamCurves>,
    /// The curves' generation the queued values were converted with.
    curves_seen: u64,
    /// Normalized values waiting for the next block (latest per parameter; never drops).
    pending: PendingParams,
    /// One block's share of `pending`, handed to the processor (capacity `PARAM_QUEUE_CAP`).
    batch: Vec<(u32, f64)>,
    /// Host-set plain values the controller has not seen yet (same order as `ids`; the bits
    /// of an `f64`, or [`SYNCED`]). `set_param` may run on the audio thread, so it never calls the
    /// controller; the state reader, which already does, hands them over first.
    unsynced: Arc<[AtomicU64]>,
    /// Pending notes, kept sorted by time. Never grows past `EVENT_CAP`.
    notes: Vec<NoteEvent>,
    /// The parameter each MIDI controller drives (a copy of [`Shared::map`], see
    /// `Instance::midi_mapping`), and a second table to copy into.
    midi_map: [Option<u32>; MAP_LEN],
    map_scratch: [Option<u32>; MAP_LEN],
    /// The [`MidiMap`] generation `midi_map` holds.
    map_seen: u64,
    /// The [`Shared::activation`] whose values were last re-sent.
    activation_seen: u64,
    /// Controller points for the next block: (time, param id, normalized value). Bounded.
    controls: Vec<(u32, u32, f64)>,
    held: [bool; 128],
    channels: usize,
    /// Every audio input of the next block is silent (VST3 silence flags).
    silent: bool,
    position: i64,
    transport: Option<Transport>,
}

impl Vst3Plugin {
    pub(crate) fn new(inst: Instance, info: &'static PluginInfo) -> Vst3Plugin {
        let ids: Vec<u32> = info.params.iter().map(|p| p.id.parse::<u32>().unwrap_or(u32::MAX)).collect();
        // Any stepped parameter (stepCount > 0) converts exactly, however many steps it has; the
        // registry's MAX_CHOICES only decides whether the UI gets a choice list.
        let steps: std::collections::HashMap<u32, i32> =
            inst.params().into_iter().filter(|r| r.step_count > 0).map(|r| (r.id, r.step_count)).collect();
        let maps: Vec<Mapping> = info
            .params
            .iter()
            .zip(&ids)
            .map(|(p, &id)| {
                if let Some(&n) = steps.get(&id) {
                    return Mapping::Stepped(f64::from(n));
                }
                match (inst.to_plain(id, 0.0), inst.to_plain(id, 1.0)) {
                    (Some(lo), Some(hi))
                        if (lo as f32 - p.min).abs() <= 1e-6 * p.min.abs().max(1.0) && (hi as f32 - p.max).abs() <= 1e-6 * p.max.abs().max(1.0) =>
                    {
                        Mapping::Plain
                    }
                    _ => Mapping::Linear,
                }
            })
            .collect();
        // Creation runs on the UI (control) thread. No curve is sampled here (a plug-in may have
        // thousands of parameters): the editor handle's idle samples them lazily and on demand.
        let plain: Vec<bool> = maps.iter().map(|m| *m == Mapping::Plain).collect();
        let curves = Arc::new(ParamCurves::new(&ids, &plain));
        let known: Vec<f64> = ids.iter().map(|&id| inst.normalized_value(id).unwrap_or(f64::NAN)).collect();
        let host = inst.host_ref();
        let shared = Arc::new(Shared {
            core: Mutex::new(Core {
                inst,
                inputs: BusBuffers::new(&[], 0),
                outputs: BusBuffers::new(&[], 0),
                has_input: false,
                has_output: false,
                max_block: 0,
                block: 0,
                sample_rate: 48_000.0,
                mono: false,
            }),
            latency: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            event_input: AtomicBool::new(false),
            activation: AtomicU64::new(0),
            map: MidiMap::new(),
            main: std::thread::current().id(),
            host,
            curves: Arc::clone(&curves),
            info,
            ids: ids.clone(),
            values: Mutex::new(Values { known, next: 0, active: false }),
        });
        lock(live()).push(Arc::downgrade(&shared));
        let mut p = Vst3Plugin {
            info,
            values: Vec::with_capacity(ids.len()),
            host_set: vec![false; ids.len()],
            curves_seen: curves.generation(),
            curves,
            pending: PendingParams::new(ids.len()),
            batch: Vec::with_capacity(PARAM_QUEUE_CAP),
            unsynced: ids.iter().map(|_| AtomicU64::new(SYNCED)).collect(),
            notes: Vec::with_capacity(EVENT_CAP),
            midi_map: [None; MAP_LEN],
            map_scratch: [None; MAP_LEN],
            map_seen: u64::MAX,
            activation_seen: 0,
            controls: Vec::with_capacity(EVENT_CAP),
            held: [false; 128],
            channels: 2,
            silent: false,
            position: 0,
            transport: None,
            ids,
            maps,
            shared,
        };
        let shared = Arc::clone(&p.shared);
        let core = lock(&shared.core);
        let values: Vec<f32> = (0..p.ids.len())
            .map(|i| {
                let (Some(pi), Some(&id)) = (p.info.params.get(i), p.ids.get(i)) else { return 0.0 };
                core.inst.normalized_value(id).and_then(|n| p.plain_via_controller(&core.inst, i, n)).map_or(pi.default, |v| pi.clamp(v))
            })
            .collect();
        drop(core);
        p.values = values;
        p
    }

    /// Normalized → plain through the controller (UI thread only: creation and state loads; the
    /// audio thread never converts this way, it only needs plain → normalized),
    /// so a value read from the plug-in is exact before its curve is sampled.
    fn plain_via_controller(&self, inst: &Instance, i: usize, n: f64) -> Option<f32> {
        let pi = self.info.params.get(i)?;
        let v = match self.maps.get(i)? {
            Mapping::Stepped(steps) => (n * steps).round(),
            Mapping::Plain => inst.to_plain(*self.ids.get(i)?, n).unwrap_or(f64::from(pi.min) + n * f64::from(pi.max - pi.min)),
            Mapping::Linear => f64::from(pi.min) + n * f64::from(pi.max - pi.min),
        };
        v.is_finite().then_some(v as f32)
    }

    /// How many plain-mapped parameters have their curve sampled so far (see [`ParamCurves`]).
    pub fn curves_sampled(&self) -> usize {
        self.curves.curves_sampled()
    }

    /// How many plain-mapped parameters there are to sample.
    pub fn curves_total(&self) -> usize {
        self.curves.curves_total()
    }

    /// Samples every unsampled or invalidated curve now, on the calling thread (no budget).
    ///
    /// For renders that never run an editor handle's `idle` (bounces, freezes, CLI renders): without
    /// it plain-mapped parameters convert linearly until `idle` gets to them. Calls the edit
    /// controller (257 times per plain-mapped parameter), so it must run on the thread that may
    /// use the controller (the UI/main thread, or the thread creating the instance), never on a
    /// thread that is rendering this instance. Not real-time. Hosts that render offline get it for
    /// free on every instance created inside an [`crate::offline_scope`].
    pub fn sample_all_curves_now(&self) {
        let core = lock(&self.shared.core);
        self.curves.step(usize::MAX, Duration::MAX, |id, n| core.inst.to_plain(id, n));
    }

    /// Plain → normalized without calling the controller (safe on the audio thread).
    fn normalized_from_plain(&self, i: usize, v: f32) -> Option<f64> {
        let pi = self.info.params.get(i)?;
        let span = f64::from(pi.max - pi.min);
        let linear = if span > 0.0 { (f64::from(v) - f64::from(pi.min)) / span } else { 0.0 };
        let n = match self.maps.get(i)? {
            Mapping::Stepped(steps) if *steps > 0.0 => f64::from(v) / steps,
            Mapping::Stepped(_) => 0.0,
            Mapping::Plain => self.curves.to_normalized(i, f64::from(v)).unwrap_or(linear),
            Mapping::Linear => linear,
        };
        n.is_finite().then(|| n.clamp(0.0, 1.0))
    }

    fn converter(&self, link: Arc<crate::ffi::EditorLink>) -> Converter {
        Converter { link, info: self.info, ids: self.ids.clone(), maps: self.maps.clone() }
    }

    /// Converts the host-set plain value of parameter `i` and queues it for the next block (and
    /// for the controller before the next state save).
    fn queue(&mut self, i: usize) {
        let Some(&v) = self.values.get(i) else { return };
        let Some(n) = self.normalized_from_plain(i, v) else { return };
        if let Some(u) = self.unsynced.get(i) {
            u.store(f64::from(v).to_bits(), Ordering::Relaxed);
        }
        self.pending.set(i, n);
    }

    /// Queues every value the host set again (after a (re)activation).
    fn resend_host_set(&mut self) {
        for i in 0..self.ids.len() {
            if self.host_set.get(i).copied().unwrap_or(false) {
                self.queue(i);
            }
        }
    }

    /// The editor's size in pixels without opening it (creates and releases a view). For
    /// diagnostics and tests; real plugins expect this on the main thread.
    pub fn editor_size(&self) -> Result<(u32, u32), String> {
        let link = lock(&self.shared.core).inst.editor();
        link.ok_or_else(|| "this plugin has no edit controller".to_string())?.probe()
    }

    /// Re-reads every parameter value from the controller (after a state load) and drops
    /// changes still queued from before it.
    fn refresh_values(&mut self, inst: &Instance) {
        for i in 0..self.ids.len() {
            let (Some(pi), Some(&id)) = (self.info.params.get(i), self.ids.get(i)) else { continue };
            let v = inst.normalized_value(id).and_then(|n| self.plain_via_controller(inst, i, n));
            if let Some(v) = v
                && let Some(slot) = self.values.get_mut(i)
            {
                *slot = pi.clamp(v);
            }
        }
        self.pending.clear();
        for u in self.unsynced.iter() {
            u.store(SYNCED, Ordering::Relaxed);
        }
        self.host_set.fill(false);
    }

    /// Takes a newer MIDI table from the main thread, if one is published (no allocation).
    fn refresh_midi_map(&mut self) {
        if self.shared.map.generation() != self.map_seen
            && let Some(generation) = self.shared.map.read(&mut self.map_scratch)
        {
            std::mem::swap(&mut self.midi_map, &mut self.map_scratch);
            self.map_seen = generation;
        }
    }

    /// Inserts keeping the queue time-sorted; drops the event when the queue is full (never
    /// allocates on the audio thread).
    fn push_note(&mut self, e: NoteEvent) {
        if self.notes.len() >= self.notes.capacity() {
            return;
        }
        let pos = self.notes.iter().rposition(|q| q.time() <= e.time()).map_or(0, |i| i + 1);
        self.notes.insert(pos, e);
    }
}

impl Plugin for Vst3Plugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { f64::from(sample_rate) } else { 48_000.0 };
        let mb = max_block.clamp(1, MAX_BLOCK);
        self.channels = channels.clamp(1, MAX_CHANNELS);
        let shared = Arc::clone(&self.shared);
        let mut core = lock(&shared.core);
        core.sample_rate = sr;
        core.block = mb;
        core.mono = self.channels == 1;
        shared.activate(&mut core);
        drop(core);
        self.activation_seen = shared.activation.load(Ordering::Acquire);
        // Re-send the values the host set, so a re-activated plugin matches the session. Values only
        // read from the plugin are left alone: some plugins report 0 for everything until they load
        // their own preset (Rhapsody), and sending those readings back set its volume and pan to
        // the minimum, silencing it.
        self.pending.clear();
        for i in resend_indices(&self.host_set, self.ids.len()).collect::<Vec<_>>() {
            self.queue(i);
        }
    }

    fn reset(&mut self) {
        keep_releases_on_reset(&mut self.notes, &mut self.held);
        self.controls.clear();
    }

    /// Runs on the audio thread (automation): converts through the sampled curves, never the
    /// controller, and queues without allocating.
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.info.params.iter().position(|p| p.id == id) else { return false };
        let Some(pi) = self.info.params.get(i) else { return false };
        let v = pi.clamp(value);
        if let Some(slot) = self.values.get_mut(i) {
            *slot = v;
        }
        if let Some(h) = self.host_set.get_mut(i) {
            *h = true;
        }
        self.queue(i);
        true
    }

    fn param(&self, id: &str) -> Option<f32> {
        let i = self.info.params.iter().position(|p| p.id == id)?;
        self.values.get(i).copied()
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        // The main thread reactivated it (`kLatencyChanged`, `kIoChanged`): send the values the
        // host set again, as after `prepare`.
        let activation = self.shared.activation.load(Ordering::Acquire);
        if activation != self.activation_seen {
            self.activation_seen = activation;
            self.resend_host_set();
        }
        // The UI thread re-sampled the curves: the host's plain values may now map elsewhere.
        let generation = self.curves.generation();
        if generation != self.curves_seen {
            self.curves_seen = generation;
            self.resend_host_set();
        }
        let shared: &Shared = &self.shared;
        // Held by the main thread only while it services the instance: skip the block.
        let Some(mut guard) = try_lock(&shared.core) else { return };
        let core = &mut *guard;
        if !core.inst.is_active() || core.max_block == 0 {
            return;
        }
        let ch = self.channels.min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        if frames == 0 || ch == 0 {
            return;
        }
        core.inputs.set_silent(self.silent);
        let last = u32::try_from(frames - 1).unwrap_or(u32::MAX);
        for e in &mut self.notes {
            if e.time() > last {
                e.set_time(last);
            }
        }
        for c in &mut self.controls {
            c.0 = c.0.min(last);
        }
        let mut start = 0usize;
        while start < frames {
            let n = (frames - start).min(core.max_block);
            let end = u32::try_from(start + n).unwrap_or(u32::MAX);
            // Feed the main input bus (mono sources fill every bus channel).
            if core.has_input {
                for c in 0..core.inputs.channels(0) {
                    let src = io.get(c.min(ch - 1)).and_then(|s| s.get(start..start + n));
                    if let (Some(src), Some(dst)) = (src, core.inputs.channel_mut(0, c).and_then(|d| d.get_mut(..n))) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            let k = self.notes.iter().position(|e| e.time() >= end).unwrap_or(self.notes.len());
            let offset = u32::try_from(start).unwrap_or(u32::MAX);
            let notes = self.notes.get(..k).unwrap_or(&[]);
            // Leave room in the pool for this block's controller points; the rest waits.
            self.batch.clear();
            self.pending.take_into(&self.ids, &mut self.batch, PARAM_QUEUE_CAP.saturating_sub(self.controls.len()));
            let r = core.inst.process(
                n,
                &mut core.inputs,
                &mut core.outputs,
                &self.batch,
                &self.controls,
                notes,
                offset,
                self.position,
                self.transport.as_ref(),
            );
            self.batch.clear();
            self.notes.drain(..k);
            self.position = self.position.saturating_add(i64::try_from(n).unwrap_or(0));
            match r {
                Ok(()) if core.has_output => {
                    let oc = core.outputs.channels(0);
                    for (c, dst) in io.iter_mut().take(ch).enumerate() {
                        let Some(dst) = dst.get_mut(start..start + n) else { continue };
                        if ch == 1 && oc >= 2 {
                            if let (Some(l), Some(r)) = (core.outputs.channel(0, 0), core.outputs.channel(0, 1)) {
                                for ((d, a), b) in dst.iter_mut().zip(l).zip(r) {
                                    *d = 0.5 * (a + b);
                                }
                            }
                        } else if let Some(src) = core.outputs.channel(0, c.min(oc.saturating_sub(1))).and_then(|s| s.get(..n)) {
                            dst.copy_from_slice(src);
                        }
                    }
                }
                Err(e) => log::debug!("{}: {e}", self.info.id),
                _ => {}
            }
            start += n;
        }
        self.controls.clear();
    }

    fn latency(&self) -> usize {
        self.shared.latency.load(Ordering::Acquire)
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.transport = Some(*transport);
    }

    fn tail_samples(&self) -> usize {
        self.shared.tail.load(Ordering::Relaxed)
    }

    fn process_status(&self) -> ProcessStatus {
        status_from_tail(u32::try_from(self.tail_samples()).unwrap_or(u32::MAX))
    }

    fn set_input_silent(&mut self, silent: bool) {
        self.silent = silent;
    }

    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        if note > 127 || !self.shared.event_input.load(Ordering::Relaxed) {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        self.push_note(NoteEvent::On { time, key: note, velocity: f32::from(velocity.min(127)) / 127.0 });
        if let Some(h) = self.held.get_mut(note as usize) {
            *h = true;
        }
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        if note > 127 || !self.shared.event_input.load(Ordering::Relaxed) {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        self.push_note(NoteEvent::Off { time, key: note });
        if let Some(h) = self.held.get_mut(note as usize) {
            *h = false;
        }
    }

    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        let [_, d1, d2] = msg.map(|b| b & 0x7F);
        let (ctrl, value) = match msg[0] & 0xF0 {
            0xB0 => (usize::from(d1), f64::from(d2) / 127.0),
            0xD0 => (AFTERTOUCH, f64::from(d1) / 127.0),
            0xE0 => (PITCH_BEND, f64::from(u16::from(d2) << 7 | u16::from(d1)) / 16383.0),
            0xA0 if self.shared.event_input.load(Ordering::Relaxed) => {
                self.push_note(NoteEvent::Pressure { time, key: d1, pressure: f32::from(d2) / 127.0 });
                return;
            }
            _ => return,
        };
        // A table re-read on the main thread (`kMidiCCAssignmentChanged`) applies from this
        // controller on.
        self.refresh_midi_map();
        if let Some(&Some(id)) = self.midi_map.get(ctrl)
            && self.controls.len() < self.controls.capacity()
        {
            self.controls.push((time, id, value));
        }
    }

    fn all_notes_off(&mut self) {
        for k in 0..128u8 {
            if self.held.get(k as usize).copied().unwrap_or(false) {
                self.note_off(0, k);
            }
        }
    }

    fn load_state(&mut self, data: &[u8]) -> bool {
        let shared = Arc::clone(&self.shared);
        let core = lock(&shared.core);
        let ok = core.inst.load_state(data);
        if ok {
            self.refresh_values(&core.inst);
        }
        // This thread owns the instance: a plug-in that remapped its controllers while loading
        // (`kMidiCCAssignmentChanged`) gets its table re-read before the next controller, and the
        // values just loaded are the known ones (the host's own change, not edits to report).
        if shared.host.midi_cc_changed.swap(false, Ordering::Acquire) {
            shared.map.publish(&core.inst.midi_mapping());
        }
        shared.host.values_changed.store(false, Ordering::Relaxed);
        shared.remember_values(&core.inst);
        ok
    }

    fn open_editor(&mut self) -> Result<(), String> {
        let link = lock(&self.shared.core).inst.editor();
        link.ok_or_else(|| "this plugin has no editor".to_string())?.open(self.info.name)
    }

    fn close_editor(&mut self) {
        let link = lock(&self.shared.core).inst.editor();
        if let Some(e) = link {
            e.close();
        }
    }

    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        let (link, host) = {
            let core = lock(&self.shared.core);
            (core.inst.editor()?, core.inst.host_ref())
        };
        Some(Box::new(Vst3Editor { conv: self.converter(link), host, curves: Arc::clone(&self.curves), shared: Arc::downgrade(&self.shared) }))
    }

    fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
        let link = lock(&self.shared.core).inst.state_link();
        let conv = self.converter(link);
        let unsynced = Arc::clone(&self.unsynced);
        Some(Box::new(move || {
            // A parameter only the controller stores (no processor state) would otherwise be
            // saved with the controller's stale value. The host's plain value converts through
            // the controller itself here (this thread may call it), so it is exact even for a
            // curve nobody sampled yet.
            for (i, u) in unsynced.iter().enumerate() {
                let bits = u.swap(SYNCED, Ordering::Relaxed);
                if bits != SYNCED
                    && let (Some(&id), Some(n)) = (conv.ids.get(i), conv.normalized(i, f64::from_bits(bits) as f32))
                {
                    conv.link.sync_param(id, n);
                }
            }
            conv.link.save_state()
        }))
    }
}

/// Plain ↔ normalized through the edit controller: UI thread only (the editor handle and the
/// state reader).
#[derive(Clone)]
struct Converter {
    link: Arc<EditorLink>,
    info: &'static PluginInfo,
    ids: Vec<u32>,
    maps: Vec<Mapping>,
}

impl Converter {
    fn plain(&self, i: usize, n: f64) -> Option<f32> {
        let pi = self.info.params.get(i)?;
        let id = *self.ids.get(i)?;
        let v = match self.maps.get(i)? {
            Mapping::Stepped(steps) => (n * steps).round(),
            Mapping::Plain => self.link.to_plain(id, n)?,
            Mapping::Linear => f64::from(pi.min) + n * f64::from(pi.max - pi.min),
        };
        v.is_finite().then(|| pi.clamp(v as f32))
    }

    fn normalized(&self, i: usize, v: f32) -> Option<f64> {
        let pi = self.info.params.get(i)?;
        let id = *self.ids.get(i)?;
        let span = f64::from(pi.max - pi.min);
        let linear = if span > 0.0 { (f64::from(v) - f64::from(pi.min)) / span } else { 0.0 };
        let n = match self.maps.get(i)? {
            Mapping::Stepped(steps) if *steps > 0.0 => f64::from(v) / steps,
            Mapping::Stepped(_) => 0.0,
            Mapping::Plain => self.link.to_normalized(id, f64::from(v)).unwrap_or(linear),
            Mapping::Linear => linear,
        };
        n.is_finite().then(|| n.clamp(0.0, 1.0))
    }
}

/// A VST3 plugin's editor (`IPlugView` in a host window), driven from the main thread.
struct Vst3Editor {
    conv: Converter,
    host: ComWrapper<HostContext>,
    /// The instance's sampled curves, sampled lazily here (UI thread) and re-sampled on a title
    /// restart.
    curves: Arc<ParamCurves>,
    /// The instance's `restartComponent` requests, serviced on each `idle`.
    shared: Weak<Shared>,
}

impl PluginEditor for Vst3Editor {
    fn open(&mut self) -> Result<(), String> {
        // Edits reported before the editor opened (e.g. during setup) are stale.
        let _ = self.host.take_edits();
        self.conv.link.open(self.conv.info.name)
    }

    fn close(&mut self) {
        self.conv.link.close();
    }

    fn is_open(&self) -> bool {
        self.conv.link.is_open()
    }

    /// Also the UI-thread hook for the lazy curve sampling (at most `CURVE_BUDGET_TABLES` curves or
    /// `CURVE_BUDGET_TIME` per call) and for `restartComponent` (see the module docs): the host
    /// polls every editor handle each UI frame, open or not.
    fn idle(&mut self) -> Vec<(String, f32)> {
        if let Some(s) = self.shared.upgrade() {
            s.service(&mut Serviced::default());
        }
        self.conv.link.idle();
        self.curves.step(CURVE_BUDGET_TABLES, CURVE_BUDGET_TIME, |id, n| self.conv.link.to_plain(id, n));
        let mut out = Vec::new();
        for (vid, n) in self.host.take_edits() {
            let Some(i) = self.conv.ids.iter().position(|&x| x == vid) else { continue };
            if let (Some(p), Some(v)) = (self.conv.info.params.get(i), self.conv.plain(i, n)) {
                out.push((p.id.to_string(), v));
            }
        }
        out
    }

    fn set_param(&mut self, id: &str, value: f32) {
        let Some(i) = self.conv.info.params.iter().position(|p| p.id == id) else { return };
        if let (Some(&vid), Some(n)) = (self.conv.ids.get(i), self.conv.normalized(i, value)) {
            self.conv.link.set_normalized(vid, n);
        }
    }
}

/// Indices of the values to re-send when the plugin is (re)activated: those the host set, at most
/// `cap` of them.
fn resend_indices(host_set: &[bool], cap: usize) -> impl Iterator<Item = usize> + '_ {
    host_set.iter().enumerate().filter(|(_, set)| **set).map(|(i, _)| i).take(cap)
}

/// Reset's share of the note queue: drop everything pending except the releases the plug-in is
/// owed. Each held key gets an off at time 0 and any off already queued survives, so a note that
/// was sounding when the transport stopped is still released on the next block.
fn keep_releases_on_reset(notes: &mut Vec<NoteEvent>, held: &mut [bool; 128]) {
    notes.retain(|e| matches!(e, NoteEvent::Off { .. }));
    for (key, h) in (0u8..).zip(held.iter_mut()) {
        if std::mem::take(h) && notes.len() < notes.capacity() {
            notes.insert(0, NoteEvent::Off { time: 0, key });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_values_the_host_set_are_re_sent() {
        assert_eq!(resend_indices(&[false, false, false], 8).count(), 0, "readings from the plugin stay put");
        assert_eq!(resend_indices(&[false, true, false, true], 8).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(resend_indices(&[true, true, true], 2).collect::<Vec<_>>(), vec![0, 1], "bounded by the queue");
    }

    #[test]
    fn process_status_follows_the_tail() {
        assert_eq!(status_from_tail(u32::MAX), ProcessStatus::Continue);
        assert_eq!(status_from_tail(0), ProcessStatus::Tail);
        assert_eq!(status_from_tail(48_000), ProcessStatus::Tail);
    }

    #[test]
    fn reset_keeps_the_releases_it_owes() {
        let mut notes = Vec::with_capacity(EVENT_CAP);
        notes.push(NoteEvent::On { time: 3, key: 72, velocity: 1.0 });
        notes.push(NoteEvent::Off { time: 5, key: 67 });
        let mut held = [false; 128];
        held[60] = true;
        held[64] = true;
        keep_releases_on_reset(&mut notes, &mut held);
        let mut keys: Vec<u8> = notes.iter().filter_map(|e| if let NoteEvent::Off { key, .. } = e { Some(*key) } else { None }).collect();
        assert_eq!(keys.len(), notes.len(), "reset left a non-release queued");
        keys.sort_unstable();
        assert_eq!(keys, [60, 64, 67], "reset must queue an off for every held or already-released key");
        assert!(held.iter().all(|h| !h), "reset must clear held");
    }
}
