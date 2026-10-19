//! Bounded ordering, admission, exact-plan prerequisites and release recovery.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use crate::action_lane::{self, CALLBACK_ACTIONS, CAPACITY, Class, Event, Outcome, SendError};
use soundcraft_mix::PlanGeneration;
use soundcraft_rt::testing::measure;

// CountingAlloc is installed once by the playback plan-bridge unit-test module.

fn generation(n: u64) -> PlanGeneration {
    PlanGeneration::from_raw(n)
}

fn bridge() -> (crate::plan_bridge::Control, crate::plan_bridge::Audio) {
    crate::plan_bridge::pair(
        soundcraft_mix::RenderTarget { device: soundcraft_mix::DeviceGeneration(1), sample_rate: 48_000, max_block: 512 },
        soundcraft_mix::PlanLimits::default(),
    )
}

fn fixture<T: Copy>() -> (action_lane::Control<T>, action_lane::Audio<T>) {
    let (control, audio) = bridge();
    let result = action_lane::pair(&control);
    control.retire(audio);
    result
}

#[test]
fn typed_receipts_keep_handler_refusal_separate_from_successful_lane_dispatch() {
    let (plan_control, mut plan_audio) = bridge();
    let (mut control, mut audio) = action_lane::pair_with_result::<u8, bool>(&plan_control);
    let transport = control.send_action(1, None).unwrap();
    let note = control.send_note(2, super::NoteKind::Off).unwrap();
    let mut calls = 0;
    let (result, stats) = measure(|| {
        audio.service(&mut super::callback_scope(), &mut plan_audio, &mut |event, _| {
            calls += 1;
            matches!(event, Event::Action { payload: 1, .. })
        })
    });
    assert!(stats.is_zero());
    assert_eq!((result.applied, calls), (2, 2));
    // Cross-class receipt order differs from admission order. Sequence owns the result.
    let receipt = control.reclaim().unwrap();
    assert_eq!((receipt.sequence, receipt.payload, receipt.outcome, receipt.handler_result), (note, 2, Outcome::Applied, Some(false)));
    let receipt = control.reclaim().unwrap();
    assert_eq!((receipt.sequence, receipt.payload, receipt.outcome, receipt.handler_result), (transport, 1, Outcome::Applied, Some(true)));
    assert!(control.retire(audio).is_empty());
    plan_control.retire(plan_audio);
}

#[test]
fn typed_receipts_have_no_result_for_actions_that_never_reached_the_handler() {
    let (plan_control, mut plan_audio) = bridge();
    let (mut control, mut audio) = action_lane::pair_with_result::<u8, u64>(&plan_control);
    let stopped = control.send_action(1, None).unwrap();
    control.stop().unwrap();
    let released = control.send_note(2, super::NoteKind::On).unwrap();
    control.all_notes_off().unwrap();
    let rejected = control.send_action(3, Some(generation(1))).unwrap();
    control.reject_plan(generation(1));
    audio.service(&mut super::callback_scope(), &mut plan_audio, &mut |event, _| {
        assert!(!matches!(event, Event::Action { .. }), "cancelled work must not reach handler");
        u64::MAX // Urgent acknowledgments must not leak this result into normal receipts.
    });
    let mut receipts = control.retire(audio);
    receipts.sort_unstable_by_key(|r| r.sequence);
    assert_eq!(receipts.len(), 3);
    assert_eq!(
        receipts.iter().map(|r| (r.sequence, r.outcome)).collect::<Vec<_>>(),
        [(stopped, Outcome::Stopped), (released, Outcome::NotesReleased), (rejected, Outcome::PlanRejected),]
    );
    assert!(receipts.iter().all(|r| r.handler_result.is_none()));
    let (mut control, audio) = action_lane::pair_with_result::<u8, u64>(&plan_control);
    let shutdown = control.send_action(4, None).unwrap();
    let receipts = control.retire(audio);
    assert_eq!((receipts[0].sequence, receipts[0].outcome, receipts[0].handler_result), (shutdown, Outcome::Shutdown, None));
    plan_control.retire(plan_audio);
}

