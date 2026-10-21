//! Step entry: notes played on the MIDI keyboard go into the open clip at a cursor instead of
//! being recorded. A chord (note-ons before the first note-off) is one step; a rest advances the
//! cursor; Backspace takes the last step back. Every step is one `event.step_input` (or
//! `midi.note_delete`) command, so one undo step each. Session state only: never saved.

use super::{Note, PPQ, StudioApp, clip_view, json, resolve};
use soundcraft_playback::PlayedNote;

/// What one entered step left behind, for Backspace.
#[derive(Debug, Clone)]
struct Entered {
    start: i64,
    notes: Vec<Note>,
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
    /// (pitch, velocity) of the note-ons since the last committed step.
    group: Vec<(u8, u8)>,
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
        self.keys.step.len.unwrap_or_else(|| self.ui.keys.snap.step())
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

    /// Notes played on the armed MIDI inputs since the last frame.
    pub(crate) fn step_poll(&mut self) {
        let Some(p) = &self.player else { return };
        for n in p.take_played_notes() {
            self.step_midi(n);
        }
    }

    /// One played note (the live MIDI path and the test hook): note-ons gather into a chord that
    /// is entered, as one step, at the first note-off.
    pub fn step_midi(&mut self, n: PlayedNote) {
        if !self.step_active() {
            self.keys.step.group.clear();
            return;
        }
        if n.on {
            self.keys.step.group.push((n.pitch, n.velocity.clamp(1, 127)));
        } else if !self.keys.step.group.is_empty() {
            let group = std::mem::take(&mut self.keys.step.group);
            self.step_enter(&group);
        }
    }

    pub fn step_rest(&mut self) {
        self.step_enter(&[]);
    }

    pub fn step_set_len(&mut self, ticks: i64) {
        self.keys.step.len = Some(ticks.max(1));
    }

    /// Enter `notes` (pitch, velocity) at the cursor, or a rest when empty, and advance.
    fn step_enter(&mut self, notes: &[(u8, u8)]) {
        let Some(clip) = self.keys.clip else { return };
        let len = self.step_len();
        let at = self.keys.step.cursor;
        let s = self.engine.session();
        let Some((track, c)) = s.find_clip(clip) else { return };
        let Some(cv) = clip_view(s, clip) else { return };
        // The extension rule of `event.step_input` stops at the next clip: say so instead.
        let next = s.track(track).and_then(|t| t.clips().iter().filter(|o| o.id != clip && o.start >= c.start).map(|o| o.start).min());
        if let Some(next) = next
            && at + len > cv.len
            && cv.c0 + at + len > s.tempo.samples_to_ticks(next, s.sample_rate)
        {
            self.status = "Step entry stopped: the next clip starts here.".into();
            return;
        }
        let pitches: Vec<u8> = notes.iter().map(|n| n.0).collect();
        let velocities: Vec<u8> = notes.iter().map(|n| n.1).collect();
        let key = self.keys_merge_key();
        let params = json!({"track": track.0, "clip": clip.0, "at": cv.c0 + at, "duration": len, "rest": notes.is_empty(),
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
        step.steps.push(Entered { start: at, notes: entered.collect() });
        step.cursor = at + len;
        step.follow = true;
    }

    /// Backspace: delete the last entered step's notes and move the cursor back onto it; with no
    /// step left to take back, just move the cursor back one step.
    pub fn step_back(&mut self) {
        let Some(clip) = self.keys.clip else { return };
        let Some(last) = self.keys.step.steps.pop() else {
            self.keys.step.cursor = (self.keys.step.cursor - self.step_len()).max(0);
            self.keys.step.follow = true;
            return;
        };
        let idx = resolve(&self.keys_notes(), &last.notes);
        if !idx.is_empty() {
            let key = self.keys_merge_key();
            if self.keys_exec("midi.note_delete", json!({"clip": clip.0, "indices": idx}), &key).is_err() {
                return;
            }
        }
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
