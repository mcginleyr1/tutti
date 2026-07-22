//! Server-side session state: the workspace/tab/pane tree plus one persistent
//! `PtyPane` per pane. Pure model — it owns no sockets and speaks no protocol,
//! it just answers the operations the dispatcher maps requests onto and reports
//! which panes it touched so the caller can drive the pty lifecycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tutti_core::{
    AgentKind, AgentState, Direction, Layout, Observation, PaneId, PaneInfo, StateEvent, TabId,
    TabInfo, TabView, WorkspaceId, WorkspaceInfo, WorkspaceView,
};

use crate::pty::{PaneSize, PtyPane, PtySpec};

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
    tabs: Vec<TabEntry>,
}

struct PaneSlot {
    meta: PaneInfo,
    pty: Arc<PtyPane>,
    tab: TabId,
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
}

impl Session {
    pub fn new(size: PaneSize) -> Self {
        Self {
            workspaces: Vec::new(),
            panes: HashMap::new(),
            current_tab: None,
            size,
            ids: Ids::default(),
        }
    }

    pub fn workspace_new(&mut self, dir: PathBuf) -> WorkspaceId {
        let id = self.ids.workspace();
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("workspace-{id}"));
        let tab = self.new_tab_entry();
        self.current_tab = Some(tab.id);
        self.workspaces.push(WorkspaceEntry {
            id,
            name,
            dir,
            tabs: vec![tab],
        });
        id
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
                name: t.name.clone(),
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
    /// new pane in the tab's layout and focusing it.
    pub fn pane_run(&mut self, tab: Option<TabId>, cmd: Vec<String>) -> Result<PaneId> {
        let tab = self.resolve_tab(tab)?;
        let (program, args) = cmd.split_first().context("empty command")?;
        let dir = self.tab_workspace_dir(tab)?;
        let mut spec = PtySpec::new(program);
        spec.args = args.to_vec();
        spec.cwd = Some(dir);
        spec.env = vec![("TERM".into(), "xterm-256color".into())];
        let title = pane_title(program);
        self.spawn_into_tab(tab, spec, title, None)
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
        self.spawn_into_tab(tab, spec, title, Some((pane, direction)))
    }

    /// The whole session as the attach protocol's view: workspaces, their tabs,
    /// each tab's layout tree and the `PaneInfo` for the panes it holds.
    pub fn view(&self) -> Vec<WorkspaceView> {
        self.workspaces
            .iter()
            .map(|w| WorkspaceView {
                id: w.id,
                name: w.name.clone(),
                dir: w.dir.clone(),
                branch: git_branch(&w.dir),
                tabs: w
                    .tabs
                    .iter()
                    .map(|t| TabView {
                        id: t.id,
                        name: t.name.clone(),
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

    /// Live panes that have been identified as agents, with their kind and pty —
    /// the targets of the state-classification pass.
    pub fn agent_panes(&self) -> Vec<(PaneId, AgentKind, Arc<PtyPane>)> {
        self.panes
            .iter()
            .filter(|(_, s)| s.meta.exited.is_none())
            .filter_map(|(id, s)| {
                s.meta
                    .agent
                    .clone()
                    .map(|agent| (*id, agent, Arc::clone(&s.pty)))
            })
            .collect()
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
        spec: PtySpec,
        title: String,
        split: Option<(PaneId, Direction)>,
    ) -> Result<PaneId> {
        let pty = Arc::new(PtyPane::spawn(spec, self.size).context("spawn pty")?);
        let id = self.ids.pane();
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
                },
                pty,
                tab,
            },
        );
        Ok(id)
    }
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
    use super::{git_branch, parse_head};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

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
