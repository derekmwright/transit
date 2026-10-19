//! Prepared playback storage travels with the mix plan through one retirement credit.
//!
//! This is the device-independent bridge for Player integration. Both endpoints must live
//! in control-owned holders; callbacks borrow the audio endpoint. The caller admits a plan
//! only at a drained render-ahead boundary, and shares one [`CallbackBudget`] across every
//! conversion chunk of an actual device callback. Device/rate transitions need a new pair.
//! The legacy Player path is replaced separately after this bridge's contract tests.

use crate::{MeterSnapshot, fold_gains, publication, record::InputRing, session_has_hosted};
use soundcraft_mix::{
    Adoption, CompileError, EnvelopeState, InstanceSpec, MAX_CHANNELS, MixEngine, NewInstance, PlanCompiler, PlanEnvelope, PlanGeneration,
    PlanLimits, PlanStatus, RenderTarget, SlotBinding, SlotGen, StripMeter, main_channels,
};
use soundcraft_model::{Session, TrackId, TrackKind};
use soundcraft_time::Samples;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Playback-owned sample/meter storage cap per publication, in addition to PlanLimits.
const PLAYBACK_BYTES: usize = 16 << 20;

/// Independent meter fields may straddle a render block; meters are display diagnostics.
/// No reader takes the callback's lock or asks it to allocate a snapshot.
struct AtomicMeter {
    peak: [AtomicU32; 2],
    peaks: [AtomicU32; MAX_CHANNELS],
    channels: AtomicUsize,
    reduction: AtomicU32,
    failures: AtomicU32,
    last_failure: AtomicUsize,
}

impl AtomicMeter {
    fn new() -> Self {
        Self {
            peak: std::array::from_fn(|_| AtomicU32::new(0)),
            peaks: std::array::from_fn(|_| AtomicU32::new(0)),
            channels: AtomicUsize::new(0),
            reduction: AtomicU32::new(0),
            failures: AtomicU32::new(0),
            last_failure: AtomicUsize::new(0),
        }
    }

    fn publish(&self, source: &StripMeter) {
        for (dst, src) in self.peak.iter().zip(source.peak) {
            dst.store(src.to_bits(), Ordering::Relaxed);
        }
        for (dst, src) in self.peaks.iter().zip(&source.peaks) {
            dst.store(src.to_bits(), Ordering::Relaxed);
        }
        self.channels.store(source.peaks.len().min(MAX_CHANNELS), Ordering::Relaxed);
        self.reduction.store(source.gain_reduction_db.to_bits(), Ordering::Relaxed);
        self.failures.store(source.plugin_failures.count, Ordering::Relaxed);
        self.last_failure.store(source.plugin_failures.last, Ordering::Relaxed);
    }

    fn clear(&self) {
        for peak in self.peak.iter().chain(&self.peaks) {
            peak.store(0, Ordering::Relaxed);
        }
        self.reduction.store(0, Ordering::Relaxed);
    }

    fn copy_into(&self, out: &mut StripMeter) {
        for (dst, src) in out.peak.iter_mut().zip(&self.peak) {
            *dst = f32::from_bits(src.load(Ordering::Relaxed));
        }
        out.peaks.resize(self.channels.load(Ordering::Relaxed).min(MAX_CHANNELS), 0.0);
        for (dst, src) in out.peaks.iter_mut().zip(&self.peaks) {
            *dst = f32::from_bits(src.load(Ordering::Relaxed));
        }
        out.gain_reduction_db = f32::from_bits(self.reduction.load(Ordering::Relaxed));
        out.plugin_failures.count = self.failures.load(Ordering::Relaxed);
        out.plugin_failures.last = self.last_failure.load(Ordering::Relaxed);
    }
}

struct MeterBank {
    tracks: Vec<(TrackId, AtomicMeter)>,
    main: AtomicMeter,
}

impl MeterBank {
    fn new(session: &Session) -> Self {
        let mut tracks: Vec<_> = session.tracks.iter().map(|t| (t.id, AtomicMeter::new())).collect();
        tracks.sort_unstable_by_key(|(id, _)| *id);
        Self { tracks, main: AtomicMeter::new() }
    }

