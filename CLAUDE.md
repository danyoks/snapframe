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

Releases are cut by pushing a `vX.Y.Z` tag that matches `Cargo.toml`'s `version`; [.github/workflows/release.yml](.github/workflows/release.yml) builds the zip and creates the GitHub Release. CI runs `cargo clippy -- -D warnings`, so warnings fail the build.

`cargo build` fails with `os error 5` while a SnapFrame instance is running. Close it first — and close it *cleanly* (the toolbar's × button, or a `WM_CLOSE`), never `Stop-Process`, for the reason in Gotchas.

## What this is

A tray ("お盆") window that adopts other apps' top-level windows as children so Windows Snap moves and sizes them as one unit. See [README.md](README.md) for the user-facing behaviour.

## Architecture

Five modules. `window_manager` and `chrome` hold the bulk of the `unsafe`; `layout` is pure safe Rust with no Win32 calls at all, which is what makes the tiling rules cheap to reason about and change.

- [src/main.rs](src/main.rs) — Win32 entry point and `wndproc`. Decodes messages and calls into `App`; holds no state of its own.
- [src/app.rs](src/app.rs) — `App`: what is on the tray, hit-testing, the drag state machine, commands, focus and the non-client behaviour of the custom frame. Owned by the main window via `GWLP_USERDATA` (boxed in `WM_CREATE`, dropped in `WM_NCDESTROY`).
- [src/layout.rs](src/layout.rs) — the tiling model, its pixel geometry and the DPI-scaled `Metrics`. No Win32 calls beyond plain structs.
- [src/chrome.rs](src/chrome.rs) — GDI painting: toolbar, window controls, pane headers, splitters. Also owns the colour palette and the cached GDI objects.
- [src/window_manager.rs](src/window_manager.rs) — every `unsafe` call that touches a *foreign* window: enumeration, `SetParent`, style save/restore, focus, and the `ADOPTED` registry.

### The tiling model

Deliberately two levels: the tray is a **row of columns**, each column a **stack of slots**. A general split tree was not worth the hit-testing and drag complexity for what a tray does.

Sizes are **weights, never pixels**. `Layout::geometry` recomputes rects from weights on every resize, so a Snap is a pure recomputation with no accumulated drift. `distribute()` derives each position from a running cumulative fraction rather than summing rounded sizes, which is why the last item always lands exactly on the far edge — don't "simplify" it into a sum of rounded widths.

### Arrangements

Tiling is one of two `layout::Arrange` modes, switched from the toolbar's 配置 menu. The default is **`Free`**: each pane keeps the pixel size its window had on the desktop and sits at `Pane::free` (header + window, tray client coordinates). A window that already overlapped the tray is adopted at its exact screen position; others are cascaded. The header drags the pane instead of swapping it, and there are no splitters.

The column structure is still maintained in free mode — `Layout::add` places every pane in the grid regardless — so switching to `Tile` always has a grid, and `Pane::free` survives tiling, so switching back restores every position.

Free panes overlap, so `Geom::order` lists slots back to front (by the `Pane::z` stamp, bumped in `App::activate` via `Layout::raise`). Painting walks it forwards, `Geom::hit_test` backwards. In tile mode it is plain index order. `free_geometry` clamps panes into the tray without writing the clamp back, so shrinking and re-growing the tray is lossless; `Layout::move_free` *does* store the clamped rect, so a drag past the edge cannot leave the pane off-screen.

`App::geom` caches the computed rects. Everything (painting, hit-testing, child placement) reads that cache; `App::relayout()` is the single place that refills it and pushes children to their new rects.

### Adoption lifecycle

`window_manager::embed` strips the frame styles, `SetParent`s, and returns a `Snapshot`. `release` puts the snapshot back. Order matters in both directions: styles are rewritten **before** `SetParent` on the way in, because reparenting a window that still has `WS_CAPTION` leaves it drawing a caption it can no longer own.

`embed` also `AttachThreadInput`s our queue to the target's thread — that is the only reason `SetFocus` on an adopted window works at all, since it lives in another process. `release` detaches.

Every adoption is additionally recorded in the `ADOPTED` static so the panic hook can hand windows back before the process dies. `App` and that registry must stay in sync: go through `window_manager::{embed, release}`, never call `SetParent` directly.

### The custom frame

There is no system title bar. `WM_NCCALCSIZE` hands the caption strip to the client area, and the toolbar is drawn where the caption used to be.

The important choice: the window keeps `WS_OVERLAPPEDWINDOW` and a real frame. Dropping to `WS_POPUP` would have been simpler and would have cost Snap, the resize borders, the drop shadow and the rounded corners — all of which matter a lot for a tray whose entire purpose is Snap. So the frame is still there; we only paint over it.

What that costs, and where each piece lives:

- **`WM_NCCALCSIZE`** (`main::wndproc`, handled *before* the `App` lookup because it arrives during `CreateWindowExW`) calls `DefWindowProcW`, then restores the original `top`. Letting even one of these fall through gives a title-bar flash on startup.
- **Maximized windows overhang the monitor** by the frame thickness on every side. `WM_NCCALCSIZE` adds `resize_handle()` back to `top` when `IsZoomed`, or the toolbar sits off-screen.
- **`WM_NCHITTEST`** (`App::on_nc_hit_test`) reconstructs everything along the top: `DefWindowProcW` still gets the left/right/bottom borders right, but the top edge is client area now, so the resize grip (`HTTOP`/`HTTOPLEFT`/`HTTOPRIGHT`), the drag region (`HTCAPTION`) and the window controls are all synthesised. No grip when maximized — that window cannot be resized.
- **The window controls are ours to run.** Reporting `HTMINBUTTON`/`HTMAXBUTTON`/`HTCLOSE` gets hover and the Windows 11 Snap Layouts flyout for free, but no click handling, so `App::on_nc_lbutton_{down,up}` implements press-and-release itself.
- Hover on those buttons arrives as `WM_NCMOUSEMOVE`, which needs its own `TrackMouseEvent` with `TME_NONCLIENT`. Moving from a button into the client area produces no `WM_NCMOUSELEAVE`, so `on_mouse_move` clears the hover too.

Double-click-to-maximize, the Alt+Space system menu and right-click-on-caption all still come from `DefWindowProcW`, because the drag region is reported as a genuine `HTCAPTION`.

Caption *colour* is no longer a thing worth setting — there is no caption. `chrome::quiet_frame` only darkens the one-pixel border that is left.

### Focus

Click-to-focus is the tray's job, not the adopted app's, and this is not obvious enough to rediscover by reading the handlers.

A window that used to be top-level received keyboard focus through *activation*; it never had to call `SetFocus` on itself when clicked, because the system did it. Reparented, it gets neither — and the click goes straight to the child, so our `wndproc` never sees it. Left alone, the tray looks focused but nothing accepts typing.

Four handlers cover it, and removing any one leaves a dead path:

| Message | Case it covers |
|---|---|
| `WM_PARENTNOTIFY` | Click inside an adopted window while the tray is already active. The only signal we get for those clicks. |
| `WM_MOUSEACTIVATE` | Click that *activates* the tray. Records the pane; focus is applied by the `WM_ACTIVATE` that follows, because focus set before activation completes does not stick. |
| `WM_ACTIVATE` | Alt-Tab back to the tray. |
| `WM_SETFOCUS` | The tray has no keyboard UI, so it forwards focus to the active pane instead of keeping it — this is what stops clicks on our own chrome from stranding the keyboard. |

`App::adopt_focus` covers the remaining case: the focused pane being released or dying.

None of this works without the `AttachThreadInput` in `embed` — `SetFocus` on a window owned by another thread is a silent no-op otherwise.

Focus is also z-order. Every one of those paths funnels through `App::activate`, which raises the pane (`wm::raise_child`) as well as focusing it; keep new paths going through it rather than calling `wm::focus_child` directly. The tiling never overlaps, but an adopted app is free to move or size itself inside the tray and plenty do, and a child window gets no activation of its own to bring it forward — so without the raise the focused pane can sit underneath a neighbour that spilled over it.

For the same reason `App::pane_at_point` asks `ChildWindowFromPointEx` which window is actually under the pointer before falling back to the cached geometry: once panes overlap, the pane that owns a point in the tiling is not necessarily the one the user clicked. The fallback is what resolves clicks on our own chrome, where there is no child window to find.

## Gotchas

- **`match` on raw `u32` message constants is a trap.** An unimported constant is a valid catch-all binding, not a compile error, so it silently swallows every arm below it. This already happened once: `WM_MOUSELEAVE` lives in `Win32::UI::Controls`, not `WindowsAndMessaging`, and its bare name in `wndproc` swallowed every arm below it — the build still succeeded, with only `unreachable pattern` warnings to show for it. Read the warnings after touching `wndproc`.
- Other modules that are not where you would guess: `AttachThreadInput` is in `Win32::System::Threading`; `ClientToScreen` is in `Graphics::Gdi`.
- **Killing the tray kills the adopted apps.** Windows destroys child windows with their parent, so `Stop-Process` on SnapFrame closes whatever was on it. The `WM_DESTROY` path and the panic hook both release first; keep it that way, and never add an exit path that skips them.
- **Toolbar commands fire on `WM_LBUTTONUP`, not down.** A `TrackPopupMenu` opened during `WM_LBUTTONDOWN` is dismissed by the `WM_LBUTTONUP` that immediately follows it. `App::pressed_button` exists solely for this.
- `#![windows_subsystem = "windows"]` means **no console**: `println!`/`dbg!` go nowhere. Panics surface through the `MessageBoxW` hook in `install_panic_dialog`; for tracing, use `OutputDebugStringW` or temporarily drop the attribute.
- **Store apps (UWP) and elevated windows cannot be reparented.** `embed` returns `None` and `App::warn_embed_failed` explains it to the user — a silent no-op here reads as a bug.
- `refresh_children()` (a `RedrawWindow` sweep) exists because some apps leave stale pixels after being resized. Call it when a resize *finishes* — never per mouse-move, which would make splitter drags crawl.
- Liveness is a 400 ms `WM_TIMER` poll, not an `EVENT_OBJECT_DESTROY` hook. That is intentional: a system-wide destroy hook wakes the process for every menu and tooltip on the desktop, which costs far more than a handful of `IsWindow` calls.
- DPI is per-monitor v2 **plus** `SetThreadDpiHostingBehavior(DPI_HOSTING_BEHAVIOR_MIXED)`. Without mixed hosting, adopting a window whose process has different DPI awareness misbehaves.
- `windows` crate features are minimal and explicit in [Cargo.toml](Cargo.toml); a new Win32 API usually needs its feature added first.

## Not implemented

Absent by choice so far, not by oversight — don't assume any of it exists when reading the code:

- No persistence. Every launch starts with an empty tray in the free arrangement; there is no config file.
- No keyboard shortcuts and no tray icon.
- No way to adopt a window that appears *later* (no watching, no rules) — the user picks from the menu each time.
- Panes can be swapped but not dragged to an arbitrary position; the two-level tiling has no way to express "move this into a new column here".

## Testing against the real desktop

Driving the GUI from PowerShell works. These quirks are worth knowing before concluding something is broken — each one cost a round of false debugging:

- `FindWindow`/`FindWindowEx` return 0 in this environment. Locate windows with `EnumWindows` instead.
- The **first** synthetic click after a script starts is consumed by window activation. Send a harmless warm-up click before the one you are actually testing.
- Drive clicks in **client** coordinates (`ClientToScreen` on the tray), not window coordinates. With the caption reclaimed, the client origin is flush with the window's top edge, so the two differ by the frame inset only on the sides.
- Caption-button positions are right-aligned, so they move with the window width — recompute them after a maximize instead of reusing coordinates from before it.
- The "add window" menu lists candidates in z-order, which changes the moment the tray is focused. A test that picks an entry by index has to enumerate *after* focusing, and is still worth a retry loop.
- Sending `WM_CLOSE` to an adopted app can block: `SendMessage` waits, and the app may be showing a modal "save changes?" prompt. Use `PostMessage`, or kill the process when the point is to test the liveness poll.
