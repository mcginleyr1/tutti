---
name: tutti
description: Coordinate sub-agents from inside tutti, the terminal multiplexer for coding agents - start Claude Code (or any agent CLI) in new panes or isolated jj workspaces, wait until they finish or need input, read their screens, answer their prompts safely, review their diffs and per-turn checkpoints, and merge their work. Use when the TUTTI_PANE environment variable is set and you want to spawn, parallelize, wait on, check on, answer, review, merge, or clean up other agents.
---

# Coordinating sub-agents with tutti

tutti runs coding agents in persistent terminal panes and knows what state each
one is in: **working**, **blocked** (waiting on input), **done** (finished,
not yet looked at), or **idle**. Its `tutti` CLI lets you, an agent in one of
those panes, start more agents and herd them.

## Are you inside tutti?

```sh
printenv TUTTI_PANE TUTTI_SESSION
```

`TUTTI_PANE` is **your own** pane's id; `TUTTI_SESSION` is the session, which
every `tutti` command you run targets by default. If nothing prints, you are
not in tutti and this skill does not apply.

The examples below use project `2`, workspace `5`, and sub-agent panes `7`,
`8`, `9`; use the ids your own commands print.

## Look around

```sh
tutti project list            # the projects: top-level directories, ID NAME DIR
tutti workspace list          # every project and workspace, with the PARENT it is nested under
tutti pane list               # every pane: ID TITLE AGENT STATE EXIT
```

Your project is the entry whose DIR holds your working directory. Projects and
workspaces share one id space: a project's id works anywhere a workspace id
does.

## Start a sub-agent

```sh
tutti run --title fix-tests -- claude "fix the failing tests in crates/api"
```

It prints the new pane's id, bare (`7`) — keep it: every later command takes
it. The agent opens in a fresh tab, so it never splits a pane someone is using,
and in your own workspace, beside you. To run it in another project or
workspace, name it with `--workspace`; to run it in another directory, name
that with `--dir` (tutti opens it as a project if it is not one yet):

```sh
tutti run --workspace 2 -- claude "fix the failing tests in crates/api"
tutti run --dir /srv/repos/web -- claude "upgrade the router dependency"
```

## Give each agent its own checkout

Agents editing the same tree trample each other. A **workspace** is an
isolated jj checkout (`jj workspace add`) nested under its project:

```sh
tutti workspace add 2 --name fix-tests     # prints: workspace 5 created
tutti run --workspace 5 -- claude "fix the failing tests in crates/api"
```

The checkout lands beside the project's repo (`<repo>-fix-tests`); `--dest`
puts it elsewhere and `-r` picks the revision it starts from. The project must
be a jj repo, and the name takes letters, digits, `-` and `_`.

```sh
tutti workspace add 2 --name spike -r main --dest /srv/repos/api-spike
```

## Wait for them

```sh
tutti pane wait 7 --until done,blocked --timeout 120
tutti pane wait 7 8 9 --until done,blocked --timeout 120
```

It blocks until each pane is done or blocked, then prints one state per pane,
in order. Exit 0 once every pane got there; exit 124 when the timeout hit
first (stderr names the pane still going); exit 1 when a pane was killed, or
its program exited without getting there. A timeout is not a failure: check on
them and wait again. Keep `--timeout` shorter than your own command timeout.

Always include `blocked`: an agent stuck on a permission prompt never becomes
done, so waiting for `done` alone would hang until the timeout. After you
answer a prompt, add `--next` so the wait skips the state the pane is already
in:

```sh
tutti pane wait 7 --next --until done,blocked --timeout 120
```

## Read their screens

```sh
tutti pane read 7 --lines 40               # the last 40 lines
tutti pane read 7 --lines 400 --unwrapped  # logical lines, soft wraps joined
tutti pane read 7 --since 0                # everything held, then `next cursor: N` on stderr
tutti pane read 7 --since 1834             # only what scrolled off since cursor 1834, then the screen
```

To follow a busy agent without rereading its whole history, keep the
`next cursor: N` each `--since` read prints on stderr and pass it to the next
one. The live screen comes back every time, because it is not final until it
scrolls.

## Answer a blocked agent

Answer through the guard, in this order:

```sh
tutti pane read 7 --fingerprint                       # prints e.g. 9f3a0c1d2e4b5a69
tutti pane read 7 --lines 12                          # the prompt that fingerprint covers
tutti pane send 7 --text 1 --expect 9f3a0c1d2e4b5a69  # answer it
```

With `--expect`, tutti writes only while the pane is still blocked on that
exact prompt; if it moved on, or someone answered first, the send is refused
and nothing is typed. Taking the fingerprint before reading keeps that safe.
`--text` types literally (`1`, `y`, a sentence); `--keys` sends key names:
`enter`, `esc`, `tab`, `space`, `backspace`, `up`, `down`, `left`, `right`,
`ctrl-c`. A permission prompt is your user's to grant: answer only what you are
sure they would approve, and ask them about the rest.

When your user runs tutti with `approvals = "hold"`, a Claude permission
request waits in tutti instead of Claude's own dialog, and `tutti pane list
--json` shows it as the pane's `approval` (tool and summary). Answer it with:

```sh
tutti pane approve 7                                   # allow the held tool call
tutti pane approve 7 --deny --message "not on main"    # refuse it, telling the agent why
```

The same rule applies: approve only what your user would.

To give a waiting agent a follow-up instruction, type it, then press enter:

```sh
tutti pane send 7 --text "also update the changelog"
tutti pane send 7 --keys enter
```

## Review their work

```sh
tutti workspace diff 5 --stat         # everything the workspace changed against trunk
tutti workspace diff 5                # the same, in full
tutti pane checkpoints 7              # the agent's turns, newest first: N AGE STAT COMMIT LABEL
tutti pane checkpoint-diff 7 --stat   # what its latest turn changed
tutti pane checkpoint-diff 7 3        # what turn 3 changed, in full
tutti pane restore 7 3                # put the working copy back the way turn 3 left it
```

tutti checkpoints a jj workspace at the end of every agent turn. A restore is
refused while the agent is mid-turn (`--force` overrides) and first saves the
state it replaces as a new checkpoint, so it can be undone the same way.

## Merge and clean up

```sh
tutti workspace merge 5               # rebase onto trunk and advance main (else master)
tutti workspace merge 5 --push        # ...and jj git push it, when there is a remote
tutti pane kill 7
tutti workspace kill 5 --discard      # jj workspace forget, and delete the checkout
```

A merge that would conflict is refused and undone; resolve it in the workspace
or hand it to your user. Merging lands work on trunk: show your user the diff
stat first, and merge only when they asked you to. `--discard` deletes the
checkout for good; without it, `workspace kill` only removes the workspace from
tutti.

## Machine-readable output

```sh
tutti --json pane list
tutti --json workspace list
tutti --json pane wait 7 --until done,blocked --timeout 120
```

`--json` prints each raw response as one JSON object per line. `pane list`
gives every pane's `id`, `title`, `agent`, `state` (`unknown`, `working`,
`blocked`, `done`, `idle`), `exited` code, and `conversation`: the agent's own
session id when its hooks report one (`claude --resume <id>` picks it up
again).

## Rules

- Never send keys to your own pane (`$TUTTI_PANE`), and never wait on it.
- Only kill panes and workspaces you created; the rest belong to your user or
  to other agents.
- Wait with `--until done,blocked`, and always pass `--timeout`.
- Answer prompts with `--expect`; ask your user about anything you would not
  approve yourself.
- Give agents that edit the same code separate workspaces.
- Report what each sub-agent changed before merging, and leave the merge to
  your user unless they handed it to you.
