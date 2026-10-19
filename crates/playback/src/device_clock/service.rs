//! Native clock fitting independent of UI/editor/factory progress. Only Copy
//! snapshots cross to audio; a stalled reader cannot stall fitting or its peer.
use super::{ClockReader, ClockReport, HostOrigin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

const PERIOD: Duration = Duration::from_millis(5);

// The pinned buffer permits Sync handles for some non-Sync payloads. Require
// the actual payload's stronger bound independently of its handle auto traits.
const _: () = {
    const fn require_copy_send_sync<T: Copy + Send + Sync>() {}
    require_copy_send_sync::<ClockReport>();
};

struct State {
    stop: AtomicBool,
    finished: AtomicBool,
}

/// Sole consumer. Keep it on its owner, or in the main-anchored audio bundle.
/// No mutex, heap operation or unbounded retry occurs in `poll`.
pub struct SnapshotReader {
    receive: triple_buffer::Output<ClockReport>,
    state: Arc<State>,
    origin: HostOrigin,
}

impl SnapshotReader {
    pub fn poll(&mut self) -> ClockReport {
        let origin = self.origin;
        self.poll_with(|| origin.now_ns())
    }

    fn poll_with(&mut self, now: impl FnOnce() -> Option<u64>) -> ClockReport {
        // Exactly one mailbox update (one load and at most one swap in the
        // pinned dependency), followed by a Copy. Slow readers see the newest
        // publication instead of a FIFO that fills with obsolete reports.
        let mut report = *self.receive.read();
        let now = now();
        if self.state.stop.load(Ordering::Acquire) || self.state.finished.load(Ordering::Acquire) {
            report.map = None;
        } else {
            report.map = report.map.filter(|map| now.is_some_and(|now| map.valid(map.key, now).is_ok()));
        }
        report
    }
}

struct Pump {
    reader: ClockReader,
    ui: triple_buffer::Input<ClockReport>,
    audio: triple_buffer::Input<ClockReport>,
    state: Arc<State>,
}

impl Pump {
    fn step(&mut self, now: impl FnOnce() -> Option<u64>) -> bool {
        if self.state.stop.load(Ordering::Acquire) {
            return false;
        }
        let report = self.reader.poll_with(now);
        self.ui.write(report);
        self.audio.write(report);
        true
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.state.finished.store(true, Ordering::Release);
    }
}

fn prepare(reader: ClockReader) -> (Pump, SnapshotReader, SnapshotReader) {
    let origin = reader.inner.origin;
    let empty = ClockReport { map: None, publication: Default::default(), fitting: Default::default() };
    let state = Arc::new(State { stop: AtomicBool::new(false), finished: AtomicBool::new(false) });
    let (ui_send, ui_receive) = triple_buffer::triple_buffer(&empty);
    let (audio_send, audio_receive) = triple_buffer::triple_buffer(&empty);
    (
        Pump { reader, ui: ui_send, audio: audio_send, state: Arc::clone(&state) },
        SnapshotReader { receive: ui_receive, state: Arc::clone(&state), origin },
        SnapshotReader { receive: audio_receive, state, origin },
    )
}

/// Own on control/lifecycle, never audio. Dropping requests stop and wakes the
/// worker; it does not wait for OS scheduling or add a blocking UI join. The
/// worker owns only its bounded clock storage, never streams or plug-ins.
pub struct ClockService {
    ui: Mutex<SnapshotReader>,
    audio: Option<SnapshotReader>,
    state: Arc<State>,
    worker: std::thread::JoinHandle<()>,
}

impl ClockService {
    /// Start before the device starts. Failure leaves the caller with an explicit
    /// unavailable clock; it must not fall back to UI-driven fitting.
    pub fn start(reader: ClockReader) -> std::io::Result<Self> {
        let origin = reader.inner.origin;
        Self::start_with(reader, move || origin.now_ns(), PERIOD)
    }

    fn start_with(reader: ClockReader, mut now: impl FnMut() -> Option<u64> + Send + 'static, period: Duration) -> std::io::Result<Self> {
        let (mut pump, ui, audio) = prepare(reader);
        let state = Arc::clone(&pump.state);
        let worker = std::thread::Builder::new().name("soundcraft-device-clock".into()).spawn(move || {
            while pump.step(&mut now) {
                std::thread::park_timeout(period);
            }
        })?;
        Ok(Self { ui: Mutex::new(ui), audio: Some(audio), state, worker })
    }

    /// UI/control snapshot only; this lock is never touched by the worker or audio.
    pub fn report(&self) -> ClockReport {
        self.ui.lock().unwrap_or_else(PoisonError::into_inner).poll()
    }

    /// Transfer the separate bounded consumer to the main-anchored audio state.
    /// Taking it does not change UI reporting, and may happen only once.
    pub fn take_audio_reader(&mut self) -> Option<SnapshotReader> {
        self.audio.take()
    }

    pub fn request_stop(&self) {
        self.state.stop.store(true, Ordering::Release);
        self.worker.thread().unpark();
    }

    /// A lifecycle owner may use this to observe completion without waiting.
    pub fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }
}

impl Drop for ClockService {
    fn drop(&mut self) {
        self.request_stop();
        // JoinHandle drop detaches. The stop token wakes the bounded worker; no
        // device handle, plug-in owner or UI callback has been moved into it.
    }
}

#[cfg(test)]
mod tests;
