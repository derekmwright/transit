//! The seam between port bookkeeping and an actual MIDI API: midir on the system, a fake in tests.

use crate::MidiIoError;
use crate::port::{BackendId, Direction, PortKey, index_of};
use crate::ring::InputSink;

/// An open input connection. Dropping it closes the port; after the drop returns, the backend
/// calls the sink no more and has dropped it.
pub(crate) trait InputConnection: Send {}

/// An open output connection. Dropping it closes the port.
pub(crate) trait OutputConnection: Send {
    fn send(&mut self, message: &[u8]) -> Result<(), MidiIoError>;
}

/// A MIDI API. Every method is control side and may allocate, lock and block.
pub(crate) trait Backend: Send + Sync {
    fn id(&self) -> BackendId;
    /// Port names in enumeration order.
    fn names(&self, direction: Direction) -> Result<Vec<String>, MidiIoError>;
    /// Opens the input `key` names and hands it the sink; the backend calls `sink.push` from its
    /// callback thread until the connection drops.
    fn open_input(&self, key: &PortKey, sink: InputSink) -> Result<Box<dyn InputConnection>, MidiIoError>;
    fn open_output(&self, key: &PortKey) -> Result<Box<dyn OutputConnection>, MidiIoError>;
}

/// midir with this platform's default API.
pub(crate) struct Midir {
    pub(crate) client_name: String,
}

struct MidirInput(#[allow(dead_code)] midir::MidiInputConnection<InputSink>);
impl InputConnection for MidirInput {}

struct MidirOutput(midir::MidiOutputConnection);
impl OutputConnection for MidirOutput {
    fn send(&mut self, message: &[u8]) -> Result<(), MidiIoError> {
        self.0.send(message).map_err(|e| MidiIoError::Send(e.to_string()))
    }
}

impl Midir {
    fn input(&self) -> Result<midir::MidiInput, MidiIoError> {
        let mut input = midir::MidiInput::new(&self.client_name).map_err(|e| MidiIoError::Backend(e.to_string()))?;
        // Everything reaches the sink, which filters and counts (SysEx, active sensing).
        input.ignore(midir::Ignore::None);
        Ok(input)
    }

    fn output(&self) -> Result<midir::MidiOutput, MidiIoError> {
        midir::MidiOutput::new(&self.client_name).map_err(|e| MidiIoError::Backend(e.to_string()))
    }
}

/// Port names in order; a port whose name cannot be read (it vanished mid-enumeration) is skipped.
fn names_of<P>(ports: &[P], name: impl Fn(&P) -> Option<String>) -> (Vec<String>, Vec<usize>) {
    let mut names = Vec::with_capacity(ports.len());
    let mut indices = Vec::with_capacity(ports.len());
    for (i, p) in ports.iter().enumerate() {
        if let Some(n) = name(p) {
            names.push(n);
            indices.push(i);
        }
    }
    (names, indices)
}

impl Backend for Midir {
    fn id(&self) -> BackendId {
        BackendId::system()
    }

    fn names(&self, direction: Direction) -> Result<Vec<String>, MidiIoError> {
        Ok(match direction {
            Direction::Input => {
                let input = self.input()?;
                names_of(&input.ports(), |p| input.port_name(p).ok()).0
            }
            Direction::Output => {
                let output = self.output()?;
                names_of(&output.ports(), |p| output.port_name(p).ok()).0
            }
        })
    }

    fn open_input(&self, key: &PortKey, sink: InputSink) -> Result<Box<dyn InputConnection>, MidiIoError> {
        let input = self.input()?;
        let ports = input.ports();
        let (names, indices) = names_of(&ports, |p| input.port_name(p).ok());
        let port = index_of(&names, key).and_then(|i| indices.get(i)).and_then(|&i| ports.get(i));
        let port = port.ok_or_else(|| MidiIoError::PortNotFound(key.clone()))?;
        let connection = input
            .connect(port, &self.client_name, |time_us, bytes, sink: &mut InputSink| sink.push(time_us, bytes), sink)
            .map_err(|e| MidiIoError::Open { key: key.clone(), reason: e.to_string() })?;
        Ok(Box::new(MidirInput(connection)))
    }

    fn open_output(&self, key: &PortKey) -> Result<Box<dyn OutputConnection>, MidiIoError> {
        let output = self.output()?;
        let ports = output.ports();
        let (names, indices) = names_of(&ports, |p| output.port_name(p).ok());
        let port = index_of(&names, key).and_then(|i| indices.get(i)).and_then(|&i| ports.get(i));
        let port = port.ok_or_else(|| MidiIoError::PortNotFound(key.clone()))?;
        let connection = output.connect(port, &self.client_name).map_err(|e| MidiIoError::Open { key: key.clone(), reason: e.to_string() })?;
        Ok(Box::new(MidirOutput(connection)))
    }
}
