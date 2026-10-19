//! SoundCraft MIDI device transport (ADR 0012, decision 1).
//!
//! A thin adapter over [midir](https://docs.rs/midir): port enumeration with stable
//! [`PortKey`]s, hot-plug polling ([`PortWatcher`]), input ports that push timestamped raw
//! messages into a preallocated lock-free ring ([`InputConsumer`]), and plain output ports. It
//! owns **no musical timing**: no session or sample mapping, no lookahead, no source generations.
//! Playback's source registry maps arrival times into the clock contract and mints generations.
//!
//! # Threads
//!
//! * **Control** (any non-realtime thread): [`MidiIo`] methods, [`PortWatcher::poll`],
//!   opening, closing and dropping handles, [`OutputHandle::send`]. These allocate, lock and make
//!   OS calls.
//! * **Backend callback** (midir's thread; the WinMM driver callback on Windows): the input push
//!   path. It reads the clock once, splits the bytes into messages and pushes them into the ring.
//!   It does not allocate, lock, log or wait (the crate's tests count its allocations). Ring
//!   full: the message is dropped and counted, never waited for.
//! * **Consumer** (typically the audio thread): [`InputConsumer::pop`], wait-free.
//!
//! # Policies
//!
//! * **SysEx** is dropped and counted ([`InputCounters::sysex_dropped`] and bytes), never carried:
//!   every ring slot stays a fixed 3-byte message. A SysEx path, if needed, belongs on the
//!   control side.
//! * **Release protection**: the ring has `capacity + release_reserve` slots; ordinary messages
//!   stop at `release_reserve` free slots, release-type messages ([`is_release`]) may fill the
//!   rest. Only when the whole ring is full is a release dropped, counted separately in
//!   [`InputCounters::dropped_releases`].
//! * **Order**: events leave the ring in backend delivery order; [`InputEvent::seq`] numbers every
//!   message offered to the ring, so drops show as gaps.
//! * **Scheduled output** is out of scope: [`OutputHandle::send`] sends immediately.
//!
//! # Timestamps
//!
//! [`InputEvent::backend_time_us`] is the backend's own time (B in the clock annex): on Windows,
//! WinMM's `MIM_DATA` milliseconds since `midiInStart`, which midir multiplies by 1000.
//! [`InputEvent::host_arrival_ns`] is host time H at callback entry, from an explicit origin
//! ([`host_origin`] by default, or [`InputConfig::host_origin`]).
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod backend;
mod clock;
mod event;
#[cfg(test)]
mod fake;
mod port;
mod ring;
#[cfg(test)]
mod tests;

pub use clock::{host_now_ns, host_ns_since, host_origin};
pub use event::{InputEvent, is_release};
pub use port::{BackendId, Direction, PortInfo, PortKey};
pub use ring::{InputConfig, InputConsumer, InputCounters};

use backend::{Backend, InputConnection, OutputConnection};
use ring::{InputSink, PortShared};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// Errors from enumeration, opening and sending. All are control side.
#[derive(Debug, thiserror::Error)]
pub enum MidiIoError {
    /// The MIDI API could not be initialized or enumerated.
    #[error("MIDI backend error: {0}")]
    Backend(String),
    /// No port with this key is present now.
    #[error("MIDI port not found: {0}")]
    PortNotFound(PortKey),
    /// The key names a port of another backend.
    #[error("MIDI port {0} belongs to another backend")]
    WrongBackend(PortKey),
    /// The backend refused to open the port (on WinMM, `MMSYSERR_ALLOCATED` when another
    /// program holds it).
    #[error("cannot open MIDI port {key}: {reason}")]
    Open {
        /// The port.
        key: PortKey,
        /// The backend's reason.
        reason: String,
    },
    /// The input configuration cannot work (zero capacity).
    #[error("invalid MIDI input configuration: {0}")]
    InvalidConfig(&'static str),
    /// Sending failed (invalid message, or the device is gone).
    #[error("MIDI send failed: {0}")]
    Send(String),
}

/// Open ports per (direction, key), so a watcher can flag the ones that vanish.
type Registry = Mutex<Vec<(Direction, PortKey, Weak<PortShared>)>>;

struct Inner {
    backend: Box<dyn Backend>,
    open: Registry,
}

/// Entry point: enumerates, opens and watches ports of one backend. Cheap to clone (shared).
///
/// All methods are control side. Handles opened here are flagged disconnected by any
/// [`PortWatcher`] made from the same `MidiIo` (or a clone).
#[derive(Clone)]
pub struct MidiIo {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MidiIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiIo").field("backend", &self.backend()).finish_non_exhaustive()
    }
}

