//! F10 (ADR 0012 G5), mix side: sample-accurate, identity-carrying instrument events through
//! [`MixEngine::queue_event`] on a realtime engine fed the way playback feeds it. A recording
//! instrument writes down every event it receives with its absolute render frame.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_mix::{
    Adoption, Arrival, DeviceGeneration, Due, EngineEpoch, EventLimits, EventOutcome, INSTRUMENT_SLOT, InstanceSpec, InstrumentEvent, MAX_OPEN,
    MAX_PEDALS, MAX_PORTS, Message, MixEngine, NewInstance, NoteId, PlanCompiler, PlanGeneration, PlanLimits, PluginEvent, RenderFrame, RenderTarget,
    Shed, SlotBinding, SlotGen, Source, TransportEpoch,
};
use soundcraft_model::{ChannelFormat, Insert, Session, TrackId, TrackKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const SR: u32 = 48_000;
const BLOCK: usize = 256;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(3), sample_rate: SR, max_block: BLOCK as u32 }
}

/// What a recording instrument received: the instance number, the absolute frame (its block's
/// first frame plus the offset) and the event. `None` marks an `all_notes_off`.
type Heard = Arc<Mutex<Vec<(usize, u64, Option<PluginEvent>)>>>;

/// An instrument that records every event at its absolute render frame.
struct Recorder {
    inner: Box<dyn Plugin>,
    heard: Heard,
    instance: usize,
    pending: Vec<(u32, Option<PluginEvent>)>,
    rendered: u64,
    /// While set, the instrument renders NaN (the mix takes it out as failed).
    fail: Arc<AtomicBool>,
}

impl Plugin for Recorder {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
        self.inner.prepare(sr, mb, ch);
    }
    /// As real hosts do: pending non-releases are dropped, releases (and `all_notes_off`) are
    /// kept for the next block, so a mix that relies on a reset to drop a release is caught.
    fn reset(&mut self) {
        self.pending.retain(|(_, e)| e.is_none_or(|e| e.message.survives_reset()));
    }
    fn set_param(&mut self, id: &str, v: f32) -> bool {
        self.inner.set_param(id, v)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let mut heard = self.heard.lock().unwrap();
        // Stable by offset, as hosts sort.
        self.pending.sort_by_key(|(o, _)| *o);
        for (o, e) in self.pending.drain(..) {
            heard.push((self.instance, self.rendered + u64::from(o), e));
        }
        self.rendered += frames as u64;
        let y = if self.fail.load(Ordering::Relaxed) { f32::NAN } else { 0.0 };
        for c in io.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x = y);
        }
    }
    fn event(&mut self, offset: u32, ev: &PluginEvent) -> bool {
        self.pending.push((offset, Some(*ev)));
        true
    }
    fn all_notes_off(&mut self) {
        self.pending.push((0, None));
    }
}

struct Rig {
    mix: MixEngine,
    comp: PlanCompiler,
    heard: Heard,
    made: Arc<Mutex<usize>>,
    generation: PlanGeneration,
    pos: i64,
    out: Vec<Vec<f32>>,
    epoch: EngineEpoch,
    track: TrackId,
    session: Session,
    fail: Arc<AtomicBool>,
}

fn instrument_session(id: &str) -> (Session, TrackId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    s.track_mut(t).unwrap().instrument = Some(Insert::new(id));
    (s, t)
}

impl Rig {
    fn new() -> Rig {
        Rig::with_epoch(EngineEpoch::FIRST)
    }

    fn with_epoch(epoch: EngineEpoch) -> Rig {
        let (session, track) = instrument_session("clap:test.rec");
        let mut rig = Rig {
            mix: MixEngine::realtime_with_events(target(), epoch, EventLimits::default()),
            comp: PlanCompiler::new(target(), PlanLimits::default()),
            heard: Arc::default(),
            made: Arc::default(),
            generation: PlanGeneration::EMPTY,
            pos: 0,
            out: vec![vec![0.0; BLOCK]; 2],
            epoch,
            track,
            session,
            fail: Arc::default(),
        };
        let s = rig.session.clone();
        rig.publish(&s);
        // The first block chases the controllers (reset all controllers): not under test.
        rig.render(1);
        rig.heard.lock().unwrap().clear();
        rig
    }

    fn publish(&mut self, s: &Session) {
        self.generation = self.generation.checked_next().unwrap();
        let (heard, made, fail) = (Arc::clone(&self.heard), Arc::clone(&self.made), Arc::clone(&self.fail));
        let mut make = move |spec: &InstanceSpec, _: SlotGen| -> Option<NewInstance> {
            let mut inner = soundcraft_dsp::create("gain")?;
            inner.prepare(SR as f32, BLOCK, spec.ch);
            let mut n = made.lock().unwrap();
            *n += 1;
            let plugin = Box::new(Recorder {
                inner,
                heard: Arc::clone(&heard),
                instance: *n,
                pending: Vec::with_capacity(4096),
                rendered: 0,
                fail: Arc::clone(&fail),
            });
            Some(NewInstance { plugin, refused_state: false })
        };
        let mut env = self.comp.compile(Arc::new(s.clone()), self.comp.active(), self.generation, &mut make).unwrap();
        assert_eq!(self.mix.adopt_plan(&mut env), Adoption::Adopted);
        self.comp.reclaim(env);
    }

