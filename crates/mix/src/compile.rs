//! The control-thread plan compiler: turns an `Arc<Session>` plus the confirmed active plan into
//! a [`PlanEnvelope`] whose tables are sized and bounded before the audio thread sees them.

use super::plan::{Attach, CellKind, CellLoc, CellMove, PlanTables, PreparedPlan, Shape, SlotBinding, needs_storage};
use super::*;
use std::collections::{BTreeMap, HashMap};

/// Resource limits of one plan. Every size is checked (with checked arithmetic) before anything
/// is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    pub tracks: u16,
    pub buses: u16,
    /// Insert slots per strip.
    pub slots_per_strip: u16,
    /// Clips with clip effects per strip.
    pub clip_fx_per_strip: u16,
    /// Largest delay-line capacity (samples per channel) a plan prepares.
    pub delay_samples: u32,
    /// Cap on the bytes one plan prepares (buffers, delay lines, parameter storage, tables).
    pub prepared_bytes: usize,
}

impl Default for PlanLimits {
    fn default() -> Self {
        PlanLimits {
            tracks: 1024,
            buses: 1024,
            slots_per_strip: 64,
            clip_fx_per_strip: 4096,
            delay_samples: MAX_LATENCY as u32,
            prepared_bytes: 1 << 30,
        }
    }
}

/// Why [`PlanCompiler::compile`] made no plan. Nothing was published and the compiler is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileError {
    /// `base` is not the confirmed active generation.
    BaseNotActive { base: PlanGeneration, active: PlanGeneration },
    /// `next` does not advance past the last generation this compiler issued.
    GenerationNotIncreasing { last: PlanGeneration, next: PlanGeneration },
    /// The generation counter is at its end; it never wraps.
    GenerationExhausted,
    /// The Session's sample rate is not the compiler's target rate (a runtime transition).
    TargetMismatch { compiler: RenderTarget },
    /// A count or the prepared bytes exceed the limits (`need` is `usize::MAX` on overflow).
    ResourceLimit { what: &'static str, need: usize, limit: usize },
    /// A plan is still in flight (N = 1): reclaim its envelope first.
    InFlight { pending: PlanGeneration },
    /// A torn envelope was reclaimed ([`PlanCompiler::faulted`]): the engine is faulted and only
    /// teardown remains. No plan is made, so nothing can list a destroyed instance as pending.
    Faulted,
}

/// Smallest delay-line capacity a plan prepares (samples per channel).
pub const MIN_DELAY_CAPACITY: usize = 8_192;

/// A slot the compiler knows the active plan holds.
#[derive(Clone)]
struct ShadowSlot {
    cell: u16,
    /// [`SlotGen::VACANT`]: the factory made nothing for it (not retried until the spec changes).
    generation: SlotGen,
    spec: InstanceSpec,
    kind: CellKind,
    layout: u16,
    /// Length of its prepared long-map storage (0: none).
    heap: usize,
    /// Identity of the sample it was handed (0: none).
    sample: usize,
    latency: usize,
}

#[derive(Clone)]
struct ShadowStrip {
    track: TrackId,
    generation: SlotGen,
    ch: u16,
    layout: u16,
    /// Capacity of its live delay lines (samples per channel).
    cap: usize,
}

/// One strip of a plan's structure: track, track index, region, channels, layout, insert count,
/// delay capacity, clips with effects (id, eq, comp).
type StripDesc = (TrackId, u16, u8, u16, u16, usize, usize, Vec<(u64, bool, bool)>);

/// The structure a plan has, compared to decide whether a new plan needs tables at all.
#[derive(Clone, Default, PartialEq)]
struct Desc {
    strips: Vec<StripDesc>,
    /// Per slot: key, kind, plugin id, channels, layout, long map length (0 inline), sample.
    slots: Vec<((TrackId, usize), CellKind, String, usize, u16, usize, usize)>,
    busses: Vec<(BusId, ChannelFormat)>,
    main: Option<ChannelFormat>,
}

/// What the compiler knows of one plan's tables.
#[derive(Clone, Default)]
struct Shadow {
    generation: PlanGeneration,
    /// `None` for the empty plan of a fresh engine.
    desc: Option<Desc>,
    strips: Vec<ShadowStrip>,
    slots: HashMap<(TrackId, usize), ShadowSlot>,
    cells: usize,
    tracks: Box<[(TrackId, u16)]>,
}

impl Shadow {
    /// The latency of a track's instrument and inserts in this plan.
    fn track_latency(&self, track: TrackId) -> usize {
        self.slots
            .iter()
            .filter(|((t, _), sl)| *t == track && matches!(sl.kind, CellKind::Insert | CellKind::Instrument))
            .fold(0usize, |a, (_, sl)| a.saturating_add(sl.latency))
    }

    /// The live strip of `track` a new strip of this shape takes over (its index), if any.
    fn movable_strip(&self, track: TrackId, ch: u16, layout: u16) -> Option<(usize, &ShadowStrip)> {
        self.strips.iter().enumerate().find(|(_, x)| x.track == track && x.ch == ch && x.layout == layout && x.generation != SlotGen::VACANT)
    }

    /// Every live instance, sorted.
    fn bindings(&self) -> Vec<SlotBinding> {
        let mut out: Vec<SlotBinding> =
            self.slots.iter().filter(|(_, s)| s.generation != SlotGen::VACANT).map(|((t, k), s)| (*t, *k, s.generation)).collect();
        out.sort_unstable();
        out
    }
}

/// The plan in flight: its generation and shadow (`None`: Session-only, same tables).
struct Pending {
    generation: PlanGeneration,
    shadow: Option<Shadow>,
}

/// A fresh instance from a refused plan, kept for the next compile.
struct Pooled {
    key: (TrackId, usize),
    spec: InstanceSpec,
    slot: PluginSlot,
}

