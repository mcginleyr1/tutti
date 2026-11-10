# CLI surface

Every `tutti` command with its flags in one listing, then the rules that apply
to all of them: how long a command waits for the daemon, `--json`, how the
session is chosen (`-s`), and `--host`.

Every TUI action is also a CLI verb over the daemon socket — if it isn't
scriptable, it isn't done:

```
tutti server start [--foreground] | stop  # start (printing its log path) or stop the session's daemon; the CLI starts it on first use anyway
tutti server restore [--discard]     # bring back the layout the last daemon saved (agents resume their conversations, other panes return as shells)
tutti project add <dir>              # open an existing directory as a project
tutti project list                   # the projects (top-level entries)
tutti project kill <id>              # remove a project and its panes from tutti; the directory stays
tutti workspace add <project> --name <name> [-r <rev>] [--dest <path>]  # jj workspace add: a checkout nested under the project (default <repo>-<name> beside it)
tutti workspace list                 # every project and workspace, with the PARENT each is nested under
tutti workspace kill <id> [--discard]  # remove from tutti; --discard also jj workspace forgets + deletes a checkout tutti created
tutti workspace merge <id> [--push]  # merge a workspace back into its project's trunk (main/master)
tutti workspace update <id>          # jj workspace update-stale (clears the sidebar's stale tag)
tutti workspace diff <id> [--stat]   # the workspace's jj diff against trunk (jj is required; --json for raw lines)
tutti tab new [--workspace <id>] | list [--workspace <id>] | select <id>
tutti run [--workspace <id> | --dir <path>] [--title <t>] [--sandbox <profile>] -- <cmd...>  # a fresh tab; prints the bare pane id
tutti ask <pane> <message> [--timeout <secs>]  # type the message + enter, wait for the turn, print the agent's reply; exit 3 if it blocks, 124 on timeout
tutti pane run [--tab <id>] [--ephemeral] [--sandbox <profile>] -- <cmd...>
tutti pane split <pane> right|down
tutti pane list | kill <pane> | focus <pane>   # list: each pane with its workspace's id; focus: the attached TUI jumps to the pane
tutti pane focus --direction left|right|up|down [--from <pane>] [--wrap]  # focus the pane beside one (default: this pane, else the TUI's) and print it; at the TUI's focused pane's left/right edge the TUI moves on (sidebar, panel, tab), exit 0; else `no pane <dir> of <id>`, exit 1
tutti pane resize --direction left|right|up|down [--amount <f>] [--from <pane>]  # move its split's border that way, as Alt+h/j/k/l (default step 0.05 of the split)
tutti pane attach <pane> [--inline [--history <rows>]]  # show one pane in this terminal, drawn by the terminal itself; prefix d detaches (never starts a daemon); --inline: on the primary screen, keeping its scrollback, after the pane's newest 1000 (--history) scrollback lines this terminal lacks
tutti pane rename <pane> <title>
tutti pane send <pane> --text <s> | --keys <s> [--expect <fp>]   # --expect: answer a blocked agent, refused if its prompt moved on; refused too once 64 sends wait on a program that stopped reading its input
tutti pane read <pane> [--lines N] [--unwrapped] [--since <cursor> | --last-output] | --fingerprint | --last-reply
                                     # --since: rows scrolled off since the cursor, then the screen; next cursor on stderr
                                     # --last-output: the last finished command's output; `exit: N` on stderr (needs shell-init)
                                     # --fingerprint: the prompt fingerprint --expect takes
                                     # --last-reply: the agent's final message of its last finished turn, from its hooks
tutti pane wait <pane>... [--until working|blocked|done|idle[,…]] [--next] [--timeout <secs>]  # block until each pane gets there; exit 124 on timeout
tutti pane approve <pane> [--id <n>] [--deny] [--message <s>]  # answer the pane's oldest held approval, or the one --id names (approvals = "hold")
tutti pane checkpoints <pane>        # the pane's per-turn checkpoints, newest first (n, age, stat, commit, label — the turn's prompt when known)
tutti pane checkpoint-diff <pane> [N] [--stat]  # one turn's diff (default: the newest)
tutti pane restore <pane> <N> [--force]         # restore the working copy to turn N (--force mid-turn)
tutti sandbox run <profile> [--name <name>] -- <cmd...>  # provision, kubectl exec, delete — what a --sandbox pane runs (the daemon names it)
tutti sandbox pull <pane> [--name <name>]  # fetch a sandbox's pushed tutti/<name> branch into a workspace under its project
tutti issue list [--source <name>]   # every configured tracker's open issues (--json for scripts)
tutti issue deploy <source> <id> [--agent claude] [--project <id>] [--here] [--sandbox <profile>]  # an agent on the issue, in its own workspace; prints the pane id, then writes back as the source opts in
tutti notes [--unread]               # the notification history, newest first
tutti notes read [--up-to <id>] [--pane <id>] | clear  # mark notes read (all, or narrowed) / empty the history
tutti hooks claude|codex|gemini|opencode [--raw]  # print the Claude Code, Codex or Gemini CLI hooks / the opencode plugin
tutti hooks claude|codex|gemini|opencode --install [--project] [--yes]  # install them (shows the change, asks first)
tutti hooks claude --tty [--raw]     # print Claude hooks that report over OSC 7501 to /dev/tty — for sandbox images
tutti skill [--raw]                  # print the tutti skill (a Claude Code SKILL.md)
tutti skill --install [--project] [--yes]  # install it into ~/.claude/skills/tutti/ (or ./.claude/skills/tutti/)
tutti agent-event claude|codex|gemini|opencode  # forward an agent hook event (JSON on stdin) — used by the hooks/plugin
tutti edit <path>[:<line>[:<col>]] [--workspace <id>]  # open in the workspace's editor pane (TUTTI_EDITOR) and focus it; returns at once
tutti shell-init zsh|bash|fish       # print the shell integration (OSC 7 cwd + OSC 133 marks) for your rc file
tutti watch                          # JSON-line snapshots of every pane's state, one per change (read-only; never starts a daemon)
tutti [attach]                       # bare `tutti` attaches; a fresh session asks where to start
tutti attach --view                  # watch only: navigation and detach work, nothing reaches a pane, no size vote, marks nothing seen or read (never starts a daemon; refuses a daemon from before views)
```

