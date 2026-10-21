#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use crate::plan_bridge;
use soundcraft_midi_io::{InputConfig, fake::Fake};
use soundcraft_mix::{DeviceGeneration, PlanLimits, RenderTarget, SlotGen};
use soundcraft_model::TrackId;
use soundcraft_rt::testing::measure;

fn route() -> Route {
    Route { engine: EngineEpoch(1), target: (TrackId(7), soundcraft_mix::INSTRUMENT_SLOT, SlotGen::VACANT), channels: u16::MAX, lookahead: 128 }
}
fn bridge() -> (plan_bridge::Control, plan_bridge::Audio) {
    plan_bridge::pair(
        RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 },
        PlanLimits::default(),
    )
}
fn pump(lane: action_lane::Control<Packet, ActionResult>) -> (SyncSender<Update>, Pump) {
    let (send, receive) = mpsc::sync_channel(sources::PORTS);
    (send, Pump { lane, receive, inputs: Vec::with_capacity(sources::PORTS), cursor: 0, shared: Arc::default() })
}
fn input(consumer: InputConsumer, permit: sources::Permit) -> Update {
    Update::Input(Input { id: 1, consumer, routes: vec![(permit, route())] })
}

#[test]
fn approximate_audio_receipts_are_counted_without_refusing_or_revoking_releases() {
    let (control, mut plans) = bridge();
    let (mut registry, sources) = sources::pair(&control).unwrap();
    let token = registry.register(4).unwrap();
    let (lane, mut audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Unmapped");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    send.send(input(consumer, registry.permit(token).unwrap())).ok().unwrap();
    fake.input_port(&key).unwrap().deliver(0, &[0x90, 60, 100, 0x80, 60, 0]);
    worker.step();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _| ActionResult::MidiBufferedApproximate);
    worker.step();
    assert_eq!(worker.shared.approximate.load(Ordering::Relaxed), 2);
    assert_eq!(worker.shared.refused.load(Ordering::Relaxed), 0);
    assert!(sources.permits(4, token.generation()));
    assert_eq!(worker.lane.note_credits(), action_lane::NOTE_CAPACITY);
}

#[test]
fn channel_messages_decode_without_losing_channel_or_release_semantics() {
    for channel in 0..16 {
        let raw = |status, a, b, len| InputEvent { bytes: [status | channel, a, b], len, ..Default::default() };
        for (status, a, b, len, expected) in [
            (0x80, 60, 64, 3, Message::NoteOff { key: 60, velocity: 64, id: NoteId::NONE }),
            (0x90, 60, 0, 3, Message::NoteOff { key: 60, velocity: 0, id: NoteId::NONE }),
            (0x90, 60, 1, 3, Message::NoteOn { key: 60, velocity: 1, id: NoteId::NONE }),
            (0xa0, 60, 85, 3, Message::PolyPressure { key: 60, value: 85, id: NoteId::NONE }),
            (0xb0, 64, 0, 3, Message::Control { cc: 64, value: 0 }),
            (0xc0, 9, 0, 2, Message::Program { program: 9 }),
            (0xd0, 85, 0, 2, Message::ChannelPressure { value: 85 }),
            (0xe0, 127, 127, 3, Message::PitchBend { value: 16383 }),
        ] {
            assert_eq!(decode(raw(status, a, b, len)), Some((channel, expected)));
        }
        for event in [raw(0x90, 60, 128, 3), raw(0x90, 128, 64, 3), raw(0x90, 60, 64, 2), raw(0xc0, 5, 0, 3), raw(0xf0, 0, 0, 1)] {
            assert_eq!(decode(event), None);
        }
    }
}