    fn binding(&self) -> SlotBinding {
        self.comp.active_bindings().into_iter().find(|b| b.0 == self.track && b.1 == INSTRUMENT_SLOT).unwrap()
    }

    fn render(&mut self, blocks: usize) {
        for _ in 0..blocks {
            self.mix.render_active(self.pos, BLOCK, &mut self.out);
            self.pos += BLOCK as i64;
        }
    }

    fn event(&self, source: Source, due: Due, message: Message) -> InstrumentEvent {
        InstrumentEvent {
            engine: self.epoch,
            target: self.binding(),
            due,
            transport: None,
            source,
            message,
            arrival: Arrival { host_ns: 0, sequence: 0 },
        }
    }

    fn send(&mut self, source: Source, at: u64, message: Message) -> EventOutcome {
        let ev = self.event(source, Due::Frame(RenderFrame(at)), message);
        self.mix.queue_event(&ev)
    }

    /// Every event instance `instance` heard: (frame, event).
    fn heard_by(&self, instance: usize) -> Vec<(u64, PluginEvent)> {
        self.heard.lock().unwrap().iter().filter(|(i, _, _)| *i == instance).filter_map(|(_, f, e)| e.map(|e| (*f, e))).collect()
    }
}

fn port(port: u16, channel: u8) -> Source {
    Source { port, channel, generation: 1 }
}

fn on(key: u8) -> Message {
    Message::NoteOn { key, velocity: 100, id: NoteId::NONE }
}

fn on_id(key: u8, id: u32) -> Message {
    Message::NoteOn { key, velocity: 100, id: NoteId(id) }
}

fn off(key: u8) -> Message {
    Message::NoteOff { key, velocity: 0, id: NoteId::NONE }
}

fn off_id(key: u8, id: u32) -> Message {
    Message::NoteOff { key, velocity: 0, id: NoteId(id) }
}

fn cc(cc: u8, value: u8) -> Message {
    Message::Control { cc, value }
}

