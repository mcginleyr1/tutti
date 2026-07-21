use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::{AgentKind, Direction};
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
        workspace: WorkspaceId,
    },
    TabList {
        workspace: WorkspaceId,
    },
    TabSelect {
        id: TabId,
    },
    PaneRun {
        tab: TabId,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error { message: String },
    WorkspaceCreated { id: WorkspaceId },
    TabCreated { id: TabId },
    PaneCreated { id: PaneId },
    Workspaces { workspaces: Vec<WorkspaceInfo> },
    Tabs { tabs: Vec<TabInfo> },
    Panes { panes: Vec<PaneInfo> },
    Content { lines: Vec<String> },
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
    LayoutChanged {
        workspace: WorkspaceId,
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
            tab: TabId(3),
            cmd: vec!["claude".into(), "--dangerously".into()],
        });
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
        roundtrip(&Request::Attach);
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
            workspace: WorkspaceId(2),
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
