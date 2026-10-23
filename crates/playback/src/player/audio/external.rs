//! Clip events of tracks with an external MIDI destination (#61), pushed into the MIDI output
//! thread's ring as each block is rendered. Audio thread: no allocation, no lock, no call into
//! `transit-midi-io`; a full ring drops (and counts) the event, with a reserve for releases.
//!
//! Each event is stamped with the host time H at which its sample is presented: the block's
//! first render frame mapped through the converter phase to an output-device frame, and that
//! frame through the device clock map (or, while the map is unavailable, the host time at
//! callback entry plus the frames between). See `docs/contributing/knowledge/midi-output.md`.
use crate::player::midi_out::{Kind, OutEvent, Producer};
use soundcraft_model::Session;
use soundcraft_time::Samples;

pub(in crate::player) struct External {
    out: Producer,
    was_playing: bool,
    /// Where the previous block ended; another start is a locate or a loop wrap.
    next: Option<Samples>,
    /// Host time at callback entry and the output frame the callback started filling.
    anchor: Option<(u64, u64)>,
}

/// The block about to render, as the scheduler needs it.
#[derive(Clone, Copy, Debug)]
pub(in crate::player) struct Block {
    pub playing: bool,
    pub position: Samples,
    pub frames: usize,
    /// Host time of the block's first sample at the output, if known.
    pub host_ns: Option<u64>,
    /// Nanoseconds per session sample at the current speed.
    pub ns_per_frame: f64,
}

impl External {
    pub(in crate::player) fn new(out: Producer) -> Self {
        Self { out, was_playing: false, next: None, anchor: None }
    }

    /// At callback entry: the host time now and the output frame about to be filled.
    pub(in crate::player) fn callback(&mut self, host_now: Option<u64>, output_frame: u64) {
        self.anchor = host_now.map(|now| (now, output_frame));
    }

    /// The fallback presentation estimate of `output_frame` when the clock map is unavailable:
    /// callback entry time plus the frames between. It ignores the device's output latency, so
    /// it is early by that latency (the destination offset can compensate).
    pub(in crate::player) fn fallback_host(&self, output_frame: u64, ns_per_output_frame: f64) -> Option<u64> {
        let (now, first) = self.anchor?;
        let ahead = output_frame.checked_sub(first)? as f64 * ns_per_output_frame;
        ahead.is_finite().then(|| now.saturating_add(ahead as u64))
    }

    pub(in crate::player) fn block(&mut self, session: &Session, block: Block) {
        let at = |offset: usize| {
            block.host_ns.map_or(0, |host| {
                let delta = offset as f64 * block.ns_per_frame;
                if delta.is_finite() { host.saturating_add(delta as u64) } else { host }
            })
        };
        if !block.playing {
            if self.was_playing {
                self.out.push(OutEvent { kind: Kind::Stop, at_ns: at(0), ..OutEvent::default() });
            }
            self.was_playing = false;
            self.next = None;
            return;
        }
        let jump = self.was_playing && self.next != Some(block.position);
        if jump {
            self.out.push(OutEvent { kind: Kind::Release, at_ns: at(0), ..OutEvent::default() });
        }
        let chase = jump || !self.was_playing;
        let any_solo = session.tracks.iter().any(|t| t.mixer.solo);
        for track in &session.tracks {
            if !track.kind.is_midi() || track.inactive || track.midi_output.is_internal() {
                continue;
            }
            let silent = track.mixer.mute || (any_solo && !track.mixer.solo && !track.mixer.solo_safe);
            let out = &mut self.out;
            soundcraft_mix::external_midi_events(session, track, block.position, block.frames, chase, |event| {
                // A muted track still ends its notes.
                if silent && !transit_midi_io::is_release(event.message()) {
                    return;
                }
                out.push(OutEvent { kind: Kind::Message, track: track.id.0, at_ns: at(event.offset), bytes: event.bytes, len: event.len });
            });
        }
        self.was_playing = true;
        self.next = block.position.checked_add(block.frames as i64);
    }
}
