# Spike #1 — Transparent Overlay

Validates the Win32 windowing primitives the Liminal Launcher needs to draw a loading screen over a running Minecraft instance during a transition.

> See [`../../CONTEXT.md`](../../CONTEXT.md) for why this matters and how it fits into the broader project.

## What it does

Creates a borderless, click-through, always-on-top, no-taskbar window covering the primary monitor. The window draws a dark background and a cycling progress bar — placeholders, not the real visual design.

Press **Ctrl+Shift+L** to toggle. Fades in and out over 300ms.

This is a windowing test, not a visual-design test. The dirt color and the progress bar are placeholders. We're trying to answer a narrow question: *does the Win32 layered-window approach work in every Minecraft display mode?*

## How to run

Requires a Rust toolchain (rustup default, MSVC target). From this directory:

```sh
cargo run --release
```

Press Ctrl+Shift+L to toggle. Kill the spike from its terminal (Ctrl+C) when done. The hotkey is global — it works even when Minecraft has focus.

## Test matrix

For each row, launch any Minecraft client, put it in the listed mode, then start the spike alongside and toggle the overlay. Record the result.

| # | Minecraft mode             | Monitor setup                  | Result   | Notes |
|---|----------------------------|--------------------------------|----------|-------|
| 1 | Windowed                   | Single                         | **PASS** | Click-through OK, no focus steal, accurate per-window targeting, no flicker, smooth fade. Diagnostic `snap:` log confirms correct rect. Tested on Windows 11. |
| 2 | Borderless fullscreen      | Single                         | TBD      | Not directly tested. Modern Minecraft's "Fullscreen ON" produces borderless on Windows; expected to behave like row 1 with the rect being the full monitor. Will verify in Spike #3 against an actively-loading instance. |
| 3 | Exclusive fullscreen       | Single                         | **FAIL** | Overlay does not appear at all when toggled. Cause: Windows disables the DWM compositor for fullscreen-optimized apps, preventing layered windows from being drawn over them. Mitigation: launcher will require borderless. |
| 4 | Windowed                   | Dual                           | TBD      | |
| 5 | Borderless fullscreen      | Dual, on primary monitor       | TBD      | |
| 6 | Borderless fullscreen      | Dual, on secondary monitor     | TBD      | Spike covers primary monitor's rect by default; per-Minecraft-window targeting should follow it to the secondary, but overlay window is still owned by the spike's primary-monitor screen — likely needs Spike #3 to handle properly. |
| 7 | Exclusive fullscreen       | Dual, on primary monitor       | TBD      | Expected to fail same as row 3. |
| 8 | Windowed, 125% / 150% DPI  | Single                         | TBD      | Spike does not declare DPI awareness — likely renders blurry/scaled. Production launcher will set `SetProcessDpiAwarenessContext`. |

For each scenario, observe and record:

- **Visibility** — does the overlay appear above Minecraft when toggled on?
- **Click-through** — can you keep playing Minecraft while the overlay is up (mouse + keyboard reaching the game)?
- **Focus** — does the overlay take focus from Minecraft? Does Minecraft pause / lose mouse capture / show its pause menu?
- **Taskbar** — does the overlay appear in the taskbar or Alt-Tab? (It shouldn't.)
- **Fade** — is the alpha animation smooth or steppy?
- **Hide** — when toggled off, is the overlay fully gone (no ghost frame, no leftover dark band)?

## What "pass" looks like

Ideal outcome for every row:

- Overlay appears smoothly above the game
- Game keeps receiving input (mouse and keyboard pass through)
- Game does not lose focus (no pause menu in single-player; no input-capture loss)
- No taskbar entry, no Alt-Tab entry
- Smooth fade in and out
- Toggling off cleanly removes the overlay

## Likely failure modes (and what each implies)

- **Exclusive fullscreen blocks layered windows.** Most likely failure. Workaround: launcher requires Minecraft to run in borderless mode. This is acceptable for the project — most servers and players already prefer borderless for alt-tabbing.
- **Overlay covers the wrong monitor in multi-monitor setups.** Expected — this spike hardcodes the primary monitor. Production code will track the game window's monitor.
- **DPI scaling distorts the overlay.** Likely. Production code needs `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` and a manifest declaration.
- **Fade is choppy.** `SetLayeredWindowAttributes` triggers a redraw of the window content underneath, which can be slow on some GPUs. Workaround if this happens: switch to `UpdateLayeredWindow` with a pre-rendered DIB section.
- **Game pauses / loses input when overlay appears.** Means `WS_EX_NOACTIVATE` isn't sufficient on this system, or Minecraft is using `WM_ACTIVATEAPP` to detect focus loss differently than expected. Investigate which event Minecraft is reacting to.

## Out of scope for this spike

- Tracking the Minecraft window's screen rect (Spike #3)
- Rendering the actual loading-screen UI (Milestone 6)
- Per-pixel alpha (per-window alpha is sufficient — the loading screen is fully opaque during the transition; only the trailing fade-out uses partial alpha, and per-window covers that)
- Multi-monitor / monitor change handling (Milestones 7-8)
- Keyboard input *to* the overlay (it's intentionally `NOACTIVATE` — no keyboard for it ever)

## After this spike

If validation passes (or we identify the constrained subset of modes that work and we're OK requiring users to be in those modes): proceed to Spike #2 (invisible JVM launch + IPC handshake). See [`../../CONTEXT.md`](../../CONTEXT.md) § "What to build next".
