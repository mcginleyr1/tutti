# Tutti — Implementation Plan

Tutti is a terminal-native **agent multiplexer**: tmux for AI coding agents. It runs
multiple terminal agents (Claude Code, Codex, Cursor CLI, OpenCode, etc.) in persistent
panes, understands each agent's state (blocked / working / done / idle), and exposes a
CLI + socket API so both humans and agents can drive it.

**Out of scope for now (deliberate):** theming (we inherit the terminal's theme),
remote/SSH/mobile clients, and a plugin system (we'll design a better one later —
do not build extension points speculatively, but keep module boundaries clean so a
plugin layer can be added without a rewrite).

---

## Architecture

Client/server split, all local:

- **`tutti-server`** — a background daemon that owns all PTYs and state. Panes keep
  running when no client is attached. One server per named session (default session:
  `tutti`).
- **`tutti` (client)** — a ratatui TUI that attaches to the server over a Unix domain
  socket, renders panes, and forwards input. Detaching (prefix, then `q`) leaves the
  server running.
- **Control plane** — the same Unix socket speaks length-prefixed frames: control
  frames carry one JSON object each (NDJSON semantics), pane content travels as
  binary vt100-delta frames (see `docs/transport-decision.md`). The `tutti` binary
  doubles as the CLI: `tutti pane split`, `tutti pane read`, etc. are thin commands
  over the socket. Anything the TUI can do, the CLI and API can do.

### Domain model

```
Session (named; isolated server instance + socket)
└── Workspace (one per repo/task; has a working directory)
    └── Tab (a layout of panes: agents / logs / servers ...)
        └── Pane (one persistent PTY; may host a detected Agent)
```

Agent state machine per pane: `Unknown → Working ⇄ Blocked`, `Working → Done`,
`Done → Idle` (Done = finished but not yet viewed by a human; viewing the pane
transitions Done → Idle). This distinction powers the "what needs my attention"
overview.

### Workspace layout (Cargo)

```
tutti/
├── Cargo.toml                 # workspace root (resolver = "3", edition 2024)
└── crates/
    ├── tutti-core/            # domain model, protocol types, state machine — no I/O deps
    ├── tutti-server/          # daemon: PTY management, vt parsing, socket server
    ├── tutti/                 # client binary: ratatui TUI + CLI subcommands
    └── tutti-agents/          # agent detection & per-agent integrations
```

### Key dependencies

Use the latest published versions (verify with `cargo search` / `cargo add`, don't
trust versions from memory):

- `ratatui` **0.30.x** — note 0.30 was a breaking release (`ratatui::init()` /
  `ratatui::run()` app model); older examples online won't compile.
- `crossterm` — terminal backend + input (keyboard **and mouse** events).
- `portable-pty` — spawn and manage PTYs cross-platform.
- `vt100` (or `termwiz` escape parsing) — maintain an in-memory screen grid per pane
  so we can render scrollback, detect agent screen content, and serve `pane read`.
- `tokio` — async runtime for the server (PTY I/O, socket clients, timers).
- `serde` / `serde_json` — protocol.
- `clap` — CLI.
- `sysinfo` or direct `/proc` + `libproc` — foreground process detection for agent
  identification.

---

## Alpha milestone (v0.1)

**Definition of alpha:** you can live in tutti all day herding Claude Code agents —
run them, split them, detach/reattach, and see at a glance which one needs you —
without falling back to tmux.

**In scope** (the cut across the phases below):

- **Phase 1, minus restore**: daemon, socket protocol, all CLI verbs, PTY + vt100
  grids. Skip state persistence entirely — a killed server loses layout, fine for alpha.
- **Phase 2, keyboard-first**: attach/detach, grid rendering, terminal + prefix modes,
  binary splits, resize, zoom, status bar. Mouse limited to click-to-focus and scroll
  wheel. Single attached client is enough.
- **Phase 3, heuristics only**: detection registry seeded with `claude` and `codex`;
  state from screen-content + output-flow heuristics; badges on borders and status
  bar; Done → Idle on focus. No agent hook integrations, no desktop notifications.

**Explicitly out until beta:** multi-client attach, drag-resize / text-select /
OSC 52 copy, navigate-mode fuzzy tree (a plain pane list is fine), all of
Phase 4 (wait/orchestration verbs, worktree workflow, skill), all of Phase 5
except the config file, which landed early (keybindings, presets, prefix, mouse
— see README Configuration).

**Build order:**

1. **M0 — transport spike.** ✅ Decided — hybrid framing: length-prefixed frames,
   JSON control messages, server-computed vt100 escape deltas for pane content,
   snapshot-on-attach. Full analysis in `docs/transport-decision.md`.
2. **M1 — headless core.** Phase 1 acceptance test: `pane run -- top`, close the
   terminal, `pane read` shows it still running.
3. **M2 — TUI.** Phase 2 acceptance: two agents side by side, split/resize/zoom,
   detach, reattach, nothing lost.
4. **M3 — badges.** Phase 3 acceptance: Blocked flagged within ~1s, Done until focused.

**Alpha exit:** one week of daily dogfooding without reaching for tmux.

## Phase 1 — Persistent PTY core (no UI)

Goal: a daemon that keeps terminals alive, and a CLI that can drive it. This is the
foundation everything else sits on; get it right before touching ratatui.

1. **`tutti-core`**: define `SessionId`, `WorkspaceId`, `TabId`, `PaneId`, the domain
   tree, and the protocol types (`Request` / `Response` / `Event` enums, serde-derived).
2. **Server lifecycle**: `tutti server start` daemonizes (or `--foreground` for dev),
   creates `$XDG_RUNTIME_DIR/tutti/<session>.sock` (fallback `/tmp/tutti-$UID/`).
   `tutti` auto-starts the server on first use. Named sessions via `-s <name>`.
3. **PTY management**: spawn shell/command per pane with `portable-pty`; async read
   loop feeds a per-pane `vt100::Parser` (screen grid + scrollback ring buffer);
   handle resize; reap dead children and mark panes exited.
4. **Socket protocol + CLI verbs** (each is both an API request and a CLI subcommand):
   - `workspace new --dir <path>` / `workspace list` / `workspace kill`
   - `tab new` / `tab list` / `tab select`
   - `pane run -- <cmd...>` / `pane split (right|down)` / `pane list` / `pane kill` / `pane rename`
   - `pane send <pane> --text "..."` (and `--keys` for control sequences)
   - `pane read <pane> [--lines N] [--unwrapped]` — dump current screen/scrollback
5. **State persistence**: server keeps everything in memory; on graceful shutdown,
   write layout (not pane content) to disk so `server start` can offer to restore.

**Acceptance:** with no TUI at all — `tutti workspace new --dir .`, `tutti pane run -- top`,
close the terminal, reopen, `tutti pane read` shows `top` still running.
Integration tests drive the socket directly; CI runs them headless.

## Phase 2 — TUI client

Goal: attachable ratatui client with real pane rendering and tmux-grade ergonomics.

1. **Attach/detach**: `tutti attach [-s session]` connects, subscribes to the event
   stream, renders. Detach with prefix `q`. Multiple clients may attach (all see the
   same session; sync sizes like tmux does — smallest client wins).
2. **Rendering**: draw each pane's vt100 grid into its ratatui area; cursor, colors,
   and attributes pass through — **no theming layer, the terminal's palette is the
   theme**. Damage-based redraw (only re-render panes whose grid changed).
3. **Input modes** (three, matching the concept model):
   - *terminal mode* — keys forwarded verbatim to the focused pane
   - *prefix mode* — `Ctrl+B` then an action key (split `%`/`"`, next tab `n`, kill `x`,
     detach `q`, navigate `w`, zoom `z`)
   - *navigate mode* — full-screen workspace/tab/pane tree with fuzzy jump
4. **Layout**: binary-split tree per tab; resize via prefix + arrows; zoom toggle.
5. **Mouse support** (crossterm mouse capture): click to focus pane, click tabs,
   drag split borders to resize, scroll wheel = scrollback, drag to select +
   copy (OSC 52 to system clipboard). Config flag to disable all mouse handling.
6. **Status bar**: session name, workspace tabs, per-pane state badges (see Phase 3).

**Acceptance:** run two agents side by side, split/resize/zoom with keyboard and
mouse, detach, reattach, nothing lost. Scrollback works in an active `claude` pane.

## Phase 3 — Agent awareness

Goal: the differentiator. Tutti knows which panes are agents and what state they're in.

1. **Detection**: identify the foreground process of each pane's PTY (process tree
   walk). Match against a registry in `tutti-agents`: claude, codex, cursor-agent,
   opencode, amp, copilot, etc. Registry is data-driven (a table of process names +
   heuristics), so adding an agent is a one-line change.
