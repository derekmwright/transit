//! `ClapPlugin`: a loaded CLAP instance behind the safe `soundcraft_dsp::Plugin` trait.
//!
//! # Requests a plug-in makes of the host (audit F08, ADR 0012 G7)
//!
//! CLAP lets a plug-in ask the host for work from any thread (`clap_host` in `host.h`). Each
//! request is serviced on the thread CLAP requires:
//!
//! - `request_process` ("the host will then activate and start processing the plugin", useful to
//!   wake it from sleep) and `clap_host_params.request_flush` ("the host will then schedule a call
//!   to either `process` or `flush`"): the audio side answers. [`ClapPlugin::process_status`]
//!   reports `Continue` while such a request is pending, so the mixer runs the next block even
//!   with silent input, no editor and a plug-in that said `CLAP_PROCESS_SLEEP`; that block clears
//!   the request. No `flush` call is needed: a plug-in in the engine is active, so a block is the
//!   answer CLAP allows.
//! - `request_restart` ("deactivate and then reactivate the plugin"): `deactivate` and `activate`
//!   are main-thread calls and `stop_processing` an audio-thread call, while the instance is owned
//!   by the audio thread. [`Shared::service`] (main thread) asks the audio side to park the
//!   instance; the next block calls `stop_processing` on the audio thread and stops calling the
//!   plug-in; the service then deactivates, re-reads the audio ports, activates at the same rate
//!   and block size, reads the latency (where `clap_host_latency.changed` is legal: during
//!   `activate`) and releases it. The instance and its state stay as they are; the new latency
//!   reaches delay compensation through [`Plugin::latency`], which the mixer reads every block.
//!   An instance that is not in the processing state (it never processed since its activation) is
//!   restarted at once, under the same lock. If no block parks it within `PARK_DEADLINE` (the
//!   device stopped, the insert is bypassed), the main thread does it all itself while holding
//!   the instance: CLAP `thread-check.h`, "The host may mark any OS thread, including the
//!   main-thread as the audio-thread", provided "only one OS thread is the audio-thread at a time
//!   in a plugin instance", which holding the instance guarantees.
//! - `request_callback`: `on_main_thread`, by the editor's `idle` or, for plug-ins without an
//!   editor, by [`Shared::service`].
//! - `clap_host_params.rescan` (main thread): `CLAP_PARAM_RESCAN_VALUES` re-reads every value on
//!   the main thread and reports those that differ from the host's own view (the last value it
//!   set or was told) as editor edits (the path a GUI edit takes into the session). The parameter list and its info are fixed per plug-in type (`PluginInfo` is
//!   `'static`), so `INFO`, `TEXT` and `ALL` changes are logged as unsupported.
//!
//! While the plug-in is not called for a block it is in the engine for (parked, or the main thread
//! holds it), its slot outputs silence with short fades and keeps its latency ([`Gate`]).
//!
//! Main-thread servicing happens in [`crate::service_requests`] (every instance) and in each
//! editor handle's `idle`, only on the thread that created the instance (CLAP creates plug-ins
//! on the main thread); other threads leave the requests alone.

use crate::ffi::{EVENT_CAP, Event, GuiLink, HostState, Instance, NotePort, PortBuffers, RELEASE_RESERVE};
use crate::gate::{Gate, MAX_CHANNELS};
use clap_sys::ext::note_ports::{CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI};
use clap_sys::ext::params::{CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT, CLAP_PARAM_RESCAN_VALUES};
use clap_sys::process::{CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL, clap_process_status};
use soundcraft_dsp::event::{Message, NoteId, PluginEvent};
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, ProcessStatus, Transport};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError, Weak};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

/// Largest block we activate a plugin for.
const MAX_BLOCK: usize = 1 << 16;
/// How long a restart waits for the audio side to park the instance before the main thread
/// stops processing itself (no audio callbacks are coming).
const PARK_DEADLINE: Duration = Duration::from_millis(250);

/// [`Shared::life`]: the audio side may process.
const RUNNING: u8 = 0;
/// The main thread waits to restart the instance: the next block stops processing and parks.
const PARK: u8 = 1;
/// The audio side stopped processing and does not touch the instance until it is `RUNNING`.
const PARKED: u8 = 2;
/// Parameter values one service call re-reads for `params.rescan`: it holds the instance (the
/// audio side skips a block it would start meanwhile), so the hold stays short.
const RESCAN_CHUNK: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
enum NoteMode {
    None,
    Clap,
    Midi,
}

use crate::Serviced;

/// What the activation decides: the plugin's audio port buffers, sized on the main thread.
struct Core {
    inst: Instance,
    inputs: PortBuffers,
    outputs: PortBuffers,
    in_main: Option<usize>,
    out_main: Option<usize>,
    /// The sidechain input port (a non-main audio input), when the plug-in declares one.
    key_port: Option<usize>,
    /// The key port holds key samples (not zeros) from the last block.
    key_live: bool,
    /// The block size and rate it is activated for (0 before the first `prepare`).
    max_block: usize,
    sample_rate: f64,
}

impl Core {
    /// Reads the audio ports, sizes their buffers and activates (inactive instance, main thread).
    /// Returns the latency read after a successful activation, `None` when it failed.
    fn activate(&mut self, id: &str) -> Option<usize> {
        let ins = self.inst.audio_ports(true);
        let outs = self.inst.audio_ports(false);
        self.in_main = pick_main(&ins);
        self.out_main = pick_main(&outs);
        self.key_port = pick_key(&ins, self.in_main);
        self.key_live = false;
        self.inputs = PortBuffers::new(&ins.iter().map(|p| p.channels).collect::<Vec<_>>(), self.max_block);
        self.outputs = PortBuffers::new(&outs.iter().map(|p| p.channels).collect::<Vec<_>>(), self.max_block);
        match self.inst.activate(self.sample_rate, 1, u32::try_from(self.max_block).unwrap_or(u32::MAX)) {
            Ok(()) => {
                // `clap_host_latency.changed` is only legal during `activate`: reading the latency
                // now covers it.
                self.inst.state().latency_changed.store(false, Ordering::Relaxed);
                Some(self.inst.latency() as usize)
            }
            Err(e) => {
                log::warn!("{id}: {e}");
                None
            }
        }
    }
}

