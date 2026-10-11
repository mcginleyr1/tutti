//! Server-side session state: the workspace/tab/pane tree plus one persistent
//! `PtyPane` per pane. Pure model — it owns no sockets and speaks no protocol,
//! it just answers the operations the dispatcher maps requests onto and reports
//! which panes it touched so the caller can drive the pty lifecycle.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tutti_core::{
    AgentHookEvent, AgentKind, AgentState, Direction, FileStat, IssueRef, Layout, Observation,
    PaneId, PaneInfo, PendingApproval, StateEvent, StateSource, SubagentInfo, TabId, TabInfo,
    TabView, Toward, WatchPane, WorkspaceId, WorkspaceInfo, WorkspaceView, prompt_fingerprint,
};

use crate::checkpoints::{Noted, Turns};
use crate::herdr;
use crate::persist::{self, Saved, SavedPane, SavedTab, SavedWorkspace};
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
/// The most user vars (OSC 1337) one pane keeps; a new name past it is
/// dropped.
const USER_VARS_CAP: usize = 16;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// What the pane was spawned with: a `pane run`'s command as given (before
    /// any sandbox wrapping), a split's login shell, a restore's recipe.
    /// Saved with the layout to say what ran here; a restore never re-runs it.
    argv: Vec<String>,
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
    /// The herdr reporter (`source`, `agent`) whose report holds the pane's
    /// hook-ranked claim, while one does (`apply_herdr`).
    herdr: Option<(String, String)>,
    /// The last `seq` accepted from each herdr source; it outlives claims.
    herdr_seqs: HashMap<String, u64>,
}

