//! Every keyboard shortcut of the studio UI, in one table (plan 06 slice 5). The handlers in
//! `menus`, `groups`, `browser`, `keys` and `mixer` ask this table ([`pressed`], [`hit`]) and the
//! menus show its text ([`label`]); Help > Keyboard Shortcuts renders it.
//!
//! The map is our own, not the engine's Pro Tools strings. Where Ableton and Cubase disagree,
//! the Ableton binding wins; each such choice is noted on its row. Logic keys stay
//! where neither Ableton nor Cubase has one for the action.
//!
//! Matching is exact: a row fires only when the held modifiers are the row's, no more (egui's own
//! `consume_key` ignores extra Shift and Alt, which let the piano roll's plain arrows swallow
//! Alt+Up and Ctrl+Alt+C act as Copy). Same key in two scopes is fine: the narrower handler runs
//! first while its pane has the keyboard (piano roll, browser drag, dialogs).

use egui::{Key, Modifiers};

/// Where a shortcut applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub enum Scope {
    /// Anywhere, unless a text field or dialog has the keyboard.
    Global,
    /// The arrangement and track list (also the selection the mixer shares).
    Arrangement,
    /// The piano roll, while it is open and active.
    PianoRoll,
    /// The piano roll while Step entry is on (its keys shadow the notes' Delete and the digits).
    Step,
    /// The browser (its drag, its Add Folder field).
    Browser,
    /// Mixer strips and the device chain (the control under the pointer, a readout being typed).
    Mixer,
    /// Modal questions (save changes, delete tracks).
    Dialog,
    /// A track or clip name being typed in place (slice 6).
    Rename,
    /// Automation lanes with points selected (slice 7).
    Automation,
}

impl Scope {
    pub const ALL: [Scope; 9] = [
        Scope::Global,
        Scope::Arrangement,
        Scope::PianoRoll,
        Scope::Step,
        Scope::Automation,
        Scope::Browser,
        Scope::Mixer,
        Scope::Dialog,
        Scope::Rename,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "Global",
            Scope::Arrangement => "Arrangement",
            Scope::PianoRoll => "Piano roll",
            Scope::Step => "Step entry",
            Scope::Browser => "Browser",
            Scope::Mixer => "Mixer",
            Scope::Dialog => "Dialogs",
            Scope::Rename => "Name field",
            Scope::Automation => "Automation",
        }
    }
}

/// What a shortcut does. One action may have several rows (several keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    PlayStop,
    Record,
    Rename,
    RenameApply,
    RenameCancel,
    Bounce,
    UndoHistory,
    NewAudioTrack,
    NewInstrumentTrack,
    ReturnToZero,
    Cycle,
    New,
    Open,
    Save,
    SaveAs,
    Undo,
    Redo,
    ToggleMixer,
    ToggleDevices,
    FullMixer,
    ToggleBrowser,
    TogglePianoRoll,
    ClearClipIndicators,
    Copy,
    Cut,
    Paste,
    Duplicate,
    Split,
    Delete,
    SelectAll,
    SelectNone,
    ZoomIn,
    ZoomOut,
    Group,
    Ungroup,
    TrackUp,
    TrackDown,
    ResetControl,
    ReadoutApply,
    ReadoutCancel,
    NotesClose,
    NotesSelectAll,
    NotesDuplicate,
    NotesQuantize,
    NotesTransposeUp,
    NotesTransposeDown,
    NotesOctaveUp,
    NotesOctaveDown,
    NotesUp,
    NotesDown,
    NotesEarlier,
    NotesLater,
    NotesDelete,
    StepToggle,
    StepRest,
    StepBack,
    StepWhole,
    StepHalf,
    StepQuarter,
    StepEighth,
    StepSixteenth,
    StepThirtySecond,
    BrowserCancel,
    BrowserAddFolder,
    DialogConfirm,
    DialogCancel,
    ToggleAutomation,
    GridNarrow,
    GridWiden,
    GridTriplet,
    GridSnap,
    DeletePoints,
    DeselectPoints,
}

