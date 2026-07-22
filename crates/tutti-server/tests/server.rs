//! Drive a real `tutti-server` through a real Unix socket: each test binds a
//! listener on a private temp path, serves it in-process, and talks the wire
//! protocol over `tokio::net::UnixStream`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use tutti_core::{
    AgentState, Direction, Event, Frame, Layout, PaneData, PaneId, Request, Response, WorkspaceId,
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
    fn start() -> Self {
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

/// Create a workspace rooted at the temp dir and return its id.
async fn new_workspace(conn: &mut Conn) -> tutti_core::WorkspaceId {
    workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        })
        .await,
    )
}

fn pane_id(response: Response) -> PaneId {
    match response {
        Response::PaneCreated { id } => id,
        other => panic!("expected PaneCreated, got {other:?}"),
    }
}

async fn run_marker(conn: &mut Conn, cmd: &str) -> PaneId {
    new_workspace(conn).await;
    pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
            ephemeral: false,
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
    let server = TestServer::start();

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
    let server = TestServer::start();
    let mut conn = server.connect().await;

    new_workspace(&mut conn).await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/cat".into()],
            ephemeral: false,
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
    let server = TestServer::start();
    let mut conn = server.connect().await;

    let workspace = new_workspace(&mut conn).await;
    let first = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            ephemeral: false,
        })
        .await,
    );
    let second = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            ephemeral: false,
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
        conn.request(Request::WorkspaceKill {
            id: workspace,
            discard: false,
        })
        .await,
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
    let server = TestServer::start();

    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/cat".into()],
                ephemeral: false,
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
    let server = TestServer::start();

    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/cat".into()],
                ephemeral: false,
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
    let server = TestServer::start();
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
    let server = TestServer::start();

    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let first = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/cat".into()],
                ephemeral: false,
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
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for the expected pane frame")
}

