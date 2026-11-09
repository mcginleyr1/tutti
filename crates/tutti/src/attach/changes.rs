//! The changes panel (`Mode::DiffFiles`, and `Mode::ConfirmRestore` over it):
//! the active workspace's changed files and the turns of the agent in view,
//! whose diffs page in an ephemeral pane, whose files open in the workspace's
//! editor pane, and whose turns restore.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use tutti_core::{Checkpoint, DIFF_RANGE, FileStat, Frame as WireFrame, PaneId, Request};

use super::app::{App, FocusDir, Mode, control};
use crate::config::{self, Action, ChangesAction};
use crate::editor;
use crate::host::Host;

/// The changed-files panel's fixed column width, and the minimum content width
/// below which it is suppressed so panes keep usable room even beside the
/// sidebar.
pub(super) const DIFF_PANEL_WIDTH: u16 = 38;
const DIFF_PANEL_MIN_TOTAL: u16 = 130;

/// How many of the newest turns the changes panel's `turns` section lists; the
/// CLI (`tutti pane checkpoints`) reaches the rest.
const TURNS_SHOWN: usize = 5;

/// The changes panel's state. The cursor and the rect outlive the panel's
/// hold on the keys: the panel stays drawn, its highlight where it was left.
#[derive(Default)]
pub(super) struct Changes {
    /// The changed-files panel column from the last size sync, for mouse
    /// hit-testing. `None` while the panel is hidden.
    pub(super) rect: Option<Rect>,
    /// The highlighted row in the changed-files panel while it is focused: the
    /// changed files first, then the listed turns.
    pub(super) selected: usize,
    /// The file the changes panel's `e` asks the event loop to hand to an
    /// editor pane.
    editor_job: Option<EditorJob>,
}

impl App {
    /// Whether the changed-files panel renders: enabled in config, the active
    /// workspace actually has changes against trunk or the agent in view has
    /// turns to list, and the content is wide enough that panes keep usable
    /// room beside both chrome columns.
    pub(super) fn diff_panel_shown(&self, total_width: u16) -> bool {
        self.config.diff_panel && total_width >= DIFF_PANEL_MIN_TOTAL && self.diff_rows() > 0
    }

    /// The active workspace's changed files — what the changed-files panel
    /// lists. Empty when no workspace is active or it is clean against trunk.
    pub fn changed_files(&self) -> &[FileStat] {
        self.active_workspace()
            .map_or(&[], |w| w.changed_files.as_slice())
    }

    /// The pane whose checkpoints the changes panel's `turns` section lists:
    /// the focused pane when it has any, else the first pane of the active tab
    /// that does — so a pager opened over a turn, or a shell split beside the
    /// agent, keeps the agent's turns in view.
    pub fn turns_pane(&self) -> Option<PaneId> {
        let has = |p: &PaneId| {
            self.panes
                .get(p)
                .is_some_and(|s| !s.info.checkpoints.is_empty())
        };
        self.focused
            .filter(has)
            .or_else(|| self.active_tab_panes().into_iter().find(has))
    }

    /// The turns the changes panel lists, newest first: the last few of the
    /// `turns_pane`'s checkpoints.
    pub fn shown_turns(&self) -> Vec<&Checkpoint> {
        self.turns_pane()
            .and_then(|p| self.panes.get(&p))
            .map_or(Vec::new(), |s| {
                s.info.checkpoints.iter().rev().take(TURNS_SHOWN).collect()
            })
    }

    /// Every row the changes-panel cursor can land on: files, then turns.
    pub(super) fn diff_rows(&self) -> usize {
        self.changed_files().len() + self.shown_turns().len()
    }

    /// The turn under the changes-panel cursor, when it sits past the files.
    fn selected_turn(&self) -> Option<Checkpoint> {
        let i = self
            .changes
            .selected
            .checked_sub(self.changed_files().len())?;
        self.shown_turns().get(i).map(|&turn| turn.clone())
    }

    /// Whether the changes panel holds the keys — including while its restore
    /// confirm is open, so the highlight stays on the turn being asked about.
    pub fn diff_panel_focused(&self) -> bool {
        matches!(self.mode, Mode::DiffFiles | Mode::ConfirmRestore(..))
    }

