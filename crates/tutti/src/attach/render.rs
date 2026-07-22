//! Drawing the attach TUI into a ratatui frame: pane borders, the vt100 grid
//! rendered cell-by-cell, the cursor, and the status bar. The vt100→ratatui
//! cell and colour mappings are pulled out as pure functions so they can be
//! unit-tested against a real parser.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tutti_core::{AgentState, PaneId, PaneInfo, SubagentInfo};

use super::app::{App, Mode};
use super::sidebar::{AgentRow, SidebarEntry, WorkspaceRow};
use crate::config::{self, Config, PrefixAction};
use crate::render::state_label;

/// The single accent colour — terminal blue — that marks the focused or active
/// thing. Everything else renders dim so the one accent is unmistakable. State
/// colours (red/yellow/green) appear only on state dots and the blocked border.
const ACCENT: Color = Color::Blue;

/// The sidebar-mode key hint, rendered two-tone in the bottom bar.
const SIDEBAR_HINT: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("enter", "jump"),
    ("n", "new"),
    ("d", "diff"),
    ("esc", "back"),
];

/// The default dim style, applied to nearly everything that is not the accent.
fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Render the whole UI: the pane area under a top tab bar, the sidebar, and the
/// bottom bar.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    let (content, bottom) = App::content_rect(area);
    let (sidebar_rect, tabs_rect, panes_area) = app.regions(content);
    // One spinner frame for the whole draw so every working agent — sidebar dot
    // and pane border alike — animates in lockstep.
    let spinner = app.spinner_char();

    let rects = app.compute_rects(content);
    if rects.is_empty() {
        draw_empty_hint(frame, app, panes_area);
    } else {
        for (pane, rect) in rects {
            draw_pane(frame, app, pane, rect, spinner);
        }
    }

    draw_tab_bar(frame, app, tabs_rect);
    if let Some(sidebar_rect) = sidebar_rect {
        draw_sidebar(frame, app, sidebar_rect, spinner);
    }
    draw_bottom_bar(frame, app, bottom);

    if app.whichkey_visible() {
        draw_whichkey(frame, app.config(), panes_area);
    }
    if app.mode == Mode::Help {
        draw_help(frame, app.config(), area);
    }
}

/// The top tab bar: a chip per tab (active = accent background with dark text,
/// inactive = dim) plus a trailing dim ` + ` new-tab chip. Sits right of the
/// sidebar; the chip widths match `App::tab_chips` so clicks land true.
fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let spans: Vec<Span> = app
        .tab_chips()
        .into_iter()
        .map(|(target, label)| {
            let active = matches!(target, Some(id) if app.active_tab == Some(id));
            Span::styled(label, tab_chip_style(active))
        })
        .collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The style of a tab-bar chip: the active tab is an accent block, every other
/// chip (and the `+`) is dim.
fn tab_chip_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Black).bg(ACCENT)
    } else {
        dim()
    }
}

/// The centred hint shown when the active area holds no panes: how to add the
/// first project on a fresh session (nothing mounted yet), or how to start a
/// pane in an existing but empty tab.
fn draw_empty_hint(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = if app.workspaces.is_empty() {
        "n → add a project"
    } else {
        "no panes — run `tutti pane run -- <cmd>` to start one"
    };
    let row = Rect::new(area.x, area.y + area.height / 2, area.width, 1);
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(dim()),
        row,
    );
}

