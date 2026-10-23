//! The window a hosted plug-in's editor lives in (issue #68): a top-level window on the plug-in
//! host thread (ADR 0018) with a thin header strip above the plug-in's own child window.
//!
//! The strip is drawn here with GDI, not with egui: the editor window belongs to the plug-in host
//! thread, which has no egui context or renderer, and a second renderer on that thread would put
//! GPU and toolkit code next to every plug-in's view code. The strip is a child window of its own
//! that never takes keyboard focus; the plug-in's child window lives in a separate container below
//! it, so the plug-in can never draw over the strip, and focus goes back to the plug-in whenever
//! the window is activated.
//!
//! | Part | What it is |
//! |---|---|
//! | frame | top-level window, client area = strip + plug-in content, exactly |
//! | strip | [`header_height`] tall, preset dropdown, previous/next, Save, Save As…, Load…, Bypass, menu |
//! | content | child window the plug-in parents its view in (VST3 `attached`, CLAP `set_parent`) |
//!
//! The strip shows a [`HeaderView`] the editor handle puts in a [`HeaderCell`]; clicks go back
//! through the same cell ([`HeaderCell::take_clicks`]). Sizes are physical pixels, as plug-ins
//! report them on Windows.
//!
//! Contract: every [`EditorWindow`] method and [`resize_content`] act only on a window of the
//! calling thread (they do nothing otherwise); [`HeaderCell`] may be used from any thread.
//! Nothing here panics. Other platforms have no such window: [`EditorWindow::new`] says so.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Height of the strip at 96 DPI, in pixels.
pub const HEADER_HEIGHT: i32 = 30;

/// Most entries a dropdown lists (a folder with thousands of presets stays usable).
pub const MAX_LISTED: usize = 500;

/// What the strip shows (see `soundcraft_dsp::EditorHeader`, which the format hosts copy here).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // independent flags of a view, not a state
pub struct HeaderView {
    /// The window title.
    pub title: String,
    /// The current preset's name (empty: none).
    pub preset: String,
    /// User presets, in dropdown order.
    pub user: Vec<String>,
    /// Factory presets (read-only); empty when the format exposes none.
    pub factory: Vec<String>,
    /// The slot is bypassed.
    pub bypassed: bool,
    /// The slot can be bypassed.
    pub can_bypass: bool,
    /// A copied state can be pasted.
    pub can_paste: bool,
    /// The current preset can be deleted.
    pub can_delete: bool,
    /// Every control is disabled, with this reason shown in the strip.
    pub disabled: Option<String>,
}

/// What the user did in the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderClick {
    /// A user preset, by index into [`HeaderView::user`].
    User(usize),
    /// A factory preset, by index into [`HeaderView::factory`].
    Factory(usize),
    /// The previous preset.
    Previous,
    /// The next preset.
    Next,
    /// Save over the current user preset.
    Save,
    /// Save under a new name.
    SaveAs,
    /// Load a preset file.
    Load,
    /// Toggle the bypass.
    Bypass,
    /// Back to the plug-in's default state.
    Reset,
    /// Copy the state.
    Copy,
    /// Paste a copied state.
    Paste,
    /// Delete the current user preset.
    Delete,
    /// Show the slot in the device chain.
    ShowInChain,
}

/// The strip's model, shared by the editor handle (any thread) and the window (its thread).
#[derive(Default)]
pub struct HeaderCell {
    inner: Mutex<CellInner>,
}

#[derive(Default)]
struct CellInner {
    view: HeaderView,
    /// Bumped by every change of `view`.
    generation: u64,
    clicks: Vec<HeaderClick>,
}

impl HeaderCell {
    /// An empty cell.
    #[must_use]
    pub fn new() -> Arc<HeaderCell> {
        Arc::new(HeaderCell::default())
    }

