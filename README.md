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
cd ~/code/myproject && tutti     # auto-starts the daemon and drops you into a shell pane
```

Bare `tutti` (no subcommand) attaches to the session, starting the daemon if
needed. If the session has no workspaces yet, it bootstraps one anchored to the
current directory with a shell pane — so `cd repo && tutti` is all you need.
`tutti attach` is an alias for the same thing.

Inside, split (`Ctrl+B %`), run an agent, and move around:

```sh
tutti pane run -- claude                     # run an agent in a pane (current tab)
```

Detach (`Ctrl+B d`) and everything keeps running. Reattach with `tutti`, or
inspect headlessly: `tutti pane list`, `tutti pane read 1`,
`tutti pane send 1 --text 'y'`.

### Stopping

Detaching never kills anything. To stop a session's daemon (and every pane it
owns), run `tutti server stop` (add `-s <name>` for a non-default session). The
daemon is per-session, so stopping one leaves the others running.

## TUI keys

Direct keys work without the prefix (smart-splits / zellij-nav muscle memory):

| Key | Action |
| --- | --- |
| `Ctrl+h/j/k/l` | focus the pane to the left/down/up/right |
| `Ctrl+h`/`Ctrl+l` at the edge | move to the previous / next tab |
| `Alt+h/j/k/l` | resize the focused split toward that direction |
| `Alt+x` | kill the focused pane (confirms) |

Because these intercept their chords before the pane sees them, apps in a pane
no longer receive those bytes (e.g. `Ctrl+l` no longer clears the shell). Rebind
or disable any of them in the config (set an entry to `"none"` to hand the key
back to the pane).

Everything else lives behind the prefix (`Ctrl+B` by default). The `default`
preset:

| Key | Action |
| --- | --- |
| `%` / `"` | split right / split down |
| `n` / `p` / `c` | next / previous / new tab |
| `w` | focus the workspace/agent sidebar |
| `o`, arrows | focus next pane / directional focus |
| `z` | zoom focused pane |
| `x` | kill pane (confirms) |
| `[` | scrollback for focused pane (`q`/esc exits) |
| `d` or `q` | detach |
| `?` | help |

You never have to memorise these. The status bar always shows a compact hint on
its right edge (`C-b ? help · C-b q detach`), and if you press the prefix and
pause, a **which-key** popup lists every follow-up (press the key, or `esc` to
back out). `Ctrl+B ?` opens the full help overlay — detach first, then the rest
of the active keymap, the direct keys, and how to stop the daemon.

Mouse: click focuses a pane, wheel scrolls its history (disable with
`mouse = false`). Colors come from your terminal — tutti has no theme of its own.

## Configuration

Optional, at `$XDG_CONFIG_HOME/tutti/config.toml` (falling back to
`~/.config/tutti/config.toml`). A missing file uses the defaults below; a
malformed file, an unknown key, or an unparseable chord is a hard error naming
the offending entry (nothing is silently ignored).

```toml
prefix = "C-b"          # prefix chord
mouse = true            # master mouse switch
preset = "default"      # prefix keymap: "default" (emacs/tmux-flavored) or "vim"
sidebar = "auto"        # workspace/agent sidebar: "auto", "on", or "off"
notifications = true    # re-emit pane bells/notifications to your real terminal

[keys]                  # direct bindings; every entry optional; "none" disables
focus_left  = "C-h"
focus_down  = "C-j"
focus_up    = "C-k"
focus_right = "C-l"
resize_left  = "A-h"
resize_down  = "A-j"
resize_up    = "A-k"
resize_right = "A-l"
kill_pane    = "A-x"
```

Chord syntax is `C-<char>` (Ctrl), `A-<char>` (Alt), or a bare printable
character. The table above is exactly the default, so a file that reproduces it
changes nothing.

**Presets.** `preset` selects the *prefix* keymap table. The which-key popup and
help overlay always render whichever preset is active, so they stay accurate.

- `default` — the emacs/tmux-flavored table above (`%`/`"` split, `x` kill,
  `d`/`q` detach, …). The `C-b` prefix chord is itself already emacs-style.
- `vim` — mnemonics vim users reach for: `v`/`s` split right/below, `h/j/k/l`
  directional focus, `q` kill pane (`:q` closes a window), `d` detach (so detach
  stays reachable), `t` new tab, `n`/`p` next/prev tab, `z`/`[`/`?` unchanged.

An unknown preset is a hard error. A dedicated `emacs` preset may land later; for
now `default` already covers that muscle memory. The `[keys]` direct bindings are
shared across presets and override the defaults on top (e.g. set
`focus_left = "none"` to reclaim `C-h` regardless of preset).

## Sidebar

A left column that turns the TUI into a control center for many projects and
agents at once. It has two stacked sections:

- **WORKSPACES** — one row per workspace: its name (bold when it owns the active
  tab) over a dim line showing the git branch (read straight from `.git/HEAD`,
  including worktrees) or the directory name. Selecting one jumps to its tab.
- **AGENTS** — one row per agent pane across *every* workspace: a state-coloured
  dot (blocked red, working yellow, done green, idle/unknown dim), the pane
  title, and a dim `state · kind` line. Sorted blocked-first so whatever needs
  you is at the top. Selecting one jumps to that pane, switching workspace and
  tab as needed. A pane that rings a bell or fires a desktop notification while
  in the background gets a 🔔 mark here that clears when you focus it.

Press the prefix then `w` to focus the sidebar (revealing it if hidden). While
focused, `j`/`k` (or the arrows) move the highlight across both sections, `Enter`
jumps and hands focus back to the pane, `esc`/`w` unfocus, and `n` opens a
one-line `dir:` prompt to create a new workspace (`~` expands to your home;
relative paths resolve against the client's cwd; a bad directory surfaces the
server's error). A mouse click focuses the sidebar, or jumps straight to the
entry clicked.

Visibility is set by the `sidebar` config value: `auto` (default) shows it once
the session is worth surfacing — more than one workspace, or at least one agent
pane — while `on` always shows it and `off` keeps it hidden until you focus it
with `w`. It is suppressed on very narrow terminals so panes keep usable room;
there, the blocked-first status-bar badges remain the fallback surface.

### Notifications

Agents inside panes emit terminal bells and desktop-notification escapes (OSC 9,
OSC 777). Tutti captures these from each pane's output and, for a **background**
(non-focused) pane, flashes the text in the status bar and re-emits a bell plus
an OSC 9 to *your* real terminal — so wezterm/kitty/etc. raise an actual desktop
notification naming the pane. The focused pane is left alone (you are already
looking). The status flash and re-emit are gated by `notifications = true`
(default); the sidebar 🔔 mark is always on regardless.

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
tutti [attach]                       # bare `tutti` attaches; bootstraps an empty session
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

Single attached client; no layout persistence across daemon restarts; mouse
limited to focus + scroll; worktree workspaces and the orchestration verbs
(`pane wait`, composite `run`) come next.
