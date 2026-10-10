//! Server-side session state: the workspace/tab/pane tree plus one persistent
//! `PtyPane` per pane. Pure model — it owns no sockets and speaks no protocol,
//! it just answers the operations the dispatcher maps requests onto and reports
//! which panes it touched so the caller can drive the pty lifecycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tutti_core::{
    AgentHookEvent, AgentKind, AgentState, Direction, FileStat, Layout, Observation, PaneId,
    PaneInfo, StateEvent, StateSource, SubagentInfo, TabId, TabInfo, TabView, WorkspaceId,
    WorkspaceInfo, WorkspaceView, prompt_fingerprint,
};

use crate::checkpoints::{Noted, Turns};
use crate::pty::{PaneSize, PtyPane, PtySpec, StatusReport};

/// The most subagent rows kept per pane before the oldest is dropped, bounding
/// the list even for an agent that spawns many short-lived subagents.
const SUBAGENT_CAP: usize = 16;
/// How long a `Working` pane not held by a hook may go without PTY output or a
/// classifier `Working` match before it reads as `Unknown`: a build killed
/// mid-report never says clear, and a wedged agent never says done.
pub const STALL_AFTER: Duration = Duration::from_secs(30);
/// The least gap between two OSC 7501 `msg` notifications from one pane (the
/// program-status spec's rate limit).
const REPORT_NOTIFY_EVERY: Duration = Duration::from_secs(30);

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
    /// The last-computed jj change stat against trunk (`4 files +120 −33`),
    /// refreshed off the hot path. `None` until probed, when not a jj repo, or
    /// when clean against trunk.
    changes: Option<String>,
    /// The per-file breakdown behind `changes`, refreshed in the same probe so
    /// the two never disagree. Empty whenever `changes` is `None`.
    changed_files: Vec<FileStat>,
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
    /// The pty output generation the last guarded answer was written at. Until
    /// the pane draws past it, the agent has not visibly taken that answer, and
    /// its prompt — fingerprint unchanged — must not be answered twice.
    answered: Option<u64>,
    /// The pane's per-turn checkpoints, overlaid onto its `PaneInfo` in views.
    turns: Turns,
    /// The label a pane with no detected agent shows under in agent surfaces
    /// once it reports its own status (OSC 7501 `app`, else its title); dropped
    /// when that report comes to rest (`settle`).
    label: Option<String>,
    /// The last sign of life — PTY output, a classifier `Working` match, a
    /// status report — that the stall timer measures from.
    alive: Instant,
    /// When an OSC 7501 `msg` last raised a notification here.
    notified: Option<Instant>,
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
            changed_files: Vec::new(),
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

    /// Store a freshly-computed change summary (stat plus per-file breakdown)
    /// for workspace `id`. Returns whether either moved (so the caller only
    /// rebroadcasts the view when something changed, and a vanished workspace is
    /// a silent no-op).
    pub fn set_changes(
        &mut self,
        id: WorkspaceId,
        changes: Option<String>,
        files: Vec<FileStat>,
    ) -> bool {
        match self.workspaces.iter_mut().find(|w| w.id == id) {
            Some(w) if w.changes != changes || w.changed_files != files => {
                w.changes = changes;
                w.changed_files = files;
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
    /// down entirely when its child exits. With a `sandbox` profile the pane
    /// runs `cmd` through `tutti sandbox run <profile> --`, titled for `cmd`.
    pub fn pane_run(
        &mut self,
        tab: Option<TabId>,
        cmd: Vec<String>,
        ephemeral: bool,
        sandbox: Option<String>,
    ) -> Result<PaneId> {
        let tab = self.resolve_tab(tab)?;
        let title = pane_title(cmd.first().context("empty command")?);
        let argv = match &sandbox {
            Some(profile) => sandbox_argv(&client_exe()?, profile, cmd),
            None => cmd,
        };
        let (program, args) = argv.split_first().context("empty command")?;
        let dir = self.tab_workspace_dir(tab)?;
        let mut spec = PtySpec::new(program);
        spec.args = args.to_vec();
        spec.cwd = Some(dir);
        spec.env = vec![("TERM".into(), "xterm-256color".into())];
        let id = self.spawn_into_tab(tab, spec, title, None, ephemeral)?;
        if let Some(slot) = self.panes.get_mut(&id) {
            slot.meta.sandbox = sandbox;
        }
        Ok(id)
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

    /// `tutti run` under one guard: resolve the workspace — `workspace`, else
    /// the one with `dir` open (compared canonically), else a fresh mount of
    /// `dir`, which must exist, else the current one — and spawn `cmd`, titled
    /// `title` when given, in a fresh tab there: a new tab, or a fresh mount's
    /// own empty one. The
    /// session's current tab stays where it was (an empty session's aside), so
    /// an attached client's launches still land in the tab it is looking at.
    /// A failed spawn leaves nothing behind. Returns the workspace, tab and
    /// pane, and whether the workspace was mounted.
    pub fn run_in_fresh_tab(
        &mut self,
        workspace: Option<WorkspaceId>,
        dir: Option<PathBuf>,
        cmd: Vec<String>,
        sandbox: Option<String>,
        title: Option<String>,
    ) -> Result<(WorkspaceId, TabId, PaneId, bool)> {
        let current = self.current_tab;
        let (workspace, tab, mounted) = match (workspace, dir) {
            (Some(_), Some(_)) => bail!("run takes a workspace or a directory, not both"),
            (None, Some(dir)) => {
                let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                let wanted = canon(&dir);
                let open = self.workspaces.iter().find(|w| canon(&w.dir) == wanted);
                match open.map(|w| w.id) {
                    Some(id) => (id, self.tab_new(Some(id))?, false),
                    None if !dir.is_dir() => bail!("directory does not exist: {}", dir.display()),
                    None => {
                        let (id, tab) = self.push_workspace(dir, None);
                        (id, tab, true)
                    }
                }
            }
            (workspace, None) => {
                let id = self.resolve_workspace(workspace)?;
                (id, self.tab_new(Some(id))?, false)
            }
        };
        let spawned = self.pane_run(Some(tab), cmd, false, sandbox);
        match &spawned {
            Ok(pane) => {
                self.current_tab = current.or(Some(tab));
                if let Some(title) = title
                    && let Some(slot) = self.panes.get_mut(pane)
                {
                    slot.meta.title = title;
                }
            }
            Err(_) => {
                self.current_tab = current;
                for w in &mut self.workspaces {
                    w.tabs.retain(|t| t.id != tab);
                }
                self.workspaces.retain(|w| !(mounted && w.id == workspace));
            }
        }
        Ok((workspace, tab, spawned?, mounted))
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
                changed_files: w.changed_files.clone(),
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

    /// A guarded send: answer the prompt a blocked agent is showing, only if it
    /// is still the prompt `expect` fingerprinted. Refused — nothing written —
    /// when the pane is not `Blocked`, when it has not drawn anything since the
    /// last guarded answer (that one is still in flight, so this would answer
    /// the same prompt twice), or when its prompt fingerprint moved. The
    /// in-flight mark is the generation of exactly the grid that was checked.
    pub fn pane_answer(&mut self, pane: PaneId, bytes: &[u8], expect: &str) -> Result<()> {
        let slot = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("no pane {pane}"))?;
        if slot.meta.state != AgentState::Blocked {
            let state = format!("{:?}", slot.meta.state).to_lowercase();
            bail!(
                "pane {pane} is not blocked ({state}): a guarded send only answers a blocked agent"
            );
        }
        let grid = slot.pty.snapshot();
        if slot.answered == Some(grid.generation) {
            bail!("pane {pane} was already answered: waiting for it to redraw");
        }
        if prompt_fingerprint(&grid.lines) != expect {
            bail!("the prompt changed since you looked: read pane {pane} again");
        }
        slot.pty.write_input(bytes)?;
        slot.answered = Some(grid.generation);
        Ok(())
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

    /// The live panes whose agent detection finds by walking their local
    /// process tree: all but sandbox panes, whose agent runs in a pod — their
    /// tree only ever holds `tutti` and `kubectl` — and was declared at spawn.
    pub fn detectable_panes(&self) -> Vec<(PaneId, Arc<PtyPane>)> {
        self.panes
            .iter()
            .filter(|(_, s)| s.meta.exited.is_none() && s.meta.sandbox.is_none())
            .map(|(id, s)| (*id, Arc::clone(&s.pty)))
            .collect()
    }

    /// Live panes the classify pass tends, with their detected kind and pty:
    /// detected agents (classified from their screen) plus any pane holding a
    /// program/progress claim (for the stall timer). A hook-driven pane is
    /// excluded: its state comes from exact hook signals, which the screen
    /// heuristics would only fight, and hooks never stall. Agent detection is
    /// unaffected (it walks `live_panes`), so a hook-driven pane keeps its badge.
    pub fn agent_panes(&self) -> Vec<(PaneId, Option<AgentKind>, Arc<PtyPane>)> {
        self.panes
            .iter()
            .filter(|(_, s)| {
                s.meta.exited.is_none()
                    && s.meta.source != Some(StateSource::Hook)
                    && (s.meta.agent.is_some() || s.meta.source.is_some_and(StateSource::claims))
            })
            .map(|(id, s)| (*id, s.meta.agent.clone(), Arc::clone(&s.pty)))
            .collect()
    }

    /// Apply an agent hook event to `pane`, which claims it for good (hooks
    /// outrank every other source) — all but `SessionStarted`, which changes no
    /// state and so leaves the screen classifier on until the agent's first
    /// turn. A `conversation` replaces the pane's (the agent switched to
    /// another); `None` keeps it. A prompt is noted for the checkpoint closing
    /// the turn it starts. Returns what moved so the caller broadcasts the right
    /// event(s): a state transition (via the shared `AgentState::apply`) drives
    /// `StateChanged`; a subagent-list or conversation change drives
    /// `LayoutChanged`. `None` for an unknown pane (a hook must never fail
    /// because its pane has gone away).
    pub fn apply_agent_event(
        &mut self,
        pane: PaneId,
        event: AgentHookEvent,
        conversation: Option<String>,
    ) -> Option<AgentEventOutcome> {
        let slot = self.panes.get_mut(&pane)?;
        if event != AgentHookEvent::SessionStarted {
            slot.meta.source = Some(StateSource::Hook);
        }
        let mut relisted = conversation.is_some() && slot.meta.conversation != conversation;
        if relisted {
            slot.meta.conversation = conversation;
        }
        let subs = &mut slot.meta.subagents;
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
                relisted = true;
            }
            AgentHookEvent::SubagentStopped { id } => {
                relisted |= stop_subagent(subs, &id);
            }
            AgentHookEvent::Activity { .. } => {
                classified = Some(Observation::Working);
            }
            AgentHookEvent::Prompt { text } => {
                slot.turns.set_prompt(text);
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
                relisted |= subs.len() != before;
            }
            AgentHookEvent::SessionStarted => {}
        }
        let transition = classified.and_then(|obs| {
            let from = slot.meta.state;
            let to = from.apply(StateEvent::Classified(obs));
            slot.meta.state = to;
            (from != to).then_some((from, to))
        });
        Some(AgentEventOutcome {
            transition,
            relisted,
        })
    }

    /// Record the agent kind detected for a pane. Returns whether it changed. A
    /// change ends any program/progress claim and the self-reported label —
    /// they spoke for whatever ran there before; a hook claim stays (sticky).
    /// An agent leaving takes its conversation and its turn's prompt along;
    /// one appearing keeps what its hooks reported before detection caught up
    /// with it. An agent appearing notes its start as a checkpoint for the
    /// caller's next change probe — detection lands well before the agent's
    /// first edit.
    pub fn set_agent(&mut self, pane: PaneId, agent: Option<AgentKind>) -> bool {
        match self.panes.get_mut(&pane) {
            Some(slot) if slot.meta.agent != agent => {
                if agent.is_some() && slot.meta.sandbox.is_none() {
                    slot.turns.note_start();
                }
                if slot.meta.agent.is_some() {
                    slot.meta.conversation = None;
                    slot.turns.set_prompt(None);
                }
                slot.meta.agent = agent;
                slot.label = None;
                if slot.meta.source != Some(StateSource::Hook) {
                    slot.meta.source = None;
                }
                true
            }
            _ => false,
        }
    }

    /// Note a pane's state transition on its checkpoint list (see
    /// `Turns::note_transition`); its workspace's next change probe records it.
    /// A sandbox pane notes nothing: its agent edits the pod's checkout, and a
    /// checkpoint of the local one would offer to "undo" someone else's work.
    pub fn note_transition(&mut self, pane: PaneId, from: AgentState, to: AgentState) {
        if let Some(slot) = self.panes.get_mut(&pane)
            && slot.meta.sandbox.is_none()
        {
            slot.turns.note_transition(from, to);
        }
    }

    /// Drain what every pane of `workspace` noted since the last probe: the
    /// checkpoints (label and prompt) the probe about to snapshot must record,
    /// per pane.
    pub fn take_turns(&mut self, workspace: WorkspaceId) -> Vec<(PaneId, Vec<Noted>)> {
        let tabs: Vec<TabId> = self
            .workspaces
            .iter()
            .filter(|w| w.id == workspace)
            .flat_map(|w| w.tabs.iter().map(|t| t.id))
            .collect();
        self.panes
            .iter_mut()
            .filter(|(_, slot)| tabs.contains(&slot.tab))
            .map(|(id, slot)| (*id, slot.turns.take_pending()))
            .filter(|(_, labels)| !labels.is_empty())
            .collect()
    }

    /// A pane's checkpoint list, `None` for an unknown pane.
    pub fn turns(&mut self, pane: PaneId) -> Option<&mut Turns> {
        self.panes.get_mut(&pane).map(|slot| &mut slot.turns)
    }

    /// Apply a program's own status report (OSC 9;4 / 7501) to `pane` under the
    /// source ranking: refused while a higher source's claim is live, else it
    /// sets the state and claims the pane for its source. A clear ends the
    /// claim only for the source holding it; what that claim held in motion
    /// (Working/Blocked) settles to Idle, an outcome (Done) stays until seen.
    /// An accepted report labels a pane with no detected agent (its `app`, else
    /// the title) so it shows where agent state does, and a `msg` on a move to
    /// Done/Blocked becomes a notification at most every 30s.
    pub fn apply_report(
        &mut self,
        pane: PaneId,
        report: StatusReport,
        now: Instant,
    ) -> ReportOutcome {
        let Some(slot) = self.panes.get_mut(&pane) else {
            return ReportOutcome::default();
        };
        let (from, held) = (slot.meta.state, slot.meta.source);
        let to = match report.state {
            Some(state) => state,
            None if held == Some(report.source) => match from {
                AgentState::Working | AgentState::Blocked => AgentState::Idle,
                other => other,
            },
            None => return ReportOutcome::default(),
        };
        if !report.source.may_move(held, from, to) {
            return ReportOutcome::default();
        }
        slot.alive = now;
        slot.meta.state = to;
        slot.meta.source = report.state.map(|_| report.source);
        let mut relisted = false;
        if report.state.is_some() {
            let label = report.app.clone().or_else(|| Some(slot.meta.title.clone()));
            relisted = slot.meta.agent.is_none() && slot.label != label;
            slot.label = label;
        }
        let notify = report
            .msg
            .filter(|_| from != to && matches!(to, AgentState::Done | AgentState::Blocked))
            .filter(|_| {
                slot.notified
                    .is_none_or(|at| now.duration_since(at) >= REPORT_NOTIFY_EVERY)
            })
            .map(|msg| {
                slot.notified = Some(now);
                (report.app, msg)
            });
        ReportOutcome {
            transition: (from != to).then_some((from, to)),
            relisted,
            notify,
        }
    }

    /// Fold one classify pass onto `pane`: PTY output since the last pass is an
    /// Activity, a screen match a Screen observation (applied second, so it
    /// wins), each landing only where the live claim lets it
    /// (`StateSource::may_move`). Output or a `Working` match keeps the pane
    /// alive; a `Working` pane not held by a hook and silent for `STALL_AFTER`
    /// drops to `Unknown`, ending its claim. Returns the net transition.
    pub fn observe(
        &mut self,
        pane: PaneId,
        advanced: bool,
        classified: Option<Observation>,
        now: Instant,
    ) -> Option<(AgentState, AgentState)> {
        let slot = self.panes.get_mut(&pane)?;
        if advanced || classified == Some(Observation::Working) {
            slot.alive = now;
        }
        let from = slot.meta.state;
        let signals = [
            (
                StateSource::Activity,
                advanced.then_some(StateEvent::Activity),
            ),
            (StateSource::Screen, classified.map(StateEvent::Classified)),
        ];
        for (source, event) in signals {
            let Some(event) = event else { continue };
            let (current, next) = (slot.meta.state, slot.meta.state.apply(event));
            if next != current && source.may_move(slot.meta.source, current, next) {
                slot.meta.state = next;
                slot.meta.source = Some(source);
            }
        }
        if slot.meta.state == AgentState::Working
            && slot.meta.source != Some(StateSource::Hook)
            && now.duration_since(slot.alive) >= STALL_AFTER
        {
            slot.meta.state = AgentState::Unknown;
            slot.meta.source = None;
        }
        let to = slot.meta.state;
        (from != to).then_some((from, to))
    }

    /// Drop a self-reported pane's label once its report has nothing left to
    /// say — no live claim, and Idle or Unknown — so it leaves the agent
    /// surfaces until it reports again. Returns whether the pane's listing
    /// changed: the caller rebroadcasts the view, since a client only patches
    /// state from `StateChanged`.
    pub fn settle(&mut self, pane: PaneId) -> bool {
        let Some(slot) = self.panes.get_mut(&pane) else {
            return false;
        };
        let resting = !slot.meta.source.is_some_and(StateSource::claims)
            && matches!(slot.meta.state, AgentState::Idle | AgentState::Unknown);
        if !resting || slot.label.is_none() {
            return false;
        }
        slot.label = None;
        slot.meta.agent.is_none()
    }

    pub fn pane_state(&self, pane: PaneId) -> Option<AgentState> {
        self.panes.get(&pane).map(|s| s.meta.state)
    }

    /// One pane's wire metadata, as `pane_list` shows it; `None` once gone.
    pub fn pane(&self, pane: PaneId) -> Option<PaneInfo> {
        self.panes.get(&pane).map(pane_info)
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
    /// classifier would, and it ends whatever claim the pane held. Returns the
    /// `(from, to)` state transition, or `None` when the pane is already gone
    /// or already marked exited.
    pub fn mark_exited(&mut self, pane: PaneId, code: i32) -> Option<(AgentState, AgentState)> {
        match self.panes.get_mut(&pane) {
            Some(slot) if slot.meta.exited.is_none() => {
                slot.meta.exited = Some(code);
                let from = slot.meta.state;
                let to = from.apply(StateEvent::Classified(Observation::Done));
                slot.meta.state = to;
                slot.meta.source = None;
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
                    checkpoints: Vec::new(),
                    source: None,
                    sandbox: None,
                    conversation: None,
                },
                pty,
                tab,
                ephemeral,
                answered: None,
                turns: Turns::default(),
                label: None,
                alive: Instant::now(),
                notified: None,
            },
        );
        Ok(id)
    }
}

/// What `apply_agent_event` changed, so the server broadcasts the right events.
pub struct AgentEventOutcome {
    /// The pane's `(from, to)` state transition, when the event moved it.
    pub transition: Option<(AgentState, AgentState)>,
    /// Whether the subagent list or the conversation changed (so a fresh view
    /// must be broadcast).
    pub relisted: bool,
}

/// What `apply_report` changed, so the server broadcasts the right events.
#[derive(Debug, Default, PartialEq)]
pub struct ReportOutcome {
    /// The pane's `(from, to)` state transition, when the report moved it.
    pub transition: Option<(AgentState, AgentState)>,
    /// Whether a pane with no detected agent gained or changed its label (so a
    /// fresh view must be broadcast).
    pub relisted: bool,
    /// A `(title, body)` notification from the report's `msg`.
    pub notify: Option<(Option<String>, String)>,
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

/// A pane's wire metadata. One with no detected agent that is self-reporting
/// presents its label as the agent, which is all every agent surface keys on.
fn pane_info(slot: &PaneSlot) -> PaneInfo {
    let mut info = PaneInfo {
        checkpoints: slot.turns.entries().to_vec(),
        ..slot.meta.clone()
    };
    if info.agent.is_none() {
        info.agent = slot.label.clone().map(AgentKind);
    }
    info
}

fn pane_title(program: &str) -> String {
    std::path::Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string())
}

/// The `tutti` client binary a sandbox pane runs: the daemon's sibling (the
/// client starts the daemon from its own directory), else `tutti` on PATH.
fn client_exe() -> Result<String> {
    let sibling = std::env::current_exe()
        .context("locating tutti-server")?
        .with_file_name("tutti");
    Ok(if sibling.is_file() {
        sibling.to_string_lossy().into_owned()
    } else {
        "tutti".into()
    })
}

/// The argv running `cmd` in sandbox `profile`: `<exe> sandbox run <profile>
/// -- <cmd…>`.
fn sandbox_argv(exe: &str, profile: &str, cmd: Vec<String>) -> Vec<String> {
    [exe, "sandbox", "run", profile, "--"]
        .into_iter()
        .map(String::from)
        .chain(cmd)
        .collect()
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
    use super::{STALL_AFTER, Session, git_branch, parse_head, sandbox_argv, stop_subagent};
    use crate::pty::{PaneSize, StatusReport};
    use AgentState::{Blocked, Done, Idle, Unknown, Working};
    use StateSource::{Hook, Program, Progress, Screen};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use tutti_core::{
        AgentHookEvent, AgentState, Observation, PaneId, PaneInfo, StateSource, SubagentInfo,
    };

    /// A session holding one live `sleep` pane (titled `sleep`). The temp dir is
    /// returned so it outlives the pane's cwd.
    fn session_with_pane() -> (Session, PaneId, TempDir) {
        let tmp = TempDir::new();
        let mut session = Session::new(PaneSize::new(24, 80), "test");
        session.workspace_new(tmp.path().to_path_buf());
        let pane = session
            .pane_run(None, vec!["/bin/sleep".into(), "30".into()], false, None)
            .unwrap();
        (session, pane, tmp)
    }

    fn report(source: StateSource, state: Option<AgentState>) -> StatusReport {
        StatusReport {
            source,
            state,
            app: None,
            msg: None,
        }
    }

    fn app_report(state: Option<AgentState>, app: &str, msg: Option<&str>) -> StatusReport {
        StatusReport {
            source: Program,
            state,
            app: Some(app.into()),
            msg: msg.map(Into::into),
        }
    }

    fn info(session: &Session, pane: PaneId) -> PaneInfo {
        session
            .pane_list()
            .into_iter()
            .find(|p| p.id == pane)
            .unwrap()
    }

    fn state_of(session: &Session, pane: PaneId) -> (AgentState, Option<StateSource>) {
        let info = info(session, pane);
        (info.state, info.source)
    }

    #[test]
    fn a_program_claim_outranks_progress_and_activity_until_cleared() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_report(pane, report(Program, Some(Working)), t0);
        assert_eq!(state_of(&s, pane), (Working, Some(Program)));

        // Lower sources cannot move it while the claim is live.
        s.apply_report(pane, report(Progress, Some(Done)), t0);
        s.apply_report(pane, report(Progress, None), t0);
        assert_eq!(state_of(&s, pane), (Working, Some(Program)));
        s.apply_report(pane, report(Program, Some(Done)), t0);
        assert_eq!(s.observe(pane, true, None, t0), None, "activity is refused");
        assert_eq!(state_of(&s, pane), (Done, Some(Program)));

        // A clear ends the claim; an outcome (Done) stays until it is seen.
        s.apply_report(pane, report(Program, None), t0);
        assert_eq!(state_of(&s, pane), (Done, None));

        // Progress now speaks, and its own clear settles Working to Idle.
        s.apply_report(pane, report(Progress, Some(Working)), t0);
        assert_eq!(state_of(&s, pane), (Working, Some(Progress)));
        s.apply_report(pane, report(Progress, None), t0);
        assert_eq!(state_of(&s, pane), (Idle, None));

        // A clear from a source holding nothing is a no-op.
        s.apply_report(pane, report(Program, Some(Blocked)), t0);
        s.apply_report(pane, report(Progress, None), t0);
        assert_eq!(state_of(&s, pane), (Blocked, Some(Program)));
        s.apply_report(pane, report(Program, None), t0);
        assert_eq!(
            state_of(&s, pane),
            (Idle, None),
            "a cleared Blocked settles"
        );
    }

    #[test]
    fn a_hook_claim_is_absolute_and_sticky() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_agent_event(pane, AgentHookEvent::Blocked { message: None }, None);
        assert_eq!(state_of(&s, pane), (Blocked, Some(Hook)));
        s.apply_report(pane, report(Program, Some(Done)), t0);
        s.apply_report(pane, report(Progress, Some(Working)), t0);
        s.observe(pane, true, Some(Observation::Working), t0);
        assert_eq!(state_of(&s, pane), (Blocked, Some(Hook)));
        s.set_agent(pane, Some("claude".into()));
        assert_eq!(
            state_of(&s, pane),
            (Blocked, Some(Hook)),
            "survives set_agent"
        );
        assert!(
            s.agent_panes().iter().all(|(p, _, _)| *p != pane),
            "a hook pane is never classified"
        );
    }

    #[test]
    fn a_screen_blocked_beats_a_stale_progress_working_and_nothing_else() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_report(pane, report(Progress, Some(Working)), t0);
        assert_eq!(s.observe(pane, true, Some(Observation::Done), t0), None);
        assert_eq!(
            s.observe(pane, false, Some(Observation::Blocked), t0),
            Some((Working, Blocked))
        );
        assert_eq!(
            state_of(&s, pane),
            (Blocked, Some(Screen)),
            "the claim ended"
        );
    }

    #[test]
    fn claims_end_when_the_agent_changes_or_the_child_exits() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_report(pane, app_report(Some(Working), "build", None), t0);
        assert!(s.set_agent(pane, Some("claude".into())));
        assert_eq!(state_of(&s, pane), (Working, None));
        assert_eq!(info(&s, pane).agent, Some("claude".into()));
        s.set_agent(pane, None);
        assert_eq!(info(&s, pane).agent, None, "the label went with the claim");

        s.apply_report(pane, report(Program, Some(Working)), t0);
        assert_eq!(s.mark_exited(pane, 0), Some((Working, Done)));
        assert_eq!(state_of(&s, pane), (Done, None));
    }

    #[test]
    fn a_quiet_working_pane_stalls_to_unknown() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_report(pane, report(Program, Some(Working)), t0);
        assert_eq!(s.observe(pane, false, None, t0 + STALL_AFTER / 2), None);
        // Output keeps it alive even though the claim refuses the Activity...
        assert_eq!(s.observe(pane, true, None, t0 + STALL_AFTER / 2), None);
        assert_eq!(s.observe(pane, false, None, t0 + STALL_AFTER), None);
        // ...and so does a classifier Working match...
        let t1 = t0 + STALL_AFTER;
        assert_eq!(s.observe(pane, false, Some(Observation::Working), t1), None);
        // ...until a full stall interval goes by with neither.
        assert_eq!(
            s.observe(pane, false, None, t1 + STALL_AFTER),
            Some((Working, Unknown))
        );
        assert_eq!(
            state_of(&s, pane),
            (Unknown, None),
            "the stall ends the claim"
        );
        assert!(
            s.settle(pane),
            "a stalled self-reported pane leaves the surfaces"
        );
    }

    #[test]
    fn a_hook_working_pane_never_stalls() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_agent_event(pane, AgentHookEvent::Activity { detail: None }, None);
        assert_eq!(s.observe(pane, false, None, t0 + STALL_AFTER * 10), None);
        assert_eq!(state_of(&s, pane), (Working, Some(Hook)));
    }

    /// Apply a hook event naming no conversation and note its transition, as
    /// the server's `publish_transition` does.
    fn hook(s: &mut Session, pane: PaneId, event: AgentHookEvent) {
        let outcome = s.apply_agent_event(pane, event, None).unwrap();
        if let Some((from, to)) = outcome.transition {
            s.note_transition(pane, from, to);
        }
    }

    #[test]
    fn hooks_name_the_conversation_and_a_later_one_replaces_it() {
        let (mut s, pane, _tmp) = session_with_pane();
        let conversation = |s: &Session| info(s, pane).conversation;
        let started = s
            .apply_agent_event(pane, AgentHookEvent::SessionStarted, Some("c1".into()))
            .unwrap();
        assert!(started.relisted, "a new conversation rebroadcasts the view");
        assert_eq!(started.transition, None);
        assert_eq!(conversation(&s).as_deref(), Some("c1"));
        assert_eq!(
            state_of(&s, pane),
            (Unknown, None),
            "a session start claims nothing: the screen speaks until the first turn"
        );

        let working = AgentHookEvent::Activity { detail: None };
        let again = s.apply_agent_event(pane, working, Some("c1".into()));
        assert!(!again.unwrap().relisted, "the same conversation is no news");
        s.apply_agent_event(pane, AgentHookEvent::Done, None);
        assert_eq!(
            conversation(&s).as_deref(),
            Some("c1"),
            "an event naming none keeps it"
        );
        s.apply_agent_event(pane, AgentHookEvent::SessionStarted, Some("c2".into()));
        assert_eq!(
            conversation(&s).as_deref(),
            Some("c2"),
            "a `/clear` moves it on"
        );
    }

    #[test]
    fn the_conversation_leaves_with_its_agent() {
        let (mut s, pane, _tmp) = session_with_pane();
        let conversation = |s: &Session| info(s, pane).conversation;
        s.apply_agent_event(pane, AgentHookEvent::SessionStarted, Some("c1".into()));
        assert!(s.set_agent(pane, Some("claude".into())));
        assert_eq!(
            conversation(&s).as_deref(),
            Some("c1"),
            "the hook beat detection to its own agent"
        );
        s.set_agent(pane, Some("codex".into()));
        assert_eq!(conversation(&s), None, "it was the previous agent's");
        s.apply_agent_event(pane, AgentHookEvent::SessionStarted, Some("c2".into()));
        s.set_agent(pane, None);
        assert_eq!(
            conversation(&s),
            None,
            "nor does it outlive its agent quitting"
        );
    }

    #[test]
    fn a_prompt_starts_a_turn_and_labels_the_checkpoint_closing_it() {
        let (mut s, pane, _tmp) = session_with_pane();
        let workspace = s.workspace_of_pane(pane).unwrap();
        let fix = Some("fix the sidebar".to_string());
        hook(&mut s, pane, AgentHookEvent::Prompt { text: fix.clone() });
        assert_eq!(state_of(&s, pane), (Working, Some(Hook)));
        hook(&mut s, pane, AgentHookEvent::Done);
        assert_eq!(
            s.take_turns(workspace),
            vec![(
                pane,
                vec![("start".to_string(), None), ("done".to_string(), fix)]
            )],
            "the baseline predates the prompt; the turn's end carries it"
        );
    }

    #[test]
    fn an_agent_quitting_mid_turn_takes_its_prompt_along() {
        let (mut s, pane, _tmp) = session_with_pane();
        let workspace = s.workspace_of_pane(pane).unwrap();
        s.set_agent(pane, Some("claude".into()));
        let fix = Some("fix the sidebar".to_string());
        hook(&mut s, pane, AgentHookEvent::Prompt { text: fix });
        s.set_agent(pane, None);
        hook(&mut s, pane, AgentHookEvent::Done);
        let noted = s.take_turns(workspace).remove(0).1;
        assert_eq!(
            noted.last(),
            Some(&("done".to_string(), None)),
            "whatever ended the turn later did not answer that prompt"
        );
    }

    #[test]
    fn a_self_reported_pane_lists_until_its_report_settles() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        assert_eq!(info(&s, pane).agent, None);
        assert!(
            s.apply_report(pane, app_report(Some(Working), "build", None), t0)
                .relisted
        );
        assert_eq!(info(&s, pane).agent, Some("build".into()));
        assert!(
            !s.apply_report(pane, app_report(Some(Done), "build", None), t0)
                .relisted
        );
        s.apply_report(pane, report(Program, None), t0);
        assert!(!s.settle(pane), "an unseen Done keeps it listed");
        assert_eq!(info(&s, pane).agent, Some("build".into()));
        s.set_pane_state(pane, Idle); // focused
        assert!(s.settle(pane));
        assert_eq!(info(&s, pane).agent, None);

        // Progress carries no app: the pane lists under its title.
        assert!(
            s.apply_report(pane, report(Progress, Some(Working)), t0)
                .relisted
        );
        assert_eq!(info(&s, pane).agent, Some("sleep".into()));
        s.apply_report(pane, report(Progress, None), t0);
        assert!(s.settle(pane));
    }

    #[test]
    fn a_report_msg_notifies_on_done_or_blocked_at_most_every_30s() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        let notify = |s: &mut Session, state, at| {
            s.apply_report(pane, app_report(Some(state), "build", Some("note")), at)
                .notify
        };
        assert_eq!(notify(&mut s, Working, t0), None, "working is no news");
        let note = Some((Some("build".to_string()), "note".to_string()));
        assert_eq!(notify(&mut s, Done, t0), note);
        assert_eq!(notify(&mut s, Done, t0), None, "no transition, no note");
        notify(&mut s, Working, t0 + Duration::from_secs(1));
        assert_eq!(notify(&mut s, Blocked, t0 + Duration::from_secs(2)), None);
        notify(&mut s, Working, t0 + Duration::from_secs(30));
        assert_eq!(notify(&mut s, Blocked, t0 + Duration::from_secs(30)), note);
    }

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

    #[test]
    fn a_sandbox_pane_runs_its_command_through_the_wrapper() {
        assert_eq!(
            sandbox_argv("/bin/tutti", "gke", vec!["claude".into(), "-c".into()]),
            ["/bin/tutti", "sandbox", "run", "gke", "--", "claude", "-c"]
        );
    }

    #[test]
    fn detection_skips_sandbox_panes_but_the_tick_still_drains_them() {
        let (mut session, pane, _tmp) = session_with_pane();
        session.panes.get_mut(&pane).unwrap().meta.sandbox = Some("gke".into());
        assert!(
            session.detectable_panes().is_empty(),
            "a sandbox pane's agent was declared; its local tree is only kubectl"
        );
        assert_eq!(
            session.live_panes().len(),
            1,
            "notifications and status reports still flow from its pty"
        );
        assert!(session.set_agent(pane, Some("claude".into())));
        assert_eq!(
            session.pane_list()[0].sandbox.as_deref(),
            Some("gke"),
            "the profile rides the pane's info"
        );
        session.note_transition(pane, Working, Done);
        let workspace = session.workspace_of_pane(pane).unwrap();
        assert!(
            session.take_turns(workspace).is_empty(),
            "no checkpoints: the agent edits the pod's checkout, not this one"
        );
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