/// The part of an instance both its owner (the audio thread, once in the engine) and the main
/// thread reach. The audio side only ever `try_lock`s `core` and the main thread only `try_lock`s
/// it while the instance may be processing, so neither waits for the other.
pub(crate) struct Shared {
    core: Mutex<Core>,
    /// `RUNNING`, `PARK` or `PARKED`: the restart handshake. Only the main thread moves it
    /// `RUNNING` → `PARK` and `PARKED` → `RUNNING`; only the audio side moves it `PARK` → `PARKED`.
    life: AtomicU8,
    /// The latency read after the last activation (the mixer reads it every block).
    latency: AtomicUsize,
    /// The plug-in has a sidechain input port (read at each activation).
    key_input: AtomicBool,
    /// The thread that created the instance: its main thread.
    main: ThreadId,
    host: Arc<HostState>,
    link: Arc<GuiLink>,
    info: &'static PluginInfo,
    /// CLAP id of each `info.params` entry (same order).
    ids: Vec<u32>,
    /// A `params.rescan` in progress (main thread only).
    rescan: Mutex<Rescan>,
    /// The host's view of each parameter (plain value, `f64` bits; same order as `ids`): what it
    /// last set or was told. A rescan reports the values that differ from it.
    view: Box<[AtomicU64]>,
    /// When the main thread asked the audio side to park (main thread only).
    park_since: Mutex<Option<Instant>>,
    /// Host values not yet processed (review N3): `set_param` numbers each change (`host_gen`,
    /// stored per parameter in `set_at`); a block that hands the plug-in every queued change
    /// records the number it started from (`processed_gen`). A parameter with `set_at` above it
    /// still has the host's value on its way, which a rescan must not report the old value over.
    host_gen: AtomicU64,
    processed_gen: AtomicU64,
    set_at: Box<[AtomicU64]>,
}

/// A `params.rescan` of the values being serviced: the next parameter to re-read.
#[derive(Default)]
struct Rescan {
    next: usize,
    active: bool,
}

fn try_lock<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// For callers that own the plugin (`&mut ClapPlugin`): nobody else can be holding it then.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every instance alive, for [`crate::service_requests`]. Weak: the list never keeps a plugin.
fn live() -> &'static Mutex<Vec<Weak<Shared>>> {
    static LIVE: OnceLock<Mutex<Vec<Weak<Shared>>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Services the requests of every live instance created on this thread (see the module docs).
pub(crate) fn service_requests() -> Serviced {
    let mut out = Serviced::default();
    let all: Vec<Arc<Shared>> = {
        let mut l = lock(live());
        l.retain(|w| w.strong_count() > 0);
        l.iter().filter_map(Weak::upgrade).collect()
    };
    // The list lock is released before calling into plug-ins. An instance whose owner dropped it
    // meanwhile goes here, on the main thread, where CLAP wants `destroy`.
    for s in all {
        s.service(&mut out);
    }
    out
}

impl Shared {
    /// Main-thread service of this instance's requests (see the module docs).
    pub(crate) fn service(&self, out: &mut Serviced) {
        if std::thread::current().id() != self.main {
            out.other_thread += 1;
            return;
        }
        if !self.link.available() && self.host.callback.load(Ordering::Relaxed) {
            // No editor handle runs `idle` for this plug-in: run it here for `on_main_thread`.
            // (Its edit queue has no reader without an editor; this drains it too.)
            let _ = self.link.idle_headless();
            out.callbacks += 1;
        }
        if self.rescan() {
            out.rescans += 1;
        }
        if self.host.restart.load(Ordering::Acquire) {
            match self.restart_when_parked() {
                Restart::Done => out.restarted += 1,
                Restart::DoneWithoutAudio => {
                    out.restarted += 1;
                    out.restarted_without_audio += 1;
                }
                Restart::Waiting => out.restarts_waiting += 1,
                Restart::Failed => out.failed += 1,
            }
        }
        let rescanning = lock(&self.rescan).active;
        if self.host.restart.load(Ordering::Relaxed) || self.host.params_rescan.load(Ordering::Relaxed) || rescanning {
            out.pending += 1;
        }
    }

    /// `params.rescan` (CLAP: a main-thread call). True when a rescan finished in this call. The
    /// values are re-read `RESCAN_CHUNK` at a time, while the instance is free; a busy instance is
    /// retried by the next call.
    fn rescan(&self) -> bool {
        let mut st = lock(&self.rescan);
        if !st.active {
            if !self.host.params_rescan.swap(false, Ordering::Acquire) {
                return false;
            }
            let flags = self.host.params_rescan_flags.swap(0, Ordering::Relaxed);
            if flags & (CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_TEXT | CLAP_PARAM_RESCAN_ALL) != 0 {
                log::info!("{}: the plugin changed its parameter list or info; unsupported, the list read at load stays", self.info.id);
            }
            if flags & (CLAP_PARAM_RESCAN_VALUES | CLAP_PARAM_RESCAN_ALL) == 0 {
                return true;
            }
            *st = Rescan { next: 0, active: true };
        }
        let Some(core) = try_lock(&self.core) else { return false };
        let ids = self.ids.get(st.next..).unwrap_or(&[]);
        let n = ids.len().min(RESCAN_CHUNK);
        let read: Vec<(usize, u32, f64)> =
            ids.iter().take(n).enumerate().filter_map(|(k, &id)| core.inst.param_value(id).map(|v| (st.next + k, id, v))).collect();
        drop(core);
        // Only what differs from the host's view is news to it, and a value the host set that has
        // not reached the plug-in yet stands (review N3).
        let processed = self.processed_gen.load(Ordering::Acquire);
        let mut values = Vec::new();
        for (i, id, v) in read {
            if self.set_at.get(i).is_some_and(|g| g.load(Ordering::Acquire) > processed) {
                continue;
            }
            if let Some(view) = self.view.get(i)
                && differs(f64::from_bits(view.load(Ordering::Relaxed)), v)
            {
                view.store(v.to_bits(), Ordering::Relaxed);
                values.push((id, v));
            }
        }
        // The values go where GUI edits go (the editor handle reports them to the session).
        let mut edits = lock(&self.host.edits);
        for (id, v) in values {
            if let Some(e) = edits.iter_mut().find(|(i, _)| *i == id) {
                e.1 = v;
            } else if edits.len() < edits.capacity() {
                edits.push((id, v));
            }
        }
        st.next += n;
        if st.next < self.ids.len() {
            return false;
        }
        *st = Rescan::default();
        true
    }

