//! Notifications: a pane's bell or desktop notification marked on its sidebar
//! row and re-emitted to the real terminal, the client's copy of the daemon's
//! history of them, and the panel that lists it (`Mode::Notes`, and
//! `Mode::ConfirmClearNotes` over it).

use std::collections::HashSet;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use tutti_core::{Frame as WireFrame, Note, NoteKind, PaneId, Request};

use super::app::{App, Mode, contains, control};

/// The notifications panel's widest width — a comfortable reading line; a
/// narrower terminal takes all but a margin.
const NOTES_WIDTH: u16 = 100;

/// The notifications panel's state, which means something only while the
/// panel is open.
#[derive(Default)]
pub(super) struct NotesPanel {
    /// The highlighted note's id while the notifications panel is open, so a
    /// note landing never slides the highlight onto another; `None` — an
    /// empty history — highlights the newest.
    anchor: Option<u64>,
    /// The terminal area from the last size sync while the notifications
    /// panel is open, for mouse hit-testing.
    pub(super) area: Option<Rect>,
}

impl App {
    /// Whether `pane` has an unseen notification (drives the sidebar bell mark).
    pub fn is_notified(&self, pane: PaneId) -> bool {
        self.notified.contains(&pane)
    }

    /// How many notes nobody has read — the app bar's notes chip.
    pub fn unread_notes(&self) -> usize {
        self.notes.iter().filter(|n| !n.read).count()
    }