/// Checks that every voice the instrument heard start was released after it, and returns the
/// number of note-ons.
fn every_on_has_its_off(heard: &[(u64, PluginEvent)]) -> usize {
    let mut open = std::collections::HashMap::new();
    let mut ons = 0;
    for (f, e) in heard {
        match e.message {
            Message::NoteOn { .. } => {
                assert!(open.insert(e.note_id, *f).is_none(), "voice id {} reused while it sounds", e.note_id);
                ons += 1;
            }
            Message::NoteOff { .. } => {
                let start = open.remove(&e.note_id).unwrap_or_else(|| panic!("off for voice {} that is not sounding", e.note_id));
                assert!(*f >= start, "voice {} released at {f} before its on at {start}", e.note_id);
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "voices never released: {open:?}");
    ons
}

#[test]
fn identity_reaches_the_plug_in_and_the_tap_sees_each_admission() {
    let mut rig = Rig::new();
    rig.mix.install_event_tap(4096);
    let start = rig.mix.next_frame().0;
    let mut sequence = 0;
    // Three ports × 16 channels, the same key and the same sender id on every one.
    for p in 0..3u16 {
        for ch in 0..16u8 {
            for (k, msg) in [on_id(60, 1), off_id(60, 1)].into_iter().enumerate() {
                sequence += 1;
                let due = start + u64::from(p) * 100 + u64::from(ch) * 3 + k as u64 * 1000;
                let ev = InstrumentEvent {
                    arrival: Arrival { host_ns: sequence * 10, sequence },
                    ..rig.event(Source { port: p, channel: ch, generation: 7 + u64::from(p) }, Due::Frame(RenderFrame(due)), msg)
                };
                assert_eq!(rig.mix.queue_event(&ev), EventOutcome::Queued);
            }
        }
    }
    rig.render(8);
    let tap = rig.mix.event_tap();
    assert_eq!(tap.len(), 96);
    for (i, r) in tap.iter().enumerate() {
        let (p, ch, k) = (i / 32, (i / 2) % 16, i % 2);
        assert_eq!((r.source.port, r.source.channel, r.source.generation), (p as u16, ch as u8, 7 + p as u64));
        assert_eq!(r.arrival, Arrival { host_ns: (i as u64 + 1) * 10, sequence: i as u64 + 1 });
        assert_eq!(r.due, Some(RenderFrame(start + p as u64 * 100 + ch as u64 * 3 + k as u64 * 1000)));
    }
    let heard = rig.heard_by(1);
    assert_eq!(every_on_has_its_off(&heard), 48);
    // Each on arrives on its source's channel at its due frame, with a voice id of its own.
    let ons: Vec<_> = heard.iter().filter(|(_, e)| matches!(e.message, Message::NoteOn { .. })).collect();
    let mut ids: Vec<i32> = ons.iter().map(|(_, e)| e.note_id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 48, "48 concurrent notes on one key, 48 voice ids");
    for (f, e) in &ons {
        let rel = f - start;
        let (p, ch) = (rel / 100, (rel % 100) / 3);
        assert_eq!(u64::from(e.channel), ch, "channel at frame {f}");
        assert!(p < 3);
    }
    let frames: Vec<u64> = heard.iter().map(|(f, _)| *f).collect();
    assert!(frames.windows(2).all(|w| w[0] <= w[1]), "delivered in due order");
}

#[test]
fn a_note_off_survives_an_overfull_queue() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let mut admitted = 0;
    // Controllers that stay queued, then three rounds of note-on/off pairs whose offs are due
    // far ahead (future pairs), one block rendered between rounds. Without the reservation the
    // offs of the first round alone would not fit.
    let now = rig.mix.next_frame().0;
    for i in 0..120u64 {
        assert_eq!(rig.send(src, now + 7000 + i, cc(1, 1)), EventOutcome::Queued);
    }
    for round in 0..3u64 {
        let now = rig.mix.next_frame().0;
        let mut ons = Vec::new();
        for i in 0..300u32 {
            let (key, id) = ((i % 128) as u8, round as u32 * 1000 + i);
            let o = rig.send(src, now + u64::from(i % 50), on_id(key, id));
            assert!(matches!(o, EventOutcome::Queued | EventOutcome::Refused(Shed::QueueFull | Shed::NotesFull)), "{o:?}");
            ons.push((key, id, o == EventOutcome::Queued));
        }
        for (key, id, queued) in ons {
            let o = rig.send(src, now + 8000, off_id(key, id));
            if queued {
                admitted += 1;
                assert_eq!(o, EventOutcome::Queued, "an admitted note's off is always queued");
            } else {
                assert_eq!(o, EventOutcome::Unmatched);
            }
        }
        rig.render(1);
    }
    rig.render(40);
    let st = rig.mix.event_status();
    assert_eq!(st.dropped_releases, 0);
    assert_eq!(st.release_collapses, 0);
    assert_eq!(st.host_refused, [0, 0]);
    assert_eq!(admitted, MAX_OPEN, "the queue, then the table filled");
    let heard = rig.heard_by(1);
    assert_eq!(every_on_has_its_off(&heard), admitted, "every admitted note-on was delivered, and released");
}

#[test]
fn a_replaced_instrument_takes_no_events_meant_for_the_old_one() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let old = rig.binding();
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(src, now, on(60)), EventOutcome::Queued);
    assert_eq!(rig.send(src, now + 5000, on(62)), EventOutcome::Queued, "a future note, still queued at the replacement");
    rig.render(1);
    // Replace the instrument (another plug-in id: a fresh instance).
    let mut s = rig.session.clone();
    s.track_mut(rig.track).unwrap().instrument = Some(Insert::new("clap:test.rec2"));
    rig.publish(&s);
    assert_ne!(rig.binding(), old);
    let ev = InstrumentEvent { target: old, ..rig.event(src, Due::Next, off(60)) };
    assert_eq!(rig.mix.queue_event(&ev), EventOutcome::StaleBinding, "a release for the old binding is refused, never retargeted");
    rig.render(30);
    assert!(rig.heard_by(2).is_empty(), "the new instance heard nothing: {:?}", rig.heard_by(2));
    let st = rig.mix.event_status();
    assert_eq!(st.refused_stale[1], 1);
    assert_eq!(st.retired_with_instance, [1, 0], "the queued note-on retired with its instance");
    assert_eq!(st.affected, Some(old));
}

#[test]
fn a_new_engine_epoch_refuses_an_aliased_binding() {
    let mut first = Rig::with_epoch(EngineEpoch(5));
    let mut second = Rig::with_epoch(EngineEpoch(6));
    assert_eq!(first.binding().2, second.binding().2, "a fresh compiler issues the same SlotGen (aliased)");
    let ev = first.event(port(1, 0), Due::Next, on(60));
    assert_eq!(second.mix.queue_event(&ev), EventOutcome::StaleEngine);
    second.render(4);
    assert!(second.heard.lock().unwrap().is_empty());
    assert_eq!(second.mix.event_status().refused_stale[0], 1);
    assert_eq!(first.mix.queue_event(&ev), EventOutcome::Queued);
}

#[test]
fn offline_and_legacy_engines_refuse_every_event() {
    let rig = Rig::new();
    let ev = rig.event(port(1, 0), Due::Next, on(60));
    for mut eng in [MixEngine::new(48_000.0, BLOCK), MixEngine::realtime(target())] {
        assert_eq!(eng.queue_event(&ev), EventOutcome::StaleEngine);
        assert_eq!(eng.queue_event(&InstrumentEvent { message: off(60), ..ev }), EventOutcome::StaleEngine);
    }
}

