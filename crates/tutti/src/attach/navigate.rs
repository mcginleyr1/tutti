//! Navigate mode's model: the full-screen go-to list. With no filter it is an
//! overview — every agent grouped by state, `needs you` (blocked) first, then
//! `done` and `working` — over every project as a tree in sidebar order: the
//! project, its tabs and their panes, then each workspace nested under it with
//! its own. Typing filters every tree row by a fuzzy subsequence match over the
//! path it sits on and collapses the grouping into one flat ranked list. The
//! model is pure — no rendering, no app state — so the grouping, matching, and
//! ranking are unit-tested in isolation; the overlay's own state (`Navigate`)
//! and `App`'s keys and clicks over it follow.

use std::cmp::Reverse;
use std::collections::HashSet;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use tutti_core::{AgentState, Frame as WireFrame, PaneId, TabId, WorkspaceId, WorkspaceView};

use super::app::{App, Mode, contains};
use super::sidebar::kind_label;

/// The overview's state groups, in attention order — also the filter's
/// tiebreak, so the two never disagree about what is most urgent.
const GROUPS: [(&str, AgentState); 3] = [
    ("needs you", AgentState::Blocked),
    ("done", AgentState::Done),
    ("working", AgentState::Working),
];

/// One go-to row: a section header with its count, the blank spacer that sets
/// a section off from the one above, or an entry enter can jump to.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header { title: &'static str, count: usize },
    Spacer,
    Entry(Entry),
}

/// An entry's stable identity, so the highlight follows the thing rather than
/// the position when a state change regroups the overview. An agent's copy in
/// a state group is distinct from its tree row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ident {
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
    Grouped(PaneId),
}

/// What leads an entry: a workspace's dot (lit while it owns the active tab),
/// a tab's app-bar number, or a pane's state dot — `None` for a plain pane,
/// which has no state to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Workspace { active: bool },
    Tab(usize),
    Pane(Option<AgentState>),
}

/// One selectable row: a project or nested workspace, a tab, or a pane.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub ident: Ident,
    /// The tab enter selects: a pane's or tab's own; for a workspace, the
    /// active tab when it owns it, else its first — the sidebar row's jump.
    pub tab: TabId,
    pub marker: Marker,
    /// The name at full intensity: the workspace's, the tab's, or the pane's
    /// title.
    pub name: String,
    /// The dim text after the name. In the tree, an agent's kind (dropped when
    /// the title already says it); a grouped or filtered row leads with where
    /// it lives — `workspace · tab` for a pane — since no tree surrounds it.
    pub meta: String,
    /// The tree indent and `├`/`└` guide in the projects section; 0 and
    /// `None` on grouped and filtered rows.
    pub depth: u8,
    pub guide: Option<char>,
    /// Where the row lives, which a flat row's meta leads with.
    context: String,
    /// What the filter matches: the project, workspace, tab name, pane title,
    /// and agent kind along the row's path, in that order.
    text: String,
}

impl Entry {
    /// The pane enter focuses — a pane row's own; `None` for a workspace or a
    /// tab, whose jump keeps the tab's own focus.
    pub fn pane(&self) -> Option<PaneId> {
        match self.ident {
            Ident::Pane(pane) | Ident::Grouped(pane) => Some(pane),
            Ident::Workspace(_) | Ident::Tab(_) => None,
        }
    }

