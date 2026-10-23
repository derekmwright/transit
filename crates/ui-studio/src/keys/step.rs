//! Step entry: notes played on the MIDI keyboard go into the open clip at a cursor instead of
//! being recorded. A chord commits on its last note-off as one step; a rest advances the
//! cursor; Backspace takes the last step back. Every step is one `event.step_input` (or
//! `midi.note_delete`) command, so one undo step each. Session state only: never saved.

use super::{Note, PPQ, StudioApp, clip_view, json, resolve};
use soundcraft_engine::step_input::{StepAction, StepBinding, StepChord, StepControl, StepController};
use soundcraft_playback::{PlayedMidi, PlayedNote};

/// What one entered step left behind, for Backspace.
#[derive(Debug, Clone)]
struct Entered {
    start: i64,
    /// Where the step ends (a rest has no notes to say so).
    end: i64,
    notes: Vec<Note>,
    /// A tie restores these note lengths when stepping back.
    before: Vec<Note>,
}

#[derive(Debug, Default)]
pub struct StepState {
    pub on: bool,
    /// The insertion point in ticks from the clip start.
    pub cursor: i64,
    /// Step length in ticks; the piano roll's grid when `None`.
    pub len: Option<i64>,
    /// Scroll the cursor into view on the next frame.
    pub(super) follow: bool,
    steps: Vec<Entered>,
    input: StepController,
    clock: Option<std::time::Instant>,
    pub(super) learning: Option<StepControl>,
    learned_down: Option<StepBinding>,
}

impl StepState {
    pub(crate) fn input_cancel_for_learn(&mut self, action: StepControl) {
        self.input.cancel();
        self.learning = Some(action);
        self.learned_down = None;
    }

    pub(super) fn pending(&self) -> bool {
        self.input.is_pending()
    }
}

/// A step length as the toolbar shows it.
pub fn length_label(ticks: i64) -> String {
    for (t, label) in [
        (PPQ * 4, "1/1"),
        (PPQ * 2, "1/2"),
        (PPQ, "1/4"),
        (PPQ / 2, "1/8"),
        (PPQ / 4, "1/16"),
        (PPQ / 8, "1/32"),
        (PPQ / 3, "1/8T"),
        (PPQ / 6, "1/16T"),
    ] {
        if t == ticks {
            return label.into();
        }
    }
    format!("{ticks}t")
}

impl StudioApp {
    /// Step entry is on and the piano roll shows the clip it enters into.
    pub fn step_active(&self) -> bool {
        self.keys.step.on && self.keys_shown()
    }

    pub fn step_len(&self) -> i64 {
        self.keys.step.len.unwrap_or_else(|| self.keys_step())
    }

    /// Turn Step on or off. On: the cursor starts at the selection or the playhead when either is
    /// inside the clip, else at the clip start.
    pub fn step_toggle(&mut self) {
        if self.keys.step.on {
            self.keys.step = StepState::default();
            return;
        }
        let Some(clip) = self.keys.clip else { return };
        let s = self.engine.session();
        let Some(cv) = clip_view(s, clip) else { return };
        let inside = |abs: i64| (abs >= cv.c0 && abs < cv.c0 + cv.len).then_some(abs - cv.c0);
        let sel = inside(s.tempo.samples_to_ticks(s.edit.selection.start.max(0), s.sample_rate));
        let head = inside(s.tempo.samples_to_ticks(self.position().max(0), s.sample_rate));
        let cursor = sel.filter(|t| *t > 0).or(head.filter(|t| *t > 0)).unwrap_or(0);
        self.keys.step = StepState { on: true, cursor, follow: true, ..StepState::default() };
    }

