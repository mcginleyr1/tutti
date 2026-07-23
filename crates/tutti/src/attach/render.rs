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
use super::launcher::LauncherRow;
use super::sidebar::{AgentRow, SidebarEntry, WorkspaceRow};
use crate::config::{self, Config, Icons, PrefixAction};
use crate::render::state_label;

/// The single accent colour — terminal blue — that marks the focused or active
/// thing. Everything else renders dim so the one accent is unmistakable. State
/// colours (red/yellow/green) appear only on state dots and the blocked border.
const ACCENT: Color = Color::Blue;

/// The neutral chrome background shades, drawn only on truecolor terminals (see
/// `App::chrome_shaded`). `CHROME_BAR` shades the bars (app bar, rule, footer,
/// floating panels); `CHROME_PANEL` is the slightly deeper sidebar shade. Both
/// are kept quiet so the accent stays the loud thing.
const CHROME_BAR: Color = Color::Rgb(30, 34, 42);
const CHROME_PANEL: Color = Color::Rgb(22, 26, 34);

/// The subtle full-row background under the selected sidebar entry.
const SELECT_BG: Color = Color::DarkGray;

/// A resolved glyph set: the concrete characters for the sidebar's dots and
/// markers. Tree guides and the braille spinner are box-drawing/shared, so they
/// are plain consts, not part of this table.
pub struct Glyphs {
    /// The active / inactive workspace dot.
    pub ws_active: char,
    pub ws_inactive: char,
    /// The blocked / done / idle agent state dots (working shows the spinner).
    pub blocked: char,
    pub done: char,
    pub idle: char,
    /// The branch marker before a workspace's ref, and the fork marker before a
    /// stale (forked) workspace's tag.
    pub branch: char,
    pub fork: char,
}

/// The safe set every terminal renders — the current default look.
const UNICODE_GLYPHS: Glyphs = Glyphs {
    ws_active: '●',
    ws_inactive: '○',
    blocked: '●',
    done: '●',
    idle: '○',
    branch: '⎇',
    fork: '⑂',
};

/// Private-use icons for a patched Nerd Font (folder, circles, powerline branch,
/// code-fork). Only legible with such a font installed — opt-in via config.
const NERDFONT_GLYPHS: Glyphs = Glyphs {
    ws_active: '\u{f07c}',
    ws_inactive: '\u{f07b}',
    blocked: '\u{f057}',
    done: '\u{f058}',
    idle: '\u{f111}',
    branch: '\u{e0a0}',
    fork: '\u{f126}',
};

/// The glyph table for the configured icon set.
fn glyphs(icons: Icons) -> &'static Glyphs {
    match icons {
        Icons::Unicode => &UNICODE_GLYPHS,
        Icons::Nerdfont => &NERDFONT_GLYPHS,
    }
}

/// The chrome bar shade when the terminal and config allow it, else `None`.
fn bar_bg(app: &App) -> Option<Color> {
    app.chrome_shaded().then_some(CHROME_BAR)
}

/// The deeper sidebar-panel shade when allowed, else `None`.
fn panel_bg(app: &App) -> Option<Color> {
    app.chrome_shaded().then_some(CHROME_PANEL)
}

/// Flood `area` with `bg` when a shade is active; a no-op otherwise so a
/// non-truecolor terminal keeps its own background.
fn paint_bg(frame: &mut Frame, area: Rect, bg: Option<Color>) {
    if let Some(bg) = bg {
        frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    }
}

/// The sidebar-mode key hint, rendered two-tone in the bottom bar.
const SIDEBAR_HINT: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("enter", "jump"),
    ("n", "add project"),
    ("d", "diff"),
    ("f", "fork"),
    ("u", "update"),
    ("x", "kill"),
    ("esc", "back"),
];

/// The default dim style, applied to nearly everything that is not the accent.
fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Render the whole UI: the top app bar and its rule, the sidebar and pane area
/// beneath, and the footer — plus the transient notification band and any
/// floating panel.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    let (content, footer) = App::content_rect(area);
    let (app_bar, rule) = App::header_rects(area);
    let (sidebar_rect, panes_area) = app.regions(content);
    // One spinner frame for the whole draw so every working agent — sidebar dot
    // and pane border alike — animates in lockstep.
    let spinner = app.spinner_char();

    let rects = app.compute_rects(content);
    if rects.is_empty() {
        draw_dashboard(frame, app, panes_area);
    } else {
        for (pane, rect) in rects {
            draw_pane(frame, app, pane, rect, spinner);
        }
    }

    draw_app_bar(frame, app, app_bar);
    draw_rule(frame, app, rule);
    if let Some(sidebar_rect) = sidebar_rect {
        draw_sidebar(frame, app, sidebar_rect, spinner);
    }
    draw_footer(frame, app, footer);
    // A transient fires the one-row notification band over the last content row,
    // just above the footer.
    if app.transient().is_some() && footer.y > content.y {
        let band = Rect::new(footer.x, footer.y - 1, footer.width, 1);
        draw_notification(frame, app, band);
    }

    if app.whichkey_visible() {
        draw_whichkey(frame, app, panes_area);
    }
    if app.mode == Mode::Help {
        draw_help(frame, app, area);
    }
    if app.mode == Mode::Launcher {
        draw_launcher(frame, app, area);
    }
    if app.mode == Mode::LauncherCommand {
        draw_launcher_command(frame, app, area);
    }
}

