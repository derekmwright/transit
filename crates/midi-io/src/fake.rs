//! An in-process backend for tests (cargo feature `fake`, and always in this crate's own tests):
//! ports appear and vanish on command, and the test plays the backend callback.
//!
//! ```
//! # #[cfg(feature = "fake")] {
//! use soundcraft_midi_io::InputConfig;
//! use soundcraft_midi_io::fake::Fake;
//!
//! let fake = Fake::default();
//! let key = fake.add_input("Keys"); // a key with a backend id, as on WinMM
//! let io = fake.io(); // a `MidiIo` driven by this fake
//! let (handle, mut consumer) = io.open_input(&key, InputConfig::default()).unwrap();
//! let port = fake.input_port(&key).unwrap();
//! assert!(port.deliver(1000, &[0x90, 60, 100])); // plays the backend callback
//! assert_eq!(consumer.pop().unwrap().message(), &[0x90, 60, 100][..]);
//! fake.remove_input(&key); // unplug; a `PortWatcher` poll flags `handle`
//! # drop(handle);
//! # }
//! ```
//!
//! API: [`Fake::io`] makes the [`MidiIo`]; [`Fake::add_input`] / [`Fake::add_output`] plug a port
//! in (with a fresh unique id) and return its [`PortKey`]; [`Fake::add_input_anonymous`] /
//! [`Fake::add_output_anonymous`] plug one in with no id, like a backend with unstable ids
//! (identity falls back to name plus ordinal); [`Fake::remove_input`] / [`Fake::remove_output`]
//! unplug (an open connection keeps its now silent sink until dropped); [`Fake::input_port`]
//! gives the [`FakeInputPort`] whose [`deliver`](FakeInputPort::deliver) plays the backend
//! callback (it stays usable after the unplug, like a dying driver thread); [`Fake::sent`]
//! returns what was sent to an output; [`Fake::key`] builds an id-less key.

use crate::backend::{Backend, InputConnection, OutputConnection};
use crate::port::{BackendId, Direction, PortKey, RawPort, resolve};
use crate::ring::InputSink;
use crate::{MidiIo, MidiIoError};
use std::sync::{Arc, Mutex, PoisonError};

type Slot = Arc<Mutex<Option<InputSink>>>;
type Sent = Arc<Mutex<Vec<Vec<u8>>>>;

struct Port<T> {
    raw: RawPort,
    data: T,
}

#[derive(Default)]
struct State {
    inputs: Vec<Port<Slot>>,
    outputs: Vec<Port<Sent>>,
    next_id: u64,
}

impl State {
    fn fresh_id(&mut self, direction: &str) -> String {
        self.next_id += 1;
        format!("fake-{direction}-{}", self.next_id)
    }
}

/// Test-side control of a fake backend. Clones share the same ports.
#[derive(Clone, Default)]
pub struct Fake {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Fake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fake").finish_non_exhaustive()
    }
}

/// The backend side of one fake input port: plays the backend callback into whatever sink is
/// connected to it.
#[derive(Clone)]
pub struct FakeInputPort(Slot);

impl FakeInputPort {
    #[cfg(test)]
    pub(crate) fn with_sink<R>(&self, f: impl FnOnce(&mut InputSink) -> R) -> Option<R> {
        lock(&self.0).as_mut().map(f)
    }

