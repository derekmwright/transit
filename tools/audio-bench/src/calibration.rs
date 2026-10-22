//! Loopback calibration (ADR 0016 slice d): from the round trip the harness measured in the
//! stamps' own time to the record `audio.calibrations` of `ui.json` keeps, and the merge into that
//! file. The Player looks the record up by device, input, mode, rate and period and hands it to
//! both streams, which then report `Measured` stamps.

use crate::analysis::FrameTrip;
use crate::probe::{CLICK_LEVEL, MIN_ENERGY_RATIO};
use soundcraft_playback::LoopbackCalibration;
use soundcraft_playback::devices::MODE_SHARED;
use std::path::Path;

/// Fewest clicks that must pair for a calibration to be trusted.
pub const MIN_PAIRED: usize = 3;
/// A settings file larger than this is not rewritten (the app's own limit).
const MAX_SETTINGS_BYTES: usize = 4 * 1024 * 1024;

/// A calibration as measured, before it is keyed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Figures {
    pub rate: u32,
    /// Median round trip, rounded to a whole frame.
    pub round_trip_frames: u32,
    /// `(p95 - min)` of the per-click round trips plus one frame of position granularity, rounded up.
    pub spread_frames: u32,
    pub paired: usize,
    pub clicks: usize,
}

impl Figures {
    fn ms(&self, frames: u32) -> f64 {
        f64::from(frames) * 1000.0 / f64::from(self.rate.max(1))
    }

    /// The line the harness prints: frames and milliseconds, with the uncertainty.
    pub fn describe(&self) -> String {
        format!(
            "calibration: round trip {} frames ({:.3} ms) +-{} frames ({:.3} ms), {} of {} clicks paired",
            self.round_trip_frames,
            self.ms(self.round_trip_frames),
            self.spread_frames,
            self.ms(self.spread_frames),
            self.paired,
            self.clicks
        )
    }
}

/// A linear peak as dBFS with one decimal; `-inf dBFS` for silence.
pub fn dbfs(peak: f32) -> String {
    if peak > 0.0 && peak.is_finite() { format!("{:.1} dBFS", 20.0 * peak.log10()) } else { "-inf dBFS".to_string() }
}

/// "input peak on ch2: -6.0 dBFS" for the 1-based input channel `in_ch`.
pub fn input_peak_line(peaks: &[f32], in_ch: u16) -> String {
    let peak = peaks.get(usize::from(in_ch).saturating_sub(1)).copied().unwrap_or(0.0);
    format!("input peak on ch{in_ch}: {}", dbfs(peak))
}

/// "input peaks: ch1 -inf dBFS, ch2 -42.1 dBFS; clicks play at -6 dBFS and a detection needs about
/// -20 dBFS" (the figures follow the click level and the detector's energy threshold).
fn peaks_note(peaks: &[f32]) -> String {
    let list = peaks.iter().enumerate().map(|(i, p)| format!("ch{} {}", i + 1, dbfs(*p))).collect::<Vec<_>>().join(", ");
    let play = 20.0 * CLICK_LEVEL.log10();
    let need = play + 10.0 * MIN_ENERGY_RATIO.log10();
    let list = if list.is_empty() { "no input channels".to_string() } else { list };
    format!("input peaks: {list}; clicks play at {play:.0} dBFS and a detection needs about {need:.0} dBFS")
}

/// The calibration of `trip` at `rate` Hz: the median of the per-click round trips, and the spread
/// `p95 - min` plus one frame (positions are whole frames) as its uncertainty.
pub fn figures(trip: &FrameTrip, rate: u32, peaks: &[f32]) -> Result<Figures, String> {
    if trip.paired < MIN_PAIRED || trip.paired * 2 < trip.clicks {
        return Err(format!(
            "only {} of {} clicks were detected on the loopback ({}); check the cable, the channels and the levels (need at least {MIN_PAIRED} and half)",
            trip.paired,
            trip.clicks,
            peaks_note(peaks)
        ));
    }
    let frames = |ns: f64| ns * f64::from(rate) / 1e9;
    let median = frames(trip.median_ns);
    if !(median.is_finite() && (0.0..f64::from(u32::MAX)).contains(&median)) {
        return Err(format!("the median round trip ({median:.1} frames) is not a delay"));
    }
    // The epsilon keeps a spread of exactly zero at one frame rather than two.
    let spread = (frames(trip.p95_ns - trip.min_ns) + 1.0 - 1e-3).ceil().max(1.0);
    Ok(Figures { rate, round_trip_frames: median.round() as u32, spread_frames: spread as u32, paired: trip.paired, clicks: trip.clicks })
}

