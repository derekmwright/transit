//! Checked musical addressing shared by MIDI commands.

use super::*;
use soundcraft_time::TempoMap;

/// A local bar grid begins at `origin`; a later meter change starts a new local bar.
/// Both components are 1-based, including durations: 1|2 is one beat, 2|1 is one bar.
pub(super) fn parse(map: &TempoMap, origin: i64, text: &str, cmd: &str) -> Result<i64> {
    let invalid = || bad(cmd, "expected bar|beat[|tick], with positive bar/beat and tick within the beat");
    let mut parts = text.split('|');
    let number = |part: Option<&str>| -> Result<i64> {
        let part = part.ok_or_else(invalid)?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        part.parse().map_err(|_| invalid())
    };
    let bar = number(parts.next())?;
    let beat = number(parts.next())?;
    let tick = parts.next().map(|p| number(Some(p))).transpose()?.unwrap_or(0);
    if parts.next().is_some() || bar < 1 || beat < 1 || origin < 0 {
        return Err(invalid());
    }
    let overflow = || bad(cmd, "musical time is out of range");
    let mut bars = bar - 1;
    let mut at = origin;
    let mut meter = map.meter_at_tick(at);
    for next in map.meters().iter().filter(|m| m.tick > origin) {
        let distance = next.tick.checked_sub(at).ok_or_else(overflow)?;
        let per_bar = meter.ticks_per_bar();
        let count = distance / per_bar + i64::from(distance % per_bar != 0);
        if bars < count {
            break;
        }
        bars -= count;
        at = next.tick;
        meter = *next;
    }
    if beat > i64::from(meter.numerator) || tick >= meter.ticks_per_beat() {
        return Err(invalid());
    }
    let offset = bars
        .checked_mul(meter.ticks_per_bar())
        .and_then(|v| v.checked_add((beat - 1) * meter.ticks_per_beat()))
        .and_then(|v| v.checked_add(tick))
        .ok_or_else(overflow)?;
    let end = at.checked_add(offset).ok_or_else(overflow)?;
    // A partial local bar ends at the next meter change; do not silently cross it.
    if map.meters().iter().any(|m| m.tick > at && m.tick <= end) {
        return Err(bad(cmd, "musical address crosses a meter change; use the next local bar"));
    }
    end.checked_sub(origin).ok_or_else(overflow)
}

pub(super) fn origin(e: &Engine, clip: ClipId, cmd: &str) -> Result<i64> {
    let s = e.session();
    let (_, c) = s.find_clip(clip).ok_or_else(|| bad(cmd, "no such clip"))?;
    Ok(s.tempo.samples_to_ticks(c.start, s.sample_rate).max(0))
}

pub(super) fn param(e: &Engine, p: &Value, key: &str, origin: i64, cmd: &str) -> Result<Option<i64>> {
    let ticks_key = format!("{key}_ticks");
    if let Some(value) = p.get(key) {
        if p.get(&ticks_key).is_some() {
            return Err(bad(cmd, format!("use either `{key}` or `{ticks_key}`, not both")));
        }
        let text =
            value.as_str().ok_or_else(|| bad(cmd, format!("`{key}` must be a bar|beat[|tick] string; use `{ticks_key}` for numeric ticks")))?;
        let ticks = parse(&e.session().tempo, origin, text, cmd)?;
        if key == "length" && ticks == 0 {
            return Err(bad(cmd, "length must be positive (1|2 is one beat; 2|1 is one bar)"));
        }
        return Ok(Some(ticks));
    }
    Ok(p.get(&ticks_key).and_then(Value::as_i64))
}

pub(super) fn timing(e: &Engine, p: &Value, clip: ClipId, default_length: i64, cmd: &str) -> Result<(i64, i64)> {
    let origin = origin(e, clip, cmd)?;
    let start = param(e, p, "start", origin, cmd)?.unwrap_or(0).max(0);
    let at = origin.checked_add(start).ok_or_else(|| bad(cmd, "note start is out of range"))?;
    let length = param(e, p, "length", at, cmd)?.unwrap_or(default_length).max(1);
    start.checked_add(length).ok_or_else(|| bad(cmd, "note end is out of range"))?;
    Ok((start, length))
}

