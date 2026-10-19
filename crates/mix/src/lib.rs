//! The SoundCraft mix engine.
//!
//! [`MixEngine::render`] produces one block of the main output for a session at a timeline
//! position: clips (with clip gain and fades) → trim/phase → inserts → pre-fader sends → fader and
//! mute (with automation) → pan → post-fader sends → busses → aux inputs → main → master faders.
//! The same code drives realtime playback and offline bounces, so what you hear is what you get.
//! Steady-state rendering does not allocate.
//!
//! ## Surround
//! The main mix has the format of the session's main output path ([`Session::main_format`]:
//! stereo by default, up to 16 channels such as 7.1.4 or 9.1.6) and every bus has its own format.
//! Channels are in SMPTE / WAV order (L R C LFE Ls Rs …, see
//! [`ChannelFormat::speakers`](soundcraft_model::ChannelFormat::speakers)). Routing a strip into a
//! destination builds a gain matrix per block:
//! - mono/stereo sources into a stereo destination use the stereo pan exactly as before;
//! - into a multichannel destination they use the track's [`SurroundPan`] (pairwise
//!   constant-power panner over the speaker layout, see `soundcraft_dsp::pan::surround_gains`),
//!   or go to L/R with the stereo pan when the track has no surround pan;
//! - sources whose format equals the destination's pass straight through, channel for channel;
//! - other multichannel sources fold to stereo with ITU-R BS.775 coefficients (C and surrounds at
//!   -3 dB, LFE dropped), or map speaker by speaker into other multichannel layouts.
//!
//! ## Plugin instances and threads
//! [`MixEngine::sync`] brings plugin instances in line with the session. Hosted third-party
//! plugins (`clap:` / `vst3:` ids, see [`is_third_party`]) are expensive and have main-thread
//! rules; built-in plugins only allocate, but allocating is not allowed on the audio thread
//! either. So a realtime host makes every instance off the audio thread:
//! - with [`MixEngine::set_external_instances`]`(true)` (realtime playback), `sync` never creates a
//!   plugin, built-in or hosted, insert, instrument or clip effect. The host's control (UI) thread
//!   creates them with [`create_instance`] from [`realtime_instance_specs`] (restoring each hosted
//!   insert's stored [`Insert::state`] and calling `prepare` there), and hands them over with
//!   [`MixEngine::adopt`]; `sync` then moves the matching one into its slot. A slot whose
//!   instance has not arrived yet passes audio through. ([`instance_specs`] lists the hosted
//!   instances only.)
//! - every instance `sync` replaces or removes (built-in or not) goes to a retired list instead of
//!   being dropped; the host takes it with [`MixEngine::take_retired`] and drops it on the control
//!   thread, so plugin destruction never runs on the audio thread either.
//! - without external instances (the default: offline [`render_range`], bounces, tests) `sync`
//!   creates every plugin itself, synchronously (a clip effect when its clip first plays),
//!   restores stored state before the first `process`, and drops retired instances at the end of
//!   `sync`.
//!
//! An instance starts from its stored state; the insert's parameter values are then applied on
//! top before the first block (the session's values are kept in step with the plugin, including
//! edits made in its own editor, so both agree; the state carries everything else). A host reads
//! states back for saving through each instance's [`Plugin::state_reader`], on its UI thread.
//!
//! A built-in instrument's parameter values are applied the same way. A sample-playing
//! instrument (the Sampler) is handed the decoded audio of its insert's
//! [`Insert::sample`] source straight from the session's pool, shared and never copied.
//!
//! ## Prepared plans (realtime, F01b)
//! An engine made with [`MixEngine::realtime`] never discovers structure on the audio thread.
//! A [`PlanCompiler`] on the control thread turns each Session into a [`PlanEnvelope`] whose
//! tables (strips, plug-in slots, clip effects, busses, main mix, delay lines, parameter
//! storage) are sized and bounded by [`PlanLimits`]. [`MixEngine::adopt_plan`] validates it
//! without changing anything, then only swaps: reused cells (a strip's running state, plug-in
//! slots) move into the new tables, and everything the old plan owned goes back in the same
//! envelope, to be dropped on the control thread at [`PlanCompiler::reclaim`].
//! [`MixEngine::render_active`] renders the active plan. The `adopt`/`sync`/`render` path
//! described above is unchanged and still drives offline renders and today's playback.
//!
//! [`Insert::state`]: soundcraft_model::Insert::state
//! [`Insert::sample`]: soundcraft_model::Insert::sample
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use soundcraft_dsp::pan::{PanLaw, SpeakerPos, SurroundParams, gains};
use soundcraft_dsp::{Plugin, ProcessStatus, SampleSource, Transport, db_to_gain};
use soundcraft_midi::CtrlKind;
use soundcraft_model::{
    AutoParam, BusId, ChannelFormat, Clip, ClipContent, Route, Session, SourceAudio, Speaker, SurroundPan, Track, TrackId, TrackKind,
};
use soundcraft_time::{Range, Samples, TICKS_PER_QUARTER};
use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Arc;

mod active;
mod compile;
mod plan;
#[cfg(test)]
mod plan_tests;

pub use compile::{CompileError, MIN_DELAY_CAPACITY, PlanCompiler, PlanLimits};
pub use plan::{
    Adoption, DeviceGeneration, EnvelopeState, PlanEnvelope, PlanGeneration, PlanIdentity, PlanStatus, PreparedPlan, RejectReason, Rejected,
    RenderTarget, Retired, SlotGen,
};

/// Default (stereo) main mix width. The actual width is [`main_channels`].
pub const MAIN_CHANNELS: usize = 2;

/// Widest strip, bus or main mix (9.1.6 / 3rd-order Ambisonics). Wider formats are truncated.
pub const MAX_CHANNELS: usize = 16;

/// Number of channels of a session's main mix.
pub fn main_channels(s: &Session) -> usize {
    s.main_format().channels().clamp(MAIN_CHANNELS, MAX_CHANNELS)
}

/// Slot index used for a track's instrument plugin in [`InstanceSpec`] and [`PreparedInstance`]
/// (insert slots use their index).
pub const INSTRUMENT_SLOT: usize = usize::MAX;

/// Hosted third-party plugin ids (CLAP or VST3): created off the audio thread in realtime use.
pub fn is_third_party(id: &str) -> bool {
    id.starts_with(soundcraft_clap_host::ID_PREFIX)
        || id.starts_with(soundcraft_vst3_host::ID_PREFIX)
        || id.starts_with(soundcraft_au_host::ID_PREFIX)
}

/// Top bit of a clip-effect slot index (see [`clip_fx_slot`]). Insert slots are small, and
/// [`INSTRUMENT_SLOT`] is never a clip-effect slot.
const CLIP_FX_BIT: usize = 1 << (usize::BITS - 1);
/// Built-in plugins a clip's effect chain is made of (Clip Effects).
const CLIP_EQ: &str = "eq_7band";
const CLIP_COMP: &str = "compressor";

/// Slot index of a clip's effect module in [`InstanceSpec`] and [`PreparedInstance`]: its EQ
/// (`comp` false) or its compressor. `None` for a clip id too large to encode (not made by
/// [`Session::new_clip_id`] in practice); such a clip plays without its effects in realtime.
#[must_use]
pub fn clip_fx_slot(clip: u64, comp: bool) -> Option<usize> {
    let clip = usize::try_from(clip).ok().filter(|c| *c < (1 << (usize::BITS - 2)) - 1)?;
    Some(CLIP_FX_BIT | (clip << 1) | usize::from(comp))
}

/// The clip and module ([`clip_fx_slot`]) a slot index names, if it is a clip-effect slot.
fn clip_fx_of_slot(slot: usize) -> Option<(u64, bool)> {
    if slot == INSTRUMENT_SLOT || slot & CLIP_FX_BIT == 0 {
        return None;
    }
    Some((((slot & !CLIP_FX_BIT) >> 1) as u64, slot & 1 == 1))
}

/// Which clip-effect modules (EQ, compressor) a clip's current parameters need.
fn clip_fx_modules(s: &Session, clip: u64) -> (bool, bool) {
    clip_fx_params(s, clip).map_or((false, false), |p| (p.clone().any(|(k, _)| k.starts_with("eq.")), p.clone().any(|(k, _)| k.starts_with("comp."))))
}

/// A plugin instance a session needs: which slot, which plugin, how many channels, and the state
/// to restore.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceSpec {
    pub track: TrackId,
    /// Insert slot index, [`INSTRUMENT_SLOT`], or a clip-effect slot ([`clip_fx_slot`]).
    pub slot: usize,
    pub id: String,
    /// Channel count the instance is prepared for.
    pub ch: usize,
    /// Decoded [`Insert::state`](soundcraft_model::Insert::state).
    pub state: Option<Vec<u8>>,
}

/// A plugin instance created and prepared off the audio thread, handed to [`MixEngine::adopt`].
pub struct PreparedInstance {
    pub track: TrackId,
    pub slot: usize,
    pub id: String,
    pub ch: usize,
    pub plugin: Box<dyn Plugin>,
}

/// Every third-party plugin slot (inserts and instruments) of a session, with the channel count
/// [`MixEngine::sync`] prepares it for. Inactive and master tracks are included (they have strips).
pub fn instance_specs(s: &Session) -> Vec<InstanceSpec> {
    let mut out = realtime_instance_specs(s);
    out.retain(|x| is_third_party(&x.id));
    out
}

/// Every plugin instance a realtime engine ([`MixEngine::set_external_instances`]) takes from
/// its host: each insert and instrument, built-in or third-party, and each clip-effect module
/// (an EQ or compressor per clip, [`clip_fx_slot`]) of an audio track, with the channel count
/// [`MixEngine::sync`] prepares it for. Only third-party plugins carry a state to restore.
/// Inactive and master tracks are included (they have strips).
pub fn realtime_instance_specs(s: &Session) -> Vec<InstanceSpec> {
    let main_ch = main_channels(s);
    let state = |ins: &soundcraft_model::Insert| if is_third_party(&ins.plugin) { ins.state_bytes() } else { None };
    let mut out = Vec::new();
    for t in &s.tracks {
        let ch = strip_channels(t, main_ch);
        for (i, ins) in t.mixer.inserts.iter().enumerate() {
            if let Some(ins) = ins {
                out.push(InstanceSpec { track: t.id, slot: i, id: ins.plugin.clone(), ch, state: state(ins) });
            }
        }
        if let Some(ins) = &t.instrument {
            out.push(InstanceSpec { track: t.id, slot: INSTRUMENT_SLOT, id: ins.plugin.clone(), ch: ch.max(2), state: state(ins) });
        }
        if t.kind == TrackKind::Audio {
            for clip in t.clips().iter().filter(|c| c.is_audio()) {
                let (eq, comp) = clip_fx_modules(s, clip.id.0);
                for (want, is_comp, id) in [(eq, false, CLIP_EQ), (comp, true, CLIP_COMP)] {
                    if let Some(slot) = clip_fx_slot(clip.id.0, is_comp).filter(|_| want) {
                        out.push(InstanceSpec { track: t.id, slot, id: id.to_string(), ch, state: None });
                    }
                }
            }
        }
    }
    out
}

/// A plugin made by [`create_instance`].
pub struct NewInstance {
    pub plugin: Box<dyn Plugin>,
    /// The plugin refused the stored state and starts from its defaults.
    pub refused_state: bool,
}

/// Creates a plugin (built-in or hosted), restores `state` (if any) and prepares it. Expensive
/// for third-party plugins: in realtime use, call it on the control thread.
pub fn create_instance(id: &str, state: Option<&[u8]>, sample_rate: f32, max_block: usize, ch: usize) -> Option<NewInstance> {
    let mut plugin = create_plugin(id)?;
    let refused_state = state.is_some_and(|st| !plugin.load_state(st));
    if refused_state {
        log::warn!("{id}: the plugin did not accept its stored state");
    }
    plugin.prepare(sample_rate, max_block, ch);
    Some(NewInstance { plugin, refused_state })
}

/// Peak meter values for one strip (linear, per channel, max over the last block).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StripMeter {
    /// First two channels (mono strips repeat channel 0), for stereo meters.
    pub peak: [f32; 2],
    /// Every channel of the strip (or of the main mix).
    pub peaks: Vec<f32>,
    /// Gain reduction of the first dynamics insert, dB.
    pub gain_reduction_db: f32,
    pub plugin_failures: PluginFailures,
}

/// How often a strip's plugins were taken out of the signal path for putting out NaN or infinity.
/// Each stays out until the engine is next reset (playback starts or stops).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PluginFailures {
    /// It only grows.
    pub count: u32,
    /// The slot of the latest: an insert index or [`INSTRUMENT_SLOT`].
    pub last: usize,
}

impl PluginFailures {
    fn record(&mut self, slot: usize) {
        self.count = self.count.wrapping_add(1);
        self.last = slot;
    }
}

impl StripMeter {
    /// Copy another meter without reallocating once the channel count is stable.
    pub fn copy_from(&mut self, o: &StripMeter) {
        self.peak = o.peak;
        self.peaks.clone_from(&o.peaks);
        self.gain_reduction_db = o.gain_reduction_db;
        self.plugin_failures = o.plugin_failures;
    }

    fn measure(&mut self, bufs: &[Vec<f32>], frames: usize, gr: f32) {
        if self.peaks.len() != bufs.len() {
            self.peaks.resize(bufs.len(), 0.0);
        }
        for (p, b) in self.peaks.iter_mut().zip(bufs.iter()) {
            *p = peak(b.get(..frames).unwrap_or(&[]));
        }
        let last = self.peaks.len().saturating_sub(1);
        self.peak = [self.peaks.first().copied().unwrap_or(0.0), self.peaks.get(1.min(last)).copied().unwrap_or(0.0)];
        self.gain_reduction_db = gr;
    }
}

/// A bus: its format and one buffer per channel.
struct BusBuf {
    fmt: ChannelFormat,
    buf: Vec<Vec<f32>>,
}

/// Where strips find the busses they read and write: a map in legacy engines, a table sorted
/// by id in plan mode.
trait Busses {
    fn bus(&self, id: BusId) -> Option<&BusBuf>;
    fn bus_mut(&mut self, id: BusId) -> Option<&mut BusBuf>;
}

impl Busses for HashMap<BusId, BusBuf> {
    fn bus(&self, id: BusId) -> Option<&BusBuf> {
        self.get(&id)
    }
    fn bus_mut(&mut self, id: BusId) -> Option<&mut BusBuf> {
        self.get_mut(&id)
    }
}

/// Plan-mode busses, sorted by id.
#[derive(Default)]
struct BusTable(Vec<(BusId, BusBuf)>);

impl Busses for BusTable {
    fn bus(&self, id: BusId) -> Option<&BusBuf> {
        let i = self.0.binary_search_by_key(&id, |(b, _)| *b).ok()?;
        self.0.get(i).map(|(_, b)| b)
    }
    fn bus_mut(&mut self, id: BusId) -> Option<&mut BusBuf> {
        let i = self.0.binary_search_by_key(&id, |(b, _)| *b).ok()?;
        self.0.get_mut(i).map(|(_, b)| b)
    }
}

/// Parameter maps up to this long are tracked inline in a [`PluginSlot`] (every built-in's is).
const APPLIED_INLINE: usize = 64;

/// The parameter values last pushed to a plugin, in the insert's parameter-map order. Up to
/// [`APPLIED_INLINE`] values live inline, so a slot filled on the audio thread allocates nothing
/// for them; a longer map (a hosted plugin's edited parameters) spills to the heap once.
struct Applied {
    len: usize,
    inline: [f32; APPLIED_INLINE],
    heap: Vec<f32>,
    /// Plan mode: `heap` is prepared off the audio thread (see `Attach::Applied`) and never
    /// resized here. A map whose length does not match it pushes the values that fit.
    fixed: bool,
    /// Plan mode: how often a map did not match the prepared storage.
    mismatch: u32,
}

impl Applied {
    const fn new() -> Self {
        Applied { len: 0, inline: [f32::NAN; APPLIED_INLINE], heap: Vec::new(), fixed: false, mismatch: 0 }
    }

    /// Plan mode: `heap` is the prepared storage (empty for a map that fits inline).
    fn prepared(heap: Vec<f32>) -> Self {
        Applied { len: 0, inline: [f32::NAN; APPLIED_INLINE], heap, fixed: true, mismatch: 0 }
    }

    /// The `n` values, all NaN ("never pushed") when the map's length changed. In plan mode a
    /// long map whose length differs from the prepared storage gets `min(n, storage)` values.
    fn values(&mut self, n: usize) -> &mut [f32] {
        if n != self.len {
            self.len = n;
            if n <= APPLIED_INLINE {
                self.inline.fill(f32::NAN);
            } else if self.fixed {
                self.heap.fill(f32::NAN);
                if self.heap.len() != n {
                    self.mismatch = self.mismatch.saturating_add(1);
                }
            } else {
                self.heap = vec![f32::NAN; n];
            }
        }
        if n <= APPLIED_INLINE { self.inline.get_mut(..n).unwrap_or_default() } else { &mut self.heap }
    }
}

struct PluginSlot {
    id: String,
    /// Channel count the plugin was prepared for.
    ch: usize,
    plugin: Box<dyn Plugin>,
    /// Last values pushed to the plugin, in the insert's parameter-map order.
    applied: Applied,
    failed: bool,
    /// A hosted plugin that has not had its first parameter pass yet (see `apply_values`).
    fresh: bool,
    /// A hosted (third-party) effect insert: it may be left unprocessed while its input is
    /// silent and its [`ProcessStatus`] allows it. Never set for instruments or built-ins.
    suspendable: bool,
    /// Samples of silent input the plugin has processed since its input was last not silent
    /// (how far into its tail it is).
    quiet: usize,
    /// The block the plugin last processed came out silent (true before the first). A plugin
    /// whose output still rings is not suspended.
    output_silent: bool,
    /// A parameter was sent to the plugin since it last processed a block. Its next block is
    /// processed even while suspended, so the queued change reaches it.
    woken: bool,
    /// Plan mode: the slot's identity across plans ([`SlotGen::VACANT`] in legacy engines).
    generation: SlotGen,
    /// Plan mode: channel-format code of the strip it was prepared for.
    layout: u16,
    /// Plan mode: the pool audio and the wrapper handed to the instrument with `set_sample`. Both
    /// owners stay here, so the plug-in never drops the last one.
    sample_audio: Option<Arc<SourceAudio>>,
    sample_wrapper: Option<Arc<dyn SampleSource>>,
}

impl PluginSlot {
    /// A slot for a new instance in slot `slot` ([`INSTRUMENT_SLOT`], an insert index or a
    /// [`clip_fx_slot`]). Hosted plug-ins start `fresh`; hosted effect inserts are suspendable.
    fn new(plugin: Box<dyn Plugin>, id: String, ch: usize, slot: usize) -> Self {
        let third = is_third_party(&id);
        PluginSlot {
            id,
            ch,
            plugin,
            applied: Applied::new(),
            failed: false,
            fresh: third,
            suspendable: third && slot != INSTRUMENT_SLOT && clip_fx_of_slot(slot).is_none(),
            quiet: 0,
            output_silent: true,
            woken: false,
            generation: SlotGen::VACANT,
            layout: 0,
            sample_audio: None,
            sample_wrapper: None,
        }
    }
}

struct Strip {
    buf: Vec<Vec<f32>>,
    /// Pre-fader copy for pre-fader sends.
    pre: Vec<Vec<f32>>,
    scratch: Vec<f32>,
    plugins: Vec<Option<PluginSlot>>,
    instrument: Option<PluginSlot>,
    /// The pool audio handed to the instrument with `set_sample` (sample-playing instruments).
    sample: Option<Arc<SourceAudio>>,
    /// MIDI notes currently sounding on the instrument (pitch), for note-offs at stops/seek.
    held: Vec<u8>,
    /// The instrument's next block starts somewhere new: send it the controllers in effect there.
    chase: bool,
    muted: bool,
    gr: f32,
    /// Per-clip effect chains (Clip Effects) and a scratch buffer to render clips into.
    clip_fx: ClipFxTable,
    clipbuf: Vec<Vec<f32>>,
    /// The engine takes its instances from the host ([`MixEngine::set_external_instances`]):
    /// clip-effect modules come from `sync`, never from the render.
    external: bool,
    /// Total latency of the active inserts (samples).
    latency: usize,
    /// Delay compensation: align this strip with the slowest one (post- and pre-fader paths),
    /// plus extra delay for direct-to-main outputs so they line up with aux returns.
    align: DelayLine,
    align_pre: DelayLine,
    out_extra: DelayLine,
    outbuf: Vec<Vec<f32>>,
    failures: PluginFailures,
    /// Plan mode: the strip's identity across plans. [`SlotGen::VACANT`] in a strip shell that
    /// waits for a move, in a strip whose state moved out, and in legacy engines.
    generation: SlotGen,
    /// Plan mode: index of the strip's track in the plan's session.
    track: u16,
    /// Plan mode: the track's id.
    id: TrackId,
    /// Plan mode: channel-format code of `buf`.
    layout: u16,
    /// Plan mode: the strip's meter (legacy engines keep theirs in [`MixEngine::meters`]).
    meter: StripMeter,
}

/// Most latency delay compensation handles: 2^20 samples, about 22 s at 48 kHz. A strip reports
/// at most this, whatever its plugins claim, so a bogus value can't stall offline renders.
const MAX_LATENCY: usize = 1 << 20;

/// A multichannel delay line (samples), resized only when the delay changes.
#[derive(Default)]
struct DelayLine {
    bufs: Vec<Vec<f32>>,
    idx: usize,
    len: usize,
    /// Plan mode: `bufs` were sized off the audio thread to this declared capacity and are never
    /// resized; a longer delay is clamped to it.
    cap: Option<usize>,
}

impl DelayLine {
    /// A plan-mode delay line with `cap` samples of storage per channel.
    fn with_capacity(ch: usize, cap: usize) -> Self {
        DelayLine { bufs: vec![vec![0.0; cap]; ch], idx: 0, len: 0, cap: Some(cap) }
    }

    /// Sets the delay. Returns the delay asked for when a plan-mode line had to clamp it to its
    /// capacity, else 0.
    fn set(&mut self, len: usize, ch: usize) -> usize {
        let len = len.min(MAX_LATENCY);
        if let Some(cap) = self.cap {
            let fit = len.min(cap);
            if fit != self.len {
                self.len = fit;
                self.idx = 0;
                for b in &mut self.bufs {
                    b.iter_mut().take(fit).for_each(|x| *x = 0.0);
                }
            }
            return if len > cap { len } else { 0 };
        }
        if len != self.len || self.bufs.len() != ch {
            self.len = len;
            self.idx = 0;
            self.bufs = vec![vec![0.0; len]; ch];
        }
        0
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if self.len == 0 {
            return;
        }
        let start = self.idx;
        for (c, ch) in io.iter_mut().enumerate() {
            let Some(ring) = self.bufs.get_mut(c) else { continue };
            let mut i = start;
            for x in ch.iter_mut().take(frames) {
                if let Some(r) = ring.get_mut(i) {
                    std::mem::swap(r, x);
                }
                i += 1;
                if i >= self.len {
                    i = 0;
                }
            }
        }
        self.idx = (start + frames) % self.len.max(1);
    }
    fn clear(&mut self) {
        let len = self.len;
        self.bufs.iter_mut().for_each(|b| b.iter_mut().take(len).for_each(|x| *x = 0.0));
    }
}

/// Clip Effects: an EQ and a compressor per clip, configured from `edit.values`
/// (`clip_fx.<clip>.eq.<param>`, `clip_fx.<clip>.comp.<param>`, `clip_fx.<clip>.gain`).
struct ClipFx {
    eq: Option<ClipModule>,
    comp: Option<ClipModule>,
    /// The clip's parameters use the module this block. A realtime engine keeps a module whose
    /// parameters went away (it is not dropped on the audio thread) until `sync` retires it.
    eq_on: bool,
    comp_on: bool,
    gain_db: f32,
    sr: f32,
    ch: usize,
    /// Offline engines: modules are made when a clip's parameters first need them and dropped
    /// when they no longer do. Realtime engines get them from the host through `sync`.
    lazy: bool,
}

/// A clip-effect module, in a slot like an insert's: the plugin id it came with from the host
/// stays with it, so that string is not freed on the audio thread when the module arrives.
type ClipModule = PluginSlot;

/// A strip's clip-effect chains, sorted by clip id, so a lookup neither hashes nor allocates.
#[derive(Default)]
struct ClipFxTable(Vec<(u64, ClipFx)>);

impl ClipFxTable {
    fn get_mut(&mut self, clip: u64) -> Option<&mut ClipFx> {
        let i = self.0.binary_search_by_key(&clip, |(c, _)| *c).ok()?;
        self.0.get_mut(i).map(|(_, fx)| fx)
    }

