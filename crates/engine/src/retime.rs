//! Keep musical-time material on its bars when the tempo map changes or the song start moves.
//!
//! Positions are stored in samples, so a change to the tempo map has to move whatever is meant
//! to sit on a bar. What follows the bars:
//! - On ticks-timebase tracks (every playlist, not only the active one): clip starts and
//!   automation. A MIDI clip is musical end to end, so its end, fades, gain breakpoints and sync
//!   point follow too (its notes are already in ticks). Audio and video clips keep their length,
//!   file offset, fades and gain envelope: the audio is not stretched.
//! - Markers and memory locations, except a track-lane marker on a samples-timebase track; key
//!   changes; the edit and timeline selections (the loop range).
//!
//! What stays: samples-timebase tracks, time-locked clips (locked to their sample position, like
//! Logic's Lock SMPTE Position), the playhead, the song start, and pre- and post-roll.
//!
//! The playlist rule (sorted, no overlap except an existing crossfade) still holds afterward: a
//! clip that would now run under the clip after it is trimmed at that clip, and a clip that a
//! time-locked one now covers loses the covered head. A change that would squeeze a clip to
//! nothing is refused, so no material is lost silently.

use crate::edit::{TimeCtx, rebase_midi_start};
use soundcraft_model::{Clip, ClipContent, Session, TrackId};
use soundcraft_time::{Range, SampleRate, Samples, TICKS_PER_QUARTER, TempoMap, to_samples};
use std::collections::HashSet;

/// A tempo map laid out for many conversions: each segment's first tick, the sample it starts at
/// and its samples per tick. The starts are summed in the order [`TempoMap`] sums them, so a
/// lookup returns exactly what [`TempoMap::samples_to_ticks_f`] and `tick_to_samples` compute,
/// in O(log segments) instead of O(segments).
struct Segments {
    ticks: Vec<f64>,
    starts: Vec<f64>,
    per_tick: Vec<f64>,
}

impl Segments {
    fn new(map: &TempoMap, sr: SampleRate) -> Self {
        let per = |bpm: f64| sr.as_f64() * 60.0 / (bpm * TICKS_PER_QUARTER as f64);
        let tempos = map.tempos();
        let first = tempos.first().map_or(120.0, |e| e.bpm);
        let (mut ticks, mut starts, mut per_tick) = (vec![0.0], vec![0.0], vec![per(first)]);
        let (mut acc, mut seg_tick, mut spt) = (0.0, 0i64, per(first));
        for e in tempos.iter().skip(1) {
            acc += (e.tick - seg_tick) as f64 * spt;
            seg_tick = e.tick;
            spt = per(e.bpm);
            ticks.push(e.tick as f64);
            starts.push(acc);
            per_tick.push(spt);
        }
        Segments { ticks, starts, per_tick }
    }

    fn segment(&self, i: usize) -> (f64, f64, f64) {
        match (self.ticks.get(i), self.starts.get(i), self.per_tick.get(i)) {
            (Some(t), Some(a), Some(p)) => (*t, *a, *p),
            _ => (0.0, 0.0, 1.0),
        }
    }

    /// Samples → fractional ticks.
    fn ticks_at(&self, s: Samples) -> f64 {
        let s = s as f64;
        let i = self.starts.get(1..).map_or(0, |rest| rest.partition_point(|a| *a <= s));
        let (t, a, p) = self.segment(i);
        t + (s - a) / p
    }

    /// Fractional ticks → samples (unrounded).
    fn samples_at(&self, tick: f64) -> f64 {
        let i = self.ticks.get(1..).map_or(0, |rest| rest.partition_point(|t| *t < tick));
        let (t, a, p) = self.segment(i);
        a + (tick - t) * p
    }
}

/// Where a sample under the old map falls under the new one: the same fractional tick, rounded
/// once. Positions are always converted from their sample under the map they were placed with,
/// so an unchanged stretch of the map leaves every position exactly where it was.
pub struct Conversion {
    old: Segments,
    new: Segments,
}

impl Conversion {
    pub fn new(old: &TempoMap, new: &TempoMap, sr: SampleRate) -> Self {
        Conversion { old: Segments::new(old, sr), new: Segments::new(new, sr) }
    }

    pub fn at(&self, x: Samples) -> Samples {
        to_samples(self.new.samples_at(self.old.ticks_at(x)))
    }
}