/// The sidebar column: a blank top-pad row, a lowercase ` workspaces` section
/// over an ` agents` section, each header carrying a right-aligned count, a
/// full-height dim `│` right edge. Dim by default; the selected row (only while
/// the sidebar is focused and not prompting) gets a subtle background and an
/// accent bar. The new-project prompt overwrites the foot row when active.
fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect, spinner: char) {
    let block = Block::default().borders(Borders::RIGHT).border_style(dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let w = inner.width;

    let sidebar = app.sidebar();
    let ws_count = sidebar.workspace_count;
    let agent_count = sidebar.entries.len() - ws_count;
    // A selection only pops while the sidebar holds focus and is not prompting.
    let pop = app.sidebar_focused() && !app.sidebar_prompt_active();
    let selected = app.sidebar_selected();

    let mut lines = vec![Line::from(String::new())]; // top-pad breathing room
    lines.push(section_header("workspaces", ws_count.to_string(), w));
    for (i, entry) in sidebar.entries.iter().take(ws_count).enumerate() {
        if let SidebarEntry::Workspace(row) = entry {
            let sel = pop && selected == i;
            lines.push(workspace_line(row, sel, w));
            lines.push(workspace_subtitle(
                row.subtitle.as_deref().unwrap_or(""),
                row.changes.as_deref(),
                row.stale,
                sel,
                w,
            ));
        }
    }
    lines.push(Line::from(String::new()));
    let agents_meta = if agent_count == 0 {
        "none".to_string()
    } else {
        agent_count.to_string()
    };
    lines.push(section_header("agents", agents_meta, w));
    if agent_count == 0 {
        lines.push(placeholder_line("no agents yet"));
    } else {
        for (i, entry) in sidebar.entries.iter().enumerate().skip(ws_count) {
            if let SidebarEntry::Agent(a) = entry {
                let sel = pop && selected == i;
                lines.push(agent_line(a, sel, spinner, w));
                lines.push(agent_subtitle(a, sel, app.is_notified(a.pane), w));
                for subagent in &a.subagents {
                    lines.push(subagent_line(subagent, spinner, w));
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if app.sidebar_prompt_active() {
        draw_sidebar_prompt(frame, app.sidebar_prompt(), inner);
    }
}

/// A lowercase dim section header with its count right-aligned one space from
/// the `│` edge.
fn section_header(title: &str, meta: String, width: u16) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let left = format!(" {title}");
    let used = left.chars().count() + meta.chars().count() + 1;
    let pad = (width as usize).saturating_sub(used).max(1);
    Line::from(vec![
        Span::styled(left, dim),
        Span::styled(" ".repeat(pad), dim),
        Span::styled(format!("{meta} "), dim),
    ])
}

/// A workspace row: an active `●` (accent) or inactive `○` (dim) dot, then the
/// bold name. Dim unless it is the popped selection.
fn workspace_line(w: &WorkspaceRow, sel: bool, width: u16) -> Line<'static> {
    let (dot, dot_style) = if w.active {
        ('●', Style::default().fg(ACCENT))
    } else {
        ('○', dim())
    };
    let mut name = Style::default().add_modifier(Modifier::BOLD);
    if !sel {
        name = name.add_modifier(Modifier::DIM);
    }
    finish_row(
        vec![
            Span::styled(format!("{dot} "), dot_style),
            Span::styled(w.name.clone(), name),
        ],
        sel,
        width,
    )
}

/// An agent row: a state dot (a spinner while working) then the pane title.
fn agent_line(a: &AgentRow, sel: bool, spinner: char, width: u16) -> Line<'static> {
    let (dot, dot_style) = agent_dot(a.state, spinner);
    let mut title = Style::default();
    if !sel {
        title = title.add_modifier(Modifier::DIM);
    }
    finish_row(
        vec![
            Span::styled(format!("{dot} "), dot_style),
            Span::styled(a.title.clone(), title),
        ],
        sel,
        width,
    )
}

/// The dim `state · kind` second line for an agent, plus the bell mark when a
/// notification is pending.
fn agent_subtitle(a: &AgentRow, sel: bool, notified: bool, width: u16) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("  {} · {}", state_label(a.state), a.kind),
        dim(),
    )];
    if notified {
        spans.push(Span::styled(
            " 🔔",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    finish_row(spans, sel, width)
}

/// A workspace's dim subtitle line: the branch (or dir) on the left, and a
/// right-aligned tag against the sidebar's edge — a dim-red `stale` marker when
/// the working copy is stale, otherwise the dim jj change stat. The stale tag
/// wins over the stat, and either is dropped first when it would collide with
/// the branch, so a narrow sidebar still shows the branch.
fn workspace_subtitle(
    branch: &str,
    changes: Option<&str>,
    stale: bool,
    sel: bool,
    width: u16,
) -> Line<'static> {
    let left = format!("  {branch}");
    // The gutter (drawn by `finish_row`) takes the first column.
    let content = (width as usize).saturating_sub(1);
    let tag: Option<(String, Style)> = if stale {
        Some(("stale".to_string(), dim().fg(Color::Red)))
    } else {
        changes.map(|c| (c.to_string(), dim()))
    };
    if let Some((text, style)) = tag {
        let need = left.chars().count() + 1 + text.chars().count();
        if need <= content {
            let gap = content - left.chars().count() - text.chars().count();
            return finish_row(
                vec![
                    Span::styled(left, dim()),
                    Span::styled(" ".repeat(gap), dim()),
                    Span::styled(text, style),
                ],
                sel,
                width,
            );
        }
    }
    finish_row(vec![Span::styled(left, dim())], sel, width)
}

/// A subagent sub-row under its agent: dim and indented, a shared spinner while
/// running so all live subagents animate in lockstep, a `·` once finished.
/// Display-only — never selectable, so it takes no selection gutter.
fn subagent_line(sub: &SubagentInfo, spinner: char, width: u16) -> Line<'static> {
    let glyph = if sub.running { spinner } else { '·' };
    finish_row(
        vec![Span::styled(format!("  {glyph} {}", sub.desc), dim())],
        false,
        width,
    )
}

