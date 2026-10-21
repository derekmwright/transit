#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use soundcraft_mix::{DeviceGeneration, PlanLimits, RenderTarget};
use soundcraft_rt::testing::measure;

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn audio_dispatch_recovery_is_independent_of_stale_worker_signals_and_main_polling() {
    let (_plans, mut plans, mut control, mut audio) = setup();
    let old = control.register(7).unwrap();
    let stale = control.permit(old).unwrap();
    let request = control.revoke(old).unwrap();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| Ok(()));
    control.release(request).unwrap();
    let current = control.register(7).unwrap();
    let permit = control.permit(current).unwrap();
    let (_, heap) = measure(|| {
        audio.invalidate(7, current.generation());
        stale.invalidate();
    });
    assert!(heap.is_zero());
    assert!(!permit.admits());
    assert!(!audio.permits(7, current.generation()));
    let mut released = None;
    let report = audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |port, generation, _| {
        released = Some((port, generation));
        Ok(())
    });
    assert_eq!(report.revoked, 1);
    assert_eq!(released, Some((7, current.generation())));
    let request = control.revoke(current).unwrap();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| panic!("already released"));
    control.release(request).unwrap();
    let newer = control.register(7).unwrap();
    audio.invalidate(7, current.generation());
    stale.invalidate();
    assert!(control.permit(newer).unwrap().admits());
}

fn setup() -> (plan_bridge::Control, plan_bridge::Audio, Control, Audio) {
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 };
    let (plans, audio) = plan_bridge::pair(target, PlanLimits::default());
    let (sources, ingress) = pair(&plans).unwrap();
    (plans, audio, sources, ingress)
}

#[test]
fn the_public_capacity_is_exactly_sixty_four_registrations_including_audition() {
    let (_plans, _audio, mut control, _ingress) = setup();
    // Expected capacity comes from the external contract, not PORTS: deriving
    // both workload and assertion from that constant hid a 65-slot mutation.
    control.register(AUDITION_PORT).unwrap();
    for port in 0..63 {
        control.register(port).unwrap();
    }
    assert_eq!(control.register(63), Err(Error::Full));
}

#[test]
fn revocation_is_idempotent_and_acknowledged_only_after_the_handler_finishes() {
    let (_plans, mut plans, mut control, mut audio) = setup();
    let token = control.register(7).unwrap();
    assert!(control.admits(token));
    assert!(audio.permits(7, token.generation()));
    let request = control.revoke(token).unwrap();
    assert_eq!(control.revoke(token), Ok(request));
    assert!(!control.admits(token));
    assert!(!audio.permits(7, token.generation()));
    assert_eq!(control.release(request), Err(Error::NotRevoked));
    assert_eq!(control.register(7), Err(Error::PortRegistered));
    let (report, heap) = measure(|| {
        audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |port, generation, _| {
            assert_eq!((port, generation), (7, token.generation()));
            assert_eq!(control.status(request), Ok(State::Pending));
            assert_eq!(control.release(request), Err(Error::NotRevoked));
            Ok(())
        })
    });
    assert!(heap.is_zero(), "{heap:?}");
    assert_eq!(report.revoked, 1);
    assert_eq!(control.status(request), Ok(State::Revoked));
    assert_eq!(control.release(request), Ok(Receipt { request, outcome: State::Revoked }));
    assert_eq!(control.release(request), Err(Error::StaleToken));
    let next = control.register(7).unwrap();
    assert!(next.generation() > token.generation());
    assert!(!audio.permits(7, token.generation()), "delayed old ingress must never become valid again");
    assert!(audio.permits(7, next.generation()));
}

#[test]
fn callback_budget_and_round_robin_prevent_a_busy_port_from_starving_other_revokes() {
    let (_plans, mut plans, mut control, mut audio) = setup();
    let tokens: Vec<_> = (0..PORTS as u16).map(|port| control.register(port).unwrap()).collect();
    assert_eq!(control.register(AUDITION_PORT), Err(Error::Full));
    let requests: Vec<_> = tokens.iter().map(|&token| control.revoke(token).unwrap()).collect();
    let mut visited = [false; PORTS];
    for expected in 0..PORTS {
        let mut budget = action_lane::callback_scope();
        let ((), heap) = measure(|| {
            for _ in 0..16 {
                audio.service(&mut budget, &mut plans, &mut |port, _, _| {
                    assert!(!visited[usize::from(port)]);
                    visited[usize::from(port)] = true;
                    Ok(())
                });
            }
        });
        assert!(heap.is_zero(), "{heap:?}");
        assert_eq!(visited.iter().filter(|&&v| v).count(), expected + 1);
        assert_eq!(control.status(requests[expected]), Ok(State::Revoked));
        // Repeatedly re-register and revoke the first port. The callback cursor
        // must still visit every other port before coming back to this one.
        if expected == 0 {
            control.release(requests[0]).unwrap();
            let again = control.register(0).unwrap();
            control.revoke(again).unwrap();
        }
    }
    assert!(visited.iter().all(|&v| v));
    assert_eq!(control.pending(), 1);
}

