//! Allocation probe for instrument events (F10, ADR 0012 G5): on a realtime engine, admitting
//! events at saturation (`queue_event` until every queue, note and pedal limit refuses),
//! delivering them (`render_active`), a transport change, a source revocation and a panic
//! allocate and free nothing on the audio thread.
//!
//! The binary installs `soundcraft_rt::testing::CountingAlloc` and counts the test thread only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_mix::{
    Adoption, Arrival, DeviceGeneration, Due, EngineEpoch, EventLimits, EventOutcome, INSTRUMENT_SLOT, InstanceSpec, InstrumentEvent, Message,
    MixEngine, NoteId, PlanCompiler, PlanGeneration, PlanLimits, RenderFrame, RenderTarget, SlotGen, Source, TransportEpoch, mark_audio_thread,
};
use soundcraft_model::{ChannelFormat, Insert, Session, TrackKind};
use soundcraft_rt::testing::{AllocStats, CountingAlloc, measure};
use std::sync::Arc;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;

#[test]
fn dense_clip_sorting_and_live_pedal_merge_allocate_nothing() {
    MixEngine::force_serial(true);
    mark_audio_thread();
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32 };
    let mut mix = MixEngine::realtime_with_events(target, EngineEpoch::FIRST, EventLimits::default());
    let mut comp = PlanCompiler::new(target, PlanLimits::default());
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    s.track_mut(t).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    let mut seq = soundcraft_midi::Sequence::default();
    for start in [8, 1, 4] {
        seq.notes.push(soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 0, channel: 0, start, length: 1 });
    }
    for tick in [5, 2] {
        for _ in 0..300 {
            seq.ctrls.push(soundcraft_midi::CtrlEvent { tick, channel: 0, kind: soundcraft_midi::CtrlKind::Cc(1), value: 1 });
        }
    }
    for (tick, value) in [(8, 63), (4, 127)] {
        seq.ctrls.push(soundcraft_midi::CtrlEvent { tick, channel: 0, kind: soundcraft_midi::CtrlKind::Cc(64), value });
    }
    let cid = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(soundcraft_model::Clip::midi(cid, "dense", 512, 48_000, seq));
    let mut make = |spec: &InstanceSpec, _: SlotGen| soundcraft_mix::create_instance(&spec.id, None, SR as f32, BLOCK, spec.ch);
    let mut env = comp.compile(Arc::new(s), PlanGeneration::EMPTY, PlanGeneration::EMPTY.checked_next().unwrap(), &mut make).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    comp.reclaim(env);
    let binding = comp.active_bindings().into_iter().find(|b| b.1 == INSTRUMENT_SLOT).unwrap();
    let event = |at, value| InstrumentEvent {
        engine: EngineEpoch::FIRST,
        target: binding,
        due: Due::Frame(RenderFrame(at)),
        transport: None,
        source: Source { port: 1, channel: 0, generation: 1 },
        message: Message::Control { cc: 64, value },
        arrival: Arrival { host_ns: 0, sequence: 0 },
    };
    let mut out = vec![vec![0.0; BLOCK]; 2];
    mix.render_active(0, BLOCK, &mut out);
    assert_eq!(mix.queue_event(&event(256, 127)), EventOutcome::Queued);
    mix.render_active(256, BLOCK, &mut out);
    let ((), stats) = measure(|| {
        assert_eq!(mix.queue_event(&event(522, 63)), EventOutcome::Queued);
        mix.render_active(512, BLOCK, &mut out);
        mix.render_active(768, BLOCK, &mut out);
    });
    assert_eq!(stats, AllocStats::default(), "dense clip sorting and mixed-source delivery");
    assert!(!mix.has_pending_events());
}

