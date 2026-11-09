//! The sidebar's peek at an agent row, floating over the screen while focus,
//! tab and layout stay put: a blocked agent's live prompt, answered from here
//! as guarded sends (`Mode::Peek`), or a done agent's last reply
//! (`Mode::Reply`) — and the frame the approval overlay shares with them.

use std::ops::Range;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use rtty_vt::Terminal;
use tutti_core::{
    AgentState, Frame as WireFrame, PaneId, Request, prompt_fingerprint, prompt_rows,
};

use super::app::{App, FollowUp, Mode, control};
use super::keys;
use super::sidebar::{self, Sidebar, SidebarEntry};

/// The peek overlay's narrowest width, so its hint row (answer keys, esc, how
/// to jump in: 62 columns padded) fits between the borders over a narrow pane.
const PEEK_MIN_WIDTH: u16 = 64;

/// What the peek, approval and reply overlays keep, which means something
/// only while one of them is open.
#[derive(Default)]
pub(super) struct Peek {
    /// The peek, approval or reply overlay from the last size sync, for mouse
    /// hit-testing. `None` while none is open.
    pub(super) rect: Option<Rect>,
    /// The text the reply overlay shows: the last reply it was handed.
    pub(super) reply: String,
}

impl App {
    /// Peek at the selected agent row when it is blocked: open the overlay over
    /// its live prompt. Nothing else moves — no focus, tab, or layout change —
    /// so the pane underneath and the prompt being fingerprinted stay put. A
    /// row whose pane holds an approval opens the approval overlay instead:
    /// Claude's dialog is held back, so there is no prompt on screen to peek.
    /// A done row asks for its last reply, which opens the reply overlay.
    pub(super) fn open_peek(&mut self, sidebar: &Sidebar) -> Vec<WireFrame> {
        match sidebar.entries.get(self.sidebar_selected) {
            Some(SidebarEntry::Agent(sidebar::AgentRow {
                pane,
                approval: Some(approval),
                ..
            })) => self.open_approval(*pane, approval.id, Mode::Sidebar),
            Some(SidebarEntry::Agent(a)) if a.state == AgentState::Blocked => {
                self.mode = Mode::Peek(a.pane);
                self.status = None;
            }
            Some(SidebarEntry::Agent(a))
                if matches!(a.state, AgentState::Done | AgentState::Idle) =>
            {
                let read = Request::PaneRead {
                    pane: a.pane,
                    lines: None,
                    unwrapped: false,
                    since: None,
                    last_output: false,
                    last_reply: true,
                };
                return vec![self.follow(&read, FollowUp::Reply(a.pane))];
            }
            _ => self.set_status("peek answers a blocked agent or reads a done one's reply".into()),
        }
        Vec::new()
    }

