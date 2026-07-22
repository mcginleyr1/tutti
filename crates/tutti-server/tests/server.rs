//! Drive a real `tutti-server` through a real Unix socket: each test binds a
//! listener on a private temp path, serves it in-process, and talks the wire
//! protocol over `tokio::net::UnixStream`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use tutti_core::{
    AgentState, Direction, Event, Frame, Layout, PaneData, PaneId, Request, Response,
};
use tutti_server::{PaneSize, serve};

const DEADLINE: Duration = Duration::from_secs(5);

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestServer {
    path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tutti-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (shutdown, rx) = oneshot::channel::<()>();
        let served = path.clone();
        let handle = tokio::spawn(async move {
            serve(listener, served, PaneSize::new(24, 80), async move {
                let _ = rx.await;
            })
            .await
        });
        Self {
            path,
            shutdown: Some(shutdown),
            handle,
        }
    }

    async fn connect(&self) -> Conn {
        Conn {
            stream: UnixStream::connect(&self.path).await.unwrap(),
            buf: Vec::new(),
        }
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = timeout(DEADLINE, self.handle).await;
    }
}

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Conn {
    async fn write_frame(&mut self, frame: &Frame) {
        self.stream.write_all(&frame.encode()).await.unwrap();
    }

    async fn send(&mut self, request: &Request) {
        self.write_frame(&Frame::Control(serde_json::to_vec(request).unwrap()))
            .await;
    }

    async fn read_frame(&mut self) -> Frame {
        timeout(DEADLINE, async {
            loop {
                if let Some((frame, consumed)) = Frame::decode(&self.buf).unwrap() {
                    self.buf.drain(..consumed);
                    return frame;
                }
                let mut chunk = [0u8; 8192];
                let n = self.stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "server closed the connection unexpectedly");
                self.buf.extend_from_slice(&chunk[..n]);
            }
        })
        .await
        .expect("timed out waiting for a frame")
    }

    async fn response(&mut self) -> Response {
        loop {
            if let Frame::Control(json) = self.read_frame().await {
                return serde_json::from_slice(&json).unwrap();
            }
        }
    }

    async fn request(&mut self, request: Request) -> Response {
        self.send(&request).await;
        self.response().await
    }
}

fn workspace_id(response: Response) -> tutti_core::WorkspaceId {
    match response {
        Response::WorkspaceCreated { id } => id,
        other => panic!("expected WorkspaceCreated, got {other:?}"),
    }
}

fn pane_id(response: Response) -> PaneId {
    match response {
        Response::PaneCreated { id } => id,
        other => panic!("expected PaneCreated, got {other:?}"),
    }
}

async fn run_marker(conn: &mut Conn, cmd: &str) -> PaneId {
    workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        })
        .await,
    );
    pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        })
        .await,
    )
}

async fn read_until(conn: &mut Conn, pane: PaneId, needle: &str) -> bool {
    timeout(DEADLINE, async {
        loop {
            let response = conn
                .request(Request::PaneRead {
                    pane,
                    lines: None,
                    unwrapped: false,
                })
                .await;
            if let Response::Content { lines } = response
                && lines.iter().any(|l| l.contains(needle))
            {
                return true;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// A pane keeps running after the client that launched it disconnects, and a
/// fresh connection can read its output.
#[tokio::test]
async fn pane_survives_client_disconnect() {
    let server = TestServer::start().await;

    // launcher disconnects at the end of this block
    let pane = {
        let mut launcher = server.connect().await;
        run_marker(&mut launcher, "printf marker-42; sleep 30").await
    };

    let mut reader = server.connect().await;
    assert!(
        read_until(&mut reader, pane, "marker-42").await,
        "second connection could not read the persisted pane"
    );

    server.stop().await;
}

/// Bytes sent with `pane send` reach the child and echo back onto its grid.
#[tokio::test]
async fn pane_send_echoes_into_grid() {
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        })
        .await,
    );
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/cat".into()],
        })
        .await,
    );

    assert_eq!(
        conn.request(Request::PaneSend {
            pane,
            text: Some("hello-echo\n".into()),
            keys: None,
        })
        .await,
        Response::Ok
    );

    assert!(
        read_until(&mut conn, pane, "hello-echo").await,
        "cat did not echo the sent text"
    );

    server.stop().await;
}

/// `pane kill` drops one pane; `workspace kill` tears the rest down.
#[tokio::test]
async fn kill_removes_panes() {
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let workspace = workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        })
        .await,
    );
    let first = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .await,
    );
    let second = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .await,
    );

    assert_eq!(pane_ids(&mut conn).await, vec![first, second]);

    assert_eq!(
        conn.request(Request::PaneKill { pane: first }).await,
        Response::Ok
    );
    assert_eq!(pane_ids(&mut conn).await, vec![second]);

    assert_eq!(
        conn.request(Request::WorkspaceKill { id: workspace }).await,
        Response::Ok
    );
    assert_eq!(pane_ids(&mut conn).await, Vec::<PaneId>::new());

    server.stop().await;
}