#[test]
fn a_transport_change_refuses_and_cancels_clip_derived_events() {
    let mut rig = Rig::new();
    let (t1, t2) = (TransportEpoch(1), TransportEpoch(2));
    rig.mix.set_transport_epoch(t1);
    let now = rig.mix.next_frame().0;
    let clip = |rig: &Rig, at: u64, m: Message, t| InstrumentEvent {
        transport: Some(t),
        ..rig.event(port(Source::CLIP, 0), Due::Frame(RenderFrame(at)), m)
    };
    assert_eq!(rig.mix.queue_event(&clip(&rig, now + 1000, on(60), t1)), EventOutcome::Queued);
    assert_eq!(rig.mix.queue_event(&clip(&rig, now + 2000, off(60), t1)), EventOutcome::Queued);
    // A live note keeps going through the change.
    assert_eq!(rig.send(port(1, 0), now + 10, on(64)), EventOutcome::Queued);
    rig.mix.set_transport_epoch(t2);
    assert_eq!(rig.mix.queue_event(&clip(&rig, now + 1000, on(61), t1)), EventOutcome::StaleTransport);
    rig.render(16);
    let heard = rig.heard_by(1);
    assert_eq!(heard.len(), 1, "only the live note: {heard:?}");
    assert!(matches!(heard[0].1.message, Message::NoteOn { key: 64, .. }));
    let st = rig.mix.event_status();
    assert_eq!(st.cancelled_transport, 1);
    assert_eq!(st.refused_stale[2], 1);
}

/// The plug-in's view of pedal `cc` on channel 0: (frame, down).
fn pedal_edges(heard: &[(u64, PluginEvent)], pedal: u8) -> Vec<(u64, bool)> {
    heard
        .iter()
        .filter_map(|(f, e)| match e.message {
            Message::Control { cc, value } if cc == pedal => Some((*f, value >= 64)),
            _ => None,
        })
        .collect()
}

/// The plug-in's view of pedal `cc`: (frame, value).
fn pedal_values(heard: &[(u64, PluginEvent)], pedal: u8) -> Vec<(u64, u8)> {
    heard
        .iter()
        .filter_map(|(f, e)| match e.message {
            Message::Control { cc, value } if cc == pedal => Some((*f, value)),
            _ => None,
        })
        .collect()
}

#[test]
fn pedals_from_two_ports_are_ored_and_a_revocation_releases_the_last_holder() {
    let mut rig = Rig::new();
    let (a, b) = (port(1, 0), port(2, 0));
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(a, now, cc(64, 127)), EventOutcome::Queued);
    assert_eq!(rig.send(b, now + 10, cc(64, 100)), EventOutcome::Queued);
    assert_eq!(rig.send(a, now + 20, cc(64, 0)), EventOutcome::Queued);
    rig.render(1);
    assert_eq!(pedal_values(&rig.heard_by(1), 64), vec![(now, 127), (now + 20, 100)], "B still holds the pedal: A's up leaves B's value");
    rig.mix.revoke_source(2, 1);
    rig.render(1);
    assert_eq!(pedal_values(&rig.heard_by(1), 64), vec![(now, 127), (now + 20, 100), (now + BLOCK as u64, 0)], "B's revocation lifts it");
    let st = rig.mix.event_status();
    assert_eq!(st.absorbed_pedal_edges, 1, "B's down, below A's value");
    assert!(st.revoked >= 1);
}

#[test]
fn future_pedals_follow_due_order() {
    let mut rig = Rig::new();
    let (a, b) = (port(1, 0), port(2, 0));
    let now = rig.mix.next_frame().0;
    for (src, at, v) in [(a, 0, 127), (a, 100, 0), (b, 200, 127), (b, 300, 0)] {
        assert_eq!(rig.send(src, now + at, cc(66, v)), EventOutcome::Queued);
    }
    rig.render(2);
    assert_eq!(pedal_edges(&rig.heard_by(1), 66), vec![(now, true), (now + 100, false), (now + 200, true), (now + 300, false)]);
}

#[test]
fn the_pedal_limit_refuses_untracked_downs_and_resets_release_everything_at_a_full_queue() {
    let mut rig = Rig::new();
    let now = rig.mix.next_frame().0;
    // 32 pedal entries (sources × channels × kinds), then the 33rd.
    let mut n = 0;
    'fill: for p in 10..30u16 {
        for pedal in [64, 66, 69] {
            if n == MAX_PEDALS {
                break 'fill;
            }
            assert_eq!(rig.send(port(p, 3), now, cc(pedal, 127)), EventOutcome::Queued);
            n += 1;
        }
    }
    assert_eq!(rig.send(port(30, 3), now, cc(64, 127)), EventOutcome::Refused(Shed::PedalsFull));
    rig.render(1);
    let ch3: usize = rig.heard_by(1).iter().filter(|(_, e)| e.channel == 3).count();
    assert_eq!(ch3, 3, "one down per pedal kind reached the plug-in; the 33rd was never delivered");
    rig.mix.revoke_source(30, 1);
    for p in 10..30u16 {
        rig.mix.revoke_source(p, 1);
    }
    rig.render(1);

    // One source holds three pedals and notes on one channel; the queue is full of future
    // controllers; then CC 121 and CC 123.
    let src = port(40, 0);
    let now = rig.mix.next_frame().0;
    for pedal in [64, 66, 69] {
        assert_eq!(rig.send(src, now, cc(pedal, 127)), EventOutcome::Queued);
    }
    for k in 0..20 {
        assert_eq!(rig.send(src, now, on(k)), EventOutcome::Queued);
    }
    let mut filled = 0;
    while rig.send(src, now + 5000 + filled, cc(1, 1)) == EventOutcome::Queued {
        filled += 1;
    }
    assert!(filled > 200, "the non-release budget is taken: {filled}");
    assert_eq!(rig.send(src, now + 1, cc(121, 0)), EventOutcome::Queued);
    assert_eq!(rig.send(src, now + 2, cc(123, 0)), EventOutcome::Queued);
    rig.render(30);
    let heard = rig.heard_by(1);
    let mine: Vec<_> = heard.iter().filter(|(f, e)| *f >= now && e.channel == 0).copied().collect();
    assert_eq!(every_on_has_its_off(&mine), 20, "CC 123 released every note");
    for pedal in [64, 66, 69] {
        assert_eq!(pedal_edges(&mine, pedal), vec![(now, true), (now + 1, false)], "CC 121 lifted pedal {pedal}");
    }
    assert_eq!(rig.mix.event_status().dropped_releases, 0);
}

