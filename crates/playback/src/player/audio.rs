//! Prepared callback integration. This is exercised before replacing the device
//! adapter: ownership, command budgets and render-ahead boundaries are shared.
use super::{AudioPorts, sources};
#[cfg(test)]
use super::{Control, endpoints};
use crate::{Shared, action_lane, device_clock::ClockKey, plan_bridge, timeline_map};
use crate::{device_clock, record::start};
#[cfg(test)]
use soundcraft_mix::{PlanLimits, RenderTarget};
use soundcraft_time::{MAX_POSITION, Range, Samples};
use std::sync::Arc;
use std::sync::atomic::Ordering;

mod phase;
use phase::Phase;
#[cfg(not(target_arch = "wasm32"))]
mod midi;

#[derive(Debug, Clone, Copy)]
pub enum Command {
    Play { from: Samples, end: Option<Samples>, looped: Option<Range> },
    Record { ticket: start::Ticket, from: Option<Samples>, end: Option<Samples>, looped: Option<Range> },
    Locate(Samples),
    Recording(bool),
    Note(soundcraft_mix::InstrumentEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionResult {
    Applied,
    RecordingStarted(start::Point),
    RecordingClock(device_clock::Unavailable),
    RecordingNotPlaying,
    RecordingCancelled,
    MidiClock(device_clock::Unavailable),
    MidiTimeline(timeline_map::MapError),
    MidiBuffered,
    /// Buffered using callback-relative scheduling, without a usable fitted map.
    MidiBufferedApproximate,
    MidiFull,
    InvalidPosition,
    Timeline(timeline_map::MapError),
    Event(soundcraft_mix::EventOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    Timeline(timeline_map::MapError),
    HandlerFault,
    Panicked,
}

struct Transport {
    timeline: timeline_map::Timeline,
    history: timeline_map::History,
    epoch: u64,
    rate: timeline_map::Rate,
    render_position: Samples,
    playing: bool,
    recording: bool,
    end: Option<Samples>,
    looped: Option<Range>,
    loop_engaged: bool,
    discard_pending: bool,
    timeline_fault: Option<timeline_map::MapError>,
    record_start: start::Publisher,
    clock_key: ClockKey,
    clock_map: Option<device_clock::ClockMap>,
    clock_now: Option<u64>,
}

impl Transport {
    fn record(
        &mut self,
        ticket: start::Ticket,
        from: Option<Samples>,
        end: Option<Samples>,
        looped: Option<Range>,
        live: &mut plan_bridge::Processing<'_>,
    ) -> ActionResult {
        let prepared = (|| {
            let mut timeline = self.timeline;
            let mut epoch = self.epoch;
            if let Some(from) = from {
                if !(-MAX_POSITION..=MAX_POSITION).contains(&from)
                    || end.is_some_and(|end| !(-MAX_POSITION..=MAX_POSITION).contains(&end) || end < from)
                {
                    return Err(ActionResult::InvalidPosition);
                }
                epoch = epoch.checked_add(1).ok_or(ActionResult::Timeline(timeline_map::MapError::Arithmetic))?;
                timeline.change(epoch, Some(from), self.rate, true, looped.map(|r| (r.start, r.end))).map_err(ActionResult::Timeline)?;
            } else if !self.playing {
                return Err(ActionResult::RecordingNotPlaying);
            }
            let map = self.clock_map.ok_or(ActionResult::RecordingClock(device_clock::Unavailable::WarmingUp))?;
            let now = self.clock_now.ok_or(ActionResult::RecordingClock(device_clock::Unavailable::Stale))?;
            let output_frame = timeline.next_output();
            let host_ns = map.frame_to_host(self.clock_key, output_frame, now).map_err(ActionResult::RecordingClock)?;
            let position = timeline.position().map_err(ActionResult::Timeline)?;
            Ok((timeline, epoch, start::Point { host_ns, output_frame, position }))
        })();
        let (timeline, epoch, point) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.record_start.cancel(ticket);
                return error;
            }
        };
        // All fallible preparation precedes publication. If Main cancelled or
        // rearmed, neither Play nor recording changes. After success the local
        // assignments are infallible and precede rendering this output frame.
        if !self.record_start.start(ticket, point) {
            return ActionResult::RecordingCancelled;
        }
        if let Some(from) = from {
            self.timeline = timeline;
            self.epoch = epoch;
            self.playing = true;
            self.render_position = from;
            self.end = end;
            self.looped = looped;
            self.loop_engaged = looped.is_some_and(|r| from < r.end);
            self.discard_pending = true;
            live.set_transport_epoch(soundcraft_mix::TransportEpoch(epoch));
        }
        // Punching in preserves already-rendered audio and DSP state. Monitoring
        // changes at its next render boundary, while capture uses this consumed D.
        self.recording = true;
        ActionResult::RecordingStarted(point)
    }

    fn change(&mut self, seek: Option<Samples>, playing: bool, looped: Option<Range>) -> Result<(), timeline_map::MapError> {
        let epoch = self.epoch.checked_add(1).ok_or(timeline_map::MapError::Arithmetic)?;
        self.timeline.change(epoch, seek, self.rate, playing, looped.map(|r| (r.start, r.end)))?;
        self.epoch = epoch;
        self.playing = playing;
        self.looped = looped;
        Ok(())
    }

    fn command(&mut self, event: action_lane::Event<Command>, live: &mut plan_bridge::Processing<'_>) -> ActionResult {
        let result = match event {
            action_lane::Event::Action { payload: Command::Record { ticket, from, end, looped }, .. } => {
                return self.record(ticket, from, end, looped, live);
            }
            action_lane::Event::Action { payload: Command::Play { from, end, looped }, .. } => {
                if !(-MAX_POSITION..=MAX_POSITION).contains(&from)
                    || end.is_some_and(|e| !(-MAX_POSITION..=MAX_POSITION).contains(&e) || e < from)
                    || looped.is_some_and(|r| r.start >= r.end || r.start < -MAX_POSITION || r.end > MAX_POSITION)
                {
                    return ActionResult::InvalidPosition;
                }
                self.change(Some(from), true, looped).map(|()| {
                    self.render_position = from;
                    self.end = end;
                    self.loop_engaged = looped.is_some_and(|r| from < r.end);
                    self.discard_pending = true;
                    live.set_transport_epoch(soundcraft_mix::TransportEpoch(self.epoch));
                })
            }
            action_lane::Event::Action { payload: Command::Locate(at), .. } => {
                if !(-MAX_POSITION..=MAX_POSITION).contains(&at) {
                    return ActionResult::InvalidPosition;
                }
                self.change(Some(at), self.playing, self.looped).map(|()| {
                    self.render_position = at;
                    self.loop_engaged = self.looped.is_some_and(|r| at < r.end);
                    self.discard_pending = true;
                    live.set_transport_epoch(soundcraft_mix::TransportEpoch(self.epoch));
                })
            }
            action_lane::Event::Action { payload: Command::Recording(on), .. } => {
                self.recording = on;
                Ok(())
            }
            action_lane::Event::Action { payload: Command::Note(event), .. } => {
                return ActionResult::Event(live.queue_event(&event));
            }
            action_lane::Event::Stop { .. } => {
                let result = self.change(None, false, None);
                self.playing = false;
                self.recording = false;
                self.end = None;
                self.loop_engaged = false;
                self.discard_pending = true;
                live.set_transport_epoch(soundcraft_mix::TransportEpoch(self.epoch));
                live.reset();
                if let Ok(position) = self.timeline.position() {
                    self.render_position = position.sample;
                }
                result
            }
            action_lane::Event::AllNotesOff { .. } => {
                live.all_notes_off();
                Ok(())
            }
        };
        if let Err(error) = result {
            self.timeline_fault = Some(error);
            live.fault();
        }
        result.map_or_else(ActionResult::Timeline, |()| ActionResult::Applied)
    }

    fn consumed(&mut self, frames: usize, shared: &Shared) -> Result<(), timeline_map::MapError> {
        let frames = u32::try_from(frames).map_err(|_| timeline_map::MapError::Arithmetic)?;
        let mut error = None;
        self.timeline.advance(frames, |segment| {
            if let Err(e) = self.history.push(segment) {
                error = Some(e);
            }
        })?;
        if let Some(error) = error {
            return Err(error);
        }
        shared.position.store(self.timeline.position()?.sample, Ordering::Relaxed);
        shared.playing.store(self.playing, Ordering::Relaxed);
        Ok(())
    }
}

pub(super) struct Audio {
    pub(super) plans: plan_bridge::Audio,
    pub(super) actions: action_lane::Audio<Command, ActionResult>,
    pub(super) sources: sources::Audio,
    #[cfg(not(target_arch = "wasm32"))]
    midi: Option<action_lane::Audio<super::ingress::Packet, ActionResult>>,
    #[cfg(not(target_arch = "wasm32"))]
    midi_inbox: midi::Inbox,
    transport: Transport,
    shared: Arc<Shared>,
    pending: usize,
    read: Phase,
    sample_rate: u32,
    device_rate: u32,
    speed: u32,
    faulted: bool,
    /// An escaped panic may own a heap payload. Retain it with the audio holder
    /// until main reclaims that holder; do not destroy it during error handling.
    panic_payload: RetainedPanic,
    #[cfg(not(target_arch = "wasm32"))]
    clock_reader: Option<device_clock::service::SnapshotReader>,
    #[cfg(not(target_arch = "wasm32"))]
    clock_origin: device_clock::HostOrigin,
}

/// Payload ownership follows Audio back to Main. This separate field wrapper
/// also protects partial moves in test/retirement paths; Audio itself remains
/// movable into its matching endpoint bundle.
struct RetainedPanic(Option<Box<dyn std::any::Any + Send>>);
impl Drop for RetainedPanic {
    fn drop(&mut self) {
        if let Some(payload) = self.0.take()
            && let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            std::mem::forget(secondary);
        }
    }
}

