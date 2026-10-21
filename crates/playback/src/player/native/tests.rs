#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use crate::{action_lane, plan_bridge, player::AudioPorts};
use soundcraft_midi_io::{BackendId, fake::Fake};
use soundcraft_mix::{DeviceGeneration, PlanLimits, RenderTarget};
use soundcraft_model::{ChannelFormat, Insert, Session, TrackId, TrackKind};

const EPOCH: EngineEpoch = EngineEpoch(41);

// A deadline is only a deadlock guard. Success always requires a published state,
// received packet, source acknowledgment, or the device owner's completion flag.
fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "native MIDI worker did not reach the required state");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

fn track(session: &mut Session, selection: MidiInput) -> TrackId {
    let id = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let track = session.track_mut(id).unwrap();
    track.instrument = Some(Insert::new("subtractive_synth"));
    track.mixer.record_arm = true;
    track.midi_input = selection;
    id
}

fn selected(key: &PortKey, channel: Option<u8>) -> MidiInput {
    MidiInput {
        port: MidiPort::Device { backend: key.backend.as_str().into(), name: key.name.clone(), ordinal: key.ordinal, id: key.id.clone() },
        channel,
    }
}

struct Fixture {
    native: Native,
    owner: Owner,
    ports: AudioPorts<Command, ActionResult>,
    midi: action_lane::Audio<ingress::Packet, ActionResult>,
}

impl Fixture {
    fn new(session: Session, io: MidiIo) -> Self {
        let target = RenderTarget { device: DeviceGeneration(EPOCH.0), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 };
        let (owner, ports) = Owner::new(Arc::new(session), None, target, PlanLimits::default()).unwrap();
        let (lane, midi) = owner.midi_lane().unwrap();
        let mut native = Native::new(lane, HostOrigin::default()).unwrap();
        native.enable(io);
        Self { native, owner, ports, midi }
    }

    fn tick(&mut self) {
        self.owner.service();
        self.ports.plans.service(&mut plan_bridge::callback_scope(), true);
        self.owner.service();
        self.native.service(&mut self.owner, EPOCH);
    }

    fn ready(&mut self, open: usize, routes: usize) {
        until(|| {
            self.tick();
            let status = self.native.status();
            assert!(status.error.is_none(), "{status:?}");
            status.open_inputs == open && status.routes == routes
        });
    }

    fn packets(&mut self, count: usize) -> Vec<ingress::Packet> {
        let mut packets = Vec::new();
        until(|| {
            self.midi.service(&mut action_lane::callback_scope(), &mut self.ports.plans, &mut |event, _| {
                let action_lane::Event::Action { payload, .. } = event else { panic!("unexpected MIDI recovery") };
                packets.push(payload);
                ActionResult::MidiBuffered
            });
            packets.len() >= count
        });
        assert_eq!(packets.len(), count);
        packets
    }

    fn release_sources(&mut self) -> Vec<(u16, u64)> {
        let mut released = Vec::new();
        for _ in 0..sources::PORTS {
            self.ports.sources.service(&mut action_lane::callback_scope(), &mut self.ports.plans, &mut |port, generation, _| {
                released.push((port, generation));
                Ok(())
            });
        }
        released
    }

    fn finish(self) {
        let Self { native, owner, ports, midi } = self;
        let finished = Arc::clone(&native.io.as_ref().unwrap().finished);
        drop(native);
        until(|| finished.load(Ordering::Acquire));
        drop(midi);
        owner.retire(ports);
    }
}