    /// Keys over the reply overlay: `esc` closes it back to the sidebar;
    /// nothing else does anything.
    pub(super) fn on_key_reply(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Sidebar;
        }
        Vec::new()
    }

    /// Keys while peeking. `esc` closes back to the sidebar — never forwarded,
    /// esc is the back key throughout. An answer key (an unmodified character,
    /// enter, an arrow, tab, backspace) goes to the peeked pane as a guarded
    /// send, fingerprinted from the mirror the overlay is drawing: the event
    /// loop draws every applied frame before it reads keys, so the server
    /// refuses the key if the prompt moved past what was on screen. Anything
    /// else is dropped — esc and free-form typing belong in the pane itself.
    pub(super) fn on_key_peek(&mut self, key: KeyEvent, pane: PaneId) -> Vec<WireFrame> {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Sidebar;
            self.status = None;
            return Vec::new();
        }
        let answer = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(
                key.code,
                KeyCode::Char(_)
                    | KeyCode::Enter
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Backspace
            );
        // Answer keys are text the guarded send types: plain, as a prompt
        // takes them whatever keyboard protocol its program asked for.
        let text = answer
            .then(|| keys::encode_key(key, None, None))
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        let (Some(text), Some(state)) = (text, self.panes.get(&pane)) else {
            return Vec::new();
        };
        vec![control(&Request::PaneSend {
            pane,
            text: Some(text),
            keys: None,
            expect: Some(prompt_fingerprint(&screen_rows(&state.term))),
            between_turns: false,
            hooks: None,
        })]
    }

    /// The peek overlay while peeking: the peeked pane, the popup rect centred
    /// in the terminal `area`, and the mirror rows it shows — the prompt region
    /// the fingerprint covers, clipped from the top only when the terminal is
    /// too short to hold it. The one geometry source the renderer and the mouse
    /// hit-test share; the overlay floats, so the pane layout never moves.
    pub fn peek_popup(&self, area: Rect) -> Option<(PaneId, Rect, Range<usize>)> {
        let Mode::Peek(pane) = self.mode else {
            return None;
        };
        let term = &self.panes.get(&pane)?.term;
        let region = prompt_rows(&screen_rows(term));
        // Two border rows and the hint row around the region.
        let height = (region.len().max(1) as u16 + 3).min(area.height);
        let width = (term.size().1 + 2).max(PEEK_MIN_WIDTH).min(area.width);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let shown = region.len().min(height.saturating_sub(3) as usize);
        Some((pane, popup, region.end - shown..region.end))
    }

    /// The approval or reply overlay while one is open: its pane, the popup
    /// rect — sized and centred as the peek's — and the body rows,
    /// hard-wrapped to the popup's inner width, a column of padding each side:
    /// an approval's tool, then its input; a reply's lines, blank ones kept.
    /// A terminal too short for every row cuts the last one shown with `…`.
    /// The one geometry source the renderer and the mouse hit-test share.
    pub fn text_popup(&self, area: Rect) -> Option<(PaneId, Rect, Vec<String>)> {
        let pane = match self.mode {
            Mode::Approval(pane) | Mode::Reply(pane) => pane,
            _ => return None,
        };
        let state = self.panes.get(&pane)?;
        let width = (state.term.size().1 + 2)
            .max(PEEK_MIN_WIDTH)
            .min(area.width);
        let wrap = |text: &str| -> Vec<String> {
            let chars: Vec<char> = text.chars().collect();
            let inner = usize::from(width.saturating_sub(4)).max(1);
            chars
                .chunks(inner)
                .map(|chunk| chunk.iter().collect())
                .collect()
        };
        let mut rows = Vec::new();
        if let Mode::Approval(_) = self.mode {
            let approval = self.shown_approval(pane)?;
            rows.push(approval.tool.clone());
            rows.extend(wrap(&approval.summary));
        } else {
            for line in self.peek.reply.lines() {
                let wrapped = wrap(line);
                if wrapped.is_empty() {
                    rows.push(String::new());
                }
                rows.extend(wrapped);
            }
        }
        // Two border rows and the hint row around the body.
        let height = (rows.len() as u16 + 3).min(area.height);
        let shown = usize::from(height.saturating_sub(3));
        if rows.len() > shown {
            rows.truncate(shown);
            if let Some(last) = rows.last_mut() {
                last.pop();
                last.push('…');
            }
        }
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        Some((pane, popup, rows))
    }
}