    fn lock(&self) -> MutexGuard<'_, CellInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Shows `view`; false when it is what the cell holds already.
    pub fn set(&self, view: &HeaderView) -> bool {
        let mut g = self.lock();
        if g.view == *view {
            return false;
        }
        g.view = view.clone();
        g.generation = g.generation.wrapping_add(1);
        true
    }

    /// What the strip shows.
    #[must_use]
    pub fn view(&self) -> HeaderView {
        self.lock().view.clone()
    }

    /// Bumped by every change of the view.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Records a click. A click while the strip is disabled, or on an entry the view does not
    /// have, is dropped.
    pub fn click(&self, click: HeaderClick) {
        let mut g = self.lock();
        let valid = match click {
            HeaderClick::User(i) => i < g.view.user.len(),
            HeaderClick::Factory(i) => i < g.view.factory.len(),
            HeaderClick::Bypass => g.view.can_bypass,
            HeaderClick::Paste => g.view.can_paste,
            HeaderClick::Delete => g.view.can_delete,
            _ => true,
        };
        if g.view.disabled.is_none() && valid && g.clicks.len() < 64 {
            g.clicks.push(click);
        }
    }

    /// The clicks since the last call, in order.
    pub fn take_clicks(&self) -> Vec<HeaderClick> {
        std::mem::take(&mut self.lock().clicks)
    }
}

/// The strip's height at `dpi` (96 when the window's is unknown).
#[must_use]
pub fn header_height(dpi: u32) -> i32 {
    scale(HEADER_HEIGHT, dpi)
}

fn scale(px: i32, dpi: u32) -> i32 {
    let dpi = i64::from(if dpi == 0 { 96 } else { dpi.min(960) });
    i32::try_from((i64::from(px) * dpi + 48) / 96).unwrap_or(px)
}

/// The frame's client size for plug-in content of `content_w`×`content_h` under a strip of
/// `header` pixels: exactly the content plus the strip, nothing else (issue #68).
#[must_use]
pub fn client_size(content_w: i32, content_h: i32, header: i32) -> (i32, i32) {
    (content_w.clamp(1, 16_384), content_h.clamp(1, 16_384).saturating_add(header.max(0)))
}

/// The plug-in content's size in a client area of `client_w`×`client_h` under a strip of
/// `header` pixels (the inverse of [`client_size`]).
#[must_use]
pub fn content_size(client_w: i32, client_h: i32, header: i32) -> (i32, i32) {
    (client_w.max(0), client_h.saturating_sub(header.max(0)).max(0))
}

/// A control of the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Control {
    /// The preset name and its dropdown.
    Preset,
    /// The previous preset.
    Previous,
    /// The next preset.
    Next,
    /// Save.
    Save,
    /// Save As…
    SaveAs,
    /// Load…
    Load,
    /// The bypass toggle.
    Bypass,
    /// The menu.
    Menu,
}

/// A rectangle in strip pixels (right and bottom exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge (exclusive).
    pub right: i32,
    /// Bottom edge (exclusive).
    pub bottom: i32,
}