/// A dim placeholder line so an empty section never looks broken.
fn placeholder_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {text}"), dim()))
}

/// The state dot for an agent: a spinner glyph while working (so multiple
/// working agents animate in lockstep), a solid coloured `●` for
/// blocked/done, a dim `○` when idle/unknown.
fn agent_dot(state: AgentState, spinner: char) -> (char, Style) {
    let dot = match state {
        AgentState::Working => spinner,
        AgentState::Blocked | AgentState::Done => '●',
        AgentState::Idle | AgentState::Unknown => '○',
    };
    (dot, state_style(state))
}

/// Turn a row's content spans into a full line: a leading gutter (`▍` accent
/// when selected, else a space), and — when selected — a subtle full-width
/// background so the selection reads as one bar even with a single entry.
fn finish_row(mut content: Vec<Span<'static>>, sel: bool, width: u16) -> Line<'static> {
    let bg = Color::DarkGray;
    let gutter = if sel {
        Span::styled("▍", Style::default().fg(ACCENT).bg(bg))
    } else {
        Span::raw(" ")
    };
    if sel {
        for span in &mut content {
            span.style = span.style.bg(bg);
        }
    }
    let used: usize = 1 + content
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>();
    let mut spans = vec![gutter];
    spans.extend(content);
    if sel && used < width as usize {
        spans.push(Span::styled(
            " ".repeat(width as usize - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(spans)
}

/// The new-project prompt on the sidebar's foot row: an accent bar, the typed
/// path, and a visible block cursor.
fn draw_sidebar_prompt(frame: &mut Frame, text: &str, inner: Rect) {
    let row = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    frame.render_widget(Clear, row);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("▍", Style::default().fg(ACCENT)),
            Span::styled(text.to_string(), Style::default()),
            Span::styled("█", Style::default().fg(ACCENT)),
        ])),
        row,
    );
}

fn draw_pane(frame: &mut Frame, app: &App, pane: PaneId, rect: Rect, spinner: char) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let focused = app.focused == Some(pane);
    let state = app.panes.get(&pane);
    let pane_state = state.map(|s| s.info.state);
    let title = state.map_or_else(
        || Line::from(pane.to_string()),
        |s| border_title(&s.info, spinner),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(pane_border_style(focused, pane_state))
        .title(title);
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

/// The decluttered bottom bar: a dim inverse session chip on the left, then a
/// transient/mode message (the sidebar mode shows a two-tone hint). With
/// nothing to say, the standing help/detach hint sits on the right. The tab
/// list and per-pane state chips are gone — the top bar and sidebar own those.
fn draw_bottom_bar(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut spans = vec![Span::styled(
        format!(" {} ", app.session),
        Style::default().add_modifier(Modifier::REVERSED | Modifier::DIM),
    )];

    let show_hint = match (app.mode, app.transient()) {
        (Mode::Terminal, None) => true,
        (Mode::Sidebar, None) => {
            spans.push(Span::raw("  "));
            spans.extend(two_tone(SIDEBAR_HINT).0);
            false
        }
        _ => {
            spans.push(Span::styled(format!("  {}", status_message(app)), dim));
            false
        }
    };

    let line = Line::from(spans);
    let left_width = line.width() as u16;
    frame.render_widget(Paragraph::new(line), area);

    if show_hint {
        draw_hint(frame, app.config(), area, left_width);
    }
}

/// The compact "how to get out" hint pinned to the bottom bar's right edge,
/// two-tone (key bright, label dim): `<prefix> ? help · <prefix> q detach`,
/// degrading to just help when space is tight.
fn draw_hint(frame: &mut Frame, cfg: &Config, area: Rect, left_width: u16) {
    let prefix = cfg.prefix.label();
    let help = cfg.prefix_key(PrefixAction::Help).map(config::key_label);
    let detach = cfg.prefix_key(PrefixAction::Detach).map(config::key_label);

    let mut pairs: Vec<(String, &str)> = Vec::new();
    if let Some(h) = &help {
        pairs.push((format!("{prefix} {h}"), "help"));
    }
    if let Some(d) = &detach {
        pairs.push((format!("{prefix} {d}"), "detach"));
    }
    if pairs.is_empty() {
        return;
    }

    let avail = area.width.saturating_sub(left_width + 1);
    let (spans, w) = fit_hint(&pairs, avail);
    if spans.is_empty() {
        return;
    }
    let rect = Rect::new(area.x + area.width - w, area.y, w, 1);
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// The widest two-tone hint that fits `avail`: the full pair list, else just the
/// first pair, else nothing.
fn fit_hint(pairs: &[(String, &str)], avail: u16) -> (Vec<Span<'static>>, u16) {
    let (spans, w) = two_tone(pairs);
    if w <= avail {
        return (spans, w);
    }
    let (spans, w) = two_tone(&pairs[..1]);
    if w <= avail {
        (spans, w)
    } else {
        (Vec::new(), 0)
    }
}

/// Two-tone key hints: each key bright, its label dim, joined by a dim `·`.
/// Returns the spans and their total rendered width.
fn two_tone<S: AsRef<str>>(pairs: &[(S, &str)]) -> (Vec<Span<'static>>, u16) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut spans = Vec::new();
    let mut width = 0usize;
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim));
            width += 3;
        }
        let key = key.as_ref().to_string();
        width += key.chars().count() + 1 + label.chars().count();
        spans.push(Span::raw(key));
        spans.push(Span::styled(format!(" {label}"), dim));
    }
    (spans, width as u16)
}

