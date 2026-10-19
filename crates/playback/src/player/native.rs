//! Native MIDI device ownership. Enumeration/open/close run off Main; the
//! independent ingress worker handles timestamped input even while UI is busy.
use super::{ActionResult, RuntimeControl, ingress, sources};
use crate::{device_clock::HostOrigin, player::audio::Command};
use soundcraft_midi_io::{Direction, InputBoundary, InputConfig, InputConsumer, InputHandle, MidiIo, PortInfo, PortKey};
use soundcraft_mix::{EngineEpoch, INSTRUMENT_SLOT};
use soundcraft_model::{MidiInput, MidiPort};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

type Owner = RuntimeControl<Command, ActionResult>;
const POLL: Duration = Duration::from_millis(100);
const LOOKAHEAD: u32 = 128;

#[derive(Default, Clone, Debug)]
pub struct Status {
    pub inputs: Vec<PortInfo>,
    pub open_inputs: usize,
    pub routes: usize,
    pub counts: ingress::Counters,
    pub error: Option<String>,
    pub timing_quality: Option<crate::device_clock::Quality>,
}

enum Request {
    Open { id: u64, key: PortKey },
    Close(u64),
}
enum Reply {
    Ports(Vec<PortInfo>),
    Opened { id: u64, consumer: InputConsumer, boundary: InputBoundary },
    Failed { id: u64, error: String },
    Error(String),
    Gone(u64),
}