#[test]
fn duplicate_and_early_releases() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let now = rig.mix.next_frame().0;
    let limit = u64::from(EventLimits::default().max_future_frames);
    assert_eq!(rig.send(src, now + limit, on(60)), EventOutcome::Queued, "at the horizon");
    assert_eq!(rig.send(src, now + limit + 500, off(60)), EventOutcome::ReleaseAdvanced { frames: 500 });
    assert_eq!(rig.send(src, now + limit + 600, off(60)), EventOutcome::Coalesced, "a duplicate takes no slot");
    assert_eq!(rig.send(src, now, off(61)), EventOutcome::Unmatched);
    rig.render(40);
    let heard = rig.heard_by(1);
    assert_eq!(every_on_has_its_off(&heard), 1);
    assert_eq!(heard.len(), 2);
    assert_eq!(heard[0].0, now + limit);
    assert_eq!(heard[1].0, now + limit, "advanced to the horizon, never before its on");
    let st = rig.mix.event_status();
    assert_eq!((st.releases_advanced, st.coalesced, st.unmatched), (1, 1, 1));
}

#[test]
fn a_release_due_before_its_note_on_is_ordered_after_it() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(src, now + 100, on(60)), EventOutcome::Queued);
    assert_eq!(rig.send(src, now + 50, off(60)), EventOutcome::Queued);
    rig.render(1);
    let heard = rig.heard_by(1);
    assert_eq!(heard.len(), 2);
    assert!(matches!(heard[0].1.message, Message::NoteOn { .. }));
    assert_eq!(heard[1].0, now + 100);
    assert_eq!(rig.mix.event_status().reordered, 1);
}

#[test]
fn the_horizon_refuses_early_notes_and_late_ones_play_at_offset_0() {
    let mut rig = Rig::new();
    rig.render(4);
    let src = port(1, 0);
    let now = rig.mix.next_frame().0;
    let limit = u64::from(EventLimits::default().max_future_frames);
    assert_eq!(rig.send(src, now + limit + 1, on(60)), EventOutcome::TooEarly);
    assert_eq!(rig.send(src, now - 300, on(61)), EventOutcome::QueuedLate { frames: 300 });
    assert_eq!(rig.mix.queue_event(&rig.event(src, Due::Next, on(62))), EventOutcome::Queued);
    rig.render(1);
    let heard = rig.heard_by(1);
    assert_eq!(heard.iter().map(|(f, _)| *f).collect::<Vec<_>>(), vec![now, now]);
    let st = rig.mix.event_status();
    assert_eq!((st.refused_too_early, st.late, st.max_late_frames), (1, 1, 300));
    assert_eq!(st.lateness_log2[9], 1, "256 <= 300 < 512");
}

#[test]
fn a_re_strike_releases_the_old_voice_first() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let now = rig.mix.next_frame().0;
    rig.send(src, now, on(60));
    rig.send(src, now + 10, on(60));
    rig.send(src, now + 20, off(60));
    rig.render(1);
    let heard = rig.heard_by(1);
    assert_eq!(every_on_has_its_off(&heard), 2);
    let kinds: Vec<(u64, bool)> = heard.iter().map(|(f, e)| (*f - now, matches!(e.message, Message::NoteOn { .. }))).collect();
    assert_eq!(kinds, vec![(0, true), (10, false), (10, true), (20, false)]);
    assert_eq!(rig.mix.event_status().restrikes, 1);
}

#[test]
fn a_revocation_cancels_queued_events_and_releases_sounding_notes() {
    let mut rig = Rig::new();
    let (a, b) = (port(1, 0), port(2, 1));
    let now = rig.mix.next_frame().0;
    rig.send(a, now, on(60));
    rig.send(a, now + 3000, on(62));
    rig.send(a, now, Message::PitchBend { value: 12000 });
    rig.send(b, now, on(60));
    rig.render(1);
    rig.mix.revoke_source(1, 1);
    rig.render(20);
    let heard = rig.heard_by(1);
    let a_events: Vec<_> = heard.iter().filter(|(_, e)| e.channel == 0).map(|(_, e)| e.message).collect();
    assert_eq!(a_events.len(), 4, "on, bend, then the revocation's off and neutral bend: {a_events:?}");
    assert!(matches!(a_events[2], Message::NoteOff { key: 60, .. }));
    assert_eq!(a_events[3], Message::PitchBend { value: 8192 });
    let b_events = heard.iter().filter(|(_, e)| e.channel == 1).count();
    assert_eq!(b_events, 1, "the other port's note keeps sounding");
}

