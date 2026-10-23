//! Fake plug-ins for the host-thread tests: slow to create and to destroy, with an editor and a
//! state reader that record the thread they run on.
#![allow(dead_code, clippy::unwrap_used)]

use soundcraft_dsp::{Category, Plugin, PluginEditor, PluginInfo, PluginStateReader};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

pub static INFO: PluginInfo = PluginInfo {
    id: "clap:test.fake",
    name: "Fake",
    short_name: "Fake",
    category: Category::Other,
    params: &[],
    is_instrument: false,
    audiosuite: false,
};

/// What a fake did, and on which thread.
#[derive(Default)]
pub struct Log {
    pub calls: Mutex<Vec<(&'static str, ThreadId)>>,
    pub state: Mutex<Vec<u8>>,
    pub editor_params: Mutex<Vec<(String, f32)>>,
}

impl Log {
    pub fn note(&self, call: &'static str) {
        self.calls.lock().unwrap().push((call, std::thread::current().id()));
    }
    pub fn thread_of(&self, call: &str) -> Option<ThreadId> {
        self.calls.lock().unwrap().iter().find(|(c, _)| *c == call).map(|(_, t)| *t)
    }
}

pub struct Fake {
    pub log: Arc<Log>,
    pub destroy: Duration,
    pub editor_fails: bool,
}

impl Fake {
    /// Sleeps `create` (a heavy plug-in's `initialize`), then returns the instance.
    pub fn make(log: &Arc<Log>, create: Duration, destroy: Duration) -> Box<dyn Plugin> {
        std::thread::sleep(create);
        log.note("create");
        Box::new(Fake { log: Arc::clone(log), destroy, editor_fails: false })
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        std::thread::sleep(self.destroy);
        self.log.note("destroy");
    }
}

impl Plugin for Fake {
    fn info(&self) -> &'static PluginInfo {
        &INFO
    }
    fn prepare(&mut self, _: f32, _: usize, _: usize) {}
    fn reset(&mut self) {}
    fn set_param(&mut self, _: &str, _: f32) -> bool {
        true
    }
    fn param(&self, _: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        for c in io.iter_mut() {
            for x in c.iter_mut().take(frames) {
                *x *= 0.5;
            }
        }
    }
    fn load_state(&mut self, data: &[u8]) -> bool {
        self.log.note("load_state");
        *self.log.state.lock().unwrap() = data.to_vec();
        true
    }
    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        Some(Box::new(FakeEditor { log: Arc::clone(&self.log), open: false, fails: self.editor_fails, sent: false }))
    }
    fn state_reader(&mut self) -> Option<Box<dyn PluginStateReader>> {
        let log = Arc::clone(&self.log);
        Some(Box::new(move || {
            log.note("save_state");
            Some(log.state.lock().unwrap().clone())
        }))
    }
}

pub struct FakeEditor {
    log: Arc<Log>,
    open: bool,
    fails: bool,
    sent: bool,
}

impl PluginEditor for FakeEditor {
    fn open(&mut self) -> Result<(), String> {
        self.log.note("editor_open");
        if self.fails {
            return Err("no window today".into());
        }
        self.open = true;
        Ok(())
    }
    fn close(&mut self) {
        self.log.note("editor_close");
        self.open = false;
    }
    fn is_open(&self) -> bool {
        self.open
    }
    /// One edit, the first time it is idled while open.
    fn idle(&mut self) -> Vec<(String, f32)> {
        if self.open && !self.sent {
            self.log.note("editor_idle");
            self.sent = true;
            return vec![("gain".into(), 0.25)];
        }
        Vec::new()
    }
    fn set_param(&mut self, id: &str, value: f32) {
        self.log.note("editor_set_param");
        self.log.editor_params.lock().unwrap().push((id.to_owned(), value));
    }
}
