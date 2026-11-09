//! `tutti pane attach`: one pane in the terminal you run it in, parsed by
//! that terminal's own emulator. The daemon streams the pane raw
//! (`Request::PaneAttach`) — a snapshot, then every pty read — and keys go
//! back as typed, but for the prefix chord: the prefix then `d` detaches, the
//! prefix twice sends one.
//!
//! The pane is drawn on the alternate screen, left on exit, so the shell's
//! screen and scrollback come back as they were. A pane program's own
//! alternate screen therefore cannot be this terminal's: an rtty-vt mirror of
//! the pane follows the stream, and a read that switches screens, or resets
//! the terminal (`ESC c`), is drawn from the mirror instead of passed through.
//! A program leaving its alternate screen so brings back the pane's primary
//! screen, not the shell's. The daemon sends each switch or reset in a frame
//! of its own, so no other byte goes with the redraw.
//!
//! `--inline` draws on the primary screen instead, for an editor's terminal
//! (Neovim's `:terminal`, Emacs's vterm): what the pane scrolls off goes into
//! that terminal's own scrollback, and the program's screen switches pass
//! through, its alternate screen being free. The pane's history from before
//! the attach is printed first, as lines the terminal wraps itself, then the
//! lines on screen go up into the scrollback before the pane is drawn, and on
//! leaving, the cursor goes under the pane's last line. A snapshot — the
//! first, and each resize's — draws both of the pane's screens from the
//! mirror (`primary`), and so does a reset, which never reaches the
//! terminal: it would turn off the light/dark reports, and some terminals
//! clear their scrollback on it. The first snapshot brings the history, and
//! so does a resync's — a stream that fell behind skips ahead to the pane's
//! screen — with the rows that scrolled off the screen meanwhile, which go
//! into the scrollback over the stale screen (`scroll_off`): it misses no
//! line the pane still holds, and a line tells of any it no longer did.
//!
//! Inline, what the terminal holds of the pane as it leaves — the rows up to
//! the screen, in the pane's numbering, and the screen's down to the last
//! drawn — is kept for this terminal (`printed`), so the next inline attach
//! in it asks for history only past that: from the first of those screen
//! rows the pane has changed since, or the first row after them.
//!
//! This terminal's keys stay plain: the daemon keeps the pane's keyboard
//! protocols (kitty's, modifyOtherKeys) out of the stream, and the mirror's
//! redraws leave them out too — the prefix is watched for in plain bytes,
//! and a detach could not be sure to turn them off.
//!
//! The terminal's colours go to the daemon first, which answers the pane's
//! colour queries with them; and the terminal is asked to tell of its
//! light/dark changes (mode 2031), each of which goes to the daemon too, with
//! the colours asked afresh — never to the pane.

