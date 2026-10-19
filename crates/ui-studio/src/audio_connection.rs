//! The device lifetime boundary. Fakes replace only the device operations; reconnect and editor
//! retry logic use the same ordering as physical devices.

use soundcraft_model::TrackId;
use soundcraft_playback::{AudioConfig, Player, record::Recorder, telemetry::CallbackTelemetry};
use std::{cell::RefCell, rc::Rc};

pub(crate) trait Editors {
    fn close_editors(&self);
    fn editor_open(&self, track: TrackId, slot: usize) -> bool;
    fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String>;
}

pub(crate) trait Connection: Editors {
    type Input;
    fn close_input(&self, input: Self::Input);
    fn open_input(&self, cfg: &AudioConfig) -> Result<Self::Input, String>;
    fn monitor(&self, input: &Self::Input);
    fn monitored_input(&self, cfg: &AudioConfig) -> Result<Self::Input, String> {
        let r = self.open_input(cfg)?;
        self.monitor(&r);
        Ok(r)
    }
}

impl Editors for Player {
    fn close_editors(&self) {
        self.close_editors();
    }
    fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.editor_open(track, slot)
    }
    fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        self.open_editor(track, slot)
    }
}

impl Connection for Player {
    type Input = Recorder;
    fn close_input(&self, input: Recorder) {
        self.close_input(input);
    }
    fn open_input(&self, cfg: &AudioConfig) -> Result<Recorder, String> {
        self.open_input(cfg)
    }
    fn monitor(&self, r: &Recorder) {
        self.set_input(std::sync::Arc::clone(&r.monitor));
    }
}

pub(crate) fn replace<P: Connection>(player: &mut Option<P>, input: &mut Option<P::Input>, open: impl FnOnce() -> P) {
    if let Some(p) = player.as_ref() {
        p.close_editors();
    }
    if let Some(r) = input.take()
        && let Some(p) = player.as_ref()
    {
        p.close_input(r);
    }
    // Assignment must finish (including Drop) before the loader runs.
    *player = None;
    *player = Some(open());
}

pub(crate) type Trace = Rc<RefCell<Vec<&'static str>>>;

#[derive(Debug)]
pub(crate) struct FakeConnection {
    pub config: AudioConfig,
    pub telemetry: CallbackTelemetry,
    pub failed: bool,
    pub editors: RefCell<Vec<(TrackId, usize)>>,
    trace: Trace,
}

impl FakeConnection {
    pub fn new(config: AudioConfig, trace: Trace) -> Self {
        trace.borrow_mut().push("open output");
        Self { config, telemetry: CallbackTelemetry::default(), failed: false, editors: RefCell::default(), trace }
    }
}

impl Drop for FakeConnection {
    fn drop(&mut self) {
        self.trace.borrow_mut().push("drop output");
    }
}

#[derive(Debug)]
pub(crate) struct FakeInput(Trace);

impl Drop for FakeInput {
    fn drop(&mut self) {
        self.0.borrow_mut().push("drop input");
    }
}

impl Editors for FakeConnection {
    fn close_editors(&self) {
        self.trace.borrow_mut().push("close editors");
        self.editors.borrow_mut().clear();
    }
    fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.editors.borrow().contains(&(track, slot))
    }
    fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        self.trace.borrow_mut().push("open editor");
        self.editors.borrow_mut().push((track, slot));
        Ok(())
    }
}

impl Connection for FakeConnection {
    type Input = FakeInput;
    fn close_input(&self, input: FakeInput) {
        drop(input);
    }
    fn open_input(&self, _cfg: &AudioConfig) -> Result<FakeInput, String> {
        self.trace.borrow_mut().push("open input");
        Ok(FakeInput(Rc::clone(&self.trace)))
    }
    fn monitor(&self, _input: &FakeInput) {
        self.trace.borrow_mut().push("connect monitor");
    }
}
