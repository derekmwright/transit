//! Sample-accurate, identity-carrying instrument events (F10, ADR 0012 G5).
//!
//! Playback hands each event to [`MixEngine::queue_event`] on the audio thread, between blocks.
//! The engine checks the event's engine epoch, then its instrument binding, then its transport
//! epoch, and queues it on the instrument it names: each plan-mode instrument slot owns a
//! 512-entry queue and a table of the notes and pedals it holds open, prepared by the compiler.
//! They travel with the instance and retire with it, so a queued event never reaches a
//! replacement.
//!
//! **Releases are reserved.** Each open note (at most [`MAX_OPEN`]) and pedal (at most
//! [`MAX_PEDALS`]) holds one release slot from its admission until its release is *delivered*.
//! Other events use the remaining 256 slots. A note-on the table cannot hold is refused
//! (`NotesFull`), and so is a pedal-down (`PedalsFull`): nothing is delivered untracked.
//!
//! **Delivery**, before the instrument's `process`: controller chase, then the clips (as
//! [`Source::CLIP`], through the same note and pedal table), then the queue entries due in the
//! block at `due − block start`. Pedals are tracked per source; the plug-in sees the OR across
//! sources, evaluated in due order. The mix assigns every note a voice id of its own
//! ([`PluginEvent::note_id`]), so two sources' notes on one key stay apart.
//!
//! Nothing here allocates, locks or logs on the audio thread; counters are fixed storage read
//! with [`MixEngine::event_status`].

use super::plan::SlotGen;
use super::{INSTRUMENT_SLOT, MixEngine, Plugin, PluginSlot, SlotBinding, Strip};
use soundcraft_dsp::event::{CC_ALL_NOTES_OFF, CC_ALL_SOUND_OFF, CC_RESET_CONTROLLERS, CC_SOSTENUTO, CC_SUSTAIN, is_pedal};
pub use soundcraft_dsp::event::{Message, NoteId, PluginEvent};

/// A position on the engine's render clock (annex R): the frame count rendered since the engine
/// started, monotonic through same-rate plans and seeks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RenderFrame(pub u64);

/// One engine (device and rate) incarnation. Playback creates it before constructing the engine
/// ([`MixEngine::realtime_with_events`]); same-rate plans and seeks keep it, and a new engine or
/// rate gets a new one. Checked: it never wraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EngineEpoch(pub u64);

impl EngineEpoch {
    /// The first epoch a process issues.
    pub const FIRST: Self = EngineEpoch(1);

    /// The next epoch, `None` once the counter is exhausted (it never wraps).
    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(EngineEpoch)
    }
}

/// Playback's transport identity (its timeline `transport`): it changes with every seek, start,
/// stop and loop wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransportEpoch(pub u64);

/// When an event is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Offset 0 of the next block (audition).
    Next,
    Frame(RenderFrame),
}

/// Where an event came from: an ingress port, its MIDI channel (0..=15) and the port's
/// generation. Note and pedal matching always includes the whole source.
///
/// `generation` is checked: a port whose counter would overflow is exhausted, never wrapped, and
/// a port slot is not reused before its old generation's revocation is acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Source {
    pub port: u16,
    pub channel: u8,
    pub generation: u64,
}

impl Source {
    /// The UI's audition port.
    pub const UI: u16 = 0xFFFE;
    /// Clips (the mix's own scheduling; never through the queue).
    pub const CLIP: u16 = 0xFFFF;

    /// The slot of this source's port in the engine's revocation table.
    fn port_slot(port: u16) -> Option<usize> {
        match port {
            Source::UI => Some(MAX_PORTS),
            Source::CLIP => Some(MAX_PORTS + 1),
            p => Some(usize::from(p)).filter(|&p| p < MAX_PORTS),
        }
    }

    /// The same port generation (any channel).
    fn sender_is(&self, port: u16, generation: u64) -> bool {
        self.port == port && self.generation == generation
    }
}

/// The clips' source on MIDI channel 1.
const CLIP_SOURCE: Source = Source { port: Source::CLIP, channel: 0, generation: 0 };

/// When and in what order playback received an event. `sequence` is checked and never wraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrival {
    pub host_ns: u64,
    pub sequence: u64,
}

/// One event for one instrument, as playback submits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstrumentEvent {
    /// Checked before the binding: a [`SlotGen`] means something only to the compiler of its engine.
    pub engine: EngineEpoch,
    /// (track, [`INSTRUMENT_SLOT`], generation) of the instrument instance.
    pub target: SlotBinding,
    pub due: Due,
    /// `Some` only for clip- or transport-derived events: refused, or cancelled once queued,
    /// when the transport moves on.
    pub transport: Option<TransportEpoch>,
    pub source: Source,
    pub message: Message,
    pub arrival: Arrival,
}

const _: () = assert!(size_of::<InstrumentEvent>() <= 112);

/// What [`MixEngine::queue_event`] did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOutcome {
    Queued,
    /// Due before the next block: queued at its offset 0, counted.
    QueuedLate {
        frames: u32,
    },
    /// Recovery: a release due beyond the horizon was clamped to it (never before its note-on),
    /// counted.
    ReleaseAdvanced {
        frames: u32,
    },
    /// A controller value replaced one queued for the same frame, or a duplicate release for an
    /// entry whose release is already queued (no extra slot).
    Coalesced,
    /// The engine epoch differs (or the engine takes no realtime events).
    StaleEngine,
    /// The source's port generation was revoked ([`MixEngine::revoke_source`]): a late event
    /// from it never starts anything.
    RevokedSource,
    /// The source's port is neither below [`MAX_PORTS`] nor [`Source::UI`] or [`Source::CLIP`].
    PortOutOfRange,
    /// The instrument at the target is another instance.
    StaleBinding,
    /// `transport` is `Some` and not current.
    StaleTransport,
    /// No strip, no instrument, a failed instance, or a slot that is not the instrument.
    NoInstrument,
    /// A non-release due beyond the horizon: playback retains it and submits it later.
    TooEarly,
    /// A release for a note or pedal this source does not hold: nothing to do.
    Unmatched,
    Refused(Shed),
    /// The engine faulted (an adoption unwound): it takes nothing.
    Faulted,
}

impl EventOutcome {
    /// The event is in the queue (or merged into a queued one).
    #[must_use]
    pub const fn accepted(&self) -> bool {
        matches!(self, EventOutcome::Queued | EventOutcome::QueuedLate { .. } | EventOutcome::ReleaseAdvanced { .. } | EventOutcome::Coalesced)
    }
}

/// Why an event was shed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shed {
    /// The 256 slots for non-releases are taken.
    QueueFull,
    /// [`MAX_OPEN`] notes are open.
    NotesFull,
    /// [`MAX_PEDALS`] pedals are down.
    PedalsFull,
}

/// The engine's limits on queued events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLimits {
    /// How far past the next block an event may be due, in render frames (a limit, not a
    /// latency guarantee; not derived from the block size).
    pub max_future_frames: u32,
}

