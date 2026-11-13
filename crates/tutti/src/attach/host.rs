//! The terminal the TUI draws into: ratatui's crossterm backend for all but
//! the drawing of cells, which says what ratatui's cells cannot — a pane's
//! underline styles (nvim's undercurl), overline and hyperlinks — and comes
//! as one synchronized update (mode 2026) a frame, so the terminal never
//! shows one half drawn.
//!
//! What a cell cannot hold rides in the bits of its `Modifier` that ratatui
//! leaves unused: they take part in the buffer diff as any modifier does, so
//! a cell whose underline style alone changed is drawn again. A hyperlink is
//! too big for them: the renderer notes each linked cell's link here for the
//! frame (`link`), and a linked cell is drawn every frame — as is the cell
//! that was one the frame before — so no change of link alone goes unseen.
//!
//! Lines on the edges of cells (the cell-border protocol, `DCS 1 + b
//! cell-border`) are no part of a cell's text either: a frame drawn with
//! them takes no rows or columns. The renderer notes each lined cell's edges
//! for the frame (`outline`, `rule`), and the frame's end sends what changed
//! as rectangles — and what the frame's own writes wiped, since a cell the
//! terminal writes loses its lines. Only a terminal that said it draws them
//! (`set_lined`) is sent any; the renderer draws box characters elsewhere.

use std::cell::{Cell as Flag, RefCell};
use std::collections::HashMap;
use std::io::{self, Stdout, Write};
use std::sync::Arc;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellDiffOption};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::execute;
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier};
use rtty_vt::{Hyperlink, Underline};

/// The underline style beyond a plain one, as its `4:n` subparameter (2–5).
const STYLE_SHIFT: u16 = 9;
const STYLE: u16 = 0b111 << STYLE_SHIFT;
pub const OVERLINE: Modifier = Modifier::from_bits_retain(1 << 12);

/// The modifier bits that draw `underline`: none, ratatui's own underline,
/// or that and the style's.
pub fn underline(underline: Underline) -> Modifier {
    let style = match underline {
        Underline::None => return Modifier::empty(),
        Underline::Single => 0,
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
    };
    Modifier::UNDERLINED | Modifier::from_bits_retain(style << STYLE_SHIFT)
}

/// A line on a cell's edge, inside the cell: its colour, and whether it is
/// the focused thing's, drawn wider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub color: Color,
    pub wide: bool,
}

/// The lines on one cell: a stroke an edge, its rounded corners (`ROUND_*`),
/// and the colour of the ground past them, which the terminal fills the
/// corner a round one cuts off with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges {
    top: Option<Stroke>,
    right: Option<Stroke>,
    bottom: Option<Stroke>,
    left: Option<Stroke>,
    round: u8,
    ground: Color,
}

/// The protocol's corner joints, clockwise from the top left.
pub const ROUND_TOP: u8 = 1 | 2;
pub const ROUND_BOTTOM: u8 = 4 | 8;
pub const ROUND_ALL: u8 = ROUND_TOP | ROUND_BOTTOM;

/// A stroke's thickness in 256ths of a cell's width, and the focused one's.
const THIN: u8 = 20;
const WIDE: u8 = 36;

thread_local! {
    /// The links of this frame's linked cells, by position.
    static LINKS: RefCell<HashMap<Position, Arc<Hyperlink>>> = RefCell::default();
    /// Whether the terminal draws lines on cell edges.
    static LINED: Flag<bool> = const { Flag::new(false) };
    /// This frame's lined cells.
    static EDGES: RefCell<HashMap<Position, Edges>> = RefCell::default();
}

/// Start a frame: no cell is linked or lined yet.
pub fn begin() {
    LINKS.with_borrow_mut(HashMap::clear);
    EDGES.with_borrow_mut(HashMap::clear);
}

/// Whether the terminal draws lines on cell edges, so a frame costs no
/// cells: it answered the cell-border query (`reader::ASK_LINES`).
pub fn lined() -> bool {
    LINED.get()
}

pub fn set_lined(lined: bool) {
    LINED.set(lined);
}

