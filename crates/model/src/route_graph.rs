//! The audio routing graph of a [`Session`]: which strips feed which busses, and which busses feed
//! which auxes, down to the main mix.
//!
//! Nodes are the session's tracks (by index), its busses (by index) and the main mix. Edges are:
//!
//! - a track's output to a bus or to the main mix ([`EdgeKind::Output`]),
//! - a track's send to a bus ([`EdgeKind::Send`], pre- or post-fader),
//! - a bus to the aux or routing folder that reads it as its input ([`EdgeKind::Input`]).
//!
//! Master faders and VCA masters are not on the graph: a master processes the main mix in place
//! and a VCA carries no audio. Folder membership does not route audio by itself; a routing folder
//! sums its members only through the bus its members output to, which is an ordinary edge here.
//! Routes to a bus that does not exist, to hardware, or to nothing are not edges (the mix drops
//! them). An audio track's bus input is not an edge either: audio tracks hear hardware input only.
//!
//! Commands use [`would_create_cycle`] to refuse a route that closes a loop (L22). The mix uses
//! [`RouteGraph::layering`] for its render order and delay compensation (F05): a session loaded
//! from an older file can still hold a loop, so `layering` breaks every loop deterministically
//! and reports the edges it cut.

use crate::{Route, Session, Track, TrackId, TrackKind};

/// A node of the routing graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RouteNode {
    /// A track, by its index in [`Session::tracks`].
    Track(usize),
    /// A bus, by its index in [`Session::busses`].
    Bus(usize),
    /// The main mix (the master faders' input).
    Main,
}

/// What an edge of the routing graph is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// The track's main output.
    Output,
    /// The track's send in this slot.
    Send { slot: usize, pre_fader: bool },
    /// The bus is the aux's (or folder's) input.
    Input,
}

/// One edge of the routing graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RouteEdge {
    pub from: RouteNode,
    pub to: RouteNode,
    pub kind: EdgeKind,
}

/// A proposed routing change, checked with [`would_create_cycle`] before it is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteChange {
    /// `track.output`: the track's output becomes `route`.
    Output { track: TrackId, route: Route },
    /// `track.input`: the track's input becomes `route`.
    Input { track: TrackId, route: Route },
    /// `mix.send`: the track's send in `slot` targets `route` (`Route::None` clears it).
    Send { track: TrackId, slot: usize, route: Route },
}

impl RouteChange {
    fn track(&self) -> TrackId {
        match self {
            RouteChange::Output { track, .. } | RouteChange::Input { track, .. } | RouteChange::Send { track, .. } => *track,
        }
    }
}

/// A routing loop: the tracks on it in signal order, starting at the first one (not repeated at
/// the end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCycle {
    pub tracks: Vec<TrackId>,
}

impl RouteCycle {
    /// The loop by track name, closed: `"FX 1 → FX 2 → FX 1"`.
    #[must_use]
    pub fn describe(&self, s: &Session) -> String {
        describe_cycle(s, &self.tracks)
    }
}

/// The loop `tracks` by name, closed by repeating the first: `"FX 1 → FX 2 → FX 1"`.
#[must_use]
pub fn describe_cycle(s: &Session, tracks: &[TrackId]) -> String {
    let name = |id: &TrackId| s.track(*id).map_or_else(|| format!("track {id}"), |t| t.name.clone());
    let mut parts: Vec<String> = tracks.iter().map(name).collect();
    if let Some(first) = tracks.first() {
        parts.push(name(first));
    }
    parts.join(" → ")
}

/// An acyclic view of the graph: every loop broken, and each track's depth.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layering {
    /// Per track index: the longest chain of tracks feeding it through busses (0 for a track
    /// nothing feeds). Every edge that is not cut goes from a smaller depth to a larger one, so
    /// ordering tracks by (depth, index) renders every producer before its consumers.
    pub depth: Vec<usize>,
    /// The edges cut to break loops, in the order they were cut. Empty for an acyclic session.
    pub cut: Vec<RouteEdge>,
    /// The loop each cut broke (same order as `cut`).
    pub loops: Vec<RouteCycle>,
}

impl Layering {
    /// Track indices in render order: by depth, then track index.
    #[must_use]
    pub fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.depth.len()).collect();
        order.sort_by_key(|&i| (self.depth.get(i).copied().unwrap_or(0), i));
        order
    }
}

/// The routing graph of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteGraph {
    ids: Vec<TrackId>,
    busses: usize,
    edges: Vec<RouteEdge>,
}

