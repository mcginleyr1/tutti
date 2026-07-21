# Tutti

A terminal-native **agent multiplexer** — tmux for AI coding agents. Tutti runs
terminal agents (Claude Code, Codex, …) in persistent panes owned by a daemon,
understands what state each agent is in (**blocked / working / done / idle**),
and exposes everything through a CLI and socket API so both humans and agents
can drive it.

Status: **alpha**. See `PLAN.md` for the roadmap and `docs/transport-decision.md`
for the wire protocol.

## Install

```sh
cargo install --path crates/tutti
cargo install --path crates/tutti-server
```

Both binaries land in `~/.cargo/bin`; the `tutti` CLI auto-starts the daemon on
first use (one daemon per named session, `-s <name>`, default `tutti`).

## Quickstart

```sh
tutti workspace new --dir ~/code/myproject   # workspace = project, anchored to a dir
tutti pane run -- claude                     # run an agent in a pane (current tab)
tutti attach                                 # sit inside the TUI
```

Detach and everything keeps running. Reattach with `tutti attach`, or inspect
headlessly: `tutti pane list`, `tutti pane read 1`, `tutti pane send 1 --text 'y'`.

## TUI keys

Prefix is `Ctrl+B`, then:

| Key | Action |
| --- | --- |
| `%` / `"` | split right / split down |
| `n` / `p` / `c` | next / previous / new tab |
| `o`, arrows | focus next pane / directional focus |
| `z` | zoom focused pane |
| `x` | kill pane (confirms) |
| `[` | scrollback for focused pane (`q`/esc exits) |
| `d` or `q` | detach |
| `?` | help |

Mouse: click focuses a pane, wheel scrolls its history. Colors come from your
terminal — tutti has no theme of its own.

## Agent state badges

Panes running a detected agent show a live badge: **blocked** (red — needs your
input, listed first in the status bar), **working** (yellow), **done** (green —
finished, not yet viewed; focusing it makes it **idle**). Background panes ring
the terminal bell when they block or finish. The alpha registry detects
`claude` and `codex`; adding an agent is one data-table row in
`crates/tutti-agents` (codex screen patterns are seeds pending live tuning).

## CLI surface

Every TUI action is also a CLI verb over the daemon socket — if it isn't
scriptable, it isn't done:

```
tutti server start|stop [-s session]
tutti workspace new --dir <path> | list | kill <id>
tutti tab new | list | select <id>
tutti pane run [--tab <id>] -- <cmd...>
tutti pane split <pane> right|down
tutti pane list | kill | rename | focus <pane>
tutti pane send <pane> --text <s> | --keys <s>
tutti pane read <pane> [--lines N] [--unwrapped]
tutti attach
```

`--json` on any verb emits the raw protocol response for scripting.

## Workspace layout

```
crates/tutti-core     pure domain model, agent state machine, protocol + frame codec
crates/tutti-server   daemon: PTYs, vt100 grids, socket API, detection & classification
crates/tutti          client: CLI verbs + ratatui TUI
crates/tutti-agents   agent registry & screen-state classifier (data-driven)
```

## Alpha limitations (deliberate — see PLAN.md)

Single attached client; no layout persistence across daemon restarts; no config
file (prefix fixed at `Ctrl+B`); mouse limited to focus + scroll; worktree
workspaces and the orchestration verbs (`pane wait`, composite `run`) come next.