#[test]
fn identities_and_requests_never_wrap_and_foreign_tokens_are_refused() {
    let (_p, _a, mut control, audio) = setup();
    let (_p2, _a2, mut foreign, _audio2) = setup();
    let token = control.register(0).unwrap();
    let other = foreign.register(0).unwrap();
    assert_eq!(token.generation(), other.generation());
    assert!(!control.admits(other));
    assert_eq!(control.revoke(other), Err(Error::StaleToken));
    control.generation = u64::MAX;
    assert_eq!(control.register(1), Err(Error::Exhausted));
    control.request = u64::MAX;
    assert_eq!(control.revoke(token), Err(Error::Exhausted));
    assert!(control.admits(token));
    assert!(audio.permits(0, token.generation()));
}

#[test]
fn faults_refuse_reuse_and_retirement_supplies_every_uncollected_outcome() {
    let (main, mut plans, mut control, mut audio) = setup();
    let first = control.register(0).unwrap();
    let second = control.register(1).unwrap();
    let done = control.revoke(first).unwrap();
    let failed = control.revoke(second).unwrap();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| Ok(()));
    let (report, heap) = measure(|| audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| Err(ApplyError::Refused)));
    assert!(heap.is_zero(), "{heap:?}");
    assert!(report.faulted);
    assert!(main.faulted());
    assert_eq!(control.status(done), Ok(State::Revoked));
    assert_eq!(control.status(failed), Ok(State::Faulted));
    assert_eq!(control.release(done), Err(Error::Faulted));
    assert_eq!(control.register(2), Err(Error::Faulted));
    assert!(!audio.permits(1, second.generation()));
    let retired = control.retire(audio);
    assert_eq!(retired, [Receipt { request: done, outcome: State::Revoked }, Receipt { request: failed, outcome: State::Faulted }]);

    let (_p, _a, mut control, audio) = setup();
    let token = control.register(0).unwrap();
    let pending = control.revoke(token).unwrap();
    assert_eq!(control.retire(audio), [Receipt { request: pending, outcome: State::Shutdown }]);
}

#[test]
fn foreign_or_faulted_bridges_never_run_a_revoke_handler() {
    for foreign in [false, true] {
        let (_p, mut plans, mut control, mut audio) = setup();
        let (_p2, mut other, _c2, _a2) = setup();
        let token = control.register(0).unwrap();
        let request = control.revoke(token).unwrap();
        let bridge = if foreign {
            &mut other
        } else {
            plans.fault();
            &mut plans
        };
        let mut called = false;
        let report = audio.service(&mut action_lane::callback_scope(), bridge, &mut |_, _, _| {
            called = true;
            Ok(())
        });
        assert!(!called, "even an otherwise successful handler must be refused");
        assert!(report.faulted);
        assert_eq!(control.status(request), Ok(State::Faulted));
    }
}

#[test]
fn an_acknowledged_tombstone_cannot_be_undone_by_a_stale_admission_flag() {
    let (_p, mut plans, mut control, mut audio) = setup();
    let token = control.register(0).unwrap();
    control.revoke(token).unwrap();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| Ok(()));
    // Force precisely the older value a concurrent atomic load could observe.
    control.shared.ports[usize::from(token.slot)].admitted.store(true, Ordering::Release);
    assert!(!audio.permits(0, token.generation()));
}

