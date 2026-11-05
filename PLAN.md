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
  binary escape-sequence frames — snapshots and redraws from the daemon's
  terminal engine (see `docs/transport-decision.md`). The `tutti` binary
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
  Since wave 8 the panes run on `rtty-vt`, our own engine (the rtty repo's
  `vt/`, on `vte`'s byte parser), in the daemon and the TUI alike.
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
   JSON control messages, server-computed vt100 escape deltas for pane content
   (since wave 8, rtty-vt's redraws), snapshot-on-attach. Full analysis in
   `docs/transport-decision.md`.
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
   loop feeds a per-pane `vt100::Parser` (screen grid + scrollback ring buffer;
   an `rtty_vt::Terminal` since wave 8); handle resize; reap dead children and
   mark panes exited.
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
   parse is set aside as `.restore.json.bad`. ✅ The save also holds the
   notification history and each pane's in-flight prompt (wave 9). A starting
   daemon takes the history back at once, its new panes taking ids past every
   one a note names; a restore points each note at the pane it came back as,
   and closes a turn left in flight as a `restart` checkpoint under its
   prompt. Known limitation, deliberate: a session with no workspaces at all
   saves nothing, so its notification history is lost at the next restart.
   ✅ A restore's rebroadcast history marks the panes its unread notes came
   back as at once (wave 11).

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
   the command at spawn. ✅ A `node` process is named by its script
   (`tutti_agents::program_name`, from the argv the walk now reads), so Gemini
   CLI (`node …/gemini`) reads as `gemini`. ✅ A node flag's value given as
   the next argument (`--require x`) is skipped: the value-taking flags `node
   --help` lists, but for the profiler, report, test-runner and TLS ones
   (wave 11). ✅ Cursor CLI under its documented name, `agent` (wave 12).
   Its installer links `agent` and the legacy `cursor-agent` to a bash
   script that execs its bundled `node` on `…/cursor-agent/versions/<v>/
   index.js`, so the process was never `cursor-agent`. Detection now
   matches that script by its path's end, never the bare name `agent`: a
   process name with a `/` matches a path's end, with `*` standing for any
   one directory. A spec's `binaries` are what the launcher looks for on
   PATH, the preferred one first. One that detection doesn't know by name
   (`agent`) counts only if it resolves to a file detection knows. The
   launcher and `issue deploy` run `agent` where it is Cursor's, by the path
   they checked (the daemon spawns on its own PATH), else `cursor-agent`,
   and a sandbox runs `cursor-agent`, the name its pane's declared agent is
   read from.
2. **State classification** per pane, using layered signals (most→least reliable):
   - agent-specific integrations (e.g. Claude Code hooks writing to a tutti-known
     location, or parsing its status line)
   - screen-content heuristics on the pane's grid (prompt patterns like `❯`,
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
   release/clear, child exit, or another agent's detection. ✅ A
   `notification.show` naming no pane lands on the pane whose process sent
   it: the socket's peer pid (tokio `peer_cred`: LOCAL_PEEREPID /
   SO_PEERCRED), walked up its parents to a pane's own process; one from
   outside every pane is refused as before (`pane_id is required`). ✅ Prime
   Agent has its own registry row, `prime-agent`: its installer's launcher
   execs `share/prime-agent/prime-agent` (the Rust port's `[[bin]]`), so it
   never reads as Pi, and detection catching up keeps its `herdr:pi` /
   `prime-agent` claim. Open: Prime Agent reports `agent_session_path`, not
   an id, so its pane holds no conversation to revive (`prime-agent
   --resume` takes a path or id); its screen patterns are guesses.
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
   | read | clear` serves scripts. ✅ The history is saved with the layout and
   back as the next daemon starts (Phase 1 item 5). A transition's note
   carries what came with it — a hook's blocked message as its body, an OSC
   7501 or herdr report's `msg` under its app — one note, not two; a note
   carrying text keeps its pane's 🔔 as a notification does. ✅ A permission
   request held on a pane already Blocked makes a note of its own (wave 11).
   ✅ So does any other blocked event there — a request in watch mode, an MCP
   elicitation, a question — but Claude's `permission_prompt` Notification,
   which comes about six seconds into a dialog whose PermissionRequest was
   reported already: `agent-event` marks it a reminder
   (`AgentHookEvent::Blocked.reminder`, wire rev 15), which makes no note on
   a pane already blocked and is noted when it is itself the move into
   Blocked (wave 12). Open: a hooks install without PermissionRequest (one
   from before it, never re-run) notes a second dialog's reminder in the
   same blocked spell not at all; so does a sandboxed command's network
   prompt while the pane is blocked, if no PermissionRequest reports those;
   and a reminder arriving after its dialog was answered and the pane went
   back to work blocks the pane again, with a note, as before wave 12.
4. **API**: `pane list --json` includes agent + state; emit state-change events on the
   socket event stream. ✅ For tools outside tutti (a notch app): `tutti watch`
   (the socket's `Watch`) streams whole-session snapshots, one per change,
   beside the attached TUI without touching it; a scripted `pane focus` jumps
   that TUI to the pane. ✅ Snapshots carry each pane's `cwd`, its held
   `approval` (the oldest) and its `issue` link, and `pane list` names each
   pane's `workspace` id (wire rev 12).

**Acceptance:** start a Claude Code task; badge shows Working; when it asks for
permission the pane flags Blocked within ~1s; when it finishes, Done until you focus it.
✅ Met — Phase 3 is done; what remains above is follow-up, not acceptance.

## Phase 4 — Agent-shaped API & orchestration

Goal: agents can drive Tutti — spawn siblings, wait on them, read their output.

1. **Wait conditions**: `tutti pane wait <pane> --until done|blocked|idle [--timeout]`
   blocks until the state transition (server-side subscription, not polling).
   ✅ `tutti pane wait <pane>... [--until working|blocked|done|idle] [--next]
   [--timeout <secs>]` blocks on a server-side subscription: a queue of its
   pane's signals fed by the `publish_transition` funnel, the reaper and every
   removal path, never polling. Level-triggered by default; `--next` counts only a
   transition into a state from outside it, so a Done → Idle focus is never a
   fresh finish. A child exit ends the wait: a matching final state answers,
   anything else is an error naming the exit code, and a wait for idle outlives
   the exit, since a look can still bring Idle. A killed pane is an error, and a
   timeout exits 124. A waiter whose client hangs up is cancelled: a
   connection's writer closes its queue once its reader sees the peer go. ✅
   Each wait's queue is its own and unbounded (`Hub.waits`, `tell_waits`), so
   a wait however far behind its pane still answers on its edge. The
   256-signal broadcast it replaced dropped the edge of a wait 256 moves
   behind and answered from a level re-check (tested at 255, 256 and 4096
   behind).
2. **Structured read**: `pane read --unwrapped` (logical lines, no soft wraps) and
   `--since` cursors so a supervisor agent can tail incrementally. ✅
   `--unwrapped` joins rows by the terminal's own soft-wrap flags, so a
   full-width line stays its own line. ✅ `--since <cursor>` tails
   incrementally: the cursor counts rows committed to the primary scrollback —
   the engine's absolute row numbers (`screen_top`), exact through alt-screen
   trips, resets and a full ring (before wave 8, a count kept through vt100's
   scrollback offset). The reply is the rows since the cursor, then the screen,
   plus `truncated`; under the alternate screen, the committed rows then that
   screen. A resize reflows and may number kept rows afresh: a cursor sees
   rows again, never skips one. ✅ `--last-reply` (wave 10, wire rev 13): the
   final message of the pane's last finished turn, as its turn-end hook names
   it — Claude's and Codex's `Stop` `last_assistant_message` (live on 2.1.318
   and 0.166.0), Gemini's `AfterAgent` `prompt_response` (docs and 0.66.0
   source; the whole turn's text), and opencode's last assistant message,
   which tutti's plugin fetches with `client.session.messages` at idle (live
   on 1.18.5) — carried on `AgentHookEvent::Done { reply }`, kept per pane
   (64 KiB, never persisted). Never a scrape of the screen: no hooks, no
   finished turn, or a turn that ended without one is an error saying which.
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
   `n` (an agent in a fresh tab). ✅ `tutti ask <pane> <message> [--timeout]`
   (wave 10), for one agent to talk to another: the daemon types the message
   only into a pane whose agent has reported through tutti's hooks since it
   started, and only between turns — not working, blocked or exited —
   checked under the same session guard as the write (`PaneSend {
   between_turns }`, answered `Sent` with the pane's count of hook-reported
   turn ends). `ask` presses enter 150 ms later (Codex 0.166.0 takes text and
   an enter that land together as a paste) and waits for a block or for that
   count to pass (`PaneWait { reply_after }`), so a turn end before the wait
   still counts and no exit, screen state or Idle → Done edge passes for one.
   It prints the reply; exit 3 when the agent blocks, 124 on timeout, 1 when
   the agent exits or the turn ended without a reply. Claude asking Codex ran
   live, and a Codex killed mid-ask reported its exit. The human surface is
   the sidebar's `p` on a done agent: a read-only reply overlay.
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
   name. ✅ A `-r` naming no commit — missing, or a revset resolving to none —
   is refused before `jj workspace add` runs (`jj::fork`); jj 0.46 registers
   the workspace and makes its checkout before it resolves the revision.
   ✅ An add that fails after that point cleans up exactly what it made
   (wave 12): tutti makes the destination itself (`create_dir`, never over
   anything), then on a failure forgets the name only while `jj workspace
   root --name` records it at that very directory, and removes the
   directory — a same-named workspace elsewhere, or another recorded at the
   same path, is left alone. An add past its 15 s is given up on but left a
   minute to end on its own, then undone whole: jj 0.47 makes a Git worktree
   per workspace in a colocated repo, and one killed halfway (measured: from
   about 10 ms into a 130 ms add to its registering the path) leaves that
   worktree registered, and git then refuses the next add there. jj 0.47.1
   drops the root from a merge's parents, so `-r 'root() | @'` no longer
   fails; the tests refuse an add with git's own stale-worktree refusal
   instead. Open: a jj still running after that minute is killed, and what it
   leaves (a Git worktree, a name with no recorded path) stays.
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
   auth and usage-limit reports don't. ✅ Only the types the hooks reference
   says wait on you block (permission, MCP form and URL dialogs,
   `agent_needs_input`, `quota_auto_resume_stale`), and an untyped one from an
   older Claude; a mod's `plugin_notification` and a type tutti does not know
   yet do not (wave 12). Open: a mod's notice lands in no note.
   Re-running `--install` upgrades an older install with just the missing
   events. ✅ opencode joins through a JavaScript plugin (`tutti hooks opencode
   [--install]`, same confirm + backup flow) that forwards session
   busy/idle/permission/error as `agent-event opencode`. ✅ Install logic for the
   hooks, the plugin and the skill lives in a side-effect-free library
   (`tutti::integrations`: `plan` → status, contents, summary and preview, then
   `apply` with a backup and a changed-since-inspected guard), shared by the
   CLI's `--install` flows and the TUI's setup panel (prefix `i`), which plans
   and installs each for the user or the active project. ✅ Codex (verified
   live against codex-cli 0.166.0) and Gemini CLI (0.66.0) join through
   `tutti hooks codex|gemini [--install [--project]]` → `tutti agent-event
   codex|gemini`, with the same plan/confirm/backup flow and setup-panel rows.
   Codex's hooks speak Claude's schema: Interrupt ends a turn (no Stop follows
   it), PermissionRequest blocks, and its hooks must be trusted once in
   `/hooks`. Codex runs embedded (`--no-daemon`) in panes tutti launches: its
   shared app-server runs every session's hooks with the env of whichever
   codex started it (observed: another pane's events landed on the first), so
   `agent-event codex` drops an event unless the session's daemon is its
   ancestor. A Gemini Notification blocks only for a `ToolPermission`. ✅
   Claude Code 2.1's subagent tool is `Agent`, not `Task`; both start a
   subagent row. Open: a codex typed by hand without `--no-daemon` runs in the
   shared app-server, so the guard drops its hook events and the pane falls
   back to the screen (no turn labels, no conversation); the guard fails open
   when the session's pid file is missing. ✅ Claude's `StopFailure` ends the
   turn as done with no reply and `PostToolUseFailure` reads as activity, in
   `agent-event` and the `--tty` hooks (wave 11; re-run `hooks claude
   --install`).
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
   come back as shells — saved commands are never re-run. ✅ Codex (`codex
   resume --no-daemon <id>`, verified live) and Gemini CLI (`gemini --resume
   <id>`, per its docs) resume recipes, with their hooks naming the id;
   neither store is harvested yet.
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
   answer). ✅ A turn interrupted with Esc (Claude sends no Stop) closes as
   `interrupted`, under its own prompt, when the next `UserPromptSubmit`
   arrives with a new `prompt_id` and no Stop since; more typed into a
   running turn (same `prompt_id`) stays in it under its first prompt. Open:
   the pane reads working from the Esc until that next prompt, and edits made
   in between join the interrupted turn; Codex's and Gemini's prompts carry
   no id, so input typed into their running turn splits it as `interrupted`.
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
   follow-up. ✅ A pane holds every request its agent raises
   (`PaneInfo.approvals`, oldest first; `approval` stays as the oldest for
   older clients), each on its own waiting row in arrival order, answered on
   its own (`pane approve --id`) with its own timeout; the pane stays blocked
   until the last, or while one passed to the agent's own dialog waits, and
   an exit drops what it held. ✅ Answered in Claude's own dialog, the pane
   leaves Blocked once its screen, having shown the dialog, shows Claude busy
   again (`· ↓ … tokens` spinner, `Running`) — the hooks reference has no
   event between that answer and `PostToolUse`. ✅ opencode holds too: its
   plugin sends `permission.asked` through `agent-event` and posts tutti's
   answer as opencode's `/permission/{id}/reply`; an answer in opencode's own
   dialog drops tutti's hold. Open: that dialog-to-Working gate relies on
   Claude 2.1.318's spinner marker; opencode's dialog shows beside a held
   request (1.18.5 never calls the plugin's `permission.ask`), and the reply
   goes through the plugin client's undocumented `_client`.
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
   `req`-keyed follow-ups. ✅ Write-back, opt-in per source: once the agent
   runs, the deploying client comments where it runs (`comment = true`:
   workspace and project, pane and session, a sandbox's branch — never a
   local path), assigns and labels the issue on GitHub (`assign`, `label`
   through `gh issue edit`) and moves it to a named workflow state on Linear
   (`state`). The CLI exits 1 after the pane id if the tracker refuses; the
   TUI writes back on a thread, with a flash. The daemon still never talks to
   a tracker. ✅ Watch snapshots carry the link (`WatchPane.issue`). ✅ A
   prompt of 128 KiB or more (Linux's MAX_ARG_STRLEN) goes to a 0600 file in
   the temp dir that the agent's first message names; tutti never deletes
   that file, so the agent can come back to it, and the OS's temp cleanup
   takes it; a sandbox deploy of one is refused. ✅ Every catalog row says how
   it takes a first message and from where that is known: Pi joins from its
   `--help`; Goose, Qwen Code, Cursor CLI, Copilot CLI, Factory Droid, Augment
   Code and Amazon Q from their docs; Crush, Aider and Amp take none. ✅ The
   native file inputs carry long prompts (wave 12), each checked in its
   docs. From 16 KiB (`FILE_PROMPT`, an eighth of the per-argument limit)
   a first message rides an agent's own file input:
   - Ori: `--interactive --prompt-file`.
   - Goose: `run --interactive --instructions`.
   - Pi and Augment: `@<file>` beside a message pointing at it.

   The 128 KiB fallback's file, and these files, are now written 0600 into
   `<session>.prompts` beside the socket (0700), not the temp dir. The
   daemon removes the files a pane's argv names once its program exits,
   killed or not, and clears the directory as it starts and stops. A
   restore never re-runs a first message. `issues::deploy_argv` builds the
   argv for `issue deploy` and the TUI's armed launcher alike. No wire
   change. Open: a prompt starting with `-` reads as an option to an agent
   that takes it as a plain argument (Claude, Codex, Pi, Vibe, Muse, Droid,
   Augment, Cursor); only Pi's docs name `--` to end its options.

**Acceptance:** a script (or an agent) can: create 3 worktree workspaces, launch an
agent in each, `wait --until done` on all three, read back each diff summary.
Runnable now that items 1 and 3 have landed (`workspace add`, `run
--workspace`, `pane wait --until done,blocked`, `workspace diff --stat`); open
until it has been run for real against live agents.

### Next waves (as of 2026-11-04)

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
   Since wave 9 the notification history and an in-flight turn's prompt
   survive too.
5. ✅ **Candidates from prior art** — herdr-compatible reporting (Crush reports
   with no install step); hook-held approvals answered from the queue; Codex
   and Gemini CLI hook integrations (wave 9, Phase 4 item 6).
6. ✅ **Connections** — several clients attach (the smallest viewer sizes each
   pane), and the daemon and client survive panics and a wedged peer (Phase 2
   item 1, Phase 5).
7. ✅ **Notifications pane** — a 200-note daemon-side history behind prefix `N`
   and `tutti notes` (Phase 3 item 3). Since wave 9 the history survives a
   restart and a transition's note carries its message.
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
     unit square, `--wrap` inside the tab. At a left or right edge of the pane
     an attached TUI shows focused, the TUI is handed the move
     (`Event::FocusEdge`) and goes on as its own Ctrl+h/l would (sidebar,
     changes panel, next tab); the CLI exits 0 (`Neighbor.relayed`).
   - The mouse reaches a pane program that turned reporting on: presses,
     releases and drags (and bare motion under 1003) inside the pane, in its
     encoding at its own cells, a press focusing the pane too; Shift keeps
     the mouse tutti's (select and copy); border drags, the chrome and the
     wheel's routing stay tutti's.
   - `editors/nvim` (user vars through `nvim_ui_send`, `navigate` past
     Neovim's edge, a smart-splits.nvim 3.0.0 backend, `:TuttiAgents`) and
     `editors/emacs/tutti.el` (`tutti-mode`, `tutti-windmove-*`, a live
     `tutti-agents` with guarded answers and held approvals).
   - Shell integration: `tutti shell-init zsh|bash|fish` and a setup-panel
     row; OSC 7 cwd (`PaneInfo.cwd`; a split starts there when it lies inside
     the workspace); OSC 133 marks read at exact positions (since wave 8, the
     engine's own marks, which travel with their rows through a reflow), for
     `pane read --last-output`; a prompt start clears the editor vars, so a
     crashed editor hands the keys back.
   - `tutti pane attach` (`PaneAttach`, `PaneRaw` frames): a snapshot, then
     every pty read, taken under one hold of the parser lock; a resize resends
     the snapshot; a stream that falls behind (1 MiB of output queued) has
     its backlog dropped for a fresh snapshot and streams on (wave 10); it
     votes in the pane's size. The CLI draws on the alternate screen and
     redraws a pane program's own screen switches from a mirror (rtty-vt
     since wave 8).
   - The daemon answers DA1/DA2/DSR/CPR always (DA2 as xterm patch 277, the
     version from which Emacs asks for colours), OSC 10/11/12 and `CSI ? 996
     n` from the newest client colour report (`HostColors`), and owns mode
     2031 (a light/dark change is told to the panes that set it) and
     modifyOtherKeys (kept from raw streams, so a `pane attach` terminal
     keeps its prefix; since wave 8 the TUI encodes keys by it); answers go
     out through a per-pane thread,
     and raw streams carry only what it did not answer. Every pane runs with
     `COLORTERM=truecolor`.
   Verified live with nvim 0.12.4 and Emacs 32 in zsh panes, under the TUI
   and `pane attach` (the w7-finish acceptance, 76 checks), and `pane attach`
   inside a tmux that honours modifyOtherKeys. Open:
   - ✅ terminal fidelity in the TUI (wave 8 step 2, on rtty-vt): underline
     styles and colours, Neovim's undercurl included; the cursor shape
     (DECSCUSR); focus events; hyperlinks; clusters;
   - ✅ the TUI's live light/dark (wave 8): the TUI reads its terminal
     itself (`attach/reader.rs`) — keys as crossterm made them, kitty's and
     modifyOtherKeys reports, SGR/X10/urxvt mouse, bracketed paste (now on:
     a paste reaches a pane whole), focus, and the replies it asks for (OSC
     10/11, DA1, kitty's flags, XTVERSION, DECRPM); on a `CSI ? 997` report
     it recolours and hands the daemon fresh colours. crossterm only draws;
   - ✅ `pane attach --inline` (wave 9): the primary screen, a program's
     screen switches passed through, so an editor's terminal keeps native
     scrollback. Attach first scrolls the lines on screen, to the cursor's (a
     CPR), up into the scrollback; each snapshot (attach, resize) and a reset
     are drawn from the mirror's `snapshot()`, both screens, less the
     terminal's own modes; detach leaves a program's alternate screen and
     puts the cursor under the pane's last text. Verified in Neovim 0.12.4's
     `:terminal`. ✅ wave 10: the pane's history from before the attach is
     printed first, once — up to `--history ROWS` (default 1000) of its
     primary scrollback's newest rows, as logical lines the editor's terminal
     wraps itself (rtty-vt's `Terminal::history`) — and a redraw erases with
     ED 0, so a terminal that scrolls ED 2 into its scrollback keeps no copy
     of the screen per resize (verified with a tutti pane as the host);
   - ✅ the escape time (wave 9): `escape_time_ms` (default 15, 0–1000),
     tmux's escape-time — a lone ESC waits it for the key after it, which
     goes with it as Alt — in the TUI's reader and in `pane attach`, which
     holds the ESC (poll(2) on stdin) and sends both in one write, an
     `A-…` prefix included. A lone Esc reaches a pane 15–20 ms after it is
     typed at the default;
   - ✅ detach under the kitty keyboard protocol: the daemon keeps the
     protocol (push, pop, set; its query it answers) from raw streams as it
     does modifyOtherKeys, so no pane turns it on in a `pane attach`
     terminal;
   - ✅ `pane attach` lost the rest of a read that switches screens to its
     mirror redraw (a program's startup queries, a bell). Fixed with the
     engine swap: the daemon answers those queries itself, and hands a
     stream each switch of screens (or reset) in a frame of its own
     (`a_screen_switching_read_loses_nothing_on_a_stream`);
   - ✅ `cwd` and the held `approval` in watch snapshots, the workspace id in
     `pane list` (wave 9, wire rev 12); the Emacs agents list reads
     approvals from its watch like the rest, and `:TuttiAgents` already read
     only the watch. ✅ The Emacs agents list answers the approval it showed,
     by its id (wave 11); Neovim's `:TuttiAgents` only focuses;
   - fish's snippet unrun; bash before 4.4 has no `C` mark; Emacs 29 and
     Neovim before 0.12 unrun;
   - ✅ XTVERSION, DECRQM beyond 2031, DECRQSS, XTGETTCAP and kitty's `CSI ?
     u` are answered by the engine since wave 8.
10. ✅ **Own terminal engine (wave 8)** — rtty-vt, the rtty repo's `vt/`, under
    tutti and rtty alike:
   - ✅ Step 1, the daemon: each pane is an `rtty_vt::Terminal`. Gone: the
     scroll-count offset trick, the run-splitting `Seq`, the window tiling,
     `NotifyScanner`, `Mirror` and its replay, the hub's per-pane vt100
     parsers and the reader's restart-blank `catch_unwind`, for a pane fault
     boundary that ends the pane (Phase 5). Rows are the engine's absolute
     numbers; events (bell, OSC 9/777, 9;4, 7501, 1337, 133) are its own,
     tutti keeping its 7501 decoding and caps; the render tick takes each
     pane's damage once (`look`); answers are the engine's `Event::Reply`,
     through the same answer thread, and a scanner that follows vte keeps
     from the streams exactly what was answered, mode 2031, and the keyboard
     protocols. Wire unchanged (rev 10). Release, a pane draining 50 MB of
     dense SGR: daemon CPU 0.68 s → 0.34 s; a full 10k ring holds +31 MiB →
     +16 MiB. The w7 editor acceptance, plus the screen-switch checks: 90/90.
   - ✅ Step 2, the TUI: each pane's mirror is an `rtty_vt::Terminal`, and
     `pane attach`'s too; vt100 left tutti. The TUI draws through its own
     backend (`host::Host`): every rendition, underline styles and colours,
     overline, OSC 8 links, whole clusters, a synchronized update (2026) a
     frame; the terminal's cursor takes the focused pane's DECSCUSR shape;
     focus reports reach the panes that asked. Keys: the daemon keeps a
     pane's kitty flags and modifyOtherKeys level (from raw streams still),
     the TUI encodes each key by them, and asks a kitty terminal for
     disambiguated keys, popped on detach. The render tick holds a pane's
     frame while its program's synchronized update is open (250 ms at most).
     Wire unchanged (rev 10). Acceptance: 99/99, the new checks undercurl in
     the TUI's bytes, nvim's insert-mode bar, Ctrl+I apart from Tab under a
     kitty terminal, focus events, and key modes left clean on detach.
   - ✅ tutti knows rtty (wire rev 11): in an rtty pane (the client's
     `RTTY_PANE` and a socket at `RTTY_SOCKET`) the TUI claims rtty's keys
     (`RTTY_KEYS=pane`, given back on detach, panic and SIGTSTP) and hands a
     move past its edge on to `rtty cli pane focus|resize`. Left and right go
     past the sidebar or changes panel first, and to the tab wrap at rtty's
     own edge. Top and bottom go to rtty only. Resize goes to rtty when tutti
     has no split on that axis. `Attach.handoff` has the daemon hand such a
     client an editor's top, bottom and resize edges too (`FocusEdge`,
     `ResizeEdge`), so Neovim → tutti → rtty is one key. Panes no longer
     inherit `RTTY_PANE`/`RTTY_SOCKET`. A link-only change of a cell always
     redraws (`CellDiffOption::AlwaysUpdate`). Acceptance 113/113, the
     harness playing rtty with a stub `rtty` on PATH.
   - ✅ What a command left on goes off at its shell's command-end mark (OSC
     133;D): mouse, focus, 2031, modifyOtherKeys and kitty's flags.
     - **The failure it fixes:** an nvim killed with SIGKILL left 2031 on, and
       each later nvim probed it as set and so never reset it. The next
       light/dark flip reached zsh's line editor as typing, which ate the
       harness's `nv`.
     - **Key encodings:** the TUI had also typed Ctrl-C into that shell as
       `CSI 99;5u`.
     - **Why at D:** shells set their own prompt modes after D (zsh, bash,
       fish measured), and fish's 2031 comes before the integration's A.
     - **Acceptance:** 115/115.
     - **Since 2026-10-23 the engine's:** rtty-vt's `set_command_end_reset`
       does it (the pin moved to rtty `ef7d4328`, which also reads kitty's
       OSC 99 notifications); tutti only tells the taps.
   - ✅ The 2031 server test's race is fixed. Everything written to a pane's
     pty goes through one writer thread in queue order: answers, light/dark
     notices and input alike. Before this, typed input took a second path and
     could overtake a report queued before it.
   - ✅ Full kitty key reports (wave 9): the TUI asks a kitty terminal for
     1|4 plus what the focused pane's program pushed — event types (2), and
     every key an escape code with its text (8|16) — set on the entry it
     pushed as the focus or the flags change (`CSI = n ; 1 u`), popped on
     detach. A `CSI … u` key keeps kitty's `Report` (code, alternates, raw
     modifier bits, text) beside its `KeyEvent`, and the encoder passes it on
     as far as the pane asked; a key held down stays with the pane it was
     pressed into — repeats and release, whatever the focus since — with
     event types kept on until it is let go; a release never reaches a pane
     whose press was tutti's, and a lone modifier is never a tutti key.
     Neovim 0.12 pushes 1|2, so it now hears repeats and releases. Wire
     unchanged. Acceptance: 58/58 (inline in nvim's `:terminal`, the escape
     time, kitty reports through a harness playing kitty), and the wave 8
     harness 120/120.
   - ✅ The openpty flake is fixed: ptys are opened one thread at a time.
     macOS's openpty(3) fails with errno -6 when two threads of one process
     call it at once. Processes never collide.
   - ✅ Step 3, rtty itself: rtty runs on rtty-vt and its own pty since rtty
     commit nkmtylwp. `alacritty_terminal` stays only as rtty-vt's
     dev-dependency, the oracle of its differential tests.
11. ✅ **Wave 9** — six stacks, wire rev 12:
   - robust: a request whose task panics is answered with an error, and the
     CLI times out an unanswered one; the daemon log is trimmed while it
     runs; `pane wait --next` loses no edge; `workspace add -r` checks the
     revision before jj makes a checkout (Phase 4 items 1 and 5, Phase 5).
   - notes: the notification history and an in-flight turn's prompt are
     saved; a transition's note carries its message; a `notification.show`
     with no pane finds it by the peer's pid; watch carries `cwd` and the
     held approval, `pane list` the workspace (Phase 1 item 5, Phase 3 items
     2–4).
   - issues: opt-in tracker write-back; the issue link in watch snapshots;
     a long prompt goes through a 0600 file; every catalog row's
     first-message data (Phase 4 item 11).
   - hooks: Codex and Gemini CLI integrations, Codex launched `--no-daemon`
     behind an ancestry guard; a Prime Agent registry row; a node program
     detected by its script (Phase 3 items 1–2, Phase 4 item 6).
   - approvals: several held approvals per pane; a turn Esc interrupted
     closed by the next `prompt_id`; Blocked ending once Claude's screen
     shows its own dialog answered; an opencode hold path; Claude 2.1's
     `Agent` subagent tool (Phase 4 items 6, 8 and 9).
   - attach: `pane attach --inline`, `escape_time_ms`, and full kitty key
     reports, a held key's release staying with the pane it was pressed
     into (items 9 and 10 above).
12. ✅ **Wave 10** — approved by the user; all done in waves 10 and 11:
   - ✅ agent-to-agent talk, wire rev 13: `pane read --last-reply`, `tutti
     ask`, the sidebar's reply overlay, and the skill teaching `ask` (Phase 4
     items 2 and 3);
   - ✅ a `pane attach` stream that falls behind is resynced: its frames go
     through a per-connection outbox the writer takes whole; past 1 MiB of
     output queued the tap drops it and is handed a snapshot from the hold
     that parsed the read
     (`a_pane_stream_that_stops_reading_skips_ahead_to_the_screen`; live: 20
     MB unread, 19 resyncs, the terminal caught up, still attached);
   - ✅ `tutti attach --view`: a watch-only TUI, client-side — it sends only
     attach, detach, focus, tab, scroll and read requests; no input, size
     vote, colours or session change; its footer leads with `view`; it never
     starts a daemon and is offered no restore; panes keep the other
     clients' sizes, else their own. `pane attach --view` is not done: a raw
     stream draws right only at the pane's size, which its vote guarantees;
   - ✅ snapshots carry scrollback: rtty-vt's `Terminal::history` (rtty
     13fb35b9), `PaneAttach.history` (wire rev 14), `pane attach --inline
     --history ROWS`; the pin moved to rtty main, whose marks sit past the
     last column after a full row (`last_output` reads them as they stand);
   - ✅ (rtty) a global hotkey through the `global-hotkey` crate (Carbon
     `RegisterEventHotKey`), rtty's `global_hotkey`.
   Their source is tarp (unlicensed: read for ideas only, nothing copied;
   its GPL renderer repo, gpui-terminal, unread).

**Open** — the headline items; each phase item above keeps its own list:
- tracker write-back stops at the deploy: tutti never closes an issue
  (GitHub's own `Fixes #N` can), and command sources stay read-only;
- under a terminal without kitty's keyboard protocol, a pane that asked for
  releases, repeats or text gets presses only (its text derived from the
  key);
- inline, a resync drops from the host's scrollback the lines it skipped,
  and a second attach in the same terminal prints the history again; a
  view's focus is the session's, so a view focusing a done agent marks it
  seen;
- `tutti ask` needs a pane its agent's hooks have reported through. ✅ A
  fresh Codex or opencode (Codex sends SessionStart only with its first
  prompt, opencode's plugin nothing before a turn) is taken when `ask` finds
  tutti's integration for it installed, in the user's config or the pane's
  project: the send names the agent (`PaneSend.hooks`, wire rev 15) and the
  daemon takes an unhooked pane only when that is the agent detected there
  (wave 12). Open: installed is all tutti sees — Codex skips a hook not yet
  trusted in `/hooks`, so such an ask runs to its timeout; a pane detection
  has not named yet (its first second) is refused. A turn Claude's Esc
  interrupts sends no Stop, so an `ask` on it runs to its timeout; a fresh
  pane can read as working while it draws (refused until it settles) or
  `unknown` behind a startup dialog (`ask` types into the dialog); replies are
  not kept across a daemon restart.

✅ Wave 11: a pane send only queues its input; a full queue (64) refuses it,
so a program that stops reading stalls nobody, and a send to an exited pane
is refused. A launcher row shows a number only where a digit launches it
(rows 1–9), never a later row or the dim catalog.

**Deliberately not done:** packaging and distribution (Phase 5) — the project
is private, so install stays `cargo install --path` from a checkout.

**Unverified live** — cover these in the acceptance run:
- checkpoints through the screen-classifier, agent-exit and focus paths (a
  real Claude Code and Codex turn's hook path ran live in wave 9); `jj util
  gc` pruning a checkpoint's commit; a turn-end probe racing a restore;
- whether Claude Code emits its own OSC 9;4 (a hook-less claude pane would
  then hold off `Done` until the progress clears);
- opencode's question/error/retry events;
- conversation ids from a real Claude Code and opencode, and prompt labels
  from a real opencode (checked against the hooks reference and the opencode
  1.18.5 bundle);
- a restore reviving a real `claude --resume` / `opencode --session` (proven
  with a stand-in recipe);
- a held Claude request passed with `esc`, or timed out, falling through to
  Claude's own dialog (holds answered from tutti, the dialog kept back, ran
  live in wave 9);
- a real Claude in a sandbox pod running the `hooks claude --tty` commands with
  the session's tty, so `/dev/tty` resolves (checked only by typing the
  commands into a busybox pod);
- Linear's issue data and write-back with a real key, and a real `gh`
  write-back (the read documents validate against the live API, and every
  document against Linear's published schema; the replies are covered only
  by fixtures, the writes only by stubs);
- Codex: a held approval answered from tutti; SubagentStart/Stop rows; the
  ancestry guard dropping a shared-daemon hook (unit-tested only);
- Gemini CLI's tool and permission events, `--resume`, and its `AfterAgent`
  reply for `--last-reply`: they need the user's Gemini login (SessionStart
  and a prompt's BeforeAgent → AfterAgent ran live);
- `tutti ask` against a real blocked or slow agent (exit 3, exit 124), and an
  opencode `session.error` or Codex `Interrupt` turn read with
  `--last-reply`;
- the first-message data for Goose, Qwen Code, Cursor, Copilot, Factory
  Droid, Augment and Amazon Q, from their docs only and unrun (Amazon Q's
  from a search excerpt of its reference page). The same goes for the file
  inputs of Goose, Pi and Augment; Ori's has only its 0.13.0 `--help` and
  changelog. Cursor's detection and `agent` launch come from its install
  script and the 2026.11.05 package's launcher, unrun;
- kitty key reports against a real kitty, WezTerm or Ghostty (only a harness
  played kitty), and `pane attach --inline` in Emacs vterm or eat;
- `tutti attach --view` on a real second terminal; the inline history in
  Neovim's `:terminal` and Emacs vterm/eat (run only with a tutti pane as
  the host);
- Claude's `StopFailure` and `PostToolUseFailure` hooks (from the hooks
  reference only).

## Phase 5 — Hardening & distribution

- Config file: `~/.config/tutti/config.toml` (prefix key, mouse on/off, default shell,
  agent registry overrides). `theme = terminal|dark|light` landed with it.
- Crash-safety: server panics must not kill child PTYs where avoidable; client
  crashes never harm the server. ✅ Every pty lock recovers from poisoning.
  Until wave 8 a vt100 panic on a pane's reader thread restarted that pane's
  screen blank (the trigger found: a wide character printed into a one-column
  pane); the pane's engine is now rtty-vt, property-tested never to panic on
  any bytes or resize (it writes that wide character as U+FFFD). ✅ A pane is
  the unit of failure: a panic in its engine, on its reader or under any lock
  of its terminal (`PtyPane::with`), is caught inside the lock, so nothing is
  poisoned, and ends that pane alone — its process group killed, its screen
  telling of the crash, a `pane crashed` note, the message in the log.
  ✅ The hub's locks
  recover from poisoning too; the render tick, VCS watch, classify and detect
  passes survive their panics (logged, the loop goes on); a panicking frame
  drops only its connection, which leaves through `drop_client` like any
  other; a failed accept is logged and retried. The detached daemon appends
  its stderr to `<session>.log` beside its socket (trimmed to its newest MiB
  past 4 MiB, at start and within a second while it runs), and `server
  start` prints the path. The TUI reseeds a pane whose snapshot or delta
  panics its parser, its panic hook restores the terminal, and `agent-event`
  gives a wedged daemon 2 s. ✅ A request's spawned task that panics answers
  that request with an error carrying its `req` (`Reply::spawn`). The CLI
  gives up on a request left unanswered after 10 s, or 3 min for one the
  daemon runs jj for (`client::deadline`); `pane wait`, `watch` and attach
  stay unbounded, and an approval hold keeps its `approval_timeout_secs`.
  ✅ A probe's count in the VCS watch and a held approval are let go of on
  every exit, a panic included (drop guards); the TUI gives up on its
  follow-ups at the CLI's deadlines, saying so (wave 11). ✅ A killed pane —
  `pane kill`, `workspace kill`, an ephemeral pane's exit, the shutdown's
  `kill_all` — goes whole into `Session::removed`, and the hub's session guard
  (`SessionGuard`) drops it after unlocking, on a `tutti-teardown` thread: on
  Linux, with a background job of its own process group holding the pty,
  `PtyPane::drop` can wait out its 2 s join, and now stalls neither the
  session nor the request that killed the pane (a test build's slow drop
  holds both under 500 ms). ✅ A view and a listing read no file: each
  workspace keeps its jj root and git branch (`session::Meta`), read as it
  mounts and by the VCS watch each second with no lock held, which also
  reads the op-heads mark before opening its book (wave 12). Open: a mount
  still reads them once under the session lock, so a project added on a
  hung network mount stalls the session that once.
- Perf targets: 60fps redraw with 8 busy panes; `pane read` on 10k-line scrollback
  < 50ms; input latency indistinguishable from raw terminal. ✅ The render tick
  formats only what changed: it takes each pane's damage (`PtyPane::look`)
  and redraws those rows, a full snapshot only for a new client or after a
  switch of screens. (Before wave 8 it kept scrollback-free vt100 mirrors and
  diffed them: per changed pane 24 µs at 80x24, 98 µs at 200x50, release.)
  `pane read` over a full 10k ring takes ~2 ms in a debug build with the
  engine optimized (`[profile.dev.package.rtty-vt]`), held by a debug-build
  test against the 50 ms target.
- Test matrix: macOS + Linux; kitty/alacritty/wezterm/iTerm2/Terminal.app/ghostty;
  sandbox panes against a local cluster running agent-sandbox (unit tests cover
  the manifests, the cluster run covers the cleanup paths). ✅ The `openpty`
  flake (`Unknown error: -6` on a test's first spawn) is fixed since wave 8:
  every pty opens through `pty::openpty`, one thread at a time — the tree's
  only `openpty` call; rtty-vt opens none. Wave 9 ran the whole suite 10
  times at `--test-threads=4` with no openpty failure. (Without the lock, 8
  threads did not reproduce it in 1,280 opens; wave 8's 16-thread run is the
  only reproduction.)
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