#[test]
fn typed_result_survives_return_backpressure_and_later_fault_without_replaying() {
    let (plan_control, mut plan_audio) = bridge();
    let (mut control, mut audio) = action_lane::pair_with_result::<u8, u64>(&plan_control);
    for _ in 0..CAPACITY {
        audio
            .actions
            .returned
            .push(super::Receipt { sequence: super::Sequence(0), payload: 0, outcome: Outcome::Shutdown, handler_result: None })
            .unwrap();
    }
    let sequence = control.send_action(9, None).unwrap();
    let mut calls = 0;
    let mut apply = |_, _: &mut crate::plan_bridge::Processing<'_>| {
        calls += 1;
        0xFEDC_BA98_7654_3210
    };
    let (result, stats) = measure(|| audio.service(&mut super::callback_scope(), &mut plan_audio, &mut apply));
    assert!(stats.is_zero());
    assert!(result.return_pending);
    audio.fault();
    assert!(audio.service(&mut super::callback_scope(), &mut plan_audio, &mut apply).return_pending);
    for _ in 0..CAPACITY {
        control.actions.returned.pop().unwrap();
    }
    let (result, stats) = measure(|| audio.service(&mut super::callback_scope(), &mut plan_audio, &mut apply));
    assert!(stats.is_zero());
    assert!(result.faulted);
    assert_eq!(calls, 1);
    let receipt = control.reclaim().unwrap();
    assert_eq!((receipt.sequence, receipt.outcome, receipt.handler_result), (sequence, Outcome::Applied, Some(0xFEDC_BA98_7654_3210)));
    assert_eq!(control.credits(), CAPACITY);
    assert!(control.retire(audio).is_empty());
    plan_control.retire(plan_audio);
}

#[test]
fn typed_handler_unwind_does_not_publish_an_invented_result() {
    let (plan_control, mut plan_audio) = bridge();
    let (mut control, mut audio) = action_lane::pair_with_result::<u8, u64>(&plan_control);
    control.send_action(1, None).unwrap();
    control.send_note(2, super::NoteKind::Off).unwrap();
    let mut calls = 0;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audio.service(&mut super::callback_scope(), &mut plan_audio, &mut |_, _| {
                calls += 1;
                panic!("typed handler failed before returning");
            });
        }))
        .is_err()
    );
    assert!(
        audio
            .service(&mut super::callback_scope(), &mut plan_audio, &mut |_, _| {
                calls += 1;
                99
            })
            .faulted
    );
    assert_eq!(calls, 1);
    let receipts = control.retire(audio);
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(|r| r.outcome == Outcome::Faulted && r.handler_result.is_none()));
    plan_control.retire(plan_audio);
}

#[test]
fn notes_do_not_wait_for_transport_prerequisites_or_consume_transport_credits() {
    use super::NoteKind;
    let (mut control, mut audio) = fixture();
    control.send_action(0, Some(generation(5))).unwrap();
    let mut applied = 0;
    for note in 1..=50 {
        control.send_note(note, NoteKind::Off).unwrap();
        let (result, stats) = measure(|| {
            audio.service_generation(&mut super::callback_scope(), generation(4), &mut |event| {
                assert!(matches!(event,Event::Action { payload,.. } if payload==note));
                applied += 1;
            })
        });
        assert!(stats.is_zero());
        assert!(result.waiting_for_plan);
        let receipt = control.reclaim().unwrap();
        assert_eq!((receipt.payload, receipt.outcome), (note, Outcome::Applied));
    }
    assert_eq!(applied, 50);
    assert_eq!(control.credits(), CAPACITY - 1);
    assert_eq!(control.note_credits(), super::NOTE_CAPACITY);
    assert_eq!(control.send(51, Class::NoteOff, Some(generation(5))), Err(SendError::NotePrerequisite));
    assert_eq!(control.retire(audio).len(), 1);
}

