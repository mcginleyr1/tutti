//! The issues panel (`Mode::Issues`) and the deploys it starts: every
//! configured tracker's issues for the active project, the launcher armed to
//! run an agent on one, the workspace and run that follow, and what the
//! landed deploy writes back to its tracker.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tutti_core::{Frame as WireFrame, PaneId, Request, WorkspaceId};

use super::app::{App, Mode};
use super::follow::FollowUp;
use super::navigate;
use crate::host::Host;
use crate::issues::{self, Issue};

/// One issues-panel row: a source still loading or failed, or an issue.
pub enum IssueRow<'a> {
    Status {
        source: &'a str,
        error: Option<&'a str>,
    },
    Issue(&'a Issue),
}

/// The issues panel's state. It outlives `Mode::Issues`: the launcher the
/// panel arms deploys into its project, a fetch lands by its generation, and
/// a write-back waits for the event loop.
#[derive(Default)]
pub(super) struct Issues {
    /// What each configured issue source's fetch returned, in config order:
    /// `None` while it loads, else its issues or why it failed.
    fetched: Vec<Option<Result<Vec<Issue>, String>>>,
    /// Bumped each time the issues panel opens, so a fetch the panel has
    /// since moved on from is dropped when it lands.
    generation: u64,
    /// The fetch an opening panel asks of the event loop: its generation and
    /// the project directory the sources run in.
    fetch: Option<(u64, PathBuf)>,
    /// What landed deploys ask the event loop to write back to trackers.
    pub(super) write_backs: Vec<WriteBack>,
    /// The project the panel lists for and deploys into, and whether it is
    /// a jj repo — so a deploy can give the issue its own workspace — probed
    /// once as the panel opens.
    project: Option<WorkspaceId>,
    pub(super) jj: bool,
    /// The panel's filter, and whether keys are typing into it.
    query: String,
    filtering: bool,
    /// The highlighted issue: an index into the filtered issues.
    selected: usize,
    /// Deploy into a fresh tab of the project itself rather than a workspace
    /// of its own — the panel's `h`.
    here: bool,
}

impl App {
    /// Open the issues panel over the active project and ask the event loop
    /// to fetch every configured source there (`take_issue_fetch`).
    pub(super) fn open_issues(&mut self) {
        let Some((id, dir)) = self
            .active_project()
            .and_then(|id| self.workspaces.iter().find(|w| w.id == id))
            .map(|w| (w.id, w.dir.clone()))
        else {
            self.set_status("add a project first".into());
            return;
        };
        self.issues.generation += 1;
        self.issues.fetched = vec![None; self.config.issues.len()];
        self.issues.jj = self.host.is_jj(&dir).unwrap_or(false);
        self.issues.fetch =
            (!self.issues.fetched.is_empty()).then_some((self.issues.generation, dir));
        self.issues.project = Some(id);
        self.issues.query.clear();
        self.issues.filtering = false;
        self.issues.selected = 0;
        self.issues.here = false;
        self.mode = Mode::Issues;
        self.status = None;
    }

    /// The fetch the issues panel asked for as it opened, for the event loop
    /// to run off its thread: the panel's generation, the project directory
    /// to run each source in, and the sources. Each result comes back through
    /// `issues_loaded`.
    pub fn take_issue_fetch(&mut self) -> Option<(u64, PathBuf, Vec<issues::Source>)> {
        let (generation, dir) = self.issues.fetch.take()?;
        Some((generation, dir, self.config.issues.clone()))
    }

    /// Land what source `index` fetched for panel `generation` — dropped when
    /// the panel has been reopened since.
    pub fn issues_loaded(
        &mut self,
        generation: u64,
        index: usize,
        fetched: Result<Vec<Issue>, String>,
    ) {
        if generation == self.issues.generation
            && let Some(slot) = self.issues.fetched.get_mut(index)
        {
            *slot = Some(fetched);
        }
    }

