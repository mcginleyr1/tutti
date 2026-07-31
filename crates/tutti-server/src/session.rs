//! Server-side session state: the workspace/tab/pane tree plus one persistent
//! `PtyPane` per pane. Pure model — it owns no sockets and speaks no protocol,
//! it just answers the operations the dispatcher maps requests onto and reports
//! which panes it touched so the caller can drive the pty lifecycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tutti_core::{
    AgentHookEvent, AgentKind, AgentState, Direction, Layout, Observation, PaneId, PaneInfo,
    StateEvent, SubagentInfo, TabId, TabInfo, TabView, WorkspaceId, WorkspaceInfo, WorkspaceView,
};

use crate::pty::{PaneSize, PtyPane, PtySpec};

/// The most subagent rows kept per pane before the oldest is dropped, bounding
/// the list even for an agent that spawns many short-lived subagents.
const SUBAGENT_CAP: usize = 16;

struct TabEntry {
    id: TabId,
    name: String,
    /// `None` until the tab's first pane exists — `tutti_core::Layout` cannot
    /// represent an empty tab.
    layout: Option<Layout>,
    active_pane: Option<PaneId>,
}

struct WorkspaceEntry {
    id: WorkspaceId,
    name: String,
    dir: PathBuf,
    /// The last-computed jj change stat (`4 files +120 −33`), refreshed off the
    /// hot path. `None` until probed, when not a jj repo, or when clean.
    changes: Option<String>,
    /// Whether the last stale probe found this workspace's jj working copy stale
    /// (its `@` was rewritten elsewhere). Only forks ever go stale in practice.
    stale: bool,
    /// Present when tutti created this workspace via `workspace fork`. Carries
    /// what a `--discard` kill needs to `jj workspace forget` it at its origin.
    fork: Option<ForkMeta>,
    tabs: Vec<TabEntry>,
}

/// What a forked workspace remembers about its origin, so `kill --discard` can
/// forget it from the repo it was forked out of and remove its checkout.
#[derive(Clone)]
pub struct ForkMeta {
    /// The `.jj` repo root the fork was added from — where `jj workspace forget`
    /// must run (a workspace cannot forget itself).
    pub origin_root: PathBuf,
    /// The jj workspace name (`jj workspace add --name`), the forget argument.
    pub jj_name: String,
}

struct PaneSlot {
    meta: PaneInfo,
    pty: Arc<PtyPane>,
    tab: TabId,
    /// An ephemeral pane is removed outright when its child exits (the reaper
    /// drops it from the layout) instead of being kept as an exited corpse.
    ephemeral: bool,
    /// Set once this pane has reported an agent hook event. From then on the
    /// screen-heuristic classifier skips it — the hooks are ground truth and
    /// would otherwise fight the exact signals.
    hook_seen: bool,
}

#[derive(Default)]
struct Ids {
    workspace: u64,
    tab: u64,
    pane: u64,
}

impl Ids {
    fn workspace(&mut self) -> WorkspaceId {
        self.workspace += 1;
        WorkspaceId(self.workspace)
    }
    fn tab(&mut self) -> TabId {
        self.tab += 1;
        TabId(self.tab)
    }
    fn pane(&mut self) -> PaneId {
        self.pane += 1;
        PaneId(self.pane)
    }
}

pub struct Session {
    workspaces: Vec<WorkspaceEntry>,
    panes: HashMap<PaneId, PaneSlot>,
    current_tab: Option<TabId>,
    size: PaneSize,
    ids: Ids,
    /// The session name, exported to every spawned pane as `TUTTI_SESSION` so a
    /// Claude Code hook running inside it can reach this daemon's socket.
    name: String,
}

impl Session {
    pub fn new(size: PaneSize, name: impl Into<String>) -> Self {
        Self {
            workspaces: Vec::new(),
            panes: HashMap::new(),
            current_tab: None,
            size,
            ids: Ids::default(),
            name: name.into(),
        }
    }

    pub fn workspace_new(&mut self, dir: PathBuf) -> WorkspaceId {
        self.push_workspace(dir, None).0
    }

    /// Create a workspace at `dir` marked as a fork, returning its id and the id
    /// of its (empty) first tab so the caller can spawn the fork's shell pane
    /// into exactly that tab.
    pub fn workspace_new_forked(&mut self, dir: PathBuf, fork: ForkMeta) -> (WorkspaceId, TabId) {
        self.push_workspace(dir, Some(fork))
    }

