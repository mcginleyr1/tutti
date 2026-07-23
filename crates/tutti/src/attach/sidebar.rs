//! The workspace/agent sidebar model: pure functions that turn the client's
//! `WorkspaceView` list into the two stacked sections the sidebar shows, plus
//! the row arithmetic mapping a click to an entry. No rendering, no state, so
//! the sections, sort order, and hit-testing are unit-tested in isolation.

use tutti_core::{AgentState, PaneId, SubagentInfo, TabId, WorkspaceView};

/// One selectable sidebar row: a workspace (jumps to a tab) or an agent pane
/// (jumps to a tab and focuses the pane).
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarEntry {
    Workspace(WorkspaceRow),
    Agent(AgentRow),
}

/// The two collapsible sections. Their headers live in the sidebar frame (the
/// top border and the fused divider); clicking one toggles that section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Projects,
    Agents,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRow {
    pub name: String,
    /// The git branch, when known — the dim second line. `None` renders a blank
    /// line rather than repeating the name (the workspace name is already the
    /// directory basename, so echoing it as a subtitle read as a bug).
    pub subtitle: Option<String>,
    /// A short jj change stat (`4 files +120 −33`), right-aligned on the subtitle
    /// line. `None` when the workspace is not a jj repo or has no changes.
    pub changes: Option<String>,
    /// Whether the workspace's jj working copy is stale (needs
    /// `workspace update`). Surfaced as a dim-red tag that wins over the stat.
    pub stale: bool,
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
    /// Hook-reported subagents, rendered as dim indented sub-rows under this
    /// agent. Display-only: they are never selectable, but they add height, so
    /// `entry_at_row` accounts for them.
    pub subagents: Vec<SubagentInfo>,
}

/// The sidebar's contents: workspace rows then agent rows. `workspace_count`
/// records where the agents section begins so the renderer and the hit-test
/// agree on row layout. `projects_collapsed`/`agents_collapsed` hide a section's
/// rows down to its header, set by the client from its collapse state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sidebar {
    pub entries: Vec<SidebarEntry>,
    pub workspace_count: usize,
    pub projects_collapsed: bool,
    pub agents_collapsed: bool,
}

impl Sidebar {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether entry `idx` is currently visible (its section is expanded).
    pub fn is_visible(&self, idx: usize) -> bool {
        if idx < self.workspace_count {
            !self.projects_collapsed
        } else {
            !self.agents_collapsed
        }
    }

    /// The screen row (relative to the sidebar frame's top) of the agents
    /// divider — the border-fused `agents` header. Row 0 is always the projects
    /// header (the top border); the divider follows the workspace rows unless the
    /// projects section is collapsed.
    fn divider_row(&self) -> usize {
        if self.projects_collapsed {
            1
        } else {
            1 + 2 * self.workspace_count
        }
    }

    /// The section header (if any) a click at `row` toggles. Row 0 is the
    /// projects header in the top border; the agents header is the fused divider.
    pub fn header_at_row(&self, row: usize) -> Option<Section> {
        if row == 0 {
            Some(Section::Projects)
        } else if row == self.divider_row() {
            Some(Section::Agents)
        } else {
            None
        }
    }

