//! Registered ingress identities and acknowledged, independent source revocation.
//! One fixed slot per registered port owns its pending request and acknowledgment.
//! Both endpoints must remain anchored on main until callback quiescence.
use crate::{action_lane, plan_bridge};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Maximum simultaneous registrations, including UI audition when present.
pub const PORTS: usize = 64;
/// F10's reserved UI audition source. Clip identity is private to mix scheduling.
pub const AUDITION_PORT: u16 = 0xFFFE;
/// A revoke traverses the engine's instruments: start with one per callback.
pub const CALLBACK_REVOCATIONS: usize = 1;
static NEXT_REGISTRY: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    registry: u64,
    slot: u8,
    port: u16,
    generation: u64,
}
impl Token {
    pub fn port(self) -> u16 {
        self.port
    }
    pub fn generation(self) -> u64 {
        self.generation
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Revocation {
    pub source: Token,
    pub id: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Revoked,
    Faulted,
    Shutdown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub request: Revocation,
    pub outcome: State,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Full,
    PortRegistered,
    PortOutOfRange,
    Exhausted,
    StaleToken,
    NotRevoked,
    Revoked,
    Faulted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    Refused,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "all MIDI source slots are occupied",
            Self::PortRegistered => "this MIDI port is already registered",
            Self::PortOutOfRange => "MIDI sources require a compact port index or the UI audition port",
            Self::Exhausted => "MIDI source identities are exhausted; reopen the runtime",
            Self::StaleToken => "this MIDI source no longer belongs to this runtime",
            Self::Revoked => "this MIDI source no longer admits input",
            Self::NotRevoked => "the audio callback has not acknowledged this source's release",
            Self::Faulted => "MIDI source recovery is faulted; reopen the runtime",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Default)]
struct Port {
    port: AtomicU32,
    generation: AtomicU64,
    admitted: AtomicBool,
    invalidated: AtomicU64,
    // Sole writer is the non-clone Audio endpoint. Keep this separate from
    // worker fetch_max so audio recovery never needs a CAS retry loop.
    audio_invalidated: AtomicU64,
    cutoff: AtomicU64,
    requested: AtomicU64,
    acknowledged: AtomicU64,
}
struct Shared {
    runtime: plan_bridge::RuntimeIdentity,
    ports: [Port; PORTS],
    faulted: AtomicBool,
    closed: AtomicBool,
}
impl Shared {
    fn faulted(&self) -> bool {
        self.runtime.faulted() || self.faulted.load(Ordering::Acquire)
    }
}
/// Read-only admission identity plus a generation-bound recovery signal for a
/// native MIDI worker. It carries no Main-thread handles or mutable producer.
/// A successful check is provisional: audio validates the source again at dispatch.
#[derive(Clone)]
pub struct Permit {
    token: Token,
    shared: Arc<Shared>,
}
impl Permit {
    pub fn token(&self) -> Token {
        self.token
    }
    pub fn admits(&self) -> bool {
        if self.shared.closed.load(Ordering::Acquire) || self.shared.faulted() {
            return false;
        }
        let Some(port) = self.shared.ports.get(usize::from(self.token.slot)) else { return false };
        port.generation.load(Ordering::Acquire) == self.token.generation
            && port.port.load(Ordering::Relaxed) == u32::from(self.token.port)
            && port.admitted.load(Ordering::Acquire)
            && port.invalidated.load(Ordering::Acquire) < self.token.generation
            && port.audio_invalidated.load(Ordering::Acquire) < self.token.generation
            && port.generation.load(Ordering::Acquire) == self.token.generation
    }
    /// Raw input sequence admission. Carry this original sequence through every
    /// worker queue, then check it again on audio; a worker can race a Stop cutoff.
    /// MAX is reserved by midi-io as the exclusive terminal cutoff, never an event.
    pub fn admits_sequence(&self, sequence: u64) -> bool {
        if sequence == u64::MAX || !self.admits() {
            return false;
        }
        let Some(port) = self.shared.ports.get(usize::from(self.token.slot)) else { return false };
        sequence >= port.cutoff.load(Ordering::Acquire) && port.generation.load(Ordering::Acquire) == self.token.generation
    }
    /// Disconnect or an unprotected dropped release closes this generation now.
    /// The audio source pass performs release without needing Main to poll. This
    /// signal is idempotent and cannot close a newer registration in the same slot.
    pub fn invalidate(&self) {
        if let Some(port) = self.shared.ports.get(usize::from(self.token.slot)) {
            port.invalidated.fetch_max(self.token.generation, Ordering::Release);
        }
    }
}

#[derive(Clone, Copy)]
struct Entry {
    token: Token,
    request: Option<Revocation>,
}

pub struct Control {
    registry: u64,
    shared: Arc<Shared>,
    entries: [Option<Entry>; PORTS],
    generation: u64,
    request: u64,
    main_thread: PhantomData<Rc<()>>,
}
pub struct Audio {
    shared: Arc<Shared>,
    seen: [u64; PORTS],
    revoked: [u64; PORTS],
    cursor: usize,
    // Retain an escaped payload for destruction by the main owner after quiescence.
    panic_payload: Option<Box<dyn std::any::Any + Send>>,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    pub revoked: usize,
    pub faulted: bool,
}

pub fn pair(bridge: &plan_bridge::Control) -> Result<(Control, Audio), Error> {
    // The renamed try_update is newer than the workspace's Rust 1.90 minimum.
    #[allow(deprecated)]
    let registry = NEXT_REGISTRY.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)).map_err(|_| Error::Exhausted)?;
    let shared = Arc::new(Shared {
        runtime: bridge.runtime_identity(),
        ports: std::array::from_fn(|_| Port::default()),
        faulted: AtomicBool::new(false),
        closed: AtomicBool::new(false),
    });
    Ok((
        Control { registry, shared: Arc::clone(&shared), entries: [None; PORTS], generation: 0, request: 0, main_thread: PhantomData },
        Audio { shared, seen: [0; PORTS], revoked: [0; PORTS], cursor: 0, panic_payload: None },
    ))
}