/// Whether a track of this kind is on the graph.
fn on_graph(t: &Track) -> bool {
    !matches!(t.kind, TrackKind::Master | TrackKind::Vca)
}

/// Whether a track of this kind reads its input bus.
fn reads_bus(t: &Track) -> bool {
    matches!(t.kind, TrackKind::Aux | TrackKind::Folder)
}

impl RouteGraph {
    /// The graph of `s` as it is.
    #[must_use]
    pub fn new(s: &Session) -> Self {
        Self::with_changes(s, &[])
    }

    /// The graph `s` would have after `changes` (applied in order; a later change to the same
    /// route wins). Changes naming a missing track or send slot are ignored.
    #[must_use]
    pub fn with_changes(s: &Session, changes: &[RouteChange]) -> Self {
        let bus = |r: &Route| match r {
            Route::Bus(b) => s.busses.iter().position(|x| x.id == *b).map(RouteNode::Bus),
            Route::Main => Some(RouteNode::Main),
            _ => None,
        };
        let mut edges = Vec::new();
        for (i, t) in s.tracks.iter().enumerate().filter(|(_, t)| on_graph(t)) {
            let mut output = &t.mixer.output;
            let mut input = &t.mixer.input;
            let mut sends: Vec<Option<&Route>> = t.mixer.sends.iter().map(|x| x.as_ref().map(|x| &x.target)).collect();
            for c in changes.iter().filter(|c| c.track() == t.id) {
                match c {
                    RouteChange::Output { route, .. } => output = route,
                    RouteChange::Input { route, .. } => input = route,
                    RouteChange::Send { slot, route, .. } => {
                        if let Some(x) = sends.get_mut(*slot) {
                            *x = Some(route);
                        }
                    }
                }
            }
            if reads_bus(t)
                && let Route::Bus(_) = input
                && let Some(from) = bus(input)
            {
                edges.push(RouteEdge { from, to: RouteNode::Track(i), kind: EdgeKind::Input });
            }
            if let Some(to) = bus(output) {
                edges.push(RouteEdge { from: RouteNode::Track(i), to, kind: EdgeKind::Output });
            }
            for (slot, target) in sends.iter().enumerate() {
                let Some(target @ Route::Bus(_)) = target else { continue };
                let Some(to) = bus(target) else { continue };
                let pre_fader = t.mixer.sends.get(slot).and_then(Option::as_ref).is_some_and(|x| x.pre_fader);
                edges.push(RouteEdge { from: RouteNode::Track(i), to, kind: EdgeKind::Send { slot, pre_fader } });
            }
        }
        RouteGraph { ids: s.tracks.iter().map(|t| t.id).collect(), busses: s.busses.len(), edges }
    }

    /// Every edge, grouped by source track in track order (inputs first, then output, then sends).
    #[must_use]
    pub fn edges(&self) -> &[RouteEdge] {
        &self.edges
    }

    fn node_count(&self) -> usize {
        self.ids.len() + self.busses + 1
    }

    fn index(&self, n: RouteNode) -> usize {
        match n {
            RouteNode::Track(i) => i,
            RouteNode::Bus(b) => self.ids.len() + b,
            RouteNode::Main => self.ids.len() + self.busses,
        }
    }

    fn track_id(&self, n: RouteNode) -> Option<TrackId> {
        match n {
            RouteNode::Track(i) => self.ids.get(i).copied(),
            _ => None,
        }
    }

    /// A path from `from` to `to` over the edges not in `skip` (breadth first, so the shortest),
    /// as the edges taken. `None` when `to` is unreachable.
    fn path(&self, from: RouteNode, to: RouteNode, skip: &[bool]) -> Option<Vec<usize>> {
        let n = self.node_count();
        let mut via: Vec<Option<usize>> = vec![None; n];
        let mut seen = vec![false; n];
        let mut queue = std::collections::VecDeque::new();
        seen[self.index(from)] = true;
        queue.push_back(from);
        while let Some(at) = queue.pop_front() {
            if at == to {
                let mut out = Vec::new();
                let mut cur = to;
                while cur != from {
                    let Some(e) = via[self.index(cur)] else { break };
                    out.push(e);
                    cur = self.edges[e].from;
                }
                out.reverse();
                return Some(out);
            }
            for (k, e) in self.edges.iter().enumerate() {
                if e.from != at || skip.get(k).copied().unwrap_or(false) {
                    continue;
                }
                let j = self.index(e.to);
                if !seen[j] {
                    seen[j] = true;
                    via[j] = Some(k);
                    queue.push_back(e.to);
                }
            }
        }
        None
    }