    /// Focus the changed-files panel. Refuses — with a status explaining why —
    /// when the active workspace is clean against trunk with no turns to list,
    /// the panel is disabled in config, or the terminal is too narrow for it.
    pub(super) fn focus_diff_panel(&mut self) {
        if self.diff_rows() == 0 {
            self.set_status("no changes against trunk".into());
            return;
        }
        if !self.diff_panel_shown(self.last_content_width) {
            self.set_status("terminal too narrow for the changes panel".into());
            return;
        }
        self.changes.selected = self.changes.selected.min(self.diff_rows() - 1);
        self.mode = Mode::DiffFiles;
        self.status = None;
    }

    /// Keys while the changed-files panel is focused, dispatched through
    /// `config::CHANGES_TABLE` (which the help overlay and bottom-bar hint
    /// render): vim/less list motion over the files and then the turns,
    /// `enter` opens the highlighted file's or turn's diff, `e` the file in the
    /// workspace's editor pane, `d` the whole diff, `r` asks to restore the
    /// highlighted turn. The panel is the content's
    /// right edge, and its edges behave like a pane's: `h`/`esc`/`q` and the
    /// focus-left binding step back into the pane area keeping its focus; `l`
    /// wraps on to the next tab, and the focus-right binding goes past tutti's
    /// right edge — on to rtty, else the next tab — as up and down go on to
    /// rtty in it.
    pub(super) fn on_key_diff_files(&mut self, key: KeyEvent) -> Vec<WireFrame> {
        if self.enter_prefix(key) {
            return Vec::new();
        }
        match self.config.keys.action_for(key) {
            Some(Action::FocusLeft) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return Vec::new();
            }
            Some(Action::FocusRight) => {
                self.mode = Mode::Terminal;
                self.status = None;
                return self.past_edge(FocusDir::Right);
            }
            Some(Action::FocusUp) if self.rtty.is_some() => return self.past_edge(FocusDir::Up),
            Some(Action::FocusDown) if self.rtty.is_some() => {
                return self.past_edge(FocusDir::Down);
            }
            _ => {}
        }
        let last = self.diff_rows().saturating_sub(1);
        let page = self
            .changes
            .rect
            .map_or(0, |r| r.height.saturating_sub(1))
            .max(1) as usize;
        let half = (page / 2).max(1);
        let Some(action) = config::panel_action(config::CHANGES_TABLE, key) else {
            return Vec::new();
        };
        self.changes.selected = match action {
            ChangesAction::Down => (self.changes.selected + 1).min(last),
            ChangesAction::Up => self.changes.selected.saturating_sub(1),
            ChangesAction::HalfPageDown => (self.changes.selected + half).min(last),
            ChangesAction::HalfPageUp => self.changes.selected.saturating_sub(half),
            ChangesAction::PageDown => (self.changes.selected + page).min(last),
            ChangesAction::PageUp => self.changes.selected.saturating_sub(page),
            ChangesAction::Top => 0,
            ChangesAction::Bottom => last,
            ChangesAction::Open => {
                return match self.selected_turn() {
                    Some(turn) => self.open_turn_diff(&turn),
                    None => self.open_selected_file_diff(),
                };
            }
            ChangesAction::Edit => {
                self.open_in_editor();
                return Vec::new();
            }
            ChangesAction::Restore => {
                self.confirm_restore();
                return Vec::new();
            }
            ChangesAction::FullDiff => {
                let Some(tab) = self.active_tab else {
                    return Vec::new();
                };
                self.mode = Mode::Terminal;
                self.status = None;
                return vec![control(&Request::PaneRun {
                    tab: Some(tab),
                    cmd: diff_pager_cmd(None),
                    ephemeral: true,
                    sandbox: None,
                })];
            }
            ChangesAction::NextTab => {
                self.mode = Mode::Terminal;
                self.status = None;
                return self.switch_tab(1);
            }
            ChangesAction::Back => {
                self.mode = Mode::Terminal;
                self.status = None;
                return Vec::new();
            }
        };
        Vec::new()
    }

    /// Open the highlighted file's diff against trunk in an ephemeral pager
    /// pane, in the active workspace's tab.
    fn open_selected_file_diff(&mut self) -> Vec<WireFrame> {
        let Some(path) = self
            .changed_files()
            .get(self.changes.selected)
            .map(|f| f.path.clone())
        else {
            return Vec::new();
        };
        let Some(tab) = self.active_tab else {
            return Vec::new();
        };
        self.mode = Mode::Terminal;
        self.status = None;
        vec![control(&Request::PaneRun {
            tab: Some(tab),
            cmd: diff_pager_cmd(Some(&path)),
            ephemeral: true,
            sandbox: None,
        })]
    }

    /// Open `turn`'s diff in an ephemeral pager pane in the active tab, the way
    /// `d` pages the whole workspace's. The first state recorded for a pane has
    /// nothing earlier to diff against, which the status line says instead.
    fn open_turn_diff(&mut self, turn: &Checkpoint) -> Vec<WireFrame> {
        let Some(from) = &turn.from else {
            self.set_status(format!("#{} is where the history starts", turn.n));
            return Vec::new();
        };
        let Some(tab) = self.active_tab else {
            return Vec::new();
        };
        self.mode = Mode::Terminal;
        self.status = None;
        vec![control(&Request::PaneRun {
            tab: Some(tab),
            cmd: turn_pager_cmd(from, &turn.commit),
            ephemeral: true,
            sandbox: None,
        })]
    }

    /// Hand the highlighted file to the active workspace's editor pane — the
    /// one `tutti edit` picks, the focused pane's tab first — for the event
    /// loop to open off its thread (`take_editor_job`). A turn row names no
    /// one file.
    fn open_in_editor(&mut self) {
        let Some(file) = self
            .changed_files()
            .get(self.changes.selected)
            .map(|f| f.path.clone())
        else {
            self.set_status("select a file to open".into());
            return;
        };
        let Some((id, dir)) = self.active_workspace().map(|w| (w.id, w.dir.clone())) else {
            return;
        };
        let found = editor::pane(&self.workspaces, self.focused, Some(id)).map(|(name, found)| {
            let found = found.map(|(pane, editor)| (pane, editor.to_string()));
            (name.to_string(), found)
        });
        match found {
            Ok((_, Some((pane, editor)))) => {
                self.changes.editor_job = Some(EditorJob {
                    pane,
                    editor,
                    dir,
                    file,
                })
            }
            Ok((name, None)) => self.set_status(format!("error: no editor pane in {name}")),
            Err(err) => self.set_status(format!("error: {err:#}")),
        }
    }

    /// The file `e` asked to hand to an editor pane, for the event loop to
    /// open off its thread; the outcome comes back through `editor_opened`.
    pub fn take_editor_job(&mut self) -> Option<EditorJob> {
        self.changes.editor_job.take()
    }

    /// Land an editor pane's answer for `file`: once it has the file, jump to
    /// the pane as a sidebar jump does and say so; else the editor's own
    /// complaint, in the error accent. A pane gone meanwhile is left be.
    pub fn editor_opened(
        &mut self,
        pane: PaneId,
        file: &str,
        opened: Result<(), String>,
    ) -> Vec<WireFrame> {
        if let Err(err) = opened {
            self.set_status(format!("error: {err}"));
            return Vec::new();
        }
        let tab = self
            .all_tabs()
            .into_iter()
            .find(|t| t.layout.as_ref().is_some_and(|l| l.contains(pane)))
            .map(|t| t.id);
        let title = self.panes.get(&pane).map(|s| s.info.title.clone());
        let (Some(tab), Some(title)) = (tab, title) else {
            return Vec::new();
        };
        let frames = self.jump_to(tab, Some(pane));
        self.set_status(format!("opened {file} in {title}"));
        frames
    }

    /// Open the restore confirm for the highlighted turn: `y` restores the
    /// working copy to it, anything else cancels. A file row has no turn.
    fn confirm_restore(&mut self) {
        let (Some(pane), Some(turn)) = (self.turns_pane(), self.selected_turn()) else {
            self.set_status("select a turn to restore".into());
            return;
        };
        self.mode = Mode::ConfirmRestore(pane, turn.n);
        self.set_status(format!("restore the working copy to #{}? y/N", turn.n));
    }

    /// Resolve the restore confirm: `y` sends the restore only when the same
    /// pane still lists checkpoint `n` (a kill may have dropped it since the
    /// confirm opened); anything else cancels. The panel keeps the keys while it
    /// has rows; a server refusal (the agent is mid-turn, the workspace stale)
    /// lands as an error in the notification band.
    pub(super) fn on_key_confirm_restore(
        &mut self,
        key: KeyEvent,
        pane: PaneId,
        n: u64,
    ) -> Vec<WireFrame> {
        self.mode = if self.diff_rows() == 0 {
            Mode::Terminal
        } else {
            Mode::DiffFiles
        };
        self.status = None;
        if !matches!(key.code, KeyCode::Char('y' | 'Y')) {
            return Vec::new();
        }
        let listed = self
            .panes
            .get(&pane)
            .is_some_and(|s| s.info.checkpoints.iter().any(|c| c.n == n));
        if !listed {
            self.set_status(format!("#{n} is gone"));
            return Vec::new();
        }
        self.set_status(format!("restoring #{n}…"));
        vec![control(&Request::PaneRestore {
            pane,
            n,
            force: false,
        })]
    }

    /// A click on the changed-files panel: focus it and open the clicked file's
    /// or turn's diff. The top row is the panel header; file rows follow the
    /// same scroll-follow window the renderer draws, then the turns block (a
    /// spacer and its header, then one row per turn).
    pub(super) fn diff_panel_click(&mut self, row: u16) -> Vec<WireFrame> {
        let Some(rect) = self.changes.rect else {
            return Vec::new();
        };
        if self.diff_rows() == 0 {
            return Vec::new();
        }
        if self.mode != Mode::DiffFiles {
            self.mode = Mode::DiffFiles;
            self.status = None;
        }
        let rel = row.saturating_sub(rect.y) as usize;
        let Some(row_in_list) = rel.checked_sub(1) else {
            return Vec::new();
        };
        let first = self.diff_first_visible(rect.height);
        let drawn = self.diff_files_drawn(rect.height);
        if row_in_list < drawn {
            self.changes.selected = first + row_in_list;
            return self.open_selected_file_diff();
        }
        let Some(j) = (row_in_list - drawn).checked_sub(2) else {
            return Vec::new();
        };
        if j >= self.shown_turns().len() {
            return Vec::new();
        }
        self.changes.selected = self.changed_files().len() + j;
        match self.selected_turn() {
            Some(turn) => self.open_turn_diff(&turn),
            None => Vec::new(),
        }
    }

    /// How many file rows fit in a changes panel of `panel_height`: all of it
    /// below the header, less the turns block at the foot (a spacer, its
    /// header, and one row per listed turn) when there are turns.
    fn diff_file_window(&self, panel_height: u16) -> usize {
        let turns = self.shown_turns().len();
        let reserved = if turns == 0 { 0 } else { 2 + turns };
        (panel_height as usize).saturating_sub(1 + reserved)
    }

    /// How many file rows a panel of `panel_height` actually draws — the turns
    /// block follows straight after them. Shared by the renderer and clicks.
    pub fn diff_files_drawn(&self, panel_height: u16) -> usize {
        let files = self.changed_files().len();
        let first = self.diff_first_visible(panel_height);
        files
            .saturating_sub(first)
            .min(self.diff_file_window(panel_height))
    }

    /// The first file row the changed-files panel shows for `panel_height` (one
    /// header row sits above the list, the turns block below it): 0 until the
    /// selection would leave the window, then following it — a cursor down in
    /// the turns keeps the files' window where it was. Shared by the renderer
    /// and click hit-testing so clicks land on what is drawn.
    pub fn diff_first_visible(&self, panel_height: u16) -> usize {
        let visible = self.diff_file_window(panel_height);
        if visible == 0 {
            return 0;
        }
        let cursor = self
            .changes
            .selected
            .min(self.changed_files().len().saturating_sub(1));
        cursor.saturating_sub(visible - 1)
    }
}