async fn pane_ids(conn: &mut Conn) -> Vec<PaneId> {
    match conn.request(Request::PaneList).await {
        Response::Panes { panes } => panes.into_iter().map(|p| p.id).collect(),
        other => panic!("expected Panes, got {other:?}"),
    }
}

/// Attaching yields a snapshot per existing pane, and writing input that
/// changes the screen produces a delta on the shared tick.
#[tokio::test]
async fn attach_receives_snapshot_then_delta() {
    let server = TestServer::start().await;

    let mut control = server.connect().await;
    workspace_id(
        control
            .request(Request::WorkspaceNew {
                dir: std::env::temp_dir(),
            })
            .await,
    );
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/cat".into()],
            })
            .await,
    );

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    match viewer.response().await {
        Response::Attached { workspaces, .. } => {
            assert!(
                workspaces
                    .iter()
                    .flat_map(|w| &w.tabs)
                    .flat_map(|t| &t.panes)
                    .any(|p| p.id == pane),
                "attached view should list the running pane"
            );
        }
        other => panic!("expected Attached, got {other:?}"),
    }

    let snapshot = expect_pane_frame(&mut viewer, pane, true).await;
    assert_eq!(snapshot.pane, pane);

    viewer
        .write_frame(&Frame::Input {
            pane,
            bytes: b"xyz\r".to_vec(),
        })
        .await;

    let delta = expect_pane_frame(&mut viewer, pane, false).await;
    assert_eq!(delta.pane, pane);
    assert!(!delta.bytes.is_empty(), "delta carried no escape bytes");

    server.stop().await;
}

/// A `PaneResize` resizes the server-side pty and grid and pushes a fresh
/// snapshot at the new dimensions to the attached client.
#[tokio::test]
async fn attach_resize_reseeds_at_new_size() {
    let server = TestServer::start().await;

    let mut control = server.connect().await;
    workspace_id(
        control
            .request(Request::WorkspaceNew {
                dir: std::env::temp_dir(),
            })
            .await,
    );
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/cat".into()],
            })
            .await,
    );

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    let _ = viewer.response().await;
    let first = expect_pane_frame(&mut viewer, pane, true).await;
    assert_eq!((first.rows, first.cols), (24, 80));

    assert_eq!(
        control
            .request(Request::PaneResize {
                pane,
                rows: 30,
                cols: 100,
            })
            .await,
        Response::Ok
    );

    let resized = expect_pane_frame(&mut viewer, pane, true).await;
    assert_eq!(
        (resized.rows, resized.cols),
        (30, 100),
        "client should get a fresh snapshot at the new size"
    );

    server.stop().await;
}

/// The server writes `<session>.pid` beside the socket while running and
/// removes it on clean shutdown, so `tutti server stop` can SIGTERM it.
#[tokio::test]
async fn pidfile_tracks_server_lifetime() {
    let server = TestServer::start().await;
    let pid_path = server.path.with_extension("pid");

    let contents = timeout(DEADLINE, async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_path) {
                return contents;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pidfile written within deadline");
    assert_eq!(contents.trim().parse::<u32>().unwrap(), std::process::id());

    server.stop().await;
    assert!(!pid_path.exists(), "pidfile should be removed on shutdown");
}

/// A `PaneResizeSplit` nudges the ratio of the focused pane's enclosing split
/// and broadcasts the fresh view carrying the new ratio.
#[tokio::test]
async fn resize_split_adjusts_ratio_and_broadcasts() {
    let server = TestServer::start().await;

    let mut control = server.connect().await;
    workspace_id(
        control
            .request(Request::WorkspaceNew {
                dir: std::env::temp_dir(),
            })
            .await,
    );
    let first = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/cat".into()],
            })
            .await,
    );
    // Split beside it: a Horizontal split at ratio 0.5.
    pane_id(
        control
            .request(Request::PaneSplit {
                pane: first,
                direction: Direction::Horizontal,
            })
            .await,
    );

    // Attach after the split so the only LayoutChanged the viewer sees is ours.
    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));

    assert_eq!(
        control
            .request(Request::PaneResizeSplit {
                pane: first,
                direction: Direction::Horizontal,
                delta: 0.05,
            })
            .await,
        Response::Ok
    );

    let ratio = next_split_ratio(&mut viewer).await;
    assert!(
        (ratio - 0.55).abs() < 1e-3,
        "expected the split ratio nudged to ~0.55, got {ratio}"
    );

    server.stop().await;
}

/// Read events until a `LayoutChanged` carrying a split arrives, returning its
/// ratio.
async fn next_split_ratio(viewer: &mut Conn) -> f32 {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Ok(Event::LayoutChanged { workspaces }) =
                    serde_json::from_slice::<Event>(&json)
            {
                for w in &workspaces {
                    for t in &w.tabs {
                        if let Some(Layout::Split { ratio, .. }) = &t.layout {
                            return *ratio;
                        }
                    }
                }
            }
        }
    })
    .await
    .expect("a LayoutChanged carrying a split")
}