/// Reports the bindings of a plan under construction if an unwind drops it.
struct ReleaseOnUnwind<'a> {
    released: &'a mut Vec<SlotBinding>,
    fresh: Vec<SlotBinding>,
}

impl Drop for ReleaseOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.released.append(&mut self.fresh);
        }
    }
}

/// Compiles Sessions into plans for one realtime engine (one [`RenderTarget`]). Control thread.
///
/// Holds the shadow of the confirmed active plan plus at most one plan in flight, and a bounded
/// pool of the fresh instances of the last refused plan. Every instance it hands the factory a
/// [`SlotGen`] for and later destroys is reported once by [`PlanCompiler::take_released`].
pub struct PlanCompiler {
    target: RenderTarget,
    limits: PlanLimits,
    active: Shadow,
    pending: Option<Pending>,
    last: PlanGeneration,
    slot_gen: u64,
    pool: Vec<Pooled>,
    released: Vec<SlotBinding>,
    /// Delay a strip asked for beyond its capacity ([`PlanCompiler::observe`]), by track: the
    /// next plans size that strip's lines for it. Cleared once a confirmed plan holds it, and
    /// dropped when it does not fit the limits.
    hints: HashMap<TrackId, usize>,
    /// Teardown began ([`PlanCompiler::clear_pool`]): reclaimed instances are released, not pooled.
    closing: bool,
    /// A torn envelope came back: the engine is faulted and the active plan is what it still owns.
    faulted: Option<Vec<SlotBinding>>,
    /// Routing loops the last compiled session was loaded with (logged when they change).
    loops: Vec<RouteCycle>,
}

/// The planned content of one slot cell.
enum SlotPlan {
    Move { from: u16, generation: SlotGen, applied: Option<usize>, sample: Option<Option<Arc<SourceAudio>>> },
    Fresh(Box<PluginSlot>),
    Absent,
}

/// The planned content of one slot cell, with what the shadow needs.
struct PlannedSlot {
    key: (TrackId, usize),
    kind: CellKind,
    index: u16,
    spec: InstanceSpec,
    params: usize,
    sample: Option<Arc<SourceAudio>>,
    plan: SlotPlan,
    /// A fresh instance's latency, read once when it is made.
    latency: usize,
}

/// The planned slots of one strip: its track index, layout, slots and clip entries.
type PlannedStrip = (usize, u16, Vec<PlannedSlot>, Vec<(u64, Option<InstanceSpec>, Option<InstanceSpec>)>);

/// Stable code of a channel format (its place in [`ChannelFormat::ALL`]).
pub(crate) fn layout_code(f: ChannelFormat) -> u16 {
    ChannelFormat::ALL.iter().position(|x| *x == f).map_or(u16::MAX, |i| u16::try_from(i + 1).unwrap_or(u16::MAX))
}

fn sample_id(a: Option<&Arc<SourceAudio>>) -> usize {
    a.map_or(0, |a| Arc::as_ptr(a) as *const () as usize)
}

fn over(what: &'static str, need: usize, limit: usize) -> Result<(), CompileError> {
    if need > limit { Err(CompileError::ResourceLimit { what, need, limit }) } else { Ok(()) }
}

/// `a * b`, or the overflow error.
fn mul(a: usize, b: usize) -> Result<usize, CompileError> {
    a.checked_mul(b).ok_or(CompileError::ResourceLimit { what: "prepared_bytes", need: usize::MAX, limit: usize::MAX })
}

fn add(a: usize, b: usize) -> Result<usize, CompileError> {
    a.checked_add(b).ok_or(CompileError::ResourceLimit { what: "prepared_bytes", need: usize::MAX, limit: usize::MAX })
}

/// Bytes one strip's prepared storage takes: `buf`, `pre`, `outbuf` and `clipbuf` (channels ×
/// block each), `scratch` (block), two delay lines (channels × (`cap` + block); 0 when the strip
/// keeps its live lines), its insert positions (filled or not), its clip-effect entries, the held-note
/// capacity, its meter, and the strip itself.
pub(crate) fn strip_bytes(ch: usize, block: usize, cap: usize, inserts: usize, clips: usize) -> Result<usize, CompileError> {
    let f = size_of::<f32>();
    let bufs = mul(mul(mul(ch, block)?, 4)?, f)?;
    let scratch = mul(block, f)?;
    let delays = if cap == 0 { 0 } else { mul(mul(mul(ch, add(cap, block)?)?, 2)?, f)? };
    let slots = mul(inserts, size_of::<Option<PluginSlot>>())?;
    let entries = mul(clips, size_of::<(u64, ClipFx)>())?;
    let held = 128 * size_of::<u8>();
    let meter = mul(ch, f)?;
    [scratch, delays, slots, entries, held, meter, size_of::<Strip>()].into_iter().try_fold(bufs, add)
}