    /// `request_restart`: done now, or waiting for the audio side to park the instance.
    fn restart_when_parked(&self) -> Restart {
        match self.life.load(Ordering::Acquire) {
            PARKED => {
                // The audio side stopped processing and no longer touches the instance.
                let Some(mut core) = try_lock(&self.core) else { return Restart::Waiting };
                let ok = self.restart(&mut core);
                drop(core);
                *lock(&self.park_since) = None;
                self.life.store(RUNNING, Ordering::Release);
                if ok { Restart::Done } else { Restart::Failed }
            }
            PARK => {
                let late = lock(&self.park_since).is_none_or(|t| t.elapsed() >= PARK_DEADLINE);
                if !late {
                    return Restart::Waiting;
                }
                // No block came to park it (no callbacks: device stopped, insert bypassed). Holding
                // the instance, this thread is its only audio thread (see the module docs).
                let Some(mut core) = try_lock(&self.core) else { return Restart::Waiting };
                core.inst.stop_processing();
                let ok = self.restart(&mut core);
                drop(core);
                *lock(&self.park_since) = None;
                self.life.store(RUNNING, Ordering::Release);
                log::info!("{}: restarted without an audio callback (none parked it within {PARK_DEADLINE:?})", self.info.id);
                if ok { Restart::DoneWithoutAudio } else { Restart::Failed }
            }
            _ => {
                if let Some(mut core) = try_lock(&self.core)
                    && !core.inst.is_processing()
                {
                    // Not in the processing state: nothing to stop on the audio thread. The lock
                    // keeps the audio side out while it restarts.
                    return if self.restart(&mut core) { Restart::Done } else { Restart::Failed };
                }
                // (The lock, if taken, is released here.)
                *lock(&self.park_since) = Some(Instant::now());
                self.life.store(PARK, Ordering::Release);
                Restart::Waiting
            }
        }
    }

    /// Deactivates and reactivates at the same rate and block size (main thread, not processing).
    /// False when the plug-in refused the activation: it stays inactive, its slot silent (review
    /// N2) with no latency, until the plug-in asks for another restart or is prepared again.
    fn restart(&self, core: &mut Core) -> bool {
        // Cleared first: a request made while it reactivates asks again.
        self.host.restart.store(false, Ordering::Relaxed);
        core.inst.deactivate();
        let activated = core.activate(self.info.id);
        self.key_input.store(core.key_port.is_some(), Ordering::Relaxed);
        let Some(latency) = activated else {
            self.latency.store(0, Ordering::Release);
            log::warn!("{}: the plug-in refused to activate again after a restart; its slot is silent", self.info.id);
            return false;
        };
        self.latency.store(latency, Ordering::Release);
        // The host starts processing again (CLAP: start_processing on the audio thread before
        // the next process); one block lets it, after which it may sleep again.
        self.host.process.store(true, Ordering::Release);
        true
    }
}

/// What a restart request came to in one service call.
enum Restart {
    Done,
    DoneWithoutAudio,
    Waiting,
    /// The reactivation failed.
    Failed,
}

/// A parameter value the host does not know yet (beyond rounding).
fn differs(host: f64, plugin: f64) -> bool {
    let d = (host - plugin).abs();
    d.is_nan() || d > 1e-9 * host.abs().max(plugin.abs()).max(1.0)
}

/// A hosted CLAP plugin. Parameter ids are the CLAP param ids as decimal strings ("0", "17", …);
/// their human names are in `info().params`.
pub struct ClapPlugin {
    shared: Arc<Shared>,
    /// The request flags (also in `shared`), for lock-free reads on the audio thread.
    host: Arc<HostState>,
    info: &'static PluginInfo,
    /// Built at creation; automation lookup never scans or allocates.
    param_index: HashMap<&'static str, usize>,
    /// CLAP id of each `info.params` entry (same order).
    ids: Vec<u32>,
    values: Vec<f32>,
    /// Pending events, kept sorted by time (stable for equal times). Never grows past `EVENT_CAP`;
    /// the last `RELEASE_RESERVE` entries only take releases.
    queue: Vec<Event>,
    notes: NoteMode,
    /// The note port takes raw MIDI (controllers, pitch bend, program changes).
    midi_in: bool,
    /// Keys held per MIDI channel, for `all_notes_off` and `reset`.
    held: [[bool; 128]; 16],
    /// MIDI has no voice ids on the wire. Keep them until sample-ordered delivery.
    midi_voices: [Option<MidiVoice>; EVENT_CAP],
    deferred_midi_offs: u64,
    channels: usize,
    steady: i64,
    transport: Option<Transport>,
    /// Every audio input of the next block is silent (CLAP `constant_mask`).
    silent: bool,
    /// What the plugin returned from its last `process` call (`Continue` before the first).
    status: ProcessStatus,
    /// The tail (`clap.tail`) read after the last `process` call, as [`tail_samples_from`] maps it.
    tail: usize,
    /// A `reset` waiting for the next block the plug-in is called for (audio side).
    reset_pending: bool,
    /// Silence with fades for blocks the plug-in is not called for.
    gate: Gate,
}

#[derive(Clone, Copy)]
struct MidiVoice {
    channel: u8,
    key: u8,
    id: i32,
    sounding: bool,
}

impl MidiVoice {
    fn matches(&self, channel: u8, key: u8, id: i32) -> bool {
        self.channel == channel && self.key == key && self.id == id
    }
}