impl Control {
    pub fn register(&mut self, port: u16) -> Result<Token, Error> {
        if self.faulted() {
            return Err(Error::Faulted);
        }
        if usize::from(port) >= PORTS && port != AUDITION_PORT {
            return Err(Error::PortOutOfRange);
        }
        if self.entries.iter().flatten().any(|entry| entry.token.port == port) {
            return Err(Error::PortRegistered);
        }
        let slot = self.entries.iter().position(Option::is_none).ok_or(Error::Full)?;
        let generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        self.generation = generation;
        let token = Token { registry: self.registry, slot: slot as u8, port, generation };
        let shared = &self.shared.ports[slot];
        shared.admitted.store(false, Ordering::Release);
        shared.port.store(u32::from(port), Ordering::Relaxed);
        shared.cutoff.store(0, Ordering::Relaxed);
        shared.generation.store(generation, Ordering::Release);
        shared.admitted.store(true, Ordering::Release);
        self.entries[slot] = Some(Entry { token, request: None });
        Ok(token)
    }
    fn entry(&self, token: Token) -> Result<Entry, Error> {
        self.entries
            .get(usize::from(token.slot))
            .copied()
            .flatten()
            .filter(|entry| token.registry == self.registry && entry.token == token)
            .ok_or(Error::StaleToken)
    }
    pub fn admits(&self, token: Token) -> bool {
        !self.faulted()
            && self.entry(token).is_ok_and(|entry| entry.request.is_none())
            && self.shared.ports[usize::from(token.slot)].invalidated.load(Ordering::Acquire) < token.generation
            && self.shared.ports[usize::from(token.slot)].audio_invalidated.load(Ordering::Acquire) < token.generation
    }
    pub fn permit(&self, token: Token) -> Result<Permit, Error> {
        if self.faulted() {
            return Err(Error::Faulted);
        }
        self.entry(token)?;
        if !self.admits(token) {
            return Err(Error::Revoked);
        }
        Ok(Permit { token, shared: Arc::clone(&self.shared) })
    }
    /// Main captures midi-io's InputBoundary cutoff, publishes it here, then admits
    /// Stop to the action lane. This filters raw offers already drained by a worker
    /// or delayed before publication; queue length and diagnostics cannot do that.
    /// Only this !Send control may write cutoffs, so an old worker cannot reset or
    /// advance the cutoff of a reused slot. Cutoffs never move backwards per open.
    pub fn cancel_before(&mut self, token: Token, cutoff: u64) -> Result<(), Error> {
        if self.faulted() {
            return Err(Error::Faulted);
        }
        self.entry(token)?;
        self.shared.ports[usize::from(token.slot)].cutoff.fetch_max(cutoff, Ordering::Release);
        Ok(())
    }
    /// Idempotent while this registration exists. Repeated calls return the SAME
    /// request; they cannot consume new storage or postpone its acknowledgment.
    pub fn revoke(&mut self, token: Token) -> Result<Revocation, Error> {
        if self.faulted() {
            return Err(Error::Faulted);
        }
        let entry = self.entry(token)?;
        if let Some(request) = entry.request {
            return Ok(request);
        }
        let id = self.request.checked_add(1).ok_or(Error::Exhausted)?;
        self.request = id;
        let request = Revocation { source: token, id };
        let slot = usize::from(token.slot);
        self.shared.ports[slot].admitted.store(false, Ordering::Release);
        self.entries[slot] = Some(Entry { token, request: Some(request) });
        self.shared.ports[slot].requested.store(id, Ordering::Release);
        Ok(request)
    }
    /// An observation, not a destructive receipt read. `release` or `retire` takes
    /// the final receipt and releases the registration's fixed storage.
    pub fn status(&self, request: Revocation) -> Result<State, Error> {
        let entry = self.entry(request.source)?;
        if entry.request != Some(request) {
            return Err(Error::StaleToken);
        }
        if self.shared.ports[usize::from(request.source.slot)].acknowledged.load(Ordering::Acquire) >= request.id {
            return Ok(State::Revoked);
        }
        if self.faulted() {
            return Ok(State::Faulted);
        }
        Ok(State::Pending)
    }
    pub fn release(&mut self, request: Revocation) -> Result<Receipt, Error> {
        if self.faulted() {
            return Err(Error::Faulted);
        }
        if self.status(request)? != State::Revoked {
            return Err(Error::NotRevoked);
        }
        self.entries[usize::from(request.source.slot)] = None;
        Ok(Receipt { request, outcome: State::Revoked })
    }
    pub fn faulted(&self) -> bool {
        self.shared.faulted()
    }
    pub(super) fn fault(&mut self) {
        self.shared.faulted.store(true, Ordering::Release);
    }
    pub fn pending(&self) -> usize {
        self.entries.iter().flatten().filter_map(|e| e.request).filter(|&r| self.status(r) == Ok(State::Pending)).count()
    }
    /// Main only, after quiescence. No callback is required for shutdown results.
    pub fn retire(self, audio: Audio) -> Vec<Receipt> {
        let paired = Arc::ptr_eq(&self.shared, &audio.shared);
        self.entries
            .iter()
            .flatten()
            .filter_map(|entry| entry.request)
            .map(|request| Receipt {
                request,
                outcome: if !paired {
                    State::Faulted
                } else {
                    match self.status(request) {
                        Ok(State::Pending) => State::Shutdown,
                        Ok(s) => s,
                        Err(_) => State::Faulted,
                    }
                },
            })
            .collect()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // Workers may retain their permit while closing native connections.
        // Owner shutdown closes admission even if no callback can run again.
        self.shared.closed.store(true, Ordering::Release);
    }
}

