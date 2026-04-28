# Athion Client — Project Handoff

> Read this first. Single source of truth for what we're building, why, what's done, and what's next.

## What this project is

**Athion's custom Minecraft client.** A closed, end-to-end branded experience for the Athion server network. Players install the Athion app and exist *inside* Athion — never seeing vanilla Minecraft chrome, never connecting to non-Athion servers, never managing instances or modpacks themselves.

The client wraps Java Minecraft as a rendering engine. Above that we own:

- A custom **launcher** (Tauri + Rust) that handles login, server selection, instance lifecycle, and the seamless-transition orchestration.
- **Custom mods** on every Athion instance that strip and replace vanilla menus — no Singleplayer button, no Multiplayer button, no third-party server access. Disconnect routes back to *our* launcher.
- A WebSocket **IPC bridge** between launcher and the Minecraft client for in-game orchestration (transitions, loading progress, lifecycle).
- Server-side **Velocity + Spigot plugins** that turn in-game events (commands, portal walks) into IPC trigger messages.

**Headline gameplay feature: seamless transitions.** Player types `/survival` in the hub server, sees a loading screen for ~12 seconds, lands in the survival world — possibly running a different Minecraft version and mod loader (e.g., NeoForge 1.21.1 hub → Fabric 1.21.11 survival). The transition feels like an in-game loading screen, not a launcher restart. Validated end-to-end against real heterogeneous instances; see "Current state" below.

## Why an end-to-end custom client

Traditional Minecraft launchers (vanilla, Prism, MultiMC, CurseForge App) are general-purpose. They support arbitrary versions, arbitrary mod sets, arbitrary auth providers, arbitrary servers. The Athion product is the opposite: **one network, one brand, one curated experience.**

Owning the full stack unlocks things general-purpose launchers can't:

- Seamless cross-loader transitions (the "/survival in hub" UX) — the headline feature, only possible with full control of mod + launcher + window management.
- Branded UI top to bottom — no "Mojang" splash, no "powered by Java Edition" signaling, no third-party launcher chrome. The user sees Athion, period.
- Pre-curated, server-managed mod sets — no "import a modpack" friction, no version-mismatch errors, no incompatible-mod debugging by the user.
- Forced lockdown to Athion-only servers — players can't accidentally hop to other networks from inside the app.
- Single source of truth for what's installed — manifest-driven from Athion's CDN, no user instance management or mod debugging.

The trade is a closed ecosystem: someone wanting to play vanilla Minecraft, or play on other servers, needs the regular Minecraft launcher (which they can install separately). The Athion app is *for Athion*.

## Goals and constraints

**Goal:** Ship a polished end-user product for Athion's player base.

**Platform:** Windows-first. macOS/Linux out of scope for v1.

**Stack:**

- **Launcher:** Tauri 2 + Rust + web frontend (HTML/CSS/JS, framework TBD).
- **Win32 layer:** `windows-rs` directly for overlay window + window targeting + DPI handling — Tauri's window abstraction isn't sufficient. Validated in Spike #1.
- **Mods:** NeoForge for hub-style instances (1.21.x), Fabric for performance-oriented instances (1.21.x). Shared Java IPC client library across both loaders. Built with each loader's standard MDK.
- **Server side:** Velocity proxy plugin for server-driven triggers + backend Spigot/Paper plugins on each Minecraft server for portal-walk events and similar.
- **Auth:** Microsoft OAuth (mandatory for Minecraft) bridged through Athion's account system.

**Scope discipline:** All v1 features listed below are required. Anything *not* listed is rejected by default. The point is a polished narrow product, not a flexible one.

**Critical constraints:**

- **One Minecraft JVM at a time.** No pre-warmed instances. (Pre-warming costs ~3GB RAM + ~500MB VRAM continuously — unacceptable for real users.) Transitions cold-launch the new instance during the loading-screen overlay.
- **Borderless or windowed Minecraft only.** True exclusive fullscreen disables Windows' DWM compositor for the foreground app, which prevents our `WS_EX_LAYERED | WS_EX_TOPMOST` overlay from being composited. Confirmed broken in Spike #1. The launcher will coerce or refuse exclusive fullscreen at transition time. Acceptable trade — most server players prefer borderless anyway for alt-tabbing.
- **Cold launch must be fast.** Target: median <12s, P95 <18s. Real engineering work — JVM Class Data Sharing, mod-load-order audit, asset pre-warming, shader cache. Not a free lunch.

## Architecture overview

### Component layout (monorepo)