/// The top app bar (full width): the accent bar and bold `tutti — <session>`
/// wordmark on the left, the tab segments right-aligned. Takes the chrome shade.
fn draw_app_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    paint_bg(frame, area, bar_bg(app));
    let title = if app.session.is_empty() {
        "tutti".to_string()
    } else {
        format!("tutti — {}", app.session)
    };
    let left = Line::from(vec![
        Span::styled("▍", Style::default().fg(ACCENT)),
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(left), area);
    draw_tab_bar(frame, app, app.tab_bar_rect(area));
}

/// The full-width dim rule under the app bar.
fn draw_rule(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    paint_bg(frame, area, bar_bg(app));
    let rule = "─".repeat(area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(rule, dim()))), area);
}

/// The app-bar tab segments: one `[<n> <name>]` per tab (active = accent block,
/// inactive = normal text, the trailing `[+]` dim), joined by a one-column
/// separator. Segment widths match `App::tab_chips` so clicks land true.
fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut spans: Vec<Span> = Vec::new();
    for (i, (target, label)) in app.tab_chips().into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let active = matches!(target, Some(id) if app.active_tab == Some(id));
        spans.push(Span::styled(
            label,
            tab_chip_style(active, target.is_none()),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The style of a tab segment: the active tab is an accent block, an inactive
/// tab is normal text on the chrome background, and the trailing `[+]` is dim.
fn tab_chip_style(active: bool, is_new: bool) -> Style {
    if active {
        Style::default().fg(Color::Black).bg(ACCENT)
    } else if is_new {
        dim()
    } else {
        Style::default()
    }
}

/// The centred mini-dashboard shown when the active area holds no panes: a small
/// `TUTTI` wordmark over a two-tone action list (how to add the first project on
/// a fresh session, or split/help in an existing but empty tab).
fn draw_dashboard(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let prefix = app.config().prefix.label();
    let actions: Vec<(String, &str)> = if app.workspaces.is_empty() {
        vec![
            ("n".to_string(), "add a project"),
            (format!("{prefix} %"), "split"),
            (format!("{prefix} ?"), "help"),
        ]
    } else {
        vec![
            (format!("{prefix} %"), "split"),
            (format!("{prefix} c"), "new tab"),
            (format!("{prefix} ?"), "help"),
        ]
    };
    let action_lines: Vec<Line> = actions
        .iter()
        .map(|(key, label)| dashboard_action(key, label))
        .collect();

    // Drop the wordmark when the block would not fit vertically.
    let wordmark = wordmark_lines();
    let mut lines: Vec<Line> = Vec::new();
    if area.height as usize >= wordmark.len() + 1 + action_lines.len() {
        lines.extend(wordmark);
        lines.push(Line::from(String::new()));
    }
    lines.extend(action_lines);

    let block_h = (lines.len() as u16).min(area.height);
    let y = area.y + area.height.saturating_sub(block_h) / 2;
    let rect = Rect::new(area.x, y, area.width, block_h);
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}

/// One two-tone dashboard action row: the key in the accent colour, its label
/// italic-dim.
fn dashboard_action(key: &str, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key}  "), Style::default().fg(ACCENT)),
        Span::styled(label.to_string(), dim().add_modifier(Modifier::ITALIC)),
    ])
}

/// The small block-letter `TUTTI` wordmark (five rows), rendered in the accent.
fn wordmark_lines() -> Vec<Line<'static>> {
    const ROWS: [&str; 5] = [
        "███ █ █ ███ ███ ███",
        " █  █ █  █   █   █ ",
        " █  █ █  █   █   █ ",
        " █  █ █  █   █   █ ",
        " █  ███  █   █  ███",
    ];
    ROWS.iter()
        .map(|r| Line::from(Span::styled((*r).to_string(), Style::default().fg(ACCENT))))
        .collect()
}

