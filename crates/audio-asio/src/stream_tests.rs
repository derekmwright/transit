//! Streaming through the real host thread, session and callback thunks against the fake driver of
//! [`crate::com::fake`]: the test thread plays the driver's callback thread, calling the callback
//! table the way a driver does. The callback state is process-wide, so these tests take one lock.

use crate::abi;
use crate::callback::testing;
use crate::com::fake::{FakeLoader, FakeSpec, Observed, entry};
use crate::host::Slot;
use crate::{AsioBackend, AsioEndpoint};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use transit_audio_backend::{self as ab, Backend};
use transit_rt::testing::{CountingAlloc, measure};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

static STREAMING: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    STREAMING.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Rig {
    backend: AsioBackend,
    observed: Arc<Observed>,
    endpoint: ab::Endpoint,
}

fn scarlett() -> FakeSpec {
    let mut spec = FakeSpec::new("Focusrite USB ASIO", 2, 2, abi::ASIOST_INT32_LSB);
    spec.buffer = (16, 1024, 256, 16);
    spec.latencies = (100, 150);
    spec.rates = vec![44_100.0, 48_000.0];
    spec.current_rate = 44_100.0;
    spec
}

fn model12() -> FakeSpec {
    let mut spec = FakeSpec::new("ModelMixer", 12, 10, abi::ASIOST_INT24_LSB);
    spec.buffer = (256, 256, 256, 0);
    spec.latencies = (300, 695);
    spec.rates = vec![44_100.0, 48_000.0];
    spec.current_rate = 44_100.0;
    spec
}

/// The test thread plays the driver and calls back when it likes: the watchdog waits an hour.
fn rig(spec: FakeSpec) -> Rig {
    rig_watching(spec, Duration::from_secs(3600))
}

fn rig_watching(spec: FakeSpec, watchdog_floor: Duration) -> Rig {
    let (loader, observed) = FakeLoader::new(vec![(entry("Driver", 40), spec)]);
    let backend = AsioBackend::with_loader_watching(Box::new(loader), Slot::default(), Duration::from_secs(10), watchdog_floor).expect("backend");
    backend.probe("Driver").expect("probe");
    let endpoint = backend.endpoints(ab::Direction::Output).expect("endpoints").remove(0);
    Rig { backend, observed, endpoint }
}

fn request(rig: &Rig, direction: ab::Direction, rate: u32, period: Option<u32>) -> ab::OpenRequest {
    let info = &rig.endpoint.handle::<AsioEndpoint>().expect("handle").info;
    let channels = info.channels(direction);
    let plan = match direction {
        ab::Direction::Output => rig.backend.plan_render(&ab::OutputWish { device: Some("Driver".into()), rates: vec![rate], channels, period }),
        ab::Direction::Input => rig.backend.plan_capture(&ab::InputWish { device: Some("Driver".into()), rate: Some(rate), period }),
    }
    .expect("plan");
    assert_eq!(plan.attempts[0].format.rate, rate);
    plan.attempts[0]
}

/// What the callbacks saw, in preallocated storage.
#[derive(Default)]
struct Seen {
    render: Mutex<Vec<(ab::Stamp, Option<u64>, usize)>>,
    capture: Mutex<Vec<(ab::Stamp, Option<u64>, Vec<f32>)>>,
    errors: Mutex<Vec<(ab::Direction, ab::StreamError)>>,
}

fn seen() -> Arc<Seen> {
    Arc::new(Seen {
        render: Mutex::new(Vec::with_capacity(256)),
        capture: Mutex::new(Vec::with_capacity(256)),
        errors: Mutex::new(Vec::with_capacity(8)),
    })
}

/// A render callback that writes channel c's sample as `values[c]` and records the block.
fn render_cb(seen: &Arc<Seen>, values: Vec<f32>) -> ab::RenderCallback {
    let seen = Arc::clone(seen);
    Box::new(move |block| {
        let channels = usize::from(block.channels);
        block.render(&mut |chunk| {
            for (i, x) in chunk.iter_mut().enumerate() {
                *x = values[i % channels];
            }
        });
        seen.render.lock().unwrap().push((block.stamp, block.device_position, block.frames));
    })
}

/// A capture callback that records the first frame of each block.
fn capture_cb(seen: &Arc<Seen>) -> ab::CaptureCallback {
    let seen = Arc::clone(seen);
    Box::new(move |block| {
        let mut first = Vec::new();
        let channels = usize::from(block.channels);
        block.chunks(&mut |offset, chunk| {
            if offset == 0 {
                first = chunk[..channels].to_vec();
            }
        });
        seen.capture.lock().unwrap().push((block.stamp, block.device_position, first));
    })
}

fn error_cb(seen: &Arc<Seen>, direction: ab::Direction) -> ab::ErrorCallback {
    let seen = Arc::clone(seen);
    Box::new(move |e| seen.errors.lock().unwrap().push((direction, e)))
}

/// Opens capture then render (the Player's and the bench's order) and starts both.
fn duplex(
    rig: &Rig,
    rate: u32,
    period: Option<u32>,
    seen: &Arc<Seen>,
    values: Vec<f32>,
) -> (Box<dyn ab::Stream>, Box<dyn ab::Stream>, ab::Achieved, ab::Achieved) {
    let (capture, cap) = rig
        .backend
        .open_capture(&rig.endpoint, &request(rig, ab::Direction::Input, rate, period), capture_cb(seen), error_cb(seen, ab::Direction::Input))
        .expect("open capture");
    let (render, ren) = rig
        .backend
        .open_render(
            &rig.endpoint,
            &request(rig, ab::Direction::Output, rate, period),
            render_cb(seen, values),
            error_cb(seen, ab::Direction::Output),
        )
        .expect("open render");
    capture.start().expect("start capture");
    render.start().expect("start render");
    (capture, render, cap, ren)
}

