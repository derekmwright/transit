#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::timeline_map::{self, Admission, History, MapError, PlacementError, Rate, Timeline};
use soundcraft_rt::testing::measure;

fn key(generation: u64, direction: Direction) -> ClockKey {
    ClockKey { generation, domain: generation + 100, direction }
}
fn instant(ns: i64) -> cpal::StreamInstant {
    cpal::StreamInstant::new(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000) as u32)
}

fn fake_clock(key: ClockKey, nominal: u32, actual: f64, partitions: &[usize], epoch: i64, backend_scale: f64) -> (ClockMap, u64) {
    let (observer, mut records) = observation_lane(key, nominal);
    let mut adapter = CpalObserver::new(observer);
    let mut fitter = ClockFitter::new(key);
    let mut frame = 0u64;
    let mut latest_host = 0;
    for &length in partitions {
        let first_ns = (frame as f64 * 1e9 / actual).round() as i64;
        let callback_ns = match key.direction {
            Direction::Input => first_ns + (length as f64 * 1e9 / actual).round() as i64 + 1_000_000,
            Direction::Output => first_ns - 3_000_000,
        };
        let backend = |ns: i64| instant(epoch + (ns as f64 * backend_scale).round() as i64);
        latest_host = (1_000_000_000 + callback_ns) as u64;
        let (_, stats) = measure(|| match key.direction {
            Direction::Input => adapter.input(
                length,
                &cpal::InputCallbackInfo::new(cpal::InputStreamTimestamp { callback: backend(callback_ns), capture: backend(first_ns) }),
                Some(latest_host),
            ),
            Direction::Output => adapter.output(
                length,
                &cpal::OutputCallbackInfo::new(cpal::OutputStreamTimestamp { callback: backend(callback_ns), playback: backend(first_ns) }),
                Some(latest_host),
            ),
        });
        assert!(stats.is_zero());
        let observation = records.pop().unwrap();
        assert_eq!(observation.first_frame, frame);
        let _ = fitter.observe(observation);
        frame += length as u64;
    }
    let map = fitter.map().unwrap();
    assert_eq!(map.quality, Quality::BackendEstimate);
    assert_eq!(map.offset_uncertainty_ns, None, "numeric timestamps do not certify ADC/DAC offsets");
    (map, latest_host)
}

#[test]
fn independent_device_schedules_place_capture_impulse_and_midi_across_loop_wraps() {
    let input_key = key(1, Direction::Input);
    let output_key = key(2, Direction::Output);
    for (input_parts, output_parts) in [(vec![147, 441, 1, 109, 512, 117, 448], vec![64, 256, 31, 512, 100, 777]), (vec![256; 8], vec![128; 16])] {
        let (input, input_now) = fake_clock(input_key, 44_100, 44_100.0, &input_parts, -4_000_000_000_000, 0.9999);
        let (output, output_now) = fake_clock(output_key, 48_000, 48_000.0, &output_parts, 9_000_000_000_000, 1.0001);
        let now = input_now.max(output_now);
        let mut history = History::default();
        let mut timeline = Timeline::new(output_key, 1, 0, 0, Rate::new(1, 1).unwrap()).unwrap();
        timeline.change(2, None, Rate::new(1, 1).unwrap(), true, Some((0, 256))).unwrap();
        for &frames in &output_parts {
            let (result, stats) = measure(|| timeline.advance(frames as u32, |s| history.push(s).unwrap()));
            result.unwrap();
            assert!(stats.is_zero());
        }
        // The synthetic captured sample is found by ordinal, independent of capture/writer chunking.
        let mut capture = vec![0.0f32; input_parts.iter().sum()];
        capture[441] = 1.0;
        let impulse_frame = capture.iter().position(|&v| v == 1.0).unwrap() as u64;
        let captured_at = input.frame_to_host(input_key, impulse_frame, now).unwrap();
        assert!(captured_at.abs_diff(1_010_000_000) <= 2);
        let impulse = timeline_map::place_host_event(captured_at, output_key, 2, &output, &history, now).unwrap();
        assert_eq!((impulse.output_frame, impulse.session.sample, impulse.loop_iteration), (480, 224, 1));
        let midi = timeline_map::place_host_event(1_020_000_000, output_key, 2, &output, &history, now).unwrap();
        assert_eq!((midi.output_frame, midi.session.sample, midi.loop_iteration), (960, 192, 3));
        assert_eq!(midi.original_host_ns, 1_020_000_000);
    }
}

