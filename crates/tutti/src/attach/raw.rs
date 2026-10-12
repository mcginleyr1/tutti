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

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::terminal;
use rtty_vt::Terminal;
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use tutti_core::{Event, Frame, HostColors, PaneDims, PaneId, Request, Response, socket_path};

use super::input::encode_key;
use super::theme::host_colors;
use crate::client;
use crate::config::{Chord, Theme};

/// Push the window title, take the alternate screen, and ask to hear of
/// light/dark changes.
const ENTER: &[u8] = b"\x1b[22;0t\x1b[?1049h\x1b[?2031h";
/// Turn off what the pane may have turned on in this terminal — mouse
/// reporting, focus events, bracketed paste, application cursor and keypad, a
/// cursor style, a hidden cursor, colours — and the light/dark reports, then
/// leave the alternate screen and pop the title.
const LEAVE: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\
\x1b[?1004l\x1b[?2004l\x1b[?2031l\x1b[?1l\x1b>\x1b[0 q\x1b[?25h\x1b[m\x1b[?1049l\x1b[23;0t";

/// A mode-2031 terminal's light/dark reports, each with whether it says dark.
const REPORTS: [(&[u8], bool); 2] = [(b"\x1b[?997;1n", true), (b"\x1b[?997;2n", false)];

/// Show `pane` of `session` in this terminal until the prefix and `d` detach
/// it, the pane exits (`[pane N exited: CODE]`), or the daemon goes away (an
/// error). Never starts a daemon: with none running there is no pane to show.
pub fn run(session: &str, pane: PaneId, prefix: Chord) -> Result<i32> {
    if !std::io::stdin().is_terminal() {
        bail!("pane attach needs a terminal on stdin");
    }
    let prefix = encode_key(KeyEvent::new(prefix.code, prefix.mods))
        .with_context(|| format!("the prefix {} has no key bytes", prefix.label()))?;
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
    let attach = Request::PaneAttach { pane, rows, cols };
    stream.write_all(&control(&attach))?;
    // A refusal — no such pane — lands before the terminal is touched; the
    // colours' `Ok` is passed over.
    let mut buf = Vec::new();
    let first = loop {
        let frame =
            read_frame(&mut stream, &mut buf)?.context("tutti-server closed the connection")?;
        let reply = match &frame {
            Frame::Control(json) => serde_json::from_slice(json).ok(),
            _ => None,
        };
        match reply {
            Some(Response::Ok) => continue,
            Some(Response::Error { message }) if message.starts_with("bad request") => {
                bail!(
                    "the daemon cannot stream a pane ({message}); restart it: `tutti server stop`"
                )
            }
            Some(Response::Error { message }) => bail!(message),
            _ => break frame,
        }
    };

    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let resizes = Signals::new([SIGWINCH]).context("watching for SIGWINCH")?;
    terminal::enable_raw_mode().context("entering raw mode")?;
    install_panic_hook();
    let detached = Arc::new(AtomicBool::new(false));
    spawn_keys(Keys::new(prefix), pane, &writer, &detached);
    spawn_resizes(resizes, pane, &writer);
    let shown = emit(ENTER).and_then(|()| show(first, &mut stream, &mut buf));
    restore();
    match shown? {
        Some(code) => println!("[pane {pane} exited: {code}]"),
        None if detached.load(Ordering::SeqCst) => println!("[detached from pane {pane}]"),
        None => bail!("tutti-server closed the connection"),
    }
    Ok(0)
}

/// Draw the stream, `first` frame on, until the pane exits (its code) or the
/// connection closes (`None`). A refusal on the way — the stream fell behind —
/// is the error.
fn show(first: Frame, stream: &mut UnixStream, buf: &mut Vec<u8>) -> Result<Option<i32>> {
    let mut mirror = Terminal::new(24, 80, 0);
    let mut last = 0;
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
                mirror = Terminal::new(data.rows, data.cols, 0);
                mirror.advance(&data.bytes);
                mirror.take_events();
                emit(&redraw(&mirror))?;
            }
            Frame::PaneRaw { bytes, .. } if feed(&mut mirror, &mut last, &bytes) => {
                emit(&redraw(&mirror))?;
            }
            Frame::PaneRaw { bytes, .. } => emit(&bytes)?,
            Frame::Control(json) => {
                if let Ok(Event::PaneExited { code, .. }) = serde_json::from_slice(&json) {
                    return Ok(Some(code));
                }
                if let Ok(Response::Error { message }) = serde_json::from_slice(&json) {
                    bail!(message);
                }
            }
            _ => {}
        }
    }
}

/// Feed one read of the pane's output to the mirror: true when it switched
/// screens or reset the terminal, which this terminal must not be handed raw.
/// A screen switch ends on `h` or `l`, so the mirror is checked after each —
/// one read can switch there and back. `last` is the byte before this read,
/// for an `ESC c` split across two.
fn feed(mirror: &mut Terminal, last: &mut u8, bytes: &[u8]) -> bool {
    let mut switched = false;
    for run in bytes.split_inclusive(|b| matches!(b, b'h' | b'l' | b'c')) {
        let alternate = mirror.modes().alt_screen;
        mirror.advance(run);
        let before = run.len().checked_sub(2).map_or(*last, |i| run[i]);
        switched |=
            alternate != mirror.modes().alt_screen || (before == 0x1b && run.ends_with(b"c"));
        *last = run[run.len() - 1];
    }
    mirror.take_events();
    switched
}

/// The mirror's screen, drawn whole on the alternate screen — taken again
/// first, in case a reset the stream slipped past left it, and cleared — with
/// its cursor and modes, but for what is this terminal's own: its keyboard
/// protocols, and the light/dark reports `ENTER` asked for.
fn redraw(mirror: &Terminal) -> Vec<u8> {
    let rows: Vec<u16> = (0..mirror.size().0).collect();
    let redraw = String::from_utf8(mirror.redraw(&rows)).expect("rtty-vt writes UTF-8");
    let mut out = String::from("\x1b[?1049h\x1b[0m\x1b[H\x1b[2J");
    let mut rest = redraw.as_str();
    while let Some(at) = rest.find("\x1b[") {
        out.push_str(&rest[..at]);
        let end = rest[at + 2..]
            .find(|c| ('\x40'..='\x7e').contains(&c))
            .map_or(rest.len(), |i| at + i + 3);
        let seq = &rest[at..end];
        let own = seq.starts_with("\x1b[=") && seq.ends_with('u')
            || seq.starts_with("\x1b[>4;") && seq.ends_with('m')
            || seq.starts_with("\x1b[?2031");
        if !own {
            out.push_str(seq);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out.into_bytes()
}

/// The keys typed into the pane, watched for the prefix chord's bytes — the
/// prefix then `d` detaches, the prefix twice sends one, and the prefix then
/// anything else sends both — and for the terminal's light/dark reports
/// (`REPORTS`), which are tutti's, never the pane's. Either still counts
/// split across reads — a report once its `ESC [ ?` has come, which no key
/// types: a lone `ESC` or `ESC [` ending a read is the key it looks like.
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
        if self.report.len() < 3 {
            for k in std::mem::take(&mut self.report) {
                if self.key(k, &mut out) {
                    return (out, true, dark);
                }
            }
        }
        (out, false, dark)
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

/// Type stdin into the pane on a thread of its own. A detach marks
/// `detached` and shuts the socket, which ends `show`.
fn spawn_keys(
    mut keys: Keys,
    pane: PaneId,
    writer: &Arc<Mutex<UnixStream>>,
    detached: &Arc<AtomicBool>,
) {
    let (writer, detached) = (Arc::clone(writer), Arc::clone(detached));
    std::thread::spawn(move || {
        let mut input = [0u8; 4096];
        loop {
            // Stdin is locked a read at a time: asking the colours locks it too.
            let n = match std::io::stdin().read(&mut input) {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            let (bytes, detach, dark) = keys.feed(&input[..n]);
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

/// Hand the terminal back as it was. Safe to run twice.
fn restore() {
    let _ = emit(LEAVE);
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
    /// reads cut them once their `ESC [ ?` has come; a near miss is keys, and
    /// so is a lone `ESC` or `ESC [` ending a read — an arrow cut there
    /// still arrives whole.
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
        // Cut after a report's `ESC` or `ESC [`, it reads as keys.
        for cut in (0..=input.len()).filter(|cut| ![2, 3, 12, 13].contains(cut)) {
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

    /// The default prefix is the byte `C-b` types.
    #[test]
    fn the_configured_chord_gives_the_prefix_bytes() {
        let bytes = |spec| {
            let chord = Chord::parse(spec).unwrap();
            encode_key(KeyEvent::new(chord.code, chord.mods)).unwrap()
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
                let switched = feed(mirror, &mut last, read);
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
        mirror.advance(b"\x1b[>5u\x1b[>4;2m\x1b[?2031h\x1b[?1049h\x1b[>1uvim");
        let (rows, cols) = mirror.size();
        let bytes = redraw(&mirror);
        let mut terminal = Terminal::new(rows, cols, 0);
        terminal.advance(b"\x1b[?2031h");
        terminal.advance(&bytes);
        assert_eq!(terminal.kitty_keyboard(), 0);
        assert_eq!(terminal.modes().modify_other_keys, 0);
        assert!(terminal.modes().color_scheme_reports);
        assert!(terminal.contents().starts_with("vim"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("\x1b[=") && !text.contains("\x1b[>4;") && !text.contains("2031"));
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