#[test]
fn create_start_stop_and_dispose_run_in_order_on_the_host_thread() {
    let _guard = lock();
    let rig = rig(model12());
    let seen = seen();
    let (capture, render, cap, ren) = duplex(&rig, 44_100, None, &seen, vec![0.0; 10]);
    assert_eq!((cap.format.channels, ren.format.channels), (12, 10), "every channel by default");
    assert_eq!((cap.format.sample, ren.format.sample), (ab::SampleFormat::I24, ab::SampleFormat::I24));
    assert_eq!(ren.period, ab::Period::Frames(256));
    assert_eq!((ren.buffer_frames, ren.min_period), (Some(256), Some(256)));
    let ms = |frames: u32| Duration::from_secs_f64(f64::from(frames) / 44_100.0);
    assert_eq!((cap.latency, ren.latency), (Some(ms(300)), Some(ms(695))), "each direction its own getLatencies figure");
    let names = rig.observed.call_names();
    let streaming: Vec<&str> = names.iter().copied().filter(|n| *n != "getLatencies").collect();
    assert_eq!(streaming, ["createBuffers", "disposeBuffers", "createBuffers", "start"], "the second open rebuilt the buffers for both directions");
    assert_eq!(rig.observed.buffers.lock().unwrap().len(), 22, "one createBuffers call for 12 inputs and 10 outputs");
    assert!(testing::installed());
    render.stop().expect("stop render");
    assert!(!rig.observed.call_names().contains(&"stop"), "the capture still runs");
    capture.stop().expect("stop capture");
    assert_eq!(rig.observed.call_names().last(), Some(&"stop"));
    assert!(capture.start().is_err(), "a stopped stream is reopened, not restarted");
    drop(render);
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 1, "the driver stays loaded for the capture stream");
    drop(capture);
    assert_eq!(rig.observed.call_names().last(), Some(&"disposeBuffers"));
    assert!(!testing::installed(), "the callback state is gone with the session");
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 0, "the driver was released with its last stream");
    let host = rig.observed.calls.lock().unwrap().iter().find(|(n, _)| *n == "createBuffers").map(|(_, t)| *t).expect("a call");
    assert!(rig.observed.calls.lock().unwrap().iter().all(|(_, t)| *t == host), "every control call on one thread");
    assert_ne!(host, std::thread::current().id(), "the host thread's, not the caller's");
}

#[test]
fn render_then_capture_also_works_but_not_after_a_start() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (render, _) = rig
        .backend
        .open_render(
            &rig.endpoint,
            &request(&rig, ab::Direction::Output, 48_000, Some(64)),
            render_cb(&seen, vec![0.0; 2]),
            error_cb(&seen, ab::Direction::Output),
        )
        .expect("render first");
    let (capture, cap) = rig
        .backend
        .open_capture(&rig.endpoint, &request(&rig, ab::Direction::Input, 48_000, Some(64)), capture_cb(&seen), error_cb(&seen, ab::Direction::Input))
        .expect("then capture");
    assert_eq!(cap.period, ab::Period::Frames(64));
    drop(capture);
    render.start().expect("start");
    let late = rig.backend.open_capture(
        &rig.endpoint,
        &request(&rig, ab::Direction::Input, 48_000, Some(64)),
        capture_cb(&seen),
        error_cb(&seen, ab::Direction::Input),
    );
    let text = late.err().expect("refused after a start").to_string();
    assert!(text.contains("before either stream of the driver starts"), "{text}");
    let other_rate = rig.backend.open_capture(
        &rig.endpoint,
        &request(&rig, ab::Direction::Input, 44_100, Some(64)),
        capture_cb(&seen),
        error_cb(&seen, ab::Direction::Input),
    );
    assert!(other_rate.is_err());
    drop(render);
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 0);
}

#[test]
fn the_session_sets_the_rate_and_snaps_the_buffer_size() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (render, achieved) = rig
        .backend
        .open_render(
            &rig.endpoint,
            &request(&rig, ab::Direction::Output, 48_000, Some(70)),
            render_cb(&seen, vec![0.0; 2]),
            error_cb(&seen, ab::Direction::Output),
        )
        .expect("open");
    assert_eq!(achieved.format.rate, 48_000, "the driver was at 44.1 kHz and was set");
    assert_eq!(achieved.period, ab::Period::Frames(64), "70 frames snaps to the 16-frame grid");
    assert_eq!(achieved.format.sample, ab::SampleFormat::I32);
    drop(render);
}

/// Drives `n` buffer switches with time info from position `from`: input channel 0 holds `first`
/// repeated (when it is not empty), the other inputs zeros.
fn drive(rig: &Rig, frames: u64, from: u64, n: u64, first: &[u8]) {
    let mut position = from;
    for _ in 0..n {
        let half = ((position / frames) % 2) as usize;
        for (channel, bytes) in rig.observed.inputs() {
            let data: Vec<u8> = if channel == 0 && !first.is_empty() { first.iter().copied().cycle().take(bytes).collect() } else { vec![0; bytes] };
            rig.observed.write_input(channel, half, &data);
        }
        testing::switch_time_info(half as i32, Some(position), Some(1_000_000 * (position * 1000 / 48_000)));
        position += frames;
    }
}

fn near(a: i64, b: i64, within: i64) -> bool {
    (a - b).abs() <= within
}

