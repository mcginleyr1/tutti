//! The workspace/agent sidebar model: pure functions that turn the client's
//! `WorkspaceView` list into the two stacked sections the sidebar shows, plus
//! the row arithmetic mapping a click to an entry. No rendering, no state, so
//! the sections, sort order, and hit-testing are unit-tested in isolation.

use tutti_core::{AgentState, PaneId, TabId, WorkspaceView};

/// One selectable sidebar row: a workspace (jumps to a tab) or an agent pane
/// (jumps to a tab and focuses the pane).
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarEntry {
    Workspace(WorkspaceRow),
    Agent(AgentRow),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRow {
    pub name: String,
    /// Branch when known, else the workspace name (its directory's last path
    /// component) — the dim second line.
    pub subtitle: String,
    /// Whether this workspace owns the active tab (rendered bold).
    pub active: bool,
    /// The tab that selecting this workspace jumps to.
    pub jump_tab: TabId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub pane: PaneId,
    pub tab: TabId,
    pub title: String,
    pub state: AgentState,
    pub kind: String,
}

/// The sidebar's contents: workspace rows then agent rows. `workspace_count`
/// records where the agents section begins so the renderer and the hit-test
/// agree on row layout.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sidebar {
    pub entries: Vec<SidebarEntry>,
    pub workspace_count: usize,
}

impl Sidebar {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entry a click at `row` (relative to the sidebar's inner top) selects,
    /// or `None` for a header, the section gap, or empty space. The layout is:
    /// header at row 0, then two rows per workspace, a blank, a header, then two
    /// rows per agent.
    pub fn entry_at_row(&self, row: usize) -> Option<usize> {
        let ws_start = 1;
        let ws_end = ws_start + 2 * self.workspace_count;
        if (ws_start..ws_end).contains(&row) {
            return Some((row - ws_start) / 2);
        }
        let agents_start = ws_end + 2; // blank line + AGENTS header
        let agents_end = agents_start + 2 * (self.entries.len() - self.workspace_count);
        if (agents_start..agents_end).contains(&row) {
            return Some(self.workspace_count + (row - agents_start) / 2);
        }
        None
    }
}

/// Build the sidebar from the client's view. `active_tab` decides which
/// workspace is bold and where each workspace jump lands. Agents are gathered
/// across every workspace and ordered blocked → working → done → idle →
/// unknown, stable by pane id within a group.
pub fn build(workspaces: &[WorkspaceView], active_tab: Option<TabId>) -> Sidebar {
    let mut entries = Vec::new();
    let mut agents = Vec::new();
    for w in workspaces {
        let owns = active_tab.is_some_and(|at| w.tabs.iter().any(|t| t.id == at));
        let jump_tab = active_tab
            .filter(|at| w.tabs.iter().any(|t| t.id == *at))
            .or_else(|| w.tabs.first().map(|t| t.id));
        if let Some(jump_tab) = jump_tab {
            entries.push(SidebarEntry::Workspace(WorkspaceRow {
                name: w.name.clone(),
                subtitle: w.branch.clone().unwrap_or_else(|| w.name.clone()),
                active: owns,
                jump_tab,
            }));
        }
        for tab in &w.tabs {
            for pane in &tab.panes {
                if let Some(agent) = &pane.agent {
                    agents.push(AgentRow {
                        pane: pane.id,
                        tab: tab.id,
                        title: pane.title.clone(),
                        state: pane.state,
                        kind: agent.to_string(),
                    });
                }
            }
        }
    }
    agents.sort_by_key(|a| (state_rank(a.state), a.pane.0));
    let workspace_count = entries.len();
    entries.extend(agents.into_iter().map(SidebarEntry::Agent));
    Sidebar {
        entries,
        workspace_count,
    }
}

/// Attention order: blocked first, then working, done, idle, unknown.
fn state_rank(state: AgentState) -> u8 {
    match state {
        AgentState::Blocked => 0,
        AgentState::Working => 1,
        AgentState::Done => 2,
        AgentState::Idle => 3,
        AgentState::Unknown => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{Direction, Layout, PaneInfo, TabView, WorkspaceId};

    fn pane(id: u64, agent: Option<&str>, state: AgentState) -> PaneInfo {
        PaneInfo {
            id: PaneId(id),
            title: format!("pane-{id}"),
            agent: agent.map(Into::into),
            state,
            exited: None,
        }
    }

    /// Two workspaces: `api` (tab 1) with a working agent and a plain shell;
    /// `web` (tab 2, the active tab) with a blocked agent and a done agent.
    fn two_workspace_view() -> Vec<WorkspaceView> {
        vec![
            WorkspaceView {
                id: WorkspaceId(1),
                name: "api".into(),
                branch: Some("main".into()),
                tabs: vec![TabView {
                    id: TabId(1),
                    name: "1".into(),
                    active: false,
                    layout: Some(Layout::Split {
                        direction: Direction::Horizontal,
                        ratio: 0.5,
                        first: Box::new(Layout::Leaf(PaneId(1))),
                        second: Box::new(Layout::Leaf(PaneId(2))),
                    }),
                    active_pane: Some(PaneId(1)),
                    panes: vec![
                        pane(1, Some("claude"), AgentState::Working),
                        pane(2, None, AgentState::Idle),
                    ],
                }],
            },
            WorkspaceView {
                id: WorkspaceId(2),
                name: "web".into(),
                branch: None,
                tabs: vec![TabView {
                    id: TabId(2),
                    name: "2".into(),
                    active: true,
                    layout: Some(Layout::Split {
                        direction: Direction::Horizontal,
                        ratio: 0.5,
                        first: Box::new(Layout::Leaf(PaneId(3))),
                        second: Box::new(Layout::Leaf(PaneId(4))),
                    }),
                    active_pane: Some(PaneId(3)),
                    panes: vec![
                        pane(3, Some("codex"), AgentState::Done),
                        pane(4, Some("claude"), AgentState::Blocked),
                    ],
                }],
            },
        ]
    }

    #[test]
    fn build_lists_a_row_per_workspace_then_agents_only() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        assert_eq!(sidebar.workspace_count, 2);
        // Two workspaces + three agent panes (pane 2 has no agent, excluded).
        assert_eq!(sidebar.len(), 5);
    }

    #[test]
    fn build_marks_the_active_workspace_and_jump_targets() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let SidebarEntry::Workspace(api) = &sidebar.entries[0] else {
            panic!("expected workspace row");
        };
        let SidebarEntry::Workspace(web) = &sidebar.entries[1] else {
            panic!("expected workspace row");
        };
        assert!(!api.active, "api does not own the active tab");
        assert_eq!(
            api.jump_tab,
            TabId(1),
            "inactive workspace jumps to its tab"
        );
        assert_eq!(api.subtitle, "main", "branch drives the subtitle");
        assert!(web.active, "web owns the active tab");
        assert_eq!(web.jump_tab, TabId(2));
        assert_eq!(web.subtitle, "web", "no branch falls back to the name");
    }