#[test]
fn a_seek_releases_the_clips_notes_and_keeps_live_ones() {
    let mut rig = Rig::new();
    let mut s = rig.session.clone();
    let mut seq = soundcraft_midi::Sequence::default();
    seq.notes.push(soundcraft_midi::Note { pitch: 48, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 9600 });
    let cid = s.new_clip_id();
    s.track_mut(rig.track).unwrap().playlist_mut().unwrap().clips.push(soundcraft_model::Clip::midi(cid, "m", 0, 48_000 * 8, seq));
    rig.publish(&s);
    rig.pos = 0;
    let now = rig.mix.next_frame().0;
    rig.send(port(1, 0), now, on(72));
    rig.render(2);
    // Seek back to the start.
    rig.pos = 0;
    rig.render(1);
    let heard = rig.heard_by(1);
    let offs: Vec<u8> = heard
        .iter()
        .filter_map(|(_, e)| match e.message {
            Message::NoteOff { key, .. } => Some(key),
            _ => None,
        })
        .collect();
    assert_eq!(offs, vec![48], "the clip's note was released at the seek, the live one was not");
}

#[test]
fn all_notes_off_releases_every_tracked_voice() {
    let mut rig = Rig::new();
    let now = rig.mix.next_frame().0;
    rig.send(port(1, 2), now, on_id(60, 9));
    rig.send(port(1, 2), now, cc(64, 127));
    rig.send(port(1, 2), now + 4000, on(61));
    rig.render(1);
    rig.mix.all_notes_off();
    rig.render(20);
    let heard = rig.heard_by(1);
    assert_eq!(every_on_has_its_off(&heard), 1, "the queued future note was dropped, the sounding one released");
    assert_eq!(
        pedal_edges(&heard.iter().map(|(f, e)| (*f, PluginEvent { channel: 0, ..*e })).collect::<Vec<_>>(), 64),
        vec![(now, true), (now + BLOCK as u64, false)]
    );
    assert!(rig.heard.lock().unwrap().iter().any(|(_, _, e)| e.is_none()), "and the plug-in was told to release everything");
}

#[test]
fn a_revoked_source_stays_revoked_until_a_newer_generation() {
    let mut rig = Rig::new();
    let (g, newer) = (Source { port: 4, channel: 0, generation: 7 }, Source { port: 4, channel: 0, generation: 8 });
    rig.mix.revoke_source(4, 7);
    let now = rig.mix.next_frame().0;
    // A note-on that was still in flight when the revocation was acknowledged.
    assert_eq!(rig.send(g, now, on(60)), EventOutcome::RevokedSource);
    assert_eq!(rig.send(Source { generation: 6, ..g }, now, on(61)), EventOutcome::RevokedSource, "older generations too");
    rig.render(2);
    assert!(rig.heard_by(1).is_empty(), "nothing from a revoked source reaches the plug-in");
    assert_eq!(rig.send(newer, now + 2 * BLOCK as u64, on(62)), EventOutcome::Queued);
    assert_eq!(rig.send(port(5, 0), now + 2 * BLOCK as u64, on(63)), EventOutcome::Queued, "other ports are untouched");
    rig.render(1);
    assert_eq!(rig.heard_by(1).len(), 2);
    assert_eq!(rig.mix.event_status().refused_revoked, 2);
}

#[test]
fn ports_beyond_the_table_are_refused_and_the_ui_port_is_not() {
    let mut rig = Rig::new();
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(port(MAX_PORTS as u16, 0), now, on(60)), EventOutcome::PortOutOfRange);
    assert_eq!(rig.send(port(0x8000, 0), now, on(60)), EventOutcome::PortOutOfRange);
    rig.mix.revoke_source(0x8000, 1);
    assert_eq!(rig.send(port(Source::UI, 0), now, on(61)), EventOutcome::Queued);
    assert_eq!(rig.send(port(MAX_PORTS as u16 - 1, 0), now, on(62)), EventOutcome::Queued);
    rig.mix.revoke_source(Source::UI, 1);
    assert_eq!(rig.send(port(Source::UI, 0), now, on(63)), EventOutcome::RevokedSource);
    assert_eq!(rig.send(Source { generation: 2, ..port(Source::UI, 0) }, now, on(63)), EventOutcome::Queued);
}

#[test]
fn pending_events_keep_a_stopped_engine_awake_until_delivered() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    assert!(!rig.mix.has_pending_events());
    let now = rig.mix.next_frame().0;
    rig.send(src, now + 600, on(60));
    rig.send(src, now + 1200, off(60));
    let mut awake = Vec::new();
    for _ in 0..8 {
        awake.push(rig.mix.has_pending_events());
        rig.render(1);
    }
    assert_eq!(awake, [true, true, true, true, true, false, false, false], "true until the off at frame +1200 is delivered");
    // A held pedal keeps it awake until a revocation releases it.
    let now = rig.mix.next_frame().0;
    rig.send(src, now, cc(64, 127));
    rig.render(3);
    assert!(rig.mix.has_pending_events(), "the pedal is held");
    rig.mix.revoke_source(1, 1);
    assert!(!rig.mix.has_pending_events(), "released by the revocation");
    rig.render(1);
    assert_eq!(pedal_edges(&rig.heard_by(1), 64).last(), Some(&(now + 3 * BLOCK as u64, false)));
}

