# Tutti — Implementation Plan

Tutti is a terminal-native **agent multiplexer**: tmux for AI coding agents. It runs
multiple terminal agents (Claude Code, Codex, Cursor CLI, OpenCode, etc.) in persistent
panes, understands each agent's state (blocked / working / done / idle), and exposes a
CLI + socket API so both humans and agents can drive it.

**Out of scope for now (deliberate):** mobile clients (remote *agents* are in:
Phase 4 item 10 launches them into Kubernetes sandboxes, with the pty still
local; remote *clients* over ssh landed 2026-11-08 — Phase 2 item 1), and a
plugin system (we'll design a better one later — do not build extension points
speculatively, but keep module boundaries clean so a plugin layer can be added
without a rewrite). Theming was on this list until 2026-09-01: the default still
inherits the terminal's palette live, and `theme = "dark" | "light"` are opt-in
fixed truecolor palettes chosen in config — the only detection is a
light-background query, at attach and again when the terminal reports a
light/dark switch, that picks the selection row's ANSI colour
([docs/configuration.md](docs/configuration.md#colours-and-themes); the design
rules live in CLAUDE.md).

What was built, phase by phase and wave by wave, is in
[docs/history.md](docs/history.md). This file keeps the architecture, the
alpha milestone, everything still open, and the ground rules.

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
    ├── tutti-wire/            # what a client speaks to the daemon: frames, protocol types, domain model, state machine, revisions — serde only
    ├── tutti-core/            # tutti-wire re-exported, with the daemon's paths, the prompt fingerprint, the sandbox names — no I/O deps
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

**In scope** (the cut across the phases, whose items are in
[docs/history.md](docs/history.md)):

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
— see [docs/configuration.md](docs/configuration.md)). Most of this has since landed ahead of beta —
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

## Open

Four lists: the headline items, what is open in the phase items, the phase
acceptance never marked met, and what no live run has covered yet. Then what
is deliberately left undone.

### Headline items

- the look the cell-border protocol opens up (2026-11-13) stops at the
  panes and the two panels' cards: popups still frame themselves in box
  characters, no field is underlined nor button outlined, and the sidebar's
  selected row and state words are as they were. `host::outline`/`rule` are
  there to draw them with;
- the daemon is no multiplexer for the cell-border protocol: it drops a
  pane's strings (`Verdict::Own`, pty.rs), so a program in a pane draws box
  characters. rtty-vt can now keep a pane's borders and carry them in its
  snapshots (`Terminal::set_cell_borders`, off in the daemon): turning it on
  means the wire's snapshots carry them too and the TUI redraws them inside
  a pane's cells;
- under `--host`, the TUI's one-shot host questions still run on its event
  loop: expanding a submitted directory, the jj check, the launcher, setup
  plans and a deploy's argv. Each is one ssh round trip after a key that
  waits on its answer anyway, but a stalled link freezes the TUI for up to
  the 10 s answer timeout. Directory completion, asked on every keystroke,
  runs on a thread (2026-11-09), as the issue fetch, write-back and editor
  do;
