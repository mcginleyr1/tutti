//! The TUI's own reader of its terminal: a thread that reads the tty and
//! hands each read to the event loop, and the decoder that turns the bytes
//! into keys, mouse events, pastes, focus changes, the replies to what the
//! TUI asks — its terminal's colours (OSC 10/11), whether it speaks kitty's
//! keyboard protocol, DA1 — and the terminal's light/dark reports
//! (`CSI ? 997 ; 1|2 n`, mode 2031), which crossterm's reader would sit on.
//!
//! Keys come out as crossterm's own `KeyEvent`s, the ones its reader made
//! for the same bytes, so every binding reads them as before.

use std::fs::File;
use std::io::{IsTerminal, Read};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode, ModifierKeyCode,
    MouseButton, MouseEvent, MouseEventKind,
};
use tutti_core::{HostColors, Rgb};

/// How long the start of a sequence — a lone `ESC`, `ESC [` — waits for the
/// rest before it is what a person typed: a terminal writes a sequence whole,
/// and nobody types the key after Esc within this.
pub const ESC_WAIT: Duration = Duration::from_millis(15);

/// How long the TUI waits on the DA1 that fences its queries before it goes
/// on with what it heard.
const ASK_WAIT: Duration = Duration::from_secs(1);

const ESC: u8 = 0x1b;
const PASTE_END: &[u8] = b"\x1b[201~";

/// One thing the terminal said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// What the terminal pasted, between its bracketed-paste marks.
    Paste(Vec<u8>),
    /// The terminal gained (`true`) or lost the focus (mode 1004).
    Focus(bool),
    /// A colour asked for: 10 the foreground, 11 the background.
    Color(u8, Rgb),
    /// The terminal turned dark (`true`) or light (mode 2031).
    Scheme(bool),
    /// The primary device attributes (DA1). Every terminal answers, so it
    /// fences the queries sent before it.
    Attributes,
    /// Kitty's keyboard flags in force: the terminal speaks the protocol.
    KeyboardFlags(u8),
    /// XTVERSION's answer: the terminal's name and version.
    Version(String),
    /// DECRPM: a DEC private mode and its state (0 unknown, 1 set, 2 reset).
    Mode(u16, u8),
}

/// What the bytes at the front of the held input come to.
enum Step {
    /// An input, from this many bytes.
    Input(Input, usize),
    /// The start of a bracketed paste, this many bytes long.
    Paste(usize),
    /// A sequence the TUI has no use for, this long.
    Skip(usize),
    /// The start of something unfinished: more is to come.
    More,
}

/// Bytes in, inputs out, a sequence a read cut short held until the rest
/// comes.
#[derive(Default)]
pub struct Decoder {
    /// Bytes read but not yet a whole input.
    held: Vec<u8>,
    /// A bracketed paste under way: its text so far.
    paste: Option<Vec<u8>>,
}

impl Decoder {
    /// Take one read; the inputs it completes.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.held.extend_from_slice(bytes);
        let mut out = Vec::new();
        self.decode(&mut out);
        out
    }

    /// Whether the decoder holds the start of a sequence, which `flush`
    /// takes as typed if nothing follows within `ESC_WAIT`. A paste is never
    /// cut short: it waits for its end mark.
    pub fn waiting(&self) -> bool {
        !self.held.is_empty() && self.paste.is_none()
    }

    /// Nothing followed what is held within `ESC_WAIT`, so a person typed
    /// it: an `ESC` alone is Esc, and with the byte after it that key with
    /// Alt (`ESC [` is Alt+[). What comes after is read afresh; a character
    /// cut short goes.
    pub fn flush(&mut self) -> Vec<Input> {
        let mut out = Vec::new();
        while self.waiting() {
            let held = std::mem::take(&mut self.held);
            if let [ESC, rest @ ..] = held.as_slice() {
                match rest.split_first() {
                    None => out.push(key(KeyCode::Esc, KeyModifiers::NONE)),
                    Some((&b, rest)) => {
                        if b == ESC {
                            out.push(key(KeyCode::Esc, KeyModifiers::ALT));
                        } else if let Step::Input(Input::Key(k), _) = parse(&[b]) {
                            out.push(Input::Key(alt(k)));
                        }
                        self.held = rest.to_vec();
                        self.decode(&mut out);
                    }
                }
            }
        }
        out
    }

    fn decode(&mut self, out: &mut Vec<Input>) {
        let mut at = 0;
        loop {
            if let Some(paste) = &mut self.paste {
                let rest = &self.held[at..];
                match find(rest, PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&rest[..end]);
                        out.push(Input::Paste(std::mem::take(paste)));
                        self.paste = None;
                        at += end + PASTE_END.len();
                        continue;
                    }
                    None => {
                        // Keep what may be the start of the end mark.
                        let keep = (1..PASTE_END.len())
                            .rev()
                            .find(|&n| rest.ends_with(&PASTE_END[..n]))
                            .unwrap_or(0);
                        paste.extend_from_slice(&rest[..rest.len() - keep]);
                        at = self.held.len() - keep;
                        break;
                    }
                }
            }
            if at == self.held.len() {
                break;
            }
            match parse(&self.held[at..]) {
                Step::Input(input, len) => {
                    out.push(input);
                    at += len;
                }
                Step::Paste(len) => {
                    self.paste = Some(Vec::new());
                    at += len;
                }
                Step::Skip(len) => at += len,
                Step::More => break,
            }
        }
        self.held.drain(..at);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Input {
    Input::Key(KeyEvent::new(code, modifiers))
}

