//! Prepared render plans (F01b, mix side): the types a control-thread [`PlanCompiler`] fills and
//! a realtime [`MixEngine`] adopts in place with [`MixEngine::adopt_plan`].
//!
//! A [`PlanEnvelope`] carries one plan from the control thread to the audio thread and back. On
//! the audio thread the engine validates it without changing anything, then only swaps values
//! between itself and the envelope: reused cells (strip state, plug-in slots) move into the new
//! tables, the new tables and the Session take the place of the old ones, and the old ones,
//! with everything they still own, go back in the same envelope. Nothing is allocated or freed
//! on the audio thread, and at every instant each owner is either in the engine or in the
//! envelope, also if a commit unwinds.
//!
//! [`PlanCompiler`]: crate::PlanCompiler

use super::{APPLIED_INLINE, BusTable, ChannelFormat, PluginSlot, SampleSource, Session, SourceAudio, Strip, StripMeter, TrackId};
use soundcraft_dsp::Plugin;
use std::sync::Arc;

/// A plan's place in the sequence of plans an engine adopts. [`PlanGeneration::EMPTY`] is a
/// fresh engine's. Generations only advance, with checked arithmetic, and never wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PlanGeneration(u64);

impl PlanGeneration {
    /// A fresh engine's generation (no plan adopted yet).
    pub const EMPTY: Self = PlanGeneration(0);

    /// A generation from its number.
    #[must_use]
    pub const fn from_raw(n: u64) -> Self {
        PlanGeneration(n)
    }

    /// The generation's number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next generation, `None` once the counter is exhausted (it never wraps).
    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(PlanGeneration)
    }
}

/// Playback's runtime device/rate generation. A new device or rate is a new engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DeviceGeneration(pub u64);

/// What an engine renders for: fixed for the lifetime of a [`MixEngine`] and its
/// [`PlanCompiler`](crate::PlanCompiler).
///
/// [`MixEngine`]: crate::MixEngine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderTarget {
    pub device: DeviceGeneration,
    pub sample_rate: u32,
    /// Largest block rendered at once. Clamped to 16..=16384 by the engine and the compiler alike.
    pub max_block: u32,
}

impl RenderTarget {
    /// The target with `max_block` clamped the way [`MixEngine::new`](crate::MixEngine::new) clamps it.
    pub(crate) fn normalized(self) -> Self {
        RenderTarget { max_block: self.max_block.clamp(16, 16_384), ..self }
    }

    pub(crate) fn block(self) -> usize {
        self.max_block as usize
    }
}

/// Which plan this is, which one it replaces, and what it was compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanIdentity {
    pub generation: PlanGeneration,
    /// The active generation the plan was compiled against: adoption requires it.
    pub base: PlanGeneration,
    pub target: RenderTarget,
}

/// Identity of a strip's state or a plug-in slot across plans. A plan may move a cell only when
/// its generation is the one the compiler saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SlotGen(pub(crate) u64);

/// One plug-in instance: its track, its slot ([`INSTRUMENT_SLOT`](crate::INSTRUMENT_SLOT), an
/// insert index or a [`clip_fx_slot`](crate::clip_fx_slot)), and its [`SlotGen`]. The compiler
/// gives each instance its generation before the factory makes it, so playback can bind handles
/// (editors, state readers) to exactly that instance.
pub type SlotBinding = (TrackId, usize, SlotGen);

impl SlotGen {
    /// Not a cell: a strip shell waiting for its state, or legacy (non-plan) storage.
    pub const VACANT: Self = SlotGen(0);

    /// The generation's number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What a cell holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellKind {
    /// A strip's running state (held notes, delay lines, meter, ...).
    Strip,
    Insert,
    Instrument,
    ClipEq,
    ClipComp,
}

/// A cell's kind, channel count and channel-format code. A move needs equal shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) kind: CellKind,
    pub(crate) ch: u16,
    pub(crate) layout: u16,
}

/// Where a cell sits in a plan's tables: its strip and, for an insert its slot index, for a clip
/// module its entry in the strip's clip-effect table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellLoc {
    pub(crate) strip: u16,
    pub(crate) index: u16,
    pub(crate) shape: Shape,
}