/// Convert in stable sample order, absorbing an off while another identity holds the key.
/// Entries are reserved at admission, including future ons, and reclaimed after this pass.
/// EVENT_CAP bounds storage; F10 has at most 224 sounding notes + 767 queued non-releases.
fn midi_events(queue: &mut Vec<Event>, voices: &mut [Option<MidiVoice>; EVENT_CAP], held: &mut [[bool; 128]; 16], deferred: &mut u64) {
    queue.retain_mut(|event| {
        match *event {
            Event::NoteOn { time, key, velocity, channel, id } => {
                if let Some(v) = voices.iter_mut().flatten().find(|v| v.matches(channel, key, id)) {
                    v.sounding = true;
                }
                *event = Event::Midi { time, data: [0x90 | channel, key, (velocity * 127.0).round().clamp(1.0, 127.0) as u8] };
            }
            Event::NoteOff { time, key, velocity, channel, id } => {
                if let Some(v) = voices.iter_mut().flatten().find(|v| v.matches(channel, key, id)) {
                    v.sounding = false;
                }
                if voices.iter().flatten().any(|v| v.sounding && v.channel == channel && v.key == key) {
                    *deferred = deferred.saturating_add(1);
                    return false;
                }
                *event = Event::Midi { time, data: [0x80 | channel, key, (velocity * 127.0).round().clamp(0.0, 127.0) as u8] };
            }
            Event::Midi { data: [status, key, value], .. } => {
                // Global/raw releases, including all_notes_off and reset, retire their keys.
                let off = status & 0xF0 == 0x80 || (status & 0xF0 == 0x90 && value == 0);
                let all = status & 0xF0 == 0xB0 && matches!(key, 120 | 123);
                if off || all {
                    for v in voices.iter_mut().flatten().filter(|v| v.channel == status & 0x0F && (all || v.key == key)) {
                        v.sounding = false;
                    }
                }
            }
            _ => {}
        }
        true
    });
    *held = [[false; 128]; 16];
    for entry in voices {
        if let Some(v) = entry {
            if v.sounding {
                if let Some(h) = held.get_mut(usize::from(v.channel)).and_then(|h| h.get_mut(usize::from(v.key))) {
                    *h = true;
                }
            } else {
                *entry = None;
            }
        }
    }
}

impl ClapPlugin {
    fn prepare_midi_events(&mut self) {
        if self.notes == NoteMode::Midi {
            midi_events(&mut self.queue, &mut self.midi_voices, &mut self.held, &mut self.deferred_midi_offs);
        }
    }

    pub(crate) fn new(inst: Instance, info: &'static PluginInfo) -> ClapPlugin {
        let ids: Vec<u32> = info.params.iter().map(|p| p.id.parse::<u32>().unwrap_or(u32::MAX)).collect();
        let values = info.params.iter().zip(&ids).map(|(p, id)| inst.param_value(*id).map_or(p.default, |v| p.clamp(v as f32))).collect();
        let host = inst.host_state();
        let link = inst.link();
        let key_input = {
            let ins = inst.audio_ports(true);
            pick_key(&ins, pick_main(&ins)).is_some()
        };
        let view: Box<[AtomicU64]> =
            info.params.iter().zip(&ids).map(|(p, id)| AtomicU64::new(inst.param_value(*id).unwrap_or(f64::from(p.default)).to_bits())).collect();
        let shared = Arc::new(Shared {
            core: Mutex::new(Core {
                inst,
                inputs: PortBuffers::new(&[], 0),
                outputs: PortBuffers::new(&[], 0),
                in_main: None,
                out_main: None,
                key_port: None,
                key_live: false,
                max_block: 0,
                sample_rate: 48_000.0,
            }),
            life: AtomicU8::new(RUNNING),
            latency: AtomicUsize::new(0),
            key_input: AtomicBool::new(key_input),
            main: std::thread::current().id(),
            host: Arc::clone(&host),
            link,
            info,
            ids: ids.clone(),
            rescan: Mutex::new(Rescan::default()),
            view,
            park_since: Mutex::new(None),
            host_gen: AtomicU64::new(0),
            processed_gen: AtomicU64::new(0),
            set_at: ids.iter().map(|_| AtomicU64::new(0)).collect(),
        });
        lock(live()).push(Arc::downgrade(&shared));
        ClapPlugin {
            shared,
            host,
            info,
            param_index: info.params.iter().enumerate().rev().map(|(i, p)| (p.id, i)).collect(),
            ids,
            values,
            queue: Vec::with_capacity(EVENT_CAP),
            notes: NoteMode::None,
            midi_in: false,
            held: [[false; 128]; 16],
            midi_voices: [None; EVENT_CAP],
            deferred_midi_offs: 0,
            channels: 2,
            steady: 0,
            transport: None,
            silent: false,
            status: ProcessStatus::Continue,
            tail: 0,
            reset_pending: false,
            gate: Gate::new(),
        }
    }

    /// Re-reads every parameter value from the plugin (after a state load).
    fn refresh_values(&mut self) {
        let core = lock(&self.shared.core);
        for (((v, p), id), view) in self.values.iter_mut().zip(self.info.params).zip(&self.ids).zip(self.shared.view.iter()) {
            if let Some(x) = core.inst.param_value(*id) {
                *v = p.clamp(x as f32);
                view.store(f64::from(*v).to_bits(), Ordering::Relaxed);
            }
        }
        drop(core);
        // Pending changes from before the load would undo it; none is on its way any more.
        self.queue.retain(|e| !matches!(e, Event::Param { .. }));
        self.shared.processed_gen.store(self.shared.host_gen.load(Ordering::Acquire), Ordering::Release);
    }

    /// Inserts keeping the queue time-sorted (after events at the same time). Refuses it when the
    /// queue is full, or for a non-release when only the release reserve is left. Never allocates
    /// on the audio thread.
    fn push(&mut self, e: Event, release: bool) -> bool {
        let room = if release { self.queue.capacity() } else { self.queue.capacity().saturating_sub(RELEASE_RESERVE) };
        if self.queue.len() >= room {
            return false;
        }
        let pos = self.queue.iter().rposition(|q| q.time() <= e.time()).map_or(0, |i| i + 1);
        self.queue.insert(pos, e);
        true
    }

    /// An event as MIDI 1.0 bytes on `channel` (two messages for a 14-bit controller).
    fn push_midi(&mut self, time: u32, channel: u8, ev: &PluginEvent, release: bool) -> bool {
        let mut ok = true;
        for data in ev.message.midi1(channel).into_iter().flatten() {
            ok &= self.push(Event::Midi { time, data }, release);
        }
        ok
    }

    /// Queues a parameter value at `time`, replacing one queued for the same parameter and time.
    fn param_at(&mut self, time: u32, cid: u32, value: f64) -> bool {
        if let Some(Event::Param { value: v, .. }) =
            self.queue.iter_mut().find(|e| matches!(e, Event::Param { id, time: t, .. } if *id == cid && *t == time))
        {
            *v = value;
            true
        } else {
            self.push(Event::Param { time, id: cid, value }, false)
        }
    }

