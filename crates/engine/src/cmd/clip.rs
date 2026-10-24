//! Clip menu commands.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{ClipContent, ClipLoop};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!("clip.edit_lock", "Edit Lock/Unlock", ["Clip"], Some("Cmd+L"), "{clips?}", has_selection, |e, p| flag(
            e,
            p,
            |c| c.edit_locked,
            |c, v| c.edit_locked = v
        )),
        cmd!("clip.time_lock", "Time Lock/Unlock", ["Clip"], Some("Ctrl+Alt+L"), "{clips?}", has_selection, |e, p| flag(
            e,
            p,
            |c| c.time_locked,
            |c, v| c.time_locked = v
        )),
        cmd!("clip.send_to_back", "Send to Back", ["Clip"], Some("Alt+Shift+B"), "{clips?}", has_selection, |e, p| layer(e, p, false)),
        cmd!("clip.bring_to_front", "Bring to Front", ["Clip"], Some("Alt+Shift+F"), "{clips?}", has_selection, |e, p| layer(e, p, true)),
        cmd!("clip.rating", "Rating", ["Clip", "Rating"], Some("Cmd+Alt+0-5"), "{clips?, rating: 0..5}", has_selection, |e, p| {
            let ids = clip_ids_param(e, p);
            let r = i64_or(p, "rating", 0).clamp(0, 5) as u8;
            let s = e.session_mut();
            for id in &ids {
                if let Some(c) = s.find_clip_mut(*id) {
                    c.rating = r
                }
            }
            Ok(json!({"rating": r}))
        }),
        cmd!("clip.group", "Group", ["Clip"], Some("Cmd+Alt+G"), "{clips?}", has_selection, |e, p| {
            let ids = clip_ids_param(e, p);
            if ids.len() < 2 {
                return Err(bad("clip.group", "select two or more clips"));
            }
            let s = e.session_mut();
            let g = s.alloc();
            for id in &ids {
                if let Some(c) = s.find_clip_mut(*id) {
                    c.group = Some(g)
                }
            }
            Ok(json!({"group": g}))
        }),
        cmd!("clip.ungroup", "Ungroup", ["Clip"], Some("Cmd+Alt+U"), "{clips?}", has_selection, |e, p| {
            let ids = clip_ids_param(e, p);
            let s = e.session_mut();
            for id in &ids {
                if let Some(c) = s.find_clip_mut(*id) {
                    c.group = None
                }
            }
            Ok(json!({}))
        }),
        cmd!("clip.ungroup_all", "Ungroup All", ["Clip"], None, "{}", always, |e, _| {
            for t in &mut e.session_mut().tracks {
                for pl in &mut t.playlists {
                    for c in &mut pl.clips {
                        c.group = None
                    }
                }
            }
            Ok(json!({}))
        }),
        cmd!(
            "clip.loop_to",
            "Loop To",
            [],
            None,
            "{clip, to: position, independent?: bool, overwrite?: bool} - the loop handle: sets the clip's arrangement length to `to`, cycling its content (an unlooped clip is first looped over its whole length, which must be at least 64 samples); at or before the first cycle's end it unloops. independent = lay plain copies up to `to` instead (a trimmed last copy). overwrite = a longer loop cuts into the clips after it (trimming or removing them, stopping at an edit-locked one) instead of stopping at the next clip; one undo step",
            always,
            loop_to
        ),
        cmd!(
            "clip.set_loop",
            "Set Loop",
            [],
            None,
            "{clips?, start?: samples, end?: samples} - loops the part of the clip from `start` to `end` (clip time, default the whole clip; at least 64 samples long); the clip keeps its length and cycles the range once playback reaches `end`",
            always,
            set_loop
        ),
        cmd!("clip.clear_loop", "Clear Loop", [], None, "{clips?} - plays the clip once again, cut to its first pass", always, clear_loop),
        cmd!("clip.loop", "Loop...", ["Clip"], Some("Cmd+Alt+L"), "{clips?, count?: n, length?: samples}", has_selection, loop_clips),
        cmd!("clip.rename", "Rename...", ["Clip"], Some("Cmd+Alt+Shift+R"), "{clip?, name}", has_selection, rename),
        cmd!("clip.gain", "Clip Gain", [], None, "{clips?, db | delta_db}", has_selection, gain),
        cmd!("clip.gain_nudge_up", "Nudge Clip Gain Up", [], Some("Ctrl+Shift+Up"), "{clips?}", has_selection, |e, p| nudge_gain(e, p, 1.0)),
        cmd!("clip.gain_nudge_down", "Nudge Clip Gain Down", [], Some("Ctrl+Shift+Down"), "{clips?}", has_selection, |e, p| nudge_gain(e, p, -1.0)),
        cmd!("clip.gain_render", "Render", ["Clip", "Clip Gain"], None, "{clips?}", has_selection, |e, p| {
            let ids = clip_ids_param(e, p);
            let mut n = 0;
            for id in ids {
                if crate::io::render_clip_gain(e, "clip.gain_render", id)? {
                    n += 1
                }
            }
            Ok(json!({"rendered": n}))
        }),
        cmd!("clip.gain_bypass", "Bypass", ["Clip", "Clip Gain"], None, "{clips?}", has_selection, |e, p| {
            let ids = clip_ids_param(e, p);
            let s = e.session_mut();
            for id in &ids {
                if let Some(c) = s.find_clip_mut(*id) {
                    c.gain_db = 0.0;
                    c.gain_env.clear()
                }
            }
            Ok(json!({}))
        }),
        cmd!("clip.identify_sync_point", "Identify Sync Point", ["Clip"], Some("Cmd+,"), "{clip?, at?}", has_selection, |e, p| {
            let at = position_param(e, "clip.identify_sync_point", p, "at")?.unwrap_or(e.session().edit.selection.start);
            let ids = clip_ids_param(e, p);
            let s = e.session_mut();
            for id in &ids {
                if let Some(c) = s.find_clip_mut(*id) {
                    c.sync_point = (at - c.start).clamp(0, c.length)
                }
            }
            Ok(json!({}))
        }),
        cmd!("clip.quantize_to_grid", "Quantize to Grid", ["Clip"], Some("Cmd+0"), "{clips?}", has_selection, quantize_to_grid),
        cmd!("clip.color", "Clip Color", [], None, "{clips?, color: [r,g,b] | null}", has_selection, |e, p| {
            let ids = clip_ids_param(e, p);
            let col = p.get("color").and_then(Value::as_array).map(|a| {
                let g = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0).min(255) as u8;
                [g(0), g(1), g(2)]
            });
            let s = e.session_mut();
            for id in &ids {
                if let Some(c) = s.find_clip_mut(*id) {
                    c.color = col
                }
            }
            Ok(json!({}))
        }),
        cmd!(
            "clip.elastic_properties",
            "Elastic Properties",
            ["Clip"],
            Some("Alt+5"),
            "{clips?, ratio?: 1.0} — on Elastic Audio tracks (polyphonic/rhythmic/monophonic/x-form) the stretch is rendered pitch-preserving; elsewhere it is varispeed",
            has_selection,
            elastic
        ),
        cmd!("clip.remove_warp", "Remove Warp", ["Clip"], None, "{clips?}", has_selection, remove_warp),
        cmd!(
            "clip.place_source",
            "Place Audio File",
            [],
            None,
            "{source: id, track?: id|name (default: a new track), at?: position}",
            always,
            |e, p| {
                let src = p
                    .get("source")
                    .and_then(Value::as_u64)
                    .map(soundcraft_model::SourceId)
                    .ok_or_else(|| bad("clip.place_source", "`source` id required"))?;
                let at = position_param(e, "clip.place_source", p, "at")?.unwrap_or(e.session().edit.selection.start).max(0);
                let track = track_param(e, "clip.place_source", p, "track")?;
                let (name, frames, chans) = e
                    .session()
                    .source(src)
                    .map(|x| (x.name.clone(), i64::try_from(x.frames).unwrap_or(0), usize::from(x.channels)))
                    .ok_or_else(|| bad("clip.place_source", "no such audio file"))?;
                let s = e.session_mut();
                let t = match track {
                    Some(t) if s.track(t).is_some_and(|x| x.kind == soundcraft_model::TrackKind::Audio) => t,
                    Some(_) => return Err(bad("clip.place_source", "audio files go on audio tracks")),
                    None => s.add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::for_channels(chans.min(2)), Some(&name)),
                };
                let (sh, len) = (s.edit.edit_mode == soundcraft_model::EditMode::Shuffle, frames.max(1));
                let at = if sh { crate::edit::shuffle_slot(s, t, at, &[]) } else { at };
                if sh {
                    crate::edit::make_room(s, t, at, len).map_err(|why| bad("clip.place_source", why))?;
                }
                let id = s.new_clip_id();
                crate::edit::place_clip(s, t, soundcraft_model::Clip::audio(id, name, src, 0, at, len));
                Ok(json!({"clip": id, "track": t}))
            }
        ),
        cmd!("clip.capture", "Capture...", ["Clip"], Some("Cmd+R"), "{name?}", has_range, |e, p| {
            // Captures the selection as a new whole-file-referencing clip in the clip list (we keep it on the track).
            let name = str_param(p, "name").unwrap_or("Captured").to_string();
            let ids = clip_ids_param(e, p);
            let s = e.session_mut();
            for id in &ids {
                if let Some(c) = s.find_clip_mut(*id) {
                    c.name = name.clone()
                }
            }
            Ok(json!({"clips": ids.len()}))
        }),
    ]
}

