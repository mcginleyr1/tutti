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
  (Restore has since landed: Phase 1 item 5.)
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
— see README Configuration). Most of this has since landed ahead of beta —
multi-client attach, drag-resize, text-select and OSC 52 copy, the go-to
overlay, Phase 4, and Phase 5's crash-safety and perf work; packaging stays
deliberately undone.

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

   (The workspace verbs have since become `project add | list | kill` for
   top-level entries, with `workspace` kept for jj checkouts: Phase 4 item 5.)
5. **State persistence**: server keeps everything in memory; on graceful shutdown,
   write layout (not pane content) to disk so `server start` can offer to restore.
   ✅ The daemon saves its layout (not pane content) to
   `$XDG_STATE_HOME/tutti/<session>.json` whenever it changes (checked every 2s,
   written only when the bytes moved) and once more at a graceful shutdown,
   before the panes die. A starting daemon turns that save into
   `<session>.restore.json` and offers it on attach (`Attached.restorable`) — the
   TUI's `restore last session?` modal, `tutti server restore [--discard]` —
   never restoring on its own. Agent panes holding a resumable conversation
   revive into it (Phase 4 item 7); every other pane comes back as a shell, and
   saved commands are never re-run. An empty session never writes the save, so
   a restarted daemon cannot push its pending offer aside. A save that does not
   parse is set aside as `.restore.json.bad`. Open: a turn in flight at
   shutdown loses its prompt label.

**Acceptance:** with no TUI at all — `tutti workspace new --dir .`, `tutti pane run -- top`,
close the terminal, reopen, `tutti pane read` shows `top` still running.
Integration tests drive the socket directly; CI runs them headless.

## Phase 2 — TUI client

Goal: attachable ratatui client with real pane rendering and tmux-grade ergonomics.

1. **Attach/detach**: `tutti attach [-s session]` connects, subscribes to the event
   stream, renders. Detach with prefix `q`. Multiple clients may attach (all see the
   same session; sync sizes like tmux does — smallest client wins). ✅ Several
   clients attach at once: each pane takes the min rows and min cols over the
   clients viewing it (`ViewSizes`, each client's whole visible set),
   re-arbitrated on every visible-set change and whenever a client leaves by
   any path; a pane nobody shows keeps its size, and an old client's
   `PaneResize` still applies. ✅ Attach refuses to nest a session inside one of
   its own panes, and `-s` defaults to the pane's own `TUTTI_SESSION` inside
   tutti.
2. **Rendering**: draw each pane's vt100 grid into its ratatui area; cursor, colors,
   and attributes pass through — on the default `terminal` theme the terminal's
   palette is the theme; the opt-in `dark`/`light` themes re-ground only a pane's
   *default* colours. Damage-based redraw (only re-render panes whose grid
   changed).
3. **Input modes** (three, matching the concept model):
   - *terminal mode* — keys forwarded verbatim to the focused pane
   - *prefix mode* — `Ctrl+B` then an action key (split `%`/`"`, next tab `n`, kill `x`,
     detach `q`, sidebar `w`, navigate `g`, zoom `z`)
   - *navigate mode* — full-screen workspace/tab/pane tree with fuzzy jump.
     ✅ prefix `g`: a full-screen overlay — the state overview over the
     project → workspace → tab → pane tree, fuzzy subsequence filtering ranked
     by match quality, then urgency, then tree order; enter or a click jumps
     through the sidebar's own jump, and the highlight follows its row's
     identity across regroups.
4. **Layout**: binary-split tree per tab; resize via prefix + arrows; zoom toggle.
5. **Mouse support** (crossterm mouse capture): click to focus pane, click tabs,
   drag split borders to resize, scroll wheel = scrollback, drag to select +
   copy (OSC 52 to system clipboard). Config flag to disable all mouse handling.
   ✅ Drag split borders: a left press on the two cells where panes meet grabs
   their split, and `PaneSplitRatio` names it by a leaf from each side (their
   lowest common ancestor). The client predicts the border with `pane_rects`'
   own arithmetic and the server's clamp, so it sends only on a whole-cell
   move. A still click there still focuses; zoom hides the borders.
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
   so a cleared shell pane can leave the waiting queue). ✅ Typing ends a
   finished report: input (not a guarded answer) past a program/progress claim
   holding Done/Idle ends it, so a reporter that never sends its `clear` (the
   spec's own shell example) no longer shuts out 9;4 for the pane's life. ✅
   ConEmu's other `OSC 9;N` subcommands (`9;9;<cwd>`, which shells emit for
   Windows Terminal) never surface as notifications: a `9;<digits>[;…]` body is
   a subcommand. ✅ herdr-compatible reporting: every pane carries `HERDR_*` and
   the daemon answers herdr's v0.9.3 reporting socket per session, so Crush (and
   other native herdr reporters) claim their own state at the Hook rank — no new
   `StateSource` variant on the wire. Unlike a hook claim, that claim ends on
   release/clear, child exit, or another agent's detection. Open:
   `notification.show` without a `pane_id` (needs peer-pid attribution), and
   Prime Agent, which reports to herdr as `prime-agent` (source `herdr:pi`),
   a kind with no registry row. Its process name is unverified: if it runs as
   `pi`, detection takes it for the registry's Pi (badlogic's pi-mono, a
   different product) and ends its first claim once; an alias would conflate
   the two, so it wants a registry row of its own once the name is known.