impl MidiIo {
    /// The platform's MIDI API through midir. `client_name` is what the OS shows as the client
    /// (ALSA, CoreMIDI); WinMM ignores it.
    #[must_use]
    pub fn new(client_name: &str) -> Self {
        Self::with_backend(Box::new(backend::Midir { client_name: client_name.to_owned() }))
    }

    pub(crate) fn with_backend(backend: Box<dyn Backend>) -> Self {
        Self { inner: Arc::new(Inner { backend, open: Mutex::new(Vec::new()) }) }
    }

    /// The backend ports come from.
    #[must_use]
    pub fn backend(&self) -> BackendId {
        self.inner.backend.id()
    }

    /// Input ports present now, in enumeration order.
    pub fn list_inputs(&self) -> Result<Vec<PortInfo>, MidiIoError> {
        self.list(Direction::Input)
    }

    /// Output ports present now, in enumeration order.
    pub fn list_outputs(&self) -> Result<Vec<PortInfo>, MidiIoError> {
        self.list(Direction::Output)
    }

    fn list(&self, direction: Direction) -> Result<Vec<PortInfo>, MidiIoError> {
        Ok(port::infos(self.backend(), direction, self.inner.backend.names(direction)?))
    }

    fn check_backend(&self, key: &PortKey) -> Result<(), MidiIoError> {
        if key.backend == self.backend() { Ok(()) } else { Err(MidiIoError::WrongBackend(key.clone())) }
    }

    fn register(&self, direction: Direction, key: &PortKey, shared: &Arc<PortShared>) {
        let mut open = self.inner.open.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|(_, _, w)| w.strong_count() > 0);
        open.push((direction, key.clone(), Arc::downgrade(shared)));
    }

    /// Opens an input port. The backend callback pushes into a ring of
    /// `config.capacity + config.release_reserve` events, allocated here; the returned consumer
    /// reads it. The port stays open until the handle drops.
    pub fn open_input(&self, key: &PortKey, config: InputConfig) -> Result<(InputHandle, InputConsumer), MidiIoError> {
        self.check_backend(key)?;
        if config.capacity == 0 {
            return Err(MidiIoError::InvalidConfig("capacity must be at least 1"));
        }
        let (sink, consumer, shared) = InputSink::new(&config);
        let connection = self.inner.backend.open_input(key, sink)?;
        self.register(Direction::Input, key, &shared);
        Ok((InputHandle { key: key.clone(), shared, connection: Some(connection) }, consumer))
    }

    /// Opens an output port for immediate sends.
    pub fn open_output(&self, key: &PortKey) -> Result<OutputHandle, MidiIoError> {
        self.check_backend(key)?;
        let connection = self.inner.backend.open_output(key)?;
        let shared = Arc::new(PortShared::default());
        self.register(Direction::Output, key, &shared);
        Ok(OutputHandle { key: key.clone(), shared, connection })
    }

    /// A watcher whose first [`PortWatcher::poll`] reports changes since now.
    pub fn watcher(&self) -> Result<PortWatcher, MidiIoError> {
        let mut watcher = PortWatcher { io: self.clone(), known: BTreeMap::new() };
        watcher.known = watcher.snapshot()?;
        Ok(watcher)
    }
}

/// An open input port. Dropping it (or [`InputHandle::close`]) closes the port.
///
/// `Send`: it may be dropped on any control thread, not necessarily the one that opened it, but
/// never on the audio thread: closing makes OS calls and, on WinMM, waits under midir's lock for
/// the driver callback to finish. After the drop returns the callback has stopped and the
/// consumer reports [`InputConsumer::is_closed`].
pub struct InputHandle {
    key: PortKey,
    shared: Arc<PortShared>,
    connection: Option<Box<dyn InputConnection>>,
}