/// The which-key popup: one `key → description` row per active prefix binding
/// (the same table dispatch reads), plus `esc → close`. Pinned bottom-right,
/// dim border.
fn draw_whichkey(frame: &mut Frame, cfg: &Config, area: Rect) {
    let mut lines: Vec<Line> = cfg
        .prefix_bindings()
        .iter()
        .map(|b| two_tone_line(&config::key_label(b.key), b.desc))
        .collect();
    lines.push(two_tone_line("esc", "close"));

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
                .border_style(dim())
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
/// Every key hint is two-tone: the key bright, its label dim.
fn help_lines(cfg: &Config) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = vec![
        Line::from(format!("tutti — press {}, then:", cfg.prefix.label())),
        Line::from(String::new()),
    ];
    if let Some(key) = cfg.prefix_key(PrefixAction::Detach) {
        lines.push(two_tone_line(&config::key_label(key), "detach"));
    }
    // The remaining prefix bindings, two per row to stay compact.
    let entries: Vec<(String, &str)> = cfg
        .prefix_bindings()
        .iter()
        .filter(|b| b.action != PrefixAction::Detach)
        .map(|b| (config::key_label(b.key), b.desc))
        .collect();
    for pair in entries.chunks(2) {
        let mut spans = vec![Span::raw(" ")];
        for (i, (key, desc)) in pair.iter().enumerate() {
            spans.push(Span::raw(format!(" {key}")));
            let label = if i == 0 && pair.len() == 2 {
                format!(" {desc:<14}")
            } else {
                format!(" {desc}")
            };
            spans.push(Span::styled(label, dim));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from("direct keys (no prefix):".to_string()));
    lines.push(two_tone_line("  Ctrl+h/j/k/l", "focus by direction"));
    lines.push(two_tone_line("  Alt+h/j/k/l", "resize split"));
    lines.push(two_tone_line("  Alt+x", "kill pane"));
    lines.push(Line::from(String::new()));
    lines.push(Line::from(
        "stop the daemon:  tutti server stop".to_string(),
    ));
    lines.push(Line::from(String::new()));
    lines.push(Line::from("(any key closes)".to_string()));
    lines
}

/// One two-tone hint line: the key bright, its label dim.
fn two_tone_line(key: &str, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!(" {key}")),
        Span::styled(format!(" {label} "), dim()),
    ])
}