    /// The panel's rows, source by source in config order: one status row
    /// for a source still loading or one that failed, else the source's
    /// issues the filter matches — fuzzily over source, id, title and labels,
    /// as the go-to overlay filters.
    pub fn issue_rows(&self) -> Vec<IssueRow<'_>> {
        let matches = |i: &Issue| {
            self.issues.query.is_empty()
                || navigate::score(
                    &self.issues.query,
                    &format!("{} {} {} {}", i.source, i.id, i.title, i.labels.join(" ")),
                )
                .is_some()
        };
        self.config
            .issues
            .iter()
            .zip(&self.issues.fetched)
            .flat_map(|(source, fetched)| match fetched {
                None => vec![IssueRow::Status {
                    source: &source.name,
                    error: None,
                }],
                Some(Err(error)) => vec![IssueRow::Status {
                    source: &source.name,
                    error: Some(error),
                }],
                Some(Ok(found)) => found
                    .iter()
                    .filter(|i| matches(i))
                    .map(IssueRow::Issue)
                    .collect(),
            })
            .collect()
    }

    /// The listed issues, in row order.
    fn listed_issues(&self) -> Vec<&Issue> {
        self.issue_rows()
            .into_iter()
            .filter_map(|row| match row {
                IssueRow::Issue(issue) => Some(issue),
                IssueRow::Status { .. } => None,
            })
            .collect()
    }

    /// The highlighted issue's index into the listed ones, kept in range as
    /// fetches land and the filter narrows.
    pub fn issues_selected(&self) -> usize {
        self.issues
            .selected
            .min(self.listed_issues().len().saturating_sub(1))
    }

    /// The panel's filter text, and whether keys are typing into it.
    pub fn issues_query(&self) -> (&str, bool) {
        (&self.issues.query, self.issues.filtering)
    }

    /// Whether a deploy from the panel lands in a fresh tab of its project
    /// rather than a new workspace: `h` toggles it, and a project outside
    /// jj, which can have no workspaces, always does.
    pub fn issues_deploy_here(&self) -> bool {
        self.issues.here || !self.issues.jj
    }

    /// The issues panel's title, naming the project it lists for.
    pub fn issues_title(&self) -> String {
        match self.issues.project.and_then(|id| self.workspace_name(id)) {
            Some(name) => format!(" issues · {name} "),
            None => " issues ".into(),
        }
    }

    /// Keys in the issues panel. While the filter is typed (after `/`),
    /// characters edit it, `enter` keeps it and `esc` clears it; otherwise
    /// `j`/`k` move, `/` starts the filter, `h` toggles deploying here,
    /// `enter` opens the launcher armed for the highlighted issue, and `esc`
    /// closes. The arrows move either way.
    pub(super) fn on_key_issues(&mut self, key: KeyEvent) {
        let typing = self.issues.filtering
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let selected = self.issues_selected();
        match key.code {
            KeyCode::Esc if typing => {
                self.issues.filtering = false;
                self.issues.query.clear();
                self.issues.selected = 0;
            }
            KeyCode::Enter if typing => self.issues.filtering = false,
            KeyCode::Backspace if typing => {
                self.issues.query.pop();
                self.issues.selected = 0;
            }
            KeyCode::Char(c) if typing => {
                self.issues.query.push(c);
                self.issues.selected = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => self.issues.selected = selected + 1,
            KeyCode::Char('k') | KeyCode::Up => self.issues.selected = selected.saturating_sub(1),
            KeyCode::Char('/') => self.issues.filtering = true,
            KeyCode::Char('h') => self.issues.here = !self.issues.here,
            KeyCode::Enter => self.arm_deploy(),
            KeyCode::Esc => {
                self.mode = Mode::Terminal;
                self.status = None;
            }
            _ => {}
        }
    }

    /// Open the launcher over the panel's project armed to deploy the
    /// highlighted issue: its rows run each agent on the issue's rendered
    /// prompt, and the pick deploys (`deploy_frames`).
    fn arm_deploy(&mut self) {
        let Some(issue) = self
            .listed_issues()
            .get(self.issues_selected())
            .map(|&i| i.clone())
        else {
            return;
        };
        let Some(template) = self
            .config
            .issues
            .iter()
            .find(|s| s.name == issue.source)
            .map(|s| s.prompt.clone())
        else {
            return;
        };
        let Some((name, dir)) = self
            .issues
            .project
            .and_then(|id| self.workspaces.iter().find(|w| w.id == id))
            .map(|w| (w.name.clone(), w.dir.clone()))
        else {
            self.set_status("that project is gone".into());
            return;
        };
        let prompt = issues::prompt(&template, &issue);
        self.launcher.issue = Some((issue, prompt));
        self.open_launcher(false, Some(name), Some(&dir));
    }

    /// Close the launcher and deploy `issue` with `agent`, its row's `argv`
    /// handed `prompt` as its first message — through a file when it is long
    /// (`issues::deploy_argv`): a `Run` in a fresh tab of the panel's project
    /// when deploying here, else a workspace named for the issue first, the
    /// `Run` following its `WorkspaceCreated`. The `Ran` jumps to the agent.
    pub(super) fn deploy_frames(
        &mut self,
        issue: &Issue,
        agent: &str,
        prompt: &str,
        argv: Vec<String>,
    ) -> Vec<WireFrame> {
        let sandbox = self.launcher_profile().map(String::from);
        self.close_launcher();
        let Some(project) = self.issues.project else {
            return Vec::new();
        };
        let session = sandbox.is_none().then_some(self.session.as_str());
        let (cmd, written) = match self.host.deploy_argv(agent, argv, prompt, session) {
            Ok(deploy) => deploy,
            Err(e) => {
                self.set_status(format!("{e:#}"));
                return Vec::new();
            }
        };
        let run = Request::Run {
            workspace: Some(project),
            dir: None,
            cmd,
            sandbox,
            title: None,
            issue: Some(issue.link()),
            from: None,
        };
        if self.issues_deploy_here() {
            return vec![self.follow(&run, FollowUp::Deployed(written))];
        }
        let name = issues::workspace_name(&issue.source, &issue.id);
        self.set_status(format!("creating workspace {name}…"));
        let fork = Request::WorkspaceFork {
            id: project,
            name,
            revision: None,
            dest: None,
        };
        vec![self.follow(&fork, FollowUp::Deploy(Box::new(run), written))]
    }

    /// The write-backs landed deploys ask for, for the event loop to run off
    /// its thread; each outcome comes back through `written_back`.
    pub fn take_write_backs(&mut self) -> Vec<WriteBack> {
        std::mem::take(&mut self.issues.write_backs)
    }

    /// Land a write-back's outcome for issue `id`: said either way, a refusal
    /// in the error accent.
    pub fn written_back(&mut self, id: &str, written: Result<(), String>) {
        self.set_status(match written {
            Ok(()) => format!("wrote back to {}", issues::label(id)),
            Err(err) => format!("error: writing back to {}: {err}", issues::label(id)),
        });
    }

    /// What the agent deployed into `pane` of `workspace` writes back, if its
    /// source asks: the comment names the workspace made for the issue in its
    /// project — or the project, deployed there — the pane, this session, and
    /// in a sandbox the branch it pushes to. The tracker is asked from the
    /// project's directory.
    pub(super) fn write_back(&self, workspace: WorkspaceId, pane: PaneId) -> Option<WriteBack> {
        let info = &self.panes.get(&pane)?.info;
        let issue = info.issue.as_deref()?;
        let source = self
            .config
            .issues
            .iter()
            .find(|s| s.name == issue.source && s.writes_back())?;
        let find = |id| self.workspaces.iter().find(|w| w.id == id);
        let ran = find(workspace)?;
        let (project, made) = match ran.parent.and_then(find) {
            Some(project) => (
                project,
                Some(issues::workspace_name(&issue.source, &issue.id)),
            ),
            None => (ran, None),
        };
        let branch = info.sandbox_name.as_deref().map(tutti_core::sandbox_branch);
        Some(WriteBack {
            source: source.name.clone(),
            id: issue.id.clone(),
            comment: issues::deployed_comment(
                &info.title,
                &project.name,
                made.as_deref(),
                pane,
                &self.session,
                branch.as_deref(),
            ),
            dir: project.dir.clone(),
        })
    }
}

