# Notifications

How tutti picks up the bells and desktop notifications that programs in panes
raise, passes them on to your terminal, and keeps a history of them together
with every time an agent blocked or finished: the notifications panel
(`Ctrl+B N`) and the `tutti notes` commands.

## From a pane to your terminal

Agents inside panes emit terminal bells and desktop-notification escapes (OSC 9,
OSC 777, kitty's OSC 99). Tutti captures these from each pane's output and, for a **background**
(non-focused) pane, flashes the text in the status bar and re-emits a bell plus
an OSC 9 to *your* real terminal — so wezterm/kitty/etc. raise an actual desktop
notification naming the pane. The focused pane is left alone (you are already
looking). The status flash and re-emit are gated by `notifications = true`
(default); the sidebar 🔔 mark is always on regardless.

What counts as one: ConEmu's numbered
`OSC 9;N` subcommands are never notifications: `9;4` is a progress report (see
[Agent state sources](agents.md#agent-state-sources)), and the rest (`9;9;<cwd>`, which
shells emit for Windows Terminal, and others) are ignored — a body that merely
starts with digits (`9;42 tests passed`) still notifies. An OSC 7501 report's
`msg` notifies when it finishes or blocks the pane, and a herdr report's
blocked message when it blocks it — at most once per 30 s per pane.

## History

**History.** The daemon keeps the newest 200 notes: every bell and desktop
notification a pane raises (a herdr `notification.show` included), and every
attention transition — an agent blocking, or finishing a turn (working →
done). A transition's note carries what came with it — a hook's blocked
message (`Bash · rm -rf build`), or an OSC 7501 or herdr report's `msg` under
its app — so a report that finishes a pane with a message makes one note, not
two. Anything else an agent already blocked waits on — another permission
request (parallel subagents ask at once), an MCP form, a question — makes a
note of its own. Claude's `permission_prompt` — its reminder, six seconds
into a dialog — makes none on a pane already blocked, where its request is
noted already; one that blocks the pane itself (a late one arriving after
the pane went back to work included) is noted as that move. Each keeps the
pane's title
and workspace as they were, so it outlives
the pane.

### The notifications panel

`Ctrl+B N` opens the **notifications panel**, newest first: an
unread dot, the age, the pane, a dim `workspace · kind`, and the text. `j`/`k`
move, `enter` jumps to the note's pane and marks its notes read, `r` marks
everything read, `X` clears the history after a `y/N`, and `esc` closes; a note
whose pane is gone stays listed, dim. The footer chip reads `NOTES`, and the
app bar's `🔔 N` chip opens the panel too.

### Read and unread

Focusing a pane marks its notes read, and a pane whose notifications have been
read — anywhere, by any client or script — loses its sidebar 🔔; attaching
brings back the 🔔 of panes with unread ones, a transition's note that carries
a message counting as one. A bell from the pane you are
looking at is not recorded (a bell says nothing else), nor is a bell repeating
its pane's still-unread bell; any other note about a pane in view lands already
read.

### From scripts, and across restarts

Scripts and agents read the same history: `tutti notes [--unread]`
(newest first: `ID AGE PANE TITLE WORKSPACE KIND UNREAD TEXT`; `--json` for
the raw notes), `tutti notes read [--up-to <id>] [--pane <id>]`, and `tutti
notes clear`. The history is saved with the layout, so a restarted daemon
has it back (see [Restoring after a restart](sessions.md#restoring-after-a-restart)); an
empty session saves nothing, its history included.
