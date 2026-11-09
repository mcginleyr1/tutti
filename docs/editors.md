# Editors in panes

How Neovim or Emacs running inside a pane works with tutti: the user vars an
editor sets to announce itself, the plugins in `editors/` that set them,
sharing `Ctrl+h/j/k/l` between the editor's windows and tutti's panes, and
opening files in that editor with `tutti edit`.

A Neovim or Emacs in a pane tells tutti it is there through iTerm2's user vars,
`ESC ] 1337 ; SetUserVar=<NAME>=<base64 value> BEL`, which any program — a
shell's `printf` included — can write. Names are `[A-Za-z0-9_]`, up to 64
bytes; values are up to 1 KiB of UTF-8; a pane keeps 16, and an empty value
unsets one. They live as long as the daemon (a restored pane starts with none),
and `tutti --json pane list` and `tutti watch` show them as `user_vars`. tutti
reads two:

- `TUTTI_KEYS=pane`: the direct keys go to the pane (see [Direct keys](keys.md#direct-keys)).
- `TUTTI_EDITOR`: `nvim <server address>` (its `v:servername`), or `emacs
  [<server name or socket path>]` — best the absolute socket path, which an
  emacsclient started from any environment finds. Files open in this pane.

`editors/` holds the plugins that speak it; [editors/README.md](../editors/README.md)
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
editor that crashed cannot: with the [shell integration](shell-integration.md)
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
nothing and exits 0, so an editor's edge leads where tutti's own keys do. A
TUI running in rtty takes the top and bottom edges on the same way, out to
rtty's panes ([tutti in rtty](rtty.md)). At any other edge it prints
`no pane <direction> of <id>` on stderr and exits 1, unless `--wrap` goes
round to the farthest pane on the other side of the tab, level with it. A
resize moves the nearest split border that way by `--amount` of the split
(0.05, the `Alt+h/j/k/l` step); with no split of tutti's on that axis, a TUI
in rtty moves rtty's instead. See [Known limitations](limitations.md) for
what an editor in a pane does not get yet.
