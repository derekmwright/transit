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
    /// The loopback calibration of a native run (`--loopback` on `--backend wasapi`), when it was measurable.
    pub calibration: Option<Result<crate::calibration::Figures, String>>,
    /// The device-loop counters of the render and capture streams, when the backend keeps them.
    pub render_stats: Option<ab::StreamStats>,
    pub capture_stats: Option<ab::StreamStats>,
    pub machine: Machine,
    pub date: String,
}

fn cell(text: &str) -> String {
    text.replace('|', "/").replace(['\r', '\n'], " ")
}

impl Row {
    /// One line per stream that kept device-loop counters (`--backend wasapi` and the fake one).
    pub fn stats_lines(&self) -> Vec<String> {
        let line = |name: &str, stats: &Option<ab::StreamStats>, capture: bool| stats.map(|s| format!("{name} backend: {}", s.line(capture)));
        [line("render", &self.render_stats, false), line("capture", &self.capture_stats, true)].into_iter().flatten().collect()
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

/// "exclusive, i32/24 valid, 128 fr": the mode, the device sample format and the period an
/// exclusive stream achieved.
pub fn exclusive_note(achieved: &ab::Achieved) -> String {
    let f = achieved.format;
    let sample = format!("{:?}", f.sample).to_lowercase();
    let sample = match f.valid_bits {
        Some(bits) => format!("{sample}/{bits} valid"),
        None => sample,
    };
    let period = achieved.period.frames().map_or_else(|| "default period".to_string(), |n| format!("{n} fr"));
    format!("exclusive, {sample}, {period}")
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
                backend_errors: 0,
                invalid_stamps: 0,
                dropped: 0,
                input_peaks: Vec::new(),
            },
            calibration: None,
            render_stats: None,
            capture_stats: None,
            machine: Machine { cpu: "Test CPU | 8 cores".into(), os: "Test OS 1.0".into() },
            date: "2026-10-21".into(),
        }
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