fn alt(mut key: KeyEvent) -> KeyEvent {
    key.modifiers |= KeyModifiers::ALT;
    key
}

fn parse(b: &[u8]) -> Step {
    let ctrl = |c: u8| Step::Input(key(KeyCode::Char(char::from(c)), KeyModifiers::CONTROL), 1);
    match b[0] {
        ESC => escape(b),
        b'\r' => Step::Input(key(KeyCode::Enter, KeyModifiers::NONE), 1),
        b'\t' => Step::Input(key(KeyCode::Tab, KeyModifiers::NONE), 1),
        0x7f => Step::Input(key(KeyCode::Backspace, KeyModifiers::NONE), 1),
        0 => ctrl(b' '),
        // Raw mode leaves LF Ctrl+J, and BS Ctrl+H.
        c @ 0x01..=0x1a => ctrl(c - 1 + b'a'),
        c @ 0x1c..=0x1f => ctrl(c - 0x1c + b'4'),
        _ => utf8(b),
    }
}

/// A character, as its first byte says how long it is; an uppercase one
/// with Shift.
fn utf8(b: &[u8]) -> Step {
    let len = match b[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Step::Skip(1),
    };
    if b.len() < len {
        return if b[1..].iter().all(|c| c & 0xc0 == 0x80) {
            Step::More
        } else {
            Step::Skip(1)
        };
    }
    match std::str::from_utf8(&b[..len])
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(c) if c.is_uppercase() => Step::Input(key(KeyCode::Char(c), KeyModifiers::SHIFT), len),
        Some(c) => Step::Input(key(KeyCode::Char(c), KeyModifiers::NONE), len),
        None => Step::Skip(1),
    }
}

fn escape(b: &[u8]) -> Step {
    let Some(&next) = b.get(1) else {
        return Step::More;
    };
    match next {
        b'[' => csi(b),
        b'O' => ss3(b),
        b']' => osc(b),
        b'P' => dcs(b),
        // Alt and whatever follows, a sequence included: `ESC ESC [ A` is
        // Alt+Up.
        _ => match parse(&b[1..]) {
            Step::Input(Input::Key(k), len) => Step::Input(Input::Key(alt(k)), len + 1),
            Step::More => Step::More,
            _ => Step::Skip(1),
        },
    }
}

/// `ESC` and the byte after it as Alt and that key: what typing Alt+[ or
/// Alt+O sends, when no sequence follows.
fn alt_key(b: &[u8]) -> Step {
    match parse(&b[1..2]) {
        Step::Input(Input::Key(k), _) => Step::Input(Input::Key(alt(k)), 2),
        _ => Step::Skip(2),
    }
}

/// `ESC O` and a letter: the cursor and F1–F4 keys in application mode.
fn ss3(b: &[u8]) -> Step {
    let Some(&c) = b.get(2) else {
        return Step::More;
    };
    match legacy_key(c) {
        Some(code) => Step::Input(key(code, KeyModifiers::NONE), 3),
        None => alt_key(b),
    }
}

/// The letter-final keys: cursor keys, Home, End and F1–F4.
fn legacy_key(c: u8) -> Option<KeyCode> {
    Some(match c {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'F' => KeyCode::End,
        b'H' => KeyCode::Home,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        _ => return None,
    })
}

/// A string sequence's end from `from` on: the length up to and with its
/// terminator — BEL, or ST (`ESC \`), or an `ESC` that opens the next
/// sequence, as vte reads it — and where its content ends. `None` when it
/// has yet to end.
fn string_end(b: &[u8], from: usize) -> Option<Option<(usize, usize)>> {
    let at = from + b[from..].iter().position(|&c| c == 0x07 || c == ESC)?;
    Some(match (b[at], b.get(at + 1)) {
        (0x07, _) => Some((at, at + 1)),
        (_, Some(b'\\')) => Some((at, at + 2)),
        (_, Some(_)) => Some((at, at)),
        (_, None) => None,
    })
}

/// The longest string sequence the decoder waits out; past it, the bytes
/// were typed.
const STRING_MAX: usize = 4096;

/// `ESC ]`: an OSC — only the colour replies the TUI asks for (`10;` or
/// `11;` then the colour). Anything else after `ESC ]` was typed: Alt+].
fn osc(b: &[u8]) -> Step {
    let digits = b[2..].iter().take_while(|c| c.is_ascii_digit()).count();
    match b.get(2 + digits) {
        None => return Step::More,
        Some(b';') if digits > 0 => {}
        Some(_) => return alt_key(b),
    }
    match string_end(b, 2) {
        None if b.len() > STRING_MAX => alt_key(b),
        None | Some(None) => Step::More,
        Some(Some((content, len))) => {
            let body = &b[2..content];
            let color = std::str::from_utf8(body).ok().and_then(|s| {
                let (which, spec) = s.split_once(';')?;
                Some((which.parse::<u8>().ok()?, parse_rgb(spec)?))
            });
            match color {
                Some((which @ (10 | 11), rgb)) => Step::Input(Input::Color(which, rgb), len),
                _ => Step::Skip(len),
            }
        }
    }
}

