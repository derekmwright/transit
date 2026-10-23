//! The Windows editor window (see the parent module): a top-level frame, the header strip and the
//! content container the plug-in parents its view in, all owned by the thread that made them.
//!
//! The frame's per-window data ([`Data`]) is boxed, its pointer kept in the frame's
//! `GWLP_USERDATA` from the moment `CreateWindowExW` returns until `WM_NCDESTROY`, which frees it.
//! It is only ever read on the frame's own thread (window procedures run there, and every public
//! entry point checks the calling thread first), and only through `Cell`s, so a window procedure
//! that re-enters (a popup menu runs a modal loop inside one) never finds it borrowed.

// Window sizes are i32 pixels clamped to 16 384 and message words are documented 16-bit fields:
// the casts below cannot lose anything that matters.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]

use super::{
    Control, Follow, HeaderCell, HeaderClick, HeaderView, MenuItem, button_click, click_for, content_size, layout, menu_items, preset_items,
};
use std::cell::Cell;
use std::ffi::c_void;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW, CreatePen, CreateSolidBrush, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, HDC, HFONT, HGDIOBJ,
    InvalidateRect, PAINTSTRUCT, PS_SOLID, RoundRect, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetActiveWindow, GetFocus, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, GCW_ATOM,
    GW_CHILD, GWL_EXSTYLE, GWL_STYLE, GWLP_USERDATA, GetClassLongPtrW, GetClientRect, GetParent, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IDC_ARROW, IsChild, IsWindow, IsWindowVisible, LoadCursorW, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MINMAXINFO,
    MoveWindow, NONCLIENTMETRICSW, RegisterClassExW, SPI_GETNONCLIENTMETRICS, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_TOPALIGN,
    TrackPopupMenu, WA_INACTIVE, WM_ACTIVATE, WM_CAPTURECHANGED, WM_CLOSE, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_SETFOCUS, WM_SIZE, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
    WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
};

/// `WM_MOUSELEAVE` (winuser.h).
const WM_MOUSELEAVE: u32 = 0x02A3;
const FRAME_STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}
const BG: COLORREF = rgb(0x1E, 0x1F, 0x22);
const CONTROL: COLORREF = rgb(0x2E, 0x30, 0x36);
const HOVER: COLORREF = rgb(0x3A, 0x3D, 0x45);
const BORDER: COLORREF = rgb(0x45, 0x48, 0x50);
const TEXT: COLORREF = rgb(0xE6, 0xE6, 0xE6);
const DIM: COLORREF = rgb(0x7A, 0x7F, 0x87);
const BYPASSED: COLORREF = rgb(0xC7, 0x7D, 0x1A);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().filter(|&c| c != 0).chain(Some(0)).collect()
}

fn owns_window(hwnd: HWND) -> bool {
    // SAFETY: both calls accept any handle value (an invalid one yields 0) and take no buffers.
    unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) == windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

/// The frame's per-window state (see the module docs).
struct Data {
    header: Cell<HWND>,
    content: Cell<HWND>,
    cell: Option<Arc<HeaderCell>>,
    dpi: Cell<u32>,
    font: Cell<HFONT>,
    /// The plug-in content's size in pixels (what the plug-in reported or its child measures).
    size: Cell<(i32, i32)>,
    /// When the host last resized the frame (the size follow leaves the child that long).
    resized: Cell<Option<Instant>>,
    follow: Cell<(Option<(i32, i32)>, u32)>,
    /// The header generation and title last shown.
    shown: Cell<u64>,
    hover: Cell<Option<Control>>,
    pressed: Cell<Option<Control>>,
    tracking: Cell<bool>,
    /// The plug-in window that had focus when the frame was deactivated.
    focus: Cell<HWND>,
}

impl Data {
    fn header_h(&self) -> i32 {
        if self.cell.is_some() { super::header_height(self.dpi.get()) } else { 0 }
    }

