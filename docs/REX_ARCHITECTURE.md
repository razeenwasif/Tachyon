# Tachyon Multiplexer Architecture (Rex-Style Multi-Host Design)

This document defines the architecture, wire protocol, CLI, UI model, and phased rollout for evolving Tachyon from a local macOS session host into a **persistent, multi-host multiplexer terminal** inspired by Superlogical Rex — with the MacBook as the native Apple silicon client, **Mac mini (`kagutsuchi`)** as the first remote server host, and **Linux and Windows PCs** supported by the exact same server protocol later.

All of Tachyon's existing differentiators stay intact: Metal + Core Text rendering, Liquid Glass, Rewind, Run Receipts, Metric Lens, Rerun Diff, Inline Images, Quick Terminal, Latency Ledger, Fairy, System Monitor, and Neutrino.

---

## 1. Core Model & Taxonomy

Every host server manages a structured tree of four primitives, shared consistently across the GUI, CLI, and agent integrations:

```text
Host (local | kagutsuchi | forge | windows-pc)
 └─ Session (persistent workspace, e.g. "misty-sierra", "four-claude-agents")
     └─ Window (tab holding a binary split layout of blocks)
         └─ Block (individual PTY / terminal, with live Term state & metadata)
Client (GUI window, CLI call, or event subscriber; multiple may attach at once)
```

| Primitive | ID format | Owned by | Key fields |
|---|---|---|---|
| **Host** | Alias (`local`, `kagutsuchi`, …) | Client config + `~/.ssh/config` | `alias`, `uri` (`unix://`, `ssh://`, later `tcp://`/`pipe://`), connection status, RTT |
| **Session** | `s_<hex>` | Server | `id`, `name` (friendly `adj-noun` default, renameable by user or agent), `windows[]`, `active_window`, `created_at` |
| **Window** | `w_<hex>` | Server | `id`, `session_id`, `name` (optional override), `layout` (split tree reusing `panes::Node`), `focused_block`, `zoomed` |
| **Block** | `b_<hex>` | Server | `id`, `window_id`, `session_id`, `pid`, `cols`, `rows`, `title`, `cwd`, `command`, `command_running`, `alive`, `exit_code`, `awake`, `program_status` (OSC 7501), `app_kind` |
| **Client** | `c_<hex>` | Server (ephemeral) | `id`, `kind` (`gui` \| `cli`), attached blocks, event subscriptions |

### Why Replace `src/mac/host.rs`'s Current Design?

`src/mac/host.rs` already proves Tachyon's core loop (detached PTYs, E-core server-side `Term` parsing, `Term::to_vt()` restore, detached query replies, job/Neutrino watching). Four limits stop it from hosting on `kagutsuchi`, Linux, or Windows:

1. **OS fd passing (`SCM_RIGHTS`)** only works between processes on one Unix machine — it cannot cross SSH, Tailscale, TCP, or Windows named pipes.
2. **macOS-coupled host code** lives under `src/mac/` with direct `libc::forkpty`, IOKit power assertions, and `UNUserNotificationCenter` calls.
3. **Flat session list + opaque layout blob** cannot support multiple hosts, concurrent GUI + CLI clients, programmatic splits/captures, or live structural event streaming.
4. **Ephemeral `/tmp` socket + 20 s idle exit** is scoped to one app bundle hash rather than a durable host daemon (`launchd` on Mac mini, `systemd --user` on Linux, service/startup on Windows).

---

## 2. Portable Server Architecture (`src/server/` & `src/mux/`)

### 2.1 Single Codebase, Headless Server Builds

`src/term/` is already 100% platform-independent Rust. To run the exact same binary on the MacBook, Mac mini (`kagutsuchi`), Linux PCs, and Windows PCs:

