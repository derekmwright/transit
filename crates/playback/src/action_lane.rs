//! Bounded ordered actions for one runtime generation. Copy payloads carry no heap owners.
//! A plan prerequisite names the exact compiled plan; an unavailable future plan defers
//! normal actions, while Stop and note-release recovery remain independently serviceable.
//! Both endpoints stay anchored on control through quiescence, like the publication ports.

use soundcraft_mix::PlanGeneration;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const CAPACITY: usize = 256;
pub const CALLBACK_ACTIONS: usize = 32;

/// Monotonic within one runtime generation. Exhaustion refuses, never aliases old work.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sequence(u64);

impl Sequence {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
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
}

/// A returned admission credit and its explicit disposition. Recovery means a rejected
/// note-off caused AllNotesOff; this is a counted failure policy, not normal note timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt<T: Copy> {
    pub sequence: Sequence,
    pub payload: T,
    pub outcome: Outcome,
    pub release_recovery: bool,
}

/// The handler must make Stop stop transport AND release notes. Recovery never waits
/// for a plan. If the handler unwinds, the lane faults and never calls it again.
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
    finished: Option<(Outcome, bool)>,
}

impl<T: Copy> Pending<T> {
    fn receipt(self) -> Receipt<T> {
        let (outcome, release_recovery) = self.finished.unwrap_or((Outcome::Shutdown, false));
        Receipt { sequence: self.entry.sequence, payload: self.entry.payload, outcome, release_recovery }
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

/// Use one budget across all chunks/blocks of the actual device callback.
pub struct Budget {
    remaining: usize,
    stop_serviced: bool,
    release_serviced: bool,
}

pub(super) fn callback_scope() -> Budget {
    Budget { remaining: CALLBACK_ACTIONS, stop_serviced: false, release_serviced: false }
}

impl Budget {
    pub fn remaining(&self) -> usize {
        self.remaining
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

pub struct Control<T: Copy> {
    send: rtrb::Producer<Entry<T>>,
    returned: rtrb::Consumer<Receipt<T>>,
    shared: Arc<Shared>,
    next: u64,
    credits: usize,
}

pub struct Audio<T: Copy> {
    receive: rtrb::Consumer<Entry<T>>,
    returned: rtrb::Producer<Receipt<T>>,
    shared: Arc<Shared>,
    pending: Option<Pending<T>>,
    seen_stop: Sequence,
    seen_release: Sequence,
    faulted: bool,
}

pub fn pair<T: Copy>() -> (Control<T>, Audio<T>) {
    let (send, receive) = rtrb::RingBuffer::new(CAPACITY);
    let (return_send, returned) = rtrb::RingBuffer::new(CAPACITY);
    let shared = Arc::new(Shared::default());
    (
        Control { send, returned, shared: Arc::clone(&shared), next: 0, credits: CAPACITY },
        Audio { receive, returned: return_send, shared, pending: None, seen_stop: Sequence(0), seen_release: Sequence(0), faulted: false },
    )
}

impl<T: Copy> Control<T> {
    fn sequence(&mut self) -> Result<Sequence, SendError> {
        self.next = self.next.checked_add(1).ok_or(SendError::SequenceExhausted)?;
        Ok(Sequence(self.next))
    }

    /// A full note-off lane requests release recovery even though the ordered action
    /// was refused. Its sequence also cancels older queued note-ons, preventing a
    /// delayed note-on from arriving after its lost release.
    pub fn send(&mut self, payload: T, class: Class, plan: Option<PlanGeneration>) -> Result<Sequence, SendError> {
        if self.credits == 0 {
            let release_recovery = if class == Class::NoteOff { Some(self.all_notes_off()?) } else { None };
            return Err(SendError::Full { release_recovery });
        }
        let sequence = self.sequence()?;
        let entry = Entry { sequence, payload, class, plan };
        if self.send.push(entry).is_err() {
            let release_recovery = if class == Class::NoteOff {
                self.shared.release.store(sequence.0, Ordering::Release);
                Some(sequence)
            } else {
                None
            };
            return Err(SendError::Full { release_recovery });
        }
        self.credits -= 1;
        Ok(sequence)
    }

    /// Supersedes older queued actions and never spends a normal admission credit.
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

    /// Call when the N=1 plan protocol confirms rejection/cancellation of a compiled
    /// generation. No later action may wait forever for that unreachable generation.
    pub fn reject_plan(&mut self, generation: PlanGeneration) {
        let previous = self.shared.rejected.load(Ordering::Relaxed);
        self.shared.rejected.store(previous.max(generation.get()), Ordering::Release);
    }

    /// Copy receipt: no owner disposal can occur after credit restoration.
    pub fn reclaim(&mut self) -> Option<Receipt<T>> {
        let receipt = self.returned.pop().ok()?;
        self.credits += 1;
        Some(receipt)
    }

    pub fn credits(&self) -> usize {
        self.credits
    }

    /// Independent acknowledgement counters may straddle a callback; not a time map.
    pub fn recovery_progress(&self) -> (Sequence, Sequence) {
        (Sequence(self.shared.applied_stop.load(Ordering::Acquire)), Sequence(self.shared.applied_release.load(Ordering::Acquire)))
    }

    /// After quiescence, recover receipts plus waiting/queued actions on control.
    /// No event handler runs during teardown. Both ring allocations also drop here.
    pub fn retire(mut self, mut audio: Audio<T>) -> Vec<Receipt<T>> {
        let mut receipts = Vec::with_capacity(CAPACITY);
        while let Some(receipt) = self.reclaim() {
            receipts.push(receipt);
        }
        if let Some(pending) = audio.pending.take() {
            receipts.push(pending.receipt());
        }
        while let Ok(entry) = audio.receive.pop() {
            receipts.push(Pending { entry, finished: None }.receipt());
        }
        receipts
    }
}

impl<T: Copy> Audio<T> {
    fn urgent(&mut self, stop: bool, sequence: Sequence, apply: &mut impl FnMut(Event<T>)) {
        if stop {
            self.seen_stop = sequence;
        } else {
            self.seen_release = self.seen_release.max(sequence);
        }
        self.faulted = true;
        apply(if stop { Event::Stop { sequence } } else { Event::AllNotesOff { sequence } });
        self.faulted = false;
        if stop {
            self.shared.applied_stop.store(sequence.0, Ordering::Release);
        }
        let released = self.seen_stop.max(self.seen_release);
        self.shared.applied_release.store(released.0, Ordering::Release);
    }

    fn recover(&mut self, budget: &mut Budget, apply: &mut impl FnMut(Event<T>)) -> bool {
        if self.faulted {
            return true;
        }
        let stop = Sequence(self.shared.stop.load(Ordering::Acquire));
        let release = Sequence(self.shared.release.load(Ordering::Acquire));
        let new_stop = stop > self.seen_stop;
        let new_release = release > self.seen_release;
        // Two observed urgent actions run in their control-side order.
        if new_release && (!new_stop || release < stop) {
            if budget.release_serviced {
                return false;
            }
            budget.release_serviced = true;
            self.urgent(false, release, apply);
        }
        if new_stop {
            if budget.stop_serviced {
                return false;
            }
            budget.stop_serviced = true;
            self.urgent(true, stop, apply);
        }
        if new_release && new_stop && release > stop {
            if budget.release_serviced {
                return false;
            }
            budget.release_serviced = true;
            self.urgent(false, release, apply);
        }
        true
    }

    fn disposition(&self, entry: Entry<T>, active: PlanGeneration) -> Option<Outcome> {
        if self.faulted {
            return Some(Outcome::Faulted);
        }
        if entry.sequence <= self.seen_stop {
            return Some(Outcome::Stopped);
        }
        if entry.class == Class::NoteOn && entry.sequence <= self.seen_release {
            return Some(Outcome::NotesReleased);
        }
        let Some(required) = entry.plan else { return Some(Outcome::Applied) };
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

    /// Bounded normal work, independent urgent recovery. Re-read urgent state after
    /// dequeuing: the queue's acquire observes urgent stores preceding this action's
    /// publication, so a newer Play cannot run before an earlier unseen Stop.
    pub fn service(&mut self, budget: &mut Budget, active: PlanGeneration, apply: &mut impl FnMut(Event<T>)) -> Service {
        let mut result = Service::default();
        if !self.recover(budget, apply) {
            result.recovery_deferred = true;
            return result;
        }
        loop {
            if let Some(pending) = self.pending.filter(|p| p.finished.is_some()) {
                if self.returned.push(pending.receipt()).is_err() {
                    result.return_pending = true;
                    break;
                }
                self.pending = None;
            }
            if budget.remaining == 0 {
                break;
            }
            if self.pending.is_none() {
                let Ok(entry) = self.receive.pop() else { break };
                self.pending = Some(Pending { entry, finished: None });
            }
            budget.remaining -= 1;
            result.examined += 1;
            if !self.recover(budget, apply) {
                result.recovery_deferred = true;
                break;
            }
            let Some(pending) = self.pending else { break };
            let Some(outcome) = self.disposition(pending.entry, active) else {
                result.waiting_for_plan = true;
                break;
            };
            let recovery = pending.entry.class == Class::NoteOff && matches!(outcome, Outcome::PlanRejected | Outcome::PlanSuperseded);
            let needs_release = recovery && pending.entry.sequence > self.seen_release.max(self.seen_stop);
            if needs_release && budget.release_serviced {
                result.recovery_deferred = true;
                break;
            }
            // Store failure disposition BEFORE invoking foreign engine/plugin behavior.
            self.pending = Some(Pending { finished: Some((Outcome::Faulted, false)), ..pending });
            if needs_release {
                budget.release_serviced = true;
                self.urgent(false, pending.entry.sequence, apply);
            }
            if outcome == Outcome::Applied {
                self.faulted = true;
                apply(Event::Action { sequence: pending.entry.sequence, payload: pending.entry.payload });
                self.faulted = false;
                result.applied += 1;
            }
            self.pending = Some(Pending { finished: Some((outcome, recovery)), ..pending });
        }
        result.faulted = self.faulted;
        result
    }

    /// After an external render fault, return future actions as Faulted without invoking
    /// handlers. Ownership and admission credits still round-trip until main reclaims.
    pub fn fault(&mut self) {
        self.faulted = true;
    }
}

#[cfg(test)]
mod tests;
