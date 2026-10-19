//! Main-thread transaction staging. A reserved action already owns its sequence and
//! return credit, even while its requested snapshot has no compiled plan identity.
use super::*;
use action_lane::{NoteKind, Outcome, Receipt, SendError, Sequence};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice<T: Copy, R: Copy> {
    Session(SessionNotice),
    Action(Receipt<T, R>),
}

#[derive(Clone, Copy)]
struct Waiting {
    sequence: Sequence,
    request: Option<SessionRequestId>,
}

pub struct Control<T: Copy, R: Copy> {
    pub(super) sessions: SessionControl,
    actions: action_lane::Control<T, R>,
    waiting: VecDeque<Waiting>,
    target: Option<(SessionRequestId, RequestState)>,
    prefer_session: bool,
}

pub fn endpoints<T: Copy, R: Copy>(target: RenderTarget, limits: PlanLimits) -> (Control<T, R>, plan_bridge::Audio, action_lane::Audio<T, R>) {
    let (sessions, plans) = session_pair(target, limits);
    let (actions, audio) = sessions.actions();
    (Control { sessions, actions, waiting: VecDeque::with_capacity(action_lane::CAPACITY), target: None, prefer_session: false }, plans, audio)
}

impl<T: Copy, R: Copy> Control<T, R> {
    pub fn request(&mut self, session: Arc<Session>, input: Option<Arc<InputRing>>) -> Result<SessionRequestId, PlayerError> {
        let id = self.sessions.request(session, input)?;
        self.target = Some((id, RequestState::Preparing));
        // Resolve coalescing before the caller can collect the session's terminal notice.
        self.resolve();
        Ok(id)
    }

    pub fn service(&mut self, make: &mut dyn FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance>) {
        let in_flight = self.sessions.published;
        self.sessions.service(make);
        if let Some(published) = in_flight
            && matches!(self.sessions.state(published.id), RequestState::Finished(SessionOutcome::Rejected(_)))
        {
            self.actions.reject_plan(published.generation);
        }
        if let Some((id, state)) = &mut self.target {
            let observed = self.sessions.state(*id);
            if observed != RequestState::Unknown {
                *state = observed;
            }
        }
        self.resolve();
    }

    /// The prerequisite is the snapshot requested at admission, not whichever plan
    /// happens to be current when the callback eventually handles the action.
    pub fn send_action(&mut self, payload: T, needs_session: bool) -> Result<Sequence, PlayerError> {
        if self.sessions.faulted() {
            return Err(PlayerError::Faulted);
        }
        let request = if needs_session { Some(self.target.ok_or(PlayerError::NoSession)?.0) } else { None };
        let sequence = self.actions.reserve_action(payload).map_err(PlayerError::Action)?;
        self.waiting.push_back(Waiting { sequence, request });
        self.resolve();
        Ok(sequence)
    }

    pub fn send_note(&mut self, payload: T, kind: NoteKind) -> Result<Sequence, PlayerError> {
        if self.sessions.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.actions.send_note(payload, kind).map_err(PlayerError::Action)
    }

    pub fn stop(&mut self) -> Result<Sequence, PlayerError> {
        let sequence = self.actions.stop().map_err(PlayerError::Action)?;
        self.waiting.clear(); // stop() produced a receipt for every reserved action.
        Ok(sequence)
    }

    pub fn all_notes_off(&mut self) -> Result<Sequence, PlayerError> {
        self.actions.all_notes_off().map_err(PlayerError::Action)
    }

    /// Monotonic acknowledgments: all recovery requests up to each returned sequence
    /// have reached the handler. Admission itself is never an execution acknowledgment.
    pub fn recovery_progress(&self) -> (Sequence, Sequence) {
        self.actions.recovery_progress()
    }

    fn resolve(&mut self) {
        // At most CAPACITY iterations; no producer can extend this main-thread pass.
        while let Some(waiting) = self.waiting.front().copied() {
            let state = waiting.request.map(|id| {
                let observed = self.sessions.state(id);
                if observed == RequestState::Unknown {
                    self.target.filter(|(target, _)| *target == id).map_or(observed, |(_, state)| state)
                } else {
                    observed
                }
            });
            let resolved = match state {
                Some(RequestState::Preparing) if !self.sessions.faulted() => break,
                Some(RequestState::Published(generation) | RequestState::Finished(SessionOutcome::Applied { generation })) => {
                    self.publish(waiting.sequence, Some(generation))
                }
                None if !self.sessions.faulted() => self.publish(waiting.sequence, None),
                Some(RequestState::Finished(SessionOutcome::Superseded)) => self.actions.cancel_reserved(waiting.sequence, Outcome::PlanSuperseded),
                Some(RequestState::Finished(SessionOutcome::Rejected(_))) => self.actions.cancel_reserved(waiting.sequence, Outcome::PlanRejected),
                _ => self.actions.cancel_reserved(waiting.sequence, Outcome::Faulted),
            };
            if !resolved {
                break;
            }
            self.waiting.pop_front();
        }
    }

    fn publish(&mut self, sequence: Sequence, plan: Option<PlanGeneration>) -> bool {
        match self.actions.publish_reserved(sequence, plan) {
            Ok(()) => true,
            // Preserve ownership and order under defensive saturation.
            Err(SendError::Full { .. } | SendError::EarlierReservation) => false,
            Err(_) => {
                self.sessions.faulted = true;
                false
            }
        }
    }

    /// Bounded copying into reusable caller storage. Session and action results take
    /// turns, so either class remains observable under continuous traffic in the other.
    pub fn poll_notices(&mut self, out: &mut [Option<Notice<T, R>>]) -> usize {
        self.resolve();
        out.fill(None);
        let mut count = 0;
        for slot in out {
            self.prefer_session = !self.prefer_session;
            let notice = if self.prefer_session {
                self.session_notice().or_else(|| self.actions.reclaim().map(Notice::Action))
            } else {
                self.actions.reclaim().map(Notice::Action).or_else(|| self.session_notice())
            };
            if notice.is_none() {
                break;
            }
            *slot = notice;
            count += 1;
        }
        count
    }

    fn session_notice(&mut self) -> Option<Notice<T, R>> {
        let mut slot = [None];
        self.sessions.poll_notices(&mut slot);
        slot[0].map(Notice::Session)
    }

    /// Call only after callbacks are quiescent, on the instances' creating thread.
    pub fn retire(self, plans: plan_bridge::Audio, audio: action_lane::Audio<T, R>) -> (Vec<Notice<T, R>>, Vec<SlotBinding>) {
        let mut notices: Vec<_> = self.actions.retire(audio).into_iter().map(Notice::Action).collect();
        let (sessions, released) = self.sessions.retire(plans);
        notices.extend(sessions.into_iter().map(Notice::Session));
        (notices, released)
    }
}
