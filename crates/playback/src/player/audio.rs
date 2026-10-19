//! Prepared callback integration. This is exercised before replacing the device
//! adapter: ownership, command budgets and render-ahead boundaries are shared.
use super::{Control, endpoints};
use crate::{Shared, action_lane, device_clock::ClockKey, plan_bridge, timeline_map};
use soundcraft_mix::{PlanLimits, RenderTarget};
use soundcraft_time::{MAX_POSITION, Range, Samples};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Play { from: Samples, end: Option<Samples>, looped: Option<Range> },
    Locate(Samples),
    Recording(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionResult {
    Applied,
    InvalidPosition,
    Timeline(timeline_map::MapError),
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
    flush_releases: bool,
    timeline_fault: Option<timeline_map::MapError>,
}

impl Transport {
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
                    live.reset();
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
                    live.reset();
                })
            }
            action_lane::Event::Action { payload: Command::Recording(on), .. } => {
                self.recording = on;
                Ok(())
            }
            action_lane::Event::Stop { .. } => {
                let result = self.change(None, false, None);
                self.playing = false;
                self.end = None;
                self.loop_engaged = false;
                self.discard_pending = true;
                self.flush_releases = true;
                live.reset();
                if let Ok(position) = self.timeline.position() {
                    self.render_position = position.sample;
                }
                result
            }
            action_lane::Event::AllNotesOff { .. } => {
                live.all_notes_off();
                self.flush_releases = true;
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
    transport: Transport,
    shared: Arc<Shared>,
    pending: usize,
    read: f64,
    sample_rate: u32,
    device_rate: u32,
    speed: u32,
    faulted: bool,
    /// An escaped panic may own a heap payload. Retain it with the audio holder
    /// until main reclaims that holder; do not destroy it during error handling.
    panic_payload: Option<Box<dyn std::any::Any + Send>>,
}

pub(super) fn pair(
    target: RenderTarget,
    limits: PlanLimits,
    key: ClockKey,
    device_rate: u32,
    shared: Arc<Shared>,
) -> Result<(Control<Command, ActionResult>, Audio), timeline_map::MapError> {
    let rate = timeline_map::Rate::new(u64::from(target.sample_rate), u64::from(device_rate))?;
    let timeline = timeline_map::Timeline::new(key, 0, 0, 0, rate)?;
    let (control, plans, actions) = endpoints(target, limits);
    Ok((
        control,
        Audio {
            plans,
            actions,
            shared,
            pending: 0,
            read: 0.0,
            sample_rate: target.sample_rate,
            device_rate,
            speed: 1000,
            faulted: false,
            panic_payload: None,
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
                flush_releases: false,
                timeline_fault: None,
            },
        },
    ))
}

impl Audio {
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
            self.transport.flush_releases = true;
            self.pending = 0;
            self.read = 0.0;
            self.plans.reset();
            self.shared.position.store(end, Ordering::Relaxed);
            self.shared.playing.store(false, Ordering::Relaxed);
        }
        Ok(())
    }

    fn service_actions(&mut self, budget: &mut action_lane::Budget) {
        let transport = &mut self.transport;
        let report = self.actions.service(budget, &mut self.plans, &mut |event, live| transport.command(event, live));
        self.faulted |= report.faulted || self.transport.timeline_fault.is_some();
        if std::mem::take(&mut self.transport.discard_pending) {
            self.pending = 0;
            self.read = 0.0;
        }
        self.shared.playing.store(self.transport.playing && !self.faulted, Ordering::Relaxed);
    }

    fn flush_releases(&mut self) {
        if std::mem::take(&mut self.transport.flush_releases) {
            self.plans.flush_releases(self.plans.engine().max_block());
        }
    }

    fn render_next(&mut self, quantum: usize) -> Result<(), timeline_map::MapError> {
        let state = &mut self.transport;
        if state.playing
            && state.loop_engaged
            && let Some(range) = state.looped
            && state.render_position >= range.end
        {
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
                self.plans.reset();
                state.flush_releases = true;
            } else {
                frames = frames.min(usize::try_from(end - state.render_position).map_err(|_| timeline_map::MapError::Arithmetic)?);
            }
        }
        self.plans.set_processing(!state.playing, state.recording, self.shared.editors_open.load(Ordering::Relaxed));
        let sounding = state.playing || self.plans.hosted() || self.plans.monitoring() || self.shared.editors_open.load(Ordering::Relaxed);
        if sounding {
            self.plans.render(state.render_position, frames);
        } else {
            self.plans.clear_meters();
        }
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
                    self.panic_payload = Some(payload);
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
        out.fill(0.0);
        if self.faulted {
            return Ok(());
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
        self.flush_releases();
        let speed = self.shared.speed.load(Ordering::Relaxed).clamp(100, 4000);
        if speed != self.speed {
            let rate = timeline_map::Rate::new(u64::from(self.sample_rate) * u64::from(speed), u64::from(self.device_rate) * 1000)?;
            let epoch = self.transport.epoch.checked_add(1).ok_or(timeline_map::MapError::Arithmetic)?;
            self.transport.timeline.change(epoch, None, rate, self.transport.playing, self.transport.looped.map(|r| (r.start, r.end)))?;
            self.transport.rate = rate;
            self.transport.epoch = epoch;
            self.speed = speed;
        }
        let step = f64::from(self.sample_rate) / f64::from(self.device_rate) * f64::from(speed) / 1000.0;
        let quantum = ((out.len() / channels) as f64 * step).ceil().clamp(1.0, self.plans.engine().max_block() as f64) as usize;
        let mut consumed = 0;
        'frames: for frame in out.chunks_exact_mut(channels) {
            while self.read >= self.pending as f64 {
                self.consumed(consumed)?;
                consumed = 0;
                self.read -= self.pending as f64;
                self.pending = 0;
                self.plans.service(plans, true);
                self.service_actions(actions);
                if self.faulted {
                    break 'frames;
                }
                self.flush_releases();
                self.render_next(quantum)?;
            }
            let sounding =
                self.transport.playing || self.plans.hosted() || self.plans.monitoring() || self.shared.editors_open.load(Ordering::Relaxed);
            let output = self.plans.output();
            let index = self.read.floor() as usize;
            let fraction = (self.read - index as f64) as f32;
            let sample = |channel: usize| {
                let buffer = output.get(channel);
                let a = buffer.and_then(|v| v.get(index)).copied().unwrap_or(0.0);
                let b = if index + 1 < self.pending { buffer.and_then(|v| v.get(index + 1)).copied().unwrap_or(a) } else { a };
                a + (b - a) * fraction
            };
            if sounding {
                if output.len() > 2 && channels < output.len() {
                    let (mut left, mut right) = (0.0f32, 0.0f32);
                    for (channel, &(l, r)) in self.plans.fold().iter().enumerate().take(output.len()) {
                        let value = sample(channel);
                        left += value * l;
                        right += value * r;
                    }
                    if let Some(out) = frame.first_mut() {
                        *out = left.clamp(-1.0, 1.0);
                    }
                    if let Some(out) = frame.get_mut(1) {
                        *out = right.clamp(-1.0, 1.0);
                    }
                } else {
                    for (channel, out) in frame.iter_mut().enumerate() {
                        *out = sample(channel).clamp(-1.0, 1.0);
                    }
                }
            }
            self.read += step;
            consumed += 1;
        }
        if self.faulted {
            out.fill(0.0);
            return Ok(());
        }
        self.consumed(consumed)?;
        self.flush_releases();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
