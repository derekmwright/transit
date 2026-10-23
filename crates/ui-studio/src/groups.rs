//! Track groups and reordering (slice 2c).
//!
//! A group is the engine's routing folder (`TrackKind::Folder`, "Routing Folder Track"): it sums
//! its members through its own bus, so effects on the group's inserts are group effects. Ctrl+G
//! makes one from the selected tracks ("Group n"), Ctrl+Shift+G dissolves it and keeps the members
//! where they were. Dragging a track header reorders it, drops it into a group (onto the group's
//! header or between its members) or out of one; Alt+Up/Down moves the selected tracks one place
//! among their siblings. A group moves with its members.
//!
//! Every change goes through `Engine::execute`. A gesture that takes several commands
//! (`track.move` per displaced track, membership changes, ungrouping) is one undo step: the
//! commands run with `execute_merged` under a key of their own.
//!
//! Whether a group is collapsed is view state ([`crate::UiState::collapsed`]), never the
//! document: the engine's `track.folder_toggle` writes `Track.folder_open` into the session and
//! dirties it, so this UI does not call it (plan 06, leak list).

use crate::StudioApp;
use crate::arrange::{R4, TrackRow};
use serde_json::{Value, json};
use soundcraft_engine::EngineError;
use soundcraft_model::{Route, Session, Track, TrackId, TrackKind};
use std::collections::HashMap;

/// Indent per group level in the track list (points).
pub const INDENT: f32 = 14.0;

/// Where a dragged track header would land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Drop {
    /// Between rows: a member of `parent` (top level when `None`), placed before `before` (last
    /// when `None`; at the top level that is still above a master track).
    Gap { parent: Option<TrackId>, before: Option<TrackId> },
    /// Onto a group's header: the group's last member.
    Into(TrackId),
}

/// A track header being dragged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeaderDrag {
    pub track: TrackId,
    pub drop: Option<Drop>,
}

/// The group holding `t`, if that track exists.
pub fn parent_of(s: &Session, t: &Track) -> Option<TrackId> {
    t.folder.filter(|f| *f != t.id && s.track(*f).is_some())
}

/// How many groups `t` sits in.
pub fn depth(s: &Session, t: &Track) -> usize {
    let mut n = 0;
    let mut cur = parent_of(s, t);
    while let Some(f) = cur {
        n += 1;
        // A cyclic chain in a hostile file stops here.
        if n >= 16 {
            break;
        }
        cur = s.track(f).and_then(|x| parent_of(s, x));
    }
    n
}

/// Whether `t` is `root` or sits in it, at any depth.
pub fn within(s: &Session, t: TrackId, root: TrackId) -> bool {
    let mut cur = Some(t);
    for _ in 0..64 {
        match cur {
            Some(x) if x == root => return true,
            Some(x) => cur = s.track(x).and_then(|x| parent_of(s, x)),
            None => return false,
        }
    }
    false
}

/// Each group's members (top level under `None`), in session order.
fn tree(s: &Session) -> HashMap<Option<TrackId>, Vec<TrackId>> {
    let mut m: HashMap<Option<TrackId>, Vec<TrackId>> = HashMap::new();
    for t in &s.tracks {
        m.entry(parent_of(s, t)).or_default().push(t.id);
    }
    m
}

/// The session order the tree stands for: each group followed by its members. Tracks the walk
/// cannot reach (a cyclic chain) keep their place at the end.
fn flatten(s: &Session, tree: &HashMap<Option<TrackId>, Vec<TrackId>>) -> Vec<TrackId> {
    fn walk(tree: &HashMap<Option<TrackId>, Vec<TrackId>>, at: Option<TrackId>, out: &mut Vec<TrackId>, depth: usize) {
        for t in tree.get(&at).into_iter().flatten() {
            if out.contains(t) || depth > 16 {
                continue;
            }
            out.push(*t);
            walk(tree, Some(*t), out, depth + 1);
        }
    }
    let mut out = Vec::with_capacity(s.tracks.len());
    walk(tree, None, &mut out, 0);
    for t in &s.tracks {
        if !out.contains(&t.id) {
            out.push(t.id);
        }
    }
    out
}

