//! The production monitor path, without an audio device.
#![forbid(unsafe_code)]

use soundcraft_playback::record::{InputRing, MonitorTelemetry};
use soundcraft_rt::testing::measure;

#[global_allocator]
static ALLOC: soundcraft_rt::testing::CountingAlloc = soundcraft_rt::testing::CountingAlloc;

#[test]
fn uneven_equal_rate_partitions_preserve_every_channel_through_wraparound() {
    let ring = InputRing::new(2, 48_000);
    let mut input = vec![0.0; 1024];
    let mut output = vec![vec![0.0; 512]; 2];
    let ((), stats) = measure(|| {
        for block in 0..200 {
            for (i, sample) in input.iter_mut().enumerate() {
                *sample = (block * 1024 + i) as f32;
            }
            ring.push(&input);
            let mut offset = 0;
            for quantum in [1, 7, 64, 3, 128, 64, 245] {
                ring.pop_into(&mut output, quantum, 48_000);
                for frame in 0..quantum {
                    for channel in 0..2 {
                        assert_eq!(output[channel][frame], input[(offset + frame) * 2 + channel]);
                    }
                }
                offset += quantum;
            }
            assert_eq!(offset, 512);
        }
    });
    assert!(stats.is_zero(), "{stats:?}");
    assert_eq!(ring.telemetry(), MonitorTelemetry::default());
}

#[test]
fn exact_one_frame_reads_need_no_extra_lookahead() {
    let ring = InputRing::new(2, 48_000);
    let mut output = vec![vec![0.0]; 2];
    for i in 0..100 {
        ring.push(&[i as f32, -(i as f32)]);
        ring.pop_into(&mut output, 1, 48_000);
        assert_eq!(output[0][0], i as f32);
        assert_eq!(output[1][0], -(i as f32));
    }
    assert_eq!(ring.telemetry(), MonitorTelemetry::default());
}

#[test]
fn interpolation_is_invariant_to_output_partitioning() {
    for (input_rate, output_rate) in [(44_100, 48_000), (48_000, 44_100), (96_000, 48_000)] {
        let whole = InputRing::new(2, input_rate);
        let split = InputRing::new(2, input_rate);
        let input: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.1).sin()).collect();
        whole.push(&input);
        split.push(&input);
        let mut expected = vec![vec![0.0; 512]; 2];
        let mut actual = vec![vec![0.0; 127]; 2];
        whole.pop_into(&mut expected, 512, output_rate);
        let mut offset = 0;
        for quantum in [1, 127, 3, 64, 127, 63, 127] {
            split.pop_into(&mut actual, quantum, output_rate);
            for channel in 0..2 {
                assert_eq!(actual[channel][..quantum], expected[channel][offset..offset + quantum]);
            }
            offset += quantum;
        }
        assert_eq!(offset, 512);
        assert_eq!(split.telemetry(), MonitorTelemetry::default());
    }
}

#[test]
fn overflow_is_counted_and_resynchronizes_without_heap_work() {
    let ring = InputRing::new(2, 48_000);
    let input = vec![0.25; 2 * 30_000];
    let mut output = vec![vec![1.0; 64]; 2];
    let ((), stats) = measure(|| {
        ring.push(&input);
        ring.pop_into(&mut output, 64, 48_000);
    });
    assert!(stats.is_zero(), "{stats:?}");
    assert!(output.iter().flatten().all(|&x| x == 0.0));
    let t = ring.telemetry();
    assert_eq!(t.overflow_frames, 6_000);
    assert_eq!(t.discarded_frames, 24_000);
    assert_eq!(t.overflow_recoveries, 1);
    assert_eq!(t.underrun_frames, 64);
    ring.push(&[0.5, -0.5]);
    ring.pop_into(&mut output, 1, 48_000);
    assert_eq!(output[0][0], 0.5);
    assert_eq!(output[1][0], -0.5);
}

