//! Drive the attach client's `App` against a real `tutti-server`. A daemon runs
//! in-process on a tempdir socket; the test talks the wire protocol over a
//! blocking `UnixStream` and feeds every inbound frame into `App`, asserting the
//! handshake view seeds the client and that typed input round-trips into the
//! client-side grid.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::net::UnixListener;
use tokio::sync::oneshot;

use tutti::attach::App;
use tutti_core::{AgentState, Frame, PaneId, Request, Response, StateSource};
use tutti_server::{PaneSize, serve};

static COUNTER: AtomicU64 = AtomicU64::new(0);
const DEADLINE: Duration = Duration::from_secs(5);

/// A daemon serving on a private socket, on its own tokio runtime thread.
struct Daemon {
    path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tutti-attach-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let served = path.clone();
        let (shutdown, rx) = oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = UnixListener::bind(&served).unwrap();
                serve(
                    listener,
                    served.clone(),
                    PaneSize::new(24, 80),
                    None,
                    async move {
                        let _ = rx.await;
                    },
                )
                .await
                .unwrap();
            });
        });
        Self {
            path,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }

    fn connect(&self) -> Wire {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Ok(stream) = UnixStream::connect(&self.path) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                return Wire {
                    stream,
                    buf: Vec::new(),
                };
            }
            assert!(Instant::now() < deadline, "server never came up");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Wire {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Wire {
    fn send(&mut self, request: &Request) {
        self.write(&Frame::Control(serde_json::to_vec(request).unwrap()));
    }

    fn write(&mut self, frame: &Frame) {
        self.stream.write_all(&frame.encode()).unwrap();
        self.stream.flush().unwrap();
    }

    /// Read the next whole frame, blocking up to the deadline.
    fn read_frame(&mut self) -> Frame {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some((frame, consumed)) = Frame::decode(&self.buf).unwrap() {
                self.buf.drain(..consumed);
                return frame;
            }
            assert!(Instant::now() < deadline, "timed out waiting for a frame");
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("server closed the connection"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("read error: {e}"),
            }
        }
    }

    /// The next Control frame decoded as a `Response`, skipping pane frames.
    fn response(&mut self) -> Response {
        loop {
            if let Frame::Control(json) = self.read_frame() {
                return serde_json::from_slice(&json).unwrap();
            }
        }
    }
}

#[test]
fn attach_seeds_app_and_input_updates_grid() {
    let daemon = Daemon::start();

    // Control connection: create a workspace and a cat pane that echoes input.
    let mut control = daemon.connect();
    control.send(&Request::WorkspaceNew {
        dir: std::env::temp_dir(),
    });
    assert!(matches!(
        control.response(),
        Response::WorkspaceCreated { .. }
    ));
    control.send(&Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/cat".into()],
        ephemeral: false,
        sandbox: None,
    });
    let pane = match control.response() {
        Response::PaneCreated { id } => id,
        other => panic!("expected PaneCreated, got {other:?}"),
    };

    // Attach a viewer and drive its App from the wire.
    let mut viewer = daemon.connect();
    viewer.send(&Request::Attach { terminal: None });

    let mut app = App::new();
    match viewer.response() {
        response @ Response::Attached { .. } => {
            app.handle_frame(Frame::Control(serde_json::to_vec(&response).unwrap()));
        }
        other => panic!("expected Attached, got {other:?}"),
    }
    assert_eq!(app.focused, Some(pane), "attach should focus the pane");
    assert!(
        app.panes.contains_key(&pane),
        "attach view should list the pane"
    );

    // The snapshot on the tick seeds the client parser.
    seed_from_snapshot(&mut viewer, &mut app, pane);

    // Type into the focused pane; cat echoes it back as a delta.
    for frame in app.on_key(key('h')) {
        viewer.write(&frame);
    }
    for frame in app.on_key(key('i')) {
        viewer.write(&frame);
    }

    // Pump frames until the client-side grid shows the echoed text.
    let deadline = Instant::now() + DEADLINE;
    loop {
        app.handle_frame(viewer.read_frame());
        if app.pane_text(pane).is_some_and(|t| t.contains("hi")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "grid never showed the typed input"
        );
    }
}

