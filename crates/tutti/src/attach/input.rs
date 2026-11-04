//! Translate crossterm key events into the raw bytes a pty expects, as the
//! pane asked for them: kitty's keyboard protocol, xterm's modifyOtherKeys,
//! or plain xterm keys. The whole terminal-mode keymap lives here so the
//! escape sequences can be unit-tested without a real terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use rtty_vt::{MouseEncoding, Terminal};

use super::reader::Report;

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

/// Kitty's keyboard flags: escape codes for what plain keys leave
/// ambiguous, repeats and releases, a key's alternates, every key an escape
/// code, and the text a key typed.
pub const DISAMBIGUATE: u8 = 1;
pub const EVENTS: u8 = 2;
pub const ALTERNATES: u8 = 4;
pub const ALL_AS_ESCAPES: u8 = 8;
pub const TEXT: u8 = 16;

const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

/// Kitty's modifier bits for Shift, Alt, Ctrl, Super, Hyper and Meta, and
/// for the locks, Caps Lock and Num Lock.
const BITS: [(KeyModifiers, u32); 6] = [
    (SHIFT, 1),
    (KeyModifiers::ALT, 2),
    (KeyModifiers::CONTROL, 4),
    (KeyModifiers::SUPER, 8),
    (KeyModifiers::HYPER, 16),
    (KeyModifiers::META, 32),
];
const LOCK_BITS: [(KeyEventState, u32); 2] = [
    (KeyEventState::CAPS_LOCK, 64),
    (KeyEventState::NUM_LOCK, 128),
];
const LOCKS: u32 = 64 | 128;

/// The bytes a terminal would send for `key` — a press, a repeat or a
/// release — to a program that asked for its keys as `keyboard` says, or
/// `None` when it sends none: a release or a lone modifier to a program that
/// did not ask for them, a key with no pty encoding. `report` is what the
/// TUI's own terminal said of the key in kitty's protocol, passed on as far
/// as the program asked for. What a protocol leaves alone — kitty's
/// functional keys, the keys modifyOtherKeys keeps plain — goes as plain
/// xterm keys do; a repeat outside kitty's event types as a press.
pub fn encode_key(key: KeyEvent, report: Option<&Report>, keyboard: Keyboard) -> Option<Vec<u8>> {
    if keyboard.kitty & (DISAMBIGUATE | ALL_AS_ESCAPES) != 0 {
        return kitty(key, report, keyboard);
    }
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let protocol = match keyboard.modify_other_keys {
        0 => None,
        level => modify_other_keys(key, level),
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

/// Kitty's report of `key` to a program that pushed `keyboard.kitty`:
/// `CSI code[:shifted[:base]] ; mods[:event] [; text] u`, the key as the
/// TUI's terminal reported it (`report`), else as `key` says (`derive`). But
/// for:
/// - **What goes as plain keys do,** unless every key is to be an escape
///   code (8): text with no modifier but Shift, and an unmodified Enter,
///   Tab or Backspace — their releases then not at all.
/// - **A lone modifier,** which goes only under 8, as do the lock bits.
/// - **The functional keys** (arrows, F1…), in their xterm forms.
///
/// The event type goes only under 2 (a repeat otherwise a press, a release
/// nothing), the shifted and base keys only under 4, the text only under
/// 16 with 8.
fn kitty(key: KeyEvent, report: Option<&Report>, keyboard: Keyboard) -> Option<Vec<u8>> {
    let flags = keyboard.kitty;
    let all = flags & ALL_AS_ESCAPES != 0;
    let event = match (key.kind, flags & EVENTS != 0) {
        (KeyEventKind::Release, false) => return None,
        (KeyEventKind::Repeat, true) => ":2",
        (KeyEventKind::Release, true) => ":3",
        _ => "",
    };
    let shown = if all { 63 | LOCKS } else { 63 };
    let Some(report) = report.cloned().or_else(|| derive(key)) else {
        let param = 1 + (bits(key) & shown);
        return xterm_key(key.code, keyboard.cursor_keys, param, event);
    };
    let mods = report.mods & shown;
    let text_key = matches!(key.code, KeyCode::Char(_)) && (key.modifiers - SHIFT).is_empty();
    if !all {
        if matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        if text_key || matches!(report.code, 9 | 13 | 127) && mods == 0 {
            return match key.kind {
                KeyEventKind::Release => None,
                _ if report.text.is_empty() => plain(key, keyboard.cursor_keys),
                _ => Some(report.text.into_bytes()),
            };
        }
    }
    let shifted = match report.shifted {
        shifted if shifted != 0 && mods & 1 != 0 => shifted.to_string(),
        _ => String::new(),
    };
    let alternates = match (flags & ALTERNATES != 0, report.base) {
        (false, _) => String::new(),
        (true, 0) if shifted.is_empty() => String::new(),
        (true, 0) => format!(":{shifted}"),
        (true, base) => format!(":{shifted}:{base}"),
    };
    let text = match (flags & TEXT != 0 && all, key.kind, key.code) {
        (false, _, _) | (_, KeyEventKind::Release, _) => String::new(),
        _ if !report.text.is_empty() => report.text,
        (_, _, KeyCode::Char(c)) if text_key => c.to_string(),
        _ => String::new(),
    };
    let text: Vec<String> = text
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| u32::from(c).to_string())
        .collect();
    let mods = match (1 + mods, event, text.is_empty()) {
        (1, "", true) => String::new(),
        (1, "", false) => ";".into(),
        (param, event, _) => format!(";{param}{event}"),
    };
    let text = if text.is_empty() {
        String::new()
    } else {
        format!(";{}", text.join(":"))
    };
    Some(format!("\x1b[{}{alternates}{mods}{text}u", report.code).into_bytes())
}

/// What kitty's protocol says of a key the TUI's terminal did not report in
/// it: a text key's code is its unshifted character, the shifted one its
/// alternate; Esc, Enter, Tab and Backspace have codes of their own. The
/// functional keys have none (`None`): they go in their xterm forms.
fn derive(key: KeyEvent) -> Option<Report> {
    let mut mods = bits(key);
    let (code, shifted) = match key.code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                mods |= 1;
            }
            let shifted = u32::from(c.to_ascii_uppercase()) * (mods & 1);
            (u32::from(c.to_ascii_lowercase()), shifted)
        }
        KeyCode::Esc => (27, 0),
        KeyCode::Enter => (13, 0),
        KeyCode::Tab => (9, 0),
        KeyCode::BackTab => {
            mods |= 1;
            (9, 0)
        }
        KeyCode::Backspace => (127, 0),
        _ => return None,
    };
    Some(Report {
        code,
        shifted,
        mods,
        ..Report::default()
    })
}

