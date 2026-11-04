//! The attach socket: a background thread decodes inbound frames onto a
//! channel while the event loop writes frames from the foreground. Keeping the
//! read side on its own thread lets the loop block on terminal input without
//! stalling pane updates.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use tutti_core::{Frame, Request, Tagged, socket_path};

use crate::client;

pub struct Connection {
    stream: UnixStream,
    rx: Receiver<Frame>,
    reader: Option<JoinHandle<()>>,
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
            view,
        })
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
    pub fn drain(&self) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
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
/// session.
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
                | Request::NotesRead { .. }
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
        ApprovalDecision, Direction, HostColors, PaneDims, PaneId, Rgb, TabId, WorkspaceId,
    };

    use super::*;
    use crate::attach::{control, tagged};

    /// A view moves its own focus, tab and scrollback, reads, and detaches —
    /// tagged or not — but types nothing, votes no size, tells no colours
    /// and changes nothing in the session.
    #[test]
    fn a_view_sends_only_what_navigates() {
        let pane = PaneId(1);
        for request in [
            Request::Attach {
                terminal: None,
                handoff: false,
            },
            Request::Detach,
            Request::PaneFocus { pane },
            Request::TabSelect { id: TabId(2) },
            Request::PaneScroll { pane, offset: 3 },
            Request::NotesRead {
                up_to: None,
                pane: None,
            },
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