impl Rect {
    /// Whether the point is inside.
    #[must_use]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// Where the strip's controls go in a strip `width` wide at `dpi`. Narrow plug-ins lose the
/// optional buttons first (Load…, Save As…, Save, previous/next, Bypass, in that order); what
/// they do stays in the menu, and the preset dropdown and the menu always show.
#[must_use]
pub fn layout(width: i32, dpi: u32) -> Vec<(Control, Rect)> {
    let h = header_height(dpi);
    let (pad, gap, inner) = (scale(6, dpi), scale(4, dpi), scale(4, dpi));
    let w = |c: Control| -> i32 {
        scale(
            match c {
                Control::Preset => 120,
                Control::Previous | Control::Next => 24,
                Control::Save => 44,
                Control::SaveAs => 66,
                Control::Load => 54,
                Control::Bypass => 60,
                Control::Menu => 28,
            },
            dpi,
        )
    };
    // Dropped from the end of this list first.
    let mut optional = vec![Control::Bypass, Control::Previous, Control::Save, Control::SaveAs, Control::Load];
    let needed = |opt: &[Control]| -> i32 {
        let n = i32::try_from(opt.len()).unwrap_or(0) + i32::from(opt.contains(&Control::Previous));
        let widths: i32 = opt.iter().map(|&c| w(c)).sum::<i32>() + if opt.contains(&Control::Previous) { w(Control::Next) } else { 0 };
        2 * pad + w(Control::Preset) + w(Control::Menu) + widths + gap * (n + 1)
    };
    while !optional.is_empty() && needed(&optional) > width {
        optional.pop();
    }
    let (top, bottom) = (inner, (h - inner).max(inner + 1));
    let mut out = Vec::new();
    // Right group: the menu at the edge, Bypass before it.
    let mut right = width - pad;
    let menu = Rect { left: right - w(Control::Menu), top, right, bottom };
    right = menu.left - gap;
    out.push((Control::Menu, menu));
    if optional.contains(&Control::Bypass) {
        let r = Rect { left: right - w(Control::Bypass), top, right, bottom };
        right = r.left - gap;
        out.push((Control::Bypass, r));
    }
    // Left group: the buttons after the dropdown take their widths; the dropdown takes the rest,
    // up to a comfortable width.
    let buttons: Vec<Control> = [Control::Previous, Control::Next, Control::Save, Control::SaveAs, Control::Load]
        .into_iter()
        .filter(|c| optional.contains(c) || (*c == Control::Next && optional.contains(&Control::Previous)))
        .collect();
    let fixed: i32 = buttons.iter().map(|&c| w(c) + gap).sum();
    let preset_w = (right - pad - fixed).clamp(scale(40, dpi).min(right - pad).max(0), scale(280, dpi));
    let mut x = pad;
    out.push((Control::Preset, Rect { left: x, top, right: x + preset_w, bottom }));
    x += preset_w + gap;
    for c in buttons {
        out.push((c, Rect { left: x, top, right: x + w(c), bottom }));
        x += w(c) + gap;
    }
    out
}

/// The control under a point of the strip.
#[must_use]
pub fn hit(width: i32, dpi: u32, x: i32, y: i32) -> Option<Control> {
    layout(width, dpi).into_iter().find(|(_, r)| r.contains(x, y)).map(|(c, _)| c)
}

/// An entry of a dropdown or of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    /// A section title (not selectable).
    Heading(String),
    /// A selectable entry; `id` maps back to a click ([`click_for`]).
    Item {
        /// The id [`click_for`] maps.
        id: u32,
        /// What it reads.
        label: String,
        /// Shown checked.
        checked: bool,
        /// Selectable.
        enabled: bool,
    },
    /// A separator line.
    Separator,
}

const ID_SAVE: u32 = 1;
const ID_SAVE_AS: u32 = 2;
const ID_LOAD: u32 = 3;
const ID_BYPASS: u32 = 4;
const ID_RESET: u32 = 5;
const ID_COPY: u32 = 6;
const ID_PASTE: u32 = 7;
const ID_DELETE: u32 = 8;
const ID_SHOW: u32 = 9;
const ID_USER: u32 = 1_000;
const ID_FACTORY: u32 = 20_000;

fn item(id: u32, label: &str, checked: bool, enabled: bool) -> MenuItem {
    MenuItem::Item { id, label: label.to_owned(), checked, enabled }
}

/// The preset dropdown: a User section and, when the format exposes one, a read-only Factory
/// section (selecting a factory preset loads it; it cannot be saved over).
#[must_use]
pub fn preset_items(view: &HeaderView) -> Vec<MenuItem> {
    let mut out = vec![MenuItem::Heading("User".into())];
    if view.user.is_empty() {
        out.push(item(0, "No user presets yet (Save As…)", false, false));
    }
    for (i, name) in view.user.iter().enumerate().take(MAX_LISTED) {
        let id = ID_USER + u32::try_from(i).unwrap_or(0);
        out.push(item(id, name, *name == view.preset, true));
    }
    if !view.factory.is_empty() {
        out.push(MenuItem::Separator);
        out.push(MenuItem::Heading("Factory".into()));
        for (i, name) in view.factory.iter().enumerate().take(MAX_LISTED) {
            let id = ID_FACTORY + u32::try_from(i).unwrap_or(0);
            out.push(item(id, name, *name == view.preset, true));
        }
    }
    out
}

