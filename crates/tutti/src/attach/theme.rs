//! The chrome's colour vocabulary, one `Palette` per theme. `terminal` (the
//! default) is the palette-tracking look: no painted grounds, ANSI colours
//! only, the DIM attribute for metadata — the terminal's own light/dark scheme
//! flows through live and flips with it. `dark` and `light` are opt-in
//! truecolor themes: a shaded chrome over a tinted pane ground, an explicit
//! muted grey instead of DIM, and state/diff colours tuned to that ground. A
//! theme is a fixed choice made in config. The one thing detected is the
//! terminal's own colours, once at attach: whether its background is light
//! only picks which ANSI colour the `terminal` theme's selection row uses,
//! and the daemon answers the panes' colour queries with them.
//!
//! The active palette is thread-local, set for the span of one `draw` by
//! `activate`, so every styling helper reads it without threading a parameter
//! through the whole renderer — and it resets to `terminal` when the guard
//! drops, so nothing leaks between draws or between test threads.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use terminal_colorsaurus::{QueryOptions, ThemeMode};
use tutti_core::{HostColors, Rgb};

use crate::config::Theme;

/// One theme's colours. `None` grounds mean "the terminal's own" — nothing is
/// painted — and a `None` muted colour means the DIM attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The one colour that marks the focused/active thing.
    pub accent: Color,
    /// Text drawn on an accent block (name chips, the mode chip, tab chip).
    pub on_accent: Color,
    /// The chrome ground: app bar, rule, sidebar, changes panel, footer.
    pub panel_bg: Option<Color>,
    /// The pane ground: frames, gaps, popups, and default-background cells
    /// inside every pane.
    pub pane_bg: Option<Color>,
    /// Default foreground for chrome text and default-foreground pane cells.
    pub fg: Option<Color>,
    /// Metadata text — everything that is not a name or an accent.
    pub muted: Option<Color>,
    /// The full-row background under a selected sidebar/panel row and the
    /// mouse selection inside a pane.
    pub select_bg: Color,
    /// Unfocused pane frames and popup borders; `None` renders them dim.
    pub border: Option<Color>,
    pub blocked: Color,
    pub working: Color,
    pub done: Color,
    /// Diff polarity in the changes panel, sidebar stat, and footer stat.
    pub added: Color,
    pub removed: Color,
}

