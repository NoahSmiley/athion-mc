//! Liminal Launcher — Spike #1: transparent always-on-top overlay window.
//!
//! # What this validates
//!
//! Whether the Win32 layered-window primitives the launcher needs for its
//! seamless-transition loading screen actually behave the way we want when
//! a real Minecraft client is running underneath. Specifically:
//!
//!   - `WS_EX_LAYERED`     → smooth alpha fade in/out
//!   - `WS_EX_TRANSPARENT` → mouse input falls through to the game
//!   - `WS_EX_TOPMOST`     → stays above the game window
//!   - `WS_EX_TOOLWINDOW`  → no taskbar entry, no Alt-Tab
//!   - `WS_EX_NOACTIVATE`  → never steals focus from the game
//!
//! Combined, these should let us draw a loading screen on top of Minecraft
//! during a transition without the user perceiving any window-management
//! activity. See the README for the test matrix this spike is meant to
//! exercise across windowed / borderless / exclusive-fullscreen modes.
//!
//! # Window targeting
//!
//! On each fade-in, the spike enumerates top-level windows, finds one
//! whose title contains "Minecraft", and snaps the overlay to that
//! window's rect. This makes the spike approximate what the production
//! launcher will do during a real transition. If no Minecraft window is
//! found, the overlay falls back to the primary monitor.
//!
//! Snapping happens only at fade-in time — not real-time tracking. Real
//! transitions only show the overlay for a few seconds, so the user
//! isn't dragging windows around mid-transition.
//!
//! # Painting is double-buffered
//!
//! The progress bar redraws ~60 times a second. Without double-buffering,
//! the layered-window compositor catches WM_PAINT mid-execution and shows
//! intermediate states (visible flicker between the dirt fill and the bar
//! being drawn on top). `paint()` composes the entire frame into an
//! off-screen memory DC + bitmap, then BitBlts the finished frame to the
//! window DC in one operation.
//!
//! # Usage
//!
//! `cargo run --release`, then press Ctrl+Shift+L to toggle the overlay.
//! Kill the process from its terminal (Ctrl+C) when done.

use std::sync::atomic::{AtomicI32, Ordering};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------- Tunables ----------

const HOTKEY_ID_TOGGLE: i32 = 1;

const TIMER_ID_FADE: usize = 1;
const TIMER_ID_PROGRESS: usize = 2;

/// Total duration of an alpha fade, in ms. Matches CONTEXT.md (300ms).
const FADE_DURATION_MS: i32 = 300;
/// Tick interval for the fade animation. ~30 ticks per fade.
const FADE_TICK_MS: u32 = 10;
/// Tick interval for the placeholder progress-bar animation. ~60 fps.
const PROGRESS_TICK_MS: u32 = 16;
/// Time for the placeholder progress bar to sweep across once.
const PROGRESS_CYCLE_MS: i32 = 5000;

/// Substring matched against top-level window titles when searching for
/// the Minecraft client. Modern Minecraft launchers always include the
/// word "Minecraft" somewhere in the window title (`"Minecraft 1.21"`,
/// `"Minecraft* 1.20.4 - Singleplayer"`, etc.).
const MINECRAFT_TITLE_NEEDLE: &str = "Minecraft";

// ---------- Animation state ----------
//
// Lives in static atomics so the bare `extern "system"` wndproc can read
// and mutate it without juggling user-data slots. Fine because there's
// exactly one overlay window in this spike. The production launcher will
// hold this in a struct attached to the window via SetWindowLongPtr so
// multiple overlays (one per monitor, eventually) don't trample state.

static CURRENT_ALPHA: AtomicI32 = AtomicI32::new(0);
static TARGET_ALPHA: AtomicI32 = AtomicI32::new(0);
static PROGRESS_MS: AtomicI32 = AtomicI32::new(0);
static VISIBLE: AtomicI32 = AtomicI32::new(0);

fn main() -> Result<()> {
    println!("Liminal overlay spike running.");
    println!("  Hotkey:    Ctrl+Shift+L (toggle overlay)");
    println!("  Targeting: snaps to Minecraft window if running, else primary monitor");
    println!("  Exit:      Ctrl+C in this terminal");
    unsafe { run() }
}