3. **Surfacing**: colored state badge per pane border + tab, workspace overview in
   navigate mode grouped by state (Blocked first), optional terminal bell / OSC 9
   desktop notification on `Working → Blocked/Done`. ✅ The go-to overlay
   (prefix `g`) opens on `needs you` / `done` / `working` groups above the tree.
   ✅ Tab badges: each app-bar tab segment trails the state dot of its most
   urgent agent (blocked > done > working; none for shells), with the sidebar's
   glyph and colour. ✅ Notifications pane: the daemon keeps the newest 200
   notes — every bell and desktop notification (herdr's `notification.show`
   too) and every attention transition (into Blocked, Working → Done) — each
   with the pane's title and workspace as they were; prefix `N` lists them
   newest first (enter jumps and marks read, `r` reads all, `X` clears after a
   confirm), the app bar's census ends with an unread `🔔 N` chip, a focus or
   `NotesRead` from anywhere clears the sidebar 🔔, and `tutti notes [--unread]
   | read | clear` serves scripts. Open: persist the history with the saved
   layout; carry a hook's Blocked message onto its note; fold a 7501 `msg` into
   the transition note it accompanies (today that is two notes).
4. **API**: `pane list --json` includes agent + state; emit state-change events on the
   socket event stream. ✅ For tools outside tutti (a notch app): `tutti watch`
   (the socket's `Watch`) streams whole-session snapshots, one per change,
   beside the attached TUI without touching it; a scripted `pane focus` jumps
   that TUI to the pane.

**Acceptance:** start a Claude Code task; badge shows Working; when it asks for
permission the pane flags Blocked within ~1s; when it finishes, Done until you focus it.
✅ Met — Phase 3 is done; what remains above is follow-up, not acceptance.

## Phase 4 — Agent-shaped API & orchestration

Goal: agents can drive Tutti — spawn siblings, wait on them, read their output.

1. **Wait conditions**: `tutti pane wait <pane> --until done|blocked|idle [--timeout]`
   blocks until the state transition (server-side subscription, not polling).
   ✅ `tutti pane wait <pane>... [--until working|blocked|done|idle] [--next]
   [--timeout <secs>]` blocks on a server-side subscription: a broadcast of pane
   signals fed by the `publish_transition` funnel, the reaper and every removal
   path, never polling. Level-triggered by default; `--next` counts only a
   transition into a state from outside it, so a Done → Idle focus is never a
   fresh finish. A child exit ends the wait: a matching final state answers,
   anything else is an error naming the exit code, and a wait for idle outlives
   the exit, since a look can still bring Idle. A killed pane is an error, and a
   timeout exits 124. A waiter whose client hangs up is cancelled: a
   connection's writer closes its queue once its reader sees the peer go. Open:
   a waiter lagging 256 signals behind re-checks level-triggered, so a `--next`
   edge can be lost under that load.
2. **Structured read**: `pane read --unwrapped` (logical lines, no soft wraps) and
   `--since` cursors so a supervisor agent can tail incrementally. ✅
   `--unwrapped` joins rows by vt100's own soft-wrap flags, so a full-width line
   stays its own line. ✅ `--since <cursor>` tails incrementally: the cursor
   counts rows committed to the primary scrollback, which the reader thread
   counts through vt100's scrollback offset; screen switches, resets and
   scroll-ups are fed as lone bytes, so the count stays exact through
   alt-screen trips and a full ring. The reply is the rows since the cursor,
   then the screen, plus `truncated`; on the alternate screen the cursor holds.
3. **Composite verbs**: `tutti run --workspace <ws> -- claude "fix the tests"` =
   create pane, run agent, return pane id — one call for orchestrators. ✅
   `tutti run [--workspace <ws> | --dir <path>] [--sandbox <p>] [--title <t>] --
   claude "fix the tests"` spawns into a fresh tab and prints the bare pane id.
   `--dir` reuses the workspace that has the directory open (compared
   canonically) or mounts it, using a fresh mount's own first tab; with neither,
   a run from inside a pane lands in that pane's workspace (`Run.from`, filled
   from `TUTTI_PANE` only in the pane's own session), else in the current one.
   The session's current tab stays put, so an attached TUI is not yanked and
   its prefix-`r` launches still land in the user's tab. A failed spawn rolls
   back its tab or mount. The human surface for the same thing is the sidebar's
   `n` (an agent in a fresh tab).
4. **Agent skill**: ship a `skills/tutti/SKILL.md` (Claude Code skill) documenting the
   CLI so any Claude instance inside a tutti pane can herd its own sub-agents.
   ✅ `skills/tutti/SKILL.md` ships the agent-facing CLI as a Claude Code skill:
   run, workspace add, pane wait, read, guarded answers, workspace diff,
   checkpoints, merge, cleanup, `--json`. Its rules: never your own pane, only
   what you created, wait `--until done,blocked` with a `--timeout`. `tutti
   skill [--raw]` prints it, and `tutti skill --install [--project] [--yes]` (or
   the setup panel) installs it with the hooks' confirm + backup flow, refusing
   a SKILL.md that is not tutti's. A test parses every command line in it
   against the CLI, so it cannot drift. ✅ It teaches `--since` tailing, `pane
   approve` for held approvals, that a run lands in the caller's own
   workspace, `tutti issue list|deploy`, and `tutti notes --unread` (marking
   and clearing notes stay the user's).
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

   ✅ CLI vocabulary reconciled: `tutti project add <dir> | list | kill <id>`
   and `tutti workspace add <project> --name <n> [-r <rev>] [--dest <path>] |
   list | kill | merge | update | diff`. `workspace list` names each entry's
   parent (`WorkspaceInfo.parent`, resolved exactly as the view's), and
   `project list` keeps the top-level entries. The old `workspace new --dir` /
   `workspace fork` spellings still parse, hidden, to the same requests. No
   help screen or CLI message says "fork"; the wire's `WorkspaceFork` keeps its
   name. Open: a `workspace add` whose `-r` names a missing revision leaves a
   half-made checkout (jj 0.46 registers the workspace before resolving the
   revision; `sandbox pull` checks first).
6. **Agent-event ingest + Claude Code hook integration**: ✅ panes get
   `TUTTI_PANE`/`TUTTI_SESSION` in their env; `tutti hooks claude` prints a hook
   config that makes the agent call `tutti agent-event claude` on subagent
   spawn/tool-use/stop and on permission notifications. Server attaches live
   subagent rows to the pane (shown indented under the agent in the sidebar) and
   upgrades Blocked/Done detection from screen heuristics to exact hook signals
   (a hook-driven pane is skipped by the screen classifier). Display only — tutti
   does not manage a foreign agent's subagents. ✅ Prompts (`UserPromptSubmit`
   starts a turn) and session starts (the conversation id before the first tool
   use); a `PermissionRequest` blocks the pane the moment Claude would ask
   (item 9); Notifications block only when they wait on you — idle reminders,
   auth and usage-limit reports don't, and an unknown type still blocks.
   Re-running `--install` upgrades an older install with just the missing
   events. ✅ opencode joins through a JavaScript plugin (`tutti hooks opencode
   [--install]`, same confirm + backup flow) that forwards session
   busy/idle/permission/error as `agent-event opencode`. ✅ Install logic for the
   hooks, the plugin and the skill lives in a side-effect-free library
   (`tutti::integrations`: `plan` → status, contents, summary and preview, then
   `apply` with a backup and a changed-since-inspected guard), shared by the
   CLI's `--install` flows and the TUI's setup panel (prefix `i`), which plans
   and installs each for the user or the active project. Open: Codex/Gemini
   integrations need their specs read, the tools installed, and a live run
   before they land.
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
   same via `tutti pane run -- <that command>` — no new verb needed. ✅ Each
   agent pane's conversation id comes from the hook payloads (Claude's
   `session_id`, opencode's root `sessionID`, a herdr report's
   `agent_session_id`) into `PaneInfo.conversation`, and
   `tutti_agents::resume_cmd(agent, id)` maps it to the tool's own resume argv
   (claude, muse, vibe, opencode `--session`). ✅ Wave 2: the id is saved with
   the layout, and a restore revives every agent pane that held one in exactly
   that conversation via its tool's recipe, under its old title; other panes
   come back as shells — saved commands are never re-run.
8. **Per-turn checkpoints**: ✅ each agent turn's end (a pane leaving Working,
   through the single `publish_transition` funnel in server.rs) checkpoints the
   workspace's jj `@` — the commit id the change probe's own snapshot already
   reads (`jj log -r @ -T commit_id`), so no extra jj operation. Where the agent
   started (on detection, or on first entering Working) is #1, so turn 1 can be
   undone. In daemon memory, newest 50 per pane, numbered per pane and never
   reused; saved with the layout, so a restored pane keeps its list, numbering
   and head. Surfaces: `tutti pane checkpoints | checkpoint-diff [N] [--stat] |
   restore <N> [--force]` and the changes panel's `turns` section (enter = turn
   diff, `r` = confirmed restore). Restores are refused mid-turn and when stale,
   and save the replaced state first as `before restore`. Sandbox panes record
   none: their agent edits the pod's checkout, and a checkpoint of the local one
   would offer to undo someone else's work. ✅ Turns are labelled with the first
   line of the prompt that started them (`Checkpoint.prompt`, from
   `UserPromptSubmit`; a blocked turn keeps its prompt for the work after you
   answer). Open: Esc interrupts a turn without a Stop, so its edits fold into
   the next turn's checkpoint under the next prompt.
