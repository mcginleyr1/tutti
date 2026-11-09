//! The approval overlay (`Mode::Approval`): a tool call an agent's pane
//! holds for tutti to allow, deny or pass on, opened from the pane's sidebar
//! row or by itself over the pane in view, once per approval.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tutti_core::{ApprovalDecision, Frame as WireFrame, PaneId, PendingApproval, Request};

use super::app::{App, Mode, control};

/// How long the approval overlay ignores answers after opening itself over
/// the pane in view: about a reaction time, so keys already on their way from
/// someone typing into that pane can never allow or deny on their behalf.
const APPROVAL_GRACE: Duration = Duration::from_millis(800);

/// What the approval overlay shows and has shown. Only `seen` means anything
/// outside `Mode::Approval`.
#[derive(Default)]
pub(super) struct Approvals {
    /// The approval the approval overlay last opened for — the one it shows
    /// and an answer names.
    shown: Option<u64>,
    /// Every approval the overlay has opened for while it is still held, so
    /// it opens by itself once per approval: never again on one answered or
    /// dismissed. Ids are never reused, so one no longer held is dropped.
    seen: HashSet<u64>,
    /// The mode the approval overlay hands back to on closing: the sidebar it
    /// was opened from, or the terminal of the pane it opened over.
    pub(super) back: Mode,
    /// When the approval overlay last opened itself over the pane in view —
    /// answers wait out `APPROVAL_GRACE` from then. `None` for an overlay
    /// opened on purpose (the sidebar's `p`), which answers at once.
    opened: Option<Instant>,
}

impl App {
    /// The approval the overlay shows, while `pane` still holds it.
    pub(super) fn shown_approval(&self, pane: PaneId) -> Option<&PendingApproval> {
        let held = &self.panes.get(&pane)?.info.approvals;
        held.iter().find(|a| Some(a.id) == self.approvals.shown)
    }

    /// Open the approval overlay on `pane`'s approval `id`, handing the keys
    /// back to `back` once it closes.
    pub(super) fn open_approval(&mut self, pane: PaneId, id: u64, back: Mode) {
        self.mode = Mode::Approval(pane);
        self.approvals.shown = Some(id);
        self.approvals.seen.insert(id);
        self.approvals.back = back;
        self.approvals.opened = None;
        self.status = None;
    }

    /// Open the approval overlay over the focused pane when it holds an
    /// approval the overlay has not opened for — the pane in view is asking,
    /// as Claude's own dialog would; the oldest such first — but only from
    /// the terminal: a panel, prompt or overlay in use is never cut into.
    pub(super) fn open_focused_approval(&mut self) {
        let held: HashSet<u64> = self
            .panes
            .values()
            .flat_map(|s| s.info.approvals.iter().map(|a| a.id))
            .collect();
        self.approvals.seen.retain(|id| held.contains(id));
        let Some(pane) = self.focused.filter(|_| self.mode == Mode::Terminal) else {
            return;
        };
        let held = self.panes.get(&pane).map(|s| &s.info.approvals);
        if let Some(id) = held
            .into_iter()
            .flatten()
            .map(|a| a.id)
            .find(|id| !self.approvals.seen.contains(id))
        {
            self.open_approval(pane, id, Mode::Terminal);
            self.approvals.opened = Some(Instant::now());
        }
    }

