//! Chord spelling for the MIDI command channel (control thread only).

use super::*;

pub(super) fn pitches(p: &Value) -> Result<Vec<u8>> {
    let cmd = "midi.chord_add";
    let root = p.get("root").ok_or_else(|| bad(cmd, "root is required (MIDI number or note name, e.g. G3)"))?;
    let root = match (root.as_i64(), root.as_str()) {
        (Some(n), _) if (0..=127).contains(&n) => n,
        (_, Some(name)) => {
            note_number(name).ok_or_else(|| bad(cmd, "invalid root; use MIDI 0..127 or a note name (C3 = 60, bare names use octave 3)"))?
        }
        _ => return Err(bad(cmd, "root must be a MIDI number 0..127 or note name")),
    };
    let mut intervals: Vec<i64> = match (p.get("quality"), p.get("notes")) {
        (Some(quality), None) => match quality.as_str() {
            Some("maj") => vec![0, 4, 7],
            Some("min") => vec![0, 3, 7],
            Some("dim") => vec![0, 3, 6],
            Some("aug") => vec![0, 4, 8],
            Some("maj7") => vec![0, 4, 7, 11],
            Some("min7") => vec![0, 3, 7, 10],
            Some("dom7") => vec![0, 4, 7, 10],
            Some("sus2") => vec![0, 2, 7],
            Some("sus4") => vec![0, 5, 7],
            _ => return Err(bad(cmd, "quality must be maj|min|dim|aug|maj7|min7|dom7|sus2|sus4")),
        },
        (None, Some(notes)) => {
            let notes =
                notes.as_array().filter(|a| !a.is_empty() && a.len() <= 32).ok_or_else(|| bad(cmd, "notes must contain 1..32 semitone intervals"))?;
            notes
                .iter()
                .map(|v| v.as_i64().filter(|n| (-127..=127).contains(n)).ok_or_else(|| bad(cmd, "intervals must be integers in -127..127")))
                .collect::<Result<_>>()?
        }
        _ => return Err(bad(cmd, "provide exactly one of quality or notes (semitone intervals from root)")),
    };
    intervals.sort_unstable();
    intervals.dedup();
    let inversion = match p.get("inversion") {
        None => 0,
        Some(v) => v
            .as_u64()
            .filter(|n| *n < intervals.len() as u64)
            .ok_or_else(|| bad(cmd, "inversion must be 0 through the number of distinct notes minus 1"))? as usize,
    };
    // Raise the lowest N chord tones by one octave, then sort the resulting voicing.
    for interval in intervals.iter_mut().take(inversion) {
        *interval += 12;
    }
    let mut pitches = intervals
        .into_iter()
        .map(|interval| {
            u8::try_from(root + interval).ok().filter(|n| *n <= 127).ok_or_else(|| bad(cmd, "chord voicing falls outside MIDI pitches 0..127"))
        })
        .collect::<Result<Vec<_>>>()?;
    pitches.sort_unstable();
    pitches.dedup();
    Ok(pitches)
}

fn note_number(name: &str) -> Option<i64> {
    let mut chars = name.chars();
    let pitch = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest = chars.as_str();
    let (accidental, octave) = if let Some(s) = rest.strip_prefix('#') {
        (1, s)
    } else if let Some(s) = rest.strip_prefix('b') {
        (-1, s)
    } else {
        (0, rest)
    };
    // Match transit's existing MIDI display convention: middle C is C3.
    let octave = if octave.is_empty() { 3 } else { octave.parse::<i64>().ok()? };
    let number = octave.checked_add(2)?.checked_mul(12)?.checked_add(pitch + accidental)?;
    (0..=127).contains(&number).then_some(number)
}