/// The record for `ui.json`: keyed by the output `device`, the paired `input` as named on the
/// command line (the app's `input_pairing`), the requested `period` (0 for the driver's default)
/// and the rate measured.
pub fn record(figures: &Figures, device: &str, input: Option<&str>, period: u32, measured_at: &str) -> LoopbackCalibration {
    LoopbackCalibration {
        device: device.to_owned(),
        input: input.map(str::to_owned),
        mode: MODE_SHARED.to_owned(),
        rate: figures.rate,
        period,
        round_trip_frames: figures.round_trip_frames,
        spread_frames: figures.spread_frames,
        measured_at: measured_at.to_owned(),
    }
}

/// Merge `calibration` into `audio.calibrations` of the settings file at `path` (created when
/// missing): an entry with the same key is replaced, any other entry and every other key of the file
/// stay. The app must be closed: it rewrites its `audio` block from memory when it saves.
pub fn merge_settings(path: &Path, calibration: &LoopbackCalibration) -> Result<(), String> {
    let mut value = match std::fs::read(path) {
        Ok(bytes) if bytes.len() > MAX_SETTINGS_BYTES => return Err(format!("{} exceeds the 4 MiB settings limit", path.display())),
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    merged(&mut value, calibration)?;
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("calibration.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// `calibration` merged into the settings document `value`.
fn merged(value: &mut serde_json::Value, calibration: &LoopbackCalibration) -> Result<(), String> {
    let root = value.as_object_mut().ok_or("the settings file is not a JSON object")?;
    let audio = root.entry("audio").or_insert_with(|| serde_json::json!({}));
    let audio = audio.as_object_mut().ok_or("`audio` in the settings file is not an object")?;
    let list = audio.entry("calibrations").or_insert_with(|| serde_json::json!([]));
    let list = list.as_array_mut().ok_or("`audio.calibrations` in the settings file is not a list")?;
    let new = serde_json::to_value(calibration).map_err(|e| e.to_string())?;
    // Entries this build cannot read are kept as they are.
    let same = |entry: &serde_json::Value| serde_json::from_value::<LoopbackCalibration>(entry.clone()).is_ok_and(|e| e.same_key(calibration));
    match list.iter().position(same) {
        Some(at) => list[at] = new,
        None => list.push(new),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trip(min: f64, median: f64, p95: f64) -> FrameTrip {
        FrameTrip { clicks: 8, paired: 8, min_ns: min, median_ns: median, p95_ns: p95 }
    }

    #[test]
    fn the_median_is_the_round_trip_and_the_spread_plus_a_frame_is_the_bound() {
        // 48 kHz: a frame is 20 833.3 ns.
        let f = figures(&trip(3_000_000.0, 3_000_000.0, 3_000_000.0), 48_000, &[]).unwrap();
        assert_eq!((f.round_trip_frames, f.spread_frames), (144, 1), "no spread: one frame of granularity");
        let f = figures(&trip(2_950_000.0, 3_000_000.0, 3_100_000.0), 48_000, &[]).unwrap();
        assert_eq!((f.round_trip_frames, f.spread_frames), (144, 9), "150 us is 7.2 frames; plus one, rounded up");
        assert!(f.describe().contains("144 frames (3.000 ms) +-9 frames (0.188 ms), 8 of 8 clicks paired"), "{}", f.describe());
    }

    #[test]
    fn levels_print_as_dbfs_with_one_decimal() {
        assert_eq!(dbfs(0.5), "-6.0 dBFS");
        assert_eq!(dbfs(0.0), "-inf dBFS");
        assert_eq!(dbfs(1.0), "0.0 dBFS");
        assert_eq!(input_peak_line(&[0.0, 0.5], 2), "input peak on ch2: -6.0 dBFS");
    }

    #[test]
    fn the_failure_names_the_input_peaks() {
        let mut t = trip(1.0, 1.0, 1.0);
        t.paired = 0;
        t.clicks = 40;
        let why = figures(&t, 48_000, &[0.0, 0.00785]).unwrap_err();
        assert!(
            why.contains("only 0 of 40 clicks were detected on the loopback (input peaks: ch1 -inf dBFS, ch2 -42.1 dBFS; clicks play at -6 dBFS and a detection needs about -20 dBFS); check the cable"),
            "{why}"
        );
    }

    #[test]
    fn a_loopback_that_lost_clicks_is_not_a_calibration() {
        let mut t = trip(1.0, 1.0, 1.0);
        t.paired = 2;
        assert!(figures(&t, 48_000, &[]).unwrap_err().contains("2 of 8"));
        t.paired = 3;
        assert!(figures(&t, 48_000, &[]).is_err(), "fewer than half");
    }

    #[test]
    fn merging_replaces_the_same_key_and_keeps_everything_else() {
        let f = Figures { rate: 48_000, round_trip_frames: 480, spread_frames: 3, paired: 8, clicks: 8 };
        let a = record(&f, "Speakers", Some("Line"), 128, "2026-10-21");
        let other = LoopbackCalibration { period: 64, ..a.clone() };
        let mut doc = serde_json::json!({
            "theme": "dark",
            "audio": {"output": "Speakers", "calibrations": [other, {"future": true}]},
        });
        merged(&mut doc, &a).unwrap();
        let newer = LoopbackCalibration { round_trip_frames: 500, ..a.clone() };
        merged(&mut doc, &newer).unwrap();
        assert_eq!(doc["theme"], "dark");
        assert_eq!(doc["audio"]["output"], "Speakers");
        let list = doc["audio"]["calibrations"].as_array().unwrap();
        assert_eq!(list.len(), 3, "the other period and the unknown entry stay; the same key was replaced");
        assert_eq!(serde_json::from_value::<LoopbackCalibration>(list[0].clone()).unwrap(), other);
        assert_eq!(list[1], serde_json::json!({"future": true}));
        assert_eq!(serde_json::from_value::<LoopbackCalibration>(list[2].clone()).unwrap(), newer);
        assert!(merged(&mut serde_json::json!([]), &a).is_err() && merged(&mut serde_json::json!({"audio": 1}), &a).is_err());
    }

    #[test]
    fn the_file_round_trips_into_the_audio_config_and_unrelated_keys_survive() {
        let dir = std::env::temp_dir().join(format!("audio-bench-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        std::fs::write(&path, r#"{"theme":"dark","audio":{"output":"Speakers","buffer_size":128},"zoom":[1,2]}"#).unwrap();
        let f = Figures { rate: 48_000, round_trip_frames: 480, spread_frames: 3, paired: 8, clicks: 8 };
        let cal = record(&f, "Speakers", None, 128, "2026-10-21");
        merge_settings(&path, &cal).unwrap();
        merge_settings(&path, &cal).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((doc["theme"].as_str(), doc["zoom"].clone()), (Some("dark"), serde_json::json!([1, 2])));
        let audio: soundcraft_playback::AudioConfig = serde_json::from_value(doc["audio"].clone()).unwrap();
        assert_eq!((audio.output.as_deref(), audio.buffer_size), (Some("Speakers"), Some(128)));
        assert_eq!(audio.calibrations, std::slice::from_ref(&cal), "merging twice leaves one entry");
        let fresh = dir.join("new.json");
        merge_settings(&fresh, &cal).unwrap();
        assert!(std::fs::read_to_string(&fresh).unwrap().contains("round_trip_frames"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
