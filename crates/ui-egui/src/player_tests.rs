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

#[test]
fn atomic_record_start_receipt_updates_transport_and_clock_refusal_collects_the_take() {
    use soundcraft_playback::{record::start, timeline_map::Position};
    for success in [false, true] {
        let mut control = control();
        let (mut starts, _) = start::pair(soundcraft_mix::EngineEpoch(1));
        let reader = starts.prepare().unwrap();
        let command = PlayerCommand::Record { ticket: reader.ticket(), from: Some(500), end: None, looped: None };
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
            ActionResult::RecordingClock(soundcraft_playback::device_clock::Unavailable::Stale)
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
            assert!(status.contains("RecordingClock"));
        }
    }
}

// The fixture uses a floating mock GUI; macOS GUI calls need a main-thread runner.
#[cfg(windows)]
mod track_render_state {
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
    fn run_alias(alias: &str) {
        fixture();
        let mut engine = soundcraft_engine::Engine::default();
        let session = engine.session_mut();
        let track = session.add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Stereo, Some("State"));
        let mut insert = soundcraft_model::Insert::new("clap:org.soundcraft.test.gain");
        insert.set_state_bytes(&state(1.0));
        session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
        let source = soundcraft_engine::io::add_source(
            session,
            "DC",
            soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![1.0; 4_800]; 2] },
            None,
            soundcraft_audio_io::FileFormat::Wav,
        );
        let clip = session.new_clip_id();
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(soundcraft_model::Clip::audio(clip, "DC", source, 0, 0, 4_800));
        let player = soundcraft_playback::Player::prepare_device_free(engine.session_arc()).unwrap();
        let mut app = SoundApp::new(engine, Some(player), crate::Services::default());
        for _ in 0..4 {
            app.player.as_ref().unwrap().step_device_free();
            app.apply_editor_edits();
            app.poll_player();
            app.sync_player_session();
        }
        let player = app.player.as_ref().unwrap();
        // This fixture's floating GUI has no native window. Showing it edits gain internally.
        player.open_editor(track, 0).unwrap();
        player.step_device_free();
        assert_eq!(player.capture_states(&app.engine.session_arc()).unwrap()[0].2, state(0.25));
        assert_eq!(app.engine.session().tracks[0].mixer.inserts[0].as_ref().unwrap().state_bytes().unwrap(), state(1.0));
        let result = app.run(alias, serde_json::json!({"tracks": [track.0]})).unwrap();
        let id = soundcraft_model::TrackId(result["tracks"][0].as_u64().unwrap());
        let source = app.engine.session().track(id).unwrap().clips()[0].source().unwrap();
        let sample = app.engine.session().pool.get(source).unwrap().channel(0).unwrap()[200];
        assert!((sample - 0.25).abs() < 1e-5, "{alias} rendered stale saved plugin state: {sample}");
    }
    #[test]
    fn track_commit_captures_live_state_before_rendering() {
        run_alias("track.commit");
    }
    #[test]
    fn track_bounce_captures_live_state_before_rendering() {
        run_alias("track.bounce");
    }
    #[test]
    fn audio_reopen_preserves_live_state_and_refuses_pending_document_without_teardown() {
        fixture();
        let mut engine = soundcraft_engine::Engine::default();
        let session = engine.session_mut();
        let track = session.add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Stereo, Some("State"));
        let mut insert = soundcraft_model::Insert::new("clap:org.soundcraft.test.gain");
        insert.set_state_bytes(&state(1.0));
        session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
        let player = soundcraft_playback::Player::prepare_device_free(engine.session_arc()).unwrap();
        let mut app = SoundApp::new(engine, Some(player), crate::Services::default());
        for _ in 0..4 {
            app.player.as_ref().unwrap().step_device_free();
            app.apply_editor_edits();
            app.poll_player();
            app.sync_player_session();
        }
        let player = app.player.as_ref().unwrap();
        player.open_editor(track, 0).unwrap(); // fixture's window-free GUI edits gain
        player.step_device_free();
        let before = player.capture_states(&app.engine.session_arc()).unwrap()[0].2.clone();
        assert_eq!(before, state(0.25));
        assert_eq!(app.engine.session().track(track).unwrap().mixer.inserts[0].as_ref().unwrap().state_bytes().unwrap(), state(1.0));
        app.apply_audio_config_using(soundcraft_playback::AudioConfig::default(), |session, _| {
            soundcraft_playback::Player::prepare_device_free(session).unwrap()
        })
        .unwrap();
        for _ in 0..4 {
            app.player.as_ref().unwrap().step_device_free();
            app.apply_editor_edits();
            app.poll_player();
            app.sync_player_session();
        }
        let after = app.player.as_ref().unwrap().capture_states(&app.engine.session_arc()).unwrap()[0].2.clone();
        assert_eq!(after, before, "Apply audio discarded unsaved native plug-in state");
        assert!(app.reconnect_audio_using(true, |session, _| soundcraft_playback::Player::prepare_device_free(session).unwrap()).unwrap());
        for _ in 0..4 {
            app.player.as_ref().unwrap().step_device_free();
            app.apply_editor_edits();
            app.poll_player();
            app.sync_player_session();
        }
        let confirmed = app.engine.session_arc();
        assert_eq!(app.player.as_ref().unwrap().capture_states(&confirmed).unwrap()[0].2, before);
        app.engine.session_mut().track_mut(track).unwrap().name = "Pending".into();
        let old_config = app.ui.audio.clone();
        let changed = soundcraft_playback::AudioConfig { buffer_size: Some(128), ..old_config.clone() };
        assert!(app.apply_audio_config_using(changed, |_, _| panic!("pending document must not open a device")).is_err());
        assert_eq!(app.ui.audio, old_config);
        assert!(app.reconnect_audio_using(true, |_, _| panic!("pending document must not tear down the player")).is_err());
        assert!(app.player.as_ref().unwrap().has_requested_session(&confirmed));
    }
}