unsafe fn run() -> Result<()> {
    let h_instance = GetModuleHandleW(None)?;
    let class_name = w!("LiminalOverlaySpike");

    let wc = WNDCLASSW {
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hInstance: h_instance.into(),
        lpszClassName: class_name,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        // We paint the full client area in WM_PAINT, so no class background
        // brush. Letting the OS clear with a default brush would cause a
        // visible flash before our paint runs.
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(Error::from_win32());
    }

    // Initial size: the primary monitor. Each fade-in calls SetWindowPos
    // to snap the overlay onto the Minecraft window's current rect, so
    // this initial sizing is just a placeholder.
    let initial = primary_monitor_rect();

    // Extended styles, in detail:
    //
    //   WS_EX_LAYERED     — required for SetLayeredWindowAttributes (which
    //                       is how we get smooth alpha fading). Also enables
    //                       UpdateLayeredWindow if we later need per-pixel
    //                       alpha; not used in this spike.
    //
    //   WS_EX_TRANSPARENT — mouse input falls through to whatever's below.
    //                       Combined with NOACTIVATE, Minecraft has no idea
    //                       the overlay exists for input purposes.
    //
    //   WS_EX_TOPMOST     — stays above normal windows. Does NOT keep us
    //                       above exclusive-fullscreen apps; whether that
    //                       matters in practice for Minecraft is exactly
    //                       what the test matrix is meant to expose.
    //
    //   WS_EX_TOOLWINDOW  — keeps us out of the taskbar and Alt-Tab. The
    //                       overlay should be invisible to the OS shell.
    //
    //   WS_EX_NOACTIVATE  — never becomes the foreground window, even if
    //                       a click somehow reaches it. Critical: without
    //                       this, the OS can yank focus from Minecraft when
    //                       we appear, which would defeat the whole point.
    let ex_style = WS_EX_LAYERED
        | WS_EX_TRANSPARENT
        | WS_EX_TOPMOST
        | WS_EX_TOOLWINDOW
        | WS_EX_NOACTIVATE;

    let hwnd = CreateWindowExW(
        ex_style,
        class_name,
        w!("Liminal Overlay Spike"),
        // WS_POPUP gives us a borderless window — no titlebar, no frame.
        WS_POPUP,
        initial.left,
        initial.top,
        initial.right - initial.left,
        initial.bottom - initial.top,
        None,
        None,
        h_instance,
        None,
    )?;

    // Start fully transparent. SW_SHOWNOACTIVATE lets the layered window
    // appear without focus change — required to honor WS_EX_NOACTIVATE.
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA)?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    // Ctrl+Shift+L is the toggle. MOD_NOREPEAT prevents key autorepeat
    // from queueing a string of toggle messages while you hold the keys.
    RegisterHotKey(
        hwnd,
        HOTKEY_ID_TOGGLE,
        MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
        'L' as u32,
    )?;

    // Drive the placeholder progress bar at ~60 fps so the spike has
    // something visibly moving on screen while you toggle.
    SetTimer(hwnd, TIMER_ID_PROGRESS, PROGRESS_TICK_MS, None);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).into() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    let _ = UnregisterHotKey(hwnd, HOTKEY_ID_TOGGLE);
    Ok(())
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID_TOGGLE => {
            let now_visible = VISIBLE.load(Ordering::SeqCst) != 0;
            let new_target = if now_visible { 0 } else { 255 };
            VISIBLE.store(if now_visible { 0 } else { 1 }, Ordering::SeqCst);
            TARGET_ALPHA.store(new_target, Ordering::SeqCst);

            // Snap to Minecraft's rect on each fade-in. The overlay is
            // currently invisible (alpha 0), so resizing now doesn't
            // visually flicker.
            if !now_visible {
                snap_to_minecraft(hwnd);
            }

            SetTimer(hwnd, TIMER_ID_FADE, FADE_TICK_MS, None);
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                t if t == TIMER_ID_FADE => tick_fade(hwnd),
                t if t == TIMER_ID_PROGRESS => tick_progress(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------- Window targeting ----------

unsafe fn snap_to_minecraft(overlay: HWND) {
    let target = match find_minecraft_window() {
        Some(mc_hwnd) => {
            let mut rect = RECT::default();
            if GetWindowRect(mc_hwnd, &mut rect).is_ok() {
                println!(
                    "snap: Minecraft at ({}, {}) {}x{}",
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                );
                rect
            } else {
                println!("snap: GetWindowRect failed; falling back to primary monitor");
                primary_monitor_rect()
            }
        }
        None => {
            println!("snap: no Minecraft window found; covering primary monitor");
            primary_monitor_rect()
        }
    };

    // SWP_NOACTIVATE: don't take focus.
    // SWP_NOZORDER:   keep the existing topmost z-order from CreateWindowExW.
    let _ = SetWindowPos(
        overlay,
        HWND::default(),
        target.left,
        target.top,
        target.right - target.left,
        target.bottom - target.top,
        SWP_NOACTIVATE | SWP_NOZORDER,
    );
}

unsafe fn primary_monitor_rect() -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: GetSystemMetrics(SM_CXSCREEN),
        bottom: GetSystemMetrics(SM_CYSCREEN),
    }
}

