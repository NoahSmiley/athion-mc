//! Liminal Launcher — real Minecraft transition spike.
//!
//! Same windowing + state-machine mechanic as Spike #3, but the JVMs being
//! swapped are now **real heterogeneous Minecraft instances** launched via
//! Prism Launcher's CLI:
//!
//!   - Instance A: Minecraft 1.21.1 + NeoForge      (Prism instance "athion")
//!   - Instance B: Minecraft 1.21.11 + Fabric       (Prism instance "1.21.11")
//!
//! Without our IPC mod inside Minecraft (Milestone 2 work), there's no
//! `ready` / `shutdown` message exchange. Substitutes:
//!
//!   - "ready"     → poll `EnumWindows` for a window whose title contains
//!                   "Minecraft", record HWND + PID
//!   - "shutdown"  → `taskkill /F /T /PID <minecraft_pid>` (ungraceful;
//!                   single-player auto-saves but a real player on a
//!                   server would lose any in-flight inventory ops)
//!
//! Both substitutes are spike-grade. Production launcher restores both
//! using a Forge/Fabric mod that connects to the IPC server (Spike #2's
//! protocol) the moment Minecraft starts.
//!
//! # Per-instance setup the user must do
//!
//! Each Prism instance must be configured to **auto-join a world** on
//! launch (Edit Instance → Settings → "Game" → "Join world on launch").
//! Otherwise Minecraft boots to its main menu and the seamless-landing
//! property is lost — the user would see "loading screen → main menu in
//! a different version", which is not the product.
//!
//! # Usage
//!
//! 1. Confirm both Prism instances launch from the GUI directly into a
//!    world (no main menu, no prompts).
//! 2. `cargo run --release` from this dir.
//! 3. Wait for Instance A's Minecraft window. May take 30–60s for first
//!    NeoForge launch.
//! 4. Press **Ctrl+Shift+T** to transition.
//! 5. Watch: overlay over A → A vanishes → loading screen → B appears in
//!    A's old rect, fade out.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const PRISM_EXE: &str = r"C:\Users\noah\AppData\Local\Programs\PrismLauncher\prismlauncher.exe";
const INSTANCE_A: &str = "athion";   // NeoForge 1.21.1
const INSTANCE_B: &str = "1.21.11";  // Fabric 1.21.11

/// Server addresses passed via Prism's `--server` flag (the multiplayer
/// equivalent of `--world`; translates to Minecraft's
/// `--quickPlayMultiplayer`). This is the production-shaped path: real
/// users transition between actual Minecraft servers, not single-player
/// worlds. We point at two local vanilla servers spun up at
/// `~/Desktop/liminal-test-servers/{hub,survival}`.
const SERVER_A: &str = "localhost:25565"; // hub vanilla 1.21.1
const SERVER_B: &str = "localhost:25566"; // survival vanilla 1.21.11

const HOTKEY_ID_TRANSITION: i32 = 1;
const TIMER_ID_FADE: usize = 1;
const TIMER_ID_PROGRESS: usize = 2;

const FADE_DURATION_MS: i32 = 300;
const FADE_TICK_MS: u32 = 10;
/// How often to redraw the progress bar. The value displayed comes from
/// `PROGRESS_BPS` (a basis-points 0-10000 atomic) which the main thread
/// updates as the transition advances through stages.
const PROGRESS_TICK_MS: u32 = 50; // ~20fps; bar isn't animating per se

const OVERLAY_LOOP_SLEEP_MS: u64 = 5;

/// Substring matched against window titles when we're hunting for a
/// Minecraft window. Modern Minecraft titles look like "Minecraft 1.21.1"
/// or "Minecraft* 1.21.11 - Singleplayer".
const MINECRAFT_TITLE_NEEDLE: &str = "Minecraft";

/// Wall-clock budget for Minecraft to start up and present a window.
/// First-launch NeoForge in particular can take a while.
const MINECRAFT_STARTUP_TIMEOUT_SECS: u64 = 180;