    /// Surface a pane notification. The focused pane is skipped entirely (the
    /// user is already looking). For a background pane: always mark its sidebar
    /// entry, and — when notifications are enabled — flash the status bar and
    /// re-emit to the real terminal so the OS raises a desktop notification.
    pub(super) fn on_notification(
        &mut self,
        pane: PaneId,
        title: Option<String>,
        body: Option<String>,
    ) {
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

    /// Open the notifications panel on the newest note.
    pub(super) fn open_notes(&mut self) {
        self.mode = Mode::Notes;
        self.notes_panel.anchor = self.notes.last().map(|n| n.id);
        self.status = None;
    }

    /// Keys in the notifications panel: `j`/`k`/`↑`/`↓` walk the highlight,
    /// enter jumps to the note's pane, `r` marks every note listed read, `X`
    /// asks to clear the history, esc closes. The prefix chord still reaches
    /// across, closing the panel into prefix mode.
    pub(super) fn on_key_notes(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if self.enter_prefix(key) {
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.notes_step(true),
            KeyCode::Char('k') | KeyCode::Up => self.notes_step(false),
            KeyCode::Enter => return self.jump_to_note(self.notes_selected()),
            // Up to the newest note shown, never one still on its way.
            KeyCode::Char('r') => {
                if let Some(newest) = self.notes.last() {
                    return vec![control(&Request::NotesRead {
                        up_to: Some(newest.id),
                        pane: None,
                    })];
                }
            }
            KeyCode::Char('X') if !self.notes.is_empty() => {
                self.mode = Mode::ConfirmClearNotes;
                self.set_status("clear all notifications? y/N".into());
            }
            KeyCode::Esc => {
                self.mode = Mode::Terminal;
                self.status = None;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Resolve the clear confirm: `y` empties the history, anything else
    /// keeps it. Either way the panel stays open.
    pub(super) fn on_key_confirm_clear_notes(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        self.mode = Mode::Notes;
        self.status = None;
        match key.code {
            KeyCode::Char('y' | 'Y') => vec![control(&Request::NotesClear)],
            _ => Vec::new(),
        }
    }

    /// Walk the panel's highlight one note older (down) or newer (up); it
    /// stays put at either end.
    pub(super) fn notes_step(&mut self, older: bool) {
        let at = self.notes_selected();
        let next = if older { at + 1 } else { at.saturating_sub(1) };
        if let Some(note) = self.notes.iter().rev().nth(next) {
            self.notes_panel.anchor = Some(note.id);
        }
    }

    /// The highlighted panel row, newest first: the anchored note's while it
    /// is listed, else the newest's.
    pub fn notes_selected(&self) -> usize {
        self.notes_panel
            .anchor
            .and_then(|id| self.notes.iter().rev().position(|n| n.id == id))
            .unwrap_or(0)
    }

    /// Jump to the pane of the note on panel row `row` (newest first) — the
    /// go-to jump, so focus follows — and mark that pane's notes read. A note
    /// whose pane is gone flashes so instead, the panel staying open.
    fn jump_to_note(&mut self, row: usize) -> Vec<WireFrame> {
        let Some(pane) = self.notes.iter().rev().nth(row).map(|n| n.pane) else {
            return Vec::new();
        };
        let Some(tab) = self.tab_of_pane(pane) else {
            self.set_status("that pane is gone".into());
            return Vec::new();
        };
        let mut frames = self.jump_to(tab, Some(pane));
        frames.push(control(&Request::NotesRead {
            up_to: None,
            pane: Some(pane),
        }));
        frames
    }

    /// A left-click while the notifications panel is open: on a note, jump
    /// to its pane; outside the panel, close it; on its border or hint row,
    /// nothing.
    pub(super) fn notes_click(&mut self, col: u16, row: u16) -> Vec<WireFrame> {
        let Some((popup, list, first)) = self
            .notes_panel
            .area
            .and_then(|area| self.notes_popup(area))
        else {
            return Vec::new();
        };
        if !contains(popup, col, row) {
            self.mode = Mode::Terminal;
            self.status = None;
            return Vec::new();
        }
        let at = first + row.saturating_sub(list.y) as usize;
        let clicked = self.notes.iter().rev().nth(at).map(|n| n.id);
        match clicked {
            Some(id) if contains(list, col, row) => {
                self.notes_panel.anchor = Some(id);
                self.jump_to_note(at)
            }
            _ => Vec::new(),
        }
    }

    /// The notifications panel while it is open: the popup centred in the
    /// terminal `area` — a reading width, as tall as its notes need between
    /// the app bar and the notification band — the rect its rows draw in
    /// (inside the border, above the hint row), and the first row shown,
    /// newest first: 0 until the highlight would leave the window, then
    /// following it. The one geometry the renderer and the mouse hit-test
    /// share.
    pub fn notes_popup(&self, area: Rect) -> Option<(Rect, Rect, usize)> {
        if !matches!(self.mode, Mode::Notes | Mode::ConfirmClearNotes) {
            return None;
        }
        let width = area.width.saturating_sub(4).min(NOTES_WIDTH);
        let height = (self.notes.len().max(1) as u16 + 3).min(area.height.saturating_sub(4));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let list = Rect::new(
            popup.x + 1,
            popup.y + 1,
            width.saturating_sub(2),
            height.saturating_sub(3),
        );
        let first = self
            .notes_selected()
            .saturating_sub((list.height as usize).saturating_sub(1));
        Some((popup, list, first))
    }
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

/// The panes holding an unread note that rang or said something in `notes` —
/// a bell, a desktop notification, or a transition carrying the message that
/// came with it — the ones the sidebar's bell marks catch up to.
pub(super) fn notified_panes(notes: &[Note]) -> HashSet<PaneId> {
    notes
        .iter()
        .filter(|n| {
            !n.read && (matches!(n.kind, NoteKind::Bell | NoteKind::Notify) || !n.text().is_empty())
        })
        .map(|n| n.pane)
        .collect()
}

/// An OSC 9 desktop-notification escape carrying `text`, BEL-terminated.
fn osc9(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 4);
    out.extend_from_slice(b"\x1b]9;");
    out.extend_from_slice(text.as_bytes());
    out.push(0x07);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::*;
    use crate::config::Config;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use tutti_core::{Event, NOTES_CAP, Response, TabId};

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

    fn note(id: u64, pane: u64, kind: NoteKind) -> Note {
        Note {
            id,
            time: 0,
            pane: PaneId(pane),
            pane_title: format!("pane-{pane}"),
            workspace: "web".into(),
            kind,
            title: None,
            body: None,
            read: false,
        }
    }

    /// Attach to `view_two_workspaces` — the shell pane 1 focused, web's
    /// agents 2 and 3 in tab 2 — with `notes` in the history.
    fn attach_noting(app: &mut App, notes: Vec<Note>) {
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                wire_floor: None,
                session: "t".into(),
                workspaces: view_two_workspaces(),
                restorable: None,
                notes,
            })
            .unwrap(),
        ));
    }

    fn open_notes(app: &mut App) {
        app.on_key(ctrl('b'));
        assert!(app.on_key(shift('N')).is_empty());
        assert_eq!(app.mode, Mode::Notes);
    }

    #[test]
    fn prefix_shift_n_opens_the_notifications_panel_in_both_presets() {
        for config in [Config::default(), vim_config()] {
            let mut app = App::with_config(config);
            attach_noting(&mut app, vec![note(1, 2, NoteKind::Done)]);
            open_notes(&mut app);
            app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(app.mode, Mode::Terminal, "esc closes it");
        }
    }

