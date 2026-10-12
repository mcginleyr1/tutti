//! Translate crossterm key presses into the raw bytes a pty expects, as the
//! pane asked for them: kitty's keyboard protocol, xterm's modifyOtherKeys,
//! or plain xterm keys. The whole terminal-mode keymap lives here so the
//! escape sequences can be unit-tested without a real terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rtty_vt::{MouseEncoding, Terminal};

/// How a pane asked for its keys, read off its terminal: kitty's keyboard
/// flags, xterm's modifyOtherKeys level and the cursor-key mode (DECCKM).
/// The default is plain xterm keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keyboard {
    pub kitty: u8,
    pub modify_other_keys: u8,
    pub cursor_keys: bool,
}

impl Keyboard {
    pub fn of(term: &Terminal) -> Keyboard {
        Keyboard {
            kitty: term.kitty_keyboard(),
            modify_other_keys: term.modes().modify_other_keys,
            cursor_keys: term.modes().cursor_keys,
        }
    }
}

/// The kitty flags the encoder honours: escape codes for what plain keys
/// leave ambiguous, a shifted key's alternate, and escape codes for every
/// key. Event types (2) and associated text (16) need what the TUI does not
/// get from its own terminal: presses are all a pane hears.
const DISAMBIGUATE: u8 = 1;
const ALTERNATES: u8 = 4;
const ALL_AS_ESCAPES: u8 = 8;

const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

/// The bytes a terminal would send for `key` to a program that asked for
/// its keys as `keyboard` says, or `None` when the key has no pty encoding
/// (bare modifier presses, unmapped function keys). What a protocol leaves
/// alone — kitty's functional keys, the keys modifyOtherKeys keeps plain —
/// goes as plain xterm keys do.
pub fn encode_key(key: KeyEvent, keyboard: Keyboard) -> Option<Vec<u8>> {
    let protocol = if keyboard.kitty & (DISAMBIGUATE | ALL_AS_ESCAPES) != 0 {
        kitty(key, keyboard.kitty)
    } else if keyboard.modify_other_keys > 0 {
        modify_other_keys(key, keyboard.modify_other_keys)
    } else {
        None
    };
    protocol.or_else(|| plain(key, keyboard.cursor_keys))
}

/// A text key as its character and modifiers: an uppercase letter is the
/// letter with Shift held, however the terminal said so.
fn text_key(key: KeyEvent) -> Option<(char, KeyModifiers)> {
    match key.code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => Some((c, key.modifiers | SHIFT)),
        KeyCode::Char(c) => Some((c, key.modifiers)),
        _ => None,
    }
}

/// Kitty's `CSI key ; modifiers u` for a text key, Esc, Enter, Tab or
/// Backspace; `None` for one that goes as it plainly would — text with no
/// modifier but Shift, or an unmodified Enter, Tab or Backspace, unless
/// every key is to be an escape code — and for the functional keys, whose
/// xterm forms kitty keeps. The key is the unshifted one, its shifted form
/// the alternate when asked for.
fn kitty(key: KeyEvent, flags: u8) -> Option<Vec<u8>> {
    let all = flags & ALL_AS_ESCAPES != 0;
    let (code, mods) = match key.code {
        KeyCode::Char(_) => {
            let (c, mods) = text_key(key)?;
            if !all && (mods - SHIFT).is_empty() {
                return None;
            }
            (u32::from(c.to_ascii_lowercase()), mods)
        }
        KeyCode::Esc => (27, key.modifiers),
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace if !all && key.modifiers.is_empty() => {
            return None;
        }
        KeyCode::Enter => (13, key.modifiers),
        KeyCode::Tab => (9, key.modifiers),
        KeyCode::BackTab => (9, key.modifiers | SHIFT),
        KeyCode::Backspace => (127, key.modifiers),
        _ => return None,
    };
    let shifted = match key.code {
        KeyCode::Char(c) if flags & ALTERNATES != 0 && mods.contains(SHIFT) => {
            format!(":{}", u32::from(c.to_ascii_uppercase()))
        }
        _ => String::new(),
    };
    let param = 1 + [
        (SHIFT, 1),
        (KeyModifiers::ALT, 2),
        (KeyModifiers::CONTROL, 4),
        (KeyModifiers::SUPER, 8),
        (KeyModifiers::HYPER, 16),
        (KeyModifiers::META, 32),
    ]
    .into_iter()
    .filter(|&(modifier, _)| mods.contains(modifier))
    .map(|(_, bit)| bit)
    .sum::<u32>();
    let mods = if param == 1 {
        String::new()
    } else {
        format!(";{param}")
    };
    Some(format!("\x1b[{code}{shifted}{mods}u").into_bytes())
}

