//! The harness end to end on `FakeBackend`: scripted cadence, jitter, one gap, a software
//! loopback with a known delay. No device is opened.
#![forbid(unsafe_code)]

use audio_bench::{Config, Kind, analysis::Measurements, fake::Script, run, session::Request};

fn config(period: u32, seconds: f64, loopback: bool, script: Script) -> Config {
    Config {
        kind: Kind::Fake,
        system: None,
        request: Request { device: None, in_device: None, rate: 48_000, period: Some(period), loopback, out_ch: 1, in_ch: 1 },
        seconds,
        fake: script,
        out: None,
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