/// Warp clips. On Elastic tracks the material is rendered with a pitch-preserving time stretch
/// (the original source is remembered in `edit.values` so Remove Warp restores it).
fn elastic(e: &mut Engine, p: &Value) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let ratio = f64_or(p, "ratio", 1.0).clamp(0.05, 20.0);
    let s = e.session_mut();
    let mut rendered = 0;
    for id in &ids {
        let Some((t, c)) = s.find_clip(*id).map(|(t, c)| (t, c.clone())) else { continue };
        let elastic = s.track(t).and_then(|x| x.elastic.clone()).filter(|a| a != "varispeed");
        // Start from the unwarped original if this clip was warped before.
        let key = |k: &str| format!("elastic.{}.{k}", id.0);
        let orig = warp_original(s, *id);
        let mut base = c.clone();
        if let Some((src, off, len)) = orig {
            base.content = soundcraft_model::ClipContent::Audio { source: src, offset: off };
            base.length = len.max(1);
            base.stretch = 1.0;
        }
        if elastic.is_none() || !base.is_audio() {
            if let Some(cm) = s.find_clip_mut(*id) {
                cm.stretch = ratio;
            }
            continue;
        }
        let out_len = soundcraft_time::to_samples(base.length as f64 * ratio).max(1);
        let Some(src) = super::more_util::render_stretched(s, "clip.elastic_properties", &base, out_len)? else { continue };
        if orig.is_none()
            && let soundcraft_model::ClipContent::Audio { source, offset } = base.content
        {
            s.edit.values.insert(key("src"), source.0 as f64);
            s.edit.values.insert(key("off"), offset as f64);
            s.edit.values.insert(key("len"), base.length as f64);
        }
        s.edit.values.insert(key("ratio"), ratio);
        let mut nc = base;
        nc.content = soundcraft_model::ClipContent::Audio { source: src, offset: 0 };
        // The new file holds every cycle already (ADR 0019).
        nc.looping = None;
        nc.length = out_len;
        nc.stretch = 1.0;
        super::more_util::replace_clip(s, t, *id, nc);
        rendered += 1;
    }
    Ok(json!({"ratio": ratio, "rendered": rendered}))
}

