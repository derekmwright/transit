//! Track > Freeze, Unfreeze and Flatten.
//!
//! Freeze renders a track's output after its inserts and before its fader (the render Commit
//! makes) into a pool source and marks the track frozen. The mix then plays that audio in place
//! of the instrument and inserts, which make no instances and keep their settings
//! (`Session::with_frozen_audio`). [`edits_frozen`] refuses any command that would change what
//! was rendered. Unfreeze clears the mark. Flatten keeps the render as the track's audio.

use super::*;
use crate::cmd;
use crate::io::{add_source, offline, refuse_longer, render_limit};
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_model::{AutoParam, Clip, Frozen, Playlist, Session, Track, TrackKind};
use soundcraft_time::Range;

/// What a frozen render adds past the last clip: a fixed tail, as Commit does (plug-ins report no
/// tail length to the host).
const TAIL_SECS: f64 = 2.0;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "track.freeze",
            "Freeze",
            ["Track"],
            None,
            "{tracks?} — renders each audio or instrument track after its inserts and before its fader, then plays the render instead of the plug-ins until unfrozen",
            has_selection,
            freeze
        ),
        cmd!("track.unfreeze", "Unfreeze", ["Track"], None, "{tracks?} — brings the instrument and inserts back", has_selection, unfreeze),
        cmd!(
            "track.flatten",
            "Flatten",
            ["Track"],
            None,
            "{tracks?} — freezes, then keeps the render as the track's audio and removes its instrument and inserts",
            has_selection,
            flatten
        ),
    ]
}

/// The first track frozen before `before` whose material `after` changed while keeping the same
/// freeze (a command that unfreezes, flattens or removes the track is allowed).
pub(crate) fn edits_frozen(before: &Session, after: &Session) -> bool {
    before.tracks.iter().filter(|t| t.freeze.is_some()).any(|t| after.track(t.id).is_some_and(|a| a.freeze == t.freeze && !t.same_freeze_inputs(a)))
}

/// Renders `t` and freezes it. False when it already is.
fn freeze_track(e: &mut Engine, id: &str, t: TrackId) -> Result<bool> {
    let s = e.session();
    let tr = s.track(t).ok_or_else(|| bad(id, format!("no track {}", t.0)))?;
    if tr.freeze.is_some() {
        return Ok(false);
    }
    if !matches!(tr.kind, TrackKind::Audio | TrackKind::Instrument) {
        return Err(bad(id, format!("`{}` is not an audio or instrument track", tr.name)));
    }
    let end = tr.clips().iter().map(Clip::end).max().unwrap_or(0);
    if end <= 0 {
        return Err(bad(id, format!("nothing to freeze: `{}` has no clips", tr.name)));
    }
    let range = Range::new(0, end + s.sample_rate.samples(TAIL_SECS));
    refuse_longer(s, id, &tr.name, "freeze", range.len(), render_limit(tr.channels().max(2)))?;
    let _offline = offline();
    let (audio, latency) = soundcraft_mix::render_track_pre_fader_timed(s, t, range);
    let name = format!("{}.frz", tr.name);
    let rate = s.sample_rate.hz();
    let revision = e.revision;
    let s = e.session_mut();
    let source = add_source(s, &name, AudioBuffer { sample_rate: rate, channels: audio }, None, FileFormat::Wav);
    if let Some(tr) = s.track_mut(t) {
        tr.freeze = Some(Frozen {
            source,
            start: range.start,
            length: range.len(),
            hash: tr.freeze_hash(),
            revision,
            latency: u32::try_from(latency).unwrap_or(u32::MAX),
        });
    }
    Ok(true)
}

fn freeze(e: &mut Engine, p: &Value) -> Result<Value> {
    let mut done = Vec::new();
    for t in tracks_required(e, "track.freeze", p)? {
        if freeze_track(e, "track.freeze", t)? {
            done.push(t.0);
        }
    }
    Ok(json!({"tracks": done}))
}

fn unfreeze(e: &mut Engine, p: &Value) -> Result<Value> {
    let tracks = tracks_required(e, "track.unfreeze", p)?;
    let s = e.session_mut();
    let mut done = Vec::new();
    for t in tracks {
        if let Some(tr) = s.track_mut(t).filter(|tr| tr.freeze.is_some()) {
            tr.freeze = None;
            done.push(t.0);
        }
    }
    Ok(json!({"tracks": done}))
}

fn flatten(e: &mut Engine, p: &Value) -> Result<Value> {
    let tracks = tracks_required(e, "track.flatten", p)?;
    let mut done = Vec::new();
    for t in tracks {
        freeze_track(e, "track.flatten", t)?;
        let s = e.session_mut();
        let clip_id = s.new_clip_id();
        let Some(tr) = s.track_mut(t) else { continue };
        let Some(f) = tr.freeze.take() else { continue };
        flatten_into(tr, &f, clip_id);
        done.push(t.0);
    }
    Ok(json!({"tracks": done}))
}

/// Makes `tr` an audio track of its render: one clip, no instrument, inserts or plug-in
/// automation. The fader, pan, sends and other automation stay.
fn flatten_into(tr: &mut Track, f: &Frozen, clip: soundcraft_model::ClipId) {
    tr.playlists = vec![Playlist { name: tr.name.clone(), clips: vec![Clip::audio(clip, tr.name.clone(), f.source, 0, f.start, f.length)] }];
    tr.active_playlist = 0;
    tr.kind = TrackKind::Audio;
    tr.instrument = None;
    tr.mixer.inserts.iter_mut().for_each(|i| *i = None);
    tr.automation.retain(|l| !matches!(l.param, AutoParam::Plugin { .. }));
    tr.ticks_timebase = false;
    tr.elastic = None;
    tr.view = "waveform".into();
}
