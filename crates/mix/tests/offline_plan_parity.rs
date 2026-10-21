//! Offline render parity (ADR 0015, slice 0): a bounce of a small session through the plan
//! renderer (`render_range`, `render_bus`, `RangeRender`) against the legacy `MixEngine::render`
//! path (`render_legacy`). The session has a track with a latent built-in insert, an aux bus with
//! a reverb fed by a send, volume automation, and an instrument on a tempo map with a tempo change.
//!
//! The legacy renderer goes away in part 2 of the slice; this file is gated with it.
#![cfg(feature = "legacy-render")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_midi::{Note, Sequence};
use soundcraft_mix::{RangeRender, render_bus, render_legacy, render_range};
use soundcraft_model::{AutoParam, Bus, BusId, ChannelFormat, Clip, Insert, Route, SendSlot, Session, SourceAudio, SourceId, TrackKind};
use soundcraft_time::Range;
use std::sync::Arc;

const SR: u32 = 48_000;
const VERB: BusId = BusId(900);

fn builtin(id: &str) -> Insert {
    let mut ins = Insert::new(id);
    for p in soundcraft_dsp::plugin_info(id).unwrap().params {
        ins.params.insert(p.id.to_string(), p.default);
    }
    ins
}

fn session(pdc: bool) -> Session {
    let mut s = Session::default();
    s.edit.delay_compensation = pdc;
    s.tempo.set_tempo(960, 150.0).unwrap();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let len = 24_000usize;
    let signal: Vec<f32> = (0..len).map(|i| if i < 12_000 { (i as f32 * 0.03).sin() * 0.4 } else { 0.0 }).collect();
    s.pool.insert(SourceId(1), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![signal; 2] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(1), 0, 0, len as i64));
    s.busses.push(Bus { id: VERB, name: "Verb".into(), format: ChannelFormat::Stereo });
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(VERB);
    s.track_mut(aux).unwrap().mixer.inserts[0] = Some(builtin("plate_reverb"));
    let tr = s.track_mut(t).unwrap();
    // The maximizer reports latency: delay compensation moves the dry tracks and the bus.
    tr.mixer.inserts[0] = Some(builtin("maximizer"));
    tr.mixer.sends[0] =
        Some(SendSlot { target: Route::Bus(VERB), level_db: -6.0, pan: 0.0, mute: false, pre_fader: false, follow_main_pan: true, surround: None });
    let lane = tr.lane_mut(&AutoParam::Volume);
    lane.set_point(0, 0.0);
    lane.set_point(9_000, -12.0);
    lane.set_point(18_000, -3.0);
    let inst = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let mut seq = Sequence::default();
    for (i, p) in [60u8, 64, 67, 72].into_iter().enumerate() {
        seq.notes.push(Note { pitch: p, velocity: 100, release_velocity: 64, channel: 0, start: i as i64 * 480, length: 700 });
    }
    let cid = s.new_clip_id();
    let tr = s.track_mut(inst).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 48_000, seq));
    tr.instrument = Some(builtin("subtractive_synth"));
    s
}

fn assert_same(what: &str, plan: &[Vec<f32>], legacy: &[Vec<f32>]) {
    assert_eq!(plan.len(), legacy.len(), "{what}: channels");
    for (c, (p, l)) in plan.iter().zip(legacy).enumerate() {
        assert_eq!(p.len(), l.len(), "{what}: channel {c} length");
        let worst = p.iter().zip(l).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        assert!(worst <= 1e-6, "{what}: channel {c} differs by {worst}");
        // The old renderer matched the plan path bit for bit on every parity session; keep it so.
        assert!(p.iter().zip(l).all(|(x, y)| x.to_bits() == y.to_bits()), "{what}: channel {c} is equal within 1e-6 but not bit for bit");
    }
}

#[test]
fn the_plan_bounce_equals_the_legacy_bounce() {
    for pdc in [true, false] {
        let s = session(pdc);
        for (range, block) in [(Range::new(0, 30_000), 512), (Range::new(0, 30_000), 100), (Range::new(4_321, 26_001), 1024)] {
            let main = render_range(&s, range, block);
            assert!(main.iter().flatten().any(|v| v.abs() > 1e-3), "the session makes sound");
            assert_same(&format!("main, pdc {pdc}, {range:?}, block {block}"), &main, &render_legacy(&s, None, range, block));
            let bus = render_bus(&s, VERB, range, block);
            assert!(bus.iter().flatten().any(|v| v.abs() > 1e-4), "the bus carries the send");
            assert_same(&format!("bus, pdc {pdc}, {range:?}, block {block}"), &bus, &render_legacy(&s, Some(VERB), range, block));
        }
    }
}

/// The two renderers do not differ today (the bit-exact check above); the streamed render is
/// exactly the range long whatever the delay compensation skipped, also for a block size that
/// does not divide it.
#[test]
fn a_streamed_bounce_is_exactly_the_range_long() {
    let s = session(true);
    let mut stream = RangeRender::new(&s, None, Range::new(0, 30_000), 777);
    let mut total = 0;
    while let Some((_, n)) = stream.next_block() {
        total += n;
    }
    assert_eq!(total, 30_000);
}

/// An inactive track with an instrument and an insert is never processed: the bounce equals the
/// legacy one, and equals the bounce of the session without that track.
#[test]
fn an_inactive_hosted_track_changes_nothing_in_the_bounce() {
    let base = session(true);
    let mut s = base.clone();
    let idle = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    let mut seq = Sequence::default();
    seq.notes.push(Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 4_000 });
    let cid = s.new_clip_id();
    let tr = s.track_mut(idle).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 48_000, seq));
    tr.instrument = Some(builtin("subtractive_synth"));
    tr.mixer.inserts[0] = Some(builtin("maximizer"));
    tr.inactive = true;
    let range = Range::new(0, 30_000);
    let plan = render_range(&s, range, 512);
    assert!(plan.iter().flatten().any(|v| v.abs() > 1e-3));
    assert_same("inactive, plan against legacy", &plan, &render_legacy(&s, None, range, 512));
    assert_same("inactive, against the session without the track", &plan, &render_range(&base, range, 512));
}
