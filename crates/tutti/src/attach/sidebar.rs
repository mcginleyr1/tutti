//! The workspace/agent sidebar model: pure functions that turn the client's
//! `WorkspaceView` list into the two stacked sections the sidebar shows — the
//! project tree (each project over its agents and nested workspaces, each of
//! those over their own agents) and the cross-project waiting queue — plus the
//! row arithmetic mapping a click to an entry. No rendering, no state, so the
//! tree order, sort rules, and hit-testing are unit-tested in isolation.

use tutti_core::{AgentState, PaneId, SubagentInfo, TabId, WorkspaceId, WorkspaceView};

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
    Waiting,
}

/// A selectable row's stable identity, independent of its position. The waiting
/// copy of an agent is distinct from its tree row so a cursor in the queue
/// re-anchors to the queue, not to the first duplicate, across rebuilds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryIdent {
    Workspace(WorkspaceId),
    Agent(PaneId),
    Waiting(PaneId),
}

/// Agent counts by attention-worthy state — the roll-up chips on a project row
/// and the app bar's census. Idle/unknown agents are deliberately uncounted:
/// the chips answer "does anything need me", not "how many panes exist".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentCensus {
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
}

impl AgentCensus {
    pub fn is_empty(&self) -> bool {
        self.blocked == 0 && self.working == 0 && self.done == 0
    }

    fn count(&mut self, state: AgentState) {
        match state {
            AgentState::Blocked => self.blocked += 1,
            AgentState::Working => self.working += 1,
            AgentState::Done => self.done += 1,
            AgentState::Idle | AgentState::Unknown => {}
        }
    }
}

/// The census over every agent pane in `workspaces`.
pub fn census(workspaces: &[WorkspaceView]) -> AgentCensus {
    let mut census = AgentCensus::default();
    for pane in workspaces
        .iter()
        .flat_map(|w| &w.tabs)
        .flat_map(|t| &t.panes)
        .filter(|p| p.agent.is_some())
    {
        census.count(pane.state);
    }
    census
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
    /// jj-workspace child, itself for a project row — the cursor's fallback
    /// anchor when this row vanishes.
    pub project: WorkspaceId,
    /// The tree-guide glyph when this row is a jj-workspace child nested under a
    /// project: `├` for a non-last sibling, `└` for the last. `None` for a
    /// top-level project row (drawn flush-left, no guide). Its presence is what
    /// tells the client this row can be merged.
    pub guide: Option<char>,
    /// Agent counts rolled up onto this row: a project row counts its whole
    /// subtree (its own agents plus nested children's), a child row its own —
    /// right-aligned chips beside the name.
    pub census: AgentCensus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub pane: PaneId,
    pub tab: TabId,
    pub title: String,
    pub state: AgentState,
    pub kind: String,
    /// The top-level project owning this agent's workspace, and its display
    /// name. The cursor's fallback anchor; the waiting section shows the name
    /// so a cross-project row says where it lives.
    pub project: WorkspaceId,
    pub project_name: String,
    /// The tree guide when this row hangs under its workspace in the tree
    /// (`├`/`└` among that workspace's agents); `None` on waiting rows.
    pub guide: Option<char>,
    /// Indent depth in the tree: 1 under a top-level project, 2 under a nested
    /// workspace child, 0 on waiting rows.
    pub depth: u8,
    /// Hook-reported subagents, rendered as dim indented sub-rows under this
    /// agent. Display-only: they are never selectable, but they add height, so
    /// `entry_at_row` accounts for them. Always empty on waiting-section rows,
    /// which stay two rows tall.
    pub subagents: Vec<SubagentInfo>,
}

/// The sidebar's contents: the project tree (workspaces interleaved with their
/// agents), then the cross-project waiting rows. `tree_count` records where the
/// waiting section begins so the renderer and the hit-test agree on row layout.
/// The `*_collapsed` flags hide a section's rows down to its header, set by the
/// client from its collapse state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sidebar {
    pub entries: Vec<SidebarEntry>,
    pub tree_count: usize,
    pub projects_collapsed: bool,
    pub waiting_collapsed: bool,
}

/// How many screen rows an entry occupies: its two lines plus one dim sub-row
/// per subagent (workspace rows never carry subagents).
fn entry_rows(entry: &SidebarEntry) -> usize {
    2 + match entry {
        SidebarEntry::Agent(a) => a.subagents.len(),
        SidebarEntry::Workspace(_) => 0,
    }
}

