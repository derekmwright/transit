//! The device lifetime boundary. Fakes replace only the device operations; reconnect and editor
//! retry logic use the same ordering as physical devices.

use soundcraft_model::TrackId;
use soundcraft_playback::{AudioConfig, Player, record::Recorder, telemetry::CallbackTelemetry};
use std::{cell::RefCell, rc::Rc};

pub(crate) trait Editors {
    fn close_editors(&self) -> Result<(), String>;
    fn editor_open(&self, track: TrackId, slot: usize) -> bool;
    fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String>;
}

pub(crate) trait Connection: Editors {
    type Input;
    fn close_input(&self, input: &mut Option<Self::Input>) -> Result<(), String>;
    fn open_input(&self, cfg: &AudioConfig) -> Result<Self::Input, String>;
    fn monitor(&self, input: &Self::Input) -> Result<(), String>;
    fn monitored_input(&self, cfg: &AudioConfig) -> Result<Self::Input, String> {
        let r = self.open_input(cfg)?;
        self.monitor(&r)?;
        Ok(r)
    }
}

impl Editors for Player {
    fn close_editors(&self) -> Result<(), String> {
        self.close_editors().map_err(|e| e.to_string())
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
    fn close_input(&self, input: &mut Option<Recorder>) -> Result<(), String> {
        self.close_input(input).map_err(|e| e.to_string())
    }
    fn open_input(&self, cfg: &AudioConfig) -> Result<Recorder, String> {
        self.open_input(cfg)
    }
    fn monitor(&self, r: &Recorder) -> Result<(), String> {
        self.set_input(std::sync::Arc::clone(&r.monitor)).map(|_| ()).map_err(|e| e.to_string())
    }
}

pub(crate) fn replace<P: Connection>(player: &mut Option<P>, input: &mut Option<P::Input>, open: impl FnOnce() -> P) -> Result<(), String> {
    if let Some(p) = player.as_ref() {
        p.close_editors()?;
        p.close_input(input)?;
    }
    if input.is_some() {
        return Err("Cannot replace audio while an input is still owned by the caller".into());
    }
    // Assignment must finish (including Drop) before the loader runs.
    *player = None;
    *player = Some(open());
    Ok(())
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
    fn close_editors(&self) -> Result<(), String> {
        self.trace.borrow_mut().push("close editors");
        self.editors.borrow_mut().clear();
        Ok(())
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
    fn close_input(&self, input: &mut Option<FakeInput>) -> Result<(), String> {
        *input = None;
        Ok(())
    }
    fn open_input(&self, _cfg: &AudioConfig) -> Result<FakeInput, String> {
        self.trace.borrow_mut().push("open input");
        Ok(FakeInput(Rc::clone(&self.trace)))
    }
    fn monitor(&self, _input: &FakeInput) -> Result<(), String> {
        self.trace.borrow_mut().push("connect monitor");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use soundcraft_playback::PlayerError;
    use std::cell::Cell;

    struct RefusingConnection {
        inner: FakeConnection,
        refusal: Cell<Option<PlayerError>>,
    }

    impl Editors for RefusingConnection {
        fn close_editors(&self) -> Result<(), String> {
            self.inner.close_editors()
        }
        fn editor_open(&self, track: TrackId, slot: usize) -> bool {
            self.inner.editor_open(track, slot)
        }
        fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
            self.inner.open_editor(track, slot)
        }
    }

    impl Connection for RefusingConnection {
        type Input = FakeInput;
        fn close_input(&self, input: &mut Option<FakeInput>) -> Result<(), String> {
            if let Some(error) = self.refusal.get() {
                return Err(error.to_string());
            }
            self.inner.close_input(input)
        }
        fn open_input(&self, cfg: &AudioConfig) -> Result<FakeInput, String> {
            self.inner.open_input(cfg)
        }
        fn monitor(&self, input: &FakeInput) -> Result<(), String> {
            self.inner.monitor(input)
        }
    }

    #[test]
    fn refused_input_return_keeps_both_owners_and_never_opens_a_replacement() {
        for refusal in [PlayerError::Busy, PlayerError::RecordingActive] {
            let trace = Trace::default();
            let mut player =
                Some(RefusingConnection { inner: FakeConnection::new(AudioConfig::default(), Rc::clone(&trace)), refusal: Cell::new(Some(refusal)) });
            let mut input = Some(player.as_ref().unwrap().open_input(&AudioConfig::default()).unwrap());
            trace.borrow_mut().clear();
            let open = || RefusingConnection { inner: FakeConnection::new(AudioConfig::default(), Rc::clone(&trace)), refusal: Cell::new(None) };
            assert_eq!(replace(&mut player, &mut input, open), Err(refusal.to_string()));
            assert!(player.is_some() && input.is_some());
            assert_eq!(*trace.borrow(), ["close editors"]);
            // Main becomes available, or the caller has finished and collected the take.
            player.as_ref().unwrap().refusal.set(None);
            trace.borrow_mut().clear();
            replace(&mut player, &mut input, open).unwrap();
            assert_eq!(*trace.borrow(), ["close editors", "drop input", "drop output", "open output"]);
            assert!(input.is_none());
        }
    }
}
