//! Exercise the production bridge, compiler and engine without opening a device.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]

use crate::{MeterSnapshot, plan_bridge, publication::Service, record::InputRing};
use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{CompileError, DeviceGeneration, EnvelopeState, InstanceSpec, NewInstance, PlanGeneration, PlanLimits, RenderTarget, SlotGen};
use soundcraft_model::{ChannelFormat, Clip, Insert, Session, SourceAudio, SourceId, TrackKind};
use soundcraft_rt::testing::measure;
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOC: soundcraft_rt::testing::CountingAlloc = soundcraft_rt::testing::CountingAlloc;

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512 }
}

fn make(spec: &InstanceSpec, _: SlotGen) -> Option<NewInstance> {
    soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), 48_000.0, 512, spec.ch)
}

fn session(format: ChannelFormat) -> Session {
    let mut s = Session::default();
    s.outputs[0].format = format;
    let track = s.add_track(TrackKind::Audio, format, None);
    let channels = (0..format.channels()).map(|i| vec![(i + 1) as f32 * 0.1; 8192]).collect();
    s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let clip = s.new_clip_id();
    s.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(clip, "tone", SourceId(5), 0, 0, 8192));
    s
}

fn adopt(audio: &mut plan_bridge::Audio) {
    let (result, stats) = measure(|| audio.service(&mut super::callback_scope(), true));
    assert_eq!(result, Service::Returned);
    assert!(stats.is_zero(), "adoption: {stats:?}");
}

#[test]
fn coalescing_and_stalled_control_bound_construction_and_retirement() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    let mut s = session(ChannelFormat::Stereo);
    s.tracks[0].mixer.inserts.push(Some(Insert::new("gain")));
    let first = Arc::new(s.clone());
    let weak = Arc::downgrade(&first);
    control.request(first, None);
    let mut constructed = 0;
    let mut factory = |spec: &InstanceSpec, generation| {
        constructed += 1;
        make(spec, generation)
    };
    assert!(control.prepare(&mut factory).unwrap());
    for i in 0..100 {
        let mut edit = s.clone();
        edit.name = format!("edit {i}");
        control.request(Arc::new(edit), None);
        assert!(!control.prepare(&mut factory).unwrap());
    }
    adopt(&mut audio);
    assert_eq!(control.active(), PlanGeneration::EMPTY);
    assert_eq!(audio.engine().session().name, s.name);
    assert!(!control.prepare(&mut factory).unwrap(), "a returned but unreclaimed credit stays spent");
    assert!(matches!(control.poll(), Some(super::ReturnReport { state: EnvelopeState::Returned { .. }, .. })));
    assert!(control.prepare(&mut factory).unwrap());
    adopt(&mut audio);
    assert_eq!(audio.engine().session().name, "edit 99");
    assert!(weak.upgrade().is_some(), "old session is still held by the return envelope");
    control.poll();
    assert!(weak.upgrade().is_none());
    assert_eq!(constructed, 1, "no speculative instance construction while the credit is spent");
    control.retire(audio);
}

#[test]
fn a_layout_change_waits_for_the_boundary_and_cannot_readmit_within_one_callback() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    control.request(Arc::new(session(ChannelFormat::Surround51)), None);
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    control.poll();
    let ((), stats) = measure(|| audio.render(0, 512));
    assert!(stats.is_zero(), "render/meters: {stats:?}");
    let old = audio.output().to_vec();
    let fold = *audio.fold();
    control.request(Arc::new(session(ChannelFormat::Stereo)), None);
    control.prepare(&mut make).unwrap();
    let mut callback = super::callback_scope();
    let (_, stats) = measure(|| {
        for _ in 0..4 {
            assert_eq!(audio.service(&mut callback, false), Service::Empty);
        }
    });
    assert!(stats.is_zero());
    assert_eq!(audio.output(), old);
    assert_eq!(audio.fold(), &fold);
    assert_eq!(audio.service(&mut callback, true), Service::Returned);
    control.poll();
    control.request(Arc::new(session(ChannelFormat::Surround51)), None);
    control.prepare(&mut make).unwrap();
    assert_eq!(audio.service(&mut callback, true), Service::Empty, "same device callback, next conversion chunk");
    assert_eq!(audio.output().len(), 2);
    adopt(&mut audio);
    assert_eq!(audio.output().len(), 6);
    control.retire(audio);
}

