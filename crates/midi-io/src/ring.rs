//! The input push path (backend callback) and the consumer side of the ring.

use crate::clock::host_ns_since;
use crate::event::{InputEvent, is_release};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// How an input port is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputConfig {
    /// Ring slots any message may use.
    pub capacity: usize,
    /// Extra ring slots only release-type messages ([`is_release`](crate::is_release)) may use:
    /// once the free space falls to this many slots, other messages are dropped (and counted in
    /// [`InputCounters::dropped`]) while releases still enter. The ring holds
    /// `capacity + release_reserve` events.
    pub release_reserve: usize,
    /// Pass active sensing (`FE`) into the ring. Off by default: most controllers send it every
    /// 300 ms and it carries nothing a DAW acts on; filtered ones are counted.
    pub pass_active_sensing: bool,
    /// Origin of [`InputEvent::host_arrival_ns`]; defaults to [`host_origin`](crate::host_origin).
    pub host_origin: Instant,
}

impl InputConfig {
    /// `capacity` general slots plus a release reserve of `max(capacity / 8, 16)` slots.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self { capacity, release_reserve: (capacity / 8).max(16), pass_active_sensing: false, host_origin: crate::host_origin() }
    }
}

impl Default for InputConfig {
    /// 1024 general slots: at the 3125 messages/s ceiling of a DIN MIDI link, about a third of a
    /// second of input with nobody draining.
    fn default() -> Self {
        Self::new(1024)
    }
}

/// A snapshot of an input port's counters. Counts are cumulative since the port opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputCounters {
    /// Backend callbacks received.
    pub callbacks: u64,
    /// Messages pushed into the ring.
    pub delivered: u64,
    /// Non-release messages dropped because the ring was full down to the release reserve.
    pub dropped: u64,
    /// Release-type messages dropped because the ring, reserve included, was completely full.
    pub dropped_releases: u64,
    /// SysEx messages dropped (SysEx never enters the ring).
    pub sysex_dropped: u64,
    /// Bytes of those SysEx messages, `F0` and `F7` included.
    pub sysex_bytes_dropped: u64,
    /// Bytes that formed no valid message: data bytes with no running status, a message cut
    /// short by a status byte or by the end of the callback's bytes, undefined status bytes.
    pub malformed_bytes: u64,
    /// Messages filtered by configuration (active sensing).
    pub filtered: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    callbacks: AtomicU64,
    delivered: AtomicU64,
    dropped: AtomicU64,
    dropped_releases: AtomicU64,
    sysex_dropped: AtomicU64,
    sysex_bytes_dropped: AtomicU64,
    malformed_bytes: AtomicU64,
    filtered: AtomicU64,
}

fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

/// State shared by the push path, the handle, the consumer and the port watcher.
#[derive(Default)]
pub(crate) struct PortShared {
    pub(crate) counters: Counters,
    /// Set by a [`PortWatcher`](crate::PortWatcher) when the port leaves the backend's list.
    pub(crate) disconnected: AtomicBool,
}

impl PortShared {
    pub(crate) fn snapshot(&self) -> InputCounters {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        InputCounters {
            callbacks: get(&c.callbacks),
            delivered: get(&c.delivered),
            dropped: get(&c.dropped),
            dropped_releases: get(&c.dropped_releases),
            sysex_dropped: get(&c.sysex_dropped),
            sysex_bytes_dropped: get(&c.sysex_bytes_dropped),
            malformed_bytes: get(&c.malformed_bytes),
            filtered: get(&c.filtered),
        }
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    pub(crate) fn mark_disconnected(&self) {
        self.disconnected.store(true, Ordering::Release);
    }
}

/// Length of the message a status byte starts; `None` for SysEx start/end and undefined bytes.
const fn message_len(status: u8) -> Option<usize> {
    match status {
        0x80..=0xBF | 0xE0..=0xEF | 0xF2 => Some(3),
        0xC0..=0xDF | 0xF1 | 0xF3 => Some(2),
        0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE | 0xFF => Some(1),
        _ => None,
    }
}

/// The producer half, owned by the backend's input callback.
///
/// [`InputSink::push`] is the whole callback: it reads the clock once, splits the bytes into
/// messages and pushes each into the preallocated ring. It never allocates, locks, logs or waits.
pub(crate) struct InputSink {
    producer: rtrb::Producer<InputEvent>,
    shared: Arc<PortShared>,
    reserve: usize,
    pass_active_sensing: bool,
    origin: Instant,
    next_seq: u64,
    /// A SysEx message continues from the previous callback (WinMM delivers long SysEx in
    /// 1024-byte buffers; continuation chunks start with data bytes).
    in_sysex: bool,
}

impl InputSink {
    /// The ring and both halves for one open port.
    pub(crate) fn new(config: &InputConfig) -> (Self, InputConsumer, Arc<PortShared>) {
        let (producer, consumer) = rtrb::RingBuffer::new(config.capacity.saturating_add(config.release_reserve));
        let shared = Arc::new(PortShared::default());
        let sink = Self {
            producer,
            shared: Arc::clone(&shared),
            reserve: config.release_reserve,
            pass_active_sensing: config.pass_active_sensing,
            origin: config.host_origin,
            next_seq: 0,
            in_sysex: false,
        };
        (sink, InputConsumer { consumer, shared: Arc::clone(&shared) }, shared)
    }

