# Sessions: stopping, restoring, several clients, other machines

A session is one named daemon and everything it owns: its projects, tabs and
panes (`-s <name>`, default `tutti`). This page covers what happens to a
session over time: stopping it, getting its layout back after a restart,
attaching from several terminals at once (or only to watch), attaching from
another machine over ssh, and what a crash costs.

## Stopping

Detaching never kills anything. To stop a session's daemon (and every pane it
owns), run `tutti server stop` (add `-s <name>` for a non-default session). The
daemon is per-session, so stopping one leaves the others running.

## Restoring after a restart

Stopping loses nothing you arranged. The daemon saves its layout as it goes —
every project and workspace, each tab's splits and their ratios, and each
agent's conversation and turns — with the [notification
history](notifications.md), to `$XDG_STATE_HOME/tutti/<session>.json` (else
`~/.local/state/tutti/`), rewriting it whenever either changes and once more
as it stops. Pane contents are not saved.

The next daemon never restores on its own: the first attach opens **restore
last session?** (`3 projects · 7 panes · 2 agents to revive · saved 2h ago`).
`enter` brings it back, `d` discards it, and `esc` leaves it for the next
attach; it comes before the first-run prompt. Scripts use `tutti server
restore [--discard]`.

- An agent pane holding a conversation id — reported by Claude Code's, Codex's
  or Gemini CLI's hooks, the opencode plugin, or a herdr report — comes back
  resuming exactly that
  conversation under its old title, when tutti knows the tool's resume recipe
  (Claude Code, Codex, Gemini CLI, opencode, Muse Code, Mistral Vibe). Every other pane comes
  back as a fresh shell in its workspace. A saved command is never re-run:
  re-running `claude "fix the tests"` would re-issue the prompt.
- Each pane's [turns](workspaces.md#turns-review-and-undo-one-agent-turn) come back too,
  numbering continued, and a revived agent stays linked to the
  [issue](issues.md) it was deployed on.
- Exited, ephemeral and sandbox panes are not saved.
- The notification history comes back at once, before any restore, read
  marks and all; a restore then points each note at the pane it came back
  as, whose sidebar 🔔 is back at once when it has unread ones. A note whose
  pane did not come back stays listed, dim.
- A project whose directory is gone is skipped and named; one already mounted
  (a configured project) takes the restored tabs beside its own.
- A save that cannot be read is set aside as `<session>.restore.json.bad` and
  never stops the daemon.

## Attaching from several terminals

Several terminals can attach to one session at once. Each pane takes the
smallest rows and the smallest columns among the clients showing it, as in
tmux, and grows back when the smaller client detaches or switches away; a pane
nobody shows keeps its size. A client whose window is bigger than a pane draws
the pane from its top-left. `tutti watch` watchers never count.

### Watching only: `attach --view`

`tutti attach --view` attaches a TUI that only watches, for a second screen or
someone pairing with you; its footer leads with `view`.

- **It navigates.** It moves around as any
  client does — focus, tabs, the sidebar and the other panels, scrollback — and
  detaches.
- **Nothing it does reaches a pane:** keys, pastes and mouse reports
  go nowhere, it casts no size vote, its terminal's colours are not told to the
  panes, and no request that changes the session (a split, a kill, a launch, an
  approval, a merge) leaves it.
- **Its focus is its own:** the daemon knows where it
  looks (a move past that pane's edge comes to it), but focusing a done agent
  marks it nobody's seen, a note about the pane it looks at stays unread — a
  bell there lands — and it moves no pane of the session's; a script or a
  watcher takes another attached client's focus over a view's.
- **Notes and approvals.** It marks no
  note read and clears none (the notes panel's `r` and a jump to a note mark
  nothing; the daemon refuses them from a view), and a held approval does not
  wait on a view, which cannot answer it.
- **An older daemon.** A daemon from before views (wire rev
  16) would take one for a client like any other. It is below the wire
  floor, which every attach refuses: restart the daemon.
- **Sizes.** The panes keep the sizes the other clients vote. With no other client they keep the size they have, drawn from
  the top-left and cut where the view is smaller: a resize would reach the
  pane's program, so a view never makes one.
- **No daemon, no restore.** It never starts a daemon, and is
  offered no restore.

## Attaching from another machine

`--host <destination>` uses the daemon on another machine over ssh — any
destination ssh takes, `user@host` or a `Host` from `~/.ssh/config`:

```sh
tutti --host devbox              # attach: the TUI draws here, the panes run there
tutti --host devbox pane list    # any other command runs there, as typed
```

Attaching runs `ssh devbox tutti proxy`, which carries the session to the far
daemon (starting it if need be) and answers everything the TUI asks about that
machine. So the directories you complete and add, the launcher's installed
agents, login shell and resumable conversations, the config's startup
projects, sandboxes and issue trackers, the issues panel's fetches, a deploy's
write-back and the file a long first message goes through, the setup panel's
installs and the changes panel's editor are all the far machine's — the one
the panes run on. Keys, theme, mouse and icons come from this machine's
config. Any other command runs on the far side whole: its paths, config,
output and exit code are that machine's, with a terminal there when yours is
one. `tutti --host devbox attach --view` watches without starting a daemon
there, as a local view does.

- Both machines need the same tutti build — the proxy's hello carries a
  revision, and a mismatch is refused — with `tutti` on the far side's
  non-interactive `PATH`, which is what `ssh devbox tutti --version` finds.
- ssh asks its questions (a password, a new host key) before the TUI opens. A
  connection that drops ends the TUI with ssh's own message; nothing ever
  falls back to a daemon on this machine.
- Each CLI call is its own ssh connection; `ControlMaster auto` with
  `ControlPersist` in `~/.ssh/config` makes them quick.

## When something crashes

The daemon is built to outlive its own bugs. A panic in a background pass —
the render tick, the classifier, detection, the VCS watch — is logged and the
pass carries on; a panic while handling one connection's request drops only
that connection, and one in a request's own task (a merge, a diff, a wait)
answers that request with an error; a lock a panic poisoned is recovered; a
failed `accept` is retried.

A pane is the unit of failure. Its terminal engine
is built never to panic, whatever a program writes — it is property-tested on
arbitrary bytes and resizes — but were it to, that pane alone ends: its
program is killed, its screen says the terminal crashed and why, a `pane
crashed` note joins the history, the panic goes to the log, and every other
pane runs on.

The client
survives its own parser's: a pane update its parser cannot apply
resets the pane and asks the daemon for a fresh copy, and any other client
panic restores your terminal on its way out.

### The daemon's log

The detached daemon appends its stderr — panics, crash notes — to
`<session>.log` beside its socket (`$XDG_RUNTIME_DIR/tutti/`, else
`/tmp/tutti-<uid>/`); `tutti server start` prints the path. A log past 4 MiB is
trimmed to its newest MiB when the daemon starts, and within a second of
passing it while the daemon runs.