/// A file the changes panel hands to an editor pane (`App::take_editor_job`):
/// the pane, its `TUTTI_EDITOR` value, the workspace's directory, and the
/// file's repo-root-relative path as the panel lists it.
#[derive(Debug, PartialEq)]
pub struct EditorJob {
    pub pane: PaneId,
    editor: String,
    dir: PathBuf,
    pub file: String,
}

impl EditorJob {
    /// Open the file in the editor, on the daemon's machine — blocking, so
    /// the event loop runs it on a thread.
    pub fn open(&self, host: &Host) -> Result<(), String> {
        host.edit(&self.editor, &self.dir, &self.file)
            .map_err(|e| format!("{e:#}"))
    }
}

/// The ephemeral-pane command paging a workspace's diff against trunk
/// (`DIFF_RANGE`), optionally scoped to one repo-root-relative `path`. stderr
/// folds into the pager so a jj failure (a stale workspace, say) is read, not
/// lost. The path rides a `root:"…"` fileset pattern — panel paths are
/// repo-root-relative, while a bare pattern would resolve against the pane's
/// cwd, which may sit below the root.
pub(super) fn diff_pager_cmd(path: Option<&str>) -> Vec<String> {
    let scope = path.map_or_else(String::new, |p| format!(" {}", sh_quote(&root_pattern(p))));
    vec![
        "sh".into(),
        "-lc".into(),
        format!("jj --no-pager diff --color=always -r '{DIFF_RANGE}'{scope} 2>&1 | less -R"),
    ]
}