- tracker write-back stops at the deploy: tutti never closes an issue
  (GitHub's own `Fixes #N` can), and command sources stay read-only;
- under a terminal without kitty's keyboard protocol, a pane that asked for
  releases, repeats or text gets presses only (its text derived from the
  key);
- `pane attach --inline`: a terminal's record says what it was handed, not what it still has
  — rows its own scrollback trimmed, or a clear of it (Cmd+K) dropped, are
  not printed again and no line says so; and rows a resize pushes off the
  pane's screen are not handed on: a terminal that shrank too keeps them by
  its own reflow, but a pane shrunk by another client's vote loses them
  from an inline terminal's scrollback unnoted. Handing them on is not
  sound: the daemon cannot tell the two cases apart — in the first they
  would come twice — nor, once its reflow renumbered the rows it kept,
  which of them the terminal holds;
- `tutti ask` needs a pane its agent's hooks have reported through. A fresh
  Codex, opencode or Antigravity CLI is taken when `ask` finds tutti's
  integration for it
  installed (the done parts of this entry are in docs/history.md), but
  installed is all tutti sees — Codex skips a hook not yet
  trusted in `/hooks`, so such an ask runs to its timeout; a pane detection
  has not named yet (its first second) is refused. A turn Claude's Esc
  interrupts sends no Stop, so an `ask` on it runs to its timeout; a fresh
  pane can read as working while it draws (refused until it settles);
  replies are not kept across a daemon restart;
- a view-only client (`attach --view`) counts as attached, so with only a
  view attached a held approval waits for `pane approve` or its timeout,
  though the view cannot answer it (seen live, w12);
- Codex runs per-invocation hooks (`-c hooks.<Event>=[…]`, its "session
  flags" source) only once trusted in its `/hooks`, which persists the
  trust (w12);
- three state edges found in the w12 review, left open (round 5). A
  `UserPromptSubmit` the daemon takes in more than `agent-event`'s 2 s still
  lands, and could let go of a request the new turn already holds (passed to
  the agent's own dialog: safe, but the hold is lost). A fresh pane's grace
  counts from detection, else from the pane's start, so a 7501 idle report
  from an agent that detection names more than 5 s after the pane opened
  reads idle at once. `PaneSlot::rest` marks a rest held back before
  `may_move` decides whether the report or match may move the pane at all.
  No path is known where that matters: a pane with a higher claim is not
  both unworked and `unknown`;
- Claude fires `PreToolUse` before its permission check, so a held request
  answered in Claude's own dialog is known only at that tool's
  `PostToolUse`: the pane reads blocked until the tool is done. For an
  `Agent`/`Task` call that is the subagent's whole run, whose own tool calls
  name the subagent and so release nothing (w12 review);
- a subagent's held request is let go at its stop by its `agent_id`, but a
  `SubagentStop` with no `agent_id` names its `agent_type` instead (Codex's
  rows read `default`), so such a hold waits for an answer or the approval
  timeout. Not checked live (w12 review);
- a restore to a checkpoint whose commit `jj util gc` pruned fails with
  jj's `Revision … doesn't exist`, leaving the working copy as it was, but
  records a `before restore` checkpoint first (w12);
- host frames (`tutti proxy`'s hello) still take `HOST_REV` by equality:
  the floor of wire rev 20 covers the daemon's wire only (w18);
- a frame from the daemon (a `PaneFocused`, say) can close the restore
  offer unanswered, and the offer then stays set until the next attach
  (w18, from reading the code; not reproduced).

### Open in the phase items

Each entry is a sentence the plan marked "Open:" inside an item that is
otherwise done, or, for the last three, a Phase 5 item that is not built in
full. The items themselves, under the numbers given here, are in
[docs/history.md](docs/history.md).

- **Phase 2 item 1 (attach, remote attach).** `Attach.terminal` names this
  machine's app, which a watcher on the far side cannot raise; ages use this
  machine's clock.
- **Phase 3 item 2 (state classification), Prime Agent.** Prime Agent reports
  `agent_session_path`, not an id, so its pane holds no conversation to
  revive (`prime-agent --resume` takes a path or id); its screen patterns
  are guesses.
- **Phase 3 item 2, Oh My Pi.** Its screen patterns are guesses (no turn was
  run). It reports its lifecycle in-band as Warp's `warp://cli-agent` OSC 777
  payloads once `WARP_CLI_AGENT_PROTOCOL_VERSION` ≥ 1, which tutti neither
  sets nor reads. And the `HERDR_*` every pane carries makes it believe it's
  in herdr: its notifier execs a `herdr` CLI that tutti's panes don't have.
- **Phase 3 item 2, Antigravity CLI.** A dialog cancelled with Esc sends no
  Stop (seen live), and the screen ends that turn. The screen ends no turn
  whose dialog it has not shown, so if Esc while the model generates sends
  no Stop either (not run), that pane reads working until its next prompt.
  Whether a message typed into a running turn comes as a first
  `PreInvocation`, closing that turn's checkpoint early, is not run either.
  Its conversations are not harvested for the launcher's resume rows (its
  store is sqlite), and its subagents get no sub-rows.
- **Phase 3 item 3 (surfacing: notes for blocked events).** A hooks install
  without PermissionRequest (one from before it, never re-run) notes a second
  dialog's reminder in the same blocked spell not at all; so does a sandboxed
  command's network prompt while the pane is blocked, if no PermissionRequest
  reports those; and a reminder arriving after its dialog was answered and
  the pane went back to work blocks the pane again, with a note, as before
  wave 12.
- **Phase 4 item 5 (jj workspaces).** An add past its 15 s is given up on but
  left a minute to end on its own: a jj still running after that minute is
  killed, and what it leaves (a Git worktree, a name with no recorded path)
  stays.
- **Phase 4 item 6 (hooks), Claude's notifications.** A mod's notice lands in
  no note.
- **Phase 4 item 6, Codex.** A codex typed by hand without `--no-daemon` runs
  in the shared app-server, so the guard drops its hook events and the pane
  falls back to the screen (no turn labels, no conversation); the guard fails
  open when the session's pid file is missing.
- **Phase 4 item 8 (per-turn checkpoints).** A turn interrupted with Esc
  closes when the next prompt arrives: the pane reads working from the Esc
  until that next prompt, and edits made in between join the interrupted
  turn; Codex's and Gemini's prompts carry no id, so input typed into their
  running turn splits it as `interrupted`.
- **Phase 4 item 9 (answer from the queue, held approvals).** Answered in
  Claude's own dialog, a pane leaves Blocked once its screen shows Claude
  busy again: that dialog-to-Working gate relies on Claude 2.1.318's spinner
  marker; opencode's dialog shows beside a held request (1.18.5 never calls
  the plugin's `permission.ask`), and the reply goes through the plugin
  client's undocumented `_client`.
- **Phase 4 item 10 (Kubernetes sandboxes).** Confirm with a real Claude in a
  pod that hook commands keep the session's tty, and a name prompt for the
  TUI pull.
- **Phase 4 item 11 (deploy tracker issues).** A prompt starting with `-`
  reads as an option to an agent that takes it as a plain argument (Claude,
  Codex, Pi, Vibe, Muse, Droid, Augment, Cursor); only Pi's docs name `--` to
  end its options.
- **Next waves item 9 (editor interop, wave 7).** fish's snippet unrun; bash
  before 4.4 has no `C` mark; Emacs 29 and Neovim before 0.12 unrun.
- **Phase 5, crash-safety.** Each workspace keeps its jj root and git branch,
  read by the VCS watch with no lock held, but a mount still reads them once
  under the session lock, so a project added on a hung network mount stalls
  the session that once.
- **Phase 5, config file (built in part).** Config file: `~/.config/tutti/config.toml` (prefix key, mouse on/off, default shell,
  agent registry overrides). `theme = terminal|dark|light` landed with it.
  The file, the prefix, the mouse flag and the theme are built
  ([docs/configuration.md](docs/configuration.md) lists every key). A default
  shell and agent registry overrides are not: the config has no key for
  either.
- **Phase 5, perf targets (not all measured).** Perf targets: 60fps redraw with 8 busy panes; `pane read` on 10k-line scrollback
  < 50ms; input latency indistinguishable from raw terminal. The work done
  against these is in docs/history.md: the render tick redraws only what
  changed, and a test holds `pane read` under 50 ms. The plan records no
  measurement of the 60 fps and input-latency targets.
- **Phase 5, test matrix.** Test matrix: macOS + Linux; kitty/alacritty/wezterm/iTerm2/Terminal.app/ghostty;
  sandbox panes against a local cluster running agent-sandbox (unit tests cover
  the manifests, the cluster run covers the cleanup paths). The plan marks
  none of the matrix done. The one recorded cluster run of sandbox panes is
  under Phase 4 item 10 in docs/history.md.

### Phase acceptance never marked met

Phases 3 and 4 were marked met (docs/history.md). These two never were. They
are judgments, not features, so they wait for someone to make them. The alpha
milestone above names both (M1 and M2), and its own exit is unmarked too.

- **Phase 1.** with no TUI at all — `tutti project add .`, `tutti pane run -- top`,
  close the terminal, reopen, `tutti pane read` shows `top` still running.
  Integration tests drive the socket directly; CI runs them headless.
  (`pane_survives_client_disconnect` in `crates/tutti-server/tests/server.rs`
  tests the first sentence over the socket.)
- **Phase 2.** run two agents side by side, split/resize/zoom with keyboard and
  mouse, detach, reattach, nothing lost. Scrollback works in an active `claude` pane.

### Unverified live — cover these in the next live run

- frames drawn as cell borders on a real screen. A test runs the TUI's
  output through rtty's engine (`rtty_takes_the_lines_the_tui_sends`,
  host.rs): the query's answer, the rectangle commands, inside placement,
  rounded corners with their outside colour and the cells a frame's writes
  wipe land cell for cell. Seen as pixels once, in rtty, in a screenshot of a single pane with
  both panels open (2026-11-13): frames, cards, gutters and tints drew. Not
  looked at since: split panes and the gutter's drag, popups over lined
  cells, a resize, a light terminal, and the Ghostty branch;
- what the w12 review changed after its run: a fresh agent's 5 s grace
  then idle (a restored agent, one given no work); a detected agent's own
  idle or done report before any work; the footer-anchored dialog and idle
  markers; a turn end releasing only its own agent's held requests (a
  background subagent asking past the main Stop); an ask's wait refusing a
  screen-read block. Each is covered by tests only;
