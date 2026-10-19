//! Main-side Player owner. The callback receives only the audio endpoints;
//! this owner retains requested snapshots, handles and every reclamation endpoint.
use super::*;
use crate::{EditorEdit, PluginStates};
use action_lane::{NoteKind, Sequence};
use soundcraft_model::TrackId;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ServiceReport {
    pub plugin_pending: usize,
    pub plugin_failed: usize,
    pub plugin_other_thread: usize,
    pub preparing: bool,
    pub source_revocations_pending: usize,
    pub sources_faulted: bool,
    pub latency_refresh: Option<SessionRequestId>,
    pub refresh_error: Option<PlayerError>,
}

/// Creating-thread owner (not Send/Sync through Control). Keep this alive, with a
/// strong holder anchoring its audio endpoints, until processing is quiescent.
/// Then pass the endpoint bundle to `retire` here. Dropping an audio endpoint on its
/// worker instead would destroy its plug-ins on that worker.
pub struct RuntimeControl<T: Copy, R: Copy> {
    control: Control<T, R>,
    handles: Handles,
    sources: sources::Control,
    requested: Arc<Session>,
    input: Option<Arc<InputRing>>,
    request: SessionRequestId,
    state: RequestState,
    /// Last capacity need for which this document lineage requested a refresh.
    /// A new document/input request resets it; an internal refresh does not.
    attempted_latency: u32,
    /// Active plan in the current requested document/input lineage. An internal
    /// refresh failure retains it; a user request clears it before any refresh.
    confirmed: Option<PlanGeneration>,
}

/// The callback-side resources form one ownership unit. Anchor this bundle on
/// main while workers borrow it, and return it intact after device quiescence.
pub struct AudioPorts<T: Copy, R: Copy> {
    pub(crate) plans: plan_bridge::Audio,
    pub(crate) actions: action_lane::Audio<T, R>,
    pub(crate) sources: sources::Audio,
}

pub struct Retirement<T: Copy, R: Copy> {
    pub notices: Vec<Notice<T, R>>,
    pub sources: Vec<sources::Receipt>,
}

impl<T: Copy, R: Copy> RuntimeControl<T, R> {
    /// Admit the initial snapshot without running a factory. Call `service` on main
    /// before starting audio; failed device opens can return the untouched endpoints.
    pub fn new(
        session: Arc<Session>,
        input: Option<Arc<InputRing>>,
        target: RenderTarget,
        limits: PlanLimits,
    ) -> Result<(Self, AudioPorts<T, R>), PlayerError> {
        if session.sample_rate.hz() != target.sample_rate {
            return Err(PlayerError::NeedsReopen { current_rate: target.sample_rate, requested_rate: session.sample_rate.hz() });
        }
        let (mut control, plans, audio) = endpoints(target, limits);
        let (sources, ingress) = sources::pair(&control.sessions.bridge).map_err(PlayerError::Source)?;
        let request = control.request(Arc::clone(&session), input.clone())?;
        Ok((
            Self {
                control,
                handles: Handles::default(),
                sources,
                requested: session,
                input,
                request,
                state: RequestState::Preparing,
                attempted_latency: 0,
                confirmed: None,
            },
            AudioPorts { plans, actions: audio, sources: ingress },
        ))
    }

