//! A track's clip events for an external MIDI destination (#61), as raw MIDI 1.0 messages.
//!
//! The same traversal as the instrument path ([`super::sorted_clip_events`]): MIDI Track Offset,
//! real-time properties, muted clips, clip-end clamping and off-before-on at one sample all match
//! what the hosted instrument would have played. Runs on the audio thread: no allocation, no lock.

use super::{ClipEvent, Rtp, midi_offset, note_spans, played_note, sorted_clip_events};
use soundcraft_model::{ClipContent, MidiOutChannel, MidiOutput, Session, Track};
use soundcraft_time::Samples;

/// One message for an external destination, at a sample offset into the block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExternalMidi {
    /// Sample offset into the block, `0..frames`.
    pub offset: usize,
    /// Status first; only `bytes[..len]` is meaningful.
    pub bytes: [u8; 3],
    /// 2 or 3.
    pub len: u8,
}

impl ExternalMidi {
    /// The meaningful bytes.
    #[must_use]
    pub fn message(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len).min(3)).unwrap_or(&[])
    }

    fn of(offset: usize, event: ClipEvent, channel: MidiOutChannel) -> Self {
        match event {
            ClipEvent::On(pitch, velocity, recorded) => {
                Self { offset, bytes: [0x90 | channel.resolve(recorded), pitch & 0x7F, velocity.clamp(1, 127)], len: 3 }
            }
            ClipEvent::Off(pitch, recorded) => Self { offset, bytes: [0x80 | channel.resolve(recorded), pitch & 0x7F, 64], len: 3 },
            ClipEvent::Control(msg, recorded) => {
                let kind = msg[0] & 0xF0;
                let len = if matches!(kind, 0xC0 | 0xD0) { 2 } else { 3 };
                Self { offset, bytes: [kind | channel.resolve(recorded), msg[1] & 0x7F, if len == 3 { msg[2] & 0x7F } else { 0 }], len }
            }
        }
    }
}

/// Visits the clip events of `t` in `[pos, pos + frames)` in delivery order, as messages on the
/// channel its [`MidiOutput`] chooses. Nothing for an internal destination.
///
/// With `chase`, every note that sounds across `pos` (started before it, ends after it) is
/// started again at offset 0, after the block's own offset-0 events: a loop lap or a locate
/// restarts held notes, as #47 does for recorded laps.
pub fn external_midi_events(s: &Session, t: &Track, pos: Samples, frames: usize, chase: bool, mut visit: impl FnMut(ExternalMidi)) {
    let MidiOutput::External { channel, .. } = &t.midi_output else { return };
    let channel = *channel;
    let mut chased = !chase;
    sorted_clip_events(s, t, pos, frames, |at, event| {
        if !chased && at > 0 {
            chased = true;
            sounding_notes(s, t, pos, |pitch, velocity, recorded| visit(ExternalMidi::of(0, ClipEvent::On(pitch, velocity, recorded), channel)));
        }
        visit(ExternalMidi::of(at, event, channel));
    });
    if !chased {
        sounding_notes(s, t, pos, |pitch, velocity, recorded| visit(ExternalMidi::of(0, ClipEvent::On(pitch, velocity, recorded), channel)));
    }
}