impl Default for EventLimits {
    fn default() -> Self {
        EventLimits { max_future_frames: 8192 }
    }
}

/// Shed events by kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ShedCounts {
    pub note_on: u64,
    pub pedal_down: u64,
    pub control: u64,
    pub pressure: u64,
    pub bend: u64,
    pub program: u64,
}

/// Event counters in fixed storage, read like [`PlanStatus`](crate::PlanStatus) and published by
/// playback. Cumulative for the engine's life (an instrument's delivery counters fold in when it
/// retires).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EventStatus {
    pub queued: u64,
    pub late: u64,
    pub max_late_frames: u32,
    /// Late events by lateness: bucket `b` holds 2^(b−1) ≤ frames < 2^b (the last one, more).
    pub lateness_log2: [u32; 16],
    /// Refused: engine, binding, transport.
    pub refused_stale: [u64; 3],
    /// Refused because their port generation was revoked, or their port is out of range.
    pub refused_revoked: u64,
    pub refused_no_instrument: u64,
    pub refused_too_early: u64,
    pub shed: ShedCounts,
    /// Releases the mix failed to deliver. Must stay 0.
    pub dropped_releases: u64,
    pub releases_advanced: u64,
    /// The release-reservation invariant broke (a bug): an instrument's queued non-releases
    /// were dropped to make room.
    pub release_collapses: u64,
    /// Refused by the plug-in's own queue: [non-release, release].
    pub host_refused: [u64; 2],
    pub unmatched: u64,
    /// A second note-on for a held key or id: the old voice was released first.
    pub restrikes: u64,
    /// Releases moved after their own note-on.
    pub reordered: u64,
    pub coalesced: u64,
    /// Pedal edges the OR across sources absorbed (not drops).
    pub absorbed_pedal_edges: u64,
    /// Queued transport-derived events cancelled by a transport change.
    pub cancelled_transport: u64,
    /// Queued events cancelled and notes or pedals released by a source revocation.
    pub revoked: u64,
    /// Queued events that retired with their instance: [non-release, release].
    pub retired_with_instance: [u64; 2],
    /// The instrument of the last refusal or shed.
    pub affected: Option<SlotBinding>,
}

impl EventStatus {
    /// Adds `o`'s counters (`affected` is kept unless unset).
    fn absorb(&mut self, o: &EventStatus) {
        let add = |a: &mut u64, b: u64| *a = a.saturating_add(b);
        add(&mut self.queued, o.queued);
        add(&mut self.late, o.late);
        self.max_late_frames = self.max_late_frames.max(o.max_late_frames);
        for (a, b) in self.lateness_log2.iter_mut().zip(o.lateness_log2) {
            *a = a.saturating_add(b);
        }
        for (a, b) in self.refused_stale.iter_mut().zip(o.refused_stale) {
            add(a, b);
        }
        add(&mut self.refused_no_instrument, o.refused_no_instrument);
        add(&mut self.refused_revoked, o.refused_revoked);
        add(&mut self.refused_too_early, o.refused_too_early);
        let (s, t) = (&mut self.shed, o.shed);
        add(&mut s.note_on, t.note_on);
        add(&mut s.pedal_down, t.pedal_down);
        add(&mut s.control, t.control);
        add(&mut s.pressure, t.pressure);
        add(&mut s.bend, t.bend);
        add(&mut s.program, t.program);
        add(&mut self.dropped_releases, o.dropped_releases);
        add(&mut self.releases_advanced, o.releases_advanced);
        add(&mut self.release_collapses, o.release_collapses);
        for (a, b) in self.host_refused.iter_mut().zip(o.host_refused) {
            add(a, b);
        }
        add(&mut self.unmatched, o.unmatched);
        add(&mut self.restrikes, o.restrikes);
        add(&mut self.reordered, o.reordered);
        add(&mut self.coalesced, o.coalesced);
        add(&mut self.absorbed_pedal_edges, o.absorbed_pedal_edges);
        add(&mut self.cancelled_transport, o.cancelled_transport);
        add(&mut self.revoked, o.revoked);
        for (a, b) in self.retired_with_instance.iter_mut().zip(o.retired_with_instance) {
            add(a, b);
        }
        if o.affected.is_some() {
            self.affected = o.affected;
        }
    }

    fn shed(&mut self, msg: &Message, binding: SlotBinding) {
        let s = &mut self.shed;
        let n = match msg {
            Message::NoteOn { .. } | Message::NoteOff { .. } => &mut s.note_on,
            Message::Control { cc, .. } if is_pedal(*cc) => &mut s.pedal_down,
            Message::Control { .. } | Message::Control14 { .. } => &mut s.control,
            Message::PolyPressure { .. } | Message::ChannelPressure { .. } => &mut s.pressure,
            Message::PitchBend { .. } => &mut s.bend,
            Message::Program { .. } => &mut s.program,
        };
        *n = n.saturating_add(1);
        self.affected = Some(binding);
    }
}

/// What the event tap recorded for one submitted event (G5 boundary tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapRecord {
    pub source: Source,
    pub arrival: Arrival,
    /// The frame it was queued for (`None` when it was not queued).
    pub due: Option<RenderFrame>,
    pub message: Message,
    pub outcome: EventOutcome,
}

/// Ingress ports playback registers at once: ports `0..MAX_PORTS`, plus [`Source::UI`] and
/// [`Source::CLIP`]. Events from other ports are refused ([`EventOutcome::PortOutOfRange`]).
pub const MAX_PORTS: usize = 64;

/// Queue slots per instrument.
pub const QUEUE_CAP: usize = 512;
/// Notes an instrument holds open at once (one per source, channel and key or sender id).
pub const MAX_OPEN: usize = 224;
/// Pedals held down at once (one per source, channel and pedal kind): a product limit.
pub const MAX_PEDALS: usize = 32;
/// Queue slots for everything but releases: the rest are reserved, one per open entry.
const NON_RELEASE_CAP: usize = QUEUE_CAP - MAX_OPEN - MAX_PEDALS;
/// Voice ids are 31-bit (the plug-in formats' `int32`, never negative).
const ID_MASK: u32 = 0x7FFF_FFFF;

/// What a queued entry does at delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// A note-on for note entry `n`.
    On(u8),
    /// The release of note entry `n` (reserved).
    Off(u8),
    /// A pedal-down for pedal entry `n`.
    Down(u8),
    /// The release of pedal entry `n` (reserved).
    Up(u8),
    /// CC 121 after its source's pedals were released: reset the other controllers.
    Reset,
    /// Any other message, delivered as it is.
    Plain,
}

impl Action {
    const fn is_release(self) -> bool {
        matches!(self, Action::Off(_) | Action::Up(_))
    }
}

#[derive(Clone, Copy, Debug)]
struct Queued {
    due: u64,
    source: Source,
    transport: Option<TransportEpoch>,
    message: Message,
    action: Action,
}

