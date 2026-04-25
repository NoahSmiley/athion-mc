# Liminal Launcher — Project Handoff

> Read this first. It's the single source of truth for project state, architecture decisions, and what to build next.

## What this project is

A custom server-branded Minecraft launcher that handles authentication, mod sync, and **seamless transitions between different Minecraft instances** (different versions, different mod loaders) running against different backend servers — without the user perceiving a process restart.

Concretely: user is in a NeoForge 1.21.1 hub, types `/survival`, sees a loading screen for ~12-15 seconds, ends up in a Fabric 1.21.11 survival world. The transition feels like an in-game loading screen, not a launcher restart.

## Why this is interesting

Traditional Minecraft launchers (Prism, MultiMC, vanilla launcher) are general-purpose: they support arbitrary versions, arbitrary mod sets, arbitrary auth providers. This launcher is the opposite — purpose-built for one specific server network, with full control over mods, instances, and the user experience.

That control unlocks the seamless-transition feature, which no general-purpose launcher can provide. It also dramatically simplifies the scope (no modpack browser, no version picker, no "import CurseForge zip" UI).

## Goals and constraints

**Goal:** Ship for real users on a real server.

**Platform:** Windows-first. Mac/Linux are out of scope for v1.

**Stack:** Tauri 2 + Rust for the launcher. Web frontend for UI. Native Win32 (via `windows-rs`) for window management — Tauri's window abstraction is not sufficient for the overlay/positioning work this project requires.

**Scope discipline:** All v1 features are required (full scope, take the time needed). But scope is bounded — features outside the explicit scope below should be rejected.

**Critical constraint:** Only one Minecraft JVM runs at a time. No pre-warmed instances. The transition cold-launches the new instance during the loading screen. This trade is intentional — pre-warming costs ~3 GB RAM and ~500 MB VRAM continuously, which is unacceptable for real users on real hardware.

## Architecture overview

Five components, monorepo layout:

```
liminal-launcher/
├── launcher/              # Rust + Tauri desktop app (this is the main work)
├── mod-shared/            # Common Java code (IPC client, protocol types)
├── mod-hub/               # NeoForge mod for the hub instance
├── mod-survival/          # Fabric mod for the survival instance
├── server-plugin/         # Velocity plugin for transition triggers
└── infra/
    ├── manifest-server/   # Hosts instance manifests + mod files
    └── auth-proxy/        # Optional: server-side auth verification
```

The launcher is the conductor. Each Minecraft instance is a child process with a narrow IPC contract. All orchestration decisions happen in Rust.

### How a transition works

1. User in hub types `/survival`
2. Velocity backend sends `liminal:transition` plugin message to client
3. Hub mod converts plugin message to IPC `transition_request`
4. Launcher: read hub window's screen rect via Win32
5. Launcher: show transparent always-on-top overlay window over hub's rect, drawing dirt-background loading screen with progress bar
6. Launcher: send `shutdown` to hub via IPC
7. Hub mod cleanly disconnects, exits process
8. Launcher waits for hub PID to exit (with timeout)
9. Launcher launches survival JVM with window initially hidden, positioned at hub's old rect
10. Survival mod connects to launcher IPC, reports loading progress messages
11. Launcher updates progress bar as messages arrive
12. Survival mod signals `first_frame_rendered` after world loads
13. Launcher: show survival window, transfer focus via `AttachThreadInput` + `SetForegroundWindow`
14. Launcher: fade overlay alpha 1.0 → 0.0 over 300ms, then hide overlay window

The user's perception: gameplay → loading screen → gameplay. No window flicker, no taskbar churn, no focus loss.

### Key architectural decisions and their reasoning

**Single launcher, multiple game children.** Launcher lifetime > all instances. If launcher crashes, instances detect lost IPC connection and exit cleanly. Don't try to make the launcher optional or removable — it's the orchestrator.

**Localhost WebSocket for IPC.** JSON messages, newline-delimited or proper WS frames. Auth token passed via system property to prevent other localhost processes from talking to the launcher. Why WebSocket over plain TCP: bidirectional, framed, broadly supported in both Rust (`tokio-tungstenite`) and Java (`java-websocket` or similar).