    fn view(&self) -> HeaderView {
        self.cell.as_ref().map(|c| c.view()).unwrap_or_default()
    }
}

/// The frame's data, if `hwnd` is a live frame of this thread.
fn data<'a>(hwnd: HWND) -> Option<&'a Data> {
    if hwnd.is_null() || !owns_window(hwnd) {
        return None;
    }
    // SAFETY: `IsWindow` and `GetClassLongPtrW` accept any handle; only a window of our frame
    // class is read further.
    if unsafe { IsWindow(hwnd) } == 0 || unsafe { GetClassLongPtrW(hwnd, GCW_ATOM) } != usize::from(classes()?.frame) {
        return None;
    }
    // SAFETY: a frame of this thread: its user data is null or the `Data` box `EditorWindow::new`
    // put there, freed only by `WM_NCDESTROY` on this thread (after which the window is gone and
    // `IsWindow` above fails). The reference does not outlive this thread's use of the window:
    // callers use it within one message or one method call.
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Data).as_ref() }
}

#[derive(Clone, Copy)]
struct Classes {
    frame: u16,
    header: u16,
    content: u16,
}

fn register(name: &str, proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT) -> Option<u16> {
    // The name must outlive the class: kept for the process.
    let name: &'static [u16] = Box::leak(wide(name).into_boxed_slice());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        // SAFETY: null asks for this process's module handle.
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        hIcon: std::ptr::null_mut(),
        // SAFETY: a system cursor (null instance and a predefined id).
        hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: name.as_ptr(),
        hIconSm: std::ptr::null_mut(),
    };
    // SAFETY: a fully initialized class description whose name lives for the process.
    let atom = unsafe { RegisterClassExW(&raw const wc) };
    (atom != 0).then_some(atom)
}

/// The three window classes, registered once per process.
fn classes() -> Option<Classes> {
    static CLASSES: OnceLock<Option<Classes>> = OnceLock::new();
    *CLASSES.get_or_init(|| {
        Some(Classes {
            frame: register("TransitEditorFrame", frame_proc)?,
            header: register("TransitEditorHeader", header_proc)?,
            content: register("TransitEditorContent", content_proc)?,
        })
    })
}

fn dpi_of(hwnd: HWND) -> u32 {
    // SAFETY: accepts any handle; 0 for an invalid one (then 96 is used).
    let d = unsafe { GetDpiForWindow(hwnd) };
    if d == 0 { 96 } else { d }
}

/// The outer size of a frame whose client area is `w`×`h` at `dpi`.
fn outer(hwnd: HWND, w: i32, h: i32, dpi: u32) -> (i32, i32) {
    // SAFETY: style reads of a live window of this thread (null reads 0, giving the default).
    let (style, ex) = unsafe { (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32, GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) };
    let style = if style == 0 { FRAME_STYLE } else { style };
    let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
    // SAFETY: `r` is a valid, writable rect.
    unsafe { AdjustWindowRectExForDpi(&raw mut r, style, 0, ex, dpi) };
    (r.right.saturating_sub(r.left), r.bottom.saturating_sub(r.top))
}

fn font_for(dpi: u32) -> HFONT {
    let mut m = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..NONCLIENTMETRICSW::default() };
    // SAFETY: `m` is a writable NONCLIENTMETRICSW whose size is passed and set in `cbSize`.
    let ok = unsafe { SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, m.cbSize, (&raw mut m).cast::<c_void>(), 0, dpi) } != 0;
    if !ok {
        return std::ptr::null_mut();
    }
    // SAFETY: a LOGFONTW the system just filled in.
    unsafe { CreateFontIndirectW(&raw const m.lfMessageFont) }
}

/// Resizes the frame so its content area is `w`×`h` (plus the strip), keeping its position.
fn set_content(hwnd: HWND, d: &Data, w: i32, h: i32) {
    let (cw, ch) = super::client_size(w, h, d.header_h());
    d.size.set((cw, ch - d.header_h()));
    d.resized.set(Some(Instant::now()));
    let (ow, oh) = outer(hwnd, cw, ch, d.dpi.get());
    // SAFETY: a live frame of this thread; only its size changes (WM_SIZE lays the children out).
    unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
    lay_out(hwnd, d);
}