/// What a deploy writes back to its tracker (`App::take_write_backs`): the
/// issue's source and id, the comment naming where its agent runs, and the
/// project directory the tracker is asked from, as a fetch is.
#[derive(Debug, PartialEq)]
pub struct WriteBack {
    source: String,
    pub id: String,
    comment: String,
    dir: PathBuf,
}

impl WriteBack {
    /// Write it back from the daemon's machine, whose tracker logins it is —
    /// blocking, so the event loop runs it on a thread.
    pub fn run(&self, host: &Host) -> Result<(), String> {
        host.write_back(&self.source, &self.id, &self.comment, &self.dir)
            .map_err(|e| format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::app::{control, tagged};
    use crate::attach::fixtures::*;
    use crate::attach::launcher::{LaunchKind, LauncherRow};
    use crate::config::Config;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use tutti_core::{AgentState, Event, Response, TabId, WorkspaceView};

    /// Two sources — GitHub, Linear — and a sandbox, so the armed launcher's
    /// rows (the whole catalog in a sandbox) never depend on this machine's
    /// PATH.
    const ISSUES_CONFIG: &str = "[[sandboxes]]\nname = \"orb\"\nimage = \"i\"\n\n\
         [[issues]]\nname = \"gh\"\nkind = \"github\"\n\n\
         [[issues]]\nname = \"linear\"\nkind = \"linear\"\n";

    fn issues_config() -> Config {
        Config::parse(ISSUES_CONFIG).unwrap()
    }

    /// Project `api` (id 1, one shell in tab 1) at `dir`.
    fn project_at(dir: &Path) -> Vec<WorkspaceView> {
        let mut api = workspace(
            1,
            "api",
            None,
            vec![tab(1, "1", true, leaf(1), vec![shell(1)])],
        );
        api.dir = dir.to_path_buf();
        vec![api]
    }

    /// A project directory that is a jj repo, as far as the client can tell.
    fn jj_project(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tutti-issues-{label}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".jj")).unwrap();
        dir
    }