fn remove_warp(e: &mut Engine, p: &Value) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let s = e.session_mut();
    let mut n = 0;
    for id in &ids {
        let key = |k: &str| format!("elastic.{}.{k}", id.0);
        let orig = warp_original(s, *id);
        if let Some(c) = s.find_clip_mut(*id) {
            c.stretch = 1.0;
            if let Some((src, off, len)) = orig {
                c.content = soundcraft_model::ClipContent::Audio { source: src, offset: off };
                c.length = len.max(1);
                c.fade_out.len = c.fade_out.len.min(c.length);
                n += 1;
            }
        }
        for k in ["src", "off", "len", "ratio"] {
            s.edit.values.remove(&key(k));
        }
    }
    Ok(json!({"restored": n}))
}

/// The audio a warped clip was rendered from, while that file is still in the session.
fn warp_original(s: &soundcraft_model::Session, id: ClipId) -> Option<(soundcraft_model::SourceId, Samples, Samples)> {
    let v = |k: &str| s.edit.values.get(&format!("elastic.{}.{k}", id.0)).copied();
    let src = soundcraft_model::SourceId(v("src")? as u64);
    s.source(src)?;
    Some((src, v("off")? as i64, v("len")? as i64))
}

fn flag(e: &mut Engine, p: &Value, get: fn(&soundcraft_model::Clip) -> bool, set: fn(&mut soundcraft_model::Clip, bool)) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let s = e.session_mut();
    let v = p.get("value").and_then(Value::as_bool).unwrap_or_else(|| !ids.iter().all(|id| s.find_clip(*id).is_some_and(|(_, c)| get(c))));
    for id in &ids {
        if let Some(c) = s.find_clip_mut(*id) {
            set(c, v);
        }
    }
    Ok(json!({"value": v, "clips": ids.len()}))
}

