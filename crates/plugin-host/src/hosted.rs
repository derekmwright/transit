//! An instance adopted by its host thread ([`crate::Ctx::adopt`]): the forwarder the render graph
//! holds, and the editor and state proxies the UI holds.

use crate::{Host, Inner, Worker, lock};
use soundcraft_dsp::event::PluginEvent;
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, PluginStateReader, ProcessStatus, SampleSource, StateBusy, Transport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// What the UI is told when the instance's host thread is wedged or gone.
pub(crate) const NOT_RESPONDING: &str = "the plug-in is not responding (its host thread is stuck)";

/// Editor state the host thread publishes and the UI's proxy reads.
#[derive(Default)]
pub(crate) struct EditorShared {
    open: AtomicBool,
    /// An open request is on its way: the proxy reports the editor open meanwhile.
    opening: AtomicBool,
    /// A close request is on its way: the host's ticks leave `open` alone meanwhile.
    closing: AtomicBool,
    edits: Mutex<Vec<(String, f32)>>,
    error: Mutex<Option<String>>,
}

impl EditorShared {
    /// From the host thread's tick: the editor's edits and whether its window is open.
    pub(crate) fn publish(&self, edits: Vec<(String, f32)>, open: bool) {
        if !edits.is_empty() {
            lock(&self.edits).extend(edits);
        }
        if !self.opening.load(Ordering::Acquire) && !self.closing.load(Ordering::Acquire) {
            self.open.store(open, Ordering::Release);
        }
    }
}

/// Keeps an adopted instance's handles alive on its host thread while the instance or any proxy
/// lives; the last one to go removes them there.
pub(crate) struct Token {
    key: u64,
    worker: Arc<Worker>,
    shared: Arc<EditorShared>,
    timeout: Duration,
    host: Weak<Inner>,
}

impl Token {
    pub(crate) fn new(key: u64, worker: Arc<Worker>, shared: Arc<EditorShared>, timeout: Duration, host: Weak<Inner>) -> Arc<Token> {
        Arc::new(Token { key, worker, shared, timeout, host })
    }

    /// Runs `f` with this instance's editor on its host thread (fire and forget).
    fn with_editor(&self, f: impl FnOnce(Option<&mut Box<dyn PluginEditor>>, &EditorShared) + Send + 'static) {
        let (key, shared) = (self.key, Arc::clone(&self.shared));
        self.worker.send_or_leak(move |ctx| f(ctx.local(key).and_then(|l| l.editor.as_mut()), &shared));
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        let key = self.key;
        self.worker.send_or_leak(move |ctx| {
            ctx.entries.remove(&key);
        });
    }
}

/// The forwarder: every [`Plugin`] call goes straight to the instance on the caller's thread (the
/// render graph calls `process` on the audio thread, as before); `editor` and `state_reader` hand
/// out proxies to the handles on the host thread; dropping it sends the instance there.
pub(crate) struct Hosted {
    /// Always `Some` until `drop` takes it.
    inner: Option<Box<dyn Plugin>>,
    info: &'static PluginInfo,
    token: Arc<Token>,
    has_editor: bool,
    has_state: bool,
}

impl Hosted {
    pub(crate) fn new(inner: Box<dyn Plugin>, token: Arc<Token>, has_editor: bool, has_state: bool) -> Hosted {
        let info = inner.info();
        Hosted { inner: Some(inner), info, token, has_editor, has_state }
    }
}

impl Drop for Hosted {
    fn drop(&mut self) {
        if let Some(plugin) = self.inner.take() {
            // Destroyed on its own thread, after any request queued before; leaked if that
            // thread is gone. `token` drops after this, so the handles go after the instance.
            self.token.worker.send_or_leak(move |_| drop(plugin));
        }
    }
}

impl Plugin for Hosted {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }
    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        if let Some(p) = &mut self.inner {
            p.prepare(sample_rate, max_block, channels);
        }
    }
    fn reset(&mut self) {
        if let Some(p) = &mut self.inner {
            p.reset();
        }
    }
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        self.inner.as_mut().is_some_and(|p| p.set_param(id, value))
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.as_ref().and_then(|p| p.param(id))
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if let Some(p) = &mut self.inner {
            p.process(io, frames);
        }
    }
    fn process_with_key(&mut self, io: &mut [Vec<f32>], key: &[Vec<f32>], frames: usize) {
        if let Some(p) = &mut self.inner {
            p.process_with_key(io, key, frames);
        }
    }
    fn has_key_input(&self) -> bool {
        self.inner.as_ref().is_some_and(|p| p.has_key_input())
    }
    fn latency(&self) -> usize {
        self.inner.as_ref().map_or(0, |p| p.latency())
    }
    fn tail_samples(&self) -> usize {
        self.inner.as_ref().map_or(0, |p| p.tail_samples())
    }
    fn process_status(&self) -> ProcessStatus {
        self.inner.as_ref().map_or(ProcessStatus::Continue, |p| p.process_status())
    }
    fn set_input_silent(&mut self, silent: bool) {
        if let Some(p) = &mut self.inner {
            p.set_input_silent(silent);
        }
    }
    fn gain_reduction_db(&self) -> f32 {
        self.inner.as_ref().map_or(0.0, |p| p.gain_reduction_db())
    }
    fn set_transport(&mut self, transport: &Transport) {
        if let Some(p) = &mut self.inner {
            p.set_transport(transport);
        }
    }
    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        if let Some(p) = &mut self.inner {
            p.note_on(offset, note, velocity);
        }
    }
    fn note_off(&mut self, offset: usize, note: u8) {
        if let Some(p) = &mut self.inner {
            p.note_off(offset, note);
        }
    }
    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        if let Some(p) = &mut self.inner {
            p.midi(offset, msg);
        }
    }
    fn all_notes_off(&mut self) {
        if let Some(p) = &mut self.inner {
            p.all_notes_off();
        }
    }
    fn event(&mut self, offset: u32, ev: &PluginEvent) -> bool {
        self.inner.as_mut().is_some_and(|p| p.event(offset, ev))
    }
    fn set_param_at(&mut self, offset: u32, id: &str, value: f32) -> bool {
        self.inner.as_mut().is_some_and(|p| p.set_param_at(offset, id, value))
    }
    fn set_sample(&mut self, sample: Option<Arc<dyn SampleSource>>) -> bool {
        self.inner.as_mut().is_some_and(|p| p.set_sample(sample))
    }
    /// The instance's own, on the calling thread (as before adoption); hosts read states through
    /// [`Plugin::state_reader`], which goes to the host thread.
    fn save_state(&mut self) -> Option<Vec<u8>> {
        self.inner.as_mut().and_then(|p| p.save_state())
    }
    fn load_state(&mut self, data: &[u8]) -> bool {
        self.inner.as_mut().is_some_and(|p| p.load_state(data))
    }
    fn open_editor(&mut self) -> Result<(), String> {
        self.editor().ok_or_else(|| "no editor".to_string())?.open()
    }
    fn close_editor(&mut self) {
        if let Some(mut e) = self.editor() {
            e.close();
        }
    }
    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        self.has_editor.then(|| Box::new(RemoteEditor { token: Arc::clone(&self.token) }) as Box<dyn PluginEditor>)
    }
    fn state_reader(&mut self) -> Option<Box<dyn PluginStateReader>> {
        self.has_state.then(|| Box::new(RemoteState { token: Arc::clone(&self.token) }) as Box<dyn PluginStateReader>)
    }
}