    pub fn update_session(&mut self, session: Arc<Session>) -> Result<SessionRequestId, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        let request = self.control.request(Arc::clone(&session), self.input.clone())?;
        self.requested = session;
        self.request = request;
        self.state = RequestState::Preparing;
        self.attempted_latency = 0;
        self.confirmed = None;
        Ok(request)
    }

    pub fn set_input(&mut self, input: Option<Arc<InputRing>>) -> Result<SessionRequestId, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        let request = self.control.request(Arc::clone(&self.requested), input.clone())?;
        self.input = input;
        self.request = request;
        self.state = RequestState::Preparing;
        self.attempted_latency = 0;
        self.confirmed = None;
        Ok(request)
    }

    /// G7 lifecycle callbacks and all factories/reclamation run on the creating
    /// thread. At most one plan is prepared per call. A G8 latency rise admits a
    /// bounded refresh for the next call; it never invokes a second compiler pass.
    pub fn service(&mut self) -> ServiceReport {
        self.service_with(|| {
            let clap = soundcraft_clap_host::service_requests();
            let vst3 = soundcraft_vst3_host::service_requests();
            (clap.pending.saturating_add(vst3.pending), clap.failed.saturating_add(vst3.failed), clap.other_thread.saturating_add(vst3.other_thread))
        })
    }

    fn service_with(&mut self, host_requests: impl FnOnce() -> (usize, usize, usize)) -> ServiceReport {
        // A terminal failure may already have been observed on control. Publish
        // it before any host/handle servicing can wait on external plug-in code.
        if self.faulted() {
            self.fault();
        }
        let (pending, failed, other_thread) = host_requests();
        self.handles.service(&mut self.control);
        let mut report = self.after_service();
        report.plugin_pending = pending;
        report.plugin_failed = failed;
        report.plugin_other_thread = other_thread;
        report
    }

    fn after_service(&mut self) -> ServiceReport {
        // Preparation can catch a factory panic and fault SessionControl locally.
        // Every endpoint must learn that terminal state, including source ingress
        // and an audio endpoint with an older active plan and queued actions.
        if self.faulted() {
            self.fault();
        }
        let observed = self.control.sessions.state(self.request);
        if observed != RequestState::Unknown {
            self.state = observed;
        }
        let mut report =
            ServiceReport { source_revocations_pending: self.sources.pending(), sources_faulted: self.sources.faulted(), ..ServiceReport::default() };
        if let RequestState::Finished(SessionOutcome::Applied { generation }) = self.state {
            self.confirmed = Some(generation);
        }
        // A failed internal refresh may be retried for a genuinely NEW capacity
        // need. A user edit/input change has already cleared this lineage marker.
        if !self.faulted()
            && !matches!(self.state, RequestState::Preparing | RequestState::Published(_))
            && let Some(generation) = self.confirmed
            && let Some(status) = self.control.sessions.bridge.status()
            && status.generation == generation
        {
            if status.status.capacity_exceeded == 0 {
                self.attempted_latency = 0;
            } else if status.status.required_latency > self.attempted_latency {
                match self.control.request(Arc::clone(&self.requested), self.input.clone()) {
                    Ok(request) => {
                        self.request = request;
                        self.state = RequestState::Preparing;
                        self.attempted_latency = status.status.required_latency;
                        report.latency_refresh = Some(request);
                    }
                    Err(error) => report.refresh_error = Some(error),
                }
            }
        }
        report.preparing = matches!(self.state, RequestState::Preparing | RequestState::Published(_));
        report
    }

    pub fn requested_state(&self) -> (SessionRequestId, RequestState) {
        (self.request, self.state)
    }
    pub fn requested_session(&self) -> &Arc<Session> {
        &self.requested
    }
    pub fn faulted(&self) -> bool {
        self.control.sessions.faulted() || self.sources.faulted()
    }
    /// A device/control failure closes all admission immediately. A running
    /// callback will silence and return work; a stopped device is reclaimed after
    /// quiescence through `retire`. This never locks or borrows the audio holder.
    pub fn fault(&mut self) {
        self.control.sessions.bridge.fault();
        self.sources.fault();
    }
    pub fn send_action(&mut self, payload: T, needs_session: bool) -> Result<Sequence, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.control.send_action(payload, needs_session)
    }
    pub fn send_note(&mut self, payload: T, kind: NoteKind) -> Result<Sequence, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.control.send_note(payload, kind)
    }
    pub fn stop(&mut self) -> Result<Sequence, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.control.stop()
    }
    pub fn all_notes_off(&mut self) -> Result<Sequence, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.control.all_notes_off()
    }
    pub fn register_source(&mut self, port: u16) -> Result<sources::Token, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.sources.register(port).map_err(PlayerError::Source)
    }
    pub fn revoke_source(&mut self, source: sources::Token) -> Result<sources::Revocation, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.sources.revoke(source).map_err(PlayerError::Source)
    }
    pub fn source_admitted(&self, source: sources::Token) -> bool {
        !self.faulted() && self.sources.admits(source)
    }
    pub fn source_status(&self, request: sources::Revocation) -> Result<sources::State, PlayerError> {
        self.sources.status(request).map_err(PlayerError::Source)
    }
    pub fn release_source(&mut self, request: sources::Revocation) -> Result<sources::Receipt, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.sources.release(request).map_err(PlayerError::Source)
    }
    pub fn recovery_progress(&self) -> (Sequence, Sequence) {
        self.control.recovery_progress()
    }
    pub fn poll_notices(&mut self, out: &mut [Option<Notice<T, R>>]) -> usize {
        self.control.poll_notices(out)
    }
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        self.control.sessions.copy_meters(out);
    }

    pub fn binding(&self, track: TrackId, slot: usize) -> Option<SlotBinding> {
        self.handles.binding(track, slot)
    }
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.handles.has_editor(track, slot)
    }
    pub fn open_editor(&mut self, track: TrackId, slot: usize) -> Result<(), String> {
        self.handles.open_editor(track, slot)
    }
    pub fn close_editor(&mut self, track: TrackId, slot: usize) {
        self.handles.close_editor(track, slot);
    }
    pub fn close_editors(&mut self) {
        self.handles.close_editors();
    }
    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.handles.editor_open(track, slot)
    }
    pub fn any_editor_open(&self) -> bool {
        self.handles.any_editor_open()
    }
    pub fn editor_set_param(&mut self, track: TrackId, slot: usize, id: &str, value: f32) {
        self.handles.set_param(track, slot, id, value);
    }
    pub fn capture_states(&mut self) -> PluginStates {
        self.handles.capture_states()
    }
    pub fn idle_editors(&mut self) -> Vec<EditorEdit> {
        self.handles.idle()
    }

    /// Creating thread only, after the device owner has established quiescence.
    pub fn retire(mut self, ports: AudioPorts<T, R>) -> Retirement<T, R> {
        self.handles.close_editors();
        let (notices, released) = self.control.retire(ports.plans, ports.actions);
        self.handles.release(released);
        let sources = self.sources.retire(ports.sources);
        Retirement { notices, sources }
    }
}

#[cfg(test)]
mod tests;
