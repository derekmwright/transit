//! Native MIDI ingestion, independent of the UI pump. The worker owns only raw
//! consumers and a bounded action producer, never devices, plug-ins or editors.
//! Main retains the input handles; closing one invalidates its source first.
use super::{ActionResult, PlayedMidi, PlayedNote, RuntimeControl, sources};
use crate::action_lane::{self, NoteKind, Outcome};
use soundcraft_mix::{EngineEpoch, EventOutcome, Message, NoteId, SlotBinding, Source};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;
use transit_midi_io::{InputBoundary, InputConsumer, InputEvent};

const BATCH: usize = 256;
const RELEASE_RESERVE: usize = 32;
/// After work, the worker keeps stepping at this interval for `LINGER` rounds so the audio
/// thread's receipts (credits, release bookkeeping) are reclaimed promptly. `thread::sleep` uses a
/// high-resolution timer on Windows; a park timeout would round up to the system tick (#62).
const LINGER_PERIOD: Duration = Duration::from_micros(250);
const LINGER: u32 = 16;
/// Idle safety net only: input data, attachments, route edits and stop all unpark the worker.
/// Port health (unplug, close) is also noticed within this period.
const IDLE: Duration = Duration::from_millis(20);

/// Original timestamps and raw sequence survive scheduling. Audio maps H into
/// its consumed-output/converter phase and rechecks the source cutoff there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    pub engine: EngineEpoch,
    pub target: SlotBinding,
    pub source: Source,
    pub message: Message,
    pub raw: InputEvent,
    /// Explicit monitoring lookahead in output-device frames, never callbacks.
    pub lookahead: u32,
    /// False for a track with an external MIDI destination (#61): the audio thread only
    /// captures the event for a MIDI recording; the instrument does not play it.
    pub deliver: bool,
}
const _: () = assert!(std::mem::size_of::<Packet>() <= 128);

pub type Lane = (action_lane::Control<Packet, ActionResult>, action_lane::Audio<Packet, ActionResult>);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub offered: u64,
    pub admitted: u64,
    pub filtered: u64,
    pub cancelled: u64,
    pub shed: u64,
    pub recovered: u64,
    pub refused: u64,
    /// Audio-admitted packets using approximate callback-relative timing.
    pub approximate: u64,
    /// Input-to-worker latency (backend callback entry to the worker popping the event) of the
    /// latest event, and the largest seen. Add the audio block period for input-to-sound.
    pub latency_last_ns: u64,
    pub latency_max_ns: u64,
}

/// UI tap capacity, preallocated before the ingress thread starts.
const TAP_CAPACITY: usize = 256;

#[derive(Default)]
struct Shared {
    /// Overflow suppresses further tap writes until the UI discards the incomplete batch.
    tap_lost: AtomicBool,
    stop: AtomicBool,
    finished: AtomicBool,
    offered: AtomicU64,
    admitted: AtomicU64,
    filtered: AtomicU64,
    cancelled: AtomicU64,
    shed: AtomicU64,
    recovered: AtomicU64,
    refused: AtomicU64,
    approximate: AtomicU64,
    /// Callback-to-worker-pop age of the latest and the largest popped event (#62).
    latency_last_ns: AtomicU64,
    latency_max_ns: AtomicU64,
}

/// Main's per-open cancellation identity. Capture/publish all port cutoffs
/// before admitting the corresponding transport Stop. This never drains a ring.
pub struct Port {
    permit: sources::Permit,
    boundary: InputBoundary,
}
impl Port {
    pub(super) fn new(permit: sources::Permit, boundary: InputBoundary) -> Self {
        Self { permit, boundary }
    }
    pub(super) fn permit(&self) -> sources::Permit {
        self.permit.clone()
    }
    pub fn token(&self) -> sources::Token {
        self.permit.token()
    }
    pub fn cancel_before_stop<T: Copy, R: Copy>(&self, owner: &mut RuntimeControl<T, R>) -> Result<(), super::PlayerError> {
        owner.cancel_source_before(self.token(), self.boundary.cutoff())
    }
    pub fn invalidate(&self) {
        self.permit.invalidate();
    }
    pub fn admitted(&self) -> bool {
        self.permit.admits()
    }
}
impl Drop for Port {
    fn drop(&mut self) {
        self.invalidate();
    }
}

