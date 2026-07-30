//! The workspace/agent sidebar model: pure functions that turn the client's
//! `WorkspaceView` list into the three stacked sections the sidebar shows —
//! projects, the selected project's agents, and the cross-project waiting
//! queue — plus the row arithmetic mapping a click to an entry. No rendering,
//! no state, so the sections, sort order, and hit-testing are unit-tested in
//! isolation.

use tutti_core::{AgentState, PaneId, SubagentInfo, TabId, WorkspaceId, WorkspaceView};

/// One selectable sidebar row: a workspace (jumps to a tab) or an agent pane
/// (jumps to a tab and focuses the pane).
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarEntry {
    Workspace(WorkspaceRow),
    Agent(AgentRow),
}

/// The three collapsible sections. Their headers live in the sidebar frame (the
/// top border and the fused dividers); clicking one toggles that section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Projects,
    Agents,
    Waiting,
}

/// A selectable row's stable identity, independent of its position. The waiting
/// copy of an agent is distinct from its agents-section row so a cursor in the
/// queue re-anchors to the queue, not to the first duplicate, across rebuilds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryIdent {
    Workspace(WorkspaceId),
    Agent(PaneId),
    Waiting(PaneId),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRow {
    /// The workspace this row is — the identity the cursor anchors to.
    pub id: WorkspaceId,
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
    /// The top-level project this row belongs to: its parent for a nested
    /// jj-workspace child, itself for a project row. Highlighting the row makes
    /// this the agents-section filter.
    pub project: WorkspaceId,
    /// The tree-guide glyph when this row is a jj-workspace child nested under a
    /// project: `├` for a non-last sibling, `└` for the last. `None` for a
    /// top-level project row (drawn flush-left, no guide). Its presence is what
    /// tells the client this row can be merged.
    pub guide: Option<char>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub pane: PaneId,
    pub tab: TabId,
    pub title: String,
    pub state: AgentState,
    pub kind: String,
    /// The top-level project owning this agent's workspace, and its display
    /// name. The filter key for the agents section; the waiting section shows
    /// the name so a cross-project row says where it lives.
    pub project: WorkspaceId,
    pub project_name: String,
    /// Hook-reported subagents, rendered as dim indented sub-rows under this
    /// agent. Display-only: they are never selectable, but they add height, so
    /// `entry_at_row` accounts for them. Always empty on waiting-section rows,
    /// which stay two rows tall.
    pub subagents: Vec<SubagentInfo>,
}

/// The sidebar's contents: workspace rows, then the selected project's agent
/// rows, then the cross-project waiting rows. `workspace_count` and
/// `agent_count` record where each section begins so the renderer and the
/// hit-test agree on row layout. The `*_collapsed` flags hide a section's rows
/// down to its header, set by the client from its collapse state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sidebar {
    pub entries: Vec<SidebarEntry>,
    pub workspace_count: usize,
    pub agent_count: usize,
    /// The display name of the project the agents section is filtered to, when
    /// one is selected and still in the view — rendered into the agents header.
    pub project: Option<String>,
    pub projects_collapsed: bool,
    pub agents_collapsed: bool,
    pub waiting_collapsed: bool,
}

impl Sidebar {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The stable identity of entry `idx`, or `None` past the end. Agent
    /// entries at or past the waiting boundary identify as `Waiting`.
    pub fn ident_at(&self, idx: usize) -> Option<EntryIdent> {
        Some(match self.entries.get(idx)? {
            SidebarEntry::Workspace(w) => EntryIdent::Workspace(w.id),
            SidebarEntry::Agent(a) if idx >= self.workspace_count + self.agent_count => {
                EntryIdent::Waiting(a.pane)
            }
            SidebarEntry::Agent(a) => EntryIdent::Agent(a.pane),
        })
    }

    /// The index currently holding `ident`, if the entry is still in the view.
    pub fn index_of(&self, ident: EntryIdent) -> Option<usize> {
        (0..self.entries.len()).find(|&i| self.ident_at(i) == Some(ident))
    }

    /// Whether entry `idx` is currently visible (its section is expanded).
    pub fn is_visible(&self, idx: usize) -> bool {
        if idx < self.workspace_count {
            !self.projects_collapsed
        } else if idx < self.workspace_count + self.agent_count {
            !self.agents_collapsed
        } else {
            !self.waiting_collapsed
        }
    }