#[test]
fn the_callback_converts_stamps_and_counts() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.25, -0.5]);
    let one = (1i32 << 30).to_le_bytes(); // 0.5 in Int32LSB
    let frames = 64u64;
    drive(&rig, frames, 6400, 10, &one);
    let renders = seen.render.lock().unwrap().clone();
    let captures = seen.capture.lock().unwrap().clone();
    assert_eq!((renders.len(), captures.len()), (10, 10));
    assert!(captures.iter().all(|(_, _, first)| first == &[0.5, 0.0]), "input channel 0 read, channel 1 silent");
    let half = |p: u64| ((p / frames) % 2) as usize;
    let last = 6400 + 9 * frames;
    let left = rig.observed.read_output(0, half(last));
    let right = rig.observed.read_output(1, half(last));
    assert_eq!(&left[..4], &((0.25f32 * 2_147_483_648.0) as i32).to_le_bytes());
    assert_eq!(&right[..4], &((-0.5f32 * 2_147_483_648.0) as i32).to_le_bytes());
    let period_ns = 64 * 1_000_000_000 / 48_000;
    for pair in renders.windows(2) {
        assert!(near(pair[1].0.backend_callback_ns - pair[0].0.backend_callback_ns, period_ns, 1), "one buffer of the position apart");
        assert!(pair[1].0.host_callback_ns > pair[0].0.host_callback_ns);
    }
    for ((r, rp, rf), (c, cp, _)) in renders.iter().zip(&captures) {
        assert_eq!((rp, *rf), (cp, 64));
        assert_eq!(r.quality, ab::Quality::BackendEstimate);
        // Each stream maps the shared anchor through the (QPC, Instant) pair of its own start.
        assert!(near(r.backend_callback_ns, c.backend_callback_ns, 100_000), "both directions share the position anchor");
        assert_eq!(r.backend_frame_ns - r.backend_callback_ns, 150 * 1_000_000_000 / 48_000, "presented the output latency later");
        assert_eq!(c.backend_callback_ns - c.backend_frame_ns, 100 * 1_000_000_000 / 48_000, "captured the input latency earlier");
    }
    assert_eq!(renders[0].1, Some(6400));
    let stats = render.stats().expect("stats");
    assert_eq!((stats.wakes, stats.overloads, stats.position_jumps, stats.resets, stats.faults), (10, 0, 0, 0, 0));
    assert_eq!(stats.wake_late_us.iter().sum::<u64>(), 9, "the first switch has nothing to be late against");
    assert_eq!(stats.service_us.iter().sum::<u64>(), 10);
    assert_eq!(capture.stats().expect("stats").wakes, 10);
    assert_eq!(rig.observed.output_ready.load(Ordering::SeqCst), 2 + 10, "probed after each createBuffers, then once per switch");
    drop((capture, render));
}

#[test]
fn a_calibration_makes_both_streams_measured_and_moves_the_capture_frame() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let calibration = ab::Calibration { round_trip_ns: 1_000_000, uncertainty_ns: 20_000 };
    let mut req_in = request(&rig, ab::Direction::Input, 48_000, Some(64));
    let mut req_out = request(&rig, ab::Direction::Output, 48_000, Some(64));
    req_in.calibration = Some(calibration);
    req_out.calibration = Some(calibration);
    let (capture, _) = rig.backend.open_capture(&rig.endpoint, &req_in, capture_cb(&seen), error_cb(&seen, ab::Direction::Input)).expect("in");
    let (render, _) =
        rig.backend.open_render(&rig.endpoint, &req_out, render_cb(&seen, vec![0.0; 2]), error_cb(&seen, ab::Direction::Output)).expect("out");
    capture.start().expect("start");
    render.start().expect("start");
    drive(&rig, 64, 0, 3, &[]);
    let (r, c) = (seen.render.lock().unwrap()[2].0, seen.capture.lock().unwrap()[2].0);
    assert_eq!((r.quality, r.offset_uncertainty_ns), (ab::Quality::Measured, Some(20_000)));
    assert_eq!((c.quality, c.offset_uncertainty_ns), (ab::Quality::Measured, Some(20_000)));
    assert_eq!(r.backend_frame_ns - r.backend_callback_ns, 150 * 1_000_000_000 / 48_000, "the output keeps its presentation time");
    assert_eq!(c.backend_callback_ns - c.backend_frame_ns, 100 * 1_000_000_000 / 48_000 + 1_000_000, "the input carries the round trip");
    drop((capture, render));
}

#[test]
fn every_sample_type_converts_both_ways_through_the_callback() {
    let _guard = lock();
    // (type, bytes of 0.5 as the driver holds it)
    let cases: [(i32, Vec<u8>); 9] = [
        (abi::ASIOST_INT16_LSB, 16_384i16.to_le_bytes().to_vec()),
        (abi::ASIOST_INT24_LSB, 4_194_304i32.to_le_bytes()[..3].to_vec()),
        (abi::ASIOST_INT32_LSB, (1i32 << 30).to_le_bytes().to_vec()),
        (abi::ASIOST_FLOAT32_LSB, 0.5f32.to_le_bytes().to_vec()),
        (abi::ASIOST_FLOAT64_LSB, 0.5f64.to_le_bytes().to_vec()),
        (abi::ASIOST_INT32_LSB16, 16_384i32.to_le_bytes().to_vec()),
        (abi::ASIOST_INT32_LSB18, 65_536i32.to_le_bytes().to_vec()),
        (abi::ASIOST_INT32_LSB20, 262_144i32.to_le_bytes().to_vec()),
        (abi::ASIOST_INT32_LSB24, 4_194_304i32.to_le_bytes().to_vec()),
    ];
    for (sample, half_scale) in cases {
        let mut spec = FakeSpec::new("Typed", 1, 1, sample);
        spec.buffer = (32, 32, 32, 0);
        spec.current_rate = 48_000.0;
        let rig = rig(spec);
        let seen = seen();
        let (capture, render, cap, _) = duplex(&rig, 48_000, None, &seen, vec![0.5]);
        drive(&rig, 32, 0, 1, &half_scale);
        assert_eq!(seen.capture.lock().unwrap()[0].2, [0.5], "type {sample} in ({:?})", cap.format);
        let out = rig.observed.read_output(0, 0);
        assert_eq!(&out[..half_scale.len()], half_scale.as_slice(), "type {sample} out");
        assert_eq!(out.len(), 32 * half_scale.len());
        drop((capture, render));
    }
}