#[test]
fn fake_backend_preserves_raw_time_and_sequence_through_the_real_action_lane() {
    let (control, mut plans) = bridge();
    let (mut registry, sources) = sources::pair(&control).unwrap();
    let token = registry.register(4).unwrap();
    let (lane, mut audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Keys");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    let port = fake.input_port(&key).unwrap();
    send.send(input(consumer, registry.permit(token).unwrap())).ok().unwrap();
    port.deliver(123_000, &[0x95, 60, 100, 0x95, 60, 0]);
    worker.step();
    let mut packets = [None; 2];
    let mut index = 0;
    let (_, heap) = measure(|| {
        audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |event, _| {
            let action_lane::Event::Action { payload, .. } = event else { panic!("unexpected recovery") };
            assert!(sources.permits_sequence(payload.source.port, payload.source.generation, payload.raw.seq));
            packets[index] = Some(payload);
            index += 1;
            ActionResult::Event(EventOutcome::Queued)
        });
    });
    assert!(heap.is_zero(), "{heap:?}");
    let (on, off) = (packets[0].unwrap(), packets[1].unwrap());
    assert_eq!((on.source.port, on.source.channel, on.source.generation), (4, 5, token.generation()));
    assert_eq!((on.raw.seq, off.raw.seq), (0, 1));
    assert_eq!((on.raw.backend_time_us, off.raw.backend_time_us), (123_000, 123_000));
    assert_eq!(on.raw.host_arrival_ns, off.raw.host_arrival_ns);
    assert!(matches!(off.message, Message::NoteOff { .. }));
    assert_eq!(on.lookahead, 128);
    worker.step();
    assert_eq!(worker.lane.note_credits(), 256);
}

