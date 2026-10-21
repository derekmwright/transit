//! Realtime playback through the system audio device (cpal: CoreAudio, WASAPI, ALSA/Pulse/
//! PipeWire via ALSA, JACK, WebAudio).
//!
//! [`Player`] owns prepared plans, bounded action ingress and Main-thread plug-in
//! handles. The callback borrows its anchored audio state and never prepares or
//! reclaims a session. Requests return admission identities; [`Player::poll_notices`]
//! and recovery frontiers report application or refusal. Position follows consumed
//! output frames. Native clock fitting runs independently of the UI.
//!
//! Call [`Player::idle`] on the creating thread for plug-in service and reclamation,
//! and drain notices regularly to return request credits. The explicit silent
//! fallback preserves elapsed frame debt without claiming a hardware clock.
//!
//! Main mix channels follow SMPTE/WAV order; fewer device channels fold to stereo
//! using ITU-R BS.775. Plug-ins and editor/state handles remain Main-owned through
//! callback quiescence. [`LegacyPlayer`] remains solely for transition regressions.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod action_lane;
pub mod device_clock;
pub mod devices;
pub mod plan_bridge;
pub mod player;
pub mod publication;
pub mod record;
pub mod scheduling;
pub mod telemetry;
pub mod timeline_map;

pub use devices::AudioConfig;
pub use player::{ActionResult, Player, PlayerCommand, PlayerError, PlayerNotice};

use soundcraft_dsp::{Plugin, PluginEditor, PluginStateReader};
use soundcraft_mix::{MAX_CHANNELS, MixEngine, PreparedInstance, StripMeter, itu_stereo_fold, main_channels};
use soundcraft_model::{MAX_DEVICE_CHANNELS, Session, TrackId, TrackKind};
use soundcraft_time::{Range, Samples};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

const BLOCK: usize = 512;

/// Room for replaced snapshots on their way back; `update_session` empties it before each send.
const OLD_SESSIONS: usize = 16;

/// The realtime audio thread marker lives in the mix engine, which carries it into the strips it
/// renders on worker threads; the cpal callbacks here set it.
pub use soundcraft_mix::{mark_audio_thread, on_audio_thread};

/// Meter values for the UI.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
}

/// Plugin states read from the live instances: `(track, slot or INSTRUMENT_SLOT, state)`.
pub type PluginStates = Vec<(TrackId, usize, Vec<u8>)>;

/// A parameter edit made in a plugin's own editor: `(track, slot, param id, value)`.
pub type EditorEdit = (TrackId, usize, String, f32);

enum Cmd {
    /// A new snapshot plus the third-party instances it needs (created on the UI thread).
    Session(Arc<Session>, Vec<PreparedInstance>),
    /// Where the callback sends the snapshots it replaces (freeing one takes thousands of frees).
    OldSessions(SyncSender<Arc<Session>>),
    Input(Arc<record::InputRing>),
    Recording(bool),
    Play {
        from: Samples,
        end: Option<Samples>,
        looped: Option<Range>,
    },
    Stop,
    Locate(Samples),
    /// Audition a note on a track's instrument (velocity 0 releases it).
    Note {
        track: TrackId,
        pitch: u8,
        velocity: u8,
    },
    /// Release every held note on every instrument, without stopping the transport.
    AllNotesOff,
}

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    /// Speed × 1000.
    speed: AtomicU32,
    meters: Mutex<MeterSnapshot>,
    /// A hosted plug-in's editor is open (set by the UI thread each frame): the engine keeps
    /// running while stopped, because such GUIs initialise and sync through `process` calls.
    editors_open: AtomicBool,
}

/// What stopped the audio output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTrouble {
    /// A block panicked; the output stays silent.
    Bug,
    /// The device went away.
    Gone,
    /// The device stopped asking for audio while playing.
    Stalled,
}

/// How the audio output is doing, for the UI.
#[derive(Default)]
struct Fault {
    telemetry: telemetry::Counters,
    /// A block panicked: output stays silent from then on.
    failed: AtomicBool,
    gone: AtomicBool,
    /// Blocks so far, to tell a device that stopped asking for audio.
    beats: std::sync::atomic::AtomicU64,
    /// The UI has been told.
    reported: AtomicBool,
}

impl Fault {
    /// What stopped the output, once.
    fn trouble(&self, stalled: bool) -> Option<OutputTrouble> {
        let trouble = if self.failed.load(Ordering::Relaxed) {
            OutputTrouble::Bug
        } else if self.gone.load(Ordering::Relaxed) {
            OutputTrouble::Gone
        } else if stalled {
            OutputTrouble::Stalled
        } else {
            return None;
        };
        (!self.reported.swap(true, Ordering::Relaxed)).then_some(trouble)
    }
}

/// Whether the player opens the input together with the output, before either runs: on ASIO
/// when the device has inputs. An ASIO driver has one set of buffers for both directions, and
/// cpal 0.16 recreates them (stopping the driver) whenever a direction is added; doing that while
/// the output's callbacks run races cpal's own stream lock and can hang. Opened first, the input
/// is in place when the output's build creates both directions' buffers at once, and arming
/// never touches the streams. The input is the output's own driver (one ASIO driver per process),
/// whatever input the configuration names. Other audio systems open the input on demand, as its
/// own device. `has_input` is asked only on ASIO.
fn input_with_output(system: &str, has_input: impl FnOnce() -> bool) -> bool {
    system == devices::ASIO_SYSTEM && has_input()
}

/// The input a player opened with its output ([`input_with_output`]).
enum HeldInput {
    /// None: the input opens on demand ([`record::Recorder::open_with`]).
    No,
    /// The input, while the player holds it (`None` while the UI has it).
    Held(Option<record::Recorder>),
    /// It could not open with the output (or the ASIO device has no inputs), and must not open
    /// later while the output runs.
    Failed(String),
}

/// How long a playing output may go without a block before it counts as stalled.
#[cfg(not(target_arch = "wasm32"))]
const STALL: std::time::Duration = std::time::Duration::from_secs(2);

/// True once the block count `beats` has stood still for `STALL` while playing; `seen` is the
/// count last seen and since when.
#[cfg(not(target_arch = "wasm32"))]
fn stalled(seen: &mut Option<(u64, std::time::Instant)>, beats: u64, playing: bool) -> bool {
    match *seen {
        Some((b, since)) if playing && b == beats => since.elapsed() >= STALL,
        _ => {
            *seen = Some((beats, std::time::Instant::now()));
            false
        }
    }
}

/// Fill one block with `fill`. If it panics, the block and every later one are silent and the
/// transport stops, rather than the panic taking the app down. The caller must retain
/// an error payload for owner-thread destruction, never discard it on the callback.
fn guarded_block(fault: &Fault, shared: &Shared, out: &mut [f32], fill: impl FnOnce(&mut [f32])) -> std::thread::Result<()> {
    fault.beats.fetch_add(1, Ordering::Relaxed);
    let failed = fault.failed.load(Ordering::Relaxed);
    let result = if failed { Ok(()) } else { std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fill(out))) };
    if failed || result.is_err() {
        fault.failed.store(true, Ordering::Relaxed);
        shared.playing.store(false, Ordering::Relaxed);
        out.fill(0.0);
    }
    result
}

/// Where the audio state lives: the callback (or the silent clock) renders from it, and the
/// player takes it out at drop so every plug-in instance is torn down on the player's thread,
/// whether or not the callback still runs.
type Holder = Arc<Mutex<Option<AudioState>>>;

/// One block from the state in `holder`, or silence when the player holds the lock (taking the
/// state at drop) or has taken the state. Never a blocking lock: this runs on the audio thread.
/// Returns false once the state is gone.
fn render_held(holder: &Mutex<Option<AudioState>>, fault: &Fault, shared: &Shared, out: &mut [f32], channels: usize) -> bool {
    let mut held = match holder.try_lock() {
        Ok(g) => g,
        // A panic escaped while the lock was held (`guarded_block` catches the block's own):
        // the state is still whole enough to render or go silent.
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => {
            fault.telemetry.contention_silence_frames.fetch_add((out.len() / channels.max(1)) as u64, Ordering::Relaxed);
            out.fill(0.0);
            return true;
        }
    };
    match held.as_mut() {
        Some(st) => {
            if let Err(payload) = guarded_block(fault, shared, out, |o| st.fill(o, channels)) {
                // The fault latch prevents another fill, so this slot is written
                // once. Retain arbitrary user destruction until control takes
                // this main-anchored state after callback quiescence.
                st.panic_payload = Some(payload);
            }
            if fault.failed.load(Ordering::Relaxed) {
                fault.telemetry.fault_silence_frames.fetch_add((out.len() / channels.max(1)) as u64, Ordering::Relaxed);
            }
            true
        }
        None => {
            fault.telemetry.unavailable_silence_frames.fetch_add((out.len() / channels.max(1)) as u64, Ordering::Relaxed);
            out.fill(0.0);
            false
        }
    }
}

/// State the audio callback renders from (inside a [`Holder`]).
struct AudioState {
    /// A caught unwind is owned here until LegacyPlayer teardown on its owner thread.
    panic_payload: Option<Box<dyn std::any::Any + Send>>,
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    session: Arc<Session>,
    mix: MixEngine,
    pos: Samples,
    playing: bool,
    end: Option<Samples>,
    looped: Option<Range>,
    /// Rendered frames waiting to be consumed (planar, main channels) and the fractional read index.
    pending: Vec<Vec<f32>>,
    /// Stereo fold-down gains per main channel (used when the device has fewer channels).
    fold: [(f32, f32); MAX_CHANNELS],
    pending_len: usize,
    read: f64,
    device_rate: f64,
    input: Option<Arc<record::InputRing>>,
    recording: bool,
    /// Auditioned notes held, and the frames left to render the last one's release.
    live_notes: usize,
    live_tail: usize,
    /// The session holds a hosted plug-in: the engine runs while stopped so the mixer can suspend plug-ins by their own signals rather than starving them.
    hosted: bool,
    /// Blocks rendered (tests observe whether the engine ran even when nothing is audible).
    #[cfg(test)]
    renders: usize,
    /// Retired plugin instances go back to the UI thread to be dropped there.
    garbage: Sender<Vec<Box<dyn Plugin>>>,
    old_sessions: Option<SyncSender<Arc<Session>>>,
    /// A stop, the end of playback or All Notes Off queued releases on the instruments; the next
    /// `fill` gives them one processing pass (even when nothing else would run) so they are delivered.
    release_flush_pending: bool,
}

impl Drop for AudioState {
    fn drop(&mut self) {
        // The holder is anchored on control until callback quiescence. Isolate
        // arbitrary payload cleanup before any plug-in fields are destroyed,
        // including when LegacyPlayer itself is being dropped during an unwind.
        if let Some(payload) = self.panic_payload.take()
            && let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            // The secondary payload can itself have a panicking destructor.
            // Retain it rather than recurse through unbounded foreign cleanup.
            std::mem::forget(secondary);
        }
    }
}

fn new_engine(sr: f32) -> MixEngine {
    let mut m = MixEngine::new(sr, BLOCK);
    m.set_external_instances(true);
    m.metronome = true;
    m
}

/// Whether any track of `s` carries a hosted (third-party) plug-in. Allocation-free: runs on the audio thread.
fn session_has_hosted(s: &Session) -> bool {
    s.tracks.iter().any(|t| {
        t.instrument.as_ref().is_some_and(|i| soundcraft_mix::is_third_party(&i.plugin))
            || t.mixer.inserts.iter().flatten().any(|i| soundcraft_mix::is_third_party(&i.plugin))
    })
}

impl AudioState {
    fn new(rx: Receiver<Cmd>, shared: Arc<Shared>, session: Arc<Session>, device_rate: f64, garbage: Sender<Vec<Box<dyn Plugin>>>) -> Self {
        let sr = session.sample_rate.as_f64() as f32;
        let nch = main_channels(&session);
        AudioState {
            panic_payload: None,
            hosted: session_has_hosted(&session),
            #[cfg(test)]
            renders: 0,
            garbage,
            old_sessions: None,
            release_flush_pending: false,
            fold: fold_gains(&session),
            rx,
            shared,
            mix: new_engine(sr),
            session,
            pos: 0,
            playing: false,
            end: None,
            looped: None,
            pending: vec![vec![0.0; BLOCK]; nch],
            pending_len: 0,
            read: 0.0,
            device_rate,
            input: None,
            recording: false,
            live_notes: 0,
            live_tail: 0,
        }
    }

