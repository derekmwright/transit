//! Editor/state-reader ownership follows confirmed SlotBinding, never a slot name.
use super::*;
use crate::{EditorEdit, PluginStates};
use soundcraft_dsp::{PluginEditor, PluginStateReader};
use soundcraft_mix::InstanceSpec;
use soundcraft_model::TrackId;
use std::collections::BTreeMap;

struct Entry {
    editor: Option<Box<dyn PluginEditor>>,
    state: Option<Box<dyn PluginStateReader>>,
    refused: bool,
    /// The plug-in has a sidechain input (read once, at creation).
    key_input: bool,
}

/// Where a third-party instance the next plan needs is on its way to being made.
enum Stage {
    /// Seen in the latest request; not yet announced to the UI.
    Queued,
    /// Announced for a frame (the UI shows it as loading); made on the next service.
    Announced,
    /// Made (or refused: `None`), waiting for the compile to take it.
    Ready(Option<NewInstance>),
}

struct Staged {
    spec: InstanceSpec,
    stage: Stage,
}

#[derive(Default)]
pub struct Handles {
    /// Third-party instances made ahead of the compile, one per service call (see [`Handles::stage`]).
    staged: Vec<Staged>,
    /// Keyed by (track, generation): the generation is the instance's identity and survives a
    /// reorder, while the slot it sits at does not.
    entries: BTreeMap<(TrackId, SlotGen), Entry>,
    active: BTreeMap<(TrackId, usize), SlotGen>,
    generation: PlanGeneration,
    main_thread: PhantomData<Rc<()>>,
}

impl Handles {
    /// Normal Player preparation: all factory/handle calls run on the owner thread.
    pub fn service<T: Copy, R: Copy>(&mut self, control: &mut Control<T, R>) {
        let target = control.sessions.target;
        self.service_with(control, &soundcraft_mix::is_third_party, &mut |spec| {
            soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), target.sample_rate as f32, target.max_block as usize, spec.ch)
        });
    }

    /// [`Self::service`] with the factory and the choice of what is made a frame ahead supplied
    /// (the tests have no plug-in hosts to load).
    pub(super) fn service_with<T: Copy, R: Copy>(
        &mut self,
        control: &mut Control<T, R>,
        deferred: &dyn Fn(&str) -> bool,
        create: &mut dyn FnMut(&InstanceSpec) -> Option<NewInstance>,
    ) {
        control.sessions.reclaim(false);
        control.sessions.hold = self.stage(&mut control.sessions, deferred, create);
        control.service(&mut |spec, generation| {
            let ready = self.staged.iter().position(|st| st.spec == *spec && matches!(st.stage, Stage::Ready(_)));
            let made = match ready.map(|i| self.staged.swap_remove(i).stage) {
                Some(Stage::Ready(made)) => made,
                _ => create(spec),
            }?;
            self.capture((spec.track, spec.slot, generation), made)
        });
        self.refresh(&mut control.sessions);
    }

    /// Makes the third-party instances the latest request needs before it is compiled, so the
    /// compile finds them and the control thread never sits in a factory with the UI unpainted:
    /// the first call only announces them ([`Self::loading`] turns true and the frame is
    /// painted), each later call makes one, and the request compiles in the call that made the
    /// last. The active plan keeps playing meanwhile and has no strip or slot for the new
    /// instance, which is silence. True while the request has to wait.
    ///
    /// Creation is one SDK call on the main thread (VST3 `createInstance` and `initialize`), so a
    /// single instance still blocks this thread for as long as the plug-in takes; what changes is
    /// that nothing else is queued behind it in the same frame and the UI has said why.
    fn stage(
        &mut self,
        sessions: &mut SessionControl,
        deferred: &dyn Fn(&str) -> bool,
        create: &mut dyn FnMut(&InstanceSpec) -> Option<NewInstance>,
    ) -> bool {
        // With a plan in flight the compiler's view is not current; wait for its return.
        let wanted: Vec<InstanceSpec> = match &sessions.latest {
            Some(request) if !sessions.faulted() && sessions.bridge.can_prepare() => {
                let all = sessions.restore.is_some();
                soundcraft_mix::realtime_instance_specs(&request.session)
                    .into_iter()
                    .filter(|spec| deferred(&spec.id) && (all || !sessions.bridge.reuses(spec)))
                    .collect()
            }
            _ => Vec::new(),
        };
        // What no request asks for any more dies here, on main.
        self.staged.retain(|st| wanted.contains(&st.spec));
        for spec in wanted {
            if !self.staged.iter().any(|st| st.spec == spec) {
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
        if let Some(st) = self.staged.iter_mut().find(|st| matches!(st.stage, Stage::Announced)) {
            // A factory unwind is a session fault, as when the compile makes the instance.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| create(&st.spec))) {
                Ok(made) => st.stage = Stage::Ready(made),
                Err(payload) => {
                    st.stage = Stage::Ready(None);
                    sessions.fault();
                    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))) {
                        std::mem::forget(secondary);
                    }
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
        self.entries.insert(key, Entry { editor, state, refused: made.refused_state, key_input });
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
    pub fn capture_states(&mut self) -> PluginStates {
        self.active
            .iter()
            .filter_map(|(&(track, slot), &generation)| {
                let entry = self.entries.get_mut(&(track, generation))?;
                if entry.refused {
                    return None;
                }
                Some((track, slot, entry.state.as_mut()?.save_state()?))
            })
            .collect()
    }
    pub fn idle(&mut self) -> Vec<EditorEdit> {
        let mut out = Vec::new();
        for (&(track, slot), &generation) in &self.active {
            if let Some(entry) = self.entries.get_mut(&(track, generation))
                && let Some(editor) = &mut entry.editor
            {
                out.extend(editor.idle().into_iter().map(|(id, value)| (track, slot, id, value)));
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
