//! Deterministic step-entry timing. The caller supplies monotonic milliseconds; no audio
//! device or wall clock is needed, and pending notes do not mutate the document.

/// Hold preferences, persisted by the UI rather than in the musical document.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HoldPrefs {
    pub threshold_ms: u64,
    pub floor_ms: u64,
}

impl Default for HoldPrefs {
    fn default() -> Self {
        Self { threshold_ms: 500, floor_ms: 80 }
    }
}

impl HoldPrefs {
    /// Number of step units at this elapsed time. Arithmetic is bounded even for a
    /// malformed preference file or a very long suspended UI frame.
    pub fn units(self, elapsed_ms: u64) -> u32 {
        let threshold = self.threshold_ms.clamp(1, 60_000);
        let floor = self.floor_ms.clamp(1, threshold);
        if elapsed_ms < threshold {
            return 1;
        }
        let mut remaining = elapsed_ms - threshold;
        let mut units = 2u64;
        for interval in [300, 220, 160, 120] {
            let interval = interval.min(threshold).max(floor);
            if remaining < interval {
                return units as u32;
            }
            remaining -= interval;
            units += 1;
        }
        units.saturating_add(remaining / floor).min(u64::from(u32::MAX)) as u32
    }
}

/// One completed chord, including notes released before the last finger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepChord {
    pub notes: Vec<(u8, u8)>,
    pub units: u32,
}