/// xterm's `CSI 27 ; modifiers ; key ~` for a modified text key, Enter,
/// Tab, Backspace or Esc, as modifyOtherKeys `level` calls for it: at 2,
/// every one with a modifier but text with Shift alone; at 1, only those
/// plain keys cannot tell apart — Ctrl with a key that has no control
/// character or with Shift, and a modified Enter, Tab, Backspace or Esc —
/// leaving Alt to its `ESC` prefix. The key is the character typed.
fn modify_other_keys(key: KeyEvent, level: u8) -> Option<Vec<u8>> {
    let ctrl = KeyModifiers::CONTROL;
    let (code, mods) = match key.code {
        KeyCode::Char(_) => text_key(key).map(|(c, mods)| (u32::from(c), mods))?,
        KeyCode::Enter => (13, key.modifiers),
        KeyCode::Tab => (9, key.modifiers),
        KeyCode::Backspace => (127, key.modifiers),
        KeyCode::Esc => (27, key.modifiers),
        _ => return None,
    };
    let mods = mods & (SHIFT | KeyModifiers::ALT | ctrl);
    let text = matches!(key.code, KeyCode::Char(_));
    let wanted = match level {
        _ if mods.is_empty() || text && mods == SHIFT => false,
        2.. => true,
        _ if !text => true,
        _ => {
            let c = char::from_u32(code).unwrap_or_default();
            mods.contains(ctrl)
                && (mods.contains(SHIFT) || control_byte(c.to_ascii_lowercase()).is_none())
        }
    };
    wanted.then(|| format!("\x1b[27;{};{code}~", modifier_param(mods)).into_bytes())
}

/// The bytes a plain xterm sends for `key`, its cursor keys in application
/// mode (`ESC O`) when `cursor_keys` and unmodified.
fn plain(key: KeyEvent, cursor_keys: bool) -> Option<Vec<u8>> {
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let param = modifier_param(m);
    let cursor = |final_byte: u8| match (cursor_keys, param) {
        (true, 1) => vec![0x1b, b'O', final_byte],
        _ => csi_final(final_byte, param),
    };

    let bytes = match key.code {
        KeyCode::Char(c) => encode_char(c, ctrl, alt),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor(b'A'),
        KeyCode::Down => cursor(b'B'),
        KeyCode::Right => cursor(b'C'),
        KeyCode::Left => cursor(b'D'),
        KeyCode::Home => cursor(b'H'),
        KeyCode::End => cursor(b'F'),
        KeyCode::Insert => csi_tilde(2, param),
        KeyCode::Delete => csi_tilde(3, param),
        KeyCode::PageUp => csi_tilde(5, param),
        KeyCode::PageDown => csi_tilde(6, param),
        KeyCode::F(n) => function_key(n, param)?,
        _ => return None,
    };
    Some(bytes)
}

fn encode_char(c: char, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    if ctrl && let Some(b) = control_byte(c) {
        out.push(b);
        return out;
    }
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    out
}

/// The C0 control byte a `Ctrl`+char chord produces, if any.
fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@'..='_' => Some(c as u8 & 0x1f),
        ' ' => Some(0),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// The xterm modifier parameter: `1 + shift + 2*alt + 4*ctrl`.
