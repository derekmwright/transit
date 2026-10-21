//! Independent bounded transport and note lanes for one prepared-plan runtime.
//! Transport may wait for an exact compiled plan. Notes never wait for transport or a
//! plan prerequisite. Each class owns its own admission, return and callback budgets.
//! Copy payloads carry no heap owners; endpoints stay anchored on control until quiescence.

use crate::plan_bridge;
use soundcraft_mix::PlanGeneration;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const CAPACITY: usize = 256;
pub const NOTE_CAPACITY: usize = 256;
pub const CALLBACK_ACTIONS: usize = 32;
pub const CALLBACK_NOTES: usize = 32;
/// Fixed upper bound on a handler receipt's stack/ring copy cost.
pub const MAX_RESULT_BYTES: usize = 64;

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
    UnknownReservation,
    EarlierReservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Match this result to the admitted request by `sequence`, never by receipt order.
/// Independent note/transport lanes can complete in a different order.
pub struct Receipt<T: Copy, R: Copy = ()> {
    pub sequence: Sequence,
    pub payload: T,
    pub outcome: Outcome,
    /// Some only after the handler returned. Applied acknowledges dispatch, not the
    /// handler's semantic success: a mix refusal is still returned here for the caller.
    pub handler_result: Option<R>,
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
struct Pending<T: Copy, R: Copy = ()> {
    entry: Entry<T>,
    finished: Option<Outcome>,
    handler_result: Option<R>,
}
impl<T: Copy, R: Copy> Pending<T, R> {
    fn receipt(self) -> Receipt<T, R> {
        Receipt {
            sequence: self.entry.sequence,
            payload: self.entry.payload,
            outcome: self.finished.unwrap_or(Outcome::Shutdown),
            handler_result: self.handler_result,
        }
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
    #[cfg(not(target_arch = "wasm32"))]
    midi_dispatch_remaining: usize,
    stop_serviced: bool,
    release_serviced: bool,
    source_scan_serviced: bool,
}
pub(super) fn callback_scope() -> Budget {
    Budget {
        remaining: CALLBACK_ACTIONS,
        notes_remaining: CALLBACK_NOTES,
        #[cfg(not(target_arch = "wasm32"))]
        midi_dispatch_remaining: CALLBACK_NOTES,
        stop_serviced: false,
        release_serviced: false,
        source_scan_serviced: false,
    }
}
impl Budget {
    /// Native input staging and final render dispatch have separate fixed work
    /// limits, shared across every scratch chunk of the same device callback.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn claim_midi_dispatch(&mut self) -> bool {
        if self.midi_dispatch_remaining == 0 {
            return false;
        }
        self.midi_dispatch_remaining -= 1;
        true
    }
    pub(super) fn claim_source_scan(&mut self) -> bool {
        !std::mem::replace(&mut self.source_scan_serviced, true)
    }
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

struct ControlLane<T: Copy, R: Copy = ()> {
    send: rtrb::Producer<Entry<T>>,
    returned: rtrb::Consumer<Receipt<T, R>>,
    credits: usize,
    reserved: Vec<Entry<T>>,
    cancelled: VecDeque<Receipt<T, R>>,
    prefer_cancelled: bool,
}
struct AudioLane<T: Copy, R: Copy = ()> {
    receive: rtrb::Consumer<Entry<T>>,
    returned: rtrb::Producer<Receipt<T, R>>,
    pending: Option<Pending<T, R>>,
}
fn lane<T: Copy, R: Copy>(capacity: usize) -> (ControlLane<T, R>, AudioLane<T, R>) {
    let (send, receive) = rtrb::RingBuffer::new(capacity);
    let (return_send, returned) = rtrb::RingBuffer::new(capacity);
    (
        ControlLane {
            send,
            returned,
            credits: capacity,
            reserved: Vec::with_capacity(capacity),
            cancelled: VecDeque::with_capacity(capacity),
            prefer_cancelled: false,
        },
        AudioLane { receive, returned: return_send, pending: None },
    )
}
impl<T: Copy, R: Copy> ControlLane<T, R> {
    fn reclaim(&mut self) -> Option<Receipt<T, R>> {
        self.prefer_cancelled = !self.prefer_cancelled;
        let receipt = if self.prefer_cancelled {
            self.cancelled.pop_front().or_else(|| self.returned.pop().ok())
        } else {
            self.returned.pop().ok().or_else(|| self.cancelled.pop_front())
        }?;
        self.credits += 1;
        Some(receipt)
    }
}

pub struct Control<T: Copy, R: Copy = ()> {
    actions: ControlLane<T, R>,
    notes: ControlLane<T, R>,
    shared: Arc<Shared>,
    next: u64,
    prefer_notes: bool,
}
pub struct Audio<T: Copy, R: Copy = ()> {
    actions: AudioLane<T, R>,
    notes: AudioLane<T, R>,
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
    pair_with_result(bridge)
}

/// Typed results travel back in the same reserved receipt, without a side channel.
/// Handler results have a fixed copy budget as well as the `Copy` ownership bound.
///
/// ```compile_fail
/// use soundcraft_playback::{action_lane, plan_bridge};
/// use soundcraft_mix::{DeviceGeneration, PlanLimits, RenderTarget};
/// let (bridge, _) = plan_bridge::pair(
///     RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 },
///     PlanLimits::default(),
/// );
/// let _ = action_lane::pair_with_result::<(), [u8; 65]>(&bridge);
/// ```
pub fn pair_with_result<T: Copy, R: Copy>(bridge: &plan_bridge::Control) -> (Control<T, R>, Audio<T, R>) {
    const {
        assert!(size_of::<R>() <= MAX_RESULT_BYTES, "action handler result exceeds its fixed copy budget");
    }
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

impl<T: Copy, R: Copy> Control<T, R> {
    fn sequence(&mut self) -> Result<Sequence, SendError> {
        self.next = self.next.checked_add(1).ok_or(SendError::SequenceExhausted)?;
        Ok(Sequence(self.next))
    }
    pub fn send_action(&mut self, payload: T, plan: Option<PlanGeneration>) -> Result<Sequence, SendError> {
        if !self.actions.reserved.is_empty() {
            return Err(SendError::EarlierReservation);
        }
        self.send(payload, Class::Ordinary, plan)
    }
    /// Reserve an action's order/credit while Player prepares its exact session plan.
    /// No entry reaches audio until publish_reserved assigns the compiled prerequisite.
    pub(super) fn reserve_action(&mut self, payload: T) -> Result<Sequence, SendError> {
        if self.actions.credits == 0 {
            return Err(SendError::Full { release_recovery: None });
        }
        let sequence = self.sequence()?;
        self.actions.reserved.push(Entry { sequence, payload, class: Class::Ordinary, plan: None });
        self.actions.credits -= 1;
        Ok(sequence)
    }
    pub(super) fn publish_reserved(&mut self, sequence: Sequence, plan: Option<PlanGeneration>) -> Result<(), SendError> {
        let Some(index) = self.actions.reserved.iter().position(|entry| entry.sequence == sequence) else {
            return Err(SendError::UnknownReservation);
        };
        if index != 0 {
            return Err(SendError::EarlierReservation);
        }
        let mut entry = self.actions.reserved.remove(index);
        entry.plan = plan;
        if let Err(rtrb::PushError::Full(entry)) = self.actions.send.push(entry) {
            // Preserve the original position/order if the credit invariant was violated.
            self.actions.reserved.insert(index, entry);
            return Err(SendError::Full { release_recovery: None });
        }
        Ok(())
    }
    pub(super) fn cancel_reserved(&mut self, sequence: Sequence, outcome: Outcome) -> bool {
        if matches!(outcome, Outcome::Applied | Outcome::NotesReleased) {
            return false;
        }
        let Some(index) = self.actions.reserved.iter().position(|entry| entry.sequence == sequence) else { return false };
        let entry = self.actions.reserved.remove(index);
        // Each cancelled entry already owns a receipt credit; this cannot grow capacity.
        self.actions.cancelled.push_back(Pending { entry, finished: Some(outcome), handler_result: None }.receipt());
        true
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
        // Old staged Plays must finish even if their session is never compiled. Their
        // order was reserved at admission, so no later publication can revive them.
        for entry in self.actions.reserved.drain(..) {
            self.actions.cancelled.push_back(Pending { entry, finished: Some(Outcome::Stopped), handler_result: None }.receipt());
        }
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
    /// Dispatch within each class is FIFO; a control-side cancellation can return
    /// before older audio work. Match every receipt by sequence, never return order.
    /// Round-robin reclamation prevents transport or cancellations starving notes/audio.
    pub fn reclaim(&mut self) -> Option<Receipt<T, R>> {
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
    pub fn retire(mut self, mut audio: Audio<T, R>) -> Vec<Receipt<T, R>> {
        let mut receipts = Vec::with_capacity(CAPACITY + NOTE_CAPACITY);
        while let Some(receipt) = self.reclaim() {
            receipts.push(receipt);
        }
        for entry in self.actions.reserved.drain(..).chain(self.notes.reserved.drain(..)) {
            receipts.push(Pending { entry, finished: Some(Outcome::Shutdown), handler_result: None }.receipt());
        }
        for lane in [&mut audio.actions, &mut audio.notes] {
            if let Some(pending) = lane.pending.take() {
                receipts.push(pending.receipt());
            }
            while let Ok(entry) = lane.receive.pop() {
                receipts.push(Pending { entry, finished: None, handler_result: None }.receipt());
            }
        }
        receipts.sort_unstable_by_key(|r| r.sequence);
        receipts
    }
}

impl State {
    fn urgent<T: Copy, R: Copy>(&mut self, stop: bool, sequence: Sequence, apply: &mut impl FnMut(Event<T>) -> Option<R>) {
        if stop {
            self.seen_stop = sequence;
        } else {
            self.seen_release = self.seen_release.max(sequence);
        }
        self.faulted = true;
        self.faulted = apply(if stop { Event::Stop { sequence } } else { Event::AllNotesOff { sequence } }).is_none();
        if self.faulted {
            return;
        }
        if stop {
            self.shared.applied_stop.store(sequence.0, Ordering::Release);
        }
        self.shared.applied_release.store(self.seen_stop.max(self.seen_release).0, Ordering::Release);
    }
    fn recover<T: Copy, R: Copy>(&mut self, budget: &mut Budget, apply: &mut impl FnMut(Event<T>) -> Option<R>) -> bool {
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
    fn service_lane<T: Copy, R: Copy>(
        &mut self,
        lane: &mut AudioLane<T, R>,
        notes: bool,
        budget: &mut Budget,
        active: PlanGeneration,
        apply: &mut impl FnMut(Event<T>) -> Option<R>,
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
                lane.pending = Some(Pending { entry, finished: None, handler_result: None });
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
            let handler_result = if outcome == Outcome::Applied {
                self.faulted = true;
                let Some(value) = apply(Event::Action { sequence: pending.entry.sequence, payload: pending.entry.payload }) else {
                    continue;
                };
                self.faulted = false;
                result.applied += 1;
                Some(value)
            } else {
                None
            };
            lane.pending = Some(Pending { finished: Some(outcome), handler_result, ..pending });
        }
        result.faulted = self.faulted;
        result
    }
}

impl<T: Copy, R: Copy> Audio<T, R> {
    /// The paired bridge supplies both generation and permanent fault state. A foreign
    /// runtime, even with equal numeric generations, permanently refuses this lane.
    pub fn service(
        &mut self,
        budget: &mut Budget,
        bridge: &mut plan_bridge::Audio,
        apply: &mut impl FnMut(Event<T>, &mut plan_bridge::Processing<'_>) -> R,
    ) -> Service {
        if !bridge.matches_runtime(&self.runtime) || bridge.faulted() {
            self.state.faulted = true;
        }
        let active = bridge.engine().active().generation;
        let Self { state, notes, actions, runtime } = self;
        let mut dispatch = |event| {
            // The restricted view cannot change these. Recheck at each dispatch so
            // future view extensions cannot silently invalidate prerequisite checks.
            if !bridge.matches_runtime(runtime) || bridge.faulted() || bridge.engine().active().generation != active {
                return None;
            }
            Some(apply(event, &mut bridge.processing()))
        };
        let mut result = state.service_lane(notes, true, budget, active, &mut dispatch);
        if !result.recovery_deferred {
            result.combine(state.service_lane(actions, false, budget, active, &mut dispatch));
        }
        result
    }
    // Private fixture seam; production callers cannot bypass runtime/fault validation.
    #[cfg(test)]
    fn service_generation(&mut self, budget: &mut Budget, active: PlanGeneration, apply: &mut impl FnMut(Event<T>) -> R) -> Service {
        let apply = &mut |event| Some(apply(event));
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