/// Checks every limit and the prepared bytes, with each strip's delay capacity from `cap_of`
/// (0: the strip keeps its live lines). Allocates nothing.
fn measure(s: &Session, l: &PlanLimits, block: usize, cap_of: &dyn Fn(&Track, usize) -> usize) -> Result<(), CompileError> {
    over("tracks", s.tracks.len(), l.tracks.into())?;
    over("buses", s.busses.len(), l.buses.into())?;
    let main_ch = main_channels(s);
    let mut bytes = 0usize;
    let mut cells = 0usize;
    for t in &s.tracks {
        let ch = strip_channels(t, main_ch);
        over("slots_per_strip", t.mixer.inserts.len(), l.slots_per_strip.into())?;
        let clips = if t.kind == TrackKind::Audio {
            t.clips().iter().filter(|c| c.is_audio()).filter(|c| clip_fx_modules(s, c.id.0) != (false, false)).count()
        } else {
            0
        };
        over("clip_fx_per_strip", clips, l.clip_fx_per_strip.into())?;
        let slots = add(add(t.mixer.inserts.iter().flatten().count(), usize::from(t.instrument.is_some()))?, mul(clips, 2)?)?;
        cells = add(add(cells, 1)?, slots)?;
        bytes = add(bytes, strip_bytes(ch, block, cap_of(t, ch), t.mixer.inserts.len(), clips)?)?;
        for ins in t.mixer.inserts.iter().flatten().chain(t.instrument.iter()) {
            if needs_storage(ins.params.len()) {
                bytes = add(bytes, mul(ins.params.len(), size_of::<f32>())?)?;
            }
        }
    }
    for b in &s.busses {
        bytes = add(bytes, mul(mul(b.format.channels().clamp(1, MAX_CHANNELS), block)?, size_of::<f32>())?)?;
        bytes = add(bytes, size_of::<(BusId, BusBuf)>())?;
    }
    bytes = add(bytes, mul(mul(main_ch, block)?, size_of::<f32>())?)?;
    bytes = add(bytes, mul(main_ch, size_of::<f32>())?)?;
    bytes = add(bytes, mul(cells, size_of::<CellLoc>() + 2 * size_of::<bool>())?)?;
    bytes = add(bytes, mul(s.tracks.len(), size_of::<(TrackId, u16)>() + size_of::<BusId>())?)?;
    over("cells", cells, usize::from(u16::MAX))?;
    over("prepared_bytes", bytes, l.prepared_bytes)
}

/// The strip index of each track in render order: live tracks other than auxes (processing
/// order), every aux and folder (dependency order), then the rest in track order. Also the
/// number of each of the first two groups.
fn render_rows(s: &Session) -> (Vec<usize>, usize, usize, Vec<RouteCycle>) {
    let is_aux = |t: &Track| matches!(t.kind, TrackKind::Aux | TrackKind::Folder);
    let live = |t: &Track| !(t.kind == TrackKind::Master || t.inactive || t.kind == TrackKind::Vca);
    let mut order = Vec::new();
    let loops = processing_order(s, &mut order);
    let track = |i: &usize| s.tracks.get(*i);
    let mut rows: Vec<usize> = order.iter().copied().filter(|i| track(i).is_some_and(|t| live(t) && !is_aux(t))).collect();
    let phase1 = rows.len();
    rows.extend(order.iter().copied().filter(|i| track(i).is_some_and(is_aux)));
    let aux_end = rows.len();
    rows.extend((0..s.tracks.len()).filter(|i| track(i).is_some_and(|t| !is_aux(t) && !live(t))));
    (rows, phase1, aux_end, loops)
}

impl PlanCompiler {
    /// A compiler for an engine made with [`MixEngine::realtime`]`(target)`.
    #[must_use]
    pub fn new(target: RenderTarget, limits: PlanLimits) -> Self {
        PlanCompiler {
            target: target.normalized(),
            limits,
            active: Shadow::default(),
            pending: None,
            last: PlanGeneration::EMPTY,
            slot_gen: 0,
            pool: Vec::new(),
            released: Vec::new(),
            hints: HashMap::new(),
            closing: false,
            faulted: None,
            loops: Vec::new(),
        }
    }

    /// The confirmed active generation.
    #[must_use]
    pub fn active(&self) -> PlanGeneration {
        self.active.generation
    }

    /// The generation in flight, if any.
    #[must_use]
    pub fn pending(&self) -> Option<PlanGeneration> {
        self.pending.as_ref().map(|p| p.generation)
    }

    /// True once a torn envelope (its commit unwound, see [`PlanEnvelope::torn`]) was reclaimed:
    /// the engine is faulted and must be torn down. Distinguishes that from a plan that simply
    /// came back unserviced.
    #[must_use]
    pub fn faulted(&self) -> bool {
        self.faulted.is_some()
    }

    /// Every instance of the confirmed active plan, sorted. After a torn reclaim
    /// ([`PlanCompiler::faulted`]), exactly the instances the faulted engine still owns (what
    /// [`MixEngine::into_retired`] will hand back): never one [`PlanCompiler::take_released`] reported.
    #[must_use]
    pub fn active_bindings(&self) -> Vec<SlotBinding> {
        match &self.faulted {
            Some(live) => live.clone(),
            None => self.active.bindings(),
        }
    }

    /// Every instance of the plan in flight (the active ones for a Session-only plan), sorted;
    /// `None` with no plan in flight.
    #[must_use]
    pub fn pending_bindings(&self) -> Option<Vec<SlotBinding>> {
        self.pending.as_ref().map(|p| p.shadow.as_ref().unwrap_or(&self.active).bindings())
    }

    /// Fresh instances kept from the last refused plan.
    #[must_use]
    pub fn pooled(&self) -> usize {
        self.pool.len()
    }

    /// Takes a status the engine reported: when a delay line had to clamp, every confirmed strip
    /// whose lines hold less than the delay asked for gets lines for it in the next structural
    /// plan, so one plan restores alignment for all of them (delay compensation can ask any strip
    /// for up to that much). Strips that already hold enough are left as they are.
    pub fn observe(&mut self, status: &PlanStatus) {
        let need = status.required_latency as usize;
        if status.capacity_exceeded == 0 || need == 0 {
            return;
        }
        for st in &self.active.strips {
            if st.generation != SlotGen::VACANT && st.cap < need {
                let h = self.hints.entry(st.track).or_default();
                *h = (*h).max(need);
            }
        }
    }

    /// A new instance or strip generation. Never wraps: an exhausted counter is an error.
    fn fresh_gen(&mut self) -> Result<SlotGen, CompileError> {
        self.slot_gen = self.slot_gen.checked_add(1).ok_or(CompileError::GenerationExhausted)?;
        Ok(SlotGen(self.slot_gen))
    }

    /// Delay-line capacity for a delay of `need` samples: a power of two, at least
    /// [`MIN_DELAY_CAPACITY`], at most the limit.
    fn delay_capacity(&self, need: usize) -> usize {
        capacity_for(need, self.limits.delay_samples)
    }

