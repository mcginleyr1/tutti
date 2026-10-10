use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::{AgentKind, Direction, Layout};
use crate::ids::{PaneId, TabId, WorkspaceId};
use crate::state::{AgentState, StateSource};

/// The wire-protocol revision. BUMP THIS on ANY change to the Request /
/// Response / Event shapes (including additive fields): the daemon reports it
/// on attach and mismatched clients tell the user to restart the daemon —
/// binaries upgrade on install, running daemons do not.
pub const WIRE_REV: u32 = 8;

/// The revset every workspace-delta surface diffs against: the fork point of
/// the trunk bookmark (local `main`, else `master`, else `trunk`; `root()` when
/// none exists — `latest` picks the bookmark over root because root is oldest)
/// and the working copy. Answers "what has this workspace changed that hasn't
/// landed on trunk" — a bare `jj diff` only shows `@`, which is nearly always
/// empty because agents commit as they go. `fork_point` keeps a workspace that
/// fell behind an advanced trunk from showing trunk's progress as reverse
/// noise. Shared by the server's stat/diff probes and the client's pager panes
/// so every surface agrees on what "the diff" is.
pub const DIFF_RANGE: &str =
    "fork_point(latest(present(main) | present(master) | present(trunk) | root()) | @)..@";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    WorkspaceNew {
        dir: PathBuf,
    },
    WorkspaceList,
    /// Bring back the layout the previous daemon saved — the offer an attach
    /// reports as `Response::Attached::restorable` — answered with
    /// `Response::SessionRestored`. Agent panes holding a conversation their
    /// tool can resume come back resuming it; every other pane comes back a
    /// login shell, since re-running a saved command could re-issue an agent's
    /// prompt. `discard` drops the offer instead, answering with zeros.
    SessionRestore {
        #[serde(default)]
        discard: bool,
    },
    WorkspaceKill {
        id: WorkspaceId,
        /// Discard a *forked* workspace's checkout on kill: `jj workspace forget`
        /// it at its origin and delete its directory. Rejected for a workspace
        /// tutti did not create — tutti never removes a checkout it did not fork.
        #[serde(default)]
        discard: bool,
    },
    /// Fork a jj workspace: `jj workspace add` a checkout named `name` (optionally
    /// at `revision`), then mount it as a tutti workspace with a shell pane. The
    /// source workspace must be under a `.jj` repo. `dest`, when given, is the
    /// absolute checkout directory (its parent already canonicalized by the
    /// client, its leaf not yet existing); when absent the server places a sibling
    /// `<repo-parent>/<repo>-<name>`. Answered with `Response::WorkspaceCreated`.
    WorkspaceFork {
        id: WorkspaceId,
        name: String,
        revision: Option<String>,
        #[serde(default)]
        dest: Option<PathBuf>,
    },
    /// Update a stale forked workspace by running `jj workspace update-stale` in
    /// its directory, then refreshing. The manual fix for the sidebar's `stale`
    /// tag. Answered with `Response::Ok` or `Response::Error`.
    WorkspaceUpdate {
        id: WorkspaceId,
    },
    /// Merge a child (jj-workspace) workspace's work back into its origin's trunk
    /// bookmark (`main`, else `master`) and, when `push`, `jj git push` it if the
    /// origin has a remote. Only valid for a workspace tutti forked. Answered with
    /// `Response::Merged` on success or `Response::Error` (unmergeable, would
    /// conflict — the merge is undone — or no trunk bookmark).
    WorkspaceMerge {
        id: WorkspaceId,
        push: bool,
    },
    TabNew {
        workspace: Option<WorkspaceId>,
    },
    TabList {
        workspace: Option<WorkspaceId>,
    },
    TabSelect {
        id: TabId,
    },
    /// The orchestrator's one call: spawn `cmd` in a fresh tab and answer
    /// `Response::Ran`. The workspace is `workspace`, else the one open at
    /// `dir` (mounted like `WorkspaceNew` when none is), else `from`'s, else
    /// the current one; naming both `workspace` and `dir` is an error. A fresh
    /// tab because an orchestrated agent must never split a tab someone is
    /// using — and the session's current tab stays put, so an attached client
    /// is not yanked over to it. `sandbox` works as in `PaneRun`; `title`
    /// names the pane.
    Run {
        #[serde(default)]
        workspace: Option<WorkspaceId>,
        #[serde(default)]
        dir: Option<PathBuf>,
        cmd: Vec<String>,
        #[serde(default)]
        sandbox: Option<String>,
        #[serde(default)]
        title: Option<String>,
        /// The calling pane (the CLI's `TUTTI_PANE`), so an agent's run lands
        /// beside it rather than wherever the human is looking. `None` from
        /// outside the session's panes and from older clients.
        #[serde(default)]
        from: Option<PaneId>,
    },
    PaneRun {
        tab: Option<TabId>,
        cmd: Vec<String>,
        /// An ephemeral pane is removed entirely when its child exits (no exited
        /// corpse row), rather than kept readable. Used for throwaway views like
        /// the jj diff pane.
        #[serde(default)]
        ephemeral: bool,
        /// Run `cmd` in a k8s sandbox from this client-config profile instead
        /// of locally: the pane runs `tutti sandbox run <profile> -- <cmd>`,
        /// which provisions the sandbox and `kubectl exec`s `cmd` in it. The
        /// pane declares the agent `cmd` names, since its local process tree
        /// only ever shows `tutti` and `kubectl`.
        #[serde(default)]
        sandbox: Option<String>,
    },
    PaneSplit {
        pane: PaneId,
        direction: Direction,
    },
    PaneList,
    PaneKill {
        pane: PaneId,
    },
    PaneRename {
        pane: PaneId,
        title: String,
    },
    PaneSend {
        pane: PaneId,
        text: Option<String>,
        keys: Option<String>,
        /// Makes this a *guarded* send — an answer to a blocked agent: the
        /// server writes only while `pane` is `Blocked`, its current
        /// `prompt_fingerprint` still equals this one, and it has redrawn since
        /// the last guarded answer; otherwise it answers Error and writes
        /// nothing. So a keypress never lands on a prompt other than the one
        /// its sender looked at, and of two answers to one prompt the first
        /// wins. `None` is the plain unguarded send.
        #[serde(default)]
        expect: Option<String>,
    },
    /// The pane's current `prompt_fingerprint`, answered with
    /// `Response::Fingerprint` — what a script passes back as a guarded
    /// send's `expect`.
    PaneFingerprint {
        pane: PaneId,
    },
    /// Block until `pane` is in one of `until`'s states, answered with
    /// `Response::Waited`: at once when it already is, or — with `next` — only
    /// on a fresh transition into one after the request arrives (a human
    /// focusing a Done pane is never a fresh finish). The child exiting ends
    /// the wait, `Waited` if its final state matches and Error otherwise —
    /// unless it waits for `Idle`, which a look can still bring. A killed pane
    /// is an Error; `timeout_ms` running out answers `Waited` with `timed_out`.
    /// Served from a server-side subscription, never by polling.
    PaneWait {
        pane: PaneId,
        until: Vec<WaitFor>,
        #[serde(default)]
        next: bool,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    PaneRead {
        pane: PaneId,
        lines: Option<usize>,
        unwrapped: bool,
        /// Tail from this cursor (a previous `Response::Tail`'s): the rows
        /// committed to scrollback since, then the live screen, answered with
        /// `Response::Tail`. `None` reads everything as `Response::Content`.
        #[serde(default)]
        since: Option<u64>,
    },
    /// Resize a pane's pty and grid to the attached client's rendered size.
    PaneResize {
        pane: PaneId,
        rows: u16,
        cols: u16,
    },
    /// View a pane's scrollback: `offset` rows back from the live screen.
    /// `offset == 0` resumes the live view.
    PaneScroll {
        pane: PaneId,
        offset: usize,
    },
    /// Mark `pane` the active pane. The server records it and applies a
    /// `Focused` state event, so a `Done` pane becomes `Idle` once looked at.
    PaneFocus {
        pane: PaneId,
    },
    /// Serve a workspace's jj diff, answered with `Response::Content`. `stat`
    /// requests the `--stat` summary instead of the full diff. jj is the required
    /// VCS: a workspace whose directory is not under a `.jj` repo answers Error.
    WorkspaceDiff {
        id: WorkspaceId,
        #[serde(default)]
        stat: bool,
    },
    /// Nudge the ratio of the nearest split enclosing `pane` whose axis is
    /// `direction`, by `delta` (clamped server-side). `h`/`l` drive a
    /// `Horizontal` split, `j`/`k` a `Vertical` one; a positive `delta` grows
    /// the first (left/top) child.
    PaneResizeSplit {
        pane: PaneId,
        direction: Direction,
        delta: f32,
    },
    /// Set the ratio of the split separating `a` from `b` — their lowest
    /// common ancestor in one tab's layout, `a` in its first (left/top)
    /// subtree and `b` in its second — clamped as `PaneResizeSplit` is. A
    /// split-border drag: the client names the split by a leaf from each side.
    /// Error when the panes share no tab or sit in the other order.
    PaneSplitRatio {
        a: PaneId,
        b: PaneId,
        ratio: f32,
    },
    /// A ground-truth agent-lifecycle signal for `pane`, forwarded from a Claude
    /// Code hook by `tutti agent-event`. Drives exact state + the subagent list,
    /// replacing screen-heuristic guessing for that pane.
    AgentEvent {
        pane: PaneId,
        event: AgentHookEvent,
        /// The agent's own id for its conversation when the payload carried
        /// one — Claude's `session_id`, opencode's root `sessionID` — which
        /// the pane remembers (`PaneInfo::conversation`). `None` from older
        /// clients and payloads without one.
        #[serde(default)]
        conversation: Option<String>,
    },
    /// Hold a Claude permission request on `pane` for an answer from tutti —
    /// what `tutti agent-event` sends under `approvals = "hold"` in place of
    /// the request's `AgentEvent`, so the pane blocks just the same. `tool`
    /// is what would run, `summary` its input on one line. Answered with
    /// `Response::Approval` once an `ApprovalAnswer` names it, or at once with
    /// `Pass` — Claude's own dialog — when no client is attached to answer,
    /// the pane is unknown, or it already holds one. The approval lives as
    /// long as this connection: hanging up drops it.
    ApprovalHold {
        pane: PaneId,
        #[serde(default)]
        conversation: Option<String>,
        tool: String,
        summary: String,
    },
    /// Answer `pane`'s held approval: `id` names the one the sender looked at
    /// (`PaneInfo::approval`), `None` whichever the pane holds. Error when the
    /// pane holds no such approval — it was answered, or its hook gave up.
    ApprovalAnswer {
        pane: PaneId,
        #[serde(default)]
        id: Option<u64>,
        decision: ApprovalDecision,
    },
    /// List `pane`'s per-turn checkpoints, oldest first. Answered with
    /// `Response::Checkpoints` (empty for a pane outside a jj workspace).
    PaneCheckpoints {
        pane: PaneId,
    },
    /// Serve one checkpoint's turn diff — from the state before it to its
    /// commit — answered with `Response::Content`. `n` defaults to the newest;
    /// `stat` asks for the `--stat` summary instead of the full diff.
    PaneCheckpointDiff {
        pane: PaneId,
        #[serde(default)]
        n: Option<u64>,
        #[serde(default)]
        stat: bool,
    },
    /// Restore the pane's workspace working copy to checkpoint `n` (`jj restore
    /// --from`), first saving the current state as a `before restore`
    /// checkpoint. Refused while the pane is working or blocked unless `force`,
    /// and in a stale workspace. Answered with `Response::Restored`.
    PaneRestore {
        pane: PaneId,
        n: u64,
        #[serde(default)]
        force: bool,
    },
    /// Pull a sandbox pane's work back: in its workspace's repo, `jj git
    /// fetch` the branch its sandbox pushes to (`sandbox_branch` of the pane's
    /// `sandbox_name`) from the remote it cloned (`pick_remote`), track it,
    /// and check it out as a new workspace like `WorkspaceFork` does — named
    /// `name`, else after the sandbox. Answered with `Response::WorkspaceCreated`;
    /// Error for a pane that is not a sandbox's, a repo with no remote, or a
    /// branch the sandbox has not pushed.
    SandboxPull {
        pane: PaneId,
        #[serde(default)]
        name: Option<String>,
    },
    /// Join the attached-client set. `terminal` names the terminal app the
    /// client runs in — macOS's `__CFBundleIdentifier`, e.g.
    /// `com.mitchellh.ghostty` — which watchers are told so they can raise it.
    /// `None` off macOS and from older clients.
    Attach {
        #[serde(default)]
        terminal: Option<String>,
    },
    Detach,
    /// Subscribe this connection to `Event::Snapshot`s: one at once, then one
    /// whenever anything a snapshot carries changes. Read-only, and not an
    /// attached client: a watcher gets no pane frames and never moves sizes,
    /// focus, or a `Done` pane's seen state, so it sits beside an attached TUI
    /// without touching it.
    Watch,
}

