//! Editor/state-reader ownership follows confirmed SlotBinding, never a slot name.
//!
//! Hosted (third-party) instances are made on the plug-in host thread (ADR 0018,
//! `transit-plugin-host`): a request is announced for a frame ("Loading…"), submitted, and polled
//! once per service call; the compile takes the instance when it is ready. The editor and state
//! handles captured here are the host's proxies, so opening an editor, idling it and reading a
//! state go to the host thread too, and a retired instance is destroyed there.
use super::*;
use crate::{EditorEdit, PluginStates};
use soundcraft_dsp::{PluginEditor, PluginStateReader, StateBusy};
use soundcraft_mix::InstanceSpec;
use soundcraft_model::TrackId;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use transit_plugin_host::{Ctx, Host, Pending, Status};

/// How long one save waits for every plug-in's state in all (issue #56). A plug-in that has not
/// answered by then keeps its stored state and the status line names it.
pub const DEFAULT_SAVE_DEADLINE: Duration = Duration::from_millis(500);

/// Makes one instance for a spec, on whichever thread runs it (the plug-in host thread for hosted
/// plug-ins).
pub(crate) type Factory = Arc<dyn Fn(&InstanceSpec) -> Option<NewInstance> + Send + Sync>;

/// What a creation job on the host thread returns: `Err` when the factory unwound.
type Made = Result<Option<NewInstance>, ()>;

/// The job that makes `spec` on the host thread and adopts the instance there.
fn creation(spec: InstanceSpec, factory: Factory) -> impl FnOnce(&mut Ctx) -> Made + Send + 'static {
    move |ctx| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory(&spec).map(|mut made| {
                made.plugin = ctx.adopt(made.plugin);
                made
            })
        }))
        .map_err(|payload| {
            if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
            }
        })
    }
}

struct Entry {
    editor: Option<Box<dyn PluginEditor>>,
    state: Option<Box<dyn PluginStateReader>>,
    refused: bool,
    /// The plug-in has a sidechain input (read once, at creation).
    key_input: bool,
    /// The plug-in's name, for the status line.
    name: &'static str,
}

/// Where a third-party instance the next plan needs is on its way to being made.
enum Stage {
    /// Seen in the latest request; not yet announced to the UI.
    Queued,
    /// Announced for a frame (the UI shows it as loading); submitted on the next service.
    Announced,
    /// Being made on the plug-in host thread.
    Requested(Pending<Made>),
    /// Made (or refused, failed or stuck: `None`), waiting for the compile to take it.
    Ready(Option<NewInstance>),
}

struct Staged {
    spec: InstanceSpec,
    stage: Stage,
}

pub struct Handles {
    /// Third-party instances made ahead of the compile on the host thread (see [`Handles::stage`]).
    staged: Vec<Staged>,
    /// Specs whose creation did not return within the host's timeout: the slot is empty and the
    /// UI says "Not responding" until the request no longer has it.
    stuck: Vec<InstanceSpec>,
    /// Why editors that accepted an open request then failed, for the status line.
    editor_errors: Vec<(TrackId, usize, String)>,
    /// How long one [`Self::capture_states`] may wait for plug-ins in all.
    save_deadline: Duration,
    /// The plug-ins whose state the last capture could not read in time (their stored state is
    /// kept), for the status line.
    unrefreshed: Vec<String>,
    host: Host,
    /// Keyed by (track, generation): the generation is the instance's identity and survives a
    /// reorder, while the slot it sits at does not.
    entries: BTreeMap<(TrackId, SlotGen), Entry>,
    active: BTreeMap<(TrackId, usize), SlotGen>,
    generation: PlanGeneration,
    main_thread: PhantomData<Rc<()>>,
}

impl Default for Handles {
    fn default() -> Self {
        Handles::with_host(transit_plugin_host::shared().clone())
    }
}

impl Handles {
    /// Handles whose hosted instances are made on `host` (tests use their own).
    pub(crate) fn with_host(host: Host) -> Self {
        Handles {
            staged: Vec::new(),
            stuck: Vec::new(),
            editor_errors: Vec::new(),
            save_deadline: DEFAULT_SAVE_DEADLINE,
            unrefreshed: Vec::new(),
            host,
            entries: BTreeMap::new(),
            active: BTreeMap::new(),
            generation: PlanGeneration::default(),
            main_thread: PhantomData,
        }
    }