#[test]
fn one_physical_open_fans_out_and_route_edits_preserve_its_sequence() {
    let fake = Fake::default();
    let key = fake.add_input("Shared keys");
    let mut session = Session::default();
    let first = track(&mut session, MidiInput::default());
    let second = track(&mut session, selected(&key, None));
    let mut f = Fixture::new(session.clone(), fake.io());
    f.ready(1, 2);
    let input_id = f.native.inputs[0].id;
    let attached_id = f.native.inputs[0].attached.as_ref().unwrap().0;
    let boundary = f.native.inputs[0].attached.as_ref().unwrap().1.clone();
    let backend = fake.input_port(&key).unwrap();
    assert!(backend.deliver(10_000, &[0x90, 60, 100]));
    let packets = f.packets(2);
    assert_eq!(packets.iter().map(|p| p.target.0).collect::<Vec<_>>(), [first, second]);
    assert_eq!(packets[0].raw, packets[1].raw);
    assert_ne!(packets[0].source.port, packets[1].source.port);
    assert_eq!(boundary.cutoff(), 1);

    let old_second = f.native.inputs[0].routes[1].port.permit();
    session.track_mut(second).unwrap().midi_input.channel = Some(1);
    f.owner.update_session(Arc::new(session.clone())).unwrap();
    f.ready(1, 2);
    assert!(!old_second.admits(), "a changed channel must retire the old route");
    let changed = f.native.inputs[0].routes.iter().find(|binding| binding.route.target.0 == second).unwrap().port.permit();
    assert!(!changed.admits_sequence(0), "the new route must reject offers preceding its physical boundary");
    assert!(changed.admits_sequence(1));
    assert_eq!(f.native.inputs[0].id, input_id);
    assert_eq!(f.native.inputs[0].attached.as_ref().unwrap().0, attached_id);
    assert!(backend.deliver(20_000, &[0x91, 61, 100]));
    let packets = f.packets(2);
    assert!(packets.iter().all(|p| p.raw.seq == 1 && p.raw.backend_time_us == 20_000));
    assert_eq!(boundary.cutoff(), 2, "reopening resets the physical sequence and fails this oracle");

    session.track_mut(second).unwrap().midi_input.port = MidiPort::None;
    f.owner.update_session(Arc::new(session)).unwrap();
    f.ready(1, 1);
    assert_eq!(f.native.inputs[0].id, input_id);
    assert!(backend.deliver(30_000, &[0x92, 62, 100]));
    let packets = f.packets(1);
    assert_eq!((packets[0].target.0, packets[0].raw.seq), (first, 2));
    assert_eq!(f.native.next_id, 1);
    f.finish();
}

#[test]
fn selections_channels_monitoring_and_disabled_tracks_are_independent() {
    let fake = Fake::default();
    let a = fake.add_input("Keys A");
    let b = fake.add_input("Keys B");
    fake.add_output("Output only");
    let mut session = Session::default();
    let a_track = track(&mut session, selected(&a, Some(0)));
    let b_track = track(&mut session, selected(&b, Some(1)));
    let monitor = session.track_mut(b_track).unwrap();
    monitor.mixer.record_arm = false;
    monitor.mixer.input_monitor = true;
    track(&mut session, MidiInput { port: MidiPort::None, channel: None });
    track(&mut session, MidiInput { port: MidiPort::All, channel: Some(16) });
    let inactive = track(&mut session, MidiInput::default());
    session.track_mut(inactive).unwrap().inactive = true;
    let unarmed = track(&mut session, MidiInput::default());
    session.track_mut(unarmed).unwrap().mixer.record_arm = false;
    let mut f = Fixture::new(session, fake.io());
    f.ready(2, 2);
    assert_eq!(f.native.status().inputs.len(), 2);
    assert!(fake.input_port(&a).unwrap().deliver(1_000, &[0x90, 60, 100, 0x91, 61, 100]));
    assert!(fake.input_port(&b).unwrap().deliver(2_000, &[0x90, 62, 100, 0x91, 63, 100]));
    let mut packets = f.packets(2);
    packets.sort_by_key(|packet| packet.target.0);
    assert_eq!(packets.iter().map(|p| (p.target.0, p.source.channel, p.raw.seq)).collect::<Vec<_>>(), [(a_track, 0, 0), (b_track, 1, 1)]);
    until(|| f.native.status().counts.offered == 4 && f.native.status().counts.filtered == 2);
    assert_eq!(f.native.status().counts.admitted, 2);
    f.finish();
}