    /// Push a fresh workspace (with its first empty tab, made current) and return
    /// its ids. Shared by the plain and forked constructors.
    fn push_workspace(&mut self, dir: PathBuf, fork: Option<ForkMeta>) -> (WorkspaceId, TabId) {
        let id = self.ids.workspace();
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("workspace-{id}"));
        let tab = self.new_tab_entry();
        let tab_id = tab.id;
        self.current_tab = Some(tab_id);
        self.workspaces.push(WorkspaceEntry {
            id,
            name,
            dir,
            changes: None,
            stale: false,
            fork,
            tabs: vec![tab],
        });
        (id, tab_id)
    }

    /// The fork metadata for workspace `id`, or `None` for a plain (non-forked)
    /// workspace or an unknown id. Drives whether a `--discard` kill is allowed.
    pub fn workspace_fork_meta(&self, id: WorkspaceId) -> Option<ForkMeta> {
        self.workspaces
            .iter()
            .find(|w| w.id == id)
            .and_then(|w| w.fork.clone())
    }

    /// Store a freshly-probed stale flag for workspace `id`, returning whether it
    /// moved (so the caller only rebroadcasts on a real change).
    pub fn set_stale(&mut self, id: WorkspaceId, stale: bool) -> bool {
        match self.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) if w.stale != stale => {
                w.stale = stale;
                true
            }
            _ => false,
        }
    }

    /// The directory of workspace `id`, for probing its VCS off the hot path.
    pub fn workspace_dir(&self, id: WorkspaceId) -> Option<PathBuf> {
        self.workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.dir.clone())
    }

    /// Every workspace's id, for a refresh pass that recomputes their stats.
    pub fn workspace_ids(&self) -> Vec<WorkspaceId> {
        self.workspaces.iter().map(|w| w.id).collect()
    }

    /// Store a freshly-computed change stat for workspace `id`. Returns whether
    /// it moved (so the caller only rebroadcasts the view when something changed,
    /// and a vanished workspace is a silent no-op).
    pub fn set_changes(&mut self, id: WorkspaceId, changes: Option<String>) -> bool {
        match self.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) if w.changes != changes => {
                w.changes = changes;
                true
            }
            _ => false,
        }
    }

    /// Whether `pane` is ephemeral (torn down on child exit rather than kept).
    pub fn is_ephemeral(&self, pane: PaneId) -> bool {
        self.panes.get(&pane).is_some_and(|s| s.ephemeral)
    }

    pub fn workspace_list(&self) -> Vec<WorkspaceInfo> {
        self.workspaces
            .iter()
            .map(|w| WorkspaceInfo {
                id: w.id,
                name: w.name.clone(),
                dir: w.dir.clone(),
            })
            .collect()
    }

    /// Remove a workspace and every pane it owns. Returns the killed panes so
    /// the caller can tear down their reaper tasks / broadcast state.
    pub fn workspace_kill(&mut self, id: WorkspaceId) -> Result<Vec<PaneId>> {
        let index = self
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .with_context(|| format!("no workspace {id}"))?;
        let workspace = self.workspaces.remove(index);
        let mut killed = Vec::new();
        for tab in &workspace.tabs {
            for pane in tab.layout.as_ref().map(Layout::panes).unwrap_or_default() {
                if let Some(slot) = self.panes.remove(&pane) {
                    let _ = slot.pty.kill();
                    killed.push(pane);
                }
            }
            if self.current_tab == Some(tab.id) {
                self.current_tab = None;
            }
        }
        if self.current_tab.is_none() {
            self.current_tab = self
                .workspaces
                .last()
                .and_then(|w| w.tabs.last())
                .map(|t| t.id);
        }
        Ok(killed)
    }

    pub fn tab_new(&mut self, workspace: Option<WorkspaceId>) -> Result<TabId> {
        let workspace = self.resolve_workspace(workspace)?;
        let tab = self.new_tab_entry();
        let id = tab.id;
        let ws = self
            .workspaces
            .iter_mut()
            .find(|w| w.id == workspace)
            .with_context(|| format!("no workspace {workspace}"))?;
        ws.tabs.push(tab);
        self.current_tab = Some(id);
        Ok(id)
    }

    pub fn tab_list(&self, workspace: Option<WorkspaceId>) -> Result<Vec<TabInfo>> {
        let workspace = self.resolve_workspace(workspace)?;
        let ws = self
            .workspaces
            .iter()
            .find(|w| w.id == workspace)
            .with_context(|| format!("no workspace {workspace}"))?;
        Ok(ws
            .tabs
            .iter()
            .map(|t| TabInfo {
                id: t.id,
                name: self.tab_display_name(t),
                active: self.current_tab == Some(t.id),
            })
            .collect())
    }

    pub fn tab_select(&mut self, id: TabId) -> Result<()> {
        if !self
            .workspaces
            .iter()
            .any(|w| w.tabs.iter().any(|t| t.id == id))
        {
            bail!("no tab {id}");
        }
        self.current_tab = Some(id);
        Ok(())
    }

    /// Spawn `cmd` in `tab` (or the current tab when `None`), placing the
    /// new pane in the tab's layout and focusing it. An `ephemeral` pane is torn
    /// down entirely when its child exits.
    pub fn pane_run(
        &mut self,
        tab: Option<TabId>,
        cmd: Vec<String>,
        ephemeral: bool,
    ) -> Result<PaneId> {
        let tab = self.resolve_tab(tab)?;
        let (program, args) = cmd.split_first().context("empty command")?;
        let dir = self.tab_workspace_dir(tab)?;
        let mut spec = PtySpec::new(program);
        spec.args = args.to_vec();
        spec.cwd = Some(dir);
        spec.env = vec![("TERM".into(), "xterm-256color".into())];
        let title = pane_title(program);
        self.spawn_into_tab(tab, spec, title, None, ephemeral)
    }

    /// Split `pane`'s cell in its tab, spawning a login shell in the new half.
    pub fn pane_split(&mut self, pane: PaneId, direction: Direction) -> Result<PaneId> {
        let tab = self
            .panes
            .get(&pane)
            .with_context(|| format!("no pane {pane}"))?
            .tab;
        let dir = self.tab_workspace_dir(tab)?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut spec = PtySpec::new(&shell);
        spec.cwd = Some(dir);
        spec.env = vec![("TERM".into(), "xterm-256color".into())];
        let title = pane_title(&shell);
        self.spawn_into_tab(tab, spec, title, Some((pane, direction)), false)
    }

    /// The whole session as the attach protocol's view: workspaces, their tabs,
    /// each tab's layout tree and the `PaneInfo` for the panes it holds.
    pub fn view(&self) -> Vec<WorkspaceView> {
        // Each workspace's jj repo root, computed once, so a forked child can be
        // resolved to whichever workspace currently owns its origin repo (or None
        // when that origin has been killed — the child then renders top-level).
        let roots: Vec<(WorkspaceId, Option<PathBuf>)> = self
            .workspaces
            .iter()
            .map(|w| (w.id, crate::jj::workspace_root(&w.dir)))
            .collect();
        self.workspaces
            .iter()
            .map(|w| WorkspaceView {
                id: w.id,
                name: w.name.clone(),
                dir: w.dir.clone(),
                branch: git_branch(&w.dir),
                changes: w.changes.clone(),
                stale: w.stale,
                parent: w.fork.as_ref().and_then(|fork| {
                    roots.iter().find_map(|(id, root)| {
                        (*id != w.id && root.as_deref() == Some(fork.origin_root.as_path()))
                            .then_some(*id)
                    })
                }),
                tabs: w
                    .tabs
                    .iter()
                    .map(|t| TabView {
                        id: t.id,
                        name: self.tab_display_name(t),
                        active: self.current_tab == Some(t.id),
                        layout: t.layout.clone(),
                        active_pane: t.active_pane,
                        panes: t
                            .layout
                            .as_ref()
                            .map(Layout::panes)
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|id| self.panes.get(id))
                            .map(pane_info)
                            .collect(),
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn pane_list(&self) -> Vec<PaneInfo> {
        let mut panes: Vec<&PaneSlot> = self.panes.values().collect();
        panes.sort_by_key(|s| s.meta.id.0);
        panes.into_iter().map(pane_info).collect()
    }

    /// Kill and forget a pane, collapsing the split that held it.
    pub fn pane_kill(&mut self, pane: PaneId) -> Result<WorkspaceId> {
        let slot = self
            .panes
            .remove(&pane)
            .with_context(|| format!("no pane {pane}"))?;
        let _ = slot.pty.kill();
        let tab_id = slot.tab;
        let workspace = self
            .workspace_of_tab(tab_id)
            .context("pane's tab vanished")?;
        let tab = self.tab_mut(tab_id).context("pane's tab vanished")?;
        tab.layout = tab.layout.as_ref().and_then(|l| l.remove(pane));
        if tab.active_pane == Some(pane) {
            tab.active_pane = tab.layout.as_ref().map(|l| l.panes()[0]);
        }
        Ok(workspace)
    }

    /// Nudge the ratio of the nearest split enclosing `pane` whose axis is
    /// `axis`, by `delta`. Returns whether the layout actually changed (it does
    /// not when the pane has no enclosing split on that axis).
    pub fn pane_resize_split(&mut self, pane: PaneId, axis: Direction, delta: f32) -> Result<bool> {
        let tab_id = self
            .panes
            .get(&pane)
            .with_context(|| format!("no pane {pane}"))?
            .tab;
        let tab = self.tab_mut(tab_id).context("pane's tab vanished")?;
        let Some(layout) = tab.layout.as_ref() else {
            return Ok(false);
        };
        match layout.resize_split(pane, axis, delta) {
            Some(new) => {
                tab.layout = Some(new);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn pane_rename(&mut self, pane: PaneId, title: String) -> Result<()> {
        let slot = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("no pane {pane}"))?;
        slot.meta.title = title;
        Ok(())
    }

    pub fn pane_send(&self, pane: PaneId, bytes: &[u8]) -> Result<()> {
        self.pty(pane)
            .with_context(|| format!("no pane {pane}"))?
            .write_input(bytes)
    }

    pub fn pane_read(
        &self,
        pane: PaneId,
        lines: Option<usize>,
        unwrapped: bool,
    ) -> Result<Vec<String>> {
        Ok(self
            .pty(pane)
            .with_context(|| format!("no pane {pane}"))?
            .read(lines, unwrapped))
    }

    pub fn pty(&self, pane: PaneId) -> Option<Arc<PtyPane>> {
        self.panes.get(&pane).map(|s| Arc::clone(&s.pty))
    }

    pub fn panes_with_pty(&self) -> Vec<(PaneId, Arc<PtyPane>)> {
        self.panes
            .iter()
            .map(|(id, s)| (*id, Arc::clone(&s.pty)))
            .collect()
    }

    /// Live (not-yet-exited) panes and their ptys, the targets of the agent
    /// detection pass. Exited panes keep their last-known agent, so they are
    /// skipped rather than re-detected as "gone".
    pub fn live_panes(&self) -> Vec<(PaneId, Arc<PtyPane>)> {
        self.panes
            .iter()
            .filter(|(_, s)| s.meta.exited.is_none())
            .map(|(id, s)| (*id, Arc::clone(&s.pty)))
            .collect()
    }

    /// Live agent panes that are *not* hook-driven, with their kind and pty —
    /// the targets of the screen-classification pass. A pane that has reported a
    /// hook event is excluded: its state comes from exact hook signals, and the
    /// screen heuristics would only fight them. Agent detection is unaffected (it
    /// walks `live_panes`), so a hook-driven pane still shows its badge.
    pub fn agent_panes(&self) -> Vec<(PaneId, AgentKind, Arc<PtyPane>)> {
        self.panes
            .iter()
            .filter(|(_, s)| s.meta.exited.is_none() && !s.hook_seen)
            .filter_map(|(id, s)| {
                s.meta
                    .agent
                    .clone()
                    .map(|agent| (*id, agent, Arc::clone(&s.pty)))
            })
            .collect()
    }

    /// Apply a Claude Code hook event to `pane`, marking it hook-driven. Returns
    /// what moved so the caller broadcasts the right event(s): a state transition
    /// (via the shared `AgentState::apply`) drives `StateChanged`; a subagent-list
    /// change drives `LayoutChanged`. `None` for an unknown pane (a hook must
    /// never fail because its pane has gone away).
    pub fn apply_agent_event(
        &mut self,
        pane: PaneId,
        event: AgentHookEvent,
    ) -> Option<AgentEventOutcome> {
        let slot = self.panes.get_mut(&pane)?;
        slot.hook_seen = true;
        let subs = &mut slot.meta.subagents;
        let mut subagents_changed = false;
        let mut classified = None;
        match event {
            AgentHookEvent::SubagentStarted { id, desc } => {
                subs.push(SubagentInfo {
                    id,
                    desc,
                    running: true,
                });
                if subs.len() > SUBAGENT_CAP {
                    subs.remove(0);
                }
                subagents_changed = true;
            }
            AgentHookEvent::SubagentStopped { id } => {
                subagents_changed = stop_subagent(subs, &id);
            }
            AgentHookEvent::Activity { .. } => {
                classified = Some(Observation::Working);
            }
            AgentHookEvent::Blocked { .. } => {
                classified = Some(Observation::Blocked);
            }
            AgentHookEvent::Done => {
                classified = Some(Observation::Done);
                // The turn ended: sweep the finished subagents that were kept
                // around only to show their completion.
                let before = subs.len();
                subs.retain(|s| s.running);
                subagents_changed = subs.len() != before;
            }
        }
        let transition = classified.and_then(|obs| {
            let from = slot.meta.state;
            let to = from.apply(StateEvent::Classified(obs));
            slot.meta.state = to;
            (from != to).then_some((from, to))
        });
        Some(AgentEventOutcome {
            transition,
            subagents_changed,
        })
    }

    /// Record the agent kind detected for a pane. Returns whether it changed.
    pub fn set_agent(&mut self, pane: PaneId, agent: Option<AgentKind>) -> bool {
        match self.panes.get_mut(&pane) {
            Some(slot) if slot.meta.agent != agent => {
                slot.meta.agent = agent;
                true
            }
            _ => false,
        }
    }

    pub fn pane_state(&self, pane: PaneId) -> Option<AgentState> {
        self.panes.get(&pane).map(|s| s.meta.state)
    }

    /// Overwrite a pane's state. Returns whether the pane exists; the caller
    /// computes the transition (via `AgentState::apply`) and decides whether it
    /// is worth broadcasting.
    pub fn set_pane_state(&mut self, pane: PaneId, state: AgentState) -> bool {
        match self.panes.get_mut(&pane) {
            Some(slot) => {
                slot.meta.state = state;
                true
            }
            None => false,
        }
    }

    /// Make `pane` the active pane of its tab (and that tab current), as the
    /// client's focus follows.
    pub fn set_active_pane(&mut self, pane: PaneId) -> Result<()> {
        let tab_id = self
            .panes
            .get(&pane)
            .with_context(|| format!("no pane {pane}"))?
            .tab;
        self.current_tab = Some(tab_id);
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.active_pane = Some(pane);
        }
        Ok(())
    }

    pub fn workspace_of_tab(&self, tab: TabId) -> Option<WorkspaceId> {
        self.workspaces
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == tab))
            .map(|w| w.id)
    }

    pub fn workspace_of_pane(&self, pane: PaneId) -> Option<WorkspaceId> {
        let tab = self.panes.get(&pane)?.tab;
        self.workspace_of_tab(tab)
    }

    /// Record a child's exit, keeping the pane and its grid readable. A child
    /// exit is a `Done` observation, so the state advances the same way the
    /// classifier would. Returns the `(from, to)` state transition, or `None`
    /// when the pane is already gone or already marked exited.
    pub fn mark_exited(&mut self, pane: PaneId, code: i32) -> Option<(AgentState, AgentState)> {
        match self.panes.get_mut(&pane) {
            Some(slot) if slot.meta.exited.is_none() => {
                slot.meta.exited = Some(code);
                let from = slot.meta.state;
                let to = from.apply(StateEvent::Classified(Observation::Done));
                slot.meta.state = to;
                Some((from, to))
            }
            _ => None,
        }
    }

    pub fn kill_all(&mut self) {
        for slot in self.panes.values() {
            let _ = slot.pty.kill();
        }
        self.panes.clear();
    }

    /// The name a tab presents: one still carrying its default numeric name
    /// borrows its active pane's title (tmux-style automatic naming), so the
    /// app-bar chip says what the tab holds instead of repeating its position.
    fn tab_display_name(&self, tab: &TabEntry) -> String {
        if tab.name != tab.id.to_string() {
            return tab.name.clone();
        }
        tab.active_pane
            .or_else(|| tab.layout.as_ref().map(|l| l.panes()[0]))
            .and_then(|id| self.panes.get(&id))
            .map(|slot| slot.meta.title.clone())
            .unwrap_or_else(|| tab.name.clone())
    }

    fn new_tab_entry(&mut self) -> TabEntry {
        let id = self.ids.tab();
        TabEntry {
            id,
            name: id.to_string(),
            layout: None,
            active_pane: None,
        }
    }

    fn resolve_tab(&self, tab: Option<TabId>) -> Result<TabId> {
        let target = match tab {
            Some(tab) => tab,
            None => self
                .current_tab
                .context("no current tab; create a workspace first")?,
        };
        if self.workspace_of_tab(target).is_none() {
            bail!("no tab {target}");
        }
        Ok(target)
    }

    fn resolve_workspace(&self, workspace: Option<WorkspaceId>) -> Result<WorkspaceId> {
        match workspace {
            Some(ws) => Ok(ws),
            None => self
                .current_tab
                .and_then(|t| self.workspace_of_tab(t))
                .context("no current workspace; pass --workspace"),
        }
    }

    fn tab_workspace_dir(&self, tab: TabId) -> Result<PathBuf> {
        self.workspaces
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == tab))
            .map(|w| w.dir.clone())
            .with_context(|| format!("no tab {tab}"))
    }

    fn tab_mut(&mut self, tab: TabId) -> Option<&mut TabEntry> {
        self.workspaces
            .iter_mut()
            .flat_map(|w| w.tabs.iter_mut())
            .find(|t| t.id == tab)
    }

    fn spawn_into_tab(
        &mut self,
        tab: TabId,
        mut spec: PtySpec,
        title: String,
        split: Option<(PaneId, Direction)>,
        ephemeral: bool,
    ) -> Result<PaneId> {
        // Allocate the id before spawning so it can be exported to the child:
        // a Claude Code hook reads `TUTTI_PANE`/`TUTTI_SESSION` to address this
        // exact pane on this daemon's socket. Every spawn path funnels here, so
        // this is the one place the pane env is stamped.
        let id = self.ids.pane();
        spec.env.push(("TUTTI_PANE".into(), id.0.to_string()));
        spec.env.push(("TUTTI_SESSION".into(), self.name.clone()));
        let pty = Arc::new(PtyPane::spawn(spec, self.size).context("spawn pty")?);
        let entry = self.tab_mut(tab).context("target tab vanished")?;
        entry.layout = Some(match (&entry.layout, split) {
            (None, _) => Layout::Leaf(id),
            (Some(layout), Some((target, direction))) => layout.split(target, id, direction),
            (Some(layout), None) => {
                let anchor = entry.active_pane.unwrap_or_else(|| layout.panes()[0]);
                layout.split(anchor, id, Direction::Horizontal)
            }
        });
        entry.active_pane = Some(id);
        self.current_tab = Some(tab);
        self.panes.insert(
            id,
            PaneSlot {
                meta: PaneInfo {
                    id,
                    title,
                    agent: None,
                    state: AgentState::default(),
                    exited: None,
                    subagents: Vec::new(),
                },
                pty,
                tab,
                ephemeral,
                hook_seen: false,
            },
        );
        Ok(id)
    }
}