/// Re-place musical material after the tempo map changed from `old` to the session's map.
/// On error `s` is partly changed: commands roll the document back.
pub fn retime_to_tempo(s: &mut Session, old: &TempoMap) -> Result<(), String> {
    if old.tempos() == s.tempo.tempos() {
        return Ok(());
    }
    let conv = Conversion::new(old, &s.tempo, s.sample_rate);
    move_material(s, &|x| conv.at(x))
}

/// Move all musical material (see the module docs) by the position map `at`, which must be
/// monotonic. Used for tempo changes and for moving the song start.
pub fn move_material(s: &mut Session, at: &dyn Fn(Samples) -> Samples) -> Result<(), String> {
    let map = s.tempo.clone();
    let ctx = TimeCtx { tempo: &map, sr: s.sample_rate };
    let ticks: HashSet<TrackId> = s.tracks.iter().filter(|t| t.ticks_timebase).map(|t| t.id).collect();
    for tr in s.tracks.iter_mut().filter(|t| t.ticks_timebase) {
        for pl in &mut tr.playlists {
            move_clips(&mut pl.clips, at, ctx).map_err(|clip| format!("the change squeezes clip `{clip}` on track `{}` to nothing", tr.name))?;
            pl.sort();
        }
        for lane in &mut tr.automation {
            for p in &mut lane.points {
                p.at = at(p.at);
            }
            lane.points.sort_by_key(|p| p.at);
        }
    }
    for m in s.markers.iter_mut().filter(|m| m.track.is_none_or(|t| ticks.contains(&t))) {
        (m.start, m.end) = (at(m.start), at(m.end));
    }
    for (k, _) in &mut s.key_signatures {
        *k = at(*k);
    }
    s.key_signatures.sort_by_key(|k| k.0);
    for r in [&mut s.edit.selection, &mut s.edit.timeline_selection] {
        *r = Range::new(at(r.start), at(r.end));
    }
    Ok(())
}

/// Move a playlist's clips and restore the playlist rule. The error is the name of a clip that
/// would vanish.
fn move_clips(clips: &mut [Clip], at: &dyn Fn(Samples) -> Samples, ctx: TimeCtx<'_>) -> Result<(), String> {
    let was: Vec<Range> = clips.iter().map(Clip::range).collect();
    for c in clips.iter_mut().filter(|c| !c.time_locked) {
        move_clip(c, at);
    }
    // The order the clips now play in; a tie keeps the earlier clip first.
    let mut order: Vec<usize> = (0..clips.len()).collect();
    order.sort_by_key(|&i| clips.get(i).map_or(0, |c| c.start));
    let midi = |c: &Clip| matches!(c.content, ClipContent::Midi { .. }) && !c.time_locked;
    for w in order.windows(2) {
        let (ia, ib) = (w[0], w[1]);
        let (Some(wa), Some(wb), Some(a), Some(b)) = (was.get(ia), was.get(ib), clips.get(ia), clips.get(ib)) else { continue };
        // Only neighbors that already overlapped (a crossfade) may keep overlapping: by the same
        // musical span for MIDI, by the same samples for audio, which keeps its length.
        let had = if ib == ia + 1 { (wa.end - wb.start).max(0) } else { 0 };
        let keep = if had > 0 && midi(a) && midi(b) { (at(wa.end) - at(wb.start)).max(0) } else { had };
        if a.end() <= b.start.saturating_add(keep) {
            continue;
        }
        if !a.time_locked {
            let end = b.start.saturating_add(keep);
            let Some(a) = clips.get_mut(ia) else { continue };
            if end <= a.start {
                return Err(a.name.clone());
            }
            a.trim_end_to(end);
        } else if !b.time_locked {
            let start = a.end() - keep;
            let Some(b) = clips.get_mut(ib) else { continue };
            if start >= b.end() {
                return Err(b.name.clone());
            }
            let old = b.start;
            b.trim_start_to(start);
            rebase_midi_start(b, old, ctx);
        }
        // Two time-locked clips kept their places, so they still fit as they did.
    }
    Ok(())
}

