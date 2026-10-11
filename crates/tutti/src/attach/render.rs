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
use tutti_core::{AgentState, Checkpoint, Note, NoteKind, PaneId, PaneInfo, SubagentInfo};

use super::app::{App, IssueRow, Mode};
use super::launcher::LauncherRow;
use super::navigate::{self, Marker};
use super::sidebar::{AgentRow, SidebarEntry, WorkspaceRow};
use super::theme;
use crate::config::{self, Config, Icons, PanelBinding, PrefixAction};
use crate::integrations::Status;
use crate::issues::{self, Issue};
use crate::render::{age, count, note_kind_label, state_label, unix_now};

/// The accent colour of the active theme — the one thing that marks the
/// focused or active thing. Everything else renders muted so it is
/// unmistakable; state colours stay on state dots, the blocked border, and
/// (with the diff colours) the change stats.
fn accent() -> Color {
    theme::current().accent
}

/// The full-row background under a selected sidebar/panel row and a pane's
/// mouse selection. On the terminal theme an ANSI palette colour, so the
/// terminal repaints it from its own scheme on a light/dark flip.
fn select_bg() -> Color {
    theme::current().select_bg
}

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

/// The peek overlay's hint row, two-tone: the answer keys, how to close, and
/// how to get at what the overlay will not forward (esc, free typing) — esc
/// back to the row, then enter jumps into the pane.
const PEEK_HINT: &[(&str, &str)] = &[
    ("y/n 1-9 ↑↓ enter", "answer"),
    ("esc", "close"),
    ("esc enter", "jump in to type"),
];

/// How many rows of an install's preview the setup panel's confirm shows.
const SETUP_PREVIEW_ROWS: usize = 10;

/// The widest a target path renders in the setup panel before it clips.
const SETUP_PATH_W: usize = 60;

/// The approval overlay's hint row, two-tone: the three answers. `esc`
/// answers too — it hands the request on to Claude's own dialog.
const APPROVAL_HINT: &[(&str, &str)] = &[("y", "allow"), ("n", "deny"), ("esc", "pass to claude")];

/// The go-to overlay's key hint, two-tone at its input row's right edge —
/// typing filters, so the movement keys are the ones worth saying.
const NAVIGATE_HINT: &[(&str, &str)] = &[("↑↓", "move"), ("enter", "jump"), ("esc", "back")];

/// The notifications panel's hint row, two-tone under its notes.
const NOTES_HINT: &[(&str, &str)] = &[
    ("enter", "jump"),
    ("r", "read all"),
    ("X", "clear"),
    ("esc", "close"),
];

/// The metadata style of the active theme — DIM on the terminal theme, an
/// explicit muted colour on the truecolor ones — applied to nearly everything
/// that is not a name or the accent.
fn dim() -> Style {
    theme::current().dim()
}

/// Paint `style` under `rect` without touching its symbols — how a theme lays
/// its ground before the chrome draws over it. A no-op for an empty style.
fn fill(frame: &mut Frame, rect: Rect, style: Style) {
    if style != Style::default() {
        frame.buffer_mut().set_style(rect, style);
    }
}

/// Clear `rect` and re-lay the theme ground `bg` under it, so a popup or an
/// overlaid prompt row stays opaque *and* on-theme (a bare `Clear` would fall
/// back to the terminal's own ground under a truecolor theme).
fn clear(frame: &mut Frame, rect: Rect, bg: Option<Color>) {
    frame.render_widget(Clear, rect);
    fill(frame, rect, theme::current().ground(bg));
}

/// Render the whole UI: the top app bar and its rule, the sidebar and pane area
/// beneath, and the footer — plus the transient notification band and any
/// floating panel.
pub fn draw(frame: &mut Frame, app: &App) {
    let _theme = theme::activate(app.config().theme);
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    let (content, footer) = App::content_rect(area);
    let (app_bar, rule) = App::header_rects(area);
    let (sidebar_rect, panes_area, diff_panel_rect) = app.regions(content);
    // The theme's grounds go down first: the pane ground under everything,
    // the panel shade under the chrome bars and columns. The terminal theme
    // has no grounds, so it paints nothing and the terminal's own shows.
    let palette = theme::current();
    fill(frame, area, palette.ground(palette.pane_bg));
    for rect in [
        Some(app_bar),
        Some(rule),
        sidebar_rect,
        diff_panel_rect,
        Some(footer),
    ]
    .into_iter()
    .flatten()
    {
        fill(frame, rect, palette.ground(palette.panel_bg));
    }
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

    draw_app_bar(frame, app, app_bar, spinner);
    draw_rule(frame, rule);
    if let Some(sidebar_rect) = sidebar_rect {
        draw_sidebar(frame, app, sidebar_rect, spinner);
    }
    if let Some(panel_rect) = diff_panel_rect {
        draw_diff_panel(frame, app, panel_rect);
    }
    draw_footer(frame, app, footer);
    // Under the band, so a refused answer's flash stays readable over it.
    draw_peek(frame, app, area);
    draw_approval(frame, app, area);
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
    if app.mode == Mode::RestoreOffer {
        draw_restore_offer(frame, app, area);
    }
    if app.mode == Mode::Launcher {
        draw_launcher(frame, app, area);
    }
    if app.mode == Mode::LauncherCommand {
        draw_launcher_command(frame, app, area);
    }
    if app.mode == Mode::Integrations {
        draw_setup(frame, app, area);
    }
    if app.mode == Mode::Navigate {
        draw_navigate(frame, app, content, spinner);
    }
    draw_notes(frame, app, area);
    if app.mode == Mode::Issues {
        draw_issues(frame, app, area);
    }
}

/// The top app bar (full width): the accent bar and bold `tutti — <session>`
/// wordmark on the left, then the census, the tab segments right-aligned.
/// Draws over the terminal's own background so it tracks the terminal's
/// light/dark scheme.
fn draw_app_bar(frame: &mut Frame, app: &App, area: Rect, spinner: char) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut spans = vec![
        Span::styled("▍", Style::default().fg(accent())),
        Span::styled(
            app.app_title(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(census_spans(app, spinner));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    // The unread-notes chip draws where its click lands, dim until a note
    // goes unread.
    let (chip, rect) = app.notes_chip(area);
    let style = if app.unread_notes() == 0 {
        dim()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    frame.render_widget(Paragraph::new(Span::styled(chip, style)), rect);
    draw_tab_bar(frame, app, app.tab_bar_rect(area), spinner);
}

/// The agent-state census after the app-bar title: one `dot N label` chip per
/// non-empty state, blocked first — the at-a-glance answer to "does anything
/// need me" without opening the sidebar — then the separator before the
/// unread-notes chip that closes it (`App::notes_chip`).
fn census_spans(app: &App, spinner: char) -> Vec<Span<'static>> {
    let glyphs = glyphs(app.config().icons);
    let mut spans = Vec::new();
    for (state, text) in app.census_chips() {
        let (dot, style) = agent_dot(state, spinner, glyphs);
        let sep = if spans.is_empty() { "   " } else { " · " };
        spans.push(Span::styled(sep, dim()));
        spans.push(Span::styled(dot.to_string(), style));
        spans.push(Span::styled(text, dim()));
    }
    let sep = if spans.is_empty() { "   " } else { " · " };
    spans.push(Span::styled(sep, dim()));
    spans
}

/// The full-width dim rule under the app bar.
fn draw_rule(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rule = "─".repeat(area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(rule, dim()))), area);
}

/// The app-bar tab segments: one padded ` <n> <name> ` per tab (active = bold
/// black on the accent block, inactive dim, the trailing ` + ` dim), joined by
/// a one-column separator. A badged tab trails its state dot and a pad — the
/// sidebar's glyph and colour, never dimmed, with the active block's ground
/// running under it. Each segment draws exactly `TabChip::width` columns so
/// clicks land true.
fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect, spinner: char) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let glyphs = glyphs(app.config().icons);
    let mut spans: Vec<Span> = Vec::new();
    for (i, chip) in app.tab_chips().into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let active = matches!(chip.target, Some(id) if app.active_tab == Some(id));
        let style = tab_chip_style(active);
        spans.push(Span::styled(chip.label, style));
        if let Some(state) = chip.badge {
            let (dot, dot_style) = agent_dot(state, spinner, glyphs);
            let dot_style = if active {
                dot_style.bg(accent())
            } else {
                dot_style
            };
            spans.push(Span::styled(dot.to_string(), dot_style));
            spans.push(Span::styled(" ", style));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The style of a tab segment: the active tab is a bold accent block; inactive
/// tabs and the trailing ` + ` recede dim so the block is the one loud thing.
fn tab_chip_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(theme::current().on_accent)
            .bg(accent())
            .add_modifier(Modifier::BOLD)
    } else {
        dim()
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
            ("a".to_string(), "add a project"),
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
        Span::styled(format!("{key}  "), Style::default().fg(accent())),
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
        .map(|r| {
            Line::from(Span::styled(
                (*r).to_string(),
                Style::default().fg(accent()),
            ))
        })
        .collect()
}

/// The sidebar: a borderless panel led by a `projects` caps header, then the
/// `agents` (filtered to the selected project) and `waiting` (cross-project
/// blocked/done) caps headers, each with a collapse arrow and a count. Entries
/// sit inside with one column of padding; a selected entry (only while the
/// sidebar holds focus and is not prompting) gets a subtle full-row highlight and
/// its name as an accent chip. Draws over the terminal's own background — no
/// shade of its own — so it tracks the terminal's light/dark scheme. The
/// new-project prompt overlays the foot when active.
fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect, spinner: char) {
    if area.width < 3 || area.height < 2 {
        return;
    }
    let w = area.width;
    let content_w = w.saturating_sub(2); // one column of padding each side

    let sidebar = app.sidebar();
    let tree_count = sidebar.tree_count;
    let waiting_count = sidebar.entries.len() - tree_count;
    // A selection only pops while the sidebar holds focus and is not prompting.
    let pop = app.sidebar_focused() && !app.sidebar_prompt_active();
    let selected = app.sidebar_selected();
    let glyphs = glyphs(app.config().icons);

    let mut lines: Vec<Line> = Vec::new();
    let focused = app.sidebar_focused();
    lines.push(section_header(
        sidebar.projects_collapsed,
        "projects",
        sidebar.workspace_rows(),
        w,
        focused,
    ));
    if !sidebar.projects_collapsed {
        for (i, entry) in sidebar.entries.iter().take(tree_count).enumerate() {
            let sel = pop && selected == i;
            match entry {
                SidebarEntry::Workspace(row) => {
                    lines.push(bare_row(workspace_inner(row, sel, content_w, glyphs), sel));
                    lines.push(bare_row(
                        workspace_subtitle_inner(row, sel, content_w, glyphs),
                        sel,
                    ));
                }
                SidebarEntry::Agent(a) => {
                    lines.push(bare_row(
                        agent_inner(a, sel, spinner, content_w, glyphs),
                        sel,
                    ));
                    lines.push(bare_row(
                        agent_subtitle_inner(a, sel, app.is_notified(a.pane), content_w),
                        sel,
                    ));
                    let last = a.subagents.len();
                    for (j, subagent) in a.subagents.iter().enumerate() {
                        lines.push(bare_row(
                            subagent_inner(subagent, a.depth, spinner, j + 1 == last, content_w),
                            false,
                        ));
                    }
                }
            }
        }
    }
    // A blank spacer above the waiting header whenever tree rows precede it, so
    // the sections breathe. Mirrored by `Sidebar::waiting_divider_row` — the
    // click arithmetic counts this row too.
    if !sidebar.projects_collapsed && tree_count > 0 {
        lines.push(Line::default());
    }
    lines.push(section_header(
        sidebar.waiting_collapsed,
        "waiting",
        waiting_count,
        w,
        focused,
    ));
    if !sidebar.waiting_collapsed {
        for (i, entry) in sidebar.entries.iter().enumerate().skip(tree_count) {
            if let SidebarEntry::Agent(a) = entry {
                let sel = pop && selected == i;
                lines.push(bare_row(
                    agent_inner(a, sel, spinner, content_w, glyphs),
                    sel,
                ));
                lines.push(bare_row(
                    waiting_subtitle_inner(a, sel, app.is_notified(a.pane), content_w),
                    sel,
                ));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
    if app.sidebar_focused() {
        draw_focus_rail(frame, area);
    }

    if app.sidebar_prompt_active() {
        // The prompt overlays the panel's content column (inside the padding).
        // The fork prompt reuses the same field with an empty listing and its
        // own label.
        let inner = Rect::new(
            area.x + 1,
            area.y + 1,
            content_w,
            area.height.saturating_sub(2),
        );
        let (completions, selected) = if app.sidebar_prompt_shows_completions() {
            (app.prompt_completions(), app.prompt_selected())
        } else {
            (&[][..], 0)
        };
        let label = app.sidebar_prompt_label();
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

/// The changed-files panel: a header with the file count and the coloured
/// `+`/`−` totals, then one row per changed file — its path (directory dim,
/// basename full intensity), its exact counts in the diff colours, and a bar
/// scaled to the largest change so the shape of the delta reads at a glance.
/// Borderless like the sidebar, on the theme's panel ground; the selection row
/// (only while the panel is focused) uses the shared selection background.
/// The visible window follows the selection via `App::diff_first_visible`,
/// which click hit-testing shares. When the agent in view has checkpoints, a
/// `turns` section follows the files: its newest turns, one row each, the
/// cursor walking on into them from the last file.
fn draw_diff_panel(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 3 || area.height < 1 {
        return;
    }
    let content_w = area.width.saturating_sub(2) as usize;
    let files = app.changed_files();
    let turns = app.shown_turns();
    let focused = app.diff_panel_focused();
    let selected = app.diff_selected();
    let in_turns = selected >= files.len();
    let first = app.diff_first_visible(area.height);
    let drawn = app.diff_files_drawn(area.height);
    let max_total = files
        .iter()
        .map(|f| f.insertions + f.deletions)
        .max()
        .unwrap_or(0);

    let mut lines = vec![changes_header(files, area.width, focused && !in_turns)];
    for (i, file) in files.iter().enumerate().skip(first).take(drawn) {
        let sel = focused && i == selected;
        lines.push(bare_row(
            file_stat_inner(file, max_total, sel, content_w),
            sel,
        ));
    }
    if !turns.is_empty() {
        let total = app
            .turns_pane()
            .and_then(|p| app.panes.get(&p))
            .map_or(0, |s| s.info.checkpoints.len());
        lines.push(Line::default());
        lines.push(turns_header(total, area.width, focused && in_turns));
        let now = unix_now();
        for (j, turn) in turns.iter().enumerate() {
            let sel = focused && selected == files.len() + j;
            lines.push(bare_row(turn_inner(turn, now, sel, content_w), sel));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
    if focused {
        draw_focus_rail(frame, area);
    }
}

/// The focus rail: while a chrome panel holds the keys, its left padding
/// column carries a thin accent line down the panel's full height — the
/// selected row's heavier `▎` mark sits on it — so the eye finds the focused
/// panel before it reads a single row. Only empty padding cells are painted;
/// anything a row already put there stays.
fn draw_focus_rail(frame: &mut Frame, area: Rect) {
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, y))
            && cell.symbol() == " "
        {
            cell.set_symbol("▏");
            cell.set_fg(accent());
        }
    }
}

/// The changed-files panel's header row (` changes     13 · +973 −190 `):
/// bold title (accent while the panel is focused), the file count and the
/// coloured totals right-aligned — the totals dropped when the row is too
/// narrow for them.
fn changes_header(files: &[tutti_core::FileStat], width: u16, focused: bool) -> Line<'static> {
    let label = "changes";
    let count = files.len().to_string();
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.insertions, r + f.deletions));
    let mut right = vec![Span::styled(count, dim())];
    let totals = format!(" · +{added} −{removed}");
    if width as usize >= 2 + label.len() + right[0].content.len() + totals.len() + 2 {
        right.push(Span::styled(" · ", dim()));
        right.extend(stat_spans(&format!("+{added} −{removed}")));
    }
    let used: usize = 2
        + label.len()
        + right
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>();
    let gap = (width as usize).saturating_sub(used);
    let mut spans = vec![
        Span::styled(format!(" {label}"), header_style(focused)),
        Span::raw(" ".repeat(gap)),
    ];
    spans.extend(right);
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// The changes panel's `turns` section header (` turns          12 `): the
/// bold title (accent while the cursor is in the section) and the pane's
/// checkpoint count right-aligned, like the `changes` header above it.
fn turns_header(count: usize, width: u16, focused: bool) -> Line<'static> {
    let label = "turns";
    let count = count.to_string();
    let gap = (width as usize).saturating_sub(2 + label.len() + count.len());
    Line::from(vec![
        Span::styled(format!(" {label}"), header_style(focused)),
        Span::raw(" ".repeat(gap)),
        Span::styled(count, dim()),
        Span::raw(" "),
    ])
}

/// One turn row's inner spans: `#N` at full intensity (the turn's identity, an
/// accent chip when selected), its age and the prompt that started it — else
/// what closed it — dim and clipped to fit, and the turn's `+A −B`
/// right-aligned in the diff colours — none for the first state recorded for a
/// pane, which has nothing earlier to diff against.
fn turn_inner(turn: &Checkpoint, now: u64, sel: bool, content_w: usize) -> Vec<Span<'static>> {
    let n = format!("#{}", turn.n);
    let stat = turn.from.as_ref().map_or_else(String::new, |_| {
        format!("+{} −{}", turn.insertions, turn.deletions)
    });
    let (n_w, stat_w) = (n.chars().count(), stat.chars().count());
    let what = turn.prompt.as_deref().unwrap_or(&turn.label);
    let meta: String = format!(" {} {what}", age(turn.time, now))
        .chars()
        .take(content_w.saturating_sub(n_w + stat_w + 1))
        .collect();
    let gap = content_w.saturating_sub(n_w + meta.chars().count() + stat_w);
    let mut spans = vec![
        name_span(&n, sel, Modifier::empty()),
        Span::styled(meta, dim()),
        Span::raw(" ".repeat(gap)),
    ];
    if !stat.is_empty() {
        spans.extend(stat_spans(&stat));
    }
    pad_inner(spans, sel, content_w as u16)
}

