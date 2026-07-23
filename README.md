# Tutti

A terminal-native **agent multiplexer** — tmux for AI coding agents. Tutti runs
terminal agents (Claude Code, Codex, …) in persistent panes owned by a daemon,
understands what state each agent is in (**blocked / working / done / idle**),
and exposes everything through a CLI and socket API so both humans and agents
can drive it.

Status: **alpha**. See `PLAN.md` for the roadmap and `docs/transport-decision.md`
for the wire protocol.

## Install

Core multiplexing works anywhere. The workspace-level VCS features (per-workspace
diffs, `workspace fork` onto isolated checkouts, branch display) are prescriptive:
they require [jj](https://jj-vcs.github.io) — there are no git/mercurial adapters.

```sh
cargo install --path crates/tutti
cargo install --path crates/tutti-server
```

Both binaries land in `~/.cargo/bin`; the `tutti` CLI auto-starts the daemon on
first use (one daemon per named session, `-s <name>`, default `tutti`).

## Quickstart

```sh
cd ~/code/myproject && tutti     # auto-starts the daemon and asks where to start
```

Bare `tutti` (no subcommand) attaches to the session, starting the daemon if
needed. On a **fresh** session it does not assume anything: the sidebar opens
focused on an add-project prompt prefilled with your current directory — press
Enter to take it as-is, edit it first, or `esc` to skip and add a project later
with `n`. Configure `[[projects]]` (see below) and those mount automatically
instead, skipping the prompt. `tutti attach` is an alias for the same thing.

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

You never have to memorise these. The bottom bar always shows a compact hint on
its right edge (`C-b ? help · C-b q detach`, the key bright and its label dim),
and if you press the prefix and pause, a **which-key** popup lists every
follow-up (press the key, or `esc` to back out). `Ctrl+B ?` opens the full help
overlay — detach first, then the rest of the active keymap, the direct keys, and
how to stop the daemon.

A **top tab bar** runs above the panes: one chip per tab (the active one an
accent block), plus a trailing ` + ` chip. Click a chip to select it, the `+` to
open a tab. The **bottom bar** stays out of the way: the session name on the
left with any transient/mode message, the standing hint on the right.

Mouse: click focuses a pane, click a tab chip or sidebar entry to jump, wheel
scrolls a pane's history (disable with `mouse = false`). Colors come from your
terminal — tutti has no theme of its own: everything renders dim, with one accent
(terminal blue) marking the focused/active thing and the red/yellow/green state
dots the only other colour.

## Configuration

Optional, at `$XDG_CONFIG_HOME/tutti/config.toml` (falling back to
`~/.config/tutti/config.toml`). A missing file uses the defaults below; a
malformed file, an unknown key, or an unparseable chord is a hard error naming
the offending entry (nothing is silently ignored).

```toml
prefix = "C-b"          # prefix chord
mouse = true            # master mouse switch
preset = "default"      # prefix keymap: "default" (emacs/tmux-flavored) or "vim"
sidebar = "on"          # workspace/agent sidebar: "on" (default), "auto", or "off"
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

# Startup projects: workspace dirs mounted (each with a shell pane) on attach.
# Idempotent — a dir already open is left alone, so restarts don't duplicate it.
# Mounting any project skips the first-run prompt. A dir that does not exist on
# disk surfaces a transient error after attach; the other projects still mount.
[[projects]]
dir = "~/develop/tutti"   # required; `~` expands to your home

[[projects]]
dir = "~/develop/other"
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
agents at once. It renders dim by default, with the focused row popping in the
accent colour. Two stacked sections, each with a lowercase header and a
right-aligned count:

- **workspaces** — one row per workspace: an `●` (accent) when it owns the active
  tab, else a dim `○`, then the bold name, over a dim line showing the git branch
  (read straight from `.git/HEAD`, including worktrees) on the left and the jj
  change stat (`4 files +120 −33`) right-aligned. No branch leaves the left blank
  rather than echoing the name; a clean or non-jj workspace shows no stat. The
  stat refreshes as agents work (on every state transition, on attach, and when a
  workspace is created) and is dropped first when the column is too narrow for
  both. A [forked](#forked-workspaces) workspace whose `@` was rewritten from
  another workspace shows a dim-red `stale` tag in the stat's place until you run
  `workspace update`. Selecting a workspace jumps to its tab.
- **agents** — one row per agent pane across *every* workspace: a state dot
  (blocked red, working an animated spinner, done green, idle/unknown dim), the
  pane title, and a dim `state · kind` line. Sorted blocked-first so whatever
  needs you is at the top. Selecting one jumps to that pane, switching workspace
  and tab as needed. A pane that rings a bell or fires a desktop notification
  while in the background gets a 🔔 mark here that clears when you focus it. When
  no agents are running the section shows a dim `no agents yet` placeholder.

The selected row (while the sidebar is focused) carries a `▍` accent bar and a
subtle full-row background, unmistakable even with a single entry.

Press the prefix then `w` to focus the sidebar (revealing it if hidden). While
focused, `j`/`k` (or the arrows) move the highlight across both sections, `Enter`
jumps and hands focus back to the pane, `esc`/`w` unfocus, and `n` opens the
**add-project** prompt — a one-line field for the path to an *existing* directory
to open (it mounts that directory as a workspace; it never creates one). The
field prefills the common parent of your current projects, so you type just the
project's name, and it completes directories as you type: `Tab` fills in the
highlighted match and opens it (a trailing `/` reveals its contents), `↑`/`↓`
move the highlight, and `Enter` always opens whatever is typed. `~` expands to
your home; relative paths resolve against the client's cwd and are canonicalized
to an absolute path before the daemon sees them; a bad directory surfaces the
server's error. `d`
opens the selected workspace's **jj diff** in an ephemeral pane — a real terminal
running `jj diff | less -R`, coloured, that vanishes the moment you quit `less`
(pressing `d` on an agent row opens the diff for the agent's workspace). A
non-jj workspace shows a transient error instead of spawning. A mouse click
focuses the sidebar, or jumps straight to the entry clicked.

Visibility is set by the `sidebar` config value: `on` (default) always shows it
— the control column is the point — while `auto` reveals it only once the
session is worth surfacing (more than one workspace, or at least one agent pane)
and `off` keeps it hidden until you focus it with `w`. It is suppressed on very
narrow terminals so panes keep usable room. With the sidebar off, a **blocked**
agent still rings the bell and turns its pane border red until you focus it.

### Notifications

Agents inside panes emit terminal bells and desktop-notification escapes (OSC 9,
OSC 777). Tutti captures these from each pane's output and, for a **background**
(non-focused) pane, flashes the text in the status bar and re-emits a bell plus
an OSC 9 to *your* real terminal — so wezterm/kitty/etc. raise an actual desktop
notification naming the pane. The focused pane is left alone (you are already
looking). The status flash and re-emit are gated by `notifications = true`
(default); the sidebar 🔔 mark is always on regardless.

### Forked workspaces

Tutti can fan an agent out onto its own isolated checkout with jj workspaces
(jj is the required VCS — there are no git/hg adapters). `tutti workspace fork
<id> --name <name>` runs `jj workspace add` to materialize a **sibling**
directory next to the repo root (`<repo>-<name>`), mounts it as a tutti
workspace, and drops a shell pane into it. Pass `-r <rev>` to check out a
specific revision; the name must be `[A-Za-z0-9_-]+` (it becomes both a path
component and a jj workspace name). The source workspace must live under a `.jj`
repo, and the destination must not already exist — neither is silently reused.

Because several workspaces share one repo, a fork's working copy can go **stale**
when its `@` is rewritten from elsewhere. Tutti surfaces this as a dim-red
`stale` tag on the workspace's sidebar row and never fixes it for you;
`tutti workspace update <id>` runs `jj workspace update-stale` to reconcile it.

Two ways to remove a fork, differing only in what happens on disk:

- `tutti workspace kill <id>` — the panes die and the workspace leaves tutti, but
  the jj workspace and its directory stay on disk. It is your checkout and your
  call; nothing is deleted.
- `tutti workspace kill <id> --discard` — additionally `jj workspace forget`s the
  fork at its origin and removes its directory. `--discard` is **only** honoured
  for a workspace tutti forked; it is a hard error on any other workspace, so
  tutti never deletes a checkout it did not create. Merging a fork back is left a
  human decision.

## Agent state badges

Panes running a detected agent show live state on the pane border and in the
sidebar's agents section: **blocked** (red — needs your input; sorted first in
the sidebar and reddening the pane border), **working** (yellow, an animated
braille spinner), **done** (green — finished, not yet viewed; focusing it makes
it **idle**). The pane's border title carries the same `agent · state` suffix;
a plain shell shows just its name. Background panes ring the terminal bell when
they block or finish. The alpha registry detects `claude` and `codex`; adding an
agent is one data-table row in `crates/tutti-agents` (codex screen patterns are
seeds pending live tuning).

## Claude Code integration

Every pane tutti spawns exports `TUTTI_PANE` (the pane id) and `TUTTI_SESSION`
(the session name) into its environment. A Claude Code instance running in such
a pane can report ground-truth lifecycle events back to tutti through Claude
Code's hooks, replacing the screen-heuristic guessing with exact signals.

Install the hook config (shows the before/after merge and asks first):

```
tutti hooks claude --install            # merge into ~/.claude/settings.json
tutti hooks claude --install --project  # …into ./.claude/settings.json instead
tutti hooks claude --install --yes      # skip the confirmation (scripts)
tutti hooks claude                      # or just print the snippet…
tutti hooks claude --raw                # …as bare JSON, for piping
```

`--install` preserves everything already in the file (foreign hooks included),
is idempotent, backs the old file up to `settings.json.bak`, and writes
atomically. The printed snippet already includes the `"hooks"` key — merged by
hand it goes at the settings.json **top level**. Each wired event runs
`tutti agent-event claude`, which reads the hook JSON on stdin, maps it, and
forwards it to the pane's daemon:

- a tool use keeps the agent **working**; a permission/idle **Notification**
  marks it **blocked**; **Stop** marks it **done**;
- a **Task** (subagent) spawn adds a dim indented sub-row under the agent in the
  sidebar — a shared spinner while it runs, a `·` once it finishes — and
  **SubagentStop** completes it; finished sub-rows clear when the turn ends.

`agent-event` is deliberately fail-safe: outside a tutti pane (no `TUTTI_PANE`),
on a malformed or irrelevant event, or when it cannot reach the daemon, it exits
0 and sends nothing — a hook never breaks a Claude session, inside tutti or out.
The one caveat: if you ever uninstall tutti while the hooks remain, Claude Code
will surface a non-blocking "command not found" warning on every tool call —
remove the entries (restore `settings.json.bak` or delete them) when you go.

Hooks are ground truth: once a pane reports one, tutti stops classifying that
pane's state from its screen (the heuristics would only fight the exact signals).
Agent **detection** (which kind of agent runs there) is unaffected, so the badge
still appears. Display only — tutti never manages a foreign agent's subagents.

## CLI surface

Every TUI action is also a CLI verb over the daemon socket — if it isn't
scriptable, it isn't done:

```
tutti server start|stop [-s session]
tutti workspace new --dir <path> | list | kill <id> [--discard]
tutti workspace fork <id> --name <name> [-r <rev>]   # jj workspace add onto a sibling checkout
tutti workspace update <id>          # jj workspace update-stale (clears the sidebar's stale tag)
tutti workspace diff <id> [--stat]   # the workspace's jj diff (jj is required; --json for raw lines)
tutti tab new | list | select <id>
tutti pane run [--tab <id>] [--ephemeral] -- <cmd...>
tutti pane split <pane> right|down
tutti pane list | kill | rename | focus <pane>
tutti pane send <pane> --text <s> | --keys <s>
tutti pane read <pane> [--lines N] [--unwrapped]
tutti hooks claude [--raw]           # print the Claude Code hooks snippet to wire agent-event
tutti agent-event claude             # forward a Claude Code hook event (JSON on stdin) — used by hooks
tutti [attach]                       # bare `tutti` attaches; a fresh session asks where to start
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