#[test]
fn independent_drift_is_fitted_instead_of_equating_nominal_rates() {
    let k = key(3, Direction::Output);
    let (map, now) = fake_clock(k, 48_000, 48_012.0, &[480; 16], i64::MAX - 1_000_000_000, 1.0002);
    assert!((map.estimated_rate() - 48_012.0).abs() < 0.01);
    assert_eq!(map.host_to_frame(k, 1_100_000_000, now).unwrap(), 4801);
    assert!(map.residual_ns < 2.0);
}

#[test]
fn timeline_preserves_fractional_phase_across_partitions_speed_seek_and_stop() {
    let k = key(4, Direction::Output);
    let rate = Rate::new(147, 160).unwrap();
    let mut timeline = Timeline::new(k, 1, 0, 0, rate).unwrap();
    timeline.change(2, None, rate, true, None).unwrap();
    let mut history = History::default();
    for frames in [1, 17, 41, 101] {
        timeline.advance(frames, |s| history.push(s).unwrap()).unwrap();
    }
    assert_eq!(timeline.position().unwrap().sample, 147);
    assert_eq!(timeline.position().unwrap().fraction, 0);
    timeline.change(3, None, Rate::new(1, 3).unwrap(), true, None).unwrap();
    timeline.advance(2, |s| history.push(s).unwrap()).unwrap();
    let phase = timeline.position().unwrap();
    assert_eq!(phase.sample, 147);
    assert_eq!(phase.fraction * 3, phase.denominator * 2);
    timeline.change(4, None, Rate::new(1, 2).unwrap(), false, None).unwrap();
    timeline.advance(50, |s| history.push(s).unwrap()).unwrap();
    assert_eq!(timeline.position().unwrap(), phase);
    timeline.change(5, Some(1000), Rate::new(1, 2).unwrap(), true, None).unwrap();
    timeline.advance(10, |s| history.push(s).unwrap()).unwrap();
    assert_eq!(history.locate(k, 5, 212).unwrap().0.sample, 1000);
    assert_eq!(history.locate(k, 4, 212), Err(MapError::WrongGeneration));
    assert_eq!(history.locate(k, 2, 160), Err(MapError::WrongGeneration), "half-open boundary belongs to the new speed segment");
}

#[test]
fn restart_a_b_a_never_reuses_old_observations_events_or_take_placement() {
    let a = key(10, Direction::Output);
    let b = key(11, Direction::Output);
    let again = key(12, Direction::Output);
    let (old, old_now) = fake_clock(a, 48_000, 48_000.0, &[128; 8], 3_000_000, 1.0);
    let (new, now) = fake_clock(again, 48_000, 48_000.0, &[128; 8], 3_000_000, 1.0);
    assert!(matches!(new.host_to_frame(a, 1_010_000_000, now), Err(Unavailable::WrongClock)));
    assert!(matches!(old.frame_to_host(again, 480, old_now), Err(Unavailable::WrongClock)));
    let mut fitter = ClockFitter::new(again);
    for stale in [a, b] {
        assert!(matches!(
            fitter.observe(Observation {
                key: stale,
                first_frame: 0,
                frames: 128,
                nominal_rate: 48_000,
                stamp: Stamp {
                    host_callback_ns: 1_000_000,
                    backend_callback_ns: 0,
                    backend_frame_ns: 0,
                    quality: Quality::Measured,
                    offset_uncertainty_ns: Some(0)
                }
            }),
            Err(Unavailable::WrongClock)
        ));
    }
    assert_eq!(fitter.counts().stale_generation, 2);
    let mut history = History::default();
    let mut timeline = Timeline::new(again, 1, 0, 0, Rate::new(1, 1).unwrap()).unwrap();
    timeline.change(2, None, Rate::new(1, 1).unwrap(), true, None).unwrap();
    timeline.advance(1024, |s| history.push(s).unwrap()).unwrap();
    assert!(matches!(timeline_map::place_host_event(1_010_000_000, a, 2, &new, &history, now), Err(PlacementError::Clock(Unavailable::WrongClock))));
}