/// The sidebar: one rounded frame whose top border carries the `projects`
/// header and whose fused divider carries the `agents` header, each with a
/// collapse arrow and a count. Entries sit inside with one column of padding; a
/// selected entry (only while the sidebar holds focus and is not prompting) gets
/// a subtle full-row highlight and its name as an accent chip. Takes the deeper
/// chrome panel shade. The new-project prompt overlays the foot when active.
fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect, spinner: char) {
    if area.width < 3 || area.height < 2 {
        return;
    }
    paint_bg(frame, area, panel_bg(app));
    let w = area.width;
    let content_w = w.saturating_sub(4); // two borders + one column of padding each side

    let sidebar = app.sidebar();
    let ws_count = sidebar.workspace_count;
    let agent_count = sidebar.entries.len() - ws_count;
    // A selection only pops while the sidebar holds focus and is not prompting.
    let pop = app.sidebar_focused() && !app.sidebar_prompt_active();
    let selected = app.sidebar_selected();
    let glyphs = glyphs(app.config().icons);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(header_border(
        '╭',
        '╮',
        sidebar.projects_collapsed,
        "projects",
        ws_count,
        w,
    ));
    if !sidebar.projects_collapsed {
        for (i, entry) in sidebar.entries.iter().take(ws_count).enumerate() {
            if let SidebarEntry::Workspace(row) = entry {
                let sel = pop && selected == i;
                lines.push(frame_row(workspace_inner(row, sel, content_w, glyphs), sel));
                lines.push(frame_row(
                    workspace_subtitle_inner(row, sel, content_w, glyphs),
                    sel,
                ));
            }
        }
    }
    lines.push(header_border(
        '├',
        '┤',
        sidebar.agents_collapsed,
        "agents",
        agent_count,
        w,
    ));
    if !sidebar.agents_collapsed {
        if agent_count == 0 {
            lines.push(frame_row(
                placeholder_inner("no agents yet", content_w),
                false,
            ));
        } else {
            for (i, entry) in sidebar.entries.iter().enumerate().skip(ws_count) {
                if let SidebarEntry::Agent(a) = entry {
                    let sel = pop && selected == i;
                    lines.push(frame_row(
                        agent_inner(a, sel, spinner, content_w, glyphs),
                        sel,
                    ));
                    lines.push(frame_row(
                        agent_subtitle_inner(a, sel, app.is_notified(a.pane), content_w),
                        sel,
                    ));
                    let last = a.subagents.len();
                    for (j, subagent) in a.subagents.iter().enumerate() {
                        lines.push(frame_row(
                            subagent_inner(subagent, spinner, j + 1 == last, content_w),
                            false,
                        ));
                    }
                }
            }
        }
    }
    // Fill to the foot, then close the frame with the bottom border.
    let h = area.height as usize;
    while lines.len() + 1 < h {
        lines.push(frame_blank(w));
    }
    if lines.len() < h {
        lines.push(bottom_border(w));
    }
    frame.render_widget(Paragraph::new(lines), area);

    if app.sidebar_prompt_active() {
        // The prompt overlays the frame's inner content column (inside the
        // borders and padding), leaving the frame edges intact. The fork prompt
        // reuses the same field with an empty listing and its own label.
        let inner = Rect::new(
            area.x + 2,
            area.y + 1,
            content_w,
            area.height.saturating_sub(2),
        );
        let (label, completions, selected) = if app.sidebar_fork_prompt_active() {
            ("fork as: ", &[][..], 0)
        } else {
            ("open: ", app.prompt_completions(), app.prompt_selected())
        };
        draw_sidebar_prompt(
            frame,
            label,
            app.sidebar_prompt(),
            completions,
            selected,
            inner,
        );
    }
}

/// A frame header line — the top border (`╭ ▼ projects ── N ╮`) or the fused
/// divider (`├ ▼ agents ── N ┤`) — with a collapse arrow, the section title, and
/// a right-aligned count, filled to `width` with dashes. All dim but the title.
fn header_border(
    left: char,
    right: char,
    collapsed: bool,
    title: &str,
    count: usize,
    width: u16,
) -> Line<'static> {
    let arrow = if collapsed { '▶' } else { '▼' };
    let count = count.to_string();
    let title_w = title.chars().count();
    // left ' ' arrow ' ' title ' ' <dashes> ' ' count ' ' right
    let fixed = 8 + title_w + count.chars().count();
    let dashes = (width as usize).saturating_sub(fixed);
    Line::from(vec![
        Span::styled(format!("{left} {arrow} "), dim()),
        Span::styled(title.to_string(), Style::default()),
        Span::styled(format!(" {} ", "─".repeat(dashes)), dim()),
        Span::styled(count, dim()),
        Span::styled(format!(" {right}"), dim()),
    ])
}

/// The frame's bottom border (`╰────╯`).
fn bottom_border(width: u16) -> Line<'static> {
    let mid = "─".repeat((width as usize).saturating_sub(2));
    Line::from(Span::styled(format!("╰{mid}╯"), dim()))
}

/// A blank interior row — the left and right frame edges over empty space.
fn frame_blank(width: u16) -> Line<'static> {
    Line::from(vec![
        Span::styled("│", dim()),
        Span::raw(" ".repeat((width as usize).saturating_sub(2))),
        Span::styled("│", dim()),
    ])
}