    /// The chain of `clip`, made with `make` when there is none (may allocate: not in plan mode).
    fn get_or_insert_with(&mut self, clip: u64, make: impl FnOnce() -> ClipFx) -> &mut ClipFx {
        let i = match self.0.binary_search_by_key(&clip, |(c, _)| *c) {
            Ok(i) => i,
            Err(i) => {
                self.0.insert(i, (clip, make()));
                i
            }
        };
        let (_, fx) = &mut self.0[i];
        fx
    }

    fn retain(&mut self, mut keep: impl FnMut(u64, &mut ClipFx) -> bool) {
        self.0.retain_mut(|(c, fx)| keep(*c, fx));
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut ClipFx> {
        self.0.iter_mut().map(|(_, fx)| fx)
    }
}

/// Room for this many clip-effect chains per audio strip is reserved when the strip is made, so
/// the first ones added during playback do not grow the table on the audio thread.
const CLIP_FX_RESERVE: usize = 16;

impl Strip {
    fn new() -> Self {
        Strip {
            buf: Vec::new(),
            pre: Vec::new(),
            scratch: Vec::new(),
            plugins: Vec::new(),
            instrument: None,
            sample: None,
            held: Vec::new(),
            chase: true,
            muted: false,
            gr: 0.0,
            clip_fx: ClipFxTable::default(),
            clipbuf: Vec::new(),
            external: false,
            latency: 0,
            align: DelayLine::default(),
            align_pre: DelayLine::default(),
            out_extra: DelayLine::default(),
            outbuf: Vec::new(),
            failures: PluginFailures::default(),
            generation: SlotGen::VACANT,
            track: 0,
            id: TrackId(0),
            layout: 0,
            meter: StripMeter::default(),
        }
    }
}

/// Renders sessions. Keep one per playback stream; plugin state persists across blocks.
pub struct MixEngine {
    sample_rate: f32,
    max_block: usize,
    strips: HashMap<TrackId, Strip>,
    busses: HashMap<BusId, BusBuf>,
    main: Vec<Vec<f32>>,
    main_fmt: ChannelFormat,
    pub meters: HashMap<TrackId, StripMeter>,
    pub main_meter: StripMeter,
    pub pan_law: PanLaw,
    /// Last rendered position end (to detect seeks).
    last_end: Samples,
    /// Snapshot key of the session the strips/order were synced to.
    synced: (usize, usize, usize),
    order: Vec<usize>,
    /// Current total compensation delay (samples) of the main mix: the slowest track and aux
    /// returns, and the master faders' inserts.
    pub latency: usize,
    /// Delay already applied to audio written onto busses: the slowest track insert.
    /// A bus render drops this so the bus lines up with the timeline. The extra delay
    /// that lines direct outputs up with aux returns stays in `latency` and is removed
    /// only from the main mix.
    track_latency: usize,
    /// Busses an aux returns onto, and the latency of the slowest aux return: audio written onto
    /// such a bus waits for it, so a bounce of the bus drops it as well.
    aux_fed: Vec<BusId>,
    aux_latency: usize,
    /// Live input for the next block (planar), set by the audio host for input monitoring.
    pub input: Vec<Vec<f32>>,
    /// Transport stopped: only monitored inputs sound (no clips).
    pub monitor_only: bool,
    /// Recording: record-armed tracks hear their input (auto input).
    pub recording: bool,
    /// Never leave a hosted effect unprocessed for silent input: every active insert runs every
    /// block. The host sets this while any plug-in editor is open, so an editor never freezes.
    pub no_suspend: bool,
    /// Mix the metronome (Options > Click) into the output. Playback turns this on; offline renders
    /// (bounces, stems, exports) leave it off so the click is never printed into a file.
    pub metronome: bool,
    /// Every plugin instance comes from [`MixEngine::adopt`] instead of being created in `sync`.
    external: bool,
    /// Instances handed over by the host, waiting for `sync` to place them (at most one per
    /// slot). The buffer is the one the host sent; `adopt` hands the previous one back.
    adopt: Vec<PreparedInstance>,
    /// Instances `sync` took out of service, for the host to drop (see [`MixEngine::take_retired`]).
    retired: Vec<Box<dyn Plugin>>,
    /// Scratch for `processing_order` (aux depths), reused so a re-sync does not allocate.
    order_depth: Vec<usize>,
    /// Ownership workspace reused for parallel strip processing.
    work: Vec<(usize, Strip)>,
    /// Render every strip on the calling thread (tests compare it with the parallel render).
    serial: bool,
    /// Plan mode ([`MixEngine::realtime`]): the active plan [`MixEngine::render_active`] renders.
    plan: Option<Box<plan::ActivePlan>>,
}

/// What `sync` needs to fill a slot.
struct SlotCtx<'a> {
    sr: f32,
    mb: usize,
    external: bool,
    adopt: &'a mut Vec<PreparedInstance>,
    retired: &'a mut Vec<Box<dyn Plugin>>,
}

impl SlotCtx<'_> {
    fn retire(&mut self, slot: Option<PluginSlot>) {
        if let Some(s) = slot {
            self.retired.push(s.plugin);
        }
    }

    fn retire_module(&mut self, module: Option<ClipModule>) {
        self.retire(module);
    }

    fn waiting_at(&self, t: TrackId, idx: usize) -> Option<usize> {
        self.adopt.iter().position(|p| p.track == t && p.slot == idx)
    }

    /// True when the host handed over an instance for this slot. It replaces whatever the slot
    /// holds, even an instance of the same plugin: the host made it because it no longer tracks
    /// the old one (it was removed and re-added between two renders), and drives its editor.
    fn waiting(&self, t: TrackId, idx: usize) -> bool {
        self.external && self.waiting_at(t, idx).is_some()
    }

    /// The next clip-effect module the host handed over for track `t`: its clip, whether it is
    /// the compressor (else the EQ), and the instance.
    fn take_clip_module(&mut self, t: TrackId) -> Option<(u64, bool, PreparedInstance)> {
        let i = self.adopt.iter().position(|p| p.track == t && clip_fx_of_slot(p.slot).is_some())?;
        let p = self.adopt.swap_remove(i);
        let (clip, comp) = clip_fx_of_slot(p.slot)?;
        Some((clip, comp, p))
    }

    /// A new instance for `ins` on track `t`, slot `idx`, prepared for `ch` channels: the one
    /// the host handed over (realtime), or one made here (offline).
    fn make(&mut self, t: TrackId, idx: usize, ins: &soundcraft_model::Insert, ch: usize) -> Option<PluginSlot> {
        let third = is_third_party(&ins.plugin);
        let (plugin, id) = if self.external {
            let p = self.waiting_at(t, idx).map(|i| self.adopt.swap_remove(i))?;
            if p.id != ins.plugin || p.ch != ch {
                self.retired.push(p.plugin);
                return None;
            }
            // The host's id string moves into the slot: nothing is copied or freed here.
            (p.plugin, p.id)
        } else {
            let state = if third { ins.state_bytes() } else { None };
            (create_instance(&ins.plugin, state.as_deref(), self.sr, self.mb, ch)?.plugin, ins.plugin.clone())
        };
        Some(PluginSlot::new(plugin, id, ch, idx))
    }
}

impl MixEngine {
    /// Force (or stop forcing) every engine in the process to render its audio-thread strips
    /// serially on the audio thread, never on the render pool. Serial is the default (see
    /// [`RENDER_POOL_ENV`]); `force_serial(false)` hands the strips to the pool. Takes effect at
    /// the next block. Offline renders are not affected.
    pub fn force_serial(on: bool) {
        FORCE_SERIAL.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// True while serial rendering is in effect: the default, until [`MixEngine::force_serial`]`(false)`
    /// or [`RENDER_POOL_ENV`] turns the render pool on.
    #[must_use]
    pub fn serial_forced() -> bool {
        FORCE_SERIAL.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Also spawns the process's render pool on first use (never from the audio thread).
    pub fn new(sample_rate: f32, max_block: usize) -> Self {
        init_render_pool();
        let max_block = max_block.clamp(16, 16_384);
        MixEngine {
            sample_rate: if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 48_000.0 },
            max_block,
            strips: HashMap::new(),
            busses: HashMap::new(),
            main: vec![vec![0.0; max_block]; MAIN_CHANNELS],
            main_fmt: ChannelFormat::Stereo,
            meters: HashMap::new(),
            main_meter: StripMeter::default(),
            pan_law: PanLaw::Minus3,
            last_end: 0,
            synced: (0, 0, 0),
            order: Vec::new(),
            latency: 0,
            track_latency: 0,
            aux_fed: Vec::new(),
            aux_latency: 0,
            input: Vec::new(),
            monitor_only: false,
            recording: false,
            no_suspend: false,
            metronome: false,
            external: false,
            adopt: Vec::new(),
            retired: Vec::new(),
            order_depth: Vec::new(),
            work: Vec::new(),
            serial: false,
            plan: None,
        }
    }

    /// Every strip: a legacy engine's, and the active plan's.
    fn all_strips(&mut self) -> impl Iterator<Item = &mut Strip> {
        self.strips.values_mut().chain(self.plan.iter_mut().flat_map(|p| p.tables.strips.iter_mut()))
    }

    /// Realtime mode: every plugin (built-in or third-party, inserts, instruments and clip
    /// effects) is created by the host and handed over with [`MixEngine::adopt`] instead of being
    /// created in `sync` (see the crate docs and [`realtime_instance_specs`]).
    pub fn set_external_instances(&mut self, on: bool) {
        self.external = on;
    }

    /// Takes instances created off the audio thread; the next render's `sync` places them.
    /// An earlier instance waiting for the same slot is retired.
    ///
    /// When everything sent before has been placed (the usual case), the engine keeps the
    /// host's vector as its waiting list and hands the previous, emptied one back through
    /// [`MixEngine::take_retired`], so a realtime caller neither allocates nor frees here.
    pub fn adopt(&mut self, mut instances: Vec<PreparedInstance>) {
        // Force a re-sync so waiting slots pick their instance up.
        self.synced = (usize::MAX, usize::MAX, usize::MAX);
        if self.adopt.is_empty() {
            std::mem::swap(&mut self.adopt, &mut instances);
        } else {
            // Two hand-overs before a sync: merge (this may allocate; F01b's plan protocol
            // coalesces on the control side instead).
            for p in instances.drain(..) {
                match self.adopt.iter_mut().find(|q| q.track == p.track && q.slot == p.slot) {
                    Some(q) => {
                        let old = std::mem::replace(q, p);
                        self.retired.push(old.plugin);
                    }
                    None => self.adopt.push(p),
                }
            }
        }
        self.hand_back(instances);
    }

    /// Returns an emptied hand-over vector to the host inside the retired list, so its memory is
    /// freed on the host's thread. The vector is converted in place (a `PreparedInstance` is a
    /// whole multiple of a `Box<dyn Plugin>` in size and alignment): nothing is allocated.
    fn hand_back(&mut self, spent: Vec<PreparedInstance>) {
        const {
            assert!(size_of::<PreparedInstance>().is_multiple_of(size_of::<Box<dyn Plugin>>()));
            assert!(align_of::<PreparedInstance>() == align_of::<Box<dyn Plugin>>());
        }
        if spent.capacity() == 0 {
            return;
        }
        let buf: Vec<Box<dyn Plugin>> = spent.into_iter().map(|p| p.plugin).collect();
        if self.retired.capacity() == 0 {
            self.retired = buf;
        }
        // Else the host has not taken the previous list yet (two hand-overs in one callback):
        // `buf` is freed here. The plan protocol (F01b) returns every buffer instead.
    }

    /// Instances taken out of service since the last call (drop them off the audio thread).
    /// Does not allocate. The vector can be empty and still hold memory (a hand-over vector
    /// [`MixEngine::adopt`] gives back): a realtime host sends it off the audio thread whenever
    /// its capacity is not zero, not only when it has elements.
    pub fn take_retired(&mut self) -> Vec<Box<dyn Plugin>> {
        std::mem::take(&mut self.retired)
    }

    /// Every instance this engine holds (strips, instruments, clip effects) plus the retired and
    /// waiting ones, leaving the engine empty: hand them to another thread to drop before
    /// replacing the engine.
    pub fn drain_plugins(&mut self) -> Vec<Box<dyn Plugin>> {
        let mut out = std::mem::take(&mut self.retired);
        for st in self.strips.values_mut() {
            out.extend(st.plugins.iter_mut().filter_map(Option::take).map(|s| s.plugin));
            out.extend(st.instrument.take().map(|s| s.plugin));
            for fx in st.clip_fx.values_mut() {
                out.extend(fx.eq.take().into_iter().chain(fx.comp.take()).map(|m| m.plugin));
            }
        }
        out.extend(self.adopt.drain(..).map(|p| p.plugin));
        self.synced = (usize::MAX, usize::MAX, usize::MAX);
        out
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// Format of the main mix as of the last sync.
    pub fn main_format(&self) -> ChannelFormat {
        self.plan.as_deref().map_or(self.main_fmt, |p| p.tables.main_fmt)
    }

    /// Channels of the main mix as of the last sync.
    pub fn main_channels(&self) -> usize {
        self.plan.as_deref().map_or(self.main.len(), |p| p.tables.main.len())
    }

    /// Play (velocity > 0) or release (velocity 0) `pitch` on a track's instrument at the next
    /// block, outside the clips: the MIDI Editor's audition. Does nothing before `sync`.
    pub fn live_note(&mut self, track: TrackId, pitch: u8, velocity: u8) {
        let strip = match self.plan.as_deref_mut() {
            Some(p) => p.tables.strip_of(track).and_then(|i| p.tables.strips.get_mut(i)),
            None => self.strips.get_mut(&track),
        };
        let Some(inst) = strip.and_then(|s| s.instrument.as_mut()) else { return };
        if velocity > 0 { inst.plugin.note_on(0, pitch.min(127), velocity.min(127)) } else { inst.plugin.note_off(0, pitch.min(127)) }
    }

    /// Reset all plugin state (on stop / seek).
    pub fn reset(&mut self) {
        for s in self.all_strips() {
            for p in s.plugins.iter_mut().flatten() {
                p.plugin.reset();
                p.failed = false;
            }
            if let Some(i) = &mut s.instrument {
                i.plugin.all_notes_off();
                i.plugin.reset();
                i.failed = false;
            }
            s.held.clear();
            s.chase = true;
            s.align.clear();
            s.align_pre.clear();
            s.out_extra.clear();
        }
        self.meters.clear();
        self.main_meter = StripMeter::default();
        // Plan mode: meters are fixed storage, zeroed in place.
        if let Some(p) = self.plan.as_deref_mut() {
            for m in p.tables.strips.iter_mut().map(|s| &mut s.meter).chain(std::iter::once(&mut p.tables.main_meter)) {
                m.peak = [0.0; 2];
                m.peaks.fill(0.0);
                m.gain_reduction_db = 0.0;
                m.plugin_failures = PluginFailures::default();
            }
        }
    }

    /// Release every held note on every instrument now. Unlike `reset`, this leaves plugin state,
    /// the playback position and note chasing alone, so queued releases still sound out.
    pub fn all_notes_off(&mut self) {
        for st in self.all_strips() {
            if let Some(i) = &mut st.instrument {
                i.plugin.all_notes_off();
            }
            st.held.clear();
        }
    }

    /// Give every instrument exactly one processing pass over silence, so a host that delivers
    /// queued events on its next `process` (a `reset` or `all_notes_off` only queues the release)
    /// gets to send them, even when the engine would otherwise not run (playback ended, stopped
    /// with nothing monitoring). `frames` is clamped to `max_block`. Nothing is scheduled, the
    /// position is untouched, inserts do not run, and the output is discarded. Allocation-free.
    pub fn flush_releases(&mut self, frames: usize) {
        let frames = frames.min(self.max_block);
        for st in self.all_strips() {
            let Some(inst) = st.instrument.as_mut().filter(|i| !i.failed) else { continue };
            // Defensive: `sync` sizes `buf` and `scratch` to `max_block`, but a plug-in must never
            // be handed a buffer shorter than `frames`, so clamp to the shortest one.
            if st.buf.len() >= 2 {
                let frames = frames.min(st.buf.iter().map(Vec::len).min().unwrap_or(0));
                for c in st.buf.iter_mut() {
                    c.iter_mut().take(frames).for_each(|x| *x = 0.0);
                }
                inst.plugin.process(&mut st.buf, frames);
            } else if let Some(c0) = st.buf.first_mut() {
                let mut io = [std::mem::take(c0), std::mem::take(&mut st.scratch)];
                let frames = frames.min(io[0].len()).min(io[1].len());
                for c in io.iter_mut() {
                    c.iter_mut().take(frames).for_each(|x| *x = 0.0);
                }
                inst.plugin.process(&mut io, frames);
                let [a, b] = io;
                *c0 = a;
                st.scratch = b;
            }
        }
    }

    /// Bring plugin instances in line with the session (creates/destroys as needed). Allocates;
    /// call outside the realtime thread when the document changes, or let `render` do it.
    pub fn sync(&mut self, s: &Session) {
        let mb = self.max_block;
        let main_ch = main_channels(s);
        self.main_fmt = s.main_format();
        if self.main.len() != main_ch {
            self.main = vec![vec![0.0; mb]; main_ch];
        }
        let mut cx = SlotCtx { sr: self.sample_rate, mb, external: self.external, adopt: &mut self.adopt, retired: &mut self.retired };
        let gone: Vec<TrackId> = self.strips.keys().filter(|id| s.track(**id).is_none()).copied().collect();
        for id in gone {
            if let Some(mut st) = self.strips.remove(&id) {
                for p in st.plugins.drain(..) {
                    cx.retire(p);
                }
                cx.retire(st.instrument.take());
                for fx in st.clip_fx.values_mut() {
                    cx.retire_module(fx.eq.take());
                    cx.retire_module(fx.comp.take());
                }
            }
        }
        for t in &s.tracks {
            let ch = strip_channels(t, main_ch);
            let strip = self.strips.entry(t.id).or_insert_with(Strip::new);
            if strip.buf.len() != ch || strip.buf.first().map_or(0, Vec::len) != mb {
                strip.buf = vec![vec![0.0; mb]; ch];
            }
            // The mono instrument path swaps `scratch` in as a second channel, and
            // `flush_releases` can run before any `render`, so size it here too.
            if strip.scratch.len() < mb {
                strip.scratch.resize(mb, 0.0);
            }
            while strip.plugins.len() > t.mixer.inserts.len() {
                let p = strip.plugins.pop().flatten();
                cx.retire(p);
            }
            strip.plugins.resize_with(t.mixer.inserts.len(), || None);
            for (idx, (slot, ins)) in strip.plugins.iter_mut().zip(t.mixer.inserts.iter()).enumerate() {
                match ins {
                    Some(i) if slot.as_ref().is_none_or(|p| p.id != i.plugin || p.ch != ch) || cx.waiting(t.id, idx) => {
                        let old = slot.take();
                        cx.retire(old);
                        *slot = cx.make(t.id, idx, i, ch);
                    }
                    None => {
                        let old = slot.take();
                        cx.retire(old);
                    }
                    _ => {}
                }
            }
            let want = t.instrument.as_ref();
            let ich = ch.max(2);
            if strip.instrument.as_ref().map(|p| (p.id.as_str(), p.ch)) != want.map(|i| (i.plugin.as_str(), ich))
                || (want.is_some() && cx.waiting(t.id, INSTRUMENT_SLOT))
            {
                let old = strip.instrument.take();
                cx.retire(old);
                strip.instrument = want.and_then(|i| cx.make(t.id, INSTRUMENT_SLOT, i, ich));
                strip.sample = None;
            }
            sync_sample(strip, want, s);
            if t.kind == TrackKind::Audio {
                sync_clip_fx(strip, t, s, ch, &mut cx);
            }
        }
        // Instances nobody claimed (the session changed again before they arrived).
        while let Some(p) = cx.adopt.pop() {
            cx.retired.push(p.plugin);
        }
        if !self.external {
            self.retired.clear();
        }
        for b in &s.busses {
            let n = b.format.channels().clamp(1, MAX_CHANNELS);
            let bus = self.busses.entry(b.id).or_insert_with(|| BusBuf { fmt: b.format, buf: Vec::new() });
            bus.fmt = b.format;
            if bus.buf.len() != n {
                bus.buf = vec![vec![0.0; mb]; n];
            }
        }
        self.busses.retain(|id, _| s.bus(*id).is_some());
    }

    /// Re-syncs plugin instances and the routing order when the session's structure changed
    /// since the last sync (or instances were adopted). `render` calls it; a realtime host may
    /// also call it right after swapping sessions so slots are filled before playback starts.
    pub fn ensure_synced(&mut self, s: &Session) {
        let key = (structure_fingerprint(s), s.tracks.len(), s.busses.len());
        if key != self.synced {
            self.sync(s);
            processing_order(s, &mut self.order, &mut self.order_depth);
            self.synced = key;
        }
    }

    /// Put the strips taken out for processing back into the engine, every one of them so none is
    /// dropped by an unwind, leaving `work` empty.
    fn restore_strips(&mut self, work: &mut Vec<(usize, Strip)>, s: &Session) {
        for (i, st) in work.drain(..) {
            if let Some(t) = s.tracks.get(i) {
                self.strips.insert(t.id, st);
            }
        }
    }

    /// Render `frames` (≤ max_block) samples starting at `pos` into `out` (≥ 2 channels).
    pub fn render(&mut self, s: &Session, pos: Samples, frames: usize, out: &mut [Vec<f32>]) {
        let frames = frames.min(self.max_block);
        // Stopped, the position stands still: not a seek (it would cut auditioned notes).
        if pos != self.last_end && !self.monitor_only {
            // Seek: silence held notes and ringing plugins.
            for st in self.strips.values_mut() {
                if let Some(i) = &mut st.instrument {
                    i.plugin.all_notes_off();
                }
                st.held.clear();
                st.chase = true;
            }
        }
        self.last_end = pos.saturating_add(frames as i64);
        self.ensure_synced(s);
        for b in self.busses.values_mut() {
            for c in b.buf.iter_mut() {
                c.iter_mut().take(frames).for_each(|x| *x = 0.0);
            }
        }
        for c in &mut self.main {
            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
        let any_solo = s.tracks.iter().any(|t| t.mixer.solo && !t.inactive);
        let order = std::mem::take(&mut self.order);
        let live = |t: &Track| !(t.kind == TrackKind::Master || t.inactive || t.kind == TrackKind::Vca);
        let is_aux = |t: &Track| matches!(t.kind, TrackKind::Aux | TrackKind::Folder);
        // Phase 1: independent strips (audio, instrument, MIDI) in parallel.
        let mut work = std::mem::take(&mut self.work);
        work.clear();
        for &i in &order {
            let Some(t) = s.tracks.get(i).filter(|t| live(t) && !is_aux(t)) else { continue };
            if let Some(st) = self.strips.remove(&t.id) {
                work.push((i, st));
            }
        }
        for (_, st) in work.iter_mut() {
            if st.scratch.len() < self.max_block {
                st.scratch.resize(self.max_block, 0.0);
            }
        }
        let busses = std::mem::take(&mut self.busses);
        let live_in = LiveInput { input: &self.input, monitor_only: self.monitor_only, recording: self.recording, no_suspend: self.no_suspend };
        // A plug-in that panics unwinds out of here. The strips go back into the engine first, so
        // their instances can still be drained and torn down off the audio thread, not dropped
        // by the unwind.
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_strips(&mut work, !self.serial, |(i, st)| {
                if let Some(t) = s.tracks.get(*i) {
                    process_strip(s, t, st, &busses, &live_in, pos, frames, any_solo);
                }
            });
        }));
        self.busses = busses;
        if let Err(panic) = ran {
            self.restore_strips(&mut work, s);
            self.work = work;
            self.order = order;
            std::panic::resume_unwind(panic);
        }
        // Delay compensation: align every strip to the slowest. Direct-to-main outputs also wait
        // for the slowest aux return. Aux latency has to be read before those outputs are routed,
        // or the first block (and a one-block bounce probe) still sees zero.
        let pdc = s.edit.delay_compensation;
        if pdc {
            for t in s.tracks.iter().filter(|t| is_aux(t) && !t.inactive) {
                if let Some(st) = self.strips.get_mut(&t.id) {
                    note_insert_latency(t, st, pos);
                }
            }
        }
        // The master faders' inserts delay the whole main mix, so their latency is part of the
        // mix's too (the faders run one after the other: it adds up).
        let mut lmax_m = 0usize;
        if pdc {
            for t in s.tracks.iter().filter(|t| t.kind == TrackKind::Master && !t.inactive) {
                if let Some(st) = self.strips.get_mut(&t.id) {
                    note_insert_latency(t, st, pos);
                    lmax_m = lmax_m.saturating_add(st.latency);
                }
            }
        }
        let lmax_t = if pdc { work.iter().map(|(_, st)| st.latency).max().unwrap_or(0) } else { 0 };
        let lmax_a =
            if pdc { self.strips.iter().filter(|(id, _)| s.track(**id).is_some_and(is_aux)).map(|(_, st)| st.latency).max().unwrap_or(0) } else { 0 };
        self.track_latency = lmax_t;
        self.latency = lmax_t + lmax_a + lmax_m;
        // An aux that returns onto a bus delays it by the slowest aux return, so the tracks routed
        // to that bus wait as well (as the ones routed to the main mix do) and the bus lines up.
        self.aux_latency = lmax_a;
        self.aux_fed.clear();
        if pdc {
            for t in s.tracks.iter().filter(|t| is_aux(t) && live(t)) {
                if let Route::Bus(b) = &t.mixer.output
                    && !self.aux_fed.contains(b)
                {
                    self.aux_fed.push(*b);
                }
            }
        }
        for (i, st) in work.iter_mut() {
            let Some(t) = s.tracks.get(*i) else { continue };
            let ch = st.buf.len();
            st.align.set(lmax_t.saturating_sub(st.latency), ch);
            st.align.process(&mut st.buf, frames);
            if t.mixer.sends.iter().flatten().any(|x| x.pre_fader) {
                st.align_pre.set(lmax_t.saturating_sub(st.latency), ch);
                st.align_pre.process(&mut st.pre, frames);
            }
            let waits = match &t.mixer.output {
                Route::Main => true,
                Route::Bus(b) => self.aux_fed.contains(b),
                _ => false,
            };
            st.out_extra.set(if waits { lmax_a } else { 0 }, ch);
        }
        for (i, st) in work.iter_mut() {
            if let Some(t) = s.tracks.get(*i) {
                self.route_strip(t, st, pos, frames);
            }
        }
        self.restore_strips(&mut work, s);
        self.work = work;
        let is_aux = |t: &Track| matches!(t.kind, TrackKind::Aux | TrackKind::Folder);
        // Phase 2: auxes in dependency order (they read busses).
        for t in order.iter().filter_map(|&i| s.tracks.get(i)).filter(|t| live(t)).filter(|t| is_aux(t)) {
            let Some(mut st) = self.strips.remove(&t.id) else { continue };
            if st.scratch.len() < self.max_block {
                st.scratch.resize(self.max_block, 0.0);
            }
            let live_in = LiveInput { input: &self.input, monitor_only: self.monitor_only, recording: self.recording, no_suspend: self.no_suspend };
            // As in phase 1: a panicking plug-in leaves its strip in the engine, not dropped by the unwind.
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process_strip(s, t, &mut st, &self.busses, &live_in, pos, frames, any_solo);
            }));
            if let Err(panic) = ran {
                self.strips.insert(t.id, st);
                self.order = order;
                std::panic::resume_unwind(panic);
            }
            let ch = st.buf.len();
            st.align.set(if pdc { lmax_a.saturating_sub(st.latency) } else { 0 }, ch);
            st.align.process(&mut st.buf, frames);
            st.out_extra.set(0, ch);
            self.route_strip(t, &mut st, pos, frames);
            self.strips.insert(t.id, st);
        }
        self.order = order;
        // Master faders process the main mix.
        let transport = transport_at(s, pos, !self.monitor_only, self.recording);
        for t in s.tracks.iter().filter(|t| t.kind == TrackKind::Master && !t.inactive) {
            if let Some(strip) = self.strips.get_mut(&t.id) {
                process_master(t, strip, &mut self.main, pos, frames, &transport, self.no_suspend);
                let m = self.meters.entry(t.id).or_default();
                m.measure(&self.main, frames, 0.0);
                m.plugin_failures = strip.failures;
            }
        }
        // Metronome: summed after the master fader, so it reaches the speakers without passing
        // through any track, send or insert. Only the playback engine sets `metronome`.
        // Not while monitoring only (transport stopped): the position does not advance there, so the
        // start of the same beat would repeat every block as a buzz.
        if self.metronome && !self.monitor_only && s.edit.click && (!s.edit.flag("click.only_during_record") || self.recording) {
            click::render(s, pos, frames, &mut self.main);
        }
        // Output: min(out.len(), main channels) channels; any further output channels are silent.
        for (c, o) in out.iter_mut().enumerate() {
            let src = self.main.get(c);
            for i in 0..frames {
                let v = src.and_then(|m| m.get(i)).copied().unwrap_or(0.0);
                if let Some(d) = o.get_mut(i) {
                    *d = if v.is_finite() { v } else { 0.0 };
                }
            }
        }
        self.main_meter.measure(&self.main, frames, 0.0);
    }

    /// The delay-compensation latency written onto `bus`: the slowest track insert, plus the
    /// slowest aux return when an aux returns onto this bus.
    fn bus_latency(&self, bus: BusId) -> usize {
        self.track_latency + if self.aux_fed.contains(&bus) { self.aux_latency } else { 0 }
    }

    /// Copy one rendered block of `bus` into `out`. A missing bus stays silent.
    fn copy_bus(&self, bus: BusId, frames: usize, out: &mut [Vec<f32>]) {
        for o in out.iter_mut() {
            for x in o.iter_mut().take(frames) {
                *x = 0.0;
            }
        }
        let Some(src) = self.busses.get(&bus) else { return };
        for (o, c) in out.iter_mut().zip(src.buf.iter()) {
            for i in 0..frames {
                let v = c.get(i).copied().unwrap_or(0.0);
                if let Some(d) = o.get_mut(i) {
                    *d = if v.is_finite() { v } else { 0.0 };
                }
            }
        }
    }

    /// Send, meter and pan a processed strip into the busses / main mix.
    fn route_strip(&mut self, t: &Track, strip: &mut Strip, pos: Samples, frames: usize) {
        let m = self.meters.entry(t.id).or_default();
        m.measure(&strip.buf, frames, strip.gr);
        m.plugin_failures = strip.failures;
        let mix = MixOut { busses: &mut self.busses, main: &mut self.main, main_fmt: self.main_fmt, law: self.pan_law };
        route_into(t, strip, pos, frames, mix);
    }
}