- Add a default Cargo feature `gui` in `Cargo.toml` guarding AppKit/Metal (`objc2-*`) on macOS and D3D11/DirectWrite (`Win32_Graphics_*`) on Windows.
- `cargo build --release --no-default-features` compiles a small headless `tachyon` binary (server + bridge + CLI only) on **macOS, Linux, and Windows** with zero GUI dependencies.
- Portable PTY abstraction (`src/server/pty.rs`):
  - `#[cfg(unix)]`: extracted from `src/mac/pty.rs` (`forkpty`, `TIOCSWINSZ`, `SIGHUP`, non-macOS QoS no-op) — covers **macOS (Mac mini & MacBook)** and **Linux**.
  - `#[cfg(windows)]`: wraps existing ConPTY implementation (`src/pty/mod.rs`) — covers **Windows PCs**.
- Platform hooks (`power` keep-awake, local `launchd`/`systemd` service install, detached notifications) sit behind thin `#[cfg(...)]` adapters with clean no-op fallbacks on headless Linux/Windows servers.

### 2.2 Daemon Lifecycle & Socket Paths

- **macOS / Linux local socket:** `$XDG_RUNTIME_DIR/tachyon/server.sock` if set, else `~/.local/state/tachyon/server.sock` (mode `0600`, peer-UID checked).
- **Windows local pipe (Milestone 4):** `\\.\pipe\tachyon-server-<username>` (owner-only DACL).
- **Persistence:** `tachyon server` stays alive across app quits, sleeps, and SSH disconnects.
  - On `local` MacBook: auto-started on demand by the GUI or CLI.
  - On Mac mini (`kagutsuchi`): auto-started by `tachyon bridge` on first connect, or kept running continuously via a user `launchd` plist (`tachyon server install-service`).
- **Session state snapshot:** Server persists metadata (sessions, window names, split ratios, cwd) on change and can cleanly report live blocks on reconnect.

---

## 3. Transport & Wire Protocol (`Tachyon Mux v2`)

### 3.1 Transports

| Mode | How it connects | Platforms | Milestone |
|---|---|---|---|
| **Local** | Unix domain socket (`server.sock`) | macOS, Linux | M1 |
| **Remote SSH (default)** | One persistent `ssh -T -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 <host> tachyon bridge` per host | MacBook → Mac mini, Linux, Windows (OpenSSH) | M1 |
| **Local Windows** | Named pipe (`\\.\pipe\tachyon-server-...`) | Windows | M4 |
| **Direct Tailscale TCP (optional)** | `tachyon server --listen <ts-ip>:7501` with bearer token / TLS | Any Tailscale node | M4 |

