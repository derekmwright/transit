//! Control-side preparation and request identity for the prepared Player runtime.
//! No session request, handle lookup or notice storage belongs to the device callback.

use crate::{MeterSnapshot, action_lane, plan_bridge, record::InputRing};
use soundcraft_mix::{
    CompileError, EnvelopeState, InstanceSpec, NewInstance, PlanGeneration, PlanLimits, RejectReason, RenderTarget, SlotBinding, SlotGen,
};
use soundcraft_model::Session;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

mod control;
pub use control::{Control, Notice, endpoints};
mod handles;
pub use handles::Handles;

// Integration fixture first; the device adapter switches to this implementation
// once its event and shutdown plumbing is complete.
#[cfg(test)]
mod audio;

pub const SESSION_REQUESTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionRequestId(u64);
impl SessionRequestId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerError {
    SessionBackpressure,
    Action(action_lane::SendError),
    NoSession,
    SequenceExhausted,
    NeedsReopen { current_rate: u32, requested_rate: u32 },
    Faulted,
}
impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionBackpressure => f.write_str("session request results must be collected before another update"),
            Self::Action(error) => write!(f, "audio action was not admitted: {error:?}"),
            Self::NoSession => f.write_str("audio runtime has no requested session"),
            Self::SequenceExhausted => f.write_str("audio request identities are exhausted; reopen the runtime"),
            Self::NeedsReopen { current_rate, requested_rate } => {
                write!(f, "audio runtime must reopen for {requested_rate} Hz (currently {current_rate} Hz)")
            }
            Self::Faulted => f.write_str("audio runtime is faulted; reopen it before submitting work"),
        }
    }
}
impl std::error::Error for PlayerError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFailure {
    Compile(CompileError),
    Adoption(RejectReason),
    NotAdopted,
    FactoryPanicked,
    Faulted,
    Shutdown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    Applied { generation: PlanGeneration },
    Superseded,
    Rejected(SessionFailure),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionNotice {
    pub request: SessionRequestId,
    pub outcome: SessionOutcome,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    Preparing,
    Published(PlanGeneration),
    Finished(SessionOutcome),
    Unknown,
}
struct Requested {
    id: SessionRequestId,
    session: Arc<Session>,
    input: Option<Arc<InputRing>>,
}
#[derive(Clone, Copy)]
struct Published {
    id: SessionRequestId,
    generation: PlanGeneration,
}

/// Main-thread-only owner. One latest uncompiled request, one in-flight publication,
/// and their notices share exactly SESSION_REQUESTS credits. Outcomes cannot be lost
/// because the UI stops polling; further admission refuses without replacing work.
pub struct SessionControl {
    bridge: plan_bridge::Control,
    target: RenderTarget,
    next: u64,
    latest: Option<Requested>,
    published: Option<Published>,
    active: Option<Published>,
    notices: VecDeque<SessionNotice>,
    credits: usize,
    faulted: bool,
    main_thread: PhantomData<Rc<()>>,
}

pub fn session_pair(target: RenderTarget, limits: PlanLimits) -> (SessionControl, plan_bridge::Audio) {
    let target = RenderTarget { max_block: target.max_block.clamp(16, 16_384), ..target };
    let (bridge, audio) = plan_bridge::pair(target, limits);
    (
        SessionControl {
            bridge,
            target,
            next: 0,
            latest: None,
            published: None,
            active: None,
            notices: VecDeque::with_capacity(SESSION_REQUESTS),
            credits: SESSION_REQUESTS,
            faulted: false,
            main_thread: PhantomData,
        },
        audio,
    )
}
impl SessionControl {
    pub fn request(&mut self, session: Arc<Session>, input: Option<Arc<InputRing>>) -> Result<SessionRequestId, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        let rate = session.sample_rate.hz();
        if rate != self.target.sample_rate {
            return Err(PlayerError::NeedsReopen { current_rate: self.target.sample_rate, requested_rate: rate });
        }
        if self.credits == 0 {
            return Err(PlayerError::SessionBackpressure);
        }
        let next = self.next.checked_add(1).ok_or(PlayerError::SequenceExhausted)?;
        let id = SessionRequestId(next);
        self.next = next;
        self.credits -= 1;
        if let Some(old) = self.latest.replace(Requested { id, session, input }) {
            self.finish(old.id, SessionOutcome::Superseded);
            // The unsent Session/InputRing owners are released here, on main.
        }
        Ok(id)
    }
    fn finish(&mut self, request: SessionRequestId, outcome: SessionOutcome) {
        // The request reserved this slot before any coalescing/compilation/adoption.
        self.notices.push_back(SessionNotice { request, outcome });
    }
    /// Reclaim at most one plan and compile at most one request. Factory calls and
    /// resource destruction occur here, never in the audio endpoint.
    pub fn service(&mut self, make: &mut dyn FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance>) {
        self.reclaim(false);
        if self.faulted() {
            if let Some(request) = self.latest.take() {
                self.finish(request.id, SessionOutcome::Rejected(SessionFailure::Faulted));
            }
            return;
        }
        if !self.bridge.can_prepare() {
            return;
        }
        let Some(request) = self.latest.take() else { return };
        self.bridge.request(request.session, request.input);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.bridge.prepare(make)));
        match result {
            Ok(Ok(true)) => self.published = Some(Published { id: request.id, generation: self.bridge.issued() }),
            Ok(Ok(false)) => {
                self.faulted = true; // Credit/publication invariant failed; do not pretend this is queued.
                self.finish(request.id, SessionOutcome::Rejected(SessionFailure::Faulted));
            }
            Ok(Err(error)) => self.finish(request.id, SessionOutcome::Rejected(SessionFailure::Compile(error))),
            Err(_) => {
                self.faulted = true;
                self.finish(request.id, SessionOutcome::Rejected(SessionFailure::FactoryPanicked));
            }
        }
    }
    fn reclaim(&mut self, shutdown: bool) {
        if let Some(report) = self.bridge.poll() {
            if let Some(published) = self.published.take() {
                let outcome = if report.torn || report.requested != published.generation {
                    self.faulted = true;
                    SessionOutcome::Rejected(SessionFailure::Faulted)
                } else {
                    match report.state {
                        EnvelopeState::Returned { .. } => {
                            self.active = Some(published);
                            SessionOutcome::Applied { generation: published.generation }
                        }
                        EnvelopeState::Rejected { reason, .. } => SessionOutcome::Rejected(SessionFailure::Adoption(reason)),
                        EnvelopeState::Pending => SessionOutcome::Rejected(if self.bridge.faulted() {
                            SessionFailure::Faulted
                        } else if shutdown {
                            SessionFailure::Shutdown
                        } else {
                            SessionFailure::NotAdopted
                        }),
                    }
                };
                self.finish(published.id, outcome);
            } else {
                self.faulted = true;
            }
        }
    }
    pub fn state(&self, id: SessionRequestId) -> RequestState {
        if let Some(request) = &self.latest
            && request.id == id
        {
            return RequestState::Preparing;
        }
        if let Some(published) = self.published
            && published.id == id
        {
            return RequestState::Published(published.generation);
        }
        if let Some(notice) = self.notices.iter().find(|n| n.request == id) {
            return RequestState::Finished(notice.outcome);
        }
        if let Some(active) = self.active
            && active.id == id
        {
            return RequestState::Finished(SessionOutcome::Applied { generation: active.generation });
        }
        RequestState::Unknown
    }
    /// Copy results into caller-owned storage; unused slots are cleared. No allocation.
    pub fn poll_notices(&mut self, out: &mut [Option<SessionNotice>]) -> usize {
        out.fill(None);
        let mut count = 0;
        for slot in out {
            let Some(notice) = self.notices.pop_front() else { break };
            *slot = Some(notice);
            self.credits += 1;
            count += 1;
        }
        count
    }
    pub fn faulted(&self) -> bool {
        self.faulted || self.bridge.faulted()
    }
    pub fn active_bindings(&self) -> Vec<SlotBinding> {
        self.bridge.active_bindings()
    }
    pub fn take_released(&mut self) -> Vec<SlotBinding> {
        self.bridge.take_released()
    }
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        self.bridge.copy_meters(out);
    }
    pub(super) fn actions<T: Copy, R: Copy>(&self) -> (action_lane::Control<T, R>, action_lane::Audio<T, R>) {
        action_lane::pair_with_result(&self.bridge)
    }
    /// Only after callback quiescence. Return all terminal notices and generation-specific
    /// handle releases; the final main-thread owner disposes the associated handles.
    pub fn retire(mut self, mut audio: plan_bridge::Audio) -> (Vec<SessionNotice>, Vec<SlotBinding>) {
        if let Some(request) = self.latest.take() {
            self.finish(request.id, SessionOutcome::Rejected(SessionFailure::Shutdown));
        }
        // A returned/adopted publication must be observed before classifying shutdown.
        audio.return_pending(&mut plan_bridge::callback_scope());
        self.reclaim(true);
        let released = self.bridge.retire(audio);
        if let Some(published) = self.published.take() {
            self.notices.push_back(SessionNotice { request: published.id, outcome: SessionOutcome::Rejected(SessionFailure::Shutdown) });
        }
        (self.notices.into_iter().collect(), released)
    }
}

#[cfg(test)]
mod tests;