/// A single agent-lifecycle signal mapped from a Claude Code hook event. Kept a
/// small, tool-agnostic vocabulary so other agents' hooks can map onto it too.
/// The parser that produces these tolerates unknown JSON fields (Claude Code's
/// hook schema evolves), so this enum only names what tutti actually reacts to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentHookEvent {
    /// A subagent/Task was launched: shown as an indented sidebar sub-row.
    SubagentStarted { id: String, desc: String },
    /// A subagent finished: its sub-row flips to "just-finished".
    SubagentStopped { id: String },
    /// Any tool use — a heartbeat that keeps the pane `Working`; `detail` names
    /// the tool when known.
    Activity {
        #[serde(default)]
        detail: Option<String>,
    },
    /// The agent is waiting on the user (a permission or idle prompt), carrying
    /// the notification text when the hook provided one.
    Blocked {
        #[serde(default)]
        message: Option<String>,
    },
    /// The agent finished responding.
    Done,
    /// The agent opened a conversation or switched to another (launch,
    /// resume, `/clear`). No state change: it names the conversation before
    /// the first tool use does, and follows the switch.
    SessionStarted,
    /// A prompt was submitted — typed, or one the agent started on its own (a
    /// scheduled task, a background subagent reporting back) — so a turn
    /// begins (`Working`). `text` is its first line, trimmed and capped, which
    /// the checkpoint closing the turn lists as; `None` when that line is empty
    /// or tool-injected content (a paste marker, a task notification).
    Prompt {
        #[serde(default)]
        text: Option<String>,
    },
}

