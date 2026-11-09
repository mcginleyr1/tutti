# Workspaces, diffs and turns

tutti's version-control features, all of which need
[jj](https://jj-vcs.github.io) (there are no git or mercurial adapters):
workspaces, which give an agent its own checkout of a project; the
changed-files panel, which lists what a workspace has changed against trunk
(the `main` bookmark, else `master` or `trunk`); and turns, the per-turn
checkpoints that let you review and undo one agent turn.

## Workspaces

A **workspace** is a jj checkout nested under a **project** (a repo you opened) —
tutti's unit for fanning an agent onto its own isolated copy of the tree, without
touching the checkout you are working in (jj is the required VCS — there are no
git/hg adapters). A workspace renders **indented beneath its project** in the
sidebar and carries the project's own branch/changes/stale line.

Creating a workspace is **not** "run another agent in this project". When you
just want a second agent in the checkout you already have, use `n` (new agent)
on the sidebar row instead: it opens the launcher over that same workspace, in
a fresh tab, no new directory.

### From the CLI

**From the CLI.** `tutti workspace add <project> --name <name>` runs `jj
workspace add` and places the checkout beside the project's repo
(`<repo>-<name>`); `--dest <path>` chooses the directory instead (the TUI's
`where:` step), and `-r <rev>` the revision it starts from (one that names no
commit is refused before anything is made). The name must be `[A-Za-z0-9_-]+`,
since it becomes both a path component and a jj workspace name; the project
must live under a `.jj` repo, and the destination must not already exist.

When an add fails:
- tutti makes the destination itself, so an add that fails anyway (a timeout,
  jj or git refusing) removes exactly what it made: that directory, and the jj
  workspace only when `jj workspace list` records it there — a same-named
  workspace elsewhere is never forgotten.
- When jj gives no answer, or the
  forget fails, both stay and the error says so: jj never keeps a workspace
  whose checkout is gone.
- An add past its 15 s is given up on but left to
  finish (up to a minute), then undone whole: a jj stopped halfway can leave a
  Git worktree registered that git then refuses the next add over.

`tutti workspace list` shows every entry with the PARENT it is nested under,
`tutti project list` just the projects. Merge is scriptable too
(`tutti workspace merge <id> [--push]`), keeping the "every TUI action is a CLI
verb" invariant. `tutti workspace diff <id> [--stat]` prints the workspace's
diff against trunk; the whole list is in [cli.md](cli.md).

### Creating one in the TUI

**Create (`w`).** Off the sidebar (`C-b w` to focus it), `w` on a project (or on
an agent row → its project) opens a two-step prompt: `workspace name:` (validated
`[A-Za-z0-9_-]+`), then `where:`, prefilled with the sibling default and editable
with the same directory completion as add-project (`esc` steps back to the name,
then cancels). On submit tutti materializes the checkout, jumps to it, flashes
`workspace <name> → <path>`, and opens the launcher so you immediately pick the
agent to run beside its shell.

### Merging back

**Merge (`m`).** When a workspace's work is ready, `m` on its row merges it back
into the project's trunk bookmark (`main`, else `master`) after a `merge <name>
into trunk? y/N` confirm. The server rebases the workspace's branch onto trunk,
**refuses and undoes** the merge if it would land a conflict (`merge would
conflict — resolve manually in the workspace`), advances the bookmark to the
workspace's real commit, and — if the origin has a remote — `jj git push`es it.
On success it flashes `merged into <bookmark>` (adding `and pushed` when a push
ran) and offers to **clean up**: `y` discards the now-merged workspace. `m` on a
top-level project flashes `only workspaces merge`.

### Stale workspaces

Because several workspaces share one repo, a workspace's working copy can go
**stale** when its `@` is rewritten from elsewhere. Tutti surfaces this as a
dim-red `stale` tag on the sidebar row and never fixes it for you;
`tutti workspace update <id>` runs `jj workspace update-stale` to reconcile it
(the sidebar's `u` does the same).

### Removing a workspace

Two ways to remove a workspace, differing only in what happens on disk:

- `tutti workspace kill <id>` — the panes die and the workspace leaves tutti, but
  the jj workspace and its directory stay on disk. It is your checkout and your
  call; nothing is deleted.
- `tutti workspace kill <id> --discard` — additionally `jj workspace forget`s the
  checkout at its origin and removes its directory, as a failed add does: the
  forget only while jj records the workspace at that directory, so a
  same-named one added since elsewhere stays, and nothing when jj gives no
  answer. `--discard` is **only** honoured for a workspace tutti created; it
  is a hard error on any other workspace, so tutti never deletes a checkout
  it did not create.

Both are reachable from the sidebar: focus it, select the workspace (or one of
its agents), and press `x` — `y` is the plain kill, `D` is the `--discard`
variant. (After a merge, the cleanup prompt is the `--discard` path.) A
top-level project leaves with `tutti project kill <id>`, which never touches
its directory.

## Changed files panel

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
its edge (the sidebar gets the same). Its keys:

- **Moving.** It moves like vim (or `less`): `j`/`k`
  step, `g`/`G` jump to the first/last file, `Ctrl+D`/`Ctrl+U` move half the
  panel (arrows, `Home`/`End`, and `PageUp`/`PageDown` work too).
- `Enter` opens
  the highlighted **file's diff** against trunk in an ephemeral pager pane.
- `d`
  opens the **whole diff**.
- `e` opens the **file itself** in the workspace's
  editor pane — a Neovim or Emacs in one of its panes that advertises itself
  (see [Editors in panes](editors.md)), the focused pane's tab first — and
  jumps there; the band says `opened <file> in <pane>`, or why not (`no editor
  pane in <workspace>`, or the editor's own complaint). The editor's remote CLI
  runs off the TUI's loop, so a slow or dead editor never freezes the screen;
  `tutti edit` is the same hand-off for scripts.
- `h`/`q`/`esc` step back to the panes
  and `l`/`Ctrl+L` wrap on to the next tab, just as the pane's own right edge
  does.
- **Mouse.** Clicking a row jumps straight to that file's diff.
- On a turn (below), `Enter` pages that turn's diff and `r` restores it.

Like every diff
surface, it answers
"what has this workspace
changed that hasn't landed on trunk" — the delta since the fork point from
`main` (else `master`/`trunk`), so a workspace that fell behind an advanced
trunk never shows trunk's own progress as noise.

## Turns: review and undo one agent turn

Every time an agent's turn ends — its pane leaves **working** for done or
blocked — tutti records a **checkpoint**: the commit id of the workspace's jj
`@` right then. jj keeps a superseded `@` addressable by id, so one turn can be
diffed and restored long after later edits replace it. The snapshot is the one
tutti's change probe already takes (no extra jj operation per turn), and a turn
that changed nothing records none. Where the agent **started** is checkpointed
too — when tutti first detects the agent, or when the pane first enters working
— so turn 1 has a "before" to diff and restore to.

### The turns list

When the pane in view has checkpoints, the changes panel grows a `turns`
section under the files: the newest five, `#N age <turn> +A −B` — the turn
reads as the first line of the prompt that started it when Claude Code's hooks
reported one, else as what closed it: `done`, `blocked`, `interrupted`,
`start`, `restart`, `before restore`. A turn that ends blocked keeps its
prompt for the work after you answer, and so does one you type more into while
it runs. A turn you interrupt with `Esc` sends no stop, so tutti closes it — as
`interrupted`, under its own prompt — when the next prompt arrives with none
since.

### Reviewing and restoring a turn

The cursor walks
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

Scripts have the same three: `tutti pane checkpoints <pane>` lists a pane's
turns, `tutti pane checkpoint-diff <pane> [N] [--stat]` shows one turn's diff
(the newest by default), and `tutti pane restore <pane> <N> [--force]`
restores to turn N, `--force` doing so mid-turn ([cli.md](cli.md)).

### Known limits

- Checkpoints live in the daemon's memory (newest 50 per pane) and
  are saved with the layout, so a pane a restart restores keeps its list and
  numbering (see [Restoring after a restart](sessions.md#restoring-after-a-restart)). A
  turn still running when the daemon stopped is closed where the restore brings
  its pane back, as a `restart` checkpoint listed under the turn's prompt.
- Several agents in one workspace share a working copy, so a pane's turn diff
  includes anything else that changed in that window (another agent's edits, your
  own).
- An interrupted turn is checkpointed only once you send the next prompt
  (the agent says nothing until then), so anything changed in between — your
  own edits — joins it, and the pane reads **working** until that prompt.
- A
  restore rewrites the working copy
  only — work an agent committed stays in history, with `@` carrying the
  reverting change.
- Workspaces outside jj get no checkpoints.