impl StudioApp {
    /// Whether group `f` is collapsed (view state).
    pub fn collapsed(&self, f: TrackId) -> bool {
        self.ui.collapsed.contains(&f)
    }

    /// Whether `t` is out of sight because a group it sits in is collapsed.
    pub fn tucked(&self, s: &Session, t: &Track) -> bool {
        let mut cur = parent_of(s, t);
        for _ in 0..64 {
            match cur {
                Some(f) if self.collapsed(f) => return true,
                Some(f) => cur = s.track(f).and_then(|x| parent_of(s, x)),
                None => return false,
            }
        }
        false
    }

    /// The disclosure triangle: collapse or expand a group. View state only: no undo step, and
    /// the document stays clean.
    pub fn toggle_group(&mut self, f: TrackId) {
        if let Some(i) = self.ui.collapsed.iter().position(|x| *x == f) {
            self.ui.collapsed.remove(i);
            return;
        }
        self.ui.collapsed.push(f);
        // The members just tucked away leave the selection, so Delete and the arrow keys can't
        // reach what the user can't see; focus follows to the group when it was on one of them.
        let s = self.engine.session_arc();
        let hidden = |t: TrackId| t != f && within(&s, t, f);
        let sel: Vec<TrackId> = s.edit.selected_tracks.clone();
        let keep: Vec<u64> = sel.iter().filter(|t| !hidden(**t)).map(|t| t.0).collect();
        let focus = self.focus_track();
        let focus_hidden = focus.is_some_and(hidden);
        if keep.len() != sel.len() || focus_hidden {
            self.tracks_selected_by_header = false;
            let visible = focus.filter(|t| !hidden(*t)).unwrap_or(f);
            if keep.is_empty() {
                self.focus(visible);
            } else {
                let _ = self.run("edit.select", json!({"tracks": keep, "exact": true}));
                if focus_hidden {
                    self.ui.focus_track = Some(f);
                }
            }
        }
        if self.ui.cursor_track.is_some_and(hidden) {
            self.ui.cursor_track = Some(f);
        }
    }

    /// Run `f` as one undo step; when it fails, take back what it did.
    pub(crate) fn composite<T>(&mut self, what: &str, f: impl FnOnce(&mut StudioApp, &str) -> Result<T, EngineError>) -> Result<T, String> {
        let before = self.engine.session_arc();
        self.arrange.serial += 1;
        let key = format!("group:{what}:{}", self.arrange.serial);
        self.engine.end_merge();
        let r = f(self, &key);
        self.engine.end_merge();
        r.map_err(|e| {
            if !std::sync::Arc::ptr_eq(&before, &self.engine.session_arc()) {
                self.engine.undo();
            }
            self.status = format!("{what}: {e}");
            self.status.clone()
        })
    }