/// Move the cell `from` of the active plan into the vacant cell `to` of the new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellMove {
    pub(crate) kind: CellKind,
    pub(crate) from: u16,
    pub(crate) to: u16,
    pub(crate) expect: SlotGen,
}

/// Storage swapped into a cell of the new tables that is live after the moves. `from_move` is the index
/// of the move that fills the cell (`None` for a fresh instance), so validation is O(1) per attachment.
pub(crate) enum Attach {
    /// NaN-filled parameter storage of the slot's new (long) map length.
    Applied { cell: u16, from_move: Option<u16>, storage: Vec<f32> },
    /// The native Sampler's new sample: both owners. `set_sample` runs after the swap.
    Sample { cell: u16, from_move: Option<u16>, audio: Option<Arc<SourceAudio>>, wrapper: Option<Arc<dyn SampleSource>> },
}

/// Every table a realtime engine renders from, sized off the audio thread.
#[derive(Default)]
pub(crate) struct PlanTables {
    /// Render order: `..phase1` the live tracks other than auxes (processing order), then
    /// `phase1..aux_end` every aux and folder (dependency order), then the rest (masters, VCAs,
    /// inactive tracks) in track order.
    pub(crate) strips: Vec<Strip>,
    pub(crate) phase1: usize,
    pub(crate) aux_end: usize,
    /// Cell index → location: the strips first (cell `i` is strip `i`), then the slots.
    pub(crate) cells: Box<[CellLoc]>,
    /// Sorted by track id: the strip of each track.
    pub(crate) tracks: Box<[(TrackId, u16)]>,
    pub(crate) busses: BusTable,
    pub(crate) main: Vec<Vec<f32>>,
    pub(crate) main_fmt: ChannelFormat,
    pub(crate) main_meter: StripMeter,
}

impl PlanTables {
    pub(crate) fn strip_of(&self, track: TrackId) -> Option<usize> {
        let i = self.tracks.binary_search_by_key(&track, |(t, _)| *t).ok()?;
        self.tracks.get(i).map(|(_, s)| usize::from(*s))
    }

    /// The slot cell at `loc` (not a strip cell).
    pub(crate) fn slot(&self, loc: &CellLoc) -> Option<&Option<PluginSlot>> {
        let st = self.strips.get(usize::from(loc.strip))?;
        let i = usize::from(loc.index);
        match loc.shape.kind {
            CellKind::Strip => None,
            CellKind::Insert => st.plugins.get(i),
            CellKind::Instrument => Some(&st.instrument),
            CellKind::ClipEq => st.clip_fx.0.get(i).map(|(_, fx)| &fx.eq),
            CellKind::ClipComp => st.clip_fx.0.get(i).map(|(_, fx)| &fx.comp),
        }
    }

    pub(crate) fn slot_mut(&mut self, loc: &CellLoc) -> Option<&mut Option<PluginSlot>> {
        let st = self.strips.get_mut(usize::from(loc.strip))?;
        let i = usize::from(loc.index);
        match loc.shape.kind {
            CellKind::Strip => None,
            CellKind::Insert => st.plugins.get_mut(i),
            CellKind::Instrument => Some(&mut st.instrument),
            CellKind::ClipEq => st.clip_fx.0.get_mut(i).map(|(_, fx)| &mut fx.eq),
            CellKind::ClipComp => st.clip_fx.0.get_mut(i).map(|(_, fx)| &mut fx.comp),
        }
    }

    /// Every plug-in instance in the tables.
    pub(crate) fn plugins(&self) -> impl Iterator<Item = &PluginSlot> {
        self.strips.iter().flat_map(|st| {
            st.plugins.iter().flatten().chain(st.instrument.iter()).chain(st.clip_fx.0.iter().flat_map(|(_, fx)| fx.eq.iter().chain(fx.comp.iter())))
        })
    }

    /// The binding (track, slot, generation) of every instance in the tables.
    pub(crate) fn bindings(&self) -> Vec<SlotBinding> {
        let mut out = Vec::new();
        for st in &self.strips {
            for (i, p) in st.plugins.iter().enumerate() {
                if let Some(p) = p {
                    out.push((st.id, i, p.generation));
                }
            }
            if let Some(p) = &st.instrument {
                out.push((st.id, crate::INSTRUMENT_SLOT, p.generation));
            }
            for (clip, fx) in &st.clip_fx.0 {
                for (m, comp) in [(&fx.eq, false), (&fx.comp, true)] {
                    if let (Some(p), Some(k)) = (m, crate::clip_fx_slot(*clip, comp)) {
                        out.push((st.id, k, p.generation));
                    }
                }
            }
        }
        out
    }
}

