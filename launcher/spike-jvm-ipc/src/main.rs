//! Liminal Launcher — Spike #2: invisible JVM launch + WebSocket IPC.
//!
//! # What this validates
//!
//! Whether the launcher's "spawn a Minecraft JVM as a child process and
//! exchange JSON messages with it over a localhost WebSocket" model
//! actually works end-to-end. Specifically:
//!
//!   - Can Rust spawn a Java child process and inherit its stdio?
//!   - Does the child reliably connect back to the launcher's WebSocket
//!     server using launcher-provided system properties?
//!   - Does an authenticated handshake (random per-launch token) succeed?
//!   - Can the launcher request shutdown via IPC and have the child exit
//!     cleanly with code 0?
//!   - When everything works, are there no orphan processes left behind?
//!
//! These are the orchestration primitives Spike #3 will combine with the
//! Spike #1 overlay to perform a real instance-to-instance transition.
//!
//! # Test client
//!
//! `test-client/TestClient.java` is a minimal Java program that pretends
//! to be a Minecraft mod's IPC client. It connects to the WebSocket URL
//! passed via `-Dliminal.connect.address`, authenticates with the token
//! from `-Dliminal.auth.token`, echoes a few messages, and exits cleanly
//! when it receives a `shutdown` message. See `README.md` for how to
//! compile it.
//!
//! # Protocol (this spike)
//!
//! Newline is irrelevant — these are WebSocket text frames. JSON shape:
//!
//!   client → server  `{"type":"auth","token":"...","protocol_version":1}`
//!   server → client  `{"type":"auth_ok"}` or `{"type":"auth_rejected","reason":"..."}`
//!   server → client  `{"type":"echo","payload":"..."}`
//!   client → server  `{"type":"echo_reply","payload":"..."}`
//!   server → client  `{"type":"shutdown","reason":"..."}`
//!   client → server  `{"type":"bye"}`   (then closes the socket and exits)
//!
//! Production protocol will be much richer (loading progress messages,
//! transition_request, first_frame_rendered, etc.) but the shape is
//! identical: tagged JSON, snake_case, protocol_version on the auth msg.
//!
//! # Usage
//!
//! 1. `cd test-client && javac TestClient.java`   (one-time, compiles the test client)
//! 2. `cargo run --release`                        (from this directory)
//!
//! The Rust side prints diagnostic logs prefixed `[launcher]`. The Java
//! child's logs (which we inherit on stdio) are prefixed `[client]`.
//! Final line says `Spike #2: PASS` on success, otherwise an error.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------- Protocol ----------

/// Bumped any time the wire format changes incompatibly. The Java client
/// echoes this back in its `auth` message; we reject mismatches.
const PROTOCOL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg {
    Auth { token: String, protocol_version: u32 },
    AuthOk,
    AuthRejected { reason: String },
    Echo { payload: String },
    EchoReply { payload: String },
    Shutdown { reason: String },
    Bye,
}

// ---------- Tunables ----------

/// How long we'll wait for the child to open its WebSocket connection.
/// Real Minecraft startup is much slower than this — the production
/// launcher will use a much higher number, plus a per-stage timeout.
const CONNECT_TIMEOUT_SECS: u64 = 15;

/// Per-message receive timeout once the connection is established. Our
/// test client should respond effectively instantly — anything longer
/// is a bug in the protocol or a hung child.
const MESSAGE_TIMEOUT_SECS: u64 = 5;

/// How long we'll wait for the child to exit after we send `shutdown`.
/// The production launcher will fall back to SIGKILL after this; for
/// the spike we just bail with an error.
const SHUTDOWN_TIMEOUT_SECS: u64 = 10;

/// How many echo round-trips to do as a smoke test of the message loop.
const ECHO_ROUND_TRIPS: usize = 3;

