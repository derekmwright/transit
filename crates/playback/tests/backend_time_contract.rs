//! Hermetic arithmetic contract for the exact CPAL fork used by playback.
//! This does not certify backend timestamp quality or a shared origin between streams.
#![forbid(unsafe_code)]

use cpal::StreamInstant;
use std::time::Duration;

#[test]
fn an_estimated_capture_before_zero_never_becomes_a_future_timestamp() {
    let callback = StreamInstant::new(0, 0);
    for rate in [44_100, 48_000, 96_000] {
        for frames in [32, 64, 128, 256, 8192] {
            let delay = Duration::from_secs_f64(f64::from(frames) / f64::from(rate));
            let capture = callback.sub(delay).unwrap();
            assert!(capture < callback, "rate={rate}, frames={frames}, capture={capture:?}");
            assert_eq!(callback.duration_since(&capture), Some(delay));
            assert_eq!(capture.add(delay), Some(callback));
        }
    }
}

#[test]
fn fractional_signed_time_carries_borrows_and_round_trips() {
    let before = StreamInstant::new(-1, 500_000_000);
    assert_eq!(before.sub(Duration::from_secs(1)), Some(StreamInstant::new(-2, 500_000_000)));
    assert_eq!(before.add(Duration::from_millis(250)), Some(StreamInstant::new(-1, 750_000_000)));
    assert_eq!(before.add(Duration::from_millis(750)), Some(StreamInstant::new(0, 250_000_000)));
    let one_ns_before_zero = StreamInstant::new(-1, 999_999_999);
    assert_eq!(one_ns_before_zero.add(Duration::from_nanos(1)), Some(StreamInstant::new(0, 0)));
    assert_eq!(StreamInstant::new(0, 0).sub(Duration::from_nanos(1)), Some(one_ns_before_zero));
}

#[test]
fn fractional_range_overflow_returns_none() {
    let min = StreamInstant::new(i64::MIN, 0);
    let max = StreamInstant::new(i64::MAX, 999_999_999);
    assert_eq!(min.sub(Duration::from_nanos(1)), None);
    assert_eq!(StreamInstant::new(i64::MIN, 1).sub(Duration::from_nanos(1)), Some(min));
    assert_eq!(max.add(Duration::from_nanos(1)), None);
    assert_eq!(StreamInstant::new(i64::MAX, 999_999_998).add(Duration::from_nanos(1)), Some(max));
    assert_eq!(StreamInstant::new(0, 0).add(Duration::MAX), None);
    assert_eq!(StreamInstant::new(0, 0).sub(Duration::MAX), None);
}