    /// Control-thread fault housekeeping. A terminal callback leaves the queue
    /// untouched: even receiving or forwarding an empty command can allocate or
    /// free a channel block, and a queued snapshot can be its final owner.
    /// Bound each UI service pass; remaining commands stay anchored until the
    /// next pass or owner-thread teardown.
    fn discard_after_fault(&mut self) {
        for _ in 0..64 {
            let Ok(command) = self.rx.try_recv() else { break };
            drop(command);
        }
    }

    fn drain(&mut self) {
        let mut changed = false;
        while let Ok(c) = self.rx.try_recv() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                Cmd::Recording(r) => self.recording = r,
                Cmd::Session(s, instances) => {
                    changed = true;
                    if (s.sample_rate.as_f64() - self.session.sample_rate.as_f64()).abs() > 0.5 {
                        let mut old = std::mem::replace(&mut self.mix, new_engine(s.sample_rate.as_f64() as f32));
                        let _ = self.garbage.send(old.drain_plugins());
                    }
                    self.mix.adopt(instances);
                    // The main format changed (a document edit, not steady state): resize.
                    let nch = main_channels(&s);
                    if self.pending.len() != nch {
                        self.pending = vec![vec![0.0; BLOCK]; nch];
                        self.pending_len = 0;
                        self.read = 0.0;
                    }
                    self.fold = fold_gains(&s);
                    self.hosted = session_has_hosted(&s);
                    let old = std::mem::replace(&mut self.session, s);
                    if let Some(tx) = &self.old_sessions {
                        let _ = tx.try_send(old);
                    }
                }
                Cmd::OldSessions(tx) => self.old_sessions = Some(tx),
                Cmd::Play { from, end, looped } => {
                    self.pos = from;
                    self.end = end;
                    self.looped = looped.filter(|r| r.len() > 64);
                    self.playing = true;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
                Cmd::Stop => {
                    self.playing = false;
                    self.end = None;
                    self.looped = None;
                    self.mix.reset();
                    self.release_flush_pending = true;
                }
                Cmd::Note { track, pitch, velocity } => {
                    self.mix.live_note(track, pitch, velocity);
                    if velocity > 0 {
                        self.live_notes = self.live_notes.saturating_add(1);
                    } else {
                        self.live_notes = self.live_notes.saturating_sub(1);
                        // Long enough for a synth's release to ring out.
                        self.live_tail = (self.device_rate * 2.0) as usize;
                    }
                }
                Cmd::Locate(at) => {
                    self.pos = at;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
                Cmd::AllNotesOff => {
                    self.mix.all_notes_off();
                    self.release_flush_pending = true;
                }
            }
        }
        if changed {
            // Place adopted instances now (not only once playback starts), so editors see them;
            // `sync` creates no third-party plugins here.
            self.mix.ensure_synced(&self.session);
        }
        self.send_garbage();
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    fn send_garbage(&mut self) {
        let g = self.mix.take_retired();
        if g.capacity() > 0 {
            let _ = self.garbage.send(g);
        }
    }

    /// Render the next block at the session rate into `pending`.
    fn render_next(&mut self, quantum: usize) {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let mut n = quantum.clamp(1, BLOCK);
        if let Some(l) = self.looped
            && self.pos >= l.end
        {
            self.pos = l.start;
        }
        if let Some(l) = self.looped {
            n = n.min(usize::try_from((l.end - self.pos).max(1)).unwrap_or(BLOCK));
        } else if let Some(e) = self.end {
            if self.pos >= e {
                // Reaching the end is a stop: release held notes (a note ending on the end sample is never otherwise released).
                self.mix.reset();
                self.release_flush_pending = true;
                self.playing = false;
                self.end = None;
                self.shared.playing.store(false, Ordering::Relaxed);
                n = 0;
            } else {
                n = n.min(usize::try_from(e - self.pos).unwrap_or(BLOCK));
            }
        }
        if n == 0 {
            for c in &mut self.pending {
                c.iter_mut().for_each(|x| *x = 0.0);
            }
            self.pending_len = BLOCK;
            return;
        }
        if let Some(ring) = &self.input {
            let ch = ring.channels.load(Ordering::Relaxed).max(1);
            if self.mix.input.len() != ch {
                self.mix.input = vec![vec![0.0; BLOCK]; ch];
            }
            ring.pop_into(&mut self.mix.input, n, self.session.sample_rate.hz());
        }
        self.mix.monitor_only = !self.playing;
        // An open hosted editor needs its plug-in processed (GUIs sync through `process`): no suspension while one is open.
        self.mix.no_suspend = self.shared.editors_open.load(Ordering::Relaxed);
        self.mix.recording = self.recording;
        self.mix.render(&self.session, self.pos, n, &mut self.pending);
        self.send_garbage();
        self.pending_len = n;
        if let Ok(mut m) = self.shared.meters.try_lock() {
            m.main.copy_from(&self.mix.main_meter);
            for (k, v) in &self.mix.meters {
                match m.tracks.get_mut(k) {
                    Some(d) => d.copy_from(v),
                    None => {
                        m.tracks.insert(*k, v.clone());
                    }
                }
            }
        }
        if self.playing {
            self.pos += n as i64;
            self.shared.position.store(self.pos, Ordering::Relaxed);
        }
    }

    /// Give the instruments their one processing pass if releases are queued (see
    /// `release_flush_pending`), then clear the flag.
    fn flush_releases_if_pending(&mut self) {
        if std::mem::take(&mut self.release_flush_pending) {
            self.mix.flush_releases(BLOCK);
        }
    }

    /// Fill an interleaved device buffer.
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        self.drain();
        self.flush_releases_if_pending();
        let channels = channels.max(1);
        // An armed or input-monitoring track keeps the engine running while stopped. An audio
        // track needs the input stream for that; an instrument track runs without one, so it
        // can be played live from its own keyboard or a MIDI device.
        // A hosted plug-in with its editor open keeps the engine running too: Guitar Rig 7 and
        // Helix Native initialise their GUIs and sync them with their DSP through `process`
        // calls, and sit frozen on an idle track until the transport runs.
        // Any hosted plug-in keeps the engine running too: it expects `process` calls, and the mixer
        // suspends plug-ins individually by their own signals, so the global gate must not starve them.
        let monitoring = self.live_notes > 0
            || self.hosted
            || self.live_tail > 0
            || self.shared.editors_open.load(Ordering::Relaxed)
            || self.session.tracks.iter().any(|t| {
                (t.mixer.input_monitor || t.mixer.record_arm) && (self.input.is_some() || matches!(t.kind, TrackKind::Instrument | TrackKind::Midi))
            });
        if !self.playing && !monitoring {
            out.iter_mut().for_each(|x| *x = 0.0);
            if let Ok(mut m) = self.shared.meters.try_lock() {
                m.main = StripMeter::default();
                m.tracks.clear();
            }
            return;
        }
        let speed = f64::from(self.shared.speed.load(Ordering::Relaxed)) / 1000.0;
        let step = self.session.sample_rate.as_f64() / self.device_rate.max(1.0) * speed.clamp(0.1, 4.0);
        let callback_frames = out.len() / channels;
        if self.live_notes == 0 {
            self.live_tail = self.live_tail.saturating_sub(callback_frames);
        }
        let quantum = ((callback_frames as f64 * step).ceil() as usize).clamp(1, BLOCK);
        for frame in out.chunks_mut(channels) {
            while self.read >= self.pending_len as f64 {
                self.read -= self.pending_len as f64;
                if !self.playing && !monitoring {
                    break;
                }
                self.render_next(quantum);
                if self.pending_len == 0 {
                    break;
                }
            }
            let i0 = self.read.floor() as usize;
            let fr = (self.read - i0 as f64) as f32;
            let sounding = self.playing || monitoring;
            let nch = self.pending.len();
            let sample = |c: usize| {
                let ch = self.pending.get(c);
                let a = ch.and_then(|v| v.get(i0)).copied().unwrap_or(0.0);
                let b = ch.and_then(|v| v.get(i0 + 1)).copied().unwrap_or(a);
                a + (b - a) * fr
            };
            if nch > 2 && channels < nch {
                // Fewer device channels than the main mix: ITU fold-down to stereo.
                let (mut l, mut r) = (0.0f32, 0.0f32);
                if sounding {
                    for (c, (gl, gr)) in self.fold.iter().enumerate().take(nch) {
                        let v = sample(c);
                        l += v * gl;
                        r += v * gr;
                    }
                }
                for (c, o) in frame.iter_mut().enumerate() {
                    *o = match c {
                        0 => l.clamp(-1.0, 1.0),
                        1 => r.clamp(-1.0, 1.0),
                        _ => 0.0,
                    };
                }
            } else {
                for (c, o) in frame.iter_mut().enumerate() {
                    // Stereo mixes on a mono device play the left channel (as before).
                    *o = if c < nch.max(1) && sounding { sample(c).clamp(-1.0, 1.0) } else { 0.0 };
                }
            }
            self.read += step;
        }
        // The end of playback is detected while rendering; deliver its releases in this callback.
        self.flush_releases_if_pending();
    }
}

/// UI-thread bookkeeping of the third-party instances the audio thread holds.
struct Side {
    /// What was last created for each slot: (plugin id, channels, created ok).
    shadow: HashMap<(TrackId, usize), (String, usize, bool)>,
    /// Session sample rate the instances were prepared for.
    rate: f32,
    /// Editor handles of the live instances.
    editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    /// State readers of the live instances.
    states: BTreeMap<(TrackId, usize), Box<dyn PluginStateReader>>,
    /// Instances that refused their stored state: saves keep that state until the plugin's editor
    /// is opened.
    refused: HashSet<(TrackId, usize)>,
    garbage: Receiver<Vec<Box<dyn Plugin>>>,
    old_sessions: Option<Receiver<Arc<Session>>>,
}

impl Side {
    fn new(rate: f32, garbage: Receiver<Vec<Box<dyn Plugin>>>) -> Side {
        Side { shadow: HashMap::new(), rate, editors: HashMap::new(), states: BTreeMap::new(), refused: HashSet::new(), garbage, old_sessions: None }
    }

    /// Drops retired instances (closing their editors first, inside the plugin's drop) and
    /// replaced session snapshots.
    fn collect_garbage(&mut self) {
        while let Ok(g) = self.garbage.try_recv() {
            for p in g {
                log::debug!("plugin instance released: {}", p.info().name);
                drop(p);
            }
        }
        if let Some(rx) = &self.old_sessions {
            rx.try_iter().for_each(drop);
        }
    }

    fn return_old_sessions(&mut self, tx: &Sender<Cmd>) {
        let (otx, orx) = sync_channel(OLD_SESSIONS);
        self.old_sessions = Some(orx);
        let _ = tx.send(Cmd::OldSessions(otx));
    }

    fn forget(&mut self, key: (TrackId, usize)) {
        self.shadow.remove(&key);
        self.states.remove(&key);
        self.refused.remove(&key);
        if let Some(mut e) = self.editors.remove(&key) {
            e.close();
        }
    }
}