/// Places the strip and the content container in the frame's client area.
fn lay_out(hwnd: HWND, d: &Data) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live frame of this thread; `r` is writable.
    unsafe { GetClientRect(hwnd, &raw mut r) };
    let hh = d.header_h();
    let (w, h) = content_size(r.right - r.left, r.bottom - r.top, hh);
    // SAFETY: the children are null (skipped by MoveWindow's failure) or live children of this
    // frame, on this thread.
    unsafe {
        if !d.header.get().is_null() {
            MoveWindow(d.header.get(), 0, 0, w, hh, 1);
        }
        if !d.content.get().is_null() {
            MoveWindow(d.content.get(), 0, hh, w, h, 1);
        }
    }
}

/// The plug-in's own top child window in the content container, if it made one.
fn plugin_child(d: &Data) -> HWND {
    let content = d.content.get();
    if content.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a live child of this thread's frame; null when it has no child.
    unsafe { GetWindow(content, GW_CHILD) }
}

/// Gives keyboard focus back to the plug-in: the window it had, or its top child. False when the
/// plug-in has no window to take it.
fn focus_plugin(d: &Data) -> bool {
    let content = d.content.get();
    let last = d.focus.get();
    // SAFETY: `IsWindow`/`IsChild` accept any handles; `SetFocus` only gets a live descendant of
    // our content container, which belongs to this thread's frame.
    unsafe {
        let target = if !last.is_null() && IsWindow(last) != 0 && IsChild(content, last) != 0 { last } else { plugin_child(d) };
        if target.is_null() {
            return false;
        }
        SetFocus(target);
        true
    }
}

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE => {
            // Closing only hides: the editor link notices in `idle` and tears the view down first.
            // SAFETY: the window this procedure was called for.
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            return 0;
        }
        WM_GETMINMAXINFO => {
            // The frame may be as small as the plug-in: no system minimum width (which would
            // leave an empty strip beside a narrow plug-in).
            // SAFETY: for this message `lparam` points to the MINMAXINFO to fill in.
            if let Some(m) = unsafe { (lparam as *mut MINMAXINFO).as_mut() } {
                m.ptMinTrackSize = POINT { x: 1, y: 1 };
            }
            return 0;
        }
        WM_SIZE => {
            if let Some(d) = data(hwnd) {
                lay_out(hwnd, d);
            }
        }
        WM_ACTIVATE => {
            if let Some(d) = data(hwnd) {
                if (wparam & 0xFFFF) as u32 == WA_INACTIVE {
                    // SAFETY: no arguments; the focus window of this thread (or null).
                    let f = unsafe { GetFocus() };
                    // SAFETY: `IsChild` accepts any handles.
                    if !f.is_null() && unsafe { IsChild(d.content.get(), f) } != 0 {
                        d.focus.set(f);
                    }
                } else if focus_plugin(d) {
                    return 0;
                }
            }
        }
        WM_SETFOCUS => {
            if data(hwnd).is_some_and(focus_plugin) {
                return 0;
            }
        }
        WM_DPICHANGED => {
            if let Some(d) = data(hwnd) {
                let dpi = (wparam & 0xFFFF) as u32;
                d.dpi.set(if dpi == 0 { 96 } else { dpi });
                let old = d.font.replace(font_for(d.dpi.get()));
                if !old.is_null() {
                    // SAFETY: the font this frame created and no DC still has selected (painting
                    // selects it only within one WM_PAINT).
                    unsafe { DeleteObject(old as HGDIOBJ) };
                }
                // The plug-in keeps its pixel size; the strip follows the new DPI.
                let (w, h) = d.size.get();
                set_content(hwnd, d, w, h);
                return 0;
            }
        }
        WM_NCDESTROY => {
            // SAFETY: the user data is null or our `Data` box (see the module docs); it is
            // cleared before the box is freed, so nothing reads it afterwards.
            let p = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *mut Data;
            if !p.is_null() {
                // SAFETY: the pointer `Box::into_raw` gave in `EditorWindow::new`, freed once here.
                let d = unsafe { Box::from_raw(p) };
                let font = d.font.get();
                if !font.is_null() {
                    // SAFETY: the font this frame created; its windows are gone.
                    unsafe { DeleteObject(font as HGDIOBJ) };
                }
            }
        }
        _ => {}
    }
    // SAFETY: default handling of a message for the window it was sent to.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn content_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_ERASEBKGND {
        // The plug-in covers the area; leave it to it (no flicker).
        return 1;
    }
    // SAFETY: default handling of a message for the window it was sent to.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    (i32::from((lparam & 0xFFFF) as u16 as i16), i32::from(((lparam >> 16) & 0xFFFF) as u16 as i16))
}