    fn publish(&self, mix: &MixEngine) {
        self.main.publish(mix.active_main_meter());
        for (id, meter) in mix.meters() {
            if let Ok(index) = self.tracks.binary_search_by_key(&id, |(id, _)| *id)
                && let Some((_, dst)) = self.tracks.get(index)
            {
                dst.publish(meter);
            }
        }
    }

    fn clear(&self) {
        self.main.clear();
        for (_, meter) in &self.tracks {
            meter.clear();
        }
    }

    fn copy_into(&self, out: &mut MeterSnapshot) {
        out.tracks.retain(|id, _| self.tracks.binary_search_by_key(id, |(id, _)| *id).is_ok());
        for (id, meter) in &self.tracks {
            meter.copy_into(out.tracks.entry(*id).or_default());
        }
        self.main.copy_into(&mut out.main);
    }
}

/// All playback resources that change with a plan. Swapped whole, never dropped on audio.
struct Buffers {
    output: Vec<Vec<f32>>,
    input_storage: Vec<Vec<f32>>,
    input: Option<Arc<InputRing>>,
    meters: Arc<MeterBank>,
    fold: [(f32, f32); MAX_CHANNELS],
    hosted: bool,
    monitoring: bool,
}

impl Buffers {
    fn prepare(session: &Session, input: Option<Arc<InputRing>>, target: RenderTarget, limits: PlanLimits) -> Result<Self, CompileError> {
        let channels = main_channels(session);
        let input_channels = input.as_ref().map_or(0, |r| r.channels.load(Ordering::Relaxed).clamp(1, u16::MAX as usize));
        let block = target.max_block.clamp(16, 16_384) as usize;
        let bytes = channels
            .checked_add(input_channels)
            .and_then(|n| {
                block
                    .checked_mul(size_of::<f32>())
                    .and_then(|samples| samples.checked_add(size_of::<Vec<f32>>()))
                    .and_then(|channel_bytes| n.checked_mul(channel_bytes))
            })
            .and_then(|n| {
                session
                    .tracks
                    .len()
                    .checked_add(1)
                    .and_then(|tracks| tracks.checked_mul(size_of::<(TrackId, AtomicMeter)>()))
                    .and_then(|meters| n.checked_add(meters))
            })
            .and_then(|n| n.checked_add(size_of::<Buffers>() + size_of::<MeterBank>() + 2 * size_of::<usize>()))
            .unwrap_or(usize::MAX);
        if bytes > PLAYBACK_BYTES {
            return Err(CompileError::ResourceLimit { what: "playback storage", need: bytes, limit: PLAYBACK_BYTES });
        }
        if session.tracks.len() > usize::from(limits.tracks) {
            return Err(CompileError::ResourceLimit { what: "playback meter tracks", need: session.tracks.len(), limit: usize::from(limits.tracks) });
        }
        Ok(Self {
            output: vec![vec![0.0; block]; channels],
            input_storage: vec![vec![0.0; block]; input_channels],
            monitoring: session.tracks.iter().any(|t| {
                (t.mixer.input_monitor || t.mixer.record_arm) && (input.is_some() || matches!(t.kind, TrackKind::Instrument | TrackKind::Midi))
            }),
            input,
            meters: Arc::new(MeterBank::new(session)),
            fold: fold_gains(session),
            hosted: session_has_hosted(session),
        })
    }
}

struct Envelope {
    plan: PlanEnvelope,
    buffers: Buffers,
}

struct Request {
    session: Arc<Session>,
    input: Option<Arc<InputRing>>,
}

/// A coherent Copy report; the queue never combines fields from different generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusReport {
    pub generation: PlanGeneration,
    pub status: PlanStatus,
}

/// Created once per actual device callback, then borrowed across scratch chunks and blocks.
/// There is no public constructor; the device adapter owns the scope.
/// ```compile_fail
/// let _ = soundcraft_playback::plan_bridge::CallbackBudget::default();
/// ```
pub struct CallbackBudget {
    serviced: bool,
    deferral_counted: bool,
}