/// Line the perimeter of `area` for this frame, on the outer edges of its
/// outermost cells, rounding the corners `round` names over `ground`.
pub fn outline(area: Rect, stroke: Stroke, round: u8, ground: Color) {
    if area.is_empty() {
        return;
    }
    let (right, bottom) = (area.right() - 1, area.bottom() - 1);
    EDGES.with_borrow_mut(|edges| {
        let mut at = |x, y, line: fn(&mut Edges, Stroke)| {
            line(edges.entry(Position { x, y }).or_default(), stroke);
        };
        for x in area.x..=right {
            at(x, area.y, |edges, stroke| edges.top = Some(stroke));
            at(x, bottom, |edges, stroke| edges.bottom = Some(stroke));
        }
        for y in area.y..=bottom {
            at(area.x, y, |edges, stroke| edges.left = Some(stroke));
            at(right, y, |edges, stroke| edges.right = Some(stroke));
        }
        let corners = [
            (area.x, area.y),
            (right, area.y),
            (right, bottom),
            (area.x, bottom),
        ];
        for (joint, (x, y)) in corners.into_iter().enumerate() {
            let corner = edges.entry(Position { x, y }).or_default();
            corner.round |= round & (1 << joint);
            corner.ground = ground;
        }
    });
}

/// Line the bottom edge of `area`'s last row for this frame.
pub fn rule(area: Rect, stroke: Stroke) {
    if area.is_empty() {
        return;
    }
    EDGES.with_borrow_mut(|edges| {
        for x in area.x..area.right() {
            let at = Position::new(x, area.bottom() - 1);
            edges.entry(at).or_default().bottom = Some(stroke);
        }
    });
}

/// Whether this frame lines the cell at `at`.
#[cfg(test)]
pub fn is_lined(at: Position) -> bool {
    EDGES.with_borrow(|edges| edges.contains_key(&at))
}

/// Take this frame's lines off `area`: what is drawn over lined cells — a
/// popup over a pane's frame — must not show them through.
pub fn unline(area: Rect) {
    EDGES.with_borrow_mut(|edges| edges.retain(|at, _| !area.contains(*at)));
}

/// Link `cell`, at `at`, to `link` for this frame. It is drawn whether or
/// not the buffer diff sees it change: ratatui's cells cannot tell one link
/// from another, and a cell that was linked differs from itself unlinked by
/// that alone.
pub fn link(cell: &mut Cell, at: Position, link: &Arc<Hyperlink>) {
    cell.set_diff_option(CellDiffOption::AlwaysUpdate);
    LINKS.with_borrow_mut(|links| links.insert(at, Arc::clone(link)));
}

/// Enter raw mode and the alternate screen, and draw there.
pub fn init() -> io::Result<ratatui::Terminal<Host>> {
    take()?;
    ratatui::Terminal::new(Host {
        out: CrosstermBackend::new(io::stdout()),
        shown: HashMap::new(),
    })
}

/// Enter raw mode and the alternate screen: at start, and again on resuming
/// from a stop.
pub fn take() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)
}

pub struct Host {
    out: CrosstermBackend<Stdout>,
    /// The lines the terminal's cells hold, as far as this end knows.
    shown: HashMap<Position, Edges>,
}

impl Backend for Host {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let out = &mut self.out;
        out.write_all(b"\x1b[?2026h")?;
        let (mut last, mut style, mut link) = (None::<Position>, None, None);
        for (x, y, cell) in content {
            if last.is_none_or(|last| (last.x + 1, last.y) != (x, y)) {
                queue!(out, MoveTo(x, y))?;
            }
            last = Some(Position { x, y });
            // A written cell takes the terminal's empty pen: its lines go,
            // and a wide character's second cell's with them.
            self.shown.remove(&Position { x, y });
            if !cell.symbol().is_ascii() {
                self.shown.remove(&Position { x: x + 1, y });
            }
            let this = Some((cell.fg, cell.bg, cell.underline_color, cell.modifier));
            if this != style {
                write!(out, "\x1b[{}m", rendition(cell))?;
                style = this;
            }
            let marked = cell.diff_option == CellDiffOption::AlwaysUpdate;
            let linked = marked
                .then(|| LINKS.with_borrow(|links| links.get(&Position { x, y }).cloned()))
                .flatten();
            if linked != link {
                write_link(out, linked.as_deref())?;
                link = linked;
            }
            out.write_all(cell.symbol().as_bytes())?;
        }
        if link.is_some() {
            write_link(out, None)?;
        }
        out.write_all(b"\x1b[0m")?;
        if lined() {
            EDGES.with_borrow(|edges| write_edges(out, edges, &mut self.shown))?;
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.out.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.out.show_cursor()
    }