fn strip_width(hwnd: HWND) -> i32 {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live window of this thread; `r` is writable.
    unsafe { GetClientRect(hwnd, &raw mut r) };
    r.right - r.left
}

/// Shows a popup list under `at` and returns the click chosen (the call runs a modal loop).
fn popup(hwnd: HWND, items: &[MenuItem], at: super::Rect) -> Option<HeaderClick> {
    // SAFETY: no arguments; a new empty menu (null on failure).
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }
    for it in items {
        let (flags, id, text) = match it {
            MenuItem::Heading(t) => (MF_STRING | MF_GRAYED, 0, wide(t)),
            MenuItem::Item { id, label, checked, enabled } => (
                MF_STRING | if *checked { MF_CHECKED } else { 0 } | if *enabled { 0 } else { MF_GRAYED },
                *id as usize,
                wide(&label.replace('&', "&&")),
            ),
            MenuItem::Separator => (MF_SEPARATOR, 0, wide("")),
        };
        // SAFETY: a live menu we own; `text` is NUL-terminated and outlives the call.
        unsafe { AppendMenuW(menu, flags, id, text.as_ptr()) };
    }
    let mut p = POINT { x: at.left, y: at.bottom };
    // SAFETY: a live window of this thread; `p` is writable.
    unsafe { ClientToScreen(hwnd, &raw mut p) };
    // SAFETY: a live menu and window of this thread; with TPM_RETURNCMD the result is the chosen
    // id (0: none), and the call returns when the menu closes.
    let id = unsafe { TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN | TPM_TOPALIGN, p.x, p.y, 0, hwnd, std::ptr::null()) };
    // SAFETY: the menu created above, no longer shown.
    unsafe { DestroyMenu(menu) };
    u32::try_from(id).ok().and_then(click_for)
}

