//! GDI chrome: the toolbar, the per-pane header strips and the splitter grooves.
//!
//! Every GDI object is created once (per DPI) and reused; painting goes through a
//! back buffer that is only reallocated when the window actually changes size.

use crate::layout::{Geom, Hit, Layout, Metrics};
use crate::window_manager::to_utf16;
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

const CLR_BG: COLORREF = COLORREF(0x00201F1E);
const CLR_TOOLBAR: COLORREF = COLORREF(0x002D2C2B);
const CLR_HEADER: COLORREF = COLORREF(0x003C3B3A);
const CLR_HEADER_HOT: COLORREF = COLORREF(0x004A4948);
const CLR_HEADER_ACTIVE: COLORREF = COLORREF(0x00A05A11);
const CLR_SPLIT: COLORREF = COLORREF(0x00201F1E);
const CLR_SPLIT_HOT: COLORREF = COLORREF(0x00A05A11);
const CLR_BTN_HOT: COLORREF = COLORREF(0x00504F4E);
const CLR_CLOSE_HOT: COLORREF = COLORREF(0x002020C0);
const CLR_TEXT: COLORREF = COLORREF(0x00E8E8E8);
const CLR_TEXT_DIM: COLORREF = COLORREF(0x00909090);

/// Tone down the parts of the frame we do *not* draw.
///
/// The caption itself is gone — `WM_NCCALCSIZE` hands that strip to us — so all that
/// is left to the system is the one-pixel border and the shadow behind it. Painting
/// the border in the tray's own background stops a bright outline from boxing in an
/// otherwise dark window.
///
/// Windows 11 build 22000+ only; older systems fail both calls harmlessly and get the
/// default border.
pub fn quiet_frame(hwnd: HWND) {
    unsafe {
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark).cast(),
            size_of::<i32>() as u32,
        );
        let border = CLR_BG;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&raw const border).cast(),
            size_of::<COLORREF>() as u32,
        );
    }
}

/// Toolbar commands. Values double as popup-menu ids, so they must stay non-zero
/// and must not collide with the window ids handed out in the add menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Add,
    Rebalance,
    ReleaseAll,
}

pub struct Button {
    pub rect: RECT,
    pub label: &'static str,
    pub cmd: Command,
}

/// The window controls we now draw ourselves, since there is no system caption left
/// to draw them for us.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaptionButton {
    Minimize,
    Maximize,
    Close,
}

pub struct CaptionSlot {
    pub rect: RECT,
    pub kind: CaptionButton,
}

pub struct Theme {
    pub font: HFONT,
    bg: HBRUSH,
    toolbar: HBRUSH,
    header: HBRUSH,
    header_hot: HBRUSH,
    header_active: HBRUSH,
    split: HBRUSH,
    split_hot: HBRUSH,
    btn_hot: HBRUSH,
    close_hot: HBRUSH,
    close_pen: HPEN,
    accent_pen: HPEN,
    /// Scratch DC kept alive purely for text measurement.
    measure_dc: HDC,
}

impl Theme {
    pub fn new(m: &Metrics) -> Self {
        unsafe {
            let mut lf = LOGFONTW {
                lfHeight: -m.font_px,
                lfWeight: FW_NORMAL.0 as i32,
                lfCharSet: DEFAULT_CHARSET,
                lfOutPrecision: OUT_TT_PRECIS,
                lfClipPrecision: CLIP_DEFAULT_PRECIS,
                lfQuality: CLEARTYPE_QUALITY,
                lfPitchAndFamily: DEFAULT_PITCH.0 | FF_DONTCARE.0,
                ..Default::default()
            };
            let face = to_utf16("Yu Gothic UI");
            let n = face.len().min(lf.lfFaceName.len());
            lf.lfFaceName[..n].copy_from_slice(&face[..n]);

            let font = CreateFontIndirectW(&lf);
            let measure_dc = CreateCompatibleDC(None);
            SelectObject(measure_dc, font.into());

            Self {
                font,
                bg: CreateSolidBrush(CLR_BG),
                toolbar: CreateSolidBrush(CLR_TOOLBAR),
                header: CreateSolidBrush(CLR_HEADER),
                header_hot: CreateSolidBrush(CLR_HEADER_HOT),
                header_active: CreateSolidBrush(CLR_HEADER_ACTIVE),
                split: CreateSolidBrush(CLR_SPLIT),
                split_hot: CreateSolidBrush(CLR_SPLIT_HOT),
                btn_hot: CreateSolidBrush(CLR_BTN_HOT),
                close_hot: CreateSolidBrush(CLR_CLOSE_HOT),
                close_pen: CreatePen(PS_SOLID, 0, CLR_TEXT),
                accent_pen: CreatePen(PS_SOLID, 0, CLR_HEADER_ACTIVE),
                measure_dc,
            }
        }
    }

    pub fn text_width(&self, s: &str) -> i32 {
        unsafe {
            let wide: Vec<u16> = s.encode_utf16().collect();
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(self.measure_dc, &wide, &mut size);
            size.cx
        }
    }

