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
        self.request_session(session, false)
    }

    /// Restore a new document on this device, retaining restore intent through coalescing.
    pub fn replace_session(&mut self, session: Arc<Session>) -> Result<SessionRequestId, PlayerError> {
        self.request_session(session, true)
    }

    fn request_session(&mut self, session: Arc<Session>, replace: bool) -> Result<SessionRequestId, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        let request = if replace {
            self.control.replace_session(Arc::clone(&session), self.input.clone())?
        } else {
            self.control.request(Arc::clone(&session), self.input.clone())?
        };
        self.requested = session;
        self.request = request;
        self.state = RequestState::Preparing;
        self.attempted_latency = 0;
        self.confirmed = None;
        Ok(request)
    }

    /// The path table for the first plan, before anything is requested again.
    pub(super) fn use_io(&mut self, io: Arc<soundcraft_model::IoMap>) {
        self.control.sessions.set_io(io);
    }

    /// Adopt a new path table: a plan for the same document is prepared with it and published
    /// like any other, so the device stays open.
    pub fn set_io(&mut self, io: Arc<soundcraft_model::IoMap>) -> Result<SessionRequestId, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.control.sessions.set_io(io);
        let request = self.control.request(Arc::clone(&self.requested), self.input.clone())?;
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
        self.republish(&mut report);
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

    /// Issue #56: an instance the compile did not predict was made on the plug-in host thread
    /// after its plan was published with the slot vacant. The same document is requested again
    /// (as a latency refresh is: the lineage is kept), and that compile takes the instance.
    fn republish(&mut self, report: &mut ServiceReport) {
        if !self.handles.take_republish() || self.faulted() {
            return;
        }
        if matches!(self.state, RequestState::Preparing | RequestState::Published(_)) {
            // A request is on its way; ask again once it is done.
            self.handles.keep_republish();
            return;
        }
        match self.control.request(Arc::clone(&self.requested), self.input.clone()) {
            Ok(request) => {
                self.request = request;
                self.state = RequestState::Preparing;
                report.preparing = true;
            }
            Err(error) => {
                self.handles.keep_republish();
                report.refresh_error = Some(error);
            }
        }
    }

    pub fn requested_state(&self) -> (SessionRequestId, RequestState) {
        (self.request, self.state)
    }
    pub fn requested_session(&self) -> &Arc<Session> {
        &self.requested
    }
    pub(super) fn failure_signal(&self) -> plan_bridge::RuntimeIdentity {
        self.control.sessions.bridge.runtime_identity()
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
    /// Build the independent native MIDI producer on Main. Its audio endpoint
    /// must join this runtime's anchored bundle and share its callback budget.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn midi_lane(&self) -> Result<super::ingress::Lane, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        Ok(action_lane::pair_with_result(&self.control.sessions.bridge))
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
    pub fn source_permit(&self, source: sources::Token) -> Result<sources::Permit, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.sources.permit(source).map_err(PlayerError::Source)
    }
    pub fn cancel_source_before(&mut self, source: sources::Token, cutoff: u64) -> Result<(), PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        self.sources.cancel_before(source, cutoff).map_err(PlayerError::Source)
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
    pub fn scope_hub(&self) -> Arc<soundcraft_mix::ScopeHub> {
        self.control.sessions.scope_hub()
    }

    pub fn binding(&self, track: TrackId, slot: usize) -> Option<SlotBinding> {
        self.handles.binding(track, slot)
    }
    pub fn loading(&self, track: TrackId, slot: usize) -> bool {
        self.handles.loading(track, slot)
    }
    pub fn any_loading(&self) -> bool {
        self.handles.any_loading()
    }
    pub fn stuck(&self, track: TrackId, slot: usize) -> bool {
        self.handles.stuck(track, slot)
    }
    pub fn any_stuck(&self) -> bool {
        self.handles.any_stuck()
    }
    pub fn plugin_host_wedged(&self) -> usize {
        self.handles.host_wedged()
    }
    pub fn take_editor_errors(&mut self) -> Vec<(TrackId, usize, String)> {
        self.handles.take_editor_errors()
    }
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.handles.has_editor(track, slot)
    }
    pub fn has_key_input(&self, track: TrackId, slot: usize) -> bool {
        self.handles.has_key_input(track, slot)
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
    pub fn set_editor_header(&mut self, track: TrackId, slot: usize, header: &soundcraft_dsp::EditorHeader) {
        self.handles.set_header(track, slot, header);
    }
    pub fn take_header_actions(&mut self) -> Vec<(TrackId, usize, soundcraft_dsp::HeaderAction)> {
        self.handles.take_header_actions()
    }
    pub fn instance(&self, track: TrackId, slot: usize) -> Option<soundcraft_mix::SlotGen> {
        self.handles.binding(track, slot).map(|b| b.2)
    }
    pub fn live_name(&self, track: TrackId, slot: usize) -> Option<&'static str> {
        self.handles.live_name(track, slot)
    }
    pub fn editor_available(&self, track: TrackId, slot: usize) -> Result<(), String> {
        self.handles.editor_available(track, slot)
    }
    pub fn begin_save_state(&mut self, track: TrackId, slot: usize) -> Result<soundcraft_dsp::StateJob<Option<Vec<u8>>>, String> {
        self.handles.begin_save_state(track, slot)
    }
    pub fn begin_load_state(
        &mut self,
        track: TrackId,
        slot: usize,
        data: Vec<u8>,
    ) -> Result<soundcraft_dsp::StateJob<Option<soundcraft_dsp::LoadedValues>>, String> {
        self.handles.begin_load_state(track, slot, data)
    }
    pub fn begin_default_state(&self, id: &str, ch: usize) -> soundcraft_dsp::StateJob<Option<Vec<u8>>> {
        self.handles.begin_default_state(id, ch)
    }
    pub fn editor_set_param(&mut self, track: TrackId, slot: usize, id: &str, value: f32) {
        self.handles.set_param(track, slot, id, value);
    }
    /// Capture only for the exact immutable document whose plan was confirmed.
    /// The caller may already have edited its document without submitting it here;
    /// comparing only the most recent request would then corrupt replacement state.
    /// Internal latency refreshes preserve this document lineage, including refusal.
    pub fn capture_states(&mut self, expected: &Arc<Session>) -> Result<PluginStates, PlayerError> {
        if self.faulted() {
            return Err(PlayerError::Faulted);
        }
        if !Arc::ptr_eq(expected, &self.requested) || self.confirmed != Some(self.control.sessions.bridge.active()) {
            return Err(PlayerError::SessionPending);
        }
        Ok(self.handles.capture_states())
    }
    /// Final Main-thread capture before recreating the device. Faults close audio
    /// admission but leave these handles owned until retirement. Never use them
    /// for a different or unconfirmed document. None means a terminal fault made
    /// that lineage unavailable; the caller must disclose document-only recovery.
    pub fn capture_states_for_reopen(&mut self, expected: &Arc<Session>) -> Result<Option<PluginStates>, PlayerError> {
        if Arc::ptr_eq(expected, &self.requested) && self.confirmed == Some(self.control.sessions.bridge.active()) {
            return Ok(Some(self.handles.capture_states()));
        }
        if self.faulted() {
            return Ok(None);
        }
        Err(PlayerError::SessionPending)
    }
    /// The plug-ins the last state capture left out (their thread was stuck, or held by a modal
    /// dialog past the save deadline; their stored states were kept), once.
    pub fn take_unrefreshed_states(&mut self) -> Vec<String> {
        self.handles.take_unrefreshed()
    }
    /// How long one state capture waits, in all, for plug-ins held by a modal dialog.
    pub fn set_save_deadline(&mut self, deadline: std::time::Duration) {
        self.handles.set_save_deadline(deadline);
    }
    pub fn idle_editors(&mut self) -> Vec<EditorEdit> {
        // Service and drain the old editors, but do not apply their parameter edits
        // to a replacement document before its instances have been adopted.
        let edits = self.handles.idle();
        if self.control.sessions.restore.is_some() { Vec::new() } else { edits }
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