/// Wrap an interior content row (already padded to the content width) in the
/// frame's `│ … │` edges with one column of padding, shading the padding when
/// the row is selected so the highlight reads as one bar between the borders.
fn frame_row(inner: Vec<Span<'static>>, sel: bool) -> Line<'static> {
    let pad = if sel {
        Style::default().bg(SELECT_BG)
    } else {
        Style::default()
    };
    let mut spans = vec![Span::styled("│", dim()), Span::styled(" ", pad)];
    spans.extend(inner);
    spans.push(Span::styled(" ", pad));
    spans.push(Span::styled("│", dim()));
    Line::from(spans)
}

/// Pad content spans to exactly `content_w` columns, applying the selection
/// background to every span that does not already carry one (so an accent name
/// chip keeps its own background).
fn pad_inner(mut spans: Vec<Span<'static>>, sel: bool, content_w: u16) -> Vec<Span<'static>> {
    if sel {
        for span in &mut spans {
            if span.style.bg.is_none() {
                span.style = span.style.bg(SELECT_BG);
            }
        }
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if used < content_w as usize {
        let fill = " ".repeat(content_w as usize - used);
        let style = if sel {
            Style::default().bg(SELECT_BG)
        } else {
            Style::default()
        };
        spans.push(Span::styled(fill, style));
    }
    spans
}

/// A name span: the accent chip (black on accent) when selected, else `base`
/// modifiers plus dim.
fn name_span(text: &str, sel: bool, base: Modifier) -> Span<'static> {
    if sel {
        Span::styled(
            text.to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(base),
        )
    } else {
        Span::styled(
            text.to_string(),
            Style::default().add_modifier(base | Modifier::DIM),
        )
    }
}

/// A workspace row: an active/inactive dot then the bold name (an accent chip
/// when selected).
fn workspace_inner(
    w: &WorkspaceRow,
    sel: bool,
    content_w: u16,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let (dot, dot_style) = if w.active {
        (glyphs.ws_active, Style::default().fg(ACCENT))
    } else {
        (glyphs.ws_inactive, dim())
    };
    pad_inner(
        vec![
            Span::styled(format!("{dot} "), dot_style),
            name_span(&w.name, sel, Modifier::BOLD),
        ],
        sel,
        content_w,
    )
}

/// A workspace's dim subtitle: the branch marker and ref on the left, and a
/// right-aligned tag — a dim-red fork `stale` marker when the working copy is
/// stale, otherwise the dim jj change stat. The stale tag wins over the stat,
/// and either is dropped when it would collide with the ref.
fn workspace_subtitle_inner(
    w: &WorkspaceRow,
    sel: bool,
    content_w: u16,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let left = format!(
        "  {} {}",
        glyphs.branch,
        w.subtitle.as_deref().unwrap_or("")
    );
    if w.stale {
        return right_tagged(
            left,
            format!("{} stale", glyphs.fork),
            dim().fg(Color::Red),
            sel,
            content_w,
        );
    }
    if let Some(changes) = w.changes.as_deref() {
        return right_tagged(left, changes.to_string(), dim(), sel, content_w);
    }
    pad_inner(vec![Span::styled(left, dim())], sel, content_w)
}

/// Lay out a subtitle's left text with a right-aligned tag inside `content_w`,
/// falling back to just the left text (padded) when the tag will not fit.
fn right_tagged(
    left: String,
    tag: String,
    tag_style: Style,
    sel: bool,
    content_w: u16,
) -> Vec<Span<'static>> {
    let cw = content_w as usize;
    let lw = left.chars().count();
    let tw = tag.chars().count();
    if lw + 1 + tw <= cw {
        let gap = cw - lw - tw;
        let mut spans = vec![
            Span::styled(left, dim()),
            Span::raw(" ".repeat(gap)),
            Span::styled(tag, tag_style),
        ];
        if sel {
            for span in &mut spans {
                if span.style.bg.is_none() {
                    span.style = span.style.bg(SELECT_BG);
                }
            }
        }
        return spans;
    }
    pad_inner(vec![Span::styled(left, dim())], sel, content_w)
}

/// An agent row: a state dot (a spinner while working) then the pane title (an
/// accent chip when selected).
fn agent_inner(
    a: &AgentRow,
    sel: bool,
    spinner: char,
    content_w: u16,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let (dot, dot_style) = agent_dot(a.state, spinner, glyphs);
    pad_inner(
        vec![
            Span::styled(format!("{dot} "), dot_style),
            name_span(&a.title, sel, Modifier::empty()),
        ],
        sel,
        content_w,
    )
}

/// The dim `state · kind` second line for an agent, plus the bell mark when a
/// notification is pending.
fn agent_subtitle_inner(
    a: &AgentRow,
    sel: bool,
    notified: bool,
    content_w: u16,
) -> Vec<Span<'static>> {
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
    pad_inner(spans, sel, content_w)
}

/// A subagent sub-row under its agent: a box-drawing tree guide (`├`/`└`), a
/// shared spinner while running or a `·` once finished, then the description.
/// Display-only — never selectable.
fn subagent_inner(
    sub: &SubagentInfo,
    spinner: char,
    last: bool,
    content_w: u16,
) -> Vec<Span<'static>> {
    let guide = if last { '└' } else { '├' };
    let glyph = if sub.running { spinner } else { '·' };
    pad_inner(
        vec![Span::styled(
            format!("  {guide} {glyph} {}", sub.desc),
            dim(),
        )],
        false,
        content_w,
    )
}

/// A dim italic placeholder so an empty section never looks broken.
fn placeholder_inner(text: &str, content_w: u16) -> Vec<Span<'static>> {
    pad_inner(
        vec![Span::styled(
            format!("  {text}"),
            dim().add_modifier(Modifier::ITALIC),
        )],
        false,
        content_w,
    )
}

/// The state dot for an agent: a spinner glyph while working (so multiple
/// working agents animate in lockstep), the configured blocked/done/idle glyph
/// otherwise, each in its state colour.
fn agent_dot(state: AgentState, spinner: char, glyphs: &Glyphs) -> (char, Style) {
    let dot = match state {
        AgentState::Working => spinner,
        AgentState::Blocked => glyphs.blocked,
        AgentState::Done => glyphs.done,
        AgentState::Idle | AgentState::Unknown => glyphs.idle,
    };
    (dot, state_style(state))
}

/// A sidebar foot prompt — an accent bar, the `label` prefix, the typed text,
/// and a block cursor — with any completions stacked dim directly above it, the
/// highlighted row in the accent. Add-project passes `open:` and its directory
/// completions; fork passes `fork as:` and an empty listing.
fn draw_sidebar_prompt(
    frame: &mut Frame,
    label: &str,
    text: &str,
    completions: &[String],
    selected: usize,
    inner: Rect,
) {
    let foot = inner.y + inner.height.saturating_sub(1);
    // The completions occupy the rows just above the input, newest listing on
    // top; already capped at 8 by `complete_dirs`, clamped here to what fits.
    let rows = (completions.len() as u16).min(inner.height.saturating_sub(1));
    for i in 0..rows {
        let style = if i as usize == selected {
            Style::default().fg(ACCENT)
        } else {
            dim()
        };
        let row = Rect::new(inner.x, foot - rows + i, inner.width, 1);
        frame.render_widget(Clear, row);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {}", completions[i as usize]),
                style,
            ))),
            row,
        );
    }

    let row = Rect::new(inner.x, foot, inner.width, 1);
    frame.render_widget(Clear, row);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("▍", Style::default().fg(ACCENT)),
            Span::styled(label.to_string(), dim()),
            Span::styled(text.to_string(), Style::default()),
            Span::styled("█", Style::default().fg(ACCENT)),
        ])),
        row,
    );
}

