# Panes as terminals

Each pane is a terminal that the daemon runs on tutti's own terminal engine,
rtty-vt. This page covers what the TUI draws of a pane, `tutti pane attach`
(one pane shown in a real terminal, on its alternate screen or inline), and how
the daemon answers the terminal queries, keyboard protocols and colour
questions that the programs in a pane ask.

## What the TUI shows of a pane

The TUI runs each pane on the daemon's own terminal engine, so it draws what
the pane's terminal holds: every rendition, underline styles and colours
(Neovim's red undercurl for spelling), overline, OSC 8 hyperlinks and whole
grapheme clusters. Your terminal's cursor takes the focused pane's shape — a
bar in Neovim's insert mode — and a program that asks for focus events hears
when its pane gains or loses the focus, your terminal's included. A program
drawing inside a synchronized update (mode 2026) is shown once the update
ends, never half drawn, and each of the TUI's own frames reaches your terminal
as one such update too.

## One pane in a real terminal

`tutti pane attach <pane>` shows one pane in the terminal you run it in, drawn
by that terminal itself rather than the TUI: a snapshot first, then the pane's
output streamed raw, and your keys typed into the pane. It is the view for
editor terminals — Ghostel in Emacs, Neovim's `:terminal` — and for a pane you
want at full fidelity.

- **Detaching.** The prefix then `d` detaches; the prefix twice sends one
  prefix through; the prefix then any other key sends both.
- **Esc.** An `Esc` waits the
  escape time (`escape_time_ms`, see [Configuration](configuration.md#the-escape-time)) for the
  key after it: one that comes in time goes to the pane with it, in one write,
  as that key with Alt — and counts as an `A-…` prefix.
- **The alternate screen.** The pane is drawn on
  the terminal's alternate screen, and your shell's screen comes back on exit,
  the terminal's modes restored. A program in the pane that uses an alternate
  screen of its own (vim, less) is redrawn from a local copy of the pane on each
  switch, so leaving it brings back the pane's shell.
- **Size.** The stream votes in the
  pane's size like an attached client and follows the window's size.
- **Falling behind.** A terminal
  that cannot keep up with the pane's output — a megabyte of it queued — skips
  ahead: the daemon drops what is queued for it and sends the pane's screen
  afresh, and the stream goes on from there.
- **Ending.** It ends with `[pane N exited:
  CODE]` (exit 0) when the pane's program exits, and with an error (exit 1) when
  the daemon goes away. It never starts a daemon, and refuses the pane it is
  run from.

What the alternate screen costs: the pane has no native scrollback in that
terminal. Nothing else is lost to the redraws: the daemon hands a stream each
switch of screens on its own, so the bell or cursor shape beside one still
reaches your terminal, and it answers the queries a program sends as it
starts itself.

### Inline: on the primary screen

`tutti pane attach --inline <pane>` draws on the primary screen instead, for
an editor's terminal (Neovim's `:terminal`, Emacs's vterm or eat), which then
keeps its native scrollback:
- **On attach,** the pane's history from before the attach is printed first,
  under your shell's lines: up to `--history ROWS` (default 1000; 0 for
  none) of its newest scrollback lines, in their colours and links, each
  soft-wrapped line whole for your terminal to wrap at its own width. Then
  the lines on your screen, down to the cursor's, go up into the scrollback,
  and the pane's screen is drawn in their place. The history is the pane's
  primary screen's, even while a program has its alternate screen up: it
  lands above the pane's shell, where that program's exit leaves you. A
  resize or a reset never prints it again.
- **A second attach in the same terminal** prints only what that terminal
  lacks. On detach, `pane attach` keeps what your terminal now holds of the
  pane — its rows up to the screen, and the screen's down to the last drawn —
  in `$XDG_STATE_HOME/tutti/inline.json` (yours alone, 0600; the newest 64
  terminal-and-pane records; updated under a lock on `inline.json.lock`, so
  terminals leaving at once each keep theirs, and one that fails to parse is
  set aside as `inline.json.bad`, which the detach names), keyed by the
  terminal: its tty, and the session that owns it, so a terminal opened later
  on the same tty is another one. The next attach there starts the history
  at the first of those screen rows the pane has changed since (the prompt
  you typed on), or past them; the screen it draws is the pane's as it is
  now, under the copy your terminal kept. `--history` still caps it, and a
  line `[pane N: M lines not shown]` stands where the cap, or a scrollback the
  pane itself cleared, left rows out. A daemon restarted since numbers its
  rows afresh, and the record counts for nothing; one from before inline
  views (wire rev 16) prints the history whole, which the attach warns of.
  The record is what your terminal was handed, not what it still has: rows
  its own scrollback trimmed to its limit, or a clear of it (Cmd+K) dropped,
  are not printed again, and no line says so.
- **While attached,** what the pane scrolls off goes into your terminal's
  scrollback, and a program's own alternate screen is your terminal's: its
  switches pass straight through. A terminal that cannot keep up skips ahead
  as above, and is first handed the lines that scrolled off meanwhile, which
  go into its scrollback over the stale screen: it misses none the pane still
  holds — the newest 10 000, its whole scrollback. Past those, a line
  `[pane N: M lines not shown]` stands where the rows the pane no longer held
  were.
- **On a resize,** the pane's screen is drawn again as the daemon reflowed it;
  what your terminal's own reflow moved into its scrollback stays there. Rows
  the daemon's reflow pushed off the pane's screen are not handed on: when
  your terminal shrank too, its own reflow kept its copy of them, but a pane
  made smaller by another client's vote loses them from your scrollback, and
  no line says so. The screen is erased for it with `ED 0`, never `ED 2`,
  which terminals that scroll a cleared screen into their scrollback (kitty,
  alacritty, tmux's scroll-on-clear) would keep a copy of each time.
- **A reset** (`ESC c`) is drawn from the copy rather than passed through: it
  would turn off the light/dark reports, and some terminals clear their
  scrollback on it.
- **On detach,** a program's alternate screen is left, and the cursor goes to
  the line under the pane's last line of text, so the shell goes on below it.

## Terminal queries and colours

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

### Keyboard protocols

A program's keyboard protocol is the pane's own: the daemon answers kitty's
keyboard query and keeps the flags a program pushes, and the level of xterm's
modifyOtherKeys it asks for (Neovim pushes kitty's flags; Emacs asks for
modifyOtherKeys). The TUI encodes each key for the focused pane as its program
asked, with rtty-vt's key and mouse encoders (`rtty_vt::input`, the ones rtty
uses) — under kitty's flags Ctrl+I is not Tab, Esc not the start of Alt — and
asks your terminal for kitty's disambiguated keys when it speaks the protocol,
so it can tell them apart in the first place; it hands the terminal back as it
was on detach. Under a terminal without it, keys still reach a kitty pane in
its encoding, but only those your terminal can tell apart. `pane attach` keeps
plain keys: the protocols never reach your terminal there, as it watches for
the prefix in plain bytes and a detach could not be sure to turn them off.

Beyond disambiguated keys, the TUI asks your kitty terminal for what the
focused pane's program pushed, and passes it on:
- **Repeats and releases** (flag 2; Neovim 0.12 asks for them). A key held
  down stays with the pane it was pressed into: its repeats and release go
  there even after the focus moved, and your terminal keeps reporting
  releases until it is let go. A release never reaches a pane whose press was
  tutti's: the prefix, a direct key.
- **Every key as an escape code** (flag 8): text keys, Enter, Tab and
  Backspace, a lone modifier, the lock keys' bits. tutti asks for the text
  too while this is on, so a prompt still types what a dead key or an input
  method made.
- **The text a key typed** (flag 16), as your terminal reported it.

A program that asked for less gets less, down to plain keys. tutti's own keys
work as before whatever is on: a lone modifier is never a key of tutti's, and
a release never ends the prefix.

### Colours and light/dark

The TUI reports your terminal's colours when it attaches. On the `dark` and
`light` themes it reports the theme's own pane ground and text instead, since
panes are painted over them. The TUI and `tutti pane attach` both follow your
terminal's light/dark switches live:
- **Asking for them:** each asks the terminal for its switch reports (mode
  2031).
- **On a switch:** it asks the terminal's colours again and hands them to the
  daemon. The TUI's own chrome takes the new light or dark at once.
- **Telling the panes:** the daemon owns mode 2031 for each pane, and tells a
  program that set it (Neovim does) of every change, as a terminal would.

### Raw streams, and what the TUI reads

A raw stream carries every query the daemon did not answer, so your terminal
answers those and never answers one twice. The full table is in
[transport-decision.md](transport-decision.md).

The TUI reads its terminal itself, so it decodes your keys, mouse, focus and
those reports on its own. It also turns on bracketed paste: a paste reaches
the focused pane whole, inside paste marks when the pane's program asked for
them. A shell then runs none of a multi-line paste's lines until you press
Enter.
