//! The host loop: transport requests, recording, plug-in editor edits and state capture,
//! autosave, device trouble. None of it is UI, but the engine leaves it to the front end
//! (plan 06, section 3, "the host problem").
//!
//! **Copied** from `crates/ui-egui/src/lib.rs` (`SoundApp`) at `transit` 2af868e (file last changed
//! 04e1fe8). Every engine fix SoundCraft makes there must be ported here until the loop moves into
//! an egui-free host crate. Carried fixes, by escape number: E3 (instruments sound while stopped:
//! the player is never stopped for that), E5 (quit order: `close_input` before the player is
//! dropped, see `apps/transit` `on_exit`), E7 (`Player::idle` every frame and a 30 Hz repaint while
//! an editor is open, see `StudioApp::logic`), E9/E10 (the input is taken from the player with
//! `Player::open_input`, which opens it inside `with_output_paused` on ASIO, and given back with
//! `Player::close_input`).
//!
//! Adapted, and marked where: play and record start at the cursor, not at the edit selection
//! (plan 06, L6); the stop-time insertion and scroll edits that read `soundcraft-ui-egui`'s view
//! state are left out; the automation write pass is deferred (plan 06, section 3).

use crate::{MeterDisplay, StudioApp};
use serde_json::json;
use soundcraft_engine::TransportRequest;
use soundcraft_model::TrackId;
use soundcraft_playback::record::Trouble;
use soundcraft_playback::{OutputTrouble, Player};
use soundcraft_time::{Range, Samples};

impl StudioApp {
    pub(crate) fn start_play(&mut self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.clear_clip_latches();
        if let Some(p) = &self.player {
            p.update_session(self.engine.session_arc());
            p.play(from, end, looped);
        } else {
            self.sim = Some((from, end, looped));
        }
        self.engine.transport.playing = true;
        self.engine.transport.position = from;
    }

    pub(crate) fn stop_play(&mut self) {
        if let Some(p) = &self.player {
            // Half-speed is a separate command. Stop must not leave that rate for the next Play.
            p.set_speed(1.0);
            p.stop();
        }
        if self.engine.transport.recording {
            self.finish_recording();
        }
        self.sim = None;
        self.engine.transport.playing = false;
        self.engine.transport.recording = false;
        // Adapted: the cursor stays where it was (Ableton), so Stop then Play starts the same
        // place again; the stopped position is the cursor.
        self.engine.transport.position = self.cursor();
    }

    /// Adapted from `SoundApp::start_recording`: without Cycle, recording starts at the cursor
    /// (with pre-roll when it is on) and runs until Stop; it does not punch into the selection,
    /// which in this UI is usually a clicked clip. With Cycle and Loop Record, it loop-records
    /// the selection as SoundCraft does.
    pub(crate) fn start_recording(&mut self) {
        let armed = self.engine.session().tracks.iter().any(|t| t.mixer.record_arm);
        if !armed {
            let why = if self.engine.session().tracks.is_empty() {
                "Nothing to record on yet: add a track, then click its R button."
            } else {
                "No track is record-enabled: click the R button on the track to record."
            };
            self.status = why.into();
            self.record_hint = Some((self.last_frame.unwrap_or_default(), why));
            return;
        }
        self.ensure_input();
        let takes = match self.arm_recorder() {
            Ok(takes) => takes,
            Err(err) => {
                self.status = format!("Cannot record: {err}");
                return;
            }
        };
        self.lost_shown = 0;
        if let Some(p) = &self.player {
            p.set_recording(true);
        }
        let already_playing = self.is_playing();
        self.engine.transport.recording = true;
        if already_playing {
            // Punch in on the fly at the current playhead.
            self.record_start = self.position();
            self.punch = None;
            self.plan_recording(takes, None);
            return;
        }
        let e = self.engine.session().edit.clone();
        let sel = e.selection;
        let cycle = e.loop_playback && !sel.is_empty();
        if cycle && e.loop_record {
            self.record_start = sel.start;
            self.punch = None;
            self.start_play(sel.start, None, Some(sel));
            self.plan_recording(takes, Some(sel.len()));
        } else if cycle {
            // Slice 6: punch. With Cycle on (and Loop Record off), record just the cycle range:
            // play from a count-in or pre-roll before it and stop at its end.
            let lead = self.lead_in(sel.start);
            self.record_start = (sel.start - lead).max(0);
            self.punch = Some(sel);
            self.start_play(self.record_start, Some(sel.end), None);
            self.plan_recording(takes, None);
        } else {
            let at = self.cursor();
            let from = (at - self.lead_in(at)).max(0);
            self.record_start = from;
            // A count-in is heard, not kept: the take starts at the cursor. (A pre-roll is kept,
            // as in SoundCraft.)
            self.punch = (self.ui.count_in && from < at).then(|| Range::new(at, at.saturating_add(i64::MAX / 4)));
            self.start_play(from, None, None);
            self.plan_recording(takes, None);
        }
    }