impl Sidebar {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The number of workspace rows in the tree — the projects header's count.
    pub fn workspace_rows(&self) -> usize {
        self.entries[..self.tree_count]
            .iter()
            .filter(|e| matches!(e, SidebarEntry::Workspace(_)))
            .count()
    }

    /// The stable identity of entry `idx`, or `None` past the end. Agent
    /// entries at or past the waiting boundary identify as `Waiting`.
    pub fn ident_at(&self, idx: usize) -> Option<EntryIdent> {
        Some(match self.entries.get(idx)? {
            SidebarEntry::Workspace(w) => EntryIdent::Workspace(w.id),
            SidebarEntry::Agent(a) if idx >= self.tree_count => EntryIdent::Waiting(a.pane),
            SidebarEntry::Agent(a) => EntryIdent::Agent(a.pane),
        })
    }

    /// The index currently holding `ident`, if the entry is still in the view.
    pub fn index_of(&self, ident: EntryIdent) -> Option<usize> {
        (0..self.entries.len()).find(|&i| self.ident_at(i) == Some(ident))
    }

    /// Whether entry `idx` is currently visible (its section is expanded).
    pub fn is_visible(&self, idx: usize) -> bool {
        if idx < self.tree_count {
            !self.projects_collapsed
        } else {
            !self.waiting_collapsed
        }
    }

    /// How many rows the tree section body occupies: none collapsed, else each
    /// entry's rows. Mirrors the renderer exactly — the waiting divider's
    /// position depends on it.
    fn tree_body_rows(&self) -> usize {
        if self.projects_collapsed {
            return 0;
        }
        self.entries[..self.tree_count].iter().map(entry_rows).sum()
    }

    /// The screen row (relative to the sidebar frame's top) of the waiting
    /// divider — the border-fused `waiting` header. Row 0 is always the
    /// projects header (the top border); the divider follows the tree body.
    fn waiting_divider_row(&self) -> usize {
        1 + self.tree_body_rows()
    }

    /// The section header (if any) a click at `row` toggles. Row 0 is the
    /// projects header in the top border; the waiting header is the fused
    /// divider.
    pub fn header_at_row(&self, row: usize) -> Option<Section> {
        if row == 0 {
            Some(Section::Projects)
        } else if row == self.waiting_divider_row() {
            Some(Section::Waiting)
        } else {
            None
        }
    }

    /// The entry a click at `row` (relative to the sidebar frame's top) selects,
    /// or `None` for a border, a header, a subagent sub-row, a collapsed section,
    /// or empty space. The layout mirrors the renderer: the projects header (top
    /// border, row 0), then per tree entry its two rows plus one dim sub-row per
    /// subagent (a click on which selects nothing), then the waiting header
    /// (the divider) and two rows per waiting entry.
    pub fn entry_at_row(&self, row: usize) -> Option<usize> {
        if !self.projects_collapsed {
            let mut cursor = 1; // right below the projects header (top border)
            for (idx, entry) in self.entries[..self.tree_count].iter().enumerate() {
                if row == cursor || row == cursor + 1 {
                    return Some(idx);
                }
                cursor += entry_rows(entry);
            }
        }
        let waiting_start = self.waiting_divider_row() + 1;
        if self.waiting_collapsed || row < waiting_start {
            return None;
        }
        let idx = self.tree_count + (row - waiting_start) / 2;
        (idx < self.entries.len()).then_some(idx)
    }
}