#[test]
fn an_unstarted_render_direction_plays_silence_over_the_drivers_garbage() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, _) = rig
        .backend
        .open_capture(&rig.endpoint, &request(&rig, ab::Direction::Input, 48_000, Some(64)), capture_cb(&seen), error_cb(&seen, ab::Direction::Input))
        .expect("in");
    let (render, _) = rig
        .backend
        .open_render(
            &rig.endpoint,
            &request(&rig, ab::Direction::Output, 48_000, Some(64)),
            render_cb(&seen, vec![0.9, 0.9]),
            error_cb(&seen, ab::Direction::Output),
        )
        .expect("out");
    assert!(rig.observed.read_output(0, 1).iter().all(|b| *b == 0), "outputs are zeroed when created");
    capture.start().expect("start capture only");
    drive(&rig, 64, 0, 2, &[]);
    assert!(seen.render.lock().unwrap().is_empty(), "the render callback waits for its own start");
    assert_eq!(seen.capture.lock().unwrap().len(), 2);
    assert!(rig.observed.read_output(1, 0).iter().all(|b| *b == 0));
    render.start().expect("start render");
    drive(&rig, 64, 128, 1, &[]);
    assert_eq!(seen.render.lock().unwrap().len(), 1);
    drop((capture, render));
}

#[test]
fn a_reset_request_is_one_stream_error_per_stream_and_a_rate_change_is_one_too() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    drive(&rig, 64, 0, 2, &[]);
    assert_eq!(testing::message(abi::K_ASIO_RESET_REQUEST, 0), 1);
    let mut errors = seen.errors.lock().unwrap().clone();
    errors.sort_by_key(|(d, _)| *d == ab::Direction::Output);
    assert_eq!(errors, [(ab::Direction::Input, ab::StreamError::ResetRequested), (ab::Direction::Output, ab::StreamError::ResetRequested)]);
    testing::message(abi::K_ASIO_RESET_REQUEST, 0);
    testing::rate_changed(48_000.0); // the same rate: not a change
    drive(&rig, 64, 128, 1, &[]);
    assert_eq!(seen.errors.lock().unwrap().len(), 2, "one error per stream, however many requests");
    assert_eq!(render.stats().expect("stats").resets, 2);
    drop((capture, render));

    let rig2 = self::rig(scarlett());
    let seen = self::seen();
    let (capture, render, _, _) = duplex(&rig2, 48_000, Some(64), &seen, vec![0.0; 2]);
    testing::rate_changed(44_100.0);
    assert_eq!(seen.errors.lock().unwrap().len(), 2, "a rate change under the streams resets them");
    assert_eq!(capture.stats().expect("stats").resets, 1);
    drop((capture, render));
}

/// Waits up to five seconds for `n` stream errors.
fn wait_for_errors(seen: &Seen, n: usize) -> Vec<(ab::Direction, ab::StreamError)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while seen.errors.lock().unwrap().len() < n && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut errors = seen.errors.lock().unwrap().clone();
    errors.sort_by_key(|(d, _)| *d == ab::Direction::Output);
    errors
}

#[test]
fn a_driver_that_stops_calling_back_is_reported_stalled_once_by_the_watchdog() {
    let _guard = lock();
    // 64 frames at 48 kHz: eight buffers are 10.7 ms, so a 60 ms floor is the limit.
    let rig = rig_watching(scarlett(), Duration::from_millis(60));
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    drive(&rig, 64, 0, 3, &[]);
    let quiet = std::time::Instant::now();
    let errors = wait_for_errors(&seen, 2);
    let waited = quiet.elapsed();
    assert_eq!(errors, [(ab::Direction::Input, ab::StreamError::Stalled), (ab::Direction::Output, ab::StreamError::Stalled)]);
    assert!(waited >= Duration::from_millis(60), "not before the limit: {waited:?}");
    let (rs, cs) = (render.stats().expect("stats"), capture.stats().expect("stats"));
    assert_eq!((rs.timeouts, cs.timeouts, rs.resets), (1, 1, 0), "counted once for the driver, apart from resets");
    // A driver that calls again is not reported again in this run.
    drive(&rig, 64, 3 * 64, 2, &[]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(seen.errors.lock().unwrap().len(), 2);
    assert_eq!(render.stats().expect("stats").timeouts, 1);
    drop((capture, render));
}

#[test]
fn the_watchdog_leaves_a_driver_that_calls_back_and_a_stopped_one_alone() {
    let _guard = lock();
    let rig = rig_watching(scarlett(), Duration::from_millis(250));
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    // The first callback may take up to a second after the start.
    std::thread::sleep(Duration::from_millis(400));
    let mut position = 0;
    let until = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < until {
        drive(&rig, 64, position, 1, &[]);
        position += 64;
        std::thread::sleep(Duration::from_millis(5));
    }
    render.stop().expect("stop");
    capture.stop().expect("stop");
    std::thread::sleep(Duration::from_millis(600));
    assert!(seen.errors.lock().unwrap().is_empty(), "{:?}", seen.errors.lock().unwrap());
    assert_eq!(render.stats().expect("stats").timeouts, 0);
    drop((capture, render));
}

/// Runs buffer switches from a thread of its own (the driver's) every millisecond until `stop`.
fn driver_thread<'s>(
    scope: &'s std::thread::Scope<'s, '_>,
    rig: &'s Rig,
    stop: &'s std::sync::atomic::AtomicBool,
) -> std::thread::ScopedJoinHandle<'s, u32> {
    scope.spawn(move || {
        let mut position = 0;
        while !stop.load(Ordering::SeqCst) {
            drive(rig, 64, position, 1, &[]);
            position += 64;
            std::thread::sleep(Duration::from_millis(1));
        }
        crate::com::current_thread_id()
    })
}

