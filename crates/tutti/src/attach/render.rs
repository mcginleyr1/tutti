//! Drawing the attach TUI into a ratatui frame: pane borders, the vt100 grid
//! rendered cell-by-cell, the cursor, and the status bar. The vt100→ratatui
//! cell and colour mappings are pulled out as pure functions so they can be
//! unit-tested against a real parser.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tutti_core::{AgentState, PaneId, PaneInfo};

use super::app::{App, Mode};

const HELP: &[&str] = &[
    "tutti — prefix Ctrl+B, then:",
    "",
    "  %   split right        \"   split down",
    "  x   kill pane          z   zoom focused pane",
    "  o   focus next pane    ←↑↓→  focus by direction",
    "  n/p next / prev tab    c   new tab",
    "  [   scrollback mode    d/q detach",
    "  ?   this help",
    "",
    "  (any key closes this help)",
];

/// Render the whole UI: the active tab's panes and the status bar.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let status = Rect::new(area.x, area.y + content.height, area.width, 1);

    let rects = app.compute_rects(content);
    if rects.is_empty() {
        let hint = Paragraph::new("no panes — run `tutti pane run -- <cmd>` to start one")
            .style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(hint, content);
    } else {
        for (pane, rect) in rects {
            draw_pane(frame, app, pane, rect);
        }
    }

    draw_status(frame, app, status);

    if app.mode == Mode::Help {
        draw_help(frame, area);
    }
}

fn draw_pane(frame: &mut Frame, app: &App, pane: PaneId, rect: Rect) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let focused = app.focused == Some(pane);
    let state = app.panes.get(&pane);
    let title = state.map_or_else(|| pane.to_string(), |s| pane_title(&s.info));

    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(Span::raw(title));
    if focused {
        block = block
            .border_type(BorderType::Thick)
            .border_style(Style::default().fg(Color::Cyan));
    } else {
        block = block.border_style(Style::default().add_modifier(Modifier::DIM));
    }
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if let Some(state) = state {
        let screen = state.parser.screen();
        draw_screen(frame.buffer_mut(), inner, screen);
        if focused && state.scroll.is_none() && !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            if col < inner.width && row < inner.height {
                frame.set_cursor_position((inner.x + col, inner.y + row));
            }
        }
    }
}

/// Blit a vt100 screen into `area`, one cell at a time, mapping colours and
/// attributes. Wide-character continuation cells are cleared so a glyph is not
/// drawn twice.
fn draw_screen(buf: &mut Buffer, area: Rect, screen: &vt100::Screen) {
    let (rows, cols) = screen.size();
    let height = area.height.min(rows);
    let width = area.width.min(cols);
    for row in 0..height {
        for col in 0..width {
            let Some(buf_cell) = buf.cell_mut((area.x + col, area.y + row)) else {
                continue;
            };
            match screen.cell(row, col) {
                Some(cell) if cell.is_wide_continuation() => {
                    buf_cell.set_symbol("");
                }
                Some(cell) => {
                    if cell.has_contents() {
                        buf_cell.set_symbol(cell.contents());
                    } else {
                        buf_cell.set_char(' ');
                    }
                    buf_cell.set_style(cell_style(cell));
                }
                None => {
                    buf_cell.set_char(' ');
                }
            }
        }
    }
}

/// The ratatui style for a vt100 cell: colours passed straight through, no
/// theming.
pub fn cell_style(cell: &vt100::Cell) -> Style {
    let mut modifier = Modifier::empty();
    if cell.bold() {
        modifier |= Modifier::BOLD;
    }
    if cell.dim() {
        modifier |= Modifier::DIM;
    }
    if cell.italic() {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifier |= Modifier::REVERSED;
    }
    Style::default()
        .fg(map_color(cell.fgcolor()))
        .bg(map_color(cell.bgcolor()))
        .add_modifier(modifier)
}

