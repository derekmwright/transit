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
//!   call; the values that differ from the host's own view (the last value it set or was told)
//!   are reported like editor edits (the path a GUI edit takes into the session). A state load re-reads the values itself and
//!   reports nothing (that is the host's own change).
//! - `kParamTitlesChanged`: the conversion curves are invalidated (the editor handle re-samples
//!   them). Titles, ranges and step counts in `PluginInfo` stay as read at load (`'static`).
//! - `kReloadComponent` ("the host has to unload completely the component ... and reload it"):
//!   not supported here (the instance would have to be created again by its owner); logged as a
//!   warning and counted in [`Serviced::unsupported`]. Other flags need nothing from this host.
//!
//! A block the audio side cannot run because the main thread holds the instance is silence with
//! short fades, and the slot keeps its latency ([`Gate`]). A reactivation the plug-in refuses is
//! logged and counted once ([`Serviced::failed`]), not retried until it asks again.
//!
//! The audio side only `try_lock`s the instance and the main thread only `try_lock`s it while it
//! may be processing, so neither ever waits for the other; a busy instance is serviced by a later
//! call. Servicing happens only on the thread that created the instance.

use crate::ffi::{
    AFTERTOUCH, BusBuffers, EVENT_CAP, EditorLink, HostContext, Instance, MIDI_CHANNELS, NoteEvent, PARAM_POINTS, PARAM_QUEUE_CAP, PITCH_BEND,
    ParamPoint, RELEASE_RESERVE,
};
use crate::gate::{Gate, MAX_CHANNELS};
use crate::params::{ParamCurves, PendingParams};
use soundcraft_dsp::event::{Message, PluginEvent};
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, ProcessStatus, Transport};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError, Weak};
use std::thread::ThreadId;
use std::time::Duration;
use vst3::ComWrapper;

/// Largest block we set a plugin up for.
const MAX_BLOCK: usize = 1 << 16;
/// An `unsynced` slot with no value waiting (a NaN pattern, which `set_param` never stores).
const SYNCED: u64 = u64::MAX;
/// Most curves one editor `idle` samples, and the time after which it stops (it always samples
/// one when any is due).
const CURVE_BUDGET_TABLES: usize = 64;
const CURVE_BUDGET_TIME: Duration = Duration::from_millis(2);
/// Entries of the MIDI controller table per channel: CCs 0–127, aftertouch and pitch bend.
const MAP_LEN: usize = PITCH_BEND + 1;
/// Entries of the whole table: 16 channels.
const MAP_ENTRIES: usize = MIDI_CHANNELS * MAP_LEN;
/// Timed parameter points (`set_param_at`) one block holds; more fall back to the block start.
const TIMED_CAP: usize = 1024;
/// VST3's `kNoParamId`: no parameter.
const NO_PARAM: u32 = u32::MAX;
/// Host values one service call hands to the controller.
const SYNC_CHUNK: usize = 64;
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

use crate::Serviced;

/// The MIDI controller → parameter table of all 16 channels (channel-major, see
/// `Instance::midi_mapping`), written on the main thread and read on the audio thread as one
/// unit: a sequence lock over atomics (odd sequence: a write is in progress).
struct MidiMap {
    seq: AtomicU64,
    ids: Box<[AtomicU32]>,
}

impl MidiMap {
    fn new() -> MidiMap {
        MidiMap { seq: AtomicU64::new(0), ids: (0..MAP_ENTRIES).map(|_| AtomicU32::new(NO_PARAM)).collect() }
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
    fn read(&self, out: &mut [Option<u32>]) -> Option<u64> {
        self.read_with(out, || {})
    }

    /// `read`, running `between` after the entries are copied and before the sequence is checked
    /// again (tests put a publication there).
    fn read_with(&self, out: &mut [Option<u32>], between: impl FnOnce()) -> Option<u64> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return None;
        }
        for (o, slot) in out.iter_mut().zip(self.ids.iter()) {
            let v = slot.load(Ordering::Relaxed);
            *o = (v != NO_PARAM).then_some(v);
        }
        between();
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
    /// The activated sidechain input bus (index into `inputs`), when the plug-in has one.
    key_bus: Option<usize>,
    /// The key bus holds key samples (not zeros) from the last block.
    key_live: bool,
    /// The block size it is set up for, and 0 while it is not (failed or never prepared).
    max_block: usize,
    /// What `prepare` asked for, to set it up again the same way.
    block: usize,
    sample_rate: f64,
    mono: bool,
}