#[test]
fn the_buffer_switch_thread_joins_mmcss_at_its_first_switch_and_leaves_it_at_the_stop() {
    use crate::mmcss::testing as mmcss;
    let _guard = lock();
    mmcss::reset();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    assert_eq!(render.thread_priority(), ab::WorkerPriority::Pending, "no buffer switch yet");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let driver = std::thread::scope(|s| {
        let driver = driver_thread(s, &rig, &stop);
        while mmcss::REGISTERS.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!((render.thread_priority(), capture.thread_priority()), (ab::WorkerPriority::MmcssProAudio, ab::WorkerPriority::MmcssProAudio));
        assert_eq!(mmcss::REGISTERS.load(Ordering::SeqCst), 1, "once per start, not per buffer");
        render.stop().expect("stop render");
        assert_eq!(mmcss::REVERTS.load(Ordering::SeqCst), 0, "the capture keeps the driver running");
        capture.stop().expect("stop capture: the driver stops");
        stop.store(true, Ordering::SeqCst);
        driver.join().unwrap()
    });
    assert_eq!(mmcss::REGISTERED_ON.load(Ordering::SeqCst), driver, "the driver's thread registered itself");
    assert_eq!((mmcss::REVERTS.load(Ordering::SeqCst), mmcss::REVERTED_ON.load(Ordering::SeqCst)), (1, driver), "and reverted on itself");
    assert_eq!(render.thread_priority(), ab::WorkerPriority::Released);
    assert!(seen.errors.lock().unwrap().is_empty());
    drop((capture, render));
}

#[test]
fn a_driver_that_does_not_call_at_the_stop_is_reverted_from_the_host_and_a_restart_registers_again() {
    use crate::mmcss::testing as mmcss;
    let _guard = lock();
    mmcss::reset();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render) = (
        rig.backend
            .open_capture(
                &rig.endpoint,
                &request(&rig, ab::Direction::Input, 48_000, Some(64)),
                capture_cb(&seen),
                error_cb(&seen, ab::Direction::Input),
            )
            .expect("in")
            .0,
        rig.backend
            .open_render(
                &rig.endpoint,
                &request(&rig, ab::Direction::Output, 48_000, Some(64)),
                render_cb(&seen, vec![0.0; 2]),
                error_cb(&seen, ab::Direction::Output),
            )
            .expect("out")
            .0,
    );
    capture.start().expect("start capture");
    drive(&rig, 64, 0, 2, &[]);
    assert_eq!(mmcss::REGISTERS.load(Ordering::SeqCst), 1);
    capture.stop().expect("stop: the driver stops, with no buffer switch to come");
    assert_eq!(mmcss::REVERTS.load(Ordering::SeqCst), 1, "reverted after the wait");
    assert_ne!(mmcss::REVERTED_ON.load(Ordering::SeqCst), crate::com::current_thread_id(), "from the host thread, not the test's");
    assert_eq!(capture.thread_priority(), ab::WorkerPriority::Released);
    // The render direction starts the driver again: its first switch registers again.
    render.start().expect("start render");
    assert_eq!(render.thread_priority(), ab::WorkerPriority::Pending);
    mmcss::FAIL.store(true, Ordering::SeqCst);
    drive(&rig, 64, 128, 2, &[]);
    assert_eq!(render.thread_priority(), ab::WorkerPriority::PromotionFailed, "a refusal is reported, and playback goes on");
    assert_eq!(seen.render.lock().unwrap().len(), 2);
    drop((capture, render));
    assert_eq!(mmcss::REVERTS.load(Ordering::SeqCst), 1, "nothing to revert after a refusal");
}

#[test]
fn a_buffer_switch_thread_the_driver_registered_itself_reads_driver_registered_before_and_after_the_stop() {
    use crate::mmcss::testing as mmcss;
    let _guard = lock();
    mmcss::reset();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    mmcss::DRIVER_HOLDS.store(true, Ordering::SeqCst);
    drive(&rig, 64, 0, 2, &[]);
    assert_eq!((render.thread_priority(), capture.thread_priority()), (ab::WorkerPriority::DriverRegistered, ab::WorkerPriority::DriverRegistered));
    render.stop().expect("stop render");
    capture.stop().expect("stop capture");
    assert_eq!(render.thread_priority(), ab::WorkerPriority::DriverRegistered, "the driver's registration is not ours to end");
    assert_eq!((mmcss::REGISTERS.load(Ordering::SeqCst), mmcss::REVERTS.load(Ordering::SeqCst)), (0, 0));
    assert_eq!(seen.render.lock().unwrap().len(), 2, "playback goes on");
    drop((capture, render));
}

#[test]
fn a_buffer_switch_that_finds_the_last_one_still_running_is_skipped_and_counted() {
    use std::sync::atomic::AtomicBool;
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (inside, release) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (i, r) = (Arc::clone(&inside), Arc::clone(&release));
    let calls = AtomicUsize::new(0);
    let held: ab::CaptureCallback = Box::new(move |block| {
        block.chunks(&mut |_, _| {});
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            i.store(true, Ordering::SeqCst);
            while !r.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
    });
    let (capture, _) = rig
        .backend
        .open_capture(&rig.endpoint, &request(&rig, ab::Direction::Input, 48_000, Some(64)), held, error_cb(&seen, ab::Direction::Input))
        .expect("in");
    let (render, _) = rig
        .backend
        .open_render(
            &rig.endpoint,
            &request(&rig, ab::Direction::Output, 48_000, Some(64)),
            render_cb(&seen, vec![0.0; 2]),
            error_cb(&seen, ab::Direction::Output),
        )
        .expect("out");
    capture.start().expect("start");
    render.start().expect("start");
    std::thread::scope(|s| {
        let first = s.spawn(|| drive(&rig, 64, 0, 1, &[]));
        while !inside.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        drive(&rig, 64, 64, 1, &[]);
        release.store(true, Ordering::SeqCst);
        first.join().unwrap();
    });
    let stats = render.stats().expect("stats");
    assert_eq!((stats.busy_skips, capture.stats().expect("stats").busy_skips), (1, 1), "the driver's count, both directions");
    assert_eq!(seen.render.lock().unwrap().len(), 1, "the skipped switch did nothing");
    assert!(stats.line(false).ends_with("; overloads 0, busy skips 1, position jumps 0, resets 0"), "{}", stats.line(false));
    drop((capture, render));
}

