#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::super::{
    Audio, Command,
    tests::{fill, fixture_session, make, recording_clock},
};
use super::*;
use crate::player::Control;
use soundcraft_mix::{EngineEpoch, Message, NoteId, SlotBinding, Source};
use soundcraft_model::{ChannelFormat, Insert, Session, TrackKind};
use soundcraft_rt::testing::measure;
use std::sync::{Arc, atomic::Ordering};

struct Rig {
    control: Control<Command, ActionResult>,
    registry: sources::Control,
    lane: action_lane::Control<Packet, ActionResult>,
    audio: Audio,
    token: sources::Token,
    target: SlotBinding,
}

impl Rig {
    fn new(device: u32) -> Self {
        let mut session = Session::default();
        let track = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        session.track_mut(track).unwrap().instrument = Some(Insert::new("subtractive_synth"));
        let (mut control, mut audio, _) = fixture_session(device, Arc::new(session));
        let (mut registry, sources) = sources::pair(&control.sessions.bridge).unwrap();
        let token = registry.register(4).unwrap();
        audio.sources = sources;
        let (lane, midi) = action_lane::pair_with_result(&control.sessions.bridge);
        audio.set_midi(midi);
        fill(&mut audio, &mut []);
        control.service(&mut make);
        let target = control.sessions.active_bindings()[0];
        recording_clock(&mut audio, device);
        audio.plans.install_event_tap_for_test(512);
        Self { control, registry, lane, audio, token, target }
    }
    fn packet(&self, output: u64, sequence: u64, release: bool) -> Packet {
        let audio = &self.audio;
        let host = audio.transport.clock_map.unwrap().frame_to_host(audio.transport.clock_key, output, audio.transport.clock_now.unwrap()).unwrap();
        Packet {
            engine: EngineEpoch(1),
            target: self.target,
            source: Source { port: self.token.port(), generation: self.token.generation(), channel: 0 },
            message: if release {
                Message::NoteOff { key: 60, velocity: 0, id: NoteId::NONE }
            } else {
                Message::NoteOn { key: 60, velocity: 100, id: NoteId::NONE }
            },
            raw: soundcraft_midi_io::InputEvent {
                host_arrival_ns: host,
                backend_time_us: 987,
                seq: sequence,
                bytes: [if release { 0x80 } else { 0x90 }, 60, 100],
                len: 3,
            },
            lookahead: 0,
        }
    }
    fn send(&mut self, output: u64, sequence: u64, release: bool) -> Packet {
        let packet = self.packet(output, sequence, release);
        self.lane.send_note(packet, if release { action_lane::NoteKind::Off } else { action_lane::NoteKind::On }).unwrap();
        packet
    }
    fn render(&mut self, frames: usize) {
        let mut output = vec![0.0; frames * 2];
        let (_, heap) = measure(|| fill(&mut self.audio, &mut output));
        assert!(heap.is_zero(), "{heap:?}");
    }
}

#[test]
fn real_audio_path_preserves_original_arrival_and_partition_independent_due_frames() {
    for device in [44_100, 48_000, 96_000] {
        for partitions in [vec![256], vec![17, 31, 64, 1, 143]] {
            let mut rig = Rig::new(device);
            let mut packet = rig.packet(100, 0, false);
            packet.lookahead = 37;
            rig.lane.send_note(packet, action_lane::NoteKind::On).unwrap();
            for frames in partitions {
                rig.render(frames);
            }
            let tap = rig.audio.plans.engine().event_tap();
            assert_eq!(tap.len(), 1);
            assert_eq!(tap[0].due, Some(RenderFrame((137 * 48_000 + u64::from(device) / 2) / u64::from(device))));
            assert_eq!(tap[0].arrival, Arrival { host_ns: packet.raw.host_arrival_ns, sequence: 0 });
            assert_eq!(tap[0].source, packet.source);
            assert_eq!(tap[0].outcome, soundcraft_mix::EventOutcome::Queued);
        }
    }
}

#[test]
fn future_note_is_projected_after_speed_changes_without_moving_its_device_deadline() {
    let mut rig = Rig::new(48_000);
    rig.send(200, 0, false);
    rig.render(64);
    assert!(rig.audio.plans.engine().event_tap().is_empty());
    assert_eq!(rig.audio.midi_inbox.count, 1);
    rig.audio.shared.speed.store(2000, Ordering::Relaxed);
    rig.render(160);
    let tap = rig.audio.plans.engine().event_tap();
    assert_eq!(tap.len(), 1);
    assert_eq!(tap[0].due, Some(RenderFrame(64 + (200 - 64) * 2)));
}

#[test]
fn stop_cutoff_cancels_a_future_note_already_buffered_on_audio() {
    let mut rig = Rig::new(48_000);
    rig.send(200, 0, false);
    rig.render(64);
    rig.registry.cancel_before(rig.token, 1).unwrap();
    rig.control.stop().unwrap();
    rig.render(256);
    assert!(rig.audio.plans.engine().event_tap().is_empty());
    assert_eq!(rig.audio.midi_inbox.count, 0);
    rig.send(400, 1, false);
    rig.render(128);
    assert_eq!(rig.audio.plans.engine().event_tap().len(), 1, "new raw offers after Stop remain eligible");
}

#[test]
fn later_arrival_does_not_hide_an_earlier_deadline_on_another_route() {
    let mut rig = Rig::new(48_000);
    rig.send(400, 0, false);
    rig.send(20, 1, false);
    rig.render(64);
    let tap = rig.audio.plans.engine().event_tap();
    assert_eq!(tap.len(), 1);
    assert_eq!(tap[0].arrival.sequence, 1);
    assert_eq!(tap[0].due, Some(RenderFrame(20)));
    assert_eq!(rig.audio.midi_inbox.count, 1);
}

#[test]
fn deferred_capacity_reserves_releases_and_unknown_releases_do_not_revoke_other_notes() {
    let mut rig = Rig::new(48_000);
    for sequence in 0..(CAPACITY - RELEASE_RESERVE) {
        assert_eq!(rig.audio.midi_inbox.admit(rig.packet(400, sequence as u64, false), 400), ActionResult::MidiBuffered);
    }
    assert_eq!(rig.audio.midi_inbox.admit(rig.packet(400, 900, false), 400), ActionResult::MidiFull);
    for sequence in 0..RELEASE_RESERVE {
        assert_eq!(rig.audio.midi_inbox.admit(rig.packet(400, 1000 + sequence as u64, true), 400), ActionResult::MidiBuffered);
    }
    assert_eq!(rig.audio.midi_inbox.admit(rig.packet(400, 1200, true), 400), ActionResult::MidiFull);
    let mut rig = Rig::new(48_000);
    rig.send(10, 0, true);
    rig.render(64);
    assert!(rig.registry.admits(rig.token));
    assert_eq!(rig.audio.plans.engine().event_tap()[0].outcome, soundcraft_mix::EventOutcome::Unmatched);
}