    /// How long Record plays before `at`: one bar with the count-in on, else the pre-roll when
    /// it is on. Never before the session start, so a count-in at bar 1 is cut short.
    pub(crate) fn lead_in(&self, at: Samples) -> Samples {
        let s = self.engine.session();
        if self.ui.count_in {
            let bar = s.tempo.bar_beat_at(at.max(0), s.sample_rate).bar;
            let start = |b: i64| s.tempo.samples_at_bar_beat(soundcraft_time::BarBeat { bar: b, beat: 1, tick: 0 }, s.sample_rate);
            return (start(bar + 1) - start(bar)).max(1);
        }
        if s.edit.pre_post_roll { s.edit.pre_roll } else { 0 }
    }

    /// Remember where the armed `takes` go (split into passes of `pass` when loop recording) and
    /// autosave that plan with the session.
    fn plan_recording(&mut self, takes: Vec<soundcraft_engine::io::Take>, pass: Option<Samples>) {
        let rate = self.recorder.as_ref().map_or(0, |r| r.sample_rate);
        self.take_plan = Some(soundcraft_engine::io::Recording { start: self.record_start, pass, punch: self.punch, rate, takes });
        if let Some(root) = self.autosave_dir.clone() {
            self.queue_autosave(root);
        }
    }

    /// Arm the input for the record-enabled tracks; a session never saved records into its
    /// autosave folder. Returns the takes it records.
    fn arm_recorder(&mut self) -> Result<Vec<soundcraft_engine::io::Take>, String> {
        let name = soundcraft_engine::io::sanitize_name(&self.engine.session().name);
        let unsaved_dir = self.autosave_dir.as_ref().map_or_else(std::env::temp_dir, |d| d.join(name));
        let Some(r) = &mut self.recorder else { return Err("no audio input device".into()) };
        let plan = soundcraft_engine::io::plan_takes(&self.engine, &unsaved_dir, r.channels);
        if plan.is_empty() {
            return Err("no audio track is record-enabled".into());
        }
        let takes = plan
            .iter()
            .map(|(track, inputs, path)| soundcraft_engine::io::Take { track: *track, path: path.clone(), channels: inputs.len() })
            .collect();
        r.arm(plan)?;
        Ok(takes)
    }

    /// Reads every live third-party plugin's state and stores it on its insert
    /// (`mix.insert_state`), so saves, bounces and autosaves include it.
    pub fn capture_plugin_states(&mut self) {
        let Some(p) = &self.player else { return };
        for (t, slot, data) in p.capture_states() {
            let slot = if slot == soundcraft_mix::INSTRUMENT_SLOT { json!("instrument") } else { json!(slot) };
            let state = soundcraft_model::b64::encode(&data);
            if let Err(e) = self.engine.execute("mix.insert_state", &json!({"track": t.0, "slot": slot, "state": state})) {
                log::warn!("storing plugin state: {e}");
            }
        }
    }

    /// Parameter edits made in plugin editors go into the session (one undo step per gesture).
    /// Calls `Player::idle`, which hosted editors need every frame (E7).
    pub(crate) fn apply_editor_edits(&mut self) {
        let Some(p) = &self.player else { return };
        for (t, slot, param, value) in p.idle() {
            if slot == soundcraft_mix::INSTRUMENT_SLOT {
                continue;
            }
            let key = format!("param:{}:{slot}:{param}", t.0);
            let _ = self.engine.execute_merged("mix.insert_param", &json!({"track": t.0, "slot": slot, "param": param, "value": value}), &key);
        }
    }

    /// Every two minutes, copy a modified session to the autosave folder (not while recording).
    /// Timed by the wall clock: egui's stands still while the window is hidden.
    pub(crate) fn autosave(&mut self) {
        let Some(dir) = self.autosave_dir.clone() else { return };
        let last = *self.last_autosave.get_or_insert_with(std::time::Instant::now);
        if last.elapsed().as_secs() < 120 || !self.engine.is_dirty() || self.engine.transport.recording {
            return;
        }
        self.last_autosave = Some(std::time::Instant::now());
        self.capture_plugin_states();
        self.autosave_to(&dir);
    }

