# Tutti — Implementation Plan

Tutti is a terminal-native **agent multiplexer**: tmux for AI coding agents. It runs
multiple terminal agents (Claude Code, Codex, Cursor CLI, OpenCode, etc.) in persistent
panes, understands each agent's state (blocked / working / done / idle), and exposes a
CLI + socket API so both humans and agents can drive it.

**Out of scope for now (deliberate):** remote/SSH/mobile clients (remote
*agents* are in: Phase 4 item 10 launches them into Kubernetes sandboxes, with
the pty still local), and a plugin system (we'll design a better one later — do not build extension points
speculatively, but keep module boundaries clean so a plugin layer can be added
without a rewrite). Theming was on this list until 2026-09-01: the default still
inherits the terminal's palette live, and `theme = "dark" | "light"` are opt-in
fixed truecolor palettes chosen in config — the only detection is a one-shot
light-background query at attach that picks the selection row's ANSI colour (README
Colour; the design rules live in CLAUDE.md).

---

## Architecture

Client/server split, all local: the daemon, the client, and every pane's pty.
Only a sandbox pane's agent runs elsewhere, in a pod its pty `kubectl exec`s
into.

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
        └── Pane (one persistent PTY; may host an Agent — detected, or
                  declared when it runs in a sandbox)
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
- `kubectl` — an external tool, not a crate: sandbox panes drive agent-sandbox
  through the user's own `kubectl`, so kubeconfig contexts and auth plugins
  (GKE's) just work. No Kubernetes client crate.

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
   and attributes pass through — on the default `terminal` theme the terminal's
   palette is the theme; the opt-in `dark`/`light` themes re-ground only a pane's
   *default* colours. Damage-based redraw (only re-render panes whose grid
   changed).
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
   heuristics), so adding an agent is a one-line change. Sandbox panes skip the
   walk (it only ever finds `tutti` and `kubectl`); their kind is declared from
   the command at spawn.
2. **State classification** per pane, using layered signals (most→least reliable):
   - agent-specific integrations (e.g. Claude Code hooks writing to a tutti-known
     location, or parsing its status line)
   - screen-content heuristics on the vt100 grid (prompt patterns like `❯`,
     "esc to interrupt", permission dialogs → `Blocked`)
   - process activity (PTY output flow, child process CPU) → `Working` vs quiet
   - fallback `Unknown`
   States: `Blocked` (needs input), `Working`, `Done` (finished, unseen),
   `Idle` (finished, viewed), `Unknown`. Focusing a `Done` pane transitions it to `Idle`.
   ✅ Ranked sources: `PaneInfo.source` names what set the state, ordered Hook >
   Program (OSC 7501) > Progress (OSC 9;4) > Screen > Activity, and a lower
   source cannot move a pane a higher one holds (one exception: a visible
   Blocked dialog beats a stale progress/program Working). A claim ends when its
   source clears it, the child exits, or the detected agent changes; hook claims
   stay sticky. Any pane may report its own state (OSC 7501 root record only, no
   `?` probe; OSC 9;4 never surfaces as a notification) and then shows where
   agent state does, labelled by its `app`. A non-hook pane `Working` with no
   output and no classifier match for 30s stalls to `Unknown` — a behaviour
   change for guess-pattern agents, which used to sit in `Working` forever after
   any output. A 7501 `clear` from `Blocked` settles to `Idle` (beyond the spec,
   so a cleared shell pane can leave the waiting queue). Open: a program claim
   ends only on its own `clear`, and the spec's own shell example never sends
   one — that pane then ignores 9;4 for life; end a Done/Idle program claim
   when the pane is typed into (TUIOS drops done records on typing). And other
   ConEmu `OSC 9;N` subcommands (`9;9;<cwd>`, which shells emit for Windows
   Terminal) still surface as bogus notifications — only 9;4 was fixed; a
   `9;<digits>;…` body is a subcommand, never a notification.
3. **Surfacing**: colored state badge per pane border + tab, workspace overview in
   navigate mode grouped by state (Blocked first), optional terminal bell / OSC 9
   desktop notification on `Working → Blocked/Done`.
