#![windows_subsystem = "windows"]

//! SnapFrame — a tray ("お盆") window that adopts several small app windows so
//! Windows Snap can move and size them as a single unit.
//!
//! This file is only the Win32 entry point and message pump; all behaviour lives in
//! [`app::App`], which hangs off the main window's `GWLP_USERDATA`.

mod app;
mod chrome;
mod layout;
mod window_manager;

use app::{App, TIMER_ID, TIMER_MS, loword, point_of};
use window_manager::to_utf16;

use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_HOSTING_BEHAVIOR_MIXED,
    SetProcessDpiAwarenessContext, SetThreadDpiHostingBehavior,
};
// WM_MOUSELEAVE lives in Controls, not WindowsAndMessaging. Without this import the
// bare name in `wndproc`'s match becomes a catch-all binding and silently swallows
// every later arm.
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const CLASS_NAME: &str = "SnapFrameTray";
const WINDOW_TITLE: &str = "SnapFrame";

fn main() -> Result<()> {
    install_panic_dialog();

    unsafe {
        // Per-monitor v2 keeps our own chrome sharp; mixed hosting lets us adopt
        // windows whose process has a different DPI awareness than ours without
        // Windows refusing the reparent or scaling them into a blur.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetThreadDpiHostingBehavior(DPI_HOSTING_BEHAVIOR_MIXED);

        let instance = GetModuleHandleW(None)?;
        let class = to_utf16(CLASS_NAME);

        let wc = WNDCLASSW {
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            // No background brush and no CS_HREDRAW/CS_VREDRAW: WM_PAINT draws every
            // pixel from a back buffer, so letting Windows erase or force-invalidate
            // would only add flicker while the tray is being resized.
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(Error::from_thread());
        }

        let title = to_utf16(WINDOW_TITLE);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            // WS_CLIPCHILDREN keeps our painting out of the adopted windows' rects.
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_CREATE {
            let app = Box::new(App::new(hwnd));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
            SetTimer(Some(hwnd), TIMER_ID, TIMER_MS, None);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if ptr.is_null() {
            // WM_GETMINMAXINFO and friends arrive before WM_CREATE.
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *ptr;

        match msg {
            WM_PAINT => {
                app.on_paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                app.on_size();
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                app.on_size_end();
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                app.on_mouse_move(point_of(lparam));
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                app.on_mouse_leave();
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                app.on_lbutton_down(point_of(lparam));
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                app.on_lbutton_up(point_of(lparam));
                LRESULT(0)
            }
            WM_SETCURSOR => {
                if loword(WPARAM(lparam.0 as usize)) == HTCLIENT && app.on_set_cursor() {
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_TIMER if wparam.0 == TIMER_ID => {
                app.on_tick();
                LRESULT(0)
            }
            WM_ACTIVATE => {
                app.on_activate(loword(wparam) != WA_INACTIVE);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                app.on_min_max(&mut *(lparam.0 as *mut MINMAXINFO));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                app.on_dpi_changed(loword(wparam), &*(lparam.0 as *const RECT));
                LRESULT(0)
            }
            WM_DESTROY => {
                // Adopted windows must go home before we stop existing, or they are
                // left parented to a dead HWND with a stripped frame.
                app.destroy();
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// `windows_subsystem = "windows"` means there is no console for a panic to print to,
/// so surface it instead of dying silently — and, more importantly, give the adopted
/// windows back first. They are our children; if we die holding them, they die too.
fn install_panic_dialog() {
    std::panic::set_hook(Box::new(|info| {
        window_manager::release_all_adopted();
        let text = to_utf16(&format!("SnapFrame panicked:\n\n{info}"));
        let caption = to_utf16("SnapFrame");
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
    }));
}
