//! Latch input placement in the independent clock worker before disk work.
use super::{Reader, Status};
use crate::device_clock::{ClockKey, ClockMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

const PENDING: u8 = 0;
const READY: u8 = 1;
const FAILED: u8 = 2;

struct Shared {
    state: AtomicU8,
    frame: AtomicU64,
}

pub(crate) struct Mapping {
    start: Reader,
    input: ClockKey,
    shared: Arc<Shared>,
}

pub(crate) struct InputStart {
    shared: Arc<Shared>,
    finished: Option<Arc<AtomicBool>>,
}

pub(crate) fn pair(start: Reader, input: ClockKey) -> (Mapping, InputStart) {
    let shared = Arc::new(Shared { state: AtomicU8::new(PENDING), frame: AtomicU64::new(0) });
    (Mapping { start, input, shared: Arc::clone(&shared) }, InputStart { shared, finished: None })
}

impl Mapping {
    /// Called by clock maintenance, never by the audio callback or disk writer.
    /// Returns true after the one immutable result has been published.
    pub(crate) fn service(&mut self, map: Option<ClockMap>, now: Option<u64>) -> bool {
        if self.shared.state.load(Ordering::Acquire) != PENDING {
            return true;
        }
        let point = match self.start.status() {
            Status::Pending => return false,
            Status::Started(point) => point,
            Status::Cancelled => {
                self.shared.state.store(FAILED, Ordering::Release);
                return true;
            }
        };
        match map.zip(now).and_then(|(map, now)| map.host_to_frame(self.input, point.host_ns, now).ok()) {
            Some(frame) => {
                self.shared.frame.store(frame, Ordering::Relaxed);
                self.shared.state.store(READY, Ordering::Release);
            }
            None => self.shared.state.store(FAILED, Ordering::Release),
        }
        true
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // Service failure, shutdown and abandoned queued requests cannot strand
        // a stopped disk writer waiting for an acknowledgment that will not come.
        let _ = self.shared.state.compare_exchange(PENDING, FAILED, Ordering::Release, Ordering::Relaxed);
    }
}

impl InputStart {
    pub(crate) fn with_lifetime(mut self, finished: Arc<AtomicBool>) -> Self {
        self.finished = Some(finished);
        self
    }
    pub(crate) fn frame(&self) -> Result<Option<u64>, &'static str> {
        let mut state = self.shared.state.load(Ordering::Acquire);
        if state == PENDING && self.finished.as_ref().is_some_and(|finished| finished.load(Ordering::Acquire)) {
            // Completion may have published between the first read and worker
            // teardown. Its release precedes `finished`; revalidate once.
            state = self.shared.state.load(Ordering::Acquire);
            if state == PENDING {
                state = FAILED;
            }
        }
        match state {
            READY => Ok(Some(self.shared.frame.load(Ordering::Relaxed))),
            PENDING => Ok(None),
            _ => Err("recording start could not be mapped by the input clock worker"),
        }
    }
}