#[test]
fn clear_discards_interpolation_history_and_incomplete_frames_never_shift_channels() {
    let ring = InputRing::new(2, 24_000);
    let mut output = vec![vec![0.0; 2]; 2];
    ring.push(&[0.1, -0.1, 0.2, -0.2]);
    ring.pop_into(&mut output, 2, 48_000);
    ring.clear_monitor();
    ring.push(&[0.5, -0.5, 123.0]);
    ring.pop_into(&mut output, 1, 48_000);
    assert_eq!(output[0][0], 0.5);
    assert_eq!(output[1][0], -0.5);
    assert_eq!(ring.telemetry().incomplete_input_samples, 1);
    assert_eq!(ring.telemetry().discarded_frames, 1);
}

#[test]
fn simultaneous_input_and_output_keep_fifo_order() {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    let ring = InputRing::new(1, 48_000);
    ring.push(&(0..256).map(|i| i as f32).collect::<Vec<_>>());
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let input_ring = Arc::clone(&ring);
    let producer = std::thread::spawn(move || {
        let mut input = vec![0.0; 256];
        let mut heap_free = true;
        for block in 1..=1000 {
            for (i, x) in input.iter_mut().enumerate() {
                *x = (block * 256 + i) as f32;
            }
            if start_rx.recv_timeout(Duration::from_secs(5)).is_err() {
                return false;
            }
            let ((), stats) = measure(|| input_ring.push(&input));
            heap_free &= stats.is_zero();
            if done_tx.try_send(()).is_err() {
                return false;
            }
        }
        heap_free
    });
    let mut output = vec![vec![0.0; 256]];
    let mut ordered = true;
    let mut heap_free = true;
    for block in 0..1000 {
        start_tx.try_send(()).unwrap();
        let ((), stats) = measure(|| ring.pop_into(&mut output, 256, 48_000));
        heap_free &= stats.is_zero();
        for (i, &x) in output[0].iter().enumerate() {
            ordered &= x == (block * 256 + i) as f32;
        }
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    // Rendezvous is outside measured callback work and has a failure-only watchdog.
    // Disconnect/timeout exits the producer if the test fails on the control thread.
    assert!(producer.join().unwrap());
    assert!(heap_free);
    assert!(ordered);
    assert_eq!(ring.telemetry(), MonitorTelemetry::default());
}

#[test]
fn a_subcapacity_stall_exposes_its_persistent_backlog_until_explicit_clear() {
    let ring = InputRing::new(2, 48_000);
    let input = vec![0.25; 512];
    let mut output = vec![vec![0.0; 256]; 2];
    ring.push(&input);
    ring.pop_into(&mut output, 256, 48_000);
    assert_eq!(ring.buffered_input_frames(), 0);
    for _ in 0..20 {
        ring.push(&input);
    }
    for _ in 0..1000 {
        ring.push(&input);
        ring.pop_into(&mut output, 256, 48_000);
        assert_eq!(ring.buffered_input_frames(), 5120);
    }
    assert!(ring.fifo_capacity_frames() > 5120);
    assert_eq!(ring.telemetry(), MonitorTelemetry::default());
    ring.clear_monitor();
    assert_eq!(ring.buffered_input_frames(), 0);
    assert_eq!(ring.telemetry().discarded_frames, 5120);
    ring.push(&[0.75, -0.75]);
    ring.pop_into(&mut output, 1, 48_000);
    assert_eq!((output[0][0], output[1][0]), (0.75, -0.75));
}

#[test]
fn output_storage_limits_consumption_and_reports_unserviceable_requests() {
    let ring = InputRing::new(2, 48_000);
    ring.push(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0]);
    ring.pop_into(&mut [], 5, 48_000);
    assert_eq!(ring.buffered_input_frames(), 4);
    let mut short = vec![vec![9.0], vec![9.0; 3]];
    ring.pop_into(&mut short, 4, 48_000);
    assert_eq!(short, vec![vec![1.0], vec![-1.0, 0.0, 0.0]]);
    assert_eq!(ring.buffered_input_frames(), 3);
    assert_eq!(ring.telemetry().short_output_frames, 8);
    assert_eq!(ring.telemetry().underrun_frames, 0);
    let mut rest = vec![vec![0.0; 3]; 2];
    ring.pop_into(&mut rest, 3, 48_000);
    assert_eq!(rest, vec![vec![2.0, 3.0, 4.0], vec![-2.0, -3.0, -4.0]]);
    assert_eq!(ring.buffered_input_frames(), 0);
}