/// A learnable key/pad or controller. Bindings apply only during step entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StepBinding {
    Note(u8),
    Cc(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepControl {
    Rest,
    Back,
    Tie,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StepBindings {
    pub rest: Option<StepBinding>,
    pub back: Option<StepBinding>,
    pub tie: Option<StepBinding>,
}

impl Default for StepBindings {
    fn default() -> Self {
        Self { rest: Some(StepBinding::Cc(64)), back: None, tie: None }
    }
}

impl StepBindings {
    /// One message performs one action. Learning a binding moves it from any old action.
    pub fn learn(&mut self, action: StepControl, binding: StepBinding) {
        for slot in [&mut self.rest, &mut self.back, &mut self.tie] {
            if *slot == Some(binding) {
                *slot = None;
            }
        }
        *match action {
            StepControl::Rest => &mut self.rest,
            StepControl::Back => &mut self.back,
            StepControl::Tie => &mut self.tie,
        } = Some(binding);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepAction {
    Enter(StepChord),
    /// Sustain plus a replayed chord: only extend if the previous pitches match.
    TieChord(StepChord),
    Rest(u32),
    Back,
    Tie,
}

/// UI-thread controller semantics. A pedal tap is deferred until release so pressing
/// a chord while the pedal is held can mean tie without also inserting a rest.
#[derive(Debug, Default)]
pub struct StepController {
    input: StepInput,
    pressed: Vec<StepBinding>,
    rest: Option<(u64, u32)>,
    pedal: bool,
    tie_chord: bool,
}

impl StepController {
    pub fn is_pending(&self) -> bool {
        self.rest.is_some() || self.input.started_ms.is_some()
    }

    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    pub fn preview(&self, now_ms: u64, prefs: HoldPrefs) -> Option<StepChord> {
        self.input.preview(now_ms, prefs)
    }

    /// Emits a single bounded batch even after a suspended frame.
    pub fn poll(&mut self, now_ms: u64, prefs: HoldPrefs) -> Option<StepAction> {
        let (started, emitted) = self.rest.as_mut()?;
        let units = prefs.units(now_ms.saturating_sub(*started));
        if units <= 1 || units <= *emitted {
            return None;
        }
        let delta = units - *emitted;
        *emitted = units;
        Some(StepAction::Rest(delta))
    }

    /// `value` is note velocity or a CC value; CC >=64 is down. Repeated CC values
    /// do not retrigger an action. Note/pad bindings consume both press and release.
    pub fn message(&mut self, binding: StepBinding, value: u8, now_ms: u64, prefs: HoldPrefs, bindings: StepBindings) -> Option<StepAction> {
        let down = match binding {
            StepBinding::Note(_) => value > 0,
            StepBinding::Cc(_) => value >= 64,
        };
        if binding == StepBinding::Cc(64) {
            self.pedal = down;
        }
        let action = if bindings.rest == Some(binding) {
            Some(StepControl::Rest)
        } else if bindings.back == Some(binding) {
            Some(StepControl::Back)
        } else if bindings.tie == Some(binding) {
            Some(StepControl::Tie)
        } else {
            None
        };
        if let Some(action) = action {
            let was_down = self.pressed.contains(&binding);
            if down == was_down {
                return None;
            }
            if down {
                self.pressed.push(binding);
            } else {
                self.pressed.retain(|b| *b != binding);
            }
            return match (action, down) {
                (StepControl::Rest, true) => {
                    // A rest cannot move the insertion point under a pending chord.
                    if self.input.started_ms.is_none() {
                        self.rest = Some((now_ms, 0));
                    }
                    None
                }
                (StepControl::Rest, false) => {
                    let (started, emitted) = self.rest.take()?;
                    let delta = prefs.units(now_ms.saturating_sub(started)).saturating_sub(emitted);
                    (delta > 0).then_some(StepAction::Rest(delta))
                }
                (StepControl::Back, true) => {
                    self.input.cancel();
                    self.rest = None;
                    self.tie_chord = false;
                    Some(StepAction::Back)
                }
                (StepControl::Tie, true) => {
                    self.input.cancel();
                    self.rest = None;
                    self.tie_chord = false;
                    Some(StepAction::Tie)
                }
                _ => None,
            };
        }
        let StepBinding::Note(pitch) = binding else { return None };
        if down {
            if self.input.started_ms.is_none() {
                self.tie_chord = self.pedal;
            }
            self.rest = None;
        }
        let chord = self.input.note(pitch, value, down, now_ms, prefs)?;
        Some(if std::mem::take(&mut self.tie_chord) { StepAction::TieChord(chord) } else { StepAction::Enter(chord) })
    }
}

/// Pending chord state; duplicate note-ons are counted so an early note-off cannot
/// finish a still-held unison. Cancellation discards a preview, never inserts it.
#[derive(Debug, Default)]
pub struct StepInput {
    held: std::collections::BTreeMap<u8, u32>,
    notes: std::collections::BTreeMap<u8, u8>,
    started_ms: Option<u64>,
}

impl StepInput {
    pub fn cancel(&mut self) {
        self.held.clear();
        self.notes.clear();
        self.started_ms = None;
    }

    pub fn preview(&self, now_ms: u64, prefs: HoldPrefs) -> Option<StepChord> {
        let start = self.started_ms?;
        Some(StepChord {
            notes: self.notes.iter().map(|(&pitch, &velocity)| (pitch, velocity)).collect(),
            units: prefs.units(now_ms.saturating_sub(start)),
        })
    }

    /// Note-on velocity zero follows MIDI's note-off convention. Unknown releases
    /// and invalid pitches are ignored.
    pub fn note(&mut self, pitch: u8, velocity: u8, on: bool, now_ms: u64, prefs: HoldPrefs) -> Option<StepChord> {
        if pitch > 127 {
            return None;
        }
        if on && velocity > 0 {
            self.started_ms.get_or_insert(now_ms);
            let count = self.held.entry(pitch).or_default();
            *count = count.saturating_add(1);
            self.notes.entry(pitch).or_insert(velocity.min(127));
            return None;
        }
        let count = self.held.get_mut(&pitch)?;
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.held.remove(&pitch);
        }
        if !self.held.is_empty() {
            return None;
        }
        let chord = self.preview(now_ms, prefs);
        self.cancel();
        chord
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hold_threshold_and_accelerating_repeat_schedule() {
        let prefs = HoldPrefs::default();
        for (ms, units) in [(0, 1), (499, 1), (500, 2), (799, 2), (800, 3), (1020, 4), (1180, 5), (1300, 6), (1380, 7)] {
            assert_eq!(prefs.units(ms), units, "at {ms} ms");
        }
        assert_eq!(HoldPrefs { threshold_ms: 200, floor_ms: 100 }.units(199), 1);
        assert_eq!(HoldPrefs { threshold_ms: 200, floor_ms: 100 }.units(200), 2);
        assert_eq!(prefs.units(u64::MAX), u32::MAX);
    }

    #[test]
    fn chord_commits_on_last_release_with_one_shared_duration() {
        let mut input = StepInput::default();
        let prefs = HoldPrefs::default();
        for (pitch, ms) in [(60, 0), (64, 20), (67, 40)] {
            assert!(input.note(pitch, 90, true, ms, prefs).is_none());
        }
        assert!(input.note(60, 0, false, 200, prefs).is_none());
        assert!(input.note(64, 0, false, 600, prefs).is_none());
        let chord = input.note(67, 0, false, 1050, prefs).unwrap();
        assert_eq!(chord.notes, vec![(60, 90), (64, 90), (67, 90)]);
        assert_eq!(chord.units, 4);
        assert!(input.preview(1100, prefs).is_none());
    }

    #[test]
    fn quick_entry_unmatched_releases_and_cancellation() {
        let mut input = StepInput::default();
        let prefs = HoldPrefs::default();
        input.note(60, 70, true, 0, prefs);
        assert!(input.note(62, 0, false, 100, prefs).is_none());
        assert_eq!(input.note(60, 0, true, 100, prefs).unwrap().units, 1);
        input.note(60, 70, true, 200, prefs);
        input.cancel();
        assert!(input.note(60, 0, false, 2000, prefs).is_none());
    }

    #[test]
    fn pedal_tap_and_hold_repeat_but_duplicate_cc_does_not_retrigger() {
        let mut input = StepController::default();
        let prefs = HoldPrefs::default();
        let bindings = StepBindings::default();
        let pedal = StepBinding::Cc(64);
        assert_eq!(input.message(pedal, 127, 0, prefs, bindings), None);
        assert_eq!(input.message(pedal, 0, 100, prefs, bindings), Some(StepAction::Rest(1)));
        input.message(pedal, 127, 200, prefs, bindings);
        assert_eq!(input.poll(699, prefs), None);
        assert_eq!(input.poll(700, prefs), Some(StepAction::Rest(2)));
        assert_eq!(input.message(pedal, 127, 800, prefs, bindings), None);
        assert_eq!(input.poll(1000, prefs), Some(StepAction::Rest(1)));
        assert_eq!(input.message(pedal, 0, 1001, prefs, bindings), None);
        assert!(!input.is_pending());
    }

    #[test]
    fn pedal_chord_ties_without_an_extra_rest_even_if_pedal_releases_first() {
        let mut input = StepController::default();
        let prefs = HoldPrefs::default();
        let bindings = StepBindings::default();
        input.message(StepBinding::Cc(64), 127, 0, prefs, bindings);
        input.message(StepBinding::Note(60), 90, 50, prefs, bindings);
        assert_eq!(input.poll(600, prefs), None);
        assert_eq!(input.message(StepBinding::Cc(64), 0, 650, prefs, bindings), None);
        assert_eq!(
            input.message(StepBinding::Note(60), 0, 700, prefs, bindings),
            Some(StepAction::TieChord(StepChord { notes: vec![(60, 90)], units: 2 }))
        );
    }

    #[test]
    fn learned_back_and_tie_keys_are_actions_not_entered_notes() {
        let mut input = StepController::default();
        let prefs = HoldPrefs::default();
        let mut bindings = StepBindings::default();
        bindings.learn(StepControl::Back, StepBinding::Note(21));
        bindings.learn(StepControl::Tie, StepBinding::Note(22));
        for (pitch, expected) in [(21, StepAction::Back), (22, StepAction::Tie)] {
            assert_eq!(input.message(StepBinding::Note(pitch), 100, 0, prefs, bindings), Some(expected));
            assert_eq!(input.message(StepBinding::Note(pitch), 0, 100, prefs, bindings), None);
            assert_eq!(input.preview(100, prefs), None);
        }
        bindings.learn(StepControl::Tie, StepBinding::Note(21));
        assert_eq!(bindings.back, None);
        assert_eq!(bindings.tie, Some(StepBinding::Note(21)));
    }
}