#[test]
fn saturated_notes_leave_transport_credit_and_each_class_keeps_its_callback_budget() {
    use super::NoteKind;
    let (mut control, mut audio) = fixture();
    for i in 0..super::NOTE_CAPACITY {
        control.send_note(i, NoteKind::On).unwrap();
    }
    for i in 0..CAPACITY {
        control.send_action(1000 + i, None).unwrap();
    }
    assert_eq!((control.credits(), control.note_credits()), (0, 0));
    let (mut notes, mut actions) = (0, 0);
    let mut budget = super::callback_scope();
    for _ in 0..3 {
        let (_, stats) = measure(|| {
            audio.service_generation(&mut budget, generation(1), &mut |event| {
                if let Event::Action { payload, .. } = event {
                    if payload < 1000 {
                        notes += 1;
                    } else {
                        actions += 1;
                    }
                }
            })
        });
        assert!(stats.is_zero());
    }
    assert_eq!((notes, actions), (super::CALLBACK_NOTES, CALLBACK_ACTIONS));
    for _ in 0..(notes + actions) {
        control.reclaim().unwrap();
    }
    assert_eq!((control.credits(), control.note_credits()), (CALLBACK_ACTIONS, super::CALLBACK_NOTES));
    assert_eq!(control.retire(audio).len(), CAPACITY + super::NOTE_CAPACITY - notes - actions);
}

#[test]
fn a_full_waiting_transport_lane_cannot_refuse_a_note_release() {
    let (mut control, mut audio) = fixture();
    for i in 0..CAPACITY {
        control.send_action(i, Some(generation(5))).unwrap();
    }
    let release = control.send_note(999, super::NoteKind::Off).unwrap();
    let mut received = None;
    let result = audio.service_generation(&mut super::callback_scope(), generation(4), &mut |event| received = Some(event));
    assert!(result.waiting_for_plan);
    assert_eq!(received, Some(Event::Action { sequence: release, payload: 999 }));
    assert_eq!(control.reclaim().unwrap().sequence, release);
    assert_eq!((control.credits(), control.note_credits()), (0, super::NOTE_CAPACITY));
    assert_eq!(control.retire(audio).len(), CAPACITY);
}

#[test]
fn urgent_handler_unwind_faults_both_classes_without_acknowledging_or_replaying() {
    for stop in [true, false] {
        let (mut control, mut audio) = fixture();
        if stop {
            control.stop().unwrap();
        } else {
            control.all_notes_off().unwrap();
        }
        control.send_action(1, None).unwrap();
        control.send_note(2, super::NoteKind::Off).unwrap();
        let mut calls = 0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                audio.service_generation(&mut super::callback_scope(), generation(1), &mut |_| {
                    calls += 1;
                    panic!("urgent handler fault");
                });
            }))
            .is_err()
        );
        let result = audio.service_generation(&mut super::callback_scope(), generation(1), &mut |_| calls += 1);
        assert!(result.faulted);
        assert_eq!(calls, 1);
        assert_eq!(control.recovery_progress(), (super::Sequence(0), super::Sequence(0)));
        let receipts = control.retire(audio);
        assert_eq!(receipts.len(), 2);
        assert!(receipts.iter().all(|r| r.outcome == Outcome::Faulted));
    }
}

#[test]
fn stop_absorbs_an_older_release_after_the_release_budget_was_spent() {
    let (mut control, mut audio) = fixture::<u8>();
    let first = control.all_notes_off().unwrap();
    let mut budget = super::callback_scope();
    let mut events = Vec::with_capacity(4);
    audio.service_generation(&mut budget, generation(1), &mut |e| events.push(e));
    control.all_notes_off().unwrap();
    let stop = control.stop().unwrap();
    let (result, stats) = measure(|| audio.service_generation(&mut budget, generation(1), &mut |e| events.push(e)));
    assert!(stats.is_zero());
    assert!(!result.recovery_deferred);
    assert_eq!(events, [Event::AllNotesOff { sequence: first }, Event::Stop { sequence: stop }]);
    assert_eq!(control.recovery_progress(), (stop, stop));
    control.retire(audio);
}