/// A screen's visible rows as text, top to bottom — what the prompt fingerprint
/// and its region are computed over, exactly as the server computes them.
fn screen_rows(term: &Terminal) -> Vec<String> {
    (0..term.size().0)
        .map(|r| term.screen_row(r).text())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::app::tagged;
    use crate::attach::fixtures::*;
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    use tutti_core::{Event, PaneData, Response};

    /// `view_two_workspaces` with blocked pane 3's mirror holding a permission
    /// dialog, the sidebar focused, and the cursor on pane 3's waiting row.
    fn peek_fixture() -> App {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(3),
            rows: 8,
            cols: 40,
            seq: 0,
            bytes: "Bash command\r\n  rm -rf build\r\n\r\nDo you want to proceed?\r\n❯ 1. Yes\r\n  2. No"
                .as_bytes()
                .to_vec(),
        }));
        focus_sidebar(&mut app);
        let waiting = app
            .sidebar()
            .index_of(sidebar::EntryIdent::Waiting(PaneId(3), None));
        for _ in 0..waiting.expect("pane 3 waits in the queue") {
            app.on_key(plain('j'));
        }
        app
    }

    /// The guarded send the peek should emit for `text`, fingerprinted from
    /// pane 3's mirror as it stands.
    fn guarded_answer(app: &App, text: &str) -> WireFrame {
        let rows = screen_rows(&app.panes[&PaneId(3)].term);
        control(&Request::PaneSend {
            pane: PaneId(3),
            text: Some(text.into()),
            keys: None,
            expect: Some(prompt_fingerprint(&rows)),
            between_turns: false,
            hooks: None,
        })
    }

    #[test]
    fn p_on_a_blocked_row_peeks_and_answer_keys_go_out_guarded() {
        let mut app = peek_fixture();
        app.focus_change(); // drain the attach-time focus report
        let (focused, tab, selected) = (app.focused, app.active_tab, app.sidebar_selected());
        assert_eq!(app.on_key(plain('p')), Vec::new(), "opening sends nothing");
        assert_eq!(app.mode, Mode::Peek(PaneId(3)));

        assert_eq!(app.on_key(plain('1')), vec![guarded_answer(&app, "1")]);
        assert_eq!(app.on_key(plain('y')), vec![guarded_answer(&app, "y")]);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.on_key(enter), vec![guarded_answer(&app, "\r")]);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.on_key(down), vec![guarded_answer(&app, "\x1b[B")]);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.on_key(backspace), vec![guarded_answer(&app, "\x7f")]);
        // Chords are not answers; they go nowhere rather than to the agent.
        assert_eq!(app.on_key(ctrl('c')), Vec::new());
        assert_eq!(app.on_key(alt('y')), Vec::new());
        assert_eq!(app.mode, Mode::Peek(PaneId(3)), "answering keeps it open");

        // esc is never forwarded: it closes, landing exactly where we were.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.on_key(esc), Vec::new());
        assert_eq!(app.mode, Mode::Sidebar);
        assert_eq!(
            (app.focused, app.active_tab, app.sidebar_selected()),
            (focused, tab, selected),
            "peeking never moves focus, the tab, or the cursor"
        );
        assert_eq!(
            app.focus_change(),
            None,
            "the server hears of no focus move"
        );
    }

    #[test]
    fn p_on_a_row_that_is_not_blocked_only_flashes() {
        let mut app = peek_fixture();
        app.on_key(plain('k')); // web's last tree agent: pane 3, blocked too
        app.on_key(plain('k')); // pane 2, working
        assert_eq!(app.on_key(plain('p')), Vec::new());
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(app.transient().is_some_and(|t| t.contains("blocked")));
    }

    /// `p` on a done agent's row asks for its last reply, and the answer
    /// opens the reply overlay over the sidebar — its lines wrapped, blank
    /// ones kept — which `esc` closes. A refusal only flashes, and a reply
    /// landing after the sidebar moved on opens nothing.
    #[test]
    fn p_on_a_done_row_reads_its_last_reply() {
        let mut app = peek_fixture();
        state_changed(&mut app, PaneId(2), AgentState::Working, AgentState::Done);
        let row = app
            .sidebar()
            .index_of(sidebar::EntryIdent::Agent(PaneId(2)));
        app.sidebar_selected = row.expect("pane 2's row");
        let read = Request::PaneRead {
            pane: PaneId(2),
            lines: None,
            unwrapped: false,
            since: None,
            last_output: false,
            last_reply: true,
        };
        let asked = app.on_key(plain('p'));
        assert_eq!(asked, vec![tagged(&read, app.last_req)]);
        assert_eq!(app.mode, Mode::Sidebar, "nothing opens before the answer");

        let said = Response::LastReply {
            text: format!("pong\n\n{}", "x".repeat(150)),
            truncated: false,
        };
        app.handle_frame(reply(app.last_req, said.clone()));
        assert_eq!(app.mode, Mode::Reply(PaneId(2)));
        let (pane, _, rows) = app.text_popup(Rect::new(0, 0, 100, 24)).unwrap();
        assert_eq!(pane, PaneId(2));
        assert_eq!(rows[..2], ["pong", ""], "a blank line is kept");
        assert!(rows.len() > 3, "a long line wraps: {rows:?}");
        assert_eq!(rows[2..].concat(), "x".repeat(150));
        assert_eq!(app.on_key(plain('q')), Vec::new());
        assert_eq!(app.mode, Mode::Reply(PaneId(2)), "it only reads");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Sidebar);

        app.on_key(plain('p'));
        let refused = Response::Error {
            message: "pane 2's last turn ended without a reply".into(),
        };
        app.handle_frame(reply(app.last_req, refused));
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("without a reply"))
        );

        app.on_key(plain('p'));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_frame(reply(app.last_req, said));
        assert_ne!(app.mode, Mode::Reply(PaneId(2)), "the sidebar was left");
    }

    #[test]
    fn the_peek_closes_itself_once_the_agent_leaves_blocked() {
        let mut app = peek_fixture();
        app.on_key(plain('p'));
        state_changed(&mut app, PaneId(2), AgentState::Working, AgentState::Done);
        assert_eq!(
            app.mode,
            Mode::Peek(PaneId(3)),
            "another pane's move is not ours"
        );
        state_changed(
            &mut app,
            PaneId(3),
            AgentState::Blocked,
            AgentState::Working,
        );
        assert_eq!(app.mode, Mode::Sidebar);

        // A peeked pane that vanishes closes it too.
        let mut app = peek_fixture();
        app.on_key(plain('p'));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged {
                workspaces: view_one_pane(),
            })
            .unwrap(),
        ));
        assert_eq!(app.mode, Mode::Sidebar);
    }

    #[test]
    fn a_refused_answer_flashes_the_reason_and_keeps_the_peek_open() {
        let mut app = peek_fixture();
        app.on_key(plain('p'));
        app.on_key(plain('1'));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Error {
                message: "the prompt changed since you looked: read pane 3 again".into(),
            })
            .unwrap(),
        ));
        assert_eq!(app.mode, Mode::Peek(PaneId(3)));
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("prompt changed"))
        );
    }

    #[test]
    fn the_mouse_peeks_with_a_right_click_and_the_overlay_is_modal() {
        let mut app = peek_fixture();
        app.mode = Mode::Terminal;
        let area = Rect::new(0, 0, 100, 24);
        app.sync_sizes(area);
        let sidebar = app.sidebar_rect.unwrap();
        let row = (sidebar.y..sidebar.y + sidebar.height)
            .find(|r| {
                app.sidebar().entry_at_row((r - sidebar.y) as usize)
                    == app
                        .sidebar()
                        .index_of(sidebar::EntryIdent::Waiting(PaneId(3), None))
            })
            .unwrap();
        app.on_mouse(MouseEventKind::Down(MouseButton::Right), sidebar.x + 2, row);
        assert_eq!(app.mode, Mode::Peek(PaneId(3)));
        app.sync_sizes(area);

        // A press inside the overlay is swallowed: the pane beneath keeps
        // its focus and nothing is sent.
        let (_, popup, _) = app.peek_popup(area).unwrap();
        let out = app.on_mouse(
            MouseEventKind::Down(MouseButton::Left),
            popup.x + popup.width / 2,
            popup.y + 1,
        );
        assert_eq!(out, Vec::new());
        assert_eq!(app.mode, Mode::Peek(PaneId(3)));
        assert_eq!(app.focused, Some(PaneId(1)));

        // A press outside dismisses it, and only that.
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 99, 23);
        assert_eq!(out, Vec::new());
        assert_eq!(app.mode, Mode::Sidebar);
        assert_eq!(app.focused, Some(PaneId(1)));
    }

    #[test]
    fn the_peek_popup_frames_exactly_the_fingerprinted_region() {
        let mut app = peek_fixture();
        app.on_key(plain('p'));
        let rows = screen_rows(&app.panes[&PaneId(3)].term);
        let region = prompt_rows(&rows);
        assert_eq!(region, 0..6, "the dialog, blank spacer row included");

        let (pane, popup, shown) = app.peek_popup(Rect::new(0, 0, 100, 24)).unwrap();
        assert_eq!(pane, PaneId(3));
        assert_eq!(shown, region);
        assert_eq!(popup.height, 6 + 3, "borders and the hint row around it");
        assert_eq!(
            popup.width, PEEK_MIN_WIDTH,
            "a narrow pane still fits the hint"
        );

        // Too short a terminal clips from the top, keeping the options in view.
        let (_, popup, shown) = app.peek_popup(Rect::new(0, 0, 100, 7)).unwrap();
        assert_eq!((popup.height, shown), (7, 2..6));
    }
}