/// One open note: from the admission of its first note-on until its release is delivered.
#[derive(Clone, Copy, Debug)]
struct NoteEntry {
    used: bool,
    source: Source,
    key: u8,
    sender: NoteId,
    /// The voice id the plug-in knows it by, `-1` while it is not sounding.
    voice: i32,
    /// Note-ons queued and not yet delivered.
    ons: u16,
    release_queued: bool,
    /// Due frame of its latest queued note-on: its release is never ordered before it.
    last_on: u64,
}

const NO_NOTE: NoteEntry =
    NoteEntry { used: false, source: CLIP_SOURCE, key: 0, sender: NoteId::NONE, voice: -1, ons: 0, release_queued: false, last_on: 0 };

impl NoteEntry {
    /// Matching includes the whole source, then the sender id when there is one, else the key.
    fn matches(&self, source: &Source, key: u8, sender: NoteId) -> bool {
        self.used && self.source == *source && if sender != NoteId::NONE { self.sender == sender } else { self.key == key }
    }
}

/// One pedal held by one source on one channel.
#[derive(Clone, Copy, Debug)]
struct PedalEntry {
    used: bool,
    source: Source,
    cc: u8,
    /// Its down was delivered: it counts as a holder.
    down: bool,
    /// Pedal-downs queued and not yet delivered.
    downs: u16,
    release_queued: bool,
    last_down: u64,
}

const NO_PEDAL: PedalEntry = PedalEntry { used: false, source: CLIP_SOURCE, cc: 0, down: false, downs: 0, release_queued: false, last_down: 0 };

/// Index of a pedal kind in the holder counts.
fn pedal_kind(cc: u8) -> usize {
    match cc {
        CC_SUSTAIN => 0,
        CC_SOSTENUTO => 1,
        _ => 2,
    }
}

const PEDALS: [u8; 3] = [CC_SUSTAIN, CC_SOSTENUTO, soundcraft_dsp::event::CC_HOLD2];

/// The last source to move a channel's bend or pressure, and whether it left it neutral.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Writer {
    port: u16,
    generation: u64,
    neutral: bool,
}

/// An instrument slot's event state (plan mode): its queue, its open notes and pedals, and its
/// delivery counters. Prepared off the audio thread by the compiler; never resized.
pub(crate) struct InstrumentEvents {
    /// Sorted by due frame, then admission order. Capacity [`QUEUE_CAP`].
    queue: Vec<Queued>,
    non_release: usize,
    notes: [NoteEntry; MAX_OPEN],
    pedals: [PedalEntry; MAX_PEDALS],
    /// Per channel and pedal kind: the sources whose down was delivered.
    holders: [[u8; 3]; 16],
    /// Per channel: bend, then channel pressure.
    writers: [[Option<Writer>; 2]; 16],
    next_voice: u32,
    status: EventStatus,
}

/// How an admission went, before the engine's own annotations.
enum Admit {
    Queued,
    Coalesced,
    Unmatched,
    Shed(Shed),
}

impl InstrumentEvents {
    pub(crate) fn new() -> Box<Self> {
        Box::new(InstrumentEvents {
            queue: Vec::with_capacity(QUEUE_CAP),
            non_release: 0,
            notes: [NO_NOTE; MAX_OPEN],
            pedals: [NO_PEDAL; MAX_PEDALS],
            holders: [[0; 3]; 16],
            writers: [[None; 2]; 16],
            next_voice: 0,
            status: EventStatus::default(),
        })
    }

    /// Queued (non-release, release) entries.
    fn queued_counts(&self) -> [u64; 2] {
        let r = self.queue.iter().filter(|q| q.action.is_release()).count();
        [(self.queue.len() - r) as u64, r as u64]
    }

    /// Inserts after every entry due at or before it, so equal frames keep admission order. A
    /// release always finds room (see the module docs); were the invariant ever broken, the
    /// queued non-releases are dropped first.
    fn push(&mut self, q: Queued) -> bool {
        if q.action.is_release() {
            if self.queue.len() >= QUEUE_CAP {
                self.status.release_collapses = self.status.release_collapses.saturating_add(1);
                self.cancel(|e| !e.action.is_release(), |_| {});
            }
            if self.queue.len() >= QUEUE_CAP {
                self.status.dropped_releases = self.status.dropped_releases.saturating_add(1);
                return false;
            }
        } else {
            if self.non_release >= NON_RELEASE_CAP || self.queue.len() >= QUEUE_CAP {
                return false;
            }
            self.non_release += 1;
        }
        let at = self.queue.partition_point(|e| e.due <= q.due);
        self.queue.insert(at, q);
        true
    }

    /// Removes the queued entries `which` selects, undoing what they held: a cancelled note-on or
    /// pedal-down frees an entry nothing else keeps. `each` sees every removed entry.
    fn cancel(&mut self, which: impl Fn(&Queued) -> bool, mut each: impl FnMut(&Queued)) {
        let (notes, pedals, non_release) = (&mut self.notes, &mut self.pedals, &mut self.non_release);
        self.queue.retain(|q| {
            if !which(q) {
                return true;
            }
            each(q);
            match q.action {
                Action::On(n) => {
                    if let Some(e) = notes.get_mut(usize::from(n)) {
                        e.ons = e.ons.saturating_sub(1);
                        if e.ons == 0 && e.voice < 0 && !e.release_queued {
                            *e = NO_NOTE;
                        }
                    }
                }
                Action::Down(n) => {
                    if let Some(p) = pedals.get_mut(usize::from(n)) {
                        p.downs = p.downs.saturating_sub(1);
                        if p.downs == 0 && !p.down && !p.release_queued {
                            *p = NO_PEDAL;
                        }
                    }
                }
                // A cancelled release leaves its entry open: the caller releases it.
                Action::Off(_) | Action::Up(_) | Action::Reset | Action::Plain => {}
            }
            if !q.action.is_release() {
                *non_release = non_release.saturating_sub(1);
            }
            false
        });
    }

    fn free_note(&mut self) -> Option<usize> {
        self.notes.iter().position(|e| !e.used)
    }

    fn free_pedal(&mut self) -> Option<usize> {
        self.pedals.iter().position(|p| !p.used)
    }