#[test]
fn input_storage_and_ring_owners_return_with_the_displaced_plan() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    let s = Arc::new(session(ChannelFormat::Stereo));
    let first = InputRing::new(3, 48_000);
    let weak = Arc::downgrade(&first);
    control.request(Arc::clone(&s), Some(first));
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    control.poll();
    assert_eq!(audio.engine().input.len(), 3);
    control.request(Arc::clone(&s), Some(InputRing::new(1, 48_000)));
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    assert_eq!(audio.engine().input.len(), 1);
    assert!(weak.upgrade().is_some());
    control.poll();
    assert!(weak.upgrade().is_none(), "last ring owner released on control");
    control.request(s, None);
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    assert!(audio.engine().input.is_empty());
    control.retire(audio);
}

#[test]
fn meter_snapshots_follow_confirmed_layout_and_clear_without_audio_heap_work() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    let s = session(ChannelFormat::Surround51);
    let id = s.tracks[0].id;
    control.request(Arc::new(s), None);
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    let ((), stats) = measure(|| audio.render(0, 128));
    assert!(stats.is_zero(), "{stats:?}");
    let mut meters = MeterSnapshot::default();
    control.copy_meters(&mut meters);
    assert!(meters.tracks.is_empty(), "adoption has not yet been confirmed to control");
    control.poll();
    control.copy_meters(&mut meters);
    assert_eq!(meters.main.peaks.len(), 6);
    assert_eq!(meters.tracks[&id].peaks.len(), 6);
    assert!(meters.main.peak[0] > 0.09);
    let ((), stats) = measure(|| audio.clear_meters());
    assert!(stats.is_zero());
    control.copy_meters(&mut meters);
    assert_eq!(meters.main.peak, [0.0; 2]);
    control.request(Arc::new(Session::default()), None);
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    control.poll();
    control.copy_meters(&mut meters);
    assert!(!meters.tracks.contains_key(&id));
    control.retire(audio);
}

#[test]
fn target_and_storage_limits_reject_before_factories_or_active_changes() {
    let (mut control, audio) = plan_bridge::pair(target(), PlanLimits { tracks: 0, ..PlanLimits::default() });
    control.request(Arc::new(session(ChannelFormat::Stereo)), None);
    assert!(matches!(control.prepare(&mut |_, _| panic!("factory must not run")), Err(CompileError::ResourceLimit { .. })));
    control.request(Arc::new(Session::new("other rate", soundcraft_time::SampleRate::new(44_100).unwrap())), None);
    assert!(matches!(control.prepare(&mut |_, _| panic!("factory must not run")), Err(CompileError::TargetMismatch { .. })));
    assert_eq!(control.active(), PlanGeneration::EMPTY);
    control.retire(audio);

    let (mut control, audio) = plan_bridge::pair(target(), PlanLimits::default());
    let input = InputRing::new(1, 48_000);
    // A corrupted public channel descriptor must not cause unbounded preparation.
    input.channels.store(usize::MAX, std::sync::atomic::Ordering::Relaxed);
    control.request(Arc::new(Session::default()), Some(input));
    assert!(matches!(control.prepare(&mut |_, _| panic!("factory must not run")), Err(CompileError::ResourceLimit { what: "playback storage", .. })));
    control.retire(audio);
}

#[test]
fn rejected_instances_keep_their_binding_when_reused_and_status_never_crosses_generations() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    let mut s = session(ChannelFormat::Stereo);
    s.tracks[0].mixer.inserts.push(Some(Insert::new("gain")));
    let s = Arc::new(s);
    control.request(Arc::clone(&s), None);
    control.prepare(&mut make).unwrap();
    let binding = control.pending_bindings().unwrap();
    audio.mix = soundcraft_mix::MixEngine::realtime(RenderTarget { device: DeviceGeneration(2), ..target() });
    adopt(&mut audio);
    assert!(matches!(control.poll(), Some(super::ReturnReport { state: EnvelopeState::Rejected { .. }, .. })));
    assert_eq!(control.active(), PlanGeneration::EMPTY);
    assert!(control.active_bindings().is_empty());
    assert!(control.take_released().is_empty(), "refused instance is retained in the bounded compiler pool");
    audio.mix = soundcraft_mix::MixEngine::realtime(target());
    control.request(Arc::clone(&s), None);
    control.prepare(&mut |_, _| panic!("pooled instance must be reused")).unwrap();
    assert_eq!(control.pending_bindings().unwrap(), binding);
    adopt(&mut audio);
    control.poll();
    audio.render(0, 64); // queue one old-generation report without polling it
    let old_generation = control.active();
    control.request(s, None);
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    audio.render(64, 64); // old report still occupies the one-slot status queue
    control.poll();
    assert!(control.status().is_none(), "old report was discarded, not attributed to the newly confirmed plan");
    let ((), stats) = measure(|| audio.render(128, 64));
    assert!(stats.is_zero());
    control.poll();
    let report = control.status().unwrap();
    assert_eq!(report.generation, control.active());
    assert!(report.generation > old_generation);
    assert_eq!(control.active_bindings(), binding);
    control.retire(audio);
}

