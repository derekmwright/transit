//! External MIDI output (#61): a dedicated thread owns every open output port and sends
//! timestamped events at their wall-clock instant.
//!
//! Two lock-free SPSC rings feed it, both preallocated before the producers run:
//!
//! * **playback** (audio thread): clip events of tracks with an external destination, stamped
//!   with the host time H at which the audio they belong with is presented, plus `Release`
//!   (loop wrap, locate) and `Stop` markers. Pushing is wait-free and never allocates; a full
//!   ring drops the event and counts it, keeping a reserve for releases.
//! * **echo** (MIDI ingress thread): live input on armed or monitored tracks with an external
//!   destination, forwarded as soon as the ingress worker pops it, never through the audio thread.
//!
//! The audio thread never calls into `transit-midi-io`; only this thread does. Control (Main)
//! sends the destination table and panics through a bounded channel and reads a status
//! snapshot. Timing model, safety rules and reconnects: `docs/contributing/knowledge/midi-output.md`.
use crate::device_clock::HostOrigin;
use soundcraft_model::{MidiOutChannel, MidiOutPort, Session, TrackId};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use transit_midi_io::{Direction, MidiIo, OutputHandle, PortInfo, PortKey, PortWatcher};

/// Events each ring holds. At 3125 messages a second (a saturated DIN link) this is over a
/// second of backlog; playback renders at most a few blocks ahead of presentation.
pub const QUEUE: usize = 4096;
/// Ring slots only releases (note-offs, pedal-ups, Release/Stop markers) may use.
const RELEASE_RESERVE: usize = 256;
/// The most events waiting for their send instant; past it new non-release events are dropped.
const PENDING: usize = 4 * QUEUE;
const PORT_POLL: Duration = Duration::from_millis(250);
/// The thread sleeps until this long before a deadline and yields for the rest.
const SPIN_NS: u64 = 300_000;
/// A send this late against its deadline counts as late.
const LATE_NS: u64 = 2_000_000;
/// Destinations in one table; tracks beyond it get no external output.
pub const MAX_DESTINATIONS: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// A MIDI message for `track`.
    #[default]
    Message,
    /// A loop wrap or locate at `at_ns`: end every note the playback path holds (#47's rule;
    /// the next lap starts the notes that sound across its start again).
    Release,
    /// The transport stopped: drop scheduled playback, then release everything on every
    /// destination (note-offs, sustain off, All Notes Off per used channel).
    Stop,
}

/// One entry of either ring. Copy and fixed size, so pushing never allocates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutEvent {
    pub kind: Kind,
    /// `TrackId.0` (unused for markers).
    pub track: u64,
    /// Host time H (the runtime's [`HostOrigin`]) the event belongs to, before the destination
    /// offset; 0 means "now". Echo events carry their arrival time and are sent at once.
    pub at_ns: u64,
    pub bytes: [u8; 3],
    pub len: u8,
}

impl OutEvent {
    #[must_use]
    pub fn message(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len).min(3)).unwrap_or(&[])
    }
    fn protected(&self) -> bool {
        self.kind != Kind::Message || transit_midi_io::is_release(self.message())
    }
}

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    /// Ring full (either producer).
    dropped: AtomicU64,
    /// A release that did not fit the ring: a note may hang until the next Stop.
    dropped_releases: AtomicU64,
    /// Sent more than [`LATE_NS`] after the deadline (a negative offset beyond the render-ahead,
    /// or a starved thread).
    late: AtomicU64,
    max_late_ns: AtomicU64,
    /// Events for a destination whose port is missing.
    unsent: AtomicU64,
    /// Sends from this thread while marked as an audio thread. Must stay 0.
    audio_thread_sends: AtomicU64,
}

/// The producing end of a ring: the audio thread's, or the ingress worker's for echo.
pub struct Producer {
    tx: rtrb::Producer<OutEvent>,
    counters: Arc<Counters>,
}

impl Producer {
    /// Wait-free; no allocation, lock or system call. False when dropped (and counted).
    pub fn push(&mut self, event: OutEvent) -> bool {
        let free = self.tx.slots();
        let room = if event.protected() { free > 0 } else { free > RELEASE_RESERVE };
        if room && self.tx.push(event).is_ok() {
            return true;
        }
        self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        if event.protected() {
            self.counters.dropped_releases.fetch_add(1, Ordering::Relaxed);
        }
        false
    }
}

