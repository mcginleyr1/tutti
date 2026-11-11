# Scripting tutti: orchestration, replies, tails and watch

How scripts, other tools and agents drive a tutti session: starting agents and
waiting on them, talking to an agent and reading its reply, tailing a pane's
output, the Claude Code skill that teaches an agent these commands, and
`tutti watch`, a JSON stream of the session's state. Every command here goes
over the daemon's Unix socket, and `--json` on any of them prints the raw
protocol response. The full command list is in [cli.md](cli.md), the guarded
answer to a blocked agent and held approvals are in
[answering.md](answering.md), and the frames on the socket are specified in
[transport-decision.md](transport-decision.md).

## Orchestrating agents

Scripts and agents drive tutti through the same CLI, and two verbs make it an
orchestrator; a third, `tutti ask`, lets one agent [talk to
another](#talking-to-an-agent).

### `tutti run`

`tutti run -- claude "fix the tests"` spawns the command in a fresh tab and
prints only the new pane's id, so `p=$(tutti run -- claude "fix the tests")`
captures it. It runs in `--workspace <id>`, or in the workspace that has
`--dir <path>` open (mounting the directory when none does); without either,
from inside a tutti pane it lands in that pane's own workspace, beside its
caller, and elsewhere in the session's current workspace. An orchestrated
agent never splits a tab someone is using, and the session's current tab stays
where it is, so an attached TUI is not pulled over and your next launch still
lands where you are looking. `--sandbox` and `--title` work as for `pane run`
and `pane rename`. A spawn that fails leaves no empty tab behind.

### `tutti pane wait`

`tutti pane wait <pane>...` blocks until each pane is `done` (finished, seen or
not), or reaches whichever `--until working|blocked|done|idle` states you name,
and prints each pane's state. The daemon answers from a subscription to state
transitions that misses none, however busy the session; nothing polls. A pane
already in the state answers at once; `--next` waits for a fresh transition
instead, and a human looking at a finished pane never counts as one. A pane
whose process exits ends its wait: a matching final state succeeds, and
anything else fails (exit 1) naming the exit code, as a pane killed mid-wait
does. `--timeout <secs>` bounds the whole wait across all panes and exits 124,
as coreutils `timeout` does. Include `blocked`: an agent stuck on a permission
prompt never becomes done.

```sh
for dir in ../api ../web ../docs; do
  panes="$panes $(tutti run --dir "$dir" -- claude "fix the failing tests")"
done
tutti pane wait $panes --until done,blocked --timeout 3600   # one state per pane, in order
```

## Talking to an agent

### `pane read --last-reply`

`tutti pane read <pane> --last-reply` prints the final message of the agent's
last finished turn as clean text: what its turn-end hook handed tutti
(Claude Code's and Codex's `Stop`, Gemini CLI's `AfterAgent`, the opencode
plugin's idle; Antigravity CLI's `Stop` names the transcript it is read
from), never a scrape of its screen. The daemon keeps each pane's
latest one (its first 64 KiB; a longer one notes the cut on stderr) until the
next turn ends. With no hooks reporting the pane, no turn finished yet, or a
last turn that ended without one (interrupted, or failed), it exits 1 saying
which. `--json` gives `{"type":"last_reply","text":"…","truncated":false}`.

### `tutti ask`

`tutti ask <pane> <message>` is the whole round trip in one blocking call, so
one agent can talk to another:

```sh
b=$(tutti run -- codex --no-daemon)
tutti ask "$b" "review crates/api/src/auth.rs and list the bugs you find" --timeout 600
```

- **When it types.** The daemon types the message only while the agent is between turns — not
  working, blocked or exited — checked in the same step as the write, so it
  never joins a running turn or answers a dialog (one just started can read as
  working for a moment while it draws; a refused ask types nothing and exits 1).
- **Which panes it takes.** It also refuses a pane whose agent has not reported through tutti's hooks —
  a plain shell, or an agent without `tutti hooks <agent> --install` — since no
  turn end of its would ever reach the wait.
- **A fresh Codex, opencode or Antigravity CLI.** Claude Code and Gemini CLI
  report as they start (SessionStart); Codex, opencode and Antigravity CLI
  only from their first turn,
  so for a fresh one `ask` checks tutti's integration itself — Codex's
  `hooks.json` in `$CODEX_HOME` (else `~/.codex`) or in the pane's project's
  `.codex/`, opencode's plugin in its user or project plugin dir,
  Antigravity's `~/.gemini/config/hooks.json` or the project's
  `.agents/hooks.json` — and,
  installed, the daemon takes the pane once detection names its agent (within
  a second of its start): the ask's message is its first turn. Codex runs a
  hook only once you have trusted it in its `/hooks`, and a project's
  `.codex/` hooks only in a project you have trusted; tutti sees neither, so
  an untrusted one leaves the ask to its `--timeout`.
- **The wait.** `ask` then presses enter (150 ms later: an agent can take text and an enter
  that land together as a paste) and waits for the next turn end the agent's
  hooks report, or a block. It counts turn ends from its own send, so a turn
  that ends before the wait starts still counts, and nothing else passes for
  one: neither a state read off the screen nor a Done the agent's exit left.
- **What it prints.** It prints the new reply as `--last-reply` would; a turn that ended without
  one (interrupted, failed) is that error.
- **An agent that exits** instead is an
  error naming the exit (exit 1), never an older reply.
- **An agent that blocks**
  instead — by its own word, a hook or its own status report, never a dialog
  read off its screen — exits 3, naming what it asks on stderr: answer it, then `tutti pane
  wait <pane> --next --until done,blocked` and `pane read --last-reply`.
- **Timeout.** `--timeout <secs>` bounds the wait and exits 124.

A human talks to an agent
by typing into its pane; the sidebar's `p` on a done agent shows its [last
reply](answering.md#reading-a-done-agents-reply).

## Tailing a pane

`tutti pane read <pane> --since 0` prints everything the pane holds and, on
stderr, `next cursor: N`. Pass that `N` back to read only the rows that
scrolled off since, then the live screen — the screen comes back on every
read, because it is not final until it scrolls. The cursor counts screen rows,
not logical lines, so `--unwrapped` folds each read on its own (it joins
exactly the rows the terminal soft-wrapped; a full-width line stays its own).
A note on stderr says when some rows had already left the 10k-row scrollback.
While the pane's alternate screen is up (vim, less, a full-screen TUI), a read
returns the rows that scrolled off before it, then that screen; the shell's
screen under it comes back with the next read once the program leaves. A
resize rewraps the scrollback to the new width and can number the rows it
keeps afresh, so a cursor from before one may see some rows again — never
skip one. A cursor past the pane's count — one from another pane or an
earlier daemon — is refused. `--json` gives
`{"type":"tail","lines":[…],"cursor":N,"truncated":false}`.

## The tutti skill

tutti ships a Claude Code skill that teaches an agent running inside a tutti
pane to herd its own sub-agents: start them (`tutti run`), give each an
isolated jj workspace (`tutti workspace add`), wait on them (`tutti pane wait
--until done,blocked --timeout …`), talk to them and read their replies
(`tutti ask`, `pane read --last-reply`), read and tail their screens
(`--since`), safely answer their prompts (`--fingerprint` / `--expect`) and held approvals
(`pane approve`), review their diffs and turns, and merge or clean up. It also
hands tracker issues to agents (`tutti issue deploy`) and reads the
notification history (`tutti notes --unread`). Its rules include never typing
into its own pane, only killing what it created, and never marking or clearing
your notes.

```
tutti skill --install            # into ~/.claude/skills/tutti/SKILL.md
tutti skill --install --project  # …into ./.claude/skills/tutti/SKILL.md instead
tutti skill --install --yes      # skip the confirmation (scripts)
tutti skill                      # or just print it…
tutti skill --raw                # …without the stderr instructions, for piping
```

`--install` (or the setup panel, `Ctrl+B i`) is idempotent, backs a previous
tutti skill up to `SKILL.md.bak`, writes atomically, and refuses to overwrite a
`SKILL.md` that is not tutti's (one whose frontmatter does not say `name:
tutti`). The source is `skills/tutti/SKILL.md`; a test parses every command in
it against the CLI, so it cannot drift from the verbs it teaches.

## Watching state from other tools

`tutti watch [-s <session>]` streams the session's state to another program —
a menu-bar or notch app, a status line — as JSON lines on stdout, each flushed
as it lands. Every line is a full snapshot of the session (replace, don't
merge). The first comes at once, then one on each change to any pane's state,
agent, title, exit, user vars, directory, held approval or issue, to the
workspace/tab/pane structure, or to the attached TUI's focus or terminal;
bursts coalesce.

```json
{"type":"snapshot","session":"tutti","wire_rev":20,"wire_floor":19,"attached_terminal":"com.mitchellh.ghostty",
 "panes":[{"id":3,"workspace":"smartnotch","dir":"/Users/me/develop/smartnotch","tab":"main",
           "title":"claude","agent":"claude","state":"blocked","source":"hook","exited":null,"focused":true,
           "user_vars":{},"cwd":"/Users/me/develop/smartnotch/src",
           "approval":{"id":4,"tool":"Bash","summary":"rm -rf build"},
           "issue":{"source":"gh","id":"123","title":"Fix the login",
                    "url":"https://github.com/acme/app/issues/123"}}]}
```

- `id` is the pane id `tutti pane focus` takes; `workspace` and `dir` are its
  project's name and absolute directory; `tab` is its tab's name.
- `agent` is the detected agent (`claude`, `codex`, …), a self-reporting
  pane's OSC 7501 `app` label, or `null` for a plain shell.
- `state` is `blocked`, `working`, `done`, `idle` or `unknown`; `source` is
  `hook`, `program`, `progress`, `screen`, `activity` or `null` (see
  [Agent state sources](agents.md#agent-state-sources)).
- `exited` is the exit code, `null` while running; `focused` marks the attached
  TUI's focused pane.
- `user_vars` holds what the pane's programs set with OSC 1337 `SetUserVar`
  (`TUTTI_KEYS`, `TUTTI_EDITOR`, …), name → value, `{}` when none (see
  [Editors in panes](editors.md)).
- `cwd` is the directory the pane's shell last reported (OSC 7, see [Shell
  integration](shell-integration.md)), `null` before a report.
- `approval` is the oldest permission request the pane's agent holds for an
  answer (see [Held approvals](answering.md#held-approvals)) — its `id`, `tool` and
  one-line `summary` — `null` without one; answer it with `tutti pane approve
  <pane>` (`--id` for another of them; `pane list --json` lists them all).
- `issue` is the tracker issue the pane's agent was
  [deployed on](issues.md) — `{source, id, title, url}`, `url` `null`
  when the tracker gave none — or `null` for every other pane.
- `attached_terminal` is the terminal app the attached TUI runs in (macOS's
  `__CFBundleIdentifier`), `null` with nobody attached or off macOS.
- `wire_rev` is the daemon's wire revision and `wire_floor` its floor: the
  oldest revision of a reader it still serves faithfully. A watcher written
  against revision `W`, which reads nothing that came after revision `Wf` (`W`
  itself, if unsure), can trust the snapshots exactly when `wire_floor <= W`
  and `Wf <= wire_rev`; otherwise it should say which end to update. A
  snapshot with no `wire_floor` is from a daemon before floors (revision 19
  and older): take its `wire_rev` for its floor. See [Revisions and
  floors](transport-decision.md#revisions-and-floors-wire-rev-20).

A watcher is read-only and is not an attached client: it sits beside the TUI
without touching its sizes or focus, and watching a **done** pane never marks
it seen. `watch` never starts a daemon — with none running it says so on stderr
and exits 1 — and exits 0 when the daemon goes away. To jump from such a tool to
a pane, run `tutti -s <session> pane focus <id>`: the attached TUI switches to
that pane's workspace and tab.

Both work across machines: `tutti --host <h> watch` streams a far session's
snapshots over ssh (tutti hands its process over to ssh, so stopping it stops
ssh), and `tutti --host <h> pane focus <id>` jumps every TUI attached to that
session — one attached from your Mac with `--host` too, whose
`attached_terminal` is your Mac's terminal app, so a menu-bar app there can
bring it to the front.