#[test]
fn native_stop_cutoff_covers_every_route_and_retains_future_input() {
    let fake = Fake::default();
    let key = fake.add_input("Stop boundary");
    let mut session = Session::default();
    track(&mut session, MidiInput::default());
    track(&mut session, MidiInput::default());
    let mut f = Fixture::new(session, fake.io());
    f.ready(1, 2);
    let backend = fake.input_port(&key).unwrap();
    let permits: Vec<_> = f.native.inputs[0].routes.iter().map(|b| b.port.permit()).collect();
    backend.deliver(1_000, &[0x80, 60, 0]);
    until(|| f.native.status().counts.admitted == 2); // drained by ingress, stalled before audio
    f.native.cutoff(&mut f.owner).unwrap();
    f.owner.stop().unwrap();
    for permit in &permits {
        assert!(!permit.admits_sequence(0));
        assert!(permit.admits_sequence(1));
    }
    backend.deliver(2_000, &[0x90, 64, 100]);
    let packets = f.packets(4);
    for packet in &packets {
        assert_eq!(f.ports.sources.permits_sequence(packet.source.port, packet.source.generation, packet.raw.seq), packet.raw.seq == 1);
    }
    assert_eq!(packets.iter().filter(|p| p.raw.seq == 0).count(), 2);
    assert_eq!(packets.iter().filter(|p| p.raw.seq == 1).count(), 2);
    assert!(permits.iter().all(sources::Permit::admits));
    f.finish();
}

#[test]
fn removed_and_replaced_routes_release_old_generations_and_can_rearm() {
    let fake = Fake::default();
    let key = fake.add_input("Rearm");
    let mut session = Session::default();
    let keep = track(&mut session, MidiInput::default());
    let edit = track(&mut session, MidiInput::default());
    let mut f = Fixture::new(session.clone(), fake.io());
    f.ready(1, 2);
    let old = f.native.inputs[0].routes.iter().find(|b| b.route.target.0 == edit).unwrap().port.permit();
    let old_target = f.owner.binding(edit, INSTRUMENT_SLOT).unwrap();
    session.track_mut(edit).unwrap().instrument = None;
    f.owner.update_session(Arc::new(session.clone())).unwrap();
    f.ready(1, 1);
    assert_eq!(f.native.inputs[0].routes[0].route.target.0, keep);
    assert!(!old.admits());
    assert!(f.release_sources().contains(&(old.token().port(), old.token().generation())));
    f.tick();
    assert!(f.native.retired.is_empty());

    session.track_mut(edit).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    f.owner.update_session(Arc::new(session.clone())).unwrap();
    f.ready(1, 2);
    let replacement = f.native.inputs[0].routes.iter().find(|b| b.route.target.0 == edit).unwrap();
    assert_ne!(replacement.route.target, old_target);
    assert_ne!(replacement.port.token(), old.token());
    fake.input_port(&key).unwrap().deliver(1_000, &[0x90, 60, 100]);
    let packets = f.packets(2);
    assert!(packets.iter().all(|p| p.target != old_target));

    for id in [keep, edit] {
        session.track_mut(id).unwrap().mixer.record_arm = false;
    }
    f.owner.update_session(Arc::new(session.clone())).unwrap();
    f.ready(0, 0);
    until(|| !fake.input_port(&key).unwrap().deliver(0, &[]));
    assert_eq!(f.release_sources().len(), 2);
    f.tick();
    session.track_mut(edit).unwrap().mixer.record_arm = true;
    f.owner.update_session(Arc::new(session)).unwrap();
    f.ready(1, 1);
    assert_eq!(f.native.inputs[0].routes[0].route.target.0, edit);
    assert!(f.native.inputs[0].routes[0].port.admitted());
    f.finish();
}

#[test]
fn disconnect_revokes_all_fanout_permits_without_servicing_main() {
    let fake = Fake::default();
    let key = fake.add_input("Unplug while Main is stalled");
    let mut session = Session::default();
    track(&mut session, MidiInput::default());
    track(&mut session, MidiInput::default());
    let mut f = Fixture::new(session, fake.io());
    f.ready(1, 2);
    let permits: Vec<_> = f.native.inputs[0].routes.iter().map(|b| b.port.permit()).collect();
    // First packet confirms the routes reached ingress before stopping Main service.
    fake.input_port(&key).unwrap().deliver(0, &[0x90, 60, 100]);
    f.packets(2);
    fake.remove_input(&key);
    until(|| permits.iter().all(|permit| !permit.admits()));
    assert_eq!(f.native.status().routes, 2, "Main has not reconciled its stale routing list");
    let released = f.release_sources();
    for permit in permits {
        assert!(released.contains(&(permit.token().port(), permit.token().generation())));
    }
    f.ready(0, 0);
    let replacement = fake.add_input("Unplug while Main is stalled");
    assert_ne!(replacement.id, key.id);
    f.ready(1, 2);
    fake.input_port(&replacement).unwrap().deliver(0, &[0x90, 64, 100]);
    assert_eq!(f.packets(2).len(), 2);
    f.finish();
}