    /// Plays the backend callback: hands `bytes` (one or more messages) stamped `backend_time_us`
    /// to the connected sink. Returns whether a connection was open.
    pub fn deliver(&self, backend_time_us: u64, bytes: &[u8]) -> bool {
        match lock(&self.0).as_mut() {
            Some(sink) => {
                sink.push(backend_time_us, bytes);
                true
            }
            None => false,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn raws<T>(ports: &[Port<T>]) -> Vec<RawPort> {
    ports.iter().map(|p| p.raw.clone()).collect()
}

fn key_at<T>(ports: &[Port<T>], index: usize) -> PortKey {
    let name = ports.get(index).map(|p| p.raw.name.clone()).unwrap_or_default();
    let id = ports.get(index).map(|p| p.raw.id.clone()).unwrap_or_default();
    let ordinal =
        u32::try_from(ports.iter().take(index).filter(|p| p.raw.name == name && (!id.is_empty() || p.raw.id.is_empty())).count()).unwrap_or(u32::MAX);
    PortKey { backend: BackendId::Fake, name, ordinal, id }
}

impl Fake {
    /// A [`MidiIo`] over this fake backend.
    #[must_use]
    pub fn io(&self) -> MidiIo {
        MidiIo::with_backend(Box::new(self.clone()))
    }

    /// A key with no backend id: name plus ordinal identity.
    #[must_use]
    pub fn key(name: &str, ordinal: u32) -> PortKey {
        PortKey { backend: BackendId::Fake, name: name.to_owned(), ordinal, id: String::new() }
    }

    /// Plugs in an input with a fresh unique id and returns its key.
    pub fn add_input(&self, name: &str) -> PortKey {
        let mut state = lock(&self.state);
        let id = state.fresh_id("in");
        Self::push_input(&mut state, name, id)
    }

    /// Plugs in an input with no id (a backend with unstable ids) and returns its key.
    pub fn add_input_anonymous(&self, name: &str) -> PortKey {
        Self::push_input(&mut lock(&self.state), name, String::new())
    }

    fn push_input(state: &mut State, name: &str, id: String) -> PortKey {
        state.inputs.push(Port { raw: RawPort { name: name.to_owned(), id }, data: Arc::default() });
        key_at(&state.inputs, state.inputs.len().saturating_sub(1))
    }

    /// Plugs in an output with a fresh unique id and returns its key.
    pub fn add_output(&self, name: &str) -> PortKey {
        let mut state = lock(&self.state);
        let id = state.fresh_id("out");
        Self::push_output(&mut state, name, id)
    }

    /// Plugs in an output with no id and returns its key.
    pub fn add_output_anonymous(&self, name: &str) -> PortKey {
        Self::push_output(&mut lock(&self.state), name, String::new())
    }

    fn push_output(state: &mut State, name: &str, id: String) -> PortKey {
        state.outputs.push(Port { raw: RawPort { name: name.to_owned(), id }, data: Arc::default() });
        key_at(&state.outputs, state.outputs.len().saturating_sub(1))
    }

    /// Unplugs the input `key` names; an open connection keeps its (now silent) sink until dropped.
    pub fn remove_input(&self, key: &PortKey) {
        let mut state = lock(&self.state);
        if let Some(i) = resolve(&raws(&state.inputs), key) {
            state.inputs.remove(i);
        }
    }

    /// Unplugs the output `key` names.
    pub fn remove_output(&self, key: &PortKey) {
        let mut state = lock(&self.state);
        if let Some(i) = resolve(&raws(&state.outputs), key) {
            state.outputs.remove(i);
        }
    }

    /// The backend side of the input `key` names, if it is present now.
    #[must_use]
    pub fn input_port(&self, key: &PortKey) -> Option<FakeInputPort> {
        self.slot(key).map(|(_, slot)| FakeInputPort(slot))
    }

    fn slot(&self, key: &PortKey) -> Option<(PortKey, Slot)> {
        let state = lock(&self.state);
        let at = resolve(&raws(&state.inputs), key)?;
        Some((key_at(&state.inputs, at), Arc::clone(&state.inputs.get(at)?.data)))
    }

    /// Everything sent to the output `key` names.
    #[must_use]
    pub fn sent(&self, key: &PortKey) -> Vec<Vec<u8>> {
        let state = lock(&self.state);
        resolve(&raws(&state.outputs), key).and_then(|i| state.outputs.get(i)).map(|p| lock(&p.data).clone()).unwrap_or_default()
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

struct FakeOutput(Sent);
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

    fn ports(&self, direction: Direction) -> Result<Vec<RawPort>, MidiIoError> {
        let state = lock(&self.state);
        Ok(match direction {
            Direction::Input => raws(&state.inputs),
            Direction::Output => raws(&state.outputs),
        })
    }

    fn open_input(&self, key: &PortKey, sink: InputSink) -> Result<(PortKey, Box<dyn InputConnection>), MidiIoError> {
        let (full, slot) = self.slot(key).ok_or_else(|| MidiIoError::PortNotFound(key.clone()))?;
        let mut held = lock(&slot);
        if held.is_some() {
            return Err(MidiIoError::Open { key: key.clone(), reason: "already open".into() });
        }
        *held = Some(sink);
        drop(held);
        Ok((full, Box::new(FakeInput(slot))))
    }

    fn open_output(&self, key: &PortKey) -> Result<(PortKey, Box<dyn OutputConnection>), MidiIoError> {
        let state = lock(&self.state);
        let at = resolve(&raws(&state.outputs), key).ok_or_else(|| MidiIoError::PortNotFound(key.clone()))?;
        let sent = state.outputs.get(at).map(|p| Arc::clone(&p.data)).ok_or_else(|| MidiIoError::PortNotFound(key.clone()))?;
        Ok((key_at(&state.outputs, at), Box::new(FakeOutput(sent))))
    }
}
