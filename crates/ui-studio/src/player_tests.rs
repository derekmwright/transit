#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use soundcraft_playback::{
    ActionResult, PlayerCommand, PlayerError, PlayerNotice,
    action_lane::{Outcome, Receipt},
    player::{SessionFailure, SessionNotice, SessionOutcome},
};

fn control() -> soundcraft_playback::player::Control<PlayerCommand, ActionResult> {
    soundcraft_playback::player::endpoints(
        soundcraft_mix::RenderTarget { device: Default::default(), sample_rate: 48_000, max_block: 512, out_channels: 0, in_channels: 0 },
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
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
    assert!(!app.is_playing());
    assert!(requests.pending());
    let refused =
        PlayerNotice::Action(Receipt { sequence, payload: command, outcome: Outcome::Applied, handler_result: Some(ActionResult::InvalidPosition) });
    assert!(requests.notice(refused, &mut app.status, &mut app.engine.transport.recording));
    assert!(requests.play.is_none());
    assert!(!app.is_playing());
    assert!(app.status.contains("position is out of range"), "{}", app.status);
    assert!(!app.status.contains("InvalidPosition"));
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
    assert!(status.contains("rejected that change"), "{status}");
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
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
    app.run("track.new", serde_json::json!({"name": "Unsaved", "count": 1})).unwrap();
    let before = serde_json::to_value(app.engine.session()).unwrap();
    let revision = app.engine.revision;
    let mut saved = false;
    let result = app.store_plugin_states(Err(PlayerError::SessionPending)).map(|()| {
        saved = true;
    });
    assert!(result.is_err());
    assert!(!saved);
    assert_eq!(app.status, PlayerError::SessionPending.to_string());
    assert_eq!(app.engine.revision, revision);
    assert_eq!(serde_json::to_value(app.engine.session()).unwrap(), before);
    assert!(app.engine.is_dirty());
}

#[test]
fn atomic_record_start_receipt_updates_transport_and_clock_refusal_collects_the_take() {
    use soundcraft_playback::{record::start, timeline_map::Position};
    for success in [false, true] {
        let mut control = control();
        let (mut starts, _) = start::pair(soundcraft_mix::EngineEpoch(1));
        let reader = starts.prepare().unwrap();
        let command = PlayerCommand::Record { ticket: reader.ticket(), from: Some(500), end: None, looped: None, count_in: None };
        let sequence = control.send_action(command, false).unwrap();
        let mut requests = PlayerRequests { play: Some(sequence), recording: Some(sequence), ..Default::default() };
        let mut status = "Record pending".to_owned();
        let mut recording = false;
        let result = if success {
            ActionResult::RecordingStarted(start::Point {
                host_ns: 100,
                output_frame: 30,
                position: Position { sample: 500, fraction: 0, denominator: 1 },
            })
        } else {
            ActionResult::RecordingClock(soundcraft_playback::device_clock::Unavailable::WarmingUp)
        };
        let collect = requests.notice(
            PlayerNotice::Action(Receipt { sequence, payload: command, outcome: Outcome::Applied, handler_result: Some(result) }),
            &mut status,
            &mut recording,
        );
        assert_eq!(collect, !success);
        assert_eq!(recording, success);
        assert!(!requests.pending());
        if success {
            assert_eq!(status, "Recording");
        } else {
            assert_eq!(status, "Recording can't start: the audio clock is still warming up; try again in a moment.");
            assert!(!status.contains("RecordingClock"));
        }
    }
}

// The fixture uses a floating mock GUI; macOS GUI calls need a main-thread runner.
#[cfg(windows)]
mod reopen_state {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        process::Command,
        sync::OnceLock,
    };

    fn fixture() {
        static READY: OnceLock<()> = OnceLock::new();
        READY.get_or_init(|| {
            soundcraft_clap_host::set_scan_extra_dirs_only(true);
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
            let target = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
                .join("ui-track-state-fixture");
            let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
                .args(["build", "--quiet", "--manifest-path"])
                .arg(manifest)
                .arg("--target-dir")
                .arg(&target)
                .env_remove("RUSTFLAGS")
                .status()
                .unwrap();
            assert!(status.success());
            let dir = target.join(format!("plugins-{}", std::process::id()));
            let dest = dir.join("Fixture.clap");
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(target.join("debug/clap_fixture.dll"), dest).unwrap();
            assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 2);
        });
    }
    fn state(gain: f64) -> Vec<u8> {
        [gain.to_le_bytes(), 0.0f64.to_le_bytes()].concat()
    }
    /// Frames until the player's instances are made on the plug-in host thread (ADR 0018) and
    /// its plan is confirmed for the app's document, then a few more.
    fn settle(app: &mut StudioApp, track: soundcraft_model::TrackId) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut extra = 4;
        while extra > 0 {
            app.player.as_ref().unwrap().step_device_free();
            app.apply_editor_edits();
            app.poll_player();
            app.sync_player_session();
            let p = app.player.as_ref().unwrap();
            if !p.any_loading() && p.has_editor(track, 0) && p.capture_states(&app.engine.session_arc()).is_ok() {
                extra -= 1;
            }
            assert!(std::time::Instant::now() < deadline, "the fixture never loaded");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[test]
    fn audio_reopen_preserves_live_state_and_refuses_pending_document_without_teardown() {
        // The host registry is process-global. Keep this real CLAP fixture out
        // of the other UI tests' deliberately empty plugin catalog.
        if std::env::var_os("TRANSIT_REOPEN_FIXTURE_CHILD").is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::player_tests::reopen_state::audio_reopen_preserves_live_state_and_refuses_pending_document_without_teardown",
                    "--nocapture",
                ])
                .env("TRANSIT_REOPEN_FIXTURE_CHILD", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            // The filter must name the test exactly, or the child runs nothing and passes.
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the child ran no test:
{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        fixture();
        let mut engine = soundcraft_engine::Engine::default();
        let session = engine.session_mut();
        let track = session.add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Stereo, Some("State"));
        let mut insert = soundcraft_model::Insert::new("clap:org.soundcraft.test.gain");
        insert.set_state_bytes(&state(1.0));
        session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
        let player = soundcraft_playback::Player::prepare_device_free(engine.session_arc()).unwrap();
        let mut app = StudioApp::new(engine, Some(player), crate::Services::default());
        settle(&mut app, track);
        let player = app.player.as_ref().unwrap();
        // The fixture's window-free GUI edits gain when it opens; the edit reaches the plug-in's
        // state with the next block. The open runs on the plug-in host thread (ADR 0018), so
        // render until the state read through that thread shows it.
        player.open_editor(track, 0).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let before = loop {
            let player = app.player.as_ref().unwrap();
            player.step_device_free();
            let now = player.capture_states(&app.engine.session_arc()).unwrap()[0].2.clone();
            if now == state(0.25) || std::time::Instant::now() > deadline {
                break now;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(before, state(0.25));
        assert_eq!(app.engine.session().track(track).unwrap().mixer.inserts[0].as_ref().unwrap().state_bytes().unwrap(), state(1.0));
        assert!(app.reconnect_audio_using(true, |session, _| soundcraft_playback::Player::prepare_device_free(session).unwrap()).unwrap());
        settle(&mut app, track);
        let after = app.player.as_ref().unwrap().capture_states(&app.engine.session_arc()).unwrap()[0].2.clone();
        assert_eq!(after, before, "Apply audio discarded unsaved native plug-in state");
        assert!(app.reconnect_audio_using(true, |session, _| soundcraft_playback::Player::prepare_device_free(session).unwrap()).unwrap());
        settle(&mut app, track);
        let confirmed = app.engine.session_arc();
        assert_eq!(app.player.as_ref().unwrap().capture_states(&confirmed).unwrap()[0].2, before);
        app.engine.session_mut().track_mut(track).unwrap().name = "Pending".into();
        let old_config = app.ui.audio.clone();
        let changed = soundcraft_playback::AudioConfig { buffer_size: Some(128), ..old_config.clone() };
        assert!(app.apply_audio(changed).is_err());
        assert_eq!(app.ui.audio, old_config);
        assert!(app.reconnect_audio_using(true, |_, _| panic!("pending document must not tear down the player")).is_err());
        assert!(app.player.as_ref().unwrap().has_requested_session(&confirmed));
    }
}

#[test]
fn refusals_read_as_sentences_and_keep_the_variant_out_of_the_status_line() {
    use soundcraft_playback::device_clock::Unavailable;
    let text = |result| crate::host::refusal_text(Outcome::Applied, Some(result));
    assert_eq!(
        text(ActionResult::RecordingClock(Unavailable::WarmingUp)),
        "Recording can't start: the audio clock is still warming up; try again in a moment."
    );
    assert_eq!(text(ActionResult::RecordingClock(Unavailable::WrongClock)), "Recording can't start: no audio input is open on this device.");
    assert_eq!(text(ActionResult::RecordingNotPlaying), "Recording can't start: the transport is not playing.");
    for result in [ActionResult::RecordingCancelled, ActionResult::InvalidPosition, ActionResult::MidiFull, ActionResult::MidiBuffered] {
        let message = text(result);
        assert!(!message.contains("Recording(") && !message.contains("Midi") && !message.contains("Invalid"), "{message}");
    }
}