    /// Autosave a modified session within a second of every change, also while recording. The
    /// plugins' own state reaches the session every two minutes (`autosave`).
    pub(crate) fn autosave_changes(&mut self) {
        if let Some(e) = self.autosaver.as_ref().and_then(soundcraft_engine::autosave::Autosaver::failure) {
            self.status = format!("Autosave failed: {e}");
        }
        if self.autosave_dir.is_none() || self.engine.revision == self.autosaved_rev {
            return;
        }
        self.autosaved_rev = self.engine.revision;
        if !self.engine.is_dirty() {
            self.release_autosave(false);
        } else if let Some(root) = self.autosave_dir.clone() {
            self.queue_autosave(root);
        }
    }

    fn queue_autosave(&mut self, root: std::path::PathBuf) {
        self.autosaved_rev = self.engine.revision;
        let w = self.autosaver.get_or_insert_with(|| soundcraft_engine::autosave::Autosaver::new(root));
        w.write(self.engine.session_arc(), self.engine.path.clone(), self.take_plan.clone());
    }

    fn autosave_to(&mut self, root: &std::path::Path) -> bool {
        self.queue_autosave(root.to_path_buf());
        let Some(w) = &self.autosaver else { return false };
        let written = w.flush();
        if let Err(e) = &written {
            self.status = format!("Autosave failed: {e}");
        }
        written.is_ok()
    }

    fn write_autosave(&mut self) -> bool {
        self.autosave_dir.clone().is_some_and(|root| self.autosave_to(&root))
    }

    pub(crate) fn release_autosave(&mut self, keep: bool) {
        if let Some(w) = &self.autosaver {
            w.close(keep);
        }
    }

    /// Done with the open session. Unsaved changes stay in its autosave folder for the next
    /// launch, unless the user quit with Quit; a take still recording is finished and always stays.
    pub fn close_autosave(&mut self) {
        let recording = self.engine.transport.recording;
        if recording {
            self.stop_play();
        }
        let keep = self.engine.is_dirty() && (recording || !self.quit_requested);
        if keep {
            self.capture_plugin_states();
            self.write_autosave();
        }
        self.release_autosave(keep);
        if let Some(w) = &self.autosaver {
            let _ = w.flush();
        }
    }