#[test]
fn a_panicking_revoke_retains_its_payload_until_main_retires_the_audio_endpoint() {
    use std::sync::Mutex;
    struct Payload(Arc<AtomicBool>, std::thread::ThreadId);
    impl Drop for Payload {
        fn drop(&mut self) {
            assert_eq!(std::thread::current().id(), self.1);
            self.0.store(true, Ordering::Release);
        }
    }
    let (_p, plans, mut control, audio) = setup();
    let token = control.register(0).unwrap();
    let request = control.revoke(token).unwrap();
    let holder = Arc::new(Mutex::new(Some((plans, audio))));
    let worker = Arc::clone(&holder);
    let dropped = Arc::new(AtomicBool::new(false));
    let payload = Payload(Arc::clone(&dropped), std::thread::current().id());
    std::thread::spawn(move || {
        let mut payload = Some(payload);
        let mut guard = worker.lock().unwrap();
        let (plans, audio) = guard.as_mut().unwrap();
        let report = audio.service(&mut action_lane::callback_scope(), plans, &mut |_, _, _| std::panic::panic_any(payload.take().unwrap()));
        assert!(report.faulted);
    })
    .join()
    .unwrap();
    assert!(!dropped.load(Ordering::Acquire));
    assert_eq!(control.status(request), Ok(State::Faulted));
    let (_plans, audio) = holder.lock().unwrap().take().unwrap();
    control.retire(audio);
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn backend_ids_and_the_mix_owned_clip_port_are_not_ingress_registrations() {
    let (_p, _a, mut control, audio) = setup();
    for port in [PORTS as u16, 1024, 0xFFFF] {
        assert_eq!(control.register(port), Err(Error::PortOutOfRange));
    }
    let audition = control.register(AUDITION_PORT).unwrap();
    assert!(audio.permits(AUDITION_PORT, audition.generation()));
    assert_eq!(control.register(AUDITION_PORT), Err(Error::PortRegistered));
}

#[test]
fn worker_disconnect_releases_without_main_polling_and_old_workers_cannot_close_reuse() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<Permit>();
    let (_bridge, mut plans, mut control, mut audio) = setup();
    let token = control.register(7).unwrap();
    let permit = control.permit(token).unwrap();
    let delayed = permit.clone();
    let heap = std::thread::spawn(move || {
        assert!(permit.admits());
        measure(|| {
            permit.invalidate();
            assert!(!permit.admits());
        })
        .1
    })
    .join()
    .unwrap();
    assert!(heap.is_zero(), "worker recovery must not allocate: {heap:?}");
    assert!(!audio.permits(token.port(), token.generation()));
    // No Control method or UI service ran between worker invalidation and this
    // real source service. Recovery runs through the same restricted audio view.
    let mut released = Vec::with_capacity(1);
    let (report, heap) = measure(|| {
        audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |port, generation, _| {
            released.push((port, generation));
            Ok(())
        })
    });
    assert!(heap.is_zero(), "{heap:?}");
    assert_eq!(report.revoked, 1);
    assert_eq!(released, [(7, token.generation())]);
    assert!(!control.admits(token));
    assert!(matches!(control.permit(token), Err(Error::Revoked)));
    let request = control.revoke(token).unwrap();
    assert_eq!(control.release(request), Err(Error::NotRevoked));
    let report = audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| panic!("already released"));
    assert_eq!(report.revoked, 0);
    assert_eq!(control.status(request), Ok(State::Revoked));
    control.release(request).unwrap();
    // Reuse both the fixed storage slot and then the original port identity.
    let replacement = control.register(7).unwrap();
    assert_eq!(replacement.slot, token.slot);
    let next = control.permit(replacement).unwrap();
    std::thread::spawn(move || {
        assert!(!delayed.admits());
        for _ in 0..1000 {
            delayed.invalidate();
        }
    })
    .join()
    .unwrap();
    assert!(next.admits());
    assert!(control.admits(replacement));
    assert!(audio.permits(7, replacement.generation()));
    let report = audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| panic!("old worker closed replacement"));
    assert_eq!(report.revoked, 0);
}

#[test]
fn worker_recovery_shares_the_actual_callback_budget_with_main_revocation() {
    let (_bridge, mut plans, mut control, mut audio) = setup();
    let tokens: Vec<_> = (0..PORTS as u16).map(|port| control.register(port).unwrap()).collect();
    let permits: Vec<_> = tokens.iter().map(|&token| control.permit(token).unwrap()).collect();
    let request = control.revoke(tokens[0]).unwrap();
    for permit in &permits {
        permit.invalidate();
    }
    let mut seen = [0; PORTS];
    for callback in 0..PORTS {
        let mut budget = action_lane::callback_scope();
        let ((), heap) = measure(|| {
            for _ in 0..8 {
                audio.service(&mut budget, &mut plans, &mut |port, _, _| {
                    seen[usize::from(port)] += 1;
                    Ok(())
                });
            }
        });
        assert!(heap.is_zero(), "{heap:?}");
        assert_eq!(seen.iter().sum::<usize>(), callback + 1);
    }
    assert_eq!(seen, [1; PORTS]);
    assert_eq!(control.status(request), Ok(State::Revoked));
    assert!(permits.iter().all(|permit| !permit.admits()));
}

