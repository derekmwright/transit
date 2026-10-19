#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use soundcraft_rt::testing::measure;

fn prepared() -> Player {
    // Allocates clocks/queues only. No device discovery, stream or fallback worker.
    Player::prepare(Arc::new(Session::default()), 48_000).unwrap().0
}
fn render<T: cpal::SizedSample + cpal::FromSample<f32>>(player: &Player, callback: &mut OutputCallback, out: &mut [T]) {
    callback.fill_with(out, |buffer, plans, actions| {
        render_held(&player.state, &player.fault, &player.shared, buffer, 2, plans, actions);
    });
}

#[test]
fn public_transport_reports_consumption_and_returns_exact_action_receipts() {
    let player = prepared();
    let _: &crate::Player = &player; // The public facade is this prepared runtime.
    let mut callback = OutputCallback::new(2, 48_000);
    render(&player, &mut callback, &mut [0.0f32; 128]);
    player.service().unwrap();
    let play = player.play(12_345, None, None).unwrap();
    assert!(!player.is_playing());
    assert_eq!(player.position(), 0, "request admission is not device consumption");
    let mut out = [1i16; 128];
    let ((), heap) = measure(|| render(&player, &mut callback, &mut out));
    assert!(heap.is_zero(), "{heap:?}");
    assert!(player.is_playing());
    assert_eq!(player.position(), 12_409);
    let stop = player.stop().unwrap();
    assert!(player.is_playing());
    assert_ne!(player.recovery_progress().unwrap().0, stop);
    render(&player, &mut callback, &mut out);
    assert!(!player.is_playing());
    assert_eq!(player.position(), 12_409);
    let mut notices = [None; 16];
    let count = player.poll_notices(&mut notices).unwrap();
    assert_eq!(player.recovery_progress().unwrap().0, stop);
    // Urgent requests use acknowledgment frontiers, not ordinary lane receipts.
    let receipts: Vec<_> = notices[..count]
        .iter()
        .flatten()
        .filter_map(|notice| match notice {
            Notice::Action(receipt) if receipt.sequence == play => Some(receipt),
            _ => None,
        })
        .collect();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].outcome, action_lane::Outcome::Applied);
    assert_eq!(receipts[0].handler_result, Some(ActionResult::Applied));
}

#[test]
fn device_loss_closes_sources_and_actions_while_main_and_holder_are_busy() {
    let player = prepared();
    let mut callback = OutputCallback::new(2, 48_000);
    render(&player, &mut callback, &mut [0.0f32; 128]);
    player.service().unwrap();
    let action = player.play(0, None, None).unwrap();
    let token = player.audition.unwrap();
    let mut error = error_callback(Arc::clone(&player.fault), Arc::clone(&player.shared), player.failure_signal.clone());
    {
        let owner = player.owner().unwrap();
        let held = player.state.lock().unwrap();
        let heap = std::thread::spawn(move || {
            crate::mark_audio_thread();
            measure(|| error(cpal::StreamError::DeviceNotAvailable)).1
        })
        .join()
        .unwrap();
        assert!(heap.is_zero(), "error callback: {heap:?}");
        assert!(owner.faulted());
        assert!(!owner.source_admitted(token));
        let audio = held.as_ref().unwrap();
        assert!(audio.plans.faulted());
        assert!(!audio.sources.permits(token.port(), token.generation()));
    }
    assert_eq!(player.stop(), Err(PlayerError::Faulted));
    let mut out = [0.75f32; 128];
    let ((), heap) = measure(|| render(&player, &mut callback, &mut out));
    assert!(heap.is_zero());
    assert_eq!(out, [0.0; 128]);
    let mut notices = [None; 16];
    let count = player.poll_notices(&mut notices).unwrap();
    assert!(notices[..count].iter().flatten().any(|notice| matches!(notice,
        Notice::Action(receipt) if receipt.sequence == action && receipt.outcome == action_lane::Outcome::Faulted
    )));
    assert_eq!(player.callback_telemetry().device_lost_errors, 1);
}

#[test]
fn a_borrowed_holder_yields_silence_without_waiting_or_allocating() {
    let player = prepared();
    let mut callback = OutputCallback::new(2, 48_000);
    let mut out = [0.75f32; 128];
    let held = player.state.lock().unwrap();
    let ((), heap) = measure(|| render(&player, &mut callback, &mut out));
    assert!(heap.is_zero());
    assert_eq!(out, [0.0; 128]);
    assert_eq!(player.callback_telemetry().contention_silence_frames, 64);
    drop(held);
}

#[test]
fn reentrant_control_and_input_requests_refuse_without_losing_ownership() {
    let player = prepared();
    let owner = player.owner().unwrap();
    assert_eq!(player.play(0, None, None), Err(PlayerError::Busy));
    drop(owner);
    let mut recorder = Some(record::Recorder::detached("caller input"));
    let input = player.input.borrow_mut();
    assert_eq!(player.close_input(&mut recorder), Err(PlayerError::Busy));
    assert_eq!(recorder.as_ref().unwrap().device_name, "caller input");
    drop(input);
    *player.input.borrow_mut() = HeldInput::Held(Some(record::Recorder::detached("held input")));
    assert_eq!(player.close_input(&mut recorder), Err(PlayerError::Busy));
    assert!(recorder.is_some());
    *player.input.borrow_mut() = HeldInput::Held(None);
    player.close_input(&mut recorder).unwrap();
    assert!(recorder.is_none());
    assert_eq!(player.open_input(&AudioConfig::default()).unwrap().device_name, "caller input");
}

#[test]
fn returning_input_cannot_finalize_or_discard_an_uncollected_take() {
    let player = prepared();
    *player.input.borrow_mut() = HeldInput::Held(None);
    let mut recorder = record::Recorder::detached("recording input");
    // Empty take list still owns a writer/capture operation; no files or devices.
    recorder.arm(Vec::new()).unwrap();
    let mut recorder = Some(recorder);
    assert_eq!(player.close_input(&mut recorder), Err(PlayerError::RecordingActive));
    assert!(recorder.as_ref().unwrap().has_pending_take());
    assert_eq!(recorder.as_mut().unwrap().finish(), (Vec::new(), None));
    player.close_input(&mut recorder).unwrap();
    assert!(recorder.is_none());
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn virtual_fallback_conserves_fractional_time_across_irregular_wakes() {
    use std::time::Duration;
    for rate in [44_100, 48_000, 96_000] {
        let mut pacer = SilentPacer::default();
        let (mut elapsed, mut rendered) = (0u128, 0u128);
        for index in 0..10_000u64 {
            let nanos = 1_000_001 + (index % 17) * 531;
            elapsed += u128::from(nanos);
            rendered += pacer.advance(Duration::from_nanos(nanos), rate, 8192).unwrap() as u128;
            assert_eq!(rendered, elapsed * u128::from(rate) / 1_000_000_000);
        }
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn virtual_fallback_bounds_each_pass_without_discarding_stall_backlog() {
    use std::time::Duration;
    let mut pacer = SilentPacer::default();
    let mut rendered = pacer.advance(Duration::from_secs(1), 48_000, 64).unwrap();
    assert_eq!(rendered, 64);
    assert_eq!(pacer.advance(Duration::ZERO, 48_000, 0), Some(0));
    for _ in 1..750 {
        assert_eq!(pacer.advance(Duration::ZERO, 48_000, 64), Some(64));
        rendered += 64;
    }
    assert_eq!(rendered, 48_000);
    assert_eq!(pacer.advance(Duration::ZERO, 48_000, 64), Some(0));
}