    /// The delay capacity of `t`'s strip in the next plan, and whether the strip keeps its live
    /// lines (it moves, and they hold at least that much). Lines never shrink under a moved
    /// strip, so a tail in them survives every plan.
    fn strip_cap(&self, t: &Track, ch: usize, need: usize, hints: bool) -> (usize, bool) {
        let hint = if hints { self.hints.get(&t.id).copied().unwrap_or(0) } else { 0 };
        let want = self.delay_capacity(need.max(hint));
        let ch16 = u16::try_from(ch).unwrap_or(u16::MAX);
        match self.active.movable_strip(t.id, ch16, layout_code(strip_format(t, ch))) {
            Some((_, old)) if old.cap >= want => (old.cap, true),
            _ => (want, false),
        }
    }

    /// The pool holds at most one plan's slots.
    fn pool_bound(&self) -> usize {
        let per = usize::from(self.limits.clip_fx_per_strip).checked_mul(2).and_then(|c| c.checked_add(usize::from(self.limits.slots_per_strip) + 1));
        per.and_then(|p| p.checked_mul(usize::from(self.limits.tracks))).unwrap_or(usize::MAX)
    }

    /// Teardown: reports and drops (on this thread) what the pool holds, and stops pooling. Every
    /// envelope reclaimed afterwards has its instances dropped and reported instead of kept, so
    /// none dies unreported with the compiler.
    pub fn clear_pool(&mut self) {
        self.closing = true;
        self.drop_pool();
    }

    /// Reports and drops (on this thread) what the pool still holds.
    fn drop_pool(&mut self) {
        let leftovers = std::mem::take(&mut self.pool);
        self.released.extend(leftovers.iter().map(|p| (p.key.0, p.key.1, p.slot.generation)));
        drop(leftovers);
    }

    /// Reports and drops every fresh instance of planned slots that will not be published.
    fn release_planned(&mut self, planned: Vec<PlannedStrip>) {
        for (_, _, slots, _) in &planned {
            for p in slots {
                if let SlotPlan::Fresh(slot) = &p.plan {
                    self.released.push((p.key.0, p.key.1, slot.generation));
                }
            }
        }
        drop(planned);
    }

    /// Compiles `session` into a plan replacing `base` (the confirmed active generation) with
    /// generation `next`. `make` creates, restores and prepares each new instance on this (the
    /// format-required) thread; it gets the [`SlotGen`] the instance will carry, so playback can
    /// bind handles to it before it is published.
    ///
    /// Every count, and the prepared bytes without delay lines, are checked before anything is
    /// allocated; the bytes with the delay lines are checked once the route graph has given the
    /// delay the active instances are known to need (its walk allocates scratch). Fresh instances can report more latency
    /// than that; if the larger delay lines then exceed the limits, the plan fails after `make`
    /// ran, and those instances are dropped here and reported by [`PlanCompiler::take_released`].
    pub fn compile(
        &mut self,
        session: Arc<Session>,
        base: PlanGeneration,
        next: PlanGeneration,
        make: &mut dyn FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance>,
    ) -> Result<PlanEnvelope, CompileError> {
        if self.faulted.is_some() {
            return Err(CompileError::Faulted);
        }
        if let Some(p) = &self.pending {
            return Err(CompileError::InFlight { pending: p.generation });
        }
        if base != self.active.generation {
            return Err(CompileError::BaseNotActive { base, active: self.active.generation });
        }
        if self.last.checked_next().is_none() {
            return Err(CompileError::GenerationExhausted);
        }
        if next <= self.last {
            return Err(CompileError::GenerationNotIncreasing { last: self.last, next });
        }
        if session.sample_rate.hz() != self.target.sample_rate {
            return Err(CompileError::TargetMismatch { compiler: self.target });
        }
        let block = self.target.block();
        // Every count, and the bytes with the smallest delay lines (a lower bound), before anything
        // is allocated: walking the route graph below allocates its scratch.
        let least = |t: &Track, ch: usize| match self.strip_cap(t, ch, 0, false) {
            (_, true) => 0,
            (cap, false) => cap,
        };
        measure(&session, &self.limits, block, &least)?;
        // Path-based (F05): the slowest path into any summing point bounds every edge's delay.
        let need = path_need(&session, &|t: &Track| self.active.track_latency(t.id));
        let sized = |me: &Self, hints: bool| {
            let cap_of = |t: &Track, ch: usize| match me.strip_cap(t, ch, need, hints) {
                (_, true) => 0,
                (cap, false) => cap,
            };
            measure(&session, &me.limits, block, &cap_of)
        };
        if let Err(e) = sized(self, true) {
            // A hinted delay that does not fit is dropped: the strip stays clamped and reported.
            if self.hints.is_empty() {
                return Err(e);
            }
            sized(self, false)?;
            log::warn!("mix: dropping delay-capacity hints that exceed the plan limits ({e:?})");
            self.hints.clear();
        }

        let id = PlanIdentity { generation: next, base, target: self.target };
        let (desc, rows, phase1, aux_end, specs, loops) = self.describe(&session, need);
        // A session loaded with a routing loop: say so once, not on every plan.
        if loops != self.loops {
            log_loops(&session, &loops);
            self.loops = loops;
        }
        if self.active.desc.as_ref() == Some(&desc) {
            let plan = PreparedPlan {
                id,
                session,
                tables: None,
                moves: Box::new([]),
                attach: Box::new([]),
                src_claimed: Box::new([]),
                dst_claimed: Box::new([]),
                tracks: self.active.tracks.clone(),
            };
            self.drop_pool();
            self.last = next;
            self.pending = Some(Pending { generation: next, shadow: None });
            return Ok(PlanEnvelope::new(plan));
        }
        let built = self.build(&session, desc, &rows, phase1, aux_end, specs, make, need);
        let (tables, moves, attach, shadow) = match built {
            Ok(b) => b,
            Err(e) => {
                self.drop_pool();
                return Err(e);
            }
        };
        let fresh = tables.bindings();
        let tracks = tables.tracks.clone();
        let plan = PreparedPlan {
            id,
            session,
            src_claimed: vec![false; self.active.cells].into_boxed_slice(),
            dst_claimed: vec![false; tables.cells.len()].into_boxed_slice(),
            tables: Some(tables),
            moves: moves.into_boxed_slice(),
            attach: attach.into_boxed_slice(),
            tracks,
        };
        // The pool goes before the plan is committed: a plug-in destructor that unwinds here
        // leaves no plan in flight (and the plan's own instances reported), not a wedge.
        let leftovers = std::mem::take(&mut self.pool);
        self.released.extend(leftovers.iter().map(|p| (p.key.0, p.key.1, p.slot.generation)));
        {
            let _guard = ReleaseOnUnwind { released: &mut self.released, fresh };
            drop(leftovers);
        }
        self.last = next;
        self.pending = Some(Pending { generation: next, shadow: Some(shadow) });
        Ok(PlanEnvelope::new(plan))
    }

