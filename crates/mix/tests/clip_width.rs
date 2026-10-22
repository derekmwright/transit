//! Width conversion must keep every source channel, in the plan renderer, without
//! allocating. Values are deliberately asymmetric: copying only the left channel cannot pass.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, SlotGen, create_instance,
    render_clips,
};
use soundcraft_model::{ChannelFormat, Clip, ClipId, Session, SourceAudio, SourceId, TrackId, TrackKind};
use soundcraft_time::Range;
use std::sync::Arc;
use transit_rt::testing::{CountingAlloc, measure};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const BLOCK: usize = 128;

fn session(values: &[f32], format: ChannelFormat) -> (Session, TrackId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, format, None);
    let audio = AudioBuffer { sample_rate: 48_000, channels: values.iter().map(|v| vec![*v; BLOCK * 4]).collect() };
    s.pool.insert(SourceId(1), Arc::new(SourceAudio::new(audio)));
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(ClipId(1), "width", SourceId(1), 0, 0, (BLOCK * 4) as i64));
    (s, t)
}

fn assert_fold(values: &[f32], format: ChannelFormat, expected: &[f32]) {
    soundcraft_mix::mark_audio_thread();
    MixEngine::force_serial(true);
    let (s, t) = session(values, format);
    let dry = render_clips(&s, t, Range::new(0, BLOCK as i64));
    assert_eq!(dry, expected.iter().map(|v| vec![*v; BLOCK]).collect::<Vec<_>>());

    let mut planned = planned_engine(&s);
    let mut b = vec![vec![0.0; BLOCK]; 2];
    let mut reference = b.clone();
    // An already-folded source is the oracle for panning, faders, and master processing.
    let (reference_session, _) = session(expected, format);
    let mut reference_mix = planned_engine(&reference_session);
    for block in 0..4 {
        let pos = (block * BLOCK) as i64;
        let (_, planned_allocs) = measure(|| planned.render_active(pos, BLOCK, &mut b));
        assert_eq!((planned_allocs.allocs, planned_allocs.frees), (0, 0), "plan: {planned_allocs:?}");
        reference_mix.render_active(pos, BLOCK, &mut reference);
        for (y, expected) in b.iter().flatten().zip(reference.iter().flatten()) {
            assert_eq!(y.to_bits(), expected.to_bits(), "folded signal must reach the mix");
        }
    }
}

/// An engine that has adopted the plan of `s`.
fn planned_engine(s: &Session) -> MixEngine {
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 };
    let mut compiler = PlanCompiler::new(target, PlanLimits::default());
    let mut plan = compiler
        .compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut |spec: &InstanceSpec, _: SlotGen| {
            create_instance(&spec.id, spec.state.as_deref(), 48_000.0, BLOCK, spec.ch)
        })
        .unwrap();
    let mut engine = MixEngine::realtime(target);
    assert_eq!(engine.adopt_plan(&mut plan), Adoption::Adopted);
    compiler.reclaim(plan);
    engine
}

#[test]
fn stereo_on_mono_averages_both_channels_without_allocating() {
    assert_fold(&[0.0, 0.75], ChannelFormat::Mono, &[0.375]);
    assert_fold(&[0.25, 0.75], ChannelFormat::Mono, &[0.5]);
    assert_fold(&[0.5, -0.5], ChannelFormat::Mono, &[0.0]);
}

#[test]
fn wider_clips_fold_every_channel_by_index_without_allocating() {
    assert_fold(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.75], ChannelFormat::Mono, &[0.125]);
    assert_fold(&[0.0, 0.0, 0.0, 0.0, 0.75, 0.375], ChannelFormat::Stereo, &[0.25, 0.125]);
    // An odd source width averages each destination's actual number of contributors.
    assert_fold(&[0.25, 0.75, 0.75], ChannelFormat::Stereo, &[0.5, 0.75]);
    for source in 0..6 {
        let mut values = [0.0; 6];
        values[source] = 0.75;
        let mut expected = [0.0; 2];
        expected[source % 2] = 0.25;
        assert_fold(&values, ChannelFormat::Stereo, &expected);
    }
}

#[test]
fn matching_and_narrower_sources_keep_their_channel_mapping() {
    assert_fold(&[0.25, 0.75], ChannelFormat::Stereo, &[0.25, 0.75]);
    assert_fold(&[0.375], ChannelFormat::Stereo, &[0.375, 0.375]);
}

#[test]
fn downmix_preserves_offset_stretch_and_clip_gain() {
    let (mut s, t) = session(&[0.0, 0.0], ChannelFormat::Mono);
    s.pool.insert(
        SourceId(1),
        Arc::new(SourceAudio::new(AudioBuffer {
            sample_rate: 48_000,
            channels: vec![vec![0.0; 8], vec![0.0, 0.25, 0.5, 0.75, 1.0, 0.75, 0.5, 0.25]],
        })),
    );
    let clip = &mut s.track_mut(t).unwrap().playlist_mut().unwrap().clips[0];
    *clip = Clip::audio(ClipId(1), "stretch", SourceId(1), 1, 3, 6);
    clip.stretch = 2.0;
    clip.gain_db = -6.0;
    let gain = soundcraft_dsp::db_to_gain(-6.0);
    let dry = render_clips(&s, t, Range::new(0, 12));
    assert_eq!(&dry[0][..3], &[0.0; 3]);
    for (i, actual) in dry[0][3..9].iter().enumerate() {
        let expected = (0.25 + i as f32 * 0.125) / 2.0 * gain;
        assert!((actual - expected).abs() < 1e-6, "{i}: {actual} vs {expected}");
    }
    assert_eq!(&dry[0][9..], &[0.0; 3]);
}