const WINDOW_POLL_MS: u64 = 250;
const WINDOW_CLOSE_TIMEOUT_SECS: u64 = 15;

/// How long to keep the overlay up *after* Minecraft's window appears,
/// to give it time to finish loading the world. The window appears
/// early in Minecraft's init (black screen → Mojang splash → loading
/// screens), well before `--quickPlaySingleplayer` actually drops you
/// into the world. Without this delay, the overlay fades out while
/// Minecraft is still on its own loading screens, which defeats the
/// "seamless transition" illusion. Production solution: IPC
/// `world_loaded` signal from a mod inside Minecraft.
///
/// Tune this if your B instance is consistently slower/faster.
const POST_WORLD_LOAD_DELAY_SECS: u64 = 20;


// ---------------------------------------------------------------------------
// Overlay command channel (main → win32 thread)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum OverlayCmd {
    /// Reposition the overlay to this rect, then fade in.
    ShowAt { rect: RECT },
    /// Reposition only (no fade trigger). Used by the tracking loop that
    /// follows B's window during the post-launch hold.
    Reposition { rect: RECT },
    /// Toggle WS_EX_TRANSPARENT. true = clicks pass through (default,
    /// needed during fade-in so any in-flight A-input still reaches A);
    /// false = overlay swallows clicks (needed once A is dead, so the
    /// user can't drag the partially-loaded B window mid-transition).
    SetClickThrough { value: bool },
    /// Fade out (rect unchanged).
    Hide,
    /// Tear everything down and exit the win32 thread.
    Quit,
}

// ---------------------------------------------------------------------------
// Overlay-thread shared state
// ---------------------------------------------------------------------------

