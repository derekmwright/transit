//! MIDI recording capture: the audio thread copies every admitted live event, with the device
//! frame it is placed at, into a bounded lock-free queue; the control side drains it into a
//! take list. The queue is preallocated when the player is prepared, so the callback never
//! allocates, locks or waits; a full queue drops the event and counts it.
use soundcraft_mix::Message;
use soundcraft_model::TrackId;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The most events one recording pass holds, in the queue and in the control-side take list.
pub const CAPACITY: usize = 64 * 1024;
/// Slots kept free for releases (note-offs, pedal-ups): losing one leaves a note open.
const RELEASE_RESERVE: usize = 1024;

/// One admitted event as the instrument receives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturedEvent {
    /// The output-device frame the event is placed at (fitted from the device clock, else the
    /// approximate callback-relative placement).
    pub frame: u64,
    pub track: TrackId,
    pub channel: u8,
    pub message: Message,
    pub approximate: bool,
}

#[derive(Default)]
struct Counters {
    /// Events the audio side dropped because the queue was full.
    dropped: AtomicU64,
    /// Events the audio side has committed, for the UI's provisional notes.
    committed: AtomicU64,
}

/// The audio thread's producer. All operations are bounded and allocation free.
pub struct Sink {
    queue: rtrb::Producer<CapturedEvent>,
    counters: Arc<Counters>,
}

/// The control side's consumer and take list.
pub struct Capture {
    queue: rtrb::Consumer<CapturedEvent>,
    counters: Arc<Counters>,
    events: Vec<CapturedEvent>,
    dropped_seen: u64,
    /// Events the take list refused (it holds [`CAPACITY`]).
    overflow: u64,
    #[cfg(feature = "test-hooks")]
    injected: Vec<CapturedEvent>,
}

pub fn pair(capacity: usize) -> (Capture, Sink) {
    let (producer, consumer) = rtrb::RingBuffer::new(capacity.max(1));
    let counters = Arc::new(Counters::default());
    (
        Capture {
            queue: consumer,
            counters: Arc::clone(&counters),
            events: Vec::new(),
            dropped_seen: 0,
            overflow: 0,
            #[cfg(feature = "test-hooks")]
            injected: Vec::new(),
        },
        Sink { queue: producer, counters },
    )
}

impl Sink {
    /// Push one event; false when it was dropped (and counted). A non-release event leaves
    /// [`RELEASE_RESERVE`] slots to releases.
    pub fn push(&mut self, event: CapturedEvent) -> bool {
        let free = self.queue.slots();
        let room = if event.message.is_release() { free > 0 } else { free > RELEASE_RESERVE.min(self.queue.buffer().capacity() / 2) };
        if room && self.queue.push(event).is_ok() {
            self.counters.committed.fetch_add(1, Ordering::Release);
            true
        } else {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

impl Capture {
    /// Move everything the audio side committed into the take list. Control thread, any rate.
    pub fn poll(&mut self) {
        while let Ok(event) = self.queue.pop() {
            if self.events.len() < CAPACITY {
                self.events.push(event);
            } else {
                self.overflow += 1;
            }
        }
        #[cfg(feature = "test-hooks")]
        self.events.append(&mut self.injected);
    }

    /// The events captured so far, oldest first (after [`Capture::poll`]).
    pub fn events(&self) -> &[CapturedEvent] {
        &self.events
    }

    /// Events lost to a full queue or take list since the last [`Capture::clear`].
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed).saturating_sub(self.dropped_seen) + self.overflow
    }

    /// Events the audio side has committed since the player was prepared.
    pub fn committed(&self) -> u64 {
        self.counters.committed.load(Ordering::Acquire)
    }

    /// Start a new pass: forget the take list and the drop count. Leftovers still in the queue
    /// from before the pass are discarded.
    pub fn clear(&mut self) {
        while self.queue.pop().is_ok() {}
        self.events.clear();
        self.overflow = 0;
        self.dropped_seen = self.counters.dropped.load(Ordering::Relaxed);
        #[cfg(feature = "test-hooks")]
        self.injected.clear();
    }

    /// Take the list and the drop count, ending the pass.
    pub fn finish(&mut self) -> (Vec<CapturedEvent>, u64) {
        self.poll();
        let dropped = self.dropped();
        let events = std::mem::take(&mut self.events);
        self.overflow = 0;
        self.dropped_seen = self.counters.dropped.load(Ordering::Relaxed);
        (events, dropped)
    }

    /// Stand in for the audio thread in device-free tests.
    #[cfg(feature = "test-hooks")]
    pub fn inject(&mut self, event: CapturedEvent) {
        self.injected.push(event);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use soundcraft_mix::NoteId;
    use soundcraft_rt::testing::measure;

    fn on(frame: u64) -> CapturedEvent {
        CapturedEvent {
            frame,
            track: TrackId(1),
            channel: 0,
            message: Message::NoteOn { key: 60, velocity: 100, id: NoteId::NONE },
            approximate: false,
        }
    }
    fn off(frame: u64) -> CapturedEvent {
        CapturedEvent { message: Message::NoteOff { key: 60, velocity: 0, id: NoteId::NONE }, ..on(frame) }
    }

    #[test]
    fn pushing_allocates_nothing_and_keeps_frames_in_order() {
        let (mut capture, mut sink) = pair(64);
        let (pushed, heap) = measure(|| (0..10).filter(|i| sink.push(on(*i * 100))).count());
        assert!(heap.is_zero(), "{heap:?}");
        assert_eq!(pushed, 10);
        assert_eq!(capture.committed(), 10);
        capture.poll();
        assert_eq!(capture.events().iter().map(|e| e.frame).collect::<Vec<_>>(), (0..10).map(|i| i * 100).collect::<Vec<_>>());
        assert_eq!(capture.dropped(), 0);
    }

    #[test]
    fn a_full_queue_drops_and_counts_but_keeps_room_for_releases() {
        let (mut capture, mut sink) = pair(8);
        let ((ons, offs), heap) = measure(|| {
            let ons = (0..8).filter(|i| sink.push(on(*i))).count();
            let offs = (0..8).filter(|i| sink.push(off(100 + *i))).count();
            (ons, offs)
        });
        assert!(heap.is_zero(), "{heap:?}");
        // Half the queue is reserved for releases in a queue this small.
        assert_eq!((ons, offs), (4, 4));
        assert_eq!(capture.dropped(), 8);
        capture.poll();
        assert_eq!(capture.events().len(), 8);
        let (events, dropped) = capture.finish();
        assert_eq!((events.len(), dropped), (8, 8));
        assert_eq!(capture.dropped(), 0, "finish ends the pass");
    }

    #[test]
    fn clear_discards_what_came_before_the_pass() {
        let (mut capture, mut sink) = pair(16);
        sink.push(on(1));
        capture.poll();
        sink.push(on(2));
        capture.clear();
        assert!(capture.events().is_empty());
        capture.poll();
        assert!(capture.events().is_empty());
    }

    #[test]
    fn draining_for_the_journal_leaves_the_audio_side_push_allocation_free() {
        // The control side polls and reads `events()` every frame to append them to the MIDI
        // journal; the audio side's `push` stays bounded and allocation free around it.
        let (mut capture, mut sink) = pair(64);
        for round in 0..4 {
            let (pushed, heap) = measure(|| (0..8).filter(|i| sink.push(on(round * 100 + *i))).count());
            assert!(heap.is_zero(), "{heap:?}");
            assert_eq!(pushed, 8);
            capture.poll();
            assert_eq!(capture.events().len(), usize::try_from((round + 1) * 8).unwrap());
        }
    }
}