    /// This tree entry dressed flat, for a state group or a filtered list: no
    /// indent or guide, and its context joined onto the meta so the row still
    /// says where it lives.
    fn flat(mut self) -> Entry {
        self.meta = [self.context.as_str(), self.meta.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        self.depth = 0;
        self.guide = None;
        self
    }
}

/// Build the go-to rows. An empty `query` gives the overview: each non-empty
/// state group under its header, then the `projects` tree, every section past
/// the first behind a spacer — empty sections omitted, so a quiet session
/// opens on its tree. Otherwise every tree entry the query matches, flat and
/// headerless: the best match first, ties to the more urgent state, then to
/// tree order. `active_tab` lights its workspace's dot and steers where a
/// workspace row jumps.
pub fn build(workspaces: &[WorkspaceView], active_tab: Option<TabId>, query: &str) -> Vec<Row> {
    let tree = tree(workspaces, active_tab);
    if !query.is_empty() {
        let mut hits: Vec<(u32, usize, Entry)> = tree
            .into_iter()
            .filter_map(|e| Some((score(query, &e.text)?, urgency(e.marker), e)))
            .collect();
        // A stable sort, so equal matches keep their tree order.
        hits.sort_by_key(|&(score, urgency, _)| (Reverse(score), urgency));
        return hits
            .into_iter()
            .map(|(_, _, e)| Row::Entry(e.flat()))
            .collect();
    }
    let mut rows = Vec::new();
    let mut section = |title: &'static str, count: usize, entries: Vec<Entry>| {
        if entries.is_empty() {
            return;
        }
        if !rows.is_empty() {
            rows.push(Row::Spacer);
        }
        rows.push(Row::Header { title, count });
        rows.extend(entries.into_iter().map(Row::Entry));
    };
    for (title, state) in GROUPS {
        let agents: Vec<Entry> = tree
            .iter()
            .filter(|e| e.marker == Marker::Pane(Some(state)))
            .map(|e| {
                let mut grouped = e.clone().flat();
                grouped.ident = Ident::Grouped(e.pane().expect("a state dot marks a pane"));
                grouped
            })
            .collect();
        section(title, agents.len(), agents);
    }
    let projects = tree
        .iter()
        .filter(|e| matches!(e.marker, Marker::Workspace { .. }))
        .count();
    section("projects", projects, tree);
    rows
}

/// Every tree entry in sidebar order: each top-level project, its tabs and
/// their panes (in layout order), then each workspace nested under it with
/// its own — a nested workspace whose project is gone stands as a project. A
/// workspace without a tab has nowhere to jump, so it gets no row.
fn tree(workspaces: &[WorkspaceView], active_tab: Option<TabId>) -> Vec<Entry> {
    let present: HashSet<WorkspaceId> = workspaces.iter().map(|w| w.id).collect();
    // App-bar order, which numbers the tab segments.
    let bar: Vec<TabId> = workspaces
        .iter()
        .flat_map(|w| &w.tabs)
        .map(|t| t.id)
        .collect();
    let mut entries = Vec::new();
    let mut push = |w: &WorkspaceView, project: Option<&WorkspaceView>| {
        let depth = u8::from(project.is_some());
        let path = match project {
            Some(p) => format!("{} {}", p.name, w.name),
            None => w.name.clone(),
        };
        let owns = active_tab.filter(|at| w.tabs.iter().any(|t| t.id == *at));
        if let Some(tab) = owns.or_else(|| w.tabs.first().map(|t| t.id)) {
            entries.push(Entry {
                ident: Ident::Workspace(w.id),
                tab,
                marker: Marker::Workspace {
                    active: owns.is_some(),
                },
                name: w.name.clone(),
                meta: String::new(),
                depth,
                guide: None,
                context: project.map_or_else(String::new, |p| p.name.clone()),
                text: path.clone(),
            });
        }
        for t in &w.tabs {
            entries.push(Entry {
                ident: Ident::Tab(t.id),
                tab: t.id,
                marker: Marker::Tab(bar.iter().position(|id| *id == t.id).map_or(0, |i| i + 1)),
                name: t.name.clone(),
                meta: String::new(),
                depth: depth + 1,
                guide: None,
                context: w.name.clone(),
                text: format!("{path} {}", t.name),
            });
            for pane in &t.panes {
                let kind = pane.agent.as_ref().map(|a| kind_label(a, pane));
                let mut text = format!("{path} {} {}", t.name, pane.title);
                if let Some(kind) = &kind {
                    text = format!("{text} {kind}");
                }
                entries.push(Entry {
                    ident: Ident::Pane(pane.id),
                    tab: t.id,
                    marker: Marker::Pane(pane.agent.is_some().then_some(pane.state)),
                    name: pane.title.clone(),
                    meta: kind.filter(|k| *k != pane.title).unwrap_or_default(),
                    depth: depth + 2,
                    guide: None,
                    context: format!("{} · {}", w.name, t.name),
                    text,
                });
            }
        }
    };
    for project in workspaces
        .iter()
        .filter(|w| !w.parent.is_some_and(|p| present.contains(&p)))
    {
        push(project, None);
        for child in workspaces.iter().filter(|c| c.parent == Some(project.id)) {
            push(child, Some(project));
        }
    }
    // A row's guide: `├` while a sibling follows — a later row at its depth
    // before the tree climbs back above it — else `└`.
    for i in 0..entries.len() {
        let depth = entries[i].depth;
        if depth > 0 {
            let sibling = entries[i + 1..]
                .iter()
                .map(|e| e.depth)
                .find(|&d| d <= depth)
                == Some(depth);
            entries[i].guide = Some(if sibling { '├' } else { '└' });
        }
    }
    entries
}