/// Pipe-separated integers are musical addresses; clock strings stay with the position parser.
pub(super) fn is_musical(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s.contains('|') && s.bytes().all(|b| b.is_ascii_digit() || b == b'|' || b == b'-' || b == b'+'))
}

pub(super) fn clip_range(e: &Engine, p: &Value, cmd: &str) -> Result<Range> {
    let s = e.session();
    let mut params = p.clone();
    for key in ["start", "end"] {
        if let Some(value) = p.get(key).filter(|v| is_musical(v)) {
            let tick = parse(&s.tempo, 0, value.as_str().unwrap_or_default(), cmd)?;
            let samples = s.tempo.tick_to_samples(tick, s.sample_rate);
            if samples > MAX_POSITION {
                return Err(bad(cmd, "clip position is out of range"));
            }
            params[key] = Value::from(samples);
        }
    }
    if let Some(value) = p.get("length").filter(|v| is_musical(v)) {
        if p.get("end").is_some() {
            return Err(bad(cmd, "use either end or length, not both"));
        }
        let start = position_param(e, cmd, &params, "start")?.unwrap_or(s.edit.selection.start).max(0);
        let origin = s.tempo.samples_to_ticks(start, s.sample_rate);
        let ticks = parse(&s.tempo, origin, value.as_str().unwrap_or_default(), cmd)?;
        if ticks == 0 {
            return Err(bad(cmd, "clip length must be positive"));
        }
        let end_tick = origin.checked_add(ticks).ok_or_else(|| bad(cmd, "clip end is out of range"))?;
        let end = s.tempo.tick_to_samples(end_tick, s.sample_rate);
        if end > MAX_POSITION {
            return Err(bad(cmd, "clip end is out of range"));
        }
        params["start"] = Value::from(start);
        params["end"] = Value::from(end);
        if let Some(object) = params.as_object_mut() {
            object.remove("length");
        }
    }
    range_param(e, cmd, &params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn musical_addresses_are_one_based_and_validate_each_component() {
        let map = TempoMap::default();
        for (text, ticks) in [("1|1", 0), ("1|1|0", 0), ("2|3", 5760), ("2|3|120", 5880), ("1|4|959", 3839)] {
            assert_eq!(parse(&map, 0, text, "test").unwrap(), ticks, "{text}");
        }
        for text in [
            "",
            "1",
            "1:2",
            "0:12",
            "00:00:10:00",
            "0|1",
            "1|0",
            "1|5",
            "-1|1",
            "1|-1",
            "1|1|-1",
            "1|1|960",
            "1|1|0|0",
            "1||2",
            "1.5|1",
            "a|1",
            "1|1|",
            "9223372036854775807|1",
            "9223372036854775808|1",
        ] {
            assert!(parse(&map, 0, text, "test").is_err(), "{text}");
        }
    }

    #[test]
    fn meter_changes_and_local_origins_determine_beats_and_bars() {
        let mut map = TempoMap::default();
        map.set_meter(7680, 6, 8).unwrap();
        assert_eq!(parse(&map, 0, "3|6|120", "test").unwrap(), 10200);
        assert_eq!(parse(&map, 0, "4|1", "test").unwrap(), 10560);
        assert!(parse(&map, 0, "3|7", "test").is_err());
        assert!(parse(&map, 0, "3|1|480", "test").is_err());
        assert_eq!(parse(&map, 7680, "2|1", "test").unwrap(), 2880);
        assert_eq!(parse(&map, 3840, "2|6", "test").unwrap(), 6240);
        // A mid-bar origin has a partial local bar before the meter change.
        assert_eq!(parse(&map, 6720, "2|1", "test").unwrap(), 960);
        assert!(parse(&map, 6720, "1|3", "test").is_err());
        assert!(parse(&map, i64::MAX, "1|2", "test").is_err());
    }
}
