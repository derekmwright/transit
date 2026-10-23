//! The Markdown row set, the machine description and the date.

use crate::analysis::Measurements;
use std::path::Path;
use std::process::Command;
use transit_audio_backend as ab;

pub const HEADER: &str = "| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |";
const RULE: &str = "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub cpu: String,
    pub os: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub backend: String,
    pub system: String,
    pub device: String,
    pub requested: Option<u32>,
    /// The mode note of an exclusive run ([`exclusive_note`]), first in the Notes column; `None`
    /// for shared mode.
    pub mode: Option<String>,
    pub measurements: Measurements,
    /// The loopback calibration of a native run (`--loopback` on `--backend wasapi` or `asio`), when
    /// it was measurable: the figure the app would store ([`crate::app_trip`]).
    pub calibration: Option<Result<crate::calibration::Figures, String>>,
    /// With `--settings`: what the file held under this run's key before the run merged its own
    /// (`Some(None)`: nothing).
    pub stored_calibration: Option<Option<soundcraft_playback::LoopbackCalibration>>,
    /// The device-loop counters of the render and capture streams, when the backend keeps them.
    pub render_stats: Option<ab::StreamStats>,
    pub capture_stats: Option<ab::StreamStats>,
    /// What each stream opened with, for the buffer and latency the backend lines report.
    pub render_achieved: Option<ab::Achieved>,
    pub capture_achieved: Option<ab::Achieved>,
    pub machine: Machine,
    pub date: String,
}

fn cell(text: &str) -> String {
    text.replace('|', "/").replace(['\r', '\n'], " ")
}

impl Row {
    /// One line per stream that kept device-loop counters (`--backend wasapi`, `asio` and the fake
    /// one). A native ASIO line always ends with the driver-event counters, zero or not.
    pub fn stats_lines(&self) -> Vec<String> {
        let asio = self.backend == "asio";
        let line = |name: &str, stats: &Option<ab::StreamStats>, achieved: &Option<ab::Achieved>, capture: bool| {
            stats.map(|s| {
                let mut text = s.line(capture);
                if asio && !text.contains("overloads") {
                    text.push_str(&s.events_tail());
                }
                format!("{name} backend: {text}{}", opened_figures(achieved.as_ref()))
            })
        };
        [line("render", &self.render_stats, &self.render_achieved, false), line("capture", &self.capture_stats, &self.capture_achieved, true)]
            .into_iter()
            .flatten()
            .collect()
    }

    /// The calibration lines a native ASIO loopback run prints (#20, known issue K31), so the
    /// driver's figures and the measured loop can be compared and tabulated: the driver-reported
    /// latencies in frames and ms, the loop the app would store at this period and rate, the
    /// difference, and what the settings file held for the key before this run. Empty for the
    /// other backends.
    pub fn asio_calibration_lines(&self) -> Vec<String> {
        if self.backend != "asio" {
            return Vec::new();
        }
        let rate = self.measurements.rate;
        let frames = |d: std::time::Duration| (d.as_secs_f64() * f64::from(rate)).round() as u64;
        let ms = |n: u64| n as f64 * 1000.0 / f64::from(rate.max(1));
        let mut lines = Vec::new();
        let (out, inp) = (self.render_achieved.and_then(|a| a.latency).map(frames), self.capture_achieved.and_then(|a| a.latency).map(frames));
        // The buffer the driver ran (the key of the app's calibration), else the one asked for.
        let period =
            self.render_achieved.and_then(|a| a.period.frames()).or(self.requested).map_or_else(|| "driver buffer".to_string(), |p| p.to_string());
        let at = format!("{period} / {rate} Hz");
        match (out, inp) {
            (Some(out), Some(inp)) => lines.push(format!(
                "driver-reported latency: out {out} fr ({:.2} ms) + in {inp} fr ({:.2} ms) = {} fr ({:.2} ms)",
                ms(out),
                ms(inp),
                out + inp,
                ms(out + inp)
            )),
            (Some(out), None) => lines.push(format!("driver-reported latency: out {out} fr ({:.2} ms); no input", ms(out))),
            _ => {}
        }
        match &self.calibration {
            Some(Ok(f)) => {
                lines.push(format!(
                    "app calibration (the click loop by sample position at {at}): {} fr ({:.2} ms) +-{} fr, {} of {} clicks",
                    f.round_trip_frames,
                    ms(u64::from(f.round_trip_frames)),
                    f.spread_frames,
                    f.paired,
                    f.clicks
                ));
                if let (Some(out), Some(inp)) = (out, inp) {
                    let difference = i64::try_from(u64::from(f.round_trip_frames)).unwrap_or(i64::MAX) - i64::try_from(out + inp).unwrap_or(i64::MAX);
                    lines.push(format!("measured minus driver-reported: {difference:+} fr"));
                }
            }
            Some(Err(why)) => lines.push(format!("app calibration at {at}: none ({why}); the app would use the driver's figures")),
            None => lines.push(format!(
                "app calibration at {at}: none (no loopback, or the driver gave no sample positions); the app would use the driver's figures"
            )),
        }
        match &self.stored_calibration {
            Some(Some(c)) => lines.push(format!(
                "stored before this run: {} fr +-{} fr ({}); replaced by this run's",
                c.round_trip_frames, c.spread_frames, c.measured_at
            )),
            Some(None) => lines.push("stored before this run: none for this key".to_string()),
            None => {}
        }
        lines
    }

