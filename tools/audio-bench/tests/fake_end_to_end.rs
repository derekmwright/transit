//! The harness end to end on `FakeBackend`: scripted cadence, jitter, one gap, a software
//! loopback with a known delay. No device is opened.
#![forbid(unsafe_code)]

use audio_bench::{Config, Kind, analysis::Measurements, fake::Script, run, session::Request};

fn config(period: u32, seconds: f64, loopback: bool, script: Script) -> Config {
    Config {
        kind: Kind::Fake,
        system: None,
        request: Request {
            device: None,
            in_device: None,
            rate: 48_000,
            period: Some(period),
            loopback,
            mode: transit_audio_backend::Mode::Shared,
            out_ch: 1,
            in_ch: 1,
        },
        seconds,
        fake: script,
        out: None,
        settings: None,
    }
}

fn script(jitter_us: u32, gap_at_s: Option<f64>, delay_frames: u32) -> Script {
    Script { seconds: 0.0, jitter_us, gap_at_s, delay_frames }
}

fn measure(config: &Config) -> Measurements {
    run(config).expect("the fake run").measurements
}

#[test]
fn a_clean_run_reports_the_period_no_jitter_no_xruns_and_the_exact_delay() {
    let m = measure(&config(64, 4.0, true, script(0, None, 144)));
    assert_eq!((m.achieved_frames, m.min_frames, m.max_frames, m.rate), (64, 64, 64, 48_000));
    assert!((m.achieved_ms - 64.0 * 1000.0 / 48_000.0).abs() < 1e-9);
    assert_eq!(m.callbacks, 3000);
    assert!(m.jitter_ms.max < 1e-6, "{:?}", m.jitter_ms);
    assert_eq!((m.xruns_out, m.xruns_in), (0, Some(0)));
    let trip = m.round_trip.expect("loopback");
    assert_eq!((trip.clicks, trip.paired), (8, 8), "every click is found");
    for ms in [trip.min_ms, trip.median_ms, trip.p95_ms] {
        assert!((ms - 3.0).abs() < 1e-6, "144 frames at 48 kHz is 3 ms, got {ms}");
    }
    assert!((trip.median_frames - 144.0).abs() < 1e-4, "{}", trip.median_frames);
}

#[test]
fn the_loopback_reports_the_input_peak_per_channel() {
    let m = measure(&config(64, 2.0, true, script(0, None, 144)));
    assert_eq!(m.input_peaks, vec![0.5, 0.0], "the click level on the looped channel, silence on the other");
    assert_eq!(audio_bench::calibration::input_peak_line(&m.input_peaks, 1), "input peak on ch1: -6.0 dBFS");
}

#[test]
fn the_delay_is_recovered_to_the_frame_at_other_periods_and_delays() {
    for (period, delay) in [(32, 145), (128, 1000), (64, 1), (256, 4799)] {
        let trip = measure(&config(period, 3.0, true, script(0, None, delay))).round_trip.expect("loopback");
        assert!(trip.paired >= 5, "period {period}: {trip:?}");
        assert!((trip.median_frames - f64::from(delay)).abs() < 1e-3, "period {period} delay {delay}: {trip:?}");
        assert!((trip.min_frames - trip.p95_frames).abs() < 1e-3);
    }
}

#[test]
fn jitter_and_one_gap_are_reported() {
    let m = measure(&config(64, 4.0, true, script(100, Some(2.0), 144)));
    let period_ms = 64.0 * 1000.0 / 48_000.0;
    assert_eq!(m.achieved_frames, 64);
    assert_eq!((m.xruns_out, m.xruns_in), (1, Some(1)), "one gap on each stream");
    assert!((m.xruns_per_minute() - 15.0).abs() < 0.1, "one xrun in four seconds: {}", m.xruns_per_minute());
    // Arrival jitter is within +-0.1 ms, so an interval deviates by at most 0.2 ms; the gap
    // interval is one period longer.
    assert!(m.jitter_ms.p50 > 0.0 && m.jitter_ms.p50 <= 0.2 + 1e-6, "{:?}", m.jitter_ms);
    assert!(m.jitter_ms.p95 <= 0.2 + 1e-6, "{:?}", m.jitter_ms);
    assert!((m.jitter_ms.max - period_ms).abs() <= 0.2 + 1e-6, "{:?}", m.jitter_ms);
    // The round trip carries both streams' dispatch jitter (the documented bias), 0.2 ms at most.
    let trip = m.round_trip.expect("loopback");
    assert_eq!(trip.paired, 8);
    assert!(trip.min_ms >= 3.0 - 0.2 - 1e-6 && trip.p95_ms <= 3.0 + 0.2 + 1e-6, "{trip:?}");
    assert!((trip.median_ms - 3.0).abs() <= 0.2 + 1e-6, "{trip:?}");
}

