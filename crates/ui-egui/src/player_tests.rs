#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use soundcraft_playback::{
    ActionResult, PlayerCommand, PlayerError, PlayerNotice,
    action_lane::{Outcome, Receipt},
    player::{SessionFailure, SessionNotice, SessionOutcome},
};

fn control() -> soundcraft_playback::player::Control<PlayerCommand, ActionResult> {
    soundcraft_playback::player::endpoints(
        soundcraft_mix::RenderTarget { device: Default::default(), sample_rate: 48_000, max_block: 512 },
        soundcraft_mix::PlanLimits::default(),
    )
    .0
}

#[test]
fn pending_play_does_not_claim_applied_transport_and_refusal_is_visible() {
    let mut control = control();
    let command = PlayerCommand::Play { from: 99, end: None, looped: None };
    let sequence = control.send_action(command, false).unwrap();
    let mut requests = PlayerRequests { play: Some(sequence), ..Default::default() };
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
    assert!(!app.is_playing());
    assert!(requests.pending());
    let refused =
        PlayerNotice::Action(Receipt { sequence, payload: command, outcome: Outcome::Applied, handler_result: Some(ActionResult::InvalidPosition) });
    assert!(requests.notice(refused, &mut app.ui.status, &mut app.engine.transport.recording));
    assert!(requests.play.is_none());
    assert!(!app.is_playing());
    assert!(app.ui.status.contains("InvalidPosition"));
}

#[test]
fn stop_and_release_wait_for_their_own_acknowledgment_frontiers() {
    let mut control = control();
    let stop = control.stop().unwrap();
    let release = control.all_notes_off().unwrap();
    let mut requests = PlayerRequests { stop: Some(stop), release: Some(release), ..Default::default() };
    let mut status = "All MIDI notes off pending".to_string();
    assert!(!requests.recovery(Default::default(), &mut status));
    assert!(requests.pending());
    assert!(requests.recovery((stop, Default::default()), &mut status));
    assert_eq!(requests.release, Some(release));
    assert_eq!(status, "All MIDI notes off pending");
    assert!(!requests.recovery((stop, release), &mut status));
    assert_eq!(status, "All MIDI notes off");
    assert!(!requests.pending());
}

#[test]
fn recording_only_changes_on_its_matching_successful_receipt() {
    let mut control = control();
    let older = control.send_action(PlayerCommand::Recording(false), false).unwrap();
    let current = control.send_action(PlayerCommand::Recording(true), false).unwrap();
    let mut requests = PlayerRequests { recording: Some(current), ..Default::default() };
    let mut status = String::new();
    let mut recording = false;
    let receipt = |sequence, on| {
        PlayerNotice::Action(Receipt {
            sequence,
            payload: PlayerCommand::Recording(on),
            outcome: Outcome::Applied,
            handler_result: Some(ActionResult::Applied),
        })
    };
    requests.notice(receipt(older, true), &mut status, &mut recording);
    assert!(!recording);
    requests.notice(receipt(current, true), &mut status, &mut recording);
    assert!(recording);
    assert!(!requests.pending());
}

#[test]
fn idle_frames_reuse_notice_storage_and_return_credits_without_pending_ui_requests() {
    let mut control = control();
    control.request(std::sync::Arc::new(soundcraft_model::Session::default()), None).unwrap();
    let mut requests = PlayerRequests::default();
    let address = requests.notices.as_ptr();
    let mut status = String::new();
    for _ in 0..32 {
        for _ in 0..requests.notices.len() {
            control.send_action(PlayerCommand::Locate(0), true).unwrap();
        }
        control.stop().unwrap();
        assert!(!requests.pending());
        let count = control.poll_notices(&mut requests.notices);
        assert_eq!(count, requests.notices.len());
        for index in 0..count {
            let notice = requests.notices[index].take().unwrap();
            requests.notice(notice, &mut status, &mut false);
        }
        assert_eq!(requests.notices.as_ptr(), address);
    }
}

#[test]
fn session_and_untracked_action_failures_reach_status() {
    let mut control = control();
    let request = control.request(std::sync::Arc::new(soundcraft_model::Session::default()), None).unwrap();
    let mut requests = PlayerRequests::default();
    let mut status = String::new();
    requests.notice(
        PlayerNotice::Session(SessionNotice { request, outcome: SessionOutcome::Rejected(SessionFailure::FactoryPanicked) }),
        &mut status,
        &mut false,
    );
    assert!(status.contains("FactoryPanicked"));
    let sequence = control.send_action(PlayerCommand::Locate(0), false).unwrap();
    requests.notice(
        PlayerNotice::Action(Receipt { sequence, payload: PlayerCommand::Locate(0), outcome: Outcome::PlanRejected, handler_result: None }),
        &mut status,
        &mut false,
    );
    assert!(status.contains("PlanRejected"));
}

#[test]
fn player_errors_keep_display_text_even_after_a_success_status() {
    let mut requests = PlayerRequests::default();
    let mut status = String::new();
    for error in [PlayerError::Busy, PlayerError::RecordingActive, PlayerError::NoInstrument, PlayerError::SessionBackpressure, PlayerError::Faulted]
    {
        assert!(requests.result::<()>(&mut status, Err(error)).is_none());
        assert!(status.contains(&error.to_string()));
    }
    status = "Recorded a take".into();
    requests.show_error(&mut status);
    assert!(status.contains(&PlayerError::Busy.to_string()));
    assert!(status.contains(&PlayerError::Faulted.to_string()));
}

#[test]
fn failed_state_capture_aborts_save_and_preserves_document() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
    app.run("track.new", serde_json::json!({"name": "Unsaved", "count": 1})).unwrap();
    let before = serde_json::to_value(app.engine.session()).unwrap();
    let revision = app.engine.revision;
    let mut saved = false;
    let result = app.store_plugin_states(Err(PlayerError::SessionPending)).map(|()| {
        saved = true;
    });
    assert!(result.is_err());
    assert!(!saved);
    assert_eq!(app.ui.status, PlayerError::SessionPending.to_string());
    assert_eq!(app.engine.revision, revision);
    assert_eq!(serde_json::to_value(app.engine.session()).unwrap(), before);
    assert!(app.engine.is_dirty());
}
