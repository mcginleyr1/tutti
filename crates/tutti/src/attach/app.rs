//! The attach client's state machine: everything the TUI knows and every
//! decision it makes, driven purely by inbound wire frames and user input. It
//! owns no socket and no terminal, so it can be exercised headlessly in tests.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use tutti_core::{
    AgentKind, AgentState, Direction, Event, Frame as WireFrame, Layout, PaneId, PaneInfo, Request,
    Response, TabId, TabView, WorkspaceId, WorkspaceView,
};

use tutti_agents::Registry;

use super::input;
use super::launcher::{self, LaunchKind, LauncherRow};
use super::layout::pane_rects;
use super::sidebar::{self, Sidebar, SidebarEntry};
use crate::config::{self, Action, Config, PrefixAction, RESIZE_DELTA, SidebarVisibility};

/// The sidebar's fixed column width, and the minimum total width below which it
/// is suppressed entirely so panes keep usable room.
const SIDEBAR_WIDTH: u16 = 30;
const SIDEBAR_MIN_TOTAL: u16 = 80;

/// Rows the top chrome header claims: the full-width app bar plus the dim rule
/// beneath it. The content region begins below them.
const HEADER_ROWS: u16 = 2;

const SCROLLBACK: usize = 10_000;
const STATUS_TTL: Duration = Duration::from_secs(4);
const MOUSE_SCROLL_STEP: usize = 3;
/// How long the prefix can sit unanswered before the which-key popup appears.
const WHICHKEY_DELAY: Duration = Duration::from_millis(500);

/// Up to this many directory completions surface under the add-project prompt.
const MAX_COMPLETIONS: usize = 8;

/// The braille working-spinner frames and their advance interval, shared by
/// every working agent so they animate in lockstep. Swap for ASCII by editing
/// this one const — no config knob.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_TICK: Duration = Duration::from_millis(100);

/// One pane's client-side mirror: a parser fed by snapshots/deltas plus the
/// metadata the status bar shows.
pub struct PaneState {
    pub parser: vt100::Parser,
    pub info: PaneInfo,
    pub damaged: bool,
    /// `Some(offset)` while the pane is frozen browsing its scrollback; deltas
    /// are ignored so the scrolled view stays put.
    pub scroll: Option<usize>,
}

impl PaneState {
    fn seeded(rows: u16, cols: u16, bytes: &[u8], info: PaneInfo) -> Self {
        let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK);
        parser.process(bytes);
        Self {
            parser,
            info,
            damaged: true,
            scroll: None,
        }
    }
}

/// The input mode the client is in. Terminal mode forwards keys to the focused
/// pane; the others intercept keys for multiplexer control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Terminal,
    Prefix,
    ConfirmKill(PaneId),
    /// Confirming a workspace kill raised from the sidebar (`y` kill, `D`
    /// discard the workspace's checkout, anything else cancels).
    ConfirmKillWorkspace(WorkspaceId),
    /// Confirming an agent-pane kill raised from a sidebar agent row (`y` kills
    /// the pane, anything else cancels). Only the pane dies — never the
    /// workspace around it.
    ConfirmKillAgent(PaneId),
    /// Confirming a merge of a child workspace back into its project's trunk
    /// (`y` sends the merge, anything else cancels).
    ConfirmMerge(WorkspaceId),
    /// After a merge lands, offering to clean up (discard) the merged workspace
    /// (`y` discards it, anything else keeps it).
    ConfirmCleanup(WorkspaceId),
    Scroll(PaneId),
    Help,
    /// Navigating the sidebar; keys drive the selection instead of the pane.
    Sidebar,
    /// Editing the add-project directory prompt at the sidebar's foot.
    SidebarPrompt,
    /// Guided workspace creation, step 1: the `workspace name:` field.
    SidebarWorkspaceName,
    /// Guided workspace creation, step 2: the `where:` destination field,
    /// prefilled with the sibling default and offering directory completion.
    SidebarWorkspaceDest,
    /// The agent launcher overlay: pick what to run in a pane.
    Launcher,
    /// The launcher's free-form command input.
    LauncherCommand,
}

