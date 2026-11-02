//! Tray state and input handling: what is on the tray, where it sits, and what the
//! mouse is doing to it.
//!
//! Owned by the main window through `GWLP_USERDATA`; `main::wndproc` does nothing but
//! decode messages and call in here.

use crate::chrome::{BackBuffer, Button, CaptionButton, CaptionSlot, Command, PaintCtx, Theme};
use crate::layout::{Arrange, Geom, Hit, Layout, Metrics, Pane};
use crate::window_manager as wm;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, InvalidateRect, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
    RedrawWindow, ScreenToClient,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
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

/// Toolbar contents for an arrangement. "均等に整列" resets tiling weights, so it is
/// only offered while tiling — a button that does nothing reads as a bug.
fn toolbar_items(arrange: Arrange) -> Vec<(Command, &'static str)> {
    let mut items = vec![
        (Command::Add, "＋ ウィンドウを追加"),
        (Command::Arrange, arrange_label(arrange)),
    ];
    if arrange == Arrange::Tile {
        items.push((Command::Rebalance, "均等に整列"));
    }
    items.push((Command::ReleaseAll, "すべて解放"));
    items
}

fn arrange_label(arrange: Arrange) -> &'static str {
    match arrange {
        Arrange::Free => "配置: 元のサイズ ▾",
        Arrange::Tile => "配置: タイル ▾",
    }
}

/// Entries of the arrangement menu, in menu order. Ids start at 1 because 0 is what
/// `TrackPopupMenu` returns when the menu is dismissed.
const ARRANGE_CHOICES: [(Arrange, &str); 2] = [
    (Arrange::Free, "元のサイズのまま"),
    (Arrange::Tile, "タイル"),
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
    /// Header drag of a free pane. Tracked by handle so a pane dying mid-drag cannot
    /// redirect it to a neighbour; `grab` is the pointer's offset from the pane's corner.
    Float { hwnd: HWND, grab: POINT },
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
    caption: Vec<CaptionSlot>,
    hot_caption: Option<CaptionButton>,
    pressed_caption: Option<CaptionButton>,
    /// Thickness of the top resize grip. The system frame no longer covers the top
    /// edge — it is our client area now — so we synthesise `HTTOP` over this band.
    resize_handle: i32,
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
    tracking_nc_leave: bool,
}

