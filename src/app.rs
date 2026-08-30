//! Tray state and input handling: what is on the tray, where it sits, and what the
//! mouse is doing to it.
//!
//! Owned by the main window through `GWLP_USERDATA`; `main::wndproc` does nothing but
//! decode messages and call in here.

use crate::chrome::{BackBuffer, Button, Command, PaintCtx, Theme};
use crate::layout::{Geom, Hit, Layout, Metrics, Pane};
use crate::window_manager as wm;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, InvalidateRect, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
    RedrawWindow, ScreenToClient,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

/// Liveness/title poll. Cheap enough at this interval (a handful of `IsWindow`
/// calls) to be preferable to a system-wide `EVENT_OBJECT_DESTROY` hook, which
/// would wake us for every menu and tooltip on the desktop.
pub const TIMER_ID: usize = 1;
pub const TIMER_MS: u32 = 400;

/// How far the pointer must travel on a header before a click becomes a reorder drag.
const DRAG_SLOP: i32 = 4;

const TOOLBAR_ITEMS: [(Command, &str); 3] = [
    (Command::Add, "＋ ウィンドウを追加"),
    (Command::Rebalance, "均等に整列"),
    (Command::ReleaseAll, "すべて解放"),
];

enum Drag {
    VSplit(usize),
    HSplit(usize, usize),
    Move {
        from: (usize, usize),
        origin: POINT,
        armed: bool,
        target: Option<(usize, usize)>,
    },
}

pub struct App {
    hwnd: HWND,
    dpi: u32,
    m: Metrics,
    theme: Theme,
    buf: BackBuffer,
    layout: Layout,
    geom: Geom,
    buttons: Vec<Button>,
    hot: Hit,
    hot_button: Option<Command>,
    /// Toolbar button currently held down. Commands fire on release, not press:
    /// `TrackPopupMenu` opened on WM_LBUTTONDOWN is dismissed by the WM_LBUTTONUP
    /// that follows it.
    pressed_button: Option<Command>,
    /// Tracked by handle, not by index, so removals can never leave it dangling.
    active: Option<HWND>,
    drag: Option<Drag>,
    tracking_leave: bool,
}