    #[test]
    fn build_orders_agents_blocked_first_then_by_pane_id() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let agents: Vec<(&str, u64)> = sidebar.entries[sidebar.workspace_count..]
            .iter()
            .map(|e| match e {
                SidebarEntry::Agent(a) => (a.kind.as_str(), a.pane.0),
                _ => panic!("expected agent row"),
            })
            .collect();
        // pane 4 blocked, pane 1 working, pane 3 done.
        assert_eq!(agents, vec![("claude", 4), ("claude", 1), ("codex", 3)]);
    }

    #[test]
    fn agent_rows_carry_tab_and_pane_jump_targets() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let SidebarEntry::Agent(blocked) = &sidebar.entries[2] else {
            panic!("expected agent row");
        };
        assert_eq!(blocked.pane, PaneId(4));
        assert_eq!(blocked.tab, TabId(2), "agent jump carries its owning tab");
        assert_eq!(blocked.state, AgentState::Blocked);
    }

    #[test]
    fn entry_at_row_maps_clicks_past_headers_and_the_gap() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        // Row 0 is the WORKSPACES header.
        assert_eq!(sidebar.entry_at_row(0), None);
        // Rows 1-2 are workspace 0, rows 3-4 workspace 1.
        assert_eq!(sidebar.entry_at_row(1), Some(0));
        assert_eq!(sidebar.entry_at_row(2), Some(0));
        assert_eq!(sidebar.entry_at_row(3), Some(1));
        // Row 5 blank, row 6 AGENTS header — neither selectable.
        assert_eq!(sidebar.entry_at_row(5), None);
        assert_eq!(sidebar.entry_at_row(6), None);
        // Rows 7-8 first agent (entry index 2), 9-10 second, 11-12 third.
        assert_eq!(sidebar.entry_at_row(7), Some(2));
        assert_eq!(sidebar.entry_at_row(10), Some(3));
        assert_eq!(sidebar.entry_at_row(12), Some(4));
        // Past the last agent.
        assert_eq!(sidebar.entry_at_row(13), None);
    }
}