2. **State classification** per pane, using layered signals (most→least reliable):
   - agent-specific integrations (e.g. Claude Code hooks writing to a tutti-known
     location, or parsing its status line)
   - screen-content heuristics on the vt100 grid (prompt patterns like `❯`,
     "esc to interrupt", permission dialogs → `Blocked`)
   - process activity (PTY output flow, child process CPU) → `Working` vs quiet
   - fallback `Unknown`
   States: `Blocked` (needs input), `Working`, `Done` (finished, unseen),
   `Idle` (finished, viewed), `Unknown`. Focusing a `Done` pane transitions it to `Idle`.
3. **Surfacing**: colored state badge per pane border + tab, workspace overview in
   navigate mode grouped by state (Blocked first), optional terminal bell / OSC 9
   desktop notification on `Working → Blocked/Done`.
4. **API**: `pane list --json` includes agent + state; emit state-change events on the
   socket event stream.

**Acceptance:** start a Claude Code task; badge shows Working; when it asks for
permission the pane flags Blocked within ~1s; when it finishes, Done until you focus it.

## Phase 4 — Agent-shaped API & orchestration

Goal: agents can drive Tutti — spawn siblings, wait on them, read their output.

1. **Wait conditions**: `tutti pane wait <pane> --until done|blocked|idle [--timeout]`
   blocks until the state transition (server-side subscription, not polling).