/// How well `query` matches `text` as a case-insensitive subsequence — higher
/// is better, `None` when it does not match at all. Each matched char scores
/// 1, plus 2 when it directly follows the previous match (a run) and 3 when it
/// starts a word (the text's head, or after a space or punctuation). The match
/// is tried from every occurrence of the query's first char, keeping the best,
/// so a later clean run beats an early scattered one.
pub(super) fn score(query: &str, text: &str) -> Option<u32> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let first = *query.first()?;
    (0..text.len())
        .filter(|&start| text[start] == first)
        .filter_map(|start| {
            let (mut score, mut next, mut prev) = (0, start, None);
            for &c in &query {
                let at = (next..text.len()).find(|&i| text[i] == c)?;
                let run = prev.is_some_and(|p: usize| p + 1 == at);
                let word = at == 0 || !text[at - 1].is_alphanumeric();
                score += 1 + 2 * u32::from(run) + 3 * u32::from(word);
                prev = Some(at);
                next = at + 1;
            }
            Some(score)
        })
        .max()
}

/// An entry's attention rank: its state group's position — blocked, done,
/// working — with everything else after (idle and unknown agents, plain
/// panes, tabs, workspaces).
fn urgency(marker: Marker) -> usize {
    GROUPS
        .iter()
        .position(|(_, state)| marker == Marker::Pane(Some(*state)))
        .unwrap_or(GROUPS.len())
}

/// The go-to overlay's state, which means something only in `Mode::Navigate`.
#[derive(Default)]
pub(super) struct Navigate {
    /// The go-to overlay's filter text.
    pub(super) query: String,
    /// The highlighted go-to row's identity, captured whenever the highlight
    /// moves, so an agent changing state (which regroups the overview) or a
    /// closing pane never slides it onto another row. `None` — on opening and
    /// after every filter edit — highlights the first entry: the most urgent
    /// agent, or the best match.
    anchor: Option<Ident>,
    /// The go-to overlay (the content area) from the last size sync, for mouse
    /// hit-testing. `None` while not navigating.
    pub(super) rect: Option<Rect>,
}

impl App {
    /// Open the go-to overlay: no filter, the first entry highlighted.
    pub(super) fn open_navigate(&mut self) {
        self.mode = Mode::Navigate;
        self.navigate.query.clear();
        self.navigate.anchor = None;
    }

