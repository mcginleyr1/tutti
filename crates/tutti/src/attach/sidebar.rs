//! The workspace/agent sidebar (`Mode::Sidebar`, and the confirms it raises).
//! Its model is pure functions that turn the client's `WorkspaceView` list
//! into the two stacked sections the sidebar shows — the project tree (each
//! project over its agents and nested workspaces, each of those over their own
//! agents) and the cross-project waiting queue — plus the row arithmetic
//! mapping a click to an entry: no rendering, no state, so the tree order,
//! sort rules, and hit-testing are unit-tested in isolation. The sidebar's own
//! state (`SidebarState`) and `App`'s keys and clicks over it follow.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use tutti_core::{
    AgentKind, AgentState, Frame as WireFrame, PaneId, PaneInfo, PendingApproval, Request,
    SubagentInfo, TabId, WorkspaceId, WorkspaceView,
};

use super::app::{App, FocusDir, Mode, contains, control};
use super::changes::diff_pager_cmd;
use super::follow::FollowUp;
use crate::config::{self, Action, SidebarAction, SidebarVisibility};

/// The sidebar's fixed column width, and the minimum total width below which it
/// is suppressed entirely so panes keep usable room.
pub(super) const SIDEBAR_WIDTH: u16 = 30;
const SIDEBAR_MIN_TOTAL: u16 = 80;

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
/// re-anchors to the queue, not to the first duplicate, across rebuilds; a
/// pane queues once per approval it holds, each row named by its approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryIdent {
    Workspace(WorkspaceId),
    Agent(PaneId),
    Waiting(PaneId, Option<u64>),
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
    /// The id of the tracker issue the agent was deployed on, shown dim after
    /// the title.
    pub issue: Option<String>,
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
    /// A held approval of the pane's — on a waiting row the one it queues
    /// for, ahead of every row without one, whatever the pane's state; on a
    /// tree row the oldest. The row names what would run.
    pub approval: Option<PendingApproval>,
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
            SidebarEntry::Agent(a) if idx >= self.tree_count => {
                EntryIdent::Waiting(a.pane, a.approval.as_ref().map(|p| p.id))
            }
            SidebarEntry::Agent(a) => EntryIdent::Agent(a.pane),
        })
    }

    /// The index currently holding `ident`, if the entry is still in the view
    /// — for a waiting row whose approval was answered, the pane's next row in
    /// the queue.
    pub fn index_of(&self, ident: EntryIdent) -> Option<usize> {
        let find = |want: &dyn Fn(EntryIdent) -> bool| {
            (0..self.entries.len()).find(|&i| self.ident_at(i).is_some_and(want))
        };
        find(&|i| i == ident).or_else(|| match ident {
            EntryIdent::Waiting(pane, _) => {
                find(&|i| matches!(i, EntryIdent::Waiting(p, _) if p == pane))
            }
            _ => None,
        })
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
    /// projects header (the top border); the divider follows the tree body,
    /// plus the renderer's one-row spacer whenever tree rows precede it.
    fn waiting_divider_row(&self) -> usize {
        let body = self.tree_body_rows();
        1 + body + usize::from(body > 0)
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
/// project — first a row per held approval, in the order they came, whatever
/// the pane's state, then blocked — subagents stripped so each row stays two
/// rows tall.
pub fn build(
    workspaces: &[WorkspaceView],
    active_tab: Option<TabId>,
    home: Option<&Path>,
) -> Sidebar {
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
            .or_else(|| {
                let mut tabs = w.tabs.iter();
                let running = tabs.find(|t| t.panes.iter().any(|p| p.exited.is_none()));
                running.map(|t| t.id)
            })
            .or_else(|| w.tabs.first().map(|t| t.id))?;
        Some(SidebarEntry::Workspace(WorkspaceRow {
            id: w.id,
            name: w.name.clone(),
            subtitle: w.branch.clone().or_else(|| shorten_home(&w.dir, home)),
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
                        kind: kind_label(agent, pane),
                        issue: pane.issue.as_ref().map(|i| i.id.clone()),
                        project: project_of(w),
                        project_name: name_of(project_of(w)),
                        guide: None,
                        depth,
                        subagents: pane.subagents.clone(),
                        approval: pane.approvals.first().cloned(),
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
                let agents = tab
                    .panes
                    .iter()
                    .filter_map(|pane| Some((pane, pane.agent.as_ref()?)));
                agents.flat_map(move |(pane, agent)| {
                    let queued: Vec<Option<PendingApproval>> = match &pane.approvals[..] {
                        [] if matches!(pane.state, AgentState::Blocked | AgentState::Done) => {
                            vec![None]
                        }
                        held => held.iter().cloned().map(Some).collect(),
                    };
                    queued.into_iter().map(move |approval| AgentRow {
                        pane: pane.id,
                        tab: tab.id,
                        title: pane.title.clone(),
                        state: pane.state,
                        kind: kind_label(agent, pane),
                        issue: pane.issue.as_ref().map(|i| i.id.clone()),
                        project: project_of(w),
                        project_name: name_of(project_of(w)),
                        guide: None,
                        depth: 0,
                        subagents: Vec::new(),
                        approval,
                    })
                })
            })
        })
        .collect();
    waiting.sort_by_key(|a| {
        let approval = a.approval.as_ref().map(|p| p.id);
        (approval.is_none(), approval, state_rank(a.state), a.pane.0)
    });
    entries.extend(waiting.into_iter().map(SidebarEntry::Agent));

    Sidebar {
        entries,
        tree_count,
        projects_collapsed: false,
        waiting_collapsed: false,
    }
}

/// An agent row's kind line: the agent, plus `on <profile>` when it runs in a
/// sandbox, so a remote agent never passes for a local one.
pub fn kind_label(agent: &AgentKind, pane: &PaneInfo) -> String {
    match &pane.sandbox {
        Some(profile) => format!("{agent} on {profile}"),
        None => agent.to_string(),
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

/// `~`-shorten `dir` against the daemon machine's `home` for the subtitle
/// line; `None` for an empty path.
fn shorten_home(dir: &Path, home: Option<&Path>) -> Option<String> {
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

/// The sidebar's own state, kept whether or not it holds the keys: where its
/// cursor is, which sections are folded, where it was last drawn.
#[derive(Default)]
pub(super) struct SidebarState {
    /// The highlighted sidebar entry while the sidebar is focused.
    pub(super) selected: usize,
    /// The selected row's identity and owning project, captured whenever the
    /// selection moves. `set_view` re-anchors the cursor through this after
    /// every layout change — same row, else its project's row, else a clamped
    /// index — so a kill or state change never silently retargets the cursor.
    anchor: Option<(EntryIdent, WorkspaceId)>,
    /// Whether the projects / agents / waiting sidebar sections are collapsed to
    /// their header. Toggled by clicking a section header.
    collapsed_projects: bool,
    collapsed_waiting: bool,
    /// The sidebar column from the last size sync, for mouse hit-testing.
    pub(super) rect: Option<Rect>,
}

impl App {
    /// Re-anchor the cursor by identity, never by position, after a layout
    /// change: the same row if it survived, else its project's row (a killed
    /// agent falls to its project), else the old index clamped into range.
    pub(super) fn reanchor_sidebar(&mut self) {
        let sidebar = self.sidebar();
        let resolved = self.sidebar.anchor.and_then(|(ident, project)| {
            sidebar
                .index_of(ident)
                .or_else(|| sidebar.index_of(EntryIdent::Workspace(project)))
        });
        self.sidebar.selected =
            resolved.unwrap_or(self.sidebar.selected.min(sidebar.len().saturating_sub(1)));
        self.sidebar.anchor = anchor_at(&sidebar, self.sidebar.selected);
    }

    /// The sidebar as it currently renders — the project tree then the waiting
    /// queue — for the renderer, hit-testing, and navigation. Carries the
    /// client's collapse state so the frame headers and row math agree.
    pub fn sidebar(&self) -> Sidebar {
        let mut sidebar = build(&self.workspaces, self.active_tab, self.home.as_deref());
        for entry in &mut sidebar.entries {
            if let SidebarEntry::Workspace(row) = entry
                && !row.active
                && let Some(tab) = self.visited.get(&row.id)
                && self.all_tabs().iter().any(|t| t.id == *tab)
            {
                row.jump_tab = *tab;
            }
        }
        sidebar.projects_collapsed = self.sidebar.collapsed_projects;
        sidebar.waiting_collapsed = self.sidebar.collapsed_waiting;
        sidebar
    }

    /// Whether the sidebar currently holds keyboard focus. The sidebar-raised
    /// confirms and overlays count: while one is open the agents filter and
    /// the selection highlight must not shift under the question being asked.
    pub fn sidebar_focused(&self) -> bool {
        matches!(
            self.mode,
            Mode::Sidebar
                | Mode::SidebarPrompt
                | Mode::SidebarWorkspaceName(_)
                | Mode::SidebarWorkspaceDest(_)
                | Mode::ConfirmKillWorkspace(_)
                | Mode::ConfirmKillAgent(_)
                | Mode::ConfirmMerge(_)
                | Mode::ConfirmCleanup(_)
                | Mode::Peek(_)
                | Mode::Reply(_)
        ) || (matches!(self.mode, Mode::Approval(_)) && self.approvals.back == Mode::Sidebar)
    }

    /// Whether the sidebar should be drawn for a content area of `total_width`.
    /// A focused sidebar always shows (so the sidebar key can reveal a hidden
    /// one); otherwise the config mode decides — `auto` showing it once the
    /// session is worth surfacing. Always suppressed below the width floor.
    pub(super) fn sidebar_shown(&self, total_width: u16) -> bool {
        if total_width < SIDEBAR_MIN_TOTAL {
            return false;
        }
        if self.sidebar_focused() {
            return true;
        }
        match self.config.sidebar {
            SidebarVisibility::On => true,
            SidebarVisibility::Off => false,
            SidebarVisibility::Auto => self.workspaces.len() > 1 || self.has_agent_pane(),
        }
    }

    fn has_agent_pane(&self) -> bool {
        self.workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .any(|p| p.agent.is_some())
    }

    /// Focus the sidebar, revealing it if hidden. Refuses when the terminal is
    /// too narrow to render it, so focus is never trapped on an invisible panel.
    pub(super) fn focus_sidebar(&mut self) {
        if self.last_content_width < SIDEBAR_MIN_TOTAL {
            self.set_status("terminal too narrow for the sidebar".into());
            return;
        }
        self.mode = Mode::Sidebar;
        // Land on the active project's row — where the user actually is — not
        // unconditionally on row 0.
        let sidebar = self.sidebar();
        let idx = self
            .active_project()
            .and_then(|p| sidebar.index_of(EntryIdent::Workspace(p)))
            .unwrap_or(0);
        self.select_entry(&sidebar, idx);
        self.status = None;
    }

    /// Move the sidebar cursor to `idx`, capturing its identity anchor so the
    /// next layout change re-finds the same row rather than the same position.
    fn select_entry(&mut self, sidebar: &Sidebar, idx: usize) {
        self.sidebar.selected = idx;
        self.sidebar.anchor = anchor_at(sidebar, idx);
    }

    /// Keys while the sidebar is focused, dispatched through
    /// `config::SIDEBAR_TABLE` — the table the help overlay and the bottom-bar
    /// hint render, so what they say is what the keys do.
    pub(super) fn on_key_sidebar(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if self.enter_prefix(key) {
            return Vec::new();
        }
        // The directional direct bindings cross the sidebar↔pane edge:
        // focus_right returns to the pane area (keeping its focus), focus_left
        // goes past tutti's left edge as the pane's left edge would with no
        // sidebar — on to rtty, else the previous tab — and in rtty, up and
        // down go on to rtty's pane that way.
        match self.config.keys.action_for(key) {
            Some(Action::FocusRight) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return Vec::new();
            }
            Some(Action::FocusLeft) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return self.past_edge(FocusDir::Left);
            }
            Some(Action::FocusUp) if self.rtty.is_some() => return self.past_edge(FocusDir::Up),
            Some(Action::FocusDown) if self.rtty.is_some() => {
                return self.past_edge(FocusDir::Down);
            }
            _ => {}
        }
        let Some(action) = config::panel_action(config::SIDEBAR_TABLE, key) else {
            return Vec::new();
        };
        let sidebar = self.sidebar();
        match action {
            SidebarAction::Down => {
                self.select_entry(&sidebar, next_visible(&sidebar, self.sidebar.selected));
                Vec::new()
            }
            SidebarAction::Up => {
                self.select_entry(&sidebar, prev_visible(&sidebar, self.sidebar.selected));
                Vec::new()
            }
            SidebarAction::Jump => self.jump_to_selected(&sidebar),
            SidebarAction::Peek => self.open_peek(&sidebar),
            SidebarAction::NewAgent => self.new_agent_in_selected(&sidebar),
            SidebarAction::AddProject => {
                self.open_project_prompt();
                Vec::new()
            }
            SidebarAction::Diff => self.open_diff_pane(&sidebar),
            SidebarAction::NewWorkspace => {
                self.open_workspace_prompt(&sidebar);
                Vec::new()
            }
            SidebarAction::Merge => {
                self.confirm_merge(&sidebar);
                Vec::new()
            }
            SidebarAction::Update => self.update_selected(&sidebar),
            SidebarAction::Kill => {
                self.confirm_kill_selected(&sidebar);
                Vec::new()
            }
            SidebarAction::Back => {
                self.mode = Mode::Terminal;
                self.status = None;
                Vec::new()
            }
        }
    }

    /// Jump to the selected entry and hand focus back to the pane: a workspace
    /// selects its tab; an agent selects its tab and focuses its pane (the
    /// `PaneFocus` follows from `focus_change`).
    fn jump_to_selected(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        match sidebar.entries.get(self.sidebar.selected) {
            Some(SidebarEntry::Workspace(w)) => self.jump_to(w.jump_tab, None),
            Some(SidebarEntry::Agent(a)) => self.jump_to(a.tab, Some(a.pane)),
            None => {
                self.mode = Mode::Terminal;
                self.status = None;
                Vec::new()
            }
        }
    }

    /// Jump to the selected entry's workspace — its active-or-first tab, the
    /// same routing a workspace-row Enter uses — and open the launcher armed to
    /// start the choice in a fresh tab there: an agent is a tab-sized thing, so
    /// it never splits whatever that tab already holds. Splits stay behind the
    /// explicit prefix verbs. An agent row targets the workspace that owns it.
    fn new_agent_in_selected(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        let Some((workspace, tab, name, dir)) = self.selected_workspace(sidebar).and_then(|w| {
            w.tabs
                .iter()
                .find(|t| t.active)
                .or_else(|| w.tabs.first())
                .map(|t| (w.id, t.id, w.name.clone(), w.dir.clone()))
        }) else {
            return Vec::new();
        };
        self.active_tab = Some(tab);
        self.zoom = false;
        self.refocus();
        self.open_launcher(false, Some(name), Some(&dir));
        self.launcher.new_tab = Some(workspace);
        vec![control(&Request::TabSelect { id: tab })]
    }

    /// Open the selected workspace's jj diff in an ephemeral pane — a real
    /// terminal running `jj diff | less -R`, removed the moment `less` quits. A
    /// workspace entry targets its own tab; an agent entry targets the tab it
    /// lives in. A non-jj workspace shows a transient error instead of spawning,
    /// since jj is the required VCS for workspace diffs.
    fn open_diff_pane(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        let tab = match sidebar.entries.get(self.sidebar.selected) {
            Some(SidebarEntry::Workspace(w)) => w.jump_tab,
            Some(SidebarEntry::Agent(a)) => a.tab,
            None => return Vec::new(),
        };
        let Some(dir) = self.workspace_dir_of_tab(tab) else {
            return Vec::new();
        };
        match self.host.is_jj(&dir) {
            Ok(true) => {}
            Ok(false) => {
                self.set_status(format!("not a jj workspace: {}", dir.display()));
                return Vec::new();
            }
            Err(e) => {
                self.set_status(format!("error: {e:#}"));
                return Vec::new();
            }
        }
        self.mode = Mode::Terminal;
        self.status = None;
        self.active_tab = Some(tab);
        self.zoom = false;
        vec![control(&Request::PaneRun {
            tab: Some(tab),
            cmd: diff_pager_cmd(None),
            ephemeral: true,
            sandbox: None,
            source: None,
        })]
    }

    /// The directory of the workspace owning `tab`, from the current view.
    fn workspace_dir_of_tab(&self, tab: TabId) -> Option<PathBuf> {
        self.workspaces
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == tab))
            .map(|w| w.dir.clone())
    }

    /// The `WorkspaceView` behind the selected entry — a workspace row's own, or
    /// the workspace owning a selected agent row's tab. `None` when nothing is
    /// selected or its workspace has left the view.
    pub(super) fn selected_workspace(&self, sidebar: &Sidebar) -> Option<&WorkspaceView> {
        match sidebar.entries.get(self.sidebar.selected)? {
            SidebarEntry::Workspace(w) => self.workspaces.iter().find(|ws| ws.id == w.id),
            SidebarEntry::Agent(a) => self
                .workspaces
                .iter()
                .find(|w| w.tabs.iter().any(|t| t.id == a.tab)),
        }
    }

    /// Confirm merging the selected child workspace back into its project's trunk.
    /// Only a nested (jj-workspace) row can merge; a top-level project flashes a
    /// note. The bookmark is resolved server-side on `y`, so the confirm names a
    /// generic `trunk`. A no-op with nothing selected.
    fn confirm_merge(&mut self, sidebar: &Sidebar) {
        let Some((id, name, is_child)) = self.selected_workspace(sidebar).map(|w| {
            let is_child = w
                .parent
                .is_some_and(|p| self.workspaces.iter().any(|o| o.id == p));
            (w.id, w.name.clone(), is_child)
        }) else {
            return;
        };
        if !is_child {
            self.set_status("only workspaces merge".into());
            return;
        }
        self.mode = Mode::ConfirmMerge(id);
        self.set_status(format!("merge {name} into trunk? y/N"));
    }

    /// Update the selected entry's workspace when its working copy is stale:
    /// dispatch `WorkspaceUpdate` and flash `updating <name>…`. A non-stale row
    /// just flashes `not stale`; the server broadcasts the cleared tag.
    fn update_selected(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        let Some((id, name, stale)) = self
            .selected_workspace(sidebar)
            .map(|w| (w.id, w.name.clone(), w.stale))
        else {
            return Vec::new();
        };
        if !stale {
            self.set_status("not stale".into());
            return Vec::new();
        }
        self.set_status(format!("updating {name}…"));
        vec![control(&Request::WorkspaceUpdate { id })]
    }

    /// Open the kill confirm for the selected row. An agent row targets only its
    /// own pane — never the workspace around it, so a stray `x` on an agent can't
    /// take a whole project down; a workspace row targets the workspace (with the
    /// discard option). A no-op with nothing selected.
    fn confirm_kill_selected(&mut self, sidebar: &Sidebar) {
        match sidebar.entries.get(self.sidebar.selected) {
            Some(SidebarEntry::Agent(a)) => {
                self.mode = Mode::ConfirmKillAgent(a.pane);
                self.set_status(format!("kill {} · {}? y/N", a.kind, a.project_name));
            }
            Some(SidebarEntry::Workspace(_)) => self.confirm_kill_workspace(sidebar),
            None => self.set_status("nothing selected".into()),
        }
    }

    /// Open the kill confirm for the selected workspace row in the transient
    /// line. `y` kills it, `D` also discards a fork's checkout (the server
    /// refuses discard for a workspace it did not fork, surfacing that error),
    /// any other key cancels. A no-op with nothing selected.
    fn confirm_kill_workspace(&mut self, sidebar: &Sidebar) {
        let Some((id, name)) = self
            .selected_workspace(sidebar)
            .map(|w| (w.id, w.name.clone()))
        else {
            self.set_status("nothing selected".into());
            return;
        };
        self.mode = Mode::ConfirmKillWorkspace(id);
        self.set_status(format!("kill {name}? y · D discard checkout · N cancel"));
    }

    /// Resolve the sidebar workspace-kill confirm: `y` kills, `D` kills and
    /// discards the fork's checkout, any other key cancels. Either way the
    /// sidebar keeps focus; the view refresh drops a killed row.
    pub(super) fn on_key_confirm_workspace(
        &mut self,
        key: KeyEvent,
        id: WorkspaceId,
    ) -> Vec<WireFrame> {
        let discard = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => false,
            KeyCode::Char('D') => true,
            _ => {
                self.mode = Mode::Sidebar;
                self.status = None;
                return Vec::new();
            }
        };
        self.mode = Mode::Sidebar;
        self.status = None;
        // The target may have died between the confirm opening and the answer;
        // re-resolve it rather than sending a kill for a stale id.
        if !self.workspaces.iter().any(|w| w.id == id) {
            self.set_status("that workspace is already gone".into());
            return Vec::new();
        }
        vec![control(&Request::WorkspaceKill { id, discard })]
    }

    /// Resolve the sidebar agent-kill confirm: `y` kills the agent's pane, any
    /// other key cancels. Either way the sidebar keeps focus; the view refresh
    /// drops the killed row.
    pub(super) fn on_key_confirm_agent(&mut self, key: KeyEvent, pane: PaneId) -> Vec<WireFrame> {
        self.mode = Mode::Sidebar;
        self.status = None;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Re-resolve the pane before killing: it may have exited or
                // been killed by another client since the confirm opened.
                if self.tab_of_pane(pane).is_none() {
                    self.set_status("that agent is already gone".into());
                    return Vec::new();
                }
                vec![control(&Request::PaneKill { pane })]
            }
            _ => Vec::new(),
        }
    }

    /// Resolve the merge confirm: `y` dispatches the `WorkspaceMerge` (letting the
    /// server pick the trunk bookmark and push if a remote exists) and arms the
    /// cleanup confirm the `Merged` reply will raise; any other key cancels.
    pub(super) fn on_key_confirm_merge(
        &mut self,
        key: KeyEvent,
        id: WorkspaceId,
    ) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Sidebar;
                let name = self.workspace_name(id).unwrap_or_default();
                self.set_status(format!("merging {name}…"));
                let merge = Request::WorkspaceMerge { id, push: true };
                vec![self.follow(&merge, FollowUp::Merge(id))]
            }
            _ => {
                self.mode = Mode::Sidebar;
                self.status = None;
                Vec::new()
            }
        }
    }

    /// Resolve the post-merge cleanup confirm: `y` discards the merged workspace
    /// (`WorkspaceKill --discard`), any other key keeps it on disk. Either way the
    /// sidebar keeps focus.
    pub(super) fn on_key_confirm_cleanup(
        &mut self,
        key: KeyEvent,
        id: WorkspaceId,
    ) -> Vec<WireFrame> {
        self.mode = Mode::Sidebar;
        self.status = None;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                vec![control(&Request::WorkspaceKill { id, discard: true })]
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn in_sidebar(&self, col: u16, row: u16) -> bool {
        self.sidebar.rect.is_some_and(|r| contains(r, col, row))
    }

    /// A left-click inside the sidebar: focus it, then act on what the row is — a
    /// section header toggles that section's collapse, an entry selects and jumps
    /// to it, and a border/blank just focuses.
    pub(super) fn sidebar_click(&mut self, row: u16) -> Vec<WireFrame> {
        if self.sidebar_prompt_active() {
            return Vec::new();
        }
        if !self.sidebar_focused() {
            self.mode = Mode::Sidebar;
            self.sidebar.selected = 0;
            self.sidebar.anchor = None;
        }
        let Some(rect) = self.sidebar.rect else {
            return Vec::new();
        };
        let sidebar = self.sidebar();
        let rel = row.saturating_sub(rect.y) as usize;
        if let Some(section) = sidebar.header_at_row(rel) {
            self.toggle_section(section);
            return Vec::new();
        }
        match sidebar.entry_at_row(rel) {
            Some(idx) => {
                self.select_entry(&sidebar, idx);
                self.jump_to_selected(&sidebar)
            }
            None => Vec::new(),
        }
    }

    /// A right-click inside the sidebar: focus it, select the row under the
    /// pointer, and peek at it — the mouse twin of `p`.
    pub(super) fn sidebar_peek_click(&mut self, row: u16) {
        let Some(rect) = self.sidebar.rect else {
            return;
        };
        if self.sidebar_prompt_active() {
            return;
        }
        let sidebar = self.sidebar();
        if let Some(idx) = sidebar.entry_at_row(row.saturating_sub(rect.y) as usize) {
            self.mode = Mode::Sidebar;
            self.select_entry(&sidebar, idx);
            self.open_peek(&sidebar);
        }
    }

    /// Collapse or expand a sidebar section (projects, agents, or waiting).
    fn toggle_section(&mut self, section: Section) {
        match section {
            Section::Projects => self.sidebar.collapsed_projects = !self.sidebar.collapsed_projects,
            Section::Waiting => self.sidebar.collapsed_waiting = !self.sidebar.collapsed_waiting,
        }
    }
}

