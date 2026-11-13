# Known limitations

tutti is alpha. This page lists what it does not do yet, or does only in
part; [PLAN.md](../PLAN.md) tracks what is open.

- **From a checkout only.** Install is `cargo install --path` from this
  repository, on macOS or Linux; there is no crates.io release, Homebrew tap,
  or installer.
- **Exact state needs an integration.** Claude Code, Codex, Gemini CLI,
  Antigravity CLI and opencode report through their hooks, and agents that
  speak herdr's protocol (Crush) report on their own; the rest of the catalog
  is read from the screen, tuned live only for some. Antigravity CLI's hooks
  report no permission dialog, so that part of its state is read from its
  screen even with them installed.
- **Replies come from the same five hooks.** `pane read --last-reply`, `tutti
  ask` and the sidebar's reply panel have nothing to show for any other
  agent, and `ask` refuses its pane; a fresh Codex, opencode or Antigravity
  CLI it takes on the strength of tutti's installed integration, so a Codex
  whose hooks are installed but not yet trusted in `/hooks` runs an ask to
  its `--timeout`. Gemini CLI's reply is wired from its docs and source and
  not yet run live; it is the whole turn's text, not only the last message.
  A turn interrupted with Esc in Claude Code or Antigravity CLI sends no
  Stop: it leaves the previous turn's reply in place, and an `ask` waiting on
  it runs to its `--timeout`. A daemon restart forgets every reply.
- **Held approvals are Claude Code's, Codex's and opencode's only.** opencode
  shows its own dialog beside a held request (whichever answers first wins),
  and so does Claude Code; Codex keeps its dialog back while the hook runs
  (see [Held approvals](answering.md#held-approvals)).
- **Write-back stops at the deploy.** A source can have a deploy comment on,
  assign, label or move its issue, but tutti never closes it, and a command
  source (any other tracker) is read-only.
- **Key releases, repeats and text need a kitty terminal.** Under a terminal
  without kitty's protocol, a pane's program that asked for them (flags 2
  and 16) gets presses only, and its text derived from the key.
- **The TUI follows light/dark only on terminals that report it** (mode
  2031). On one that doesn't, reattach after switching the OS theme.
- **Frames that cost no cells need a terminal that draws them.** The
  [cell-border protocol](https://www.joshka.net/cell-borders/) is a draft
  only a Ghostty branch implements so far; everywhere else panes keep their
  frames of box characters. tutti's own wire is unverified against a real
  terminal: it has been drawn only by a stand-in that models the protocol's
  rectangle commands.
- **A program in a pane cannot draw cell borders in the TUI.** tutti's daemon
  does not speak the protocol to its panes: their query goes unanswered, as
  the protocol asks of a multiplexer, so they draw box characters. `pane
  attach` is the exception, and a wrong one: it hands the query on to your
  terminal, whose answer the program then acts on though the daemon keeps
  none of the lines it draws.
- **In rtty, an editor's move off tutti's edge always reads as made.** The
  TUI takes it on and `tutti pane focus` exits 0, even where rtty's own edge
  then stops it. An editor that does something of its own when tutti
  reports no move does not do it there.
- **Across `--host`, ages use this machine's clock** against the daemon's
  timestamps, so a skewed clock skews the "2h ago" labels.