unsafe extern "system" fn header_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: a child window's parent (null for none); `data` checks it is our frame.
    let parent = unsafe { GetParent(hwnd) };
    let Some(d) = data(parent) else {
        // SAFETY: default handling of a message for the window it was sent to.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let invalidate = || {
        // SAFETY: a live window of this thread; the whole client area.
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
    };
    match msg {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            paint(hwnd, d);
            return 0;
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lparam);
            let over = super::hit(strip_width(hwnd), d.dpi.get(), x, y);
            if over != d.hover.get() {
                d.hover.set(over);
                invalidate();
            }
            if !d.tracking.get() {
                let mut t =
                    TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                // SAFETY: a valid TRACKMOUSEEVENT for a live window of this thread.
                if unsafe { TrackMouseEvent(&raw mut t) } != 0 {
                    d.tracking.set(true);
                }
            }
            return 0;
        }
        WM_MOUSELEAVE => {
            d.tracking.set(false);
            if d.hover.take().is_some() {
                invalidate();
            }
            return 0;
        }
        WM_LBUTTONDOWN => {
            let view = d.view();
            let (x, y) = lparam_point(lparam);
            let width = strip_width(hwnd);
            let Some(control) = super::hit(width, d.dpi.get(), x, y) else { return 0 };
            if view.disabled.is_some() {
                return 0;
            }
            let rect = layout(width, d.dpi.get()).into_iter().find(|(c, _)| *c == control).map(|(_, r)| r).unwrap_or_default();
            let list = match control {
                Control::Preset => Some(preset_items(&view)),
                Control::Menu => Some(menu_items(&view)),
                _ => None,
            };
            if let Some(items) = list {
                d.pressed.set(Some(control));
                invalidate();
                let chosen = popup(hwnd, &items, rect);
                d.pressed.set(None);
                invalidate();
                if let (Some(click), Some(cell)) = (chosen, &d.cell) {
                    cell.click(click);
                }
                return 0;
            }
            d.pressed.set(Some(control));
            // SAFETY: a live window of this thread takes the mouse until the button is released.
            unsafe { SetCapture(hwnd) };
            invalidate();
            return 0;
        }
        WM_LBUTTONUP => {
            let pressed = d.pressed.take();
            // SAFETY: releases this thread's capture (harmless when it holds none).
            unsafe { ReleaseCapture() };
            let (x, y) = lparam_point(lparam);
            let over = super::hit(strip_width(hwnd), d.dpi.get(), x, y);
            if let (Some(p), Some(cell)) = (pressed, &d.cell)
                && over == Some(p)
                && let Some(click) = button_click(p)
            {
                cell.click(click);
            }
            invalidate();
            return 0;
        }
        WM_CAPTURECHANGED => {
            if d.pressed.take().is_some() {
                invalidate();
            }
            return 0;
        }
        _ => {}
    }
    // SAFETY: default handling of a message for the window it was sent to.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// One rounded control: fill, border and centered (or left-aligned) text.
