//! The attach client's state machine: everything the TUI knows and every
//! decision it makes, driven purely by inbound wire frames and user input. It
//! owns no socket and no terminal, so it can be exercised headlessly in tests.
//! `App` and `Mode` are here, with the frames in, the key dispatch and the
//! layout. What one mode or panel keeps is a plain struct in the sibling
//! module of its name, beside the `impl App` that works on it and its tests.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::text::Span;
use rtty_vt::Terminal;
use rtty_vt::input;
use tutti_core::{
    AgentKind, AgentState, Direction, Event, Frame as WireFrame, Layout, NOTES_CAP, Note, PaneDims,
    PaneId, PaneInfo, Request, Response, RestoreSummary, TabId, TabView, Tagged, Toward,
    WorkspaceId, WorkspaceView,
};

use super::approval::Approvals;
use super::changes::{Changes, DIFF_PANEL_WIDTH};
use super::deploy::Issues;
use super::follow::{FollowUp, FollowUps};
use super::keys;
use super::launcher::Launcher;
use super::layout::{Boundary, boundaries, pane_rects};
use super::mouse::Pointer;
use super::navigate::Navigate;
use super::notes::{NotesPanel, notified_panes};
use super::peek::Peek;
use super::prompt::Prompt;
use super::reader::{Decoder, Input, Report};
use super::rtty::{Handoff, Rtty};
use super::setup::Setup;
use super::sidebar::{self, SIDEBAR_WIDTH, SidebarState};
use crate::config::{self, Action, Config, PrefixAction, RESIZE_DELTA};
use crate::host::{Host, Info, Local};
use crate::issues;

/// Rows the top chrome header claims: the full-width app bar plus the dim rule
/// beneath it. The content region begins below them.
const HEADER_ROWS: u16 = 2;

const STATUS_TTL: Duration = Duration::from_secs(4);
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
    pub(super) boundaries: Vec<Boundary>,
    pub(super) pointer: Pointer,
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
    pub(super) sidebar: SidebarState,
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
    pub(super) changes: Changes,
    /// The top tab-bar row from the last size sync, for mouse hit-testing.
    pub(super) tab_bar_rect: Option<Rect>,
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
    pub(super) notes_chip_rect: Option<Rect>,
    /// Escape sequences queued for the real terminal (bell + OSC 9 re-emit), so
    /// the user's own terminal raises a desktop notification. Drained by the
    /// event loop.
    pub(super) terminal_out: Vec<Vec<u8>>,
    /// Whether the terminal should be capturing mouse events. Starts at the
    /// config's master switch; the mouse-toggle prefix key flips it so the
    /// terminal's own drag-to-select and copy work while released. The event
    /// loop syncs the real capture state to this.
    mouse_capture: bool,
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
            pointer: Pointer::default(),
            pressed: HashMap::new(),
            status: None,
            view_sent: Vec::new(),
            focus_sent: None,
            host_focused: true,
            focus_reported: None,
            rtty: None,
            bell: false,
            sidebar: SidebarState::default(),
            prompt: Prompt::default(),
            launcher: Launcher::default(),
            host,
            home: info.home,
            shell: info.shell,
            setup: Setup::default(),
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
            mouse_capture: config.mouse,
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
        self.reanchor_sidebar();
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

    /// The tab whose layout holds `pane`, if the current view carries it.
    pub(super) fn tab_of_pane(&self, pane: PaneId) -> Option<TabId> {
        self.workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .find(|t| t.layout.as_ref().is_some_and(|l| l.panes().contains(&pane)))
            .map(|t| t.id)
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
    pub(super) fn tab_bar_click(&mut self, col: u16) -> Vec<WireFrame> {
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
        self.sidebar.rect = sidebar_rect;
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

    pub(super) fn exit_scroll(&mut self, pane: PaneId) -> Vec<WireFrame> {
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
            FocusDir::Left if self.sidebar.rect.is_some() => {
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

    pub(super) fn set_scroll(&mut self, pane: PaneId, offset: Option<usize>) {
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

    pub(super) fn pane_at(&self, col: u16, row: u16) -> Option<PaneId> {
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

/// Inner (borderless) size of a pane rect as `(rows, cols)`, clamped to at
/// least 1 so a pty is never asked for a zero dimension. A pane rect reserves
/// its top row for the title line drawn above the frame, so height loses that
/// row plus the two border rows.
pub(super) fn inner_size(rect: Rect) -> (u16, u16) {
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
    use crate::attach::sidebar::SidebarEntry;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use tutti_core::PaneData;

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
        assert_eq!(app.prompt.input, "/tmp/proj");

        let mut app = offered();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.on_key(enter), restore(false));
        assert_eq!(app.mode, Mode::Terminal, "the restored panes take the keys");
        assert!(app.prompt.input.is_empty(), "the prompt is cancelled");

        let mut app = offered();
        assert_eq!(app.on_key(plain('d')), restore(true));
        assert_eq!(app.mode, Mode::Sidebar, "as the prompt's own esc would");
        assert!(app.prompt.input.is_empty(), "the prompt is cancelled");
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
