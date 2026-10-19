//! `ui.audio_device`: the audio device choice is kept only once the devices reopen with it.
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp};

#[test]
fn invalid_or_recording_device_changes_preserve_the_applied_choice() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let before = app.ui.audio.clone();

    let r = app.run("ui.audio_device", json!({})).unwrap();
    assert_eq!(r["config"], json!(before), "no change: just the choice and the device");

    assert!(app.run("ui.audio_device", json!({"sample_rate": 12})).is_err());
    assert_eq!(app.ui.audio, before, "an out-of-range rate changes nothing");

    app.engine.transport.recording = true;
    let err = app.run("ui.audio_device", json!({"buffer_size": 256, "output": "Elsewhere"})).unwrap_err();
    assert!(err.contains("stop recording"), "{err}");
    assert_eq!(app.ui.audio, before, "refused while recording: the saved choice stays the open device's");
}

#[test]
#[ignore = "opens a physical audio device; run explicitly when the device is available"]
fn applying_an_audio_choice_reopens_the_device() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    // Opens the chosen device, or the default, or a silent clock without one.
    let r = app.run("ui.audio_device", json!({"buffer_size": 256})).unwrap();
    assert_eq!(app.ui.audio.buffer_size, Some(256));
    assert_eq!(r["config"]["buffer_size"], 256);
    assert!(r["device"].is_object(), "{r}");
}