#[test]
fn production_service_rejects_a_foreign_runtime_even_with_equal_numeric_generations() {
    let (first_control, mut first_audio) = bridge();
    let (second_control, mut second_audio) = bridge();
    assert_eq!(first_audio.engine().active(), second_audio.engine().active());
    let (mut control, mut audio) = action_lane::pair(&first_control);
    control.send_action(1, None).unwrap();
    control.send_note(2, super::NoteKind::Off).unwrap();
    let (result, stats) = measure(|| audio.service(&mut super::callback_scope(), &mut second_audio, &mut |_, _| panic!("wrong runtime")));
    assert!(stats.is_zero());
    assert!(result.faulted);
    control.send_action(3, None).unwrap();
    assert!(audio.service(&mut super::callback_scope(), &mut first_audio, &mut |_, _| panic!("fault must be permanent")).faulted);
    assert!(control.retire(audio).iter().all(|r| r.outcome == Outcome::Faulted));
    first_control.retire(first_audio);
    second_control.retire(second_audio);
}

#[test]
fn production_handler_mutates_the_checked_bridge_without_heap_work() {
    let (plan_control, mut plan_audio) = bridge();
    let (mut control, mut audio) = action_lane::pair(&plan_control);
    control.send_action(1, None).unwrap();
    let mut called = 0;
    let (result, stats) = measure(|| {
        audio.service(&mut super::callback_scope(), &mut plan_audio, &mut |event, bridge| {
            assert!(matches!(event, Event::Action { payload: 1, .. }));
            bridge.set_processing(true, true, true);
            bridge.reset();
            bridge.live_note(soundcraft_model::TrackId(1), 60, 100);
            called += 1;
        })
    });
    assert!(!result.faulted);
    assert!(stats.is_zero());
    assert_eq!(called, 1);
    assert!(plan_audio.engine().monitor_only && plan_audio.engine().recording && plan_audio.engine().no_suspend);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Applied);
    assert!(control.retire(audio).is_empty());
    plan_control.retire(plan_audio);
}

#[test]
fn capacity_and_callback_budget_are_separate_and_credit_waits_for_reclaim() {
    let (mut control, mut audio) = fixture();
    for i in 0..CAPACITY {
        control.send(i, Class::Ordinary, None).unwrap();
    }
    assert_eq!(control.send(999, Class::Ordinary, None), Err(SendError::Full { release_recovery: None }));
    let mut budget = super::callback_scope();
    let mut seen = Vec::with_capacity(CAPACITY);
    let ((), stats) = measure(|| {
        for _ in 0..4 {
            audio.service_generation(&mut budget, generation(1), &mut |e| seen.push(e));
        }
    });
    assert!(stats.is_zero(), "{stats:?}");
    assert_eq!(seen.len(), CALLBACK_ACTIONS);
    assert_eq!(budget.remaining(), 0);
    assert_eq!(control.credits(), 0);
    assert!(control.send(999, Class::Ordinary, None).is_err());
    for i in 0..CALLBACK_ACTIONS {
        let receipt = control.reclaim().unwrap();
        assert_eq!((receipt.payload, receipt.outcome), (i, Outcome::Applied));
    }
    assert_eq!(control.credits(), CALLBACK_ACTIONS);
    let remainder = control.retire(audio);
    assert_eq!(remainder.len(), CAPACITY - CALLBACK_ACTIONS);
    assert!(remainder.iter().all(|r| r.outcome == Outcome::Shutdown));
}

