//! The audio routing graph of a [`Session`]: which strips feed which busses, and which busses feed
//! which auxes, down to the main mix.
//!
//! Nodes are the session's tracks (by index), its busses (by index) and the main mix. Edges are:
//!
//! - a track's output to a bus or to the main mix ([`EdgeKind::Output`]),
//! - a track's send to a bus ([`EdgeKind::Send`], pre- or post-fader),
//! - a bus to the aux or routing folder that reads it as its input ([`EdgeKind::Input`]),
//! - a strip or bus to a strip whose insert detects on it ([`EdgeKind::Key`], a sidechain key).
//!   A key reaches a track other than an aux only from a track: busses and auxes render after the
//!   audio tracks, so such a key is inert (no edge) and the insert keeps detecting on its own input.
//!
//! Master faders and VCA masters are not on the graph: a master processes the main mix in place
//! and a VCA carries no audio. Folder membership does not route audio by itself; a routing folder
//! sums its members only through the bus its members output to, which is an ordinary edge here.
//! Routes to a bus that does not exist, to hardware, or to nothing are not edges (the mix drops
//! them). An audio track's bus input is not an edge either: audio tracks hear hardware input only.
//!
//! Commands use [`would_create_cycle`] and [`new_cycle`] to refuse an edit that closes a loop
//! (L22). The mix renders in [`RouteScratch::order`] and drops exactly [`RouteScratch::dropped`]
//! (F05): a session loaded from an older file can still hold a loop, and every loop is broken
//! deterministically, in time linear in the graph per round of cuts.

use crate::{BusId, KeyFrom, KeySource, Route, Session, Tap, Track, TrackId, TrackKind};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// The track's main output.
    Output,
    /// The track's send in this slot.
    Send { slot: usize, pre_fader: bool },
    /// The bus is the aux's (or folder's) input.
    Input,
    /// The insert in this slot of the target track detects on the source (a sidechain key),
    /// taken before the source's fader when `pre_fader`.
    Key { slot: usize, pre_fader: bool },
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
    /// `mix.insert_key`: the insert in `slot` of the track takes `key` (`None` clears it).
    Key { track: TrackId, slot: usize, key: Option<KeySource> },
}

