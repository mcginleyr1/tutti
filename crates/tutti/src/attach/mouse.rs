//! The mouse: a press, drag and release handed to a pane's program when it
//! reports the mouse, and otherwise tutti's own — a click that focuses, a
//! drag that selects and copies, a split border dragged, the wheel, and the
//! clicks the modal overlays take.

use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use rtty_vt::input::{self, Button, Code, Key, Mouse};
use tutti_core::{Frame as WireFrame, PaneId, Request};

use super::app::{App, Mode, contains, control, inner_size};
use super::layout::Boundary;

const MOUSE_SCROLL_STEP: usize = 3;

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

/// What the mouse holds between a press and what follows it.
#[derive(Default)]
pub(super) struct Pointer {
    /// The border drag in progress; `None` while no border is held.
    border: Option<BorderDrag>,
    /// The pane a press went to (its program reports the mouse): its drag
    /// and release go there too, wherever the pointer is.
    grab: Option<PaneId>,
    /// The live mouse text selection, set on a left-drag inside a pane and kept
    /// highlighted until the next click. `None` when nothing is selected.
    selection: Option<Selection>,
}

impl App {
    /// A mouse event from the terminal. Inside a pane whose program turned on
    /// mouse reporting, a press, its drag and its release — and bare motion,
    /// when the program asked for all of it — go to that program, a press
    /// focusing the pane too, as tmux and WezTerm do; Shift held keeps the
    /// mouse tutti's (select and copy). The wheel, border drags and the chrome
    /// are tutti's: `on_mouse`. A press ends the hold an earlier one left on
    /// the mouse: its release came with Shift held, or never came.
    pub fn on_mouse_event(&mut self, mouse: MouseEvent) -> Vec<WireFrame> {
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.pointer.grab = None;
        }
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
            MouseEventKind::Drag(_) => self.pointer.grab?,
            MouseEventKind::Up(_) => self.pointer.grab.take()?,
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
                self.pointer.selection = None;
                self.pointer.grab = Some(pane);
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
                self.pointer.selection = None;
                self.pointer.border = self.boundary_at(col, row).map(|boundary| BorderDrag {
                    boundary,
                    grab: boundary.at - boundary.along(col, row),
                    moved: false,
                });
                // A press on the cells where two panes meet grabs their split's
                // border instead of focusing or selecting.
                if self.pointer.border.is_some() {
                    return Vec::new();
                }
                if let Some(pane) = self.pane_at(col, row) {
                    let frames = self.mouse_focus(pane);
                    if let Some((pane, r, c)) = self.pane_cell_at(col, row) {
                        self.pointer.selection = Some(Selection {
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
            MouseEventKind::Drag(MouseButton::Left) if self.pointer.border.is_some() => {
                self.drag_border(col, row)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.extend_selection(col, row);
                Vec::new()
            }
            MouseEventKind::Up(MouseButton::Left) if self.pointer.border.is_some() => {
                let still = self.pointer.border.take().is_some_and(|drag| !drag.moved);
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
        let Some(drag) = self.pointer.border.as_mut() else {
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
        let Some(sel) = self.pointer.selection else {
            return;
        };
        let Some((r, c)) = self.clamp_to_pane(sel.pane, col, row) else {
            return;
        };
        if let Some(sel) = self.pointer.selection.as_mut() {
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
            self.pointer.selection = None;
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
        let sel = self.pointer.selection?;
        (sel.pane == pane && sel.dragged).then(|| ordered(sel.anchor, sel.head))
    }

    /// The currently selected text, read from the pane's visible grid as a
    /// stream selection: the first row from its start column to end-of-line, any
    /// whole rows between, and the last row to its end column, each row's
    /// trailing blanks trimmed. `None` unless a drag is live.
    fn selected_text(&self) -> Option<String> {
        let sel = self.pointer.selection?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::*;
    use crate::config::Config;
    use tutti_core::{Direction, Layout, PaneData};

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
        assert!(app.pointer.selection.is_none() && app.take_terminal_out().is_empty());
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
        assert!(app.pointer.selection.is_none() && app.take_terminal_out().is_empty());

        let mut app = reporting_fixture(b"\x1b[?9h\x1b[?1006h");
        let out = app.on_mouse_event(mouse(Down(MouseButton::Left), 55, 6, none));
        assert_eq!(out, to_two(b"\x1b[<0;5;3M"));
        assert!(
            app.on_mouse_event(mouse(Up(MouseButton::Left), 55, 6, none))
                .is_empty()
        );
        assert!(
            app.pointer.grab.is_none(),
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

    /// A press ends the hold an earlier press left on the mouse — its release
    /// came with Shift held, which keeps the mouse tutti's, or never came:
    /// the drag that follows is this press's own, not the old pane's.
    #[test]
    fn a_press_ends_the_hold_an_unreleased_press_left() {
        use MouseEventKind::{Down, Drag, Up};
        let (none, shift) = (KeyModifiers::NONE, KeyModifiers::SHIFT);
        for released in [true, false] {
            let mut app = reporting_fixture(b"\x1b[?1002h\x1b[?1006h");
            let out = app.on_mouse_event(mouse(Down(MouseButton::Left), 55, 6, none));
            assert_eq!(out, to_two(b"\x1b[<0;5;3M"), "pane 2's program takes it");
            if released {
                app.on_mouse_event(mouse(Up(MouseButton::Left), 55, 6, shift));
            }
            // Pane 1's program reports nothing: a drag there selects.
            app.on_mouse_event(mouse(Down(MouseButton::Left), 5, 6, none));
            let out = app.on_mouse_event(mouse(Drag(MouseButton::Left), 15, 6, none));
            assert_eq!(out, Vec::new(), "pane 2 hears nothing of it");
            assert!(app.selection_span(PaneId(1)).is_some());
        }
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
            app.pointer.selection.is_none(),
            "…nor reaches the pane beneath (a press there starts a selection)"
        );
    }
}