struct Tracked {
    inner: Box<dyn soundcraft_dsp::Plugin>,
    drops: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    panic_sample: Option<Arc<std::sync::atomic::AtomicBool>>,
    panic_processing: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl soundcraft_dsp::Plugin for Tracked {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, rate: f32, block: usize, channels: usize) {
        self.inner.prepare(rate, block, channels);
    }
    fn reset(&mut self) {
        assert!(!self.panic_processing.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)), "reset after torn adoption");
        self.inner.reset();
    }
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        self.inner.set_param(id, value)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        assert!(!self.panic_processing.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)), "process after torn adoption");
        self.inner.process(io, frames);
    }
    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        assert!(!self.panic_processing.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)), "note after torn adoption");
        self.inner.note_on(offset, note, velocity);
    }
    fn note_off(&mut self, offset: usize, note: u8) {
        assert!(!self.panic_processing.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)), "release after torn adoption");
        self.inner.note_off(offset, note);
    }
    fn all_notes_off(&mut self) {
        assert!(
            !self.panic_processing.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)),
            "all-notes-off after torn adoption"
        );
        self.inner.all_notes_off();
    }
    fn set_sample(&mut self, sample: Option<Arc<dyn soundcraft_dsp::SampleSource>>) -> bool {
        assert!(!self.panic_sample.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)), "injected sample adoption fault");
        self.inner.set_sample(sample)
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.lock().unwrap().push(std::thread::current().id());
    }
}

#[test]
fn shutdown_recovers_queued_returned_rejected_and_live_instances_on_control() {
    for stage in 0..5 {
        let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
        let mut s = session(ChannelFormat::Stereo);
        s.tracks[0].mixer.inserts.push(Some(Insert::new("clap:test.owned")));
        let drops = Arc::new(Mutex::new(Vec::new()));
        control.request(Arc::new(s), None);
        control
            .prepare(&mut |spec, _| {
                let mut inner = soundcraft_dsp::create("gain")?;
                inner.prepare(48_000.0, 512, spec.ch);
                Some(NewInstance {
                    plugin: Box::new(Tracked { inner, drops: Arc::clone(&drops), panic_sample: None, panic_processing: None }),
                    refused_state: false,
                })
            })
            .unwrap();
        let bindings = control.pending_bindings().unwrap();
        if stage == 4 {
            audio.mix = soundcraft_mix::MixEngine::realtime(RenderTarget { device: DeviceGeneration(2), ..target() });
        }
        if stage != 0 {
            // The owner stays on control in production; this harness returns the entire
            // endpoint without dropping it on the spawned rendering thread.
            audio = std::thread::spawn(move || {
                let (_, stats) = measure(|| {
                    if stage == 3 {
                        audio.return_pending(&mut super::callback_scope());
                    } else {
                        audio.service(&mut super::callback_scope(), true);
                    }
                });
                (audio, stats)
            })
            .join()
            .map(|(audio, stats)| {
                assert!(stats.is_zero(), "{stats:?}");
                audio
            })
            .unwrap();
        }
        if stage == 2 {
            control.poll();
        }
        assert!(drops.lock().unwrap().is_empty());
        let released = control.retire(audio);
        assert_eq!(released, bindings);
        assert_eq!(*drops.lock().unwrap(), [std::thread::current().id()]);
    }
}