#[test]
fn idle_clear_counts_an_overflow_recovery_exactly_once() {
    let ring = InputRing::new(1, 48_000);
    ring.push(&vec![0.25; ring.fifo_capacity_frames() + 435]);
    ring.clear_monitor();
    assert_eq!(ring.telemetry().overflow_frames, 435);
    assert_eq!(ring.telemetry().overflow_recoveries, 1);
    assert_eq!(ring.telemetry().discarded_frames, ring.fifo_capacity_frames() as u64);
    assert_eq!(ring.buffered_input_frames(), 0);
    ring.clear_monitor();
    ring.pop_into(&mut [vec![0.0; 16]], 16, 48_000);
    assert_eq!(ring.telemetry().overflow_recoveries, 1);
}

#[test]
fn concurrent_publication_reads_and_clears_conserve_aligned_ordered_frames() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};
    let ring = InputRing::new(3, 48_000);
    let done = Arc::new(AtomicBool::new(false));
    let input_ring = Arc::clone(&ring);
    let input_done = Arc::clone(&done);
    let producer = std::thread::spawn(move || {
        let mut input = vec![0.0; 509 * 3];
        let mut produced = 0u64;
        let mut heap_free = true;
        for block in 0..4096 {
            let frames = [37, 1, 509, 13][block % 4];
            for (f, frame) in input[..frames * 3].as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let id = (produced + f as u64 + 1) as f32 * 4.0;
                frame.copy_from_slice(&[id, id + 1.0, id + 2.0]);
            }
            let ((), stats) = measure(|| input_ring.push(&input[..frames * 3]));
            heap_free &= stats.is_zero();
            produced += frames as u64;
            std::thread::yield_now();
        }
        input_done.store(true, Ordering::Release);
        (produced, heap_free)
    });
    let clear_ring = Arc::clone(&ring);
    let clearer = std::thread::spawn(move || {
        let mut heap_free = true;
        // Finite work: no barrier or unbounded wait that can strand a failing test.
        for _ in 0..4096 {
            let ((), stats) = measure(|| clear_ring.clear_monitor());
            heap_free &= stats.is_zero();
            std::thread::yield_now();
        }
        heap_free
    });
    let mut output = vec![vec![0.0; 400]; 3];
    let (mut consumed, mut last, mut ordered, mut heap_free) = (0u64, 0.0, true, true);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut calls = 0;
    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
        let frames = [53, 1, 255, 7, 400][calls % 5];
        let ((), stats) = measure(|| ring.pop_into(&mut output, frames, 48_000));
        heap_free &= stats.is_zero();
        for ((&a, &b), &c) in output[0].iter().zip(&output[1]).zip(&output[2]).take(frames) {
            if a == 0.0 {
                ordered &= b == 0.0 && c == 0.0;
            } else {
                ordered &= a > last && a % 4.0 == 0.0 && b == a + 1.0 && c == a + 2.0;
                last = a;
                consumed += 1;
            }
        }
        calls += 1;
        std::thread::yield_now();
    }
    let finished = done.load(Ordering::Acquire);
    let (produced, producer_heap_free) = producer.join().unwrap();
    let clearer_heap_free = clearer.join().unwrap();
    ring.clear_monitor();
    let t = ring.telemetry();
    assert!(finished, "producer exceeded the failure watchdog");
    assert!(ordered, "channel alignment or strict frame order changed");
    assert!(heap_free && producer_heap_free && clearer_heap_free);
    assert_eq!(ring.buffered_input_frames(), 0);
    assert_eq!(produced, consumed + t.discarded_frames + t.overflow_frames + t.input_contention_frames + t.input_fault_frames);
}