    /// Admits one event due at `due` (already validated and clamped by the engine).
    fn admit(&mut self, due: u64, source: Source, transport: Option<TransportEpoch>, message: Message, st: &mut EventStatus) -> Admit {
        let q = |action| Queued { due, source, transport, message, action };
        let room = self.non_release < NON_RELEASE_CAP;
        match message {
            Message::NoteOn { key, id, .. } => {
                let open = self.notes.iter().position(|e| e.matches(&source, key, id) && !e.release_queued);
                if !room {
                    return Admit::Shed(Shed::QueueFull);
                }
                let n = match open {
                    Some(n) => n,
                    None => {
                        let Some(n) = self.free_note() else { return Admit::Shed(Shed::NotesFull) };
                        if let Some(e) = self.notes.get_mut(n) {
                            *e = NoteEntry { used: true, source, key, sender: id, voice: -1, ons: 0, release_queued: false, last_on: due };
                        }
                        n
                    }
                };
                if !self.push(q(Action::On(n as u8))) {
                    if open.is_none()
                        && let Some(e) = self.notes.get_mut(n)
                    {
                        *e = NO_NOTE;
                    }
                    return Admit::Shed(Shed::QueueFull);
                }
                if let Some(e) = self.notes.get_mut(n) {
                    e.ons = e.ons.saturating_add(1);
                    e.last_on = e.last_on.max(due);
                }
                Admit::Queued
            }
            Message::NoteOff { key, id, .. } => {
                let Some(n) = self.notes.iter().position(|e| e.matches(&source, key, id) && !e.release_queued) else {
                    let dup = self.notes.iter().any(|e| e.matches(&source, key, id));
                    return if dup { Admit::Coalesced } else { Admit::Unmatched };
                };
                self.release_note(n, due, source, transport, message, st);
                Admit::Queued
            }
            Message::Control { cc, value } if is_pedal(cc) && value >= 64 => {
                if !room {
                    return Admit::Shed(Shed::QueueFull);
                }
                let open = self.pedals.iter().position(|p| p.used && p.source == source && p.cc == cc && !p.release_queued);
                let n = match open {
                    Some(n) => n,
                    None => {
                        let Some(n) = self.free_pedal() else { return Admit::Shed(Shed::PedalsFull) };
                        if let Some(p) = self.pedals.get_mut(n) {
                            *p = PedalEntry { used: true, source, cc, down: false, downs: 0, release_queued: false, last_down: due };
                        }
                        n
                    }
                };
                if !self.push(q(Action::Down(n as u8))) {
                    if open.is_none()
                        && let Some(p) = self.pedals.get_mut(n)
                    {
                        *p = NO_PEDAL;
                    }
                    return Admit::Shed(Shed::QueueFull);
                }
                if let Some(p) = self.pedals.get_mut(n) {
                    p.downs = p.downs.saturating_add(1);
                    p.last_down = p.last_down.max(due);
                }
                Admit::Queued
            }
            Message::Control { cc, .. } if is_pedal(cc) => {
                let Some(n) = self.pedals.iter().position(|p| p.used && p.source == source && p.cc == cc && !p.release_queued) else {
                    let dup = self.pedals.iter().any(|p| p.used && p.source == source && p.cc == cc);
                    return if dup { Admit::Coalesced } else { Admit::Unmatched };
                };
                self.release_pedal(n, due, source, transport, message, st);
                Admit::Queued
            }
            Message::Control { cc: CC_ALL_SOUND_OFF | CC_ALL_NOTES_OFF, .. } => {
                // The source's own notes on this channel, as releases already reserved.
                let mut any = false;
                for n in 0..MAX_OPEN {
                    let Some(e) = self.notes.get(n) else { break };
                    if e.used && e.source == source && !e.release_queued {
                        let off = Message::NoteOff { key: e.key, velocity: 0, id: e.sender };
                        self.release_note(n, due, source, transport, off, st);
                        any = true;
                    }
                }
                if any {
                    Admit::Queued
                } else if self.notes.iter().any(|e| e.used && e.source == source) {
                    Admit::Coalesced
                } else {
                    Admit::Unmatched
                }
            }
            Message::Control { cc: CC_RESET_CONTROLLERS, value } => {
                // The source's pedals are released (reserved); the other controllers reset with
                // one non-release entry.
                let mut any = false;
                for n in 0..MAX_PEDALS {
                    let Some(p) = self.pedals.get(n) else { break };
                    if p.used && p.source == source && !p.release_queued {
                        let up = Message::Control { cc: p.cc, value: 0 };
                        self.release_pedal(n, due, source, transport, up, st);
                        any = true;
                    }
                }
                let reset = Queued { message: Message::Control { cc: CC_RESET_CONTROLLERS, value }, ..q(Action::Reset) };
                if room && self.push(reset) {
                    Admit::Queued
                } else if any {
                    st.shed.control = st.shed.control.saturating_add(1);
                    Admit::Queued
                } else {
                    Admit::Shed(Shed::QueueFull)
                }
            }
            _ => {
                // A controller value for a frame that already has one from this source replaces it.
                let from = self.queue.partition_point(|e| e.due < due);
                let same = self.queue.get(from..).unwrap_or(&[]).iter().take_while(|e| e.due == due).position(|e| {
                    e.action == Action::Plain && e.source == source && e.transport == transport && same_controller(&e.message, &message)
                });
                if let Some(e) = same.and_then(|i| self.queue.get_mut(from + i)) {
                    e.message = message;
                    return Admit::Coalesced;
                }
                if self.push(q(Action::Plain)) { Admit::Queued } else { Admit::Shed(Shed::QueueFull) }
            }
        }
    }

    /// Queues note entry `n`'s release, never before its last note-on.
    fn release_note(&mut self, n: usize, due: u64, source: Source, transport: Option<TransportEpoch>, message: Message, st: &mut EventStatus) {
        let Some(e) = self.notes.get_mut(n) else { return };
        e.release_queued = true;
        let due = if due < e.last_on {
            st.reordered = st.reordered.saturating_add(1);
            e.last_on
        } else {
            due
        };
        self.push(Queued { due, source, transport, message, action: Action::Off(n as u8) });
    }

    /// Queues pedal entry `n`'s release, never before its last pedal-down.
    fn release_pedal(&mut self, n: usize, due: u64, source: Source, transport: Option<TransportEpoch>, message: Message, st: &mut EventStatus) {
        let Some(p) = self.pedals.get_mut(n) else { return };
        p.release_queued = true;
        let due = if due < p.last_down {
            st.reordered = st.reordered.saturating_add(1);
            p.last_down
        } else {
            due
        };
        self.push(Queued { due, source, transport, message, action: Action::Up(n as u8) });
    }

    /// A voice id no open note uses (at most `MAX_OPEN + 1` probes).
    fn voice_id(&mut self) -> i32 {
        loop {
            let id = (self.next_voice & ID_MASK) as i32;
            self.next_voice = self.next_voice.wrapping_add(1) & ID_MASK;
            if !self.notes.iter().any(|e| e.used && e.voice == id) {
                return id;
            }
        }
    }

    /// Hands one event to the plug-in, counting a refusal.
    fn send(&mut self, plugin: &mut dyn Plugin, offset: u32, channel: u8, note_id: i32, message: Message) -> bool {
        let ev = PluginEvent { channel, note_id, message };
        let ok = plugin.event(offset, &ev);
        if !ok {
            let i = usize::from(ev.is_release());
            if let Some(n) = self.status.host_refused.get_mut(i) {
                *n = n.saturating_add(1);
            }
        }
        ok
    }

