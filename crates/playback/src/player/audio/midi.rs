//! Keep future MIDI deadlines in device time until their render block exists.
//! A varispeed change cannot invalidate an eagerly projected future render frame.
use super::{ActionResult, Phase, sources, timeline_map};
use crate::player::ingress::Packet;
use crate::{action_lane, plan_bridge};
use soundcraft_mix::{Arrival, Due, InstrumentEvent, RenderFrame};

const CAPACITY: usize = action_lane::NOTE_CAPACITY;
const RELEASE_RESERVE: usize = 32;

#[derive(Clone, Copy)]
struct Pending {
    packet: Packet,
    output: u64,
}

pub(super) struct Inbox {
    pending: Box<[Option<Pending>]>,
    count: usize,
}

impl Inbox {
    /// Main prepares all storage before the callback can enter.
    pub(super) fn new() -> Self {
        Self { pending: vec![None; CAPACITY].into_boxed_slice(), count: 0 }
    }

    pub(super) fn admit(&mut self, packet: Packet, output: u64) -> ActionResult {
        if self.count == CAPACITY || (!packet.raw.is_release() && self.count >= CAPACITY - RELEASE_RESERVE) {
            return ActionResult::MidiFull;
        }
        let Some(slot) = self.pending.iter_mut().find(|slot| slot.is_none()) else {
            return ActionResult::MidiFull;
        };
        *slot = Some(Pending { packet, output });
        self.count += 1;
        ActionResult::MidiBuffered
    }

    pub(super) fn dispatch(
        &mut self,
        phase: Phase,
        current_output: u64,
        step: u64,
        frames: usize,
        budget: &mut action_lane::Budget,
        sources: &sources::Audio,
        plans: &mut plan_bridge::Audio,
    ) -> Result<(), timeline_map::MapError> {
        // Stop/revoke can advance after raw dequeue, intake, or a previous block.
        // This fixed-capacity pass also frees stale future entries immediately.
        for slot in &mut self.pending {
            if slot.is_some_and(|pending| {
                let packet = pending.packet;
                !sources.permits_sequence(packet.source.port, packet.source.generation, packet.raw.seq)
            }) {
                *slot = None;
                self.count -= 1;
            }
        }
        let next = plans.engine().next_frame();
        let end = next.0.checked_add(frames as u64).ok_or(timeline_map::MapError::Arithmetic)?;
        loop {
            // Device deadlines are globally ordered across inputs. A later offer
            // on one port cannot hide an earlier release waiting on another.
            let earliest =
                self.pending.iter().enumerate().filter_map(|(index, pending)| pending.map(|pending| (index, pending))).min_by_key(|(_, pending)| {
                    (pending.output, pending.packet.raw.host_arrival_ns, pending.packet.source.port, pending.packet.raw.seq)
                });
            let Some((index, pending)) = earliest else { break };
            let due = phase.project(next, 0, current_output, pending.output, step)?;
            if due.0 >= end || !budget.claim_midi_dispatch() {
                break;
            }
            if let Some(slot) = self.pending.get_mut(index) {
                *slot = None;
            }
            self.count -= 1;
            let packet = pending.packet;
            // Final check immediately before queue_event, matching the worker's
            // sequence cutoff protocol. No future render block is committed here.
            if !sources.permits_sequence(packet.source.port, packet.source.generation, packet.raw.seq) {
                continue;
            }
            let outcome = plans.processing().queue_event(&InstrumentEvent {
                engine: packet.engine,
                target: packet.target,
                due: Due::Frame(RenderFrame(due.0)),
                transport: None,
                source: packet.source,
                message: packet.message,
                arrival: Arrival { host_ns: packet.raw.host_arrival_ns, sequence: packet.raw.seq },
            });
            if !outcome.accepted() && outcome != soundcraft_mix::EventOutcome::Unmatched && packet.raw.is_release() {
                sources.invalidate(packet.source.port, packet.source.generation);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