/// Thickness of the window's resize border at `dpi`, which is also how tall a top
/// grip has to be to feel the same as the system's.
fn resize_handle_for(dpi: u32) -> i32 {
    unsafe {
        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
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
            caption: Vec::new(),
            hot_caption: None,
            pressed_caption: None,
            resize_handle: resize_handle_for(dpi),
            hot: Hit::None,
            hot_button: None,
            pressed_button: None,
            active: None,
            drag: None,
            tracking_leave: false,
            tracking_nc_leave: false,
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
        for (cmd, label) in toolbar_items(self.layout.arrange) {
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

    /// Caption buttons hug the right edge, so unlike the toolbar commands they have
    /// to be re-laid-out whenever the window's width changes.
    fn build_caption_buttons(&mut self) {
        self.caption.clear();
        let right = self.client().right;
        let mut x = right;
        for kind in [
            CaptionButton::Close,
            CaptionButton::Maximize,
            CaptionButton::Minimize,
        ] {
            x -= self.m.caption_w;
            self.caption.push(CaptionSlot {
                rect: RECT {
                    left: x,
                    top: 0,
                    right: x + self.m.caption_w,
                    bottom: self.m.toolbar,
                },
                kind,
            });
        }
    }

    fn caption_button_at(&self, pt: POINT) -> Option<CaptionButton> {
        self.caption
            .iter()
            .find(|s| crate::layout::contains(&s.rect, pt))
            .map(|s| s.kind)
    }

    fn is_maximized(&self) -> bool {
        unsafe { IsZoomed(self.hwnd).as_bool() }
    }

    /// Recompute pixel geometry and push the adopted windows to their new rects.
    pub fn relayout(&mut self) {
        self.build_caption_buttons();
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
        let free = self.initial_free_rect(&snap);
        self.layout.add(Pane {
            hwnd: target,
            title,
            snap,
            free,
            z: 0,
        });
        self.relayout();
        self.refresh_children();
        self.activate(target);
    }

    /// Make `hwnd` the active pane: frontmost among its siblings and holding the
    /// keyboard. Every path that changes the focused pane goes through here, so the
    /// two never drift apart — a pane that has the keyboard but sits underneath an
    /// overlapping neighbour is indistinguishable from a broken tray.
    fn activate(&mut self, hwnd: HWND) {
        self.active = Some(hwnd);
        wm::raise_child(hwnd);
        wm::focus_child(hwnd);
        // Headers are ours to paint, so they follow the window to the front.
        if self.layout.raise(hwnd) {
            self.geom = self.layout.geometry(self.content(), &self.m);
            self.invalidate();
        }
    }

    /// Where a pane starts out in the free arrangement, at the window's own size.
    ///
    /// If the window already overlaps the tray it stays exactly where it is on screen,
    /// header added above it, so adopting it looks like nothing moved. Otherwise it is
    /// cascaded from the top-left corner. Either way the geometry pass slides it fully
    /// inside the tray.
    fn initial_free_rect(&self, snap: &wm::Snapshot) -> RECT {
        let (w, h) = (
            snap.rect.right - snap.rect.left,
            snap.rect.bottom - snap.rect.top,
        );
        let content = self.content();
        let mut origin = POINT {
            x: snap.rect.left,
            y: snap.rect.top,
        };
        let mapped = unsafe { ScreenToClient(self.hwnd, &mut origin).as_bool() };
        let at = RECT {
            left: origin.x,
            top: origin.y - self.m.header,
            right: origin.x + w,
            bottom: origin.y + h,
        };
        if mapped && crate::layout::intersects(&at, &content) {
            return at;
        }
        let step = self.m.header * (self.layout.pane_count() % 8) as i32;
        let left = content.left + self.m.pad + step;
        let top = content.top + self.m.pad + step;
        RECT {
            left,
            top,
            right: left + w,
            bottom: top + self.m.header + h,
        }
    }

    fn set_arrange(&mut self, arrange: Arrange) {
        if self.layout.arrange == arrange {
            return;
        }
        self.layout.arrange = arrange;
        self.drag = None;
        self.hot = Hit::None;
        self.hot_button = None;
        self.build_buttons();
        self.relayout();
        self.refresh_children();
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
        self.active = None;
        let first = self.layout.panes().next().map(|p| p.hwnd);
        if let Some(hwnd) = first {
            self.activate(hwnd);
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
            Command::Arrange => self.show_arrange_menu(),
            Command::Rebalance => {
                self.layout.rebalance();
                self.relayout();
            }
            Command::ReleaseAll => self.release_all(),
        }
    }

    /// Screen point just below a toolbar button, where its popup menu opens.
    fn menu_anchor(&self, cmd: Command) -> POINT {
        let mut pt = self
            .buttons
            .iter()
            .find(|b| b.cmd == cmd)
            .map(|b| POINT {
                x: b.rect.left,
                y: b.rect.bottom,
            })
            .unwrap_or_default();
        unsafe {
            let _ = ClientToScreen(self.hwnd, &mut pt);
        }
        pt
    }

    fn show_arrange_menu(&mut self) {
        let pt = self.menu_anchor(Command::Arrange);
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            for (i, (arrange, label)) in ARRANGE_CHOICES.iter().enumerate() {
                let checked = if *arrange == self.layout.arrange {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
                let text = wm::to_utf16(label);
                let _ = AppendMenuW(menu, MF_STRING | checked, i + 1, PCWSTR(text.as_ptr()));
            }
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
            if let Some((arrange, _)) = idx.checked_sub(1).and_then(|i| ARRANGE_CHOICES.get(i)) {
                self.set_arrange(*arrange);
            }
        }
    }

    fn show_add_menu(&mut self) {
        let candidates: Vec<(HWND, String)> = wm::enum_candidates()
            .into_iter()
            .filter(|(h, _)| *h != self.hwnd && !self.layout.contains(*h))
            .collect();

        let pt = self.menu_anchor(Command::Add);

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
        // Moving into the client area means we left the caption buttons; no
        // WM_NCMOUSELEAVE arrives for that transition.
        self.clear_caption_hover();
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
            Some(Drag::Float { hwnd, grab }) => {
                self.layout
                    .move_free(hwnd, pt.x - grab.x, pt.y - grab.y, self.content());
                self.drag = Some(Drag::Float { hwnd, grab });
                self.relayout();
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
                // Free panes are moved by their header; tiled ones are swapped by it.
                self.drag = match self.layout.arrange {
                    Arrange::Free => {
                        let (Some(pane), Some(g)) = (
                            self.layout.pane_at(c, s),
                            self.geom.slots.get(c).and_then(|col| col.get(s)),
                        ) else {
                            return;
                        };
                        Some(Drag::Float {
                            hwnd: pane.hwnd,
                            grab: POINT {
                                x: pt.x - g.full.left,
                                y: pt.y - g.full.top,
                            },
                        })
                    }
                    Arrange::Tile => Some(Drag::Move {
                        from: (c, s),
                        origin: pt,
                        armed: false,
                        target: None,
                    }),
                };
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
            Some(Drag::VSplit(_) | Drag::HSplit(..) | Drag::Float { .. }) => {
                self.invalidate();
                self.refresh_children();
            }
            _ => self.invalidate(),
        }
    }

    fn focus_pane(&mut self, col: usize, slot: usize) {
        let Some(hwnd) = self.layout.pane_at(col, slot).map(|p| p.hwnd) else {
            return;
        };
        let changed = self.active != Some(hwnd);
        self.activate(hwnd);
        if changed {
            self.invalidate();
        }
    }

    fn to_client(&self, screen: POINT) -> Option<POINT> {
        let mut pt = screen;
        unsafe {
            if !ScreenToClient(self.hwnd, &mut pt).as_bool() {
                return None;
            }
        }
        Some(pt)
    }

    fn cursor_client(&self) -> Option<POINT> {
        let mut pt = POINT::default();
        unsafe { GetCursorPos(&mut pt).ok()? };
        self.to_client(pt)
    }

    /// Which pane a client-area point belongs to.
    ///
    /// The adopted window actually under the point wins, because panes can overlap
    /// once an app sizes itself past its slot — and a click there went to that window,
    /// not to whoever owns the pixel in our tiling. `ChildWindowFromPointEx` reports
    /// the immediate child, so a click on a deeply nested control still resolves to
    /// its pane. Our own chrome (headers, splitters, background) has no child window
    /// there, which is what the geometry fallback is for.
    fn pane_at_point(&self, pt: POINT) -> Option<(usize, usize)> {
        let child = unsafe { ChildWindowFromPointEx(self.hwnd, pt, CWP_SKIPINVISIBLE) };
        if child != self.hwnd
            && child != HWND::default()
            && let Some(idx) = self.layout.find(child)
        {
            return Some(idx);
        }
        match self.geom.hit_test(pt) {
            Hit::Header(c, s) | Hit::Close(c, s) | Hit::Client(c, s) => Some((c, s)),
            _ => None,
        }
    }

    /// Focus whichever pane the pointer is over.
    fn focus_pane_at_cursor(&mut self) {
        let Some(pt) = self.cursor_client() else {
            return;
        };
        if let Some((c, s)) = self.pane_at_point(pt) {
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
        if let Some((c, s)) = self.pane_at_point(pt)
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
        let pane = self.active.or_else(|| self.layout.panes().next().map(|p| p.hwnd));
        if let Some(hwnd) = pane {
            self.activate(hwnd);
        }
    }

    /// The tray itself has no keyboard UI, so it never keeps focus: hand it straight
    /// on to the active pane. Without this, clicking our own chrome (a header, a
    /// splitter, the empty background) would leave the keyboard pointing at nothing.
    pub fn on_set_focus(&mut self) {
        if let Some(hwnd) = self.active {
            self.activate(hwnd);
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

    // ---- custom frame ----------------------------------------------------

    /// Classify a point now that the caption strip is part of our client area.
    ///
    /// `def` is what `DefWindowProcW` said. It still gets the left, right and bottom
    /// borders right, because those are the only non-client edges left; everything
    /// along the top — the resize grip, the drag region, the window controls — has to
    /// be reconstructed here.
    pub fn on_nc_hit_test(&self, screen: POINT, def: LRESULT) -> LRESULT {
        let Some(pt) = self.to_client(screen) else {
            return def;
        };
        let ht = def.0 as u32;
        // A maximized window cannot be resized, so no grip along its top edge.
        let in_grip = !self.is_maximized() && pt.y >= 0 && pt.y < self.resize_handle;

        let code = match ht {
            HTCLIENT if in_grip => HTTOP,
            HTLEFT if in_grip => HTTOPLEFT,
            HTRIGHT if in_grip => HTTOPRIGHT,
            HTCLIENT => match self.caption_button_at(pt) {
                Some(CaptionButton::Minimize) => HTMINBUTTON,
                // Windows 11 shows its Snap Layouts flyout when the pointer rests on
                // something reported as HTMAXBUTTON — worth having in a tray app.
                Some(CaptionButton::Maximize) => HTMAXBUTTON,
                Some(CaptionButton::Close) => HTCLOSE,
                None if pt.y < self.m.toolbar && self.hit(pt).1.is_none() => HTCAPTION,
                None => return def,
            },
            _ => return def,
        };
        LRESULT(code as isize)
    }

    pub fn on_nc_mouse_move(&mut self, screen: POINT) {
        if !self.tracking_nc_leave {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE | TME_NONCLIENT,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            self.tracking_nc_leave = true;
        }
        let hot = self.to_client(screen).and_then(|pt| self.caption_button_at(pt));
        if hot != self.hot_caption {
            self.hot_caption = hot;
            self.invalidate();
        }
    }

    pub fn on_nc_mouse_leave(&mut self) {
        self.tracking_nc_leave = false;
        self.clear_caption_hover();
    }

    fn clear_caption_hover(&mut self) {
        if self.hot_caption.is_some() {
            self.hot_caption = None;
            self.invalidate();
        }
    }

    /// Returns true when we consumed the click and `DefWindowProcW` must not run.
    pub fn on_nc_lbutton_down(&mut self, ht: u32) -> bool {
        let kind = match ht {
            HTMINBUTTON => CaptionButton::Minimize,
            HTMAXBUTTON => CaptionButton::Maximize,
            HTCLOSE => CaptionButton::Close,
            _ => return false,
        };
        self.pressed_caption = Some(kind);
        self.invalidate();
        true
    }

    pub fn on_nc_lbutton_up(&mut self, ht: u32) -> bool {
        let Some(pressed) = self.pressed_caption.take() else {
            return false;
        };
        self.invalidate();
        let released_on = match ht {
            HTMINBUTTON => Some(CaptionButton::Minimize),
            HTMAXBUTTON => Some(CaptionButton::Maximize),
            HTCLOSE => Some(CaptionButton::Close),
            _ => None,
        };
        if released_on != Some(pressed) {
            return true;
        }
        unsafe {
            match pressed {
                CaptionButton::Minimize => {
                    let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
                }
                CaptionButton::Maximize => {
                    let cmd = if self.is_maximized() {
                        SW_RESTORE
                    } else {
                        SW_MAXIMIZE
                    };
                    let _ = ShowWindow(self.hwnd, cmd);
                }
                CaptionButton::Close => {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        true
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
        self.resize_handle = resize_handle_for(self.dpi);
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
            caption: &self.caption,
            hot_caption: self.hot_caption,
            pressed_caption: self.pressed_caption,
            maximized: self.is_maximized(),
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