    /// Note-on for entry `n`: a re-strike releases the voice it had first, at the same offset.
    fn deliver_on(&mut self, plugin: &mut dyn Plugin, offset: u32, n: usize, velocity: u8) {
        let Some(e) = self.notes.get(n).copied() else { return };
        if e.voice >= 0 {
            self.status.restrikes = self.status.restrikes.saturating_add(1);
            self.send(plugin, offset, e.source.channel, e.voice, Message::NoteOff { key: e.key, velocity: 0, id: e.sender });
        }
        let voice = self.voice_id();
        let ok = self.send(plugin, offset, e.source.channel, voice, Message::NoteOn { key: e.key, velocity, id: e.sender });
        if let Some(e) = self.notes.get_mut(n) {
            e.voice = if ok { voice } else { -1 };
        }
    }

    /// Releases entry `n`'s voice (when it sounds) and frees the entry.
    fn deliver_off(&mut self, plugin: &mut dyn Plugin, offset: u32, n: usize, velocity: u8) {
        let Some(e) = self.notes.get(n).copied() else { return };
        if e.voice >= 0 {
            self.send(plugin, offset, e.source.channel, e.voice, Message::NoteOff { key: e.key, velocity, id: e.sender });
        }
        if let Some(e) = self.notes.get_mut(n) {
            *e = NO_NOTE;
        }
    }

    /// A pedal edge through the OR across sources: the plug-in sees down on 0 → 1 holders and up
    /// on 1 → 0; other edges are absorbed.
    fn pedal_edge(&mut self, plugin: &mut dyn Plugin, offset: u32, n: usize, down: bool, value: u8) {
        let Some(p) = self.pedals.get(n).copied() else { return };
        let ch = usize::from(p.source.channel & 0x0F);
        let Some(count) = self.holders.get_mut(ch).and_then(|h| h.get_mut(pedal_kind(p.cc))) else { return };
        let edge = if down && !p.down {
            *count = count.saturating_add(1);
            *count == 1
        } else if !down && p.down {
            *count = count.saturating_sub(1);
            *count == 0
        } else {
            false
        };
        if edge {
            self.send(plugin, offset, p.source.channel, PluginEvent::NO_ID, Message::Control { cc: p.cc, value });
        } else {
            self.status.absorbed_pedal_edges = self.status.absorbed_pedal_edges.saturating_add(1);
        }
        if let Some(p) = self.pedals.get_mut(n) {
            if down {
                p.down = true;
            } else {
                *p = NO_PEDAL;
            }
        }
    }

    /// Reset all controllers on `channel`, then the pedals other sources still hold, down again.
    fn deliver_reset(&mut self, plugin: &mut dyn Plugin, offset: u32, channel: u8, value: u8) {
        self.send(plugin, offset, channel, PluginEvent::NO_ID, Message::Control { cc: CC_RESET_CONTROLLERS, value });
        let held = self.holders.get(usize::from(channel & 0x0F)).copied().unwrap_or_default();
        for (cc, h) in PEDALS.into_iter().zip(held) {
            if h > 0 {
                self.send(plugin, offset, channel, PluginEvent::NO_ID, Message::Control { cc, value: 127 });
            }
        }
    }

    /// Any other message: poly pressure goes to the voice of the source's note; bend and
    /// channel pressure remember their writer.
    fn deliver_plain(&mut self, plugin: &mut dyn Plugin, offset: u32, source: Source, message: Message) {
        let mut voice = PluginEvent::NO_ID;
        match message {
            Message::PolyPressure { key, id, .. } => {
                voice = self.notes.iter().find(|e| e.matches(&source, key, id)).map_or(PluginEvent::NO_ID, |e| e.voice);
            }
            Message::PitchBend { value } => self.write(source, 0, value == 8192),
            Message::ChannelPressure { value } => self.write(source, 1, value == 0),
            _ => {}
        }
        self.send(plugin, offset, source.channel, voice, message);
    }

    fn write(&mut self, source: Source, what: usize, neutral: bool) {
        if let Some(w) = self.writers.get_mut(usize::from(source.channel & 0x0F)).and_then(|w| w.get_mut(what)) {
            *w = Some(Writer { port: source.port, generation: source.generation, neutral });
        }
    }

    /// Delivers the entries due before `start + frames` at `due − start` (late ones at 0).
    pub(crate) fn deliver(&mut self, plugin: &mut dyn Plugin, start: u64, frames: usize) {
        let end = start.saturating_add(frames as u64);
        let k = self.queue.partition_point(|q| q.due < end);
        let last = u32::try_from(frames.saturating_sub(1)).unwrap_or(u32::MAX);
        for i in 0..k {
            let Some(q) = self.queue.get(i).copied() else { break };
            let offset = u32::try_from(q.due.saturating_sub(start)).unwrap_or(u32::MAX).min(last);
            match (q.action, q.message) {
                (Action::On(n), Message::NoteOn { velocity, .. }) => {
                    if let Some(e) = self.notes.get_mut(usize::from(n)) {
                        e.ons = e.ons.saturating_sub(1);
                    }
                    self.deliver_on(plugin, offset, usize::from(n), velocity);
                }
                (Action::Off(n), Message::NoteOff { velocity, .. }) => self.deliver_off(plugin, offset, usize::from(n), velocity),
                (Action::Off(n), _) => self.deliver_off(plugin, offset, usize::from(n), 0),
                (Action::Down(n), Message::Control { value, .. }) => {
                    if let Some(p) = self.pedals.get_mut(usize::from(n)) {
                        p.downs = p.downs.saturating_sub(1);
                    }
                    self.pedal_edge(plugin, offset, usize::from(n), true, value);
                }
                (Action::Up(n), Message::Control { value, .. }) => self.pedal_edge(plugin, offset, usize::from(n), false, value),
                (Action::Up(n), _) => self.pedal_edge(plugin, offset, usize::from(n), false, 0),
                (Action::Reset, Message::Control { value, .. }) => self.deliver_reset(plugin, offset, q.source.channel, value),
                (_, m) => self.deliver_plain(plugin, offset, q.source, m),
            }
        }
        let released = self.queue.get(..k).map_or(0, |d| d.iter().filter(|q| q.action.is_release()).count());
        self.non_release = self.non_release.saturating_sub(k - released);
        self.queue.drain(..k);
    }