    /// The per-channel input peaks, when a loopback run detected no click at all: printed whether or
    /// not a stamp-time calibration exists, so a silent or too-quiet input is never invisible.
    pub fn failed_detection_peaks(&self) -> Option<String> {
        let none = self.measurements.round_trip.is_some_and(|t| t.paired == 0);
        none.then(|| crate::calibration::input_peaks_line(&self.measurements.input_peaks))
    }

    pub fn markdown(&self) -> String {
        let m = &self.measurements;
        let requested = self.requested.map_or_else(|| "default".to_string(), |p| p.to_string());
        let varied = if m.min_frames == m.max_frames { String::new() } else { format!(" (blocks {} to {})", m.min_frames, m.max_frames) };
        let achieved = format!("{} fr / {:.3} ms{varied}", m.achieved_frames, m.achieved_ms);
        let jitter = format!("{:.3} / {:.3} / {:.3}", m.jitter_ms.p50, m.jitter_ms.p95, m.jitter_ms.max);
        let xruns = format!("{}/{}", m.xruns_out, m.xruns_in.map_or_else(|| "n/a".to_string(), |n| n.to_string()));
        let cpu = format!("{:.1} / {:.1}", m.cpu_us.p50, m.cpu_us.p95);
        let (trip_ms, trip_frames) = match m.round_trip {
            None => ("n/a".to_string(), "n/a".to_string()),
            Some(t) if t.paired == 0 => (format!("no click detected (0 of {})", t.clicks), "n/a".to_string()),
            Some(t) => (
                format!("{:.3} / {:.3} / {:.3} ({} of {})", t.min_ms, t.median_ms, t.p95_ms, t.paired, t.clicks),
                format!("{:.0} / {:.0} / {:.0}", t.min_frames, t.median_frames, t.p95_frames),
            ),
        };
        let mut notes: Vec<String> = self.mode.iter().cloned().collect();
        if m.backend_errors > 0 {
            notes.push(format!("backend errors {}", m.backend_errors));
        }
        if m.invalid_stamps > 0 {
            notes.push(format!("invalid stamps {}", m.invalid_stamps));
        }
        if m.dropped > 0 {
            notes.push(format!("dropped log entries {}", m.dropped));
        }
        let notes = if notes.is_empty() { "-".to_string() } else { notes.join("; ") };
        let cells = [
            self.backend.clone(),
            self.system.clone(),
            self.device.clone(),
            requested,
            achieved,
            m.rate.to_string(),
            jitter,
            xruns,
            format!("{:.2}", m.xruns_per_minute()),
            cpu,
            trip_ms,
            trip_frames,
            self.machine.cpu.clone(),
            self.machine.os.clone(),
            self.date.clone(),
            notes,
        ];
        format!("| {} |", cells.iter().map(|c| cell(c)).collect::<Vec<_>>().join(" | "))
    }
}

