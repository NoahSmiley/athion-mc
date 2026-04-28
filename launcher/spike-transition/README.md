# Spike #3 — Full Transition

Combines Spike #1 (Win32 transparent overlay) with Spike #2 (JVM child + WebSocket IPC) into a real instance-A → instance-B handoff. This is the full seamless-transition mechanic exercised against test JVMs in place of real Minecraft.

> See [`../../CONTEXT.md`](../../CONTEXT.md) for the broader context.

## What it does

1. Spawns the Win32 overlay window on a dedicated thread (Spike #1 code, generalized)
2. Starts a localhost WebSocket IPC server with a random per-launch auth token (Spike #2 code, generalized)
3. Launches **Instance A** — a 250-line Java/Swing program with a blue background and a big "A" label, positioned at a default rect, visible immediately
4. Waits for A to send `ready` over IPC
5. Waits for the user to press **Ctrl+Shift+T**
6. Runs the transition:
   - Snapshot A's current screen rect via `EnumWindows` + `GetWindowRect`
   - Show overlay at that rect, fade in (300ms)
   - Send `shutdown` to A via IPC, wait for clean exit (timeout 10s)
   - Launch **Instance B** — same Java program parameterized as red/"B", launched **hidden**, positioned at A's old rect
   - Wait for B's `ready` (B builds its JFrame in `setVisible(false)` mode and reports ready immediately)
   - Send `show_window` to B (B calls `setVisible(true)` on the EDT)
   - Brief settle, then fade overlay out (300ms)
7. Holds for 5 seconds so you can confirm B is visible where A used to be
8. Sends `shutdown` to B, waits for clean exit, prints `Spike #3: PASS`

## Prerequisites

- Rust toolchain (already needed)
- JDK 11+ with `java` and `javac` on `PATH` (already needed for Spike #2)

## How to run

One-time, from this directory: compile the test instance.

```sh
cd test-instance
javac TestInstance.java
cd ..
```

Then run the spike:

```sh
cargo run --release
```

You'll see a blue "A" window appear at roughly (240, 120) at 960×640. When you press **Ctrl+Shift+T**, the overlay should fade in over the A window, hold for a moment as the JVM swap happens, then fade out — revealing a red "B" window in the same place.

## What "pass" looks like

- Blue A window appears
- Press Ctrl+Shift+T
- Smooth fade-in of dirt-brown overlay covering the A window's rect (and only that rect, not the rest of the screen)
- Loading screen with the green progress bar holds for ~1-2 seconds
- Smooth fade-out
- Red B window is now where A used to be
- Final terminal line: `Spike #3: PASS`

The "no perceptible window-management churn" criterion: there should be **no taskbar flash, no other window popping forward, no brief glimpse of the desktop** between A disappearing and B appearing. The whole transition should look like a loading screen, not a process restart.

## Likely failure modes

- **Overlay covers the wrong rect** — `EnumWindows` may have grabbed a different window with "Liminal Test Instance" in its title. Check the `[launcher] A rect: ...` log.
- **B appears at the wrong position** — `liminal.window.x/y/width/height` properties not being read. Check the Java `[B] starting; ... bounds=(...)` log.
- **Brief desktop flash between A vanishing and B appearing** — settle delay too short. Increase the `sleep(150ms)` between `ShowWindow` and `Hide`.
- **Hotkey doesn't fire** — another app has Ctrl+Shift+T globally. The spike will print no "hotkey received" line. Pick a different hotkey via `'T' as u32` in the source.
- **B's window doesn't appear after `show_window`** — the `frame.setVisible(true)` call didn't reach the EDT, or the frame was disposed prematurely. Check the `[B] received show_window; revealing frame` log.
- **A doesn't shut down cleanly** — Java side stuck in some Swing event loop. The launcher will hit `SHUTDOWN_TIMEOUT_SECS` and bail.

## Why this spike matters

Per [`../../CONTEXT.md`](../../CONTEXT.md), the seamless transition is the headline feature of the entire launcher. This spike validates that:

- The mechanism actually works end-to-end on real Windows
- The user perceptually experiences "loading screen", not "process restart"
- The IPC primitives (Spike #2) are sufficient to drive the transition lifecycle
- The overlay primitives (Spike #1) hold up when the underlying window is being torn down and replaced

If this passes, all three spike foundations are proven and we move to Milestone 0 — real Tauri app skeleton, Microsoft OAuth, vanilla Minecraft launch. Every milestone after that is *building toward shipping*, not "are we sure this approach works."

## Out of scope for this spike

- Real Minecraft (the test instances are Swing programs)
- Mod loading, manifest sync, auth (production work)
- Loading-progress messages with stage weights (Milestone 6)
- Multi-monitor / DPI handling (Milestone 7-8)
- Failure recovery (e.g., what if B fails to start? — Milestone 6)
- Crossfade vs hard cut between A and B (currently A vanishes during the loading screen, B appears during it; for the spike that's acceptable)
