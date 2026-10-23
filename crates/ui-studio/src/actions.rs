//! What the arrangement's gestures and shortcuts do, as engine commands. Nothing here changes
//! the session except through `Engine::execute` (and the cursor, see `StudioApp::set_cursor`).

use crate::StudioApp;
use serde_json::{Value, json};
use soundcraft_model::{ClipId, Route, Track, TrackId, TrackKind};
use soundcraft_time::{Range, Samples};

impl StudioApp {
    pub(crate) fn width_change_disabled_reason(&self) -> Option<&'static str> {
        self.engine.transport.recording.then_some("Stop recording before changing a track's channel width.")
    }

    pub(crate) fn history_disabled_reason(&self) -> Option<&'static str> {
        self.engine.transport.recording.then_some("Stop recording before using Undo or Redo.")
    }

    /// Preflight the input before changing either the width or the route. The menu uses the
    /// same check, including when an input selected on a previous device is now unavailable.
    pub(crate) fn width_input(&self, track: &Track, format: &str) -> Result<Option<String>, String> {
        if track.kind != TrackKind::Audio {
            return Ok(None);
        }
        let input = match &track.mixer.input {
            Route::Hardware(input) => input.as_str(),
            Route::None => "In 1",
            _ => return Ok(None),
        };
        let Some(first) = input.strip_prefix("In ").and_then(|v| v.split('-').next()).and_then(|n| n.parse::<usize>().ok()).filter(|n| *n > 0) else {
            return Ok(None);
        };
        let mapped = if format == "mono" {
            if first > self.device_inputs() {
                return Err(format!("Input {first} is unavailable on this device; pick an available input first"));
            }
            format!("In {first}")
        } else {
            let start = first - (first - 1) % 2;
            let last = start
                .checked_add(1)
                .filter(|last| *last <= self.device_inputs())
                .ok_or_else(|| format!("Input {first} has no stereo pair on this device; pick a stereo input first"))?;
            format!("In {start}-{last}")
        };
        if mapped == input {
            return Ok(None);
        }
        // `track.input` resolves bus names first; never turn hardware into a bus.
        if self.engine.session().bus_by_name(&mapped).is_some() {
            return Err("the hardware input name matches a bus; rename that bus first".into());
        }
        Ok(Some(mapped))
    }

    /// Keep hardware capture/monitoring and its label consistent with the new width, as one
    /// undo step. Called by `run` so menu and control-channel invocations get the same checks.
    pub(crate) fn change_track_width(&mut self, params: &Value) -> soundcraft_engine::Result<Value> {
        use soundcraft_engine::cmd::{bad, tracks_required};
        const COMMAND: &str = "track.change_width";
        if let Some(reason) = self.width_change_disabled_reason() {
            return Err(bad(COMMAND, reason));
        }
        let Some(format @ ("mono" | "stereo")) = params.get("format").and_then(Value::as_str) else {
            return self.engine.execute(COMMAND, params);
        };
        let tracks = tracks_required(&self.engine, COMMAND, params)?;
        let s = self.engine.session();
        let mut inputs = Vec::new();
        for id in tracks {
            let Some(t) = s.track(id) else { continue };
            if let Some(mapped) = self.width_input(t, format).map_err(|reason| bad(COMMAND, reason))? {
                inputs.push((id, mapped));
            }
        }
        self.engine.end_merge();
        let result = (|| {
            let r = self.engine.execute_merged(COMMAND, params, COMMAND)?;
            for (id, input) in inputs {
                self.engine.execute_merged("track.input", &json!({"tracks": [id.0], "input": input, "create_bus": false}), COMMAND)?;
            }
            Ok(r)
        })();
        self.engine.end_merge();
        result
    }

    /// The selected clips and their tracks, in session order.
    pub fn selected_clips(&self) -> Vec<(TrackId, ClipId, Range)> {
        let s = self.engine.session();
        let sel = &s.edit.selected_clips;
        let mut out = Vec::new();
        for t in &s.tracks {
            for c in t.clips() {
                if sel.contains(&c.id) {
                    out.push((t.id, c.id, Range::new(c.start, c.end())));
                }
            }
        }
        out
    }

    fn clip_ids(&self) -> Vec<ClipId> {
        self.engine.session().tracks.iter().flat_map(|t| t.clips().iter().map(|c| c.id)).collect()
    }

    /// Click on a clip: select it (`additive` adds it to or removes it from the selection).
    pub fn click_clip(&mut self, clip: ClipId, additive: bool) {
        self.auto.selection.clear();
        if let Some((t, _)) = self.engine.session().find_clip(clip) {
            self.ui.focus_track = Some(t);
        }
        let mut clips: Vec<u64> = if additive { self.engine.session().edit.selected_clips.iter().map(|c| c.0).collect() } else { Vec::new() };
        if additive && clips.contains(&clip.0) {
            clips.retain(|c| *c != clip.0);
        } else {
            clips.push(clip.0);
        }
        if clips.is_empty() {
            let _ = self.run("edit.select_none", json!({}));
        } else {
            let _ = self.run("edit.select", json!({"clips": clips}));
        }
    }

    /// Ctrl+A in the arrangement: every clip. Tracks stay unselected, so Delete takes clips only.
    pub fn select_all_clips(&mut self) {
        let clips: Vec<u64> = self.clip_ids().into_iter().map(|c| c.0).collect();
        self.tracks_selected_by_header = false;
        if clips.is_empty() {
            let _ = self.run("edit.select_none", json!({}));
        } else {
            let _ = self.run("edit.select", json!({"clips": clips}));
        }
    }

    /// Click on empty timeline: deselect the clips, select the track and put the cursor there.
    /// While playing, the playhead jumps there too (Ableton).
    pub fn click_empty(&mut self, track: Option<TrackId>, at: Samples) {
        self.auto.selection.clear();
        let at = at.max(0);
        let params = match track {
            Some(t) => json!({"tracks": [t.0], "start": at}),
            None => json!({"start": at}),
        };
        let _ = self.run("edit.select", params);
        if track.is_some() {
            self.ui.focus_track = track;
        }
        self.set_cursor(at, track);
        self.locate_playing(at);
    }

    /// Click on the ruler: the cursor moves (and a running playhead), the selection stays.
    pub fn click_ruler(&mut self, at: Samples) {
        let at = at.max(0);
        self.set_cursor(at, None);
        self.locate_playing(at);
    }

    /// Move `clips` by `by` samples, the first one's track to `to_track` when given.
    pub fn move_clips(&mut self, clips: &[ClipId], by: Samples, to_track: Option<TrackId>) -> Result<Value, String> {
        let ids: Vec<u64> = clips.iter().map(|c| c.0).collect();
        let mut p = json!({"clips": ids, "by": by});
        if let Some(t) = to_track {
            p["track"] = json!(t.0);
        }
        self.run("edit.move_clips", p)
    }

    /// Alt-drag: a copy of `clip` lands at `at` on `to_track`; the original stays and the copy is
    /// selected. Built from Copy and Paste (one undo step: Copy has none); the clipboard is
    /// restored afterwards so the gesture doesn't replace what the user copied.
    pub fn copy_clip_to(&mut self, clip: ClipId, at: Samples, to_track: TrackId) -> Result<Value, String> {
        let Some((from, range)) = self.engine.session().find_clip(clip).map(|(t, c)| (t, Range::new(c.start, c.end()))) else {
            return Err("no such clip".into());
        };
        let target = Range::new(at.max(0), at.max(0).saturating_add((range.end - range.start).max(1)));
        if from == to_track && range.start < target.end && target.start < range.end {
            self.status = "Copy would land on the original clip and replace part of it: drag it further.".into();
            return Err(self.status.clone());
        }
        let before = self.clip_ids();
        let clipboard = self.engine.clipboard.clone();
        let copied = self.run("edit.copy", json!({"tracks": [from.0], "start": range.start, "end": range.end}));
        let pasted = copied.and_then(|_| self.run("edit.paste", json!({"tracks": [to_track.0], "at": at.max(0)})));
        self.engine.clipboard = clipboard;
        let pasted = pasted?;
        let new: Vec<u64> = self.clip_ids().into_iter().filter(|c| !before.contains(c)).map(|c| c.0).collect();
        if !new.is_empty() {
            let _ = self.run("edit.select", json!({"clips": new}));
        }
        Ok(pasted)
    }

    /// Trim one edge of a clip to `to`.
    pub fn trim_clip(&mut self, clip: ClipId, start_edge: bool, to: Samples) -> Result<Value, String> {
        let edge = if start_edge { "start" } else { "end" };
        self.run("edit.trim_clip", json!({"clip": clip.0, edge: to.max(0)}))
    }

    /// Split at the cursor: the selected clips under the cursor, or else every clip under the
    /// cursor on the cursor's track.
    pub fn split_at_cursor(&mut self) -> Result<Value, String> {
        let at = self.cursor();
        let mut tracks: Vec<u64> = Vec::new();
        for (t, _, r) in self.selected_clips() {
            if r.start < at && at < r.end && !tracks.contains(&t.0) {
                tracks.push(t.0);
            }
        }
        if tracks.is_empty() {
            let Some(t) = self.cursor_track() else {
                self.status = "Split: put the cursor on a clip first.".into();
                return Err(self.status.clone());
            };
            tracks.push(t.0);
        }
        self.run("edit.separate", json!({"tracks": tracks, "at": at}))
    }

    /// Delete the selected clips (only them, not the time range they cover). With no clips
    /// selected, Delete takes the selected tracks, but only when their headers were what the
    /// user selected (Ableton); anything else does nothing.
    pub fn delete_selected(&mut self) -> Result<Value, String> {
        let sel = self.selected_clips();
        if sel.is_empty() {
            let s = self.engine.session();
            let tracks: Vec<TrackId> =
                s.edit.selected_tracks.iter().copied().filter(|t| s.track(*t).is_some_and(|t| t.kind != TrackKind::Master)).collect();
            if !self.tracks_selected_by_header || tracks.is_empty() {
                self.status = "Select a track header to delete the track".into();
                return Err(self.status.clone());
            }
            return self.request_delete_tracks(tracks);
        }
        self.tracks_selected_by_header = false;
        let mut tracks: Vec<u64> = sel.iter().map(|(t, _, _)| t.0).collect();
        tracks.dedup();
        let clips: Vec<u64> = sel.iter().map(|(_, c, _)| c.0).collect();
        // An empty range with clips named removes exactly those clips (`cmd/edit.rs` `clear`).
        self.run("edit.clear", json!({"tracks": tracks, "clips": clips, "start": 0, "end": 0}))
    }

    /// Delete `tracks` now when that is one empty track; otherwise ask first ("Delete 3 tracks
    /// and their clips?"). The master is never deleted.
    pub fn request_delete_tracks(&mut self, tracks: Vec<TrackId>) -> Result<Value, String> {
        let s = self.engine.session_arc();
        let tracks: Vec<TrackId> = tracks.into_iter().filter(|t| s.track(*t).is_some_and(|t| t.kind != TrackKind::Master)).collect();
        if tracks.is_empty() {
            self.status = "The master track can't be deleted".into();
            return Err(self.status.clone());
        }
        let clips = tracks.iter().any(|t| s.track(*t).is_some_and(|t| !t.clips().is_empty()));
        let group = tracks.iter().any(|t| s.track(*t).is_some_and(|t| t.is_folder()));
        if tracks.len() > 1 || clips || group {
            self.dialog = Some(crate::Dialog::DeleteTracks { tracks, clips });
            return Ok(json!({"confirm": true}));
        }
        self.delete_tracks(&tracks)
    }

    /// Delete one track (see [`Self::delete_tracks`]).
    pub fn delete_track(&mut self, t: TrackId) -> Result<Value, String> {
        self.delete_tracks(&[t])
    }

    /// Delete tracks as one undo step. An FX channel (an aux track listening to a bus) takes its
    /// bus with it when nothing else listens to or feeds the bus, and its senders' sends to it go
    /// first. The first error stops the batch, undoes it and goes to the status line. The master
    /// is never deleted.
    pub fn delete_tracks(&mut self, tracks: &[TrackId]) -> Result<Value, String> {
        self.tracks_selected_by_header = false;
        self.mixer.serial += 1;
        let key = format!("delete-tracks:{}", self.mixer.serial);
        let before = self.engine.session_arc();
        self.engine.end_merge();
        let mut notes: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        let mut last = json!({});
        let mut failed: Option<String> = None;
        // Groups take the Ungroup path: members stay, the bus goes when nothing else uses it.
        let groups: Vec<TrackId> = tracks.iter().copied().filter(|t| before.track(*t).is_some_and(|t| t.is_folder())).collect();
        if !groups.is_empty() {
            match self.dissolve(&groups, &key) {
                Ok((v, rerouted)) => {
                    last = v;
                    for g in &groups {
                        names.push(before.track(*g).map_or_else(String::new, |t| t.name.clone()));
                    }
                    if groups.len() == 1 && !rerouted.is_empty() {
                        notes.push(format!("Deleted {}{}", names[0], crate::groups::reroute_note(&rerouted)));
                    }
                    self.ui.collapsed.retain(|x| !groups.contains(x));
                }
                Err(e) => failed = Some(format!("Delete group: {e}")),
            }
        }
        for t in tracks.iter().filter(|t| !groups.contains(t)) {
            if failed.is_some() {
                break;
            }
            match self.delete_track_in(*t, &key) {
                Ok((v, name, note)) => {
                    last = v;
                    names.push(name);
                    notes.extend(note);
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        self.engine.end_merge();
        if let Some(e) = failed {
            if !std::sync::Arc::ptr_eq(&before, &self.engine.session_arc()) {
                self.engine.undo();
            }
            self.status = e;
            return Err(self.status.clone());
        }
        self.status = match (names.len(), notes.first()) {
            (1, Some(n)) => n.clone(),
            (1, None) => format!("Deleted {}", names[0]),
            (n, _) => format!("Deleted {n} tracks"),
        };
        Ok(last)
    }

    /// One track's deletion under `key`: (result, name, a status note for the special cases).
    fn delete_track_in(&mut self, t: TrackId, key: &str) -> Result<(Value, String, Option<String>), String> {
        let s = self.engine.session_arc();
        let Some(track) = s.track(t) else { return Err("Delete track: no such track".into()) };
        let name = track.name.clone();
        if track.kind == TrackKind::Master {
            return Err("The master track can't be deleted".into());
        }
        let fail = |e: soundcraft_engine::EngineError| format!("Delete {name}: {e}");
        let bus = match (&track.kind, &track.mixer.input) {
            (TrackKind::Aux, Route::Bus(b)) => Some(*b),
            _ => None,
        };
        let Some(bus) = bus else {
            let folder_bus = match (&track.kind, &track.mixer.input) {
                (TrackKind::Folder, Route::Bus(b)) => Some(*b),
                _ => None,
            };
            let r = self.engine.execute_merged("track.delete", &json!({"tracks": [t.0]}), key).map_err(fail)?;
            let note = folder_bus
                .filter(|b| crate::mixer::orphan_busses(&self.engine.session_arc()).contains(b))
                .map(|_| format!("Deleted {name}; its bus stays, remove it with Remove unused buses"));
            return Ok((r, name, note));
        };
        let (listeners, feeders, senders) = s.bus_usage(bus);
        if listeners != [t] || !feeders.is_empty() {
            let r = self.engine.execute_merged("track.delete", &json!({"tracks": [t.0]}), key).map_err(fail)?;
            let note = format!("Deleted {name}; its bus stays, other tracks use it");
            return Ok((r, name, Some(note)));
        }
        let mut sends: Vec<(u64, usize)> = Vec::new();
        for id in &senders {
            for (slot, x) in s.track(*id).map(|x| x.mixer.sends.iter().enumerate()).into_iter().flatten() {
                if x.as_ref().is_some_and(|x| x.target == Route::Bus(bus)) {
                    sends.push((id.0, slot));
                }
            }
        }
        let e = &mut self.engine;
        for (id, slot) in &sends {
            e.execute_merged("mix.send_remove", &json!({"track": id, "slot": slot}), key).map_err(fail)?;
        }
        let r = e.execute_merged("track.delete", &json!({"tracks": [t.0]}), key).map_err(fail)?;
        e.execute_merged("mix.remove_bus", &json!({"bus": bus.0}), key).map_err(fail)?;
        let note = match sends.len() {
            0 => format!("Removed {name}"),
            1 => format!("Removed {name} and its send"),
            n => format!("Removed {name} and its {n} sends"),
        };
        Ok((r, name, Some(note)))
    }

    /// Remove every bus nobody listens to, as one undo step.
    pub fn remove_unused_busses(&mut self) -> Result<Value, String> {
        let s = self.engine.session_arc();
        let orphans = crate::mixer::orphan_busses(&s);
        if orphans.is_empty() {
            self.status = "No unused buses".into();
            return Ok(json!({"removed": 0}));
        }
        self.mixer.serial += 1;
        let key = format!("remove-unused:{}", self.mixer.serial);
        self.engine.end_merge();
        let (mut sends, mut outputs) = (0, 0);
        let r = (|| {
            for b in &orphans {
                // `force`: a bus nobody hears may still have sends and outputs; they are cleared.
                let r = self.engine.execute_merged("mix.remove_bus", &json!({"bus": b.0, "force": true}), &key)?;
                let n = |k: &str| r["cleared"][k].as_array().map_or(0, Vec::len);
                sends += n("senders");
                outputs += n("feeders");
            }
            Ok::<(), soundcraft_engine::EngineError>(())
        })();
        self.engine.end_merge();
        match r {
            Ok(()) => {
                let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
                let mut cleared = Vec::new();
                if sends > 0 {
                    cleared.push(plural(sends, "send"));
                }
                if outputs > 0 {
                    cleared.push(plural(outputs, "output"));
                }
                let tail = if cleared.is_empty() { String::new() } else { format!(" (cleared {})", cleared.join(", ")) };
                let buses = if orphans.len() == 1 { "1 unused bus".to_string() } else { format!("{} unused buses", orphans.len()) };
                self.status = format!("Removed {buses}{tail}");
                Ok(json!({"removed": orphans.len()}))
            }
            Err(e) => {
                if !std::sync::Arc::ptr_eq(&s, &self.engine.session_arc()) {
                    self.engine.undo();
                }
                self.status = format!("Remove unused buses: {e}");
                Err(self.status.clone())
            }
        }
    }

    /// The tracks and range covering the selected clips.
    fn selection_span(&self) -> Option<(Vec<u64>, Range)> {
        let sel = self.selected_clips();
        let first = sel.first()?;
        let mut range = first.2;
        let mut tracks = Vec::new();
        for (t, _, r) in &sel {
            range = Range::new(range.start.min(r.start), range.end.max(r.end));
            if !tracks.contains(&t.0) {
                tracks.push(t.0);
            }
        }
        Some((tracks, range))
    }

    /// Copy the selected clips.
    pub fn copy_selected(&mut self, cut: bool) -> Result<Value, String> {
        let Some((tracks, range)) = self.selection_span() else {
            self.status = "Nothing selected to copy.".into();
            return Err(self.status.clone());
        };
        let r = self.run(if cut { "edit.cut" } else { "edit.copy" }, json!({"tracks": tracks, "start": range.start, "end": range.end}))?;
        self.copied = if cut { Vec::new() } else { tracks.iter().map(|t| (TrackId(*t), range)).collect() };
        Ok(r)
    }

    /// Paste at the cursor, on the cursor's track (and the ones below it for a multi-track copy).
    /// Never onto the copied clips themselves: Paste there would silently replace the original
    /// (UX gap 2026-10-18, plan 06), so it is refused with a hint.
    pub fn paste_at_cursor(&mut self) -> Result<Value, String> {
        let at = self.cursor();
        let len = self.engine.clipboard.length;
        let lanes = self.engine.clipboard.tracks.len().max(1);
        let s = self.engine.session();
        let first = self.cursor_track().or_else(|| s.edit.selected_tracks.first().copied()).or_else(|| s.tracks.first().map(|t| t.id));
        let Some(first) = first else {
            self.status = "Paste: add a track first.".into();
            return Err(self.status.clone());
        };
        let start = s.tracks.iter().position(|t| t.id == first).unwrap_or(0);
        let tracks: Vec<TrackId> = s.tracks.iter().skip(start).filter(|t| t.kind.has_playlist()).take(lanes).map(|t| t.id).collect();
        let target = Range::new(at, at.saturating_add(len.max(1)));
        if self.copied.iter().any(|(t, r)| tracks.contains(t) && r.start < target.end && target.start < r.end) {
            self.status = "Paste would land on the copied clip and replace it: click where it should go first.".into();
            return Err(self.status.clone());
        }
        let before = self.clip_ids();
        let ids: Vec<u64> = tracks.iter().map(|t| t.0).collect();
        let r = self.run("edit.paste", json!({"tracks": ids, "at": at}))?;
        let new: Vec<u64> = self.clip_ids().into_iter().filter(|c| !before.contains(c)).map(|c| c.0).collect();
        if !new.is_empty() {
            let _ = self.run("edit.select", json!({"clips": new}));
        }
        Ok(r)
    }

    /// Duplicate the selected clips right after themselves (Ctrl+D).
    pub fn duplicate_selected(&mut self) -> Result<Value, String> {
        let Some((tracks, range)) = self.selection_span() else { return Ok(json!({})) };
        let before = self.clip_ids();
        let r = self.run("edit.duplicate", json!({"tracks": tracks, "start": range.start, "end": range.end}))?;
        let new: Vec<u64> = self.clip_ids().into_iter().filter(|c| !before.contains(c)).map(|c| c.0).collect();
        if !new.is_empty() {
            let _ = self.run("edit.select", json!({"clips": new}));
        }
        Ok(r)
    }

    /// Add a track (Add Track): `audio`, `instrument` or `midi` (a bare MIDI track with no
    /// instrument; it can feed an external port). It becomes the cursor's track. An audio
    /// track is mono, the engine's default and the common case (a guitar or a mic).
    pub fn add_track(&mut self, kind: &str) -> Result<Value, String> {
        if kind == "audio" {
            return self.add_audio_track("mono");
        }
        self.add_track_with(json!({"kind": kind}))
    }

    /// Add an audio track of `format` (`mono`, `stereo`, ...). A mono track reads "In 1", a stereo
    /// one "In 1-2" (`mixer::route_label`).
    pub fn add_audio_track(&mut self, format: &str) -> Result<Value, String> {
        self.add_track_with(json!({"kind": "audio", "format": format}))
    }

    pub(crate) fn add_track_with(&mut self, p: Value) -> Result<Value, String> {
        let r = self.run("track.new", p)?;
        if let Some(id) = r.get("tracks").and_then(|t| t.get(0)).and_then(Value::as_u64) {
            let at = self.cursor();
            self.set_cursor(at, Some(TrackId(id)));
            self.ui.focus_track = Some(TrackId(id));
        }
        Ok(r)
    }

    /// A one-bar MIDI clip at the cursor on the cursor's track (MIDI or instrument tracks).
    pub fn new_midi_clip(&mut self, track: Option<TrackId>) -> Result<Value, String> {
        let s = self.engine.session();
        let Some(t) = track.or_else(|| self.cursor_track()).and_then(|t| s.track(t)).filter(|t| t.kind.is_midi()).map(|t| t.id) else {
            self.status = "New MIDI clip: click on an instrument track first.".into();
            return Err(self.status.clone());
        };
        let at = self.cursor();
        self.run("midi.new_clip", json!({"tracks": [t.0], "start": at, "end": at}))
    }

    /// Whether the track can hold clips (not a master, aux or VCA fader).
    pub fn holds_clips(&self, t: TrackId) -> bool {
        self.engine.session().track(t).is_some_and(|t| t.kind.has_playlist() && t.kind != TrackKind::Video)
    }
}
