//! Asynchronous audio import (plan 06, L30): the slow part of an import (read, decode, resample,
//! peaks) runs on worker threads the engine owns; the quick part (a new source and a clip, one
//! undo step) runs where commands run.
//!
//! `file.prepare_import` queues a file and returns an id. Up to [`MAX_PREPARES`] files are
//! prepared at once; the rest wait in order. `file.import_progress` reports a job's state and
//! percentage, `file.import_cancel` drops it, and `file.commit_import` places a finished one.
//! None of them but the commit touches the document.
//!
//! Thread rules: a worker never touches the engine or the document, only the staging map behind
//! one mutex, which the audio thread never sees. The map is emptied when the session is replaced
//! (new, open, close, revert), so a commit can never place audio prepared for another session.

use crate::io::PreparedAudio;
use crate::{EngineError, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// How many files are prepared at once; more wait in a queue.
pub const MAX_PREPARES: usize = 4;

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
        }
    }
}

struct Job {
    path: String,
    name: String,
    rate: u32,
    roots: Option<Arc<crate::file_access::AutomationRoots>>,
    cancel: Arc<AtomicBool>,
    state: JobState,
    /// 0 to 1.
    progress: f32,
    error: Option<String>,
    prepared: Option<PreparedAudio>,
}

#[derive(Default)]
struct Staging {
    jobs: BTreeMap<u64, Job>,
    queue: VecDeque<u64>,
    /// Worker threads alive.
    running: usize,
}

/// The engine's staging map of imports being prepared or ready to commit.
pub struct Imports {
    shared: Arc<Mutex<Staging>>,
    next: u64,
}

impl Default for Imports {
    fn default() -> Self {
        Imports { shared: Arc::new(Mutex::new(Staging::default())), next: 1 }
    }
}

impl Drop for Imports {
    fn drop(&mut self) {
        self.clear();
    }
}

fn lock(m: &Mutex<Staging>) -> MutexGuard<'_, Staging> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn gone(id: u64) -> EngineError {
    EngineError::BadParams("file.commit_import".into(), format!("no import {id}: it was canceled or committed, or the session changed"))
}

impl Imports {
    /// Queue `path` to be prepared at `rate`; returns its id. The automation roots in force on
    /// this thread apply on the worker too.
    pub fn prepare(&mut self, path: &str, rate: u32) -> u64 {
        let id = self.next;
        self.next += 1;
        let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("Audio").to_string();
        let job = Job {
            path: path.to_string(),
            name,
            rate,
            roots: crate::file_access::current(),
            cancel: Arc::new(AtomicBool::new(false)),
            state: JobState::Queued,
            progress: 0.0,
            error: None,
            prepared: None,
        };
        let spawn = {
            let mut g = lock(&self.shared);
            g.jobs.insert(id, job);
            g.queue.push_back(id);
            let spawn = g.running < MAX_PREPARES;
            if spawn {
                g.running += 1;
            }
            spawn
        };
        if spawn {
            let shared = Arc::clone(&self.shared);
            if cfg!(target_arch = "wasm32") {
                work(&shared);
            } else if let Err(err) = std::thread::Builder::new().name("soundcraft-import".into()).spawn(move || work(&shared)) {
                log::warn!("import worker not started ({err}); preparing on this thread");
                work(&self.shared);
            }
        }
        id
    }

    /// A job's state as JSON: `{id, name, state, progress (0-100), error?, frames?, channels?,
    /// duration?}`; `None` when there is no such job.
    pub fn progress(&self, id: u64) -> Option<Value> {
        let g = lock(&self.shared);
        g.jobs.get(&id).map(|j| describe(id, j))
    }

    /// Every job, oldest first.
    pub fn all(&self) -> Vec<Value> {
        let g = lock(&self.shared);
        g.jobs.iter().map(|(id, j)| describe(*id, j)).collect()
    }

