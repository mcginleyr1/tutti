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

use super::input;
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
    Scroll(PaneId),
    Help,
    /// Navigating the sidebar; keys drive the selection instead of the pane.
    Sidebar,
    /// Editing the add-project directory prompt at the sidebar's foot.
    SidebarPrompt,
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
    /// The directory being typed while in `SidebarPrompt` mode (add project).
    sidebar_prompt: String,
    /// Directory completions for the current `sidebar_prompt`, recomputed on
    /// every edit so the render path only reads them (never touches the fs).
    prompt_completions: Vec<String>,
    /// The highlighted completion row; `Tab` fills it, the arrows move it.
    prompt_selected: usize,
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
    /// Panes that raised a notification while unfocused; their sidebar entry
    /// shows a bell mark until the pane is focused.
    notified: HashSet<PaneId>,
    /// Whether the projects / agents sidebar sections are collapsed to their
    /// header. Toggled by clicking a section header.
    collapsed_projects: bool,
    collapsed_agents: bool,
    /// Whether the real terminal advertises truecolor (`COLORTERM`), gating the
    /// chrome background shades. Resolved once at startup; `false` in tests.
    truecolor: bool,
    /// Escape sequences queued for the real terminal (bell + OSC 9 re-emit), so
    /// the user's own terminal raises a desktop notification. Drained by the
    /// event loop.
    terminal_out: Vec<Vec<u8>>,
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
            sidebar_prompt: String::new(),
            prompt_completions: Vec::new(),
            prompt_selected: 0,
            sidebar_rect: None,
            tab_bar_rect: None,
            spinner_epoch: Instant::now(),
            last_content_width: 0,
            adopt_active_view: false,
            notified: HashSet::new(),
            terminal_out: Vec::new(),
            collapsed_projects: false,
            collapsed_agents: false,
            truecolor: false,
            config,
        }
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
            } => {
                self.session = session;
                self.set_view(workspaces);
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
            Response::Error { message } => {
                // A failed new-workspace request must not later hijack the tab.
                self.adopt_active_view = false;
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
        self.refocus();
    }

    // ---- keyboard -------------------------------------------------------

    /// Handle a key press, returning frames to send to the server.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match self.mode {
            Mode::Terminal => self.on_key_terminal(key),
            Mode::Prefix => self.on_key_prefix(key),
            Mode::ConfirmKill(pane) => self.on_key_confirm(key, pane),
            Mode::Scroll(pane) => self.on_key_scroll(key, pane),
            Mode::Sidebar => self.on_key_sidebar(key),
            Mode::SidebarPrompt => self.on_key_prompt(key),
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
        match (self.focused, input::encode_key(key)) {
            (Some(pane), Some(bytes)) => vec![WireFrame::Input { pane, bytes }],
            // No pane to type into: honor the dashboard's advertised key so
            // `n → add a project` works without focusing the sidebar first.
            (None, _) if key.code == KeyCode::Char('n') && key.modifiers.is_empty() => {
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
            PrefixAction::Detach => self.detach(),
            PrefixAction::Help => {
                self.mode = Mode::Help;
                Vec::new()
            }
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent, pane: PaneId) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char('y') => {
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

    /// The sidebar as it currently renders — workspaces then agents — for the
    /// renderer, hit-testing, and navigation. Carries the client's collapse state
    /// so the frame headers and row math agree.
    pub fn sidebar(&self) -> Sidebar {
        let mut sidebar = sidebar::build(&self.workspaces, self.active_tab);
        sidebar.projects_collapsed = self.collapsed_projects;
        sidebar.agents_collapsed = self.collapsed_agents;
        sidebar
    }

    /// Whether the sidebar currently holds keyboard focus.
    pub fn sidebar_focused(&self) -> bool {
        matches!(self.mode, Mode::Sidebar | Mode::SidebarPrompt)
    }

    pub fn sidebar_selected(&self) -> usize {
        self.sidebar_selected
    }

    pub fn sidebar_prompt_active(&self) -> bool {
        matches!(self.mode, Mode::SidebarPrompt)
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
        self.sidebar_selected = 0;
        self.prefix_since = None;
        self.status = None;
    }

    fn on_key_sidebar(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        let sidebar = self.sidebar();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.sidebar_selected = next_visible(&sidebar, self.sidebar_selected);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.sidebar_selected = prev_visible(&sidebar, self.sidebar_selected);
                Vec::new()
            }
            KeyCode::Enter => self.jump_to_selected(&sidebar),
            KeyCode::Char('n') => {
                self.open_project_prompt();
                Vec::new()
            }
            KeyCode::Char('d') => self.open_diff_pane(&sidebar),
            KeyCode::Esc | KeyCode::Char('w') => {
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
            // highlight. Enter always submits whatever is typed (never the
            // completion), so a highlighted row is only ever taken via Tab.
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
    /// bootstrap a shell pane in it (matching bare `tutti`), and arm the jump to
    /// the new tab.
    fn submit_prompt(&mut self) -> Vec<WireFrame> {
        let input = self.sidebar_prompt.trim().to_string();
        self.clear_prompt();
        self.status = None;
        self.mode = Mode::Terminal;
        if input.is_empty() {
            return Vec::new();
        }
        let dir = expand_dir(&input);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        self.adopt_active_view = true;
        vec![
            control(&Request::WorkspaceNew { dir }),
            control(&Request::PaneRun {
                tab: None,
                cmd: vec![shell],
                ephemeral: false,
            }),
        ]
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

    /// The app-bar tab segments, left to right: one `[<n> <name>]` per tab
    /// (carrying its id) then a trailing `[+]` (a `None` target = new tab).
    /// Shared by the renderer and the click hit-test so a click lands on exactly
    /// what is drawn; the renderer joins them with a one-column separator.
    pub fn tab_chips(&self) -> Vec<(Option<TabId>, String)> {
        let mut chips: Vec<(Option<TabId>, String)> = self
            .all_tabs()
            .iter()
            .enumerate()
            .map(|(i, t)| (Some(t.id), format!("[{} {}]", i + 1, t.name)))
            .collect();
        chips.push((None, "[+]".into()));
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
        if matches!(self.mode, Mode::SidebarPrompt) {
            return Vec::new();
        }
        if !self.sidebar_focused() {
            self.mode = Mode::Sidebar;
            self.sidebar_selected = 0;
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
                self.sidebar_selected = idx;
                self.jump_to_selected(&sidebar)
            }
            None => Vec::new(),
        }
    }

    /// Collapse or expand a sidebar section (projects or agents).
    fn toggle_section(&mut self, section: sidebar::Section) {
        match section {
            sidebar::Section::Projects => self.collapsed_projects = !self.collapsed_projects,
            sidebar::Section::Agents => self.collapsed_agents = !self.collapsed_agents,
        }
    }

    fn mouse_scroll(&mut self, col: u16, row: u16, up: bool) -> Vec<WireFrame> {
        let Some(pane) = self.pane_at(col, row) else {
            return Vec::new();
        };
        self.focused = Some(pane);
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
        let workspace = self.active_workspace();
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

    /// Directional focus that falls through to the neighbouring tab when the
    /// focused pane is at the left/right edge (zellij-nav parity). Vertical
    /// edges are no-ops.
    fn focus_or_tab(&mut self, dir: FocusDir) -> Vec<WireFrame> {
        if self.move_focus(dir) {
            return Vec::new();
        }
        match dir {
            FocusDir::Left => self.switch_tab(-1),
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

    fn active_workspace(&self) -> Option<WorkspaceId> {
        let id = self.active_tab?;
        self.workspaces
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == id))
            .map(|w| w.id)
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
    fn ctrl_h_on_leftmost_pane_switches_to_previous_tab() {
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        let out = app.on_key(ctrl('h'));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
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

        app.on_key(plain('j')); // second workspace
        assert_eq!(app.sidebar_selected(), 1);
        app.on_key(plain('j')); // crosses into the agents section
        assert_eq!(app.sidebar_selected(), 2);
        assert!(matches!(app.sidebar().entries[2], SidebarEntry::Agent(_)));
        app.on_key(plain('j')); // last agent
        assert_eq!(app.sidebar_selected(), 3);
        app.on_key(plain('j')); // clamped at the end
        assert_eq!(app.sidebar_selected(), 3);
        app.on_key(plain('k')); // back up into the workspaces
        assert_eq!(app.sidebar_selected(), 2);

        // esc unfocuses; the pane keeps its own focus.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Terminal);
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
        app.on_key(plain('j')); // first agent: blocked pane 3, in tab 2
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
        assert_eq!(app.active_tab, Some(TabId(2)));
        assert_eq!(app.focused, Some(PaneId(3)));
        // The PaneFocus follows from the focus-change the event loop drains.
        assert_eq!(
            app.focus_change(),
            Some(control(&Request::PaneFocus { pane: PaneId(3) })),
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

    #[test]
    fn new_project_prompt_prefills_the_common_parent_then_submits() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        focus_sidebar(&mut app);
        app.on_key(plain('n'));
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

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![
                control(&Request::WorkspaceNew {
                    dir: PathBuf::from("/tmp/w/api"),
                }),
                control(&Request::PaneRun {
                    tab: None,
                    cmd: vec![shell],
                    ephemeral: false,
                }),
            ],
            "submit mounts the typed directory then bootstraps a shell pane"
        );
        assert_eq!(app.mode, Mode::Terminal);
        assert!(app.sidebar_prompt().is_empty());
        assert!(
            app.adopt_active_view,
            "the jump to the new workspace is armed"
        );
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
    fn n_opens_the_project_prompt_when_no_pane_can_take_input() {
        let mut app = App::new();
        assert_eq!(app.mode, Mode::Terminal);
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(
            app.sidebar_prompt_active(),
            "with no panes, the dashboard's `n` must work from terminal mode"
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
    fn first_run_prompt_enter_creates_workspace_and_shell() {
        let mut app = App::new();
        app.start_first_run_prompt("/tmp/proj".into());
        attach_with(&mut app, vec![]);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![
                control(&Request::WorkspaceNew {
                    dir: PathBuf::from("/tmp/proj"),
                }),
                control(&Request::PaneRun {
                    tab: None,
                    cmd: vec![shell],
                    ephemeral: false,
                }),
            ],
        );
        assert_eq!(app.mode, Mode::Terminal);
        assert!(
            app.adopt_active_view,
            "the jump to the new workspace is armed"
        );
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
        app.on_key(plain('n'));
        app.on_key(plain('a'));
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
        app.on_key(plain('j')); // first agent = blocked pane 3
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
    fn enter_submits_the_typed_input_ignoring_the_selection() {
        let root = temp_tree(&["alpha", "beta"]);
        let mut app = App::new();
        app.start_first_run_prompt(format!("{}/", root.display()));
        // Two matches; move the highlight onto the second.
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.prompt_selected(), 1);

        let typed = app.sidebar_prompt().to_string();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Enter mounts the *typed* directory (the tempdir), never the highlight.
        let dir = std::fs::canonicalize(&typed).unwrap();
        assert_eq!(
            out,
            vec![
                control(&Request::WorkspaceNew { dir }),
                control(&Request::PaneRun {
                    tab: None,
                    cmd: vec![shell],
                    ephemeral: false,
                }),
            ]
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
}