#[test]
fn an_adoption_unwind_keeps_the_outer_envelope_until_control_teardown() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    let mut s = Session::default();
    let track = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let mut sampler = Insert::new("sampler");
    sampler.sample = Some(SourceId(1));
    s.track_mut(track).unwrap().instrument = Some(sampler);
    for source in [SourceId(1), SourceId(2)] {
        s.pool.insert(source, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 64]] })));
    }
    let drops = Arc::new(Mutex::new(Vec::new()));
    let fault = Arc::new(AtomicBool::new(false));
    let forbid = Arc::new(AtomicBool::new(false));
    control.request(Arc::new(s.clone()), None);
    control
        .prepare(&mut |spec, _| {
            let mut inner = soundcraft_dsp::create(&spec.id)?;
            inner.prepare(48_000.0, 512, spec.ch);
            Some(NewInstance {
                plugin: Box::new(Tracked {
                    inner,
                    drops: Arc::clone(&drops),
                    panic_sample: Some(Arc::clone(&fault)),
                    panic_processing: Some(Arc::clone(&forbid)),
                }),
                refused_state: false,
            })
        })
        .unwrap();
    adopt(&mut audio);
    control.poll();
    let live = control.active_bindings();
    s.track_mut(track).unwrap().instrument.as_mut().unwrap().sample = Some(SourceId(2));
    control.request(Arc::new(s), None);
    control.prepare(&mut |_, _| panic!("instance must be reused")).unwrap();
    fault.store(true, Ordering::Relaxed);
    audio = std::thread::spawn(move || {
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| audio.service(&mut super::callback_scope(), true))).is_err();
        (audio, failed)
    })
    .join()
    .map(|(audio, failed)| {
        assert!(failed);
        audio
    })
    .unwrap();
    assert!(audio.engine().faulted());
    forbid.store(true, Ordering::Relaxed);
    let (mut actions, mut action_audio) = crate::action_lane::pair(&control);
    actions.send_action(1u8, Some(audio.engine().active().generation)).unwrap();
    actions.send_note(2u8, crate::action_lane::NoteKind::Off).unwrap();
    let (result, stats) = measure(|| {
        action_audio.service(&mut crate::action_lane::callback_scope(), &mut audio, &mut |_, _| panic!("torn bridge must refuse action handlers"))
    });
    assert!(stats.is_zero());
    assert!(result.faulted);
    assert!(actions.retire(action_audio).iter().all(|r| r.outcome == crate::action_lane::Outcome::Faulted));
    let ((), stats) = measure(|| {
        audio.reset();
        audio.all_notes_off();
        audio.flush_releases(128);
        audio.live_note(track, 60, 100);
        audio.live_note(track, 60, 0);
    });
    assert!(stats.is_zero());
    assert!(drops.lock().unwrap().is_empty());
    assert!(control.poll().is_none(), "in-flight envelope remains anchored on the audio endpoint after unwind");
    let (result, stats) = measure(|| audio.return_pending(&mut super::callback_scope()));
    assert_eq!(result, Service::Returned);
    assert!(stats.is_zero());
    assert!(drops.lock().unwrap().is_empty());
    let report = control.poll().unwrap();
    assert!(report.torn);
    assert_eq!(report.state, EnvelopeState::Pending);
    assert!(control.faulted());
    assert!(control.pending_bindings().is_none());
    let issued = control.issued;
    control.request(Arc::new(session(ChannelFormat::Stereo)), Some(InputRing::new(2, 48_000)));
    let (result, stats) = measure(|| control.prepare(&mut |_, _| panic!("factory must not run after torn adoption")));
    assert!(matches!(result, Err(CompileError::Faulted)));
    assert!(stats.is_zero(), "fault refusal must precede allocating playback storage or consuming the request");
    assert_eq!(control.issued, issued);
    assert!(control.latest.is_some());
    let released = control.retire(audio);
    assert_eq!(released, live);
    assert_eq!(*drops.lock().unwrap(), [std::thread::current().id()]);
}

#[test]
fn the_device_adapter_carries_one_budget_across_conversion_chunks_and_fault_returns() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    control.request(Arc::new(session(ChannelFormat::Stereo)), None);
    control.prepare(&mut make).unwrap();
    let mut callback = crate::OutputCallback::new(2, 48_000);
    let mut device = vec![0.0f32; 40_000]; // three real scratch-conversion chunks
    let mut chunks = 0;
    callback.fill_with(&mut device, |_, budget, _actions| {
        if chunks == 0 {
            assert_eq!(audio.service(budget, true), Service::Returned);
            // Emulate control running between chunks. Its allocations are not callback work.
            control.poll();
            control.request(Arc::new(session(ChannelFormat::Surround51)), None);
            control.prepare(&mut make).unwrap();
        } else {
            let (result, stats) = measure(|| audio.return_pending(budget));
            assert_eq!(result, Service::Empty, "fault-return must respect the same device budget");
            assert!(stats.is_zero());
            assert_eq!(audio.service(budget, true), Service::Empty);
        }
        chunks += 1;
    });
    assert_eq!(chunks, 3);
    assert!(control.poll().is_none());
    assert_eq!(audio.output().len(), 2);
    callback.fill_with(&mut device[..128], |_, budget, _actions| {
        audio.return_pending(budget);
    });
    let report = control.poll().unwrap();
    assert_eq!(report.state, EnvelopeState::Pending);
    assert!(!report.torn);
    control.retire(audio);
}