    /// Keys over the approval overlay: `y` allows the held tool call, `n`
    /// denies it, `esc` passes it on to Claude's own dialog — each answers the
    /// approval the overlay shows, by id, and closes it. Anything else, chords
    /// included, is dropped — as is every key in the grace after the overlay
    /// opened itself over the pane being typed into.
    pub(super) fn on_key_approval(&mut self, key: KeyEvent, pane: PaneId) -> Vec<WireFrame> {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || self
                .approvals
                .opened
                .is_some_and(|at| at.elapsed() < APPROVAL_GRACE)
        {
            return Vec::new();
        }
        let decision = match key.code {
            KeyCode::Char('y' | 'Y') => ApprovalDecision::Allow,
            KeyCode::Char('n' | 'N') => ApprovalDecision::Deny { message: None },
            KeyCode::Esc => ApprovalDecision::Pass,
            _ => return Vec::new(),
        };
        self.mode = self.approvals.back;
        vec![control(&Request::ApprovalAnswer {
            pane,
            id: self.approvals.shown,
            decision,
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::*;
    use crate::attach::sidebar::{self, SidebarEntry};
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::layout::Rect;
    use tutti_core::{AgentState, Event, TabId, WorkspaceView};

    /// `view_two_workspaces`, blocked pane 3 holding approvals `ids`, oldest
    /// first, each running `touch <id>`.
    fn view_holding(ids: &[u64]) -> Vec<WorkspaceView> {
        let mut view = view_two_workspaces();
        let pane = &mut view[1].tabs[0].panes[1];
        pane.approvals = ids
            .iter()
            .map(|&id| PendingApproval {
                id,
                tool: "Bash".into(),
                summary: format!("touch {id}"),
            })
            .collect();
        pane.approval = pane.approvals.first().cloned();
        view
    }

    /// The answer to pane 3's approval `id`.
    fn answer(id: u64, decision: ApprovalDecision) -> Vec<WireFrame> {
        vec![control(&Request::ApprovalAnswer {
            pane: PaneId(3),
            id: Some(id),
            decision,
        })]
    }

    /// Pane 3 holding approval 7, the sidebar focused with the cursor on its
    /// waiting row, and `p` pressed there.
    fn approval_fixture() -> App {
        let mut app = App::new();
        attach_with(&mut app, view_holding(&[7]));
        focus_sidebar(&mut app);
        let waiting = app
            .sidebar()
            .index_of(sidebar::EntryIdent::Waiting(PaneId(3), Some(7)));
        for _ in 0..waiting.expect("pane 3 waits in the queue") {
            app.on_key(plain('j'));
        }
        assert_eq!(app.on_key(plain('p')), Vec::new(), "opening sends nothing");
        app
    }

    /// The overlay's input row: the held tool call's.
    fn approval_shown_input(app: &App) -> String {
        let (_, _, rows) = app.text_popup(Rect::new(0, 0, 100, 24)).unwrap();
        rows[1].clone()
    }

    /// A pane holding several approvals queues a row for each, and each is
    /// answered on its own: from the queue, the cursor lands on the pane's
    /// next row once one is answered; over the focused pane, the next opens
    /// by itself — the oldest first, never one already shown.
    #[test]
    fn a_pane_holding_several_approvals_answers_each_on_its_own() {
        let mut app = App::new();
        attach_with(&mut app, view_holding(&[7, 9]));
        focus_sidebar(&mut app);
        let queue = app.sidebar();
        let first = queue.index_of(sidebar::EntryIdent::Waiting(PaneId(3), Some(7)));
        let second = queue.index_of(sidebar::EntryIdent::Waiting(PaneId(3), Some(9)));
        assert_eq!(second, first.map(|i| i + 1), "one row each, in order");
        for _ in 0..second.unwrap() {
            app.on_key(plain('j'));
        }
        app.on_key(plain('p'));
        assert_eq!(approval_shown_input(&app), "touch 9", "the row's own");
        assert_eq!(
            app.on_key(plain('n')),
            answer(9, ApprovalDecision::Deny { message: None })
        );
        layout_changed(&mut app, view_holding(&[7]));
        assert_eq!(
            app.sidebar().ident_at(app.sidebar_selected()),
            Some(sidebar::EntryIdent::Waiting(PaneId(3), Some(7))),
            "the cursor falls to the pane's next row in the queue"
        );
        app.on_key(plain('p'));
        assert_eq!(app.on_key(plain('y')), answer(7, ApprovalDecision::Allow));

        let mut app = App::new();
        attach_with(&mut app, view_holding(&[]));
        app.active_tab = Some(TabId(2));
        app.focused = Some(PaneId(3));
        app.focus_change();
        layout_changed(&mut app, view_holding(&[7, 9]));
        assert_eq!(app.mode, Mode::Approval(PaneId(3)));
        assert_eq!(approval_shown_input(&app), "touch 7", "the oldest first");
        app.approvals.opened = None;
        assert_eq!(app.on_key(plain('y')), answer(7, ApprovalDecision::Allow));
        layout_changed(&mut app, view_holding(&[9]));
        assert_eq!(
            app.mode,
            Mode::Approval(PaneId(3)),
            "the next opens by itself"
        );
        assert_eq!(approval_shown_input(&app), "touch 9");
        app.approvals.opened = None;
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        layout_changed(&mut app, view_holding(&[9]));
        app.focus_change();
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "an answer still in flight never reopens it"
        );
    }

    /// A pane whose child exits leaves the queue's approval rows, and its
    /// overlay closes: the daemon dropped what it held.
    #[test]
    fn an_exited_panes_approvals_leave_the_queue() {
        let mut app = approval_fixture();
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::PaneExited {
                pane: PaneId(3),
                code: 0,
            })
            .unwrap(),
        ));
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(
            app.sidebar().entries.iter().all(|e| !matches!(
                e,
                SidebarEntry::Agent(sidebar::AgentRow {
                    approval: Some(_),
                    ..
                })
            )),
            "no row offers an approval"
        );
    }