#[test]
fn without_loopback_there_is_no_round_trip_and_the_table_says_so() {
    let row = run(&config(64, 2.0, false, script(0, None, 144))).expect("the fake run");
    assert!(row.measurements.round_trip.is_none() && row.measurements.xruns_in.is_none());
    assert_eq!((row.backend.as_str(), row.system.as_str(), row.requested), ("fake", "Fake", Some(64)));
    let markdown = row.markdown();
    assert!(markdown.contains("| n/a | n/a |") && markdown.contains("| 0/n/a |"), "{markdown}");
}

#[test]
fn the_default_period_and_bad_channels() {
    let mut c = config(64, 1.0, false, script(0, None, 0));
    c.request.period = None;
    assert_eq!(measure(&c).achieved_frames, audio_bench::FAKE_DEFAULT_PERIOD);
    c.request.out_ch = 0;
    assert!(run(&c).is_err());
}

fn calibration_of(config: &Config) -> audio_bench::calibration::Figures {
    run(config).expect("the fake run").calibration.expect("a loopback on a native-style backend measures a calibration").expect("clicks were found")
}

#[test]
fn the_frame_time_calibration_recovers_the_scripted_delay_to_a_frame() {
    for (period, delay) in [(64, 144), (32, 145), (128, 1000), (64, 1), (256, 4799)] {
        let f = calibration_of(&config(period, 3.0, true, script(0, None, delay)));
        assert_eq!(f.round_trip_frames, delay, "period {period}: {f:?}");
        assert!(f.spread_frames <= 1, "period {period}: {f:?}");
        assert!(f.paired >= 5 && f.paired * 2 >= f.clicks, "{f:?}");
    }
}

#[test]
fn arrival_jitter_widens_the_bound_but_not_the_delay() {
    // Stamps jitter by +-100 us (4.8 frames) per stream, so a round trip is off by up to 9.6 frames either way.
    let f = calibration_of(&config(64, 4.0, true, script(100, None, 144)));
    assert!(f.round_trip_frames.abs_diff(144) <= 10, "{f:?}");
    assert!((2..=20).contains(&f.spread_frames), "{f:?}");
}

#[test]
fn a_run_without_a_loopback_has_no_calibration_and_writes_no_settings() {
    assert!(run(&config(64, 2.0, false, script(0, None, 144))).unwrap().calibration.is_none());
    // A run without a loopback has nothing to calibrate, so --settings refuses and writes nothing.
    let mut no_loopback = config(64, 2.0, false, script(0, None, 144));
    no_loopback.settings = Some(std::env::temp_dir().join("audio-bench-never-written.json"));
    assert!(run(&no_loopback).unwrap_err().contains("--settings needs"));
    assert!(!no_loopback.settings.unwrap().exists());
}

#[test]
fn settings_receive_the_measured_calibration_and_keep_what_was_there() {
    let dir = std::env::temp_dir().join(format!("audio-bench-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ui.json");
    std::fs::write(&path, r#"{"keep":{"me":1},"audio":{"output":"Fake device"}}"#).unwrap();
    let mut c = config(64, 3.0, true, script(0, None, 240));
    c.settings = Some(path.clone());
    c.request.in_device = Some("Fake input".into());
    for _ in 0..2 {
        run(&c).expect("the fake run");
    }
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["keep"]["me"], 1);
    let audio: soundcraft_playback::AudioConfig = serde_json::from_value(doc["audio"].clone()).unwrap();
    assert_eq!(audio.output.as_deref(), Some("Fake device"));
    let [cal] = audio.calibrations.as_slice() else { panic!("one entry after two runs: {:?}", audio.calibrations) };
    assert_eq!((cal.device.as_str(), cal.input.as_deref(), cal.mode.as_str()), ("Fake device", Some("Fake input"), "shared"));
    assert_eq!((cal.rate, cal.period, cal.round_trip_frames, cal.spread_frames), (48_000, 64, 240, 1));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_exclusive_run_notes_its_mode_and_calibrates_under_the_exclusive_key() {
    let dir = std::env::temp_dir().join(format!("audio-bench-e2e-exclusive-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ui.json");
    let mut c = config(128, 3.0, true, script(0, None, 240));
    c.request.mode = transit_audio_backend::Mode::Exclusive;
    c.settings = Some(path.clone());
    let row = run(&c).expect("the fake accepts exclusive as if shared");
    assert_eq!(row.mode.as_deref(), Some("exclusive, f32, 128 fr"));
    assert!(row.markdown().ends_with("| exclusive, f32, 128 fr |"), "{}", row.markdown());
    // A shared run of the same pair is another key: both entries stay.
    c.request.mode = transit_audio_backend::Mode::Shared;
    assert_eq!(run(&c).expect("the shared run").mode, None);
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let audio: soundcraft_playback::AudioConfig = serde_json::from_value(doc["audio"].clone()).unwrap();
    let modes: Vec<&str> = audio.calibrations.iter().map(|c| c.mode.as_str()).collect();
    assert_eq!(modes, [soundcraft_playback::devices::MODE_EXCLUSIVE, soundcraft_playback::devices::MODE_SHARED]);
    std::fs::remove_dir_all(&dir).unwrap();
}