// Only the parent device adapter can mint production tokens. Tests are inside this
// module so no public constructor or Default implementation is needed for fixtures.
pub(super) fn callback_scope() -> CallbackBudget {
    CallbackBudget { serviced: false, deferral_counted: false }
}

#[derive(Default)]
struct Counters {
    faulted: AtomicBool,
    published: AtomicU64,
    admitted: AtomicU64,
    deferred: AtomicU64,
    age: AtomicU64,
    maximum_age: AtomicU64,
}

/// Identity of this actual pair, independent of numeric device/plan generations.
pub(super) struct RuntimeIdentity(Arc<Counters>);
impl RuntimeIdentity {
    pub(super) fn faulted(&self) -> bool {
        self.0.faulted.load(Ordering::Acquire)
    }
}

/// Independent diagnostic fields can straddle a callback. Age counts callbacks in
/// which a pending publication could not be admitted; it is not physical latency.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeferralTelemetry {
    pub published_generation: u64,
    pub admitted_generation: u64,
    pub deferred_callbacks: u64,
    pub current_age_callbacks: u64,
    pub maximum_age_callbacks: u64,
}

/// The disposition and requested identity are reported even for a torn adoption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReturnReport {
    pub requested: PlanGeneration,
    pub state: EnvelopeState,
    pub torn: bool,
}

/// Main-thread compiler, publisher and reclaimer. Never held or dropped by a callback.
pub struct Control {
    compiler: PlanCompiler,
    port: publication::ControlPort<Envelope>,
    latest: Option<Request>,
    target: RenderTarget,
    limits: PlanLimits,
    issued: PlanGeneration,
    meters: Arc<MeterBank>,
    pending_meters: Option<Arc<MeterBank>>,
    status_rx: rtrb::Consumer<StatusReport>,
    status: Option<StatusReport>,
    counters: Arc<Counters>,
}

/// Borrowed by audio, anchored by control until quiescence. The caller owns transport and
/// render-ahead indices; [`Audio::service`] must not swap a still-audible output buffer.
pub struct Audio {
    port: publication::AudioPort<Envelope>,
    mix: MixEngine,
    buffers: Buffers,
    status_tx: rtrb::Producer<StatusReport>,
    counters: Arc<Counters>,
    deferred_age: u64,
    maximum_age: u64,
}

/// Prepare an empty fixed-target engine and one publication/return credit on control.
pub fn pair(target: RenderTarget, limits: PlanLimits) -> (Control, Audio) {
    let target = RenderTarget { max_block: target.max_block.clamp(16, 16_384), ..target };
    let (port, audio_port) = publication::channel(NonZeroUsize::MIN);
    let (status_tx, status_rx) = rtrb::RingBuffer::new(1);
    let counters = Arc::new(Counters::default());
    let meters = Arc::new(MeterBank { tracks: Vec::new(), main: AtomicMeter::new() });
    let buffers = Buffers {
        output: Vec::new(),
        input_storage: Vec::new(),
        input: None,
        meters: Arc::clone(&meters),
        fold: [(0.0, 0.0); MAX_CHANNELS],
        hosted: false,
        monitoring: false,
    };
    let mut mix = MixEngine::realtime(target);
    mix.metronome = true;
    (
        Control {
            compiler: PlanCompiler::new(target, limits),
            port,
            latest: None,
            target,
            limits,
            issued: PlanGeneration::EMPTY,
            meters,
            pending_meters: None,
            status_rx,
            status: None,
            counters: Arc::clone(&counters),
        },
        Audio { port: audio_port, mix, buffers, status_tx, counters, deferred_age: 0, maximum_age: 0 },
    )
}

impl Control {
    pub(super) fn runtime_identity(&self) -> RuntimeIdentity {
        RuntimeIdentity(Arc::clone(&self.counters))
    }
    /// Coalesce before construction. Replaced unsent owners die here, on control.
    pub fn request(&mut self, session: Arc<Session>, input: Option<Arc<InputRing>>) {
        self.latest = Some(Request { session, input });
    }