/// `rgb:R/G/B`, each of one to four hex digits, as 16 bits a channel.
fn parse_rgb(spec: &str) -> Option<Rgb> {
    let mut channels = spec.strip_prefix("rgb:")?.split('/').map(|hex| {
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len().clamp(1, 4))) - 1;
        (hex.len() <= 4).then(|| (value * 0xffff / max) as u16)
    });
    let (r, g, b) = (channels.next()??, channels.next()??, channels.next()??);
    channels.next().is_none().then_some(Rgb { r, g, b })
}

/// `ESC P`: a DCS — only XTVERSION's answer (`>|` then the name) and the
/// replies to DECRQSS (`1$r`, `0$r`). Anything else after `ESC P` was
/// typed: Alt+P.
fn dcs(b: &[u8]) -> Step {
    let rest = &b[2..];
    let known: [&[u8]; 3] = [b">|", b"1$r", b"0$r"];
    if known.iter().any(|k| k.starts_with(rest)) {
        return Step::More;
    }
    if !known.iter().any(|k| rest.starts_with(k)) {
        return alt_key(b);
    }
    match string_end(b, 2) {
        None if b.len() > STRING_MAX => alt_key(b),
        None | Some(None) => Step::More,
        Some(Some((content, len))) => match b[2..content].strip_prefix(b">|") {
            Some(name) => Step::Input(
                Input::Version(String::from_utf8_lossy(name).into_owned()),
                len,
            ),
            None => Step::Skip(len),
        },
    }
}

/// `ESC [`: a CSI — keys, mouse reports, focus, a paste's start, and the
/// replies to the TUI's queries.
fn csi(b: &[u8]) -> Step {
    let body = &b[2..];
    match body.first() {
        None => return Step::More,
        // X10 mouse: `ESC [ M` and three bytes.
        Some(b'M') => {
            return match b.get(3..6) {
                Some(&[cb, x, y]) => mouse(
                    cb.wrapping_sub(32),
                    u16::from(x.saturating_sub(33)),
                    u16::from(y.saturating_sub(33)),
                    6,
                ),
                _ => Step::More,
            };
        }
        _ => {}
    }
    let Some(end) = body.iter().position(|c| (0x40..=0x7e).contains(c)) else {
        // Parameters and intermediates only, so far.
        return if body.len() < 64 && body.iter().all(|c| (0x20..=0x3f).contains(c)) {
            Step::More
        } else {
            alt_key(b)
        };
    };
    let (params, fin, len) = (&body[..end], body[end], end + 3);
    if !params.iter().all(|c| (0x20..=0x3f).contains(c)) {
        return alt_key(b);
    }
    if params == b"200" && fin == b'~' {
        return Step::Paste(len);
    }
    let private = params.first().copied().filter(|c| b"<=>?".contains(c));
    let digits = &params[usize::from(private.is_some())..];
    let split = digits.iter().position(|c| (0x20..=0x2f).contains(c));
    let (numbers, inter) = split.map_or((digits, &b""[..]), |at| digits.split_at(at));
    let groups = groups(numbers);
    let n = |group: usize, index: usize| {
        groups
            .get(group)
            .and_then(|g: &Vec<u32>| g.get(index))
            .copied()
    };
    let input = match (private, inter, fin) {
        (Some(b'<'), b"", b'M' | b'm') => {
            let (Some(cb), Some(x), Some(y)) = (n(0, 0), n(1, 0), n(2, 0)) else {
                return Step::Skip(len);
            };
            let to16 = |v: u32| u16::try_from(v.saturating_sub(1)).unwrap_or(u16::MAX);
            return match mouse(cb as u8, to16(x), to16(y), len) {
                Step::Input(Input::Mouse(mut m), len) if fin == b'm' => {
                    if let MouseEventKind::Down(button) = m.kind {
                        m.kind = MouseEventKind::Up(button);
                    }
                    Step::Input(Input::Mouse(m), len)
                }
                step => step,
            };
        }
        (Some(b'?'), b"", b'u') => Input::KeyboardFlags(n(0, 0).unwrap_or(0) as u8),
        (Some(b'?'), b"", b'c') => Input::Attributes,
        (Some(b'?'), b"", b'n') => match (n(0, 0), n(1, 0)) {
            (Some(997), Some(1)) => Input::Scheme(true),
            (Some(997), Some(2)) => Input::Scheme(false),
            _ => return Step::Skip(len),
        },
        (Some(b'?'), b"$", b'y') => match (n(0, 0), n(1, 0)) {
            (Some(mode), Some(state)) => Input::Mode(mode as u16, state as u8),
            _ => return Step::Skip(len),
        },
        (None, b"", b'I') if numbers.is_empty() => Input::Focus(true),
        (None, b"", b'O') if numbers.is_empty() => Input::Focus(false),
        (None, b"", b'Z') => key(KeyCode::BackTab, KeyModifiers::SHIFT),
        (None, b"", b'~') => match tilde(&groups) {
            Some(input) => input,
            None => return Step::Skip(len),
        },
        (None, b"", b'u') => match csi_u(&groups) {
            Some(input) => input,
            None => return Step::Skip(len),
        },
        // urxvt mouse: `ESC [ Cb ; Cx ; Cy M`.
        (None, b"", b'M') => {
            let (Some(cb), Some(x), Some(y)) = (n(0, 0), n(1, 0), n(2, 0)) else {
                return Step::Skip(len);
            };
            let to16 = |v: u32| u16::try_from(v.saturating_sub(1)).unwrap_or(u16::MAX);
            return mouse((cb as u8).wrapping_sub(32), to16(x), to16(y), len);
        }
        (None, b"", c) => match legacy_key(c) {
            Some(code) => {
                let (modifiers, kind, state) = modifiers(n(1, 0), n(1, 1));
                Input::Key(KeyEvent::new_with_kind_and_state(
                    code, modifiers, kind, state,
                ))
            }
            None => return Step::Skip(len),
        },
        _ => return Step::Skip(len),
    };
    Step::Input(input, len)
}

