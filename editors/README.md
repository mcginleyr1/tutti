# Editor plugins

An editor running in a tutti pane can tell tutti it is there. tutti then hands
it the direct keys (`Ctrl+h/j/k/l`, `Alt+h/j/k/l`, `Alt+x`) instead of
intercepting them, and `tutti edit <path>[:line[:col]]` opens files in it. The
editor moves between its own windows and, past its last one, on to tutti's
panes. Each plugin also lists the session's agents.

Both plugins do nothing outside a tutti pane (no `$TUTTI_PANE`).

- [Neovim](#neovim): `editors/nvim`, Neovim 0.10 or later (verified on 0.12)
- [Emacs](#emacs): `editors/emacs/tutti.el`, Emacs 29.1 or later, built-ins only

## The user-var contract

A program in a pane announces itself with an OSC 1337 user var, as iTerm2 and
WezTerm define it: `ESC ] 1337 ; SetUserVar=<NAME>=<base64 value> BEL`. An
empty value clears the var. Anything can speak it, a shell included:

```sh
printf '\033]1337;SetUserVar=TUTTI_KEYS=%s\007' "$(printf pane | base64)"
```

| Var | Value | Effect |
| --- | --- | --- |
| `TUTTI_KEYS` | `pane` | tutti stops intercepting its direct keys and passes them to the pane |
| `TUTTI_EDITOR` | `nvim <servername>`, `emacs`, or `emacs <server socket path>` | `tutti edit` opens files in this editor |

Both plugins set their vars when the editor starts or resumes, and clear them
when it suspends (`Ctrl-Z`) or exits, so the shell underneath gets tutti's keys
back. An editor that crashes cannot; with tutti's shell integration
(`tutti shell-init`) the shell's next prompt clears them instead.

Past the editor's edge, the plugins call the CLI, with `--from` set to the
editor's own `$TUTTI_PANE`:

- `tutti pane focus --direction <left|right|up|down> --from <pane> [--wrap]`
  exits 0 and prints the new pane's id when focus moved. At an edge it exits
  1 with `no pane <direction> of <pane>`. `--wrap` takes the farthest pane on
  the other side instead. Focus stays within the pane's tab.
- `tutti pane resize --direction <left|right|up|down> [--amount <ratio>]
  --from <pane>` moves the nearest split border that way, exactly as
  `Alt+h/j/k/l` do. The amount is a fraction of the split, 0.05 by default.

## Neovim

With [lazy.nvim](https://github.com/folke/lazy.nvim):

```lua
{
  dir = '~/src/tutti/editors/nvim',
  name = 'tutti',
  main = 'tutti',
  opts = { keys = true },
}
```

Any plugin manager works. Put `editors/nvim` on the runtimepath and call
`require('tutti').setup({ keys = true })`.

In a tutti pane, the plugin always announces `TUTTI_EDITOR=nvim
<v:servername>`. The keys are opt-in:

- `keys = true` maps, in normal mode, `Ctrl+h/j/k/l` to
  `require('tutti').navigate(direction)` and `Alt+h/j/k/l` to
  `require('tutti').resize(direction)`, and only then announces
  `TUTTI_KEYS=pane`. `navigate` moves to the Neovim window that way, and from
  the last window on to the tutti pane beyond. `resize` resizes tutti's split,
  as `Alt+h/j/k/l` does outside Neovim.
- Without it, tutti keeps the keys. `Ctrl+h` then still leaves Neovim the way
  it leaves any program, and never gets stuck in a pane with nothing mapped.
- To map the keys yourself, set `require('tutti').keys = true` before
  `VimEnter` and call `navigate`/`resize` from your mappings.
- `Alt+x` reaches Neovim unmapped while Neovim holds the keys. Kill the pane
  with the prefix and `x`.

`:TuttiAgents` opens a `vim.ui.select` picker over the session's agents from
`tutti watch`'s first snapshot. Each row shows the agent's state, title and
workspace. Picking one runs `tutti pane focus <id>`, so the attached TUI jumps
to it.

The Lua API is `require('tutti')`:

- `navigate(direction)`: move to the window or pane in that direction.
- `focus(direction, wrap)`: focus the tutti pane in that direction. Returns
  true when focus moved.
- `resize(direction, amount)`: resize tutti's split.
- `agents()`: open the `:TuttiAgents` picker.

`focus`, `resize` and `navigate` give tutti 500 ms
(`require('tutti').timeout`), and tutti never makes them throw. A tutti that is
missing, slow or refusing reads as "did not move". Outside a tutti pane they
never call tutti at all: `focus` and `resize` return false, and `navigate`
stays among Neovim's windows. An unknown direction is an error.

### smart-splits.nvim

`tutti.smart_splits` is a backend for
[smart-splits.nvim](https://github.com/mrjones2014/smart-splits.nvim) protocol
3.0.0. smart-splits moves and resizes Neovim's windows, and at Neovim's edge
hands the move to tutti:

```lua
{
  'mrjones2014/smart-splits.nvim',
  dependencies = { { dir = '~/src/tutti/editors/nvim', name = 'tutti' } },
  opts = {
    mux = { backend = 'tutti.smart_splits' },
    -- or, outside tutti, fall back to another multiplexer:
    -- mux = { backend = { 'tutti.smart_splits', 'smart-splits-backend-zellij-lua' } },
  },
}
```

Map the keys through smart-splits as usual, and leave `keys` off in tutti's
own setup. The backend works like this:

- `detect()` is true inside a tutti pane with `tutti` on the PATH.
- `activate()` announces `TUTTI_KEYS=pane`, because smart-splits holds the
  keys.
- `move` focuses the tutti pane beyond the edge, wrapping around tutti's edges
  only when smart-splits' `at_edge` is `'wrap'`.
- `resize` moves tutti's split border by tutti's own `Alt+h/j/k/l` step (5%
  of the split), or by `count` steps with a count. smart-splits counts its
  `amount` in cells and tutti in split ratio, and Neovim can't see the split's
  size to convert between them.

## Emacs

With `use-package`:

```elisp
(use-package tutti
  :load-path "~/src/tutti/editors/emacs"
  :demand t
  :config
  (server-start)   ; optional: lets `tutti edit' reach this Emacs through emacsclient
  (tutti-mode 1)
  :bind (("S-<left>" . tutti-windmove-left)
         ("S-<right>" . tutti-windmove-right)
         ("S-<up>" . tutti-windmove-up)
         ("S-<down>" . tutti-windmove-down)))
```

`tutti-mode` is a global minor mode. In a terminal frame inside a tutti pane,
it announces `TUTTI_KEYS=pane` and `TUTTI_EDITOR=emacs`. When this Emacs runs
a server, the var is `emacs <socket>` instead, with the socket's absolute path
(`server-name` under `server-socket-dir`). emacsclient then finds the socket
whatever environment `tutti edit` runs in. Turn the mode on after
`server-start`. While the mode is on, `C-h`, `C-j`, `C-k`, `C-l`, `M-h`, `M-j`,
`M-k`, `M-l` and `M-x` are Emacs's own keys again. The mode clears the vars on
`suspend-hook` and `kill-emacs-hook`, and announces again on
`suspend-resume-hook`.

Emacs asks the terminal for its colours at startup, and tutti answers with
those of the terminal you attached from, so `background-mode` follows it,
light or dark.

`tutti-windmove-left`, `-right`, `-up` and `-down` move like `windmove`. Where
windmove would refuse, at the frame's edge or toward an inactive minibuffer,
they focus the tutti pane in that direction instead. Outside a tutti pane they
are plain windmove, refusal included, so the same bindings work everywhere. Nothing is bound by
default. The shifted arrows above follow `windmove-default-keybindings`; pick
`C-c <arrow>` instead if Org mode already claims them.

`M-x tutti-agents` lists the session's agents in a table of state, title and
workspace. Blocked agents are red, working agents yellow and done agents
green. A `tutti watch` process keeps the table live, and each snapshot it
prints replaces the table. Plain shells are left out.

| Key | Action |
| --- | --- |
| `RET` | `tutti pane focus <id>`: the attached TUI jumps to the agent |
| `p` | answer a blocked agent with tutti's guarded send (below) |
| `y` | allow the agent's held approval (`tutti pane approve <id>`), after showing the tool and its input |
| `n` | deny it (`--deny`), reading a reason for the agent; leave it empty for tutti's default |
| `g` | restart the watch |
| `q` | bury the list; killing the buffer stops the watch |

`p` takes the prompt's fingerprint first (`pane read --fingerprint`). It then
shows the 12 rows that fingerprint covers (`pane read --lines 12`) in a side
window and reads the answer. The answer goes out with `pane send --text
<answer> --expect <fingerprint>`, exactly as typed (`C-q RET` adds an Enter).
If the prompt changed in between, tutti refuses the send, writes nothing, and
the reason appears in the echo area. `y` and `n` read the held approval from
`tutti --json pane list`, and refuse when the pane holds none.

`tutti-program` (default `"tutti"`) names the executable.

## Tests

Both suites run against `editors/tests/tutti`, a stub that logs each call's
argv and answers from control files. They assert the exact commands sent and
the plugins' return values:

```sh
nvim --headless -u NONE -l "$PWD/editors/nvim/tests/run.lua"
emacs --batch -l ert -l "$PWD/editors/emacs/tutti.el" -l "$PWD/editors/emacs/tutti-test.el" \
  -f ert-run-tests-batch-and-exit
```

`editors/nvim/tests/osc.py` runs a real Neovim TUI in a pty under a
job-control shell. It checks the exact OSC bytes that reach the terminal on
start, `Ctrl-Z`, `fg` and quit:

```sh
python3 -I editors/nvim/tests/osc.py "$(command -v nvim)" "$PWD/editors/nvim"
```