/// The identity anchor for entry `idx`: its ident plus the project it belongs
/// to (a workspace row's own project, an agent row's owner) — the fallback
/// target when the row itself vanishes.
fn anchor_at(sidebar: &Sidebar, idx: usize) -> Option<(EntryIdent, WorkspaceId)> {
    let ident = sidebar.ident_at(idx)?;
    let project = match sidebar.entries.get(idx)? {
        SidebarEntry::Workspace(w) => w.project,
        SidebarEntry::Agent(a) => a.project,
    };
    Some((ident, project))
}

/// The next selectable entry at or after `from`, skipping entries whose section
/// is collapsed; stays on `from` when nothing visible lies ahead. With no
/// section collapsed this is a plain `from + 1` clamped to the last entry.
fn next_visible(sidebar: &Sidebar, from: usize) -> usize {
    if sidebar.is_empty() {
        return 0;
    }
    let last = sidebar.len() - 1;
    let mut i = from;
    while i < last {
        i += 1;
        if sidebar.is_visible(i) {
            return i;
        }
    }
    from
}

/// The previous visible entry before `from`, skipping collapsed sections; stays
/// on `from` when nothing visible lies behind.
fn prev_visible(sidebar: &Sidebar, from: usize) -> usize {
    let mut i = from;
    while i > 0 {
        i -= 1;
        if sidebar.is_visible(i) {
            return i;
        }
    }
    from
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::app::tagged;
    use crate::attach::fixtures::*;
    use crate::config::Config;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use tutti_core::{DIFF_RANGE, Direction, Event, Response};

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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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

    fn held(id: u64, summary: &str) -> PendingApproval {
        PendingApproval {
            id,
            tool: "Bash".into(),
            summary: summary.into(),
        }
    }

    /// Each held approval queues on its own row, in the order the requests
    /// came across every pane, ahead of the blocked and done rows — whatever
    /// the holding pane's state — and the tree row offers the oldest.
    #[test]
    fn each_held_approval_queues_first_in_the_order_it_came() {
        let mut view = two_workspace_view();
        // api's agent holds two while its parallel subagents keep it working;
        // pane 4, blocked, holds one that came between them.
        view[0].tabs[0].panes[0].approvals = vec![held(7, "touch a.txt"), held(9, "touch c.txt")];
        view[1].tabs[0].panes[1].approvals = vec![held(8, "touch b.txt")];
        let sidebar = build(&view, Some(TabId(2)), None);
        let waiting: Vec<(u64, Option<u64>)> = sidebar.entries[sidebar.tree_count..]
            .iter()
            .map(|e| match e {
                SidebarEntry::Agent(a) => (a.pane.0, a.approval.as_ref().map(|p| p.id)),
                _ => panic!("expected waiting agent rows"),
            })
            .collect();
        assert_eq!(
            waiting,
            vec![(1, Some(7)), (4, Some(8)), (1, Some(9)), (3, None)],
            "pane 4 queues once per approval, never again for its state"
        );
        let SidebarEntry::Agent(tree) = &sidebar.entries[1] else {
            panic!("expected api's agent row");
        };
        assert_eq!(tree.approval, Some(held(7, "touch a.txt")));
    }

    #[test]
    fn idents_distinguish_the_tree_row_from_its_waiting_copy() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        assert_eq!(
            sidebar.ident_at(0),
            Some(EntryIdent::Workspace(WorkspaceId(1)))
        );
        assert_eq!(sidebar.ident_at(4), Some(EntryIdent::Agent(PaneId(4))));
        assert_eq!(
            sidebar.ident_at(5),
            Some(EntryIdent::Waiting(PaneId(4), None))
        );
        assert_eq!(
            sidebar.index_of(EntryIdent::Waiting(PaneId(4), None)),
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
        let sidebar = build(&view, Some(TabId(1)), None);
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

    /// From another project, a project's row goes to the tab it last
    /// showed; one never shown here goes to a tab with something running
    /// before one whose program exited.
    #[test]
    fn a_workspace_row_goes_back_to_the_tab_it_last_showed() {
        let mut exited = shell(1);
        exited.exited = Some(0);
        let api_tabs = vec![
            tab(1, "omp", false, leaf(1), vec![exited]),
            tab(2, "claude", false, leaf(2), vec![shell(2)]),
            tab(3, "zsh", false, leaf(3), vec![shell(3)]),
        ];
        let view = vec![
            workspace(1, "api", Some("main"), api_tabs),
            workspace(
                2,
                "web",
                None,
                vec![tab(4, "4", true, leaf(4), vec![shell(4)])],
            ),
        ];
        let jump = |app: &App| match &app.sidebar().entries[0] {
            SidebarEntry::Workspace(api) => api.jump_tab,
            SidebarEntry::Agent(_) => panic!("expected api's row"),
        };
        let mut app = App::new();
        attach_with(&mut app, view);
        app.focus_change();
        assert_eq!(jump(&app), TabId(2), "the first tab with a live pane");

        app.jump_to(TabId(3), None);
        app.focus_change();
        app.jump_to(TabId(4), None);
        app.focus_change();
        assert_eq!(jump(&app), TabId(3), "where api was left");
    }

    #[test]
    fn build_marks_the_active_workspace_and_jump_targets() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        let sidebar = build(&view, Some(TabId(2)), None);
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
        let sidebar = build(&view, Some(TabId(2)), None);
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
        let sidebar = build(&view, Some(TabId(1)), None);
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
    fn shorten_home_prefers_tilde_and_skips_empty_dirs() {
        let home = || Some(Path::new("/Users/me"));
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
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        let SidebarEntry::Agent(blocked) = &sidebar.entries[4] else {
            panic!("expected agent row");
        };
        assert_eq!(blocked.pane, PaneId(4));
        assert_eq!(blocked.tab, TabId(2), "agent jump carries its owning tab");
        assert_eq!(blocked.state, AgentState::Blocked);
    }

    #[test]
    fn a_sandbox_agent_row_says_where_it_runs() {
        let mut view = two_workspace_view();
        let pane = &mut view[1].tabs[0].panes[1];
        assert_eq!(pane.state, AgentState::Blocked);
        pane.sandbox = Some("gke".into());
        let sidebar = build(&view, Some(TabId(2)), None);
        let kinds: Vec<&str> = sidebar
            .entries
            .iter()
            .filter_map(|e| match e {
                SidebarEntry::Agent(a) if a.pane == PaneId(4) => Some(a.kind.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            ["claude on gke", "claude on gke"],
            "the tree row and its waiting-queue row both"
        );
    }

    #[test]
    fn entry_at_row_maps_clicks_past_the_frame_headers() {
        let sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
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
        // Row 11 is the spacer, 12 the waiting divider, then two rows per
        // waiting entry.
        assert_eq!(sidebar.entry_at_row(11), None);
        assert_eq!(sidebar.header_at_row(11), None);
        assert_eq!(sidebar.entry_at_row(12), None);
        assert_eq!(sidebar.header_at_row(12), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(13), Some(5));
        assert_eq!(sidebar.entry_at_row(15), Some(6));
        // Past the last waiting entry.
        assert_eq!(sidebar.entry_at_row(17), None);
    }

    #[test]
    fn collapsing_projects_shifts_the_waiting_divider_up() {
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        sidebar.projects_collapsed = true;
        assert_eq!(sidebar.entry_at_row(1), None, "tree rows are hidden");
        // No tree body, no spacer — the divider sits right below the header.
        assert_eq!(sidebar.header_at_row(1), Some(Section::Waiting));
        // Waiting rows follow immediately after the divider.
        assert_eq!(sidebar.entry_at_row(2), Some(5));
        assert_eq!(sidebar.entry_at_row(4), Some(6));
        assert!(!sidebar.is_visible(0), "a tree entry is not visible");
        assert!(sidebar.is_visible(5), "a waiting entry stays visible");
    }

    #[test]
    fn collapsing_waiting_hides_the_queue_rows() {
        let mut sidebar = build(&two_workspace_view(), Some(TabId(2)), None);
        sidebar.waiting_collapsed = true;
        assert_eq!(sidebar.entry_at_row(1), Some(0), "tree rows still map");
        assert_eq!(sidebar.header_at_row(12), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(13), None, "waiting rows are hidden");
        assert!(!sidebar.is_visible(5), "a waiting entry is not visible");
        assert!(sidebar.is_visible(0), "a tree entry stays visible");
    }

    #[test]
    fn build_carries_subagents_onto_the_tree_row() {
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
        // Row 9 is the spacer, 10 the waiting divider (the done agent queues
        // there), pushed down past the subagent sub-rows; its entry sits at
        // rows 11-12.
        assert_eq!(sidebar.entry_at_row(9), None, "the spacer");
        assert_eq!(sidebar.entry_at_row(10), None, "the waiting divider");
        assert_eq!(sidebar.header_at_row(10), Some(Section::Waiting));
        assert_eq!(sidebar.entry_at_row(11), Some(3));
        assert_eq!(sidebar.entry_at_row(13), None, "past the last waiting row");
    }

    #[test]
    fn sidebar_visibility_follows_config_and_width() {
        fn auto() -> App {
            App::with_config(Config::parse("sidebar = \"auto\"\n").unwrap())
        }

        // auto: hidden for a single plain workspace.
        let mut app = auto();
        attached(&mut app);
        let (sb, panes, _) = app.split_content(Rect::new(0, 0, 100, 24));
        assert!(sb.is_none(), "auto hides an agentless single workspace");
        assert_eq!(panes.width, 100);

        // auto: shown with multiple workspaces, panes shrink by the column width.
        let mut app = auto();
        attach_with(&mut app, view_two_workspaces());
        let (sb, panes, _) = app.split_content(Rect::new(0, 0, 100, 24));
        assert_eq!(sb.unwrap().width, 30);
        assert_eq!(panes.width, 70);
        assert_eq!(panes.x, 30);

        // width floor suppresses it regardless of contents.
        let (sb, panes, _) = app.split_content(Rect::new(0, 0, 79, 24));
        assert!(sb.is_none(), "below the width floor the sidebar is hidden");
        assert_eq!(panes.width, 79);

        // on (the default): shown even for a plain single workspace.
        let mut app = App::new();
        attached(&mut app);
        assert!(
            app.split_content(Rect::new(0, 0, 100, 24)).0.is_some(),
            "on is the default and shows for a single workspace"
        );

        // off: hidden even with multiple workspaces.
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        attach_with(&mut app, view_two_workspaces());
        assert!(app.split_content(Rect::new(0, 0, 100, 24)).0.is_none());
    }

    #[test]
    fn sidebar_key_focuses_and_selection_crosses_the_section_boundary() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        assert_eq!(app.mode, Mode::Sidebar);
        assert_eq!(app.sidebar.selected, 0);

        app.on_key(plain('j')); // second workspace — the filter follows it
        assert_eq!(app.sidebar.selected, 1);
        app.on_key(plain('j')); // crosses into the agents section
        assert_eq!(app.sidebar.selected, 2);
        assert!(matches!(app.sidebar().entries[2], SidebarEntry::Agent(_)));
        app.on_key(plain('j')); // last agent
        assert_eq!(app.sidebar.selected, 3);
        app.on_key(plain('j')); // crosses into the waiting section
        assert_eq!(app.sidebar.selected, 4);
        app.on_key(plain('j')); // clamped at the end
        assert_eq!(app.sidebar.selected, 4);
        app.on_key(plain('k')); // back up into the agents
        assert_eq!(app.sidebar.selected, 3);

        // esc unfocuses; the pane keeps its own focus.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn every_projects_agents_stay_visible_regardless_of_the_cursor() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        // web's agents hang under web in the tree even while the cursor sits on
        // api — there is no cursor-driven filter to reshuffle the rows.
        let before = app.sidebar();
        assert_eq!(before.tree_count, 4, "api, web, and web's two agents");
        app.on_key(plain('j')); // highlight web
        assert_eq!(
            app.sidebar().entries,
            before.entries,
            "moving the cursor never changes the tree's shape"
        );
    }

    #[test]
    fn a_waiting_row_cursor_stays_in_the_queue_across_moves() {
        let mut view = view_two_workspaces();
        view.push(workspace(
            3,
            "zed",
            None,
            vec![tab(
                3,
                "3",
                false,
                leaf(4),
                vec![agent(4, "claude", AgentState::Blocked)],
            )],
        ));
        let mut app = App::new();
        attach_with(&mut app, view);
        focus_sidebar(&mut app);
        // Tree: api, web, web's agents 2+3, zed, zed's agent 4; waiting:
        // blocked panes 3 then 4.
        let sidebar = app.sidebar();
        assert_eq!(sidebar.tree_count, 6);
        for _ in 0..6 {
            app.on_key(plain('j'));
        }
        let SidebarEntry::Agent(a) = &sidebar.entries[app.sidebar.selected] else {
            panic!("expected the first waiting row");
        };
        assert_eq!(
            a.pane,
            PaneId(3),
            "blocked panes lead the queue in pane order"
        );
    }

    #[test]
    fn enter_on_a_workspace_selects_its_tab() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // second workspace, jumps to tab 2
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.active_tab, Some(TabId(2)));
    }

    #[test]
    fn enter_on_an_agent_selects_its_tab_and_focuses_its_pane() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('j')); // web's first agent in pane order: pane 2, tab 2
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.active_tab, Some(TabId(2)));
        assert_eq!(app.focused, Some(PaneId(2)));
        // The PaneFocus follows from the focus-change the event loop drains.
        assert_eq!(
            app.focus_change(),
            Some(control(&Request::PaneFocus {
                pane: PaneId(2),
                seen: false
            })),
        );
    }

    #[test]
    fn n_on_a_workspace_row_jumps_to_it_and_opens_the_launcher_for_it() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // select the second workspace (web, tab 2)
        let out = app.on_key(plain('n'));
        assert_eq!(
            out,
            vec![control(&Request::TabSelect { id: TabId(2) })],
            "n jumps to the selected workspace's tab, the same routing as Enter"
        );
        assert_eq!(app.active_tab, Some(TabId(2)));
        assert_eq!(
            app.mode,
            Mode::Launcher,
            "and lands in the launcher so the next choice runs there"
        );
        assert_eq!(
            app.launcher_title(),
            " run in web ",
            "the launcher title names the workspace it will run in"
        );
    }

    #[test]
    fn n_on_an_agent_row_targets_the_workspace_that_owns_it() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('j')); // first agent row, living in web's tab 2
        assert!(matches!(app.sidebar().entries[2], SidebarEntry::Agent(_)));
        let out = app.on_key(plain('n'));
        assert_eq!(
            out,
            vec![control(&Request::TabSelect { id: TabId(2) })],
            "an agent row targets the tab of the workspace that owns it"
        );
        assert_eq!(app.active_tab, Some(TabId(2)));
        assert_eq!(app.mode, Mode::Launcher);
        assert_eq!(
            app.launcher_title(),
            " run in web ",
            "and names that owning workspace in the launcher title"
        );
    }

    #[test]
    fn d_on_a_jj_workspace_opens_an_ephemeral_diff_pane() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        // A workspace dir that is a jj repo (holds a `.jj`).
        let dir = std::env::temp_dir().join(format!("tutti-app-jj-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join(".jj")).unwrap();

        let mut app = App::new();
        let mut ws = workspace(
            1,
            "api",
            Some("main"),
            vec![tab(1, "1", true, leaf(1), vec![shell(1)])],
        );
        ws.dir = dir.clone();
        attach_with(&mut app, vec![ws]);
        focus_sidebar(&mut app);

        let out = app.on_key(plain('d'));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: Some(TabId(1)),
                cmd: diff_pager_cmd(None),
                ephemeral: true,
                sandbox: None,
                source: None,
            })],
            "d spawns the ephemeral jj-diff pane in the workspace's tab"
        );
        assert!(
            diff_pager_cmd(None)[2].contains(DIFF_RANGE),
            "the pager diffs against trunk, not the bare working copy"
        );
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.active_tab, Some(TabId(1)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn d_on_a_non_jj_workspace_shows_a_transient_error() {
        let mut app = App::new();
        // view_two_workspaces roots workspaces at `/tmp/w`, which is not a jj repo.
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        let out = app.on_key(plain('d'));
        assert!(out.is_empty(), "no pane is spawned for a non-jj workspace");
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("not a jj workspace")),
            "a transient error names the missing jj repo"
        );
        assert_eq!(app.mode, Mode::Sidebar, "focus stays on the sidebar");
    }

    #[test]
    fn f_is_gone_from_the_sidebar_dispatch() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        let out = app.on_key(plain('f'));
        assert!(out.is_empty(), "f sends nothing");
        assert_eq!(app.mode, Mode::Sidebar, "f no longer opens any prompt");
    }

    #[test]
    fn m_on_a_child_workspace_confirms_merges_then_offers_cleanup() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[1].parent = Some(WorkspaceId(1)); // web (id 2) nested under api
        attach_with(&mut app, view);
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // select the nested child (web, id 2)

        // `m` raises the confirm, sending nothing yet.
        let out = app.on_key(plain('m'));
        assert!(out.is_empty(), "m opens the confirm, sending nothing");
        assert_eq!(app.mode, Mode::ConfirmMerge(WorkspaceId(2)));
        assert!(
            app.transient().is_some_and(|t| t.contains("merge web")),
            "the confirm names the workspace"
        );

        // `y` dispatches the merge (push requested; the server owns the bookmark),
        // tagged so its reply alone completes the follow-up.
        let out = app.on_key(plain('y'));
        assert_eq!(
            out,
            vec![tagged(
                &Request::WorkspaceMerge {
                    id: WorkspaceId(2),
                    push: true,
                },
                1
            )],
            "y sends the merge request"
        );

        // The Merged reply flashes the outcome and raises the cleanup confirm.
        app.handle_response(
            Some(1),
            Response::Merged {
                pushed: false,
                bookmark: "main".into(),
            },
        );
        assert_eq!(app.mode, Mode::ConfirmCleanup(WorkspaceId(2)));
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("merged into main") && t.contains("clean up")),
            "the transient reports the merge and offers cleanup"
        );

        // `y` discards the now-merged workspace.
        let out = app.on_key(plain('y'));
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceKill {
                id: WorkspaceId(2),
                discard: true,
            })],
            "cleanup discards the merged workspace"
        );
        assert_eq!(app.mode, Mode::Sidebar);
    }

    #[test]
    fn merged_reply_notes_a_push_when_it_happened() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[1].parent = Some(WorkspaceId(1));
        attach_with(&mut app, view);
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('m'));
        app.on_key(plain('y'));
        app.handle_response(
            Some(1),
            Response::Merged {
                pushed: true,
                bookmark: "main".into(),
            },
        );
        assert!(
            app.transient().is_some_and(|t| t.contains("and pushed")),
            "a pushed merge says so"
        );
    }

    #[test]
    fn merge_cleanup_declined_keeps_the_workspace() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[1].parent = Some(WorkspaceId(1));
        attach_with(&mut app, view);
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('m'));
        app.on_key(plain('y'));
        app.handle_response(
            Some(1),
            Response::Merged {
                pushed: false,
                bookmark: "main".into(),
            },
        );
        // `n` at the cleanup confirm keeps the workspace on disk.
        let out = app.on_key(plain('n'));
        assert!(out.is_empty(), "declining cleanup sends nothing");
        assert_eq!(app.mode, Mode::Sidebar);
    }

    #[test]
    fn m_on_a_project_row_flashes_only_workspaces_merge() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces()); // both top-level projects
        focus_sidebar(&mut app); // selection rests on api (a project, no parent)
        let out = app.on_key(plain('m'));
        assert!(out.is_empty(), "a project row merges nothing");
        assert_eq!(app.mode, Mode::Sidebar, "no confirm is raised");
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("only workspaces merge")),
            "a project row flashes the guidance"
        );
    }

    #[test]
    fn merge_confirm_cancels_on_any_other_key() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[1].parent = Some(WorkspaceId(1));
        attach_with(&mut app, view);
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('m'));
        let out = app.on_key(plain('n'));
        assert!(out.is_empty(), "n merges nothing");
        assert_eq!(app.mode, Mode::Sidebar);
    }

    #[test]
    fn u_on_a_stale_workspace_row_emits_a_workspace_update() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[0].stale = true; // the api workspace's working copy is stale
        attach_with(&mut app, view);
        focus_sidebar(&mut app);

        let out = app.on_key(plain('u'));
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceUpdate { id: WorkspaceId(1) })],
            "u updates the selected stale workspace"
        );
        assert!(
            app.transient().is_some_and(|t| t.contains("updating")),
            "the transient names the workspace being updated"
        );
    }

    #[test]
    fn u_on_a_non_stale_row_flashes_and_emits_nothing() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces()); // none stale
        focus_sidebar(&mut app);
        let out = app.on_key(plain('u'));
        assert!(out.is_empty(), "a healthy workspace sends no update");
        assert!(
            app.transient().is_some_and(|t| t.contains("not stale")),
            "a non-stale row flashes `not stale`"
        );
    }

    #[test]
    fn x_on_a_workspace_row_opens_the_kill_confirm() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app); // selection rests on the api workspace (id 1)
        let out = app.on_key(plain('x'));
        assert!(out.is_empty(), "x opens the confirm, sending nothing yet");
        assert_eq!(app.mode, Mode::ConfirmKillWorkspace(WorkspaceId(1)));
        assert!(
            app.transient().is_some_and(|t| t.contains("kill api?")),
            "the confirm names the workspace and its options"
        );
    }

    #[test]
    fn kill_confirm_y_emits_a_plain_workspace_kill() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('x'));
        let out = app.on_key(plain('y'));
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceKill {
                id: WorkspaceId(1),
                discard: false,
            })],
            "y kills the workspace without discarding its checkout"
        );
        assert_eq!(app.mode, Mode::Sidebar, "focus returns to the sidebar");
    }

    #[test]
    fn kill_confirm_shift_d_discards_the_forks_checkout() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('x'));
        let out = app.on_key(plain('D'));
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceKill {
                id: WorkspaceId(1),
                discard: true,
            })],
            "D kills and discards; the server refuses discard for a non-fork"
        );
    }

    #[test]
    fn kill_confirm_cancels_on_esc_or_any_other_key() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);

        app.on_key(plain('x'));
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty(), "esc kills nothing");
        assert_eq!(app.mode, Mode::Sidebar);

        app.on_key(plain('x'));
        let out = app.on_key(plain('n'));
        assert!(out.is_empty(), "any non-y/D key cancels");
        assert_eq!(app.mode, Mode::Sidebar);
    }

    #[test]
    fn x_on_an_agent_row_kills_only_that_pane() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // web workspace
        app.on_key(plain('j')); // web's first agent (pane 2, tab 2 → workspace 2)
        app.on_key(plain('x'));
        assert_eq!(
            app.mode,
            Mode::ConfirmKillAgent(PaneId(2)),
            "x on an agent row confirms killing the pane, never the workspace"
        );
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("kill claude · web?")),
            "the confirm names the agent and its project"
        );
        let out = app.on_key(plain('y'));
        assert_eq!(
            out,
            vec![control(&Request::PaneKill { pane: PaneId(2) })],
            "y kills the agent's pane only"
        );
        assert_eq!(app.mode, Mode::Sidebar, "focus returns to the sidebar");
    }

    #[test]
    fn agent_kill_confirm_cancels_on_any_other_key() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('j'));
        app.on_key(plain('x'));
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty(), "esc kills nothing");
        assert_eq!(app.mode, Mode::Sidebar);
    }

    #[test]
    fn selection_follows_the_row_identity_across_a_view_change() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // the web workspace row

        // A new workspace lands ahead of web in the view; the cursor must stay
        // on web, not on whatever now occupies its old index.
        let mut view = view_two_workspaces();
        view.insert(
            1,
            workspace(
                3,
                "mid",
                None,
                vec![tab(3, "3", false, leaf(9), vec![shell(9)])],
            ),
        );
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        let sidebar = app.sidebar();
        let SidebarEntry::Workspace(w) = &sidebar.entries[app.sidebar.selected] else {
            panic!("expected a workspace row");
        };
        assert_eq!(w.name, "web", "the cursor followed the row's identity");
    }

    #[test]
    fn a_killed_agents_cursor_falls_to_its_project_row() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // web row
        app.on_key(plain('j')); // web's first agent (pane 2)

        // The agent dies: its row vanishes and the sidebar reshuffles. The
        // cursor falls to the agent's project row — never to the unrelated row
        // that now sits at the old index.
        let mut view = view_two_workspaces();
        view[1].tabs[0].layout = Some(leaf(3));
        view[1].tabs[0].panes = vec![agent(3, "claude", AgentState::Blocked)];
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        let sidebar = app.sidebar();
        let SidebarEntry::Workspace(w) = &sidebar.entries[app.sidebar.selected] else {
            panic!("expected the project row, got an agent row");
        };
        assert_eq!(
            w.name, "web",
            "the killed agent's cursor lands on its project"
        );
    }

    #[test]
    fn confirm_y_after_the_workspace_vanished_sends_nothing() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('x')); // confirm kill of api (id 1)

        // api dies under the confirm (another client, a crash) — y must not
        // fire a kill at the stale id.
        let mut view = view_two_workspaces();
        view.remove(0);
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        let out = app.on_key(plain('y'));
        assert!(out.is_empty(), "no kill is sent for a vanished workspace");
        assert!(
            app.transient().is_some_and(|t| t.contains("gone")),
            "the miss is reported, not swallowed"
        );
    }

    #[test]
    fn the_sidebar_holds_steady_during_a_kill_confirm() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // web row
        let before = app.sidebar();
        app.on_key(plain('x'));
        assert!(matches!(app.mode, Mode::ConfirmKillWorkspace(_)));
        assert_eq!(
            app.sidebar().entries,
            before.entries,
            "opening the confirm must not reshuffle the rows under the question"
        );
        assert!(
            app.sidebar_focused(),
            "the confirm keeps sidebar focus so the highlight stays visible"
        );
    }

    #[test]
    fn capital_y_confirms_a_kill() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('x'));
        let out = app.on_key(plain('Y'));
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceKill {
                id: WorkspaceId(1),
                discard: false,
            })],
            "a shifted Y is a confirm, not a silent cancel"
        );
    }

    #[test]
    fn a_shorter_view_clamps_the_sidebar_selection() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('j'));
        app.on_key(plain('j')); // the last entry (the working agent)
        assert_eq!(app.sidebar.selected, 3);

        // A killed workspace shrinks the view to a single agentless row.
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged {
                workspaces: view_one_pane(),
            })
            .unwrap(),
        ));
        assert_eq!(
            app.sidebar.selected, 0,
            "the selection clamps onto the surviving row"
        );
    }

    #[test]
    fn sidebar_ctrl_l_returns_to_the_pane_area_keeping_focus() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        let focused = app.focused;
        let out = app.on_key(ctrl('l'));
        assert!(out.is_empty(), "returning to the pane emits nothing");
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(
            app.focused, focused,
            "the previously-focused pane is restored"
        );
    }

    #[test]
    fn sidebar_ctrl_h_wraps_to_the_previous_tab() {
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        focus_sidebar(&mut app);
        let out = app.on_key(ctrl('h'));
        assert_eq!(
            out,
            vec![control(&Request::TabSelect { id: TabId(2) })],
            "ctrl+h in the sidebar keeps the old previous-tab behaviour"
        );
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.active_tab, Some(TabId(2)));
    }

    #[test]
    fn panes_offset_by_the_sidebar_and_mouse_hit_tests_past_it() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        app.active_tab = Some(TabId(2)); // the two-pane workspace
        app.sync_sizes(Rect::new(0, 0, 100, 24));

        let left = app.rects.iter().find(|(p, _)| *p == PaneId(2)).unwrap().1;
        assert_eq!(left.x, 30, "the leftmost pane sits right of the sidebar");

        assert!(
            app.pane_at(32, 5).is_some(),
            "clicks past the sidebar and below the tab bar hit a pane"
        );
        assert!(
            app.pane_at(5, 5).is_none(),
            "clicks inside the sidebar miss the panes"
        );
    }

    #[test]
    fn clicking_a_sidebar_entry_focuses_and_jumps() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        // The sidebar frame starts at content row 2 (below the app-bar header).
        // Within it: projects header (border) rel 0, workspace 0 rel 1-2,
        // workspace 1 rel 3-4 — so screen row 2 + 3 = 5 hits the second workspace.
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 2, 5);
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.active_tab, Some(TabId(2)));
    }

    #[test]
    fn clicking_the_sidebar_background_focuses_it_without_jumping() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        // A blank filler row deep in the frame (past the entries, above the
        // bottom border) is background — not a header or an entry.
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 2, 20);
        assert!(out.is_empty(), "a background click jumps nowhere");
        assert!(app.sidebar_focused(), "but it does focus the sidebar");
    }

    #[test]
    fn clicking_a_section_header_toggles_its_collapse() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        // The projects header is the sidebar's top border, at screen row 2.
        assert!(!app.sidebar().projects_collapsed);
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert!(out.is_empty(), "toggling a section jumps nowhere");
        assert!(
            app.sidebar().projects_collapsed,
            "clicking the projects header collapses it"
        );
        assert!(app.sidebar_focused(), "and focuses the sidebar");
        // Clicking it again expands.
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert!(!app.sidebar().projects_collapsed);
    }
}