pub(super) struct InitError {
    pub(super) error: timeline_map::MapError,
    // A failed initialization returns every endpoint to main for retirement.
    pub(super) ports: Box<AudioPorts<Command, ActionResult>>,
}
impl std::fmt::Debug for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InitError").field("error", &self.error).finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(super) fn pair(
    target: RenderTarget,
    limits: PlanLimits,
    key: ClockKey,
    device_rate: u32,
    shared: Arc<Shared>,
) -> Result<(Control<Command, ActionResult>, Audio), timeline_map::MapError> {
    let (control, plans, actions) = endpoints(target, limits);
    let (_, sources) = sources::pair(&control.sessions.bridge).map_err(|_| timeline_map::MapError::Arithmetic)?;
    Audio::from_ports(AudioPorts { plans, actions, sources }, key, device_rate, shared)
        .map(|(audio, _)| (control, audio))
        .map_err(|failure| failure.error)
}

impl Audio {
    pub(super) fn fault(&mut self) {
        self.faulted = true;
        self.plans.fault();
        self.actions.fault();
        self.sources.fault();
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(midi) = &mut self.midi {
            midi.fault();
        }
        self.shared.playing.store(false, Ordering::Relaxed);
    }
    /// Construct on main from the owner's exact endpoint bundle, before starting
    /// a callback. No second compiler, action lane or source registry is created.
    pub(super) fn from_ports(
        ports: AudioPorts<Command, ActionResult>,
        key: ClockKey,
        device_rate: u32,
        shared: Arc<Shared>,
    ) -> Result<(Self, start::Control), InitError> {
        let target = ports.plans.engine().active().target;
        let init = || {
            if target.device.0 != key.generation {
                return Err(timeline_map::MapError::WrongGeneration);
            }
            let rate = timeline_map::Rate::new(u64::from(target.sample_rate), u64::from(device_rate))?;
            let timeline = timeline_map::Timeline::new(key, 0, 0, 0, rate)?;
            Ok::<_, timeline_map::MapError>((rate, timeline, Phase::new(device_rate)?))
        };
        let (rate, timeline, read) = match init() {
            Ok(value) => value,
            Err(error) => return Err(InitError { error, ports: Box::new(ports) }),
        };
        let (record_control, record_start) = start::pair(soundcraft_mix::EngineEpoch(key.generation));
        Ok((
            Self {
                plans: ports.plans,
                actions: ports.actions,
                sources: ports.sources,
                #[cfg(not(target_arch = "wasm32"))]
                midi: None,
                #[cfg(not(target_arch = "wasm32"))]
                midi_inbox: midi::Inbox::new(),
                shared,
                pending: 0,
                read,
                sample_rate: target.sample_rate,
                device_rate,
                speed: 1000,
                faulted: false,
                panic_payload: RetainedPanic(None),
                #[cfg(not(target_arch = "wasm32"))]
                clock_reader: None,
                #[cfg(not(target_arch = "wasm32"))]
                clock_origin: device_clock::HostOrigin::default(),
                transport: Transport {
                    timeline,
                    history: timeline_map::History::default(),
                    epoch: 0,
                    rate,
                    render_position: 0,
                    playing: false,
                    recording: false,
                    end: None,
                    looped: None,
                    loop_engaged: false,
                    discard_pending: false,
                    timeline_fault: None,
                    record_start,
                    clock_key: key,
                    clock_map: None,
                    clock_now: None,
                },
            },
            record_control,
        ))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn set_clock(&mut self, reader: Option<device_clock::service::SnapshotReader>, origin: device_clock::HostOrigin) {
        self.clock_reader = reader;
        self.clock_origin = origin;
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn set_midi(&mut self, midi: action_lane::Audio<super::ingress::Packet, ActionResult>) {
        self.midi = Some(midi);
    }

    /// Main only, after callback quiescence. Retained panic payloads are dropped
    /// here; plugin and session owners travel back through RuntimeControl::retire.
    pub(super) fn into_ports(self) -> AudioPorts<Command, ActionResult> {
        AudioPorts { plans: self.plans, actions: self.actions, sources: self.sources }
    }
    fn consumed(&mut self, frames: usize) -> Result<(), timeline_map::MapError> {
        self.transport.consumed(frames, &self.shared)?;
        if self.transport.playing
            && !self.transport.loop_engaged
            && let Some(end) = self.transport.end
            && self.transport.timeline.position()?.sample >= end
        {
            self.transport.change(Some(end), false, None)?;
            self.transport.end = None;
            self.transport.render_position = end;
            self.pending = 0;
            self.read.reset();
            self.plans.processing().set_transport_epoch(soundcraft_mix::TransportEpoch(self.transport.epoch));
            self.plans.reset();
            self.shared.position.store(end, Ordering::Relaxed);
            self.shared.playing.store(false, Ordering::Relaxed);
        }
        Ok(())
    }

    fn service_actions(&mut self, budget: &mut action_lane::Budget) {
        let source_report = self.sources.service(budget, &mut self.plans, &mut |port, generation, live| {
            live.revoke_source(port, generation);
            Ok(())
        });
        if source_report.faulted {
            self.faulted = true;
            self.plans.fault();
        }
        let transport = &mut self.transport;
        let sources = &self.sources;
        let report = self.actions.service(budget, &mut self.plans, &mut |event, live| {
            if let action_lane::Event::Action { payload: Command::Note(note), .. } = event
                && !sources.permits(note.source.port, note.source.generation)
            {
                return ActionResult::Event(soundcraft_mix::EventOutcome::RevokedSource);
            }
            transport.command(event, live)
        });
        self.faulted |= report.faulted || self.transport.timeline_fault.is_some();
        if std::mem::take(&mut self.transport.discard_pending) {
            self.pending = 0;
            self.read.reset();
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.service_midi(budget);
        self.shared.playing.store(self.transport.playing && !self.faulted, Ordering::Relaxed);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn service_midi(&mut self, budget: &mut action_lane::Budget) {
        let Some(midi) = &mut self.midi else { return };
        let sources = &self.sources;
        let state = &self.transport;
        let inbox = &mut self.midi_inbox;
        let report = midi.service(budget, &mut self.plans, &mut |event, live| {
            let action_lane::Event::Action { payload: packet, .. } = event else {
                live.all_notes_off();
                return ActionResult::Applied;
            };
            // Worker admission is provisional: Stop can capture raw input after
            // the worker popped it and before this final dispatch.
            if !sources.permits_sequence(packet.source.port, packet.source.generation, packet.raw.seq) {
                return ActionResult::Event(soundcraft_mix::EventOutcome::RevokedSource);
            }
            let mapped =
                state.clock_map.zip(state.clock_now).and_then(|(map, now)| map.host_to_frame(state.clock_key, packet.raw.host_arrival_ns, now).ok());
            // Live playing remains available during warmup, invalid estimates or
            // expiry. Use the consumed device ordinal at audio admission, plus
            // the same explicit frame lookahead. This is approximate monitoring,
            // not a replacement clock map or a recording-placement calibration.
            // Original arrival/sequence and every source check remain intact.
            let output = mapped.unwrap_or_else(|| state.timeline.next_output());
            let Some(output) = output.checked_add(u64::from(packet.lookahead)) else {
                return ActionResult::MidiTimeline(timeline_map::MapError::Arithmetic);
            };
            inbox.admit(packet, output, mapped.is_none(), sources)
        });
        self.faulted |= report.faulted;
    }

    fn render_next(&mut self, quantum: usize, actions: &mut action_lane::Budget) -> Result<(), timeline_map::MapError> {
        let state = &mut self.transport;
        if state.playing
            && state.loop_engaged
            && let Some(range) = state.looped
            && state.render_position >= range.end
        {
            // The previous pending block has been consumed before render_next.
            // Timeline already advanced through that wrap; preserve its precise
            // consumed position/iteration when issuing the new event identity.
            state.change(None, state.playing, state.looped)?;
            self.plans.processing().set_transport_epoch(soundcraft_mix::TransportEpoch(state.epoch));
            state.render_position = range.start;
        }
        let mut frames = quantum.clamp(1, self.plans.engine().max_block());
        if state.playing
            && state.loop_engaged
            && let Some(range) = state.looped
        {
            frames = frames.min(usize::try_from(range.end - state.render_position).map_err(|_| timeline_map::MapError::Arithmetic)?);
        } else if state.playing
            && let Some(end) = state.end
        {
            if state.render_position >= end {
                state.change(Some(end), false, None)?;
                state.end = None;
                self.plans.processing().set_transport_epoch(soundcraft_mix::TransportEpoch(state.epoch));
                self.plans.reset();
            } else {
                frames = frames.min(usize::try_from(end - state.render_position).map_err(|_| timeline_map::MapError::Arithmetic)?);
            }
        }
        self.plans.set_processing(!state.playing, state.recording, self.shared.editors_open.load(Ordering::Relaxed));
        #[cfg(not(target_arch = "wasm32"))]
        self.midi_inbox.dispatch(
            self.read,
            state.timeline.next_output(),
            u64::from(self.sample_rate) * u64::from(self.speed),
            frames,
            actions,
            &self.sources,
            &mut self.plans,
        )?;
        #[cfg(target_arch = "wasm32")]
        let _ = actions;
        // The mixer owns each processor's suspension rules. A completed note
        // release does not prove that its envelope or any downstream tail ended.
        self.plans.render(state.render_position, frames);
        self.pending = frames;
        if state.playing {
            state.render_position =
                state.render_position.checked_add(frames as i64).filter(|p| *p <= MAX_POSITION).ok_or(timeline_map::MapError::Arithmetic)?;
        }
        Ok(())
    }

    /// One device callback shares these budgets across all scratch chunks and mix blocks.
    /// No Session/plan owner is dropped here. Conversion quality remains the separate F04
    /// gate; this integration initially preserves the existing linear converter.
    pub(super) fn fill(
        &mut self,
        out: &mut [f32],
        channels: usize,
        plans: &mut plan_bridge::CallbackBudget,
        actions: &mut action_lane::Budget,
    ) -> Result<(), RenderError> {
        let was_faulted = self.faulted || self.plans.faulted();
        let mut error = if was_faulted {
            None
        } else {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.fill_inner(out, channels, plans, actions))) {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(RenderError::Timeline(error)),
                Err(payload) => {
                    self.panic_payload.0 = Some(payload);
                    Some(RenderError::Panicked)
                }
            }
        };
        if !was_faulted && error.is_none() && (self.faulted || self.plans.faulted()) {
            error = Some(self.transport.timeline_fault.map_or(RenderError::HandlerFault, RenderError::Timeline));
        }
        if was_faulted || error.is_some() {
            self.faulted = true;
            self.transport.playing = false;
            self.shared.playing.store(false, Ordering::Relaxed);
            self.plans.fault();
            self.actions.fault();
            self.sources.fault();
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(midi) = &mut self.midi {
                midi.fault();
                midi.service(actions, &mut self.plans, &mut |_, _| ActionResult::Timeline(timeline_map::MapError::Invalid));
            }
            // Keep returning work under the SAME callback budgets. No plugin calls
            // or successful urgent acknowledgments are fabricated after failure.
            self.actions.service(actions, &mut self.plans, &mut |_, _| ActionResult::Timeline(timeline_map::MapError::Invalid));
            self.plans.return_pending(plans);
            self.plans.clear_meters();
            out.fill(0.0);
        }
        error.map_or(Ok(()), Err)
    }

    fn fill_inner(
        &mut self,
        out: &mut [f32],
        channels: usize,
        plans: &mut plan_bridge::CallbackBudget,
        actions: &mut action_lane::Budget,
    ) -> Result<(), timeline_map::MapError> {
        // fill() excludes an already-faulted local state before entering here.
        out.fill(0.0);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(reader) = &mut self.clock_reader {
            self.transport.clock_map = reader.poll().map;
            self.transport.clock_now = self.clock_origin.now_ns();
        }
        let channels = channels.max(1);
        self.service_actions(actions);
        if self.faulted {
            return Ok(());
        }
        self.plans.service(plans, self.pending == 0);
        self.service_actions(actions);
        if self.faulted {
            return Ok(());
        }
        let speed = self.shared.speed.load(Ordering::Relaxed).clamp(100, 4000);
        if speed != self.speed {
            let rate = timeline_map::Rate::new(u64::from(self.sample_rate) * u64::from(speed), u64::from(self.device_rate) * 1000)?;
            let epoch = self.transport.epoch.checked_add(1).ok_or(timeline_map::MapError::Arithmetic)?;
            self.transport.timeline.change(epoch, None, rate, self.transport.playing, self.transport.looped.map(|r| (r.start, r.end)))?;
            self.transport.rate = rate;
            self.transport.epoch = epoch;
            self.plans.processing().set_transport_epoch(soundcraft_mix::TransportEpoch(epoch));
            self.speed = speed;
        }
        let step = u64::from(self.sample_rate) * u64::from(speed);
        let quantum = self.read.quantum(out.len() / channels, step, self.plans.engine().max_block());
        let mut consumed = 0;
        'frames: for frame in out.chunks_exact_mut(channels) {
            while self.read.covers(self.pending) {
                self.consumed(consumed)?;
                consumed = 0;
                self.read.remove(self.pending)?;
                self.pending = 0;
                self.plans.service(plans, true);
                self.service_actions(actions);
                if self.faulted {
                    break 'frames;
                }
                self.render_next(quantum, actions)?;
            }
            let output = self.plans.output();
            let (index, fraction) = self.read.index()?;
            let sample = |channel: usize| {
                let buffer = output.get(channel);
                let a = buffer.and_then(|v| v.get(index)).copied().unwrap_or(0.0);
                let b = if index + 1 < self.pending { buffer.and_then(|v| v.get(index + 1)).copied().unwrap_or(a) } else { a };
                a + (b - a) * fraction
            };
            {
                // The block holds Main's channels, then the output sinks' (ADR 0015). With sinks,
                // everything is summed per device channel first and clamped once after.
                let main_len = self.plans.main_dest().len();
                let sinks = self.plans.sink_dest();
                let limit = |v: f32| if sinks.is_empty() { v.clamp(-1.0, 1.0) } else { v };
                if main_len > 2 && channels < main_len {
                    let (mut left, mut right) = (0.0f32, 0.0f32);
                    for (channel, &(l, r)) in self.plans.fold().iter().enumerate().take(main_len) {
                        let value = sample(channel);
                        left += value * l;
                        right += value * r;
                    }
                    if let Some(out) = frame.first_mut() {
                        *out = limit(left);
                    }
                    if let Some(out) = frame.get_mut(1) {
                        *out = limit(right);
                    }
                } else {
                    // The frame was zeroed above: device channels outside Main's path stay silent.
                    for (channel, &dest) in self.plans.main_dest().iter().enumerate() {
                        if let Some(out) = frame.get_mut(usize::from(dest)) {
                            *out = limit(sample(channel));
                        }
                    }
                }
                if !sinks.is_empty() {
                    for (k, &dest) in sinks.iter().enumerate() {
                        if let Some(out) = frame.get_mut(usize::from(dest)) {
                            *out += sample(main_len + k);
                        }
                    }
                    for out in frame.iter_mut() {
                        *out = out.clamp(-1.0, 1.0);
                    }
                }
            }
            self.read.advance(step)?;
            consumed += 1;
        }
        if self.faulted {
            out.fill(0.0);
            return Ok(());
        }
        self.consumed(consumed)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
