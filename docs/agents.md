# Agents: the catalog, state detection and integrations

Which agents tutti recognises, how it works out what state each one is in
(blocked, working, done or idle), the ways any program can report its own
state, and the integrations that give exact state for Claude Code, Codex,
Gemini CLI, Antigravity CLI and opencode.

## The catalog

Tutti detects and drives the agents below; it does not install them. Each has
to be on your `PATH`. A missing agent shows dim in the launcher with its link,
so the picker doubles as the install list.

| Agent | Binary on `PATH` | Screen detection of blocked / working / done |
| --- | --- | --- |
| [Claude Code](https://claude.com/claude-code) | `claude` | tuned fully |
| [Codex CLI](https://github.com/openai/codex) | `codex` | tuned fully |
| [Gemini CLI](https://github.com/google-gemini/gemini-cli) | `gemini` | generic heuristics |
| [Antigravity CLI](https://antigravity.google) | `agy` | tuned fully |
| [OpenCode](https://opencode.ai) | `opencode` | generic heuristics |
| [Crush](https://github.com/charmbracelet/crush) | `crush` | generic heuristics |
| [Aider](https://aider.chat) | `aider` | generic heuristics |
| [Goose](https://github.com/block/goose) | `goose` | generic heuristics |
| [Pi](https://github.com/badlogic/pi-mono) | `pi` | generic heuristics |
| [Oh My Pi](https://github.com/can1357/oh-my-pi) | `omp` | generic heuristics |
| [Prime Agent](https://github.com/PrimeIntellect-ai/prime-agent) | `prime-agent` | generic heuristics |
| [Qwen Code](https://github.com/QwenLM/qwen-code) | `qwen` | generic heuristics |
| [Cursor CLI](https://cursor.com/cli) | `agent`, else `cursor-agent` ([below](#cursor-clis-two-names)) | generic heuristics |
| [Copilot CLI](https://github.com/github/copilot-cli) | `copilot` | generic heuristics |
| [Amp](https://ampcode.com) | `amp` | generic heuristics |
| [Factory Droid](https://factory.ai) | `droid` | generic heuristics |
| [Muse Code](https://dev.meta.ai/docs/muse-code) | `muse` | tuned in part: working and done |
| [Mistral Vibe](https://github.com/mistralai/mistral-vibe) | `vibe` | tuned in part: blocked and working |
| [Augment Code](https://www.augmentcode.com) | `auggie` | generic heuristics |
| [Amazon Q](https://github.com/aws/amazon-q-developer-cli) | `q` | generic heuristics |
| [Ori](https://openrouter.ai/labs/ori) | `ori` | tuned fully |

Precise blocked/working/done detection is tuned per agent — Claude Code,
Codex, Antigravity CLI and Ori fully, Muse Code (working/done) and Mistral Vibe
(blocked/working) in part; the rest start from generic heuristics — see
`crates/tutti-agents/src/registry.rs`, which is also where the binary names
come from.

## Agent state badges

Panes running a detected agent show live state on the pane border and in the
sidebar tree: **blocked** (red — needs your input; leading the waiting queue
and reddening the pane border), **working** (yellow, an animated braille
spinner), **done** (green — finished, not yet viewed; focusing it makes it
**idle**). The pane's border title carries the same `agent · state` suffix;
a plain shell shows just its name. Background panes ring the terminal bell when
they block or finish.

Detection covers the whole [catalog](#the-catalog) above, with screen patterns tuned per agent where they have been
checked live; adding an agent is one data-table row in
`crates/tutti-agents/src/registry.rs`. It matches the process's executable
name, or a node or bun program's script — Gemini CLI runs as `node …/gemini`,
Oh My Pi installed through bun as `bun …/omp`, and
Cursor CLI as `node …/cursor-agent/versions/<version>/index.js`, matched by
where that script lives, since its name says nothing.

### Cursor CLI's two names

Cursor CLI's installer puts two names on your `PATH`: `agent`, the one its
docs use now, and `cursor-agent`, the old one. The launcher and `issue
deploy` run `agent` when it is Cursor's own (a link into Cursor's install),
and `cursor-agent` otherwise, so an unrelated `agent` on your `PATH` never
stands in for Cursor. They run `agent` by the full path they found it at, so
a daemon started with a different `PATH` still runs that same `agent`. A
sandbox's image can't be looked into, so there it runs `cursor-agent`.

## Agent state sources

A pane's state can come from five sources, ranked highest first:

1. **hook** — the agent's own exact lifecycle signals: its hooks (`tutti hooks
   claude`, `tutti hooks codex`, `tutti hooks gemini`, `tutti hooks
   antigravity`, `tutti hooks opencode`) or a
   [herdr report](#herdr-compatible-reporting).
2. **program** — the pane's program reporting itself with OSC 7501.
3. **progress** — an OSC 9;4 progress report (ConEmu / Windows Terminal /
   Ghostty).
4. **screen** — the classifier reading a detected agent's screen. An agent
   not yet seen working since it started (a working match, a hook, a report;
   output alone is it drawing) is never done at its prompt, nor by its own
   idle or done report (Codex 0.166.0 reports idle as it starts, sometimes
   before tutti has detected it), having
   finished nothing: for its first 5 seconds it reads **unknown** — its first frames
   come before its first message's hook, and a `pane wait --until done`
   right after `tutti run` must wait for the real turn — then **idle**, as an
   agent given no work, or revived by a restore, settles.
5. **activity** — output flowing (a finished agent that prints again is working).

### Claims

Hook, program and progress are *claims*: a claim holds the state until its own
source ends it, and nothing ranked below may move it meanwhile — so a build that
reports `done` stays done while it keeps printing, and a hook-driven agent's own
progress bar never fights its hooks. Three exceptions: a screen **blocked**
match (a visible permission dialog) overrides a program/progress **working**
claim, which is likelier stale than right; a hook's **blocked** gives way
to a screen **working** match once the agent's own dialog it blocked for has
shown on the screen — answered there, which no hook reports, while tutti
holds none of the pane's requests; and Antigravity CLI's hooks say nothing of
its dialogs, so a screen **blocked** match overrides its hooks' **working**
(see [Antigravity CLI integration](#antigravity-cli-integration)). A hook claim holds until the pane's
child exits; a herdr report's claim also ends when its agent releases or clears
it, or when another agent is detected. A program/progress claim ends when that
source clears it, when the pane's child exits, or when its detected agent
changes — and when you type into the pane (not a guarded answer) while it holds
the pane done or idle, so a reporter that never sends its clear cannot shut out
the next report; the state stays until something moves it. `tutti --json pane
list` shows each pane's `source`.

### Reporting from any program: OSC 9;4 and OSC 7501

**Any pane may report**, not just a detected agent. A shell pane whose program
reports shows wherever agent state does — the sidebar row with its dot, the
waiting queue, the app-bar census, the pane border — labelled by its OSC 7501
`app`, else the pane title, for as long as the report has something to say (a
live claim, or a done/blocked you have not looked at yet).

**OSC 9;4** — `ESC ] 9 ; 4 ; <state> [; <percent>]` then ST or BEL. States 1
(normal), 3 (indeterminate) and 4 (paused) are **working**; 2 (error) is
**done** (tutti has no errored state; done puts it in the waiting queue); 0
clears — a pane it held working settles to **idle**. The percentage is not
shown.

```sh
printf '\e]9;4;1;40\e\\'   # working, 40%
printf '\e]9;4;0\e\\'      # clear
```

**OSC 7501** ([program status](https://tuios.dev/docs/program-status.md)) —
`ESC ] 7501 ; key=value:key=value… ST` (or BEL). `state` is required: `idle`,
`working`, `done`, `blocked`, `error` (shown as done) or `clear` (ends the
claim; working/blocked settle to idle, an unseen done stays). `app` labels the
pane; `msg` (base64) becomes a notification on a move to done/blocked, at most
every 30 s; `kind` (blocked only), `progress` (0–100) and `title` (base64) are
validated but not shown. A malformed report — missing/unknown state, `kind`
off blocked, out-of-range `progress`, an over-long field, or a `msg`/`title`
that is not base64 of control-free UTF-8 — is dropped whole. tutti tracks only
the root record (a report with an `id` is ignored) and does not answer the `?`
support probe.

```sh
printf '\e]7501;state=working:app=build\e\\'
printf '\e]7501;state=done:app=build:msg=%s\e\\' "$(printf 'Built' | base64)"
```

### Stall timer

**Stall timer.** A **working** pane not held by a hook that goes 30 s with no
output and no classifier working match drops to **unknown**, ending its claim —
a build killed mid-report never says clear, and a wedged agent never says done.

## herdr-compatible reporting

Every pane also carries [herdr](https://github.com/ogulcancelik/herdr)'s
environment — `HERDR_ENV=1`, `HERDR_SOCKET_PATH`, and `HERDR_PANE_ID` /
`HERDR_TAB_ID` / `HERDR_WORKSPACE_ID` in herdr's `w1:p3` shape — pointing at a
per-session socket beside tutti's own (`<session>.herdr.sock`, owner-only). An
agent that reports its lifecycle to herdr natively reports to tutti with no
install step; Crush does this out of the box.

- Reports rank as **hook**. `working` and `blocked` map straight over, and a
  blocked report's message raises a notification.
- `idle` reads as **done** after work the agent reported, else **idle**; a done
  you have not looked at stays done.
- The report's `agent` labels the pane, and its `agent_session_id` names the
  pane's `conversation`.
- Quitting releases the pane: the claim ends and the state settles.

tutti speaks herdr's v0.9.3 reporting protocol: `pane.report_agent`,
`pane.report_agent_session`, `pane.release_agent`, `pane.clear_agent_authority`,
`notification.show`, `pane.report_metadata` (acknowledged, not shown) and
`ping`. A notification lands on the pane its `pane_id` names; one naming none
lands on the pane whose process sent it — the socket's peer, or the pane it
descends from — and one sent from outside every pane is refused (`pane_id is
required`). `HERDR_*` variables inherited from
outside are dropped, so a tutti pane never reports to a herdr that tutti itself
runs inside.

## Claude Code integration

Every pane tutti spawns exports `TUTTI_PANE` (the pane id) and `TUTTI_SESSION`
(the session name) into its environment. A Claude Code instance running in such
a pane can report ground-truth lifecycle events back to tutti through Claude
Code's hooks, replacing the screen-heuristic guessing with exact signals.

(A daemon started inside an [rtty](https://github.com/mcginleyr1/rtty) pane
leaves that pane's `RTTY_PANE` and `RTTY_SOCKET` out of its own panes', where
they would be wrong; its `TERM_PROGRAM` goes through.)

Install the hook config (shows the before/after merge and asks first):

```
tutti hooks claude --install            # merge into ~/.claude/settings.json
tutti hooks claude --install --project  # …into ./.claude/settings.json instead
tutti hooks claude --install --yes      # skip the confirmation (scripts)
tutti hooks claude                      # or just print the snippet…
tutti hooks claude --raw                # …as bare JSON, for piping
```

`--install` preserves everything already in the file (foreign hooks included),
is idempotent, backs the old file up to `settings.json.bak`, and writes
atomically. The printed snippet already includes the `"hooks"` key — merged by
hand it goes at the settings.json **top level**. From the TUI, `Ctrl+B i`
installs the same thing through the setup panel. Installed before prompt labels
or approvals existed? Re-run `tutti hooks claude --install`: it adds just the
missing `UserPromptSubmit`, `SessionStart`, `PermissionRequest`, `StopFailure`
and `PostToolUseFailure` entries and leaves the rest alone.

Each wired event runs `tutti agent-event claude`, which reads the hook JSON on
stdin, maps it, and forwards it to the pane's daemon:

- a tool use keeps the agent **working**; a submitted prompt
  (**UserPromptSubmit**) starts a turn, and its first line labels that turn's
  checkpoint in the [turns](workspaces.md#turns-review-and-undo-one-agent-turn) list —
  unless it carries the running turn's `prompt_id` (you typed more into that
  turn), and a prompt arriving while a turn is still open closes that one as
  `interrupted` (Esc sends no Stop); **Stop** marks it **done**, handing tutti
  the turn's final message (`last_assistant_message`), which `tutti pane read
  --last-reply` and `tutti ask` print, and **StopFailure** — an API error
  ended the turn — marks it done with no reply;
- a **PermissionRequest** marks it **blocked** the moment Claude is about to
  ask, naming the tool and its input (`Bash · rm -rf build`); with `approvals =
  "hold"` the request also waits for an answer from tutti, beside Claude's
  dialog (see [Held approvals](answering.md#held-approvals)); an agent's turn end (Stop
  or StopFailure for the main agent, SubagentStop for a subagent) lets go of
  the requests that agent still holds;
- a **Notification** that waits on you — a permission prompt, an MCP dialog, a
  background agent's question, an Enter after a usage limit — marks it
  **blocked** too, as does one from an older Claude that names no type; idle
  reminders (Stop already marked the turn done), auth, MCP results, background
  agents finishing, usage-limit reports, a mod's notice and any type tutti
  does not know yet don't;
- **SessionStart** and every event's `session_id` record the conversation the
  pane holds — `conversation` in `tutti --json pane list`, resumable with
  `claude --resume <id>` and revived by a
  [restore](sessions.md#restoring-after-a-restart); it follows `/clear` and `--resume`,
  and is dropped when the agent leaves the pane;
- a subagent spawn (the **Agent** tool, **Task** before Claude Code 2.1) adds a
  dim indented sub-row under the agent in the sidebar — a shared spinner while
  it runs, a `·` once it finishes — and **SubagentStop** completes it;
  finished sub-rows clear when the turn ends.

`agent-event` is deliberately fail-safe: outside a tutti pane (no `TUTTI_PANE`),
on a malformed or irrelevant event, when it cannot reach the daemon, or when
the daemon has not answered within 2 s, it exits 0 with at most a note on
stderr — a hook never breaks or stalls a Claude session, inside tutti or out (a
held approval waits out its own `approval_timeout_secs` instead).
It never writes to stdout, which Claude Code feeds to the model on a prompt or
a session start — except a held approval's decision, which Claude reads from
it. If you ever uninstall tutti while the hooks remain, Claude Code will
surface a non-blocking "command not found" warning on every tool call — remove
the entries (restore `settings.json.bak` or delete them) when you go.

Hooks are ground truth: once a pane reports one, its state is the hook's alone
— tutti stops classifying that pane's screen (but for a dialog the hook
blocked it for, answered in the pane), and the agent's own OSC 9;4 / 7501
reports are ignored (see [Agent state sources](#agent-state-sources)).
Agent **detection** (which kind of agent runs there) is unaffected, so the badge
still appears. Display only — tutti never manages a foreign agent's subagents.

## Codex integration

Codex's hooks speak Claude Code's schema, so Codex reports to tutti the same
way. Install them (shows the before/after merge and asks first):

```
tutti hooks codex --install            # merge into ~/.codex/hooks.json ($CODEX_HOME honoured)
tutti hooks codex --install --project  # …into ./.codex/hooks.json instead
tutti hooks codex --install --yes      # skip the confirmation (scripts)
tutti hooks codex                      # or just print the hooks.json…
tutti hooks codex --raw                # …as bare JSON, for piping
```

`--install` keeps everything else in the file, is idempotent, backs the old
file up to `hooks.json.bak`, and writes atomically; `Ctrl+B i` installs the
same through the setup panel. **Then trust them in Codex**: it skips a new or
changed hook until you do — open `/hooks` in Codex (it also offers a review
when it starts) and trust tutti's `tutti agent-event codex` entries. A project
install also needs the project trusted, which Codex asks the first time it
opens there.

Each wired event runs `tutti agent-event codex` (verified live against
codex-cli 0.166.0):

- a tool use keeps the agent **working**; a submitted prompt
  (**UserPromptSubmit**) starts a turn and labels its checkpoint; **Stop** —
  or **Interrupt**, as an interrupted turn sends no Stop — marks it **done**,
  Stop with the turn's final message (`last_assistant_message`) for `pane read
  --last-reply` and `tutti ask`, Interrupt with none;
- a **PermissionRequest** marks it **blocked**, naming the command (`Bash ·
  touch hello.txt`); with `approvals = "hold"` it is held as Claude's is, the
  answer going back as the decision object Codex's hooks reference documents
  (see [Held approvals](answering.md#held-approvals));
- **SessionStart** and every event's `session_id` record the conversation —
  resumable with `codex resume --no-daemon <id>` and revived by a
  [restore](sessions.md#restoring-after-a-restart);
- **SubagentStart**/**SubagentStop** add and complete a subagent sub-row.

**Run codex with `--no-daemon` inside tutti** — the launcher, `issue deploy`
and a restore do. By default Codex runs its sessions in a shared background
app-server, which runs every session's hooks with the environment of
whichever codex started it: their `TUTTI_PANE` names that first pane, and
another pane's events would land on it. `agent-event codex` therefore drops an
event unless the session's tutti-server is among its ancestors, with a note on
stderr. So a `codex` you type into a pane yourself, without `--no-daemon`,
runs in that shared app-server and its hooks never reach tutti: the pane falls
back to Codex's own OSC 7501 reports (0.166.0 sends working, blocked and done)
and screen detection, its turns unlabelled and its conversation unrecorded.
Type `codex --no-daemon` (passing Codex a `-c` override also keeps it
embedded). Otherwise `agent-event codex` is fail-safe as Claude's is.

## Gemini CLI integration

Gemini CLI reports through hooks in its settings.json. Install them (shows the
before/after merge and asks first):

```
tutti hooks gemini --install            # merge into ~/.gemini/settings.json ($GEMINI_CLI_HOME honoured)
tutti hooks gemini --install --project  # …into ./.gemini/settings.json instead
tutti hooks gemini --install --yes      # skip the confirmation (scripts)
tutti hooks gemini                      # or just print the snippet…
tutti hooks gemini --raw                # …as bare JSON, for piping
```

`--install` keeps the rest of the settings, is idempotent, backs the old file
up to `settings.json.bak`, and writes atomically; `Ctrl+B i` installs the same
through the setup panel. Each hook is named `tutti`, so `/hooks disable tutti`
in Gemini turns them off. A project's hooks run once Gemini trusts the folder
(it asks), with a one-time warning naming them. Each wired event runs `tutti
agent-event gemini`:

- **BeforeAgent** (a submitted prompt) starts a turn and labels its
  checkpoint; **BeforeTool**/**AfterTool** keep it **working**; **AfterAgent**
  marks it **done**, its `prompt_response` the turn's reply for `pane read
  --last-reply` and `tutti ask` — every model response of the turn joined,
  narration before a tool call included (per Gemini CLI 0.66.0's source; not
  yet run live);
- a **Notification** of type `ToolPermission` — a tool permission prompt —
  marks it **blocked** with its message, and the tool's run puts it back to
  working; any other type is a report and changes nothing;
- **SessionStart** and every event's `session_id` record the conversation,
  resumable with `gemini --resume <id>` and revived by a
  [restore](sessions.md#restoring-after-a-restart).

Gemini CLI runs as `node …/gemini`; tutti detects it by that script. Gemini's
Notification hook only observes a permission prompt — it cannot answer one —
so `approvals = "hold"` does not apply. `agent-event gemini` is fail-safe as
Claude's is.

## Antigravity CLI integration

Antigravity CLI (`agy`) reports through a hook in its hooks.json, which keys
each hook by a name: tutti's is the one named `tutti`. Install it (shows the
hook and asks first):

```
tutti hooks antigravity --install            # into ~/.gemini/config/hooks.json
tutti hooks antigravity --install --project  # …into ./.agents/hooks.json instead
tutti hooks antigravity --install --yes      # skip the confirmation (scripts)
tutti hooks antigravity                      # or just print the hook…
tutti hooks antigravity --raw                # …as bare JSON, for piping
```

`--install` keeps the file's other hooks, replaces an older hook named
`tutti`, is idempotent, backs the old file up to `hooks.json.bak`, and writes
atomically; `Ctrl+B i` installs the same through the setup panel. A project's
hooks run once Antigravity trusts the folder (it asks). Each wired event runs
`tutti agent-event antigravity` (verified live against Antigravity CLI 1.5.6,
from a project's `.agents/hooks.json`; the user file's place is from its
docs):

- **PreInvocation** comes before each model call: a turn's first starts the
  turn and labels its checkpoint with the prompt, the rest keep it
  **working**, as **PreToolUse**/**PostToolUse** do;
- **Stop** marks it **done**. It carries no message, so the turn's reply for
  `pane read --last-reply` and `tutti ask` is read from the conversation's
  transcript: its last line, when that is the model's final message;
- **SessionStart** and every event's `conversationId` record the
  conversation, resumable with `agy --conversation <id>` and revived by a
  [restore](sessions.md#restoring-after-a-restart). SessionStart comes with a
  new conversation's first prompt, so a fresh pane has reported nothing yet:
  `tutti ask` takes one on the strength of the installed hook, as it does a
  fresh Codex.

**Its dialogs come from the screen.** No hook fires when Antigravity asks
for permission (PreToolUse comes before the dialog), nor when you cancel the
dialog with Esc, which ends the turn with no Stop. So while its hooks hold a
pane working, tutti still reads that pane's screen: a dialog there marks it
**blocked**, the approved tool running sets it **working** again, and its
idle prompt after a cancelled dialog marks it **done**, with no reply. Its
hooks only observe a tool call from tutti's side, so `approvals = "hold"`
does not apply. `agent-event antigravity` is fail-safe as Claude's is, and
writes nothing to stdout.

## opencode integration

opencode reports to tutti through a plugin. Install it (shows the plugin and
asks first):

```
tutti hooks opencode --install            # into ~/.config/opencode/plugins/tutti.js ($XDG_CONFIG_HOME honoured)
tutti hooks opencode --install --project  # …into ./.opencode/plugins/tutti.js instead
tutti hooks opencode --install --yes      # skip the confirmation (scripts)
tutti hooks opencode                      # or just print the plugin (JavaScript)…
tutti hooks opencode --raw                # …without the stderr instructions, for piping
```

`--install` is idempotent, backs a previous tutti plugin up to `tutti.js.bak`,
writes atomically, and refuses to overwrite a `tutti.js` that is not tutti's.
From the TUI, `Ctrl+B i` installs the same plugin through the setup panel.
Inside a tutti pane the plugin pipes each relevant opencode event to `tutti
agent-event opencode` (outside one it does nothing):

- `session.status` **busy**/**retry** keeps the agent **working**; **idle**
  (and `session.idle`, `session.error`) marks it **done**. On an idle the
  plugin also reads the session's last message through opencode's own client
  and, when it is the assistant's, sends its text as the turn's reply for
  `pane read --last-reply` and `tutti ask` (a plugin installed before that
  sends none: re-run `--install`);
- `permission.asked` and `question.asked` mark it **blocked**; the matching
  `permission.replied` / `question.replied` / `question.rejected` puts it back
  to **working** — a `permission.replied` only once no other request tutti
  holds there still waits. With `approvals = "hold"` a permission request
  also waits on tutti for an answer, which the plugin posts as opencode's
  reply (see
  [Held approvals](answering.md#held-approvals));
- a subagent's child session never moves the pane on its own busy/idle (its
  permission prompts still block it);
- the root session's id (from its `session.*` events — never a subagent's) is
  recorded as the pane's `conversation`, resumable with `opencode --session
  <id>` and revived by a [restore](sessions.md#restoring-after-a-restart).

`agent-event opencode` is fail-safe in the same way as for Claude: it exits 0
outside a tutti pane, on an irrelevant event, and when the daemon is
unreachable or has not answered within 2 s (a held request waits out its own
`approval_timeout_secs`). Its stdout carries only a held request's reply. Remove `tutti.js` from the plugins
directory if you uninstall tutti.