/// The strip's menu: everything the buttons do (a narrow strip hides some), plus Reset, Copy,
/// Paste, Delete and Show in Device Chain.
#[must_use]
pub fn menu_items(view: &HeaderView) -> Vec<MenuItem> {
    let mut out = vec![item(ID_SAVE, "Save", false, true), item(ID_SAVE_AS, "Save As…", false, true), item(ID_LOAD, "Load…", false, true)];
    if view.can_bypass {
        out.push(MenuItem::Separator);
        out.push(item(ID_BYPASS, "Bypass", view.bypassed, true));
    }
    out.push(MenuItem::Separator);
    out.push(item(ID_RESET, "Reset to Default", false, true));
    out.push(item(ID_COPY, "Copy State", false, true));
    out.push(item(ID_PASTE, "Paste State", false, view.can_paste));
    if view.can_delete {
        out.push(item(ID_DELETE, &format!("Delete \"{}\"", view.preset), false, true));
    }
    out.push(MenuItem::Separator);
    out.push(item(ID_SHOW, "Show in Device Chain", false, true));
    out
}

/// The click a dropdown or menu id stands for.
#[must_use]
pub fn click_for(id: u32) -> Option<HeaderClick> {
    Some(match id {
        ID_SAVE => HeaderClick::Save,
        ID_SAVE_AS => HeaderClick::SaveAs,
        ID_LOAD => HeaderClick::Load,
        ID_BYPASS => HeaderClick::Bypass,
        ID_RESET => HeaderClick::Reset,
        ID_COPY => HeaderClick::Copy,
        ID_PASTE => HeaderClick::Paste,
        ID_DELETE => HeaderClick::Delete,
        ID_SHOW => HeaderClick::ShowInChain,
        i if (ID_USER..ID_FACTORY).contains(&i) => HeaderClick::User((i - ID_USER) as usize),
        i if (ID_FACTORY..ID_FACTORY + 100_000).contains(&i) => HeaderClick::Factory((i - ID_FACTORY) as usize),
        _ => return None,
    })
}

/// The click a button stands for (the dropdown and the menu open a list instead).
#[must_use]
pub fn button_click(control: Control) -> Option<HeaderClick> {
    Some(match control {
        Control::Previous => HeaderClick::Previous,
        Control::Next => HeaderClick::Next,
        Control::Save => HeaderClick::Save,
        Control::SaveAs => HeaderClick::SaveAs,
        Control::Load => HeaderClick::Load,
        Control::Bypass => HeaderClick::Bypass,
        Control::Preset | Control::Menu => return None,
    })
}

/// Follows the size of the plug-in's own child window (issue #68: a plug-in whose child is not the
/// size it reported left an empty strip in the window, seen with Battery 4). A size the child
/// keeps for [`Follow::STABLE`] observations, and no sooner than [`Follow::GRACE`] after the
/// host last resized the window, becomes the content size.
#[derive(Debug, Default)]
pub struct Follow {
    seen: Option<(i32, i32)>,
    count: u32,
}

impl Follow {
    /// Observations a different child size must last before the window follows it.
    pub const STABLE: u32 = 3;
    /// How long after a resize the host made the child is left to catch up.
    pub const GRACE: Duration = Duration::from_millis(200);
    /// Smaller children are taken as not laid out yet.
    pub const MIN: i32 = 16;

