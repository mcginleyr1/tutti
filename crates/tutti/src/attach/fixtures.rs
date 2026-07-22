//! Shared test fixtures for the attach TUI: compact constructors for the
//! workspace/tab/pane views the unit tests attach `App` to, and the key events
//! they drive it with. Every constructor mirrors the literals it replaces, so a
//! ported test keeps its exact assertions.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tutti_core::{
    AgentState, Direction, Layout, PaneId, PaneInfo, TabId, TabView, WorkspaceId, WorkspaceView,
};

/// A leaf layout holding pane `id`.
pub(crate) fn leaf(id: u64) -> Layout {
    Layout::Leaf(PaneId(id))
}

/// A half-and-half split of `first` and `second` along `direction`.
pub(crate) fn split(direction: Direction, first: Layout, second: Layout) -> Layout {
    Layout::Split {
        direction,
        ratio: 0.5,
        first: Box::new(first),
        second: Box::new(second),
    }
}

/// A pane with an explicit title, optional agent kind, and state.
pub(crate) fn pane(id: u64, title: &str, agent: Option<&str>, state: AgentState) -> PaneInfo {
    PaneInfo {
        id: PaneId(id),
        title: title.into(),
        agent: agent.map(Into::into),
        state,
        exited: None,
    }
}

/// A plain agentless shell whose title is its id, in the `Unknown` state — the
/// placeholder pane the layout fixtures fill with.
pub(crate) fn shell(id: u64) -> PaneInfo {
    pane(id, &id.to_string(), None, AgentState::Unknown)
}

/// An agent pane titled `pane-<id>`, matching the sidebar/agent fixtures.
pub(crate) fn agent(id: u64, kind: &str, state: AgentState) -> PaneInfo {
    pane(id, &format!("pane-{id}"), Some(kind), state)
}

/// A tab carrying `layout` and `panes`; its active pane is the first one.
pub(crate) fn tab(
    id: u64,
    name: &str,
    active: bool,
    layout: Layout,
    panes: Vec<PaneInfo>,
) -> TabView {
    let active_pane = panes.first().map(|p| p.id);
    TabView {
        id: TabId(id),
        name: name.into(),
        active,
        layout: Some(layout),
        active_pane,
        panes,
    }
}

/// A workspace rooted at `/tmp/w` with `name`, optional git `branch`, and `tabs`.
pub(crate) fn workspace(
    id: u64,
    name: &str,
    branch: Option<&str>,
    tabs: Vec<TabView>,
) -> WorkspaceView {
    WorkspaceView {
        id: WorkspaceId(id),
        name: name.into(),
        dir: PathBuf::from("/tmp/w"),
        branch: branch.map(Into::into),
        changes: None,
        stale: false,
        tabs,
    }
}

pub(crate) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub(crate) fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

pub(crate) fn plain(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