/// One compiled plan: the Session, the new tables (none when the structure is unchanged), the
/// cell moves and attachments that carry state over, and the claim scratch validation marks.
pub struct PreparedPlan {
    pub(crate) id: PlanIdentity,
    pub(crate) session: Arc<Session>,
    pub(crate) tables: Option<PlanTables>,
    pub(crate) moves: Box<[CellMove]>,
    pub(crate) attach: Box<[Attach]>,
    /// One per cell of the active plan.
    pub(crate) src_claimed: Box<[bool]>,
    /// One per cell of `tables`.
    pub(crate) dst_claimed: Box<[bool]>,
    /// Sorted by id: each track and its strip index in the plan.
    pub(crate) tracks: Box<[(TrackId, u16)]>,
}

impl PreparedPlan {
    /// The plan's identity.
    #[must_use]
    pub fn id(&self) -> &PlanIdentity {
        &self.id
    }

    /// True when the plan carries new tables (a structural edit), false for a Session-only plan.
    #[must_use]
    pub fn is_structural(&self) -> bool {
        self.tables.is_some()
    }

    /// Plug-in instances in the plan's tables: before adoption the fresh ones, after it the ones
    /// the plan displaced.
    #[must_use]
    pub fn instances(&self) -> usize {
        self.tables.as_ref().map_or(0, |t| t.plugins().count())
    }
}

/// Where an envelope is in its round trip. A state, not a variant holding the plan: the plan
/// never moves between variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeState {
    /// Sent, not serviced yet.
    Pending,
    /// Adopted: the envelope now holds what the plan displaced.
    Returned { replaced: PlanGeneration },
    /// Refused whole: the envelope still holds its entire input.
    Rejected { reason: RejectReason, active: PlanGeneration },
}

/// Why [`MixEngine::adopt_plan`](crate::MixEngine::adopt_plan) refused a plan. Nothing changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The plan was not compiled against the active generation (or does not advance it).
    BaseMismatch,
    /// Compiled for another device generation, rate or maximum block.
    TargetMismatch,
    /// A move or the claim scratch names a cell that does not exist.
    MoveOutOfRange,
    /// Two moves take the same active cell.
    DuplicateSource,
    /// Two moves fill the same new cell.
    DuplicateDestination,
    /// A move's source cell holds nothing.
    SourceNotLive,
    /// A move's source holds another generation than the compiler saw.
    SlotGenMismatch,
    /// A move's destination is already filled.
    DestinationNotVacant,
    /// A move's source and destination differ in kind, channel count or channel format.
    ShapeMismatch,
    /// An attachment targets a cell that is not a live slot of its kind after the moves.
    AttachInvalid,
    /// An earlier adoption unwound: the engine may be half-adopted and adopts nothing more.
    Faulted,
}

/// What [`MixEngine::adopt_plan`](crate::MixEngine::adopt_plan) did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adoption {
    Adopted,
    Rejected(RejectReason),
    /// The envelope was not pending (adopted or rejected before): nothing done.
    AlreadyServiced,
}

/// The owning envelope of one plan, from the control thread to the audio thread and back.
pub struct PlanEnvelope {
    pub(crate) plan: PreparedPlan,
    pub(crate) state: EnvelopeState,
    /// Set when a commit starts. Still `Pending` with this set means the commit unwound: the
    /// envelope is torn (it may hold old and new cells alike) and nothing in it is reused.
    pub(crate) committing: bool,
}

impl std::fmt::Debug for PlanEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanEnvelope")
            .field("id", &self.plan.id)
            .field("state", &self.state)
            .field("structural", &self.plan.tables.is_some())
            .finish()
    }
}

impl PlanEnvelope {
    pub(crate) fn new(plan: PreparedPlan) -> Self {
        PlanEnvelope { plan, state: EnvelopeState::Pending, committing: false }
    }