/// One row: an action on a key in a scope.
#[derive(Debug, Clone, Copy)]
pub struct Shortcut {
    pub action: Action,
    pub label: &'static str,
    pub mods: Modifiers,
    pub key: Key,
    pub scope: Scope,
    /// Where the key comes from, and the choice made when DAWs disagree.
    pub note: &'static str,
}

const NONE: Modifiers = Modifiers::NONE;
const CMD: Modifiers = Modifiers::COMMAND;
const ALT: Modifiers = Modifiers::ALT;
const SHIFT: Modifiers = Modifiers::SHIFT;
const CMD_SHIFT: Modifiers = Modifiers { shift: true, ..Modifiers::COMMAND };
const CMD_ALT: Modifiers = Modifiers { alt: true, ..Modifiers::COMMAND };

const fn row(action: Action, label: &'static str, mods: Modifiers, key: Key, scope: Scope, note: &'static str) -> Shortcut {
    Shortcut { action, label, mods, key, scope, note }
}

use Action as A;
use Scope as S;

/// The table. The first row of an action is the one its menu shows.
pub const TABLE: &[Shortcut] = &[
    // Transport.
    row(A::PlayStop, "Play / Stop", NONE, Key::Space, S::Global, "All DAWs."),
    row(
        A::Record,
        "Record",
        NONE,
        Key::R,
        S::Global,
        "Logic. Cubase records on numpad *, which egui does not tell apart from *; Ableton has no bare-key record.",
    ),
    row(A::Record, "Record", NONE, Key::F9, S::Global, "Ableton and Cubase. Ctrl+R is Rename (Ableton) since slice 6; Logic records on it."),
    row(A::NewAudioTrack, "New Audio Track", CMD, Key::T, S::Global, "Ableton (Insert Audio Track)."),
    row(A::NewInstrumentTrack, "New Instrument Track", CMD_SHIFT, Key::T, S::Global, "Ableton (Insert MIDI Track)."),
    row(A::ReturnToZero, "Return to Zero", NONE, Key::Home, S::Global, "Ableton and Cubase."),
    row(
        A::ReturnToZero,
        "Return to Zero",
        NONE,
        Key::Enter,
        S::Global,
        "Logic's Return. Cubase's numpad Enter plays instead; Ableton has none, so Logic's stays.",
    ),
    row(
        A::Cycle,
        "Cycle On / Off",
        NONE,
        Key::C,
        S::Global,
        "Logic. Ableton's Ctrl+L loops the selection and Cubase uses numpad /; our cycle is the selection, so one key toggles it.",
    ),
    // File.
    row(A::New, "New", CMD, Key::N, S::Global, "All DAWs."),
    row(A::Open, "Open…", CMD, Key::O, S::Global, "All DAWs."),
    row(A::SaveAs, "Save As…", CMD_SHIFT, Key::S, S::Global, "All DAWs. Checked before Save."),
    row(A::Save, "Save", CMD, Key::S, S::Global, "All DAWs."),
    row(A::Bounce, "Bounce…", CMD_SHIFT, Key::R, S::Global, "Ableton (Export Audio/Video). Checked before Rename."),
    // Undo.
    row(A::Redo, "Redo", CMD_SHIFT, Key::Z, S::Global, "Cubase and Logic. Checked before Undo."),
    row(A::Redo, "Redo", CMD, Key::Y, S::Global, "Ableton on Windows."),
    row(A::Undo, "Undo", CMD, Key::Z, S::Global, "All DAWs."),
    row(A::UndoHistory, "Undo History", ALT, Key::Z, S::Global, "Logic; Ableton and Cubase have no key for it."),
    // Views.
    row(A::ToggleBrowser, "Browser", CMD_ALT, Key::B, S::Global, "Ableton."),
    row(A::ToggleMixer, "Mixer", CMD_ALT, Key::M, S::Global, "Ableton (mixer section). Cubase's F3 opens the full mixer below."),
    row(A::ToggleDevices, "Devices", CMD_ALT, Key::L, S::Global, "Ableton (detail view)."),
    row(A::FullMixer, "Full Mixer Window", NONE, Key::F3, S::Global, "Cubase (MixConsole); Ableton has no key for it."),
    row(
        A::TogglePianoRoll,
        "Piano Roll",
        CMD_ALT,
        Key::K,
        S::Global,
        "Ours; Ableton toggles its clip view with Shift+Tab, which egui keeps for focus.",
    ),
    row(A::ClearClipIndicators, "Clear Clip Indicators", ALT, Key::C, S::Global, "SoundCraft's. Checked before Cycle (C)."),
    // Arrangement editing.
    row(A::Copy, "Copy", CMD, Key::C, S::Arrangement, "All DAWs."),
    row(A::Cut, "Cut", CMD, Key::X, S::Arrangement, "All DAWs."),
    row(A::Paste, "Paste at Cursor", CMD, Key::V, S::Arrangement, "All DAWs; pastes at the edit cursor."),
    row(A::Duplicate, "Duplicate", CMD, Key::D, S::Arrangement, "Ableton and Cubase agree."),
    row(A::Split, "Split at Cursor", CMD, Key::E, S::Arrangement, "Ableton wins over Cubase (Alt+X) and Logic (T)."),
    row(A::Delete, "Delete", NONE, Key::Delete, S::Arrangement, "Clips; tracks only when picked by their header (Ableton)."),
    row(A::Delete, "Delete", NONE, Key::Backspace, S::Arrangement, "Same as Delete (laptops)."),
    row(
        A::SelectAll,
        "Select All Clips",
        CMD,
        Key::A,
        S::Arrangement,
        "Ableton. The piano roll's own Ctrl+A (notes) wins while it is active; Delete after it removes clips, never tracks.",
    ),
    row(A::SelectNone, "Select None", NONE, Key::Escape, S::Arrangement, "Ours."),
    row(
        A::Rename,
        "Rename",
        CMD,
        Key::R,
        S::Arrangement,
        "Ableton: the selected clip, else the selected track. Logic records on Ctrl+R; R and F9 record here.",
    ),
    row(A::Rename, "Rename", NONE, Key::F2, S::Arrangement, "Windows' rename key; Cubase and Logic have none. Same as Ctrl+R."),
    row(A::RenameApply, "Apply Name", NONE, Key::Enter, S::Rename, "Also a click elsewhere."),
    row(A::RenameCancel, "Cancel Rename", NONE, Key::Escape, S::Rename, "All DAWs."),
    row(A::ZoomIn, "Zoom In", NONE, Key::Plus, S::Arrangement, "Ableton wins over Cubase (H)."),
    row(A::ZoomIn, "Zoom In", NONE, Key::Equals, S::Arrangement, "The unshifted + key."),
    row(A::ZoomIn, "Zoom In", SHIFT, Key::Plus, S::Arrangement, "The + key where it takes Shift."),
    row(A::ZoomOut, "Zoom Out", NONE, Key::Minus, S::Arrangement, "Ableton wins over Cubase (G)."),
    row(A::Ungroup, "Ungroup", CMD_SHIFT, Key::G, S::Arrangement, "Ableton. Checked before Group."),
    row(A::Group, "Group Tracks", CMD, Key::G, S::Arrangement, "Ableton (Logic's Cmd+Shift+D makes a stack instead)."),
    row(A::TrackUp, "Move Track Up", ALT, Key::ArrowUp, S::Arrangement, "Ours."),
    row(A::TrackDown, "Move Track Down", ALT, Key::ArrowDown, S::Arrangement, "Ours."),
    row(
        A::ToggleAutomation,
        "Show Automation",
        NONE,
        Key::A,
        S::Arrangement,
        "Ableton (Automation Mode); here for the focused track. Logic's A shows all tracks' lanes.",
    ),
    row(A::GridNarrow, "Narrow Grid", CMD, Key::Num1, S::Arrangement, "Ableton. Pins the grid one division finer than the one in force."),
    row(A::GridWiden, "Widen Grid", CMD, Key::Num2, S::Arrangement, "Ableton. Pins the grid one division coarser."),
    row(A::GridTriplet, "Triplet Grid", CMD, Key::Num3, S::Arrangement, "Ableton. A 1/8 triplet grid is a third of a beat."),
    row(A::GridSnap, "Snap to Grid On / Off", CMD, Key::Num4, S::Arrangement, "Ableton. Shift still bypasses the snap for one drag."),
    // Mixer and device chain.
    row(A::ResetControl, "Reset Control Under Pointer", NONE, Key::Delete, S::Mixer, "Ableton (Delete resets a hovered control)."),
    row(A::ResetControl, "Reset Control Under Pointer", NONE, Key::Backspace, S::Mixer, "Same as Delete."),
    row(A::ReadoutApply, "Apply Typed Value", NONE, Key::Enter, S::Mixer, "While a readout is being typed."),
    row(A::ReadoutCancel, "Cancel Typing", NONE, Key::Escape, S::Mixer, "While a readout is being typed."),
    // Piano roll.
    row(A::NotesClose, "Close Piano Roll", NONE, Key::Escape, S::PianoRoll, "Ours; first cancels a gesture."),
    row(A::NotesSelectAll, "Select All Notes", CMD, Key::A, S::PianoRoll, "All DAWs."),
    row(A::NotesDuplicate, "Duplicate Notes", CMD, Key::D, S::PianoRoll, "Ableton."),
    row(A::NotesQuantize, "Quantize", CMD, Key::U, S::PianoRoll, "Ableton wins over Cubase (Q)."),
    row(A::NotesTransposeUp, "Transpose Up an Octave", CMD, Key::ArrowUp, S::PianoRoll, "Cubase; checked before the arrow nudges."),
    row(A::NotesTransposeDown, "Transpose Down an Octave", CMD, Key::ArrowDown, S::PianoRoll, "Cubase; checked before the arrow nudges."),
    row(A::NotesOctaveUp, "Move Up an Octave", SHIFT, Key::ArrowUp, S::PianoRoll, "Ableton."),
    row(A::NotesOctaveDown, "Move Down an Octave", SHIFT, Key::ArrowDown, S::PianoRoll, "Ableton."),
    row(A::NotesUp, "Move Up a Semitone", NONE, Key::ArrowUp, S::PianoRoll, "All DAWs."),
    row(A::NotesDown, "Move Down a Semitone", NONE, Key::ArrowDown, S::PianoRoll, "All DAWs."),
    row(A::NotesEarlier, "Move Earlier by the Grid", NONE, Key::ArrowLeft, S::PianoRoll, "All DAWs."),
    row(A::NotesLater, "Move Later by the Grid", NONE, Key::ArrowRight, S::PianoRoll, "All DAWs."),
    row(A::NotesDelete, "Delete Notes", NONE, Key::Delete, S::PianoRoll, "All DAWs."),
    row(A::NotesDelete, "Delete Notes", NONE, Key::Backspace, S::PianoRoll, "Same as Delete."),
    row(
        A::StepToggle,
        "Step Entry",
        NONE,
        Key::S,
        S::PianoRoll,
        "Ours (Logic's step input is a bar button; Ableton has none). MIDI notes then enter at the cursor.",
    ),
    row(
        A::StepRest,
        "Step Rest",
        NONE,
        Key::Period,
        S::Step,
        "Ours; Space stays Play/Stop, and Tab moves egui's focus (it then blocks every shortcut), so it cannot be the rest key.",
    ),
    row(A::StepBack, "Step Back (Delete the Last Step)", NONE, Key::Backspace, S::Step, "Ours; wins over Delete Notes while Step is on."),
    row(A::StepWhole, "Step Length: Whole", NONE, Key::Num1, S::Step, "Ours."),
    row(A::StepHalf, "Step Length: Half", NONE, Key::Num2, S::Step, "Ours."),
    row(A::StepQuarter, "Step Length: Quarter", NONE, Key::Num3, S::Step, "Ours."),
    row(A::StepEighth, "Step Length: Eighth", NONE, Key::Num4, S::Step, "Ours."),
    row(A::StepSixteenth, "Step Length: Sixteenth", NONE, Key::Num5, S::Step, "Ours."),
    row(A::StepThirtySecond, "Step Length: Thirty-Second", NONE, Key::Num6, S::Step, "Ours."),
    // Automation lanes (slice 7): only while points are selected, before the arrangement's Delete.
    row(A::DeletePoints, "Delete Selected Points", NONE, Key::Delete, S::Automation, "All DAWs. Without selected points Delete takes clips."),
    row(A::DeletePoints, "Delete Selected Points", NONE, Key::Backspace, S::Automation, "Same as Delete."),
    row(A::DeselectPoints, "Deselect Points", NONE, Key::Escape, S::Automation, "Ours; a click on a clip or empty lane does the same."),
    // Browser.
    row(A::BrowserCancel, "Cancel Drag, Add Folder or Import", NONE, Key::Escape, S::Browser, "Ours."),
    row(A::BrowserAddFolder, "Add the Typed Folder", NONE, Key::Enter, S::Browser, "In the Add Folder field."),
    // Dialogs.
    row(A::DialogConfirm, "Confirm", NONE, Key::Enter, S::Dialog, "Delete Tracks."),
    row(A::DialogCancel, "Cancel", NONE, Key::Escape, S::Dialog, "Every dialog."),
];