**Auth tokens stored in Windows Credential Manager.** Use the `keyring` crate. Refresh tokens persist across launcher sessions; access tokens are ephemeral. Never write tokens to disk in plain text.

**Content-addressed mod storage.** Mods stored by SHA-256 hash. Instances reference mods by hash. Mod shared between hub and survival is downloaded once. Switching to a new manifest version doesn't redownload unchanged files.

**Manifests are signed.** Signed JSON describes each instance: game version, loader version, mods list (hash + URL + load order), JVM args, asset index. Launcher verifies signature before trusting. This protects users from supply-chain attacks if the CDN is ever compromised.

**Honest progress bars require weighted stages.** Each launch stage has a weight (its expected fraction of total time). Within-stage progress is reported where available (per-mod loading, per-asset loading). Weights are learned from rolling averages of recent launches on this user's machine. The bar caps at 99% until `first_frame_rendered` arrives, then snaps to 100% as the overlay fades.

**Native Win32 for the overlay, not Tauri.** Tauri 2 has known issues with transparent windows on Windows (per-pixel alpha gets ignored in some configurations). The overlay window is created directly via `windows-rs` with `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`. Tauri is used for the main launcher UI (login screen, play button) only.

**Cold launch performance is critical.** Target: median cold launch under 12 seconds, P95 under 18 seconds. Achievable but requires JVM Class Data Sharing (AppCDS), shader cache persistence, file system page cache pre-warming, and audited mod load order. This is real engineering work, not a free lunch.

## v1 feature scope

### Launcher (Tauri + Rust)

- Microsoft OAuth flow with refresh token persistence (Credential Manager)
- Manifest fetching from CDN with signature verification
- Instance file sync: parallel downloads, hash verification, atomic file replacement
- JVM spawning with full classpath construction (vanilla, NeoForge, Fabric)
- IPC server with auth tokens
- Transition orchestrator (state machine for handoffs)
- Win32 window manipulation (rect tracking, focus transfer, show/hide)
- Transparent always-on-top overlay window
- Loading screen with weighted progress bar, stage labels, detail text, rotating tips
- Self-update mechanism

### Hub mod (NeoForge 1.21.1)

- IPC client connecting to launcher on startup
- Server plugin message listener (`liminal:transition` channel)
- Window position/size reporter to launcher
- Clean shutdown handler

### Survival mod (Fabric 1.21.11)

- IPC client (shared code with hub mod)
- Window hidden on startup if `liminal.window.hidden=true` system property set
- Window positioning via system properties (`liminal.window.x`, `.y`, `.width`, `.height`)
- Render-loop hook to detect first frame and emit `first_frame_rendered`
- Per-mod loading progress reporting (hooks into Fabric Loader entrypoints)
- Connect-on-launch via system property (`liminal.connect.address`)

### Server plugin (Velocity)

- Command handlers: `/survival`, `/hub`, `/lobby`
- Plugin message channel for transition commands
- Per-player auth token validation

### Manifest server

- Static file server with proper cache headers
- Signed JSON manifests per instance
- Mod files served from same CDN
- Versioned: rolling back is supported

## Milestone breakdown

### Milestone 0: Foundations (1-2 weeks)

- Tauri project skeleton, Rust workspace
- Microsoft OAuth flow end-to-end
- Vanilla Minecraft launch from launcher (no mods, no IPC)
- Stub manifest server (S3 bucket or static file host)
- **Done when:** Click Play, log in, vanilla Minecraft launches.

### Milestone 1: Mod sync + modded launch

- Manifest schema finalized (versioned, signed)
- Asset/library/mod download with parallel streams + hashing
- File store layout (content-addressed)
- NeoForge launch (modded classpath construction)
- Fabric launch
- **Done when:** Launcher can sync and launch both hub and survival instances.

### Milestone 2: IPC layer

- WebSocket server in launcher with auth token validation
- Java IPC client library (used by both mods)
- Mod skeletons that connect and send `ready`
- Loading progress messages flowing through to launcher
- Graceful disconnect handling
- **Done when:** Both instances launch, connect to launcher, exchange messages.

