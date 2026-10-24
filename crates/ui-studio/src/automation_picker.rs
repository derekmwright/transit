//! The automation lane picker's parameter model (issue #65). A plug-in can expose thousands of
//! parameters, so the picker groups them by the plug-in's own module (CLAP) or unit (VST3) path,
//! filters them by a search box, and lists the ones last moved in the editor under "Recent". The
//! grouped model is built once per cached parameter table and shared between frames.

use crate::automation::Lane;
use crate::mixer::MenuItem;
use egui::Ui;
use soundcraft_dsp::PluginInfo;
use std::collections::HashMap;
use std::sync::Arc;

/// Parameters remembered per plug-in type for the "Recent" group.
pub(crate) const RECENT_MAX: usize = 8;
/// Search matches drawn at most; the rest are summarized.
const MATCH_MAX: usize = 200;
/// A leaf list longer than this scrolls by rows instead of drawing every button.
const ROWS_ABOVE: usize = 40;

/// One parameter in the model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    /// The plug-in's parameter id.
    pub id: String,
    /// The group path (parts joined by `/`), empty when ungrouped.
    pub group: String,
    /// The name without its group.
    pub leaf: String,
    /// The label shown where the group is not: "Cell A1 > Poly Mode On".
    pub path: String,
    lower: String,
}

/// A group in the tree: its sub-groups and the entries directly in it.
#[derive(Debug, Default)]
pub(crate) struct Node {
    pub name: String,
    pub children: Vec<Node>,
    pub leaves: Vec<usize>,
}

/// A device's parameters, grouped.
#[derive(Debug, Default)]
pub(crate) struct DeviceModel {
    pub entries: Vec<Entry>,
    pub root: Node,
    pub by_id: HashMap<String, usize>,
    /// Whether any parameter has a group; without one the picker is a flat list.
    pub grouped: bool,
    /// Entries whose leaf name repeats within their group show their full path.
    pub dup: Vec<bool>,
}

impl DeviceModel {
    /// Builds the model from (id, name, group) rows. A name that starts with its group and `/`
    /// (CLAP reports "module/name") is cut to the leaf.
    pub fn build<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> Self {
        let mut m = DeviceModel::default();
        for (id, name, group) in rows {
            let leaf = name.strip_prefix(group).and_then(|r| r.strip_prefix('/')).filter(|_| !group.is_empty()).unwrap_or(name);
            let path = if group.is_empty() { leaf.to_string() } else { format!("{} > {leaf}", group.split('/').collect::<Vec<_>>().join(" > ")) };
            m.grouped |= !group.is_empty();
            let i = m.entries.len();
            m.by_id.insert(id.to_string(), i);
            m.entries.push(Entry { id: id.into(), group: group.into(), leaf: leaf.into(), lower: path.to_lowercase(), path });
            let mut node = &mut m.root;
            for part in group.split('/').filter(|p| !p.is_empty()) {
                let at = match node.children.iter().position(|c| c.name == part) {
                    Some(at) => at,
                    None => {
                        node.children.push(Node { name: part.into(), ..Node::default() });
                        node.children.len() - 1
                    }
                };
                node = &mut node.children[at];
            }
            node.leaves.push(i);
        }
        let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
        for e in &m.entries {
            *seen.entry((e.group.as_str(), e.leaf.as_str())).or_default() += 1;
        }
        m.dup = m.entries.iter().map(|e| seen[&(e.group.as_str(), e.leaf.as_str())] > 1).collect();
        m
    }

    pub fn of_info(info: &PluginInfo) -> Self {
        Self::build(info.params.iter().map(|p| (p.id, p.name, p.group)))
    }

    /// The entries whose full path holds every word of `query`, case-insensitive, in table order.
    pub fn search(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }
        (0..self.entries.len()).filter(|&i| words.iter().all(|w| self.entries[i].lower.contains(w))).collect()
    }

    /// The label for `i` where its group is not shown.
    pub fn leaf_label(&self, i: usize) -> &str {
        if self.dup[i] { &self.entries[i].path } else { &self.entries[i].leaf }
    }
}

/// The models by plug-in id, each with the identity of the cached table it was built from. The
/// host caches a table once per plug-in type, so a new table (address) is the only invalidation.
#[derive(Debug, Default)]
pub(crate) struct PickerState {
    models: HashMap<String, (usize, Arc<DeviceModel>)>,
    /// Models built so far (tests watch it).
    pub builds: usize,
    pub query: String,
    pub focus: bool,
    /// Per plug-in type, the parameters last moved in an editor, newest first.
    pub recent: HashMap<String, Vec<String>>,
}