    /// The host-set value of parameter `id`, clamped and noted: (CLAP id, value).
    fn host_value(&mut self, id: &str, value: f32) -> Option<(u32, f64)> {
        let i = self.param_index.get(id).copied()?;
        let (Some(pi), Some(&cid)) = (self.info.params.get(i), self.ids.get(i)) else { return None };
        let v = pi.clamp(value);
        if let Some(slot) = self.values.get_mut(i) {
            *slot = v;
        }
        if let Some(view) = self.shared.view.get(i) {
            view.store(f64::from(v).to_bits(), Ordering::Relaxed);
        }
        if let Some(at) = self.shared.set_at.get(i) {
            at.store(self.shared.host_gen.fetch_add(1, Ordering::AcqRel) + 1, Ordering::Release);
        }
        Some((cid, f64::from(v)))
    }
}

/// The sidechain port: the first non-main audio input with channels (the main input is not one).
fn pick_key(ports: &[crate::ffi::PortInfo], main: Option<usize>) -> Option<usize> {
    ports.iter().enumerate().position(|(i, p)| !p.main && p.channels > 0 && Some(i) != main)
}

fn pick_main(ports: &[crate::ffi::PortInfo]) -> Option<usize> {
    ports.iter().position(|p| p.main && p.channels > 0).or_else(|| ports.iter().position(|p| p.channels > 0))
}

/// `clap.tail` values from here up mean an infinite tail.
const INFINITE_TAIL: u32 = i32::MAX as u32;

/// The tail as a finite sample count: `None` without the `clap.tail` extension or when it is
/// infinite.
fn finite_tail(tail: Option<u32>) -> Option<u32> {
    tail.filter(|&t| t < INFINITE_TAIL)
}

/// The trait's tail from `clap.tail`: 0 without the extension; an infinite tail becomes
/// `u32::MAX`, as the VST3 host reports `kInfiniteTail` (the offline render caps it; the mixer
/// never suspends such a plugin, see [`status_from_clap`]).
fn tail_samples_from(tail: Option<u32>) -> usize {
    match tail {
        None => 0,
        Some(t) if t >= INFINITE_TAIL => u32::MAX as usize,
        Some(t) => usize::try_from(t).unwrap_or(usize::MAX),
    }
}

/// Maps a `clap_process_status` and the plugin's `clap.tail` (`None` without the extension) to
/// the trait's status. An error or an unknown value keeps the plugin processing. `TAIL` becomes
/// [`ProcessStatus::Tail`] only with a known finite tail (the mixer counts it down); without one
/// the plugin keeps processing, and a plugin with an infinite tail always does, whatever it
/// returned.
fn status_from_clap(s: clap_process_status, tail: Option<u32>) -> ProcessStatus {
    if tail.is_some_and(|t| t >= INFINITE_TAIL) {
        return ProcessStatus::Continue;
    }
    match s {
        CLAP_PROCESS_CONTINUE_IF_NOT_QUIET => ProcessStatus::ContinueIfNotQuiet,
        CLAP_PROCESS_TAIL if finite_tail(tail).is_some() => ProcessStatus::Tail,
        CLAP_PROCESS_TAIL => ProcessStatus::Continue,
        CLAP_PROCESS_SLEEP => ProcessStatus::Sleep,
        CLAP_PROCESS_CONTINUE => ProcessStatus::Continue,
        _ => ProcessStatus::Continue,
    }
}

fn note_mode(p: Option<NotePort>) -> NoteMode {
    match p {
        Some(p) if p.supported & CLAP_NOTE_DIALECT_CLAP != 0 => NoteMode::Clap,
        Some(p) if p.supported & CLAP_NOTE_DIALECT_MIDI != 0 => NoteMode::Midi,
        _ => NoteMode::None,
    }
}

