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
- **kubectl** — optional, for launching agents into
  [Kubernetes sandboxes](#kubernetes-sandboxes) (a cluster running
  [agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox)).
- **gh** or **curl** — optional, for [deploying issues](#deploying-issues) from
  GitHub (through your `gh` login) or Linear (its API through `curl`).
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
  [Factory Droid](https://factory.ai), [Muse
  Code](https://dev.meta.ai/docs/muse-code), [Mistral
  Vibe](https://github.com/mistralai/mistral-vibe), [Augment
  Code](https://www.augmentcode.com), [Amazon
  Q](https://github.com/aws/amazon-q-developer-cli), and
  [Ori](https://openrouter.ai/labs/ori). A missing agent shows dim
  in the launcher with its link, so the picker doubles as the install list.
  Precise blocked/working/done detection is tuned per agent — Claude Code,
  Codex, and Ori fully, Muse Code (working/done) and Mistral Vibe
  (blocked/working) in part; the rest start from generic heuristics — see
  `crates/tutti-agents/src/registry.rs`.

From a checkout of this repository:

```sh
cargo install --path crates/tutti          # the CLI + TUI client
cargo install --path crates/tutti-server   # the daemon
```

Both binaries land in `~/.cargo/bin` (make sure that is on your `PATH`). You
never launch `tutti-server` yourself — the `tutti` CLI auto-starts the daemon
on first use, one per named session (`-s <name>`, default `tutti`).

Optional finishing touches:

- `tutti hooks claude --install` upgrades Claude Code state detection from
  screen heuristics to exact hook signals — recommended; see
  [Claude Code integration](#claude-code-integration). It shows the merge and
  asks before touching `~/.claude/settings.json`. `tutti hooks opencode
  --install` does the same for opencode via a plugin — see
  [opencode integration](#opencode-integration).
- `tutti skill --install` gives the Claude Code agents in your panes the tutti
  skill, so they can start and coordinate sub-agents of their own — see
  [The tutti skill](#the-tutti-skill).
- `eval "$(tutti shell-init zsh)"` in your shell's rc file lets a split start
  where you are and `tutti pane read --last-output` find the last command's
  output — see [Shell integration](#shell-integration).
- Neovim or Emacs in a pane: the plugins in `editors/` let the editor and tutti
  share `Ctrl+h/j/k/l` and open files for `tutti edit` — see
  [Editors in panes](#editors-in-panes).
- The hooks, the plugin, the skill and the shell integration's rc line also
  install from the TUI: `Ctrl+B i` opens the setup panel (see
  [TUI keys](#tui-keys)).
- Using a Nerd Font? Set `icons = "nerdfont"` in the config for crisper
  sidebar glyphs (see [Configuration](#configuration)).

### Upgrading

Re-run the two `cargo install` commands. A daemon that is already running keeps
executing the **old** binary until it is restarted — finish or detach your
agents, `tutti server stop`, and the next `tutti` starts the new one and offers
your layout back (see [Restoring after a restart](#restoring-after-a-restart)).
The attach handshake warns whenever client and daemon disagree on the wire
protocol, so version skew is loud, never silent. The hooks run whichever
`tutti` is first on a pane's `PATH`, and a hook install from an older tutti
lacks the newer events — re-run `tutti hooks claude --install` (or `Ctrl+B i`),
which adds just the missing ones.

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

4. **Herd from the sidebar.** `Ctrl+B w` focuses the sidebar: a tree of your
   projects with each one's agents nested beneath it (state dots: blocked red,
   working spinner, done green), and the cross-project **waiting** queue of
   everything blocked on you. `Enter` jumps to what you highlight; `p` on a
   blocked agent **peeks** at its prompt and lets you answer it in place,
   without leaving your pane; `n` starts another agent in the selected project
   (in its own tab), `d` shows its jj diff, `w` creates a nested workspace, `m`
   merges one back. See [Sidebar](#sidebar) and [Workspaces](#workspaces).

5. **Detach and return.** `Ctrl+B d` detaches; every pane keeps running under
   the daemon. `tutti` reattaches — state dots and the waiting queue catch you
   up on what happened while you were away.

6. **Or drive it headless.** The same daemon answers the CLI, so scripts (and
   agents) work the panes without a screen: `tutti run -- claude "fix the
   tests"` prints the new pane's id, `tutti pane wait 7 --until done,blocked`
   blocks until that agent finishes or needs you, `tutti pane read 7` shows its
   screen, and `tutti pane send 7 --text y` types into it. See
   [Orchestrating agents](#orchestrating-agents) and [CLI surface](#cli-surface).

### Stopping

Detaching never kills anything. To stop a session's daemon (and every pane it
owns), run `tutti server stop` (add `-s <name>` for a non-default session). The
daemon is per-session, so stopping one leaves the others running.

### Restoring after a restart

Stopping loses nothing you arranged. The daemon saves its layout as it goes —
every project and workspace, each tab's splits and their ratios, and each
agent's conversation and turns — to `$XDG_STATE_HOME/tutti/<session>.json`
(else `~/.local/state/tutti/`), rewriting it whenever the layout changes and
once more as it stops. Pane contents are not saved.

The next daemon never restores on its own: the first attach opens **restore
last session?** (`3 projects · 7 panes · 2 agents to revive · saved 2h ago`).
`enter` brings it back, `d` discards it, and `esc` leaves it for the next
attach; it comes before the first-run prompt. Scripts use `tutti server
restore [--discard]`.

- An agent pane holding a conversation id — reported by Claude Code's hooks,
  the opencode plugin, or a herdr report — comes back resuming exactly that
  conversation under its old title, when tutti knows the tool's resume recipe
  (Claude Code, opencode, Muse Code, Mistral Vibe). Every other pane comes
  back as a fresh shell in its workspace. A saved command is never re-run:
  re-running `claude "fix the tests"` would re-issue the prompt.
- Each pane's [turns](#turns-review-and-undo-one-agent-turn) come back too,
  numbering continued, and a revived agent stays linked to the
  [issue](#deploying-issues) it was deployed on.
- Exited, ephemeral and sandbox panes are not saved, and neither is the
  [notification history](#notifications).
- A project whose directory is gone is skipped and named; one already mounted
  (a configured project) takes the restored tabs beside its own.
- A save that cannot be read is set aside as `<session>.restore.json.bad` and
  never stops the daemon.

### Attaching from several terminals

Several terminals can attach to one session at once. Each pane takes the
smallest rows and the smallest columns among the clients showing it, as in
tmux, and grows back when the smaller client detaches or switches away; a pane
nobody shows keeps its size. A client whose window is bigger than a pane draws
the pane from its top-left. `tutti watch` watchers never count.

### What the TUI shows of a pane

The TUI runs each pane on the daemon's own terminal engine, so it draws what
the pane's terminal holds: every rendition, underline styles and colours
(Neovim's red undercurl for spelling), overline, OSC 8 hyperlinks and whole
grapheme clusters. Your terminal's cursor takes the focused pane's shape — a
bar in Neovim's insert mode — and a program that asks for focus events hears
when its pane gains or loses the focus, your terminal's included. A program
drawing inside a synchronized update (mode 2026) is shown once the update
ends, never half drawn, and each of the TUI's own frames reaches your terminal
as one such update too.

### One pane in a real terminal

`tutti pane attach <pane>` shows one pane in the terminal you run it in, drawn
by that terminal itself rather than the TUI: a snapshot first, then the pane's
output streamed raw, and your keys typed into the pane. It is the view for
editor terminals — Ghostel in Emacs, Neovim's `:terminal` — and for a pane you
want at full fidelity. The prefix then `d` detaches; the prefix twice sends one
prefix through; the prefix then any other key sends both. The pane is drawn on
the terminal's alternate screen, and your shell's screen comes back on exit,
the terminal's modes restored. A program in the pane that uses an alternate
screen of its own (vim, less) is redrawn from a local copy of the pane on each
switch, so leaving it brings back the pane's shell. The stream votes in the
pane's size like an attached client and follows the window's size. It ends
with `[pane N exited: CODE]` (exit 0) when the pane's program exits, and with
an error (exit 1) when the daemon goes away or the terminal cannot keep up with
the pane's output. It never starts a daemon, and refuses the pane it is run
from.

What the alternate screen costs: the pane has no native scrollback in that
terminal. Nothing else is lost to the redraws: the daemon hands a stream each
switch of screens on its own, so the bell or cursor shape beside one still
reaches your terminal, and it answers the queries a program sends as it
starts itself.

### Terminal queries and colours

A pane has no terminal of its own, so the daemon answers what programs ask
theirs, on the pane's pty, attached or not — each pane runs on tutti's own
terminal engine, [rtty-vt](https://github.com/mcginleyr1/rtty/tree/main/vt),
which answers as a terminal does: device attributes (DA1, and DA2 as xterm
patch 277), the version (XTVERSION), status (DSR), the cursor position (CPR),
the state of a mode (DECRQM), the current rendition and margins (DECRQSS),
the terminal's capabilities (XTGETTCAP: Neovim learns of truecolour,
undercurl and OSC 52 from these) and its size. That is what lets Neovim suspend
on `Ctrl-Z` (it waits on a DA1 answer first), start without probing timeouts,
and a TUI ask where its cursor is. Once a client has reported its terminal's
colours, it also answers the foreground, background and cursor colour queries
(OSC 10/11/12) and light/dark (`CSI ? 996 n`), so Neovim's `background` and
Emacs's `background-mode` match what you see. Nothing reported, nothing is
made up: programs keep their own defaults. Every pane also runs with
`COLORTERM=truecolor`, so Neovim turns on `termguicolors` and Emacs `-nw` uses
24-bit colour whatever terminal the daemon started from.

A program's keyboard protocol is the pane's own: the daemon answers kitty's
keyboard query and keeps the flags a program pushes, and the level of xterm's
modifyOtherKeys it asks for (Neovim pushes kitty's flags; Emacs asks for
modifyOtherKeys). The TUI encodes each key for the focused pane as its program
asked — under kitty's flags Ctrl+I is not Tab, Esc not the start of Alt — and
asks your terminal for kitty's disambiguated keys when it speaks the protocol,
so it can tell them apart in the first place; it hands the terminal back as it
was on detach. Under a terminal without it, keys still reach a kitty pane in
its encoding, but only those your terminal can tell apart. `pane attach` keeps
plain keys: the protocols never reach your terminal there, as it watches for
the prefix in plain bytes and a detach could not be sure to turn them off.

The TUI reports your terminal's colours when it attaches — on the `dark` and
`light` themes, the theme's own pane ground and text, which panes are painted
over. `tutti pane attach` reports them too, and also passes on your terminal's
light/dark switches: the daemon owns mode 2031 for each pane, and tells a
program that set it (Neovim does) of every change, as a terminal would. The TUI
cannot: its input library would stall on the terminal's switch report, so it
keeps its attach-time answer. Reattach after switching the OS theme, and the
programs that set 2031 hear of the change then. A raw stream carries every
query the daemon did not answer, so your terminal answers those and never one
twice. The full table is in `docs/transport-decision.md`.

### When something crashes

The daemon is built to outlive its own bugs. A panic in a background pass —
the render tick, the classifier, detection, the VCS watch — is logged and the
pass carries on; a panic while handling one connection's request drops only
that connection; a lock a panic poisoned is recovered; a failed `accept` is
retried. A pane is the unit of failure. Its terminal engine is built never to
panic, whatever a program writes — it is property-tested on arbitrary bytes
and resizes — but were it to, that pane alone ends: its program is killed,
its screen says the terminal crashed and why, a `pane crashed` note joins the
history, the panic goes to the log, and every other pane runs on. The client
survives its own parser's: a pane update its parser cannot apply
resets the pane and asks the daemon for a fresh copy, and any other client
panic restores your terminal on its way out.

The detached daemon appends its stderr — panics, crash notes — to
`<session>.log` beside its socket (`$XDG_RUNTIME_DIR/tutti/`, else
`/tmp/tutti-<uid>/`); `tutti server start` prints the path. A log past 4 MiB is
trimmed to its newest MiB when the daemon starts.

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

A program can claim them back: while the focused pane carries the user var
`TUTTI_KEYS=pane` (OSC 1337 `SetUserVar`, which tutti's Neovim and Emacs
plugins set), the direct keys reach the pane as its own bytes. An editor then
moves through its own splits first and calls `tutti pane focus --direction` at
its edge (see [Editors in panes](#editors-in-panes)). The prefix is never handed
over, and a pane whose program exited claims nothing.

Everything else lives behind the prefix (`Ctrl+B` by default). The `default`
preset:

| Key | Action |
| --- | --- |
| `%` / `"` | split right / split down |
| `n` / `p` / `c` | next / previous / new tab |
| `w` | focus the workspace/agent sidebar |
| `g` | go to: a full-screen jump list — agents by state over every project, fuzzy-filtered |
| `f` | focus the changed-files panel (per-file diff stats) |
| `N` | notifications: the history of bells, desktop notifications, and agents that blocked or finished |
| `r` | run an agent / shell / command here (opens the launcher) |
| `I` | issues: list your trackers' issues and deploy one to an agent |
| `i` | setup: install the Claude Code hooks, the opencode plugin, the tutti skill, and the shell integration — for you, or (`s`) into the active project |
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
overlay — detach first, then the rest of the active keymap, the direct keys, the
sidebar's and the changes panel's own keys, and how to stop the daemon. The
prefix and panel sections render from the live keymap tables, so they always
match what the keys do.

A full-width **app bar** runs across the top: an accent bar and the bold
`tutti — <session>` wordmark on the left (just `tutti` when the session shares
the name), the tab segments right-aligned (` 1 claude ●  2 logs  + `, the
active one a bold accent block, the rest dim), over a dim rule. Click a segment
to select its tab, the ` + ` to open one. A tab you haven't named borrows its
active pane's title, so segments say what they hold, not just where they sit.
A tab whose agents want you trails a state dot after its name — the most
urgent of its agents: red for blocked, then green for done, else the shared
working spinner — so a blocked agent in a background tab shows from the tab bar
itself; a tab of plain shells carries none. Each pane carries its **title on
its own line above its rounded frame** — an
accent `❯` plus the agent/state for the focused pane, dim for the rest. The
**footer** earns its row: in terminal mode it shows where you are (an accent
dot, the active workspace's name, its branch and jj change stat); leaving
terminal mode swaps in a mode chip (`SIDEBAR`, `SCROLL`, …) on the left, with
the standing hint always on the right; a transient fires a one-line
**notification band** just above it (accent for info, red for errors).

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
whatever you type via your login shell. With [sandboxes](#kubernetes-sandboxes)
configured, `tab` switches where the pick runs — locally, then each sandbox
profile in turn (a `tab run in …` hint row says what is next). The title
follows (` run in <project> · sandbox <name> `). A sandbox's rows are the whole
catalog, since tutti can't see what an image carries, and its shell and
command rows run `sh`. Adding a project (`a` in the sidebar) opens the launcher
for the new project's first pane too — `esc` there just drops you into a shell.
(Startup `[[projects]]` still mount plain shells, no launcher.)

At the foot of the panel sit **resume rows** — `resume   claude · 2h · <first
prompt>` — harvested from the agent tools' own on-disk session stores for the
target workspace's directory, up to three per agent, newest first (read-only;
Claude Code from `~/.claude/projects/…`, each candidate verified against the
`cwd` its own transcript records; Muse Code from
`$XDG_DATA_HOME/muse/sessions/…`, verified against the `workspace_root` its
log names; Mistral Vibe from `$VIBE_HOME/logs/session/…`, verified against
its `working_directory`). Picking one relaunches the conversation in a new
pane via the tool's own resume recipe (`claude --resume <session-id>`, `muse
resume <session-id>`, `vibe --resume <session-id>`), so a conversation
orphaned by a daemon restart — or one from before the project was mounted in
tutti — is one keystroke from continuing. A resume row for an uninstalled
binary dims like any agent row.

An **exited** agent pane advertises the way back in its title — `exited 0 ·
r resume · x close`. `r` opens the launcher over that pane's workspace (resume
rows on top) armed to **replace the corpse**: the pick runs in the dead pane's
tab and the corpse is dropped, so a finished or accidentally-quit agent is two
keystrokes from its own conversation. `x` just closes the corpse. An exited
[sandbox](#kubernetes-sandboxes) pane adds `p pull`, which brings the
sandbox's pushed work back as a workspace under its project.

**Go to** (`Ctrl+B g`): a full-screen jump list over the content area, for
when the sidebar's 30 columns are not enough. With nothing typed it is an
overview: every agent grouped by state — `needs you` (blocked) first, then
`done` and `working` — each row its state dot, its title, and a dim
`workspace · tab · kind`, over every project as a tree in sidebar order (the
project, its tabs numbered as in the app bar and their panes, plain shells
included, then its nested workspaces). Typing filters with a fuzzy subsequence
match over each row's path — project, workspace, tab, pane title, agent kind,
so `web cl` finds web's claude — and collapses the groups into one ranked
list: the tightest match first (runs and word starts count), ties to the more
urgent state, then tree order; `no matches` when nothing fits. `↑`/`↓` or
`C-p`/`C-n` move (letters, `j`/`k` included, always type into the filter);
`enter` or a click jumps — a pane row to its tab with the pane focused, a tab
row to that tab, a project or workspace row to its active tab — and `esc`
closes. The highlight sticks to its row while agents change state underneath
it. The footer chip reads `GO TO`.

**Setup panel** (`Ctrl+B i`): a floating ` setup · user ` panel listing what
tutti installs — the Claude Code hooks, the opencode plugin, the
[tutti skill](#the-tutti-skill), and the [shell integration](#shell-integration)'s
line in your shell's rc file — each with a status dot and word (installed,
needs update, not installed, not tutti's, unreadable) over its target file.
`s` switches to the active project (` setup · project <name> `). `enter` on a
row with something to install shows the change's summary and the head of the
change, then asks `y/N`; `y` writes it — backing up a file it replaces, the
same plan-and-apply the CLI's `--install` uses — and the row updates. A file
that is not tutti's, or one it cannot read, is never written. The shell row
appends its line to the rc file of your `$SHELL` once (a line already naming
`tutti shell-init` counts as installed) and is yours alone: a project has none.
The footer chip reads `SETUP`.

Sidebar edge-nav: at a pane's **left edge**, `Ctrl+h` steps into the sidebar when
it is visible (nvim-explorer muscle memory) instead of jumping straight to the
previous tab; from the sidebar, `Ctrl+l` returns to the pane you left and `Ctrl+h`
goes on to the previous tab. With the sidebar hidden the left edge wraps to the
previous tab as before.

Mouse: click focuses a pane, click a tab segment or sidebar entry to jump.
Drag the border between two panes to resize their split: grab the frame column
on either side where side-by-side panes meet, or the upper pane's bottom edge
or the lower pane's title row where stacked panes meet. The border follows the
pointer cell by cell, held to the same 10–90% bounds as the keyboard resize; a
click there that never moves it still just focuses the pane, and a zoomed pane
has no border to drag. The wheel routes by what the pane's program declared: a
program that asked for mouse events (vim with `mouse=a`, htop) receives the
wheel itself; a full-screen
program without mouse reporting (Claude Code, less) gets arrow keys — the xterm
"alternate scroll" convention — so it scrolls its own view; only a plain
primary-screen pane (a shell) enters tutti's frozen scrollback browse
(`q`/esc exits). Clicks and drags inside a pane whose program asked for them
(Neovim's `mouse` option, Emacs's `xterm-mouse-mode`, htop) go to that program
as they would in a terminal of its own — presses, releases and drags, and bare
motion when it asked for all of it, in its own encoding — so a click places
Neovim's cursor and a drag moves its split borders; a click on another pane
focuses it and reaches its program too. Hold `Shift` to select and copy
there instead (tutti's selection, or your terminal's own where the terminal
keeps `Shift` for itself). Elsewhere, drag inside a pane to select its text;
releasing copies the
selection to your system clipboard (an OSC 52 write your terminal fulfils) and
flashes `copied N chars`, so selecting works even while tutti holds the mouse for
scroll and focus — the selection also reaches the pane's scrollback while you
browse it. Copy needs a terminal that honours OSC 52 (iTerm2, kitty, WezTerm,
Ghostty, tmux with `set-clipboard on`); where it does not (Apple Terminal), press
the prefix then `m` to hand the mouse back for the terminal's own drag-to-select,
and `m` again to re-grab it (most terminals also bypass capture while
`Shift`/`Option` is held). `mouse = false` starts with the mouse off.

**Colour.** The default `theme = "terminal"` takes its colours from your
terminal: everything renders dim, one accent (terminal blue) marks the
focused/active thing — the focused pane's frame and title, the focused panel's
header, the `▎` gutter and name chip on a selected row — the red/yellow/green
state dots say what agents are doing, and green/red `+`/`−` carry diff polarity
in the changes panel, sidebar stat, and footer. The chrome paints no background
of its own — it draws over your terminal's default, so it follows your
terminal's light/dark scheme live, the same way the agent panes do.
`theme = "dark"` and `theme = "light"` are explicit truecolor themes instead: a
shaded chrome (app bar, sidebar, changes panel, footer) a step apart from a
tinted pane ground, muted grey metadata rather than the DIM attribute, and
accent/state/diff colours tuned to that ground — panes' default colours take
the theme's ground too, so the whole screen reads as one surface. Pick the one
matching your terminal; nothing is detected.

## Editors in panes

A Neovim or Emacs in a pane tells tutti it is there through iTerm2's user vars,
`ESC ] 1337 ; SetUserVar=<NAME>=<base64 value> BEL`, which any program — a
shell's `printf` included — can write. Names are `[A-Za-z0-9_]`, up to 64
bytes; values are up to 1 KiB of UTF-8; a pane keeps 16, and an empty value
unsets one. They live as long as the daemon (a restored pane starts with none),
and `tutti --json pane list` and `tutti watch` show them as `user_vars`. tutti
reads two:

- `TUTTI_KEYS=pane`: the direct keys go to the pane (see [TUI keys](#tui-keys)).
- `TUTTI_EDITOR`: `nvim <server address>` (its `v:servername`), or `emacs
  [<server name or socket path>]` — best the absolute socket path, which an
  emacsclient started from any environment finds. Files open in this pane.

`editors/` holds the plugins that speak it; [editors/README.md](editors/README.md)
has the lazy.nvim and `use-package` installs, the keymaps and the Lua API.

- **Neovim** (`editors/nvim`, 0.10 or later) always announces `TUTTI_EDITOR`.
  `require('tutti').setup({ keys = true })` also maps `Ctrl+h/j/k/l` to move
  between Neovim's windows and on to tutti's panes past its edge, and
  `Alt+h/j/k/l` to tutti's split resize, and only then claims the keys. A
  smart-splits.nvim backend (`tutti.smart_splits`) does the same through
  smart-splits' own mappings. `:TuttiAgents` picks an agent and focuses it.
- **Emacs** (`editors/emacs/tutti.el`, 29.1 or later): `tutti-mode` claims the
  keys, so `C-h`, `C-k`, `C-l`, `M-x` and the rest are Emacs's again, and
  announces this Emacs — by its server's socket when `server-start` ran first.
  `tutti-windmove-left`/`-right`/`-up`/`-down` cross from windows to panes;
  `M-x tutti-agents` is a live agent list (`RET` focuses an agent, `p` answers a
  blocked one with the guarded send, `y`/`n` answer a held approval).

Both set their vars when the editor starts or resumes and clear them when it
suspends (`Ctrl-Z`) or exits, so the shell beneath gets tutti's keys back. An
editor that crashed cannot: with the [shell integration](#shell-integration)
the shell's next prompt clears them instead, and the prefix always works.

`tutti edit <path>[:<line>[:<col>]]` opens a file in the workspace's editor
pane — the caller's own tab first, then layout order — through `nvim --server …
--remote-expr` or `emacsclient -n`, and focuses that pane, so the attached TUI
jumps there. It returns at once and is not a blocking `$EDITOR`. From outside a
pane, pass `--workspace <id>`; with no editor pane it says `no editor pane in
workspace <name>` and exits 1. The changes panel's `e` is the same hand-off
from the TUI.

`tutti pane focus --direction left|right|up|down` and `tutti pane resize
--direction …` give an editor's split navigation (smart-splits.nvim and the
like) tutti's side of the move. Both start from `--from`, else the calling
pane, else the attached TUI's focused pane. A focus prints the pane it moved
to. At a left or right edge of the pane the attached TUI shows focused, the
TUI takes the move on as `Ctrl+h`/`Ctrl+l` would — into the sidebar or the
changes panel, else to the previous or next tab — and the command prints
nothing and exits 0, so an editor's edge leads where tutti's own keys do. At
any other edge it prints `no pane <direction> of <id>` on stderr and exits 1,
unless `--wrap` goes round to the farthest pane on the other side of the tab,
level with it. A resize moves the nearest split border that
way by `--amount` of the split (0.05, the `Alt+h/j/k/l` step). See
[Known limitations](#known-limitations) for what an editor in a pane does not
get yet.

## Shell integration

Add one line to your shell's rc file, or let the setup panel (`Ctrl+B i`, row
*shell integration*) append it for you, with a backup:

```sh
eval "$(tutti shell-init zsh)"   # ~/.zshrc (bash: ~/.bashrc, `shell-init bash`)
tutti shell-init fish | source   # ~/.config/fish/config.fish
```

Inside a tutti pane the shell then reports its working directory (OSC 7) and
marks each prompt and command (OSC 133); elsewhere it does nothing, and loading
it twice changes nothing. With it:

- a split (`%`, `"`, `tutti pane split`) starts in the directory you are in
  when that lies inside the workspace, and in the workspace's directory
  otherwise;
- `tutti --json pane list` shows each pane's `cwd`;
- `tutti pane read <pane> --last-output` prints the output of the last command
  that finished, and `exit: N` on stderr. While a command runs you get the one
  before it. It is exact to the cell: zsh's `%` marker and a prompt sharing the
  row are not part of it. While a full-screen program holds the alternate
  screen it is refused, and a pane without the integration says `no finished
  command`;
- each prompt clears the editor vars (`TUTTI_KEYS`, `TUTTI_EDITOR`), so an
  editor that crashed without clearing them hands the keys back.

bash needs 4.4 or newer for `--last-output` (`PS0`); macOS's `/bin/bash` 3.2
reports only the directory. The fish snippet has not been run live yet.

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
diff_panel = "on"       # changed-files panel: "on" (default) or "off"
icons = "unicode"       # sidebar glyph set: "unicode" (default) or "nerdfont"
theme = "terminal"      # "terminal" (default: your terminal's colours, live light/dark),
                        # "dark", or "light" (shaded truecolor chrome)
notifications = true    # re-emit pane bells/notifications to your real terminal
approvals = "watch"     # Claude permission requests: "watch" (default: block the pane,
                        # Claude shows its dialog) or "hold" (answer from tutti)
approval_timeout_secs = 120  # a held request falls through to Claude's dialog after this (1–599)

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

# Kubernetes sandboxes agents can launch into (see "Kubernetes sandboxes").
[[sandboxes]]
name = "gke"              # required; [A-Za-z0-9_-]+, what the launcher and --sandbox call it
context = "gke_acme_prod" # kubectl context; default: the current one
namespace = "agents"      # default: the context's namespace
warm_pool = "claude-pool" # a SandboxClaim on this SandboxWarmPool…
# image = "ghcr.io/acme/agent:1"  # …or a Sandbox running this image (set exactly one)
container = "agent"       # the container to exec into; default: the pod's default
inject_env = true         # warm_pool only: TUTTI_GIT_REMOTE/TUTTI_BRANCH on the claim
                          # (the template must allow it; costs the warm pod; default false)
ttl_minutes = 60          # how long an orphan outlives a crashed tutti (default 60)

# Issue trackers to deploy from (see "Deploying issues"). Every key but `name`
# and `kind` is optional.
[[issues]]
name = "gh"               # [A-Za-z0-9_-]+, what the panel and --source call it
kind = "github"           # through your own `gh` (it is the auth)
repo = "acme/api"         # default: the repo the project's git remote names
search = "label:agent"    # a `gh issue list --search` query
limit = 50                # default 50

[[issues]]
name = "linear"
kind = "linear"           # Linear's API through `curl`
api_key_env = "LINEAR_API_KEY"  # where your personal API key is (the default)
team = "ENG"              # only this team's issues
# filter = { priority = { lte = 2 } }  # an IssueFilter; replaces the default
                          # (your assigned issues not completed, canceled or duplicate)

[[issues]]
name = "jira"
kind = "command"          # any tracker: prints [{id, title, body?, url?, labels?}]
command = ["~/bin/jira-issues", "--json"]
prompt = "Fix {id}: {title}\n\n{body}\n\n{url}"  # any kind; the agent's first message
```

Chord syntax is `C-<char>` (Ctrl), `A-<char>` (Alt), or a bare printable
character. The top-level keys and `[keys]` above are exactly the defaults, so a
file that reproduces them changes nothing; `[[projects]]`, `[[sandboxes]]` and
`[[issues]]` are examples.

**Presets.** `preset` selects the *prefix* keymap table. The which-key popup and
help overlay always render whichever preset is active, so they stay accurate.

- `default` — the emacs/tmux-flavored table above (`%`/`"` split, `x` kill,
  `d`/`q` detach, …). The `C-b` prefix chord is itself already emacs-style.
- `vim` — mnemonics vim users reach for: `v`/`s` split right/below, `h/j/k/l`
  directional focus, `q` kill pane (`:q` closes a window), `d` detach (so detach
  stays reachable), `t` new tab, `n`/`p` next/prev tab,
  `w`/`g`/`f`/`N`/`r`/`I`/`i`/`z`/`[`/`m`/`?` unchanged.

An unknown preset is a hard error. A dedicated `emacs` preset may land later; for
now `default` already covers that muscle memory. The `[keys]` direct bindings are
shared across presets and override the defaults on top (e.g. set
`focus_left = "none"` to reclaim `C-h` regardless of preset).

**Chrome colors.** tutti's chrome — the app bar, sidebar, footer, and the
which-key/help/notification panels — paints no background of its own. It draws
over your terminal's own background, so when your terminal flips light/dark (for
example following the OS appearance) the chrome flips with it, live, exactly like
the agent panes do. The only background tutti paints is the selected sidebar
row's highlight, and that uses an ANSI palette colour (not a fixed RGB), so the
terminal repaints it from its own scheme too. At attach, tutti asks the terminal
once whether its background is light (OSC 11). If it is, the highlight uses ANSI
7 (white) rather than ANSI 8 (bright black), which some light schemes set to
pure black. That choice lasts until you reattach. There is nothing to configure.

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
agents at once — a borderless panel, each section
labelled by a quiet lowercase header (` ▾ projects`, ` ▾ waiting` — bold but
dim, so weight carries the hierarchy without brightness; caps stay reserved
for the footer's mode chip) with a `▾`/`▸`
collapse arrow and a right-aligned count. A blank spacer row above the
waiting header keeps the two sections from running together. Names render at full intensity and
everything else recedes dim, so the eye lands on identity; the focused row
pops in the accent colour. Two stacked sections:

- **projects** — a tree. One row per top-level project with **its agents
  nested beneath it** on `├`/`└` guides, then any [nested
  workspaces](#workspaces) (indented, guided, same two-line row, name bold),
  each followed by *its* agents one level deeper. A workspace row is an `●`
  (accent) when it owns the active tab, else a dim `○`, then the bold name —
  with right-aligned roll-up chips (`● 2 ● 1`) counting the subtree's
  blocked/working/done agents — over a dim line showing a branch marker and
  the git branch (read straight from `.git/HEAD`, including worktrees) on the
  left and the jj change stat (`4 files +120 −33`) right-aligned — the
  workspace's whole delta **against trunk**, not just its working copy, so it
  tracks what the workspace has built up rather than going blank each time an
  agent commits. No branch
  leaves the left blank rather than echoing the name; a workspace clean
  against trunk or non-jj shows no stat. The stat refreshes as agents work (on
  every state transition, on attach, when a workspace is created, and whenever
  jj records an operation in the workspace — a commit, bookmark move, or push
  from any shell) and is dropped first
  when the column is too narrow for both. A nested [workspace](#workspaces)
  whose `@` was rewritten from another workspace shows a dim-red `stale` tag
  in the stat's place until you run `workspace update`. An agent row is a
  state dot (blocked red, working an animated spinner, done green,
  idle/unknown dim), the pane title (then, dim, the tracker issue it was
  [deployed on](#deploying-issues) — `#123`, `ENG-42`), and a dim `state ·
  kind` line, with any hook-reported subagents hanging below on their own
  guides. Agent rows keep **pane order** — a state change never reshuffles the
  tree under your cursor; attention order lives in the waiting queue.
  Selecting a workspace jumps to its tab; selecting an agent jumps to its
  pane, switching workspace and tab as needed. A pane that rings a bell or
  fires a desktop notification while in the background gets a 🔔 mark that
  clears once you focus it or its [notifications](#notifications) are read.
- **waiting** — the cross-project attention queue: every **blocked** or
  **done** agent from *every* project, blocked first, each over a dim
  `project · kind` line naming where it lives (subagent rows stay in the
  tree). An agent holding a [permission request](#held-approvals) leads the
  queue whatever its state, its row reading the tool and its input (`Bash ·
  rm -rf build`). `Enter` (or a click) jumps to the agent itself, so a stuck
  agent two projects away is never invisible and always one keystroke away —
  and `p` answers it without going there at all
  ([below](#answering-from-the-queue)).

The app bar answers "does anything need me?" from across the room: after the
session title it carries a state census (`● 1 blocked · ⠋ 2 working · ● 1
done`) counting every agent in the session, empty when nothing runs, and ends
with the unread notification count (`🔔 N`, dim at zero) — click it to open the
[notifications panel](#notifications).

The selected row (while the sidebar is focused) takes a subtle full-row
highlight with its name rendered as an accent chip, unmistakable even with a
single entry. The highlight follows the row's **identity**, not its position:
when a kill or a state change reshapes the list, the cursor stays on the row
it was on (falling back to the row's project when the row itself vanished),
and a kill confirm re-checks its target before firing — a confirm can never
land on the wrong workspace or agent. Click a section header (the `projects`
or `waiting` row) to collapse that section down to its header, or click again
to expand it.

Press the prefix then `w` to focus the sidebar (revealing it if hidden); the
cursor lands on the active project's row. Its keys — like the changes panel's
— come from one table that drives dispatch, the footer's hints (the verbs) and
the help overlay (every key), so the three never disagree. While focused,
`j`/`k` (or the arrows) move the highlight across both sections, `Enter`
jumps and hands focus back to the pane, `p` peeks at a blocked agent to answer
it in place, and `esc` unfocuses (esc is the back key
throughout). `w` opens **guided [workspace](#workspaces) creation** on the
selected project (below); `n` starts a **new agent** (or shell/command) in the
selected project: it jumps to that workspace and opens the launcher over it, and
the choice lands in a **fresh tab** there rather than splitting whatever that
tab already holds (an agent row targets the workspace that owns it) — reach for
`n`, not `w`, when you just want another agent in the checkout you already
have; splits stay behind `Ctrl+B %`/`"` and `Ctrl+B r` runs into the current
tab. `a` opens the
**add-project** prompt — a one-line field for the path to an *existing* directory
to open (it mounts that directory as a workspace; it never creates one). The
field prefills the common parent of your current projects, so you type just the
project's name, and it completes directories as you type: `Tab` fills in the
highlighted match and opens it (a trailing `/` reveals its contents), `↑`/`↓`
move the highlight, and `Enter` always opens whatever is typed. `~` expands to
your home; relative paths resolve against the client's cwd and are canonicalized
to an absolute path before the daemon sees them; a bad directory surfaces the
server's error. `d`
opens the selected workspace's **jj diff against trunk** in an ephemeral pane — a
real terminal paging the workspace's whole delta since it forked from `main`
(else `master`/`trunk`), coloured, that vanishes the moment you quit `less`
(pressing `d` on an agent row opens the diff for the agent's workspace). It
diffs the *accumulated* work, not just the working copy — agents commit as they
go, so a bare `jj diff` would almost always be empty. A
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
jumps straight to the entry clicked; a right-click on a blocked agent's row
peeks at it instead.

Visibility is set by the `sidebar` config value: `on` (default) always shows it
— the control column is the point — while `auto` reveals it only once the
session is worth surfacing (more than one workspace, or at least one agent pane)
and `off` keeps it hidden until you focus it with `w`. It is suppressed on very
narrow terminals so panes keep usable room. With the sidebar off, a **blocked**
agent still rings the bell and turns its pane border red until you focus it.

### Answering from the queue

`p` on a **blocked** agent's row (in the waiting queue, or its tree row) — or a
right-click on it — opens a **peek**: a floating ` answer <agent> · <workspace> `
panel showing that agent's live prompt, the tail of its screen (exactly the
rows the safety check below covers), drawn from the screen tutti already
mirrors, so it updates as the agent redraws. (A row holding a permission
request opens the [approval overlay](#held-approvals) instead.) While it is
open, answer keys go
straight to that agent: digits and letters (`1`, `y`/`n`, …), `Enter`, the
arrows, `Tab`, `Backspace`. Nothing else moves — your focused pane, tab, and
layout stay exactly where they were, and closing the peek leaves you on the
sidebar row you opened it from. `esc` closes it and is never forwarded (esc is
the back key throughout); for an esc or free-form typing, close the peek and
press `Enter` on the row to jump into the pane. Clicks inside the panel do
nothing (the pane beneath never takes focus); a click outside closes it. The
footer shows an `ANSWER` chip while it is up.

Every key is a **guarded send**: tutti fingerprints the prompt you are looking
at (the last 12 non-empty rows of the agent's screen, trailing whitespace
ignored) and the daemon re-reads the live screen before writing. If the prompt
changed since you looked — the agent moved on, or someone answered from
another client — the key is refused and **nothing is written**; the reason
flashes in the notification band and the peek stays open over the new prompt.
The first answer to a prompt wins: until the agent visibly redraws, a second
answer to the same prompt is refused too. A guarded send also only ever goes to
a pane that is blocked. Once the agent leaves the blocked state the peek closes
itself.

Scripts get the same guard (`--json` works on both):

```sh
fp=$(tutti pane read 3 --fingerprint)   # take the fingerprint first…
tutti pane read 3 --lines 12            # …then read what it covers and decide
tutti pane send 3 --text 1 --expect "$fp"  # refused if the prompt moved on
```

Taking the fingerprint *before* reading keeps it safe: if the screen changes in
between, the fingerprint is the older one and the send is refused.

### Held approvals

With the [Claude Code hooks](#claude-code-integration) installed, a permission
request blocks its pane the moment Claude is about to ask, naming the tool and
its input. By default (`approvals = "watch"`) that is all: Claude shows its own
dialog, which you answer in the pane or through a peek. Set `approvals =
"hold"` and the hook holds the request for an answer from tutti instead,
keeping Claude's dialog back:

- The agent's waiting row reads `Bash · rm -rf build`, and a held approval
  leads the queue whatever the pane's state.
- `p` on that row (its tree row too), or a right-click, opens an **approval**
  overlay framed and placed like the peek: the tool over its input, line
  breaks shown as `⏎`, long input cut with `…` past 500 characters. `y` allows
  the tool call, `n` denies it, and `esc` passes it on to Claude's own dialog,
  which then appears in the pane. The footer shows an `APPROVE` chip; a click
  outside closes the overlay without answering.
- When the pane you are looking at is the one asking, the overlay opens over
  it by itself, the way Claude's dialog would, and ignores keys for its first
  800 ms, so a `y` or `n` already on its way into the pane can't answer it. It
  never opens over a panel, prompt or overlay you are using, and it closes
  itself once the approval ends — answered from another client or the CLI,
  timed out, or the pane gone.
- Scripts and agents answer with `tutti pane approve <pane> [--deny]
  [--message <reason>]`. The reason is what Claude is told; it defaults to "the
  user denied this in tutti". `tutti --json pane list` shows a held approval as
  `approval: {id, tool, summary}`.

A held request falls through to Claude's own dialog — the hook printing
nothing and exiting 0 — when no TUI is attached to answer it, when the pane
already holds one (parallel subagents ask one at a time), on `esc`, and when
nobody answers within `approval_timeout_secs` (default 120, at most 599:
Claude stops a hook at 600 s). The hook never fails a Claude session. An allow
or a deny from tutti sets the pane **working** at once — the tool runs, or the
agent hears the refusal. Answered in Claude's own dialog instead, the pane
stays blocked until Claude's next tool event, since Claude has no "permission
granted" hook: an approved long `cargo test` then reads as blocked for its
whole run.

### Changed files panel

The right-hand column mirrors the sidebar for *code*: whenever the active
workspace has changes against trunk, a borderless `changes` panel lists every
changed file — directory dim (clipped by whole components behind `…/`, never
mid-name fragments), basename bright — with its exact `+added −removed` counts
in green/red and a six-cell bar scaled to the largest change in the list
(added cells green, removed red), so the shape of the delta reads before the
numbers do. The header carries the file count and the coloured totals. Always
on and refreshed as agents work and on every jj operation in the workspace
(same probe as the sidebar stat, so the two never disagree); it disappears
when the workspace is clean and the pane in view has no turns to list (see
below), on narrow terminals, or with `diff_panel = "off"`.

`Ctrl+B f` focuses it — or `Ctrl+L` from the rightmost pane, the way `Ctrl+H`
steps into the sidebar — and while it holds the keys an accent rail runs down
its edge (the sidebar gets the same). It moves like vim (or `less`): `j`/`k`
step, `g`/`G` jump to the first/last file, `Ctrl+D`/`Ctrl+U` move half the
panel (arrows, `Home`/`End`, and `PageUp`/`PageDown` work too). `Enter` opens
the highlighted **file's diff** against trunk in an ephemeral pager pane, `d`
opens the **whole diff**, and `e` opens the **file itself** in the workspace's
editor pane — a Neovim or Emacs in one of its panes that advertises itself
(see [Editors in panes](#editors-in-panes)), the focused pane's tab first — and
jumps there; the band says `opened <file> in <pane>`, or why not (`no editor
pane in <workspace>`, or the editor's own complaint). The editor's remote CLI
runs off the TUI's loop, so a slow or dead editor never freezes the screen;
`tutti edit` is the same hand-off for scripts. `h`/`esc` step back to the panes
and `l`/`Ctrl+L` wrap on to the next tab, just as the pane's own right edge
does. Clicking a row jumps straight to that file's diff. Like every diff
surface, it answers
"what has this workspace
changed that hasn't landed on trunk" — the delta since the fork point from
`main` (else `master`/`trunk`), so a workspace that fell behind an advanced
trunk never shows trunk's own progress as noise.

### Turns: review and undo one agent turn

Every time an agent's turn ends — its pane leaves **working** for done or
blocked — tutti records a **checkpoint**: the commit id of the workspace's jj
`@` right then. jj keeps a superseded `@` addressable by id, so one turn can be
diffed and restored long after later edits replace it. The snapshot is the one
tutti's change probe already takes (no extra jj operation per turn), and a turn
that changed nothing records none. Where the agent **started** is checkpointed
too — when tutti first detects the agent, or when the pane first enters working
— so turn 1 has a "before" to diff and restore to.

When the pane in view has checkpoints, the changes panel grows a `turns`
section under the files: the newest five, `#N age <turn> +A −B` — the turn
reads as the first line of the prompt that started it when Claude Code's hooks
reported one, else as what closed it: `done`, `blocked`, `start`, `before
restore`. A turn that ends blocked keeps its prompt for the work after you
answer. The cursor walks
from the last file on into the turns; `Enter` on a turn pages **that turn's
diff** (from the checkpoint before it) in an ephemeral pager pane, and `r` asks
`restore the working copy to #N? y/N`. A restore rewrites the workspace's
working copy to the turn's state (`jj restore --from <checkpoint>`): it is
refused while the agent is working or blocked (the agent would write straight
over it) and in a stale workspace, and refusals land in the notification band.
Before restoring, tutti saves the current state as a `before restore`
checkpoint (when anything changed since the last one), so the restore is itself
undoable from the same list — and jj's op log (`jj op log` / `jj undo`) keeps
every restore reversible regardless.

Known limits: checkpoints live in the daemon's memory (newest 50 per pane) and
are saved with the layout, so a pane a restart restores keeps its list and
numbering (see [Restoring after a restart](#restoring-after-a-restart)).
Several agents in one workspace share a working copy, so a pane's turn diff
includes anything else that changed in that window (another agent's edits, your
own). A turn interrupted with Esc sends no stop, so its edits land in the next
turn's checkpoint, under the next prompt. A restore rewrites the working copy
only — work an agent committed stays in history, with `@` carrying the
reverting change. Workspaces outside jj get no checkpoints.

### Notifications

Agents inside panes emit terminal bells and desktop-notification escapes (OSC 9,
OSC 777). Tutti captures these from each pane's output and, for a **background**
(non-focused) pane, flashes the text in the status bar and re-emits a bell plus
an OSC 9 to *your* real terminal — so wezterm/kitty/etc. raise an actual desktop
notification naming the pane. The focused pane is left alone (you are already
looking). The status flash and re-emit are gated by `notifications = true`
(default); the sidebar 🔔 mark is always on regardless. ConEmu's numbered
`OSC 9;N` subcommands are never notifications: `9;4` is a progress report (see
[Agent state sources](#agent-state-sources)), and the rest (`9;9;<cwd>`, which
shells emit for Windows Terminal, and others) are ignored — a body that merely
starts with digits (`9;42 tests passed`) still notifies. An OSC 7501 report's
`msg` notifies when it finishes or blocks the pane, and a herdr report's
blocked message when it blocks it — at most once per 30 s per pane.

**History.** The daemon keeps the newest 200 notes: every bell and desktop
notification a pane raises (a herdr `notification.show` included), and every
attention transition — an agent blocking, or finishing a turn (working →
done). Each keeps the pane's title and workspace as they were, so it outlives
the pane. `Ctrl+B N` opens the **notifications panel**, newest first: an
unread dot, the age, the pane, a dim `workspace · kind`, and the text. `j`/`k`
move, `enter` jumps to the note's pane and marks its notes read, `r` marks
everything read, `X` clears the history after a `y/N`, and `esc` closes; a note
whose pane is gone stays listed, dim. The footer chip reads `NOTES`, and the
app bar's `🔔 N` chip opens the panel too.

Focusing a pane marks its notes read, and a pane whose notifications have been
read — anywhere, by any client or script — loses its sidebar 🔔; attaching
brings back the 🔔 of panes with unread ones. A bell from the pane you are
looking at is not recorded (a bell says nothing else), nor is a bell repeating
its pane's still-unread bell; any other note about a pane in view lands already
read. Scripts and agents read the same history: `tutti notes [--unread]`
(newest first: `ID AGE PANE TITLE WORKSPACE KIND UNREAD TEXT`; `--json` for
the raw notes), `tutti notes read [--up-to <id>] [--pane <id>]`, and `tutti
notes clear`. The history lives in the daemon's memory: a restarted daemon
starts it empty.

### Workspaces

A **workspace** is a jj checkout nested under a **project** (a repo you opened) —
tutti's unit for fanning an agent onto its own isolated copy of the tree, without
touching the checkout you are working in (jj is the required VCS — there are no
git/hg adapters). A workspace renders **indented beneath its project** in the
sidebar and carries the project's own branch/changes/stale line.

**From the CLI.** `tutti workspace add <project> --name <name>` runs `jj
workspace add` and places the checkout beside the project's repo
(`<repo>-<name>`); `--dest <path>` chooses the directory instead (the TUI's
`where:` step), and `-r <rev>` the revision it starts from. The name must be
`[A-Za-z0-9_-]+`, since it becomes both a path component and a jj workspace
name; the project must live under a `.jj` repo, and the destination must not
already exist. `tutti workspace list` shows every entry with the PARENT it is
nested under, `tutti project list` just the projects. Merge is scriptable too
(`tutti workspace merge <id> [--push]`), keeping the "every TUI action is a CLI
verb" invariant.

Creating a workspace is **not** "run another agent in this project". When you
just want a second agent in the checkout you already have, use `n` (new agent)
on the sidebar row instead: it opens the launcher over that same workspace, in
a fresh tab, no new directory.

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
variant. (After a merge, the cleanup prompt is the `--discard` path.) A
top-level project leaves with `tutti project kill <id>`, which never touches
its directory.

## Deploying issues

Point tutti at your issue trackers (`[[issues]]` in the
[configuration](#configuration)) and an issue becomes an agent in a keystroke.
`Ctrl+B I` opens the **issues panel** over the current project
(` issues · <project> `): every source's open issues, fetched in the background
with your own tools, so the TUI never waits on a tracker and the daemon never
talks to one.

- **GitHub** goes through your `gh` (it is the auth), run in the project's
  directory so it picks the repo from the git remote — set `repo` where it
  can't, as in a jj repo without a colocated git checkout.
- **Linear** goes through its GraphQL API with your personal API key, read
  from `LINEAR_API_KEY` (or the variable `api_key_env` names). The key reaches
  `curl` on stdin — never a command line, never the config, never printed. By
  default the panel lists your assigned issues that are not completed,
  canceled or duplicate; `team` narrows that, and `filter` (a Linear
  `IssueFilter`) replaces it.
- **Any other tracker** — Jira, GitLab, a script of your own — works through a
  `command` that prints a JSON array of `{id, title, body?, url?, labels?}`,
  run in the project's directory.

A source still fetching shows `loading…`; one that fails shows why, dim red.
`/` filters (the go-to's fuzzy match, over source, id, title and labels),
`j`/`k` move, and `esc` closes. `enter` opens the run launcher armed for the
issue — ` deploy #123 · Fix the login ` — offering only the agents that take a
first message on their command line (Claude Code, Codex, Gemini CLI, OpenCode,
Muse Code, Mistral Vibe, Ori; Crush's interactive mode takes none), `tab`
cycling sandboxes as usual and `esc` backing out to the panel. Picking one
creates a **workspace for the issue** under the project, named after it
(`gh-123`, `eng-42`), starts the agent there with the issue as its first
message, jumps to it, and flashes `deployed #123 to claude`. That message
defaults to `Work on this issue and commit your changes when done.` followed
by the id, title, body and link; set `prompt` per source to change it (`{id}`,
`{title}`, `{body}`, `{url}`). `h` in the panel deploys into a fresh tab of the
project itself instead; a project that isn't a jj repo always does. The footer
chip reads `ISSUES`.

The agent's sidebar row and pane title carry the issue id (`claude #123`),
`tutti --json pane list` carries its link (`issue: {source, id, title, url}`),
and a [restore](#restoring-after-a-restart) that revives the agent keeps it.
tutti only reads trackers: it never comments on, assigns, or moves an issue.

Scripts get the same. `tutti issue list [--source <name>]` fetches in the
current directory and prints a table of `SOURCE ID TITLE LABELS URL` (`--json`
for the issues); a failing source is named on stderr and exits 1 once the rest
are listed. `tutti issue deploy <source> <id> [--agent claude] [--project <id>]
[--here] [--sandbox <profile>]` deploys into the project open at the current
directory unless `--project` names one, and prints the new pane's id.

## Agent state badges

Panes running a detected agent show live state on the pane border and in the
sidebar tree: **blocked** (red — needs your input; leading the waiting queue
and reddening the pane border), **working** (yellow, an animated braille
spinner), **done** (green — finished, not yet viewed; focusing it makes it
**idle**). The pane's border title carries the same `agent · state` suffix;
a plain shell shows just its name. Background panes ring the terminal bell when
they block or finish. Detection covers the whole catalog under
[Install](#install), with screen patterns tuned per agent where they have been
checked live; adding an agent is one data-table row in
`crates/tutti-agents/src/registry.rs`.

### Agent state sources

A pane's state can come from five sources, ranked highest first:

1. **hook** — the agent's own exact lifecycle signals: its hooks (`tutti hooks
   claude`, `tutti hooks opencode`) or a
   [herdr report](#herdr-compatible-reporting).
2. **program** — the pane's program reporting itself with OSC 7501.
3. **progress** — an OSC 9;4 progress report (ConEmu / Windows Terminal /
   Ghostty).
4. **screen** — the classifier reading a detected agent's screen.
5. **activity** — output flowing (a finished agent that prints again is working).

Hook, program and progress are *claims*: a claim holds the state until its own
source ends it, and nothing ranked below may move it meanwhile — so a build that
reports `done` stays done while it keeps printing, and a hook-driven agent's own
progress bar never fights its hooks. The one exception: a screen **blocked**
match (a visible permission dialog) overrides a program/progress **working**
claim, which is likelier stale than right. A hook claim holds until the pane's
child exits; a herdr report's claim also ends when its agent releases or clears
it, or when another agent is detected. A program/progress claim ends when that
source clears it, when the pane's child exits, or when its detected agent
changes — and when you type into the pane (not a guarded answer) while it holds
the pane done or idle, so a reporter that never sends its clear cannot shut out
the next report; the state stays until something moves it. `tutti --json pane
list` shows each pane's `source`.

**Any pane may report**, not just a detected agent. A shell pane whose program
reports shows wherever agent state does — the sidebar row with its dot, the
waiting queue, the app-bar census, the pane border — labelled by its OSC 7501
`app`, else the pane title, for as long as the report has something to say (a
live claim, or a done/blocked you have not looked at yet).

**OSC 9;4** — `ESC ] 9 ; 4 ; <state> [; <percent>]` then ST or BEL. States 1
(normal), 3 (indeterminate) and 4 (paused) are **working**; 2 (error) is
**done** (tutti has no errored state; done puts it in the waiting queue); 0
clears — a pane it held working settles to **idle**. The percentage is not
shown.

```sh
printf '\e]9;4;1;40\e\\'   # working, 40%
printf '\e]9;4;0\e\\'      # clear
```

**OSC 7501** ([program status](https://tuios.dev/docs/program-status.md)) —
`ESC ] 7501 ; key=value:key=value… ST` (or BEL). `state` is required: `idle`,
`working`, `done`, `blocked`, `error` (shown as done) or `clear` (ends the
claim; working/blocked settle to idle, an unseen done stays). `app` labels the
pane; `msg` (base64) becomes a notification on a move to done/blocked, at most
every 30 s; `kind` (blocked only), `progress` (0–100) and `title` (base64) are
validated but not shown. A malformed report — missing/unknown state, `kind`
off blocked, out-of-range `progress`, an over-long field, or a `msg`/`title`
that is not base64 of control-free UTF-8 — is dropped whole. tutti tracks only
the root record (a report with an `id` is ignored) and does not answer the `?`
support probe.

```sh
printf '\e]7501;state=working:app=build\e\\'
printf '\e]7501;state=done:app=build:msg=%s\e\\' "$(printf 'Built' | base64)"
```

**Stall timer.** A **working** pane not held by a hook that goes 30 s with no
output and no classifier working match drops to **unknown**, ending its claim —
a build killed mid-report never says clear, and a wedged agent never says done.

### herdr-compatible reporting

Every pane also carries [herdr](https://github.com/ogulcancelik/herdr)'s
environment — `HERDR_ENV=1`, `HERDR_SOCKET_PATH`, and `HERDR_PANE_ID` /
`HERDR_TAB_ID` / `HERDR_WORKSPACE_ID` in herdr's `w1:p3` shape — pointing at a
per-session socket beside tutti's own (`<session>.herdr.sock`, owner-only). An
agent that reports its lifecycle to herdr natively reports to tutti with no
install step; Crush does this out of the box.

- Reports rank as **hook**. `working` and `blocked` map straight over, and a
  blocked report's message raises a notification.
- `idle` reads as **done** after work the agent reported, else **idle**; a done
  you have not looked at stays done.
- The report's `agent` labels the pane, and its `agent_session_id` names the
  pane's `conversation`.
- Quitting releases the pane: the claim ends and the state settles.

tutti speaks herdr's v0.9.3 reporting protocol: `pane.report_agent`,
`pane.report_agent_session`, `pane.release_agent`, `pane.clear_agent_authority`,
`notification.show` (on the pane its `pane_id` names), `pane.report_metadata`
(acknowledged, not shown) and `ping`. `HERDR_*` variables inherited from
outside are dropped, so a tutti pane never reports to a herdr that tutti itself
runs inside.

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
hand it goes at the settings.json **top level**. From the TUI, `Ctrl+B i`
installs the same thing through the setup panel. Installed before prompt labels
or approvals existed? Re-run `tutti hooks claude --install`: it adds just the
missing `UserPromptSubmit`, `SessionStart` and `PermissionRequest` entries and
leaves the rest alone.

Each wired event runs `tutti agent-event claude`, which reads the hook JSON on
stdin, maps it, and forwards it to the pane's daemon:

- a tool use keeps the agent **working**; a submitted prompt
  (**UserPromptSubmit**) starts a turn, and its first line labels that turn's
  checkpoint in the [turns](#turns-review-and-undo-one-agent-turn) list;
  **Stop** marks it **done**;
- a **PermissionRequest** marks it **blocked** the moment Claude is about to
  ask, naming the tool and its input (`Bash · rm -rf build`); with `approvals =
  "hold"` the request waits for an answer from tutti instead of Claude's dialog
  (see [Held approvals](#held-approvals));
- a **Notification** that waits on you — a permission prompt, an MCP dialog, a
  background agent's question, an Enter after a usage limit — marks it
  **blocked** too; idle reminders, auth and usage-limit reports don't (Stop
  already marked the turn done), and a type tutti does not know yet blocks, to
  be safe;
- **SessionStart** and every event's `session_id` record the conversation the
  pane holds — `conversation` in `tutti --json pane list`, resumable with
  `claude --resume <id>` and revived by a
  [restore](#restoring-after-a-restart); it follows `/clear` and `--resume`,
  and is dropped when the agent leaves the pane;
- a **Task** (subagent) spawn adds a dim indented sub-row under the agent in the
  sidebar — a shared spinner while it runs, a `·` once it finishes — and
  **SubagentStop** completes it; finished sub-rows clear when the turn ends.

`agent-event` is deliberately fail-safe: outside a tutti pane (no `TUTTI_PANE`),
on a malformed or irrelevant event, when it cannot reach the daemon, or when
the daemon has not answered within 2 s, it exits 0 with at most a note on
stderr — a hook never breaks or stalls a Claude session, inside tutti or out (a
held approval waits out its own `approval_timeout_secs` instead).
It never writes to stdout, which Claude Code feeds to the model on a prompt or
a session start — except a held approval's decision, which Claude reads from
it. If you ever uninstall tutti while the hooks remain, Claude Code will
surface a non-blocking "command not found" warning on every tool call — remove
the entries (restore `settings.json.bak` or delete them) when you go.

Hooks are ground truth: once a pane reports one, its state is the hook's alone
— tutti stops classifying that pane's screen, and the agent's own OSC 9;4 /
7501 reports are ignored (see [Agent state sources](#agent-state-sources)).
Agent **detection** (which kind of agent runs there) is unaffected, so the badge
still appears. Display only — tutti never manages a foreign agent's subagents.

## opencode integration

opencode reports to tutti through a plugin. Install it (shows the plugin and
asks first):

```
tutti hooks opencode --install            # into ~/.config/opencode/plugins/tutti.js ($XDG_CONFIG_HOME honoured)
tutti hooks opencode --install --project  # …into ./.opencode/plugins/tutti.js instead
tutti hooks opencode --install --yes      # skip the confirmation (scripts)
tutti hooks opencode                      # or just print the plugin (JavaScript)…
tutti hooks opencode --raw                # …without the stderr instructions, for piping
```

`--install` is idempotent, backs a previous tutti plugin up to `tutti.js.bak`,
writes atomically, and refuses to overwrite a `tutti.js` that is not tutti's.
From the TUI, `Ctrl+B i` installs the same plugin through the setup panel.
Inside a tutti pane the plugin pipes each relevant opencode event to `tutti
agent-event opencode` (outside one it does nothing):

- `session.status` **busy**/**retry** keeps the agent **working**; **idle**
  (and `session.idle`, `session.error`) marks it **done**;
- `permission.asked` and `question.asked` mark it **blocked**; the matching
  `permission.replied` / `question.replied` / `question.rejected` puts it back
  to **working**;
- a subagent's child session never moves the pane on its own busy/idle (its
  permission prompts still block it);
- the root session's id (from its `session.*` events — never a subagent's) is
  recorded as the pane's `conversation`, resumable with `opencode --session
  <id>` and revived by a [restore](#restoring-after-a-restart).

`agent-event opencode` is fail-safe in the same way as for Claude: it exits 0
outside a tutti pane, on an irrelevant event, and when the daemon is
unreachable or has not answered within 2 s. Remove `tutti.js` from the plugins
directory if you uninstall tutti.

## Kubernetes sandboxes

An agent can run in a Kubernetes sandbox instead of on your machine. You get
tutti's view of the agent unchanged: the pane, its state badge, the waiting
queue, and answering from the queue. The agent itself runs in a pod. tutti
drives [agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox)
(`agents.x-k8s.io/v1beta1`) through your own `kubectl`, so your kubeconfig,
contexts, and auth plugins (GKE's included) work as they already do.

Configure one or more `[[sandboxes]]` profiles (see
[Configuration](#configuration)). Then press `tab` in the run launcher to
target one, or from scripts run `tutti run --sandbox <profile> -- claude` (or
`pane run --sandbox`). The pane runs `tutti sandbox run <profile> --name
<name> -- claude`, which:

1. **creates** the sandbox, named by the daemon for its pane
   (`tutti-p<pane>-<hex>`, passed as `--name`). A `warm_pool` profile creates a
   `SandboxClaim` on that `SandboxWarmPool`, which is fast because a pod is
   already waiting. An `image` profile creates a `Sandbox` whose single
   container, `agent`, runs that image. Either is labelled
   `app.kubernetes.io/managed-by=tutti`.
2. **waits** until it is Ready, echoing the controller's status messages into
   the pane (ctrl-c cancels). A claim whose env its template refuses fails at
   once, saying so.
3. **execs** the agent with `kubectl exec -it` (forwarding `TERM` and
   `COLORTERM`). The pane's pty stays local, so the screen classifier, OSC
   9;4/7501 reports, bells, and guarded answers all work as they do for a local
   agent.
4. **deletes** the sandbox however the pane ends: the agent exits, the pane is
   killed, the daemon stops, or you press ctrl-c. A watchdog in its own
   process group waits on a pipe that only the pane process holds, and runs the
   delete once that pipe closes. As a backstop, the sandbox's `shutdownTime` is
   kept a full `ttl_minutes` ahead (renewed every third of it), with
   `shutdownPolicy: Delete`. If tutti dies hard, the cluster reaps the sandbox
   within the TTL.

**The sandbox owns the code.** tutti does not copy your checkout in or out. The
image or template clones the repo, and the agent pushes its work. The sandbox
is told where: `TUTTI_GIT_REMOTE`, the workspace's git remote (`origin`, else
the first `jj git remote`), and `TUTTI_BRANCH`, `tutti/<sandbox name>`, the
branch to push to. An `image` sandbox's container always gets both; its
entrypoint has to keep running for the exec, for example `git clone
"$TUTTI_GIT_REMOTE" /work && exec sleep infinity`. A warm-pool claim carries
them only with `inject_env = true`, and only if the pool's `SandboxTemplate`
sets `envVarsInjectionPolicy: Allowed`: the default rejects the claim, and
tutti fails the pane at once saying so. Injecting env costs the pool's head
start, since a running pod cannot take env and the claim gets a fresh one.
Without `inject_env`, the template has to know the repo itself.

**Pulling the work back.** `tutti sandbox pull <pane> [--name <workspace>]` —
or `p` on the exited sandbox pane, whose title reads `r resume · p pull · x
close` — fetches the sandbox's branch from the project's remote, tracks it, and
checks it out as a workspace nested under the project, named after the sandbox
unless you pass `--name`. Its diff, the changes panel, and merge then work as
for any workspace. It refuses a pane that is not a sandbox's, a repo with no
remote, and a branch the sandbox has not pushed. The TUI's `p` asks for no
name, so pulling the same sandbox twice takes the CLI's `--name`.

**Hooks from the pod.** `tutti agent-event` can't reach the daemon from a pod,
but the pane's pty can. `tutti hooks claude --tty` prints a Claude Code hooks
config whose commands write OSC 7501 straight to `/dev/tty` with plain
`printf`, so the image needs no tutti binary: a prompt or tool use reports
**working**, a permission or question Notification **blocked**, Stop **done**,
and SessionEnd clears. Each command is silent and never fails. To bake it in,
run `tutti hooks claude --tty --raw > hooks.json` and merge it into the agent
user's `~/.claude/settings.json` (top level) as the image is built, or commit
it as `.claude/settings.json` in the repo the sandbox clones. The pane's state
then comes from source `program`. Its Notification matcher names the waiting
types, so a type it does not know yet will not block, unlike through
`agent-event`.

The pane title and sidebar mark where the agent runs (`claude on gke`). Since
the local process tree only ever shows `tutti` and `kubectl`, the agent kind
comes from the command, not from detection. Known limits: without the `--tty`
hooks, state comes from the screen. Turns and checkpoints stay local and say
nothing about the sandbox's work, so sandbox panes record none; pull the work
back for its diff. Resume rows are hidden in a sandbox launcher because those
conversations live on this machine. An exited sandbox pane's `r` reopens the
launcher on the same sandbox.

## Orchestrating agents

Scripts and agents drive tutti through the same CLI, and two verbs make it an
orchestrator.

`tutti run -- claude "fix the tests"` spawns the command in a fresh tab and
prints only the new pane's id, so `p=$(tutti run -- claude "fix the tests")`
captures it. It runs in `--workspace <id>`, or in the workspace that has
`--dir <path>` open (mounting the directory when none does); without either,
from inside a tutti pane it lands in that pane's own workspace, beside its
caller, and elsewhere in the session's current workspace. An orchestrated
agent never splits a tab someone is using, and the session's current tab stays
where it is, so an attached TUI is not pulled over and your next launch still
lands where you are looking. `--sandbox` and `--title` work as for `pane run`
and `pane rename`. A spawn that fails leaves no empty tab behind.

`tutti pane wait <pane>...` blocks until each pane is `done` (finished, seen or
not), or reaches whichever `--until working|blocked|done|idle` states you name,
and prints each pane's state. The daemon answers from a subscription to state
transitions; nothing polls. A pane already in the state answers at once;
`--next` waits for a fresh transition instead, and a human looking at a
finished pane never counts as one. A pane whose process exits ends its wait: a
matching final state succeeds, and anything else fails (exit 1) naming the exit
code, as a pane killed mid-wait does. `--timeout <secs>` bounds the whole wait
across all panes and exits 124, as coreutils `timeout` does. Include
`blocked`: an agent stuck on a permission prompt never becomes done.

```sh
for dir in ../api ../web ../docs; do
  panes="$panes $(tutti run --dir "$dir" -- claude "fix the failing tests")"
done
tutti pane wait $panes --until done,blocked --timeout 3600   # one state per pane, in order
```

### Tailing a pane

`tutti pane read <pane> --since 0` prints everything the pane holds and, on
stderr, `next cursor: N`. Pass that `N` back to read only the rows that
scrolled off since, then the live screen — the screen comes back on every
read, because it is not final until it scrolls. The cursor counts screen rows,
not logical lines, so `--unwrapped` folds each read on its own (it joins
exactly the rows the terminal soft-wrapped; a full-width line stays its own).
A note on stderr says when some rows had already left the 10k-row scrollback.
While the pane's alternate screen is up (vim, less, a full-screen TUI), a read
returns the rows that scrolled off before it, then that screen; the shell's
screen under it comes back with the next read once the program leaves. A
resize rewraps the scrollback to the new width and can number the rows it
keeps afresh, so a cursor from before one may see some rows again — never
skip one. A cursor past the pane's count — one from another pane or an
earlier daemon — is refused. `--json` gives
`{"type":"tail","lines":[…],"cursor":N,"truncated":false}`.

### The tutti skill

tutti ships a Claude Code skill that teaches an agent running inside a tutti
pane to herd its own sub-agents: start them (`tutti run`), give each an
isolated jj workspace (`tutti workspace add`), wait on them (`tutti pane wait
--until done,blocked --timeout …`), read and tail their screens (`--since`),
safely answer their prompts (`--fingerprint` / `--expect`) and held approvals
(`pane approve`), review their diffs and turns, and merge or clean up. It also
hands tracker issues to agents (`tutti issue deploy`) and reads the
notification history (`tutti notes --unread`). Its rules include never typing
into its own pane, only killing what it created, and never marking or clearing
your notes.

```
tutti skill --install            # into ~/.claude/skills/tutti/SKILL.md
tutti skill --install --project  # …into ./.claude/skills/tutti/SKILL.md instead
tutti skill --install --yes      # skip the confirmation (scripts)
tutti skill                      # or just print it…
tutti skill --raw                # …without the stderr instructions, for piping
```

`--install` (or the setup panel, `Ctrl+B i`) is idempotent, backs a previous
tutti skill up to `SKILL.md.bak`, writes atomically, and refuses to overwrite a
`SKILL.md` that is not tutti's (one whose frontmatter does not say `name:
tutti`). The source is `skills/tutti/SKILL.md`; a test parses every command in
it against the CLI, so it cannot drift from the verbs it teaches.

## Watching state from other tools

`tutti watch [-s <session>]` streams the session's state to another program —
a menu-bar or notch app, a status line — as JSON lines on stdout, each flushed
as it lands. Every line is a full snapshot of the session (replace, don't
merge). The first comes at once, then one on each change to any pane's state,
agent, title, exit or user vars, to the workspace/tab/pane structure, or to the
attached TUI's focus or terminal; bursts coalesce.

```json
{"type":"snapshot","session":"tutti","wire_rev":10,"attached_terminal":"com.mitchellh.ghostty",
 "panes":[{"id":3,"workspace":"smartnotch","dir":"/Users/me/develop/smartnotch","tab":"main",
           "title":"claude","agent":"claude","state":"working","source":"hook","exited":null,"focused":true,
           "user_vars":{}}]}
```

- `id` is the pane id `tutti pane focus` takes; `workspace` and `dir` are its
  project's name and absolute directory; `tab` is its tab's name.
- `agent` is the detected agent (`claude`, `codex`, …), a self-reporting
  pane's OSC 7501 `app` label, or `null` for a plain shell.
- `state` is `blocked`, `working`, `done`, `idle` or `unknown`; `source` is
  `hook`, `program`, `progress`, `screen`, `activity` or `null` (see
  [Agent state sources](#agent-state-sources)).
- `exited` is the exit code, `null` while running; `focused` marks the attached
  TUI's focused pane.
- `user_vars` holds what the pane's programs set with OSC 1337 `SetUserVar`
  (`TUTTI_KEYS`, `TUTTI_EDITOR`, …), name → value, `{}` when none (see
  [Editors in panes](#editors-in-panes)).
- `attached_terminal` is the terminal app the attached TUI runs in (macOS's
  `__CFBundleIdentifier`), `null` with nobody attached or off macOS;
  `wire_rev` is the daemon's protocol revision.

A watcher is read-only and is not an attached client: it sits beside the TUI
without touching its sizes or focus, and watching a **done** pane never marks
it seen. `watch` never starts a daemon — with none running it says so on stderr
and exits 1 — and exits 0 when the daemon goes away. To jump from such a tool to
a pane, run `tutti -s <session> pane focus <id>`: the attached TUI switches to
that pane's workspace and tab.

## CLI surface

Every TUI action is also a CLI verb over the daemon socket — if it isn't
scriptable, it isn't done:

```
tutti server start [--foreground] | stop  # start (printing its log path) or stop the session's daemon; the CLI starts it on first use anyway
tutti server restore [--discard]     # bring back the layout the last daemon saved (agents resume their conversations, other panes return as shells)
tutti project add <dir>              # open an existing directory as a project
tutti project list                   # the projects (top-level entries)
tutti project kill <id>              # remove a project and its panes from tutti; the directory stays
tutti workspace add <project> --name <name> [-r <rev>] [--dest <path>]  # jj workspace add: a checkout nested under the project (default <repo>-<name> beside it)
tutti workspace list                 # every project and workspace, with the PARENT each is nested under
tutti workspace kill <id> [--discard]  # remove from tutti; --discard also jj workspace forgets + deletes a checkout tutti created
tutti workspace merge <id> [--push]  # merge a workspace back into its project's trunk (main/master)
tutti workspace update <id>          # jj workspace update-stale (clears the sidebar's stale tag)
tutti workspace diff <id> [--stat]   # the workspace's jj diff against trunk (jj is required; --json for raw lines)
tutti tab new [--workspace <id>] | list [--workspace <id>] | select <id>
tutti run [--workspace <id> | --dir <path>] [--title <t>] [--sandbox <profile>] -- <cmd...>  # a fresh tab; prints the bare pane id
tutti pane run [--tab <id>] [--ephemeral] [--sandbox <profile>] -- <cmd...>
tutti pane split <pane> right|down
tutti pane list | kill <pane> | focus <pane>   # focus: the attached TUI jumps to the pane
tutti pane focus --direction left|right|up|down [--from <pane>] [--wrap]  # focus the pane beside one (default: this pane, else the TUI's) and print it; at the TUI's focused pane's left/right edge the TUI moves on (sidebar, panel, tab), exit 0; else `no pane <dir> of <id>`, exit 1
tutti pane resize --direction left|right|up|down [--amount <f>] [--from <pane>]  # move its split's border that way, as Alt+h/j/k/l (default step 0.05 of the split)
tutti pane attach <pane>             # show one pane in this terminal, drawn by the terminal itself; prefix d detaches (never starts a daemon)
tutti pane rename <pane> <title>
tutti pane send <pane> --text <s> | --keys <s> [--expect <fp>]   # --expect: answer a blocked agent, refused if its prompt moved on
tutti pane read <pane> [--lines N] [--unwrapped] [--since <cursor> | --last-output] | --fingerprint
                                     # --since: rows scrolled off since the cursor, then the screen; next cursor on stderr
                                     # --last-output: the last finished command's output; `exit: N` on stderr (needs shell-init)
                                     # --fingerprint: the prompt fingerprint --expect takes
tutti pane wait <pane>... [--until working|blocked|done|idle[,…]] [--next] [--timeout <secs>]  # block until each pane gets there; exit 124 on timeout
tutti pane approve <pane> [--deny] [--message <s>]  # answer the pane's held approval (approvals = "hold")
tutti pane checkpoints <pane>        # the pane's per-turn checkpoints, newest first (n, age, stat, commit, label — the turn's prompt when known)
tutti pane checkpoint-diff <pane> [N] [--stat]  # one turn's diff (default: the newest)
tutti pane restore <pane> <N> [--force]         # restore the working copy to turn N (--force mid-turn)
tutti sandbox run <profile> [--name <name>] -- <cmd...>  # provision, kubectl exec, delete — what a --sandbox pane runs (the daemon names it)
tutti sandbox pull <pane> [--name <name>]  # fetch a sandbox's pushed tutti/<name> branch into a workspace under its project
tutti issue list [--source <name>]   # every configured tracker's open issues (--json for scripts)
tutti issue deploy <source> <id> [--agent claude] [--project <id>] [--here] [--sandbox <profile>]  # an agent on the issue, in its own workspace; prints the pane id
tutti notes [--unread]               # the notification history, newest first
tutti notes read [--up-to <id>] [--pane <id>] | clear  # mark notes read (all, or narrowed) / empty the history
tutti hooks claude|opencode [--raw]  # print the Claude Code hooks snippet / the opencode plugin
tutti hooks claude|opencode --install [--project] [--yes]  # install them (shows the change, asks first)
tutti hooks claude --tty [--raw]     # print Claude hooks that report over OSC 7501 to /dev/tty — for sandbox images
tutti skill [--raw]                  # print the tutti skill (a Claude Code SKILL.md)
tutti skill --install [--project] [--yes]  # install it into ~/.claude/skills/tutti/ (or ./.claude/skills/tutti/)
tutti agent-event claude|opencode    # forward an agent hook event (JSON on stdin) — used by the hooks/plugin
tutti edit <path>[:<line>[:<col>]] [--workspace <id>]  # open in the workspace's editor pane (TUTTI_EDITOR) and focus it; returns at once
tutti shell-init zsh|bash|fish       # print the shell integration (OSC 7 cwd + OSC 133 marks) for your rc file
tutti watch                          # JSON-line snapshots of every pane's state, one per change (read-only; never starts a daemon)
tutti [attach]                       # bare `tutti` attaches; a fresh session asks where to start
```

`--json` on any verb emits the raw protocol response for scripting. A
project's id works anywhere a workspace id does (`run --workspace`, `tab new
--workspace`, `workspace diff`). Scripts written against the old spellings
`workspace new --dir <path>` and `workspace fork <id> --name <name>` keep
working: both are hidden aliases of `project add` and `workspace add`.

Every verb takes `-s <session>`. Inside a tutti pane it defaults to that pane's
own session (`TUTTI_SESSION`, which every pane exports beside its `TUTTI_PANE`
id), else to `tutti`, so an agent's commands reach the daemon running it rather
than auto-starting a stray one. For the same reason, bare `tutti` (or `tutti
attach`) refuses to attach a session from inside one of its own panes, which
would nest the session inside itself; pass `-s <other-session>` to attach a
different one.

## Workspace layout

```
crates/tutti-core     pure domain model, agent state machine, protocol + frame codec
crates/tutti-server   daemon: PTYs and their terminals (rtty-vt), socket API, detection & classification
crates/tutti          client: CLI verbs + ratatui TUI
crates/tutti-agents   agent registry & screen-state classifier (data-driven)
```

## Known limitations

tutti is alpha; PLAN.md tracks what is open.

- **From a checkout only.** Install is `cargo install --path` from this
  repository, on macOS or Linux; there is no crates.io release, Homebrew tap,
  or installer.
- **Exact state needs an integration.** Claude Code and opencode report through
  their hooks, and agents that speak herdr's protocol (Crush) report on their
  own; Codex, Gemini and the rest of the catalog are read from the screen,
  tuned live only for some.
- **Approvals are Claude Code's only**, one held per pane at a time.
- **Trackers are read-only.** A deploy never comments on, assigns, or moves
  its issue.
- **The notification history starts empty after a restart**: it lives in the
  daemon's memory, unlike the layout.
- **A slow request whose handler panics never answers.** The daemon logs the
  panic and carries on, but the CLI waiting on that merge, diff or wait waits
  until you interrupt it.
- **Light/dark switches reach panes only through `pane attach`.** The TUI
  reports your terminal's colours when it attaches; reattach after switching
  the OS theme.
- **The TUI hears no light/dark switch while attached** (above), and a pane's
  program that asked for key releases or the text of a key (kitty's flags 2
  and 16) gets presses only: the TUI asks your terminal for neither.
