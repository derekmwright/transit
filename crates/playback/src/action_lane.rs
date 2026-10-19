//! Independent bounded transport and note lanes for one prepared-plan runtime.
//! Transport may wait for an exact compiled plan. Notes never wait for transport or a
//! plan prerequisite. Each class owns its own admission, return and callback budgets.
//! Copy payloads carry no heap owners; endpoints stay anchored on control until quiescence.

use crate::plan_bridge;
use soundcraft_mix::PlanGeneration;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const CAPACITY: usize = 256;
pub const NOTE_CAPACITY: usize = 256;
pub const CALLBACK_ACTIONS: usize = 32;
pub const CALLBACK_NOTES: usize = 32;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sequence(u64);
impl Sequence {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    On,
    Off,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Ordinary,
    NoteOn,
    NoteOff,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    PlanSuperseded,
    PlanRejected,
    Stopped,
    NotesReleased,
    Faulted,
    Shutdown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    Full { release_recovery: Option<Sequence> },
    SequenceExhausted,
    NotePrerequisite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Match this result to the admitted request by `sequence`, never by receipt order.
/// Independent note/transport lanes can complete in a different order.
pub struct Receipt<T: Copy> {
    pub sequence: Sequence,
    pub payload: T,
    pub outcome: Outcome,
}

/// Stop must stop transport AND release notes. A handler unwind permanently faults both lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<T: Copy> {
    Action { sequence: Sequence, payload: T },
    Stop { sequence: Sequence },
    AllNotesOff { sequence: Sequence },
}

#[derive(Clone, Copy)]
struct Entry<T: Copy> {
    sequence: Sequence,
    payload: T,
    class: Class,
    plan: Option<PlanGeneration>,
}
#[derive(Clone, Copy)]
struct Pending<T: Copy> {
    entry: Entry<T>,
    finished: Option<Outcome>,
}
impl<T: Copy> Pending<T> {
    fn receipt(self) -> Receipt<T> {
        Receipt { sequence: self.entry.sequence, payload: self.entry.payload, outcome: self.finished.unwrap_or(Outcome::Shutdown) }
    }
}

#[derive(Default)]
struct Shared {
    stop: AtomicU64,
    release: AtomicU64,
    rejected: AtomicU64,
    applied_stop: AtomicU64,
    applied_release: AtomicU64,
}