    /// The structure `s` needs: its description, the strips' tracks in render order (with the
    /// sizes of the first two groups), and the instance specs by slot.
    #[allow(clippy::type_complexity)]
    fn describe(&self, s: &Session, need: usize) -> (Desc, Vec<usize>, usize, usize, HashMap<(TrackId, usize), InstanceSpec>, Vec<RouteCycle>) {
        let main_ch = main_channels(s);
        let specs: HashMap<(TrackId, usize), InstanceSpec> = realtime_instance_specs(s).into_iter().map(|x| ((x.track, x.slot), x)).collect();
        let (rows, phase1, aux_end, loops) = render_rows(s);
        let mut desc = Desc { main: Some(s.main_format()), ..Desc::default() };
        for (r, &ti) in rows.iter().enumerate() {
            let Some(t) = s.tracks.get(ti) else { continue };
            let ch = strip_channels(t, main_ch);
            let layout = layout_code(strip_format(t, ch));
            let region = if r < phase1 {
                0
            } else if r < aux_end {
                1
            } else {
                2
            };
            let clips = clip_entries(t.id, &specs);
            let index = u16::try_from(ti).unwrap_or(u16::MAX);
            let (cap, _) = self.strip_cap(t, ch, need, true);
            desc.strips.push((
                t.id,
                index,
                region,
                ch as u16,
                layout,
                t.mixer.inserts.len(),
                cap,
                clips.iter().map(|(c, e, k)| (*c, e.is_some(), k.is_some())).collect(),
            ));
            for (key, kind, ins) in slot_keys(t, &clips) {
                let Some(spec) = specs.get(&key) else { continue };
                let params = ins.map_or(0, |i| i.params.len());
                let sample = if kind == CellKind::Instrument { sample_id(instrument_sample(s, t).as_ref()) } else { 0 };
                desc.slots.push((key, kind, spec.id.clone(), spec.ch, layout, if needs_storage(params) { params } else { 0 }, sample));
            }
        }
        for b in &s.busses {
            desc.busses.push((b.id, b.format));
        }
        (desc, rows, phase1, aux_end, specs, loops)
    }

    /// Decides, for one slot, between moving the active one, leaving it absent, and a fresh one.
    fn plan_slot(
        &mut self,
        p: &mut PlannedSlot,
        layout: u16,
        make: &mut dyn FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance>,
    ) -> Result<(), CompileError> {
        let sample = sample_id(p.sample.as_ref());
        let long = if needs_storage(p.params) { p.params } else { 0 };
        if let Some(old) = self.active.slots.get(&p.key)
            && old.kind == p.kind
            && old.spec.id == p.spec.id
            && old.spec.ch == p.spec.ch
            && old.layout == layout
        {
            if old.generation == SlotGen::VACANT {
                p.plan = SlotPlan::Absent;
                return Ok(());
            }
            let sampler = old.spec.id == SAMPLER;
            if old.sample == sample || sampler {
                let applied = (long > 0 && long != old.heap).then_some(long);
                let sample = (old.sample != sample).then(|| p.sample.clone());
                p.plan = SlotPlan::Move { from: old.cell, generation: old.generation, applied, sample };
                return Ok(());
            }
        }
        let slot = self.pool.iter().position(|x| x.key == p.key && x.spec == p.spec).map(|i| self.pool.swap_remove(i).slot);
        let slot = match slot {
            Some(s) => Some(s),
            None => {
                let generation = self.fresh_gen()?;
                make(&p.spec, generation).map(|n| {
                    let mut s = PluginSlot::new(n.plugin, p.spec.id.clone(), p.spec.ch, p.key.1);
                    s.generation = generation;
                    s
                })
            }
        };
        let Some(slot) = slot else {
            p.plan = SlotPlan::Absent;
            return Ok(());
        };
        // The instance is in the planned list before any of its own code runs: an unwind from here
        // on finds it there and reports it.
        p.plan = SlotPlan::Fresh(Box::new(slot));
        let SlotPlan::Fresh(slot) = &mut p.plan else { return Ok(()) };
        slot.layout = layout;
        if slot.applied.heap.len() != long || !slot.applied.fixed {
            slot.applied = Applied::prepared(if long > 0 { vec![f32::NAN; long] } else { Vec::new() });
        }
        if p.kind == CellKind::Instrument && sample_id(slot.sample_audio.as_ref()) != sample {
            let wrapper = p.sample.as_ref().map(|a| Arc::new(PoolSample(Arc::clone(a))) as Arc<dyn SampleSource>);
            slot.plugin.set_sample(wrapper.clone());
            slot.sample_audio = p.sample.clone();
            slot.sample_wrapper = wrapper;
        }
        p.latency = slot.plugin.latency();
        Ok(())
    }

