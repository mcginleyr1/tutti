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
    Attach,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: TabId,
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        roundtrip(&Request::Attach);
    }

    fn sample_view() -> Vec<WorkspaceView> {
        vec![WorkspaceView {
            id: WorkspaceId(1),
            name: "api".into(),
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
}