    /// The tracks on a loop of edges, in signal order.
    fn loop_tracks(&self, edges: &[usize]) -> Vec<TrackId> {
        edges.iter().filter_map(|&e| self.track_id(self.edges[e].from)).collect()
    }

    /// A loop closed by edge `e`, as the loop's edges starting with `e`.
    fn loop_through(&self, e: usize, skip: &[bool]) -> Option<Vec<usize>> {
        let edge = self.edges.get(e)?;
        if edge.from == edge.to {
            return Some(vec![e]);
        }
        let mut rest = self.path(edge.to, edge.from, skip)?;
        rest.insert(0, e);
        Some(rest)
    }

    /// Some loop in the graph, if there is one: the tracks on it in signal order.
    #[must_use]
    pub fn find_cycle(&self) -> Option<RouteCycle> {
        let l = self.layering();
        l.loops.into_iter().next()
    }

    /// Track indices in render order when the graph is acyclic, else the first loop found.
    pub fn topological_order(&self) -> Result<Vec<usize>, RouteCycle> {
        let l = self.layering();
        match l.loops.first() {
            Some(c) => Err(c.clone()),
            None => Ok(l.order()),
        }
    }

    /// Breaks every loop and layers the tracks.
    ///
    /// Loops are broken deterministically: Kahn's algorithm takes every node it can; the nodes
    /// left over all sit on or behind a loop. From the leftover node of the lowest index, walking
    /// back along each node's first leftover input edge reaches a loop. On that loop, the edge cut
    /// is the one that writes into the bus the loop's first track (lowest track index) reads:
    /// that aux keeps every other source on its input, and only the feedback is lost. Repeat
    /// until no node is left over.
    #[must_use]
    pub fn layering(&self) -> Layering {
        let n = self.node_count();
        let mut cut_mask = vec![false; self.edges.len()];
        let mut cut = Vec::new();
        let mut loops = Vec::new();
        loop {
            let (order, left) = self.kahn(&cut_mask);
            if !left.iter().any(|l| *l) {
                let mut depth = vec![0usize; self.ids.len()];
                // Longest chain of tracks: a track's depth is one more than the deepest track
                // feeding a bus it reads.
                let mut node_depth = vec![0usize; n];
                for &v in &order {
                    for (k, e) in self.edges.iter().enumerate() {
                        if cut_mask[k] || self.index(e.from) != v {
                            continue;
                        }
                        let step = usize::from(matches!(e.to, RouteNode::Track(_)));
                        let d = node_depth[v].saturating_add(step);
                        let j = self.index(e.to);
                        node_depth[j] = node_depth[j].max(d);
                    }
                }
                for (i, d) in depth.iter_mut().enumerate() {
                    *d = node_depth[i];
                }
                return Layering { depth, cut, loops };
            }
            let Some(edges) = self.leftover_loop(&left, &cut_mask) else {
                // Unreachable for a finite graph: leftover nodes always contain a loop. Keep the
                // order total anyway by cutting every leftover edge.
                for (k, e) in self.edges.iter().enumerate() {
                    if !cut_mask[k] && left[self.index(e.from)] && left[self.index(e.to)] {
                        cut_mask[k] = true;
                        cut.push(*e);
                    }
                }
                continue;
            };
            let tracks = self.loop_tracks(&edges);
            // The loop's first track (lowest index) and the edge writing into the bus it reads.
            let first = edges
                .iter()
                .enumerate()
                .filter_map(|(pos, &k)| match self.edges[k].to {
                    RouteNode::Track(i) => Some((i, pos)),
                    _ => None,
                })
                .min();
            let victim = match first {
                Some((_, pos)) => {
                    // edges[pos] is bus → track; the edge before it writes into that bus.
                    let before = (pos + edges.len() - 1) % edges.len();
                    edges[before]
                }
                None => edges[0],
            };
            cut_mask[victim] = true;
            cut.push(self.edges[victim]);
            let start = self.track_id(self.edges[victim].from);
            let mut tracks = tracks;
            if let Some(start) = start
                && let Some(p) = tracks.iter().position(|t| *t == start)
            {
                tracks.rotate_left(p);
            }
            loops.push(RouteCycle { tracks });
        }
    }

