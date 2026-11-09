//! The prompts at the sidebar's foot: the add-project directory
//! (`Mode::SidebarPrompt`, the first-run prompt too) and guided workspace
//! creation's two steps — the name (`Mode::SidebarWorkspaceName`), then the
//! destination (`Mode::SidebarWorkspaceDest`) — with the directory
//! completions the daemon's machine answers off the event loop.

use std::path::{Component, Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tutti_core::{Frame as WireFrame, Request, WorkspaceId};

use super::app::{App, Mode};
use super::follow::FollowUp;
use super::sidebar::Sidebar;

/// What a foot prompt is editing, which means something only in the three
/// prompt modes.
#[derive(Default)]
pub(super) struct Prompt {
    /// The text being typed: a directory in `SidebarPrompt` (add project) and
    /// `SidebarWorkspaceDest`, the name in `SidebarWorkspaceName`.
    pub(super) input: String,
    /// Directory completions and the input they answer. Every edit asks the
    /// host again on a thread — under `--host` that is an ssh round trip — and
    /// the list shown meanwhile answers an older input, which Tab and Enter
    /// never take.
    completions: Vec<String>,
    completions_for: String,
    /// The input to ask about next, taken by the event loop.
    ask: Option<String>,
    /// The highlighted completion row; `Tab` fills it, the arrows move it.
    selected: usize,
    /// The workspace name captured in step 1, carried while the `where:` step is
    /// edited so submit can send both name and destination.
    name: String,
}

impl App {
    /// Arm the first-run prompt: open the sidebar's new-project prompt prefilled
    /// with `dir`, so an empty session asks where to start instead of assuming
    /// the cwd. Reuses the sidebar prompt machinery — Enter creates the
    /// workspace + shell pane, Esc drops to the (empty) sidebar creating nothing.
    pub fn start_first_run_prompt(&mut self, dir: String) {
        self.mode = Mode::SidebarPrompt;
        self.prompt.input = dir;
        self.prompt.selected = 0;
        self.refresh_completions();
    }

    /// Whether a foot-of-sidebar prompt (add-project or guided workspace create)
    /// is being edited — either overlays the frame and suppresses the selection
    /// highlight.
    pub fn sidebar_prompt_active(&self) -> bool {
        matches!(
            self.mode,
            Mode::SidebarPrompt | Mode::SidebarWorkspaceName(_) | Mode::SidebarWorkspaceDest(_)
        )
    }

    /// The label prefix for the active foot prompt.
    pub fn sidebar_prompt_label(&self) -> &'static str {
        match self.mode {
            Mode::SidebarWorkspaceName(_) => "workspace name: ",
            Mode::SidebarWorkspaceDest(_) => "where: ",
            _ => "open: ",
        }
    }

    /// Whether the active foot prompt shows directory completions — the
    /// add-project prompt and the `where:` step do; the `workspace name:` step
    /// (a bare identifier, not a path) does not.
    pub fn sidebar_prompt_shows_completions(&self) -> bool {
        matches!(
            self.mode,
            Mode::SidebarPrompt | Mode::SidebarWorkspaceDest(_)
        )
    }

    pub fn sidebar_prompt(&self) -> &str {
        &self.prompt.input
    }

    /// The directory completions shown under the add-project prompt.
    pub fn prompt_completions(&self) -> &[String] {
        &self.prompt.completions
    }

    /// The highlighted completion row index.
    pub fn prompt_selected(&self) -> usize {
        self.prompt.selected
    }

    /// Open the add-project prompt on the sidebar `n` path, prefilled with the
    /// common parent of the existing workspaces so only the project name is left
    /// to type — the directory-completion panel fills in the rest.
    pub(super) fn open_project_prompt(&mut self) {
        self.mode = Mode::SidebarPrompt;
        let dirs: Vec<PathBuf> = self.workspaces.iter().map(|w| w.dir.clone()).collect();
        self.prompt.input = prompt_prefill(&dirs, self.home.as_deref());
        self.prompt.selected = 0;
        self.status = None;
        self.refresh_completions();
    }

    pub(super) fn on_key_prompt(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.prompt.input.push(c);
                self.prompt.selected = 0;
                self.refresh_completions();
                Vec::new()
            }
            KeyCode::Backspace => {
                self.prompt.input.pop();
                self.prompt.selected = 0;
                self.refresh_completions();
                Vec::new()
            }
            // Tab fills in the highlighted directory; the arrows move the
            // highlight. Enter submits the typed path when it exists, and
            // otherwise takes the highlighted completion — typing a prefix of
            // the directory you want and hitting Enter must not mount the
            // prefix as a dead project.
            KeyCode::Tab => {
                self.complete_selection();
                Vec::new()
            }
            KeyCode::Down => {
                if !self.prompt.completions.is_empty() {
                    self.prompt.selected =
                        (self.prompt.selected + 1).min(self.prompt.completions.len() - 1);
                }
                Vec::new()
            }
            KeyCode::Up => {
                self.prompt.selected = self.prompt.selected.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Esc => {
                self.mode = Mode::Sidebar;
                self.prompt = Prompt::default();
                self.status = None;
                Vec::new()
            }
            KeyCode::Enter => self.submit_prompt(),
            _ => Vec::new(),
        }
    }

    /// Tab-complete the prompt to the highlighted directory, appending `/` so the
    /// next component's listing opens immediately. A no-op with no completions.
    fn complete_selection(&mut self) {
        let Some(name) = self.highlighted().cloned() else {
            return;
        };
        let dir_part = match self.prompt.input.rfind('/') {
            Some(i) => &self.prompt.input[..=i],
            None => "",
        };
        self.prompt.input = format!("{dir_part}{name}/");
        self.prompt.selected = 0;
        self.refresh_completions();
    }

    /// Ask for the directory completions of the current input on the daemon's
    /// machine, off the event loop: every edit calls this.
    fn refresh_completions(&mut self) {
        self.prompt.ask = Some(self.prompt.input.clone());
    }

    pub fn take_completion_ask(&mut self) -> Option<String> {
        self.prompt.ask.take()
    }

    /// Land the completions found for `input`, unless the prompt has moved
    /// on: only the latest input's answer is ever shown.
    pub fn completed(&mut self, input: String, found: Vec<String>) {
        if input != self.prompt.input {
            return;
        }
        self.prompt.completions = found;
        self.prompt.completions_for = input;
        if self.prompt.selected >= self.prompt.completions.len() {
            self.prompt.selected = 0;
        }
    }

    /// The highlighted completion, if the list answers what is typed now.
    fn highlighted(&self) -> Option<&String> {
        (self.prompt.completions_for == self.prompt.input)
            .then(|| self.prompt.completions.get(self.prompt.selected))
            .flatten()
    }

    /// Submit the add-project prompt: mount the typed directory as a workspace,
    /// arm the jump to the new tab, then open the launcher to pick its first
    /// pane (esc there spawns the shell, preserving bare `tutti`'s outcome).
    /// A typed path that is not on disk falls back to the highlighted
    /// completion; with no completion either, the prompt stays open with a
    /// transient rather than mounting a dead directory.
    fn submit_prompt(&mut self) -> Vec<WireFrame> {
        let input = self.prompt.input.trim().to_string();
        if input.is_empty() {
            self.prompt = Prompt::default();
            self.status = None;
            self.mode = Mode::Terminal;
            return Vec::new();
        }
        let expanded = self.host.expand_dir(&input).and_then(|(dir, exists)| {
            if exists {
                return Ok(Some(dir));
            }
            if self.highlighted().is_none() {
                return Ok(None);
            }
            self.complete_selection();
            self.host
                .expand_dir(self.prompt.input.trim())
                .map(|(dir, _)| Some(dir))
        });
        let dir = match expanded {
            Ok(Some(dir)) => dir,
            Ok(None) => {
                self.set_status(format!("no such directory: {input}"));
                return Vec::new();
            }
            Err(e) => {
                self.set_status(format!("error: {e:#}"));
                return Vec::new();
            }
        };
        self.prompt = Prompt::default();
        self.status = None;
        self.adopt_active_view = true;
        let name = workspace_name_from_dir(&dir);
        // Harvest against the directory being added: an existing project often
        // has conversations from before it was mounted in tutti.
        self.open_launcher(true, name, Some(dir.as_path()));
        vec![self.follow(&Request::WorkspaceNew { dir }, FollowUp::Mount)]
    }

    /// Open guided workspace creation for the selected entry's workspace: step 1,
    /// the `workspace name:` field at the sidebar's foot. The origin is the
    /// selected workspace (an agent row targets the workspace that owns it). A
    /// no-op with nothing selected.
    pub(super) fn open_workspace_prompt(&mut self, sidebar: &Sidebar) {
        let Some(id) = self.selected_workspace(sidebar).map(|w| w.id) else {
            return;
        };
        self.prompt.name.clear();
        self.mode = Mode::SidebarWorkspaceName(id);
        self.prompt.input.clear();
        self.status = None;
    }

    /// Guided create, step 1 (`workspace name:`): type the name, `esc` cancels
    /// back to the sidebar, `Enter` validates it and advances to the `where:`
    /// step. No directory completions — a workspace name is a bare identifier.
    pub(super) fn on_key_workspace_name(
        &mut self,
        key: KeyEvent,
        origin: WorkspaceId,
    ) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.prompt.input.push(c);
                Vec::new()
            }
            KeyCode::Backspace => {
                self.prompt.input.pop();
                Vec::new()
            }
            KeyCode::Esc => {
                self.mode = Mode::Sidebar;
                self.prompt.input.clear();
                self.status = None;
                Vec::new()
            }
            KeyCode::Enter => self.advance_to_dest_step(origin),
            _ => Vec::new(),
        }
    }

    /// Validate the typed workspace name (fail fast, staying on step 1 with a
    /// transient naming the rule) and, when valid, advance to step 2: prefill the
    /// `where:` field with the sibling default and open its directory completion.
    fn advance_to_dest_step(&mut self, origin: WorkspaceId) -> Vec<WireFrame> {
        let name = self.prompt.input.trim().to_string();
        if !is_valid_fork_name(&name) {
            self.set_status("workspace name: letters, digits, '-' or '_' only".into());
            return Vec::new();
        }
        let dir = self
            .workspaces
            .iter()
            .find(|w| w.id == origin)
            .map(|w| w.dir.clone());
        self.prompt.name = name.clone();
        self.prompt.input = dir
            .and_then(|dir| self.host.sibling_dest(&dir, &name).ok())
            .unwrap_or_default();
        self.prompt.selected = 0;
        self.mode = Mode::SidebarWorkspaceDest(origin);
        self.status = None;
        self.refresh_completions();
        Vec::new()
    }

    /// Guided create, step 2 (`where:`): edit the destination with the same
    /// directory completion the add-project prompt uses. `esc` steps back to the
    /// name field (restoring it), `Enter` submits the `WorkspaceFork`.
    pub(super) fn on_key_workspace_dest(
        &mut self,
        key: KeyEvent,
        origin: WorkspaceId,
    ) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.prompt.input.push(c);
                self.prompt.selected = 0;
                self.refresh_completions();
                Vec::new()
            }
            KeyCode::Backspace => {
                self.prompt.input.pop();
                self.prompt.selected = 0;
                self.refresh_completions();
                Vec::new()
            }
            KeyCode::Tab => {
                self.complete_selection();
                Vec::new()
            }
            KeyCode::Down => {
                if !self.prompt.completions.is_empty() {
                    self.prompt.selected =
                        (self.prompt.selected + 1).min(self.prompt.completions.len() - 1);
                }
                Vec::new()
            }
            KeyCode::Up => {
                self.prompt.selected = self.prompt.selected.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Esc => {
                // Back to step 1, restoring the name the user had typed.
                self.prompt.input = std::mem::take(&mut self.prompt.name);
                self.prompt.completions.clear();
                self.prompt.selected = 0;
                self.mode = Mode::SidebarWorkspaceName(origin);
                self.status = None;
                Vec::new()
            }
            KeyCode::Enter => self.submit_workspace(origin),
            _ => Vec::new(),
        }
    }

    /// Submit guided create: dispatch a `WorkspaceFork` for the origin workspace
    /// carrying the chosen name and destination (an empty field falls back to the
    /// server's sibling default), and arm the post-create jump + launcher
    /// (`WorkspaceCreated` completes it). A non-jj source is left to the server to
    /// reject — the single source of truth.
    fn submit_workspace(&mut self, id: WorkspaceId) -> Vec<WireFrame> {
        let input = self.prompt.input.trim().to_string();
        let dest = match (!input.is_empty())
            .then(|| self.host.expand_dest(&input))
            .transpose()
        {
            Ok(dest) => dest,
            Err(e) => {
                self.set_status(format!("error: {e:#}"));
                return Vec::new();
            }
        };
        let name = std::mem::take(&mut self.prompt.name);
        self.prompt = Prompt::default();
        self.mode = Mode::Sidebar;
        self.status = None;
        let fork = Request::WorkspaceFork {
            id,
            name,
            revision: None,
            dest,
        };
        vec![self.follow(&fork, FollowUp::Fork)]
    }
}