    /// Plans every slot of every strip into `out` (fresh instances made here).
    fn plan_slots(
        &mut self,
        s: &Session,
        rows: &[usize],
        specs: &HashMap<(TrackId, usize), InstanceSpec>,
        make: &mut dyn FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance>,
        out: &mut Vec<PlannedStrip>,
    ) -> Result<(), CompileError> {
        let main_ch = main_channels(s);
        for &ti in rows {
            let Some(t) = s.tracks.get(ti) else { continue };
            let ch = strip_channels(t, main_ch);
            let layout = layout_code(strip_format(t, ch));
            let clips = clip_entries(t.id, specs);
            out.push((ti, layout, Vec::new(), Vec::new()));
            for (key, kind, ins) in slot_keys(t, &clips) {
                let Some(spec) = specs.get(&key) else { continue };
                let index = match kind {
                    CellKind::Insert => key.1,
                    CellKind::ClipEq | CellKind::ClipComp => {
                        clip_fx_of_slot(key.1).and_then(|(c, _)| clips.iter().position(|(x, _, _)| *x == c)).unwrap_or(0)
                    }
                    _ => 0,
                };
                let sample = if kind == CellKind::Instrument { instrument_sample(s, t) } else { None };
                let p = PlannedSlot {
                    key,
                    kind,
                    index: u16::try_from(index).unwrap_or(u16::MAX),
                    spec: spec.clone(),
                    params: ins.map_or(0, |i| i.params.len()),
                    sample,
                    plan: SlotPlan::Absent,
                    latency: 0,
                };
                // Planned in place, so whatever it makes is in `out` the moment it exists.
                let Some((_, _, planned, _)) = out.last_mut() else { continue };
                planned.push(p);
                if let Some(p) = planned.last_mut() {
                    self.plan_slot(p, layout, make)?;
                }
            }
            if let Some((_, _, _, c)) = out.last_mut() {
                *c = clips;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn build(
        &mut self,
        s: &Session,
        mut desc: Desc,
        rows: &[usize],
        phase1: usize,
        aux_end: usize,
        specs: HashMap<(TrackId, usize), InstanceSpec>,
        make: &mut dyn FnMut(&InstanceSpec, SlotGen) -> Option<NewInstance>,
        need0: usize,
    ) -> Result<(PlanTables, Vec<CellMove>, Vec<Attach>, Shadow), CompileError> {
        let block = self.target.block();
        let main_ch = main_channels(s);
        let sr = self.target.sample_rate as f32;
        // Slots first: their instances tell the delay the strips need.
        let mut per_strip: Vec<PlannedStrip> = Vec::new();
        // The factory and the instances run plug-in code that may unwind: report what was made (or
        // taken from the pool) before the unwind drops it, then let it continue.
        let planned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.plan_slots(s, rows, &specs, make, &mut per_strip)));
        match planned {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.release_planned(per_strip);
                return Err(e);
            }
            Err(panic) => {
                self.release_planned(per_strip);
                std::panic::resume_unwind(panic);
            }
        }
        let latency_of = |p: &PlannedSlot, me: &Self| match &p.plan {
            SlotPlan::Move { .. } => me.active.slots.get(&p.key).map_or(0, |o| o.latency),
            SlotPlan::Fresh(_) => p.latency,
            SlotPlan::Absent => 0,
        };
        // Each track's planned instrument and insert latency, then the slowest path (F05).
        let planned: HashMap<TrackId, usize> = per_strip
            .iter()
            .filter_map(|(ti, _, ps, _)| {
                let t = s.tracks.get(*ti)?;
                let l = ps
                    .iter()
                    .filter(|p| matches!(p.kind, CellKind::Insert | CellKind::Instrument))
                    .fold(0usize, |a, p| a.saturating_add(latency_of(p, self)));
                Some((t.id, l))
            })
            .collect();
        let need1 = path_need(s, &|t: &Track| planned.get(&t.id).copied().unwrap_or(0));
        let need = need0.max(need1);
        if need > need0 {
            // Fresh instances need longer lines than measured before they existed: check again.
            let cap_of = |t: &Track, ch: usize| match self.strip_cap(t, ch, need, true) {
                (_, true) => 0,
                (cap, false) => cap,
            };
            if let Err(e) = measure(s, &self.limits, block, &cap_of) {
                self.release_planned(per_strip);
                return Err(e);
            }
        }

        // Strips and cells.
        let mut tables = PlanTables { phase1, aux_end, main_fmt: s.main_format(), ..PlanTables::default() };
        let mut cells: Vec<CellLoc> = Vec::new();
        let mut moves = Vec::new();
        let mut attach = Vec::new();
        let mut shadow = Shadow { desc: None, ..Shadow::default() };
        let mut strip_moves = Vec::new();
        desc.strips.clear();
        for (si, (ti, layout, _, clips)) in per_strip.iter().enumerate() {
            let Some(t) = s.tracks.get(*ti) else { continue };
            let ch = strip_channels(t, main_ch);
            let ch16 = u16::try_from(ch).unwrap_or(u16::MAX);
            let (cap, keep) = self.strip_cap(t, ch, need, true);
            let mut st = Strip::new();
            st.buf = vec![vec![0.0; block]; ch];
            st.pre = (0..ch).map(|_| Vec::with_capacity(block)).collect();
            st.scratch = vec![0.0; block];
            st.plugins = (0..t.mixer.inserts.len()).map(|_| None).collect();
            st.held = Vec::with_capacity(128);
            st.clipbuf = vec![vec![0.0; block]; ch];
            st.external = true;
            // A strip that keeps its live lines gets placeholders: the move brings the lines.
            let line = || if keep { DelayLine::placeholder() } else { DelayLine::with_capacity(ch, cap, block) };
            st.tap = line();
            st.tap_pre = line();
            st.outbuf = (0..ch).map(|_| Vec::with_capacity(block)).collect();
            st.track = u16::try_from(*ti).unwrap_or(u16::MAX);
            st.id = t.id;
            st.layout = *layout;
            st.meter = StripMeter { peaks: vec![0.0; ch], ..StripMeter::default() };
            st.clip_fx = ClipFxTable(clips.iter().map(|(c, _, _)| (*c, ClipFx::new(sr, ch, false))).collect());
            let generation = match self.active.movable_strip(t.id, ch16, *layout) {
                Some((from, old)) => {
                    strip_moves.push(CellMove { kind: CellKind::Strip, from: from as u16, to: si as u16, expect: old.generation });
                    old.generation
                }
                None => match self.fresh_gen() {
                    Ok(g) => {
                        st.generation = g;
                        g
                    }
                    Err(e) => {
                        self.release_planned(per_strip);
                        return Err(e);
                    }
                },
            };
            let region = if si < phase1 {
                0
            } else if si < aux_end {
                1
            } else {
                2
            };
            desc.strips.push((
                t.id,
                st.track,
                region,
                ch16,
                *layout,
                t.mixer.inserts.len(),
                cap,
                clips.iter().map(|(c, e, k)| (*c, e.is_some(), k.is_some())).collect(),
            ));
            cells.push(CellLoc { strip: si as u16, index: 0, shape: Shape { kind: CellKind::Strip, ch: ch16, layout: *layout } });
            shadow.strips.push(ShadowStrip { track: t.id, generation, ch: ch16, layout: *layout, cap });
            tables.strips.push(st);
        }
        moves.extend(strip_moves);
        for (si, (_, layout, planned, _)) in per_strip.into_iter().enumerate() {
            for p in planned {
                let cell = u16::try_from(cells.len()).unwrap_or(u16::MAX);
                let loc = CellLoc {
                    strip: si as u16,
                    index: p.index,
                    shape: Shape { kind: p.kind, ch: u16::try_from(p.spec.ch).unwrap_or(u16::MAX), layout },
                };
                cells.push(loc);
                let long = if needs_storage(p.params) { p.params } else { 0 };
                let mut sh = ShadowSlot {
                    cell,
                    generation: SlotGen::VACANT,
                    spec: p.spec,
                    kind: p.kind,
                    layout,
                    heap: long,
                    sample: sample_id(p.sample.as_ref()),
                    latency: 0,
                };
                match p.plan {
                    SlotPlan::Move { from, generation, applied, sample } => {
                        let old = self.active.slots.get(&p.key);
                        sh.generation = generation;
                        sh.latency = old.map_or(0, |o| o.latency);
                        sh.heap = old.map_or(0, |o| o.heap);
                        let from_move = u16::try_from(moves.len()).ok();
                        moves.push(CellMove { kind: p.kind, from, to: cell, expect: generation });
                        if let Some(n) = applied {
                            attach.push(Attach::Applied { cell, from_move, storage: vec![f32::NAN; n] });
                            sh.heap = n;
                        }
                        if let Some(audio) = sample {
                            let wrapper = audio.as_ref().map(|a| Arc::new(PoolSample(Arc::clone(a))) as Arc<dyn SampleSource>);
                            attach.push(Attach::Sample { cell, from_move, audio, wrapper });
                        }
                    }
                    SlotPlan::Fresh(slot) => {
                        sh.generation = slot.generation;
                        sh.latency = p.latency;
                        sh.heap = slot.applied.heap.len();
                        if let Some(c) = tables.slot_mut(&loc) {
                            *c = Some(*slot);
                        }
                    }
                    SlotPlan::Absent => {}
                }
                shadow.slots.insert(p.key, sh);
            }
        }
        let mut tracks: Vec<(TrackId, u16)> = tables.strips.iter().enumerate().map(|(i, st)| (st.id, i as u16)).collect();
        tracks.sort_unstable();
        tables.tracks = tracks.into_boxed_slice();
        let mut busses: Vec<(BusId, BusBuf)> =
            s.busses.iter().map(|b| (b.id, BusBuf::new(b.format, vec![vec![0.0; block]; b.format.channels().clamp(1, MAX_CHANNELS)]))).collect();
        busses.sort_by_key(|(id, _)| *id);
        busses.dedup_by_key(|(id, _)| *id);
        tables.busses = BusTable(busses);
        tables.main = vec![vec![0.0; block]; main_ch];
        tables.main_meter = StripMeter { peaks: vec![0.0; main_ch], ..StripMeter::default() };
        shadow.cells = cells.len();
        tables.cells = cells.into_boxed_slice();
        shadow.tracks = tables.tracks.clone();
        shadow.desc = Some(desc);
        Ok((tables, moves, attach, shadow))
    }

