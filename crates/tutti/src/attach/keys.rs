//! Crossterm's key events as rtty-vt's (`rtty_vt::input`), which encodes them for a pane as its
//! program asked: kitty's keyboard protocol, xterm's modifyOtherKeys, or plain xterm keys.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use rtty_vt::input::{self, Code, Key, Kind};
use rtty_vt::{Modes, Terminal};

use super::reader::Report;

/// Kitty's bits for crossterm's modifiers and locks.
const BITS: [(KeyModifiers, u8); 6] = [
    (KeyModifiers::SHIFT, input::SHIFT),
    (KeyModifiers::ALT, input::ALT),
    (KeyModifiers::CONTROL, input::CTRL),
    (KeyModifiers::SUPER, input::SUPER),
    (KeyModifiers::HYPER, input::HYPER),
    (KeyModifiers::META, input::META),
];
const LOCK_BITS: [(KeyEventState, u8); 2] = [
    (KeyEventState::CAPS_LOCK, input::CAPS_LOCK),
    (KeyEventState::NUM_LOCK, input::NUM_LOCK),
];

/// The bytes a terminal would send for `key` — a press, a repeat or a release — to the program
/// behind `term` (plain xterm keys with none), or `None` when it sends none. `report` is what
/// the TUI's own terminal said of the key in kitty's protocol, passed on as far as the program
/// asked for. An uppercase letter is the letter with Shift held, however the terminal said so,
/// and BackTab is Shift+Tab.
pub fn encode_key(
    key: KeyEvent,
    report: Option<&Report>,
    term: Option<&Terminal>,
) -> Option<Vec<u8>> {
    let held = BITS.iter().filter(|(m, _)| key.modifiers.contains(*m));
    let locked = LOCK_BITS.iter().filter(|(l, _)| key.state.contains(*l));
    let mut mods: u8 = held
        .map(|(_, bit)| bit)
        .chain(locked.map(|(_, bit)| bit))
        .sum();
    let code = match key.code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                mods |= input::SHIFT;
            }
            Code::Char(c.to_ascii_lowercase())
        }
        KeyCode::BackTab => {
            mods |= input::SHIFT;
            Code::Tab
        }
        KeyCode::Enter => Code::Enter,
        KeyCode::Tab => Code::Tab,
        KeyCode::Backspace => Code::Backspace,
        KeyCode::Esc => Code::Escape,
        KeyCode::Up => Code::Up,
        KeyCode::Down => Code::Down,
        KeyCode::Left => Code::Left,
        KeyCode::Right => Code::Right,
        KeyCode::Home => Code::Home,
        KeyCode::End => Code::End,
        KeyCode::Insert => Code::Insert,
        KeyCode::Delete => Code::Delete,
        KeyCode::PageUp => Code::PageUp,
        KeyCode::PageDown => Code::PageDown,
        KeyCode::F(n) => Code::F(n),
        KeyCode::Modifier(_) => Code::Modifier,
        _ => Code::Other,
    };
    let kind = match key.kind {
        KeyEventKind::Press => Kind::Press,
        KeyEventKind::Repeat => Kind::Repeat,
        KeyEventKind::Release => Kind::Release,
    };
    let key = match report {
        Some(report) => Key {
            code,
            mods: report.mods as u8,
            kind,
            text: &report.text,
            number: Some(report.code),
            shifted: report.shifted,
            base: report.base,
        },
        None => Key {
            code,
            mods,
            kind,
            ..Key::default()
        },
    };
    let plain = Modes::default();
    let (modes, flags) = term.map_or((&plain, 0), |term| (term.modes(), term.kitty_keyboard()));
    input::encode_key(&key, modes, flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal whose program asked for its keys with `setup`.
    fn term(setup: &[u8]) -> Terminal {
        let mut term = Terminal::new(2, 10, 0);
        term.advance(setup);
        term
    }

    fn send(key: KeyEvent, term: Option<&Terminal>) -> Option<String> {
        encode_key(key, None, term).map(|bytes| String::from_utf8(bytes).unwrap())
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    /// Crossterm's keys reach rtty-vt's encoder as the keys they are: an uppercase letter is
    /// Shift and the letter, BackTab Shift+Tab, Super and the locks kitty's bits.
    #[test]
    fn crossterm_keys_reach_the_encoder_as_the_keys_they_are() {
        let plain = |code, mods| send(KeyEvent::new(code, mods), None);
        assert_eq!(plain(KeyCode::Char('a'), NONE).unwrap(), "a");
        assert_eq!(plain(KeyCode::Char('A'), SHIFT).unwrap(), "A");
        assert_eq!(plain(KeyCode::Char('A'), NONE).unwrap(), "A");
        assert_eq!(plain(KeyCode::Char('é'), NONE).unwrap(), "é");
        assert_eq!(plain(KeyCode::Char('c'), CTRL).unwrap(), "\x03");
        assert_eq!(plain(KeyCode::Char('A'), CTRL).unwrap(), "\x01");
        assert_eq!(plain(KeyCode::Char('x'), ALT).unwrap(), "\x1bx");
        assert_eq!(plain(KeyCode::Char('a'), ALT | CTRL).unwrap(), "\x1b\x01");
        assert_eq!(plain(KeyCode::BackTab, SHIFT).unwrap(), "\x1b[Z");
        assert_eq!(plain(KeyCode::Esc, NONE).unwrap(), "\x1b");
        assert_eq!(plain(KeyCode::Delete, CTRL).unwrap(), "\x1b[3;5~");
        assert_eq!(plain(KeyCode::F(12), NONE).unwrap(), "\x1b[24~");
        assert_eq!(plain(KeyCode::F(20), NONE), None);
        assert_eq!(plain(KeyCode::Null, NONE), None);
        assert_eq!(plain(KeyCode::CapsLock, NONE), None);
        let disambiguate = term(b"\x1b[>1u");
        let kitty = |code, mods| send(KeyEvent::new(code, mods), Some(&disambiguate));
        assert_eq!(kitty(KeyCode::Char('A'), CTRL).unwrap(), "\x1b[97;6u");
        assert_eq!(kitty(KeyCode::BackTab, SHIFT).unwrap(), "\x1b[9;2u");
        let super_ctrl = KeyModifiers::SUPER | CTRL;
        assert_eq!(
            kitty(KeyCode::Char('s'), super_ctrl).unwrap(),
            "\x1b[115;13u"
        );
        let all = term(b"\x1b[>8u");
        let num_lock = KeyEvent::new_with_kind_and_state(
            KeyCode::Up,
            NONE,
            KeyEventKind::Press,
            KeyEventState::NUM_LOCK,
        );
        assert_eq!(send(num_lock, Some(&all)).unwrap(), "\x1b[1;129A");
        let release = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('a'),
            CTRL,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        assert_eq!(
            send(release, Some(&term(b"\x1b[>3u"))).unwrap(),
            "\x1b[97;5:3u"
        );
        assert_eq!(send(release, None), None);
    }

    /// The pane's own modes, read off its terminal: DECCKM, modifyOtherKeys, kitty's flags.
    #[test]
    fn the_panes_terminal_says_how_its_program_asked() {
        let up = KeyEvent::new(KeyCode::Up, NONE);
        assert_eq!(send(up, Some(&term(b"\x1b[?1h"))).unwrap(), "\x1bOA");
        let ctrl_i = KeyEvent::new(KeyCode::Char('i'), CTRL);
        assert_eq!(send(ctrl_i, Some(&term(b""))).unwrap(), "\x09");
        assert_eq!(
            send(ctrl_i, Some(&term(b"\x1b[>4;2m"))).unwrap(),
            "\x1b[27;5;105~"
        );
        assert_eq!(
            send(ctrl_i, Some(&term(b"\x1b[>4;2m\x1b[>1u"))).unwrap(),
            "\x1b[105;5u"
        );
    }

    /// A key as the TUI's own kitty terminal reported it reaches a program
    /// as far as it asked: every key an escape code (8) with its text (16),
    /// a lone modifier, the lock bits, the shifted and base-layout keys (4),
    /// composed text. A program that asked for less gets less, down to the
    /// bytes a plain terminal sends.
    #[test]
    fn kitty_reports_pass_on_what_the_program_asked_for() {
        use crate::attach::reader::{Decoder, Input};
        let pass = |bytes: &[u8], flags: u8| match Decoder::default().feed(bytes).as_slice() {
            [Input::Key(key, report)] => {
                let term = term(format!("\x1b[>{flags}u").as_bytes());
                encode_key(*key, report.as_ref(), Some(&term))
                    .map(|b| String::from_utf8(b).unwrap())
            }
            inputs => panic!("{bytes:?}: {inputs:?}"),
        };
        let some = |s: &str| Some(s.to_string());
        let (one, eight, sixteen) = (input::DISAMBIGUATE, input::ALL_KEYS, input::ASSOCIATED_TEXT);
        let all = one | input::EVENT_TYPES | input::ALTERNATE_KEYS | eight | sixteen;
        for (sent, flags, want) in [
            // Shift+a, and its text.
            (&b"\x1b[97:65;2;65u"[..], all, some("\x1b[97:65;2;65u")),
            (b"\x1b[97:65;2;65u", eight, some("\x1b[97;2u")),
            (b"\x1b[97:65;2;65u", eight | sixteen, some("\x1b[97;2;65u")),
            (b"\x1b[97:65;2;65u", one, some("A")),
            // Plain text from a plain terminal, every key an escape code.
            (b"a", all, some("\x1b[97;;97u")),
            (b"\x1b[49:33;2;33u", one | eight, some("\x1b[49;2u")),
            (b"\x1b[49:33;2;33u", one, some("!")),
            // A lone modifier, pressed and released.
            (b"\x1b[57441;2u", all, some("\x1b[57441;2u")),
            (b"\x1b[57441;1:3u", all, some("\x1b[57441;1:3u")),
            (b"\x1b[57441;2u", one | input::EVENT_TYPES, None),
            // Caps Lock: its bit, and the text it made.
            (b"\x1b[97;65;65u", all, some("\x1b[97;65;65u")),
            (b"\x1b[97;65;65u", one, some("A")),
            (b"\x1b[27;65u", one, some("\x1b[27u")),
            // Ctrl+С on a Cyrillic layout: the base-layout key.
            (b"\x1b[1089::99;5u", all, some("\x1b[1089::99;5u")),
            (b"\x1b[1089::99;5u", one, some("\x1b[1089;5u")),
            // Text with no key, as an input method makes it.
            (b"\x1b[0;;101:769u", all, some("\x1b[0;;101:769u")),
            (b"\x1b[0;;101:769u", one, some("e\u{301}")),
            // The keypad: Enter is a key of its own, a digit is text.
            (b"\x1b[57414u", one, some("\x1b[57414u")),
            (b"\x1b[57414u", 0, some("\r")),
            (b"\x1b[57399;;48u", one, some("0")),
            (b"\x1b[57399;;48u", all, some("\x1b[57399;;48u")),
        ] {
            assert_eq!(pass(sent, flags), want, "{sent:?} under {flags}");
        }
    }
}
