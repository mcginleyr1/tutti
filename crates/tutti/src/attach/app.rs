//! The attach client's state machine: everything the TUI knows and every
//! decision it makes, driven purely by inbound wire frames and user input. It
//! owns no socket and no terminal, so it can be exercised headlessly in tests.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use tutti_core::{
    AgentKind, AgentState, Direction, Event, Frame as WireFrame, Layout, PaneId, PaneInfo, Request,
    Response, TabId, TabView, WorkspaceId, WorkspaceView,
};

use super::input;
use super::layout::pane_rects;

const SCROLLBACK: usize = 10_000;
const STATUS_TTL: Duration = Duration::from_secs(4);
const MOUSE_SCROLL_STEP: usize = 3;

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
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
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
        }
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
            Event::StateChanged { pane, to, .. } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.info.state = to;
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
        if input::is_prefix(key) {
            self.mode = Mode::Prefix;
            self.set_status("prefix (Ctrl+B): % \" x n p c o z d [ ?".into());
            return Vec::new();
        }
        match (self.focused, input::encode_key(key)) {
            (Some(pane), Some(bytes)) => vec![WireFrame::Input { pane, bytes }],
            _ => Vec::new(),
        }
    }

    fn on_key_prefix(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        self.mode = Mode::Terminal;
        self.status = None;
        let focused = self.focused;
        match key.code {
            KeyCode::Char('%') => self.split(Direction::Horizontal),
            KeyCode::Char('"') => self.split(Direction::Vertical),
            KeyCode::Char('x') => {
                match focused {
                    Some(pane) => {
                        self.mode = Mode::ConfirmKill(pane);
                        self.set_status(format!("kill pane {pane}? (y/n)"));
                    }
                    None => self.set_status("no pane to kill".into()),
                }
                Vec::new()
            }
            KeyCode::Char('n') => self.switch_tab(1),
            KeyCode::Char('p') => self.switch_tab(-1),
            KeyCode::Char('c') => self.new_tab(),
            KeyCode::Char('o') => {
                self.focus_cycle();
                Vec::new()
            }
            KeyCode::Left => self.focus_dir(FocusDir::Left),
            KeyCode::Right => self.focus_dir(FocusDir::Right),
            KeyCode::Up => self.focus_dir(FocusDir::Up),
            KeyCode::Down => self.focus_dir(FocusDir::Down),
            KeyCode::Char('z') => {
                if self.focused.is_some() {
                    self.zoom = !self.zoom;
                }
                Vec::new()
            }
            KeyCode::Char('d') | KeyCode::Char('q') => self.detach(),
            KeyCode::Char('[') => self.enter_scroll(),
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                Vec::new()
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => match focused {
                Some(pane) => vec![WireFrame::Input {
                    pane,
                    bytes: vec![0x02],
                }],
                None => Vec::new(),
            },
            other => {
                self.set_status(format!("unknown prefix key: {}", describe(other)));
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

    fn focus_dir(&mut self, dir: FocusDir) -> Vec<WireFrame> {
        let Some(current) = self.focused else {
            return Vec::new();
        };
        let Some(from) = self
            .rects
            .iter()
            .find(|(p, _)| *p == current)
            .map(|(_, r)| *r)
        else {
            return Vec::new();
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
        if let Some(pane) = best {
            self.focused = Some(pane);
        }
        Vec::new()
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
}