#[test]
fn admission_delivery_revocation_and_panic_allocate_nothing() {
    MixEngine::force_serial(true);
    mark_audio_thread();
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32 };
    let epoch = EngineEpoch::FIRST;
    let mut mix = MixEngine::realtime_with_events(target, epoch, EventLimits::default());
    let mut comp = PlanCompiler::new(target, PlanLimits::default());
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    s.track_mut(t).unwrap().instrument = Some(Insert::new("subtractive_synth"));
    let mut make = |spec: &InstanceSpec, _: SlotGen| soundcraft_mix::create_instance(&spec.id, None, SR as f32, BLOCK, spec.ch);
    let mut env = comp.compile(Arc::new(s), PlanGeneration::EMPTY, PlanGeneration::EMPTY.checked_next().unwrap(), &mut make).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    comp.reclaim(env);
    let target_binding = comp.active_bindings().into_iter().find(|b| b.1 == INSTRUMENT_SLOT).unwrap();
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let mut pos = 0i64;
    // Warm up: the first blocks (controller chase), not measured.
    for _ in 0..4 {
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
    }
    mix.set_transport_epoch(TransportEpoch(1));

    let ev = |source: Source, at: u64, message: Message, transport: Option<TransportEpoch>| InstrumentEvent {
        engine: epoch,
        target: target_binding,
        due: Due::Frame(RenderFrame(at)),
        transport,
        source,
        message,
        arrival: Arrival { host_ns: 0, sequence: 0 },
    };
    let mut outcomes = [0usize; 4];
    let ((), stats) = measure(|| {
        let now = mix.next_frame().0;
        for round in 0..2u64 {
            for p in 0..40u16 {
                let src = Source { port: p, channel: (p % 16) as u8, generation: 1 };
                for cc in [64, 66, 67, 69] {
                    let o = mix.queue_event(&ev(src, now + round, Message::Control { cc, value: 127 }, None));
                    outcomes[usize::from(o.accepted())] += 1;
                }
                for k in 0..20u8 {
                    let id = NoteId(u32::from(k) + u32::from(p) * 100);
                    let o = mix.queue_event(&ev(src, now + u64::from(k), Message::NoteOn { key: k, velocity: 90, id }, None));
                    outcomes[usize::from(o.accepted())] += 1;
                    let o = mix.queue_event(&ev(src, now + 4000, Message::NoteOff { key: k, velocity: 0, id }, None));
                    outcomes[2 + usize::from(o == EventOutcome::Queued)] += 1;
                }
                for v in 0..10u16 {
                    mix.queue_event(&ev(src, now + 7, Message::PitchBend { value: v * 1000 }, Some(TransportEpoch(1))));
                }
            }
        }
        for _ in 0..4 {
            mix.render_active(pos, BLOCK, &mut out);
            pos += BLOCK as i64;
        }
        mix.set_transport_epoch(TransportEpoch(2));
        for p in 0..20u16 {
            mix.revoke_source(p, 1);
        }
        for _ in 0..4 {
            mix.render_active(pos, BLOCK, &mut out);
            pos += BLOCK as i64;
        }
        mix.all_notes_off();
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        // Half-pedal values, re-strikes, and releases carrying the old transport (review fixes).
        for p in 20..40u16 {
            let src = Source { port: p, channel: (p % 16) as u8, generation: 1 };
            let now = mix.next_frame().0;
            for (k, v) in [90u8, 70, 30].into_iter().enumerate() {
                mix.queue_event(&ev(src, now + k as u64, Message::Control { cc: 64, value: v }, None));
            }
            let id = NoteId(u32::from(p) * 100);
            mix.queue_event(&ev(src, now + 3, Message::NoteOn { key: 0, velocity: 90, id }, Some(TransportEpoch(2))));
            mix.queue_event(&ev(src, now + 4, Message::NoteOn { key: 0, velocity: 90, id }, Some(TransportEpoch(2))));
        }
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        mix.set_transport_epoch(TransportEpoch(3));
        for p in 20..40u16 {
            let src = Source { port: p, channel: (p % 16) as u8, generation: 1 };
            let id = NoteId(u32::from(p) * 100);
            let now = mix.next_frame().0;
            mix.queue_event(&ev(src, now, Message::NoteOff { key: 0, velocity: 0, id }, Some(TransportEpoch(2))));
            mix.queue_event(&ev(src, now, Message::Control { cc: 121, value: 0 }, Some(TransportEpoch(2))));
        }
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        // A live pedal survives a seek, then its nonzero up survives another epoch change.
        let src = Source { port: 49, channel: 0, generation: 1 };
        let now = mix.next_frame().0;
        assert_eq!(mix.queue_event(&ev(src, now, Message::Control { cc: 64, value: 100 }, None)), EventOutcome::Queued);
        mix.render_active(pos, BLOCK, &mut out);
        pos += 10_000;
        mix.set_transport_epoch(TransportEpoch(4));
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        let now = mix.next_frame().0;
        for k in 0..256 {
            assert_eq!(mix.queue_event(&ev(src, now + 1000 + k, Message::Control { cc: 1, value: 1 }, None)), EventOutcome::Queued);
        }
        assert_eq!(mix.queue_event(&ev(src, now + 20, Message::Control { cc: 64, value: 63 }, Some(TransportEpoch(4)))), EventOutcome::Queued);
        mix.set_transport_epoch(TransportEpoch(5));
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        mix.revoke_source(49, 1);
        // Stop: release everything, then reset the plug-ins.
        let now = mix.next_frame().0;
        let src = Source { port: 50, channel: 0, generation: 1 };
        mix.queue_event(&ev(src, now, Message::NoteOn { key: 60, velocity: 90, id: NoteId::NONE }, None));
        mix.queue_event(&ev(src, now, Message::Control { cc: 64, value: 127 }, None));
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        mix.reset();
        mix.render_active(pos, BLOCK, &mut out);
        pos += BLOCK as i64;
        let _ = mix.event_status();
    });
    let st = mix.event_status();
    println!("{stats:?}; refused/accepted {outcomes:?}; {st:?}");
    assert_eq!(stats, AllocStats::default(), "on the audio thread");
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "saturated: some events were refused");
    assert_eq!(st.dropped_releases, 0);
    assert!(st.shed.note_on > 0 && st.shed.pedal_down > 0);
    assert!(st.revoked > 0);
    assert!(st.restrikes > 0);

    // A track made inactive: adoption voids its instrument's queue (prepared off the measure).
    let src = Source { port: 51, channel: 0, generation: 1 };
    let now = mix.next_frame().0;
    mix.queue_event(&ev(src, now + 5000, Message::NoteOn { key: 61, velocity: 90, id: NoteId::NONE }, None));
    let mut s = (**mix.session()).clone();
    s.track_mut(t).unwrap().inactive = true;
    let mut make = |spec: &InstanceSpec, _: SlotGen| soundcraft_mix::create_instance(&spec.id, None, SR as f32, BLOCK, spec.ch);
    let mut env = comp.compile(Arc::new(s), comp.active(), PlanGeneration::EMPTY.checked_next().unwrap().checked_next().unwrap(), &mut make).unwrap();
    let (adopted, stats) = measure(|| {
        let a = mix.adopt_plan(&mut env);
        mix.render_active(pos, BLOCK, &mut out);
        a
    });
    assert_eq!(adopted, Adoption::Adopted);
    assert_eq!(stats, AllocStats::default(), "adoption voiding a queue, on the audio thread");
    assert!(!mix.has_pending_events());
    assert_eq!(mix.event_status().voided[0], 1);
    comp.reclaim(env);
}