    pub fn destroy(&self) {
        unsafe {
            let _ = DeleteDC(self.measure_dc);
            for obj in [
                self.font.into(),
                self.bg.into(),
                self.toolbar.into(),
                self.header.into(),
                self.header_hot.into(),
                self.header_active.into(),
                self.split.into(),
                self.split_hot.into(),
                self.btn_hot.into(),
                self.close_hot.into(),
                self.close_pen.into(),
                self.accent_pen.into(),
            ] {
                let _ = DeleteObject(obj);
            }
        }
    }
}

/// Off-screen surface reused across paints; only grows when the window does.
#[derive(Default)]
pub struct BackBuffer {
    dc: HDC,
    bmp: HBITMAP,
    prev: HGDIOBJ,
    w: i32,
    h: i32,
}

impl BackBuffer {
    fn ensure(&mut self, target: HDC, w: i32, h: i32) -> HDC {
        if self.dc.is_invalid() {
            unsafe { self.dc = CreateCompatibleDC(Some(target)) };
        }
        if w > self.w || h > self.h {
            let (nw, nh) = (w.max(self.w), h.max(self.h));
            unsafe {
                let bmp = CreateCompatibleBitmap(target, nw, nh);
                let prev = SelectObject(self.dc, bmp.into());
                if !self.bmp.is_invalid() {
                    let _ = DeleteObject(self.bmp.into());
                } else {
                    self.prev = prev;
                }
                self.bmp = bmp;
            }
            self.w = nw;
            self.h = nh;
        }
        self.dc
    }

    pub fn destroy(&mut self) {
        unsafe {
            if !self.dc.is_invalid() {
                if !self.prev.is_invalid() {
                    SelectObject(self.dc, self.prev);
                }
                let _ = DeleteDC(self.dc);
            }
            if !self.bmp.is_invalid() {
                let _ = DeleteObject(self.bmp.into());
            }
        }
        *self = Self::default();
    }
}

pub struct PaintCtx<'a> {
    pub layout: &'a Layout,
    pub geom: &'a Geom,
    pub metrics: &'a Metrics,
    pub theme: &'a Theme,
    pub buttons: &'a [Button],
    pub hot: Hit,
    pub hot_button: Option<Command>,
    pub pressed_button: Option<Command>,
    pub active: Option<(usize, usize)>,
    pub drop_target: Option<(usize, usize)>,
    pub caption: &'a [CaptionSlot],
    pub hot_caption: Option<CaptionButton>,
    pub pressed_caption: Option<CaptionButton>,
    pub maximized: bool,
}

pub fn paint(hwnd: HWND, buf: &mut BackBuffer, ctx: &PaintCtx) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let (w, h) = (client.right, client.bottom);

        let dc = buf.ensure(hdc, w, h);
        SelectObject(dc, ctx.theme.font.into());
        SetBkMode(dc, TRANSPARENT);

        draw_all(dc, client, ctx);

        let _ = BitBlt(hdc, 0, 0, w, h, Some(dc), 0, 0, SRCCOPY);
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe fn draw_all(dc: HDC, client: RECT, ctx: &PaintCtx) {
    let m = ctx.metrics;
    let t = ctx.theme;
    unsafe {
        FillRect(dc, &client, t.bg);

        let toolbar = RECT {
            bottom: m.toolbar,
            ..client
        };
        FillRect(dc, &toolbar, t.toolbar);

        for b in ctx.buttons {
            if ctx.pressed_button == Some(b.cmd) && ctx.hot_button == Some(b.cmd) {
                FillRect(dc, &b.rect, t.header_active);
            } else if ctx.hot_button == Some(b.cmd) {
                FillRect(dc, &b.rect, t.btn_hot);
            }
            draw_label(dc, b.rect, b.label, CLR_TEXT, DT_CENTER);
        }

        draw_caption_buttons(dc, ctx);

        if ctx.layout.is_empty() {
            let hint = RECT {
                top: m.toolbar,
                ..client
            };
            draw_label(
                dc,
                hint,
                "「＋ ウィンドウを追加」から、まとめたいアプリを載せてください",
                CLR_TEXT_DIM,
                DT_CENTER,
            );
            return;
        }

        for split in &ctx.geom.vsplits {
            let hot = matches!(ctx.hot, Hit::VSplit(i) if ctx.geom.vsplits.get(i).is_some_and(|r| r == split));
            FillRect(dc, split, if hot { t.split_hot } else { t.split });
        }
        for (ci, splits) in ctx.geom.hsplits.iter().enumerate() {
            for (si, split) in splits.iter().enumerate() {
                let hot = ctx.hot == Hit::HSplit(ci, si);
                FillRect(dc, split, if hot { t.split_hot } else { t.split });
            }
        }

        for (ci, col) in ctx.geom.slots.iter().enumerate() {
            for (si, g) in col.iter().enumerate() {
                let at = (ci, si);
                let brush = if ctx.active == Some(at) {
                    t.header_active
                } else if matches!(ctx.hot, Hit::Header(c, s) | Hit::Close(c, s) if (c, s) == at) {
                    t.header_hot
                } else {
                    t.header
                };
                FillRect(dc, &g.header, brush);

                if let Some(pane) = ctx.layout.pane_at(ci, si) {
                    let label = RECT {
                        left: g.header.left + m.pad,
                        right: g.close.left - m.pad,
                        ..g.header
                    };
                    if label.right > label.left {
                        draw_label(
                            dc,
                            label,
                            &pane.title,
                            CLR_TEXT,
                            DT_LEFT | DT_END_ELLIPSIS,
                        );
                    }
                }

                let close_hot = ctx.hot == Hit::Close(ci, si);
                if close_hot {
                    FillRect(dc, &g.close, t.close_hot);
                }
                draw_cross(dc, g.close, m, t.close_pen);

                if ctx.drop_target == Some(at) {
                    frame(dc, g.full, t.accent_pen);
                }
            }
        }
    }
}

