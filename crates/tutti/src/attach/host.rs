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

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Stdout, Write};
use std::sync::Arc;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellDiffOption};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::execute;
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::layout::{Position, Size};
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

thread_local! {
    /// The links of this frame's linked cells, by position.
    static LINKS: RefCell<HashMap<Position, Arc<Hyperlink>>> = RefCell::default();
}

/// Start a frame: no cell is linked yet.
pub fn clear_links() {
    LINKS.with_borrow_mut(HashMap::clear);
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
    ratatui::Terminal::new(Host(CrosstermBackend::new(io::stdout())))
}

/// Enter raw mode and the alternate screen: at start, and again on resuming
/// from a stop.
pub fn take() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)
}

pub struct Host(CrosstermBackend<Stdout>);

impl Backend for Host {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let out = &mut self.0;
        out.write_all(b"\x1b[?2026h")?;
        let (mut last, mut style, mut link) = (None::<Position>, None, None);
        for (x, y, cell) in content {
            if last.is_none_or(|last| (last.x + 1, last.y) != (x, y)) {
                queue!(out, MoveTo(x, y))?;
            }
            last = Some(Position { x, y });
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
        out.write_all(b"\x1b[0m")
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
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
        self.0.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.0.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.0.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.0.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.0.window_size()
    }

    /// The frame is whole: end its synchronized update.
    fn flush(&mut self) -> io::Result<()> {
        self.0.write_all(b"\x1b[?2026l")?;
        Backend::flush(&mut self.0)
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
fn named_index(color: Color) -> u8 {
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

    /// A linked cell is drawn every frame, and the frame knows its link
    /// until the next one starts.
    #[test]
    fn linked_cells_are_marked() {
        clear_links();
        let link = Arc::new(Hyperlink {
            id: None,
            uri: "https://example.com".into(),
        });
        let (at, mut linked) = (Position { x: 1, y: 2 }, Cell::new("x"));
        super::link(&mut linked, at, &link);
        assert_eq!(linked.diff_option, CellDiffOption::AlwaysUpdate);
        assert!(LINKS.with_borrow(|links| links.contains_key(&at)));
        clear_links();
        assert!(LINKS.with_borrow(HashMap::is_empty));
    }
}