fn draw_pane(frame: &mut Frame, app: &App, pane: PaneId, rect: Rect, spinner: char) {
    if rect.width < 2 || rect.height < 3 {
        return;
    }
    let focused = app.focused == Some(pane);
    let state = app.panes.get(&pane);
    let pane_state = state.map(|s| s.info.state);

    // The pane rect reserves its top row for the title line, which sits above
    // the rounded frame; the frame fills the rows below it.
    let title_row = Rect::new(rect.x, rect.y, rect.width, 1);
    let title = state.map_or_else(
        || pane_title_line(&placeholder_title(pane), focused, spinner),
        |s| pane_title_line(&s.info, focused, spinner),
    );
    frame.render_widget(Paragraph::new(title), title_row);

    let frame_rect = Rect::new(rect.x, rect.y + 1, rect.width, rect.height - 1);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(pane_border_style(focused, pane_state));
    let inner = block.inner(frame_rect);
    frame.render_widget(block, frame_rect);

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

/// The footer bar (full width, chrome shade): a mode chip on the left in the
/// accent block when not in terminal mode — plus the two-tone sidebar hint in
/// sidebar mode — and the standing help/detach hint pinned to the right. The tab
/// list and per-pane state chips are gone: the app bar and sidebar own those,
/// and transients fire the notification band above.
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    paint_bg(frame, area, bar_bg(app));
    let mut spans: Vec<Span> = Vec::new();
    if let Some(label) = mode_label(app.mode) {
        spans.push(Span::styled(
            format!(" {label} "),
            Style::default().fg(Color::Black).bg(ACCENT),
        ));
    }
    if app.mode == Mode::Sidebar {
        spans.push(Span::raw("  "));
        spans.extend(two_tone(SIDEBAR_HINT).0);
    }
    let line = Line::from(spans);
    let left_width = line.width() as u16;
    if left_width > 0 {
        frame.render_widget(Paragraph::new(line), area);
    }
    draw_hint(frame, app.config(), area, left_width);
}

/// The mode chip label for the footer's left segment — `None` in terminal mode
/// (no chip), otherwise the current mode's short name.
fn mode_label(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Terminal => None,
        Mode::Prefix => Some("PREFIX"),
        Mode::ConfirmKill(_) | Mode::ConfirmKillWorkspace(_) => Some("CONFIRM"),
        Mode::Scroll(_) => Some("SCROLL"),
        Mode::Help => Some("HELP"),
        Mode::Sidebar => Some("SIDEBAR"),
        Mode::SidebarPrompt => Some("ADD PROJECT"),
        Mode::SidebarForkPrompt => Some("FORK"),
        Mode::Launcher | Mode::LauncherCommand => Some("RUN"),
    }
}

/// The one-row notification band drawn above the footer while a transient is
/// live: a coloured strip — red for an error, accent otherwise — with the
/// message centred in black. Auto-clears with the transient.
fn draw_notification(frame: &mut Frame, app: &App, area: Rect) {
    let Some(msg) = app.transient() else {
        return;
    };
    let bg = if msg.contains("error") {
        Color::Red
    } else {
        ACCENT
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            msg.to_string(),
            Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(bg))
        .alignment(Alignment::Center),
        area,
    );
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

/// The which-key popup: one `key description` row per active prefix binding (the
/// same table dispatch reads), plus `esc close`. Pinned bottom-right in a
/// rounded, chrome-shaded panel titled ` keys `, key letters in the accent.
fn draw_whichkey(frame: &mut Frame, app: &App, area: Rect) {
    let cfg = app.config();
    let mut lines: Vec<Line> = cfg
        .prefix_bindings()
        .iter()
        .map(|b| popup_key_line(&config::key_label(b.key), b.desc))
        .collect();
    lines.push(popup_key_line("esc", "close"));

    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_w + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(width);
    let y = area.y + area.height.saturating_sub(height);
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(popup_block(app, " keys ")),
        popup,
    );
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let lines = help_lines(app.config());
    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_w + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(popup_block(app, " help ")),
        popup,
    );
}

/// The agent launcher overlay: a centred rounded panel titled ` run ` listing
/// what can start in a pane — the registry agents (dim-italic and
/// `(not installed)` when their binary is absent), then the shell and command
/// rows. The selected row takes an accent `❯` marker and accent name; each row
/// shows its quick-select number.
fn draw_launcher(frame: &mut Frame, app: &App, area: Rect) {
    let selected = app.launcher_selected();
    let mut lines: Vec<Line> = app
        .launcher_rows()
        .iter()
        .enumerate()
        .map(|(i, row)| launcher_line(i, row, i == selected))
        .collect();
    lines.push(popup_key_line("esc", "cancel"));
    draw_centered_popup(frame, app, area, " run ", lines);
}

