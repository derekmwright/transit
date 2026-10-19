//! An in-process backend for tests: ports appear and vanish on command, and the test plays the
//! backend callback by calling [`Fake::deliver`].

use crate::backend::{Backend, InputConnection, OutputConnection};
use crate::port::{BackendId, Direction, PortKey, index_of};
use crate::ring::InputSink;
use crate::{MidiIo, MidiIoError};
use std::sync::{Arc, Mutex, PoisonError};

type Slot = Arc<Mutex<Option<InputSink>>>;

#[derive(Default)]
struct State {
    inputs: Vec<(String, Slot)>,
    outputs: Vec<(String, Arc<Mutex<Vec<Vec<u8>>>>)>,
}

/// Test-side control of a fake backend.
#[derive(Clone, Default)]
pub(crate) struct Fake {
    state: Arc<Mutex<State>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Fake {
    pub(crate) fn io(&self) -> MidiIo {
        MidiIo::with_backend(Box::new(self.clone()))
    }

    pub(crate) fn key(name: &str, ordinal: u32) -> PortKey {
        PortKey { backend: BackendId::Fake, name: name.to_owned(), ordinal }
    }

    pub(crate) fn add_input(&self, name: &str) {
        lock(&self.state).inputs.push((name.to_owned(), Arc::default()));
    }

    pub(crate) fn add_output(&self, name: &str) {
        lock(&self.state).outputs.push((name.to_owned(), Arc::default()));
    }

    /// Unplugs the input `key`; an open connection keeps its (now silent) sink until dropped.
    pub(crate) fn remove_input(&self, key: &PortKey) {
        let mut state = lock(&self.state);
        let names: Vec<String> = state.inputs.iter().map(|(n, _)| n.clone()).collect();
        if let Some(i) = index_of(&names, key) {
            state.inputs.remove(i);
        }
    }

    /// The input `key`'s slot: whatever sink is connected to it.
    pub(crate) fn input_slot(&self, key: &PortKey) -> Option<Slot> {
        let state = lock(&self.state);
        let names: Vec<String> = state.inputs.iter().map(|(n, _)| n.clone()).collect();
        index_of(&names, key).and_then(|i| state.inputs.get(i)).map(|(_, s)| Arc::clone(s))
    }

    /// Plays the backend callback for the input `key`. Returns whether a port was open.
    pub(crate) fn deliver(slot: &Slot, backend_time_us: u64, bytes: &[u8]) -> bool {
        match lock(slot).as_mut() {
            Some(sink) => {
                sink.push(backend_time_us, bytes);
                true
            }
            None => false,
        }
    }

    pub(crate) fn sent(&self, key: &PortKey) -> Vec<Vec<u8>> {
        let state = lock(&self.state);
        let names: Vec<String> = state.outputs.iter().map(|(n, _)| n.clone()).collect();
        index_of(&names, key).and_then(|i| state.outputs.get(i)).map(|(_, s)| lock(s).clone()).unwrap_or_default()
    }
}

struct FakeInput(Slot);
impl InputConnection for FakeInput {}
impl Drop for FakeInput {
    fn drop(&mut self) {
        // Closing drops the sink (and with it the ring's producer), as midir's close does.
        lock(&self.0).take();
    }
}

struct FakeOutput(Arc<Mutex<Vec<Vec<u8>>>>);
impl OutputConnection for FakeOutput {
    fn send(&mut self, message: &[u8]) -> Result<(), MidiIoError> {
        lock(&self.0).push(message.to_vec());
        Ok(())
    }
}

impl Backend for Fake {
    fn id(&self) -> BackendId {
        BackendId::Fake
    }

    fn names(&self, direction: Direction) -> Result<Vec<String>, MidiIoError> {
        let state = lock(&self.state);
        Ok(match direction {
            Direction::Input => state.inputs.iter().map(|(n, _)| n.clone()).collect(),
            Direction::Output => state.outputs.iter().map(|(n, _)| n.clone()).collect(),
        })
    }

    fn open_input(&self, key: &PortKey, sink: InputSink) -> Result<Box<dyn InputConnection>, MidiIoError> {
        let slot = self.input_slot(key).ok_or_else(|| MidiIoError::PortNotFound(key.clone()))?;
        let mut held = lock(&slot);
        if held.is_some() {
            return Err(MidiIoError::Open { key: key.clone(), reason: "already open".into() });
        }
        *held = Some(sink);
        drop(held);
        Ok(Box::new(FakeInput(slot)))
    }

    fn open_output(&self, key: &PortKey) -> Result<Box<dyn OutputConnection>, MidiIoError> {
        let state = lock(&self.state);
        let names: Vec<String> = state.outputs.iter().map(|(n, _)| n.clone()).collect();
        let sent = index_of(&names, key).and_then(|i| state.outputs.get(i)).map(|(_, s)| Arc::clone(s));
        let sent = sent.ok_or_else(|| MidiIoError::PortNotFound(key.clone()))?;
        Ok(Box::new(FakeOutput(sent)))
    }
}
