# Answering agents: the peek, replies and held approvals

How to answer an agent from the sidebar's waiting queue (see
[tui.md](tui.md#sidebar)) without leaving the pane you are in: the peek, which
types into a blocked agent's prompt behind a safety check; the reply panel,
which shows what a finished agent said; and held approvals, where a
permission request waits for a yes or no from tutti. Each has a form for
scripts: `pane send --expect`, `pane read --last-reply` and `pane approve`.

## Answering from the queue

`p` on a **blocked** agent's row (in the waiting queue, or its tree row) — or a
right-click on it — opens a **peek**: a floating ` answer <agent> · <workspace> `
panel showing that agent's live prompt, the tail of its screen (exactly the
rows the safety check below covers), drawn from the screen tutti already
mirrors, so it updates as the agent redraws. (A row holding a permission
request opens the [approval overlay](#held-approvals) instead.) While it is
open, answer keys go
straight to that agent: digits and letters (`1`, `y`/`n`, …), `Enter`, the
arrows, `Tab`, `Backspace`. Nothing else moves — your focused pane, tab, and
layout stay exactly where they were, and closing the peek leaves you on the
sidebar row you opened it from. `esc` closes it and is never forwarded (esc is
the back key throughout); for an esc or free-form typing, close the peek and
press `Enter` on the row to jump into the pane. Clicks inside the panel do
nothing (the pane beneath never takes focus); a click outside closes it. The
footer shows an `ANSWER` chip while it is up.

### The guarded send

Every key is a **guarded send**: tutti fingerprints the prompt you are looking
at (the last 12 non-empty rows of the agent's screen, trailing whitespace
ignored) and the daemon re-reads the live screen before writing. If the prompt
changed since you looked — the agent moved on, or someone answered from
another client — the key is refused and **nothing is written**; the reason
flashes in the notification band and the peek stays open over the new prompt.
The first answer to a prompt wins: until the agent visibly redraws, a second
answer to the same prompt is refused too. A guarded send also only ever goes to
a pane that is blocked. Once the agent leaves the blocked state the peek closes
itself.

### The same guard from scripts

Scripts get the same guard (`--json` works on both):

```sh
fp=$(tutti pane read 3 --fingerprint)   # take the fingerprint first…
tutti pane read 3 --lines 12            # …then read what it covers and decide
tutti pane send 3 --text 1 --expect "$fp"  # refused if the prompt moved on
```

Taking the fingerprint *before* reading keeps it safe: if the screen changes in
between, the fingerprint is the older one and the send is refused.

## Reading a done agent's reply

`p` on a **done** (or idle) agent's row opens a floating ` reply <agent> ·
<workspace> ` panel with the final message of its last finished turn, as its
hooks reported it — the text alone, without the agent's input box, spinners or
wrapping — so you can see what each finished agent said without visiting it.
It reads only: `esc` closes it, back on the row, and a click outside does too.
The footer shows a `REPLY` chip while it is up. A pane with no reply to show
(no hooks, no finished turn yet, or a turn that ended without one) flashes
why instead. Scripts read the same with `tutti pane read <pane> --last-reply`
(see [Talking to an agent](api.md#talking-to-an-agent)).

## Held approvals

With the [Claude Code hooks](agents.md#claude-code-integration) installed, a permission
request blocks its pane the moment Claude is about to ask, naming the tool and
its input. By default (`approvals = "watch"`) that is all: Claude shows its own
dialog, which you answer in the pane or through a peek. Set `approvals =
"hold"` and the hook also holds the request for an answer from tutti.

### Beside the agent's own dialog

Claude Code 2.1.318 shows its own dialog beside the hold (Codex 0.166.0
keeps its back while the hook runs). An answer from tutti closes that dialog
(`Allowed by PermissionRequest hook`). A request answered in the dialog
instead leaves its hook waiting: tutti lets go of it when that call's run
reports, a new prompt comes, or the turn of the agent that asked ends. A run
names its call as the request does: the agent asking (the main agent, or a
subagent by the hook's `agent_id`), the tool and its whole input — less the
`description` Codex's request alone adds, its reason for asking — so another
agent's run, or a parallel call's, lets go of nothing. Calls alike in all
three can't be told apart: a run lets go of the oldest such request, and if
that one's own call has not run yet, the agent's own dialog asks for it
(and the pane may read working meanwhile).

### Answering a held request

- **In the waiting queue.** The agent's waiting row reads `Bash · rm -rf build`, and a held approval
  leads the queue whatever the pane's state. A pane holds every request its
  agent raises — parallel subagents ask at once — each on a waiting row of
  its own, in the order they came, answered on its own and waiting out its
  own `approval_timeout_secs`. The pane stays blocked until the last is
  answered, though the agent works meanwhile on what you allowed (Codex
  runs parallel tool calls, each asking on its own).
- **The approval overlay.** `p` on that row (its tree row too), or a right-click, opens an **approval**
  overlay framed and placed like the peek: the tool over its input, line
  breaks shown as `⏎`, long input cut with `…` past 500 characters. `y` allows
  the tool call, `n` denies it, and `esc` passes it on to Claude's own dialog,
  which then appears in the pane. The footer shows an `APPROVE` chip; a click
  outside closes the overlay without answering.
- **Opening by itself.** When the pane you are looking at is the one asking, the overlay opens over
  it by itself, the way Claude's dialog would — the oldest request first, the
  next once that one is answered — and ignores keys for its first 800 ms, so
  a `y` or `n` already on its way into the pane can't answer it. It never
  opens over a panel, prompt or overlay you are using, and it closes itself
  once the approval ends — answered from another client or the CLI, timed
  out, or the pane gone or its program exited (an exit drops every request
  the pane held).
- **From scripts.** Scripts and agents answer with `tutti pane approve <pane> [--id <n>]
  [--deny] [--message <reason>]`: the oldest request, or the one `--id`
  names. The reason is what Claude is told; it defaults to "the user denied
  this in tutti". `tutti --json pane list` lists a pane's held requests,
  oldest first, as `approvals: [{id, tool, summary}]` (and the oldest alone
  as `approval`, for older scripts).

### When a held request falls through

A held request falls through to Claude's own dialog — the hook printing
nothing and exiting 0 — when no TUI is attached to answer it, on `esc`, and
when nobody answers within `approval_timeout_secs` (default 120, at most 599:
Claude stops a hook at 600 s). An agent's turn ending also lets go of the
requests that agent raised, as a fall-through to its dialog: the main
agent's Stop, StopFailure or Codex's Interrupt lets go of the main agent's,
and a SubagentStop lets go of that subagent's (a request names its subagent
by the hook's `agent_id`). A background subagent can still run, and ask,
after the main agent's Stop, so its requests wait for its own stop. A
request answered in Claude's dialog therefore never lingers on a finished
pane, and an answer from tutti that comes after the turn end is refused
with a message saying so. The hook never fails a Claude session.

### The pane's state after an answer

An allow
or a deny from tutti sets the pane **working** at once — the tool runs, or the
agent hears the refusal — unless another request still waits on you: held in
tutti, or one that fell through to Claude's dialog, until a hook (or the
screen, below) shows Claude moved on. Answered in Claude's own dialog, the pane goes
back to **working** once its screen shows Claude busy again — its spinner's
`(12s · ↓ 98 tokens)`, or `Running…`: Claude has no "permission granted" hook
(between your answer and the tool's end, `PostToolUse`, none fires), so tutti,
having seen the dialog on the pane's screen, reads the screen until it goes. A
request you deny there stays blocked — Claude asks what to do instead — until
your next prompt.

### opencode

**opencode** holds its permission requests the same way through
[its plugin](agents.md#opencode-integration): with `approvals = "hold"`, a
`permission.asked` waits on tutti — `bash · rm -rf build` on its waiting row —
and an allow or a deny from the overlay or `pane approve` becomes opencode's
reply (a deny's reason reaches the agent). opencode gives a plugin no way to
keep its dialog back, so its own dialog shows meanwhile: answered there first,
the plugin drops tutti's hold, and the `permission.replied` naming that
request (its `requestID`) lets go of it alone; `esc`, a timeout or no attached
TUI leave it to that dialog.