    /// The screen row (relative to the sidebar frame's top) of the agents
    /// divider — the border-fused `agents` header. Row 0 is always the projects
    /// header (the top border); the divider follows the workspace rows unless the
    /// projects section is collapsed.
    fn agents_divider_row(&self) -> usize {
        if self.projects_collapsed {
            1
        } else {
            1 + 2 * self.workspace_count
        }
    }

    /// How many rows the agents section body occupies: none collapsed, one for
    /// the placeholder line when the filter leaves no agents, else two per agent
    /// plus its subagent sub-rows. Mirrors the renderer exactly — the waiting
    /// divider's position depends on it.
    fn agents_body_rows(&self) -> usize {
        if self.agents_collapsed {
            return 0;
        }
        if self.agent_count == 0 {
            return 1; // the `no agents here` placeholder line
        }
        self.entries
            .iter()
            .skip(self.workspace_count)
            .take(self.agent_count)
            .map(|e| match e {
                SidebarEntry::Agent(a) => 2 + a.subagents.len(),
                SidebarEntry::Workspace(_) => 0,
            })
            .sum()
    }

    /// The screen row of the waiting divider — the second fused header, after
    /// the agents section body.
    fn waiting_divider_row(&self) -> usize {
        self.agents_divider_row() + 1 + self.agents_body_rows()
    }

    /// The section header (if any) a click at `row` toggles. Row 0 is the
    /// projects header in the top border; the agents and waiting headers are the
    /// fused dividers.
    pub fn header_at_row(&self, row: usize) -> Option<Section> {
        if row == 0 {
            Some(Section::Projects)
        } else if row == self.agents_divider_row() {
            Some(Section::Agents)
        } else if row == self.waiting_divider_row() {
            Some(Section::Waiting)
        } else {
            None
        }
    }

    /// The entry a click at `row` (relative to the sidebar frame's top) selects,
    /// or `None` for a border, a header, a subagent sub-row, a collapsed section,
    /// or empty space. The layout mirrors the renderer: the projects header (top
    /// border, row 0), two rows per workspace, the agents header (fused divider),
    /// per agent its two rows plus one dim sub-row per subagent (a click on
    /// which selects nothing), then the waiting header (second divider) and two
    /// rows per waiting entry.
    pub fn entry_at_row(&self, row: usize) -> Option<usize> {
        if !self.projects_collapsed {
            let ws_start = 1; // right below the projects header (top border)
            let ws_end = ws_start + 2 * self.workspace_count;
            if (ws_start..ws_end).contains(&row) {
                return Some((row - ws_start) / 2);
            }
        }
        // Each agent block is its two rows followed by its subagent sub-rows; a
        // click on the two head rows selects the agent, one on a sub-row does not.
        let agents_start = self.agents_divider_row() + 1;
        if !self.agents_collapsed && row >= agents_start {
            let mut cursor = agents_start;
            for (idx, entry) in self
                .entries
                .iter()
                .enumerate()
                .skip(self.workspace_count)
                .take(self.agent_count)
            {
                let SidebarEntry::Agent(a) = entry else {
                    continue;
                };
                if row == cursor || row == cursor + 1 {
                    return Some(idx);
                }
                cursor += 2 + a.subagents.len();
            }
        }
        let waiting_start = self.waiting_divider_row() + 1;
        if self.waiting_collapsed || row < waiting_start {
            return None;
        }
        let idx = self.workspace_count + self.agent_count + (row - waiting_start) / 2;
        (idx < self.entries.len()).then_some(idx)
    }
}

