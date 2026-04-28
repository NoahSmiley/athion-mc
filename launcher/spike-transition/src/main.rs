//! Liminal Launcher — Spike #3: full instance-A → instance-B transition.
//!
//! Combines the Spike #1 overlay window with the Spike #2 JVM/IPC machinery
//! into a real seamless transition: overlay fades in over Instance A, A is
//! killed via IPC `shutdown`, B is launched hidden at A's old rect, B
//! reports `ready`, B is told to `show_window`, overlay fades out, B is
//! visible where A used to be.
//!
//! This is the rehearsal for the production transition path. Every
//! mechanism here will be reused — same overlay style, same IPC protocol
//! shape, same JVM-spawn pattern, same window-targeting trick.
//!
//! # Threads & channels
//!
//! - **Tokio runtime** (main): hosts the IPC server, manages instance
//!   lifecycles, drives the transition state machine.
//! - **Win32 thread**: hosts the overlay window + global hotkey, runs the
//!   Win32 message pump (interleaved with `try_recv` on the command
//!   channel so it can react to overlay commands without blocking
//!   exclusively in `GetMessage`).
//!
//! Communication:
//! - `OverlayCmd` (std mpsc, main → win32): `ShowAt` / `Hide` / `Quit`.
//! - Hotkey events (tokio mpsc, win32 → main): unit signal when
//!   Ctrl+Shift+T fires. (`tokio::sync::mpsc::UnboundedSender::send` is
//!   non-blocking and `Send`, safe to call from the win32 thread.)
//!
//! # Usage
//!
//! 1. `cd test-instance && javac TestInstance.java`  (one-time)
//! 2. `cargo run --release`
//! 3. Wait for the blue "A" window to appear.
//! 4. Press **Ctrl+Shift+T**.
//! 5. Watch: overlay fades in over A → A vanishes → loading screen
//!    holds → red "B" window appears underneath as overlay fades out.
//! 6. After 5 seconds B is shut down and the spike exits.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

const PROTOCOL_VERSION: u32 = 1;

const HOTKEY_ID_TRANSITION: i32 = 1;

const TIMER_ID_FADE: usize = 1;
const TIMER_ID_PROGRESS: usize = 2;

const FADE_DURATION_MS: i32 = 300;
const FADE_TICK_MS: u32 = 10;
const PROGRESS_TICK_MS: u32 = 16;
const PROGRESS_CYCLE_MS: i32 = 5000;

/// How long the overlay thread sleeps between drain cycles. ~200Hz event
/// handling, fast enough for hotkey + fade animation.
const OVERLAY_LOOP_SLEEP_MS: u64 = 5;

const CONNECT_TIMEOUT_SECS: u64 = 30;
const READY_TIMEOUT_SECS: u64 = 30;
const MESSAGE_TIMEOUT_SECS: u64 = 10;
const SHUTDOWN_TIMEOUT_SECS: u64 = 10;

/// Title prefix the test instances use; we look for the full window via
/// substring match against this prefix + the instance name.
const INSTANCE_TITLE_PREFIX: &str = "Liminal Test Instance ";

/// Where Instance A appears initially (cargo `current_dir` is the spike
/// crate root, so this is just the launch position; on transition we
/// snapshot whatever rect the user has dragged the window to).
const INITIAL_INSTANCE_RECT: RECT = RECT {
    left: 240,
    top: 120,
    right: 240 + 960,
    bottom: 120 + 640,
};

/// How long to leave Instance B on screen after the transition completes
/// before tearing it down — gives the user time to confirm visually that
/// the swap really happened.
const POST_TRANSITION_HOLD_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg {
    // ---- client → server ----
    Auth { token: String, protocol_version: u32 },
    Ready { instance_name: String },
    Bye,

    // ---- server → client ----
    AuthOk,
    AuthRejected { reason: String },
    ShowWindow,
    Shutdown { reason: String },
}

// ---------------------------------------------------------------------------
// Overlay thread interface
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum OverlayCmd {
    /// Reposition the overlay to this rect, then fade in.
    ShowAt { rect: RECT },
    /// Fade out (rect unchanged).
    Hide,
    /// Tear everything down and exit the win32 thread.
    Quit,
}

