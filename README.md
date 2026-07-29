# Tutti

A terminal-native **agent multiplexer** — tmux for AI coding agents. Tutti runs
terminal agents (Claude Code, Codex, …) in persistent panes owned by a daemon,
understands what state each agent is in (**blocked / working / done / idle**),
and exposes everything through a CLI and socket API so both humans and agents
can drive it.

Status: **alpha**. See `PLAN.md` for the roadmap and `docs/transport-decision.md`
for the wire protocol.

## Install

You need:

- **A stable [Rust toolchain](https://rustup.rs)** — the workspace builds on
  edition 2024, so rustc **1.85 or newer**. macOS and Linux.
- **[jj](https://jj-vcs.github.io)** — optional. Core multiplexing works
  anywhere; the workspace-level VCS features (per-workspace diffs, nested
  [workspaces](#workspaces) on isolated checkouts, merge-back, branch display)
  are prescriptive: they require jj — there are no git/mercurial adapters.
- **The agents themselves** on your `PATH`. Tutti detects and drives them; it
  does not install them. The recognized catalog: [Claude
  Code](https://claude.com/claude-code), [Codex
  CLI](https://github.com/openai/codex), [Gemini
  CLI](https://github.com/google-gemini/gemini-cli),
  [OpenCode](https://opencode.ai),
  [Crush](https://github.com/charmbracelet/crush), [Aider](https://aider.chat),
  [Goose](https://github.com/block/goose),
  [Pi](https://github.com/badlogic/pi-mono), [Qwen
  Code](https://github.com/QwenLM/qwen-code), [Cursor
  CLI](https://cursor.com/cli), [Copilot
  CLI](https://github.com/github/copilot-cli), [Amp](https://ampcode.com),
  [Factory Droid](https://factory.ai), [Augment
  Code](https://www.augmentcode.com), and [Amazon
  Q](https://github.com/aws/amazon-q-developer-cli). A missing agent shows dim
  in the launcher with its link, so the picker doubles as the install list.
  Precise blocked/working/done detection is tuned per agent (Claude Code and
  Codex today); the rest start from generic heuristics — see
  `crates/tutti-agents/src/registry.rs`.

From a checkout of this repository:

```sh
cargo install --path crates/tutti          # the CLI + TUI client
cargo install --path crates/tutti-server   # the daemon
```

Both binaries land in `~/.cargo/bin` (make sure that is on your `PATH`). You
never launch `tutti-server` yourself — the `tutti` CLI auto-starts the daemon
on first use, one per named session (`-s <name>`, default `tutti`).

Two optional finishing touches:

- `tutti hooks claude --install` upgrades Claude Code state detection from
  screen heuristics to exact hook signals — recommended; see
  [Claude Code integration](#claude-code-integration). It shows the merge and
  asks before touching `~/.claude/settings.json`.
- Using a Nerd Font? Set `icons = "nerdfont"` in the config for crisper
  sidebar glyphs (see [Configuration](#configuration)).

### Upgrading

Re-run the two `cargo install` commands. A daemon that is already running keeps
executing the **old** binary until it is restarted — finish or detach your
agents, `tutti server stop`, and the next `tutti` starts the new one. The
attach handshake warns whenever client and daemon disagree on the wire
protocol, so version skew is loud, never silent.

## Quickstart

Your first session, end to end:

```sh
cd ~/code/myproject && tutti
```

1. **Mount your project.** Bare `tutti` attaches, auto-starting the daemon
   (`tutti attach` is the same thing). On a fresh session nothing is assumed:
   the sidebar opens on an add-project prompt prefilled with your current
   directory — `Enter` takes it, or edit the path first, or `esc` to skip and
   add one later with `n`. (With `[[projects]]` configured — see
   [Configuration](#configuration) — they mount automatically and the prompt
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

4. **Herd from the sidebar.** `Ctrl+B w` focuses the sidebar: your projects,
   the selected project's agents (state dots: blocked red, working spinner,
   done green), and the cross-project **waiting** queue of everything blocked
   on you. `Enter` jumps to what you highlight; `r` runs another agent in the
   selected project, `d` shows its jj diff, `w` creates a nested workspace,
   `m` merges one back. See [Sidebar](#sidebar) and [Workspaces](#workspaces).

5. **Detach and return.** `Ctrl+B d` detaches; every pane keeps running under
   the daemon. `tutti` reattaches — state dots and the waiting queue catch you
   up on what happened while you were away.

6. **Or drive it headless.** The same daemon answers the CLI, so scripts (and
   agents) work the panes without a screen: `tutti pane list`,
   `tutti pane read 1`, `tutti pane send 1 --text 'y'`,
   `tutti pane run -- claude`. See [CLI surface](#cli-surface).

### Stopping

Detaching never kills anything. To stop a session's daemon (and every pane it
owns), run `tutti server stop` (add `-s <name>` for a non-default session). The
daemon is per-session, so stopping one leaves the others running.

## TUI keys

Direct keys work without the prefix (smart-splits / zellij-nav muscle memory):

| Key | Action |
| --- | --- |
| `Ctrl+h/j/k/l` | focus the pane to the left/down/up/right |
| `Ctrl+h` at the left edge | step into the sidebar (when visible), then the previous tab |
| `Ctrl+l` at the right edge | move to the next tab |
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
| `r` | run an agent / shell / command here (opens the launcher) |
| `o`, arrows | focus next pane / directional focus |
| `z` | zoom focused pane |
| `x` | kill pane (confirms) |
| `[` | scrollback for focused pane (`q`/esc exits) |
| `m` | mouse on/off (off hands the mouse back to the terminal for select/copy) |
| `d` or `q` | detach |
| `?` | help |

You never have to memorise these. The bottom bar always shows a compact hint on
its right edge (`C-b ? help · C-b q detach`, the key bright and its label dim),
and if you press the prefix and pause, a **which-key** popup lists every
follow-up (press the key, or `esc` to back out). `Ctrl+B ?` opens the full help
overlay — detach first, then the rest of the active keymap, the direct keys, and
how to stop the daemon.

A full-width **app bar** runs across the top: an accent bar and the bold
`tutti — <session>` wordmark on the left, the tab segments right-aligned
(`[1 main] [2 logs] [+]`, the active one an accent block, the `+` dim), over a
dim rule. Click a segment to select its tab, the `[+]` to open one. Each pane
carries its **title on its own line above its rounded frame** — an accent `❯`
plus the agent/state for the focused pane, dim for the rest. The **footer**
stays out of the way: a mode chip on the left when you leave terminal mode
(`SIDEBAR`, `SCROLL`, …) and the standing hint on the right; a transient fires a
one-line **notification band** just above it (accent for info, red for errors).

**Run launcher** (`Ctrl+B r`): a floating ` run in <project> ` panel that answers
"what should start here?" — the agents **installed on this machine** first
(`claude`, `crush`, …), then `shell` and `command…`, then the rest of tutti's
agent catalog dim and unselectable at the foot, each row naming the product and
its project link (`gemini   Gemini CLI · github.com/google-gemini/gemini-cli`)
so the picker doubles as "what else is out there". Its title names the
workspace the choice lands in, so it is never ambiguous where a pane will
spawn.
Move with `j/k`/arrows, press a row's number to launch it outright, `enter`
launches the highlight, `esc` closes; `command…` opens a one-line input that runs
whatever you type via your login shell. Adding a project (`n`) opens the launcher
for the new workspace's first pane too — `esc` there just drops you into a shell,
exactly as before. (Startup `[[projects]]` still mount plain shells, no launcher.)

At the foot of the panel sit up to three **resume rows** — `resume   claude ·
2h · <first prompt>` — harvested from the agent tools' own on-disk session
stores for the target workspace's directory (read-only; Claude Code today,
`~/.claude/projects/…`, each candidate verified against the `cwd` its own
transcript records). Picking one relaunches the conversation in a new pane via
the tool's resume flag (`claude --resume <session-id>`), so a conversation
orphaned by a daemon restart — or one from before the project was mounted in
tutti — is one keystroke from continuing. A resume row for an uninstalled
binary dims like any agent row.

Sidebar edge-nav: at a pane's **left edge**, `Ctrl+h` steps into the sidebar when
it is visible (nvim-explorer muscle memory) instead of jumping straight to the
previous tab; from the sidebar, `Ctrl+l` returns to the pane you left and `Ctrl+h`
goes on to the previous tab. With the sidebar hidden the left edge wraps to the
previous tab as before.

Mouse: click focuses a pane, click a tab segment or sidebar entry to jump, wheel
scrolls a pane's history. Because tutti captures the mouse, your terminal's own
drag-to-select can't see the drag — press the prefix then `m` to hand the mouse
back, select and copy normally, and `m` again to re-grab it (most terminals also
bypass capture while `Shift`/`Option` is held). `mouse = false` starts with the
mouse off. Colors come from your
terminal — tutti has no theme of its own: everything renders dim, with one accent
(terminal blue) marking the focused/active thing and the red/yellow/green state
dots the only other colour. On truecolor terminals a subtle neutral shade sits
behind the chrome (app bar, sidebar, footer, panels); see `chrome_background`.

## Configuration

Optional, at `$XDG_CONFIG_HOME/tutti/config.toml` (falling back to
`~/.config/tutti/config.toml`). A missing file uses the defaults below; a
malformed file, an unknown key, or an unparseable chord is a hard error naming
the offending entry (nothing is silently ignored).

```toml
prefix = "C-b"          # prefix chord
mouse = true            # mouse capture at startup (prefix `m` toggles it live)
preset = "default"      # prefix keymap: "default" (emacs/tmux-flavored) or "vim"
sidebar = "on"          # workspace/agent sidebar: "on" (default), "auto", or "off"
chrome_background = true # subtle shade behind the chrome (truecolor only)
icons = "unicode"       # sidebar glyph set: "unicode" (default) or "nerdfont"
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
  stays reachable), `t` new tab, `n`/`p` next/prev tab, `z`/`[`/`m`/`?`
  unchanged.

An unknown preset is a hard error. A dedicated `emacs` preset may land later; for
now `default` already covers that muscle memory. The `[keys]` direct bindings are
shared across presets and override the defaults on top (e.g. set
`focus_left = "none"` to reclaim `C-h` regardless of preset).

**Chrome background.** `chrome_background` (default `true`) paints a subtle
neutral dark shade behind the chrome — the app bar, sidebar, footer, and the
which-key/help/notification panels — so they read as one surface distinct from
the pane interiors (which are never shaded; agent output always renders on your
terminal's own background). The shade is only drawn on **truecolor** terminals
(those advertising `COLORTERM=truecolor`/`24bit`); elsewhere it falls back to no
background rather than approximating the shade in the 256-colour palette. Light-
theme users, or anyone who prefers a flat look, may want `chrome_background =
false`. An unknown value is a hard error.

**Icons.** `icons` picks the sidebar glyph set. `unicode` (default) is the safe
set every terminal renders — `●`/`○` workspace and state dots, a branch marker,
and a `⑂` stale marker. `nerdfont` swaps in private-use icons (folder, circles,
powerline branch, and the stale marker) for users with a patched
[Nerd Font](https://www.nerdfonts.com)
installed; without one they render as tofu. The tree guides and the working
spinner are box-drawing/braille and shared across both. An unknown value is a
hard error.

## Sidebar

A left column that turns the TUI into a control center for many projects and
agents at once — one rounded frame whose top border carries the `projects`
header and whose fused dividers (`├ agents · name ── N ┤`, `├ waiting ── N ┤`)
carry the `agents` and `waiting` headers, each with a `▼`/`▶` collapse arrow
and a right-aligned count. It renders dim by default, with the focused row
popping in the accent colour. Three stacked sections:

- **projects** — one row per top-level project, with any [nested
  workspaces](#workspaces) rendered **indented beneath it** on `├`/`└` tree
  guides (same two-line row, name bold). Each row is an `●` (accent) when it owns
  the active tab, else a dim `○`, then the bold name, over a dim line showing a
  branch marker and the git branch (read straight from `.git/HEAD`, including
  worktrees) on the left and the jj change stat (`4 files +120 −33`)
  right-aligned. No branch leaves the left blank rather than echoing the name; a
  clean or non-jj workspace shows no stat. The stat refreshes as agents work (on
  every state transition, on attach, and when a workspace is created) and is
  dropped first when the column is too narrow for both. A nested
  [workspace](#workspaces) whose `@` was rewritten from another workspace shows a
  dim-red `stale` tag in the stat's place until you run `workspace update`.
  Collapsing a project hides its nested workspaces too. Selecting a workspace
  jumps to its tab.
- **agents** — one row per agent pane in the **selected project** (the header
  names it): a state dot (blocked red, working an animated spinner, done green,
  idle/unknown dim), the pane title, and a dim `state · kind` line, with any
  hook-reported subagents hanging below on `├`/`└` tree guides. Sorted
  blocked-first, and a nested workspace's agents count as its parent project's.
  The filter follows the sidebar highlight: land on a project (or one of its
  workspaces) and this section shows its agents; outside the sidebar it tracks
  the project that owns the active tab. Selecting an agent jumps to that pane,
  switching workspace and tab as needed. A pane that rings a bell or fires a
  desktop notification while in the background gets a 🔔 mark here that clears
  when you focus it. When the selected project runs no agents the section shows
  a dim italic `no agents here` placeholder.
- **waiting** — the cross-project attention queue: every **blocked** or
  **done** agent from *every* project, blocked first, each over a dim
  `project · kind` line naming where it lives (subagent rows stay in the agents
  section). Highlighting a row here re-points the agents section at that
  agent's project; `Enter` (or a click) jumps to the agent itself, so a stuck
  agent two projects away is never invisible and always one keystroke away.

The selected row (while the sidebar is focused) takes a subtle full-row
highlight with its name rendered as an accent chip, unmistakable even with a
single entry. Click a section header (the top border or the divider) to collapse
that section down to its header, or click again to expand it.

Press the prefix then `w` to focus the sidebar (revealing it if hidden). While
focused, `j`/`k` (or the arrows) move the highlight across all three sections, `Enter`
jumps and hands focus back to the pane, and `esc` unfocuses (esc is the back key
throughout). `w` opens **guided [workspace](#workspaces) creation** on the
selected project (below); `r` **runs** an agent (or shell/command) in the
selected project: it jumps to that workspace and opens the launcher over it, so
the choice lands in a new pane in its tab (an agent row targets the workspace
that owns it) — reach for `r`, not `w`, when you just want another agent in the
checkout you already have. `n` opens the
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
non-jj workspace shows a transient error instead of spawning. `w` starts a new
[**workspace**](#workspaces) nested under the selected project: a two-step prompt
— `workspace name:` (`[A-Za-z0-9_-]+`; a bad name flashes the rule) then `where:`,
prefilled with the sibling default (`<repo-parent>/<repo>-<name>`) and editable
with the same directory completion as add-project (`esc` steps back a step, then
cancels). On success tutti jumps to the new workspace, flashes `workspace <name>
→ <path>`, and opens the launcher to pick the agent to run beside its shell.
`m` **merges** a child workspace back into its project's trunk (`main`, else
`master`) after a `merge <name> into trunk? y/N` confirm: on success it flashes
`merged into <bookmark>` (and `and pushed` when a remote took it) and offers to
**clean up** (discard) the merged workspace; `m` on a top-level project just
flashes `only workspaces merge`. `u` runs `jj workspace update-stale` on a
workspace whose row shows the **stale** tag, clearing it; on a healthy row it
just flashes `not stale`. `x`
**kills** the selected row after a one-line confirm. On a workspace row — `y`
removes it from tutti (leaving its checkout on disk), `D` also **discards** a
workspace's checkout (`jj workspace forget` + delete; the server refuses this
for a workspace tutti did not create, surfacing that error), and any other key
cancels. On an agent row, `x` kills **only that agent's pane** (`kill claude?
y/N`) — never the workspace around it. A mouse click focuses the sidebar, or
jumps straight to the entry clicked.

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

### Workspaces

A **workspace** is a jj checkout nested under a **project** (a repo you opened) —
tutti's unit for fanning an agent onto its own isolated copy of the tree, without
touching the checkout you are working in (jj is the required VCS — there are no
git/hg adapters). A workspace renders **indented beneath its project** in the
sidebar and carries the project's own branch/changes/stale line.

> **CLI vocabulary note.** The agent-facing CLI verb is still `tutti workspace
> fork <id> --name <name>` (it runs `jj workspace add`) — the scriptable name has
> not been renamed, only the TUI wording. Treat "fork" in a CLI verb as a synonym
> for "create a workspace". `tutti workspace fork` places a **sibling** directory
> next to the repo root (`<repo>-<name>`); pass `-r <rev>` to check out a specific
> revision. The name must be `[A-Za-z0-9_-]+` (it becomes both a path component
> and a jj workspace name); the source must live under a `.jj` repo and the
> destination must not already exist. Merge is scriptable too — `tutti workspace
> merge <id> [--push]` — keeping the "every TUI action is a CLI verb" invariant.

Creating a workspace is **not** "run another agent in this project". When you
just want a second agent in the checkout you already have, use `r` (run) on the
sidebar row instead: it opens the launcher over that same workspace, no new
directory.

**Create (`w`).** Off the sidebar (`C-b w` to focus it), `w` on a project (or on
an agent row → its project) opens a two-step prompt: `workspace name:` (validated
`[A-Za-z0-9_-]+`), then `where:`, prefilled with the sibling default and editable
with the same directory completion as add-project (`esc` steps back to the name,
then cancels). On submit tutti materializes the checkout, jumps to it, flashes
`workspace <name> → <path>`, and opens the launcher so you immediately pick the
agent to run beside its shell.

**Merge (`m`).** When a workspace's work is ready, `m` on its row merges it back
into the project's trunk bookmark (`main`, else `master`) after a `merge <name>
into trunk? y/N` confirm. The server rebases the workspace's branch onto trunk,
**refuses and undoes** the merge if it would land a conflict (`merge would
conflict — resolve manually in the workspace`), advances the bookmark to the
workspace's real commit, and — if the origin has a remote — `jj git push`es it.
On success it flashes `merged into <bookmark>` (adding `and pushed` when a push
ran) and offers to **clean up**: `y` discards the now-merged workspace. `m` on a
top-level project flashes `only workspaces merge`.

Because several workspaces share one repo, a workspace's working copy can go
**stale** when its `@` is rewritten from elsewhere. Tutti surfaces this as a
dim-red `stale` tag on the sidebar row and never fixes it for you;
`tutti workspace update <id>` runs `jj workspace update-stale` to reconcile it.

Two ways to remove a workspace, differing only in what happens on disk:

- `tutti workspace kill <id>` — the panes die and the workspace leaves tutti, but
  the jj workspace and its directory stay on disk. It is your checkout and your
  call; nothing is deleted.
- `tutti workspace kill <id> --discard` — additionally `jj workspace forget`s the
  checkout at its origin and removes its directory. `--discard` is **only**
  honoured for a workspace tutti created; it is a hard error on any other
  workspace, so tutti never deletes a checkout it did not create.

Both are reachable from the sidebar: focus it, select the workspace (or one of
its agents), and press `x` — `y` is the plain kill, `D` is the `--discard`
variant. (After a merge, the cleanup prompt is the `--discard` path.)

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
tutti workspace fork <id> --name <name> [-r <rev>]   # create a workspace: jj workspace add onto a sibling checkout
tutti workspace merge <id> [--push]  # merge a workspace back into its project's trunk (main/master)
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