fn layer(e: &mut Engine, p: &Value, front: bool) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let s = e.session_mut();
    for t in &mut s.tracks {
        if let Some(pl) = t.playlist_mut() {
            let (mut sel, mut rest): (Vec<_>, Vec<_>) = pl.clips.drain(..).partition(|c| ids.contains(&c.id));
            if front {
                rest.append(&mut sel);
                pl.clips = rest;
            } else {
                sel.append(&mut rest);
                pl.clips = sel;
            }
        }
    }
    Ok(json!({}))
}

fn loop_clips(e: &mut Engine, p: &Value) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let count = i64_or(p, "count", 4).clamp(1, 512);
    let total = position_param(e, "clip.loop", p, "length")?;
    let s = e.session_mut();
    let mut made = 0;
    for id in ids {
        let Some((tid, mut c)) = s.find_clip(id).map(|(t, c)| (t, c.clone())) else { continue };
        let reps = total.map_or(count - 1, |l| (l / c.length.max(1)).max(1) - 1).clamp(0, 1000);
        if reps > 0 && c.loop_id.is_none() {
            let lid = s.alloc();
            if let Some(src) = s.find_clip_mut(id) {
                src.loop_id = Some(lid);
            }
            c.loop_id = Some(lid);
        }
        let mut copies = Vec::new();
        for k in 1..=reps {
            let mut nc = c.clone();
            nc.start = 0;
            copies.push((c.start + c.length * k, nc));
        }
        for (at, nc) in copies {
            let _ = crate::edit::paste(s, tid, at, &[nc], c.length, false);
            made += 1;
        }
    }
    Ok(json!({"loops": made}))
}

/// The handle's command (ADR 0019): set the arrangement length of the clip's loop to `to`.
/// An unlooped clip is first looped over its whole length. At or before the end of the first
/// cycle the loop is removed and the clip is cut to that cycle. `independent` lays Repeat copies
/// (no loop) back to back after the clip up to `to`, the last one trimmed there.
fn loop_to(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "clip.loop_to";
    let Some(&clip) = clip_ids_param(e, p).first() else { return Err(bad(id, "`clip` required")) };
    let to = position_param(e, id, p, "to")?.ok_or_else(|| bad(id, "`to` required"))?;
    let independent = p.get("independent").and_then(Value::as_bool).unwrap_or(false);
    let overwrite = p.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    let s = e.session_mut();
    let (track, base) = s.find_clip(clip).map(|(t, c)| (t, c.clone())).ok_or_else(|| bad(id, "no such clip"))?;
    if base.edit_locked {
        return Err(bad(id, "the clip is edit locked"));
    }
    if independent {
        return repeat_copies(s, track, base, to);
    }
    if to <= base.start {
        return Err(bad(id, "`to` is before the clip"));
    }
    let cycle_end = base.looping.map_or(base.end(), |l| base.start.saturating_add(l.end));
    let was_looped = base.looping.is_some();
    if !was_looped && to <= base.end() {
        return Ok(json!({"looped": false, "length": base.length}));
    }
    if !was_looped && base.length < soundcraft_model::MIN_CYCLE {
        return Err(bad(id, "the clip is too short to loop"));
    }
    // Looping the whole clip needs its source to cover it; a clip at its natural length does.
    if !was_looped && let Some(c) = s.find_clip_mut(clip) {
        c.looping = Some(ClipLoop { start: 0, end: c.length });
    }
    let mut target = if was_looped && to <= cycle_end { cycle_end } else { to };
    if overwrite && target > base.end() {
        // Make room first: the neighbors under the new span are trimmed or removed, but the
        // loop never runs into an edit-locked clip.
        if let Some(lock) = s
            .track(track)
            .map(|t| t.clips())
            .unwrap_or_default()
            .iter()
            .filter(|c| c.edit_locked && c.id != clip && c.end() > base.end())
            .map(|c| c.start.max(base.end()))
            .min()
        {
            target = target.min(lock);
        }
        if target > base.end() {
            crate::edit::clear_range(s, track, soundcraft_time::Range::new(base.end(), target), false);
        }
    }
    if was_looped
        && to <= cycle_end
        && let Some(c) = s.find_clip_mut(clip)
    {
        c.looping = None;
    }
    let Some(done) = crate::edit::trim_clip(s, clip, None, Some(target), false) else {
        return Err(bad(id, "the clip cannot be resized"));
    };
    Ok(json!({"looped": done.looping.is_some(), "length": done.length}))
}