struct IoThread {
    send: SyncSender<Request>,
    receive: Receiver<Reply>,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl IoThread {
    fn start(io: MidiIo, origin: HostOrigin) -> std::io::Result<Self> {
        let (send, requests) = mpsc::sync_channel(sources::PORTS);
        let (replies, receive) = mpsc::sync_channel(sources::PORTS);
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (end, done) = (Arc::clone(&stop), Arc::clone(&finished));
        let thread = std::thread::Builder::new().name("soundcraft-midi-devices".into()).spawn(move || {
            struct Finished(Arc<AtomicBool>);
            impl Drop for Finished {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _finished = Finished(done);
            let mut watcher = None;
            let mut handles: Vec<(u64, InputHandle)> = Vec::with_capacity(sources::PORTS);
            let mut pending = None;
            let mut last_poll = None;
            while !end.load(Ordering::Acquire) {
                if last_poll.is_none_or(|last: Instant| last.elapsed() >= POLL) {
                    last_poll = Some(Instant::now());
                    if watcher.is_none() {
                        match io.watcher() {
                            Ok(value) => watcher = Some(value),
                            Err(error) => {
                                let _ = replies.try_send(Reply::Error(error.to_string()));
                            }
                        }
                    }
                    if let Some(watcher) = &mut watcher {
                        match watcher.poll() {
                            Ok(_) => {
                                let inputs = watcher.ports().filter(|port| port.direction == Direction::Input).cloned().collect();
                                let _ = replies.try_send(Reply::Ports(inputs));
                            }
                            Err(error) => {
                                let _ = replies.try_send(Reply::Error(error.to_string()));
                            }
                        }
                    }
                    for (id, handle) in &handles {
                        if handle.is_disconnected() || handle.counters().dropped_releases != 0 || handle.boundary().is_exhausted() {
                            let _ = replies.try_send(Reply::Gone(*id));
                        }
                    }
                }
                // Keep an open result until Main receives it. A full UI queue
                // must not orphan a device or stop independent unplug detection.
                if let Some(reply) = pending.take() {
                    match replies.try_send(reply) {
                        Ok(()) => {}
                        Err(TrySendError::Full(reply)) => pending = Some(reply),
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
                if pending.is_none() {
                    match requests.try_recv() {
                        Ok(Request::Open { id, key }) => {
                            pending = Some(if handles.len() >= sources::PORTS {
                                Reply::Failed { id, error: "MIDI device capacity exhausted".into() }
                            } else {
                                match io.open_input(&key, InputConfig { host_origin: origin.instant(), ..InputConfig::default() }) {
                                    Ok((handle, consumer)) => {
                                        let boundary = handle.boundary();
                                        handles.push((id, handle));
                                        Reply::Opened { id, consumer, boundary }
                                    }
                                    Err(error) => Reply::Failed { id, error: error.to_string() },
                                }
                            });
                        }
                        Ok(Request::Close(id)) => handles.retain(|(open, _)| *open != id),
                        Err(mpsc::TryRecvError::Disconnected) => break,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                }
                std::thread::park_timeout(Duration::from_millis(2));
            }
            // Every native device destructor runs here, including failed opens,
            // unplug and application shutdown. No Main/audio join is required.
            drop(handles);
        })?;
        Ok(Self { send, receive, stop, finished, thread })
    }

    fn request(&self, request: Request) -> Result<(), &'static str> {
        self.send.try_send(request).map_err(|_| "MIDI device request queue unavailable")?;
        self.thread.thread().unpark();
        Ok(())
    }
}

impl Drop for IoThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
    }
}

struct Binding {
    port: ingress::Port,
    route: ingress::Route,
}
struct Input {
    id: u64,
    key: PortKey,
    attached: Option<(u64, InputBoundary)>,
    routes: Vec<Binding>,
    dirty: bool,
    routes_applied: Option<Arc<AtomicBool>>,
    failed: bool,
    closed: bool,
    lost: bool,
}

pub(super) struct Native {
    worker: ingress::Worker,
    io: Option<IoThread>,
    inputs: Vec<Input>,
    retired: Vec<sources::Revocation>,
    next_id: u64,
    status: Status,
    origin: HostOrigin,
}

impl Native {
    pub(super) fn new(lane: crate::action_lane::Control<ingress::Packet, ActionResult>, origin: HostOrigin) -> std::io::Result<Self> {
        Ok(Self {
            worker: ingress::Worker::start(lane)?,
            io: None,
            inputs: Vec::with_capacity(sources::PORTS),
            retired: Vec::with_capacity(sources::PORTS),
            next_id: 0,
            status: Status::default(),
            origin,
        })
    }

    pub(super) fn enable(&mut self, io: MidiIo) {
        match IoThread::start(io, self.origin) {
            Ok(thread) => self.io = Some(thread),
            Err(error) => self.status.error = Some(error.to_string()),
        }
    }

    pub(super) fn status(&self) -> Status {
        let mut status = self.status.clone();
        status.counts = self.worker.counters();
        status.open_inputs = self.inputs.iter().filter(|input| input.attached.is_some()).count();
        status.routes = self
            .inputs
            .iter()
            .filter(|input| !input.dirty && input.routes_applied.as_ref().is_some_and(|applied| applied.load(Ordering::Acquire)))
            .map(|input| input.routes.len())
            .sum();
        status
    }

    pub(super) fn cutoff(&self, owner: &mut Owner) -> Result<(), super::PlayerError> {
        for input in &self.inputs {
            // One physical cutoff covers every route of the same backend offer.
            if let Some((_, boundary)) = &input.attached {
                let cutoff = boundary.cutoff();
                for binding in &input.routes {
                    owner.cancel_source_before(binding.port.token(), cutoff)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn service(&mut self, owner: &mut Owner, epoch: EngineEpoch) {
        let Some(io) = &self.io else { return };
        if io.finished.load(Ordering::Acquire) || owner.faulted() {
            for input in &self.inputs {
                for binding in &input.routes {
                    binding.port.invalidate();
                }
            }
            self.status.error = Some("MIDI devices stopped; reconnect audio to reopen MIDI".into());
            return;
        }
        for _ in 0..sources::PORTS {
            match io.receive.try_recv() {
                Ok(Reply::Ports(ports)) => self.status.inputs = ports,
                Ok(Reply::Opened { id, consumer, boundary }) => {
                    if let Some(input) = self.inputs.iter_mut().find(|input| input.id == id) {
                        match self.worker.attach_input(consumer) {
                            Ok(id) => {
                                input.attached = Some((id, boundary));
                                input.dirty = true;
                            }
                            Err(error) => {
                                input.failed = true;
                                self.status.error = Some(error.into());
                            }
                        }
                    }
                }
                Ok(Reply::Failed { id, error }) => {
                    if let Some(input) = self.inputs.iter_mut().find(|input| input.id == id) {
                        input.failed = true;
                    }
                    self.status.error = Some(error);
                }
                Ok(Reply::Error(error)) => self.status.error = Some(error),
                Ok(Reply::Gone(id)) => {
                    if let Some(input) = self.inputs.iter_mut().find(|input| input.id == id) {
                        input.lost = true;
                    }
                }
                Err(_) => break,
            }
        }
        self.retired.retain(|request| {
            if owner.source_status(*request) == Ok(sources::State::Revoked) { owner.release_source(*request).is_err() } else { true }
        });
        let routes: Vec<_> = owner
            .requested_session()
            .tracks
            .iter()
            .filter(|track| track.kind.is_midi() && !track.inactive && (track.mixer.record_arm || track.mixer.input_monitor))
            .filter_map(|track| {
                let channels = match track.midi_input.channel {
                    None => u16::MAX,
                    Some(channel) if channel < 16 => 1 << channel,
                    Some(_) => return None,
                };
                owner
                    .binding(track.id, INSTRUMENT_SLOT)
                    .map(|target| (track.midi_input.clone(), ingress::Route { engine: epoch, target, channels, lookahead: LOOKAHEAD }))
            })
            .collect();
        for input in &mut self.inputs {
            let present = self.status.inputs.iter().any(|port| same_port(&port.key, &input.key));
            let routes: Vec<_> =
                routes.iter().filter(|(selection, _)| accepts_port(selection, &input.key, &self.status.inputs)).map(|(_, route)| *route).collect();
            input.routes.retain(|binding| {
                if present && !input.failed && !input.lost && binding.port.admitted() && routes.contains(&binding.route) {
                    return true;
                }
                binding.port.invalidate();
                if let Ok(request) = owner.revoke_source(binding.port.token()) {
                    self.retired.push(request);
                }
                input.dirty = true;
                false
            });
            if (!present || input.failed || input.lost || routes.is_empty()) && !input.closed && io.request(Request::Close(input.id)).is_ok() {
                input.closed = true;
                input.attached = None;
            }
        }
        // Failed opens remain visible until a reconnect or a port identity change;
        // do not spin native opens at UI repaint frequency.
        self.inputs
            .retain(|input| !input.closed || (input.failed && !input.lost && self.status.inputs.iter().any(|port| same_port(&port.key, &input.key))));
        if !routes.is_empty() {
            for port in &self.status.inputs {
                if self.inputs.len() == sources::PORTS
                    || !routes.iter().any(|(selection, _)| accepts_port(selection, &port.key, &self.status.inputs))
                    || self.inputs.iter().any(|input| same_port(&input.key, &port.key))
                {
                    continue;
                }
                let Some(id) = self.next_id.checked_add(1) else {
                    self.status.error = Some("MIDI input identities exhausted".into());
                    break;
                };
                self.next_id = id;
                match io.request(Request::Open { id, key: port.key.clone() }) {
                    Ok(()) => self.inputs.push(Input {
                        id,
                        key: port.key.clone(),
                        attached: None,
                        routes: Vec::new(),
                        dirty: true,
                        routes_applied: None,
                        failed: false,
                        closed: false,
                        lost: false,
                    }),
                    Err(error) => {
                        self.status.error = Some(error.into());
                        break;
                    }
                }
            }
        }
        let mut used = [false; sources::PORTS];
        for token in self
            .inputs
            .iter()
            .flat_map(|input| &input.routes)
            .map(|binding| binding.port.token())
            .chain(self.retired.iter().map(|request| request.source))
        {
            if let Some(slot) = used.get_mut(usize::from(token.port())) {
                *slot = true;
            }
        }
        for input in &mut self.inputs {
            let Some((id, boundary)) = &input.attached else { continue };
            for (_, route) in routes.iter().filter(|(selection, _)| accepts_port(selection, &input.key, &self.status.inputs)) {
                if input.routes.iter().any(|binding| binding.route == *route) {
                    continue;
                }
                let Some(port) = used.iter().position(|used| !used) else {
                    self.status.error = Some("MIDI routing capacity exhausted".into());
                    break;
                };
                let token = match owner.register_source(port as u16) {
                    Ok(token) => token,
                    Err(error) => {
                        self.status.error = Some(error.to_string());
                        break;
                    }
                };
                used[port] = true;
                if let Err(error) = owner.cancel_source_before(token, boundary.cutoff()) {
                    self.status.error = Some(error.to_string());
                    if let Ok(request) = owner.revoke_source(token) {
                        self.retired.push(request);
                    }
                    break;
                }
                match owner.source_permit(token) {
                    Ok(permit) => input.routes.push(Binding { port: ingress::Port::new(permit, boundary.clone()), route: *route }),
                    Err(error) => {
                        self.status.error = Some(error.to_string());
                        if let Ok(request) = owner.revoke_source(token) {
                            self.retired.push(request);
                        }
                    }
                }
                input.dirty = true;
            }
            if input.dirty {
                let routes = input.routes.iter().map(|binding| (binding.port.permit(), binding.route)).collect();
                match self.worker.set_routes(*id, routes) {
                    Ok(applied) => {
                        input.routes_applied = Some(applied);
                        input.dirty = false;
                    }
                    Err(error) => self.status.error = Some(error.into()),
                }
            }
        }
    }
}

fn accepts_port(selection: &MidiInput, key: &PortKey, ports: &[PortInfo]) -> bool {
    match &selection.port {
        MidiPort::All => true,
        MidiPort::None => false,
        MidiPort::Device { backend, name, ordinal, id } => {
            if backend != key.backend.as_str() {
                return false;
            }
            if !id.is_empty() {
                return id == &key.id;
            }
            // Match midi-io's legacy resolver: anonymous ports take precedence;
            // an anonymous miss must never select an identified namesake.
            let anonymous = ports.iter().any(|port| port.key.backend == key.backend && port.key.name == *name && port.key.id.is_empty());
            key.name == *name && key.ordinal == *ordinal && (!anonymous || key.id.is_empty())
        }
    }
}

fn same_port(a: &PortKey, b: &PortKey) -> bool {
    a.backend == b.backend
        && if a.id.is_empty() || b.id.is_empty() {
            a.id.is_empty() && b.id.is_empty() && a.name == b.name && a.ordinal == b.ordinal
        } else {
            a.id == b.id
        }
}

impl Drop for Native {
    fn drop(&mut self) {
        for input in &self.inputs {
            for binding in &input.routes {
                binding.port.invalidate();
            }
        }
        // Fields then close worker admission and signal the device owner; no
        // device connection or plug-in crosses to the callback during teardown.
    }
}

#[cfg(test)]
mod tests;