impl Palette {
    /// The metadata style: an explicit muted colour, or DIM on the terminal
    /// theme so the terminal's own scheme decides what "dim" looks like.
    pub fn dim(&self) -> Style {
        match self.muted {
            Some(colour) => Style::default().fg(colour),
            None => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// The style of an unfocused pane frame or popup border.
    pub fn frame(&self) -> Style {
        match self.border {
            Some(colour) => Style::default().fg(colour),
            None => self.dim(),
        }
    }

    /// The style that paints a ground: the theme foreground over `bg`. Empty
    /// on the terminal theme, so painting it is a no-op.
    pub fn ground(&self, bg: Option<Color>) -> Style {
        let mut style = Style::default();
        if let Some(fg) = self.fg {
            style = style.fg(fg);
        }
        if let Some(bg) = bg {
            style = style.bg(bg);
        }
        style
    }
}

/// Today's look: the terminal's own colours, nothing painted, DIM for metadata.
pub static TERMINAL: Palette = Palette {
    accent: Color::Blue,
    on_accent: Color::Black,
    panel_bg: None,
    pane_bg: None,
    fg: None,
    muted: None,
    select_bg: Color::DarkGray,
    border: None,
    blocked: Color::Red,
    working: Color::Yellow,
    done: Color::Green,
    added: Color::Green,
    removed: Color::Red,
};

/// `terminal` over a light background. ANSI 8 ("bright black") is a dark grey
/// on most dark schemes but pure black on some light ones, so a light terminal
/// takes its selection from ANSI 7 instead.
pub static TERMINAL_LIGHT: Palette = Palette {
    select_bg: Color::Gray,
    ..TERMINAL
};

static LIGHT_TERMINAL: AtomicBool = AtomicBool::new(false);

/// The colours a pane shows on, which the daemon answers its colour queries
/// with (`Request::HostColors`). On the `terminal` theme they are the
/// terminal's own, asked once here before the TUI takes over input — which
/// also picks the selection colour, fixed for the attach, a later light/dark
/// flip keeping it. A terminal that doesn't answer gives `None` and is
/// treated as dark. On `dark` and `light` they are the theme's own pane
/// ground and text, which panes are painted over.
pub fn host_colors(theme: Theme) -> Option<HostColors> {
    if theme != Theme::Terminal {
        let theme_palette = palette(theme);
        let (Some(Color::Rgb(r, g, b)), Some(Color::Rgb(br, bg, bb))) =
            (theme_palette.fg, theme_palette.pane_bg)
        else {
            return None;
        };
        let wide = |r: u8, g: u8, b: u8| Rgb {
            r: u16::from(r) * 257,
            g: u16::from(g) * 257,
            b: u16::from(b) * 257,
        };
        return Some(HostColors {
            fg: wide(r, g, b),
            bg: wide(br, bg, bb),
            cursor: None,
            dark: theme == Theme::Dark,
        });
    }
    let mut options = QueryOptions::default();
    options.timeout = Duration::from_millis(300);
    let detected = terminal_colorsaurus::color_palette(options).ok();
    let dark = detected
        .as_ref()
        .is_none_or(|found| matches!(found.theme_mode(), ThemeMode::Dark));
    LIGHT_TERMINAL.store(!dark, Ordering::Relaxed);
    let rgb = |c: terminal_colorsaurus::Color| Rgb {
        r: c.r,
        g: c.g,
        b: c.b,
    };
    detected.map(|found| HostColors {
        fg: rgb(found.foreground),
        bg: rgb(found.background),
        cursor: None,
        dark,
    })
}

/// A graphite dark theme: chrome a step darker than the pane ground, soft blue
/// accent, muted steel metadata.
pub static DARK: Palette = Palette {
    accent: Color::Rgb(122, 162, 247),
    on_accent: Color::Rgb(13, 16, 23),
    panel_bg: Some(Color::Rgb(19, 21, 27)),
    pane_bg: Some(Color::Rgb(27, 30, 38)),
    fg: Some(Color::Rgb(205, 211, 222)),
    muted: Some(Color::Rgb(108, 116, 132)),
    select_bg: Color::Rgb(42, 47, 60),
    border: Some(Color::Rgb(60, 66, 80)),
    blocked: Color::Rgb(247, 118, 142),
    working: Color::Rgb(224, 175, 104),
    done: Color::Rgb(158, 206, 106),
    added: Color::Rgb(158, 206, 106),
    removed: Color::Rgb(247, 118, 142),
};

/// A paper light theme: cool grey chrome beside an off-white pane ground,
/// saturated accent and state colours so they hold up on the bright ground.
pub static LIGHT: Palette = Palette {
    accent: Color::Rgb(47, 111, 235),
    on_accent: Color::Rgb(255, 255, 255),
    panel_bg: Some(Color::Rgb(236, 239, 244)),
    pane_bg: Some(Color::Rgb(250, 250, 251)),
    fg: Some(Color::Rgb(43, 48, 59)),
    muted: Some(Color::Rgb(128, 137, 152)),
    select_bg: Color::Rgb(218, 225, 236),
    border: Some(Color::Rgb(198, 204, 214)),
    blocked: Color::Rgb(208, 61, 61),
    working: Color::Rgb(176, 124, 0),
    done: Color::Rgb(46, 139, 64),
    added: Color::Rgb(46, 139, 64),
    removed: Color::Rgb(208, 61, 61),
};

/// The palette for a configured theme.
pub fn palette(theme: Theme) -> &'static Palette {
    match theme {
        Theme::Terminal if LIGHT_TERMINAL.load(Ordering::Relaxed) => &TERMINAL_LIGHT,
        Theme::Terminal => &TERMINAL,
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

thread_local! {
    static CURRENT: Cell<&'static Palette> = const { Cell::new(&TERMINAL) };
}

/// The palette every styling helper reads: the one activated by the draw in
/// progress, else `terminal`.
pub fn current() -> &'static Palette {
    CURRENT.get()
}

/// Make `theme` current for the lifetime of the returned guard — one `draw`.
#[must_use]
pub fn activate(theme: Theme) -> Active {
    CURRENT.set(palette(theme));
    Active
}

/// Resets the current palette to `terminal` when dropped.
pub struct Active;

impl Drop for Active {
    fn drop(&mut self) {
        CURRENT.set(&TERMINAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_palette_paints_nothing_and_dims_with_the_attribute() {
        assert_eq!(TERMINAL.ground(TERMINAL.panel_bg), Style::default());
        assert!(TERMINAL.dim().add_modifier.contains(Modifier::DIM));
        assert_eq!(TERMINAL.frame(), TERMINAL.dim());
    }

    #[test]
    fn light_terminal_differs_only_in_a_palette_selection_colour() {
        assert_eq!(TERMINAL_LIGHT.select_bg, Color::Gray);
        assert_eq!(
            Palette {
                select_bg: TERMINAL.select_bg,
                ..TERMINAL_LIGHT
            },
            TERMINAL
        );
    }

    /// On a truecolor theme panes are painted over the theme's own ground,
    /// so that is what their colour queries are answered with — the terminal
    /// is never asked.
    #[test]
    fn a_truecolor_theme_reports_its_own_pane_ground() {
        let light = host_colors(Theme::Light).unwrap();
        assert_eq!(
            light.bg,
            Rgb {
                r: 0xfafa,
                g: 0xfafa,
                b: 0xfbfb
            }
        );
        assert_eq!(
            light.fg,
            Rgb {
                r: 0x2b2b,
                g: 0x3030,
                b: 0x3b3b
            }
        );
        assert!(!light.dark && light.cursor.is_none());
        let dark = host_colors(Theme::Dark).unwrap();
        assert_eq!(
            dark.bg,
            Rgb {
                r: 0x1b1b,
                g: 0x1e1e,
                b: 0x2626
            }
        );
        assert!(dark.dark);
    }

    #[test]
    fn truecolor_palettes_ground_and_mute_explicitly() {
        for palette in [&DARK, &LIGHT] {
            let ground = palette.ground(palette.panel_bg);
            assert!(matches!(ground.bg, Some(Color::Rgb(..))));
            assert!(matches!(ground.fg, Some(Color::Rgb(..))));
            assert!(!palette.dim().add_modifier.contains(Modifier::DIM));
            assert_ne!(
                palette.panel_bg, palette.pane_bg,
                "the chrome is shaded apart from the panes"
            );
        }
    }

    #[test]
    fn activation_is_scoped_to_the_guard() {
        assert_eq!(current(), &TERMINAL);
        {
            let _active = activate(Theme::Dark);
            assert_eq!(current(), &DARK);
        }
        assert_eq!(current(), &TERMINAL, "the guard resets on drop");
    }
}