impl InputHandle {
    /// The port.
    #[must_use]
    pub fn key(&self) -> &PortKey {
        &self.key
    }

    /// The port's counters (atomics, readable from any thread).
    #[must_use]
    pub fn counters(&self) -> InputCounters {
        self.shared.snapshot()
    }

    /// A [`PortWatcher`] saw this port vanish. The connection is dead (midir reports no error,
    /// events simply stop): close it and, when the port returns, open it again.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.shared.is_disconnected()
    }

    /// Closes the port now (the same as dropping the handle).
    pub fn close(mut self) {
        self.connection.take();
    }
}

impl std::fmt::Debug for InputHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputHandle").field("key", &self.key).field("open", &self.connection.is_some()).finish_non_exhaustive()
    }
}

/// An open output port for immediate sends. Dropping it closes the port. Control side only;
/// scheduled output (ADR 0012, decision 5) is not part of this crate yet.
pub struct OutputHandle {
    key: PortKey,
    shared: Arc<PortShared>,
    connection: Box<dyn OutputConnection>,
}

impl OutputHandle {
    /// The port.
    #[must_use]
    pub fn key(&self) -> &PortKey {
        &self.key
    }

    /// Sends one complete message (or a whole SysEx) now. Blocking OS call; not for the audio
    /// thread.
    pub fn send(&mut self, message: &[u8]) -> Result<(), MidiIoError> {
        self.connection.send(message)
    }

    /// A [`PortWatcher`] saw this port vanish.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.shared.is_disconnected()
    }
}

impl std::fmt::Debug for OutputHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputHandle").field("key", &self.key).finish_non_exhaustive()
    }
}

/// A port appeared or vanished between two polls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortChange {
    /// A port is present that was not at the previous poll.
    Added(PortInfo),
    /// A port present at the previous poll is gone.
    Removed {
        /// The port as it was listed.
        port: PortInfo,
        /// Handles of this port were open; they are now flagged disconnected.
        was_open: bool,
    },
}

/// Hot-plug detection by polling: midir has no device-change callbacks. A control thread calls
/// [`PortWatcher::poll`] periodically (a few times per second is plenty).
///
/// A port that vanishes and returns between two polls is not seen to change, yet its open
/// handle is then dead; poll more often than a person can unplug and replug.
pub struct PortWatcher {
    io: MidiIo,
    known: BTreeMap<(Direction, PortKey), PortInfo>,
}

impl std::fmt::Debug for PortWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortWatcher").field("ports", &self.known.len()).finish_non_exhaustive()
    }
}

impl PortWatcher {
    fn snapshot(&self) -> Result<BTreeMap<(Direction, PortKey), PortInfo>, MidiIoError> {
        let mut all = self.io.list_inputs()?;
        all.extend(self.io.list_outputs()?);
        Ok(all.into_iter().map(|p| ((p.direction, p.key.clone()), p)).collect())
    }

    /// Enumerates again and reports what changed since the previous poll: removals first, then
    /// additions, each in (direction, key) order. Open handles of removed ports are flagged
    /// ([`InputHandle::is_disconnected`], [`InputConsumer::is_disconnected`]). On an enumeration
    /// error nothing is reported or flagged and the previous state is kept.
    pub fn poll(&mut self) -> Result<Vec<PortChange>, MidiIoError> {
        let now = self.snapshot()?;
        let mut changes = Vec::new();
        let mut open = self.io.inner.open.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|(_, _, w)| w.strong_count() > 0);
        for (id, port) in &self.known {
            if now.contains_key(id) {
                continue;
            }
            let mut was_open = false;
            for (direction, key, weak) in open.iter() {
                if *direction == id.0
                    && *key == id.1
                    && let Some(shared) = weak.upgrade()
                {
                    shared.mark_disconnected();
                    was_open = true;
                }
            }
            changes.push(PortChange::Removed { port: port.clone(), was_open });
        }
        drop(open);
        changes.extend(now.iter().filter(|(id, _)| !self.known.contains_key(*id)).map(|(_, p)| PortChange::Added(p.clone())));
        self.known = now;
        Ok(changes)
    }

    /// The ports present at the last poll (or at creation).
    pub fn ports(&self) -> impl Iterator<Item = &PortInfo> {
        self.known.values()
    }
}