/// The answer to a held approval: let the tool call run, refuse it — with
/// the reason Claude is told, else a stock one — or pass it on to Claude's
/// own permission dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny {
        #[serde(default)]
        message: Option<String>,
    },
    Pass,
}

/// A state `PaneWait` waits for. `Done` reads "finished, seen or not", so it
/// matches `Idle` too and a wait never hangs on a pane someone already looked
/// at; `Idle` alone is "finished and seen".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitFor {
    Working,
    Blocked,
    Done,
    Idle,
}

impl WaitFor {
    /// Whether a pane in `state` satisfies this condition.
    pub fn matches(self, state: AgentState) -> bool {
        match self {
            WaitFor::Working => state == AgentState::Working,
            WaitFor::Blocked => state == AgentState::Blocked,
            WaitFor::Done => matches!(state, AgentState::Done | AgentState::Idle),
            WaitFor::Idle => state == AgentState::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub dir: PathBuf,
    /// The entry this one is nested under, resolved as `WorkspaceView::parent`
    /// is: `None` for a top-level project — what `project list` keeps.
    /// Defaults `None` for older servers.
    #[serde(default)]
    pub parent: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: TabId,
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub title: String,
    /// The detected agent kind. A pane with none that reports its own status
    /// (OSC 7501 / 9;4) carries its label here instead — the 7501 `app`, else
    /// its title — while that report has something to say, so a build in a
    /// shell pane shows wherever agent state does.
    pub agent: Option<AgentKind>,
    pub state: AgentState,
    pub exited: Option<i32>,
    /// Subagents this pane's agent has spawned, newest last, reported by hooks.
    /// A finished subagent is kept (`running == false`) until the agent's next
    /// `Done` so it visibly completes. Defaults empty for older servers and
    /// non-hook panes.
    #[serde(default)]
    pub subagents: Vec<SubagentInfo>,
    /// The pane's per-turn checkpoints, oldest first (the newest 50 kept).
    /// Empty outside a jj workspace, before the first turn, and from older
    /// servers.
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
    /// The permission request the pane's agent holds for an answer from
    /// tutti (`approvals = "hold"`), until it is answered or its hook gives
    /// up. `None` without one and from older servers.
    #[serde(default)]
    pub approval: Option<PendingApproval>,
    /// What set `state`: a hook, the pane's own program report (OSC 7501 /
    /// 9;4), the screen classifier, or output activity. `None` until something
    /// has, and after a claim ends. Defaults `None` for older servers.
    #[serde(default)]
    pub source: Option<StateSource>,
    /// The sandbox profile the pane runs in (`PaneRun::sandbox`); `None` for a
    /// local pane and from older servers.
    #[serde(default)]
    pub sandbox: Option<String>,
    /// The conversation the pane's agent holds, by the agent's own id for it
    /// (Claude's `session_id`, opencode's root `sessionID`) from the latest
    /// hook event naming one — what resuming it takes. Dropped when the agent
    /// leaves the pane. `None` without hooks and from older servers.
    #[serde(default)]
    pub conversation: Option<String>,
    /// The name the daemon gave the pane's sandbox: the k8s object's, and
    /// under `tutti/` the branch the sandbox pushes its work to, which
    /// `SandboxPull` fetches. `None` for a local pane and from older servers.
    #[serde(default)]
    pub sandbox_name: Option<String>,
}

/// One agent turn's checkpoint: the commit id of the workspace's `@` right
/// after the turn ended. jj keeps a superseded `@` commit addressable by id, so
/// the turn can be diffed and restored long after later snapshots replace it.
/// `n` counts per pane and is never reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub n: u64,
    pub commit: String,
    /// The state the turn started from — the previous checkpoint, or whatever a
    /// restore left — so the turn's diff is `from..commit`. `None` when there
    /// was nothing earlier to diff against (the pane's first recorded state).
    pub from: Option<String>,
    /// Unix seconds when it was recorded.
    pub time: u64,
    /// What closed the turn: the state it ended in (`done`, `blocked`), `start`
    /// for where the agent began, or `before restore`.
    pub label: String,
    /// The first line of the prompt that started the turn, when a hook
    /// reported one — what the turn lists as, over `label`. `None` for
    /// `start` and `before restore`, without hooks, and from older servers.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The turn's stat over `from..commit`, computed once at record time.
    pub files: usize,
    pub insertions: u64,
    pub deletions: u64,
}

/// A held permission request: the tool it would run, that tool's input on
/// one line, and the id an answer names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub id: u64,
    pub tool: String,
    pub summary: String,
}

/// One pane in a watcher's `Event::Snapshot`, flat: where it lives, by name,
/// and its agent state — the shape `tutti watch` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchPane {
    pub id: PaneId,
    /// The workspace (project) name and its absolute directory.
    pub workspace: String,
    pub dir: PathBuf,
    /// The tab's display name.
    pub tab: String,
    pub title: String,
    /// As `PaneInfo::agent`: the detected agent, else a self-reporting pane's
    /// label, else `None` for a plain shell.
    pub agent: Option<AgentKind>,
    pub state: AgentState,
    pub source: Option<StateSource>,
    pub exited: Option<i32>,
    /// Whether this is the attached client's focused pane.
    pub focused: bool,
}

/// One subagent row under a hook-driven agent in the sidebar: a short
/// description and whether it is still running. Display-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentInfo {
    pub id: String,
    pub desc: String,
    pub running: bool,
}

/// One changed file in a workspace's delta against trunk (`DIFF_RANGE`):
/// its repo-root-relative path and exact added/removed line counts. Drives the
/// client's changed-files panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub insertions: u64,
    pub deletions: u64,
}