    /// Never asked of the terminal: crossterm's query reads the tty for the
    /// answer, which the TUI's own reader (`reader`) would take first — and
    /// ratatui asks only to put the cursor back after a clear, which the
    /// next fullscreen draw places anyway.
    /// Never asked of the terminal: crossterm's query reads the tty for the
    /// answer, which the TUI's own reader (`reader`) would take first — and
    /// ratatui asks only to put the cursor back after a clear, which the
    /// next fullscreen draw places anyway.
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(Position::ORIGIN)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.out.set_cursor_position(position)
    }

    /// An erased cell takes the terminal's empty pen, as a written one does.
    fn clear(&mut self) -> io::Result<()> {
        self.shown.clear();
        self.out.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.shown.clear();
        self.out.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.out.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.out.window_size()
    }

    /// The frame is whole: end its synchronized update.
    fn flush(&mut self) -> io::Result<()> {
        self.out.write_all(b"\x1b[?2026l")?;
        Backend::flush(&mut self.out)
    }
}

/// Bring the terminal's lines from `shown` to `edges`: each cell whose lines
/// differ is cleared of the old and given the new, the cells that change
/// alike sent as one rectangle. The terminal's pen is never set, so the
/// cells a frame writes come out bare and are lined again here.
fn write_edges(
    out: &mut impl Write,
    edges: &HashMap<Position, Edges>,
    shown: &mut HashMap<Position, Edges>,
) -> io::Result<()> {
    let stale = shown.iter().filter(|(at, _)| !edges.contains_key(at));
    let fresh = edges.iter().filter(|(at, now)| shown.get(at) != Some(now));
    let changes = stale
        .map(|(at, _)| (*at, (true, None)))
        .chain(fresh.map(|(at, now)| (*at, (shown.contains_key(at), Some(*now)))))
        .collect();
    for (rect, (was, now)) in rects(changes) {
        queue!(out, MoveTo(rect.x, rect.y))?;
        let size = format!("{};{}", rect.height, rect.width);
        if was {
            write!(out, "\x1bP1+bcell-border;clear;4095;{size}\x1b\\")?;
            write!(out, "\x1bP1+bcell-border;square;15;{size}\x1b\\")?;
        }
        let Some(now) = now else { continue };
        let sides = [
            (now.top, 3),
            (now.right, 12),
            (now.bottom, 48),
            (now.left, 192),
        ];
        let mut strokes: Vec<(Stroke, u16)> = Vec::new();
        for (stroke, side) in sides.into_iter().filter_map(|(s, side)| Some((s?, side))) {
            match strokes.iter_mut().find(|(drawn, _)| *drawn == stroke) {
                Some((_, mask)) => *mask |= side,
                None => strokes.push((stroke, side)),
            }
        }
        for (stroke, mask) in strokes {
            let width = if stroke.wide { WIDE } else { THIN };
            let ink = ink(stroke.color);
            write!(
                out,
                "\x1bP1+bcell-border;set;{mask};{width};{ink};0;0;{size}\x1b\\"
            )?;
        }
        if now.round != 0 {
            let ground = match now.ground {
                Color::Reset => "default".into(),
                ground => ink(ground),
            };
            write!(
                out,
                "\x1bP1+bcell-border;round;{};{size};outside={ground}\x1b\\",
                now.round
            )?;
        }
    }
    shown.clone_from(edges);
    Ok(())
}

/// `cells` as rectangles of cells with one `key`: runs down a column first,
/// then equal runs side by side.
fn rects<K: Copy + PartialEq>(mut cells: Vec<(Position, K)>) -> Vec<(Rect, K)> {
    cells.sort_by_key(|(at, _)| (at.x, at.y));
    let mut strips: Vec<(Rect, K)> = Vec::new();
    for (at, key) in cells {
        match strips.last_mut() {
            Some((strip, k)) if *k == key && (strip.x, strip.bottom()) == (at.x, at.y) => {
                strip.height += 1;
            }
            _ => strips.push((Rect::new(at.x, at.y, 1, 1), key)),
        }
    }
    strips.sort_by_key(|(strip, _)| (strip.y, strip.height, strip.x));
    let mut out: Vec<(Rect, K)> = Vec::new();
    for (strip, key) in strips {
        match out.last_mut() {
            Some((rect, k))
                if *k == key
                    && (rect.y, rect.height, rect.right()) == (strip.y, strip.height, strip.x) =>
            {
                rect.width += 1;
            }
            _ => out.push((strip, key)),
        }
    }
    out
}