### Milestone 3: Transition trigger plumbing

- Velocity plugin sending `liminal:transition` plugin messages
- Hub mod converting plugin messages to IPC requests
- Launcher receiving requests, doing naive transition (kill old, launch new — visible flicker is fine at this stage)
- **Done when:** `/survival` in hub leads to survival instance loading, even if ugly.

### Milestone 4: The transition system itself (the hill)

- Win32 windowing utilities (rect tracking, focus transfer)
- Transparent overlay window (native Win32, not Tauri)
- Loading screen rendering
- Window show/hide orchestration with proper sequencing
- Fade timing tuning
- Crossfade with no visible flicker
- **Done when:** Cold-launch transition has no visible flicker, no taskbar churn, no focus loss.

### Milestone 5: Cold launch optimization

- JVM Class Data Sharing (AppCDS archive per instance)
- Mod load order audit
- Shader cache persistence
- File system pre-warming during hub session
- Asset prefetching
- **Done when:** Median cold launch under 12 seconds.

### Milestone 6: Loading screen as a feature

- Stage-aware progress display with weighted timing
- Tips/lore/news system (server-fetched, locally cached)
- Animations and visual polish (matching Minecraft aesthetic)
- Error recovery UI (cancellation, retry, fallback to hub)
- **Done when:** Loading screen feels native to Minecraft, not like a launcher.

### Milestones 7-8: Production hardening

- Multi-monitor handling
- DPI scaling
- Display change handling (monitor unplugged mid-session)
- Antivirus compatibility (code-signing the binary)
- Crash reporting (Sentry or similar)
- Auto-update for launcher binary
- User-facing error messages for every failure mode
- Code-signing certificate (~$200/year for production trust)
- Installer (MSI or NSIS)
- Manifest CDN with proper caching, signing, version pinning
- Mod tampering detection
- Documentation + operational runbook

## Current state

### Completed

- Architecture designed
- Tech stack chosen (Tauri 2 + Rust + native Win32 for overlay; Java for mods; Velocity for server plugin)
- Project skeleton bootstrapped (this directory)
- Spike #1 written and validated: transparent always-on-top overlay with hotkey toggle, 300ms alpha fade, per-Minecraft-window targeting (EnumWindows by title + SetWindowPos), double-buffered paint. Validated against windowed Minecraft on Windows 11: overlay snaps to the game's rect, click-through works, no focus steal, no taskbar entry, no flicker, smooth fade. Exclusive-fullscreen-style mode confirmed broken (overlay does not appear) — launcher will require borderless mode in production. See "Constraints discovered during spike phase" below.

### In progress

- Spike #2 (invisible JVM launch + IPC handshake)

### Not started

- Spike #3 (full transition: kill instance A, launch hidden instance B, swap windows)
- Everything from Milestone 0 onward

### Constraints discovered during spike phase

- **Players must run Minecraft in windowed or borderless fullscreen.** True exclusive fullscreen disables Windows' DWM compositor for the foreground app, which prevents our `WS_EX_LAYERED | WS_EX_TOPMOST` overlay from being composited over the game. Confirmed via Spike #1 on Windows 11. The launcher will detect exclusive fullscreen at transition time and either (a) refuse to transition with a user-facing explanation, or (b) coerce the game to borderless before transitioning. Most server-oriented players already prefer borderless for alt-tabbing, so this isn't a meaningful UX cost. Borderless fullscreen behavior not yet directly tested in Spike #1 — will verify in Spike #3 when we test against an actively-loading instance.

## What to build next

**Immediate next step:** Run the overlay spike at `launcher/spike-overlay/`. Follow the README's test matrix. Document which scenarios pass and which fail. The pattern of failures determines which production fixes are needed (for instance, "exclusive fullscreen fails but borderless works" → the launcher requires borderless mode).

**After the overlay spike validates (or we adapt around its failures):**

Build Spike #2: invisible JVM launch + IPC.