/// Where [`route_into`] sends a strip: the busses, the main mix and its format, and the pan law.
struct MixOut<'a, B: Busses + ?Sized> {
    busses: &'a mut B,
    main: &'a mut [Vec<f32>],
    main_fmt: ChannelFormat,
    law: PanLaw,
}

/// Send and pan a processed (and metered) strip into the busses / main mix.
fn route_into<B: Busses + ?Sized>(t: &Track, strip: &mut Strip, pos: Samples, frames: usize, out: MixOut<'_, B>) {
    let MixOut { busses, main, main_fmt, law } = out;
    {
        let src_fmt = strip_format(t, strip.buf.len());
        for (i, snd) in t.mixer.sends.iter().enumerate() {
            let Some(snd) = snd else { continue };
            if snd.mute || strip.muted {
                continue;
            }
            let lvl = send_level(t, i, snd.level_db, pos);
            let gain = db_to_gain(lvl);
            let src = if snd.pre_fader { &strip.pre } else { &strip.buf };
            let Route::Bus(b) = &snd.target else { continue };
            let Some(dst) = busses.bus_mut(*b) else { continue };
            if gain <= 0.0 {
                continue;
            }
            // Mono sends use the send's own pan; stereo sends go straight to L/R.
            let stereo = if src.len() == 1 { [snd.pan, 0.0] } else { [-1.0, 1.0] };
            let surround = snd.surround.as_ref().or(if snd.follow_main_pan { t.mixer.surround.as_ref() } else { None });
            let m = build_matrix(src_fmt, src.len(), dst.fmt, dst.buf.len(), &PanSpec { stereo, surround }, law);
            apply_matrix(src, frames, &mut dst.buf, &m, gain);
        }
        let src: &[Vec<f32>] = match &t.mixer.output {
            Route::Main | Route::Bus(_) if strip.out_extra.len > 0 => {
                if strip.outbuf.len() != strip.buf.len() {
                    strip.outbuf = vec![Vec::new(); strip.buf.len()];
                }
                for (d, src) in strip.outbuf.iter_mut().zip(strip.buf.iter()) {
                    d.clear();
                    d.extend_from_slice(src.get(..frames).unwrap_or(&[]));
                }
                strip.out_extra.process(&mut strip.outbuf, frames);
                &strip.outbuf
            }
            _ => &strip.buf,
        };
        let (dst, dst_fmt): (&mut [Vec<f32>], ChannelFormat) = match &t.mixer.output {
            Route::Main => (main, main_fmt),
            Route::Bus(b) => match busses.bus_mut(*b) {
                Some(d) => (&mut d.buf, d.fmt),
                None => return,
            },
            _ => return,
        };
        let mut start = 0;
        while start < frames {
            let (at, end) = pan_segment(t, pos, start, frames);
            let spec = PanSpec { stereo: [pan_at(t, 0, at), pan_at(t, 1, at)], surround: t.mixer.surround.as_ref() };
            let m = build_matrix(src_fmt, src.len(), dst_fmt, dst.len(), &spec, law);
            apply_matrix_span(src, start..end, dst, &m, 1.0);
            start = end;
        }
    }
}

/// Source → trim → inserts → (pre-fader copy) → mute/fader for one strip. Independent of other
/// strips except that aux inputs read `busses`, so non-aux strips can run in parallel.
/// Live input routed to monitored tracks.
struct LiveInput<'a> {
    input: &'a [Vec<f32>],
    monitor_only: bool,
    recording: bool,
    /// [`MixEngine::no_suspend`]: every insert runs every block.
    no_suspend: bool,
}

#[allow(clippy::too_many_arguments)]
fn process_strip<B: Busses + ?Sized>(
    s: &Session,
    t: &Track,
    strip: &mut Strip,
    busses: &B,
    live: &LiveInput<'_>,
    pos: Samples,
    frames: usize,
    any_solo: bool,
) {
    for c in strip.buf.iter_mut() {
        c.iter_mut().take(frames).for_each(|x| *x = 0.0);
    }
    let transport = transport_at(s, pos, !live.monitor_only, live.recording);
    match t.kind {
        TrackKind::Audio if !live.input.is_empty() && (t.mixer.input_monitor || (t.mixer.record_arm && (live.monitor_only || live.recording))) => {
            // Input monitoring: the track hears its hardware input instead of its clips.
            let first = match &t.mixer.input {
                Route::Hardware(h) => {
                    h.trim_start_matches("In ").split(['-', ' ']).next().and_then(|n| n.parse::<usize>().ok()).map_or(0, |n| n.saturating_sub(1))
                }
                _ => 0,
            };
            let n_in = live.input.len().max(1);
            for (k, dst) in strip.buf.iter_mut().enumerate() {
                if let Some(src) = live.input.get((first + k) % n_in) {
                    for (d, x) in dst.iter_mut().zip(src.iter()).take(frames) {
                        *d = *x;
                    }
                }
            }
        }
        TrackKind::Audio if live.monitor_only => {}
        TrackKind::Audio => {
            let block = Range { start: pos, end: pos.saturating_add(frames as i64) };
            for clip in t.clips() {
                if clip.muted || !clip.is_audio() || !clip.range().overlaps(&block) {
                    continue;
                }
                match clip_fx_params(s, clip.id.0) {
                    None => render_clip(s, clip, pos, frames, &mut strip.buf, &mut strip.scratch),
                    Some(params) => {
                        let nch = strip.buf.len();
                        if strip.clipbuf.len() != nch || strip.clipbuf.first().map_or(0, Vec::len) < frames {
                            strip.clipbuf = vec![vec![0.0; frames.max(strip.scratch.len())]; nch];
                        }
                        for c in strip.clipbuf.iter_mut() {
                            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
                        }
                        render_clip(s, clip, pos, frames, &mut strip.clipbuf, &mut strip.scratch);
                        let sr = s.sample_rate.as_f64() as f32;
                        let mut gain_only;
                        let fx = if strip.external {
                            // Realtime: `sync` placed the chain with its modules. A clip without
                            // one (only a gain, or modules still on their way) uses a chain
                            // without modules here, which allocates nothing.
                            match strip.clip_fx.get_mut(clip.id.0) {
                                Some(fx) => fx,
                                None => {
                                    gain_only = ClipFx::new(sr, nch, false);
                                    &mut gain_only
                                }
                            }
                        } else {
                            strip.clip_fx.get_or_insert_with(clip.id.0, || ClipFx::new(sr, nch, true))
                        };
                        fx.configure(params);
                        fx.process(&mut strip.clipbuf, frames);
                        for (d, c) in strip.buf.iter_mut().zip(strip.clipbuf.iter()) {
                            for (x, y) in d.iter_mut().zip(c.iter()).take(frames) {
                                *x += *y;
                            }
                        }
                    }
                }
            }
        }
        TrackKind::Aux | TrackKind::Folder => {
            if let Route::Bus(b) = &t.mixer.input
                && let Some(src) = busses.bus(*b)
            {
                let (ns, nd) = (src.buf.len(), strip.buf.len());
                let dst_fmt = strip_format(t, nd);
                let m = if src.fmt == dst_fmt && ns == nd {
                    identity_matrix(ns)
                } else if ns == 1 && nd == 2 {
                    // A mono bus feeds both sides of a stereo aux.
                    let mut m = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
                    m[0][0] = 1.0;
                    m[0][1] = 1.0;
                    m
                } else {
                    let stereo = if ns == 1 { [0.0, 0.0] } else { [-1.0, 1.0] };
                    build_matrix(src.fmt, ns, dst_fmt, nd, &PanSpec { stereo, surround: None }, PanLaw::Minus3)
                };
                apply_matrix(&src.buf, frames, &mut strip.buf, &m, 1.0);
            }
        }
        TrackKind::Instrument | TrackKind::Midi => {
            if let Some(inst) = strip.instrument.as_mut().filter(|i| !i.failed) {
                // #152: push the instrument's stored parameter values into the live plugin
                // before processing, the same way inserts do below. Without this, the values
                // loaded from a session (or set via `mix.insert_state`) never reach the plugin
                // instance and the rendered audio matches the plugin's defaults — e.g. a saved
                // `level = -12 dB` sounds the same as `level = -6 dB`.
                if let Some(ins) = &t.instrument {
                    apply_params(inst, ins, t, INSTRUMENT_SLOT, pos);
                }
                inst.plugin.set_transport(&transport);
                // Stopped: no clips, but the instrument still plays auditioned notes.
                if !live.monitor_only {
                    if std::mem::take(&mut strip.chase) {
                        chase_controllers(s, t, pos, inst);
                    }
                    schedule_notes(s, t, pos, frames, inst, &mut strip.held);
                }
                if strip.buf.len() >= 2 {
                    inst.plugin.process(&mut strip.buf, frames);
                } else if let Some(c0) = strip.buf.first_mut() {
                    // Mono MIDI track: render the instrument in stereo and fold.
                    let mut st = [std::mem::take(c0), std::mem::take(&mut strip.scratch)];
                    inst.plugin.process(&mut st, frames);
                    let [a, b] = st;
                    *c0 = a;
                    strip.scratch = b;
                    for (x, y) in c0.iter_mut().zip(strip.scratch.iter()).take(frames) {
                        *x = 0.5 * (*x + *y);
                    }
                }
                if contain_failure(inst, &mut strip.buf, frames) {
                    strip.failures.record(INSTRUMENT_SLOT);
                }
            }
        }
        _ => {}
    }
    let trim = db_to_gain(t.mixer.trim_db) * if t.mixer.phase_invert { -1.0 } else { 1.0 };
    if (trim - 1.0).abs() > f32::EPSILON {
        for c in strip.buf.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x *= trim);
        }
    }
    let mut gr = 0.0f32;
    let mut latency = 0usize;
    for (i, ins) in t.mixer.inserts.iter().enumerate() {
        let (Some(ins), Some(Some(slot))) = (ins, strip.plugins.get_mut(i)) else { continue };
        if !ins.active || ins.bypass || slot.failed {
            continue;
        }
        apply_params(slot, ins, t, i, pos);
        if run_insert(slot, &mut strip.buf, frames, &transport, live.no_suspend) && contain_failure(slot, &mut strip.buf, frames) {
            strip.failures.record(i);
            continue;
        }
        latency = latency.saturating_add(slot.plugin.latency());
        if gr == 0.0 {
            gr = slot.plugin.gain_reduction_db();
        }
    }
    strip.gr = gr;
    strip.latency = latency.min(MAX_LATENCY);
    let auto_mute = t.mixer.automation_mode.reads() && t.lane(&AutoParam::Mute).is_some_and(|l| l.value_at(pos, 0.0) >= 0.5);
    let soloed_out = any_solo && !t.mixer.solo && !t.mixer.solo_safe && !receives_solo(s, t);
    strip.muted = t.mixer.mute || auto_mute || soloed_out;
    if t.mixer.sends.iter().flatten().any(|x| x.pre_fader) {
        strip.pre.resize_with(strip.buf.len(), Vec::new);
        for (d, src) in strip.pre.iter_mut().zip(strip.buf.iter()) {
            d.clear();
            d.extend_from_slice(src.get(..frames).unwrap_or(&[]));
        }
    }
    // Endpoints are the first and last sample of this block. The next block starts on its own
    // first sample, so a breakpoint there does not ramp the block before it.
    let last = pos.saturating_add(frames.saturating_sub(1) as i64);
    let mut v0 = volume_db_at(t, pos);
    let mut v1 = volume_db_at(t, last);
    // Trim automation (an offset on top of the volume curve).
    if let Some(l) = t
        .automation
        .iter()
        .find(|l| matches!(&l.param, AutoParam::Plugin { slot: u8::MAX, param } if param == "trim"))
        .filter(|l| !l.points.is_empty())
    {
        v0 += l.value_at(pos, 0.0);
        v1 += l.value_at(last, 0.0);
    }
    // VCA master: its fader offsets the member's, and its mute mutes the member.
    if let Some(vid) = t.mixer.vca
        && let Some(vca) = s.tracks.iter().find(|x| x.id.0 == vid && x.kind == TrackKind::Vca)
    {
        v0 += volume_db_at(vca, pos);
        v1 += volume_db_at(vca, last);
        if vca.mixer.mute {
            strip.muted = true;
        }
    }
    let (g0, g1) = if strip.muted { (-144.0, -144.0) } else { (v0.min(12.0), v1.min(12.0)) };
    for c in strip.buf.iter_mut() {
        for i in 0..frames {
            if let Some(x) = c.get_mut(i) {
                *x *= lerp_gain(g0, g1, i, frames.saturating_sub(1));
            }
        }
    }
}

/// A master fader over the main mix: copies the mix into the strip, runs its inserts and writes
/// it back through the fader.
fn process_master(t: &Track, strip: &mut Strip, main: &mut [Vec<f32>], pos: Samples, frames: usize, transport: &Transport, no_suspend: bool) {
    for (dst, src) in strip.buf.iter_mut().zip(main.iter()) {
        if let (Some(d), Some(sr)) = (dst.get_mut(..frames), src.get(..frames)) {
            d.copy_from_slice(sr);
        }
    }
    for (i, ins) in t.mixer.inserts.iter().enumerate() {
        if let (Some(ins), Some(Some(slot))) = (ins, strip.plugins.get_mut(i))
            && ins.active
            && !ins.bypass
            && !slot.failed
        {
            apply_params(slot, ins, t, i, pos);
            if run_insert(slot, &mut strip.buf, frames, transport, no_suspend) && contain_failure(slot, &mut strip.buf, frames) {
                strip.failures.record(i);
            }
        }
    }
    let v0 = volume_db_at(t, pos);
    let v1 = volume_db_at(t, pos + frames as i64);
    let n = main.len();
    for (ch, src) in strip.buf.iter().enumerate().take(n) {
        if let Some(m) = main.get_mut(ch) {
            for i in 0..frames {
                let g = lerp_gain(v0, v1, i, frames);
                if let (Some(d), Some(x)) = (m.get_mut(i), src.get(i)) {
                    *d = x * g;
                }
            }
        }
    }
}

/// Below this magnitude (about -120 dBFS) a sample counts as silence for insert suspension.
const SILENCE_THRESHOLD: f32 = 1e-6;

/// True when every sample of every channel in the block is exactly 0.0: no threshold, because
/// CLAP's `CLAP_PROCESS_SLEEP` means "sleep until a non-zero input", however quiet.
fn is_all_zero(buf: &[Vec<f32>], frames: usize) -> bool {
    buf.iter().all(|c| c.get(..frames).unwrap_or(c).iter().fold(true, |ok, x| ok & (*x == 0.0)))
}

/// True when every sample of every channel in the block is below [`SILENCE_THRESHOLD`].
fn is_silent(buf: &[Vec<f32>], frames: usize) -> bool {
    // A branch-free fold vectorizes (see `contain_failure`).
    buf.iter().all(|c| c.get(..frames).unwrap_or(c).iter().fold(true, |ok, x| ok & (x.abs() < SILENCE_THRESHOLD)))
}

/// Runs one insert over the block, or leaves it unprocessed when it is a hosted effect, its
/// input is silent and its [`ProcessStatus`] allows that (`Sleep` while the input is exactly
/// zero, any nonzero sample wakes it; `ContinueIfNotQuiet` once its last output was silent too; `Tail` once `tail_samples()` of
/// silence have gone through and its last output was silent). The buffers are then left as they
/// are: silent in, silent out. Any non-silent block resumes it, and a parameter change wakes it
/// for one block (without restarting its tail count). `no_suspend` runs every insert. True when
/// the plugin processed the block.
fn run_insert(slot: &mut PluginSlot, buf: &mut [Vec<f32>], frames: usize, transport: &Transport, no_suspend: bool) -> bool {
    if slot.suspendable {
        // Exactly zero implies below the threshold, so the second scan runs only when it must.
        let all_zero = is_all_zero(buf, frames);
        let silent = all_zero || is_silent(buf, frames);
        if silent {
            let skip = !no_suspend
                && !slot.woken
                && match slot.plugin.process_status() {
                    ProcessStatus::Continue => false,
                    ProcessStatus::ContinueIfNotQuiet => slot.output_silent,
                    ProcessStatus::Sleep => all_zero,
                    ProcessStatus::Tail => slot.output_silent && slot.quiet >= slot.plugin.tail_samples(),
                };
            if skip {
                return false;
            }
            slot.quiet = slot.quiet.saturating_add(frames);
        } else {
            slot.quiet = 0;
        }
        // The flag asserts true silence (VST3 `silenceFlags`, CLAP `constant_mask`), so a quiet but
        // nonzero block must not set it.
        slot.plugin.set_input_silent(all_zero);
    }
    slot.woken = false;
    slot.plugin.set_transport(transport);
    slot.plugin.process(buf, frames);
    if slot.suspendable {
        slot.output_silent = is_silent(buf, frames);
    }
    true
}

/// Silences a block the plugin filled with NaN or infinity and takes the plugin out until the
/// next [`MixEngine::reset`]. True then.
fn contain_failure(slot: &mut PluginSlot, buf: &mut [Vec<f32>], frames: usize) -> bool {
    // A branch-free fold vectorizes; `all` runs about 8 times slower here.
    if buf.iter().all(|c| c.get(..frames).unwrap_or(c).iter().fold(true, |ok, x| ok & x.is_finite())) {
        return false;
    }
    for c in buf.iter_mut() {
        c.iter_mut().take(frames).for_each(|x| *x = 0.0);
    }
    slot.failed = true;
    true
}

/// Clip-effect parameters of a clip, or None when it has none or they are bypassed.
fn clip_fx_params(s: &Session, clip: u64) -> Option<impl Iterator<Item = (&str, f32)> + Clone> {
    if s.edit.values.is_empty() || Key::new(format_args!("clip_fx.bypass.{clip}")).is_none_or(|k| s.edit.flag(k.as_str())) {
        return None;
    }
    let pre = Key::new(format_args!("clip_fx.{clip}."))?;
    let mut params = s
        .edit
        .values
        .range::<str, _>((Bound::Included(pre.as_str()), Bound::Unbounded))
        .take_while(move |(k, _)| k.starts_with(pre.as_str()))
        .map(move |(k, v)| (k.get(pre.len..).unwrap_or(""), *v as f32))
        .peekable();
    params.peek()?;
    Some(params)
}

/// A short key formatted on the stack: lookups made on the audio thread must not allocate.
#[derive(Clone, Copy)]
struct Key {
    buf: [u8; 64],
    len: usize,
}

impl Key {
    /// None when the key doesn't fit.
    fn new(args: std::fmt::Arguments<'_>) -> Option<Key> {
        let mut k = Key { buf: [0; 64], len: 0 };
        std::fmt::Write::write_fmt(&mut k, args).ok()?;
        Some(k)
    }

    fn as_str(&self) -> &str {
        self.buf.get(..self.len).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("")
    }
}