/// A route is bound to one instrument incarnation. A replacement needs a new
/// binding; stale queued packets are refused by the mixer, never retargeted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub engine: EngineEpoch,
    pub target: SlotBinding,
    pub channels: u16,
    pub lookahead: u32,
    /// Play the event on the target instrument (false: capture only, see [`Packet::deliver`]).
    pub deliver: bool,
    /// Forward the event to the track's external MIDI destination through the output thread's
    /// echo ring, as soon as it is popped here (#61).
    pub echo: bool,
}

struct Input {
    id: u64,
    consumer: InputConsumer,
    routes: Vec<(sources::Permit, Route)>,
}
impl Input {
    fn healthy(&self) -> bool {
        !self.consumer.is_closed()
            && !self.consumer.is_disconnected()
            && !self.consumer.is_exhausted()
            && self.consumer.counters().dropped_releases == 0
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        for (permit, _) in &self.routes {
            permit.invalidate();
        }
    }
}

enum Update {
    Input(Input),
    Echo(super::midi_out::Producer),
    Routes { id: u64, routes: Vec<(sources::Permit, Route)>, applied: Arc<AtomicBool> },
}

/// Control-side handle. Drop closes admission immediately and wakes the worker;
/// it does not join or move an input-device destructor onto the audio thread.
pub struct Worker {
    attach: SyncSender<Update>,
    permits: Vec<sources::Permit>,
    next_input: u64,
    shared: Arc<Shared>,
    thread: std::thread::JoinHandle<()>,
    tap: RefCell<rtrb::Consumer<PlayedMidi>>,
}
impl Worker {
    pub fn start(lane: action_lane::Control<Packet, ActionResult>) -> std::io::Result<Self> {
        let (attach, receive) = mpsc::sync_channel(sources::PORTS);
        let shared = Arc::new(Shared::default());
        let (tap, tap_receive) = rtrb::RingBuffer::new(TAP_CAPACITY);
        let mut pump = Pump { lane, receive, inputs: Vec::with_capacity(sources::PORTS), cursor: 0, shared: Arc::clone(&shared), echo: None, tap };
        let thread = std::thread::Builder::new().name("soundcraft-midi-ingress".into()).spawn(move || {
            let mut linger = 0u32;
            while !pump.shared.stop.load(Ordering::Acquire) {
                if pump.step() {
                    linger = LINGER;
                }
                if linger > 0 {
                    linger -= 1;
                    std::thread::sleep(LINGER_PERIOD);
                } else {
                    // Woken by the input callback (`InputConsumer::wake_on`) or an attach.
                    std::thread::park_timeout(IDLE);
                }
            }
        })?;
        Ok(Self { attach, permits: Vec::with_capacity(sources::PORTS), next_input: 0, shared, thread, tap: RefCell::new(tap_receive) })
    }