#[test]
fn shutdown_invalidates_immediately_and_device_owner_closes_the_connection() {
    let fake = Fake::default();
    let key = fake.add_input("Async close");
    let mut session = Session::default();
    track(&mut session, MidiInput::default());
    let mut f = Fixture::new(session, fake.io());
    f.ready(1, 1);
    let permit = f.native.inputs[0].routes[0].port.permit();
    let backend = fake.input_port(&key).unwrap();
    assert!(backend.deliver(0, &[]));
    let io = f.native.io.as_ref().unwrap();
    assert_ne!(io.thread.thread().id(), std::thread::current().id());
    let finished = Arc::clone(&io.finished);
    let Fixture { native, owner, ports, midi } = f;
    drop(native); // does not join: device handles are lexical locals of IoThread's closure
    assert!(!permit.admits());
    until(|| finished.load(Ordering::Acquire));
    assert!(!backend.deliver(0, &[]));
    drop(midi);
    owner.retire(ports);
}

#[test]
fn failed_open_is_visible_without_retries_until_reconnect() {
    let fake = Fake::default();
    let key = fake.add_input("Busy input");
    let io = fake.io();
    let (occupied, consumer) = io.open_input(&key, InputConfig::default()).unwrap();
    let mut session = Session::default();
    track(&mut session, MidiInput::default());
    let mut f = Fixture::new(session.clone(), io.clone());
    until(|| {
        f.tick();
        f.native.inputs.iter().any(|input| input.failed)
    });
    assert_eq!(f.native.status().open_inputs, 0);
    assert_eq!(f.native.status().routes, 0);
    assert!(f.native.status().error.as_deref().unwrap().contains("already open"));
    let id = f.native.inputs[0].id;
    drop(occupied);
    drop(consumer);
    for _ in 0..128 {
        f.tick();
    }
    assert_eq!(f.native.next_id, id);
    assert!(f.native.inputs[0].failed && f.native.inputs[0].closed);
    assert!(!fake.input_port(&key).unwrap().deliver(0, &[]));
    f.finish();
    let mut reopened = Fixture::new(session, io);
    reopened.ready(1, 1);
    assert!(fake.input_port(&key).unwrap().deliver(0, &[0x90, 60, 100]));
    assert_eq!(reopened.packets(1)[0].raw.seq, 0);
    reopened.finish();
}

#[test]
fn enumerated_identity_and_anonymous_fallback_match_midi_io_resolution() {
    let fake = Fake::default();
    let identified = fake.add_input("Same name");
    let another = fake.add_input("Same name");
    let io = fake.io();
    let listed = io.list_inputs().unwrap();
    let mut legacy = selected(&Fake::key("Same name", 1), None);
    assert!(accepts_port(&legacy, &another, &listed));
    let mut renamed = identified.clone();
    renamed.name = "New display name".into();
    renamed.ordinal = 99;
    assert!(accepts_port(&selected(&identified, None), &renamed, &listed));
    assert!(same_port(&identified, &renamed));
    assert!(!same_port(&identified, &another));
    renamed.backend = BackendId::WinMm;
    assert!(!accepts_port(&selected(&identified, None), &renamed, &listed));
    assert!(!same_port(&identified, &renamed));

    let anonymous = fake.add_input_anonymous("Same name");
    let listed = io.list_inputs().unwrap();
    assert!(!accepts_port(&legacy, &another, &listed), "anonymous miss must not fall through to identified ordinal 1");
    assert!(io.open_input(&Fake::key("Same name", 1), InputConfig::default()).is_err());
    legacy = selected(&Fake::key("Same name", 0), None);
    assert!(accepts_port(&legacy, &anonymous, &listed));
    assert!(!accepts_port(&legacy, &identified, &listed));
    assert!(!same_port(&identified, &anonymous));
    let (handle, _consumer) = io.open_input(&Fake::key("Same name", 0), InputConfig::default()).unwrap();
    assert_eq!(handle.key(), &anonymous);
}
