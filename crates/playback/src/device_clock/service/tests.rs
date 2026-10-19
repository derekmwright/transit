#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::device_clock::{ClockFitter, ClockKey, Direction, Observer, Quality, Reader, Stamp, observation_lane};
use soundcraft_rt::testing::measure;
use std::sync::mpsc;

fn fixture() -> (Observer, Pump, SnapshotReader, SnapshotReader) {
    let (observer, reader) = raw_fixture();
    let (pump, ui, audio) = prepare(reader);
    (observer, pump, ui, audio)
}

fn raw_fixture() -> (Observer, ClockReader) {
    let key = ClockKey { generation: 7, domain: 9, direction: Direction::Output };
    let (observer, observations) = observation_lane(key, 48_000);
    let reader = ClockReader { inner: Box::new(Reader { observations, fitter: ClockFitter::new(key), origin: HostOrigin::default() }) };
    (observer, reader)
}

fn offer(observer: &mut Observer) -> u64 {
    let host = 1_000_000_000 + observer.next_frame() * 1_000_000_000 / 48_000;
    assert!(observer.record(
        240,
        Stamp {
            host_callback_ns: host,
            backend_callback_ns: host as i64,
            backend_frame_ns: host as i64,
            quality: Quality::BackendEstimate,
            offset_uncertainty_ns: None,
        },
    ));
    host
}

#[test]
fn ui_can_remain_locked_while_worker_fits_and_audio_places_events() {
    let (mut observer, mut pump, ui, mut audio) = fixture();
    let ui = Mutex::new(ui);
    let mut held_ui = ui.lock().unwrap();
    let (step, work) = mpsc::sync_channel::<()>(0);
    let (done, ready) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        for _ in work {
            let now = offer(&mut observer);
            assert!(pump.step(|| Some(now)));
            done.send((now, observer.next_frame() - 240)).unwrap();
        }
        pump
    });
    assert_ne!(worker.thread().id(), std::thread::current().id());
    let mut latest = 0;
    for index in 0..256 {
        step.send(()).unwrap();
        let (now, frame) = ready.recv().unwrap();
        latest = now;
        let (report, allocations) = measure(|| audio.poll_with(|| Some(now)));
        assert!(allocations.is_zero(), "{allocations:?}");
        if index >= 24 {
            let map = report.map.unwrap();
            assert_eq!(map.host_to_frame(map.key, now, now).unwrap(), frame);
            assert_eq!(map.quality, Quality::BackendEstimate);
            assert_eq!(map.offset_uncertainty_ns, None);
        }
        assert_eq!(report.publication.dropped, 0);
    }
    assert!(held_ui.poll_with(|| Some(latest)).map.is_some(), "a stalled UI receives the latest publication on its first read");
    step.send(()).unwrap();
    let (now, _) = ready.recv().unwrap();
    assert!(held_ui.poll_with(|| Some(now)).map.is_some(), "UI catches up after releasing its queue credit");
    drop(step);
    drop(worker.join().unwrap());
}

#[test]
fn an_unread_valid_snapshot_is_replaced_by_new_invalid_observations() {
    let (mut observer, mut pump, mut ui, mut audio) = fixture();
    let mut now = 0;
    for _ in 0..32 {
        now = offer(&mut observer);
        assert!(pump.step(|| Some(now)));
        audio.poll_with(|| Some(now));
        ui.poll_with(|| Some(now));
    }
    assert!(audio.poll_with(|| Some(now)).map.is_some());
    // Leave audio unread across many still-valid maps while UI continues reading.
    for _ in 0..32 {
        now = offer(&mut observer);
        assert!(pump.step(|| Some(now)));
        assert!(ui.poll_with(|| Some(now)).map.is_some());
    }
    assert!(observer.record(
        240,
        Stamp {
            host_callback_ns: now + 5_000_000,
            backend_callback_ns: 0,
            backend_frame_ns: 0,
            quality: Quality::Invalid,
            offset_uncertainty_ns: None,
        },
    ));
    assert!(pump.step(|| Some(now + 5_000_000)));
    assert!(ui.poll_with(|| Some(now + 5_000_000)).map.is_none());
    let (report, allocations) = measure(|| audio.poll_with(|| Some(now + 5_000_000)));
    assert!(allocations.is_zero());
    assert!(report.map.is_none(), "the latest invalid report replaces pre-discontinuity reports even before their time horizon");
    // Repeated reads retain the latest invalid report, never resurrect an old map.
    assert!(pump.step(|| Some(now + 10_000_000)));
    assert_eq!(audio.poll_with(|| Some(now + 10_000_000)).fitting.invalid, 1);
}

#[test]
fn cached_map_expires_without_another_worker_tick_or_valid_host_clock() {
    let (mut observer, mut pump, _, mut audio) = fixture();
    let mut now = 0;
    for _ in 0..32 {
        now = offer(&mut observer);
        assert!(pump.step(|| Some(now)));
        audio.poll_with(|| Some(now));
    }
    assert!(audio.poll_with(|| Some(now)).map.is_some());
    assert!(audio.poll_with(|| None).map.is_none());
    assert!(audio.poll_with(|| Some(now + 100_000_001)).map.is_none());
}