#[test]
fn bounded_publication_preserves_ordinal_after_overflow_and_refuses_arithmetic_wrap() {
    let k = key(5, Direction::Output);
    let (mut producer, mut consumer) = observation_lane(k, 48_000);
    let stamp =
        Stamp { host_callback_ns: 0, backend_callback_ns: 0, backend_frame_ns: 0, quality: Quality::Measured, offset_uncertainty_ns: Some(0) };
    let (_, stats) = measure(|| {
        for _ in 0..OBSERVATIONS + 3 {
            producer.record(64, stamp);
        }
        producer.record(0, stamp);
    });
    assert!(stats.is_zero());
    assert_eq!(consumer.counts(), PublicationCounts { dropped: 3, zero_frames: 1, exhausted: 0 });
    for i in 0..OBSERVATIONS {
        assert_eq!(consumer.pop().unwrap().first_frame, i as u64 * 64);
    }
    assert!(producer.record(1, stamp));
    assert_eq!(consumer.pop().unwrap().first_frame, (OBSERVATIONS as u64 + 3) * 64);
    producer.next_frame = u64::MAX;
    assert!(!producer.record(1, stamp));
    assert!(!producer.record(1, stamp));
    assert_eq!(producer.next_frame(), u64::MAX);
    assert_eq!(consumer.counts().exhausted, 1);
}

#[test]
fn invalid_discontinuous_reordered_stale_and_extreme_observations_fail_explicitly() {
    let k = key(6, Direction::Output);
    let mut fitter = ClockFitter::new(k);
    let make = |i: u64| Observation {
        key: k,
        first_frame: i * 48,
        frames: 48,
        nominal_rate: 48_000,
        stamp: Stamp {
            host_callback_ns: 1_000_000_000 + i * 1_000_000,
            backend_callback_ns: i as i64 * 1_000_000,
            backend_frame_ns: i as i64 * 1_000_000,
            quality: Quality::Measured,
            offset_uncertainty_ns: Some(17),
        },
    };
    for i in 0..4 {
        let _ = fitter.observe(make(i));
    }
    let map = fitter.map().unwrap();
    assert_eq!(map.offset_uncertainty_ns, Some(17));
    assert!(matches!(map.host_to_frame(k, 1_001_000_000, 1_103_000_001), Err(Unavailable::Stale)));
    assert!(matches!(fitter.observe(make(1)), Err(Unavailable::Reordered)));
    assert!(fitter.map().is_some());
    let mut invalid = make(4);
    invalid.stamp.quality = Quality::Invalid;
    assert!(matches!(fitter.observe(invalid), Err(Unavailable::Invalid)));
    assert!(fitter.map().is_none());
    for i in 4..7 {
        let _ = fitter.observe(make(i));
    }
    let mut reset = make(7);
    reset.stamp.backend_callback_ns = -1;
    assert!(matches!(fitter.observe(reset), Err(Unavailable::Discontinuity)));
    assert!(fitter.map().is_none());
    let mut zero = make(8);
    zero.frames = 0;
    assert!(matches!(fitter.observe(zero), Err(Unavailable::Invalid)));
    let mut extreme = make(9);
    extreme.first_frame = u64::MAX;
    assert!(matches!(fitter.observe(extreme), Err(Unavailable::Invalid)));
    assert_eq!(fitter.counts(), FitCounts { invalid: 3, stale_generation: 0, reordered: 1, discontinuities: 1 });
}