#[test]
fn release_reserve_and_generation_recovery_survive_a_stalled_audio_consumer() {
    let (control, mut plans) = bridge();
    let (mut registry, mut sources) = sources::pair(&control).unwrap();
    let token = registry.register(4).unwrap();
    let (lane, _audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Flood");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig { capacity: 1024, ..Default::default() }).unwrap();
    send.send(input(consumer, registry.permit(token).unwrap())).ok().unwrap();
    let port = fake.input_port(&key).unwrap();
    for _ in 0..300 {
        port.deliver(0, &[0x90, 60, 100]);
    }
    worker.step();
    worker.step();
    assert_eq!(worker.lane.note_credits(), 32, "ordinary messages cannot consume the release reserve");
    for _ in 0..32 {
        port.deliver(0, &[0x80, 60, 0]);
    }
    worker.step();
    assert_eq!(worker.lane.note_credits(), 0);
    assert!(sources.permits(4, token.generation()));
    port.deliver(0, &[0xb0, 64, 0]);
    worker.step();
    assert!(!sources.permits(4, token.generation()));
    let mut released = None;
    let (_, heap) = measure(|| {
        sources.service(&mut action_lane::callback_scope(), &mut plans, &mut |port, generation, _| {
            released = Some((port, generation));
            Ok(())
        });
    });
    assert!(heap.is_zero());
    assert_eq!(released, Some((4, token.generation())));
}

#[test]
fn raw_overflow_and_disconnect_revoke_without_a_ui_poll() {
    for disconnect in [false, true] {
        let (control, _plans) = bridge();
        let (mut registry, audio_sources) = sources::pair(&control).unwrap();
        let token = registry.register(2).unwrap();
        let (lane, _audio) = action_lane::pair_with_result(&control);
        let (send, mut worker) = pump(lane);
        let fake = Fake::default();
        let key = fake.add_input("Unplug");
        let io = fake.io();
        let (_handle, consumer) = io.open_input(&key, InputConfig { capacity: 1, release_reserve: 0, ..Default::default() }).unwrap();
        send.send(input(consumer, registry.permit(token).unwrap())).ok().unwrap();
        worker.step();
        if disconnect {
            let mut watcher = io.watcher().unwrap();
            fake.remove_input(&key);
            watcher.poll().unwrap();
        } else {
            let port = fake.input_port(&key).unwrap();
            port.deliver(0, &[0x90, 60, 100]);
            port.deliver(0, &[0x80, 60, 0]);
        }
        worker.step();
        assert!(!audio_sources.permits(2, token.generation()));
        assert!(worker.inputs.is_empty());
    }
}

#[test]
fn shared_note_budget_is_not_renewed_for_ingress_or_scratch_chunks() {
    let (control, mut plans) = bridge();
    let (mut registry, _sources) = sources::pair(&control).unwrap();
    let token = registry.register(0).unwrap();
    let (lane, mut midi_audio) = action_lane::pair_with_result(&control);
    let (mut ui, mut ui_audio) = action_lane::pair_with_result::<(), ()>(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Busy");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    send.send(input(consumer, registry.permit(token).unwrap())).ok().unwrap();
    for _ in 0..64 {
        fake.input_port(&key).unwrap().deliver(0, &[0x90, 60, 100]);
        ui.send_note((), NoteKind::On).unwrap();
    }
    worker.step();
    let mut budget = action_lane::callback_scope();
    let mut count = 0;
    let (_, heap) = measure(|| {
        for _ in 0..8 {
            midi_audio.service(&mut budget, &mut plans, &mut |_, _| {
                count += 1;
                ActionResult::Event(EventOutcome::Queued)
            });
            ui_audio.service(&mut budget, &mut plans, &mut |_, _| count += 1);
        }
    });
    assert!(heap.is_zero());
    assert_eq!(count, 32);
}

#[test]
fn stop_cutoff_rejects_already_drained_raw_events_and_preserves_later_input() {
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 };
    let (mut owner, mut ports) =
        RuntimeControl::<(), ()>::new(Arc::new(soundcraft_model::Session::default()), None, target, PlanLimits::default()).unwrap();
    let token = owner.register_source(3).unwrap();
    let (lane, mut audio) = owner.midi_lane().unwrap();
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Stop race");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    let permit = owner.source_permit(token).unwrap();
    let main_port = Port { permit: permit.clone(), boundary: consumer.boundary() };
    send.send(input(consumer, permit)).ok().unwrap();
    let backend = fake.input_port(&key).unwrap();
    backend.deliver(1_000, &[0x80, 60, 0]);
    worker.step(); // already drained from the raw queue, not dispatched to audio
    main_port.cancel_before_stop(&mut owner).unwrap();
    owner.stop().unwrap();
    backend.deliver(2_000, &[0x90, 64, 100]);
    worker.step();
    let mut seen = [None; 2];
    let mut index = 0;
    audio.service(&mut action_lane::callback_scope(), &mut ports.plans, &mut |event, _| {
        let action_lane::Event::Action { payload, .. } = event else { panic!("unexpected recovery") };
        let admitted = ports.sources.permits_sequence(payload.source.port, payload.source.generation, payload.raw.seq);
        seen[index] = Some((payload.raw.seq, admitted));
        index += 1;
        ActionResult::Event(if admitted { EventOutcome::Queued } else { EventOutcome::RevokedSource })
    });
    assert_eq!(seen, [Some((0, false)), Some((1, true))]);
    worker.step(); // intentional cancellation of an old release must not close the port
    assert!(main_port.admitted(), "Stop cancels old raw offers without revoking future live input");
}

#[test]
fn a_refused_release_closes_only_its_source_generation() {
    let (control, mut plans) = bridge();
    let (mut registry, sources) = sources::pair(&control).unwrap();
    let token = registry.register(1).unwrap();
    let other = registry.register(2).unwrap();
    let (lane, mut audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Refused release");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    send.send(input(consumer, registry.permit(token).unwrap())).ok().unwrap();
    fake.input_port(&key).unwrap().deliver(0, &[0x80, 60, 0]);
    worker.step();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _| ActionResult::Event(EventOutcome::StaleBinding));
    worker.step();
    assert!(!sources.permits(1, token.generation()));
    assert!(sources.permits(2, other.generation()));
}

#[test]
fn destroying_a_delayed_worker_cannot_close_a_reopened_port() {
    let (control, mut plans) = bridge();
    let (mut registry, mut sources) = sources::pair(&control).unwrap();
    let old = registry.register(5).unwrap();
    let (lane, _audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Reopen");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    send.send(input(consumer, registry.permit(old).unwrap())).ok().unwrap();
    worker.step();
    let revoke = registry.revoke(old).unwrap();
    sources.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| Ok(()));
    registry.release(revoke).unwrap();
    let new = registry.register(5).unwrap();
    assert_ne!(old.generation(), new.generation());
    drop(worker); // its stale permit closes only old.generation()
    assert!(registry.admits(new));
    assert!(sources.permits(5, new.generation()));
}

#[test]
fn native_worker_ingests_and_shutdown_invalidates_while_main_does_not_service() {
    let (control, mut plans) = bridge();
    let (mut registry, audio_sources) = sources::pair(&control).unwrap();
    let token = registry.register(0).unwrap();
    let (lane, mut audio) = action_lane::pair_with_result(&control);
    let mut worker = Worker::start(lane).unwrap();
    let fake = Fake::default();
    let key = fake.add_input("Thread");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    let _port = worker.attach(consumer, registry.permit(token).unwrap(), route()).unwrap();
    fake.input_port(&key).unwrap().deliver(55_000, &[0x90, 60, 100]);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut observed = None;
    while observed.is_none() && std::time::Instant::now() < deadline {
        audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |event, _| {
            if let action_lane::Event::Action { payload, .. } = event {
                observed = Some(payload);
            }
            ActionResult::Event(EventOutcome::Queued)
        });
        std::thread::yield_now();
    }
    assert_eq!(observed.unwrap().raw.backend_time_us, 55_000);
    drop(worker);
    assert!(!audio_sources.permits(0, token.generation()), "shutdown closes admission synchronously without joining");
}

