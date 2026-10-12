# CLAUDE.md — working conventions for the tutti repo

Product docs live in README.md; roadmap and ground rules in PLAN.md; wire
protocol in docs/transport-decision.md. This file is the non-obvious knowledge
that isn't derivable from those.

## Version control

- **jj only, never git.** Working-copy edits auto-join `@`; commit with
  `jj commit -m`, the real commit is then `@-`.
- Multi-agent work uses one jj workspace per agent, pinned to an explicit base
  commit. Merge ritual: verify the agent's gate independently, `jj diff --stat`
  scope check, then rebase **from the default workspace** (`jj rebase -s
  <agent-change-id> -d <dest>`) — never from inside the workspace being merged
  or deleted. After merging: `jj workspace forget <name>` then `rm -rf` the dir.

## The gate (before every commit/merge)

```
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

- **Never run the gate through pipes that mask exit codes** (`cmd | rg | bc &&
  jj commit`) — a pipeline's status is the last command's. This shipped red
  commits three times. Chain with `&&` on the cargo commands themselves, or
  check statuses explicitly.
- Server integration tests must each finish well under 10s; a test "running
  over 60 seconds" means a deadlock, not slowness.

## Correctness landmines (break one and a real bug ships)

- **Session-mutex guards**: never let a `hub.session()` guard live inside a
  `for`-iterator expression or `if let` scrutinee — both extend the guard
  across the body while inner calls re-lock the same non-reentrant mutex.
  Bind with a plain `let` first.
- **`WIRE_REV`** (tutti-core/src/protocol.rs): bump on ANY protocol shape
  change, additive included. Daemons outlive binary upgrades; the attach
  handshake warning is the only thing standing between users and silent
  version skew.
- Protocol changes are **additive-only** with `#[serde(default)]` on new
  fields, plus round-trip and old-payload-defaults tests.
- TUI geometry has one source of truth (`App::regions`/`content_rect`); any
  new chrome row/column updates it there and nowhere else, and mouse hit-tests
  follow automatically.
- **The change probe's order is load-bearing** (`jj::probe` in
  tutti-server/src/jj.rs): `jj log -r @` snapshots the working copy — tutti's
  one recorded operation — then the op-heads mark is read, then the diff runs
  `--ignore-working-copy`. Reorder it, or drop the flag, and the VCS watch
  re-triggers on tutti's own snapshot: a working-copy scan per workspace every
  second for as long as an agent keeps editing.