4. **API**: `pane list --json` includes agent + state; emit state-change events on the
   socket event stream.

**Acceptance:** start a Claude Code task; badge shows Working; when it asks for
permission the pane flags Blocked within ~1s; when it finishes, Done until you focus it.
✅ Met — Phase 3 is done; what remains above is follow-up, not acceptance.

## Phase 4 — Agent-shaped API & orchestration

Goal: agents can drive Tutti — spawn siblings, wait on them, read their output.

1. **Wait conditions**: `tutti pane wait <pane> --until done|blocked|idle [--timeout]`
   blocks until the state transition (server-side subscription, not polling).
   Open — next wave. Every transition source now lands in `publish_transition`,
   so the wait subscribes to the `StateChanged` stream it emits; a pane already
   in the wanted state answers at once.
2. **Structured read**: `pane read --unwrapped` (logical lines, no soft wraps) and
   `--since` cursors so a supervisor agent can tail incrementally. ✅
   `--unwrapped`. Open — next wave: `--since`.
3. **Composite verbs**: `tutti run --workspace <ws> -- claude "fix the tests"` =
   create pane, run agent, return pane id — one call for orchestrators. Open —
   next wave: `pane run` targets a `--tab`, not a workspace, so today this is
   two calls.
4. **Agent skill**: ship a `skills/tutti/SKILL.md` (Claude Code skill) documenting the
   CLI so any Claude instance inside a tutti pane can herd its own sub-agents.
   Open — next wave, after 1–3 so it documents the verbs an orchestrator needs.
5. **jj workspace workflow** (first-class; jj is the required VCS for all
   workspace-level VCS features — no git/hg adapters): a **workspace** is a jj
   checkout nested under a **project** (its origin repo), rendered indented under
   the project in the sidebar. ✅ Guided create: sidebar `w` (a project or agent
   row) runs a two-step `workspace name:` → `where:` prompt (sibling default
   prefilled, directory completion) and `WorkspaceFork` with the chosen
   destination runs `jj workspace add`. ✅ `workspace kill --discard` forgets and
   removes a workspace (refused for one tutti did not create). ✅ stale workspaces
   are surfaced with a dim-red tag and cleared by `workspace update`
   (`jj workspace update-stale`). ✅ `tutti workspace diff [--stat]` serves
   per-workspace diffs — **against trunk** (the fork point from
   `main`/`master`/`trunk`), so committed agent work shows, not just the working
   copy (sidebar shows `N files +A −B` per workspace, refreshed on state
   transitions and on every operation the jj repo records — a commit,
   bookmark move, or push from any shell). ✅ Changed-files panel: an
   always-on right column listing
   the active workspace's changed files with exact `+`/`−` counts (`C-b f`
   focuses; enter = file diff, `d` = full diff, both in ephemeral pager panes). ✅ Merge back: sidebar `m` (or `tutti workspace merge <id>
   [--push]`) rebases the workspace onto trunk (`main`/`master`), refuses+undoes
   on conflict, advances the bookmark, optionally pushes, then offers cleanup —
   still a human-triggered decision.

   **CLI-vocabulary debt:** the TUI banished "fork" (git baggage) for
   project/workspace/merge wording, but the agent-facing CLI verb is still
   `tutti workspace fork` (unchanged for scripts). Reconcile the CLI verb naming
   in a later pass rather than half-renaming now.
6. **Agent-event ingest + Claude Code hook integration**: ✅ panes get
   `TUTTI_PANE`/`TUTTI_SESSION` in their env; `tutti hooks claude` prints a hook
   config that makes the agent call `tutti agent-event claude` on subagent
   spawn/tool-use/stop and on permission notifications. Server attaches live
   subagent rows to the pane (shown indented under the agent in the sidebar) and
   upgrades Blocked/Done detection from screen heuristics to exact hook signals
   (a hook-driven pane is skipped by the screen classifier). Display only — tutti
   does not manage a foreign agent's subagents. ✅ opencode joins through a
   JavaScript plugin (`tutti hooks opencode [--install]`, same confirm + backup
   flow) that forwards session busy/idle/permission/error as `agent-event
   opencode`. Open: hook install is CLI-only for both (no TUI path yet — debt
   against the ground rule), and Codex/Gemini integrations need their specs read
   and a live run before they land.