impl std::fmt::Write for Key {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(std::fmt::Error)?;
        self.buf.get_mut(self.len..end).ok_or(std::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl ClipFx {
    fn new(sr: f32, ch: usize, lazy: bool) -> ClipFx {
        ClipFx { eq: None, comp: None, eq_on: false, comp_on: false, gain_db: 0.0, sr, ch, lazy }
    }
    fn make(&self, id: &str) -> Option<ClipModule> {
        soundcraft_dsp::create(id).map(|mut p| {
            p.prepare(self.sr, 16_384, self.ch.max(1));
            PluginSlot::new(p, String::new(), self.ch, CLIP_FX_BIT)
        })
    }
    /// A module runs only while one of its parameters is set. Offline, modules are made and
    /// dropped here accordingly; realtime, `sync` places and retires them.
    fn configure<'a>(&mut self, params: impl Iterator<Item = (&'a str, f32)> + Clone) {
        self.gain_db = 0.0;
        self.eq_on = params.clone().any(|(k, _)| k.starts_with("eq."));
        self.comp_on = params.clone().any(|(k, _)| k.starts_with("comp."));
        if self.lazy {
            if self.eq_on && self.eq.is_none() {
                self.eq = self.make(CLIP_EQ);
            } else if !self.eq_on {
                self.eq = None;
            }
            if self.comp_on && self.comp.is_none() {
                self.comp = self.make(CLIP_COMP);
            } else if !self.comp_on {
                self.comp = None;
            }
        }
        for (k, v) in params {
            if let Some(p) = k.strip_prefix("eq.") {
                if let Some(eq) = &mut self.eq
                    && eq.plugin.param(p) != Some(v)
                {
                    eq.plugin.set_param(p, v);
                }
            } else if let Some(p) = k.strip_prefix("comp.") {
                if let Some(c) = &mut self.comp
                    && c.plugin.param(p) != Some(v)
                {
                    c.plugin.set_param(p, v);
                }
            } else if k == "gain" {
                self.gain_db = v;
            }
        }
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if let Some(eq) = self.eq.as_mut().filter(|_| self.eq_on) {
            eq.plugin.process(io, frames);
        }
        if let Some(c) = self.comp.as_mut().filter(|_| self.comp_on) {
            c.plugin.process(io, frames);
        }
        if self.gain_db.abs() > 1e-6 {
            let g = db_to_gain(self.gain_db);
            for ch in io.iter_mut() {
                ch.iter_mut().take(frames).for_each(|x| *x *= g);
            }
        }
    }
}

/// Brings an audio strip's clip-effect chains in line with its clips: places the modules the
/// host handed over (realtime), and retires modules whose clip is gone or whose parameters no
/// longer use them (bypassed clips included), so they are dropped off the audio thread. A
/// realtime strip also gets its clip buffer and room for [`CLIP_FX_RESERVE`] chains here, when
/// it is made, instead of when a clip effect is first added during playback.
fn sync_clip_fx(strip: &mut Strip, t: &Track, s: &Session, ch: usize, cx: &mut SlotCtx<'_>) {
    strip.external = cx.external;
    if cx.external {
        if strip.clipbuf.len() != ch || strip.clipbuf.first().map_or(0, Vec::len) < cx.mb {
            strip.clipbuf = vec![vec![0.0; cx.mb]; ch];
        }
        if strip.clip_fx.0.capacity() == 0 {
            strip.clip_fx.0.reserve(CLIP_FX_RESERVE);
        }
    }
    while let Some((clip, comp, p)) = cx.take_clip_module(t.id) {
        if p.ch != ch {
            cx.retired.push(p.plugin);
            continue;
        }
        let fx = strip.clip_fx.get_or_insert_with(clip, || ClipFx::new(cx.sr, ch, !cx.external));
        let slot = clip_fx_slot(clip, comp).unwrap_or(CLIP_FX_BIT);
        let module = Some(PluginSlot::new(p.plugin, p.id, p.ch, slot));
        let old = if comp { std::mem::replace(&mut fx.comp, module) } else { std::mem::replace(&mut fx.eq, module) };
        cx.retire_module(old);
    }
    strip.clip_fx.retain(|clip, fx| {
        let exists = t.clips().iter().any(|c| c.id.0 == clip);
        let (eq, comp) = if exists { clip_fx_modules(s, clip) } else { (false, false) };
        if !eq {
            cx.retire_module(fx.eq.take());
        }
        if !comp {
            cx.retire_module(fx.comp.take());
        }
        exists
    });
}

/// Hash of everything `sync`/`processing_order` depend on (tracks, kinds, routing, plugins, sends).
fn structure_fingerprint(s: &Session) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for t in &s.tracks {
        t.id.hash(&mut h);
        t.kind.hash(&mut h);
        t.format.hash(&mut h);
        t.inactive.hash(&mut h);
        t.mixer.inserts.len().hash(&mut h);
        t.mixer.input.hash(&mut h);
        t.mixer.output.hash(&mut h);
        for i in &t.mixer.inserts {
            i.as_ref().map(|x| x.plugin.as_str()).hash(&mut h);
        }
        for snd in t.mixer.sends.iter().flatten() {
            snd.target.hash(&mut h);
        }
        t.instrument.as_ref().map(|x| x.plugin.as_str()).hash(&mut h);
        let sample = t.instrument.as_ref().and_then(|x| x.sample);
        sample.hash(&mut h);
        // Which decoded audio it is, so new audio under the same id (another session) re-syncs.
        sample.and_then(|id| s.pool.get(id)).map(|a| Arc::as_ptr(a) as usize).hash(&mut h);
    }
    for b in &s.busses {
        b.id.hash(&mut h);
        b.format.hash(&mut h);
    }
    s.main_format().hash(&mut h);
    h.finish() as usize
}

std::thread_local! {
    static AUDIO_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the calling thread as the realtime audio thread (`soundcraft_playback`'s cpal callbacks
/// do, every call: one thread-local store). Strips [`MixEngine::render`] hands to worker threads
/// count as the audio thread while they render for it. Code that might run there asks
/// [`on_audio_thread`] before doing anything that blocks; the desktop app's logger does, so a
/// `log::` record from the audio thread never takes a lock or touches a file.
pub fn mark_audio_thread() {
    AUDIO_THREAD.set(true);
}

/// True on the thread [`mark_audio_thread`] marked, and on a worker while it renders strips for
/// it. Lock-free and allocation-free.
pub fn on_audio_thread() -> bool {
    AUDIO_THREAD.get()
}

/// Run `f` on this thread as (or as not) the audio thread, then restore the thread's own mark
/// (also when `f` unwinds): a global pool worker may render for an offline bounce.
#[cfg(not(target_arch = "wasm32"))]
fn as_audio_thread(audio: bool, f: impl FnOnce()) {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            AUDIO_THREAD.set(self.0);
        }
    }
    let _restore = Restore(AUDIO_THREAD.replace(audio));
    f();
}

/// Strips render in parallel from this many on; below it the fan-out costs more than it saves.
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_STRIPS: usize = 4;

/// The render pool never has more workers than this, whatever the core count.
pub const MAX_RENDER_WORKERS: usize = 8;

/// Environment variable: `1` opts in to the render pool for audio-thread strips, as
/// [`MixEngine::force_serial`]`(false)` does. Read once, when the render pool is built.
///
/// Serial is the realtime default until the bounded render plan (notes/10) lands; measured
/// 2026-10-18: promoted serial 0 late blocks vs promoted pool 1-5 per 2000 under load.
pub const RENDER_POOL_ENV: &str = "SOUNDCRAFT_RENDER_POOL";

/// Environment variable: `1` leaves the render pool's workers at the OS default priority instead
/// of promoting them with [`soundcraft_rt::promote_audio_thread`]. Read once, when the render
/// pool is built. It does not affect the audio callback thread: this crate never promotes that.
pub const RENDER_NO_PRIORITY_ENV: &str = "SOUNDCRAFT_RENDER_NO_PRIORITY";

/// Audio-thread strips render serially when set ([`MixEngine::force_serial`]). On by default;
/// [`RENDER_POOL_ENV`] clears it at startup.
static FORCE_SERIAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// What each render pool worker's [`soundcraft_rt::promote_audio_thread`] achieved, by worker
/// index; written once by the worker's start handler, never on the render path.
#[cfg(not(target_arch = "wasm32"))]
static WORKER_PROMOTION: [std::sync::OnceLock<soundcraft_rt::Promotion>; MAX_RENDER_WORKERS] =
    [const { std::sync::OnceLock::new() }; MAX_RENDER_WORKERS];

/// True when `name` is set to `1` (or `true`/`yes`, any case).
#[cfg(not(target_arch = "wasm32"))]
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// What each render pool worker's promotion achieved, by worker index (`None` for a worker that
/// does not exist or has not started yet; `Unsupported("disabled")` under
/// [`RENDER_NO_PRIORITY_ENV`]). For diagnostics and tests.
#[must_use]
pub fn render_worker_promotions() -> [Option<soundcraft_rt::Promotion>; MAX_RENDER_WORKERS] {
    #[cfg(not(target_arch = "wasm32"))]
    return std::array::from_fn(|i| WORKER_PROMOTION.get(i).and_then(|slot| slot.get().copied()));
    #[cfg(target_arch = "wasm32")]
    return [None; MAX_RENDER_WORKERS];
}

/// The realtime render pool: strips rendered for the audio thread run here, never on Rayon's
/// global pool, which offline bounces and video decode/encode share and which runs at normal
/// priority behind whatever they queued. `None` when the machine has too few cores for a pool to
/// pay (or the threads could not be spawned): the audio thread then renders its strips itself.
///
/// Serial is the realtime default until the bounded render plan (notes/10) lands; measured
/// 2026-10-18: promoted serial 0 late blocks vs promoted pool 1-5 per 2000 under load.
///
/// The audio callback thread is promoted by the audio backend (`playback`), before its first data
/// callback; `MixEngine::render` makes no OS priority call (MMCSS registration is a service call
/// that can block). Only the pool workers are promoted here, by their start handler at pool
/// build, off the callback. The pool
/// is still built (and its workers promoted) so [`MixEngine::force_serial`]`(false)` and
/// [`RENDER_POOL_ENV`] can opt in.
#[cfg(not(target_arch = "wasm32"))]
static RENDER_POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();

/// Spawn the render pool, once per process. [`MixEngine::new`] calls this, so the pool exists
/// before the first callback; it never spawns from the audio thread (a `MixEngine` built there,
/// on a sample-rate change, reuses the pool an earlier engine built). The switches
/// [`RENDER_POOL_ENV`] and [`RENDER_NO_PRIORITY_ENV`] are read here, once.
#[cfg(not(target_arch = "wasm32"))]
fn init_render_pool() {
    if !on_audio_thread() {
        RENDER_POOL.get_or_init(|| {
            if env_flag(RENDER_POOL_ENV) {
                log::info!("mix: {RENDER_POOL_ENV} is set; audio-thread strips render on the pool");
                FORCE_SERIAL.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            let promote = !env_flag(RENDER_NO_PRIORITY_ENV);
            build_render_pool(promote)
        });
    }
}

#[cfg(target_arch = "wasm32")]
fn init_render_pool() {}

/// One worker per core but the one the audio callback runs on, at most [`MAX_RENDER_WORKERS`].
/// Fewer than two workers is no pool: one worker plus a waiting callback is slower than the
/// callback alone.
///
/// Priority: each worker promotes itself at start with [`soundcraft_rt::promote_audio_thread`]
/// (MMCSS "Pro Audio" on Windows, `SCHED_FIFO` on Linux, the Mach time-constraint policy on
/// macOS; ADR 0013 keeps that `unsafe` out of this crate), unless `promote` is false. The result
/// lands in [`render_worker_promotions`] and is logged once per worker at debug level. A refused
/// promotion leaves the worker at the default priority; the pool is still dedicated, so nothing
/// queued by a bounce or a video job runs ahead of a strip.
#[cfg(not(target_arch = "wasm32"))]
fn build_render_pool(promote: bool) -> Option<rayon::ThreadPool> {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let workers = cores.saturating_sub(1).min(MAX_RENDER_WORKERS);
    if workers < 2 {
        return None;
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("mix-render-{i}"))
        .start_handler(move |i| {
            let p = if promote { soundcraft_rt::promote_audio_thread() } else { soundcraft_rt::Promotion::Unsupported("disabled") };
            if let Some(slot) = WORKER_PROMOTION.get(i) {
                let _ = slot.set(p);
            }
            // Before the mark: the logger treats the audio thread as never-block.
            log::debug!("mix: render worker {i} priority: {p:?}");
            // A worker only ever renders for the audio thread, so it is the audio thread for good.
            mark_audio_thread();
        })
        .build()
        .map_err(|e| log::warn!("mix: no render pool ({e}); the audio thread renders strips itself"))
        .ok()
}

/// Process strips, in parallel on native targets when there are enough of them.
///
/// On the audio thread the strips go to the dedicated render pool (or run right here when there
/// is none, or while [`MixEngine::force_serial`] is on, the default); they never wait behind Rayon's global
/// pool. Elsewhere (offline bounces) they use the global pool as before. The strips are
/// independent and are summed afterwards in a fixed order, so the result is the same whichever
/// way they ran.
///
/// Known residual, as before this pool existed: each parallel dispatch pushes one job onto Rayon's
/// injector queue, which allocates a 1.5 KB block on the caller every 63 pushes.
#[cfg(not(target_arch = "wasm32"))]
fn run_strips<T: Send>(work: &mut [T], parallel: bool, f: impl Fn(&mut T) + Sync + Send) {
    use rayon::prelude::*;
    if !parallel || work.len() < PARALLEL_STRIPS {
        work.iter_mut().for_each(f);
    } else if on_audio_thread() {
        let pool = if MixEngine::serial_forced() { None } else { RENDER_POOL.get().and_then(Option::as_ref) };
        match pool {
            // The pool's workers are marked as the audio thread (`build_render_pool`).
            Some(pool) => pool.install(|| work.par_iter_mut().for_each(f)),
            None => work.iter_mut().for_each(f),
        }
    } else {
        // A global worker renders for an offline caller here: it is not the audio thread.
        work.par_iter_mut().for_each(|w| as_audio_thread(false, || f(w)));
    }
}

#[cfg(target_arch = "wasm32")]
fn run_strips<T: Send>(work: &mut [T], _parallel: bool, f: impl Fn(&mut T) + Sync + Send) {
    work.iter_mut().for_each(f);
}

fn strip_channels(t: &Track, main_ch: usize) -> usize {
    match t.kind {
        TrackKind::Master => main_ch,
        TrackKind::Instrument => t.channels().clamp(2, MAX_CHANNELS),
        _ => t.channels().clamp(1, MAX_CHANNELS),
    }
}

/// The format a strip's buffer actually carries (a mono instrument renders in stereo).
fn strip_format(t: &Track, n: usize) -> ChannelFormat {
    if t.format.channels().min(MAX_CHANNELS) == n { t.format } else { ChannelFormat::for_channels(n) }
}

/// Longest chain of auxes feeding one another that the processing order tells apart (cycles
/// cap out here).
const MAX_AUX_DEPTH: usize = 8;

/// Writes the processing order into `order`: audio/instrument tracks first, then auxes ordered so
/// bus producers come before consumers (by depth, then track order). `depth` is scratch. Both
/// keep their capacity, so a re-sync allocates only when the track count grew.
fn processing_order(s: &Session, order: &mut Vec<usize>, depth: &mut Vec<usize>) {
    let is_aux = |t: &Track| matches!(t.kind, TrackKind::Aux | TrackKind::Folder);
    // `f` feeds aux `t` when it is an aux whose output or a send goes to `t`'s input bus.
    let feeds = |f: &Track, t: &Track| {
        let r = &t.mixer.input;
        is_aux(f) && matches!(r, Route::Bus(_)) && (&f.mixer.output == r || f.mixer.sends.iter().flatten().any(|sn| &sn.target == r))
    };
    order.clear();
    order.extend(s.tracks.iter().enumerate().filter(|(_, t)| !is_aux(t)).map(|(i, _)| i));
    // Depth = the longest chain of auxes feeding this aux's input bus, capped. Each round extends
    // every chain by one hop, so after MAX_AUX_DEPTH rounds each depth is min(longest, cap).
    depth.clear();
    depth.resize(s.tracks.len(), 0);
    for _ in 0..MAX_AUX_DEPTH {
        for (j, t) in s.tracks.iter().enumerate().filter(|(_, t)| is_aux(t)) {
            let deepest = s.tracks.iter().zip(depth.iter()).filter(|(f, _)| feeds(f, t)).map(|(_, d)| (d + 1).min(MAX_AUX_DEPTH)).max();
            if let (Some(d), Some(slot)) = (deepest, depth.get_mut(j)) {
                *slot = (*slot).max(d);
            }
        }
    }
    for d in 0..=MAX_AUX_DEPTH {
        order.extend(s.tracks.iter().zip(depth.iter()).enumerate().filter(|(_, (t, dt))| is_aux(t) && **dt == d).map(|(i, _)| i));
    }
}

/// Implicit solo: an aux is not muted when a soloed track feeds its input bus.
fn receives_solo(s: &Session, t: &Track) -> bool {
    let Route::Bus(b) = &t.mixer.input else { return false };
    s.tracks
        .iter()
        .any(|o| o.mixer.solo && (o.mixer.output == Route::Bus(*b) || o.mixer.sends.iter().flatten().any(|sn| sn.target == Route::Bus(*b))))
}

/// Metronome: a short synthesized blip on every beat of the tempo map, accented on beat 1 of
/// each bar. Settings come from `setup.click_countoff` (`click.*` values).
mod click {
    use super::*;

    /// Blip length in seconds.
    const LEN_S: f64 = 0.03;

    pub fn render(s: &Session, pos: Samples, frames: usize, main: &mut [Vec<f32>]) {
        let sr = s.sample_rate;
        let rate = sr.as_f64();
        let len = (LEN_S * rate) as i64;
        let end = pos.saturating_add(frames as i64);
        let gain = f64::from(db_to_gain(s.edit.value("click.volume_db", -6.0).clamp(-60.0, 12.0) as f32));
        let accent = (note_hz(s.edit.value("click.accent_note", 84.0)), s.edit.value("click.accent_velocity", 127.0));
        let normal = (note_hz(s.edit.value("click.normal_note", 72.0)), s.edit.value("click.normal_velocity", 100.0));
        // Start one blip length early so a click that began in an earlier block still rings into this one.
        let t0 = s.tempo.samples_to_ticks(pos.saturating_sub(len), sr).max(0).saturating_sub(1);
        let mut tick = t0.saturating_sub(s.tempo.bar_beat_at_tick(t0).tick);
        // Blocks hold a few beats at most; the guard only protects against a malformed tempo map.
        for _ in 0..4096 {
            let at = s.tempo.tick_to_samples(tick, sr);
            if at >= end {
                break;
            }
            if at.saturating_add(len) > pos {
                let (hz, vel) = if s.tempo.bar_beat_at_tick(tick).beat == 1 { accent } else { normal };
                let amp = gain * (vel.clamp(1.0, 127.0) / 127.0);
                blip(main, at - pos, frames, hz, amp as f32, rate);
            }
            tick = tick.saturating_add(s.tempo.meter_at_tick(tick).ticks_per_beat());
        }
    }

    /// Adds one decaying sine blip whose first sample sits `rel` frames into the block (negative
    /// when it began before the block) to every channel.
    fn blip(main: &mut [Vec<f32>], rel: i64, frames: usize, hz: f64, amp: f32, rate: f64) {
        let len = (LEN_S * rate) as usize;
        let w = std::f64::consts::TAU * hz / rate;
        let first = usize::try_from(-rel).unwrap_or(0);
        for i in first..len {
            let Ok(at) = usize::try_from(rel + i as i64) else { continue };
            if at >= frames {
                break;
            }
            let env = (-6.0 * i as f64 / len as f64).exp() as f32;
            let v = ((w * i as f64).sin() as f32) * env * amp;
            for ch in main.iter_mut() {
                if let Some(d) = ch.get_mut(at) {
                    *d += v;
                }
            }
        }
    }

    /// Click pitch for a MIDI note setting (clamped to the MIDI range).
    fn note_hz(note: f64) -> f64 {
        soundcraft_dsp::midi_to_hz(note.clamp(0.0, 127.0))
    }
}

fn render_clip(s: &Session, clip: &Clip, pos: Samples, frames: usize, buf: &mut [Vec<f32>], _scratch: &mut [f32]) {
    let ClipContent::Audio { source, offset } = clip.content else { return };
    let block = Range { start: pos, end: pos.saturating_add(frames as i64) };
    let Some(isect) = clip.range().intersect(&block) else { return };
    let Some(audio) = s.pool.get(source) else { return };
    let nsrc = audio.buffer.channels.len();
    if nsrc == 0 {
        return;
    }
    let stretch = if clip.stretch.is_finite() && clip.stretch > 0.0 { clip.stretch } else { 1.0 };
    let i0 = usize::try_from(isect.start - pos).unwrap_or(0);
    let i1 = usize::try_from(isect.end - pos).unwrap_or(0).min(frames);
    // Gain is evaluated every 32 samples and interpolated (fades and envelopes stay smooth).
    const STEP: usize = 32;
    for (ch, dst) in buf.iter_mut().enumerate() {
        let src = audio.buffer.channels.get(ch % nsrc).map(Vec::as_slice).unwrap_or(&[]);
        let mut i = i0;
        while i < i1 {
            let seg_end = (i + STEP).min(i1);
            let rel0 = pos.saturating_add(i as i64).saturating_sub(clip.start);
            // The last sample of this step, not the first sample of the next one, so a breakpoint
            // on the boundary does not ramp the audio before it.
            let rel1 = pos.saturating_add(seg_end.saturating_sub(1) as i64).saturating_sub(clip.start);
            let g0 = clip.gain_at(rel0);
            let g1 = clip.gain_at(rel1.min(clip.length));
            let n = (seg_end - i).saturating_sub(1).max(1) as f32;
            for k in i..seg_end {
                let rel = pos.saturating_add(k as i64).saturating_sub(clip.start);
                let v = if (stretch - 1.0).abs() < 1e-9 {
                    usize::try_from(offset.saturating_add(rel)).ok().and_then(|x| src.get(x)).copied().unwrap_or(0.0)
                } else {
                    let f = offset as f64 + rel as f64 / stretch;
                    let x0 = f.floor();
                    let fr = (f - x0) as f32;
                    let a = if x0 >= 0.0 { src.get(x0 as usize).copied().unwrap_or(0.0) } else { 0.0 };
                    let b = if x0 + 1.0 >= 0.0 { src.get((x0 as usize).saturating_add(1)).copied().unwrap_or(0.0) } else { 0.0 };
                    a + (b - a) * fr
                };
                let t = (k - i) as f32 / n;
                if let Some(d) = dst.get_mut(k) {
                    *d += v * (g0 + (g1 - g0) * t);
                }
            }
            i = seg_end;
        }
    }
}

/// The track's MIDI Track Offset (`midi.offset.<id>` in the edit values), in samples. Looked up
/// without formatting the key, since this runs on the audio thread every block.
fn midi_offset(s: &Session, t: &Track) -> i64 {
    const PREFIX: &str = "midi.offset.";
    s.edit
        .values
        .range::<str, _>((std::ops::Bound::Included(PREFIX), std::ops::Bound::Unbounded))
        .take_while(|(k, _)| k.starts_with(PREFIX))
        .find(|(k, _)| k.get(PREFIX.len()..).and_then(|id| id.parse::<u64>().ok()) == Some(t.id.0))
        .map(|(_, v)| *v)
        .filter(|v| v.is_finite())
        .map_or(0, |v| v as i64)
}

/// Send note on/offs and controllers for MIDI clips overlapping the block.
fn schedule_notes(s: &Session, t: &Track, pos: Samples, frames: usize, inst: &mut PluginSlot, held: &mut Vec<u8>) {
    let sr = s.sample_rate;
    // MIDI Track Offsets: the track's notes sound `offset` samples later (earlier if negative), so
    // the block looks that far back into the clips.
    let offset = midi_offset(s, t);
    let pos = pos.saturating_sub(offset);
    let end = pos.saturating_add(frames as i64);
    // Two passes, so at any one sample offset every note-off precedes every note-on: a clip that
    // ends where the next begins (or a repeated pitch) must not have its new note killed by the
    // old one's release. Allocation-free; a note whose on and off share a sample keeps on-then-off.
    for ons in [false, true] {
        for clip in t.clips() {
            if clip.muted {
                continue;
            }
            let ClipContent::Midi { sequence } = &clip.content else { continue };
            // A note-off is clamped to `clip.end()`, which the half-open range excludes: when that
            // sample is the first of this block (`pos == clip.end()`) the clip still has to be visited
            // or the off is never sent and the note holds forever.
            if clip.end() < pos || clip.start >= end {
                continue;
            }
            let base = s.tempo.samples_to_ticks(clip.start, sr);
            // MIDI Real-Time Properties (non-destructive velocity/transpose/duration/delay).
            let rtp = |name: &str, d: f64| Key::new(format_args!("rtp.{}.{name}", t.id.0)).map_or(d, |k| s.edit.value(k.as_str(), d));
            let (dv, dp, dur, delay) = if s.edit.values.is_empty() {
                (0i64, 0i64, 1.0f64, 0i64)
            } else {
                (
                    rtp("velocity", 0.0).clamp(-127.0, 127.0) as i64,
                    rtp("transpose", 0.0).clamp(-127.0, 127.0) as i64,
                    rtp("duration", 100.0).clamp(1.0, 1000.0) / 100.0,
                    rtp("delay", 0.0).clamp(-1e7, 1e7) as i64,
                )
            };
            for n0 in &sequence.notes {
                let mut n = *n0;
                n.velocity = (i64::from(n.velocity) + dv).clamp(1, 127) as u8;
                n.pitch = (i64::from(n.pitch) + dp).clamp(0, 127) as u8;
                n.length = ((n.length as f64 * dur) as i64).max(1);
                n.start = n.start.saturating_add(delay).max(0);
                let on = s.tempo.tick_to_samples(base.saturating_add(n.start), sr);
                let off = s.tempo.tick_to_samples(base.saturating_add(n.start).saturating_add(n.length), sr).min(clip.end());
                if on >= clip.end() {
                    continue;
                }
                if ons && on >= pos && on < end {
                    inst.plugin.note_on(usize::try_from(on - pos).unwrap_or(0), n.pitch, n.velocity);
                    if !held.contains(&n.pitch) && held.len() < 128 {
                        held.push(n.pitch);
                    }
                }
                if off >= pos && off < end && (ons == (off == on)) {
                    inst.plugin.note_off(usize::try_from(off - pos).unwrap_or(0), n.pitch);
                    held.retain(|p| *p != n.pitch);
                }
            }
            if !ons {
                continue;
            }
            let near = s.tempo.samples_to_ticks(pos, sr).saturating_sub(base).saturating_sub(1)
                ..=s.tempo.samples_to_ticks(end, sr).saturating_sub(base).saturating_add(1);
            // Events before tick 0 are what a start trim hid: they don't play.
            for c in sequence.ctrls.iter().filter(|c| c.tick >= 0) {
                let tick = c.tick.saturating_add(delay).max(0);
                if !near.contains(&tick) {
                    continue;
                }
                let at = s.tempo.tick_to_samples(base.saturating_add(tick), sr);
                if at >= pos && at < end && at < clip.end() {
                    inst.plugin.midi(usize::try_from(at - pos).unwrap_or(0), ctrl_message(c.kind, c.value, dp));
                }
            }
        }
    }
}

/// A controller event as a MIDI 1.0 message on channel 1 (where the notes go), with `transpose`
/// applied to a poly-pressure key.
fn ctrl_message(kind: CtrlKind, value: i32, transpose: i64) -> [u8; 3] {
    let v7 = value.clamp(0, 127) as u8;
    match kind {
        CtrlKind::Cc(n) => [0xB0, n & 0x7F, v7],
        CtrlKind::PitchBend => {
            let v = value.saturating_add(8192).clamp(0, 16383);
            [0xE0, (v & 0x7F) as u8, (v >> 7) as u8]
        }
        CtrlKind::ChannelPressure => [0xD0, v7, 0],
        CtrlKind::PolyPressure(k) => [0xA0, (i64::from(k) + transpose).clamp(0, 127) as u8, v7],
        CtrlKind::Program => [0xC0, v7, 0],
    }
}

/// Before an instrument plays from `pos`: reset its controllers, then send each one the last value
/// it had on this track before `pos`.
fn chase_controllers(s: &Session, t: &Track, pos: Samples, inst: &mut PluginSlot) {
    inst.plugin.midi(0, [0xB0, 121, 0]);
    let pos = pos.saturating_sub(midi_offset(s, t));
    const BEND: usize = 128;
    const PRESSURE: usize = 129;
    const PROGRAM: usize = 130;
    let mut last = [(i64::MIN, 0i32); 131];
    let sr = s.sample_rate;
    for clip in t.clips() {
        let ClipContent::Midi { sequence } = &clip.content else { continue };
        if clip.muted || clip.start >= pos {
            continue;
        }
        let base = s.tempo.samples_to_ticks(clip.start, sr);
        let until = pos.min(clip.end());
        for c in sequence.ctrls.iter().filter(|c| c.tick >= 0) {
            let slot = match c.kind {
                CtrlKind::Cc(n) => usize::from(n & 0x7F),
                CtrlKind::PitchBend => BEND,
                CtrlKind::ChannelPressure => PRESSURE,
                CtrlKind::Program => PROGRAM,
                CtrlKind::PolyPressure(_) => continue,
            };
            let at = s.tempo.tick_to_samples(base.saturating_add(c.tick), sr);
            if at < until
                && let Some(l) = last.get_mut(slot)
                && at >= l.0
            {
                *l = (at, c.value);
            }
        }
    }
    for (slot, &(at, value)) in last.iter().enumerate() {
        let kind = match slot {
            BEND => CtrlKind::PitchBend,
            PRESSURE => CtrlKind::ChannelPressure,
            PROGRAM => CtrlKind::Program,
            n => CtrlKind::Cc(u8::try_from(n).unwrap_or(0)),
        };
        if at != i64::MIN {
            inst.plugin.midi(0, ctrl_message(kind, value, 0));
        }
    }
}

/// Session audio shared with a sample-playing instrument.
struct PoolSample(Arc<SourceAudio>);

impl SampleSource for PoolSample {
    fn sample_rate(&self) -> f32 {
        self.0.buffer.sample_rate as f32
    }
    fn num_channels(&self) -> usize {
        self.0.buffer.num_channels()
    }
    fn channel(&self, ch: usize) -> Option<&[f32]> {
        self.0.channel(ch)
    }
}

/// Hands a strip's instrument the audio of its insert's sample source when it changed (a new
/// sample, a cleared one, or a new instance). Missing media plays nothing.
fn sync_sample(strip: &mut Strip, want: Option<&soundcraft_model::Insert>, s: &Session) {
    let audio = want.and_then(|i| i.sample).and_then(|id| s.pool.get(id));
    let same = match (strip.sample.as_ref(), audio) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    if let Some(inst) = strip.instrument.as_mut() {
        let shared = audio.map(|a| Arc::new(PoolSample(Arc::clone(a))) as Arc<dyn SampleSource>);
        inst.plugin.set_sample(shared);
    }
    strip.sample = audio.cloned();
}

/// Record insert latency without processing audio, so this block can compensate for it.
fn note_insert_latency(t: &Track, strip: &mut Strip, pos: Samples) {
    let mut latency = 0usize;
    for (i, ins) in t.mixer.inserts.iter().enumerate() {
        let (Some(ins), Some(Some(slot))) = (ins, strip.plugins.get_mut(i)) else { continue };
        if !ins.active || ins.bypass {
            continue;
        }
        apply_params(slot, ins, t, i, pos);
        latency = latency.saturating_add(slot.plugin.latency());
    }
    strip.latency = latency.min(MAX_LATENCY);
}

fn apply_params(slot: &mut PluginSlot, ins: &soundcraft_model::Insert, t: &Track, i: usize, pos: Samples) {
    // Automation lanes address insert slots; the instrument (`INSTRUMENT_SLOT`) has none, so it
    // must not pick up slot A's lanes.
    let Ok(slot_u8) = u8::try_from(i) else {
        return apply_values(slot, ins, |_, v| v);
    };
    let automated = t.mixer.automation_mode.reads()
        && t.automation.iter().any(|l| matches!(&l.param, AutoParam::Plugin { slot, .. } if *slot == slot_u8) && !l.points.is_empty());
    apply_values(slot, ins, |k, val| {
        if automated
            && let Some(l) = t.automation.iter().find(|l| matches!(&l.param, AutoParam::Plugin { slot, param } if *slot == slot_u8 && param == k))
            && !l.points.is_empty()
        {
            return l.value_at(pos, val);
        }
        val
    });
}

/// Sends each stored param (as mapped by `value`) to the plugin when it changed since last time,
/// and then marks the slot [`woken`](PluginSlot::woken).
fn apply_values(slot: &mut PluginSlot, ins: &soundcraft_model::Insert, value: impl Fn(&str, f32) -> f32) {
    // First pass over a hosted plugin: it has just loaded its own settings, so a stored value equal to
    // the default it reports is not a choice and is left alone. Older sessions store every parameter
    // at that default, often 0, and pushing them over a restored plugin silenced Rhapsody (volume and
    // pan at minimum). Values that differ from the default, and every later change, are pushed.
    let first = std::mem::take(&mut slot.fresh);
    for ((k, v), last) in ins.params.iter().zip(slot.applied.values(ins.params.len()).iter_mut()) {
        let val = value(k, *v);
        if val.to_bits() != last.to_bits() {
            let untouched = first && slot.plugin.info().param(k).is_some_and(|pi| pi.default.to_bits() == val.to_bits());
            if !untouched {
                slot.plugin.set_param(k, val);
            }
            *last = val;
            slot.woken = true;
        }
    }
}

fn transport_at(s: &Session, pos: Samples, playing: bool, recording: bool) -> Transport {
    let sr = s.sample_rate;
    let ticks = Some(s.tempo.samples_to_ticks_f(pos, sr)).filter(|t| t.is_finite()).unwrap_or(0.0);
    let tick = ticks.floor() as i64;
    let meter = s.tempo.meter_at_tick(tick);
    let bar = s.tempo.bar_beat_at_tick(tick).bar;
    let q = TICKS_PER_QUARTER as f64;
    Transport {
        playing,
        recording,
        tempo: s.tempo.tempo_at_tick(tick),
        numerator: meter.numerator,
        denominator: meter.denominator,
        samples: pos,
        seconds: pos as f64 / sr.as_f64().max(1.0),
        quarters: ticks / q,
        bar_start: s.tempo.bar_start_tick(bar) as f64 / q,
        bar: bar - 1,
    }
}

fn volume_db_at(t: &Track, pos: Samples) -> f32 {
    if t.mixer.automation_mode.reads()
        && t.mixer.automation_mode != soundcraft_model::AutomationMode::Write
        && let Some(l) = t.lane(&AutoParam::Volume)
        && !l.points.is_empty()
    {
        return l.value_at(pos, t.mixer.volume_db);
    }
    t.mixer.volume_db
}

fn pan_at(t: &Track, idx: usize, pos: Samples) -> f32 {
    let base = t.mixer.pan.get(idx).copied().unwrap_or(if idx == 0 { 0.0 } else { 1.0 });
    if t.mixer.automation_mode.reads()
        && let Some(l) = t.lane(&AutoParam::Pan(u8::try_from(idx).unwrap_or(0)))
        && !l.points.is_empty()
    {
        return l.value_at(pos, base);
    }
    base
}

/// Pan automation is read on a grid of [`PAN_STEP`] timeline samples rather than once per block,
/// so playback (callback-sized blocks) and bounces (1024-frame blocks) pan identically.
const PAN_STEP: Samples = 64;

/// The position to read the pan at for the frames from `start` of the block at `pos`, and the end
/// of the frames that share it. Without pan automation the whole block shares one pan.
fn pan_segment(t: &Track, pos: Samples, start: usize, frames: usize) -> (Samples, usize) {
    let automated = t.mixer.automation_mode.reads() && t.automation.iter().any(|l| matches!(l.param, AutoParam::Pan(_)) && !l.points.is_empty());
    if !automated {
        return (pos, frames);
    }
    let here = pos.saturating_add(start as i64);
    let at = here.div_euclid(PAN_STEP).saturating_mul(PAN_STEP);
    let left = usize::try_from(at.saturating_add(PAN_STEP).saturating_sub(here)).unwrap_or(1).max(1);
    (at, start.saturating_add(left).min(frames))
}

fn send_level(t: &Track, i: usize, base: f32, pos: Samples) -> f32 {
    if t.mixer.automation_mode.reads()
        && let Some(l) = t.lane(&AutoParam::SendLevel(u8::try_from(i).unwrap_or(0)))
        && !l.points.is_empty()
    {
        return l.value_at(pos, base);
    }
    base
}

fn lerp_gain(db0: f32, db1: f32, i: usize, n: usize) -> f32 {
    let g0 = db_to_gain(db0);
    let g1 = db_to_gain(db1);
    if n == 0 { g0 } else { g0 + (g1 - g0) * (i as f32 / n as f32) }
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| if v.is_finite() { m.max(v.abs()) } else { m })
}