    /// Notes played on the armed MIDI inputs since the last frame. One drain feeds both the
    /// piano roll's held keys and Step entry (each event reaches each exactly once).
    pub(crate) fn step_poll(&mut self) {
        if !self.step_active() {
            self.keys.step.input.cancel();
        }
        let inputs: Vec<_> = self.engine.session().tracks.iter().map(|t| t.midi_input.clone()).collect();
        if inputs != self.keys.live_inputs {
            // A device was closed or reselected: its note-offs may never come.
            self.keys.live_inputs = inputs;
            self.live_clear();
        }
        let messages = self.player.as_ref().map(|p| p.take_played_midi()).unwrap_or_default();
        for message in messages {
            self.live_played_midi(message);
        }
        let now = self.step_now_ms();
        self.step_tick_at(now);
    }

    pub(crate) fn live_played_midi(&mut self, message: PlayedMidi) {
        match message {
            PlayedMidi::Note(n) => self.live_midi(n),
            PlayedMidi::Control { cc, value } => {
                let now = self.step_now_ms();
                self.step_message_at(StepBinding::Cc(cc), value, now);
            }
            PlayedMidi::Reset => self.live_clear(),
        }
    }

    /// One played note: update the held keys, then Step entry.
    pub fn live_midi(&mut self, n: PlayedNote) {
        if let Some(v) = self.keys.live.get_mut(usize::from(n.pitch)) {
            *v = if n.on { n.velocity.min(127) } else { 0 };
        }
        self.step_midi(n);
    }

    /// Let go of every live key (Stop, all notes off, input change).
    pub fn live_clear(&mut self) {
        self.keys.live = [0; 128];
        self.keys.step.input.cancel();
        self.keys.step.learned_down = None;
    }

    /// The keys held on the MIDI keyboard as (pitch, velocity), lowest first.
    pub fn live_keys(&self) -> Vec<(u8, u8)> {
        (0u8..128).zip(self.keys.live).filter(|(_, v)| *v > 0).collect()
    }

    /// One played note (the live MIDI path and the test hook): note-ons gather into a chord that
    /// is entered, as one step, at the last note-off.
    pub fn step_midi(&mut self, n: PlayedNote) {
        let now = self.step_now_ms();
        self.step_midi_at(n, now);
    }

