use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::{AgentKind, Direction, Layout};
use crate::ids::{PaneId, TabId, WorkspaceId};
use crate::state::AgentState;

/// The wire-protocol revision. BUMP THIS on ANY change to the Request /
/// Response / Event shapes (including additive fields): the daemon reports it
/// on attach and mismatched clients tell the user to restart the daemon —
/// binaries upgrade on install, running daemons do not.
pub const WIRE_REV: u32 = 4;

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
    PaneRun {
        tab: Option<TabId>,
        cmd: Vec<String>,
        /// An ephemeral pane is removed entirely when its child exits (no exited
        /// corpse row), rather than kept readable. Used for throwaway views like
        /// the jj diff pane.
        #[serde(default)]
        ephemeral: bool,
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
    PaneRead {
        pane: PaneId,
        lines: Option<usize>,
        unwrapped: bool,
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
    /// A ground-truth agent-lifecycle signal for `pane`, forwarded from a Claude
    /// Code hook by `tutti agent-event`. Drives exact state + the subagent list,
    /// replacing screen-heuristic guessing for that pane.
    AgentEvent {
        pane: PaneId,
        event: AgentHookEvent,
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
    Attach,
    Detach,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub dir: PathBuf,
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
    /// The turn's stat over `from..commit`, computed once at record time.
    pub files: usize,
    pub insertions: u64,
    pub deletions: u64,
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
    /// A `PaneFingerprint` reply: the pane's prompt fingerprint, 16 hex digits.
    Fingerprint {
        fingerprint: String,
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
    /// The attach handshake reply: the session name and its full view. The
    /// pane snapshots that seed the client's parsers follow as pane frames.
    /// `wire_rev` is the server's `WIRE_REV`; a client warns on mismatch (an
    /// old daemon defaults to 0, which correctly reads as outdated).
    Attached {
        session: String,
        workspaces: Vec<WorkspaceView>,
        #[serde(default)]
        wire_rev: u32,
    },
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
    PaneNotification {
        pane: PaneId,
        title: Option<String>,
        body: Option<String>,
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
        roundtrip(&Request::WorkspaceNew {
            dir: PathBuf::from("/tmp/project"),
        });
        roundtrip(&Request::PaneRun {
            tab: Some(TabId(3)),
            cmd: vec!["claude".into(), "--dangerously".into()],
            ephemeral: false,
        });
        roundtrip(&Request::PaneRun {
            tab: None,
            cmd: vec!["sh".into()],
            ephemeral: true,
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
        roundtrip(&Request::PaneRead {
            pane: PaneId(2),
            lines: Some(50),
            unwrapped: true,
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
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::SubagentStarted {
                id: "s1".into(),
                desc: "write the tests".into(),
            },
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::SubagentStopped { id: "s1".into() },
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Activity {
                detail: Some("Bash".into()),
            },
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Blocked {
                message: Some("allow edit?".into()),
            },
        });
        roundtrip(&Request::AgentEvent {
            pane: PaneId(3),
            event: AgentHookEvent::Done,
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
        roundtrip(&Request::Attach);
    }

    fn sample_checkpoint() -> Checkpoint {
        Checkpoint {
            n: 2,
            commit: "0fe66d0872aa60dad12f02a7c49e1d220a67229d".into(),
            from: Some("da7330dd4aa56869ad23109aba788bb16deb53af".into()),
            time: 1_760_000_000,
            label: "done".into(),
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
        // A hook event serialized without the optional field still parses.
        let activity: AgentHookEvent = serde_json::from_str(r#"{"kind":"activity"}"#).unwrap();
        assert_eq!(activity, AgentHookEvent::Activity { detail: None });
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
            }],
        });
        roundtrip(&Response::Checkpoints {
            checkpoints: vec![
                Checkpoint {
                    n: 1,
                    from: None,
                    label: "start".into(),
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
        roundtrip(&Response::Content {
            lines: vec!["line one".into(), "line two".into()],
        });
        roundtrip(&Response::Fingerprint {
            fingerprint: "af63dc4c8601ec8c".into(),
        });
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
    fn additive_fields_default_when_omitted() {
        // `pane_run` from before `ephemeral` existed still parses (non-ephemeral).
        let run: Request =
            serde_json::from_str(r#"{"type":"pane_run","tab":null,"cmd":["sh"]}"#).unwrap();
        assert_eq!(
            run,
            Request::PaneRun {
                tab: None,
                cmd: vec!["sh".into()],
                ephemeral: false,
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
        // A `pane_info` from before `subagents` existed defaults to an empty list.
        let pane: PaneInfo = serde_json::from_str(
            r#"{"id":1,"title":"shell","agent":null,"state":"idle","exited":null}"#,
        )
        .unwrap();
        assert!(pane.subagents.is_empty());
        // ...and before `checkpoints` existed, to no checkpoints.
        assert!(pane.checkpoints.is_empty());
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
    }
}