    /// Reclaim at most one return and observe one coherent status. Call before preparation.
    /// Drain [`Self::take_released`] after this and after every preparation attempt, even
    /// if the factory unwinds; handles are keyed by the entire SlotBinding.
    pub fn poll(&mut self) -> Option<ReturnReport> {
        let mut result = None;
        let Self { compiler, port, meters, pending_meters, .. } = self;
        port.reclaim_one(|env| {
            result = Some(ReturnReport { requested: env.plan.plan().id().generation, state: env.plan.state(), torn: env.plan.torn() });
            if matches!(env.plan.state(), EnvelopeState::Returned { .. })
                && let Some(new_meters) = pending_meters.take()
            {
                *meters = new_meters;
            } else {
                pending_meters.take();
            }
            compiler.reclaim(env.plan);
            drop(env.buffers);
        });
        if let Ok(report) = self.status_rx.pop()
            && report.generation == self.compiler.active()
            && !self.compiler.faulted()
        {
            self.compiler.observe(&report.status);
            self.status = Some(report);
        }
        result
    }

    /// Prepare only with a returned credit. Errors leave the active plan untouched; the
    /// failed request is consumed and can be corrected/resubmitted by the caller.
    pub fn prepare(&mut self, make: &mut dyn FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance>) -> Result<bool, CompileError> {
        if self.faulted() {
            return Err(CompileError::Faulted);
        }
        if self.port.credits() == 0 {
            return Ok(false);
        }
        let Some(request) = self.latest.take() else { return Ok(false) };
        if request.session.sample_rate.hz() != self.target.sample_rate {
            return Err(CompileError::TargetMismatch { compiler: self.target });
        }
        let next = self.issued.checked_next().ok_or(CompileError::GenerationExhausted)?;
        let buffers = Buffers::prepare(&request.session, request.input, self.target, self.limits)?;
        let plan = self.compiler.compile(request.session, self.compiler.active(), next, make)?;
        self.issued = next;
        self.pending_meters = Some(Arc::clone(&buffers.meters));
        let previous = self.counters.published.load(Ordering::Relaxed);
        self.counters.published.store(next.get(), Ordering::Release);
        if let Err(env) = self.port.try_publish(Envelope { plan, buffers }) {
            // Defensive invariant failure: ownership remains on control; no overflow queue.
            self.pending_meters.take();
            self.compiler.reclaim(env.plan);
            self.counters.published.store(previous, Ordering::Release);
            return Ok(false);
        }
        Ok(true)
    }

    pub fn active(&self) -> PlanGeneration {
        self.compiler.active()
    }
    pub(super) fn can_prepare(&self) -> bool {
        self.port.credits() != 0
    }
    pub(super) fn issued(&self) -> PlanGeneration {
        self.issued
    }

    /// A torn adoption or callback fault permanently faults this pair. Retire it on
    /// control after quiescence; ordinary admission must stop even before a return arrives.
    pub fn faulted(&self) -> bool {
        self.compiler.faulted() || self.counters.faulted.load(Ordering::Acquire)
    }
    /// Permanently close admission after a control/device failure. The current
    /// audio borrower observes this flag; main never borrows processing state.
    pub fn fault(&mut self) {
        self.counters.faulted.store(true, Ordering::Release);
    }
    pub fn active_bindings(&self) -> Vec<SlotBinding> {
        self.compiler.active_bindings()
    }
    pub fn pending_bindings(&self) -> Option<Vec<SlotBinding>> {
        self.compiler.pending_bindings()
    }
    pub fn take_released(&mut self) -> Vec<SlotBinding> {
        self.compiler.take_released()
    }
    pub fn status(&self) -> Option<StatusReport> {
        self.status.filter(|s| s.generation == self.compiler.active())
    }

    pub fn deferral_telemetry(&self) -> DeferralTelemetry {
        DeferralTelemetry {
            published_generation: self.counters.published.load(Ordering::Acquire),
            admitted_generation: self.counters.admitted.load(Ordering::Acquire),
            deferred_callbacks: self.counters.deferred.load(Ordering::Relaxed),
            current_age_callbacks: self.counters.age.load(Ordering::Relaxed),
            maximum_age_callbacks: self.counters.maximum_age.load(Ordering::Relaxed),
        }
    }