    /// The envelope's state.
    #[must_use]
    pub fn state(&self) -> EnvelopeState {
        self.state
    }

    /// The plan inside (after adoption: the new plan's identity, the displaced contents).
    #[must_use]
    pub fn plan(&self) -> &PreparedPlan {
        &self.plan
    }

    /// True when an adoption of this envelope unwound mid-commit: it is still `Pending`, the
    /// engine is faulted, and the envelope may hold cells of both plans. Tear down both.
    #[must_use]
    pub fn torn(&self) -> bool {
        self.committing && self.state == EnvelopeState::Pending
    }

    /// Control thread only: what came back. `Ok` holds everything the adopted plan displaced;
    /// `Err` holds the whole refused (or never serviced) input. Dropping either frees it here.
    pub fn into_outcome(self) -> Result<Retired, Rejected> {
        match self.state {
            EnvelopeState::Returned { .. } => Ok(Retired::from_plan(self.plan)),
            EnvelopeState::Rejected { reason, active } => Err(Rejected { reason: Some(reason), active, plan: Box::new(self.plan) }),
            EnvelopeState::Pending => Err(Rejected { reason: None, active: self.plan.id.base, plan: Box::new(self.plan) }),
        }
    }
}

/// Plan-mode problems the audio thread noticed, in fixed storage. `Copy`; playback copies it to
/// telemetry. Reset when a plan is adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanStatus {
    /// Blocks in which a delay line had to clamp a delay to its capacity.
    pub capacity_exceeded: u32,
    /// The largest delay asked for beyond capacity (samples): what the next plan should hold.
    pub required_latency: u32,
    /// The strip (track and strip generation) that asked for it last.
    pub affected: Option<(TrackId, SlotGen)>,
    /// Parameter maps whose length did not match their prepared storage.
    pub applied_mismatch: u32,
}

/// Everything an adopted plan displaced, or a torn-down engine held: drop it on the control
/// thread (the plug-in lifecycle thread).
pub struct Retired {
    pub(crate) session: Option<Arc<Session>>,
    pub(crate) tables: Option<PlanTables>,
    /// Displaced attachment storage: held so it drops with the rest, on the control thread.
    pub(crate) _attach: Box<[Attach]>,
    pub(crate) legacy: Vec<Box<dyn Plugin>>,
}

impl Retired {
    pub(crate) fn from_plan(p: PreparedPlan) -> Self {
        Retired { session: Some(p.session), tables: p.tables, _attach: p.attach, legacy: Vec::new() }
    }

    /// The plug-in instances inside.
    pub fn plugins(&self) -> impl Iterator<Item = &dyn Plugin> {
        self.tables.iter().flat_map(PlanTables::plugins).map(|s| &*s.plugin).chain(self.legacy.iter().map(|p| &**p))
    }

    /// The Session inside (the displaced one).
    #[must_use]
    pub fn session(&self) -> Option<&Arc<Session>> {
        self.session.as_ref()
    }

    /// The binding of every plan-mode instance inside: dropping `self` destroys them, so
    /// playback releases their handles (legacy instances have none).
    #[must_use]
    pub fn instances(&self) -> Vec<SlotBinding> {
        self.tables.as_ref().map(PlanTables::bindings).unwrap_or_default()
    }
}

/// A refused or never-serviced plan, whole.
pub struct Rejected {
    /// `None` when the plan was never serviced.
    pub reason: Option<RejectReason>,
    /// The engine's active generation when it refused.
    pub active: PlanGeneration,
    pub(crate) plan: Box<PreparedPlan>,
}

impl Rejected {
    /// The refused plan.
    #[must_use]
    pub fn plan(&self) -> &PreparedPlan {
        &self.plan
    }
}

/// The active plan of a realtime engine.
pub(crate) struct ActivePlan {
    pub(crate) id: PlanIdentity,
    pub(crate) session: Arc<Session>,
    pub(crate) tables: PlanTables,
    pub(crate) status: PlanStatus,
    /// Set while a commit runs; still set after an unwind out of one. Render and adoption stop.
    pub(crate) faulted: bool,
}

/// Parameter maps longer than this need prepared storage (`Attach::Applied`).
pub(crate) const fn needs_storage(params: usize) -> bool {
    params > APPLIED_INLINE
}