fn move_clip(c: &mut Clip, at: &dyn Fn(Samples) -> Samples) {
    let start = at(c.start);
    if !matches!(c.content, ClipContent::Midi { .. }) {
        c.start = start;
        return;
    }
    // A MIDI clip is musical from end to end: re-place every edge and offset as a musical position.
    let old = c.start;
    let rel = |off: Samples| at(old.saturating_add(off)) - start;
    let end = rel(c.length).max(1);
    c.fade_in.len = rel(c.fade_in.len);
    c.fade_out.len = end - rel(c.length - c.fade_out.len);
    for (t, _) in &mut c.gain_env {
        *t = rel(*t);
    }
    if c.sync_point != 0 {
        c.sync_point = rel(c.sync_point);
    }
    c.start = start;
    c.trim_end_to(start + end);
}

/// Put back the material a tempo gesture started from (see `Engine::execute_merged`), so each
/// step converts from the gesture's start instead of the previous step's result.
pub fn restore_material(s: &mut Session, base: &Session) {
    for tr in s.tracks.iter_mut().filter(|t| t.ticks_timebase) {
        if let Some(b) = base.track(tr.id) {
            tr.playlists.clone_from(&b.playlists);
            tr.automation.clone_from(&b.automation);
        }
    }
    s.markers.clone_from(&base.markers);
    s.key_signatures.clone_from(&base.key_signatures);
    s.edit.selection = base.edit.selection;
    s.edit.timeline_selection = base.edit.timeline_selection;
}

/// Whether two documents hold the same tempo map and the same material a retime moves, so a
/// gesture can keep converting from its start.
pub fn same_material(a: &Session, b: &Session) -> bool {
    a.tempo == b.tempo
        && a.markers == b.markers
        && a.key_signatures == b.key_signatures
        && a.edit.selection == b.edit.selection
        && a.edit.timeline_selection == b.edit.timeline_selection
        && a.tracks.len() == b.tracks.len()
        && a.tracks.iter().zip(&b.tracks).all(|(x, y)| {
            x.id == y.id
                && x.ticks_timebase == y.ticks_timebase
                && (!x.ticks_timebase || (x.playlists == y.playlists && x.automation == y.automation))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-position conversion the segment table replaced: a walk of every tempo event.
    fn reference(old: &TempoMap, new: &TempoMap, sr: SampleRate, x: Samples) -> Samples {
        let tick = old.samples_to_ticks_f(x, sr);
        let per_tick = |bpm: f64| sr.as_f64() * 60.0 / (bpm * TICKS_PER_QUARTER as f64);
        let tempos = new.tempos();
        let (mut acc, mut seg) = (0.0, 0.0);
        let mut bpm = tempos.first().map_or(120.0, |e| e.bpm);
        for e in tempos.iter().skip(1) {
            let t = e.tick as f64;
            if t >= tick {
                break;
            }
            acc += (t - seg) * per_tick(bpm);
            seg = t;
            bpm = e.bpm;
        }
        to_samples(acc + (tick - seg) * per_tick(bpm))
    }

    #[test]
    fn the_segment_table_matches_the_per_position_walk_exactly() {
        let sr = SampleRate::HZ_48000;
        let mut rng = soundcraft_midi::XorShift::new(0x5eed);
        for round in 0..40 {
            let (mut old, mut new) = (TempoMap::default(), TempoMap::default());
            for m in [&mut old, &mut new] {
                let n = 1 + rng.next_u64() % if round % 2 == 0 { 8 } else { 400 };
                for _ in 0..n {
                    let tick = (rng.next_u64() % 2_000_000) as i64;
                    let bpm = 5.0 + (rng.next_u64() % 995_000) as f64 / 1000.0;
                    m.set_tempo(tick, bpm).unwrap();
                }
            }
            let conv = Conversion::new(&old, &new, sr);
            for i in 0..2_000usize {
                // Tempo event boundaries and their neighbors, then arbitrary positions.
                let x = if i % 3 == 0 {
                    let e = old.tempos()[(i / 3) % old.tempos().len()];
                    old.tick_to_samples(e.tick, sr) + (i as i64 % 5) - 2
                } else {
                    (rng.next_u64() % 2_000_000_000) as i64
                };
                assert_eq!(conv.at(x), reference(&old, &new, sr, x), "round {round}, sample {x}");
                assert_eq!(conv.old.ticks_at(x).to_bits(), old.samples_to_ticks_f(x, sr).to_bits(), "ticks at {x}");
            }
            for e in new.tempos() {
                assert_eq!(to_samples(conv.new.samples_at(e.tick as f64)), new.tick_to_samples(e.tick, sr));
            }
        }
    }
}