// ---- review fixes (F10 post-review) ----

/// Review F10-1: `MixEngine::reset` (Stop) releases every source and the plug-in, resetting like
/// a real host, still gets the pedal-up and the note-off on the next block.
#[test]
fn a_reset_releases_every_source_and_the_releases_survive_the_host_reset() {
    let mut rig = Rig::new();
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(port(1, 0), now, on(72)), EventOutcome::Queued);
    assert_eq!(rig.send(port(1, 0), now, cc(64, 127)), EventOutcome::Queued);
    rig.render(1);
    rig.heard.lock().unwrap().clear();
    rig.mix.reset();
    rig.render(1);
    let heard = rig.heard_by(1);
    assert!(heard.iter().any(|(_, e)| matches!(e.message, Message::NoteOff { key: 72, .. })), "the note-off: {heard:?}");
    assert_eq!(pedal_values(&heard, 64), vec![(now + BLOCK as u64, 0)], "the pedal-up");
    assert!(!rig.mix.has_pending_events());
    assert_eq!(rig.send(port(1, 0), rig.mix.next_frame().0, off(72)), EventOutcome::Unmatched, "the reset released it");
}

/// Review F10-2: a release carrying an older transport epoch, for a note and pedal that sound, is
/// admitted and delivered; nothing stays pending.
#[test]
fn a_release_with_a_stale_transport_is_still_delivered() {
    let mut rig = Rig::new();
    let (t1, t2) = (TransportEpoch(1), TransportEpoch(2));
    rig.mix.set_transport_epoch(t1);
    let now = rig.mix.next_frame().0;
    let src = port(3, 0);
    let tev = |rig: &Rig, at: u64, m: Message| InstrumentEvent { transport: Some(t1), ..rig.event(src, Due::Frame(RenderFrame(at)), m) };
    assert_eq!(rig.mix.queue_event(&tev(&rig, now, on(60))), EventOutcome::Queued);
    assert_eq!(rig.mix.queue_event(&tev(&rig, now, cc(64, 127))), EventOutcome::Queued);
    rig.render(1);
    rig.mix.set_transport_epoch(t2);
    assert_eq!(rig.mix.queue_event(&tev(&rig, now + 300, off(60))), EventOutcome::Queued);
    assert_eq!(rig.mix.queue_event(&tev(&rig, now + 300, cc(64, 0))), EventOutcome::Queued);
    assert_eq!(rig.mix.queue_event(&tev(&rig, now + 300, on(61))), EventOutcome::StaleTransport, "non-releases still are refused");
    // A later transport change does not cancel the admitted releases.
    rig.mix.set_transport_epoch(TransportEpoch(3));
    rig.render(2);
    let heard = rig.heard_by(1);
    assert_eq!(every_on_has_its_off(&heard), 1);
    assert_eq!(pedal_values(&heard, 64), vec![(now, 127), (now + 300, 0)]);
    assert!(!rig.mix.has_pending_events());
}

/// Review F10-3: one holder's half-pedal values pass through exactly.
#[test]
fn half_pedal_values_pass_through() {
    let mut rig = Rig::new();
    let now = rig.mix.next_frame().0;
    for (at, v) in [(0, 127), (10, 90), (20, 70), (30, 0)] {
        assert_eq!(rig.send(port(1, 0), now + at, cc(64, v)), EventOutcome::Queued);
    }
    rig.render(1);
    assert_eq!(pedal_values(&rig.heard_by(1), 64), vec![(now, 127), (now + 10, 90), (now + 20, 70), (now + 30, 0)]);
    assert!(!rig.mix.has_pending_events());
}

/// Review F10-3: with two holders the plug-in gets the maximum of their values.
#[test]
fn two_holders_give_the_plug_in_their_maximum() {
    let mut rig = Rig::new();
    let (a, b) = (port(1, 0), port(2, 0));
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(a, now, cc(64, 100)), EventOutcome::Queued);
    assert_eq!(rig.send(b, now + 10, cc(64, 60)), EventOutcome::Queued);
    assert_eq!(rig.send(a, now + 20, cc(64, 0)), EventOutcome::Queued);
    assert_eq!(rig.send(b, now + 30, cc(64, 0)), EventOutcome::Queued);
    rig.render(1);
    assert_eq!(pedal_values(&rig.heard_by(1), 64), vec![(now, 100), (now + 20, 60), (now + 30, 0)]);
    assert!(!rig.mix.has_pending_events());
}