/// Per-block routing gains: `m[src][dst]`.
type Matrix = [[f32; MAX_CHANNELS]; MAX_CHANNELS];

/// How a source is panned: the stereo pan pair (mono uses `[0]`) and the optional surround pan.
struct PanSpec<'a> {
    stereo: [f32; 2],
    surround: Option<&'a SurroundPan>,
}

fn identity_matrix(n: usize) -> Matrix {
    let mut m = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
    for (i, row) in m.iter_mut().enumerate().take(n) {
        if let Some(g) = row.get_mut(i) {
            *g = 1.0;
        }
    }
    m
}

/// ITU-R BS.775 stereo fold-down gains (to L, to R) for one speaker: C and surrounds at -3 dB,
/// the centre surround at -6 dB per side, LFE dropped.
pub fn itu_stereo_fold(sp: Speaker) -> (f32, f32) {
    use Speaker::*;
    let h = std::f32::consts::FRAC_1_SQRT_2;
    match sp {
        L | Lc | Lw => (1.0, 0.0),
        R | Rc | Rw => (0.0, 1.0),
        C => (h, h),
        Lfe => (0.0, 0.0),
        Cs => (0.5, 0.5),
        Ls | Lss | Lrs | Ltf | Ltm | Ltr => (h, 0.0),
        Rs | Rss | Rrs | Rtf | Rtm | Rtr => (0.0, h),
    }
}

/// Speaker directions of a format (for the DSP panner). Returns the count written.
fn layout_of(fmt: ChannelFormat, out: &mut [SpeakerPos; MAX_CHANNELS]) -> usize {
    let sp = fmt.speakers();
    for (o, s) in out.iter_mut().zip(sp.iter()) {
        *o = SpeakerPos { az: s.azimuth(), el: s.elevation(), lfe: s.is_lfe() };
    }
    sp.len().min(MAX_CHANNELS)
}

fn surround_params(sp: &SurroundPan, dx: f32) -> SurroundParams {
    SurroundParams { x: (sp.x + dx).clamp(-1.0, 1.0), y: sp.y, z: sp.z, divergence: sp.divergence, center: sp.center / 100.0 }
}

/// Routing gains from a source (`ns` channels of `src_fmt`) into a destination (`nd` channels of
/// `dst_fmt`). See the crate docs for the rules.
fn build_matrix(src_fmt: ChannelFormat, ns: usize, dst_fmt: ChannelFormat, nd: usize, pan: &PanSpec<'_>, law: PanLaw) -> Matrix {
    let mut m = [[0.0f32; MAX_CHANNELS]; MAX_CHANNELS];
    let (ns, nd) = (ns.min(MAX_CHANNELS), nd.min(MAX_CHANNELS));
    if ns == 0 || nd == 0 {
        return m;
    }
    let set = |m: &mut Matrix, s: usize, d: usize, g: f32| {
        if let Some(x) = m.get_mut(s).and_then(|r| r.get_mut(d)) {
            *x += g;
        }
    };
    if nd == 1 {
        // Mono destination: the stereo routing summed at -6 dB.
        let st = build_matrix(src_fmt, ns, ChannelFormat::Stereo, 2, pan, law);
        for (row, srow) in m.iter_mut().zip(st.iter()).take(ns) {
            row[0] = 0.5 * (srow[0] + srow[1]);
        }
        return m;
    }
    let src_sp = src_fmt.speakers();
    let src_amb = src_fmt.is_ambisonic() && ns >= 4;
    if nd == 2 && !dst_fmt.is_ambisonic() {
        match ns {
            1 => {
                let (gl, gr) = gains(pan.stereo[0], law);
                m[0][0] = gl;
                m[0][1] = gr;
            }
            2 if !src_amb => {
                let (a_l, a_r) = gains(pan.stereo[0], PanLaw::Zero);
                let (b_l, b_r) = gains(pan.stereo[1], PanLaw::Zero);
                m[0] = [0.0; MAX_CHANNELS];
                m[0][0] = a_l;
                m[0][1] = a_r;
                m[1][0] = b_l;
                m[1][1] = b_r;
            }
            _ if src_amb => {
                for (d, az) in [(0usize, -30.0f32), (1, 30.0)] {
                    let g = soundcraft_dsp::pan::ambisonic_decode(az, 0.0, 2);
                    for (c, gc) in g.iter().enumerate() {
                        set(&mut m, c, d, *gc);
                    }
                }
            }
            _ if src_sp.len() == ns => {
                for (s, sp) in src_sp.iter().enumerate() {
                    let (l, r) = itu_stereo_fold(*sp);
                    set(&mut m, s, 0, l);
                    set(&mut m, s, 1, r);
                }
            }
            _ => {
                // Unknown layout: first two channels to L/R, the rest to the centre.
                let h = std::f32::consts::FRAC_1_SQRT_2;
                m[0][0] = 1.0;
                m[1][1] = 1.0;
                for row in m.iter_mut().take(ns).skip(2) {
                    row[0] = h;
                    row[1] = h;
                }
            }
        }
        return m;
    }
    if ns > 2 && src_fmt == dst_fmt && ns == nd {
        return identity_matrix(ns);
    }
    // Multichannel destination.
    let mut layout = [SpeakerPos::default(); MAX_CHANNELS];
    let nl = layout_of(dst_fmt, &mut layout);
    let layout = layout.get(..nl).unwrap_or(&[]);
    let dst_sp = dst_fmt.speakers();
    let dst_amb = dst_fmt.is_ambisonic();
    let find = |sp: Speaker| dst_sp.iter().position(|x| *x == sp).filter(|i| *i < nd);
    let mut g = [0.0f32; MAX_CHANNELS];
    if dst_amb {
        if src_amb || (src_fmt.is_ambisonic() && ns > 2) {
            return identity_matrix(ns.min(nd));
        }
        let mut enc = |m: &mut Matrix, s: usize, az: f32, el: f32, spread: f32| {
            soundcraft_dsp::pan::ambisonic_encode(az, el, spread, &mut g);
            for (d, gd) in g.iter().enumerate().take(nd) {
                set(m, s, d, *gd);
            }
        };
        match (ns, pan.surround) {
            (1 | 2, Some(sp)) => {
                for s in 0..ns {
                    let dx = if ns == 2 { if s == 0 { -1.0 } else { 1.0 } } else { 0.0 };
                    let p = surround_params(sp, dx);
                    enc(&mut m, s, soundcraft_dsp::pan::puck_azimuth(&[], p.x, p.y), p.z * 90.0, p.divergence);
                }
            }
            (1, None) => enc(&mut m, 0, pan.stereo[0].clamp(-1.0, 1.0) * 30.0, 0.0, 0.0),
            (2, None) => {
                enc(&mut m, 0, -30.0, 0.0, 0.0);
                enc(&mut m, 1, 30.0, 0.0, 0.0);
            }
            _ => {
                for (s, sp) in src_sp.iter().enumerate().take(ns) {
                    if !sp.is_lfe() {
                        enc(&mut m, s, sp.azimuth(), sp.elevation(), 0.0);
                    }
                }
            }
        }
        return m;
    }
    if dst_sp.len() != nd {
        // No layout to pan on: channel for channel.
        return identity_matrix(ns.min(nd));
    }
    let (li, ri) = (find(Speaker::L), find(Speaker::R));
    match (ns, pan.surround) {
        (1 | 2, Some(sp)) if !src_amb => {
            for s in 0..ns {
                let dx = if ns == 2 { if s == 0 { -1.0 } else { 1.0 } } else { 0.0 };
                soundcraft_dsp::pan::surround_gains(layout, &surround_params(sp, dx), &mut g);
                for (d, gd) in g.iter().enumerate().take(nd) {
                    set(&mut m, s, d, *gd);
                }
                if let Some(lfe) = find(Speaker::Lfe)
                    && sp.lfe_db > -143.9
                {
                    set(&mut m, s, lfe, db_to_gain(sp.lfe_db.min(12.0)) / ns as f32);
                }
            }
        }
        (1, None) => {
            let (gl, gr) = gains(pan.stereo[0], law);
            if let (Some(l), Some(r)) = (li, ri) {
                set(&mut m, 0, l, gl);
                set(&mut m, 0, r, gr);
            }
        }
        (2, None) if !src_amb => {
            let (a_l, a_r) = gains(pan.stereo[0], PanLaw::Zero);
            let (b_l, b_r) = gains(pan.stereo[1], PanLaw::Zero);
            if let (Some(l), Some(r)) = (li, ri) {
                set(&mut m, 0, l, a_l);
                set(&mut m, 0, r, a_r);
                set(&mut m, 1, l, b_l);
                set(&mut m, 1, r, b_r);
            }
        }
        _ if src_amb => {
            let n_spk = dst_sp.iter().filter(|s| !s.is_lfe()).count();
            for (d, sp) in dst_sp.iter().enumerate().take(nd) {
                if sp.is_lfe() {
                    continue;
                }
                let dec = soundcraft_dsp::pan::ambisonic_decode(sp.azimuth(), sp.elevation(), n_spk);
                for (c, gc) in dec.iter().enumerate() {
                    set(&mut m, c, d, *gc);
                }
            }
        }
        _ if src_sp.len() == ns => {
            for (s, sp) in src_sp.iter().enumerate() {
                if let Some(d) = find(*sp) {
                    set(&mut m, s, d, 1.0);
                } else if sp.is_lfe() {
                    // No LFE in the destination: dropped.
                } else {
                    soundcraft_dsp::pan::direction_gains(layout, sp.azimuth(), sp.elevation(), &mut g);
                    for (d, gd) in g.iter().enumerate().take(nd) {
                        set(&mut m, s, d, *gd);
                    }
                }
            }
        }
        _ => return identity_matrix(ns.min(nd)),
    }
    m
}

/// `dst[d] += Σ_s src[s] × gain × m[s][d]` for every destination channel (no allocation).
fn apply_matrix(src: &[Vec<f32>], frames: usize, dst: &mut [Vec<f32>], m: &Matrix, gain: f32) {
    apply_matrix_span(src, 0..frames, dst, m, gain);
}

/// [`apply_matrix`] over the frames in `span` only.
fn apply_matrix_span(src: &[Vec<f32>], span: std::ops::Range<usize>, dst: &mut [Vec<f32>], m: &Matrix, gain: f32) {
    let ns = src.len().min(MAX_CHANNELS);
    for (d, out) in dst.iter_mut().enumerate().take(MAX_CHANNELS) {
        let mut idx = [(0usize, 0.0f32); MAX_CHANNELS];
        let mut k = 0;
        for (s, row) in m.iter().enumerate().take(ns) {
            let g = row.get(d).copied().unwrap_or(0.0);
            if g != 0.0
                && let Some(slot) = idx.get_mut(k)
            {
                *slot = (s, g);
                k += 1;
            }
        }
        let taps = idx.get(..k).unwrap_or(&[]);
        if taps.is_empty() {
            continue;
        }
        for (i, o) in out.iter_mut().enumerate().take(span.end).skip(span.start) {
            let mut acc = 0.0f32;
            for &(s, g) in taps {
                acc += src.get(s).and_then(|c| c.get(i)).copied().unwrap_or(0.0) * gain * g;
            }
            *o += acc;
        }
    }
}

/// Ceiling on one in-memory offline render (samples summed over channels): 2^29 f32 = 2 GiB, about
/// 93 minutes of stereo at 48 kHz. Longer requests are truncated rather than exhausting memory;
/// [`RangeRender`] streams any length.
pub const MAX_RENDER_SAMPLES: usize = 1 << 29;

/// Offline render of the main mix over `range` (planar, [`main_channels`] channels in SMPTE/WAV
/// order). The mix's delay-compensation latency is removed, so the result lines up with the
/// timeline.
pub fn render_range(s: &Session, range: Range, block: usize) -> Vec<Vec<f32>> {
    render_in_memory(s, None, range, block)
}

/// Offline render of one bus over `range` (planar, the bus format's channel count).
/// The compensation delay written onto the bus is removed (track inserts, and the aux returns
/// when an aux returns onto it), so the bus lines up with the timeline. An unknown bus is silent.
pub fn render_bus(s: &Session, bus: BusId, range: Range, block: usize) -> Vec<Vec<f32>> {
    render_in_memory(s, Some(bus), range, block)
}