- **A killed pane gets no signal to clean up on**: `PtyPane::kill` SIGKILLs
  the pane's whole process group at once — no SIGHUP first. Anything a pane
  must undo on the way out (the sandbox wrapper's delete) lives in a process
  outside that group, triggered by a pipe closing, never by a trapped signal.
- **Every state transition goes through `publish_transition`** (server.rs),
  followed by a refresh of the pane's workspace — on entering `Working` too. A
  site that broadcasts `StateChanged` directly, or only refreshes on turn ends,
  silently loses checkpoints (the first turn's baseline would be snapshotted at
  its end and deduped away).
- **`tutti agent-event` never writes stdout**: Claude Code feeds a
  `UserPromptSubmit`/`SessionStart` hook's stdout to the model as context. The
  one exception is a held `PermissionRequest`'s decision JSON — that is the
  hook's answer. Errors go to stderr; the exit is always 0.
- **A sequence the pane's terminal might answer ends a run fed to it**
  (`Term::process`/`settle`, `Verdict::Ask`, pty.rs): the reply it drives out
  is then that query's own, and the query is kept from the taps exactly when
  it was answered. A reply from mid-run would be pinned on the next query and
  strip the wrong bytes. When rtty-vt learns to answer a new sequence, add it
  to `verdict`'s Ask arms (the owned-queries test enumerates them).
- **The render tick is the one consumer of a pane's damage**
  (`PtyPane::look`): it takes it once and fans the same redraw out. Anything
  else that took the damage would starve the clients' deltas.
- **Shutdown order** (`serve`, server.rs): abort *and await* the layout saver,
  write the final save, then `kill_all`. A save after `kill_all` — or a saver
  tick racing the final write — records a layout with every pane gone. An empty
  session never writes the live save: it would push the pending restore offer
  aside.
- **Every client exit goes through `drop_client`** (server.rs), which
  re-arbitrates the panes that client voted a size for. Removing a client from
  `hub.clients` any other way leaves its panes stuck at a departed client's
  size.
- **Panics cost one pass, never the daemon**: hub locks go through the
  poison-recovering accessors (`hub.session()`, `hub.clients()`, … — never
  `.lock().expect()`), and background passes run inside `guarded` (or as their
  own task if async). In the TUI, any new `catch_unwind` must set `FEEDING` as
  `fed()` does, or the panic hook restores the terminal under a TUI that keeps
  running.
- **A TUI request whose reply drives a follow-up goes out through
  `App::follow`** (tagged with a `req`), never behind a bare flag — an
  unrelated refusal would cancel it (it did, to a merge's cleanup offer).
- **Terminal-query answers go out through the pane's answer thread**
  (`tutti-pty-answers`, pty.rs): the reader `try_send`s each read's answers
  after releasing the parser lock, and nothing writes the pty from the reader
  or under that lock. A raw-mode program that stops reading its input blocks a
  pty write; a reader blocked there never drains the program's output, and the
  pane deadlocks on something as plain as `cat`ting a file of `ESC[6n`.
- **Taps are fed in the same parser-lock hold as `Term::process`, minus what
  the daemon answered or owns** (`Term::taps`, `verdict`; it owns mode 2031,
  and drops modifyOtherKeys and kitty's keyboard protocol before the terminal
  sees them, so they never reach a stream or a snapshot): a stream's snapshot
  and its first raw read come from one hold, so nothing is lost or repeated,
  and a resize hands taps a fresh snapshot in the hold that resizes. An
  answered query that reached a raw stream would be answered again by the
  client's terminal, into the pane; an unanswered one must reach it. A switch
  of screens goes to a tap alone (`Processed::cuts`): `pane attach` redraws a
  frame that switches. A tap runs on the reader thread under that lock: it
  never blocks, waits or takes another lock — it `try_send`s.
- **A prompt start (OSC 133;A) clears `TUTTI_KEYS` and `TUTTI_EDITOR`**
  (`broadcast_tick`, server.rs): the only thing that hands the direct keys back
  after an editor is `kill -9`'d. A new user var that claims something of the
  pane belongs in that list.
- **Every spawn stamps `COLORTERM=truecolor` in `spawn_into_tab`**
  (session.rs), beside `TUTTI_PANE`/`TUTTI_SESSION`: Neovim's `termguicolors`
  and Emacs's 24-bit colour read it, not `TERM`. A spawn that bypassed the
  funnel would give its pane 256 colours.
- **User vars are never persisted** (`PaneInfo.user_vars`): they describe a
  running program — an nvim server socket, a claim on the keys — that a
  restore does not bring back. A restored pane starts with none; never write
  them into the layout save.

## Product vocabulary and rules

- User-facing: **project** (top-level sidebar entry) and **workspace** (a jj
  checkout nested under it). The word "fork" is banned from UI text — git
  baggage. The CLI matches: `project add|list|kill`, `workspace
  add|list|kill|merge|update|diff`. The pre-rename `workspace new --dir` /
  `workspace fork` spellings are hidden aliases (tests pin them to the same
  requests), and a test keeps "fork" out of every help screen. The wire keeps
  `WorkspaceNew`/`WorkspaceFork`: never rename protocol variants.
- **No human-facing capability may ship CLI-only** (PLAN ground rule). The CLI
  is the agent/script API; the TUI is the human surface. A wave adding a
  human-relevant verb lands its TUI path in the same wave.
- jj is the only VCS integrated — no git/mercurial adapters, by decree.
- Prior art: consulting permissively-licensed (Apache/MIT/BSD) tools for design
  ideas is fine — never copy code verbatim, never consult AGPL/copyleft sources.
  (Relaxed 2026-08-31: the tool that motivated the old clean-room rule
  relicensed to Apache 2.0.)

## Design system (TUI)

- All colour goes through `attach/theme.rs` (`theme::current()`, activated
  per draw): never write a `Color::…` literal in render code outside that file.
  Three palettes: `terminal` (default), `dark`, `light` — `theme = …` in config,
  an explicit choice. The one detection is `theme::host_colors` (OSC 10/11 via
  terminal-colorsaurus, once at attach, before crossterm owns input — the
  colours the daemon then answers panes' colour queries with): a light terminal
  selects with ANSI 7 instead of ANSI 8, because some light schemes (Ghostty
  "Primary") map ANSI 8 to pure black.
- Dim by default; ONE accent marks the focused/active thing (focused pane frame
  and title, the focused panel's header and the rail down its edge, the `▎`
  gutter + name chip on a selected row); red/yellow/green on state dots, the working spinner, the
  blocked border — and, as diff polarity, on `+`/`−` stats and bars (changes
  panel, sidebar stat, footer). Names (projects, agents, the active workspace
  in the footer) stay at full intensity — identity bright, metadata dim; don't
  re-dim them. Chrome panels are borderless (lowercase bold-dim section
  headers, no box-drawing frames); pane frames are the only boxes. Caps are
  reserved for the footer's mode chip — nothing else in the chrome is uppercase.
- The `terminal` theme paints **no background of its own** — it draws over the
  terminal's default so it tracks the terminal's own light/dark scheme live
  (the same mechanism that keeps pane interiors in step). Its only painted
  background is the selected row, in an **ANSI palette colour** (never RGB) so
  the terminal repaints it too on a scheme flip. Never put a fixed-RGB shade in
  that palette — it would stay dark over a light terminal. The `dark`/`light`
  palettes are the opposite by design: fixed truecolor grounds (chrome a step
  apart from the pane ground, pane default colours re-grounded too) and an
  explicit muted grey instead of DIM — that is what the user opts into.
- Glyphs: safe-unicode set by default, nerd-font set behind `icons =
  "nerdfont"`. No private-use codepoints in the default set.
- Which-key, the help overlay, and the bottom-bar hints all render from the
  live keymap tables — the prefix table and the panels' `config::SIDEBAR_TABLE`
  / `CHANGES_TABLE` (`hint` marks a binding the bottom bar shows). New bindings
  go through a table or they won't appear in the discoverability surfaces
  (tests pin each table == dispatch).

## Testing rituals

- Acceptance = drive the real binaries: CLI round-trips against a live daemon,
  and the TUI inside a Python `pty.fork()` harness (see the scratchpad
  patterns). Two traps: ratatui damage-rendering means assertions must run on
  COMBINED capture windows (unchanged rows aren't re-emitted), and the 30-col
  sidebar truncates long strings — probe for short substrings.
- Verify OUTCOMES, not chrome: after driving a flow, check `workspace list`/
  `pane list`/filesystem state, not just that pixels appeared.
- Fake agent binaries for detection tests must be real compiled executables
  (`cc` a 5-line C file named `claude`): macOS SIGKILLs copied system
  binaries (broken signatures), so `cp /bin/sh fake-claude` hangs forever.
- jj-integration tests run against throwaway `jj git init` tempdir repos with
  an explicit `main` bookmark — never against this repo.
- Inside a pane, `-s` defaults to `$TUTTI_SESSION` and `tutti run` names its
  caller from `$TUTTI_PANE`: tests that drive the real binary must
  `env_remove` both (cli.rs's scenario runner does) or set them, or they break
  when the suite runs inside tutti.
- `skills/tutti/SKILL.md` is drift-tested against clap: its fenced `tutti`
  lines must stay literal (no `<placeholder>`, no `$var`).

## Dev loop

- `just install` kills ALL running daemons then reinstalls both binaries —
  deliberate dev behavior so you're always testing the code you just built.
  End users get the non-killing flow documented in README.
- Installs — `hooks … --install`, `skill --install`, the setup panel (prefix
  `i`) — all go through `tutti::integrations`: plan, confirm, then `apply`
  with a backup and a changed-since-inspected guard. Never write
  `~/.claude/settings.json` (or the plugin and skill files) outside that flow.