    /// Keys in the go-to overlay. Printable characters edit the filter and
    /// backspace deletes, each edit re-highlighting the first entry (the best
    /// match); `↑`/`↓` and `C-p`/`C-n` walk the highlight past the headers;
    /// enter jumps to it and `esc` closes (esc is the back key throughout).
    /// The prefix chord still reaches across, closing the overlay into prefix
    /// mode.
    pub(super) fn on_key_navigate(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if self.enter_prefix(key) {
            return Vec::new();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Terminal;
                self.status = None;
            }
            KeyCode::Enter => {
                let (rows, selected) = self.navigate_rows();
                if let Some(Row::Entry(e)) = selected.map(|i| &rows[i]) {
                    return self.jump_to(e.tab, e.pane());
                }
            }
            KeyCode::Down => self.navigate_step(true),
            KeyCode::Up => self.navigate_step(false),
            KeyCode::Char('n') if ctrl => self.navigate_step(true),
            KeyCode::Char('p') if ctrl => self.navigate_step(false),
            KeyCode::Backspace => {
                self.navigate.query.pop();
                self.navigate.anchor = None;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.navigate.query.push(c);
                self.navigate.anchor = None;
            }
            _ => {}
        }
        Vec::new()
    }

    /// The go-to rows for the current view and filter, with the index of the
    /// highlighted entry: the anchored row while it is still listed (a grouped
    /// agent that left every group falls to its tree row), else the first
    /// entry. `None` when nothing is listed.
    pub fn navigate_rows(&self) -> (Vec<Row>, Option<usize>) {
        let rows = build(&self.workspaces, self.active_tab, &self.navigate.query);
        let at = |ident: Ident| {
            rows.iter()
                .position(|r| matches!(r, Row::Entry(e) if e.ident == ident))
        };
        let anchored = self.navigate.anchor.and_then(|anchor| match anchor {
            Ident::Grouped(pane) => at(anchor).or_else(|| at(Ident::Pane(pane))),
            _ => at(anchor),
        });
        let selected = anchored.or_else(|| rows.iter().position(|r| matches!(r, Row::Entry(_))));
        (rows, selected)
    }

    /// Walk the go-to highlight to the next entry down (or up), past headers
    /// and spacers; it stays put at either end.
    pub(super) fn navigate_step(&mut self, down: bool) {
        let (rows, selected) = self.navigate_rows();
        let Some(at) = selected else {
            return;
        };
        let is_entry = |r: &Row| matches!(r, Row::Entry(_));
        let next = if down {
            rows[at + 1..].iter().position(is_entry).map(|i| at + 1 + i)
        } else {
            rows[..at].iter().rposition(is_entry)
        };
        if let Some(Row::Entry(e)) = next.map(|i| &rows[i]) {
            self.navigate.anchor = Some(e.ident);
        }
    }

    /// A left-click in the go-to overlay jumps to the entry under the
    /// pointer; the input row, a header or spacer, empty space, and anything
    /// outside the list do nothing.
    pub(super) fn navigate_click(&mut self, col: u16, row: u16) -> Vec<WireFrame> {
        let Some(area) = self.navigate.rect else {
            return Vec::new();
        };
        let (rows, selected) = self.navigate_rows();
        let (list, first) = App::navigate_list(area, selected);
        if !contains(list, col, row) {
            return Vec::new();
        }
        match rows.get(first + (row - list.y) as usize) {
            Some(Row::Entry(e)) => self.jump_to(e.tab, e.pane()),
            _ => Vec::new(),
        }
    }

    /// The go-to overlay's list within the overlay `area` (the content
    /// region): the rect its rows draw in, below the filter input and a spacer
    /// row, and the first row shown — 0 until the highlight would leave the
    /// window, then following it. The one geometry the renderer and the click
    /// hit-test share, so a click lands on what is drawn.
    pub fn navigate_list(area: Rect, selected: Option<usize>) -> (Rect, usize) {
        let top = 2.min(area.height);
        let list = Rect::new(area.x, area.y + top, area.width, area.height - top);
        let first = selected
            .unwrap_or(0)
            .saturating_sub((list.height as usize).saturating_sub(1));
        (list, first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::app::control;
    use crate::attach::fixtures::*;
    use crate::config::Config;
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    use tutti_core::{Direction, Request};

    /// `api` (tab 1, the active tab: a working claude and a plain zsh), its
    /// nested workspace `api-feat` (tab 3: a blocked claude) — listed first in
    /// the view, so it takes the app bar's first number, yet nested under api
    /// in the tree — and `web` (tab 2: a done codex and a blocked claude).
    fn view() -> Vec<WorkspaceView> {
        let mut feat = workspace(
            3,
            "api-feat",
            None,
            vec![tab(
                3,
                "3",
                false,
                leaf(5),
                vec![agent(5, "claude", AgentState::Blocked)],
            )],
        );
        feat.parent = Some(WorkspaceId(1));
        vec![
            feat,
            workspace(
                1,
                "api",
                Some("main"),
                vec![tab(
                    1,
                    "1",
                    true,
                    split(Direction::Horizontal, leaf(1), leaf(2)),
                    vec![
                        agent(1, "claude", AgentState::Working),
                        pane(2, "zsh", None, AgentState::Idle),
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
                    false,
                    split(Direction::Horizontal, leaf(3), leaf(4)),
                    vec![
                        agent(3, "codex", AgentState::Done),
                        agent(4, "claude", AgentState::Blocked),
                    ],
                )],
            ),
        ]
    }

    fn entries(rows: &[Row]) -> Vec<&Entry> {
        rows.iter()
            .filter_map(|r| match r {
                Row::Entry(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    fn idents(rows: &[Row]) -> Vec<Ident> {
        entries(rows).iter().map(|e| e.ident).collect()
    }

    #[test]
    fn the_overview_groups_agents_by_state_blocked_first_over_the_tree() {
        let rows = build(&view(), Some(TabId(1)), "");
        let headers: Vec<(&str, usize)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Header { title, count } => Some((*title, *count)),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers,
            [
                ("needs you", 2),
                ("done", 1),
                ("working", 1),
                ("projects", 3)
            ]
        );
        assert_eq!(
            rows[0],
            Row::Header {
                title: "needs you",
                count: 2
            },
            "the first section leads, no spacer above it"
        );
        assert_eq!(
            rows.iter().filter(|r| **r == Row::Spacer).count(),
            3,
            "every later section is set off by a spacer"
        );
        assert_eq!(
            idents(&rows)[..4],
            [
                Ident::Grouped(PaneId(5)),
                Ident::Grouped(PaneId(4)),
                Ident::Grouped(PaneId(3)),
                Ident::Grouped(PaneId(1)),
            ],
            "both blocked agents in tree order, then done, then working"
        );

        // A state nobody is in leaves no empty section behind.
        let mut quiet = view();
        quiet[2].tabs[0].panes[0].state = AgentState::Working;
        let rows = build(&quiet, Some(TabId(1)), "");
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, Row::Header { title: "done", .. })),
            "no done agents, no done header: {rows:?}"
        );
    }

    #[test]
    fn the_tree_runs_each_project_its_tabs_and_panes_then_its_nested_workspaces() {
        let rows = build(&view(), Some(TabId(1)), "");
        let tree = &entries(&rows)[4..];
        let shape: Vec<(Ident, u8, Option<char>)> =
            tree.iter().map(|e| (e.ident, e.depth, e.guide)).collect();
        assert_eq!(
            shape,
            [
                (Ident::Workspace(WorkspaceId(1)), 0, None),
                (Ident::Tab(TabId(1)), 1, Some('├')),
                (Ident::Pane(PaneId(1)), 2, Some('├')),
                (Ident::Pane(PaneId(2)), 2, Some('└')),
                (Ident::Workspace(WorkspaceId(3)), 1, Some('└')),
                (Ident::Tab(TabId(3)), 2, Some('└')),
                (Ident::Pane(PaneId(5)), 3, Some('└')),
                (Ident::Workspace(WorkspaceId(2)), 0, None),
                (Ident::Tab(TabId(2)), 1, Some('└')),
                (Ident::Pane(PaneId(3)), 2, Some('├')),
                (Ident::Pane(PaneId(4)), 2, Some('└')),
            ],
            "api's tab and panes, plain zsh included, then api-feat nested under it, then web"
        );
        assert_eq!(tree[0].marker, Marker::Workspace { active: true });
        assert_eq!(tree[7].marker, Marker::Workspace { active: false });
        assert_eq!(
            (tree[1].marker, tree[5].marker, tree[8].marker),
            (Marker::Tab(2), Marker::Tab(1), Marker::Tab(3)),
            "tabs carry their app-bar numbers, which follow the view's order"
        );
        assert_eq!(
            tree[3].marker,
            Marker::Pane(None),
            "a plain pane has no state"
        );
        assert_eq!(tree[2].meta, "claude", "an agent's tree row names its kind");
        assert_eq!(
            (tree[0].tab, tree[4].tab, tree[7].tab),
            (TabId(1), TabId(3), TabId(2)),
            "a workspace jumps to the active tab it owns, else its first"
        );
    }

    #[test]
    fn grouped_and_filtered_rows_say_where_they_live() {
        let mut view = view();
        view[1].tabs[0].panes[0].title = "claude".into();
        let rows = build(&view, Some(TabId(1)), "");
        let blocked = entries(&rows)[0];
        assert_eq!(
            (blocked.name.as_str(), blocked.meta.as_str()),
            ("pane-5", "api-feat · 3 · claude"),
            "workspace · tab · kind"
        );
        assert_eq!((blocked.depth, blocked.guide), (0, None));
        assert_eq!((blocked.tab, blocked.pane()), (TabId(3), Some(PaneId(5))));
        assert_eq!(
            entries(&rows)[3].meta,
            "api · 1",
            "a pane titled for its agent drops the stuttered kind"
        );

        let rows = build(&view, Some(TabId(1)), "feat");
        let metas: Vec<&str> = entries(&rows).iter().map(|e| e.meta.as_str()).collect();
        assert_eq!(
            metas,
            ["api-feat · 3 · claude", "api", "api-feat"],
            "a pane names its workspace and tab, a nested workspace its project, a tab its workspace"
        );
    }

    #[test]
    fn filtering_is_a_case_insensitive_fuzzy_subsequence_match_over_the_path() {
        let view = view();
        let rows = build(&view, Some(TabId(1)), "FEAT");
        assert!(
            rows.iter().all(|r| matches!(r, Row::Entry(_))),
            "filtering collapses the grouping: {rows:?}"
        );
        assert_eq!(
            idents(&rows),
            [
                Ident::Pane(PaneId(5)),
                Ident::Workspace(WorkspaceId(3)),
                Ident::Tab(TabId(3)),
            ],
            "api-feat's rows only — its blocked agent ahead of the equal workspace and tab"
        );
        assert_eq!(
            idents(&build(&view, Some(TabId(1)), "web cl")),
            [Ident::Pane(PaneId(4))],
            "a subsequence across the path: web's claude, not its codex"
        );
        assert!(build(&view, Some(TabId(1)), "xyz").is_empty());
    }

    #[test]
    fn a_tighter_match_outranks_tree_order() {
        let view = vec![workspace(
            1,
            "w",
            None,
            vec![tab(
                1,
                "1",
                true,
                split(Direction::Horizontal, leaf(1), leaf(2)),
                vec![
                    pane(1, "tests", None, AgentState::Idle),
                    pane(2, "the status", None, AgentState::Idle),
                ],
            )],
        )];
        assert_eq!(
            idents(&build(&view, Some(TabId(1)), "ts")),
            [Ident::Pane(PaneId(2)), Ident::Pane(PaneId(1))],
            "`ts` starts both words of \"the status\" but trails mid-word in \"tests\""
        );
    }

    #[test]
    fn equal_matches_rank_by_urgency_then_tree_order() {
        assert_eq!(
            idents(&build(&view(), Some(TabId(1)), "claude")),
            [
                Ident::Pane(PaneId(5)),
                Ident::Pane(PaneId(4)),
                Ident::Pane(PaneId(1)),
            ],
            "the blocked two in tree order, then the working one"
        );
        assert_eq!(
            idents(&build(&view(), Some(TabId(1)), "web")),
            [
                Ident::Pane(PaneId(4)),
                Ident::Pane(PaneId(3)),
                Ident::Workspace(WorkspaceId(2)),
                Ident::Tab(TabId(2)),
            ],
            "blocked, done, then the rest of web in tree order"
        );
    }

    #[test]
    fn score_rewards_runs_and_word_starts_and_keeps_the_best_start() {
        assert_eq!(score("ba", "ab"), None, "order matters");
        assert_eq!(score("API", "api"), score("api", "api"), "case never does");
        assert!(
            score("cl", "claude") > score("cl", "cool"),
            "a run beats a gap"
        );
        assert!(
            score("c", "x claude") > score("c", "xc"),
            "a word start beats mid-word"
        );
        assert_eq!(
            score("ab", "a xb ab"),
            Some(7),
            "the later clean run, not the first scattered match"
        );
    }

    /// `view_two_workspaces` with the go-to overlay open over a 100x24
    /// terminal. Its rows: the needs-you header over pane 3, a spacer, the
    /// working header over pane 2, a spacer, then the projects header over
    /// api, tab 1, pane 1, web, tab 2, pane 2, pane 3.
    fn navigate_fixture(config: Config) -> App {
        let mut app = App::with_config(config);
        attach_with(&mut app, view_two_workspaces());
        app.on_key(ctrl('b'));
        app.on_key(plain('g'));
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        app
    }

    /// The identity of the highlighted go-to entry.
    fn navigate_selected(app: &App) -> Option<Ident> {
        let (rows, selected) = app.navigate_rows();
        match selected.map(|i| &rows[i]) {
            Some(Row::Entry(e)) => Some(e.ident),
            _ => None,
        }
    }

    #[test]
    fn prefix_g_opens_the_go_to_in_both_presets_and_esc_or_the_prefix_leaves_it() {
        for config in [Config::default(), vim_config()] {
            let mut app = navigate_fixture(config);
            assert_eq!(app.mode, Mode::Navigate);
            assert_eq!(
                navigate_selected(&app),
                Some(Ident::Grouped(PaneId(3))),
                "the blocked agent leads"
            );
            let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(out, Vec::new());
            assert_eq!(app.mode, Mode::Terminal);
            assert_eq!(
                (app.active_tab, app.focused),
                (Some(TabId(1)), Some(PaneId(1))),
                "closing moves nothing"
            );
        }
        // The prefix reaches across, closing the overlay into prefix mode.
        let mut app = navigate_fixture(Config::default());
        app.on_key(ctrl('b'));
        assert!(matches!(app.mode, Mode::Prefix(_)));
        app.on_key(plain('w'));
        assert_eq!(app.mode, Mode::Sidebar);
    }

    #[test]
    fn typing_filters_the_go_to_and_backspace_edits_the_filter() {
        let mut app = navigate_fixture(Config::default());
        app.on_key(plain('j'));
        app.on_key(plain('k'));
        assert_eq!(app.navigate.query, "jk", "j and k type; they do not move");
        assert!(app.navigate_rows().0.is_empty(), "nothing matches j-k");
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.on_key(backspace);
        app.on_key(backspace);
        assert_eq!(app.navigate.query, "");
        app.on_key(plain('w'));
        app.on_key(plain('e'));
        let (rows, _) = app.navigate_rows();
        assert!(
            rows.iter().all(|r| matches!(r, Row::Entry(_))),
            "filtering collapses the groups into one flat list"
        );
        assert_eq!(
            navigate_selected(&app),
            Some(Ident::Pane(PaneId(3))),
            "the best match is highlighted — web's rows tie, its blocked agent first"
        );
    }

    #[test]
    fn arrows_and_ctrl_n_p_walk_the_go_to_highlight_past_the_headers() {
        let mut app = navigate_fixture(Config::default());
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.on_key(down);
        assert_eq!(
            navigate_selected(&app),
            Some(Ident::Grouped(PaneId(2))),
            "past the spacer and the working header"
        );
        app.on_key(ctrl('n'));
        assert_eq!(
            navigate_selected(&app),
            Some(Ident::Workspace(WorkspaceId(1))),
            "on into the projects tree"
        );
        app.on_key(up);
        assert_eq!(navigate_selected(&app), Some(Ident::Grouped(PaneId(2))));
        app.on_key(ctrl('p'));
        assert_eq!(navigate_selected(&app), Some(Ident::Grouped(PaneId(3))));
        app.on_key(up);
        assert_eq!(
            navigate_selected(&app),
            Some(Ident::Grouped(PaneId(3))),
            "the first entry stays put"
        );
        for _ in 0..20 {
            app.on_key(down);
        }
        assert_eq!(
            navigate_selected(&app),
            Some(Ident::Pane(PaneId(3))),
            "and so does the last"
        );
    }

    #[test]
    fn enter_on_a_go_to_pane_row_selects_its_tab_and_focuses_it() {
        let mut app = navigate_fixture(Config::default());
        app.focus_change(); // drain the attach-time focus report
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(
            (app.active_tab, app.focused),
            (Some(TabId(2)), Some(PaneId(3)))
        );
        assert_eq!(
            app.focus_change(),
            Some(control(&Request::PaneFocus {
                pane: PaneId(3),
                seen: false
            })),
            "the same focus report a sidebar jump sends"
        );
    }

    #[test]
    fn enter_on_a_go_to_project_or_tab_row_selects_its_tab() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        // Five steps down is web's project row, six its tab: either selects
        // tab 2, focusing the tab's own active pane.
        for (steps, row) in [
            (5, Ident::Workspace(WorkspaceId(2))),
            (6, Ident::Tab(TabId(2))),
        ] {
            let mut app = navigate_fixture(Config::default());
            for _ in 0..steps {
                app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            assert_eq!(navigate_selected(&app), Some(row));
            assert_eq!(
                app.on_key(enter),
                vec![control(&Request::TabSelect { id: TabId(2) })]
            );
            assert_eq!(
                (app.mode, app.active_tab, app.focused),
                (Mode::Terminal, Some(TabId(2)), Some(PaneId(2)))
            );
        }
    }

    #[test]
    fn the_go_to_highlight_follows_its_agent_across_a_regroup() {
        let mut app = navigate_fixture(Config::default());
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // pane 2
        state_changed(&mut app, PaneId(2), AgentState::Working, AgentState::Done);
        assert_eq!(
            navigate_selected(&app),
            Some(Ident::Grouped(PaneId(2))),
            "regrouped under done, still highlighted"
        );
        state_changed(&mut app, PaneId(2), AgentState::Done, AgentState::Idle);
        assert_eq!(
            navigate_selected(&app),
            Some(Ident::Pane(PaneId(2))),
            "out of every group, it falls to its own tree row"
        );
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.focused, Some(PaneId(2)));
    }

    #[test]
    fn clicking_a_go_to_row_jumps_and_the_wheel_walks_the_highlight() {
        let mut app = navigate_fixture(Config::default());
        app.on_mouse(MouseEventKind::ScrollDown, 50, 10);
        assert_eq!(navigate_selected(&app), Some(Ident::Grouped(PaneId(2))));
        app.on_mouse(MouseEventKind::ScrollUp, 50, 10);
        assert_eq!(navigate_selected(&app), Some(Ident::Grouped(PaneId(3))));
        // The overlay is the content area (rows 2-22); its list starts on row
        // 4 below the input and a spacer: the needs-you header, pane 3, a
        // spacer, the working header, pane 2, … pane 3's tree row on 17.
        let click = MouseEventKind::Down(MouseButton::Left);
        for (col, row) in [(50, 4), (50, 2), (50, 0), (5, 23), (50, 20)] {
            // A header, the input, the app bar, the footer, past the last row.
            assert_eq!(app.on_mouse(click, col, row), Vec::new());
            assert_eq!(
                app.mode,
                Mode::Navigate,
                "a click at {col},{row} does nothing"
            );
        }
        let out = app.on_mouse(click, 50, 8);
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!((app.mode, app.focused), (Mode::Terminal, Some(PaneId(2))));
    }
}
