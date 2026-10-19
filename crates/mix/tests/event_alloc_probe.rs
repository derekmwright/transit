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
                for cc in [64, 66, 69] {
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
        let _ = mix.event_status();
    });
    let st = mix.event_status();
    println!("{stats:?}; refused/accepted {outcomes:?}; {st:?}");
    assert_eq!(stats, AllocStats::default(), "on the audio thread");
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "saturated: some events were refused");
    assert_eq!(st.dropped_releases, 0);
    assert!(st.shed.note_on > 0 && st.shed.pedal_down > 0);
    assert!(st.revoked > 0);
}