unsafe fn draw_label(
    dc: HDC,
    rect: RECT,
    text: &str,
    color: COLORREF,
    format: DRAW_TEXT_FORMAT,
) {
    unsafe {
        SetTextColor(dc, color);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut r = rect;
        DrawTextW(
            dc,
            &mut wide,
            &mut r,
            DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | format,
        );
    }
}

unsafe fn draw_caption_buttons(dc: HDC, ctx: &PaintCtx) {
    let t = ctx.theme;
    unsafe {
        for slot in ctx.caption {
            let hot = ctx.hot_caption == Some(slot.kind);
            let pressed = ctx.pressed_caption == Some(slot.kind);
            if hot || pressed {
                // Close goes red on hover, the way every Windows title bar does; the
                // other two just lighten.
                let brush = match (slot.kind, pressed) {
                    (CaptionButton::Close, _) => t.close_hot,
                    (_, true) => t.header_active,
                    _ => t.btn_hot,
                };
                FillRect(dc, &slot.rect, brush);
            }

            let r = slot.rect;
            let cx = (r.left + r.right) / 2;
            let cy = (r.top + r.bottom) / 2;
            // Glyphs are stroked rather than typed so they stay crisp at any DPI and
            // do not depend on the UI font carrying the box-drawing symbols.
            let half = ((r.bottom - r.top) / 6).max(4);
            let prev = SelectObject(dc, t.close_pen.into());
            match slot.kind {
                CaptionButton::Minimize => {
                    let _ = MoveToEx(dc, cx - half, cy, None);
                    let _ = LineTo(dc, cx + half, cy);
                }
                CaptionButton::Maximize if ctx.maximized => {
                    let off = (half / 3).max(2);
                    frame_lines(dc, cx - half, cy - half + off, cx + half - off, cy + half);
                    frame_lines(dc, cx - half + off, cy - half, cx + half, cy + half - off);
                }
                CaptionButton::Maximize => {
                    frame_lines(dc, cx - half, cy - half, cx + half, cy + half);
                }
                CaptionButton::Close => {
                    let _ = MoveToEx(dc, cx - half, cy - half, None);
                    let _ = LineTo(dc, cx + half, cy + half);
                    let _ = MoveToEx(dc, cx + half - 1, cy - half, None);
                    let _ = LineTo(dc, cx - half - 1, cy + half);
                }
            }
            SelectObject(dc, prev);
        }
    }
}

/// Stroke a hollow rectangle with the pen already selected into `dc`.
unsafe fn frame_lines(dc: HDC, left: i32, top: i32, right: i32, bottom: i32) {
    unsafe {
        let _ = MoveToEx(dc, left, top, None);
        let _ = LineTo(dc, right, top);
        let _ = LineTo(dc, right, bottom);
        let _ = LineTo(dc, left, bottom);
        let _ = LineTo(dc, left, top);
    }
}

/// The pane's detach button. Drawn as two strokes rather than a glyph so it stays
/// crisp at every DPI without depending on the font's symbol coverage.
unsafe fn draw_cross(dc: HDC, r: RECT, m: &Metrics, pen: HPEN) {
    unsafe {
        let inset = (m.close / 3).max(3);
        let prev = SelectObject(dc, pen.into());
        let (l, tp, rt, b) = (
            r.left + inset,
            r.top + inset,
            r.right - inset,
            r.bottom - inset,
        );
        let _ = MoveToEx(dc, l, tp, None);
        let _ = LineTo(dc, rt, b);
        let _ = MoveToEx(dc, rt - 1, tp, None);
        let _ = LineTo(dc, l - 1, b);
        SelectObject(dc, prev);
    }
}

unsafe fn frame(dc: HDC, r: RECT, pen: HPEN) {
    unsafe {
        let prev_pen = SelectObject(dc, pen.into());
        let prev_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(dc, r.left, r.top, r.right, r.bottom);
        SelectObject(dc, prev_brush);
        SelectObject(dc, prev_pen);
    }
}
