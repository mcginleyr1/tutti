//! The attach client's state machine: everything the TUI knows and every
//! decision it makes, driven purely by inbound wire frames and user input. It
//! owns no socket and no terminal, so it can be exercised headlessly in tests.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::text::Span;
use rtty_vt::Terminal;
use rtty_vt::input::{self, Button, Code, Key, Mouse};
use tutti_core::{
    AgentKind, AgentState, Direction, Event, Frame as WireFrame, Layout, NOTES_CAP, Note, PaneDims,
    PaneId, PaneInfo, Request, Response, RestoreSummary, TabId, TabView, Tagged, Toward,
    WorkspaceId, WorkspaceView,
};

use super::approval::Approvals;
use super::changes::{Changes, DIFF_PANEL_WIDTH, diff_pager_cmd};
use super::deploy::Issues;
use super::follow::{FollowUp, FollowUps};
use super::keys;
use super::launcher::Launcher;
use super::layout::{Boundary, boundaries, pane_rects};
use super::navigate::Navigate;
use super::notes::{NotesPanel, notified_panes};
use super::peek::Peek;
use super::prompt::Prompt;
use super::reader::{Decoder, Input, Report};
use super::rtty::{Handoff, Rtty};
use super::setup::Setup;
use super::sidebar::{self, Sidebar, SidebarEntry};
use crate::config::{
    self, Action, Config, PrefixAction, RESIZE_DELTA, SidebarAction, SidebarVisibility,
};
use crate::host::{Host, Info, Local};
use crate::issues;

/// The sidebar's fixed column width, and the minimum total width below which it
/// is suppressed entirely so panes keep usable room.
const SIDEBAR_WIDTH: u16 = 30;
const SIDEBAR_MIN_TOTAL: u16 = 80;

/// Rows the top chrome header claims: the full-width app bar plus the dim rule
/// beneath it. The content region begins below them.
const HEADER_ROWS: u16 = 2;

const STATUS_TTL: Duration = Duration::from_secs(4);
const MOUSE_SCROLL_STEP: usize = 3;
/// How long the prefix can sit unanswered before the which-key popup appears.
const WHICHKEY_DELAY: Duration = Duration::from_millis(500);

/// The braille working-spinner frames and their advance interval, shared by
/// every working agent so they animate in lockstep. Swap for ASCII by editing
/// this one const — no config knob.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_TICK: Duration = Duration::from_millis(100);

/// One pane's client-side mirror: a terminal fed by snapshots/deltas plus the
/// metadata the status bar shows.
pub struct PaneState {
    pub term: Terminal,
    pub info: PaneInfo,
    pub damaged: bool,
    /// `Some(offset)` while the pane is frozen browsing its scrollback; deltas
    /// are ignored so the scrolled view stays put.
    pub scroll: Option<usize>,
}

impl PaneState {
    fn seeded(rows: u16, cols: u16, bytes: &[u8], info: PaneInfo) -> Self {
        let mut state = Self {
            term: Terminal::new(rows, cols, 0),
            info,
            damaged: true,
            scroll: None,
        };
        state.feed(bytes);
        state
    }

    /// Apply a snapshot or delta. Its events — the modes and title it sets —
    /// are read off the terminal instead, so they are let go.
    fn feed(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        if bytes == tests::PANIC_FRAME {
            panic!("injected: a frame that breaks its pane's terminal");
        }
        self.term.advance(bytes);
        self.term.take_events();
    }
}

/// An in-progress or just-finished mouse text selection within one pane. `anchor`
/// is where the drag began, `head` where it currently is — both 0-based `(row,
/// col)` cells of the pane's visible grid (so it tracks scrollback). A
/// selection is only meaningful inside the single pane it started in; `dragged`
/// stays false for a plain click (head never left the anchor cell) so a focus
/// click copies nothing.
#[derive(Clone, Copy)]
struct Selection {
    pane: PaneId,
    anchor: (u16, u16),
    head: (u16, u16),
    dragged: bool,
}

/// A split-border drag, from a left press on a boundary to the release.
/// `boundary.at` follows where the last `PaneSplitRatio` put the border, and
/// `grab` is the pressed cell's offset from it (1 on the first child's border
/// cell, 0 on the second's), so the grabbed cell stays under the pointer.
/// `moved` stays false for a press that never moved the border: its release is
/// a plain click.
#[derive(Clone, Copy)]
struct BorderDrag {
    boundary: Boundary,
    grab: u16,
    moved: bool,
}