/// A CSI's numeric parameters: `;` between groups, `:` within one, a
/// missing number 0.
fn groups(numbers: &[u8]) -> Vec<Vec<u32>> {
    if numbers.is_empty() {
        return Vec::new();
    }
    numbers
        .split(|&c| c == b';')
        .map(|group| {
            group
                .split(|&c| c == b':')
                .map(|n| {
                    n.iter().fold(0u32, |acc, d| {
                        acc.saturating_mul(10).saturating_add(u32::from(d - b'0'))
                    })
                })
                .collect()
        })
        .collect()
}

/// The modifiers, event kind and lock state a key's `1 + bits` parameter
/// and its event-type sub-parameter carry.
fn modifiers(mask: Option<u32>, kind: Option<u32>) -> (KeyModifiers, KeyEventKind, KeyEventState) {
    let bits = mask.unwrap_or(1).saturating_sub(1);
    let mut modifiers = KeyModifiers::NONE;
    for (bit, modifier) in [
        (1, KeyModifiers::SHIFT),
        (2, KeyModifiers::ALT),
        (4, KeyModifiers::CONTROL),
        (8, KeyModifiers::SUPER),
        (16, KeyModifiers::HYPER),
        (32, KeyModifiers::META),
    ] {
        if bits & bit != 0 {
            modifiers |= modifier;
        }
    }
    let mut state = KeyEventState::NONE;
    if bits & 64 != 0 {
        state |= KeyEventState::CAPS_LOCK;
    }
    if bits & 128 != 0 {
        state |= KeyEventState::NUM_LOCK;
    }
    let kind = match kind {
        Some(2) => KeyEventKind::Repeat,
        Some(3) => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    };
    (modifiers, kind, state)
}

/// `ESC [ n ; m ~`: the editing and function keys, and xterm's
/// modifyOtherKeys form, `ESC [ 27 ; m ; code ~`.
fn tilde(groups: &[Vec<u32>]) -> Option<Input> {
    let n = |group: usize, index: usize| groups.get(group).and_then(|g| g.get(index)).copied();
    let (modifiers, kind, state) = modifiers(n(1, 0), n(1, 1));
    let code = match n(0, 0)? {
        27 => {
            let code = char_key(n(2, 0)?, modifiers)?;
            return Some(Input::Key(KeyEvent::new_with_kind_and_state(
                code, modifiers, kind, state,
            )));
        }
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        v @ 11..=15 => KeyCode::F((v - 10) as u8),
        v @ 17..=21 => KeyCode::F((v - 11) as u8),
        v @ 23..=26 => KeyCode::F((v - 12) as u8),
        v @ 28..=29 => KeyCode::F((v - 15) as u8),
        v @ 31..=34 => KeyCode::F((v - 17) as u8),
        _ => return None,
    };
    Some(Input::Key(KeyEvent::new_with_kind_and_state(
        code, modifiers, kind, state,
    )))
}

/// A key by its Unicode code: the controls that are keys of their own, else
/// the character.
fn char_key(code: u32, modifiers: KeyModifiers) -> Option<KeyCode> {
    Some(match code {
        27 => KeyCode::Esc,
        13 => KeyCode::Enter,
        9 if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        9 => KeyCode::Tab,
        127 => KeyCode::Backspace,
        code => KeyCode::Char(char::from_u32(code)?),
    })
}