impl Audio {
    /// A refused release during final native-input dispatch must close the
    /// source even when Main and the ingress worker are stalled. Registration
    /// generations never repeat, so this cannot invalidate a newer binding.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn invalidate(&self, port: u16, generation: u64) {
        for shared in &self.shared.ports {
            if generation != 0 && shared.generation.load(Ordering::Acquire) == generation && shared.port.load(Ordering::Relaxed) == u32::from(port) {
                let previous = shared.audio_invalidated.load(Ordering::Relaxed);
                if previous < generation {
                    shared.audio_invalidated.store(generation, Ordering::Release);
                }
            }
        }
    }
    /// Validate AGAIN immediately before event dispatch. Main may have revoked
    /// this source since admission; the local tombstone also prevents a stale load
    /// of the shared admission flag from undoing an already acknowledged revoke.
    pub fn permits(&self, port: u16, generation: u64) -> bool {
        self.permits_inner(port, generation, None)
    }
    /// Final raw-ingress check immediately before dispatch, including a Stop
    /// cutoff that may have advanced since the worker admitted this event.
    pub fn permits_sequence(&self, port: u16, generation: u64, sequence: u64) -> bool {
        sequence != u64::MAX && self.permits_inner(port, generation, Some(sequence))
    }
    fn permits_inner(&self, port: u16, generation: u64, sequence: Option<u64>) -> bool {
        if self.shared.closed.load(Ordering::Acquire) || self.shared.faulted() {
            return false;
        }
        self.shared.ports.iter().enumerate().any(|(slot, shared)| {
            if generation == 0 || generation <= self.revoked[slot] {
                return false;
            }
            let observed = shared.generation.load(Ordering::Acquire);
            observed == generation
                && shared.port.load(Ordering::Relaxed) == u32::from(port)
                && shared.admitted.load(Ordering::Acquire)
                && shared.invalidated.load(Ordering::Acquire) < generation
                && shared.audio_invalidated.load(Ordering::Acquire) < generation
                && sequence.is_none_or(|sequence| sequence >= shared.cutoff.load(Ordering::Acquire))
                && shared.generation.load(Ordering::Acquire) == observed
        })
    }
    pub fn fault(&mut self) {
        self.shared.faulted.store(true, Ordering::Release);
    }

    /// One bounded round-robin scan per actual device callback, independent of
    /// normal note/transport/plan credits. Handler completion PRECEDES the ack.
    /// The restricted bridge view cannot adopt a plan or swap runtime ownership.
    pub fn service(
        &mut self,
        budget: &mut action_lane::Budget,
        bridge: &mut plan_bridge::Audio,
        apply: &mut impl FnMut(u16, u64, &mut plan_bridge::Processing<'_>) -> Result<(), ApplyError>,
    ) -> Service {
        if !bridge.matches_runtime(&self.shared.runtime) || bridge.faulted() {
            self.fault();
        }
        if self.shared.faulted() {
            return Service { revoked: 0, faulted: true };
        }
        if !budget.claim_source_scan() {
            return Service::default();
        }
        let mut result = Service::default();
        for _ in 0..PORTS {
            let slot = self.cursor;
            self.cursor = (self.cursor + 1) % PORTS;
            let port = &self.shared.ports[slot];
            let requested = port.requested.load(Ordering::Acquire);
            let generation = port.generation.load(Ordering::Acquire);
            let worker_recovery = generation != 0
                && (port.invalidated.load(Ordering::Acquire) >= generation || port.audio_invalidated.load(Ordering::Acquire) >= generation)
                && generation > self.revoked[slot];
            if requested <= self.seen[slot] && !worker_recovery {
                continue;
            }
            if generation <= self.revoked[slot] {
                // A later Main revoke can acknowledge the worker's completed
                // release without applying it again. This scan remains bounded.
                self.seen[slot] = requested;
                port.acknowledged.store(requested, Ordering::Release);
                continue;
            }
            let id = port.port.load(Ordering::Relaxed) as u16;
            let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(id, generation, &mut bridge.processing())));
            let succeeded = match applied {
                Ok(Ok(())) => !bridge.faulted(),
                Ok(Err(_)) => false,
                Err(payload) => {
                    self.panic_payload = Some(payload);
                    false
                }
            };
            if !succeeded {
                self.fault();
                bridge.fault();
                result.faulted = true;
                break;
            }
            self.revoked[slot] = self.revoked[slot].max(generation);
            self.seen[slot] = requested;
            port.acknowledged.store(requested, Ordering::Release);
            result.revoked += 1;
            if result.revoked == CALLBACK_REVOCATIONS {
                break;
            }
        }
        result
    }
}

#[cfg(test)]
mod tests;