/// The width of a changed-file row's bar, in cells.
const BAR_W: usize = 6;

/// One changed-file row's inner spans: the path clipped to fit beside the stat
/// (directory prefix dim, basename full intensity — identity bright, metadata
/// dim), then the exact `+A −B` in the diff colours and the scaled bar, both
/// right-aligned so the bars line up down the column.
fn file_stat_inner(
    file: &tutti_core::FileStat,
    max_total: u64,
    sel: bool,
    content_w: usize,
) -> Vec<Span<'static>> {
    let stat = format!("+{} −{}", file.insertions, file.deletions);
    let stat_w = stat.chars().count();
    let avail = content_w.saturating_sub(stat_w + 1 + BAR_W + 1);
    let (dir, base) = clipped_path(&file.path, avail);
    let path_w = dir.chars().count() + base.chars().count();
    let gap = content_w.saturating_sub(path_w + stat_w + 1 + BAR_W);
    let mut spans = vec![
        Span::styled(dir, dim()),
        name_span(&base, sel, Modifier::empty()),
        Span::raw(" ".repeat(gap)),
    ];
    spans.extend(stat_spans(&stat));
    spans.push(Span::raw(" "));
    spans.extend(stat_bar(file.insertions, file.deletions, max_total));
    pad_inner(spans, sel, content_w as u16)
}

/// A `BAR_W`-cell bar for one file: its length is the file's share of the
/// largest change in the panel (never empty for a non-empty change), split
/// into added and removed cells in proportion — each side keeping at least one
/// cell when it is non-zero and the bar is long enough to show both.
fn stat_bar(insertions: u64, deletions: u64, max_total: u64) -> Vec<Span<'static>> {
    let total = insertions + deletions;
    if total == 0 || max_total == 0 {
        return vec![Span::raw(" ".repeat(BAR_W))];
    }
    let bar = BAR_W as u64;
    let filled = (total * bar).div_ceil(max_total).clamp(1, bar);
    let mut plus = (filled * insertions + total / 2) / total;
    if insertions > 0 && plus == 0 && filled > 1 {
        plus = 1;
    }
    if deletions > 0 && plus == filled && filled > 1 {
        plus = filled - 1;
    }
    let minus = filled - plus;
    let palette = theme::current();
    vec![
        Span::styled(
            "█".repeat(plus as usize),
            Style::default().fg(palette.added),
        ),
        Span::styled(
            "█".repeat(minus as usize),
            Style::default().fg(palette.removed),
        ),
        Span::raw(" ".repeat(BAR_W - filled as usize)),
    ]
}

/// A change stat (`3 files +13 −4`, or just `+13 −4`) as spans: `+N` in the
/// added colour, `−N` in the removed colour — a zero count dim — and anything
/// else dim. Shared by the changes panel, the sidebar stat, and the footer.
fn stat_spans(stat: &str) -> Vec<Span<'static>> {
    let palette = theme::current();
    let mut spans = Vec::new();
    for (i, token) in stat.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let style = match token.chars().next() {
            Some('+') if token != "+0" => Style::default().fg(palette.added),
            Some('−') if token != "−0" => Style::default().fg(palette.removed),
            _ => dim(),
        };
        spans.push(Span::styled(token.to_string(), style));
    }
    spans
}

/// Split `path` into a (dim) directory prefix and its basename, clipped to fit
/// `avail` columns. The directory clips first — whole components dropped from
/// the left behind `…/`, never mid-name fragments (`…/attach/app.rs`, not
/// `…ti/src/attach/app.rs`) — keeping the basename whole; a basename that alone
/// overflows clips at its end.
fn clipped_path(path: &str, avail: usize) -> (String, String) {
    let (dir, base) = match path.rsplit_once('/') {
        Some((dir, base)) => (format!("{dir}/"), base.to_string()),
        None => (String::new(), path.to_string()),
    };
    if dir.chars().count() + base.chars().count() <= avail {
        return (dir, base);
    }
    let base_w = base.chars().count();
    if base_w + 2 <= avail {
        let budget = avail - base_w - 2; // the `…/` prefix spends two columns
        let mut kept = String::new();
        for comp in dir.trim_end_matches('/').rsplit('/') {
            if comp.chars().count() + 1 + kept.chars().count() > budget {
                break;
            }
            kept = format!("{comp}/{kept}");
        }
        return (format!("…/{kept}"), base);
    }
    if base_w <= avail {
        return (String::new(), base);
    }
    let clipped: String = base.chars().take(avail.saturating_sub(1)).collect();
    (String::new(), format!("{clipped}…"))
}

/// A section header row (` ▾ projects      N `): a collapse arrow, the
/// lowercase title bold-but-dim (weight gives hierarchy without brightness;
/// caps stay reserved for the footer's mode chip), and the count right-aligned
/// to the panel edge. Kept quiet — the rows carry the brightness, the header
/// just labels the group.
fn section_header(
    collapsed: bool,
    title: &str,
    count: usize,
    width: u16,
    focused: bool,
) -> Line<'static> {
    let arrow = if collapsed { '▸' } else { '▾' };
    let label = title.to_string();
    let count = count.to_string();
    // ' ' arrow ' ' label <gap> count ' '
    let used = 4 + label.chars().count() + count.chars().count();
    let gap = (width as usize).saturating_sub(used);
    Line::from(vec![
        Span::styled(format!(" {arrow} "), dim()),
        Span::styled(label, header_style(focused)),
        Span::raw(" ".repeat(gap)),
        Span::styled(format!("{count} "), dim()),
    ])
}

/// A section header label: bold-dim at rest, bold accent while its panel holds
/// focus — the accent marks where the keys go.
fn header_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(accent()).add_modifier(Modifier::BOLD)
    } else {
        dim().add_modifier(Modifier::BOLD)
    }
}

/// A content row: one column of padding each side, the selection background
/// carried across both so the highlight reads as one full-width bar.
fn bare_row(inner: Vec<Span<'static>>, sel: bool) -> Line<'static> {
    let pad = if sel {
        Style::default().bg(select_bg())
    } else {
        Style::default()
    };
    // The selected row leads with an accent gutter mark, so focus reads even
    // where the highlight is faint.
    let gutter = if sel {
        Span::styled("▎", pad.fg(accent()))
    } else {
        Span::raw(" ")
    };
    let mut spans = vec![gutter];
    spans.extend(inner);
    spans.push(Span::styled(" ", pad));
    Line::from(spans)
}

/// Pad content spans to exactly `content_w` columns, applying the selection
/// background to every span that does not already carry one (so an accent name
/// chip keeps its own background).
fn pad_inner(mut spans: Vec<Span<'static>>, sel: bool, content_w: u16) -> Vec<Span<'static>> {
    if sel {
        for span in &mut spans {
            if span.style.bg.is_none() {
                span.style = span.style.bg(select_bg());
            }
        }
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if used < content_w as usize {
        let fill = " ".repeat(content_w as usize - used);
        let style = if sel {
            Style::default().bg(select_bg())
        } else {
            Style::default()
        };
        spans.push(Span::styled(fill, style));
    }
    spans
}

/// A name span: the accent chip (black on accent) when selected, else `base`
/// modifiers at full intensity — names are the one thing in the panel that
/// stays bright, so the eye lands on identity and the metadata recedes.
fn name_span(text: &str, sel: bool, base: Modifier) -> Span<'static> {
    if sel {
        Span::styled(
            text.to_string(),
            Style::default()
                .fg(theme::current().on_accent)
                .bg(accent())
                .add_modifier(base),
        )
    } else {
        Span::styled(text.to_string(), Style::default().add_modifier(base))
    }
}

/// A workspace row: an active/inactive dot then the bold name (an accent chip
/// when selected). A nested child (`w.guide` set) is prefixed with its dim tree
/// guide so it reads as sitting beneath its project.
fn workspace_inner(
    w: &WorkspaceRow,
    sel: bool,
    content_w: u16,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let (dot, dot_style) = if w.active {
        (glyphs.ws_active, Style::default().fg(accent()))
    } else {
        (glyphs.ws_inactive, dim())
    };
    let mut spans = Vec::new();
    if let Some(guide) = w.guide {
        spans.push(Span::styled(format!("{guide} "), dim()));
    }
    spans.push(Span::styled(format!("{dot} "), dot_style));
    spans.push(name_span(&w.name, sel, Modifier::BOLD));
    // Right-aligned roll-up chips (`● 2 ● 1`): the subtree's agent census, so a
    // glance at the project row answers "anything of mine need me?".
    if !w.census.is_empty() {
        let chips = [
            (w.census.blocked, AgentState::Blocked),
            (w.census.working, AgentState::Working),
            (w.census.done, AgentState::Done),
        ];
        let mut chip_spans = Vec::new();
        for (count, state) in chips {
            if count == 0 {
                continue;
            }
            // A static dot even for working — the row chip counts, it does not
            // animate; the animation budget belongs to the agent rows.
            let (_, style) = agent_dot(state, '·', glyphs);
            if !chip_spans.is_empty() {
                chip_spans.push(Span::raw(" "));
            }
            chip_spans.push(Span::styled(glyphs.blocked.to_string(), style));
            chip_spans.push(Span::styled(format!(" {count}"), dim()));
        }
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let chip_w: usize = chip_spans.iter().map(|s| s.content.chars().count()).sum();
        if used + 1 + chip_w <= content_w as usize {
            spans.push(Span::raw(" ".repeat(content_w as usize - used - chip_w)));
            spans.extend(chip_spans);
        }
    }
    pad_inner(spans, sel, content_w)
}

/// A workspace's dim subtitle: the branch marker and ref on the left, and a
/// right-aligned tag — a dim-red fork `stale` marker when the working copy is
/// stale, otherwise the dim jj change stat. The stale tag wins over the stat,
/// and either is dropped when it would collide with the ref. A nested child's
/// subtitle indents two extra columns so it aligns under the child's name.
fn workspace_subtitle_inner(
    w: &WorkspaceRow,
    sel: bool,
    content_w: u16,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let indent = if w.guide.is_some() { "    " } else { "  " };
    let left = format!(
        "{indent}{} {}",
        glyphs.branch,
        w.subtitle.as_deref().unwrap_or("")
    );
    if w.stale {
        let tag = Span::styled(
            format!("{} stale", glyphs.fork),
            dim().fg(theme::current().blocked),
        );
        return right_tagged(left, vec![tag], sel, content_w);
    }
    if let Some(changes) = w.changes.as_deref() {
        return right_tagged(left, stat_spans(changes), sel, content_w);
    }
    pad_inner(vec![Span::styled(left, dim())], sel, content_w)
}

/// Lay out a subtitle's left text with a right-aligned tag inside `content_w`,
/// falling back to just the left text (padded) when the tag will not fit.
fn right_tagged(
    left: String,
    tag: Vec<Span<'static>>,
    sel: bool,
    content_w: u16,
) -> Vec<Span<'static>> {
    let cw = content_w as usize;
    let lw = left.chars().count();
    let tw: usize = tag.iter().map(|s| s.content.chars().count()).sum();
    if lw + 1 + tw <= cw {
        let gap = cw - lw - tw;
        let mut spans = vec![Span::styled(left, dim()), Span::raw(" ".repeat(gap))];
        spans.extend(tag);
        if sel {
            for span in &mut spans {
                if span.style.bg.is_none() {
                    span.style = span.style.bg(select_bg());
                }
            }
        }
        return spans;
    }
    pad_inner(vec![Span::styled(left, dim())], sel, content_w)
}

/// An agent row: a state dot (a spinner while working) then the pane title (an
/// accent chip when selected), and the issue it was deployed on, dim.
fn agent_inner(
    a: &AgentRow,
    sel: bool,
    spinner: char,
    content_w: u16,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let (dot, dot_style) = agent_dot(a.state, spinner, glyphs);
    let mut spans = Vec::new();
    if let Some(guide) = a.guide {
        let indent = "  ".repeat(a.depth.saturating_sub(1) as usize);
        spans.push(Span::styled(format!("{indent}{guide} "), dim()));
    }
    spans.push(Span::styled(format!("{dot} "), dot_style));
    spans.push(name_span(&a.title, sel, Modifier::empty()));
    if let Some(issue) = &a.issue {
        spans.push(Span::styled(format!(" {}", issues::label(issue)), dim()));
    }
    pad_inner(spans, sel, content_w)
}

/// The dim `state · kind` second line for an agent — indented under its title,
/// which a tree row's guide pushes right — plus the bell mark when a
/// notification is pending. A default-titled pane already says the kind in its
/// title row, so the kind is dropped there rather than stuttered.
fn agent_subtitle_inner(
    a: &AgentRow,
    sel: bool,
    notified: bool,
    content_w: u16,
) -> Vec<Span<'static>> {
    let indent = "  ".repeat(a.depth as usize + 1);
    let text = if a.kind == a.title {
        format!("{indent}{}", state_label(a.state))
    } else {
        format!("{indent}{} · {}", state_label(a.state), a.kind)
    };
    let mut spans = vec![Span::styled(text, dim())];
    if notified {
        spans.push(Span::styled(
            " 🔔",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    pad_inner(spans, sel, content_w)
}

/// The dim `project · kind` second line for a waiting-section row — the rows
/// are cross-project, so the project is the orienting fact — plus the bell mark
/// when a notification is pending. A default-titled pane already says the kind
/// in its title row, so the kind is dropped there rather than stuttered. A
/// held approval says what it would run instead: `tool · input`, the tool at
/// full intensity.
fn waiting_subtitle_inner(
    a: &AgentRow,
    sel: bool,
    notified: bool,
    content_w: u16,
) -> Vec<Span<'static>> {
    let mut spans = match &a.approval {
        Some(approval) => vec![
            Span::raw(format!("  {}", approval.tool)),
            Span::styled(format!(" · {}", approval.summary), dim()),
        ],
        None if a.kind == a.title => vec![Span::styled(format!("  {}", a.project_name), dim())],
        None => vec![Span::styled(
            format!("  {} · {}", a.project_name, a.kind),
            dim(),
        )],
    };
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
    depth: u8,
    spinner: char,
    last: bool,
    content_w: u16,
) -> Vec<Span<'static>> {
    let indent = "  ".repeat(depth.max(1) as usize);
    let guide = if last { '└' } else { '├' };
    let glyph = if sub.running { spinner } else { '·' };
    pad_inner(
        vec![Span::styled(
            format!("{indent}{guide} {glyph} {}", sub.desc),
            dim(),
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
/// highlighted row in the accent. Add-project passes `open:` and the new
/// workspace's `where:` step pass directory completions; the `workspace name:`
/// step passes an empty listing.
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
            Style::default().fg(accent())
        } else {
            dim()
        };
        let row = Rect::new(inner.x, foot - rows + i, inner.width, 1);
        clear(frame, row, theme::current().panel_bg);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {}", completions[i as usize]),
                style,
            ))),
            row,
        );
    }

    let row = Rect::new(inner.x, foot, inner.width, 1);
    clear(frame, row, theme::current().panel_bg);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("▍", Style::default().fg(accent())),
            Span::styled(label.to_string(), dim()),
            Span::styled(text.to_string(), Style::default()),
            Span::styled("█", Style::default().fg(accent())),
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
        draw_screen(
            frame.buffer_mut(),
            inner,
            screen,
            0,
            app.selection_span(pane),
        );
        if focused && state.scroll.is_none() && !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            if col < inner.width && row < inner.height {
                frame.set_cursor_position((inner.x + col, inner.y + row));
            }
        }
    }
}

