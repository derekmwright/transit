//! Fresh-context review regression: the actual exit handler must keep verified audio preferences.

use super::*;

#[test]
fn review_quit_before_verification_keeps_previous_good_audio() {
    const CHILD: &str = "TRANSIT_AUDIO_REVIEW_QUIT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = std::env::temp_dir().join(format!(
            "transit-audio-review-quit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "audio_review_tests::review_quit_before_verification_keeps_previous_good_audio", "--nocapture"])
            .env(CHILD, "1")
            .env("TRANSIT_CONFIG_DIR", &dir)
            .env("SOUNDCRAFT_CONFIG_DIR", dir.join("unused-soundcraft"))
            .env_remove("TRANSIT_NO_PREFS")
            .env_remove("SOUNDCRAFT_NO_PREFS")
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        return;
    }

    let path = prefs_path().unwrap();
    let mut studio = StudioApp::new(Engine::default(), None, Services::default());
    studio.ui.audio.output = Some("previous verified output".into());
    config::save_ui(&path, &studio.ui).unwrap();
    let previous = studio.ui.audio.clone();
    studio.audio_devices.fake = soundcraft_ui_studio::audio_devices::fake_from_json(&serde_json::json!({
        "systems": [{"name": "WASAPI", "is_default": true, "outputs": [{"name": "untested output", "is_default": true}]}]
    }))
    .unwrap();
    studio.apply_audio(soundcraft_playback::AudioConfig { output: Some("untested output".into()), ..Default::default() }).unwrap();
    assert_eq!(config::load_ui(&path).unwrap().unwrap().audio, previous, "Apply itself preserves the last good choice");
    assert!(studio.player.is_none(), "no real audio device may open in this review");
    studio.ui.window_size = [1234.0, 789.0];
    studio.ui.count_in = !studio.ui.count_in;
    let mut expected = studio.ui.clone();
    expected.audio = previous.clone();
    // Zero callbacks, zero elapsed frames: execute the real desktop exit handler.
    let mut app = App(studio, None);
    eframe::App::on_exit(&mut app);
    assert_eq!(config::load_ui(&path).unwrap().unwrap().audio, previous, "quit must not bypass callback verification");
    assert_eq!(
        serde_json::to_value(config::load_ui(&path).unwrap().unwrap()).unwrap(),
        serde_json::to_value(expected).unwrap(),
        "quit still saves the current values of every other preference"
    );
}