    /// The entry a click at `row` (relative to the sidebar frame's top) selects,
    /// or `None` for a border, a header, a subagent sub-row, a collapsed section,
    /// or empty space. The layout mirrors the renderer: the projects header (top
    /// border, row 0), two rows per workspace, the agents header (fused divider),
    /// then — per agent — its two rows plus one dim sub-row per subagent (a click
    /// on which selects nothing).
    pub fn entry_at_row(&self, row: usize) -> Option<usize> {
        if !self.projects_collapsed {
            let ws_start = 1; // right below the projects header (top border)
            let ws_end = ws_start + 2 * self.workspace_count;
            if (ws_start..ws_end).contains(&row) {
                return Some((row - ws_start) / 2);
            }
        }
        if self.agents_collapsed {
            return None;
        }
        let agents_start = self.divider_row() + 1;
        if row < agents_start {
            return None;
        }
        // Each agent block is its two rows followed by its subagent sub-rows; a
        // click on the two head rows selects the agent, one on a sub-row does not.
        let mut cursor = agents_start;
        for (idx, entry) in self.entries.iter().enumerate().skip(self.workspace_count) {
            let SidebarEntry::Agent(a) = entry else {
                continue;
            };
            if row == cursor || row == cursor + 1 {
                return Some(idx);
            }
            cursor += 2 + a.subagents.len();
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
                subtitle: w
                    .branch
                    .clone()
                    .or_else(|| shorten_home(&w.dir, std::env::var_os("HOME").map(Into::into))),
                changes: w.changes.clone(),
                stale: w.stale,
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
                        subagents: pane.subagents.clone(),
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
        projects_collapsed: false,
        agents_collapsed: false,
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

/// `~`-shorten `dir` for the subtitle line; `None` for an empty path.
fn shorten_home(dir: &std::path::Path, home: Option<std::path::PathBuf>) -> Option<String> {
    if dir.as_os_str().is_empty() {
        return None;
    }
    let text = match home.and_then(|h| dir.strip_prefix(h).map(|r| r.to_owned()).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::{agent, leaf, pane, split, sub, tab, workspace};
    use tutti_core::Direction;

    /// One workspace (active tab) with two agents; the first (Working, so sorted
    /// ahead of the second's Done) carries two subagents — one running, one done.
    fn view_with_subagents() -> Vec<WorkspaceView> {
        let mut first = agent(1, "claude", AgentState::Working);
        first.subagents = vec![sub("build core", true), sub("write tests", false)];
        vec![workspace(
            1,
            "api",
            Some("main"),
            vec![tab(
                1,
                "1",
                true,
                split(Direction::Horizontal, leaf(1), leaf(2)),
                vec![first, agent(2, "codex", AgentState::Done)],
            )],
        )]
    }

    /// Two workspaces: `api` (tab 1) with a working agent and a plain shell;
    /// `web` (tab 2, the active tab) with a blocked agent and a done agent.
    fn two_workspace_view() -> Vec<WorkspaceView> {
        vec![
            workspace(
                1,
                "api",
                Some("main"),
                vec![tab(
                    1,
                    "1",
                    false,
                    split(Direction::Horizontal, leaf(1), leaf(2)),
                    vec![
                        agent(1, "claude", AgentState::Working),
                        pane(2, "pane-2", None, AgentState::Idle),
                    ],
                )],
            ),
            workspace(
                2,
                "web",
                None,
                vec![tab(
                    2,
                    "2",
                    true,
                    split(Direction::Horizontal, leaf(3), leaf(4)),
                    vec![
                        agent(3, "codex", AgentState::Done),
                        agent(4, "claude", AgentState::Blocked),
                    ],
                )],
            ),
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
        assert_eq!(
            api.subtitle.as_deref(),
            Some("main"),
            "branch drives the subtitle"
        );
        assert!(web.active, "web owns the active tab");
        assert_eq!(web.jump_tab, TabId(2));
        assert_eq!(
            web.subtitle.as_deref(),
            Some("/tmp/w"),
            "no branch falls back to the dir, never the name"
        );
    }

    #[test]
    fn build_carries_the_workspace_change_stat() {
        let mut view = two_workspace_view();
        view[0].changes = Some("2 files +5 −0".into());
        let sidebar = build(&view, Some(TabId(2)));
        let SidebarEntry::Workspace(api) = &sidebar.entries[0] else {
            panic!("expected workspace row");
        };
        let SidebarEntry::Workspace(web) = &sidebar.entries[1] else {
            panic!("expected workspace row");
        };
        assert_eq!(
            api.changes.as_deref(),
            Some("2 files +5 −0"),
            "the stat rides onto the row"
        );
        assert_eq!(web.changes, None, "a workspace without a stat stays quiet");
    }

    #[test]
    fn build_carries_the_workspace_stale_flag() {
        let mut view = two_workspace_view();
        view[0].stale = true;
        let sidebar = build(&view, Some(TabId(2)));
        let SidebarEntry::Workspace(api) = &sidebar.entries[0] else {
            panic!("expected workspace row");
        };
        let SidebarEntry::Workspace(web) = &sidebar.entries[1] else {
            panic!("expected workspace row");
        };
        assert!(api.stale, "the stale flag rides onto the row");
        assert!(!web.stale, "a healthy workspace is not stale");
    }

    #[test]
    fn shorten_home_prefers_tilde_and_skips_empty_dirs() {
        use std::path::{Path, PathBuf};
        let home = || Some(PathBuf::from("/Users/me"));
        assert_eq!(
            shorten_home(Path::new("/Users/me/develop/x"), home()),
            Some("~/develop/x".into())
        );
        assert_eq!(
            shorten_home(Path::new("/Users/me"), home()),
            Some("~".into())
        );
        assert_eq!(
            shorten_home(Path::new("/srv/data"), home()),
            Some("/srv/data".into())
        );
        assert_eq!(shorten_home(Path::new(""), home()), None);
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
    fn entry_at_row_maps_clicks_past_the_frame_headers() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        // Row 0 is the projects header (the top border) — a section toggle, not
        // an entry.
        assert_eq!(sidebar.entry_at_row(0), None);
        assert_eq!(sidebar.header_at_row(0), Some(Section::Projects));
        // Rows 1-2 are workspace 0, rows 3-4 workspace 1.
        assert_eq!(sidebar.entry_at_row(1), Some(0));
        assert_eq!(sidebar.entry_at_row(2), Some(0));
        assert_eq!(sidebar.entry_at_row(3), Some(1));
        assert_eq!(sidebar.entry_at_row(4), Some(1));
        // Row 5 is the agents divider (fused header) — a toggle, not an entry.
        assert_eq!(sidebar.entry_at_row(5), None);
        assert_eq!(sidebar.header_at_row(5), Some(Section::Agents));
        // Rows 6-7 first agent (entry index 2), 8-9 second, 10-11 third.
        assert_eq!(sidebar.entry_at_row(6), Some(2));
        assert_eq!(sidebar.entry_at_row(9), Some(3));
        assert_eq!(sidebar.entry_at_row(11), Some(4));
        // Past the last agent.
        assert_eq!(sidebar.entry_at_row(12), None);
    }

    #[test]
    fn collapsing_projects_shifts_the_agents_divider_up() {
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)));
        sidebar.projects_collapsed = true;
        // With projects collapsed, no workspace rows: the divider is row 1 and
        // the workspace rows are gone.
        assert_eq!(sidebar.entry_at_row(1), None, "workspace rows are hidden");
        assert_eq!(sidebar.header_at_row(1), Some(Section::Agents));
        // Agents follow immediately after the divider.
        assert_eq!(sidebar.entry_at_row(2), Some(2));
        assert_eq!(sidebar.entry_at_row(3), Some(2));
        assert_eq!(sidebar.entry_at_row(4), Some(3));
    }

    #[test]
    fn collapsing_agents_hides_every_agent_row() {
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)));
        sidebar.agents_collapsed = true;
        // Workspaces still map; agent rows past the divider select nothing.
        assert_eq!(sidebar.entry_at_row(1), Some(0));
        assert_eq!(sidebar.header_at_row(5), Some(Section::Agents));
        assert_eq!(sidebar.entry_at_row(6), None, "agent rows are hidden");
        assert!(!sidebar.is_visible(2), "an agent entry is not visible");
        assert!(sidebar.is_visible(0), "a workspace entry stays visible");
    }

    #[test]
    fn build_carries_subagents_onto_the_agent_row() {
        let sidebar = build(&view_with_subagents(), Some(TabId(1)));
        let SidebarEntry::Agent(a) = &sidebar.entries[1] else {
            panic!("expected the first agent row");
        };
        assert_eq!(a.subagents.len(), 2);
        assert_eq!(a.subagents[0].desc, "build core");
        assert!(a.subagents[0].running, "the first subagent is running");
        assert!(!a.subagents[1].running, "the second subagent has finished");
    }

    #[test]
    fn entry_at_row_skips_subagent_rows_and_shifts_the_next_agent_down() {
        let sidebar = build(&view_with_subagents(), Some(TabId(1)));
        // One workspace: projects header row 0, workspace rows 1-2, agents
        // divider row 3.
        assert_eq!(sidebar.entry_at_row(1), Some(0), "the workspace row");
        assert_eq!(sidebar.header_at_row(3), Some(Section::Agents));
        // Agent entry 1 (with two subagents) occupies rows 4-5 as its head.
        assert_eq!(sidebar.entry_at_row(4), Some(1));
        assert_eq!(sidebar.entry_at_row(5), Some(1));
        // Its two subagent sub-rows (6-7) are display-only: not selectable.
        assert_eq!(sidebar.entry_at_row(6), None);
        assert_eq!(sidebar.entry_at_row(7), None);
        // The next agent is pushed down past the sub-rows, to 8-9.
        assert_eq!(sidebar.entry_at_row(8), Some(2));
        assert_eq!(sidebar.entry_at_row(9), Some(2));
        assert_eq!(sidebar.entry_at_row(10), None, "past the last agent");
    }
}
