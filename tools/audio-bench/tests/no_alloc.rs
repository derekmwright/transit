//! Neither harness callback allocates, frees or locks while it runs (ADR 0016, G9), checked
//! with the counting allocator over many blocks that include whole clicks and their detection.
#![forbid(unsafe_code)]

use audio_bench::probe::{ClickTemplate, Shared, capture_callback, render_callback};
use soundcraft_audio_backend as ab;
use soundcraft_audio_backend::Backend as _;
use soundcraft_audio_backend::fake::{FakeBackend, SliceSink, SliceSource};
use soundcraft_rt::testing::measure;

#[global_allocator]
static ALLOC: soundcraft_rt::testing::CountingAlloc = soundcraft_rt::testing::CountingAlloc;

const RATE: u32 = 48_000;
const PERIOD: usize = 64;
const DELAY: usize = 100;

fn stamp(ns: u64) -> ab::Stamp {
    ab::Stamp { host_callback_ns: ns, backend_callback_ns: 0, backend_frame_ns: 0, quality: ab::Quality::Arrival, offset_uncertainty_ns: None }
}

#[test]
fn the_callbacks_allocate_nothing_and_still_find_the_clicks() {
    let format = ab::Format::new(RATE, 2, ab::SampleFormat::F32);
    let fake = FakeBackend::new(format).with_capture(2, false);
    let shared = Shared::new(RATE, 2.0);
    shared.render_click.set(ClickTemplate::new(RATE).unwrap()).ok().unwrap();
    shared.capture_click.set(ClickTemplate::new(RATE).unwrap()).ok().unwrap();
    let render_plan = fake.plan_render(&ab::OutputWish::default()).unwrap();
    let capture_plan = fake.plan_capture(&ab::InputWish::default()).unwrap();
    let (render_stream, _) =
        fake.open_render(&render_plan.endpoint, &render_plan.attempts[0], render_callback(shared.clone(), 0, true), Box::new(|_| {})).unwrap();
    let (capture_stream, _) =
        fake.open_capture(&capture_plan.endpoint, &capture_plan.attempts[0], capture_callback(shared.clone(), 0), Box::new(|_| {})).unwrap();
    render_stream.start().unwrap();
    capture_stream.start().unwrap();

    let blocks = 2 * RATE as usize / PERIOD;
    let mut render = vec![0.0f32; PERIOD * 2];
    let mut capture = vec![0.0f32; PERIOD * 2];
    let mut line = vec![0.0f32; (blocks + 2) * PERIOD + DELAY];
    let ((), stats) = measure(|| {
        for k in 0..blocks {
            let now = 1_000_000 + k as u64 * 1_333_333;
            assert!(fake.pump_render(PERIOD, stamp(now), &mut SliceSink { data: &mut render, channels: 2 }));
            for (i, frame) in render.as_chunks::<2>().0.iter().enumerate() {
                line[k * PERIOD + DELAY + i] = frame[0];
            }
            for (i, frame) in capture.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                frame[0] = line[k * PERIOD + i];
            }
            assert!(fake.pump_capture(PERIOD, stamp(now), format, &mut SliceSource { data: &capture, channels: 2 }));
        }
    });
    assert_eq!((stats.allocs, stats.frees), (0, 0), "{stats:?}");
    // The detector ran inside the measured region: it found the clicks of two seconds.
    let found = shared.detections.snapshot();
    assert_eq!(found.len(), 4, "{found:?}");
    for (k, at) in found.iter().enumerate() {
        assert_eq!(*at, 12_000 + 24_000 * k as u64 + DELAY as u64);
    }
}