    /// The overlay remembers which approvals it opened for, not the newest:
    /// pane 2's later #8, shown and answered, never keeps pane 3's earlier
    /// #7 from opening once pane 3 is in view.
    #[test]
    fn an_older_approval_on_another_pane_still_opens_when_its_pane_is_in_view() {
        let held = |id: u64| PendingApproval {
            id,
            tool: "Bash".into(),
            summary: format!("touch {id}"),
        };
        let view = |two: &[u64], three: &[u64]| {
            let mut view = view_two_workspaces();
            let panes = &mut view[1].tabs[0].panes;
            panes[0].approvals = two.iter().map(|&id| held(id)).collect();
            panes[1].approvals = three.iter().map(|&id| held(id)).collect();
            view
        };
        let mut app = App::new();
        attach_with(&mut app, view(&[], &[]));
        app.active_tab = Some(TabId(2));
        app.focused = Some(PaneId(2));
        app.focus_change();
        layout_changed(&mut app, view(&[8], &[7]));
        assert_eq!(app.mode, Mode::Approval(PaneId(2)), "pane 2 is in view");
        app.approvals.opened = None;
        assert_eq!(
            app.on_key(plain('y')),
            vec![control(&Request::ApprovalAnswer {
                pane: PaneId(2),
                id: Some(8),
                decision: ApprovalDecision::Allow,
            })]
        );
        layout_changed(&mut app, view(&[], &[7]));
        assert_eq!(app.mode, Mode::Terminal);

        app.focused = Some(PaneId(3));
        app.focus_change();
        assert_eq!(
            app.mode,
            Mode::Approval(PaneId(3)),
            "#7 opens over its pane"
        );
        assert_eq!(approval_shown_input(&app), "touch 7");
    }