/// Build the sidebar from the client's view. `active_tab` decides which
/// workspace is bold and where each workspace jump lands. The tree section
/// interleaves each workspace with the agents it runs: a top-level project row,
/// its agent rows (indented on `├`/`└` guides), then each nested jj-workspace
/// child (indented, guided) followed by that child's agents one level deeper.
/// Agents keep pane order — positional stability over urgency, so a state
/// change never reshuffles the tree under the cursor; attention order lives in
/// the waiting section, which gathers blocked and done agents across every
/// project — blocked first, subagents stripped so each row stays two rows tall.
pub fn build(workspaces: &[WorkspaceView], active_tab: Option<TabId>) -> Sidebar {
    let present: std::collections::HashSet<_> = workspaces.iter().map(|w| w.id).collect();
    // A workspace nests only when its parent is actually present; a child whose
    // origin was killed renders as a top-level project.
    let is_child = |w: &WorkspaceView| w.parent.is_some_and(|p| present.contains(&p));
    let project_of = |w: &WorkspaceView| w.parent.filter(|p| present.contains(p)).unwrap_or(w.id);
    let name_of = |id: WorkspaceId| {
        workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.name.clone())
            .unwrap_or_default()
    };

    // A project row's chips roll up its whole subtree; a child row counts its
    // own agents only.
    let census_of = |w: &WorkspaceView, subtree: bool| -> AgentCensus {
        let mut census = AgentCensus::default();
        let mut tally = |ws: &WorkspaceView| {
            for pane in ws
                .tabs
                .iter()
                .flat_map(|t| &t.panes)
                .filter(|p| p.agent.is_some())
            {
                census.count(pane.state);
            }
        };
        tally(w);
        if subtree {
            for child in workspaces.iter().filter(|c| c.parent == Some(w.id)) {
                tally(child);
            }
        }
        census
    };

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
            census: census_of(w, guide.is_none() && !is_child(w)),
        }))
    };

    let agent_rows_of = |w: &WorkspaceView, depth: u8| -> Vec<SidebarEntry> {
        let mut rows: Vec<AgentRow> = w
            .tabs
            .iter()
            .flat_map(|tab| {
                tab.panes.iter().filter_map(move |pane| {
                    pane.agent.as_ref().map(|agent| AgentRow {
                        pane: pane.id,
                        tab: tab.id,
                        title: pane.title.clone(),
                        state: pane.state,
                        kind: agent.to_string(),
                        project: project_of(w),
                        project_name: name_of(project_of(w)),
                        guide: None,
                        depth,
                        subagents: pane.subagents.clone(),
                    })
                })
            })
            .collect();
        rows.sort_by_key(|a| a.pane.0);
        let last = rows.len();
        for (i, row) in rows.iter_mut().enumerate() {
            row.guide = Some(if i + 1 == last { '└' } else { '├' });
        }
        rows.into_iter().map(SidebarEntry::Agent).collect()
    };

    let mut entries = Vec::new();
    for w in workspaces {
        if is_child(w) {
            continue; // emitted under its parent below
        }
        entries.extend(row(w, None));
        entries.extend(agent_rows_of(w, 1));
        let children: Vec<&WorkspaceView> = workspaces
            .iter()
            .filter(|c| c.parent == Some(w.id))
            .collect();
        let last = children.len();
        for (i, child) in children.iter().enumerate() {
            let glyph = if i + 1 == last { '└' } else { '├' };
            entries.extend(row(child, Some(glyph)));
            entries.extend(agent_rows_of(child, 2));
        }
    }
    let tree_count = entries.len();

    let mut waiting: Vec<AgentRow> = workspaces
        .iter()
        .flat_map(|w| {
            w.tabs.iter().flat_map(move |tab| {
                tab.panes.iter().filter_map(move |pane| {
                    pane.agent
                        .as_ref()
                        .filter(|_| matches!(pane.state, AgentState::Blocked | AgentState::Done))
                        .map(|agent| AgentRow {
                            pane: pane.id,
                            tab: tab.id,
                            title: pane.title.clone(),
                            state: pane.state,
                            kind: agent.to_string(),
                            project: project_of(w),
                            project_name: name_of(project_of(w)),
                            guide: None,
                            depth: 0,
                            subagents: Vec::new(),
                        })
                })
            })
        })
        .collect();
    waiting.sort_by_key(|a| (state_rank(a.state), a.pane.0));
    entries.extend(waiting.into_iter().map(SidebarEntry::Agent));

    Sidebar {
        entries,
        tree_count,
        projects_collapsed: false,
        waiting_collapsed: false,
    }
}