#[tokio::main]
async fn main() -> Result<()> {
    println!("[launcher] Liminal Spike #2 — JVM launch + WebSocket IPC");

    // ---- Step 1: bind a localhost WebSocket server on an ephemeral port
    //
    // We bind to 127.0.0.1 (loopback only) so nothing outside this machine
    // can reach the IPC socket. Combined with the per-launch auth token,
    // an attacker would need both local code execution AND the token to
    // impersonate the child.
    let listener = TcpListener::bind("127.0.0.1:0").await
        .context("failed to bind localhost listener")?;
    let local_addr = listener.local_addr()?;
    let port = local_addr.port();
    println!("[launcher] WebSocket server listening on ws://{}", local_addr);

    // ---- Step 2: generate a per-launch auth token
    //
    // 32 chars of base62 ≈ 190 bits of entropy. Massive overkill for a
    // single-process token that's never written to disk — but cheap.
    let auth_token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    println!("[launcher] Generated auth token: {}…", &auth_token[..8]);

    // ---- Step 3: locate the compiled TestClient
    let test_client_dir: PathBuf = std::env::current_dir()?.join("test-client");
    let test_class = test_client_dir.join("TestClient.class");
    if !test_class.exists() {
        bail!(
            "TestClient.class not found at {}.\n\
             Compile it first: cd test-client && javac TestClient.java",
            test_class.display()
        );
    }

    // ---- Step 4: spawn the Java child
    //
    // Pass the IPC address and auth token via -D system properties — the
    // production launcher will use the same mechanism to inject these
    // into the Minecraft JVM at launch.
    let mut child = Command::new("java")
        .current_dir(&test_client_dir)
        .arg(format!("-Dliminal.connect.address=ws://127.0.0.1:{}", port))
        .arg(format!("-Dliminal.auth.token={}", auth_token))
        .arg("TestClient")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true) // ensure no orphan if this process panics
        .spawn()
        .context("failed to spawn java — is `java` on PATH?")?;

    let child_pid = child.id().ok_or_else(|| anyhow!("spawned child has no PID"))?;
    println!("[launcher] Spawned Java child PID {}", child_pid);

    // ---- Step 5: accept the child's WebSocket connection (with timeout)
    let accept = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), listener.accept())
        .await
        .map_err(|_| anyhow!("child did not connect within {CONNECT_TIMEOUT_SECS}s"))?
        .context("accept failed")?;
    let (tcp_stream, peer_addr) = accept;
    println!("[launcher] Child connected from {}", peer_addr);

    let ws = accept_async(tcp_stream).await
        .context("WebSocket handshake failed")?;
    let (mut tx, mut rx) = ws.split();

    // ---- Step 6: validate the auth handshake
    let first = recv_msg(&mut rx).await.context("expected auth message")?;
    match first {
        Msg::Auth { token, protocol_version } => {
            if token != auth_token {
                send_msg(&mut tx, &Msg::AuthRejected { reason: "bad token".into() }).await.ok();
                bail!("child sent wrong auth token (impersonation attempt?)");
            }
            if protocol_version != PROTOCOL_VERSION {
                let reason = format!(
                    "protocol_version mismatch: launcher={PROTOCOL_VERSION}, client={protocol_version}"
                );
                send_msg(&mut tx, &Msg::AuthRejected { reason: reason.clone() }).await.ok();
                bail!("{reason}");
            }
            println!("[launcher] Auth OK (token + protocol_version match)");
            send_msg(&mut tx, &Msg::AuthOk).await?;
        }
        other => bail!("expected Auth message, got {other:?}"),
    }

    // ---- Step 7: echo round-trips (smoke test of the message loop)
    for i in 0..ECHO_ROUND_TRIPS {
        let payload = format!("ping-{i}");
        println!("[launcher] → echo({payload:?})");
        send_msg(&mut tx, &Msg::Echo { payload: payload.clone() }).await?;
        match recv_msg(&mut rx).await? {
            Msg::EchoReply { payload: got } if got == payload => {
                println!("[launcher] ← echo_reply({got:?}) ✓");
            }
            Msg::EchoReply { payload: got } => {
                bail!("echo mismatch: sent {payload:?}, got {got:?}");
            }
            other => bail!("expected EchoReply, got {other:?}"),
        }
    }

    // ---- Step 8: request shutdown
    println!("[launcher] → shutdown");
    send_msg(&mut tx, &Msg::Shutdown { reason: "spike test complete".into() }).await?;

    match recv_msg(&mut rx).await {
        Ok(Msg::Bye) => println!("[launcher] ← bye ✓"),
        Ok(other) => println!("[launcher] expected Bye, got {other:?} (will still wait for exit)"),
        Err(e) => println!("[launcher] no bye received ({e}) — child may have just dropped the socket"),
    }

    // Drop the writer half so the WS close frame flushes and the child
    // sees EOF on its side, in case it's blocked on a read.
    drop(tx);

    // ---- Step 9: wait for the child to exit
    let exit = timeout(Duration::from_secs(SHUTDOWN_TIMEOUT_SECS), child.wait()).await;
    match exit {
        Ok(Ok(status)) if status.success() => {
            println!("[launcher] Child exited cleanly (code {})", status.code().unwrap_or(0));
            println!("\n[launcher] Spike #2: PASS");
            Ok(())
        }
        Ok(Ok(status)) => {
            bail!("child exited with non-zero status: {status}")
        }
        Ok(Err(e)) => bail!("error waiting for child: {e}"),
        Err(_) => {
            println!("[launcher] Child did not exit within {SHUTDOWN_TIMEOUT_SECS}s — killing");
            child.kill().await.ok();
            bail!("child failed to shut down within timeout (would orphan in production)")
        }
    }
}

// ---------- WebSocket message helpers ----------

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
    S: futures_util::Stream<Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let frame = timeout(Duration::from_secs(MESSAGE_TIMEOUT_SECS), rx.next())
        .await
        .map_err(|_| anyhow!("timed out waiting for next message"))?
        .ok_or_else(|| anyhow!("websocket closed unexpectedly"))?
        .context("websocket error")?;
    match frame {
        WsMessage::Text(s) => Ok(serde_json::from_str(&s)
            .with_context(|| format!("parse JSON message: {s}"))?),
        WsMessage::Close(_) => bail!("websocket closed by peer mid-protocol"),
        other => bail!("expected text frame, got {other:?}"),
    }
}