/// One launcher row: `<n>  <name>   <role>`, an accent `❯` marker and accent
/// name when selected. An unavailable agent row is dim-italic and tagged
/// ` (not installed)`.
fn launcher_line(idx: usize, row: &LauncherRow, selected: bool) -> Line<'static> {
    let num = Span::styled(format!(" {} ", idx + 1), dim());
    if !row.available {
        return Line::from(vec![
            num,
            Span::styled(
                format!("  {}   {}  (not installed)", row.name, row.role),
                dim().add_modifier(Modifier::ITALIC),
            ),
        ]);
    }
    let marker = if selected {
        Span::styled(
            "❯ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };
    let name_style = if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Line::from(vec![
        num,
        marker,
        Span::styled(row.name.clone(), name_style),
        Span::styled(format!("   {}", row.role), dim()),
    ])
}

/// The launcher's free-form command input: a centred rounded panel titled
/// ` run ` with a single `▍run: <text>█` line, mirroring the sidebar prompt's
/// accent-bar input.
fn draw_launcher_command(frame: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::styled("▍", Style::default().fg(ACCENT)),
        Span::styled("run: ", dim()),
        Span::styled(app.launcher_command().to_string(), Style::default()),
        Span::styled("█", Style::default().fg(ACCENT)),
    ]);
    // A minimum width so a short command still reads as an input field.
    let width = (line.width() as u16 + 2).max(24).min(area.width);
    let height = 3u16.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(line).block(popup_block(app, " run ")), popup);
}

/// Render `lines` in a rounded, chrome-shaded panel titled `title`, centred over
/// `area`. Shared by the launcher picker with the help/which-key panel look.
fn draw_centered_popup(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &'static str,
    lines: Vec<Line>,
) {
    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_w + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(popup_block(app, title)), popup);
}

/// A floating panel's block: a rounded dim border carrying `title`, with the
/// chrome shade when the terminal allows it.
fn popup_block(app: &App, title: &'static str) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(title);
    if let Some(bg) = bar_bg(app) {
        block = block.style(Style::default().bg(bg));
    }
    block
}

/// The help overlay content, generated from the active keymap so it always
/// matches dispatch. Detach is listed first (it is what a stuck user needs).
/// Every key hint renders its key in the accent, its label dim.
fn help_lines(cfg: &Config) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = vec![
        Line::from(format!("tutti — press {}, then:", cfg.prefix.label())),
        Line::from(String::new()),
    ];
    if let Some(key) = cfg.prefix_key(PrefixAction::Detach) {
        lines.push(popup_key_line(&config::key_label(key), "detach"));
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
            spans.push(Span::styled(format!(" {key}"), Style::default().fg(ACCENT)));
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
    lines.push(popup_key_line("  Ctrl+h/j/k/l", "focus by direction"));
    lines.push(popup_key_line("  Alt+h/j/k/l", "resize split"));
    lines.push(popup_key_line("  Alt+x", "kill pane"));
    lines.push(Line::from(String::new()));
    lines.push(Line::from("sidebar (after C-b w):".to_string()));
    lines.push(popup_key_line("  n / d", "add project / diff"));
    lines.push(popup_key_line("  f / u", "fork / update stale"));
    lines.push(popup_key_line("  x", "kill workspace"));
    lines.push(Line::from(String::new()));
    lines.push(Line::from(
        "stop the daemon:  tutti server stop".to_string(),
    ));
    lines.push(Line::from(String::new()));
    lines.push(Line::from("(any key closes)".to_string()));
    lines
}

/// One popup hint line: the key in the accent colour, its label dim. Used by the
/// which-key and help panels.
fn popup_key_line(key: &str, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key}"), Style::default().fg(ACCENT)),
        Span::styled(format!(" {label} "), dim()),
    ])
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

/// The pane title line drawn above the frame: an accent `❯` marker when focused
/// (else two dim spaces), then the `border_title` badge — dimmed whole when the
/// pane is unfocused so only the focused pane's title reads loud.
fn pane_title_line(info: &PaneInfo, focused: bool, spinner: char) -> Line<'static> {
    let marker = if focused {
        Span::styled(
            "❯ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("  ", dim())
    };
    let mut content = border_title(info, spinner);
    if !focused {
        for span in &mut content.spans {
            span.style = dim();
        }
    }
    let mut spans = vec![marker];
    spans.extend(content.spans);
    Line::from(spans)
}