    /// Releases everything `port` / `generation` holds now (offset 0 of the next block) and
    /// cancels its queued events: notes off, pedals up where it was the last holder, bend and
    /// pressure back to neutral where it was the last writer. Returns the work done.
    pub(crate) fn release_sender(&mut self, plugin: &mut dyn Plugin, port: u16, generation: u64) -> u64 {
        let mut done = 0u64;
        self.cancel(|q| q.source.sender_is(port, generation), |_| done += 1);
        for n in 0..MAX_OPEN {
            let Some(e) = self.notes.get(n) else { break };
            if e.used && e.source.sender_is(port, generation) {
                self.deliver_off(plugin, 0, n, 0);
                done += 1;
            }
        }
        for n in 0..MAX_PEDALS {
            let Some(p) = self.pedals.get(n) else { break };
            if p.used && p.source.sender_is(port, generation) {
                if p.down {
                    self.pedal_edge(plugin, 0, n, false, 0);
                } else if let Some(p) = self.pedals.get_mut(n) {
                    *p = NO_PEDAL;
                }
                done += 1;
            }
        }
        for ch in 0..16u8 {
            for (what, neutral) in [(0usize, Message::PitchBend { value: 8192 }), (1, Message::ChannelPressure { value: 0 })] {
                let Some(w) = self.writers.get_mut(usize::from(ch)).and_then(|w| w.get_mut(what)) else { continue };
                if let Some(x) = *w
                    && x.port == port
                    && x.generation == generation
                {
                    *w = None;
                    if !x.neutral {
                        self.send(plugin, 0, ch, PluginEvent::NO_ID, neutral);
                    }
                }
            }
        }
        done
    }

    /// The transport moved to `current`: queued events derived from another transport are
    /// cancelled; their releases stay (a sounding note still needs its off).
    pub(crate) fn transport_changed(&mut self, current: TransportEpoch) {
        let mut n = 0u64;
        let stale = |t: Option<TransportEpoch>| t.is_some_and(|t| t != current);
        self.cancel(|q| !q.action.is_release() && stale(q.transport), |_| n += 1);
        for q in self.queue.iter_mut().filter(|q| stale(q.transport)) {
            q.transport = None;
        }
        self.status.cancelled_transport = self.status.cancelled_transport.saturating_add(n);
    }

    /// Stop or panic: every sounding note and held pedal is released now and the queue is
    /// emptied (the caller also tells the plug-in to release everything).
    pub(crate) fn release_all(&mut self, plugin: &mut dyn Plugin) {
        self.queue.clear();
        self.non_release = 0;
        for n in 0..MAX_OPEN {
            if self.notes.get(n).is_some_and(|e| e.used) {
                self.deliver_off(plugin, 0, n, 0);
            }
        }
        for n in 0..MAX_PEDALS {
            if self.pedals.get(n).is_some_and(|p| p.used && p.down) {
                self.pedal_edge(plugin, 0, n, false, 0);
            }
        }
        self.pedals = [NO_PEDAL; MAX_PEDALS];
        self.holders = [[0; 3]; 16];
        self.writers = [[None; 2]; 16];
    }

    /// The sounding notes: source, key, voice id.
    #[cfg(test)]
    pub(crate) fn sounding(&self) -> impl Iterator<Item = (Source, u8, i32)> + '_ {
        self.notes.iter().filter(|e| e.used && e.voice >= 0).map(|e| (e.source, e.key, e.voice))
    }

    /// Anything queued, sounding or held.
    fn busy(&self) -> bool {
        !self.queue.is_empty() || self.notes.iter().any(|e| e.used) || self.pedals.iter().any(|p| p.used)
    }

    // ---- clips: delivered at once, through the same table, as `Source::CLIP` ----

    /// A clip note-on. A full table sheds it (counted): it is not delivered untracked.
    pub(crate) fn clip_on(&mut self, plugin: &mut dyn Plugin, offset: u32, key: u8, velocity: u8) {
        let n = match self.notes.iter().position(|e| e.matches(&CLIP_SOURCE, key, NoteId::NONE)) {
            Some(n) => n,
            None => {
                let Some(n) = self.free_note() else {
                    self.status.shed.note_on = self.status.shed.note_on.saturating_add(1);
                    return;
                };
                if let Some(e) = self.notes.get_mut(n) {
                    *e = NoteEntry {
                        used: true,
                        source: CLIP_SOURCE,
                        key,
                        sender: NoteId::NONE,
                        voice: -1,
                        ons: 0,
                        release_queued: false,
                        last_on: 0,
                    };
                }
                n
            }
        };
        self.deliver_on(plugin, offset, n, velocity.clamp(1, 127));
    }

    /// A clip note-off (nothing when the clip's note is not open: shed, or released at a seek).
    pub(crate) fn clip_off(&mut self, plugin: &mut dyn Plugin, offset: u32, key: u8) {
        if let Some(n) = self.notes.iter().position(|e| e.matches(&CLIP_SOURCE, key, NoteId::NONE)) {
            self.deliver_off(plugin, offset, n, 0);
        }
    }

    /// A clip controller (MIDI 1.0 bytes): pedals through the per-source OR, reset all
    /// controllers keeping other sources' pedals, the rest as is.
    pub(crate) fn clip_ctrl(&mut self, plugin: &mut dyn Plugin, offset: u32, msg: [u8; 3]) {
        let Some(message) = message_from_midi1(msg) else { return };
        let source = Source { channel: msg[0] & 0x0F, ..CLIP_SOURCE };
        match message {
            Message::Control { cc, value } if is_pedal(cc) => {
                let open = self.pedals.iter().position(|p| p.used && p.source == source && p.cc == cc);
                match (open, value >= 64) {
                    (Some(n), down) => self.pedal_edge(plugin, offset, n, down, value),
                    (None, true) => {
                        let Some(n) = self.free_pedal() else {
                            self.status.shed.pedal_down = self.status.shed.pedal_down.saturating_add(1);
                            return;
                        };
                        if let Some(p) = self.pedals.get_mut(n) {
                            *p = PedalEntry { used: true, source, cc, down: false, downs: 0, release_queued: false, last_down: 0 };
                        }
                        self.pedal_edge(plugin, offset, n, true, value);
                    }
                    (None, false) => {}
                }
            }
            Message::Control { cc: CC_RESET_CONTROLLERS, value } => {
                for n in 0..MAX_PEDALS {
                    if self.pedals.get(n).is_some_and(|p| p.used && p.source == source) {
                        self.pedal_edge(plugin, offset, n, false, 0);
                    }
                }
                self.deliver_reset(plugin, offset, source.channel, value);
            }
            m => self.deliver_plain(plugin, offset, source, m),
        }
    }

    /// Seek or loop wrap: the clips' notes and pedals are released (live sources keep sounding).
    pub(crate) fn release_clips(&mut self, plugin: &mut dyn Plugin) {
        self.release_sender(plugin, Source::CLIP, 0);
    }
}

/// Two messages set the same controller (and replace each other at one frame).
fn same_controller(a: &Message, b: &Message) -> bool {
    match (a, b) {
        (Message::Control { cc: x, .. }, Message::Control { cc: y, .. }) => x == y,
        (Message::Control14 { msb: x, .. }, Message::Control14 { msb: y, .. }) => x == y,
        (Message::PolyPressure { key: x, id: i, .. }, Message::PolyPressure { key: y, id: j, .. }) => x == y && i == j,
        (Message::PitchBend { .. }, Message::PitchBend { .. }) | (Message::ChannelPressure { .. }, Message::ChannelPressure { .. }) => true,
        _ => false,
    }
}