impl RouteChange {
    fn track(&self) -> TrackId {
        match self {
            RouteChange::Output { track, .. }
            | RouteChange::Input { track, .. }
            | RouteChange::Send { track, .. }
            | RouteChange::Key { track, .. } => *track,
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
    /// The edges cut to break loops, in edge order: exactly the edges the mix drops (each writes
    /// a bus that a track earlier in the render order already read). Empty for an acyclic session.
    pub cut: Vec<RouteEdge>,
    /// For each cut edge (same order), a loop it closed.
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

/// Whether a track of this kind is on the graph.
fn on_graph(t: &Track) -> bool {
    !matches!(t.kind, TrackKind::Master | TrackKind::Vca)
}

/// Whether a track of this kind reads its input bus.
fn reads_bus(t: &Track) -> bool {
    matches!(t.kind, TrackKind::Aux | TrackKind::Folder)
}

const NONE: usize = usize::MAX;

/// Working storage for the routing graph of a session: the edges, the loop cuts, the render
/// order and the dropped edges. Reuse one across sessions: after [`RouteScratch::reserve`] for a
/// session, [`RouteScratch::layer`] of that session (or of any with no more tracks, busses and
/// send slots) allocates nothing, so the mix can re-order on the audio thread.
///
/// Loops are broken in rounds: each round finds the strongly connected components (Tarjan,
/// linear in the graph); in every component with a loop, the edges inside it that write the
/// bus its first track (lowest index) reads are cut, so that aux keeps every source from outside
/// the loop and loses only feedback. A session without loops takes one round; loops nested
/// inside one another take one more round per level.
#[derive(Debug, Clone, Default)]
pub struct RouteScratch {
    tracks: usize,
    busses: usize,
    /// Sorted (id, index): bus lookup without hashing.
    bus_ids: Vec<(BusId, usize)>,
    edges: Vec<RouteEdge>,
    from: Vec<usize>,
    to: Vec<usize>,
    /// Out-edges by node (compressed rows): `adj[off[v]..off[v + 1]]`.
    off: Vec<usize>,
    adj: Vec<usize>,
    cut: Vec<bool>,
    index: Vec<usize>,
    low: Vec<usize>,
    on: Vec<bool>,
    stack: Vec<usize>,
    call: Vec<(usize, usize)>,
    comp: Vec<usize>,
    /// Per component: its lowest track index when it holds a loop, else `NONE`.
    comp_min: Vec<usize>,
    /// Per track: the bus node it reads, or `NONE`.
    input: Vec<usize>,
    indeg: Vec<usize>,
    queue: Vec<usize>,
    depth: Vec<usize>,
    order: Vec<usize>,
    pos: Vec<usize>,
    /// Per bus: the render position of its first reader, or `NONE`.
    reader: Vec<usize>,
    dropped: Vec<usize>,
}

/// The node a route reaches: a bus (first one with that id) or the main mix.
fn route_node(bus_ids: &[(BusId, usize)], r: &Route) -> Option<RouteNode> {
    match r {
        Route::Bus(b) => {
            let i = bus_ids.partition_point(|(id, _)| id < b);
            bus_ids.get(i).filter(|(id, _)| id == b).map(|(_, k)| RouteNode::Bus(*k))
        }
        Route::Main => Some(RouteNode::Main),
        _ => None,
    }
}

/// Grows `v` to hold `n` without reallocating later.
fn hold<T>(v: &mut Vec<T>, n: usize) {
    v.clear();
    v.reserve(n);
}

impl RouteScratch {
    /// Empty storage.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes room for `s`'s graph (allocates when it is larger than any seen before).
    pub fn reserve(&mut self, s: &Session) {
        let n = s.tracks.len() + s.busses.len() + 1;
        let e: usize = s.tracks.iter().map(|t| t.mixer.sends.len().saturating_add(t.mixer.inserts.len()).saturating_add(2)).sum();
        hold(&mut self.bus_ids, s.busses.len());
        hold(&mut self.edges, e);
        hold(&mut self.from, e);
        hold(&mut self.to, e);
        hold(&mut self.off, n + 1);
        hold(&mut self.adj, e);
        hold(&mut self.cut, e);
        for v in
            [&mut self.index, &mut self.low, &mut self.comp, &mut self.comp_min, &mut self.indeg, &mut self.queue, &mut self.depth, &mut self.stack]
        {
            hold(v, n);
        }
        hold(&mut self.on, n);
        hold(&mut self.call, n);
        hold(&mut self.input, s.tracks.len());
        hold(&mut self.order, s.tracks.len());
        hold(&mut self.pos, s.tracks.len());
        hold(&mut self.reader, s.busses.len());
        hold(&mut self.dropped, e);
    }

    /// Builds and solves the graph of `s`: loop cuts, depths, render order, dropped edges.
    pub fn layer(&mut self, s: &Session) {
        self.load(s, &[]);
        self.solve();
    }

    /// The graph `s` would have after `changes` (applied in order; a later change to the same
    /// route wins). Changes naming a missing track or send slot are ignored.
    fn load(&mut self, s: &Session, changes: &[RouteChange]) {
        self.tracks = s.tracks.len();
        self.busses = s.busses.len();
        self.bus_ids.clear();
        self.bus_ids.extend(s.busses.iter().enumerate().map(|(i, b)| (b.id, i)));
        self.bus_ids.sort_unstable();
        self.edges.clear();
        for (i, t) in s.tracks.iter().enumerate().filter(|(_, t)| on_graph(t)) {
            let mut output = &t.mixer.output;
            let mut input = &t.mixer.input;
            for c in changes.iter().filter(|c| c.track() == t.id) {
                match c {
                    RouteChange::Output { route, .. } => output = route,
                    RouteChange::Input { route, .. } => input = route,
                    RouteChange::Send { .. } | RouteChange::Key { .. } => {}
                }
            }
            if reads_bus(t)
                && let Route::Bus(_) = input
                && let Some(from) = route_node(&self.bus_ids, input)
            {
                self.edges.push(RouteEdge { from, to: RouteNode::Track(i), kind: EdgeKind::Input });
            }
            if let Some(to) = route_node(&self.bus_ids, output) {
                self.edges.push(RouteEdge { from: RouteNode::Track(i), to, kind: EdgeKind::Output });
            }
            for (slot, snd) in t.mixer.sends.iter().enumerate() {
                let mut target = snd.as_ref().map(|x| &x.target);
                for c in changes {
                    if let RouteChange::Send { track, slot: k, route } = c
                        && *track == t.id
                        && *k == slot
                    {
                        target = Some(route);
                    }
                }
                // A send to an output path (`Route::Hardware`) is a sink: nothing reads it, so it
                // cannot close a loop and is not an edge (ADR 0015 section 6).
                let Some(target @ Route::Bus(_)) = target else { continue };
                let Some(to) = route_node(&self.bus_ids, target) else { continue };
                let pre_fader = snd.as_ref().is_some_and(|x| x.pre_fader);
                self.edges.push(RouteEdge { from: RouteNode::Track(i), to, kind: EdgeKind::Send { slot, pre_fader } });
            }
            for (slot, ins) in t.mixer.inserts.iter().enumerate() {
                let mut key = ins.as_ref().and_then(|x| x.key);
                for c in changes {
                    if let RouteChange::Key { track, slot: k, key: new } = c
                        && *track == t.id
                        && *k == slot
                    {
                        key = *new;
                    }
                }
                let Some(key) = key else { continue };
                if let Some(from) = self.key_node(s, t, i, key.from) {
                    self.edges.push(RouteEdge { from, to: RouteNode::Track(i), kind: EdgeKind::Key { slot, pre_fader: key.tap == Tap::Pre } });
                }
            }
        }
    }

    /// The node an insert's key reaches track `i` from, `None` when the key is inert: a missing
    /// source, the track itself, or a source that renders after `t` (see the module docs).
    fn key_node(&self, s: &Session, t: &Track, i: usize, from: KeyFrom) -> Option<RouteNode> {
        match from {
            KeyFrom::Track(id) => {
                let k = s.track_index(id).filter(|&k| k != i)?;
                let src = s.tracks.get(k).filter(|x| on_graph(x))?;
                (reads_bus(t) || !reads_bus(src)).then_some(RouteNode::Track(k))
            }
            KeyFrom::Bus(b) => {
                let node = route_node(&self.bus_ids, &Route::Bus(b))?;
                reads_bus(t).then_some(node)
            }
        }
    }

    fn nodes(&self) -> usize {
        self.tracks + self.busses + 1
    }

    /// Adjacency (compressed rows) and per-track inputs of the loaded edges.
    fn index_edges(&mut self) {
        let n = self.nodes();
        let (tracks, busses) = (self.tracks, self.busses);
        let node = |x: RouteNode| match x {
            RouteNode::Track(i) => i,
            RouteNode::Bus(b) => tracks + b,
            RouteNode::Main => tracks + busses,
        };
        self.from.clear();
        self.to.clear();
        self.from.extend(self.edges.iter().map(|e| node(e.from)));
        self.to.extend(self.edges.iter().map(|e| node(e.to)));
        self.off.clear();
        self.off.resize(n + 1, 0);
        for &f in &self.from {
            if let Some(x) = self.off.get_mut(f + 1) {
                *x += 1;
            }
        }
        for v in 0..n {
            let prev = self.off[v];
            self.off[v + 1] += prev;
        }
        self.adj.clear();
        self.adj.resize(self.edges.len(), 0);
        // `indeg` as the fill cursor per node.
        self.indeg.clear();
        self.indeg.extend_from_slice(&self.off[..n]);
        for (e, &f) in self.from.iter().enumerate() {
            let at = self.indeg[f];
            self.adj[at] = e;
            self.indeg[f] += 1;
        }
        self.input.clear();
        self.input.resize(self.tracks, NONE);
        for (e, edge) in self.edges.iter().enumerate() {
            if let (EdgeKind::Input, RouteNode::Track(t)) = (edge.kind, edge.to) {
                self.input[t] = self.from[e];
            }
        }
    }

    /// Strongly connected components over the edges not cut. Fills `comp` and `comp_min`.
    fn components(&mut self) {
        let n = self.nodes();
        self.index.clear();
        self.index.resize(n, NONE);
        self.low.clear();
        self.low.resize(n, 0);
        self.on.clear();
        self.on.resize(n, false);
        self.comp.clear();
        self.comp.resize(n, NONE);
        self.comp_min.clear();
        self.stack.clear();
        self.call.clear();
        let mut counter = 0usize;
        for root in 0..n {
            if self.index[root] != NONE {
                continue;
            }
            self.index[root] = counter;
            self.low[root] = counter;
            counter += 1;
            self.stack.push(root);
            self.on[root] = true;
            self.call.push((root, self.off[root]));
            while let Some(&(v, i)) = self.call.last() {
                if i < self.off[v + 1] {
                    if let Some(top) = self.call.last_mut() {
                        top.1 += 1;
                    }
                    let e = self.adj[i];
                    if self.cut[e] {
                        continue;
                    }
                    let w = self.to[e];
                    if self.index[w] == NONE {
                        self.index[w] = counter;
                        self.low[w] = counter;
                        counter += 1;
                        self.stack.push(w);
                        self.on[w] = true;
                        self.call.push((w, self.off[w]));
                    } else if self.on[w] {
                        self.low[v] = self.low[v].min(self.index[w]);
                    }
                } else {
                    self.call.pop();
                    if let Some(&(p, _)) = self.call.last() {
                        self.low[p] = self.low[p].min(self.low[v]);
                    }
                    if self.low[v] == self.index[v] {
                        let c = self.comp_min.len();
                        let (mut size, mut first) = (0usize, NONE);
                        while let Some(w) = self.stack.pop() {
                            self.on[w] = false;
                            self.comp[w] = c;
                            size += 1;
                            if w < self.tracks {
                                first = first.min(w);
                            }
                            if w == v {
                                break;
                            }
                        }
                        self.comp_min.push(if size > 1 { first } else { NONE });
                    }
                }
            }
        }
    }

    /// Cuts loops, then layers, orders and finds the dropped edges.
    fn solve(&mut self) {
        self.index_edges();
        self.cut.clear();
        self.cut.resize(self.edges.len(), false);
        loop {
            self.components();
            let mut any = false;
            for e in 0..self.edges.len() {
                let (u, v) = (self.from[e], self.to[e]);
                if self.cut[e] || self.comp[u] != self.comp[v] {
                    continue;
                }
                let first = self.comp_min[self.comp[u]];
                if first != NONE && self.input[first] == v {
                    self.cut[e] = true;
                    any = true;
                }
            }
            if !any {
                // No bus input to cut: the loop runs through keys. Cut the keys into its first track.
                for e in 0..self.edges.len() {
                    let (u, v) = (self.from[e], self.to[e]);
                    if self.cut[e] || self.comp[u] != self.comp[v] || !matches!(self.edges[e].kind, EdgeKind::Key { .. }) {
                        continue;
                    }
                    if self.comp_min[self.comp[u]] == v {
                        self.cut[e] = true;
                        any = true;
                    }
                }
            }
            if !any {
                break;
            }
        }
        // Depths: longest chain of tracks, in topological order of the edges not cut.
        let n = self.nodes();
        self.indeg.clear();
        self.indeg.resize(n, 0);
        for e in 0..self.edges.len() {
            if !self.cut[e] {
                self.indeg[self.to[e]] += 1;
            }
        }
        self.depth.clear();
        self.depth.resize(n, 0);
        self.queue.clear();
        self.queue.extend((0..n).filter(|&v| self.indeg[v] == 0));
        let mut head = 0;
        while let Some(&v) = self.queue.get(head) {
            head += 1;
            for i in self.off[v]..self.off[v + 1] {
                let e = self.adj[i];
                if self.cut[e] {
                    continue;
                }
                let w = self.to[e];
                let d = self.depth[v] + usize::from(w < self.tracks);
                self.depth[w] = self.depth[w].max(d);
                self.indeg[w] -= 1;
                if self.indeg[w] == 0 {
                    self.queue.push(w);
                }
            }
        }
        self.order.clear();
        self.order.extend(0..self.tracks);
        let depth = &self.depth;
        self.order.sort_unstable_by_key(|&i| (depth[i], i));
        self.pos.clear();
        self.pos.resize(self.tracks, 0);
        for (k, &t) in self.order.iter().enumerate() {
            self.pos[t] = k;
        }
        self.reader.clear();
        self.reader.resize(self.busses, NONE);
        for t in 0..self.tracks {
            let b = self.input[t];
            if b != NONE {
                let r = &mut self.reader[b - self.tracks];
                *r = (*r).min(self.pos[t]);
            }
        }
        // The edges the mix drops: a track writing a bus that a track before it already read.
        self.dropped.clear();
        for e in 0..self.edges.len() {
            let (u, v) = (self.from[e], self.to[e]);
            let key_cut = self.cut[e] && matches!(self.edges[e].kind, EdgeKind::Key { .. });
            if key_cut || (u < self.tracks && v >= self.tracks && v < self.tracks + self.busses && self.pos[u] >= self.reader[v - self.tracks]) {
                self.dropped.push(e);
            }
        }
    }

    /// Track indices in render order: by depth, then track index.
    #[must_use]
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    /// The depth of a track (see [`Layering::depth`]).
    #[must_use]
    pub fn depth(&self, track: usize) -> usize {
        self.depth.get(track).copied().unwrap_or(0)
    }

    /// Every edge.
    #[must_use]
    pub fn edges(&self) -> &[RouteEdge] {
        &self.edges
    }

    /// The edges the render drops (loop feedback), in edge order.
    pub fn dropped(&self) -> impl Iterator<Item = &RouteEdge> {
        self.dropped.iter().filter_map(|&e| self.edges.get(e))
    }

    /// Whether any edge is dropped (the session holds a loop).
    #[must_use]
    pub fn has_loops(&self) -> bool {
        !self.dropped.is_empty()
    }

    /// The index of bus `id` in the session (the first with that id).
    #[must_use]
    pub fn bus_slot(&self, id: BusId) -> Option<usize> {
        match route_node(&self.bus_ids, &Route::Bus(id)) {
            Some(RouteNode::Bus(i)) => Some(i),
            _ => None,
        }
    }

    /// A path of edges from node `from` to node `to`, breadth first, inside `from`'s component of
    /// the uncut graph when `within` is given. Allocates (diagnostics and commands only).
    fn path(&self, from: usize, to: usize, within: Option<&[usize]>) -> Option<Vec<usize>> {
        let n = self.nodes();
        let mut via: Vec<usize> = vec![NONE; n];
        let mut seen = vec![false; n];
        let mut queue = std::collections::VecDeque::new();
        seen[from] = true;
        queue.push_back(from);
        let same = |w: usize| within.is_none_or(|c| c.get(w) == c.get(from));
        while let Some(at) = queue.pop_front() {
            if at == to {
                let mut out = Vec::new();
                let mut cur = to;
                while cur != from {
                    let e = via[cur];
                    if e == NONE {
                        break;
                    }
                    out.push(e);
                    cur = self.from[e];
                }
                out.reverse();
                return Some(out);
            }
            for i in self.off[at]..self.off[at + 1] {
                let e = self.adj[i];
                let w = self.to[e];
                if !seen[w] && same(w) {
                    seen[w] = true;
                    via[w] = e;
                    queue.push_back(w);
                }
            }
        }
        None
    }

    /// A loop closed by edge `e`, as its edges in signal order starting with `e`.
    fn loop_through(&self, e: usize, within: Option<&[usize]>) -> Option<Vec<usize>> {
        let mut edges = self.path(self.to[e], self.from[e], within)?;
        edges.insert(0, e);
        Some(edges)
    }
}

/// The routing graph of one session.
#[derive(Debug, Clone)]
pub struct RouteGraph {
    ids: Vec<TrackId>,
    scratch: RouteScratch,
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
        let mut scratch = RouteScratch::new();
        scratch.reserve(s);
        scratch.load(s, changes);
        scratch.solve();
        RouteGraph { ids: s.tracks.iter().map(|t| t.id).collect(), scratch }
    }

    /// Every edge, grouped by source track in track order (input first, then output, then sends).
    #[must_use]
    pub fn edges(&self) -> &[RouteEdge] {
        &self.scratch.edges
    }

    fn tracks_of(&self, edges: &[usize]) -> Vec<TrackId> {
        edges
            .iter()
            .filter_map(|&e| match self.scratch.edges.get(e).map(|x| x.from) {
                Some(RouteNode::Track(i)) => self.ids.get(i).copied(),
                _ => None,
            })
            .collect()
    }

    /// Some loop in the graph, if there is one: the tracks on it in signal order.
    #[must_use]
    pub fn find_cycle(&self) -> Option<RouteCycle> {
        let e = *self.scratch.dropped.first()?;
        self.cycle_through(e)
    }

    fn cycle_through(&self, e: usize) -> Option<RouteCycle> {
        // The loop lies inside the component of the graph with no cuts: compute it once here.
        let mut whole = self.scratch.clone();
        whole.cut.iter_mut().for_each(|c| *c = false);
        whole.components();
        let edges = whole.loop_through(e, Some(&whole.comp))?;
        Some(RouteCycle { tracks: self.tracks_of(&edges) })
    }

    /// Track indices in render order when the graph is acyclic, else the first loop found.
    pub fn topological_order(&self) -> Result<Vec<usize>, RouteCycle> {
        match self.find_cycle() {
            Some(c) => Err(c),
            None => Ok(self.scratch.order.clone()),
        }
    }

    /// Breaks every loop and layers the tracks (see [`RouteScratch`] for the rule). The cut set
    /// is exactly what the mix drops.
    #[must_use]
    pub fn layering(&self) -> Layering {
        let sc = &self.scratch;
        let depth = sc.depth.get(..sc.tracks).map(<[usize]>::to_vec).unwrap_or_default();
        let cut: Vec<RouteEdge> = sc.dropped().copied().collect();
        let mut loops = Vec::with_capacity(cut.len());
        if !cut.is_empty() {
            let mut whole = sc.clone();
            whole.cut.iter_mut().for_each(|c| *c = false);
            whole.components();
            for &e in &sc.dropped {
                let edges = whole.loop_through(e, Some(&whole.comp)).unwrap_or_else(|| vec![e]);
                loops.push(RouteCycle { tracks: self.tracks_of(&edges) });
            }
        }
        Layering { depth, cut, loops }
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
    let changed: Vec<usize> = changes.iter().filter_map(|c| s.track_index(c.track())).collect();
    // Either end: a key edge joins two tracks.
    let touches = |e: &RouteEdge| [e.from, e.to].iter().any(|n| matches!(n, RouteNode::Track(i) if changed.contains(i)));
    let old: Vec<&RouteEdge> = before.edges().iter().filter(|e| touches(e)).collect();
    for (k, e) in after.edges().iter().enumerate() {
        // Only edges the changes add can close a new loop.
        if !touches(e) || old.contains(&e) {
            continue;
        }
        if let Some(edges) = after.scratch.loop_through(k, None) {
            let mut tracks = after.tracks_of(&edges);
            let ids: Vec<TrackId> = changes.iter().map(RouteChange::track).collect();
            if let Some(p) = tracks.iter().position(|t| ids.contains(t)) {
                tracks.rotate_left(p);
            }
            return Some(tracks);
        }
    }
    None
}

/// Whether two sessions route alike (same tracks, kinds, inputs, outputs, send targets and busses,
/// in the same order): then no edit between them can have closed a loop. Allocates nothing.
fn same_routing(a: &Session, b: &Session) -> bool {
    a.tracks.len() == b.tracks.len()
        && a.busses.len() == b.busses.len()
        && a.busses.iter().zip(&b.busses).all(|(x, y)| x.id == y.id)
        && a.tracks.iter().zip(&b.tracks).all(|(x, y)| {
            x.id == y.id
                && x.kind == y.kind
                && x.mixer.input == y.mixer.input
                && x.mixer.output == y.mixer.output
                && x.mixer.inserts.len() == y.mixer.inserts.len()
                && x.mixer.inserts.iter().zip(&y.mixer.inserts).all(|(p, q)| p.as_ref().map(|z| z.key) == q.as_ref().map(|z| z.key))
                && x.mixer.sends.len() == y.mixer.sends.len()
                && x.mixer.sends.iter().zip(&y.mixer.sends).all(|(p, q)| p.as_ref().map(|z| &z.target) == q.as_ref().map(|z| &z.target))
        })
}

/// A node by identity (not index), to compare the graphs of two sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NodeKey {
    Track(TrackId),
    Bus(BusId),
    Main,
}

fn edge_keys(s: &Session, g: &RouteGraph) -> Vec<(NodeKey, NodeKey, EdgeKind)> {
    let key = |n: RouteNode| match n {
        RouteNode::Track(i) => s.tracks.get(i).map_or(NodeKey::Main, |t| NodeKey::Track(t.id)),
        RouteNode::Bus(i) => s.busses.get(i).map_or(NodeKey::Main, |b| NodeKey::Bus(b.id)),
        RouteNode::Main => NodeKey::Main,
    };
    g.edges().iter().map(|e| (key(e.from), key(e.to), e.kind)).collect()
}

/// Whether `after` (an edit of `before`) holds a loop through an edge `before` did not have: the
/// loop's tracks in signal order starting at that edge's track. Loops `before` already had (an
/// older file) are not reported. Compares by track and bus identity, so edits that add, delete,
/// reorder or import tracks and busses are covered.
#[must_use]
pub fn new_cycle(before: &Session, after: &Session) -> Option<Vec<TrackId>> {
    if same_routing(before, after) {
        return None;
    }
    let a = RouteGraph::new(after);
    if !a.scratch.has_loops() {
        return None;
    }
    let mut old = edge_keys(before, &RouteGraph::new(before));
    old.sort_unstable();
    for (k, key) in edge_keys(after, &a).into_iter().enumerate() {
        if old.binary_search(&key).is_ok() {
            continue;
        }
        if let Some(edges) = a.scratch.loop_through(k, None) {
            return Some(a.tracks_of(&edges));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BusId, ChannelFormat, Insert, SendSlot, time::SampleRate};

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
    fn output_paths_are_sinks_and_never_make_a_loop() {
        let mut s = session();
        let (a, ab) = aux(&mut s, "A");
        let src = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Src"));
        set_output(&mut s, src, Route::Bus(ab));
        set_output(&mut s, a, Route::Hardware("Out 3-4".into()));
        if let Some(t) = s.track_mut(a) {
            t.mixer.sends[0] = Some(SendSlot::new(Route::Hardware("Out 5-6".into())));
        }
        // Nothing reads a path, so neither the output nor the send changes the graph or refuses an edit.
        let routes = RouteGraph::new(&s);
        let from_a = |e: &&RouteEdge| matches!(e.from, RouteNode::Track(i) if s.tracks.get(i).is_some_and(|t| t.id == a));
        assert_eq!(routes.edges().iter().filter(from_a).count(), 0);
        assert!(would_create_cycle(&s, &RouteChange::Output { track: src, route: Route::Hardware("Out 1-2".into()) }).is_none());
        assert!(would_create_cycle(&s, &RouteChange::Send { track: a, slot: 1, route: Route::Hardware("Out 3-4".into()) }).is_none());
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

    fn keyed(s: &mut Session, t: TrackId, slot: usize, from: KeyFrom) {
        if let Some(t) = s.track_mut(t) {
            let mut i = Insert::new("compressor");
            i.key = Some(KeySource { from, tap: Tap::Post });
            t.mixer.inserts[slot] = Some(i);
        }
    }

    fn key_edges(s: &Session) -> Vec<RouteEdge> {
        RouteGraph::new(s).edges().iter().filter(|e| matches!(e.kind, EdgeKind::Key { .. })).copied().collect()
    }

    #[test]
    fn a_key_cycle_is_refused_at_edit_time() {
        let mut s = session();
        let a = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("A"));
        let b = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("B"));
        keyed(&mut s, a, 0, KeyFrom::Track(b));
        keyed(&mut s, b, 0, KeyFrom::Track(b));
        let back = Some(KeySource { from: KeyFrom::Track(a), tap: Tap::Pre });
        let c = would_create_cycle(&s, &RouteChange::Key { track: b, slot: 0, key: back }).expect("A keyed from B, B keyed from A");
        assert_eq!(names(&s, &c), ["B", "A"]);
        assert!(would_create_cycle(&s, &RouteChange::Key { track: b, slot: 0, key: None }).is_none());
        // An aux keyed from a bus it feeds.
        let (x, _) = aux(&mut s, "X");
        let out = s.add_bus("Out", ChannelFormat::Stereo);
        set_output(&mut s, x, Route::Bus(out));
        let from_out = Some(KeySource { from: KeyFrom::Bus(out), tap: Tap::Post });
        assert_eq!(would_create_cycle(&s, &RouteChange::Key { track: x, slot: 0, key: from_out }), Some(vec![x]));
        // The same bus is a fine key for an aux that does not feed it, and so is a track.
        let (y, _) = aux(&mut s, "Y");
        assert!(would_create_cycle(&s, &RouteChange::Key { track: y, slot: 0, key: from_out }).is_none());
    }

    #[test]
    fn a_loaded_two_track_mutual_key_is_cut_deterministically() {
        let mut s = session();
        let a = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("A"));
        let b = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("B"));
        keyed(&mut s, a, 0, KeyFrom::Track(b));
        keyed(&mut s, b, 3, KeyFrom::Track(a));
        let loaded = Session::from_json(&s.to_json().expect("save")).expect("load");
        let g = loaded.route_graph();
        assert!(g.find_cycle().is_some(), "the loop is reported");
        let l = g.layering();
        // The key into the first track (A) is the one dropped.
        assert_eq!(l.cut.len(), 1);
        assert_eq!(l.cut[0].kind, EdgeKind::Key { slot: 0, pre_fader: false });
        assert_eq!((l.depth[0], l.depth[1]), (0, 1), "A's key is cut, so B (keyed from A) renders after A");
        assert_eq!(loaded.route_graph().layering(), l);
        // Edge storage was sized for the inserts (no growth past `reserve`).
        let mut scratch = RouteScratch::new();
        scratch.reserve(&loaded);
        scratch.layer(&loaded);
        assert_eq!(scratch.dropped().count(), 1);
    }