    /// Takes back a returned, refused or never-serviced envelope (control thread) and drops what
    /// it holds here. A returned plan becomes the confirmed active one; after a refusal the
    /// active one stays, and the refused plan's fresh instances wait in the pool for the next
    /// compile (anything that compile does not reuse is dropped at its end). A torn envelope (its
    /// commit unwound) is dropped whole. Every instance dropped here is reported by
    /// [`PlanCompiler::take_released`].
    pub fn reclaim(&mut self, mut env: PlanEnvelope) {
        let generation = env.plan.id.generation;
        let torn = env.torn();
        match self.pending.take() {
            Some(p) if p.generation == generation && !torn => match env.state {
                EnvelopeState::Returned { .. } => {
                    match p.shadow {
                        Some(sh) => self.active = Shadow { generation, ..sh },
                        None => self.active.generation = generation,
                    }
                    // A hint is done once a confirmed strip holds it (lines never shrink under it).
                    let (active, limit) = (&self.active, self.limits.delay_samples);
                    self.hints.retain(|t, h| active.strips.iter().any(|s| s.track == *t && s.cap < capacity_for(*h, limit)));
                }
                EnvelopeState::Rejected { .. } | EnvelopeState::Pending => {
                    if let (false, Some(sh), Some(tables)) = (self.closing, p.shadow, env.plan.tables.as_mut()) {
                        self.harvest(tables, &sh);
                    }
                }
            },
            Some(p) if p.generation == generation => {
                // Torn: the commit unwound part way. The faulted engine owns what the old and the
                // new plan held, less what came back here (all of it is destroyed below).
                let back = env.plan.tables.as_ref().map(PlanTables::bindings).unwrap_or_default();
                let mut live = self.active.bindings();
                live.extend(p.shadow.as_ref().map(Shadow::bindings).unwrap_or_default());
                live.retain(|b| !back.contains(b));
                live.sort_unstable();
                live.dedup();
                self.faulted = Some(live);
            }
            other => self.pending = other,
        }
        // What is still in the envelope dies here: displaced, refused beyond the pool, or torn.
        if let Some(t) = &env.plan.tables {
            self.released.extend(t.bindings());
        }
        drop(env);
    }