/// "; buffer 441 fr; latency 12.5 ms": what the driver allocated and reported at open, each only
/// where the backend says.
fn opened_figures(achieved: Option<&ab::Achieved>) -> String {
    let Some(a) = achieved else { return String::new() };
    let buffer = a.buffer_frames.map(|n| format!("; buffer {n} fr"));
    let latency = a.latency.map(|d| {
        let frames = (d.as_secs_f64() * f64::from(a.format.rate)).round();
        format!("; latency {:.1} ms ({frames:.0} fr)", d.as_secs_f64() * 1000.0)
    });
    let sample = format!("{:?}", a.format.sample).to_lowercase();
    let sample = a.format.valid_bits.map_or(sample.clone(), |bits| format!("{sample}/{bits}"));
    let format = Some(format!("; fmt {sample} {} Hz", a.format.rate));
    buffer.into_iter().chain(latency).chain(format).collect()
}

/// "exclusive, i32/24 valid, 128 fr, buffer 128 fr": the mode, the device sample format, the period
/// an exclusive stream achieved and the buffer the driver allocated.
pub fn exclusive_note(achieved: &ab::Achieved) -> String {
    let f = achieved.format;
    let sample = format!("{:?}", f.sample).to_lowercase();
    let sample = match f.valid_bits {
        Some(bits) => format!("{sample}/{bits} valid"),
        None => sample,
    };
    let period = achieved.period.frames().map_or_else(|| "default period".to_string(), |n| format!("{n} fr"));
    let buffer = achieved.buffer_frames.map(|n| format!(", buffer {n} fr")).unwrap_or_default();
    format!("exclusive, {sample}, {period}{buffer}")
}

/// The table for `rows`.
pub fn table(rows: &[Row]) -> String {
    let mut text = format!("{HEADER}\n{RULE}\n");
    for row in rows {
        text.push_str(&row.markdown());
        text.push('\n');
    }
    text
}

/// Write the table to `path`. An existing file that already has this table gets the rows
/// appended, so one file can collect a session's runs.
pub fn write(path: &Path, rows: &[Row]) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(existing) if existing.lines().any(|line| line == HEADER) => {
            let mut text = existing;
            if !text.ends_with('\n') {
                text.push('\n');
            }
            for row in rows {
                text.push_str(&row.markdown());
                text.push('\n');
            }
            text
        }
        _ => table(rows),
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// CPU model and OS build, best effort ("unknown" when the system will not say).
pub fn machine() -> Machine {
    let unknown = || "unknown".to_string();
    #[cfg(windows)]
    {
        let cpu = run("reg", &["query", r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0", "/v", "ProcessorNameString"])
            .and_then(|text| text.lines().find_map(|l| l.split("REG_SZ").nth(1).map(|v| v.trim().to_string())))
            .or_else(|| std::env::var("PROCESSOR_IDENTIFIER").ok());
        let os = run("cmd", &["/C", "ver"]).map(|v| v.trim_matches(|c: char| c.is_whitespace()).to_string());
        Machine { cpu: cpu.unwrap_or_else(unknown), os: os.unwrap_or_else(unknown) }
    }
    #[cfg(target_os = "macos")]
    {
        let cpu = run("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let os = run("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}"));
        Machine { cpu: cpu.unwrap_or_else(unknown), os: os.unwrap_or_else(unknown) }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let cpu = std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|text| text.lines().find_map(|l| l.strip_prefix("model name").and_then(|v| v.split(':').nth(1)).map(|v| v.trim().to_string())));
        let release = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|text| text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())));
        let kernel = run("uname", &["-r"]);
        let os = match (release, kernel) {
            (Some(r), Some(k)) => Some(format!("{r}, kernel {k}")),
            (r, k) => r.or(k),
        };
        Machine { cpu: cpu.unwrap_or_else(unknown), os: os.unwrap_or_else(unknown) }
    }
}