impl std::fmt::Debug for Producer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Producer").finish_non_exhaustive()
    }
}

/// One track's external destination, as control resolved it from the session.
#[derive(Clone, Debug, PartialEq)]
pub struct Destination {
    pub track: TrackId,
    pub port: MidiOutPort,
    pub channel: MidiOutChannel,
    /// Added to every playback event's instant (not to echo).
    pub offset_ns: i64,
}

impl Destination {
    /// Every active MIDI track of `session` with an external destination.
    #[must_use]
    pub fn of_session(session: &Session) -> Vec<Self> {
        session
            .tracks
            .iter()
            .filter(|t| t.kind.is_midi() && !t.inactive)
            .filter_map(|t| match &t.midi_output {
                soundcraft_model::MidiOutput::External { port, channel, .. } => Some(Self {
                    track: t.id,
                    port: port.clone(),
                    channel: *channel,
                    offset_ns: (f64::from(t.midi_output.offset_ms()) * 1e6) as i64,
                }),
                soundcraft_model::MidiOutput::Internal => None,
            })
            .take(MAX_DESTINATIONS)
            .collect()
    }
}

/// What a track's destination is doing now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestinationState {
    /// Waiting for the output thread's first look.
    Pending,
    /// The port is open.
    Connected,
    /// The port is not present (never was, or unplugged). Nothing is sent; it reopens when the
    /// port appears again.
    Missing,
    /// The port is present but would not open (another program may hold it).
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackOutput {
    pub track: TrackId,
    /// The port as the session names it.
    pub port: String,
    pub state: DestinationState,
}

/// The output side as the UI sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputStatus {
    /// Output ports present at the thread's last poll.
    pub ports: Vec<PortInfo>,
    pub tracks: Vec<TrackOutput>,
    pub sent: u64,
    pub dropped: u64,
    pub dropped_releases: u64,
    pub late: u64,
    pub max_late_ns: u64,
    pub unsent: u64,
    pub audio_thread_sends: u64,
    pub error: Option<String>,
}

impl OutputStatus {
    /// The state of `track`'s destination; `None` when it has none (internal) or the thread has
    /// not seen it yet.
    #[must_use]
    pub fn track(&self, track: TrackId) -> Option<&DestinationState> {
        self.tracks.iter().find(|t| t.track == track).map(|t| &t.state)
    }
}

enum Control {
    Destinations(Vec<Destination>),
    /// All notes off now on every open destination (Stop and the panic button, from Main).
    Panic,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    finished: AtomicBool,
    status: Mutex<(Vec<PortInfo>, Vec<TrackOutput>, Option<String>)>,
}

/// Control's handle (Main). Dropping it stops the thread, which first releases every held note
/// on every open port; the drop waits briefly for that and never joins a stuck driver.
pub struct Out {
    control: SyncSender<Control>,
    receive: Option<Receiver<Control>>,
    rings: Option<(rtrb::Consumer<OutEvent>, rtrb::Consumer<OutEvent>)>,
    counters: Arc<Counters>,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    last: Option<Vec<Destination>>,
    error: Option<String>,
}

impl std::fmt::Debug for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Out").field("running", &self.thread.is_some()).finish_non_exhaustive()
    }
}

impl Out {
    /// Main, before any producer runs: the handle, the audio thread's producer and the ingress
    /// worker's echo producer. Nothing is sent until [`Out::start`].
    #[must_use]
    pub fn new() -> (Self, Producer, Producer) {
        let counters = Arc::new(Counters::default());
        let (playback_tx, playback_rx) = rtrb::RingBuffer::new(QUEUE);
        let (echo_tx, echo_rx) = rtrb::RingBuffer::new(QUEUE);
        let (control, receive) = mpsc::sync_channel(64);
        (
            Self {
                control,
                receive: Some(receive),
                rings: Some((playback_rx, echo_rx)),
                counters: Arc::clone(&counters),
                shared: Arc::new(Shared::default()),
                thread: None,
                last: None,
                error: None,
            },
            Producer { tx: playback_tx, counters: Arc::clone(&counters) },
            Producer { tx: echo_tx, counters },
        )
    }