    /// Run part of a frame. If it panics, the session is autosaved (once per change) and the app
    /// carries on instead of quitting.
    pub fn guarded(&mut self, f: impl FnOnce(&mut StudioApp)) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))).is_ok() || self.panic_rev == self.engine.revision {
            return;
        }
        self.panic_rev = self.engine.revision;
        let saved = self.engine.is_dirty() && self.write_autosave();
        self.status = if saved {
            "Internal error (see the log). Your changes were autosaved: save them, then restart.".into()
        } else {
            "Internal error (see the log). Restart to be safe.".into()
        };
    }

    /// Replace the open session with unsaved work an earlier run left behind.
    pub fn recover(&mut self, left: &soundcraft_engine::autosave::Leftover) {
        if self.engine.transport.recording {
            self.status = "Stop recording before recovering other work.".into();
            return;
        }
        self.close_autosave();
        match soundcraft_engine::autosave::recover(&mut self.engine, left) {
            Ok(missing) => {
                if self.write_autosave() {
                    soundcraft_engine::autosave::discard(left);
                }
                self.status = match missing.len() {
                    0 => format!("Recovered “{}”. Save it to keep the changes.", left.name),
                    n => format!("Recovered “{}”, but {n} audio file(s) are missing. Save it to keep the changes.", left.name),
                };
            }
            Err(e) => self.status = format!("Couldn't recover “{}”: {e}", left.name),
        }
    }

    fn meters_live(&self) -> bool {
        self.engine.transport.playing
            || (self.recorder.is_some() && self.engine.session().tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm))
    }

    /// Done with the input, if open: it goes back to the player (`Player::close_input`), which
    /// keeps an input it opened with the output (ASIO) and closes any other. Must run before the
    /// player is dropped (E5).
    pub fn close_input(&mut self) {
        let Some(r) = self.recorder.take() else { return };
        match &self.player {
            Some(p) => p.close_input(r),
            None => drop(r),
        }
    }

    /// Take the input once (for recording and input monitoring) from the player and feed the
    /// player: on ASIO the one it opened with the output, elsewhere a newly opened device.
    pub(crate) fn ensure_input(&mut self) {
        if self.recorder.is_some() || self.recorder_failed {
            return;
        }
        let Some(p) = &self.player else { return };
        match p.open_input(&self.ui.audio) {
            Ok(r) => {
                p.set_input(std::sync::Arc::clone(&r.monitor));
                self.recorder = Some(r);
            }
            Err(e) => {
                self.recorder_failed = true;
                self.status = format!("No audio input: {e}");
            }
        }
    }

    /// Stop the transport, keeping a take in progress, when the audio output fails.
    pub(crate) fn watch_output(&mut self) {
        let Some(p) = &self.player else { return };
        let Some(trouble) = p.trouble() else { return };
        let name = p.device_name.clone();
        let (cause, then) = match trouble {
            OutputTrouble::Bug => ("the audio engine hit an internal error (see the log)".to_string(), "Use Audio › Reconnect Device, or restart."),
            OutputTrouble::Gone => (format!("“{name}” was disconnected"), "Plug it back in, then use Audio › Reconnect Device."),
            OutputTrouble::Stalled => (format!("“{name}” stopped asking for audio"), "Use Audio › Reconnect Device."),
        };
        let (recording, playing) = (self.engine.transport.recording, self.engine.transport.playing);
        if recording || playing {
            self.stop_play();
        }
        self.status = if recording {
            format!("Recording stopped: {cause}. The take is kept up to there. {then}")
        } else if playing {
            format!("Playback stopped: {cause}. {then}")
        } else {
            format!("Audio output: {cause}. {then}")
        };
    }

    /// Stop recording, keeping the take, when the input device or the disk fails.
    pub(crate) fn watch_input(&mut self) {
        let recording = self.engine.transport.recording;
        let Some(r) = &mut self.recorder else { return };
        let Some(trouble) = r.trouble() else {
            let lost = r.lost_ms();
            if recording && lost > self.lost_shown {
                self.lost_shown = lost;
                self.status = format!("The disk is too slow: this take has lost {lost} ms of input so far.");
            }
            return;
        };
        if recording {
            self.stop_play();
        }
        let (what, then) = match trouble {
            Trouble::Device(what) => {
                self.close_input();
                self.recorder_failed = true;
                (what, "Reconnect it, then use Audio › Reconnect Device.")
            }
            Trouble::Disk(what) => (what, "Check the disk, then record again."),
        };
        self.status = if recording {
            format!("Recording stopped: {what}. The take is kept up to there. {then}")
        } else {
            format!("Audio input: {what}. {then}")
        };
    }

    /// Reopen the audio devices, keeping a take in progress.
    pub fn reconnect_audio(&mut self) {
        if self.is_playing() || self.engine.transport.recording {
            self.stop_play();
        }
        if let Some(p) = &self.player {
            // Its plugin editors go with it; closed here, on the main thread.
            p.close_editors();
        }
        self.close_input();
        self.recorder_failed = false;
        // The old device closes first: an ASIO driver is process-wide.
        self.player = None;
        self.player = Some(Player::with_config(self.engine.session_arc(), &self.ui.audio));
    }

    fn finish_recording(&mut self) {
        self.engine.transport.recording = false;
        if let Some(p) = &self.player {
            p.set_recording(false);
        }
        let plan = self.take_plan.take();
        let Some(r) = &mut self.recorder else { return };
        let (takes, problem) = r.finish();
        let tracks = takes.len().max(1);
        let plan = plan.unwrap_or_else(|| soundcraft_engine::io::Recording {
            start: self.record_start,
            pass: None,
            punch: self.punch,
            rate: 0,
            takes: Vec::new(),
        });
        self.punch = None;
        self.autosaved_rev = 0;
        let end = self.engine.transport.position;
        self.place_takes(&plan, takes, end, tracks);
        if let Some(p) = problem {
            log::warn!("recording: {p}");
            self.status = format!("{} ({p})", self.status);
        }
    }

    /// Slice 6: place finished take files. A take recorded over clips on its track goes on a new
    /// playlist that copies the old one (`track.playlist_duplicate`), so the old clips stay as the
    /// previous take (plan 06, L8: takes are track playlists). The new clips are selected and the
    /// status line says what was recorded where. `end` is where the playhead stopped.
    pub(crate) fn place_takes(
        &mut self,
        plan: &soundcraft_engine::io::Recording,
        takes: Vec<(TrackId, std::path::PathBuf)>,
        end: Samples,
        tracks: usize,
    ) {
        let from = plan.punch.map_or(plan.start, |p| p.start.max(plan.start));
        let to = plan.punch.map_or(end, |p| p.end.min(end));
        let mut new_take = Vec::new();
        if plan.pass.is_none() && to > from {
            for (t, _) in &takes {
                let over = self.engine.session().track(*t).is_some_and(|tr| tr.clips().iter().any(|c| c.start < to && c.end() > from));
                if over && self.run("track.playlist_duplicate", json!({"track": t.0})).is_ok() {
                    new_take.push(*t);
                }
            }
        }
        let ids = soundcraft_engine::io::place_recording(&mut self.engine, plan, takes);
        if plan.pass.is_some() {
            self.status = format!("Loop-recorded {} take(s)", ids.len() / tracks.max(1));
            return;
        }
        let s = self.engine.session_arc();
        let placed: Vec<(TrackId, &soundcraft_model::Clip)> = ids.iter().filter_map(|id| s.find_clip(*id)).collect();
        let Some(longest) = placed.iter().map(|(_, c)| c.length).max() else {
            self.status = "Recorded nothing".into();
            return;
        };
        let mut names: Vec<String> = Vec::new();
        for (t, _) in &placed {
            if let Some(tr) = s.track(*t)
                && !names.contains(&tr.name)
            {
                names.push(tr.name.clone());
            }
        }
        let secs = s.sample_rate.seconds(longest);
        let n = placed.len();
        self.status = format!("Recorded {n} clip{} ({secs:.1} s) on {}", if n == 1 { "" } else { "s" }, names.join(", "));
        if let [t] = new_take.as_slice()
            && let Some(tr) = s.track(*t)
        {
            self.status.push_str(&format!(" as take {}", tr.active_playlist + 1));
        }
        let clips: Vec<u64> = ids.iter().map(|c| c.0).collect();
        let _ = self.run("edit.select", json!({"clips": clips}));
        if let Some((t, _)) = placed.first() {
            self.ui.focus_track = Some(*t);
        }
    }

    /// Adapted from `SoundApp::play_from_selection`: play from the cursor; with Cycle on and a
    /// selection, loop the selection (the engine's loop range is the selection, plan 06 L6).
    pub fn play_from_cursor(&mut self) {
        let s = self.engine.session();
        let sel = s.edit.selection;
        if s.edit.loop_playback && !sel.is_empty() {
            self.start_play(sel.start, None, Some(sel));
        } else {
            let preroll = if s.edit.pre_post_roll { s.edit.pre_roll } else { 0 };
            let at = self.cursor();
            self.start_play((at - preroll).max(0), None, None);
        }
    }

    /// Move a running playhead (a click on the ruler or the timeline while playing).
    pub(crate) fn locate_playing(&mut self, at: Samples) {
        if !self.is_playing() {
            return;
        }
        if let Some(p) = &self.player {
            p.locate(at);
        }
        if let Some(sim) = &mut self.sim {
            sim.0 = at;
        }
        self.engine.transport.position = at;
    }

    pub(crate) fn handle_transport(&mut self, dt: f32) {
        let reqs: Vec<TransportRequest> = std::mem::take(&mut self.engine.transport_requests);
        for r in reqs {
            match r {
                TransportRequest::ResetSession => {
                    if let Some(p) = &self.player {
                        p.stop();
                        p.set_recording(false);
                        p.set_speed(1.0);
                        p.locate(self.engine.transport.position);
                    }
                    self.sim = None;
                    // The old capture belongs to the replaced document, never to the new one.
                    self.close_input();
                    self.recorder_failed = false;
                    self.punch = None;
                    self.arrange.gesture = None;
                    self.copied.clear();
                    self.meters.clear();
                    self.main_meter = MeterDisplay::default();
                    // The cursor is per-session view state.
                    self.ui.cursor = 0;
                    self.ui.cursor_track = None;
                    self.ui.focus_track = None;
                    self.mixer.gesture = None;
                    self.engine.transport.position = self.cursor();
                }
                TransportRequest::Play => {
                    if !self.is_playing() {
                        self.play_from_cursor();
                    }
                }
                TransportRequest::Stop | TransportRequest::Pause => self.stop_play(),
                TransportRequest::TogglePlay => {
                    if self.is_playing() {
                        self.stop_play();
                    } else {
                        self.play_from_cursor();
                    }
                }
                TransportRequest::Record => {
                    if self.engine.transport.recording {
                        self.stop_play();
                    } else {
                        self.start_recording();
                    }
                }
                TransportRequest::Locate(at) => {
                    // Adapted: a locate command (Return to Zero, `transport.locate`) also moves the cursor.
                    self.set_cursor(at, None);
                    self.locate_playing(at);
                    self.engine.transport.position = at;
                }
                TransportRequest::PlaySelection => {
                    let sel = self.engine.session().edit.selection;
                    self.start_play(sel.start, Some(sel.end.max(sel.start + 1)), None);
                }
                TransportRequest::HalfSpeed => {
                    if let Some(p) = &self.player {
                        p.set_speed(0.5);
                    }
                    self.play_from_cursor();
                }
                TransportRequest::Scrub(at) => self.engine.transport.position = at,
                TransportRequest::AllNotesOff => {
                    if let Some(p) = &self.player {
                        p.all_notes_off();
                    }
                    self.status = "All MIDI notes off".into();
                }
            }
        }
        // Read back the position.
        if let Some(p) = &self.player {
            let playing = p.is_playing();
            let ended = self.engine.transport.playing && !playing;
            if ended {
                p.set_speed(1.0);
            }
            if self.engine.transport.playing {
                self.engine.transport.position = p.position();
            }
            let now = self.last_frame.unwrap_or_default();
            if now - self.last_meter_poll >= 1.0 / 20.0 {
                p.copy_meters(&mut self.meter_snapshot);
                self.last_meter_poll = now;
            }
            let snap = &self.meter_snapshot;
            for (id, m) in &snap.tracks {
                let d = self.meters.entry(*id).or_default();
                feed_meter(d, m.peak, m.gain_reduction_db, dt);
                if m.plugin_failures.count != d.plugin_failures {
                    d.plugin_failures = m.plugin_failures.count;
                    if let Some(msg) = failed_plugin_status(self.engine.session(), *id, m.plugin_failures.last) {
                        self.status = msg;
                    }
                }
            }
            let main = snap.main.peak;
            feed_meter(&mut self.main_meter, main, 0.0, dt);
            if ended {
                self.stop_play();
            }
        } else if let Some((pos, end, looped)) = &mut self.sim {
            let sr = self.engine.session().sample_rate.as_f64();
            *pos += (f64::from(dt) * sr) as i64;
            if let Some(l) = looped
                && *pos >= l.end
            {
                *pos = l.start;
            }
            let stop = end.is_some_and(|e| *pos >= e);
            self.engine.transport.position = *pos;
            if stop {
                self.stop_play();
            }
        }
        if !self.meters_live() {
            for d in self.meters.values_mut() {
                feed_meter(d, [0.0, 0.0], 0.0, dt);
            }
            feed_meter(&mut self.main_meter, [0.0, 0.0], 0.0, dt);
        }
    }
}