/// A stroke's colour as the protocol spells it: the terminal's own
/// foreground, a palette slot — resolved as the terminal draws, so it
/// follows a change of scheme — or RGB.
fn ink(color: Color) -> String {
    match color {
        Color::Reset => "fg".into(),
        Color::Rgb(r, g, b) => format!("{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => format!("p{n}"),
        named => format!("p{}", named_index(named)),
    }
}

/// OSC 8 opening `link`, or closing the open one. A program's link goes out
/// without the control characters that could end the string early.
fn write_link(out: &mut impl Write, link: Option<&Hyperlink>) -> io::Result<()> {
    let clean = |text: &str| text.chars().filter(|c| !c.is_control()).collect::<String>();
    match link {
        Some(link) => {
            let id = link.id.as_deref().map(|id| format!("id={}", clean(id)));
            write!(
                out,
                "\x1b]8;{};{}\x1b\\",
                id.unwrap_or_default(),
                clean(&link.uri)
            )
        }
        None => out.write_all(b"\x1b]8;;\x1b\\"),
    }
}

/// The SGR parameters that select `cell`'s rendition from scratch.
fn rendition(cell: &Cell) -> String {
    let modifier = cell.modifier;
    let mut sgr = String::from("0");
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::RAPID_BLINK, "6"),
        (Modifier::REVERSED, "7"),
        (Modifier::HIDDEN, "8"),
        (Modifier::CROSSED_OUT, "9"),
        (OVERLINE, "53"),
    ] {
        if modifier.contains(flag) {
            sgr.push(';');
            sgr.push_str(code);
        }
    }
    if modifier.contains(Modifier::UNDERLINED) {
        match (modifier.bits() & STYLE) >> STYLE_SHIFT {
            0 => sgr.push_str(";4"),
            style => sgr.push_str(&format!(";4:{style}")),
        }
    }
    // A named colour has codes of its own but under the text, where it is a
    // palette slot.
    let colors = [
        (cell.fg, Some(30), 38),
        (cell.bg, Some(40), 48),
        (cell.underline_color, None, 58),
    ];
    for (color, base, extended) in colors {
        let code = match (color, base) {
            (Color::Reset, _) => continue,
            (Color::Indexed(n), _) => format!("{extended};5;{n}"),
            (Color::Rgb(r, g, b), _) => format!("{extended};2;{r};{g};{b}"),
            (named, None) => format!("{extended};5;{}", named_index(named)),
            (named, Some(base)) => match named_index(named) {
                n @ 0..8 => (base + n).to_string(),
                n => (base + 52 + n).to_string(),
            },
        };
        sgr.push(';');
        sgr.push_str(&code);
    }
    sgr
}