    /// Control-only snapshot. Channel/track allocation, when needed, happens here.
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        self.meters.copy_into(out);
    }

    /// After callback quiescence, reclaim both endpoints and every remaining owner on main.
    /// The returned bindings include live instances as well as compiler/queue retirement.
    pub fn retire(mut self, mut audio: Audio) -> Vec<SlotBinding> {
        self.compiler.clear_pool();
        self.poll();
        // N=1: either poll already recovered the only envelope, or this returns it pending.
        audio.port.service_one(|_| {});
        self.poll();
        let retired = audio.mix.into_retired();
        let mut released = self.compiler.take_released();
        released.extend(retired.instances());
        drop(retired);
        released
    }
}

/// Borrowed processing controls for an action handler. It cannot adopt, retire or
/// replace the engine while an exact-plan action is being dispatched.
///
/// ```compile_fail
/// use soundcraft_playback::plan_bridge::{CallbackBudget, Processing};
/// fn adopt_during_action(view: &mut Processing<'_>, budget: &mut CallbackBudget) {
///     view.service(budget, true);
/// }
/// ```
/// ```compile_fail
/// use soundcraft_playback::plan_bridge::{Audio, Processing};
/// fn swap_runtime(view: &mut Processing<'_>, foreign: &mut Audio) {
///     std::mem::swap(view, foreign);
/// }
/// ```
/// ```compile_fail
/// use soundcraft_playback::plan_bridge::{Audio, Processing};
/// fn requires_owner_access<T: std::ops::DerefMut<Target = Audio>>() {}
/// requires_owner_access::<Processing<'static>>();
/// ```
pub struct Processing<'a> {
    audio: &'a mut Audio,
}

impl Processing<'_> {
    /// Refuse subsequent work immediately if a handler detects an unrecoverable
    /// runtime error. This cannot change plan identity or invoke processor code.
    pub fn fault(&mut self) {
        self.audio.fault();
    }
    pub fn set_processing(&mut self, monitor_only: bool, recording: bool, editors_open: bool) {
        self.audio.set_processing(monitor_only, recording, editors_open);
    }
    pub fn reset(&mut self) {
        self.audio.reset();
    }
    pub fn all_notes_off(&mut self) {
        self.audio.all_notes_off();
    }
    pub fn flush_releases(&mut self, frames: usize) {
        self.audio.flush_releases(frames);
    }
    pub fn live_note(&mut self, track: TrackId, pitch: u8, velocity: u8) {
        self.audio.live_note(track, pitch, velocity);
    }
}