2. **Structured read**: `pane read --unwrapped` (logical lines, no soft wraps) and
   `--since` cursors so a supervisor agent can tail incrementally.
3. **Composite verbs**: `tutti run --workspace <ws> -- claude "fix the tests"` =
   create pane, run agent, return pane id — one call for orchestrators.
4. **Agent skill**: ship a `skills/tutti/SKILL.md` (Claude Code skill) documenting the
   CLI so any Claude instance inside a tutti pane can herd its own sub-agents.
5. **jj workspace workflow** (first-class; jj is the required VCS for all
   workspace-level VCS features — no git/hg adapters): ✅ `tutti workspace fork
   <workspace> --name <name> [-r <rev>]` runs `jj workspace add` onto a sibling
   checkout and points a tutti workspace at it; ✅ `workspace kill --discard`
   forgets and removes a fork (refused for a non-fork); ✅ stale forks are
   surfaced in the sidebar with a dim-red tag and cleared by `workspace update`
   (`jj workspace update-stale`). ✅ `tutti workspace diff [--stat]` serves
   per-workspace diffs (sidebar shows `N files +A −B` per workspace, refreshed on
   state transitions). Merging back stays a human decision.
6. **Agent-event ingest + Claude Code hook integration**: ✅ panes get
   `TUTTI_PANE`/`TUTTI_SESSION` in their env; `tutti hooks claude` prints a hook
   config that makes the agent call `tutti agent-event claude` on subagent
   spawn/tool-use/stop and on permission notifications. Server attaches live
   subagent rows to the pane (shown indented under the agent in the sidebar) and
   upgrades Blocked/Done detection from screen heuristics to exact hook signals
   (a hook-driven pane is skipped by the screen classifier). Display only — tutti
   does not manage a foreign agent's subagents.

**Acceptance:** a script (or an agent) can: create 3 worktree workspaces, launch an
agent in each, `wait --until done` on all three, read back each diff summary.

## Phase 5 — Hardening & distribution

- Config file: `~/.config/tutti/config.toml` (prefix key, mouse on/off, default shell,
  agent registry overrides). No theme options.
- Crash-safety: server panics must not kill child PTYs where avoidable; client
  crashes never harm the server.
- Perf targets: 60fps redraw with 8 busy panes; `pane read` on 10k-line scrollback
  < 50ms; input latency indistinguishable from raw terminal.
- Test matrix: macOS + Linux; kitty/alacritty/wezterm/iTerm2/Terminal.app/ghostty.
- Packaging: `cargo install tutti`, Homebrew tap, curl installer. Windows deferred.

---

## Ground rules for implementation

- **Clean-room rule.** Do not read, reference, or consult the source code of any
  similar AGPL-licensed tool. Implement everything from this spec alone.
- `tutti-core` stays free of I/O and UI dependencies — pure types + state machine,
  heavily unit-tested (especially the agent state transitions and layout tree).
- Every feature lands with: the socket API first, the CLI verb second, the TUI last.
  If it isn't scriptable, it isn't done.
- Prefer boring, well-maintained crates; check latest versions at time of adding
  rather than pinning from this document.
- Keep module boundaries such that a future plugin/extension layer (event hooks,
  custom panes) can subscribe to the existing event stream without core changes.
