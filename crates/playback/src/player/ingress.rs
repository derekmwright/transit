//! Native MIDI ingestion, independent of the UI pump. The worker owns only raw
//! consumers and a bounded action producer, never devices, plug-ins or editors.
//! Main retains the input handles; closing one invalidates its source first.
use super::{ActionResult, RuntimeControl, sources};
use crate::action_lane::{self, NoteKind, Outcome};
use soundcraft_midi_io::{InputBoundary, InputConsumer, InputEvent};
use soundcraft_mix::{EngineEpoch, EventOutcome, Message, NoteId, SlotBinding, Source};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

const BATCH: usize = 256;
const RELEASE_RESERVE: usize = 32;
const PERIOD: Duration = Duration::from_millis(1);

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
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    finished: AtomicBool,
    offered: AtomicU64,
    admitted: AtomicU64,
    filtered: AtomicU64,
    cancelled: AtomicU64,
    shed: AtomicU64,
    recovered: AtomicU64,
    refused: AtomicU64,
}

/// Main's per-open cancellation identity. Capture/publish all port cutoffs
/// before admitting the corresponding transport Stop. This never drains a ring.
pub struct Port {
    permit: sources::Permit,
    boundary: InputBoundary,
}
impl Port {
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
#[derive(Clone, Copy)]
pub struct Route {
    pub engine: EngineEpoch,
    pub target: SlotBinding,
    pub channels: u16,
    pub lookahead: u32,
}

struct Input {
    consumer: InputConsumer,
    permit: sources::Permit,
    route: Route,
}
impl Input {
    fn healthy(&self) -> bool {
        self.permit.admits()
            && !self.consumer.is_closed()
            && !self.consumer.is_disconnected()
            && !self.consumer.is_exhausted()
            && self.consumer.counters().dropped_releases == 0
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        self.permit.invalidate();
    }
}

/// Control-side handle. Drop closes admission immediately and wakes the worker;
/// it does not join or move an input-device destructor onto the audio thread.
pub struct Worker {
    attach: SyncSender<Input>,
    permits: Vec<sources::Permit>,
    shared: Arc<Shared>,
    thread: std::thread::JoinHandle<()>,
}
impl Worker {
    pub fn start(lane: action_lane::Control<Packet, ActionResult>) -> std::io::Result<Self> {
        let (attach, receive) = mpsc::sync_channel(sources::PORTS);
        let shared = Arc::new(Shared::default());
        let mut pump = Pump { lane, receive, inputs: Vec::with_capacity(sources::PORTS), cursor: 0, shared: Arc::clone(&shared) };
        let thread = std::thread::Builder::new().name("soundcraft-midi-ingress".into()).spawn(move || {
            while !pump.shared.stop.load(Ordering::Acquire) {
                pump.step();
                std::thread::park_timeout(PERIOD);
            }
        })?;
        Ok(Self { attach, permits: Vec::with_capacity(sources::PORTS), shared, thread })
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
        let port = Port { boundary: consumer.boundary(), permit: permit.clone() };
        self.attach.try_send(Input { consumer, permit: permit.clone(), route }).map_err(|_| "MIDI worker attachment queue unavailable")?;
        self.permits.push(permit);
        self.thread.thread().unpark();
        Ok(port)
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
    receive: Receiver<Input>,
    inputs: Vec<Input>,
    cursor: usize,
    shared: Arc<Shared>,
}
impl Pump {
    fn step(&mut self) {
        // Drop only raw ring consumers here, never the native device handles.
        self.inputs.retain(|input| {
            let keep = input.healthy();
            if !keep {
                self.shared.recovered.fetch_add(1, Ordering::Relaxed);
            }
            keep
        });
        for _ in 0..sources::PORTS {
            let Ok(input) = self.receive.try_recv() else { break };
            if self.inputs.len() < sources::PORTS && input.healthy() {
                self.inputs.push(input);
            }
        }
        for _ in 0..action_lane::NOTE_CAPACITY {
            let Some(receipt) = self.lane.reclaim() else { break };
            let accepted = receipt.outcome == Outcome::Applied
                && matches!(
                    receipt.handler_result,
                    Some(ActionResult::Event(
                        EventOutcome::Queued | EventOutcome::QueuedLate { .. } | EventOutcome::ReleaseAdvanced { .. } | EventOutcome::Coalesced
                    ))
                );
            if !accepted {
                self.shared.refused.fetch_add(1, Ordering::Relaxed);
                if receipt.payload.raw.is_release() {
                    for input in &self.inputs {
                        let token = input.permit.token();
                        if token.port() == receipt.payload.source.port
                            && token.generation() == receipt.payload.source.generation
                            && input.permit.admits_sequence(receipt.payload.raw.seq)
                            && !matches!(receipt.outcome, Outcome::Stopped | Outcome::NotesReleased)
                        {
                            input.permit.invalidate();
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
                input.permit.invalidate();
                continue;
            }
            let Some(raw) = input.consumer.pop() else { continue };
            self.shared.offered.fetch_add(1, Ordering::Relaxed);
            if !input.permit.admits_sequence(raw.seq) {
                self.shared.cancelled.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let Some((channel, message)) = decode(raw) else {
                self.shared.filtered.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if input.route.channels & (1 << channel) == 0 {
                self.shared.filtered.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let release = raw.is_release();
            let credits = self.lane.note_credits();
            if credits == 0 || (!release && credits <= RELEASE_RESERVE) {
                self.shared.shed.fetch_add(1, Ordering::Relaxed);
                if release {
                    input.permit.invalidate();
                }
                continue;
            }
            let token = input.permit.token();
            let packet = Packet {
                engine: input.route.engine,
                target: input.route.target,
                source: Source { port: token.port(), channel, generation: token.generation() },
                message,
                raw,
                lookahead: input.route.lookahead,
            };
            if self.lane.send_note(packet, if release { NoteKind::Off } else { NoteKind::On }).is_ok() {
                self.shared.admitted.fetch_add(1, Ordering::Relaxed);
            } else {
                self.shared.shed.fetch_add(1, Ordering::Relaxed);
                input.permit.invalidate();
            }
        }
    }
}
impl Drop for Pump {
    fn drop(&mut self) {
        self.shared.finished.store(true, Ordering::Release);
        // Close admission for every input before reclaiming any worker storage.
        for input in &self.inputs {
            input.permit.invalidate();
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