/// A MIDI 1.0 channel message as a [`Message`] (`None` for anything else).
pub(crate) fn message_from_midi1(msg: [u8; 3]) -> Option<Message> {
    let [status, d1, d2] = msg;
    let (d1, d2) = (d1 & 0x7F, d2 & 0x7F);
    Some(match status & 0xF0 {
        0x80 => Message::NoteOff { key: d1, velocity: d2, id: NoteId::NONE },
        0x90 if d2 == 0 => Message::NoteOff { key: d1, velocity: 0, id: NoteId::NONE },
        0x90 => Message::NoteOn { key: d1, velocity: d2, id: NoteId::NONE },
        0xA0 => Message::PolyPressure { key: d1, value: d2, id: NoteId::NONE },
        0xB0 => Message::Control { cc: d1, value: d2 },
        0xC0 => Message::Program { program: d1 },
        0xD0 => Message::ChannelPressure { value: d1 },
        0xE0 => Message::PitchBend { value: u16::from(d2) << 7 | u16::from(d1) },
        _ => return None,
    })
}

/// The engine's side: epoch, limits, the render clock, the transport and the counters.
pub(crate) struct EngineEvents {
    /// `None`: the engine takes no realtime events (offline and legacy engines).
    pub(crate) epoch: Option<EngineEpoch>,
    pub(crate) limits: EventLimits,
    /// The render frame of the next block's first sample.
    pub(crate) next_frame: u64,
    pub(crate) transport: Option<TransportEpoch>,
    pub(crate) status: EventStatus,
    tap: Vec<TapRecord>,
    /// Per port slot (see [`MAX_PORTS`]): one past the highest revoked generation (0: none).
    revoked: [u64; MAX_PORTS + 2],
}

impl EngineEvents {
    pub(crate) fn new(epoch: Option<EngineEpoch>, limits: EventLimits) -> Self {
        EngineEvents { epoch, limits, next_frame: 0, transport: None, status: EventStatus::default(), tap: Vec::new(), revoked: [0; MAX_PORTS + 2] }
    }
}

/// The instrument slot an event names, when it is a live, event-taking instrument.
fn instrument_of<'a>(
    strips: &'a mut [Strip],
    tracks: &[(soundcraft_model::TrackId, u16)],
    target: &SlotBinding,
) -> Result<&'a mut PluginSlot, EventOutcome> {
    let (track, slot, generation) = *target;
    let i = tracks.binary_search_by_key(&track, |(t, _)| *t).ok().and_then(|i| tracks.get(i)).map(|(_, s)| usize::from(*s));
    let inst = i.and_then(|i| strips.get_mut(i)).and_then(|st| st.instrument.as_mut()).filter(|_| slot == INSTRUMENT_SLOT);
    let Some(inst) = inst else { return Err(EventOutcome::NoInstrument) };
    if generation == SlotGen::VACANT || inst.generation != generation {
        return Err(EventOutcome::StaleBinding);
    }
    if inst.failed || inst.events.is_none() {
        return Err(EventOutcome::NoInstrument);
    }
    Ok(inst)
}

impl MixEngine {
    /// A realtime engine ([`MixEngine::realtime`]) that takes instrument events stamped with
    /// `epoch` ([`MixEngine::queue_event`]). Playback creates the epoch before construction and
    /// gives every engine (device or rate) incarnation a new one. Engines made any other way
    /// refuse every event with [`EventOutcome::StaleEngine`].
    #[must_use]
    pub fn realtime_with_events(target: super::RenderTarget, epoch: EngineEpoch, limits: EventLimits) -> Self {
        let mut eng = MixEngine::realtime(target);
        eng.events = EngineEvents::new(Some(epoch), limits);
        eng
    }

    /// Audio thread, between blocks: validates `ev` (engine epoch, then binding, then transport
    /// epoch) and queues it on its instrument. Never allocates, locks or logs.
    pub fn queue_event(&mut self, ev: &InstrumentEvent) -> EventOutcome {
        let (outcome, due) = self.admit(ev);
        let st = &mut self.events.status;
        let bump = |n: &mut u64| *n = n.saturating_add(1);
        match outcome {
            EventOutcome::Queued => bump(&mut st.queued),
            EventOutcome::QueuedLate { frames } => {
                bump(&mut st.queued);
                bump(&mut st.late);
                st.max_late_frames = st.max_late_frames.max(frames);
                let b = (32 - frames.leading_zeros()).min(15) as usize;
                if let Some(n) = st.lateness_log2.get_mut(b) {
                    *n = n.saturating_add(1);
                }
            }
            EventOutcome::ReleaseAdvanced { .. } => {
                bump(&mut st.queued);
                bump(&mut st.releases_advanced);
            }
            EventOutcome::Coalesced => bump(&mut st.coalesced),
            EventOutcome::StaleEngine => bump(&mut st.refused_stale[0]),
            EventOutcome::RevokedSource | EventOutcome::PortOutOfRange => bump(&mut st.refused_revoked),
            EventOutcome::StaleBinding => bump(&mut st.refused_stale[1]),
            EventOutcome::StaleTransport => bump(&mut st.refused_stale[2]),
            EventOutcome::NoInstrument => bump(&mut st.refused_no_instrument),
            EventOutcome::TooEarly => bump(&mut st.refused_too_early),
            EventOutcome::Unmatched => bump(&mut st.unmatched),
            EventOutcome::Refused(_) => st.shed(&ev.message, ev.target),
            EventOutcome::Faulted => {}
        }
        if !matches!(outcome, EventOutcome::Queued | EventOutcome::QueuedLate { .. } | EventOutcome::ReleaseAdvanced { .. } | EventOutcome::Coalesced)
        {
            st.affected = Some(ev.target);
        }
        if self.events.tap.len() < self.events.tap.capacity() {
            let due = due.filter(|_| outcome.accepted()).map(RenderFrame);
            self.events.tap.push(TapRecord { source: ev.source, arrival: ev.arrival, due, message: ev.message, outcome });
        }
        outcome
    }