/// What `apply_agent_event` changed, so the server broadcasts the right events.
pub struct AgentEventOutcome {
    /// The pane's `(from, to)` state transition, when the event moved it.
    pub transition: Option<(AgentState, AgentState)>,
    /// Whether the subagent list changed (so a fresh view must be broadcast).
    pub subagents_changed: bool,
}

/// Mark a subagent stopped. Prefers the running entry whose id matches; failing
/// that (a Claude `SubagentStop` carries no id that lines up with the `Task`
/// tool_input) it finishes the oldest still-running subagent. Returns whether
/// anything changed.
fn stop_subagent(subs: &mut [SubagentInfo], id: &str) -> bool {
    if let Some(s) = subs.iter_mut().find(|s| s.running && s.id == id) {
        s.running = false;
        return true;
    }
    if let Some(s) = subs.iter_mut().find(|s| s.running) {
        s.running = false;
        return true;
    }
    false
}

fn pane_info(slot: &PaneSlot) -> PaneInfo {
    slot.meta.clone()
}

fn pane_title(program: &str) -> String {
    std::path::Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string())
}

/// The current git branch of `dir`, read straight from `.git/HEAD` — no
/// subprocess. Handles `.git` as a directory and as a worktree/submodule file
/// (`gitdir: <path>`). Returns `None` when `dir` is not a checkout or HEAD is
/// unreadable.
fn git_branch(dir: &Path) -> Option<String> {
    let git = dir.join(".git");
    let head = if git.is_dir() {
        git.join("HEAD")
    } else {
        let pointer = std::fs::read_to_string(&git).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        let gitdir = Path::new(target);
        let gitdir = if gitdir.is_absolute() {
            gitdir.to_path_buf()
        } else {
            dir.join(gitdir)
        };
        gitdir.join("HEAD")
    };
    parse_head(&std::fs::read_to_string(head).ok()?)
}

