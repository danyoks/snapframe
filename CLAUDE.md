# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo run
```

```bash
cargo clippy --all-targets
```

Windows-only (Win32, `x86_64-pc-windows-msvc`). A plain PowerShell works — rustc locates the MSVC toolchain itself, no VS Developer Prompt needed. There is no test suite; `cargo check` is the fast loop.

`cargo build` fails with `os error 5` while a SnapFrame instance is running. Close it first — and close it *cleanly* (title-bar ×, or `WM_CLOSE`), never `Stop-Process`, for the reason in Gotchas.

## What this is

A tray ("お盆") window that adopts other apps' top-level windows as children so Windows Snap moves and sizes them as one unit. See [README.md](README.md) for the user-facing behaviour.

## Architecture

Four modules, layered. `unsafe` is concentrated at the bottom; the middle layers are safe Rust.

- [src/main.rs](src/main.rs) — Win32 entry point and `wndproc`. Decodes messages and calls into `App`; holds no state of its own.
- [src/app.rs](src/app.rs) — `App`: what is on the tray, hit-testing, drag state machine, commands. Owned by the main window via `GWLP_USERDATA` (boxed in `WM_CREATE`, dropped in `WM_NCDESTROY`).
- [src/layout.rs](src/layout.rs) — the tiling model and its pixel geometry. No Win32 calls beyond plain structs.
- [src/chrome.rs](src/chrome.rs) — GDI painting of the toolbar, pane headers and splitters.
- [src/window_manager.rs](src/window_manager.rs) — every `unsafe` call that touches a foreign window: enumeration, `SetParent`, style save/restore, focus.

### The tiling model

Deliberately two levels: the tray is a **row of columns**, each column a **stack of slots**. A general split tree was not worth the hit-testing and drag complexity for what a tray does.

Sizes are **weights, never pixels**. `Layout::geometry` recomputes rects from weights on every resize, so a Snap is a pure recomputation with no accumulated drift. `distribute()` derives each position from a running cumulative fraction rather than summing rounded sizes, which is why the last item always lands exactly on the far edge — don't "simplify" it into a sum of rounded widths.

`App::geom` caches the computed rects. Everything (painting, hit-testing, child placement) reads that cache; `App::relayout()` is the single place that refills it and pushes children to their new rects.

### Adoption lifecycle

`window_manager::embed` strips the frame styles, `SetParent`s, and returns a `Snapshot`. `release` puts the snapshot back. Order matters in both directions: styles are rewritten **before** `SetParent` on the way in, because reparenting a window that still has `WS_CAPTION` leaves it drawing a caption it can no longer own.

`embed` also `AttachThreadInput`s our queue to the target's thread — that is the only reason `SetFocus` on an adopted window works at all, since it lives in another process. `release` detaches.

Every adoption is additionally recorded in the `ADOPTED` static so the panic hook can hand windows back before the process dies. `App` and that registry must stay in sync: go through `window_manager::{embed, release}`, never call `SetParent` directly.

## Gotchas

- **`match` on raw `u32` message constants is a trap.** An unimported constant is a valid catch-all binding, not a compile error, so it silently swallows every arm below it. This already happened once: `WM_MOUSELEAVE` lives in `Win32::UI::Controls`, not `WindowsAndMessaging`, and its bare name in `wndproc` killed ten handlers with only a `unreachable pattern` warning. Check warnings on any new message arm.
- Other modules that are not where you would guess: `AttachThreadInput` is in `Win32::System::Threading`; `ClientToScreen` is in `Graphics::Gdi`.
- **Killing the tray kills the adopted apps.** Windows destroys child windows with their parent, so `Stop-Process` on SnapFrame closes whatever was on it. The `WM_DESTROY` path and the panic hook both release first; keep it that way, and never add an exit path that skips them.
- **Toolbar commands fire on `WM_LBUTTONUP`, not down.** A `TrackPopupMenu` opened during `WM_LBUTTONDOWN` is dismissed by the `WM_LBUTTONUP` that immediately follows it. `App::pressed_button` exists solely for this.
- `#![windows_subsystem = "windows"]` means **no console**: `println!`/`dbg!` go nowhere. Panics surface through the `MessageBoxW` hook in `install_panic_dialog`; for tracing, use `OutputDebugStringW` or temporarily drop the attribute.
- **Store apps (UWP) and elevated windows cannot be reparented.** `embed` returns `None` and `App::warn_embed_failed` explains it to the user — a silent no-op here reads as a bug.
- `refresh_children()` (a `RedrawWindow` sweep) exists because some apps leave stale pixels after being resized. Call it when a resize *finishes* — never per mouse-move, which would make splitter drags crawl.
- Liveness is a 400 ms `WM_TIMER` poll, not an `EVENT_OBJECT_DESTROY` hook. That is intentional: a system-wide destroy hook wakes the process for every menu and tooltip on the desktop, which costs far more than a handful of `IsWindow` calls.
- DPI is per-monitor v2 **plus** `SetThreadDpiHostingBehavior(DPI_HOSTING_BEHAVIOR_MIXED)`. Without mixed hosting, adopting a window whose process has different DPI awareness misbehaves.
- `windows` crate features are minimal and explicit in [Cargo.toml](Cargo.toml); a new Win32 API usually needs its feature added first.

## Testing against the real desktop

Driving the GUI from PowerShell works, with two quirks worth knowing before concluding something is broken:

- `FindWindow`/`FindWindowEx` return 0 in this environment. Locate windows with `EnumWindows` instead.
- The **first** synthetic click after a script starts is consumed by window activation. Send a harmless warm-up click before the one you are actually testing.
