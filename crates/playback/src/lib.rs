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
pub use player::{ActionResult, CountIn, PlayedNote, Player, PlayerCommand, PlayerError, PlayerNotice, StartFailure};

use soundcraft_mix::{MAX_CHANNELS, StripMeter, itu_stereo_fold};
use soundcraft_model::{MAX_DEVICE_CHANNELS, Session, TrackId};
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

/// Whether any track of `s` carries a hosted (third-party) plug-in. Allocation-free: runs on the audio thread.
fn session_has_hosted(s: &Session) -> bool {
    s.tracks.iter().any(|t| {
        t.instrument.as_ref().is_some_and(|i| soundcraft_mix::is_third_party(&i.plugin))
            || t.mixer.inserts.iter().flatten().any(|i| soundcraft_mix::is_third_party(&i.plugin))
    })
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
    timing: telemetry::Accumulator,
}

impl OutputCallback {
    fn new(channels: usize) -> Self {
        // Initialise the platform clock (including any frequency cache) before callbacks run.
        let _ = telemetry::Timer::start().elapsed();
        Self { scratch: vec![0.0; 8192 * channels.max(1)], channels: channels.max(1), timing: telemetry::Accumulator::default() }
    }

    /// One device callback scope, shared by all scratch conversion chunks; it must never be
    /// reconstructed inside a chunk.
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
