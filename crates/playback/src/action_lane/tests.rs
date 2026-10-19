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

#[test]
fn capacity_and_callback_budget_are_separate_and_credit_waits_for_reclaim() {
    let (mut control, mut audio) = action_lane::pair();
    for i in 0..CAPACITY {
        control.send(i, Class::Ordinary, None).unwrap();
    }
    assert_eq!(control.send(999, Class::Ordinary, None), Err(SendError::Full { release_recovery: None }));
    let mut budget = super::callback_scope();
    let mut seen = Vec::with_capacity(CAPACITY);
    let ((), stats) = measure(|| {
        for _ in 0..4 {
            audio.service(&mut budget, generation(1), &mut |e| seen.push(e));
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
    let (mut control, mut audio) = action_lane::pair();
    control.send(1, Class::Ordinary, Some(generation(2))).unwrap();
    control.send(2, Class::Ordinary, Some(generation(1))).unwrap();
    let mut events = Vec::with_capacity(8);
    let result = audio.service(&mut super::callback_scope(), generation(1), &mut |e| events.push(e));
    assert!(result.waiting_for_plan);
    assert!(events.is_empty());
    let stop = control.stop().unwrap();
    let play = control.send(3, Class::Ordinary, Some(generation(1))).unwrap();
    let (_, stats) = measure(|| audio.service(&mut super::callback_scope(), generation(1), &mut |e| events.push(e)));
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
    let (mut control, mut audio) = action_lane::pair();
    control.send(1, Class::Ordinary, Some(generation(2))).unwrap();
    let mut events = Vec::with_capacity(8);
    assert!(audio.service(&mut super::callback_scope(), generation(1), &mut |e| events.push(e)).waiting_for_plan);
    audio.service(&mut super::callback_scope(), generation(2), &mut |e| events.push(e));
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Applied);
    control.send(2, Class::Ordinary, Some(generation(1))).unwrap();
    control.send(3, Class::Ordinary, Some(generation(3))).unwrap();
    control.reject_plan(generation(3));
    audio.service(&mut super::callback_scope(), generation(2), &mut |e| events.push(e));
    assert_eq!(events.len(), 1);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::PlanSuperseded);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::PlanRejected);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn rejected_note_off_recovers_before_a_newer_note_on() {
    let (mut control, mut audio) = action_lane::pair();
    let off = control.send(0, Class::NoteOff, Some(generation(3))).unwrap();
    let on = control.send(1, Class::NoteOn, Some(generation(2))).unwrap();
    control.reject_plan(generation(3));
    let mut events = Vec::with_capacity(4);
    let (_, stats) = measure(|| audio.service(&mut super::callback_scope(), generation(2), &mut |e| events.push(e)));
    assert!(stats.is_zero());
    assert_eq!(events, [Event::AllNotesOff { sequence: off }, Event::Action { sequence: on, payload: 1 }]);
    let receipt = control.reclaim().unwrap();
    assert_eq!(receipt.outcome, Outcome::PlanRejected);
    assert!(receipt.release_recovery);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Applied);
    control.retire(audio);
}

