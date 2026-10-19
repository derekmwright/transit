//! A realtime engine's side of the plan contract: [`MixEngine::adopt_plan`] (validate, then swap)
//! and [`MixEngine::render_active`] (render the active plan's tables). Neither allocates, frees,
//! locks, makes a system call, waits or calls a plug-in lifecycle function (the native Sampler's
//! `set_sample` aside, which only stops voices and stores the shared sample).

use super::compile::SAMPLER;
use super::plan::{ActivePlan, Attach, CellKind, CellLoc, PlanTables, Shape};
use super::*;

/// The Session of an engine without a plan.
static NO_SESSION: std::sync::LazyLock<Arc<Session>> = std::sync::LazyLock::new(|| Arc::new(Session::default()));

/// The identity of an engine without a plan.
const NO_PLAN: PlanIdentity = PlanIdentity {
    generation: PlanGeneration::EMPTY,
    base: PlanGeneration::EMPTY,
    target: RenderTarget { device: DeviceGeneration(0), sample_rate: 0, max_block: 0 },
};

#[cfg(test)]
std::thread_local! {
    /// Test hook: the commit step at which an adoption unwinds (0: never).
    pub(crate) static FAULT_AT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Test hook: unwind at commit step `step` when a test asked for it.
#[cfg(test)]
#[allow(clippy::panic)]
fn fault_point(step: usize) {
    if FAULT_AT.get() == step {
        panic!("injected fault at commit step {step}");
    }
}

#[cfg(not(test))]
fn fault_point(_step: usize) {}

/// The shape of a live strip's state, `None` when it holds none.
fn strip_shape(st: &Strip) -> Option<Shape> {
    (st.generation != SlotGen::VACANT).then(|| Shape {
        kind: CellKind::Strip,
        ch: u16::try_from(st.buf.len()).unwrap_or(u16::MAX),
        layout: st.layout,
    })
}

fn slot_shape(kind: CellKind, s: &PluginSlot) -> Shape {
    Shape { kind, ch: u16::try_from(s.ch).unwrap_or(u16::MAX), layout: s.layout }
}

/// Moves a strip's running state into a new strip shell: held notes, note chasing, mute, gain
/// reduction, latency, failures, meter, and delay lines of the same capacity. Only swaps and
/// copies; what the shell had goes back with the old strip.
fn move_strip(old: &mut Strip, new: &mut Strip) {
    std::mem::swap(&mut old.held, &mut new.held);
    new.chase = old.chase;
    new.muted = old.muted;
    new.gr = old.gr;
    new.latency = old.latency;
    new.failures = old.failures;
    for (a, b) in [(&mut old.align, &mut new.align), (&mut old.align_pre, &mut new.align_pre), (&mut old.out_extra, &mut new.out_extra)] {
        if a.cap == b.cap && a.bufs.len() == b.bufs.len() {
            std::mem::swap(a, b);
        }
    }
    if old.meter.peaks.len() == new.meter.peaks.len() {
        std::mem::swap(&mut old.meter, &mut new.meter);
    }
    new.generation = old.generation;
    old.generation = SlotGen::VACANT;
}

/// Validates `plan` against the active plan without changing either (beyond the plan's claim
/// scratch).
fn validate(active: &ActivePlan, plan: &mut PreparedPlan) -> Result<(), RejectReason> {
    if plan.id.base != active.id.generation || plan.id.generation <= active.id.generation {
        return Err(RejectReason::BaseMismatch);
    }
    if plan.id.target != active.id.target {
        return Err(RejectReason::TargetMismatch);
    }
    let old = &active.tables;
    let Some(new) = plan.tables.as_ref() else {
        if !plan.moves.is_empty() {
            return Err(RejectReason::MoveOutOfRange);
        }
        if !plan.attach.is_empty() {
            return Err(RejectReason::AttachInvalid);
        }
        return Ok(());
    };
    if plan.src_claimed.len() != old.cells.len() || plan.dst_claimed.len() != new.cells.len() {
        return Err(RejectReason::MoveOutOfRange);
    }
    for mv in &plan.moves {
        let (from, to) = (usize::from(mv.from), usize::from(mv.to));
        let (Some(src), Some(dst)) = (old.cells.get(from), new.cells.get(to)) else { return Err(RejectReason::MoveOutOfRange) };
        let (Some(sc), Some(dc)) = (plan.src_claimed.get(from).copied(), plan.dst_claimed.get(to).copied()) else {
            return Err(RejectReason::MoveOutOfRange);
        };
        if sc {
            return Err(RejectReason::DuplicateSource);
        }
        if dc {
            return Err(RejectReason::DuplicateDestination);
        }
        if let (Some(s), Some(d)) = (plan.src_claimed.get_mut(from), plan.dst_claimed.get_mut(to)) {
            *s = true;
            *d = true;
        }
        if src.shape.kind != mv.kind || dst.shape.kind != mv.kind {
            return Err(RejectReason::ShapeMismatch);
        }
        let (live, vacant) = if mv.kind == CellKind::Strip {
            let s = old.strips.get(usize::from(src.strip)).filter(|_| usize::from(src.strip) == from);
            let d = new.strips.get(usize::from(dst.strip)).filter(|_| usize::from(dst.strip) == to);
            let (Some(s), Some(d)) = (s, d) else { return Err(RejectReason::MoveOutOfRange) };
            let shell = Shape { kind: CellKind::Strip, ch: u16::try_from(d.buf.len()).unwrap_or(u16::MAX), layout: d.layout };
            if shell != dst.shape {
                return Err(RejectReason::ShapeMismatch);
            }
            (strip_shape(s).map(|sh| (sh, s.generation)), d.generation == SlotGen::VACANT)
        } else {
            let (Some(s), Some(d)) = (old.slot(src), new.slot(dst)) else { return Err(RejectReason::MoveOutOfRange) };
            (s.as_ref().map(|s| (slot_shape(mv.kind, s), s.generation)), d.is_none())
        };
        let Some((shape, generation)) = live else { return Err(RejectReason::SourceNotLive) };
        if generation != mv.expect {
            return Err(RejectReason::SlotGenMismatch);
        }
        if !vacant {
            return Err(RejectReason::DestinationNotVacant);
        }
        if shape != dst.shape {
            return Err(RejectReason::ShapeMismatch);
        }
    }
    for a in &plan.attach {
        let (cell, sample) = match a {
            Attach::Applied { cell, storage } => {
                if !plan::needs_storage(storage.len()) {
                    return Err(RejectReason::AttachInvalid);
                }
                (*cell, false)
            }
            Attach::Sample { cell, .. } => (*cell, true),
        };
        let Some(loc) = new.cells.get(usize::from(cell)) else { return Err(RejectReason::AttachInvalid) };
        let kind_ok = if sample { loc.shape.kind == CellKind::Instrument } else { matches!(loc.shape.kind, CellKind::Insert | CellKind::Instrument) };
        if !kind_ok {
            return Err(RejectReason::AttachInvalid);
        }
        // The slot that will be live there after the moves.
        let moved = plan.dst_claimed.get(usize::from(cell)).copied().unwrap_or(false);
        let slot = if moved {
            plan.moves
                .iter()
                .find(|m| m.to == cell)
                .and_then(|m| old.cells.get(usize::from(m.from)))
                .and_then(|l| old.slot(l))
                .and_then(Option::as_ref)
        } else {
            new.slot(loc).and_then(Option::as_ref)
        };
        let Some(slot) = slot else { return Err(RejectReason::AttachInvalid) };
        if slot_shape(loc.shape.kind, slot) != loc.shape || (sample && slot.id != SAMPLER) {
            return Err(RejectReason::AttachInvalid);
        }
    }
    Ok(())
}

/// Swaps the cell `src` of `old` with the cell `dst` of `new`.
fn swap_cells(kind: CellKind, old: &mut PlanTables, src: &CellLoc, new: &mut PlanTables, dst: &CellLoc) {
    if kind == CellKind::Strip {
        if let (Some(a), Some(b)) = (old.strips.get_mut(usize::from(src.strip)), new.strips.get_mut(usize::from(dst.strip))) {
            move_strip(a, b);
        }
    } else if let (Some(a), Some(b)) = (old.slot_mut(src), new.slot_mut(dst)) {
        std::mem::swap(a, b);
    }
}

impl MixEngine {
    /// A realtime engine for `target` (control thread). It renders plans adopted with
    /// [`MixEngine::adopt_plan`] through [`MixEngine::render_active`]; its generation starts at
    /// [`PlanGeneration::EMPTY`] with nothing to render.
    #[must_use]
    pub fn realtime(target: RenderTarget) -> Self {
        let target = target.normalized();
        let mut eng = MixEngine::new(target.sample_rate as f32, target.block());
        eng.set_external_instances(true);
        eng.plan = Some(Box::new(ActivePlan {
            id: PlanIdentity { generation: PlanGeneration::EMPTY, base: PlanGeneration::EMPTY, target },
            session: Arc::new(Session::default()),
            tables: PlanTables::default(),
            status: PlanStatus::default(),
            faulted: false,
        }));
        eng
    }

    /// Audio thread: adopts the plan in `env` in place, at most once per envelope. It validates
    /// first and changes nothing when it refuses (the envelope keeps its entire input); then it
    /// only swaps values between the engine and the envelope, which afterwards holds everything
    /// the plan displaced. No allocation, free, lock, system call or lifecycle call; the work is
    /// bounded by the cells of the old and the new plan.
    pub fn adopt_plan(&mut self, env: &mut PlanEnvelope) -> Adoption {
        if env.state != EnvelopeState::Pending {
            return Adoption::AlreadyServiced;
        }
        let Some(active) = self.plan.as_deref_mut() else {
            env.state = EnvelopeState::Rejected { reason: RejectReason::TargetMismatch, active: PlanGeneration::EMPTY };
            return Adoption::Rejected(RejectReason::TargetMismatch);
        };
        let reject = |env: &mut PlanEnvelope, reason, active: &ActivePlan| {
            env.state = EnvelopeState::Rejected { reason, active: active.id.generation };
            Adoption::Rejected(reason)
        };
        if active.faulted {
            return reject(env, RejectReason::Faulted, active);
        }
        if let Err(reason) = validate(active, &mut env.plan) {
            return reject(env, reason, active);
        }
        // Commit. Every step is a complete swap; the plan never leaves its envelope, so an unwind
        // at any point leaves each owner either here or there. `faulted` stays set if one does.
        active.faulted = true;
        let replaced = active.id.generation;
        let plan = &mut env.plan;
        if let Some(new) = plan.tables.as_mut() {
            for mv in plan.moves.iter() {
                let (Some(src), Some(dst)) = (active.tables.cells.get(usize::from(mv.from)).copied(), new.cells.get(usize::from(mv.to)).copied())
                else {
                    continue;
                };
                swap_cells(mv.kind, &mut active.tables, &src, new, &dst);
                fault_point(1);
            }
            for a in plan.attach.iter_mut() {
                let cell = match a {
                    Attach::Applied { cell, .. } | Attach::Sample { cell, .. } => *cell,
                };
                let Some(loc) = new.cells.get(usize::from(cell)).copied() else { continue };
                let Some(Some(slot)) = new.slot_mut(&loc) else { continue };
                match a {
                    Attach::Applied { storage, .. } => {
                        std::mem::swap(&mut slot.applied.heap, storage);
                        slot.applied.fixed = true;
                        // The next block treats the map as new: every value is pushed again.
                        slot.applied.len = usize::MAX;
                    }
                    Attach::Sample { audio, wrapper, .. } => {
                        std::mem::swap(&mut slot.sample_audio, audio);
                        std::mem::swap(&mut slot.sample_wrapper, wrapper);
                    }
                }
                fault_point(2);
            }
            fault_point(3);
            std::mem::swap(&mut active.tables, new);
        }
        std::mem::swap(&mut active.session, &mut plan.session);
        fault_point(4);
        // Last: the native Sampler takes its new sample. The old (audio, wrapper) pair is already
        // in the envelope, so the wrapper the Sampler drops here is never the last owner.
        for a in plan.attach.iter() {
            let Attach::Sample { cell, .. } = a else { continue };
            let Some(loc) = active.tables.cells.get(usize::from(*cell)).copied() else { continue };
            if let Some(Some(slot)) = active.tables.slot_mut(&loc)
                && slot.id == SAMPLER
            {
                slot.plugin.set_sample(slot.sample_wrapper.clone());
            }
        }
        active.id = plan.id;
        active.status = PlanStatus::default();
        active.faulted = false;
        env.state = EnvelopeState::Returned { replaced };
        Adoption::Adopted
    }

    /// The active plan's identity ([`PlanGeneration::EMPTY`] before the first adoption, and for
    /// an engine not made with [`MixEngine::realtime`]).
    #[must_use]
    pub fn active(&self) -> &PlanIdentity {
        self.plan.as_deref().map_or(&NO_PLAN, |p| &p.id)
    }

    /// The active plan's Session.
    #[must_use]
    pub fn session(&self) -> &Arc<Session> {
        self.plan.as_deref().map_or(&NO_SESSION, |p| &p.session)
    }

    /// Each strip's meter in the active plan, by track.
    pub fn meters(&self) -> impl Iterator<Item = (TrackId, &StripMeter)> {
        self.plan.as_deref().into_iter().flat_map(|p| p.tables.strips.iter().map(|st| (st.id, &st.meter)))
    }

    /// The main mix's meter in the active plan (a legacy engine's is [`MixEngine::main_meter`]).
    #[must_use]
    pub fn active_main_meter(&self) -> &StripMeter {
        self.plan.as_deref().map_or(&self.main_meter, |p| &p.tables.main_meter)
    }

    /// What the audio thread noticed since the last adoption, in fixed storage.
    #[must_use]
    pub fn status(&self) -> PlanStatus {
        let Some(p) = self.plan.as_deref() else { return PlanStatus::default() };
        let mismatch = p.tables.plugins().fold(0u32, |n, s| n.saturating_add(s.applied.mismatch));
        PlanStatus { applied_mismatch: mismatch, ..p.status }
    }

    /// True after an adoption unwound: the engine may be half-adopted, renders silence and
    /// adopts nothing. Tear it down with [`MixEngine::into_retired`].
    #[must_use]
    pub fn faulted(&self) -> bool {
        self.plan.as_deref().is_some_and(|p| p.faulted)
    }

    /// Control thread, after the audio side is quiescent (also after a fault): everything the
    /// engine owns, to drop there.
    pub fn into_retired(mut self) -> Retired {
        let legacy = self.drain_plugins();
        match self.plan.take() {
            Some(p) => {
                let p = *p;
                Retired { session: Some(p.session), tables: Some(p.tables), _attach: Box::new([]), legacy }
            }
            None => Retired { session: None, tables: None, _attach: Box::new([]), legacy },
        }
    }

    /// Audio thread: renders `frames` (≤ max block) samples of the active plan starting at `pos`
    /// into `out`, as [`MixEngine::render`] renders a Session. Silence before the first adoption
    /// and after a fault.
    pub fn render_active(&mut self, pos: Samples, frames: usize, out: &mut [Vec<f32>]) {
        let frames = frames.min(self.max_block);
        let Some(plan) = self.plan.as_deref_mut().filter(|p| !p.faulted) else {
            for o in out.iter_mut() {
                o.iter_mut().take(frames).for_each(|x| *x = 0.0);
            }
            return;
        };
        let ActivePlan { session, tables, status, .. } = plan;
        let s: &Session = session;
        let PlanTables { strips, phase1, aux_end, busses, main, main_fmt, main_meter, aux_fed, .. } = tables;
        let (phase1, aux_end) = (*phase1, *aux_end);
        let main_fmt = *main_fmt;
        if pos != self.last_end && !self.monitor_only {
            for st in strips.iter_mut() {
                if let Some(i) = &mut st.instrument {
                    i.plugin.all_notes_off();
                }
                st.held.clear();
                st.chase = true;
            }
        }
        self.last_end = pos.saturating_add(frames as i64);
        for (_, b) in busses.0.iter_mut() {
            for c in b.buf.iter_mut() {
                c.iter_mut().take(frames).for_each(|x| *x = 0.0);
            }
        }
        for c in main.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
        let any_solo = s.tracks.iter().any(|t| t.mixer.solo && !t.inactive);
        let live = |t: &Track| !(t.kind == TrackKind::Master || t.inactive || t.kind == TrackKind::Vca);
        let is_aux = |t: &Track| matches!(t.kind, TrackKind::Aux | TrackKind::Folder);
        let track = |st: &Strip| s.tracks.get(usize::from(st.track));
        let live_in = LiveInput { input: &self.input, monitor_only: self.monitor_only, recording: self.recording, no_suspend: self.no_suspend };
        let n_strips = strips.len();
        let (p1, rest) = strips.split_at_mut(phase1.min(n_strips));
        let (auxes, others) = rest.split_at_mut(aux_end.saturating_sub(phase1).min(rest.len()));
        // Phase 1: independent strips. They stay in the table; nothing to restore on an unwind.
        {
            let busses: &BusTable = busses;
            run_strips(p1, !self.serial, |st| {
                if let Some(t) = track(st).filter(|t| live(t) && !is_aux(t)) {
                    process_strip(s, t, st, busses, &live_in, pos, frames, any_solo);
                }
            });
        }
        let pdc = s.edit.delay_compensation;
        if pdc {
            for st in auxes.iter_mut() {
                if let Some(t) = track(st).filter(|t| !t.inactive) {
                    note_insert_latency(t, st, pos);
                }
            }
        }
        let mut lmax_m = 0usize;
        if pdc {
            for st in others.iter_mut() {
                if let Some(t) = track(st).filter(|t| t.kind == TrackKind::Master && !t.inactive) {
                    note_insert_latency(t, st, pos);
                    lmax_m = lmax_m.saturating_add(st.latency);
                }
            }
        }
        let lmax_t =
            if pdc { p1.iter().filter(|st| track(st).is_some_and(|t| live(t) && !is_aux(t))).map(|st| st.latency).max().unwrap_or(0) } else { 0 };
        let lmax_a = if pdc { auxes.iter().map(|st| st.latency).max().unwrap_or(0) } else { 0 };
        self.track_latency = lmax_t;
        self.latency = lmax_t + lmax_a + lmax_m;
        self.aux_latency = lmax_a;
        aux_fed.clear();
        if pdc {
            for t in s.tracks.iter().filter(|t| is_aux(t) && live(t)) {
                if let Route::Bus(b) = &t.mixer.output
                    && !aux_fed.contains(b)
                    && aux_fed.len() < aux_fed.capacity()
                {
                    aux_fed.push(*b);
                }
            }
        }
        let law = self.pan_law;
        let mut exceeded = |need: usize, st: &Strip| {
            if need > 0 {
                status.capacity_exceeded = status.capacity_exceeded.saturating_add(1);
                status.required_latency = status.required_latency.max(u32::try_from(need).unwrap_or(u32::MAX));
                status.affected = Some((st.id, st.generation));
            }
        };
        for st in p1.iter_mut() {
            let Some(t) = track(st).filter(|t| live(t) && !is_aux(t)) else { continue };
            let ch = st.buf.len();
            let need = st.align.set(lmax_t.saturating_sub(st.latency), ch);
            exceeded(need, st);
            st.align.process(&mut st.buf, frames);
            if t.mixer.sends.iter().flatten().any(|x| x.pre_fader) {
                let need = st.align_pre.set(lmax_t.saturating_sub(st.latency), ch);
                exceeded(need, st);
                st.align_pre.process(&mut st.pre, frames);
            }
            let waits = match &t.mixer.output {
                Route::Main => true,
                Route::Bus(b) => aux_fed.contains(b),
                _ => false,
            };
            let need = st.out_extra.set(if waits { lmax_a } else { 0 }, ch);
            exceeded(need, st);
        }
        for st in p1.iter_mut() {
            let Some(t) = track(st).filter(|t| live(t) && !is_aux(t)) else { continue };
            st.meter.measure(&st.buf, frames, st.gr);
            st.meter.plugin_failures = st.failures;
            route_into(t, st, pos, frames, MixOut { busses, main, main_fmt, law });
        }
        // Phase 2: auxes in dependency order (they read busses).
        for st in auxes.iter_mut() {
            let Some(t) = track(st).filter(|t| live(t)) else { continue };
            process_strip(s, t, st, &*busses, &live_in, pos, frames, any_solo);
            let ch = st.buf.len();
            let need = st.align.set(if pdc { lmax_a.saturating_sub(st.latency) } else { 0 }, ch);
            exceeded(need, st);
            st.align.process(&mut st.buf, frames);
            st.out_extra.set(0, ch);
            st.meter.measure(&st.buf, frames, st.gr);
            st.meter.plugin_failures = st.failures;
            route_into(t, st, pos, frames, MixOut { busses, main, main_fmt, law });
        }
        // Master faders process the main mix, in track order.
        let transport = transport_at(s, pos, !self.monitor_only, self.recording);
        for st in others.iter_mut() {
            let Some(t) = track(st).filter(|t| t.kind == TrackKind::Master && !t.inactive) else { continue };
            process_master(t, st, main, pos, frames, &transport, self.no_suspend);
            st.meter.measure(main, frames, 0.0);
            st.meter.plugin_failures = st.failures;
        }
        if self.metronome && !self.monitor_only && s.edit.click && (!s.edit.flag("click.only_during_record") || self.recording) {
            click::render(s, pos, frames, main);
        }
        for (c, o) in out.iter_mut().enumerate() {
            let src = main.get(c);
            for i in 0..frames {
                let v = src.and_then(|m| m.get(i)).copied().unwrap_or(0.0);
                if let Some(d) = o.get_mut(i) {
                    *d = if v.is_finite() { v } else { 0.0 };
                }
            }
        }
        main_meter.measure(main, frames, 0.0);
    }
}
