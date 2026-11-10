# Install and upgrade

What tutti needs on your machine, how to build and install it from a checkout,
the optional integrations worth turning on afterwards, and how to upgrade a
running install.

## What you need

- **A stable [Rust toolchain](https://rustup.rs)** — the workspace builds on
  edition 2024, so rustc **1.85 or newer**. macOS and Linux.
- **[jj](https://jj-vcs.github.io)** — optional. Core multiplexing works
  anywhere; the workspace-level VCS features (per-workspace diffs, nested
  [workspaces](workspaces.md#workspaces) on isolated checkouts, merge-back, branch display)
  are prescriptive: they require jj — there are no git/mercurial adapters.
- **kubectl** — optional, for launching agents into
  [Kubernetes sandboxes](sandboxes.md) (a cluster running
  [agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox)).
- **gh** or **curl** — optional, for [deploying issues](issues.md) from
  GitHub (through your `gh` login) or Linear (its API through `curl`).
- **The agents themselves** on your `PATH`. Tutti detects and drives them; it
  does not install them. The recognized catalog, with each agent's link and how
  far its state detection is tuned, is in [agents.md](agents.md#the-catalog).

## Build and install

From a checkout of this repository:

```sh
cargo install --path crates/tutti          # the CLI + TUI client
cargo install --path crates/tutti-server   # the daemon
```

Both binaries land in `~/.cargo/bin` (make sure that is on your `PATH`). You
never launch `tutti-server` yourself — the `tutti` CLI auto-starts the daemon
on first use, one per named session (`-s <name>`, default `tutti`).

## Optional finishing touches

- `tutti hooks claude --install` upgrades Claude Code state detection from
  screen heuristics to exact hook signals — recommended; see
  [Claude Code integration](agents.md#claude-code-integration). It shows the merge and
  asks before touching `~/.claude/settings.json`. `tutti hooks codex
  --install` and `tutti hooks gemini --install` do the same for Codex and
  Gemini CLI — see [Codex integration](agents.md#codex-integration) and
  [Gemini CLI integration](agents.md#gemini-cli-integration) — and `tutti hooks
  opencode --install` for opencode via a plugin — see
  [opencode integration](agents.md#opencode-integration).
- `tutti skill --install` gives the Claude Code agents in your panes the tutti
  skill, so they can start and coordinate sub-agents of their own — see
  [The tutti skill](api.md#the-tutti-skill).
- `eval "$(tutti shell-init zsh)"` in your shell's rc file lets a split start
  where you are and `tutti pane read --last-output` find the last command's
  output — see [Shell integration](shell-integration.md).
- Neovim or Emacs in a pane: the plugins in `editors/` let the editor and tutti
  share `Ctrl+h/j/k/l` and open files for `tutti edit` — see
  [Editors in panes](editors.md).
- The hooks, the plugin, the skill and the shell integration's rc line also
  install from the TUI: `Ctrl+B i` opens the setup panel (see
  [Setup panel](tui.md#setup-panel)).
- Using a Nerd Font? Set `icons = "nerdfont"` in the config for crisper
  sidebar glyphs (see [Configuration](configuration.md)).

## Upgrading

Re-run the two `cargo install` commands. A daemon that is already running keeps
executing the **old** binary until it is restarted — finish or detach your
agents, `tutti server stop`, and the next `tutti` starts the new one and offers
your layout back (see [Restoring after a restart](sessions.md#restoring-after-a-restart)).
The attach handshake refuses a daemon whose wire protocol the client cannot
speak, naming the end to update, so version skew is loud, never silent. The hooks run whichever
`tutti` is first on a pane's `PATH`, and a hook install from an older tutti
lacks the newer events — re-run `tutti hooks claude --install` (or `Ctrl+B i`),
which adds just the missing ones.