7. **Conversation resume**: ✅ wave 1 — the launcher harvests resumable
   conversations for its target workspace from the agent tools' own session
   stores (read-only; each candidate verified against the working directory
   its own record names, since the stores are keyed lossily or not at all):
   Claude Code (`~/.claude/projects/<munged-cwd>/<session-id>.jsonl`), Muse
   Code (`$XDG_DATA_HOME/muse/sessions/YYYY/MM/DD/<id>/session.jsonl`), and
   Mistral Vibe (`$VIBE_HOME/logs/session/session_*/meta.json`), merged
   newest-first by mtime, up to three `resume` rows per agent at the panel's
   foot; picking one spawns the tool's own recipe (`claude --resume <id>`,
   `muse resume <id>`, `vibe --resume <id>`) in a new pane. Scripts get the
   same via `tutti pane run -- <that command>` — no new verb needed. Wave 2
   (open): capture `session_id` per pane from the hook payloads so a restarted
   daemon can offer to revive the exact conversation each dead agent pane was
   holding, not just the project's most recent ones.
8. **Per-turn checkpoints**: ✅ each agent turn's end (a pane leaving Working,
   through the single `publish_transition` funnel in server.rs) checkpoints the
   workspace's jj `@` — the commit id the change probe's own snapshot already
   reads (`jj log -r @ -T commit_id`), so no extra jj operation. Where the agent
   started (on detection, or on first entering Working) is #1, so turn 1 can be
   undone. In daemon memory, newest 50 per pane, numbered per pane and never
   reused; not persisted across daemon restarts. Surfaces: `tutti pane
   checkpoints | checkpoint-diff [N] [--stat] | restore <N> [--force]` and the
   changes panel's `turns` section (enter = turn diff, `r` = confirmed
   restore). Restores are refused mid-turn and when stale, and save the replaced
   state first as `before restore`. Sandbox panes record none: their agent
   edits the pod's checkout, and a checkpoint of the local one would offer to
   undo someone else's work. Open: persist the lists (they are just commit
   ids), and label turns from hook payloads.
9. **Answer from the queue**: ✅ `p` (or a right-click) on a blocked sidebar row
   opens a peek overlay of that agent's live prompt; answer keys go out as a
   guarded `PaneSend` whose `expect` is the prompt fingerprint (FNV-1a over the
   last 12 non-empty screen rows, one function in tutti-core shared by client
   and server). The daemon writes only while the pane is Blocked, the
   fingerprint still matches, and the pane has redrawn since the last guarded
   answer — so the first answer wins and a refusal writes nothing. Scripts get
   the same guard via `pane read --fingerprint` and `pane send --expect`.
   Open: hook-held approvals (a hook that blocks until answered from the
   queue), a table-driven sidebar keymap (only prefix keys are table-driven
   today), and correlating responses so a refused answer can't cancel a pending
   fork/merge follow-up.