#[test]
fn stop_bypasses_a_waiting_plan_and_a_newer_play_survives() {
    let (mut control, mut audio) = fixture();
    control.send(1, Class::Ordinary, Some(generation(2))).unwrap();
    control.send(2, Class::Ordinary, Some(generation(1))).unwrap();
    let mut events = Vec::with_capacity(8);
    let result = audio.service_generation(&mut super::callback_scope(), generation(1), &mut |e| events.push(e));
    assert!(result.waiting_for_plan);
    assert!(events.is_empty());
    let stop = control.stop().unwrap();
    let play = control.send(3, Class::Ordinary, Some(generation(1))).unwrap();
    let (_, stats) = measure(|| audio.service_generation(&mut super::callback_scope(), generation(1), &mut |e| events.push(e)));
    assert!(stats.is_zero());
    assert_eq!(events, [Event::Stop { sequence: stop }, Event::Action { sequence: play, payload: 3 }]);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Stopped);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Stopped);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Applied);
    assert_eq!(control.recovery_progress(), (stop, stop));
    assert!(control.retire(audio).is_empty());
}

#[test]
fn plan_arrival_unblocks_exact_actions_and_rejection_cancels_unreachable_ones() {
    let (mut control, mut audio) = fixture();
    control.send(1, Class::Ordinary, Some(generation(2))).unwrap();
    let mut events = Vec::with_capacity(8);
    assert!(audio.service_generation(&mut super::callback_scope(), generation(1), &mut |e| events.push(e)).waiting_for_plan);
    audio.service_generation(&mut super::callback_scope(), generation(2), &mut |e| events.push(e));
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Applied);
    control.send(2, Class::Ordinary, Some(generation(1))).unwrap();
    control.send(3, Class::Ordinary, Some(generation(3))).unwrap();
    control.reject_plan(generation(3));
    audio.service_generation(&mut super::callback_scope(), generation(2), &mut |e| events.push(e));
    assert_eq!(events.len(), 1);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::PlanSuperseded);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::PlanRejected);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn full_note_off_requests_recovery_and_cancels_older_queued_note_ons() {
    let (mut control, mut audio) = fixture();
    for i in 0..CAPACITY {
        control.send(i, Class::NoteOn, None).unwrap();
    }
    let Err(SendError::Full { release_recovery: Some(recovery) }) = control.send(0, Class::NoteOff, None) else {
        panic!("full note-off must explicitly report recovery");
    };
    let mut events = Vec::with_capacity(2);
    for _ in 0..CAPACITY.div_ceil(CALLBACK_ACTIONS) {
        let (result, stats) = measure(|| audio.service_generation(&mut super::callback_scope(), generation(1), &mut |e| events.push(e)));
        assert!(stats.is_zero());
        assert!(!result.waiting_for_plan);
    }
    assert_eq!(events, [Event::AllNotesOff { sequence: recovery }]);
    assert_eq!(control.recovery_progress().1, recovery);
    for i in 0..CAPACITY {
        let receipt = control.reclaim().unwrap();
        assert_eq!((receipt.payload, receipt.outcome), (i, Outcome::NotesReleased));
    }
    assert_eq!(control.credits(), CAPACITY);
    control.retire(audio);
}

#[test]
fn handler_unwind_is_not_replayed_and_future_actions_return_faulted() {
    let (mut control, mut audio) = fixture();
    control.send(1, Class::Ordinary, None).unwrap();
    control.send(2, Class::Ordinary, None).unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audio.service_generation(&mut super::callback_scope(), generation(1), &mut |_| panic!("injected action failure"));
        }))
        .is_err()
    );
    control.stop().unwrap();
    let (result, stats) =
        measure(|| audio.service_generation(&mut super::callback_scope(), generation(1), &mut |_| panic!("handler must stay faulted")));
    assert!(stats.is_zero());
    assert!(result.faulted);
    for payload in [1, 2] {
        let receipt = control.reclaim().unwrap();
        assert_eq!((receipt.payload, receipt.outcome), (payload, Outcome::Faulted));
    }
    control.retire(audio);
}

#[test]
fn teardown_preserves_finished_receipts_and_cancels_every_waiting_or_queued_action() {
    let (mut control, mut audio) = fixture();
    for i in 0..7 {
        control.send(i, Class::Ordinary, None).unwrap();
    }
    audio.service_generation(&mut super::callback_scope(), generation(1), &mut |_| {});
    for i in 7..12 {
        control.send(i, Class::Ordinary, Some(generation(99))).unwrap();
    }
    assert!(audio.service_generation(&mut super::callback_scope(), generation(1), &mut |_| {}).waiting_for_plan);
    let receipts = control.retire(audio);
    assert_eq!(receipts.len(), 12);
    for (i, receipt) in receipts.into_iter().enumerate() {
        assert_eq!(receipt.payload, i);
        assert_eq!(receipt.outcome, if i < 7 { Outcome::Applied } else { Outcome::Shutdown });
    }
}