/// `ESC [ code[:shifted[:base]] ; mods[:kind] [; text] u`: kitty's keyboard
/// protocol. A shifted key's alternate stands for the key, Shift then
/// spent, as crossterm read it.
fn csi_u(groups: &[Vec<u32>]) -> Option<Input> {
    let n = |group: usize, index: usize| groups.get(group).and_then(|g| g.get(index)).copied();
    let codepoint = n(0, 0)?;
    let (mut modifiers, kind, mut state) = modifiers(n(1, 0), n(1, 1));
    let mut code = match functional(codepoint) {
        Some((code, keypad)) => {
            if keypad {
                state |= KeyEventState::KEYPAD;
            }
            code
        }
        None => char_key(codepoint, modifiers)?,
    };
    if let KeyCode::Modifier(held) = code {
        modifiers |= match held {
            ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => KeyModifiers::SHIFT,
            ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => KeyModifiers::CONTROL,
            ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => KeyModifiers::ALT,
            ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper => KeyModifiers::SUPER,
            ModifierKeyCode::LeftHyper | ModifierKeyCode::RightHyper => KeyModifiers::HYPER,
            ModifierKeyCode::LeftMeta | ModifierKeyCode::RightMeta => KeyModifiers::META,
            _ => KeyModifiers::NONE,
        };
    }
    if modifiers.contains(KeyModifiers::SHIFT)
        && let Some(shifted) = n(0, 1).filter(|&c| c != 0).and_then(char::from_u32)
    {
        code = KeyCode::Char(shifted);
        modifiers.remove(KeyModifiers::SHIFT);
    }
    Some(Input::Key(KeyEvent::new_with_kind_and_state(
        code, modifiers, kind, state,
    )))
}

/// Kitty's codes for the keys that have no character, from its private-use
/// range; `true` for the keypad's.
fn functional(code: u32) -> Option<(KeyCode, bool)> {
    let keypad = |code| Some((code, true));
    match code {
        57399..=57408 => keypad(KeyCode::Char(char::from(b'0' + (code - 57399) as u8))),
        57409 => keypad(KeyCode::Char('.')),
        57410 => keypad(KeyCode::Char('/')),
        57411 => keypad(KeyCode::Char('*')),
        57412 => keypad(KeyCode::Char('-')),
        57413 => keypad(KeyCode::Char('+')),
        57414 => keypad(KeyCode::Enter),
        57415 => keypad(KeyCode::Char('=')),
        57416 => keypad(KeyCode::Char(',')),
        57417 => keypad(KeyCode::Left),
        57418 => keypad(KeyCode::Right),
        57419 => keypad(KeyCode::Up),
        57420 => keypad(KeyCode::Down),
        57421 => keypad(KeyCode::PageUp),
        57422 => keypad(KeyCode::PageDown),
        57423 => keypad(KeyCode::Home),
        57424 => keypad(KeyCode::End),
        57425 => keypad(KeyCode::Insert),
        57426 => keypad(KeyCode::Delete),
        57427 => keypad(KeyCode::KeypadBegin),
        57358 => Some((KeyCode::CapsLock, false)),
        57359 => Some((KeyCode::ScrollLock, false)),
        57360 => Some((KeyCode::NumLock, false)),
        57361 => Some((KeyCode::PrintScreen, false)),
        57362 => Some((KeyCode::Pause, false)),
        57363 => Some((KeyCode::Menu, false)),
        57376..=57398 => Some((KeyCode::F((code - 57376 + 13) as u8), false)),
        57428..=57440 => {
            let media = [
                MediaKeyCode::Play,
                MediaKeyCode::Pause,
                MediaKeyCode::PlayPause,
                MediaKeyCode::Reverse,
                MediaKeyCode::Stop,
                MediaKeyCode::FastForward,
                MediaKeyCode::Rewind,
                MediaKeyCode::TrackNext,
                MediaKeyCode::TrackPrevious,
                MediaKeyCode::Record,
                MediaKeyCode::LowerVolume,
                MediaKeyCode::RaiseVolume,
                MediaKeyCode::MuteVolume,
            ];
            Some((KeyCode::Media(media[(code - 57428) as usize]), false))
        }
        57441..=57454 => {
            let held = [
                ModifierKeyCode::LeftShift,
                ModifierKeyCode::LeftControl,
                ModifierKeyCode::LeftAlt,
                ModifierKeyCode::LeftSuper,
                ModifierKeyCode::LeftHyper,
                ModifierKeyCode::LeftMeta,
                ModifierKeyCode::RightShift,
                ModifierKeyCode::RightControl,
                ModifierKeyCode::RightAlt,
                ModifierKeyCode::RightSuper,
                ModifierKeyCode::RightHyper,
                ModifierKeyCode::RightMeta,
                ModifierKeyCode::IsoLevel3Shift,
                ModifierKeyCode::IsoLevel5Shift,
            ];
            Some((KeyCode::Modifier(held[(code - 57441) as usize]), false))
        }
        _ => None,
    }
}

/// A mouse report's button byte as an event: its low two bits and top two
/// the button, 32 a drag, then Shift (4), Alt (8) and Ctrl (16).
fn mouse(cb: u8, column: u16, row: u16, len: usize) -> Step {
    let button = (cb & 0b11) | ((cb & 0b1100_0000) >> 4);
    let kind = match (button, cb & 32 != 0) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return Step::Skip(len),
    };
    let mut modifiers = KeyModifiers::NONE;
    for (bit, modifier) in [
        (4, KeyModifiers::SHIFT),
        (8, KeyModifiers::ALT),
        (16, KeyModifiers::CONTROL),
    ] {
        if cb & bit != 0 {
            modifiers |= modifier;
        }
    }
    Step::Input(
        Input::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers,
        }),
        len,
    )
}

