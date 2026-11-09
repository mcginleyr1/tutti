//! Shared test fixtures for the attach TUI: compact constructors for the
//! workspace/tab/pane views the unit tests attach `App` to, and the key events
//! they drive it with. Every constructor mirrors the literals it replaces, so a
//! ported test keeps its exact assertions.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use tutti_core::{
    AgentState, Direction, Event, Frame as WireFrame, Layout, PaneId, PaneInfo, Request, Response,
    SubagentInfo, TabId, TabView, Tagged, WorkspaceId, WorkspaceView,
};

use super::app::App;
use crate::config::Config;

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
        subagents: Vec::new(),
        checkpoints: Vec::new(),
        approval: None,
        approvals: Vec::new(),
        source: None,
        sandbox: None,
        issue: None,
        conversation: None,
        sandbox_name: None,
        user_vars: Default::default(),
        cwd: None,
        workspace: None,
    }
}

/// A subagent row: `desc` and whether it is still running.
pub(crate) fn sub(desc: &str, running: bool) -> SubagentInfo {
    SubagentInfo {
        id: desc.into(),
        desc: desc.into(),
        running,
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
        changed_files: Vec::new(),
        stale: false,
        parent: None,
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

pub(crate) fn view_one_pane() -> Vec<WorkspaceView> {
    vec![workspace(
        1,
        "w",
        None,
        vec![tab(1, "1", true, leaf(1), vec![shell(1)])],
    )]
}

/// A reply as a current daemon sends it: tagged with its request's `req`.
pub(crate) fn reply(req: u64, response: Response) -> WireFrame {
    let tagged = Tagged {
        req: Some(req),
        body: response,
    };
    WireFrame::Control(serde_json::to_vec(&tagged).unwrap())
}

pub(crate) fn attached(app: &mut App) {
    app.handle_frame(reply(
        0,
        Response::Attached {
            wire_rev: tutti_core::WIRE_REV,
            session: "tutti".into(),
            workspaces: view_one_pane(),
            restorable: None,
            notes: Vec::new(),
        },
    ));
}

pub(crate) fn view_two_panes() -> Vec<WorkspaceView> {
    vec![workspace(
        1,
        "w",
        None,
        vec![tab(
            1,
            "1",
            true,
            split(Direction::Horizontal, leaf(1), leaf(2)),
            vec![shell(1), shell(2)],
        )],
    )]
}

pub(crate) fn state_changed(app: &mut App, pane: PaneId, from: AgentState, to: AgentState) {
    app.handle_frame(WireFrame::Control(
        serde_json::to_vec(&Event::StateChanged { pane, from, to }).unwrap(),
    ));
}

pub(crate) fn attach_with(app: &mut App, workspaces: Vec<WorkspaceView>) {
    app.handle_frame(reply(
        0,
        Response::Attached {
            wire_rev: tutti_core::WIRE_REV,
            session: "t".into(),
            workspaces,
            restorable: None,
            notes: Vec::new(),
        },
    ));
}

pub(crate) fn vim_config() -> Config {
    Config::parse("preset = \"vim\"\n").unwrap()
}

/// A 2x2 grid: left column stacks panes 1 (top) / 3 (bottom); right column
/// stacks 2 (top) / 4 (bottom).
pub(crate) fn view_2x2() -> Vec<WorkspaceView> {
    let column = |top, bottom| split(Direction::Vertical, leaf(top), leaf(bottom));
    let layout = split(Direction::Horizontal, column(1, 3), column(2, 4));
    vec![workspace(
        1,
        "w",
        None,
        vec![tab(1, "1", true, layout, (1..=4).map(shell).collect())],
    )]
}

pub(crate) fn view_two_tabs() -> Vec<WorkspaceView> {
    vec![workspace(
        1,
        "w",
        None,
        vec![
            tab(1, "1", true, leaf(1), vec![shell(1)]),
            tab(2, "2", false, leaf(2), vec![shell(2)]),
        ],
    )]
}

/// Workspace `api` (tab 1, active, one shell) and `web` (tab 2, two agents:
/// pane 2 working, pane 3 blocked).
pub(crate) fn view_two_workspaces() -> Vec<WorkspaceView> {
    vec![
        workspace(
            1,
            "api",
            Some("main"),
            vec![tab(1, "1", true, leaf(1), vec![shell(1)])],
        ),
        workspace(
            2,
            "web",
            None,
            vec![tab(
                2,
                "2",
                false,
                split(Direction::Horizontal, leaf(2), leaf(3)),
                vec![
                    agent(2, "claude", AgentState::Working),
                    agent(3, "claude", AgentState::Blocked),
                ],
            )],
        ),
    ]
}

pub(crate) fn focus_sidebar(app: &mut App) {
    app.sync_sizes(Rect::new(0, 0, 100, 24));
    app.on_key(ctrl('b'));
    app.on_key(plain('w'));
}

pub(crate) fn layout_changed(app: &mut App, workspaces: Vec<WorkspaceView>) {
    app.handle_frame(WireFrame::Control(
        serde_json::to_vec(&Event::LayoutChanged { workspaces }).unwrap(),
    ));
}

/// Decode the single control frame in `out` into a request, for asserting the
/// shape of a submitted `WorkspaceFork` whose `dest` is environment-derived.
pub(crate) fn only_request(out: &[WireFrame]) -> Request {
    match out {
        [WireFrame::Control(bytes)] => serde_json::from_slice(bytes).unwrap(),
        other => panic!("expected exactly one control frame, got {other:?}"),
    }
}

pub(crate) fn event(app: &mut App, event: Event) {
    app.handle_frame(WireFrame::Control(serde_json::to_vec(&event).unwrap()));
}

pub(crate) fn shift(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}