impl ClapPlugin {
    /// `process`, with the sidechain key when there is one.
    fn process_keyed(&mut self, io: &mut [Vec<f32>], key: Option<&[Vec<f32>]>, frames: usize) {
        let ch = self.channels.min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        if frames == 0 || ch == 0 {
            return;
        }
        let shared: &Shared = &self.shared;
        match shared.life.load(Ordering::Acquire) {
            RUNNING => {}
            PARK => {
                // The main thread waits to restart it: leave the processing state here, on the
                // audio thread (CLAP `stop_processing`), then keep out until it is done.
                if let Some(mut core) = try_lock(&shared.core) {
                    core.inst.stop_processing();
                    drop(core);
                    shared.life.store(PARKED, Ordering::Release);
                }
                self.gate.skip(io, ch, frames);
                return;
            }
            _ => {
                self.gate.skip(io, ch, frames);
                return;
            }
        }
        // Held by the main thread only while it restarts or rescans an instance: skip the block.
        let Some(mut guard) = try_lock(&shared.core) else {
            self.gate.skip(io, ch, frames);
            return;
        };
        let core = &mut *guard;
        if core.max_block == 0 {
            return; // never prepared
        }
        if !core.inst.is_active() {
            // Its activation failed: silent, not the input at unity (review N2).
            drop(guard);
            self.gate.skip(io, ch, frames);
            return;
        }
        // Every change queued so far goes out in this block (parameter events are at time 0).
        let queued_up_to = shared.host_gen.load(Ordering::Acquire);
        if std::mem::take(&mut self.reset_pending) {
            core.inst.reset();
        }
        // This block answers a pending `request_process` or `params.request_flush` (one made
        // during it asks again).
        self.host.process.store(false, Ordering::Relaxed);
        core.inputs.set_constant(self.silent);
        let last = u32::try_from(frames - 1).unwrap_or(u32::MAX);
        for e in &mut self.queue {
            if e.time() > last {
                e.set_time(last);
            }
        }
        if self.notes == NoteMode::Midi {
            midi_events(&mut self.queue, &mut self.midi_voices, &mut self.held, &mut self.deferred_midi_offs);
        }
        let mut start = 0usize;
        while start < frames {
            let n = (frames - start).min(core.max_block);
            let end = u32::try_from(start + n).unwrap_or(u32::MAX);
            // Feed the main input port (mono sources fill every port channel).
            if let Some(p) = core.in_main {
                for c in 0..core.inputs.channels(p) {
                    let src = io.get(c.min(ch - 1)).and_then(|s| s.get(start..start + n));
                    if let (Some(src), Some(dst)) = (src, core.inputs.channel_mut(p, c).and_then(|d| d.get_mut(..n))) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            if let Some(kp) = core.key_port {
                match key {
                    Some(key) => {
                        if let Some(dst) = core.inputs.port_mut(kp) {
                            soundcraft_dsp::map_key_channels(key, dst, start, n);
                        }
                        core.inputs.set_port_constant(kp, false);
                        core.key_live = true;
                    }
                    None => {
                        if core.key_live {
                            if let Some(dst) = core.inputs.port_mut(kp) {
                                dst.iter_mut().for_each(|c| c.fill(0.0));
                            }
                            core.key_live = false;
                        }
                        core.inputs.set_port_constant(kp, true);
                    }
                }
            }
            let k = self.queue.iter().position(|e| e.time() >= end).unwrap_or(self.queue.len());
            let offset = u32::try_from(start).unwrap_or(u32::MAX);
            let events = self.queue.get(..k).unwrap_or(&[]);
            let r = core.inst.process(n, self.steady, &mut core.inputs, &mut core.outputs, events, offset, self.transport.as_ref());
            self.queue.drain(..k);
            let tail = core.inst.tail();
            self.tail = tail_samples_from(tail);
            self.status = r.as_ref().map_or(ProcessStatus::Continue, |&s| status_from_clap(s, tail));
            self.steady = self.steady.saturating_add(i64::try_from(n).unwrap_or(0));
            // Failed blocks retain the silence prepared above; never log from this callback.
            if let (Ok(_), Some(p)) = (r, core.out_main) {
                let oc = core.outputs.channels(p);
                for (c, dst) in io.iter_mut().take(ch).enumerate() {
                    let Some(dst) = dst.get_mut(start..start + n) else { continue };
                    if ch == 1 && oc >= 2 {
                        if let (Some(l), Some(r)) = (core.outputs.channel(p, 0), core.outputs.channel(p, 1)) {
                            for ((d, a), b) in dst.iter_mut().zip(l).zip(r) {
                                *d = 0.5 * (a + b);
                            }
                        }
                    } else if let Some(src) = core.outputs.channel(p, c.min(oc.saturating_sub(1))).and_then(|s| s.get(..n)) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            start += n;
        }
        shared.processed_gen.store(queued_up_to, Ordering::Release);
        drop(guard);
        self.gate.processed(io, ch, frames);
    }
}

impl Plugin for ClapPlugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { f64::from(sample_rate) } else { 48_000.0 };
        let mb = max_block.clamp(1, MAX_BLOCK);
        self.channels = channels.clamp(1, MAX_CHANNELS);
        let shared = Arc::clone(&self.shared);
        let mut core = lock(&shared.core);
        core.inst.deactivate();
        core.inst.service_callback();
        core.sample_rate = sr;
        core.max_block = mb;
        self.gate.set_rate(sr);
        // A (re)activation starts from a clean state anyway.
        self.reset_pending = false;
        let port = core.inst.note_port();
        self.notes = note_mode(port);
        self.midi_in = port.is_some_and(|p| p.supported & CLAP_NOTE_DIALECT_MIDI != 0);
        self.status = ProcessStatus::Continue;
        // A full (re)activation answers any restart request, including one left half done.
        self.host.restart.store(false, Ordering::Relaxed);
        let latency = core.activate(self.info.id).unwrap_or(0);
        shared.key_input.store(core.key_port.is_some(), Ordering::Relaxed);
        drop(core);
        shared.latency.store(latency, Ordering::Release);
        shared.life.store(RUNNING, Ordering::Release);
    }

    fn reset(&mut self) {
        // CLAP `reset` (audio thread, active plugin) goes out right before the next block the
        // plug-in is called for, under the same lock as that block, so a main thread holding the
        // instance meanwhile can never make it miss one (review R6).
        self.reset_pending = true;
        self.prepare_midi_events();
        keep_releases_on_reset(&mut self.queue, &mut self.held, self.notes);
        self.midi_voices.fill(None);
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some((cid, v)) = self.host_value(id, value) else { return false };
        // Coalesce with a pending change of the same parameter at the block start.
        self.param_at(0, cid, v);
        true
    }

    /// One `Event::Param` per offset: points of one parameter coalesce only at an equal time.
    fn set_param_at(&mut self, offset: u32, id: &str, value: f32) -> bool {
        let Some((cid, v)) = self.host_value(id, value) else { return false };
        self.param_at(offset, cid, v)
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

    /// What the plugin said after its last block, except that a pending request keeps it
    /// processed: `request_process` / `params.request_flush` (CLAP: the host schedules a
    /// `process`), and a restart waiting for this side to stop processing.
    fn process_status(&self) -> ProcessStatus {
        if self.host.process.load(Ordering::Relaxed) || self.shared.life.load(Ordering::Relaxed) == PARK {
            ProcessStatus::Continue
        } else {
            self.status
        }
    }

    fn tail_samples(&self) -> usize {
        self.tail
    }

    fn set_input_silent(&mut self, silent: bool) {
        self.silent = silent;
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.transport = Some(*transport);
    }

    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        let message = Message::NoteOn { key: note, velocity, id: NoteId::NONE };
        self.event(u32::try_from(offset).unwrap_or(u32::MAX), &PluginEvent { channel: 0, note_id: PluginEvent::NO_ID, message });
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        let message = Message::NoteOff { key: note, velocity: 0, id: NoteId::NONE };
        self.event(u32::try_from(offset).unwrap_or(u32::MAX), &PluginEvent { channel: 0, note_id: PluginEvent::NO_ID, message });
    }

    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        if self.midi_in {
            self.push(Event::Midi { time: u32::try_from(offset).unwrap_or(u32::MAX), data: msg }, midi_is_release(msg));
        }
    }

    /// CLAP-dialect ports get note events (with channel and `note_id`) and poly pressure as a
    /// pressure note expression; MIDI-dialect ports get the bytes with the channel nibble.
    /// Controllers, bend, channel pressure and programs need a port that takes MIDI. Releases
    /// may use the reserved entries.
    fn event(&mut self, offset: u32, ev: &PluginEvent) -> bool {
        let ch = ev.channel & 0x0F;
        let release = ev.is_release();
        let unit = |v: u8| f64::from(v.min(127)) / 127.0;
        let held = |me: &mut Self, key: u8, on: bool| {
            if let Some(h) = me.held.get_mut(usize::from(ch)).and_then(|h| h.get_mut(usize::from(key))) {
                *h = on;
            }
        };
        match (ev.message, self.notes) {
            (Message::NoteOn { key, .. } | Message::NoteOff { key, .. } | Message::PolyPressure { key, .. }, _) if key > 127 => true,
            (Message::NoteOn { key, velocity: 0, id }, NoteMode::Midi) => {
                self.event(offset, &PluginEvent { message: Message::NoteOff { key, velocity: 0, id }, ..*ev })
            }
            (_, NoteMode::None) if !self.midi_in => true,
            (Message::NoteOn { key, velocity, .. }, NoteMode::Clap) => {
                let ok = self.push(Event::NoteOn { time: offset, key, velocity: unit(velocity), channel: ch, id: ev.note_id }, release);
                if ok {
                    held(self, key, true);
                }
                ok
            }
            (Message::NoteOff { key, velocity, .. }, NoteMode::Clap) => {
                held(self, key, false);
                self.push(Event::NoteOff { time: offset, key, velocity: unit(velocity), channel: ch, id: ev.note_id }, release)
            }
            (Message::PolyPressure { key, value, .. }, NoteMode::Clap) => {
                self.push(Event::Pressure { time: offset, key, value: unit(value), channel: ch, id: ev.note_id }, release)
            }
            (Message::NoteOn { key, velocity, .. }, NoteMode::Midi) => {
                let slot = self
                    .midi_voices
                    .iter()
                    .position(|v| v.is_some_and(|v| v.matches(ch, key, ev.note_id)))
                    .or_else(|| self.midi_voices.iter().position(Option::is_none));
                let Some(slot) = slot else { return false };
                let ok = self.push(Event::NoteOn { time: offset, key, velocity: unit(velocity), channel: ch, id: ev.note_id }, release);
                if ok
                    && let Some(v) = self.midi_voices.get_mut(slot)
                    && v.is_none()
                {
                    *v = Some(MidiVoice { channel: ch, key, id: ev.note_id, sounding: false });
                }
                ok
            }
            (Message::NoteOff { key, velocity, .. }, NoteMode::Midi) => {
                self.push(Event::NoteOff { time: offset, key, velocity: unit(velocity), channel: ch, id: ev.note_id }, release)
            }
            (Message::NoteOn { .. } | Message::NoteOff { .. }, NoteMode::None) => true,
            _ if self.midi_in => self.push_midi(offset, ch, ev, release),
            _ => true,
        }
    }

    fn all_notes_off(&mut self) {
        self.prepare_midi_events();
        if self.notes == NoteMode::Midi {
            // Retire the identities and pending ons together, so a future on cannot follow
            // the global off and leave an untracked key sounding.
            keep_releases_on_reset(&mut self.queue, &mut self.held, self.notes);
            self.midi_voices.fill(None);
            return;
        }
        for ch in 0..16u8 {
            for key in 0..128u8 {
                if self.held.get(usize::from(ch)).and_then(|h| h.get(usize::from(key))).copied().unwrap_or(false) {
                    let message = Message::NoteOff { key, velocity: 0, id: NoteId::NONE };
                    self.event(0, &PluginEvent { channel: ch, note_id: PluginEvent::NO_ID, message });
                }
            }
        }
    }

    fn load_state(&mut self, data: &[u8]) -> bool {
        let ok = lock(&self.shared.core).inst.load_state(data);
        if ok {
            self.refresh_values();
        }
        ok
    }

    fn open_editor(&mut self) -> Result<(), String> {
        let link = lock(&self.shared.core).inst.gui().ok_or_else(|| "this plugin has no editor".to_string())?;
        link.open(self.info.name)
    }

    fn close_editor(&mut self) {
        let link = lock(&self.shared.core).inst.gui();
        if let Some(link) = link {
            link.close();
        }
    }

    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        let link = lock(&self.shared.core).inst.gui()?;
        Some(Box::new(ClapEditor { link, info: self.info, ids: self.ids.clone(), shared: Arc::downgrade(&self.shared) }))
    }

    fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
        let link = lock(&self.shared.core).inst.state_link()?;
        Some(Box::new(move || link.save_state()))
    }
}

/// A CLAP plugin's floating editor, driven from the main thread (see [`GuiLink`]).
struct ClapEditor {
    link: Arc<GuiLink>,
    info: &'static PluginInfo,
    ids: Vec<u32>,
    /// The instance's requests, serviced on each `idle` (the host calls it every UI frame).
    shared: Weak<Shared>,
}

impl PluginEditor for ClapEditor {
    fn open(&mut self) -> Result<(), String> {
        // Edits reported before the editor opened are stale.
        let _ = self.link.idle_headless();
        self.link.open(self.info.name)
    }

    fn close(&mut self) {
        self.link.close();
    }

    fn is_open(&self) -> bool {
        self.link.is_open()
    }

    fn idle(&mut self) -> Vec<(String, f32)> {
        if let Some(s) = self.shared.upgrade() {
            s.service(&mut Serviced::default());
        }
        let raw = self.link.idle();
        let _ = self.link.host().dirty.swap(false, Ordering::Relaxed);
        let mut out: Vec<(String, f32)> = Vec::new();
        for (cid, v) in raw {
            let Some(i) = self.ids.iter().position(|&x| x == cid) else { continue };
            let Some(p) = self.info.params.get(i) else { continue };
            let v = p.clamp(v as f32);
            // Keep only the latest value per parameter.
            match out.iter_mut().find(|(id, _)| id == p.id) {
                Some(e) => e.1 = v,
                None => out.push((p.id.to_string(), v)),
            }
        }
        out
    }
}

/// Reset's share of the event queue: keep parameter changes and every release the plug-in is owed
/// ([`Message::survives_reset`]: note-offs in either dialect, a MIDI note-on with velocity 0,
/// pedals going up, CC 120, 121 and 123), drop the rest. The mix frees a pedal or note once it
/// has handed its release over, so a release dropped here would hang at the plug-in. Each held
/// key also gets an off at time 0 (a CLAP note-off, or a MIDI 0x80 message for MIDI-only ports),
/// so a note that was sounding when the transport stopped is still released on the next block.
fn keep_releases_on_reset(queue: &mut Vec<Event>, held: &mut [[bool; 128]; 16], notes: NoteMode) {
    queue.retain(|e| match *e {
        Event::Param { .. } | Event::NoteOff { .. } => true,
        Event::NoteOn { velocity, .. } => velocity <= 0.0,
        Event::Midi { data, .. } => Message::midi1_survives_reset(data),
        Event::Pressure { .. } => false,
    });
    for (channel, keys) in (0u8..).zip(held.iter_mut()) {
        for (key, h) in (0u8..).zip(keys.iter_mut()) {
            if std::mem::take(h) && queue.len() < queue.capacity() {
                let off = match notes {
                    NoteMode::Midi => Event::Midi { time: 0, data: [0x80 | channel, key, 0] },
                    NoteMode::Clap | NoteMode::None => Event::NoteOff { time: 0, key, velocity: 0.0, channel, id: -1 },
                };
                queue.insert(0, off);
            }
        }
    }
}

