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
//! state are left out. The automation write pass is in [`crate::automation::write_pass`] (slice 7).

use crate::{MeterDisplay, StudioApp};
use serde_json::json;
use soundcraft_engine::TransportRequest;
use soundcraft_model::TrackId;
use soundcraft_playback::record::Trouble;
use soundcraft_playback::{OutputTrouble, Player};
use soundcraft_time::{Range, Samples};

/// Said when Record is asked with the count-in on and the cursor sits at the start of the session.
const COUNT_IN_SKIPPED: &str = "Count-in skipped: no room before bar 1";

impl StudioApp {
    pub(crate) fn start_play(&mut self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.clear_clip_latches();
        if !self.sync_player_session() {
            return;
        }
        if let Some(p) = &self.player {
            let Some(sequence) = self.player_requests.result(&mut self.status, p.play(from, end, looped)) else { return };
            self.player_requests.play = Some(sequence);
            self.status = "Play pending".into();
        } else {
            self.sim = Some((from, end, looped));
            self.engine.transport.playing = true;
            self.engine.transport.position = from;
        }
    }

    pub(crate) fn stop_play(&mut self) {
        if let Some(p) = &self.player {
            // Half-speed is a separate command. Stop must not leave that rate for the next Play.
            p.set_speed(1.0);
            let stopped = self.player_requests.result(&mut self.status, p.stop());
            if let Some(sequence) = stopped {
                self.player_requests.stop = Some(sequence);
                self.player_requests.play = None;
                self.player_requests.recording = None;
                self.status = "Stop pending".into();
            }
            if self.recording_active() {
                self.finish_recording();
            }
            return;
        }
        self.complete_stop();
    }