/// The full structure an attached client renders: every workspace, its tabs,
/// each tab's `Layout` tree and the panes living in it. Sent on attach and on
/// every structural change so the client can lay panes out without a second
/// round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: WorkspaceId,
    pub name: String,
    pub dir: PathBuf,
    /// The workspace's current git branch, when its directory is a git
    /// checkout. `None` when it is not a repo or HEAD is unreadable.
    pub branch: Option<String>,
    /// A short pre-formatted jj change stat like `4 files +120 −33`, refreshed
    /// as agents work. `None` when the workspace is not a jj repo, has no
    /// changes, or has not been probed yet — the sidebar stays quiet then.
    #[serde(default)]
    pub changes: Option<String>,
    /// The per-file breakdown behind `changes`: each changed file with its exact
    /// `+`/`−` line counts, in diff order. Empty when the workspace is not a jj
    /// repo, is clean against trunk, or the server predates the field.
    #[serde(default)]
    pub changed_files: Vec<FileStat>,
    /// Whether this workspace's jj working copy is stale — its `@` was rewritten
    /// from another workspace, so it needs `jj workspace update-stale`. Surfaced
    /// as a sidebar tag; never auto-fixed. Defaults false for non-jj workspaces
    /// and older servers.
    #[serde(default)]
    pub stale: bool,
    /// The workspace this one is nested under: for a jj-workspace child, its
    /// origin project's id, so the sidebar renders it indented beneath the parent.
    /// `None` for a top-level project (and when the origin has been killed — the
    /// child then renders top-level). Defaults `None` for older servers.
    #[serde(default)]
    pub parent: Option<WorkspaceId>,
    pub tabs: Vec<TabView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabView {
    pub id: TabId,
    pub name: String,
    /// Whether this is the session's current tab.
    pub active: bool,
    /// `None` until the tab's first pane exists.
    pub layout: Option<Layout>,
    pub active_pane: Option<PaneId>,
    /// Per-pane metadata, in the tab's layout order.
    pub panes: Vec<PaneInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error {
        message: String,
    },
    WorkspaceCreated {
        id: WorkspaceId,
    },
    /// A `WorkspaceMerge` landed: `bookmark` is the trunk it advanced (`main` or
    /// `master`), `pushed` whether `jj git push` ran and succeeded.
    Merged {
        pushed: bool,
        bookmark: String,
    },
    TabCreated {
        id: TabId,
    },
    PaneCreated {
        id: PaneId,
    },
    /// A `PaneWait` ended: `pane` as it is now, its `state` the one that
    /// satisfied the wait — or, when `timed_out`, the one it is still in.
    Waited {
        pane: PaneInfo,
        timed_out: bool,
    },
    /// A `Run` landed: the workspace it ran in, its fresh tab, the new pane.
    Ran {
        workspace: WorkspaceId,
        tab: TabId,
        pane: PaneId,
    },
    Workspaces {
        workspaces: Vec<WorkspaceInfo>,
    },
    Tabs {
        tabs: Vec<TabInfo>,
    },
    Panes {
        panes: Vec<PaneInfo>,
    },
    Content {
        lines: Vec<String>,
    },
    /// A `PaneRead { since: Some(_) }` reply: the rows committed to the pane's
    /// scrollback since that cursor, oldest first, then the live screen (the
    /// screen alone, cursor held, while its alternate screen is up). `cursor`
    /// is the next read's `since` — it counts screen rows, so `unwrapped`
    /// folds each read on its own. `truncated`: rows past the cursor had
    /// already left the scrollback ring.
    Tail {
        lines: Vec<String>,
        cursor: u64,
        truncated: bool,
    },
    /// A `PaneFingerprint` reply: the pane's prompt fingerprint, 16 hex digits.
    Fingerprint {
        fingerprint: String,
    },
    /// An `ApprovalHold` reply: how the held permission request was answered.
    Approval {
        decision: ApprovalDecision,
    },
    /// A pane's checkpoints, oldest first.
    Checkpoints {
        checkpoints: Vec<Checkpoint>,
    },
    /// A `PaneRestore` landed: the working copy now matches checkpoint `n`.
    /// `saved` is the `before restore` checkpoint holding the state it replaced
    /// — `None` when nothing had changed since the pane's last checkpoint or
    /// restore, so no new one was needed.
    Restored {
        n: u64,
        saved: Option<u64>,
    },
    /// A `SessionRestore` landed: how many workspaces the saved layout came
    /// back into, the panes it spawned, how many of those revived an agent's
    /// conversation, and what could not come back (a workspace whose directory
    /// is gone, a pane that failed to spawn). All zero for a discard.
    SessionRestored {
        workspaces: usize,
        panes: usize,
        revived: usize,
        skipped: Vec<String>,
    },
    /// The attach handshake reply: the session name and its full view. The
    /// pane snapshots that seed the client's parsers follow as pane frames.
    /// `wire_rev` is the server's `WIRE_REV`; a client warns on mismatch (an
    /// old daemon defaults to 0, which correctly reads as outdated).
    /// `restorable` summarises the layout a previous daemon left to offer back
    /// (`Request::SessionRestore`); `None` when there is none, and from older
    /// servers.
    Attached {
        session: String,
        workspaces: Vec<WorkspaceView>,
        #[serde(default)]
        wire_rev: u32,
        #[serde(default)]
        restorable: Option<RestoreSummary>,
    },
}