/// Alt on the handle (a Repeat): plain copies of `base` back to back from its end up to `to`, the
/// last one trimmed there. Nothing is linked; the original is unchanged.
fn repeat_copies(s: &mut soundcraft_model::Session, track: TrackId, mut base: soundcraft_model::Clip, to: Samples) -> Result<Value> {
    base.loop_id = None;
    let len = base.length.max(1);
    let from = base.end();
    let span = to.saturating_sub(from).max(0);
    let reps = (span / len).min(MAX_REPEATS);
    let tail = if reps < MAX_REPEATS { span - reps * len } else { 0 };
    let mut made = 0;
    for k in 0..=reps {
        let mut nc = base.clone();
        nc.start = 0;
        if k == reps {
            if tail == 0 {
                break;
            }
            nc.trim_end_to(tail);
        }
        let _ = crate::edit::paste(s, track, from + len * k, &[nc], len, false);
        made += 1;
    }
    Ok(json!({"repeats": made, "tail": tail}))
}

/// Most repeats one `clip.loop_to` with `independent` lays down.
const MAX_REPEATS: i64 = 1000;

fn set_loop(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "clip.set_loop";
    let ids = clip_ids_param(e, p);
    if ids.is_empty() {
        return Err(bad(id, "select a clip"));
    }
    let s = e.session_mut();
    let mut done = 0;
    for cid in ids {
        let Some((_, c)) = s.find_clip(cid) else { continue };
        let start = i64_or(p, "start", 0).clamp(-MAX_POSITION, MAX_POSITION);
        let end = i64_or(p, "end", c.length).clamp(-MAX_POSITION, MAX_POSITION);
        if end <= 0 || end - start < soundcraft_model::MIN_CYCLE {
            return Err(bad(id, format!("a loop must be at least {} samples long and end inside the clip", soundcraft_model::MIN_CYCLE)));
        }
        if c.edit_locked {
            continue;
        }
        match &c.content {
            ClipContent::Audio { source, offset } => {
                let frames = s.source(*source).map_or(i64::MAX, |src| i64::try_from(src.frames).unwrap_or(i64::MAX));
                // The range is clip time; a stretched clip reads the file at 1 / stretch.
                let stretch = if c.stretch.is_finite() && c.stretch > 0.0 { c.stretch } else { 1.0 };
                let file = |x: Samples| offset.saturating_add((x as f64 / stretch).round() as Samples);
                if file(start) < 0 || file(end) > frames {
                    return Err(bad(id, "the loop range reaches outside the audio file"));
                }
            }
            ClipContent::Midi { .. } if start < 0 => return Err(bad(id, "a MIDI loop starts at or after the clip start")),
            _ => {}
        }
        if let Some(c) = s.find_clip_mut(cid) {
            c.looping = Some(ClipLoop { start, end });
            done += 1;
        }
    }
    Ok(json!({"clips": done}))
}