    fn gh_issue(id: &str, title: &str) -> Issue {
        Issue {
            source: "gh".into(),
            id: id.into(),
            title: title.into(),
            body: "It loops.".into(),
            url: Some(format!("https://github.com/acme/api/issues/{id}")),
            labels: vec!["bug".into()],
        }
    }

    /// The panel opened over `dir`'s project, GitHub's fetch landed with two
    /// issues and Linear's failed.
    fn issues_panel(dir: &Path) -> App {
        let mut app = App::with_config(issues_config());
        attach_with(&mut app, project_at(dir));
        app.on_key(ctrl('b'));
        app.on_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT));
        let (generation, _, _) = app.take_issue_fetch().unwrap();
        app.issues_loaded(
            generation,
            0,
            Ok(vec![
                gh_issue("123", "Fix the login redirect"),
                gh_issue("120", "Docs typo"),
            ]),
        );
        app.issues_loaded(generation, 1, Err("LINEAR_API_KEY is not set".into()));
        app
    }

    fn listed(app: &App) -> Vec<String> {
        app.issue_rows()
            .iter()
            .map(|row| match row {
                IssueRow::Issue(issue) => issue.id.clone(),
                IssueRow::Status { source, error } => {
                    format!("[{source}] {}", error.unwrap_or("loading"))
                }
            })
            .collect()
    }

    #[test]
    fn prefix_shift_i_opens_the_panel_and_asks_the_loop_to_fetch() {
        let dir = jj_project("open");
        let vim = Config::parse(&format!("preset = \"vim\"\n{ISSUES_CONFIG}")).unwrap();
        for cfg in [issues_config(), vim] {
            let mut app = App::with_config(cfg);
            attach_with(&mut app, project_at(&dir));
            app.on_key(ctrl('b'));
            let out = app.on_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT));
            assert!(out.is_empty(), "opening the panel sends nothing");
            assert_eq!(app.mode, Mode::Issues);
            assert_eq!(app.issues_title(), " issues · api ");
            let (generation, at, sources) = app.take_issue_fetch().expect("a fetch to run");
            assert_eq!(at, dir, "each source runs in the project directory");
            assert_eq!(sources.len(), 2);
            assert!(app.take_issue_fetch().is_none(), "asked once");
            assert_eq!(listed(&app), ["[gh] loading", "[linear] loading"]);

            // A fetch from before the panel reopened is dropped.
            app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(app.mode, Mode::Terminal, "esc closes the panel");
            app.on_key(ctrl('b'));
            app.on_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT));
            app.issues_loaded(generation, 0, Ok(vec![gh_issue("1", "stale")]));
            assert_eq!(listed(&app), ["[gh] loading", "[linear] loading"]);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn landed_issues_list_by_source_and_slash_filters_them() {
        let dir = jj_project("filter");
        let mut app = issues_panel(&dir);
        assert_eq!(
            listed(&app),
            ["123", "120", "[linear] LINEAR_API_KEY is not set"]
        );
        app.on_key(plain('j'));
        assert_eq!(app.issues_selected(), 1);
        app.on_key(plain('j'));
        assert_eq!(app.issues_selected(), 1, "stays on the last issue");

        app.on_key(plain('/'));
        for c in "login".chars() {
            app.on_key(plain(c));
        }
        assert_eq!(app.issues_query(), ("login", true));
        assert_eq!(
            listed(&app),
            ["123", "[linear] LINEAR_API_KEY is not set"],
            "the filter narrows the issues, not the sources' status"
        );
        assert_eq!(app.issues_selected(), 0, "typing re-highlights the top");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.issues_query(), ("login", false), "enter keeps it");
        assert_eq!(app.mode, Mode::Issues);

        app.on_key(plain('/'));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.issues_query(), ("", false), "esc clears it");
        assert_eq!(listed(&app).len(), 3);
        assert_eq!(app.mode, Mode::Issues, "and leaves the panel open");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enter_arms_the_launcher_and_the_pick_gives_the_issue_its_own_workspace() {
        let dir = jj_project("deploy");
        let mut app = issues_panel(&dir);
        assert!(!app.issues_deploy_here(), "a jj project gets a workspace");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Launcher);
        assert_eq!(
            app.launcher_title(),
            " deploy #123 · Fix the login redirect "
        );
        let prompt = "Work on this issue and commit your changes when done.\n\n\
                      123: Fix the login redirect\n\nIt loops.\n\n\
                      https://github.com/acme/api/issues/123";
        let names = |app: &App| -> Vec<String> {
            app.launcher.rows.iter().map(|r| r.name.clone()).collect()
        };
        let claude = app
            .launcher
            .rows
            .iter()
            .find(|r| r.name == "claude")
            .unwrap();
        assert_eq!(claude.kind, LaunchKind::Agent(vec!["claude".into()]));

        // In the sandbox every agent that takes a prompt is selectable — and
        // only those, no shell or command.
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.launcher_title(),
            " deploy #123 · Fix the login redirect · sandbox orb "
        );
        assert_eq!(
            names(&app),
            [
                "claude", "codex", "gemini", "opencode", "goose", "pi", "omp", "qwen", "cursor",
                "copilot", "droid", "muse", "vibe", "auggie", "amazon-q", "ori"
            ]
        );
        assert!(app.launcher.rows.iter().all(LauncherRow::selectable));
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let fork = Request::WorkspaceFork {
            id: WorkspaceId(1),
            name: "gh-123".into(),
            revision: None,
            dest: None,
        };
        assert_eq!(out, vec![tagged(&fork, 1)], "the issue's workspace first");
        assert_eq!(app.mode, Mode::Terminal);

        // Another request's reply moves nothing; the fork's own sends the run.
        let unrelated = Response::WorkspaceCreated { id: WorkspaceId(7) };
        assert!(app.handle_response(Some(9), unrelated).is_empty());
        let out = app.handle_response(Some(1), Response::WorkspaceCreated { id: WorkspaceId(2) });
        let link = gh_issue("123", "Fix the login redirect").link();
        let run = Request::Run {
            workspace: Some(WorkspaceId(2)),
            dir: None,
            cmd: vec!["claude".into(), prompt.into()],
            sandbox: Some("orb".into()),
            title: None,
            issue: Some(link.clone()),
            from: None,
        };
        assert_eq!(
            out,
            vec![tagged(&run, 2)],
            "the agent runs there on the issue"
        );

        // The view carrying the agent lands before its `Ran`, which the
        // client follows to the agent.
        let mut view = project_at(&dir);
        let mut deployed = agent(9, "claude", AgentState::Working);
        deployed.title = "claude".into();
        deployed.issue = Some(Box::new(link));
        view.push(workspace(
            2,
            "api-gh-123",
            None,
            vec![tab(5, "5", false, leaf(9), vec![deployed])],
        ));
        app.handle_event(Event::LayoutChanged { workspaces: view });
        let ran = Response::Ran {
            workspace: WorkspaceId(2),
            tab: TabId(5),
            pane: PaneId(9),
        };
        let out = app.handle_response(Some(2), ran);
        assert_eq!(
            (app.active_tab, app.focused),
            (Some(TabId(5)), Some(PaneId(9)))
        );
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(5) })]);
        assert_eq!(app.transient(), Some("deployed #123 to claude"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn h_deploys_into_a_fresh_tab_of_the_project_and_a_failed_workspace_runs_nothing() {
        let dir = jj_project("here");
        let mut app = issues_panel(&dir);
        app.on_key(plain('h'));
        assert!(app.issues_deploy_here());
        app.on_key(plain('j'));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let typo = gh_issue("120", "Docs typo");
        let run = Request::Run {
            workspace: Some(WorkspaceId(1)),
            dir: None,
            cmd: vec![
                "claude".into(),
                issues::prompt(issues::DEFAULT_PROMPT, &typo),
            ],
            sandbox: Some("orb".into()),
            title: None,
            issue: Some(typo.link()),
            from: None,
        };
        assert_eq!(
            out,
            vec![tagged(&run, 1)],
            "here, the run goes straight to the project"
        );

        // A workspace that fails to come up runs nothing.
        let mut app = issues_panel(&dir);
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let refused = Response::Error {
            message: "workspace destination already exists".into(),
        };
        assert!(app.handle_response(Some(1), refused).is_empty());
        let late = Response::WorkspaceCreated { id: WorkspaceId(3) };
        assert!(app.handle_response(Some(1), late).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Once a deploy's agent runs, a source that asks for write-back has the
    /// event loop write it: the comment names the workspace made for the
    /// issue and its project, the pane, the session and the sandbox's branch,
    /// asked from the project's directory. Its outcome is said either way. A
    /// source asking for nothing gets nothing.
    #[test]
    fn a_landed_deploy_writes_back_as_its_source_asks() {
        let dir = jj_project("writeback");
        for (config, asks) in [
            (ISSUES_CONFIG.to_string(), false),
            (
                ISSUES_CONFIG.replace("kind = \"github\"\n", "kind = \"github\"\ncomment = true\n"),
                true,
            ),
        ] {
            let mut app = App::with_config(Config::parse(&config).unwrap());
            attach_with(&mut app, project_at(&dir));
            app.on_key(ctrl('b'));
            app.on_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT));
            let (generation, _, _) = app.take_issue_fetch().unwrap();
            let fixed = gh_issue("123", "Fix the login redirect");
            app.issues_loaded(generation, 0, Ok(vec![fixed.clone()]));
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            app.handle_response(Some(1), Response::WorkspaceCreated { id: WorkspaceId(2) });

            let mut view = project_at(&dir);
            let mut deployed = agent(9, "claude", AgentState::Working);
            deployed.title = "claude".into();
            deployed.issue = Some(Box::new(fixed.link()));
            deployed.sandbox_name = Some("claude-9-x".into());
            let mut made = workspace(
                2,
                "api-gh-123",
                None,
                vec![tab(5, "5", false, leaf(9), vec![deployed])],
            );
            made.parent = Some(WorkspaceId(1));
            view.push(made);
            app.handle_event(Event::LayoutChanged { workspaces: view });
            let ran = Response::Ran {
                workspace: WorkspaceId(2),
                tab: TabId(5),
                pane: PaneId(9),
            };
            app.handle_response(Some(2), ran);
            let jobs = app.take_write_backs();
            if !asks {
                assert!(jobs.is_empty(), "a source asking nothing gets nothing");
                continue;
            }
            assert_eq!(
                jobs,
                [WriteBack {
                    source: app.config.issues[0].name.clone(),
                    id: "123".into(),
                    comment: "tutti deployed claude on this issue: workspace `gh-123` of project \
                              `api`, pane 9 of session `t`, pushing to branch `tutti/claude-9-x`."
                        .into(),
                    dir: dir.clone(),
                }]
            );
            assert!(app.take_write_backs().is_empty(), "taken once");
            app.written_back("123", Err("gh: HTTP 403".into()));
            assert_eq!(
                app.transient(),
                Some("error: writing back to #123: gh: HTTP 403")
            );
            app.written_back("123", Ok(()));
            assert_eq!(app.transient(), Some("wrote back to #123"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An issue whose prompt is too long for one argument deploys through a
    /// file of the session's prompt directory (`issues::deploy_argv`) — the
    /// agent's own file input where it has one, else a file its message
    /// names — and never into a sandbox, whose agent can't read it.
    #[test]
    fn a_prompt_too_long_for_one_argument_deploys_through_a_file_never_a_sandbox() {
        let mut app = issues_panel(Path::new("/tmp/w"));
        let files = std::env::temp_dir().join(format!("tutti-app-{}.prompts", std::process::id()));
        app.set_local(|local| local.prompts = Some(files.clone()));
        let mut long = gh_issue("7", "Long");
        long.body = "a".repeat(issues::MAX_ARG);
        app.issues_loaded(app.issues.generation, 0, Ok(vec![long]));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "nothing runs");
        assert!(
            app.transient()
                .is_some_and(|s| s.contains("too long for a command line")),
            "{:?}",
            app.transient()
        );

        app.mode = Mode::Issues;
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let (issue, prompt) = app.launcher.issue.take().unwrap();
        let mut run = |agent: &str, argv: &[&str]| {
            let argv = argv.iter().map(|s| s.to_string()).collect();
            let out = app.deploy_frames(&issue, agent, &prompt, argv);
            let [WireFrame::Control(sent)] = &out[..] else {
                panic!("expected one run, got {out:?}");
            };
            let sent: serde_json::Value = serde_json::from_slice(sent).unwrap();
            assert_eq!(sent["type"], "run");
            let cmd: Vec<String> = serde_json::from_value(sent["cmd"].clone()).unwrap();
            cmd
        };
        let claude = run("claude", &["claude"]);
        assert!(claude[1].len() < 1024, "{claude:?}");
        let named = claude[1]
            .split("in the file ")
            .nth(1)
            .and_then(|rest| rest.split(". Read").next())
            .unwrap()
            .to_string();
        let ori = run("ori", &["ori", "code"]);
        assert_eq!(ori[..4], ["ori", "code", "--interactive", "--prompt-file"]);
        for file in [named.as_str(), ori[4].as_str()] {
            assert!(Path::new(file).starts_with(&files), "{file}");
            assert_eq!(std::fs::read_to_string(file).unwrap(), prompt);
        }
        std::fs::remove_dir_all(&files).unwrap();
    }

    /// A deploy that no pane ends up with — its fork or its run refused, or
    /// left unanswered past its deadline — removes the prompt file it wrote,
    /// which nothing else would before the daemon stops. One whose run lands
    /// leaves it to the daemon, which removes it as the agent exits.
    #[test]
    fn a_deploy_no_pane_took_removes_its_prompt_file() {
        let files =
            std::env::temp_dir().join(format!("tutti-app-{}-refused.prompts", std::process::id()));
        let count = || std::fs::read_dir(&files).map_or(0, |dir| dir.count());
        let refused = || Response::Error {
            message: "refused".into(),
        };
        for (dir, forks) in [
            (PathBuf::from("/tmp/w"), false),
            (jj_project("refused"), true),
        ] {
            let mut app = issues_panel(&dir);
            app.set_local(|local| local.prompts = Some(files.clone()));
            let mut long = gh_issue("7", "Long");
            long.body = "a".repeat(issues::FILE_PROMPT);
            app.issues_loaded(app.issues.generation, 0, Ok(vec![long]));
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            let (issue, prompt) = app.launcher.issue.take().unwrap();
            let deploy = |app: &mut App| {
                let argv = vec!["ori".into(), "code".into()];
                assert_eq!(app.deploy_frames(&issue, "ori", &prompt, argv).len(), 1);
                assert_eq!(count(), 1, "the prompt file is written");
                app.follow_ups.last_req
            };
            // The run's own request: sent once the fork lands, if any.
            let run = |app: &mut App, req: u64| {
                if forks {
                    let created = Response::WorkspaceCreated { id: WorkspaceId(2) };
                    assert_eq!(app.handle_response(Some(req), created).len(), 1);
                }
                app.follow_ups.last_req
            };

            let req = deploy(&mut app);
            app.handle_response(Some(req), refused());
            assert_eq!(count(), 0, "refused at once (forks: {forks})");
            let req = deploy(&mut app);
            let req = run(&mut app, req);
            app.handle_response(Some(req), refused());
            assert_eq!(count(), 0, "its run refused (forks: {forks})");
            deploy(&mut app);
            assert!(app.expire_follow_ups(Instant::now() + Duration::from_secs(181)));
            assert_eq!(count(), 0, "never answered (forks: {forks})");

            let req = deploy(&mut app);
            let req = run(&mut app, req);
            let ran = Response::Ran {
                workspace: WorkspaceId(2),
                tab: TabId(1),
                pane: PaneId(9),
            };
            app.handle_response(Some(req), ran);
            assert_eq!(count(), 1, "a landed run's file is the daemon's");
            std::fs::remove_dir_all(&files).unwrap();
        }
        std::fs::remove_dir_all(jj_project("refused")).ok();
    }

    #[test]
    fn a_project_outside_jj_deploys_here_and_esc_backs_out_to_the_panel() {
        // `/tmp/w`, the fixtures' directory, is no jj repo.
        let mut app = issues_panel(Path::new("/tmp/w"));
        assert!(!app.issues.jj);
        assert!(app.issues_deploy_here(), "no workspace to give the issue");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Launcher);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Issues, "esc backs out to the panel");
        assert_eq!(app.launcher_title(), " run ", "disarmed");
        assert_eq!(listed(&app).len(), 3, "its issues still listed");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Terminal);
    }
}