/// A client that attaches and then stops reading has its outbound queue bounded:
/// once it backs up the server disconnects it (its socket reaches EOF), while a
/// second, well-behaved client attached at the same time keeps receiving frames
/// and can still complete a request round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wedged_client_is_dropped_but_others_keep_flowing() {
    let server = TestServer::start();

    let mut control = server.connect().await;
    new_workspace(&mut control).await;

    // Attaches, then never reads again: its bounded queue will fill.
    let mut wedged = server.connect().await;
    wedged.send(&Request::Attach).await;

    // Attaches and will keep draining.
    let mut good = server.connect().await;
    good.send(&Request::Attach).await;
    assert!(matches!(good.response().await, Response::Attached { .. }));

    // Flood a pane with bells; each becomes a broadcast frame, so the
    // non-reading client's queue overflows within a second or two. Emitted in
    // throttled bursts rather than a tight loop, so the flood stays a good CPU
    // citizen next to the well-behaved client (and to concurrent tests).
    pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "while :; do i=0; while [ $i -lt 40 ]; do printf '\\007'; \
                     i=$((i+1)); done; sleep 0.02; done"
                        .into(),
                ],
                ephemeral: false,
            })
            .await,
    );

    // Drain `good` continuously; after a grace period, prove it can still round
    // trip a request while the flood is in flight.
    let good_handle = tokio::spawn(async move {
        let mut good = good;
        let mut sent = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        loop {
            if !sent && tokio::time::Instant::now() >= deadline {
                good.send(&Request::PaneList).await;
                sent = true;
            }
            let frame = good.read_frame().await;
            if sent
                && let Frame::Control(json) = &frame
                && matches!(
                    serde_json::from_slice::<Response>(json),
                    Ok(Response::Panes { .. })
                )
            {
                return;
            }
        }
    });

    // Deliberately leave `wedged` unread until now — reading it would drain its
    // queue and un-wedge it. By this point the server has dropped it, so a drain
    // to EOF confirms the disconnect.
    sleep(Duration::from_secs(2)).await;
    let dropped = timeout(DEADLINE, async {
        let mut buf = [0u8; 4096];
        loop {
            match wedged.stream.read(&mut buf).await {
                Ok(0) | Err(_) => return true,
                Ok(_) => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(dropped, "server did not disconnect the wedged client");

    timeout(DEADLINE, good_handle)
        .await
        .expect("well-behaved client round-trip timed out")
        .expect("good task panicked");

    server.stop().await;
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

/// Poll `pane list` until `pane` is detected as agent `kind` or the deadline
/// elapses.
async fn wait_agent(conn: &mut Conn, pane: PaneId, kind: &str) -> bool {
    timeout(DEADLINE, async {
        loop {
            if let Response::Panes { panes } = conn.request(Request::PaneList).await
                && panes.iter().any(|p| {
                    p.id == pane
                        && p.agent.as_ref().map(ToString::to_string).as_deref() == Some(kind)
                })
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
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let bin = copy_bin("claude", "/bin/sleep");

    new_workspace(&mut conn).await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec![bin.display().to_string(), "30".into()],
            ephemeral: false,
        })
        .await,
    );

    assert!(
        wait_agent(&mut conn, pane, "claude").await,
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
    let server = TestServer::start();
    let mut control = server.connect().await;
    let bin = copy_bin("claude", "/bin/cat");

    new_workspace(&mut control).await;
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec![bin.display().to_string()],
                ephemeral: false,
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

/// A fresh empty temp directory, unique per call.
fn fresh_dir(prefix: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tutti-{prefix}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Initialize a real jj repo in a fresh temp dir with two added files, so a
/// diff has real content and its `--stat` summary reads `2 files changed`.
/// Returns `None` (test skips) when `jj` is not on PATH.
fn init_jj_repo() -> Option<PathBuf> {
    if std::process::Command::new("jj")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: jj is not on PATH");
        return None;
    }
    let dir = fresh_dir("jjrepo");
    let out = std::process::Command::new("jj")
        .args(["git", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "jj git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(dir.join("tracked_file.txt"), "hello from tutti\n").unwrap();
    std::fs::write(dir.join("second.txt"), "another line\n").unwrap();
    Some(dir)
}

/// `workspace diff` shells out to jj: the full diff names the edited file and
/// the `--stat` form carries the summary line. Skips cleanly without `jj`.
#[tokio::test]
async fn workspace_diff_serves_jj_content_and_stat() {
    let Some(dir) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );

    match conn
        .request(Request::WorkspaceDiff {
            id: ws,
            stat: false,
        })
        .await
    {
        Response::Content { lines } => assert!(
            lines.iter().any(|l| l.contains("tracked_file.txt")),
            "diff should mention the edited file, got {lines:?}"
        ),
        other => panic!("expected Content, got {other:?}"),
    }

    match conn
        .request(Request::WorkspaceDiff { id: ws, stat: true })
        .await
    {
        Response::Content { lines } => assert!(
            lines.iter().any(|l| l.contains("files changed")),
            "stat should carry the summary line, got {lines:?}"
        ),
        other => panic!("expected Content, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// A `workspace diff` on a directory that is not a jj repo answers Error.
#[tokio::test]
async fn workspace_diff_non_repo_errors() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let dir = fresh_dir("nonjj");
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );

    match conn
        .request(Request::WorkspaceDiff {
            id: ws,
            stat: false,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("not a jj workspace"),
            "expected a not-a-jj-workspace error, got {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// An ephemeral pane leaves no corpse: when its child exits it is removed from
/// the pane list entirely, and an attached client sees a `LayoutChanged` that no
/// longer carries it (rather than an exited-marked row).
#[tokio::test]
async fn ephemeral_pane_vanishes_on_child_exit() {
    let server = TestServer::start();

    let mut control = server.connect().await;
    new_workspace(&mut control).await;

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));

    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
                ephemeral: true,
            })
            .await,
    );

    assert!(
        wait_gone(&mut control, pane).await,
        "an ephemeral pane should be removed from the pane list on exit"
    );
    assert!(
        layout_drops_pane(&mut viewer, pane).await,
        "the removal should broadcast a LayoutChanged without the pane"
    );

    server.stop().await;
}

/// Poll `pane list` until `pane` is gone entirely.
async fn wait_gone(conn: &mut Conn, pane: PaneId) -> bool {
    timeout(DEADLINE, async {
        loop {
            if let Response::Panes { panes } = conn.request(Request::PaneList).await
                && !panes.iter().any(|p| p.id == pane)
            {
                return true;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// Read events until a `LayoutChanged` whose view no longer lists `pane`.
async fn layout_drops_pane(viewer: &mut Conn, pane: PaneId) -> bool {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Ok(Event::LayoutChanged { workspaces }) =
                    serde_json::from_slice::<Event>(&json)
                && !workspaces
                    .iter()
                    .flat_map(|w| &w.tabs)
                    .flat_map(|t| &t.panes)
                    .any(|p| p.id == pane)
            {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Focusing a `Done` pane marks it seen: its state becomes `Idle`.
#[tokio::test]
async fn focus_transitions_done_pane_to_idle() {
    let server = TestServer::start();
    let mut conn = server.connect().await;

    new_workspace(&mut conn).await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            ephemeral: false,
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

/// Run a `jj` subcommand in `dir` and return its output (helpers below drive a
/// real repo directly to set up stale/forget scenarios).
fn run_jj(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("jj")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// The short id of `dir`'s current jj operation.
fn jj_op_head(dir: &Path) -> String {
    let out = run_jj(
        dir,
        &[
            "op",
            "log",
            "--no-graph",
            "--limit",
            "1",
            "-T",
            "id.short()",
        ],
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The sibling fork destination for `repo` named `name`, matching the server's
/// `<repo-parent>/<repo-basename>-<name>` rule.
fn fork_sibling(repo: &Path, name: &str) -> PathBuf {
    repo.parent().unwrap().join(format!(
        "{}-{name}",
        repo.file_name().unwrap().to_string_lossy()
    ))
}

/// The ids of the session's workspaces.
async fn workspace_ids(conn: &mut Conn) -> Vec<WorkspaceId> {
    match conn.request(Request::WorkspaceList).await {
        Response::Workspaces { workspaces } => workspaces.into_iter().map(|w| w.id).collect(),
        other => panic!("expected Workspaces, got {other:?}"),
    }
}

/// Read `LayoutChanged` events on an attached connection until workspace `ws`
/// reports `stale == want`.
async fn wait_workspace_stale(viewer: &mut Conn, ws: WorkspaceId, want: bool) -> bool {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Ok(Event::LayoutChanged { workspaces }) =
                    serde_json::from_slice::<Event>(&json)
                && workspaces.iter().any(|w| w.id == ws && w.stale == want)
            {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Forking a jj workspace materializes a sibling checkout and mounts it as a
/// tutti workspace with a shell pane. Skips cleanly without `jj`.
#[tokio::test]
async fn fork_creates_sibling_checkout_with_a_shell_pane() {
    let Some(origin) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: origin.clone(),
        })
        .await,
    );
    let fork = workspace_id(
        conn.request(Request::WorkspaceFork {
            id: ws,
            name: "feature".into(),
            revision: None,
        })
        .await,
    );

    let dest = fork_sibling(&origin, "feature");
    assert!(
        dest.join(".jj").exists(),
        "the fork should have its own jj working copy at {}",
        dest.display()
    );

    // Both workspaces are listed, and the fork carries a shell pane.
    let ids = workspace_ids(&mut conn).await;
    assert!(
        ids.contains(&ws) && ids.contains(&fork),
        "both listed: {ids:?}"
    );

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    let workspaces = match viewer.response().await {
        Response::Attached { workspaces, .. } => workspaces,
        other => panic!("expected Attached, got {other:?}"),
    };
    let forked = workspaces
        .iter()
        .find(|w| w.id == fork)
        .expect("fork workspace present in the view");
    assert!(
        forked.tabs.iter().any(|t| !t.panes.is_empty()),
        "the fork should have been given a shell pane"
    );

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&dest);
    server.stop().await;
}

/// A fork whose destination directory already exists is refused rather than
/// silently reused. Skips cleanly without `jj`.
#[tokio::test]
async fn fork_name_collision_errors() {
    let Some(origin) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: origin.clone(),
        })
        .await,
    );
    // First fork succeeds.
    workspace_id(
        conn.request(Request::WorkspaceFork {
            id: ws,
            name: "dup".into(),
            revision: None,
        })
        .await,
    );
    // Second fork with the same name collides on the existing directory.
    match conn
        .request(Request::WorkspaceFork {
            id: ws,
            name: "dup".into(),
            revision: None,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("already exists"),
            "expected a destination-exists error, got {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(fork_sibling(&origin, "dup"));
    server.stop().await;
}

/// An invalid fork name is rejected fail-fast, before any jj call (so this runs
/// even without `jj` installed).
#[tokio::test]
async fn fork_invalid_name_errors() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        })
        .await,
    );
    match conn
        .request(Request::WorkspaceFork {
            id: ws,
            name: "bad name".into(),
            revision: None,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("invalid fork name"),
            "expected an invalid-name error, got {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
    server.stop().await;
}

/// Forking a workspace that is not under a jj repo errors (jj is required; no
/// git/hg adapters). Needs no `jj` binary — the check is a `.jj` ancestor walk.
#[tokio::test]
async fn fork_of_non_jj_workspace_errors() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let dir = fresh_dir("nonjj-fork");
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );
    match conn
        .request(Request::WorkspaceFork {
            id: ws,
            name: "x".into(),
            revision: None,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("not a jj workspace"),
            "expected a not-a-jj-workspace error, got {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// A fork whose `@` is rewritten from the origin is reported stale, and
/// `workspace update` clears it. Skips cleanly without `jj`.
#[tokio::test]
async fn fork_goes_stale_then_update_clears_it() {
    let Some(origin) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: origin.clone(),
        })
        .await,
    );
    let fork = workspace_id(
        conn.request(Request::WorkspaceFork {
            id: ws,
            name: "stalefork".into(),
            revision: None,
        })
        .await,
    );
    let dest = fork_sibling(&origin, "stalefork");

    // Make the fork stale: note the origin's op, advance the fork's own working
    // copy, then rewind the repo from the origin to before that advance. jj then
    // sees the fork's on-disk state ahead of the view — the stale condition.
    let before = jj_op_head(&origin);
    std::fs::write(dest.join("forkwork.txt"), "local\n").unwrap();
    run_jj(&dest, &["status"]);
    let restored = run_jj(&origin, &["op", "restore", &before]);
    assert!(
        restored.status.success(),
        "op restore failed: {}",
        String::from_utf8_lossy(&restored.stderr)
    );

    // Attaching triggers a refresh across all workspaces, flipping the stale flag.
    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));
    assert!(
        wait_workspace_stale(&mut viewer, fork, true).await,
        "the fork should be reported stale after its @ was rewritten"
    );

    // `workspace update` runs update-stale and refreshes, clearing the flag.
    assert_eq!(
        conn.request(Request::WorkspaceUpdate { id: fork }).await,
        Response::Ok
    );
    assert!(
        wait_workspace_stale(&mut viewer, fork, false).await,
        "workspace update should clear the stale flag"
    );

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&dest);
    server.stop().await;
}

/// `kill --discard` on a fork removes the checkout from disk, forgets it in jj,
/// and drops the tutti workspace. Skips cleanly without `jj`.
#[tokio::test]
async fn discard_removes_and_forgets_a_fork() {
    let Some(origin) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: origin.clone(),
        })
        .await,
    );
    let fork = workspace_id(
        conn.request(Request::WorkspaceFork {
            id: ws,
            name: "gone".into(),
            revision: None,
        })
        .await,
    );
    let dest = fork_sibling(&origin, "gone");
    assert!(dest.exists(), "fork checkout should exist before discard");

    // The reply follows the async cleanup, so by the time it lands the checkout
    // is gone and jj has forgotten the workspace.
    assert_eq!(
        conn.request(Request::WorkspaceKill {
            id: fork,
            discard: true,
        })
        .await,
        Response::Ok
    );
    assert!(!dest.exists(), "discard should remove the fork checkout");
    let listed = run_jj(&origin, &["workspace", "list"]);
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains("gone"),
        "jj should no longer list the forgotten workspace"
    );
    let ids = workspace_ids(&mut conn).await;
    assert!(!ids.contains(&fork), "the tutti workspace should be gone");

    let _ = std::fs::remove_dir_all(&origin);
    server.stop().await;
}

/// `kill --discard` on a workspace tutti did not fork is refused outright, and
/// nothing on disk is touched — tutti never deletes a checkout it did not create.
#[tokio::test]
async fn discard_on_a_non_fork_is_refused_and_deletes_nothing() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let dir = fresh_dir("keepme");
    std::fs::write(dir.join("sentinel.txt"), "keep\n").unwrap();
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );

    match conn
        .request(Request::WorkspaceKill {
            id: ws,
            discard: true,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("did not fork"),
            "expected a refusal error, got {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(
        dir.join("sentinel.txt").exists(),
        "a non-fork workspace must not be deleted by --discard"
    );
    let ids = workspace_ids(&mut conn).await;
    assert!(
        ids.contains(&ws),
        "a refused discard must leave the workspace in place"
    );

    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}
