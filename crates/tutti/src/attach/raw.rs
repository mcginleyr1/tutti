//! `tutti pane attach`: one pane in the terminal you run it in, parsed by
//! that terminal's own emulator. The daemon streams the pane raw
//! (`Request::PaneAttach`) — a snapshot, then every pty read — and keys go
//! back as typed, but for the prefix chord: the prefix then `d` detaches, the
//! prefix twice sends one.
//!
//! The pane is drawn on the alternate screen, left on exit, so the shell's
//! screen and scrollback come back as they were. A pane program's own
//! alternate screen therefore cannot be this terminal's: a vt100 mirror of
//! the pane follows the stream, and a read that switches screens, or resets
//! the terminal (`ESC c`), is drawn from the mirror instead of passed through.
//! A program leaving its alternate screen so brings back the pane's primary
//! screen, not the shell's.

use std::io::{IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::terminal;
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use tutti_core::{Event, Frame, PaneDims, PaneId, Request, Response, socket_path};

use super::input::encode_key;
use crate::client;
use crate::config::Chord;

/// Push the window title and take the alternate screen.
const ENTER: &[u8] = b"\x1b[22;0t\x1b[?1049h";
/// Turn off what the pane may have turned on in this terminal — mouse
/// reporting, focus events, bracketed paste, application cursor and keypad, a
/// cursor style, a hidden cursor, colours — then leave the alternate screen
/// and pop the title.
const LEAVE: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\
\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[0 q\x1b[?25h\x1b[m\x1b[?1049l\x1b[23;0t";

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
    let (cols, rows) = terminal::size().context("reading the terminal size")?;
    let attach = Request::PaneAttach { pane, rows, cols };
    stream.write_all(&control(&attach))?;
    // A refusal — no such pane — lands before the terminal is touched.
    let mut buf = Vec::new();
    let first = read_frame(&mut stream, &mut buf)?.context("tutti-server closed the connection")?;
    if let Frame::Control(json) = &first
        && let Ok(Response::Error { message }) = serde_json::from_slice(json)
    {
        if message.starts_with("bad request") {
            bail!("the daemon cannot stream a pane ({message}); restart it: `tutti server stop`");
        }
        bail!(message);
    }

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
    let mut mirror = vt100::Parser::default();
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
                mirror = vt100::Parser::new(data.rows, data.cols, 0);
                mirror.process(&data.bytes);
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
fn feed(mirror: &mut vt100::Parser, last: &mut u8, bytes: &[u8]) -> bool {
    let mut switched = false;
    for run in bytes.split_inclusive(|b| matches!(b, b'h' | b'l' | b'c')) {
        let alternate = mirror.screen().alternate_screen();
        mirror.process(run);
        let before = run.len().checked_sub(2).map_or(*last, |i| run[i]);
        switched |= alternate != mirror.screen().alternate_screen()
            || (before == 0x1b && run.ends_with(b"c"));
        *last = run[run.len() - 1];
    }
    switched
}

/// The mirror's screen, drawn whole on the alternate screen — taken again
/// first, in case a reset the stream slipped past left it.
fn redraw(mirror: &vt100::Parser) -> Vec<u8> {
    [
        b"\x1b[?1049h".as_slice(),
        &mirror.screen().state_formatted(),
    ]
    .concat()
}

/// The keys typed into the pane, watched for the prefix chord's bytes: the
/// prefix then `d` detaches, the prefix twice sends one, and the prefix then
/// anything else sends both. A prefix split across reads still counts.
struct Keys {
    prefix: Vec<u8>,
    /// How many of the prefix's bytes the input has matched so far.
    held: usize,
    /// A whole prefix came: the next key decides.
    armed: bool,
}

impl Keys {
    fn new(prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            held: 0,
            armed: false,
        }
    }

    /// The bytes to send for one read of input, and whether it detached;
    /// what follows a detach is dropped.
    fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        for &b in input {
            if self.armed && self.held == 0 && b == b'd' {
                return (out, true);
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
                continue;
            }
            if std::mem::take(&mut self.armed) {
                out.extend_from_slice(&self.prefix);
            }
            out.extend_from_slice(&self.prefix[..self.held]);
            self.held = usize::from(self.prefix[0] == b);
            if self.held == 0 {
                out.push(b);
            }
        }
        (out, false)
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
        let mut stdin = std::io::stdin().lock();
        let mut input = [0u8; 4096];
        loop {
            let n = match stdin.read(&mut input) {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            let (bytes, detach) = keys.feed(&input[..n]);
            let mut stream = writer.lock().unwrap_or_else(|e| e.into_inner());
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
            let (bytes, detach) = keys.feed(read);
            sent.extend(bytes);
            if detach {
                return (sent, true);
            }
        }
        (sent, false)
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
    fn shown(mirror: &mut vt100::Parser, reads: &[&[u8]]) -> (vt100::Parser, Vec<bool>) {
        let (rows, cols) = mirror.screen().size();
        let mut terminal = vt100::Parser::new(rows, cols, 0);
        terminal.process(&redraw(mirror));
        let mut last = 0;
        let switched = reads
            .iter()
            .map(|read| {
                let switched = feed(mirror, &mut last, read);
                terminal.process(&if switched {
                    redraw(mirror)
                } else {
                    read.to_vec()
                });
                switched
            })
            .collect();
        (terminal, switched)
    }

    /// A pane program's trip to its alternate screen never reaches this
    /// terminal's: each switch is drawn from the mirror, so leaving brings
    /// back the pane's own screen, and the terminal stays on its alternate.
    #[test]
    fn a_pane_programs_alternate_screen_is_drawn_from_the_mirror() {
        let mut mirror = vt100::Parser::new(6, 20, 0);
        mirror.process(b"$ ls\r\nfile\r\n$ ");
        let (terminal, switched) = shown(
            &mut mirror,
            &[
                b"vim\r\n\x1b[?1049h\x1b[H\x1b[2Jediting",
                b"\x1b[31mmore\x1b[m",
                b"\x1b[?1049l$ done\r\n$ ",
            ],
        );
        assert_eq!(switched, [true, false, true]);
        assert!(
            terminal
                .screen()
                .contents()
                .starts_with("$ ls\nfile\n$ vim\n$ done")
        );
        assert_eq!(
            terminal.screen().contents_formatted(),
            mirror.screen().contents_formatted(),
            "the pane's primary screen came back, cursor and all"
        );
        assert!(
            terminal.screen().alternate_screen() && !mirror.screen().alternate_screen(),
            "this terminal stays on its alternate screen"
        );
    }

    /// A read that goes there and back, a switch cut across two reads, and a
    /// reset — whole or cut after its `ESC` — all redraw; plain text and an
    /// `ESC` before other letters pass through.
    #[test]
    fn switches_and_resets_redraw_wherever_they_fall() {
        let mut mirror = vt100::Parser::new(4, 20, 0);
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
        assert_eq!(terminal.screen().contents(), "boldchl");
        assert!(!mirror.screen().alternate_screen());
    }
}