#[test]
fn concurrent_borrowed_audio_endpoint_preserves_all_twenty_thousand_receipts() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let (mut control, mut audio) = fixture();
    let stop = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut sent, mut received, mut ordered) = (0, 0, true);
    let (processed, worker_ordered, heap_free) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let (mut count, mut ordered, mut heap_free) = (0, true, true);
            while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                let (_, stats) = measure(|| {
                    audio.service_generation(&mut super::callback_scope(), generation(1), &mut |event| {
                        if let Event::Action { payload, .. } = event {
                            ordered &= payload == count;
                            count += 1;
                        } else {
                            ordered = false;
                        }
                    })
                });
                heap_free &= stats.is_zero();
                std::thread::yield_now();
            }
            (count, ordered, heap_free)
        });
        while (sent < 20_000 || received < sent) && Instant::now() < deadline {
            if sent < 20_000 && control.send(sent, Class::Ordinary, None).is_ok() {
                sent += 1;
            }
            if let Some(receipt) = control.reclaim() {
                ordered &= receipt.payload == received && receipt.outcome == Outcome::Applied;
                received += 1;
            }
            std::thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        worker.join().unwrap()
    });
    assert_eq!((sent, received, processed), (20_000, 20_000, 20_000));
    assert!(ordered && worker_ordered && heap_free);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn urgent_storms_are_bounded_and_a_deferred_stop_precedes_newer_play() {
    let (mut control, mut audio) = fixture();
    let first = control.stop().unwrap();
    let mut events = Vec::with_capacity(8);
    let mut budget = super::callback_scope();
    audio.service_generation(&mut budget, generation(1), &mut |e| events.push(e));
    let second = control.stop().unwrap();
    let release = control.all_notes_off().unwrap();
    let play = control.send(7, Class::Ordinary, None).unwrap();
    for _ in 0..10 {
        let (result, stats) = measure(|| audio.service_generation(&mut budget, generation(1), &mut |e| events.push(e)));
        assert!(stats.is_zero());
        assert!(result.recovery_deferred);
    }
    assert_eq!(events, [Event::Stop { sequence: first }]);
    assert!(control.reclaim().is_none());
    audio.service_generation(&mut super::callback_scope(), generation(1), &mut |e| events.push(e));
    assert_eq!(
        events,
        [
            Event::Stop { sequence: first },
            Event::Stop { sequence: second },
            Event::AllNotesOff { sequence: release },
            Event::Action { sequence: play, payload: 7 }
        ]
    );
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Applied);
    assert_eq!(control.recovery_progress(), (second, release));
    assert!(control.retire(audio).is_empty());
}

#[test]
fn a_stop_published_during_service_is_observed_before_its_following_play() {
    let (mut control, mut audio) = fixture();
    let initial = control.send(1, Class::Ordinary, None).unwrap();
    let mut events = Vec::with_capacity(4);
    let (mut stop, mut play) = (None, None);
    let (_, stats) = measure(|| {
        audio.service_generation(&mut super::callback_scope(), generation(1), &mut |event| {
            events.push(event);
            if event == (Event::Action { sequence: initial, payload: 1 }) {
                stop = Some(control.stop().unwrap());
                play = Some(control.send(2, Class::Ordinary, None).unwrap());
            }
        })
    });
    assert!(stats.is_zero());
    assert_eq!(
        events,
        [
            Event::Action { sequence: initial, payload: 1 },
            Event::Stop { sequence: stop.unwrap() },
            Event::Action { sequence: play.unwrap(), payload: 2 }
        ]
    );
    assert_eq!(control.retire(audio).len(), 2);
}