fn render_in_memory(s: &Session, bus: Option<BusId>, range: Range, block: usize) -> Vec<Vec<f32>> {
    let nch = source_channels(s, bus);
    let len = usize::try_from(range.len().max(0)).unwrap_or(0).min(MAX_RENDER_SAMPLES / nch.max(1));
    let mut out: Vec<Vec<f32>> = (0..nch).map(|_| Vec::with_capacity(len)).collect();
    let mut render = RangeRender::new(s, bus, Range::new(range.start, range.start + len as i64), block);
    while let Some((chunk, n)) = render.next_block(s) {
        for (o, c) in out.iter_mut().zip(chunk) {
            o.extend_from_slice(c.get(..n).unwrap_or_default());
        }
    }
    out
}

/// Channels of the main mix, or of `bus` (none for an unknown bus).
pub fn source_channels(s: &Session, bus: Option<BusId>) -> usize {
    match bus {
        Some(id) => s.bus(id).map_or(0, |b| b.format.channels()),
        None => main_channels(s),
    }
}

/// [`render_range`] or [`render_bus`] of any length, a block at a time.
pub struct RangeRender {
    eng: MixEngine,
    bus: Option<BusId>,
    tmp: Vec<Vec<f32>>,
    start: Samples,
    done: usize,
    total: usize,
    latency: usize,
}

impl RangeRender {
    /// Renders the main mix over `range`, or `bus` when one is named.
    pub fn new(s: &Session, bus: Option<BusId>, range: Range, block: usize) -> Self {
        let nch = source_channels(s, bus);
        let eng = MixEngine::new(s.sample_rate.as_f64() as f32, block);
        let tmp = vec![vec![0.0f32; eng.max_block()]; nch];
        let len = if nch == 0 { 0 } else { usize::try_from(range.len().max(0)).unwrap_or(0) };
        RangeRender { eng, bus, tmp, start: range.start, done: 0, total: len, latency: 0 }
    }

    fn render(&mut self, s: &Session, pos: Samples, n: usize) {
        match self.bus {
            Some(id) => {
                self.eng.render(s, pos, n, &mut []);
                self.eng.copy_bus(id, n, &mut self.tmp);
            }
            None => self.eng.render(s, pos, n, &mut self.tmp),
        }
    }

    /// Renders one more block of the mix of `s` (the session given to [`RangeRender::new`]) and
    /// returns it as `(channels, n)`: the next `n` samples of the range lead each channel (none
    /// while the mix's latency is still being skipped). `None` once the range is done.
    pub fn next_block(&mut self, s: &Session) -> Option<(&[Vec<f32>], usize)> {
        if self.done >= self.total {
            return None;
        }
        let n = (self.total - self.done).min(self.eng.max_block());
        self.render(s, self.start + self.done as i64, n);
        if self.done == 0 {
            // The first block tells the mix's latency: render that much more and drop the head.
            self.latency = match self.bus {
                Some(b) => self.eng.bus_latency(b),
                None => self.eng.latency,
            };
            self.total = self.total.saturating_add(self.latency);
        }
        let skip = self.latency.saturating_sub(self.done).min(n);
        self.done += n;
        if skip > 0 && skip < n {
            for c in &mut self.tmp {
                c.copy_within(skip..n, 0);
            }
        }
        Some((&self.tmp, n - skip))
    }
}

/// ITU-R BS.775 stereo fold-down of planar audio in `fmt` (C and surrounds at -3 dB, LFE dropped;
/// Ambisonics play their W channel). Mono/stereo input is returned unchanged.
pub fn fold_down_stereo(fmt: ChannelFormat, ch: &[Vec<f32>]) -> Vec<Vec<f32>> {
    if ch.len() <= 2 {
        return ch.to_vec();
    }
    let len = ch.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = vec![vec![0.0f32; len]; 2];
    let sp = fmt.speakers();
    let h = std::f32::consts::FRAC_1_SQRT_2;
    for (c, src) in ch.iter().enumerate() {
        let (gl, gr) = match sp.get(c) {
            Some(x) if sp.len() == ch.len() => itu_stereo_fold(*x),
            _ if c == 0 => (h, h),
            _ => (0.0, 0.0),
        };
        for (g, dst) in [(gl, 0usize), (gr, 1)] {
            if g == 0.0 {
                continue;
            }
            if let Some(d) = out.get_mut(dst) {
                for (o, x) in d.iter_mut().zip(src.iter()) {
                    *o += x * g;
                }
            }
        }
    }
    out
}

/// Render only one track's clips (no mixer processing) — Consolidate.
pub fn render_clips(s: &Session, track: TrackId, range: Range) -> Vec<Vec<f32>> {
    let Some(t) = s.track(track) else { return Vec::new() };
    let len = usize::try_from(range.len().max(0)).unwrap_or(0).min(MAX_RENDER_SAMPLES / t.channels().max(1));
    let mut out = vec![vec![0.0f32; len]; t.channels().max(1)];
    let mut scratch = Vec::new();
    for c in t.clips().iter().filter(|c| !c.muted && c.range().overlaps(&range)) {
        match clip_fx_params(s, c.id.0) {
            None => render_clip(s, c, range.start, len, &mut out, &mut scratch),
            Some(params) => {
                let mut tmp = vec![vec![0.0f32; len]; out.len()];
                render_clip(s, c, range.start, len, &mut tmp, &mut scratch);
                let mut fx = ClipFx::new(s.sample_rate.as_f64() as f32, tmp.len(), true);
                fx.configure(params);
                let mut done = 0;
                while done < len {
                    let n = (len - done).min(4096);
                    let mut block: Vec<Vec<f32>> = tmp.iter().map(|ch| ch.get(done..done + n).map(<[f32]>::to_vec).unwrap_or_default()).collect();
                    fx.process(&mut block, n);
                    for (o, b) in out.iter_mut().zip(block.iter()) {
                        if let Some(dst) = o.get_mut(done..done + n) {
                            for (x, y) in dst.iter_mut().zip(b.iter()) {
                                *x += *y;
                            }
                        }
                    }
                    done += n;
                }
            }
        }
    }
    out
}

/// Render one track through its inserts (pre-fader) — Commit / Freeze.
pub fn render_track_pre_fader(s: &Session, track: TrackId, range: Range) -> Vec<Vec<f32>> {
    let mut solo = s.clone();
    // Multichannel tracks render into a main mix of their own format (straight through).
    let fmt = s.track(track).map_or(ChannelFormat::Stereo, |t| if t.channels() > 2 { t.format } else { ChannelFormat::Stereo });
    solo.outputs = vec![soundcraft_model::OutputPath { name: "Out".into(), first_channel: 0, format: fmt }];
    for t in &mut solo.tracks {
        if t.id == track {
            t.mixer.volume_db = 0.0;
            t.mixer.mute = false;
            t.mixer.solo = false;
            t.mixer.output = Route::Main;
            t.mixer.pan = match t.channels() {
                1 => vec![0.0],
                _ => vec![-1.0, 1.0],
            };
            t.automation.retain(|l| matches!(l.param, AutoParam::Plugin { .. }));
            t.mixer.sends.iter_mut().for_each(|x| *x = None);
            t.mixer.surround = None;
        } else {
            t.inactive = true;
        }
    }
    let mut out = render_range(&solo, range, 1024);
    // Mono tracks: fold back to one channel (undo the centre pan law).
    if s.track(track).is_some_and(|t| t.channels() == 1)
        && let Some(l) = out.first().cloned()
    {
        let g = std::f32::consts::SQRT_2;
        out = vec![l.iter().map(|x| x * g).collect()];
    }
    out
}

/// A built-in plugin, else a hosted CLAP plugin (`clap:<id>`).
fn create_plugin(id: &str) -> Option<Box<dyn Plugin>> {
    // Tests never load real plug-ins; adopt a fake instead. Scanning the machine's plug-in folders
    // loads every installed plug-in in-process, and some never unload, so a test that reached the
    // real hosts would hang on a developer machine (CI has none installed).
    #[cfg(test)]
    if is_third_party(id) {
        return None;
    }
    soundcraft_dsp::create(id)
        .or_else(|| soundcraft_clap_host::create(id))
        .or_else(|| soundcraft_vst3_host::create(id))
        .or_else(|| soundcraft_au_host::create(id))
}

#[cfg(test)]
mod audio_thread_tests {
    use super::*;

    #[test]
    fn only_a_marked_thread_is_the_audio_thread() {
        assert!(!on_audio_thread());
        let marked = std::thread::spawn(|| {
            let before = on_audio_thread();
            mark_audio_thread();
            (before, on_audio_thread())
        })
        .join()
        .unwrap();
        assert_eq!(marked, (false, true));
        assert!(!on_audio_thread(), "marking another thread leaves this one alone");
    }

    /// What `on_audio_thread` says inside each strip job, for strips run from a thread that is
    /// (or is not) the audio thread.
    fn seen_by_strips(audio: bool) -> Vec<bool> {
        jobs(audio).into_iter().map(|j| j.audio).collect()
    }

    /// Where one strip job ran.
    #[derive(Clone, Copy, Default)]
    struct Job {
        audio: bool,
        /// On a worker of Rayon's global pool.
        global: bool,
        /// On a worker of the render pool.
        render: bool,
    }

    fn jobs(audio: bool) -> Vec<Job> {
        init_render_pool();
        std::thread::spawn(move || {
            if audio {
                mark_audio_thread();
            }
            let mut work = vec![Job::default(); 64];
            run_strips(&mut work, true, |w| {
                // Long enough that the pool's workers take part.
                std::thread::sleep(std::time::Duration::from_millis(1));
                let render = std::thread::current().name().is_some_and(|n| n.starts_with("mix-render-"));
                let global = rayon::current_thread_index().is_some() && !render;
                *w = Job { audio: on_audio_thread(), global, render };
            });
            work
        })
        .join()
        .unwrap()
    }

    #[test]
    fn strips_rendered_for_the_audio_thread_count_as_the_audio_thread_and_only_then() {
        assert!(seen_by_strips(true).iter().all(|&rt| rt), "a worker rendering for the audio thread is on it");
        assert!(seen_by_strips(false).iter().all(|&rt| !rt), "the workers don't stay marked afterwards");
    }

    /// ADR 0012 gate G1: a render on the audio thread dispatches no job to the global pool.
    #[test]
    fn audio_thread_strips_never_reach_the_global_rayon_pool() {
        let _switch = SERIAL_SWITCH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        MixEngine::force_serial(false); // the pool is opt-in; this tests where it runs
        let audio = jobs(true);
        assert!(audio.iter().all(|j| !j.global), "an audio-thread strip ran on the global pool");
        if RENDER_POOL.get().is_some_and(Option::is_some) {
            assert!(audio.iter().any(|j| j.render), "with a render pool, the strips fan out to it");
        }
        let offline = jobs(false);
        MixEngine::force_serial(true); // back to the default
        assert!(offline.iter().all(|j| !j.render), "an offline render never takes the render pool");
    }

    /// Serializes the tests that depend on the process-wide serial switch.
    static SERIAL_SWITCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The F01 safety valve: with the switch on, the audio thread renders every strip itself;
    /// off again, the pool takes them back.
    #[test]
    fn force_serial_keeps_audio_thread_strips_on_the_audio_thread() {
        let _switch = SERIAL_SWITCH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        MixEngine::force_serial(true);
        let forced = jobs(true);
        let was_forced = MixEngine::serial_forced();
        MixEngine::force_serial(false);
        assert!(was_forced);
        assert!(forced.iter().all(|j| j.audio && !j.render && !j.global), "a strip left the audio thread while forced serial");
        if RENDER_POOL.get().is_some_and(Option::is_some) {
            assert!(jobs(true).iter().any(|j| j.render), "the pool is used again once the switch is off");
        }
        MixEngine::force_serial(true); // back to the default
    }

    /// Sixteen strips of distinct deterministic material, levels, pans and built-in inserts.
    fn sixteen_strips() -> Session {
        use soundcraft_audio_io::AudioBuffer;
        use soundcraft_model::{Insert, SourceAudio, SourceId};
        let mut s = Session::default();
        let len = 48_000usize;
        for k in 0..16u32 {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            let f = 0.003 + 0.0007 * k as f32;
            let signal: Vec<f32> = (0..len).map(|i| (i as f32 * f).sin() * 0.2 + ((i * (k as usize + 3)) % 97) as f32 * 1e-3).collect();
            let src = SourceId(1000 + u64::from(k));
            s.pool.insert(src, std::sync::Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![signal] })));
            let id = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "x", src, 0, 0, len as i64));
            tr.mixer.volume_db = -(k as f32) * 0.5;
            tr.mixer.pan = vec![(k as f32 / 7.5) - 1.0];
            tr.mixer.inserts[0] = Some(Insert::new(if k % 2 == 0 { "compressor" } else { "room_reverb" }));
        }
        s
    }

    /// Render `blocks` blocks of 512 on a fresh thread, marked as the audio thread or not.
    fn render_blocks(s: &Session, serial: bool, audio: bool, blocks: i64) -> Vec<Vec<f32>> {
        let s = s.clone();
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.serial = serial;
        std::thread::spawn(move || {
            if audio {
                mark_audio_thread();
            }
            let mut all = vec![Vec::new(); 2];
            let mut out = vec![vec![0.0f32; 512]; 2];
            for b in 0..blocks {
                eng.render(&s, b * 512, 512, &mut out);
                for (a, o) in all.iter_mut().zip(&out) {
                    a.extend_from_slice(o);
                }
            }
            all
        })
        .join()
        .unwrap()
    }

    #[test]
    fn parallel_and_serial_renders_are_bit_identical() {
        let s = sixteen_strips();
        let serial = render_blocks(&s, true, false, 40);
        assert!(serial[0].iter().any(|x| x.abs() > 1e-3), "the fixture should sound");
        assert!(serial == render_blocks(&s, false, true, 40), "render pool differs from serial");
        assert!(serial == render_blocks(&s, false, false, 40), "global pool differs from serial");
        assert!(serial == render_blocks(&s, true, true, 40), "serial on the audio thread differs");
    }
}

#[cfg(test)]
mod tests {
    //! Rule: tests never call the real plug-in hosts. Third-party ids are not created here
    //! (create_plugin returns None under cfg(test)); a test that needs a hosted plug-in
    //! adopts a fake under a clap:test.* id via MixEngine::adopt.

    use super::*;
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::{Insert, SourceAudio, SourceId};
    use std::sync::Arc;

