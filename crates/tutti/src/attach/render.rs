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
use super::sidebar::{AgentRow, SidebarEntry, WorkspaceRow};
use crate::config::{self, Config, PrefixAction};

/// Render the whole UI: the active tab's panes and the status bar.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let status = Rect::new(area.x, area.y + content.height, area.width, 1);
    let (sidebar_rect, panes_area) = app.split_content(content);

    let rects = app.compute_rects(content);
    if rects.is_empty() {
        let hint = Paragraph::new("no panes — run `tutti pane run -- <cmd>` to start one")
            .style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(hint, panes_area);
    } else {
        for (pane, rect) in rects {
            draw_pane(frame, app, pane, rect);
        }
    }

    if let Some(sidebar_rect) = sidebar_rect {
        draw_sidebar(frame, app, sidebar_rect);
    }

    draw_status(frame, app, status);

    if app.whichkey_visible() {
        draw_whichkey(frame, app.config(), panes_area);
    }
    if app.mode == Mode::Help {
        draw_help(frame, app.config(), area);
    }
}

/// The sidebar column: a WORKSPACES section over an AGENTS section, a right
/// border separating it from the panes. The selection bar and the
/// new-workspace prompt appear while the sidebar is focused.
fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().add_modifier(Modifier::DIM));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let sidebar = app.sidebar();
    let focused = app.sidebar_focused();
    let selected = app.sidebar_selected();
    let selected_at = |i: usize| focused && selected == i;

    let mut lines = vec![section_header("WORKSPACES")];
    for (i, entry) in sidebar
        .entries
        .iter()
        .take(sidebar.workspace_count)
        .enumerate()
    {
        if let SidebarEntry::Workspace(w) = entry {
            lines.push(workspace_line(w, selected_at(i)));
            lines.push(sidebar_subtitle(&w.subtitle, selected_at(i)));
        }
    }
    lines.push(Line::from(String::new()));
    lines.push(section_header("AGENTS"));
    for (i, entry) in sidebar
        .entries
        .iter()
        .enumerate()
        .skip(sidebar.workspace_count)
    {
        if let SidebarEntry::Agent(a) = entry {
            lines.push(agent_line(a, selected_at(i), app.is_notified(a.pane)));
            lines.push(sidebar_subtitle(
                &format!("{} · {}", state_label(a.state), a.kind),
                selected_at(i),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if app.sidebar_prompt_active() {
        let row = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        frame.render_widget(Clear, row);
        frame.render_widget(
            Paragraph::new(format!("dir: {}", app.sidebar_prompt()))
                .style(Style::default().add_modifier(Modifier::REVERSED)),
            row,
        );
    }
}

/// A dim section label heading a sidebar group.
fn section_header(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// A workspace entry's title line: bold when it owns the active tab, the whole
/// row reversed when it is the sidebar's selection.
fn workspace_line(w: &WorkspaceRow, selected: bool) -> Line<'static> {
    let mut style = Style::default();
    if w.active {
        style = style.add_modifier(Modifier::BOLD);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::from(Span::styled(format!(" {}", w.name), style))
}

/// An agent entry's title line: a state-coloured dot, the pane title, and a
/// trailing bell mark while the pane has an unseen notification.
fn agent_line(a: &AgentRow, selected: bool, notified: bool) -> Line<'static> {
    let mut dot = state_style(a.state);
    let mut title = Style::default();
    if selected {
        dot = dot.add_modifier(Modifier::REVERSED);
        title = title.add_modifier(Modifier::REVERSED);
    }
    let mut spans = vec![
        Span::styled(" ● ", dot),
        Span::styled(a.title.clone(), title),
    ];
    if notified {
        spans.push(Span::styled(
            " 🔔",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// A dim second line under a sidebar entry (branch, or `state · kind`).
fn sidebar_subtitle(text: &str, selected: bool) -> Line<'static> {
    let mut style = Style::default().add_modifier(Modifier::DIM);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::from(Span::styled(format!("   {text}"), style))
}

fn draw_pane(frame: &mut Frame, app: &App, pane: PaneId, rect: Rect) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let focused = app.focused == Some(pane);
    let state = app.panes.get(&pane);
    let title = state.map_or_else(|| Line::from(pane.to_string()), |s| border_title(&s.info));

    let mut block = Block::default().borders(Borders::ALL).title(title);
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

    // Per-pane state badges, the panes needing attention (Blocked) first.
    let mut infos: Vec<&PaneInfo> = app.panes.values().map(|s| &s.info).collect();
    infos.sort_by_key(|i| (i.state != AgentState::Blocked, i.id.0));
    for info in infos {
        let mut style = state_style(info.state);
        if app.focused == Some(info.id) {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(
            format!(" {}:{} ", identity(info), state_label(info.state)),
            style,
        ));
    }

    // A transient/mode message occupies the status line; otherwise the always-on
    // discoverability hint sits on the right edge.
    let message = status_message(app);
    let show_hint = message.is_empty();
    if !show_hint {
        spans.push(Span::styled(
            format!("  {message}"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    let line = Line::from(spans);
    let left_width = line.width() as u16;
    frame.render_widget(
        Paragraph::new(line).style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
    );

    if show_hint {
        draw_hint(frame, app.config(), area, left_width);
    }
}

/// The compact "how to get out" hint pinned to the status bar's right edge:
/// `<prefix> ? help · <prefix> q detach`, degrading to `<prefix> ?` when space
/// is tight, using the configured prefix and keymap.
fn draw_hint(frame: &mut Frame, cfg: &Config, area: Rect, left_width: u16) {
    let prefix = cfg.prefix.label();
    let help = cfg.prefix_key(PrefixAction::Help).map(config::key_label);
    let detach = cfg.prefix_key(PrefixAction::Detach).map(config::key_label);

    let full = match (&help, &detach) {
        (Some(h), Some(d)) => Some(format!("{prefix} {h} help · {prefix} {d} detach")),
        _ => None,
    };
    let short = help.as_ref().map(|h| format!("{prefix} {h}"));

    let avail = area.width.saturating_sub(left_width + 1);
    let text = match (full, short) {
        (Some(full), _) if full.chars().count() as u16 <= avail => full,
        (_, Some(short)) if short.chars().count() as u16 <= avail => short,
        _ => return,
    };

    let w = text.chars().count() as u16;
    let rect = Rect::new(area.x + area.width - w, area.y, w, 1);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().add_modifier(Modifier::REVERSED | Modifier::DIM)),
        rect,
    );
}

/// The which-key popup: one `key → description` row per active prefix binding
/// (the same table dispatch reads), plus `esc → close`. Pinned bottom-right,
/// dim border.
fn draw_whichkey(frame: &mut Frame, cfg: &Config, area: Rect) {
    let mut lines: Vec<Line> = cfg
        .prefix_bindings()
        .iter()
        .map(|b| Line::from(format!(" {} → {} ", config::key_label(b.key), b.desc)))
        .collect();
    lines.push(Line::from(" esc → close "));

    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_w + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(width);
    let y = area.y + area.height.saturating_sub(height);
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().add_modifier(Modifier::DIM))
                .title("prefix"),
        ),
        popup,
    );
}

fn draw_help(frame: &mut Frame, cfg: &Config, area: Rect) {
    let lines = help_lines(cfg);
    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_w + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("help")),
        popup,
    );
}

/// The help overlay content, generated from the active keymap so it always
/// matches dispatch. Detach is listed first (it is what a stuck user needs).
fn help_lines(cfg: &Config) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("tutti — press {}, then:", cfg.prefix.label())),
        Line::from(String::new()),
    ];
    if let Some(key) = cfg.prefix_key(PrefixAction::Detach) {
        lines.push(Line::from(format!("  {} → detach", config::key_label(key))));
    }
    // The remaining prefix bindings, two per row to stay compact.
    let entries: Vec<String> = cfg
        .prefix_bindings()
        .iter()
        .filter(|b| b.action != PrefixAction::Detach)
        .map(|b| format!("{} → {}", config::key_label(b.key), b.desc))
        .collect();
    for pair in entries.chunks(2) {
        let line = match pair {
            [a, b] => format!("  {a:<22}{b}"),
            [a] => format!("  {a}"),
            _ => String::new(),
        };
        lines.push(Line::from(line));
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from("direct keys (no prefix):".to_string()));
    lines.push(Line::from("  Ctrl+h/j/k/l  focus by direction".to_string()));
    lines.push(Line::from("  Alt+h/j/k/l   resize split".to_string()));
    lines.push(Line::from("  Alt+x         kill pane".to_string()));
    lines.push(Line::from(String::new()));
    lines.push(Line::from(
        "stop the daemon:  tutti server stop".to_string(),
    ));
    lines.push(Line::from(String::new()));
    lines.push(Line::from("(any key closes)".to_string()));
    lines
}