#[test]
fn sequence_exhaustion_refuses_without_reusing_or_cancelling_existing_work() {
    let (mut control, mut audio) = fixture();
    control.next = u64::MAX - 1;
    let last = control.send(1, Class::Ordinary, None).unwrap();
    assert_eq!(last.get(), u64::MAX);
    assert_eq!(control.send(2, Class::Ordinary, None), Err(SendError::SequenceExhausted));
    assert_eq!(control.stop(), Err(SendError::SequenceExhausted));
    assert_eq!(control.all_notes_off(), Err(SendError::SequenceExhausted));
    let mut seen = None;
    audio.service_generation(&mut super::callback_scope(), generation(1), &mut |e| seen = Some(e));
    assert_eq!(seen, Some(Event::Action { sequence: last, payload: 1 }));
    assert_eq!(control.reclaim().unwrap().sequence, last);
    assert_eq!(control.credits(), CAPACITY);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn a_defensively_full_return_lane_retains_the_finished_action_without_replaying_it() {
    let (mut control, mut audio) = fixture();
    // Artificially consume the reserved return slots, violating the normal credit invariant.
    // The defensive path must still preserve the real pending receipt.
    for _ in 0..CAPACITY {
        audio
            .actions
            .returned
            .push(super::Receipt { sequence: super::Sequence(0), payload: 0, outcome: Outcome::Shutdown, handler_result: None })
            .unwrap();
    }
    let sequence = control.send(9, Class::Ordinary, None).unwrap();
    let mut calls = 0;
    let mut budget = super::callback_scope();
    for _ in 0..3 {
        assert!(audio.service_generation(&mut budget, generation(1), &mut |_| calls += 1).return_pending);
    }
    assert_eq!(calls, 1);
    for _ in 0..CAPACITY {
        control.actions.returned.pop().unwrap();
    }
    audio.service_generation(&mut budget, generation(1), &mut |_| calls += 1);
    assert_eq!(calls, 1);
    assert_eq!(control.reclaim().unwrap().sequence, sequence);
    assert_eq!(control.credits(), CAPACITY);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn the_real_device_adapter_shares_action_and_urgent_budgets_across_chunks() {
    let (mut control, mut audio) = fixture();
    for i in 0..100 {
        control.send(i, Class::Ordinary, None).unwrap();
    }
    let mut callback = crate::OutputCallback::new(2, 48_000);
    let mut device = vec![0.0f32; 40_000];
    let (mut actions, mut chunks) = (0, 0);
    let ((), stats) = measure(|| {
        callback.fill_with(&mut device, |_, _, budget| {
            audio.service_generation(budget, generation(1), &mut |event| {
                if matches!(event, Event::Action { .. }) {
                    actions += 1;
                }
            });
            chunks += 1;
        })
    });
    assert!(stats.is_zero());
    assert_eq!((actions, chunks), (CALLBACK_ACTIONS, 3));
    assert_eq!(control.credits(), CAPACITY - 100);
    let first = control.stop().unwrap();
    let (mut second, mut play) = (None, None);
    let mut events = Vec::with_capacity(4);
    chunks = 0;
    callback.fill_with(&mut device, |_, _, budget| {
        let result = audio.service_generation(budget, generation(1), &mut |e| events.push(e));
        if chunks == 0 {
            second = Some(control.stop().unwrap());
            play = Some(control.send(999, Class::Ordinary, None).unwrap());
        } else {
            assert!(result.recovery_deferred);
        }
        chunks += 1;
    });
    assert_eq!(events, [Event::Stop { sequence: first }]);
    for _ in 0..2 {
        callback.fill_with(&mut device, |_, _, budget| {
            audio.service_generation(budget, generation(1), &mut |e| events.push(e));
        });
    }
    assert_eq!(
        events,
        [Event::Stop { sequence: first }, Event::Stop { sequence: second.unwrap() }, Event::Action { sequence: play.unwrap(), payload: 999 }]
    );
    assert_eq!(control.retire(audio).len(), 101);
}
