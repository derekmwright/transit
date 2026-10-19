#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use soundcraft_mix::{DeviceGeneration, PlanLimits, RenderTarget};
use soundcraft_rt::testing::measure;

fn setup() -> (plan_bridge::Control, plan_bridge::Audio, Control, Audio) {
    let target = RenderTarget { device: DeviceGeneration(1), sample_rate: 48_000, max_block: 512 };
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