#[test]
fn one_raw_consumer_fans_out_with_independent_channels_and_identical_timestamps() {
    let (control, mut plans) = bridge();
    let (mut registry, sources) = sources::pair(&control).unwrap();
    let first = registry.register(0).unwrap();
    let second = registry.register(1).unwrap();
    let (lane, mut audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Fanout");
    let (_handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    let a = route();
    let b = Route { target: (TrackId(8), soundcraft_mix::INSTRUMENT_SLOT, SlotGen::VACANT), channels: 1 << 1, ..a };
    send.send(Update::Input(Input { id: 1, consumer, routes: vec![(registry.permit(first).unwrap(), a), (registry.permit(second).unwrap(), b)] }))
        .ok()
        .unwrap();
    fake.input_port(&key).unwrap().deliver(77_000, &[0x90, 60, 100, 0x91, 61, 100, 0x92, 62, 100]);
    worker.step();
    let mut packets = [None; 4];
    let mut count = 0;
    let (_, heap) = measure(|| {
        audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |event, _| {
            let action_lane::Event::Action { payload, .. } = event else { panic!("unexpected recovery") };
            assert!(sources.permits_sequence(payload.source.port, payload.source.generation, payload.raw.seq));
            packets[count] = Some(payload);
            count += 1;
            ActionResult::MidiBuffered
        });
    });
    assert!(heap.is_zero(), "{heap:?}");
    assert_eq!(count, 4);
    let packets = packets.map(Option::unwrap);
    assert_eq!(
        packets.map(|p| (p.target.0, p.source.port, p.source.channel, p.raw.seq)),
        [(TrackId(7), 0, 0, 0), (TrackId(7), 0, 1, 1), (TrackId(8), 1, 1, 1), (TrackId(7), 0, 2, 2),]
    );
    assert_eq!(packets[1].raw, packets[2].raw);
    assert!(packets.iter().all(|packet| packet.raw.backend_time_us == 77_000));
    worker.step();
    assert_eq!(worker.lane.note_credits(), action_lane::NOTE_CAPACITY);
}

#[test]
fn reconfiguration_retires_only_removed_routes_and_new_routes_skip_old_raw_offers() {
    let (control, mut plans) = bridge();
    let (mut registry, sources) = sources::pair(&control).unwrap();
    let removed = registry.register(0).unwrap();
    let retained = registry.register(1).unwrap();
    let replacement = registry.register(2).unwrap();
    let (lane, mut audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Reconfigure pending raw input");
    let (handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    let target = route();
    send.send(Update::Input(Input {
        id: 1,
        consumer,
        routes: vec![(registry.permit(removed).unwrap(), target), (registry.permit(retained).unwrap(), target)],
    }))
    .ok()
    .unwrap();
    worker.step();
    let port = fake.input_port(&key).unwrap();
    port.deliver(1_000, &[0x80, 60, 0]); // offered before the new route, still in the raw ring
    registry.cancel_before(replacement, handle.boundary().cutoff()).unwrap();
    let next_target = Route { target: (TrackId(9), soundcraft_mix::INSTRUMENT_SLOT, SlotGen::VACANT), ..target };
    let applied = Arc::new(AtomicBool::new(false));
    send.send(Update::Routes {
        id: 1,
        routes: vec![(registry.permit(retained).unwrap(), target), (registry.permit(replacement).unwrap(), next_target)],
        applied: Arc::clone(&applied),
    })
    .ok()
    .unwrap();
    assert!(!applied.load(Ordering::Acquire), "enqueuing a route is not applying it");
    port.deliver(2_000, &[0x90, 64, 100]);
    worker.step();
    assert!(applied.load(Ordering::Acquire));
    assert!(!sources.permits(removed.port(), removed.generation()));
    assert!(sources.permits(retained.port(), retained.generation()));
    assert!(sources.permits(replacement.port(), replacement.generation()));
    let mut observed = Vec::new();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |event, _| {
        let action_lane::Event::Action { payload, .. } = event else { panic!("unexpected recovery") };
        observed.push((payload.target.0, payload.source.port, payload.raw.seq));
        ActionResult::MidiBuffered
    });
    assert_eq!(observed, [(TrackId(7), 1, 0), (TrackId(7), 1, 1), (TrackId(9), 2, 1)]);
    worker.step();
    assert!(registry.admits(retained));
    assert!(registry.admits(replacement));
    assert_eq!(handle.boundary().cutoff(), 2);
}

#[test]
fn pending_initial_route_preserves_post_boundary_input_until_acknowledged() {
    let (control, mut plans) = bridge();
    let (mut registry, _sources) = sources::pair(&control).unwrap();
    let token = registry.register(0).unwrap();
    let (lane, mut audio) = action_lane::pair_with_result(&control);
    let (send, mut worker) = pump(lane);
    let fake = Fake::default();
    let key = fake.add_input("Initial route pending");
    let (handle, consumer) = fake.io().open_input(&key, InputConfig::default()).unwrap();
    send.send(Update::Input(Input { id: 1, consumer, routes: Vec::new() })).ok().unwrap();
    worker.step();
    let port = fake.input_port(&key).unwrap();
    port.deliver(1_000, &[0x90, 60, 100]);
    registry.cancel_before(token, handle.boundary().cutoff()).unwrap();
    port.deliver(2_000, &[0x90, 64, 100]);
    worker.step(); // native input is attached, but its routing update has not arrived
    assert_eq!(worker.shared.offered.load(Ordering::Relaxed), 0, "pending routing must not discard post-cutoff input");
    let applied = Arc::new(AtomicBool::new(false));
    send.send(Update::Routes { id: 1, routes: vec![(registry.permit(token).unwrap(), route())], applied: Arc::clone(&applied) }).ok().unwrap();
    assert!(!applied.load(Ordering::Acquire));
    worker.step();
    assert!(applied.load(Ordering::Acquire));
    let mut observed = Vec::new();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |event, _| {
        let action_lane::Event::Action { payload, .. } = event else { panic!("unexpected recovery") };
        observed.push((payload.raw.seq, payload.raw.backend_time_us));
        ActionResult::MidiBuffered
    });
    assert_eq!(observed, [(1, 2_000)]);
    assert_eq!(worker.shared.cancelled.load(Ordering::Relaxed), 1);
}