fn clear_loop(e: &mut Engine, p: &Value) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let s = e.session_mut();
    let mut done = 0;
    for cid in ids {
        let Some(c) = s.find_clip_mut(cid) else { continue };
        let Some(l) = c.looping.take() else { continue };
        if !c.edit_locked {
            c.length = c.length.min(l.end.max(1));
            c.clamp_fades();
            done += 1;
        } else {
            c.looping = Some(l);
        }
    }
    if done == 0 {
        return Err(bad("clip.clear_loop", "the selection has no loop"));
    }
    Ok(json!({"clips": done}))
}

fn rename(e: &mut Engine, p: &Value) -> Result<Value> {
    let name = str_param(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad("clip.rename", "`name` required"))?.to_string();
    let ids = clip_ids_param(e, p);
    let s = e.session_mut();
    for id in &ids {
        if let Some(c) = s.find_clip_mut(*id) {
            c.name = name.clone();
        }
    }
    Ok(json!({"clips": ids.len()}))
}

fn gain(e: &mut Engine, p: &Value) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let abs = p.get("db").and_then(Value::as_f64);
    let delta = p.get("delta_db").and_then(Value::as_f64);
    if abs.is_none() && delta.is_none() {
        return Err(bad("clip.gain", "`db` or `delta_db` required"));
    }
    let s = e.session_mut();
    for id in &ids {
        if let Some(c) = s.find_clip_mut(*id) {
            let v = abs.unwrap_or(f64::from(c.gain_db) + delta.unwrap_or(0.0));
            c.gain_db = if v.is_finite() { (v as f32).clamp(-144.0, 36.0) } else { c.gain_db };
        }
    }
    Ok(json!({"clips": ids.len()}))
}

fn nudge_gain(e: &mut Engine, p: &Value, db: f64) -> Result<Value> {
    let clips = clip_ids_param(e, p);
    gain(e, &json!({"clips": clips, "delta_db": db}))
}