    #[test]
    fn the_panel_lists_newest_first_and_enter_jumps_to_the_notes_pane() {
        let mut app = App::new();
        attach_noting(
            &mut app,
            vec![
                note(1, 2, NoteKind::Done),
                note(2, 3, NoteKind::Blocked),
                note(3, 1, NoteKind::Bell),
            ],
        );
        open_notes(&mut app);
        assert_eq!(app.notes_selected(), 0, "the newest note is highlighted");
        app.on_key(plain('j'));
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(plain('j'));
        assert_eq!(app.notes_selected(), 2, "the highlight stops at the oldest");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out,
            vec![
                control(&Request::TabSelect { id: TabId(2) }),
                control(&Request::NotesRead {
                    up_to: None,
                    pane: Some(PaneId(3)),
                }),
            ],
            "the jump goes to the blocked agent, marking its pane's notes read"
        );
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(app.focused, Some(PaneId(3)));
    }

    #[test]
    fn r_reads_up_to_the_newest_note_and_x_clears_after_asking() {
        let mut app = App::new();
        attach_noting(
            &mut app,
            vec![note(4, 2, NoteKind::Done), note(9, 3, NoteKind::Notify)],
        );
        open_notes(&mut app);
        assert_eq!(
            app.on_key(plain('r')),
            vec![control(&Request::NotesRead {
                up_to: Some(9),
                pane: None,
            })]
        );
        assert_eq!(app.mode, Mode::Notes, "the panel stays open");

        app.on_key(shift('X'));
        assert_eq!(app.mode, Mode::ConfirmClearNotes);
        assert_eq!(app.transient(), Some("clear all notifications? y/N"));
        assert!(app.on_key(plain('n')).is_empty(), "anything else keeps it");
        assert_eq!(app.mode, Mode::Notes);
        app.on_key(shift('X'));
        assert_eq!(app.on_key(plain('y')), vec![control(&Request::NotesClear)]);
        assert_eq!(app.mode, Mode::Notes);
    }

    #[test]
    fn a_note_whose_pane_is_gone_stays_listed_and_enter_says_so() {
        let mut app = App::new();
        attach_noting(&mut app, vec![note(1, 99, NoteKind::Done)]);
        open_notes(&mut app);
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty());
        assert_eq!(app.transient(), Some("that pane is gone"));
        assert_eq!(app.mode, Mode::Notes, "the panel stays open on it");
        assert_eq!(app.notes.len(), 1);
    }

    #[test]
    fn the_highlight_keeps_its_note_as_new_ones_land() {
        let mut app = App::new();
        attach_noting(
            &mut app,
            vec![note(1, 2, NoteKind::Done), note(2, 3, NoteKind::Blocked)],
        );
        open_notes(&mut app);
        app.on_key(plain('j'));
        event(
            &mut app,
            Event::Noted {
                note: note(3, 1, NoteKind::Bell),
            },
        );
        assert_eq!(app.notes_selected(), 2, "pushed down with its note");
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            out[1],
            control(&Request::NotesRead {
                up_to: None,
                pane: Some(PaneId(2)),
            })
        );
    }

    #[test]
    fn the_census_chip_opens_the_panel_whose_notes_click_through() {
        let mut app = App::new();
        attach_noting(
            &mut app,
            vec![note(1, 2, NoteKind::Done), note(2, 3, NoteKind::Blocked)],
        );
        let area = Rect::new(0, 0, 100, 24);
        app.sync_sizes(area);
        let (_, chip) = app.notes_chip(App::header_rects(area).0);
        let down = MouseEventKind::Down(MouseButton::Left);
        assert!(app.on_mouse(down, chip.x + 1, chip.y).is_empty());
        assert_eq!(app.mode, Mode::Notes);

        app.sync_sizes(area);
        let (popup, list, _) = app.notes_popup(area).unwrap();
        app.on_mouse(MouseEventKind::ScrollDown, list.x, list.y);
        assert_eq!(app.notes_selected(), 1, "the wheel walks the highlight");
        let out = app.on_mouse(down, list.x + 4, list.y + 1);
        assert_eq!(
            out.first(),
            Some(&control(&Request::TabSelect { id: TabId(2) })),
            "a click on the older note jumps to pane 2"
        );
        assert_eq!(app.focused, Some(PaneId(2)));

        app.on_mouse(down, chip.x, chip.y);
        app.sync_sizes(area);
        assert!(app.on_mouse(down, popup.x - 1, popup.y).is_empty());
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "a press outside closes it, reaching nothing beneath"
        );
    }

    #[test]
    fn a_note_lands_once_in_id_order_and_the_copy_caps_like_the_daemons() {
        let mut app = App::new();
        attach_noting(&mut app, Vec::new());
        for id in [1, 3, 2, 3] {
            event(
                &mut app,
                Event::Noted {
                    note: note(id, 2, NoteKind::Notify),
                },
            );
        }
        let ids: Vec<u64> = app.notes.iter().map(|n| n.id).collect();
        assert_eq!(ids, [1, 2, 3], "a late note slots in, a repeat is dropped");
        for id in 4..=NOTES_CAP as u64 + 2 {
            event(
                &mut app,
                Event::Noted {
                    note: note(id, 2, NoteKind::Notify),
                },
            );
        }
        assert_eq!(app.notes.len(), NOTES_CAP);
        assert_eq!(app.notes[0].id, 3, "the oldest left first");
        assert_eq!(app.unread_notes(), NOTES_CAP);
    }

    #[test]
    fn a_rebroadcast_history_clears_the_marks_of_panes_read_elsewhere() {
        let mut app = App::new();
        attach_noting(&mut app, Vec::new());
        notify(&mut app, PaneId(2), None, Some("built"));
        notify(&mut app, PaneId(3), None, None);
        let built = Note {
            body: Some("built".into()),
            ..note(1, 2, NoteKind::Notify)
        };
        let mut notes = vec![built, note(2, 3, NoteKind::Bell)];
        for note in &notes {
            event(&mut app, Event::Noted { note: note.clone() });
        }
        assert!(app.is_notified(PaneId(2)) && app.is_notified(PaneId(3)));

        // Pane 2's notes were read — by another client, or a script.
        notes[0].read = true;
        event(&mut app, Event::NotesChanged { notes });
        assert!(!app.is_notified(PaneId(2)), "its mark follows the history");
        assert!(app.is_notified(PaneId(3)), "pane 3's bell is still unread");
        assert_eq!(app.unread_notes(), 1);

        event(&mut app, Event::NotesChanged { notes: Vec::new() });
        assert!(
            !app.is_notified(PaneId(3)),
            "a clear leaves nothing pending"
        );
        assert!(app.notes.is_empty());
    }

    #[test]
    fn an_attach_catches_up_on_the_history_and_its_unread_marks() {
        let mut app = App::new();
        let read = Note {
            read: true,
            ..note(1, 2, NoteKind::Notify)
        };
        let built = Note {
            title: Some("build".into()),
            body: Some("Built".into()),
            ..note(4, 5, NoteKind::Done)
        };
        attach_noting(
            &mut app,
            vec![
                read,
                note(2, 3, NoteKind::Bell),
                note(3, 2, NoteKind::Done),
                built,
            ],
        );
        assert_eq!(app.notes.len(), 4);
        assert_eq!(app.unread_notes(), 3);
        assert!(app.is_notified(PaneId(3)), "an unread bell marks its pane");
        assert!(
            !app.is_notified(PaneId(2)),
            "a read notification marks nothing, and a bare done is no notification"
        );
        assert!(
            app.is_notified(PaneId(5)),
            "a done carrying its report's msg is that notification"
        );
    }

    /// A restore points the history's notes at the panes that came back, and
    /// their unread notes mark them at once, as the next attach would; a
    /// pane the history already named keeps the mark it had.
    #[test]
    fn a_restore_marks_the_panes_its_notes_came_back_as() {
        let mut app = App::new();
        let blocked = Note {
            body: Some("allow edit?".into()),
            ..note(2, 3, NoteKind::Blocked)
        };
        attach_noting(&mut app, vec![note(1, 2, NoteKind::Bell), blocked]);
        app.notified.remove(&PaneId(3));
        assert!(app.is_notified(PaneId(2)));

        let restored = vec![
            note(1, 12, NoteKind::Bell),
            Note {
                body: Some("allow edit?".into()),
                ..note(2, 3, NoteKind::Blocked)
            },
            Note {
                read: true,
                ..note(3, 13, NoteKind::Bell)
            },
        ];
        event(&mut app, Event::NotesChanged { notes: restored });
        assert!(app.is_notified(PaneId(12)), "its unread bell came back");
        assert!(!app.is_notified(PaneId(13)), "a read note marks nothing");
        assert!(!app.is_notified(PaneId(2)), "the pane it was is gone");
        assert!(
            !app.is_notified(PaneId(3)),
            "a pane the history named already gains no mark"
        );
    }
}