/// Today's date in UTC as `YYYY-MM-DD`.
pub fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    civil_date(i64::try_from(secs / 86_400).unwrap_or(0))
}

/// The proleptic Gregorian date of a day count since 1970-01-01.
pub fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Percentiles, RoundTrip};

    fn row(loopback: bool) -> Row {
        let round_trip = loopback.then_some(RoundTrip {
            clicks: 8,
            paired: 8,
            min_ms: 3.0,
            median_ms: 3.0,
            p95_ms: 3.125,
            min_frames: 144.0,
            median_frames: 144.0,
            p95_frames: 150.0,
        });
        Row {
            backend: "fake".into(),
            system: "Fake".into(),
            device: "Fake device".into(),
            requested: Some(64),
            mode: None,
            measurements: Measurements {
                rate: 48_000,
                callbacks: 3000,
                achieved_frames: 64,
                min_frames: 64,
                max_frames: 64,
                achieved_ms: 64.0 * 1000.0 / 48_000.0,
                jitter_ms: Percentiles { p50: 0.05, p95: 0.19, max: 1.4 },
                duration_s: 4.0,
                xruns_out: 1,
                xruns_in: loopback.then_some(1),
                cpu_us: Percentiles { p50: 1.5, p95: 2.4, max: 9.0 },
                round_trip,
                frame_trip: None,
                position_trip: None,
                backend_errors: 0,
                invalid_stamps: 0,
                dropped: 0,
                input_peaks: Vec::new(),
            },
            calibration: None,
            stored_calibration: None,
            render_stats: None,
            capture_stats: None,
            render_achieved: None,
            capture_achieved: None,
            machine: Machine { cpu: "Test CPU | 8 cores".into(), os: "Test OS 1.0".into() },
            date: "2026-10-21".into(),
        }
    }

    #[test]
    fn the_backend_lines_carry_the_buffer_and_latency_where_known() {
        let mut r = row(true);
        r.render_stats = Some(ab::StreamStats::default());
        let mut a = ab::Achieved::requested(ab::Format::new(48_000, 2, ab::SampleFormat::F32), ab::Period::Frames(64));
        a.buffer_frames = Some(128);
        a.latency = Some(std::time::Duration::from_micros(12_500));
        r.render_achieved = Some(a);
        let line = &r.stats_lines()[0];
        assert!(
            line.starts_with("render backend: wakes 0") && line.ends_with("; buffer 128 fr; latency 12.5 ms (600 fr); fmt f32 48000 Hz"),
            "{line}"
        );
    }

    #[test]
    fn an_asio_run_prints_the_driver_figures_in_frames_beside_the_loop_the_app_would_store() {
        let mut r = row(true);
        assert!(r.asio_calibration_lines().is_empty(), "only native ASIO runs");
        r.backend = "asio".into();
        r.requested = Some(128);
        let opened = |latency_us: u64| {
            let mut a = ab::Achieved::requested(ab::Format::new(48_000, 2, ab::SampleFormat::I32), ab::Period::Frames(256));
            a.latency = Some(std::time::Duration::from_micros(latency_us));
            a
        };
        // The Scarlett at 256 frames (asked 128, the driver ran 256): 12.9 + 11.9 ms reported, 1196
        // frames by sample position.
        (r.render_achieved, r.capture_achieved) = (Some(opened(12_896)), Some(opened(11_896)));
        r.calibration = Some(Ok(crate::calibration::Figures { rate: 48_000, round_trip_frames: 1_196, spread_frames: 7, paired: 16, clicks: 16 }));
        r.stored_calibration = Some(None);
        assert_eq!(
            r.asio_calibration_lines(),
            [
                "driver-reported latency: out 619 fr (12.90 ms) + in 571 fr (11.90 ms) = 1190 fr (24.79 ms)",
                "app calibration (the click loop by sample position at 256 / 48000 Hz): 1196 fr (24.92 ms) +-7 fr, 16 of 16 clicks",
                "measured minus driver-reported: +6 fr",
                "stored before this run: none for this key",
            ]
        );
        r.calibration = None;
        assert!(r.asio_calibration_lines()[1].ends_with("the app would use the driver's figures"), "{:?}", r.asio_calibration_lines());
    }

    #[test]
    fn a_stream_with_counters_gets_one_backend_line_after_the_table() {
        let mut with = row(true);
        assert!(with.stats_lines().is_empty());
        with.render_stats = Some(ab::StreamStats { wakes: 10, skipped_wakes: 1, min_padding_frames: 64, ..Default::default() });
        with.capture_stats = Some(ab::StreamStats { wakes: 9, max_packets_per_wake: 2, ..Default::default() });
        assert_eq!(
            with.stats_lines(),
            [
                "render backend: wakes 10, late p95 0 µs, max 0 µs; service p95 0 µs, max 0 µs; skipped 1, timeouts 0; min padding 64",
                "capture backend: wakes 9, late p95 0 µs, max 0 µs; service p95 0 µs, max 0 µs; skipped 0, timeouts 0; max packets 2",
            ]
        );
    }

    #[test]
    fn a_native_asio_line_always_ends_with_the_driver_events() {
        let mut r = row(true);
        r.backend = "asio".into();
        r.render_stats = Some(ab::StreamStats { wakes: 10, ..Default::default() });
        r.capture_stats = Some(ab::StreamStats { wakes: 10, overloads: 1, ..Default::default() });
        let lines = r.stats_lines();
        assert!(lines[0].ends_with("min padding 0; overloads 0, busy skips 0, position jumps 0, resets 0"), "{}", lines[0]);
        assert!(lines[1].ends_with("max packets 0; overloads 1, busy skips 0, position jumps 0, resets 0"), "{}", lines[1]);
    }

    #[test]
    fn the_table_snapshot() {
        let expected = format!(
            "{HEADER}\n{RULE}\n\
| fake | Fake | Fake device | 64 | 64 fr / 1.333 ms | 48000 | 0.050 / 0.190 / 1.400 | 1/1 | 15.00 | 1.5 / 2.4 | 3.000 / 3.000 / 3.125 (8 of 8) | 144 / 144 / 150 | Test CPU / 8 cores | Test OS 1.0 | 2026-10-21 | - |\n\
| fake | Fake | Fake device | 64 | 64 fr / 1.333 ms | 48000 | 0.050 / 0.190 / 1.400 | 1/n/a | 15.00 | 1.5 / 2.4 | n/a | n/a | Test CPU / 8 cores | Test OS 1.0 | 2026-10-21 | - |\n"
        );
        assert_eq!(table(&[row(true), row(false)]), expected);
        assert_eq!(HEADER.matches('|').count(), RULE.matches('|').count());
    }

    #[test]
    fn an_exclusive_run_names_its_format_and_period_first_in_the_notes() {
        let format = ab::Format { valid_bits: Some(24), ..ab::Format::new(48_000, 2, ab::SampleFormat::I32) };
        let achieved = ab::Achieved::requested(format, ab::Period::Frames(128));
        assert_eq!(exclusive_note(&achieved), "exclusive, i32/24 valid, 128 fr");
        let float = ab::Achieved::requested(ab::Format::new(48_000, 2, ab::SampleFormat::F32), ab::Period::Default);
        assert_eq!(exclusive_note(&float), "exclusive, f32, default period");
        let mut r = row(false);
        r.mode = Some(exclusive_note(&achieved));
        r.measurements.invalid_stamps = 2;
        assert!(r.markdown().ends_with("| exclusive, i32/24 valid, 128 fr; invalid stamps 2 |"), "{}", r.markdown());
    }

    #[test]
    fn rows_append_to_a_file_with_the_table() {
        let dir = std::env::temp_dir().join(format!("audio-bench-report-{}", std::process::id()));
        let path = dir.join("nested").join("out.md");
        write(&path, &[row(true)]).unwrap();
        write(&path, &[row(false)]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(HEADER).count(), 1);
        assert_eq!(text.lines().count(), 4);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_747), "2026-10-21");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }
}
