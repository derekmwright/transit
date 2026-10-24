//! Looped clips (ADR 0019): audio wraps sample accurately without allocating; MIDI notes and
//! controllers repeat per cycle, a note crossing the seam ends at it, and the result does not
//! depend on how the timeline is cut into blocks.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_midi::{CtrlEvent, Note, Sequence};
use soundcraft_model::{ChannelFormat, ClipId, SourceAudio, SourceId, TrackKind};
use soundcraft_time::TICKS_PER_QUARTER;

fn audio_session(start: Samples, length: Samples, offset: Samples, range: Option<(Samples, Samples)>) -> (Session, Clip) {
    let mut s = Session::default();
    s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    let data: Vec<f32> = (0..1000).map(|i| i as f32 / 1000.0).collect();
    s.pool.insert(SourceId(1), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![data] })));
    let mut c = Clip::audio(ClipId(1), "loop", SourceId(1), offset, start, length);
    c.set_loop(range.map(|(start, end)| ClipLoop { start, end }));
    (s, c)
}

#[test]
fn audio_wraps_at_the_seam_sample_accurately_and_in_any_block_size() {
    let (s, c) = audio_session(50, 2500, 10, Some((100, 400)));
    let total = 2600usize;
    let expect: Vec<f32> = (0..total as i64)
        .map(|k| {
            let rel = k - 50;
            if !(0..2500).contains(&rel) {
                return 0.0;
            }
            let at = c.content_pos(rel) + 10;
            at as f32 / 1000.0
        })
        .collect();
    // The seam itself: the sample at clip time 400 is content 100.
    assert_eq!(expect[50 + 400], 110.0 / 1000.0);
    assert_eq!(expect[50 + 399], 409.0 / 1000.0);
    for block in [total, 128, 97, 1] {
        let mut got = vec![0.0f32; total];
        let mut pos = 0;
        while pos < total {
            let n = block.min(total - pos);
            let mut buf = vec![vec![0.0f32; n]];
            let (_, stats) = transit_rt::testing::measure(|| render_clip(&s, &c, pos as i64, n, &mut buf));
            assert!(stats.is_zero(), "render_clip allocated on the audio thread: {stats:?}");
            got[pos..pos + n].copy_from_slice(&buf[0]);
            pos += n;
        }
        assert_eq!(got, expect, "block {block}");
    }
}

fn note(pitch: u8, beat: i64, beats: i64) -> Note {
    Note { pitch, velocity: 100, release_velocity: 64, channel: 0, start: beat * TICKS_PER_QUARTER, length: beats * TICKS_PER_QUARTER }
}

/// A MIDI track with one clip of `beats_len` beats whose first four beats loop.
fn midi_session(beats_len: i64) -> (Session, TrackId, Samples) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Midi, ChannelFormat::Mono, None);
    let beat = s.tempo.tick_to_samples(TICKS_PER_QUARTER, s.sample_rate);
    let mut seq = Sequence::default();
    seq.notes.push(note(60, 0, 1));
    seq.notes.push(note(64, 3, 2)); // crosses the seam at beat 4
    seq.ctrls.push(CtrlEvent { tick: 2 * TICKS_PER_QUARTER, channel: 0, kind: CtrlKind::Cc(1), value: 90 });
    let mut c = Clip::midi(ClipId(1), "m", 0, beat * beats_len, seq);
    c.set_loop(Some(ClipLoop { start: 0, end: 4 * beat }));
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
    (s, t, beat)
}

fn collect(s: &Session, t: TrackId, total: Samples, block: usize) -> Vec<(Samples, String)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < total {
        let n = block.min((total - pos) as usize);
        visit_clip_events(s, s.track(t).unwrap(), pos, n, |at, e| {
            let what = match e {
                ClipEvent::On(k, _, _) => format!("on {k}"),
                ClipEvent::Off(k, _) => format!("off {k}"),
                ClipEvent::Control(m, _) => format!("cc {} {}", m[1], m[2]),
            };
            out.push((pos + at as Samples, what));
        });
        pos += n as Samples;
    }
    out.sort();
    out
}