unsafe fn find_minecraft_window() -> Option<HWND> {
    // Use the LPARAM as an out-parameter: pass a pointer to our Option,
    // the callback writes into it on a match and returns FALSE to halt
    // enumeration.
    let mut found: Option<HWND> = None;
    let _ = EnumWindows(
        Some(enum_callback),
        LPARAM(&mut found as *mut Option<HWND> as isize),
    );
    found
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // Skip invisible windows — Minecraft launchers and other tooling often
    // hold hidden child windows we don't want to grab.
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1); // continue enumeration
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len <= 0 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    if title.contains(MINECRAFT_TITLE_NEEDLE) {
        let out = lparam.0 as *mut Option<HWND>;
        *out = Some(hwnd);
        return BOOL(0); // stop enumeration
    }
    BOOL(1)
}

// ---------- Animation ----------

unsafe fn tick_fade(hwnd: HWND) {
    let cur = CURRENT_ALPHA.load(Ordering::SeqCst);
    let tgt = TARGET_ALPHA.load(Ordering::SeqCst);

    if cur == tgt {
        let _ = KillTimer(hwnd, TIMER_ID_FADE);
        return;
    }

    // Linear lerp. 300ms / 10ms tick = 30 steps; 255 / 30 ≈ 8.5, so we
    // step ~8 alpha units per tick. `.max(1)` guarantees forward progress
    // for any timing config; the min/max-against-target clamp prevents
    // overshoot on the last tick.
    let step = (255 * FADE_TICK_MS as i32 / FADE_DURATION_MS).max(1);
    let next = if cur < tgt {
        (cur + step).min(tgt)
    } else {
        (cur - step).max(tgt)
    };
    CURRENT_ALPHA.store(next, Ordering::SeqCst);
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), next as u8, LWA_ALPHA);
}

unsafe fn tick_progress(hwnd: HWND) {
    let new_ms =
        (PROGRESS_MS.load(Ordering::SeqCst) + PROGRESS_TICK_MS as i32) % PROGRESS_CYCLE_MS;
    PROGRESS_MS.store(new_ms, Ordering::SeqCst);
    // false = don't erase background; we paint the full client area.
    let _ = InvalidateRect(hwnd, None, false);
}

// ---------- Painting (double-buffered) ----------

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;

    if w <= 0 || h <= 0 {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // Double-buffer: paint everything into a memory DC + bitmap, then
    // BitBlt the finished frame to the window DC in one operation. Without
    // this, the layered-window compositor can catch us mid-paint and show
    // intermediate states (visible flicker between the dirt fill and the
    // bar being drawn on top).
    let mem_dc = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old_bmp = SelectObject(mem_dc, bmp);

    // Background: stylized dirt-brown placeholder. The real loading screen
    // will use an actual tiled dirt texture matching Minecraft's aesthetic.
    // COLORREF byte order is 0x00BBGGRR.
    let bg = CreateSolidBrush(COLORREF(0x00284040));
    let _ = FillRect(mem_dc, &rect, bg);
    let _ = DeleteObject(bg);

    // Progress bar: centered horizontally in the lower third.
    let bar_w = w * 6 / 10;
    let bar_h = 24;
    let bar_x = (w - bar_w) / 2;
    let bar_y = h * 3 / 4;

    // White outline border (2px).
    let outline = RECT {
        left: bar_x - 2,
        top: bar_y - 2,
        right: bar_x + bar_w + 2,
        bottom: bar_y + bar_h + 2,
    };
    let outline_brush = CreateSolidBrush(COLORREF(0x00FFFFFF));
    let _ = FillRect(mem_dc, &outline, outline_brush);
    let _ = DeleteObject(outline_brush);

    // Bar background (very dark gray).
    let bg_bar = RECT {
        left: bar_x,
        top: bar_y,
        right: bar_x + bar_w,
        bottom: bar_y + bar_h,
    };
    let bg_brush = CreateSolidBrush(COLORREF(0x00101010));
    let _ = FillRect(mem_dc, &bg_bar, bg_brush);
    let _ = DeleteObject(bg_brush);

    // Bar fill, animated. Bright Minecraft-y green.
    let progress = PROGRESS_MS.load(Ordering::SeqCst) as f32 / PROGRESS_CYCLE_MS as f32;
    let fill = RECT {
        left: bar_x,
        top: bar_y,
        right: bar_x + (bar_w as f32 * progress) as i32,
        bottom: bar_y + bar_h,
    };
    let fill_brush = CreateSolidBrush(COLORREF(0x0080FF80));
    let _ = FillRect(mem_dc, &fill, fill_brush);
    let _ = DeleteObject(fill_brush);

    // Composite the finished frame to the window DC in a single op.
    let _ = BitBlt(hdc, 0, 0, w, h, mem_dc, 0, 0, SRCCOPY);

    // Cleanup: restore the original bitmap, delete ours, delete the mem DC.
    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem_dc);

    let _ = EndPaint(hwnd, &ps);
}