/// Review F10-4a: revoking generation MAX refuses generation MAX.
#[test]
fn revoking_the_last_generation_refuses_it() {
    let mut rig = Rig::new();
    let now = rig.mix.next_frame().0;
    let last = Source { port: 2, channel: 0, generation: u64::MAX };
    rig.mix.revoke_source(2, u64::MAX);
    assert_eq!(rig.send(last, now, on(60)), EventOutcome::RevokedSource);
    assert_eq!(rig.send(Source { generation: u64::MAX - 1, ..last }, now, on(60)), EventOutcome::RevokedSource);
    rig.mix.revoke_source(3, 5);
    assert_eq!(rig.send(Source { port: 3, generation: 5, ..last }, now, on(60)), EventOutcome::RevokedSource);
    assert_eq!(rig.send(Source { port: 3, generation: 6, ..last }, now, on(60)), EventOutcome::Queued);
}

/// Review F10-4b: an events engine with no adopted plan has no instrument to take an event.
#[test]
fn an_events_engine_without_a_plan_refuses_events() {
    let rig = Rig::new();
    let ev = rig.event(port(1, 0), Due::Next, on(60));
    let mut eng = MixEngine::realtime_with_events(target(), rig.epoch, EventLimits::default());
    assert_eq!(eng.queue_event(&ev), EventOutcome::NoInstrument);
    assert!(!eng.has_pending_events());
}

/// Review F10-4c: an instrument that fails keeps nothing pending; its queue is voided (counted)
/// and its sounding note and pedal released.
#[test]
fn a_failed_instrument_keeps_nothing_pending() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let now = rig.mix.next_frame().0;
    rig.send(src, now, on(60));
    rig.send(src, now, cc(64, 127));
    rig.send(src, now + 3000, on(62));
    rig.send(src, now + 4000, off(62));
    rig.render(1);
    rig.fail.store(true, Ordering::Relaxed);
    rig.render(1);
    assert!(!rig.mix.has_pending_events(), "the failed instrument's queue is void");
    assert_eq!(rig.mix.event_status().voided, [1, 1], "the future on and its off");
    assert_eq!(rig.send(src, rig.mix.next_frame().0, on(64)), EventOutcome::NoInstrument);
    rig.fail.store(false, Ordering::Relaxed);
    rig.mix.reset();
    rig.render(1);
    let heard = rig.heard_by(1);
    assert_eq!(every_on_has_its_off(&heard), 1);
    assert_eq!(pedal_values(&heard, 64).last().map(|x| x.1), Some(0));
}

/// Review F10-4c: an instrument whose track becomes inactive keeps nothing pending either, and
/// takes no new events.
#[test]
fn an_inactive_instrument_keeps_nothing_pending() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let now = rig.mix.next_frame().0;
    rig.send(src, now, on(60));
    rig.send(src, now + 3000, on(62));
    rig.render(1);
    let mut s = rig.session.clone();
    s.track_mut(rig.track).unwrap().inactive = true;
    rig.publish(&s);
    assert!(!rig.mix.has_pending_events());
    assert_eq!(rig.mix.event_status().voided, [1, 0]);
    assert_eq!(rig.send(src, rig.mix.next_frame().0, on(64)), EventOutcome::NoInstrument);
    rig.render(1);
}

/// Review F10-4d: a re-strike's off is charged with its note-on, two slots, against the
/// non-release budget.
#[test]
fn a_re_strike_is_charged_two_slots() {
    let mut rig = Rig::new();
    let src = port(1, 0);
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(src, now, on(60)), EventOutcome::Queued);
    rig.render(1);
    let now = rig.mix.next_frame().0;
    // 255 of the 256 non-release slots.
    for k in 0..255u64 {
        assert_eq!(rig.send(src, now + 1000 + k, cc(1, 1)), EventOutcome::Queued);
    }
    assert_eq!(rig.send(src, now, on(60)), EventOutcome::Refused(Shed::QueueFull), "a re-strike needs two slots");
    assert_eq!(rig.send(src, now, on(61)), EventOutcome::Queued, "a new note needs one");
    rig.render(8);
    // Delivered: the budget is back, and a re-strike fits.
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(src, now, on(60)), EventOutcome::Queued);
    rig.render(1);
    assert_eq!(rig.mix.event_status().restrikes, 1);
    rig.mix.reset();
    rig.render(1);
    assert_eq!(every_on_has_its_off(&rig.heard_by(1)), 3);
}

/// Review F10-4e (deviation 5): CC 121 from one source while another holds the pedal: the plug-in
/// sees CC 121, then the pedal down again for the holder, at its value.
#[test]
fn reset_controllers_re_sends_other_sources_pedals() {
    let mut rig = Rig::new();
    let (a, b) = (port(1, 0), port(2, 0));
    let now = rig.mix.next_frame().0;
    assert_eq!(rig.send(b, now, cc(64, 100)), EventOutcome::Queued);
    assert_eq!(rig.send(a, now + 10, cc(121, 0)), EventOutcome::Queued);
    rig.render(1);
    let ctl: Vec<(u64, u8, u8)> = rig
        .heard_by(1)
        .iter()
        .filter_map(|(f, e)| if let Message::Control { cc, value } = e.message { Some((*f, cc, value)) } else { None })
        .collect();
    assert_eq!(ctl, vec![(now, 64, 100), (now + 10, 121, 0), (now + 10, 64, 100)]);
}