#[test]
fn full_note_off_requests_recovery_and_cancels_older_queued_note_ons() {
    let (mut control, mut audio) = action_lane::pair();
    for i in 0..CAPACITY {
        control.send(i, Class::NoteOn, Some(generation(9))).unwrap();
    }
    let Err(SendError::Full { release_recovery: Some(recovery) }) = control.send(0, Class::NoteOff, Some(generation(9))) else {
        panic!("full note-off must explicitly report recovery");
    };
    let mut events = Vec::with_capacity(2);
    for _ in 0..CAPACITY.div_ceil(CALLBACK_ACTIONS) {
        let (result, stats) = measure(|| audio.service(&mut super::callback_scope(), generation(1), &mut |e| events.push(e)));
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
    let (mut control, mut audio) = action_lane::pair();
    control.send(1, Class::Ordinary, None).unwrap();
    control.send(2, Class::Ordinary, None).unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audio.service(&mut super::callback_scope(), generation(1), &mut |_| panic!("injected action failure"));
        }))
        .is_err()
    );
    control.stop().unwrap();
    let (result, stats) = measure(|| audio.service(&mut super::callback_scope(), generation(1), &mut |_| panic!("handler must stay faulted")));
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
    let (mut control, mut audio) = action_lane::pair();
    for i in 0..7 {
        control.send(i, Class::Ordinary, None).unwrap();
    }
    audio.service(&mut super::callback_scope(), generation(1), &mut |_| {});
    for i in 7..12 {
        control.send(i, Class::Ordinary, Some(generation(99))).unwrap();
    }
    assert!(audio.service(&mut super::callback_scope(), generation(1), &mut |_| {}).waiting_for_plan);
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
    let (mut control, mut audio) = action_lane::pair();
    let stop = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut sent, mut received, mut ordered) = (0, 0, true);
    let (processed, worker_ordered, heap_free) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let (mut count, mut ordered, mut heap_free) = (0, true, true);
            while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                let (_, stats) = measure(|| {
                    audio.service(&mut super::callback_scope(), generation(1), &mut |event| {
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
    let (mut control, mut audio) = action_lane::pair();
    let first = control.stop().unwrap();
    let mut events = Vec::with_capacity(8);
    let mut budget = super::callback_scope();
    audio.service(&mut budget, generation(1), &mut |e| events.push(e));
    let second = control.stop().unwrap();
    let release = control.all_notes_off().unwrap();
    let play = control.send(7, Class::Ordinary, None).unwrap();
    for _ in 0..10 {
        let (result, stats) = measure(|| audio.service(&mut budget, generation(1), &mut |e| events.push(e)));
        assert!(stats.is_zero());
        assert!(result.recovery_deferred);
    }
    assert_eq!(events, [Event::Stop { sequence: first }]);
    assert!(control.reclaim().is_none());
    audio.service(&mut super::callback_scope(), generation(1), &mut |e| events.push(e));
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
    let (mut control, mut audio) = action_lane::pair();
    let initial = control.send(1, Class::Ordinary, None).unwrap();
    let mut events = Vec::with_capacity(4);
    let (mut stop, mut play) = (None, None);
    let (_, stats) = measure(|| {
        audio.service(&mut super::callback_scope(), generation(1), &mut |event| {
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
fn rejected_note_offs_share_the_release_budget_and_stay_pending_until_recovered() {
    let (mut control, mut audio) = action_lane::pair();
    let first = control.send(1, Class::NoteOff, Some(generation(2))).unwrap();
    let second = control.send(2, Class::NoteOff, Some(generation(2))).unwrap();
    let on = control.send(3, Class::NoteOn, Some(generation(1))).unwrap();
    control.reject_plan(generation(2));
    let mut events = Vec::with_capacity(4);
    let mut budget = super::callback_scope();
    for _ in 0..3 {
        assert!(audio.service(&mut budget, generation(1), &mut |e| events.push(e)).recovery_deferred);
    }
    assert_eq!(events, [Event::AllNotesOff { sequence: first }]);
    assert!(control.reclaim().unwrap().release_recovery);
    assert!(control.reclaim().is_none());
    audio.service(&mut super::callback_scope(), generation(1), &mut |e| events.push(e));
    assert_eq!(events, [Event::AllNotesOff { sequence: first }, Event::AllNotesOff { sequence: second }, Event::Action { sequence: on, payload: 3 }]);
    assert!(control.reclaim().unwrap().release_recovery);
    assert_eq!(control.reclaim().unwrap().outcome, Outcome::Applied);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn sequence_exhaustion_refuses_without_reusing_or_cancelling_existing_work() {
    let (mut control, mut audio) = action_lane::pair();
    control.next = u64::MAX - 1;
    let last = control.send(1, Class::Ordinary, None).unwrap();
    assert_eq!(last.get(), u64::MAX);
    assert_eq!(control.send(2, Class::Ordinary, None), Err(SendError::SequenceExhausted));
    assert_eq!(control.stop(), Err(SendError::SequenceExhausted));
    assert_eq!(control.all_notes_off(), Err(SendError::SequenceExhausted));
    let mut seen = None;
    audio.service(&mut super::callback_scope(), generation(1), &mut |e| seen = Some(e));
    assert_eq!(seen, Some(Event::Action { sequence: last, payload: 1 }));
    assert_eq!(control.reclaim().unwrap().sequence, last);
    assert_eq!(control.credits(), CAPACITY);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn a_defensively_full_return_lane_retains_the_finished_action_without_replaying_it() {
    let (mut control, mut audio) = action_lane::pair();
    // Artificially consume the reserved return slots, violating the normal credit invariant.
    // The defensive path must still preserve the real pending receipt.
    for _ in 0..CAPACITY {
        audio
            .returned
            .push(super::Receipt { sequence: super::Sequence(0), payload: 0, outcome: Outcome::Shutdown, release_recovery: false })
            .unwrap();
    }
    let sequence = control.send(9, Class::Ordinary, None).unwrap();
    let mut calls = 0;
    let mut budget = super::callback_scope();
    for _ in 0..3 {
        assert!(audio.service(&mut budget, generation(1), &mut |_| calls += 1).return_pending);
    }
    assert_eq!(calls, 1);
    for _ in 0..CAPACITY {
        control.returned.pop().unwrap();
    }
    audio.service(&mut budget, generation(1), &mut |_| calls += 1);
    assert_eq!(calls, 1);
    assert_eq!(control.reclaim().unwrap().sequence, sequence);
    assert_eq!(control.credits(), CAPACITY);
    assert!(control.retire(audio).is_empty());
}

#[test]
fn the_real_device_adapter_shares_action_and_urgent_budgets_across_chunks() {
    let (mut control, mut audio) = action_lane::pair();
    for i in 0..100 {
        control.send(i, Class::Ordinary, None).unwrap();
    }
    let mut callback = crate::OutputCallback::new(2, 48_000);
    let mut device = vec![0.0f32; 40_000];
    let (mut actions, mut chunks) = (0, 0);
    let ((), stats) = measure(|| {
        callback.fill_with(&mut device, |_, _, budget| {
            audio.service(budget, generation(1), &mut |event| {
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
        let result = audio.service(budget, generation(1), &mut |e| events.push(e));
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
            audio.service(budget, generation(1), &mut |e| events.push(e));
        });
    }
    assert_eq!(
        events,
        [Event::Stop { sequence: first }, Event::Stop { sequence: second.unwrap() }, Event::Action { sequence: play.unwrap(), payload: 999 }]
    );
    assert_eq!(control.retire(audio).len(), 101);
}
