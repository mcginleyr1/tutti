# Keys and mouse

The keys and mouse actions of the TUI, the full-screen client that `tutti`
opens: the direct keys that work without a prefix, the keys behind the prefix
(`Ctrl+B` by default), how to look a key up while attached, and what the mouse
does. Rebinding and the keymap presets are in
[configuration.md](configuration.md). Each panel has keys of its own, listed
with the panel: the table at the end says where.

## Direct keys

Direct keys work without the prefix (smart-splits / zellij-nav muscle memory):

| Key | Action |
| --- | --- |
| `Ctrl+h/j/k/l` | focus the pane to the left/down/up/right |
| `Ctrl+h` at the left edge | step into the sidebar (when visible), then the previous tab |
| `Ctrl+l` at the right edge | step into the changes panel (when visible), then the next tab |
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
its edge (see [Editors in panes](editors.md)). The prefix is never handed
over, and a pane whose program exited claims nothing.

Run the TUI in an [rtty](https://github.com/mcginleyr1/rtty) pane and a move
past tutti's own edge goes on to rtty's panes: see [tutti in rtty](rtty.md).

## Prefix keys

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
| `i` | setup: install the Claude Code, Codex and Gemini CLI hooks, the opencode plugin, the tutti skill, and the shell integration — for you, or (`s`) into the active project |
| `o`, arrows | focus next pane / directional focus |
| `z` | zoom focused pane |
| `x` | kill pane (confirms) |
| `[` | scrollback for focused pane (`q`/esc exits) |
| `m` | mouse on/off (off hands the mouse back to the terminal for select/copy) |
| `d` or `q` | detach |
| `?` | help |

The other preset, `vim`, and how to change the prefix are in
[configuration.md](configuration.md#presets).

## Finding a key: the hint, which-key and help

You never have to memorise these. The bottom bar always shows a compact hint on
its right edge (`C-b ? help · C-b q detach`, the key bright and its label dim),
and if you press the prefix and pause, a **which-key** popup lists every
follow-up (press the key, or `esc` to back out). `Ctrl+B ?` opens the full help
overlay — detach first, then the rest of the active keymap, the direct keys, the
sidebar's and the changes panel's own keys, and how to stop the daemon. The
prefix and panel sections render from the live keymap tables, so they always
match what the keys do.

## Stepping into the sidebar

Sidebar edge-nav: at a pane's **left edge**, `Ctrl+h` steps into the sidebar when
it is visible (nvim-explorer muscle memory) instead of jumping straight to the
previous tab; from the sidebar, `Ctrl+l` returns to the pane you left and `Ctrl+h`
goes on to the previous tab. With the sidebar hidden the left edge wraps to the
previous tab as before. In rtty, rtty's pane to the left comes before that tab
([tutti in rtty](rtty.md)).

## Mouse

Mouse: click focuses a pane, click a tab segment or sidebar entry to jump.

- **Resizing a split.** Drag the border between two panes to resize their
  split: grab the frame column
  on either side where side-by-side panes meet, or the upper pane's bottom edge
  or the lower pane's title row where stacked panes meet. The border follows the
  pointer cell by cell, held to the same 10–90% bounds as the keyboard resize; a
  click there that never moves it still just focuses the pane, and a zoomed pane
  has no border to drag.
- **The wheel** routes by what the pane's program declared: a
  program that asked for mouse events (vim with `mouse=a`, htop) receives the
  wheel itself; a full-screen
  program without mouse reporting (Claude Code, less) gets arrow keys — the xterm
  "alternate scroll" convention — so it scrolls its own view; only a plain
  primary-screen pane (a shell) enters tutti's frozen scrollback browse
  (`q`/esc exits).
- **Clicks and drags inside a pane** whose program asked for them
  (Neovim's `mouse` option, Emacs's `xterm-mouse-mode`, htop) go to that program
  as they would in a terminal of its own — presses, releases and drags, and bare
  motion when it asked for all of it, in its own encoding — so a click places
  Neovim's cursor and a drag moves its split borders; a click on another pane
  focuses it and reaches its program too. Hold `Shift` to select and copy
  there instead (tutti's selection, or your terminal's own where the terminal
  keeps `Shift` for itself).
- **Selecting and copying.** Elsewhere, drag inside a pane to select its text;
  releasing copies the
  selection to your system clipboard (an OSC 52 write your terminal fulfils) and
  flashes `copied N chars`, so selecting works even while tutti holds the mouse for
  scroll and focus — the selection also reaches the pane's scrollback while you
  browse it. Copy needs a terminal that honours OSC 52 (iTerm2, kitty, WezTerm,
  Ghostty, tmux with `set-clipboard on`); where it does not (Apple Terminal), press
  the prefix then `m` to hand the mouse back for the terminal's own drag-to-select,
  and `m` again to re-grab it (most terminals also bypass capture while
  `Shift`/`Option` is held).
- **Off at startup.** `mouse = false` starts with the mouse off.

## Keys of the panels

| Panel | Opens with | Its keys are in |
| --- | --- | --- |
| sidebar | prefix `w`, or `Ctrl+h` at the left edge | [tui.md](tui.md#sidebar-keys) |
| changed-files panel and its turns | prefix `f`, or `Ctrl+l` at the right edge | [workspaces.md](workspaces.md#changed-files-panel) |
| run launcher | prefix `r` | [tui.md](tui.md#run-launcher) |
| go to | prefix `g` | [tui.md](tui.md#go-to) |
| setup panel | prefix `i` | [tui.md](tui.md#setup-panel) |
| notifications panel | prefix `N` | [notifications.md](notifications.md#history) |
| issues panel | prefix `I` | [issues.md](issues.md) |
| peek, reply and approval overlays | `p` on a sidebar row | [answering.md](answering.md) |
