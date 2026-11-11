# Configuration

The config file: where it lives, every key with its default, chord syntax, the
escape time, the keymap presets, themes and colours, and the icon sets.
Sandbox profiles and issue trackers are configured here too; what they do is
in [sandboxes.md](sandboxes.md) and [issues.md](issues.md).

## The file

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
approvals = "watch"     # Claude/opencode permission requests: "watch" (default: block the
                        # pane, the agent shows its dialog) or "hold" (answer from tutti)
approval_timeout_secs = 120  # a held request falls through to the agent's dialog after this (1–599)
escape_time_ms = 15     # how long a lone Esc waits for the key after it, which then goes
                        # with it as that key with Alt — tmux's escape-time (0–1000)

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

# Kubernetes sandboxes agents can launch into (see sandboxes.md).
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

# Issue trackers to deploy from (see issues.md). Every key but `name`
# and `kind` is optional, except a command source's `command`.
[[issues]]
name = "gh"               # [A-Za-z0-9_-]+, what the panel and --source call it
kind = "github"           # through your own `gh` (it is the auth)
repo = "acme/api"         # default: the repo the project's git remote names
search = "label:agent"    # a `gh issue list --search` query
limit = 50                # default 50
comment = true            # a deploy comments where its agent runs (default false)
assign = "@me"            # ...assigns the issue to this login
label = "agent"           # ...and adds this label

[[issues]]
name = "linear"
kind = "linear"           # Linear's API through `curl`
api_key_env = "LINEAR_API_KEY"  # where your personal API key is (the default)
team = "ENG"              # only this team's issues
# filter = { priority = { lte = 2 } }  # an IssueFilter; replaces the default
                          # (your assigned issues not completed, canceled or duplicate)
limit = 50                # default 50
comment = true            # a deploy comments where its agent runs (default false)
state = "In Progress"     # ...and moves the issue to this workflow state

[[issues]]
name = "jira"
kind = "command"          # any tracker: prints [{id, title, body?, url?, labels?}]
command = ["~/bin/jira-issues", "--json"]
prompt = "Fix {id}: {title}\n\n{body}\n\n{url}"  # any kind; the agent's first message

# A shell file an agent is started behind, by its catalog name (see below).
[agents.omp]
source = "~/secrets.sh"   # absolute, or under `~`
```

### Chords, and what is a default

Chord syntax is `C-<char>` (Ctrl), `A-<char>` (Alt), or a bare printable
character. The top-level keys and `[keys]` above are exactly the defaults, so a
file that reproduces them changes nothing; `[[projects]]`, `[[sandboxes]]`,
`[[issues]]` and `[agents.omp]` are examples.

## A file an agent starts behind

`[agents.<name>] source = "<file>"` gives one agent what it needs from your
shell — an API key, say — without putting it in every pane's environment.
`<name>` is the agent's name in the [catalog](agents.md#the-catalog)'s
launcher (`claude`, `omp`, `antigravity`, …); an unknown one is an error.

- **What it does.** The daemon starts the agent as `/bin/sh -c '. <file> &&
  exec <agent> …'`: the file is sourced by `sh`, so keep it to POSIX
  (`export KEY=…` lines), and then the agent takes the shell's place. The
  pane is the agent's own: its title, its detection, the command saved with
  the layout. A file that is missing or fails ends the pane on the shell's
  error, in the pane.
- **Where it applies.** The launcher's agent and resume rows, an issue
  deploy, `tutti run` and `tutti pane run` when the command is that agent's
  (named as detection names it, so Cursor run as `agent` is matched only from
  its launcher row), and a [restore](sessions.md#restoring-after-a-restart)
  reviving the agent's conversation.
- **Where it does not.** A sandbox, which cannot read this machine's file;
  the launcher's shell and `command…` rows; an agent you type into a shell
  yourself.
- **Whose config.** The daemon's machine's: under `tutti --host` the file is
  read there, from that machine's config.

## The escape time

**The escape time.** A terminal sends Alt+x as `Esc` then `x`, so a lone `Esc`
waits `escape_time_ms` for the key after it, in the TUI and in `tutti pane
attach`. A key within it goes with the `Esc` as that key with Alt: an `A-…`
binding, or Alt+key to the pane. Past it, the `Esc` is Esc. The default, 15
ms, is far longer than the gap within one sequence a terminal writes, and too
short to notice: Esc in Neovim feels instant. Raise it (to 100–300 ms) to type
Alt as `Esc` then a key, on a terminal whose Option or Alt key sends no Meta.
A terminal that speaks kitty's keyboard protocol sends Esc unambiguously, so
the TUI never waits on it there.

## Presets

**Presets.** `preset` selects the *prefix* keymap table. The which-key popup and
help overlay always render whichever preset is active, so they stay accurate.

- `default` — the emacs/tmux-flavored table in [keys.md](keys.md#prefix-keys) (`%`/`"` split, `x` kill,
  `d`/`q` detach, …). The `C-b` prefix chord is itself already emacs-style.
- `vim` — mnemonics vim users reach for: `v`/`s` split right/below, `h/j/k/l`
  directional focus, `q` kill pane (`:q` closes a window), `d` detach (so detach
  stays reachable), `t` new tab, `n`/`p` next/prev tab,
  `w`/`g`/`f`/`N`/`r`/`I`/`i`/`z`/`[`/`m`/`?` unchanged.

An unknown preset is a hard error. A dedicated `emacs` preset may land later; for
now `default` already covers that muscle memory. The `[keys]` direct bindings are
shared across presets and override the defaults on top (e.g. set
`focus_left = "none"` to reclaim `C-h` regardless of preset).

## Colours and themes

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

**Chrome colors.** tutti's chrome — the app bar, sidebar, footer, and the
which-key/help/notification panels — paints no background of its own. It draws
over your terminal's own background, so when your terminal flips light/dark (for
example following the OS appearance) the chrome flips with it, live, exactly like
the agent panes do. The only background tutti paints is the selected sidebar
row's highlight, and that uses an ANSI palette colour (not a fixed RGB), so the
terminal repaints it from its own scheme too. At attach, tutti asks the terminal
whether its background is light (OSC 11), and asks again each time the terminal
reports a light/dark switch (mode 2031). If it is, the highlight uses ANSI
7 (white) rather than ANSI 8 (bright black), which some light schemes set to
pure black. On a terminal that reports no switches, that choice lasts until
you reattach. There is nothing to configure.

## Icons

**Icons.** `icons` picks the sidebar glyph set. `unicode` (default) is the safe
set every terminal renders — `●`/`○` workspace and state dots, a branch marker,
and a `⑂` stale marker. `nerdfont` swaps in private-use icons (folder, circles,
powerline branch, and the stale marker) for users with a patched
[Nerd Font](https://www.nerdfonts.com)
installed; without one they render as tofu. The tree guides and the working
spinner are box-drawing/braille and shared across both. An unknown value is a
hard error.