#[test]
fn overloads_resyncs_and_position_jumps_are_counted() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    drive(&rig, 64, 0, 3, &[]);
    testing::message(abi::K_ASIO_OVERLOAD, 0);
    testing::message(abi::K_ASIO_OVERLOAD, 0);
    // A lost buffer: the position skips one period.
    drive(&rig, 64, 4 * 64, 2, &[]);
    assert_eq!(testing::message(abi::K_ASIO_RESYNC_REQUEST, 0), 1);
    drive(&rig, 64, 100_000, 2, &[]);
    let stats = render.stats().expect("stats");
    assert_eq!((stats.overloads, stats.position_jumps, stats.resets), (2, 1, 1), "a resync forgets the position: no jump there");
    assert!(seen.errors.lock().unwrap().is_empty(), "overloads and resyncs are counted, not errors");
    let renders = seen.render.lock().unwrap().clone();
    assert!(renders[..5].windows(2).all(|w| w[1].0.backend_callback_ns > w[0].0.backend_callback_ns), "forward up to the resync");
    let qualities: Vec<ab::Quality> = renders.iter().map(|(s, _, _)| s.quality).collect();
    let (estimate, invalid) = (ab::Quality::BackendEstimate, ab::Quality::Invalid);
    assert_eq!(qualities, [estimate, estimate, estimate, invalid, estimate, invalid, estimate], "the block after the lost one and after the resync");
    let captures: Vec<ab::Quality> = seen.capture.lock().unwrap().iter().map(|(s, _, _)| s.quality).collect();
    assert_eq!(captures, qualities, "both directions restart their fit");
    assert!(stats.line(false).contains("overloads 2, busy skips 0, position jumps 1, resets 1"), "{}", stats.line(false));
    drop((capture, render));
}

#[test]
fn a_position_that_goes_back_invalidates_the_next_stamp_and_reanchors() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    drive(&rig, 64, 6400, 3, &[]);
    drive(&rig, 64, 0, 3, &[]);
    let renders = seen.render.lock().unwrap().clone();
    let qualities: Vec<ab::Quality> = renders.iter().map(|(s, _, _)| s.quality).collect();
    assert_eq!(qualities[3], ab::Quality::Invalid, "the first block after the counter went back");
    assert!(qualities.iter().enumerate().all(|(i, q)| i == 3 || *q == ab::Quality::BackendEstimate), "{qualities:?}");
    let period_ns = 64 * 1_000_000_000 / 48_000;
    assert!(near(renders[5].0.backend_callback_ns - renders[4].0.backend_callback_ns, period_ns, 1), "the new anchor maps the new counter");
    assert!(renders.windows(2).all(|w| w[1].0.host_callback_ns > w[0].0.host_callback_ns));
    drop((capture, render));
}

#[test]
fn a_session_orphaned_while_the_host_was_stuck_is_swept() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let open_render = |rig: &Rig| {
        rig.backend
            .open_render(
                &rig.endpoint,
                &request(rig, ab::Direction::Output, 48_000, Some(64)),
                render_cb(&seen, vec![0.0; 2]),
                error_cb(&seen, ab::Direction::Output),
            )
            .expect("open render")
            .0
    };
    // A stream dropped in the window where the host refuses requests: its detach is never sent,
    // and the driver keeps running with no stream.
    let render = open_render(&rig);
    render.start().expect("start");
    rig.backend.host.set_stuck(true);
    drop(render);
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 1, "nothing told the host");
    rig.backend.host.set_stuck(false);
    // The next open ends that session instead of refusing to attach to a running driver.
    let (capture, _) = rig
        .backend
        .open_capture(&rig.endpoint, &request(&rig, ab::Direction::Input, 48_000, Some(64)), capture_cb(&seen), error_cb(&seen, ab::Direction::Input))
        .expect("the orphaned session is swept first");
    drop(capture);
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 0);
    // With no open coming, the idle host sweeps it by itself.
    let render = open_render(&rig);
    render.start().expect("start");
    rig.backend.host.set_stuck(true);
    drop(render);
    rig.backend.host.set_stuck(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while rig.observed.live.load(Ordering::Acquire) != 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 0, "the host swept the orphan between requests");
    assert_eq!(rig.observed.call_names().last(), Some(&"disposeBuffers"));
    assert!(!testing::installed());
}

#[test]
fn stop_waits_for_a_callback_in_flight_while_the_other_direction_keeps_the_driver_running() {
    use std::sync::atomic::AtomicBool;
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (entered, left) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (e, l) = (Arc::clone(&entered), Arc::clone(&left));
    let calls = AtomicUsize::new(0);
    let slow: ab::CaptureCallback = Box::new(move |block| {
        block.chunks(&mut |_, _| {});
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            e.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            l.store(true, Ordering::SeqCst);
        }
    });
    let (capture, _) = rig
        .backend
        .open_capture(&rig.endpoint, &request(&rig, ab::Direction::Input, 48_000, Some(64)), slow, error_cb(&seen, ab::Direction::Input))
        .expect("in");
    let (render, _) = rig
        .backend
        .open_render(
            &rig.endpoint,
            &request(&rig, ab::Direction::Output, 48_000, Some(64)),
            render_cb(&seen, vec![0.0; 2]),
            error_cb(&seen, ab::Direction::Output),
        )
        .expect("out");
    capture.start().expect("start");
    render.start().expect("start");
    std::thread::scope(|s| {
        s.spawn(|| drive(&rig, 64, 0, 1, &[]));
        while !entered.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        capture.stop().expect("stop capture");
        assert!(left.load(Ordering::SeqCst), "stop returned while the capture callback was still running");
    });
    assert!(!rig.observed.call_names().contains(&"stop"), "the render stream keeps the driver running");
    drive(&rig, 64, 64, 2, &[]);
    assert_eq!(seen.render.lock().unwrap().len(), 3);
    drop((capture, render));
}