    /// Kahn's algorithm over the edges not cut, taking ready nodes lowest index first. Returns the
    /// order taken and, per node, whether it was left over.
    fn kahn(&self, cut: &[bool]) -> (Vec<usize>, Vec<bool>) {
        let n = self.node_count();
        let mut indeg = vec![0usize; n];
        for (k, e) in self.edges.iter().enumerate() {
            if !cut[k] {
                indeg[self.index(e.to)] += 1;
            }
        }
        let mut ready: std::collections::BTreeSet<usize> = (0..n).filter(|&v| indeg[v] == 0).collect();
        let mut order = Vec::with_capacity(n);
        let mut done = vec![false; n];
        while let Some(v) = ready.pop_first() {
            order.push(v);
            done[v] = true;
            for (k, e) in self.edges.iter().enumerate() {
                if cut[k] || self.index(e.from) != v {
                    continue;
                }
                let j = self.index(e.to);
                indeg[j] = indeg[j].saturating_sub(1);
                if indeg[j] == 0 {
                    ready.insert(j);
                }
            }
        }
        let left = done.iter().map(|d| !d).collect();
        (order, left)
    }

    /// A loop among the leftover nodes, as its edges in signal order.
    fn leftover_loop(&self, left: &[bool], cut: &[bool]) -> Option<Vec<usize>> {
        let n = self.node_count();
        let start = (0..n).find(|&v| left[v])?;
        // Each leftover node's first leftover input edge.
        let pred =
            |v: usize| self.edges.iter().enumerate().find(|(k, e)| !cut[*k] && self.index(e.to) == v && left[self.index(e.from)]).map(|(k, _)| k);
        let mut seen_at: Vec<Option<usize>> = vec![None; n];
        let mut walk: Vec<usize> = Vec::new();
        let mut v = start;
        loop {
            if let Some(at) = seen_at[v] {
                // walk[at..] are the edges into the loop's nodes, walking backwards.
                let mut edges: Vec<usize> = walk.get(at..)?.to_vec();
                edges.reverse();
                return Some(edges);
            }
            seen_at[v] = Some(walk.len());
            let e = pred(v)?;
            walk.push(e);
            v = self.index(self.edges[e].from);
        }
    }
}

impl Session {
    /// The routing graph of this session.
    #[must_use]
    pub fn route_graph(&self) -> RouteGraph {
        RouteGraph::new(self)
    }
}

/// Whether applying `change` to `s` would close a routing loop. Returns the loop's tracks in
/// signal order, starting at the changed track, or `None` when the change is safe. Loops already
/// in the session (an older file) that the change does not close are not reported.
#[must_use]
pub fn would_create_cycle(s: &Session, change: &RouteChange) -> Option<Vec<TrackId>> {
    would_create_cycle_all(s, std::slice::from_ref(change))
}