fn draw_control(dc: HDC, r: super::Rect, fill: COLORREF, text: &str, color: COLORREF, left: bool, radius: i32) {
    // SAFETY: `dc` is the memory DC of the current WM_PAINT; every object created here is selected
    // out and deleted before returning.
    unsafe {
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(PS_SOLID, 1, BORDER);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        RoundRect(dc, r.left, r.top, r.right, r.bottom, radius, radius);
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        DeleteObject(brush as HGDIOBJ);
        DeleteObject(pen as HGDIOBJ);
        SetTextColor(dc, color);
        let inset = if left { (r.bottom - r.top) / 3 } else { 2 };
        let mut tr = RECT { left: r.left + inset, top: r.top, right: r.right - inset, bottom: r.bottom };
        let w: Vec<u16> = text.encode_utf16().collect();
        let align = if left { DT_LEFT } else { DT_CENTER };
        DrawTextW(
            dc,
            w.as_ptr(),
            i32::try_from(w.len()).unwrap_or(0),
            &raw mut tr,
            align | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
}

fn paint(hwnd: HWND, d: &Data) {
    let view = d.view();
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: a live window of this thread in WM_PAINT; `ps` is writable and ended below.
    let dc = unsafe { BeginPaint(hwnd, &raw mut ps) };
    if dc.is_null() {
        return;
    }
    let width = strip_width(hwnd);
    let height = d.header_h().max(1);
    // SAFETY: a memory DC and bitmap compatible with the paint DC, selected, drawn into, copied out
    // and released (bitmap deselected and deleted, DC deleted) before EndPaint.
    unsafe {
        let mem = CreateCompatibleDC(dc);
        let bmp = CreateCompatibleBitmap(dc, width.max(1), height);
        let target = if mem.is_null() || bmp.is_null() { dc } else { mem };
        let old_bmp = if target == mem { SelectObject(mem, bmp as HGDIOBJ) } else { std::ptr::null_mut() };
        let font = d.font.get();
        let old_font = if font.is_null() { std::ptr::null_mut() } else { SelectObject(target, font as HGDIOBJ) };
        let bg = CreateSolidBrush(BG);
        let all = RECT { left: 0, top: 0, right: width, bottom: height };
        FillRect(target, &raw const all, bg);
        DeleteObject(bg as HGDIOBJ);
        SetBkMode(target, TRANSPARENT as i32);
        let disabled = view.disabled.is_some();
        let radius = super::scale(6, d.dpi.get());
        for (control, r) in layout(width, d.dpi.get()) {
            let active = !disabled && (d.hover.get() == Some(control) || d.pressed.get() == Some(control));
            let mut fill = if active { HOVER } else { CONTROL };
            let mut color = if disabled { DIM } else { TEXT };
            let label = match control {
                Control::Preset => match &view.disabled {
                    Some(why) => why.clone(),
                    None if view.preset.is_empty() => "No preset  \u{25BE}".to_owned(),
                    None => format!("{}  \u{25BE}", view.preset),
                },
                Control::Previous => "\u{25C0}".into(),
                Control::Next => "\u{25B6}".into(),
                Control::Save => "Save".into(),
                Control::SaveAs => "Save As\u{2026}".into(),
                Control::Load => "Load\u{2026}".into(),
                Control::Bypass => {
                    if view.bypassed {
                        fill = BYPASSED;
                        color = BG;
                    } else if !view.can_bypass {
                        color = DIM;
                    }
                    "Bypass".into()
                }
                Control::Menu => "\u{2261}".into(),
            };
            draw_control(target, r, fill, &label, color, control == Control::Preset, radius);
        }
        if !old_font.is_null() {
            SelectObject(target, old_font);
        }
        if target == mem {
            BitBlt(dc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old_bmp);
        }
        if !bmp.is_null() {
            DeleteObject(bmp as HGDIOBJ);
        }
        if !mem.is_null() {
            DeleteDC(mem);
        }
        EndPaint(hwnd, &raw const ps);
    }
}

/// A plug-in editor window (see the parent module). Owns the frame; destroyed by
/// [`EditorWindow::close`] on its thread.
pub struct EditorWindow {
    hwnd: HWND,
}

// SAFETY: the handle is only used on the thread that created the window (every method checks
// with `owns_window`); the struct merely carries it between threads' locks.
unsafe impl Send for EditorWindow {}

impl EditorWindow {
    /// Makes a hidden frame for plug-in content of `w`×`h` pixels titled `title`, with the strip
    /// when `header` is given. Call [`EditorWindow::raise`] to show it once the plug-in attached.
    ///
    /// # Errors
    /// When the window classes cannot be registered or the windows cannot be created.
    pub fn new(title: &str, w: f64, h: f64, header: Option<Arc<HeaderCell>>) -> Result<EditorWindow, String> {
        let classes = classes().ok_or_else(|| "cannot register the editor window classes".to_string())?;
        let title = wide(title);
        // SAFETY: null asks for this process's module handle.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        // SAFETY: the active window (or null) of this thread's queue.
        let owner = unsafe { GetActiveWindow() };
        // SAFETY: a registered class and NUL-terminated strings that outlive the call; the owner
        // is null or a window of this thread. Created hidden at a placeholder size.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                usize::from(classes.frame) as *const u16,
                title.as_ptr(),
                FRAME_STYLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                100,
                100,
                owner,
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err("cannot create the editor window".into());
        }
        let dpi = dpi_of(hwnd);
        let d = Box::new(Data {
            header: Cell::new(std::ptr::null_mut()),
            content: Cell::new(std::ptr::null_mut()),
            cell: header,
            dpi: Cell::new(dpi),
            font: Cell::new(font_for(dpi)),
            size: Cell::new((1, 1)),
            resized: Cell::new(None),
            follow: Cell::new((None, 0)),
            shown: Cell::new(u64::MAX),
            hover: Cell::new(None),
            pressed: Cell::new(None),
            tracking: Cell::new(false),
            focus: Cell::new(std::ptr::null_mut()),
        });
        let has_header = d.cell.is_some();
        // SAFETY: our frame, on this thread; the box is freed by its WM_NCDESTROY (module docs).
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(d) as isize) };
        let child = |class: u16, style: u32| {
            // SAFETY: a registered class; the parent is the live frame just made on this thread.
            unsafe {
                CreateWindowExW(
                    0,
                    usize::from(class) as *const u16,
                    std::ptr::null(),
                    WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | style,
                    0,
                    0,
                    1,
                    1,
                    hwnd,
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                )
            }
        };
        let content = child(classes.content, WS_CLIPCHILDREN);
        let strip = if has_header { child(classes.header, 0) } else { std::ptr::null_mut() };
        if content.is_null() || (has_header && strip.is_null()) {
            // SAFETY: the live frame just made (its children go with it); destroying it frees its
            // data, and no reference to that data is held here.
            unsafe { DestroyWindow(hwnd) };
            return Err("cannot create the editor window's parts".into());
        }
        let Some(d) = data(hwnd) else {
            // SAFETY: as above.
            unsafe { DestroyWindow(hwnd) };
            return Err("cannot create the editor window".into());
        };
        d.content.set(content);
        d.header.set(strip);
        set_content(hwnd, d, w as i32, h as i32);
        let win = EditorWindow { hwnd };
        win.sync();
        Ok(win)
    }

    fn live(&self) -> Option<&Data> {
        data(self.hwnd)
    }

    /// The frame's handle (for the plug-in's resize requests, [`resize_content`]).
    #[must_use]
    pub fn raw(&self) -> *mut c_void {
        self.hwnd
    }

    /// The window the plug-in makes its child window in (the content container); null once the
    /// window is gone or on another thread.
    #[must_use]
    pub fn parent(&self) -> *mut c_void {
        self.live().map_or(std::ptr::null_mut(), |d| d.content.get())
    }

    /// Shows the window and brings it to the front.
    pub fn raise(&self) {
        if self.live().is_some() {
            // SAFETY: a live window of this thread.
            unsafe {
                ShowWindow(self.hwnd, SW_SHOW);
                SetForegroundWindow(self.hwnd);
            }
        }
    }

    /// Whether the window is shown (closing it only hides it).
    #[must_use]
    pub fn is_visible(&self) -> bool {
        // SAFETY: a live window of this thread.
        self.live().is_some() && unsafe { IsWindowVisible(self.hwnd) } != 0
    }

    /// Housekeeping on the window's thread, from the editor's `idle`: shows header changes (title
    /// and strip) and follows the plug-in child's size ([`Follow`]).
    pub fn sync(&self) {
        let Some(d) = self.live() else { return };
        if let Some(cell) = &d.cell {
            let generation = cell.generation();
            if generation != d.shown.get() {
                d.shown.set(generation);
                let view = cell.view();
                if !view.title.is_empty() {
                    let t = wide(&view.title);
                    // SAFETY: a live window of this thread; `t` is NUL-terminated.
                    unsafe { SetWindowTextW(self.hwnd, t.as_ptr()) };
                }
                // SAFETY: a live child of this thread's frame.
                unsafe { InvalidateRect(d.header.get(), std::ptr::null(), 0) };
            }
        }
        let child = plugin_child(d);
        let size = (!child.is_null()).then(|| {
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: a window handle from GetWindow (any value is accepted); `r` is writable.
            unsafe { GetWindowRect(child, &raw mut r) };
            (r.right - r.left, r.bottom - r.top)
        });
        let (seen, count) = d.follow.get();
        let mut f = Follow { seen, count };
        let to = f.observe(size, d.size.get(), super::since(d.resized.get()));
        d.follow.set((f.seen, f.count));
        if let Some((w, h)) = to {
            log::debug!("editor window: the plug-in's view is {w}×{h}, not {:?}; the window follows it", d.size.get());
            set_content(self.hwnd, d, w, h);
        }
    }

    /// Destroys the window (after the plug-in's view was detached from it).
    pub fn close(self) {
        if self.live().is_some() {
            // SAFETY: our own live window on this thread; its data is freed by WM_NCDESTROY.
            unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

/// Resizes the plug-in content of the editor window `window` ([`EditorWindow::raw`]) to `w`×`h`,
/// the frame growing or shrinking by exactly that (the strip keeps its height). Does nothing for a
/// window that is not a live editor window of the calling thread.
pub fn resize_content(window: *mut c_void, w: f64, h: f64) {
    if let Some(d) = data(window) {
        set_content(window, d, w as i32, h as i32);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Duration;

    fn client(hwnd: HWND) -> (i32, i32) {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: a live window of this thread; `r` is writable.
        unsafe { GetClientRect(hwnd, &raw mut r) };
        (r.right - r.left, r.bottom - r.top)
    }

    #[test]
    fn the_frame_is_the_content_plus_the_strip_on_open_and_on_resize() {
        let cell = HeaderCell::new();
        let win = EditorWindow::new("Drums · Insert A · Fixture", 640.0, 400.0, Some(Arc::clone(&cell))).unwrap();
        let hh = super::super::header_height(dpi_of(win.raw()));
        assert_eq!(client(win.raw()), (640, 400 + hh));
        assert_eq!(client(win.parent()), (640, 400), "the plug-in's area is exactly what it reported");
        resize_content(win.raw(), 333.0, 222.0);
        assert_eq!(client(win.raw()), (333, 222 + hh));
        assert_eq!(client(win.parent()), (333, 222));
        // A narrow plug-in gets a narrow window: no system minimum width.
        resize_content(win.raw(), 90.0, 60.0);
        assert_eq!(client(win.raw()), (90, 60 + hh));
        // Without a strip, the window is the content alone.
        let bare = EditorWindow::new("bare", 300.0, 200.0, None).unwrap();
        assert_eq!(client(bare.raw()), (300, 200));
        bare.close();
        win.close();
    }

    #[test]
    fn the_window_follows_a_plugin_child_of_another_size() {
        let win = EditorWindow::new("follow", 800.0, 500.0, Some(HeaderCell::new())).unwrap();
        let classes = classes().unwrap();
        // The "plug-in" makes its child smaller than it reported (the Battery 4 strip).
        // SAFETY: a registered class; the parent is our live content container on this thread.
        let child = unsafe {
            CreateWindowExW(
                0,
                usize::from(classes.content) as *const u16,
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                700,
                480,
                win.parent(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        assert!(!child.is_null());
        std::thread::sleep(Follow::GRACE + Duration::from_millis(20));
        for _ in 0..Follow::STABLE {
            win.sync();
        }
        assert_eq!(client(win.parent()), (700, 480), "the content area follows the child");
        let hh = super::super::header_height(dpi_of(win.raw()));
        assert_eq!(client(win.raw()), (700, 480 + hh));
        win.close();
    }

    #[test]
    fn a_title_change_reaches_the_frame_and_other_threads_cannot_drive_it() {
        let cell = HeaderCell::new();
        let win = EditorWindow::new("old", 200.0, 100.0, Some(Arc::clone(&cell))).unwrap();
        cell.set(&HeaderView { title: "Drums · Insert A · Battery 4".into(), ..HeaderView::default() });
        win.sync();
        let mut buf = [0u16; 128];
        // SAFETY: a live window of this thread; the buffer and its length match.
        let n = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW(win.raw(), buf.as_mut_ptr(), 128) };
        assert_eq!(String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap()]), "Drums · Insert A · Battery 4");
        let raw = win.raw() as usize;
        let other = std::thread::spawn(move || {
            resize_content(raw as *mut c_void, 50.0, 50.0);
            data(raw as HWND).is_none()
        })
        .join()
        .unwrap();
        assert!(other, "another thread gets nothing");
        assert_eq!(client(win.parent()), (200, 100), "and changes nothing");
        win.close();
    }
}