#[test]
fn prolonged_render_ahead_deferral_is_counted_once_per_actual_callback() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    control.request(Arc::new(session(ChannelFormat::Stereo)), None);
    control.prepare(&mut make).unwrap();
    let mut callback = crate::OutputCallback::new(2, 48_000);
    let mut device = vec![0.0f32; 40_000];
    let ((), stats) = measure(|| {
        for _ in 0..10 {
            callback.fill_with(&mut device, |_, budget, _actions| {
                audio.service(budget, false);
            });
        }
    });
    assert!(stats.is_zero());
    let telemetry = control.deferral_telemetry();
    assert_eq!(telemetry.deferred_callbacks, 10);
    assert_eq!(telemetry.current_age_callbacks, 10);
    assert_eq!(telemetry.maximum_age_callbacks, 10);
    assert!(telemetry.published_generation > telemetry.admitted_generation);
    callback.fill_with(&mut device[..128], |_, budget, _actions| {
        audio.service(budget, true);
    });
    let telemetry = control.deferral_telemetry();
    assert_eq!(telemetry.current_age_callbacks, 0);
    assert_eq!(telemetry.maximum_age_callbacks, 10);
    assert_eq!(telemetry.published_generation, telemetry.admitted_generation);
    control.retire(audio);
}

#[test]
fn returning_an_unadopted_plan_spends_the_callback_budget_before_a_new_publication() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    control.request(Arc::new(session(ChannelFormat::Stereo)), None);
    control.prepare(&mut make).unwrap();
    let mut callback = crate::OutputCallback::new(2, 48_000);
    let mut device = vec![0.0f32; 40_000];
    let mut chunks = 0;
    callback.fill_with(&mut device, |_, budget, _actions| {
        if chunks == 0 {
            assert_eq!(audio.return_pending(budget), Service::Returned);
            assert_eq!(control.poll().unwrap().state, EnvelopeState::Pending);
            control.request(Arc::new(session(ChannelFormat::Surround51)), None);
            control.prepare(&mut make).unwrap();
        }
        assert_eq!(audio.service(budget, true), Service::Empty);
        chunks += 1;
    });
    assert_eq!(chunks, 3);
    assert_eq!(audio.engine().active().generation, PlanGeneration::EMPTY);
    callback.fill_with(&mut device[..128], |_, budget, _actions| {
        assert_eq!(audio.service(budget, true), Service::Returned);
    });
    assert_eq!(audio.output().len(), 6);
    control.poll();
    control.retire(audio);
}

#[test]
fn displaced_input_storage_returns_in_the_same_envelope() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    let s = Arc::new(session(ChannelFormat::Stereo));
    control.request(Arc::clone(&s), Some(InputRing::new(3, 48_000)));
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    control.poll();
    let previous = audio.engine().input.as_ptr();
    control.request(s, Some(InputRing::new(1, 48_000)));
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    assert!(control.port.reclaim_one(|env| {
        assert_eq!(env.buffers.input_storage.len(), 3);
        assert_eq!(env.buffers.input_storage.as_ptr(), previous);
        control.compiler.reclaim(env.plan);
    }));
    control.retire(audio);
}

#[test]
fn stale_status_cannot_change_the_compilers_delay_capacity_hints() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    let s = Arc::new(session(ChannelFormat::Stereo));
    control.request(Arc::clone(&s), None);
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    control.poll();
    let stale = control.active();
    control.request(Arc::clone(&s), None);
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    audio
        .status_tx
        .push(super::StatusReport {
            generation: stale,
            status: soundcraft_mix::PlanStatus { capacity_exceeded: 1, required_latency: 16_384, ..Default::default() },
        })
        .unwrap();
    control.poll();
    let base = control.active();
    let env = control.compiler.compile(s, base, base.checked_next().unwrap(), &mut make).unwrap();
    assert!(!env.plan().is_structural(), "a stale delay hint must not cause a structural rebuild");
    control.compiler.reclaim(env);
    control.retire(audio);
}