/// The transport and audio device.
#[doc(hidden)]
pub struct LegacyPlayer {
    shared: Arc<Shared>,
    fault: Arc<Fault>,
    /// The block count last seen while playing, and since when.
    #[cfg(not(target_arch = "wasm32"))]
    watch: Mutex<Option<(u64, std::time::Instant)>>,
    tx: Sender<Cmd>,
    side: Mutex<Side>,
    /// The audio state the callback renders from; taken back at drop.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    state: Holder,
    #[cfg(not(target_arch = "wasm32"))]
    _stream: Option<cpal::Stream>,
    #[cfg(target_arch = "wasm32")]
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub device_rate: u32,
    /// Output channels the device was opened with (0 for the silent clock).
    pub device_channels: u16,
    /// The device buffer in frames when one was chosen (`None`: the device default).
    pub device_buffer: Option<u32>,
    /// True when no audio device could be opened (silent clock).
    pub silent: bool,
    /// The input opened with the output, on ASIO ([`LegacyPlayer::open_input`]).
    input: Mutex<HeldInput>,
    /// Native fitting is independent of the UI; control owns service cancellation.
    clock: Mutex<Option<device_clock::MaintainedClock>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for LegacyPlayer {
    /// Takes the audio state (and so every plugin instance) back from the callback and drops it
    /// here, then closes the stream, then collects what was retired before.
    fn drop(&mut self) {
        // Closing the stream joins the audio thread on WASAPI and ALSA, which destroys the callback
        // there (on ASIO cpal drops the closure on the caller's thread inside `remove_callback`);
        // plugin instances it owned would be dropped on the audio thread. A VST3's `terminate` may wait for
        // the UI thread (Arturia's Jup-8000 V does), which is the thread waiting in the join: the
        // app never exits. So the state is taken out of the holder here, without the callback's
        // help: a device that has stopped calling (a device error, an unplugged interface, a
        // stalled driver) must not leave the instances behind. The stream then holds only an
        // empty holder.
        if let Some(stream) = &self._stream {
            use cpal::traits::StreamTrait;
            if let Err(e) = stream.pause() {
                log::debug!("audio stream did not pause at close: {e}");
            }
        }
        // No settle sleep after `pause`: the callback only `try_lock`s, so once this holds the lock
        // a late callback renders silence, and a block already under way is waited for here.
        // Blocking is fine here: the callback holds the lock for one block at most. If a plug-in's
        // `process` never returns (it waits on the UI thread), this lock waits with it; that hang
        // is the same class as before this change, not a new one.
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner).take();
        if state.is_some() {
            log::debug!("audio state released at close");
        }
        drop(state);
        // Before the fields drop (`side`, with the retire channel's receiver, is declared first).
        drop(self._stream.take());
        self.side().collect_garbage();
    }
}

impl LegacyPlayer {
    /// Open the default output device. Never fails: without a device it runs a silent clock.
    pub fn new(session: Arc<Session>) -> LegacyPlayer {
        LegacyPlayer::with_config(session, &AudioConfig::default())
    }

    /// Open the output device `cfg` chooses (system, device, rate, buffer size), falling back to the
    /// defaults for whatever is unavailable. Never fails: without a device it runs a silent clock.
    pub fn with_config(session: Arc<Session>, cfg: &AudioConfig) -> LegacyPlayer {
        let p = LegacyPlayer::open(Arc::clone(&session), cfg);
        p.side().return_old_sessions(&p.tx);
        // Hand the initial session's third-party plugins over.
        p.update_session(session);
        p
    }

    /// The audio input, for recording and input monitoring. On ASIO it is the one opened with the
    /// output ([`input_with_output`]), or the reason it could not open; it never opens while the
    /// output runs. Elsewhere it opens the input `audio` chooses as its own device.
    pub fn open_input(&self, audio: &AudioConfig) -> Result<record::Recorder, String> {
        match &mut *self.input.lock().unwrap_or_else(PoisonError::into_inner) {
            HeldInput::No => record::Recorder::open_with(audio),
            HeldInput::Held(r) => {
                let r = r.take().ok_or_else(|| "the audio input is already open".to_string())?;
                // It has run since startup with nothing reading the monitor: start from now.
                r.monitor.clear_monitor();
                Ok(r)
            }
            HeldInput::Failed(e) => Err(e.clone()),
        }
    }

    /// Done with the input from [`LegacyPlayer::open_input`]: its takes are finished. One opened with
    /// the output stays open (closing it while the output runs is what must not happen) for the
    /// next `open_input`; any other closes.
    pub fn close_input(&self, mut r: record::Recorder) {
        let mut held = self.input.lock().unwrap_or_else(PoisonError::into_inner);
        if let HeldInput::Held(slot @ None) = &mut *held {
            r.finish();
            *slot = Some(r);
        }
    }

    /// What stopped the audio output, once: a bug in the engine, or the device went away or
    /// stopped asking for audio while playing.
    pub fn trouble(&self) -> Option<OutputTrouble> {
        #[cfg(not(target_arch = "wasm32"))]
        let stalled = {
            let playing = self.shared.playing.load(Ordering::Relaxed) && !self.silent;
            stalled(&mut self.watch.lock().unwrap_or_else(PoisonError::into_inner), self.fault.beats.load(Ordering::Relaxed), playing)
        };
        #[cfg(target_arch = "wasm32")]
        let stalled = false;
        self.fault.trouble(stalled)
    }

    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open(session: Arc<Session>, cfg: &AudioConfig) -> LegacyPlayer {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let want = session.sample_rate.hz();
        let side_rate = session.sample_rate.as_f64() as f32;
        let new_side = |garbage: Receiver<Vec<Box<dyn Plugin>>>| Mutex::new(Side::new(side_rate, garbage));
        let want_ch = u16::try_from(main_channels(&session)).unwrap_or(2);
        match open_device(want, want_ch, cfg) {
            Ok((device, config, format, name, system)) => {
                let duplex = input_with_output(system, || {
                    use cpal::traits::DeviceTrait;
                    device.default_input_config().is_ok_and(|c| record::input_usable(&c))
                });
                if duplex && cfg.input.as_ref().is_some_and(|i| *i != name) {
                    log::warn!("audio input `{}`: on ASIO the input is the output's device, `{name}`", cfg.input.as_deref().unwrap_or_default());
                }
                let mut input_error = "no audio input device".to_string();
                // The chosen buffer size first; if the device refuses it, its default.
                let wide = config.channels;
                let attempts = output_attempts(&config, want_ch);
                for config in attempts {
                    let rate = config.sample_rate.0;
                    let channels = usize::from(config.channels);
                    let (tx, rx) = channel();
                    let (gtx, grx) = channel();
                    let state: Holder =
                        Arc::new(Mutex::new(Some(AudioState::new(rx, Arc::clone(&shared), Arc::clone(&session), f64::from(rate), gtx))));
                    let fault = Arc::new(Fault::default());
                    use cpal::traits::StreamTrait;
                    // On ASIO the input first, on the output's own device and not started, so the
                    // output's build creates both directions' buffers with the driver stopped (cpal
                    // 0.16 `get_or_create_output_stream` takes the input's buffer list along). A
                    // stream that is not playing returns from its callback before cpal's stream
                    // lock, so the input's callback cannot meet that rebuild. Same rate and buffer
                    // as the output: the rebuild sizes both directions at the output's buffer.
                    let input = if duplex {
                        match record::Recorder::build_on(&device, rate, config.buffer_size) {
                            Ok(r) => Some(r),
                            Err(e) => {
                                log::warn!("cannot open the audio input with the output: {e}");
                                input_error = format!("the audio input did not open with the output ({e})");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let built = match format {
                        cpal::SampleFormat::I32 => build_output::<i32>(&device, &config, &state, channels, &fault, &shared),
                        cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, &state, channels, &fault, &shared),
                        _ => build_output::<f32>(&device, &config, &state, channels, &fault, &shared),
                    };
                    match built {
                        Ok((stream, clock)) => {
                            let clock = device_clock::MaintainedClock::start(clock)
                                .map_err(|error| {
                                    log::warn!("output clock maintenance unavailable: {error}");
                                })
                                .ok();
                            let input = match input {
                                Some(r) => match r.start() {
                                    Ok(()) => HeldInput::Held(Some(r)),
                                    Err(e) => {
                                        log::warn!("audio input failed to start: {e}");
                                        HeldInput::Failed(format!("the audio input did not start ({e})"))
                                    }
                                },
                                // Never opened later on ASIO: that would recreate the buffers
                                // under the running output.
                                None if system == devices::ASIO_SYSTEM => HeldInput::Failed(input_error.clone()),
                                None => HeldInput::No,
                            };
                            if let Err(e) = stream.play() {
                                log::warn!("audio stream failed to start: {e}");
                            }
                            let buffer = match config.buffer_size {
                                cpal::BufferSize::Fixed(n) => Some(n),
                                cpal::BufferSize::Default => None,
                            };
                            log::info!(
                                "audio device: {name} @ {rate} Hz, {channels} ch (widest {wide}), {format}, buffer {}",
                                buffer.map_or("default".to_string(), |n| n.to_string())
                            );
                            return LegacyPlayer {
                                shared,
                                fault,
                                #[cfg(not(target_arch = "wasm32"))]
                                watch: Mutex::new(None),
                                tx,
                                side: new_side(grx),
                                state,
                                _stream: Some(stream),
                                device_name: name,
                                device_rate: rate,
                                device_channels: config.channels,
                                device_buffer: buffer,
                                silent: false,
                                input: Mutex::new(input),
                                clock: Mutex::new(clock),
                            };
                        }
                        Err(e) => log::warn!("cannot open audio stream ({:?}): {e}", config.buffer_size),
                    }
                }
                log::warn!("no usable audio stream; using a silent clock");
                let (tx, rx) = channel();
                let (gtx, grx) = channel();
                LegacyPlayer::silent_clock(shared, tx, rx, want, new_side(grx), gtx)
            }
            Err(e) => {
                log::warn!("no audio output: {e}; using a silent clock");
                let (tx, rx) = channel();
                let (gtx, grx) = channel();
                LegacyPlayer::silent_clock_with(shared, tx, rx, Arc::clone(&session), new_side(grx), gtx)
            }
        }
    }

    fn silent_clock(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        rx: Receiver<Cmd>,
        rate: u32,
        side: Mutex<Side>,
        gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> LegacyPlayer {
        let s = Arc::new(Session::new("Untitled", soundcraft_time::SampleRate::new(rate).unwrap_or_default()));
        LegacyPlayer::silent_clock_with(shared, tx, rx, s, side, gtx)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn silent_clock_with(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        rx: Receiver<Cmd>,
        session: Arc<Session>,
        side: Mutex<Side>,
        gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> LegacyPlayer {
        let rate = session.sample_rate.hz();
        let device_rate = f64::from(rate);
        let state: Holder = Arc::new(Mutex::new(Some(AudioState::new(rx, Arc::clone(&shared), session, device_rate, gtx))));
        let fault = Arc::new(Fault::default());
        let (failed, held, clock) = (Arc::clone(&fault), Arc::clone(&state), Arc::clone(&shared));
        let spawn = std::thread::Builder::new().name("soundcraft-clock".into()).spawn(move || {
            let mut buf = vec![0.0f32; 4096];
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let now = std::time::Instant::now();
                let frames = ((now - last).as_secs_f64() * device_rate) as usize;
                last = now;
                let n = (frames * 2).min(buf.len());
                let present = match buf.get_mut(..n) {
                    Some(b) => render_held(&held, &failed, &clock, b, 2),
                    None => true,
                };
                // The player took the state back, or is gone.
                if !present || Arc::strong_count(&held) <= 1 {
                    break;
                }
            }
        });
        if let Err(e) = spawn {
            log::warn!("clock thread failed: {e}");
        }
        LegacyPlayer::silent(shared, fault, tx, side, state, rate)
    }

    /// A silent player around `state`, shared with whatever renders it (the clock thread, or
    /// nothing).
    #[cfg(not(target_arch = "wasm32"))]
    fn silent(shared: Arc<Shared>, fault: Arc<Fault>, tx: Sender<Cmd>, side: Mutex<Side>, state: Holder, rate: u32) -> LegacyPlayer {
        LegacyPlayer {
            shared,
            fault,
            watch: Mutex::new(None),
            tx,
            side,
            state,
            _stream: None,
            device_name: "No audio device (silent)".into(),
            device_rate: rate,
            device_channels: 0,
            device_buffer: None,
            silent: true,
            input: Mutex::new(HeldInput::No),
            clock: Mutex::new(None),
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn silent_clock_with(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        _rx: Receiver<Cmd>,
        session: Arc<Session>,
        side: Mutex<Side>,
        _gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> LegacyPlayer {
        LegacyPlayer {
            shared,
            fault: Arc::default(),
            tx,
            side,
            state: Arc::default(),
            _stream: None,
            device_name: "No audio device".into(),
            device_rate: session.sample_rate.hz(),
            device_channels: 0,
            device_buffer: None,
            silent: true,
            input: Mutex::new(HeldInput::No),
            clock: Mutex::new(None),
        }
    }

    /// Sends a new session snapshot to the audio thread, with freshly created (state-restored,
    /// prepared) instances of any third-party plugin it newly needs. Call on the UI thread.
    pub fn update_session(&self, s: Arc<Session>) {
        let instances = self.side().prepare(&s);
        let _ = self.tx.send(Cmd::Session(s, instances));
    }

    /// Reads the state of every live third-party plugin, for saving the session.
    pub fn capture_states(&self) -> PluginStates {
        self.side().capture_states()
    }

    /// Per-UI-frame housekeeping: drops retired instances and services plugin editors. Returns
    /// the parameter edits made in open editors since the last call.
    pub fn idle(&self) -> Vec<EditorEdit> {
        if self.fault.failed.load(Ordering::Relaxed)
            && let Ok(mut held) = self.state.try_lock()
            && let Some(state) = held.as_mut()
        {
            state.discard_after_fault();
        }
        let mut side = self.side();
        side.collect_garbage();
        let mut out = Vec::new();
        for (&(t, slot), e) in side.editors.iter_mut() {
            for (id, v) in e.idle() {
                out.push((t, slot, id, v));
            }
        }
        self.shared.editors_open.store(side.editors.values().any(|e| e.is_open()), Ordering::Relaxed);
        out
    }

    /// Snapshot of independently maintained native fitting. None means no clock
    /// service; stale/uncalibrated models never certify hardware latency.
    pub fn clock_report(&self) -> Option<device_clock::ClockReport> {
        self.clock.lock().unwrap_or_else(PoisonError::into_inner).as_mut().map(device_clock::MaintainedClock::report)
    }

    /// True when the plugin in this slot has its own editor.
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.contains_key(&(track, slot))
    }

    /// Opens the plugin's own editor window. Main thread only.
    pub fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        let r = self.side().open_editor((track, slot));
        if r.is_ok() {
            self.shared.editors_open.store(true, Ordering::Relaxed);
        }
        r
    }

    pub fn close_editor(&self, track: TrackId, slot: usize) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.close();
        }
    }

    /// Closes every plugin editor window (main thread). Call before replacing the player.
    pub fn close_editors(&self) {
        for e in self.side().editors.values_mut() {
            e.close();
        }
    }

    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.get(&(track, slot)).is_some_and(|e| e.is_open())
    }

    pub fn any_editor_open(&self) -> bool {
        self.side().editors.values().any(|e| e.is_open())
    }

    /// Shows a parameter change made elsewhere in the plugin's open editor.
    pub fn editor_set_param(&self, track: TrackId, slot: usize, id: &str, value: f32) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.set_param(id, value);
        }
    }