/// What a pending restore would bring back, as an attach offers it: the saved
/// workspaces and panes, how many of the panes are agents it would revive in
/// their own conversation, and when the layout was saved (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSummary {
    pub workspaces: usize,
    pub panes: usize,
    pub agents: usize,
    pub saved_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    PaneOutput {
        pane: PaneId,
    },
    StateChanged {
        pane: PaneId,
        from: AgentState,
        to: AgentState,
    },
    PaneExited {
        pane: PaneId,
        code: i32,
    },
    /// The workspace/tab/pane structure changed; carries the fresh view so the
    /// client re-lays-out without asking. Fresh pane snapshots follow on the tick.
    LayoutChanged {
        workspaces: Vec<WorkspaceView>,
    },
    /// A pane emitted a bell or desktop-notification escape (OSC 9 / 777). A
    /// bare bell carries no text; OSC 9 fills `body`; OSC 777 fills `title`
    /// and/or `body`. Purely an attention signal — it never drives pane state.
    /// An OSC 7501 report's `msg` lands here too (`title` = its `app`) when the
    /// report moves the pane to done/blocked, at most once per 30s per pane.
    PaneNotification {
        pane: PaneId,
        title: Option<String>,
        body: Option<String>,
    },
    /// Someone other than an attached client focused `pane` — a script's
    /// `pane focus`, a notch app's click: the attached client shows it,
    /// switching workspace and tab. Never sent for a client's own focus.
    PaneFocused {
        pane: PaneId,
    },
    /// The whole session as a watcher (`Request::Watch`) sees it, replacing
    /// the previous snapshot: every pane flat, in sidebar order.
    /// `attached_terminal` is the newest attached client's `terminal` — `None`
    /// with nobody attached — and `focused` marks that client's pane.
    Snapshot {
        session: String,
        wire_rev: u32,
        attached_terminal: Option<String>,
        panes: Vec<WatchPane>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(*value, back, "roundtrip mismatch via {json}");
    }

    #[test]
    fn request_roundtrip() {
        roundtrip(&Request::WorkspaceList);
        roundtrip(&Request::SessionRestore { discard: false });
        roundtrip(&Request::SessionRestore { discard: true });
        roundtrip(&Request::WorkspaceNew {
            dir: PathBuf::from("/tmp/project"),
        });
        roundtrip(&Request::PaneRun {
            tab: Some(TabId(3)),
            cmd: vec!["claude".into(), "--dangerously".into()],
            ephemeral: false,
            sandbox: None,
        });
        roundtrip(&Request::PaneRun {
            tab: None,
            cmd: vec!["sh".into()],
            ephemeral: true,
            sandbox: None,
        });
        roundtrip(&Request::PaneRun {
            tab: None,
            cmd: vec!["claude".into()],
            ephemeral: false,
            sandbox: Some("gke".into()),
        });
        roundtrip(&Request::WorkspaceDiff {
            id: WorkspaceId(2),
            stat: true,
        });
        roundtrip(&Request::WorkspaceKill {
            id: WorkspaceId(3),
            discard: true,
        });
        roundtrip(&Request::WorkspaceFork {
            id: WorkspaceId(4),
            name: "feature".into(),
            revision: Some("@".into()),
            dest: Some(PathBuf::from("/tmp/repo-feature")),
        });
        roundtrip(&Request::WorkspaceFork {
            id: WorkspaceId(4),
            name: "feature".into(),
            revision: None,
            dest: None,
        });
        roundtrip(&Request::WorkspaceUpdate { id: WorkspaceId(5) });
        roundtrip(&Request::WorkspaceMerge {
            id: WorkspaceId(6),
            push: true,
        });
        roundtrip(&Request::WorkspaceMerge {
            id: WorkspaceId(6),
            push: false,
        });
        roundtrip(&Request::TabNew { workspace: None });
        roundtrip(&Request::Run {
            workspace: Some(WorkspaceId(2)),
            dir: None,
            cmd: vec!["claude".into(), "fix the tests".into()],
            sandbox: Some("gke".into()),
            title: Some("fixer".into()),
            from: None,
        });
        roundtrip(&Request::Run {
            workspace: None,
            dir: Some(PathBuf::from("/tmp/project")),
            cmd: vec!["sh".into()],
            sandbox: None,
            title: None,
            from: None,
        });
        roundtrip(&Request::Run {
            workspace: None,
            dir: None,
            cmd: vec!["claude".into()],
            sandbox: None,
            title: None,
            from: Some(PaneId(7)),
        });
        roundtrip(&Request::PaneSplit {
            pane: PaneId(1),
            direction: Direction::Vertical,
        });
        roundtrip(&Request::PaneSend {
            pane: PaneId(2),
            text: Some("hello".into()),
            keys: None,
            expect: None,
        });
        roundtrip(&Request::PaneSend {
            pane: PaneId(2),
            text: None,
            keys: Some("enter".into()),
            expect: Some("af63dc4c8601ec8c".into()),
        });
        roundtrip(&Request::PaneFingerprint { pane: PaneId(2) });
        roundtrip(&Request::PaneWait {
            pane: PaneId(2),
            until: vec![WaitFor::Blocked, WaitFor::Done],
            next: true,
            timeout_ms: Some(1500),
        });
        roundtrip(&Request::PaneRead {
            pane: PaneId(2),
            lines: Some(50),
            unwrapped: true,
            since: None,
        });
        roundtrip(&Request::PaneRead {
            pane: PaneId(2),
            lines: None,
            unwrapped: false,
            since: Some(10_177),
        });
        roundtrip(&Request::PaneResize {
            pane: PaneId(2),
            rows: 40,
            cols: 120,
        });
        roundtrip(&Request::PaneScroll {
            pane: PaneId(2),
            offset: 120,
        });
        roundtrip(&Request::PaneFocus { pane: PaneId(2) });
        roundtrip(&Request::PaneResizeSplit {
            pane: PaneId(2),
            direction: Direction::Horizontal,
            delta: 0.05,
        });
        roundtrip(&Request::PaneSplitRatio {
            a: PaneId(2),
            b: PaneId(3),
            ratio: 0.35,
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::SubagentStarted {
                id: "s1".into(),
                desc: "write the tests".into(),
            },
            conversation: None,
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::SubagentStopped { id: "s1".into() },
            conversation: None,
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Activity {
                detail: Some("Bash".into()),
            },
            conversation: Some("0b9e6c1e-3d2a-4f5b-9c8d-7e6f5a4b3c2d".into()),
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Blocked {
                message: Some("allow edit?".into()),
            },
            conversation: None,
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Done,
            conversation: None,
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::SessionStarted,
            conversation: Some("ses_1".into()),
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Prompt {
                text: Some("fix the failing test".into()),
            },
            conversation: Some("ses_1".into()),
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Prompt { text: None },
            conversation: None,
        });
        roundtrip(&Request::ApprovalHold {
            pane: PaneId(3),
            conversation: Some("ses_1".into()),
            tool: "Bash".into(),
            summary: "rm -rf build".into(),
        });
        roundtrip(&Request::ApprovalAnswer {
            pane: PaneId(3),
            id: Some(7),
            decision: ApprovalDecision::Allow,
        });
        roundtrip(&Request::ApprovalAnswer {
            pane: PaneId(3),
            id: None,
            decision: ApprovalDecision::Deny {
                message: Some("not on main".into()),
            },
        });
        roundtrip(&Request::ApprovalAnswer {
            pane: PaneId(3),
            id: Some(7),
            decision: ApprovalDecision::Pass,
        });
        roundtrip(&Request::PaneCheckpoints { pane: PaneId(3) });
        roundtrip(&Request::PaneCheckpointDiff {
            pane: PaneId(3),
            n: Some(2),
            stat: true,
        });
        roundtrip(&Request::PaneRestore {
            pane: PaneId(3),
            n: 1,
            force: true,
        });
        roundtrip(&Request::SandboxPull {
            pane: PaneId(3),
            name: Some("review".into()),
        });
        roundtrip(&Request::SandboxPull {
            pane: PaneId(3),
            name: None,
        });
        roundtrip(&Request::Attach { terminal: None });
        roundtrip(&Request::Attach {
            terminal: Some("com.mitchellh.ghostty".into()),
        });
        roundtrip(&Request::Watch);
    }

    fn sample_checkpoint() -> Checkpoint {
        Checkpoint {
            n: 2,
            commit: "0fe66d0872aa60dad12f02a7c49e1d220a67229d".into(),
            from: Some("da7330dd4aa56869ad23109aba788bb16deb53af".into()),
            time: 1_760_000_000,
            label: "done".into(),
            prompt: Some("fix the failing test".into()),
            files: 1,
            insertions: 2,
            deletions: 1,
        }
    }

    #[test]
    fn agent_hook_event_is_kind_tagged_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentHookEvent::Done).unwrap(),
            r#"{"kind":"done"}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentHookEvent::SubagentStopped { id: "a".into() }).unwrap(),
            r#"{"kind":"subagent_stopped","id":"a"}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentHookEvent::SessionStarted).unwrap(),
            r#"{"kind":"session_started"}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentHookEvent::Prompt {
                text: Some("fix it".into())
            })
            .unwrap(),
            r#"{"kind":"prompt","text":"fix it"}"#
        );
        // A hook event serialized without the optional field still parses.
        let activity: AgentHookEvent = serde_json::from_str(r#"{"kind":"activity"}"#).unwrap();
        assert_eq!(activity, AgentHookEvent::Activity { detail: None });
        let prompt: AgentHookEvent = serde_json::from_str(r#"{"kind":"prompt"}"#).unwrap();
        assert_eq!(prompt, AgentHookEvent::Prompt { text: None });
    }

    /// A decision rides the wire tagged by `behavior`, as Claude's hook
    /// output spells it; a hold without a conversation, an answer without an
    /// id (whichever is held), and a denial without a reason all parse.
    #[test]
    fn approvals_ride_the_wire_behavior_tagged() {
        assert_eq!(
            serde_json::to_string(&Response::Approval {
                decision: ApprovalDecision::Allow
            })
            .unwrap(),
            r#"{"type":"approval","decision":{"behavior":"allow"}}"#
        );
        let hold: Request = serde_json::from_str(
            r#"{"type":"approval_hold","pane":3,"tool":"Edit","summary":"src/main.rs"}"#,
        )
        .unwrap();
        assert_eq!(
            hold,
            Request::ApprovalHold {
                pane: PaneId(3),
                conversation: None,
                tool: "Edit".into(),
                summary: "src/main.rs".into(),
            }
        );
        let answer: Request = serde_json::from_str(
            r#"{"type":"approval_answer","pane":3,"decision":{"behavior":"deny"}}"#,
        )
        .unwrap();
        assert_eq!(
            answer,
            Request::ApprovalAnswer {
                pane: PaneId(3),
                id: None,
                decision: ApprovalDecision::Deny { message: None },
            }
        );
    }

    #[test]
    fn wait_for_done_means_finished_seen_or_not() {
        use AgentState::*;
        let matching = |w: WaitFor| {
            [Unknown, Working, Blocked, Done, Idle]
                .into_iter()
                .filter(|s| w.matches(*s))
                .collect::<Vec<_>>()
        };
        assert_eq!(matching(WaitFor::Working), [Working]);
        assert_eq!(matching(WaitFor::Blocked), [Blocked]);
        assert_eq!(matching(WaitFor::Done), [Done, Idle]);
        assert_eq!(matching(WaitFor::Idle), [Idle]);
        assert_eq!(serde_json::to_string(&WaitFor::Idle).unwrap(), r#""idle""#);
    }

    fn sample_view() -> Vec<WorkspaceView> {
        vec![WorkspaceView {
            id: WorkspaceId(1),
            name: "api".into(),
            dir: PathBuf::from("/tmp/w"),
            branch: Some("main".into()),
            changes: Some("4 files +120 −33".into()),
            changed_files: vec![FileStat {
                path: "src/lib.rs".into(),
                insertions: 120,
                deletions: 33,
            }],
            stale: false,
            parent: None,
            tabs: vec![TabView {
                id: TabId(1),
                name: "main".into(),
                active: true,
                layout: Some(Layout::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Layout::Leaf(PaneId(1))),
                    second: Box::new(Layout::Leaf(PaneId(2))),
                }),
                active_pane: Some(PaneId(2)),
                panes: vec![PaneInfo {
                    id: PaneId(1),
                    title: "shell".into(),
                    agent: None,
                    state: AgentState::Idle,
                    exited: None,
                    subagents: Vec::new(),
                    checkpoints: vec![sample_checkpoint()],
                    approval: None,
                    source: None,
                    sandbox: None,
                    conversation: None,
                    sandbox_name: None,
                }],
            }],
        }]
    }

    #[test]
    fn response_roundtrip() {
        roundtrip(&Response::Ok);
        roundtrip(&Response::Error {
            message: "boom".into(),
        });
        roundtrip(&Response::PaneCreated { id: PaneId(5) });
        let pane = sample_view()[0].tabs[0].panes[0].clone();
        roundtrip(&Response::Waited {
            pane: pane.clone(),
            timed_out: false,
        });
        roundtrip(&Response::Waited {
            pane,
            timed_out: true,
        });
        roundtrip(&Response::Ran {
            workspace: WorkspaceId(1),
            tab: TabId(4),
            pane: PaneId(9),
        });
        roundtrip(&Response::Panes {
            panes: vec![PaneInfo {
                id: PaneId(1),
                title: "shell".into(),
                agent: Some("claude".into()),
                state: AgentState::Working,
                exited: None,
                subagents: vec![SubagentInfo {
                    id: "s1".into(),
                    desc: "build the core".into(),
                    running: true,
                }],
                checkpoints: Vec::new(),
                approval: Some(PendingApproval {
                    id: 7,
                    tool: "Bash".into(),
                    summary: "rm -rf build".into(),
                }),
                source: Some(StateSource::Hook),
                sandbox: Some("gke".into()),
                conversation: Some("0b9e6c1e-3d2a-4f5b-9c8d-7e6f5a4b3c2d".into()),
                sandbox_name: Some("tutti-p1-18f2a9c0de".into()),
            }],
        });
        roundtrip(&Response::Panes {
            panes: vec![PaneInfo {
                id: PaneId(2),
                title: "zsh".into(),
                agent: Some("build".into()),
                state: AgentState::Done,
                exited: None,
                subagents: Vec::new(),
                checkpoints: Vec::new(),
                approval: None,
                source: Some(StateSource::Program),
                sandbox: None,
                conversation: None,
                sandbox_name: None,
            }],
        });
        roundtrip(&Response::Checkpoints {
            checkpoints: vec![
                Checkpoint {
                    n: 1,
                    from: None,
                    label: "start".into(),
                    prompt: None,
                    files: 0,
                    insertions: 0,
                    deletions: 0,
                    ..sample_checkpoint()
                },
                sample_checkpoint(),
            ],
        });
        roundtrip(&Response::Restored {
            n: 1,
            saved: Some(3),
        });
        roundtrip(&Response::Restored { n: 2, saved: None });
        roundtrip(&Response::SessionRestored {
            workspaces: 2,
            panes: 5,
            revived: 1,
            skipped: vec!["/srv/gone: directory is gone".into()],
        });
        roundtrip(&Response::Content {
            lines: vec!["line one".into(), "line two".into()],
        });
        roundtrip(&Response::Tail {
            lines: vec!["committed".into(), "screen".into()],
            cursor: 238,
            truncated: false,
        });
        roundtrip(&Response::Tail {
            lines: Vec::new(),
            cursor: 0,
            truncated: true,
        });
        roundtrip(&Response::Fingerprint {
            fingerprint: "af63dc4c8601ec8c".into(),
        });
        for decision in [
            ApprovalDecision::Allow,
            ApprovalDecision::Deny {
                message: Some("not on main".into()),
            },
            ApprovalDecision::Deny { message: None },
            ApprovalDecision::Pass,
        ] {
            roundtrip(&Response::Approval { decision });
        }
        roundtrip(&Response::Merged {
            pushed: true,
            bookmark: "main".into(),
        });
        roundtrip(&Response::Merged {
            pushed: false,
            bookmark: "master".into(),
        });
        roundtrip(&Response::Attached {
            wire_rev: WIRE_REV,
            session: "tutti".into(),
            workspaces: sample_view(),
            restorable: None,
        });
        roundtrip(&Response::Attached {
            wire_rev: WIRE_REV,
            session: "tutti".into(),
            workspaces: Vec::new(),
            restorable: Some(RestoreSummary {
                workspaces: 3,
                panes: 7,
                agents: 2,
                saved_at: 1_760_000_000,
            }),
        });
    }

    #[test]
    fn event_roundtrip() {
        roundtrip(&Event::PaneOutput { pane: PaneId(1) });
        roundtrip(&Event::StateChanged {
            pane: PaneId(1),
            from: AgentState::Working,
            to: AgentState::Done,
        });
        roundtrip(&Event::PaneExited {
            pane: PaneId(1),
            code: 0,
        });
        roundtrip(&Event::LayoutChanged {
            workspaces: sample_view(),
        });
        roundtrip(&Event::PaneNotification {
            pane: PaneId(1),
            title: None,
            body: None,
        });
        roundtrip(&Event::PaneNotification {
            pane: PaneId(2),
            title: Some("agent".into()),
            body: Some("ready to merge".into()),
        });
        roundtrip(&Event::PaneFocused { pane: PaneId(3) });
        roundtrip(&sample_snapshot());
        roundtrip(&Event::Snapshot {
            session: "tutti".into(),
            wire_rev: WIRE_REV,
            attached_terminal: None,
            panes: Vec::new(),
        });
    }

    fn sample_snapshot() -> Event {
        Event::Snapshot {
            session: "tutti".into(),
            wire_rev: 6,
            attached_terminal: Some("com.mitchellh.ghostty".into()),
            panes: vec![WatchPane {
                id: PaneId(3),
                workspace: "smartnotch".into(),
                dir: PathBuf::from("/Users/me/develop/smartnotch"),
                tab: "main".into(),
                title: "claude".into(),
                agent: Some("claude".into()),
                state: AgentState::Working,
                source: Some(StateSource::Hook),
                exited: None,
                focused: true,
            }],
        }
    }

    /// `tutti watch` prints snapshots verbatim, and a notch app is written
    /// against this exact line.
    #[test]
    fn a_snapshot_is_the_watch_contract_line() {
        assert_eq!(
            serde_json::to_string(&sample_snapshot()).unwrap(),
            concat!(
                r#"{"type":"snapshot","session":"tutti","wire_rev":6,"attached_terminal":"com.mitchellh.ghostty","#,
                r#""panes":[{"id":3,"workspace":"smartnotch","dir":"/Users/me/develop/smartnotch","tab":"main","#,
                r#""title":"claude","agent":"claude","state":"working","source":"hook","exited":null,"focused":true}]}"#
            )
        );
    }

    #[test]
    fn ndjson_shape_is_tagged_snake_case() {
        assert_eq!(
            serde_json::to_string(&Request::WorkspaceList).unwrap(),
            r#"{"type":"workspace_list"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::PaneSplit {
                pane: PaneId(1),
                direction: Direction::Horizontal,
            })
            .unwrap(),
            r#"{"type":"pane_split","pane":1,"direction":"horizontal"}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::StateChanged {
                pane: PaneId(1),
                from: AgentState::Working,
                to: AgentState::Idle,
            })
            .unwrap(),
            r#"{"type":"state_changed","pane":1,"from":"working","to":"idle"}"#
        );
    }

    #[test]
    fn a_workspace_listing_names_each_entrys_parent() {
        let project = WorkspaceInfo {
            id: WorkspaceId(1),
            name: "api".into(),
            dir: PathBuf::from("/srv/api"),
            parent: None,
        };
        let workspace = WorkspaceInfo {
            id: WorkspaceId(2),
            name: "api-fix".into(),
            dir: PathBuf::from("/srv/api-fix"),
            parent: Some(WorkspaceId(1)),
        };
        roundtrip(&Response::Workspaces {
            workspaces: vec![project, workspace],
        });
        // A listing from before `parent` existed reads as all top-level.
        let old: Response = serde_json::from_str(
            r#"{"type":"workspaces","workspaces":[{"id":1,"name":"api","dir":"/srv/api"}]}"#,
        )
        .unwrap();
        let Response::Workspaces { workspaces } = old else {
            panic!("expected a workspace listing");
        };
        assert_eq!(workspaces[0].parent, None);
    }

    #[test]
    fn additive_fields_default_when_omitted() {
        // `pane_run` from before `ephemeral`/`sandbox` existed still parses
        // (non-ephemeral, local).
        let run: Request =
            serde_json::from_str(r#"{"type":"pane_run","tab":null,"cmd":["sh"]}"#).unwrap();
        assert_eq!(
            run,
            Request::PaneRun {
                tab: None,
                cmd: vec!["sh".into()],
                ephemeral: false,
                sandbox: None,
            }
        );
        // `workspace_diff` without `stat` defaults to the full diff.
        let diff: Request = serde_json::from_str(r#"{"type":"workspace_diff","id":7}"#).unwrap();
        assert_eq!(
            diff,
            Request::WorkspaceDiff {
                id: WorkspaceId(7),
                stat: false,
            }
        );
        // A view serialized without `changes`/`changed_files`/`stale`/`parent`
        // (older server) defaults them.
        let view: WorkspaceView =
            serde_json::from_str(r#"{"id":1,"name":"api","dir":"/tmp/w","branch":null,"tabs":[]}"#)
                .unwrap();
        assert_eq!(view.changes, None);
        assert!(view.changed_files.is_empty());
        assert!(!view.stale);
        assert_eq!(view.parent, None);
        // A `parent` present on the wire round-trips into a nested child.
        let child: WorkspaceView = serde_json::from_str(
            r#"{"id":2,"name":"feature","dir":"/tmp/w-feature","branch":null,"parent":1,"tabs":[]}"#,
        )
        .unwrap();
        assert_eq!(child.parent, Some(WorkspaceId(1)));
        // A `pane_info` from before `subagents`/`source` existed defaults them.
        let pane: PaneInfo = serde_json::from_str(
            r#"{"id":1,"title":"shell","agent":null,"state":"idle","exited":null}"#,
        )
        .unwrap();
        assert!(pane.subagents.is_empty());
        assert_eq!(pane.source, None);
        assert_eq!(pane.sandbox, None, "and before `sandbox`, a local pane");
        assert_eq!(pane.conversation, None, "and before `conversation`, none");
        assert_eq!(pane.sandbox_name, None, "and before `sandbox_name`, none");
        assert_eq!(pane.approval, None, "and before `approval`, nothing held");
        // ...and before `checkpoints` existed, to no checkpoints.
        assert!(pane.checkpoints.is_empty());
        // A checkpoint from before `prompt` existed lists as its label.
        let checkpoint: Checkpoint = serde_json::from_str(
            r#"{"n":2,"commit":"c2","from":"c1","time":1,"label":"done","files":1,"insertions":2,"deletions":1}"#,
        )
        .unwrap();
        assert_eq!(checkpoint.prompt, None);
        // `agent_event` from before `conversation` existed names none, which
        // keeps whatever the pane holds.
        let event: Request =
            serde_json::from_str(r#"{"type":"agent_event","pane":3,"event":{"kind":"done"}}"#)
                .unwrap();
        assert_eq!(
            event,
            Request::AgentEvent {
                pane: PaneId(3),
                event: AgentHookEvent::Done,
                conversation: None,
            }
        );
        // `pane_checkpoint_diff` without `n`/`stat` is the newest turn's full
        // diff; `pane_restore` without `force` respects the state guard.
        let diff: Request =
            serde_json::from_str(r#"{"type":"pane_checkpoint_diff","pane":4}"#).unwrap();
        assert_eq!(
            diff,
            Request::PaneCheckpointDiff {
                pane: PaneId(4),
                n: None,
                stat: false,
            }
        );
        let restore: Request =
            serde_json::from_str(r#"{"type":"pane_restore","pane":4,"n":2}"#).unwrap();
        assert_eq!(
            restore,
            Request::PaneRestore {
                pane: PaneId(4),
                n: 2,
                force: false,
            }
        );
        // The source rides the wire snake_case.
        let pane: PaneInfo = serde_json::from_str(
            r#"{"id":1,"title":"sh","agent":null,"state":"working","exited":null,"source":"progress"}"#,
        )
        .unwrap();
        assert_eq!(pane.source, Some(StateSource::Progress));
        // `workspace_kill` from before `discard` existed still parses (keep).
        let kill: Request = serde_json::from_str(r#"{"type":"workspace_kill","id":2}"#).unwrap();
        assert_eq!(
            kill,
            Request::WorkspaceKill {
                id: WorkspaceId(2),
                discard: false,
            }
        );
        // `pane_send` from before `expect` existed is a plain, unguarded send.
        let send: Request =
            serde_json::from_str(r#"{"type":"pane_send","pane":2,"text":"y","keys":null}"#)
                .unwrap();
        assert_eq!(
            send,
            Request::PaneSend {
                pane: PaneId(2),
                text: Some("y".into()),
                keys: None,
                expect: None,
            }
        );
        // `pane_read` from before `since` existed reads everything, as Content.
        let read: Request =
            serde_json::from_str(r#"{"type":"pane_read","pane":2,"lines":null,"unwrapped":false}"#)
                .unwrap();
        assert_eq!(
            read,
            Request::PaneRead {
                pane: PaneId(2),
                lines: None,
                unwrapped: false,
                since: None,
            }
        );
        // `attach` from before `terminal` existed names no terminal.
        let attach: Request = serde_json::from_str(r#"{"type":"attach"}"#).unwrap();
        assert_eq!(attach, Request::Attach { terminal: None });
        // ...and an older daemon, whose `Attach` is a unit variant, still
        // accepts an attach that names one.
        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum OldRequest {
            Attach,
        }
        let old: OldRequest =
            serde_json::from_str(r#"{"type":"attach","terminal":"com.mitchellh.ghostty"}"#)
                .unwrap();
        assert_eq!(old, OldRequest::Attach);
        // `workspace_fork` without a revision or dest defaults both to `None`.
        let fork: Request =
            serde_json::from_str(r#"{"type":"workspace_fork","id":1,"name":"x"}"#).unwrap();
        assert_eq!(
            fork,
            Request::WorkspaceFork {
                id: WorkspaceId(1),
                name: "x".into(),
                revision: None,
                dest: None,
            }
        );
        // `pane_wait` without `next`/`timeout_ms` is level-triggered and
        // unbounded.
        let wait: Request =
            serde_json::from_str(r#"{"type":"pane_wait","pane":3,"until":["done"]}"#).unwrap();
        assert_eq!(
            wait,
            Request::PaneWait {
                pane: PaneId(3),
                until: vec![WaitFor::Done],
                next: false,
                timeout_ms: None,
            }
        );
        // `sandbox_pull` without a name names the workspace after the sandbox.
        let pull: Request = serde_json::from_str(r#"{"type":"sandbox_pull","pane":3}"#).unwrap();
        assert_eq!(
            pull,
            Request::SandboxPull {
                pane: PaneId(3),
                name: None,
            }
        );
        // `run` with only a command runs locally in the current workspace,
        // under the command's own title — and from before `from` existed,
        // from no pane.
        let run: Request = serde_json::from_str(r#"{"type":"run","cmd":["claude"]}"#).unwrap();
        assert_eq!(
            run,
            Request::Run {
                workspace: None,
                dir: None,
                cmd: vec!["claude".into()],
                sandbox: None,
                title: None,
                from: None,
            }
        );
        // A bare `session_restore` restores rather than discards.
        let restore: Request = serde_json::from_str(r#"{"type":"session_restore"}"#).unwrap();
        assert_eq!(restore, Request::SessionRestore { discard: false });
        // An `attached` from before `restorable` existed offers nothing.
        let attached: Response = serde_json::from_str(
            r#"{"type":"attached","session":"tutti","workspaces":[],"wire_rev":6}"#,
        )
        .unwrap();
        assert_eq!(
            attached,
            Response::Attached {
                session: "tutti".into(),
                workspaces: Vec::new(),
                wire_rev: 6,
                restorable: None,
            }
        );
    }
}
