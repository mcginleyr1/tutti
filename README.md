# Tutti

A terminal-native **agent multiplexer** — tmux for AI coding agents. Tutti runs
terminal agents (Claude Code, Codex, …) in persistent panes owned by a daemon,
understands what state each agent is in (**blocked / working / done / idle**),
and exposes everything through a CLI and socket API so both humans and agents
can drive it.

Status: **alpha**. [docs/limitations.md](docs/limitations.md) says what is
missing today, and [PLAN.md](PLAN.md) tracks what is open.

The words this page uses:

| Word | Meaning |
| --- | --- |
| daemon | `tutti-server`, the background process that owns every pane; panes keep running when nothing is attached |
| session | one named daemon and everything it owns (`-s <name>`, default `tutti`) |
| pane | one terminal the daemon runs: an agent, a shell, or any other command |
| tab | a layout of panes, split right or down |
| project | a directory you opened in tutti |
| workspace | a [jj](https://jj-vcs.github.io) checkout nested under a project, so an agent works on its own copy of the tree |
| TUI | the full-screen client that `tutti` opens in your terminal |
| prefix | `Ctrl+B`, the key pressed before most TUI commands |

An agent is **blocked** when it needs your input, **working** while it has a
task running, **done** when it has finished and you have not looked yet, and
**idle** once you have.

## Install

You need a stable [Rust toolchain](https://rustup.rs) (rustc 1.85 or newer) on
macOS or Linux, and the agents you want to run on your `PATH`: tutti detects
and drives them, it does not install them. From a checkout of this repository:

```sh
cargo install --path crates/tutti          # the CLI + TUI client
cargo install --path crates/tutti-server   # the daemon
```

Both binaries land in `~/.cargo/bin` (make sure that is on your `PATH`). You
never launch `tutti-server` yourself: the `tutti` CLI starts the daemon on
first use.

Optional: [jj](https://jj-vcs.github.io) for workspaces and diffs, `kubectl`
for Kubernetes sandboxes, `gh` or `curl` for deploying issues. Exact state for
Claude Code needs its hooks, installed once with `tutti hooks claude
--install` (or `Ctrl+B i` in the TUI); Codex, Gemini CLI and opencode have
theirs. The requirements in full, the agent catalog, the optional
integrations and upgrading are in [docs/install.md](docs/install.md).

## Quickstart

Your first session, end to end:

```sh
cd ~/code/myproject && tutti
```

1. **Mount your project.** Bare `tutti` attaches, auto-starting the daemon
   (`tutti attach` is the same thing). On a fresh session nothing is assumed:
   the sidebar opens on an add-project prompt prefilled with your current
   directory — `Enter` takes it, or edit the path first, or `esc` to skip and
   add one later with `a`. (With `[[projects]]` configured — see
   [Configuration](docs/configuration.md) — they mount automatically and the prompt
   is skipped.)

2. **Pick what runs.** The run launcher opens over the new project: your
   installed agents first (`claude`, `crush`, …), a plain `shell`, a free-form
   `command…` — press a row's number or `Enter`. Conversations you had in that
   directory before tutti appear as **resume rows**; below them, the rest of
   the agent catalog sits dim with project links. Later, `Ctrl+B r` reopens
   the launcher over whatever project you are in.

3. **Split and move.** `Ctrl+B %` splits right, `Ctrl+B "` splits down;
   `Ctrl+h/j/k/l` moves between panes, `Ctrl+B z` zooms one. Run more agents
   in the splits, or leave a shell beside them.

4. **Herd from the sidebar.** `Ctrl+B w` focuses the sidebar: a tree of your
   projects with each one's agents nested beneath it (state dots: blocked red,
   working spinner, done green), and the cross-project **waiting** queue of
   everything blocked on you. `Enter` jumps to what you highlight; `p` on a
   blocked agent **peeks** at its prompt and lets you answer it in place,
   without leaving your pane, and on a done one shows its last reply; `n`
   starts another agent in the selected project
   (in its own tab), `d` shows its jj diff, `w` creates a nested workspace, `m`
   merges one back. See [Sidebar](docs/tui.md#sidebar) and [Workspaces](docs/workspaces.md#workspaces).

5. **Detach and return.** `Ctrl+B d` detaches; every pane keeps running under
   the daemon. `tutti` reattaches — state dots and the waiting queue catch you
   up on what happened while you were away.

6. **Or drive it headless.** The same daemon answers the CLI, so scripts (and
   agents) work the panes without a screen: `tutti run -- claude "fix the
   tests"` prints the new pane's id, `tutti pane wait 7 --until done,blocked`
   blocks until that agent finishes or needs you, `tutti pane read 7` shows its
   screen, and `tutti pane send 7 --text y` types into it. `tutti ask 7 "…"`
   does the round trip in one call: it types the message, waits for the turn,
   and prints the agent's reply, so one agent can talk to another. See
   [Orchestrating agents](docs/api.md#orchestrating-agents) and [CLI surface](docs/cli.md).

Detaching never kills anything. `tutti server stop` stops the session's
daemon and every pane it owns; the next `tutti` offers the layout back
([docs/sessions.md](docs/sessions.md)).

## Everyday keys

These work in a pane without the prefix:

| Key | Action |
| --- | --- |
| `Ctrl+h/j/k/l` | focus the pane to the left/down/up/right |
| `Alt+h/j/k/l` | resize the focused split toward that direction |
| `Alt+x` | kill the focused pane (confirms) |

These follow the prefix, `Ctrl+B`:

| Key | Action |
| --- | --- |
| `%` / `"` | split right / split down |
| `c` / `n` / `p` | new / next / previous tab |
| `r` | run an agent, a shell or a command here (opens the launcher) |
| `w` | focus the sidebar: projects, agents and the waiting queue |
| `g` | go to: a jump list of every agent by state |
| `f` | focus the changed-files panel |
| `N` | notifications |
| `z` | zoom the focused pane |
| `[` | scrollback for the focused pane (`q`/esc exits) |
| `d` or `q` | detach |
| `?` | help: every key |

In the sidebar: `Enter` jumps to the row, `p` answers a blocked agent in place
or shows a done agent's reply, `n` starts another agent, `w` creates a
workspace, `d` shows its diff, `m` merges it back, `x` kills. Press the prefix
and pause, and a popup lists every key that can follow. All keys, the mouse
and rebinding: [docs/keys.md](docs/keys.md).

## Everyday commands

Everything the TUI does is also a command, for scripts and for agents:

```sh
tutti                                    # attach (starts the daemon if needed)
tutti run -- claude "fix the tests"      # an agent in a fresh tab; prints the new pane's id
tutti pane list                          # every pane with its agent and state
tutti pane wait 7 --until done,blocked   # block until pane 7 finishes or needs you
tutti pane read 7                        # print pane 7's screen
tutti pane send 7 --text y               # type into it
tutti ask 7 "what did you change?"       # send a message, wait for the turn, print the reply
tutti workspace add 1 --name fix-login   # a jj checkout under project 1 for an agent to work in
tutti workspace diff 2 --stat            # what workspace 2 changed against trunk
tutti notes --unread                     # what blocked, finished or rang while you were away
tutti server stop                        # stop the daemon and every pane it owns
```

`--json` on any command prints the raw response. The full list is in
[docs/cli.md](docs/cli.md); orchestration, replies and `tutti watch` are in
[docs/api.md](docs/api.md).

## Where things are documented

When you change a feature, update the file that documents it.

| File | What it covers |
| --- | --- |
| [docs/install.md](docs/install.md) | requirements, building, optional integrations, upgrading |
| [docs/sessions.md](docs/sessions.md) | stopping, restoring after a restart, several clients and `--view`, `--host` over ssh, crashes and the log |
| [docs/keys.md](docs/keys.md) | direct keys, prefix keys, help, mouse |
| [docs/tui.md](docs/tui.md) | app bar and footer, run launcher, go to, setup panel, sidebar |
| [docs/answering.md](docs/answering.md) | answering a blocked agent from the queue, reading a reply, held approvals |
| [docs/workspaces.md](docs/workspaces.md) | jj workspaces and merge, the changed-files panel, turns (per-turn checkpoints) |
| [docs/notifications.md](docs/notifications.md) | bells and desktop notifications, the history, `tutti notes` |
| [docs/agents.md](docs/agents.md) | the agent catalog, state detection and its sources, OSC 9;4 / 7501 and herdr reporting, the Claude Code, Codex, Gemini CLI and opencode integrations |
| [docs/issues.md](docs/issues.md) | deploying issues from GitHub, Linear and other trackers to agents |
| [docs/sandboxes.md](docs/sandboxes.md) | running agents in Kubernetes sandboxes |
| [docs/api.md](docs/api.md) | scripting: `run`, `pane wait`, `ask`, tailing a pane, the tutti skill, `tutti watch` |
| [docs/cli.md](docs/cli.md) | every command and flag, timeouts, `--json`, `-s`, `--host` |
| [docs/configuration.md](docs/configuration.md) | the config file, chords, presets, themes, icons |
| [docs/terminal.md](docs/terminal.md) | what a pane shows, `pane attach`, terminal queries, key protocols, colours |
| [docs/shell-integration.md](docs/shell-integration.md) | `tutti shell-init`: working directories and command marks |
| [docs/editors.md](docs/editors.md) | Neovim and Emacs in panes, `tutti edit`; the plugins are in [editors/](editors/README.md) |
| [docs/rtty.md](docs/rtty.md) | the TUI inside the rtty terminal; rtty showing tutti panes itself |
| [docs/limitations.md](docs/limitations.md) | known limitations |
| [docs/transport-decision.md](docs/transport-decision.md) | the wire protocol on the daemon's socket |
| [PLAN.md](PLAN.md) | architecture, what is open, ground rules for implementation |
| [docs/history.md](docs/history.md) | what each finished phase and wave built |
| [CLAUDE.md](CLAUDE.md) | conventions for agents working in this repository |

## Repository layout

```
crates/tutti-core     pure domain model, agent state machine, protocol + frame codec
crates/tutti-server   daemon: PTYs and their terminals (rtty-vt), socket API, detection & classification
crates/tutti          client: CLI verbs + ratatui TUI
crates/tutti-agents   agent registry & screen-state classifier (data-driven)
```