/// The ANSI palette slot a named ratatui colour stands for.
pub fn named_index(color: Color) -> u8 {
    match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        _ => 15,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(modifier: Modifier) -> Cell {
        let mut cell = Cell::new("x");
        cell.modifier = modifier;
        cell
    }

    #[test]
    fn renditions_carry_underline_styles_and_overline() {
        assert_eq!(rendition(&cell(Modifier::empty())), "0");
        assert_eq!(rendition(&cell(underline(Underline::Single))), "0;4");
        assert_eq!(rendition(&cell(underline(Underline::Curly))), "0;4:3");
        assert_eq!(rendition(&cell(underline(Underline::Dashed))), "0;4:5");
        assert_eq!(
            rendition(&cell(
                Modifier::BOLD | OVERLINE | underline(Underline::Double)
            )),
            "0;1;53;4:2"
        );
    }

    #[test]
    fn renditions_carry_every_colour() {
        let mut c = cell(Modifier::empty());
        (c.fg, c.bg, c.underline_color) = (Color::Red, Color::LightBlue, Color::Rgb(1, 2, 3));
        assert_eq!(rendition(&c), "0;31;104;58;2;1;2;3");
        (c.fg, c.bg, c.underline_color) = (Color::Indexed(200), Color::Reset, Color::Cyan);
        assert_eq!(rendition(&c), "0;38;5;200;58;5;6");
    }

    #[test]
    fn a_link_goes_out_without_controls() {
        let mut out = Vec::new();
        let link = Hyperlink {
            id: Some("a\x07b".into()),
            uri: "http://x/\x1by".into(),
        };
        write_link(&mut out, Some(&link)).unwrap();
        write_link(&mut out, None).unwrap();
        assert_eq!(out, b"\x1b]8;id=ab;http://x/y\x1b\\\x1b]8;;\x1b\\");
    }

    fn sent(shown: &mut HashMap<Position, Edges>) -> String {
        let mut out = Vec::new();
        EDGES
            .with_borrow(|edges| write_edges(&mut out, edges, shown))
            .unwrap();
        String::from_utf8(out)
            .unwrap()
            .replace("\x1bP1+bcell-border;", "<")
            .replace("\x1b\\", ">")
            .replace("\x1b[", "@")
    }

    /// An outline goes out as its corners, its two rows and its two
    /// columns, each a rectangle of cells lined alike, and a frame that
    /// changes nothing sends nothing.
    #[test]
    fn an_outline_is_sent_once_as_rectangles() {
        begin();
        let stroke = Stroke {
            color: Color::Blue,
            wide: true,
        };
        outline(Rect::new(2, 1, 4, 3), stroke, ROUND_TOP, Color::Reset);
        let mut shown = HashMap::new();
        assert_eq!(
            sent(&mut shown),
            "@2;3H<set;195;36;p4;0;0;1;1><round;1;1;1;outside=default>\
             @2;4H<set;3;36;p4;0;0;1;2>\
             @2;6H<set;15;36;p4;0;0;1;1><round;2;1;1;outside=default>\
             @3;3H<set;192;36;p4;0;0;1;1>\
             @3;6H<set;12;36;p4;0;0;1;1>\
             @4;3H<set;240;36;p4;0;0;1;1>\
             @4;4H<set;48;36;p4;0;0;1;2>\
             @4;6H<set;60;36;p4;0;0;1;1>"
        );
        assert_eq!(sent(&mut shown), "");
    }

    /// A cell the frame wrote lost its lines to the terminal's empty pen,
    /// so it is lined again; one whose lines changed is cleared first; one
    /// no longer lined is cleared.
    #[test]
    fn lines_follow_what_the_terminal_holds() {
        begin();
        let dim = Stroke {
            color: Color::Rgb(1, 2, 255),
            wide: false,
        };
        rule(Rect::new(0, 0, 3, 1), dim);
        let mut shown = HashMap::new();
        assert_eq!(sent(&mut shown), "@1;1H<set;48;20;0102ff;0;0;1;3>");

        shown.remove(&Position::new(1, 0));
        assert_eq!(sent(&mut shown), "@1;2H<set;48;20;0102ff;0;0;1;1>");

        begin();
        let fg = Stroke {
            color: Color::Reset,
            wide: false,
        };
        rule(Rect::new(0, 0, 1, 1), fg);
        assert_eq!(
            sent(&mut shown),
            "@1;1H<clear;4095;1;1><square;15;1;1><set;48;20;fg;0;0;1;1>\
             @1;2H<clear;4095;1;2><square;15;1;2>"
        );

        outline(Rect::new(0, 0, 9, 9), fg, ROUND_ALL, Color::Rgb(9, 9, 9));
        unline(Rect::new(0, 0, 9, 9));
        assert_eq!(sent(&mut shown), "@1;1H<clear;4095;1;1><square;15;1;1>");
        assert!(shown.is_empty());
    }

    /// The lines the TUI sends are ones rtty's engine takes, cell for cell,
    /// and its answer to the query is one the TUI's reader takes.
    #[test]
    fn rtty_takes_the_lines_the_tui_sends() {
        use rtty_vt::{Color as Ink, Event, Terminal};
        let mut term = Terminal::new(6, 10, 0);
        term.set_cell_borders(true);
        term.advance(crate::attach::reader::ASK_LINES);
        let replies: Vec<_> = term.take_events();
        let Some(Event::Reply(reply)) = replies.first() else {
            panic!("no answer to the query: {replies:?}");
        };
        let mut decoder = crate::attach::reader::Decoder::default();
        let inputs = decoder.feed(reply);
        assert!(
            matches!(&inputs[..], [crate::attach::reader::Input::Lines(f)] if f.iter().any(|f| f == "rect")),
            "{inputs:?}"
        );

        begin();
        let accent = Stroke {
            color: Color::Blue,
            wide: true,
        };
        let grey = Stroke {
            color: Color::Rgb(1, 2, 255),
            wide: false,
        };
        outline(Rect::new(2, 1, 4, 3), accent, ROUND_ALL, Color::Reset);
        rule(Rect::new(0, 0, 8, 1), grey);
        let mut shown = HashMap::new();
        let mut out = Vec::new();
        EDGES
            .with_borrow(|edges| write_edges(&mut out, edges, &mut shown))
            .unwrap();
        term.advance(&out);
        // Each cell's drawn segments as (bit, width, colour), and its joints.
        let lines = |term: &Terminal, x: usize, y: u16| {
            let borders = term.screen_row(y).borders(x)?;
            let segments: Vec<_> = (0..12)
                .filter_map(|bit| {
                    let stroke = borders.segments[bit]?;
                    assert!(!stroke.centered && !stroke.above);
                    Some((bit, stroke.width, stroke.color))
                })
                .collect();
            Some((segments, borders.joints, borders.outside))
        };
        let blue = |bits: &[usize]| bits.iter().map(|b| (*b, 36, Ink::Indexed(4))).collect();
        let corner: Vec<_> = blue(&[0, 1, 6, 7]);
        assert_eq!(lines(&term, 2, 1), Some((corner, 1, Some(Ink::Default))));
        assert_eq!(lines(&term, 3, 1), Some((blue(&[0, 1]), 0, None)));
        assert_eq!(lines(&term, 5, 2), Some((blue(&[2, 3]), 0, None)));
        let corner: Vec<_> = blue(&[2, 3, 4, 5]);
        assert_eq!(lines(&term, 5, 3), Some((corner, 4, Some(Ink::Default))));
        let ruled = vec![(4, 20, Ink::Rgb(1, 2, 255)), (5, 20, Ink::Rgb(1, 2, 255))];
        assert_eq!(lines(&term, 0, 0), Some((ruled, 0, None)));
        assert_eq!(lines(&term, 3, 2), None, "inside the outline");

        // A written cell loses its lines; the next frame lines it again.
        term.advance(b"\x1b[2;4Hx");
        assert_eq!(lines(&term, 3, 1), None);
        shown.remove(&Position::new(3, 1));
        out.clear();
        EDGES
            .with_borrow(|edges| write_edges(&mut out, edges, &mut shown))
            .unwrap();
        term.advance(&out);
        assert_eq!(lines(&term, 3, 1), Some((blue(&[0, 1]), 0, None)));

        // A frame with no lines takes them all off: no segment is left and
        // no joint is round (the protocol has a corner keep its outside
        // colour, unused).
        begin();
        out.clear();
        EDGES
            .with_borrow(|edges| write_edges(&mut out, edges, &mut shown))
            .unwrap();
        term.advance(&out);
        for (x, y) in [(2, 1), (3, 1), (5, 3), (0, 0)] {
            let bare =
                lines(&term, x, y).is_none_or(|(drawn, round, _)| drawn.is_empty() && round == 0);
            assert!(bare, "({x},{y}) is bare again");
        }
    }

    /// A linked cell is drawn every frame, and the frame knows its link
    /// until the next one starts.
    #[test]
    fn linked_cells_are_marked() {
        begin();
        let link = Arc::new(Hyperlink {
            id: None,
            uri: "https://example.com".into(),
        });
        let (at, mut linked) = (Position { x: 1, y: 2 }, Cell::new("x"));
        super::link(&mut linked, at, &link);
        assert_eq!(linked.diff_option, CellDiffOption::AlwaysUpdate);
        assert!(LINKS.with_borrow(|links| links.contains_key(&at)));
        begin();
        assert!(LINKS.with_borrow(HashMap::is_empty));
    }
}