/// A branch name from `HEAD` contents: `ref: refs/heads/<name>` yields `<name>`;
/// a detached bare hash yields its first 8 chars; empty yields `None`.
fn parse_head(contents: &str) -> Option<String> {
    let line = contents.lines().next()?.trim();
    if let Some(reference) = line.strip_prefix("ref:") {
        let reference = reference.trim();
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        Some(name.to_string())
    } else if line.is_empty() {
        None
    } else {
        Some(line.chars().take(8).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{git_branch, parse_head, stop_subagent};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tutti_core::SubagentInfo;

    fn sub(id: &str, running: bool) -> SubagentInfo {
        SubagentInfo {
            id: id.into(),
            desc: id.into(),
            running,
        }
    }

    #[test]
    fn stop_subagent_prefers_a_matching_running_id() {
        let mut subs = vec![sub("a", true), sub("b", true)];
        assert!(stop_subagent(&mut subs, "b"));
        assert!(subs[0].running, "the unmatched entry keeps running");
        assert!(!subs[1].running, "the id match is finished");
    }

    #[test]
    fn stop_subagent_falls_back_to_the_oldest_running() {
        // No id matches (the real SubagentStop id never lines up), so the oldest
        // still-running subagent is finished.
        let mut subs = vec![sub("a", false), sub("b", true), sub("c", true)];
        assert!(stop_subagent(&mut subs, "no-such-id"));
        assert!(!subs[1].running, "the oldest running entry is finished");
        assert!(subs[2].running, "later running entries are untouched");
    }

    #[test]
    fn stop_subagent_with_nothing_running_is_a_noop() {
        let mut subs = vec![sub("a", false)];
        assert!(!stop_subagent(&mut subs, "a"));
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique empty temp directory, cleaned up on drop. Avoids a dev-dep on a
    /// tempdir crate (the workspace forbids new deps).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("tutti-branch-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_head_reads_ref_form() {
        assert_eq!(
            parse_head("ref: refs/heads/main\n").as_deref(),
            Some("main")
        );
        assert_eq!(
            parse_head("ref: refs/heads/feature/x\n").as_deref(),
            Some("feature/x")
        );
    }

    #[test]
    fn parse_head_reads_detached_hash_as_prefix() {
        assert_eq!(
            parse_head("0123456789abcdef0123456789abcdef01234567\n").as_deref(),
            Some("01234567")
        );
    }

    #[test]
    fn parse_head_empty_is_none() {
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("\n"), None);
    }

    #[test]
    fn git_branch_reads_ref_from_git_dir() {
        let tmp = TempDir::new();
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/trunk\n").unwrap();
        assert_eq!(git_branch(tmp.path()).as_deref(), Some("trunk"));
    }

    #[test]
    fn git_branch_follows_gitdir_file_indirection() {
        let tmp = TempDir::new();
        // A worktree/submodule `.git` is a file pointing at the real git dir.
        let real = tmp.path().join("real-git");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: real-git\n").unwrap();
        assert_eq!(git_branch(tmp.path()).as_deref(), Some("wt"));
    }

    #[test]
    fn git_branch_missing_head_is_none() {
        let tmp = TempDir::new();
        assert_eq!(git_branch(tmp.path()), None);
    }
}