    /// One observation: the child's size, the content area's size, and the time since the host
    /// last resized the window. `Some(size)`: resize the content to it.
    pub fn observe(&mut self, child: Option<(i32, i32)>, content: (i32, i32), since_resize: Duration) -> Option<(i32, i32)> {
        let Some(child) = child.filter(|&(w, h)| w >= Follow::MIN && h >= Follow::MIN && w <= 16_384 && h <= 16_384) else {
            *self = Follow::default();
            return None;
        };
        if child == content {
            *self = Follow::default();
            return None;
        }
        if self.seen == Some(child) {
            self.count += 1;
        } else {
            self.seen = Some(child);
            self.count = 1;
        }
        if self.count >= Follow::STABLE && since_resize >= Follow::GRACE {
            *self = Follow::default();
            return Some(child);
        }
        None
    }
}

/// When the host last resized a window (for [`Follow`]).
#[cfg(any(windows, test))]
fn since(at: Option<std::time::Instant>) -> Duration {
    at.map_or(Duration::MAX, |t| t.elapsed())
}

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{EditorWindow, resize_content};

#[cfg(not(windows))]
mod other {
    use super::HeaderCell;
    use std::ffi::c_void;
    use std::sync::Arc;

    /// No editor windows are hosted on this platform.
    pub struct EditorWindow;

    impl EditorWindow {
        /// Always an error here.
        ///
        /// # Errors
        /// Editor windows are not hosted on this platform.
        pub fn new(_title: &str, _w: f64, _h: f64, _header: Option<Arc<HeaderCell>>) -> Result<EditorWindow, String> {
            Err("plug-in editor windows are not hosted on this platform yet".into())
        }
        /// Null.
        #[must_use]
        pub fn raw(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        /// Null.
        #[must_use]
        pub fn parent(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        /// Nothing.
        pub fn raise(&self) {}
        /// False.
        #[must_use]
        pub fn is_visible(&self) -> bool {
            false
        }
        /// Nothing.
        pub fn sync(&self) {}
        /// Nothing.
        pub fn close(self) {}
    }

    /// Nothing.
    pub fn resize_content(_window: *mut c_void, _w: f64, _h: f64) {}
}
#[cfg(not(windows))]
pub use other::{EditorWindow, resize_content};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_client_area_is_the_content_plus_the_strip_and_nothing_else() {
        assert_eq!(client_size(1010, 600, 30), (1010, 630));
        assert_eq!(content_size(1010, 630, 30), (1010, 600));
        for dpi in [96, 120, 144, 192] {
            let h = header_height(dpi);
            let (w, ch) = client_size(733, 411, h);
            assert_eq!(content_size(w, ch, h), (733, 411), "round trip at {dpi} dpi");
        }
        assert_eq!(header_height(96), 30);
        assert_eq!(header_height(144), 45);
        assert_eq!(header_height(0), 30, "unknown DPI reads as 96");
        assert_eq!(client_size(0, -5, 30), (1, 31), "degenerate sizes clamp");
        assert_eq!(client_size(100, 100, 0), (100, 100), "no strip, no extra height");
    }

    #[test]
    fn the_layout_fits_the_width_and_never_overlaps() {
        for dpi in [96, 144] {
            for width in [60, 150, 240, 320, 400, 520, 640, 1200] {
                let l = layout(width, dpi);
                assert!(l.iter().any(|(c, _)| *c == Control::Preset));
                assert!(l.iter().any(|(c, _)| *c == Control::Menu));
                for (i, (_, a)) in l.iter().enumerate() {
                    for (_, b) in l.iter().skip(i + 1) {
                        assert!(a.right <= b.left || b.right <= a.left, "overlap at {width}: {l:?}");
                    }
                }
                if width >= 520 {
                    assert!(l.iter().all(|(_, r)| r.left >= 0 && r.right <= width), "fits {width}: {l:?}");
                }
            }
        }
        let wide = layout(1200, 96);
        assert_eq!(wide.len(), 8, "every control on a wide plug-in");
        let narrow = layout(260, 96);
        assert!(!narrow.iter().any(|(c, _)| *c == Control::Load), "Load… goes first");
        assert_eq!(hit(1200, 96, 10, 15), Some(Control::Preset));
        assert_eq!(hit(1200, 96, 10, 0), None, "the margin above the controls");
    }