/// Blit a vt100 screen into `area`, one cell at a time, mapping colours and
/// attributes, starting from screen row `first_row` (0 for a pane; the prompt
/// region's top for a peek). Wide-character continuation cells are cleared so a
/// glyph is not drawn twice. `selection`, when set, is the inclusive `(start,
/// end)` span of a live mouse selection in screen coordinates; cells inside it
/// take the `select_bg()` highlight — an ANSI palette colour, so it repaints
/// with the terminal's scheme like the sidebar selection.
fn draw_screen(
    buf: &mut Buffer,
    area: Rect,
    screen: &vt100::Screen,
    first_row: u16,
    selection: Option<((u16, u16), (u16, u16))>,
) {
    let (rows, cols) = screen.size();
    let height = area.height.min(rows.saturating_sub(first_row));
    let width = area.width.min(cols);
    for y in 0..height {
        let row = first_row + y;
        for col in 0..width {
            let Some(buf_cell) = buf.cell_mut((area.x + col, area.y + y)) else {
                continue;
            };
            let selected = selection.is_some_and(|(s, e)| in_span(s, e, cols, row, col));
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
                    let style = cell_style(cell);
                    buf_cell.set_style(if selected {
                        style.bg(select_bg())
                    } else {
                        style
                    });
                }
                None => {
                    buf_cell.set_char(' ');
                    if selected {
                        buf_cell.set_style(Style::default().bg(select_bg()));
                    }
                }
            }
        }
    }
}

/// Whether cell `(row, col)` falls inside the inclusive stream selection from
/// `start` to `end` (top-to-bottom, then left-to-right): the first row runs from
/// its start column, the last to its end column, and whole rows between span the
/// full `cols` width.
fn in_span(start: (u16, u16), end: (u16, u16), cols: u16, row: u16, col: u16) -> bool {
    if row < start.0 || row > end.0 {
        return false;
    }
    let c0 = if row == start.0 { start.1 } else { 0 };
    let c1 = if row == end.0 {
        end.1
    } else {
        cols.saturating_sub(1)
    };
    col >= c0 && col <= c1
}

/// The ratatui style for a vt100 cell: explicit colours pass straight through;
/// the default foreground/background take the theme's pane ground (nothing —
/// the terminal's own — on the terminal theme).
pub fn cell_style(cell: &vt100::Cell) -> Style {
    let palette = theme::current();
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
    let fg = match cell.fgcolor() {
        vt100::Color::Default => palette.fg.unwrap_or(Color::Reset),
        colour => map_color(colour),
    };
    let bg = match cell.bgcolor() {
        vt100::Color::Default => palette.pane_bg.unwrap_or(Color::Reset),
        colour => map_color(colour),
    };
    Style::default().fg(fg).bg(bg).add_modifier(modifier)
}