#[test]
fn stopped_or_finished_workers_never_leave_a_usable_cached_fit() {
    for mode in 0..2 {
        let (mut observer, mut pump, _, mut audio) = fixture();
        let mut now = 0;
        for _ in 0..32 {
            now = offer(&mut observer);
            assert!(pump.step(|| Some(now)));
            audio.poll_with(|| Some(now));
        }
        assert!(audio.poll_with(|| Some(now)).map.is_some());
        if mode == 0 {
            pump.state.stop.store(true, Ordering::Release);
            assert!(!pump.step(|| Some(now)));
            assert!(audio.poll_with(|| Some(now)).map.is_none(), "stop must invalidate before worker destruction");
        }
        drop(pump);
        assert!(audio.poll_with(|| Some(now)).map.is_none());
    }
}

#[test]
fn native_service_can_stop_with_no_ui_pump_and_wakes_its_worker() {
    let (_, reader) = crate::device_clock::stream_clock(Direction::Output, 48_000).unwrap();
    let mut service = ClockService::start(reader).unwrap();
    let mut audio = service.take_audio_reader().unwrap();
    assert!(service.take_audio_reader().is_none());
    service.request_stop();
    assert!(audio.poll().map.is_none());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !service.is_finished() {
        assert!(std::time::Instant::now() < deadline, "stopped clock worker did not finish");
        std::thread::yield_now();
    }
    assert!(service.report().map.is_none());
}

#[test]
fn dropping_service_does_not_join_a_worker_paused_by_the_scheduler() {
    let (_, pump, ui, audio) = fixture();
    let state = Arc::clone(&pump.state);
    let (release, held) = mpsc::sync_channel::<()>(0);
    let worker = std::thread::spawn(move || {
        held.recv().unwrap();
        drop(pump);
    });
    let service = ClockService { ui: Mutex::new(ui), audio: Some(audio), state: Arc::clone(&state), worker };
    let (done, returned) = mpsc::sync_channel(1);
    let owner = std::thread::spawn(move || {
        drop(service);
        done.send(()).unwrap();
    });
    let completed = returned.recv_timeout(Duration::from_secs(30));
    release.send(()).unwrap(); // Always release, including a mutation that joins.
    owner.join().unwrap();
    assert!(completed.is_ok(), "control waited for the clock worker during drop");
    assert!(state.stop.load(Ordering::Acquire));
}

#[test]
fn callbacks_slower_than_four_worker_periods_still_get_recent_maps() {
    let (mut observer, mut pump, mut ui, mut audio) = fixture();
    let mut placed = 0;
    let mut now = 0;
    // Real device ordinals at 2048 / 48 kHz, maintenance at 5 ms. UI is left
    // completely unread; audio reads only once per device callback.
    for tick in 0..400_u64 {
        now = 1_000_000_000 + tick * 5_000_000;
        let callback_at = 1_000_000_000 + observer.next_frame() * 1_000_000_000 / 48_000;
        let callback = now >= callback_at;
        if callback {
            assert!(observer.record(
                2048,
                Stamp {
                    host_callback_ns: callback_at,
                    backend_callback_ns: callback_at as i64,
                    backend_frame_ns: callback_at as i64,
                    quality: Quality::BackendEstimate,
                    offset_uncertainty_ns: None,
                }
            ));
        }
        assert!(pump.step(|| Some(now)));
        if callback {
            let (report, heap) = measure(|| audio.poll_with(|| Some(now)));
            assert!(heap.is_zero());
            assert_eq!(report.publication.dropped, 0);
            if tick >= 40 {
                let map = report.map.unwrap();
                let expected = (now - 1_000_000_000) * 48_000 / 1_000_000_000;
                assert!(map.host_to_frame(map.key, now, now).unwrap().abs_diff(expected) <= 1);
                placed += 1;
            }
        }
    }
    assert!(placed >= 39);
    // First UI read after two seconds still sees the current valid estimate.
    assert!(ui.poll_with(|| Some(now)).map.is_some());
}

#[test]
fn native_worker_fits_without_ui_and_stop_wakes_a_long_park() {
    let (mut observer, reader) = raw_fixture();
    let mut now = 0;
    for _ in 0..32 {
        now = offer(&mut observer);
    }
    // Same native loop as start(), with a deterministic clock and a deliberately
    // long park. Removing fitting or unpark must fail without a timing race over
    // the normal 5 ms timeout. The watchdog is only a test deadlock guard.
    let mut service = ClockService::start_with(reader, move || Some(now), Duration::from_secs(3600)).unwrap();
    let mut audio = service.take_audio_reader().unwrap();
    let held_ui = service.ui.lock().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut fitted = false;
    while std::time::Instant::now() < deadline && !service.is_finished() {
        let (report, heap) = measure(|| audio.poll_with(|| Some(now)));
        assert!(heap.is_zero());
        if report.map.is_some() {
            fitted = true;
            break;
        }
        std::thread::yield_now();
    }
    service.request_stop();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !service.is_finished() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    let woke = service.is_finished();
    service.worker.thread().unpark(); // Cleanup even for the removed-unpark mutant.
    drop(held_ui);
    assert!(fitted, "the actual native worker must maintain fits with no UI polling");
    assert!(woke, "stop must wake the parked native worker");
    assert!(audio.poll_with(|| Some(now)).map.is_none());
}
