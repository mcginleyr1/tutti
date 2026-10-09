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

## Correctness landmines (each shipped a real bug once)

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
- **Every state transition goes through `publish_transition`** (server.rs),
  followed by a refresh of the pane's workspace — on entering `Working` too. A
  site that broadcasts `StateChanged` directly, or only refreshes on turn ends,
  silently loses checkpoints (the first turn's baseline would be snapshotted at
  its end and deduped away).

## Product vocabulary and rules

- User-facing: **project** (top-level sidebar entry) and **workspace** (a jj
  checkout nested under it). The word "fork" is banned from UI text — git
  baggage. CLI verbs still say `workspace` for projects: known vocabulary
  debt, tracked in PLAN.md; don't half-rename it.
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
  an explicit choice. The one detection is `theme::detect_terminal_background`
  (OSC 11 via terminal-colorsaurus, once at attach, before crossterm owns
  input): a light terminal selects with ANSI 7 instead of ANSI 8, because some
  light schemes (Ghostty "Primary") map ANSI 8 to pure black.
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
  live keymap table — new bindings go through that table or they won't appear
  in the discoverability surfaces (there's a test pinning table == dispatch).

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

## Dev loop

- `just install` kills ALL running daemons then reinstalls both binaries —
  deliberate dev behavior so you're always testing the code you just built.
  End users get the non-killing flow documented in README.
- Claude Code hooks integration: `tutti hooks claude --install` merges into
  `~/.claude/settings.json` with confirm + backup; never write that file
  without the confirm flow.