fn status_message(app: &App) -> String {
    match app.mode {
        Mode::Prefix => app.transient().unwrap_or("PREFIX").to_string(),
        Mode::ConfirmKill(_) => app.transient().unwrap_or("confirm kill? (y/n)").to_string(),
        Mode::Scroll(_) => app.transient().unwrap_or("SCROLL (q to exit)").to_string(),
        Mode::Help => "HELP (any key closes)".to_string(),
        Mode::Sidebar => app
            .transient()
            .unwrap_or("SIDEBAR (j/k move · enter jump · n new · esc back)")
            .to_string(),
        Mode::SidebarPrompt => app.transient().unwrap_or("new workspace dir").to_string(),
        Mode::Terminal => app.transient().unwrap_or("").to_string(),
    }
}

/// A pane's display name: its detected agent kind when known, else the process
/// title.
fn identity(info: &PaneInfo) -> String {
    info.agent
        .as_ref()
        .map(|agent| agent.to_string())
        .unwrap_or_else(|| info.title.clone())
}

/// The border badge: `{identity} · {state}` with the state coloured, plus the
/// exit code once the child is gone.
fn border_title(info: &PaneInfo) -> Line<'static> {
    let mut spans = vec![
        Span::raw(identity(info)),
        Span::raw(" · "),
        Span::styled(state_label(info.state), state_style(info.state)),
    ];
    if let Some(code) = info.exited {
        spans.push(Span::raw(format!(" (exited {code})")));
    }
    Line::from(spans)
}