/// A scripted `pane focus` — a notch app's click — jumps the attached client to
/// that pane, switching its workspace and tab, and the client reports the new
/// focus back as its own.
#[test]
fn a_scripted_pane_focus_jumps_the_attached_client() {
    let daemon = Daemon::start();

    let mut control = daemon.connect();
    let mut cat_in_new_workspace = || {
        control.send(&Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        });
        assert!(matches!(
            control.response(),
            Response::WorkspaceCreated { .. }
        ));
        control.send(&Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/cat".into()],
            ephemeral: false,
            sandbox: None,
        });
        match control.response() {
            Response::PaneCreated { id } => id,
            other => panic!("expected PaneCreated, got {other:?}"),
        }
    };
    let first = cat_in_new_workspace();
    let second = cat_in_new_workspace();

    let mut viewer = daemon.connect();
    viewer.send(&Request::Attach { terminal: None });
    let mut app = App::new();
    let response = viewer.response();
    app.handle_frame(Frame::Control(serde_json::to_vec(&response).unwrap()));
    assert_eq!(app.focused, Some(second), "attach lands on the newest pane");
    let home = app.active_tab;
    if let Some(frame) = app.focus_change() {
        viewer.write(&frame);
    }

    control.send(&Request::PaneFocus { pane: first });
    assert_eq!(control.response(), Response::Ok);

    let deadline = Instant::now() + DEADLINE;
    while app.focused != Some(first) {
        app.handle_frame(viewer.read_frame());
        assert!(Instant::now() < deadline, "the client never jumped");
    }
    assert_ne!(app.active_tab, home, "the jump switched workspace and tab");
    assert_eq!(
        app.focus_change(),
        Some(Frame::Control(
            serde_json::to_vec(&Request::PaneFocus { pane: first }).unwrap()
        )),
        "the client reports the jumped-to pane as its focus"
    );
}

/// The `--tty` hooks' commands report over OSC 7501 through the pane's own
/// pty — as from a sandbox, whose pty `kubectl exec -it` joins to the pane —
/// each moving the pane as its event should, under the `claude` label, with no
/// tutti binary involved.
#[test]
fn tty_hook_commands_report_through_the_panes_pty() {
    let daemon = Daemon::start();
    let mut control = daemon.connect();
    control.send(&Request::WorkspaceNew {
        dir: std::env::temp_dir(),
    });
    assert!(matches!(
        control.response(),
        Response::WorkspaceCreated { .. }
    ));
    // Each hook waits for a line first, so the test moves them along in turn.
    let hooks = tutti::hooks::claude_tty_hooks_json();
    let steps: Vec<String> = ["UserPromptSubmit", "Notification", "Stop", "SessionEnd"]
        .iter()
        .map(|event| {
            let command = hooks["hooks"][event][0]["hooks"][0]["command"].as_str();
            format!("read _; {}", command.unwrap())
        })
        .collect();
    control.send(&Request::PaneRun {
        tab: None,
        cmd: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("{}; sleep 30", steps.join("; ")),
        ],
        ephemeral: false,
        sandbox: None,
    });
    let pane = match control.response() {
        Response::PaneCreated { id } => id,
        other => panic!("expected PaneCreated, got {other:?}"),
    };
    for expected in [
        (AgentState::Working, Some(StateSource::Program)),
        (AgentState::Blocked, Some(StateSource::Program)),
        (AgentState::Done, Some(StateSource::Program)),
        (AgentState::Done, None),
    ] {
        control.send(&Request::PaneSend {
            pane,
            text: Some("\n".into()),
            keys: None,
            expect: None,
        });
        assert_eq!(control.response(), Response::Ok);
        let deadline = Instant::now() + DEADLINE;
        loop {
            control.send(&Request::PaneList);
            let Response::Panes { panes } = control.response() else {
                panic!("expected Panes");
            };
            let info = panes.into_iter().find(|p| p.id == pane).unwrap();
            if (info.state, info.source) == expected {
                assert_eq!(info.agent, Some("claude".into()), "the report's app");
                break;
            }
            assert!(Instant::now() < deadline, "never {expected:?}: {info:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn seed_from_snapshot(viewer: &mut Wire, app: &mut App, pane: PaneId) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let frame = viewer.read_frame();
        let is_snapshot = matches!(&frame, Frame::PaneSnapshot(d) if d.pane == pane);
        app.handle_frame(frame);
        if is_snapshot {
            return;
        }
        assert!(Instant::now() < deadline, "never received a snapshot");
    }
}

fn key(c: char) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char(c),
        ratatui::crossterm::event::KeyModifiers::NONE,
    )
}
