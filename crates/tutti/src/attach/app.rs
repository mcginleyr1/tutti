//! The attach client's state machine: everything the TUI knows and every
//! decision it makes, driven purely by inbound wire frames and user input. It
//! owns no socket and no terminal, so it can be exercised headlessly in tests.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use tutti_core::{
    AgentKind, AgentState, Direction, Event, Frame as WireFrame, Layout, PaneId, PaneInfo, Request,
    Response, TabId, TabView, WorkspaceId, WorkspaceView,
};

use super::input;
use super::layout::pane_rects;
use crate::config::{Action, Config, PrefixAction, RESIZE_DELTA};

const SCROLLBACK: usize = 10_000;
const STATUS_TTL: Duration = Duration::from_secs(4);
const MOUSE_SCROLL_STEP: usize = 3;
/// How long the prefix can sit unanswered before the which-key popup appears.
const WHICHKEY_DELAY: Duration = Duration::from_millis(500);

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
            config,
        }
    }

    /// The active configuration, for the renderer (hint, which-key, help).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether the which-key popup should be shown: the prefix has been held,
    /// unanswered, past the delay.
    pub fn whichkey_visible(&self) -> bool {
        matches!(self.mode, Mode::Prefix)
            && self
                .prefix_since
                .is_some_and(|since| since.elapsed() >= WHICHKEY_DELAY)
    }

    /// The `PaneFocus` frame to send if focus changed since the last call, so
    /// the server can mark the newly-focused pane seen (`Done → Idle`) and track
    /// the active pane. Returns `None` when focus is unchanged.
    pub fn focus_change(&mut self) -> Option<WireFrame> {
        if self.focus_sent == self.focused {
            return None;
        }
        self.focus_sent = self.focused;
        self.focused
            .map(|pane| control(&Request::PaneFocus { pane }))
    }

    /// Whether a bell is pending (a non-focused pane just blocked or finished),
    /// clearing the flag.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
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
            Event::PaneOutput { .. } => {}
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
            Response::Error { message } => self.set_status(format!("error: {message}")),
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
                self.set_status(format!("unknown prefix key: {}", describe(key.code)));
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
                }
                Vec::new()
            }
            MouseEventKind::ScrollUp => self.mouse_scroll(col, row, true),
            MouseEventKind::ScrollDown => self.mouse_scroll(col, row, false),
            _ => Vec::new(),
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

    /// Recompute pane rectangles for `content` and emit resize requests for any
    /// pane whose rendered size changed, so the server's ptys track the client.
    pub fn sync_sizes(&mut self, content: Rect) -> Vec<WireFrame> {
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
        let Some(layout) = self.active_tab_view().and_then(|t| t.layout.as_ref()) else {
            return Vec::new();
        };
        let zoom = if self.zoom { self.focused } else { None };
        pane_rects(layout, content, zoom)
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
            .find(|(_, r)| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
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

fn center(r: Rect) -> (u16, u16) {
    (r.x + r.width / 2, r.y + r.height / 2)
}

/// Inner (borderless) size of a pane rect as `(rows, cols)`, clamped to at
/// least 1 so a pty is never asked for a zero dimension.
fn inner_size(rect: Rect) -> (u16, u16) {
    (
        rect.height.saturating_sub(2).max(1),
        rect.width.saturating_sub(2).max(1),
    )
}

fn control(request: &Request) -> WireFrame {
    WireFrame::Control(serde_json::to_vec(request).expect("serialize request"))
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
    }
}

fn describe(code: KeyCode) -> String {
    match code {
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use tutti_core::{PaneData, TabView};

    fn view_one_pane() -> Vec<WorkspaceView> {
        vec![WorkspaceView {
            id: WorkspaceId(1),
            name: "w".into(),
            tabs: vec![TabView {
                id: TabId(1),
                name: "1".into(),
                active: true,
                layout: Some(Layout::Leaf(PaneId(1))),
                active_pane: Some(PaneId(1)),
                panes: vec![placeholder_info(PaneId(1))],
            }],
        }]
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
        let mut app = App::new();
        attached(&mut app);
        let area = Rect::new(0, 0, 80, 24);
        let first = app.sync_sizes(area);
        assert_eq!(
            first,
            vec![control(&Request::PaneResize {
                pane: PaneId(1),
                rows: 22,
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
        vec![WorkspaceView {
            id: WorkspaceId(1),
            name: "w".into(),
            tabs: vec![TabView {
                id: TabId(1),
                name: "1".into(),
                active: true,
                layout: Some(Layout::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Layout::Leaf(PaneId(1))),
                    second: Box::new(Layout::Leaf(PaneId(2))),
                }),
                active_pane: Some(PaneId(1)),
                panes: vec![placeholder_info(PaneId(1)), placeholder_info(PaneId(2))],
            }],
        }]
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

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }
    fn plain(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn vim_config() -> Config {
        Config::parse("preset = \"vim\"\n").unwrap()
    }

    /// A 2x2 grid: left column stacks panes 1 (top) / 3 (bottom); right column
    /// stacks 2 (top) / 4 (bottom).
    fn view_2x2() -> Vec<WorkspaceView> {
        let column = |top, bottom| Layout::Split {
            direction: Direction::Vertical,
            ratio: 0.5,
            first: Box::new(Layout::Leaf(top)),
            second: Box::new(Layout::Leaf(bottom)),
        };
        let layout = Layout::Split {
            direction: Direction::Horizontal,
            ratio: 0.5,
            first: Box::new(column(PaneId(1), PaneId(3))),
            second: Box::new(column(PaneId(2), PaneId(4))),
        };
        vec![WorkspaceView {
            id: WorkspaceId(1),
            name: "w".into(),
            tabs: vec![TabView {
                id: TabId(1),
                name: "1".into(),
                active: true,
                layout: Some(layout),
                active_pane: Some(PaneId(1)),
                panes: (1..=4).map(|id| placeholder_info(PaneId(id))).collect(),
            }],
        }]
    }

    fn view_two_tabs() -> Vec<WorkspaceView> {
        let tab = |id: u64, active: bool, pane: u64| TabView {
            id: TabId(id),
            name: id.to_string(),
            active,
            layout: Some(Layout::Leaf(PaneId(pane))),
            active_pane: Some(PaneId(pane)),
            panes: vec![placeholder_info(PaneId(pane))],
        };
        vec![WorkspaceView {
            id: WorkspaceId(1),
            name: "w".into(),
            tabs: vec![tab(1, true, 1), tab(2, false, 2)],
        }]
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
}