impl App {
    pub fn new(hwnd: HWND) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let m = Metrics::for_dpi(dpi);
        let theme = Theme::new(&m);
        let mut app = Self {
            hwnd,
            dpi,
            m,
            theme,
            buf: BackBuffer::default(),
            layout: Layout::default(),
            geom: Geom::default(),
            buttons: Vec::new(),
            hot: Hit::None,
            hot_button: None,
            pressed_button: None,
            active: None,
            drag: None,
            tracking_leave: false,
        };
        app.build_buttons();
        app.relayout();
        app
    }

    // ---- geometry -------------------------------------------------------

    fn client(&self) -> RECT {
        let mut r = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut r).ok() };
        r
    }

    /// The area the tiling gets: everything below the toolbar.
    fn content(&self) -> RECT {
        RECT {
            top: self.m.toolbar,
            ..self.client()
        }
    }

    fn build_buttons(&mut self) {
        self.buttons.clear();
        let mut x = self.m.pad;
        for (cmd, label) in TOOLBAR_ITEMS {
            let w = self.theme.text_width(label) + self.m.pad * 4;
            self.buttons.push(Button {
                rect: RECT {
                    left: x,
                    top: self.m.pad / 3,
                    right: x + w,
                    bottom: self.m.toolbar - self.m.pad / 3,
                },
                label,
                cmd,
            });
            x += w + self.m.pad / 2;
        }
    }

    /// Recompute pixel geometry and push the adopted windows to their new rects.
    pub fn relayout(&mut self) {
        self.geom = self.layout.geometry(self.content(), &self.m);
        self.place_children();
        self.invalidate();
    }

    /// One batched pass so a Snap resize moves every adopted window in a single
    /// atomic update instead of N separate `SetWindowPos` repaints.
    fn place_children(&self) {
        let n = self.layout.pane_count();
        if n == 0 {
            return;
        }
        unsafe {
            let Ok(mut hdwp) = BeginDeferWindowPos(n as i32) else {
                return;
            };
            for (ci, col) in self.geom.slots.iter().enumerate() {
                for (si, g) in col.iter().enumerate() {
                    let Some(pane) = self.layout.pane_at(ci, si) else {
                        continue;
                    };
                    let r = g.client;
                    match DeferWindowPos(
                        hdwp,
                        pane.hwnd,
                        None,
                        r.left,
                        r.top,
                        (r.right - r.left).max(0),
                        (r.bottom - r.top).max(0),
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    ) {
                        Ok(next) => hdwp = next,
                        Err(_) => return,
                    }
                }
            }
            let _ = EndDeferWindowPos(hdwp);
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    // ---- panes ----------------------------------------------------------

    fn add_pane(&mut self, target: HWND) {
        if target == self.hwnd || self.layout.contains(target) || !wm::is_valid(target) {
            return;
        }
        let Some(snap) = wm::embed(target, self.hwnd) else {
            self.warn_embed_failed(&wm::window_title(target));
            return;
        };
        let mut title = wm::window_title(target);
        if title.is_empty() {
            title = "(無題)".into();
        }
        self.layout.add(Pane {
            hwnd: target,
            title,
            snap,
        });
        self.active = Some(target);
        self.relayout();
        self.refresh_children();
        wm::focus_child(target);
    }

    /// `SetParent` is refused for store apps and for windows owned by a process at a
    /// higher integrity level. Say so rather than appearing to do nothing.
    fn warn_embed_failed(&self, title: &str) {
        let text = wm::to_utf16(&format!(
            "「{title}」は取り込めませんでした。\n\n\
             ストアアプリ (UWP) や、管理者として実行されているアプリのウィンドウは、\n\
             Windows の制約により他のウィンドウの子にできません。"
        ));
        let caption = wm::to_utf16("SnapFrame");
        unsafe {
            MessageBoxW(
                Some(self.hwnd),
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
        }
    }

    /// Nudge adopted windows to repaint. Some apps (Paint, for one) leave stale
    /// pixels behind after being resized, so this runs when a resize *finishes* —
    /// never per mouse-move, which would make splitter drags crawl.
    fn refresh_children(&self) {
        unsafe {
            for pane in self.layout.panes() {
                let _ = RedrawWindow(
                    Some(pane.hwnd),
                    None,
                    None,
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
            }
        }
    }

    fn release_pane(&mut self, col: usize, slot: usize) {
        if let Some(pane) = self.layout.remove(col, slot) {
            if self.active == Some(pane.hwnd) {
                self.active = None;
            }
            wm::release(pane.hwnd, &pane.snap);
        }
        self.hot = Hit::None;
        self.relayout();
        self.adopt_focus();
    }

    /// Hand the keyboard to some surviving pane after the focused one goes away, the
    /// way closing a tab moves you to a neighbour rather than to nothing.
    fn adopt_focus(&mut self) {
        if self.active.is_some_and(wm::is_valid) {
            return;
        }
        self.active = self.layout.panes().next().map(|p| p.hwnd);
        if let Some(hwnd) = self.active {
            wm::focus_child(hwnd);
        }
    }

    /// Hand every adopted window back to the desktop. Also the shutdown path, so it
    /// must stay safe to call when the tray is already half torn down.
    pub fn release_all(&mut self) {
        let cols = std::mem::take(&mut self.layout.cols);
        for col in cols {
            for slot in col.slots {
                wm::release(slot.pane.hwnd, &slot.pane.snap);
            }
        }
        self.active = None;
        self.hot = Hit::None;
        self.relayout();
    }

    /// Drop panes whose window died on us, and pick up title changes.
    pub fn on_tick(&mut self) {
        let mut dead = Vec::new();
        for (ci, col) in self.layout.cols.iter().enumerate() {
            for (si, slot) in col.slots.iter().enumerate() {
                if !wm::is_valid(slot.pane.hwnd) {
                    dead.push((ci, si));
                }
            }
        }
        let mut dirty = !dead.is_empty();
        // Descending, so a column collapsing to empty cannot shift an index we
        // have not visited yet.
        for (ci, si) in dead.into_iter().rev() {
            if let Some(pane) = self.layout.remove(ci, si)
                && self.active == Some(pane.hwnd)
            {
                self.active = None;
            }
        }

        for col in &mut self.layout.cols {
            for slot in &mut col.slots {
                let title = wm::window_title(slot.pane.hwnd);
                if !title.is_empty() && title != slot.pane.title {
                    slot.pane.title = title;
                    dirty = true;
                }
            }
        }

        if dirty {
            self.relayout();
            self.adopt_focus();
        }
    }

    // ---- commands -------------------------------------------------------

    fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::Add => self.show_add_menu(),
            Command::Rebalance => {
                self.layout.rebalance();
                self.relayout();
            }
            Command::ReleaseAll => self.release_all(),
        }
    }

    fn show_add_menu(&mut self) {
        let candidates: Vec<(HWND, String)> = wm::enum_candidates()
            .into_iter()
            .filter(|(h, _)| *h != self.hwnd && !self.layout.contains(*h))
            .collect();

        let anchor = self
            .buttons
            .iter()
            .find(|b| b.cmd == Command::Add)
            .map(|b| POINT {
                x: b.rect.left,
                y: b.rect.bottom,
            })
            .unwrap_or_default();

        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            if candidates.is_empty() {
                let text = wm::to_utf16("(載せられるウィンドウがありません)");
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(text.as_ptr()));
            } else {
                for (i, (_, title)) in candidates.iter().enumerate() {
                    let text = wm::to_utf16(&ellipsize(title, 64));
                    let _ = AppendMenuW(menu, MF_STRING, i + 1, PCWSTR(text.as_ptr()));
                }
            }

            let mut pt = anchor;
            let _ = ClientToScreen(self.hwnd, &mut pt);
            let choice = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);

            let idx = choice.0 as usize;
            if idx >= 1 && idx <= candidates.len() {
                self.add_pane(candidates[idx - 1].0);
            }
        }
    }

    // ---- input ----------------------------------------------------------

    fn hit(&self, pt: POINT) -> (Hit, Option<Command>) {
        if pt.y < self.m.toolbar {
            let cmd = self
                .buttons
                .iter()
                .find(|b| crate::layout::contains(&b.rect, pt))
                .map(|b| b.cmd);
            return (Hit::None, cmd);
        }
        (self.geom.hit_test(pt), None)
    }

    pub fn on_mouse_move(&mut self, pt: POINT) {
        if !self.tracking_leave {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            self.tracking_leave = true;
        }

        match self.drag.take() {
            Some(Drag::VSplit(i)) => {
                self.layout
                    .drag_vsplit(i, pt.x, self.content(), &self.m);
                self.drag = Some(Drag::VSplit(i));
                self.relayout();
                return;
            }
            Some(Drag::HSplit(c, s)) => {
                self.layout
                    .drag_hsplit(c, s, pt.y, self.content(), &self.m);
                self.drag = Some(Drag::HSplit(c, s));
                self.relayout();
                return;
            }
            Some(Drag::Move {
                from,
                origin,
                mut armed,
                ..
            }) => {
                if !armed
                    && ((pt.x - origin.x).abs() > DRAG_SLOP || (pt.y - origin.y).abs() > DRAG_SLOP)
                {
                    armed = true;
                }
                let target = match self.geom.hit_test(pt) {
                    Hit::Header(c, s) | Hit::Close(c, s) | Hit::Client(c, s) if (c, s) != from => {
                        Some((c, s))
                    }
                    _ => None,
                };
                let target = if armed { target } else { None };
                self.drag = Some(Drag::Move {
                    from,
                    origin,
                    armed,
                    target,
                });
                self.invalidate();
                return;
            }
            None => {}
        }

        let (hot, hot_button) = self.hit(pt);
        if hot != self.hot || hot_button != self.hot_button {
            self.hot = hot;
            self.hot_button = hot_button;
            self.invalidate();
        }
    }

    pub fn on_mouse_leave(&mut self) {
        self.tracking_leave = false;
        if self.hot != Hit::None || self.hot_button.is_some() {
            self.hot = Hit::None;
            self.hot_button = None;
            self.invalidate();
        }
    }

    pub fn on_lbutton_down(&mut self, pt: POINT) {
        let (hit, button) = self.hit(pt);
        if let Some(cmd) = button {
            self.pressed_button = Some(cmd);
            unsafe { SetCapture(self.hwnd) };
            self.invalidate();
            return;
        }

        match hit {
            Hit::Close(c, s) => self.release_pane(c, s),
            Hit::VSplit(i) => {
                self.drag = Some(Drag::VSplit(i));
                unsafe { SetCapture(self.hwnd) };
            }
            Hit::HSplit(c, s) => {
                self.drag = Some(Drag::HSplit(c, s));
                unsafe { SetCapture(self.hwnd) };
            }
            Hit::Header(c, s) => {
                self.focus_pane(c, s);
                self.drag = Some(Drag::Move {
                    from: (c, s),
                    origin: pt,
                    armed: false,
                    target: None,
                });
                unsafe { SetCapture(self.hwnd) };
            }
            Hit::Client(c, s) => self.focus_pane(c, s),
            Hit::None => {}
        }
    }

    pub fn on_lbutton_up(&mut self, pt: POINT) {
        let drag = self.drag.take();
        let pressed = self.pressed_button.take();
        unsafe {
            let _ = ReleaseCapture();
        }

        if let Some(cmd) = pressed {
            self.invalidate();
            // Only fire if the pointer is still on the button it was pressed on.
            if self.hit(pt).1 == Some(cmd) {
                self.run_command(cmd);
            }
            return;
        }

        match drag {
            Some(Drag::Move {
                from,
                target: Some(to),
                armed: true,
                ..
            }) => {
                self.layout.swap(from, to);
                self.relayout();
                self.refresh_children();
            }
            Some(Drag::VSplit(_) | Drag::HSplit(..)) => {
                self.invalidate();
                self.refresh_children();
            }
            _ => self.invalidate(),
        }
    }

    fn focus_pane(&mut self, col: usize, slot: usize) {
        if let Some(pane) = self.layout.pane_at(col, slot) {
            let changed = self.active != Some(pane.hwnd);
            self.active = Some(pane.hwnd);
            wm::focus_child(pane.hwnd);
            if changed {
                self.invalidate();
            }
        }
    }

    fn cursor_client(&self) -> Option<POINT> {
        unsafe {
            let mut pt = POINT::default();
            GetCursorPos(&mut pt).ok()?;
            if !ScreenToClient(self.hwnd, &mut pt).as_bool() {
                return None;
            }
            Some(pt)
        }
    }

    /// Focus whichever pane the pointer is over. The point is resolved through the
    /// cached geometry rather than the clicked HWND, so a click on a deeply nested
    /// control inside an adopted app still resolves to its pane.
    fn focus_pane_at_cursor(&mut self) {
        let Some(pt) = self.cursor_client() else {
            return;
        };
        if let Hit::Header(c, s) | Hit::Close(c, s) | Hit::Client(c, s) = self.geom.hit_test(pt) {
            self.focus_pane(c, s);
        }
    }

    /// A click landed inside an adopted window.
    ///
    /// This is the whole reason click-to-focus works. The click itself goes straight
    /// to the child and never reaches our `wndproc`; a window that used to be
    /// top-level got its keyboard focus from activation and so never learned to call
    /// `SetFocus` on itself. As its container, we have to do it for it.
    pub fn on_parent_notify(&mut self, event: u32) {
        if matches!(
            event,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        ) {
            self.focus_pane_at_cursor();
        }
    }

    /// A click is about to activate the tray. Record which pane it landed on; the
    /// `WM_ACTIVATE` that follows is what actually moves the focus, because focus set
    /// before activation completes does not stick.
    pub fn on_mouse_activate(&mut self) {
        let Some(pt) = self.cursor_client() else {
            return;
        };
        if let Hit::Header(c, s) | Hit::Close(c, s) | Hit::Client(c, s) = self.geom.hit_test(pt)
            && let Some(pane) = self.layout.pane_at(c, s)
        {
            self.active = Some(pane.hwnd);
        }
    }

    /// Give the keyboard back to the active pane when the tray regains activation.
    pub fn on_activate(&mut self, active: bool) {
        if !active {
            return;
        }
        if self.active.is_none() {
            self.active = self.layout.panes().next().map(|p| p.hwnd);
        }
        if let Some(hwnd) = self.active {
            wm::focus_child(hwnd);
        }
    }

    /// The tray itself has no keyboard UI, so it never keeps focus: hand it straight
    /// on to the active pane. Without this, clicking our own chrome (a header, a
    /// splitter, the empty background) would leave the keyboard pointing at nothing.
    pub fn on_set_focus(&mut self) {
        if let Some(hwnd) = self.active {
            wm::focus_child(hwnd);
        }
    }

    /// Returns true when we set a resize cursor and the default handler must not run.
    pub fn on_set_cursor(&self) -> bool {
        let id = match self.hot {
            Hit::VSplit(_) => IDC_SIZEWE,
            Hit::HSplit(..) => IDC_SIZENS,
            _ => return false,
        };
        unsafe {
            if let Ok(cursor) = LoadCursorW(None, id) {
                SetCursor(Some(cursor));
                return true;
            }
        }
        false
    }

    // ---- window plumbing -------------------------------------------------

    pub fn on_size(&mut self) {
        self.relayout();
    }

    /// The user let go of the tray's own resize grip / finished a Snap.
    pub fn on_size_end(&mut self) {
        self.refresh_children();
    }

    pub fn on_dpi_changed(&mut self, dpi: u32, suggested: &RECT) {
        self.dpi = dpi.max(96);
        self.m = Metrics::for_dpi(self.dpi);
        self.theme.destroy();
        self.theme = Theme::new(&self.m);
        self.buf.destroy();
        self.build_buttons();
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.relayout();
    }

    pub fn on_paint(&mut self) {
        let active = self.active.and_then(|h| self.layout.find(h));
        let drop_target = match &self.drag {
            Some(Drag::Move {
                target, armed: true, ..
            }) => *target,
            _ => None,
        };
        let ctx = PaintCtx {
            layout: &self.layout,
            geom: &self.geom,
            metrics: &self.m,
            theme: &self.theme,
            buttons: &self.buttons,
            hot: self.hot,
            hot_button: self.hot_button,
            pressed_button: self.pressed_button,
            active,
            drop_target,
        };
        crate::chrome::paint(self.hwnd, &mut self.buf, &ctx);
    }

    pub fn on_min_max(&self, mmi: &mut MINMAXINFO) {
        let s = |dip: i32| (dip * self.dpi as i32 + 48) / 96;
        mmi.ptMinTrackSize.x = s(480);
        mmi.ptMinTrackSize.y = s(320);
    }

    pub fn destroy(&mut self) {
        self.release_all();
        self.theme.destroy();
        self.buf.destroy();
    }
}

pub fn point_of(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xFFFF) as u16 as i16 as i32,
        y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
    }
}

pub fn loword(wparam: WPARAM) -> u32 {
    (wparam.0 & 0xFFFF) as u32
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    s.chars().take(max_chars - 1).collect::<String>() + "…"
}