/// The input mode the client is in. Terminal mode forwards keys to the focused
/// pane; the others intercept keys for multiplexer control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Terminal,
    /// Awaiting the key after the prefix, pressed at this instant: the
    /// which-key popup appears once it has sat unanswered `WHICHKEY_DELAY`.
    Prefix(Instant),
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
    /// Confirming a restore of the pane's workspace to its checkpoint `n`,
    /// raised from the changes panel's `turns` section (`y` restores, anything
    /// else cancels).
    ConfirmRestore(PaneId, u64),
    Scroll(PaneId),
    Help,
    /// The attach-time offer to bring back the layout a previous daemon saved:
    /// `enter` restores it, `d` discards it, `esc` leaves it for next time.
    RestoreOffer,
    /// Navigating the sidebar; keys drive the selection instead of the pane.
    Sidebar,
    /// Navigating the changed-files panel; `enter` opens the highlighted file's
    /// (or turn's) diff, `d` the whole workspace diff, `r` restores to a turn.
    DiffFiles,
    /// Editing the add-project directory prompt at the sidebar's foot.
    SidebarPrompt,
    /// Guided workspace creation, step 1: the `workspace name:` field, for a
    /// fork of this origin workspace.
    SidebarWorkspaceName(WorkspaceId),
    /// Guided workspace creation, step 2: the `where:` destination field,
    /// prefilled with the sibling default and offering directory completion.
    SidebarWorkspaceDest(WorkspaceId),
    /// The agent launcher overlay: pick what to run in a pane.
    Launcher,
    /// The launcher's free-form command input.
    LauncherCommand,
    /// The setup panel: install the agent integrations tutti ships, for the
    /// user or into the active project.
    Integrations,
    /// Peeking at a blocked agent from the sidebar: its live prompt floats in an
    /// overlay and answer keys go to it as guarded sends, while focus, tab, and
    /// layout stay exactly where they were.
    Peek(PaneId),
    /// The full-screen go-to: every agent grouped by state over the project
    /// tree, fuzzy-filtered as you type; enter jumps to the highlighted row.
    Navigate,
    /// Answering a held approval — a Claude permission request waiting on
    /// tutti: the tool and its input float in an overlay framed like the
    /// peek; `y` allows, `n` denies, `esc` passes it on to Claude's own
    /// dialog.
    Approval(PaneId),
    /// Reading a done agent's last reply from the sidebar: the final message
    /// of its last finished turn floats in an overlay framed like the peek;
    /// `esc` closes it.
    Reply(PaneId),
    /// The notifications panel: the daemon's history of bells, desktop
    /// notifications and agents wanting attention, newest first; enter jumps
    /// to the highlighted note's pane.
    Notes,
    /// Confirming a clear of the notification history, raised from its panel
    /// (`y` clears, anything else cancels).
    ConfirmClearNotes,
    /// The issues panel: every configured tracker's issues for the active
    /// project, `/` filtering them; enter opens the launcher armed to deploy
    /// the highlighted one.
    Issues,
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
    /// The active tab's split boundaries from the last size sync, in the same
    /// coordinates — what a press grabs to drag a border. Empty while zoomed.
    boundaries: Vec<Boundary>,
    /// The border drag in progress; `None` while no border is held.
    border_drag: Option<BorderDrag>,
    /// The pane a press went to (its program reports the mouse): its drag
    /// and release go there too, wherever the pointer is.
    mouse_grab: Option<PaneId>,
    /// The pane each key held down was pressed into (`held`), for the keys
    /// whose release its program asked for: the key's repeats and release go
    /// there too, wherever the focus went. A release whose press was tutti's
    /// goes nowhere.
    pressed: HashMap<KeyCode, PaneId>,
    pub(super) status: Option<(String, Instant)>,
    /// The visible set last sent as `ViewSizes` — each shown pane at its
    /// rendered size — so a draw that moved nothing sends nothing.
    view_sent: Vec<PaneDims>,
    /// The focus last reported to the server via `PaneFocus`; lets the event
    /// loop notice a focus change from any source (input, layout, attach).
    focus_sent: Option<PaneId>,
    /// Whether the terminal the TUI runs in has the focus, as it last
    /// reported (mode 1004); it has until it says otherwise.
    host_focused: bool,
    /// The pane last told it has the focus (`focus_reports`).
    focus_reported: Option<PaneId>,
    /// The rtty pane the TUI runs in, when it runs in one (`rtty::pane`), and
    /// the moves past tutti's edge handed on to rtty from it.
    pub(super) rtty: Option<Rtty>,
    /// Set when a non-focused pane finished or blocked; the event loop rings the
    /// terminal bell once and clears it.
    bell: bool,
    /// The highlighted sidebar entry while the sidebar is focused.
    pub(super) sidebar_selected: usize,
    /// The selected row's identity and owning project, captured whenever the
    /// selection moves. `set_view` re-anchors the cursor through this after
    /// every layout change — same row, else its project's row, else a clamped
    /// index — so a kill or state change never silently retargets the cursor.
    sidebar_anchor: Option<(sidebar::EntryIdent, WorkspaceId)>,
    pub(super) prompt: Prompt,
    pub(super) launcher: Launcher,
    /// The daemon's machine: every directory, agent, conversation and install
    /// the TUI asks about lives there — in this process, or across ssh.
    pub(crate) host: Host,
    /// Its home directory, which paths shown here shorten to `~`.
    pub(super) home: Option<PathBuf>,
    /// Its login shell, which the launcher's shell and command rows run.
    pub(super) shell: String,
    pub(super) setup: Setup,
    /// The sidebar column from the last size sync, for mouse hit-testing.
    pub(super) sidebar_rect: Option<Rect>,
    pub(super) changes: Changes,
    /// The top tab-bar row from the last size sync, for mouse hit-testing.
    tab_bar_rect: Option<Rect>,
    pub(super) peek: Peek,
    pub(super) approvals: Approvals,
    pub(super) navigate: Navigate,
    pub(super) issues: Issues,
    /// When the client attached — the epoch the working spinner advances from,
    /// so every working agent shares one frame counter.
    spinner_epoch: Instant,
    /// The content width from the last size sync, so the sidebar key can refuse
    /// to focus a column too narrow to render.
    pub(super) last_content_width: u16,
    /// Set after creating a workspace so the next view adopts the server's newly
    /// current tab — the "jump to it" that follows a new-workspace prompt.
    pub(super) adopt_active_view: bool,
    /// The saved layout the attach offered back (`Mode::RestoreOffer`), with
    /// the mode it opened over — the first-run prompt, when armed — for `esc`
    /// to return to.
    restore_offer: Option<(RestoreSummary, Mode)>,
    pub(super) follow_ups: FollowUps,
    /// Whether the daemon predates request tags (wire rev 8): it answered the
    /// attach without its `req`. Such a daemon echoes none and knows only
    /// per-pane `PaneResize`, so the client speaks the old way to it: a reply
    /// completes the oldest follow-up of its kind, and any `Error` cancels
    /// them all.
    pub(super) old_daemon: bool,
    /// Panes that raised a notification while unfocused; their sidebar entry
    /// shows a bell mark until the pane is focused, or the history says its
    /// notifications were read.
    pub(super) notified: HashSet<PaneId>,
    /// The client's copy of the daemon's notification history, oldest first.
    pub(super) notes: Vec<Note>,
    pub(super) notes_panel: NotesPanel,
    /// The app bar's unread-notes chip from the last size sync, for mouse
    /// hit-testing.
    notes_chip_rect: Option<Rect>,
    /// Whether the projects / agents / waiting sidebar sections are collapsed to
    /// their header. Toggled by clicking a section header.
    collapsed_projects: bool,
    collapsed_waiting: bool,
    /// Escape sequences queued for the real terminal (bell + OSC 9 re-emit), so
    /// the user's own terminal raises a desktop notification. Drained by the
    /// event loop.
    pub(super) terminal_out: Vec<Vec<u8>>,
    /// Whether the terminal should be capturing mouse events. Starts at the
    /// config's master switch; the mouse-toggle prefix key flips it so the
    /// terminal's own drag-to-select and copy work while released. The event
    /// loop syncs the real capture state to this.
    mouse_capture: bool,
    /// The live mouse text selection, set on a left-drag inside a pane and kept
    /// highlighted until the next click. `None` when nothing is selected.
    selection: Option<Selection>,
    /// Prefix chord, direct bindings, and the active prefix keymap.
    pub(super) config: Config,
    /// A `tutti attach --view`: it only watches, which its footer says.
    pub view: bool,
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

    /// An app whose daemon runs on this machine.
    pub fn with_config(config: Config) -> Self {
        let host = Host::Local(std::sync::Arc::new(Local::from_env(config.clone())));
        let info = host.info().expect("this machine describes itself");
        Self::with_host(config, host, info)
    }

    /// An app asking `host` about the daemon's machine, which `info` describes:
    /// its sandboxes and issue trackers replace `config`'s, which keeps the
    /// rest — keys, theme, mouse — from the machine the TUI draws on.
    pub fn with_host(mut config: Config, host: Host, info: Info) -> Self {
        config.sandboxes = info.sandboxes;
        config.issues = info.issues;
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
            boundaries: Vec::new(),
            border_drag: None,
            mouse_grab: None,
            pressed: HashMap::new(),
            status: None,
            view_sent: Vec::new(),
            focus_sent: None,
            host_focused: true,
            focus_reported: None,
            rtty: None,
            bell: false,
            sidebar_selected: 0,
            sidebar_anchor: None,
            prompt: Prompt::default(),
            launcher: Launcher::default(),
            host,
            home: info.home,
            shell: info.shell,
            setup: Setup::default(),
            sidebar_rect: None,
            changes: Changes::default(),
            tab_bar_rect: None,
            peek: Peek::default(),
            approvals: Approvals::default(),
            navigate: Navigate::default(),
            issues: Issues::default(),
            spinner_epoch: Instant::now(),
            last_content_width: 0,
            adopt_active_view: false,
            restore_offer: None,
            follow_ups: FollowUps::default(),
            old_daemon: false,
            notified: HashSet::new(),
            notes: Vec::new(),
            notes_panel: NotesPanel::default(),
            notes_chip_rect: None,
            terminal_out: Vec::new(),
            collapsed_projects: false,
            collapsed_waiting: false,
            mouse_capture: config.mouse,
            selection: None,
            config,
            view: false,
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

    /// Where questions about the daemon's machine go, for the event loop's
    /// threads.
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The daemon machine's home directory, which shown paths shorten to `~`.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// Point this machine's host at fixture directories.
    #[cfg(test)]
    pub(crate) fn set_local(&mut self, change: impl FnOnce(&mut Local)) {
        let Host::Local(local) = &self.host else {
            panic!("not a local host");
        };
        let mut local = Local::clone(local);
        change(&mut local);
        self.home = local.dirs.home.clone();
        self.host = Host::Local(std::sync::Arc::new(local));
    }

    /// The saved layout on offer, for the renderer's restore modal.
    pub fn restore_offer(&self) -> Option<&RestoreSummary> {
        self.restore_offer.as_ref().map(|(offer, _)| offer)
    }

    /// Post a transient status line — e.g. a startup-project error surfaced once
    /// the client has attached.
    pub fn note(&mut self, message: String) {
        self.set_status(message);
    }

    /// Whether the which-key popup should be shown: the prefix has been held,
    /// unanswered, past the delay.
    pub fn whichkey_visible(&self) -> bool {
        matches!(self.mode, Mode::Prefix(since) if since.elapsed() >= WHICHKEY_DELAY)
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
    /// the active pane. Returns `None` when focus is unchanged. The event loop
    /// calls it once a pass, after input, so it is also where an approval the
    /// focused pane holds opens its overlay: focus just moved onto the pane, or
    /// a key just closed what covered it.
    pub fn focus_change(&mut self) -> Option<WireFrame> {
        self.open_focused_approval();
        if self.focus_sent == self.focused {
            return None;
        }
        self.focus_sent = self.focused;
        // A pane gaining focus clears its pending notification mark.
        if let Some(pane) = self.focused {
            self.notified.remove(&pane);
        }
        self.focused
            .map(|pane| control(&Request::PaneFocus { pane, seen: false }))
    }

    /// The terminal the TUI runs in gained or lost the focus.
    pub fn host_focus(&mut self, focused: bool) {
        self.host_focused = focused;
    }

    /// The focus reports (`CSI I`, `CSI O`, mode 1004) owed since the last
    /// call: a pane has the focus while it is tutti's focused pane and the
    /// terminal has it, and a program that asked hears when that changes.
    pub fn focus_reports(&mut self) -> Vec<WireFrame> {
        let now = self.focused.filter(|_| self.host_focused);
        if now == self.focus_reported {
            return Vec::new();
        }
        let was = std::mem::replace(&mut self.focus_reported, now);
        [(was, b"\x1b[O"), (now, b"\x1b[I")]
            .into_iter()
            .filter_map(|(pane, report)| {
                let pane = pane?;
                let state = self.panes.get(&pane)?;
                (state.term.modes().focus_reports && state.info.exited.is_none()).then(|| {
                    WireFrame::Input {
                        pane,
                        bytes: report.to_vec(),
                    }
                })
            })
            .collect()
    }

    /// The DECSCUSR parameter the terminal's cursor takes: the shape the
    /// focused pane's program set (nvim's insert-mode bar), else the
    /// terminal's own default, 0 — for a pane browsing its scrollback too.
    pub fn cursor_shape(&self) -> u8 {
        self.focused
            .and_then(|pane| self.panes.get(&pane))
            .filter(|s| s.scroll.is_none())
            .and_then(|s| s.term.cursor_style())
            .map_or(0, |style| style.decscusr())
    }

    /// Kitty's keyboard flags the TUI wants of its terminal: disambiguated
    /// keys and their alternates always, and what the focused pane's program
    /// asked for beyond — repeats and releases (2), and every key an escape
    /// code (8) with its text (16), which a prompt needs too: it is what a
    /// dead key or an input method made. Releases stay on while a key a
    /// program waits to hear released is held (`pressed`), wherever the
    /// focus went.
    pub fn key_flags(&self) -> u8 {
        let pane = self.focused_term().map_or(0, Terminal::kitty_keyboard);
        let all = if pane & input::ALL_KEYS != 0 {
            input::ALL_KEYS | input::ASSOCIATED_TEXT
        } else {
            0
        };
        let held = if self.pressed.is_empty() {
            0
        } else {
            input::EVENT_TYPES
        };
        input::DISAMBIGUATE | input::ALTERNATE_KEYS | (pane & input::EVENT_TYPES) | held | all
    }

    /// Whether a bell is pending (a non-focused pane just blocked or finished),
    /// clearing the flag.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Drain the escape sequences queued for the real terminal — bell and OSC 9
    /// re-emits — for the event loop to write to stdout.
    pub fn take_terminal_out(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.terminal_out)
    }

    // ---- inbound frames -------------------------------------------------

    /// Apply one frame received from the server, mutating client state, and
    /// return what it calls for: a fresh snapshot of a pane whose frame broke
    /// its terminal, a deploy's next step.
    pub fn handle_frame(&mut self, frame: WireFrame) -> Vec<WireFrame> {
        match frame {
            WireFrame::Control(json) => return self.handle_control(&json),
            WireFrame::PaneSnapshot(data) => {
                let info = self
                    .panes
                    .get(&data.pane)
                    .map(|s| s.info.clone())
                    .unwrap_or_else(|| placeholder_info(data.pane));
                match fed(|| PaneState::seeded(data.rows, data.cols, &data.bytes, info.clone())) {
                    Some(state) => {
                        self.panes.insert(data.pane, state);
                    }
                    None => return self.reseed(data.pane, data.rows, data.cols, info),
                }
            }
            WireFrame::PaneDelta(data) => {
                if let Some(state) = self.panes.get_mut(&data.pane)
                    && state.scroll.is_none()
                {
                    if fed(|| state.feed(&data.bytes)).is_none() {
                        let info = state.info.clone();
                        return self.reseed(data.pane, data.rows, data.cols, info);
                    }
                    state.damaged = true;
                }
            }
            // Host replies go to their callers before frames reach here.
            WireFrame::Input { .. } | WireFrame::PaneRaw { .. } | WireFrame::Host(_) => {}
        }
        Vec::new()
    }

    /// Recover a pane whose frame panicked its terminal: start it over blank
    /// and have the server reseed it — a `PaneScroll` back to the live view
    /// does — instead of letting one pane take the TUI down.
    fn reseed(&mut self, pane: PaneId, rows: u16, cols: u16, info: PaneInfo) -> Vec<WireFrame> {
        self.panes
            .insert(pane, PaneState::seeded(rows, cols, b"", info));
        self.set_status(format!(
            "pane {pane}'s output broke its terminal; reseeding"
        ));
        vec![control(&Request::PaneScroll { pane, offset: 0 })]
    }

    fn handle_control(&mut self, json: &[u8]) -> Vec<WireFrame> {
        let mut frames = Vec::new();
        if let Ok(event) = serde_json::from_slice::<Event>(json) {
            frames = self.handle_event(event);
        } else if let Ok(Tagged { req, body }) = serde_json::from_slice::<Tagged<Response>>(json) {
            frames = self.handle_response(req, body);
        }
        // A peek only ever answers a blocked agent: once the agent moves on —
        // answered from here or from anywhere else — or vanishes, the overlay
        // closes itself back onto the sidebar it was opened from.
        if let Mode::Peek(pane) = self.mode
            && self
                .panes
                .get(&pane)
                .is_none_or(|s| s.info.state != AgentState::Blocked)
        {
            self.mode = Mode::Sidebar;
        }
        // An approval overlay closes with its approval — answered from here or
        // anywhere else, passed on, timed out — or its pane.
        if let Mode::Approval(pane) = self.mode
            && self.shown_approval(pane).is_none()
        {
            self.mode = self.approvals.back;
        }
        // A reply overlay closes with its pane.
        if let Mode::Reply(pane) = self.mode
            && !self.panes.contains_key(&pane)
        {
            self.mode = Mode::Sidebar;
        }
        self.open_focused_approval();
        frames
    }

    pub(super) fn handle_event(&mut self, event: Event) -> Vec<WireFrame> {
        match event {
            Event::LayoutChanged { workspaces } => self.set_view(workspaces),
            Event::StateChanged { pane, from, to } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.info.state = to;
                }
                self.patch_view(pane, |info| info.state = to);
                // Ring once when a pane that was working needs attention or
                // finished while the user was looking elsewhere.
                let needs_attention = matches!(to, AgentState::Blocked | AgentState::Done)
                    && from == AgentState::Working;
                if needs_attention && self.focused != Some(pane) {
                    self.bell = true;
                }
            }
            // The daemon dropped the pane's held approvals with it.
            Event::PaneExited { pane, code } => {
                let exit = |info: &mut PaneInfo| {
                    info.exited = Some(code);
                    info.state = AgentState::Done;
                    info.approval = None;
                    info.approvals.clear();
                };
                if let Some(state) = self.panes.get_mut(&pane) {
                    exit(&mut state.info);
                }
                self.patch_view(pane, exit);
            }
            Event::PaneNotification { pane, title, body } => {
                self.on_notification(pane, title, body)
            }
            // Notes land in id order, but one can cross a `NotesChanged` in
            // flight: a repeat is dropped, a late one slots in by id.
            Event::Noted { note } => {
                let at = self.notes.partition_point(|n| n.id < note.id);
                if self.notes.get(at).is_none_or(|n| n.id != note.id) {
                    self.notes.insert(at, note);
                    if self.notes.len() > NOTES_CAP {
                        self.notes.remove(0);
                    }
                }
            }
            // A pane whose notifications have all been read, by anyone,
            // loses its bell mark; one the history names for the first time
            // with an unread one — a restored pane its notes now point at —
            // gains it, as it would at an attach.
            Event::NotesChanged { notes } => {
                let pending = notified_panes(&notes);
                let named: HashSet<PaneId> = self.notes.iter().map(|n| n.pane).collect();
                self.notified.retain(|pane| pending.contains(pane));
                self.notified
                    .extend(pending.into_iter().filter(|pane| !named.contains(pane)));
                self.notes = notes;
            }
            // A script focused `pane` (a notch app's click): show it as an
            // agent-row jump does — its tab, wherever that lives, focused.
            Event::PaneFocused { pane } => {
                let tab = self
                    .all_tabs()
                    .into_iter()
                    .find(|t| t.layout.as_ref().is_some_and(|l| l.contains(pane)))
                    .map(|t| t.id);
                if let Some(tab) = tab {
                    self.mode = Mode::Terminal;
                    self.status = None;
                    self.active_tab = Some(tab);
                    self.zoom = false;
                    self.focused = Some(pane);
                }
            }
            // The program holding the direct keys in the focused pane (an
            // editor at its own edge) found no pane that way: the move goes
            // on as the key itself would take it here.
            Event::FocusEdge { pane, toward } => {
                if self.mode == Mode::Terminal && self.focused == Some(pane) {
                    return self.focus_or_tab(match toward {
                        Toward::Left => FocusDir::Left,
                        Toward::Right => FocusDir::Right,
                        Toward::Up => FocusDir::Up,
                        Toward::Down => FocusDir::Down,
                    });
                }
            }
            // The program holding the direct keys asked tutti to resize, and
            // no split of tutti's takes it: rtty's does.
            Event::ResizeEdge { pane, toward } => {
                if self.mode == Mode::Terminal && self.focused == Some(pane) {
                    self.rtty_handoff(Handoff::Resize(toward), None);
                }
            }
            Event::PaneOutput { .. } | Event::PaneRows { .. } | Event::Snapshot { .. } => {}
        }
        Vec::new()
    }

    /// Apply `patch` to `pane`'s entry in the workspace view — what the sidebar
    /// and the app-bar census render — so a `StateChanged`/`PaneExited` shows
    /// there at once instead of waiting for the next `LayoutChanged`, which the
    /// server sends only when a stat or the layout itself moved.
    fn patch_view(&mut self, pane: PaneId, patch: impl Fn(&mut PaneInfo)) {
        for info in self
            .workspaces
            .iter_mut()
            .flat_map(|w| w.tabs.iter_mut())
            .flat_map(|t| t.panes.iter_mut())
        {
            if info.id == pane {
                patch(info);
            }
        }
    }

    /// Apply a reply — `req` is the one its request rode out with, echoed —
    /// and return what it calls for: a deploy's next step.
    pub(super) fn handle_response(
        &mut self,
        req: Option<u64>,
        response: Response,
    ) -> Vec<WireFrame> {
        match response {
            Response::Attached {
                session,
                workspaces,
                wire_rev,
                restorable,
                notes,
            } => {
                self.old_daemon = req.is_none();
                self.session = session;
                self.set_view(workspaces);
                // A reattach catches up on the history — and on the bell
                // marks of the panes whose notifications nobody has read.
                self.notified = notified_panes(&notes);
                self.notes = notes;
                if wire_rev != tutti_core::WIRE_REV {
                    self.note(format!(
                        "daemon is running an older build (wire rev {wire_rev}, client {}) — `tutti server stop`, reinstall, and reattach",
                        tutti_core::WIRE_REV
                    ));
                }
                // The offer comes first, over a first-run prompt too — but
                // not to a view, which could take it up in no way.
                if let Some(offer) = restorable.filter(|_| !self.view) {
                    self.restore_offer = Some((offer, self.mode));
                    self.mode = Mode::RestoreOffer;
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
                // open the launcher over it, and flash what it made and where. A
                // deploy waited for its issue's workspace: run the agent there.
                let creates = |f: &FollowUp| {
                    matches!(f, FollowUp::Mount | FollowUp::Fork | FollowUp::Deploy(..))
                };
                match self.take_follow_up(req, creates) {
                    Some(FollowUp::Fork) => {
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
                    Some(FollowUp::Deploy(mut run, file)) => {
                        if let Request::Run { workspace, .. } = run.as_mut() {
                            *workspace = Some(id);
                        }
                        return vec![self.follow(&run, FollowUp::Deployed(file))];
                    }
                    _ => {}
                }
            }
            Response::Merged { pushed, bookmark } => {
                // The merge landed: report where, then offer to clean up (discard)
                // the now-merged workspace.
                let merges = |f: &FollowUp| matches!(f, FollowUp::Merge(_));
                if let Some(FollowUp::Merge(id)) = self.take_follow_up(req, merges) {
                    let mut msg = format!("merged into {bookmark}");
                    if pushed {
                        msg.push_str(" and pushed");
                    }
                    msg.push_str(" — clean up workspace? y/N");
                    self.set_status(msg);
                    self.mode = Mode::ConfirmCleanup(id);
                }
            }
            // A deploy's run landed: follow it to the agent's pane, and write
            // back to the tracker as its source asks.
            Response::Ran {
                workspace,
                tab,
                pane,
            } => {
                let deploys = |f: &FollowUp| matches!(f, FollowUp::Deployed(_));
                if let Some(FollowUp::Deployed(_)) = self.take_follow_up(req, deploys) {
                    let frames = self.jump_to(tab, Some(pane));
                    let deployed = self.panes.get(&pane).and_then(|s| {
                        let issue = s.info.issue.as_ref()?;
                        Some(format!(
                            "deployed {} to {}",
                            issues::label(&issue.id),
                            s.info.title
                        ))
                    });
                    if let Some(deployed) = deployed {
                        self.set_status(deployed);
                    }
                    self.issues
                        .write_backs
                        .extend(self.write_back(workspace, pane));
                    return frames;
                }
            }
            // The asked-for reply came: show it, unless the sidebar it was
            // asked from has moved on to something else meanwhile.
            Response::LastReply { text, .. } => {
                let replies = |f: &FollowUp| matches!(f, FollowUp::Reply(_));
                if let Some(FollowUp::Reply(pane)) = self.take_follow_up(req, replies)
                    && self.mode == Mode::Sidebar
                {
                    self.peek.reply = text;
                    self.mode = Mode::Reply(pane);
                    self.status = None;
                }
            }
            Response::Restored { n, saved } => self.set_status(match saved {
                Some(saved) => format!("restored to #{n} — the state it replaced is #{saved}"),
                None => format!("restored to #{n}"),
            }),
            Response::SessionRestored {
                workspaces,
                revived,
                skipped,
                ..
            } => {
                // The view carrying the restored layout landed just before
                // this reply: follow it to the tab that was current.
                if workspaces > 0 {
                    self.active_tab = self.flagged_active_tab().or(self.active_tab);
                    self.zoom = false;
                    self.refocus();
                }
                self.set_status(crate::render::restored(workspaces, revived, skipped.len()));
            }
            Response::Error { message } => {
                // A failed new-workspace, guided-create, merge, or deploy request
                // must not later hijack the tab, the launcher, the cleanup
                // confirm, or another workspace — and cancels only its own: a
                // refused answer leaves a merge's cleanup offer standing. An old
                // daemon names no request, so its refusal cancels them all.
                let cancelled: Vec<FollowUp> = match req {
                    Some(req) => self
                        .follow_ups
                        .pending
                        .remove(&req)
                        .map(|(f, ..)| f)
                        .into_iter()
                        .collect(),
                    None if self.old_daemon => self
                        .follow_ups
                        .pending
                        .drain()
                        .map(|(_, (f, ..))| f)
                        .collect(),
                    None => Vec::new(),
                };
                if (req.is_none() && self.old_daemon) || cancelled.contains(&FollowUp::Mount) {
                    self.adopt_active_view = false;
                }
                // A cancelled deploy's prompt file went to no pane, so nothing
                // else would remove it before the daemon stops.
                for follow_up in cancelled {
                    if let FollowUp::Deploy(_, Some(file)) | FollowUp::Deployed(Some(file)) =
                        follow_up
                    {
                        let _ = self.host.forget_prompt(&file);
                    }
                }
                self.set_status(format!("error: {message}"));
            }
            _ => {}
        }
        Vec::new()
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
        // Keep the changes-panel cursor in range (after refocus: the turns
        // follow the focused pane); a panel emptied by a refresh (everything
        // merged, say) drops focus back to the panes.
        let rows = self.diff_rows();
        self.changes.selected = self.changes.selected.min(rows.saturating_sub(1));
        if self.mode == Mode::DiffFiles && rows == 0 {
            self.mode = Mode::Terminal;
        }
    }

    // ---- keyboard -------------------------------------------------------

    /// A key from the terminal, with what kitty's protocol said of it. A
    /// press does what `on_key` does with it. A key pressed into a pane that
    /// asked to hear it released (`pressed`) stays that pane's: its repeats
    /// and its release go there, whatever the focus or mode since. Any other
    /// repeat is a press; any other release goes nowhere, and a lone
    /// modifier only to the focused pane — never to tutti's own keys.
    pub fn on_typed(&mut self, key: KeyEvent, report: Option<&Report>) -> Vec<WireFrame> {
        let id = held(key, report);
        match (key.kind, self.pressed.get(&id).copied()) {
            (KeyEventKind::Release, owner) => {
                self.pressed.remove(&id);
                owner.map_or_else(Vec::new, |pane| self.typed_into(pane, key, report))
            }
            (KeyEventKind::Repeat, Some(pane)) => self.typed_into(pane, key, report),
            (kind, _) => {
                if kind == KeyEventKind::Press {
                    self.pressed.remove(&id);
                }
                match self.mode {
                    Mode::Terminal => self.on_key_terminal(key, report),
                    _ if matches!(key.code, KeyCode::Modifier(_)) => Vec::new(),
                    _ => self.on_key(key),
                }
            }
        }
    }

    /// `key` typed into `pane` as its program asked for its keys: nothing
    /// into an exited pane.
    fn typed_into(&self, pane: PaneId, key: KeyEvent, report: Option<&Report>) -> Vec<WireFrame> {
        self.panes
            .get(&pane)
            .filter(|s| s.info.exited.is_none())
            .and_then(|s| keys::encode_key(key, report, Some(&s.term)))
            .map(|bytes| vec![WireFrame::Input { pane, bytes }])
            .unwrap_or_default()
    }

    /// Handle a key press, returning frames to send to the server.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        match self.mode {
            Mode::Terminal => self.on_key_terminal(key, None),
            Mode::Prefix(_) => self.on_key_prefix(key),
            Mode::ConfirmKill(pane) => self.on_key_confirm(key, pane),
            Mode::ConfirmKillWorkspace(id) => self.on_key_confirm_workspace(key, id),
            Mode::ConfirmKillAgent(pane) => self.on_key_confirm_agent(key, pane),
            Mode::ConfirmMerge(id) => self.on_key_confirm_merge(key, id),
            Mode::ConfirmCleanup(id) => self.on_key_confirm_cleanup(key, id),
            Mode::ConfirmRestore(pane, n) => self.on_key_confirm_restore(key, pane, n),
            Mode::RestoreOffer => self.on_key_restore_offer(key),
            Mode::Scroll(pane) => self.on_key_scroll(key, pane),
            Mode::Sidebar => self.on_key_sidebar(key),
            Mode::DiffFiles => self.on_key_diff_files(key),
            Mode::SidebarPrompt => self.on_key_prompt(key),
            Mode::SidebarWorkspaceName(origin) => self.on_key_workspace_name(key, origin),
            Mode::SidebarWorkspaceDest(origin) => self.on_key_workspace_dest(key, origin),
            Mode::Launcher => self.on_key_launcher(key),
            Mode::Navigate => self.on_key_navigate(key),
            Mode::LauncherCommand => self.on_key_launcher_command(key),
            Mode::Integrations => {
                self.on_key_setup(key);
                Vec::new()
            }
            Mode::Peek(pane) => self.on_key_peek(key, pane),
            Mode::Help => {
                self.mode = Mode::Terminal;
                Vec::new()
            }
            Mode::Approval(pane) => self.on_key_approval(key, pane),
            Mode::Reply(_) => self.on_key_reply(key),
            Mode::Notes => self.on_key_notes(key),
            Mode::ConfirmClearNotes => self.on_key_confirm_clear_notes(key),
            Mode::Issues => {
                self.on_key_issues(key);
                Vec::new()
            }
        }
    }

    /// Text the terminal pasted (bracketed paste). In a pane, as a terminal
    /// hands a paste to its program: whole, inside the paste marks when the
    /// program set bracketed paste, so a shell runs none of its lines, and
    /// never to an exited pane, whose keys act on it. Elsewhere — a prompt,
    /// a panel — typed, as the keys its bytes are.
    pub fn on_paste(&mut self, text: &[u8]) -> Vec<WireFrame> {
        if self.mode == Mode::Terminal {
            return match self.focused.map(|pane| (pane, self.panes.get(&pane))) {
                Some((pane, Some(state))) if state.info.exited.is_none() => {
                    let bytes = if state.term.modes().bracketed_paste {
                        [b"\x1b[200~".as_slice(), text, b"\x1b[201~"].concat()
                    } else {
                        text.to_vec()
                    };
                    vec![WireFrame::Input { pane, bytes }]
                }
                _ => Vec::new(),
            };
        }
        let mut decoder = Decoder::default();
        let mut inputs = decoder.feed(text);
        inputs.extend(decoder.flush());
        inputs
            .into_iter()
            .flat_map(|input| match input {
                Input::Key(key, _) => self.on_key(key),
                _ => Vec::new(),
            })
            .collect()
    }

    fn on_key_terminal(&mut self, key: KeyEvent, report: Option<&Report>) -> Vec<WireFrame> {
        if self.enter_prefix(key) {
            return Vec::new();
        }
        // Direct bindings intercept their chords before they reach the pane —
        // unless the pane's program claimed them with the user var
        // `TUTTI_KEYS=pane` (an editor moving through its own splits first).
        let pane_keys = self
            .focused
            .and_then(|pane| self.panes.get(&pane))
            .is_some_and(|s| {
                s.info.exited.is_none()
                    && s.info
                        .user_vars
                        .get("TUTTI_KEYS")
                        .is_some_and(|v| v == "pane")
            });
        if !pane_keys && let Some(action) = self.config.keys.action_for(key) {
            return self.direct_action(action);
        }
        // An exited pane has no child to type at; the keys its title
        // advertises act on the corpse instead: relaunch, pull a sandbox's
        // pushed work, or close.
        if let Some(pane) = self.focused
            && key.modifiers.is_empty()
            && let Some(state) = self.panes.get(&pane)
            && state.info.exited.is_some()
        {
            match (key.code, state.info.sandbox_name.clone()) {
                (KeyCode::Char('r'), _) => return self.relaunch_exited(pane),
                (KeyCode::Char('p'), Some(sandbox)) => {
                    // The reply lands as guided create's does: a jump to the
                    // new workspace and the launcher over it.
                    let branch = tutti_core::sandbox_branch(&sandbox);
                    self.set_status(format!("pulling {branch}…"));
                    let pull = Request::SandboxPull { pane, name: None };
                    return vec![self.follow(&pull, FollowUp::Fork)];
                }
                (KeyCode::Char('x'), _) => return vec![control(&Request::PaneKill { pane })],
                _ => {}
            }
        }
        let term = self.focused_term();
        match (self.focused, keys::encode_key(key, report, term)) {
            (Some(pane), Some(bytes)) => {
                let release = KeyEvent {
                    kind: KeyEventKind::Release,
                    ..key
                };
                if key.kind == KeyEventKind::Press
                    && keys::encode_key(release, report, term).is_some()
                {
                    self.pressed.insert(held(key, report), pane);
                }
                vec![WireFrame::Input { pane, bytes }]
            }
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

    /// The focused pane's terminal, which says how its program asked for its keys.
    fn focused_term(&self) -> Option<&Terminal> {
        self.focused
            .and_then(|pane| self.panes.get(&pane))
            .map(|s| &s.term)
    }

    /// Enter prefix mode if `key` is the prefix chord, returning whether it was.
    /// Shared by the terminal, sidebar, and changes-panel modes, so the prefix
    /// verbs (`w`, `f`, `q`, …) reach across from whichever panel holds the
    /// keys — prefix mode itself hands back to the terminal before dispatching.
    pub(super) fn enter_prefix(&mut self, key: KeyEvent) -> bool {
        if !self.config.prefix.matches(key) {
            return false;
        }
        self.mode = Mode::Prefix(Instant::now());
        true
    }

    /// Perform a direct-binding action: directional focus (past the edge on
    /// to a panel, rtty or the next tab: `focus_or_tab`), split resize, or
    /// killing the focused pane.
    fn direct_action(&mut self, action: Action) -> Vec<WireFrame> {
        match action {
            Action::FocusLeft => self.focus_or_tab(FocusDir::Left),
            Action::FocusRight => self.focus_or_tab(FocusDir::Right),
            Action::FocusUp => self.focus_or_tab(FocusDir::Up),
            Action::FocusDown => self.focus_or_tab(FocusDir::Down),
            Action::ResizeLeft => self.resize_split(Direction::Horizontal, -RESIZE_DELTA),
            Action::ResizeRight => self.resize_split(Direction::Horizontal, RESIZE_DELTA),
            Action::ResizeUp => self.resize_split(Direction::Vertical, -RESIZE_DELTA),
            Action::ResizeDown => self.resize_split(Direction::Vertical, RESIZE_DELTA),
            Action::KillPane => self.request_kill(),
        }
    }

    fn on_key_prefix(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        self.mode = Mode::Terminal;
        self.status = None;
        // Esc (and any key while the which-key popup is up) leaves prefix mode.
        if key.code == KeyCode::Esc {
            return Vec::new();
        }
        // The prefix pressed twice forwards the literal prefix chord to the pane.
        if self.config.prefix.matches(key) {
            let bytes = keys::encode_key(key, None, self.focused_term());
            return match (self.focused, bytes) {
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
            PrefixAction::Navigate => {
                self.open_navigate();
                Vec::new()
            }
            PrefixAction::DiffFiles => {
                self.focus_diff_panel();
                Vec::new()
            }
            PrefixAction::Notes => {
                self.open_notes();
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
            PrefixAction::Issues => {
                self.open_issues();
                Vec::new()
            }
            PrefixAction::Integrations => {
                self.open_setup();
                Vec::new()
            }
            PrefixAction::MouseToggle => {
                self.mouse_capture = !self.mouse_capture;
                self.set_status(if self.mouse_capture {
                    "mouse on — drag to select & copy".into()
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
        let mut sidebar = sidebar::build(&self.workspaces, self.active_tab, self.home.as_deref());
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
    pub(super) fn active_project(&self) -> Option<WorkspaceId> {
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
    /// confirms and overlays count: while one is open the agents filter and
    /// the selection highlight must not shift under the question being asked.
    pub fn sidebar_focused(&self) -> bool {
        matches!(
            self.mode,
            Mode::Sidebar
                | Mode::SidebarPrompt
                | Mode::SidebarWorkspaceName(_)
                | Mode::SidebarWorkspaceDest(_)
                | Mode::ConfirmKillWorkspace(_)
                | Mode::ConfirmKillAgent(_)
                | Mode::ConfirmMerge(_)
                | Mode::ConfirmCleanup(_)
                | Mode::Peek(_)
                | Mode::Reply(_)
        ) || (matches!(self.mode, Mode::Approval(_)) && self.approvals.back == Mode::Sidebar)
    }

    pub fn sidebar_selected(&self) -> usize {
        self.sidebar_selected
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

    /// Split a content area into the sidebar column (when shown), the pane
    /// region, and the changed-files panel (when shown) carved off the pane
    /// region's right edge. The single source of truth for both chrome columns.
    pub(super) fn split_content(&self, content: Rect) -> (Option<Rect>, Rect, Option<Rect>) {
        let (sidebar, mut panes) = if self.sidebar_shown(content.width) {
            let w = SIDEBAR_WIDTH.min(content.width);
            let sidebar = Rect::new(content.x, content.y, w, content.height);
            let panes = Rect::new(content.x + w, content.y, content.width - w, content.height);
            (Some(sidebar), panes)
        } else {
            (None, content)
        };
        let diff_panel = if self.diff_panel_shown(content.width) {
            let w = DIFF_PANEL_WIDTH.min(panes.width);
            let panel = Rect::new(panes.x + panes.width - w, panes.y, w, panes.height);
            panes.width -= w;
            Some(panel)
        } else {
            None
        };
        (sidebar, panes, diff_panel)
    }

    /// The three regions a content area splits into: the sidebar column (when
    /// shown, full height), the pane area, and the changed-files panel (when
    /// shown, full height) at the right edge. The tab list now lives in the top
    /// app bar, so the content region no longer carries a tab-bar row. Every
    /// rect the renderer, resize sync, and mouse hit-testing use flows from here
    /// so they stay in agreement.
    pub fn regions(&self, content: Rect) -> (Option<Rect>, Rect, Option<Rect>) {
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

    /// The rendered width of the tab segments: the chips plus one-column
    /// separators between them.
    fn tab_bar_width(&self) -> u16 {
        let chips = self.tab_chips();
        let segments: u16 = chips.iter().map(TabChip::width).sum();
        segments + chips.len().saturating_sub(1) as u16
    }

    /// The app bar's title: `tutti`, plus ` — <session>` when the session is
    /// named anything else — one named after the binary would read as a
    /// stutter ("tutti — tutti").
    pub fn app_title(&self) -> String {
        if self.session.is_empty() || self.session == "tutti" {
            "tutti".to_string()
        } else {
            format!("tutti — {}", self.session)
        }
    }

    /// The agent-state census after the app-bar title: one `(state, text)`
    /// chip per state with agents in it, blocked first — each drawn after a
    /// three-column separator and its one-column state dot.
    pub fn census_chips(&self) -> Vec<(AgentState, String)> {
        let counts = self.agent_census();
        [
            (counts.blocked, AgentState::Blocked, "blocked"),
            (counts.working, AgentState::Working, "working"),
            (counts.done, AgentState::Done, "done"),
        ]
        .into_iter()
        .filter(|(count, ..)| *count > 0)
        .map(|(count, state, label)| (state, format!(" {count} {label}")))
        .collect()
    }

    /// The unread-notes chip closing the app-bar census — the bell and the
    /// unread count — and the cells it covers on the `app_bar` row: past the
    /// accent bar, the title, each census chip and its own separator. The
    /// renderer draws it there and a click there opens the notifications
    /// panel.
    pub fn notes_chip(&self, app_bar: Rect) -> (String, Rect) {
        let text = format!("🔔 {}", self.unread_notes());
        let census: usize = self
            .census_chips()
            .iter()
            .map(|(_, chip)| 4 + Span::raw(chip.as_str()).width())
            .sum();
        let offset = 1 + Span::raw(self.app_title()).width() + census + 3;
        let x = app_bar.x.saturating_add(offset as u16).min(app_bar.right());
        let width = (Span::raw(text.as_str()).width() as u16).min(app_bar.right() - x);
        (text, Rect::new(x, app_bar.y, width, 1))
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
        self.status = None;
    }

    /// Move the sidebar cursor to `idx`, capturing its identity anchor so the
    /// next layout change re-finds the same row rather than the same position.
    fn select_entry(&mut self, sidebar: &Sidebar, idx: usize) {
        self.sidebar_selected = idx;
        self.sidebar_anchor = anchor_at(sidebar, idx);
    }

    /// Keys while the sidebar is focused, dispatched through
    /// `config::SIDEBAR_TABLE` — the table the help overlay and the bottom-bar
    /// hint render, so what they say is what the keys do.
    fn on_key_sidebar(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if self.enter_prefix(key) {
            return Vec::new();
        }
        // The directional direct bindings cross the sidebar↔pane edge:
        // focus_right returns to the pane area (keeping its focus), focus_left
        // goes past tutti's left edge as the pane's left edge would with no
        // sidebar — on to rtty, else the previous tab — and in rtty, up and
        // down go on to rtty's pane that way.
        match self.config.keys.action_for(key) {
            Some(Action::FocusRight) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return Vec::new();
            }
            Some(Action::FocusLeft) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return self.past_edge(FocusDir::Left);
            }
            Some(Action::FocusUp) if self.rtty.is_some() => return self.past_edge(FocusDir::Up),
            Some(Action::FocusDown) if self.rtty.is_some() => {
                return self.past_edge(FocusDir::Down);
            }
            _ => {}
        }
        let Some(action) = config::panel_action(config::SIDEBAR_TABLE, key) else {
            return Vec::new();
        };
        let sidebar = self.sidebar();
        match action {
            SidebarAction::Down => {
                self.select_entry(&sidebar, next_visible(&sidebar, self.sidebar_selected));
                Vec::new()
            }
            SidebarAction::Up => {
                self.select_entry(&sidebar, prev_visible(&sidebar, self.sidebar_selected));
                Vec::new()
            }
            SidebarAction::Jump => self.jump_to_selected(&sidebar),
            SidebarAction::Peek => self.open_peek(&sidebar),
            SidebarAction::NewAgent => self.new_agent_in_selected(&sidebar),
            SidebarAction::AddProject => {
                self.open_project_prompt();
                Vec::new()
            }
            SidebarAction::Diff => self.open_diff_pane(&sidebar),
            SidebarAction::NewWorkspace => {
                self.open_workspace_prompt(&sidebar);
                Vec::new()
            }
            SidebarAction::Merge => {
                self.confirm_merge(&sidebar);
                Vec::new()
            }
            SidebarAction::Update => self.update_selected(&sidebar),
            SidebarAction::Kill => {
                self.confirm_kill_selected(&sidebar);
                Vec::new()
            }
            SidebarAction::Back => {
                self.mode = Mode::Terminal;
                self.status = None;
                Vec::new()
            }
        }
    }

    /// Jump to the selected entry and hand focus back to the pane: a workspace
    /// selects its tab; an agent selects its tab and focuses its pane (the
    /// `PaneFocus` follows from `focus_change`).
    fn jump_to_selected(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        match sidebar.entries.get(self.sidebar_selected) {
            Some(SidebarEntry::Workspace(w)) => self.jump_to(w.jump_tab, None),
            Some(SidebarEntry::Agent(a)) => self.jump_to(a.tab, Some(a.pane)),
            None => {
                self.mode = Mode::Terminal;
                self.status = None;
                Vec::new()
            }
        }
    }

    /// Select `tab` and hand the keys back to the panes, focusing `pane` —
    /// else a focus already in that tab, else the tab's own active pane. The
    /// one jump the sidebar's rows and the go-to overlay share, so both send
    /// the same `TabSelect` (and `focus_change` the same `PaneFocus`).
    pub(super) fn jump_to(&mut self, tab: TabId, pane: Option<PaneId>) -> Vec<WireFrame> {
        self.mode = Mode::Terminal;
        self.status = None;
        self.active_tab = Some(tab);
        self.zoom = false;
        match pane {
            Some(pane) => self.focused = Some(pane),
            None => self.refocus(),
        }
        vec![control(&Request::TabSelect { id: tab })]
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
        self.launcher.new_tab = Some(workspace);
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
        match self.host.is_jj(&dir) {
            Ok(true) => {}
            Ok(false) => {
                self.set_status(format!("not a jj workspace: {}", dir.display()));
                return Vec::new();
            }
            Err(e) => {
                self.set_status(format!("error: {e:#}"));
                return Vec::new();
            }
        }
        self.mode = Mode::Terminal;
        self.status = None;
        self.active_tab = Some(tab);
        self.zoom = false;
        vec![control(&Request::PaneRun {
            tab: Some(tab),
            cmd: diff_pager_cmd(None),
            ephemeral: true,
            sandbox: None,
        })]
    }

    /// Answer the restore offer: `enter` brings the saved layout back, `d`
    /// discards it, `esc` closes the offer — still pending, it comes back on
    /// the next attach — onto whatever it opened over, a first-run prompt
    /// included. Enter and `d` cancel that prompt instead: the session is no
    /// longer empty, or was emptied on purpose and drops to the sidebar, as the
    /// prompt's own `esc` would. Every other key waits for an answer.
    fn on_key_restore_offer(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        let discard = match key.code {
            KeyCode::Enter => false,
            KeyCode::Char('d') => true,
            KeyCode::Esc => {
                self.mode = self.restore_offer.take().map_or(Mode::Terminal, |(_, m)| m);
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        let under = self.restore_offer.take().map_or(Mode::Terminal, |(_, m)| m);
        self.mode = if under == Mode::SidebarPrompt {
            self.prompt = Prompt::default();
            if discard {
                Mode::Sidebar
            } else {
                Mode::Terminal
            }
        } else {
            under
        };
        vec![control(&Request::SessionRestore { discard })]
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
    pub(super) fn selected_workspace(&self, sidebar: &Sidebar) -> Option<&WorkspaceView> {
        match sidebar.entries.get(self.sidebar_selected)? {
            SidebarEntry::Workspace(w) => self.workspaces.iter().find(|ws| ws.id == w.id),
            SidebarEntry::Agent(a) => self
                .workspaces
                .iter()
                .find(|w| w.tabs.iter().any(|t| t.id == a.tab)),
        }
    }

    /// The tab whose layout holds `pane`, if the current view carries it.
    pub(super) fn tab_of_pane(&self, pane: PaneId) -> Option<TabId> {
        self.workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .find(|t| t.layout.as_ref().is_some_and(|l| l.panes().contains(&pane)))
            .map(|t| t.id)
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

    /// Resolve the merge confirm: `y` dispatches the `WorkspaceMerge` (letting the
    /// server pick the trunk bookmark and push if a remote exists) and arms the
    /// cleanup confirm the `Merged` reply will raise; any other key cancels.
    fn on_key_confirm_merge(&mut self, key: KeyEvent, id: WorkspaceId) -> Vec<WireFrame> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Sidebar;
                let name = self.workspace_name(id).unwrap_or_default();
                self.set_status(format!("merging {name}…"));
                let merge = Request::WorkspaceMerge { id, push: true };
                vec![self.follow(&merge, FollowUp::Merge(id))]
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

    // ---- mouse ----------------------------------------------------------

    /// A mouse event from the terminal. Inside a pane whose program turned on
    /// mouse reporting, a press, its drag and its release — and bare motion,
    /// when the program asked for all of it — go to that program, a press
    /// focusing the pane too, as tmux and WezTerm do; Shift held keeps the
    /// mouse tutti's (select and copy). The wheel, border drags and the chrome
    /// are tutti's: `on_mouse`.
    pub fn on_mouse_event(&mut self, mouse: MouseEvent) -> Vec<WireFrame> {
        if !mouse.modifiers.contains(KeyModifiers::SHIFT)
            && let Some(frames) = self.forward_mouse(mouse)
        {
            return frames;
        }
        self.on_mouse_held(mouse.kind, mouse.column, mouse.row, mouse.modifiers)
    }

    /// The frames handing `mouse` to the program it falls to, or `None` when
    /// it is tutti's. A press over a pane's cells goes to a program reporting
    /// presses, and holds the mouse for that pane: its drags and release go
    /// there too, wherever the pointer is — reported when the program asked
    /// for them, dropped when it did not.
    fn forward_mouse(&mut self, mouse: MouseEvent) -> Option<Vec<WireFrame>> {
        let (col, row) = (mouse.column, mouse.row);
        let pane = match mouse.kind {
            MouseEventKind::Down(_)
                if matches!(self.mode, Mode::Terminal | Mode::Sidebar | Mode::DiffFiles)
                    && self.boundary_at(col, row).is_none() =>
            {
                self.pane_cell_at(col, row)?.0
            }
            MouseEventKind::Moved if self.mode == Mode::Terminal => self.pane_cell_at(col, row)?.0,
            MouseEventKind::Drag(_) => self.mouse_grab?,
            MouseEventKind::Up(_) => self.mouse_grab.take()?,
            _ => return None,
        };
        let (button, action) = match mouse.kind {
            MouseEventKind::Down(b) => (mouse_button(b), input::Action::Press),
            MouseEventKind::Drag(b) => (mouse_button(b), input::Action::Motion),
            MouseEventKind::Up(b) => (mouse_button(b), input::Action::Release),
            _ => (Button::None, input::Action::Motion),
        };
        let (r, c) = self.clamp_to_pane(pane, col, row)?;
        let report = Mouse {
            button,
            action,
            mods: mouse_mods(mouse.modifiers),
            cell: (r, c),
            pixel: (c.into(), r.into()),
        };
        let bytes = {
            let state = self
                .panes
                .get(&pane)
                .filter(|s| s.scroll.is_none() && s.info.exited.is_none())?;
            input::encode_mouse(&report, state.term.modes())
        };
        let mut frames = Vec::new();
        match (mouse.kind, &bytes) {
            (MouseEventKind::Down(_), Some(_)) => {
                self.selection = None;
                self.mouse_grab = Some(pane);
                frames = self.mouse_focus(pane);
            }
            (MouseEventKind::Down(_) | MouseEventKind::Moved, None) => return None,
            _ => {}
        }
        frames.extend(bytes.map(|bytes| WireFrame::Input { pane, bytes }));
        Some(frames)
    }

    /// `on_mouse_held` with no modifier held.
    #[cfg(test)]
    pub fn on_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> Vec<WireFrame> {
        self.on_mouse_held(kind, col, row, KeyModifiers::NONE)
    }

    /// Handle a mouse event as tutti's own — focus, select and copy, border
    /// drags, the chrome, the wheel — returning frames to send to the server.
    /// A wheel tick passes Alt and Ctrl (`held`) on to a program reporting
    /// the mouse.
    fn on_mouse_held(
        &mut self,
        kind: MouseEventKind,
        col: u16,
        row: u16,
        held: KeyModifiers,
    ) -> Vec<WireFrame> {
        // The go-to overlay is modal over the whole screen: a click on an
        // entry jumps there and the wheel walks the highlight; nothing reaches
        // the panes, panels, or tab bar beneath.
        if self.mode == Mode::Navigate {
            match kind {
                MouseEventKind::Down(MouseButton::Left) => return self.navigate_click(col, row),
                MouseEventKind::ScrollDown => self.navigate_step(true),
                MouseEventKind::ScrollUp => self.navigate_step(false),
                _ => {}
            }
            return Vec::new();
        }
        // The restore offer is answered from the keyboard only: a stray click
        // must neither dismiss it nor focus whatever lies beneath.
        if self.mode == Mode::RestoreOffer {
            return Vec::new();
        }
        // The peek, approval and reply overlays are modal: a press inside one
        // is swallowed — the pane beneath must not take focus — and a press
        // outside dismisses it, answering nothing.
        if let Mode::Peek(_) | Mode::Approval(_) | Mode::Reply(_) = self.mode {
            if matches!(kind, MouseEventKind::Down(_))
                && !self.peek.rect.is_some_and(|r| contains(r, col, row))
            {
                self.mode = match self.mode {
                    Mode::Approval(_) => self.approvals.back,
                    _ => Mode::Sidebar,
                };
            }
            return Vec::new();
        }
        // The notifications panel is modal too: a click on a note jumps to
        // its pane and the wheel walks the highlight — its clear confirm is
        // answered from the keyboard — and nothing reaches what lies beneath.
        if matches!(self.mode, Mode::Notes | Mode::ConfirmClearNotes) {
            match kind {
                MouseEventKind::Down(MouseButton::Left) if self.mode == Mode::Notes => {
                    return self.notes_click(col, row);
                }
                MouseEventKind::ScrollDown => self.notes_step(true),
                MouseEventKind::ScrollUp => self.notes_step(false),
                _ => {}
            }
            return Vec::new();
        }
        match kind {
            MouseEventKind::Down(MouseButton::Right) if self.in_sidebar(col, row) => {
                self.sidebar_peek_click(row);
                Vec::new()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Any press drops a prior selection's highlight (and a border
                // whose release went astray) before starting fresh, wherever
                // it lands.
                self.selection = None;
                self.border_drag = self.boundary_at(col, row).map(|boundary| BorderDrag {
                    boundary,
                    grab: boundary.at - boundary.along(col, row),
                    moved: false,
                });
                // A press on the cells where two panes meet grabs their split's
                // border instead of focusing or selecting.
                if self.border_drag.is_some() {
                    return Vec::new();
                }
                if let Some(pane) = self.pane_at(col, row) {
                    let frames = self.mouse_focus(pane);
                    if let Some((pane, r, c)) = self.pane_cell_at(col, row) {
                        self.selection = Some(Selection {
                            pane,
                            anchor: (r, c),
                            head: (r, c),
                            dragged: false,
                        });
                    }
                    return frames;
                }
                if self.in_sidebar(col, row) {
                    return self.sidebar_click(row);
                }
                if self.changes.rect.is_some_and(|r| contains(r, col, row)) {
                    return self.diff_panel_click(row);
                }
                if self.tab_bar_rect.is_some_and(|r| contains(r, col, row)) {
                    return self.tab_bar_click(col);
                }
                if self.notes_chip_rect.is_some_and(|r| contains(r, col, row)) {
                    self.open_notes();
                }
                Vec::new()
            }
            MouseEventKind::Drag(MouseButton::Left) if self.border_drag.is_some() => {
                self.drag_border(col, row)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.extend_selection(col, row);
                Vec::new()
            }
            MouseEventKind::Up(MouseButton::Left) if self.border_drag.is_some() => {
                let still = self.border_drag.take().is_some_and(|drag| !drag.moved);
                // A press that never moved the border was a click: it focuses
                // the pane beneath, as a press anywhere else in it does.
                match self.pane_at(col, row) {
                    Some(pane) if still => self.mouse_focus(pane),
                    _ => Vec::new(),
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.finish_selection();
                Vec::new()
            }
            MouseEventKind::ScrollUp => self.mouse_scroll(col, row, true, held),
            MouseEventKind::ScrollDown => self.mouse_scroll(col, row, false, held),
            _ => Vec::new(),
        }
    }

    /// The split boundary whose border cells hold `(col, row)`. Panes tile
    /// with no gap, each drawing its own frame, so those are the two cells that
    /// meet there: the frame column on either side of side-by-side panes, the
    /// upper pane's bottom frame row and the lower pane's title row of stacked
    /// ones. Where two boundaries cross, the outer split's wins.
    fn boundary_at(&self, col: u16, row: u16) -> Option<Boundary> {
        self.boundaries.iter().copied().find(|b| {
            let along = b.along(col, row);
            contains(b.area, col, row) && (along == b.at || along + 1 == b.at)
        })
    }

    /// Follow a border drag to `(col, row)`: the ratio that brings the grabbed
    /// cell under the pointer, sent only when it moves the border by a cell or
    /// more. The drag predicts exactly where the server will put the border, so
    /// a pointer wandering within one cell, or pinned past the clamp, sends
    /// nothing.
    fn drag_border(&mut self, col: u16, row: u16) -> Vec<WireFrame> {
        let Some(drag) = self.border_drag.as_mut() else {
            return Vec::new();
        };
        let b = drag.boundary;
        let (ratio, at) = b.drag_to(b.along(col, row).saturating_add(drag.grab));
        if at == b.at {
            return Vec::new();
        }
        drag.boundary.at = at;
        drag.moved = true;
        vec![control(&Request::PaneSplitRatio {
            a: b.first,
            b: b.second,
            ratio,
        })]
    }

    /// Extend the live selection's head to the cell under `(col, row)`, clamped
    /// to the interior of the pane the drag began in — so dragging past the pane
    /// edge selects to the edge rather than jumping panes. Marks the selection
    /// `dragged` once the head leaves the anchor cell.
    fn extend_selection(&mut self, col: u16, row: u16) {
        let Some(sel) = self.selection else {
            return;
        };
        let Some((r, c)) = self.clamp_to_pane(sel.pane, col, row) else {
            return;
        };
        if let Some(sel) = self.selection.as_mut() {
            sel.head = (r, c);
            if sel.head != sel.anchor {
                sel.dragged = true;
            }
        }
    }

    /// Finish a drag: copy the selected text to the system clipboard (an OSC 52
    /// write re-emitted to the real terminal) and flash a confirmation. A plain
    /// click — no drag — leaves nothing selected. The highlight is kept after a
    /// copy so the user sees what was taken, until the next press clears it.
    fn finish_selection(&mut self) {
        let Some(text) = self.selected_text() else {
            self.selection = None;
            return;
        };
        let chars = text.chars().count();
        self.terminal_out.push(osc52(&text));
        self.set_status(format!("copied {chars} chars"));
    }

    /// The inclusive selected-cell span `(start, end)` for `pane` in screen
    /// coordinates, if a dragged selection is live there — for the renderer to
    /// paint the highlight. Ordered so `start` precedes `end` in reading order.
    pub fn selection_span(&self, pane: PaneId) -> Option<((u16, u16), (u16, u16))> {
        let sel = self.selection?;
        (sel.pane == pane && sel.dragged).then(|| ordered(sel.anchor, sel.head))
    }

    /// The currently selected text, read from the pane's visible grid as a
    /// stream selection: the first row from its start column to end-of-line, any
    /// whole rows between, and the last row to its end column, each row's
    /// trailing blanks trimmed. `None` unless a drag is live.
    fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        if !sel.dragged {
            return None;
        }
        let term = &self.panes.get(&sel.pane)?.term;
        let (rows, cols) = term.size();
        let (start, end) = ordered(sel.anchor, sel.head);
        let mut out = String::new();
        for r in start.0..=end.0.min(rows.saturating_sub(1)) {
            let c0 = if r == start.0 { start.1 } else { 0 };
            let c1 = if r == end.0 {
                end.1
            } else {
                cols.saturating_sub(1)
            };
            let row = term.screen_row(r);
            let mut line = String::new();
            for c in usize::from(c0)..=usize::from(c1.min(cols.saturating_sub(1))) {
                row.push_grapheme(c, &mut line);
            }
            out.push_str(line.trim_end_matches(' '));
            if r != end.0 {
                out.push('\n');
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Map an absolute terminal point to the pane and 0-based grid cell beneath
    /// it, or `None` when the point is over a pane's border/title rather than its
    /// interior. Used to seed a selection on the exact cell clicked; a point
    /// past a grid smaller than the pane takes the grid's nearest edge cell.
    fn pane_cell_at(&self, col: u16, row: u16) -> Option<(PaneId, u16, u16)> {
        let (pane, rect) = self.rects.iter().find(|(_, r)| contains(*r, col, row))?;
        let (rows, cols) = inner_size(*rect);
        let (x0, y0) = (rect.x + 1, rect.y + 2);
        if col < x0 || row < y0 {
            return None;
        }
        let (c, r) = (col - x0, row - y0);
        let (grid_rows, grid_cols) = self.cell_bounds(*pane, *rect);
        (c < cols && r < rows).then_some((*pane, r.min(grid_rows - 1), c.min(grid_cols - 1)))
    }

    /// Clamp an absolute point to `pane`'s interior, returning the 0-based cell —
    /// so a drag that leaves the pane still resolves to its nearest edge cell.
    fn clamp_to_pane(&self, pane: PaneId, col: u16, row: u16) -> Option<(u16, u16)> {
        let rect = self
            .rects
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| *r)?;
        let (rows, cols) = self.cell_bounds(pane, rect);
        let c = col.saturating_sub(rect.x + 1).min(cols - 1);
        let r = row.saturating_sub(rect.y + 2).min(rows - 1);
        Some((r, c))
    }

    /// The cells a pointer can pick in `pane`, `(rows, cols)`: its rect's
    /// interior, cut to its grid where that is smaller — the grid is sized by
    /// the smallest client viewing the pane, which may be another one.
    fn cell_bounds(&self, pane: PaneId, rect: Rect) -> (u16, u16) {
        let (rows, cols) = inner_size(rect);
        let (grid_rows, grid_cols) = self
            .panes
            .get(&pane)
            .map_or((rows, cols), |s| s.term.size());
        (rows.min(grid_rows).max(1), cols.min(grid_cols).max(1))
    }

    fn in_sidebar(&self, col: u16, row: u16) -> bool {
        self.sidebar_rect.is_some_and(|r| contains(r, col, row))
    }

    /// The app-bar tab segments, left to right: one ` <n> <name> ` per tab
    /// (carrying its id, and a badge for the most urgent state among its
    /// agents — blocked, then done, then working; none for a tab of shells)
    /// then a trailing ` + ` (a `None` target = new tab) — space-padded so the
    /// active segment's background reads as a filled block. Shared by the
    /// renderer and the click hit-test so a click lands on exactly what is
    /// drawn; the renderer joins them with a one-column separator.
    pub fn tab_chips(&self) -> Vec<TabChip> {
        let mut chips: Vec<TabChip> = self
            .all_tabs()
            .iter()
            .enumerate()
            .map(|(i, t)| TabChip {
                target: Some(t.id),
                label: format!(" {} {} ", i + 1, t.name),
                badge: [AgentState::Blocked, AgentState::Done, AgentState::Working]
                    .into_iter()
                    .find(|&state| {
                        t.panes
                            .iter()
                            .any(|p| p.agent.is_some() && p.state == state)
                    }),
            })
            .collect();
        chips.push(TabChip {
            target: None,
            label: " + ".into(),
            badge: None,
        });
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
        for chip in self.tab_chips() {
            let w = chip.width();
            if col >= x && col < x + w {
                return match chip.target {
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

    /// A right-click inside the sidebar: focus it, select the row under the
    /// pointer, and peek at it — the mouse twin of `p`.
    fn sidebar_peek_click(&mut self, row: u16) {
        let Some(rect) = self.sidebar_rect else {
            return;
        };
        if self.sidebar_prompt_active() {
            return;
        }
        let sidebar = self.sidebar();
        if let Some(idx) = sidebar.entry_at_row(row.saturating_sub(rect.y) as usize) {
            self.mode = Mode::Sidebar;
            self.select_entry(&sidebar, idx);
            self.open_peek(&sidebar);
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
    fn mouse_scroll(&mut self, col: u16, row: u16, up: bool, held: KeyModifiers) -> Vec<WireFrame> {
        let Some(pane) = self.pane_at(col, row) else {
            return Vec::new();
        };
        let mut frames = self.mouse_focus(pane);
        if self.panes.get(&pane).is_some_and(|s| s.scroll.is_none())
            && let Some(bytes) = self.forwarded_wheel(pane, col, row, up, held)
        {
            frames.push(WireFrame::Input { pane, bytes });
            return frames;
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

    /// The bytes a wheel tick sends straight to `pane`'s child — a wheel
    /// button with Alt and Ctrl as `held` to a program reporting the mouse,
    /// else on the alternate screen arrow keys, as the program asked for its
    /// keys — or `None` for a primary-screen pane whose scrollback tutti
    /// browses itself.
    fn forwarded_wheel(
        &self,
        pane: PaneId,
        col: u16,
        row: u16,
        up: bool,
        held: KeyModifiers,
    ) -> Option<Vec<u8>> {
        let term = &self.panes.get(&pane)?.term;
        let modes = term.modes();
        if modes.mouse != rtty_vt::MouseMode::None {
            // The cell within the pane interior, below its title and frame rows.
            let (r, c) = self.clamp_to_pane(pane, col, row)?;
            let button = if up {
                Button::WheelUp
            } else {
                Button::WheelDown
            };
            let wheel = Mouse {
                button,
                action: input::Action::Press,
                mods: mouse_mods(held),
                cell: (r, c),
                pixel: (c.into(), r.into()),
            };
            return input::encode_mouse(&wheel, modes);
        }
        if !modes.alt_screen {
            return None;
        }
        let arrow = Key {
            code: if up { Code::Up } else { Code::Down },
            ..Key::default()
        };
        let arrow = input::encode_key(&arrow, modes, term.kitty_keyboard());
        arrow.map(|arrow| arrow.repeat(MOUSE_SCROLL_STEP))
    }

    // ---- layout / sizing ------------------------------------------------

    /// Recompute pane rectangles for the terminal `area` and, when the visible
    /// set moved, send it whole as `ViewSizes` — each shown pane at its
    /// rendered size — so the server sizes every pane by its smallest viewer
    /// (an old daemon gets a `PaneResize` per pane whose size moved instead).
    /// Records the sidebar and tab-bar rects and the split boundaries for
    /// mouse hit-testing.
    pub fn sync_sizes(&mut self, area: Rect) -> Vec<WireFrame> {
        let (content, _footer) = App::content_rect(area);
        let (app_bar, _rule) = App::header_rects(area);
        self.last_content_width = content.width;
        let (sidebar_rect, panes, diff_panel_rect) = self.regions(content);
        self.sidebar_rect = sidebar_rect;
        self.changes.rect = diff_panel_rect;
        self.tab_bar_rect = Some(self.tab_bar_rect(app_bar));
        self.notes_chip_rect = Some(self.notes_chip(app_bar).1);
        self.peek.rect = self
            .peek_popup(area)
            .map(|(_, rect, _)| rect)
            .or_else(|| self.text_popup(area).map(|(_, rect, _)| rect));
        self.navigate.rect = (self.mode == Mode::Navigate).then_some(content);
        self.notes_panel.area = self.notes_popup(area).map(|_| area);
        self.rects = self.compute_rects(content);
        let zoom = if self.zoom { self.focused } else { None };
        self.boundaries = self
            .active_tab_view()
            .and_then(|t| t.layout.as_ref())
            .map_or_else(Vec::new, |layout| boundaries(layout, panes, zoom));
        let view: Vec<PaneDims> = self
            .rects
            .iter()
            .map(|(pane, rect)| {
                let (rows, cols) = inner_size(*rect);
                PaneDims {
                    pane: *pane,
                    rows,
                    cols,
                }
            })
            .collect();
        if view == self.view_sent {
            return Vec::new();
        }
        let out = if self.old_daemon {
            view.iter()
                .filter(|dims| !self.view_sent.contains(dims))
                .map(|&PaneDims { pane, rows, cols }| {
                    control(&Request::PaneResize { pane, rows, cols })
                })
                .collect()
        } else {
            vec![control(&Request::ViewSizes {
                panes: view.clone(),
            })]
        };
        self.view_sent = view;
        out
    }

    pub fn compute_rects(&self, content: Rect) -> Vec<(PaneId, Rect)> {
        let (_, panes, _) = self.regions(content);
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
        self.panes.get(&pane).map(|s| s.term.contents())
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

    pub(super) fn switch_tab(&mut self, delta: isize) -> Vec<WireFrame> {
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

    /// Focus `pane` from a click or wheel tick. Keys dispatch on `mode`, so the
    /// mode follows the focus: prefix and the sidebar/changes panels hand the
    /// keyboard back to the pane, and another pane's scrollback browse ends —
    /// clicking the pane being browsed keeps the browse so a selection can copy
    /// from it.
    fn mouse_focus(&mut self, pane: PaneId) -> Vec<WireFrame> {
        self.focused = Some(pane);
        match self.mode {
            Mode::Prefix(_) => {
                self.mode = Mode::Terminal;
                Vec::new()
            }
            Mode::Sidebar | Mode::DiffFiles => {
                self.mode = Mode::Terminal;
                self.status = None;
                Vec::new()
            }
            Mode::Scroll(scrolled) if scrolled != pane => self.exit_scroll(scrolled),
            _ => Vec::new(),
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

    /// Directional focus that falls through at the layout's edge. Left steps
    /// into the sidebar when it is on screen (nvim-explorer parity), right
    /// into the changes panel; past them — or at the top or bottom — the
    /// move goes on to rtty's pane that way when the TUI runs in rtty
    /// (`past_edge`), and else, left and right, wraps to the next tab.
    fn focus_or_tab(&mut self, dir: FocusDir) -> Vec<WireFrame> {
        if self.move_focus(dir) {
            return Vec::new();
        }
        match dir {
            FocusDir::Left if self.sidebar_rect.is_some() => {
                self.focus_sidebar();
                Vec::new()
            }
            FocusDir::Right if self.changes.rect.is_some() => {
                self.focus_diff_panel();
                Vec::new()
            }
            dir => self.past_edge(dir),
        }
    }

    /// A move past tutti's own edge — its panes' and its panels' — that way:
    /// on to rtty's pane beyond when the TUI runs in rtty, else (and at
    /// rtty's own edge too, left and right) the previous or next tab.
    pub(super) fn past_edge(&mut self, dir: FocusDir) -> Vec<WireFrame> {
        let step = match dir {
            FocusDir::Left => Some(-1),
            FocusDir::Right => Some(1),
            FocusDir::Up | FocusDir::Down => None,
        };
        if self.rtty_handoff(Handoff::Focus(dir.toward()), step) {
            return Vec::new();
        }
        step.map(|step| self.switch_tab(step)).unwrap_or_default()
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
    /// `axis` by `delta` — or, with no split of tutti's on that axis, rtty's
    /// split that way when the TUI runs in rtty.
    fn resize_split(&mut self, axis: Direction, delta: f32) -> Vec<WireFrame> {
        let Some(pane) = self.focused else {
            return Vec::new();
        };
        let split = self
            .active_tab_view()
            .and_then(|tab| tab.layout.as_ref())
            .is_some_and(|layout| layout.resize_split(pane, axis, delta).is_some());
        let toward = match (axis, delta < 0.0) {
            (Direction::Horizontal, true) => Toward::Left,
            (Direction::Horizontal, false) => Toward::Right,
            (Direction::Vertical, true) => Toward::Up,
            (Direction::Vertical, false) => Toward::Down,
        };
        if !split && self.rtty_handoff(Handoff::Resize(toward), None) {
            return Vec::new();
        }
        vec![control(&Request::PaneResizeSplit {
            pane,
            direction: axis,
            delta,
        })]
    }

    pub(super) fn refocus(&mut self) {
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

    pub(super) fn set_status(&mut self, message: String) {
        self.status = Some((message, Instant::now()));
    }

    pub(super) fn active_tab_panes(&self) -> Vec<PaneId> {
        self.active_tab_view()
            .and_then(|t| t.layout.as_ref().map(Layout::panes))
            .unwrap_or_default()
    }

    /// The name of the workspace with `id`, from the current view.
    pub(super) fn workspace_name(&self, id: WorkspaceId) -> Option<String> {
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

/// One app-bar tab segment, as `App::tab_chips` lays it out.
#[derive(Debug)]
pub struct TabChip {
    /// The tab the segment selects; `None` for the trailing ` + ` (new tab).
    pub target: Option<TabId>,
    pub label: String,
    /// The state the segment's dot shows, drawn after the label with a pad.
    pub badge: Option<AgentState>,
}

impl TabChip {
    /// The columns the segment covers — the one measure the renderer draws to
    /// and the click hit-test steps by. Every dot glyph is one column wide.
    pub fn width(&self) -> u16 {
        let dot = if self.badge.is_some() { 2 } else { 0 };
        self.label.chars().count() as u16 + dot
    }
}

#[derive(Clone, Copy)]
pub(super) enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

impl FocusDir {
    fn toward(self) -> Toward {
        match self {
            FocusDir::Left => Toward::Left,
            FocusDir::Right => Toward::Right,
            FocusDir::Up => Toward::Up,
            FocusDir::Down => Toward::Down,
        }
    }

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
pub(super) fn contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

fn center(r: Rect) -> (u16, u16) {
    (r.x + r.width / 2, r.y + r.height / 2)
}

fn mouse_button(button: MouseButton) -> Button {
    match button {
        MouseButton::Left => Button::Left,
        MouseButton::Middle => Button::Middle,
        MouseButton::Right => Button::Right,
    }
}

/// The modifiers a mouse report carries: Alt and Ctrl. Shift keeps the mouse
/// tutti's.
fn mouse_mods(held: KeyModifiers) -> u8 {
    input::ALT * u8::from(held.contains(KeyModifiers::ALT))
        + input::CTRL * u8::from(held.contains(KeyModifiers::CONTROL))
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

/// What a key's press and its release have in common: kitty's code for the
/// key, when the terminal said it, else the key, a letter unshifted.
fn held(key: KeyEvent, report: Option<&Report>) -> KeyCode {
    match (report.and_then(|r| char::from_u32(r.code)), key.code) {
        (Some(code), _) => KeyCode::Char(code),
        (None, KeyCode::Char(c)) => KeyCode::Char(c.to_ascii_lowercase()),
        (None, code) => code,
    }
}

pub(crate) fn control(request: &Request) -> WireFrame {
    WireFrame::Control(serde_json::to_vec(request).expect("serialize request"))
}

/// `control`, tagged with `req` for the server to echo on the answer.
pub(crate) fn tagged(request: &Request, req: u64) -> WireFrame {
    let tagged = Tagged {
        req: Some(req),
        body: request,
    };
    WireFrame::Control(serde_json::to_vec(&tagged).expect("serialize request"))
}

thread_local! {
    /// Set while `fed` feeds a pane frame to its terminal, so the panic hook
    /// leaves the host terminal be for a panic that is about to be caught.
    static FEEDING: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is feeding a pane frame to its terminal — for the
/// panic hook: that panic is caught and the pane reseeded, so the TUI runs on.
pub(crate) fn feeding() -> bool {
    FEEDING.get()
}

/// Feed a pane frame to its terminal, catching a panic as `None`: the
/// terminal it left is suspect. A pane is the unit of failure here as in the
/// daemon — rtty-vt is property-tested never to panic, and this keeps one
/// that did to its pane.
fn fed<T>(feed: impl FnOnce() -> T) -> Option<T> {
    FEEDING.set(true);
    let fed = catch_unwind(AssertUnwindSafe(feed)).ok();
    FEEDING.set(false);
    fed
}

/// Order two `(row, col)` cells so the first precedes the second in reading
/// order (top-to-bottom, then left-to-right).
fn ordered(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// An OSC 52 clipboard-write escape targeting the system clipboard (`c`),
/// carrying `text` base64-encoded and BEL-terminated. Re-emitted to the real
/// terminal, it copies on the user's behalf — so a mouse selection can copy
/// while tutti itself holds the mouse for scroll and select. Requires a terminal
/// that honours OSC 52 (iTerm2, kitty, wezterm, tmux with `set-clipboard on`).
fn osc52(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(base64(text.as_bytes()).as_bytes());
    out.push(0x07);
    out
}

/// Standard-alphabet base64 with `=` padding — enough to wrap a clipboard
/// payload for OSC 52 without pulling in a dependency.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).map_or(0, |&b| b as u32);
        let b2 = chunk.get(2).map_or(0, |&b| b as u32);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
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
        term: Terminal::new(24, 80, 0),
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
        checkpoints: Vec::new(),
        approval: None,
        approvals: Vec::new(),
        source: None,
        sandbox: None,
        issue: None,
        conversation: None,
        sandbox_name: None,
        user_vars: Default::default(),
        cwd: None,
        workspace: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::*;
    use ratatui::crossterm::event::KeyModifiers;
    use tutti_core::{DIFF_RANGE, PaneData};

    #[test]
    fn attach_sets_focus_and_session() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(app.session, "tutti");
        assert_eq!(app.focused, Some(PaneId(1)));
        assert_eq!(app.active_tab, Some(TabId(1)));
    }

    #[test]
    fn snapshot_seeds_the_terminal_then_delta_updates_it() {
        let mut src = Terminal::new(24, 80, 0);
        src.advance(b"hello");
        let mut app = App::new();
        attached(&mut app);
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: src.snapshot(),
        }));
        assert!(app.pane_text(PaneId(1)).unwrap().contains("hello"));

        // A delta is the daemon's redraw of its terminal's damage.
        src.take_damage();
        src.advance(b" world");
        let damage = src.take_damage();
        let delta = src.redraw(&damage);
        app.handle_frame(WireFrame::PaneDelta(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 1,
            bytes: delta,
        }));
        assert!(app.pane_text(PaneId(1)).unwrap().contains("hello world"));
    }

    /// The terminal's cursor takes the shape the focused pane's program set,
    /// and its default while that pane browses its scrollback.
    #[test]
    fn the_cursor_takes_the_focused_panes_shape() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(app.cursor_shape(), 0);
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: b"\x1b[6 q".to_vec(),
        }));
        assert_eq!(app.cursor_shape(), 6, "a steady bar");
        app.set_scroll(PaneId(1), Some(5));
        assert_eq!(app.cursor_shape(), 0);
    }

    /// A program that asked for focus reports hears when its pane gains and
    /// loses the focus — the terminal's too — and only then.
    #[test]
    fn focus_reports_reach_the_pane_that_asked() {
        let mut app = App::new();
        attached(&mut app);
        let reports = |app: &mut App| -> Vec<Vec<u8>> {
            app.focus_reports()
                .into_iter()
                .map(|frame| match frame {
                    WireFrame::Input {
                        pane: PaneId(1),
                        bytes,
                    } => bytes,
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: b"\x1b[?1004h".to_vec(),
        }));
        assert_eq!(reports(&mut app), [b"\x1b[I"]);
        assert!(reports(&mut app).is_empty());
        app.host_focus(false);
        assert_eq!(reports(&mut app), [b"\x1b[O"]);
        app.host_focus(true);
        assert_eq!(reports(&mut app), [b"\x1b[I"]);
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: b"".to_vec(),
        }));
        app.host_focus(false);
        assert!(reports(&mut app).is_empty(), "it stopped asking");
    }

    /// Keys go to the focused pane as its program asked for them: plain
    /// until it pushes kitty's flags, then Ctrl+I apart from Tab.
    #[test]
    fn keys_follow_the_focused_panes_keyboard_protocol() {
        let mut app = App::new();
        attached(&mut app);
        let snapshot = |bytes: &[u8]| {
            WireFrame::PaneSnapshot(PaneData {
                pane: PaneId(1),
                rows: 24,
                cols: 80,
                seq: 0,
                bytes: bytes.to_vec(),
            })
        };
        let typed = |app: &mut App| match app.on_key(ctrl('i')).as_slice() {
            [WireFrame::Input { bytes, .. }] => bytes.clone(),
            frames => panic!("{frames:?}"),
        };
        app.handle_frame(snapshot(b""));
        assert_eq!(typed(&mut app), b"\t");
        app.handle_frame(snapshot(b"\x1b[>1u"));
        assert_eq!(typed(&mut app), b"\x1b[105;5u");
    }

    /// Two panes, each with the kitty flags its program pushed.
    fn kitty_panes(app: &mut App, first: &[u8], second: &[u8]) {
        attach_with(app, view_two_panes());
        for (pane, bytes) in [(1, first), (2, second)] {
            app.handle_frame(WireFrame::PaneSnapshot(PaneData {
                pane: PaneId(pane),
                rows: 24,
                cols: 80,
                seq: 0,
                bytes: bytes.to_vec(),
            }));
        }
        app.focused = Some(PaneId(1));
    }

    /// What the terminal sent, as the TUI's reader decodes it.
    fn typed_in(app: &mut App, bytes: &[u8]) -> Vec<(PaneId, String)> {
        Decoder::default()
            .feed(bytes)
            .into_iter()
            .flat_map(|input| match input {
                Input::Key(key, report) => app.on_typed(key, report.as_ref()),
                input => panic!("{input:?}"),
            })
            .filter_map(|frame| match frame {
                WireFrame::Input { pane, bytes } => Some((pane, String::from_utf8(bytes).unwrap())),
                _ => None,
            })
            .collect()
    }

    /// The TUI asks its terminal for what the focused pane's program asked
    /// for beyond disambiguated keys and their alternates: event types, and
    /// every key an escape code — with the text, always, for a prompt.
    #[test]
    fn the_terminals_kitty_flags_follow_the_focused_panes_program() {
        let mut app = App::new();
        kitty_panes(&mut app, b"\x1b[>3u", b"\x1b[>11u");
        assert_eq!(app.key_flags(), 1 | 2 | 4, "nvim's 1|2");
        app.focused = Some(PaneId(2));
        assert_eq!(app.key_flags(), 1 | 2 | 4 | 8 | 16);
        kitty_panes(&mut app, b"", b"\x1b[>16u");
        assert_eq!(app.key_flags(), 1 | 4);
        app.focused = Some(PaneId(2));
        assert_eq!(app.key_flags(), 1 | 4, "text means nothing without 8");
    }

    /// A release goes to the pane that had its press, as its program asked
    /// for it — after a move of the focus too — and never one whose press
    /// was tutti's: the prefix, a direct chord. A repeat is a press; a lone
    /// modifier reaches only a program that asked for every key, and never
    /// ends the prefix.
    #[test]
    fn releases_go_where_their_press_went_and_never_from_tuttis_keys() {
        let mut app = App::new();
        kitty_panes(&mut app, b"\x1b[>3u", b"\x1b[>27u");
        let one = |s: &str| vec![(PaneId(1), s.to_string())];
        assert_eq!(typed_in(&mut app, b"\x1b[27u"), one("\x1b[27u"));
        assert_eq!(typed_in(&mut app, b"\x1b[27;1:2u"), one("\x1b[27;1:2u"));
        assert_eq!(typed_in(&mut app, b"\x1b[27;1:3u"), one("\x1b[27;1:3u"));
        assert!(
            typed_in(&mut app, b"\x1b[27;1:3u").is_empty(),
            "released once"
        );
        assert_eq!(
            typed_in(&mut app, b"a\x1b[57441;2u"),
            one("a"),
            "no lone Shift for 1|2"
        );

        // Ctrl+A held while the focus moves: its release follows the press.
        assert_eq!(typed_in(&mut app, b"\x1b[97;5u"), one("\x1b[97;5u"));
        assert!(
            typed_in(&mut app, b"\x1b[108;3u").is_empty(),
            "Alt+L is tutti's"
        );
        app.focused = Some(PaneId(2));
        assert!(typed_in(&mut app, b"\x1b[108;3:3u").is_empty());
        assert_eq!(typed_in(&mut app, b"\x1b[97;5:3u"), one("\x1b[97;5:3u"));
        assert_eq!(
            typed_in(&mut app, b"\x1b[57441;2u\x1b[97:65;2;65u\x1b[57441;1:3u"),
            [
                (PaneId(2), "\x1b[57441;2u".to_string()),
                (PaneId(2), "\x1b[97;2;65u".to_string()),
                (PaneId(2), "\x1b[57441;1:3u".to_string()),
            ]
        );

        // The prefix, its release, a lone Shift, then `%`: a split, and the
        // pane hears none of it.
        assert!(typed_in(&mut app, b"\x1b[98;5u\x1b[98;5:3u").is_empty());
        assert!(matches!(app.mode, Mode::Prefix(_)));
        assert!(typed_in(&mut app, b"\x1b[57441;2u").is_empty());
        assert!(
            matches!(app.mode, Mode::Prefix(_)),
            "a lone modifier is no prefix key"
        );
        let split = app.on_typed(KeyEvent::new(KeyCode::Char('%'), KeyModifiers::NONE), None);
        assert!(
            split.iter().all(|f| !matches!(f, WireFrame::Input { .. })) && !split.is_empty(),
            "{split:?}"
        );
        assert!(typed_in(&mut app, b"\x1b[53:37;2:3u\x1b[57441;1:3u").is_empty());
        assert_eq!(app.mode, Mode::Terminal);
    }

    /// A key pressed into a pane that hears releases stays that pane's while
    /// held: its repeats and release reach it after the focus moved, and in
    /// another mode, and the terminal keeps reporting releases until the last
    /// is in. A key the pane hears no release of holds nothing, and a repeat
    /// no pane holds is a press where the focus is.
    #[test]
    fn a_held_key_stays_with_its_pane_and_keeps_releases_on() {
        let mut app = App::new();
        kitty_panes(&mut app, b"\x1b[>3u", b"");
        let one = |s: &str| vec![(PaneId(1), s.to_string())];
        assert_eq!(typed_in(&mut app, b"\x1b[97;5u"), one("\x1b[97;5u"));
        app.focused = Some(PaneId(2));
        assert_eq!(app.key_flags(), 1 | 2 | 4, "Ctrl+A is held in pane 1");
        assert_eq!(typed_in(&mut app, b"\x1b[97;5:2u"), one("\x1b[97;5:2u"));
        app.mode = Mode::Sidebar;
        assert_eq!(typed_in(&mut app, b"\x1b[97;5:2u"), one("\x1b[97;5:2u"));
        assert_eq!(typed_in(&mut app, b"\x1b[97;5:3u"), one("\x1b[97;5:3u"));
        assert_eq!(app.key_flags(), 1 | 4, "pane 2 asked for no releases");
        app.mode = Mode::Terminal;

        app.focused = Some(PaneId(1));
        assert_eq!(typed_in(&mut app, b"x"), one("x"));
        app.focused = Some(PaneId(2));
        assert_eq!(app.key_flags(), 1 | 4, "pane 1 hears no release of text");
        assert_eq!(
            typed_in(&mut app, b"\x1b[120;1:2u"),
            [(PaneId(2), "x".to_string())]
        );
    }

    /// A paste reaches the focused pane whole — inside the paste marks once
    /// its program set bracketed paste, so a shell runs none of its lines —
    /// and a prompt types it in, as the keys its bytes are.
    #[test]
    fn a_paste_reaches_the_pane_whole_and_a_prompt_as_typing() {
        let mut app = App::new();
        attached(&mut app);
        let snapshot = |bytes: &[u8]| {
            WireFrame::PaneSnapshot(PaneData {
                pane: PaneId(1),
                rows: 24,
                cols: 80,
                seq: 0,
                bytes: bytes.to_vec(),
            })
        };
        let pasted = |app: &mut App| match app.on_paste(b"ls\rrm -rf x\r").as_slice() {
            [WireFrame::Input { pane, bytes }] if *pane == PaneId(1) => bytes.clone(),
            frames => panic!("{frames:?}"),
        };
        app.handle_frame(snapshot(b""));
        assert_eq!(pasted(&mut app), b"ls\rrm -rf x\r");
        app.handle_frame(snapshot(b"\x1b[?2004h"));
        assert_eq!(pasted(&mut app), b"\x1b[200~ls\rrm -rf x\r\x1b[201~");

        app.open_project_prompt();
        app.prompt.input.clear();
        assert!(app.on_paste(b"/nonexistent/src").is_empty());
        assert_eq!(app.prompt.input, "/nonexistent/src");
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

    /// What makes a pane's terminal panic, through the test hook in
    /// `PaneState::feed`.
    pub(super) const PANIC_FRAME: &[u8] = b"tutti\x01panic";

    /// A frame that panics its pane's terminal resets that pane blank and
    /// asks the server to reseed it, instead of taking the TUI down.
    #[test]
    fn a_frame_that_panics_the_terminal_reseeds_its_pane() {
        let mut app = App::new();
        attached(&mut app);
        let frame = |rows, cols, bytes: &[u8]| PaneData {
            pane: PaneId(1),
            rows,
            cols,
            seq: 0,
            bytes: bytes.to_vec(),
        };
        let reseed = vec![control(&Request::PaneScroll {
            pane: PaneId(1),
            offset: 0,
        })];
        assert_eq!(
            app.handle_frame(WireFrame::PaneSnapshot(frame(2, 1, PANIC_FRAME))),
            reseed
        );
        assert_eq!(app.panes[&PaneId(1)].term.size(), (2, 1));
        assert_eq!(app.pane_text(PaneId(1)).unwrap(), "", "reset blank");
        assert!(app.transient().is_some_and(|t| t.contains("reseeding")));
        // A delta can break a seeded pane the same way.
        assert!(
            app.handle_frame(WireFrame::PaneSnapshot(frame(2, 1, b"x")))
                .is_empty()
        );
        assert_eq!(
            app.handle_frame(WireFrame::PaneDelta(frame(2, 1, PANIC_FRAME))),
            reseed
        );
        assert!(!feeding(), "the panic hook is armed again");
        // The reseed lands like any snapshot.
        assert!(
            app.handle_frame(WireFrame::PaneSnapshot(frame(2, 4, b"ok")))
                .is_empty()
        );
        assert!(app.pane_text(PaneId(1)).unwrap().contains("ok"));
    }

    /// Another client viewing a pane smaller leaves its grid smaller than its
    /// rect here: a click past the grid lands on the grid's nearest cell.
    #[test]
    fn a_click_past_a_smaller_grid_clamps_to_its_edge() {
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        attached(&mut app);
        app.sync_sizes(Rect::new(0, 0, 80, 24)); // an 18x78 interior from (1, 4)
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 5,
            cols: 10,
            seq: 0,
            bytes: b"0123456789".to_vec(),
        }));
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), 60, 15);
        app.on_mouse(MouseEventKind::Drag(MouseButton::Left), 1, 4);
        assert_eq!(app.selection_span(PaneId(1)), Some(((0, 0), (4, 9))));
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
        assert!(matches!(app.mode, Mode::Prefix(_)));
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
    fn sync_sizes_sends_the_visible_set_once_per_change() {
        // Sidebar off so the lone pane keeps the full width. The chrome costs
        // four rows of the pane's height: the two-row app-bar header, the footer,
        // and the pane's own title line above its frame (plus the two borders).
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        attached(&mut app);
        let area = Rect::new(0, 0, 80, 24);
        let dims = |pane, rows, cols| PaneDims {
            pane: PaneId(pane),
            rows,
            cols,
        };
        assert_eq!(
            app.sync_sizes(area),
            vec![control(&Request::ViewSizes {
                panes: vec![dims(1, 18, 78)],
            })]
        );
        // Same size again produces nothing.
        assert!(app.sync_sizes(area).is_empty());
        // A split changes the set: it goes out whole, both panes in it.
        let mut view = view_one_pane();
        view[0].tabs[0].layout = Some(split(Direction::Horizontal, leaf(1), leaf(2)));
        view[0].tabs[0].panes.push(shell(2));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        assert_eq!(
            app.sync_sizes(area),
            vec![control(&Request::ViewSizes {
                panes: vec![dims(1, 18, 38), dims(2, 18, 38)],
            })]
        );
        // A tab with nothing in it shows no pane: the empty set retracts the
        // client's votes.
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged {
                workspaces: Vec::new(),
            })
            .unwrap(),
        ));
        assert_eq!(
            app.sync_sizes(area),
            vec![control(&Request::ViewSizes { panes: Vec::new() })]
        );
    }

    /// A daemon that answered the attach untagged predates `ViewSizes`: the
    /// client sends it a `PaneResize` per pane whose size moved, as before.
    #[test]
    fn an_old_daemon_gets_a_resize_per_moved_pane() {
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: 7,
                session: "t".into(),
                workspaces: view_one_pane(),
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            app.sync_sizes(area),
            vec![control(&Request::PaneResize {
                pane: PaneId(1),
                rows: 18,
                cols: 78,
            })]
        );
        assert!(app.sync_sizes(area).is_empty());
        assert_eq!(
            app.sync_sizes(Rect::new(0, 0, 80, 30)),
            vec![control(&Request::PaneResize {
                pane: PaneId(1),
                rows: 24,
                cols: 78,
            })]
        );
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
    fn state_changed_updates_the_sidebar_view_without_a_layout_event() {
        use crate::attach::fixtures::agent;
        let mut app = App::new();
        attach_with(
            &mut app,
            vec![workspace(
                1,
                "api",
                Some("main"),
                vec![tab(
                    1,
                    "1",
                    true,
                    leaf(2),
                    vec![agent(2, "claude", AgentState::Working)],
                )],
            )],
        );
        let row_state = |app: &App| {
            app.sidebar().entries.iter().find_map(|e| match e {
                SidebarEntry::Agent(a) if a.pane == PaneId(2) => Some(a.state),
                _ => None,
            })
        };
        assert_eq!(row_state(&app), Some(AgentState::Working));

        state_changed(&mut app, PaneId(2), AgentState::Working, AgentState::Done);
        assert_eq!(
            row_state(&app),
            Some(AgentState::Done),
            "the sidebar follows the transition without a LayoutChanged"
        );
        assert_eq!(app.agent_census().done, 1, "so does the app-bar census");

        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::PaneExited {
                pane: PaneId(2),
                code: 0,
            })
            .unwrap(),
        ));
        let exited = app
            .workspaces
            .iter()
            .flat_map(tab_infos)
            .find(|i| i.id == PaneId(2))
            .and_then(|i| i.exited);
        assert_eq!(exited, Some(0), "the view learns of the exit too");
    }

    #[test]
    fn focus_change_emits_panefocus_on_attach_then_dedupes() {
        let mut app = App::new();
        attached(&mut app);
        assert_eq!(
            app.focus_change(),
            Some(control(&Request::PaneFocus {
                pane: PaneId(1),
                seen: false
            })),
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
                restorable: None,
                notes: Vec::new(),
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

    /// A tab opened elsewhere — say an orchestrating agent's `tutti run` —
    /// arrives as a plain `LayoutChanged` and never moves the view, even when
    /// it flags the new tab current: the client keeps its own tab and focus.
    #[test]
    fn a_tab_opened_elsewhere_does_not_yank_the_view() {
        let mut app = App::new();
        attached(&mut app);
        let mut view = view_one_pane();
        view[0].tabs[0].active = false;
        view[0]
            .tabs
            .push(tab(2, "2", true, leaf(2), vec![shell(2)]));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        assert_eq!(app.active_tab, Some(TabId(1)));
        assert_eq!(app.focused, Some(PaneId(1)));
        assert!(
            app.panes.contains_key(&PaneId(2)),
            "the new pane is tracked"
        );
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
    fn ctrl_l_at_the_right_edge_steps_into_the_changes_panel_when_shown() {
        let mut app = App::new();
        attach_with(&mut app, view_with_changes());
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        assert_eq!(app.mode, Mode::Terminal);
        let out = app.on_key(ctrl('l'));
        assert!(
            out.is_empty(),
            "stepping into the panel emits no tab select"
        );
        assert_eq!(app.mode, Mode::DiffFiles);
        assert_eq!(app.active_tab, Some(TabId(1)), "the tab is unchanged");

        // The panel's edges behave like a pane's: left steps back keeping the
        // pane focus, right (or vim `l`) wraps on to the next tab.
        app.on_key(ctrl('h'));
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.focused, Some(PaneId(1)), "the pane keeps its focus");
        app.on_key(ctrl('l'));
        assert_eq!(app.mode, Mode::DiffFiles);
        app.on_key(ctrl('l'));
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "focus-right from the panel leaves it"
        );
        app.on_key(ctrl('l'));
        app.on_key(plain('l'));
        assert_eq!(app.mode, Mode::Terminal, "vim l mirrors focus-right");

        // With the panel hidden (a clean workspace) the right edge still wraps
        // to the next tab.
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        let out = app.on_key(ctrl('l'));
        assert_eq!(out, vec![control(&Request::TabSelect { id: TabId(2) })]);
    }

    #[test]
    fn prefix_verbs_work_from_inside_the_sidebar_and_the_changes_panel() {
        let mut app = App::new();
        attach_with(&mut app, view_with_changes());
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        assert_eq!(app.mode, Mode::DiffFiles);
        // From the panel, prefix-w hops straight to the sidebar …
        app.on_key(ctrl('b'));
        app.on_key(plain('w'));
        assert_eq!(
            app.mode,
            Mode::Sidebar,
            "prefix w reaches across from the panel"
        );
        // … and from the sidebar, prefix-f hops back to the panel.
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        assert_eq!(
            app.mode,
            Mode::DiffFiles,
            "prefix f reaches across from the sidebar"
        );
        // Detach is reachable from a focused panel too.
        app.on_key(ctrl('b'));
        let out = app.on_key(plain('q'));
        assert!(
            out.iter().any(|f| matches!(f, WireFrame::Control(_))),
            "prefix q from the panel still detaches: {out:?}"
        );
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

    /// An editor holding the direct keys hands its edge back through the
    /// daemon (`FocusEdge`), and the move goes on as the key itself would
    /// take it — left into the sidebar, right on to the next tab — but only
    /// for the pane shown focused, and only from the panes.
    #[test]
    fn a_focus_edge_goes_on_as_the_direct_key_would() {
        let edge = |pane, toward| {
            let event = Event::FocusEdge {
                pane: PaneId(pane),
                toward,
            };
            WireFrame::Control(serde_json::to_vec(&event).unwrap())
        };
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        assert!(app.handle_frame(edge(9, Toward::Left)).is_empty());
        assert_eq!(app.mode, Mode::Terminal, "another pane's edge");
        assert!(app.handle_frame(edge(1, Toward::Up)).is_empty());
        assert_eq!(app.mode, Mode::Terminal, "an upper edge goes nowhere");
        assert!(app.handle_frame(edge(1, Toward::Left)).is_empty());
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(app.handle_frame(edge(1, Toward::Right)).is_empty());
        assert_eq!(app.mode, Mode::Sidebar, "a late edge leaves the sidebar be");
        app.on_key(ctrl('l'));
        assert_eq!(
            app.handle_frame(edge(1, Toward::Right)),
            vec![control(&Request::TabSelect { id: TabId(2) })],
            "no changes panel: on to the next tab"
        );
        assert_eq!(app.active_tab, Some(TabId(2)));
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

    /// A pane whose program set the user var `TUTTI_KEYS=pane` gets the direct
    /// chords as its own bytes, as unbound keys; the prefix stays tutti's, and
    /// any other value — or the pane exiting — hands the chords back.
    #[test]
    fn tutti_keys_pane_hands_the_direct_chords_to_the_pane() {
        let with_keys = |value: &str, exited| {
            let mut view = view_2x2();
            let pane = &mut view[0].tabs[0].panes[0];
            pane.user_vars.insert("TUTTI_KEYS".into(), value.into());
            pane.exited = exited;
            view
        };
        let mut app = App::new();
        attach_with(&mut app, with_keys("pane", None));
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        let input = |bytes: &[u8]| {
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: bytes.to_vec(),
            }]
        };
        assert_eq!(app.on_key(ctrl('l')), input(&[0x0c]));
        assert_eq!(app.on_key(alt('h')), input(b"\x1bh"));
        assert_eq!(app.on_key(alt('x')), input(b"\x1bx"));
        assert_eq!((app.focused, app.mode), (Some(PaneId(1)), Mode::Terminal));
        app.on_key(ctrl('b'));
        assert!(
            matches!(app.mode, Mode::Prefix(_)),
            "the prefix is still tutti's"
        );
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        layout_changed(&mut app, with_keys("pane", Some(0)));
        assert!(
            app.on_key(ctrl('j')).is_empty(),
            "an exited pane claims nothing"
        );
        assert_eq!(app.focused, Some(PaneId(3)));
        app.on_key(ctrl('k'));
        layout_changed(&mut app, with_keys("tutti", None));
        assert!(app.on_key(ctrl('l')).is_empty());
        assert_eq!(
            app.focused,
            Some(PaneId(2)),
            "only `pane` claims the chords"
        );
    }

    #[test]
    fn whichkey_popup_appears_only_after_the_delay() {
        let mut app = App::new();
        attached(&mut app);
        app.on_key(ctrl('b'));
        assert!(matches!(app.mode, Mode::Prefix(_)));
        assert!(!app.whichkey_visible(), "hidden immediately after prefix");

        // Simulate the delay elapsing.
        app.mode = Mode::Prefix(Instant::now() - WHICHKEY_DELAY - Duration::from_millis(10));
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
        assert!(!matches!(app.mode, Mode::Prefix(_)));
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

    #[test]
    fn a_scripted_focus_jumps_across_workspaces() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        assert_eq!(app.active_tab, Some(TabId(1)));
        app.focus_change();
        let focused =
            |pane| WireFrame::Control(serde_json::to_vec(&Event::PaneFocused { pane }).unwrap());

        app.handle_frame(focused(PaneId(3)));
        assert_eq!(app.active_tab, Some(TabId(2)));
        assert_eq!(app.focused, Some(PaneId(3)));
        assert_eq!(
            app.focus_change(),
            Some(control(&Request::PaneFocus {
                pane: PaneId(3),
                seen: false
            }))
        );

        app.handle_frame(focused(PaneId(9)));
        assert_eq!(
            app.focused,
            Some(PaneId(3)),
            "an unknown pane moves nothing"
        );
    }

    #[test]
    fn sidebar_visibility_follows_config_and_width() {
        fn auto() -> App {
            App::with_config(Config::parse("sidebar = \"auto\"\n").unwrap())
        }

        // auto: hidden for a single plain workspace.
        let mut app = auto();
        attached(&mut app);
        let (sb, panes, _) = app.split_content(Rect::new(0, 0, 100, 24));
        assert!(sb.is_none(), "auto hides an agentless single workspace");
        assert_eq!(panes.width, 100);

        // auto: shown with multiple workspaces, panes shrink by the column width.
        let mut app = auto();
        attach_with(&mut app, view_two_workspaces());
        let (sb, panes, _) = app.split_content(Rect::new(0, 0, 100, 24));
        assert_eq!(sb.unwrap().width, 30);
        assert_eq!(panes.width, 70);
        assert_eq!(panes.x, 30);

        // width floor suppresses it regardless of contents.
        let (sb, panes, _) = app.split_content(Rect::new(0, 0, 79, 24));
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
            Some(control(&Request::PaneFocus {
                pane: PaneId(2),
                seen: false
            })),
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
                cmd: diff_pager_cmd(None),
                ephemeral: true,
                sandbox: None,
            })],
            "d spawns the ephemeral jj-diff pane in the workspace's tab"
        );
        assert!(
            diff_pager_cmd(None)[2].contains(DIFF_RANGE),
            "the pager diffs against trunk, not the bare working copy"
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

        // `y` dispatches the merge (push requested; the server owns the bookmark),
        // tagged so its reply alone completes the follow-up.
        let out = app.on_key(plain('y'));
        assert_eq!(
            out,
            vec![tagged(
                &Request::WorkspaceMerge {
                    id: WorkspaceId(2),
                    push: true,
                },
                1
            )],
            "y sends the merge request"
        );

        // The Merged reply flashes the outcome and raises the cleanup confirm.
        app.handle_response(
            Some(1),
            Response::Merged {
                pushed: false,
                bookmark: "main".into(),
            },
        );
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
        app.handle_response(
            Some(1),
            Response::Merged {
                pushed: true,
                bookmark: "main".into(),
            },
        );
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
        app.handle_response(
            Some(1),
            Response::Merged {
                pushed: false,
                bookmark: "main".into(),
            },
        );
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
        app.handle_response(
            None,
            Response::Attached {
                session: "t".into(),
                workspaces: Vec::new(),
                wire_rev: 0,
                restorable: None,
                notes: Vec::new(),
            },
        );
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

    /// Attach to a daemon that has a saved layout to offer back.
    fn attach_offering(app: &mut App, workspaces: Vec<WorkspaceView>) {
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "t".into(),
                workspaces,
                restorable: Some(RestoreSummary {
                    workspaces: 3,
                    panes: 7,
                    agents: 2,
                    saved_at: 0,
                }),
                notes: Vec::new(),
            })
            .unwrap(),
        ));
    }

    fn restore(discard: bool) -> Vec<WireFrame> {
        vec![control(&Request::SessionRestore { discard })]
    }

    #[test]
    fn an_offered_layout_opens_the_restore_offer_and_enter_restores_it() {
        let mut app = App::new();
        attach_offering(&mut app, view_one_pane());
        assert_eq!(app.mode, Mode::RestoreOffer);
        assert_eq!(app.restore_offer().map(|offer| offer.panes), Some(7));
        assert!(app.on_key(plain('x')).is_empty(), "other keys wait");
        assert_eq!(app.mode, Mode::RestoreOffer);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.on_key(enter), restore(false));
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.restore_offer(), None);
    }

    /// A view could neither restore nor discard: it is offered nothing.
    #[test]
    fn a_view_is_offered_no_layout() {
        let mut app = App::new();
        app.view = true;
        attach_offering(&mut app, view_one_pane());
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.restore_offer(), None);
    }

    #[test]
    fn the_restore_offer_ignores_the_mouse() {
        let mut app = App::new();
        attach_offering(&mut app, view_one_pane());
        app.sync_sizes(Rect::new(0, 0, 120, 40));
        let (_, rect) = app.rects[0];
        let click = MouseEventKind::Down(MouseButton::Left);
        assert!(app.on_mouse(click, rect.x + 2, rect.y + 3).is_empty());
        assert_eq!(
            app.mode,
            Mode::RestoreOffer,
            "a click neither dismisses it…"
        );
        assert!(
            app.selection.is_none(),
            "…nor reaches the pane beneath (a press there starts a selection)"
        );
    }

    #[test]
    fn d_discards_the_offer_and_esc_leaves_it_for_the_next_attach() {
        let mut app = App::new();
        attach_offering(&mut app, view_one_pane());
        assert_eq!(app.on_key(plain('d')), restore(true));
        assert_eq!(app.mode, Mode::Terminal);

        let mut app = App::new();
        attach_offering(&mut app, view_one_pane());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            app.on_key(esc).is_empty(),
            "esc sends nothing: the offer stays pending on the daemon"
        );
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.restore_offer(), None);
    }

    #[test]
    fn the_restore_offer_comes_before_the_first_run_prompt() {
        let offered = || {
            let mut app = App::new();
            app.start_first_run_prompt("/tmp/proj".into());
            attach_offering(&mut app, vec![]);
            assert_eq!(app.mode, Mode::RestoreOffer);
            app
        };
        let mut app = offered();
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            app.sidebar_prompt_active(),
            "esc falls through to the prompt"
        );
        assert_eq!(app.sidebar_prompt(), "/tmp/proj");

        let mut app = offered();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.on_key(enter), restore(false));
        assert_eq!(app.mode, Mode::Terminal, "the restored panes take the keys");
        assert!(app.sidebar_prompt().is_empty(), "the prompt is cancelled");

        let mut app = offered();
        assert_eq!(app.on_key(plain('d')), restore(true));
        assert_eq!(app.mode, Mode::Sidebar, "as the prompt's own esc would");
        assert!(app.sidebar_prompt().is_empty(), "the prompt is cancelled");
    }

    #[test]
    fn a_restore_reply_flashes_its_outcome_and_follows_the_restored_current_tab() {
        let mut app = App::new();
        attached(&mut app);
        let mut restored = view_two_workspaces();
        restored[0].tabs[0].active = false;
        restored[1].tabs[0].active = true;
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged {
                workspaces: restored,
            })
            .unwrap(),
        ));
        assert_eq!(app.active_tab, Some(TabId(1)), "a view alone keeps the tab");
        app.handle_response(
            None,
            Response::SessionRestored {
                workspaces: 2,
                panes: 3,
                revived: 2,
                skipped: vec!["/srv/gone: the directory is gone".into()],
            },
        );
        assert_eq!(app.active_tab, Some(TabId(2)));
        assert_eq!(app.focused, Some(PaneId(2)));
        assert_eq!(
            app.transient(),
            Some("restored 2 projects · revived 2 agents · skipped 1")
        );

        app.handle_response(
            None,
            Response::SessionRestored {
                workspaces: 0,
                panes: 0,
                revived: 0,
                skipped: Vec::new(),
            },
        );
        assert_eq!(app.transient(), Some("discarded the saved layout"));
        assert_eq!(app.active_tab, Some(TabId(2)), "a discard moves nothing");
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

    #[test]
    fn a_tab_chip_badges_its_most_urgent_agent() {
        use tutti_core::AgentState::{Blocked, Done, Idle, Working};
        // An exited plain shell is `Done` too, but only agents earn a dot.
        let mut exited = shell(9);
        exited.state = Done;
        exited.exited = Some(0);
        let tabs = vec![
            tab(1, "1", true, leaf(1), vec![shell(1)]),
            tab(
                2,
                "2",
                false,
                leaf(2),
                vec![agent(2, "claude", Working), agent(3, "codex", Done)],
            ),
            tab(3, "3", false, leaf(4), vec![agent(4, "claude", Working)]),
            tab(
                4,
                "4",
                false,
                leaf(5),
                vec![
                    agent(5, "claude", Done),
                    agent(6, "claude", Blocked),
                    agent(7, "codex", Working),
                ],
            ),
            tab(
                5,
                "5",
                false,
                leaf(8),
                vec![agent(8, "claude", Idle), exited],
            ),
        ];
        let mut app = App::new();
        attach_with(&mut app, vec![workspace(1, "w", None, tabs)]);
        let badges: Vec<Option<AgentState>> = app.tab_chips().iter().map(|c| c.badge).collect();
        assert_eq!(
            badges,
            vec![None, Some(Done), Some(Working), Some(Blocked), None, None],
            "blocked beats done beats working; shells, idle agents, and the + earn none"
        );
    }

    #[test]
    fn a_badged_tab_chip_still_clicks_true() {
        let mut app = App::new();
        attach_with(
            &mut app,
            vec![workspace(
                1,
                "w",
                None,
                vec![
                    tab(1, "1", true, leaf(1), vec![shell(1)]),
                    tab(
                        2,
                        "2",
                        false,
                        leaf(2),
                        vec![agent(2, "claude", AgentState::Blocked)],
                    ),
                ],
            )],
        );
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        // " 1 1 " (5), " 2 2 " plus its dot and pad (7), and " + " (3), with
        // two separators: 17 columns from col 83, so tab 2 covers 89-95 — its
        // dot on 94, the pad on 95 — and the + starts at 97.
        let widths: Vec<u16> = app.tab_chips().iter().map(TabChip::width).collect();
        assert_eq!(widths, vec![5, 7, 3]);
        for col in [89, 94, 95] {
            assert_eq!(
                app.on_mouse(MouseEventKind::Down(MouseButton::Left), col, 0),
                vec![control(&Request::TabSelect { id: TabId(2) })],
                "col {col} is tab 2's segment"
            );
        }
        assert!(
            app.on_mouse(MouseEventKind::Down(MouseButton::Left), 96, 0)
                .is_empty(),
            "the separator selects nothing"
        );
        assert_eq!(
            app.on_mouse(MouseEventKind::Down(MouseButton::Left), 97, 0),
            vec![control(&Request::TabNew {
                workspace: Some(WorkspaceId(1)),
            })]
        );
    }

    // ---- mouse wheel ----------------------------------------------------

    /// Seed pane 1's terminal with `bytes` so its declared modes (alt screen,
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
                bytes: b"\x1b[<64;5;2M".to_vec(),
            }],
            "the program asked for mouse events, so it gets the wheel itself, \
             at the cell under the pointer: below the title and frame rows"
        );
        assert_eq!(app.mode, Mode::Terminal);
    }

    /// A forwarded wheel tick carries Alt and Ctrl, as a click does; Shift,
    /// which keeps the mouse tutti's, adds nothing.
    #[test]
    fn a_forwarded_wheel_carries_alt_and_ctrl() {
        let mut app = App::new();
        let rect = wheel_fixture(&mut app, b"\x1b[?1000h\x1b[?1006h");
        let mut wheel = |kind, modifiers| {
            let out = app.on_mouse_event(mouse(kind, rect.x + 5, rect.y + 3, modifiers));
            let [WireFrame::Input { bytes, .. }] = out.as_slice() else {
                panic!("{out:?}")
            };
            String::from_utf8(bytes.clone()).unwrap()
        };
        let (up, down) = (MouseEventKind::ScrollUp, MouseEventKind::ScrollDown);
        assert_eq!(wheel(up, KeyModifiers::CONTROL), "\x1b[<80;5;2M");
        assert_eq!(wheel(down, KeyModifiers::ALT), "\x1b[<73;5;2M");
        let both = KeyModifiers::ALT | KeyModifiers::CONTROL;
        assert_eq!(wheel(up, both), "\x1b[<88;5;2M");
        assert_eq!(wheel(up, KeyModifiers::SHIFT), "\x1b[<64;5;2M");
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

    // ---- mouse selection ------------------------------------------------

    /// Seed pane 1 with `text` and return its rect; the pane interior's top-left
    /// cell (0,0) then sits at absolute `(rect.x + 1, rect.y + 2)`, so a cell
    /// `(r, c)` is driven at `(rect.x + 1 + c, rect.y + 2 + r)`.
    fn selection_fixture(app: &mut App, text: &[u8]) -> Rect {
        attached(app);
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: text.to_vec(),
        }));
        app.rects
            .iter()
            .find(|(p, _)| *p == PaneId(1))
            .map(|(_, r)| *r)
            .expect("pane 1 has a rect")
    }

    /// The absolute terminal point of pane cell `(r, c)` inside `rect`.
    fn cell_at(rect: Rect, r: u16, c: u16) -> (u16, u16) {
        (rect.x + 1 + c, rect.y + 2 + r)
    }

    #[test]
    fn dragging_across_one_row_copies_that_span() {
        let mut app = App::new();
        let rect = selection_fixture(&mut app, b"hello world");
        let (x0, y0) = cell_at(rect, 0, 0);
        let (x1, y1) = cell_at(rect, 0, 4);
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), x0, y0);
        app.on_mouse(MouseEventKind::Drag(MouseButton::Left), x1, y1);
        app.on_mouse(MouseEventKind::Up(MouseButton::Left), x1, y1);
        assert_eq!(app.take_terminal_out(), vec![osc52("hello")]);
        assert_eq!(app.transient(), Some("copied 5 chars"));
    }

    #[test]
    fn dragging_across_rows_streams_first_partial_whole_and_last_partial() {
        let mut app = App::new();
        let rect = selection_fixture(&mut app, b"hello world\r\nsecond line\r\nthird");
        // From "world" on row 0, through all of row 1, to "third" on row 2.
        let (x0, y0) = cell_at(rect, 0, 6);
        let (x1, y1) = cell_at(rect, 2, 4);
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), x0, y0);
        app.on_mouse(MouseEventKind::Drag(MouseButton::Left), x1, y1);
        app.on_mouse(MouseEventKind::Up(MouseButton::Left), x1, y1);
        assert_eq!(
            app.take_terminal_out(),
            vec![osc52("world\nsecond line\nthird")],
            "trailing blanks trimmed; whole middle row taken in full"
        );
    }

    #[test]
    fn a_plain_click_selects_and_copies_nothing() {
        let mut app = App::new();
        let rect = selection_fixture(&mut app, b"hello world");
        let (x, y) = cell_at(rect, 0, 2);
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        app.on_mouse(MouseEventKind::Up(MouseButton::Left), x, y);
        assert!(
            app.take_terminal_out().is_empty(),
            "a focus click puts nothing on the clipboard"
        );
        assert!(app.selection_span(PaneId(1)).is_none());
    }

    #[test]
    fn clicking_a_pane_hands_the_keyboard_back_from_the_panels() {
        for panel in [Mode::Sidebar, Mode::DiffFiles] {
            let mut app = App::new();
            let rect = selection_fixture(&mut app, b"hello");
            app.mode = panel;
            let (x, y) = cell_at(rect, 0, 0);
            app.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
            assert_eq!(
                app.mode,
                Mode::Terminal,
                "keys must follow the focus the click showed"
            );
        }
    }

    #[test]
    fn clicking_another_pane_ends_the_scrollback_browse() {
        let mut app = App::new();
        attach_with(&mut app, view_two_panes());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        app.mode = Mode::Scroll(PaneId(1));
        app.set_scroll(PaneId(1), Some(3));
        let rect = app
            .rects
            .iter()
            .find(|(p, _)| *p == PaneId(2))
            .map(|(_, r)| *r)
            .expect("pane 2 has a rect");
        let (x, y) = cell_at(rect, 0, 0);
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        assert_eq!(
            out,
            vec![control(&Request::PaneScroll {
                pane: PaneId(1),
                offset: 0,
            })]
        );
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.focused, Some(PaneId(2)));
    }

    #[test]
    fn selection_span_tracks_the_drag_and_a_press_clears_it() {
        let mut app = App::new();
        let rect = selection_fixture(&mut app, b"hello world");
        let (x0, y0) = cell_at(rect, 0, 0);
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), x0, y0);
        assert!(
            app.selection_span(PaneId(1)).is_none(),
            "a press alone has not dragged yet"
        );
        let (x1, y1) = cell_at(rect, 0, 4);
        app.on_mouse(MouseEventKind::Drag(MouseButton::Left), x1, y1);
        assert_eq!(app.selection_span(PaneId(1)), Some(((0, 0), (0, 4))));
        // A fresh press drops the highlight.
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), x0, y0);
        assert!(app.selection_span(PaneId(1)).is_none());
    }

    #[test]
    fn dragging_past_the_right_edge_clamps_to_the_last_column() {
        let mut app = App::new();
        let rect = selection_fixture(&mut app, b"hello world");
        let (x0, y0) = cell_at(rect, 0, 0);
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), x0, y0);
        // Way past the pane's right edge, still on the first row.
        app.on_mouse(MouseEventKind::Drag(MouseButton::Left), 9999, y0);
        app.on_mouse(MouseEventKind::Up(MouseButton::Left), 9999, y0);
        assert_eq!(
            app.take_terminal_out(),
            vec![osc52("hello world")],
            "the drag clamps to the pane and takes the whole line"
        );
    }

    // ---- split-border drag ----------------------------------------------

    /// Shells 1 and 2 split along `direction` as the one tab, synced to a
    /// 100x24 terminal with the sidebar off, so the pane area is the full
    /// width under the two header rows: (0, 2, 100, 21).
    fn border_fixture(direction: Direction) -> App {
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        attach_with(
            &mut app,
            vec![workspace(
                1,
                "w",
                None,
                vec![tab(
                    1,
                    "1",
                    true,
                    split(direction, leaf(1), leaf(2)),
                    vec![shell(1), shell(2)],
                )],
            )],
        );
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        app
    }

    fn split_ratio(ratio: f32) -> WireFrame {
        control(&Request::PaneSplitRatio {
            a: PaneId(1),
            b: PaneId(2),
            ratio,
        })
    }

    #[test]
    fn dragging_a_side_by_side_border_sends_split_ratios_not_a_selection() {
        use MouseEventKind::{Down, Drag, Up};
        let mut app = border_fixture(Direction::Horizontal);
        // Panes 1 | 2 meet at column 50: pane 1's right frame is column 49,
        // pane 2's left frame column 50. Grab pane 2's side.
        assert!(app.on_mouse(Down(MouseButton::Left), 50, 10).is_empty());
        assert_eq!(
            app.on_mouse(Drag(MouseButton::Left), 69, 10),
            vec![split_ratio(0.69)]
        );
        assert!(
            app.on_mouse(Drag(MouseButton::Left), 69, 15).is_empty(),
            "moving along the border moves it no cell"
        );
        assert_eq!(
            app.on_mouse(Drag(MouseButton::Left), 70, 15),
            vec![split_ratio(0.7)]
        );
        assert!(app.on_mouse(Up(MouseButton::Left), 70, 15).is_empty());
        assert_eq!(
            app.focused,
            Some(PaneId(1)),
            "a border drag leaves focus where it was"
        );
        assert!(app.selection_span(PaneId(1)).is_none());
        assert!(app.selection_span(PaneId(2)).is_none());
        assert!(app.take_terminal_out().is_empty(), "and copies nothing");
        // Grabbed on pane 1's side, that column stays under the pointer:
        // dragged to 59, pane 1 ends there and pane 2 starts at 60.
        app.on_mouse(Down(MouseButton::Left), 49, 3);
        assert_eq!(
            app.on_mouse(Drag(MouseButton::Left), 59, 3),
            vec![split_ratio(0.6)]
        );
    }

    #[test]
    fn dragging_a_stacked_border_sends_split_ratios_not_a_selection() {
        use MouseEventKind::{Down, Drag, Up};
        let mut app = border_fixture(Direction::Vertical);
        // The 21 content rows split 11 / 10: pane 1 holds rows 2-12, its bottom
        // frame on 12; pane 2 rows 13-22, its title on 13.
        assert!(app.on_mouse(Down(MouseButton::Left), 30, 13).is_empty());
        assert_eq!(
            app.on_mouse(Drag(MouseButton::Left), 30, 8),
            vec![split_ratio(6.0 / 21.0)]
        );
        assert!(app.on_mouse(Up(MouseButton::Left), 30, 8).is_empty());
        // Grabbed on pane 1's bottom frame, the border lands one row below the
        // pointer.
        app.on_mouse(Down(MouseButton::Left), 30, 12);
        assert_eq!(
            app.on_mouse(Drag(MouseButton::Left), 30, 16),
            vec![split_ratio(15.0 / 21.0)]
        );
        assert_eq!(app.focused, Some(PaneId(1)));
        assert!(app.selection_span(PaneId(2)).is_none());
    }

    #[test]
    fn a_border_dragged_past_the_clamp_sends_once() {
        use MouseEventKind::{Down, Drag};
        let mut app = border_fixture(Direction::Horizontal);
        app.on_mouse(Down(MouseButton::Left), 50, 10);
        assert_eq!(
            app.on_mouse(Drag(MouseButton::Left), 99, 10),
            vec![split_ratio(Layout::MAX_RATIO)]
        );
        assert!(
            app.on_mouse(Drag(MouseButton::Left), 98, 10).is_empty(),
            "held past the bound, the border is already where it can go"
        );
    }

    #[test]
    fn a_still_click_on_a_border_focuses_like_any_click() {
        use MouseEventKind::{Down, Up};
        let mut app = border_fixture(Direction::Vertical);
        // Pane 2's title row is its side of the border: the press grabs it, and
        // a release that never moved it focuses the pane.
        app.on_mouse(Down(MouseButton::Left), 30, 13);
        assert_eq!(app.focused, Some(PaneId(1)), "the press alone only grabs");
        app.on_mouse(Up(MouseButton::Left), 30, 13);
        assert_eq!(app.focused, Some(PaneId(2)));
    }

    #[test]
    fn a_drag_inside_a_split_pane_still_selects_and_copies() {
        use MouseEventKind::{Down, Drag, Up};
        let mut app = border_fixture(Direction::Horizontal);
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 18,
            cols: 48,
            seq: 0,
            bytes: b"hello world".to_vec(),
        }));
        // Pane 1's interior starts at (1, 4); the drag runs on past the border
        // at column 50 and clamps to the pane, as it always has.
        app.on_mouse(Down(MouseButton::Left), 1, 4);
        assert!(app.on_mouse(Drag(MouseButton::Left), 60, 4).is_empty());
        app.on_mouse(Up(MouseButton::Left), 60, 4);
        assert_eq!(app.take_terminal_out(), vec![osc52("hello world")]);
        assert_eq!(app.transient(), Some("copied 11 chars"));
    }

    // ---- the mouse to programs ----------------------------------------------

    fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers,
        }
    }

    /// Side-by-side panes 1 | 2 (`border_fixture`), pane 2's program having
    /// written `bytes`: pane 2's interior cell (r, c) sits at (51 + c, 4 + r).
    fn reporting_fixture(bytes: &[u8]) -> App {
        let mut app = border_fixture(Direction::Horizontal);
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(2),
            rows: 18,
            cols: 48,
            seq: 0,
            bytes: bytes.to_vec(),
        }));
        app
    }

    fn to_two(bytes: &[u8]) -> Vec<WireFrame> {
        vec![WireFrame::Input {
            pane: PaneId(2),
            bytes: bytes.to_vec(),
        }]
    }

    /// A program with button-motion reporting (1002, SGR) gets a press —
    /// which focuses its pane too — the drag and the release, at its own
    /// 1-based cells, clamped to the pane once the pointer leaves it; tutti
    /// selects and copies nothing.
    #[test]
    fn a_press_drag_and_release_go_to_a_program_reporting_them() {
        use MouseEventKind::{Down, Drag, Up};
        let none = KeyModifiers::NONE;
        let mut app = reporting_fixture(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(app.focused, Some(PaneId(1)));
        let out = app.on_mouse_event(mouse(Down(MouseButton::Left), 55, 6, none));
        assert_eq!(out, to_two(b"\x1b[<0;5;3M"));
        assert_eq!(app.focused, Some(PaneId(2)), "the press focuses its pane");
        let out = app.on_mouse_event(mouse(Drag(MouseButton::Left), 70, 7, none));
        assert_eq!(out, to_two(b"\x1b[<32;20;4M"));
        let out = app.on_mouse_event(mouse(Drag(MouseButton::Left), 99, 30, none));
        assert_eq!(out, to_two(b"\x1b[<32;48;18M"), "clamped to the pane");
        let out = app.on_mouse_event(mouse(Up(MouseButton::Left), 99, 30, none));
        assert_eq!(out, to_two(b"\x1b[<0;48;18m"));
        assert!(app.selection.is_none() && app.take_terminal_out().is_empty());
        // Ctrl and Alt ride the button: a ctrl right-click.
        let out = app.on_mouse_event(mouse(
            Down(MouseButton::Right),
            51,
            4,
            KeyModifiers::CONTROL,
        ));
        assert_eq!(out, to_two(b"\x1b[<18;1;1M"));
    }

    /// What a program did not ask for it does not get — and a press it took
    /// keeps its drag and release from tutti: presses and releases (1000)
    /// get no drags; presses alone (9) no release either.
    #[test]
    fn a_program_gets_only_the_reports_it_asked_for() {
        use MouseEventKind::{Down, Drag, Moved, Up};
        let none = KeyModifiers::NONE;
        let mut app = reporting_fixture(b"\x1b[?1000h\x1b[?1006h");
        app.on_mouse_event(mouse(Down(MouseButton::Left), 55, 6, none));
        assert!(
            app.on_mouse_event(mouse(Drag(MouseButton::Left), 70, 7, none))
                .is_empty()
        );
        let out = app.on_mouse_event(mouse(Up(MouseButton::Left), 70, 7, none));
        assert_eq!(out, to_two(b"\x1b[<0;20;4m"));
        assert!(app.on_mouse_event(mouse(Moved, 60, 8, none)).is_empty());
        assert!(app.selection.is_none() && app.take_terminal_out().is_empty());

        let mut app = reporting_fixture(b"\x1b[?9h\x1b[?1006h");
        let out = app.on_mouse_event(mouse(Down(MouseButton::Left), 55, 6, none));
        assert_eq!(out, to_two(b"\x1b[<0;5;3M"));
        assert!(
            app.on_mouse_event(mouse(Up(MouseButton::Left), 55, 6, none))
                .is_empty()
        );
        assert!(
            app.mouse_grab.is_none(),
            "the release ended the press all the same"
        );

        // Every motion (1003) goes, a button held or not.
        let mut app = reporting_fixture(b"\x1b[?1003h\x1b[?1006h");
        app.on_mouse_event(mouse(Down(MouseButton::Left), 51, 4, none));
        app.on_mouse_event(mouse(Up(MouseButton::Left), 51, 4, none));
        let out = app.on_mouse_event(mouse(Moved, 60, 8, none));
        assert_eq!(out, to_two(b"\x1b[<35;10;5M"));
    }

    /// Without SGR the report is the legacy one: values offset by 32 into a
    /// byte each, a release as button 3.
    #[test]
    fn a_legacy_reporting_program_gets_legacy_reports() {
        use MouseEventKind::{Down, Up};
        let none = KeyModifiers::NONE;
        let mut app = reporting_fixture(b"\x1b[?1000h");
        let out = app.on_mouse_event(mouse(Down(MouseButton::Left), 55, 6, none));
        assert_eq!(out, to_two(&[0x1b, b'[', b'M', 32, 32 + 5, 32 + 3]));
        let out = app.on_mouse_event(mouse(Up(MouseButton::Left), 55, 6, none));
        assert_eq!(out, to_two(&[0x1b, b'[', b'M', 32 + 3, 32 + 5, 32 + 3]));
    }

    /// Shift keeps the mouse tutti's over a reporting program — select and
    /// copy — and a pane whose program reports nothing selects as ever; a
    /// border stays a border either way.
    #[test]
    fn shift_or_no_reporting_keeps_the_mouse_tuttis() {
        use MouseEventKind::{Down, Drag, Up};
        let shift = KeyModifiers::SHIFT;
        let mut app = reporting_fixture(b"\x1b[?1002h\x1b[?1006hhello world");
        for kind in [
            Down(MouseButton::Left),
            Drag(MouseButton::Left),
            Up(MouseButton::Left),
        ] {
            let column = if matches!(kind, Down(_)) { 51 } else { 61 };
            let out = app.on_mouse_event(mouse(kind, column, 4, shift));
            assert!(out.iter().all(|f| !matches!(f, WireFrame::Input { .. })));
        }
        assert_eq!(app.take_terminal_out(), vec![osc52("hello world")]);

        let none = KeyModifiers::NONE;
        let mut app = reporting_fixture(b"hello world");
        for kind in [
            Down(MouseButton::Left),
            Drag(MouseButton::Left),
            Up(MouseButton::Left),
        ] {
            let column = if matches!(kind, Down(_)) { 51 } else { 61 };
            let out = app.on_mouse_event(mouse(kind, column, 4, none));
            assert!(out.iter().all(|f| !matches!(f, WireFrame::Input { .. })));
        }
        assert_eq!(app.take_terminal_out(), vec![osc52("hello world")]);

        let mut app = reporting_fixture(b"\x1b[?1002h\x1b[?1006h");
        assert!(
            app.on_mouse_event(mouse(Down(MouseButton::Left), 50, 10, none))
                .is_empty()
        );
        assert_eq!(
            app.on_mouse_event(mouse(Drag(MouseButton::Left), 69, 10, none)),
            vec![split_ratio(0.69)],
            "pane 2's frame column is still the border's"
        );
    }

    #[test]
    fn zoom_leaves_no_border_to_grab() {
        use MouseEventKind::{Down, Drag};
        let mut app = border_fixture(Direction::Horizontal);
        app.on_key(ctrl('b'));
        app.on_key(plain('z'));
        assert!(app.zoom);
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        assert!(app.boundaries.is_empty());
        // Column 50 is now deep inside the zoomed pane: the press selects.
        app.on_mouse(Down(MouseButton::Left), 50, 10);
        assert!(
            app.on_mouse(Drag(MouseButton::Left), 70, 10).is_empty(),
            "no split ratio while zoomed"
        );
        assert!(app.selection_span(PaneId(1)).is_some());
    }

    #[test]
    fn ordered_sorts_cells_in_reading_order() {
        assert_eq!(ordered((2, 3), (1, 5)), ((1, 5), (2, 3)));
        assert_eq!(ordered((1, 1), (1, 4)), ((1, 1), (1, 4)));
    }

    #[test]
    fn base64_matches_the_rfc_test_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_wraps_base64_in_a_clipboard_write() {
        assert_eq!(osc52("hi"), b"\x1b]52;c;aGk=\x07".to_vec());
    }

    // ---- exited panes ---------------------------------------------------

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
    fn p_on_an_exited_sandbox_pane_pulls_its_work_and_jumps_to_it() {
        let mut app = App::new();
        let mut view = view_one_exited_agent();
        view[0].tabs[0].panes[0].sandbox = Some("gke".into());
        view[0].tabs[0].panes[0].sandbox_name = Some("tutti-p1-ab".into());
        attach_with(&mut app, view.clone());
        let out = app.on_key(plain('p'));
        let pull = Request::SandboxPull {
            pane: PaneId(1),
            name: None,
        };
        assert_eq!(out, vec![tagged(&pull, 1)]);
        assert_eq!(app.transient(), Some("pulling tutti/tutti-p1-ab…"));

        // The view carrying the pulled workspace lands before the reply.
        view.push(workspace(
            2,
            "w-tutti-p1-ab",
            None,
            vec![tab(2, "2", true, leaf(5), vec![shell(5)])],
        ));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        app.handle_response(Some(1), Response::WorkspaceCreated { id: WorkspaceId(2) });
        assert_eq!(
            app.active_tab,
            Some(TabId(2)),
            "the reply jumps to the pulled workspace, as guided create's does"
        );

        let mut local = App::new();
        attach_with(&mut local, view_one_exited_agent());
        assert_eq!(
            local.on_key(plain('p')),
            vec![WireFrame::Input {
                pane: PaneId(1),
                bytes: b"p".to_vec(),
            }],
            "a local corpse has no pushed work to pull"
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
}