/// The workspace name a directory mounts under — its final path component — for
/// the launcher title shown before the new workspace's own view has landed.
fn workspace_name_from_dir(dir: &Path) -> Option<String> {
    dir.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Whether `name` is a legal fork name: a non-empty run of ASCII letters,
/// digits, '-' or '_'. Mirrors the server's `valid_fork_name` so a bad name
/// fails fast in the client without a round-trip.
fn is_valid_fork_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The `n` add-project prompt's prefill: the workspaces' common parent
/// directory, home-shortened with a trailing slash so only the project name is
/// left to type. Falls back to `~/` with no workspaces or when the sole shared
/// ancestor is the filesystem root.
fn prompt_prefill(dirs: &[PathBuf], home: Option<&Path>) -> String {
    let Some(base) = common_parent(dirs).filter(|p| p.parent().is_some()) else {
        return "~/".to_string();
    };
    let shown = match home.and_then(|h| base.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => base.display().to_string(),
    };
    format!("{shown}/")
}

/// The deepest directory containing every path in `dirs`: their longest common
/// ancestor, or — for a single path — its parent. `None` for an empty slice or
/// a path without a parent.
fn common_parent(dirs: &[PathBuf]) -> Option<PathBuf> {
    match dirs {
        [] => None,
        [single] => Some(single.parent()?.to_path_buf()),
        [first, rest @ ..] => {
            let mut common: Vec<Component> = first.components().collect();
            for dir in rest {
                let dcomps: Vec<Component> = dir.components().collect();
                let shared = common
                    .iter()
                    .zip(&dcomps)
                    .take_while(|(a, b)| a == b)
                    .count();
                common.truncate(shared);
            }
            let mut path = PathBuf::new();
            for c in &common {
                path.push(c.as_os_str());
            }
            Some(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::app::{control, tagged};
    use crate::attach::fixtures::*;
    use crate::attach::sidebar::SidebarEntry;
    use tutti_core::{Event, Response, TabId};

    #[test]
    fn a_opens_the_project_prompt_when_no_pane_can_take_input() {
        let mut app = App::new();
        assert_eq!(app.mode, Mode::Terminal);
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(
            app.sidebar_prompt_active(),
            "with no panes, the dashboard's `a` must work from terminal mode"
        );
    }

    #[test]
    fn first_run_prompt_arms_prefilled_with_cwd() {
        let mut app = App::new();
        app.start_first_run_prompt("/home/alice/proj".into());
        // Attaching an empty session leaves the prompt engaged and prefilled.
        attach_with(&mut app, vec![]);
        assert!(
            app.sidebar_prompt_active(),
            "the first-run prompt is active"
        );
        assert_eq!(app.sidebar_prompt(), "/home/alice/proj");
        assert!(app.sidebar_focused());
    }

    #[test]
    fn first_run_prompt_enter_creates_workspace_then_opens_the_launcher() {
        // A real directory: the prompt refuses to mount one missing on disk.
        let root = temp_tree(&[]);
        let mut app = App::new();
        app.start_first_run_prompt(root.display().to_string());
        attach_with(&mut app, vec![]);
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let dir = std::fs::canonicalize(&root).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            out,
            vec![tagged(&Request::WorkspaceNew { dir }, 1)],
            "the first-run prompt mounts the workspace, then the launcher picks its pane"
        );
        assert_eq!(app.mode, Mode::Launcher);
        assert!(
            app.adopt_active_view,
            "the jump to the new workspace is armed"
        );

        // esc launches the shell into the new workspace (the old first-run outcome).
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec![shell],
                ephemeral: false,
                sandbox: None,
            })],
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn first_run_prompt_esc_creates_nothing_and_drops_to_the_sidebar() {
        let mut app = App::new();
        app.start_first_run_prompt("/tmp/proj".into());
        attach_with(&mut app, vec![]);
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty(), "esc creates nothing");
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(app.sidebar_prompt().is_empty());
    }

    #[test]
    fn new_project_prompt_prefills_the_common_parent_then_submits() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('a'));
        assert_eq!(app.mode, Mode::SidebarPrompt);
        // Both fixture workspaces live at `/tmp/w`, so the prompt prefills their
        // common parent with a trailing slash — the user types just the name.
        assert_eq!(app.sidebar_prompt(), "/tmp/w/");

        // A name of this process's own, so suites running at once (one per
        // workspace) never race on the directory created below.
        let name = format!("api{}", std::process::id());
        let path = format!("/tmp/w/{name}");
        for c in name.chars() {
            app.on_key(plain(c));
        }
        app.on_key(plain('x'));
        assert_eq!(app.sidebar_prompt(), format!("{path}x"));
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.sidebar_prompt(), path);

        // Enter only mounts a directory that exists on disk, so make it real.
        std::fs::create_dir_all(&path).unwrap();
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let dir = std::fs::canonicalize(&path).unwrap();
        let _ = std::fs::remove_dir_all(&path);
        assert_eq!(
            out,
            vec![tagged(&Request::WorkspaceNew { dir }, 1)],
            "submit mounts the typed directory, then the launcher picks its first pane"
        );
        assert_eq!(app.mode, Mode::Launcher);
        assert!(app.sidebar_prompt().is_empty());
        assert!(
            app.adopt_active_view,
            "the jump to the new workspace is armed"
        );

        // esc in the launcher preserves the old outcome: a shell in the new workspace.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec![shell],
                ephemeral: false,
                sandbox: None,
            })],
            "esc after add-project spawns the shell into the new workspace"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    /// Answer the prompt's completion asks, as the event loop's thread would.
    fn complete(app: &mut App) {
        while let Some(input) = app.take_completion_ask() {
            let found = app.host().complete_dirs(&input);
            app.completed(input, found);
        }
    }

    #[test]
    fn prompt_prefill_uses_the_common_parent_of_the_workspaces() {
        let home = Path::new("/Users/me");
        // Two projects under ~/develop → their common parent, home-shortened.
        let dirs = vec![
            PathBuf::from("/Users/me/develop/tutti"),
            PathBuf::from("/Users/me/develop/other"),
        ];
        assert_eq!(prompt_prefill(&dirs, Some(home)), "~/develop/");

        // A single project: the common parent is its own parent.
        let dirs = vec![PathBuf::from("/Users/me/develop/tutti")];
        assert_eq!(prompt_prefill(&dirs, Some(home)), "~/develop/");

        // No workspaces → the home fallback.
        assert_eq!(prompt_prefill(&[], Some(home)), "~/");

        // Projects whose only shared ancestor is the root fall back to home.
        let dirs = vec![PathBuf::from("/foo/a"), PathBuf::from("/bar/b")];
        assert_eq!(prompt_prefill(&dirs, Some(home)), "~/");

        // A shared parent outside home is shown verbatim.
        let dirs = vec![PathBuf::from("/srv/app/a"), PathBuf::from("/srv/app/b")];
        assert_eq!(prompt_prefill(&dirs, Some(home)), "/srv/app/");
    }

    #[test]
    fn tab_completes_to_the_selected_dir_and_opens_it() {
        let root = temp_tree(&["alpha", "beta"]);
        let mut app = App::new();
        // An absolute prefix keeps the test independent of the real HOME/cwd.
        app.start_first_run_prompt(format!("{}/a", root.display()));
        complete(&mut app);
        assert_eq!(app.prompt_completions(), &["alpha".to_string()]);

        let out = app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(out.is_empty(), "Tab sends nothing to the server");
        assert_eq!(
            app.sidebar_prompt(),
            format!("{}/alpha/", root.display()),
            "Tab fills the highlight and opens the directory with a trailing slash"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_submits_the_typed_input_when_it_is_a_real_dir() {
        let root = temp_tree(&["alpha", "beta"]);
        let mut app = App::new();
        app.start_first_run_prompt(format!("{}/", root.display()));
        complete(&mut app);
        // Two matches; move the highlight onto the second.
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.prompt_selected(), 1);

        let typed = app.sidebar_prompt().to_string();
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // The typed path exists, so Enter mounts it (the tempdir), not the
        // highlight — the highlight only wins when the typed path is missing.
        let dir = std::fs::canonicalize(&typed).unwrap();
        assert_eq!(
            out,
            vec![tagged(&Request::WorkspaceNew { dir }, 1)],
            "submit mounts the typed directory, then opens the launcher"
        );
        assert_eq!(app.mode, Mode::Launcher);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_takes_the_highlight_when_the_typed_prefix_is_not_a_dir() {
        // The trap: type "the", see "the-librarian" highlighted, hit Enter —
        // it must mount the-librarian, not a dead "the" project.
        let root = temp_tree(&["the-librarian"]);
        let mut app = App::new();
        app.start_first_run_prompt(format!("{}/the", root.display()));
        complete(&mut app);
        assert_eq!(app.prompt_completions(), &["the-librarian".to_string()]);

        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let dir = std::fs::canonicalize(root.join("the-librarian")).unwrap();
        assert_eq!(
            out,
            vec![tagged(&Request::WorkspaceNew { dir }, 1)],
            "enter completes to the highlighted directory and mounts it"
        );
        assert_eq!(app.mode, Mode::Launcher);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_with_no_match_keeps_the_prompt_open() {
        let root = temp_tree(&["alpha"]);
        let mut app = App::new();
        app.start_first_run_prompt(format!("{}/zzz", root.display()));
        complete(&mut app);
        assert!(app.prompt_completions().is_empty());

        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "nothing is sent for a missing directory");
        assert_eq!(app.mode, Mode::SidebarPrompt);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|(m, _)| m.contains("no such directory")),
            "the prompt explains why nothing was mounted"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn arrow_keys_move_the_completion_selection_and_typing_resets_it() {
        let root = temp_tree(&["alpha", "beta", "gamma"]);
        let mut app = App::new();
        app.start_first_run_prompt(format!("{}/", root.display()));
        complete(&mut app);
        assert_eq!(app.prompt_completions().len(), 3);
        assert_eq!(app.prompt_selected(), 0);

        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.prompt_selected(), 2);
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.prompt_selected(), 2, "Down clamps at the last row");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.prompt_selected(), 1);

        // Typing filters and snaps the highlight back to the best match.
        app.on_key(plain('g'));
        complete(&mut app);
        assert_eq!(app.prompt_selected(), 0);
        assert_eq!(app.prompt_completions(), &["gamma".to_string()]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn completions_for_an_older_input_are_shown_but_never_taken() {
        let root = temp_tree(&["alpha", "beta"]);
        let mut app = App::new();
        app.start_first_run_prompt(format!("{}/", root.display()));
        let asked = app.take_completion_ask().unwrap();
        // Typing outpaces the answer, which then lands for nothing.
        app.on_key(plain('a'));
        let found = app.host().complete_dirs(&asked);
        app.completed(asked, found);
        assert!(
            app.prompt_completions().is_empty(),
            "an answer for an older input is dropped"
        );

        complete(&mut app);
        assert_eq!(app.prompt_completions(), &["alpha".to_string()]);
        app.on_key(plain('x'));
        assert_eq!(
            app.prompt_completions(),
            &["alpha".to_string()],
            "the older list stays up until the new answer lands"
        );
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.sidebar_prompt(),
            format!("{}/ax", root.display()),
            "Tab never fills from a list for an older input"
        );
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "nor does Enter mount its highlight");
        assert_eq!(app.mode, Mode::SidebarPrompt);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn w_guided_create_walks_name_then_dest_and_submits() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app); // selection rests on the api workspace (id 1)

        // Step 1: `w` opens the name field, sending nothing yet.
        let out = app.on_key(plain('w'));
        assert!(out.is_empty(), "w opens the name prompt, sending nothing");
        assert!(matches!(app.mode, Mode::SidebarWorkspaceName(_)));
        for c in "feature".chars() {
            app.on_key(plain(c));
        }
        assert_eq!(app.sidebar_prompt(), "feature");

        // Enter validates the name and advances to the prefilled `where:` step.
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "advancing to the dest step sends nothing");
        assert!(matches!(app.mode, Mode::SidebarWorkspaceDest(_)));
        assert!(
            app.sidebar_prompt().contains("w-feature"),
            "the dest step prefills the sibling default: {:?}",
            app.sidebar_prompt()
        );

        // Submitting the dest step fires the fork request carrying name + dest.
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.mode,
            Mode::Sidebar,
            "back to the sidebar awaiting reply"
        );
        assert!(app.sidebar_prompt().is_empty());
        match only_request(&out) {
            Request::WorkspaceFork {
                id,
                name,
                revision,
                dest,
            } => {
                assert_eq!(id, WorkspaceId(1));
                assert_eq!(name, "feature");
                assert_eq!(revision, None);
                assert!(
                    dest.is_some(),
                    "the guided destination rides on the request"
                );
            }
            other => panic!("expected WorkspaceFork, got {other:?}"),
        }
    }

    #[test]
    fn w_on_an_agent_row_creates_under_its_owning_workspace() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // web workspace
        app.on_key(plain('j')); // first agent (blocked pane 3, in tab 2 → workspace 2)
        assert!(matches!(
            app.sidebar().entries[app.sidebar_selected()],
            SidebarEntry::Agent(_)
        ));

        app.on_key(plain('w'));
        for c in "hotfix".chars() {
            app.on_key(plain(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // to dest step
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match only_request(&out) {
            Request::WorkspaceFork { id, name, .. } => {
                assert_eq!(id, WorkspaceId(2), "targets the workspace owning the tab");
                assert_eq!(name, "hotfix");
            }
            other => panic!("expected WorkspaceFork, got {other:?}"),
        }
    }

    #[test]
    fn w_starts_create_rather_than_unfocusing_the_sidebar() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('w'));
        assert!(
            matches!(app.mode, Mode::SidebarWorkspaceName(_)),
            "w opens guided create; esc is the back key, not w"
        );
    }

    #[test]
    fn workspace_name_step_rejects_a_bad_name() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('w'));
        for c in "bad name".chars() {
            app.on_key(plain(c)); // the space makes the name invalid
        }
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "an invalid name emits no request");
        assert!(
            matches!(app.mode, Mode::SidebarWorkspaceName(_)),
            "step 1 stays open so the name can be fixed"
        );
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("letters, digits")),
            "the transient names the naming rule"
        );
    }

    #[test]
    fn workspace_name_step_esc_cancels_back_to_the_sidebar() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('w'));
        app.on_key(plain('x'));
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty(), "esc creates nothing");
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(app.sidebar_prompt().is_empty());
    }

    #[test]
    fn workspace_dest_step_esc_steps_back_to_the_name() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('w'));
        for c in "feature".chars() {
            app.on_key(plain(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // to dest step
        assert!(matches!(app.mode, Mode::SidebarWorkspaceDest(_)));
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty(), "esc sends nothing");
        assert!(
            matches!(app.mode, Mode::SidebarWorkspaceName(_)),
            "esc steps back a step"
        );
        assert_eq!(
            app.sidebar_prompt(),
            "feature",
            "the name the user typed is restored"
        );
    }

    #[test]
    fn workspace_created_jumps_to_it_and_opens_the_launcher() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('w'));
        for c in "feature".chars() {
            app.on_key(plain(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // to dest step
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // submit

        // The server broadcasts the fresh view (carrying the new workspace and its
        // tab) before the WorkspaceCreated reply — mirror that ordering here.
        let mut view = view_two_workspaces();
        let mut created = workspace(
            3,
            "w-feature",
            Some("main"),
            vec![tab(3, "3", true, leaf(9), vec![shell(9)])],
        );
        created.dir = std::path::PathBuf::from("/tmp/repo-feature");
        view.push(created);
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        // The old tab is untouched until the reply lands.
        assert_eq!(app.active_tab, Some(TabId(1)));

        // The fork was the first tagged request: req 1.
        app.handle_response(Some(1), Response::WorkspaceCreated { id: WorkspaceId(3) });
        assert_eq!(
            app.active_tab,
            Some(TabId(3)),
            "the WorkspaceCreated reply jumps to the new workspace's tab"
        );
        assert_eq!(
            app.mode,
            Mode::Launcher,
            "and opens the launcher to pick the agent to run beside its shell"
        );
        assert_eq!(
            app.launcher_title(),
            " run in w-feature ",
            "the launcher names the workspace it will run in"
        );
        // The transient names what was made and where, in the new vocabulary.
        let transient = app.transient().expect("a post-create transient is posted");
        assert!(
            transient.contains("workspace")
                && transient.contains("w-feature")
                && transient.contains("/tmp/repo-feature"),
            "the transient names the workspace and its path: {transient:?}"
        );
    }

    #[test]
    fn new_workspace_prompt_esc_cancels_back_to_the_sidebar() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('a'));
        app.on_key(plain('n'));
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty());
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(app.sidebar_prompt().is_empty());
    }

    #[test]
    fn is_valid_fork_name_matches_the_server_rule() {
        assert!(is_valid_fork_name("feature-1_x"));
        assert!(is_valid_fork_name("ABC"));
        assert!(!is_valid_fork_name(""));
        assert!(!is_valid_fork_name("has space"));
        assert!(!is_valid_fork_name("has/slash"));
        assert!(!is_valid_fork_name("dots.bad"));
    }
}