    /// One backend callback: `data` holds one or more messages (or a SysEx chunk) the backend
    /// observed at `backend_time_us`. Running status is honored within one callback only.
    pub(crate) fn push(&mut self, backend_time_us: u64, data: &[u8]) {
        let host = host_ns_since(self.origin);
        bump(&self.shared.counters.callbacks, 1);
        let mut running: Option<u8> = None;
        let mut i = 0;
        while let Some(&b) = data.get(i) {
            if b >= 0xF8 {
                // Real-time bytes may appear anywhere, even inside other messages.
                self.real_time(b, backend_time_us, host);
                i += 1;
                continue;
            }
            if self.in_sysex {
                // Data bytes and the closing F7 belong to the SysEx; any other status ends it.
                if b < 0x80 || b == 0xF7 {
                    bump(&self.shared.counters.sysex_bytes_dropped, 1);
                    self.in_sysex = b != 0xF7;
                    i += 1;
                    continue;
                }
                self.in_sysex = false;
            }
            if b == 0xF0 {
                bump(&self.shared.counters.sysex_dropped, 1);
                bump(&self.shared.counters.sysex_bytes_dropped, 1);
                self.in_sysex = true;
                running = None;
                i += 1;
                continue;
            }
            let (status, start) = if b >= 0x80 {
                (b, i + 1)
            } else if let Some(s) = running {
                (s, i)
            } else {
                bump(&self.shared.counters.malformed_bytes, 1);
                i += 1;
                continue;
            };
            let Some(len) = message_len(status) else {
                // Undefined (F4, F5) or a stray EOX (F7).
                bump(&self.shared.counters.malformed_bytes, 1);
                running = None;
                i += 1;
                continue;
            };
            running = if status < 0xF0 { Some(status) } else { None };
            let mut bytes = [status, 0, 0];
            let mut got = 1;
            let mut j = start;
            while got < len {
                match data.get(j) {
                    Some(&d) if d < 0x80 => {
                        if let Some(slot) = bytes.get_mut(got) {
                            *slot = d;
                        }
                        got += 1;
                        j += 1;
                    }
                    Some(&d) if d >= 0xF8 => {
                        self.real_time(d, backend_time_us, host);
                        j += 1;
                    }
                    // Cut short by another status byte or by the end of the bytes.
                    _ => break,
                }
            }
            if got == len {
                self.offer(bytes, len, backend_time_us, host);
            } else {
                // No need to clear running status: the next byte is a status byte, which sets or
                // clears it itself, or the callback's bytes have ended.
                bump(&self.shared.counters.malformed_bytes, (got - usize::from(start == i)).max(1) as u64);
            }
            i = j.max(i + 1);
        }
    }

    fn real_time(&mut self, b: u8, backend_time_us: u64, host: u64) {
        match message_len(b) {
            Some(_) if b == 0xFE && !self.pass_active_sensing => bump(&self.shared.counters.filtered, 1),
            Some(_) => self.offer([b, 0, 0], 1, backend_time_us, host),
            None => bump(&self.shared.counters.malformed_bytes, 1),
        }
    }

    fn offer(&mut self, bytes: [u8; 3], len: usize, backend_time_us: u64, host_arrival_ns: u64) {
        let seq = self.next_seq;
        self.next_seq = seq.wrapping_add(1);
        let event = InputEvent { bytes, len: len as u8, backend_time_us, host_arrival_ns, seq };
        let release = is_release(event.message());
        let free = self.producer.slots();
        let admit = if release { free > 0 } else { free > self.reserve };
        let counters = &self.shared.counters;
        if admit && self.producer.push(event).is_ok() {
            bump(&counters.delivered, 1);
        } else if release {
            bump(&counters.dropped_releases, 1);
        } else {
            bump(&counters.dropped, 1);
        }
    }
}

/// The consumer half of an input port's ring: what the realtime side (playback's source
/// registry) reads.
///
/// `pop` is wait-free and allocation-free, so the consumer may move to the audio thread. Do not
/// drop it there: if it is the ring's last owner, dropping frees the ring.
pub struct InputConsumer {
    consumer: rtrb::Consumer<InputEvent>,
    shared: Arc<PortShared>,
}

impl InputConsumer {
    /// The oldest pending event, if any.
    pub fn pop(&mut self) -> Option<InputEvent> {
        self.consumer.pop().ok()
    }

    /// The oldest pending event without removing it.
    #[must_use]
    pub fn peek(&self) -> Option<&InputEvent> {
        self.consumer.peek().ok()
    }

    /// Events pending.
    #[must_use]
    pub fn len(&self) -> usize {
        self.consumer.slots()
    }

    /// No event pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.consumer.is_empty()
    }

    /// The port was closed (its [`InputHandle`](crate::InputHandle) dropped): no further events
    /// will arrive. Events already in the ring stay readable.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.consumer.is_abandoned()
    }

    /// A [`PortWatcher`](crate::PortWatcher) saw the port vanish. Events already in the ring stay
    /// readable; the owner should release what the port holds (F10 source revocation).
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.shared.is_disconnected()
    }

    /// The port's counters (atomics, readable from any thread).
    #[must_use]
    pub fn counters(&self) -> InputCounters {
        self.shared.snapshot()
    }
}