/// Map a vt100 colour to the equivalent ratatui colour.
pub fn map_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(
        format!(" {} ", app.session),
        Style::default().add_modifier(Modifier::BOLD),
    )];

    for tab in app.all_tabs() {
        let active = app.active_tab == Some(tab.id);
        let style = if active {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(format!(" {} ", tab.name), style));
    }

    if let Some(pane) = app.focused
        && let Some(state) = app.panes.get(&pane)
    {
        spans.push(Span::raw(format!(
            "  {} [{}] ",
            pane_title(&state.info),
            state_label(state.info.state)
        )));
    }

    let message = status_message(app);
    if !message.is_empty() {
        spans.push(Span::styled(
            format!("  {message}"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 46u16.min(area.width);
    let height = (HELP.len() as u16 + 2).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    let lines: Vec<Line> = HELP.iter().map(|l| Line::from(*l)).collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("help")),
        popup,
    );
}

fn status_message(app: &App) -> String {
    match app.mode {
        Mode::Prefix => app.transient().unwrap_or("PREFIX").to_string(),
        Mode::ConfirmKill(_) => app.transient().unwrap_or("confirm kill? (y/n)").to_string(),
        Mode::Scroll(_) => app.transient().unwrap_or("SCROLL (q to exit)").to_string(),
        Mode::Help => "HELP (any key closes)".to_string(),
        Mode::Terminal => app.transient().unwrap_or("").to_string(),
    }
}

fn pane_title(info: &PaneInfo) -> String {
    let mut title = info.title.clone();
    if let Some(agent) = &info.agent {
        title = format!("{title} · {agent}");
    }
    if let Some(code) = info.exited {
        title = format!("{title} (exited {code})");
    }
    title
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Unknown => "unknown",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Done => "done",
        AgentState::Idle => "idle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tutti_core::{Frame as WireFrame, Layout, PaneData, Response, TabView, WorkspaceView};
    use tutti_core::{TabId, WorkspaceId};

    #[test]
    fn maps_default_indexed_and_rgb_colors() {
        assert_eq!(map_color(vt100::Color::Default), Color::Reset);
        assert_eq!(map_color(vt100::Color::Idx(9)), Color::Indexed(9));
        assert_eq!(map_color(vt100::Color::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn cell_style_reads_bold_red_from_a_real_parser() {
        let mut parser = vt100::Parser::new(1, 10, 0);
        parser.process(b"\x1b[1;31mX");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).unwrap();
        let style = cell_style(cell);
        assert_eq!(style.fg, Some(Color::Indexed(1)));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn cell_style_maps_underline_and_reverse() {
        let mut parser = vt100::Parser::new(1, 10, 0);
        parser.process(b"\x1b[4;7mY");
        let screen = parser.screen();
        let style = cell_style(screen.cell(0, 0).unwrap());
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }

    fn app_with_pane(text: &[u8]) -> App {
        let mut app = App::new();
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                session: "demo".into(),
                workspaces: vec![WorkspaceView {
                    id: WorkspaceId(1),
                    name: "w".into(),
                    tabs: vec![TabView {
                        id: TabId(1),
                        name: "main".into(),
                        active: true,
                        layout: Some(Layout::Leaf(PaneId(1))),
                        active_pane: Some(PaneId(1)),
                        panes: vec![PaneInfo {
                            id: PaneId(1),
                            title: "shell".into(),
                            agent: None,
                            state: AgentState::Idle,
                            exited: None,
                        }],
                    }],
                }],
            })
            .unwrap(),
        ));
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(1),
            rows: 6,
            cols: 18,
            seq: 0,
            bytes: text.to_vec(),
        }));
        app
    }

    fn buffer_text(buf: &Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn draws_pane_grid_and_status_into_buffer() {
        let app = app_with_pane(b"HELLO");
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("HELLO"), "grid text missing: {text:?}");
        assert!(text.contains("demo"), "session name missing from status");
        assert!(text.contains("main"), "tab name missing from status");
    }

    #[test]
    fn empty_session_renders_hint() {
        let app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(60, 4)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("no panes"),
            "expected empty-session hint: {text:?}"
        );
    }
}