9. **Answer from the queue**: ✅ `p` (or a right-click) on a blocked sidebar row
   opens a peek overlay of that agent's live prompt; answer keys go out as a
   guarded `PaneSend` whose `expect` is the prompt fingerprint (FNV-1a over the
   last 12 non-empty screen rows, one function in tutti-core shared by client
   and server). The daemon writes only while the pane is Blocked, the
   fingerprint still matches, and the pane has redrawn since the last guarded
   answer — so the first answer wins and a refusal writes nothing. Scripts get
   the same guard via `pane read --fingerprint` and `pane send --expect`.
   ✅ Hook-held approvals: a `PermissionRequest` hook (wired by `tutti hooks
   claude`) blocks the pane the moment Claude would ask, naming the tool and its
   input. With `approvals = "hold"` the hook holds the request on the daemon
   (`ApprovalHold`, answered by `ApprovalAnswer`) until it is answered from the
   waiting queue's approval overlay (`y`/`n`; `esc` passes to Claude's dialog;
   it auto-opens over the focused pane) or `pane approve`. It falls through to
   Claude's own dialog with no client attached, on timeout
   (`approval_timeout_secs`, default 120) or on hook hang-up. An allow or a deny
   sets the pane Working at once (Claude sends no hook until the tool is done);
   an overlay that opened itself over the pane in view ignores keys for 800 ms,
   so a `y`/`n` already on its way into the pane can't answer it. ✅ The sidebar and
   changes-panel keys are table-driven (`config::SIDEBAR_TABLE` /
   `CHANGES_TABLE`): dispatch, the help overlay and the footer hints all render
   from them. ✅ Replies carry their request's id: a control frame may carry a
   top-level `req` (`id` was taken by variants' own fields), which the daemon
   echoes on that request's answer, async ones included; the TUI keys its
   add-project, workspace, merge, sandbox-pull and issue-deploy follow-ups on it
   (`App::follow`), so a refused answer cancels only its own request's
   follow-up. Open: one held approval per pane; Blocked lasting through an
   approved tool's run when it was answered in Claude's own dialog (Claude has
   no granted-permission hook); an opencode hold path.
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
   ctrl-c mid-provisioning each delete the sandbox. ✅ The three opens landed:
   hooks report from the pod over OSC 7501 (`tutti hooks claude --tty`: plain
   `printf` to `/dev/tty`, which `kubectl exec -it` carries back); `inject_env`
   puts `TUTTI_GIT_REMOTE` and `TUTTI_BRANCH` on a warm-pool claim (the template
   must allow it, a refusal fails the pane at once, and a claim with env gets a
   fresh pod rather than the warm one); and the daemon names each sandbox
   (`PaneInfo.sandbox_name`) and tells it `TUTTI_BRANCH=tutti/<name>`, which
   `SandboxPull` (CLI `sandbox pull`, TUI `p` on the corpse) fetches, tracks and
   checks out as a nested workspace for diff and merge. The wrapper's `--name`
   is optional, so a daemon from before sandbox names still launches it
   mid-upgrade. Open: confirm with a real Claude in a pod that hook commands
   keep the session's tty, and a name prompt for the TUI pull.
11. **Deploy tracker issues**: ✅ `[[issues]]` sources — GitHub through the
   user's `gh`, Linear's GraphQL API through `curl` with a personal API key
   from the environment (handed to curl on stdin, never on a command line), any
   tracker through a command printing JSON — feed a prefix-`I` issues panel
   (fetched on threads, never on the event loop; the daemon never talks to a
   tracker) and `tutti issue list|deploy`. A deploy creates a jj workspace
   named for the issue (or a fresh tab with `--here`/`h`, and always for a
   non-jj project), runs an agent whose registry row says how it takes a first
   message (`prompt_flags`: claude, codex, gemini, opencode, muse, vibe, ori;
   crush takes none) on the issue's rendered prompt, and links the pane
   (`Run.issue` → `PaneInfo.issue`, shown in the sidebar and pane title, kept
   by a revived agent across a restore). The TUI deploy's fork and run ride
   `req`-keyed follow-ups. Read-only toward trackers. Open: write-back (comment
   the link, move the issue's state); carry the link in watch snapshots; a
   Linear run with a real key; prompt-passing data for the unverified catalog
   agents; a prompt past Linux's 128 KiB single-argument limit fails the spawn.

**Acceptance:** a script (or an agent) can: create 3 worktree workspaces, launch an
agent in each, `wait --until done` on all three, read back each diff summary.
Runnable now that items 1 and 3 have landed (`workspace add`, `run
--workspace`, `pane wait --until done,blocked`, `workspace diff --stat`); open
until it has been run for real against live agents.

### Next waves (as of 2026-10-11)

In order; each lands its TUI path in the same wave as its verbs.

1. ✅ **Finish Phase 4** — `pane wait`, `pane read --since`, `tutti run`
   (`--workspace`/`--dir`, or beside its caller) and the agent skill landed.
   Open: close the wave by running the acceptance script above for real,
   against live agents.
2. ✅ **State follow-ups** (Phase 3 item 2) — typing ends a Done/Idle program
   claim; `OSC 9;N` subcommands never surface as notifications.
3. ✅ **Debts against the ground rules** — a TUI path for installing the hooks,
   the plugin and the skill (the setup panel, prefix `i`); table-driven sidebar
   and changes-panel keymaps, so CLAUDE.md's "everything renders from the live
   keymap table" holds for the panels too; replies correlated with their
   requests (`req`), so a refused answer can't cancel a pending fork/merge
   follow-up.
4. ✅ **Survive a daemon restart** — the layout, each pane's conversation id
   and its checkpoint list share one small state file, offered back on attach.
5. **Candidates from prior art** — ✅ herdr-compatible reporting (Crush reports
   with no install step); ✅ hook-held approvals answered from the queue. Open:
   Codex and Gemini hook integrations — read each spec, install the tool, and
   verify with a live run.
6. ✅ **Connections** — several clients attach (the smallest viewer sizes each
   pane), and the daemon and client survive panics and a wedged peer (Phase 2
   item 1, Phase 5).
7. ✅ **Notifications pane** — a 200-note daemon-side history behind prefix `N`
   and `tutti notes` (Phase 3 item 3).
8. ✅ **Issue dispatch** — `[[issues]]` sources behind prefix `I` and `tutti
   issue list|deploy` (Phase 4 item 11).
9. ✅ **Editor interop (wave 7)** — Neovim and Emacs in panes, wire rev 10:
   - OSC 1337 user vars on `PaneInfo.user_vars`, in `pane list --json` and
     `watch` (16 a pane, names `[A-Za-z0-9_]{1,64}`, values ≤ 1 KiB, empty
     unsets, never persisted). `TUTTI_KEYS=pane` hands the direct chords to the
     pane; `TUTTI_EDITOR` names where `tutti edit` and the changes panel's `e`
     open a file, through the editor's own remote CLI (the daemon only names
     the pane, `Request::View`).
   - `pane focus|resize --direction`, from `--from`, else `$TUTTI_PANE`, else
     the attached TUI's focus, on `Layout::neighbor`: the TUI's rule on the
     unit square, `--wrap` inside the tab.
   - `editors/nvim` (user vars through `nvim_ui_send`, `navigate` past
     Neovim's edge, a smart-splits.nvim 3.0.0 backend, `:TuttiAgents`) and
     `editors/emacs/tutti.el` (`tutti-mode`, `tutti-windmove-*`, a live
     `tutti-agents` with guarded answers and held approvals).
   - Shell integration: `tutti shell-init zsh|bash|fish` and a setup-panel
     row; OSC 7 cwd (`PaneInfo.cwd`; a split starts there when it lies inside
     the workspace); OSC 133 marks read at exact positions by the run-splitting
     `Seq` scanner, for `pane read --last-output`; a prompt start clears the
     editor vars, so a crashed editor hands the keys back.
   - `tutti pane attach` (`PaneAttach`, `PaneRaw` frames): a snapshot, then
     every pty read, taken under one hold of the parser lock; a resize resends
     the snapshot; a stream that falls behind is dropped with an error; it
     votes in the pane's size. The CLI draws on the alternate screen and
     redraws a pane program's own screen switches from a vt100 mirror.
   - The daemon answers DA1/DA2/DSR/CPR always (DA2 as xterm patch 277, the
     version from which Emacs asks for colours), OSC 10/11/12 and `CSI ? 996
     n` from the newest client colour report (`HostColors`), and owns mode
     2031 (a light/dark change is told to the panes that set it) and
     modifyOtherKeys (dropped: panes get legacy keys, and a `pane attach`
     terminal keeps its prefix); answers go out through a per-pane thread,
     and raw streams carry only what it did not answer. Every pane runs with
     `COLORTERM=truecolor`.
   Verified live with nvim 0.12.4 and Emacs 32 in zsh panes, under the TUI
   and `pane attach` (the w7-finish acceptance, 76 checks), and `pane attach`
   inside a tmux that honours modifyOtherKeys. Open:
   - terminal fidelity, for the next wave's own engine: undercurl and
     underline colour flattened, the cursor shape (DECSCUSR) dropped, no focus
     events;
   - the TUI's live light/dark: crossterm 0.29 swallows `CSI ? 997 n`, so it
     reports colours at attach only;
   - `pane attach --inline` (primary screen, switches passed through) so
     editor terminals keep native scrollback; detach under the kitty keyboard
     protocol; escape-time for an Alt prefix; a switching read's other bytes
     (queries, a bell) are lost to the redraw;
   - an editor holding the keys loses the TUI's edge moves (sidebar, next tab):
     `pane focus` at an edge could hand the move to the attached TUI;
   - clicks and drags never reach a pane program, only the wheel;
   - `cwd` in watch snapshots; `pane list` has no workspace and watch no
     approval, so the plugins read one from each;
   - fish's snippet unrun; bash before 4.4 has no `C` mark; Emacs 29 and
     Neovim before 0.12 unrun;
   - XTVERSION, DECRQM beyond 2031 and kitty's `CSI ? u` go unanswered without
     a raw stream.

**Open** — the headline items; each phase item above keeps its own list:
- the notification history is not persisted — a restarted daemon starts it
  empty;
- no tracker write-back: a deploy never comments on, assigns, or moves its
  issue;
- Prime Agent reports to herdr as `prime-agent`, a kind with no registry
  row, and its process name is unverified (if it is `pi`, detection takes it
  for Pi, a different product);
- a spawned request task (merge, fork, diff, wait, …) that panics leaves its
  caller waiting: the daemon logs it, but the CLI has no client-side timeout;
- under heavy parallel test load `openpty` occasionally fails a test's first
  spawn (`Unknown error: -6`); a rerun passes;
- Codex and Gemini integrations need the tools installed and a live run;
- an editor in a pane misses vt100's gaps (undercurl, cursor shape, focus
  events) until the next wave replaces vt100 with tutti's own engine.

**Deliberately not done:** packaging and distribution (Phase 5) — the project
is private, so install stays `cargo install --path` from a checkout.

**Unverified live** — cover these in the acceptance run:
- checkpoints through a real Claude Code turn (verified only with synthetic
  hook JSON) and through the screen-classifier, agent-exit and focus paths;
  `jj util gc` pruning a checkpoint's commit; a turn-end probe racing a
  restore;
- whether Claude Code emits its own OSC 9;4 (a hook-less claude pane would
  then hold off `Done` until the progress clears);
- opencode's question/error/retry events and `hooks opencode --install
  --project`;
- prompt labels and conversation ids from a real Claude Code and opencode
  (checked against the hooks reference and the opencode 1.18.5 bundle), and
  whether Claude fires `UserPromptSubmit` for a message typed mid-turn (if it
  does, the newest prompt relabels the running turn);
- a restore reviving a real `claude --resume` / `opencode --session` (proven
  with a stand-in recipe);
- `approvals = "hold"` against a real Claude: what its UI shows while the hook
  holds, that its own dialog stays back until `esc` or the timeout, and that a
  timed-out hold falls through to it;
- a real Claude in a sandbox pod running the `hooks claude --tty` commands with
  the session's tty, so `/dev/tty` resolves (checked only by typing the
  commands into a busybox pod);
- Linear's issue data with a real key (the request documents validate against
  the live API; the replies are covered only by fixtures).

## Phase 5 — Hardening & distribution

- Config file: `~/.config/tutti/config.toml` (prefix key, mouse on/off, default shell,
  agent registry overrides). `theme = terminal|dark|light` landed with it.
- Crash-safety: server panics must not kill child PTYs where avoidable; client
  crashes never harm the server. ✅ A vt100 panic on a pane's reader thread
  restarts that pane's screen blank instead of killing the reader or poisoning
  the lock every tick takes (the trigger found: a wide character printed into a
  one-column pane), a mirror vt100 cannot replay falls back to a blank screen
  (and clients get that blank snapshot rather than the bytes that would panic
  theirs), and every pty lock recovers from poisoning. ✅ The hub's locks
  recover from poisoning too; the render tick, VCS watch, classify and detect
  passes survive their panics (logged, the loop goes on); a panicking frame
  drops only its connection, which leaves through `drop_client` like any
  other; a failed accept is logged and retried. The detached daemon appends
  its stderr to `<session>.log` beside its socket (trimmed to its newest MiB
  past 4 MiB, at start), and `server start` prints the path. The TUI reseeds a
  pane whose snapshot or delta panics its parser, its panic hook restores the
  terminal, and `agent-event` gives a wedged daemon 2 s. Open: a spawned
  request task that panics leaves its caller waiting (no client-side timeout);
  the log is trimmed only when a daemon starts.
- Perf targets: 60fps redraw with 8 busy panes; `pane read` on 10k-line scrollback
  < 50ms; input latency indistinguishable from raw terminal. ✅ The render tick
  keeps scrollback-free mirrors (visible state formatted under the lock and
  replayed) instead of cloning each screen's 10k ring twice: per changed pane
  1.5 ms → 24 µs at 80x24 and 5.2 ms → 98 µs at 200x50 (release), so 8 busy
  panes take about 0.2 ms / 0.8 ms of the 16 ms frame. `pane read` over a full
  10k ring takes 2.8 ms in release, held by a debug-build test against the 50 ms
  target.
- Test matrix: macOS + Linux; kitty/alacritty/wezterm/iTerm2/Terminal.app/ghostty;
  sandbox panes against a local cluster running agent-sandbox (unit tests cover
  the manifests, the cluster run covers the cleanup paths). Open: under heavy
  parallel test load `openpty` occasionally fails a test's first spawn
  (`Unknown error: -6`); a rerun passes.
- Packaging: `cargo install tutti`, Homebrew tap, curl installer. Windows deferred.
  Deliberately not done: the project is private, so install stays `cargo
  install --path` from a checkout. (Should that change, `cargo package` needs
  `skills/tutti/SKILL.md` copied into the `tutti` crate — its `include_str!`
  reaches outside it today.)

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