// Shared state for the overlay thread. Lives in static atomics + a mutex
// so the bare `extern "system"` wndproc can read/write it without juggling
// user-data slots. There's exactly one overlay window in this spike;
// production launchers might want one per monitor and would put this
// state on a struct attached to each window via SetWindowLongPtr.
static CURRENT_ALPHA: AtomicI32 = AtomicI32::new(0);
static TARGET_ALPHA: AtomicI32 = AtomicI32::new(0);
static PROGRESS_MS: AtomicI32 = AtomicI32::new(0);

/// Set by the overlay thread on startup, read by the wndproc to forward
/// hotkey presses back to the tokio runtime.
static HOTKEY_SENDER: Mutex<Option<mpsc::UnboundedSender<()>>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    println!("[launcher] Liminal Spike #3 — full transition");

    // Channels between tokio main and the win32 thread.
    let (overlay_cmd_tx, overlay_cmd_rx) = std::sync::mpsc::channel::<OverlayCmd>();
    let (hotkey_tx, mut hotkey_rx) = mpsc::unbounded_channel::<()>();

    // Spawn the win32 thread. It owns the overlay HWND + the global hotkey.
    let overlay_handle = std::thread::Builder::new()
        .name("liminal-overlay".into())
        .spawn(move || {
            if let Err(e) = run_overlay_thread(overlay_cmd_rx, hotkey_tx) {
                eprintln!("[overlay] error: {e:#}");
            }
        })
        .context("failed to spawn overlay thread")?;

    // IPC server.
    let listener = TcpListener::bind("127.0.0.1:0").await
        .context("bind localhost listener")?;
    let port = listener.local_addr()?.port();
    let auth_token = generate_auth_token();
    println!("[launcher] WebSocket listening on ws://127.0.0.1:{port}");

    // Locate the compiled test instance.
    let test_dir = std::env::current_dir()?.join("test-instance");
    let test_class = test_dir.join("TestInstance.class");
    if !test_class.exists() {
        bail!(
            "TestInstance.class not found at {}.\n\
             Compile it first: cd test-instance && javac TestInstance.java",
            test_class.display()
        );
    }

    // ---- Stage 1: launch Instance A ----
    println!("\n[launcher] === Stage 1: launch Instance A ===");
    let mut current = launch_instance(InstanceLaunch {
        listener: &listener,
        auth_token: &auth_token,
        port,
        test_dir: &test_dir,
        name: "A",
        color: "blue",
        rect: INITIAL_INSTANCE_RECT,
        hidden: false,
    })
    .await
    .context("launching Instance A")?;
    wait_for_ready(&mut current).await.context("waiting for A ready")?;

    println!("\n[launcher] Instance A is up. Press Ctrl+Shift+T to transition to Instance B.\n");

    // ---- Stage 2: wait for hotkey ----
    hotkey_rx.recv().await.ok_or_else(|| anyhow!("hotkey channel closed"))?;
    println!("\n[launcher] === Stage 2: hotkey received — beginning transition ===");

    // ---- Stage 3: snap overlay over A ----
    // Snapshot A's current screen rect (the user might have moved/resized
    // the window since launch). Fall back to the launch rect if lookup
    // somehow fails — overlay-over-launch-rect is still useful.
    let title_a = format!("{INSTANCE_TITLE_PREFIX}{}", current.name);
    let a_rect = find_window_by_title(&title_a)
        .and_then(|hwnd| unsafe { get_window_rect(hwnd) })
        .unwrap_or(INITIAL_INSTANCE_RECT);
    println!(
        "[launcher] A rect: ({},{}) {}x{}",
        a_rect.left, a_rect.top,
        a_rect.right - a_rect.left, a_rect.bottom - a_rect.top
    );
    overlay_cmd_tx.send(OverlayCmd::ShowAt { rect: a_rect })?;
    sleep(Duration::from_millis(FADE_DURATION_MS as u64 + 50)).await;
    println!("[launcher] overlay faded in.");

    // ---- Stage 4: shut down A ----
    println!("\n[launcher] === Stage 4: shut down Instance A ===");
    current.send(Msg::Shutdown { reason: "transition to B".into() })?;
    // Drain any final messages (we expect Bye).
    let _ = timeout(Duration::from_secs(2), current.inp.recv()).await;
    let exit = timeout(
        Duration::from_secs(SHUTDOWN_TIMEOUT_SECS),
        current.process.wait(),
    )
    .await
    .context("A failed to exit within timeout")??;
    println!("[launcher] A exited with code {}", exit.code().unwrap_or(-1));

    // ---- Stage 5: launch Instance B (hidden, at A's rect) ----
    println!("\n[launcher] === Stage 5: launch Instance B (hidden, at A's rect) ===");
    let mut next = launch_instance(InstanceLaunch {
        listener: &listener,
        auth_token: &auth_token,
        port,
        test_dir: &test_dir,
        name: "B",
        color: "red",
        rect: a_rect,
        hidden: true,
    })
    .await
    .context("launching Instance B")?;
    wait_for_ready(&mut next).await.context("waiting for B ready")?;

    // ---- Stage 6: reveal B ----
    println!("\n[launcher] === Stage 6: reveal B and fade out overlay ===");
    next.send(Msg::ShowWindow)?;
    // Brief settle so Swing's setVisible(true) actually paints before we
    // start fading the overlay. Without this, the user briefly sees the
    // empty desktop where the overlay used to be.
    sleep(Duration::from_millis(150)).await;
    overlay_cmd_tx.send(OverlayCmd::Hide)?;
    sleep(Duration::from_millis(FADE_DURATION_MS as u64 + 50)).await;

    println!("\n[launcher] === Spike #3: PASS — B is visible where A used to be ===\n");

    // ---- Stage 7: hold for visual confirmation, then teardown ----
    println!("[launcher] holding for {POST_TRANSITION_HOLD_SECS}s, then shutting down B...");
    sleep(Duration::from_secs(POST_TRANSITION_HOLD_SECS)).await;

    next.send(Msg::Shutdown { reason: "spike complete".into() })?;
    let _ = timeout(Duration::from_secs(2), next.inp.recv()).await;
    let _ = timeout(Duration::from_secs(SHUTDOWN_TIMEOUT_SECS), next.process.wait()).await;
    println!("[launcher] B exited.");

    // ---- Stage 8: tear down overlay thread ----
    let _ = overlay_cmd_tx.send(OverlayCmd::Quit);
    let _ = overlay_handle.join();
    println!("[launcher] all clean. exit.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Instance lifecycle
// ---------------------------------------------------------------------------

/// Owns one running test instance: its OS process plus the IPC channels
/// for talking to it.
struct Instance {
    name: String,
    process: Child,
    out: mpsc::UnboundedSender<Msg>,
    inp: mpsc::UnboundedReceiver<Msg>,
}

impl Instance {
    fn send(&self, msg: Msg) -> Result<()> {
        self.out.send(msg).map_err(|_| anyhow!("instance {} disconnected", self.name))?;
        Ok(())
    }
}

struct InstanceLaunch<'a> {
    listener: &'a TcpListener,
    auth_token: &'a str,
    port: u16,
    test_dir: &'a Path,
    name: &'a str,
    color: &'a str,
    rect: RECT,
    hidden: bool,
}