    /// Put the tracks in `order` (a permutation of the session's tracks) with `track.move`, one
    /// command per track out of place, under merge `key`.
    fn reorder(&mut self, order: &[TrackId], key: &str) -> Result<usize, EngineError> {
        let mut n = 0;
        for (i, id) in order.iter().enumerate() {
            if self.engine.session().track_index(*id).is_some_and(|at| at != i) {
                self.engine.execute_merged("track.move", &json!({"track": id.0, "to": i}), key)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// The tracks Group and the arrow keys act on: the selected tracks, else the focused one.
    fn chosen_tracks(&self) -> Vec<TrackId> {
        let s = self.engine.session();
        let sel: Vec<TrackId> = s.edit.selected_tracks.iter().copied().filter(|t| s.track(*t).is_some()).collect();
        if sel.is_empty() { self.focus_track().into_iter().collect() } else { sel }
    }

    /// Ctrl+G: group the selected tracks.
    pub fn group_selected(&mut self) -> Result<TrackId, String> {
        let tracks = self.chosen_tracks();
        self.group_tracks(&tracks)
    }

    /// Track > New Group Track, the header menu's entry and "+ Group": an empty folder ("Group n"),
    /// appended like every other new track, selected and focused. One undo step.
    pub fn new_group_track(&mut self) -> Result<TrackId, String> {
        let s = self.engine.session_arc();
        let name = (1..).map(|i| format!("Group {i}")).find(|n| s.track_by_name(n).is_none()).unwrap_or_else(|| "Group".into());
        let r = self.add_track_with(json!({"kind": "folder", "name": name}))?;
        let id = TrackId(r.get("tracks").and_then(|t| t.get(0)).and_then(Value::as_u64).unwrap_or(0));
        if self.engine.session().track(id).is_none() {
            return Err("New Group Track: the engine made no track".into());
        }
        self.ui.collapsed.retain(|x| *x != id);
        self.focus(id);
        self.status = format!("Added {name}");
        Ok(id)
    }

    /// Track > Edit Group: the Pro Tools style edit/mix group (`track.group`) over `tracks`,
    /// named "Group n" by the engine. Not the folder `Group Tracks` makes.
    pub fn edit_group(&mut self, tracks: &[TrackId]) -> Result<Value, String> {
        if tracks.is_empty() {
            self.status = "Edit Group: select the tracks first".into();
            return Err(self.status.clone());
        }
        let list: Vec<u64> = tracks.iter().map(|t| t.0).collect();
        let r = self.run("track.group", json!({"tracks": list, "edit": true, "mix": true}))?;
        self.status = format!("Edit group of {} track{}", list.len(), if list.len() == 1 { "" } else { "s" });
        Ok(r)
    }

    /// Make a group ("Group n") of `tracks`, placed where the first of them was, as one undo step.
    /// Groups are one level deep: members of another group leave it first. When the selection
    /// holds a group's header, the other tracks join that group instead of nesting it; two
    /// headers are refused ("Groups are one level deep"). Afterwards every group's members sit
    /// directly after its header.
    pub fn group_tracks(&mut self, tracks: &[TrackId]) -> Result<TrackId, String> {
        let s = self.engine.session_arc();
        let ids: Vec<TrackId> = s.tracks.iter().filter(|t| tracks.contains(&t.id) && t.kind != TrackKind::Master).map(|t| t.id).collect();
        if ids.is_empty() {
            self.status = "Group Tracks: select the tracks to group first".into();
            return Err(self.status.clone());
        }
        let headers: Vec<TrackId> = ids.iter().copied().filter(|t| s.track(*t).is_some_and(Track::is_folder)).collect();
        if headers.len() > 1 {
            self.status = "Groups are one level deep".into();
            return Err(self.status.clone());
        }
        if let Some(&g) = headers.first() {
            let join: Vec<u64> = ids.iter().filter(|t| **t != g && s.track(**t).is_some_and(|x| parent_of(&s, x) != Some(g))).map(|t| t.0).collect();
            let name = s.track(g).map_or_else(String::new, |t| t.name.clone());
            if join.is_empty() {
                self.status = format!("{name}: select the tracks to add first");
                return Err(self.status.clone());
            }
            let n = join.len();
            self.composite("Group Tracks", |app, key| {
                app.engine.execute_merged("track.move_to_folder", &json!({"tracks": join, "folder": g.0}), key)?;
                app.normalize(key)
            })?;
            self.focus(g);
            self.status = format!("Added {n} track{} to {name}", if n == 1 { "" } else { "s" });
            return Ok(g);
        }
        let nested: Vec<u64> = ids.iter().filter(|t| s.track(**t).and_then(|t| parent_of(&s, t)).is_some()).map(|t| t.0).collect();
        let name = (1..).map(|i| format!("Group {i}")).find(|n| s.track_by_name(n).is_none()).unwrap_or_else(|| "Group".into());
        let list: Vec<u64> = ids.iter().map(|t| t.0).collect();
        let n = list.len();
        let folder = self.composite("Group Tracks", |app, key| {
            if !nested.is_empty() {
                app.engine.execute_merged("track.remove_from_folder", &json!({"tracks": nested}), key)?;
            }
            let r = app.engine.execute_merged("track.move_to_new_folder", &json!({"tracks": list, "name": name}), key)?;
            app.normalize(key)?;
            Ok(TrackId(r.get("folder").and_then(Value::as_u64).unwrap_or(0)))
        })?;
        self.ui.collapsed.retain(|x| *x != folder);
        self.focus(folder);
        self.status = format!("Grouped {n} track{} as {name}", if n == 1 { "" } else { "s" });
        Ok(folder)
    }

    /// Put every group's members directly after its header (the session order the tree stands
    /// for), keeping the order within each level.
    fn normalize(&mut self, key: &str) -> Result<usize, EngineError> {
        let s = self.engine.session_arc();
        let order = flatten(&s, &tree(&s));
        self.reorder(&order, key)
    }

    /// The groups Ungroup dissolves: the selected groups, or the group of a selected member;
    /// else the focused track's.
    pub fn ungroup_targets(&self) -> Vec<TrackId> {
        let s = self.engine.session();
        let mut out = Vec::new();
        for t in self.chosen_tracks() {
            let Some(tr) = s.track(t) else { continue };
            let g = if tr.is_folder() { Some(t) } else { parent_of(s, tr) };
            if let Some(g) = g.filter(|g| !out.contains(g)) {
                out.push(g);
            }
        }
        out
    }

    /// Ctrl+Shift+G: dissolve the chosen groups.
    pub fn ungroup_selected(&mut self) -> Result<Value, String> {
        let groups = self.ungroup_targets();
        if groups.is_empty() {
            self.status = "Ungroup: select a group first".into();
            return Err(self.status.clone());
        }
        self.ungroup(&groups)
    }

    /// Dissolve `groups`, keeping their members in place, as one undo step: the members leave
    /// (`track.remove_from_folder`, which routes them back to where the group went), the group
    /// track goes (`track.delete`), and so does its bus when nothing else uses it
    /// (`mix.remove_bus`). The members end up selected.
    pub fn ungroup(&mut self, groups: &[TrackId]) -> Result<Value, String> {
        let s = self.engine.session_arc();
        let groups: Vec<TrackId> = groups.iter().copied().filter(|g| s.track(*g).is_some_and(Track::is_folder)).collect();
        if groups.is_empty() {
            return Err("Ungroup: not a group".into());
        }
        let members: Vec<TrackId> = s.tracks.iter().filter(|t| parent_of(&s, t).is_some_and(|p| groups.contains(&p))).map(|t| t.id).collect();
        let name =
            if groups.len() == 1 { s.track(groups[0]).map_or_else(String::new, |g| g.name.clone()) } else { format!("{} groups", groups.len()) };
        let (out, rerouted) = self.composite("Ungroup", |app, key| app.dissolve(&groups, key))?;
        self.ui.collapsed.retain(|x| !groups.contains(x));
        if let Some(first) = members.first() {
            let list: Vec<u64> = members.iter().map(|t| t.0).collect();
            let _ = self.run("edit.select", json!({"tracks": list}));
            self.ui.focus_track = Some(*first);
        }
        self.status = format!("Ungrouped {name}{}", reroute_note(&rerouted));
        Ok(out)
    }

    /// The engine steps of dissolving `groups` under merge `key`: the members leave
    /// (`track.remove_from_folder`, which routes them back to where the group went), the group
    /// tracks go (`track.delete`), a bus nobody uses goes (`mix.remove_bus`), and the order is
    /// restored. A bus that outside tracks still feed is kept, and they are rerouted to Main so
    /// nothing goes silent. Returns the delete result and the names of the rerouted tracks.
    pub(crate) fn dissolve(&mut self, groups: &[TrackId], key: &str) -> Result<(Value, Vec<String>), EngineError> {
        let s = self.engine.session_arc();
        let members: Vec<TrackId> = s.tracks.iter().filter(|t| parent_of(&s, t).is_some_and(|p| groups.contains(&p))).map(|t| t.id).collect();
        let busses: Vec<soundcraft_model::BusId> = groups
            .iter()
            .filter_map(|g| s.track(*g))
            .filter_map(|g| match g.mixer.input {
                Route::Bus(b) => Some(b),
                _ => None,
            })
            .collect();
        // Members stay where they were: the session order without the group tracks.
        let order: Vec<TrackId> = s.tracks.iter().map(|t| t.id).filter(|t| !groups.contains(t)).collect();
        let e = &mut self.engine;
        if !members.is_empty() {
            let list: Vec<u64> = members.iter().rev().map(|t| t.0).collect();
            e.execute_merged("track.remove_from_folder", &json!({"tracks": list}), key)?;
        }
        let ids: Vec<u64> = groups.iter().map(|t| t.0).collect();
        let r = e.execute_merged("track.delete", &json!({"tracks": ids}), key)?;
        let mut rerouted = Vec::new();
        for b in &busses {
            let (l, f, n) = e.session().bus_usage(*b);
            if l.is_empty() && f.is_empty() && n.is_empty() {
                e.execute_merged("mix.remove_bus", &json!({"bus": b.0}), key)?;
            } else if !f.is_empty() {
                let list: Vec<u64> = f.iter().map(|t| t.0).collect();
                e.execute_merged("track.output", &json!({"tracks": list, "output": "main"}), key)?;
                rerouted.extend(f.iter().filter_map(|t| e.session().track(*t).map(|t| t.name.clone())));
            }
        }
        self.reorder(&order, key)?;
        Ok((r, rerouted))
    }

    /// Alt+Up/Down: move the chosen tracks one place among their siblings (a group moves with its
    /// members and a track passes a whole group). One undo step. A master track stays put.
    pub fn nudge_tracks(&mut self, up: bool) -> Result<bool, String> {
        let s = self.engine.session_arc();
        let chosen: Vec<TrackId> = self.chosen_tracks().into_iter().filter(|t| s.track(*t).is_some_and(|t| t.kind != TrackKind::Master)).collect();
        if chosen.is_empty() {
            return Ok(false);
        }
        let fixed = |t: TrackId| s.track(t).is_none_or(|t| t.kind == TrackKind::Master);
        let mut tree = tree(&s);
        for kids in tree.values_mut() {
            let n = kids.len();
            for k in 1..n {
                let (a, b) = if up { (k - 1, k) } else { (n - k - 1, n - k) };
                let (mover, other) = if up { (b, a) } else { (a, b) };
                if chosen.contains(&kids[mover]) && !chosen.contains(&kids[other]) && !fixed(kids[other]) {
                    kids.swap(a, b);
                }
            }
        }
        let order = flatten(&s, &tree);
        if order.iter().eq(s.tracks.iter().map(|t| &t.id)) {
            return Ok(false);
        }
        self.composite("Move Track", |app, key| app.reorder(&order, key))?;
        self.status = match chosen.as_slice() {
            [one] => format!("Moved {}", s.track(*one).map_or("track", |t| t.name.as_str())),
            many => format!("Moved {} tracks", many.len()),
        };
        Ok(true)
    }

    /// Finish a header drag: move `t` (with its members, if it is a group) to `drop`, as one undo
    /// step. False when that is where it already is, or a group would land inside itself.
    pub fn drop_track(&mut self, t: TrackId, drop: Drop) -> Result<bool, String> {
        let s = self.engine.session_arc();
        let Some(tr) = s.track(t).filter(|t| t.kind != TrackKind::Master) else { return Ok(false) };
        let from = parent_of(&s, tr);
        let (to, before) = match drop {
            Drop::Gap { parent, before } => (parent, before),
            Drop::Into(f) => (Some(f), None),
        };
        if to.is_some_and(|p| within(&s, p, t) || !s.track(p).is_some_and(Track::is_folder)) || before.is_some_and(|b| within(&s, b, t)) {
            return Ok(false);
        }
        if matches!(drop, Drop::Into(f) if from == Some(f)) {
            return Ok(false);
        }
        let mut tree = tree(&s);
        if let Some(v) = tree.get_mut(&from) {
            v.retain(|x| *x != t);
        }
        let list = tree.entry(to).or_default();
        let master = |x: &TrackId| s.track(*x).is_some_and(|x| x.kind == TrackKind::Master);
        let at = match before.and_then(|b| list.iter().position(|x| *x == b)) {
            Some(i) => i,
            None if to.is_none() => list.iter().position(master).unwrap_or(list.len()),
            None => list.len(),
        };
        list.insert(at, t);
        let order = flatten(&s, &tree);
        if from == to && order.iter().eq(s.tracks.iter().map(|t| &t.id)) {
            return Ok(false);
        }
        let label = match (from, to) {
            (a, b) if a == b => "Move Track",
            (_, Some(_)) => "Move into Group",
            (_, None) => "Move out of Group",
        };
        self.composite(label, |app, key| {
            if from != to {
                match to {
                    Some(f) => {
                        app.engine.execute_merged("track.move_to_folder", &json!({"tracks": [t.0], "folder": f.0}), key)?;
                    }
                    None => {
                        // Out of nested groups one level at a time (the engine's own step).
                        for _ in 0..16 {
                            let still = app.engine.session().track(t).and_then(|x| parent_of(app.engine.session(), x));
                            if still.is_none() {
                                break;
                            }
                            app.engine.execute_merged("track.remove_from_folder", &json!({"tracks": [t.0]}), key)?;
                        }
                    }
                }
            }
            app.reorder(&order, key)
        })?;
        self.status = format!("Moved {}", tr.name);
        Ok(true)
    }

    /// A click on a track header: select and focus it; Shift extends the selection from the last
    /// plainly clicked track over the rows in between (group members included); Ctrl adds or
    /// removes it.
    pub fn click_header(&mut self, t: TrackId, shift: bool, toggle: bool) {
        let s = self.engine.session_arc();
        let anchor = self.arrange.anchor.filter(|a| s.track(*a).is_some()).or_else(|| self.focus_track());
        if shift && let Some(a) = anchor {
            let rows = &self.arrange.order;
            if let (Some(i), Some(j)) = (rows.iter().position(|x| *x == a), rows.iter().position(|x| *x == t)) {
                let range: Vec<u64> = rows[i.min(j)..=i.max(j)].iter().map(|x| x.0).collect();
                let _ = self.run("edit.select", json!({"tracks": range, "exact": true}));
                self.ui.focus_track = Some(t);
                self.tracks_selected_by_header = true;
                return;
            }
        }
        if toggle {
            let mut sel: Vec<TrackId> = s.edit.selected_tracks.clone();
            if let Some(i) = sel.iter().position(|x| *x == t) {
                sel.remove(i);
            } else {
                sel.push(t);
                self.ui.focus_track = Some(t);
            }
            sel.sort_by_key(|x| s.track_index(*x).unwrap_or(usize::MAX));
            let list: Vec<u64> = sel.iter().map(|x| x.0).collect();
            let _ = self.run("edit.select", json!({"tracks": list, "exact": true}));
            self.arrange.anchor = Some(t);
            self.tracks_selected_by_header = !list.is_empty();
            return;
        }
        self.arrange.anchor = Some(t);
        self.focus_header(t, false);
    }
}

/// The status tail naming the tracks rerouted to Main ("; X now outputs to Main").
pub(crate) fn reroute_note(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("; {one} now outputs to Main"),
        many => format!("; {} now output to Main", many.join(", ")),
    }
}

/// Where the pointer at `y` would drop the dragged track `t`, and the indicator to draw: a line
/// between rows, or the outline of the group header it would join.
pub fn drop_at(s: &Session, rows: &[TrackRow], t: TrackId, y: f32) -> Option<(Drop, R4)> {
    let first = rows.first()?;
    let last = rows.last()?;
    let gap = |b: usize| -> Option<(Drop, R4)> {
        // Below a master row reads as above it: the master stays last.
        let b = if b == rows.len() && last.kind == TrackKind::Master { b - 1 } else { b };
        let below = rows.get(b);
        let parent = below.and_then(|r| r.folder).map(TrackId);
        let before = below.map(|r| TrackId(r.id));
        if before == Some(t) || (b > 0 && rows.get(b - 1).is_some_and(|r| r.id == t.0)) && parent == s.track(t).and_then(|x| parent_of(s, x)) {
            return None;
        }
        if before.is_some_and(|x| within(s, x, t)) || parent.is_some_and(|p| within(s, p, t)) {
            return None;
        }
        let ly = below.map_or(last.header[3], |r| r.header[1]);
        let indent = below.filter(|_| parent.is_some()).map_or(0.0, |r| r.depth as f32 * INDENT);
        Some((Drop::Gap { parent, before }, [first.header[0] + indent, ly - 1.0, first.lane[2], ly + 1.0]))
    };
    if y < first.header[1] {
        return gap(0);
    }
    let Some(i) = rows.iter().position(|r| y >= r.header[1] && y < r.header[3]) else { return gap(rows.len()) };
    let r = &rows[i];
    let f = (y - r.header[1]) / (r.header[3] - r.header[1]).max(1.0);
    if r.kind == TrackKind::Folder && (0.25..0.75).contains(&f) && r.id != t.0 && !within(s, TrackId(r.id), t) {
        let joins = s.track(t).and_then(|x| parent_of(s, x)) != Some(TrackId(r.id));
        return joins.then_some((Drop::Into(TrackId(r.id)), r.header));
    }
    gap(if f < 0.5 { i } else { i + 1 })
}

/// The group entries of the Track menu and of a track header's menu (`on`: the header's track;
/// Group takes the selection when `on` is part of it, else `on` alone).
pub fn menu_items(app: &mut StudioApp, ui: &mut egui::Ui, on: Option<TrackId>) {
    let tracks = match on {
        Some(t) if !app.engine.session().edit.selected_tracks.contains(&t) => vec![t],
        _ => app.chosen_tracks(),
    };
    let groups = match on {
        Some(t) if !app.engine.session().edit.selected_tracks.contains(&t) => {
            let s = app.engine.session();
            s.track(t).and_then(|x| if x.is_folder() { Some(t) } else { parent_of(s, x) }).into_iter().collect()
        }
        _ => app.ungroup_targets(),
    };
    let new = ui.button("New Group Track");
    let group =
        ui.add_enabled(!tracks.is_empty(), egui::Button::new("Group Tracks").shortcut_text(crate::shortcuts::label(crate::shortcuts::Action::Group)));
    let ungroup =
        ui.add_enabled(!groups.is_empty(), egui::Button::new("Ungroup").shortcut_text(crate::shortcuts::label(crate::shortcuts::Action::Ungroup)));
    let edit = ui.add_enabled(!tracks.is_empty(), egui::Button::new("Edit Group…"));
    let list = if on.is_some() { &mut app.arrange.menu } else { &mut app.arrange.bar_menu };
    list.retain(|m| !matches!(m.label.as_str(), "Group Tracks" | "Ungroup" | "New Group Track" | "Edit Group…"));
    list.push(crate::mixer::MenuItem::at(ui, "New Group Track".into(), String::new(), new.rect));
    list.push(crate::mixer::MenuItem::at(ui, "Edit Group…".into(), String::new(), edit.rect));
    list.push(crate::mixer::MenuItem::at(ui, "Group Tracks".into(), String::new(), group.rect));
    list.push(crate::mixer::MenuItem::at(ui, "Ungroup".into(), String::new(), ungroup.rect));
    if new.clicked() {
        let _ = app.new_group_track();
        ui.close();
    }
    if edit.clicked() {
        let _ = app.edit_group(&tracks);
        ui.close();
    }
    if group.clicked() {
        let _ = app.group_tracks(&tracks);
        ui.close();
    }
    if ungroup.clicked() {
        let _ = app.ungroup(&groups);
        ui.close();
    }
}

/// Ctrl+G, Ctrl+Shift+G, Alt+Up, Alt+Down (unless a text field has the keyboard).
pub fn shortcuts(app: &mut StudioApp, ctx: &egui::Context) {
    use crate::shortcuts::{Action as A, pressed};
    if ctx.egui_wants_keyboard_input() || app.dialog.is_some() {
        return;
    }
    // Ungroup first: Ctrl+G also matches Ctrl+Shift+G.
    if pressed(ctx, A::Ungroup) {
        let _ = app.ungroup_selected();
    }
    if pressed(ctx, A::Group) {
        let _ = app.group_selected();
    }
    if pressed(ctx, A::TrackUp) {
        let _ = app.nudge_tracks(true);
    }
    if pressed(ctx, A::TrackDown) {
        let _ = app.nudge_tracks(false);
    }
}
