//! Translate crossterm key presses into the raw bytes a pty expects. The whole
//! terminal-mode keymap lives here, one function with an exhaustive table, so
//! the escape sequences can be unit-tested without a real terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The bytes a terminal would send for `key`, or `None` when the key has no
/// pty encoding (bare modifier presses, unmapped function keys).
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let param = modifier_param(m);

    let bytes = match key.code {
        KeyCode::Char(c) => encode_char(c, ctrl, alt),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi_final(b'A', param),
        KeyCode::Down => csi_final(b'B', param),
        KeyCode::Right => csi_final(b'C', param),
        KeyCode::Left => csi_final(b'D', param),
        KeyCode::Home => csi_final(b'H', param),
        KeyCode::End => csi_final(b'F', param),
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

    #[test]
    fn plain_and_shifted_chars() {
        assert_eq!(encode_key(key(KeyCode::Char('a'))), Some(b"a".to_vec()));
        assert_eq!(
            encode_key(keym(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some(b"A".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('é'))),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn control_chords() {
        assert_eq!(
            encode_key(keym(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(keym(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(vec![0x01])
        );
        assert_eq!(
            encode_key(keym(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            Some(vec![0x00])
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            encode_key(keym(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            encode_key(keym(
                KeyCode::Char('a'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            )),
            Some(vec![0x1b, 0x01])
        );
    }

    #[test]
    fn named_keys() {
        assert_eq!(encode_key(key(KeyCode::Enter)), Some(vec![b'\r']));
        assert_eq!(encode_key(key(KeyCode::Tab)), Some(vec![b'\t']));
        assert_eq!(encode_key(key(KeyCode::Backspace)), Some(vec![0x7f]));
        assert_eq!(encode_key(key(KeyCode::Esc)), Some(vec![0x1b]));
        assert_eq!(encode_key(key(KeyCode::BackTab)), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn arrows_and_modifiers() {
        assert_eq!(encode_key(key(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(key(KeyCode::Left)), Some(b"\x1b[D".to_vec()));
        assert_eq!(
            encode_key(keym(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            encode_key(keym(KeyCode::Up, KeyModifiers::SHIFT)),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn home_end_and_tilde_keys() {
        assert_eq!(encode_key(key(KeyCode::Home)), Some(b"\x1b[H".to_vec()));
        assert_eq!(encode_key(key(KeyCode::End)), Some(b"\x1b[F".to_vec()));
        assert_eq!(encode_key(key(KeyCode::Delete)), Some(b"\x1b[3~".to_vec()));
        assert_eq!(encode_key(key(KeyCode::PageUp)), Some(b"\x1b[5~".to_vec()));
        assert_eq!(
            encode_key(keym(KeyCode::Delete, KeyModifiers::CONTROL)),
            Some(b"\x1b[3;5~".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(encode_key(key(KeyCode::F(1))), Some(b"\x1bOP".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(4))), Some(b"\x1bOS".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(5))), Some(b"\x1b[15~".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(12))), Some(b"\x1b[24~".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(20))), None);
    }

    #[test]
    fn unmapped_keys_return_none() {
        assert_eq!(encode_key(key(KeyCode::Null)), None);
        assert_eq!(encode_key(key(KeyCode::CapsLock)), None);
    }
}
