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
//! callback quiescence.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod action_lane;
pub mod backend;
pub mod device_clock;
pub mod devices;
pub mod midi_capture;
pub mod plan_bridge;
pub mod player;
pub mod publication;
pub mod record;
pub mod scheduling;
pub mod telemetry;
pub mod timeline_map;

#[cfg(test)]
pub(crate) use backend::cpal::OutputCallback;
pub use devices::AudioConfig;
pub use player::{ActionResult, CountIn, MidiTake, PlayedNote, Player, PlayerCommand, PlayerError, PlayerNotice, StartFailure};

use soundcraft_mix::{MAX_CHANNELS, StripMeter, itu_stereo_fold};
use soundcraft_model::{Session, TrackId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, Ordering};

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

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    /// Speed × 1000.
    speed: AtomicU32,
    /// A hosted plug-in's editor is open (set by the UI thread each frame): the engine keeps
    /// running while stopped, because such GUIs initialise and sync through `process` calls.
    editors_open: AtomicBool,
    metro: Metro,
}

/// The click's level and the record count-in, shared between Main and the audio thread. Plain
/// atomics: neither side allocates or waits.
struct Metro {
    /// Linear click trim, as `f32` bits.
    gain: AtomicU32,
    /// Project samples of the count-in still to play; 0 when none is running.
    count_in_left: AtomicI64,
    /// Length of the running count-in, in project samples.
    count_in_total: AtomicI64,
    /// Why the recording start at the end of a count-in failed (a [`StartFailure`] code); 0 when it
    /// did not. Main takes it with `swap`.
    start_failure: AtomicU8,
}

impl Default for Metro {
    fn default() -> Self {
        Self {
            gain: AtomicU32::new(1.0f32.to_bits()),
            count_in_left: AtomicI64::new(0),
            count_in_total: AtomicI64::new(0),
            start_failure: AtomicU8::new(0),
        }
    }
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

/// The input a player opened with its output (`Backend::capture_with_render`).
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

/// Whether any track of `s` carries a hosted (third-party) plug-in. Allocation-free: runs on the audio thread.
fn session_has_hosted(s: &Session) -> bool {
    s.tracks.iter().any(|t| {
        t.instrument.as_ref().is_some_and(|i| soundcraft_mix::is_third_party(&i.plugin))
            || t.mixer.inserts.iter().flatten().any(|i| soundcraft_mix::is_third_party(&i.plugin))
    })
}

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