fn status_message(app: &App) -> String {
    match app.mode {
        Mode::Prefix => app.transient().unwrap_or("PREFIX").to_string(),
        Mode::ConfirmKill(_) => app.transient().unwrap_or("confirm kill? (y/n)").to_string(),
        Mode::Scroll(_) => app.transient().unwrap_or("SCROLL (q to exit)").to_string(),
        Mode::Help => "HELP (any key closes)".to_string(),
        Mode::Sidebar => app.transient().unwrap_or_default().to_string(),
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

/// The border badge. The name is dim-bold; a detected agent adds a dim `·`
/// then the state in its colour (a spinner precedes `working`); a plain shell
/// shows just its name. An exited pane appends a dim `exited <code>`.
fn border_title(info: &PaneInfo, spinner: char) -> Line<'static> {
    let mut spans = vec![Span::styled(
        identity(info),
        Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
    )];
    if info.agent.is_some() {
        spans.push(Span::styled(" · ", dim()));
        let label = if info.state == AgentState::Working {
            format!("{spinner} working")
        } else {
            state_label(info.state).to_string()
        };
        spans.push(Span::styled(label, state_style(info.state)));
    }
    if let Some(code) = info.exited {
        spans.push(Span::styled(format!(" exited {code}"), dim()));
    }
    Line::from(spans)
}

/// A pane's border style: accent (bold) when focused; red when an unfocused
/// pane's agent is blocked (the one place a state colour touches a border);
/// dim otherwise.
fn pane_border_style(focused: bool, state: Option<AgentState>) -> Style {
    if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else if state == Some(AgentState::Blocked) {
        Style::default().fg(Color::Red)
    } else {
        dim()
    }
}

/// Terminal-palette colours for each state — no theming layer. Blocked is the
/// loud one: it is the pane asking for the user.
fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Blocked => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        AgentState::Working => Style::default().fg(Color::Yellow),
        AgentState::Done => Style::default().fg(Color::Green),
        AgentState::Idle | AgentState::Unknown => dim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::{leaf, pane, tab, workspace};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tutti_core::{Frame as WireFrame, PaneData, Response};

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
                workspaces: vec![workspace(
                    1,
                    "w",
                    None,
                    vec![tab(
                        1,
                        "main",
                        true,
                        leaf(1),
                        vec![pane(1, "shell", None, AgentState::Idle)],
                    )],
                )],
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
                    workspace(
                        1,
                        "api",
                        Some("main"),
                        vec![tab(
                            1,
                            "1",
                            true,
                            leaf(1),
                            vec![pane(1, "zsh", None, AgentState::Idle)],
                        )],
                    ),
                    workspace(
                        2,
                        "web",
                        None,
                        vec![tab(
                            2,
                            "2",
                            false,
                            leaf(2),
                            vec![pane(2, "agent", Some("claude"), AgentState::Blocked)],
                        )],
                    ),
                ],
            })
            .unwrap(),
        ));
        app
    }

    #[test]
    fn sidebar_renders_both_sections_with_branch_and_agent() {
        // 100 cols clears the width floor so the sidebar shows.
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("workspaces"),
            "workspaces header missing: {text:?}"
        );
        assert!(text.contains("agents"), "agents header missing: {text:?}");
        assert!(text.contains("api"), "workspace name missing: {text:?}");
        assert!(text.contains("main"), "branch subtitle missing: {text:?}");
        assert!(text.contains("claude"), "agent kind missing: {text:?}");
    }

    #[test]
    fn sidebar_renders_the_change_stat_beside_the_branch() {
        let mut app = App::new();
        let mut view = vec![workspace(
            1,
            "api",
            Some("main"),
            vec![tab(
                1,
                "1",
                true,
                leaf(1),
                vec![pane(1, "zsh", None, AgentState::Idle)],
            )],
        )];
        view[0].changes = Some("4 files +120 −33".into());
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                session: "demo".into(),
                workspaces: view,
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("main"), "branch still shows: {text:?}");
        assert!(
            text.contains("4 files +120 −33"),
            "change stat missing from the subtitle line: {text:?}"
        );
    }

    #[test]
    fn sidebar_shows_a_dim_red_stale_tag_for_a_stale_workspace() {
        let mut app = App::new();
        let mut view = vec![workspace(
            1,
            "api",
            Some("main"),
            vec![tab(
                1,
                "1",
                true,
                leaf(1),
                vec![pane(1, "zsh", None, AgentState::Idle)],
            )],
        )];
        // A stale workspace whose change stat is suppressed (jj reports staleness
        // instead of a diff) — the stale tag takes the stat's place.
        view[0].stale = true;
        view[0].changes = None;
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                session: "demo".into(),
                workspaces: view,
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("stale"), "stale tag missing: {text:?}");
        // The tag renders in red (the one place a workspace subtitle takes colour).
        let stale_start = buf
            .content()
            .windows(5)
            .position(|w| w.iter().map(|c| c.symbol()).collect::<String>() == "stale")
            .expect("the stale glyphs are contiguous");
        assert_eq!(
            buf.content()[stale_start].fg,
            Color::Red,
            "the stale tag should be dim-red"
        );
    }

    #[test]
    fn sidebar_shows_a_placeholder_when_no_agents() {
        // A single agentless workspace, sidebar forced on.
        let mut app = App::with_config(Config::parse("sidebar = \"on\"\n").unwrap());
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                session: "demo".into(),
                workspaces: vec![workspace(
                    1,
                    "solo",
                    None,
                    vec![tab(
                        1,
                        "1",
                        true,
                        leaf(1),
                        vec![pane(1, "zsh", None, AgentState::Idle)],
                    )],
                )],
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("no agents yet"),
            "empty agents section needs a placeholder: {text:?}"
        );
    }

    #[test]
    fn sidebar_renders_subagent_rows_under_a_hook_driven_agent() {
        use crate::attach::fixtures::{agent, leaf, sub};
        let mut app = App::with_config(Config::parse("sidebar = \"on\"\n").unwrap());
        let mut agent_pane = agent(1, "claude", AgentState::Working);
        agent_pane.subagents = vec![sub("build the core", true), sub("write the tests", false)];
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                session: "demo".into(),
                workspaces: vec![workspace(
                    1,
                    "api",
                    Some("main"),
                    vec![tab(1, "1", true, leaf(1), vec![agent_pane])],
                )],
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("build the core"),
            "a running subagent sub-row is missing: {text:?}"
        );
        assert!(
            text.contains("write the tests"),
            "a finished subagent sub-row is missing: {text:?}"
        );
    }

    fn accent_bar_row(buf: &Buffer) -> Option<u16> {
        let w = buf.area.width;
        buf.content()
            .iter()
            .position(|c| c.symbol() == "▍")
            .map(|i| i as u16 / w)
    }

    #[test]
    fn focused_sidebar_shows_a_moving_accent_selection() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        // Establish the content width so the sidebar key may focus.
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 100, 20));

        // Prefix then `w` focuses the sidebar.
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(app.sidebar_focused(), "w focuses the sidebar");
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let before = accent_bar_row(terminal.backend().buffer());
        assert!(before.is_some(), "the selection accent bar is drawn");

        // `j` moves the visible selection to a new row.
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let after = accent_bar_row(terminal.backend().buffer());
        assert!(after.is_some());
        assert_ne!(before, after, "j slides the accent bar down a row");
    }

    #[test]
    fn top_tab_bar_lists_a_new_tab_chip() {
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("+"), "new-tab chip missing: {text:?}");
    }

    #[test]
    fn bottom_bar_drops_the_tab_list_and_pane_badges() {
        let (w, h) = (100usize, 12usize);
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(w as u16, h as u16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let content = terminal.backend().buffer().content();
        let bottom: String = content[(h - 1) * w..h * w]
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            bottom.contains("demo"),
            "session name belongs in the bottom bar: {bottom:?}"
        );
        assert!(
            !bottom.contains("blocked"),
            "no per-pane state chips in the bottom bar: {bottom:?}"
        );
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

    fn title_text(info: &PaneInfo) -> String {
        // A fixed spinner frame keeps the assertion deterministic.
        border_title(info, '⠋')
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn border_title_shows_agent_and_state() {
        let info = PaneInfo {
            id: PaneId(1),
            title: "zsh".into(),
            agent: Some("claude".into()),
            state: AgentState::Blocked,
            exited: None,
            subagents: Vec::new(),
        };
        assert_eq!(title_text(&info), "claude · blocked");
    }

    #[test]
    fn border_title_animates_a_working_agent_with_the_spinner() {
        let info = PaneInfo {
            id: PaneId(1),
            title: "zsh".into(),
            agent: Some("claude".into()),
            state: AgentState::Working,
            exited: None,
            subagents: Vec::new(),
        };
        assert_eq!(title_text(&info), "claude · ⠋ working");
    }

    #[test]
    fn border_title_omits_state_for_a_plain_shell() {
        let info = PaneInfo {
            id: PaneId(1),
            title: "zsh".into(),
            agent: None,
            state: AgentState::Unknown,
            exited: None,
            subagents: Vec::new(),
        };
        assert_eq!(title_text(&info), "zsh", "no `· unknown` suffix");
    }

    #[test]
    fn border_title_shows_exit_marker_for_an_exited_shell() {
        let info = PaneInfo {
            id: PaneId(1),
            title: "zsh".into(),
            agent: None,
            state: AgentState::Done,
            exited: Some(0),
            subagents: Vec::new(),
        };
        assert_eq!(title_text(&info), "zsh exited 0");
    }

    #[test]
    fn pane_border_style_reflects_focus_and_blocked_attention() {
        assert_eq!(
            pane_border_style(true, Some(AgentState::Idle)).fg,
            Some(ACCENT),
            "focused pane border is the accent"
        );
        assert_eq!(
            pane_border_style(false, Some(AgentState::Blocked)).fg,
            Some(Color::Red),
            "an unfocused blocked pane border turns red"
        );
        assert_eq!(
            pane_border_style(true, Some(AgentState::Blocked)).fg,
            Some(ACCENT),
            "focus wins over the blocked red"
        );
        assert!(
            pane_border_style(false, Some(AgentState::Idle))
                .add_modifier
                .contains(Modifier::DIM),
            "an ordinary unfocused pane border is dim"
        );
    }

    #[test]
    fn agent_dot_spins_only_while_working() {
        assert_eq!(
            agent_dot(AgentState::Working, '⠹').0,
            '⠹',
            "working shows the current spinner frame"
        );
        assert_eq!(agent_dot(AgentState::Blocked, '⠹').0, '●');
        assert_eq!(agent_dot(AgentState::Done, '⠹').0, '●');
        assert_eq!(agent_dot(AgentState::Idle, '⠹').0, '○');
    }

    #[test]
    fn active_tab_chip_uses_the_accent_background() {
        assert_eq!(tab_chip_style(true).bg, Some(ACCENT));
        assert_eq!(tab_chip_style(true).fg, Some(Color::Black));
        assert!(
            tab_chip_style(false).add_modifier.contains(Modifier::DIM),
            "inactive chips are dim with no background"
        );
        assert_eq!(tab_chip_style(false).bg, None);
    }

    #[test]
    fn empty_session_renders_add_project_hint() {
        let app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(60, 4)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("add a project"),
            "expected the fresh-session add-project hint: {text:?}"
        );
    }

    #[test]
    fn two_tone_renders_bright_keys_and_dim_labels() {
        let line = two_tone_line("j/k", "move");
        assert_eq!(
            line.spans[0].style,
            Style::default(),
            "key span stays bright"
        );
        assert_eq!(line.spans[1].style, dim(), "label span is dim");
        let (spans, _) = two_tone(&[("a", "one"), ("b", "two")]);
        assert!(
            spans
                .iter()
                .any(|s| s.content == " · " && s.style.add_modifier.contains(Modifier::DIM)),
            "separator between pairs is dim"
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
