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

## Product vocabulary and rules

- User-facing: **project** (top-level sidebar entry) and **workspace** (a jj
  checkout nested under it). The word "fork" is banned from UI text — git
  baggage. CLI verbs still say `workspace` for projects: known vocabulary
  debt, tracked in PLAN.md; don't half-rename it.
- **No human-facing capability may ship CLI-only** (PLAN ground rule). The CLI
  is the agent/script API; the TUI is the human surface. A wave adding a
  human-relevant verb lands its TUI path in the same wave.
- jj is the only VCS integrated — no git/mercurial adapters, by decree.
- Clean-room: never read or reference the source of any AGPL-licensed agent
  multiplexer, and never name such products in code, docs, commits, or plans.

## Design system (TUI)

- Dim by default; ONE accent (terminal blue) marks the focused/active thing;
  red/yellow/green only on state dots, the working spinner, and the blocked
  border. Chrome backgrounds are truecolor-gated (`COLORTERM`) with a
  `chrome_background = false` escape; **pane interiors are never themed**.
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