#[test]
fn midi_notes_wrap_cycle_by_cycle_and_a_note_over_the_seam_ends_at_it() {
    // 10 beats: two full cycles and half of a third.
    let (s, t, beat) = midi_session(10);
    let total = 12 * beat;
    let got = collect(&s, t, total, total as usize);
    let at = |beats: i64| beats * beat;
    let mut want = vec![
        (at(0), "on 60"),
        (at(1), "off 60"),
        (at(2), "cc 1 90"),
        (at(3), "on 64"),
        (at(4), "off 64"), // the held note ends at the seam
        (at(4), "on 60"),
        (at(5), "off 60"),
        (at(6), "cc 1 90"),
        (at(7), "on 64"),
        (at(8), "off 64"),
        (at(8), "on 60"),
        (at(9), "off 60"),
        (at(10), "cc 1 90"), // not played: the clip ends at beat 10
    ];
    want.retain(|(a, w)| !(*a >= at(10) && w.starts_with("cc")));
    let want: Vec<(Samples, String)> = want.into_iter().map(|(a, w)| (a, w.to_string())).collect();
    let mut want = want;
    want.sort();
    assert_eq!(got, want);
    // The same events whatever the block size (a note's cycles are found per block).
    for block in [480, 1001, 4096, 24_000] {
        assert_eq!(collect(&s, t, total, block), want, "block {block}");
    }
}

#[test]
fn the_latest_cycle_before_a_position_is_found_for_chase() {
    let beat = 1000;
    let l = ClipLoop { start: 0, end: 4 * beat };
    // A controller 2 beats in: cycle 1 lands at beat 6, cycle 2 at beat 10.
    let first = cycle_at(0, l, 2 * beat, 1);
    assert_eq!(first, 6 * beat);
    assert_eq!(cycle_at(0, l, 2 * beat, 2), 10 * beat);
    // Seeking to beat 9 the latest one before it is cycle 1; to beat 11, cycle 2.
    let latest = |until: i64| cycle_at(0, l, 2 * beat, (until - 1 - first).div_euclid(l.cycle()) + 1);
    assert_eq!(latest(9 * beat), 6 * beat);
    assert_eq!(latest(11 * beat), 10 * beat);
}

/// A clip that starts between two ticks (7 samples, 25 per tick at 120 bpm and 48 kHz) whose loop
/// range ends on a bar line. Notes sound on the tick grid, so every cycle of a note must sound
/// exactly one bar after the last: the sample cycle (a bar minus 7) used to slip the grid 7
/// samples per cycle, and the note on tick 0 never sounded at all.
#[test]
fn a_loop_on_a_clip_that_starts_between_ticks_keeps_every_cycle_on_the_grid_offset_of_the_first() {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Midi, ChannelFormat::Mono, None);
    let beat = s.tempo.tick_to_samples(TICKS_PER_QUARTER, s.sample_rate);
    let (start, bar, cycles) = (7, 4 * beat, 51);
    let mut seq = Sequence::default();
    seq.notes.push(note(60, 0, 1));
    seq.notes.push(note(64, 2, 1));
    let mut c = Clip::midi(ClipId(1), "m", start, cycles * bar - start, seq);
    c.set_loop(Some(ClipLoop { start: 0, end: bar - start }));
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
    let ons = |block: usize| -> Vec<(Samples, String)> {
        collect(&s, t, cycles * bar + 100, block).into_iter().filter(|(_, w)| w.starts_with("on")).collect()
    };
    let want: Vec<(Samples, String)> =
        (0..cycles).flat_map(|k| [(k * bar, "on 60".to_string()), (k * bar + 2 * beat, "on 64".to_string())]).collect();
    assert_eq!(ons(1 << 20), want);
    assert_eq!(ons(4096), want, "the same in small blocks");
}