#[test]
fn worker_permits_close_on_shared_fault_main_revoke_and_owner_shutdown() {
    let (mut bridge, _plans, mut control, _audio) = setup();
    let (_other_bridge, _other_plans, mut other, _other_audio) = setup();
    let token = control.register(2).unwrap();
    let foreign = other.register(2).unwrap();
    assert!(matches!(control.permit(foreign), Err(Error::StaleToken)));
    let permit = control.permit(token).unwrap();
    bridge.fault();
    assert!(!permit.admits());
    assert!(matches!(control.permit(token), Err(Error::Faulted)));

    let (_bridge, _plans, mut control, audio) = setup();
    let token = control.register(2).unwrap();
    let permit = control.permit(token).unwrap();
    control.revoke(token).unwrap();
    assert!(!permit.admits());
    assert!(!audio.permits(2, token.generation()));

    let (_bridge, _plans, mut control, audio) = setup();
    let token = control.register(2).unwrap();
    let permit = control.permit(token).unwrap();
    drop(control);
    assert!(!permit.admits(), "no callback is needed to observe owner shutdown");
    assert!(!audio.permits(2, token.generation()));
}

#[test]
fn failed_worker_recovery_faults_every_source_without_acknowledging_release() {
    let (_bridge, mut plans, mut control, mut audio) = setup();
    let token = control.register(1).unwrap();
    let permit = control.permit(token).unwrap();
    let other = control.register(2).unwrap();
    let other_permit = control.permit(other).unwrap();
    permit.invalidate();
    let report = audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| Err(ApplyError::Refused));
    assert!(report.faulted);
    assert_eq!(report.revoked, 0);
    assert!(!other_permit.admits());
    assert!(!audio.permits(2, other.generation()));
    assert_eq!(control.revoke(token), Err(Error::Faulted));
}

#[test]
fn raw_stop_cutoff_covers_worker_backlog_and_only_the_original_open_generation() {
    let (_bridge, mut plans, mut control, mut audio) = setup();
    let token = control.register(4).unwrap();
    let other = control.register(5).unwrap();
    let permit = control.permit(token).unwrap();
    let delayed = permit.clone();
    let (admitted_send, admitted_recv) = std::sync::mpsc::sync_channel(1);
    let (resume_send, resume_recv) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        assert!(permit.admits_sequence(12));
        admitted_send.send(()).unwrap(); // event already left the backend queue
        resume_recv.recv().unwrap();
        assert!(!permit.admits_sequence(12));
        assert!(permit.admits_sequence(13));
    });
    admitted_recv.recv().unwrap();
    control.cancel_before(token, 13).unwrap();
    control.cancel_before(token, 9).unwrap(); // an older cutoff cannot resurrect input
    resume_send.send(()).unwrap();
    worker.join().unwrap();
    let ((), heap) = measure(|| {
        assert!(!audio.permits_sequence(4, token.generation(), 12), "final dispatch rejects worker backlog");
        assert!(audio.permits_sequence(4, token.generation(), 13));
        assert!(audio.permits_sequence(5, other.generation(), 0), "another port has its own sequence domain");
        assert!(!audio.permits_sequence(4, token.generation(), u64::MAX));
    });
    assert!(heap.is_zero(), "{heap:?}");
    control.cancel_before(token, u64::MAX).unwrap();
    assert!(!delayed.admits_sequence(u64::MAX - 1));
    assert!(!delayed.admits_sequence(u64::MAX));
    let revoke = control.revoke(token).unwrap();
    audio.service(&mut action_lane::callback_scope(), &mut plans, &mut |_, _, _| Ok(()));
    control.release(revoke).unwrap();
    let replacement = control.register(4).unwrap();
    assert_eq!(replacement.slot, token.slot);
    let new_permit = control.permit(replacement).unwrap();
    assert_eq!(control.cancel_before(token, u64::MAX), Err(Error::StaleToken));
    assert!(!delayed.admits_sequence(0));
    assert!(new_permit.admits_sequence(0), "new open starts its own sequence at zero");
    assert!(audio.permits_sequence(4, replacement.generation(), 0));
}