/// Kitty's modifier bits for `key`, the locks included.
fn bits(key: KeyEvent) -> u32 {
    let held = BITS
        .iter()
        .filter(|(modifier, _)| key.modifiers.contains(*modifier))
        .map(|(_, bit)| bit);
    let locked = LOCK_BITS
        .iter()
        .filter(|(lock, _)| key.state.contains(*lock))
        .map(|(_, bit)| bit);
    held.chain(locked).sum()
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
    let bytes = match key.code {
        KeyCode::Char(c) => encode_char(c, ctrl, alt),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        code => return xterm_key(code, cursor_keys, modifier_param(m), ""),
    };
    Some(bytes)
}

/// xterm's form of a cursor, editing or function key: `CSI final` or `CSI
/// n ~` alone, else with its modifier `param` and kitty's `event` type —
/// `CSI 1 ; param event final`, `CSI n ; param event ~`. Unmodified, the
/// cursor keys and Home and End take `SS3 final` under `cursor_keys`, and F1
/// to F4 always; F3 with an event type is `CSI 13 ~`, as kitty sends it, a
/// `CSI … R` being a cursor report.
fn xterm_key(code: KeyCode, cursor_keys: bool, param: u32, event: &str) -> Option<Vec<u8>> {
    let bare = param == 1 && event.is_empty();
    let letter = |c: char, ss3: bool| match (bare, ss3) {
        (true, true) => format!("\x1bO{c}"),
        (true, false) => format!("\x1b[{c}"),
        (false, _) => format!("\x1b[1;{param}{event}{c}"),
    };
    let tilde = |n: u8| match bare {
        true => format!("\x1b[{n}~"),
        false => format!("\x1b[{n};{param}{event}~"),
    };
    let bytes = match code {
        KeyCode::Up => letter('A', cursor_keys),
        KeyCode::Down => letter('B', cursor_keys),
        KeyCode::Right => letter('C', cursor_keys),
        KeyCode::Left => letter('D', cursor_keys),
        KeyCode::Home => letter('H', cursor_keys),
        KeyCode::End => letter('F', cursor_keys),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(3) if !event.is_empty() => tilde(13),
        KeyCode::F(n @ 1..=4) => letter(char::from(b'P' + n - 1), true),
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]),
        _ => return None,
    };
    Some(bytes.into_bytes())
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
        encode_key(key, None, Keyboard::default())
    }
    fn with(keyboard: Keyboard) -> impl Fn(KeyCode, KeyModifiers) -> String {
        move |code, mods| {
            String::from_utf8(encode_key(keym(code, mods), None, keyboard).unwrap()).unwrap()
        }
    }
    fn kitty_flags(kitty: u8) -> Keyboard {
        Keyboard {
            kitty,
            ..Keyboard::default()
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

    /// Event types (2): a repeat and a release carry theirs, the modifiers
    /// then written out, a functional key's in its xterm form. Text keys and
    /// an unmodified Enter, Tab or Backspace have none — a repeat goes as a
    /// press, a release not at all — unless every key is an escape code
    /// (8). Without 2, or kitty's protocol, a release is nothing and a
    /// repeat a press.
    #[test]
    fn kitty_event_types_reach_a_program_that_asked() {
        let event = |kind, code, mods, flags| {
            let key = KeyEvent::new_with_kind_and_state(code, mods, kind, KeyEventState::NONE);
            encode_key(key, None, kitty_flags(flags)).map(|b| String::from_utf8(b).unwrap())
        };
        use KeyEventKind::{Release, Repeat};
        let some = |s: &str| Some(s.to_string());
        assert_eq!(event(Release, KeyCode::Esc, NONE, 3), some("\x1b[27;1:3u"));
        assert_eq!(
            event(Repeat, KeyCode::Char('a'), CTRL, 3),
            some("\x1b[97;5:2u")
        );
        assert_eq!(event(Release, KeyCode::Up, NONE, 3), some("\x1b[1;1:3A"));
        assert_eq!(event(Release, KeyCode::F(3), NONE, 3), some("\x1b[13;1:3~"));
        assert_eq!(
            event(Repeat, KeyCode::Delete, SHIFT, 3),
            some("\x1b[3;2:2~")
        );
        assert_eq!(event(Release, KeyCode::Char('a'), NONE, 3), None);
        assert_eq!(event(Repeat, KeyCode::Char('a'), NONE, 3), some("a"));
        assert_eq!(event(Release, KeyCode::Enter, NONE, 3), None);
        assert_eq!(
            event(Release, KeyCode::Char('a'), NONE, 10),
            some("\x1b[97;1:3u")
        );
        assert_eq!(
            event(Release, KeyCode::Enter, NONE, 10),
            some("\x1b[13;1:3u")
        );
        assert_eq!(event(Release, KeyCode::Esc, NONE, 1), None);
        assert_eq!(event(Repeat, KeyCode::Esc, NONE, 1), some("\x1b[27u"));
        assert_eq!(event(Release, KeyCode::Char('a'), NONE, 0), None);
        assert_eq!(event(Repeat, KeyCode::Char('a'), NONE, 0), some("a"));
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
            [Input::Key(key, report)] => encode_key(*key, report.as_ref(), kitty_flags(flags))
                .map(|b| String::from_utf8(b).unwrap()),
            inputs => panic!("{bytes:?}: {inputs:?}"),
        };
        let some = |s: &str| Some(s.to_string());
        let all = DISAMBIGUATE | EVENTS | ALTERNATES | ALL_AS_ESCAPES | TEXT;
        for (sent, flags, want) in [
            // Shift+a, and its text.
            (&b"\x1b[97:65;2;65u"[..], all, some("\x1b[97:65;2;65u")),
            (b"\x1b[97:65;2;65u", ALL_AS_ESCAPES, some("\x1b[97;2u")),
            (
                b"\x1b[97:65;2;65u",
                ALL_AS_ESCAPES | TEXT,
                some("\x1b[97;2;65u"),
            ),
            (b"\x1b[97:65;2;65u", DISAMBIGUATE, some("A")),
            // Plain text from a plain terminal, every key an escape code.
            (b"a", all, some("\x1b[97;;97u")),
            (
                b"\x1b[49:33;2;33u",
                DISAMBIGUATE | ALL_AS_ESCAPES,
                some("\x1b[49;2u"),
            ),
            (b"\x1b[49:33;2;33u", DISAMBIGUATE, some("!")),
            // A lone modifier, pressed and released.
            (b"\x1b[57441;2u", all, some("\x1b[57441;2u")),
            (b"\x1b[57441;1:3u", all, some("\x1b[57441;1:3u")),
            (b"\x1b[57441;2u", DISAMBIGUATE | EVENTS, None),
            // Caps Lock: its bit, and the text it made.
            (b"\x1b[97;65;65u", all, some("\x1b[97;65;65u")),
            (b"\x1b[97;65;65u", DISAMBIGUATE, some("A")),
            (b"\x1b[27;65u", DISAMBIGUATE, some("\x1b[27u")),
            // Ctrl+С on a Cyrillic layout: the base-layout key.
            (b"\x1b[1089::99;5u", all, some("\x1b[1089::99;5u")),
            (b"\x1b[1089::99;5u", DISAMBIGUATE, some("\x1b[1089;5u")),
            // Text with no key, as an input method makes it.
            (b"\x1b[0;;101:769u", all, some("\x1b[0;;101:769u")),
            (b"\x1b[0;;101:769u", DISAMBIGUATE, some("e\u{301}")),
            // The keypad: Enter is a key of its own, a digit is text.
            (b"\x1b[57414u", DISAMBIGUATE, some("\x1b[57414u")),
            (b"\x1b[57414u", 0, some("\r")),
            (b"\x1b[57399;;48u", DISAMBIGUATE, some("0")),
            (b"\x1b[57399;;48u", all, some("\x1b[57399;;48u")),
        ] {
            assert_eq!(pass(sent, flags), want, "{sent:?} under {flags}");
        }
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
