# Deploying issues

How to turn an issue from GitHub, Linear or any other tracker into a running
agent: where issues come from, the issues panel (`Ctrl+B I`), what a deploy
creates, how a long first message is handed over, write-back to the tracker,
and the `tutti issue` commands.

## Sources

Point tutti at your issue trackers (`[[issues]]` in the
[configuration](configuration.md)) and an issue becomes an agent in a keystroke.
`Ctrl+B I` opens the **issues panel** over the current project
(` issues · <project> `): every source's open issues, fetched in the background
with your own tools, so the TUI never waits on a tracker and the daemon never
talks to one.

- **GitHub** goes through your `gh` (it is the auth), run in the project's
  directory so it picks the repo from the git remote — set `repo` where it
  can't, as in a jj repo without a colocated git checkout.
- **Linear** goes through its GraphQL API with your personal API key, read
  from `LINEAR_API_KEY` (or the variable `api_key_env` names). The key reaches
  `curl` on stdin — never a command line, never the config, never printed. By
  default the panel lists your assigned issues that are not completed,
  canceled or duplicate; `team` narrows that, and `filter` (a Linear
  `IssueFilter`) replaces it.
- **Any other tracker** — Jira, GitLab, a script of your own — works through a
  `command` that prints a JSON array of `{id, title, body?, url?, labels?}`,
  run in the project's directory.

## The issues panel and a deploy

A source still fetching shows `loading…`; one that fails shows why, dim red.
`/` filters (the go-to's fuzzy match, over source, id, title and labels),
`j`/`k` move, and `esc` closes. `enter` opens the run launcher armed for the
issue — ` deploy #123 · Fix the login ` — offering only the agents that take a
first message on their command line (listed below), `tab` cycling
sandboxes as usual and `esc` backing out to the panel. The footer
chip reads `ISSUES`.

Picking one creates a
**workspace for the issue** under the project, named after it
(`gh-123`, `eng-42`), starts the agent there with the issue as its first
message, jumps to it, and flashes `deployed #123 to claude`. That message
defaults to `Work on this issue and commit your changes when done.` followed
by the id, title, body and link; set `prompt` per source to change it (`{id}`,
`{title}`, `{body}`, `{url}`). `h` in the panel deploys into a fresh tab of the
project itself instead; a project that isn't a jj repo always does.

The agents that take a first message on their command line:

- Claude Code, Codex, Gemini CLI, OpenCode,
  Pi, Oh My Pi, Muse Code, Mistral Vibe, Ori;
- and, from their docs alone, untried: Goose,
  Qwen Code, Cursor CLI, Copilot CLI, Factory Droid, Augment Code and Amazon Q.

The interactive modes of Crush, Aider and Amp take none, and Prime Agent
documents none, so those four are not offered.

## Long first messages

A long message goes to a file instead of the command line. The file is
readable only by you (0600), in a directory private to you beside the
session's socket (`<session>.prompts`, 0700), not in `$TMPDIR`.

- **From 16 KiB, for an agent that reads a first message from a file:**
  the agent gets the file through its own option.
  - Ori: `--interactive --prompt-file <file>`.
  - Goose: `run --interactive --instructions <file>`.
  - Pi, Oh My Pi and Augment Code: `@<file>`, plus a short message pointing at it.
- **Any other agent, from 128 KiB (all that Linux's exec takes in one
  argument):** its first message names the file instead, for the agent to
  read as it reads any file. It may ask first: the file lies outside the
  project.

16 KiB is an eighth of Linux's 128 KiB limit, so an argument that size is
safe on Linux and macOS alike. Past it, a message is a document of some 300
lines, and putting it in a file also keeps it out of `ps` and out of the
saved layout.

The daemon deletes the file once the agent that was given it exits, whether
the agent quit or its pane was killed. A deploy that ends with no pane
running deletes its own file at once: its workspace or its run was refused,
or the daemon hung up or never answered. Stopping the daemon deletes any
that are left, and so does starting one after a daemon crashed. A
[restore](sessions.md#restoring-after-a-restart) never needs them, because it revives
each agent in its own conversation rather than on its first message. A
sandbox's agent can't read this machine's files, so a sandbox deploy sends
up to 128 KiB on the command line and refuses anything longer.

## The link to the issue

The agent's sidebar row and pane title carry the issue id (`claude #123`),
`tutti --json pane list` and [`tutti watch`](api.md#watching-state-from-other-tools)
carry its link (`issue: {source, id, title, url}`), and a
[restore](sessions.md#restoring-after-a-restart) that revives the agent keeps it.

## Write-back

**Write-back** is off until a source asks for it, piece by piece, and runs
once the agent is running, through the same `gh` or `curl` (key on stdin) the
fetch used:

- `comment = true` (GitHub or Linear) leaves a comment naming where the agent
  runs, and in a sandbox the branch it pushes to (`tutti/<name>`) — never a
  path on your machine:

  > tutti deployed claude on this issue: workspace `gh-123` of project `api`,
  > pane 7 of session `tutti`.
- On GitHub, `assign` adds an assignee (`@me` is you) and `label` a label
  (`gh issue edit --add-assignee/--add-label`).
- On Linear, `state` moves the issue to the workflow state of that name in its
  team; a name the team lacks fails, listing the ones it has.

The panel's deploy writes back in the background and flashes `wrote back to
#123`, or the tracker's refusal. A refused write-back leaves the agent running:
`tutti issue deploy` names the failure on stderr after the pane id and exits 1.
tutti never closes an issue: GitHub closes one itself when a commit saying
`Fixes #123` reaches the default branch, which a source's `prompt` can ask the
agent to write.

## From scripts

Scripts get the same. `tutti issue list [--source <name>]` fetches in the
current directory and prints a table of `SOURCE ID TITLE LABELS URL` (`--json`
for the issues); a failing source is named on stderr and exits 1 once the rest
are listed. `tutti issue deploy <source> <id> [--agent claude] [--project <id>]
[--here] [--sandbox <profile>]` deploys into the project open at the current
directory unless `--project` names one, and prints the new pane's id.