10. **Kubernetes sandboxes**: ✅ `[[sandboxes]]` client-config profiles name
   an agent-sandbox target, either a `SandboxClaim` on a warm pool or a
   `Sandbox` from an image. `PaneRun { sandbox }` (CLI `pane run --sandbox`,
   TUI launcher `tab`) runs the command through `tutti sandbox run`, which
   provisions through the user's `kubectl`, waits for Ready, and `kubectl exec
   -it`s the agent. The pty stays local, so classification, OSC reports and
   guarded answers are unchanged. The agent kind is declared from the command
   (detection skips sandbox panes) and sandbox panes record no turns. Cleanup
   is a death-pipe watchdog in its own process group, since tutti SIGKILLs a
   killed pane's whole group, backed by a renewed `shutdownTime` TTL. The
   sandbox owns the code: the image or template clones it (`TUTTI_GIT_REMOTE`
   for image sandboxes) and the agent pushes. Verified against agent-sandbox
   v1.0.2 on a local cluster (OrbStack): exit, pane kill, `server stop` and
   ctrl-c mid-provisioning each delete the sandbox. Open: hook signals from the
   pod (an agent's hook could report over OSC 7501, which already rides the
   pty); `TUTTI_GIT_REMOTE` for claims whose template sets
   `envVarsInjectionPolicy: Allowed` (the default rejects claim env); and
   pulling a sandbox's pushed branch into a local workspace for diff/merge.

**Acceptance:** a script (or an agent) can: create 3 worktree workspaces, launch an
agent in each, `wait --until done` on all three, read back each diff summary.
Not yet runnable: it needs items 1 and 3.

### Next waves (as of 2026-10-10)

In order; each lands its TUI path in the same wave as its verbs.

1. **Finish Phase 4** — `pane wait`, `pane read --since`, the one-call `tutti
   run --workspace`, then the agent skill. Close the wave by running the
   acceptance script above for real, against live agents.
2. **State follow-ups** (Phase 3 item 2's opens) — end a Done/Idle program
   claim when the pane is typed into; stop `OSC 9;N` subcommands surfacing as
   notifications.
3. **Debts against the ground rules** — a TUI path for hook install (claude
   and opencode are CLI-only); a table-driven sidebar and changes-panel keymap
   (CLAUDE.md's "everything renders from the live keymap table" holds only for
   prefix keys — `p`, `r` and the panel keys were wired by hand); correlate
   responses with requests so a refused answer can't cancel a pending
   fork/merge follow-up.
4. **Survive a daemon restart** — persist checkpoint lists (they are commit
   ids) and resume wave 2 (session ids from hook payloads); both want the same
   small state file.
5. **Candidates from prior art** — read each one's spec before building:
   herdr-compatible reporting (TUIOS answers on herdr's env and socket, so
   Crush and Muse Code — both in our catalog — report with no install step);
   hook-held approvals answered from the queue; a Codex hook integration
   (needs a live run).

**Unverified live — cover these in wave 1's acceptance run:** checkpoints
through a real Claude Code turn (verified only with synthetic hook JSON) and
through the screen-classifier, agent-exit and focus paths; whether Claude Code
emits its own OSC 9;4 (a hook-less claude pane would then hold off `Done`
until the progress clears); opencode's question/error/retry events and `hooks
opencode --install --project`; `jj util gc` pruning a checkpoint's commit; a
turn-end probe racing a restore.

## Phase 5 — Hardening & distribution

- Config file: `~/.config/tutti/config.toml` (prefix key, mouse on/off, default shell,
  agent registry overrides). `theme = terminal|dark|light` landed with it.
- Crash-safety: server panics must not kill child PTYs where avoidable; client
  crashes never harm the server.
- Perf targets: 60fps redraw with 8 busy panes; `pane read` on 10k-line scrollback
  < 50ms; input latency indistinguishable from raw terminal.
- Test matrix: macOS + Linux; kitty/alacritty/wezterm/iTerm2/Terminal.app/ghostty;
  sandbox panes against a local cluster running agent-sandbox (unit tests cover
  the manifests, the cluster run covers the cleanup paths).
- Packaging: `cargo install tutti`, Homebrew tap, curl installer. Windows deferred.

---

## Ground rules for implementation

- **Prior art.** Consulting permissively-licensed (Apache/MIT/BSD) tools for
  design ideas is fine; never copy code verbatim, and never consult
  AGPL/copyleft sources.
- `tutti-core` stays free of I/O and UI dependencies — pure types + state machine,
  heavily unit-tested (especially the agent state transitions and layout tree).
- Every feature lands with: the socket API first, the CLI verb second, the TUI last.
  If it isn't scriptable, it isn't done. The converse holds for humans: the CLI is
  the agent/script API, not the human interface — **no human-facing capability may
  ship CLI-only**. A wave that adds a verb a human would use must land its TUI
  path in the same wave.
- Prefer boring, well-maintained crates; check latest versions at time of adding
  rather than pinning from this document.
- Keep module boundaries such that a future plugin/extension layer (event hooks,
  custom panes) can subscribe to the existing event stream without core changes.
