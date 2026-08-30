//! Win32 primitives for adopting a foreign top-level window as a child of the tray,
//! and for putting it back exactly the way we found it.
//!
//! Everything `unsafe` that touches foreign windows lives here. Higher layers
//! (`app`, `layout`, `chrome`) stay safe Rust.

use std::ffi::c_void;
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::BOOL;

/// Styles a top-level window carries that make no sense once it is a child of the tray.
/// Stripped on embed, restored verbatim on release.
const STRIP_STYLE: u32 = WS_POPUP.0
    | WS_CAPTION.0
    | WS_THICKFRAME.0
    | WS_MINIMIZEBOX.0
    | WS_MAXIMIZEBOX.0
    | WS_SYSMENU.0
    | WS_DLGFRAME.0
    | WS_BORDER.0;

const STRIP_EXSTYLE: u32 = WS_EX_APPWINDOW.0
    | WS_EX_WINDOWEDGE.0
    | WS_EX_CLIENTEDGE.0
    | WS_EX_DLGMODALFRAME.0
    | WS_EX_STATICEDGE.0;

pub fn to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn is_valid(hwnd: HWND) -> bool {
    hwnd != HWND::default() && unsafe { IsWindow(Some(hwnd)).as_bool() }
}

/// Everything needed to undo [`embed`]. Kept per pane for the lifetime of the adoption.
#[derive(Clone, Copy)]
pub struct Snapshot {
    pub style: u32,
    pub exstyle: u32,
    pub rect: RECT,
    pub was_maximized: bool,
}

/// Mirror of what is currently adopted, kept outside [`crate::app::App`] so a panic
/// can hand the windows back before the process dies.
///
/// This does not save us from a hard kill: Windows destroys child windows along with
/// their parent, so Task Manager on SnapFrame takes the adopted apps with it. That is
/// inherent to `SetParent` and is the reason the tray is worth exiting cleanly.
static ADOPTED: Mutex<Vec<(isize, Snapshot)>> = Mutex::new(Vec::new());

fn remember(hwnd: HWND, snap: Snapshot) {
    if let Ok(mut v) = ADOPTED.lock() {
        v.push((hwnd.0 as isize, snap));
    }
}

fn forget(hwnd: HWND) {
    if let Ok(mut v) = ADOPTED.lock() {
        v.retain(|(h, _)| *h != hwnd.0 as isize);
    }
}

/// Emergency path: give every still-adopted window back. Safe to call twice.
pub fn release_all_adopted() {
    let pending = match ADOPTED.lock() {
        Ok(mut v) => std::mem::take(&mut *v),
        Err(_) => return,
    };
    for (raw, snap) in pending {
        restore(HWND(raw as *mut c_void), &snap);
    }
}

pub fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn window_class(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn is_cloaked(hwnd: HWND) -> bool {
    unsafe {
        let mut cloaked: u32 = 0;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
    }
}

pub fn thread_of(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

fn process_of(hwnd: HWND) -> u32 {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    }
}

/// Is this a real, user-facing top-level window the user could plausibly want on the tray?
fn is_candidate(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        // Owned dialogs/popups ride along with their owner; only offer the root.
        if GetAncestor(hwnd, GA_ROOTOWNER) != hwnd {
            return false;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        let exstyle = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if exstyle & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        // UWP keeps invisible "ghost" frames around; DWM knows they are cloaked.
        if is_cloaked(hwnd) {
            return false;
        }
        if process_of(hwnd) == GetCurrentProcessId() {
            return false;
        }
        !matches!(
            window_class(hwnd).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Windows.UI.Core.CoreWindow"
        )
    }
}

struct EnumCtx {
    out: Vec<(HWND, String)>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut EnumCtx) };
    if is_candidate(hwnd) {
        ctx.out.push((hwnd, window_title(hwnd)));
    }
    BOOL(1)
}

/// Top-level windows worth offering in the "add" menu, in z-order (front first).
pub fn enum_candidates() -> Vec<(HWND, String)> {
    let mut ctx = EnumCtx { out: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&raw mut ctx as isize));
    }
    ctx.out
}

/// Reparent `child` under `parent`, stripping the frame it no longer needs.
///
/// Returns the state needed to reverse this. Order matters: the style has to be
/// rewritten *before* `SetParent`, otherwise the child keeps a caption it can no
/// longer draw correctly.
pub fn embed(child: HWND, parent: HWND) -> Option<Snapshot> {
    if !is_valid(child) || !is_valid(parent) {
        return None;
    }

    unsafe {
        let was_maximized = IsZoomed(child).as_bool();
        if was_maximized || IsIconic(child).as_bool() {
            let _ = ShowWindow(child, SW_RESTORE);
        }

        let mut rect = RECT::default();
        let _ = GetWindowRect(child, &mut rect);

        let style = GetWindowLongW(child, GWL_STYLE) as u32;
        let exstyle = GetWindowLongW(child, GWL_EXSTYLE) as u32;
        let snap = Snapshot {
            style,
            exstyle,
            rect,
            was_maximized,
        };

        SetWindowLongW(
            child,
            GWL_STYLE,
            ((style & !STRIP_STYLE) | WS_CHILD.0) as i32,
        );
        SetWindowLongW(child, GWL_EXSTYLE, (exstyle & !STRIP_EXSTYLE) as i32);

        if SetParent(child, Some(parent)).is_err() {
            // Put the styles back; the window never moved.
            SetWindowLongW(child, GWL_STYLE, style as i32);
            SetWindowLongW(child, GWL_EXSTYLE, exstyle as i32);
            return None;
        }

        let _ = SetWindowPos(
            child,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );

        // Share an input queue so keystrokes reach the adopted window.
        let _ = AttachThreadInput(GetCurrentThreadId(), thread_of(child), true);

        remember(child, snap);
        Some(snap)
    }
}

/// Undo [`embed`]: hand the window back to the desktop with its original frame and rect.
pub fn release(child: HWND, snap: &Snapshot) {
    forget(child);
    restore(child, snap);
}

fn restore(child: HWND, snap: &Snapshot) {
    if !is_valid(child) {
        return;
    }
    unsafe {
        let _ = AttachThreadInput(GetCurrentThreadId(), thread_of(child), false);

        let _ = SetParent(child, None);
        SetWindowLongW(child, GWL_STYLE, snap.style as i32);
        SetWindowLongW(child, GWL_EXSTYLE, snap.exstyle as i32);

        let _ = SetWindowPos(
            child,
            None,
            snap.rect.left,
            snap.rect.top,
            snap.rect.right - snap.rect.left,
            snap.rect.bottom - snap.rect.top,
            SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );

        if snap.was_maximized {
            let _ = ShowWindow(child, SW_MAXIMIZE);
        }
    }
}

/// Give keyboard focus to an adopted window. Only works because [`embed`] attached
/// our input queue to its thread.
pub fn focus_child(child: HWND) {
    if is_valid(child) {
        unsafe {
            let _ = SetFocus(Some(child));
        }
    }
}