/// Minted only by the actual device adapter and shared across every conversion chunk.
pub struct Budget {
    remaining: usize,
    notes_remaining: usize,
    stop_serviced: bool,
    release_serviced: bool,
}
pub(super) fn callback_scope() -> Budget {
    Budget { remaining: CALLBACK_ACTIONS, notes_remaining: CALLBACK_NOTES, stop_serviced: false, release_serviced: false }
}
impl Budget {
    pub fn remaining(&self) -> usize {
        self.remaining
    }
    pub fn notes_remaining(&self) -> usize {
        self.notes_remaining
    }
    fn available(&self, notes: bool) -> usize {
        if notes { self.notes_remaining } else { self.remaining }
    }
    fn spend(&mut self, notes: bool) {
        if notes {
            self.notes_remaining -= 1;
        } else {
            self.remaining -= 1;
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    pub examined: usize,
    pub applied: usize,
    pub waiting_for_plan: bool,
    pub return_pending: bool,
    pub recovery_deferred: bool,
    pub faulted: bool,
}
impl Service {
    fn combine(&mut self, other: Self) {
        self.examined += other.examined;
        self.applied += other.applied;
        self.waiting_for_plan |= other.waiting_for_plan;
        self.return_pending |= other.return_pending;
        self.recovery_deferred |= other.recovery_deferred;
        self.faulted |= other.faulted;
    }
}

struct ControlLane<T: Copy> {
    send: rtrb::Producer<Entry<T>>,
    returned: rtrb::Consumer<Receipt<T>>,
    credits: usize,
}
struct AudioLane<T: Copy> {
    receive: rtrb::Consumer<Entry<T>>,
    returned: rtrb::Producer<Receipt<T>>,
    pending: Option<Pending<T>>,
}
fn lane<T: Copy>(capacity: usize) -> (ControlLane<T>, AudioLane<T>) {
    let (send, receive) = rtrb::RingBuffer::new(capacity);
    let (return_send, returned) = rtrb::RingBuffer::new(capacity);
    (ControlLane { send, returned, credits: capacity }, AudioLane { receive, returned: return_send, pending: None })
}
impl<T: Copy> ControlLane<T> {
    fn reclaim(&mut self) -> Option<Receipt<T>> {
        let receipt = self.returned.pop().ok()?;
        self.credits += 1;
        Some(receipt)
    }
}

pub struct Control<T: Copy> {
    actions: ControlLane<T>,
    notes: ControlLane<T>,
    shared: Arc<Shared>,
    next: u64,
    prefer_notes: bool,
}
pub struct Audio<T: Copy> {
    actions: AudioLane<T>,
    notes: AudioLane<T>,
    state: State,
    runtime: plan_bridge::RuntimeIdentity,
}
struct State {
    shared: Arc<Shared>,
    seen_stop: Sequence,
    seen_release: Sequence,
    faulted: bool,
}

/// Pair only with this bridge incarnation. A new device/rate bridge needs new action lanes.
pub fn pair<T: Copy>(bridge: &plan_bridge::Control) -> (Control<T>, Audio<T>) {
    let (actions, audio_actions) = lane(CAPACITY);
    let (notes, audio_notes) = lane(NOTE_CAPACITY);
    let shared = Arc::new(Shared::default());
    (
        Control { actions, notes, shared: Arc::clone(&shared), next: 0, prefer_notes: false },
        Audio {
            actions: audio_actions,
            notes: audio_notes,
            state: State { shared, seen_stop: Sequence(0), seen_release: Sequence(0), faulted: false },
            runtime: bridge.runtime_identity(),
        },
    )
}

impl<T: Copy> Control<T> {
    fn sequence(&mut self) -> Result<Sequence, SendError> {
        self.next = self.next.checked_add(1).ok_or(SendError::SequenceExhausted)?;
        Ok(Sequence(self.next))
    }
    pub fn send_action(&mut self, payload: T, plan: Option<PlanGeneration>) -> Result<Sequence, SendError> {
        self.send(payload, Class::Ordinary, plan)
    }
    /// Notes have no plan prerequisite. The payload's instrument/note identity is validated
    /// by the event handler; a future instrument must be refused, never block a release.
    pub fn send_note(&mut self, payload: T, kind: NoteKind) -> Result<Sequence, SendError> {
        self.send(
            payload,
            match kind {
                NoteKind::On => Class::NoteOn,
                NoteKind::Off => Class::NoteOff,
            },
            None,
        )
    }
    fn send(&mut self, payload: T, class: Class, plan: Option<PlanGeneration>) -> Result<Sequence, SendError> {
        let note = class != Class::Ordinary;
        if note && plan.is_some() {
            return Err(SendError::NotePrerequisite);
        }
        let credits = if note { self.notes.credits } else { self.actions.credits };
        if credits == 0 {
            let release_recovery = if class == Class::NoteOff { Some(self.all_notes_off()?) } else { None };
            return Err(SendError::Full { release_recovery });
        }
        let sequence = self.sequence()?;
        let entry = Entry { sequence, payload, class, plan };
        let lane = if note { &mut self.notes } else { &mut self.actions };
        if lane.send.push(entry).is_err() {
            let release_recovery = if class == Class::NoteOff {
                self.shared.release.store(sequence.0, Ordering::Release);
                Some(sequence)
            } else {
                None
            };
            return Err(SendError::Full { release_recovery });
        }
        lane.credits -= 1;
        Ok(sequence)
    }
    pub fn stop(&mut self) -> Result<Sequence, SendError> {
        let sequence = self.sequence()?;
        self.shared.stop.store(sequence.0, Ordering::Release);
        Ok(sequence)
    }
    pub fn all_notes_off(&mut self) -> Result<Sequence, SendError> {
        let sequence = self.sequence()?;
        self.shared.release.store(sequence.0, Ordering::Release);
        Ok(sequence)
    }
    /// This pair's strictly sequential N=1 plan protocol confirms an unreachable generation.
    pub fn reject_plan(&mut self, generation: PlanGeneration) {
        let previous = self.shared.rejected.load(Ordering::Relaxed);
        self.shared.rejected.store(previous.max(generation.get()), Ordering::Release);
    }
    /// Each class is FIFO; receipts from independent classes may arrive out of sequence.
    /// Round-robin reclamation prevents a busy transport lane starving note credits.
    pub fn reclaim(&mut self) -> Option<Receipt<T>> {
        self.prefer_notes = !self.prefer_notes;
        if self.prefer_notes {
            self.notes.reclaim().or_else(|| self.actions.reclaim())
        } else {
            self.actions.reclaim().or_else(|| self.notes.reclaim())
        }
    }
    pub fn credits(&self) -> usize {
        self.actions.credits
    }
    pub fn note_credits(&self) -> usize {
        self.notes.credits
    }
    pub fn recovery_progress(&self) -> (Sequence, Sequence) {
        (Sequence(self.shared.applied_stop.load(Ordering::Acquire)), Sequence(self.shared.applied_release.load(Ordering::Acquire)))
    }
    pub fn retire(mut self, mut audio: Audio<T>) -> Vec<Receipt<T>> {
        let mut receipts = Vec::with_capacity(CAPACITY + NOTE_CAPACITY);
        while let Some(receipt) = self.reclaim() {
            receipts.push(receipt);
        }
        for lane in [&mut audio.actions, &mut audio.notes] {
            if let Some(pending) = lane.pending.take() {
                receipts.push(pending.receipt());
            }
            while let Ok(entry) = lane.receive.pop() {
                receipts.push(Pending { entry, finished: None }.receipt());
            }
        }
        receipts.sort_unstable_by_key(|r| r.sequence);
        receipts
    }
}

impl State {
    fn urgent<T: Copy>(&mut self, stop: bool, sequence: Sequence, apply: &mut impl FnMut(Event<T>) -> bool) {
        if stop {
            self.seen_stop = sequence;
        } else {
            self.seen_release = self.seen_release.max(sequence);
        }
        self.faulted = true;
        self.faulted = !apply(if stop { Event::Stop { sequence } } else { Event::AllNotesOff { sequence } });
        if self.faulted {
            return;
        }
        if stop {
            self.shared.applied_stop.store(sequence.0, Ordering::Release);
        }
        self.shared.applied_release.store(self.seen_stop.max(self.seen_release).0, Ordering::Release);
    }
    fn recover<T: Copy>(&mut self, budget: &mut Budget, apply: &mut impl FnMut(Event<T>) -> bool) -> bool {
        if self.faulted {
            return true;
        }
        let stop = Sequence(self.shared.stop.load(Ordering::Acquire));
        let release = Sequence(self.shared.release.load(Ordering::Acquire));
        let new_stop = stop > self.seen_stop;
        let new_release = release > self.seen_release.max(self.seen_stop);
        // Stop includes release through its sequence. An older release never delays it,
        // even if this callback already spent the independent AllNotesOff budget.
        if new_stop {
            if budget.stop_serviced {
                return false;
            }
            budget.stop_serviced = true;
            self.urgent(true, stop, apply);
            if self.faulted {
                return true;
            }
        }
        if new_release && release > self.seen_stop {
            if budget.release_serviced {
                return false;
            }
            budget.release_serviced = true;
            self.urgent(false, release, apply);
        }
        true
    }
    fn disposition<T: Copy>(&self, entry: Entry<T>, active: PlanGeneration) -> Option<Outcome> {
        if self.faulted {
            return Some(Outcome::Faulted);
        }
        if entry.sequence <= self.seen_stop {
            return Some(Outcome::Stopped);
        }
        if entry.class == Class::NoteOn && entry.sequence <= self.seen_release {
            return Some(Outcome::NotesReleased);
        }
        let Some(required) = entry.plan else {
            return Some(Outcome::Applied);
        };
        if required == active {
            Some(Outcome::Applied)
        } else if required < active {
            Some(Outcome::PlanSuperseded)
        } else if required.get() <= self.shared.rejected.load(Ordering::Acquire) {
            Some(Outcome::PlanRejected)
        } else {
            None
        }
    }
    fn service_lane<T: Copy>(
        &mut self,
        lane: &mut AudioLane<T>,
        notes: bool,
        budget: &mut Budget,
        active: PlanGeneration,
        apply: &mut impl FnMut(Event<T>) -> bool,
    ) -> Service {
        let mut result = Service::default();
        if !self.recover(budget, apply) {
            result.recovery_deferred = true;
            return result;
        }
        loop {
            if let Some(pending) = lane.pending.filter(|p| p.finished.is_some()) {
                if lane.returned.push(pending.receipt()).is_err() {
                    result.return_pending = true;
                    break;
                }
                lane.pending = None;
            }
            if budget.available(notes) == 0 {
                break;
            }
            if lane.pending.is_none() {
                let Ok(entry) = lane.receive.pop() else {
                    break;
                };
                lane.pending = Some(Pending { entry, finished: None });
            }
            budget.spend(notes);
            result.examined += 1;
            // Acquire after dequeue observes urgent stores preceding a newer publication.
            if !self.recover(budget, apply) {
                result.recovery_deferred = true;
                break;
            }
            let Some(pending) = lane.pending else {
                break;
            };
            let Some(outcome) = self.disposition(pending.entry, active) else {
                result.waiting_for_plan = true;
                break;
            };
            lane.pending = Some(Pending { finished: Some(Outcome::Faulted), ..pending });
            if outcome == Outcome::Applied {
                self.faulted = true;
                self.faulted = !apply(Event::Action { sequence: pending.entry.sequence, payload: pending.entry.payload });
                if self.faulted {
                    continue;
                }
                result.applied += 1;
            }
            lane.pending = Some(Pending { finished: Some(outcome), ..pending });
        }
        result.faulted = self.faulted;
        result
    }
}

impl<T: Copy> Audio<T> {
    /// The paired bridge supplies both generation and permanent fault state. A foreign
    /// runtime, even with equal numeric generations, permanently refuses this lane.
    pub fn service(
        &mut self,
        budget: &mut Budget,
        bridge: &mut plan_bridge::Audio,
        apply: &mut impl FnMut(Event<T>, &mut plan_bridge::Processing<'_>),
    ) -> Service {
        if !bridge.matches_runtime(&self.runtime) || bridge.engine().faulted() {
            self.state.faulted = true;
        }
        let active = bridge.engine().active().generation;
        let Self { state, notes, actions, runtime } = self;
        let mut dispatch = |event| {
            // The restricted view cannot change these. Recheck at each dispatch so
            // future view extensions cannot silently invalidate prerequisite checks.
            if !bridge.matches_runtime(runtime) || bridge.engine().faulted() || bridge.engine().active().generation != active {
                return false;
            }
            apply(event, &mut bridge.processing());
            true
        };
        let mut result = state.service_lane(notes, true, budget, active, &mut dispatch);
        if !result.recovery_deferred {
            result.combine(state.service_lane(actions, false, budget, active, &mut dispatch));
        }
        result
    }
    // Private fixture seam; production callers cannot bypass runtime/fault validation.
    #[cfg(test)]
    fn service_generation(&mut self, budget: &mut Budget, active: PlanGeneration, apply: &mut impl FnMut(Event<T>)) -> Service {
        let apply = &mut |event| {
            apply(event);
            true
        };
        let mut result = self.state.service_lane(&mut self.notes, true, budget, active, apply);
        if !result.recovery_deferred {
            result.combine(self.state.service_lane(&mut self.actions, false, budget, active, apply));
        }
        result
    }
    pub fn fault(&mut self) {
        self.state.faulted = true;
    }
}

#[cfg(test)]
mod tests;