impl PickerState {
    /// The model of a cached table, built on the first call and again only when the table is replaced.
    pub fn model(&mut self, plugin: &str, info: &'static PluginInfo) -> Arc<DeviceModel> {
        let key = std::ptr::from_ref(info) as usize;
        if let Some((k, m)) = self.models.get(plugin)
            && *k == key
        {
            return m.clone();
        }
        self.builds += 1;
        let m = Arc::new(DeviceModel::of_info(info));
        self.models.insert(plugin.to_string(), (key, m.clone()));
        m
    }

    /// `param` was moved in `plugin`'s editor.
    pub fn touched(&mut self, plugin: &str, param: &str) {
        let r = self.recent.entry(plugin.to_string()).or_default();
        r.retain(|p| p != param);
        r.insert(0, param.to_string());
        r.truncate(RECENT_MAX);
    }
}

/// What the popup lists for one device.
pub(crate) struct DeviceView<'a> {
    pub name: &'a str,
    pub slot: u8,
    pub model: &'a DeviceModel,
    pub recent: &'a [String],
}

/// Draws one entry; returns its lane id when clicked.
fn leaf(ui: &mut Ui, v: &DeviceView, i: usize, label: &str, current: &str, menu: &mut Vec<MenuItem>) -> Option<String> {
    let id = Lane::Plugin { slot: v.slot, param: v.model.entries[i].id.clone() }.id();
    let b = crate::menus::check_button(ui, id == current, label);
    menu.push(MenuItem::at(ui, label.to_string(), id.clone(), b.rect));
    b.clicked().then_some(id)
}

fn leaves(ui: &mut Ui, v: &DeviceView, list: &[usize], current: &str, menu: &mut Vec<MenuItem>) -> Option<String> {
    let mut picked = None;
    if list.len() > ROWS_ABOVE {
        let h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show_rows(ui, h, list.len(), |ui, rows| {
            for &i in &list[rows] {
                picked = picked.take().or(leaf(ui, v, i, v.model.leaf_label(i), current, menu));
            }
        });
    } else {
        for &i in list {
            picked = picked.take().or(leaf(ui, v, i, v.model.leaf_label(i), current, menu));
        }
    }
    picked
}

fn tree(ui: &mut Ui, v: &DeviceView, node: &Node, current: &str, menu: &mut Vec<MenuItem>) -> Option<String> {
    let mut picked = None;
    for c in &node.children {
        let r = ui.menu_button(crate::menus::menu_contents(c.name.as_str()), |ui| {
            let p = tree(ui, v, c, current, menu);
            if p.is_some() {
                ui.close();
            }
            p
        });
        if let Some(Some(id)) = r.inner {
            picked = Some(id);
        }
    }
    picked.or_else(|| leaves(ui, v, &node.leaves, current, menu))
}

/// One device's section of the popup: Recent first, then the tree (or the flat list). `matches`
/// is the search result, when a search is active.
pub(crate) fn device(ui: &mut Ui, v: &DeviceView, matches: Option<&[usize]>, current: &str, menu: &mut Vec<MenuItem>) -> Option<String> {
    let mut picked = None;
    let dim = crate::theme::TOKENS.text_dim;
    if let Some(m) = matches {
        if m.is_empty() {
            return None;
        }
        ui.label(egui::RichText::new(v.name).small().color(dim));
        let shown = &m[..m.len().min(MATCH_MAX)];
        for &i in shown {
            picked = picked.take().or(leaf(ui, v, i, &v.model.entries[i].path, current, menu));
        }
        if m.len() > shown.len() {
            ui.label(egui::RichText::new(format!("{} more: refine the search", m.len() - shown.len())).small().color(dim));
        }
        return picked;
    }
    ui.label(egui::RichText::new(v.name).small().color(dim));
    let recent: Vec<usize> = v.recent.iter().filter_map(|p| v.model.by_id.get(p).copied()).collect();
    if !recent.is_empty() {
        ui.label(egui::RichText::new("Recent").small().color(dim));
        for i in recent {
            picked = picked.take().or(leaf(ui, v, i, &v.model.entries[i].path, current, menu));
        }
    }
    let rest = if v.model.grouped { tree(ui, v, &v.model.root, current, menu) } else { leaves(ui, v, &v.model.root.leaves, current, menu) };
    picked.or(rest)
}
