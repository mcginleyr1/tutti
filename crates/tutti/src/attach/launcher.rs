//! The agent launcher (`Mode::Launcher`, and `Mode::LauncherCommand` for its
//! free-form input): the selectable "what should run here?" rows, where the
//! pick runs, and the frames that start it. The row model is pure — no
//! rendering, no app state and no machine probes: which agents are installed,
//! and the argv each runs as there, is the daemon's machine's to say
//! (`host::Launcher`) — so the row construction and the selection arithmetic
//! are unit-tested in isolation; the open launcher's state (`Launcher`) and
//! `App`'s keys over it follow.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tutti_agents::{Registry, ResumeSession};
use tutti_core::{Frame as WireFrame, PaneId, Request, WorkspaceId};

use super::app::{App, Mode, control};
use super::setup;
use crate::integrations::{Integration, Plan, Status};
use crate::issues::{self, Issue};

/// What a launcher row runs when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchKind {
    /// Exec this agent — a `PaneRun` carrying its argv: the binary alone for
    /// most (`[claude]`), plus the spec's launch args for one that needs a
    /// subcommand to open its agent (`[ori, code]`).
    Agent(Vec<String>),
    /// The user's login shell — today's default pane.
    Shell,
    /// Prompt for an arbitrary command line, run via `$SHELL -lc <input>`.
    Command,
    /// Resume a harvested conversation — a `PaneRun` carrying the full argv
    /// (e.g. `claude --resume <session-id>`).
    Resume(Vec<String>),
}

/// One row in the launcher overlay: its two-tone label, what it runs, and
/// whether it can be selected — every fixed row can; an agent row only when its
/// binary was found on the daemon's machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherRow {
    pub name: String,
    pub role: String,
    pub kind: LaunchKind,
    pub available: bool,
}

impl LauncherRow {
    /// Whether the row can be launched (an absent agent binary cannot).
    pub fn selectable(&self) -> bool {
        self.available
    }
}

/// Build the launcher rows: the agents `installed` names first (registry
/// order, bold name, the product name as the dim role), each running the argv
/// its machine found it as, then the two fixed rows — the login `shell` and
/// the free-form command entry — then the rest of the catalog dim and
/// unselectable at the foot, each showing its project link so the picker
/// doubles as "what else is out there". Resume rows are inserted between the
/// fixed rows and the catalog by the caller.
pub fn build_rows(
    registry: &Registry,
    installed: &BTreeMap<String, Vec<String>>,
    shell: &str,
) -> Vec<LauncherRow> {
    let argv = |spec: &tutti_agents::AgentSpec| installed.get(&spec.kind.to_string()).cloned();
    let agent_row = |spec: &tutti_agents::AgentSpec, available: bool| {
        let role = if available {
            spec.display.clone()
        } else {
            format!("{} · {}", spec.display, short_url(&spec.url))
        };
        LauncherRow {
            name: spec.kind.to_string(),
            role,
            kind: LaunchKind::Agent(argv(spec).unwrap_or_else(|| spec.argv(None))),
            available,
        }
    };
    let installed = |spec: &&tutti_agents::AgentSpec| argv(spec).is_some();

    let mut rows: Vec<LauncherRow> = registry
        .specs()
        .iter()
        .filter(installed)
        .map(|spec| agent_row(spec, true))
        .collect();
    rows.push(LauncherRow {
        name: "shell".into(),
        role: shell.into(),
        kind: LaunchKind::Shell,
        available: true,
    });
    rows.push(LauncherRow {
        name: "command…".into(),
        role: "type any command".into(),
        kind: LaunchKind::Command,
        available: true,
    });
    rows.extend(
        registry
            .specs()
            .iter()
            .filter(|spec| !installed(spec))
            .map(|spec| agent_row(spec, false)),
    );
    rows
}

/// The rows for a launch into a sandbox: every catalog agent, selectable in
/// registry order — whether an image carries one can't be probed from here —
/// then the fixed rows, which run `sh` there.
pub fn sandbox_rows(registry: &Registry) -> Vec<LauncherRow> {
    registry
        .specs()
        .iter()
        .map(|spec| LauncherRow {
            name: spec.kind.to_string(),
            role: spec.display.clone(),
            kind: LaunchKind::Agent(spec.argv(None)),
            available: true,
        })
        .chain([
            LauncherRow {
                name: "shell".into(),
                role: "sh".into(),
                kind: LaunchKind::Shell,
                available: true,
            },
            LauncherRow {
                name: "command…".into(),
                role: "type any command".into(),
                kind: LaunchKind::Command,
                available: true,
            },
        ])
        .collect()
}

/// The rows of a launcher armed to deploy an issue: the rows of the agents
/// that take a first message (a shell, command or resume row names no agent),
/// still dim when not installed. The pick hands its agent the prompt
/// (`issues::deploy_argv`).
pub fn armed_rows(rows: Vec<LauncherRow>, registry: &Registry) -> Vec<LauncherRow> {
    rows.into_iter()
        .filter(|row| {
            matches!(row.kind, LaunchKind::Agent(_))
                && registry
                    .spec(&row.name.as_str().into())
                    .is_some_and(|spec| spec.prompt_flags.is_some())
        })
        .collect()
}

/// Where the caller inserts rows that must stay above the dim uninstalled
/// catalog (the resume rows): right before the first unavailable row, or the
/// end when everything is installed.
pub fn catalog_start(rows: &[LauncherRow]) -> usize {
    rows.iter().position(|r| !r.available).unwrap_or(rows.len())
}

/// A URL shortened for a dim launcher row: scheme and `www.` stripped.
fn short_url(url: &str) -> &str {
    let url = url.strip_prefix("https://").unwrap_or(url);
    url.strip_prefix("www.").unwrap_or(url)
}