    fn synth_session(level_db: f32) -> Session {
        use soundcraft_midi::{Note, Sequence};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let mut seq = Sequence::default();
        seq.notes.push(Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 960 });
        let cid = s.new_clip_id();
        let tr = s.track_mut(t).unwrap();
        tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 24_000, seq));
        let mut ins = Insert::new("subtractive_synth");
        ins.params.insert("level".into(), level_db);
        tr.instrument = Some(ins);
        s
    }

    #[test]
    fn stored_instrument_params_reach_the_plugin() {
        // #152: a saved instrument level of -12 dB must render quieter than -6 dB.
        // RMS over the held part of the note, past any parameter smoothing.
        let rms = |db: f32| {
            let out = render_range(&synth_session(db), Range::new(0, 12_000), 512);
            let tail = out[0].get(6_000..).unwrap_or_default();
            (tail.iter().map(|x| x * x).sum::<f32>() / tail.len().max(1) as f32).sqrt()
        };
        let (loud, quiet) = (rms(-6.0), rms(-12.0));
        assert!(loud > 0.0, "the synth should sound");
        assert!((quiet / loud - 0.5).abs() < 0.05, "-12 dB vs -6 dB should halve the level: {quiet} / {loud}");
    }

    #[test]
    fn live_notes_sound_while_stopped_without_the_clips() {
        let s = synth_session(-6.0);
        let t = s.tracks[0].id;
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.monitor_only = true;
        let mut out = vec![vec![0.0f32; 512]; 2];
        let peak = |out: &[Vec<f32>]| out.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
        let mut loudest = 0.0f32;
        for _ in 0..8 {
            eng.render(&s, 0, 512, &mut out);
            loudest = loudest.max(peak(&out));
        }
        assert_eq!(loudest, 0.0, "stopped: the clip's note must not play");
        eng.live_note(t, 72, 100);
        for _ in 0..8 {
            eng.render(&s, 0, 512, &mut out);
            loudest = loudest.max(peak(&out));
        }
        assert!(loudest > 0.01, "the auditioned note should sound: {loudest}");
        eng.live_note(t, 72, 0);
        for _ in 0..400 {
            eng.render(&s, 0, 512, &mut out);
        }
        assert!(peak(&out) < 1e-4, "the released note should die away: {}", peak(&out));
    }

    #[test]
    fn insert_slot_a_automation_does_not_drive_the_instrument() {
        let mut s = synth_session(-6.0);
        let reference = render_range(&s, Range::new(0, 12_000), 512);
        let tr = &mut s.tracks[0];
        let mut lane = soundcraft_model::AutomationLane::new(AutoParam::Plugin { slot: 0, param: "level".into() });
        lane.set_point(0, -48.0);
        tr.automation.push(lane);
        assert_eq!(render_range(&s, Range::new(0, 12_000), 512), reference);
    }

    fn session_with_dc(level: f32, frames: usize) -> (Session, TrackId) {
        let mut s = Session::default();
        let t = add_dc_track(&mut s, level, frames);
        (s, t)
    }

    fn add_dc_track(s: &mut Session, level: f32, frames: usize) -> TrackId {
        // A mono strip carrying an instrument (the one-channel buffer is what takes the scratch path).
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; frames]] };
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(buf)));
        let id = s.new_clip_id();
        if let Some(pl) = s.track_mut(t).and_then(|t| t.playlist_mut()) {
            pl.clips.push(Clip::audio(id, "dc", SourceId(900), 0, 0, frames as i64));
        }
        t
    }

    #[test]
    fn every_builtin_parameter_map_is_tracked_inline() {
        for info in soundcraft_dsp::plugins() {
            assert!(info.params.len() <= APPLIED_INLINE, "{}: {} parameters", info.id, info.params.len());
        }
        let mut a = Applied::new();
        assert!(a.values(3).iter().all(|x| x.is_nan()));
        a.values(3).fill(1.0);
        assert_eq!(a.values(3), [1.0; 3], "kept while the map's length stays");
        assert_eq!(a.values(APPLIED_INLINE + 1).len(), APPLIED_INLINE + 1, "a longer map spills to the heap");
    }

    #[test]
    fn clip_effect_slots_round_trip_and_never_collide() {
        for clip in [0u64, 1, 7, 1 << 20] {
            for comp in [false, true] {
                let slot = clip_fx_slot(clip, comp).unwrap();
                assert_eq!(clip_fx_of_slot(slot), Some((clip, comp)));
                assert_ne!(slot, INSTRUMENT_SLOT);
            }
        }
        assert_eq!(clip_fx_of_slot(INSTRUMENT_SLOT), None);
        assert_eq!(clip_fx_of_slot(3), None, "insert slots are not clip-effect slots");
        assert_eq!(clip_fx_slot(u64::MAX, false), None);
    }

    #[test]
    fn realtime_specs_list_builtins_and_clip_effects_and_instance_specs_only_hosted_ones() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("room_reverb"));
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.x"));
        let clip = s.track(t).unwrap().clips()[0].id.0;
        s.edit.values.insert(format!("clip_fx.{clip}.comp.threshold"), -20.0);
        let specs = realtime_instance_specs(&s);
        let slots: Vec<(usize, &str)> = specs.iter().map(|x| (x.slot, x.id.as_str())).collect();
        assert_eq!(slots, [(0, "room_reverb"), (1, "clap:test.x"), (clip_fx_slot(clip, true).unwrap(), CLIP_COMP)]);
        assert!(specs.iter().all(|x| x.state.is_none()));
        assert_eq!(instance_specs(&s).iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["clap:test.x"]);
    }

    #[test]
    fn a_realtime_engine_creates_no_builtin_and_plays_adopted_ones() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let mut ins = Insert::new("gain");
        ins.params.insert("gain".into(), -12.0);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        let render = |eng: &mut MixEngine| {
            let mut out = vec![vec![0.0; 256]; 2];
            (0..8)
                .flat_map(|b| {
                    eng.render(&s, b * 256, 256, &mut out);
                    out[0].clone()
                })
                .collect::<Vec<f32>>()
        };
        let realtime = || {
            let mut eng = MixEngine::new(48_000.0, 256);
            eng.set_external_instances(true);
            eng
        };
        let reference = render(&mut MixEngine::new(48_000.0, 256));
        let dry = render(&mut realtime());
        let tail = |x: &[f32]| peak(x.get(x.len() - 256..).unwrap_or_default());
        assert!(tail(&dry) > tail(&reference) * 2.0, "nothing adopted: the slot passes audio through");
        let mut eng = realtime();
        let spec = realtime_instance_specs(&s).remove(0);
        let plugin = create_instance(&spec.id, None, 48_000.0, 256, spec.ch).unwrap().plugin;
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: spec.id, ch: spec.ch, plugin }]);
        assert_eq!(render(&mut eng), reference, "the adopted gain plays as the offline engine's own");
        assert_eq!(eng.take_retired().capacity(), 0, "nothing retired");
        // The engine keeps the host's (now empty) vector until the next hand-over returns it.
        eng.adopt(Vec::new());
        let back = eng.take_retired();
        assert!(back.is_empty() && back.capacity() > 0, "the emptied hand-over vector comes back to the host");
    }

    #[test]
    fn mono_center_pan_is_minus_3db() {
        let (s, _) = session_with_dc(0.5, 4800);
        let out = render_range(&s, Range::new(0, 4800), 512);
        let expect = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0][1000] - expect).abs() < 1e-3, "{}", out[0][1000]);
        assert!((out[1][1000] - expect).abs() < 1e-3);
    }

    #[test]
    fn fader_mute_and_solo() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.track_mut(t).unwrap().mixer.volume_db = -6.0;
        let out = render_range(&s, Range::new(0, 4800), 512);
        let expect = 0.5 * std::f32::consts::FRAC_1_SQRT_2 * db_to_gain(-6.0);
        assert!((out[0][2000] - expect).abs() < 1e-3);
        s.track_mut(t).unwrap().mixer.mute = true;
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][2000].abs() < 1e-6);
        // Soloing another track mutes this one.
        s.track_mut(t).unwrap().mixer.mute = false;
        let other = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(other).unwrap().mixer.solo = true;
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][2000].abs() < 1e-6);
    }

    #[test]
    fn hard_pans() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.track_mut(t).unwrap().mixer.pan = vec![-1.0];
        let out = render_range(&s, Range::new(0, 4800), 256);
        assert!(out[0][100] > 0.49 && out[1][100].abs() < 1e-4);
    }

    #[test]
    fn fade_curvature_reaches_the_rendered_audio() {
        for fade_in in [true, false] {
            for curvature in [-0.5, 0.0, 0.5] {
                let (mut s, t) = session_with_dc(1.0, 8192);
                s.track_mut(t).unwrap().mixer.pan = vec![-1.0];
                let fade = soundcraft_model::Fade { len: 8192, shape: soundcraft_model::FadeShape::SCurve, curvature };
                let c = &mut s.track_mut(t).unwrap().playlist_mut().unwrap().clips[0];
                if fade_in {
                    c.fade_in = fade;
                } else {
                    c.fade_out = fade;
                }
                let out = render_range(&s, Range::new(0, 8192), 512);
                let expected = if curvature < 0.0 {
                    0.2
                } else if curvature > 0.0 {
                    0.8
                } else {
                    0.5
                };
                assert!((out[0][4096] - expected).abs() < 1e-5, "{fade_in}, {curvature}: {} vs {expected}", out[0][4096]);
                assert!(out[1][4096].abs() < 1e-6);
                assert!(out[0][if fade_in { 0 } else { 8191 }].abs() < 1e-4);
            }
        }
    }

    #[test]
    fn pan_automation_does_not_depend_on_the_block_size() {
        // Bounces render in 1024-frame blocks, playback in whatever its callback asks for (#244).
        let (mut s, t) = session_with_dc(0.5, 4800);
        {
            let l = s.track_mut(t).unwrap().lane_mut(&AutoParam::Pan(0));
            l.set_point(0, -1.0);
            l.set_point(480, 1.0);
        }
        let bounce = render_range(&s, Range::new(0, 4800), 1024);
        assert!(bounce[0][600].abs() < 1e-4 && bounce[1][600] > 0.49, "{} {}", bounce[0][600], bounce[1][600]);
        for block in [512, 100, 37] {
            assert_eq!(render_range(&s, Range::new(0, 4800), block), bounce, "block {block}");
        }
    }

    #[test]
    fn fades_and_clip_gain_apply() {
        let (mut s, t) = session_with_dc(1.0, 48_000);
        {
            let c = &mut s.track_mut(t).unwrap().playlist_mut().unwrap().clips[0];
            c.fade_in = soundcraft_model::Fade { len: 1000, shape: soundcraft_model::FadeShape::Linear, curvature: 0.0 };
            c.gain_db = -6.0;
        }
        let out = render_range(&s, Range::new(0, 2000), 512);
        let g = std::f32::consts::FRAC_1_SQRT_2 * db_to_gain(-6.0);
        assert!(out[0][0].abs() < 1e-3);
        assert!((out[0][500] - 0.5 * g).abs() < 0.02, "{}", out[0][500]);
        assert!((out[0][1500] - g).abs() < 1e-3);
    }

    #[test]
    fn sends_feed_aux_via_bus() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let bus = s.add_bus("Verb", ChannelFormat::Stereo);
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Verb"));
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        {
            let tr = s.track_mut(t).unwrap();
            tr.mixer.output = Route::None;
            let mut snd = soundcraft_model::SendSlot::new(Route::Bus(bus));
            snd.level_db = 0.0;
            tr.mixer.sends[0] = Some(snd);
        }
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][1000] > 0.1, "aux should carry the send: {}", out[0][1000]);
    }

    #[test]
    fn render_bus_is_the_bus_not_the_main_mix() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let stem = s.add_bus("Stem", ChannelFormat::Stereo);
        let empty = s.add_bus("Empty", ChannelFormat::Stereo);
        s.track_mut(t).unwrap().mixer.output = Route::Bus(stem);
        let main = render_range(&s, Range::new(0, 4800), 512);
        let bus = render_bus(&s, stem, Range::new(0, 4800), 512);
        let silent = render_bus(&s, empty, Range::new(0, 4800), 512);
        assert!(main[0][1000].abs() < 1e-4, "main should not carry a track routed to the bus: {}", main[0][1000]);
        assert!(bus[0][1000].abs() > 0.1, "bus should carry the track: {}", bus[0][1000]);
        assert!(silent.iter().all(|c| c.iter().all(|x| x.abs() < 1e-6)));
    }

    #[test]
    fn volume_automation_ramps() {
        let (mut s, t) = session_with_dc(0.5, 48_000);
        {
            let tr = s.track_mut(t).unwrap();
            let l = tr.lane_mut(&AutoParam::Volume);
            l.set_point(0, 0.0);
            l.set_point(24_000, -144.0);
        }
        let out = render_range(&s, Range::new(0, 48_000), 512);
        assert!(out[0][100] > 0.3);
        assert!(out[0][30_000].abs() < 1e-4);
    }

    #[test]
    fn inserts_process_and_survive_unknown_plugins() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("does-not-exist"));
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][100] > 0.3);
        let id = soundcraft_dsp::plugins().iter().find(|p| p.id.contains("gain")).map(|p| p.id).unwrap_or("gain");
        let mut ins = Insert::new(id);
        ins.params.insert("gain".into(), -6.0);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        let _ = render_range(&s, Range::new(0, 4800), 512);
    }

    /// A built-in gain behind a third-party id, with a log of parameter sets.
    struct Stateful {
        inner: Box<dyn Plugin>,
        blob: Vec<u8>,
        sets: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Plugin for Stateful {
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
            self.sets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.inner.process(io, frames);
        }
        fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
            let blob = self.blob.clone();
            Some(Box::new(move || Some(blob.clone())))
        }
    }

    struct Clock {
        inner: Box<dyn Plugin>,
        seen: Arc<std::sync::Mutex<Vec<Transport>>>,
    }

    impl Plugin for Clock {
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
            self.inner.process(io, frames);
        }
        fn set_transport(&mut self, t: &Transport) {
            self.seen.lock().unwrap().push(*t);
        }
    }

    #[test]
    fn plugins_get_the_session_transport() {
        let (mut s, t) = session_with_dc(0.5, 48_000 * 4);
        s.tempo.set_tempo(0, 90.0).unwrap();
        s.tempo.set_meter(0, 3, 4).unwrap();
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.clock"));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let clock = Clock { inner: soundcraft_dsp::create("gain").unwrap(), seen: Arc::clone(&seen) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "clap:test.clock".into(), ch: 1, plugin: Box::new(clock) }]);
        render_block(&mut eng, &s, 96_000);
        eng.monitor_only = true;
        render_block(&mut eng, &s, 96_000);
        let seen = seen.lock().unwrap();
        let t0 = seen[0];
        assert!(t0.playing && !t0.recording);
        assert_eq!((t0.tempo, t0.numerator, t0.denominator, t0.samples, t0.bar), (90.0, 3, 4, 96_000, 1));
        assert!((t0.seconds - 2.0).abs() < 1e-9 && (t0.quarters - 3.0).abs() < 1e-9 && (t0.bar_start - 3.0).abs() < 1e-9);
        assert!(!seen[1].playing);
    }

    struct Ears {
        inner: Box<dyn Plugin>,
        heard: Arc<std::sync::Mutex<Vec<(usize, [u8; 3])>>>,
    }

    impl Plugin for Ears {
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
            self.inner.process(io, frames);
        }
        fn midi(&mut self, offset: usize, msg: [u8; 3]) {
            self.heard.lock().unwrap().push((offset, msg));
        }
    }

    #[test]
    fn controllers_play_at_their_offsets_and_are_chased_after_a_seek() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.ears"));
        let cc = |tick, kind, value| soundcraft_midi::CtrlEvent { tick, channel: 0, kind, value };
        let ctrls = vec![cc(4, CtrlKind::Cc(64), 127), cc(960, CtrlKind::PitchBend, -8192), cc(1920, CtrlKind::Cc(64), 0)];
        let seq = soundcraft_midi::Sequence { ctrls, ..Default::default() };
        let cid = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "m", 0, 48_000 * 4, seq));
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ears = Ears { inner: soundcraft_dsp::create("gain").unwrap(), heard: Arc::clone(&heard) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.ears".into(), ch: 2, plugin: Box::new(ears) }]);
        render_block(&mut eng, &s, 0);
        assert_eq!(std::mem::take(&mut *heard.lock().unwrap()), vec![(0, [0xB0, 121, 0]), (100, [0xB0, 64, 127])]);
        render_block(&mut eng, &s, 36_000);
        assert_eq!(std::mem::take(&mut *heard.lock().unwrap()), vec![(0, [0xB0, 121, 0]), (0, [0xB0, 64, 127]), (0, [0xE0, 0, 0])]);
        render_block(&mut eng, &s, 36_512);
        assert!(heard.lock().unwrap().is_empty());
    }

    struct Keys {
        inner: Box<dyn Plugin>,
        heard: Arc<std::sync::Mutex<Vec<(u8, u8)>>>,
    }

    impl Plugin for Keys {
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
            self.inner.process(io, frames);
        }
        fn note_on(&mut self, _offset: usize, note: u8, velocity: u8) {
            self.heard.lock().unwrap().push((note, velocity));
        }
    }

    #[test]
    fn real_time_properties_reach_the_instrument() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.keys"));
        let mut seq = soundcraft_midi::Sequence::default();
        seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 480 });
        let cid = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "m", 0, 48_000, seq));
        s.edit.values.insert(format!("rtp.{}.transpose", t.0), 12.0);
        s.edit.values.insert(format!("rtp.{}.velocity", t.0), -20.0);
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let keys = Keys { inner: soundcraft_dsp::create("gain").unwrap(), heard: Arc::clone(&heard) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.keys".into(), ch: 2, plugin: Box::new(keys) }]);
        render_block(&mut eng, &s, 0);
        assert_eq!(*heard.lock().unwrap(), vec![(72, 80)]);
    }

    struct Ears2 {
        inner: Box<dyn Plugin>,
        heard: Arc<std::sync::Mutex<Vec<(&'static str, usize, u8)>>>,
    }

    impl Plugin for Ears2 {
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
            self.inner.process(io, frames);
        }
        fn note_on(&mut self, offset: usize, note: u8, _velocity: u8) {
            self.heard.lock().unwrap().push(("on", offset, note));
        }
        fn note_off(&mut self, offset: usize, note: u8) {
            self.heard.lock().unwrap().push(("off", offset, note));
        }
    }

    #[test]
    fn a_note_off_on_the_clip_end_block_boundary_is_sent() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.ears2"));
        let mut seq = soundcraft_midi::Sequence::default();
        // Far longer than the clip, so the off is clamped to the clip end (sample 1024).
        seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 480 });
        let cid = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "m", 0, 1024, seq));
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ears = Ears2 { inner: soundcraft_dsp::create("gain").unwrap(), heard: Arc::clone(&heard) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.ears2".into(), ch: 2, plugin: Box::new(ears) }]);
        for pos in [0, 512, 1024] {
            render_block(&mut eng, &s, pos);
        }
        assert_eq!(*heard.lock().unwrap(), vec![("on", 0, 60), ("off", 0, 60)]);
        // Past the clip nothing more is sent.
        render_block(&mut eng, &s, 1536);
        assert_eq!(heard.lock().unwrap().len(), 2);
    }

    #[derive(Default)]
    struct Calls {
        on: Vec<u8>,
        all_off: usize,
        resets: usize,
        processed: usize,
        /// `(frames, shortest channel length)` of every `process` call.
        shapes: Vec<(usize, usize)>,
    }

    struct Spy {
        inner: Box<dyn Plugin>,
        calls: Arc<std::sync::Mutex<Calls>>,
    }

    impl Plugin for Spy {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.calls.lock().unwrap().resets += 1;
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            {
                let mut c = self.calls.lock().unwrap();
                c.processed += 1;
                c.shapes.push((frames, io.iter().map(Vec::len).min().unwrap_or(0)));
            }
            self.inner.process(io, frames);
        }
        fn note_on(&mut self, _offset: usize, note: u8, _velocity: u8) {
            self.calls.lock().unwrap().on.push(note);
        }
        fn all_notes_off(&mut self) {
            self.calls.lock().unwrap().all_off += 1;
        }
    }

    #[test]
    fn a_note_off_at_a_clip_boundary_precedes_the_next_clips_note_on() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.ears2"));
        let note = || {
            let mut seq = soundcraft_midi::Sequence::default();
            seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 960 });
            seq
        };
        // The later clip is first in the playlist, so playlist order alone would send its on first.
        let (late, early) = (s.new_clip_id(), s.new_clip_id());
        let clips = &mut s.track_mut(t).unwrap().playlist_mut().unwrap().clips;
        clips.push(Clip::midi(late, "b", 24_000, 24_000, note()));
        clips.push(Clip::midi(early, "a", 0, 24_000, note()));
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ears = Ears2 { inner: soundcraft_dsp::create("gain").unwrap(), heard: Arc::clone(&heard) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.ears2".into(), ch: 2, plugin: Box::new(ears) }]);
        // One beat at 120 bpm / 48 kHz is 24 000 samples, so the boundary sits on a tick and lands
        // at offset 256 of the block starting 256 samples before it.
        render_block(&mut eng, &s, 0);
        heard.lock().unwrap().clear();
        render_block(&mut eng, &s, 23_744);
        assert_eq!(*heard.lock().unwrap(), vec![("off", 256, 60), ("on", 256, 60)]);
        assert_eq!(eng.strips[&t].held, vec![60]);
    }

    #[test]
    fn all_notes_off_releases_every_held_note_without_resetting() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.spy"));
        let mut seq = soundcraft_midi::Sequence::default();
        seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 48_000 });
        let cid = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "m", 0, 48_000 * 4, seq));
        let calls = Arc::new(std::sync::Mutex::new(Calls::default()));
        let spy = Spy { inner: soundcraft_dsp::create("gain").unwrap(), calls: Arc::clone(&calls) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.spy".into(), ch: 2, plugin: Box::new(spy) }]);
        render_block(&mut eng, &s, 0);
        assert_eq!(eng.strips[&t].held, vec![60]);
        let (offs, resets) = {
            let c = calls.lock().unwrap();
            assert_eq!(c.on, vec![60]);
            (c.all_off, c.resets)
        };
        eng.all_notes_off();
        let c = calls.lock().unwrap();
        assert_eq!(c.all_off, offs + 1, "the instrument was told to release");
        assert_eq!(c.resets, resets, "reset would discard the queued releases");
        assert!(eng.strips[&t].held.is_empty());
    }

    #[test]
    fn flush_releases_processes_every_instrument_once_without_scheduling() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.spy"));
        let mut seq = soundcraft_midi::Sequence::default();
        seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 480 });
        let cid = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "m", 0, 48_000, seq));
        let calls = Arc::new(std::sync::Mutex::new(Calls::default()));
        let spy = Spy { inner: soundcraft_dsp::create("gain").unwrap(), calls: Arc::clone(&calls) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.spy".into(), ch: 2, plugin: Box::new(spy) }]);
        eng.ensure_synced(&s);
        eng.flush_releases(512);
        {
            let c = calls.lock().unwrap();
            assert_eq!(c.processed, 1, "one pass");
            assert!(c.on.is_empty(), "nothing was scheduled");
        }
        render_block(&mut eng, &s, 0);
        assert_eq!(calls.lock().unwrap().on, vec![60], "the position was untouched, so the first note still schedules");
    }

    #[test]
    fn flush_releases_hands_a_mono_instrument_full_length_buffers_before_any_render() {
        let mut s = Session::default();
        // A mono strip carrying an instrument (the one-channel buffer is what takes the scratch path).
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.spy"));
        let calls = Arc::new(std::sync::Mutex::new(Calls::default()));
        let spy = Spy { inner: soundcraft_dsp::create("gain").unwrap(), calls: Arc::clone(&calls) };
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.spy".into(), ch: 2, plugin: Box::new(spy) }]);
        eng.ensure_synced(&s);
        assert_eq!(eng.strips[&t].buf.len(), 1, "a mono strip");
        eng.flush_releases(512);
        let c = calls.lock().unwrap();
        assert_eq!(c.shapes.len(), 1, "one pass");
        assert_eq!(c.shapes[0].0, 512);
        assert!(c.shapes[0].1 >= 512, "shortest channel of {} samples", c.shapes[0].1);
    }

    fn render_block(eng: &mut MixEngine, s: &Session, pos: i64) -> f32 {
        let mut out = vec![vec![0.0f32; 512]; 2];
        eng.render(s, pos, 512, &mut out);
        out[0][500]
    }

    /// An effect that reports a fixed [`ProcessStatus`] and tail, counts its `process` calls and
    /// records what it was told with `set_input_silent`. After its input goes silent it rings on:
    /// it outputs 0.5 for `decay` more processed blocks.
    struct Sleepy {
        inner: Box<dyn Plugin>,
        status: ProcessStatus,
        tail: usize,
        decay: usize,
        /// Ringing blocks left.
        left: usize,
        /// `set_param` calls.
        sets: Arc<std::sync::atomic::AtomicUsize>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        told: Arc<std::sync::Mutex<Vec<bool>>>,
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
            self.sets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let silent_in = io.iter().all(|c| c.iter().take(frames).all(|x| *x == 0.0));
            self.inner.process(io, frames);
            if !silent_in {
                self.left = self.decay;
            } else if self.left > 0 {
                self.left -= 1;
                io.iter_mut().for_each(|c| c.iter_mut().take(frames).for_each(|x| *x = 0.5));
            }
        }
        fn tail_samples(&self) -> usize {
            self.tail
        }
        fn process_status(&self) -> ProcessStatus {
            self.status
        }
        fn set_input_silent(&mut self, silent: bool) {
            self.told.lock().unwrap().push(silent);
        }
    }

    #[test]
    fn hosted_effects_are_suspended_by_their_own_status() {
        use ProcessStatus::*;
        // DC at 0.5 for the first 1024 samples, silence after: blocks at 4096.. are silent input.
        const LOUD: i64 = 0;
        const QUIET: [i64; 4] = [4096, 4608, 5120, 5632];
        struct Row {
            status: ProcessStatus,
            tail: usize,
            /// Blocks the fake rings on (outputs 0.5) after its input goes silent.
            decay: usize,
            /// Before this block, the session changes one of the insert's parameters.
            param_at: Option<usize>,
            no_suspend: bool,
            instrument: bool,
            /// Level of the DC clip that feeds the insert (0.5 unless a row needs a tiny one).
            dc: f32,
            /// Block positions rendered, in order.
            blocks: &'static [i64],
            /// `process` calls after each block.
            calls: &'static [usize],
            /// Everything passed to `set_input_silent`, in order.
            told: &'static [bool],
        }
        let rows = [
            Row {
                status: Continue,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.5,
                blocks: &[QUIET[0]],
                calls: &[1],
                told: &[true],
            },
            Row {
                status: ContinueIfNotQuiet,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.5,
                blocks: &[QUIET[0], LOUD],
                calls: &[0, 1],
                told: &[false],
            },
            Row {
                status: Sleep,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.5,
                blocks: &[QUIET[0], LOUD],
                calls: &[0, 1],
                told: &[false],
            },
            Row {
                status: Tail,
                tail: 1024,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.5,
                blocks: &[QUIET[0], QUIET[1], QUIET[2], LOUD],
                calls: &[1, 2, 2, 3],
                told: &[true, true, false],
            },
            Row {
                status: Sleep,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: true,
                instrument: false,
                dc: 0.5,
                blocks: &[QUIET[0]],
                calls: &[1],
                told: &[true],
            },
            // Output, not input: a decaying ContinueIfNotQuiet effect is processed while its output
            // rings after the input went silent, and suspended once it came out silent.
            Row {
                status: ContinueIfNotQuiet,
                tail: 0,
                decay: 2,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.5,
                blocks: &[LOUD, QUIET[0], QUIET[1], QUIET[2], QUIET[3]],
                calls: &[1, 2, 3, 4, 4],
                told: &[false, true, true, true],
            },
            // Tail: the tail counter alone (0 here) does not suspend an effect still ringing.
            Row {
                status: Tail,
                tail: 0,
                decay: 1,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.5,
                blocks: &[LOUD, QUIET[0], QUIET[1], QUIET[2]],
                calls: &[1, 2, 3, 3],
                told: &[false, true, true],
            },
            // A parameter change wakes a sleeping effect for that block only, so the change reaches it.
            Row {
                status: Sleep,
                tail: 0,
                decay: 0,
                param_at: Some(1),
                no_suspend: false,
                instrument: false,
                dc: 0.5,
                blocks: &[QUIET[0], QUIET[1], QUIET[2]],
                calls: &[0, 1, 1],
                told: &[true],
            },
            Row {
                status: Sleep,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: true,
                dc: 0.5,
                blocks: &[QUIET[0], QUIET[1]],
                calls: &[1, 2],
                told: &[],
            },
            // Sleep wakes on any nonzero sample, even one below SILENCE_THRESHOLD, and sleeps on exact zero.
            Row {
                status: Sleep,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 1e-7,
                blocks: &[LOUD],
                calls: &[1],
                told: &[false],
            },
            // Exact zero is told it is silent.
            Row {
                status: Continue,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.0,
                blocks: &[LOUD],
                calls: &[1],
                told: &[true],
            },
            Row {
                status: Sleep,
                tail: 0,
                decay: 0,
                param_at: None,
                no_suspend: false,
                instrument: false,
                dc: 0.0,
                blocks: &[LOUD],
                calls: &[0],
                told: &[],
            },
        ];
        for (n, row) in rows.iter().enumerate() {
            let (mut s, mut t) = session_with_dc(row.dc, 1024);
            let id = "clap:test.sleepy";
            let slot = if row.instrument {
                t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
                s.track_mut(t).unwrap().instrument = Some(Insert::new(id));
                INSTRUMENT_SLOT
            } else {
                s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new(id));
                0
            };
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let told = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let fake = Sleepy {
                inner: soundcraft_dsp::create("gain").unwrap(),
                status: row.status,
                tail: row.tail,
                decay: row.decay,
                left: 0,
                sets: Arc::clone(&sets),
                calls: Arc::clone(&calls),
                told: Arc::clone(&told),
            };
            let mut eng = MixEngine::new(48_000.0, 512);
            eng.set_external_instances(true);
            eng.no_suspend = row.no_suspend;
            let ch = if row.instrument { 2 } else { 1 };
            eng.adopt(vec![PreparedInstance { track: t, slot, id: id.into(), ch, plugin: Box::new(fake) }]);
            for (b, (&pos, &want)) in row.blocks.iter().zip(row.calls).enumerate() {
                if row.param_at == Some(b)
                    && let Some(Some(ins)) = s.track_mut(t).unwrap().mixer.inserts.get_mut(0)
                {
                    ins.params.insert("gain".into(), -6.0);
                }
                let out = render_block(&mut eng, &s, pos);
                assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), want, "row {n} ({:?}), block {b} at {pos}", row.status);
                if pos != LOUD && row.decay == 0 {
                    assert_eq!(out, 0.0, "row {n}: silent in, silent out");
                }
            }
            assert_eq!(*told.lock().unwrap(), row.told, "row {n} ({:?}): set_input_silent", row.status);
            let want_sets = usize::from(row.param_at.is_some());
            assert_eq!(sets.load(std::sync::atomic::Ordering::Relaxed), want_sets, "row {n} ({:?}): set_param", row.status);
        }
    }

    #[test]
    fn external_instances_are_adopted_and_retired() {
        let (mut s, t) = session_with_dc(0.5, 48_000);
        let mut ins = Insert::new("clap:test.fake");
        ins.params.insert("gain".into(), -6.0);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        let specs = instance_specs(&s);
        assert_eq!(specs, vec![InstanceSpec { track: t, slot: 0, id: "clap:test.fake".into(), ch: 1, state: None }]);
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        // Nothing to adopt yet: the slot passes audio through (and sync never tried to load a
        // CLAP plugin on this thread).
        let dry = render_block(&mut eng, &s, 0);
        assert!(dry > 0.3, "{dry}");
        let sets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut gain = soundcraft_dsp::create("gain").unwrap();
        gain.prepare(48_000.0, 512, 1);
        let fake = Stateful { inner: gain, blob: Vec::new(), sets: Arc::clone(&sets) };
        // An instance for the wrong channel count is retired, not used.
        let wrong = Stateful { inner: soundcraft_dsp::create("gain").unwrap(), blob: Vec::new(), sets: Arc::clone(&sets) };
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "clap:test.fake".into(), ch: 2, plugin: Box::new(wrong) }]);
        assert!(render_block(&mut eng, &s, 512) > 0.3);
        assert_eq!(eng.take_retired().len(), 1);
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "clap:test.fake".into(), ch: 1, plugin: Box::new(fake) }]);
        let mut wet = 0.0;
        for b in 2..20 {
            wet = render_block(&mut eng, &s, b * 512);
        }
        assert!((wet / dry - 0.501).abs() < 0.01, "{wet} vs {dry}");
        assert!(sets.load(std::sync::atomic::Ordering::Relaxed) > 0, "no stored state: session params are pushed");
        assert!(eng.take_retired().is_empty());
        // Removing the insert retires the instance instead of dropping it here.
        s.track_mut(t).unwrap().mixer.inserts[0] = None;
        render_block(&mut eng, &s, 20 * 512);
        assert_eq!(eng.take_retired().len(), 1);
        // Instances nobody claims are retired too; drain_plugins empties the engine.
        eng.adopt(vec![PreparedInstance { track: t, slot: 3, id: "clap:x".into(), ch: 1, plugin: soundcraft_dsp::create("gain").unwrap() }]);
        render_block(&mut eng, &s, 21 * 512);
        assert_eq!(eng.take_retired().len(), 1);
        assert!(eng.drain_plugins().is_empty());
    }

    struct Nan(Box<dyn Plugin>);

    impl Plugin for Nan {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.0.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.0.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.0.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.0.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.0.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            io.iter_mut().for_each(|c| c.iter_mut().take(frames).for_each(|x| *x = f32::NAN));
        }
    }

    fn nan(track: TrackId, slot: usize, ch: usize) -> Vec<PreparedInstance> {
        vec![PreparedInstance { track, slot, id: "clap:test.nan".into(), ch, plugin: Box::new(Nan(soundcraft_dsp::create("gain").unwrap())) }]
    }

    /// A plug-in whose `process` panics.
    struct Panics(Box<dyn Plugin>);

    impl Plugin for Panics {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.0.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.0.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.0.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.0.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.0.param(id)
        }
        fn process(&mut self, _io: &mut [Vec<f32>], _frames: usize) {
            std::panic::panic_any("a plug-in bug");
        }
    }

    /// A plug-in that panics while a block renders must leave its strip in the engine, so the
    /// instance can be drained and torn down off the audio thread instead of by the unwind. One row
    /// per phase: an insert on an ordinary track, and one on an aux.
    #[test]
    fn a_panicking_plugin_leaves_its_strip_drainable() {
        for on_aux in [false, true] {
            let (mut s, t) = session_with_dc(0.5, 48_000);
            let aux = s.add_track(TrackKind::Aux, ChannelFormat::Mono, None);
            let track = if on_aux { aux } else { t };
            s.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.panics"));
            let mut eng = MixEngine::new(48_000.0, 512);
            eng.set_external_instances(true);
            let mut gain = soundcraft_dsp::create("gain").unwrap();
            gain.prepare(48_000.0, 512, 1);
            eng.adopt(vec![PreparedInstance { track, slot: 0, id: "clap:test.panics".into(), ch: 1, plugin: Box::new(Panics(gain)) }]);
            let mut out = vec![vec![0.0; 512]; 2];
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| eng.render(&s, 0, 512, &mut out)));
            assert!(ran.is_err(), "the plug-in's panic comes out of render (on_aux: {on_aux})");
            assert_eq!(eng.drain_plugins().len(), 1, "the instance is still in the engine after the panic (on_aux: {on_aux})");
        }
    }

    #[test]
    fn an_insert_that_puts_out_nan_is_taken_out_until_the_next_reset() {
        let (mut s, a) = session_with_dc(0.5, 48_000);
        let b = add_dc_track(&mut s, 0.5, 48_000);
        s.track_mut(a).unwrap().mixer.inserts[2] = Some(Insert::new("clap:test.nan"));
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        let both = render_block(&mut eng, &s, 0);
        eng.adopt(nan(a, 2, 1));
        let only_b = render_block(&mut eng, &s, 512);
        assert!(only_b > 0.3 && (only_b - both / 2.0).abs() < 1e-6, "{only_b} vs {both}");
        assert_eq!(eng.meters[&a].plugin_failures, PluginFailures { count: 1, last: 2 });
        assert_eq!(eng.meters[&b].plugin_failures, PluginFailures::default());
        assert_eq!(render_block(&mut eng, &s, 1024), both);
        eng.reset();
        assert_eq!(render_block(&mut eng, &s, 1536), only_b);
        assert_eq!(eng.meters[&a].plugin_failures.count, 2);
    }

    #[test]
    fn nan_from_an_instrument_or_a_master_insert_is_contained() {
        let mut s = synth_session(-6.0);
        let synth = s.tracks[0].id;
        s.track_mut(synth).unwrap().instrument = Some(Insert::new("clap:test.nan"));
        let b = add_dc_track(&mut s, 0.5, 48_000);
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        eng.adopt(nan(synth, INSTRUMENT_SLOT, 2));
        let only_b = render_block(&mut eng, &s, 0);
        assert!(only_b > 0.3, "{only_b}");
        assert_eq!(eng.meters[&synth].plugin_failures, PluginFailures { count: 1, last: INSTRUMENT_SLOT });
        assert_eq!(eng.meters[&b].plugin_failures, PluginFailures::default());
        let m = s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
        s.track_mut(m).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.nan"));
        eng.adopt(nan(m, 0, 2));
        assert_eq!(render_block(&mut eng, &s, 512), 0.0);
        assert_eq!(eng.meters[&m].plugin_failures, PluginFailures { count: 1, last: 0 });
        assert_eq!(render_block(&mut eng, &s, 1024), only_b);
    }

    #[test]
    fn a_new_instance_replaces_a_matching_one_in_its_slot() {
        // The host removed and re-added the plugin, and both session updates reached the audio
        // thread in one go: the slot still holds the old instance (same id, same channels), but
        // the host made a new one and drives its editor. The new one must win, or the editor
        // talks to an instance that was retired ("the plugin is gone").
        let (mut s, t) = session_with_dc(0.5, 48_000);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.fake"));
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        let sets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make = |blob: &[u8]| {
            let mut gain = soundcraft_dsp::create("gain").unwrap();
            gain.prepare(48_000.0, 512, 1);
            Box::new(Stateful { inner: gain, blob: blob.to_vec(), sets: Arc::clone(&sets) })
        };
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "clap:test.fake".into(), ch: 1, plugin: make(b"old") }]);
        render_block(&mut eng, &s, 0);
        assert!(eng.take_retired().is_empty());
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "clap:test.fake".into(), ch: 1, plugin: make(b"new") }]);
        render_block(&mut eng, &s, 512);
        let mut retired = eng.take_retired();
        assert_eq!(retired.len(), 1, "the old instance is retired");
        assert_eq!(retired[0].save_state(), Some(b"old".to_vec()), "the new instance stays");
    }

    #[test]
    fn stored_state_comes_first_then_session_params() {
        let (mut s, t) = session_with_dc(0.5, 48_000);
        let mut ins = Insert::new("vst3:00000000000000000000000000000001");
        ins.params.insert("gain".into(), -6.0);
        ins.set_state_bytes(b"stored");
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        assert_eq!(instance_specs(&s)[0].state.as_deref(), Some(&b"stored"[..]));
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        let sets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fake = Stateful { inner: soundcraft_dsp::create("gain").unwrap(), blob: Vec::new(), sets: Arc::clone(&sets) };
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "vst3:00000000000000000000000000000001".into(), ch: 1, plugin: Box::new(fake) }]);
        render_block(&mut eng, &s, 0);
        assert_eq!(sets.load(std::sync::atomic::Ordering::Relaxed), 1, "session params apply once");
        render_block(&mut eng, &s, 512);
        assert_eq!(sets.load(std::sync::atomic::Ordering::Relaxed), 1);
        s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("gain".into(), -12.0);
        render_block(&mut eng, &s, 1024);
        assert_eq!(sets.load(std::sync::atomic::Ordering::Relaxed), 2, "later session edits apply");
        // Offline (not external): third-party plugins are simply absent (tests never load real ones).
        let out = render_range(&s, Range::new(0, 1024), 512);
        assert!(out[0][100] > 0.3);
    }

    #[test]
    fn a_hosted_plugin_keeps_its_own_values_where_the_session_holds_its_defaults() {
        let (mut s, t) = session_with_dc(0.5, 48_000);
        let id = "vst3:00000000000000000000000000000002";
        let mut ins = Insert::new(id);
        // Older sessions store every parameter at the default the plugin reports (0 for gain).
        ins.params.insert("gain".into(), 0.0);
        ins.set_state_bytes(b"stored");
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        let sets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fake = Stateful { inner: soundcraft_dsp::create("gain").unwrap(), blob: Vec::new(), sets: Arc::clone(&sets) };
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: id.into(), ch: 1, plugin: Box::new(fake) }]);
        let count = || sets.load(std::sync::atomic::Ordering::Relaxed);
        render_block(&mut eng, &s, 0);
        assert_eq!(count(), 0, "a stored default is not pushed over the restored plugin");
        render_block(&mut eng, &s, 512);
        assert_eq!(count(), 0);
        // Later edits are pushed, including a return to the default.
        s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("gain".into(), -6.0);
        render_block(&mut eng, &s, 1024);
        assert_eq!(count(), 1);
        s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("gain".into(), 0.0);
        render_block(&mut eng, &s, 1536);
        assert_eq!(count(), 2);
    }

    #[test]
    fn clip_effects_gain_and_trim_automation_apply() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let cid = s.track(t).unwrap().clips()[0].id.0;
        s.edit.values.insert(format!("clip_fx.{cid}.gain"), -6.0);
        let out = render_range(&s, Range::new(0, 4800), 512);
        let expect = 0.5 * std::f32::consts::FRAC_1_SQRT_2 * db_to_gain(-6.0);
        assert!((out[0][2000] - expect).abs() < 0.01, "{}", out[0][2000]);
        s.edit.set_flag(&format!("clip_fx.bypass.{cid}"), true);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!((out[0][2000] - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01);
        s.track_mut(t).unwrap().lane_mut(&AutoParam::Plugin { slot: u8::MAX, param: "trim".into() }).set_point(0, -12.0);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][2000] < 0.2, "{}", out[0][2000]);
    }

    #[test]
    fn delay_compensation_aligns_latent_tracks() {
        let mut s = Session::default();
        let mut imp = vec![0.0f32; 48_000];
        imp[1000] = 1.0;
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![imp] })));
        let mut ids = Vec::new();
        for (pan, fx) in [(-1.0f32, true), (1.0, false)] {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            let cid = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "i", SourceId(900), 0, 0, 48_000));
            tr.mixer.pan = vec![pan];
            if fx {
                tr.mixer.inserts[0] = Some(Insert::new("maximizer"));
            }
            ids.push(t);
        }
        let argmax = |c: &[f32]| c.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(i, _)| i).unwrap();
        let lat = soundcraft_dsp::create("maximizer").unwrap().latency();
        assert!(lat > 0, "test needs a latent plugin");
        let out = render_range(&s, Range::new(0, 8000), 512);
        assert_eq!(argmax(&out[0]), argmax(&out[1]), "PDC should align L and R");
        s.edit.delay_compensation = false;
        let out = render_range(&s, Range::new(0, 8000), 512);
        assert_ne!(argmax(&out[0]), argmax(&out[1]));
    }

    /// A reset (stop, locate) must not leave the previous position's audio in the alignment delay.
    #[test]
    fn reset_clears_delay_compensation_history() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.edit.click = false;
        s.track_mut(t).unwrap().mixer.pan = vec![1.0];
        let latent = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let mut comp = Insert::new("compressor");
        comp.params.insert("lookahead".into(), 5.0);
        s.track_mut(latent).unwrap().mixer.inserts[0] = Some(comp);
        let mut eng = MixEngine::new(48_000.0, 512);
        let mut out = vec![vec![0.0f32; 512]; 2];
        for b in 0..9 {
            eng.render(&s, b * 512, 512, &mut out);
        }
        assert!(eng.latency > 0 && out[1][500] > 0.3, "the fixture plays through the alignment delay");
        eng.reset();
        eng.render(&s, 1_000_000, 512, &mut out);
        assert!(out.iter().flatten().all(|x| *x == 0.0), "old aligned audio after a reset");
    }

    fn impulse_at(ch: &[f32]) -> Vec<usize> {
        ch.iter().enumerate().filter(|(_, v)| v.abs() > 1e-4).map(|(i, _)| i).collect()
    }

    /// An aux insert's lookahead is part of the mix latency on the first block, not the block after.
    #[test]
    fn delay_compensation_aligns_an_aux_insert() {
        let mut s = Session::default();
        let n = 8192;
        let mut left = vec![0.0f32; n];
        for at in [0usize, 2048, 8000] {
            left[at] = 0.25;
        }
        s.pool.insert(SourceId(901), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![left.clone(), left] })));
        let bus = s.add_bus("Test Bus", ChannelFormat::Stereo);
        let source = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Source"));
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Return"));
        let cid = s.new_clip_id();
        s.track_mut(source).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "impulses", SourceId(901), 0, 0, n as i64));
        s.track_mut(source).unwrap().mixer.output = Route::Bus(bus);
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        let render = |s: &Session, start: i64, end: i64| render_range(s, Range::new(start, end), 1024);
        let control = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&control[0]), vec![0, 2048, 8000]);
        assert_eq!(impulse_at(&control[1]), impulse_at(&control[0]));

        let mut comp = Insert::new("compressor");
        comp.params.insert("lookahead".into(), 5.0);
        comp.params.insert("ratio".into(), 1.0);
        s.track_mut(source).unwrap().mixer.inserts[0] = Some(comp.clone());
        let on_source = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&on_source[0]), impulse_at(&control[0]));

        s.track_mut(source).unwrap().mixer.inserts[0] = None;
        s.track_mut(aux).unwrap().mixer.inserts[0] = Some(comp);
        let on_aux = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&on_aux[0]), vec![0, 2048, 8000], "aux lookahead must stay inside the bounce");
        assert_eq!(impulse_at(&on_aux[1]), impulse_at(&on_aux[0]));
        assert!((on_aux[0][8000] - 0.25).abs() < 1e-3, "{}", on_aux[0][8000]);
        let partial = render(&s, 2048, 3072);
        assert_eq!(impulse_at(&partial[0]), vec![0]);

        s.track_mut(aux).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("lookahead".into(), 0.0);
        let zero = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&zero[0]), impulse_at(&control[0]));
    }

    #[test]
    fn offline_renders_line_up_with_the_timeline_in_any_block_size() {
        let mut s = Session::default();
        let mut imp = vec![0.0f32; 48_000];
        imp[1000] = 1.0;
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![imp] })));
        // A mono strip carrying an instrument (the one-channel buffer is what takes the scratch path).
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let bus = s.add_bus("Stem", ChannelFormat::Stereo);
        let cid = s.new_clip_id();
        let tr = s.track_mut(t).unwrap();
        tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "i", SourceId(900), 0, 0, 48_000));
        tr.mixer.inserts[0] = Some(Insert::new("maximizer"));
        let mut send = soundcraft_model::SendSlot::new(Route::Bus(bus));
        send.level_db = 0.0;
        tr.mixer.sends[0] = Some(send);
        let latency = soundcraft_dsp::create("maximizer").unwrap().latency();
        assert!(latency > 16);
        for source in [None, Some(bus)] {
            for block in [16, 100, 512] {
                let mut got = Vec::new();
                let mut skipped = 0;
                let mut render = RangeRender::new(&s, source, Range::new(900, 2900), block);
                while let Some((chunk, n)) = render.next_block(&s) {
                    assert!(n <= block);
                    skipped += usize::from(n == 0);
                    got.extend_from_slice(&chunk[0][..n]);
                }
                assert_eq!(got.len(), 2000, "{source:?} block {block}");
                assert_eq!(skipped, latency / block, "{source:?} block {block}");
                let peak = got.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(i, _)| i);
                assert_eq!(peak, Some(100), "{source:?} block {block}");
            }
        }
    }

    #[test]
    fn a_plugins_bogus_latency_is_capped() {
        struct Latent(Box<dyn Plugin>);
        impl Plugin for Latent {
            fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
                self.0.info()
            }
            fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
                self.0.prepare(sr, mb, ch);
            }
            fn reset(&mut self) {
                self.0.reset();
            }
            fn set_param(&mut self, id: &str, v: f32) -> bool {
                self.0.set_param(id, v)
            }
            fn param(&self, id: &str) -> Option<f32> {
                self.0.param(id)
            }
            fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
                self.0.process(io, frames);
            }
            fn latency(&self) -> usize {
                u32::MAX as usize
            }
        }
        let (mut s, t) = session_with_dc(0.5, 48_000);
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Mono, None);
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        for track in [t, aux] {
            s.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("clap:test.latent"));
            let mut gain = soundcraft_dsp::create("gain").unwrap();
            gain.prepare(48_000.0, 512, 1);
            eng.adopt(vec![PreparedInstance { track, slot: 0, id: "clap:test.latent".into(), ch: 1, plugin: Box::new(Latent(gain)) }]);
        }
        render_block(&mut eng, &s, 0);
        assert_eq!((eng.track_latency, eng.latency), (MAX_LATENCY, 2 * MAX_LATENCY));
    }

    #[test]
    fn input_monitoring_replaces_clips() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.input = vec![vec![0.25; 512]];
        let mut out = vec![vec![0.0; 512]; 2];
        // Not monitored: the clip plays.
        eng.render(&s, 0, 512, &mut out);
        assert!((out[0][100] - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        // Monitored: the input replaces it, and it still sounds when stopped.
        s.track_mut(t).unwrap().mixer.input_monitor = true;
        eng.render(&s, 512, 512, &mut out);
        assert!((out[0][100] - 0.25 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3, "{}", out[0][100]);
        eng.monitor_only = true;
        eng.render(&s, 1024, 512, &mut out);
        assert!(out[0][100] > 0.1);
        s.track_mut(t).unwrap().mixer.input_monitor = false;
        eng.render(&s, 1024, 512, &mut out);
        assert!(out[0][100].abs() < 1e-6, "stopped, unmonitored: silence");
    }

    #[test]
    fn empty_session_is_silent() {
        let s = Session::default();
        let out = render_range(&s, Range::new(0, 1000), 128);
        assert!(out.iter().all(|c| c.iter().all(|x| *x == 0.0)));
    }

    #[test]
    fn strip_workspace_keeps_its_allocation_between_blocks() {
        let mut s = Session::default();
        for _ in 0..6 {
            s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        }
        let mut eng = MixEngine::new(48_000.0, 512);
        let mut out = vec![vec![0.0; 512]; 2];
        eng.render(&s, 0, 512, &mut out);
        let capacity = eng.work.capacity();
        assert!(capacity >= 6);
        eng.render(&s, 512, 512, &mut out);
        assert_eq!(eng.work.capacity(), capacity);
    }

    #[test]
    fn stopped_monitoring_does_not_click() {
        let mut s = Session::default();
        s.edit.click = true;
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.metronome = true;
        eng.monitor_only = true;
        let mut out = vec![vec![0.0f32; 512]; 2];
        for _ in 0..4 {
            eng.render(&s, 0, 512, &mut out);
            assert!(out.iter().all(|c| c.iter().all(|x| *x == 0.0)));
        }
    }

    /// Renders `len` frames from zero the way playback does (metronome on), in blocks of `block`.
    fn render_playback(s: &Session, len: usize, block: usize, recording: bool) -> Vec<Vec<f32>> {
        let mut eng = MixEngine::new(s.sample_rate.as_f64() as f32, block);
        eng.metronome = true;
        eng.recording = recording;
        let mut out = vec![vec![0.0f32; len]; 2];
        let mut tmp = vec![vec![0.0f32; block]; 2];
        let mut done = 0usize;
        while done < len {
            let n = (len - done).min(block);
            eng.render(s, done as i64, n, &mut tmp);
            for (o, t) in out.iter_mut().zip(tmp.iter()) {
                if let (Some(d), Some(src)) = (o.get_mut(done..done + n), t.get(..n)) {
                    d.copy_from_slice(src);
                }
            }
            done += n;
        }
        out
    }

    #[test]
    fn click_sounds_on_every_beat_only_in_playback() {
        let mut s = Session::default();
        let beat = s.sample_rate.as_f64() as usize * 60 / 120; // 120 BPM quarter notes
        let len = beat * 8;
        s.edit.click = true;
        // Offline renders (bounces, stems) never print the metronome.
        let offline = render_range(&s, Range::new(0, len as i64), 512);
        assert!(offline.iter().all(|c| c.iter().all(|x| *x == 0.0)));

        let out = render_playback(&s, len, 512, false);
        for b in 0..8usize {
            let at = b * beat;
            let on: f32 = out[0][at..at + 200].iter().map(|x| x.abs()).fold(0.0, f32::max);
            let off: f32 = out[0][at + beat / 2..at + beat / 2 + 200].iter().map(|x| x.abs()).fold(0.0, f32::max);
            assert!(on > 0.1, "beat {b} should click, peak {on}");
            assert_eq!(off, 0.0, "between beats must stay silent");
            assert_eq!(out[0][at], out[1][at], "both channels get the click");
        }
        // Blocks that start mid-beat render the same clicks as a single pass.
        let split = render_playback(&s, len, 97, false);
        assert_eq!(out, split);
    }

    #[test]
    fn click_only_during_record_follows_recording() {
        let mut s = Session::default();
        s.edit.click = true;
        s.edit.set_flag("click.only_during_record", true);
        let idle = render_playback(&s, 48_000, 512, false);
        assert!(idle.iter().all(|c| c.iter().all(|x| *x == 0.0)));
        let rec = render_playback(&s, 48_000, 512, true);
        assert!(rec.iter().any(|c| c.iter().any(|x| x.abs() > 0.1)));
    }

    /// A session with one track of `fmt` playing a distinct DC level per channel (0.1, 0.2, …).
    fn multichannel_session(fmt: ChannelFormat, main: ChannelFormat, frames: usize) -> (Session, TrackId) {
        let mut s = Session::default();
        s.outputs[0].format = main;
        let t = s.add_track(TrackKind::Audio, fmt, None);
        let chans: Vec<Vec<f32>> = (0..fmt.channels()).map(|c| (0..frames).map(|i| 0.1 * (c + 1) as f32 + (i % 7) as f32 * 1e-3).collect()).collect();
        s.pool.insert(SourceId(901), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: chans })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "mc", SourceId(901), 0, 0, frames as i64));
        (s, t)
    }

    #[test]
    fn surround_track_passes_through_bit_exact() {
        let (s, _) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Surround51, 4800);
        let src = s.pool.get(SourceId(901)).unwrap().buffer.channels.clone();
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert_eq!(out.len(), 6);
        for (o, x) in out.iter().zip(src.iter()) {
            assert_eq!(o, x, "5.1 → 5.1 must be bit-exact");
        }
    }

    #[test]
    fn surround_to_stereo_folds_down_itu() {
        let (s, _) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Stereo, 4800);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert_eq!(out.len(), 2);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let x = |c: usize| 0.1 * (c + 1) as f32 + (1000 % 7) as f32 * 1e-3;
        // L R C LFE Ls Rs.
        let l = x(0) + h * x(2) + h * x(4);
        let r = x(1) + h * x(2) + h * x(5);
        assert!((out[0][1000] - l).abs() < 1e-5, "{} vs {l}", out[0][1000]);
        assert!((out[1][1000] - r).abs() < 1e-5, "{} vs {r}", out[1][1000]);
    }

    #[test]
    fn mono_track_on_surround_main_pans() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.outputs[0].format = ChannelFormat::Surround51;
        // No surround pan: stereo behaviour on L/R.
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert_eq!(out.len(), 6);
        let h = 0.5 * gains(0.0, PanLaw::Minus3).0;
        assert!((out[0][100] - h).abs() < 1e-6 && (out[1][100] - h).abs() < 1e-6);
        assert!(out[2..].iter().all(|c| c[100].abs() < 1e-7));
        // Front centre: the centre speaker only.
        s.track_mut(t).unwrap().mixer.surround = Some(SurroundPan::default());
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!((out[2][100] - 0.5).abs() < 1e-4, "{}", out[2][100]);
        assert!(out.iter().enumerate().all(|(c, x)| c == 2 || x[100].abs() < 1e-4));
        // Rear right with LFE: Rs plus the LFE send.
        s.track_mut(t).unwrap().mixer.surround = Some(SurroundPan { x: 1.0, y: -1.0, lfe_db: -6.0, ..SurroundPan::default() });
        let mut eng = MixEngine::new(48_000.0, 512);
        let mut buf = vec![vec![0.0; 512]; 6];
        eng.render(&s, 0, 512, &mut buf);
        assert!((buf[5][100] - 0.5).abs() < 1e-4 && (buf[3][100] - 0.5 * db_to_gain(-6.0)).abs() < 1e-4);
        assert_eq!(eng.main_meter.peaks.len(), 6);
        assert_eq!(eng.meters.get(&t).map(|m| m.peaks.len()), Some(1));
        // A stereo device buffer gets the first two channels.
        let mut two = vec![vec![1.0; 512]; 2];
        eng.render(&s, 512, 512, &mut two);
        assert!(two[0][10].abs() < 1e-6);
    }

    #[test]
    fn surround_busses_and_master_fader() {
        let (mut s, t) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Surround51, 4800);
        let bus = s.add_bus("Stem", ChannelFormat::Surround51);
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Surround51, Some("Stem"));
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        s.track_mut(t).unwrap().mixer.output = Route::Bus(bus);
        let m = s.add_track(TrackKind::Master, ChannelFormat::Surround51, None);
        s.track_mut(m).unwrap().mixer.volume_db = -6.0;
        let out = render_range(&s, Range::new(0, 4800), 512);
        let g = db_to_gain(-6.0);
        for (c, ch) in out.iter().enumerate() {
            let x = 0.1 * (c + 1) as f32 + (1000 % 7) as f32 * 1e-3;
            assert!((ch[1000] - x * g).abs() < 1e-5, "channel {c}: {} vs {}", ch[1000], x * g);
        }
        // A mono bus sums a stereo source like the old stereo bus did.
        let (mut s, t) = session_with_dc(0.5, 4800);
        let bus = s.add_bus("Mono", ChannelFormat::Mono);
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Mono, Some("M"));
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        s.track_mut(aux).unwrap().mixer.pan = vec![-1.0];
        s.track_mut(t).unwrap().mixer.output = Route::Bus(bus);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!((out[0][1000] - 0.5 * gains(0.0, PanLaw::Minus3).0).abs() < 1e-6, "{}", out[0][1000]);
    }

    #[test]
    fn surround_upmix_and_downmix_between_layouts() {
        // 5.1 → 7.1: shared speakers map 1:1; 5.x surrounds pan between the 7.1 sides and rears.
        let (s, _) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Surround71, 2000);
        let out = render_range(&s, Range::new(0, 2000), 512);
        assert_eq!(out.len(), 8);
        let x = |c: usize| 0.1 * (c + 1) as f32 + (100 % 7) as f32 * 1e-3;
        for (c, ch) in out.iter().enumerate().take(4) {
            assert!((ch[100] - x(c)).abs() < 1e-6);
        }
        let e_ls = out[4][100].powi(2) + out[6][100].powi(2);
        assert!((e_ls - x(4).powi(2)).abs() < 1e-5, "Ls energy is preserved");
    }
}