    /// Starts the output thread on `io`, timing against `origin` (the runtime's host clock).
    pub fn start(&mut self, io: MidiIo, origin: HostOrigin) -> std::io::Result<()> {
        let (Some(receive), Some((playback, echo))) = (self.receive.take(), self.rings.take()) else {
            return Ok(());
        };
        let counters = Arc::clone(&self.counters);
        let shared = Arc::clone(&self.shared);
        let thread = std::thread::Builder::new().name("transit-midi-out".into()).spawn(move || {
            let mut worker = Worker::new(io, origin, receive, playback, echo, counters, Arc::clone(&shared));
            worker.run();
            drop(worker);
            shared.finished.store(true, Ordering::Release);
        });
        match thread {
            Ok(thread) => {
                self.thread = Some(thread);
                Ok(())
            }
            Err(error) => {
                self.error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Sends the destination table when it changed. Control side, non-blocking.
    pub fn configure(&mut self, destinations: Vec<Destination>) {
        if self.last.as_ref() == Some(&destinations) {
            return;
        }
        match self.control.try_send(Control::Destinations(destinations.clone())) {
            Ok(()) => {
                self.last = Some(destinations);
                self.wake();
            }
            // Full: try again at the next service; the thread drains the queue every millisecond.
            Err(_) => self.error = Some("MIDI output control queue is full".into()),
        }
    }

    /// All notes off on every open destination, now.
    pub fn panic(&self) {
        if self.control.try_send(Control::Panic).is_ok() {
            self.wake();
        }
    }

    fn wake(&self) {
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }

    #[must_use]
    pub fn status(&self) -> OutputStatus {
        let (ports, tracks, error) = self.shared.status.lock().map(|s| s.clone()).unwrap_or_else(|p| p.into_inner().clone());
        let c = &self.counters;
        OutputStatus {
            ports,
            tracks,
            sent: c.sent.load(Ordering::Relaxed),
            dropped: c.dropped.load(Ordering::Relaxed),
            dropped_releases: c.dropped_releases.load(Ordering::Relaxed),
            late: c.late.load(Ordering::Relaxed),
            max_late_ns: c.max_late_ns.load(Ordering::Relaxed),
            unsent: c.unsent.load(Ordering::Relaxed),
            audio_thread_sends: c.audio_thread_sends.load(Ordering::Relaxed),
            error: error.or_else(|| self.error.clone()),
        }
    }

    /// The thread has exited (after its final releases).
    #[must_use]
    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.wake();
        // Let the final releases go out before the process can exit; never wait on a stuck
        // driver for long, and never join it.
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.thread.is_some() && !self.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if self.finished()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

/// Which ring an event came from: wraps release only what playback holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Playback,
    Echo,
}

#[derive(Clone, Copy)]
struct Pending {
    deadline: u64,
    seq: u64,
    origin: Origin,
    event: OutEvent,
}

struct Port {
    /// The key as the session names it (by name, and id when there is one).
    key: PortKey,
    label: String,
    handle: Option<OutputHandle>,
    state: DestinationState,
    /// Channels sent on since the port opened: All Notes Off goes to these.
    used: u16,
    /// The session names a port of another backend (a session from another OS): never opened.
    foreign: bool,
}

/// A destination's held state, per origin: notes as 128-bit sets per channel, pedals per channel.
#[derive(Clone, Copy, Default)]
struct Held {
    notes: [[u128; 16]; 2],
    sustain: [u16; 2],
}

struct Track {
    destination: Destination,
    port: usize,
    held: Held,
    /// Channels this track sent on: a destination change ends notes only on these, so another
    /// track sharing the port keeps playing.
    used: u16,
}

struct Worker {
    io: MidiIo,
    origin: HostOrigin,
    receive: Receiver<Control>,
    playback: rtrb::Consumer<OutEvent>,
    echo: rtrb::Consumer<OutEvent>,
    counters: Arc<Counters>,
    shared: Arc<Shared>,
    watcher: Option<PortWatcher>,
    last_poll: Option<Instant>,
    ports: Vec<Port>,
    tracks: Vec<Track>,
    pending: Vec<Pending>,
    seq: u64,
    error: Option<String>,
    dirty: bool,
}

fn origin_index(origin: Origin) -> usize {
    match origin {
        Origin::Playback => 0,
        Origin::Echo => 1,
    }
}

fn label(port: &MidiOutPort) -> String {
    if port.ordinal == 0 { port.name.clone() } else { format!("{} #{}", port.name, u64::from(port.ordinal) + 1) }
}

impl Worker {
    fn new(
        io: MidiIo,
        origin: HostOrigin,
        receive: Receiver<Control>,
        playback: rtrb::Consumer<OutEvent>,
        echo: rtrb::Consumer<OutEvent>,
        counters: Arc<Counters>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            io,
            origin,
            receive,
            playback,
            echo,
            counters,
            shared,
            watcher: None,
            last_poll: None,
            ports: Vec::new(),
            tracks: Vec::new(),
            pending: Vec::with_capacity(PENDING),
            seq: 0,
            error: None,
            dirty: true,
        }
    }

    fn now(&self) -> u64 {
        self.origin.now_ns().unwrap_or(0)
    }

    fn run(&mut self) {
        while !self.shared.stop.load(Ordering::Acquire) {
            self.control();
            self.drain();
            if self.last_poll.is_none_or(|last| last.elapsed() >= PORT_POLL) {
                self.last_poll = Some(Instant::now());
                self.poll_ports();
            }
            self.check_lost();
            self.send_due();
            if self.dirty {
                self.publish();
            }
            self.wait();
        }
        // Exit (application quit, device reopen): what is still due now goes out, then every
        // held note and pedal is released on every open port.
        self.control();
        self.drain();
        self.pending.clear();
        self.release_all(true);
        self.publish();
    }

    fn wait(&self) {
        let Some(next) = self.pending.first().map(|p| p.deadline) else {
            // `sleep` uses a high-resolution timer on Windows; a park timeout rounds to the
            // system timer tick, which can be 15.6 ms.
            std::thread::sleep(Duration::from_millis(1));
            return;
        };
        let now = self.now();
        let wait = next.saturating_sub(now);
        if wait > SPIN_NS {
            // The rings are polled at least every millisecond, which bounds echo latency.
            std::thread::sleep(Duration::from_nanos((wait - SPIN_NS).min(1_000_000)));
        } else if wait > 0 {
            std::thread::yield_now();
        }
    }

    fn control(&mut self) {
        loop {
            match self.receive.try_recv() {
                Ok(Control::Destinations(destinations)) => self.set_destinations(destinations),
                Ok(Control::Panic) => {
                    self.pending.retain(|p| p.origin == Origin::Echo);
                    self.release_all(true);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.shared.stop.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }

    fn set_destinations(&mut self, destinations: Vec<Destination>) {
        // A track whose port or channel changes (or that goes internal) first releases what it
        // holds on the old destination; an offset change alone keeps its notes.
        let mut kept: Vec<Track> = Vec::with_capacity(destinations.len());
        let old = std::mem::take(&mut self.tracks);
        for mut track in old {
            match destinations.iter().find(|d| d.track == track.destination.track) {
                Some(d) if d.port == track.destination.port && d.channel == track.destination.channel => {
                    track.destination = d.clone();
                    kept.push(track);
                }
                _ => {
                    self.release_track(&mut track, None, true);
                    self.pending.retain(|p| p.event.track != track.destination.track.0 || p.event.kind != Kind::Message);
                }
            }
        }
        for d in destinations {
            if kept.iter().any(|t| t.destination.track == d.track) {
                continue;
            }
            let port = self.port_index(&d.port);
            kept.push(Track { destination: d, port, held: Held::default(), used: 0 });
        }
        self.tracks = kept;
        // Close ports no destination uses any more.
        let used: Vec<bool> = (0..self.ports.len()).map(|i| self.tracks.iter().any(|t| t.port == i)).collect();
        for (i, port) in self.ports.iter_mut().enumerate() {
            if !used.get(i).copied().unwrap_or(false) {
                port.handle = None;
                port.used = 0;
                port.state = DestinationState::Pending;
            }
        }
        self.open_missing(true);
        self.dirty = true;
    }

    fn port_index(&mut self, port: &MidiOutPort) -> usize {
        let backend = self.io.backend();
        let key = PortKey { backend, name: port.name.clone(), ordinal: port.ordinal, id: port.id.clone() };
        let foreign = port.backend != backend.as_str();
        if let Some(i) = self.ports.iter().position(|p| p.key == key && p.foreign == foreign) {
            return i;
        }
        // A port of another backend (a session from another OS) is kept and stays missing.
        let foreign = port.backend != backend.as_str();
        let state = if foreign { DestinationState::Missing } else { DestinationState::Pending };
        self.ports.push(Port { key, label: label(port), handle: None, state, used: 0, foreign });
        self.ports.len() - 1
    }

    /// Opens every used port without a handle. With `retry_failed` false, a port that failed to
    /// open is not retried (it waits for a hot-plug change).
    fn open_missing(&mut self, retry_failed: bool) {
        for i in 0..self.ports.len() {
            if !self.tracks.iter().any(|t| t.port == i) {
                continue;
            }
            let Some(port) = self.ports.get_mut(i) else { continue };
            if port.handle.is_some() || port.foreign {
                continue;
            }
            if matches!(port.state, DestinationState::Failed(_)) && !retry_failed {
                continue;
            }
            match self.io.open_output(&port.key) {
                Ok(mut handle) => {
                    // A device that comes back (or a port opened for the first time) starts clean.
                    for channel in 0..16u8 {
                        if port.used & (1 << channel) != 0 {
                            let _ = handle.send(&[0xB0 | channel, 64, 0]);
                            let _ = handle.send(&[0xB0 | channel, 123, 0]);
                        }
                    }
                    port.handle = Some(handle);
                    port.state = DestinationState::Connected;
                }
                Err(transit_midi_io::MidiIoError::PortNotFound(_)) => port.state = DestinationState::Missing,
                Err(error) => port.state = DestinationState::Failed(error.to_string()),
            }
            self.dirty = true;
        }
    }

    fn poll_ports(&mut self) {
        if self.watcher.is_none() {
            match self.io.watcher() {
                Ok(watcher) => self.watcher = Some(watcher),
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.dirty = true;
                    return;
                }
            }
        }
        let Some(watcher) = &mut self.watcher else { return };
        match watcher.poll() {
            Ok(changes) => {
                let added = changes.iter().any(|c| matches!(c, transit_midi_io::PortChange::Added(p) if p.direction == Direction::Output));
                if !changes.is_empty() {
                    self.dirty = true;
                }
                if self.error.take().is_some() {
                    self.dirty = true;
                }
                // Disconnections flagged by this poll are handled by check_lost; a returning
                // port reopens here. A failed open is retried only when ports change.
                self.check_lost();
                if added || self.ports.iter().any(|p| p.handle.is_none() && p.state == DestinationState::Pending) {
                    self.open_missing(added);
                }
            }
            Err(error) => {
                self.error = Some(error.to_string());
                self.dirty = true;
            }
        }
    }

    /// A port flagged disconnected (by any watcher of this `MidiIo`) is closed and marked missing;
    /// what its tracks held is forgotten (nothing can be sent to a device that is gone) and its
    /// scheduled events are dropped as they come due.
    fn check_lost(&mut self) {
        for i in 0..self.ports.len() {
            let lost = self.ports.get(i).and_then(|p| p.handle.as_ref()).is_some_and(OutputHandle::is_disconnected);
            if lost {
                self.lose(i);
            }
        }
    }

    fn lose(&mut self, port: usize) {
        if let Some(p) = self.ports.get_mut(port) {
            p.handle = None;
            p.state = DestinationState::Missing;
        }
        for track in self.tracks.iter_mut().filter(|t| t.port == port) {
            track.held = Held::default();
        }
        self.dirty = true;
    }

    fn drain(&mut self) {
        let now = self.now();
        while let Ok(event) = self.playback.pop() {
            match event.kind {
                Kind::Stop => {
                    // Scheduled playback is audio that will not be heard: drop it, then release.
                    self.pending.retain(|p| p.origin == Origin::Echo);
                    self.release_playback_and_reset();
                }
                Kind::Release => {
                    // One release per destination at the wrap instant plus that destination's
                    // offset, so a delayed destination ends its lap where its notes do.
                    // Playback scheduled at or after the jump belongs to a block the transport
                    // discarded (a locate drops its rendered-ahead block): it never sounds.
                    let at = if event.at_ns == 0 { now } else { event.at_ns };
                    for i in 0..self.tracks.len() {
                        let Some(track) = self.tracks.get(i) else { continue };
                        let id = track.destination.track.0;
                        let deadline = at.saturating_add_signed(track.destination.offset_ns);
                        self.pending.retain(|p| !(p.origin == Origin::Playback && p.event.track == id && p.deadline >= deadline));
                        let release = OutEvent { kind: Kind::Release, track: id, ..event };
                        self.schedule(deadline, Origin::Playback, release);
                    }
                }
                Kind::Message => {
                    let offset = self.tracks.iter().find(|t| t.destination.track.0 == event.track).map(|t| t.destination.offset_ns);
                    let Some(offset) = offset else { continue };
                    let at = if event.at_ns == 0 { now } else { event.at_ns };
                    self.schedule(at.saturating_add_signed(offset), Origin::Playback, event);
                }
            }
        }
        while let Ok(event) = self.echo.pop() {
            if event.kind == Kind::Message {
                // Live input goes out now, ahead of anything scheduled later.
                self.schedule(now, Origin::Echo, event);
            }
        }
    }

    fn schedule(&mut self, deadline: u64, origin: Origin, event: OutEvent) {
        if self.pending.len() >= PENDING && !event.protected() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.seq += 1;
        let pending = Pending { deadline, seq: self.seq, origin, event };
        // Sorted by (deadline, seq): equal instants keep arrival order (offs before ons).
        let at = self.pending.partition_point(|p| (p.deadline, p.seq) <= (deadline, pending.seq));
        self.pending.insert(at, pending);
    }

    fn send_due(&mut self) {
        let now = self.now();
        let due = self.pending.partition_point(|p| p.deadline <= now);
        if due == 0 {
            return;
        }
        let batch: Vec<Pending> = self.pending.drain(..due).collect();
        for p in batch {
            let late = now.saturating_sub(p.deadline);
            if p.origin == Origin::Playback && late > LATE_NS {
                self.counters.late.fetch_add(1, Ordering::Relaxed);
            }
            self.counters.max_late_ns.fetch_max(late, Ordering::Relaxed);
            let Some(i) = self.tracks.iter().position(|t| t.destination.track.0 == p.event.track) else { continue };
            match p.event.kind {
                Kind::Release => {
                    if let Some(mut track) = self.take_track(i) {
                        self.release_track(&mut track, Some(Origin::Playback), false);
                        self.put_track(i, track);
                    }
                }
                Kind::Message => self.deliver(i, p.origin, p.event),
                Kind::Stop => {}
            }
        }
    }

    fn take_track(&mut self, i: usize) -> Option<Track> {
        if i < self.tracks.len() { Some(self.tracks.swap_remove(i)) } else { None }
    }

    fn put_track(&mut self, i: usize, track: Track) {
        self.tracks.push(track);
        let last = self.tracks.len() - 1;
        self.tracks.swap(i, last);
    }

    /// One message for track `i`: channel resolved (echo keeps or rewrites the input channel as
    /// the destination says; playback arrives resolved), held state tracked, then sent.
    fn deliver(&mut self, i: usize, origin: Origin, event: OutEvent) {
        let Some(track) = self.tracks.get_mut(i) else { return };
        let mut bytes = event.bytes;
        let status = bytes[0];
        if !(0x80..0xF0).contains(&status) {
            return;
        }
        if origin == Origin::Echo {
            bytes[0] = (status & 0xF0) | track.destination.channel.resolve(status & 0x0F);
        }
        let message = bytes.get(..usize::from(event.len).min(3)).unwrap_or(&[]);
        let channel = usize::from(bytes[0] & 0x0F);
        let held = &mut track.held;
        let o = origin_index(origin);
        match (bytes[0] & 0xF0, message) {
            (0x90, [_, key, velocity]) if *velocity > 0 => {
                if let Some(set) = held.notes[o].get_mut(channel) {
                    *set |= 1u128 << (key & 0x7F);
                }
            }
            (0x80 | 0x90, [_, key, _]) => {
                if let Some(set) = held.notes[o].get_mut(channel) {
                    *set &= !(1u128 << (key & 0x7F));
                }
            }
            (0xB0, [_, 64, value]) => {
                if *value >= 64 {
                    held.sustain[o] |= 1 << channel;
                } else {
                    held.sustain[o] &= !(1 << channel);
                }
            }
            (0xB0, [_, 120 | 123, _]) => {
                held.notes[o][channel] = 0;
            }
            _ => {}
        }
        let message = message.to_vec();
        let port = track.port;
        track.used |= 1 << channel;
        if !self.send(port, &message) {
            // Not sent: the device does not hold the note either.
            if let Some(track) = self.tracks.get_mut(i)
                && let Some(set) = track.held.notes[o].get_mut(channel)
                && message.first().is_some_and(|s| s & 0xF0 == 0x90)
            {
                *set &= !(1u128 << (message.get(1).copied().unwrap_or(0) & 0x7F));
            }
        }
    }

    /// Sends on `port`; false when the port is not open or the send failed (then it is lost).
    fn send(&mut self, port: usize, message: &[u8]) -> bool {
        if soundcraft_mix::on_audio_thread() {
            // Never: this thread is not an audio thread. Counted so tests can prove it.
            self.counters.audio_thread_sends.fetch_add(1, Ordering::Relaxed);
        }
        let Some(p) = self.ports.get_mut(port) else { return false };
        let Some(handle) = p.handle.as_mut() else {
            self.counters.unsent.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        match handle.send(message) {
            Ok(()) => {
                if let Some(status) = message.first() {
                    p.used |= 1 << (status & 0x0F);
                }
                self.counters.sent.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(error) => {
                self.error = Some(format!("{}: {error}", p.label));
                self.lose(port);
                false
            }
        }
    }

    /// Note-offs for what `track` holds (from `origin`, or both), sustain off where it was down;
    /// with `all_off` also Sustain off and All Notes Off on every channel its port was used on.
    fn release_track(&mut self, track: &mut Track, origin: Option<Origin>, all_off: bool) {
        let port = track.port;
        let origins: &[Origin] = match origin {
            Some(Origin::Playback) => &[Origin::Playback],
            Some(Origin::Echo) => &[Origin::Echo],
            None => &[Origin::Playback, Origin::Echo],
        };
        for &o in origins {
            let i = origin_index(o);
            for channel in 0..16u8 {
                let set = track.held.notes[i][usize::from(channel)];
                if set != 0 {
                    for key in 0..128u8 {
                        if set & (1u128 << key) != 0 {
                            self.send(port, &[0x80 | channel, key, 64]);
                        }
                    }
                }
                if track.held.sustain[i] & (1 << channel) != 0 {
                    self.send(port, &[0xB0 | channel, 64, 0]);
                }
            }
            track.held.notes[i] = [0; 16];
            track.held.sustain[i] = 0;
        }
        if all_off {
            for channel in 0..16u8 {
                if track.used & (1 << channel) != 0 {
                    self.send(port, &[0xB0 | channel, 64, 0]);
                    self.send(port, &[0xB0 | channel, 123, 0]);
                }
            }
            track.used = 0;
        }
    }

    /// Stop: what playback holds is ended on every destination, then each used port gets
    /// Sustain off and All Notes Off on its used channels (once per port, not per track).
    fn release_playback_and_reset(&mut self) {
        self.release_all(true);
    }

    fn release_all(&mut self, all_off: bool) {
        let mut tracks = std::mem::take(&mut self.tracks);
        for track in &mut tracks {
            self.release_track(track, None, false);
        }
        self.tracks = tracks;
        if all_off {
            for port in 0..self.ports.len() {
                let used = self.ports.get(port).map_or(0, |p| p.used);
                for channel in 0..16u8 {
                    if used & (1 << channel) != 0 {
                        self.send(port, &[0xB0 | channel, 64, 0]);
                        self.send(port, &[0xB0 | channel, 123, 0]);
                    }
                }
            }
        }
    }

    fn publish(&mut self) {
        self.dirty = false;
        let ports = self.watcher.as_ref().map(|w| w.ports().filter(|p| p.direction == Direction::Output).cloned().collect()).unwrap_or_default();
        let tracks = self
            .tracks
            .iter()
            .map(|t| {
                let port = self.ports.get(t.port);
                TrackOutput {
                    track: t.destination.track,
                    port: port.map(|p| p.label.clone()).unwrap_or_default(),
                    state: port.map_or(DestinationState::Missing, |p| p.state.clone()),
                }
            })
            .collect();
        let mut status = self.shared.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status = (ports, tracks, self.error.clone());
    }
}

#[cfg(test)]
mod tests;