use std::io::{IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::{cursor, terminal};
use rtty_vt::Terminal;
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use tutti_core::{
    Event, Frame, HostColors, PaneDims, PaneId, Printed, Request, Response, row_hash, socket_path,
};

use super::keys::encode_key;
use super::printed;
use super::theme::host_colors;
use crate::client;
use crate::config::{Chord, Config, Theme};

/// Push the window title and ask to hear of light/dark changes; but for
/// `--inline`, take the alternate screen first.
const ENTER: &[u8] = b"\x1b[22;0t\x1b[?2031h";
const ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
/// Turn off what the pane may have turned on in this terminal — mouse
/// reporting, focus events, bracketed paste, application cursor and keypad, a
/// cursor style, a hidden cursor, colours — and the light/dark reports; then
/// leave the alternate screen, and pop the title.
const MODES_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\
\x1b[?1004l\x1b[?2004l\x1b[?2031l\x1b[?1l\x1b>\x1b[0 q\x1b[?25h\x1b[m";
const LEAVE_ALTERNATE: &[u8] = b"\x1b[?1049l";
const POP_TITLE: &[u8] = b"\x1b[23;0t";
/// Inline, the screen a resync's rows are printed on: the cursor hidden
/// until the snapshot shows it, the pane's alternate screen left, the scroll
/// region whole, origin mode and insert off, autowrap on, no rendition, link
/// or other character set, and the primary cleared from the top with ED 0 —
/// which, unlike ED 2, no terminal pushes into its scrollback.
const PLAIN: &[u8] =
    b"\x1b[?25l\x1b[?1049l\x1b[r\x1b[?6l\x1b[?7h\x1b[4l\x1b[0m\x1b]8;;\x1b\\\x1b(B\x0f\x1b[H\x1b[J";

/// Whether the view is `--inline`, and whether the pane's program has this
/// terminal's alternate screen up: what `restore`, a panic's included,
/// leaves.
static INLINE: AtomicBool = AtomicBool::new(false);
static ALTERNATE: AtomicBool = AtomicBool::new(false);

/// A mode-2031 terminal's light/dark reports, each with whether it says dark.
const REPORTS: [(&[u8], bool); 2] = [(b"\x1b[?997;1n", true), (b"\x1b[?997;2n", false)];

/// Show `pane` of `session` in this terminal until the config's prefix and
/// `d` detach it, the pane exits (`[pane N exited: CODE]`), or the daemon
/// goes away (an error) — on the alternate screen, or `inline` on the
/// primary one, after up to `history` of the pane's newest scrollback lines:
/// those this terminal lacks, by what the last inline attach in it left
/// (`printed`), which this one then leaves in its place. Never starts a
/// daemon: with none running there is no pane to show.
pub fn run(
    session: &str,
    pane: PaneId,
    config: &Config,
    inline: bool,
    history: u32,
) -> Result<i32> {
    if !std::io::stdin().is_terminal() {
        bail!("pane attach needs a terminal on stdin");
    }
    let Chord { code, mods } = config.prefix;
    let prefix = encode_key(KeyEvent::new(code, mods), None, None)
        .with_context(|| format!("the prefix {} has no key bytes", config.prefix.label()))?;
    let mut stream = match UnixStream::connect(socket_path(session)) {
        Ok(stream) => stream,
        Err(e) if client::not_running(&e) => {
            bail!("no pane {pane}: no tutti-server is running for session '{session}'")
        }
        Err(e) => return Err(e).context("connecting to tutti-server"),
    };
    if let Some(colors) = host_colors(Theme::Terminal) {
        stream.write_all(&control(&Request::HostColors(colors)))?;
    }
    let (cols, rows) = terminal::size().context("reading the terminal size")?;
    let kept = inline
        .then(printed::terminal)
        .flatten()
        .zip(printed::file());
    let since = kept
        .as_ref()
        .and_then(|(terminal, file)| printed::load(file, terminal, session, pane));
    let attach = Request::PaneAttach {
        pane,
        rows,
        cols,
        history,
        inline,
        since,
    };
    stream.write_all(&control(&attach))?;
    let mut buf = Vec::new();
    let (first, past, mut numbering) = opening(pane, &mut stream, &mut buf)?;
    // Only a daemon from before inline views numbers no rows: it takes the
    // stream for a plain one.
    if inline && numbering.is_none() {
        eprintln!(
            "tutti: the daemon predates this inline view (wire rev 16): it prints the pane's \
             history whole however much of it this terminal holds, and a resync drops the \
             lines it skips — restart it: `tutti server stop`"
        );
    }

    // Inline, the lines on screen go up into the scrollback, up to the
    // cursor's, so the pane's screen is drawn on a blank one and they stay.
    // The pane's history goes before, printed from the cursor's line on, so
    // it scrolls up after them.
    let lift = if inline {
        let mut at = cursor::position().context("reading the cursor position")?;
        if !past.is_empty() {
            emit(&[if at.0 > 0 { b"\r\n".as_slice() } else { b"" }, &past].concat())?;
            at = cursor::position().context("reading the cursor position")?;
        }
        let (col, row) = at;
        format!(
            "\x1b[{rows};1H{}",
            "\n".repeat(usize::from(row + u16::from(col > 0)))
        )
    } else {
        String::new()
    };

    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let resizes = Signals::new([SIGWINCH]).context("watching for SIGWINCH")?;
    terminal::enable_raw_mode().context("entering raw mode")?;
    INLINE.store(inline, Ordering::SeqCst);
    install_panic_hook();
    let detached = Arc::new(AtomicBool::new(false));
    spawn_keys(
        Keys::new(prefix),
        config.escape_time,
        pane,
        &writer,
        &detached,
    );
    spawn_resizes(resizes, pane, &writer);
    let mut mirror = Terminal::new(24, 80, 0);
    let enter = [ENTER, if inline { b"" } else { ALTERNATE_SCREEN }].concat();
    let shown = emit(&enter).and_then(|()| {
        let lift = lift.into_bytes();
        show(
            pane,
            first,
            &mut stream,
            &mut buf,
            &mut mirror,
            lift,
            &mut numbering,
        )
    });
    let left = inline.then(|| {
        let (leave, last) = below(&mut mirror);
        let _ = emit(&leave);
        last
    });
    restore();
    let holds = numbering
        .zip(left)
        .and_then(|(numbering, last)| numbering.holds(&mirror, last));
    if let (Some((terminal, file)), Some(holds)) = (&kept, holds)
        && let Err(err) = printed::save(file, terminal, session, pane, holds)
    {
        eprintln!("tutti: cannot keep what this terminal holds of pane {pane}: {err}");
    }
    match shown? {
        Some(code) => println!("[pane {pane} exited: {code}]"),
        None if detached.load(Ordering::SeqCst) => println!("[detached from pane {pane}]"),
        None => bail!("tutti-server closed the connection"),
    }
    Ok(0)
}

/// How `pane`'s stream opens, up to its first snapshot, which comes back
/// first: a refusal — no such pane — is the error, before the terminal is
/// touched; the colours' `Ok` is passed over; the history, the raw frames
/// before the snapshot, is gathered after a line on what was skipped; and
/// the snapshot's `PaneRows` numbers the rows, which a daemon from before
/// inline views never sends.
fn opening(
    pane: PaneId,
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
) -> Result<(Frame, Vec<u8>, Option<Numbering>)> {
    let (mut past, mut numbering) = (Vec::new(), None);
    loop {
        let frame = read_frame(stream, buf)?.context("tutti-server closed the connection")?;
        let reply = match &frame {
            Frame::Control(json) => serde_json::from_slice(json).ok(),
            _ => None,
        };
        match (reply, frame) {
            (Some(Response::Ok), _) => {}
            (Some(Response::Error { message }), _) if message.starts_with("bad request") => {
                bail!(
                    "the daemon cannot stream a pane ({message}); restart it: `tutti server stop`"
                )
            }
            (Some(Response::Error { message }), _) => bail!(message),
            (_, Frame::PaneRaw { bytes, .. }) => past.extend(bytes),
            (_, Frame::Control(json)) => {
                if let Ok(Event::PaneRows {
                    epoch,
                    top,
                    skipped,
                    ..
                }) = serde_json::from_slice(&json)
                {
                    past = skip_note(pane, skipped);
                    numbering = Some(Numbering::new(epoch, top));
                }
            }
            (_, frame) => return Ok((frame, past, numbering)),
        }
    }
}

/// Where an inline view stands in the pane's numbered rows (`PaneRows`):
/// the daemon run they are numbered in, the absolute row at which the screen
/// of the snapshot they came before starts, and the mirror's own count of
/// rows once that snapshot seeded it — `None` until it has.
struct Numbering {
    epoch: u64,
    top: u64,
    base: Option<u64>,
}

impl Numbering {
    fn new(epoch: u64, top: u64) -> Self {
        Self {
            epoch,
            top,
            base: None,
        }
    }

    /// What a terminal holds of the pane once `mirror` leaves it, drawn down
    /// to its row `last`: every row before the screen, which starts at the
    /// absolute row the mirror's count says, and the screen's rows to `last`
    /// as they stand.
    fn holds(&self, mirror: &Terminal, last: u16) -> Option<Printed> {
        Some(Printed {
            epoch: self.epoch,
            row: self.top + mirror.screen_top().checked_sub(self.base?)?,
            held: (0..=last)
                .map(|row| row_hash(&mirror.screen_row(row).text()))
                .collect(),
        })
    }
}

/// Draw `pane`'s stream, `first` frame on, until the pane exits (its code)
/// or the connection closes (`None`), with `mirror` following it. `lift`
/// goes out before the first snapshot. Inline, the rows a resync hands on —
/// the raw frames between its `PaneRows` and its snapshot — go into this
/// terminal's scrollback first (`scroll_off`), and `numbering` follows each
/// snapshot's `PaneRows`, if it came after one. A refusal on the way is the
/// error.
fn show(
    pane: PaneId,
    first: Frame,
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    mirror: &mut Terminal,
    mut lift: Vec<u8>,
    numbering: &mut Option<Numbering>,
) -> Result<Option<i32>> {
    let inline = INLINE.load(Ordering::SeqCst);
    let view = |mirror: &Terminal| {
        if inline {
            primary(mirror)
        } else {
            redraw(mirror)
        }
    };
    let mut last = 0;
    let mut past: Option<Vec<u8>> = None;
    let mut frame = Some(first);
    loop {
        let next = match frame.take() {
            Some(frame) => frame,
            None => match read_frame(stream, buf) {
                Ok(Some(frame)) => frame,
                Ok(None) | Err(_) => return Ok(None),
            },
        };
        match next {
            Frame::PaneSnapshot(data) => {
                *mirror = Terminal::new(data.rows, data.cols, 0);
                mirror.advance(&data.bytes);
                mirror.take_events();
                *numbering = numbering
                    .take()
                    .filter(|numbered| numbered.base.is_none())
                    .map(|numbered| Numbering {
                        base: Some(mirror.screen_top()),
                        ..numbered
                    });
                let past = past.take().unwrap_or_default();
                let scrolled = if past.is_empty() {
                    Vec::new()
                } else {
                    scroll_off(&past, terminal::size().unwrap_or((80, 24)))
                };
                emit(&[std::mem::take(&mut lift), scrolled, view(mirror)].concat())?;
            }
            Frame::PaneRaw { bytes, .. } if past.is_some() => {
                past.get_or_insert_default().extend(bytes);
            }
            Frame::PaneRaw { bytes, .. } if feed(mirror, &mut last, &bytes, inline) => {
                emit(&view(mirror))?;
            }
            Frame::PaneRaw { bytes, .. } => emit(&bytes)?,
            Frame::Control(json) => {
                if let Ok(Event::PaneExited { code, .. }) = serde_json::from_slice(&json) {
                    return Ok(Some(code));
                }
                if let Ok(Event::PaneRows {
                    epoch,
                    top,
                    skipped,
                    ..
                }) = serde_json::from_slice(&json)
                {
                    past = Some(skip_note(pane, skipped));
                    *numbering = Some(Numbering::new(epoch, top));
                }
                if let Ok(Response::Error { message }) = serde_json::from_slice(&json) {
                    bail!(message);
                }
            }
            _ => {}
        }
        ALTERNATE.store(mirror.modes().alt_screen, Ordering::SeqCst);
    }
}

/// A line on the rows of `pane` left out before the history that follows,
/// if any were.
fn skip_note(pane: PaneId, skipped: u64) -> Vec<u8> {
    if skipped == 0 {
        return Vec::new();
    }
    format!("[pane {pane}: {skipped} lines not shown]\r\n").into_bytes()
}

/// What prints `past` — the rows a resync handed on — into the scrollback
/// of this terminal, `(cols, rows)` big, over the stale screen: a plain
/// primary screen cleared from the top (`PLAIN`), the rows, then as many
/// lines fed at the bottom as they fill of the screen, so the snapshot is
/// drawn on a blank one. A scratch terminal of this one's size counts them:
/// no cursor report can be asked here, as the keys' thread reads the
/// terminal.
fn scroll_off(past: &[u8], (cols, rows): (u16, u16)) -> Vec<u8> {
    let mut scratch = Terminal::new(rows, cols, 0);
    scratch.advance(past);
    let (row, col) = scratch.cursor_position();
    let lift = format!(
        "\x1b[{rows};1H{}",
        "\n".repeat(usize::from(row + u16::from(col > 0)))
    );
    [PLAIN, past, lift.as_bytes()].concat()
}

/// Feed one read of the pane's output to the mirror: true when it reset the
/// terminal, or — but `inline` — switched screens, which this terminal must
/// not be handed raw. A screen switch ends on `h` or `l`, so the mirror is
/// checked after each — one read can switch there and back. `last` is the
/// byte before this read, for an `ESC c` split across two.
fn feed(mirror: &mut Terminal, last: &mut u8, bytes: &[u8], inline: bool) -> bool {
    let mut switched = false;
    for run in bytes.split_inclusive(|b| matches!(b, b'h' | b'l' | b'c')) {
        let alternate = mirror.modes().alt_screen;
        mirror.advance(run);
        let before = run.len().checked_sub(2).map_or(*last, |i| run[i]);
        switched |= !inline && alternate != mirror.modes().alt_screen
            || (before == 0x1b && run.ends_with(b"c"));
        *last = run[run.len() - 1];
    }
    mirror.take_events();
    switched
}

/// The mirror's screen, drawn whole on the alternate screen — taken again
/// first, in case a reset the stream slipped past left it, and cleared — with
/// its cursor and modes.
fn redraw(mirror: &Terminal) -> Vec<u8> {
    let rows: Vec<u16> = (0..mirror.size().0).collect();
    [
        b"\x1b[?1049h\x1b[0m\x1b[H\x1b[2J".as_slice(),
        &strip(mirror.redraw(&rows)),
    ]
    .concat()
}

/// The mirror drawn whole, both screens, on an inline view's primary one —
/// cleared with ED 0 from the top, not the snapshot's ED 2, which many
/// terminals (kitty, alacritty, tmux's scroll-on-clear) push into their
/// scrollback: a copy of the pane's screen there on every resize.
fn primary(mirror: &Terminal) -> Vec<u8> {
    let drawing = String::from_utf8(strip(mirror.snapshot())).expect("rtty-vt writes UTF-8");
    drawing
        .replacen("\x1b[H\x1b[2J", "\x1b[H\x1b[J", 1)
        .into_bytes()
}

/// A mirror's drawing, but for what is this terminal's own: its keyboard
/// protocols, and the light/dark reports `ENTER` asked for.
fn strip(drawing: Vec<u8>) -> Vec<u8> {
    let drawing = String::from_utf8(drawing).expect("rtty-vt writes UTF-8");
    let mut out = String::new();
    let mut rest = drawing.as_str();
    while let Some(at) = rest.find("\x1b[") {
        out.push_str(&rest[..at]);
        let end = rest[at + 2..]
            .find(|c| ('\x40'..='\x7e').contains(&c))
            .map_or(rest.len(), |i| at + i + 3);
        let seq = &rest[at..end];
        let own = (seq.starts_with("\x1b[=") || seq.starts_with("\x1b[>")) && seq.ends_with('u')
            || seq.starts_with("\x1b[>4;") && (seq.ends_with('m') || seq.ends_with('f'))
            || seq.starts_with("\x1b[?2031");
        if !own {
            out.push_str(seq);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out.into_bytes()
}

/// Where an inline view leaves this terminal: the pane's alternate screen
/// left, the scroll region whole, and the cursor on the line under the last
/// of the primary screen's text or its cursor, whichever is lower — that
/// row, the last the terminal keeps, with the bytes.
fn below(mirror: &mut Terminal) -> (Vec<u8>, u16) {
    let leave = if ALTERNATE.swap(false, Ordering::SeqCst) {
        mirror.advance(LEAVE_ALTERNATE);
        LEAVE_ALTERNATE
    } else {
        b""
    };
    let rows = mirror.size().0;
    let last = (0..rows)
        .rev()
        .find(|&r| !mirror.screen_row(r).text().trim_end().is_empty())
        .unwrap_or(0)
        .max(mirror.cursor_position().0);
    let to = format!("\x1b[r\x1b[{};1H\r\n", last + 1);
    ([leave, to.as_bytes()].concat(), last)
}

/// The keys typed into the pane, watched for the prefix chord's bytes — the
/// prefix then `d` detaches, the prefix twice sends one, and the prefix then
/// anything else sends both — and for the terminal's light/dark reports
/// (`REPORTS`), which are tutti's, never the pane's. Either still counts
/// split across reads: the start of one, a lone `ESC` included, is held
/// (`waiting`) until the rest comes — or until the escape time goes by with
/// nothing more (`flush`), when it is what a person typed. So an `ESC` and
/// the key a person typed after it within the escape time reach the pane
/// together, as that key with Alt.
struct Keys {
    prefix: Vec<u8>,
    /// How many of the prefix's bytes the input has matched so far.
    held: usize,
    /// A whole prefix came: the next key decides.
    armed: bool,
    /// The bytes of a report begun so far.
    report: Vec<u8>,
}

impl Keys {
    fn new(prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            held: 0,
            armed: false,
            report: Vec::new(),
        }
    }

    /// What one read of input comes to: the bytes to send, whether it
    /// detached — what follows a detach is dropped — and what its last
    /// light/dark report said (dark: `true`).
    fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool, Option<bool>) {
        let (mut out, mut dark) = (Vec::new(), None);
        for &b in input {
            if self.report.is_empty() && b != 0x1b {
                if self.key(b, &mut out) {
                    return (out, true, dark);
                }
                continue;
            }
            self.report.push(b);
            if let Some(&(_, said)) = REPORTS.iter().find(|(r, _)| *r == self.report) {
                self.report.clear();
                dark = Some(said);
            } else if !REPORTS.iter().any(|(r, _)| r.starts_with(&self.report)) {
                // No report: keys — but for an `ESC`, which may begin one.
                let keys = self.report.len() - usize::from(b == 0x1b);
                for k in self.report.drain(..keys).collect::<Vec<_>>() {
                    if self.key(k, &mut out) {
                        return (out, true, dark);
                    }
                }
            }
        }
        (out, false, dark)
    }

    /// Whether the start of a report or of the prefix is held.
    fn waiting(&self) -> bool {
        !self.report.is_empty() || self.held > 0
    }

    /// Nothing followed what is held within the escape time: it was typed.
    /// The bytes to send, and whether they detached.
    fn flush(&mut self) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        for k in std::mem::take(&mut self.report) {
            if self.key(k, &mut out) {
                return (out, true);
            }
        }
        if self.held > 0 {
            if std::mem::take(&mut self.armed) {
                out.extend_from_slice(&self.prefix);
            }
            out.extend_from_slice(&self.prefix[..std::mem::take(&mut self.held)]);
        }
        (out, false)
    }

    /// Step the prefix watch over one key's byte, adding to `out` what goes
    /// on to the pane; true on a detach.
    fn key(&mut self, b: u8, out: &mut Vec<u8>) -> bool {
        if self.armed && self.held == 0 && b == b'd' {
            return true;
        }
        if self.prefix[self.held] == b {
            self.held += 1;
            if self.held == self.prefix.len() {
                self.held = 0;
                if self.armed {
                    out.extend_from_slice(&self.prefix);
                }
                self.armed = !self.armed;
            }
            return false;
        }
        if std::mem::take(&mut self.armed) {
            out.extend_from_slice(&self.prefix);
        }
        out.extend_from_slice(&self.prefix[..self.held]);
        self.held = usize::from(self.prefix[0] == b);
        if self.held == 0 {
            out.push(b);
        }
        false
    }
}

/// Type stdin into the pane on a thread of its own, what `keys` holds going
/// as typed once `escape` goes by with nothing more. A detach marks
/// `detached` and shuts the socket, which ends `show`.
fn spawn_keys(
    mut keys: Keys,
    escape: Duration,
    pane: PaneId,
    writer: &Arc<Mutex<UnixStream>>,
    detached: &Arc<AtomicBool>,
) {
    let (writer, detached) = (Arc::clone(writer), Arc::clone(detached));
    std::thread::spawn(move || {
        // Larger than stdin's own buffer, so a read goes past it, and what
        // `readable` sees waiting is all there is.
        let mut input = [0u8; 16384];
        loop {
            let (bytes, detach, dark) = if keys.waiting() && !readable(escape) {
                let (bytes, detach) = keys.flush();
                (bytes, detach, None)
            } else {
                // Stdin is locked a read at a time: asking the colours locks
                // it too.
                let n = match std::io::stdin().read(&mut input) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                };
                keys.feed(&input[..n])
            };
            // A light/dark change: the colours are asked afresh, for the
            // daemon, from here — no other read of the terminal is under way,
            // and the query holds stdout, so no pane output lands amid it.
            let report = dark.and_then(|dark| {
                host_colors(Theme::Terminal).map(|colors| HostColors { dark, ..colors })
            });
            let mut stream = writer.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(colors) = report
                && stream
                    .write_all(&control(&Request::HostColors(colors)))
                    .is_err()
            {
                return;
            }
            if !bytes.is_empty()
                && stream
                    .write_all(&Frame::Input { pane, bytes }.encode())
                    .is_err()
            {
                return;
            }
            if detach {
                detached.store(true, Ordering::SeqCst);
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
        }
    });
}

/// Whether stdin has input within `wait`.
fn readable(wait: Duration) -> bool {
    let until = Instant::now() + wait;
    loop {
        let mut stdin = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let left = until.saturating_duration_since(Instant::now()).as_millis();
        // SAFETY: one pollfd, which outlives the call.
        match unsafe { libc::poll(&mut stdin, 1, i32::try_from(left).unwrap_or(i32::MAX)) } {
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            n => return n > 0,
        }
    }
}

/// Send the terminal's size as the pane's view on every SIGWINCH.
fn spawn_resizes(mut signals: Signals, pane: PaneId, writer: &Arc<Mutex<UnixStream>>) {
    let writer = Arc::clone(writer);
    std::thread::spawn(move || {
        for _ in signals.forever() {
            let Ok((cols, rows)) = terminal::size() else {
                continue;
            };
            let view = Request::ViewSizes {
                panes: vec![PaneDims { pane, rows, cols }],
            };
            let mut stream = writer.lock().unwrap_or_else(|e| e.into_inner());
            if stream.write_all(&control(&view)).is_err() {
                return;
            }
        }
    });
}

fn control(request: &Request) -> Vec<u8> {
    Frame::Control(serde_json::to_vec(request).expect("a request serializes")).encode()
}

fn read_frame(stream: &mut UnixStream, buf: &mut Vec<u8>) -> Result<Option<Frame>> {
    loop {
        if let Some((frame, consumed)) = Frame::decode(buf)? {
            buf.drain(..consumed);
            return Ok(Some(frame));
        }
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn emit(bytes: &[u8]) -> Result<()> {
    let mut out = std::io::stdout();
    out.write_all(bytes)?;
    out.flush()?;
    Ok(())
}

/// Hand the terminal back as it was: the modes off, the alternate screen left
/// — inline, only if the pane's program has it up — and the title popped.
/// Safe to run twice.
fn restore() {
    let alternate = !INLINE.load(Ordering::SeqCst) || ALTERNATE.swap(false, Ordering::SeqCst);
    let leave = if alternate { LEAVE_ALTERNATE } else { b"" };
    let _ = emit(&[MODES_OFF, leave, POP_TITLE].concat());
    let _ = terminal::disable_raw_mode();
}

/// Restore the terminal before any panic prints, from whichever thread.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `reads` to a fresh filter one at a time, collecting what goes to
    /// the pane and whether any read detached.
    fn typed(prefix: &[u8], reads: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut keys = Keys::new(prefix.to_vec());
        let mut sent = Vec::new();
        for read in reads {
            let (bytes, detach, _) = keys.feed(read);
            sent.extend(bytes);
            if detach {
                return (sent, true);
            }
        }
        (sent, false)
    }

    /// The terminal's light/dark reports never reach the pane, wherever
    /// reads cut them, and a near miss is keys — an arrow cut after its
    /// `ESC` still arrives whole.
    #[test]
    fn light_dark_reports_are_kept_from_the_pane() {
        let input = b"a\x1b[?997;2nb\x1b[?997;1n\x1b[?997;3n\x1b[Ac";
        let feed = |reads: &[&[u8]]| {
            let mut keys = Keys::new(vec![0x02]);
            let (mut sent, mut said) = (Vec::new(), Vec::new());
            for read in reads {
                let (bytes, _, dark) = keys.feed(read);
                sent.extend(bytes);
                said.extend(dark);
            }
            (sent, said)
        };
        let keys = b"ab\x1b[?997;3n\x1b[Ac".to_vec();
        assert_eq!(
            feed(&[input]),
            (keys.clone(), vec![true]),
            "the read's last"
        );
        for cut in 0..=input.len() {
            let (sent, said) = feed(&[&input[..cut], &input[cut..]]);
            assert_eq!(sent, keys, "cut at {cut}");
            assert_eq!(said.last(), Some(&true), "cut at {cut}");
            // One report a read: both are seen when the cut parts them.
            assert_eq!(
                said.len(),
                1 + usize::from((10..20).contains(&cut)),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn the_prefix_then_d_detaches_after_sending_what_came_before() {
        assert_eq!(typed(b"\x02", &[b"ls\r\x02d"]), (b"ls\r".to_vec(), true));
        assert_eq!(
            typed(b"\x02", &[b"ls\r\x02dtrailing"]),
            (b"ls\r".to_vec(), true)
        );
    }

    #[test]
    fn the_prefix_twice_sends_one_and_before_another_key_both() {
        assert_eq!(typed(b"\x02", &[b"\x02\x02"]), (b"\x02".to_vec(), false));
        assert_eq!(typed(b"\x02", &[b"\x02\x02d"]), (b"\x02d".to_vec(), false));
        assert_eq!(typed(b"\x02", &[b"\x02x"]), (b"\x02x".to_vec(), false));
        assert_eq!(
            typed(b"\x02", &[b"\x02\x02\x02d"]),
            (b"\x02".to_vec(), true)
        );
        assert_eq!(typed(b"\x02", &[b"d\x02"]), (b"d".to_vec(), false));
    }

    /// Every way to cut the input across reads lands the same.
    #[test]
    fn a_prefix_split_across_reads_still_counts() {
        for (prefix, input, want) in [
            (
                b"\x02".as_slice(),
                b"a\x02\x02b\x02d".as_slice(),
                (b"a\x02b".to_vec(), true),
            ),
            (b"\x1bb", b"x\x1bb\x1bbd\x1bbd", (b"x\x1bbd".to_vec(), true)),
            (b"\x1bb", b"\x1bx\x1b\x1bbd", (b"\x1bx\x1b".to_vec(), true)),
            (b"\x1bb", b"\x1bb\x1bxd", (b"\x1bb\x1bxd".to_vec(), false)),
            (b"\x1bb", b"\x1bb\x1bd", (b"\x1bb\x1bd".to_vec(), false)),
        ] {
            assert_eq!(typed(prefix, &[input]), want, "{input:?} whole");
            for cut in 0..=input.len() {
                assert_eq!(
                    typed(prefix, &[&input[..cut], &input[cut..]]),
                    want,
                    "{input:?} cut at {cut}"
                );
            }
            let bytes: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(typed(prefix, &bytes), want, "{input:?} a byte a read");
        }
    }

    /// What is held when the escape time runs out was typed: a lone `ESC`
    /// is Esc, and the start of a report or of an Alt prefix goes on as
    /// keys — but an `ESC` and the key after it within the escape time are
    /// that key with Alt: an `A-b` prefix, or Alt+x for the pane, in one
    /// write.
    #[test]
    fn what_is_held_when_the_escape_time_runs_out_was_typed() {
        let mut keys = Keys::new(b"\x1bb".to_vec());
        let flushed = |keys: &mut Keys| {
            assert!(keys.waiting());
            let flushed = keys.flush();
            assert!(!keys.waiting());
            flushed
        };
        assert_eq!(keys.feed(b"\x1b"), (vec![], false, None));
        assert_eq!(flushed(&mut keys), (b"\x1b".to_vec(), false));
        assert_eq!(keys.feed(b"b"), (b"b".to_vec(), false, None));
        assert_eq!(keys.feed(b"\x1b[?99"), (vec![], false, None));
        assert_eq!(flushed(&mut keys), (b"\x1b[?99".to_vec(), false));
        assert_eq!(keys.feed(b"\x1b"), (vec![], false, None));
        assert_eq!(keys.feed(b"x"), (b"\x1bx".to_vec(), false, None));
        assert_eq!(keys.feed(b"\x1b"), (vec![], false, None));
        assert_eq!(keys.feed(b"b"), (vec![], false, None));
        assert_eq!(keys.feed(b"d"), (vec![], true, None), "Alt+b, then d");

        // The prefix, then an `ESC` the time ran out on: both go.
        let mut keys = Keys::new(b"\x1bb".to_vec());
        assert_eq!(keys.feed(b"\x1bb\x1b"), (vec![], false, None));
        assert_eq!(flushed(&mut keys), (b"\x1bb\x1b".to_vec(), false));
    }

    /// The default prefix is the byte `C-b` types.
    #[test]
    fn the_configured_chord_gives_the_prefix_bytes() {
        let bytes = |spec| {
            let chord = Chord::parse(spec).unwrap();
            encode_key(KeyEvent::new(chord.code, chord.mods), None, None).unwrap()
        };
        assert_eq!(bytes("C-b"), vec![0x02]);
        assert_eq!(bytes("C-a"), vec![0x01]);
        assert_eq!(bytes("A-x"), b"\x1bx".to_vec());
    }

    /// What this terminal shows after `writes`: the stream fed whole where
    /// `feed` passes a read, the mirror's redraw where it does not.
    fn shown(mirror: &mut Terminal, reads: &[&[u8]]) -> (Terminal, Vec<bool>) {
        let (rows, cols) = mirror.size();
        let mut terminal = Terminal::new(rows, cols, 0);
        terminal.advance(&redraw(mirror));
        let mut last = 0;
        let switched = reads
            .iter()
            .map(|read| {
                let switched = feed(mirror, &mut last, read, false);
                terminal.advance(&if switched {
                    redraw(mirror)
                } else {
                    read.to_vec()
                });
                switched
            })
            .collect();
        (terminal, switched)
    }

    /// Every cell of the screen in view, and the cursor.
    fn screen(term: &Terminal) -> (Vec<Vec<rtty_vt::Cell>>, (u16, u16)) {
        let rows = (0..term.size().0).map(|r| term.screen_row(r).cells().to_vec());
        (rows.collect(), term.cursor_position())
    }

    /// A pane program's trip to its alternate screen never reaches this
    /// terminal's: each switch is drawn from the mirror, so leaving brings
    /// back the pane's own screen, and the terminal stays on its alternate.
    #[test]
    fn a_pane_programs_alternate_screen_is_drawn_from_the_mirror() {
        let mut mirror = Terminal::new(6, 20, 0);
        mirror.advance(b"$ ls\r\nfile\r\n$ ");
        let (terminal, switched) = shown(
            &mut mirror,
            &[
                b"vim\r\n\x1b[?1049h\x1b[H\x1b[2Jediting",
                b"\x1b[31mmore\x1b[m",
                b"\x1b[?1049l$ done\r\n$ ",
            ],
        );
        assert_eq!(switched, [true, false, true]);
        assert!(terminal.contents().starts_with("$ ls\nfile\n$ vim\n$ done"));
        assert_eq!(
            screen(&terminal),
            screen(&mirror),
            "the pane's primary screen came back, cursor and all"
        );
        assert!(
            terminal.modes().alt_screen && !mirror.modes().alt_screen,
            "this terminal stays on its alternate screen"
        );
    }

    /// A redraw hands this terminal neither the pane's keyboard protocols
    /// nor a word on its light/dark reports — whatever the mirror holds.
    #[test]
    fn a_redraw_keeps_this_terminals_own_modes() {
        let mut mirror = Terminal::new(4, 20, 0);
        mirror.advance(b"\x1b[>5u\x1b[>4;2m\x1b[>4;1f\x1b[?2031h\x1b[?1049h\x1b[>1uvim");
        let (rows, cols) = mirror.size();
        let bytes = redraw(&mirror);
        let mut terminal = Terminal::new(rows, cols, 0);
        terminal.advance(b"\x1b[?2031h");
        terminal.advance(&bytes);
        assert_eq!(terminal.kitty_keyboard(), 0);
        assert_eq!(terminal.modes().modify_other_keys, 0);
        assert_eq!(terminal.modes().format_other_keys, 0);
        assert!(terminal.modes().color_scheme_reports);
        assert!(terminal.contents().starts_with("vim"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("\x1b[=") && !text.contains("\x1b[>4;") && !text.contains("2031"));
    }

    /// Inline, on the primary screen: the lines on screen go up into the
    /// terminal's scrollback before the pane is drawn, and so do the pane's
    /// own as it scrolls; the program's alternate screen is the terminal's,
    /// passed through; a reset is drawn from the mirror, and keeps the
    /// light/dark reports on; no drawing leaves a copy of the screen in a
    /// scrollback that ED 2 pushes into (this one's); leaving puts the
    /// cursor under the pane's text.
    #[test]
    fn inline_keeps_the_scrollback_and_passes_screen_switches_through() {
        let history = scrollback;
        let mut terminal = Terminal::new(4, 20, 100);
        terminal.advance(b"\x1b[?2031h$ old\r\n$ tutti pane attach\r\n");
        let mut mirror = Terminal::new(4, 20, 0);
        mirror.advance(b"$ ls\r\nfile\r\n$ ");
        // What `run` lifts: the cursor stood at the start of row 2.
        terminal.advance(b"\x1b[4;1H\n\n");
        terminal.advance(&primary(&mirror));
        assert_eq!(history(&terminal), ["$ old", "$ tutti pane attach"]);
        assert_eq!(screen(&terminal), screen(&mirror));
        terminal.advance(&primary(&mirror));
        assert_eq!(history(&terminal), ["$ old", "$ tutti pane attach"]);

        let mut last = 0;
        for read in [
            &b"vim\r\n"[..],
            b"\x1b[?1049h",
            b"\x1b[H\x1b[2Jediting",
            b"\x1b[?1049l",
            b"one\r\ntwo\r\nthree\r\n$ ",
            b"\x1bc",
            b"after",
        ] {
            let redrawn = feed(&mut mirror, &mut last, read, true);
            assert_eq!(redrawn, read == b"\x1bc", "{read:?}");
            terminal.advance(&if redrawn {
                primary(&mirror)
            } else {
                read.to_vec()
            });
            assert_eq!(screen(&terminal), screen(&mirror), "after {read:?}");
            assert_eq!(terminal.modes().alt_screen, mirror.modes().alt_screen);
        }
        assert_eq!(
            history(&terminal),
            ["$ old", "$ tutti pane attach", "$ ls", "file", "$ vim"],
            "the pane's lines scrolled into the terminal's own scrollback, once"
        );
        assert!(terminal.modes().color_scheme_reports, "the reset kept 2031");

        mirror.advance(b"\x1b[?1049hless");
        terminal.advance(b"\x1b[?1049hless");
        ALTERNATE.store(true, Ordering::SeqCst);
        let (leave, last) = below(&mut mirror);
        terminal.advance(&leave);
        assert!(!terminal.modes().alt_screen && !ALTERNATE.load(Ordering::SeqCst));
        assert_eq!(terminal.cursor_position(), (1, 0), "under `after`");
        assert_eq!(last, 0, "the row of `after` is the last kept");
    }

    /// What a terminal holds once an inline view leaves it is numbered as
    /// the pane's rows are: the screen starts at the row the pane's does,
    /// however far the stream scrolled it past the snapshot that numbered
    /// it, with the screen's rows as the pane has them down to the last
    /// drawn — none before a numbered snapshot seeded the mirror.
    #[test]
    fn what_a_terminal_holds_is_numbered_as_the_panes_rows() {
        let mut pane = Terminal::new(4, 20, 100);
        pane.advance(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");
        let mut numbering = Numbering::new(7, pane.screen_top());
        let mut mirror = Terminal::new(4, 20, 0);
        assert_eq!(numbering.holds(&mirror, 0), None, "not seeded yet");
        mirror.advance(&pane.snapshot());
        numbering.base = Some(mirror.screen_top());
        for read in [&b"six\r\nseven\r\n"[..], b"$ ls"] {
            pane.advance(read);
            let mut last = 0;
            feed(&mut mirror, &mut last, read, true);
        }
        // `below`'s: the row of `$ ls`, at the bottom.
        let last = 3;
        let holds = numbering.holds(&mirror, last).unwrap();
        assert_eq!((holds.epoch, holds.row), (7, pane.screen_top()));
        let screen: Vec<u64> = (0..=last)
            .map(|row| row_hash(&pane.screen_row(row).text()))
            .collect();
        assert_eq!(holds.held, screen);
        assert_eq!(holds.held.last(), Some(&row_hash("$ ls")));
    }

    /// What `opening` makes of `frames` from a daemon played over a socket
    /// pair.
    fn opened(frames: &[Frame]) -> Result<(Frame, Vec<u8>, Option<Numbering>)> {
        let (mut ours, mut daemon) = UnixStream::pair().unwrap();
        for frame in frames {
            daemon.write_all(&frame.encode()).unwrap();
        }
        opening(PaneId(3), &mut ours, &mut Vec::new())
    }

    /// A stream opens on its history and the snapshot's numbering — a line
    /// on the rows skipped first — past the colours' `Ok`; a daemon from
    /// before inline views numbers nothing, which `run` warns of; a refusal
    /// is the error.
    #[test]
    fn a_stream_opens_on_its_history_and_numbering() {
        let json = |value: serde_json::Value| Frame::Control(value.to_string().into_bytes());
        let ok = json(serde_json::to_value(Response::Ok).unwrap());
        let rows = json(
            serde_json::to_value(Event::PaneRows {
                pane: PaneId(3),
                epoch: 7,
                top: 40,
                skipped: 2,
            })
            .unwrap(),
        );
        let history = Frame::PaneRaw {
            pane: PaneId(3),
            bytes: b"one\r\n".to_vec(),
        };
        let snapshot = Frame::PaneSnapshot(tutti_core::PaneData {
            pane: PaneId(3),
            rows: 4,
            cols: 20,
            seq: 0,
            bytes: b"screen".to_vec(),
        });

        let numbered = [ok.clone(), rows, history.clone(), snapshot.clone()];
        let (first, past, numbering) = opened(&numbered).unwrap();
        assert_eq!(first, snapshot);
        assert_eq!(past, b"[pane 3: 2 lines not shown]\r\none\r\n");
        let numbering = numbering.expect("numbered");
        assert_eq!((numbering.epoch, numbering.top), (7, 40));

        let old = [ok, history, snapshot.clone()];
        let (first, past, numbering) = opened(&old).unwrap();
        assert_eq!((first, past), (snapshot, b"one\r\n".to_vec()));
        assert!(numbering.is_none(), "an old daemon numbers nothing");

        let refusal = Response::Error {
            message: "no pane 3".into(),
        };
        let refused = opened(&[json(serde_json::to_value(refusal).unwrap())]);
        assert_eq!(
            refused.err().map(|e| e.to_string()).as_deref(),
            Some("no pane 3")
        );
    }

    /// The rows `term` has scrolled into its scrollback, as text.
    fn scrollback(term: &Terminal) -> Vec<String> {
        term.rows(term.oldest_row(), term.screen_top())
            .map(|(_, row)| row.text().trim_end().to_string())
            .collect()
    }

    /// Inline, a resync's rows go into this terminal's scrollback over its
    /// stale screen — the pane's alternate screen and scroll region there
    /// or not, the rows fewer or more than the screen holds — after a line
    /// on those left out, if any; then the snapshot is drawn on a blank
    /// screen. The scrollback runs on from what scrolled before, nothing
    /// missed and nothing twice.
    #[test]
    fn a_resyncs_rows_go_into_the_scrollback_before_its_snapshot() {
        let lines = |n: usize| -> String { (0..n).map(|i| format!("L{i}\r\n")).collect() };
        let names = |rows: std::ops::Range<usize>| rows.map(|i| format!("L{i}")).collect();
        for (stale, printed, from, want) in [
            ("", 8, 3, names(0..5)),
            ("", 21, 3, names(0..18)),
            ("\x1b[?1049h\x1b[2;3rvim", 21, 3, names(0..18)),
            ("", 21, 10, {
                let mut want: Vec<String> = names(0..3);
                want.push("[pane 3: 7 lines not shown]".into());
                want.extend(names(10..18));
                want
            }),
        ] {
            // The terminal had the pane up to its row 3 on screen; the pane
            // went on to print `printed` lines, which the stream skipped.
            let mut terminal = Terminal::new(4, 40, 100);
            terminal.advance(lines(6).as_bytes());
            terminal.advance(stale.as_bytes());
            assert_eq!(scrollback(&terminal), names(0..3));
            let mut pane = Terminal::new(4, 40, 100);
            pane.advance(lines(printed).as_bytes());
            let top = pane.screen_top();
            let past = [
                skip_note(PaneId(3), (from - 3) as u64),
                pane.history(from as u64, top),
            ]
            .concat();
            let mut mirror = Terminal::new(4, 40, 0);
            mirror.advance(&pane.snapshot());

            terminal.advance(&scroll_off(&past, (40, 4)));
            terminal.advance(&primary(&mirror));
            assert_eq!(scrollback(&terminal), want, "{stale:?}, {printed} printed");
            assert_eq!(screen(&terminal), screen(&mirror), "{stale:?}");
            assert!(!terminal.modes().alt_screen);
        }
    }

    /// A read that goes there and back, a switch cut across two reads, and a
    /// reset — whole or cut after its `ESC` — all redraw; plain text and an
    /// `ESC` before other letters pass through.
    #[test]
    fn switches_and_resets_redraw_wherever_they_fall() {
        let mut mirror = Terminal::new(4, 20, 0);
        let (terminal, switched) = shown(
            &mut mirror,
            &[
                b"hello",
                b"\x1b[?1049hless\x1b[?1049l back",
                b"\x1b[?10",
                b"49h alt",
                b"\x1b[?1049l",
                b"x\x1bc",
                b"after\x1b",
                b"c",
                b"\x1b[1mbold\x1b[mchl",
            ],
        );
        assert_eq!(
            switched,
            [false, true, false, true, true, true, false, true, false]
        );
        assert_eq!(terminal.contents(), "boldchl");
        assert!(!mirror.modes().alt_screen);
    }
}