    /// The outcome and the frame it was (or would have been) queued for. Order: engine epoch,
    /// port revocation, plan, binding, then transport (non-releases only).
    fn admit(&mut self, ev: &InstrumentEvent) -> (EventOutcome, Option<u64>) {
        if self.events.epoch != Some(ev.engine) {
            return (EventOutcome::StaleEngine, None);
        }
        // Port generations only increase: one at or below the revoked one is gone for good.
        let Some(port) = Source::port_slot(ev.source.port) else { return (EventOutcome::PortOutOfRange, None) };
        if self.events.revoked.get(port).is_some_and(|&r| ev.source.generation < r) {
            return (EventOutcome::RevokedSource, None);
        }
        let Some(plan) = self.plan.as_deref_mut() else { return (EventOutcome::StaleEngine, None) };
        if plan.faulted {
            return (EventOutcome::Faulted, None);
        }
        let inst = match instrument_of(&mut plan.tables.strips, &plan.tables.tracks, &ev.target) {
            Ok(i) => i,
            Err(o) => return (o, None),
        };
        let release = ev.message.is_release() || matches!(ev.message, Message::NoteOn { velocity: 0, .. });
        // A release is never refused for transport staleness: its note or pedal may already sound.
        // It is no longer the old transport's either (a later change does not cancel it).
        let mut transport = ev.transport;
        if transport.is_some_and(|t| Some(t) != self.events.transport) {
            if !release {
                return (EventOutcome::StaleTransport, None);
            }
            transport = None;
        }
        let next = self.events.next_frame;
        let horizon = next.saturating_add(u64::from(self.events.limits.max_future_frames));
        let (due, late, advanced) = match ev.due {
            Due::Next => (next, 0, 0),
            Due::Frame(RenderFrame(f)) if f < next => (next, next - f, 0),
            Due::Frame(RenderFrame(f)) if f > horizon => {
                if !release {
                    return (EventOutcome::TooEarly, Some(f));
                }
                (horizon, 0, f - horizon)
            }
            Due::Frame(RenderFrame(f)) => (f, 0, 0),
        };
        let Some(events) = inst.events.as_deref_mut() else { return (EventOutcome::NoInstrument, None) };
        let st = &mut self.events.status;
        let frames = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        // A note-on with velocity 0 is a note-off (ingress normally does this already).
        let message = match ev.message {
            Message::NoteOn { key, velocity: 0, id } => Message::NoteOff { key, velocity: 0, id },
            m => m,
        };
        let outcome = match events.admit(due, ev.source, transport, message, st) {
            Admit::Queued if advanced > 0 => EventOutcome::ReleaseAdvanced { frames: frames(advanced) },
            Admit::Queued if late > 0 => EventOutcome::QueuedLate { frames: frames(late) },
            Admit::Queued => EventOutcome::Queued,
            Admit::Coalesced => EventOutcome::Coalesced,
            Admit::Unmatched => EventOutcome::Unmatched,
            Admit::Shed(s) => EventOutcome::Refused(s),
        };
        (outcome, Some(due))
    }

    /// Audio thread, with every seek, start, stop and loop wrap: the current transport epoch.
    /// Queued events derived from another transport are cancelled (counted
    /// `cancelled_transport`); their releases are kept, and a release that arrives later with an
    /// old epoch is still admitted.
    pub fn set_transport_epoch(&mut self, epoch: TransportEpoch) {
        if self.events.transport == Some(epoch) {
            return;
        }
        self.events.transport = Some(epoch);
        for e in self.instrument_events() {
            e.transport_changed(epoch);
        }
    }

    /// Audio thread, when a port generation retires (unplug): cancels that source's queued
    /// events, releases its notes and pedals at offset 0 of the next block (a pedal goes up at
    /// the plug-in only when it was the last holder), and returns bend and pressure to neutral
    /// where it was the last writer. Bounded: instruments × (512 + 224 + 32). Counted `revoked`.
    ///
    /// The port stays revoked: a later event from this generation or an older one (one still in
    /// flight in playback's lane) is refused with [`EventOutcome::RevokedSource`] until a newer
    /// generation of the port appears.
    pub fn revoke_source(&mut self, port: u16, generation: u64) {
        let mut done = 0u64;
        if let Some(p) = self.plan.as_deref_mut() {
            for st in p.tables.strips.iter_mut() {
                if let Some(inst) = st.instrument.as_mut()
                    && let Some(e) = inst.events.as_deref_mut()
                {
                    done = done.saturating_add(e.release_sender(&mut *inst.plugin, port, generation));
                }
            }
        }
        self.events.status.revoked = self.events.status.revoked.saturating_add(done);
        if let Some(r) = Source::port_slot(port).and_then(|i| self.events.revoked.get_mut(i)) {
            *r = (*r).max(generation.saturating_add(1));
        }
    }

    /// The wake contract: true while any instrument still has something to deliver or release:
    /// queued events (future ones included), sounding notes, held pedals, or reserved releases.
    /// A stopped host keeps processing while this holds. Allocation-free; bounded by the
    /// instruments × (512 + 224 + 32) entries.
    ///
    /// It covers undelivered events, holds and reservations only. `false` does not mean the
    /// engine is quiet: release envelopes, one-shots and insert or delay tails still need
    /// processing after the last release is delivered; the caller decides when to idle (see
    /// [`Plugin::tail_samples`] and [`Plugin::process_status`]).
    #[must_use]
    pub fn has_pending_events(&self) -> bool {
        self.plan.as_deref().is_some_and(|p| {
            p.tables.strips.iter().filter_map(|st| st.instrument.as_ref()).filter_map(|i| i.events.as_deref()).any(InstrumentEvents::busy)
        })
    }

    /// The render frame of the next block's first sample.
    #[must_use]
    pub fn next_frame(&self) -> RenderFrame {
        RenderFrame(self.events.next_frame)
    }

    /// The event counters: the engine's and every instrument's.
    #[must_use]
    pub fn event_status(&self) -> EventStatus {
        let mut s = self.events.status;
        if let Some(p) = self.plan.as_deref() {
            for e in p.tables.strips.iter().filter_map(|st| st.instrument.as_ref()).filter_map(|i| i.events.as_deref()) {
                s.absorb(&e.status);
            }
        }
        s
    }

    /// Control thread: record the next `capacity` submitted events (G5 boundary tests). Replaces
    /// what was recorded.
    pub fn install_event_tap(&mut self, capacity: usize) {
        self.events.tap = Vec::with_capacity(capacity);
    }

    /// What the tap recorded, in submission order.
    #[must_use]
    pub fn event_tap(&self) -> &[TapRecord] {
        &self.events.tap
    }

    /// Every instrument's event state in the active plan.
    fn instrument_events(&mut self) -> impl Iterator<Item = &mut InstrumentEvents> {
        self.plan.iter_mut().flat_map(|p| p.tables.strips.iter_mut()).filter_map(|st| st.instrument.as_mut()).filter_map(|i| i.events.as_deref_mut())
    }

    /// After an adoption: the displaced instruments' queues retire with them (counted) and their
    /// counters fold into the engine's.
    pub(crate) fn retire_events(status: &mut EventStatus, displaced: &mut [Strip]) {
        for e in displaced.iter().filter_map(|st| st.instrument.as_ref()).filter_map(|i| i.events.as_deref()) {
            let [n, r] = e.queued_counts();
            status.absorb(&e.status);
            status.retired_with_instance[0] = status.retired_with_instance[0].saturating_add(n);
            status.retired_with_instance[1] = status.retired_with_instance[1].saturating_add(r);
        }
    }
}
