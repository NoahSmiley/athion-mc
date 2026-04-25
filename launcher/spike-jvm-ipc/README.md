# Spike #2 — JVM Launch + WebSocket IPC

Validates that the Liminal Launcher can spawn a Java child process, exchange JSON messages with it over a localhost WebSocket, and shut it down cleanly with no orphan.

> See [`../../CONTEXT.md`](../../CONTEXT.md) for why this matters and how it fits.

## What it does

1. Rust binds a localhost WebSocket server on an ephemeral port
2. Generates a random per-launch auth token
3. Spawns `java TestClient` with `-Dliminal.connect.address=ws://...` and `-Dliminal.auth.token=...`
4. Accepts the child's WebSocket connection
5. Validates the auth handshake (token + protocol version)
6. Sends a few `echo` messages and verifies the replies match
7. Sends `shutdown`, expects `bye`, waits for the child to exit cleanly

Final line is `Spike #2: PASS` on success, otherwise an `Error: ...` describing what failed.

## Why these specific things

This is the orchestration machinery the seamless transition needs. During a real `/survival` transition, the launcher will:

- Spawn the survival JVM as a child process (Step 3, validated here)
- Wait for it to connect to the launcher's IPC server (Step 4, validated here)
- Authenticate to prevent other localhost processes from impersonating it (Step 5)
- Receive loading-progress messages from the JVM and update the overlay (analogous to Step 6)
- Tell the hub JVM to shut down cleanly when it's time to swap (Step 7)
- Detect orphans / stuck JVMs and force-kill (the timeout path in Step 7)

This spike is a stripped-down rehearsal of all of that against a 200-line Java test client instead of a full Minecraft instance.

## Prerequisites

- Rust toolchain (already needed for Spike #1)
- A JDK with `java` and `javac` on `PATH` — Java 11 or newer (uses `java.net.http.WebSocket`, added in 11). [Microsoft OpenJDK 21](https://learn.microsoft.com/en-us/java/openjdk/download) is a clean choice for Windows.

## How to run

One-time, from this directory: compile the test client.

```sh
cd test-client
javac TestClient.java
cd ..
```

Then run the spike:

```sh
cargo run --release
```

You should see interleaved `[launcher]` and `[client]` log lines, ending in:

```
[launcher] Child exited cleanly (code 0)

[launcher] Spike #2: PASS
```

## Protocol

JSON over WebSocket text frames. Tagged via the `type` field, snake_case keys, `protocol_version` on the auth message. Same shape the production launcher will use:

| Direction       | `type`          | Other fields                          |
|-----------------|-----------------|---------------------------------------|
| client → server | `auth`          | `token: string`, `protocol_version: int` |
| server → client | `auth_ok`       | —                                     |
| server → client | `auth_rejected` | `reason: string`                      |
| server → client | `echo`          | `payload: string`                     |
| client → server | `echo_reply`    | `payload: string`                     |
| server → client | `shutdown`      | `reason: string`                      |
| client → server | `bye`           | —                                     |

## Test matrix

| # | Scenario                                  | Result | Notes |
|---|-------------------------------------------|--------|-------|
| 1 | Happy path                                | TBD    | Launcher launches client, full handshake, shutdown round-trip |
| 2 | Bad auth token (manually tampered)        | TBD    | Launcher should reject and the child should exit non-zero |
| 3 | Child dies before sending auth            | TBD    | Launcher should bail with "child did not connect" |
| 4 | Child ignores shutdown (infinite loop)    | TBD    | Launcher should hit `SHUTDOWN_TIMEOUT_SECS` and kill the child |
| 5 | Launcher dies mid-protocol                | TBD    | Child should detect WS close and exit cleanly (no orphan) |

Row 1 is what the spike does by default. Rows 2-5 require manual code tweaks; we'll do them iteratively as needed for confidence.

## Likely failure modes

- **`java not found on PATH`** — install a JDK and retry. The spike prints a clear error.
- **`TestClient.class not found`** — forgot the `javac` step. The spike prints a clear error pointing at the file.
- **Connect timeout** — child started but couldn't reach the WebSocket. Firewall? Wrong port? Look at `[client]` stderr.
- **Auth mismatch** — protocol version drift between Rust and Java; bump both `PROTOCOL_VERSION` constants in lockstep.
- **Child won't exit on shutdown** — Java's `WebSocket.Listener` callback didn't trigger countdown, or the latch is leaking. Investigate the `[client]` logs.

## Out of scope for this spike

- Real Minecraft mod integration (Milestone 2)
- Multiple concurrent IPC connections (production launcher only ever has one game running)
- TLS / encryption (loopback-only, plus per-launch token is sufficient for the local-process threat model)
- Reconnection / retry logic (transitions are short-lived; reconnect doesn't help)
- Loading-progress messages / per-stage weights (Milestone 6)

## After this spike

If this passes, proceed to Spike #3: combine the overlay (Spike #1) with the JVM-IPC machinery (this spike) to perform a real instance-to-instance transition. See [`../../CONTEXT.md`](../../CONTEXT.md) § "What to build next".