    /// Drop a job, stopping its work at the next step. `false` when there was no such job.
    pub fn cancel(&mut self, id: u64) -> bool {
        let mut g = lock(&self.shared);
        g.queue.retain(|q| *q != id);
        match g.jobs.remove(&id) {
            Some(j) => {
                j.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Drop every job (the session was replaced).
    pub fn clear(&mut self) {
        let mut g = lock(&self.shared);
        g.queue.clear();
        for j in g.jobs.values() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        g.jobs.clear();
    }

    /// Check a finished job is there, without taking it.
    pub fn ready(&self, id: u64) -> Result<()> {
        let g = lock(&self.shared);
        let j = g.jobs.get(&id).ok_or_else(|| gone(id))?;
        match j.state {
            JobState::Done => Ok(()),
            JobState::Failed => Err(EngineError::Io(j.error.clone().unwrap_or_else(|| format!("{}: failed", j.name)))),
            s => Err(EngineError::BadParams("file.commit_import".into(), format!("import {id} ({}) is still {}", j.name, s.as_str()))),
        }
    }

    /// Take a finished job's audio out of the map.
    pub fn take(&mut self, id: u64) -> Result<PreparedAudio> {
        self.ready(id)?;
        let mut g = lock(&self.shared);
        g.jobs.remove(&id).and_then(|j| j.prepared).ok_or_else(|| gone(id))
    }

    /// Jobs in the map (queued, running, finished or failed).
    pub fn len(&self) -> usize {
        lock(&self.shared).jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn describe(id: u64, j: &Job) -> Value {
    let mut v = json!({
        "id": id,
        "name": j.name,
        "path": j.path,
        "state": j.state.as_str(),
        "progress": (j.progress.clamp(0.0, 1.0) * 100.0).floor() as u32,
    });
    if let Some(e) = &j.error {
        v["error"] = json!(e);
    }
    if let Some(p) = &j.prepared {
        let rate = p.rate().max(1);
        v["frames"] = json!(p.frames());
        v["channels"] = json!(p.channels());
        v["duration"] = json!(p.frames() as f64 / f64::from(rate));
    }
    v
}

/// A worker: prepare queued jobs one after another until the queue is empty.
fn work(shared: &Arc<Mutex<Staging>>) {
    loop {
        let next = {
            let mut g = lock(shared);
            loop {
                let Some(id) = g.queue.pop_front() else {
                    g.running -= 1;
                    return;
                };
                if let Some(j) = g.jobs.get_mut(&id) {
                    j.state = JobState::Running;
                    break (id, j.path.clone(), j.name.clone(), j.rate, j.roots.clone(), Arc::clone(&j.cancel));
                }
            }
        };
        let (id, path, name, rate, roots, cancel) = next;
        let set = |p: f32| {
            if let Some(j) = lock(shared).jobs.get_mut(&id) {
                j.progress = p;
            }
            !cancel.load(Ordering::Relaxed)
        };
        let run = || {
            let read = |path: &str| crate::io::read_file_with_progress(path, &mut |done, total| set(0.3 * done as f32 / total.max(1) as f32));
            let bytes = match &roots {
                Some(r) => crate::file_access::confine(Some(r), || read(&path)),
                None => crate::file_access::unconfined(|| read(&path)),
            }?;
            crate::io::prepare_audio_bytes(&name, &bytes, Some(&path), rate, &mut |f| set(0.3 + 0.7 * f))
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        let mut g = lock(shared);
        // A job canceled meanwhile is gone from the map: its result is dropped here.
        if let Some(j) = g.jobs.get_mut(&id) {
            match result {
                Ok(Ok(p)) => {
                    j.state = JobState::Done;
                    j.progress = 1.0;
                    j.prepared = Some(p);
                }
                Ok(Err(e)) => {
                    j.state = JobState::Failed;
                    j.error = Some(e.to_string());
                }
                Err(_) => {
                    j.state = JobState::Failed;
                    j.error = Some(format!("{name}: internal error while decoding"));
                }
            }
        }
    }
}