    /// Open the input with this runtime's HostOrigin before attaching. The caller
    /// retains its InputHandle and this Port until it explicitly disconnects.
    pub fn attach(&mut self, consumer: InputConsumer, permit: sources::Permit, route: Route) -> Result<Port, &'static str> {
        self.permits.retain(sources::Permit::admits);
        if route.channels == 0 || route.lookahead > 8192 {
            permit.invalidate();
            return Err("invalid MIDI route or lookahead");
        }
        if self.shared.finished.load(Ordering::Acquire) || self.permits.len() >= sources::PORTS || !permit.admits() {
            permit.invalidate();
            return Err("MIDI worker unavailable or source revoked");
        }
        if self.permits.iter().any(|old| old.token() == permit.token()) {
            return Err("MIDI source already attached");
        }
        consumer.wake_on(self.thread.thread().clone());
        let port = Port { boundary: consumer.boundary(), permit: permit.clone() };
        let id = self.next_input.checked_add(1).ok_or("MIDI input identities exhausted")?;
        self.next_input = id;
        self.attach
            .try_send(Update::Input(Input { id, consumer, routes: vec![(permit.clone(), route)] }))
            .map_err(|_| "MIDI worker attachment queue unavailable")?;
        self.permits.push(permit);
        self.thread.thread().unpark();
        Ok(port)
    }

    /// One consumer per physical open; routing edits never reopen the device.
    pub(super) fn attach_input(&mut self, consumer: InputConsumer) -> Result<u64, &'static str> {
        if self.shared.finished.load(Ordering::Acquire) {
            return Err("MIDI worker unavailable");
        }
        let id = self.next_input.checked_add(1).ok_or("MIDI input identities exhausted")?;
        self.next_input = id;
        consumer.wake_on(self.thread.thread().clone());
        self.attach.try_send(Update::Input(Input { id, consumer, routes: Vec::new() })).map_err(|_| "MIDI attachment queue unavailable")?;
        self.thread.thread().unpark();
        Ok(id)
    }

    pub(super) fn set_routes(&mut self, id: u64, routes: Vec<(sources::Permit, Route)>) -> Result<Arc<AtomicBool>, &'static str> {
        self.permits.retain(sources::Permit::admits);
        if self.shared.finished.load(Ordering::Acquire)
            || routes.len() > sources::PORTS
            || routes.iter().any(|(permit, route)| !permit.admits() || route.channels == 0 || route.lookahead > 8192)
        {
            return Err("MIDI route unavailable or invalid");
        }
        for (permit, _) in &routes {
            if !self.permits.iter().any(|old| old.token() == permit.token()) {
                if self.permits.len() == sources::PORTS {
                    return Err("MIDI source capacity exhausted");
                }
                self.permits.push(permit.clone());
            }
        }
        let applied = Arc::new(AtomicBool::new(false));
        self.attach.try_send(Update::Routes { id, routes, applied: Arc::clone(&applied) }).map_err(|_| "MIDI route queue unavailable")?;
        self.thread.thread().unpark();
        Ok(applied)
    }

    /// Hands the worker the output thread's echo producer (#61).
    pub(super) fn set_echo(&mut self, echo: super::midi_out::Producer) -> Result<(), &'static str> {
        self.attach.try_send(Update::Echo(echo)).map_err(|_| "MIDI echo queue unavailable")?;
        self.thread.thread().unpark();
        Ok(())
    }

    /// The notes played since the last call, oldest first.
    pub fn take_notes(&self) -> Vec<PlayedNote> {
        self.take_midi()
            .into_iter()
            .filter_map(|m| match m {
                PlayedMidi::Note(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    /// Ordered notes and controls; only the UI allocates or borrows the consumer.
    pub fn take_midi(&self) -> Vec<PlayedMidi> {
        let Ok(mut tap) = self.tap.try_borrow_mut() else { return Vec::new() };
        let mut messages = Vec::new();
        // Bound UI work as well: input may continue arriving while this frame drains.
        for _ in 0..TAP_CAPACITY {
            let Ok(message) = tap.pop() else { break };
            messages.push(message);
        }
        if self.shared.tap_lost.load(Ordering::Acquire) {
            while tap.pop().is_ok() {}
            messages.clear();
            messages.push(PlayedMidi::Reset);
            self.shared.tap_lost.store(false, Ordering::Release);
        }
        messages
    }

    /// Independent diagnostic counters, not a coherent cancellation frontier.
    pub fn counters(&self) -> Counters {
        let s = &self.shared;
        Counters {
            offered: s.offered.load(Ordering::Relaxed),
            admitted: s.admitted.load(Ordering::Relaxed),
            filtered: s.filtered.load(Ordering::Relaxed),
            cancelled: s.cancelled.load(Ordering::Relaxed),
            shed: s.shed.load(Ordering::Relaxed),
            recovered: s.recovered.load(Ordering::Relaxed),
            refused: s.refused.load(Ordering::Relaxed),
            approximate: s.approximate.load(Ordering::Relaxed),
            latency_last_ns: s.latency_last_ns.load(Ordering::Relaxed),
            latency_max_ns: s.latency_max_ns.load(Ordering::Relaxed),
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        for permit in &self.permits {
            permit.invalidate();
        }
        self.shared.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
    }
}

struct Pump {
    lane: action_lane::Control<Packet, ActionResult>,
    receive: Receiver<Update>,
    inputs: Vec<Input>,
    cursor: usize,
    shared: Arc<Shared>,
    echo: Option<super::midi_out::Producer>,
    tap: rtrb::Producer<PlayedMidi>,
}
impl Pump {
    fn tap(&mut self, message: Message) {
        let message = match message {
            Message::NoteOn { key, velocity, .. } => PlayedMidi::Note(PlayedNote { pitch: key, velocity, on: true }),
            Message::NoteOff { key, .. } => PlayedMidi::Note(PlayedNote { pitch: key, velocity: 0, on: false }),
            Message::Control { cc: 120 | 123, .. } => PlayedMidi::Reset,
            Message::Control { cc, value } => PlayedMidi::Control { cc, value },
            _ => return,
        };
        if !self.shared.tap_lost.load(Ordering::Acquire) && self.tap.push(message).is_err() {
            self.shared.tap_lost.store(true, Ordering::Release);
        }
    }
}
impl Pump {
    /// One pass; true when it popped at least one event.
    fn step(&mut self) -> bool {
        let mut worked = false;
        // Drop only raw ring consumers here, never the native device handles.
        self.inputs.retain(|input| {
            let keep = input.healthy();
            if !keep {
                self.shared.recovered.fetch_add(1, Ordering::Relaxed);
            }
            keep
        });
        for _ in 0..sources::PORTS {
            match self.receive.try_recv() {
                Ok(Update::Input(input)) => {
                    if self.inputs.len() < sources::PORTS && input.healthy() {
                        self.inputs.push(input);
                    }
                }
                Ok(Update::Echo(echo)) => self.echo = Some(echo),
                Ok(Update::Routes { id, routes, applied }) => {
                    if let Some(input) = self.inputs.iter_mut().find(|input| input.id == id) {
                        for (old, _) in &input.routes {
                            if !routes.iter().any(|(new, _)| old.token() == new.token()) {
                                old.invalidate();
                            }
                        }
                        input.routes = routes;
                        applied.store(true, Ordering::Release);
                    } else {
                        for (permit, _) in routes {
                            permit.invalidate();
                        }
                    }
                }
                Err(_) => break,
            }
        }
        for _ in 0..action_lane::NOTE_CAPACITY {
            let Some(receipt) = self.lane.reclaim() else { break };
            let accepted = receipt.outcome == Outcome::Applied
                && matches!(
                    receipt.handler_result,
                    Some(ActionResult::MidiBuffered)
                        | Some(ActionResult::MidiBufferedApproximate)
                        | Some(ActionResult::Event(
                            EventOutcome::Queued
                                | EventOutcome::QueuedLate { .. }
                                | EventOutcome::ReleaseAdvanced { .. }
                                | EventOutcome::Coalesced
                                | EventOutcome::Unmatched
                        ))
                );
            if accepted && receipt.handler_result == Some(ActionResult::MidiBufferedApproximate) {
                self.shared.approximate.fetch_add(1, Ordering::Relaxed);
            }
            if !accepted {
                self.shared.refused.fetch_add(1, Ordering::Relaxed);
                if receipt.payload.raw.is_release() {
                    for (permit, _) in self.inputs.iter().flat_map(|input| &input.routes) {
                        let token = permit.token();
                        if token.port() == receipt.payload.source.port
                            && token.generation() == receipt.payload.source.generation
                            && permit.admits_sequence(receipt.payload.raw.seq)
                            && !matches!(receipt.outcome, Outcome::Stopped | Outcome::NotesReleased)
                        {
                            permit.invalidate();
                        }
                    }
                }
            }
        }
        // A fixed amount of work even under continuous MIDI traffic. Round-robin
        // across ports prevents one input backlog from starving another release.
        for _ in 0..BATCH {
            if self.inputs.is_empty() {
                break;
            }
            self.cursor %= self.inputs.len();
            let Some(input) = self.inputs.get_mut(self.cursor) else { break };
            self.cursor += 1;
            if !input.healthy() {
                for (permit, _) in &input.routes {
                    permit.invalidate();
                }
                continue;
            }
            // An input can attach before its initial routing update arrives.
            // Preserve that backlog; each new route's raw cutoff rejects older
            // offers once the worker applies it. Admission is not application.
            if !input.routes.iter().any(|(permit, _)| permit.admits()) {
                continue;
            }
            let Some(raw) = input.consumer.pop() else { continue };
            worked = true;
            let age = input.consumer.age_ns(&raw);
            self.shared.latency_last_ns.store(age, Ordering::Relaxed);
            self.shared.latency_max_ns.fetch_max(age, Ordering::Relaxed);
            self.shared.offered.fetch_add(1, Ordering::Relaxed);
            let Some((channel, message)) = decode(raw) else {
                self.shared.filtered.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let mut tapped = false;
            for (permit, route) in &input.routes {
                if !permit.admits_sequence(raw.seq) {
                    self.shared.cancelled.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if route.channels & (1 << channel) == 0 {
                    self.shared.filtered.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if route.echo
                    && let Some(echo) = &mut self.echo
                {
                    echo.push(super::midi_out::OutEvent {
                        kind: super::midi_out::Kind::Message,
                        track: route.target.0.0,
                        at_ns: raw.host_arrival_ns,
                        bytes: raw.bytes,
                        len: raw.len,
                    });
                }
                let release = raw.is_release();
                let credits = self.lane.note_credits();
                if credits == 0 || (!release && credits <= RELEASE_RESERVE) {
                    self.shared.shed.fetch_add(1, Ordering::Relaxed);
                    if release {
                        permit.invalidate();
                    }
                    continue;
                }
                let token = permit.token();
                let packet = Packet {
                    engine: route.engine,
                    target: route.target,
                    source: Source { port: token.port(), channel, generation: token.generation() },
                    message,
                    raw,
                    lookahead: route.lookahead,
                    deliver: route.deliver,
                };
                if self.lane.send_note(packet, if release { NoteKind::Off } else { NoteKind::On }).is_ok() {
                    self.shared.admitted.fetch_add(1, Ordering::Relaxed);
                    tapped = true;
                } else {
                    self.shared.shed.fetch_add(1, Ordering::Relaxed);
                    permit.invalidate();
                }
            }
            if tapped {
                self.tap(message);
            }
        }
        worked
    }
}
impl Drop for Pump {
    fn drop(&mut self) {
        self.shared.finished.store(true, Ordering::Release);
        // Close admission for every input before reclaiming any worker storage.
        for (permit, _) in self.inputs.iter().flat_map(|input| &input.routes) {
            permit.invalidate();
        }
        for _ in 0..sources::PORTS {
            match self.receive.try_recv() {
                Ok(Update::Input(input)) => drop(input),
                Ok(Update::Echo(echo)) => drop(echo),
                Ok(Update::Routes { routes, .. }) => {
                    for (permit, _) in routes {
                        permit.invalidate();
                    }
                }
                Err(_) => break,
            }
        }
    }
}

fn decode(raw: InputEvent) -> Option<(u8, Message)> {
    let status = raw.bytes[0];
    if !(0x80..0xf0).contains(&status) || raw.len != if status & 0xe0 == 0xc0 { 2 } else { 3 } {
        return None;
    }
    if raw.message().iter().skip(1).any(|byte| *byte > 127) {
        return None;
    }
    let (a, b) = (raw.bytes[1], raw.bytes[2]);
    let message = match status & 0xf0 {
        0x80 => Message::NoteOff { key: a, velocity: b, id: NoteId::NONE },
        0x90 if b == 0 => Message::NoteOff { key: a, velocity: 0, id: NoteId::NONE },
        0x90 => Message::NoteOn { key: a, velocity: b, id: NoteId::NONE },
        0xa0 => Message::PolyPressure { key: a, value: b, id: NoteId::NONE },
        0xb0 => Message::Control { cc: a, value: b },
        0xc0 => Message::Program { program: a },
        0xd0 => Message::ChannelPressure { value: a },
        0xe0 => Message::PitchBend { value: u16::from(a) | (u16::from(b) << 7) },
        _ => return None,
    };
    Some((status & 0x0f, message))
}

#[cfg(test)]
mod tests;