/// Attention order for the waiting queue: blocked first, then working, done,
/// idle, unknown.
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

    /// One workspace (active tab) with two agents; the first carries two
    /// subagents — one running, one done.
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
    fn build_interleaves_each_workspace_with_its_agents_then_waiting() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        // api, its claude, web, its codex + claude (pane order), then the
        // waiting section: pane 4 (blocked) ahead of pane 3 (done).
        assert_eq!(sidebar.tree_count, 5);
        assert_eq!(sidebar.len(), 7);
        let names: Vec<String> = sidebar
            .entries
            .iter()
            .map(|e| match e {
                SidebarEntry::Workspace(w) => w.name.clone(),
                SidebarEntry::Agent(a) => format!("pane-{}", a.pane.0),
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "api", "pane-1", "web", "pane-3", "pane-4", "pane-4", "pane-3"
            ]
        );
    }

    #[test]
    fn tree_agents_keep_pane_order_not_attention_order() {
        // web's blocked agent (pane 4) follows its done agent (pane 3) in the
        // tree — a state change must never reshuffle rows under the cursor.
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let panes: Vec<u64> = sidebar.entries[3..5]
            .iter()
            .map(|e| match e {
                SidebarEntry::Agent(a) => a.pane.0,
                _ => panic!("expected web's agent rows"),
            })
            .collect();
        assert_eq!(panes, vec![3, 4], "pane order, even though 4 is blocked");
    }

    #[test]
    fn tree_agents_carry_guides_and_depth() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let SidebarEntry::Agent(api_agent) = &sidebar.entries[1] else {
            panic!("expected api's agent row");
        };
        assert_eq!(
            api_agent.guide,
            Some('└'),
            "an only agent gets the last guide"
        );
        assert_eq!(api_agent.depth, 1);
        let SidebarEntry::Agent(mid) = &sidebar.entries[3] else {
            panic!("expected web's first agent row");
        };
        assert_eq!(mid.guide, Some('├'), "a non-last agent gets the mid guide");
    }

    #[test]
    fn waiting_gathers_blocked_then_done_across_projects() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let waiting: Vec<(u64, Option<char>, u8)> = sidebar.entries[sidebar.tree_count..]
            .iter()
            .map(|e| match e {
                SidebarEntry::Agent(a) => (a.pane.0, a.guide, a.depth),
                _ => panic!("expected waiting agent rows"),
            })
            .collect();
        assert_eq!(
            waiting,
            vec![(4, None, 0), (3, None, 0)],
            "blocked first, then done; waiting rows carry no tree dressing"
        );
    }

    #[test]
    fn idents_distinguish_the_tree_row_from_its_waiting_copy() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        assert_eq!(
            sidebar.ident_at(0),
            Some(EntryIdent::Workspace(WorkspaceId(1)))
        );
        assert_eq!(sidebar.ident_at(4), Some(EntryIdent::Agent(PaneId(4))));
        assert_eq!(sidebar.ident_at(5), Some(EntryIdent::Waiting(PaneId(4))));
        assert_eq!(
            sidebar.index_of(EntryIdent::Waiting(PaneId(4))),
            Some(5),
            "the waiting copy resolves to the queue, not the tree row"
        );
        assert_eq!(sidebar.index_of(EntryIdent::Agent(PaneId(4))), Some(4));
        assert_eq!(sidebar.ident_at(7), None);
    }

    #[test]
    fn waiting_rows_carry_the_project_name_and_no_subagents() {
        let mut view = view_with_subagents();
        view[0].tabs[0].panes[0].state = AgentState::Blocked; // subagent carrier
        let sidebar = build(&view, Some(TabId(1)));
        let SidebarEntry::Agent(a) = &sidebar.entries[sidebar.tree_count] else {
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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let SidebarEntry::Workspace(api) = &sidebar.entries[0] else {
            panic!("expected workspace row");
        };
        let SidebarEntry::Workspace(web) = &sidebar.entries[2] else {
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
    fn build_carries_the_workspace_change_stat_and_stale_flag() {
        let mut view = two_workspace_view();
        view[0].changes = Some("2 files +5 −0".into());
        view[0].stale = true;
        let sidebar = build(&view, Some(TabId(2)));
        let SidebarEntry::Workspace(api) = &sidebar.entries[0] else {
            panic!("expected workspace row");
        };
        let SidebarEntry::Workspace(web) = &sidebar.entries[2] else {
            panic!("expected workspace row");
        };
        assert_eq!(api.changes.as_deref(), Some("2 files +5 −0"));
        assert!(api.stale);
        assert_eq!(web.changes, None);
        assert!(!web.stale);
    }

    #[test]
    fn build_nests_a_child_workspace_and_its_agents_under_the_parent() {
        let mut view = two_workspace_view();
        // Make `web` (id 2) a jj-workspace child of `api` (id 1).
        view[1].parent = Some(WorkspaceId(1));
        let sidebar = build(&view, Some(TabId(2)));
        // api, api's agent, child web, then web's agents one level deeper.
        assert_eq!(sidebar.tree_count, 5);
        let SidebarEntry::Workspace(web) = &sidebar.entries[2] else {
            panic!("expected the nested child row");
        };
        assert_eq!(web.guide, Some('└'), "the only child gets the last guide");
        assert_eq!(web.project, WorkspaceId(1), "the child rolls up to api");
        let SidebarEntry::Agent(a) = &sidebar.entries[3] else {
            panic!("expected the child's agent row");
        };
        assert_eq!(a.depth, 2, "a child workspace's agents indent one deeper");
        assert_eq!(a.project, WorkspaceId(1), "child agents roll up to api");
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
        let sidebar = build(&view, Some(TabId(1)));
        assert_eq!(sidebar.tree_count, 3);
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
        let sidebar = build(&view, Some(TabId(2)));
        let SidebarEntry::Workspace(web) = &sidebar.entries[0] else {
            panic!("expected a workspace row");
        };
        assert_eq!(
            web.guide, None,
            "a child whose origin was killed renders flush-left"
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
    fn agent_rows_carry_tab_and_pane_jump_targets() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)));
        let SidebarEntry::Agent(blocked) = &sidebar.entries[4] else {
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
        // Tree rows, two per entry: api 1-2, its agent 3-4, web 5-6, codex 7-8,
        // claude 9-10.
        assert_eq!(sidebar.entry_at_row(1), Some(0));
        assert_eq!(sidebar.entry_at_row(2), Some(0));
        assert_eq!(sidebar.entry_at_row(3), Some(1));
        assert_eq!(sidebar.entry_at_row(6), Some(2));
        assert_eq!(sidebar.entry_at_row(8), Some(3));
        assert_eq!(sidebar.entry_at_row(10), Some(4));
        // Row 11 is the waiting divider, then two rows per waiting entry.
        assert_eq!(sidebar.entry_at_row(11), None);
        assert_eq!(sidebar.header_at_row(11), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(12), Some(5));
        assert_eq!(sidebar.entry_at_row(14), Some(6));
        // Past the last waiting entry.
        assert_eq!(sidebar.entry_at_row(16), None);
    }

    #[test]
    fn collapsing_projects_shifts_the_waiting_divider_up() {
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)));
        sidebar.projects_collapsed = true;
        assert_eq!(sidebar.entry_at_row(1), None, "tree rows are hidden");
        assert_eq!(sidebar.header_at_row(1), Some(Section::Waiting));
        // Waiting rows follow immediately after the divider.
        assert_eq!(sidebar.entry_at_row(2), Some(5));
        assert_eq!(sidebar.entry_at_row(4), Some(6));
        assert!(!sidebar.is_visible(0), "a tree entry is not visible");
        assert!(sidebar.is_visible(5), "a waiting entry stays visible");
    }

    #[test]
    fn collapsing_waiting_hides_the_queue_rows() {
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)));
        sidebar.waiting_collapsed = true;
        assert_eq!(sidebar.entry_at_row(1), Some(0), "tree rows still map");
        assert_eq!(sidebar.header_at_row(11), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(12), None, "waiting rows are hidden");
        assert!(!sidebar.is_visible(5), "a waiting entry is not visible");
        assert!(sidebar.is_visible(0), "a tree entry stays visible");
    }

    #[test]
    fn build_carries_subagents_onto_the_tree_row() {
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
        // One workspace: projects header row 0, workspace rows 1-2; agent entry
        // 1 (with two subagents) occupies rows 3-4 as its head.
        assert_eq!(sidebar.entry_at_row(1), Some(0), "the workspace row");
        assert_eq!(sidebar.entry_at_row(3), Some(1));
        assert_eq!(sidebar.entry_at_row(4), Some(1));
        // Its two subagent sub-rows (5-6) are display-only: not selectable.
        assert_eq!(sidebar.entry_at_row(5), None);
        assert_eq!(sidebar.entry_at_row(6), None);
        // The next agent is pushed down past the sub-rows, to 7-8.
        assert_eq!(sidebar.entry_at_row(7), Some(2));
        assert_eq!(sidebar.entry_at_row(8), Some(2));
        // Row 9 is the waiting divider (the done agent queues there), pushed
        // down past the subagent sub-rows; its entry sits at rows 10-11.
        assert_eq!(sidebar.entry_at_row(9), None, "the waiting divider");
        assert_eq!(sidebar.header_at_row(9), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(10), Some(3));
        assert_eq!(sidebar.entry_at_row(12), None, "past the last waiting row");
    }
}