/// The rows of `action`, in table order.
pub fn rows(action: Action) -> impl Iterator<Item = &'static Shortcut> {
    TABLE.iter().filter(move |s| s.action == action)
}

/// Consume a press of any of `action`'s keys. The held modifiers must be exactly the row's.
pub fn pressed(ctx: &egui::Context, action: Action) -> bool {
    ctx.input_mut(|i| rows(action).fold(false, |hit, s| hit | (i.modifiers.matches_exact(s.mods) && i.consume_key(s.mods, s.key))))
}

/// Was one of `action`'s keys pressed this frame (not consumed; modifiers ignored, for fields
/// and dialogs that read Enter and Escape).
pub fn hit(i: &egui::InputState, action: Action) -> bool {
    rows(action).any(|s| i.key_pressed(s.key))
}

fn key_name(k: Key) -> &'static str {
    match k {
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::Plus => "+",
        Key::Minus => "-",
        Key::Equals => "=",
        other => other.name(),
    }
}

/// `Ctrl+Shift+Z`: a row's keys as the menus and the shortcuts window show them.
pub fn text(s: &Shortcut) -> String {
    let mut out = String::new();
    if s.mods.command || s.mods.ctrl || s.mods.mac_cmd {
        out.push_str("Ctrl+");
    }
    if s.mods.alt {
        out.push_str("Alt+");
    }
    if s.mods.shift {
        out.push_str("Shift+");
    }
    // A bare "+" reads fine, but "Shift++" does not: with a modifier the key is spelled out.
    let any_mod = s.mods.command || s.mods.ctrl || s.mods.mac_cmd || s.mods.alt || s.mods.shift;
    out.push_str(if any_mod && s.key == Key::Plus { "Plus" } else { key_name(s.key) });
    out
}

/// The menu text of `action`: its first row's keys.
pub fn label(action: Action) -> String {
    rows(action).next().map(text).unwrap_or_default()
}

/// The table filtered by `query` (action, keys, scope or note; case-insensitive), by scope.
pub fn filtered(query: &str) -> Vec<(Scope, Vec<&'static Shortcut>)> {
    let q = query.trim().to_lowercase();
    Scope::ALL
        .iter()
        .map(|sc| {
            let rows = TABLE
                .iter()
                .filter(|s| s.scope == *sc)
                .filter(|s| {
                    q.is_empty()
                        || [s.label.to_lowercase(), text(s).to_lowercase(), sc.label().to_lowercase(), s.note.to_lowercase()]
                            .iter()
                            .any(|t| t.contains(&q))
                })
                .collect::<Vec<_>>();
            (*sc, rows)
        })
        .filter(|(_, r)| !r.is_empty())
        .collect()
}
