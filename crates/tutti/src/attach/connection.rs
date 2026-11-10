//! The attach connection: a background thread decodes inbound frames onto a
//! channel while the event loop writes frames from the foreground. Keeping the
//! read side on its own thread lets the loop block on terminal input without
//! stalling pane updates. The far end is the daemon's socket, or — for
//! `tutti --host` — an `ssh … tutti proxy` child carrying the same frames,
//! whose host replies the reader hands to the calls waiting on them.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tutti_core::{Frame, Request, Tagged};

use crate::client;
use crate::host::{self, HOST_REV, Remote, Startup, Waiting};

/// The most of ssh's stderr kept to explain a connection that ended.
const STDERR_TAIL: usize = 4096;

pub struct Connection {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    rx: Receiver<Frame>,
    reader: Option<JoinHandle<()>>,
    waiting: Waiting,
    end: End,
    /// Frames read off `rx` ahead of `drain`, which hands them on first.
    early: Vec<Frame>,
    /// A `tutti attach --view`: `send` passes on only what navigates.
    pub view: bool,
}

enum End {
    Socket(UnixStream),
    Ssh {
        child: Child,
        destination: String,
        stderr: Arc<Mutex<Vec<u8>>>,
    },
}

impl Connection {
    /// Connect to the session's daemon (auto-starting it, but for a `view`),
    /// spawning a reader thread that decodes inbound frames onto a channel.
    pub fn open(session: &str, view: bool) -> Result<Self> {
        Self::new(client::open_attach(session, view)?, view)
    }

    /// The connection over the daemon's socket `stream`, with its reader
    /// thread.
    fn new(stream: UnixStream, view: bool) -> Result<Self> {
        let reader = stream.try_clone().context("clone attach socket")?;
        let writer = stream.try_clone().context("clone attach socket")?;
        let (conn, _) = Self::spawn(
            Box::new(reader),
            Box::new(writer),
            End::Socket(stream),
            view,
        )?;
        Ok(conn)
    }