/// A stream a callback can reach, taken out at the end so the session can end.
type StreamSlot = Arc<Mutex<Option<crate::AsioStream>>>;

/// Stops the stream in `slot` from inside a callback: how long `stop` took, and whether the
/// driver's `stop` had been called by the time it returned.
fn stop_from_inside(slot: &StreamSlot, observed: &Observed) -> (Duration, bool) {
    let started = std::time::Instant::now();
    let guard = slot.lock().unwrap();
    ab::Stream::stop(guard.as_ref().expect("the stream is in its slot")).expect("a stop from inside the callback");
    (started.elapsed(), observed.call_names().contains(&"stop"))
}

#[test]
fn a_stop_from_inside_the_callback_returns_at_once_and_the_host_stops_the_driver_after_the_switch() {
    use crate::session::Callback;
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (render_slot, capture_slot): (StreamSlot, StreamSlot) = (Arc::default(), Arc::default());
    let stops: Arc<Mutex<Vec<(ab::Direction, Duration, bool)>>> = Arc::default();
    let (render_calls, capture_calls) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let render: ab::RenderCallback = {
        let (slot, stops, calls, observed) = (Arc::clone(&render_slot), Arc::clone(&stops), Arc::clone(&render_calls), Arc::clone(&rig.observed));
        Box::new(move |block| {
            block.render(&mut |chunk| chunk.fill(0.0));
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                let (took, driver_stopped) = stop_from_inside(&slot, &observed);
                stops.lock().unwrap().push((ab::Direction::Output, took, driver_stopped));
            }
        })
    };
    let capture: ab::CaptureCallback = {
        let (slot, stops, calls, observed) = (Arc::clone(&capture_slot), Arc::clone(&stops), Arc::clone(&capture_calls), Arc::clone(&rig.observed));
        Box::new(move |block| {
            block.chunks(&mut |_, _| {});
            if calls.fetch_add(1, Ordering::SeqCst) == 2 {
                let (took, driver_stopped) = stop_from_inside(&slot, &observed);
                stops.lock().unwrap().push((ab::Direction::Input, took, driver_stopped));
            }
        })
    };
    let (c, _) = rig
        .backend
        .open_stream(&rig.endpoint, &request(&rig, ab::Direction::Input, 48_000, Some(64)), Callback::Capture(capture), error_cb(&seen, ab::Direction::Input))
        .expect("open capture");
    let (r, _) = rig
        .backend
        .open_stream(&rig.endpoint, &request(&rig, ab::Direction::Output, 48_000, Some(64)), Callback::Render(render), error_cb(&seen, ab::Direction::Output))
        .expect("open render");
    *capture_slot.lock().unwrap() = Some(c);
    *render_slot.lock().unwrap() = Some(r);
    for slot in [&capture_slot, &render_slot] {
        ab::Stream::start(slot.lock().unwrap().as_ref().unwrap()).expect("start");
    }
    // The second render callback stops its own stream; the capture keeps the driver running.
    drive(&rig, 64, 0, 2, &[]);
    drive(&rig, 64, 128, 1, &[]);
    assert_eq!(render_calls.load(Ordering::SeqCst), 2, "no render callback after the stop from inside it");
    assert_eq!(capture_calls.load(Ordering::SeqCst), 3);
    // The third capture callback stopped the last running stream: the host stops the driver once
    // the buffer switch has returned, never from inside it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !rig.observed.call_names().contains(&"stop") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let stops = stops.lock().unwrap().clone();
    assert_eq!(stops.iter().map(|(d, _, _)| *d).collect::<Vec<_>>(), [ab::Direction::Output, ab::Direction::Input]);
    for (direction, took, driver_stopped) in &stops {
        assert!(*took < Duration::from_millis(100), "{direction:?}: stop returned at once, not after the host: {took:?}");
        assert!(!driver_stopped, "{direction:?}: the driver's stop was not called from inside its buffer switch");
    }
    let calls = rig.observed.calls.lock().unwrap().clone();
    let (_, thread) = calls.iter().find(|(n, _)| *n == "stop").expect("the host stopped the driver");
    assert_ne!(*thread, std::thread::current().id(), "on the host thread, not the driver's");
    drive(&rig, 64, 192, 1, &[]);
    assert_eq!((render_calls.load(Ordering::SeqCst), capture_calls.load(Ordering::SeqCst)), (2, 3), "nothing called after the stops");
    assert!(seen.errors.lock().unwrap().is_empty());
    let (c, r) = (capture_slot.lock().unwrap().take(), render_slot.lock().unwrap().take());
    drop((c, r));
    assert!(!testing::installed(), "the session ended with its streams");
}

#[test]
fn latencies_changed_refetches_on_the_host_thread() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    let before = rig.observed.call_names().iter().filter(|n| **n == "getLatencies").count();
    rig.observed.extra_latency.store(48, Ordering::SeqCst);
    assert_eq!(testing::message(abi::K_ASIO_LATENCIES_CHANGED, 0), 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while rig.observed.call_names().iter().filter(|n| **n == "getLatencies").count() == before && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    drive(&rig, 64, 0, 1, &[]);
    let r = seen.render.lock().unwrap()[0].0;
    assert_eq!(r.backend_frame_ns - r.backend_callback_ns, (150 + 48) * 1_000_000_000 / 48_000, "the stamps use the new output latency");
    drop((capture, render));
}