static CURRENT_ALPHA: AtomicI32 = AtomicI32::new(0);
static TARGET_ALPHA: AtomicI32 = AtomicI32::new(0);
/// Progress bar fill, in basis points (0-10000 = 0.0%-100.0%). The main
/// thread writes this as the transition advances; the overlay thread
/// reads it in `paint`. No cycling/animation — what you see is the
/// actual stage progress. Production launcher will be driven by IPC
/// `loading_progress` messages from a mod inside Minecraft.
static PROGRESS_BPS: AtomicI32 = AtomicI32::new(0);
static HOTKEY_SENDER: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    println!("[launcher] Liminal — real Minecraft transition spike");
    println!("[launcher]   Instance A: {INSTANCE_A}  (NeoForge 1.21.1)");
    println!("[launcher]   Instance B: {INSTANCE_B}  (Fabric 1.21.11)");

    // Sanity-check Prism is where we expect.
    if !std::path::Path::new(PRISM_EXE).exists() {
        bail!("PrismLauncher.exe not found at {PRISM_EXE}.\nUpdate the PRISM_EXE constant in src/main.rs.");
    }

    // Channels between main thread and the win32 (overlay+hotkey) thread.
    let (overlay_cmd_tx, overlay_cmd_rx) = mpsc::channel::<OverlayCmd>();
    let (hotkey_tx, hotkey_rx) = mpsc::channel::<()>();

    let overlay_handle = thread::Builder::new()
        .name("liminal-overlay".into())
        .spawn(move || {
            if let Err(e) = run_overlay_thread(overlay_cmd_rx, hotkey_tx) {
                eprintln!("[overlay] error: {e:#}");
            }
        })
        .context("spawn overlay thread")?;

    // ---- Stage 1: launch Instance A ----
    println!("\n[launcher] === Stage 1: launch Instance A ({INSTANCE_A}) ===");
    let mut a_prism = launch_via_prism(INSTANCE_A, SERVER_A)?;
    println!("[launcher]   Prism PID for A: {}", a_prism.id());
    println!("[launcher]   Waiting for Minecraft window (timeout {MINECRAFT_STARTUP_TIMEOUT_SECS}s)...");
    let (a_hwnd, a_pid) = wait_for_minecraft_window(MINECRAFT_STARTUP_TIMEOUT_SECS)?;
    println!("[launcher]   Minecraft A window: HWND={:?} PID={a_pid}", a_hwnd.0);

    println!("\n[launcher] Instance A is up. Press Ctrl+Alt+L to transition to {INSTANCE_B}.\n");

    // ---- Stage 2: wait for hotkey ----
    hotkey_rx.recv().context("hotkey channel closed")?;
    println!("\n[launcher] === Hotkey received — beginning transition ===");

    // ---- Stage 3: snap overlay over A ----
    let a_rect = unsafe { get_window_rect(a_hwnd) }.unwrap_or_else(|| primary_monitor_rect());
    println!(
        "[launcher]   A rect: ({},{}) {}x{}",
        a_rect.left, a_rect.top,
        a_rect.right - a_rect.left, a_rect.bottom - a_rect.top
    );
    overlay_cmd_tx.send(OverlayCmd::ShowAt { rect: a_rect })?;
    PROGRESS_BPS.store(500, Ordering::SeqCst); // 5%
    thread::sleep(Duration::from_millis(FADE_DURATION_MS as u64 + 50));
    PROGRESS_BPS.store(1000, Ordering::SeqCst); // 10% — overlay fully up
    println!("[launcher]   overlay faded in.");

    // ---- Stage 4: kill A ----
    println!("\n[launcher] === Stage 4: kill Instance A (PID {a_pid}) ===");
    kill_process_tree(a_pid).context("killing Minecraft A")?;
    wait_for_window_gone(a_hwnd, Duration::from_secs(WINDOW_CLOSE_TIMEOUT_SECS))?;
    PROGRESS_BPS.store(2500, Ordering::SeqCst); // 25% — A is gone
    println!("[launcher]   A window gone.");

    // Tear down the Prism parent in case it's still around (depends on Prism's
    // close-after-launch setting; `kill()` is a no-op if it already exited).
    let _ = a_prism.kill();
    let _ = a_prism.wait();

    // ---- Stage 5: launch Instance B ----
    println!("\n[launcher] === Stage 5: launch Instance B ({INSTANCE_B}) ===");
    let b_prism = launch_via_prism(INSTANCE_B, SERVER_B)?;
    PROGRESS_BPS.store(3000, Ordering::SeqCst); // 30% — Prism spawned
    println!("[launcher]   Prism PID for B: {}", b_prism.id());
    println!("[launcher]   Waiting for Minecraft window...");
    let (b_hwnd, b_pid) = wait_for_minecraft_window(MINECRAFT_STARTUP_TIMEOUT_SECS)?;
    PROGRESS_BPS.store(5000, Ordering::SeqCst); // 50% — Minecraft window is up
    println!("[launcher]   Minecraft B window: HWND={:?} PID={b_pid}", b_hwnd.0);

    // ---- Stage 6: position B at A's old rect ----
    // Real Minecraft's GLFW window doesn't honor a position arg the way
    // our Spike #3 test instances did. Instead we SetWindowPos right after
    // the window appears. Fast enough to feel "always was there" once the
    // overlay fades out.
    unsafe {
        let _ = SetWindowPos(
            b_hwnd,
            HWND::default(),
            a_rect.left,
            a_rect.top,
            a_rect.right - a_rect.left,
            a_rect.bottom - a_rect.top,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
    // Block clicks from passing through to B during the load hold. Without
    // this, the user can grab B's title bar (visible in the gap between
    // overlay and B if our SetWindowPos didn't take, or even through the
    // overlay if WS_EX_TRANSPARENT is still active) and drag B around.
    overlay_cmd_tx.send(OverlayCmd::SetClickThrough { value: false })?;

    // Hold the overlay for `POST_WORLD_LOAD_DELAY_SECS`, but DON'T just
    // sleep — track B's window position the whole time. GLFW reposition
    // the Minecraft window during init (back to a default location after
    // our SetWindowPos), and that drift breaks the illusion. Re-snap the
    // overlay every 250ms whenever B's rect changes.
    println!("[launcher]   holding overlay {POST_WORLD_LOAD_DELAY_SECS}s, tracking B's rect...");
    let hold_start = Instant::now();
    let hold_duration = Duration::from_secs(POST_WORLD_LOAD_DELAY_SECS);
    let mut last_rect = a_rect;
    // Progress climbs linearly from 50% (window up) to 95% (just before
    // we say "loaded"). The remaining 5% lands on Hide, since the user
    // perceives "complete" only when the overlay is gone.
    const HOLD_PROGRESS_START: i32 = 5000;
    const HOLD_PROGRESS_END: i32 = 9500;
    while hold_start.elapsed() < hold_duration {
        if let Some(rect) = unsafe { get_window_rect(b_hwnd) } {
            if rect.left != last_rect.left
                || rect.top != last_rect.top
                || rect.right != last_rect.right
                || rect.bottom != last_rect.bottom
            {
                let _ = overlay_cmd_tx.send(OverlayCmd::Reposition { rect });
                last_rect = rect;
            }
        }
        let frac = (hold_start.elapsed().as_millis() as f32
            / hold_duration.as_millis() as f32).min(1.0);
        let bps = HOLD_PROGRESS_START
            + ((HOLD_PROGRESS_END - HOLD_PROGRESS_START) as f32 * frac) as i32;
        PROGRESS_BPS.store(bps, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(250));
    }
    PROGRESS_BPS.store(10000, Ordering::SeqCst); // 100% — done

    // ---- Stage 7: fade overlay out ----
    println!("\n[launcher] === Stage 7: reveal B (fade out overlay) ===");
    overlay_cmd_tx.send(OverlayCmd::Hide)?;
    thread::sleep(Duration::from_millis(FADE_DURATION_MS as u64 + 50));

    println!("\n[launcher] === Spike: PASS — B is visible where A used to be ===\n");

    // ---- Stage 8: spike done; leave B running for the user ----
    // The transition has completed; Fabric is now the user's to play with.
    // We do NOT kill B here. `std::process::Child::Drop` does not kill the
    // underlying process, so dropping `b_prism` at end-of-scope leaves the
    // Prism+Java tree alive. The user closes Minecraft normally when done.
    //
    // We do still tear down the overlay thread so its (currently invisible)
    // window goes away cleanly and our process exits. Suppressing unused
    // warnings on b_pid since it's no longer referenced after this point.
    let _ = b_pid;
    let _ = b_prism;
    println!("[launcher] transition complete. Leaving B running. Exiting spike.");

    let _ = overlay_cmd_tx.send(OverlayCmd::Quit);
    let _ = overlay_handle.join();

    Ok(())
}

// ---------------------------------------------------------------------------
// Process management
// ---------------------------------------------------------------------------

fn launch_via_prism(instance: &str, server: &str) -> Result<std::process::Child> {
    Command::new(PRISM_EXE)
        .arg("--launch")
        .arg(instance)
        .arg("--server")
        .arg(server)
        // Don't pollute our terminal with Prism's own logs — the user
        // can open Prism's GUI to see them.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn Prism with --launch {instance} --server {server:?}"))
}

/// `taskkill /F /T /PID <pid>` — kill the process and its descendants.
/// `/T` matters: if Prism is the parent of javaw, killing only Prism
/// doesn't kill Minecraft on Windows (no auto-cascade like POSIX).
fn kill_process_tree(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("invoke taskkill")?;
    if !status.success() {
        bail!("taskkill exited non-zero (process may have already gone)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Window discovery
// ---------------------------------------------------------------------------

/// Polls `EnumWindows` until we see a visible top-level window whose title
/// contains "Minecraft". Returns its HWND and owner PID.
fn wait_for_minecraft_window(timeout_secs: u64) -> Result<(HWND, u32)> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if let Some(hwnd) = find_window_by_title(MINECRAFT_TITLE_NEEDLE) {
            let pid = unsafe { get_window_pid(hwnd) };
            return Ok((hwnd, pid));
        }
        if start.elapsed() > timeout {
            bail!("Minecraft window did not appear within {timeout_secs}s");
        }
        thread::sleep(Duration::from_millis(WINDOW_POLL_MS));
    }
}

fn wait_for_window_gone(hwnd: HWND, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let exists = unsafe { IsWindow(hwnd).as_bool() };
        if !exists {
            return Ok(());
        }
        if start.elapsed() > timeout {
            bail!("window {:?} did not close within timeout", hwnd.0);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn find_window_by_title(needle: &str) -> Option<HWND> {
    unsafe {
        let mut found: Option<HWND> = None;
        let mut data: (&str, *mut Option<HWND>) = (needle, &mut found);
        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(&mut data as *mut _ as isize),
        );
        found
    }
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &*(lparam.0 as *const (&str, *mut Option<HWND>));
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len <= 0 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    if title.contains(data.0) {
        *data.1 = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

unsafe fn get_window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_ok() { Some(rect) } else { None }
}

unsafe fn get_window_pid(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    pid
}

fn primary_monitor_rect() -> RECT {
    unsafe {
        RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 overlay thread (lifted from Spike #3, no IPC dependencies)
// ---------------------------------------------------------------------------

fn run_overlay_thread(
    cmd_rx: mpsc::Receiver<OverlayCmd>,
    hotkey_tx: mpsc::Sender<()>,
) -> Result<()> {
    unsafe { run_overlay_thread_inner(cmd_rx, hotkey_tx) }
}

unsafe fn run_overlay_thread_inner(
    cmd_rx: mpsc::Receiver<OverlayCmd>,
    hotkey_tx: mpsc::Sender<()>,
) -> Result<()> {
    let h_instance = GetModuleHandleW(None)?;
    let class_name = w!("LiminalMcOverlay");

    let wc = WNDCLASSW {
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hInstance: h_instance.into(),
        lpszClassName: class_name,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_wndproc),
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(Error::from_win32().into());
    }

    let initial = primary_monitor_rect();

    let ex_style = WS_EX_LAYERED
        | WS_EX_TRANSPARENT
        | WS_EX_TOPMOST
        | WS_EX_TOOLWINDOW
        | WS_EX_NOACTIVATE;

    let hwnd = CreateWindowExW(
        ex_style,
        class_name,
        w!("Liminal MC Overlay"),
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

    *HOTKEY_SENDER.lock().unwrap() = Some(hotkey_tx);

    SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA)?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    // Ctrl+Alt+L. Avoiding Ctrl+Shift+T because Chrome and other browsers
    // grab it for "reopen last closed tab" when they're the foreground
    // window — global hotkeys lose to focused-app shortcuts on Windows.
    RegisterHotKey(
        hwnd,
        HOTKEY_ID_TRANSITION,
        MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
        'L' as u32,
    )?;

    SetTimer(hwnd, TIMER_ID_PROGRESS, PROGRESS_TICK_MS, None);

    loop {
        // Drain commands.
        loop {
            match cmd_rx.try_recv() {
                Ok(OverlayCmd::ShowAt { rect }) => {
                    let _ = SetWindowPos(
                        hwnd,
                        HWND::default(),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                    TARGET_ALPHA.store(255, Ordering::SeqCst);
                    SetTimer(hwnd, TIMER_ID_FADE, FADE_TICK_MS, None);
                }
                Ok(OverlayCmd::Reposition { rect }) => {
                    let _ = SetWindowPos(
                        hwnd,
                        HWND::default(),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                }
                Ok(OverlayCmd::SetClickThrough { value }) => {
                    // Toggle WS_EX_TRANSPARENT on the live window. Style
                    // changes don't take effect until SWP_FRAMECHANGED.
                    let mut style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                    if value {
                        style |= WS_EX_TRANSPARENT.0 as isize;
                    } else {
                        style &= !(WS_EX_TRANSPARENT.0 as isize);
                    }
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
                    let _ = SetWindowPos(
                        hwnd,
                        HWND::default(),
                        0, 0, 0, 0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER
                            | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                }
                Ok(OverlayCmd::Hide) => {
                    TARGET_ALPHA.store(0, Ordering::SeqCst);
                    SetTimer(hwnd, TIMER_ID_FADE, FADE_TICK_MS, None);
                }
                Ok(OverlayCmd::Quit) => {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ID_TRANSITION);
                    let _ = DestroyWindow(hwnd);
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ID_TRANSITION);
                    let _ = DestroyWindow(hwnd);
                    return Ok(());
                }
            }
        }

        // Drain Win32 messages (PeekMessage is non-blocking).
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return Ok(());
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        thread::sleep(Duration::from_millis(OVERLAY_LOOP_SLEEP_MS));
    }
}

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID_TRANSITION => {
            if let Ok(guard) = HOTKEY_SENDER.lock() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.send(());
                }
            }
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

unsafe fn tick_fade(hwnd: HWND) {
    let cur = CURRENT_ALPHA.load(Ordering::SeqCst);
    let tgt = TARGET_ALPHA.load(Ordering::SeqCst);

    if cur == tgt {
        let _ = KillTimer(hwnd, TIMER_ID_FADE);
        return;
    }

    let step = (255 * FADE_TICK_MS as i32 / FADE_DURATION_MS).max(1);
    let next = if cur < tgt { (cur + step).min(tgt) } else { (cur - step).max(tgt) };
    CURRENT_ALPHA.store(next, Ordering::SeqCst);
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), next as u8, LWA_ALPHA);
}

unsafe fn tick_progress(hwnd: HWND) {
    // Just invalidate — the bar's value comes from PROGRESS_BPS, which
    // the main thread writes. Periodic redraw picks up changes without
    // the main thread needing to know the overlay's hwnd.
    let _ = InvalidateRect(hwnd, None, false);
}

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

    // Double-buffer to a memory DC, BitBlt the composite once.
    let mem_dc = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old_bmp = SelectObject(mem_dc, bmp);

    let bg = CreateSolidBrush(COLORREF(0x00284040));
    let _ = FillRect(mem_dc, &rect, bg);
    let _ = DeleteObject(bg);

    let bar_w = w * 6 / 10;
    let bar_h = 24;
    let bar_x = (w - bar_w) / 2;
    let bar_y = h * 3 / 4;

    let outline = RECT {
        left: bar_x - 2, top: bar_y - 2,
        right: bar_x + bar_w + 2, bottom: bar_y + bar_h + 2,
    };
    let outline_brush = CreateSolidBrush(COLORREF(0x00FFFFFF));
    let _ = FillRect(mem_dc, &outline, outline_brush);
    let _ = DeleteObject(outline_brush);

    let bg_bar = RECT {
        left: bar_x, top: bar_y,
        right: bar_x + bar_w, bottom: bar_y + bar_h,
    };
    let bg_brush = CreateSolidBrush(COLORREF(0x00101010));
    let _ = FillRect(mem_dc, &bg_bar, bg_brush);
    let _ = DeleteObject(bg_brush);

    let progress = (PROGRESS_BPS.load(Ordering::SeqCst).clamp(0, 10000) as f32) / 10000.0;
    let fill = RECT {
        left: bar_x, top: bar_y,
        right: bar_x + (bar_w as f32 * progress) as i32,
        bottom: bar_y + bar_h,
    };
    let fill_brush = CreateSolidBrush(COLORREF(0x0080FF80));
    let _ = FillRect(mem_dc, &fill, fill_brush);
    let _ = DeleteObject(fill_brush);

    let _ = BitBlt(hdc, 0, 0, w, h, mem_dc, 0, 0, SRCCOPY);

    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem_dc);

    let _ = EndPaint(hwnd, &ps);
}