```
liminal-launcher/                        # repo root (codename, see "Naming" below)
├── launcher/                            # Tauri + Rust desktop app
│   ├── spike-overlay/                   # Spike #1 — Win32 overlay, validated
│   ├── spike-jvm-ipc/                   # Spike #2 — JVM child + WebSocket IPC, validated
│   ├── spike-transition/                # Spike #3 — full transition w/ test JVMs, validated
│   └── spike-mc-transition/             # Spike #4 — full transition w/ real Minecraft + IPC server, validated
├── mod-hub/                             # NeoForge mod for hub-style instances
│   └── (Liminal hub mod — IPC client + F9 debug trigger; menu lockdown TBD)
├── mod-survival/                        # Fabric mod for survival-style instances — TBD
├── mod-shared/                          # Shared Java IPC + protocol code — TBD
├── server-plugin/                       # Velocity plugin for server-driven triggers — TBD
└── infra/                               # Manifest server, auth proxy, etc. — TBD
```

### How a player's session flows (target product UX)

1. Player launches the Athion app
2. Login (Athion account; first run binds the player's Microsoft Minecraft account)
3. **Server picker** — list of Athion servers (Hub, Survival, Lobby, events, ...) with live status, ping, player count
4. Click a server → launcher boots Minecraft directly into that server (no main menu, no server-list screen)
5. In-game: the Athion mod has stripped vanilla menus. Singleplayer button gone. Multiplayer button gone. ESC menu's "Disconnect" returns to *the Athion launcher*, not Mojang's main menu
6. Server-driven transitions: player types `/survival`, the server sends a `liminal:transition` plugin message to the client, mod forwards via IPC, launcher kills the hub JVM and cold-launches the survival JVM (with `--server` pointed at the survival backend), overlay covers the swap, player lands in survival

### How a transition works (validated end-to-end against real Minecraft)

1. Player types `/survival` (eventual) — *currently:* presses F9 in-game (validated)
2. Velocity sends `liminal:transition` plugin message — *eventual* — *currently:* mod's F9 handler triggers directly
3. Mod forwards to launcher as `transition_request` over IPC WebSocket ← validated
4. Launcher reads hub window's screen rect via Win32 `EnumWindows` + `GetWindowRect` ← validated
5. Launcher shows transparent always-on-top overlay window over hub's rect, fades in 300ms ← validated
6. Launcher sends `kill A` (production: clean `shutdown` over IPC; spike: `taskkill /F /T`) ← spike-grade validated
7. Launcher cold-launches survival JVM with `--server <addr>` so it auto-connects to survival ← validated
8. Launcher tracks survival window position during loading (re-snaps overlay every 250ms — GLFW reposition is a real thing) ← validated
9. Once survival's world is rendered, overlay fades out 300ms ← validated, currently driven by fixed 20s post-window-load delay (TBD: replace with `world_loaded` IPC signal)
10. Player perceives: gameplay → loading screen → gameplay. No taskbar churn, no flicker, no focus loss, no main menu visit ← validated

### Key architectural decisions and their reasoning

**Single launcher, multiple game children.** Launcher lifetime > all instances. If launcher crashes, instances detect lost IPC and exit cleanly. Launcher is not optional — it's the orchestrator.

**Localhost WebSocket for IPC.** JSON tagged messages, snake_case, `protocol_version` on auth. Auth via random per-launch token. WebSocket because it's bidirectional, framed, and well-supported in both Rust (`tungstenite`) and Java (`java.net.http.WebSocket`, no extra dependency).

**Discovery: mod reads `%APPDATA%\Liminal\ipc.json`.** Launcher writes URL + token to this file on startup. Mod reads it on Minecraft startup. Workaround for the fact that we currently use Prism for development and can't inject system properties through it — when we own the launch (Milestone 1+), we'll switch to `-Dliminal.connect.address` / `-Dliminal.auth.token`.

**Auth tokens stored in Windows Credential Manager.** Use the `keyring` crate. Refresh tokens persist across sessions; access tokens are ephemeral. Never plaintext on disk.

**Content-addressed mod storage.** Mods stored by SHA-256 hash. Instances reference mods by hash. Shared mods download once. Manifest version bumps don't redownload unchanged files.

**Manifests are signed.** Signed JSON describes each instance: game version, loader version, mod list (hash + URL + load order), JVM args, asset index. Launcher verifies signature before trusting. Protects against CDN compromise.

**Honest progress bars require weighted stages.** Each launch stage has a weight; within-stage progress reported where available (per-mod loading, per-asset loading). Weights learned from rolling averages of the user's recent launches. Bar caps at 99% until `first_frame_rendered` (or for now, a fixed delay), then snaps to 100% as the overlay fades.

**Native Win32 for the overlay, not Tauri.** Tauri 2 has known issues with transparent windows on Windows. Overlay is created directly via `windows-rs` with `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`. Tauri is for the main launcher UI (login, server picker) only.

**Cold launch performance is critical.** Median <12s, P95 <18s targets. AppCDS (Class Data Sharing), audited mod load order, shader cache persistence, file system page cache pre-warming, asset prefetching. Real engineering, multiple weeks of dedicated work in Milestone 5.

**Menu lockdown via mixin-based UI replacement.** Vanilla Minecraft's `MainScreen`, multiplayer screen, etc. are intercepted via Mixin in the Athion mod and replaced with either no-op (button removed) or Athion-branded equivalents. This is the only way to truly lock the experience — the launcher can't prevent the user from clicking buttons inside Minecraft.

## v1 feature scope

### Launcher (Tauri + Rust)

- Branded login screen (Athion account + Microsoft OAuth pairing)
- Server picker as primary UI — live list of Athion servers with status, ping, player count, news
- IPC server with random per-launch auth token
- Manifest fetching from CDN with signature verification
- Instance file sync: parallel downloads, hash verification, atomic file replacement
- JVM spawning with full classpath construction (vanilla, NeoForge, Fabric) — no Prism in production
- Transition orchestrator (state machine for handoffs)
- Win32 window manipulation: rect tracking, focus transfer, show/hide, position snapping
- Transparent always-on-top overlay window
- Loading screen with weighted progress, stage labels, detail text, rotating tips
- Self-update mechanism

### Athion hub mod (NeoForge, currently 1.21.1)

- IPC client connecting to launcher on Minecraft startup
- Server plugin message channel listener (`liminal:transition`) — eventual server-driven trigger
- Window position/size reporter to launcher
- `world_loaded` signal to replace fixed delay
- **Menu lockdown:** strip Singleplayer / Multiplayer / Realms buttons, replace main menu, route Disconnect → quit Minecraft → return to Athion launcher
- Branded splash + loading screens (replace Mojang/Forge defaults where licensable)
- Clean shutdown handler

### Athion survival mod (Fabric, currently 1.21.11)

- Same shape as hub mod (most code shared via mod-shared library)
- Different loader scaffolding
- Render-loop hook to detect first frame and emit `first_frame_rendered`
- Per-mod loading progress reporting (Fabric Loader entrypoints)

### Server plugin (Velocity)

- Command handlers: `/survival`, `/hub`, `/lobby`
- `liminal:transition` plugin message channel for server-driven triggers
- Per-player auth token validation (so a malicious player can't spoof transition messages)

### Backend Spigot/Paper plugins (one per Minecraft backend server)

- Portal-walk event detection → signals Velocity to send transition message
- Other gameplay-driven trigger sources

### Manifest server / CDN

- Static file server with proper cache headers
- Signed JSON manifests per instance
- Mod files served from same CDN
- Versioned, with rollback support

### Auth proxy (Athion-side)

- Validates Microsoft OAuth tokens
- Maps Microsoft identity → Athion account
- Issues short-lived Athion session tokens for client/server auth

## Milestone breakdown

> Spike phase complete. Real Milestone work begins next.

### Milestone 0: Foundations (~1 week)

- Tauri project skeleton
- Rust workspace structure (replace spike crates with proper modules)
- Microsoft OAuth flow
- Vanilla Minecraft launch from raw Rust java cmd (no Prism) — basic case
- Stub manifest server (S3 or static host)
- **Done when:** the Athion app shows a login screen, you log in, vanilla Minecraft launches.

### Milestone 1: Production Minecraft launching (~1 week)

- Parse Mojang/Fabric/NeoForge version JSONs
- Construct full classpath for vanilla, Fabric, NeoForge launches
- Library + asset download with parallel streams + hashing (content-addressed)
- File store layout
- **Done when:** the launcher launches a hub-style NeoForge instance and a survival-style Fabric instance from raw java cmds, lands in their respective servers via `--quickPlayMultiplayer`.

### Milestone 2: Mod sync + menu lockdown (~1 week)

- Manifest schema finalized (versioned, signed)
- Mod files synced based on per-instance manifests
- Athion mod's menu-lockdown layer (replace vanilla screens via Mixin)
- **Done when:** when launching any Athion instance, players see only Athion-branded UI inside Minecraft (no Singleplayer button, no Multiplayer button, etc.).

### Milestone 3: Real production trigger flow (~1 week)

- Velocity plugin sending `liminal:transition` plugin messages
- Athion mod listening on the plugin message channel and forwarding via IPC
- Backend Spigot plugin for portal-walk triggers
- **Done when:** `/survival` typed in the hub triggers a real transition end-to-end via the Velocity → mod → launcher path.

### Milestone 4: Polish the transition (~1 week)

- `world_loaded` IPC signal replaces fixed delay
- Loading screen with stage labels driven by real progress messages
- Tips/lore/news system
- Error recovery (transition fails gracefully)
- **Done when:** the transition feels native — no fixed delays, real progress, tasteful loading screen.

### Milestone 5: Cold launch optimization (~1-2 weeks)

- JVM Class Data Sharing (AppCDS archive per instance)
- Mod load order audit
- Shader cache persistence
- Asset prefetching during hub session
- File system pre-warming
- **Done when:** median cold launch <12s, P95 <18s.

### Milestone 6: Multi-monitor / DPI / display change (~1 week)

- DPI awareness (`SetProcessDpiAwarenessContext`)
- Track game window monitor; overlay follows
- Display change handling (monitor unplugged mid-session)

### Milestone 7: Production hardening (~2-3 weeks)

- Antivirus compatibility (code-signing the binary; ~$200/year cert)
- Crash reporting (Sentry or similar)
- Auto-update for launcher binary
- User-facing error messages for every failure mode
- Installer (MSI or NSIS)
- Manifest CDN with proper caching/signing/version pinning
- Mod tampering detection
- Documentation + operational runbook

### Milestone 8: Launch + iteration

- Beta with a small set of Athion regulars
- Telemetry-driven iteration
- Public launch

## Current state (validated)

### Spikes — all four validated end-to-end

| # | What | State |
|---|---|---|
| 1 | Win32 transparent overlay (`launcher/spike-overlay/`) | ✓ Windowed + borderless work; exclusive fullscreen broken (acceptable, documented) |
| 2 | JVM child process + WebSocket IPC (`launcher/spike-jvm-ipc/`) | ✓ Java test client connects, authenticates, exchanges messages, exits cleanly |
| 3 | Full transition mechanic with test JVMs (`launcher/spike-transition/`) | ✓ Hotkey → overlay → swap → reveal, all primitives combined |
| 4 | Real Minecraft transition via Prism CLI (`launcher/spike-mc-transition/`) | ✓ NeoForge 1.21.1 hub → loading screen → Fabric 1.21.11 survival, auto-joining real multiplayer servers, with the Athion hub mod inside Minecraft triggering transitions via IPC |

### Athion hub mod (`mod-hub/`)

- NeoForge MDK customized as `mod_id=liminal_hub`, client-only
- IPC client reading `%APPDATA%\Liminal\ipc.json` for launcher discovery; runs Spike #2's auth/ready handshake
- F9 hotkey inside Minecraft sends `transition_request` to launcher → launcher fires the existing transition flow
- Validated against the launcher: full mod ↔ launcher trigger path works end-to-end

### Local test infrastructure (outside the repo)

- NeoForge 1.21.1 server at `localhost:25565` for hub testing
- Vanilla 1.21.11 server at `localhost:25566` for survival testing
- Microsoft OpenJDK 21 installed for Java work
- Prism Launcher used as a development-time stand-in for production launching (will be replaced in Milestone 1)

### What's been intentionally deferred

- Production Minecraft launching — currently rides on Prism CLI; replaced in Milestone 1
- Auth — currently uses Prism's stored Microsoft account; replaced in Milestone 0
- Manifest sync — no manifest server yet; Milestone 1
- Menu lockdown — none yet; Milestone 2
- `world_loaded` IPC signal — currently a fixed 20s post-window-load delay; Milestone 4
- Survival mod (Fabric edition) — only hub mod exists; Milestone 2 or 3 depending on order
- Velocity plugin — currently F9 hotkey is the trigger; Milestone 3
- Tauri UI — none yet, launcher is currently a CLI-style spike; Milestone 0

## What to build next

The spike phase is *done*. Time for real Milestone work.

**Highest impact direction:** Milestone 0 (Tauri skeleton + Microsoft OAuth + vanilla Minecraft launch from raw java cmd). This unlocks everything downstream because every other milestone needs the launcher to be more than a spike binary.

**Alternate near-term high-impact bets:**

- **Milestone 3 first** (Velocity plugin + mod plugin message channel) — finishes the architecture diagram against the existing spike infrastructure. Lets us demo "type `/survival` in real game, get to survival" end-to-end without any new launcher work. Defers Tauri UI but completes the *headline UX*.
- **Menu lockdown spike** in `mod-hub/` — visually striking, technically self-contained, validates the lockdown approach (Mixins to replace screens) before we depend on it for the launcher's "no main menu" guarantee.

My honest recommendation: **Milestone 0 first.** Without a real launcher, every other piece is bottlenecked. Tauri scaffolding + login screen + "click → launch vanilla Minecraft" is concrete, demoable, and the foundation everything else builds on. Track 3 work (Velocity plugin, menu lockdown) is meaningfully easier when there's a real launcher to integrate with.

## Open questions / TBDs

1. **Naming.** Public brand is **Athion**. Internal codename and code prefix is **Liminal** (mod IDs, system properties, IPC file paths). Working assumption: keep the codename internally (no rename pain), present as Athion externally. If we want a unified name, decide before Milestone 7 (installer naming).
2. **Server hostnames.** What addresses will Athion's hub/survival/etc. live at in production? Direct DNS, behind Cloudflare, single Velocity port routing internally?
3. **Microsoft Azure tenant.** Need an Azure app registration for OAuth (free, ~20 min). Has this been done?
4. **CDN for mod hosting.** Cloudflare R2? AWS S3? Self-hosted on Athion's Proxmox? Affects manifest URLs, signing strategy, cache behavior.
5. **Manifest signing.** Ed25519? Where does the private key live? How are public keys baked into / rotated from the launcher binary?
6. **Tips/news backend in loading screen.** Static file refreshed periodically? Dynamic API? Ship-with-launcher-and-update?
7. **Telemetry strategy.** Crash reporting yes (Sentry default). Usage telemetry — opt-in or none? GDPR if EU users will be served.
8. **Athion account ↔ Microsoft account binding.** First-run flow design — single sign-in via Athion that proxies Microsoft? Two separate sign-ins? Account recovery story?

## Decisions explicitly deferred

These are *not* in scope for v1 unless explicitly added:

- macOS support
- Linux support
- Multi-server federation (one launcher serving multiple unrelated networks)
- In-launcher chat / social features (could come later as Athion features, not v1)
- Modpack browser / mod marketplace
- Custom Minecraft proxy / network code
- Voice chat integration
- Streaming / OBS integration
- Player-managed instances or mod customization

Any feature request that lands here defaults to "no" unless it advances the v1 product.

## Naming and codebase conventions

**Brand vs codename:**

- **Athion** is the player-facing brand. App name, installer name, server-list UI text, marketing.
- **Liminal** is the internal codename. Mod IDs (`liminal_hub`, `liminal_survival`, `liminal_shared`), system properties (`liminal.connect.address`, `liminal.auth.token`), IPC file path (`%APPDATA%\Liminal\ipc.json`), namespace prefixes in the Rust workspace.

The dual name is intentional: internal stability (no global rename) + clean external brand. Don't rename `liminal_*` identifiers unless we have a strong reason; do present everything user-facing as Athion.

**Note on the `liminal-launcher/` directory name:** the repo lives at `~/Desktop/liminal-launcher/` because there's an unrelated project at `~/Desktop/liminal/` (a Tauri-based AI terminal IDE — completely different thing that happens to share the name). The `-launcher` suffix disambiguates them. The GitHub repo is `athion-mc`; the local dir name is historical and fine to leave as-is.

**Code conventions:**

- **Rust:** standard rustfmt, clippy clean. Comments explain *why*, not *what*. Module-level docs explain each module's role.
- **Java:** standard Minecraft modding conventions. Mixins prefixed `Mixin*`. Mod IDs `liminal_hub`, `liminal_survival`, `liminal_shared`.
- **JSON IPC messages:** snake_case keys, `type` tag on every message, `protocol_version` on the auth message.
- **Logging:** structured, separate launcher and game-side logs, correlation IDs for transitions so a single transition can be traced end-to-end across processes.

## How to use this document with Claude Code

When starting a Claude Code session in this project's directory:

> Read CONTEXT.md to understand the project's vision, current state, and what to build next. Then [specific task].

The doc captures the *why* behind decisions, not just the *what*, so Claude can make consistent choices on related questions without re-deriving them. Update this doc whenever an architectural decision changes or a milestone moves — it's the project's memory.

## Outstanding clarifications

This doc reflects decisions made through the spike phase. Anything not explicitly here is open. When in doubt, the decisions in this document take precedence over assumptions or general best practices, because they account for project-specific constraints that general advice doesn't know about.