/// Read frames until a snapshot (`want_snapshot`) or delta for `pane` arrives.
async fn expect_pane_frame(conn: &mut Conn, pane: PaneId, want_snapshot: bool) -> PaneData {
    timeout(DEADLINE, async {
        loop {
            match conn.read_frame().await {
                Frame::PaneSnapshot(data) if want_snapshot && data.pane == pane => return data,
                Frame::PaneDelta(data) if !want_snapshot && data.pane == pane => return data,
                _ => continue,
            }
        }
    })
    .await
    .expect("timed out waiting for the expected pane frame")
}

/// Copy `src` to a fresh temp directory under the name `name` and make it
/// executable, so a benign binary can masquerade as an agent for detection.
fn copy_bin(name: &str, src: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tutti-bin-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join(name);
    std::fs::copy(src, &dst).unwrap();
    let mut perms = std::fs::metadata(&dst).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&dst, perms).unwrap();
    // Apple Silicon refuses to exec a copied system binary whose code signature
    // no longer matches; an ad-hoc re-sign restores it. A no-op elsewhere (the
    // command is absent on Linux, where unsigned copies run fine).
    let _ = std::process::Command::new("codesign")
        .args(["--sign", "-", "--force"])
        .arg(&dst)
        .output();
    dst
}

/// Poll `pane list` until `pane` reaches `want` or the deadline elapses.
async fn wait_state(conn: &mut Conn, pane: PaneId, want: AgentState) -> bool {
    timeout(DEADLINE, async {
        loop {
            if let Response::Panes { panes } = conn.request(Request::PaneList).await
                && panes.iter().any(|p| p.id == pane && p.state == want)
            {
                return true;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// A running binary whose name is in the registry is detected as that agent,
/// proving the live `sysinfo` process-tree walk (not just the unit matcher).
#[tokio::test]
async fn detects_agent_process_by_name() {
    let server = TestServer::start().await;
    let mut conn = server.connect().await;
    let bin = copy_bin("claude", "/bin/sleep");

    workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        })
        .await,
    );
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec![bin.display().to_string(), "30".into()],
        })
        .await,
    );

    let detected = timeout(DEADLINE, async {
        loop {
            if let Response::Panes { panes } = conn.request(Request::PaneList).await
                && panes.iter().any(|p| {
                    p.id == pane
                        && p.agent.as_ref().map(|a| a.to_string()).as_deref() == Some("claude")
                })
            {
                return true;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        detected,
        "a live process named claude was not detected as the claude agent"
    );

    server.stop().await;
}

/// Read state-change events for `pane` off an attached connection, recording
/// each `to` state, until `want` is seen.
async fn wait_state_event(
    viewer: &mut Conn,
    pane: PaneId,
    want: AgentState,
    order: &mut Vec<AgentState>,
) {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Ok(Event::StateChanged { pane: p, to, .. }) =
                    serde_json::from_slice::<Event>(&json)
                && p == pane
            {
                order.push(to);
                if to == want {
                    return;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("did not observe {want:?} within the deadline"));
}

/// A detected agent pane emits `StateChanged` events driven by its screen text:
/// a working marker moves it to `Working`, and a later blocked marker to
/// `Blocked`, in that order. The pane runs `cat` (renamed `claude`), so sent
/// text echoes onto the screen; the blocked marker is sent only after `Working`
/// is observed, fixing the order without depending on classifier timing.
#[tokio::test]
async fn agent_state_changes_working_then_blocked() {
    let server = TestServer::start().await;
    let mut control = server.connect().await;
    let bin = copy_bin("claude", "/bin/cat");

    workspace_id(
        control
            .request(Request::WorkspaceNew {
                dir: std::env::temp_dir(),
            })
            .await,
    );
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec![bin.display().to_string()],
            })
            .await,
    );

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;

    let mut order = Vec::new();

    assert_eq!(
        control
            .request(Request::PaneSend {
                pane,
                text: Some("esc to interrupt\n".into()),
                keys: None,
            })
            .await,
        Response::Ok
    );
    wait_state_event(&mut viewer, pane, AgentState::Working, &mut order).await;

    assert_eq!(
        control
            .request(Request::PaneSend {
                pane,
                text: Some("Do you want to continue?\n".into()),
                keys: None,
            })
            .await,
        Response::Ok
    );
    wait_state_event(&mut viewer, pane, AgentState::Blocked, &mut order).await;

    let working = order.iter().position(|s| *s == AgentState::Working);
    let blocked = order.iter().position(|s| *s == AgentState::Blocked);
    assert!(
        working.is_some() && working < blocked,
        "expected Working before Blocked, got {order:?}"
    );

    server.stop().await;
}

/// Focusing a `Done` pane marks it seen: its state becomes `Idle`.
#[tokio::test]
async fn focus_transitions_done_pane_to_idle() {
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        })
        .await,
    );
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        })
        .await,
    );

    assert!(
        wait_state(&mut conn, pane, AgentState::Done).await,
        "an exited pane should reach Done"
    );

    assert_eq!(
        conn.request(Request::PaneFocus { pane }).await,
        Response::Ok
    );
    assert!(
        wait_state(&mut conn, pane, AgentState::Idle).await,
        "focusing a Done pane should transition it to Idle"
    );

    server.stop().await;
}