#[derive(Default)]
struct Ids {
    workspace: u64,
    tab: u64,
    pane: u64,
    approval: u64,
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
    fn approval(&mut self) -> u64 {
        self.approval += 1;
        self.approval
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
    /// herdr's reporting socket once it listens, exported to every pane
    /// spawned from then on with the rest of `HERDR_*`.
    herdr: Option<PathBuf>,
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
            herdr: None,
        }
    }

    /// Point every pane spawned from now on at herdr's socket, `socket`.
    pub fn set_herdr_socket(&mut self, socket: PathBuf) {
        self.herdr = Some(socket);
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
            .zip(self.parents())
            .map(|(w, parent)| WorkspaceInfo {
                id: w.id,
                name: w.name.clone(),
                dir: w.dir.clone(),
                parent,
            })
            .collect()
    }

    /// Each workspace's parent, in workspace order: a forked child resolves to
    /// whichever workspace currently owns its origin repo, or `None` when that
    /// origin has been killed — the child then stands top-level. Every jj repo
    /// root is computed once. The one answer `view` and `workspace_list` share.
    fn parents(&self) -> Vec<Option<WorkspaceId>> {
        let roots: Vec<(WorkspaceId, Option<PathBuf>)> = self
            .workspaces
            .iter()
            .map(|w| (w.id, crate::jj::workspace_root(&w.dir)))
            .collect();
        self.workspaces
            .iter()
            .map(|w| {
                w.fork.as_ref().and_then(|fork| {
                    roots.iter().find_map(|(id, root)| {
                        (*id != w.id && root.as_deref() == Some(fork.origin_root.as_path()))
                            .then_some(*id)
                    })
                })
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
    /// runs `cmd` through `tutti sandbox run <profile> --name <name> --`,
    /// titled for `cmd`, its sandbox named for the pane (`sandbox_name`).
    pub fn pane_run(
        &mut self,
        tab: Option<TabId>,
        cmd: Vec<String>,
        ephemeral: bool,
        sandbox: Option<String>,
    ) -> Result<PaneId> {
        let tab = self.resolve_tab(tab)?;
        let title = pane_title(cmd.first().context("empty command")?);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        // Named for the id `spawn_into_tab` allocates next.
        let sandbox =
            sandbox.map(|profile| (profile, sandbox_name(PaneId(self.ids.pane + 1), nanos)));
        let argv = match &sandbox {
            Some((profile, name)) => sandbox_argv(&client_exe()?, profile, name, cmd.clone()),
            None => cmd.clone(),
        };
        let (program, args) = argv.split_first().context("empty command")?;
        let dir = self.tab_workspace_dir(tab)?;
        let mut spec = PtySpec::new(program);
        spec.args = args.to_vec();
        spec.cwd = Some(dir);
        spec.env = vec![("TERM".into(), "xterm-256color".into())];
        let id = self.spawn_into_tab(tab, spec, cmd, title, None, ephemeral)?;
        if let Some(slot) = self.panes.get_mut(&id) {
            (slot.meta.sandbox, slot.meta.sandbox_name) = sandbox.unzip();
        }
        Ok(id)
    }

    /// Split `pane`'s cell in its tab, spawning a login shell in the new half
    /// — in the directory `pane`'s shell last reported (OSC 7) when that lies
    /// inside its workspace's directory (compared canonically), else in the
    /// workspace's directory.
    pub fn pane_split(&mut self, pane: PaneId, direction: Direction) -> Result<PaneId> {
        let slot = self
            .panes
            .get(&pane)
            .with_context(|| format!("no pane {pane}"))?;
        let tab = slot.tab;
        let workspace = self.tab_workspace_dir(tab)?;
        let canon = |p: &Path| std::fs::canonicalize(p).ok();
        let dir = slot
            .pty
            .cwd()
            .map(PathBuf::from)
            .filter(|cwd| {
                canon(cwd)
                    .zip(canon(&workspace))
                    .is_some_and(|(cwd, ws)| cwd.starts_with(ws))
            })
            .unwrap_or(workspace);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut spec = PtySpec::new(&shell);
        spec.cwd = Some(dir);
        spec.env = vec![("TERM".into(), "xterm-256color".into())];
        let title = pane_title(&shell);
        self.spawn_into_tab(
            tab,
            spec,
            vec![shell],
            title,
            Some((pane, direction)),
            false,
        )
    }

    /// `tutti run` under one guard: resolve the workspace — `target`'s
    /// `workspace`, else the one with its `dir` open (compared canonically),
    /// else a fresh mount of `dir`, which must exist, else that of `from` (the
    /// calling pane), else the current one — and spawn `cmd`, titled `title`
    /// when given and linked to `issue`, in a fresh tab there: a new tab, or a
    /// fresh mount's own empty one. The session's current tab stays where it
    /// was (an empty session's aside), so an attached client's launches still
    /// land in the tab it is looking at. A failed spawn leaves nothing behind.
    /// Returns the workspace, tab and pane, and whether the workspace was
    /// mounted.
    pub fn run_in_fresh_tab(
        &mut self,
        RunTarget {
            workspace,
            dir,
            from,
        }: RunTarget,
        cmd: Vec<String>,
        sandbox: Option<String>,
        title: Option<String>,
        issue: Option<IssueRef>,
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
                let workspace = match (workspace, from) {
                    (None, Some(pane)) => Some(
                        self.workspace_of_pane(pane)
                            .with_context(|| format!("no pane {pane}"))?,
                    ),
                    (workspace, _) => workspace,
                };
                let id = self.resolve_workspace(workspace)?;
                (id, self.tab_new(Some(id))?, false)
            }
        };
        let spawned = self.pane_run(Some(tab), cmd, false, sandbox);
        match &spawned {
            Ok(pane) => {
                self.current_tab = current.or(Some(tab));
                if let Some(slot) = self.panes.get_mut(pane) {
                    if let Some(title) = title {
                        slot.meta.title = title;
                    }
                    slot.meta.issue = issue.map(Box::new);
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
        self.workspaces
            .iter()
            .zip(self.parents())
            .map(|(w, parent)| WorkspaceView {
                id: w.id,
                name: w.name.clone(),
                dir: w.dir.clone(),
                branch: git_branch(&w.dir),
                changes: w.changes.clone(),
                changed_files: w.changed_files.clone(),
                stale: w.stale,
                parent,
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

    /// Every pane flat, in sidebar order, as a watcher's snapshot lists it:
    /// its workspace and tab by name, and whether it is `focused`. Read
    /// straight off the model — `view` probes git and jj, and this runs on
    /// the render tick.
    pub fn watch_panes(&self, focused: Option<PaneId>) -> Vec<WatchPane> {
        self.workspaces
            .iter()
            .flat_map(|w| w.tabs.iter().map(move |t| (w, t)))
            .flat_map(|(w, t)| {
                t.layout
                    .as_ref()
                    .map(Layout::panes)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|id| self.panes.get(&id))
                    .map(move |slot| WatchPane {
                        id: slot.meta.id,
                        workspace: w.name.clone(),
                        dir: w.dir.clone(),
                        tab: self.tab_display_name(t),
                        title: slot.meta.title.clone(),
                        // As `pane_info` presents it, without cloning the
                        // checkpoints on every tick.
                        agent: slot
                            .meta
                            .agent
                            .clone()
                            .or_else(|| slot.label.clone().map(AgentKind)),
                        state: slot.meta.state,
                        source: slot.meta.source,
                        exited: slot.meta.exited,
                        focused: focused == Some(slot.meta.id),
                        user_vars: slot.meta.user_vars.clone(),
                    })
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

    /// Set the ratio of the split separating `a` from `b` in their tab — a
    /// split-border drag. Returns whether the layout actually changed (a drag
    /// held against the clamp does not).
    pub fn pane_split_ratio(&mut self, a: PaneId, b: PaneId, ratio: f32) -> Result<bool> {
        let tab_of = |pane: PaneId| {
            self.panes
                .get(&pane)
                .map(|slot| slot.tab)
                .with_context(|| format!("no pane {pane}"))
        };
        let tab_id = tab_of(a)?;
        if tab_of(b)? != tab_id {
            bail!("panes {a} and {b} are in different tabs");
        }
        let tab = self.tab_mut(tab_id).context("pane's tab vanished")?;
        let layout = tab.layout.as_ref().context("pane's tab has no layout")?;
        let new = layout
            .set_split_ratio(a, b, ratio)
            .with_context(|| format!("no split has pane {a} before pane {b}"))?;
        let changed = *layout != new;
        tab.layout = Some(new);
        Ok(changed)
    }

    pub fn pane_rename(&mut self, pane: PaneId, title: String) -> Result<()> {
        let slot = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("no pane {pane}"))?;
        slot.meta.title = title;
        Ok(())
    }

    /// Set one of `pane`'s user vars (OSC 1337 `SetUserVar`); an empty
    /// `value` unsets it, and a new name past `USER_VARS_CAP` is dropped.
    /// Returns whether the pane's vars changed.
    pub fn set_user_var(&mut self, pane: PaneId, name: String, value: String) -> bool {
        let Some(vars) = self.panes.get_mut(&pane).map(|s| &mut s.meta.user_vars) else {
            return false;
        };
        if value.is_empty() {
            return vars.remove(&name).is_some();
        }
        if vars.len() >= USER_VARS_CAP && !vars.contains_key(&name) {
            return false;
        }
        vars.insert(name, value.clone()) != Some(value)
    }

    /// The pane `toward` of `pane` in its tab (`Layout::neighbor`), `None` at
    /// an edge.
    pub fn pane_neighbor(
        &self,
        pane: PaneId,
        toward: Toward,
        wrap: bool,
    ) -> Result<Option<PaneId>> {
        let tab = self
            .panes
            .get(&pane)
            .with_context(|| format!("no pane {pane}"))?
            .tab;
        Ok(self
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .find(|t| t.id == tab)
            .and_then(|t| t.layout.as_ref())
            .and_then(|layout| layout.neighbor(pane, toward, wrap)))
    }

    /// Type `bytes` into `pane`. Input past a finished program's own report —
    /// a Program or Progress claim holding Done or Idle — ends that claim, the
    /// state staying until something moves it, so a reporter that never sent
    /// its clear cannot shut out the next one. Returns whether that settled
    /// the pane out of the agent surfaces (the caller rebroadcasts the view).
    pub fn pane_send(&mut self, pane: PaneId, bytes: &[u8]) -> Result<bool> {
        let slot = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("no pane {pane}"))?;
        slot.pty.write_input(bytes)?;
        let finished = matches!(
            slot.meta.source,
            Some(StateSource::Program | StateSource::Progress)
        ) && matches!(slot.meta.state, AgentState::Done | AgentState::Idle);
        if finished {
            slot.meta.source = None;
        }
        Ok(finished && self.settle(pane))
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
            slot.herdr = None;
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

    /// Hold a permission request on `pane` for an answer from tutti: the
    /// pending approval its view carries, under a fresh id an answer must
    /// name. `None` — the request goes straight on to Claude's own dialog —
    /// for an unknown pane, or one already holding an approval (parallel
    /// subagents can ask at once; one is held at a time).
    pub fn hold_approval(&mut self, pane: PaneId, tool: String, summary: String) -> Option<u64> {
        let slot = self
            .panes
            .get_mut(&pane)
            .filter(|s| s.meta.approval.is_none())?;
        let id = self.ids.approval();
        slot.meta.approval = Some(PendingApproval { id, tool, summary });
        Some(id)
    }

    /// End `pane`'s pending approval if it is `id` (whichever it is, when
    /// `None`) — answered, or its hook gone — returning the id it held.
    pub fn take_approval(&mut self, pane: PaneId, id: Option<u64>) -> Option<u64> {
        let slot = self.panes.get_mut(&pane)?;
        let held = slot.meta.approval.as_ref()?.id;
        if id.is_some_and(|id| id != held) {
            return None;
        }
        slot.meta.approval = None;
        Some(held)
    }

    /// Record the agent kind detected for a pane. Returns whether it changed. A
    /// change ends any program/progress claim and the self-reported label —
    /// they spoke for whatever ran there before; a hook claim stays (sticky),
    /// and so does a herdr claim when the agent detected is the one that
    /// reported it (detection catching up). An agent leaving takes its
    /// conversation and its turn's prompt along; one appearing keeps what its
    /// hooks reported before detection caught up with it. An agent appearing
    /// notes its start as a checkpoint for the caller's next change probe —
    /// detection lands well before the agent's first edit.
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
                let kept = slot.herdr.as_ref().is_none_or(|(_, reporter)| {
                    agent.as_ref().is_some_and(|kind| kind.0 == *reporter)
                });
                if slot.meta.source != Some(StateSource::Hook) || !kept {
                    slot.meta.source = None;
                    slot.herdr = None;
                }
                slot.meta.agent = agent;
                slot.label = None;
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

    /// Apply a herdr report (see `herdr`) to `pane`: the agent's own exact
    /// lifecycle, so its states claim at the hook rank, labelled by the agent
    /// as an OSC 7501 `app` labels and noting a message like a 7501 `msg`
    /// (`apply_report`). Where a hook claim is sticky, a herdr one ends when
    /// its reporter releases or clears it — settling as a 7501 clear does —
    /// when the child exits, and when another agent is detected. herdr leaves
    /// "done" to the multiplexer: an idle after work a herdr report held is
    /// Done — not after work another source guessed, like Crush's own OSC 9;4
    /// racing its startup idle — and an unseen Done stays. A stale `seq`
    /// changes nothing, nor does a state for an exited pane; the conversation
    /// follows `agent_session_id`. `None` for an unknown pane.
    pub fn apply_herdr(
        &mut self,
        pane: PaneId,
        report: herdr::Report,
        now: Instant,
    ) -> Option<ReportOutcome> {
        let slot = self.panes.get_mut(&pane)?;
        if !report.fresh(&mut slot.herdr_seqs) {
            return Some(ReportOutcome::default());
        }
        let renamed = report.session.is_some() && slot.meta.conversation != report.session;
        if renamed {
            slot.meta.conversation = report.session;
        }
        let holder = report.source.clone().zip(report.agent.clone());
        let ends = match report.act {
            herdr::Act::Release => holder.is_some() && slot.herdr == holder,
            herdr::Act::Clear => slot.herdr.as_ref().is_some_and(|(held, _)| {
                report.source.as_ref().is_none_or(|source| source == held)
            }),
            herdr::Act::State(..) | herdr::Act::Session => false,
        };
        let status = match report.act {
            herdr::Act::State(Some(state), msg) if slot.meta.exited.is_none() => {
                let reported_work = slot.herdr.is_some()
                    && matches!(slot.meta.state, AgentState::Working | AgentState::Blocked);
                let finished = reported_work || slot.meta.state == AgentState::Done;
                let state = match state {
                    AgentState::Idle if finished => AgentState::Done,
                    state => state,
                };
                slot.herdr = holder;
                Some(StatusReport {
                    source: StateSource::Hook,
                    state: Some(state),
                    app: report.agent,
                    msg,
                })
            }
            _ if ends => {
                slot.herdr = None;
                Some(StatusReport {
                    source: StateSource::Hook,
                    state: None,
                    app: None,
                    msg: None,
                })
            }
            _ => None,
        };
        let mut outcome = status.map_or_else(ReportOutcome::default, |status| {
            self.apply_report(pane, status, now)
        });
        outcome.relisted |= renamed;
        Some(outcome)
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
                slot.herdr = None;
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

    /// The session as `persist` saves it: each workspace's directory, name and
    /// fork meta; each tab's layout, active pane and own name; each pane's
    /// title, argv, agent, conversation and checkpoints; and the current tab.
    /// Exited and ephemeral panes are left out, and so are sandbox panes —
    /// their pods die with them. A tab left with no panes goes too; a
    /// workspace left with no tabs stays, a bare mount. `None` for an empty
    /// session, which has nothing to offer back.
    pub fn snapshot(&self) -> Option<Saved> {
        if self.workspaces.is_empty() {
            return None;
        }
        let kept = |id: &PaneId| {
            self.panes.get(id).is_some_and(|s| {
                s.meta.exited.is_none() && !s.ephemeral && s.meta.sandbox.is_none()
            })
        };
        let workspaces = self
            .workspaces
            .iter()
            .map(|w| SavedWorkspace {
                dir: w.dir.clone(),
                name: w.name.clone(),
                fork: w.fork.clone(),
                tabs: w
                    .tabs
                    .iter()
                    .filter_map(|t| {
                        let layout = t.layout.as_ref()?;
                        let ids: HashMap<PaneId, PaneId> = layout
                            .panes()
                            .into_iter()
                            .filter(kept)
                            .map(|id| (id, id))
                            .collect();
                        let layout = rename_leaves(layout, &ids)?;
                        Some(SavedTab {
                            id: t.id,
                            name: (t.name != t.id.to_string()).then(|| t.name.clone()),
                            active_pane: t.active_pane.filter(|p| ids.contains_key(p)),
                            panes: layout
                                .panes()
                                .into_iter()
                                .map(|id| {
                                    let slot = &self.panes[&id];
                                    SavedPane {
                                        id,
                                        title: slot.meta.title.clone(),
                                        argv: slot.argv.clone(),
                                        agent: slot.meta.agent.clone(),
                                        conversation: slot.meta.conversation.clone(),
                                        issue: slot.meta.issue.clone(),
                                        turns: slot.turns.clone(),
                                    }
                                })
                                .collect(),
                            layout,
                        })
                    })
                    .collect(),
            })
            .collect();
        Some(Saved {
            format: persist::FORMAT,
            workspaces,
            current_tab: self.current_tab,
        })
    }

    /// Bring a saved session back. Each saved workspace is mounted with its
    /// fork meta — so a restored workspace still merges and discards — or, when
    /// one is already open at the same directory (compared canonically), its
    /// tabs join that one; a workspace whose directory is gone is skipped. Each
    /// saved tab comes back as a new tab with the same split tree, ratios, name
    /// and active pane, every leaf a freshly spawned pane. A pane `revive` maps
    /// to an argv — an agent's own resume recipe, `SavedPane::revive` — runs it
    /// under its old title, keeping its agent and conversation; every other one
    /// is a login shell in the workspace's directory. A saved command is never
    /// re-run — re-running `claude "fix the tests"` would re-issue the prompt.
    /// Each pane's checkpoints carry over, numbering and head intact; a revived
    /// agent notes its start, as a detected one does. A pane that fails to
    /// spawn is dropped from its tree and named in `skipped`. The saved current
    /// tab becomes current again.
    pub fn restore(
        &mut self,
        saved: Saved,
        revive: impl Fn(&SavedPane) -> Option<Vec<String>>,
    ) -> Restored {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let previous = self.current_tab;
        let mut current = None;
        let mut out = Restored::default();
        for saved_ws in saved.workspaces {
            let dir = saved_ws.dir;
            if !dir.is_dir() {
                out.skipped
                    .push(format!("{}: the directory is gone", dir.display()));
                continue;
            }
            let wanted = canon(&dir);
            let (workspace, mut fresh_tab) =
                match self.workspaces.iter_mut().find(|w| canon(&w.dir) == wanted) {
                    Some(open) => {
                        open.fork = open.fork.take().or(saved_ws.fork);
                        (open.id, None)
                    }
                    None => {
                        let (id, tab) = self.push_workspace(dir.clone(), saved_ws.fork);
                        if let Some(mounted) = self.workspaces.last_mut() {
                            mounted.name = saved_ws.name;
                        }
                        (id, Some(tab))
                    }
                };
            if !out.workspaces.contains(&workspace) {
                out.workspaces.push(workspace);
            }
            for saved_tab in saved_ws.tabs {
                // A fresh mount's own first tab takes its first saved tab.
                let tab = match fresh_tab.take() {
                    Some(tab) => tab,
                    None => self.tab_new(Some(workspace)).expect("the workspace exists"),
                };
                let mut ids = HashMap::new();
                for pane in saved_tab.panes {
                    let revived = revive(&pane);
                    let (argv, title) = match &revived {
                        Some(cmd) => (cmd.clone(), pane.title.clone()),
                        None => (vec![shell.clone()], pane_title(&shell)),
                    };
                    let mut spec = PtySpec::new(&argv[0]);
                    spec.args = argv[1..].to_vec();
                    spec.cwd = Some(dir.clone());
                    spec.env = vec![("TERM".into(), "xterm-256color".into())];
                    let id = match self.spawn_into_tab(tab, spec, argv, title, None, false) {
                        Ok(id) => id,
                        Err(err) => {
                            out.skipped.push(format!("{}: {err:#}", pane.title));
                            continue;
                        }
                    };
                    ids.insert(pane.id, id);
                    out.panes.push(id);
                    if let Some(slot) = self.panes.get_mut(&id) {
                        slot.turns = pane.turns;
                        if revived.is_some() {
                            slot.meta.conversation = pane.conversation;
                            slot.meta.issue = pane.issue;
                        }
                    }
                    if revived.is_some() {
                        self.set_agent(id, pane.agent);
                        out.revived += 1;
                    }
                }
                let entry = self.tab_mut(tab).expect("the tab exists");
                entry.layout = rename_leaves(&saved_tab.layout, &ids);
                entry.active_pane = saved_tab
                    .active_pane
                    .and_then(|p| ids.get(&p).copied())
                    .or_else(|| entry.layout.as_ref().map(|l| l.panes()[0]));
                if let Some(name) = saved_tab.name {
                    entry.name = name;
                }
                if saved.current_tab == Some(saved_tab.id) && entry.layout.is_some() {
                    current = Some(tab);
                }
            }
        }
        self.current_tab = current.or(previous).or(self.current_tab);
        out
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
        argv: Vec<String>,
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
        // Its herdr reporters reach this daemon, never a herdr the daemon
        // itself was started inside.
        let inherited = std::env::vars_os().filter_map(|(key, _)| key.into_string().ok());
        spec.env_remove
            .extend(inherited.filter(|key| key.starts_with("HERDR_")));
        if let (Some(socket), Some(workspace)) = (&self.herdr, self.workspace_of_tab(tab)) {
            spec.env.extend(herdr::env(socket, workspace, tab, id));
        }
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
                    approval: None,
                    source: None,
                    sandbox: None,
                    issue: None,
                    conversation: None,
                    sandbox_name: None,
                    user_vars: BTreeMap::new(),
                    cwd: None,
                },
                pty,
                tab,
                argv,
                ephemeral,
                answered: None,
                turns: Turns::default(),
                label: None,
                alive: Instant::now(),
                notified: None,
                herdr: None,
                herdr_seqs: HashMap::new(),
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

/// Where a `tutti run` lands (`Session::run_in_fresh_tab`): `workspace`, else
/// the one open at `dir`, else a fresh mount of it, else the calling pane
/// `from`'s workspace, else the current one.
#[derive(Debug, Default)]
pub struct RunTarget {
    pub workspace: Option<WorkspaceId>,
    pub dir: Option<PathBuf>,
    pub from: Option<PaneId>,
}

/// What `Session::restore` brought back, so the server can wire it up.
#[derive(Debug, Default)]
pub struct Restored {
    /// Each workspace the saved ones landed in, mounted or joined — the ones
    /// to refresh.
    pub workspaces: Vec<WorkspaceId>,
    /// The panes it spawned, each needing a reaper.
    pub panes: Vec<PaneId>,
    /// How many of those revived an agent's conversation.
    pub revived: usize,
    /// What could not come back, one line each.
    pub skipped: Vec<String>,
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

/// A pane's wire metadata, with the cwd its shell last reported. One with no
/// detected agent that is self-reporting presents its label as the agent,
/// which is all every agent surface keys on.
fn pane_info(slot: &PaneSlot) -> PaneInfo {
    let mut info = PaneInfo {
        checkpoints: slot.turns.entries().to_vec(),
        cwd: slot.pty.cwd(),
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

/// `layout` with every leaf renamed through `ids` and a leaf missing from it
/// dropped, its split collapsing into the sibling, ratios untouched — how a
/// snapshot leaves unsaved panes out and a restore maps saved panes onto fresh
/// ones. `None` once no leaf is left.
fn rename_leaves(layout: &Layout, ids: &HashMap<PaneId, PaneId>) -> Option<Layout> {
    match layout {
        Layout::Leaf(id) => ids.get(id).map(|id| Layout::Leaf(*id)),
        Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => match (rename_leaves(first, ids), rename_leaves(second, ids)) {
            (Some(first), Some(second)) => Some(Layout::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (first, second) => first.or(second),
        },
    }
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

/// The argv running `cmd` in sandbox `profile` under `name`: `<exe> sandbox
/// run <profile> --name <name> -- <cmd…>`.
fn sandbox_argv(exe: &str, profile: &str, name: &str, cmd: Vec<String>) -> Vec<String> {
    [exe, "sandbox", "run", profile, "--name", name, "--"]
        .into_iter()
        .map(String::from)
        .chain(cmd)
        .collect()
}

/// A DNS-1123 name for pane `pane`'s sandbox, unique to this launch:
/// `tutti-p<pane>-<nanos hex>`.
fn sandbox_name(pane: PaneId, nanos: u64) -> String {
    format!("tutti-p{pane}-{nanos:x}")
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
    use super::{
        ForkMeta, STALL_AFTER, Session, git_branch, pane_title, parse_head, rename_leaves,
        sandbox_argv, sandbox_name, stop_subagent,
    };
    use crate::herdr;
    use crate::persist::{Saved, SavedPane};
    use crate::pty::{PaneSize, StatusReport};
    use AgentState::{Blocked, Done, Idle, Unknown, Working};
    use StateSource::{Hook, Program, Progress, Screen};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use tutti_core::{
        AgentHookEvent, AgentState, Direction, FileStat, Layout, Observation, PaneId, PaneInfo,
        PendingApproval, StateSource, SubagentInfo,
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
    fn user_vars_set_replace_unset_and_cap_at_sixteen() {
        let (mut s, pane, _tmp) = session_with_pane();
        let mut set = |name: &str, value: &str| s.set_user_var(pane, name.into(), value.into());
        assert!(set("TUTTI_KEYS", "pane"));
        assert!(!set("TUTTI_KEYS", "pane"), "the same value changes nothing");
        assert!(set("TUTTI_KEYS", "tutti"));
        assert!(set("TUTTI_KEYS", ""), "an empty value unsets");
        assert!(
            !set("TUTTI_KEYS", ""),
            "unsetting an unset var changes nothing"
        );
        for n in 0..super::USER_VARS_CAP {
            assert!(set(&format!("V{n}"), "1"));
        }
        assert!(
            !set("ONE_TOO_MANY", "1"),
            "a new name past the cap is dropped"
        );
        assert!(set("V0", "2"), "a held name still changes");
        assert!(!s.set_user_var(PaneId(99), "A".into(), "1".into()));
        let vars = info(&s, pane).user_vars;
        assert_eq!(vars.len(), super::USER_VARS_CAP);
        assert_eq!(vars.get("V0").map(String::as_str), Some("2"));
        assert!(!vars.contains_key("ONE_TOO_MANY"));
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
    fn a_pane_holds_one_approval_until_it_is_taken_by_id() {
        let (mut s, pane, _tmp) = session_with_pane();
        let held = |s: &Session| info(s, pane).approval;
        let first = s
            .hold_approval(pane, "Bash".into(), "rm -rf build".into())
            .unwrap();
        assert_eq!(
            held(&s),
            Some(PendingApproval {
                id: first,
                tool: "Bash".into(),
                summary: "rm -rf build".into(),
            })
        );
        assert_eq!(
            s.hold_approval(pane, "Edit".into(), "src/main.rs".into()),
            None,
            "one is held at a time: a second goes on to Claude's dialog"
        );
        assert_eq!(s.take_approval(pane, Some(first + 1)), None, "a stale id");
        assert!(held(&s).is_some(), "takes nothing");
        assert_eq!(s.take_approval(pane, Some(first)), Some(first));
        assert_eq!(held(&s), None);
        assert_eq!(s.take_approval(pane, None), None, "nothing left to take");

        let second = s
            .hold_approval(pane, "Edit".into(), "src/main.rs".into())
            .unwrap();
        assert!(second > first, "an id is never reused");
        assert_eq!(
            s.take_approval(pane, None),
            Some(second),
            "`None` takes whichever is held"
        );
        assert_eq!(
            s.hold_approval(PaneId(99), "Bash".into(), "ls".into()),
            None,
            "an unknown pane holds nothing"
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
        assert_eq!(
            s.watch_panes(None)[0].agent,
            Some("build".into()),
            "watchers see the label too"
        );
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

    #[test]
    fn typing_ends_a_finished_program_claim_and_nothing_else() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_report(pane, app_report(Some(Done), "build", None), t0);
        assert!(
            !s.pane_send(pane, b"x").unwrap(),
            "an unseen Done stays listed"
        );
        assert_eq!(
            state_of(&s, pane),
            (Done, None),
            "the claim ended, the state stays"
        );
        s.apply_report(pane, report(Progress, Some(Working)), t0);
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Progress)),
            "so the next report lands"
        );
        s.pane_send(pane, b"x").unwrap();
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Progress)),
            "a claim in motion holds"
        );
        s.apply_report(pane, app_report(Some(Idle), "build", None), t0);
        assert!(
            s.pane_send(pane, b"x").unwrap(),
            "an Idle settles out of the surfaces"
        );
        assert_eq!(state_of(&s, pane), (Idle, None));
        s.apply_agent_event(pane, AgentHookEvent::Done, None);
        s.pane_send(pane, b"x").unwrap();
        assert_eq!(
            state_of(&s, pane),
            (Done, Some(Hook)),
            "a hook claim never ends so"
        );
    }

    /// A herdr report from Crush (source and agent `crush`) with `seq`.
    fn crush(seq: u64, act: herdr::Act) -> herdr::Report {
        herdr::Report {
            source: Some("crush".into()),
            agent: Some("crush".into()),
            seq: Some(seq),
            session: None,
            act,
        }
    }

    fn crush_state(seq: u64, state: AgentState) -> herdr::Report {
        crush(seq, herdr::Act::State(Some(state), None))
    }

    #[test]
    fn a_herdr_report_claims_at_the_hook_rank_until_released() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        // Crush's own OSC 9;4;3 races its startup idle (observed live).
        s.apply_report(pane, report(Progress, Some(Working)), t0);
        let outcome = s.apply_herdr(pane, crush_state(1, Idle), t0).unwrap();
        assert!(outcome.relisted, "the agent labels the pane");
        assert_eq!(
            state_of(&s, pane),
            (Idle, Some(Hook)),
            "idle after work no herdr report held is no finish"
        );
        assert_eq!(info(&s, pane).agent, Some("crush".into()));

        s.apply_herdr(pane, crush_state(2, Working), t0);
        s.apply_report(pane, report(Program, Some(Done)), t0);
        assert_eq!(s.observe(pane, true, Some(Observation::Blocked), t0), None);
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Hook)),
            "nothing below moves it"
        );

        let blocked = herdr::Act::State(Some(Blocked), Some("allow bash?".into()));
        let outcome = s.apply_herdr(pane, crush(3, blocked), t0).unwrap();
        let note = Some((Some("crush".to_string()), "allow bash?".to_string()));
        assert_eq!(
            outcome.notify, note,
            "the message notifies, titled by the agent"
        );
        s.apply_herdr(pane, crush_state(4, Idle), t0);
        assert_eq!(
            state_of(&s, pane),
            (Done, Some(Hook)),
            "idle after work is a finish"
        );
        s.apply_herdr(pane, crush_state(5, Idle), t0);
        assert_eq!(state_of(&s, pane).0, Done, "and an unseen finish stays one");

        let mut stranger = crush(6, herdr::Act::Release);
        stranger.agent = Some("other".into());
        s.apply_herdr(pane, stranger, t0);
        assert_eq!(
            state_of(&s, pane),
            (Done, Some(Hook)),
            "only its holder releases it"
        );
        s.apply_herdr(pane, crush(7, herdr::Act::Release), t0);
        assert_eq!(
            state_of(&s, pane),
            (Done, None),
            "released: an outcome stays"
        );
        s.apply_report(pane, report(Program, Some(Working)), t0);
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Program)),
            "the claim is gone"
        );
    }

    #[test]
    fn a_herdr_report_follows_seq_and_names_the_conversation() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_herdr(pane, crush_state(10, Working), t0);
        s.apply_herdr(pane, crush_state(9, Blocked), t0);
        let mut seqless = crush_state(0, Blocked);
        seqless.seq = None;
        s.apply_herdr(pane, seqless, t0);
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Hook)),
            "stale reports change nothing"
        );
        s.apply_herdr(pane, crush(11, herdr::Act::State(None, None)), t0);
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Hook)),
            "unknown changes nothing"
        );

        let mut named = crush(12, herdr::Act::Session);
        named.session = Some("s1".into());
        assert!(s.apply_herdr(pane, named, t0).unwrap().relisted);
        assert_eq!(info(&s, pane).conversation.as_deref(), Some("s1"));
        s.apply_herdr(pane, crush(13, herdr::Act::Release), t0);
        assert_eq!(
            state_of(&s, pane),
            (Idle, None),
            "released mid-work, it settles"
        );
        assert!(s.settle(pane), "and leaves the agent surfaces");
        s.apply_herdr(pane, crush_state(13, Working), t0);
        assert_eq!(
            state_of(&s, pane),
            (Idle, None),
            "the seq mark outlives the claim"
        );
    }

    #[test]
    fn a_herdr_claim_ends_on_another_agent_but_not_on_its_own() {
        let (mut s, pane, _tmp) = session_with_pane();
        let t0 = Instant::now();
        s.apply_herdr(pane, crush_state(1, Working), t0);
        s.set_agent(pane, Some("crush".into()));
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Hook)),
            "detection caught up"
        );
        s.set_agent(pane, None);
        assert_eq!(
            state_of(&s, pane),
            (Working, None),
            "its agent left: the stall decides"
        );

        s.apply_herdr(pane, crush_state(2, Working), t0);
        s.set_agent(pane, Some("claude".into()));
        assert_eq!(
            state_of(&s, pane),
            (Working, None),
            "another agent took over"
        );

        s.apply_herdr(pane, crush_state(3, Working), t0);
        let mut clear = crush(4, herdr::Act::Clear);
        clear.source = Some("other".into());
        s.apply_herdr(pane, clear, t0);
        assert_eq!(state_of(&s, pane).1, Some(Hook), "a clear names its source");
        let mut clear = crush(5, herdr::Act::Clear);
        (clear.source, clear.agent) = (None, None);
        s.apply_herdr(pane, clear, t0);
        assert_eq!(
            state_of(&s, pane),
            (Idle, None),
            "or clears whichever holds"
        );

        s.apply_herdr(pane, crush_state(6, Working), t0);
        s.apply_agent_event(pane, AgentHookEvent::Activity { detail: None }, None);
        s.apply_herdr(pane, crush(7, herdr::Act::Release), t0);
        assert_eq!(
            state_of(&s, pane),
            (Working, Some(Hook)),
            "a hook took the claim over, for good"
        );

        s.mark_exited(pane, 0);
        s.apply_herdr(pane, crush_state(8, Working), t0);
        assert_eq!(
            state_of(&s, pane),
            (Done, None),
            "an exited pane takes no claim"
        );
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

    /// A workspace tutti checked out of a project lists that project as its
    /// parent — the same answer the view nests it by — and stands top-level
    /// once the project is gone.
    #[test]
    fn a_nested_workspace_lists_its_parent_project() {
        let tmp = TempDir::new();
        let origin = tmp.path().join("api");
        std::fs::create_dir_all(origin.join(".jj")).unwrap();
        let child = tmp.path().join("api-fix");
        std::fs::create_dir_all(child.join(".jj")).unwrap();
        let mut session = Session::new(PaneSize::new(24, 80), "test");
        let project = session.workspace_new(origin.clone());
        let fork = super::ForkMeta {
            origin_root: origin,
            jj_name: "fix".into(),
        };
        let (workspace, _) = session.workspace_new_forked(child, fork);

        let parents = |s: &Session| {
            s.workspace_list()
                .into_iter()
                .map(|w| (w.id, w.parent))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parents(&session),
            [(project, None), (workspace, Some(project))]
        );
        let view: Vec<_> = session
            .view()
            .into_iter()
            .map(|w| (w.id, w.parent))
            .collect();
        assert_eq!(
            view,
            parents(&session),
            "the listing nests as the view does"
        );

        session.workspace_kill(project).unwrap();
        assert_eq!(parents(&session), [(workspace, None)]);
    }

    #[test]
    fn a_sandbox_pane_runs_its_command_through_the_wrapper() {
        assert_eq!(
            sandbox_argv(
                "/bin/tutti",
                "gke",
                "tutti-p3-ab",
                vec!["claude".into(), "-c".into()]
            ),
            [
                "/bin/tutti",
                "sandbox",
                "run",
                "gke",
                "--name",
                "tutti-p3-ab",
                "--",
                "claude",
                "-c"
            ]
        );
    }

    #[test]
    fn a_sandbox_is_named_for_its_pane_and_launch() {
        assert_eq!(sandbox_name(PaneId(12), 0xabc), "tutti-p12-abc");
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

    fn sleep_cmd() -> Vec<String> {
        vec!["/bin/sleep".into(), "30".into()]
    }

    /// Revive exactly the panes the real recipe would, but run `sleep` in
    /// place of their agent tool: no real agent ever starts under test.
    fn revive_as_sleep(pane: &SavedPane) -> Option<Vec<String>> {
        pane.revive().map(|_| sleep_cmd())
    }

    /// `saved` through the JSON the live file holds, as the next daemon reads
    /// it back.
    fn reread(saved: Saved) -> Saved {
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap()
    }

    #[test]
    fn a_snapshot_restores_its_layout_agents_and_turns() {
        let (one, two) = (TempDir::new(), TempDir::new());
        let mut s = Session::new(PaneSize::new(24, 80), "test");
        assert!(s.snapshot().is_none(), "an empty session saves nothing");
        let a = s.workspace_new(one.path().to_path_buf());
        let agent = s.pane_run(None, sleep_cmd(), false, None).unwrap();
        let shell = s.pane_split(agent, Direction::Vertical).unwrap();
        s.pane_resize_split(agent, Direction::Vertical, 0.15)
            .unwrap();
        s.pane_rename(agent, "fixer".into()).unwrap();
        s.set_agent(agent, Some("claude".into()));
        s.apply_agent_event(agent, AgentHookEvent::SessionStarted, Some("c1".into()));
        let issue = tutti_core::IssueRef {
            source: "gh".into(),
            id: "123".into(),
            title: "the tests fail".into(),
            url: None,
        };
        s.panes.get_mut(&agent).unwrap().meta.issue = Some(Box::new(issue.clone()));
        let edit = [FileStat {
            path: "a.rs".into(),
            insertions: 3,
            deletions: 1,
        }];
        let turns = s.turns(agent).unwrap();
        turns.take_pending();
        turns.record(None, "k1", "start", None, 1, &[]).unwrap();
        let fix = Some("fix it");
        turns
            .record(Some("k1".into()), "k2", "done", fix, 2, &edit)
            .unwrap();
        let logs = s.tab_new(Some(a)).unwrap();
        s.pane_run(Some(logs), sleep_cmd(), false, None).unwrap();
        s.tab_mut(logs).unwrap().name = "logs".into();
        let fork = ForkMeta {
            origin_root: one.path().to_path_buf(),
            jj_name: "feature".into(),
        };
        s.workspace_new_forked(two.path().to_path_buf(), fork.clone());
        s.pane_run(None, sleep_cmd(), false, None).unwrap();
        s.set_active_pane(shell).unwrap();

        let mut r = Session::new(PaneSize::new(24, 80), "test");
        let out = r.restore(reread(s.snapshot().unwrap()), revive_as_sleep);
        assert_eq!(
            (out.workspaces.len(), out.panes.len(), out.revived),
            (2, 4, 1)
        );
        assert!(out.skipped.is_empty(), "{:?}", out.skipped);

        let (before, after) = (s.view(), r.view());
        assert_eq!(after.len(), 2);
        for (was, now) in before.iter().zip(&after) {
            assert_eq!((&was.dir, &was.name), (&now.dir, &now.name));
            assert_eq!(was.tabs.len(), now.tabs.len());
            for (was, now) in was.tabs.iter().zip(&now.tabs) {
                let (old, new) = (was.layout.as_ref().unwrap(), now.layout.as_ref().unwrap());
                let ids: HashMap<PaneId, PaneId> =
                    old.panes().into_iter().zip(new.panes()).collect();
                assert_eq!(
                    rename_leaves(old, &ids).as_ref(),
                    Some(new),
                    "the same tree, ratios exact"
                );
                assert_eq!(was.active_pane.map(|p| ids[&p]), now.active_pane);
                assert_eq!(was.active, now.active, "the current tab is current again");
            }
        }
        assert_eq!(
            after[0].tabs[1].name, "logs",
            "a renamed tab keeps its name"
        );
        assert_eq!(r.workspace_fork_meta(after[1].id), Some(fork));

        let shell_program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let shell_title = pane_title(&shell_program);
        let titles: Vec<&str> = after
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .map(|p| p.title.as_str())
            .collect();
        assert_eq!(
            titles,
            ["fixer", &shell_title, &shell_title, &shell_title],
            "a revived agent keeps its title; the rest are shells"
        );
        let (revived, respawned) = (after[0].tabs[0].panes[0].id, after[0].tabs[0].panes[1].id);
        let info = r.pane(revived).unwrap();
        assert_eq!(info.agent, Some("claude".into()));
        assert_eq!(info.conversation.as_deref(), Some("c1"));
        assert_eq!(
            info.issue.as_deref(),
            Some(&issue),
            "a revived agent stays linked to its issue"
        );
        assert_eq!(
            r.panes[&revived].argv,
            sleep_cmd(),
            "the revive recipe ran, not the saved command"
        );
        assert_eq!(r.panes[&respawned].argv, [shell_program]);
        assert_eq!(r.pane(respawned).unwrap().agent, None);

        let turns = r.turns(revived).unwrap();
        let listed: Vec<(u64, Option<&str>)> = turns
            .entries()
            .iter()
            .map(|c| (c.n, c.prompt.as_deref()))
            .collect();
        assert_eq!(listed, [(1, None), (2, fix)]);
        assert_eq!(
            turns.take_pending(),
            [("start".to_string(), None)],
            "a revived agent notes where it starts, as a detected one does"
        );
        assert_eq!(
            turns.record(Some("k2".into()), "k3", "done", None, 3, &edit),
            Some(3),
            "the numbering and the head carry on"
        );
    }

    #[test]
    fn exited_ephemeral_and_sandbox_panes_are_left_out_of_a_snapshot() {
        let (mut s, pane, _tmp) = session_with_pane();
        let exited = s.pane_split(pane, Direction::Horizontal).unwrap();
        s.mark_exited(exited, 0);
        s.pane_run(None, sleep_cmd(), true, None).unwrap();
        let sandboxed = s.pane_split(pane, Direction::Vertical).unwrap();
        s.panes.get_mut(&sandboxed).unwrap().meta.sandbox = Some("gke".into());
        let lone = s.tab_new(None).unwrap();
        let dead = s.pane_run(Some(lone), sleep_cmd(), false, None).unwrap();
        s.mark_exited(dead, 1);
        let bare = TempDir::new();
        s.workspace_new(bare.path().to_path_buf());

        let saved = s.snapshot().unwrap();
        let tabs = &saved.workspaces[0].tabs;
        assert_eq!(tabs.len(), 1, "a tab left with no panes is dropped");
        assert_eq!(tabs[0].layout, Layout::Leaf(pane), "their splits collapse");
        assert_eq!(
            tabs[0].panes.iter().map(|p| p.id).collect::<Vec<_>>(),
            [pane]
        );
        assert!(
            saved.workspaces[1].tabs.is_empty(),
            "a workspace left with no panes is still saved"
        );

        let mut r = Session::new(PaneSize::new(24, 80), "test");
        let out = r.restore(saved, |_| None);
        assert_eq!((out.workspaces.len(), out.panes.len()), (2, 1));
        assert_eq!(
            r.tab_list(Some(out.workspaces[1])).unwrap().len(),
            1,
            "it comes back a bare mount, its own empty tab and nothing else"
        );
    }

    #[test]
    fn a_workspace_whose_directory_is_gone_is_skipped() {
        let (mut s, _pane, _tmp) = session_with_pane();
        let gone = TempDir::new();
        s.workspace_new(gone.path().to_path_buf());
        let saved = s.snapshot().unwrap();
        let path = gone.path().display().to_string();
        drop(gone);

        let mut r = Session::new(PaneSize::new(24, 80), "test");
        let out = r.restore(saved, |_| None);
        assert_eq!((out.workspaces.len(), out.panes.len()), (1, 1));
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].contains(&path), "{:?}", out.skipped);
        assert_eq!(r.workspace_list().len(), 1);
    }

    #[test]
    fn a_saved_workspace_joins_the_one_already_open_at_its_directory() {
        let (s, _pane, tmp) = session_with_pane();
        let saved = s.snapshot().unwrap();

        let mut r = Session::new(PaneSize::new(24, 80), "test");
        let open = r.workspace_new(tmp.path().join("."));
        r.pane_run(None, sleep_cmd(), false, None).unwrap();
        let out = r.restore(saved, |_| None);
        assert_eq!(out.workspaces, [open]);
        assert_eq!(
            r.workspace_list().len(),
            1,
            "no second mount of the same directory"
        );
        assert_eq!(
            r.tab_list(Some(open)).unwrap().len(),
            2,
            "the saved tab joined the open workspace's own"
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