/// The ephemeral-pane command paging one turn's diff, `from..to`. Both ends
/// are commit ids, so the pager reads commits only (`--ignore-working-copy`)
/// and records no jj operation of its own.
fn turn_pager_cmd(from: &str, to: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-lc".into(),
        format!(
            "jj --no-pager --ignore-working-copy diff --color=always --from {} --to {} 2>&1 | less -R",
            sh_quote(from),
            sh_quote(to)
        ),
    ]
}

/// A jj fileset pattern selecting exactly the repo-root-relative `path`, with
/// the characters special inside a fileset string literal escaped.
fn root_pattern(path: &str) -> String {
    format!(
        "root:\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// `s` single-quoted for `sh -c`, with embedded single quotes escaped the only
/// way sh allows: close, backslash-quote, reopen.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::*;
    use crate::config::Config;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use tutti_core::{AgentState, Direction, Event, Response, TabId, WorkspaceView};

    #[test]
    fn prefix_f_focuses_the_panel_and_enter_opens_the_file_diff() {
        let mut app = App::new();
        attach_with(&mut app, view_with_changes());
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        assert_eq!(app.mode, Mode::DiffFiles);

        app.on_key(plain('j'));
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match only_request(&out) {
            Request::PaneRun {
                tab,
                cmd,
                ephemeral,
                sandbox: None,
            } => {
                assert_eq!(tab, Some(TabId(1)));
                assert!(ephemeral, "the pager pane vanishes when less quits");
                assert!(
                    cmd[2].contains(DIFF_RANGE),
                    "the file diff uses the against-trunk revset: {}",
                    cmd[2]
                );
                assert!(
                    cmd[2].contains("root:\"crates/deep/dir/mod.rs\""),
                    "the diff is scoped to the highlighted file, root-relative: {}",
                    cmd[2]
                );
            }
            other => panic!("expected PaneRun, got {other:?}"),
        }
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "opening the diff leaves the panel"
        );
    }

    /// `e` on a file row hands it to the active workspace's editor pane — the
    /// focused pane's tab first — through the event loop, which jumps there
    /// and says so once the editor has it. No editor pane, or an editor that
    /// refuses, lands in the band's error accent.
    #[test]
    fn e_hands_the_highlighted_file_to_the_workspaces_editor_pane() {
        assert_eq!(
            config::panel_action(config::CHANGES_TABLE, plain('e')),
            Some(ChangesAction::Edit)
        );
        let editor = |id, title: &str, value: &str| {
            let mut info = crate::attach::fixtures::pane(id, title, None, AgentState::Idle);
            info.user_vars.insert("TUTTI_EDITOR".into(), value.into());
            info
        };
        let mut view = view_with_changes();
        view[0].tabs = vec![
            tab(
                1,
                "1",
                true,
                split(Direction::Horizontal, leaf(1), leaf(2)),
                vec![shell(1), editor(2, "nvim", "nvim /tmp/n.sock")],
            ),
            tab(2, "2", false, leaf(3), vec![editor(3, "emacs", "emacs")]),
        ];
        let mut app = App::new();
        attach_with(&mut app, view);
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        let status = |app: &App| app.status.as_ref().map(|(text, _)| text.clone());
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        app.on_key(plain('j'));
        assert!(
            app.on_key(plain('e')).is_empty(),
            "the editor runs off the loop"
        );
        assert_eq!(
            app.take_editor_job(),
            Some(EditorJob {
                pane: PaneId(2),
                editor: "nvim /tmp/n.sock".into(),
                dir: PathBuf::from("/tmp/w"),
                file: "crates/deep/dir/mod.rs".into(),
            }),
            "the focused pane's tab first"
        );
        assert_eq!(app.take_editor_job(), None, "a job is taken once");

        let file = "crates/deep/dir/mod.rs";
        assert_eq!(
            app.editor_opened(PaneId(2), file, Ok(())),
            vec![control(&Request::TabSelect { id: TabId(1) })]
        );
        assert_eq!((app.focused, app.mode), (Some(PaneId(2)), Mode::Terminal));
        assert_eq!(
            status(&app).as_deref(),
            Some(&*format!("opened {file} in nvim"))
        );
        let refused = "nvim could not open /tmp/w/x: no server";
        assert!(
            app.editor_opened(PaneId(2), "x", Err(refused.into()))
                .is_empty()
        );
        assert_eq!(status(&app), Some(format!("error: {refused}")));

        layout_changed(&mut app, view_with_changes());
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        app.on_key(plain('e'));
        assert_eq!(app.take_editor_job(), None);
        assert_eq!(
            status(&app).as_deref(),
            Some("error: no editor pane in api")
        );
    }

    #[test]
    fn panel_vim_keys_jump_page_and_step_back_to_the_panes() {
        let mut app = App::new();
        let mut view = view_with_changes();
        view[0].changed_files = (0..200)
            .map(|i| FileStat {
                path: format!("src/f{i}.rs"),
                insertions: i,
                deletions: 0,
            })
            .collect();
        attach_with(&mut app, view);
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        assert_eq!(app.mode, Mode::DiffFiles);
        let page = app.changes.rect.unwrap().height as usize - 1;
        let half = page / 2;
        assert!(
            half > 1 && 2 * page < 200,
            "the list must overflow the panel"
        );

        app.on_key(plain('G'));
        assert_eq!(app.changes.selected, 199, "G jumps to the last file");
        app.on_key(plain('g'));
        assert_eq!(app.changes.selected, 0, "g jumps to the first file");
        assert!(
            app.on_key(ctrl('d')).is_empty(),
            "C-d pages; it must not open the full diff like plain d"
        );
        assert_eq!(app.changes.selected, half);
        app.on_key(ctrl('d'));
        assert_eq!(app.changes.selected, 2 * half);
        app.on_key(ctrl('u'));
        assert_eq!(app.changes.selected, half);
        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.changes.selected, half + page);
        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.changes.selected, 199);
        app.on_key(ctrl('d'));
        assert_eq!(app.changes.selected, 199, "paging clamps at the last file");
        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.changes.selected, 199 - page);
        app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.changes.selected, 0);
        app.on_key(ctrl('u'));
        assert_eq!(
            app.changes.selected, 0,
            "paging saturates at the first file"
        );
        assert_eq!(
            app.mode,
            Mode::DiffFiles,
            "motion keys keep the panel focused"
        );

        assert!(app.on_key(plain('h')).is_empty());
        assert_eq!(app.mode, Mode::Terminal, "h steps back into the panes");
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        assert_eq!(app.changes.selected, 0, "refocusing keeps the cursor");
        app.on_key(plain('l'));
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "l steps back too: nothing sits right of the panel"
        );
    }

    #[test]
    fn prefix_f_with_a_clean_workspace_reports_instead_of_focusing() {
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        assert_eq!(app.mode, Mode::Terminal, "nothing to show, focus stays put");
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("no changes against trunk")),
            "the transient explains why the panel did not open"
        );
    }

    #[test]
    fn changed_files_panel_needs_changes_and_width() {
        let content = Rect::new(0, 0, 140, 37);

        // A clean workspace shows no panel.
        let mut app = App::new();
        attach_with(&mut app, view_two_workspaces());
        assert!(app.split_content(content).2.is_none());

        // With changes and room, the panel takes the right edge and panes give
        // up its width.
        let mut app = App::new();
        attach_with(&mut app, view_with_changes());
        let (_, panes, panel) = app.split_content(content);
        let panel = panel.expect("changes + width show the panel");
        assert_eq!(panel.width, 38);
        assert_eq!(panel.x + panel.width, 140);
        assert_eq!(panes.width, 140 - 30 - 38);

        // Too narrow, or disabled in config, and it is suppressed.
        assert!(app.split_content(Rect::new(0, 0, 100, 37)).2.is_none());
        let mut app = App::with_config(Config::parse("diff_panel = \"off\"\n").unwrap());
        attach_with(&mut app, view_with_changes());
        assert!(app.split_content(content).2.is_none());
    }

    #[test]
    fn clicking_a_panel_row_opens_that_files_diff() {
        let mut app = App::new();
        attach_with(&mut app, view_with_changes());
        app.sync_sizes(Rect::new(0, 0, 140, 40));

        // The panel spans x 108..140 below the header; its top row is the
        // CHANGES header, so the second file sits two rows down.
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 110, 4);
        match only_request(&out) {
            Request::PaneRun { cmd, .. } => assert!(
                cmd[2].contains("root:\"crates/deep/dir/mod.rs\""),
                "the click resolves to the drawn row: {}",
                cmd[2]
            ),
            other => panic!("expected PaneRun, got {other:?}"),
        }
    }

    #[test]
    fn diff_pager_quoting_survives_hostile_paths() {
        assert_eq!(
            root_pattern(r#"a"b\c.txt"#),
            r#"root:"a\"b\\c.txt""#,
            "fileset string literals escape quotes and backslashes"
        );
        assert_eq!(
            sh_quote("it's"),
            r#"'it'\''s'"#,
            "single quotes close, escape, and reopen for sh"
        );
    }

    fn checkpoint(n: u64, from: Option<&str>, commit: &str, label: &str) -> Checkpoint {
        Checkpoint {
            n,
            commit: commit.into(),
            from: from.map(Into::into),
            time: 0,
            label: label.into(),
            prompt: None,
            files: 1,
            insertions: n,
            deletions: 0,
        }
    }

    /// `view_with_changes` whose one pane is an agent with three turns behind
    /// it: where it started, a turn that blocked, and one that finished.
    fn view_with_turns() -> Vec<WorkspaceView> {
        let mut view = view_with_changes();
        let pane = &mut view[0].tabs[0].panes[0];
        pane.agent = Some("claude".into());
        pane.checkpoints = vec![
            checkpoint(1, None, "c1", "start"),
            checkpoint(2, Some("c1"), "c2", "blocked"),
            checkpoint(3, Some("c2"), "c3", "done"),
        ];
        view
    }

    fn focus_panel(app: &mut App) {
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        assert_eq!(app.mode, Mode::DiffFiles);
    }

    #[test]
    fn the_panel_walks_from_files_into_turns_and_enter_pages_a_turn() {
        let mut app = App::new();
        attach_with(&mut app, view_with_turns());
        focus_panel(&mut app);
        let shown: Vec<u64> = app.shown_turns().iter().map(|t| t.n).collect();
        assert_eq!(shown, [3, 2, 1], "the newest turn lists first");

        app.on_key(plain('G'));
        assert_eq!(app.changes.selected, 4, "two files, then three turns");
        app.on_key(plain('k'));
        app.on_key(plain('k'));
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match only_request(&out) {
            Request::PaneRun {
                tab,
                cmd,
                ephemeral,
                sandbox: None,
            } => {
                assert_eq!(tab, Some(TabId(1)));
                assert!(ephemeral, "the pager pane vanishes when less quits");
                assert!(
                    cmd[2].contains("--from 'c2' --to 'c3'"),
                    "the newest turn diffs from the one before it: {}",
                    cmd[2]
                );
                assert!(
                    cmd[2].contains("--ignore-working-copy"),
                    "paging commits never snapshots: {}",
                    cmd[2]
                );
            }
            other => panic!("expected PaneRun, got {other:?}"),
        }
        assert_eq!(
            app.mode,
            Mode::Terminal,
            "opening the diff leaves the panel"
        );
    }

    #[test]
    fn enter_on_the_first_state_explains_instead_of_paging() {
        let mut app = App::new();
        attach_with(&mut app, view_with_turns());
        focus_panel(&mut app);
        app.on_key(plain('G'));
        let out = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_empty(), "#1 has nothing earlier to diff against");
        assert!(app.transient().is_some_and(|t| t.contains("#1")));
        assert_eq!(app.mode, Mode::DiffFiles);
    }

    #[test]
    fn r_confirms_a_restore_and_only_y_sends_it() {
        let mut app = App::new();
        attach_with(&mut app, view_with_turns());
        focus_panel(&mut app);
        app.on_key(plain('G'));
        assert!(app.on_key(plain('r')).is_empty(), "r asks first");
        assert_eq!(app.mode, Mode::ConfirmRestore(PaneId(1), 1));
        assert!(app.diff_panel_focused(), "the turn stays highlighted");
        assert!(app.transient().is_some_and(|t| t.contains("#1? y/N")));

        let out = app.on_key(plain('y'));
        assert_eq!(
            only_request(&out),
            Request::PaneRestore {
                pane: PaneId(1),
                n: 1,
                force: false,
            },
            "the TUI never forces: a mid-turn refusal comes back as an error"
        );
        assert_eq!(app.mode, Mode::DiffFiles);

        app.on_key(plain('r'));
        assert!(app.on_key(plain('n')).is_empty(), "any other key cancels");
        assert_eq!(app.mode, Mode::DiffFiles);

        app.on_key(plain('g'));
        assert!(app.on_key(plain('r')).is_empty());
        assert_eq!(app.mode, Mode::DiffFiles, "a file row has no turn");
        assert!(app.transient().is_some_and(|t| t.contains("select a turn")));
    }

    #[test]
    fn a_restore_confirm_rechecks_the_turn_before_sending() {
        let mut app = App::new();
        attach_with(&mut app, view_with_turns());
        focus_panel(&mut app);
        app.on_key(plain('G'));
        app.on_key(plain('r'));
        // The turn leaves the list under the confirm (dropped past the cap, the
        // pane killed and its id reused) — y must not restore a stale target.
        let mut view = view_with_turns();
        view[0].tabs[0].panes[0].checkpoints.remove(0);
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        assert!(app.on_key(plain('y')).is_empty());
        assert!(app.transient().is_some_and(|t| t.contains("#1 is gone")));
    }

    #[test]
    fn turns_alone_show_the_panel_and_follow_the_agent_past_a_pager() {
        let content = Rect::new(0, 0, 140, 37);
        let mut view = view_with_turns();
        view[0].changed_files.clear();
        view[0].changes = None;
        let mut app = App::new();
        attach_with(&mut app, view.clone());
        assert!(
            app.split_content(content).2.is_some(),
            "a clean workspace still lists the agent's turns"
        );

        // A pager pane split beside the agent takes focus but has no turns:
        // the panel keeps showing the agent's.
        view[0].tabs[0].layout = Some(split(Direction::Horizontal, leaf(1), leaf(2)));
        view[0].tabs[0].panes.push(shell(2));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        app.focused = Some(PaneId(2));
        assert_eq!(app.turns_pane(), Some(PaneId(1)));
        assert!(app.split_content(content).2.is_some());
    }

    #[test]
    fn clicking_a_turn_row_pages_that_turn() {
        let mut app = App::new();
        attach_with(&mut app, view_with_turns());
        app.sync_sizes(Rect::new(0, 0, 140, 40));
        // Panel rows from its top (y 2): header, two files, a spacer, the
        // `turns` header, then the turns newest first — #2 sits at y 8.
        let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), 110, 8);
        match only_request(&out) {
            Request::PaneRun { cmd, .. } => assert!(
                cmd[2].contains("--from 'c1' --to 'c2'"),
                "the click resolves to the drawn turn: {}",
                cmd[2]
            ),
            other => panic!("expected PaneRun, got {other:?}"),
        }
        assert!(
            app.on_mouse(MouseEventKind::Down(MouseButton::Left), 110, 6)
                .is_empty(),
            "the turns header is not a row"
        );
    }

    #[test]
    fn a_restored_reply_names_the_state_it_saved() {
        let mut app = App::new();
        attach_with(&mut app, view_with_turns());
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Restored {
                n: 1,
                saved: Some(4),
            })
            .unwrap(),
        ));
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("restored to #1") && t.contains("#4"))
        );
    }
}