    /// Start playback at `from`, stopping at `end`, or looping `looped`.
    pub fn play(&self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.shared.position.store(from, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play { from, end, looped });
    }

    /// Feed live input (from a `record::Recorder`) into the mix for input monitoring.
    pub fn set_input(&self, ring: Arc<record::InputRing>) {
        let _ = self.tx.send(Cmd::Input(ring));
    }

    pub fn set_recording(&self, on: bool) {
        let _ = self.tx.send(Cmd::Recording(on));
    }

    /// Play `pitch` on `track`'s instrument while the transport is stopped or playing, until a
    /// call with velocity 0 releases it (the MIDI Editor's audition).
    pub fn note(&self, track: TrackId, pitch: u8, velocity: u8) {
        let _ = self.tx.send(Cmd::Note { track, pitch, velocity });
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    /// Release every held note on every instrument (a panic button); playback carries on.
    pub fn all_notes_off(&self) {
        let _ = self.tx.send(Cmd::AllNotesOff);
    }

    pub fn locate(&self, at: Samples) {
        self.shared.position.store(at, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Locate(at));
    }

    pub fn set_speed(&self, speed: f32) {
        let v = if speed.is_finite() { (speed.clamp(0.1, 4.0) * 1000.0) as u32 } else { 1000 };
        self.shared.speed.store(v, Ordering::Relaxed);
    }

    /// Playback rate last requested, 1.0 at full speed.
    pub fn speed(&self) -> f32 {
        self.shared.speed.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn position(&self) -> Samples {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn meters(&self) -> MeterSnapshot {
        self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Observed scheduling state of this LegacyPlayer's output worker. A new LegacyPlayer owns a fresh
    /// observation; the previous device generation's success cannot carry into it.
    /// Reads backend state on the caller; performs no OS registration or log parsing.
    pub fn thread_priority(&self) -> scheduling::WorkerPriority {
        scheduling::observe(self._stream.as_ref())
    }

    /// Lifetime callback durations, nominal budget overruns, backend errors and render-silence reasons.
    /// Nonblocking and allocation-free; independent fields may straddle a callback. A newly
    /// opened LegacyPlayer starts from zero. See [`telemetry::CallbackTelemetry`] for measurement limits.
    pub fn callback_telemetry(&self) -> telemetry::CallbackTelemetry {
        self.fault.telemetry.snapshot()
    }

    /// Copy meters into reusable UI storage. Once track/channel counts are stable this performs
    /// no allocation, unlike cloning a new snapshot every video frame.
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        let source = self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner);
        copy_meter_snapshot(&source, out);
    }
}

fn copy_meter_snapshot(source: &MeterSnapshot, out: &mut MeterSnapshot) {
    out.tracks.retain(|id, _| source.tracks.contains_key(id));
    for (id, meter) in &source.tracks {
        out.tracks.entry(*id).or_default().copy_from(meter);
    }
    out.main.copy_from(&source.main);
}

impl Side {
    fn capture_states(&mut self) -> PluginStates {
        self.states.iter_mut().filter(|(k, _)| !self.refused.contains(*k)).filter_map(|(&(t, slot), r)| Some((t, slot, r.save_state()?))).collect()
    }

    fn open_editor(&mut self, key: (TrackId, usize)) -> Result<(), String> {
        let e = self.editors.get_mut(&key).ok_or_else(|| "this plugin has no editor (or it is not loaded)".to_string())?;
        e.open()?;
        self.refused.remove(&key);
        Ok(())
    }

    /// Creates the plug-in instances (built-in and third-party) `s` needs that the audio thread does not have yet.
    fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        self.collect_garbage();
        let sr = s.sample_rate.as_f64() as f32;
        if (sr - self.rate).abs() > 0.5 {
            // The audio thread rebuilds its engine: everything is created anew.
            let keys: Vec<(TrackId, usize)> = self.shadow.keys().copied().collect();
            for k in keys {
                self.forget(k);
            }
            self.rate = sr;
        }
        let specs = soundcraft_mix::realtime_instance_specs(s);
        let stale: Vec<(TrackId, usize)> = self.shadow.keys().filter(|k| !specs.iter().any(|x| (x.track, x.slot) == **k)).copied().collect();
        for k in stale {
            self.forget(k);
        }
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch, _)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            self.forget(key);
            let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), sr, BLOCK, spec.ch);
            log::debug!("plugin instance for {:?} slot {}: {} ({} ch, created: {})", spec.track, spec.slot, spec.id, spec.ch, made.is_some());
            self.shadow.insert(key, (spec.id.clone(), spec.ch, made.is_some()));
            if let Some(soundcraft_mix::NewInstance { mut plugin, refused_state }) = made {
                if let Some(e) = plugin.editor() {
                    self.editors.insert(key, e);
                }
                if let Some(r) = plugin.state_reader() {
                    self.states.insert(key, r);
                }
                if refused_state {
                    self.refused.insert(key);
                }
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin });
            }
        }
        out
    }
}

/// What [`open_device`] opens: the device, its config and format, its name and the audio system's.
type OpenedDevice = (cpal::Device, cpal::StreamConfig, cpal::SampleFormat, String, &'static str);

/// ITU stereo fold-down gains for each main channel of a session.
fn fold_gains(s: &Session) -> [(f32, f32); MAX_CHANNELS] {
    let mut g = [(0.0, 0.0); MAX_CHANNELS];
    let f = s.main_format();
    let sp = f.speakers();
    if sp.len() == f.channels() {
        for (o, x) in g.iter_mut().zip(sp.iter()) {
            *o = itu_stereo_fold(*x);
        }
    } else {
        // Ambisonics: W only, at -3 dB per side.
        g[0] = (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
    }
    g
}

/// Callback-local storage, prepared before starting the stream. Keeping the entry point here
/// lets tests exercise the same chunking, conversion and diagnostics as the real device.
struct OutputCallback {
    scratch: Vec<f32>,
    channels: usize,
    rate: u32,
    timing: telemetry::Accumulator,
}

impl OutputCallback {
    fn new(channels: usize, rate: u32) -> Self {
        // Initialise the platform clock (including any frequency cache) before callbacks run.
        let _ = telemetry::Timer::start().elapsed();
        Self { scratch: vec![0.0; 8192 * channels.max(1)], channels: channels.max(1), rate, timing: telemetry::Accumulator::default() }
    }

    fn fill<T: cpal::SizedSample + cpal::FromSample<f32>>(
        &mut self,
        data: &mut [T],
        state: &Holder,
        fault: &Fault,
        shared: &Shared,
        observe: impl FnOnce(),
    ) {
        let started = telemetry::Timer::start();
        mark_audio_thread();
        observe();
        let channels = self.channels;
        self.fill_with(data, |s, _plans, _actions| {
            render_held(state, fault, shared, s, channels);
        });
        self.timing.record(&fault.telemetry, data.len() / self.channels, self.rate, started.elapsed());
    }

    /// One device callback scope, shared by all scratch conversion chunks. The prepared
    /// LegacyPlayer integration uses this token; it must never be reconstructed inside a chunk.
    fn fill_with<T: cpal::SizedSample + cpal::FromSample<f32>>(
        &mut self,
        data: &mut [T],
        mut render: impl FnMut(&mut [f32], &mut plan_bridge::CallbackBudget, &mut action_lane::Budget),
    ) {
        let mut budget = plan_bridge::callback_scope();
        let mut actions = action_lane::callback_scope();
        for out in data.chunks_mut(self.scratch.len()) {
            let Some(s) = self.scratch.get_mut(..out.len()) else { break };
            render(s, &mut budget, &mut actions);
            for (o, x) in out.iter_mut().zip(s.iter()) {
                *o = T::from_sample(*x);
            }
        }
    }
}

/// The same error handler is registered with CPAL and exercised by hardware-free tests.
fn output_error_callback(gone: Arc<Fault>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    // CPAL may call this on the audio thread (on ALSA repeatedly while the device is broken).
    // Mark it so the logger hands records off instead of writing them here.
    move |e| {
        mark_audio_thread();
        let device_lost = matches!(e, cpal::StreamError::DeviceNotAvailable);
        gone.telemetry.error(device_lost);
        if device_lost {
            gone.gone.store(true, Ordering::Relaxed);
        }
        log::warn!("audio stream error: {e}");
    }
}

/// The output stream: render through preallocated scratch and convert to the device format.
fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: &Holder,
    channels: usize,
    fault: &Arc<Fault>,
    shared: &Arc<Shared>,
) -> Result<(cpal::Stream, device_clock::ClockReader), cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let mut output = OutputCallback::new(channels, config.sample_rate.0);
    let (mut clock, reader) = device_clock::stream_clock(device_clock::Direction::Output, config.sample_rate.0)
        .map_err(|error| cpal::BackendSpecificError { description: error.into() })?;
    let (failed, guard, gone, held) = (Arc::clone(fault), Arc::clone(shared), Arc::clone(fault), Arc::clone(state));
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels.max(1);
            output.fill(data, &held, &failed, &guard, || clock.cpal.output(frames, info, clock.origin.now_ns()));
        },
        output_error_callback(gone),
        None,
    )?;
    Ok((stream, reader))
}

/// What an output stream is opened with: the configuration, the buffer sizes it allows, the format.
type OutputChoice = (cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat);