/// A `kParamValuesChanged` re-read in progress (main thread only): the next parameter.
struct Values {
    next: usize,
    active: bool,
}

/// A normalized value the host does not know yet (beyond conversion rounding).
fn differs(host: f64, plugin: f64) -> bool {
    let d = (host - plugin).abs();
    d.is_nan() || d > 1e-6
}

/// The part of an instance both its owner (the audio thread, once in the engine) and the main
/// thread reach (see the module docs).
pub(crate) struct Shared {
    core: Mutex<Core>,
    latency: AtomicUsize,
    tail: AtomicUsize,
    event_input: AtomicBool,
    /// The plug-in takes a sidechain key (declared at creation, confirmed by each activation).
    key_input: AtomicBool,
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
    /// The host's view of each parameter (normalized, `f64` bits; same order as `ids`): what it
    /// last sent or was told. A values re-read reports what differs from it.
    view: Box<[AtomicU64]>,
    /// Parameters whose host value the controller has not been given yet (review N1): set by
    /// `set_param` (any thread, often the audio thread), cleared on the main thread when the
    /// service hands the value over with `setParamNormalized`, as VST3 has hosts inform the
    /// controller. A values re-read runs only once none is left, and never reports over one.
    /// Shared with the state reader, which hands values over too.
    to_controller: Arc<[AtomicBool]>,
    /// The editor/state link, for `setParamNormalized` on the main thread (under its own lock,
    /// never the instance's, so the audio side is not held up).
    link: Arc<EditorLink>,
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
    /// audio side reads: latency, tail, event input, the MIDI table and a new activation. False
    /// when the plug-in refused (logged; it stays inactive until asked again).
    fn activate(&self, core: &mut Core) -> bool {
        // Cleared first: a request made while it reactivates asks again, and a refused one is
        // not retried by every later service call (review R5).
        self.host.latency_changed.store(false, Ordering::Relaxed);
        self.host.io_changed.store(false, Ordering::Relaxed);
        self.host.midi_cc_changed.store(false, Ordering::Relaxed);
        core.max_block = 0;
        match core.inst.configure(core.sample_rate, core.block, core.mono) {
            Ok(layout) => {
                core.inputs = BusBuffers::new(&layout.inputs, core.block);
                core.outputs = BusBuffers::new(&layout.outputs, core.block);
                core.has_input = layout.inputs.first().is_some_and(|&c| c > 0);
                core.has_output = layout.outputs.first().is_some_and(|&c| c > 0);
                core.key_bus = layout.key_bus;
                core.key_live = false;
                self.key_input.store(layout.key_bus.is_some(), Ordering::Relaxed);
                core.max_block = core.block;
                self.event_input.store(layout.event_input, Ordering::Relaxed);
                // VST3: getLatencySamples returns the new latency after setActive(true).
                self.latency.store(core.inst.latency() as usize, Ordering::Release);
                self.tail.store(core.inst.tail() as usize, Ordering::Relaxed);
                self.map.publish(&core.inst.midi_mapping());
                self.activation.fetch_add(1, Ordering::Release);
                true
            }
            Err(e) => {
                log::warn!("{}: {e}", self.info.id);
                self.latency.store(0, Ordering::Release);
                self.activation.fetch_add(1, Ordering::Release);
                false
            }
        }
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
        // The controller learns the host's values first, so a re-read cannot mistake what it has
        // not seen yet for a change of the plug-in's (review N1).
        let synced = self.sync_controller();
        if synced && self.rescan_values() {
            out.value_rescans += 1;
        }
        if h.latency_changed.load(Ordering::Acquire) || h.io_changed.load(Ordering::Acquire) {
            match try_lock(&self.core).map(|mut core| self.activate(&mut core)) {
                Some(true) => out.restarted += 1,
                Some(false) => out.failed += 1,
                None => out.restarts_waiting += 1,
            }
        }
        let rescanning = lock(&self.values).active || !synced;
        if h.midi_cc_changed.load(Ordering::Relaxed)
            || h.values_changed.load(Ordering::Relaxed)
            || h.latency_changed.load(Ordering::Relaxed)
            || h.io_changed.load(Ordering::Relaxed)
            || rescanning
        {
            out.pending += 1;
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
        // Only what differs from the host's view is news to it (review R3).
        let mut changed = Vec::new();
        for (i, v) in read {
            if self.to_controller.get(i).is_some_and(|f| f.load(Ordering::Acquire)) {
                continue; // the host's newer value is on its way to the controller: it stands
            }
            if let (Some(view), Some(&id)) = (self.view.get(i), self.ids.get(i))
                && differs(f64::from_bits(view.load(Ordering::Relaxed)), v)
            {
                view.store(v.to_bits(), Ordering::Relaxed);
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

    /// Hands up to `SYNC_CHUNK` host values the controller has not seen to it
    /// (`setParamNormalized`, main thread). True when none is left.
    fn sync_controller(&self) -> bool {
        let mut sent = 0;
        for (i, flag) in self.to_controller.iter().enumerate() {
            if !flag.load(Ordering::Acquire) {
                continue;
            }
            if sent == SYNC_CHUNK {
                return false;
            }
            flag.store(false, Ordering::Relaxed);
            if let (Some(&id), Some(view)) = (self.ids.get(i), self.view.get(i)) {
                self.link.sync_param(id, f64::from_bits(view.load(Ordering::Acquire)));
                sent += 1;
            }
        }
        true
    }

    /// The controller's values become the host's view (after a load the host made itself).
    fn remember_values(&self, inst: &Instance) {
        let mut st = lock(&self.values);
        for (view, &id) in self.view.iter().zip(&self.ids) {
            if let Some(v) = inst.normalized_value(id) {
                view.store(v.to_bits(), Ordering::Relaxed);
            }
        }
        st.active = false;
    }
}

/// A hosted VST3 plugin. Parameter ids are VST3 `ParamID`s as decimal strings ("0", "17", …);
/// values are plain (display) values with their names and ranges in `info().params`.
pub struct Vst3Plugin {
    shared: Arc<Shared>,
    info: &'static PluginInfo,
    /// Built at creation; automation lookup never scans or allocates.
    param_index: HashMap<&'static str, usize>,
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
    /// Pending notes, kept sorted by time. Never grows past `EVENT_CAP`; the last
    /// `RELEASE_RESERVE` entries only take releases.
    notes: Vec<NoteEvent>,
    /// The parameter each MIDI controller drives on each channel (a copy of [`Shared::map`], see
    /// `Instance::midi_mapping`), and a second table to copy into.
    midi_map: Box<[Option<u32>]>,
    map_scratch: Box<[Option<u32>]>,
    /// The [`MidiMap`] generation `midi_map` holds.
    map_seen: u64,
    /// The [`Shared::activation`] whose values were last re-sent.
    activation_seen: u64,
    /// Controller points for the next block (class 1). Bounded; the last `RELEASE_RESERVE`
    /// entries only take pedal-offs and resets.
    controls: Vec<ParamPoint>,
    /// Parameter points at an offset ([`Plugin::set_param_at`]) for the next block (class 0).
    timed: Vec<ParamPoint>,
    /// One `process` call's points, merged (capacity [`PARAM_POINTS`]).
    points: Vec<ParamPoint>,
    /// The order points were set in this block (ties at one offset keep it).
    seq: u32,
    /// Parameter points dropped because a block's slab ran out (the last per parameter is kept).
    points_dropped: u64,
    /// Keys held per MIDI channel, for `all_notes_off` and `reset`.
    held: [[bool; 128]; MIDI_CHANNELS],
    channels: usize,
    /// Every audio input of the next block is silent (VST3 silence flags).
    silent: bool,
    /// Silence with fades for blocks the plug-in is not called for.
    gate: Gate,
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
        let view: Box<[AtomicU64]> = ids.iter().map(|&id| AtomicU64::new(inst.normalized_value(id).unwrap_or(f64::NAN).to_bits())).collect();
        let host = inst.host_ref();
        let link = inst.state_link();
        let key_input = inst.declares_key_input();
        let shared = Arc::new(Shared {
            core: Mutex::new(Core {
                inst,
                inputs: BusBuffers::new(&[], 0),
                outputs: BusBuffers::new(&[], 0),
                has_input: false,
                has_output: false,
                key_bus: None,
                key_live: false,
                max_block: 0,
                block: 0,
                sample_rate: 48_000.0,
                mono: false,
            }),
            latency: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            event_input: AtomicBool::new(false),
            key_input: AtomicBool::new(key_input),
            activation: AtomicU64::new(0),
            map: MidiMap::new(),
            main: std::thread::current().id(),
            host,
            curves: Arc::clone(&curves),
            info,
            ids: ids.clone(),
            values: Mutex::new(Values { next: 0, active: false }),
            view,
            to_controller: ids.iter().map(|_| AtomicBool::new(false)).collect::<Vec<_>>().into(),
            link,
        });
        lock(live()).push(Arc::downgrade(&shared));
        let mut p = Vst3Plugin {
            info,
            param_index: info.params.iter().enumerate().rev().map(|(i, p)| (p.id, i)).collect(),
            values: Vec::with_capacity(ids.len()),
            host_set: vec![false; ids.len()],
            curves_seen: curves.generation(),
            curves,
            pending: PendingParams::new(ids.len()),
            batch: Vec::with_capacity(PARAM_QUEUE_CAP),
            unsynced: ids.iter().map(|_| AtomicU64::new(SYNCED)).collect(),
            notes: Vec::with_capacity(EVENT_CAP),
            midi_map: vec![None; MAP_ENTRIES].into_boxed_slice(),
            map_scratch: vec![None; MAP_ENTRIES].into_boxed_slice(),
            map_seen: u64::MAX,
            activation_seen: 0,
            controls: Vec::with_capacity(EVENT_CAP),
            timed: Vec::with_capacity(TIMED_CAP),
            points: Vec::with_capacity(PARAM_POINTS),
            seq: 0,
            points_dropped: 0,
            held: [[false; 128]; MIDI_CHANNELS],
            channels: 2,
            silent: false,
            gate: Gate::new(),
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
        Converter { link, info: self.info, param_index: self.param_index.clone(), ids: self.ids.clone(), maps: self.maps.clone() }
    }

    /// Converts the host-set plain value of parameter `i` and queues it for the next block (and
    /// for the controller before the next state save).
    fn queue(&mut self, i: usize) {
        if let Some(n) = self.host_value(i) {
            self.pending.set(i, n);
        }
    }

    /// The host-set plain value of parameter `i`, normalized, noted for the controller (and the
    /// next state save).
    fn host_value(&mut self, i: usize) -> Option<f64> {
        let &v = self.values.get(i)?;
        let n = self.normalized_from_plain(i, v)?;
        // A value re-sent unchanged (after a reactivation or a curve re-sample) is nothing new
        // for the controller.
        if let Some(view) = self.shared.view.get(i)
            && view.swap(n.to_bits(), Ordering::AcqRel) != n.to_bits()
            && let Some(f) = self.shared.to_controller.get(i)
        {
            f.store(true, Ordering::Release);
        }
        if let Some(u) = self.unsynced.get(i) {
            u.store(f64::from(v).to_bits(), Ordering::Relaxed);
        }
        Some(n)
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

    /// Inserts keeping the queue time-sorted (after events at the same time). Refuses it when the
    /// queue is full, or for a non-release when only the release reserve is left. Never allocates
    /// on the audio thread.
    fn push_note(&mut self, e: NoteEvent, release: bool) -> bool {
        let room = if release { self.notes.capacity() } else { self.notes.capacity().saturating_sub(RELEASE_RESERVE) };
        if self.notes.len() >= room {
            return false;
        }
        let pos = self.notes.iter().rposition(|q| q.time() <= e.time()).map_or(0, |i| i + 1);
        self.notes.insert(pos, e);
        true
    }

    /// A MIDI controller (`ctrl`: a CC, [`AFTERTOUCH`] or [`PITCH_BEND`]) on `channel`: a point on
    /// the parameter the plug-in maps it to. An unmapped controller has nothing to drive (true);
    /// false when the controller queue refused it (non-releases stop at the release reserve).
    /// `message` decides the class: a release may take the reserve, and a point that must reach
    /// the plug-in ([`Message::survives_reset`]) is kept through a reset.
    fn control(&mut self, time: u32, channel: u8, ctrl: usize, value: f64, message: &Message) -> bool {
        let release = message.is_release();
        // A table re-read on the main thread (`kMidiCCAssignmentChanged`) applies from this
        // controller on.
        self.refresh_midi_map();
        let Some(&Some(id)) = self.midi_map.get(usize::from(channel & 0x0F) * MAP_LEN + ctrl) else { return true };
        let room = if release { self.controls.capacity() } else { self.controls.capacity().saturating_sub(RELEASE_RESERVE) };
        if self.controls.len() >= room {
            return false;
        }
        self.seq = self.seq.wrapping_add(1);
        self.controls.push(ParamPoint { time, id, class: 1, seq: self.seq, value, release: message.survives_reset() });
        true
    }

    /// Parameter points dropped so far because a block's point slab ran out.
    pub fn param_points_dropped(&self) -> u64 {
        self.points_dropped
    }
}

impl Vst3Plugin {
    /// `process`, with the sidechain key when there is one.
    fn process_keyed(&mut self, io: &mut [Vec<f32>], key: Option<&[Vec<f32>]>, frames: usize) {
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
        let ch = self.channels.min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        if frames == 0 || ch == 0 {
            return;
        }
        let shared: &Shared = &self.shared;
        // Held by the main thread only while it services the instance: a silent block.
        let Some(mut guard) = try_lock(&shared.core) else {
            self.gate.skip(io, ch, frames);
            return;
        };
        let core = &mut *guard;
        if !core.inst.is_active() || core.max_block == 0 {
            if core.block > 0 {
                // Prepared, but its (re)activation failed: silent, not the buffer as it was
                // (review N2).
                drop(guard);
                self.gate.skip(io, ch, frames);
            }
            return;
        }
        core.inputs.set_silent(self.silent);
        let last = u32::try_from(frames - 1).unwrap_or(u32::MAX);
        for e in &mut self.notes {
            if e.time() > last {
                e.set_time(last);
            }
        }
        for c in self.controls.iter_mut().chain(self.timed.iter_mut()) {
            c.time = c.time.min(last);
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
            if let Some(kb) = core.key_bus {
                match key {
                    Some(key) => {
                        if let Some(dst) = core.inputs.bus_mut(kb) {
                            soundcraft_dsp::map_key_channels(key, dst, start, n);
                        }
                        core.inputs.set_bus_silent(kb, false);
                        core.key_live = true;
                    }
                    None => {
                        if core.key_live {
                            if let Some(dst) = core.inputs.bus_mut(kb) {
                                dst.iter_mut().for_each(|c| c.fill(0.0));
                            }
                            core.key_live = false;
                        }
                        core.inputs.set_bus_silent(kb, true);
                    }
                }
            }
            let k = self.notes.iter().position(|e| e.time() >= end).unwrap_or(self.notes.len());
            let offset = u32::try_from(start).unwrap_or(u32::MAX);
            let notes = self.notes.get(..k).unwrap_or(&[]);
            // This call's points: the host values at its start (leaving queues for the timed and
            // controller points; the rest waits), then the timed points and the controllers in it.
            let in_call = |p: &&ParamPoint| p.time >= offset && p.time < end;
            let timed = self.timed.iter().filter(in_call).count() + self.controls.iter().filter(in_call).count();
            self.batch.clear();
            self.pending.take_into(&self.ids, &mut self.batch, PARAM_QUEUE_CAP.saturating_sub(timed));
            self.points.clear();
            for (seq, &(id, value)) in (0u32..).zip(&self.batch) {
                self.points.push(ParamPoint { time: offset, id, class: 0, seq, value, release: false });
            }
            self.batch.clear();
            let room = self.points.capacity();
            for p in self.timed.iter().chain(self.controls.iter()).filter(in_call) {
                if self.points.len() < room {
                    self.points.push(*p);
                }
            }
            let r =
                core.inst.process(n, &mut core.inputs, &mut core.outputs, &mut self.points, notes, offset, self.position, self.transport.as_ref());
            self.points.clear();
            self.notes.drain(..k);
            self.position = self.position.saturating_add(i64::try_from(n).unwrap_or(0));
            if let Ok(dropped) = r {
                self.points_dropped = self.points_dropped.saturating_add(u64::from(dropped));
            }
            match r {
                Ok(_) if core.has_output => {
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
                _ => {}
            }
            start += n;
        }
        drop(guard);
        self.controls.clear();
        self.timed.clear();
        self.seq = 0;
        self.gate.processed(io, ch, frames);
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
        self.gate.set_rate(sr);
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
        // A pedal-up or reset controller is a release too: the mix has already freed the pedal,
        // so dropping its point here would leave sustain on (review F10-1).
        self.controls.retain(|c| c.release);
        self.timed.clear();
    }

    /// Runs on the audio thread (automation): converts through the sampled curves, never the
    /// controller, and queues without allocating.
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.param_index.get(id).copied() else { return false };
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

    /// Like `set_param`, but the point reaches the plug-in at `offset` within the next block, in
    /// a queue of its own with every other point of that parameter.
    fn set_param_at(&mut self, offset: u32, id: &str, value: f32) -> bool {
        let Some(i) = self.param_index.get(id).copied() else { return false };
        let (Some(pi), Some(&pid)) = (self.info.params.get(i), self.ids.get(i)) else { return false };
        let v = pi.clamp(value);
        if let Some(slot) = self.values.get_mut(i) {
            *slot = v;
        }
        if let Some(h) = self.host_set.get_mut(i) {
            *h = true;
        }
        let Some(n) = self.host_value(i) else { return true };
        if self.timed.len() < self.timed.capacity() {
            self.seq = self.seq.wrapping_add(1);
            self.timed.push(ParamPoint { time: offset, id: pid, class: 0, seq: self.seq, value: n, release: false });
        } else {
            // Out of timed points this block: the value still arrives, at the block start.
            self.pending.set(i, n);
        }
        true
    }

    fn param(&self, id: &str) -> Option<f32> {
        let i = self.param_index.get(id).copied()?;
        self.values.get(i).copied()
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        self.process_keyed(io, None, frames);
    }

    fn process_with_key(&mut self, io: &mut [Vec<f32>], key: &[Vec<f32>], frames: usize) {
        self.process_keyed(io, Some(key), frames);
    }

    fn has_key_input(&self) -> bool {
        self.shared.key_input.load(Ordering::Relaxed)
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
        let message = Message::NoteOn { key: note, velocity, id: soundcraft_dsp::event::NoteId::NONE };
        self.event(u32::try_from(offset).unwrap_or(u32::MAX), &PluginEvent { channel: 0, note_id: PluginEvent::NO_ID, message });
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        let message = Message::NoteOff { key: note, velocity: 0, id: soundcraft_dsp::event::NoteId::NONE };
        self.event(u32::try_from(offset).unwrap_or(u32::MAX), &PluginEvent { channel: 0, note_id: PluginEvent::NO_ID, message });
    }

    /// Any MIDI 1.0 channel message, on the channel in its status byte.
    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        let [status, d1, d2] = msg;
        let (d1, d2) = (d1 & 0x7F, d2 & 0x7F);
        let none = soundcraft_dsp::event::NoteId::NONE;
        let message = match status & 0xF0 {
            0xA0 => Message::PolyPressure { key: d1, value: d2, id: none },
            0xB0 => Message::Control { cc: d1, value: d2 },
            0xD0 => Message::ChannelPressure { value: d1 },
            0xE0 => Message::PitchBend { value: u16::from(d2) << 7 | u16::from(d1) },
            _ => return,
        };
        self.event(u32::try_from(offset).unwrap_or(u32::MAX), &PluginEvent { channel: status & 0x0F, note_id: PluginEvent::NO_ID, message });
    }

    /// Notes and poly pressure go to the event bus with their channel and voice id (`noteId`);
    /// controllers, channel pressure and pitch bend become points on the parameters the plug-in
    /// maps them to on that channel (`IMidiMapping`). Releases may use the reserved entries.
    fn event(&mut self, offset: u32, ev: &PluginEvent) -> bool {
        let ch = ev.channel & 0x0F;
        let release = ev.is_release();
        let notes_in = self.shared.event_input.load(Ordering::Relaxed);
        let unit = |v: u8| f32::from(v.min(127)) / 127.0;
        match ev.message {
            Message::NoteOn { key, velocity, .. } if key <= 127 => {
                if !notes_in {
                    return true;
                }
                let ok = self.push_note(NoteEvent::On { time: offset, key, velocity: unit(velocity), channel: ch, id: ev.note_id }, release);
                if ok && let Some(h) = self.held.get_mut(usize::from(ch)).and_then(|h| h.get_mut(usize::from(key))) {
                    *h = true;
                }
                ok
            }
            Message::NoteOff { key, velocity, .. } if key <= 127 => {
                if !notes_in {
                    return true;
                }
                if let Some(h) = self.held.get_mut(usize::from(ch)).and_then(|h| h.get_mut(usize::from(key))) {
                    *h = false;
                }
                self.push_note(NoteEvent::Off { time: offset, key, velocity: unit(velocity), channel: ch, id: ev.note_id }, release)
            }
            Message::PolyPressure { key, value, .. } if key <= 127 => {
                !notes_in || self.push_note(NoteEvent::Pressure { time: offset, key, pressure: unit(value), channel: ch, id: ev.note_id }, release)
            }
            Message::Control { cc, value } if cc <= 127 => self.control(offset, ch, usize::from(cc), f64::from(value.min(127)) / 127.0, &ev.message),
            Message::Control14 { msb, value } if msb < 32 => {
                self.control(offset, ch, usize::from(msb), f64::from(value.min(16383)) / 16383.0, &ev.message)
            }
            Message::ChannelPressure { value } => self.control(offset, ch, AFTERTOUCH, f64::from(value.min(127)) / 127.0, &ev.message),
            Message::PitchBend { value } => self.control(offset, ch, PITCH_BEND, f64::from(value.min(16383)) / 16383.0, &ev.message),
            _ => true,
        }
    }

    fn all_notes_off(&mut self) {
        for ch in 0..MIDI_CHANNELS as u8 {
            for key in 0..128u8 {
                if self.held.get(usize::from(ch)).and_then(|h| h.get(usize::from(key))).copied().unwrap_or(false) {
                    let message = Message::NoteOff { key, velocity: 0, id: soundcraft_dsp::event::NoteId::NONE };
                    self.event(0, &PluginEvent { channel: ch, note_id: PluginEvent::NO_ID, message });
                }
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
        let to_controller = Arc::clone(&self.shared.to_controller);
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
                    // Handed over: the service need not send it again (review N1).
                    if let Some(f) = to_controller.get(i) {
                        f.store(false, Ordering::Release);
                    }
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
    param_index: HashMap<&'static str, usize>,
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
        let Some(i) = self.conv.param_index.get(id).copied() else { return };
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
/// owed. Each held key gets an off at time 0 on its channel and any off already queued survives,
/// so a note that was sounding when the transport stopped is still released on the next block.
fn keep_releases_on_reset(notes: &mut Vec<NoteEvent>, held: &mut [[bool; 128]; MIDI_CHANNELS]) {
    notes.retain(|e| match *e {
        NoteEvent::Off { .. } => true,
        NoteEvent::On { velocity, .. } => velocity <= 0.0,
        NoteEvent::Pressure { .. } => false,
    });
    for (channel, keys) in (0u8..).zip(held.iter_mut()) {
        for (key, h) in (0u8..).zip(keys.iter_mut()) {
            if std::mem::take(h) && notes.len() < notes.capacity() {
                notes.insert(0, NoteEvent::Off { time: 0, key, velocity: 0.0, channel, id: -1 });
            }
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
    fn a_table_published_while_it_is_read_is_not_taken() {
        let m = MidiMap::new();
        let a: Vec<Option<u32>> = (0..MAP_ENTRIES as u32).map(Some).collect();
        let b: Vec<Option<u32>> = vec![Some(7); MAP_ENTRIES];
        m.publish(&a);
        let mut out = vec![None; MAP_ENTRIES];
        let g = m.read(&mut out).expect("a published table");
        assert_eq!(out.to_vec(), a);
        // A publication that lands after the entries were copied: the copy may mix both tables,
        // so the sequence check must refuse it (review mutant V1).
        assert_eq!(m.read_with(&mut out, || m.publish(&b)), None);
        assert_eq!(m.read(&mut out), Some(g + 2));
        assert_eq!(out.to_vec(), b);
        // A write in progress (odd sequence) is refused before anything is copied.
        m.seq.fetch_add(1, Ordering::Relaxed);
        assert_eq!(m.read(&mut out), None);
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
        notes.push(NoteEvent::On { time: 3, key: 72, velocity: 1.0, channel: 0, id: -1 });
        notes.push(NoteEvent::Off { time: 5, key: 67, velocity: 0.0, channel: 0, id: -1 });
        notes.push(NoteEvent::Pressure { time: 6, key: 67, pressure: 0.5, channel: 0, id: -1 });
        notes.push(NoteEvent::On { time: 7, key: 50, velocity: 0.0, channel: 2, id: 4 });
        let mut held = [[false; 128]; MIDI_CHANNELS];
        held[0][60] = true;
        held[9][64] = true;
        keep_releases_on_reset(&mut notes, &mut held);
        assert!(notes.contains(&NoteEvent::On { time: 7, key: 50, velocity: 0.0, channel: 2, id: 4 }), "a velocity-0 note-on is a release");
        notes.retain(|e| !matches!(e, NoteEvent::On { velocity: 0.0, .. }));
        let mut keys: Vec<u8> = notes.iter().filter_map(|e| if let NoteEvent::Off { key, .. } = e { Some(*key) } else { None }).collect();
        assert_eq!(keys.len(), notes.len(), "reset left a non-release queued");
        keys.sort_unstable();
        assert_eq!(keys, [60, 64, 67], "reset must queue an off for every held or already-released key");
        assert!(held.iter().flatten().all(|h| !h), "reset must clear held");
        assert!(notes.iter().any(|e| matches!(e, NoteEvent::Off { key: 64, channel: 9, .. })), "on its channel");
    }
}
