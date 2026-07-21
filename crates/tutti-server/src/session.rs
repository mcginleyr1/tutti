//! Server-side session state: the workspace/tab/pane tree plus one persistent
//! `PtyPane` per pane. Pure model — it owns no sockets and speaks no protocol,
//! it just answers the operations the dispatcher maps requests onto and reports
//! which panes it touched so the caller can drive the pty lifecycle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tutti_core::{
    AgentState, Direction, Layout, Pane, PaneId, PaneInfo, TabId, TabInfo, WorkspaceId,
    WorkspaceInfo,
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
    meta: Pane,
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

    pub fn pane_list(&self) -> Vec<PaneInfo> {
        let mut panes: Vec<&PaneSlot> = self.panes.values().collect();
        panes.sort_by_key(|s| s.meta.id.0);
        panes
            .into_iter()
            .map(|s| PaneInfo {
                id: s.meta.id,
                title: s.meta.title.clone(),
                agent: s.meta.agent.clone(),
                state: s.meta.state,
                exited: s.meta.exited,
            })
            .collect()
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

    /// Record a child's exit, keeping the pane and its grid readable. Returns
    /// `false` when the pane is already gone or already marked exited.
    pub fn mark_exited(&mut self, pane: PaneId, code: i32) -> bool {
        match self.panes.get_mut(&pane) {
            Some(slot) if slot.meta.exited.is_none() => {
                slot.meta.exited = Some(code);
                slot.meta.state = AgentState::Done;
                true
            }
            _ => false,
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
                meta: Pane {
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

fn pane_title(program: &str) -> String {
    std::path::Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string())
}