/// Hosts whose channel order is the driver's physical order, so opening every channel puts
/// device channel 1 on output 1 (ADR 0015). Elsewhere (ALSA, CoreAudio, others) a wide config may
/// map the first channels to something other than the front pair (PipeWire's 32-channel
/// "default", aggregate devices).
// TODO(ADR 0015): validate each remaining host's channel order, then add it here.
fn physical_channel_order(host: &str) -> bool {
    host == devices::ASIO_SYSTEM || host == "WASAPI"
}

/// The best output configuration at one of `rates` (tried in order): the best sample format first
/// (f32, else 32- or 16-bit integers), then the most channels. On ASIO and WASAPI
/// ([`physical_channel_order`]) that is the widest offered from stereo up to
/// [`MAX_DEVICE_CHANNELS`] (ADR 0015: every output opens; paths other than Main stay silent in
/// this slice). On every other host the previous rule holds: stereo up to `want_ch`, the session's
/// width. A stereo-only device gets its two channels exactly as before. WASAPI shared mode lists
/// only the system mix format's channel count, so on Windows more than that is ASIO.
fn choose_output(configs: &[cpal::SupportedStreamConfigRange], rates: &[u32], host: &str, want_ch: u16) -> Option<OutputChoice> {
    let most = if physical_channel_order(host) { MAX_DEVICE_CHANNELS } else { usize::from(want_ch.max(2)) };
    let better = |c: &cpal::SupportedStreamConfigRange, x: &OutputChoice| {
        let (rc, rx) = (devices::format_rank(c.sample_format()).unwrap_or(usize::MAX), devices::format_rank(x.2).unwrap_or(usize::MAX));
        rc < rx || (rc == rx && c.channels() > x.0.channels)
    };
    for &rate in rates {
        let mut chosen: Option<OutputChoice> = None;
        for c in configs {
            if devices::format_rank(c.sample_format()).is_some()
                && c.min_sample_rate().0 <= rate
                && c.max_sample_rate().0 >= rate
                && c.channels() >= 2
                && usize::from(c.channels()) <= most
                && chosen.as_ref().is_none_or(|x| better(c, x))
            {
                chosen = Some((c.with_sample_rate(cpal::SampleRate(rate)).config(), *c.buffer_size(), c.sample_format()));
            }
        }
        if chosen.is_some() {
            return chosen;
        }
    }
    None
}

/// The stream configurations to try, in order: the chosen one with its buffer size, then with the
/// device's default buffer; and, when the chosen width is wider than the session's (every output
/// opened, ADR 0015), the same two at `session_width` (at least 2), so a driver that refuses a
/// wide buffer set still opens as it did before.
fn output_attempts(config: &cpal::StreamConfig, session_width: u16) -> Vec<cpal::StreamConfig> {
    let narrow = session_width.max(2);
    let widths: Vec<u16> = if config.channels > narrow { vec![config.channels, narrow] } else { vec![config.channels] };
    let mut attempts = Vec::new();
    for channels in widths {
        attempts.push(cpal::StreamConfig { channels, ..config.clone() });
        if config.buffer_size != cpal::BufferSize::Default {
            attempts.push(cpal::StreamConfig { channels, buffer_size: cpal::BufferSize::Default, ..config.clone() });
        }
    }
    attempts
}