    #[test]
    fn p_on_a_held_approval_opens_its_overlay_and_y_n_esc_answer_it() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        for (key, decision) in [
            (plain('y'), ApprovalDecision::Allow),
            (plain('n'), ApprovalDecision::Deny { message: None }),
            (esc, ApprovalDecision::Pass),
        ] {
            let mut app = approval_fixture();
            assert_eq!(app.mode, Mode::Approval(PaneId(3)));
            assert!(
                app.sidebar_focused(),
                "the queue keeps its highlight beneath"
            );
            assert_eq!(app.on_key(ctrl('y')), Vec::new(), "a chord answers nothing");
            assert_eq!(app.on_key(plain('1')), Vec::new());
            assert_eq!(app.on_key(key), answer(7, decision));
            assert_eq!(
                app.mode,
                Mode::Sidebar,
                "back on the row it was opened from"
            );
        }
    }

    #[test]
    fn the_approval_overlay_closes_itself_once_its_approval_ends() {
        // Answered from another client, passed on, timed out — or the pane
        // gone with it.
        for after in [view_holding(&[]), view_one_pane()] {
            let mut app = approval_fixture();
            state_changed(&mut app, PaneId(2), AgentState::Working, AgentState::Done);
            assert_eq!(
                app.mode,
                Mode::Approval(PaneId(3)),
                "another pane's news is not ours"
            );
            layout_changed(&mut app, after);
            assert_eq!(app.mode, Mode::Sidebar);
        }
    }

    /// The focused pane gaining an approval opens the overlay over it, as
    /// Claude's own dialog would appear — once: dismissed or answered, that
    /// approval never reopens it, while the pane's next one does.
    #[test]
    fn the_focused_panes_approval_opens_over_it_once() {
        let mut app = App::new();
        attach_with(&mut app, view_holding(&[]));
        app.active_tab = Some(TabId(2));
        app.focused = Some(PaneId(3));
        app.focus_change();
        layout_changed(&mut app, view_holding(&[7]));
        assert_eq!(app.mode, Mode::Approval(PaneId(3)), "the pane in view asks");
        assert!(!app.sidebar_focused());

        // A click outside dismisses it, answering nothing, for good.
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        let click = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(app.on_mouse(click, 99, 23), Vec::new());
        assert_eq!(app.mode, Mode::Terminal, "back on the pane");
        layout_changed(&mut app, view_holding(&[7]));
        app.focus_change();
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "a dismissed approval stays dismissed"
        );

        layout_changed(&mut app, view_holding(&[8]));
        assert_eq!(app.mode, Mode::Approval(PaneId(3)), "the next one opens it");
        assert!(
            app.on_key(plain('y')).is_empty(),
            "a key typed into the pane as it asked never answers"
        );
        assert_eq!(app.mode, Mode::Approval(PaneId(3)));
        app.approvals.opened = app
            .approvals
            .opened
            .map(|at| at - APPROVAL_GRACE - Duration::from_millis(1));
        assert_eq!(app.on_key(plain('y')), answer(8, ApprovalDecision::Allow));
        assert_eq!(app.mode, Mode::Terminal);
        layout_changed(&mut app, view_holding(&[8]));
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "nor does an answered one, its end still in flight"
        );
    }

    /// Only the terminal is ever cut into: a panel open over the focused pane
    /// holds the overlay back until it closes, and an approval elsewhere waits
    /// in the queue until a jump lands on its pane.
    #[test]
    fn an_approval_never_cuts_into_a_panel_in_use() {
        let mut app = App::new();
        attach_with(&mut app, view_holding(&[]));
        app.active_tab = Some(TabId(2));
        app.focused = Some(PaneId(3));
        app.on_key(ctrl('b'));
        app.on_key(plain('?'));
        assert_eq!(app.mode, Mode::Help);
        layout_changed(&mut app, view_holding(&[7]));
        app.focus_change();
        assert_eq!(app.mode, Mode::Help, "the help stays up");
        app.on_key(plain('q'));
        app.focus_change();
        assert_eq!(
            app.mode,
            Mode::Approval(PaneId(3)),
            "closing it uncovers the ask"
        );

        let mut app = App::new();
        attach_with(&mut app, view_holding(&[7]));
        app.focus_change();
        assert_eq!(app.mode, Mode::Terminal, "pane 1 is in view, pane 3 asks");
        focus_sidebar(&mut app);
        let tree_row = app
            .sidebar()
            .index_of(sidebar::EntryIdent::Agent(PaneId(3)));
        for _ in 0..tree_row.expect("pane 3 sits in the tree") {
            app.on_key(plain('j'));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focused, Some(PaneId(3)));
        app.focus_change();
        assert_eq!(
            app.mode,
            Mode::Approval(PaneId(3)),
            "the jump lands on the ask"
        );
    }
}
