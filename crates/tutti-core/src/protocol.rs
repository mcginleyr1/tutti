use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::{AgentKind, Direction, Layout};
use crate::ids::{PaneId, TabId, WorkspaceId};
use crate::state::AgentState;

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
    /// Fork a jj workspace: `jj workspace add` a sibling checkout named `name`
    /// (optionally at `revision`), then mount it as a tutti workspace with a
    /// shell pane. The source workspace must be under a `.jj` repo. Answered with
    /// `Response::WorkspaceCreated`.
    WorkspaceFork {
        id: WorkspaceId,
        name: String,
        revision: Option<String>,
    },
    /// Update a stale forked workspace by running `jj workspace update-stale` in
    /// its directory, then refreshing. The manual fix for the sidebar's `stale`
    /// tag. Answered with `Response::Ok` or `Response::Error`.
    WorkspaceUpdate {
        id: WorkspaceId,
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
    Attach,
    Detach,
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
    /// Whether this workspace's jj working copy is stale — its `@` was rewritten
    /// from another workspace, so it needs `jj workspace update-stale`. Surfaced
    /// as a sidebar tag; never auto-fixed. Defaults false for non-jj workspaces
    /// and older servers.
    #[serde(default)]
    pub stale: bool,
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
    /// The attach handshake reply: the session name and its full view. The
    /// pane snapshots that seed the client's parsers follow as pane frames.
    Attached {
        session: String,
        workspaces: Vec<WorkspaceView>,
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
        });
        roundtrip(&Request::WorkspaceFork {
            id: WorkspaceId(4),
            name: "feature".into(),
            revision: None,
        });
        roundtrip(&Request::WorkspaceUpdate { id: WorkspaceId(5) });
        roundtrip(&Request::TabNew { workspace: None });
        roundtrip(&Request::PaneSplit {
            pane: PaneId(1),
            direction: Direction::Vertical,
        });
        roundtrip(&Request::PaneSend {
            pane: PaneId(2),
            text: Some("hello".into()),
            keys: None,
        });
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
        roundtrip(&Request::Attach);
    }

    fn sample_view() -> Vec<WorkspaceView> {
        vec![WorkspaceView {
            id: WorkspaceId(1),
            name: "api".into(),
            dir: PathBuf::from("/tmp/w"),
            branch: Some("main".into()),
            changes: Some("4 files +120 −33".into()),
            stale: false,
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
            }],
        });
        roundtrip(&Response::Content {
            lines: vec!["line one".into(), "line two".into()],
        });
        roundtrip(&Response::Attached {
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
        // A view serialized without `changes`/`stale` (older server) defaults.
        let view: WorkspaceView =
            serde_json::from_str(r#"{"id":1,"name":"api","dir":"/tmp/w","branch":null,"tabs":[]}"#)
                .unwrap();
        assert_eq!(view.changes, None);
        assert!(!view.stale);
        // `workspace_kill` from before `discard` existed still parses (keep).
        let kill: Request = serde_json::from_str(r#"{"type":"workspace_kill","id":2}"#).unwrap();
        assert_eq!(
            kill,
            Request::WorkspaceKill {
                id: WorkspaceId(2),
                discard: false,
            }
        );
        // `workspace_fork` without a revision defaults to `None`.
        let fork: Request =
            serde_json::from_str(r#"{"type":"workspace_fork","id":1,"name":"x"}"#).unwrap();
        assert_eq!(
            fork,
            Request::WorkspaceFork {
                id: WorkspaceId(1),
                name: "x".into(),
                revision: None,
            }
        );
    }
}