#[test]
fn a_rejected_plan_cannot_replace_output_input_fold_or_meter_owners() {
    let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
    control.request(Arc::new(session(ChannelFormat::Surround51)), Some(InputRing::new(3, 48_000)));
    control.prepare(&mut make).unwrap();
    adopt(&mut audio);
    control.poll();
    audio.render(0, 128);
    let mut previous_meters = MeterSnapshot::default();
    control.copy_meters(&mut previous_meters);
    let previous_output = audio.output().to_vec();
    let previous_fold = *audio.fold();
    let previous_input = Arc::as_ptr(audio.buffers.input.as_ref().unwrap());
    control.request(Arc::new(session(ChannelFormat::Stereo)), Some(InputRing::new(1, 48_000)));
    control.prepare(&mut make).unwrap();
    // Deliberately corrupt the target on control to exercise atomic rejection.
    audio.mix = soundcraft_mix::MixEngine::realtime(RenderTarget { device: DeviceGeneration(2), ..target() });
    adopt(&mut audio);
    assert_eq!(audio.output(), previous_output);
    assert_eq!(*audio.fold(), previous_fold);
    assert_eq!(Arc::as_ptr(audio.buffers.input.as_ref().unwrap()), previous_input);
    assert!(matches!(control.poll().unwrap().state, EnvelopeState::Rejected { .. }));
    let mut meters = MeterSnapshot::default();
    control.copy_meters(&mut meters);
    assert_eq!(meters.main, previous_meters.main);
    assert_eq!(meters.tracks, previous_meters.tracks);
    control.retire(audio);
}

#[test]
fn public_stop_handler_reaches_the_plugin_before_ack_and_faults_both_classes_on_unwind() {
    use crate::action_lane::{self, Event, NoteKind, Outcome};
    use std::sync::atomic::{AtomicBool, Ordering};
    for panic in [false, true] {
        let (mut control, mut audio) = plan_bridge::pair(target(), PlanLimits::default());
        let mut session = Session::default();
        let track = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        session.track_mut(track).unwrap().instrument = Some(Insert::new("clap:test.stop"));
        let drops = Arc::new(Mutex::new(Vec::new()));
        let fault = Arc::new(AtomicBool::new(false));
        control.request(Arc::new(session), None);
        control
            .prepare(&mut |spec, _| {
                let mut inner = soundcraft_dsp::create("gain")?;
                inner.prepare(48_000.0, 512, spec.ch);
                Some(NewInstance {
                    plugin: Box::new(Tracked { inner, drops: Arc::clone(&drops), panic_sample: None, panic_processing: Some(Arc::clone(&fault)) }),
                    refused_state: false,
                })
            })
            .unwrap();
        adopt(&mut audio);
        control.poll();
        audio.set_processing(false, true, false);
        let (mut actions, mut lane) = action_lane::pair(&control);
        let stop = actions.stop().unwrap();
        actions.send_action(1, None).unwrap();
        actions.send_note(2, NoteKind::On).unwrap();
        fault.store(panic, Ordering::Relaxed);
        let mut stopped = false;
        let mut handler = |event, bridge: &mut plan_bridge::Processing<'_>| {
            if matches!(event, Event::Stop { .. }) {
                bridge.all_notes_off();
                bridge.set_processing(true, false, false);
                stopped = true;
                assert_eq!(actions.recovery_progress().0.get(), 0, "no acknowledgement before handler returns");
            }
        };
        if panic {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    lane.service(&mut action_lane::callback_scope(), &mut audio, &mut handler)
                }))
                .is_err(),
                "the actual plugin must be called inside service"
            );
            assert!(!stopped);
            assert_eq!(actions.recovery_progress().0.get(), 0);
            let (report, stats) =
                measure(|| lane.service(&mut action_lane::callback_scope(), &mut audio, &mut |_, _| panic!("faulted handler replay")));
            assert!(report.faulted);
            assert!(stats.is_zero());
            let receipts = actions.retire(lane);
            assert_eq!(receipts.len(), 2);
            assert!(receipts.iter().all(|r| r.outcome == Outcome::Faulted));
        } else {
            let (report, stats) = measure(|| lane.service(&mut action_lane::callback_scope(), &mut audio, &mut handler));
            assert!(!report.faulted);
            assert!(stats.is_zero());
            assert!(stopped && !audio.engine().recording && audio.engine().monitor_only);
            assert_eq!(actions.recovery_progress().0, stop);
            let receipts = actions.retire(lane);
            assert_eq!(receipts.len(), 2);
            assert!(receipts.iter().all(|r| r.outcome == Outcome::Applied));
        }
        fault.store(false, Ordering::Relaxed);
        control.retire(audio);
        assert_eq!(*drops.lock().unwrap(), [std::thread::current().id()]);
    }
}