fn modifier_param(m: KeyModifiers) -> u32 {
    1 + u32::from(m.contains(KeyModifiers::SHIFT))
        + 2 * u32::from(m.contains(KeyModifiers::ALT))
        + 4 * u32::from(m.contains(KeyModifiers::CONTROL))
}

/// `ESC [ final` unmodified, else `ESC [ 1 ; param final` (arrows, Home, End).
fn csi_final(final_byte: u8, param: u32) -> Vec<u8> {
    if param == 1 {
        vec![0x1b, b'[', final_byte]
    } else {
        format!("\x1b[1;{param}{}", final_byte as char).into_bytes()
    }
}

/// `ESC [ num ~` unmodified, else `ESC [ num ; param ~` (Ins/Del/PgUp/PgDn/Fn).
fn csi_tilde(num: u32, param: u32) -> Vec<u8> {
    if param == 1 {
        format!("\x1b[{num}~").into_bytes()
    } else {
        format!("\x1b[{num};{param}~").into_bytes()
    }
}

/// The wheel-tick bytes for a program that enabled mouse reporting: buttons
/// 64/65 are wheel-up/down, as `encode_mouse` spells them.
pub fn encode_wheel(encoding: MouseEncoding, up: bool, x: u16, y: u16) -> Vec<u8> {
    encode_mouse(encoding, if up { 64 } else { 65 }, false, x, y)
}

/// A mouse report for a program that enabled mouse reporting: `button` is
/// xterm's code — 0/1/2 left/middle/right, plus 32 while it moves (3 + 32
/// for motion with none held), 64/65 the wheel, plus 8 for Alt and 16 for
/// Ctrl — and `release` ends a press. Coordinates are 1-based cells within
/// the pane. SGR spells everything out in decimal and tells a release by its
/// final `m` (tutti knows no pixels, so SGR-pixels reports cells as SGR
/// does); urxvt spells out the legacy values in decimal; the legacy encodings
/// report a release as button 3 and offset each value by 32 into one byte
/// (UTF-8 mode encodes that byte as a codepoint).
pub fn encode_mouse(
    encoding: MouseEncoding,
    button: u16,
    release: bool,
    x: u16,
    y: u16,
) -> Vec<u8> {
    let legacy = if release { button | 3 } else { button };
    match encoding {
        MouseEncoding::Sgr | MouseEncoding::SgrPixels => {
            let final_byte = if release { 'm' } else { 'M' };
            format!("\x1b[<{button};{x};{y}{final_byte}").into_bytes()
        }
        MouseEncoding::Urxvt => format!("\x1b[{};{x};{y}M", 32 + legacy).into_bytes(),
        MouseEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for value in [legacy, x, y] {
                let c = char::from_u32(u32::from(32 + value.min(2015))).unwrap_or(' ');
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
        MouseEncoding::Default => {
            let mut out = b"\x1b[M".to_vec();
            for value in [legacy, x, y] {
                out.push((32 + value.min(223)) as u8);
            }
            out
        }
    }
}

/// A wheel tick for an alternate-screen program without mouse reporting:
/// `n` arrow keys, honouring application-cursor mode — the xterm "alternate
/// scroll" convention, which lets pagers and TUIs scroll under the wheel.
pub fn encode_wheel_arrows(application_cursor: bool, up: bool, n: usize) -> Vec<u8> {
    let seq: &[u8] = match (application_cursor, up) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1bOB",
        (false, true) => b"\x1b[A",
        (false, false) => b"\x1b[B",
    };
    seq.repeat(n)
}