/// Notes of `t` that started before `pos` and are still sounding at it, as playback (loop cycles
/// included) would have them. Runs where the chase does (the external driver, possibly the audio
/// thread): no allocation, no lock.
fn sounding_notes(s: &Session, t: &Track, pos: Samples, mut visit: impl FnMut(u8, u8, u8)) {
    let pos = pos.saturating_sub(midi_offset(s, t));
    let rtp = Rtp::of(s, t);
    for clip in t.clips() {
        if clip.muted || clip.start >= pos || clip.end() <= pos {
            continue;
        }
        let ClipContent::Midi { sequence } = &clip.content else { continue };
        let base = s.tempo.samples_to_ticks(clip.start, s.sample_rate);
        let looping = clip.looping.filter(|l| l.is_valid());
        for n0 in &sequence.notes {
            let Some((n, on, off)) = played_note(s, clip, base, rtp, n0, looping.is_some()) else { continue };
            // The same spans playback plays (loop cycles, seam cuts, negative-tick rule), looked up
            // for the one sample `pos`.
            note_spans(clip, looping, on, off, pos, pos.saturating_add(1), |on, off| {
                if on < pos && off > pos {
                    visit(n.pitch, n.velocity, n.channel);
                }
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use soundcraft_midi::{CtrlEvent, CtrlKind, Note, Sequence};
    use soundcraft_model::{ChannelFormat, Clip, ClipId, MidiOutPort, TrackKind};

    fn session(channel: MidiOutChannel) -> (Session, soundcraft_model::TrackId) {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Midi, ChannelFormat::Mono, None);
        let ppq = soundcraft_time::TICKS_PER_QUARTER;
        let mut seq = Sequence::default();
        // At 120 bpm and 48 kHz a quarter is 24 000 samples.
        seq.notes.push(Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 2, start: 0, length: ppq });
        seq.notes.push(Note { pitch: 64, velocity: 90, release_velocity: 64, channel: 2, start: ppq, length: ppq * 2 });
        seq.ctrls.push(CtrlEvent { tick: ppq / 2, channel: 2, kind: CtrlKind::Cc(64), value: 127 });
        seq.ctrls.push(CtrlEvent { tick: ppq / 2, channel: 2, kind: CtrlKind::Program, value: 5 });
        let track = s.track_mut(t).unwrap();
        track.playlist_mut().unwrap().clips.push(Clip::midi(ClipId(1), "phrase", 0, 4 * 24_000, seq));
        track.midi_output = MidiOutput::External { port: MidiOutPort::default(), channel, offset_ms: 0.0 };
        (s, t)
    }

    fn events(s: &Session, t: soundcraft_model::TrackId, pos: Samples, frames: usize, chase: bool) -> Vec<(usize, Vec<u8>)> {
        let mut out = Vec::new();
        external_midi_events(s, s.track(t).unwrap(), pos, frames, chase, |e| out.push((e.offset, e.message().to_vec())));
        out
    }

    #[test]
    fn notes_and_controllers_keep_order_and_the_recorded_or_forced_channel() {
        let (s, t) = session(MidiOutChannel::AsRecorded);
        assert_eq!(s.sample_rate.hz(), 48_000);
        let got = events(&s, t, 0, 48_000, false);
        assert_eq!(
            got,
            vec![
                (0, vec![0x92, 60, 100]),
                (12_000, vec![0xB2, 64, 127]),
                (12_000, vec![0xC2, 5]),
                (24_000, vec![0x82, 60, 64]),
                (24_000, vec![0x92, 64, 90]),
            ]
        );
        let (s, t) = session(MidiOutChannel::Channel(9));
        assert!(events(&s, t, 0, 48_000, false).iter().all(|(_, m)| m[0] & 0x0F == 9));
    }

    #[test]
    fn chase_restarts_notes_sounding_across_the_position_after_offset_zero_offs() {
        let (s, t) = session(MidiOutChannel::AsRecorded);
        // At 36 000 the second note (24 000..72 000) sounds; nothing else happens until its off.
        assert_eq!(events(&s, t, 36_000, 512, true), vec![(0, vec![0x92, 64, 90])]);
        assert!(events(&s, t, 36_000, 512, false).is_empty());
        // At 24 000 the first note's off and the second note's on are block events, not chased.
        assert_eq!(events(&s, t, 24_000, 512, true), vec![(0, vec![0x82, 60, 64]), (0, vec![0x92, 64, 90])]);
    }

    #[test]
    fn internal_tracks_emit_nothing() {
        let (mut s, t) = session(MidiOutChannel::AsRecorded);
        s.track_mut(t).unwrap().midi_output = MidiOutput::Internal;
        assert!(events(&s, t, 0, 48_000, true).is_empty());
    }

    /// A looped clip (loop 0..4 beats, length 6) with E64 from beat 3 to 5 (crosses the seam) and a
    /// note a start trim moved before the clip start (-1 beat, 3 beats long).
    fn looped() -> (Session, soundcraft_model::TrackId, i64) {
        let (mut s, t) = session(MidiOutChannel::AsRecorded);
        let ppq = soundcraft_time::TICKS_PER_QUARTER;
        let beat = s.tempo.tick_to_samples(ppq, s.sample_rate);
        let mut seq = Sequence::default();
        seq.notes.push(Note { pitch: 64, velocity: 100, release_velocity: 64, channel: 5, start: 3 * ppq, length: 2 * ppq });
        let track = s.track_mut(t).unwrap();
        let mut c = Clip::midi(ClipId(2), "loop", 0, 6 * beat, seq);
        c.looping = Some(soundcraft_model::ClipLoop { start: 0, end: 4 * beat });
        let clips = &mut track.playlist_mut().unwrap().clips;
        clips.clear();
        clips.push(c);
        (s, t, beat)
    }

    #[test]
    fn chase_follows_the_loop_cut_at_the_seam_and_restarted_in_later_cycles() {
        let (s, t, beat) = looped();
        // Playing from 0 sounds E on at 3, off at the seam (4), on again at 7 (cut by the clip end at 6).
        let all: Vec<_> = events(&s, t, 0, 8 * beat as usize, false).into_iter().map(|(at, m)| (at as i64 / beat, m[0])).collect();
        assert_eq!(all, vec![(3, 0x95), (4, 0x85)]);
        // 4.5: the loop is not sounding E (cycle 1 restarts it at 7, past the clip end): nothing chased.
        assert!(events(&s, t, 4 * beat + beat / 2, 512, true).is_empty());
        // 3.5: inside the first pass, E sounds; its off at 4 follows in the next block.
        let at = 3 * beat + beat / 2;
        assert_eq!(events(&s, t, at, 512, true), vec![(0, vec![0x95, 64, 100])]);
        let off = events(&s, t, 4 * beat - 10, 512, false);
        assert_eq!(off, vec![(10, vec![0x85, 64, 64])]);
    }

    #[test]
    fn chase_into_a_later_cycle_restarts_a_note_the_loop_sounds_there_once() {
        let (mut s, t, beat) = looped();
        s.find_clip_mut(ClipId(2)).unwrap().length = 12 * beat;
        // Cycle 1 sounds E from 7 to 8 (cut at its seam 8): at 7.5 it is chased exactly once.
        assert_eq!(events(&s, t, 7 * beat + beat / 2, 512, true), vec![(0, vec![0x95, 64, 100])]);
        // 8.5 is past the seam: not sounding.
        assert!(events(&s, t, 8 * beat + beat / 2, 512, true).is_empty());
    }

    #[test]
    fn chase_skips_a_trimmed_negative_tick_note_in_the_first_pass() {
        let (mut s, t, beat) = looped();
        let ppq = soundcraft_time::TICKS_PER_QUARTER;
        let c = s.find_clip_mut(ClipId(2)).unwrap();
        let ClipContent::Midi { sequence: seq } = &mut c.content else { panic!() };
        seq.notes.clear();
        seq.notes.push(Note { pitch: 70, velocity: 80, release_velocity: 64, channel: 0, start: -ppq, length: 3 * ppq });
        // The note starts before the clip and before the loop range: the loop plays it nowhere, so no
        // position inside it, in the first pass or a later cycle, chases it.
        assert!(events(&s, t, beat / 2, 512, true).is_empty());
        assert!(events(&s, t, 4 * beat + beat / 2, 512, true).is_empty());
    }
}