    fn complete_stop(&mut self) {
        if self.recording_active() {
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
        self.engine.transport.recording = self.player.is_none();
        self.begin_pass(takes);
    }

    /// Start the recording pass for `takes`: on the fly at the playhead when already playing, else
    /// from the count-in or pre-roll, or the cycle range.
    pub(crate) fn begin_pass(&mut self, takes: Vec<soundcraft_engine::io::Take>) {
        let already_playing = self.is_playing();
        if already_playing {
            // Punch in on the fly at the current playhead.
            self.record_start = self.position();
            self.punch = None;
            self.plan_recording(takes, None, None);
            return;
        }
        let e = self.engine.session().edit.clone();
        let sel = e.cycle_range();
        let cycle = e.loop_playback && !sel.is_empty();
        if cycle && e.loop_record {
            self.record_start = sel.start;
            self.punch = None;
            self.plan_recording(takes, Some(sel.len()), Some((sel.start, None, Some(sel))));
        } else if cycle {
            // Slice 6: punch. With Cycle on (and Loop Record off), record just the cycle range:
            // play from a count-in or pre-roll before it and stop at its end.
            let lead = self.lead_in(sel.start);
            self.record_start = (sel.start - lead).max(0);
            self.punch = Some(sel);
            self.plan_recording(takes, None, Some((self.record_start, Some(sel.end), None)));
            if self.ui.count_in && self.record_start >= sel.start {
                self.status = COUNT_IN_SKIPPED.into();
            }
        } else {
            let at = self.cursor();
            let from = (at - self.lead_in(at)).max(0);
            self.record_start = from;
            // A count-in is heard, not kept: the take starts at the cursor. (A pre-roll is kept,
            // as in SoundCraft.)
            self.punch = (self.ui.count_in && from < at).then(|| Range::new(at, at.saturating_add(i64::MAX / 4)));
            self.plan_recording(takes, None, Some((from, None, None)));
            if self.ui.count_in && from >= at {
                self.status = COUNT_IN_SKIPPED.into();
            }
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
    fn plan_recording(
        &mut self,
        takes: Vec<soundcraft_engine::io::Take>,
        pass: Option<Samples>,
        play: Option<(Samples, Option<Samples>, Option<Range>)>,
    ) {
        let rate = self.recorder.as_ref().map_or(0, |r| r.sample_rate);
        self.take_plan = Some(soundcraft_engine::io::Recording { start: self.record_start, pass, punch: self.punch, rate, takes });
        if !self.sync_player_session() {
            self.finish_recording();
            return;
        }
        if let Some(p) = &self.player {
            let Some(recorder) = self.recorder.as_ref() else {
                self.finish_recording();
                return;
            };
            let (from, end, looped) = play.map_or((None, None, None), |(from, end, looped)| (Some(from), end, looped));
            let Some(sequence) = self.player_requests.result(&mut self.status, p.record(recorder, from, end, looped)) else {
                self.finish_recording();
                return;
            };
            self.player_requests.recording = Some(sequence);
            if play.is_some() {
                self.player_requests.play = Some(sequence);
            }
            self.status = "Record pending".into();
        } else if let Some((from, end, looped)) = play {
            self.start_play(from, end, looped);
        }
        if let Some(root) = self.autosave_dir.clone() {
            self.queue_autosave(root);
        }
    }

    /// Arm the input for the record-enabled tracks; a session never saved records into its
    /// autosave folder. Returns the takes it records.
    fn arm_recorder(&mut self) -> Result<Vec<soundcraft_engine::io::Take>, String> {
        if self.recorder_failed {
            return Err(self.status.clone());
        }
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
        if let Some(player) = &self.player {
            player.prepare_recording(r, plan)?;
        } else {
            r.arm(plan)?;
        }
        Ok(takes)
    }

    /// Synchronize the immutable snapshot, including view-only COW changes.
    /// Revision alone cannot establish the lineage required by state capture.
    pub(crate) fn sync_player_session(&mut self) -> bool {
        let Some(p) = &self.player else {
            self.restore_pending = false;
            self.last_rev = self.engine.revision;
            return true;
        };
        // ResetSession must first stop/close old-document transport and input.
        if self.engine.transport_requests.contains(&TransportRequest::ResetSession) {
            return false;
        }
        let session = self.engine.session_arc();
        if !self.restore_pending && self.last_rev == self.engine.revision && p.has_requested_session(&session) {
            return true;
        }
        let result = if self.restore_pending { p.replace_session(session) } else { p.update_session(session) };
        if matches!(result, Err(soundcraft_playback::PlayerError::NeedsReopen { .. })) {
            if let Err(error) = self.reconnect_audio_using(false, Player::with_config) {
                self.player_requests.fail(&mut self.status, error);
                return false;
            }
            if self.player.as_ref().is_some_and(|p| p.has_requested_session(&self.engine.session_arc())) {
                self.restore_pending = false;
                self.last_rev = self.engine.revision;
                return true;
            }
            return false;
        }
        if self.player_requests.result(&mut self.status, result).is_none() {
            return false;
        }
        self.restore_pending = false;
        self.last_rev = self.engine.revision;
        true
    }

    /// Reads every live third-party plugin's state and stores it on its insert
    /// (`mix.insert_state`), so saves, bounces and autosaves include it.
    pub fn capture_plugin_states(&mut self) -> Result<(), String> {
        // Saving must not supersede the prerequisite of an admitted Play/Locate.
        let action_pending = self.player_requests.play.is_some() || self.player_requests.locate.is_some() || self.player_requests.recording.is_some();
        let changed = self.player.as_ref().is_some_and(|p| !p.has_requested_session(&self.engine.session_arc()));
        if (action_pending && changed) || !self.sync_player_session() {
            return Err("The audio session is still updating; try again after it applies".into());
        }
        let Some(p) = &self.player else { return Ok(()) };
        let states = p.capture_states(&self.engine.session_arc());
        self.store_plugin_states(states)
    }

    fn preserve_audio_state(&mut self) -> Result<bool, String> {
        let Some(player) = &self.player else { return Ok(true) };
        let states = player.capture_states_for_reopen(&self.engine.session_arc());
        let states = self.player_requests.result(&mut self.status, states).ok_or_else(|| self.status.clone())?;
        let Some(states) = states else { return Ok(false) };
        self.store_plugin_states(Ok(states))?;
        Ok(true)
    }

    fn store_plugin_states(&mut self, states: Result<soundcraft_playback::PluginStates, soundcraft_playback::PlayerError>) -> Result<(), String> {
        let states = self.player_requests.result(&mut self.status, states).ok_or_else(|| self.status.clone())?;
        for (t, slot, data) in states {
            let slot = if slot == soundcraft_mix::INSTRUMENT_SLOT { json!("instrument") } else { json!(slot) };
            let state = soundcraft_model::b64::encode(&data);
            if let Err(e) = self.engine.execute("mix.insert_state", &json!({"track": t.0, "slot": slot, "state": state})) {
                self.status = format!("Cannot store plugin state: {e}");
                return Err(self.status.clone());
            }
        }
        Ok(())
    }

    /// Parameter edits made in plugin editors go into the session (one undo step per gesture).
    /// Calls `Player::idle`, which hosted editors need every frame (E7).
    pub(crate) fn apply_editor_edits(&mut self) {
        let discard = self.restore_pending || self.engine.transport_requests.contains(&TransportRequest::ResetSession);
        let Some(p) = &self.player else { return };
        let Some(edits) = self.player_requests.result(&mut self.status, p.idle()) else { return };
        if discard {
            return;
        }
        for (t, slot, param, value) in edits {
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
        if self.capture_plugin_states().is_ok() {
            self.autosave_to(&dir);
        }
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
        if let Some(point) = self.recorder.as_ref().and_then(soundcraft_playback::record::Recorder::applied_start)
            && let Some(plan) = &mut self.take_plan
        {
            plan.start = point.position.sample;
            self.record_start = plan.start;
        }
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
        let recording = self.recording_active();
        if recording {
            self.stop_play();
        }
        let keep = self.engine.is_dirty() && (recording || !self.quit_requested);
        if keep && self.capture_plugin_states().is_ok() {
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
        if self.recording_active() {
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
        if self.recording_active() {
            self.finish_recording();
        }
        if let Some(p) = &self.player {
            self.player_requests.result(&mut self.status, p.close_input(&mut self.recorder));
        } else {
            self.recorder = None;
        }
    }

    pub(crate) fn recording_active(&self) -> bool {
        self.engine.transport.recording
            || self.take_plan.is_some()
            || self.recorder.as_ref().is_some_and(soundcraft_playback::record::Recorder::has_pending_take)
    }

    pub(crate) fn poll_player(&mut self) {
        let Some(p) = &self.player else { return };
        let polled = p.poll_notices(&mut self.player_requests.notices);
        let count = self.player_requests.result(&mut self.status, polled);
        let mut collect = false;
        for index in 0..count.unwrap_or(0).min(self.player_requests.notices.len()) {
            if let Some(notice) = self.player_requests.notices.get_mut(index).and_then(Option::take) {
                collect |= self.player_requests.notice(notice, &mut self.status, &mut self.engine.transport.recording);
            }
        }
        let stopped = self
            .player_requests
            .result(&mut self.status, p.recovery_progress())
            .is_some_and(|frontiers| self.player_requests.recovery(frontiers, &mut self.status));
        if stopped && !p.is_playing() {
            self.engine.transport.position = p.position();
            self.complete_stop();
        }
        if collect && self.recording_active() {
            self.finish_recording();
        }
        if let Some(point) = self.recorder.as_ref().and_then(soundcraft_playback::record::Recorder::applied_start)
            && self.take_plan.as_ref().is_some_and(|plan| plan.start != point.position.sample)
        {
            self.record_start = point.position.sample;
            if let Some(plan) = &mut self.take_plan {
                plan.start = self.record_start;
            }
            if let Some(root) = self.autosave_dir.clone() {
                self.queue_autosave(root);
            }
        }
    }

    /// Take the input once (for recording and input monitoring) from the player and feed the
    /// player: on ASIO the one it opened with the output, elsewhere a newly opened device.
    pub(crate) fn ensure_input(&mut self) {
        use crate::audio_connection::Connection;
        if let Some(f) = &mut self.audio_devices.fake {
            if f.input.is_none()
                && let Some(p) = &f.connection
            {
                f.input = p.monitored_input(&self.ui.audio).ok();
            }
            return;
        }
        if self.recorder.is_some() || self.recorder_failed {
            return;
        }
        let Some(p) = &self.player else { return };
        match p.monitored_input(&self.ui.audio) {
            Ok(r) => {
                self.recorder = Some(r);
            }
            Err(e) => {
                self.recorder_failed = true;
                self.player_requests.fail(&mut self.status, format!("No audio input: {e}"));
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
        let (recording, playing) = (self.recording_active(), self.engine.transport.playing);
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
        let recording = self.recording_active();
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

    /// Reopen audio with editors closed before teardown. Never interrupt a recording.
    pub fn reconnect_audio(&mut self) {
        match self.reconnect_audio_using(true, Player::with_config) {
            Ok(false) => self.status = "Audio recovered from the document state; live plug-in state was unavailable after the audio fault.".into(),
            Ok(true) => {}
            Err(error) => self.player_requests.fail(&mut self.status, error),
        }
    }

    pub(crate) fn reconnect_audio_using(
        &mut self,
        preserve: bool,
        open: impl FnOnce(std::sync::Arc<soundcraft_model::Session>, &soundcraft_playback::AudioConfig) -> Player,
    ) -> Result<bool, String> {
        if self.recording_active() {
            return Err(crate::audio_devices::REFUSED_RECORDING.into());
        }
        if self.player.is_none() && self.audio_devices.fake.is_none() {
            return Ok(true);
        }
        let captured = !preserve || self.preserve_audio_state()?;
        if self.is_playing() {
            self.stop_play();
        }
        // A device reopen closes native editors deliberately; they remain wanted. Give the new
        // instances a fresh loading deadline instead of treating their missing windows as user closes.
        self.editors.clear();
        for key in self.ui.plugin_windows.clone() {
            self.want_editor(key);
        }
        self.recorder_failed = false;
        let cfg = self.ui.audio.clone();
        if let Some(f) = &mut self.audio_devices.fake {
            let trace = std::rc::Rc::clone(&f.trace);
            crate::audio_connection::replace(&mut f.connection, &mut f.input, || crate::audio_connection::FakeConnection::new(cfg.clone(), trace))?;
            f.reopened.push(cfg);
        } else {
            let session = self.engine.session_arc();
            crate::audio_connection::replace(&mut self.player, &mut self.recorder, || {
                soundcraft_playback::devices::release_asio_device_cache();
                open(session, &cfg)
            })?;
            self.player_requests.reset();
        }
        if self.engine.session().tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm) {
            self.ensure_input();
        }
        self.audio_devices.begin_verification();
        self.io_adopt = true;
        Ok(captured)
    }

    fn finish_recording(&mut self) {
        if self.player.is_none() {
            self.engine.transport.recording = false;
        }
        if let Some(p) = &self.player
            && let Some(sequence) = self.player_requests.result(&mut self.status, p.set_recording(false))
        {
            self.player_requests.recording = Some(sequence);
        }
        let plan = self.take_plan.take();
        let finished = self.recorder.as_mut().filter(|r| r.has_pending_take()).map(soundcraft_playback::record::Recorder::finish);
        // Tests substitute only the device's finished files; placement and stop ordering stay real.
        #[cfg(test)]
        let finished = finished.or_else(|| self.finished_test_takes.take());
        let Some((takes, problem)) = finished else { return };
        let tracks = takes.len().max(1);
        let mut plan = plan.unwrap_or_else(|| soundcraft_engine::io::Recording {
            start: self.record_start,
            pass: None,
            punch: self.punch,
            rate: 0,
            takes: Vec::new(),
        });
        if let Some(point) = self.recorder.as_ref().and_then(soundcraft_playback::record::Recorder::applied_start) {
            plan.start = point.position.sample;
            self.record_start = plan.start;
        }
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
    /// playlist that copies the old one (the engine does it inside the one "Record" undo step), so the old clips stay as the
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
                if over {
                    new_take.push(*t);
                }
            }
        }
        let ids = soundcraft_engine::io::place_recording_take(&mut self.engine, plan, takes, true);
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
        let sel = s.edit.cycle_range();
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
        if let Some(p) = &self.player
            && let Some(sequence) = self.player_requests.result(&mut self.status, p.locate(at))
        {
            self.player_requests.locate = Some(sequence);
        }
        if let Some(sim) = &mut self.sim {
            sim.0 = at;
        }
        if self.player.is_none() {
            self.engine.transport.position = at;
        }
    }

    /// The loop region reaches a running player the moment it changes (or Cycle goes off), like
    /// Ableton: the playhead is not moved. Not while recording, where the loop shapes the take passes.
    fn retarget_loop(&mut self) {
        let edit = &self.engine.session().edit;
        let want = (edit.loop_playback && !edit.loop_range.is_empty()).then_some(edit.loop_range);
        if want == self.loop_seen {
            return;
        }
        self.loop_seen = want;
        if self.recording_active() || !self.is_playing() {
            return;
        }
        if let Some(p) = &self.player {
            let _ = self.player_requests.result(&mut self.status, p.set_loop(want));
        } else if let Some(sim) = &mut self.sim {
            sim.2 = want;
        }
    }

    pub(crate) fn handle_transport(&mut self, dt: f32) {
        let mut reqs: std::collections::VecDeque<TransportRequest> = std::mem::take(&mut self.engine.transport_requests).into();
        while let Some(r) = reqs.pop_front() {
            match r {
                TransportRequest::ResetSession => {
                    self.restore_pending = true;
                    if let Some(p) = &self.player {
                        if let Some(sequence) = self.player_requests.result(&mut self.status, p.stop()) {
                            self.player_requests.stop = Some(sequence);
                        }
                        if let Some(sequence) = self.player_requests.result(&mut self.status, p.set_recording(false)) {
                            self.player_requests.recording = Some(sequence);
                        }
                        p.set_speed(1.0);
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
                    self.auto.gesture = None;
                    self.auto.selection.clear();
                    self.engine.transport.position = self.cursor();
                    if self.sync_player_session()
                        && let Some(p) = &self.player
                        && let Some(sequence) = self.player_requests.result(&mut self.status, p.locate(self.engine.transport.position))
                    {
                        self.player_requests.locate = Some(sequence);
                    }
                }
                TransportRequest::Play => {
                    if !self.is_playing() && self.player_requests.play.is_none() {
                        self.play_from_cursor();
                    }
                }
                TransportRequest::Stop => {
                    let was_stopped = !self.is_playing() && !self.engine.transport.recording;
                    self.stop_play();
                    if was_stopped {
                        // First Stop keeps the play-start cursor and finishes the take. A second
                        // Stop locates through the engine, without adding an undo step. Handle
                        // that locate before any later queued Play/Record request.
                        let _ = self.run("transport.rtz", json!({}));
                        for request in std::mem::take(&mut self.engine.transport_requests).into_iter().rev() {
                            reqs.push_front(request);
                        }
                    }
                }
                TransportRequest::Pause => self.stop_play(),
                TransportRequest::TogglePlay => {
                    if self.is_playing() || self.player_requests.play.is_some() {
                        self.stop_play();
                    } else {
                        self.play_from_cursor();
                    }
                }
                TransportRequest::Record => {
                    if self.recording_active() {
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
                        if let Some(sequence) = self.player_requests.result(&mut self.status, p.all_notes_off()) {
                            self.player_requests.release = Some(sequence);
                            self.status = "All MIDI notes off pending".into();
                        }
                    } else {
                        self.status = "All MIDI notes off".into();
                    }
                }
            }
        }
        self.retarget_loop();
        // Read back the position.
        if let Some(p) = &self.player {
            let playing = p.is_playing();
            let ended = self.engine.transport.playing && !playing;
            if ended {
                p.set_speed(1.0);
            }
            self.engine.transport.playing = playing;
            self.engine.transport.position = p.position();
            let now = self.last_frame.unwrap_or_default();
            if now - self.last_meter_poll >= 1.0 / 20.0 {
                self.player_requests.result(&mut self.status, p.copy_meters(&mut self.meter_snapshot));
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
            if ended && self.player_requests.stop.is_none() {
                self.complete_stop();
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

/// Admission is separate from callback application. Storage is reused every frame,
/// including idle frames, so terminal results always return admission credits.
pub(crate) struct PlayerRequests {
    notices: [Option<soundcraft_playback::PlayerNotice>; 64],
    pub(crate) play: Option<soundcraft_playback::action_lane::Sequence>,
    pub(crate) locate: Option<soundcraft_playback::action_lane::Sequence>,
    stop: Option<soundcraft_playback::action_lane::Sequence>,
    release: Option<soundcraft_playback::action_lane::Sequence>,
    pub(crate) recording: Option<soundcraft_playback::action_lane::Sequence>,
    error: Option<String>,
}

impl Default for PlayerRequests {
    fn default() -> Self {
        Self { notices: [None; 64], play: None, locate: None, stop: None, release: None, recording: None, error: None }
    }
}

impl PlayerRequests {
    pub(crate) fn reset(&mut self) {
        let error = self.error.take();
        *self = Self { error, ..Self::default() };
    }

    pub(crate) fn pending(&self) -> bool {
        self.play.is_some() || self.locate.is_some() || self.stop.is_some() || self.release.is_some() || self.recording.is_some()
    }

    pub(crate) fn fail(&mut self, status: &mut String, message: String) {
        log::warn!("{message}");
        let error = self.error.get_or_insert_with(String::new);
        if !error.is_empty() {
            error.push_str("; ");
        }
        error.push_str(&message);
        status.clone_from(error);
    }

    pub(crate) fn result<T>(&mut self, status: &mut String, result: Result<T, soundcraft_playback::PlayerError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                if error == soundcraft_playback::PlayerError::Faulted {
                    self.reset();
                }
                self.fail(status, error.to_string());
                None
            }
        }
    }

    /// Returns true when a refused start requires collecting an armed take.
    pub(crate) fn notice(&mut self, notice: soundcraft_playback::PlayerNotice, status: &mut String, recording: &mut bool) -> bool {
        use soundcraft_playback::{ActionResult, PlayerCommand, PlayerNotice, action_lane::Outcome, player::SessionOutcome};
        match notice {
            PlayerNotice::Session(n) => {
                if let SessionOutcome::Rejected(reason) = n.outcome {
                    self.fail(status, format!("Audio session {} rejected: {reason:?}", n.request.get()));
                }
            }
            PlayerNotice::Action(r) => {
                let applied = r.outcome == Outcome::Applied
                    && matches!(
                        r.handler_result,
                        Some(
                            ActionResult::Applied
                                | ActionResult::RecordingStarted(_)
                                | ActionResult::Event(
                                    soundcraft_mix::EventOutcome::Queued
                                        | soundcraft_mix::EventOutcome::QueuedLate { .. }
                                        | soundcraft_mix::EventOutcome::ReleaseAdvanced { .. }
                                        | soundcraft_mix::EventOutcome::Coalesced
                                )
                        )
                    );
                if self.locate == Some(r.sequence) {
                    self.locate = None;
                }
                let play = self.play == Some(r.sequence);
                let record = self.recording == Some(r.sequence);
                if play {
                    self.play = None;
                    if applied && status == "Play pending" {
                        *status = "Playing".into();
                    }
                }
                if record {
                    self.recording = None;
                    if applied {
                        match r.payload {
                            PlayerCommand::Recording(on) => *recording = on,
                            PlayerCommand::Record { .. } => {
                                *recording = true;
                                if status == "Record pending" {
                                    *status = "Recording".into();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !applied && !matches!(r.outcome, Outcome::Stopped | Outcome::NotesReleased) {
                    self.fail(status, format!("Audio action {} refused: {:?} ({:?})", r.sequence.get(), r.outcome, r.handler_result));
                }
                return (play || record) && !applied && self.stop.is_none();
            }
        }
        false
    }

    pub(crate) fn recovery(
        &mut self,
        frontiers: (soundcraft_playback::action_lane::Sequence, soundcraft_playback::action_lane::Sequence),
        status: &mut String,
    ) -> bool {
        let stopped = self.stop.is_some_and(|s| frontiers.0 >= s);
        if stopped {
            self.stop = None;
            if status == "Stop pending" {
                *status = "Stopped".into();
            }
        }
        if self.release.is_some_and(|s| frontiers.1 >= s) {
            self.release = None;
            *status = "All MIDI notes off".into();
        }
        stopped
    }

    pub(crate) fn show_error(&mut self, status: &mut String) {
        if let Some(error) = self.error.take() {
            *status = error;
        }
    }
}

#[cfg(test)]
#[path = "player_tests.rs"]
mod player_tests;