- a turn-end probe racing a restore;
- opencode's question/error/retry events;
- conversation ids from a real opencode, and prompt labels from a real
  opencode (checked against the opencode 1.18.5 bundle);
- a restore reviving a real `opencode --session` (proven with a stand-in
  recipe);
- a real Claude in a sandbox pod running the `hooks claude --tty` commands with
  the session's tty, so `/dev/tty` resolves (checked only by typing the
  commands into a busybox pod);
- Linear's issue data and write-back with a real key, and a real `gh`
  write-back (the read documents validate against the live API, and every
  document against Linear's published schema; the replies are covered only
  by fixtures, the writes only by stubs);
- Codex: the ancestry guard dropping a shared-daemon hook (unit-tested
  only);
- a held request let go by a new prompt, a subagent's request let go by
  its own call's run, and an opencode hold let go by its
  `permission.replied` (tested only; round 5 ran the main agent's call's
  run live);
- Gemini CLI's tool and permission events, `--resume`, and its `AfterAgent`
  reply for `--last-reply`: they need the user's Gemini login (SessionStart
  and a prompt's BeforeAgent → AfterAgent ran live);
- Antigravity CLI: its user hooks file, `~/.gemini/config/hooks.json` (from
  its docs; a project's `.agents/hooks.json` ran live), a restore reviving
  `agy --conversation <id>` (the command ran by hand), an `issue deploy` to
  it (`--prompt-interactive` ran through `tutti run`), and a turn
  interrupted mid-generation;
- an opencode `session.error` turn read with `--last-reply`;
- the first-message data for Goose, Qwen Code, Cursor, Copilot, Factory
  Droid, Augment and Amazon Q, from their docs only and unrun (Amazon Q's
  from a search excerpt of its reference page). The same goes for the file
  inputs of Goose, Pi and Augment; Ori's has only its 0.13.0 `--help` and
  changelog. Cursor's detection and `agent` launch come from its install
  script and the 2026.11.05 package's launcher, unrun;
- kitty key reports against a real kitty, WezTerm or Ghostty (only a harness
  played kitty), and `pane attach --inline` in Emacs vterm or eat;
- `tutti attach --view` in a real terminal app (w12 drove it under a pty);
  the inline history, a resync's rows and a second attach's in Neovim's
  `:terminal` and Emacs vterm/eat (run only with a tutti pane as the host:
  getsid and the tty there are those of a pane's shell).
- `tutti --host` against a real sshd: driven end to end through a stand-in
  `ssh` whose far side had its own HOME, config, SHELL and runtime dir (the
  TUI in a pty: first run, completion, launcher, a setup install, an issue
  fetch, detach, and the forwarded CLI), never over a real network; nor
  `pane attach` (raw) through `ssh -t`.

### Deliberately not done

Packaging and distribution (Phase 5) — the project
is private, so install stays `cargo install --path` from a checkout. The
Phase 5 bullet:

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
- `tutti-wire` and `tutti-core` stay free of I/O and UI dependencies — pure types + state machine,
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