pub struct App {
    pub session: String,
    pub workspaces: Vec<WorkspaceView>,
    pub panes: HashMap<PaneId, PaneState>,
    pub active_tab: Option<TabId>,
    pub focused: Option<PaneId>,
    pub mode: Mode,
    pub zoom: bool,
    pub should_quit: bool,
    /// Pane rectangles from the last size sync, in content-area coordinates —
    /// used for mouse hit-testing and directional focus.
    pub rects: Vec<(PaneId, Rect)>,
    status: Option<(String, Instant)>,
    requested_sizes: HashMap<PaneId, (u16, u16)>,
    /// The focus last reported to the server via `PaneFocus`; lets the event
    /// loop notice a focus change from any source (input, layout, attach).
    focus_sent: Option<PaneId>,
    /// Set when a non-focused pane finished or blocked; the event loop rings the
    /// terminal bell once and clears it.
    bell: bool,
    /// When the prefix was pressed, while awaiting the follow-up key. Drives the
    /// which-key popup's delayed appearance.
    prefix_since: Option<Instant>,
    /// The highlighted sidebar entry while the sidebar is focused.
    sidebar_selected: usize,
    /// The selected row's identity and owning project, captured whenever the
    /// selection moves. `set_view` re-anchors the cursor through this after
    /// every layout change — same row, else its project's row, else a clamped
    /// index — so a kill or state change never silently retargets the cursor.
    sidebar_anchor: Option<(sidebar::EntryIdent, WorkspaceId)>,
    /// The directory being typed while in `SidebarPrompt` mode (add project).
    sidebar_prompt: String,
    /// Directory completions for the current `sidebar_prompt`, recomputed on
    /// every edit so the render path only reads them (never touches the fs).
    prompt_completions: Vec<String>,
    /// The highlighted completion row; `Tab` fills it, the arrows move it.
    prompt_selected: usize,
    /// The launcher rows, built when the launcher opens so render and dispatch
    /// read one list.
    launcher: Vec<LauncherRow>,
    /// The highlighted launcher row.
    launcher_selected: usize,
    /// Whether the launcher opened right after add-project — `esc` there spawns
    /// the shell into the new workspace (the old outcome) rather than closing.
    launcher_after_add: bool,
    /// The command line typed in the launcher's `command…` input.
    launcher_command: String,
    /// When set, the next launch first creates a fresh tab in this workspace
    /// (`TabNew` then the run): the sidebar's new-agent flow, where an agent
    /// gets its own tab rather than splitting an existing one.
    launcher_new_tab: Option<WorkspaceId>,
    /// When set, the next launch replaces this exited pane: run in the corpse's
    /// tab, then kill the corpse. Armed by `r` on a focused exited pane.
    launcher_replace: Option<PaneId>,
    /// The name of the workspace the open launcher targets, shown in its title
    /// so the choice's destination is never ambiguous. `None` renders a bare
    /// ` run ` title.
    launcher_target: Option<String>,
    /// The root the resume harvest reads the agent tools' session stores under
    /// — the real home directory, pointed at a fixture tree in tests.
    resume_home: Option<PathBuf>,
    /// The sidebar column from the last size sync, for mouse hit-testing.
    sidebar_rect: Option<Rect>,
    /// The top tab-bar row from the last size sync, for mouse hit-testing.
    tab_bar_rect: Option<Rect>,
    /// When the client attached — the epoch the working spinner advances from,
    /// so every working agent shares one frame counter.
    spinner_epoch: Instant,
    /// The content width from the last size sync, so the sidebar key can refuse
    /// to focus a column too narrow to render.
    last_content_width: u16,
    /// Set after creating a workspace so the next view adopts the server's newly
    /// current tab — the "jump to it" that follows a new-workspace prompt.
    adopt_active_view: bool,
    /// The origin workspace a pending guided-create prompt forks from — the
    /// source of the `WorkspaceFork` request submitted from the two-step prompt.
    fork_target: Option<WorkspaceId>,
    /// The workspace name captured in step 1, carried while the `where:` step is
    /// edited so submit can send both name and destination.
    new_workspace_name: String,
    /// Set while a `WorkspaceFork` is in flight so the `WorkspaceCreated` reply
    /// (shared with add-project) knows to jump to the new workspace and open its
    /// launcher.
    fork_pending: bool,
    /// The child workspace a `WorkspaceMerge` is in flight for, so the `Merged`
    /// reply knows which workspace the follow-up cleanup confirm targets.
    merge_pending: Option<WorkspaceId>,
    /// Panes that raised a notification while unfocused; their sidebar entry
    /// shows a bell mark until the pane is focused.
    notified: HashSet<PaneId>,
    /// Whether the projects / agents / waiting sidebar sections are collapsed to
    /// their header. Toggled by clicking a section header.
    collapsed_projects: bool,
    collapsed_waiting: bool,
    /// Whether the real terminal advertises truecolor (`COLORTERM`), gating the
    /// chrome background shades. Resolved once at startup; `false` in tests.
    truecolor: bool,
    /// Escape sequences queued for the real terminal (bell + OSC 9 re-emit), so
    /// the user's own terminal raises a desktop notification. Drained by the
    /// event loop.
    terminal_out: Vec<Vec<u8>>,
    /// Whether the terminal should be capturing mouse events. Starts at the
    /// config's master switch; the mouse-toggle prefix key flips it so the
    /// terminal's own drag-to-select and copy work while released. The event
    /// loop syncs the real capture state to this.
    mouse_capture: bool,
    /// Prefix chord, direct bindings, and the active prefix keymap.
    config: Config,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        Self {
            session: String::new(),
            workspaces: Vec::new(),
            panes: HashMap::new(),
            active_tab: None,
            focused: None,
            mode: Mode::Terminal,
            zoom: false,
            should_quit: false,
            rects: Vec::new(),
            status: None,
            requested_sizes: HashMap::new(),
            focus_sent: None,
            bell: false,
            prefix_since: None,
            sidebar_selected: 0,
            sidebar_anchor: None,
            sidebar_prompt: String::new(),
            prompt_completions: Vec::new(),
            prompt_selected: 0,
            launcher: Vec::new(),
            launcher_selected: 0,
            launcher_after_add: false,
            launcher_command: String::new(),
            launcher_new_tab: None,
            launcher_replace: None,
            launcher_target: None,
            resume_home: std::env::var_os("HOME").map(PathBuf::from),
            sidebar_rect: None,
            tab_bar_rect: None,
            spinner_epoch: Instant::now(),
            last_content_width: 0,
            adopt_active_view: false,
            fork_target: None,
            new_workspace_name: String::new(),
            fork_pending: false,
            merge_pending: None,
            notified: HashSet::new(),
            terminal_out: Vec::new(),
            collapsed_projects: false,
            collapsed_waiting: false,
            truecolor: false,
            mouse_capture: config.mouse,
            config,
        }
    }

    /// Whether the terminal should be capturing mouse events right now.
    pub fn mouse_capture(&self) -> bool {
        self.mouse_capture
    }

    /// The active configuration, for the renderer (hint, which-key, help).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Record whether the real terminal advertises truecolor. The chrome shades
    /// are only drawn when this is set (and the config enables them).
    pub fn set_truecolor(&mut self, on: bool) {
        self.truecolor = on;
    }

    /// Whether the chrome background shades should be drawn: the config enables
    /// them and the terminal can render a 24-bit colour.
    pub fn chrome_shaded(&self) -> bool {
        self.config.chrome_background && self.truecolor
    }

    /// Arm the first-run prompt: open the sidebar's new-project prompt prefilled
    /// with `dir`, so an empty session asks where to start instead of assuming
    /// the cwd. Reuses the sidebar prompt machinery — Enter creates the
    /// workspace + shell pane, Esc drops to the (empty) sidebar creating nothing.
    pub fn start_first_run_prompt(&mut self, dir: String) {
        self.mode = Mode::SidebarPrompt;
        self.sidebar_prompt = dir;
        self.prompt_selected = 0;
        self.refresh_completions();
    }

    /// Post a transient status line — e.g. a startup-project error surfaced once
    /// the client has attached.
    pub fn note(&mut self, message: String) {
        self.set_status(message);
    }

    /// Whether the which-key popup should be shown: the prefix has been held,
    /// unanswered, past the delay.
    pub fn whichkey_visible(&self) -> bool {
        matches!(self.mode, Mode::Prefix)
            && self
                .prefix_since
                .is_some_and(|since| since.elapsed() >= WHICHKEY_DELAY)
    }

    /// The current working-spinner frame index. The event loop watches this to
    /// redraw while an agent works; the renderer reads `spinner_char`.
    pub fn spinner_frame(&self) -> usize {
        (self.spinner_epoch.elapsed().as_millis() / SPINNER_TICK.as_millis()) as usize
            % SPINNER.len()
    }

    /// The braille glyph for the current spinner frame.
    pub fn spinner_char(&self) -> char {
        SPINNER[self.spinner_frame()]
    }

    /// Whether any pane is a working agent — the condition under which the event
    /// loop must keep redrawing to animate the spinner.
    pub fn has_working_agent(&self) -> bool {
        self.panes
            .values()
            .any(|s| s.info.agent.is_some() && s.info.state == AgentState::Working)
    }

    /// The `PaneFocus` frame to send if focus changed since the last call, so
    /// the server can mark the newly-focused pane seen (`Done → Idle`) and track
    /// the active pane. Returns `None` when focus is unchanged.
    pub fn focus_change(&mut self) -> Option<WireFrame> {
        if self.focus_sent == self.focused {
            return None;
        }
        self.focus_sent = self.focused;
        // A pane gaining focus clears its pending notification mark.
        if let Some(pane) = self.focused {
            self.notified.remove(&pane);
        }
        self.focused
            .map(|pane| control(&Request::PaneFocus { pane }))
    }

    /// Whether a bell is pending (a non-focused pane just blocked or finished),
    /// clearing the flag.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Whether `pane` has an unseen notification (drives the sidebar bell mark).
    pub fn is_notified(&self, pane: PaneId) -> bool {
        self.notified.contains(&pane)
    }

    /// Drain the escape sequences queued for the real terminal — bell and OSC 9
    /// re-emits — for the event loop to write to stdout.
    pub fn take_terminal_out(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.terminal_out)
    }

    // ---- inbound frames -------------------------------------------------

    /// Apply one frame received from the server, mutating client state.
    pub fn handle_frame(&mut self, frame: WireFrame) {
        match frame {
            WireFrame::Control(json) => self.handle_control(&json),
            WireFrame::PaneSnapshot(data) => {
                let info = self
                    .panes
                    .get(&data.pane)
                    .map(|s| s.info.clone())
                    .unwrap_or_else(|| placeholder_info(data.pane));
                self.panes.insert(
                    data.pane,
                    PaneState::seeded(data.rows, data.cols, &data.bytes, info),
                );
            }
            WireFrame::PaneDelta(data) => {
                if let Some(state) = self.panes.get_mut(&data.pane)
                    && state.scroll.is_none()
                {
                    state.parser.process(&data.bytes);
                    state.damaged = true;
                }
            }
            WireFrame::Input { .. } => {}
        }
    }

    fn handle_control(&mut self, json: &[u8]) {
        if let Ok(event) = serde_json::from_slice::<Event>(json) {
            self.handle_event(event);
        } else if let Ok(response) = serde_json::from_slice::<Response>(json) {
            self.handle_response(response);
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::LayoutChanged { workspaces } => self.set_view(workspaces),
            Event::StateChanged { pane, from, to } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.info.state = to;
                }
                // Ring once when a pane that was working needs attention or
                // finished while the user was looking elsewhere.
                let needs_attention = matches!(to, AgentState::Blocked | AgentState::Done)
                    && from == AgentState::Working;
                if needs_attention && self.focused != Some(pane) {
                    self.bell = true;
                }
            }
            Event::PaneExited { pane, code } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.info.exited = Some(code);
                    state.info.state = AgentState::Done;
                }
            }
            Event::PaneNotification { pane, title, body } => {
                self.on_notification(pane, title, body)
            }
            Event::PaneOutput { .. } => {}
        }
    }

    /// Surface a pane notification. The focused pane is skipped entirely (the
    /// user is already looking). For a background pane: always mark its sidebar
    /// entry, and — when notifications are enabled — flash the status bar and
    /// re-emit to the real terminal so the OS raises a desktop notification.
    fn on_notification(&mut self, pane: PaneId, title: Option<String>, body: Option<String>) {
        if self.focused == Some(pane) {
            return;
        }
        self.notified.insert(pane);
        if !self.config.notifications {
            return;
        }
        let pane_title = self
            .panes
            .get(&pane)
            .map(|s| s.info.title.clone())
            .unwrap_or_else(|| pane.to_string());
        let text = notification_text(title, body);
        match &text {
            Some(text) => self.set_status(format!("{pane_title}: {text}")),
            None => self.set_status("bell".into()),
        }
        // Re-emit a bell, plus an OSC 9 desktop notification when there is text.
        self.terminal_out.push(vec![0x07]);
        if let Some(text) = text {
            self.terminal_out
                .push(osc9(&format!("{pane_title}: {text}")));
        }
    }

    fn handle_response(&mut self, response: Response) {
        match response {
            Response::Attached {
                session,
                workspaces,
                wire_rev,
            } => {
                self.session = session;
                self.set_view(workspaces);
                if wire_rev != tutti_core::WIRE_REV {
                    self.note(format!(
                        "daemon is running an older build (wire rev {wire_rev}, client {}) — `tutti server stop`, reinstall, and reattach",
                        tutti_core::WIRE_REV
                    ));
                }
            }
            Response::PaneCreated { id } => {
                self.focused = Some(id);
                self.zoom = false;
            }
            Response::TabCreated { id } => {
                self.active_tab = Some(id);
                self.zoom = false;
                self.refocus();
            }
            Response::WorkspaceCreated { id } => {
                // Add-project ignores this (it armed its jump + launcher up
                // front); guided create waited for it: jump to the new workspace,
                // open the launcher over it, and flash what it made and where.
                if std::mem::take(&mut self.fork_pending) {
                    let created = self
                        .workspaces
                        .iter()
                        .find(|w| w.id == id)
                        .map(|w| (w.name.clone(), w.dir.clone()));
                    if !self.jump_to_workspace(id) {
                        // The fresh view has not landed yet; adopt on the next.
                        self.adopt_active_view = true;
                    }
                    self.open_launcher(
                        false,
                        created.as_ref().map(|(name, _)| name.clone()),
                        created.as_ref().map(|(_, dir)| dir.as_path()),
                    );
                    if let Some((name, dir)) = created {
                        self.set_status(format!("workspace {name} → {}", dir.display()));
                    }
                }
            }
            Response::Merged { pushed, bookmark } => {
                // The merge landed: report where, then offer to clean up (discard)
                // the now-merged workspace.
                if let Some(id) = self.merge_pending.take() {
                    let mut msg = format!("merged into {bookmark}");
                    if pushed {
                        msg.push_str(" and pushed");
                    }
                    msg.push_str(" — clean up workspace? y/N");
                    self.set_status(msg);
                    self.mode = Mode::ConfirmCleanup(id);
                }
            }
            Response::Error { message } => {
                // A failed new-workspace, guided-create, or merge request must not
                // later hijack the tab, the launcher, or the cleanup confirm.
                self.adopt_active_view = false;
                self.fork_pending = false;
                self.merge_pending = None;
                self.set_status(format!("error: {message}"));
            }
            _ => {}
        }
    }

    /// Reconcile the pane/tab tree with a fresh view: update pane metadata, drop
    /// vanished panes, add placeholders for new ones, and keep focus valid.
    fn set_view(&mut self, workspaces: Vec<WorkspaceView>) {
        self.workspaces = workspaces;
        let mut present: HashSet<PaneId> = HashSet::new();
        for info in self.workspaces.iter().flat_map(tab_infos) {
            present.insert(info.id);
            match self.panes.get_mut(&info.id) {
                Some(state) => state.info = info.clone(),
                None => {
                    self.panes.insert(info.id, empty_pane_state(info.clone()));
                }
            }
        }
        self.panes.retain(|id, _| present.contains(id));
        self.requested_sizes.retain(|id, _| present.contains(id));

        // After a new-workspace prompt, follow the server's freshly-current tab.
        if std::mem::take(&mut self.adopt_active_view) {
            self.active_tab = self.flagged_active_tab().or(self.active_tab);
        }
        if !self.active_tab.is_some_and(|t| self.tab_exists(t)) {
            self.active_tab = self.flagged_active_tab().or_else(|| self.first_tab());
        }
        // Re-anchor the cursor by identity, never by position: the same row if
        // it survived, else its project's row (a killed agent falls to its
        // project), else the old index clamped into range.
        let sidebar = self.sidebar();
        let resolved = self.sidebar_anchor.and_then(|(ident, project)| {
            sidebar
                .index_of(ident)
                .or_else(|| sidebar.index_of(sidebar::EntryIdent::Workspace(project)))
        });
        self.sidebar_selected =
            resolved.unwrap_or(self.sidebar_selected.min(sidebar.len().saturating_sub(1)));
        self.sidebar_anchor = anchor_at(&sidebar, self.sidebar_selected);
        self.refocus();
    }

    // ---- keyboard -------------------------------------------------------

    /// Handle a key press, returning frames to send to the server.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match self.mode {
            Mode::Terminal => self.on_key_terminal(key),
            Mode::Prefix => self.on_key_prefix(key),
            Mode::ConfirmKill(pane) => self.on_key_confirm(key, pane),
            Mode::ConfirmKillWorkspace(id) => self.on_key_confirm_workspace(key, id),
            Mode::ConfirmKillAgent(pane) => self.on_key_confirm_agent(key, pane),
            Mode::ConfirmMerge(id) => self.on_key_confirm_merge(key, id),
            Mode::ConfirmCleanup(id) => self.on_key_confirm_cleanup(key, id),
            Mode::Scroll(pane) => self.on_key_scroll(key, pane),
            Mode::Sidebar => self.on_key_sidebar(key),
            Mode::SidebarPrompt => self.on_key_prompt(key),
            Mode::SidebarWorkspaceName => self.on_key_workspace_name(key),
            Mode::SidebarWorkspaceDest => self.on_key_workspace_dest(key),
            Mode::Launcher => self.on_key_launcher(key),
            Mode::LauncherCommand => self.on_key_launcher_command(key),
            Mode::Help => {
                self.mode = Mode::Terminal;
                Vec::new()
            }
        }
    }

    fn on_key_terminal(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if self.config.prefix.matches(key) {
            self.mode = Mode::Prefix;
            self.prefix_since = Some(Instant::now());
            return Vec::new();
        }
        // Direct bindings intercept their chords before they reach the pane.
        if let Some(action) = self.config.keys.action_for(key) {
            return self.direct_action(action);
        }
        // An exited pane has no child to type at; the two keys its title
        // advertises act on the corpse instead: relaunch or close.
        if let Some(pane) = self.focused
            && key.modifiers.is_empty()
            && self
                .panes
                .get(&pane)
                .is_some_and(|s| s.info.exited.is_some())
        {
            match key.code {
                KeyCode::Char('r') => return self.relaunch_exited(pane),
                KeyCode::Char('x') => return vec![control(&Request::PaneKill { pane })],
                _ => {}
            }
        }
        match (self.focused, input::encode_key(key)) {
            (Some(pane), Some(bytes)) => vec![WireFrame::Input { pane, bytes }],
            // No pane to type into: honor the dashboard's advertised key so
            // `a → add a project` works without focusing the sidebar first.
            (None, _) if key.code == KeyCode::Char('a') && key.modifiers.is_empty() => {
                self.mode = Mode::Sidebar;
                self.open_project_prompt();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Perform a direct-binding action: directional focus (with edge-to-tab
    /// wrap on left/right), split resize, or killing the focused pane.
    fn direct_action(&mut self, action: Action) -> Vec<WireFrame> {
        match action {
            Action::FocusLeft => self.focus_or_tab(FocusDir::Left),
            Action::FocusRight => self.focus_or_tab(FocusDir::Right),
            Action::FocusUp => {
                self.move_focus(FocusDir::Up);
                Vec::new()
            }
            Action::FocusDown => {
                self.move_focus(FocusDir::Down);
                Vec::new()
            }
            Action::ResizeLeft => self.resize_split(Direction::Horizontal, -RESIZE_DELTA),
            Action::ResizeRight => self.resize_split(Direction::Horizontal, RESIZE_DELTA),
            Action::ResizeUp => self.resize_split(Direction::Vertical, -RESIZE_DELTA),
            Action::ResizeDown => self.resize_split(Direction::Vertical, RESIZE_DELTA),
            Action::KillPane => self.request_kill(),
        }
    }

    fn on_key_prefix(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        self.mode = Mode::Terminal;
        self.prefix_since = None;
        self.status = None;
        // Esc (and any key while the which-key popup is up) leaves prefix mode.
        if key.code == KeyCode::Esc {
            return Vec::new();
        }
        // The prefix pressed twice forwards the literal prefix chord to the pane.
        if self.config.prefix.matches(key) {
            return match (self.focused, input::encode_key(key)) {
                (Some(pane), Some(bytes)) => vec![WireFrame::Input { pane, bytes }],
                _ => Vec::new(),
            };
        }
        match self.config.prefix_action(key.code) {
            Some(action) => self.prefix_action(action),
            None => {
                self.set_status(format!(
                    "unknown prefix key: {}",
                    config::key_label(key.code)
                ));
                Vec::new()
            }
        }
    }

    /// Perform a prefix-mode action from the active keymap.
    fn prefix_action(&mut self, action: PrefixAction) -> Vec<WireFrame> {
        match action {
            PrefixAction::SplitRight => self.split(Direction::Horizontal),
            PrefixAction::SplitDown => self.split(Direction::Vertical),
            PrefixAction::FocusLeft => {
                self.move_focus(FocusDir::Left);
                Vec::new()
            }
            PrefixAction::FocusDown => {
                self.move_focus(FocusDir::Down);
                Vec::new()
            }
            PrefixAction::FocusUp => {
                self.move_focus(FocusDir::Up);
                Vec::new()
            }
            PrefixAction::FocusRight => {
                self.move_focus(FocusDir::Right);
                Vec::new()
            }
            PrefixAction::FocusCycle => {
                self.focus_cycle();
                Vec::new()
            }
            PrefixAction::KillPane => self.request_kill(),
            PrefixAction::Zoom => {
                if self.focused.is_some() {
                    self.zoom = !self.zoom;
                }
                Vec::new()
            }
            PrefixAction::Scrollback => self.enter_scroll(),
            PrefixAction::TabNext => self.switch_tab(1),
            PrefixAction::TabPrev => self.switch_tab(-1),
            PrefixAction::TabNew => self.new_tab(),
            PrefixAction::Sidebar => {
                self.focus_sidebar();
                Vec::new()
            }
            PrefixAction::Run => {
                let ws = self
                    .active_workspace()
                    .map(|w| (w.name.clone(), w.dir.clone()));
                self.open_launcher(
                    false,
                    ws.as_ref().map(|(name, _)| name.clone()),
                    ws.as_ref().map(|(_, dir)| dir.as_path()),
                );
                Vec::new()
            }
            PrefixAction::MouseToggle => {
                self.mouse_capture = !self.mouse_capture;
                self.set_status(if self.mouse_capture {
                    "mouse on".into()
                } else {
                    "mouse off — select/copy with the terminal".into()
                });
                Vec::new()
            }
            PrefixAction::Detach => self.detach(),
            PrefixAction::Help => {
                self.mode = Mode::Help;
                Vec::new()
            }
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent, pane: PaneId) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Terminal;
                self.status = None;
                vec![control(&Request::PaneKill { pane })]
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.mode = Mode::Terminal;
                self.status = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Resolve the sidebar workspace-kill confirm: `y` kills, `D` kills and
    /// discards the fork's checkout, any other key cancels. Either way the
    /// sidebar keeps focus; the view refresh drops a killed row.
    fn on_key_confirm_workspace(&mut self, key: KeyEvent, id: WorkspaceId) -> Vec<WireFrame> {
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
    fn on_key_confirm_agent(&mut self, key: KeyEvent, pane: PaneId) -> Vec<WireFrame> {
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

    fn on_key_scroll(&mut self, key: KeyEvent, pane: PaneId) -> Vec<WireFrame> {
        let page = self.inner_rows(pane).max(1) as usize;
        let current = self.panes.get(&pane).and_then(|s| s.scroll).unwrap_or(0);
        let offset = match key.code {
            KeyCode::Up => current + 1,
            KeyCode::Down => current.saturating_sub(1),
            KeyCode::PageUp => current + page,
            KeyCode::PageDown => current.saturating_sub(page),
            KeyCode::Char('q') | KeyCode::Esc => return self.exit_scroll(pane),
            _ => return Vec::new(),
        };
        self.set_scroll(pane, Some(offset));
        self.set_status(format!("scroll -{offset} (q to exit)"));
        vec![control(&Request::PaneScroll { pane, offset })]
    }

    // ---- sidebar --------------------------------------------------------

    /// The sidebar as it currently renders — the project tree then the waiting
    /// queue — for the renderer, hit-testing, and navigation. Carries the
    /// client's collapse state so the frame headers and row math agree.
    pub fn sidebar(&self) -> Sidebar {
        let mut sidebar = sidebar::build(&self.workspaces, self.active_tab);
        sidebar.projects_collapsed = self.collapsed_projects;
        sidebar.waiting_collapsed = self.collapsed_waiting;
        sidebar
    }

    /// The agent-state census across every project — the app bar's chips.
    pub fn agent_census(&self) -> sidebar::AgentCensus {
        sidebar::census(&self.workspaces)
    }

    /// The workspace owning the active tab — the footer's context segment.
    pub fn active_workspace(&self) -> Option<&WorkspaceView> {
        let at = self.active_tab?;
        self.workspaces
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == at))
    }

    /// The top-level project owning the active tab: the owning workspace's
    /// parent when that parent is in the view, else the workspace itself.
    fn active_project(&self) -> Option<WorkspaceId> {
        let at = self.active_tab?;
        let ws = self
            .workspaces
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == at))?;
        Some(
            ws.parent
                .filter(|p| self.workspaces.iter().any(|o| o.id == *p))
                .unwrap_or(ws.id),
        )
    }

    /// Whether the sidebar currently holds keyboard focus. The sidebar-raised
    /// confirms count: while one is open the agents filter and the selection
    /// highlight must not shift under the question being asked.
    pub fn sidebar_focused(&self) -> bool {
        matches!(
            self.mode,
            Mode::Sidebar
                | Mode::SidebarPrompt
                | Mode::SidebarWorkspaceName
                | Mode::SidebarWorkspaceDest
                | Mode::ConfirmKillWorkspace(_)
                | Mode::ConfirmKillAgent(_)
                | Mode::ConfirmMerge(_)
                | Mode::ConfirmCleanup(_)
        )
    }

    pub fn sidebar_selected(&self) -> usize {
        self.sidebar_selected
    }

    /// Whether a foot-of-sidebar prompt (add-project or guided workspace create)
    /// is being edited — either overlays the frame and suppresses the selection
    /// highlight.
    pub fn sidebar_prompt_active(&self) -> bool {
        matches!(
            self.mode,
            Mode::SidebarPrompt | Mode::SidebarWorkspaceName | Mode::SidebarWorkspaceDest
        )
    }

    /// The label prefix for the active foot prompt.
    pub fn sidebar_prompt_label(&self) -> &'static str {
        match self.mode {
            Mode::SidebarWorkspaceName => "workspace name: ",
            Mode::SidebarWorkspaceDest => "where: ",
            _ => "open: ",
        }
    }

    /// Whether the active foot prompt shows directory completions — the
    /// add-project prompt and the `where:` step do; the `workspace name:` step
    /// (a bare identifier, not a path) does not.
    pub fn sidebar_prompt_shows_completions(&self) -> bool {
        matches!(self.mode, Mode::SidebarPrompt | Mode::SidebarWorkspaceDest)
    }

    pub fn sidebar_prompt(&self) -> &str {
        &self.sidebar_prompt
    }

    /// The directory completions shown under the add-project prompt.
    pub fn prompt_completions(&self) -> &[String] {
        &self.prompt_completions
    }

    /// The highlighted completion row index.
    pub fn prompt_selected(&self) -> usize {
        self.prompt_selected
    }

    /// Whether the sidebar should be drawn for a content area of `total_width`.
    /// A focused sidebar always shows (so the sidebar key can reveal a hidden
    /// one); otherwise the config mode decides — `auto` showing it once the
    /// session is worth surfacing. Always suppressed below the width floor.
    fn sidebar_shown(&self, total_width: u16) -> bool {
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

    /// Split a content area into the sidebar column (when shown) and the region
    /// to its right. The single source of truth for the sidebar column: the
    /// region right of it is further split into the tab bar and panes.
    fn split_content(&self, content: Rect) -> (Option<Rect>, Rect) {
        if !self.sidebar_shown(content.width) {
            return (None, content);
        }
        let w = SIDEBAR_WIDTH.min(content.width);
        let sidebar = Rect::new(content.x, content.y, w, content.height);
        let panes = Rect::new(content.x + w, content.y, content.width - w, content.height);
        (Some(sidebar), panes)
    }

    /// The two regions a content area splits into: the sidebar column (when
    /// shown, full height) and the pane area to its right. The tab list now lives
    /// in the top app bar, so the content region no longer carries a tab-bar row.
    /// Every rect the renderer, resize sync, and mouse hit-testing use flows from
    /// here so they stay in agreement.
    pub fn regions(&self, content: Rect) -> (Option<Rect>, Rect) {
        self.split_content(content)
    }

    /// Split the terminal `area` vertically into the content region (between the
    /// top app-bar header and the footer) and the one-row footer — the single
    /// source of truth for that split, shared by the renderer and the resize
    /// sync. The app bar and its rule claim `HEADER_ROWS` at the top.
    pub fn content_rect(area: Rect) -> (Rect, Rect) {
        let top = HEADER_ROWS.min(area.height);
        let content_h = area.height.saturating_sub(HEADER_ROWS + 1);
        let content = Rect::new(area.x, area.y + top, area.width, content_h);
        let footer = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(1),
            area.width,
            1,
        );
        (content, footer)
    }

    /// The top app-bar row (full width) and the dim rule beneath it. Derived from
    /// the terminal `area`; the tab segments and the wordmark render onto the app
    /// bar, the content region begins below the rule.
    pub fn header_rects(area: Rect) -> (Rect, Rect) {
        let app_bar = Rect::new(area.x, area.y, area.width, 1);
        let rule = Rect::new(area.x, area.y + 1.min(area.height), area.width, 1);
        (app_bar, rule)
    }

    /// The right-aligned tab-segment region on the app-bar row, sized to the tab
    /// chips. Shared by the renderer and the click hit-test.
    pub fn tab_bar_rect(&self, app_bar: Rect) -> Rect {
        let w = self.tab_bar_width().min(app_bar.width);
        Rect::new(app_bar.x + app_bar.width.saturating_sub(w), app_bar.y, w, 1)
    }

    /// The rendered width of the tab segments: the chip labels plus one-column
    /// separators between them.
    fn tab_bar_width(&self) -> u16 {
        let chips = self.tab_chips();
        let labels: usize = chips.iter().map(|(_, l)| l.chars().count()).sum();
        let seps = chips.len().saturating_sub(1);
        (labels + seps) as u16
    }

    /// Focus the sidebar, revealing it if hidden. Refuses when the terminal is
    /// too narrow to render it, so focus is never trapped on an invisible panel.
    fn focus_sidebar(&mut self) {
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
            .and_then(|p| sidebar.index_of(sidebar::EntryIdent::Workspace(p)))
            .unwrap_or(0);
        self.select_entry(&sidebar, idx);
        self.prefix_since = None;
        self.status = None;
    }

    /// Move the sidebar cursor to `idx`, capturing its identity anchor so the
    /// next layout change re-finds the same row rather than the same position.
    fn select_entry(&mut self, sidebar: &Sidebar, idx: usize) {
        self.sidebar_selected = idx;
        self.sidebar_anchor = anchor_at(sidebar, idx);
    }

    fn on_key_sidebar(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        // The directional direct bindings cross the sidebar↔pane edge:
        // focus_right returns to the pane area (keeping its focus), focus_left
        // steps to the previous tab — the same wrap the pane's left edge does.
        match self.config.keys.action_for(key) {
            Some(Action::FocusRight) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return Vec::new();
            }
            Some(Action::FocusLeft) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return self.switch_tab(-1);
            }
            _ => {}
        }
        let sidebar = self.sidebar();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_entry(&sidebar, next_visible(&sidebar, self.sidebar_selected));
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_entry(&sidebar, prev_visible(&sidebar, self.sidebar_selected));
                Vec::new()
            }
            KeyCode::Enter => self.jump_to_selected(&sidebar),
            KeyCode::Char('n') => self.new_agent_in_selected(&sidebar),
            KeyCode::Char('a') => {
                self.open_project_prompt();
                Vec::new()
            }
            KeyCode::Char('d') => self.open_diff_pane(&sidebar),
            KeyCode::Char('w') => {
                self.open_workspace_prompt(&sidebar);
                Vec::new()
            }
            KeyCode::Char('m') => {
                self.confirm_merge(&sidebar);
                Vec::new()
            }
            KeyCode::Char('u') => self.update_selected(&sidebar),
            KeyCode::Char('x') => {
                self.confirm_kill_selected(&sidebar);
                Vec::new()
            }
            KeyCode::Esc => {
                self.mode = Mode::Terminal;
                self.status = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Jump to the selected entry and hand focus back to the pane: a workspace
    /// selects its tab; an agent selects its tab and focuses its pane (the
    /// `PaneFocus` follows from `focus_change`).
    fn jump_to_selected(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        self.mode = Mode::Terminal;
        self.status = None;
        match sidebar.entries.get(self.sidebar_selected) {
            Some(SidebarEntry::Workspace(w)) => {
                self.active_tab = Some(w.jump_tab);
                self.zoom = false;
                self.refocus();
                vec![control(&Request::TabSelect { id: w.jump_tab })]
            }
            Some(SidebarEntry::Agent(a)) => {
                self.active_tab = Some(a.tab);
                self.zoom = false;
                self.focused = Some(a.pane);
                vec![control(&Request::TabSelect { id: a.tab })]
            }
            None => Vec::new(),
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
        self.launcher_new_tab = Some(workspace);
        vec![control(&Request::TabSelect { id: tab })]
    }

    /// Jump to workspace `id`'s tab if the current view carries it, returning
    /// whether it did. Selects the workspace's active-or-first tab and hands
    /// focus to its pane, mirroring a workspace-row jump; used after a fork lands.
    fn jump_to_workspace(&mut self, id: WorkspaceId) -> bool {
        let Some(tab) = self
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .and_then(|w| w.tabs.iter().find(|t| t.active).or_else(|| w.tabs.first()))
            .map(|t| t.id)
        else {
            return false;
        };
        self.active_tab = Some(tab);
        self.zoom = false;
        self.refocus();
        true
    }

    /// Open the selected workspace's jj diff in an ephemeral pane — a real
    /// terminal running `jj diff | less -R`, removed the moment `less` quits. A
    /// workspace entry targets its own tab; an agent entry targets the tab it
    /// lives in. A non-jj workspace shows a transient error instead of spawning,
    /// since jj is the required VCS for workspace diffs.
    fn open_diff_pane(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        let tab = match sidebar.entries.get(self.sidebar_selected) {
            Some(SidebarEntry::Workspace(w)) => w.jump_tab,
            Some(SidebarEntry::Agent(a)) => a.tab,
            None => return Vec::new(),
        };
        let Some(dir) = self.workspace_dir_of_tab(tab) else {
            return Vec::new();
        };
        if !is_jj_workspace(&dir) {
            self.set_status(format!("not a jj workspace: {}", dir.display()));
            return Vec::new();
        }
        self.mode = Mode::Terminal;
        self.status = None;
        self.active_tab = Some(tab);
        self.zoom = false;
        vec![control(&Request::PaneRun {
            tab: Some(tab),
            cmd: vec![
                "sh".into(),
                "-lc".into(),
                "jj --no-pager diff --color=always | less -R".into(),
            ],
            ephemeral: true,
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
    fn selected_workspace(&self, sidebar: &Sidebar) -> Option<&WorkspaceView> {
        match sidebar.entries.get(self.sidebar_selected)? {
            SidebarEntry::Workspace(w) => self.workspaces.iter().find(|ws| ws.id == w.id),
            SidebarEntry::Agent(a) => self
                .workspaces
                .iter()
                .find(|w| w.tabs.iter().any(|t| t.id == a.tab)),
        }
    }

    /// The tab whose layout holds `pane`, if the current view carries it.
    fn tab_of_pane(&self, pane: PaneId) -> Option<TabId> {
        self.workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .find(|t| t.layout.as_ref().is_some_and(|l| l.panes().contains(&pane)))
            .map(|t| t.id)
    }

    /// Open the launcher over an exited pane's workspace, armed to replace the
    /// corpse — resume rows surface first, so a dead agent is one `r` and an
    /// enter from picking its conversation back up.
    fn relaunch_exited(&mut self, pane: PaneId) -> Vec<WireFrame> {
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
        self.launcher_replace = Some(pane);
        Vec::new()
    }

    /// Open guided workspace creation for the selected entry's workspace: step 1,
    /// the `workspace name:` field at the sidebar's foot. The origin is the
    /// selected workspace (an agent row targets the workspace that owns it). A
    /// no-op with nothing selected.
    fn open_workspace_prompt(&mut self, sidebar: &Sidebar) {
        let Some(id) = self.selected_workspace(sidebar).map(|w| w.id) else {
            return;
        };
        self.fork_target = Some(id);
        self.new_workspace_name.clear();
        self.mode = Mode::SidebarWorkspaceName;
        self.sidebar_prompt.clear();
        self.status = None;
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
        match sidebar.entries.get(self.sidebar_selected) {
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

    fn on_key_prompt(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.sidebar_prompt.push(c);
                self.prompt_selected = 0;
                self.refresh_completions();
                Vec::new()
            }
            KeyCode::Backspace => {
                self.sidebar_prompt.pop();
                self.prompt_selected = 0;
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
                if !self.prompt_completions.is_empty() {
                    self.prompt_selected =
                        (self.prompt_selected + 1).min(self.prompt_completions.len() - 1);
                }
                Vec::new()
            }
            KeyCode::Up => {
                self.prompt_selected = self.prompt_selected.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Esc => {
                self.mode = Mode::Sidebar;
                self.clear_prompt();
                self.status = None;
                Vec::new()
            }
            KeyCode::Enter => self.submit_prompt(),
            _ => Vec::new(),
        }
    }

    /// Open the add-project prompt on the sidebar `n` path, prefilled with the
    /// common parent of the existing workspaces so only the project name is left
    /// to type — the directory-completion panel fills in the rest.
    fn open_project_prompt(&mut self) {
        self.mode = Mode::SidebarPrompt;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let dirs: Vec<PathBuf> = self.workspaces.iter().map(|w| w.dir.clone()).collect();
        self.sidebar_prompt = prompt_prefill(&dirs, home.as_deref());
        self.prompt_selected = 0;
        self.status = None;
        self.refresh_completions();
    }

    /// Tab-complete the prompt to the highlighted directory, appending `/` so the
    /// next component's listing opens immediately. A no-op with no completions.
    fn complete_selection(&mut self) {
        let Some(name) = self.prompt_completions.get(self.prompt_selected).cloned() else {
            return;
        };
        let dir_part = match self.sidebar_prompt.rfind('/') {
            Some(i) => &self.sidebar_prompt[..=i],
            None => "",
        };
        self.sidebar_prompt = format!("{dir_part}{name}/");
        self.prompt_selected = 0;
        self.refresh_completions();
    }

    /// Recompute the directory completions for the current input against the live
    /// environment. Kept off the render path: every edit calls this, and the
    /// renderer only reads the cached result.
    fn refresh_completions(&mut self) {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.prompt_completions = complete_dirs(&self.sidebar_prompt, home.as_deref(), &cwd);
        if self.prompt_selected >= self.prompt_completions.len() {
            self.prompt_selected = 0;
        }
    }

    fn clear_prompt(&mut self) {
        self.sidebar_prompt.clear();
        self.prompt_completions.clear();
        self.prompt_selected = 0;
    }

    /// Submit the add-project prompt: mount the typed directory as a workspace,
    /// arm the jump to the new tab, then open the launcher to pick its first
    /// pane (esc there spawns the shell, preserving bare `tutti`'s outcome).
    /// A typed path that is not on disk falls back to the highlighted
    /// completion; with no completion either, the prompt stays open with a
    /// transient rather than mounting a dead directory.
    fn submit_prompt(&mut self) -> Vec<WireFrame> {
        let input = self.sidebar_prompt.trim().to_string();
        if input.is_empty() {
            self.clear_prompt();
            self.status = None;
            self.mode = Mode::Terminal;
            return Vec::new();
        }
        let mut dir = expand_dir(&input);
        if !dir.is_dir() {
            if self.prompt_completions.get(self.prompt_selected).is_none() {
                self.set_status(format!("no such directory: {input}"));
                return Vec::new();
            }
            self.complete_selection();
            dir = expand_dir(self.sidebar_prompt.trim());
        }
        self.clear_prompt();
        self.status = None;
        self.adopt_active_view = true;
        let name = workspace_name_from_dir(&dir);
        // Harvest against the directory being added: an existing project often
        // has conversations from before it was mounted in tutti.
        self.open_launcher(true, name, Some(dir.as_path()));
        vec![control(&Request::WorkspaceNew { dir })]
    }

    /// Guided create, step 1 (`workspace name:`): type the name, `esc` cancels
    /// back to the sidebar, `Enter` validates it and advances to the `where:`
    /// step. No directory completions — a workspace name is a bare identifier.
    fn on_key_workspace_name(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.sidebar_prompt.push(c);
                Vec::new()
            }
            KeyCode::Backspace => {
                self.sidebar_prompt.pop();
                Vec::new()
            }
            KeyCode::Esc => {
                self.mode = Mode::Sidebar;
                self.sidebar_prompt.clear();
                self.status = None;
                Vec::new()
            }
            KeyCode::Enter => self.advance_to_dest_step(),
            _ => Vec::new(),
        }
    }

    /// Validate the typed workspace name (fail fast, staying on step 1 with a
    /// transient naming the rule) and, when valid, advance to step 2: prefill the
    /// `where:` field with the sibling default and open its directory completion.
    fn advance_to_dest_step(&mut self) -> Vec<WireFrame> {
        let name = self.sidebar_prompt.trim().to_string();
        if !is_valid_fork_name(&name) {
            self.set_status("workspace name: letters, digits, '-' or '_' only".into());
            return Vec::new();
        }
        let dir = self
            .fork_target
            .and_then(|id| self.workspaces.iter().find(|w| w.id == id))
            .map(|w| w.dir.clone());
        self.new_workspace_name = name.clone();
        self.sidebar_prompt = dir
            .map(|dir| sibling_dest_prefill(&dir, &name))
            .unwrap_or_default();
        self.prompt_selected = 0;
        self.mode = Mode::SidebarWorkspaceDest;
        self.status = None;
        self.refresh_completions();
        Vec::new()
    }

    /// Guided create, step 2 (`where:`): edit the destination with the same
    /// directory completion the add-project prompt uses. `esc` steps back to the
    /// name field (restoring it), `Enter` submits the `WorkspaceFork`.
    fn on_key_workspace_dest(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.sidebar_prompt.push(c);
                self.prompt_selected = 0;
                self.refresh_completions();
                Vec::new()
            }
            KeyCode::Backspace => {
                self.sidebar_prompt.pop();
                self.prompt_selected = 0;
                self.refresh_completions();
                Vec::new()
            }
            KeyCode::Tab => {
                self.complete_selection();
                Vec::new()
            }
            KeyCode::Down => {
                if !self.prompt_completions.is_empty() {
                    self.prompt_selected =
                        (self.prompt_selected + 1).min(self.prompt_completions.len() - 1);
                }
                Vec::new()
            }
            KeyCode::Up => {
                self.prompt_selected = self.prompt_selected.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Esc => {
                // Back to step 1, restoring the name the user had typed.
                self.sidebar_prompt = std::mem::take(&mut self.new_workspace_name);
                self.prompt_completions.clear();
                self.prompt_selected = 0;
                self.mode = Mode::SidebarWorkspaceName;
                self.status = None;
                Vec::new()
            }
            KeyCode::Enter => self.submit_workspace(),
            _ => Vec::new(),
        }
    }

    /// Submit guided create: dispatch a `WorkspaceFork` for the origin workspace
    /// carrying the chosen name and destination (an empty field falls back to the
    /// server's sibling default), and arm the post-create jump + launcher
    /// (`WorkspaceCreated` completes it). A non-jj source is left to the server to
    /// reject — the single source of truth.
    fn submit_workspace(&mut self) -> Vec<WireFrame> {
        let Some(id) = self.fork_target else {
            return Vec::new();
        };
        let name = std::mem::take(&mut self.new_workspace_name);
        let input = self.sidebar_prompt.trim().to_string();
        let dest = (!input.is_empty()).then(|| expand_dest(&input));
        self.clear_prompt();
        self.mode = Mode::Sidebar;
        self.status = None;
        self.fork_pending = true;
        vec![control(&Request::WorkspaceFork {
            id,
            name,
            revision: None,
            dest,
        })]
    }

    /// Resolve the merge confirm: `y` dispatches the `WorkspaceMerge` (letting the
    /// server pick the trunk bookmark and push if a remote exists) and arms the
    /// cleanup confirm the `Merged` reply will raise; any other key cancels.
    fn on_key_confirm_merge(&mut self, key: KeyEvent, id: WorkspaceId) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.merge_pending = Some(id);
                self.mode = Mode::Sidebar;
                let name = self.workspace_name(id).unwrap_or_default();
                self.set_status(format!("merging {name}…"));
                vec![control(&Request::WorkspaceMerge { id, push: true })]
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
    fn on_key_confirm_cleanup(&mut self, key: KeyEvent, id: WorkspaceId) -> Vec<WireFrame> {
        self.mode = Mode::Sidebar;
        self.status = None;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                vec![control(&Request::WorkspaceKill { id, discard: true })]
            }
            _ => Vec::new(),
        }
    }

    /// Open the agent launcher — the "what should run here?" picker — building
    /// its rows from the registry and the live PATH. `after_add` records that it
    /// fired right after add-project, so `esc` still spawns the shell into the
    /// new workspace (today's outcome); otherwise `esc` just closes. `target`
    /// names the workspace the choice will land in, shown in the panel title;
    /// `dir` is its directory — when known, conversations harvested from the
    /// agent tools' own session stores append as resume rows at the foot.
    fn open_launcher(&mut self, after_add: bool, target: Option<String>, dir: Option<&Path>) {
        self.launcher =
            launcher::build_rows(&Registry::default(), std::env::var_os("PATH").as_deref());
        if let (Some(dir), Some(home)) = (dir, self.resume_home.as_deref()) {
            let sessions = tutti_agents::resume_sessions(dir, home, 3);
            let resume =
                launcher::resume_rows(&sessions, &self.launcher, std::time::SystemTime::now());
            // Resume rows sit between the fixed rows and the dim uninstalled
            // catalog, so the actionable part of the picker stays together.
            let at = launcher::catalog_start(&self.launcher);
            self.launcher.splice(at..at, resume);
        }
        self.launcher_selected = launcher::first_selectable(&self.launcher);
        self.launcher_after_add = after_add;
        self.launcher_command.clear();
        self.launcher_new_tab = None;
        self.launcher_replace = None;
        self.launcher_target = target;
        self.mode = Mode::Launcher;
        self.prefix_since = None;
        self.status = None;
    }

    /// The launcher rows as they currently render, for the renderer.
    pub fn launcher_rows(&self) -> &[LauncherRow] {
        &self.launcher
    }

    /// The highlighted launcher row index.
    pub fn launcher_selected(&self) -> usize {
        self.launcher_selected
    }

    /// The text typed in the launcher's `command…` input.
    pub fn launcher_command(&self) -> &str {
        &self.launcher_command
    }

    /// The launcher panel title, naming the workspace the choice will run in so
    /// the target is never ambiguous — ` run in <name> `, or a bare ` run ` when
    /// the target is unknown.
    pub fn launcher_title(&self) -> String {
        match &self.launcher_target {
            Some(name) => format!(" run in {name} "),
            None => " run ".into(),
        }
    }

    fn on_key_launcher(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.launcher_selected =
                    launcher::next_selectable(&self.launcher, self.launcher_selected);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.launcher_selected =
                    launcher::prev_selectable(&self.launcher, self.launcher_selected);
                Vec::new()
            }
            // Quick-select launches the numbered row outright.
            KeyCode::Char(c @ '1'..='9') => self.launch_index(c as usize - '1' as usize),
            KeyCode::Enter => self.launch_index(self.launcher_selected),
            KeyCode::Esc => {
                if self.launcher_after_add {
                    self.launch_shell_and_close()
                } else {
                    self.close_launcher();
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
        let kind = match self.launcher.get(idx) {
            Some(row) if row.selectable() => row.kind.clone(),
            _ => return Vec::new(),
        };
        match kind {
            LaunchKind::Agent(cmd) => self.launch_frames(vec![cmd]),
            LaunchKind::Shell => self.launch_shell_and_close(),
            LaunchKind::Command => {
                self.launcher_command.clear();
                self.mode = Mode::LauncherCommand;
                Vec::new()
            }
            LaunchKind::Resume(cmd) => self.launch_frames(cmd),
        }
    }

    fn launch_shell_and_close(&mut self) -> Vec<WireFrame> {
        self.launch_frames(vec![launcher::login_shell()])
    }

    /// Close the launcher and produce the frames that start `cmd` where it was
    /// aimed: a `TabNew` first when armed to give the choice its own tab, a run
    /// into an exited pane's tab followed by that corpse's kill when armed to
    /// replace it, else a plain run into the current tab. Requests share one
    /// ordered connection, so the run always lands after the tab exists and
    /// before the corpse goes.
    fn launch_frames(&mut self, cmd: Vec<String>) -> Vec<WireFrame> {
        let new_tab = self.launcher_new_tab.take();
        let replace = self.launcher_replace.take();
        self.close_launcher();
        if let Some(workspace) = new_tab {
            return vec![
                control(&Request::TabNew {
                    workspace: Some(workspace),
                }),
                run_pane(cmd),
            ];
        }
        if let Some(corpse) = replace
            && let Some(tab) = self.tab_of_pane(corpse)
        {
            return vec![
                control(&Request::PaneRun {
                    tab: Some(tab),
                    cmd,
                    ephemeral: false,
                }),
                control(&Request::PaneKill { pane: corpse }),
            ];
        }
        vec![run_pane(cmd)]
    }

    /// Dismiss the launcher back to terminal mode, dropping its transient state.
    fn close_launcher(&mut self) {
        self.mode = Mode::Terminal;
        self.launcher.clear();
        self.launcher_command.clear();
        self.launcher_after_add = false;
        self.launcher_new_tab = None;
        self.launcher_replace = None;
        self.launcher_target = None;
        self.status = None;
    }

    fn on_key_launcher_command(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.launcher_command.push(c);
                Vec::new()
            }
            KeyCode::Backspace => {
                self.launcher_command.pop();
                Vec::new()
            }
            // Esc backs up to the picker rather than closing outright.
            KeyCode::Esc => {
                self.launcher_command.clear();
                self.mode = Mode::Launcher;
                Vec::new()
            }
            KeyCode::Enter => {
                let input = self.launcher_command.trim().to_string();
                if input.is_empty() {
                    return Vec::new();
                }
                self.launch_frames(vec![launcher::login_shell(), "-lc".into(), input])
            }
            _ => Vec::new(),
        }
    }

    // ---- mouse ----------------------------------------------------------

    /// Handle a mouse event, returning frames to send to the server.
    pub fn on_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> Vec<WireFrame> {
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(pane) = self.pane_at(col, row) {
                    self.focused = Some(pane);
                    if matches!(self.mode, Mode::Prefix) {
                        self.mode = Mode::Terminal;
                        self.prefix_since = None;
                    }
                    return Vec::new();
                }
                if self.in_sidebar(col, row) {
                    return self.sidebar_click(row);
                }
                if self.tab_bar_rect.is_some_and(|r| contains(r, col, row)) {
                    return self.tab_bar_click(col);
                }
                Vec::new()
            }
            MouseEventKind::ScrollUp => self.mouse_scroll(col, row, true),
            MouseEventKind::ScrollDown => self.mouse_scroll(col, row, false),
            _ => Vec::new(),
        }
    }

    fn in_sidebar(&self, col: u16, row: u16) -> bool {
        self.sidebar_rect.is_some_and(|r| contains(r, col, row))
    }

    /// The app-bar tab segments, left to right: one ` <n> <name> ` per tab
    /// (carrying its id) then a trailing ` + ` (a `None` target = new tab) —
    /// space-padded so the active segment's background reads as a filled block.
    /// Shared by the renderer and the click hit-test so a click lands on exactly
    /// what is drawn; the renderer joins them with a one-column separator.
    pub fn tab_chips(&self) -> Vec<(Option<TabId>, String)> {
        let mut chips: Vec<(Option<TabId>, String)> = self
            .all_tabs()
            .iter()
            .enumerate()
            .map(|(i, t)| (Some(t.id), format!(" {} {} ", i + 1, t.name)))
            .collect();
        chips.push((None, " + ".into()));
        chips
    }

    /// A click on the tab bar: select the tab whose segment was hit, or create a
    /// new tab when the trailing `[+]` segment was hit. Segments are laid out from
    /// the bar's left edge with a one-column separator between them, matching the
    /// renderer.
    fn tab_bar_click(&mut self, col: u16) -> Vec<WireFrame> {
        let Some(rect) = self.tab_bar_rect else {
            return Vec::new();
        };
        let mut x = rect.x;
        for (target, label) in self.tab_chips() {
            let w = label.chars().count() as u16;
            if col >= x && col < x + w {
                return match target {
                    Some(id) => {
                        self.active_tab = Some(id);
                        self.zoom = false;
                        self.refocus();
                        vec![control(&Request::TabSelect { id })]
                    }
                    None => self.new_tab(),
                };
            }
            x += w + 1; // step past the segment and its separator column
        }
        Vec::new()
    }

    /// A left-click inside the sidebar: focus it, then act on what the row is — a
    /// section header toggles that section's collapse, an entry selects and jumps
    /// to it, and a border/blank just focuses.
    fn sidebar_click(&mut self, row: u16) -> Vec<WireFrame> {
        if self.sidebar_prompt_active() {
            return Vec::new();
        }
        if !self.sidebar_focused() {
            self.mode = Mode::Sidebar;
            self.sidebar_selected = 0;
            self.sidebar_anchor = None;
        }
        let Some(rect) = self.sidebar_rect else {
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

    /// Collapse or expand a sidebar section (projects, agents, or waiting).
    fn toggle_section(&mut self, section: sidebar::Section) {
        match section {
            sidebar::Section::Projects => self.collapsed_projects = !self.collapsed_projects,
            sidebar::Section::Waiting => self.collapsed_waiting = !self.collapsed_waiting,
        }
    }

    /// A wheel tick over a pane, routed by what the pane's program declared: a
    /// program that switched on mouse reporting gets the encoded wheel event
    /// itself; an alternate-screen program without mouse reporting has no
    /// scrollback to browse, so the wheel becomes arrow keys it can act on
    /// (the xterm "alternate scroll" convention); only a primary-screen pane
    /// enters tutti's frozen scrollback browse.
    fn mouse_scroll(&mut self, col: u16, row: u16, up: bool) -> Vec<WireFrame> {
        let Some(pane) = self.pane_at(col, row) else {
            return Vec::new();
        };
        self.focused = Some(pane);
        if self.panes.get(&pane).is_some_and(|s| s.scroll.is_none())
            && let Some(bytes) = self.forwarded_wheel(pane, col, row, up)
        {
            return vec![WireFrame::Input { pane, bytes }];
        }
        let current = self.panes.get(&pane).and_then(|s| s.scroll).unwrap_or(0);
        let offset = if up {
            current + MOUSE_SCROLL_STEP
        } else {
            current.saturating_sub(MOUSE_SCROLL_STEP)
        };
        if offset == 0 {
            if current == 0 {
                return Vec::new();
            }
            return self.exit_scroll(pane);
        }
        self.mode = Mode::Scroll(pane);
        self.set_scroll(pane, Some(offset));
        self.set_status(format!("scroll -{offset} (q to exit)"));
        vec![control(&Request::PaneScroll { pane, offset })]
    }

    /// The bytes a wheel tick sends straight to `pane`'s child, or `None` for
    /// a primary-screen pane whose scrollback tutti browses itself.
    fn forwarded_wheel(&self, pane: PaneId, col: u16, row: u16, up: bool) -> Option<Vec<u8>> {
        let screen = self.panes.get(&pane)?.parser.screen();
        if screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
            let rect = self
                .rects
                .iter()
                .find(|(p, _)| *p == pane)
                .map(|(_, r)| *r)?;
            // 1-based cell within the pane interior (the rect includes the
            // border row/column).
            let x = col.saturating_sub(rect.x).max(1);
            let y = row.saturating_sub(rect.y).max(1);
            return Some(input::encode_wheel(
                screen.mouse_protocol_encoding(),
                up,
                x,
                y,
            ));
        }
        screen
            .alternate_screen()
            .then(|| input::encode_wheel_arrows(screen.application_cursor(), up, MOUSE_SCROLL_STEP))
    }

    // ---- layout / sizing ------------------------------------------------

    /// Recompute pane rectangles for the terminal `area` and emit resize requests
    /// for any pane whose rendered size changed, so the server's ptys track the
    /// client. Records the sidebar and tab-bar rects for mouse hit-testing.
    pub fn sync_sizes(&mut self, area: Rect) -> Vec<WireFrame> {
        let (content, _footer) = App::content_rect(area);
        let (app_bar, _rule) = App::header_rects(area);
        self.last_content_width = content.width;
        let (sidebar_rect, _panes) = self.regions(content);
        self.sidebar_rect = sidebar_rect;
        self.tab_bar_rect = Some(self.tab_bar_rect(app_bar));
        self.rects = self.compute_rects(content);
        let mut out = Vec::new();
        for (pane, rect) in &self.rects {
            let (rows, cols) = inner_size(*rect);
            if self.requested_sizes.get(pane) != Some(&(rows, cols)) {
                self.requested_sizes.insert(*pane, (rows, cols));
                out.push(control(&Request::PaneResize {
                    pane: *pane,
                    rows,
                    cols,
                }));
            }
        }
        out
    }

    pub fn compute_rects(&self, content: Rect) -> Vec<(PaneId, Rect)> {
        let (_, panes) = self.regions(content);
        let Some(layout) = self.active_tab_view().and_then(|t| t.layout.as_ref()) else {
            return Vec::new();
        };
        let zoom = if self.zoom { self.focused } else { None };
        pane_rects(layout, panes, zoom)
    }

    // ---- helpers used by the render/event loop --------------------------

    /// The transient status message, if one is set and still fresh.
    pub fn transient(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|(msg, at)| (at.elapsed() < STATUS_TTL).then_some(msg.as_str()))
    }

    pub fn active_tab_view(&self) -> Option<&TabView> {
        let id = self.active_tab?;
        self.workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .find(|t| t.id == id)
    }

    pub fn all_tabs(&self) -> Vec<&TabView> {
        self.workspaces.iter().flat_map(|w| &w.tabs).collect()
    }

    /// The focused pane's screen contents as text, for assertions.
    pub fn pane_text(&self, pane: PaneId) -> Option<String> {
        self.panes.get(&pane).map(|s| s.parser.screen().contents())
    }

    // ---- internal actions ----------------------------------------------

    fn split(&mut self, direction: Direction) -> Vec<WireFrame> {
        match self.focused {
            Some(pane) => vec![control(&Request::PaneSplit { pane, direction })],
            None => {
                self.set_status("no pane to split".into());
                Vec::new()
            }
        }
    }

    fn switch_tab(&mut self, delta: isize) -> Vec<WireFrame> {
        let tabs: Vec<TabId> = self.all_tabs().iter().map(|t| t.id).collect();
        if tabs.is_empty() {
            return Vec::new();
        }
        let current = self
            .active_tab
            .and_then(|id| tabs.iter().position(|t| *t == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(tabs.len() as isize) as usize;
        let id = tabs[next];
        self.active_tab = Some(id);
        self.zoom = false;
        self.refocus();
        vec![control(&Request::TabSelect { id })]
    }

    fn new_tab(&mut self) -> Vec<WireFrame> {
        let workspace = self.active_workspace().map(|w| w.id);
        vec![control(&Request::TabNew { workspace })]
    }

    fn detach(&mut self) -> Vec<WireFrame> {
        self.should_quit = true;
        vec![control(&Request::Detach)]
    }

    fn enter_scroll(&mut self) -> Vec<WireFrame> {
        match self.focused {
            Some(pane) => {
                self.mode = Mode::Scroll(pane);
                self.set_scroll(pane, Some(0));
                self.set_status("scroll: up/down pgup/pgdn, q to exit".into());
                vec![control(&Request::PaneScroll { pane, offset: 0 })]
            }
            None => Vec::new(),
        }
    }

    fn exit_scroll(&mut self, pane: PaneId) -> Vec<WireFrame> {
        self.set_scroll(pane, None);
        self.mode = Mode::Terminal;
        self.status = None;
        vec![control(&Request::PaneScroll { pane, offset: 0 })]
    }

    fn focus_cycle(&mut self) {
        let panes = self.active_tab_panes();
        if panes.is_empty() {
            return;
        }
        let current = self
            .focused
            .and_then(|f| panes.iter().position(|p| *p == f))
            .unwrap_or(0);
        self.focused = Some(panes[(current + 1) % panes.len()]);
    }

    /// Move focus to the nearest pane in `dir` (overlapping axis, closest
    /// centre). Returns whether focus actually moved — `false` means the
    /// focused pane is already at that edge of the layout.
    fn move_focus(&mut self, dir: FocusDir) -> bool {
        let Some(current) = self.focused else {
            return false;
        };
        let Some(from) = self
            .rects
            .iter()
            .find(|(p, _)| *p == current)
            .map(|(_, r)| *r)
        else {
            return false;
        };
        let (fx, fy) = center(from);
        let best = self
            .rects
            .iter()
            .filter(|(p, _)| *p != current)
            .filter(|(_, r)| dir.accepts(from, *r))
            .min_by_key(|(_, r)| {
                let (cx, cy) = center(*r);
                (fx.abs_diff(cx) as u32).pow(2) + (fy.abs_diff(cy) as u32).pow(2)
            })
            .map(|(p, _)| *p);
        match best {
            Some(pane) => {
                self.focused = Some(pane);
                true
            }
            None => false,
        }
    }

    /// Directional focus that falls through at the left/right edge. Left steps
    /// into the sidebar when it is on screen (nvim-explorer parity), else wraps
    /// to the previous tab; right wraps to the next tab. Vertical edges are
    /// no-ops.
    fn focus_or_tab(&mut self, dir: FocusDir) -> Vec<WireFrame> {
        if self.move_focus(dir) {
            return Vec::new();
        }
        match dir {
            FocusDir::Left => {
                if self.sidebar_rect.is_some() {
                    self.focus_sidebar();
                    Vec::new()
                } else {
                    self.switch_tab(-1)
                }
            }
            FocusDir::Right => self.switch_tab(1),
            FocusDir::Up | FocusDir::Down => Vec::new(),
        }
    }

    /// Confirm-kill the focused pane, reusing the prefix-`x` flow.
    fn request_kill(&mut self) -> Vec<WireFrame> {
        match self.focused {
            Some(pane) => {
                self.mode = Mode::ConfirmKill(pane);
                self.set_status(format!("kill pane {pane}? (y/n)"));
            }
            None => self.set_status("no pane to kill".into()),
        }
        Vec::new()
    }

    /// Ask the server to nudge the focused pane's enclosing split ratio along
    /// `axis` by `delta`.
    fn resize_split(&mut self, axis: Direction, delta: f32) -> Vec<WireFrame> {
        match self.focused {
            Some(pane) => vec![control(&Request::PaneResizeSplit {
                pane,
                direction: axis,
                delta,
            })],
            None => Vec::new(),
        }
    }

    fn refocus(&mut self) {
        let panes = self.active_tab_panes();
        let keep = self.focused.filter(|f| panes.contains(f));
        self.focused = keep
            .or_else(|| {
                self.active_tab_view()
                    .and_then(|t| t.active_pane)
                    .filter(|p| panes.contains(p))
            })
            .or_else(|| panes.first().copied());
        if self.focused.is_none() {
            self.zoom = false;
        }
    }

    fn set_scroll(&mut self, pane: PaneId, offset: Option<usize>) {
        if let Some(state) = self.panes.get_mut(&pane) {
            state.scroll = offset;
            state.damaged = true;
        }
    }

    fn set_status(&mut self, message: String) {
        self.status = Some((message, Instant::now()));
    }

    fn active_tab_panes(&self) -> Vec<PaneId> {
        self.active_tab_view()
            .and_then(|t| t.layout.as_ref().map(Layout::panes))
            .unwrap_or_default()
    }

    /// The name of the workspace with `id`, from the current view.
    fn workspace_name(&self, id: WorkspaceId) -> Option<String> {
        self.workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.name.clone())
    }

    fn tab_exists(&self, id: TabId) -> bool {
        self.all_tabs().iter().any(|t| t.id == id)
    }

    fn flagged_active_tab(&self) -> Option<TabId> {
        self.all_tabs().iter().find(|t| t.active).map(|t| t.id)
    }

    fn first_tab(&self) -> Option<TabId> {
        self.all_tabs().first().map(|t| t.id)
    }

    fn pane_at(&self, col: u16, row: u16) -> Option<PaneId> {
        self.rects
            .iter()
            .find(|(_, r)| contains(*r, col, row))
            .map(|(p, _)| *p)
    }

    fn inner_rows(&self, pane: PaneId) -> u16 {
        self.rects
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| inner_size(*r).0)
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

impl FocusDir {
    /// Whether `to` lies in this direction from `from` (by rectangle position).
    fn accepts(self, from: Rect, to: Rect) -> bool {
        match self {
            FocusDir::Left => to.x + to.width <= from.x + 1,
            FocusDir::Right => to.x + 1 >= from.x + from.width,
            FocusDir::Up => to.y + to.height <= from.y + 1,
            FocusDir::Down => to.y + 1 >= from.y + from.height,
        }
    }
}

/// Whether the point `(col, row)` lies inside `r`.
fn contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

fn center(r: Rect) -> (u16, u16) {
    (r.x + r.width / 2, r.y + r.height / 2)
}

/// Inner (borderless) size of a pane rect as `(rows, cols)`, clamped to at
/// least 1 so a pty is never asked for a zero dimension. A pane rect reserves
/// its top row for the title line drawn above the frame, so height loses that
/// row plus the two border rows.
fn inner_size(rect: Rect) -> (u16, u16) {
    (
        rect.height.saturating_sub(3).max(1),
        rect.width.saturating_sub(2).max(1),
    )
}

pub(crate) fn control(request: &Request) -> WireFrame {
    WireFrame::Control(serde_json::to_vec(request).expect("serialize request"))
}

/// The workspace name a directory mounts under — its final path component — for
/// the launcher title shown before the new workspace's own view has landed.
fn workspace_name_from_dir(dir: &Path) -> Option<String> {
    dir.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// A `PaneRun` into the session's current tab (the just-created workspace after
/// add-project, or the active tab for prefix-`r`), spawning a persistent pane.
fn run_pane(cmd: Vec<String>) -> WireFrame {
    control(&Request::PaneRun {
        tab: None,
        cmd,
        ephemeral: false,
    })
}

/// Whether `dir` or an ancestor holds a `.jj` directory. A local mirror of the
/// server's jj probe so the sidebar diff key fails fast without a round-trip;
/// jj is the required VCS for workspace diffs.
fn is_jj_workspace(dir: &Path) -> bool {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".jj").exists() {
            return true;
        }
        cur = d.parent();
    }
    false
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

/// Flatten a notification's title/body into one human line: `title: body` when
/// both are present, otherwise whichever exists, or `None` for a bare bell.
fn notification_text(title: Option<String>, body: Option<String>) -> Option<String> {
    match (title, body) {
        (Some(t), Some(b)) => Some(format!("{t}: {b}")),
        (Some(t), None) => Some(t),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// An OSC 9 desktop-notification escape carrying `text`, BEL-terminated.
fn osc9(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 4);
    out.extend_from_slice(b"\x1b]9;");
    out.extend_from_slice(text.as_bytes());
    out.push(0x07);
    out
}

/// Resolve a prompt-entered directory against the live environment: `~`
/// expands to `$HOME`, relative paths against the client's cwd. The result is
/// canonicalized to an absolute path so the daemon (which has its own cwd)
/// records the real directory — without this, `.`-relative inputs land against
/// the daemon's cwd and its git-branch probe misses.
fn expand_dir(input: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let resolved = resolve_dir(input, home.as_deref(), &cwd);
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

/// Resolve a guided-create destination: expand `~`/relative like `expand_dir`,
/// then canonicalize the *parent* while keeping the (not-yet-existing) leaf, so
/// the daemon receives an absolute path whose parent is real. A missing parent is
/// left as-is for the server to reject.
fn expand_dest(input: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let resolved = resolve_dir(input, home.as_deref(), &cwd);
    match (resolved.parent(), resolved.file_name()) {
        (Some(parent), Some(leaf)) => std::fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(leaf),
        _ => resolved,
    }
}

/// The `where:` prefill for guided create: the sibling default
/// `<repo-parent>/<repo>-<name>`, home-shortened for display so the daemon still
/// canonicalizes the parent on submit. Falls back to a sibling of `dir` itself
/// when `dir` is not under a `.jj` repo (the server rejects a non-jj source).
fn sibling_dest_prefill(dir: &Path, name: &str) -> String {
    let root = jj_root(dir).unwrap_or_else(|| dir.to_path_buf());
    let base = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sibling = root
        .parent()
        .unwrap_or(&root)
        .join(format!("{base}-{name}"));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home
        && let Ok(rest) = sibling.strip_prefix(home)
    {
        return format!("~/{}", rest.display());
    }
    sibling.display().to_string()
}

/// The `.jj` repo root at or above `dir` — the client's mirror of the server's
/// `workspace_root`, so the guided-create prefill can name a sibling of the repo
/// without a round-trip.
fn jj_root(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".jj").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Pure directory resolution: `~`/`~/rest` against `home`, relative paths
/// against `cwd`, absolute paths untouched.
fn resolve_dir(input: &str, home: Option<&Path>, cwd: &Path) -> PathBuf {
    if let Some(home) = home {
        if input == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = input.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    let path = Path::new(input);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Directory completions for the add-project prompt: the sub-directories of the
/// input's parent whose names begin with its final path component (an empty
/// component lists every sub-directory). Directories only, dot-dirs hidden
/// unless the component itself starts with `.`, alphabetical, capped at
/// `MAX_COMPLETIONS`. An unreadable parent yields nothing, so a half-typed path
/// never flashes an error. Tilde/relative resolution matches `resolve_dir`.
fn complete_dirs(input: &str, home: Option<&Path>, cwd: &Path) -> Vec<String> {
    let (dir_part, comp) = match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    };
    let base = resolve_dir(dir_part, home, cwd);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|name| name.starts_with(comp) && (comp.starts_with('.') || !name.starts_with('.')))
        .collect();
    names.sort();
    names.truncate(MAX_COMPLETIONS);
    names
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

/// The next selectable entry at or after `from`, skipping entries whose section
/// is collapsed; stays on `from` when nothing visible lies ahead. With no
/// section collapsed this is a plain `from + 1` clamped to the last entry.
/// The identity anchor for entry `idx`: its ident plus the project it belongs
/// to (a workspace row's own project, an agent row's owner) — the fallback
/// target when the row itself vanishes.
fn anchor_at(sidebar: &Sidebar, idx: usize) -> Option<(sidebar::EntryIdent, WorkspaceId)> {
    let ident = sidebar.ident_at(idx)?;
    let project = match sidebar.entries.get(idx)? {
        SidebarEntry::Workspace(w) => w.project,
        SidebarEntry::Agent(a) => a.project,
    };
    Some((ident, project))
}

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

fn tab_infos(w: &WorkspaceView) -> impl Iterator<Item = &PaneInfo> {
    w.tabs.iter().flat_map(|t| &t.panes)
}

fn empty_pane_state(info: PaneInfo) -> PaneState {
    PaneState {
        parser: vt100::Parser::new(24, 80, SCROLLBACK),
        info,
        damaged: true,
        scroll: None,
    }
}

fn placeholder_info(pane: PaneId) -> PaneInfo {
    PaneInfo {
        id: pane,
        title: pane.to_string(),
        agent: None::<AgentKind>,
        state: AgentState::Unknown,
        exited: None,
        subagents: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::{agent, alt, ctrl, leaf, plain, shell, split, tab, workspace};
    use ratatui::crossterm::event::KeyModifiers;
    use tutti_core::PaneData;

    fn view_one_pane() -> Vec<WorkspaceView> {
        vec![workspace(
            1,
            "w",
            None,
            vec![tab(1, "1", true, leaf(1), vec![shell(1)])],
        )]
    }

    fn attached(app: &mut App) {
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "tutti".into(),
                workspaces: view_one_pane(),
            })
            .unwrap(),
        ));
    }

    #[test]
    fn attach_sets_focus_and_session() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(app.session, "tutti");
        assert_eq!(app.focused, Some(PaneId(1)));
        assert_eq!(app.active_tab, Some(TabId(1)));
    }

    #[test]
    fn snapshot_seeds_parser_then_delta_updates_it() {
        let mut app = App::new();
        attached(&mut app);
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: b"hello".to_vec(),
        }));
        assert!(app.pane_text(PaneId(1)).unwrap().contains("hello"));

        // A delta encoded by a vt100 parser transforms the client's screen.
        let mut src = vt100::Parser::new(24, 80, 0);
        src.process(b"hello");
        let before = src.screen().clone();
        src.process(b" world");
        let delta = src.screen().contents_diff(&before);
        app.handle_frame(WireFrame::PaneDelta(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 1,
            bytes: delta,
        }));
        assert!(app.pane_text(PaneId(1)).unwrap().contains("hello world"));
    }

    #[test]
    fn scroll_mode_ignores_deltas() {
        let mut app = App::new();
        attached(&mut app);
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: b"frozen".to_vec(),
        }));
        app.set_scroll(PaneId(1), Some(5));
        app.handle_frame(WireFrame::PaneDelta(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 1,
            bytes: b"\x1b[2Jlive".to_vec(),
        }));
        assert!(app.pane_text(PaneId(1)).unwrap().contains("frozen"));
    }

    #[test]
    fn typing_forwards_bytes_to_focused_pane() {
        let mut app = App::new();
        attached(&mut app);
        let out = app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: b"x".to_vec()
            }]
        );
    }

    #[test]
    fn prefix_split_emits_split_request() {
        let mut app = App::new();
        attached(&mut app);
        assert!(
            app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
                .is_empty()
        );
        assert_eq!(app.mode, Mode::Prefix);
        let out = app.on_key(KeyEvent::new(KeyCode::Char('%'), KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneSplit {
                pane: PaneId(1),
                direction: Direction::Horizontal,
            })]
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn prefix_m_toggles_mouse_capture() {
        let mut app = App::new();
        attached(&mut app);
        assert!(app.mouse_capture(), "capture starts at the config default");
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(
            app.on_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
                .is_empty()
        );
        assert!(!app.mouse_capture(), "prefix-m releases capture");
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(app.mouse_capture(), "prefix-m again re-grabs capture");
    }

    #[test]
    fn prefix_kill_confirms_then_sends() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::ConfirmKill(PaneId(1)));
        assert!(
            app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
                .is_empty()
        );
        assert_eq!(app.mode, Mode::Terminal);

        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let out = app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(out, vec![control(&Request::PaneKill { pane: PaneId(1) })]);
    }

    #[test]
    fn prefix_detach_quits() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        let out = app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(out, vec![control(&Request::Detach)]);
        assert!(app.should_quit);
    }

    #[test]
    fn zoom_toggles() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(app.zoom);
    }

    #[test]
    fn sync_sizes_requests_resize_once_per_size() {
        // Sidebar off so the lone pane keeps the full width. The chrome costs
        // four rows of the pane's height: the two-row app-bar header, the footer,
        // and the pane's own title line above its frame (plus the two borders).
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        attached(&mut app);
        let area = Rect::new(0, 0, 80, 24);
        let first = app.sync_sizes(area);
        assert_eq!(
            first,
            vec![control(&Request::PaneResize {
                pane: PaneId(1),
                rows: 18,
                cols: 78,
            })]
        );
        // Same size again produces nothing.
        assert!(app.sync_sizes(area).is_empty());
    }

    #[test]
    fn unknown_prefix_key_flashes_and_returns_to_terminal() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        let out = app.on_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));
        assert!(out.is_empty());
        assert_eq!(app.mode, Mode::Terminal);
        assert!(app.transient().unwrap().contains("unknown"));
    }

    fn view_two_panes() -> Vec<WorkspaceView> {
        vec![workspace(
            1,
            "w",
            None,
            vec![tab(
                1,
                "1",
                true,
                split(Direction::Horizontal, leaf(1), leaf(2)),
                vec![shell(1), shell(2)],
            )],
        )]
    }

    fn state_changed(app: &mut App, pane: PaneId, from: AgentState, to: AgentState) {
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::StateChanged { pane, from, to }).unwrap(),
        ));
    }

    #[test]
    fn focus_change_emits_panefocus_on_attach_then_dedupes() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(
            app.focus_change(),
            Some(control(&Request::PaneFocus { pane: PaneId(1) })),
            "attach should notify focus of the initial pane"
        );
        assert_eq!(app.focus_change(), None, "unchanged focus emits nothing");
    }

    #[test]
    fn bell_rings_only_for_nonfocused_attention_transitions() {
        let mut app = App::new();
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "t".into(),
                workspaces: view_two_panes(),
            })
            .unwrap(),
        ));
        app.focused = Some(PaneId(1));

        // A non-focused pane going Working -> Blocked rings once, then clears.
        state_changed(
            &mut app,
            PaneId(2),
            AgentState::Working,
            AgentState::Blocked,
        );
        assert!(app.take_bell());
        assert!(!app.take_bell());

        // Working -> Done on a non-focused pane also rings.
        state_changed(&mut app, PaneId(2), AgentState::Working, AgentState::Done);
        assert!(app.take_bell());

        // The focused pane's own transition never rings.
        state_changed(
            &mut app,
            PaneId(1),
            AgentState::Working,
            AgentState::Blocked,
        );
        assert!(!app.take_bell());

        // A non-attention transition (not from Working) never rings.
        state_changed(&mut app, PaneId(2), AgentState::Idle, AgentState::Working);
        assert!(!app.take_bell());
    }

    #[test]
    fn layout_change_removes_vanished_panes() {
        let mut app = App::new();
        attached(&mut app);
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: vec![] }).unwrap(),
        ));
        assert!(app.panes.is_empty());
        assert_eq!(app.focused, None);
    }

    fn attach_with(app: &mut App, workspaces: Vec<WorkspaceView>) {
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "t".into(),
                workspaces,
            })
            .unwrap(),
        ));
    }

    fn vim_config() -> Config {
        Config::parse("preset = \"vim\"\n").unwrap()
    }

    /// A 2x2 grid: left column stacks panes 1 (top) / 3 (bottom); right column
    /// stacks 2 (top) / 4 (bottom).
    fn view_2x2() -> Vec<WorkspaceView> {
        let column = |top, bottom| split(Direction::Vertical, leaf(top), leaf(bottom));
        let layout = split(Direction::Horizontal, column(1, 3), column(2, 4));
        vec![workspace(
            1,
            "w",
            None,
            vec![tab(1, "1", true, layout, (1..=4).map(shell).collect())],
        )]
    }

    fn view_two_tabs() -> Vec<WorkspaceView> {
        vec![workspace(
            1,
            "w",
            None,
            vec![
                tab(1, "1", true, leaf(1), vec![shell(1)]),
                tab(2, "2", false, leaf(2), vec![shell(2)]),
            ],
        )]
    }

    #[test]
    fn ctrl_l_moves_focus_to_the_right_pane() {
        let mut app = App::new();
        attach_with(&mut app, view_2x2());
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        assert_eq!(app.focused, Some(PaneId(1)));
        let out = app.on_key(ctrl('l'));
        assert!(out.is_empty(), "focus keys never forward bytes");
        assert_eq!(app.focused, Some(PaneId(2)));
    }

    #[test]
    fn ctrl_j_moves_focus_to_the_pane_below() {
        let mut app = App::new();
        attach_with(&mut app, view_2x2());
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        app.on_key(ctrl('j'));
        assert_eq!(app.focused, Some(PaneId(3)));
    }

    #[test]
    fn ctrl_l_on_rightmost_pane_switches_to_next_tab() {
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        assert_eq!(app.active_tab, Some(TabId(1)));
        let out = app.on_key(ctrl('l'));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.active_tab, Some(TabId(2)));
    }

    #[test]
    fn ctrl_h_on_leftmost_pane_switches_to_previous_tab_when_the_sidebar_is_hidden() {
        // With the sidebar off, the left edge keeps its old wrap-to-previous-tab
        // behaviour (the sidebar-visible path is covered separately below).
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        let out = app.on_key(ctrl('h'));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.active_tab, Some(TabId(2)));
        assert_eq!(app.mode, Mode::Terminal, "no sidebar to step into");
    }

    #[test]
    fn ctrl_h_at_the_left_edge_steps_into_the_sidebar_when_it_is_visible() {
        // The default sidebar is on and wide enough here, so the left edge lands
        // in the sidebar rather than wrapping to the previous tab.
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        assert_eq!(app.mode, Mode::Terminal);
        let out = app.on_key(ctrl('h'));
        assert!(
            out.is_empty(),
            "stepping into the sidebar emits no tab select"
        );
        assert_eq!(app.mode, Mode::Sidebar);
        assert_eq!(app.active_tab, Some(TabId(1)), "the tab is unchanged");
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
    fn ctrl_j_at_a_vertical_edge_is_a_noop() {
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        let out = app.on_key(ctrl('j'));
        assert!(out.is_empty());
        assert_eq!(app.focused, Some(PaneId(1)));
        assert_eq!(app.active_tab, Some(TabId(1)));
    }

    #[test]
    fn alt_x_confirms_kill_of_the_focused_pane() {
        let mut app = App::new();
        attached(&mut app);
        let out = app.on_key(alt('x'));
        assert!(out.is_empty());
        assert_eq!(app.mode, Mode::ConfirmKill(PaneId(1)));
    }

    #[test]
    fn alt_h_and_alt_l_send_horizontal_resize_requests() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(
            app.on_key(alt('l')),
            vec![control(&Request::PaneResizeSplit {
                pane: PaneId(1),
                direction: Direction::Horizontal,
                delta: RESIZE_DELTA,
            })]
        );
        assert_eq!(
            app.on_key(alt('h')),
            vec![control(&Request::PaneResizeSplit {
                pane: PaneId(1),
                direction: Direction::Horizontal,
                delta: -RESIZE_DELTA,
            })]
        );
    }

    #[test]
    fn alt_j_sends_a_vertical_resize_request() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(
            app.on_key(alt('j')),
            vec![control(&Request::PaneResizeSplit {
                pane: PaneId(1),
                direction: Direction::Vertical,
                delta: RESIZE_DELTA,
            })]
        );
    }

    #[test]
    fn ctrl_c_and_ctrl_d_forward_to_the_focused_pane() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(
            app.on_key(ctrl('c')),
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: vec![0x03],
            }],
            "Ctrl+C must reach the pane so interrupts still work"
        );
        assert_eq!(
            app.on_key(ctrl('d')),
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: vec![0x04],
            }],
            "Ctrl+D must reach the pane so EOF still works"
        );
    }

    #[test]
    fn whichkey_popup_appears_only_after_the_delay() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(ctrl('b'));
        assert_eq!(app.mode, Mode::Prefix);
        assert!(!app.whichkey_visible(), "hidden immediately after prefix");

        // Simulate the delay elapsing.
        app.prefix_since = Some(Instant::now() - WHICHKEY_DELAY - Duration::from_millis(10));
        assert!(app.whichkey_visible(), "shown once the delay passes");

        // A follow-up key dispatches and dismisses the popup.
        app.on_key(plain('z'));
        assert_eq!(app.mode, Mode::Terminal);
        assert!(!app.whichkey_visible());
    }

    #[test]
    fn esc_closes_prefix_without_acting() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(ctrl('b'));
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty());
        assert_eq!(app.mode, Mode::Terminal);
        assert!(app.prefix_since.is_none());
    }

    #[test]
    fn prefix_pressed_twice_forwards_the_prefix_byte() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(ctrl('b'));
        assert_eq!(
            app.on_key(ctrl('b')),
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: vec![0x02],
            }]
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn vim_preset_prefix_dispatch() {
        let mut app = App::with_config(vim_config());
        attached(&mut app);

        // v → split right
        app.on_key(ctrl('b'));
        assert_eq!(
            app.on_key(plain('v')),
            vec![control(&Request::PaneSplit {
                pane: PaneId(1),
                direction: Direction::Horizontal,
            })]
        );

        // q → kill pane (confirm), n cancels
        app.on_key(ctrl('b'));
        app.on_key(plain('q'));
        assert_eq!(app.mode, Mode::ConfirmKill(PaneId(1)));
        app.on_key(plain('n'));

        // d → detach (stays reachable in vim)
        app.on_key(ctrl('b'));
        assert_eq!(app.on_key(plain('d')), vec![control(&Request::Detach)]);
        assert!(app.should_quit);
    }

    // ---- sidebar --------------------------------------------------------

    /// Workspace `api` (tab 1, active, one shell) and `web` (tab 2, two agents:
    /// pane 2 working, pane 3 blocked).
    fn view_two_workspaces() -> Vec<WorkspaceView> {
        vec![
            workspace(
                1,
                "api",
                Some("main"),
                vec![tab(1, "1", true, leaf(1), vec![shell(1)])],
            ),
            workspace(
                2,
                "web",
                None,
                vec![tab(
                    2,
                    "2",
                    false,
                    split(Direction::Horizontal, leaf(2), leaf(3)),
                    vec![
                        agent(2, "claude", AgentState::Working),
                        agent(3, "claude", AgentState::Blocked),
                    ],
                )],
            ),
        ]
    }

    fn focus_sidebar(app: &mut App) {
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        app.on_key(ctrl('b'));
        app.on_key(plain('w'));
    }

    #[test]
    fn sidebar_visibility_follows_config_and_width() {
        fn auto() -> App {
            App::with_config(Config::parse("sidebar = \"auto\"\n").unwrap())
        }

        // auto: hidden for a single plain workspace.
        let mut app = auto();
        attached(&mut app);
        let (sb, panes) = app.split_content(Rect::new(0, 0, 100, 24));
        assert!(sb.is_none(), "auto hides an agentless single workspace");
        assert_eq!(panes.width, 100);

        // auto: shown with multiple workspaces, panes shrink by the column width.
        let mut app = auto();
        attach_with(&mut app, view_two_workspaces());
        let (sb, panes) = app.split_content(Rect::new(0, 0, 100, 24));
        assert_eq!(sb.unwrap().width, 30);
        assert_eq!(panes.width, 70);
        assert_eq!(panes.x, 30);

        // width floor suppresses it regardless of contents.
        let (sb, panes) = app.split_content(Rect::new(0, 0, 79, 24));
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
        assert_eq!(app.sidebar_selected(), 0);

        app.on_key(plain('j')); // second workspace — the filter follows it
        assert_eq!(app.sidebar_selected(), 1);
        app.on_key(plain('j')); // crosses into the agents section
        assert_eq!(app.sidebar_selected(), 2);
        assert!(matches!(app.sidebar().entries[2], SidebarEntry::Agent(_)));
        app.on_key(plain('j')); // last agent
        assert_eq!(app.sidebar_selected(), 3);
        app.on_key(plain('j')); // crosses into the waiting section
        assert_eq!(app.sidebar_selected(), 4);
        app.on_key(plain('j')); // clamped at the end
        assert_eq!(app.sidebar_selected(), 4);
        app.on_key(plain('k')); // back up into the agents
        assert_eq!(app.sidebar_selected(), 3);

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
        let SidebarEntry::Agent(a) = &sidebar.entries[app.sidebar_selected()] else {
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
            Some(control(&Request::PaneFocus { pane: PaneId(2) })),
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
    fn sidebar_n_launch_creates_a_tab_first_so_the_agent_never_splits() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('j')); // select web (workspace 2)
        app.on_key(plain('n'));
        assert_eq!(app.mode, Mode::Launcher);
        // Pin the rows so the test is independent of what PATH carries.
        app.launcher = vec![LauncherRow {
            name: "claude".into(),
            role: "Claude Code".into(),
            kind: LaunchKind::Agent("claude".into()),
            available: true,
        }];
        app.launcher_selected = 0;
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
        app.launcher = vec![LauncherRow {
            name: "claude".into(),
            role: "Claude Code".into(),
            kind: LaunchKind::Agent("claude".into()),
            available: true,
        }];
        app.launcher_selected = 0;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["claude".into()],
                ephemeral: false,
            })],
            "esc dropped the armed TabNew; C-b r still runs in place"
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
                cmd: vec![
                    "sh".into(),
                    "-lc".into(),
                    "jj --no-pager diff --color=always | less -R".into(),
                ],
                ephemeral: true,
            })],
            "d spawns the ephemeral jj-diff pane in the workspace's tab"
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

    // ---- guided workspace create / merge / update stale -----------------

    /// Decode the single control frame in `out` into a request, for asserting the
    /// shape of a submitted `WorkspaceFork` whose `dest` is environment-derived.
    fn only_request(out: &[WireFrame]) -> Request {
        match out {
            [WireFrame::Control(bytes)] => serde_json::from_slice(bytes).unwrap(),
            other => panic!("expected exactly one control frame, got {other:?}"),
        }
    }

    #[test]
    fn w_guided_create_walks_name_then_dest_and_submits() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app); // selection rests on the api workspace (id 1)

        // Step 1: `w` opens the name field, sending nothing yet.
        let out = app.on_key(plain('w'));
        assert!(out.is_empty(), "w opens the name prompt, sending nothing");
        assert_eq!(app.mode, Mode::SidebarWorkspaceName);
        for c in "feature".chars() {
            app.on_key(plain(c));
        }
        assert_eq!(app.sidebar_prompt(), "feature");

        // Enter validates the name and advances to the prefilled `where:` step.
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "advancing to the dest step sends nothing");
        assert_eq!(app.mode, Mode::SidebarWorkspaceDest);
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
    fn f_is_gone_from_the_sidebar_dispatch() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        let out = app.on_key(plain('f'));
        assert!(out.is_empty(), "f sends nothing");
        assert_eq!(app.mode, Mode::Sidebar, "f no longer opens any prompt");
    }

    #[test]
    fn w_starts_create_rather_than_unfocusing_the_sidebar() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('w'));
        assert_eq!(
            app.mode,
            Mode::SidebarWorkspaceName,
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
        assert_eq!(
            app.mode,
            Mode::SidebarWorkspaceName,
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
        assert_eq!(app.mode, Mode::SidebarWorkspaceDest);
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty(), "esc sends nothing");
        assert_eq!(
            app.mode,
            Mode::SidebarWorkspaceName,
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

        app.handle_response(Response::WorkspaceCreated { id: WorkspaceId(3) });
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

        // `y` dispatches the merge (push requested; the server owns the bookmark).
        let out = app.on_key(plain('y'));
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceMerge {
                id: WorkspaceId(2),
                push: true,
            })],
            "y sends the merge request"
        );

        // The Merged reply flashes the outcome and raises the cleanup confirm.
        app.handle_response(Response::Merged {
            pushed: false,
            bookmark: "main".into(),
        });
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
        app.handle_response(Response::Merged {
            pushed: true,
            bookmark: "main".into(),
        });
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
        app.handle_response(Response::Merged {
            pushed: false,
            bookmark: "main".into(),
        });
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
    fn is_valid_fork_name_matches_the_server_rule() {
        assert!(is_valid_fork_name("feature-1_x"));
        assert!(is_valid_fork_name("ABC"));
        assert!(!is_valid_fork_name(""));
        assert!(!is_valid_fork_name("has space"));
        assert!(!is_valid_fork_name("has/slash"));
        assert!(!is_valid_fork_name("dots.bad"));
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
        let SidebarEntry::Workspace(w) = &sidebar.entries[app.sidebar_selected()] else {
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
        let SidebarEntry::Workspace(w) = &sidebar.entries[app.sidebar_selected()] else {
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
        assert_eq!(app.sidebar_selected(), 3);

        // A killed workspace shrinks the view to a single agentless row.
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged {
                workspaces: view_one_pane(),
            })
            .unwrap(),
        ));
        assert_eq!(
            app.sidebar_selected(),
            0,
            "the selection clamps onto the surviving row"
        );
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

        for c in "api".chars() {
            app.on_key(plain(c));
        }
        app.on_key(plain('x'));
        assert_eq!(app.sidebar_prompt(), "/tmp/w/apix");
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.sidebar_prompt(), "/tmp/w/api");

        // Enter only mounts a directory that exists on disk, so make it real.
        std::fs::create_dir_all("/tmp/w/api").unwrap();
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let dir = std::fs::canonicalize("/tmp/w/api").unwrap();
        let _ = std::fs::remove_dir_all("/tmp/w/api");
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceNew { dir })],
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
            })],
            "esc after add-project spawns the shell into the new workspace"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn spinner_advances_only_with_a_working_agent() {
        let mut app = App::new();
        attached(&mut app); // a single shell, no agent
        assert!(!app.has_working_agent());
        assert!(SPINNER.contains(&app.spinner_char()));

        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        assert!(
            app.has_working_agent(),
            "the working claude agent drives the spinner"
        );
    }

    #[test]
    fn attached_with_old_wire_rev_warns_to_restart_the_daemon() {
        let mut app = App::new();
        app.handle_response(Response::Attached {
            session: "t".into(),
            workspaces: Vec::new(),
            wire_rev: 0,
        });
        let warning = app.transient().unwrap_or_default().to_string();
        assert!(
            warning.contains("older build"),
            "expected skew warning, got {warning:?}"
        );
        assert!(
            warning.contains("server stop"),
            "warning must say how to fix: {warning:?}"
        );
    }

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
            vec![control(&Request::WorkspaceNew { dir })],
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
    fn resolve_dir_expands_home_and_relative_paths() {
        let home = Path::new("/home/alice");
        let cwd = Path::new("/work/proj");
        assert_eq!(
            resolve_dir("~", Some(home), cwd),
            PathBuf::from("/home/alice")
        );
        assert_eq!(
            resolve_dir("~/src/api", Some(home), cwd),
            PathBuf::from("/home/alice/src/api")
        );
        assert_eq!(
            resolve_dir("sub/dir", Some(home), cwd),
            PathBuf::from("/work/proj/sub/dir")
        );
        assert_eq!(resolve_dir("/etc", Some(home), cwd), PathBuf::from("/etc"));
        assert_eq!(
            resolve_dir("~", None, cwd),
            PathBuf::from("/work/proj/~"),
            "without a home, ~ is treated as a relative path"
        );
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

    #[test]
    fn clicking_a_tab_chip_selects_it_and_plus_creates_a_tab() {
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        // The tab segments are right-aligned on the app-bar row (row 0). Labels
        // "[1 1]" "[2 2]" "[+]" total 15 cols with separators, so they start at
        // col 85: "[1 1]" 85-89, "[2 2]" 91-95, "[+]" 97-99.
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 93, 0);
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.active_tab, Some(TabId(2)));

        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 98, 0);
        assert_eq!(
            out,
            vec![control(&Request::TabNew {
                workspace: Some(WorkspaceId(1)),
            })],
            "the + segment creates a tab in the active workspace"
        );
    }

    // ---- mouse wheel ----------------------------------------------------

    /// Seed pane 1's parser with `bytes` so its declared modes (alt screen,
    /// mouse reporting) drive the wheel routing, and return its rect.
    fn wheel_fixture(app: &mut App, bytes: &[u8]) -> Rect {
        attached(app);
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 20,
            cols: 60,
            seq: 0,
            bytes: bytes.to_vec(),
        }));
        app.rects
            .iter()
            .find(|(p, _)| *p == PaneId(1))
            .map(|(_, r)| *r)
            .expect("pane 1 has a rect")
    }

    #[test]
    fn wheel_over_an_alt_screen_pane_sends_arrows_instead_of_freezing() {
        let mut app = App::new();
        let rect = wheel_fixture(&mut app, b"\x1b[?1049h");
        let out = app.on_mouse(MouseEventKind::ScrollUp, rect.x + 5, rect.y + 3);
        assert_eq!(
            out,
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: b"\x1b[A".repeat(MOUSE_SCROLL_STEP),
            }],
            "a full-screen TUI has no scrollback; the wheel becomes arrows"
        );
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "and never freezes into scroll mode"
        );
        let out = app.on_mouse(MouseEventKind::ScrollDown, rect.x + 5, rect.y + 3);
        assert_eq!(
            out,
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: b"\x1b[B".repeat(MOUSE_SCROLL_STEP),
            }]
        );
    }

    #[test]
    fn wheel_over_a_mouse_reporting_pane_forwards_the_event() {
        let mut app = App::new();
        // Mouse press reporting on, SGR encoding on.
        let rect = wheel_fixture(&mut app, b"\x1b[?1000h\x1b[?1006h");
        let out = app.on_mouse(MouseEventKind::ScrollUp, rect.x + 5, rect.y + 3);
        assert_eq!(
            out,
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: b"\x1b[<64;5;3M".to_vec(),
            }],
            "the program asked for mouse events, so it gets the wheel itself"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn wheel_over_a_primary_screen_pane_still_browses_scrollback() {
        let mut app = App::new();
        let rect = wheel_fixture(&mut app, b"plain shell output");
        let out = app.on_mouse(MouseEventKind::ScrollUp, rect.x + 2, rect.y + 2);
        assert_eq!(
            out,
            vec![control(&Request::PaneScroll {
                pane: PaneId(1),
                offset: MOUSE_SCROLL_STEP,
            })]
        );
        assert_eq!(app.mode, Mode::Scroll(PaneId(1)));
    }

    // ---- exited panes ---------------------------------------------------

    fn view_one_exited_agent() -> Vec<WorkspaceView> {
        let mut view = view_one_pane();
        view[0].tabs[0].panes[0].agent = Some("claude".into());
        view[0].tabs[0].panes[0].exited = Some(0);
        view
    }

    #[test]
    fn r_on_an_exited_pane_relaunches_into_its_tab_and_drops_the_corpse() {
        let mut app = App::new();
        attach_with(&mut app, view_one_exited_agent());
        assert_eq!(app.focused, Some(PaneId(1)));
        let out = app.on_key(plain('r'));
        assert!(out.is_empty(), "r opens the launcher, sending nothing yet");
        assert_eq!(app.mode, Mode::Launcher);
        app.launcher = vec![LauncherRow {
            name: "resume".into(),
            role: "claude · now · fix the thing".into(),
            kind: LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "abc".into()]),
            available: true,
        }];
        app.launcher_selected = 0;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![
                control(&Request::PaneRun {
                    tab: Some(TabId(1)),
                    cmd: vec!["claude".into(), "--resume".into(), "abc".into()],
                    ephemeral: false,
                }),
                control(&Request::PaneKill { pane: PaneId(1) }),
            ],
            "the pick runs in the corpse's tab, then the corpse goes"
        );
    }

    #[test]
    fn x_on_an_exited_pane_closes_it_outright() {
        let mut app = App::new();
        attach_with(&mut app, view_one_exited_agent());
        let out = app.on_key(plain('x'));
        assert_eq!(
            out,
            vec![control(&Request::PaneKill { pane: PaneId(1) })],
            "a corpse needs no kill confirm"
        );
    }

    #[test]
    fn r_at_a_live_pane_still_types_into_it() {
        let mut app = App::new();
        attached(&mut app);
        let out = app.on_key(plain('r'));
        assert_eq!(
            out,
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: b"r".to_vec(),
            }],
            "the corpse keys only intercept once the child is gone"
        );
    }

    // ---- notifications --------------------------------------------------

    fn notify(app: &mut App, pane: PaneId, title: Option<&str>, body: Option<&str>) {
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::PaneNotification {
                pane,
                title: title.map(Into::into),
                body: body.map(Into::into),
            })
            .unwrap(),
        ));
    }

    #[test]
    fn background_notification_marks_flashes_and_reemits() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        assert_eq!(app.focused, Some(PaneId(1)), "the shell pane is focused");

        notify(&mut app, PaneId(2), None, Some("build done"));
        assert!(app.is_notified(PaneId(2)), "the background pane is marked");
        assert!(
            app.transient().unwrap().contains("build done"),
            "the status bar flashes the body"
        );
        let out = app.take_terminal_out();
        assert_eq!(out.len(), 2, "a bell and an OSC 9 re-emit");
        assert_eq!(out[0], vec![0x07]);
        assert!(out[1].starts_with(b"\x1b]9;") && out[1].ends_with(&[0x07]));
        assert!(
            app.take_terminal_out().is_empty(),
            "draining clears the queue"
        );
    }

    #[test]
    fn bare_bell_notification_reemits_a_bell_only() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        notify(&mut app, PaneId(2), None, None);
        assert!(app.is_notified(PaneId(2)));
        assert_eq!(app.transient().unwrap(), "bell");
        assert_eq!(
            app.take_terminal_out(),
            vec![vec![0x07]],
            "no OSC 9 without text"
        );
    }

    #[test]
    fn notification_on_the_focused_pane_is_ignored() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        app.active_tab = Some(TabId(2));
        app.focused = Some(PaneId(2));
        notify(&mut app, PaneId(2), Some("Agent"), Some("hi"));
        assert!(!app.is_notified(PaneId(2)), "focused pane is not marked");
        assert!(app.transient().is_none(), "no flash for the focused pane");
        assert!(
            app.take_terminal_out().is_empty(),
            "no re-emit for the focused pane"
        );
    }

    #[test]
    fn notification_mark_clears_when_the_pane_gains_focus() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        notify(&mut app, PaneId(3), None, Some("blocked"));
        assert!(app.is_notified(PaneId(3)));

        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('j')); // web's first agent (pane 2)
        app.on_key(plain('j')); // its second: the blocked pane 3
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focused, Some(PaneId(3)));
        let _ = app.focus_change();
        assert!(
            !app.is_notified(PaneId(3)),
            "gaining focus clears the pane's mark"
        );
    }

    #[test]
    fn notifications_disabled_still_marks_the_sidebar() {
        let mut app = App::with_config(Config::parse("notifications = false\n").unwrap());
        attach_with(&mut app, view_two_workspaces());
        notify(&mut app, PaneId(2), None, Some("done"));
        assert!(app.is_notified(PaneId(2)), "the sidebar mark is always on");
        assert!(app.transient().is_none(), "no flash when disabled");
        assert!(
            app.take_terminal_out().is_empty(),
            "no re-emit when disabled"
        );
    }

    #[test]
    fn notification_text_combines_title_and_body() {
        assert_eq!(notification_text(None, None), None);
        assert_eq!(notification_text(None, Some("b".into())), Some("b".into()));
        assert_eq!(notification_text(Some("t".into()), None), Some("t".into()));
        assert_eq!(
            notification_text(Some("t".into()), Some("b".into())),
            Some("t: b".into())
        );
    }

    #[test]
    fn osc9_wraps_text_in_the_escape() {
        assert_eq!(osc9("hi"), b"\x1b]9;hi\x07".to_vec());
    }

    // ---- add-project prompt: prefill and completion -----------------------

    /// A throwaway directory tree seeded with `subdirs`, uniquely named so
    /// parallel test runs never collide.
    fn temp_tree(subdirs: &[&str]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("tutti-complete-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for d in subdirs {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        root
    }

    #[test]
    fn complete_dirs_lists_matching_subdirs_only() {
        let root = temp_tree(&["alpha", "altair", "beta", ".hidden"]);
        std::fs::write(root.join("apple.txt"), b"x").unwrap();
        let home = Path::new("/no-home");
        let cwd = Path::new("/no-cwd");

        // A prefix matches directories only — the `apple.txt` file is excluded.
        let got = complete_dirs(&format!("{}/al", root.display()), Some(home), cwd);
        assert_eq!(got, vec!["alpha".to_string(), "altair".to_string()]);

        // An empty component lists every visible sub-directory, alphabetical.
        let got = complete_dirs(&format!("{}/", root.display()), Some(home), cwd);
        assert_eq!(got, vec!["alpha", "altair", "beta"]);

        // A `.`-leading component reveals the dot-directory it would otherwise hide.
        let got = complete_dirs(&format!("{}/.", root.display()), Some(home), cwd);
        assert_eq!(got, vec![".hidden"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn complete_dirs_caps_at_eight_and_treats_unreadable_as_empty() {
        let many: Vec<String> = (0..12).map(|i| format!("d{i:02}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let root = temp_tree(&refs);

        let got = complete_dirs(&format!("{}/", root.display()), None, Path::new("/"));
        assert_eq!(got.len(), 8, "capped at eight matches");
        assert_eq!(got[0], "d00", "the first eight, alphabetical");
        assert_eq!(got[7], "d07");

        // A parent that cannot be read yields nothing rather than an error.
        let missing = format!("{}/nope/x", root.display());
        assert!(complete_dirs(&missing, None, Path::new("/")).is_empty());

        let _ = std::fs::remove_dir_all(&root);
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
            vec![control(&Request::WorkspaceNew { dir })],
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
        assert_eq!(app.prompt_completions(), &["the-librarian".to_string()]);

        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let dir = std::fs::canonicalize(root.join("the-librarian")).unwrap();
        assert_eq!(
            out,
            vec![control(&Request::WorkspaceNew { dir })],
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
        assert_eq!(app.prompt_selected(), 0);
        assert_eq!(app.prompt_completions(), &["gamma".to_string()]);

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- agent launcher -------------------------------------------------

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
            let names: Vec<&str> = app
                .launcher_rows()
                .iter()
                .map(|r| r.name.as_str())
                .collect();
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
        app.launcher = vec![LauncherRow {
            name: "claude".into(),
            role: "Claude Code".into(),
            kind: LaunchKind::Agent("claude".into()),
            available: true,
        }];
        app.launcher_selected = 0;
        app.mode = Mode::Launcher;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["claude".into()],
                ephemeral: false,
            })],
            "enter launches the selected agent in a new pane"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn launcher_enter_on_a_resume_row_emits_the_resume_argv() {
        let mut app = App::new();
        attached(&mut app);
        app.launcher = vec![LauncherRow {
            name: "resume".into(),
            role: "claude · 2h · fix the sidebar".into(),
            kind: LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "abc".into()]),
            available: true,
        }];
        app.launcher_selected = 0;
        app.mode = Mode::Launcher;
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec!["claude".into(), "--resume".into(), "abc".into()],
                ephemeral: false,
            })],
            "enter resumes the conversation in a new pane"
        );
        assert_eq!(app.mode, Mode::Terminal);
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
        app.resume_home = Some(home.clone());
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('n')); // new agent in the selected (first) workspace
        assert_eq!(app.mode, Mode::Launcher);
        let rows = app.launcher_rows();
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
        app.open_launcher(false, None, None);
        // The shell row follows the installed agents; find it, whatever this
        // machine has on PATH, and launch it by number.
        let shell_number = app
            .launcher_rows()
            .iter()
            .position(|r| matches!(r.kind, LaunchKind::Shell))
            .unwrap()
            + 1;
        let digit = char::from_digit(shell_number as u32, 10).unwrap();
        let out = app.on_key(plain(digit));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec![login_shell()],
                ephemeral: false,
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
        // Find the command row, whatever this machine has on PATH.
        let command_number = app
            .launcher_rows()
            .iter()
            .position(|r| matches!(r.kind, LaunchKind::Command))
            .unwrap()
            + 1;
        let digit = char::from_digit(command_number as u32, 10).unwrap();
        let out = app.on_key(plain(digit));
        assert!(out.is_empty(), "opening the command input emits nothing");
        assert_eq!(app.mode, Mode::LauncherCommand);

        for c in "npm test".chars() {
            app.on_key(plain(c));
        }
        assert_eq!(app.launcher_command(), "npm test");
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![control(&Request::PaneRun {
                tab: None,
                cmd: vec![login_shell(), "-lc".into(), "npm test".into()],
                ephemeral: false,
            })],
            "the command runs through the login shell's -lc"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    #[test]
    fn launcher_command_esc_backs_up_to_the_picker() {
        let mut app = App::new();
        attached(&mut app);
        app.open_launcher(false, None, None);
        let command_number = app
            .launcher_rows()
            .iter()
            .position(|r| matches!(r.kind, LaunchKind::Command))
            .unwrap()
            + 1;
        let digit = char::from_digit(command_number as u32, 10).unwrap();
        app.on_key(plain(digit));
        app.on_key(plain('x'));
        assert_eq!(app.mode, Mode::LauncherCommand);
        let out = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.is_empty());
        assert_eq!(app.mode, Mode::Launcher, "esc steps back to the picker");
        assert!(app.launcher_command().is_empty());
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
        app.launcher = vec![
            LauncherRow {
                name: "up".into(),
                role: "".into(),
                kind: LaunchKind::Agent("up".into()),
                available: true,
            },
            LauncherRow {
                name: "gone".into(),
                role: "".into(),
                kind: LaunchKind::Agent("gone".into()),
                available: false,
            },
            LauncherRow {
                name: "down".into(),
                role: "".into(),
                kind: LaunchKind::Agent("down".into()),
                available: true,
            },
        ];
        app.launcher_selected = 0;
        app.mode = Mode::Launcher;
        app.on_key(plain('j'));
        assert_eq!(app.launcher_selected(), 2, "j skips the unavailable row");
        app.on_key(plain('k'));
        assert_eq!(app.launcher_selected(), 0, "k skips it too");
    }
}