/// Map a vt100 colour to the equivalent ratatui colour.
pub fn map_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The footer bar (full width): a mode chip on the left in the accent block when
/// not in terminal mode — plus the two-tone sidebar hint in sidebar mode — and
/// the standing help/detach hint pinned to the right. Draws over the terminal's
/// own background so it tracks the terminal's light/dark scheme. The tab list and
/// per-pane state chips are gone: the app bar and sidebar own those, and
/// transients fire the notification band above.
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    if let Some(label) = mode_label(app.mode) {
        spans.push(Span::styled(
            format!(" {label} "),
            Style::default().fg(theme::current().on_accent).bg(accent()),
        ));
    } else if let Some(ws) = app.active_workspace() {
        // Terminal mode has no chip, so the bar earns its row with context:
        // where am I (dot + workspace), on what (branch), how dirty (stat).
        let glyphs = glyphs(app.config().icons);
        spans.push(Span::styled(
            format!(" {} ", glyphs.ws_active),
            Style::default().fg(accent()),
        ));
        spans.push(Span::styled(
            ws.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        if let Some(branch) = &ws.branch {
            spans.push(Span::styled(format!(" · {branch}"), dim()));
        }
        if let Some(changes) = &ws.changes {
            spans.push(Span::styled(" · ", dim()));
            spans.extend(stat_spans(changes));
        }
    }
    if app.mode == Mode::Sidebar {
        spans.push(Span::raw("  "));
        spans.extend(two_tone(&key_groups(config::SIDEBAR_TABLE, true)).0);
    }
    if app.mode == Mode::DiffFiles {
        spans.push(Span::raw("  "));
        spans.extend(two_tone(&key_groups(config::CHANGES_TABLE, true)).0);
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
        Mode::ConfirmKill(_)
        | Mode::ConfirmKillWorkspace(_)
        | Mode::ConfirmKillAgent(_)
        | Mode::ConfirmMerge(_)
        | Mode::ConfirmCleanup(_)
        | Mode::ConfirmRestore(..) => Some("CONFIRM"),
        Mode::Scroll(_) => Some("SCROLL"),
        Mode::Help => Some("HELP"),
        Mode::RestoreOffer => Some("RESTORE"),
        Mode::Sidebar => Some("SIDEBAR"),
        Mode::DiffFiles => Some("CHANGES"),
        Mode::SidebarPrompt => Some("ADD PROJECT"),
        Mode::SidebarWorkspaceName | Mode::SidebarWorkspaceDest => Some("WORKSPACE"),
        Mode::Launcher | Mode::LauncherCommand => Some("RUN"),
        Mode::Integrations => Some("SETUP"),
        Mode::Peek(_) => Some("ANSWER"),
        Mode::Navigate => Some("GO TO"),
        Mode::Issues => Some("ISSUES"),
        Mode::Approval(_) => Some("APPROVE"),
        Mode::Notes => Some("NOTES"),
        Mode::ConfirmClearNotes => Some("CONFIRM"),
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
        theme::current().blocked
    } else {
        accent()
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            msg.to_string(),
            Style::default()
                .fg(theme::current().on_accent)
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

/// Two-tone key hints: each key bright, its label dim, pairs separated by two
/// plain spaces — the single space inside a pair binds key to label tighter
/// than pair sits to pair, so the grouping needs no separator glyph. Returns
/// the spans and their total rendered width.
fn two_tone<S: AsRef<str>>(pairs: &[(S, &str)]) -> (Vec<Span<'static>>, u16) {
    let dim = dim();
    let mut spans = Vec::new();
    let mut width = 0usize;
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            width += 2;
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
/// rounded panel titled ` keys `, key letters in the accent.
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

    clear(frame, popup, theme::current().pane_bg);
    frame.render_widget(Paragraph::new(lines).block(popup_block(" keys ")), popup);
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let lines = help_lines(app.config());
    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_w + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    clear(frame, popup, theme::current().pane_bg);
    frame.render_widget(Paragraph::new(lines).block(popup_block(" help ")), popup);
}

/// The restore offer: a centred panel over everything asking whether to bring
/// back the layout a previous daemon saved — what it holds and how old it is —
/// over its two-tone keys.
fn draw_restore_offer(frame: &mut Frame, app: &App, area: Rect) {
    let Some(offer) = app.restore_offer() else {
        return;
    };
    let saved = match age(offer.saved_at, unix_now()).as_str() {
        "now" => "saved just now".to_string(),
        ago => format!("saved {ago} ago"),
    };
    let summary = format!(
        " {} · {} · {} to revive · {saved} ",
        count(offer.workspaces, "project"),
        count(offer.panes, "pane"),
        count(offer.agents, "agent"),
    );
    let mut keys = vec![Span::raw(" ")];
    keys.extend(two_tone(&[("enter", "restore"), ("d", "discard"), ("esc", "later")]).0);
    let lines = vec![Line::from(summary), Line::from(""), Line::from(keys)];
    draw_centered_popup(frame, area, " restore last session? ", lines);
}

/// The agent launcher overlay: a centred rounded panel titled
/// ` run in <workspace> ` listing what can start in a pane — the registry agents
/// (dim-italic and `(not installed)` when their binary is absent), then the
/// shell and command rows. The selected row takes an accent `❯` marker and
/// accent name; each row shows its quick-select number.
fn draw_launcher(frame: &mut Frame, app: &App, area: Rect) {
    let selected = app.launcher_selected();
    let mut lines: Vec<Line> = app
        .launcher_rows()
        .iter()
        .enumerate()
        .map(|(i, row)| launcher_line(i, row, i == selected))
        .collect();
    if let Some(next) = app.launcher_tab_hint() {
        lines.push(popup_key_line("tab", &format!("run in {next}")));
    }
    lines.push(popup_key_line("esc", "cancel"));
    draw_centered_popup(frame, area, &app.launcher_title(), lines);
}

/// One launcher row: `<n>  <name>   <role>`, an accent `❯` marker and accent
/// name when selected. An unavailable agent row is dim-italic and tagged
/// ` (not installed)`.
fn launcher_line(idx: usize, row: &LauncherRow, selected: bool) -> Line<'static> {
    let num = Span::styled(format!(" {} ", idx + 1), dim());
    if !row.available {
        // The dim catalog foot: not selectable, the role already carries the
        // product name and its project link.
        return Line::from(vec![
            num,
            Span::styled(
                format!("  {}   {}", row.name, row.role),
                dim().add_modifier(Modifier::ITALIC),
            ),
        ]);
    }
    let marker = if selected {
        Span::styled(
            "❯ ",
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };
    let name_style = if selected {
        Style::default().fg(accent()).add_modifier(Modifier::BOLD)
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
/// ` run in <workspace> ` with a single `▍run: <text>█` line, mirroring the
/// sidebar prompt's accent-bar input.
fn draw_launcher_command(frame: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::styled("▍", Style::default().fg(accent())),
        Span::styled("run: ", dim()),
        Span::styled(app.launcher_command().to_string(), Style::default()),
        Span::styled("█", Style::default().fg(accent())),
    ]);
    // A minimum width so a short command still reads as an input field.
    let width = (line.width() as u16 + 2).max(24).min(area.width);
    let height = 3u16.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);
    clear(frame, popup, theme::current().pane_bg);
    let title = app.launcher_title();
    frame.render_widget(Paragraph::new(line).block(popup_block(&title)), popup);
}

/// The issues panel: a centred popup titled ` issues · <project> ` listing
/// every configured source's issues — `[source] id  title`, labels and url
/// dim, the highlighted row with the accent `❯` — with a dim `loading…` row
/// while a source fetches and its error dim-red when it fails. The filter
/// input leads while it is typed or set; the keys close the panel, `enter`
/// saying where a deploy lands. A list taller than the screen shows the
/// window around the highlight.
fn draw_issues(frame: &mut Frame, app: &App, area: Rect) {
    let selected = app.issues_selected();
    let (mut at, mut n) = (0, 0);
    let mut body: Vec<Line> = Vec::new();
    for row in app.issue_rows() {
        let line = match row {
            IssueRow::Status {
                source,
                error: None,
            } => Line::from(Span::styled(format!("   [{source}] loading…"), dim())),
            IssueRow::Status {
                source,
                error: Some(error),
            } => Line::from(Span::styled(
                format!("   [{source}] {}", error.lines().next().unwrap_or_default()),
                dim().fg(theme::current().blocked),
            )),
            IssueRow::Issue(issue) => {
                if n == selected {
                    at = body.len();
                }
                n += 1;
                issue_line(issue, n - 1 == selected)
            }
        };
        body.push(line);
    }
    if body.is_empty() {
        let empty = if app.config().issues.is_empty() {
            "   no [[issues]] sources in the config"
        } else {
            "   no issues"
        };
        body.push(Line::from(Span::styled(empty, dim())));
    }
    let (query, typing) = app.issues_query();
    let mut head = Vec::new();
    if typing || !query.is_empty() {
        let cursor = if typing { "█" } else { "" };
        head.push(Line::from(vec![
            Span::styled(" ▍", Style::default().fg(accent())),
            Span::styled("filter ", dim()),
            Span::raw(query.to_string()),
            Span::styled(cursor, Style::default().fg(accent())),
        ]));
    }
    let here = app.issues_deploy_here();
    let mut keys = vec![(
        "enter",
        if here {
            "deploy here"
        } else {
            "deploy in a new workspace"
        },
    )];
    if app.issues_jj() {
        keys.push(("h", if here { "new workspace" } else { "here" }));
    }
    keys.extend([("/", "filter"), ("esc", "close")]);
    let mut hint = vec![Span::raw(" ")];
    hint.extend(two_tone(&keys).0);
    let foot = [Line::default(), Line::from(hint)];
    // The list gets what the borders, the input and the keys leave.
    let room = (area.height as usize)
        .saturating_sub(2 + head.len() + foot.len())
        .max(1);
    let lines = head
        .into_iter()
        .chain(
            body.into_iter()
                .skip(at.saturating_sub(room - 1))
                .take(room),
        )
        .chain(foot)
        .collect();
    draw_centered_popup(frame, area, &app.issues_title(), lines);
}

/// One issue row: `[source] id  title`, then its labels and url dim. The
/// highlighted row leads with the accent `❯` and takes the accent on its id
/// and title.
fn issue_line(issue: &Issue, selected: bool) -> Line<'static> {
    let lit = Style::default().fg(accent()).add_modifier(Modifier::BOLD);
    let (marker, name) = if selected {
        (Span::styled(" ❯ ", lit), lit)
    } else {
        (Span::raw("   "), Style::default())
    };
    let mut meta = String::new();
    if !issue.labels.is_empty() {
        meta = format!("  {}", issue.labels.join(", "));
    }
    if let Some(url) = &issue.url {
        meta.push_str(&format!("  {url}"));
    }
    Line::from(vec![
        marker,
        Span::styled(format!("[{}] ", issue.source), dim()),
        Span::styled(
            format!("{}  {}", issues::label(&issue.id), issue.title),
            name,
        ),
        Span::styled(meta, dim()),
    ])
}

/// The setup panel: a centred rounded panel titled ` setup · user ` — or
/// ` setup · project <name> ` once `s` switches it — over the integrations
/// tutti installs. Each row leads with a status dot (green installed, yellow
/// out of date, dim not installed, red where tutti cannot or will not write),
/// then the name bright and the status dim, the target file dim beneath —
/// clipped from the left so its name survives; the highlighted row takes the
/// launcher's accent `❯`. While it asks to install, the panel holds that row's
/// question, change summary, and the head of its preview instead.
fn draw_setup(frame: &mut Frame, app: &App, area: Rect) {
    let title = match app.setup_project() {
        Some(name) => format!(" setup · project {name} "),
        None => " setup · user ".to_string(),
    };
    let rows: Vec<_> = app.setup_rows().collect();
    let selected = app.setup_selected();
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let target = |path: &std::path::Path| {
        let (dir, base) = clipped_path(&path.display().to_string(), SETUP_PATH_W);
        format!("{dir}{base}")
    };
    if app.setup_confirming()
        && let Some((label, Ok(plan))) = rows.get(selected)
        && let Status::Missing(change) | Status::Outdated(change) = &plan.status
    {
        let preview: Vec<&str> = change.preview.lines().collect();
        let mut lines = vec![
            Line::from(vec![
                Span::styled(format!(" install {label}? "), bold),
                Span::styled("y/N ", dim()),
            ]),
            Line::from(Span::styled(format!(" {} ", target(&plan.path)), dim())),
            Line::from(format!(" {} ", change.summary)),
            Line::default(),
        ];
        lines.extend(
            preview
                .iter()
                .take(SETUP_PREVIEW_ROWS)
                .map(|row| Line::from(Span::styled(format!(" {row} "), dim()))),
        );
        if preview.len() > SETUP_PREVIEW_ROWS {
            let more = preview.len() - SETUP_PREVIEW_ROWS;
            lines.push(Line::from(Span::styled(
                format!(" … {more} more lines "),
                dim(),
            )));
        }
        return draw_centered_popup(frame, area, &title, lines);
    }
    let palette = theme::current();
    let glyphs = glyphs(app.config().icons);
    let colour = |c| Style::default().fg(c);
    let name_w = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for (i, (label, plan)) in rows.into_iter().enumerate() {
        let (dot, dot_style, status, file) = match plan {
            Ok(plan) => {
                let (dot, style, status) = match &plan.status {
                    Status::Installed => (glyphs.done, colour(palette.done), "installed".into()),
                    Status::Outdated(_) => {
                        (glyphs.idle, colour(palette.working), "needs update".into())
                    }
                    Status::Missing(_) => (glyphs.idle, dim(), "not installed".into()),
                    Status::Foreign => (
                        glyphs.blocked,
                        colour(palette.blocked),
                        "not tutti's".into(),
                    ),
                    Status::Unreadable(why) => (
                        glyphs.blocked,
                        colour(palette.blocked),
                        format!("unreadable: {why}"),
                    ),
                };
                (dot, style, status, target(&plan.path))
            }
            Err(e) => (
                glyphs.blocked,
                colour(palette.blocked),
                "unavailable".to_string(),
                format!("{e:#}"),
            ),
        };
        let (marker, name_style) = if i == selected {
            let accent = Style::default().fg(accent()).add_modifier(Modifier::BOLD);
            (Span::styled("❯ ", accent), accent)
        } else {
            (Span::raw("  "), bold)
        };
        lines.push(Line::from(vec![
            Span::raw(" "),
            marker,
            Span::styled(format!("{dot} "), dot_style),
            Span::styled(format!("{label:<name_w$}"), name_style),
            Span::styled(format!("  {status} "), dim()),
        ]));
        lines.push(Line::from(Span::styled(format!("     {file} "), dim())));
    }
    let scope = match app.setup_project() {
        Some(_) => "user scope",
        None => "project scope",
    };
    let mut hint = vec![Span::raw(" ")];
    hint.extend(two_tone(&[("enter", "install"), ("s", scope), ("esc", "close")]).0);
    hint.push(Span::raw(" "));
    lines.push(Line::default());
    lines.push(Line::from(hint));
    draw_centered_popup(frame, area, &title, lines);
}

/// The peek overlay: a centred rounded panel titled ` answer <agent> ·
/// <workspace> ` showing the live tail of a blocked agent's screen — exactly
/// the prompt region its fingerprint covers — blitted cell by cell from the
/// client's mirror, over the two-tone hint row. Geometry comes from
/// `App::peek_popup`, which the mouse hit-test shares.
fn draw_peek(frame: &mut Frame, app: &App, area: Rect) {
    let Some((pane, popup, rows)) = app.peek_popup(area) else {
        return;
    };
    let Some(state) = app.panes.get(&pane) else {
        return;
    };
    let Some(body) = draw_overlay_frame(frame, app, popup, pane, "answer", PEEK_HINT) else {
        return;
    };
    draw_screen(
        frame.buffer_mut(),
        body,
        state.parser.screen(),
        rows.start as u16,
        None,
    );
}

/// The approval overlay, framed as the peek is — ` approve <agent> ·
/// <workspace> ` — naming the tool a held permission request would run, in
/// bold, over its input and the two-tone hint row. Geometry comes from
/// `App::approval_popup`, which the mouse hit-test shares.
fn draw_approval(frame: &mut Frame, app: &App, area: Rect) {
    let Some((pane, popup, rows)) = app.approval_popup(area) else {
        return;
    };
    let Some(body) = draw_overlay_frame(frame, app, popup, pane, "approve", APPROVAL_HINT) else {
        return;
    };
    let lines: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let style = if i == 0 {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!(" {row}"), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), body);
}