impl Audio {
    pub(super) fn processing(&mut self) -> Processing<'_> {
        Processing { audio: self }
    }
    pub(super) fn matches_runtime(&self, identity: &RuntimeIdentity) -> bool {
        Arc::ptr_eq(&self.counters, &identity.0)
    }
    fn deferred(&mut self, budget: &mut CallbackBudget) {
        if !budget.deferral_counted && self.counters.published.load(Ordering::Acquire) > self.counters.admitted.load(Ordering::Relaxed) {
            budget.deferral_counted = true;
            self.deferred_age = self.deferred_age.saturating_add(1);
            self.maximum_age = self.maximum_age.max(self.deferred_age);
            self.counters.deferred.fetch_add(1, Ordering::Relaxed);
            self.counters.age.store(self.deferred_age, Ordering::Relaxed);
            self.counters.maximum_age.store(self.maximum_age, Ordering::Relaxed);
        }
    }

    fn reset_age(&mut self) {
        self.deferred_age = 0;
        self.counters.age.store(0, Ordering::Relaxed);
    }
    /// Attempt at most one publication per device callback, only at a drained boundary.
    /// `ready` is false while old render-ahead samples still need their old layout/fold.
    pub fn service(&mut self, budget: &mut CallbackBudget, ready: bool) -> publication::Service {
        if self.faulted() {
            return self.return_pending(budget);
        }
        if budget.serviced || !ready {
            self.deferred(budget);
            return publication::Service::Empty;
        }
        budget.serviced = true;
        let Self { port, mix, buffers, counters, .. } = self;
        let result = port.service_one(|env| {
            counters.admitted.store(env.plan.plan().id().generation.get(), Ordering::Release);
            if mix.adopt_plan(&mut env.plan) == Adoption::Adopted {
                std::mem::swap(buffers, &mut env.buffers);
                std::mem::swap(&mut mix.input, &mut buffers.input_storage);
                // input_storage belongs to the displaced plan and travels back in the envelope.
                std::mem::swap(&mut buffers.input_storage, &mut env.buffers.input_storage);
            }
        });
        if result == publication::Service::Empty {
            self.deferred(budget);
        } else {
            self.reset_age();
        }
        result
    }

    /// Fault/stopped-device service: return unadopted or interrupted work without rendering.
    pub fn return_pending(&mut self, budget: &mut CallbackBudget) -> publication::Service {
        if budget.serviced {
            self.deferred(budget);
            return publication::Service::Empty;
        }
        budget.serviced = true;
        let counters = &self.counters;
        let result = self.port.service_one(|env| {
            counters.admitted.store(env.plan.plan().id().generation.get(), Ordering::Release);
        });
        if result == publication::Service::Empty {
            self.deferred(budget);
        } else {
            self.reset_age();
        }
        result
    }

    pub fn engine(&self) -> &MixEngine {
        &self.mix
    }
    /// Publish permanent callback failure without invoking or destroying processors.
    pub fn fault(&mut self) {
        self.counters.faulted.store(true, Ordering::Release);
    }
    pub fn faulted(&self) -> bool {
        self.mix.faulted() || self.counters.faulted.load(Ordering::Acquire)
    }
    pub fn set_processing(&mut self, monitor_only: bool, recording: bool, editors_open: bool) {
        self.mix.monitor_only = monitor_only;
        self.mix.recording = recording;
        self.mix.no_suspend = editors_open;
    }
    pub fn reset(&mut self) {
        if !self.faulted() {
            self.mix.reset();
        }
    }
    pub fn all_notes_off(&mut self) {
        if !self.faulted() {
            self.mix.all_notes_off();
        }
    }
    pub fn flush_releases(&mut self, frames: usize) {
        if !self.faulted() {
            self.mix.flush_releases(frames);
        }
    }
    pub fn live_note(&mut self, track: TrackId, pitch: u8, velocity: u8) {
        if !self.faulted() {
            self.mix.live_note(track, pitch, velocity);
        }
    }
    pub fn output(&self) -> &[Vec<f32>] {
        &self.buffers.output
    }
    pub fn fold(&self) -> &[(f32, f32); MAX_CHANNELS] {
        &self.buffers.fold
    }
    pub fn hosted(&self) -> bool {
        self.buffers.hosted
    }
    pub fn monitoring(&self) -> bool {
        self.buffers.monitoring
    }

    /// Render into the prepared output storage. Frames are capped at the fixed target block.
    pub fn render(&mut self, position: Samples, frames: usize) {
        if self.faulted() {
            for channel in &mut self.buffers.output {
                channel.iter_mut().take(frames).for_each(|sample| *sample = 0.0);
            }
            self.clear_meters();
            return;
        }
        let target = self.mix.active().target;
        let frames = frames.min(target.max_block as usize);
        if let Some(input) = &self.buffers.input {
            input.pop_into(&mut self.mix.input, frames, target.sample_rate);
        }
        self.mix.render_active(position, frames, &mut self.buffers.output);
        self.buffers.meters.publish(&self.mix);
        // Full means an older report still awaits control. The next render tries its current
        // cumulative status, not a queue of stale reports; no field is published separately.
        let _ = self.status_tx.push(StatusReport { generation: self.mix.active().generation, status: self.mix.status() });
    }

    pub fn clear_meters(&self) {
        self.buffers.meters.clear();
    }
}

#[cfg(test)]
mod tests;
