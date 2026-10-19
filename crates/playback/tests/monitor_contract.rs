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
    use std::sync::{Arc, Barrier};
    let ring = InputRing::new(1, 48_000);
    ring.push(&(0..256).map(|i| i as f32).collect::<Vec<_>>());
    let gate = Arc::new(Barrier::new(2));
    let input_ring = Arc::clone(&ring);
    let input_gate = Arc::clone(&gate);
    let producer = std::thread::spawn(move || {
        let mut input = vec![0.0; 256];
        let mut heap_free = true;
        for block in 1..=1000 {
            for (i, x) in input.iter_mut().enumerate() {
                *x = (block * 256 + i) as f32;
            }
            input_gate.wait();
            let ((), stats) = measure(|| input_ring.push(&input));
            heap_free &= stats.is_zero();
            input_gate.wait();
        }
        heap_free
    });
    let mut output = vec![vec![0.0; 256]];
    let mut ordered = true;
    let mut heap_free = true;
    for block in 0..1000 {
        gate.wait();
        let ((), stats) = measure(|| ring.pop_into(&mut output, 256, 48_000));
        heap_free &= stats.is_zero();
        for (i, &x) in output[0].iter().enumerate() {
            ordered &= x == (block * 256 + i) as f32;
        }
        gate.wait();
    }
    // Assertions happen after all rendezvous, so a mismatch cannot strand the other thread.
    assert!(producer.join().unwrap());
    assert!(heap_free);
    assert!(ordered);
    assert_eq!(ring.telemetry(), MonitorTelemetry::default());
}