/// Instant attack, ~26 dB/s release, 2 s peak hold. Copied from `soundcraft-ui-egui`.
pub fn feed_meter(d: &mut MeterDisplay, peak: [f32; 2], gr: f32, dt: f32) {
    let fall = 10f32.powf(-26.0 * dt / 20.0);
    for c in 0..2 {
        let v = peak.get(c).copied().unwrap_or(0.0);
        let lvl = d.level.get(c).copied().unwrap_or(0.0);
        let nv = if v >= lvl { v } else { (lvl * fall).max(v) };
        if let Some(x) = d.level.get_mut(c) {
            *x = if nv < 1e-5 { 0.0 } else { nv };
        }
        if v >= d.hold.get(c).copied().unwrap_or(0.0) {
            if let Some(h) = d.hold.get_mut(c) {
                *h = v;
            }
            d.hold_age = 0.0;
        }
        if v >= 1.0 {
            d.clip = true;
        }
        d.peak = d.peak.max(v);
    }
    d.hold_age += dt;
    if d.hold_age > 2.0 {
        d.hold = d.level;
    }
    d.gr = if gr > d.gr { gr } else { d.gr * fall };
}

/// The status line naming the plugin in `slot` that the engine took out of the signal path.
fn failed_plugin_status(s: &soundcraft_model::Session, track: TrackId, slot: usize) -> Option<String> {
    let t = s.track(track)?;
    let ins = if slot == soundcraft_mix::INSTRUMENT_SLOT { t.instrument.as_ref() } else { t.mixer.inserts.get(slot)?.as_ref() }?;
    Some(format!("{} on {} put out invalid audio, so it is bypassed until playback stops or starts again.", ins.plugin, t.name))
}
