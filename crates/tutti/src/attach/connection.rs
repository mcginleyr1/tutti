//! The attach socket: a background thread decodes inbound frames onto a
//! channel while the event loop writes frames from the foreground. Keeping the
//! read side on its own thread lets the loop block on terminal input without
//! stalling pane updates.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tutti_core::{Frame, Request, Response, Tagged, socket_path};

use crate::client;

/// The wire rev from which a daemon knows a view (`Attach.view`).
const VIEW_REV: u32 = 16;

pub struct Connection {
    stream: UnixStream,
    rx: Receiver<Frame>,
    reader: Option<JoinHandle<()>>,
    /// Frames read off `rx` ahead of `drain`, which hands them on first.
    early: Vec<Frame>,
    /// A `tutti attach --view`: `send` passes on only what navigates.
    pub view: bool,
}

impl Connection {
    /// Connect to the session's daemon (auto-starting it, but for a `view`),
    /// spawning a reader thread that decodes inbound frames onto a channel.
    pub fn open(session: &str, view: bool) -> Result<Self> {
        let stream = if view {
            UnixStream::connect(socket_path(session)).with_context(|| {
                format!("no tutti-server answers for session '{session}': nothing to view")
            })?
        } else {
            client::open(session)?
        };
        Self::new(stream, view)
    }

    /// The connection over `stream`, with its reader thread.
    fn new(stream: UnixStream, view: bool) -> Result<Self> {
        let reader_stream = stream.try_clone().context("clone attach socket")?;
        let (tx, rx) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("tutti-attach-reader".into())
            .spawn(move || read_loop(reader_stream, &tx))
            .context("spawn attach reader thread")?;
        Ok(Self {
            stream,
            rx,
            reader: Some(reader),
            early: Vec::new(),
            view,
        })
    }

    /// A view's attach just sent, refused by the daemon's `Attached` reply
    /// when that is from before `VIEW_REV`: such a daemon takes a view for a
    /// client like any other, its focus marking agents seen and notes read
    /// for everybody — refused before the view moves it once.
    pub fn refuse_an_old_daemon(&mut self) -> Result<()> {
        let rev = self.wire_rev(Duration::from_secs(5))?;
        if rev < VIEW_REV {
            bail!(
                "the daemon predates `attach --view` (wire rev {rev}, a view needs {VIEW_REV}): \
                 it would take this view's focus for yours — restart it: `tutti server stop`"
            );
        }
        Ok(())
    }

    /// The daemon's wire rev, from its `Attached` reply to the attach just
    /// sent, waited for up to `wait`. Every frame read on the way, the reply
    /// included, stays for `drain`.
    fn wire_rev(&mut self, wait: Duration) -> Result<u32> {
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
            let rev = match &frame {
                Frame::Control(json) => match serde_json::from_slice(json) {
                    Ok(Tagged {
                        body: Response::Attached { wire_rev, .. },
                        ..
                    }) => Some(wire_rev),
                    _ => None,
                },
                _ => None,
            };
            self.early.push(frame);
            if let Some(rev) = rev {
                return Ok(rev);
            }
        }
    }

    /// Write `frame` to the daemon — from a view, only when it `navigates`:
    /// anything else is dropped here.
    pub fn send(&mut self, frame: &Frame) -> Result<()> {
        if self.view && !navigates(frame) {
            return Ok(());
        }
        self.stream
            .write_all(&frame.encode())
            .context("write attach frame")?;
        self.stream.flush().context("flush attach frame")
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
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
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
                | Request::PaneFocus { .. }
                | Request::TabSelect { .. }
                | Request::PaneScroll { .. }
                | Request::PaneRead { .. }
        )
    })
}

fn read_loop(mut stream: UnixStream, tx: &mpsc::Sender<Frame>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match Frame::decode(&buf) {
            Ok(Some((frame, consumed))) => {
                buf.drain(..consumed);
                if tx.send(frame).is_err() {
                    return;
                }
                continue;
            }
            Ok(None) => {}
            Err(_) => return,
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use tutti_core::{
        ApprovalDecision, Direction, Event, HostColors, PaneDims, PaneId, Rgb, TabId, WorkspaceId,
    };

    use super::*;
    use crate::attach::{control, tagged};

    /// A connection to a daemon played over a socket pair that answers an
    /// attach as wire rev `rev`, after an event in the gap: what
    /// `refuse_an_old_daemon` says, and what `drain` hands on after it.
    fn attached_at(rev: u32) -> (Result<()>, Vec<Frame>) {
        let (ours, mut daemon) = UnixStream::pair().unwrap();
        let mut conn = Connection::new(ours, true).unwrap();
        let json = |value: serde_json::Value| Frame::Control(value.to_string().into_bytes());
        let event = json(serde_json::to_value(Event::PaneOutput { pane: PaneId(1) }).unwrap());
        let attached = json(
            serde_json::to_value(Tagged {
                req: Some(0),
                body: Response::Attached {
                    session: "s".into(),
                    workspaces: Vec::new(),
                    wire_rev: rev,
                    restorable: None,
                    notes: Vec::new(),
                },
            })
            .unwrap(),
        );
        for frame in [&event, &attached] {
            daemon.write_all(&frame.encode()).unwrap();
        }
        let refused = conn.refuse_an_old_daemon();
        (refused, conn.drain().unwrap())
    }

    /// A view on a daemon from before views is refused, naming the fix; on
    /// one that knows views it goes on, and nothing read on the way to the
    /// daemon's answer is lost to the TUI.
    #[test]
    fn a_view_refuses_a_daemon_from_before_views() {
        let (refused, _) = attached_at(VIEW_REV - 1);
        let message = refused.unwrap_err().to_string();
        assert!(message.contains("tutti server stop"), "{message}");
        let (refused, drained) = attached_at(VIEW_REV);
        refused.unwrap();
        assert_eq!(drained.len(), 2, "the event and the reply, in order");
        assert!(
            matches!(&drained[0], Frame::Control(json) if json.windows(11).any(|w| w == b"pane_output"))
        );
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
            },
            Request::Detach,
            Request::PaneFocus { pane },
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