/// The UI's handle to an editor on the host thread. Open, close and parameter echoes are sent and
/// not waited for; the window state and the edits come back through [`EditorShared`].
struct RemoteEditor {
    token: Arc<Token>,
}

impl PluginEditor for RemoteEditor {
    fn open(&mut self) -> Result<(), String> {
        if !self.token.worker.responsive() {
            return Err(NOT_RESPONDING.into());
        }
        let shared = &self.token.shared;
        lock(&shared.error).take();
        shared.closing.store(false, Ordering::Release);
        shared.opening.store(true, Ordering::Release);
        self.token.with_editor(|editor, shared| {
            match editor.map(|e| e.open()) {
                Some(Ok(())) => shared.open.store(true, Ordering::Release),
                Some(Err(e)) => {
                    shared.open.store(false, Ordering::Release);
                    *lock(&shared.error) = Some(e);
                }
                None => *lock(&shared.error) = Some("this plug-in instance is gone".into()),
            }
            shared.opening.store(false, Ordering::Release);
        });
        Ok(())
    }

    fn close(&mut self) {
        let shared = &self.token.shared;
        shared.opening.store(false, Ordering::Release);
        shared.closing.store(true, Ordering::Release);
        shared.open.store(false, Ordering::Release);
        self.token.with_editor(|editor, shared| {
            if let Some(e) = editor {
                e.close();
            }
            shared.closing.store(false, Ordering::Release);
        });
    }

    fn is_open(&self) -> bool {
        let shared = &self.token.shared;
        // An open still on its way counts only while its thread can run it (a wedged thread's
        // pending open would otherwise read open forever; it clears the flag when it returns).
        shared.open.load(Ordering::Acquire) || (shared.opening.load(Ordering::Acquire) && self.token.worker.responsive())
    }

    fn idle(&mut self) -> Vec<(String, f32)> {
        std::mem::take(&mut *lock(&self.token.shared.edits))
    }

    fn set_param(&mut self, id: &str, value: f32) {
        let id = id.to_owned();
        self.token.with_editor(move |editor, _| {
            if let Some(e) = editor {
                e.set_param(&id, value);
            }
        });
    }

    fn take_error(&mut self) -> Option<String> {
        lock(&self.token.shared.error).take()
    }
}

/// The UI's state reader: a request to the host thread, waited for up to the timeout (the one
/// place the UI waits on a plug-in, ADR 0018 section 6). A wedged or gone thread answers `None`
/// at once. A save uses [`PluginStateReader::save_state_within`] with its own deadline instead.
struct RemoteState {
    token: Arc<Token>,
}

impl RemoteState {
    /// Reads the state on the host thread, waiting at most `timeout`; `Err` when it did not
    /// answer in time, its thread is wedged or gone, or the thread has been inside one window
    /// message (a modal dialog) for longer than the request timeout: that one answers at once.
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, StateBusy> {
        let worker = &self.token.worker;
        if !worker.responsive() || worker.in_dispatch_longer_than(self.token.timeout) {
            return Err(StateBusy);
        }
        if worker.is_current_thread() {
            log::error!("plug-in host: a state read from the host thread itself would wait for itself");
            return Err(StateBusy);
        }
        let host = Host { inner: self.token.host.upgrade().ok_or(StateBusy)? };
        let key = self.token.key;
        host.submit_to(worker, move |ctx| ctx.local(key).and_then(|l| l.state.as_mut()).and_then(|s| s.save_state())).wait(timeout).ok_or(StateBusy)
    }
}

impl PluginStateReader for RemoteState {
    fn save_state(&mut self) -> Option<Vec<u8>> {
        self.read(self.token.timeout).ok().flatten()
    }

    fn save_state_within(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, StateBusy> {
        self.read(timeout.min(self.token.timeout))
    }
}
