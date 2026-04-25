# Liminal Launcher

A custom Minecraft launcher with seamless instance transitions: switch between different game versions and mod loaders without the user perceiving a process restart.

See [`CONTEXT.md`](CONTEXT.md) for architecture, scope, milestones, and current state. Read it before making changes.

## Status

Pre-Milestone-0. Spike phase.

| Spike | Goal | State |
|---|---|---|
| #1 | Transparent overlay window (Win32 layered, click-through, no focus steal) | Written; runtime test pending — see [`launcher/spike-overlay/README.md`](launcher/spike-overlay/README.md) |
| #2 | Invisible JVM launch + IPC handshake | Not started |
| #3 | Full transition end-to-end (kill A, hidden launch B, swap windows) | Not started |

Milestone 0 begins after all three spikes validate.

## Layout

```
liminal-launcher/
├── launcher/              # Rust + Tauri desktop app (the main work)
│   └── spike-overlay/     # Spike #1 — standalone Cargo binary
├── mod-shared/            # Common Java code (IPC client, protocol types) — TBD
├── mod-hub/               # NeoForge mod for hub instance — TBD
├── mod-survival/          # Fabric mod for survival instance — TBD
├── server-plugin/         # Velocity plugin for transition triggers — TBD
└── infra/                 # Manifest server, optional auth proxy — TBD
```

Subdirectories without code yet are documented in `CONTEXT.md`; they'll be created when their milestones begin.

## Platform

Windows-first, Windows-only for v1. macOS and Linux are explicitly out of scope.

## Quick start (overlay spike)

```sh
cd launcher/spike-overlay
cargo run --release
```

Then press **Ctrl+Shift+L** to toggle the overlay. See the spike's README for the test matrix to walk through.