    /// Normal Player preparation: hosted instances are made on the plug-in host thread,
    /// built-ins (cheap) on this, the owner thread, during the compile.
    pub fn service<T: Copy, R: Copy>(&mut self, control: &mut Control<T, R>) {
        let target = control.sessions.target;
        let factory: Factory = Arc::new(move |spec: &InstanceSpec| {
            soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), target.sample_rate as f32, target.max_block as usize, spec.ch)
        });
        self.service_with(control, &soundcraft_mix::is_third_party, &factory);
    }

    /// [`Self::service`] with the factory and the choice of what goes to the host thread
    /// supplied (the tests have no plug-in hosts to load).
    pub(super) fn service_with<T: Copy, R: Copy>(&mut self, control: &mut Control<T, R>, deferred: &dyn Fn(&str) -> bool, factory: &Factory) {
        self.host.check();
        control.sessions.reclaim(false);
        control.sessions.hold = self.stage(&mut control.sessions, deferred, factory);
        control.service(&mut |spec, generation| {
            let ready = self.staged.iter().position(|st| st.spec == *spec && matches!(st.stage, Stage::Ready(_)));
            let made = match ready.map(|i| self.staged.swap_remove(i).stage) {
                Some(Stage::Ready(made)) => made,
                _ if self.stuck.contains(spec) => None,
                // Not predicted by `stage` (a slot that moved for layout reasons): made now on
                // the host thread, waited for up to its timeout.
                _ if deferred(&spec.id) => self.make_now(spec, factory),
                _ => factory(spec),
            }?;
            self.capture((spec.track, spec.slot, generation), made)
        });
        self.refresh(&mut control.sessions);
    }

    /// Makes a hosted instance the compile asks for that `stage` did not predict: on the host
    /// thread, waiting at most the host's timeout. A factory unwind unwinds the compile (a
    /// session fault, as before); a stuck creation leaves the slot empty and marked.
    fn make_now(&mut self, spec: &InstanceSpec, factory: &Factory) -> Option<NewInstance> {
        let pending = self.host.submit(creation(spec.clone(), Arc::clone(factory)));
        let timeout = self.host.timeout();
        match pending.wait(timeout) {
            Some(Ok(made)) => made,
            Some(Err(())) => std::panic::resume_unwind(Box::new("a plug-in factory panicked on the host thread")),
            None => {
                log::error!("{}: not made within {timeout:?}; the slot stays empty", spec.id);
                self.stuck.push(spec.clone());
                None
            }
        }
    }

    /// Makes the third-party instances the latest request needs before it is compiled, so the
    /// compile finds them and the control thread never sits in a factory: the first call only
    /// announces them ([`Self::loading`] turns true and the frame is painted), the next submits
    /// them to the plug-in host thread, and each later call polls them; the request compiles in
    /// the call that finds the last one ready. The active plan keeps playing meanwhile and has no
    /// strip or slot for the new instance, which is silence. True while the request has to wait.
    ///
    /// A creation that runs past the host's timeout is stuck (ADR 0018 section 5): its slot is
    /// compiled empty and reads "Not responding" ([`Self::stuck`]) while the request keeps it.
    fn stage(&mut self, sessions: &mut SessionControl, deferred: &dyn Fn(&str) -> bool, factory: &Factory) -> bool {
        // With a plan in flight the compiler's view is not current; wait for its return.
        let wanted: Vec<InstanceSpec> = match &sessions.latest {
            Some(request) if !sessions.faulted() && sessions.bridge.can_prepare() => {
                let all = sessions.restore.is_some();
                let specs = soundcraft_mix::realtime_instance_specs(&request.session);
                // A stuck mark lasts while the request still has that instance.
                self.stuck.retain(|spec| specs.contains(spec));
                specs.into_iter().filter(|spec| deferred(&spec.id) && (all || !sessions.bridge.reuses(spec))).collect()
            }
            _ => Vec::new(),
        };
        // What no request asks for any more dies here: a request not yet started is cancelled,
        // a running one finishes on the host thread and its instance is destroyed there.
        self.staged.retain(|st| wanted.contains(&st.spec));
        for spec in wanted {
            if !self.staged.iter().any(|st| st.spec == spec) {
                // Asked for afresh (a reopen): it gets another try.
                self.stuck.retain(|s| *s != spec);
                self.staged.push(Staged { spec, stage: Stage::Queued });
            }
        }
        if self.staged.iter().any(|st| matches!(st.stage, Stage::Queued)) {
            for st in &mut self.staged {
                if matches!(st.stage, Stage::Queued) {
                    st.stage = Stage::Announced;
                }
            }
            return true;
        }
        // Inline (macOS) runs each job where it is submitted: one per call, as before.
        let inline = self.host.is_inline();
        for st in self.staged.iter_mut().filter(|st| matches!(st.stage, Stage::Announced)) {
            st.stage = Stage::Requested(self.host.submit(creation(st.spec.clone(), Arc::clone(factory))));
            if inline {
                break;
            }
        }
        for st in &mut self.staged {
            let Stage::Requested(pending) = &st.stage else { continue };
            match pending.poll() {
                Status::Waiting { .. } => {}
                Status::Ready(Ok(made)) => st.stage = Stage::Ready(made),
                Status::Ready(Err(())) => {
                    // A factory unwind is a session fault, as when the compile makes the instance.
                    st.stage = Stage::Ready(None);
                    sessions.fault();
                }
                Status::Failed => {
                    log::error!("{}: the plug-in host could not make it", st.spec.id);
                    st.stage = Stage::Ready(None);
                }
                Status::Stuck => {
                    log::error!("{}: not made within {:?}; the slot stays empty", st.spec.id, self.host.timeout());
                    self.stuck.push(st.spec.clone());
                    st.stage = Stage::Ready(None);
                }
            }
        }
        self.staged.iter().any(|st| !matches!(st.stage, Stage::Ready(_)))
    }

    /// Whether the instance for this slot is announced or being made, and not yet in a plan.
    pub fn loading(&self, track: TrackId, slot: usize) -> bool {
        self.staged.iter().any(|st| st.spec.track == track && st.spec.slot == slot && !matches!(st.stage, Stage::Ready(_)))
    }

    /// Whether any instance is waiting to be made.
    pub fn any_loading(&self) -> bool {
        self.staged.iter().any(|st| !matches!(st.stage, Stage::Ready(_)))
    }

    /// Whether this slot's plug-in did not finish loading within the host's timeout: the slot is
    /// empty and the UI says it is not responding.
    pub fn stuck(&self, track: TrackId, slot: usize) -> bool {
        self.stuck.iter().any(|spec| spec.track == track && spec.slot == slot)
    }

    /// Whether any slot is stuck.
    pub fn any_stuck(&self) -> bool {
        !self.stuck.is_empty()
    }

    /// How many plug-in host threads are not responding now (the status line says so); it goes
    /// back down when a thread returns from the long call.
    pub fn host_wedged(&self) -> usize {
        self.host.wedged()
    }

    /// The plug-in host these handles use (the tests stall it).
    #[cfg(test)]
    pub(crate) fn host(&self) -> &Host {
        &self.host
    }

    /// Editor opens that failed after they were accepted: (track, slot, why), each once.
    pub fn take_editor_errors(&mut self) -> Vec<(TrackId, usize, String)> {
        std::mem::take(&mut self.editor_errors)
    }

    /// Take every possibly-panicking handle before committing the entry. On a factory
    /// unwind, local resources die on main and there is no partial visible binding.
    pub fn capture(&mut self, binding: SlotBinding, mut made: NewInstance) -> Option<NewInstance> {
        let key = (binding.0, binding.2);
        if self.entries.contains_key(&key) {
            return None;
        }
        let editor = made.plugin.editor();
        let state = made.plugin.state_reader();
        let key_input = made.plugin.has_key_input();
        let name = made.plugin.info().name;
        self.entries.insert(key, Entry { editor, state, refused: made.refused_state, key_input, name });
        Some(made)
    }

    pub fn refresh(&mut self, sessions: &mut SessionControl) {
        self.release(sessions.take_released());
        let generation = sessions.bridge.active();
        if generation != self.generation {
            self.active.clear();
            for (track, slot, generation) in sessions.active_bindings() {
                self.active.insert((track, slot), generation);
            }
            self.generation = generation;
        }
    }

    pub fn release(&mut self, bindings: impl IntoIterator<Item = SlotBinding>) {
        for binding in bindings {
            if let Some(mut entry) = self.entries.remove(&(binding.0, binding.2))
                && let Some(editor) = &mut entry.editor
            {
                editor.close();
            }
            self.active.retain(|&(track, _), generation| (track, *generation) != (binding.0, binding.2));
        }
    }

    pub fn binding(&self, track: TrackId, slot: usize) -> Option<SlotBinding> {
        self.active.get(&(track, slot)).map(|generation| (track, slot, *generation))
    }
    fn entry(&self, track: TrackId, slot: usize) -> Option<&Entry> {
        let binding = self.binding(track, slot)?;
        self.entries.get(&(binding.0, binding.2))
    }
    fn entry_mut(&mut self, track: TrackId, slot: usize) -> Option<&mut Entry> {
        let binding = self.binding(track, slot)?;
        self.entries.get_mut(&(binding.0, binding.2))
    }
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.entry(track, slot).is_some_and(|entry| entry.editor.is_some())
    }
    /// Whether the live plug-in in this slot has a sidechain input.
    pub fn has_key_input(&self, track: TrackId, slot: usize) -> bool {
        self.entry(track, slot).is_some_and(|entry| entry.key_input)
    }
    pub fn open_editor(&mut self, track: TrackId, slot: usize) -> Result<(), String> {
        let entry = self.entry_mut(track, slot).ok_or_else(|| "this plugin instance is not active".to_owned())?;
        let editor = entry.editor.as_mut().ok_or_else(|| "this plugin has no editor".to_owned())?;
        editor.open()?;
        entry.refused = false;
        Ok(())
    }
    pub fn close_editor(&mut self, track: TrackId, slot: usize) {
        if let Some(entry) = self.entry_mut(track, slot)
            && let Some(editor) = &mut entry.editor
        {
            editor.close();
        }
    }
    pub fn close_editors(&mut self) {
        for entry in self.entries.values_mut() {
            if let Some(editor) = &mut entry.editor {
                editor.close();
            }
        }
    }
    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.entry(track, slot).and_then(|entry| entry.editor.as_ref()).is_some_and(|editor| editor.is_open())
    }
    pub fn any_editor_open(&self) -> bool {
        self.active.keys().any(|&(track, slot)| self.editor_open(track, slot))
    }
    pub fn set_param(&mut self, track: TrackId, slot: usize, id: &str, value: f32) {
        if let Some(entry) = self.entry_mut(track, slot)
            && let Some(editor) = &mut entry.editor
        {
            editor.set_param(id, value);
        }
    }
    /// Reads every live plug-in's state for a save, waiting at most the save deadline in all
    /// (issue #56): a plug-in whose thread is inside a long call or a modal dialog is left out,
    /// so the caller keeps its stored state, and is named by [`Self::take_unrefreshed`].
    pub fn capture_states(&mut self) -> PluginStates {
        let deadline = Instant::now() + self.save_deadline;
        let mut unrefreshed = Vec::new();
        let states = self
            .active
            .iter()
            .filter_map(|(&(track, slot), &generation)| {
                let entry = self.entries.get_mut(&(track, generation))?;
                if entry.refused {
                    return None;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                match entry.state.as_mut()?.save_state_within(left) {
                    Ok(state) => Some((track, slot, state?)),
                    Err(StateBusy) => {
                        log::warn!("{}: state not read within the save deadline; its stored state is kept", entry.name);
                        unrefreshed.push(entry.name.to_owned());
                        None
                    }
                }
            })
            .collect();
        self.unrefreshed = unrefreshed;
        states
    }

    /// The plug-ins the last [`Self::capture_states`] could not read in time, once.
    pub fn take_unrefreshed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unrefreshed)
    }

    /// How long one save waits for plug-in states in all ([`DEFAULT_SAVE_DEADLINE`]).
    pub fn set_save_deadline(&mut self, deadline: Duration) {
        self.save_deadline = deadline;
    }
    pub fn idle(&mut self) -> Vec<EditorEdit> {
        let mut out = Vec::new();
        for (&(track, slot), &generation) in &self.active {
            if let Some(entry) = self.entries.get_mut(&(track, generation))
                && let Some(editor) = &mut entry.editor
            {
                out.extend(editor.idle().into_iter().map(|(id, value)| (track, slot, id, value)));
                if let Some(e) = editor.take_error() {
                    self.editor_errors.push((track, slot, e));
                }
            }
        }
        out
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        self.close_editors();
    }
}