- Rust program that spawns `java -jar <test-jar>` as a child process
- Rust program runs WebSocket server on localhost
- Pass IPC port + auth token via system properties
- Java side: tiny program that connects to WebSocket, handshakes, sends test messages, listens for shutdown command, exits cleanly
- Validates: child process management, IPC handshake, clean shutdown, no orphan processes
- This spike is cross-platform — can be developed on Mac if needed, then validated on Windows

**After Spike #2:**

Build Spike #3: full transition mechanics.

- Combine spikes #1 and #2
- Two pre-built Minecraft instances (vanilla 1.21.1 Fabric for both, simplest possible)
- Single test mod that works in both
- Hotkey trigger to start transition
- Full sequence: overlay shown, instance A killed, instance B launched hidden, instance B revealed, focus transferred, overlay faded
- Validates: the entire seamless transition mechanic end-to-end
- Same Minecraft version on both sides for the spike — cross-version comes later because version differences don't change the windowing problem

**Only after all three spikes validate:** start Milestone 0 (proper foundations).

## Open questions to resolve before milestone 1

These need answers before code that depends on them can be written:

1. **Server hostnames.** What addresses will hub and survival actually live at? Direct DNS (`hub.liminal.gg`, `survival.liminal.gg`)? Behind Cloudflare? Velocity proxy on a single port routing internally?

2. **Microsoft Azure tenant.** Need to register an Azure application to get an OAuth client ID. Free, ~20 min, requires Microsoft account. Has this been done?

3. **CDN for mod hosting.** Cloudflare R2? AWS S3? Self-hosted on the homelab? Affects manifest URL structure, signing strategy, cache behavior.

4. **Manifest signing strategy.** Ed25519 keypair? RSA? Where does the private key live? How are public keys embedded in the launcher binary (and rotated if needed)?

5. **Backend for tips/news in loading screen.** Static file refreshed periodically? Dynamic API? Local-only with launcher updates pushing new tips?

6. **Telemetry strategy.** Crash reporting yes (Sentry default). Usage telemetry — opt-in or none? GDPR considerations if EU users will be served.

## Decisions explicitly deferred

These are *not* in scope for the foreseeable future:

- macOS support
- Linux support
- Multi-server federation (one launcher serving multiple unrelated server networks)
- In-launcher chat / social features
- Modpack browser / mod marketplace
- Custom Minecraft proxy / network code
- Voice chat integration
- Streaming / OBS integration

If a feature request lands here, default answer is no unless it advances the v1 goal.

## Key files

- `launcher/spike-overlay/Cargo.toml` — dependencies for overlay spike
- `launcher/spike-overlay/src/main.rs` — overlay spike implementation, heavily commented
- `launcher/spike-overlay/README.md` — test matrix and how to run

## Coding conventions

- **Rust:** standard rustfmt, clippy clean. Comments explain *why*, not *what*. Module-level docs explain the role of each module.
- **Java:** standard conventions for the Minecraft mod ecosystem. Mixins prefixed `Mixin*`. Mod IDs `liminal_hub`, `liminal_survival`, `liminal_shared`.
- **JSON IPC messages:** snake_case keys, type tag in `"type"` field, protocol version in `"protocol_version"` field on every message.
- **Logging:** structured logs (JSON or key=value), separate launcher and game-side logs, correlation IDs for transitions so you can trace a transition end-to-end across processes.

## How to use this document with Claude Code

When starting a Claude Code session in this project's directory, prompt it with something like:

> Read CONTEXT.md to understand this project's state, architecture, and constraints. Then [specific task].

The doc captures the *why* of decisions, which lets Claude Code make consistent choices on related questions without needing to re-derive them. Update this doc whenever an architectural decision changes — it's the project's memory.

## Outstanding clarifications

This doc reflects decisions made in the planning conversation that produced it. Anything not explicitly decided here is open. When in doubt, the decisions in this document take precedence over assumptions or general best practices, because the decisions here account for project-specific constraints that general advice doesn't know about.

## Note on the name

The name "Liminal Launcher" coexists with an unrelated project at `~/Desktop/liminal/` — a Tauri-based AI terminal IDE that happens to share the "liminal" name. They have nothing to do with each other. To avoid confusion, this project's directory is `liminal-launcher/`, not `liminal/`.