    /// Moves the fresh instances of a refused plan's tables into the pool, up to its bound;
    /// the rest stay in the tables (dropped and reported with them).
    fn harvest(&mut self, tables: &mut PlanTables, sh: &Shadow) {
        let bound = self.pool_bound();
        for st in &mut tables.strips {
            let id = st.id;
            let mut cells: Vec<(usize, &mut Option<PluginSlot>)> = st.plugins.iter_mut().enumerate().collect();
            cells.push((INSTRUMENT_SLOT, &mut st.instrument));
            for (clip, fx) in &mut st.clip_fx.0 {
                if let Some(k) = clip_fx_slot(*clip, false) {
                    cells.push((k, &mut fx.eq));
                }
                if let Some(k) = clip_fx_slot(*clip, true) {
                    cells.push((k, &mut fx.comp));
                }
            }
            for (k, cell) in cells {
                let key = (id, k);
                if self.pool.len() >= bound {
                    return;
                }
                let Some(shs) = sh.slots.get(&key) else { continue };
                if let Some(slot) = cell.take() {
                    self.pool.push(Pooled { key, spec: shs.spec.clone(), slot });
                }
            }
        }
    }

    /// Test hook: where the instance and strip generation counter stands.
    #[cfg(test)]
    pub(crate) fn set_slot_generation_for_tests(&mut self, n: u64) {
        self.slot_gen = n;
    }

    /// Every instance this compiler destroyed since the last call (dropped on this thread): its
    /// track, slot and generation. A slot can show up here while another instance of a newer
    /// generation lives there; playback releases the handles of exactly these generations.
    pub fn take_released(&mut self) -> Vec<SlotBinding> {
        std::mem::take(&mut self.released)
    }

    /// Each track of the plan in `env` and its strip index (sorted by track): playback sizes its
    /// meters from it.
    #[must_use]
    pub fn track_ids<'a>(&self, env: &'a PlanEnvelope) -> &'a [(TrackId, u16)] {
        &env.plan.tracks
    }
}

/// Delay-line capacity for a delay of `need` samples: a power of two, at least
/// [`MIN_DELAY_CAPACITY`], at most `limit`.
fn capacity_for(need: usize, limit: u32) -> usize {
    let pow = need.checked_next_power_of_two().unwrap_or(usize::MAX);
    pow.max(MIN_DELAY_CAPACITY).min(limit as usize)
}

/// Plug-in id of the native Sampler: the only plug-in whose sample changes in adoption.
pub(crate) const SAMPLER: &str = "sampler";

/// A track's clips with effects, sorted: (clip, EQ spec, compressor spec).
fn clip_entries(t: TrackId, specs: &HashMap<(TrackId, usize), InstanceSpec>) -> Vec<(u64, Option<InstanceSpec>, Option<InstanceSpec>)> {
    let mut m: BTreeMap<u64, (Option<InstanceSpec>, Option<InstanceSpec>)> = BTreeMap::new();
    for ((tr, slot), spec) in specs {
        if *tr != t {
            continue;
        }
        if let Some((clip, comp)) = clip_fx_of_slot(*slot) {
            let e = m.entry(clip).or_default();
            if comp {
                e.1 = Some(spec.clone());
            } else {
                e.0 = Some(spec.clone());
            }
        }
    }
    m.into_iter().map(|(c, (e, k))| (c, e, k)).collect()
}

/// A strip's slot keys in cell order: inserts, the instrument, then each clip's EQ and compressor.
fn slot_keys<'a>(
    t: &'a Track,
    clips: &[(u64, Option<InstanceSpec>, Option<InstanceSpec>)],
) -> Vec<((TrackId, usize), CellKind, Option<&'a soundcraft_model::Insert>)> {
    let mut out = Vec::new();
    for (i, ins) in t.mixer.inserts.iter().enumerate() {
        if let Some(ins) = ins {
            out.push(((t.id, i), CellKind::Insert, Some(ins)));
        }
    }
    if let Some(ins) = &t.instrument {
        out.push(((t.id, INSTRUMENT_SLOT), CellKind::Instrument, Some(ins)));
    }
    for (clip, eq, comp) in clips {
        if eq.is_some()
            && let Some(k) = clip_fx_slot(*clip, false)
        {
            out.push(((t.id, k), CellKind::ClipEq, None));
        }
        if comp.is_some()
            && let Some(k) = clip_fx_slot(*clip, true)
        {
            out.push(((t.id, k), CellKind::ClipComp, None));
        }
    }
    out
}

/// The pool audio a track's instrument plays.
fn instrument_sample(s: &Session, t: &Track) -> Option<Arc<SourceAudio>> {
    t.instrument.as_ref().and_then(|i| i.sample).and_then(|id| s.pool.get(id)).cloned()
}