/// Build the sidebar from the client's view. `active_tab` decides which
/// workspace is bold and where each workspace jump lands. Workspaces are ordered
/// parent→children: each top-level project is followed immediately by its
/// jj-workspace children (indented, `├`/`└` guides), so a child renders nested
/// beneath its origin. The agents section holds only `project`'s agents (every
/// agent when `None`), ordered blocked → working → done → idle → unknown,
/// stable by pane id within a group. The waiting section gathers blocked and
/// done agents across every project — the cross-project attention queue —
/// blocked first, subagents stripped so each row stays two rows tall.
pub fn build(
    workspaces: &[WorkspaceView],
    active_tab: Option<TabId>,
    project: Option<WorkspaceId>,
) -> Sidebar {
    let present: std::collections::HashSet<_> = workspaces.iter().map(|w| w.id).collect();
    // A workspace nests only when its parent is actually present; a child whose
    // origin was killed renders as a top-level project.
    let is_child = |w: &WorkspaceView| w.parent.is_some_and(|p| present.contains(&p));
    let project_of = |w: &WorkspaceView| w.parent.filter(|p| present.contains(p)).unwrap_or(w.id);

    let row = |w: &WorkspaceView, guide: Option<char>| -> Option<SidebarEntry> {
        let owns = active_tab.is_some_and(|at| w.tabs.iter().any(|t| t.id == at));
        let jump_tab = active_tab
            .filter(|at| w.tabs.iter().any(|t| t.id == *at))
            .or_else(|| w.tabs.first().map(|t| t.id))?;
        Some(SidebarEntry::Workspace(WorkspaceRow {
            id: w.id,
            name: w.name.clone(),
            subtitle: w
                .branch
                .clone()
                .or_else(|| shorten_home(&w.dir, std::env::var_os("HOME").map(Into::into))),
            changes: w.changes.clone(),
            stale: w.stale,
            active: owns,
            jump_tab,
            project: project_of(w),
            guide,
        }))
    };

    let mut entries = Vec::new();
    for w in workspaces {
        if is_child(w) {
            continue; // emitted under its parent below
        }
        entries.extend(row(w, None));
        let children: Vec<&WorkspaceView> = workspaces
            .iter()
            .filter(|c| c.parent == Some(w.id))
            .collect();
        let last = children.len();
        for (i, child) in children.iter().enumerate() {
            let glyph = if i + 1 == last { '└' } else { '├' };
            entries.extend(row(child, Some(glyph)));
        }
    }
    let workspace_count = entries.len();

    let name_of = |id: WorkspaceId| {
        workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.name.clone())
            .unwrap_or_default()
    };
    let mut all = Vec::new();
    for w in workspaces {
        let project = project_of(w);
        for tab in &w.tabs {
            for pane in &tab.panes {
                if let Some(agent) = &pane.agent {
                    all.push(AgentRow {
                        pane: pane.id,
                        tab: tab.id,
                        title: pane.title.clone(),
                        state: pane.state,
                        kind: agent.to_string(),
                        project,
                        project_name: name_of(project),
                        subagents: pane.subagents.clone(),
                    });
                }
            }
        }
    }

    let mut agents: Vec<AgentRow> = all
        .iter()
        .filter(|a| project.is_none_or(|p| a.project == p))
        .cloned()
        .collect();
    agents.sort_by_key(|a| (state_rank(a.state), a.pane.0));
    let agent_count = agents.len();
    entries.extend(agents.into_iter().map(SidebarEntry::Agent));

    let mut waiting: Vec<AgentRow> = all
        .into_iter()
        .filter(|a| matches!(a.state, AgentState::Blocked | AgentState::Done))
        .map(|a| AgentRow {
            subagents: Vec::new(),
            ..a
        })
        .collect();
    waiting.sort_by_key(|a| (state_rank(a.state), a.pane.0));
    entries.extend(waiting.into_iter().map(SidebarEntry::Agent));

    Sidebar {
        entries,
        workspace_count,
        agent_count,
        project: project.map(name_of).filter(|n| !n.is_empty()),
        projects_collapsed: false,
        agents_collapsed: false,
        waiting_collapsed: false,
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
    use tutti_core::{Direction, WorkspaceId};

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
    fn build_lists_a_row_per_workspace_then_agents_then_waiting() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        assert_eq!(sidebar.workspace_count, 2);
        // Three agent panes (pane 2 has no agent, excluded) — no filter.
        assert_eq!(sidebar.agent_count, 3);
        // Plus the waiting section: pane 4 (blocked) and pane 3 (done).
        assert_eq!(sidebar.len(), 7);
    }

    #[test]
    fn build_filters_agents_to_the_selected_project() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), Some(WorkspaceId(1)));
        // Workspace 1 owns only the working claude (pane 1).
        assert_eq!(sidebar.agent_count, 1);
        let SidebarEntry::Agent(a) = &sidebar.entries[2] else {
            panic!("expected the filtered agent row");
        };
        assert_eq!(a.pane, PaneId(1));
        assert_eq!(sidebar.project.as_deref(), Some("api"));
        // The waiting section stays cross-project: web's blocked + done agents.
        let waiting: Vec<u64> = sidebar.entries[3..]
            .iter()
            .map(|e| match e {
                SidebarEntry::Agent(a) => a.pane.0,
                _ => panic!("expected waiting agent rows"),
            })
            .collect();
        assert_eq!(waiting, vec![4, 3], "blocked first, then done");
    }

    #[test]
    fn build_filters_a_child_workspace_agent_under_its_parent_project() {
        let mut view = two_workspace_view();
        view[1].parent = Some(WorkspaceId(1)); // web is a child of api
        let sidebar = build(&view, Some(TabId(2)), Some(WorkspaceId(1)));
        // Filtering by the parent project keeps the child's agents too.
        assert_eq!(sidebar.agent_count, 3);
        let SidebarEntry::Agent(a) = &sidebar.entries[2] else {
            panic!("expected an agent row");
        };
        assert_eq!(a.project, WorkspaceId(1), "child agents roll up to api");
    }

    #[test]
    fn waiting_rows_carry_the_project_name_and_no_subagents() {
        let mut view = view_with_subagents();
        view[0].tabs[0].panes[0].state = AgentState::Blocked; // subagent carrier
        let sidebar = build(&view, Some(TabId(1)), None);
        let waiting_start = sidebar.workspace_count + sidebar.agent_count;
        let SidebarEntry::Agent(a) = &sidebar.entries[waiting_start] else {
            panic!("expected a waiting row");
        };
        assert_eq!(a.pane, PaneId(1), "the blocked agent leads the queue");
        assert_eq!(a.project_name, "api");
        assert!(
            a.subagents.is_empty(),
            "waiting rows strip subagents to stay two rows tall"
        );
    }

    #[test]
    fn build_marks_the_active_workspace_and_jump_targets() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        let sidebar = build(&view, Some(TabId(2)), None);
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
        let sidebar = build(&view, Some(TabId(2)), None);
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
    fn build_nests_a_child_workspace_under_its_parent() {
        let mut view = two_workspace_view();
        // Make `web` (id 2) a jj-workspace child of `api` (id 1).
        view[1].parent = Some(WorkspaceId(1));
        let sidebar = build(&view, Some(TabId(2)), None);
        // Both stay workspace entries, api then its child web, in that order.
        assert_eq!(sidebar.workspace_count, 2);
        let SidebarEntry::Workspace(api) = &sidebar.entries[0] else {
            panic!("expected the parent workspace row");
        };
        let SidebarEntry::Workspace(web) = &sidebar.entries[1] else {
            panic!("expected the nested child row");
        };
        assert_eq!(api.name, "api");
        assert_eq!(api.guide, None, "the parent project is flush-left");
        assert_eq!(web.name, "web");
        assert_eq!(web.guide, Some('└'), "the only child gets the last guide");
    }

    #[test]
    fn build_marks_all_but_the_last_child_with_the_mid_guide() {
        let mut view = vec![
            workspace(
                1,
                "api",
                Some("main"),
                vec![tab(
                    1,
                    "1",
                    true,
                    leaf(1),
                    vec![pane(1, "sh", None, AgentState::Idle)],
                )],
            ),
            workspace(
                2,
                "feat-a",
                Some("main"),
                vec![tab(
                    2,
                    "2",
                    false,
                    leaf(2),
                    vec![pane(2, "sh", None, AgentState::Idle)],
                )],
            ),
            workspace(
                3,
                "feat-b",
                Some("main"),
                vec![tab(
                    3,
                    "3",
                    false,
                    leaf(3),
                    vec![pane(3, "sh", None, AgentState::Idle)],
                )],
            ),
        ];
        view[1].parent = Some(WorkspaceId(1));
        view[2].parent = Some(WorkspaceId(1));
        let sidebar = build(&view, Some(TabId(1)), None);
        assert_eq!(sidebar.workspace_count, 3);
        let guides: Vec<Option<char>> = sidebar.entries[..3]
            .iter()
            .map(|e| match e {
                SidebarEntry::Workspace(w) => w.guide,
                _ => panic!("expected workspace rows"),
            })
            .collect();
        assert_eq!(
            guides,
            vec![None, Some('├'), Some('└')],
            "parent flush-left, first child mid-guide, last child end-guide"
        );
    }

    #[test]
    fn build_renders_a_child_top_level_when_its_parent_is_gone() {
        let mut view = vec![workspace(
            2,
            "web",
            None,
            vec![tab(
                2,
                "2",
                true,
                leaf(2),
                vec![pane(2, "sh", None, AgentState::Idle)],
            )],
        )];
        view[0].parent = Some(WorkspaceId(99)); // origin not present in the view
        let sidebar = build(&view, Some(TabId(2)), None);
        let SidebarEntry::Workspace(web) = &sidebar.entries[0] else {
            panic!("expected a workspace row");
        };
        assert_eq!(
            web.guide, None,
            "a child whose origin was killed renders flush-left"
        );
    }

    #[test]
    fn entry_at_row_selects_nested_children_as_ordinary_rows() {
        let mut view = two_workspace_view();
        view[1].parent = Some(WorkspaceId(1)); // web nested under api
        let sidebar = build(&view, Some(TabId(2)), None);
        // Projects header row 0; api rows 1-2; child web rows 3-4; divider row 5.
        assert_eq!(sidebar.entry_at_row(1), Some(0));
        assert_eq!(
            sidebar.entry_at_row(3),
            Some(1),
            "a nested child is an ordinary selectable entry"
        );
        assert_eq!(sidebar.entry_at_row(4), Some(1));
        assert_eq!(sidebar.header_at_row(5), Some(Section::Agents));
        // Collapsing the projects section hides the nested children too — the
        // child entry (index 1, in the projects section) is no longer visible.
        let mut collapsed = sidebar.clone();
        collapsed.projects_collapsed = true;
        assert!(
            !collapsed.is_visible(1),
            "collapsing projects hides the nested child"
        );
        assert!(
            collapsed.is_visible(2),
            "the agents section stays visible when projects collapse"
        );
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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        let agents: Vec<(&str, u64)> = sidebar.entries
            [sidebar.workspace_count..sidebar.workspace_count + sidebar.agent_count]
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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        let SidebarEntry::Agent(blocked) = &sidebar.entries[2] else {
            panic!("expected agent row");
        };
        assert_eq!(blocked.pane, PaneId(4));
        assert_eq!(blocked.tab, TabId(2), "agent jump carries its owning tab");
        assert_eq!(blocked.state, AgentState::Blocked);
    }

    #[test]
    fn entry_at_row_maps_clicks_past_the_frame_headers() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        // Row 12 is the waiting divider, then two rows per waiting entry.
        assert_eq!(sidebar.entry_at_row(12), None);
        assert_eq!(sidebar.header_at_row(12), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(13), Some(5));
        assert_eq!(sidebar.entry_at_row(16), Some(6));
        // Past the last waiting entry.
        assert_eq!(sidebar.entry_at_row(17), None);
    }

    #[test]
    fn collapsing_projects_shifts_the_agents_divider_up() {
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        sidebar.agents_collapsed = true;
        // Workspaces still map; agent rows past the divider select nothing —
        // the waiting divider fuses right below the collapsed agents header.
        assert_eq!(sidebar.entry_at_row(1), Some(0));
        assert_eq!(sidebar.header_at_row(5), Some(Section::Agents));
        assert_eq!(sidebar.header_at_row(6), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(6), None, "agent rows are hidden");
        assert_eq!(
            sidebar.entry_at_row(7),
            Some(5),
            "waiting rows stay clickable under a collapsed agents section"
        );
        assert!(!sidebar.is_visible(2), "an agent entry is not visible");
        assert!(sidebar.is_visible(0), "a workspace entry stays visible");
        assert!(sidebar.is_visible(5), "a waiting entry stays visible");
    }

    #[test]
    fn build_carries_subagents_onto_the_agent_row() {
        let sidebar = build(&view_with_subagents(), Some(TabId(1)), None);
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
        let sidebar = build(&view_with_subagents(), Some(TabId(1)), None);
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
        // Row 10 is the waiting divider (the done agent queues there), pushed
        // down past the subagent sub-rows; its entry sits at rows 11-12.
        assert_eq!(sidebar.entry_at_row(10), None, "the waiting divider");
        assert_eq!(sidebar.header_at_row(10), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(11), Some(3));
        assert_eq!(sidebar.entry_at_row(13), None, "past the last waiting row");
    }
}