    /// Reach the daemon of `session` (the far side's default when `None`) on
    /// `destination` through `ssh destination tutti proxy` — which, for a
    /// `view`, starts no daemon there — and wait for the proxy's hello: until
    /// then the terminal is still the user's, for ssh to ask its questions on.
    /// A connection that ends first is an error saying why — never a fall
    /// back to a local daemon.
    pub fn open_remote(
        destination: &str,
        session: Option<&str>,
        view: bool,
    ) -> Result<(Self, Startup)> {
        let mut argv = vec!["tutti".to_string()];
        if let Some(session) = session {
            argv.extend(["-s".into(), session.into()]);
        }
        argv.push("proxy".into());
        if view {
            argv.push("--view".into());
        }
        let mut child = client::ssh(destination, &argv, false)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running ssh")?;
        let input = child.stdout.take().expect("piped stdout");
        let output = child.stdin.take().expect("piped stdin");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let mut said = child.stderr.take().expect("piped stderr");
        let keep = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n @ 1..) = said.read(&mut chunk) {
                let mut kept = keep.lock().unwrap_or_else(PoisonError::into_inner);
                kept.extend_from_slice(&chunk[..n]);
                let over = kept.len().saturating_sub(STDERR_TAIL);
                kept.drain(..over);
            }
        });
        let end = End::Ssh {
            child,
            destination: destination.into(),
            stderr,
        };
        let (mut conn, hello) = Self::spawn(Box::new(input), Box::new(output), end, view)?;
        match hello.recv() {
            Ok((rev, startup)) if rev == HOST_REV => Ok((conn, startup)),
            Ok((rev, _)) => bail!(
                "the tutti on {destination} speaks host revision {rev}, this one {HOST_REV}: install the same build on both"
            ),
            Err(_) => Err(conn
                .ended()
                .unwrap_or_else(|| anyhow!("ssh {destination} closed"))),
        }
    }

    fn spawn(
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
        end: End,
        view: bool,
    ) -> Result<(Self, Receiver<(u32, Startup)>)> {
        let (tx, rx) = mpsc::channel();
        let (hello_tx, hello_rx) = mpsc::channel();
        let waiting = Waiting::default();
        let routes = Arc::clone(&waiting);
        let reader = thread::Builder::new()
            .name("tutti-attach-reader".into())
            .spawn(move || read_loop(input, &tx, &routes, &hello_tx))
            .context("spawn attach reader thread")?;
        let conn = Self {
            writer: Arc::new(Mutex::new(output)),
            rx,
            reader: Some(reader),
            waiting,
            end,
            early: Vec::new(),
            view,
        };
        Ok((conn, hello_rx))
    }

    /// Questions about the far end's machine, answered by its proxy.
    pub fn remote(&self) -> Remote {
        Remote::new(Arc::clone(&self.writer), Arc::clone(&self.waiting))
    }

    /// The attach just sent, refused when the daemon's answer puts the two
    /// ends past each other's floors (`Wire::accepts`) — before the TUI takes
    /// the terminal or sends anything else: a daemon from before views would
    /// take a view's focus for everybody's. The answer is waited for up to
    /// `wait`; every frame read on the way, the answer included, stays for
    /// `drain`.
    pub fn refuse_a_daemon_past_the_floor(&mut self, wait: Duration) -> Result<()> {
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let frame = match self.rx.recv_timeout(left) {
                Ok(frame) => frame,
                Err(RecvTimeoutError::Timeout) => {
                    bail!("tutti-server did not answer the attach within {wait:?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("tutti-server closed the attach connection")
                }
            };
            let wire = match &frame {
                Frame::Control(json) => tutti_core::attached_wire(json),
                _ => None,
            };
            self.early.push(frame);
            if let Some((rev, floor)) = wire {
                return tutti_core::WIRE
                    .accepts("tutti", rev, floor)
                    .map_err(|refusal| anyhow!(refusal));
            }
        }
    }

    /// Write `frame` to the daemon — from a view, only when it `navigates`:
    /// anything else is dropped here.
    pub fn send(&mut self, frame: &Frame) -> Result<()> {
        if self.view && !navigates(frame) {
            return Ok(());
        }
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        writer
            .write_all(&frame.encode())
            .context("write attach frame")?;
        writer.flush().context("flush attach frame")
    }

    /// Every frame the reader has decoded since the last call. `Err` means the
    /// server closed the connection and no frames remain.
    pub fn drain(&mut self) -> Result<Vec<Frame>> {
        let mut out = std::mem::take(&mut self.early);
        loop {
            match self.rx.try_recv() {
                Ok(frame) => out.push(frame),
                Err(TryRecvError::Empty) => return Ok(out),
                Err(TryRecvError::Disconnected) => {
                    if out.is_empty() {
                        bail!("tutti-server closed the attach connection");
                    }
                    // Deliver the final frames; the next call reports the close.
                    return Ok(out);
                }
            }
        }
    }

    /// Why an ssh connection ended, once it has: ssh's exit when it failed,
    /// and what it — or the tutti it ran — printed. `None` for a clean end
    /// (the far daemon stopped) and for a local socket.
    pub fn ended(&mut self) -> Option<anyhow::Error> {
        let End::Ssh {
            child,
            destination,
            stderr,
        } = &mut self.end
        else {
            return None;
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                _ => break None,
            }
        };
        let said = String::from_utf8_lossy(&stderr.lock().unwrap_or_else(PoisonError::into_inner))
            .trim()
            .to_string();
        if status.is_some_and(|s| s.success()) && said.is_empty() {
            return None;
        }
        let how = status.map_or_else(|| "did not exit".into(), |s| s.to_string());
        Some(anyhow!(
            format!("ssh {destination}: {how}\n{said}")
                .trim_end()
                .to_string()
        ))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        match &mut self.end {
            End::Socket(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            End::Ssh { child, .. } => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// What a view may send: attaching and detaching, moves of its focus, tab
/// and scrollback, and reads. Never input, a size vote, its terminal's
/// colours — the panes' programs would hear of them — or any change to the
/// session, marking notes read included: the history is the user's.
fn navigates(frame: &Frame) -> bool {
    let Frame::Control(json) = frame else {
        return false;
    };
    serde_json::from_slice::<Tagged<Request>>(json).is_ok_and(|tagged| {
        matches!(
            tagged.body,
            Request::Attach { .. }
                | Request::Detach
                | Request::PaneFocus { seen: false, .. }
                | Request::TabSelect { .. }
                | Request::PaneScroll { .. }
                | Request::PaneRead { .. }
        )
    })
}

fn read_loop(
    mut input: Box<dyn Read + Send>,
    tx: &mpsc::Sender<Frame>,
    waiting: &Waiting,
    hello: &mpsc::Sender<(u32, Startup)>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match Frame::decode(&buf) {
            Ok(Some((frame, consumed))) => {
                buf.drain(..consumed);
                match frame {
                    Frame::Host(json) => host::route(&json, waiting, hello),
                    frame => {
                        if tx.send(frame).is_err() {
                            break;
                        }
                    }
                }
                continue;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        match input.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    // Nothing will answer the calls still out: fail them now, not at their
    // timeouts.
    waiting
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

#[cfg(test)]
mod tests {
    use tutti_core::{
        ApprovalDecision, Direction, Event, HostColors, PaneDims, PaneId, Response, Rgb, TabId,
        WorkspaceId,
    };

    use super::*;
    use crate::attach::{control, tagged};

    /// A connection to a daemon played over a socket pair that answers the
    /// attach with `answer`, after an event in the gap: what
    /// `refuse_a_daemon_past_the_floor` says, and what `drain` hands on after it.
    fn answered(answer: Vec<u8>) -> (Result<()>, Vec<Frame>) {
        let (ours, mut daemon) = UnixStream::pair().unwrap();
        let mut conn = Connection::new(ours, false).unwrap();
        let event = serde_json::to_vec(&Event::PaneOutput { pane: PaneId(1) }).unwrap();
        for json in [event, answer] {
            daemon.write_all(&Frame::Control(json).encode()).unwrap();
        }
        let refused = conn.refuse_a_daemon_past_the_floor(Duration::from_secs(5));
        (refused, conn.drain().unwrap())
    }

    fn attached(wire_rev: u32) -> Vec<u8> {
        serde_json::to_vec(&Tagged {
            req: Some(0),
            body: Response::Attached {
                session: "s".into(),
                workspaces: Vec::new(),
                wire_rev,
                wire_floor: None,
                restorable: None,
                notes: Vec::new(),
            },
        })
        .unwrap()
    }

    /// A daemon below this build's floor is refused, naming the fix; one
    /// at the floor is taken, and nothing read on the way to its answer is
    /// lost to the TUI.
    #[test]
    fn the_attach_refuses_a_daemon_below_the_floor() {
        let (refused, _) = answered(attached(tutti_core::WIRE_FLOOR - 1));
        let message = refused.unwrap_err().to_string();
        assert!(message.contains("tutti server stop"), "{message}");
        let (refused, drained) = answered(attached(tutti_core::WIRE_FLOOR));
        refused.unwrap();
        assert_eq!(drained.len(), 2, "the event and the reply, in order");
        assert!(
            matches!(&drained[0], Frame::Control(json) if json.windows(11).any(|w| w == b"pane_output"))
        );
    }

    /// A daemon too new for this build says so in an answer whose rest no
    /// longer reads here: its revision and floor are read on their own, so
    /// the refusal still names the end to update.
    #[test]
    fn an_answer_too_new_to_read_whole_still_names_the_end_to_update() {
        let newer = tutti_core::WIRE_REV + 1;
        let answer = format!(
            r#"{{"req":0,"type":"attached","wire_rev":{newer},"wire_floor":{newer},"workspaces":7}}"#
        );
        assert!(serde_json::from_str::<Tagged<Response>>(&answer).is_err());
        let (refused, _) = answered(answer.into_bytes());
        let message = refused.unwrap_err().to_string();
        assert!(
            message.contains(&format!("wire revision {newer} (floor {newer})")),
            "{message}"
        );
        assert!(message.ends_with("update tutti"), "{message}");
    }

    /// A view moves its own focus, tab and scrollback, reads, and detaches —
    /// tagged or not — but types nothing, votes no size, tells no colours
    /// and changes nothing in the session, not even which notes are read.
    #[test]
    fn a_view_sends_only_what_navigates() {
        let pane = PaneId(1);
        for request in [
            Request::Attach {
                terminal: None,
                handoff: false,
                view: false,
                quiet: false,
            },
            Request::Detach,
            Request::PaneFocus { pane, seen: false },
            Request::TabSelect { id: TabId(2) },
            Request::PaneScroll { pane, offset: 3 },
        ] {
            assert!(navigates(&control(&request)), "{request:?}");
            assert!(navigates(&tagged(&request, 7)), "tagged {request:?}");
        }
        let black = Rgb { r: 0, g: 0, b: 0 };
        for request in [
            Request::ViewSizes {
                panes: vec![PaneDims {
                    pane,
                    rows: 10,
                    cols: 40,
                }],
            },
            Request::PaneResize {
                pane,
                rows: 10,
                cols: 40,
            },
            Request::HostColors(HostColors {
                fg: black,
                bg: black,
                cursor: None,
                dark: true,
            }),
            Request::PaneSplit {
                pane,
                direction: Direction::Vertical,
            },
            Request::PaneKill { pane },
            Request::ApprovalAnswer {
                pane,
                id: None,
                decision: ApprovalDecision::Allow,
            },
            Request::WorkspaceKill {
                id: WorkspaceId(1),
                discard: false,
            },
            // A view's focus says nobody is looking.
            Request::PaneFocus { pane, seen: true },
            Request::NotesRead {
                up_to: None,
                pane: None,
            },
            Request::NotesClear,
        ] {
            assert!(!navigates(&control(&request)), "{request:?}");
        }
        let input = Frame::Input {
            pane,
            bytes: b"ls\r".to_vec(),
        };
        assert!(!navigates(&input));
    }
}