A verb gives up on a daemon that has not answered within 10 s (exit 1, `did
not answer within 10s`), or within 3 minutes for one the daemon runs jj for:
`workspace add`, `merge`, `update`, `diff` and `kill --discard`, `sandbox
pull`, `pane checkpoint-diff` and `restore`. `pane wait` and `ask` wait as
long as their `--timeout`, and `watch` and attach as long as the daemon lives.
The TUI gives up on what it waits to follow up — a project or workspace add,
a merge, a deploy, a sandbox pull, a last reply — at the same deadlines, and
says so on its status line.

`--json` on any verb emits the raw protocol response for scripting. A
project's id works anywhere a workspace id does (`run --workspace`, `tab new
--workspace`, `workspace diff`).

Every verb takes `-s <session>`. Inside a tutti pane it defaults to that pane's
own session (`TUTTI_SESSION`, which every pane exports beside its `TUTTI_PANE`
id), else to `tutti`, so an agent's commands reach the daemon running it rather
than auto-starting a stray one. For the same reason, bare `tutti` (or `tutti
attach`) refuses to attach a session from inside one of its own panes, which
would nest the session inside itself; pass `-s <other-session>` to attach a
different one.

Every verb also takes `--host <ssh destination>`: attach draws here from the
daemon there, and any other verb runs there whole — see [Attaching from another
machine](sessions.md#attaching-from-another-machine). Without `-s` the far side picks its
own default.