fn function_key(n: u8, param: u32) -> Option<Vec<u8>> {
    let bytes = match n {
        1..=4 => {
            let final_byte = b'P' + (n - 1);
            if param == 1 {
                vec![0x1b, b'O', final_byte]
            } else {
                format!("\x1b[1;{param}{}", final_byte as char).into_bytes()
            }
        }
        5 => csi_tilde(15, param),
        6 => csi_tilde(17, param),
        7 => csi_tilde(18, param),
        8 => csi_tilde(19, param),
        9 => csi_tilde(20, param),
        10 => csi_tilde(21, param),
        11 => csi_tilde(23, param),
        12 => csi_tilde(24, param),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn keym(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn legacy(key: KeyEvent) -> Option<Vec<u8>> {
        encode_key(key, Keyboard::default())
    }
    fn with(keyboard: Keyboard) -> impl Fn(KeyCode, KeyModifiers) -> String {
        move |code, mods| {
            String::from_utf8(encode_key(keym(code, mods), keyboard).unwrap()).unwrap()
        }
    }
    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    /// Kitty's disambiguation: text stays text, shifted or not, and so do
    /// an unmodified Enter, Tab and Backspace; Esc and every chord with Ctrl
    /// or Alt — Ctrl+I apart from Tab — become `CSI key ; mods u` on the
    /// unshifted key; functional keys keep their xterm forms.
    #[test]
    fn kitty_disambiguates_what_plain_keys_cannot() {
        let kitty = with(Keyboard {
            kitty: DISAMBIGUATE,
            ..Keyboard::default()
        });
        assert_eq!(kitty(KeyCode::Char('a'), NONE), "a");
        assert_eq!(kitty(KeyCode::Char('A'), SHIFT), "A");
        assert_eq!(kitty(KeyCode::Enter, NONE), "\r");
        assert_eq!(kitty(KeyCode::Tab, NONE), "\t");
        assert_eq!(kitty(KeyCode::Char('i'), CTRL), "\x1b[105;5u");
        assert_eq!(kitty(KeyCode::Esc, NONE), "\x1b[27u");
        assert_eq!(kitty(KeyCode::Char('x'), ALT), "\x1b[120;3u");
        assert_eq!(
            kitty(KeyCode::Char('A'), CTRL),
            "\x1b[97;6u",
            "Ctrl+Shift+a"
        );
        assert_eq!(kitty(KeyCode::Enter, SHIFT), "\x1b[13;2u");
        assert_eq!(kitty(KeyCode::BackTab, SHIFT), "\x1b[9;2u");
        assert_eq!(kitty(KeyCode::Backspace, CTRL), "\x1b[127;5u");
        assert_eq!(kitty(KeyCode::Char(' '), CTRL), "\x1b[32;5u");
        assert_eq!(
            kitty(KeyCode::Char('s'), KeyModifiers::SUPER | CTRL),
            "\x1b[115;13u"
        );
        assert_eq!(kitty(KeyCode::Up, CTRL), "\x1b[1;5A");
        assert_eq!(kitty(KeyCode::F(5), NONE), "\x1b[15~");
    }

    /// Every key an escape code, and a shifted key's alternate.
    #[test]
    fn kitty_reports_every_key_and_alternates_when_asked() {
        let kitty = with(Keyboard {
            kitty: DISAMBIGUATE | ALTERNATES | ALL_AS_ESCAPES,
            ..Keyboard::default()
        });
        assert_eq!(kitty(KeyCode::Char('a'), NONE), "\x1b[97u");
        assert_eq!(kitty(KeyCode::Char('A'), SHIFT), "\x1b[97:65;2u");
        assert_eq!(kitty(KeyCode::Enter, NONE), "\x1b[13u");
        assert_eq!(kitty(KeyCode::Char('é'), NONE), "\x1b[233u");
    }

    /// modifyOtherKeys 2 spells out every chord with a modifier, Ctrl+I
    /// apart from Tab; 1 only those plain keys cannot tell apart, leaving
    /// the control characters and Alt's `ESC` prefix be.
    #[test]
    fn modify_other_keys_spells_out_the_chords_its_level_asks_for() {
        let level = |n| {
            with(Keyboard {
                modify_other_keys: n,
                ..Keyboard::default()
            })
        };
        let (one, two) = (level(1), level(2));
        assert_eq!(two(KeyCode::Char('i'), CTRL), "\x1b[27;5;105~");
        assert_eq!(two(KeyCode::Char('x'), ALT), "\x1b[27;3;120~");
        assert_eq!(two(KeyCode::Char('A'), SHIFT), "A");
        assert_eq!(two(KeyCode::Char('a'), NONE), "a");
        assert_eq!(two(KeyCode::Enter, SHIFT), "\x1b[27;2;13~");
        assert_eq!(one(KeyCode::Char('i'), CTRL), "\x09");
        assert_eq!(one(KeyCode::Char('x'), ALT), "\x1bx");
        assert_eq!(one(KeyCode::Char(';'), CTRL), "\x1b[27;5;59~");
        assert_eq!(one(KeyCode::Char('A'), CTRL), "\x1b[27;6;65~");
        assert_eq!(one(KeyCode::Enter, CTRL), "\x1b[27;5;13~");
        assert_eq!(one(KeyCode::Up, CTRL), "\x1b[1;5A");
    }

    /// Kitty's flags win over modifyOtherKeys when a program set both.
    #[test]
    fn kitty_comes_before_modify_other_keys() {
        let both = with(Keyboard {
            kitty: DISAMBIGUATE,
            modify_other_keys: 2,
            ..Keyboard::default()
        });
        assert_eq!(both(KeyCode::Char('i'), CTRL), "\x1b[105;5u");
    }

    #[test]
    fn cursor_keys_in_application_mode() {
        let app = with(Keyboard {
            cursor_keys: true,
            ..Keyboard::default()
        });
        assert_eq!(app(KeyCode::Up, NONE), "\x1bOA");
        assert_eq!(app(KeyCode::End, NONE), "\x1bOF");
        assert_eq!(app(KeyCode::Up, CTRL), "\x1b[1;5A");
        assert_eq!(app(KeyCode::PageUp, NONE), "\x1b[5~");
    }

    #[test]
    fn plain_and_shifted_chars() {
        assert_eq!(legacy(key(KeyCode::Char('a'))), Some(b"a".to_vec()));
        assert_eq!(
            legacy(keym(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some(b"A".to_vec())
        );
        assert_eq!(
            legacy(key(KeyCode::Char('é'))),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn control_chords() {
        assert_eq!(
            legacy(keym(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
        assert_eq!(
            legacy(keym(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(vec![0x01])
        );
        assert_eq!(
            legacy(keym(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            Some(vec![0x00])
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            legacy(keym(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            legacy(keym(
                KeyCode::Char('a'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            )),
            Some(vec![0x1b, 0x01])
        );
    }

    #[test]
    fn named_keys() {
        assert_eq!(legacy(key(KeyCode::Enter)), Some(vec![b'\r']));
        assert_eq!(legacy(key(KeyCode::Tab)), Some(vec![b'\t']));
        assert_eq!(legacy(key(KeyCode::Backspace)), Some(vec![0x7f]));
        assert_eq!(legacy(key(KeyCode::Esc)), Some(vec![0x1b]));
        assert_eq!(legacy(key(KeyCode::BackTab)), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn arrows_and_modifiers() {
        assert_eq!(legacy(key(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
        assert_eq!(legacy(key(KeyCode::Left)), Some(b"\x1b[D".to_vec()));
        assert_eq!(
            legacy(keym(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            legacy(keym(KeyCode::Up, KeyModifiers::SHIFT)),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn home_end_and_tilde_keys() {
        assert_eq!(legacy(key(KeyCode::Home)), Some(b"\x1b[H".to_vec()));
        assert_eq!(legacy(key(KeyCode::End)), Some(b"\x1b[F".to_vec()));
        assert_eq!(legacy(key(KeyCode::Delete)), Some(b"\x1b[3~".to_vec()));
        assert_eq!(legacy(key(KeyCode::PageUp)), Some(b"\x1b[5~".to_vec()));
        assert_eq!(
            legacy(keym(KeyCode::Delete, KeyModifiers::CONTROL)),
            Some(b"\x1b[3;5~".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(legacy(key(KeyCode::F(1))), Some(b"\x1bOP".to_vec()));
        assert_eq!(legacy(key(KeyCode::F(4))), Some(b"\x1bOS".to_vec()));
        assert_eq!(legacy(key(KeyCode::F(5))), Some(b"\x1b[15~".to_vec()));
        assert_eq!(legacy(key(KeyCode::F(12))), Some(b"\x1b[24~".to_vec()));
        assert_eq!(legacy(key(KeyCode::F(20))), None);
    }

    #[test]
    fn unmapped_keys_return_none() {
        assert_eq!(legacy(key(KeyCode::Null)), None);
        assert_eq!(legacy(key(KeyCode::CapsLock)), None);
    }

    #[test]
    fn wheel_encodings() {
        use MouseEncoding as E;
        assert_eq!(
            encode_wheel(E::Sgr, true, 5, 3),
            b"\x1b[<64;5;3M".to_vec(),
            "SGR spells the button and 1-based cell in decimal"
        );
        assert_eq!(encode_wheel(E::Sgr, false, 1, 1), b"\x1b[<65;1;1M".to_vec());
        assert_eq!(
            encode_wheel(E::Default, true, 5, 3),
            vec![0x1b, b'[', b'M', 32 + 64, 32 + 5, 32 + 3],
            "the legacy encoding offsets each value by 32 into one byte"
        );
        assert_eq!(
            encode_wheel(E::Default, true, 1000, 1000),
            vec![0x1b, b'[', b'M', 32 + 64, 255, 255],
            "legacy coordinates cap at what a byte can carry"
        );
        // UTF-8 mode encodes the offset value as a codepoint, so a coordinate
        // past 95 becomes a two-byte sequence instead of clamping.
        assert_eq!(
            encode_wheel(E::Utf8, true, 200, 1),
            [
                b"\x1b[M".as_slice(),
                "\u{60}".as_bytes(),
                "\u{e8}".as_bytes(),
                b"!"
            ]
            .concat()
        );
    }

    /// Presses, releases, drags and motion: SGR keeps the button on a
    /// release and ends it `m`; the legacy encodings release as button 3,
    /// the modifier bits kept.
    #[test]
    fn mouse_encodings() {
        use MouseEncoding as E;
        assert_eq!(encode_mouse(E::Sgr, 0, false, 5, 3), b"\x1b[<0;5;3M");
        assert_eq!(encode_mouse(E::Sgr, 0, true, 5, 3), b"\x1b[<0;5;3m");
        assert_eq!(encode_mouse(E::Sgr, 2 + 16, false, 1, 1), b"\x1b[<18;1;1M");
        assert_eq!(encode_mouse(E::Sgr, 32, false, 6, 3), b"\x1b[<32;6;3M");
        assert_eq!(encode_mouse(E::Sgr, 35, false, 7, 4), b"\x1b[<35;7;4M");
        assert_eq!(
            encode_mouse(E::Default, 0, false, 5, 3),
            [0x1b, b'[', b'M', 32, 32 + 5, 32 + 3]
        );
        assert_eq!(
            encode_mouse(E::Default, 2 + 8, true, 5, 3),
            [0x1b, b'[', b'M', 32 + 3 + 8, 32 + 5, 32 + 3]
        );
        assert_eq!(
            encode_mouse(E::Utf8, 32, false, 200, 1),
            [b"\x1b[M@".as_slice(), "\u{e8}".as_bytes(), b"!"].concat()
        );
        assert_eq!(encode_mouse(E::Urxvt, 0, false, 5, 300), b"\x1b[32;5;300M");
        assert_eq!(encode_mouse(E::Urxvt, 2, true, 5, 3), b"\x1b[35;5;3M");
        assert_eq!(encode_mouse(E::SgrPixels, 0, true, 5, 3), b"\x1b[<0;5;3m");
    }

    #[test]
    fn wheel_arrows_honour_application_cursor_mode() {
        assert_eq!(encode_wheel_arrows(false, true, 3), b"\x1b[A".repeat(3));
        assert_eq!(encode_wheel_arrows(false, false, 3), b"\x1b[B".repeat(3));
        assert_eq!(encode_wheel_arrows(true, true, 1), b"\x1bOA".to_vec());
        assert_eq!(encode_wheel_arrows(true, false, 1), b"\x1bOB".to_vec());
    }
}