**How `tachyon bridge` works:**
1. Client opens one SSH process to `kagutsuchi` running `tachyon bridge` (checking `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `$PATH`).
2. `tachyon bridge` connects to the remote host's local `server.sock` (starting `tachyon server --daemon` first if needed) and splices stdin/stdout directly to the Unix socket.
3. **One SSH connection multiplexes every session, pane, keystroke, resize, CLI RPC, and event stream on that host** — zero open ports, uses your existing `~/.ssh/config` and Tailscale routing, and requires no separate auth layer.
4. When the MacBook sleeps or loses network, SSH drops cleanly; `tachyon server` on `kagutsuchi` keeps all shells and coding agents running; on wake, the MacBook client reconnects automatically with exponential backoff and re-attaches visible panes.

### 3.2 Binary Framing (Zero New Dependencies)

Every message on the transport stream uses a 9-byte little-endian header:

```text
 0       4      5         9                    9 + len
┌───────┬──────┬─────────┬────────────────────────────┐
│  len  │ kind │ channel │          payload           │
│  u32  │  u8  │   u32   │         [u8; len]          │
└───────┴──────┴─────────┴────────────────────────────┘
```

| `kind` | Name | `channel` | Payload | Direction |
|---|---|---|---|---|
| `0x01` | `RPC_REQ` | `req_id` | JSON object `{"op": "...", ...}` | Client → Server |
| `0x02` | `RPC_RES` | `req_id` | JSON object `{"ok": true, "data": ...}` or `{"ok": false, "error": "..."}` | Server → Client |
| `0x03` | `EVENT` | `0` | JSON object `{"event": "...", ...}` | Server → Client |
| `0x04` | `PTY_OUT` | `stream_id` | Raw PTY output bytes (zero JSON/base64 overhead) | Server → Client |
| `0x05` | `PTY_IN` | `stream_id` | Raw input bytes (keystrokes / paste / terminal replies) | Client → Server |
| `0x06` | `PTY_SNAPSHOT` | `stream_id` | `[flags: u8][lz4_compressed Term::to_vt() bytes]` (via existing `lz4_flex`) | Server → Client |
| `0x07` | `PTY_RESIZE` | `stream_id` | `[cols: u16, rows: u16, cell_w: u16, cell_h: u16]` | Client → Server |
| `0x08` | `PING` | `seq` | `[sent_ns: u64]` | Both |
| `0x09` | `PONG` | `seq` | `[sent_ns: u64]` | Both |

### 3.3 Core RPC Operations

- `hello` (`protocol: 2`, `client_kind`, `client_name`) → server version, host info (`hostname`, `os`, `arch`), `client_id`.
- `state.get` → full snapshot of all Sessions, Windows, Blocks, and connected Clients.
- `events.subscribe` (`filters`) → begins streaming `EVENT` frames (`session.*`, `window.*`, `block.*`, `block.status`, `block.title`, `block.command`, `block.exited`).
- `session.create` (`name?`, `cwd?`, `cmd?`, `cols`, `rows`) / `session.rename` / `session.kill` / `session.inspect`
- `window.create` (`session_id`, `name?`, `cwd?`, `cmd?`, `cols`, `rows`) / `window.rename` / `window.kill` / `window.focus` / `window.set_zoom` / `window.set_ratio`
- `block.split` (`block_id`, `dir: right|down`, `cwd?`, `cmd?`, `label?`, `cols`, `rows`) / `block.kill` / `block.inspect` / `block.set_awake`
- `block.attach` (`block_id`, `mode: interactive|readonly`, `restore: bool`) → assigns a `stream_id`; server drains pending parser queue under lock, emits `PTY_SNAPSHOT`, then streams live `PTY_OUT` with no gap or duplication.
- `block.detach` (`stream_id`)
- `block.send` (`block_id`, `data_b64` or `text`, `enter?`) — convenience RPC for CLI scripting without attaching a stream.
- `block.capture` (`block_id`, `lines?`, `ansi?`) — renders viewport + optional scrollback text directly from the server's `Term`.
- `block.wait` (`block_id`, `timeout_ms?`) — resolves when a `run` block exits, returning `exit_code`.

### 3.4 Multi-Client Resize & Slow-Client Backpressure

- **Resize policy:** Passive/CLI clients never send `PTY_RESIZE`. Among interactive GUI clients attached to the same block, the most recently focused/writing client controls PTY dimensions.
- **Backpressure:** Each client connection maintains a bounded per-stream outbound queue (`4 MiB`). If a remote Tailscale/SSH link stalls during heavy output (`cat` flood), the server drops queued raw `PTY_OUT` frames for that stream, marks it `resync_needed`, and sends a fresh LZ4-compressed `PTY_SNAPSHOT` once the socket drains. A slow client never stalls a PTY or starves control RPCs.
- **Local throughput & latency:** Server-side `Term` parsing runs on a background `QOS_CLASS_UTILITY` thread (as in `src/mac/host.rs`), so `PTY_OUT` forwarding to local and remote clients never waits on parsing. Measured echo & throughput benchmarks (`host_echo`, `host_cost`) will gate Milestone 1.

---

## 4. Automation CLI (`tachyon`)

Every PTY spawned by `tachyon server` receives:

```bash
TACHYON_SERVER=<host-alias>   # e.g. local or kagutsuchi
TACHYON_SESSION=<session-id>
TACHYON_WINDOW=<window-id>
TACHYON_BLOCK=<block-id>
```

And ensures the `tachyon` CLI binary is on `PATH` (via `~/.local/bin/tachyon` or bundle symlink).

### Command Surface

All read/mutation commands support `--json` and `-S, --server <host>` (defaulting to `$TACHYON_SERVER` or `local`):

```text
tachyon                                 # launch GUI app (macOS / Windows)
tachyon server run | start | stop | status | install-service
tachyon bridge                          # stdio <-> local socket bridge (used by SSH)

tachyon hosts ls | add <alias> <uri> | remove <alias> | test <alias>

tachyon ls                              # list sessions, windows, blocks & statuses
tachyon new [NAME] [--cwd DIR] [--cmd CMD...]
tachyon session ls | new | rename <ID> <NAME> | kill <ID> | inspect [ID]
tachyon window  ls | new | rename <ID> <NAME> | kill <ID> | focus <ID>

tachyon split [--right|--down] [-b BLOCK] [--cwd DIR] [--label NAME] [-- CMD...]
tachyon run   [--session S] [--split right|down] [--wait] [--cwd DIR] [--label NAME] -- CMD...
tachyon send  [-b BLOCK] [--enter] <TEXT>
tachyon capture [-b BLOCK] [--lines N] [--ansi]
tachyon events [--session S] [--json]

tachyon skill setup [--agent claude|codex|opencode|pi|all]
```

---

## 5. OSC 7501 Program Status & Coding Agent Integrations

### 5.1 OSC 7501 in `src/term/`

Implement Mitchell Hashimoto / Rex's OSC 7501 status escape sequence in `src/term/mod.rs` (`osc_dispatch`, `b"7501"`):

```text
ESC ] 7501 ; state=<state> ; app=<name> ; kind=<kind> ; msg=<base64> ; label=<base64> BEL/ST
```

- `state`: `idle` | `working` | `blocked` | `done` | `error` | `clear`.
- Stored on `Term::program_status: Option<ProgramStatus>`, preserved across `Term::to_vt()` restores (`src/term/restore.rs`), and broadcast as `block.status` server events.
- **Notifications:** When a block on `local` or any remote host (`kagutsuchi`) transitions to `blocked` (e.g., agent waiting for tool approval) or `done`/`error` while unfocused, the macOS client posts a native notification. Clicking the notification focuses that exact Host → Session → Window → Block.

### 5.2 Coding Agent Skills & Hooks

- `tachyon skill setup` (and the `Settings > Integrations` toggle) installs:
  1. A **Tachyon skill** teaching coding agents (Claude Code, Codex, OpenCode, Pi) how to use `tachyon split`, `tachyon run --wait`, `tachyon capture`, `tachyon send`, and `tachyon ls --json`.
  2. Lightweight **session hooks** that emit OSC 7501 status transitions (`working`, `blocked`, `done`, `idle`) and — when **"Let agents name sessions"** is enabled — label the session after the task being worked on via `tachyon session rename`.

---

## 6. Native macOS UI Architecture

### 6.1 Window Layout (Sidebar + Terminal Canvas)

Inside `host_glass` (`NSGlassEffectView`), a root container divides the window horizontally:

```text
┌──────────────────────────┬─────────────────────────────────────────────────┐
│ ● ● ●  [◧]   👁  ⌘  [+] │ ✳ Claude 1                     [◫][⬓][⤢][×]  │
│                          ├─────────────────────────────────────────────────┤
│ 🖥 This Mac              │                                                 │
│   misty-sierra           │             TachyonView (Metal + Core Text)     │
│     ✳ Claude Code   [●] │             Splits, Rewind, Receipts, Lens,     │
│   four-claude-agents     │             Rerun Diff, Inline Images           │
│     >_ zsh ~             │                                                 │
│     ✳ Claude agents (4) │                                                 │
│                          │                                                 │
│ 🌐 kagutsuchi        ●  │                                                 │
│   dawn-bluff             │                                                 │
│     >_ zsh ~             │                                                 │
│                          │                         [Neutrino][Sys][Fairy]  │
├──────────────────────────┤                                                 │
│ [Filter...]       [⚙][🌐]│                                                 │
└──────────────────────────┴─────────────────────────────────────────────────┘
```

- **Collapsible Liquid Glass Sidebar (`SidebarView`):**
  - Default open (`240 pt`, resizable `180–360 pt`), toggled anytime with **`⌘B`** or the top-left sidebar button.
  - Because `TachyonView` simply takes the remaining right frame (`x = sidebar_w`, `w = window_w - sidebar_w`), all existing Metal rendering, hit-testing, splits, Find bar, Rewind badge, Charts, and Fairy/Sysmon/Neutrino cards keep working cleanly without coordinate hacks.
  - Collapsing the sidebar drops cleanly back to Tachyon's edge-to-edge mode (with the floating tab capsule when multiple windows exist).
  - **Host sections:** `This Mac` always first, followed by configured remote hosts (`kagutsuchi`, later Linux/Windows PCs) with live reachability/connection dot and quick `+` new session button.
  - **Session & Window rows:** Live app/agent icon, title, split count badge, and OSC 7501 / OSC 133 status indicator. Bottom `Filter` search box filters across all hosts live.
- **Subtle Per-Pane Header Bars (`24 pt`):**
  - Shows app/agent icon, block title, remote host pill when viewing a remote pane (`kagutsuchi`), and short OSC 7501 status note.
  - Right actions (subtle, brighter on hover): split right (`⌘D`), split down (`⌘⇧D`), zoom (`⌘⇧Enter`), close (`⌘W`).
  - Configurable via `window.pane_headers = auto | always | never` (default `always` with sidebar open, `auto` when collapsed).
- **Existing Tachyon Tools:** Fairy, System Monitor, Neutrino, Quick Terminal (`⌃\``), Rewind (`⌘[`/`⌘]`), Receipts, Metric Lens, and Latency Ledger (`⌥⌘L`) remain integrated in place.
- **Native Settings Window (`⌘,`):**
  - Sidebar-navigated macOS Liquid Glass settings window with panes for **Server**, **Remote Hosts** (connect / test / deploy binary / install `launchd` service, seeded from `~/.ssh/config`), **Integrations** (agent skills/hooks + auto-naming toggle), **Interface**, and **Notifications**.