/// [`would_create_cycle`] for several changes applied together (one command routing several
/// tracks): the first loop any of them closes.
#[must_use]
pub fn would_create_cycle_all(s: &Session, changes: &[RouteChange]) -> Option<Vec<TrackId>> {
    let before = RouteGraph::new(s);
    let after = RouteGraph::with_changes(s, changes);
    let none = vec![false; after.edges.len()];
    for (k, e) in after.edges.iter().enumerate() {
        // Only edges the changes add can close a new loop.
        if before.edges.contains(e) {
            continue;
        }
        if let Some(edges) = after.loop_through(k, &none) {
            let mut tracks = after.loop_tracks(&edges);
            if tracks.is_empty() {
                // The new edge is an input (bus → track): the loop starts at that track.
                if let Some(t) = after.track_id(e.to) {
                    tracks.push(t);
                }
            }
            // Start at the changed track.
            let changed: Vec<TrackId> = changes.iter().map(RouteChange::track).collect();
            if let Some(p) = tracks.iter().position(|t| changed.contains(t)) {
                tracks.rotate_left(p);
            }
            return Some(tracks);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BusId, ChannelFormat, SendSlot, time::SampleRate};

    fn session() -> Session {
        Session::new("t", SampleRate::HZ_48000)
    }

    fn aux(s: &mut Session, name: &str) -> (TrackId, BusId) {
        let id = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some(name));
        let b = s.add_bus(&format!("{name} In"), ChannelFormat::Stereo);
        if let Some(t) = s.track_mut(id) {
            t.mixer.input = Route::Bus(b);
        }
        (id, b)
    }

    fn set_output(s: &mut Session, t: TrackId, r: Route) {
        if let Some(t) = s.track_mut(t) {
            t.mixer.output = r;
        }
    }

    fn names(s: &Session, ts: &[TrackId]) -> Vec<String> {
        ts.iter().map(|t| s.track(*t).map(|t| t.name.clone()).unwrap_or_default()).collect()
    }

    #[test]
    fn a_chain_is_acyclic_and_ordered_by_depth() {
        let mut s = session();
        let src = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Src"));
        let mut prev = src;
        let mut auxes = Vec::new();
        for i in 0..12 {
            let (a, b) = aux(&mut s, &format!("A{i}"));
            set_output(&mut s, prev, Route::Bus(b));
            prev = a;
            auxes.push(a);
        }
        let g = s.route_graph();
        let order = g.topological_order().expect("acyclic");
        let pos = |id: TrackId| order.iter().position(|&i| s.tracks[i].id == id).expect("in order");
        for w in auxes.windows(2) {
            assert!(pos(w[0]) < pos(w[1]));
        }
        let l = g.layering();
        assert_eq!(l.depth[s.track_index(auxes[11]).expect("track")], 12);
        assert!(l.cut.is_empty());
    }

    #[test]
    fn output_change_that_closes_a_loop_is_reported_with_its_path() {
        let mut s = session();
        let (a, _ab) = aux(&mut s, "FX 1");
        let (b, bb) = aux(&mut s, "FX 2");
        set_output(&mut s, a, Route::Bus(bb));
        let ab = s.track(a).map(|t| t.mixer.input.clone()).expect("a");
        let c = would_create_cycle(&s, &RouteChange::Output { track: b, route: ab }).expect("loop");
        assert_eq!(names(&s, &c), ["FX 2", "FX 1"]);
        assert_eq!(describe_cycle(&s, &c), "FX 2 → FX 1 → FX 2");
        assert!(would_create_cycle(&s, &RouteChange::Output { track: b, route: Route::Main }).is_none());
    }

    #[test]
    fn self_loops_through_output_input_and_send_are_reported() {
        let mut s = session();
        let (a, ab) = aux(&mut s, "FX");
        assert_eq!(would_create_cycle(&s, &RouteChange::Output { track: a, route: Route::Bus(ab) }), Some(vec![a]));
        assert_eq!(would_create_cycle(&s, &RouteChange::Send { track: a, slot: 0, route: Route::Bus(ab) }), Some(vec![a]));
        let other = s.add_bus("Other", ChannelFormat::Stereo);
        set_output(&mut s, a, Route::Bus(other));
        assert_eq!(would_create_cycle(&s, &RouteChange::Input { track: a, route: Route::Bus(other) }), Some(vec![a]));
    }

    #[test]
    fn an_audio_tracks_bus_input_is_not_an_edge() {
        let mut s = session();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Gtr"));
        let b = s.add_bus("B", ChannelFormat::Stereo);
        set_output(&mut s, t, Route::Bus(b));
        assert!(would_create_cycle(&s, &RouteChange::Input { track: t, route: Route::Bus(b) }).is_none());
    }

    #[test]
    fn a_loaded_loop_is_broken_at_the_feedback_into_the_first_aux() {
        let mut s = session();
        let src = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Src"));
        let (a, ab) = aux(&mut s, "A");
        let (b, bb) = aux(&mut s, "B");
        set_output(&mut s, src, Route::Bus(ab));
        set_output(&mut s, a, Route::Bus(bb));
        // B feeds A back through a send: a loop A → B → A.
        if let Some(t) = s.track_mut(b) {
            t.mixer.sends[2] = Some(SendSlot::new(Route::Bus(ab)));
        }
        let g = s.route_graph();
        let err = g.topological_order().expect_err("loop");
        assert_eq!(names(&s, &err.tracks), ["B", "A"]);
        let l = g.layering();
        let bi = s.track_index(b).expect("b");
        assert_eq!(l.cut, vec![RouteEdge { from: RouteNode::Track(bi), to: RouteNode::Bus(0), kind: EdgeKind::Send { slot: 2, pre_fader: false } }]);
        let order: Vec<TrackId> = l.order().into_iter().map(|i| s.tracks[i].id).collect();
        let pos = |id| order.iter().position(|x| *x == id).expect("in order");
        assert!(pos(src) < pos(a) && pos(a) < pos(b));
        // Deterministic: the same session breaks the same way.
        assert_eq!(s.route_graph().layering(), l);
    }

    #[test]
    fn an_existing_loop_does_not_block_unrelated_changes() {
        let mut s = session();
        let (a, ab) = aux(&mut s, "A");
        set_output(&mut s, a, Route::Bus(ab));
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("T"));
        assert!(would_create_cycle(&s, &RouteChange::Output { track: t, route: Route::Bus(ab) }).is_none());
    }
}