#[test]
fn the_buffer_switch_path_without_time_info_asks_for_the_position() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (capture, render, _, _) = duplex(&rig, 48_000, Some(64), &seen, vec![0.0; 2]);
    for (i, position) in [640u64, 704, 768].into_iter().enumerate() {
        rig.observed.position.store(position, Ordering::SeqCst);
        testing::switch_plain(i as i32 % 2);
    }
    rig.observed.position.store(u64::MAX, Ordering::SeqCst);
    testing::switch_plain(1);
    let renders = seen.render.lock().unwrap().clone();
    assert_eq!(renders.len(), 4);
    assert!(renders[..3].iter().all(|(s, _, _)| s.quality == ab::Quality::BackendEstimate));
    assert!(near(renders[1].0.backend_callback_ns - renders[0].0.backend_callback_ns, 64 * 1_000_000_000 / 48_000, 1));
    assert_eq!((renders[3].0.quality, renders[3].1), (ab::Quality::Arrival, None), "no position: the arrival, said so");
    let calls = rig.observed.call_names();
    assert_eq!(calls.iter().filter(|n| **n == "getSamplePosition").count(), 4);
    drop((capture, render));
}

#[test]
fn a_panicking_render_callback_is_reported_once_and_plays_silence() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let render: ab::RenderCallback = Box::new(move |block| {
        counted.fetch_add(1, Ordering::SeqCst);
        block.render(&mut |chunk| chunk.fill(0.7));
        panic!("render callback bug");
    });
    let (render, _) = rig
        .backend
        .open_render(&rig.endpoint, &request(&rig, ab::Direction::Output, 48_000, Some(64)), render, error_cb(&seen, ab::Direction::Output))
        .expect("open");
    render.start().expect("start");
    drive(&rig, 64, 0, 3, &[]);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "not called after it panicked");
    assert_eq!(seen.errors.lock().unwrap().as_slice(), [(ab::Direction::Output, ab::StreamError::Other)]);
    assert!(rig.observed.read_output(0, 0).iter().all(|b| *b == 0), "silence, not the half-written block");
    assert_eq!(render.stats().expect("stats").faults, 1);
    drop(render);
}

#[test]
fn the_host_answers_the_drivers_questions() {
    // No state is needed for these (and none is touched).
    assert_eq!(testing::message(abi::K_ASIO_SELECTOR_SUPPORTED, abi::K_ASIO_SUPPORTS_TIME_INFO), 1);
    assert_eq!(testing::message(abi::K_ASIO_SELECTOR_SUPPORTED, abi::K_ASIO_OVERLOAD), 1);
    assert_eq!(testing::message(abi::K_ASIO_SELECTOR_SUPPORTED, abi::K_ASIO_BUFFER_SIZE_CHANGE), 0);
    assert_eq!(testing::message(abi::K_ASIO_SELECTOR_SUPPORTED, 9), 0, "kAsioMMCCommand");
    assert_eq!(testing::message(abi::K_ASIO_ENGINE_VERSION, 0), 2);
    assert_eq!(testing::message(abi::K_ASIO_SUPPORTS_TIME_INFO, 0), 1);
    assert_eq!(testing::message(abi::K_ASIO_SUPPORTS_TIME_CODE, 0), 0);
    assert_eq!(testing::message(abi::K_ASIO_BUFFER_SIZE_CHANGE, 128), 0, "not handled in place: the driver sends a reset request");
}

#[test]
fn the_buffer_switch_allocates_nothing() {
    let _guard = lock();
    let rig = rig(model12());
    let frames = Arc::new(AtomicU64::new(0));
    let (f1, f2) = (Arc::clone(&frames), Arc::clone(&frames));
    let render: ab::RenderCallback = Box::new(move |block| {
        f1.fetch_add(block.frames as u64, Ordering::Relaxed);
        block.render(&mut |chunk| chunk.fill(0.1));
    });
    let capture: ab::CaptureCallback = Box::new(move |block| {
        f2.fetch_add(block.frames as u64, Ordering::Relaxed);
        block.chunks(&mut |_, _| {});
    });
    let (capture, _) =
        rig.backend.open_capture(&rig.endpoint, &request(&rig, ab::Direction::Input, 44_100, None), capture, Box::new(|_| {})).expect("in");
    let (render, _) =
        rig.backend.open_render(&rig.endpoint, &request(&rig, ab::Direction::Output, 44_100, None), render, Box::new(|_| {})).expect("out");
    capture.start().expect("start");
    render.start().expect("start");
    let ((), stats) = measure(|| {
        let mut position = 0u64;
        for i in 0..200 {
            testing::switch_time_info(i % 2, Some(position), Some(position * 22_675));
            if i == 100 {
                testing::message(abi::K_ASIO_OVERLOAD, 0);
                testing::message(abi::K_ASIO_RESYNC_REQUEST, 0);
            }
            position += 256;
        }
        rig.observed.position.store(512, Ordering::SeqCst);
        testing::switch_plain(0);
    });
    assert!(stats.is_zero(), "the buffer switch allocated: {stats:?}");
    assert_eq!(frames.load(Ordering::Relaxed), 2 * 201 * 256);
    drop((capture, render));
}

#[test]
fn a_loaded_handle_and_the_streams_share_the_driver() {
    let _guard = lock();
    let rig = rig(scarlett());
    let seen = seen();
    let (render, _) = rig
        .backend
        .open_render(
            &rig.endpoint,
            &request(&rig, ab::Direction::Output, 48_000, Some(64)),
            render_cb(&seen, vec![0.0; 2]),
            error_cb(&seen, ab::Direction::Output),
        )
        .expect("open");
    let handle = rig.backend.load("Driver").expect("the streaming driver is shared with a handle");
    assert_eq!(handle.latencies().expect("latencies"), (100, 150));
    drop(render);
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 1, "the handle keeps it loaded");
    handle.close().expect("close");
    assert_eq!(rig.observed.live.load(Ordering::Acquire), 0);
}

#[test]
fn the_factory_shares_one_backend_per_process() {
    let (a, b) = (AsioBackend::shared().expect("shared"), AsioBackend::shared().expect("shared"));
    assert!(Arc::ptr_eq(&a.host, &b.host), "one host thread");
    assert!(Arc::ptr_eq(&a.probes, &b.probes), "one probe cache");
}