/// Read the terminal — stdin, else the controlling tty — on a thread of its
/// own, handing each read to `reads` until either ends.
pub fn spawn(reads: Sender<Vec<u8>>) -> std::io::Result<()> {
    let mut tty: Box<dyn Read + Send> = if std::io::stdin().is_terminal() {
        Box::new(std::io::stdin())
    } else {
        Box::new(File::open("/dev/tty")?)
    };
    std::thread::Builder::new()
        .name("tutti-tty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match tty.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        if reads.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        })?;
    Ok(())
}

/// Whether the terminal speaks kitty's keyboard protocol: one that does
/// answers with its flags (`Input::KeyboardFlags`), and that is all there is
/// to hear.
pub const ASK_KEYBOARD: &[u8] = b"\x1b[?u";

/// The terminal's colours (OSC 10/11), asked and yet to be heard in full:
/// a DA1 goes after the queries — every terminal answers it — so its reply
/// says the rest is in.
pub struct Asked {
    at: Instant,
    /// The light/dark a 997 report said, which the colours go out with.
    dark: Option<bool>,
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    /// The DA1s yet to come back: an ask that takes over from one still
    /// out waits for its fence too, so the colours last told are the ones
    /// that count.
    fences: u32,
}

impl Asked {
    /// Ask the colours — with `dark`, what a light/dark report said — taking
    /// over from `before`, still out: the bytes to write, and what keeps the
    /// answers.
    pub fn ask(dark: Option<bool>, before: Option<&Asked>) -> (Self, Vec<u8>) {
        let asked = Asked {
            at: Instant::now(),
            dark,
            fg: before.and_then(|b| b.fg),
            bg: before.and_then(|b| b.bg),
            fences: 1 + before.map_or(0, |b| b.fences),
        };
        (asked, b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c".to_vec())
    }

    /// Take `input` if it answers. Once the last DA1 is in, or `ASK_WAIT`
    /// has gone by without it: done, with the colours when the terminal
    /// gave both.
    pub fn take(&mut self, input: &Input) -> Option<Option<HostColors>> {
        match input {
            Input::Color(10, rgb) => self.fg = Some(*rgb),
            Input::Color(11, rgb) => self.bg = Some(*rgb),
            Input::Attributes => {
                self.fences = self.fences.saturating_sub(1);
                return (self.fences == 0).then(|| self.colors());
            }
            _ => {}
        }
        None
    }

    /// `ASK_WAIT` went by with a DA1 still out: done with what came.
    pub fn expired(&self) -> Option<Option<HostColors>> {
        (self.at.elapsed() >= ASK_WAIT).then(|| self.colors())
    }

    fn colors(&self) -> Option<HostColors> {
        let (fg, bg) = (self.fg?, self.bg?);
        Some(HostColors {
            fg,
            bg,
            cursor: None,
            dark: self.dark.unwrap_or(luma(bg) < luma(fg)),
        })
    }
}

/// A colour's relative luminance, for which of two is the darker.
fn luma(c: Rgb) -> f64 {
    0.2126 * f64::from(c.r) + 0.7152 * f64::from(c.g) + 0.0722 * f64::from(c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::input::{Keyboard, encode_key};

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    fn k(code: KeyCode, modifiers: KeyModifiers) -> Input {
        key(code, modifiers)
    }

    fn c(ch: char, modifiers: KeyModifiers) -> Input {
        key(KeyCode::Char(ch), modifiers)
    }

    fn click(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> Input {
        Input::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers,
        })
    }

    fn rgb(r: u16, g: u16, b: u16) -> Rgb {
        Rgb { r, g, b }
    }

    /// Every input the TUI reads, each as the bytes a terminal sends for it.
    fn corpus() -> Vec<(&'static [u8], Vec<Input>)> {
        let left = MouseButton::Left;
        vec![
            (b"a", vec![c('a', NONE)]),
            (b"A", vec![c('A', SHIFT)]),
            ("é日".as_bytes(), vec![c('é', NONE), c('日', NONE)]),
            (b"\x08\x0c\x0b", vec![c('h', CTRL), c('l', CTRL), c('k', CTRL)]),
            (b"\x02\x00\x1c", vec![c('b', CTRL), c(' ', CTRL), c('4', CTRL)]),
            (
                b"\r\t\x7f\n",
                vec![
                    k(KeyCode::Enter, NONE),
                    k(KeyCode::Tab, NONE),
                    k(KeyCode::Backspace, NONE),
                    c('j', CTRL),
                ],
            ),
            (b"\x1bx\x1bl", vec![c('x', ALT), c('l', ALT)]),
            (b"\x1b\x1b[A", vec![k(KeyCode::Up, ALT)]),
            ("\x1bé".as_bytes(), vec![c('é', ALT)]),
            (
                b"\x1b[A\x1b[1;5D\x1bOP\x1b[1;2Q\x1b[15~\x1b[3;2~\x1b[Z\x1b[5~",
                vec![
                    k(KeyCode::Up, NONE),
                    k(KeyCode::Left, CTRL),
                    k(KeyCode::F(1), NONE),
                    k(KeyCode::F(2), SHIFT),
                    k(KeyCode::F(5), NONE),
                    k(KeyCode::Delete, SHIFT),
                    k(KeyCode::BackTab, SHIFT),
                    k(KeyCode::PageUp, NONE),
                ],
            ),
            // Kitty's keyboard protocol: Ctrl+H and Ctrl+I apart from
            // Backspace and Tab, Esc apart from Alt, a shifted key's
            // alternate, the keypad.
            (
                b"\x1b[104;5u\x1b[105;5u\x1b[27u\x1b[120;3u\x1b[97:65;2u",
                vec![
                    c('h', CTRL),
                    c('i', CTRL),
                    k(KeyCode::Esc, NONE),
                    c('x', ALT),
                    c('A', NONE),
                ],
            ),
            (
                b"\x1b[57414u",
                vec![Input::Key(KeyEvent::new_with_kind_and_state(
                    KeyCode::Enter,
                    NONE,
                    KeyEventKind::Press,
                    KeyEventState::KEYPAD,
                ))],
            ),
            // xterm's modifyOtherKeys.
            (b"\x1b[27;5;105~", vec![c('i', CTRL)]),
            (
                b"\x1b[<0;10;5M\x1b[<32;11;5M\x1b[<0;11;5m\x1b[<64;1;1M\x1b[<65;1;1M\x1b[<35;3;4M\x1b[<4;2;2M",
                vec![
                    click(MouseEventKind::Down(left), 9, 4, NONE),
                    click(MouseEventKind::Drag(left), 10, 4, NONE),
                    click(MouseEventKind::Up(left), 10, 4, NONE),
                    click(MouseEventKind::ScrollUp, 0, 0, NONE),
                    click(MouseEventKind::ScrollDown, 0, 0, NONE),
                    click(MouseEventKind::Moved, 2, 3, NONE),
                    click(MouseEventKind::Down(left), 1, 1, SHIFT),
                ],
            ),
            (b"\x1b[M !!", vec![click(MouseEventKind::Down(left), 0, 0, NONE)]),
            (b"\x1b[I\x1b[O", vec![Input::Focus(true), Input::Focus(false)]),
            (
                b"\x1b[200~hi\x1b[A\r\n\x1b[20x\x1b[201~",
                vec![Input::Paste(b"hi\x1b[A\r\n\x1b[20x".to_vec())],
            ),
            (
                b"\x1b[?997;1n\x1b[?997;2n",
                vec![Input::Scheme(true), Input::Scheme(false)],
            ),
            (
                b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\\x1b]10;rgb:65/7b/83\x07",
                vec![
                    Input::Color(11, rgb(0xfdfd, 0xf6f6, 0xe3e3)),
                    Input::Color(10, rgb(0x6565, 0x7b7b, 0x8383)),
                ],
            ),
            (
                b"\x1b[?62;22c\x1b[?5u\x1b[?2031;2$y",
                vec![
                    Input::Attributes,
                    Input::KeyboardFlags(5),
                    Input::Mode(2031, 2),
                ],
            ),
            (
                b"\x1bP>|rtty-vt 0.1\x1b\\",
                vec![Input::Version("rtty-vt 0.1".into())],
            ),
            // What the TUI has no use for goes, and what only looks like the
            // start of a reply was typed: Alt+] and Alt+P, then the key.
            (b"\x1b[?1;2x\x1b]x\x1bPx", vec![c(']', ALT), c('x', NONE), c('P', SHIFT | ALT), c('x', NONE)]),
        ]
    }

    fn decode(reads: &[&[u8]]) -> Vec<Input> {
        let mut decoder = Decoder::default();
        let mut out: Vec<Input> = reads.iter().flat_map(|read| decoder.feed(read)).collect();
        out.extend(decoder.flush());
        out
    }

    #[test]
    fn every_input_decodes_from_its_bytes() {
        for (bytes, want) in corpus() {
            assert_eq!(decode(&[bytes]), want, "{bytes:?}");
        }
    }

    /// However reads cut the stream — a sequence split across two, or a
    /// byte a read — it decodes the same.
    #[test]
    fn a_stream_decodes_the_same_however_reads_cut_it() {
        let (stream, want): (Vec<u8>, Vec<Input>) = corpus().into_iter().fold(
            (Vec::new(), Vec::new()),
            |(mut stream, mut want), (bytes, inputs)| {
                stream.extend_from_slice(bytes);
                want.extend(inputs);
                (stream, want)
            },
        );
        for cut in 0..=stream.len() {
            assert_eq!(
                decode(&[&stream[..cut], &stream[cut..]]),
                want,
                "cut at {cut}"
            );
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(decode(&bytes), want, "a byte a read");
    }

    /// What stays the start of a sequence until the wait runs out was typed:
    /// Esc alone, or Alt and the key after `ESC`, then whatever followed.
    /// A paste is never cut short: it waits for its end mark.
    #[test]
    fn a_wait_with_nothing_after_takes_what_is_held_as_typed() {
        for (held, want) in [
            (&b"\x1b"[..], vec![k(KeyCode::Esc, NONE)]),
            (b"\x1b[", vec![c('[', ALT)]),
            (b"\x1b[1;", vec![c('[', ALT), c('1', NONE), c(';', NONE)]),
            (b"\x1b]1", vec![c(']', ALT), c('1', NONE)]),
            (b"\x1bP", vec![c('P', SHIFT | ALT)]),
            (b"\x1bO", vec![c('O', SHIFT | ALT)]),
            (b"\x1b\x1b", vec![k(KeyCode::Esc, ALT)]),
            (b"\x1b\x1b[", vec![k(KeyCode::Esc, ALT), c('[', NONE)]),
        ] {
            let mut decoder = Decoder::default();
            assert_eq!(decoder.feed(held), vec![], "{held:?}");
            assert!(decoder.waiting(), "{held:?}");
            assert_eq!(decoder.flush(), want, "{held:?}");
            assert!(!decoder.waiting());
        }
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(b"\x1b[200~abc\x1b"), vec![]);
        assert!(!decoder.waiting(), "a paste waits for its end");
        assert_eq!(decoder.flush(), vec![]);
        assert_eq!(
            decoder.feed(b"[201~x"),
            vec![Input::Paste(b"abc".to_vec()), c('x', NONE)]
        );
    }

    /// The decoder reads back what tutti's own key encoder sends a pane —
    /// plain keys, and kitty's protocol, which tells apart what plain bytes
    /// cannot.
    #[test]
    fn keys_the_encoder_writes_decode_back_to_themselves() {
        let plain = [
            KeyEvent::new(KeyCode::Char('q'), NONE),
            KeyEvent::new(KeyCode::Char('Q'), SHIFT),
            KeyEvent::new(KeyCode::Char('h'), CTRL),
            KeyEvent::new(KeyCode::Char('x'), ALT),
            KeyEvent::new(KeyCode::Enter, NONE),
            KeyEvent::new(KeyCode::Backspace, NONE),
            KeyEvent::new(KeyCode::Esc, NONE),
            KeyEvent::new(KeyCode::Up, NONE),
            KeyEvent::new(KeyCode::Right, CTRL),
            KeyEvent::new(KeyCode::Home, NONE),
            KeyEvent::new(KeyCode::Delete, NONE),
            KeyEvent::new(KeyCode::PageDown, NONE),
            KeyEvent::new(KeyCode::F(1), NONE),
            KeyEvent::new(KeyCode::F(12), NONE),
            KeyEvent::new(KeyCode::BackTab, SHIFT),
        ];
        let kitty = Keyboard {
            kitty: 1 | 4,
            ..Keyboard::default()
        };
        let disambiguated = [
            KeyEvent::new(KeyCode::Char('i'), CTRL),
            KeyEvent::new(KeyCode::Char('h'), CTRL),
            KeyEvent::new(KeyCode::Esc, NONE),
            KeyEvent::new(KeyCode::Char('x'), ALT),
        ];
        let cases = plain.into_iter().map(|key| (key, Keyboard::default()));
        for (key, keyboard) in cases.chain(disambiguated.into_iter().map(|key| (key, kitty))) {
            let bytes = encode_key(key, keyboard).unwrap();
            assert_eq!(
                decode(&[&bytes]),
                vec![Input::Key(key)],
                "{key:?} as {bytes:?}"
            );
        }
    }

    /// The colours are in with the DA1 that fences them — the last DA1 out,
    /// when an ask took over from one still out, so the colours last told
    /// count. Light or dark is a 997 report's word, else whichever of the
    /// two is the darker.
    #[test]
    fn the_colours_are_in_once_the_last_da1_is() {
        let (dark_fg, dark_bg) = (rgb(0xabab, 0xb2b2, 0xbfbf), rgb(0x2828, 0x2c2c, 0x3434));
        let (light_fg, light_bg) = (rgb(0x6565, 0x7b7b, 0x8383), rgb(0xfdfd, 0xf6f6, 0xe3e3));
        let (mut asked, query) = Asked::ask(None, None);
        assert_eq!(query, b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c");
        for input in [
            Input::Color(10, dark_fg),
            Input::Color(11, dark_bg),
            c('x', NONE),
        ] {
            assert_eq!(asked.take(&input), None);
        }
        let dark = HostColors {
            fg: dark_fg,
            bg: dark_bg,
            cursor: None,
            dark: true,
        };
        assert_eq!(asked.take(&Input::Attributes), Some(Some(dark)));

        // A flip while the first ask is out: the new one waits for both
        // fences, and the second answers win.
        let (first, _) = Asked::ask(None, None);
        let (mut asked, _) = Asked::ask(Some(false), Some(&first));
        for input in [
            Input::Color(10, dark_fg),
            Input::Color(11, dark_bg),
            Input::Attributes,
            Input::Color(10, light_fg),
            Input::Color(11, light_bg),
        ] {
            assert_eq!(asked.take(&input), None, "{input:?}");
        }
        let light = HostColors {
            fg: light_fg,
            bg: light_bg,
            cursor: None,
            dark: false,
        };
        assert_eq!(asked.take(&Input::Attributes), Some(Some(light)));

        let (mut asked, _) = Asked::ask(None, None);
        assert_eq!(
            asked.take(&Input::Attributes),
            Some(None),
            "a terminal that tells no colours"
        );
        assert_eq!(asked.expired(), None, "not yet");
    }
}