    #[test]
    fn menu_ids_map_back_to_their_clicks() {
        let view = HeaderView {
            preset: "Kit B".into(),
            user: vec!["Kit A".into(), "Kit B".into()],
            factory: vec!["Init".into()],
            can_bypass: true,
            can_delete: true,
            ..HeaderView::default()
        };
        let ids = |items: Vec<MenuItem>| -> Vec<HeaderClick> {
            items.into_iter().filter_map(|m| if let MenuItem::Item { id, enabled: true, .. } = m { click_for(id) } else { None }).collect()
        };
        assert_eq!(ids(preset_items(&view)), vec![HeaderClick::User(0), HeaderClick::User(1), HeaderClick::Factory(0)]);
        assert!(preset_items(&view).contains(&item(ID_USER + 1, "Kit B", true, true)), "the current one is checked");
        let menu = ids(menu_items(&view));
        for c in [
            HeaderClick::Save,
            HeaderClick::SaveAs,
            HeaderClick::Load,
            HeaderClick::Bypass,
            HeaderClick::Reset,
            HeaderClick::Copy,
            HeaderClick::Delete,
            HeaderClick::ShowInChain,
        ] {
            assert!(menu.contains(&c), "{c:?} in the menu");
        }
        assert!(!menu.contains(&HeaderClick::Paste), "nothing copied: Paste is disabled");
        assert_eq!(click_for(0), None);
        assert_eq!(click_for(12_345_678), None);
    }

    #[test]
    fn a_disabled_strip_drops_clicks_and_a_cell_reports_changes_once() {
        let cell = HeaderCell::new();
        let mut view = HeaderView { user: vec!["A".into()], ..HeaderView::default() };
        assert!(cell.set(&view));
        assert!(!cell.set(&view), "unchanged");
        cell.click(HeaderClick::User(0));
        cell.click(HeaderClick::User(5));
        cell.click(HeaderClick::Bypass);
        assert_eq!(cell.take_clicks(), vec![HeaderClick::User(0)], "out-of-range and unavailable clicks are dropped");
        view.disabled = Some("not responding".into());
        let g = cell.generation();
        assert!(cell.set(&view));
        assert_eq!(cell.generation(), g + 1);
        cell.click(HeaderClick::Save);
        assert!(cell.take_clicks().is_empty(), "disabled: nothing goes through");
    }

    #[test]
    fn the_size_follow_waits_for_a_stable_child_and_the_grace_after_a_resize() {
        let mut f = Follow::default();
        let long = Duration::from_secs(1);
        assert_eq!(f.observe(Some((900, 500)), (1000, 500), long), None);
        assert_eq!(f.observe(Some((900, 500)), (1000, 500), long), None);
        assert_eq!(f.observe(Some((900, 500)), (1000, 500), long), Some((900, 500)), "stable for three looks");
        // A child still catching up with a resize the host just made is left alone.
        let mut f = Follow::default();
        for _ in 0..5 {
            assert_eq!(f.observe(Some((640, 480)), (800, 600), Duration::from_millis(50)), None);
        }
        assert_eq!(f.observe(Some((640, 480)), (800, 600), long), Some((640, 480)));
        // A size that changes restarts the count; a matching or tiny child resets it.
        let mut f = Follow::default();
        f.observe(Some((700, 400)), (800, 400), long);
        f.observe(Some((710, 400)), (800, 400), long);
        assert_eq!(f.observe(Some((710, 400)), (800, 400), long), None);
        assert_eq!(f.observe(Some((800, 400)), (800, 400), long), None);
        assert_eq!(f.observe(Some((4, 4)), (800, 400), long), None);
        assert_eq!(f.observe(None, (800, 400), long), None);
        assert_eq!(since(None), Duration::MAX);
    }
}