/// Open the output device `cfg` chooses (else the default) at the chosen rate (else the session
/// rate) if possible, with every channel it offers ([`choose_output`]), and the chosen buffer
/// size when the device allows it.
/// Also returns the audio system's name.
fn open_device(want_rate: u32, want_ch: u16, cfg: &AudioConfig) -> Result<OpenedDevice, String> {
    use cpal::traits::DeviceTrait;
    let host = devices::host(cfg).ok_or_else(|| "no audio system".to_string())?;
    let device = devices::device(host, cfg.output.as_deref(), true).ok_or_else(|| "no output device".to_string())?;
    #[allow(deprecated)]
    let name = device.name().unwrap_or_else(|_| "Audio device".into());
    // Prefer a config at the chosen (else session) rate; else the device default (we resample).
    let rates: Vec<u32> = cfg.sample_rate.into_iter().chain(Some(want_rate)).collect();
    let chosen =
        device.supported_output_configs().ok().and_then(|configs| choose_output(&configs.collect::<Vec<_>>(), &rates, host.id().name(), want_ch));
    let (config, format) = match chosen {
        Some((mut c, buffers, format)) => {
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
        None => {
            let d = device.default_output_config().map_err(|e| e.to_string())?;
            if devices::format_rank(d.sample_format()).is_none() {
                return Err(format!("unsupported device sample format {:?}", d.sample_format()));
            }
            let buffers = *d.buffer_size();
            let format = d.sample_format();
            let mut c = d.config();
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
    };
    Ok((device, config, format, name, host.id().name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output ranges as an ASIO driver lists them: every width 1..=`most` at 44.1 and 48 kHz.
    fn widths(most: u16, format: cpal::SampleFormat) -> Vec<cpal::SupportedStreamConfigRange> {
        (1..=most)
            .map(|n| {
                cpal::SupportedStreamConfigRange::new(
                    n,
                    cpal::SampleRate(44_100),
                    cpal::SampleRate(48_000),
                    cpal::SupportedBufferSize::Range { min: 64, max: 2048 },
                    format,
                )
            })
            .collect()
    }

    #[test]
    fn a_two_channel_device_opens_the_same_two_channels_as_before() {
        let (config, _, format) = choose_output(&widths(2, cpal::SampleFormat::F32), &[48_000], "ASIO", 2).unwrap();
        assert_eq!((config.channels, config.sample_rate.0, format), (2, 48_000, cpal::SampleFormat::F32));
        // A device that lists only stereo (WASAPI's mix format) is the same.
        let only = widths(2, cpal::SampleFormat::F32).split_off(1);
        assert_eq!(choose_output(&only, &[48_000], "ASIO", 2).unwrap().0.channels, 2);
    }

    #[test]
    fn a_wide_device_opens_every_output_up_to_the_cap() {
        assert_eq!(choose_output(&widths(10, cpal::SampleFormat::I32), &[48_000], "ASIO", 2).unwrap().0.channels, 10);
        let capped = choose_output(&widths(34, cpal::SampleFormat::I32), &[48_000], "ASIO", 2).unwrap().0.channels;
        assert_eq!(usize::from(capped), MAX_DEVICE_CHANNELS, "a 34-channel device opens its first 32");
        // A device that lists only an over-cap width gets the default configuration instead.
        assert!(choose_output(&widths(34, cpal::SampleFormat::I32).split_off(33), &[48_000], "ASIO", 2).is_none());
        // A mono-only device is not chosen here either (the default configuration serves it).
        assert!(choose_output(&widths(1, cpal::SampleFormat::F32), &[48_000], "ASIO", 2).is_none());
    }

    #[test]
    fn hosts_without_a_known_channel_order_keep_the_session_width_rule() {
        // PipeWire's ALSA "default" lists up to 32 channels: not opened wide.
        for host in ["ALSA", "CoreAudio", "JACK"] {
            let wide = widths(32, cpal::SampleFormat::F32);
            assert_eq!(choose_output(&wide, &[48_000], host, 2).unwrap().0.channels, 2, "{host}: a stereo session");
            assert_eq!(choose_output(&wide, &[48_000], host, 6).unwrap().0.channels, 6, "{host}: a 5.1 session");
            assert_eq!(choose_output(&wide, &[48_000], host, 1).unwrap().0.channels, 2, "{host}: never below stereo");
        }
        // ASIO and WASAPI open the widest, whatever the session width.
        for host in ["ASIO", "WASAPI"] {
            assert_eq!(choose_output(&widths(10, cpal::SampleFormat::F32), &[48_000], host, 2).unwrap().0.channels, 10, "{host}");
        }
    }

    #[test]
    fn a_refused_wide_open_retries_at_the_session_width_before_giving_up() {
        let wide = cpal::StreamConfig { channels: 10, sample_rate: cpal::SampleRate(48_000), buffer_size: cpal::BufferSize::Fixed(128) };
        let tried: Vec<(u16, cpal::BufferSize)> = output_attempts(&wide, 2).into_iter().map(|c| (c.channels, c.buffer_size)).collect();
        assert_eq!(
            tried,
            [(10, cpal::BufferSize::Fixed(128)), (10, cpal::BufferSize::Default), (2, cpal::BufferSize::Fixed(128)), (2, cpal::BufferSize::Default)]
        );
        // A 5.1 session retries at six; a device already at the session width has nothing narrower.
        assert_eq!(output_attempts(&wide, 6).last().map(|c| c.channels), Some(6));
        let stereo = cpal::StreamConfig { channels: 2, buffer_size: cpal::BufferSize::Default, ..wide };
        assert_eq!(output_attempts(&stereo, 2).len(), 1);
        assert_eq!(output_attempts(&stereo, 1).len(), 1, "the session width is at least stereo");
    }

    #[test]
    fn format_beats_width_and_the_wanted_rate_beats_the_other() {
        let mut configs = widths(10, cpal::SampleFormat::I32);
        configs.extend(widths(2, cpal::SampleFormat::F32));
        assert_eq!(choose_output(&configs, &[48_000], "ASIO", 2).unwrap().0.channels, 2, "f32 stereo is preferred over 32-bit integers");
        let (config, ..) = choose_output(&widths(4, cpal::SampleFormat::F32), &[96_000, 44_100], "ASIO", 2).unwrap();
        assert_eq!((config.channels, config.sample_rate.0), (4, 44_100));
    }

    #[test]
    fn reusable_meter_snapshot_updates_and_drops_stale_tracks() {
        let keep = TrackId(1);
        let stale = TrackId(2);
        let mut source = MeterSnapshot::default();
        source.tracks.insert(keep, StripMeter { peak: [0.5, 0.25], peaks: vec![0.5, 0.25], gain_reduction_db: 3.0, ..StripMeter::default() });
        source.main.peaks = vec![0.75, 0.5];
        let mut out = MeterSnapshot::default();
        out.tracks.insert(stale, StripMeter::default());
        copy_meter_snapshot(&source, &mut out);
        assert_eq!(out.tracks.len(), 1);
        assert_eq!(out.tracks[&keep], source.tracks[&keep]);
        assert_eq!(out.main, source.main);
    }

    #[test]
    fn a_block_that_panics_goes_silent_and_is_reported_once() {
        let shared = Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(true),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        };
        let fault = Fault::default();
        let mut out = [0.5f32; 8];
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25)).unwrap();
        assert_eq!((out, fault.trouble(false)), ([0.25; 8], None));
        let payload = guarded_block(&fault, &shared, &mut out, |_| std::panic::panic_any("a bug in the mix")).unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"a bug in the mix"));
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25)).unwrap();
        assert_eq!(out, [0.0; 8]);
        assert!(!shared.playing.load(Ordering::Relaxed));
        assert_eq!((fault.trouble(false), fault.trouble(false)), (Some(OutputTrouble::Bug), None));
        assert_eq!(fault.beats.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn callback_telemetry_counts_device_blocks_and_silence_reasons() {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let holder: Holder =
            Arc::new(Mutex::new(Some(AudioState::new(channel().1, Arc::clone(&shared), Arc::new(Session::default()), 48_000.0, channel().0))));
        let fault = Fault::default();
        let mut callback = OutputCallback::new(2, 48_000);
        let mut data = vec![1i16; 2 * (8192 + 64)];
        // Two scratch chunks are still exactly one device callback, with conversion included.
        let locked = holder.lock().unwrap();
        callback.fill(&mut data, &holder, &fault, &shared, || {});
        drop(locked);
        assert!(data.iter().all(|&x| x == 0));
        let s = fault.telemetry.snapshot();
        assert_eq!((s.callbacks, s.frames, s.contention_silence_frames), (1, 8256, 8256));
        assert_eq!((s.unavailable_silence_frames, s.fault_silence_frames), (0, 0));
        fault.failed.store(true, Ordering::Relaxed);
        callback.fill(&mut data[..128], &holder, &fault, &shared, || {});
        holder.lock().unwrap().take();
        callback.fill(&mut data[..128], &holder, &fault, &shared, || {});
        let s = fault.telemetry.snapshot();
        assert_eq!((s.callbacks, s.frames), (3, 8384));
        assert_eq!((s.contention_silence_frames, s.fault_silence_frames, s.unavailable_silence_frames), (8256, 64, 64));
        assert_eq!(s.backend_errors, 0);
        #[cfg(not(target_arch = "wasm32"))]
        {
            assert_eq!(s.timed_callbacks, 3);
            assert_eq!(s.duration_histogram.iter().sum::<u64>(), 3);
        }
    }

    #[test]
    fn cpal_error_callback_classifies_device_loss_without_inventing_budget_failures() {
        let fault = Arc::new(Fault::default());
        let mut callback = output_error_callback(Arc::clone(&fault));
        callback(cpal::StreamError::BackendSpecific { err: cpal::BackendSpecificError { description: "test backend failure".into() } });
        assert_eq!(fault.trouble(false), None);
        callback(cpal::StreamError::DeviceNotAvailable);
        assert_eq!((fault.trouble(false), fault.trouble(false)), (Some(OutputTrouble::Gone), None));
        let s = fault.telemetry.snapshot();
        assert_eq!((s.backend_errors, s.device_lost_errors), (2, 1));
        assert_eq!((s.callbacks, s.over_budget_callbacks), (0, 0));
    }

    #[test]
    fn silent_clock_has_no_device_counts_and_new_players_start_fresh() {
        let player = silent_player();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while player.fault.beats.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(player.fault.beats.load(Ordering::Relaxed) > 0, "exercise the real silent-clock worker");
        assert_eq!(player.callback_telemetry(), telemetry::CallbackTelemetry::default());
        output_error_callback(Arc::clone(&player.fault))(cpal::StreamError::DeviceNotAvailable);
        assert_eq!(player.callback_telemetry().device_lost_errors, 1);
        let fresh = silent_player();
        assert_eq!(fresh.callback_telemetry(), telemetry::CallbackTelemetry::default());
    }

    #[test]
    fn a_lost_or_stalled_device_is_reported_once() {
        let ago = |seen: &mut Option<(u64, std::time::Instant)>| {
            if let Some(s) = seen {
                s.1 = std::time::Instant::now().checked_sub(STALL).unwrap();
            }
        };
        let mut seen = None;
        assert!(!stalled(&mut seen, 5, true));
        ago(&mut seen);
        assert!(stalled(&mut seen, 5, true));
        assert!(!stalled(&mut seen, 6, true), "it asked again");
        ago(&mut seen);
        assert!(!stalled(&mut seen, 6, false), "stopped, so nothing to ask for");
        assert_eq!(Fault::default().trouble(true), Some(OutputTrouble::Stalled));
        let fault = Fault::default();
        fault.gone.store(true, Ordering::Relaxed);
        assert_eq!((fault.trouble(false), fault.trouble(true)), (Some(OutputTrouble::Gone), None));
    }

    #[test]
    fn audio_state_plays_and_stops_at_end() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(shared.position.load(Ordering::Relaxed) >= 1000);
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    struct LogInstrument {
        inner: Box<dyn Plugin>,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Plugin for LogInstrument {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.log.lock().unwrap().push("reset");
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.log.lock().unwrap().push("process");
            self.inner.process(io, frames);
        }
        fn note_on(&mut self, _offset: usize, _note: u8, _velocity: u8) {
            self.log.lock().unwrap().push("note_on");
        }
        fn note_off(&mut self, _offset: usize, _note: u8) {
            self.log.lock().unwrap().push("note_off");
        }
        fn all_notes_off(&mut self) {
            self.log.lock().unwrap().push("all_notes_off");
        }
    }

    #[test]
    fn reaching_the_end_releases_held_notes() {
        use soundcraft_model::{ChannelFormat, Clip, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.log"));
        let mut seq = soundcraft_midi::Sequence::default();
        seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 480 });
        let cid = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "m", 0, 1024, seq));
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, channel().0);
        let log = Arc::new(Mutex::new(Vec::new()));
        let plugin = Box::new(LogInstrument { inner: soundcraft_dsp::create("gain").unwrap(), log: Arc::clone(&log) });
        tx.send(Cmd::Session(
            Arc::new(s),
            vec![PreparedInstance { track: t, slot: soundcraft_mix::INSTRUMENT_SLOT, id: "clap:test.log".into(), ch: 2, plugin }],
        ))
        .unwrap();
        tx.send(Cmd::Play { from: 0, end: Some(1024), looped: None }).unwrap();
        let mut out = vec![0.0f32; 2 * 2048];
        st.fill(&mut out, 2);
        st.fill(&mut out, 2);
        let log = log.lock().unwrap();
        let on = log.iter().position(|e| *e == "note_on");
        assert!(on.is_some(), "the note started: {log:?}");
        let off = log.iter().rposition(|e| *e == "all_notes_off");
        assert!(off.is_some_and(|o| o > on.unwrap_or(0)), "held notes were released at the end: {log:?}");
        assert!(log.iter().skip(off.unwrap_or(0)).any(|e| *e == "process"), "a process pass followed the release: {log:?}");
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn short_device_callbacks_render_short_mix_blocks() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 2 * 64];
        st.fill(&mut out, 2);
        assert_eq!(shared.position.load(Ordering::Relaxed), 64);
    }

    #[test]
    fn surround_main_maps_to_device_channels_or_folds_down() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
        let mut s = soundcraft_model::Session::default();
        s.outputs[0].format = ChannelFormat::Surround51;
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Surround51, None);
        let chans: Vec<Vec<f32>> = (0..6).map(|c| vec![0.1 * (c + 1) as f32; 48_000]).collect();
        s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: chans })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 48_000));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        // An 8-channel device: channels in order, the rest silent.
        let mut out = vec![0.0f32; 8 * 256];
        st.fill(&mut out, 8);
        let f = &out[8 * 100..8 * 101];
        for c in 0..6 {
            assert!((f[c] - 0.1 * (c + 1) as f32).abs() < 1e-5, "{f:?}");
        }
        assert!(f[6] == 0.0 && f[7] == 0.0);
        // A stereo device: ITU fold-down.
        let mut out = vec![0.0f32; 2 * 256];
        st.fill(&mut out, 2);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let l = (0.1 + h * 0.3 + h * 0.5f32).min(1.0);
        assert!((out[200] - l).abs() < 1e-4, "{} vs {l}", out[200]);
        assert_eq!(shared.meters.lock().unwrap().main.peaks.len(), 6);
    }

    #[test]
    fn instances_arrive_with_the_session_and_leave_through_the_return_channel() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.fake"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let plugin = soundcraft_dsp::create("gain").unwrap();
        tx.send(Cmd::Session(Arc::clone(&s), vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin }])).unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        assert!(grx.try_recv().is_err(), "nothing retired yet");
        // A session without the insert retires the instance to the return channel.
        tx.send(Cmd::Session(Arc::new(soundcraft_model::Session::default()), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
        // A sample-rate change rebuilds the engine without dropping plugins on this thread.
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin: soundcraft_dsp::create("gain").unwrap() }],
        ))
        .unwrap();
        st.fill(&mut out, 2);
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(Cmd::Session(Arc::new(s44), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
    }

    /// Records the thread of every lifecycle call, so a test can assert where teardown ran.
    struct ThreadLog {
        inner: Box<dyn Plugin>,
        log: Arc<std::sync::Mutex<Vec<(&'static str, std::thread::ThreadId)>>>,
    }

    impl ThreadLog {
        fn note(&self, what: &'static str) {
            let mut log = self.log.lock().unwrap();
            if log.last().map(|(w, _)| *w) != Some(what) {
                log.push((what, std::thread::current().id()));
            }
        }
    }

    impl Plugin for ThreadLog {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.note("prepare");
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.note("process");
            self.inner.process(io, frames);
        }
    }

    impl Drop for ThreadLog {
        fn drop(&mut self) {
            self.note("drop");
        }
    }

    /// Gate (quit deadlock, 2026-10-18): a hosted plugin's teardown runs on the thread that owns the
    /// player, never on the audio thread. A VST3's `terminate` may wait for the UI thread; if it ran on
    /// the audio thread while the UI thread joined that thread, the app hung on quit. The fake records
    /// where every call ran; the player here drives the real silent-clock thread and the real `Drop`.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn plugin_teardown_never_runs_on_the_audio_thread() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.threads"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let player = LegacyPlayer::silent_clock_with(shared, tx.clone(), rx, Arc::clone(&s), Mutex::new(Side::new(48_000.0, grx)), gtx);
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = ThreadLog { inner: soundcraft_dsp::create("gain").unwrap(), log: Arc::clone(&log) };
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance {
                track: t,
                slot: soundcraft_mix::INSTRUMENT_SLOT,
                id: "clap:test.threads".into(),
                ch: 2,
                plugin: Box::new(plugin),
            }],
        ))
        .unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        // Wait until the clock thread has processed the instance (it runs every 10 ms).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !log.lock().unwrap().iter().any(|(w, _)| *w == "process") {
            assert!(std::time::Instant::now() < deadline, "the clock thread never processed the instance: {:?}", log.lock().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(player);
        let log = log.lock().unwrap();
        let here = std::thread::current().id();
        let processed_on = log.iter().find(|(w, _)| *w == "process").map(|(_, id)| *id).unwrap();
        let dropped_on = log.iter().find(|(w, _)| *w == "drop").map(|(_, id)| *id);
        assert_ne!(processed_on, here, "sanity: processing ran on the clock thread, not here");
        assert_eq!(dropped_on, Some(here), "teardown must run on the player's thread, not the audio thread: {log:?}");
    }

    /// Puts out a constant level whenever it is processed, so a test can hear whether a track ran.
    struct Tone {
        inner: Box<dyn Plugin>,
    }

    impl Plugin for Tone {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            for ch in io.iter_mut() {
                for x in ch.iter_mut().take(frames) {
                    *x = 0.5;
                }
            }
        }
    }

    /// Gate (silent armed instrument, 2026-10-17; hosted rule, ADR 0007): the stopped-transport
    /// engine table. Each row is (track kind, armed, input monitor, hosted editor open, hosted
    /// plug-in present) and whether the engine must run while the transport is stopped and no audio
    /// input stream is open. Any hosted plug-in keeps it running (it expects `process` calls); without
    /// one, an armed or monitoring instrument runs so it can be played from its own keyboard, an idle
    /// track does not (no CPU for nothing), an armed audio track without an input stream has nothing to
    /// monitor, and an open editor runs the engine. A new cell is one more row.
    #[test]
    fn stopped_transport_audibility_table() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        // (kind, armed, input monitor, editor open, hosted plug-in present, engine must run)
        let cells = [
            (TrackKind::Instrument, true, false, false, true, true),
            (TrackKind::Instrument, false, true, false, true, true),
            (TrackKind::Instrument, false, false, false, true, true),
            (TrackKind::Instrument, false, false, true, true, true),
            (TrackKind::Audio, true, false, false, true, true),
            (TrackKind::Audio, false, true, false, true, true),
            (TrackKind::Audio, false, false, true, true, true),
            (TrackKind::Audio, false, false, false, false, false),
            (TrackKind::Instrument, false, false, false, false, false),
            (TrackKind::Instrument, true, false, false, false, true),
            (TrackKind::Audio, true, false, false, false, false),
        ];
        for (kind, armed, monitor, editor_open, hosted, expect_runs) in cells {
            let mut s = Session::default();
            let t = s.add_track(kind, ChannelFormat::Stereo, None);
            let slot = if kind == TrackKind::Instrument { soundcraft_mix::INSTRUMENT_SLOT } else { 0 };
            {
                let tr = s.track_mut(t).unwrap();
                tr.mixer.record_arm = armed;
                tr.mixer.input_monitor = monitor;
                if hosted {
                    if slot == soundcraft_mix::INSTRUMENT_SLOT {
                        tr.instrument = Some(Insert::new("clap:test.tone"));
                    } else {
                        tr.mixer.inserts[0] = Some(Insert::new("clap:test.tone"));
                    }
                }
            }
            let s = Arc::new(s);
            let shared = Arc::new(Shared {
                position: AtomicI64::new(0),
                playing: AtomicBool::new(false),
                speed: AtomicU32::new(1000),
                meters: Mutex::new(MeterSnapshot::default()),
                editors_open: AtomicBool::new(false),
            });
            let (tx, rx) = channel();
            let (gtx, _grx) = channel();
            let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(Session::default()), 48_000.0, gtx);
            let instances = if hosted {
                let plugin = Tone { inner: soundcraft_dsp::create("gain").unwrap() };
                vec![PreparedInstance { track: t, slot, id: "clap:test.tone".into(), ch: 2, plugin: Box::new(plugin) }]
            } else {
                Vec::new()
            };
            tx.send(Cmd::Session(Arc::clone(&s), instances)).unwrap();
            shared.editors_open.store(editor_open, Ordering::Relaxed);
            let mut out = vec![0.0f32; 2 * 2048];
            // Two callbacks: the first adopts the instance, the second is steady state.
            st.fill(&mut out, 2);
            st.fill(&mut out, 2);
            let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            let cell = format!("kind={kind:?} armed={armed} monitor={monitor} editor_open={editor_open} hosted={hosted}");
            assert_eq!(st.renders > 0, expect_runs, "engine ran: {cell}: renders {}", st.renders);
            if hosted && expect_runs {
                assert!(peak > 0.1, "hosted tone audible: {cell}: peak {peak}");
            }
        }
    }

    /// An effect that sleeps by its own status and counts its `process` calls.
    struct Sleepy {
        inner: Box<dyn Plugin>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Plugin for Sleepy {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.process(io, frames);
        }
        fn process_status(&self) -> soundcraft_dsp::ProcessStatus {
            soundcraft_dsp::ProcessStatus::Sleep
        }
    }

    #[test]
    fn an_open_editor_keeps_hosted_effects_from_being_suspended() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.sleepy"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, _grx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let plugin = Sleepy { inner: soundcraft_dsp::create("gain").unwrap(), calls: Arc::clone(&calls) };
        let instances = vec![PreparedInstance { track: t, slot: 0, id: "clap:test.sleepy".into(), ch: 2, plugin: Box::new(plugin) }];
        tx.send(Cmd::Session(s, instances)).unwrap();
        let mut out = vec![0.0f32; 2 * 2048];
        st.fill(&mut out, 2);
        let after_adoption = calls.load(Ordering::Relaxed);
        st.fill(&mut out, 2);
        assert_eq!(calls.load(Ordering::Relaxed), after_adoption, "suspended: no process calls with silent input and no editor open");
        shared.editors_open.store(true, Ordering::Relaxed);
        st.fill(&mut out, 2);
        st.fill(&mut out, 2);
        assert!(calls.load(Ordering::Relaxed) > after_adoption, "an open editor must keep the effect processed");
    }

    /// Review finding (PR 752): a device that stops calling (WASAPI after a device error, an
    /// unplugged interface, a stalled driver) never runs the callback again, so nothing on the
    /// audio side can hand the instances back. The player takes the state out of the holder
    /// itself. Here a second thread renders once, then only waits for the player to go and
    /// releases its share of the holder, as cpal's worker destroys the callback after a join.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn the_player_takes_the_state_back_when_the_callback_has_stopped() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.threads"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let state: Holder = Arc::new(Mutex::new(Some(AudioState::new(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, gtx))));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = ThreadLog { inner: soundcraft_dsp::create("gain").unwrap(), log: Arc::clone(&log) };
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance {
                track: t,
                slot: soundcraft_mix::INSTRUMENT_SLOT,
                id: "clap:test.threads".into(),
                ch: 2,
                plugin: Box::new(plugin),
            }],
        ))
        .unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let fault = Arc::new(Fault::default());
        let (held, failed, clock) = (Arc::clone(&state), Arc::clone(&fault), Arc::clone(&shared));
        let (rendered, once) = sync_channel(1);
        let device = std::thread::spawn(move || {
            let mut out = vec![0.0f32; 1024];
            assert!(render_held(&held, &failed, &clock, &mut out, 2), "the state is there to render");
            rendered.send(()).unwrap();
            // The device stops calling: the holder is never locked again, only let go of once
            // the player has gone.
            while Arc::strong_count(&held) > 1 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        once.recv().unwrap();
        assert!(log.lock().unwrap().iter().any(|(w, _)| *w == "process"), "sanity: the instance was processed");
        let player = LegacyPlayer::silent(shared, fault, tx, Mutex::new(Side::new(48_000.0, grx)), state, 48_000);
        drop(player);
        device.join().unwrap();
        let log = log.lock().unwrap();
        let here = std::thread::current().id();
        let processed_on = log.iter().find(|(w, _)| *w == "process").map(|(_, id)| *id).unwrap();
        let dropped_on = log.iter().find(|(w, _)| *w == "drop").map(|(_, id)| *id);
        assert_ne!(processed_on, here, "sanity: processing ran on the device thread, not here");
        assert_eq!(dropped_on, Some(here), "teardown must run on the player's thread even when the callback has stopped: {log:?}");
    }

    /// Records where an arbitrary caught panic payload is destroyed.
    struct PanicPayloadLog(Arc<Mutex<Option<std::thread::ThreadId>>>);

    impl Drop for PanicPayloadLog {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Some(std::thread::current().id());
        }
    }

    /// Like `ThreadLog`, but `process` panics; records both destruction threads.
    struct PanicLog {
        payload_dropped_on: Arc<Mutex<Option<std::thread::ThreadId>>>,
        inner: Box<dyn Plugin>,
        dropped_on: Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>,
    }

    impl Plugin for PanicLog {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, _io: &mut [Vec<f32>], _frames: usize) {
            std::panic::panic_any(PanicPayloadLog(Arc::clone(&self.payload_dropped_on)));
        }
    }

    impl Drop for PanicLog {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = Some(std::thread::current().id());
        }
    }

    /// Gate (review finding, PR 752): after a plug-in panicked on the audio thread the player's
    /// drop still gets the instances back and tears them down on its own thread.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn plugin_and_panic_payload_teardown_never_run_on_the_audio_thread_after_a_fault() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.panics"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let player = LegacyPlayer::silent_clock_with(shared, tx.clone(), rx, Arc::clone(&s), Mutex::new(Side::new(48_000.0, grx)), gtx);
        let dropped_on = Arc::new(std::sync::Mutex::new(None));
        let payload_dropped_on = Arc::new(Mutex::new(None));
        let plugin = PanicLog {
            inner: soundcraft_dsp::create("gain").unwrap(),
            dropped_on: Arc::clone(&dropped_on),
            payload_dropped_on: Arc::clone(&payload_dropped_on),
        };
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance {
                track: t,
                slot: soundcraft_mix::INSTRUMENT_SLOT,
                id: "clap:test.panics".into(),
                ch: 2,
                plugin: Box::new(plugin),
            }],
        ))
        .unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !player.fault.failed.load(Ordering::Relaxed) {
            assert!(std::time::Instant::now() < deadline, "the plug-in never faulted the engine");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Synchronize with completion of the callback, not merely its fault flag.
        // The silent-clock callback and the real device share render_held.
        {
            let held = player.state.lock().unwrap_or_else(PoisonError::into_inner);
            assert!(held.as_ref().unwrap().panic_payload.is_some());
            assert_eq!(*payload_dropped_on.lock().unwrap(), None, "the callback must retain the caught payload");
        }
        drop(player);
        let here = std::thread::current().id();
        assert_eq!(*dropped_on.lock().unwrap(), Some(here), "after a fault, teardown must still run on the player's thread");
        assert_eq!(*payload_dropped_on.lock().unwrap(), Some(here), "panic payload teardown must also run on the player's thread");
    }

    /// Like `ThreadLog`, but `process` is slow and says when it started and finished.
    struct SlowLog {
        inner: Box<dyn Plugin>,
        log: Arc<std::sync::Mutex<Vec<(&'static str, std::thread::ThreadId)>>>,
    }

    impl Plugin for SlowLog {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.log.lock().unwrap().push(("process started", std::thread::current().id()));
            std::thread::sleep(std::time::Duration::from_millis(60));
            self.inner.process(io, frames);
            self.log.lock().unwrap().push(("process finished", std::thread::current().id()));
        }
    }

    impl Drop for SlowLog {
        fn drop(&mut self) {
            self.log.lock().unwrap().push(("dropped", std::thread::current().id()));
        }
    }

    /// Review finding (PR 752): the player's drop must wait for a block in progress before it takes
    /// the state; taking it mid-block would drop the instances under the running `process`.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn the_player_waits_for_a_block_in_progress_before_taking_the_state() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.slow"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let player = LegacyPlayer::silent_clock_with(shared, tx.clone(), rx, Arc::clone(&s), Mutex::new(Side::new(48_000.0, grx)), gtx);
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = SlowLog { inner: soundcraft_dsp::create("gain").unwrap(), log: Arc::clone(&log) };
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance { track: t, slot: soundcraft_mix::INSTRUMENT_SLOT, id: "clap:test.slow".into(), ch: 2, plugin: Box::new(plugin) }],
        ))
        .unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !log.lock().unwrap().iter().any(|(w, _)| *w == "process started") {
            assert!(std::time::Instant::now() < deadline, "the clock thread never started a block: {:?}", log.lock().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // The block is under way and holds the lock; the player's drop must wait for it.
        drop(player);
        let log = log.lock().unwrap();
        let at = |what: &str| log.iter().position(|(w, _)| *w == what);
        assert!(at("process finished").is_some(), "the drop must wait for the block in progress: {log:?}");
        assert!(at("process finished") < at("dropped") && at("dropped").is_some(), "teardown must follow the block: {log:?}");
        let here = std::thread::current().id();
        assert_eq!(at("dropped").map(|i| log[i].1), Some(here), "teardown must run on the player's thread: {log:?}");
    }

    /// Fault callbacks retain queued owners; Main services at most 64 per pass.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn postfault_commands_wait_for_bounded_control_reclamation() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.late"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let holder: Holder = Arc::new(Mutex::new(Some(AudioState::new(rx, Arc::clone(&shared), Arc::new(Session::default()), 48_000.0, gtx))));
        let fault = Arc::new(Fault::default());
        let mut out = vec![0.0f32; 8];
        let _payload = guarded_block(&fault, &shared, &mut out, |_| std::panic::panic_any("a plug-in bug")).unwrap_err();
        assert!(fault.failed.load(Ordering::Relaxed));
        // ThreadLog coalesces consecutive equal lifecycle events. Each owner
        // gets its own witness so the count is not hidden by that coalescing.
        let logs: Vec<_> = (0..65).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
        for log in &logs {
            let plugin = ThreadLog { inner: soundcraft_dsp::create("gain").unwrap(), log: Arc::clone(log) };
            let late =
                PreparedInstance { track: t, slot: soundcraft_mix::INSTRUMENT_SLOT, id: "clap:test.late".into(), ch: 2, plugin: Box::new(plugin) };
            tx.send(Cmd::Session(Arc::clone(&s), vec![late])).unwrap();
        }
        tx.send(Cmd::Stop).unwrap();
        let player = LegacyPlayer::silent(shared, fault, tx, Mutex::new(Side::new(48_000.0, grx)), holder, 48_000);
        let (held, fault, shared) = (Arc::clone(&player.state), Arc::clone(&player.fault), Arc::clone(&player.shared));
        let heap = std::thread::spawn(move || {
            let mut out = [1.0; 8];
            let (alive, heap) = soundcraft_rt::testing::measure(|| render_held(&held, &fault, &shared, &mut out, 2));
            assert!(alive);
            assert_eq!(out, [0.0; 8]);
            heap
        })
        .join()
        .unwrap();
        assert!(heap.is_zero(), "fault callbacks leave queued owners untouched: {heap:?}");
        let untouched = logs.iter().all(|log| log.lock().unwrap().is_empty());
        assert!(untouched);
        player.idle();
        // Release every witness lock before asserting: a failed assertion must
        // not poison a log that a still-queued owner's destructor will use.
        let first = logs.iter().filter(|log| !log.lock().unwrap().is_empty()).count();
        assert_eq!(first, 64, "Main service is bounded");
        player.idle();
        let observed: Vec<_> = logs.iter().flat_map(|log| log.lock().unwrap().clone()).collect();
        assert_eq!(observed.len(), 65);
        assert!(observed.iter().all(|(what, id)| *what == "drop" && *id == std::thread::current().id()));
        let held = player.state.lock().unwrap();
        assert!(held.as_ref().unwrap().rx.try_recv().is_err());
    }

    #[test]
    fn session_has_hosted_sees_third_party_plugins_only() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = Session::default();
        assert!(!session_has_hosted(&s));
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
        assert!(!session_has_hosted(&s), "a built-in insert is not hosted");
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.x"));
        assert!(session_has_hosted(&s));
    }

    struct Spy {
        inner: Box<dyn Plugin>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Plugin for Spy {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.calls.lock().unwrap().push("reset");
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.calls.lock().unwrap().push("process");
            self.inner.process(io, frames);
        }
        fn all_notes_off(&mut self) {
            self.calls.lock().unwrap().push("all_notes_off");
        }
    }

    #[test]
    fn all_notes_off_reaches_the_instrument() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.spy"));
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, channel().0);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let spy = Spy { inner: soundcraft_dsp::create("gain").unwrap(), calls: Arc::clone(&calls) };
        let inst = PreparedInstance { track: t, slot: soundcraft_mix::INSTRUMENT_SLOT, id: "clap:test.spy".into(), ch: 2, plugin: Box::new(spy) };
        tx.send(Cmd::Session(Arc::new(s), vec![inst])).unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        calls.lock().unwrap().clear();
        tx.send(Cmd::AllNotesOff).unwrap();
        st.fill(&mut out, 2);
        {
            let calls = calls.lock().unwrap().clone();
            assert_eq!(calls.first().copied(), Some("all_notes_off"), "told to release first: {calls:?}");
            assert!(calls.contains(&"process"), "then processed so the release is delivered while stopped and unmonitored: {calls:?}");
            assert!(!calls.contains(&"reset"), "and not reset: {calls:?}");
        }
    }

    #[test]
    fn plugin_states_are_read_without_the_audio_thread() {
        let mut side = Side::new(48_000.0, channel().1);
        let (a, b) = (TrackId(1), TrackId(2));
        side.states.insert((b, 0), Box::new(|| Some(b"b0".to_vec())));
        side.states.insert((a, soundcraft_mix::INSTRUMENT_SLOT), Box::new(|| Some(b"synth".to_vec())));
        side.states.insert((a, 3), Box::new(|| Some(b"a3".to_vec())));
        side.states.insert((a, 4), Box::new(|| None));
        let want = vec![(a, 3, b"a3".to_vec()), (a, soundcraft_mix::INSTRUMENT_SLOT, b"synth".to_vec()), (b, 0, b"b0".to_vec())];
        assert_eq!(side.capture_states(), want);
        side.forget((a, 3));
        assert_eq!(side.capture_states(), want[1..]);
    }

    struct Window;

    impl PluginEditor for Window {
        fn open(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self) {}
        fn is_open(&self) -> bool {
            false
        }
        fn idle(&mut self) -> Vec<(String, f32)> {
            Vec::new()
        }
    }

    #[test]
    fn a_refused_state_is_kept_until_the_editor_opens() {
        let mut side = Side::new(48_000.0, channel().1);
        let k = (TrackId(1), 0);
        side.states.insert(k, Box::new(|| Some(b"defaults".to_vec())));
        side.editors.insert(k, Box::new(Window));
        side.refused.insert(k);
        assert!(side.capture_states().is_empty());
        side.open_editor(k).unwrap();
        assert_eq!(side.capture_states(), vec![(TrackId(1), 0, b"defaults".to_vec())]);
    }

    #[test]
    fn side_creates_each_missing_instance_once() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut side = Side::new(48_000.0, channel().1);
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("gain"));
        assert_eq!(side.prepare(&s).len(), 1, "the plugin is not installed; the built-in is made here too");
        assert_eq!(side.shadow.get(&(t, 0)), Some(&("clap:does.not.exist".to_string(), 1, false)));
        assert!(side.prepare(&s).is_empty());
        assert_eq!(side.shadow.len(), 2, "failures are remembered, not retried every update");
        s.track_mut(t).unwrap().mixer.inserts[0] = None;
        side.prepare(&s);
        assert_eq!(side.shadow.len(), 1, "only the built-in is left");
    }

    #[test]
    fn replaced_sessions_are_freed_on_the_ui_thread() {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, grx);
        side.return_old_sessions(&tx);
        let mut out = vec![0.0f32; 256];
        let mut watched = Vec::new();
        for _ in 0..3 * OLD_SESSIONS {
            let s = Arc::new(soundcraft_model::Session::default());
            watched.push(Arc::downgrade(&s));
            side.collect_garbage();
            tx.send(Cmd::Session(s, Vec::new())).unwrap();
            st.fill(&mut out, 2);
            let replaced = watched.len().checked_sub(2).and_then(|i| watched.get(i));
            assert!(replaced.is_none_or(|w| w.strong_count() == 1), "freed on the audio thread");
        }
        side.collect_garbage();
        assert_eq!(watched.iter().filter(|w| w.strong_count() > 0).count(), 1, "only the live snapshot is left");
    }

    #[test]
    fn looping_wraps() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: Some(Range::new(0, 1000)) }).unwrap();
        let mut out = vec![0.0f32; 20_000];
        st.fill(&mut out, 2);
        let p = shared.position.load(Ordering::Relaxed);
        assert!(p <= 1000, "position {p} should stay inside the loop");
        assert!(shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn locate_while_playing_drops_the_old_position_audio() {
        use soundcraft_model::{ChannelFormat, Clip, Insert, SourceAudio, SourceId, TrackKind};
        for (plugin, params) in [("compressor", &[("lookahead", 5.0), ("ratio", 1.0)][..]), ("room_reverb", &[][..])] {
            let mut s = soundcraft_model::Session::default();
            s.edit.click = false;
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            s.pool.insert(
                SourceId(5),
                Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 4800]] })),
            );
            let id = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 4800));
            let mut ins = Insert::new(plugin);
            for (k, v) in params {
                ins.params.insert((*k).into(), *v);
            }
            tr.mixer.inserts[0] = Some(ins);
            let shared = Arc::new(Shared {
                position: AtomicI64::new(0),
                playing: AtomicBool::new(false),
                speed: AtomicU32::new(1000),
                meters: Mutex::new(MeterSnapshot::default()),
                editors_open: AtomicBool::new(false),
            });
            let (tx, rx) = channel();
            let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, channel().0);
            tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
            let mut out = vec![0.0f32; 2 * 512];
            let mut before = 0.0f32;
            for _ in 0..9 {
                st.fill(&mut out, 2);
                before = out.iter().fold(before, |m, x| m.max(x.abs()));
            }
            assert!(before > 0.1, "{plugin}: the clip plays before the locate");
            tx.send(Cmd::Locate(1_000_000)).unwrap();
            for _ in 0..8 {
                st.fill(&mut out, 2);
                assert!(out.iter().all(|x| x.abs() < 1e-6), "{plugin}: audio from before the locate");
            }
        }
    }

    #[test]
    fn armed_tracks_are_heard_and_metered_after_playback_stops() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.record_arm = true;
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, channel().0);
        let ring = record::InputRing::new(1, 48_000);
        tx.send(Cmd::Input(Arc::clone(&ring))).unwrap();
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(!shared.playing.load(Ordering::Relaxed));
        // Two blocks of input for the two blocks asked (a longer backlog is trimmed to one).
        ring.push(&[0.5; 1024]);
        out.truncate(2048);
        st.fill(&mut out, 2);
        assert!(out.iter().any(|x| x.abs() > 0.1), "the input is heard");
        assert!(shared.meters.lock().unwrap().tracks.get(&t).is_some_and(|m| m.peak[0] > 0.1), "and metered");
    }

    #[test]
    fn only_asio_opens_the_input_with_the_output() {
        assert!(input_with_output("ASIO", || true), "ASIO with an input device: duplex up front");
        assert!(!input_with_output("ASIO", || false), "ASIO without inputs: output only");
        let asked = std::cell::Cell::new(false);
        let has_input = || {
            asked.set(true);
            true
        };
        assert!(!input_with_output("WASAPI", has_input), "WASAPI opens the input on demand");
        assert!(!asked.get(), "the device is not asked off ASIO");
        for other in ["CoreAudio", "ALSA", "JACK", "asio", ""] {
            assert!(!input_with_output(other, || true), "{other}");
        }
    }

    fn silent_player() -> LegacyPlayer {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            editors_open: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        LegacyPlayer::silent_clock(shared, tx, rx, 48_000, Mutex::new(Side::new(48_000.0, grx)), gtx)
    }

    #[test]
    fn absent_output_and_input_never_claim_observed_priority() {
        assert_eq!(silent_player().thread_priority(), scheduling::WorkerPriority::NoWorker);
        assert_eq!(record::Recorder::detached("no input").thread_priority(), scheduling::WorkerPriority::NoWorker);
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires real default WASAPI input/output; output is silent and input is not recorded"]
    fn wasapi_workers_publish_observed_priority() {
        let config = AudioConfig { system: Some("WASAPI".into()), ..AudioConfig::default() };
        for _ in 0..3 {
            let player = LegacyPlayer::with_config(Arc::new(Session::default()), &config);
            assert!(!player.silent, "requires an actual output device");
            let recorder = record::Recorder::open_with(&config).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while (player.callback_telemetry().callbacks < 2 || recorder.thread_priority() == scheduling::WorkerPriority::Pending)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(player.callback_telemetry().callbacks >= 2);
            assert_eq!(player.thread_priority(), scheduling::WorkerPriority::MmcssAudio);
            assert_eq!(recorder.thread_priority(), scheduling::WorkerPriority::MmcssAudio);
            let observed = match player._stream.as_ref().unwrap().as_inner() {
                cpal::platform::StreamInner::Wasapi(stream) => stream.thread_priority_observation(),
                #[cfg(feature = "asio")]
                cpal::platform::StreamInner::Asio(_) => panic!("test requested WASAPI"),
            };
            drop(recorder);
            drop(player);
            assert_eq!(observed.status(), cpal::platform::WasapiThreadPriorityStatus::Released);
        }
    }

    #[test]
    fn an_input_opened_with_the_output_is_reused_and_never_closed() {
        let player = silent_player();
        *player.input.lock().unwrap() = HeldInput::Held(Some(record::Recorder::detached("Duplex In")));
        let r = player.open_input(&AudioConfig::default()).unwrap();
        assert_eq!(r.device_name, "Duplex In", "the held input, not a new device");
        assert!(player.open_input(&AudioConfig::default()).is_err(), "it is out once");
        player.close_input(r);
        assert!(matches!(&*player.input.lock().unwrap(), HeldInput::Held(Some(_))), "closing gives it back, open");
        let again = player.open_input(&AudioConfig::default()).unwrap();
        assert_eq!(again.device_name, "Duplex In");
    }

    #[test]
    fn an_input_that_failed_with_the_output_reports_and_does_not_open() {
        let player = silent_player();
        *player.input.lock().unwrap() = HeldInput::Failed("no audio input device".into());
        assert_eq!(player.open_input(&AudioConfig::default()).err().as_deref(), Some("no audio input device"));
        // A recorder from elsewhere is closed, not kept.
        player.close_input(record::Recorder::detached("Other"));
        assert!(matches!(&*player.input.lock().unwrap(), HeldInput::Failed(_)));
    }

    #[test]
    fn without_a_duplex_input_closing_drops_the_recorder() {
        let player = silent_player();
        assert!(matches!(&*player.input.lock().unwrap(), HeldInput::No), "the silent clock opens inputs on demand");
        player.close_input(record::Recorder::detached("Mic"));
        assert!(matches!(&*player.input.lock().unwrap(), HeldInput::No));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod panic_cleanup_tests;
