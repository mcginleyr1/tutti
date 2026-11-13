# The TUI: screen, launcher, panels and sidebar

What the TUI puts on screen around the panes, and the panels that open over
them: the app bar, pane titles and footer; the run launcher with its resume
rows; the go-to list; the setup panel; and the sidebar, the left column that
lists every project, workspace and agent. The keys that open each are in
[keys.md](keys.md). Answering an agent from the sidebar is in
[answering.md](answering.md), the changed-files panel and turns are in
[workspaces.md](workspaces.md), the notifications panel is in
[notifications.md](notifications.md), and the issues panel is in
[issues.md](issues.md).

## The screen: app bar, pane titles, footer

A full-width **app bar** runs across the top: an accent bar and the bold
`tutti — <session>` wordmark on the left (just `tutti` when the session shares
the name), the tab segments right-aligned (` 1 claude ●  2 logs  + `, the
active one a bold accent block, the rest dim), over a dim rule. Click a segment
to select its tab, the ` + ` to open one. A tab you haven't named borrows its
active pane's title, so segments say what they hold, not just where they sit.
A tab whose agents want you trails a state dot after its name — the most
urgent of its agents: red for blocked, then green for done, else the shared
working spinner — so a blocked agent in a background tab shows from the tab bar
itself; a tab of plain shells carries none. After the session title the app
bar also carries a census of agent states and the unread notification count:
see [The census in the app bar](#the-census-in-the-app-bar).

Each pane **wears a colour**: its agent's state while that is one to notice
— red blocked, yellow working, green done — else the accent on the focused
pane and a quiet grey on the rest. Its rounded frame is that colour, drawn
heavier on the focused pane; its ground takes a faint tint of it; and its
**title row** carries its name as a chip of it (a solid block on the focused
pane), then what the agent is doing, with the pane's id (`#29`, the id `tutti
pane …` takes) at the far end.

**Frames that cost no cells.** In a terminal that speaks the
[cell-border protocol](https://www.joshka.net/cell-borders/) — tutti asks at
attach — a pane's frame is lines on the edges of its own cells, around the
title row and all: no row or column of box characters, so each pane shows
two rows more. Panes then stand a small gutter apart (two columns, one row),
which is where the mouse grabs a split's border, and the rule under the app
bar is a line on the bar's own edge. The sidebar's two sections and the
changes panel's files and turns are framed the same way, each a card under
its header: in the accent while it holds the keys, and the waiting queue in
red while an agent in it is blocked (green when all it holds is done).
Everywhere else a pane's frame is box characters under its title row, as
before, and the panels have none.

The
**footer** earns its row: in terminal mode it shows where you are (an accent
dot, the active workspace's name, its branch and jj change stat); leaving
terminal mode swaps in a mode chip (`SIDEBAR`, `SCROLL`, …) on the left, with
the standing hint always on the right; a transient fires a one-line
**notification band** just above it (accent for info, red for errors).

## Run launcher

**Run launcher** (`Ctrl+B r`): a floating ` run in <project> ` panel that answers
"what should start here?" — the agents **installed on the daemon's machine** first
(`claude`, `crush`, …), then `shell` and `command…`, then the rest of tutti's
agent catalog dim and unselectable at the foot, each row naming the product and
its project link (`gemini   Gemini CLI · github.com/google-gemini/gemini-cli`)
so the picker doubles as "what else is out there". Its title names the
workspace the choice lands in, so it is never ambiguous where a pane will
spawn.
Move with `j/k`/arrows, press a row's number (`1`–`9`; a later row and the
dim catalog show none, and a later row is reached by moving to it) to launch
it outright, `enter` launches the highlight, `esc`
closes; `command…` opens a one-line input that runs
whatever you type via your login shell. With [sandboxes](sandboxes.md)
configured, `tab` switches where the pick runs — locally, then each sandbox
profile in turn (a `tab run in …` hint row says what is next). The title
follows (` run in <project> · sandbox <name> `). A sandbox's rows are the whole
catalog, since tutti can't see what an image carries, and its shell and
command rows run `sh`. Adding a project (`a` in the sidebar) opens the launcher
for the new project's first pane too — `esc` there just drops you into a shell.
(Startup `[[projects]]` still mount plain shells, no launcher.)

### A project without tutti's hooks

Pick an agent — or one of its resume rows — for a project that has none of
tutti's [hooks](agents.md) for it, when your user config has none either, and
the launcher asks before it starts: `no Claude Code hooks in this project:
install into .claude/settings.json?` `y` (or `enter`) installs them into the
project, as the [setup panel](#setup-panel) would on its project scope, and
runs the pick, so the agent starts with them. `n` runs it without, and the
launcher does not ask again for that project until you reattach. `esc` goes
back to the list. Without the hooks tutti reads the agent's state off its
screen and cannot bring its conversation back after a restart. A sandbox's
agent and one an issue is deployed to are not asked about.

### Resume rows

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

### An exited pane

An **exited** agent pane advertises the way back in its title — `exited 0 ·
r resume · x close`. `r` opens the launcher over that pane's workspace (resume
rows on top) armed to **replace the corpse**: the pick runs in the dead pane's
tab and the corpse is dropped, so a finished or accidentally-quit agent is two
keystrokes from its own conversation. `x` just closes the corpse. An exited
[sandbox](sandboxes.md) pane adds `p pull`, which brings the
sandbox's pushed work back as a workspace under its project.

## Go to

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

## Setup panel

**Setup panel** (`Ctrl+B i`): a floating ` setup · user ` panel listing what
tutti installs — the Claude Code, Codex, Gemini CLI and Antigravity CLI
hooks, the opencode plugin, the [tutti skill](api.md#the-tutti-skill), and the [shell integration](shell-integration.md)'s
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

## Sidebar

A left column that turns the TUI into a control center for many projects and
agents at once — a borderless panel, each section
labelled by a quiet lowercase header (` ▾ projects`, ` ▾ waiting` — bold but
dim, so weight carries the hierarchy without brightness; caps stay reserved
for the footer's mode chip) with a `▾`/`▸`
collapse arrow and a right-aligned count. A blank spacer row above the
waiting header keeps the two sections from running together. Names render at full intensity and
everything else recedes dim, so the eye lands on identity; the focused row
pops in the accent colour. Two stacked sections, **projects** and
**waiting**.

### The projects tree

**projects** — a tree. One row per top-level project with **its agents
nested beneath it** on `├`/`└` guides, then any [nested
workspaces](workspaces.md#workspaces) (indented, guided, same two-line row, name bold),
each followed by *its* agents one level deeper.

- **A workspace row** is an `●`
  (accent) when it owns the active tab, else a dim `○`, then the bold name —
  with right-aligned roll-up chips (`● 2 ● 1`) counting the subtree's
  blocked/working/done agents — over a dim line showing a branch marker and
  the git branch (read straight from `.git/HEAD`, including worktrees) on the
  left and the jj change stat (`4 files +120 −33`) right-aligned — the
  workspace's whole delta **against trunk**, not just its working copy, so it
  tracks what the workspace has built up rather than going blank each time an
  agent commits. No branch
  leaves the left blank rather than echoing the name; a workspace clean
  against trunk or non-jj shows no stat.
- **The stat** refreshes as agents work (on
  every state transition, on attach, when a workspace is created, and whenever
  jj records an operation in the workspace — a commit, bookmark move, or push
  from any shell) and is dropped first
  when the column is too narrow for both.
- **The stale tag.** A nested [workspace](workspaces.md#workspaces)
  whose `@` was rewritten from another workspace shows a dim-red `stale` tag
  in the stat's place until you run `workspace update`.
- **An agent row** is a
  state dot (blocked red, working an animated spinner, done green,
  idle/unknown dim), the pane title (then, dim, the tracker issue it was
  [deployed on](issues.md) — `#123`, `ENG-42`), and a dim `state ·
  kind` line, with any hook-reported subagents hanging below on their own
  guides. Agent rows keep **pane order** — a state change never reshuffles the
  tree under your cursor; attention order lives in the waiting queue.
- **Selecting** a workspace jumps to the tab you last had open there (one you
  have not opened since attaching: its first tab with a program still
  running); selecting an agent jumps to its pane, switching workspace and tab
  as needed.
- **The 🔔 mark.** A pane that rings a bell or
  fires a desktop notification while in the background gets a 🔔 mark that
  clears once you focus it or its [notifications](notifications.md) are read.

### The waiting queue

**waiting** — the cross-project attention queue: every **blocked** or
**done** agent from *every* project, blocked first, each over a dim
`project · kind` line naming where it lives (subagent rows stay in the
tree). Each [permission request](answering.md#held-approvals) an agent holds leads the
queue on a row of its own, in the order they came, whatever the agent's
state, the row reading the tool and its input (`Bash · rm -rf build`). `Enter` (or a click) jumps to the agent itself, so a stuck
agent two projects away is never invisible and always one keystroke away —
and `p` answers it without going there at all
([Answering from the queue](answering.md#answering-from-the-queue)).

### The census in the app bar

The app bar answers "does anything need me?" from across the room: after the
session title it carries a state census (`● 1 blocked · ⠋ 2 working · ● 1
done`) counting every agent in the session, empty when nothing runs, and ends
with the unread notification count (`🔔 N`, dim at zero) — click it to open the
[notifications panel](notifications.md).

### The selected row

The selected row (while the sidebar is focused) takes a subtle full-row
highlight with its name rendered as an accent chip, unmistakable even with a
single entry. The highlight follows the row's **identity**, not its position:
when a kill or a state change reshapes the list, the cursor stays on the row
it was on (falling back to the row's project when the row itself vanished),
and a kill confirm re-checks its target before firing — a confirm can never
land on the wrong workspace or agent. Click a section header (the `projects`
or `waiting` row) to collapse that section down to its header, or click again
to expand it.

### Sidebar keys

Press the prefix then `w` to focus the sidebar (revealing it if hidden); the
cursor lands on the active project's row. Its keys — like the changes panel's
— come from one table that drives dispatch, the footer's hints (the verbs) and
the help overlay (every key), so the three never disagree. While focused:

- `j`/`k` (or the arrows) move the highlight across both sections.
- `Enter`
  jumps and hands focus back to the pane.
- `p` peeks at a blocked agent to answer
  it in place (on a done agent it shows its [last reply](answering.md#reading-a-done-agents-reply)).
- `esc` unfocuses (esc is the back key
  throughout).
- `n` starts a **new agent** (or shell/command) in the
  selected project: it jumps to that workspace and opens the launcher over it, and
  the choice lands in a **fresh tab** there rather than splitting whatever that
  tab already holds (an agent row targets the workspace that owns it) — reach for
  `n`, not `w`, when you just want another agent in the checkout you already
  have; splits stay behind `Ctrl+B %`/`"` and `Ctrl+B r` runs into the current
  tab.
- `a` opens the
  **add-project** prompt — a one-line field for the path to an *existing* directory
  to open (it mounts that directory as a workspace; it never creates one). The
  field prefills the common parent of your current projects, so you type just the
  project's name, and it completes directories as you type: `Tab` fills in the
  highlighted match and opens it (a trailing `/` reveals its contents), `↑`/`↓`
  move the highlight, and `Enter` always opens whatever is typed. `~` expands to
  your home; relative paths resolve against the client's cwd and are canonicalized
  to an absolute path before the daemon sees them; a bad directory surfaces the
  server's error.
- `d`
  opens the selected workspace's **jj diff against trunk** in an ephemeral pane — a
  real terminal paging the workspace's whole delta since it forked from `main`
  (else `master`/`trunk`), coloured, that vanishes the moment you quit `less`
  (pressing `d` on an agent row opens the diff for the agent's workspace). It
  diffs the *accumulated* work, not just the working copy — agents commit as they
  go, so a bare `jj diff` would almost always be empty. A
  non-jj workspace shows a transient error instead of spawning.
- `w` opens **guided [workspace](workspaces.md#workspaces) creation** on the
  selected project: it starts a new
  [**workspace**](workspaces.md#workspaces) nested under the selected project: a two-step prompt
  — `workspace name:` (`[A-Za-z0-9_-]+`; a bad name flashes the rule) then `where:`,
  prefilled with the sibling default (`<repo-parent>/<repo>-<name>`) and editable
  with the same directory completion as add-project (`esc` steps back a step, then
  cancels). On success tutti jumps to the new workspace, flashes `workspace <name>
  → <path>`, and opens the launcher to pick the agent to run beside its shell.
- `m` **merges** a child workspace back into its project's trunk (`main`, else
  `master`) after a `merge <name> into trunk? y/N` confirm: on success it flashes
  `merged into <bookmark>` (and `and pushed` when a remote took it) and offers to
  **clean up** (discard) the merged workspace; `m` on a top-level project just
  flashes `only workspaces merge`.
- `u` runs `jj workspace update-stale` on a
  workspace whose row shows the **stale** tag, clearing it; on a healthy row it
  just flashes `not stale`.
- `x`
  **kills** the selected row after a one-line confirm. On a workspace row — `y`
  removes it from tutti (leaving its checkout on disk), `D` also **discards** a
  workspace's checkout (`jj workspace forget` + delete; the server refuses this
  for a workspace tutti did not create, surfacing that error), and any other key
  cancels. On an agent row, `x` kills **only that agent's pane** (`kill claude?
  y/N`) — never the workspace around it.
- **Mouse.** A mouse click focuses the sidebar, or
  jumps straight to the entry clicked; a right-click on a blocked agent's row
  peeks at it instead.

### When the sidebar shows

Visibility is set by the `sidebar` config value: `on` (default) always shows it
— the control column is the point — while `auto` reveals it only once the
session is worth surfacing (more than one workspace, or at least one agent pane)
and `off` keeps it hidden until you focus it with `w`. It is suppressed on very
narrow terminals so panes keep usable room. With the sidebar off, a **blocked**
agent still rings the bell and turns its pane border red until you focus it.