---

## 7. Phased Rollout Plan

Each milestone lands on its own feature branch, passes unit + integration tests, and ships as a usable vertical slice:

1. **Milestone 1 — Portable Server, Mux Protocol, Mac mini Host & CLI (`mux-server`)**
   - `gui` Cargo feature split for headless server builds (macOS, Linux, Windows).
   - Portable `src/mux/` (wire protocol + framing + client) and `src/server/` (Unix PTY, `Session → Window → Block` tree, LZ4 snapshots, backpressure, events, `tachyon server` + `tachyon bridge`).
   - Full `tachyon` CLI (`server`, `bridge`, `hosts`, `ls`, `new`, `session`, `window`, `split`, `run --wait`, `send`, `capture`, `events`) with `--json` and `-S`.
   - Deploy `tachyon` to Mac mini (`kagutsuchi`) over SSH, install user `launchd` agent, and verify persistent sessions + remote CLI + local echo/throughput benchmarks.

2. **Milestone 2 — Multi-Host Mac Client, Liquid Glass Sidebar & Pane Headers (`rex-sidebar`)**
   - `HostManager` connecting the macOS app to `local` + remote hosts (`kagutsuchi`) via `MuxClient` with auto-reconnect and RTT tracking, replacing the local `SCM_RIGHTS` `host::Client`.
   - Native collapsible `SidebarView` (host sections, sessions, windows, live filter, host/server controls) + `⌘B` toggle.
   - Subtle per-pane header bars (icon, title, host pill, split/zoom/close) wired to server `Window`/`Block` mutations.

3. **Milestone 3 — OSC 7501, Coding Agent Integrations & Settings UI (`agent-integrations`)**
   - OSC 7501 parser in `src/term/mod.rs` + `restore.rs`, live sidebar/header status indicators, and remote blocked/done macOS notifications.
   - `tachyon skill setup` + Claude Code / Codex / OpenCode / Pi skill & hook installer + "Let agents name sessions".
   - Native `⌘,` Settings window (`Server`, `Remote Hosts`, `Integrations`, `Interface`, `Notifications`).

4. **Milestone 4 — Linux & Windows PC Hosting + Optional Tailscale TCP (`cross-platform-hosts`)**
   - Linux build + `systemd --user` service installer on your Linux machine(s).
   - Windows ConPTY server wiring (`src/pty/mod.rs`) + Windows named pipe listener + service installer.
   - Optional direct Tailscale TCP listener (`--listen` + bearer token) alongside SSH.