/// The release class of a raw MIDI 1.0 message (see `Message::is_release`).
fn midi_is_release(msg: [u8; 3]) -> bool {
    let [status, d1, d2] = msg;
    match status & 0xF0 {
        0x80 => true,
        0x90 => d2 == 0,
        0xB0 => Message::Control { cc: d1 & 0x7F, value: d2 & 0x7F }.is_release(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::process::CLAP_PROCESS_ERROR;

    #[test]
    fn clap_process_status_maps_to_the_trait_status() {
        use ProcessStatus::*;
        let inf = Some(INFINITE_TAIL);
        let rows: &[(clap_process_status, Option<u32>, ProcessStatus)] = &[
            (CLAP_PROCESS_CONTINUE, None, Continue),
            (CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, None, ContinueIfNotQuiet),
            (CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, Some(4800), ContinueIfNotQuiet),
            (CLAP_PROCESS_SLEEP, None, Sleep),
            (CLAP_PROCESS_SLEEP, Some(0), Sleep),
            (CLAP_PROCESS_ERROR, None, Continue),
            (99, None, Continue),
            (-1, Some(10), Continue),
            // TAIL is followed only with a finite tail the mixer can count down.
            (CLAP_PROCESS_TAIL, Some(48_000), Tail),
            (CLAP_PROCESS_TAIL, Some(0), Tail),
            (CLAP_PROCESS_TAIL, Some(INFINITE_TAIL - 1), Tail),
            (CLAP_PROCESS_TAIL, None, Continue),
            (CLAP_PROCESS_TAIL, inf, Continue),
            (CLAP_PROCESS_TAIL, Some(u32::MAX), Continue),
            // An infinite tail keeps the plugin processing whatever it returned.
            (CLAP_PROCESS_SLEEP, inf, Continue),
            (CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, Some(u32::MAX), Continue),
        ];
        for &(s, tail, want) in rows {
            assert_eq!(status_from_clap(s, tail), want, "status {s}, tail {tail:?}");
        }
        assert_eq!(finite_tail(Some(512)), Some(512));
        assert_eq!(finite_tail(inf), None);
        assert_eq!(finite_tail(None), None);
        assert_eq!(tail_samples_from(Some(48_000)), 48_000);
        assert_eq!(tail_samples_from(Some(0)), 0);
        assert_eq!(tail_samples_from(inf), u32::MAX as usize);
        assert_eq!(tail_samples_from(Some(u32::MAX)), u32::MAX as usize);
        assert_eq!(tail_samples_from(None), 0);
    }

    #[test]
    fn reset_keeps_the_releases_it_owes() {
        let mut queue = Vec::with_capacity(EVENT_CAP);
        queue.push(Event::NoteOn { time: 3, key: 72, velocity: 1.0, channel: 0, id: -1 });
        queue.push(Event::NoteOff { time: 5, key: 67, velocity: 0.0, channel: 0, id: -1 });
        let mut held = [[false; 128]; 16];
        held[0][60] = true;
        held[0][64] = true;
        keep_releases_on_reset(&mut queue, &mut held, NoteMode::Clap);
        let mut keys: Vec<u8> = queue
            .iter()
            .map(|e| match e {
                Event::NoteOff { key, .. } => *key,
                other => panic!("reset left a non-release queued: {other:?}"),
            })
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, [60, 64, 67], "reset must queue an off for every held or already-released key");
        assert!(held.iter().flatten().all(|h| !h), "reset must clear held");

        // A MIDI-only port gets 0x80 messages for the held keys instead.
        let mut queue = Vec::with_capacity(EVENT_CAP);
        let mut held = [[false; 128]; 16];
        held[0][60] = true;
        held[0][64] = true;
        keep_releases_on_reset(&mut queue, &mut held, NoteMode::Midi);
        let mut msgs: Vec<[u8; 3]> = queue
            .iter()
            .map(|e| match e {
                Event::Midi { time: 0, data } => *data,
                other => panic!("MIDI port must get 0x80 messages, got {other:?}"),
            })
            .collect();
        msgs.sort_unstable();
        assert_eq!(msgs, [[0x80, 60, 0], [0x80, 64, 0]]);
        assert!(held.iter().flatten().all(|h| !h), "reset must clear held");
    }

    /// Every release survives a reset in both dialects, not only 0x80: pedals going up (64, 66,
    /// 67, 69), CC 120, 121 and 123, and note-ons with velocity 0 (review F10-1).
    #[test]
    fn reset_keeps_every_release_in_both_dialects() {
        let releases: [[u8; 3]; 9] = [
            [0xB0, 64, 0],
            [0xB1, 66, 0],
            [0xB2, 67, 0],
            [0xB3, 69, 10],
            [0xB4, 120, 0],
            [0xB5, 121, 0],
            [0xB6, 123, 0],
            [0x97, 60, 0],
            [0x88, 61, 0],
        ];
        let others: [[u8; 3]; 5] = [[0xB0, 64, 127], [0xB0, 1, 0], [0x90, 62, 100], [0xE0, 0, 64], [0xA0, 60, 3]];
        let mut queue = Vec::with_capacity(EVENT_CAP);
        for (t, data) in (0u32..).zip(others.iter().chain(&releases)) {
            queue.push(Event::Midi { time: t, data: *data });
        }
        queue.push(Event::NoteOn { time: 20, key: 70, velocity: 0.0, channel: 2, id: 9 });
        queue.push(Event::NoteOn { time: 21, key: 71, velocity: 0.5, channel: 2, id: 10 });
        let mut held = [[false; 128]; 16];
        keep_releases_on_reset(&mut queue, &mut held, NoteMode::Midi);
        let kept: Vec<[u8; 3]> = queue.iter().filter_map(|e| if let Event::Midi { data, .. } = e { Some(*data) } else { None }).collect();
        assert_eq!(kept, releases, "every MIDI release survives, in order, and nothing else");
        assert_eq!(queue.last(), Some(&Event::NoteOn { time: 20, key: 70, velocity: 0.0, channel: 2, id: 9 }), "a velocity-0 note-on is a release");
        assert_eq!(queue.len(), releases.len() + 1);
    }
}