fn quantize_to_grid(e: &mut Engine, p: &Value) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    let s = e.session();
    let moves: Vec<(soundcraft_model::ClipId, i64)> =
        ids.iter().filter_map(|id| s.find_clip(*id).map(|(_, c)| (*id, crate::edit::grid_delta(s, c, 0, false)))).collect();
    let s = e.session_mut();
    for (id, d) in &moves {
        crate::edit::move_clips(s, &[*id], *d, &[]);
    }
    Ok(json!({"clips": moves.len()}))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use soundcraft_model::{ClipId, TrackId};

    #[test]
    fn elastic_warp_renders_and_remove_warp_restores() {
        let mut e = crate::demo::demo_engine();
        let pad = e.session().track_by_name("Pad").map(|t| t.id.0).unwrap();
        e.execute("track.elastic", &json!({"track": pad, "algorithm": "polyphonic"})).unwrap();
        let cid = e.session().track(TrackId(pad)).unwrap().clips()[0].id.0;
        let len0 = e.session().find_clip(ClipId(cid)).unwrap().1.length;
        let r = e.execute("clip.elastic_properties", &json!({"clips": [cid], "ratio": 1.5})).unwrap();
        assert_eq!(r["rendered"], 1);
        let c = e.session().find_clip(ClipId(cid)).unwrap().1.clone();
        assert!((c.length as f64 - len0 as f64 * 1.5).abs() < 2.0, "{} vs {}", c.length, len0);
        assert_eq!(c.stretch, 1.0);
        // Re-warping starts from the original, not the rendered audio.
        e.execute("clip.elastic_properties", &json!({"clips": [cid], "ratio": 0.5})).unwrap();
        let c = e.session().find_clip(ClipId(cid)).unwrap().1.clone();
        assert!((c.length as f64 - len0 as f64 * 0.5).abs() < 2.0);
        e.execute("clip.remove_warp", &json!({"clips": [cid]})).unwrap();
        assert_eq!(e.session().find_clip(ClipId(cid)).unwrap().1.length, len0);
    }

    /// A clip on an Elastic track playing `clip_len` samples of a source that is silent before `tone_at`.
    fn elastic_clip(source_len: usize, tone_at: usize, clip_len: i64) -> (crate::Engine, TrackId, ClipId) {
        use soundcraft_model::{ChannelFormat, Clip, Session, TrackKind};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Elastic"));
        let samples = (0..source_len).map(|i| if i >= tone_at { (i as f32 * 0.2).sin() * 0.4 } else { 0.0 }).collect();
        let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![samples] };
        let src = crate::io::add_source(&mut s, "Tone", buf, None, soundcraft_audio_io::FileFormat::Wav);
        let cid = s.new_clip_id();
        crate::edit::place_clip(&mut s, t, Clip::audio(cid, "Clip", src, 0, 0, clip_len));
        let mut e = crate::Engine::new(s);
        e.execute("track.elastic", &json!({"track": t.0, "algorithm": "polyphonic"})).unwrap();
        (e, t, cid)
    }

    fn rendered_peak(e: &crate::Engine, t: TrackId, cid: ClipId) -> f32 {
        let range = e.session().find_clip(cid).unwrap().1.range();
        soundcraft_mix::render_clips(e.session(), t, range).iter().flatten().fold(0.0, |m, x| m.max(x.abs()))
    }

    /// Elastic Properties stretches the material the clip plays, by the requested ratio (#227).
    #[test]
    fn elastic_properties_stretches_the_clip_material() {
        let (mut e, t, cid) = elastic_clip(20_000, 10_000, 20_000);
        e.execute("clip.elastic_properties", &json!({"clips": [cid.0], "ratio": 2.0})).unwrap();
        assert_eq!(e.session().find_clip(cid).unwrap().1.length, 40_000);
        assert!(rendered_peak(&e, t, cid) > 0.3, "the tone in the second half was lost");

        let (mut e, t, cid) = elastic_clip(60_000, 20_000, 20_000);
        e.execute("clip.elastic_properties", &json!({"clips": [cid.0], "ratio": 0.5})).unwrap();
        assert_eq!(rendered_peak(&e, t, cid), 0.0, "audio from past the clip end leaked in");

        for (ratio, len) in [(4.0, 80_000), (0.2, 4_000)] {
            let (mut e, _, cid) = elastic_clip(20_000, 10_000, 20_000);
            e.execute("clip.elastic_properties", &json!({"clips": [cid.0], "ratio": ratio})).unwrap();
            assert_eq!(e.session().find_clip(cid).unwrap().1.length, len, "ratio {ratio}");
        }
    }

    #[test]
    fn remove_warp_keeps_the_audio_when_the_original_file_is_gone() {
        let mut e = crate::demo::demo_engine();
        let pad = e.session().track_by_name("Pad").map(|t| t.id).unwrap();
        e.execute("track.elastic", &json!({"track": pad, "algorithm": "polyphonic"})).unwrap();
        let clip = e.session().track(pad).unwrap().clips()[0].clone();
        e.execute("clip.elastic_properties", &json!({"clips": [clip.id], "ratio": 1.5})).unwrap();
        e.execute("clip.clear", &json!({"sources": [clip.source()]})).unwrap();
        let r = e.execute("clip.remove_warp", &json!({"clips": [clip.id]})).unwrap();
        assert_eq!(r["restored"], 0);
        let src = e.session().find_clip(clip.id).and_then(|(_, c)| c.source()).unwrap();
        assert!(e.session().source(src).is_some());
    }

    #[test]
    fn gain_nudges_one_db_at_a_time_up_to_the_ceiling() {
        let mut e = crate::demo::demo_engine();
        let kick = e.session().track_by_name("Kick").unwrap().clips()[0].id;
        let gain = |e: &crate::Engine| e.session().find_clip(kick).unwrap().1.gain_db;
        e.execute("edit.select", &json!({"clips": [kick.0]})).unwrap();
        e.execute("clip.gain_nudge_up", &json!({})).unwrap();
        e.execute("clip.gain_nudge_up", &json!({})).unwrap();
        e.execute("clip.gain_nudge_down", &json!({})).unwrap();
        assert_eq!(gain(&e), 1.0);
        e.execute("clip.gain", &json!({"db": 35.5})).unwrap();
        e.execute("clip.gain_nudge_up", &json!({})).unwrap();
        assert_eq!(gain(&e), 36.0);
    }
}