/// Rows for harvested conversations, appended below the fixed rows — the
/// "pick up where you left off" foot of the picker. Each shows its agent, a
/// compact age, and the conversation's first prompt, and is selectable only
/// when its agent's own row is (resuming claude without claude installed
/// cannot work). `now` is passed in so the age labels are testable.
pub fn resume_rows(
    sessions: &[ResumeSession],
    rows: &[LauncherRow],
    now: SystemTime,
) -> Vec<LauncherRow> {
    sessions
        .iter()
        .map(|s| {
            let available = rows.iter().any(|r| r.name == s.agent && r.available);
            let title = s.title.as_deref().unwrap_or("untitled");
            LauncherRow {
                name: "resume".into(),
                role: format!(
                    "{} · {} · {}",
                    s.agent,
                    age_label(s.last_active, now),
                    truncate(title, 28)
                ),
                kind: LaunchKind::Resume(s.cmd.clone()),
                available,
            }
        })
        .collect()
}

/// A compact age for a resume row: `now`, `Nm`, `Nh`, or `Nd`.
fn age_label(then: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(then).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `text` capped to `max` characters, the last one replaced by `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.into();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The first selectable row's index, or 0 when none is (there is always at
/// least the shell row, so this only falls back for an empty list).
pub fn first_selectable(rows: &[LauncherRow]) -> usize {
    rows.iter().position(LauncherRow::selectable).unwrap_or(0)
}

/// The next selectable row after `from`, skipping unavailable rows; stays on
/// `from` when nothing selectable lies ahead.
pub fn next_selectable(rows: &[LauncherRow], from: usize) -> usize {
    let mut i = from;
    while i + 1 < rows.len() {
        i += 1;
        if rows[i].selectable() {
            return i;
        }
    }
    from
}

/// The previous selectable row before `from`, skipping unavailable rows; stays
/// on `from` when nothing selectable lies behind.
pub fn prev_selectable(rows: &[LauncherRow], from: usize) -> usize {
    let mut i = from;
    while i > 0 {
        i -= 1;
        if rows[i].selectable() {
            return i;
        }
    }
    from
}

/// The open launcher's state: what it lists and what its pick is armed to
/// do. Closing the launcher drops it all.
#[derive(Default)]
pub(super) struct Launcher {
    /// The launcher rows, built when the launcher opens so render and dispatch
    /// read one list.
    pub(super) rows: Vec<LauncherRow>,
    /// The highlighted launcher row.
    pub(super) selected: usize,
    /// Whether the launcher opened right after add-project — `esc` there spawns
    /// the shell into the new workspace (the old outcome) rather than closing.
    after_add: bool,
    /// The command line typed in the launcher's `command…` input.
    pub(super) command: String,
    /// When set, the next launch first creates a fresh tab in this workspace
    /// (`TabNew` then the run): the sidebar's new-agent flow, where an agent
    /// gets its own tab rather than splitting an existing one.
    pub(super) new_tab: Option<WorkspaceId>,
    /// When set, the next launch replaces this exited pane: run in the corpse's
    /// tab, then kill the corpse. Armed by `r` on a focused exited pane.
    replace: Option<PaneId>,
    /// The name of the workspace the open launcher targets, shown in its title
    /// so the choice's destination is never ambiguous. `None` renders a bare
    /// ` run ` title.
    target: Option<String>,
    /// The open launcher's directory, kept so switching where it runs can
    /// rebuild the local rows (and their resume rows).
    dir: Option<PathBuf>,
    /// Where the open launcher runs its pick: `None` locally, else the index
    /// of a configured sandbox profile. `Tab` cycles it.
    sandbox: Option<usize>,
    /// The issue the open launcher deploys, with the prompt it renders to —
    /// armed from the issues panel: the agent rows run on that prompt, and
    /// the pick deploys rather than runs.
    pub(super) issue: Option<(Issue, String)>,
    /// What the open launcher is asking before it runs the row at this
    /// index: the agent picked has none of tutti's hooks, in this project or
    /// for the user, and this plan installs them into the project.
    offer: Option<(usize, Integration, Plan)>,
    /// The hooks a project went without when asked, by its directory: the
    /// question comes once an attach.
    declined: HashSet<(PathBuf, Integration)>,
}

impl App {
    /// Open the launcher over an exited pane's workspace, armed to replace the
    /// corpse — resume rows surface first, so a dead agent is one `r` and an
    /// enter from picking its conversation back up.
    pub(super) fn relaunch_exited(&mut self, pane: PaneId) -> Vec<WireFrame> {
        let Some(tab) = self.tab_of_pane(pane) else {
            return Vec::new();
        };
        let Some((name, dir)) = self
            .workspaces
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == tab))
            .map(|w| (w.name.clone(), w.dir.clone()))
        else {
            return Vec::new();
        };
        self.open_launcher(false, Some(name), Some(&dir));
        self.launcher.replace = Some(pane);
        // A sandbox corpse relaunches where it ran.
        let profile = self.panes.get(&pane).and_then(|p| p.info.sandbox.clone());
        self.launcher.sandbox =
            profile.and_then(|name| self.config.sandboxes.iter().position(|p| p.name == name));
        if self.launcher.sandbox.is_some() {
            self.rebuild_launcher();
        }
        Vec::new()
    }

    /// Open the agent launcher — the "what should run here?" picker — building
    /// its rows from the registry and the live PATH. `after_add` records that it
    /// fired right after add-project, so `esc` still spawns the shell into the
    /// new workspace (today's outcome); otherwise `esc` just closes. `target`
    /// names the workspace the choice will land in, shown in the panel title;
    /// `dir` is its directory — when known, conversations harvested from the
    /// agent tools' own session stores append as resume rows at the foot.
    /// It opens armed with nothing, whatever an earlier launcher left: one
    /// left by a click or a frame was never closed.
    pub(super) fn open_launcher(
        &mut self,
        after_add: bool,
        target: Option<String>,
        dir: Option<&Path>,
    ) {
        self.status = None;
        self.launcher.dir = dir.map(Path::to_path_buf);
        self.launcher.sandbox = None;
        self.launcher.issue = None;
        self.launcher.offer = None;
        self.rebuild_launcher();
        self.launcher.after_add = after_add;
        self.launcher.command.clear();
        self.launcher.new_tab = None;
        self.launcher.replace = None;
        self.launcher.target = target;
        self.mode = Mode::Launcher;
    }

    /// Arm the open launcher to deploy `issue`: only its agent rows stay, each
    /// to run on the issue's `prompt`, and the pick deploys rather than runs.
    pub(super) fn arm_launcher(&mut self, issue: Issue, prompt: String) {
        self.launcher.issue = Some((issue, prompt));
        let rows = std::mem::take(&mut self.launcher.rows);
        self.launcher.rows = armed_rows(rows, &Registry::default());
        self.launcher.selected = first_selectable(&self.launcher.rows);
    }

    /// Build the rows for where the launcher runs its pick. On the daemon's
    /// machine: the agents found on its PATH, with resume rows for the
    /// directory. In a sandbox: the whole catalog, since what an image carries
    /// can't be probed from here, and no resume rows — those conversations
    /// live on the daemon's machine. Armed with an issue, only the agents,
    /// each on the issue's prompt. A machine that cannot say offers its shell
    /// and the command row alone.
    fn rebuild_launcher(&mut self) {
        let registry = Registry::default();
        let rows = match self.launcher.sandbox {
            Some(_) => sandbox_rows(&registry),
            None => {
                let found = self
                    .host
                    .launcher(self.launcher.dir.as_deref())
                    .unwrap_or_else(|e| {
                        self.set_status(format!("error: {e:#}"));
                        crate::host::Launcher {
                            installed: Default::default(),
                            resume: Vec::new(),
                        }
                    });
                let mut rows = build_rows(&registry, &found.installed, &self.shell);
                let resume = resume_rows(&found.resume, &rows, std::time::SystemTime::now());
                // Resume rows sit between the fixed rows and the dim
                // uninstalled catalog, so the actionable part of the picker
                // stays together.
                let at = catalog_start(&rows);
                rows.splice(at..at, resume);
                rows
            }
        };
        self.launcher.rows = match &self.launcher.issue {
            Some(_) => armed_rows(rows, &registry),
            None => rows,
        };
        self.launcher.selected = first_selectable(&self.launcher.rows);
    }

    /// The sandbox profile the open launcher runs its pick in, if any.
    pub(super) fn launcher_profile(&self) -> Option<&str> {
        self.launcher
            .sandbox
            .and_then(|i| self.config.sandboxes.get(i))
            .map(|p| p.name.as_str())
    }

    /// What `Tab` in the launcher switches to — `sandbox <name>` or `local` —
    /// for its hint row; `None` when no sandbox is configured.
    pub fn launcher_tab_hint(&self) -> Option<String> {
        let next = self.launcher.sandbox.map_or(0, |i| i + 1);
        (!self.config.sandboxes.is_empty()).then(|| match self.config.sandboxes.get(next) {
            Some(profile) => format!("sandbox {}", profile.name),
            None => "local".into(),
        })
    }

    /// The launcher rows as they currently render, for the renderer.
    pub fn launcher_rows(&self) -> &[LauncherRow] {
        &self.launcher.rows
    }

    /// The launcher panel title, naming the workspace the choice will run in so
    /// the target is never ambiguous — ` run in <name> `, or a bare ` run ` when
    /// the target is unknown, ` deploy <id> · <title> ` when armed with an
    /// issue — and the sandbox, when it runs in one.
    pub fn launcher_title(&self) -> String {
        let place = match (&self.launcher.issue, &self.launcher.target) {
            (Some((issue, _)), _) => format!(
                " deploy {} · {}",
                issues::label(&issue.id),
                truncate(&issue.title, 40)
            ),
            (None, Some(name)) => format!(" run in {name}"),
            (None, None) => " run".into(),
        };
        match self.launcher_profile() {
            Some(profile) => format!("{place} · sandbox {profile} "),
            None => format!("{place} "),
        }
    }

    pub(super) fn on_key_launcher(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if let Some((idx, integration, plan)) = self.launcher.offer.take() {
            return self.answer_hooks_offer(key, idx, integration, &plan);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.launcher.selected =
                    next_selectable(&self.launcher.rows, self.launcher.selected);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.launcher.selected =
                    prev_selectable(&self.launcher.rows, self.launcher.selected);
                Vec::new()
            }
            // Quick-select launches the numbered row outright.
            KeyCode::Char(c @ '1'..='9') => self.launch_index(c as usize - '1' as usize),
            KeyCode::Enter => self.launch_index(self.launcher.selected),
            // Where the pick runs: local, then each sandbox profile, round.
            KeyCode::Tab if !self.config.sandboxes.is_empty() => {
                let next = self.launcher.sandbox.map_or(0, |i| i + 1);
                self.launcher.sandbox = (next < self.config.sandboxes.len()).then_some(next);
                self.rebuild_launcher();
                Vec::new()
            }
            KeyCode::Esc => {
                if self.launcher.after_add {
                    // Backing out keeps today's outcome: a local shell.
                    self.launcher.sandbox = None;
                    self.launch_shell_and_close()
                } else {
                    // An armed launcher backs out to the issues panel.
                    let armed = self.launcher.issue.is_some();
                    self.close_launcher();
                    if armed {
                        self.mode = Mode::Issues;
                    }
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Launch the row at `idx` when it exists and is selectable; an out-of-range
    /// or unavailable quick-select is ignored. An agent/shell spawns a pane and
    /// closes the launcher; `command…` opens the free-form input instead.
    fn launch_index(&mut self, idx: usize) -> Vec<WireFrame> {
        let (name, kind) = match self.launcher.rows.get(idx) {
            Some(row) if row.selectable() => (row.name.clone(), row.kind.clone()),
            _ => return Vec::new(),
        };
        if let LaunchKind::Agent(cmd) | LaunchKind::Resume(cmd) = &kind
            && let Some((integration, plan)) = self.hooks_offer(&name, cmd)
        {
            self.launcher.offer = Some((idx, integration, plan));
            return Vec::new();
        }
        match kind {
            LaunchKind::Agent(cmd) | LaunchKind::Resume(cmd) => match self.launcher.issue.take() {
                Some((issue, prompt)) => self.deploy_frames(&issue, &name, &prompt, cmd),
                // An agent row is named for its agent; a resume row's
                // command names it.
                None => {
                    let source = self.config.sources.get(&name).cloned();
                    let source = source.or_else(|| self.config.source(&cmd));
                    self.launch_frames(cmd, source)
                }
            },
            LaunchKind::Shell => self.launch_shell_and_close(),
            LaunchKind::Command => {
                self.launcher.command.clear();
                self.mode = Mode::LauncherCommand;
                Vec::new()
            }
        }
    }

    /// What to ask before running an agent — the row `name`, its command
    /// `cmd` — in the launcher's project: whether to install tutti's hooks
    /// for it there, when the project has none and neither has the user.
    /// Without them tutti reads the agent's state off its screen and cannot
    /// bring its conversation back. A sandbox's agent and a deploy's are not
    /// asked about, nor a project that already said no.
    fn hooks_offer(&self, name: &str, cmd: &[String]) -> Option<(Integration, Plan)> {
        let program = Path::new(cmd.first()?).file_name()?.to_str()?;
        let integration = Integration::of_agent(name).or(Integration::of_agent(program))?;
        let dir = self.launcher.dir.clone()?;
        let settled = self.launcher.sandbox.is_some()
            || self.launcher.issue.is_some()
            || self.launcher.declined.contains(&(dir.clone(), integration));
        if settled {
            return None;
        }
        let missing = |project| {
            let plans = self.host.setup_plans(vec![integration], project);
            let plan = plans.ok()?.pop()?.ok()?;
            matches!(plan.status, Status::Missing(_)).then_some(plan)
        };
        let plan = missing(Some(dir))?;
        missing(None).map(|_| (integration, plan))
    }

    /// A key at the hooks question: `y` or enter installs them into the
    /// project and runs the pick, `n` runs it without, and any other goes
    /// back to the list. Either answer settles it for this project.
    fn answer_hooks_offer(
        &mut self,
        key: KeyEvent,
        idx: usize,
        integration: Integration,
        plan: &Plan,
    ) -> Vec<WireFrame> {
        let install = match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => true,
            KeyCode::Char('n' | 'N') => false,
            _ => return Vec::new(),
        };
        if let Some(dir) = self.launcher.dir.clone() {
            self.launcher.declined.insert((dir, integration));
        }
        let installed = install.then(|| self.host.setup_apply(plan));
        if let Some(Err(e)) = &installed {
            self.set_status(format!("error: {e:#}"));
            return Vec::new();
        }
        let frames = self.launch_index(idx);
        if installed.is_some() {
            let (label, path) = (setup::label(integration), plan.path.display());
            self.set_status(format!("{label} installed into {path}"));
        }
        frames
    }

    /// The hooks question the launcher is asking, for the renderer: what it
    /// would install, and the file it would write under the project.
    pub fn launcher_offer(&self) -> Option<(&'static str, String)> {
        let (_, integration, plan) = self.launcher.offer.as_ref()?;
        let dir = self.launcher.dir.as_deref().unwrap_or(Path::new(""));
        let file = plan.path.strip_prefix(dir).unwrap_or(&plan.path);
        Some((setup::label(*integration), file.display().to_string()))
    }

    fn launch_shell_and_close(&mut self) -> Vec<WireFrame> {
        self.launch_frames(vec![self.launcher_shell()], None)
    }

    /// The shell the launcher's shell and command rows run: the login shell
    /// locally, `sh` in a sandbox — the one shell every image carries.
    fn launcher_shell(&self) -> String {
        match self.launcher.sandbox {
            Some(_) => "sh".into(),
            None => self.shell.clone(),
        }
    }

    /// Close the launcher and produce the frames that start `cmd` where it was
    /// aimed: a `TabNew` first when armed to give the choice its own tab, a run
    /// into an exited pane's tab followed by that corpse's kill when armed to
    /// replace it, else a plain run into the current tab. Requests share one
    /// ordered connection, so the run always lands after the tab exists and
    /// before the corpse goes. `cmd` starts behind `source`, the file its
    /// agent's config names — locally: a sandbox cannot read it.
    fn launch_frames(&mut self, cmd: Vec<String>, source: Option<PathBuf>) -> Vec<WireFrame> {
        let new_tab = self.launcher.new_tab.take();
        let replace = self.launcher.replace.take();
        let sandbox = self.launcher_profile().map(String::from);
        let source = source.filter(|_| sandbox.is_none());
        self.close_launcher();
        // A persistent pane in `tab`, else the session's current one: the
        // just-created workspace after add-project, the active tab for
        // prefix-`r`.
        let run = |tab| {
            control(&Request::PaneRun {
                tab,
                cmd,
                ephemeral: false,
                sandbox,
                source,
            })
        };
        if let Some(workspace) = new_tab {
            let tab = Request::TabNew {
                workspace: Some(workspace),
            };
            return vec![control(&tab), run(None)];
        }
        if let Some(corpse) = replace
            && let Some(tab) = self.tab_of_pane(corpse)
        {
            return vec![run(Some(tab)), control(&Request::PaneKill { pane: corpse })];
        }
        vec![run(None)]
    }

    /// Dismiss the launcher back to terminal mode, dropping its transient state.
    pub(super) fn close_launcher(&mut self) {
        self.mode = Mode::Terminal;
        // What a project said no to outlives the launcher that asked.
        let declined = std::mem::take(&mut self.launcher.declined);
        self.launcher = Launcher {
            declined,
            ..Launcher::default()
        };
        self.status = None;
    }

    pub(super) fn on_key_launcher_command(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.launcher.command.push(c);
                Vec::new()
            }
            KeyCode::Backspace => {
                self.launcher.command.pop();
                Vec::new()
            }
            // Esc backs up to the picker rather than closing outright.
            KeyCode::Esc => {
                self.launcher.command.clear();
                self.mode = Mode::Launcher;
                Vec::new()
            }
            KeyCode::Enter => {
                let input = self.launcher.command.trim().to_string();
                if input.is_empty() {
                    return Vec::new();
                }
                self.launch_frames(vec![self.launcher_shell(), "-lc".into(), input], None)
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::*;
    use crate::config::Config;
    use tutti_core::TabId;

    fn agent_row(name: &str, available: bool) -> LauncherRow {
        LauncherRow {
            name: name.into(),
            role: name.into(),
            kind: LaunchKind::Agent(vec![name.into()]),
            available,
        }
    }

    /// What a host's launcher answer names installed: each kind with its argv.
    fn installed(argvs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        argvs
            .iter()
            .map(|(kind, argv)| {
                (
                    kind.to_string(),
                    argv.iter().map(|a| a.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn build_rows_lists_agents_then_shell_and_command_with_availability() {
        // Only `claude` is installed; `codex` stays unavailable.
        let rows = build_rows(
            &Registry::default(),
            &installed(&[("claude", &["claude"])]),
            "/bin/zsh",
        );
        // Installed agents lead, the fixed rows follow, and the rest of the
        // catalog sits dim at the foot with its project links.
        assert_eq!(rows[0].name, "claude");
        assert!(rows[0].available, "claude is installed");
        assert_eq!(
            rows[0].role, "Claude Code",
            "an installed row shows the product name"
        );
        assert_eq!(rows[0].kind, LaunchKind::Agent(vec!["claude".into()]));
        assert!(matches!(rows[1].kind, LaunchKind::Shell));
        assert_eq!(
            rows[1].role, "/bin/zsh",
            "the shell row names the host's shell"
        );
        assert!(matches!(rows[2].kind, LaunchKind::Command));
        assert!(
            rows[1].available && rows[2].available,
            "the fixed rows are always selectable"
        );
        assert_eq!(
            catalog_start(&rows),
            3,
            "the dim catalog starts after the fixed rows"
        );
        assert_eq!(
            rows.len(),
            Registry::default().specs().len() + 2,
            "every catalog agent gets a row"
        );
        let codex = rows[3..].iter().find(|r| r.name == "codex").unwrap();
        assert!(!codex.available, "codex is not installed");
        assert_eq!(
            codex.role, "Codex CLI · github.com/openai/codex",
            "an uninstalled row links to the project, scheme stripped"
        );
        assert!(
            rows[3..].iter().all(|r| !r.available),
            "everything after the fixed rows is the uninstalled catalog"
        );
    }

    #[test]
    fn sandbox_rows_offer_the_whole_catalog_then_sh() {
        let registry = Registry::default();
        let rows = sandbox_rows(&registry);
        assert_eq!(rows.len(), registry.specs().len() + 2);
        assert!(
            rows.iter().all(LauncherRow::selectable),
            "nothing can be probed in an image, so nothing is dimmed"
        );
        assert_eq!(rows[0].name, "claude");
        assert_eq!(rows[0].kind, LaunchKind::Agent(vec!["claude".into()]));
        let ori = rows.iter().find(|r| r.name == "ori").unwrap();
        assert_eq!(
            ori.kind,
            LaunchKind::Agent(vec!["ori".into(), "code".into()])
        );
        let shell = &rows[rows.len() - 2];
        assert_eq!(
            (shell.kind.clone(), shell.role.as_str()),
            (LaunchKind::Shell, "sh")
        );
        assert_eq!(rows[rows.len() - 1].kind, LaunchKind::Command);
    }

    /// An installed row runs the argv its machine found it as — Cursor by the
    /// `agent` path checked there — and an uninstalled one the spec's own,
    /// launch args included.
    #[test]
    fn an_agent_row_runs_the_argv_its_machine_found() {
        let cursor = "/home/u/.local/bin/agent";
        let rows = build_rows(
            &Registry::default(),
            &installed(&[("cursor", &[cursor])]),
            "/bin/sh",
        );
        assert_eq!(rows[0].name, "cursor");
        assert_eq!(rows[0].kind, LaunchKind::Agent(vec![cursor.into()]));
        let ori = rows.iter().find(|r| r.name == "ori").unwrap();
        assert!(!ori.available);
        assert_eq!(
            ori.kind,
            LaunchKind::Agent(vec!["ori".into(), "code".into()]),
            "bare `ori` only prints its help; the row opens its agent"
        );
    }

    /// Armed to deploy, the picker keeps only the agents that take a first
    /// message, each with its own argv, an uninstalled one still dim.
    #[test]
    fn armed_rows_keep_the_agents_that_take_a_prompt() {
        let registry = Registry::default();
        let rows = vec![
            agent_row("claude", true),
            agent_row("crush", true),
            agent_row("codex", false),
            LauncherRow {
                name: "shell".into(),
                role: "zsh".into(),
                kind: LaunchKind::Shell,
                available: true,
            },
            LauncherRow {
                name: "resume".into(),
                role: "claude · 2h · old".into(),
                kind: LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "a".into()]),
                available: true,
            },
        ];
        let armed = armed_rows(rows, &registry);
        let names: Vec<&str> = armed.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["claude", "codex"], "crush takes no prompt");
        assert_eq!(armed[0].kind, LaunchKind::Agent(vec!["claude".into()]));
        assert!(armed[0].selectable());
        assert!(!armed[1].selectable(), "codex is not installed");
    }

    #[test]
    fn resume_rows_label_age_and_gate_on_the_agents_availability() {
        use std::time::Duration;
        let now = SystemTime::now();
        let sessions = vec![
            ResumeSession {
                agent: "claude".into(),
                id: "abc".into(),
                title: Some("fix the sidebar filtering because it is way too long".into()),
                last_active: now - Duration::from_secs(2 * 3600),
                cmd: vec!["claude".into(), "--resume".into(), "abc".into()],
            },
            ResumeSession {
                agent: "codex".into(),
                id: "def".into(),
                title: None,
                last_active: now - Duration::from_secs(30),
                cmd: vec!["codex".into(), "resume".into()],
            },
        ];
        let fixed = vec![agent_row("claude", true), agent_row("codex", false)];

        let rows = resume_rows(&sessions, &fixed, now);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "resume");
        assert_eq!(rows[0].role, "claude · 2h · fix the sidebar filtering b…");
        assert_eq!(
            rows[0].kind,
            LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "abc".into()])
        );
        assert!(rows[0].available, "claude is installed, so resumable");
        assert_eq!(rows[1].role, "codex · now · untitled");
        assert!(
            !rows[1].available,
            "a conversation for an absent binary cannot resume"
        );
    }

    #[test]
    fn navigation_skips_unavailable_rows() {
        let rows = vec![
            agent_row("a", true),
            agent_row("b", false),
            agent_row("c", true),
        ];
        assert_eq!(first_selectable(&rows), 0);
        assert_eq!(
            next_selectable(&rows, 0),
            2,
            "j jumps past the unavailable middle row"
        );
        assert_eq!(prev_selectable(&rows, 2), 0, "k jumps back past it");
        assert_eq!(next_selectable(&rows, 2), 2, "stays put at the end");
        assert_eq!(prev_selectable(&rows, 0), 0, "stays put at the start");
    }

    #[test]
    fn sidebar_n_launch_creates_a_tab_first_so_the_agent_never_splits() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // select web (workspace 2)
        app.on_key(plain('n'));
        assert_eq!(app.mode, Mode::Launcher);
        // Pin the rows so the test is independent of what PATH carries.
        app.launcher.rows = vec![LauncherRow {
            name: "claude".into(),
            role: "Claude Code".into(),
            kind: LaunchKind::Agent(vec!["claude".into()]),
            available: true,
        }];
        app.launcher.selected = 0;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![
                control(&Request::TabNew {
                    workspace: Some(WorkspaceId(2)),
                }),
                control(&Request::PaneRun {
                    tab: None,
                    cmd: vec!["claude".into()],
                    ephemeral: false,
                    sandbox: None,
                    source: None,
                }),
            ],
            "the run follows a TabNew on the same ordered connection, so it \
             lands in the fresh tab instead of splitting the current one"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn escaping_the_new_agent_launcher_disarms_the_tab_creation() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('n'));
        assert_eq!(app.mode, Mode::Launcher);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Terminal);
        // A later launch through another door runs plainly into the current tab.
        app.on_key(ctrl('b'));
        app.on_key(plain('r'));
        app.launcher.rows = vec![LauncherRow {
            name: "claude".into(),
            role: "Claude Code".into(),
            kind: LaunchKind::Agent(vec!["claude".into()]),
            available: true,
        }];
        app.launcher.selected = 0;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["claude".into()],
                ephemeral: false,
                sandbox: None,
                source: None,
            })],
            "esc dropped the armed TabNew; C-b r still runs in place"
        );
    }

    #[test]
    fn r_on_an_exited_pane_relaunches_into_its_tab_and_drops_the_corpse() {
        let mut app = App::new();
        attach_with(&mut app, view_one_exited_agent());
        assert_eq!(app.focused, Some(PaneId(1)));
        let out = app.on_key(plain('r'));
        assert!(out.is_empty(), "r opens the launcher, sending nothing yet");
        assert_eq!(app.mode, Mode::Launcher);
        app.launcher.rows = vec![LauncherRow {
            name: "resume".into(),
            role: "claude · now · fix the thing".into(),
            kind: LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "abc".into()]),
            available: true,
        }];
        app.launcher.selected = 0;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![
                control(&Request::PaneRun {
                    tab: Some(TabId(1)),
                    cmd: vec!["claude".into(), "--resume".into(), "abc".into()],
                    ephemeral: false,
                    sandbox: None,
                    source: None,
                }),
                control(&Request::PaneKill { pane: PaneId(1) }),
            ],
            "the pick runs in the corpse's tab, then the corpse goes"
        );
    }

    #[test]
    fn r_on_an_exited_sandbox_pane_relaunches_in_its_sandbox() {
        let mut app = App::with_config(two_sandboxes());
        let mut view = view_one_exited_agent();
        view[0].tabs[0].panes[0].sandbox = Some("gke".into());
        attach_with(&mut app, view);
        app.on_key(plain('r'));
        assert_eq!(app.mode, Mode::Launcher);
        assert!(
            app.launcher_title().ends_with(" · sandbox gke "),
            "the launcher opens where the corpse ran: {}",
            app.launcher_title()
        );
        let out = app.on_key(plain('1'));
        assert_eq!(
            out,
            vec![
                control(&Request::PaneRun {
                    tab: Some(TabId(1)),
                    cmd: vec!["claude".into()],
                    ephemeral: false,
                    sandbox: Some("gke".into()),
                    source: None,
                }),
                control(&Request::PaneKill { pane: PaneId(1) }),
            ]
        );
    }

    fn login_shell() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }

    #[test]
    fn prefix_r_opens_the_launcher_in_both_presets() {
        for cfg in [Config::default(), vim_config()] {
            let mut app = App::with_config(cfg);
            attached(&mut app);
            app.on_key(ctrl('b'));
            let out = app.on_key(plain('r'));
            assert!(out.is_empty(), "opening the launcher emits no frames");
            assert_eq!(app.mode, Mode::Launcher);
            // The rows are the registry agents plus the shell and command entries.
            let names: Vec<&str> = app.launcher.rows.iter().map(|r| r.name.as_str()).collect();
            assert!(names.contains(&"claude"), "registry agents seed the rows");
            assert!(names.contains(&"shell"), "the shell row is always present");
            assert!(names.contains(&"command…"), "the command row too");
        }
    }

    #[test]
    fn launcher_enter_on_an_agent_row_emits_a_panerun() {
        let mut app = App::new();
        attached(&mut app);
        // Drive the launcher into a known state with one available agent row.
        app.launcher.rows = vec![LauncherRow {
            name: "claude".into(),
            role: "Claude Code".into(),
            kind: LaunchKind::Agent(vec!["claude".into()]),
            available: true,
        }];
        app.launcher.selected = 0;
        app.mode = Mode::Launcher;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["claude".into()],
                ephemeral: false,
                sandbox: None,
                source: None,
            })],
            "enter launches the selected agent in a new pane"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn launcher_enter_on_a_resume_row_emits_the_resume_argv() {
        let mut app = App::new();
        attached(&mut app);
        app.launcher.rows = vec![LauncherRow {
            name: "resume".into(),
            role: "claude · 2h · fix the sidebar".into(),
            kind: LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "abc".into()]),
            available: true,
        }];
        app.launcher.selected = 0;
        app.mode = Mode::Launcher;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["claude".into(), "--resume".into(), "abc".into()],
                ephemeral: false,
                sandbox: None,
                source: None,
            })],
            "enter resumes the conversation in a new pane"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    /// An agent with a `source` file in the config starts behind it: its own
    /// row by the row's name (Cursor's runs a path that names nobody), a
    /// resume row by its command. A shell, a command line and a sandbox —
    /// which cannot read the file — get none.
    /// Picking an agent for a project with none of tutti's hooks for it, the
    /// user having none either, asks first: `y` installs them into the
    /// project and runs the pick, `n` runs it without and settles it for
    /// that project, and any other key goes back to the list. A resume row
    /// is asked about by its command's agent. An agent tutti ships nothing
    /// for is not asked about, nor one whose hooks the user has.
    #[test]
    fn picking_an_agent_asks_to_install_a_projects_missing_hooks() {
        use crate::integrations::{self, Dirs};
        let tree = temp_tree(&["home", "api", "web"]);
        let (home, api, web) = (tree.join("home"), tree.join("api"), tree.join("web"));
        let user = Dirs {
            home: Some(home.clone()),
            ..Dirs::default()
        };
        let mut app = App::new();
        app.set_local(|local| local.dirs = user.clone());
        attached(&mut app);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let pick = |app: &mut App, dir: &Path, row: LauncherRow| {
            app.open_launcher(false, Some("api".into()), Some(dir));
            app.launcher.rows = vec![row];
            app.launcher.selected = 0;
            app.on_key(enter)
        };
        let claude = || agent_row("claude", true);

        assert!(pick(&mut app, &api, claude()).is_empty(), "asked first");
        let asked = ("Claude Code hooks", ".claude/settings.json".to_string());
        assert_eq!(app.launcher_offer(), Some(asked));
        assert!(app.on_key(plain('x')).is_empty());
        assert!(app.launcher_offer().is_none() && app.mode == Mode::Launcher);

        app.on_key(enter);
        let Request::PaneRun { cmd, .. } = only_request(&app.on_key(plain('y'))) else {
            panic!("y runs the pick");
        };
        assert_eq!(cmd, ["claude"]);
        let written = std::fs::read_to_string(api.join(".claude/settings.json")).unwrap();
        assert!(written.contains("tutti agent-event claude"));
        assert!(
            app.transient()
                .unwrap()
                .starts_with("Claude Code hooks installed into")
        );
        assert!(!pick(&mut app, &api, claude()).is_empty(), "they are there");

        assert!(pick(&mut app, &web, claude()).is_empty());
        assert!(!app.on_key(plain('n')).is_empty(), "n runs it without");
        assert!(!web.join(".claude").exists());
        assert!(!pick(&mut app, &web, claude()).is_empty(), "web said no");

        let resume = LauncherRow {
            name: "fix the tests".into(),
            role: "codex".into(),
            kind: LaunchKind::Resume(vec!["/opt/bin/codex".into(), "resume".into()]),
            available: true,
        };
        assert!(pick(&mut app, &web, resume).is_empty());
        assert_eq!(app.launcher_offer().unwrap().0, "Codex hooks");

        assert!(!pick(&mut app, &web, agent_row("crush", true)).is_empty());
        let gemini = Integration::GeminiHooks.plan(&user).unwrap();
        integrations::apply(&gemini).unwrap();
        assert!(!pick(&mut app, &web, agent_row("gemini", true)).is_empty());
    }

    #[test]
    fn an_agent_with_a_source_file_starts_behind_it_locally() {
        let sourced = "[agents.claude]\nsource = \"/h/secrets.sh\"\n\n\
                       [agents.cursor]\nsource = \"/h/cursor.sh\"\n\n\
                       [[sandboxes]]\nname = \"orb\"\nimage = \"i\"\n";
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let launched = |row: LauncherRow, sandbox: Option<usize>| {
            let mut app = App::with_config(Config::parse(sourced).unwrap());
            attached(&mut app);
            app.launcher.rows = vec![row];
            app.launcher.sandbox = sandbox;
            app.mode = Mode::Launcher;
            match only_request(&app.on_key(enter)) {
                Request::PaneRun { source, .. } => source,
                other => panic!("{other:?}"),
            }
        };
        let row = |name: &str, kind: LaunchKind| LauncherRow {
            name: name.into(),
            role: name.into(),
            kind,
            available: true,
        };
        let argv = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        let claude = || row("claude", LaunchKind::Agent(argv(&["claude"])));
        let secrets = Some(PathBuf::from("/h/secrets.sh"));
        assert_eq!(launched(claude(), None), secrets);
        assert_eq!(
            launched(
                row("cursor", LaunchKind::Agent(argv(&["/u/bin/agent"]))),
                None
            ),
            Some(PathBuf::from("/h/cursor.sh"))
        );
        let resume = LaunchKind::Resume(argv(&["claude", "--resume", "abc"]));
        assert_eq!(launched(row("resume", resume), None), secrets);
        assert_eq!(launched(agent_row("codex", true), None), None);
        assert_eq!(launched(row("shell", LaunchKind::Shell), None), None);
        assert_eq!(launched(claude(), Some(0)), None, "a sandbox cannot");
    }

    #[test]
    fn launcher_appends_harvested_resume_rows_for_the_target_workspace() {
        // A fixture home holding one claude conversation for /tmp/w — the
        // directory every workspace fixture uses.
        let home = std::env::temp_dir().join(format!("tutti-app-resume-{}", std::process::id()));
        let store = home.join(".claude/projects/-tmp-w");
        std::fs::create_dir_all(&store).unwrap();
        let line = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "wire the resume rows"},
            "cwd": "/tmp/w",
        });
        std::fs::write(store.join("sess-1.jsonl"), format!("{line}\n")).unwrap();

        let mut app = App::new();
        app.set_local(|local| local.dirs.home = Some(home.clone()));
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('n')); // new agent in the selected (first) workspace
        assert_eq!(app.mode, Mode::Launcher);
        let rows = app.launcher.rows;
        let idx = rows
            .iter()
            .position(|r| r.name == "resume")
            .expect("a resume row for the harvested conversation");
        assert!(
            rows[idx].role.contains("wire the resume rows"),
            "the row carries the conversation's first prompt: {}",
            rows[idx].role
        );
        assert_eq!(
            rows[idx].kind,
            LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "sess-1".into()])
        );
        assert!(
            rows[idx + 1..].iter().all(|r| !r.available),
            "resume sits above the dim uninstalled catalog"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn launcher_quick_select_number_launches_the_shell_row() {
        let mut app = App::new();
        attached(&mut app);
        // The shell row follows the installed agents: none on an empty PATH.
        app.set_local(|local| local.path = None);
        app.open_launcher(false, None, None);
        assert_eq!(app.launcher.rows[0].kind, LaunchKind::Shell);
        let out = app.on_key(plain('1'));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec![login_shell()],
                ephemeral: false,
                sandbox: None,
                source: None,
            })],
            "the shell row's number launches it outright"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn launcher_command_row_opens_an_input_that_runs_via_shell_lc() {
        let mut app = App::new();
        attached(&mut app);
        app.open_launcher(false, None, None);
        // Walk to the command row, whatever this machine has on PATH: past
        // nine installed agents it has no quick-select number.
        let command_row = app
            .launcher
            .rows
            .iter()
            .position(|r| matches!(r.kind, LaunchKind::Command))
            .unwrap();
        for _ in 0..command_row {
            app.on_key(plain('j'));
        }
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "opening the command input emits nothing");
        assert_eq!(app.mode, Mode::LauncherCommand);

        for c in "npm test".chars() {
            app.on_key(plain(c));
        }
        assert_eq!(app.launcher.command, "npm test");
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec![login_shell(), "-lc".into(), "npm test".into()],
                ephemeral: false,
                sandbox: None,
                source: None,
            })],
            "the command runs through the login shell's -lc"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    fn two_sandboxes() -> Config {
        Config::parse(
            "[[sandboxes]]\nname = \"orb\"\nimage = \"i\"\n\n\
             [[sandboxes]]\nname = \"gke\"\nwarm_pool = \"p\"\n",
        )
        .unwrap()
    }

    #[test]
    fn launcher_tab_cycles_local_then_each_sandbox_and_launches_there() {
        let mut app = App::with_config(two_sandboxes());
        attached(&mut app);
        app.open_launcher(false, Some("api".into()), None);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.launcher_title(), " run in api ");
        assert_eq!(app.launcher_tab_hint().as_deref(), Some("sandbox orb"));

        app.on_key(tab);
        assert_eq!(app.launcher_title(), " run in api · sandbox orb ");
        assert!(
            app.launcher.rows.iter().all(LauncherRow::selectable),
            "a sandbox offers the whole catalog"
        );
        assert_eq!(app.launcher_tab_hint().as_deref(), Some("sandbox gke"));
        app.on_key(tab);
        assert_eq!(app.launcher_title(), " run in api · sandbox gke ");
        assert_eq!(app.launcher_tab_hint().as_deref(), Some("local"));
        app.on_key(tab);
        assert_eq!(app.launcher_title(), " run in api ", "and round to local");

        app.on_key(tab);
        let out = app.on_key(plain('1'));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["claude".into()],
                ephemeral: false,
                sandbox: Some("orb".into()),
                source: None,
            })],
            "the pick runs in the selected sandbox"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn a_sandbox_shell_row_runs_sh() {
        let mut app = App::with_config(two_sandboxes());
        attached(&mut app);
        app.open_launcher(false, None, None);
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let shell = app
            .launcher
            .rows
            .iter()
            .position(|r| matches!(r.kind, LaunchKind::Shell))
            .unwrap();
        app.launcher.selected = shell;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["sh".into()],
                ephemeral: false,
                sandbox: Some("orb".into()),
                source: None,
            })],
            "an image has sh, not this machine's login shell"
        );
    }

    #[test]
    fn launcher_tab_is_inert_without_sandboxes() {
        let mut app = App::new();
        attached(&mut app);
        app.open_launcher(false, Some("api".into()), None);
        assert_eq!(app.launcher_tab_hint(), None, "no hint row to show");
        let out = app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(out.is_empty());
        assert_eq!(app.launcher_title(), " run in api ");
    }

    #[test]
    fn launcher_command_esc_backs_up_to_the_picker() {
        let mut app = App::new();
        attached(&mut app);
        app.open_launcher(false, None, None);
        let command_row = app
            .launcher
            .rows
            .iter()
            .position(|r| matches!(r.kind, LaunchKind::Command))
            .unwrap();
        for _ in 0..command_row {
            app.on_key(plain('j'));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(plain('x'));
        assert_eq!(app.mode, Mode::LauncherCommand);
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty());
        assert_eq!(app.mode, Mode::Launcher, "esc steps back to the picker");
        assert!(app.launcher.command.is_empty());
    }

    #[test]
    fn launcher_esc_from_prefix_r_closes_without_spawning() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(ctrl('b'));
        app.on_key(plain('r'));
        assert_eq!(app.mode, Mode::Launcher);
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            out.is_empty(),
            "esc from a prefix-r launcher spawns nothing"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn launcher_jk_skips_unavailable_agent_rows() {
        let mut app = App::new();
        attached(&mut app);
        app.launcher.rows = vec![
            LauncherRow {
                name: "up".into(),
                role: "".into(),
                kind: LaunchKind::Agent(vec!["up".into()]),
                available: true,
            },
            LauncherRow {
                name: "gone".into(),
                role: "".into(),
                kind: LaunchKind::Agent(vec!["gone".into()]),
                available: false,
            },
            LauncherRow {
                name: "down".into(),
                role: "".into(),
                kind: LaunchKind::Agent(vec!["down".into()]),
                available: true,
            },
        ];
        app.launcher.selected = 0;
        app.mode = Mode::Launcher;
        app.on_key(plain('j'));
        assert_eq!(app.launcher.selected, 2, "j skips the unavailable row");
        app.on_key(plain('k'));
        assert_eq!(app.launcher.selected, 0, "k skips it too");
    }
}