/// A stand-in `PaneInfo` for a pane whose snapshot has not yet arrived, so its
/// title line reads as a plain unnamed shell.
fn placeholder_title(pane: PaneId) -> PaneInfo {
    PaneInfo {
        id: pane,
        title: pane.to_string(),
        agent: None,
        state: AgentState::Unknown,
        exited: None,
        subagents: Vec::new(),
    }
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
                wire_rev: tutti_core::WIRE_REV,
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
        // 40 cols so the app bar fits both the wordmark and the tab segment
        // without them colliding at the narrow end.
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("HELLO"), "grid text missing: {text:?}");
        assert!(
            text.contains("demo"),
            "session name missing from the app bar"
        );
        assert!(text.contains("main"), "tab name missing from the app bar");
    }

    fn app_two_workspaces() -> App {
        let mut app = App::new();
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
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
            text.contains("projects"),
            "projects header missing: {text:?}"
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
                wire_rev: tutti_core::WIRE_REV,
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
                wire_rev: tutti_core::WIRE_REV,
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
                wire_rev: tutti_core::WIRE_REV,
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
                wire_rev: tutti_core::WIRE_REV,
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

    /// The row of the selection's accent chip — the first cell with an accent
    /// background inside the sidebar column (below the app-bar header, left of
    /// the pane area). The app-bar tab block also uses an accent background, so
    /// the search is scoped past it.
    fn accent_chip_row(buf: &Buffer) -> Option<u16> {
        let w = buf.area.width;
        buf.content().iter().enumerate().find_map(|(i, c)| {
            let col = i as u16 % w;
            let row = i as u16 / w;
            (c.bg == ACCENT && col < 30 && row >= 2).then_some(row)
        })
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
        let before = accent_chip_row(terminal.backend().buffer());
        assert!(before.is_some(), "the selection accent chip is drawn");

        // `j` moves the visible selection to a new row.
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let after = accent_chip_row(terminal.backend().buffer());
        assert!(after.is_some());
        assert_ne!(before, after, "j slides the accent chip down a row");
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
    fn footer_drops_the_session_tab_list_and_pane_badges() {
        let (w, h) = (100usize, 12usize);
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(w as u16, h as u16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let content = terminal.backend().buffer().content();
        let bottom: String = content[(h - 1) * w..h * w]
            .iter()
            .map(|c| c.symbol())
            .collect();
        let top: String = content[0..w].iter().map(|c| c.symbol()).collect();
        assert!(
            top.contains("demo"),
            "the session name moves to the app bar: {top:?}"
        );
        assert!(
            !bottom.contains("demo"),
            "and leaves the footer: {bottom:?}"
        );
        assert!(
            !bottom.contains("blocked"),
            "no per-pane state chips in the footer: {bottom:?}"
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
        let g = &UNICODE_GLYPHS;
        assert_eq!(
            agent_dot(AgentState::Working, '⠹', g).0,
            '⠹',
            "working shows the current spinner frame"
        );
        assert_eq!(agent_dot(AgentState::Blocked, '⠹', g).0, '●');
        assert_eq!(agent_dot(AgentState::Done, '⠹', g).0, '●');
        assert_eq!(agent_dot(AgentState::Idle, '⠹', g).0, '○');
    }

    #[test]
    fn tab_segment_marks_active_and_dims_new() {
        // The active tab is an accent block; an inactive tab is normal text on
        // the chrome background; the trailing `[+]` segment is dim.
        assert_eq!(tab_chip_style(true, false).bg, Some(ACCENT));
        assert_eq!(tab_chip_style(true, false).fg, Some(Color::Black));
        assert_eq!(tab_chip_style(false, false), Style::default());
        assert!(
            tab_chip_style(false, true)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(tab_chip_style(false, true).bg, None);
    }

    #[test]
    fn icons_config_selects_the_glyph_table() {
        // The default unicode set uses the safe dots; nerdfont swaps in
        // private-use icons, so the two tables differ.
        assert_eq!(glyphs(Icons::Unicode).ws_active, '●');
        assert_eq!(glyphs(Icons::Unicode).ws_inactive, '○');
        assert_ne!(glyphs(Icons::Nerdfont).ws_active, '●');
        assert_ne!(
            glyphs(Icons::Nerdfont).branch,
            glyphs(Icons::Unicode).branch
        );
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
    fn popup_key_line_uses_accent_keys_and_dim_labels() {
        // The floating panels (which-key, help) render key letters in the
        // accent, labels dim.
        let line = popup_key_line("j/k", "move");
        assert_eq!(line.spans[0].style.fg, Some(ACCENT), "key span is accent");
        assert_eq!(line.spans[1].style, dim(), "label span is dim");
        // The footer hint keeps bright keys with a dim separator.
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

        // Tall enough to clear the whole panel, including the sidebar section.
        let mut terminal = Terminal::new(TestBackend::new(64, 30)).unwrap();
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
        // The sidebar section documents the fork, update-stale, and kill keys.
        assert!(
            text.contains("fork")
                && text.contains("update stale")
                && text.contains("kill workspace"),
            "help missing the sidebar fork/update/kill keys: {text:?}"
        );
        // Detach is listed before the split bindings.
        assert!(
            text.find("detach") < text.find("split"),
            "detach should be listed first: {text:?}"
        );
    }

    #[test]
    fn sidebar_fork_prompt_shows_the_fork_as_label() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 100, 20));
        // Focus the sidebar, then open the fork prompt on the first workspace.
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("fork as:"),
            "the fork prompt label is missing: {text:?}"
        );
    }

    #[test]
    fn sidebar_prompt_stacks_completions_above_the_open_input() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("tutti-render-complete-{}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();

        let mut app = App::new();
        // An absolute prefix keeps the listing independent of HOME/cwd.
        app.start_first_run_prompt(format!("{}/a", root.display()));

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();

        assert!(text.contains("alpha"), "completion row missing: {text:?}");
        assert!(text.contains("open:"), "prompt prefix missing: {text:?}");
        // The completion sits on a row strictly above the input line.
        let alpha_row = text.find("alpha").unwrap() / w;
        let open_row = text.find("open:").unwrap() / w;
        assert!(
            alpha_row < open_row,
            "completions should stack above the prompt: alpha@{alpha_row} open@{open_row}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The buffer text of one row (`row`), for asserting against a bar.
    fn row_text(buf: &Buffer, row: usize) -> String {
        let w = buf.area.width as usize;
        buf.content()[row * w..(row + 1) * w]
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn app_bar_shows_session_and_the_active_tab_segment() {
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        // The wordmark carries the session on the app-bar row (row 0).
        assert!(
            row_text(buf, 0).contains("demo"),
            "session missing from the app bar: {:?}",
            row_text(buf, 0)
        );
        // The active tab's segment is an accent block on that same row.
        let w = buf.area.width as usize;
        let active = buf.content()[0..w].iter().any(|c| c.bg == ACCENT);
        assert!(active, "the active tab segment should be an accent block");
    }

    #[test]
    fn sidebar_frame_headers_carry_their_counts() {
        // api (no agent) + web (one blocked claude): two projects, one agent.
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        // The projects header is the sidebar's top border, on content row 2 —
        // scoped to the 30-column sidebar (the pane area sits to its right).
        let top: String = row_text(buf, 2).chars().take(30).collect();
        assert!(top.contains("projects"), "projects header: {top:?}");
        assert!(
            top.trim_end().ends_with("2 ╮"),
            "projects count in border: {top:?}"
        );
        // The agents divider carries its own count somewhere below.
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("agents"), "agents header: {text:?}");
        assert!(
            text.contains("1 ┤"),
            "agents count fused in the divider: {text:?}"
        );
    }

    #[test]
    fn focused_pane_title_renders_above_the_rounded_frame() {
        let app = app_with_pane(b"HELLO");
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        // Content starts at row 2: the pane title line, then the frame's top
        // border on row 3.
        let title = row_text(buf, 2);
        assert!(
            title.contains('❯'),
            "focused pane marker missing: {title:?}"
        );
        assert!(title.contains("shell"), "pane title missing: {title:?}");
        assert!(
            row_text(buf, 3).contains('╭'),
            "the rounded frame should open on the row below the title"
        );
    }

    #[test]
    fn footer_shows_the_mode_chip_in_sidebar_mode() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 100, 16));
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let footer = row_text(buf, 15);
        assert!(footer.contains("SIDEBAR"), "mode chip missing: {footer:?}");
        // The chip is an accent block.
        let w = buf.area.width as usize;
        assert!(
            buf.content()[15 * w..16 * w].iter().any(|c| c.bg == ACCENT),
            "the mode chip should be an accent block"
        );
    }

    #[test]
    fn notification_band_colours_errors_red_and_info_accent() {
        let mut app = app_with_pane(b"hi");
        app.note("error: boom".into());
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        // The band overlays the last content row (footer.y - 1 = row 8).
        let w = buf.area.width as usize;
        assert!(
            buf.content()[8 * w..9 * w]
                .iter()
                .all(|c| c.bg == Color::Red),
            "an error transient paints the band red"
        );

        let mut app = app_with_pane(b"hi");
        app.note("saved the file".into());
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(
            buf.content()[8 * w..9 * w].iter().all(|c| c.bg == ACCENT),
            "an info transient paints the band with the accent"
        );
    }

    #[test]
    fn chrome_shade_paints_bars_only_on_truecolor() {
        // Truecolor on + the default config → the footer takes the bar shade.
        let mut app = app_two_workspaces();
        app.set_truecolor(true);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let w = 100usize;
        let shaded = terminal.backend().buffer().content()[11 * w..12 * w]
            .iter()
            .any(|c| c.bg == CHROME_BAR);
        assert!(
            shaded,
            "the footer should take the chrome shade on truecolor"
        );
    }

    #[test]
    fn chrome_shade_absent_without_truecolor_or_when_disabled() {
        // No truecolor → no shade anywhere.
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            !terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|c| c.bg == CHROME_BAR || c.bg == CHROME_PANEL),
            "no chrome shade without a truecolor terminal"
        );

        // Truecolor but chrome_background = false → still no shade.
        let mut app = App::with_config(Config::parse("chrome_background = false\n").unwrap());
        app.set_truecolor(true);
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "demo".into(),
                workspaces: vec![workspace(
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
                )],
            })
            .unwrap(),
        ));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            !terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|c| c.bg == CHROME_BAR || c.bg == CHROME_PANEL),
            "chrome_background = false suppresses the shade even on truecolor"
        );
    }

    #[test]
    fn subagent_guides_connect_under_an_agent() {
        use crate::attach::fixtures::{agent, leaf, sub};
        let mut app = App::with_config(Config::parse("sidebar = \"on\"\n").unwrap());
        let mut agent_pane = agent(1, "claude", AgentState::Working);
        agent_pane.subagents = vec![sub("build the core", true), sub("write the tests", false)];
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
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
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains('├'),
            "a mid tree guide connects the first subagent"
        );
        assert!(
            text.contains('└'),
            "an end tree guide closes the last subagent"
        );
    }

    #[test]
    fn empty_dashboard_renders_the_wordmark_and_actions() {
        // A tall enough empty area shows the TUTTI wordmark over the actions.
        let app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains('█'), "the block-letter wordmark is drawn");
        assert!(
            text.contains("add a project"),
            "the action list offers the first project: {text:?}"
        );
    }

    #[test]
    fn launcher_overlay_lists_the_run_choices() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_with_pane(b"hi");
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Launcher);

        let mut terminal = Terminal::new(TestBackend::new(64, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("run"), "the panel title is missing: {text:?}");
        assert!(text.contains("claude"), "an agent row is missing: {text:?}");
        assert!(text.contains("shell"), "the shell row is missing: {text:?}");
        assert!(
            text.contains("command"),
            "the command row is missing: {text:?}"
        );
    }

    #[test]
    fn launcher_command_input_shows_the_run_prompt() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_with_pane(b"hi");
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        // The command row is last; select it by its number to open the input.
        let command_number = tutti_agents::Registry::default().specs().len() + 2;
        let digit = char::from_digit(command_number as u32, 10).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char(digit), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::LauncherCommand);

        let mut terminal = Terminal::new(TestBackend::new(64, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("run:"),
            "the command input prompt is missing: {text:?}"
        );
    }
}