/// Terminal-palette colours for each state — no theming layer. Blocked is the
/// loud one: it is the pane asking for the user.
fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Blocked => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        AgentState::Working => Style::default().fg(Color::Yellow),
        AgentState::Done => Style::default().fg(Color::Green),
        AgentState::Idle | AgentState::Unknown => Style::default().add_modifier(Modifier::DIM),
    }
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
                    branch: None,
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

    fn app_two_workspaces() -> App {
        let mut app = App::new();
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                session: "demo".into(),
                workspaces: vec![
                    WorkspaceView {
                        id: WorkspaceId(1),
                        name: "api".into(),
                        branch: Some("main".into()),
                        tabs: vec![TabView {
                            id: TabId(1),
                            name: "1".into(),
                            active: true,
                            layout: Some(Layout::Leaf(PaneId(1))),
                            active_pane: Some(PaneId(1)),
                            panes: vec![PaneInfo {
                                id: PaneId(1),
                                title: "zsh".into(),
                                agent: None,
                                state: AgentState::Idle,
                                exited: None,
                            }],
                        }],
                    },
                    WorkspaceView {
                        id: WorkspaceId(2),
                        name: "web".into(),
                        branch: None,
                        tabs: vec![TabView {
                            id: TabId(2),
                            name: "2".into(),
                            active: false,
                            layout: Some(Layout::Leaf(PaneId(2))),
                            active_pane: Some(PaneId(2)),
                            panes: vec![PaneInfo {
                                id: PaneId(2),
                                title: "agent".into(),
                                agent: Some("claude".into()),
                                state: AgentState::Blocked,
                                exited: None,
                            }],
                        }],
                    },
                ],
            })
            .unwrap(),
        ));
        app
    }

    #[test]
    fn sidebar_renders_both_sections_with_branch_and_agent() {
        // Two workspaces trigger the auto sidebar; 100 cols clears the floor.
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("WORKSPACES"),
            "workspaces header missing: {text:?}"
        );
        assert!(text.contains("AGENTS"), "agents header missing: {text:?}");
        assert!(text.contains("api"), "workspace name missing: {text:?}");
        assert!(text.contains("main"), "branch subtitle missing: {text:?}");
        assert!(text.contains("claude"), "agent kind missing: {text:?}");
    }

    #[test]
    fn sidebar_shows_a_bell_mark_for_a_notified_agent() {
        let mut app = app_two_workspaces();
        // pane 2 (an agent, not the focused pane) raises a notification.
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&tutti_core::Event::PaneNotification {
                pane: PaneId(2),
                title: None,
                body: Some("done".into()),
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("🔔"), "bell mark missing: {text:?}");
    }

    #[test]
    fn state_style_colors_match_the_spec() {
        assert_eq!(state_style(AgentState::Blocked).fg, Some(Color::Red));
        assert!(
            state_style(AgentState::Blocked)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(state_style(AgentState::Working).fg, Some(Color::Yellow));
        assert_eq!(state_style(AgentState::Done).fg, Some(Color::Green));
        assert!(
            state_style(AgentState::Idle)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            state_style(AgentState::Unknown)
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn border_title_shows_agent_and_state() {
        let info = PaneInfo {
            id: PaneId(1),
            title: "zsh".into(),
            agent: Some("claude".into()),
            state: AgentState::Working,
            exited: None,
        };
        let text: String = border_title(&info)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "claude · working");
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

    #[test]
    fn status_bar_shows_the_detach_hint() {
        let app = app_with_pane(b"hi");
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("C-b"), "hint prefix missing: {text:?}");
        assert!(text.contains("detach"), "hint detach missing: {text:?}");
    }

    #[test]
    fn help_overlay_lists_detach_first_with_direct_keys_and_stop() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_with_pane(b"hi");
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Help);

        let mut terminal = Terminal::new(TestBackend::new(64, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("detach"), "help missing detach: {text:?}");
        assert!(
            text.contains("tutti server stop"),
            "help missing daemon-stop line: {text:?}"
        );
        assert!(
            text.contains("direct keys"),
            "help missing direct keys section: {text:?}"
        );
        // Detach is listed before the split bindings.
        assert!(
            text.find("detach") < text.find("split"),
            "detach should be listed first: {text:?}"
        );
    }
}