    #[test]
    fn keys_that_cannot_reach_their_strip_are_not_edges() {
        let mut s = session();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("T"));
        let (a, _) = aux(&mut s, "A");
        let kb = s.add_bus("K", ChannelFormat::Stereo);
        // An audio track renders before every aux and bus: neither can key it. Itself neither.
        keyed(&mut s, t, 0, KeyFrom::Track(a));
        keyed(&mut s, t, 1, KeyFrom::Bus(kb));
        keyed(&mut s, t, 2, KeyFrom::Track(t));
        keyed(&mut s, t, 3, KeyFrom::Track(TrackId(99_999)));
        assert!(key_edges(&s).is_empty());
        // An aux can be keyed by a track and a bus, which orders it after them.
        keyed(&mut s, a, 0, KeyFrom::Track(t));
        keyed(&mut s, a, 1, KeyFrom::Bus(kb));
        assert_eq!(key_edges(&s).len(), 2);
        assert_eq!(s.route_graph().layering().depth[s.track_index(a).expect("a")], 1);
    }

    /// 1,024 auxes, each feeding its own input (a self-loop) and sending to the one before.
    fn self_looped(n: usize) -> Session {
        let mut s = session();
        let mut prev: Option<BusId> = None;
        for i in 0..n {
            let (a, b) = aux(&mut s, &format!("a{i}"));
            set_output(&mut s, a, Route::Bus(b));
            if let (Some(p), Some(t)) = (prev, s.track_mut(a)) {
                for k in 0..4 {
                    t.mixer.sends[k] = Some(SendSlot::new(Route::Bus(p)));
                }
            }
            prev = Some(b);
        }
        s
    }

    #[test]
    fn many_self_loops_are_each_cut_once() {
        let s = self_looped(64);
        let l = s.route_graph().layering();
        assert_eq!(l.cut.len(), 64, "one feedback edge per aux");
        assert!(l.cut.iter().all(|e| e.kind == EdgeKind::Output));
    }

    /// Release build: `cargo test -p soundcraft-model --release -- --ignored layering_is_fast`.
    #[test]
    #[ignore = "timing; run in release"]
    fn layering_is_fast() {
        let s = self_looped(1024);
        let t0 = std::time::Instant::now();
        let l = s.route_graph().layering();
        let took = t0.elapsed();
        println!("1024 self-looped auxes, 4 sends each: layering {took:?}, {} cuts", l.cut.len());
        assert_eq!(l.cut.len(), 1024);
        assert!(took < std::time::Duration::from_millis(20), "{took:?}");
    }
}