#[test]
fn too_many_wraps_refuse_atomically_and_overwritten_history_is_reported() {
    let k = key(7, Direction::Output);
    let mut timeline = Timeline::new(k, 1, 0, 0, Rate::new(1, 1).unwrap()).unwrap();
    timeline.change(2, None, Rate::new(1, 1).unwrap(), true, Some((0, 1))).unwrap();
    let mut count = 0;
    assert_eq!(timeline.advance(65, |_| count += 1), Err(MapError::TooManyBoundaries));
    assert_eq!((count, timeline.next_output()), (0, 0));
    let mut history = History::default();
    for _ in 0..65 {
        timeline.advance(1, |s| history.push(s).unwrap()).unwrap();
    }
    assert_eq!(history.overwritten, 1);
    assert_eq!(history.locate(k, 2, 0), Err(MapError::MissingHistory));
    assert_eq!(history.locate(k, 2, 64).unwrap().1, 64);
}

#[test]
fn lookahead_boundary_and_lateness_are_independent_of_recorded_timestamp() {
    assert_eq!(timeline_map::admit(90, 10, 100, 64), Ok(Admission::Ready { offset: 0, late_frames: 0 }));
    assert_eq!(timeline_map::admit(154, 10, 100, 64), Ok(Admission::Future));
    assert_eq!(timeline_map::admit(80, 10, 100, 64), Ok(Admission::Ready { offset: 0, late_frames: 10 }));
    assert_eq!(timeline_map::admit(u64::MAX, 1, 0, 64), Err(MapError::Arithmetic));
    assert_eq!(timeline_map::admit(100, 0, 100, 0), Ok(Admission::Future));
}

#[test]
fn fractional_loop_overshoot_survives_callback_boundaries_and_the_next_cursor_is_wrapped() {
    let k = key(8, Direction::Output);
    for parts in [vec![16], vec![1, 3, 1, 2, 5, 4]] {
        let rate = Rate::new(147, 160).unwrap();
        let mut timeline = Timeline::new(k, 1, 0, 100, rate).unwrap();
        timeline.change(2, None, rate, true, Some((100, 103))).unwrap();
        let mut history = History::default();
        for frames in parts {
            timeline.advance(frames, |s| history.push(s).unwrap()).unwrap();
        }
        for frame in 0..16 {
            let (position, iteration) = history.locate(k, 2, frame).unwrap();
            let numerator = frame * 147;
            assert_eq!(iteration, numerator / 480);
            assert_eq!(position.sample, 100 + (numerator % 480 / 160) as i64);
            assert_eq!(position.fraction, numerator % 160);
        }
        let next = timeline.position().unwrap();
        assert_eq!((next.sample, next.fraction, next.denominator), (102, 112, 160));
    }
    let mut timeline = Timeline::new(k, 1, 0, 0, Rate::new(1, 1).unwrap()).unwrap();
    timeline.change(2, None, Rate::new(1, 1).unwrap(), true, Some((0, 128))).unwrap();
    timeline.advance(128, |_| {}).unwrap();
    assert_eq!(timeline.position().unwrap().sample, 0);
}

#[test]
fn timestamp_extraction_does_not_fabricate_host_time_and_checks_signed_range() {
    let k = key(9, Direction::Input);
    let (observer, mut observations) = observation_lane(k, 48_000);
    let mut adapter = CpalObserver::new(observer);
    adapter.input(
        0,
        &cpal::InputCallbackInfo::new(cpal::InputStreamTimestamp {
            callback: cpal::StreamInstant::new(i64::MIN, 0),
            capture: cpal::StreamInstant::new(i64::MIN, 0),
        }),
        Some(0),
    );
    assert!(adapter.backend_origin.is_none(), "an empty callback must not establish an epoch");
    adapter.input(128, &cpal::InputCallbackInfo::new(cpal::InputStreamTimestamp { callback: instant(0), capture: instant(-2_666_667) }), None);
    let invalid = observations.pop().unwrap();
    assert_eq!(invalid.stamp.quality, Quality::Invalid);
    assert_eq!(invalid.stamp.backend_frame_ns, -2_666_667);
    assert_eq!(relative_ns(cpal::StreamInstant::new(i64::MAX, 0), cpal::StreamInstant::new(i64::MIN, 0)), None);
    assert_eq!(relative_ns(instant(-1), instant(0)), Some(-1));
}