async fn launch_instance(opts: InstanceLaunch<'_>) -> Result<Instance> {
    // Build java command with -D properties. Mirrors what the production
    // launcher will do for real Minecraft mods.
    let mut cmd = Command::new("java");
    cmd.current_dir(opts.test_dir)
        .arg(format!("-Dliminal.connect.address=ws://127.0.0.1:{}", opts.port))
        .arg(format!("-Dliminal.auth.token={}", opts.auth_token))
        .arg(format!("-Dliminal.instance.name={}", opts.name))
        .arg(format!("-Dliminal.instance.color={}", opts.color))
        .arg(format!("-Dliminal.window.x={}", opts.rect.left))
        .arg(format!("-Dliminal.window.y={}", opts.rect.top))
        .arg(format!("-Dliminal.window.width={}", opts.rect.right - opts.rect.left))
        .arg(format!("-Dliminal.window.height={}", opts.rect.bottom - opts.rect.top));
    if opts.hidden {
        cmd.arg("-Dliminal.window.hidden=true");
    }
    cmd.arg("TestInstance")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let child = cmd.spawn().context("spawn java — is it on PATH?")?;
    let pid = child.id().ok_or_else(|| anyhow!("spawned child has no PID"))?;
    println!("[launcher] spawned {} as PID {}", opts.name, pid);

    // Wait for the child to dial back into our WebSocket and authenticate.
    let (out, inp) = accept_one_connection(opts.listener, opts.auth_token).await?;

    Ok(Instance {
        name: opts.name.to_string(),
        process: child,
        out,
        inp,
    })
}