/// Clear `popup` and frame a floating overlay over `pane` in it: the rounded
/// ` <verb> <agent> · <workspace> ` panel with `hint` two-tone on its last
/// row. Returns the body rect above the hint, `None` when there is no room.
fn draw_overlay_frame(
    frame: &mut Frame,
    app: &App,
    popup: Rect,
    pane: PaneId,
    verb: &str,
    hint: &[(&str, &str)],
) -> Option<Rect> {
    let info = &app.panes.get(&pane)?.info;
    let workspace = app
        .workspaces
        .iter()
        .find(|w| w.tabs.iter().any(|t| t.panes.iter().any(|p| p.id == pane)))
        .map_or_else(String::new, |w| format!(" · {}", w.name));
    let title = format!(" {verb} {}{workspace} ", identity(info));
    clear(frame, popup, theme::current().pane_bg);
    let block = popup_block(&title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return None;
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(two_tone(hint).0);
    let hint_row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    frame.render_widget(Paragraph::new(Line::from(spans)), hint_row);
    Some(Rect::new(inner.x, inner.y, inner.width, inner.height - 1))
}

/// The go-to overlay (navigate mode): an opaque panel over the whole content
/// `area`. The `go to` filter input leads, its key hint at the right edge;
/// then the rows — with no filter the state groups (`needs you`, `done`,
/// `working`) over the project tree, while filtering one flat ranked list, or
/// a dim `no matches`. Headers are the sidebar's bold-dim labels and entries
/// draw like its rows, so the highlight is the one accent: a full-row bar with
/// the `▎` gutter and the name chip. The window follows the highlight through
/// `App::navigate_list`, which the click hit-test shares.
fn draw_navigate(frame: &mut Frame, app: &App, area: Rect, spinner: char) {
    if area.width < 3 || area.height < 3 {
        return;
    }
    clear(frame, area, theme::current().panel_bg);
    let input = Line::from(vec![
        Span::styled("▍", Style::default().fg(accent())),
        Span::styled("go to ", dim()),
        Span::raw(app.navigate_query().to_string()),
        Span::styled("█", Style::default().fg(accent())),
    ]);
    let input_w = input.width() as u16;
    frame.render_widget(
        Paragraph::new(input),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let (hint, hint_w) = two_tone(NAVIGATE_HINT);
    if input_w + 2 + hint_w < area.width {
        let rect = Rect::new(area.x + area.width - hint_w - 1, area.y, hint_w, 1);
        frame.render_widget(Paragraph::new(Line::from(hint)), rect);
    }

    let (rows, selected) = app.navigate_rows();
    let (list, first) = App::navigate_list(area, selected);
    if rows.is_empty() {
        let none = Line::from(Span::styled(" no matches", dim()));
        frame.render_widget(Paragraph::new(none), list);
        return;
    }
    let content_w = list.width.saturating_sub(2);
    let glyphs = glyphs(app.config().icons);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(list.height as usize)
        .map(|(i, row)| match row {
            navigate::Row::Header { title, count } => {
                let count = count.to_string();
                let gap = (list.width as usize).saturating_sub(2 + title.len() + count.len());
                Line::from(vec![
                    Span::styled(format!(" {title}"), header_style(false)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(format!("{count} "), dim()),
                ])
            }
            navigate::Row::Spacer => Line::default(),
            navigate::Row::Entry(e) => {
                let sel = selected == Some(i);
                bare_row(navigate_inner(e, sel, spinner, content_w, glyphs), sel)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
}

/// One go-to entry: its tree guide in the projects section, then what leads
/// it — a workspace's dot (accent while it owns the active tab), a tab's dim
/// app-bar number, an agent's state dot or spinner, nothing for a plain pane —
/// the name at full intensity (bold for a workspace, the accent chip when
/// selected), and the dim meta.
fn navigate_inner(
    e: &navigate::Entry,
    sel: bool,
    spinner: char,
    content_w: u16,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(guide) = e.guide {
        let indent = "  ".repeat(e.depth.saturating_sub(1) as usize);
        spans.push(Span::styled(format!("{indent}{guide} "), dim()));
    }
    let (lead, lead_style, name) = match e.marker {
        Marker::Workspace { active: true } => (
            glyphs.ws_active.to_string(),
            Style::default().fg(accent()),
            Modifier::BOLD,
        ),
        Marker::Workspace { active: false } => {
            (glyphs.ws_inactive.to_string(), dim(), Modifier::BOLD)
        }
        Marker::Tab(n) => (n.to_string(), dim(), Modifier::empty()),
        Marker::Pane(Some(state)) => {
            let (dot, style) = agent_dot(state, spinner, glyphs);
            (dot.to_string(), style, Modifier::empty())
        }
        Marker::Pane(None) => (" ".into(), Style::default(), Modifier::empty()),
    };
    spans.push(Span::styled(format!("{lead} "), lead_style));
    spans.push(name_span(&e.name, sel, name));
    if !e.meta.is_empty() {
        spans.push(Span::styled(format!("  {}", e.meta), dim()));
    }
    pad_inner(spans, sel, content_w)
}

/// The notifications panel: a centred rounded panel titled ` notifications `
/// (and its unread count) listing the history newest first, one note a row,
/// over its two-tone hint row — or a dim `no notifications`. Rows draw like
/// the sidebar's, so the highlight is the one accent. The window follows the
/// highlight through `App::notes_popup`, which the mouse hit-test shares.
fn draw_notes(frame: &mut Frame, app: &App, area: Rect) {
    let Some((popup, list, first)) = app.notes_popup(area) else {
        return;
    };
    let title = match app.unread_notes() {
        0 => " notifications ".to_string(),
        unread => format!(" notifications · {unread} unread "),
    };
    clear(frame, popup, theme::current().pane_bg);
    frame.render_widget(popup_block(&title), popup);
    if list.height == 0 {
        return;
    }
    let content_w = list.width.saturating_sub(2) as usize;
    let selected = app.notes_selected();
    let (now, glyphs) = (unix_now(), glyphs(app.config().icons));
    let lines: Vec<Line> = if app.notes().is_empty() {
        vec![Line::from(Span::styled(" no notifications", dim()))]
    } else {
        app.notes()
            .iter()
            .rev()
            .enumerate()
            .skip(first)
            .take(list.height as usize)
            .map(|(i, note)| {
                let gone = !app.panes.contains_key(&note.pane);
                let sel = i == selected;
                bare_row(note_inner(note, now, gone, sel, content_w, glyphs), sel)
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), list);
    let hint = Rect::new(list.x, list.y + list.height, list.width, 1);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(two_tone(NOTES_HINT).0);
    frame.render_widget(Paragraph::new(Line::from(spans)), hint);
}

/// One note row's inner spans: its unread dot — the state dot for a blocked or
/// done note, a bold dot for a bell or notification, none once read — its age,
/// the pane title at full intensity (the accent chip when selected), the dim
/// `workspace · kind`, and the note's own text, clipped to fit. A note whose
/// pane is gone draws dim throughout.
fn note_inner(
    note: &Note,
    now: u64,
    gone: bool,
    sel: bool,
    content_w: usize,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    let (dot, dot_style) = match (note.read, note.kind) {
        (true, _) => (' ', Style::default()),
        (false, NoteKind::Blocked) => agent_dot(AgentState::Blocked, ' ', glyphs),
        (false, NoteKind::Done) => agent_dot(AgentState::Done, ' ', glyphs),
        (false, NoteKind::Bell | NoteKind::Notify) => {
            ('●', Style::default().add_modifier(Modifier::BOLD))
        }
    };
    let mut spans = vec![
        Span::styled(dot.to_string(), dot_style),
        Span::styled(format!(" {:>3}  ", age(note.time, now)), dim()),
        name_span(&note.pane_title, sel, Modifier::empty()),
        Span::styled(
            format!("  {} · {}", note.workspace, note_kind_label(note.kind)),
            dim(),
        ),
    ];
    let text = note.text();
    if !text.is_empty() {
        let used: usize = spans.iter().map(Span::width).sum();
        let room = content_w.saturating_sub(used);
        spans.push(Span::raw(
            format!("  {text}").chars().take(room).collect::<String>(),
        ));
    }
    if gone {
        for span in &mut spans {
            span.style = dim();
        }
    }
    pad_inner(spans, sel, content_w as u16)
}

/// Render `lines` in a rounded panel titled `title`, centred over `area`. Shared
/// by the launcher picker with the help/which-key panel look.
fn draw_centered_popup(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line>) {
    let inner_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_w + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);
    clear(frame, popup, theme::current().pane_bg);
    frame.render_widget(Paragraph::new(lines).block(popup_block(title)), popup);
}

/// A floating panel's block: a rounded dim border carrying `title`. The panel is
/// cleared to the terminal's own background before it draws (see the `Clear`
/// each caller renders), so it stays opaque over the pane and tracks the
/// terminal's light/dark scheme.
fn popup_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::current().frame())
        .title(title)
}

/// The help overlay content, generated from the active keymaps so it always
/// matches dispatch: the prefix table, then the sidebar's and the changes
/// panel's. Detach is listed first (it is what a stuck user needs). Every key
/// hint renders its key in the accent, its label dim.
fn help_lines(cfg: &Config) -> Vec<Line<'static>> {
    let prefix = cfg.prefix.label();
    let mut lines = vec![
        Line::from(format!("tutti — press {prefix}, then:")),
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
    lines.extend(two_columns(&entries));
    lines.push(Line::from(String::new()));
    lines.push(Line::from("direct keys (no prefix):".to_string()));
    lines.push(popup_key_line("  Ctrl+h/j/k/l", "focus by direction"));
    lines.push(popup_key_line("  Alt+h/j/k/l", "resize split"));
    lines.push(popup_key_line("  Alt+x", "kill pane"));
    lines.push(Line::styled(
        "  (a pane setting TUTTI_KEYS=pane gets these itself)",
        dim(),
    ));
    // The panels' own keys, each headed by the prefix key that focuses it.
    for (panel, focus, entries) in [
        (
            "sidebar",
            PrefixAction::Sidebar,
            key_groups(config::SIDEBAR_TABLE, false),
        ),
        (
            "changes",
            PrefixAction::DiffFiles,
            key_groups(config::CHANGES_TABLE, false),
        ),
    ] {
        let after = cfg.prefix_key(focus).map_or_else(String::new, |key| {
            format!(" (after {prefix} {})", config::key_label(key))
        });
        lines.push(Line::from(String::new()));
        lines.push(Line::from(format!("{panel}{after}:")));
        lines.extend(two_columns(&entries));
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(
        "stop the daemon:  tutti server stop".to_string(),
    ));
    lines.push(Line::from(String::new()));
    lines.push(Line::from("(any key closes)".to_string()));
    lines
}

/// Help entries two per row, keys in the accent and labels dim, each column
/// padded so the rows line up.
fn two_columns(entries: &[(String, &str)]) -> Vec<Line<'static>> {
    let key_w = entries
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let desc_w = entries
        .iter()
        .step_by(2)
        .map(|(_, desc)| desc.chars().count())
        .max()
        .unwrap_or(0);
    entries
        .chunks(2)
        .map(|pair| {
            let mut spans = vec![Span::raw(" ")];
            for (i, (key, desc)) in pair.iter().enumerate() {
                spans.push(Span::styled(
                    format!(" {key:<key_w$}"),
                    Style::default().fg(accent()),
                ));
                let pad = if i == 0 && pair.len() == 2 { desc_w } else { 0 };
                spans.push(Span::styled(format!(" {desc:<pad$} "), dim()));
            }
            Line::from(spans)
        })
        .collect()
}

/// A panel's bindings as help entries — `hint_only` keeps the ones the bottom
/// bar shows — neighbours sharing a description merged into one entry under
/// their joined keys: `j/k move`.
fn key_groups<A>(table: &[PanelBinding<A>], hint_only: bool) -> Vec<(String, &'static str)> {
    let mut groups: Vec<(String, &'static str)> = Vec::new();
    for binding in table.iter().filter(|b| b.hint || !hint_only) {
        match groups.last_mut() {
            Some((keys, desc)) if *desc == binding.desc => {
                keys.push('/');
                keys.push_str(&binding.key.label());
            }
            _ => groups.push((binding.key.label(), binding.desc)),
        }
    }
    groups
}

/// One popup hint line: the key in the accent colour, its label dim. Used by the
/// which-key and help panels.
fn popup_key_line(key: &str, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key}"), Style::default().fg(accent())),
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

/// The border badge. The name is dim-bold, followed by a dim `on <profile>`
/// for a pane running in a sandbox and the dim id of the issue it was
/// deployed on; a detected agent adds a dim `·` then the state in its colour
/// (a spinner precedes `working`); a plain shell shows just its name. An
/// exited pane appends a dim `exited <code>` plus the keys that act on the
/// corpse — `p` pulls a sandbox's pushed work — each in the accent, so the
/// way back to a dead agent's conversation is visible right where it died.
fn border_title(info: &PaneInfo, spinner: char) -> Line<'static> {
    let mut spans = vec![Span::styled(
        identity(info),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if let Some(profile) = &info.sandbox {
        spans.push(Span::styled(format!(" on {profile}"), dim()));
    }
    if let Some(issue) = &info.issue {
        spans.push(Span::styled(
            format!(" {}", issues::label(&issue.id)),
            dim(),
        ));
    }
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
        spans.push(Span::styled(format!(" exited {code} · "), dim()));
        spans.push(Span::styled("r", Style::default().fg(accent())));
        spans.push(Span::styled(" resume · ", dim()));
        if info.sandbox_name.is_some() {
            spans.push(Span::styled("p", Style::default().fg(accent())));
            spans.push(Span::styled(" pull · ", dim()));
        }
        spans.push(Span::styled("x", Style::default().fg(accent())));
        spans.push(Span::styled(" close", dim()));
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
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
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
        checkpoints: Vec::new(),
        approval: None,
        source: None,
        sandbox: None,
        issue: None,
        conversation: None,
        sandbox_name: None,
        user_vars: Default::default(),
    }
}

/// A pane's border style: accent (bold) when focused; red when an unfocused
/// pane's agent is blocked (the one place a state colour touches a border);
/// dim otherwise.
fn pane_border_style(focused: bool, state: Option<AgentState>) -> Style {
    if focused {
        Style::default().fg(accent()).add_modifier(Modifier::BOLD)
    } else if state == Some(AgentState::Blocked) {
        Style::default().fg(theme::current().blocked)
    } else {
        theme::current().frame()
    }
}

/// The theme's colour for each state. Blocked is the loud one: it is the pane
/// asking for the user.
fn state_style(state: AgentState) -> Style {
    let palette = theme::current();
    match state {
        AgentState::Blocked => Style::default()
            .fg(palette.blocked)
            .add_modifier(Modifier::BOLD),
        AgentState::Working => Style::default().fg(palette.working),
        AgentState::Done => Style::default().fg(palette.done),
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
                restorable: None,
                notes: Vec::new(),
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

    #[test]
    fn a_drag_selection_highlights_its_cells() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};
        let mut app = app_with_pane(b"HELLO");
        let area = Rect::new(0, 0, 40, 8);
        app.sync_sizes(area);
        // At 40 cols the sidebar is hidden, so pane 1 fills the width and its
        // interior top-left sits at (1, 4); drag the five cells of "HELLO".
        app.on_mouse(MouseEventKind::Down(MouseButton::Left), 1, 4);
        app.on_mouse(MouseEventKind::Drag(MouseButton::Left), 5, 4);

        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        for x in 1..=5u16 {
            assert_eq!(
                buf.cell((x, 4)).unwrap().bg,
                select_bg(),
                "selected cell {x} carries the highlight",
            );
        }
        assert_ne!(
            buf.cell((6, 4)).unwrap().bg,
            select_bg(),
            "the cell past the selection is not highlighted",
        );
    }

    /// A pane's grid is sized by its smallest viewer, so here it can be smaller
    /// than the pane: it draws from the top-left, blank beyond.
    #[test]
    fn a_grid_smaller_than_its_pane_draws_from_the_top_left() {
        let app = app_with_pane(b"HELLO\r\nWORLD"); // a 6x18 grid
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| {
            (1..39)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        };
        assert!(row(4).starts_with("HELLO") && row(5).starts_with("WORLD"));
        assert_eq!(row(4)[18..].trim(), "", "blank past the grid's columns");
        assert_eq!(row(10).trim(), "", "and past its rows");
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
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        app
    }

    #[test]
    fn sidebar_renders_the_tree_and_waiting_sections() {
        // 100 cols clears the width floor so the sidebar shows.
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("projects"),
            "projects header missing: {text:?}"
        );
        assert!(text.contains("waiting"), "waiting header missing: {text:?}");
        assert!(text.contains("api"), "workspace name missing: {text:?}");
        assert!(text.contains("main"), "branch subtitle missing: {text:?}");
        assert!(
            text.contains("└ ●"),
            "the agent nests under its project on a tree guide: {text:?}"
        );
        assert!(text.contains("claude"), "agent kind missing: {text:?}");
    }

    #[test]
    fn changed_files_panel_lists_paths_with_exact_counts() {
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
        view[0].changes = Some("2 files +13 −4".into());
        view[0].changed_files = vec![
            tutti_core::FileStat {
                path: "src/main.rs".into(),
                insertions: 12,
                deletions: 4,
            },
            tutti_core::FileStat {
                path: "README.md".into(),
                insertions: 1,
                deletions: 0,
            },
        ];
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "demo".into(),
                workspaces: view,
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("changes"), "panel header missing: {text:?}");
        assert!(
            text.contains("main.rs") && text.contains("+12 −4"),
            "file row with exact counts missing: {text:?}"
        );
        assert!(
            text.contains("README.md") && text.contains("+1 −0"),
            "second file row missing: {text:?}"
        );
    }

    #[test]
    fn clipped_path_keeps_the_basename_and_clips_the_directory_first() {
        assert_eq!(
            clipped_path("src/main.rs", 20),
            ("src/".into(), "main.rs".into())
        );
        assert_eq!(
            clipped_path("README.md", 20),
            ("".into(), "README.md".into())
        );
        // The directory clips from the left behind `…/`, dropping whole
        // components — never mid-name fragments; the basename survives.
        assert_eq!(
            clipped_path("crates/tutti/src/attach/app.rs", 16),
            ("…/attach/".into(), "app.rs".into())
        );
        assert_eq!(
            clipped_path("crates/tutti/src/attach/app.rs", 28),
            ("…/tutti/src/attach/".into(), "app.rs".into())
        );
        // Even when no whole component fits, the `…/` still marks the elision.
        assert_eq!(
            clipped_path("crates/tutti/src/attach/app.rs", 9),
            ("…/".into(), "app.rs".into())
        );
        // A basename that alone overflows clips at its end.
        let (dir, base) = clipped_path("averylongfilename.rs", 8);
        assert_eq!(dir, "");
        assert_eq!(base.chars().count(), 8);
        assert!(base.ends_with('…'));
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
                restorable: None,
                notes: Vec::new(),
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
                restorable: None,
                notes: Vec::new(),
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
    fn an_agentless_project_shows_no_agent_rows() {
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
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            !text.contains("AGENTS"),
            "no agents header, no placeholder — the tree shows reality: {text:?}"
        );
        assert!(
            text.contains("waiting"),
            "the waiting header still closes the tree: {text:?}"
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
                restorable: None,
                notes: Vec::new(),
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
            (c.bg == accent() && col < 30 && row >= 2).then_some(row)
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
        // 16 rows: the notified agent is another project's, so its bell rides
        // the waiting-section row below the (empty) filtered agents section.
        // Scoped to the sidebar column: the app bar's notes chip is a bell too.
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let sidebar: String = (2..16)
            .map(|row| row_text(buf, row).chars().take(30).collect::<String>())
            .collect();
        assert!(sidebar.contains("🔔"), "bell mark missing: {sidebar:?}");
    }

    fn note(id: u64, pane: u64, title: &str, kind: NoteKind) -> Note {
        Note {
            id,
            time: unix_now(),
            pane: PaneId(pane),
            pane_title: title.into(),
            workspace: "web".into(),
            kind,
            title: None,
            body: None,
            read: false,
        }
    }

    fn with_notes(app: &mut App, notes: Vec<Note>) {
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&tutti_core::Event::NotesChanged { notes }).unwrap(),
        ));
    }

    #[test]
    fn the_notifications_panel_lists_notes_newest_first_over_its_hint() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let prefix = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let open = KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT);
        app.on_key(prefix);
        app.on_key(open);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains(" notifications "), "the title: {text:?}");
        assert!(
            text.contains("no notifications"),
            "the empty history: {text:?}"
        );

        let built = Note {
            title: Some("build".into()),
            body: Some("Built".into()),
            read: true,
            ..note(2, 1, "zsh", NoteKind::Notify)
        };
        with_notes(
            &mut app,
            vec![
                note(1, 2, "agent", NoteKind::Blocked),
                built,
                note(3, 9, "old", NoteKind::Done),
            ],
        );
        // Off the newest note, so the gone pane's row draws unselected.
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        let text = buffer_text(buf);
        assert!(text.contains(" notifications · 2 unread "), "{text:?}");
        let (old, zsh, agent) = (
            row_containing(buf, "old"),
            row_containing(buf, "build: Built"),
            row_containing(buf, "agent  web · blocked"),
        );
        assert!(old < zsh && zsh < agent, "newest first: {text:?}");
        assert!(
            row_text(buf, zsh).contains("now  zsh  web · notify  build: Built"),
            "age, pane, where and what, then the text: {:?}",
            row_text(buf, zsh)
        );
        let dot_col = col_of(buf, agent, "agent") - 7;
        let dot = &buf.content()[agent * w + dot_col];
        assert_eq!(
            (dot.symbol(), dot.fg),
            ("●", Color::Red),
            "an unread blocked note leads with the blocked dot"
        );
        assert_eq!(
            buf.content()[zsh * w + dot_col].symbol(),
            " ",
            "a read note has no unread dot"
        );
        let name = &buf.content()[zsh * w + col_of(buf, zsh, "zsh")];
        assert_eq!(name.bg, accent(), "the highlighted note's pane is the chip");
        let gone = &buf.content()[old * w + col_of(buf, old, "old")];
        assert!(
            gone.modifier.contains(Modifier::DIM),
            "a gone pane's note draws dim"
        );
        assert!(
            row_text(buf, agent + 1).contains("enter jump"),
            "the hint row"
        );
        assert!(row_text(buf, 19).contains("NOTES"), "the footer mode chip");
    }

    #[test]
    fn the_app_bar_census_closes_with_the_unread_count() {
        let mut app = app_two_workspaces();
        let area = Rect::new(0, 0, 100, 12);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let (_, chip) = app.notes_chip(App::header_rects(area).0);
        let buf = terminal.backend().buffer();
        let bar = row_text(buf, 0);
        assert!(bar.contains("1 blocked · 🔔"), "after the states: {bar:?}");
        assert_eq!(
            col_of(buf, 0, "🔔"),
            chip.x as usize,
            "drawn where it clicks"
        );
        let count = &buf.content()[chip.x as usize + 3];
        assert_eq!(count.symbol(), "0");
        assert!(count.modifier.contains(Modifier::DIM), "dim at zero");

        with_notes(&mut app, vec![note(1, 2, "agent", NoteKind::Blocked)]);
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let count = &terminal.backend().buffer().content()[chip.x as usize + 3];
        assert_eq!(count.symbol(), "1");
        assert!(
            !count.modifier.contains(Modifier::DIM),
            "an unread note brings it to full intensity"
        );
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
            checkpoints: Vec::new(),
            approval: None,
            source: None,
            sandbox: None,
            issue: None,
            conversation: None,
            sandbox_name: None,
            user_vars: Default::default(),
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
            checkpoints: Vec::new(),
            approval: None,
            source: None,
            sandbox: None,
            issue: None,
            conversation: None,
            sandbox_name: None,
            user_vars: Default::default(),
        };
        assert_eq!(title_text(&info), "claude · ⠋ working");
    }

    #[test]
    fn border_title_names_the_sandbox_a_pane_runs_in() {
        let info = PaneInfo {
            sandbox: Some("gke".into()),
            ..crate::attach::fixtures::agent(1, "claude", AgentState::Blocked)
        };
        assert_eq!(title_text(&info), "claude on gke · blocked");
        let line = border_title(&info, '⠋');
        assert_eq!(
            line.spans[1].style,
            dim(),
            "the sandbox is metadata: dim, after the bright identity"
        );
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
            checkpoints: Vec::new(),
            approval: None,
            source: None,
            sandbox: None,
            issue: None,
            conversation: None,
            sandbox_name: None,
            user_vars: Default::default(),
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
            checkpoints: Vec::new(),
            approval: None,
            source: None,
            sandbox: None,
            issue: None,
            conversation: None,
            sandbox_name: None,
            user_vars: Default::default(),
        };
        assert_eq!(
            title_text(&info),
            "zsh exited 0 · r resume · x close",
            "the corpse advertises its two keys next to the exit code"
        );
    }

    #[test]
    fn border_title_offers_a_pull_on_an_exited_sandbox_pane() {
        let info = PaneInfo {
            sandbox: Some("gke".into()),
            sandbox_name: Some("tutti-p1-ab".into()),
            exited: Some(0),
            ..crate::attach::fixtures::agent(1, "claude", AgentState::Done)
        };
        assert_eq!(
            title_text(&info),
            "claude on gke · done exited 0 · r resume · p pull · x close"
        );
    }

    #[test]
    fn pane_border_style_reflects_focus_and_blocked_attention() {
        assert_eq!(
            pane_border_style(true, Some(AgentState::Idle)).fg,
            Some(accent()),
            "focused pane border is the accent"
        );
        assert_eq!(
            pane_border_style(false, Some(AgentState::Blocked)).fg,
            Some(Color::Red),
            "an unfocused blocked pane border turns red"
        );
        assert_eq!(
            pane_border_style(true, Some(AgentState::Blocked)).fg,
            Some(accent()),
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
        // The active tab is a bold accent block; everything else recedes dim.
        assert_eq!(tab_chip_style(true).bg, Some(accent()));
        assert_eq!(tab_chip_style(true).fg, Some(Color::Black));
        assert!(tab_chip_style(true).add_modifier.contains(Modifier::BOLD));
        assert!(tab_chip_style(false).add_modifier.contains(Modifier::DIM));
        assert_eq!(tab_chip_style(false).bg, None);
    }

    #[test]
    fn tab_segments_trail_their_most_urgent_agents_dot_and_click_true() {
        use crate::attach::app::control;
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};
        use tutti_core::{Request, WorkspaceId};
        let claude = |id, state| pane(id, "agent", Some("claude"), state);
        let mut exited = pane(2, "zsh", None, AgentState::Done);
        exited.exited = Some(0);
        let mut app = App::new();
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                restorable: None,
                notes: Vec::new(),
                session: "demo".into(),
                workspaces: vec![workspace(
                    1,
                    "w",
                    None,
                    vec![
                        tab(
                            1,
                            "sh",
                            false,
                            leaf(1),
                            vec![pane(1, "zsh", None, AgentState::Idle), exited],
                        ),
                        tab(2, "w", false, leaf(3), vec![claude(3, AgentState::Working)]),
                        tab(
                            3,
                            "d",
                            false,
                            leaf(4),
                            vec![claude(4, AgentState::Working), claude(5, AgentState::Done)],
                        ),
                        tab(
                            4,
                            "b",
                            true,
                            leaf(6),
                            vec![
                                claude(6, AgentState::Done),
                                claude(7, AgentState::Blocked),
                                claude(8, AgentState::Working),
                            ],
                        ),
                    ],
                )],
            })
            .unwrap(),
        ));
        let area = Rect::new(0, 0, 120, 12);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let cell = |x: u16| buf.cell((x, 0)).unwrap();
        // Walk the segments by `TabChip::width`: each draws its label, a badged
        // one then its dot and a pad, and the next starts past one separator.
        let mut x = app.tab_bar_rect(Rect::new(0, 0, area.width, 1)).x;
        let mut after_label = Vec::new();
        let mut segments = Vec::new();
        for chip in app.tab_chips() {
            let label_end = x + chip.label.chars().count() as u16;
            let drawn: String = (x..label_end).map(|c| cell(c).symbol()).collect();
            assert_eq!(drawn, chip.label);
            if chip.target.is_some() {
                let next = cell(label_end);
                after_label.push((next.symbol().to_string(), next.fg, next.bg));
            }
            if chip.badge.is_some() {
                assert_eq!(cell(label_end + 1).symbol(), " ", "a pad closes {chip:?}");
            }
            segments.push((chip.target, x + chip.width() - 1));
            x += chip.width() + 1;
        }
        assert_eq!(
            x - 1,
            area.width,
            "the segments fill the bar to its right edge"
        );
        let spinner = &after_label[1].0;
        assert!(
            spinner
                .chars()
                .all(|c| ('\u{2800}'..='\u{28ff}').contains(&c)),
            "a working tab spins the shared braille spinner: {spinner:?}"
        );
        assert_eq!(after_label[0].0, " ", "a tab of shells carries no dot");
        assert_eq!(
            after_label[1..4]
                .iter()
                .map(|(_, fg, _)| *fg)
                .collect::<Vec<_>>(),
            vec![Color::Yellow, Color::Green, Color::Red],
            "working alone spins; done beats working; blocked beats both"
        );
        assert_eq!(
            (after_label[3].0.as_str(), after_label[3].2),
            ("●", accent()),
            "the active block's ground runs under its dot"
        );
        assert_eq!(
            after_label[2].2,
            Color::Reset,
            "an inactive dot has no ground"
        );
        // A click on a segment's last column — a badged one's pad — selects it.
        app.sync_sizes(area);
        for (target, last) in segments {
            let out = app.on_mouse(MouseEventKind::Down(MouseButton::Left), last, 0);
            let want = match target {
                Some(id) => Request::TabSelect { id },
                None => Request::TabNew {
                    workspace: Some(WorkspaceId(1)),
                },
            };
            assert_eq!(out, vec![control(&want)], "column {last}");
        }
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
        assert_eq!(line.spans[0].style.fg, Some(accent()), "key span is accent");
        assert_eq!(line.spans[1].style, dim(), "label span is dim");
        // The footer hint separates pairs with two plain spaces — proximity
        // groups a key with its label, no separator glyph.
        let (spans, w) = two_tone(&[("a", "one"), ("b", "two")]);
        assert!(
            spans.iter().any(|s| s.content == "  "),
            "pairs are separated by two spaces"
        );
        assert!(
            !spans.iter().any(|s| s.content.contains('·')),
            "no separator glyph between pairs"
        );
        assert_eq!(w, 12, "a one  b two");
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

        // Tall enough to clear the whole panel, including the sidebar and
        // changes sections.
        let mut terminal = Terminal::new(TestBackend::new(64, 44)).unwrap();
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
        assert!(
            text.contains("TUTTI_KEYS=pane"),
            "help missing the direct keys' hand-off: {text:?}"
        );
        // The sidebar section documents the workspace, merge, update-stale, and
        // kill keys.
        let sidebar = &text[text.find("sidebar (after C-b w)").expect("sidebar section")..];
        assert!(
            sidebar.contains("new workspace")
                && sidebar.contains("merge")
                && sidebar.contains("update stale")
                && sidebar.contains("kill"),
            "help missing the sidebar workspace/merge/update/kill keys: {text:?}"
        );
        let changes = &text[text.find("changes (after C-b f)").expect("changes section")..];
        assert!(
            text.contains("changed files")
                && changes.contains("full diff")
                && changes.contains("restore turn")
                && changes.contains("half page"),
            "help missing the changed-files panel keys: {text:?}"
        );
        // Detach is listed before the split bindings.
        assert!(
            text.find("detach") < text.find("split"),
            "detach should be listed first: {text:?}"
        );
    }

    #[test]
    fn the_restore_offer_floats_its_summary_and_keys_over_everything() {
        let mut app = App::new();
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "demo".into(),
                workspaces: Vec::new(),
                restorable: Some(tutti_core::RestoreSummary {
                    workspaces: 3,
                    panes: 7,
                    agents: 2,
                    saved_at: unix_now() - 2 * 3600,
                }),
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        for needle in [
            " restore last session? ",
            "3 projects · 7 panes · 2 agents to revive · saved 2h ago",
            "enter restore  d discard  esc later",
            " RESTORE ",
        ] {
            assert!(text.contains(needle), "{needle:?} missing: {text:?}");
        }
    }

    #[test]
    fn sidebar_workspace_prompt_walks_from_name_to_where() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 100, 20));
        // `C-b w` focuses the sidebar; a second `w` opens the guided create —
        // step 1 is the name field.
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("workspace name:"),
            "step 1 shows the name prompt: {text:?}"
        );

        // A valid name advances to step 2, the prefilled `where:` field.
        for c in "feature".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("where:"),
            "step 2 shows the destination prompt: {text:?}"
        );
    }

    #[test]
    fn sidebar_renders_a_nested_child_with_its_guide() {
        let mut app = App::with_config(Config::parse("sidebar = \"on\"\n").unwrap());
        let mut view = vec![
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
                "feature",
                Some("main"),
                vec![tab(
                    2,
                    "2",
                    false,
                    leaf(2),
                    vec![pane(2, "zsh", None, AgentState::Idle)],
                )],
            ),
        ];
        view[1].parent = Some(tutti_core::WorkspaceId(1)); // feature nested under api
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "demo".into(),
                workspaces: view,
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("feature"), "the child name renders: {text:?}");
        // The child's row carries the end-guide glyph, scoped to the sidebar rows.
        assert!(
            text.contains('└'),
            "the nested child shows a tree guide: {text:?}"
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
        let active = buf.content()[0..w].iter().any(|c| c.bg == accent());
        assert!(active, "the active tab segment should be an accent block");
    }

    #[test]
    fn sidebar_frame_headers_carry_their_counts() {
        // api (no agent) + web (one blocked claude): two projects, one agent.
        let app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        // The projects header is the sidebar's first row, on content row 2 —
        // scoped to the 30-column sidebar (the pane area sits to its right).
        let top: String = row_text(buf, 2).chars().take(30).collect();
        assert!(top.contains("projects"), "projects header: {top:?}");
        assert!(
            top.trim_end().ends_with('2'),
            "projects count right-aligned in the header: {top:?}"
        );
        // The waiting header carries its own count below (web's blocked agent).
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("waiting"), "waiting header: {text:?}");
        assert!(
            text.contains("waiting                  1"),
            "waiting count right-aligned in the header: {text:?}"
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
            buf.content()[15 * w..16 * w]
                .iter()
                .any(|c| c.bg == accent()),
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
            buf.content()[8 * w..9 * w].iter().all(|c| c.bg == accent()),
            "an info transient paints the band with the accent"
        );
    }

    #[test]
    fn selection_highlight_uses_an_ansi_palette_colour() {
        // The chrome carries no background of its own — it draws over the
        // terminal's default so it tracks the terminal's light/dark scheme. The
        // one background it does paint, the selected sidebar row, must be an ANSI
        // palette colour (here DarkGray) and never truecolor RGB, so it too is
        // repainted by the terminal when the scheme flips.
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 100, 20));
        // Prefix then `w` focuses the sidebar, popping the selection highlight.
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let cells = terminal.backend().buffer().content();
        assert!(
            cells.iter().any(|c| c.bg == select_bg()),
            "the selected sidebar row should carry the ANSI selection highlight"
        );
        // The app bar is pure chrome across its full width: it must paint no
        // background of its own (terminal default) — never a truecolor shade — so
        // it tracks the terminal's scheme.
        let w = 100usize;
        assert!(
            cells[0..w]
                .iter()
                .all(|c| !matches!(c.bg, Color::Rgb(_, _, _))),
            "the app bar must not paint a truecolor background shade"
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
                restorable: None,
                notes: Vec::new(),
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

    /// The issues panel over `w`: GitHub's two issues landed, Linear failed.
    fn issues_app() -> App {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let config = Config::parse(
            "[[issues]]\nname = \"gh\"\nkind = \"github\"\n\n\
             [[issues]]\nname = \"linear\"\nkind = \"linear\"\n",
        )
        .unwrap();
        let mut app = App::with_config(config);
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
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT));
        let (generation, _, _) = app.take_issue_fetch().unwrap();
        let issue = |id: &str, title: &str, labels: &[&str]| Issue {
            source: "gh".into(),
            id: id.into(),
            title: title.into(),
            body: String::new(),
            url: Some(format!("https://github.com/acme/w/issues/{id}")),
            labels: labels.iter().map(|l| l.to_string()).collect(),
        };
        app.issues_loaded(
            generation,
            0,
            Ok(vec![
                issue("123", "Fix the login redirect", &["bug"]),
                issue("120", "Docs typo", &[]),
            ]),
        );
        app.issues_loaded(
            generation,
            1,
            Err("LINEAR_API_KEY is not set (a Linear personal API key)".into()),
        );
        app
    }

    #[test]
    fn the_issues_panel_lists_each_sources_issues_over_its_keys() {
        let app = issues_app();
        let mut terminal = Terminal::new(TestBackend::new(110, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let text = buffer_text(buf);
        assert!(text.contains(" issues · w "), "the title: {text:?}");
        let top = row_containing(buf, "#123");
        let row = row_text(buf, top);
        assert!(
            row.contains(
                "❯ [gh] #123  Fix the login redirect  bug  https://github.com/acme/w/issues/123"
            ),
            "the highlighted issue, its labels and url: {row:?}"
        );
        let x = col_of(buf, top, "#123") as u16;
        assert_eq!(
            buf.cell((x, top as u16)).unwrap().fg,
            accent(),
            "one accent"
        );
        assert!(row_text(buf, top + 1).contains("   [gh] #120  Docs typo"));
        let failed = row_containing(buf, "[linear]");
        assert!(row_text(buf, failed).contains("LINEAR_API_KEY is not set"));
        let x = col_of(buf, failed, "LINEAR") as u16;
        assert_eq!(
            buf.cell((x, failed as u16)).unwrap().fg,
            theme::current().blocked,
            "a failed source reads dim-red"
        );
        assert!(
            text.contains("enter deploy here")
                && text.contains("/ filter")
                && text.contains("esc close"),
            "the keys, here because `w` is no jj repo: {text:?}"
        );
        assert!(!text.contains("h new workspace"), "nothing for h to toggle");
        assert!(text.contains(" ISSUES "), "the footer's mode chip");
    }

    #[test]
    fn an_issue_linked_agent_shows_its_issue_in_the_sidebar_and_pane_title() {
        let mut app = app_two_workspaces();
        let mut view = app.workspaces.clone();
        view[1].tabs[0].panes[0].issue = Some(Box::new(tutti_core::IssueRef {
            source: "linear".into(),
            id: "ENG-42".into(),
            title: "Fix the login redirect".into(),
            url: None,
        }));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&tutti_core::Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let row = row_containing(buf, "agent ENG-42");
        let x = col_of(buf, row, "ENG-42") as u16;
        assert!(
            buf.cell((x, row as u16))
                .unwrap()
                .modifier
                .contains(Modifier::DIM),
            "the issue id is metadata: dim beside the bright title"
        );

        let info = PaneInfo {
            issue: Some(Box::new(tutti_core::IssueRef {
                source: "gh".into(),
                id: "123".into(),
                title: "Fix it".into(),
                url: None,
            })),
            ..crate::attach::fixtures::agent(1, "claude", AgentState::Blocked)
        };
        assert_eq!(title_text(&info), "claude #123 · blocked");
    }

    #[test]
    fn launcher_title_names_the_target_workspace() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 100, 20));
        // Focus the sidebar and start a new agent in the selected (first)
        // workspace, `api`.
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Launcher);
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("run in api"),
            "the launcher panel title names the target workspace: {text:?}"
        );
    }

    #[test]
    fn peek_overlay_shows_the_blocked_prompt_over_its_hint() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        app.handle_frame(WireFrame::PaneSnapshot(PaneData {
            pane: PaneId(2),
            rows: 10,
            cols: 50,
            seq: 0,
            bytes: b"old transcript\r\n\r\nDo you want to proceed?\r\n> 1. Yes\r\n  2. No".to_vec(),
        }));
        let area = Rect::new(0, 0, 100, 24);
        app.sync_sizes(area);
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        // api, web, web's agent, then the agent's waiting row.
        for _ in 0..3 {
            app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Peek(PaneId(2)));

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let text = buffer_text(buf);
        assert!(text.contains("answer claude · web"), "title: {text:?}");
        let (_, popup, _) = app.peek_popup(area).unwrap();
        let inside = |needle: &str| {
            let row = row_containing(buf, needle) as u16;
            let col = col_of(buf, row as usize, needle) as u16;
            row > popup.y && row < popup.y + popup.height && col > popup.x
        };
        assert!(inside("Do you want to proceed?"), "the prompt: {text:?}");
        assert!(inside("2. No"), "its options: {text:?}");
        assert!(inside("jump in to type"), "the hint row: {text:?}");
        assert!(row_text(buf, 23).contains("ANSWER"), "the footer mode chip");
    }

    /// A held approval's waiting row names what would run, and its overlay
    /// — framed as the peek is — puts the tool over its input above the
    /// three answers, the footer chip saying APPROVE.
    #[test]
    fn a_held_approval_shows_in_its_waiting_row_and_its_overlay() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        let mut view = app.workspaces.clone();
        view[1].tabs[0].panes[0].approval = Some(tutti_core::PendingApproval {
            id: 7,
            tool: "Bash".into(),
            summary: "rm -rf build".into(),
        });
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&tutti_core::Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        let area = Rect::new(0, 0, 100, 24);
        app.sync_sizes(area);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("Bash · rm -rf build"),
            "the waiting row: {text:?}"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        // api, web, web's agent, then the agent's waiting row.
        for _ in 0..3 {
            app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Approval(PaneId(2)));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let (_, popup, _) = app.approval_popup(area).unwrap();
        let row = |n: u16| row_text(buf, usize::from(popup.y + n));
        assert!(row(0).contains("approve claude · web"), "{:?}", row(0));
        assert!(row(1).contains(" Bash "), "the tool: {:?}", row(1));
        assert!(row(2).contains(" rm -rf build "), "its input: {:?}", row(2));
        assert!(
            row(3).contains("y allow  n deny  esc pass to claude"),
            "the answers: {:?}",
            row(3)
        );
        assert!(
            row_text(buf, 23).contains("APPROVE"),
            "the footer mode chip"
        );
    }

    /// `app_two_workspaces` with the go-to overlay open: its rows are the
    /// needs-you header over web's blocked agent, a spacer, then the projects
    /// header over api, tab 1, zsh, web, tab 2, and web's agent again.
    fn navigating() -> App {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Navigate);
        app
    }

    #[test]
    fn navigate_overlay_draws_the_groups_over_the_tree_with_one_accent_row() {
        let app = navigating();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = 100usize;
        // The overlay is the content area, rows 2-14: the input and its hint,
        // a spacer, then the list.
        assert!(row_text(buf, 2).contains("go to"), "{:?}", row_text(buf, 2));
        assert!(row_text(buf, 2).contains("enter jump"), "the key hint");
        let header = row_text(buf, 4);
        assert!(
            header.contains("needs you") && header.trim_end().ends_with('1'),
            "the header and its count: {header:?}"
        );
        let label = &buf.content()[4 * w + col_of(buf, 4, "needs")];
        assert!(
            label.modifier.contains(Modifier::BOLD) && label.modifier.contains(Modifier::DIM),
            "headers are the sidebar's bold-dim labels"
        );
        let agent = row_text(buf, 5);
        assert!(
            agent.contains("agent  web · 2 · claude"),
            "the title, then where it lives and its kind: {agent:?}"
        );
        let gutter = &buf.content()[5 * w];
        assert_eq!(
            (gutter.symbol(), gutter.fg, gutter.bg),
            ("▎", accent(), select_bg()),
            "the highlighted row leads with the accent gutter"
        );
        assert_eq!(
            buf.content()[5 * w + col_of(buf, 5, "●")].fg,
            Color::Red,
            "the sidebar's blocked dot"
        );
        assert_eq!(
            buf.content()[5 * w + col_of(buf, 5, "agent")].bg,
            accent(),
            "the name chip"
        );
        assert_eq!(
            (2..15)
                .filter(|r| buf.content()[r * w..(r + 1) * w]
                    .iter()
                    .any(|c| c.bg == select_bg()))
                .count(),
            1,
            "one highlighted row, nothing of the sidebar beneath"
        );
        assert!(row_text(buf, 7).contains("projects"));
        assert!(row_text(buf, 13).contains("└ ● agent  claude"), "the tree");
        assert!(row_text(buf, 15).contains("GO TO"), "the footer mode chip");
    }

    #[test]
    fn navigate_overlay_keeps_the_highlight_in_view_and_says_when_nothing_matches() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = navigating();
        for _ in 0..6 {
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        // Ten rows leave the list five below the input: the tree's last row,
        // highlighted, scrolls up to the window's foot.
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(
            row_text(buf, 8).starts_with('▎') && row_text(buf, 8).contains("agent"),
            "{:?}",
            buffer_text(buf)
        );
        assert!(
            !buffer_text(buf).contains("needs you"),
            "the top scrolled off"
        );

        for c in "zzz".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(buffer_text(terminal.backend().buffer()).contains("no matches"));
    }

    #[test]
    fn sidebar_footer_hint_offers_workspace_and_merge_and_drops_move() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_two_workspaces();
        // A wide terminal so the full-width footer hint renders untruncated.
        let mut terminal = Terminal::new(TestBackend::new(140, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 140, 16));
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let footer = row_text(terminal.backend().buffer(), 15);
        assert!(
            footer.contains("new agent") && footer.contains("add project"),
            "the sidebar hint offers the new-agent and add-project verbs: {footer:?}"
        );
        assert!(
            footer.contains("workspace") && footer.contains("merge"),
            "and offers the workspace and merge verbs: {footer:?}"
        );
        assert!(
            !footer.contains("fork"),
            "and drops the banned `fork` word: {footer:?}"
        );
        assert!(
            !footer.contains("move"),
            "and drops the table-stakes j/k move pair: {footer:?}"
        );
    }

    #[test]
    fn launcher_command_input_shows_the_run_prompt() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_with_pane(b"hi");
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        // Find the command row, whatever this machine has on PATH.
        use crate::attach::launcher::LaunchKind;
        let command_number = app
            .launcher_rows()
            .iter()
            .position(|r| matches!(r.kind, LaunchKind::Command))
            .unwrap()
            + 1;
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

    /// `app_with_pane` with the setup panel open over a fixture home holding
    /// the tutti skill, someone else's opencode plugin, and no Claude
    /// settings. Returns the app and the home.
    fn setup_app() -> (App, std::path::PathBuf) {
        use crate::attach::fixtures::{ctrl, plain};
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let home =
            std::env::temp_dir().join(format!("tutti-render-setup-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for (path, text) in [
            (".claude/skills/tutti/SKILL.md", crate::integrations::SKILL),
            (
                ".config/opencode/plugins/tutti.js",
                "export const Mine = 1\n",
            ),
        ] {
            let path = home.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let mut app = app_with_pane(b"hi");
        app.setup_dirs = crate::integrations::Dirs {
            home: Some(home.clone()),
            ..Default::default()
        };
        app.on_key(ctrl('b'));
        app.on_key(plain('i'));
        assert_eq!(app.mode, Mode::Integrations);
        (app, home)
    }

    #[test]
    fn setup_panel_lists_each_integration_with_its_status_dot_and_target() {
        let (app, home) = setup_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        assert!(
            buffer_text(buf).contains(" setup · user "),
            "the scope titles it"
        );
        let palette = theme::current();
        for (label, status, file) in [
            (
                "Claude Code hooks",
                "not installed",
                ".claude/settings.json",
            ),
            (
                "opencode plugin",
                "not tutti's",
                ".config/opencode/plugins/tutti.js",
            ),
            ("tutti skill", "installed", ".claude/skills/tutti/SKILL.md"),
        ] {
            let row = row_containing(buf, label);
            let text = row_text(buf, row);
            assert!(text.contains(&format!("  {status} ")), "{text:?}");
            // A long temp-dir path clips from the left, keeping the file.
            let (dir, base) = clipped_path(&home.join(file).display().to_string(), SETUP_PATH_W);
            let beneath = row_text(buf, row + 1);
            assert!(
                beneath.contains(&format!("{dir}{base}")) && beneath.contains(file),
                "the target file sits beneath: {beneath:?}"
            );
            let dot = &buf.content()[row * w + col_of(buf, row, label) - 2];
            match status {
                "not installed" => assert!(dot.modifier.contains(Modifier::DIM)),
                "not tutti's" => assert_eq!(dot.fg, palette.blocked),
                _ => assert_eq!(dot.fg, palette.done),
            }
        }
        let hooks = row_containing(buf, "Claude Code hooks");
        assert!(
            row_text(buf, hooks).contains('❯'),
            "the first row is highlighted"
        );
        assert!(
            buffer_text(buf).contains("enter install  s project scope  esc close"),
            "the panel's own key hint"
        );
        assert!(row_text(buf, 23).contains("SETUP"), "the footer mode chip");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn setup_confirm_asks_with_the_change_summary_and_the_preview_head() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, home) = setup_app();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.setup_confirming());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("install Claude Code hooks? y/N"), "{text:?}");
        assert!(text.contains("adding tutti hooks for "), "the summary");
        assert!(text.contains("--- hooks before ---"), "the preview's head");
        assert!(text.contains("more lines"), "and how much of it is cut");
        assert!(
            !text.contains("opencode plugin"),
            "the question holds the panel"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn help_overlay_renders_each_panel_section_from_its_table() {
        use crate::attach::fixtures::{ctrl, plain};
        let mut app = app_with_pane(b"hi");
        app.on_key(ctrl('b'));
        app.on_key(plain('?'));
        let mut terminal = Terminal::new(TestBackend::new(64, 44)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let sidebar = row_containing(buf, "sidebar (after C-b w):");
        let changes = row_containing(buf, "changes (after C-b f):");
        let stop = row_containing(buf, "tutti server stop");
        for (rows, groups) in [
            (
                sidebar + 1..changes,
                key_groups(config::SIDEBAR_TABLE, false),
            ),
            (changes + 1..stop, key_groups(config::CHANGES_TABLE, false)),
        ] {
            for (keys, desc) in groups {
                assert!(
                    rows.clone().any(|row| {
                        let text = row_text(buf, row);
                        text.contains(&format!(" {keys} ")) && text.contains(desc)
                    }),
                    "{keys} {desc} missing from rows {rows:?}: {:?}",
                    buffer_text(buf)
                );
            }
        }
        assert!(
            row_text(buf, sidebar + 1).contains(" j/k/↓/↑ move "),
            "keys sharing a description share an entry"
        );
    }

    #[test]
    fn panel_footer_hints_are_their_tables_hint_rows() {
        use crate::attach::fixtures::{ctrl, plain};
        let mut app = changes_app(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 160, 20));
        app.on_key(ctrl('b'));
        app.on_key(plain('w'));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let footer = row_text(terminal.backend().buffer(), 19);
        assert!(
            footer.contains(
                "enter jump  p peek  n new agent  w new workspace  a add project  \
                 d diff  m merge  u update stale  x kill  esc back"
            ),
            "{footer:?}"
        );

        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let footer = row_text(terminal.backend().buffer(), 19);
        assert!(
            footer.contains("enter diff  d full diff  r restore turn  j/k move  esc back"),
            "{footer:?}"
        );
    }

    /// An attached app whose one workspace carries two changed files (16 and 1
    /// lines), so the changes panel and its stat surfaces all render.
    fn changes_app(config: Config) -> App {
        let mut app = App::with_config(config);
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
        view[0].changes = Some("2 files +13 −4".into());
        view[0].changed_files = vec![
            tutti_core::FileStat {
                path: "src/main.rs".into(),
                insertions: 12,
                deletions: 4,
            },
            tutti_core::FileStat {
                path: "README.md".into(),
                insertions: 1,
                deletions: 0,
            },
        ];
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: tutti_core::WIRE_REV,
                session: "demo".into(),
                workspaces: view,
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        app
    }

    /// The row index of the first buffer row whose text contains `needle`.
    fn row_containing(buf: &Buffer, needle: &str) -> usize {
        (0..buf.area.height as usize)
            .find(|r| row_text(buf, *r).contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}: {:?}", buffer_text(buf)))
    }

    /// The column at which `needle` starts on buffer row `row`.
    fn col_of(buf: &Buffer, row: usize, needle: &str) -> usize {
        let text = row_text(buf, row);
        let at = text
            .find(needle)
            .unwrap_or_else(|| panic!("row {row} lacks {needle:?}: {text:?}"));
        text[..at].chars().count()
    }

    /// The count of bar cells on `row` in the added and removed colours.
    fn bar_cells(buf: &Buffer, row: usize) -> (usize, usize) {
        let w = buf.area.width as usize;
        let palette = theme::current();
        let cells = &buf.content()[row * w..(row + 1) * w];
        let count = |colour: Color| {
            cells
                .iter()
                .filter(|c| c.symbol() == "█" && c.fg == colour)
                .count()
        };
        (count(palette.added), count(palette.removed))
    }

    #[test]
    fn dark_theme_lays_the_panel_and_pane_grounds() {
        let app = changes_app(Config::parse("theme = \"dark\"\n").unwrap());
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = 140usize;
        let cells = buf.content();
        // The app bar, sidebar, changes panel, and footer sit on the panel shade.
        for (row, col) in [(0, 5), (5, 2), (5, 130), (19, 5)] {
            assert_eq!(
                cells[row * w + col].bg,
                theme::DARK.panel_bg.unwrap(),
                "chrome at ({row},{col}) is on the panel shade"
            );
        }
        // A pane's interior sits on the pane ground, its default text on the
        // theme foreground.
        assert_eq!(cells[5 * w + 60].bg, theme::DARK.pane_bg.unwrap());
        assert_eq!(cells[5 * w + 60].fg, theme::DARK.fg.unwrap());
        // Metadata is an explicit muted colour — the DIM attribute is the
        // terminal theme's device.
        assert!(
            cells.iter().all(|c| !c.modifier.contains(Modifier::DIM)),
            "a truecolor theme never leans on DIM"
        );
    }

    #[test]
    fn terminal_theme_paints_no_truecolor_ground_anywhere() {
        let app = changes_app(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let cells = terminal.backend().buffer().content();
        assert!(
            cells.iter().all(|c| !matches!(c.bg, Color::Rgb(..))),
            "the terminal theme leaves every ground to the terminal"
        );
        assert!(
            cells.iter().any(|c| c.modifier.contains(Modifier::DIM)),
            "metadata dims with the attribute so the terminal decides its shade"
        );
    }

    #[test]
    fn changes_panel_shows_coloured_totals_and_bars_scaled_to_the_largest_change() {
        let app = changes_app(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let header = row_containing(buf, "changes");
        let w = 140usize;
        assert!(
            row_text(buf, header).contains("2 · +13 −4"),
            "the header carries the count and totals: {:?}",
            row_text(buf, header)
        );
        // main.rs is the largest change (16 lines): a full six-cell bar, five
        // added to one removed. README.md (1 line) still gets one added cell.
        assert_eq!(bar_cells(buf, row_containing(buf, "main.rs")), (5, 1));
        assert_eq!(bar_cells(buf, row_containing(buf, "README.md")), (1, 0));
        // The exact counts wear the diff colours.
        let main_row = row_containing(buf, "main.rs");
        let plus = col_of(buf, main_row, "+12");
        assert_eq!(buf.content()[main_row * w + plus].fg, theme::TERMINAL.added);
    }

    #[test]
    fn changes_panel_lists_the_agents_turns_newest_first_under_the_files() {
        let mut app = changes_app(Config::default());
        let turn = |n: u64, from: Option<&str>, label: &str| Checkpoint {
            n,
            commit: format!("c{n}"),
            from: from.map(Into::into),
            time: unix_now(),
            label: label.into(),
            prompt: None,
            files: 1,
            insertions: 3,
            deletions: 1,
        };
        let mut view = app.workspaces.clone();
        view[0].tabs[0].panes[0] = pane(1, "zsh", Some("claude"), AgentState::Done);
        view[0].tabs[0].panes[0].checkpoints =
            vec![turn(1, None, "start"), turn(2, Some("c1"), "done")];
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&tutti_core::Event::LayoutChanged { workspaces: view }).unwrap(),
        ));
        use crate::attach::fixtures::{ctrl, plain};
        app.sync_sizes(Rect::new(0, 0, 140, 20));
        app.on_key(ctrl('b'));
        app.on_key(plain('f'));
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = 140usize;

        let header = row_containing(buf, "turns");
        assert!(
            header > row_containing(buf, "README.md"),
            "turns follow the files"
        );
        assert!(row_text(buf, header).trim_end().ends_with('2'), "the count");
        let newest = row_containing(buf, "#2");
        assert_eq!(newest, header + 1, "the newest turn lists first");
        assert!(row_text(buf, newest).contains("now done"));
        let plus = col_of(buf, newest, "+3");
        assert_eq!(buf.content()[newest * w + plus].fg, theme::TERMINAL.added);
        let start = row_text(buf, row_containing(buf, "#1"));
        assert!(
            start.contains("start") && !start.contains("+3"),
            "the first state has no diff to count: {start:?}"
        );
        assert!(
            buffer_text(buf).contains("r restore turn"),
            "the footer hint offers the restore key"
        );
    }

    #[test]
    fn a_turn_row_reads_as_the_prompt_that_started_it() {
        let now = unix_now();
        let turn = Checkpoint {
            n: 4,
            commit: "c4".into(),
            from: Some("c3".into()),
            time: now,
            label: "done".into(),
            prompt: Some("fix the failing sidebar test in the resume rows".into()),
            files: 2,
            insertions: 14,
            deletions: 3,
        };
        let row = |w: usize| -> String {
            turn_inner(&turn, now, false, w)
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        let wide = row(70);
        assert!(
            wide.starts_with("#4 now fix the failing sidebar test in the resume rows")
                && !wide.contains("done"),
            "the prompt stands in for the label: {wide:?}"
        );
        let narrow = row(30);
        assert!(
            narrow.starts_with("#4 now fix the") && narrow.trim_end().ends_with("+14 −3"),
            "a long prompt clips, the stat stays: {narrow:?}"
        );
    }

    #[test]
    fn stat_bar_keeps_both_sides_visible_and_never_empties_a_change() {
        let widths = |spans: Vec<Span<'static>>| -> Vec<usize> {
            spans.iter().map(|s| s.content.chars().count()).collect()
        };
        assert_eq!(
            widths(stat_bar(0, 0, 10)),
            vec![BAR_W],
            "no change, blank bar"
        );
        assert_eq!(
            widths(stat_bar(10, 0, 10)),
            vec![6, 0, 0],
            "all added, full"
        );
        assert_eq!(
            widths(stat_bar(1, 99, 100)),
            vec![1, 5, 0],
            "a sliver of added still shows"
        );
        assert_eq!(
            widths(stat_bar(99, 1, 100)),
            vec![5, 1, 0],
            "a sliver of removed still shows"
        );
        assert_eq!(
            widths(stat_bar(1, 0, 100)),
            vec![1, 0, 5],
            "the smallest change keeps one cell"
        );
    }

    #[test]
    fn stat_spans_colour_polarity_and_dim_zero_counts() {
        let spans = stat_spans("3 files +13 −0");
        let by_text = |t: &str| spans.iter().find(|s| s.content == t).unwrap().style;
        assert_eq!(by_text("+13").fg, Some(theme::TERMINAL.added));
        assert!(
            by_text("−0").add_modifier.contains(Modifier::DIM),
            "a zero count stays dim"
        );
        assert!(by_text("files").add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn focused_panel_header_and_selected_row_take_the_accent() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = changes_app(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 140, 20));
        let buf = terminal.backend().buffer();
        let w = 140usize;
        let header = row_containing(buf, "changes");
        let label = col_of(buf, header, "changes");
        assert_ne!(
            buf.content()[header * w + label].fg,
            accent(),
            "at rest the header is dim"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::DiffFiles);
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(
            buf.content()[header * w + label].fg,
            accent(),
            "focus paints the header accent"
        );
        let selected = row_containing(buf, "main.rs");
        let gutter = &buf.content()[selected * w + 102];
        assert_eq!(
            (gutter.symbol(), gutter.fg),
            ("▎", accent()),
            "the selected row leads with an accent gutter mark"
        );
        assert_eq!(gutter.bg, select_bg());
    }

    #[test]
    fn a_focused_panel_wears_an_accent_rail_down_its_left_edge() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = changes_app(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        app.sync_sizes(Rect::new(0, 0, 140, 20));
        let w = 140usize;
        // The changes panel starts at column 102; content rows run 2..=18.
        let rail = |buf: &Buffer, col: usize| -> usize {
            (2..19)
                .filter(|row| {
                    let c = &buf.content()[row * w + col];
                    c.fg == accent() && (c.symbol() == "▏" || c.symbol() == "▎")
                })
                .count()
        };
        assert_eq!(rail(terminal.backend().buffer(), 102), 0, "no rail at rest");
        assert_eq!(rail(terminal.backend().buffer(), 0), 0);

        app.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, Mode::DiffFiles, "ctrl-l steps into the panel");
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert_eq!(
            rail(terminal.backend().buffer(), 102),
            17,
            "the whole panel edge is accent"
        );
        assert_eq!(
            rail(terminal.backend().buffer(), 0),
            0,
            "the sidebar stays quiet"
        );

        // Prefix-w from inside the panel hops to the sidebar; the rail follows.
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(
            app.sidebar_focused(),
            "prefix w reaches across from the panel"
        );
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert_eq!(
            rail(terminal.backend().buffer(), 0),
            17,
            "focus moves the rail to the sidebar"
        );
        assert_eq!(rail(terminal.backend().buffer(), 102), 0);
    }

    #[test]
    fn section_header_lights_up_only_while_its_panel_is_focused() {
        let at_rest = section_header(false, "projects", 2, 30, false);
        assert!(at_rest.spans[1].style.add_modifier.contains(Modifier::DIM));
        let focused = section_header(false, "projects", 2, 30, true);
        assert_eq!(focused.spans[1].style.fg, Some(accent()));
        assert!(focused.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn focused_pane_title_is_bright_and_unfocused_recedes() {
        let info = PaneInfo {
            id: PaneId(1),
            title: "claude".into(),
            agent: Some("claude".into()),
            state: AgentState::Done,
            exited: None,
            subagents: Vec::new(),
            checkpoints: Vec::new(),
            approval: None,
            source: None,
            sandbox: None,
            issue: None,
            conversation: None,
            sandbox_name: None,
            user_vars: Default::default(),
        };
        let focused = pane_title_line(&info, true, ' ');
        assert_eq!(
            focused.spans[0].style.fg,
            Some(accent()),
            "the ❯ marker is accent"
        );
        assert!(
            !focused.spans[1].style.add_modifier.contains(Modifier::DIM)
                && focused.spans[1].style.add_modifier.contains(Modifier::BOLD),
            "the focused pane's name reads at full intensity"
        );
        let unfocused = pane_title_line(&info, false, ' ');
        assert!(
            unfocused.spans[1]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }
}