    fn step_now_ms(&mut self) -> u64 {
        self.keys.step.clock.get_or_insert_with(std::time::Instant::now).elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    /// Deterministic input hook shared by the live path and timing regression tests.
    pub(crate) fn step_midi_at(&mut self, n: PlayedNote, now_ms: u64) {
        self.step_message_at(StepBinding::Note(n.pitch), if n.on { n.velocity } else { 0 }, now_ms);
    }

    pub(crate) fn step_message_at(&mut self, binding: StepBinding, value: u8, now_ms: u64) {
        if !self.step_active() {
            self.keys.step.input.cancel();
            return;
        }
        let down = match binding {
            StepBinding::Note(_) => value > 0,
            StepBinding::Cc(_) => value >= 64,
        };
        if self.keys.step.learned_down == Some(binding) {
            if !down {
                self.keys.step.learned_down = None;
            }
            return;
        }
        if down && let Some(action) = self.keys.step.learning.take() {
            self.ui.keys.step_bindings.learn(action, binding);
            self.keys.step.input.cancel();
            self.keys.step.learned_down = Some(binding);
            return;
        }
        if let Some(action) = self.keys.step.input.message(binding, value, now_ms, self.ui.keys.step_hold, self.ui.keys.step_bindings) {
            self.step_action(action);
        }
    }

    pub(crate) fn step_tick_at(&mut self, now_ms: u64) {
        if self.step_active()
            && let Some(action) = self.keys.step.input.poll(now_ms, self.ui.keys.step_hold)
        {
            self.step_action(action);
        }
    }

    fn step_action(&mut self, action: StepAction) {
        match action {
            StepAction::Enter(chord) => self.step_enter_units(&chord.notes, chord.units),
            StepAction::Rest(units) => self.step_enter_units(&[], units),
            StepAction::Back => self.step_back(),
            StepAction::Tie => self.step_tie(&[], 1),
            StepAction::TieChord(chord) => {
                let mut previous: Vec<_> = self.keys_notes().iter().filter(|n| n.end() == self.keys.step.cursor).map(|n| n.pitch).collect();
                previous.sort_unstable();
                previous.dedup();
                let wanted: Vec<_> = chord.notes.iter().map(|n| n.0).collect();
                if previous == wanted {
                    self.step_tie(&chord.notes, chord.units);
                } else {
                    self.step_enter_units(&chord.notes, chord.units);
                }
            }
        }
    }

    fn step_tie(&mut self, notes: &[(u8, u8)], units: u32) {
        let Some(clip) = self.keys.clip else { return };
        let Some(track) = self.keys_track() else { return };
        let Some(cv) = clip_view(self.engine.session(), clip) else { return };
        let at = self.keys.step.cursor;
        let Some(len) = self.step_len().checked_mul(i64::from(units)) else { return };
        let Some(end) = at.checked_add(len) else { return };
        let Some(abs) = cv.c0.checked_add(at) else { return };
        let before: Vec<_> = self.keys_notes().into_iter().filter(|n| n.end() == at).collect();
        let pitches: Vec<_> = notes.iter().map(|n| n.0).collect();
        let key = self.keys_merge_key();
        if self
            .keys_exec(
                "event.step_input",
                json!({"track": track.0, "clip": clip.0, "at": abs, "duration": len, "pitches": pitches, "tie": true, "advance": false}),
                &key,
            )
            .is_err()
        {
            return;
        }
        let after = before.iter().map(|n| Note { length: n.length + len, ..*n }).collect();
        self.keys.step.steps.push(Entered { start: at, end, notes: after, before });
        self.keys.step.cursor = end;
        self.keys.step.follow = true;
    }

    pub(super) fn step_preview(&mut self) -> Option<StepChord> {
        if !self.step_active() {
            return None;
        }
        let now = self.step_now_ms();
        self.keys.step.input.preview(now, self.ui.keys.step_hold)
    }

    pub fn step_rest(&mut self) {
        self.keys.step.input.cancel();
        self.step_enter(&[]);
    }

    pub fn step_set_len(&mut self, ticks: i64) {
        self.keys.step.len = Some(ticks.max(1));
    }

    /// Enter `notes` (pitch, velocity) at the cursor, or a rest when empty, and advance.
    fn step_enter(&mut self, notes: &[(u8, u8)]) {
        self.step_enter_units(notes, 1);
    }

    fn step_enter_units(&mut self, notes: &[(u8, u8)], units: u32) {
        let Some(clip) = self.keys.clip else { return };
        let Some(len) = self.step_len().checked_mul(i64::from(units)) else { return };
        let at = self.keys.step.cursor;
        let Some(end) = at.checked_add(len) else { return };
        let s = self.engine.session();
        let Some((track, c)) = s.find_clip(clip) else { return };
        let Some(cv) = clip_view(s, clip) else { return };
        let Some(abs) = cv.c0.checked_add(at) else { return };
        let Some(abs_end) = abs.checked_add(len) else { return };
        // The extension rule of `event.step_input` stops at the next clip: say so instead.
        let next = s.track(track).and_then(|t| t.clips().iter().filter(|o| o.id != clip && o.start >= c.start).map(|o| o.start).min());
        if let Some(next) = next
            && end > cv.len
            && abs_end > s.tempo.samples_to_ticks(next, s.sample_rate)
        {
            self.status = "Step entry stopped: the next clip starts here.".into();
            return;
        }
        let fixed = self.ui.keys.step_fixed.then_some(self.ui.keys.step_velocity.clamp(1, 127));
        let notes: Vec<(u8, u8)> = notes.iter().map(|(pitch, played)| (*pitch, fixed.unwrap_or(*played))).collect();
        let pitches: Vec<u8> = notes.iter().map(|n| n.0).collect();
        let velocities: Vec<u8> = notes.iter().map(|n| n.1).collect();
        let key = self.keys_merge_key();
        let params = json!({"track": track.0, "clip": clip.0, "at": abs, "duration": len, "rest": notes.is_empty(),
            "pitches": pitches, "velocities": velocities, "advance": false});
        if self.keys_exec("event.step_input", params, &key).is_err() {
            return;
        }
        let entered = notes.iter().map(|(pitch, velocity)| Note {
            pitch: *pitch,
            velocity: *velocity,
            release_velocity: 64,
            channel: 0,
            start: at,
            length: len,
        });
        let step = &mut self.keys.step;
        step.steps.push(Entered { start: at, end, notes: entered.collect(), before: Vec::new() });
        step.cursor = end;
        step.follow = true;
    }

    /// Left and Right: move the cursor one step length back (`-1`) or forward (`1`) without
    /// editing anything; it stops at the clip start.
    pub fn step_move(&mut self, dir: i64) {
        let len = self.step_len();
        let step = &mut self.keys.step;
        step.input.cancel();
        step.cursor = step.cursor.saturating_add(dir.saturating_mul(len)).max(0);
        step.follow = true;
    }

    /// Home: the cursor to the clip start.
    pub fn step_home(&mut self) {
        self.keys.step.input.cancel();
        self.keys.step.cursor = 0;
        self.keys.step.follow = true;
    }

    /// End: the cursor to the end of the material: the last step entered this session or the last
    /// note already in the clip, whichever is later.
    pub fn step_end(&mut self) {
        self.keys.step.input.cancel();
        let entered = self.keys.step.steps.iter().map(|e| e.end).max().unwrap_or(0);
        let existing = self.keys_notes().iter().map(Note::end).max().unwrap_or(0);
        self.keys.step.cursor = entered.max(existing);
        self.keys.step.follow = true;
    }

    /// Backspace: delete the last entered step's notes and move the cursor back onto it; with no
    /// step left to take back, just move the cursor back one step.
    pub fn step_back(&mut self) {
        self.keys.step.input.cancel();
        let Some(clip) = self.keys.clip else { return };
        let Some(last) = self.keys.step.steps.last().cloned() else {
            self.keys.step.cursor = (self.keys.step.cursor - self.step_len()).max(0);
            self.keys.step.follow = true;
            return;
        };
        let idx = resolve(&self.keys_notes(), &last.notes);
        if !last.before.is_empty() {
            if idx.len() != last.notes.len() {
                self.status = "Step back stopped: the tied notes have changed.".into();
                return;
            }
            let key = self.keys_merge_key();
            for (current, before) in last.notes.iter().zip(&last.before) {
                let indices = resolve(&self.keys_notes(), &[*current]);
                let Some(index) = indices.first() else { return };
                if self.keys_exec("midi.note_edit", json!({"clip": clip.0, "index": index, "length_ticks": before.length}), &key).is_err() {
                    return;
                }
            }
        } else if !idx.is_empty() {
            let key = self.keys_merge_key();
            if self.keys_exec("midi.note_delete", json!({"clip": clip.0, "indices": idx}), &key).is_err() {
                return;
            }
        }
        self.keys.step.steps.pop();
        self.keys.selection.clear();
        self.keys.step.cursor = last.start;
        self.keys.step.follow = true;
    }

    /// Scroll to keep the cursor on screen once after it moves; its position and extent in ticks.
    pub(super) fn step_cursor(&mut self, ppt: f64, width: f32) -> Option<(i64, i64)> {
        if !self.step_active() {
            return None;
        }
        let (cursor, len) = (self.keys.step.cursor, self.step_len());
        if std::mem::take(&mut self.keys.step.follow) {
            let visible = f64::from(width) / ppt;
            if (cursor as f64) < self.keys.scroll || (cursor + len) as f64 > self.keys.scroll + visible {
                self.keys.scroll = (cursor as f64 - visible * 0.25).max(0.0);
            }
        }
        Some((cursor, len))
    }
}