async fn wait_for_ready(inst: &mut Instance) -> Result<()> {
    let msg = timeout(Duration::from_secs(READY_TIMEOUT_SECS), inst.inp.recv())
        .await
        .map_err(|_| anyhow!("{} did not send ready within {READY_TIMEOUT_SECS}s", inst.name))?
        .ok_or_else(|| anyhow!("{} disconnected before ready", inst.name))?;
    match msg {
        Msg::Ready { instance_name } => {
            println!("[launcher] {} reported ready (instance_name={instance_name:?})", inst.name);
            Ok(())
        }
        other => bail!("expected Ready from {}, got {other:?}", inst.name),
    }
}

// ---------------------------------------------------------------------------
// IPC server
// ---------------------------------------------------------------------------

async fn accept_one_connection(
    listener: &TcpListener,
    expected_token: &str,
) -> Result<(mpsc::UnboundedSender<Msg>, mpsc::UnboundedReceiver<Msg>)> {
    let (tcp, peer) = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), listener.accept())
        .await
        .map_err(|_| anyhow!("child did not connect within {CONNECT_TIMEOUT_SECS}s"))?
        .context("accept failed")?;
    println!("[launcher] connection from {peer}");

    let ws = accept_async(tcp).await.context("websocket handshake")?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Auth handshake — same shape as Spike #2.
    let auth = recv_msg(&mut ws_rx).await.context("recv auth")?;
    match auth {
        Msg::Auth { token, protocol_version } => {
            if token != expected_token {
                let _ = send_msg(&mut ws_tx, &Msg::AuthRejected { reason: "bad token".into() }).await;
                bail!("client sent wrong auth token");
            }
            if protocol_version != PROTOCOL_VERSION {
                let reason = format!(
                    "protocol_version mismatch (launcher={PROTOCOL_VERSION}, client={protocol_version})"
                );
                let _ = send_msg(&mut ws_tx, &Msg::AuthRejected { reason: reason.clone() }).await;
                bail!("{reason}");
            }
            send_msg(&mut ws_tx, &Msg::AuthOk).await?;
        }
        other => bail!("expected Auth, got {other:?}"),
    }

    // Bridge the websocket halves onto mpsc channels so the rest of the
    // program can talk to the instance without juggling stream/sink.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Msg>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Msg>();

    // Writer task: drain `out_rx` to the websocket.
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if send_msg(&mut ws_tx, &msg).await.is_err() {
                break;
            }
        }
    });

    // Reader task: pull from the websocket into `in_tx`.
    tokio::spawn(async move {
        loop {
            match recv_msg(&mut ws_rx).await {
                Ok(msg) => {
                    if in_tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok((out_tx, in_rx))
}

async fn send_msg<S>(tx: &mut S, msg: &Msg) -> Result<()>
where
    S: futures_util::Sink<WsMessage> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let json = serde_json::to_string(msg)?;
    tx.send(WsMessage::Text(json.into()))
        .await
        .map_err(|e| anyhow!("websocket send failed: {e}"))?;
    Ok(())
}

async fn recv_msg<S>(rx: &mut S) -> Result<Msg>
where
    S: futures_util::Stream<
            Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    let frame = timeout(Duration::from_secs(MESSAGE_TIMEOUT_SECS), rx.next())
        .await
        .map_err(|_| anyhow!("timed out waiting for next message"))?
        .ok_or_else(|| anyhow!("websocket closed unexpectedly"))?
        .context("websocket error")?;
    match frame {
        WsMessage::Text(s) => Ok(serde_json::from_str(&s)
            .with_context(|| format!("parse JSON: {s}"))?),
        WsMessage::Close(_) => bail!("websocket closed by peer mid-protocol"),
        other => bail!("expected text frame, got {other:?}"),
    }
}

fn generate_auth_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

unsafe fn get_window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_ok() {
        Some(rect)
    } else {
        None
    }
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

/// Find a top-level visible window whose title contains `needle`.
/// Returns the first match (enumeration is z-order, roughly).
fn find_window_by_title(needle: &str) -> Option<HWND> {
    unsafe {
        // Pass a tuple `(needle, &mut found_slot)` through LPARAM. The
        // tuple lives on the stack of this function — safe because
        // EnumWindows is synchronous.
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
    let needle = data.0;
    let out_ptr = data.1;

    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1); // continue
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len <= 0 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    if title.contains(needle) {
        *out_ptr = Some(hwnd);
        return BOOL(0); // stop
    }
    BOOL(1)
}

// ---------------------------------------------------------------------------
// Win32 overlay thread
// ---------------------------------------------------------------------------

fn run_overlay_thread(
    cmd_rx: std::sync::mpsc::Receiver<OverlayCmd>,
    hotkey_tx: mpsc::UnboundedSender<()>,
) -> Result<()> {
    unsafe { run_overlay_thread_inner(cmd_rx, hotkey_tx) }
}

unsafe fn run_overlay_thread_inner(
    cmd_rx: std::sync::mpsc::Receiver<OverlayCmd>,
    hotkey_tx: mpsc::UnboundedSender<()>,
) -> Result<()> {
    let h_instance = GetModuleHandleW(None)?;
    let class_name = w!("LiminalSpike3Overlay");

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

    // Initial position is irrelevant — we'll SetWindowPos on every ShowAt.
    let initial = primary_monitor_rect();

    let ex_style = WS_EX_LAYERED
        | WS_EX_TRANSPARENT
        | WS_EX_TOPMOST
        | WS_EX_TOOLWINDOW
        | WS_EX_NOACTIVATE;

    let hwnd = CreateWindowExW(
        ex_style,
        class_name,
        w!("Liminal Overlay"),
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

    // Stash the hotkey sender so the wndproc can reach it.
    *HOTKEY_SENDER.lock().unwrap() = Some(hotkey_tx);

    // Start fully transparent.
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA)?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    RegisterHotKey(
        hwnd,
        HOTKEY_ID_TRANSITION,
        MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
        'T' as u32,
    )?;

    // Drive the placeholder progress bar at ~60fps so the user sees motion
    // during the transition (real launcher will report actual progress).
    SetTimer(hwnd, TIMER_ID_PROGRESS, PROGRESS_TICK_MS, None);

    // Main loop: drain command channel, drain Win32 messages, sleep briefly.
    loop {
        // Drain any commands from main thread.
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
                Ok(OverlayCmd::Hide) => {
                    TARGET_ALPHA.store(0, Ordering::SeqCst);
                    SetTimer(hwnd, TIMER_ID_FADE, FADE_TICK_MS, None);
                }
                Ok(OverlayCmd::Quit) => {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ID_TRANSITION);
                    let _ = DestroyWindow(hwnd);
                    return Ok(());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Main thread is gone — exit cleanly.
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

        std::thread::sleep(Duration::from_millis(OVERLAY_LOOP_SLEEP_MS));
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
    let new_ms =
        (PROGRESS_MS.load(Ordering::SeqCst) + PROGRESS_TICK_MS as i32) % PROGRESS_CYCLE_MS;
    PROGRESS_MS.store(new_ms, Ordering::SeqCst);
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

    // Double-buffer (same as Spike #1) — composite to mem DC, BitBlt once.
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

    let progress = PROGRESS_MS.load(Ordering::SeqCst) as f32 / PROGRESS_CYCLE_MS as f32;
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

