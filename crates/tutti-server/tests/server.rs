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
    AgentHookEvent, AgentState, Checkpoint, Direction, Event, Frame, Layout, PaneData, PaneId,
    PaneInfo, Request, Response, StateSource, SubagentInfo, WorkspaceId,
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
            sandbox: None,
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
                    since: None,
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

fn wait_for(
    pane: PaneId,
    until: &[tutti_core::WaitFor],
    next: bool,
    timeout_ms: Option<u64>,
) -> Request {
    Request::PaneWait {
        pane,
        until: until.to_vec(),
        next,
        timeout_ms,
    }
}

/// The state a wait answered with, and whether it timed out.
fn waited(response: Response) -> (AgentState, bool) {
    match response {
        Response::Waited { pane, timed_out } => (pane.state, timed_out),
        other => panic!("expected Waited, got {other:?}"),
    }
}

fn error_message(response: Response) -> String {
    match response {
        Response::Error { message } => message,
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Send a wait on `waiter` and return once the server has subscribed it: one
/// connection's frames are handled in order, so a later request's reply
/// proves the wait is in place — still pending, its own reply comes after.
async fn start_wait(waiter: &mut Conn, wait: Request) {
    waiter.send(&wait).await;
    assert!(matches!(
        waiter.request(Request::PaneList).await,
        Response::Panes { .. }
    ));
}

/// `pane wait` is level-triggered: a pane already in an `until` state answers
/// at once, and one that is not runs out the timeout, answering with the state
/// it is still in. A missing pane and an empty `until` are errors.
#[tokio::test]
async fn pane_wait_answers_at_once_or_times_out() {
    use tutti_core::WaitFor;
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_sleeper(&mut conn).await;

    hook(&mut conn, pane, AgentHookEvent::Activity { detail: None }).await;
    assert_eq!(
        waited(
            conn.request(wait_for(pane, &[WaitFor::Done], false, Some(200)))
                .await
        ),
        (AgentState::Working, true),
        "a working pane runs out the timeout"
    );
    assert_eq!(
        waited(
            conn.request(wait_for(
                pane,
                &[WaitFor::Blocked, WaitFor::Working],
                false,
                None
            ))
            .await
        ),
        (AgentState::Working, false),
        "a pane already in a wanted state answers at once"
    );
    hook(&mut conn, pane, AgentHookEvent::Done).await;
    assert_eq!(
        waited(
            conn.request(wait_for(pane, &[WaitFor::Done], false, None))
                .await
        ),
        (AgentState::Done, false)
    );

    let missing = error_message(
        conn.request(wait_for(PaneId(999), &[WaitFor::Done], false, None))
            .await,
    );
    assert!(missing.contains("no pane 999"), "{missing}");
    let empty = error_message(conn.request(wait_for(pane, &[], false, None)).await);
    assert!(empty.contains("a state to wait for"), "{empty}");

    server.stop().await;
}

/// A wait not yet satisfied answers on the transition that satisfies it,
/// driven here by hook events on another connection.
#[tokio::test]
async fn pane_wait_answers_on_a_later_transition() {
    use tutti_core::WaitFor;
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    hook(
        &mut control,
        pane,
        AgentHookEvent::Activity { detail: None },
    )
    .await;

    let mut waiter = server.connect().await;
    start_wait(
        &mut waiter,
        wait_for(pane, &[WaitFor::Done, WaitFor::Blocked], false, None),
    )
    .await;
    hook(
        &mut control,
        pane,
        AgentHookEvent::Blocked { message: None },
    )
    .await;
    assert_eq!(
        waited(waiter.response().await),
        (AgentState::Blocked, false)
    );

    server.stop().await;
}

/// `next` waits for a fresh transition: it ignores the state the pane is
/// already in, and a human focusing a Done pane (Done → Idle) is no fresh
/// finish — though it is exactly what `--until idle --next` waits for.
#[tokio::test]
async fn pane_wait_next_ignores_the_current_state_and_a_focus() {
    use tutti_core::WaitFor;
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    hook(&mut control, pane, AgentHookEvent::Done).await;
    assert_eq!(
        waited(
            control
                .request(wait_for(pane, &[WaitFor::Done], true, Some(200)))
                .await
        ),
        (AgentState::Done, true),
        "a pane done before the wait began is no fresh finish"
    );

    let mut waiter = server.connect().await;
    start_wait(&mut waiter, wait_for(pane, &[WaitFor::Done], true, None)).await;
    let focus = Request::PaneFocus { pane };
    assert_eq!(control.request(focus.clone()).await, Response::Ok);
    hook(
        &mut control,
        pane,
        AgentHookEvent::Activity { detail: None },
    )
    .await;
    hook(&mut control, pane, AgentHookEvent::Done).await;
    assert_eq!(
        waited(waiter.response().await),
        (AgentState::Done, false),
        "the focus (it would answer idle) is skipped for the next turn's done"
    );

    start_wait(&mut waiter, wait_for(pane, &[WaitFor::Idle], true, None)).await;
    assert_eq!(control.request(focus).await, Response::Ok);
    assert_eq!(waited(waiter.response().await), (AgentState::Idle, false));

    server.stop().await;
}

/// The child exiting ends a wait, since the pane can move no more: a matching
/// final state answers — even when the exit moved nothing (a Done pane
/// exiting) — and anything else is an error naming the exit. An exited pane
/// answers at once: by its final state, or under `next` with an error, as it
/// can never finish again. A wait for idle outlives the exit: a look still
/// turns the pane's Done into Idle.
#[tokio::test]
async fn a_child_exit_ends_a_wait() {
    use tutti_core::WaitFor;
    let server = TestServer::start();
    let mut control = server.connect().await;
    let mut waiter = server.connect().await;
    let exit_with = |pane, code: &str| Request::PaneSend {
        pane,
        text: Some(format!("{code}\n")),
        keys: None,
        expect: None,
    };

    let done = run_marker(&mut control, "read code; exit $code").await;
    hook(&mut control, done, AgentHookEvent::Done).await;
    start_wait(&mut waiter, wait_for(done, &[WaitFor::Done], true, None)).await;
    assert_eq!(control.request(exit_with(done, "0")).await, Response::Ok);
    assert_eq!(waited(waiter.response().await), (AgentState::Done, false));

    let busy = run_marker(&mut control, "read code; exit $code").await;
    hook(
        &mut control,
        busy,
        AgentHookEvent::Activity { detail: None },
    )
    .await;
    start_wait(
        &mut waiter,
        wait_for(busy, &[WaitFor::Blocked], false, None),
    )
    .await;
    assert_eq!(control.request(exit_with(busy, "3")).await, Response::Ok);
    let exited = error_message(waiter.response().await);
    assert!(
        exited.contains(&format!("pane {busy} exited with code 3 (state: done)")),
        "{exited}"
    );

    assert_eq!(
        waited(
            control
                .request(wait_for(busy, &[WaitFor::Done], false, None))
                .await
        ),
        (AgentState::Done, false)
    );
    let never = error_message(
        control
            .request(wait_for(busy, &[WaitFor::Done], true, None))
            .await,
    );
    assert!(never.contains("exited with code 3"), "{never}");
    start_wait(&mut waiter, wait_for(busy, &[WaitFor::Idle], false, None)).await;
    assert_eq!(
        control.request(Request::PaneFocus { pane: busy }).await,
        Response::Ok
    );
    assert_eq!(waited(waiter.response().await), (AgentState::Idle, false));

    server.stop().await;
}

/// A waited-on pane leaving the session ends the wait with an error, whether
/// it is killed or its workspace is.
#[tokio::test]
async fn a_killed_pane_ends_its_wait() {
    use tutti_core::WaitFor;
    let server = TestServer::start();
    let mut control = server.connect().await;
    let mut waiter = server.connect().await;
    let sleeper = Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/sleep".into(), "30".into()],
        ephemeral: false,
        sandbox: None,
    };

    let workspace = new_workspace(&mut control).await;
    let pane = pane_id(control.request(sleeper.clone()).await);
    start_wait(&mut waiter, wait_for(pane, &[WaitFor::Done], false, None)).await;
    assert_eq!(
        control.request(Request::PaneKill { pane }).await,
        Response::Ok
    );
    let gone = error_message(waiter.response().await);
    assert!(gone.contains(&format!("pane {pane} is gone")), "{gone}");

    let pane = pane_id(control.request(sleeper).await);
    start_wait(&mut waiter, wait_for(pane, &[WaitFor::Done], false, None)).await;
    assert_eq!(
        control
            .request(Request::WorkspaceKill {
                id: workspace,
                discard: false,
            })
            .await,
        Response::Ok
    );
    let gone = error_message(waiter.response().await);
    assert!(gone.contains(&format!("pane {pane} is gone")), "{gone}");

    server.stop().await;
}

/// A waiter hanging up mid-wait leaves the daemon serving: the pane moves on,
/// and a fresh wait on another connection still answers.
#[tokio::test]
async fn a_waiter_hanging_up_leaves_the_server_serving() {
    use tutti_core::WaitFor;
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    {
        let mut waiter = server.connect().await;
        start_wait(&mut waiter, wait_for(pane, &[WaitFor::Done], false, None)).await;
    }
    hook(&mut control, pane, AgentHookEvent::Done).await;
    let mut waiter = server.connect().await;
    assert_eq!(
        waited(
            waiter
                .request(wait_for(pane, &[WaitFor::Done], false, None))
                .await
        ),
        (AgentState::Done, false)
    );
    assert_eq!(pane_ids(&mut control).await, vec![pane]);

    server.stop().await;
}

fn ran(response: Response) -> (WorkspaceId, tutti_core::TabId, PaneId) {
    match response {
        Response::Ran {
            workspace,
            tab,
            pane,
        } => (workspace, tab, pane),
        other => panic!("expected Ran, got {other:?}"),
    }
}

fn run_sleep(workspace: Option<WorkspaceId>, dir: Option<PathBuf>, title: Option<&str>) -> Request {
    Request::Run {
        workspace,
        dir,
        cmd: vec!["/bin/sleep".into(), "30".into()],
        sandbox: None,
        title: title.map(Into::into),
    }
}

/// `run` spawns into a fresh tab on every call — never splitting a tab in use
/// — titles the pane when asked, and leaves the session's current tab where it
/// was, so an attached client's next launch still lands where it is looking.
#[tokio::test]
async fn run_opens_a_fresh_tab_per_call_and_keeps_the_current_one() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let workspace = new_workspace(&mut conn).await;
    pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/cat".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );

    let (first_ws, first_tab, _) = ran(conn.request(run_sleep(None, None, Some("worker"))).await);
    let (second_ws, second_tab, _) =
        ran(conn.request(run_sleep(Some(workspace), None, None)).await);
    assert_eq!((first_ws, second_ws), (workspace, workspace));
    assert_ne!(first_tab, second_tab);
    match conn.request(Request::TabList { workspace: None }).await {
        Response::Tabs { tabs } => {
            let tabs: Vec<_> = tabs.iter().map(|t| (t.name.as_str(), t.active)).collect();
            assert_eq!(
                tabs,
                [("cat", true), ("worker", false), ("sleep", false)],
                "each run got its own tab; the current one stayed put"
            );
        }
        other => panic!("expected Tabs, got {other:?}"),
    }
    assert_eq!(pane_ids(&mut conn).await.len(), 3);

    server.stop().await;
}

/// `run --dir` reuses the workspace that has the directory open, however it is
/// spelled, and otherwise mounts it — spawning into the mount's own first tab
/// rather than leaving it empty beside a second.
#[tokio::test]
async fn run_reuses_an_open_dir_and_mounts_a_new_one() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let open = fresh_dir("run-open");
    let workspace = workspace_id(
        conn.request(Request::WorkspaceNew { dir: open.clone() })
            .await,
    );

    let (reused, _, _) = ran(conn
        .request(run_sleep(None, Some(open.join(".")), None))
        .await);
    assert_eq!(reused, workspace, "the open dir's workspace is reused");

    let fresh = fresh_dir("run-mount");
    let (mounted, tab, _) = ran(conn
        .request(run_sleep(None, Some(fresh.clone()), None))
        .await);
    assert_ne!(mounted, workspace);
    assert!(workspace_ids(&mut conn).await.contains(&mounted));
    match conn
        .request(Request::TabList {
            workspace: Some(mounted),
        })
        .await
    {
        Response::Tabs { tabs } => assert_eq!(
            tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
            [tab],
            "the mount's own tab holds the pane"
        ),
        other => panic!("expected Tabs, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&open);
    let _ = std::fs::remove_dir_all(&fresh);
    server.stop().await;
}

/// `run` refuses a directory that is not on disk and a workspace named beside
/// a directory, and a command that cannot spawn leaves no empty tab or
/// workspace behind.
#[tokio::test]
async fn run_refuses_bad_targets_and_rolls_back_a_failed_spawn() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let workspace = new_workspace(&mut conn).await;
    let fresh = fresh_dir("run-rollback");
    let no_program = |dir| Request::Run {
        workspace: None,
        dir,
        cmd: vec!["/nonexistent/tutti-no-such-program".into()],
        sandbox: None,
        title: None,
    };

    for (request, needle) in [
        (
            run_sleep(None, Some("/nonexistent/tutti-run-dir".into()), None),
            "does not exist",
        ),
        (
            run_sleep(Some(workspace), Some(fresh.clone()), None),
            "not both",
        ),
        (no_program(None), "failed to spawn"),
        (no_program(Some(fresh.clone())), "failed to spawn"),
    ] {
        let message = error_message(conn.request(request).await);
        assert!(message.contains(needle), "{message}");
    }
    assert_eq!(
        workspace_ids(&mut conn).await,
        [workspace],
        "no mount was left"
    );
    assert_eq!(tab_names(&mut conn).await.len(), 1, "no empty tab was left");

    let _ = std::fs::remove_dir_all(&fresh);
    server.stop().await;
}

/// Mounting a directory that is not on disk is refused outright — otherwise
/// every pane spawned in it would land in $HOME via portable-pty's fallback.
#[tokio::test]
async fn workspace_new_rejects_a_missing_dir() {
    let server = TestServer::start();
    let mut conn = server.connect().await;

    let response = conn
        .request(Request::WorkspaceNew {
            dir: "/nonexistent/tutti-test-dir".into(),
        })
        .await;
    match response {
        Response::Error { message } => assert!(
            message.contains("does not exist"),
            "unexpected error message: {message}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }

    server.stop().await;
}

/// The conversation a hook event names is remembered on its pane and listed
/// with it — what resuming the conversation takes — until an event naming
/// another replaces it; an event naming none keeps it.
#[tokio::test]
async fn an_agent_events_conversation_shows_in_pane_list() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_sleeper(&mut conn).await;
    let event = |event: AgentHookEvent, conversation: Option<&str>| Request::AgentEvent {
        pane,
        event,
        conversation: conversation.map(Into::into),
    };
    let listed = |response: Response| match response {
        Response::Panes { panes } => panes.into_iter().find(|p| p.id == pane).unwrap(),
        other => panic!("expected Panes, got {other:?}"),
    };

    let started = event(AgentHookEvent::SessionStarted, Some("c1"));
    assert_eq!(conn.request(started).await, Response::Ok);
    let info = listed(conn.request(Request::PaneList).await);
    assert_eq!(info.conversation.as_deref(), Some("c1"));
    assert_eq!(info.source, None, "a session start claims nothing");

    let unnamed = event(AgentHookEvent::Activity { detail: None }, None);
    assert_eq!(conn.request(unnamed).await, Response::Ok);
    let info = listed(conn.request(Request::PaneList).await);
    assert_eq!(info.conversation.as_deref(), Some("c1"), "none named, kept");

    let cleared = event(AgentHookEvent::SessionStarted, Some("c2"));
    assert_eq!(conn.request(cleared).await, Response::Ok);
    let info = listed(conn.request(Request::PaneList).await);
    assert_eq!(
        info.conversation.as_deref(),
        Some("c2"),
        "a `/clear` moves on"
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
            sandbox: None,
        })
        .await,
    );

    assert_eq!(
        conn.request(Request::PaneSend {
            pane,
            text: Some("hello-echo\n".into()),
            keys: None,
            expect: None,
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

/// `pane read --since` tails a scrolling pane. From 0 it returns every row
/// and a cursor counting those committed to scrollback; reread with that
/// cursor it returns only the rows committed since, then the screen — whose
/// rows come back once more, as committed rows, when they scroll off.
#[tokio::test]
async fn pane_read_since_tails_scrolled_output() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_marker(&mut conn, "seq 1 200; read x; seq 201 260; sleep 30").await;
    assert!(read_until(&mut conn, pane, "200").await);

    let numbers = |range: std::ops::RangeInclusive<u32>| -> Vec<String> {
        range.map(|n| n.to_string()).collect()
    };
    // 200 lines on 24 rows: 177 scrolled off, 178..200 over the cursor row.
    assert_eq!(
        read_since(&mut conn, pane, 0).await,
        (numbers(1..=200), 177, false)
    );
    assert_eq!(
        read_since(&mut conn, pane, 177).await,
        (numbers(178..=200), 177, false),
        "caught up, a read is the screen alone"
    );

    // Answering `read` echoes a newline — one more row scrolls — then 60
    // more lines print.
    assert_eq!(
        conn.request(Request::PaneSend {
            pane,
            text: Some("\n".into()),
            keys: None,
            expect: None,
        })
        .await,
        Response::Ok
    );
    assert!(read_until(&mut conn, pane, "260").await);
    let mut since_first = numbers(178..=200);
    since_first.push(String::new());
    since_first.extend(numbers(201..=260));
    assert_eq!(
        read_since(&mut conn, pane, 177).await,
        (since_first, 238, false)
    );
    assert_eq!(
        read_since(&mut conn, pane, 238).await,
        (numbers(238..=260), 238, false)
    );

    server.stop().await;
}

/// One `pane read --since`, as `(lines, cursor, truncated)`.
async fn read_since(conn: &mut Conn, pane: PaneId, since: u64) -> (Vec<String>, u64, bool) {
    match conn
        .request(Request::PaneRead {
            pane,
            lines: None,
            unwrapped: false,
            since: Some(since),
        })
        .await
    {
        Response::Tail {
            lines,
            cursor,
            truncated,
        } => (lines, cursor, truncated),
        other => panic!("expected Tail, got {other:?}"),
    }
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
            sandbox: None,
        })
        .await,
    );
    let second = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            ephemeral: false,
            sandbox: None,
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
                sandbox: None,
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

/// A snapshot carries the pane's input modes, not just its cell grid. A
/// full-screen agent like Claude Code turns on mouse reporting; if that state
/// never reaches the client, the client's mirror parser misreads the pane as a
/// plain shell and swallows the wheel into tutti's own (empty) scrollback
/// instead of forwarding it to the child.
#[tokio::test]
async fn snapshot_carries_mouse_reporting_mode() {
    let server = TestServer::start();

    let mut control = server.connect().await;
    // Turn on button/any-event mouse tracking with SGR encoding, then print a
    // visible marker so we can wait until those escapes have been parsed.
    let pane = run_marker(
        &mut control,
        "printf '\\033[?1000h\\033[?1002h\\033[?1003h\\033[?1006hREADY'; sleep 30",
    )
    .await;
    assert!(
        read_until(&mut control, pane, "READY").await,
        "pane never produced its readiness marker"
    );

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    let _ = viewer.response().await;

    let bytes = expect_pane_frame(&mut viewer, pane, true).await.bytes;
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(
        has(b"\x1b[?1003h"),
        "snapshot dropped the mouse-reporting mode"
    );
    assert!(
        has(b"\x1b[?1006h"),
        "snapshot dropped the SGR mouse encoding"
    );

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
                sandbox: None,
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
                sandbox: None,
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

/// A `PaneSplitRatio` — a split-border drag — sets the split separating its
/// two panes, clamped like a keyboard resize, and broadcasts the fresh view;
/// panes named in the wrong order, or from different tabs, are refused.
#[tokio::test]
async fn split_ratio_sets_the_separating_split_and_broadcasts() {
    let server = TestServer::start();

    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let cat = |tab| Request::PaneRun {
        tab,
        cmd: vec!["/bin/cat".into()],
        ephemeral: false,
        sandbox: None,
    };
    let left = pane_id(control.request(cat(None)).await);
    let right = pane_id(
        control
            .request(Request::PaneSplit {
                pane: left,
                direction: Direction::Horizontal,
            })
            .await,
    );

    // Attach after the split so the only LayoutChanged the viewer sees is ours.
    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));

    let drag = |a, b, ratio| Request::PaneSplitRatio { a, b, ratio };
    assert_eq!(control.request(drag(left, right, 0.3)).await, Response::Ok);
    let ratio = next_split_ratio(&mut viewer).await;
    assert!((ratio - 0.3).abs() < 1e-3, "expected ~0.3, got {ratio}");

    assert_eq!(control.request(drag(left, right, 0.99)).await, Response::Ok);
    let ratio = next_split_ratio(&mut viewer).await;
    assert!(
        (ratio - Layout::MAX_RATIO).abs() < 1e-3,
        "a drag past the bound clamps to it, got {ratio}"
    );

    let reversed = error_message(control.request(drag(right, left, 0.5)).await);
    assert!(
        reversed.contains("before"),
        "the second pane must sit right of the first: {reversed}"
    );

    let Response::TabCreated { id: tab } =
        control.request(Request::TabNew { workspace: None }).await
    else {
        panic!("expected TabCreated");
    };
    let elsewhere = pane_id(control.request(cat(Some(tab))).await);
    let apart = error_message(control.request(drag(left, elsewhere, 0.5)).await);
    assert!(
        apart.contains("different tabs"),
        "panes in different tabs share no split: {apart}"
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
                sandbox: None,
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

/// Every spawned pane carries `TUTTI_PANE` (its id) and `TUTTI_SESSION` (the
/// socket's session name) in its environment, so a Claude Code hook running
/// inside it can address this daemon. The test server's socket is `s.sock`, so
/// the session name is `s`.
#[tokio::test]
async fn spawned_pane_carries_tutti_env() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_marker(
        &mut conn,
        "printf 'ENV %s %s\\n' \"$TUTTI_PANE\" \"$TUTTI_SESSION\"; sleep 30",
    )
    .await;
    assert!(
        read_until(&mut conn, pane, &format!("ENV {pane} s")).await,
        "the pane env should carry TUTTI_PANE=<id> and TUTTI_SESSION=<name>"
    );
    server.stop().await;
}

/// The subagents of `pane` from a fresh `pane list`.
async fn pane_subagents(conn: &mut Conn, pane: PaneId) -> Vec<SubagentInfo> {
    match conn.request(Request::PaneList).await {
        Response::Panes { panes } => panes
            .into_iter()
            .find(|p| p.id == pane)
            .map(|p| p.subagents)
            .unwrap_or_default(),
        other => panic!("expected Panes, got {other:?}"),
    }
}

/// Run a plain (non-agent) sleeper pane and return its id.
async fn run_sleeper(conn: &mut Conn) -> PaneId {
    new_workspace(conn).await;
    pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    )
}

/// A `Blocked` hook event flips the pane's state and broadcasts `StateChanged`
/// to an attached client within a tick — no screen classification involved.
#[tokio::test]
async fn agent_event_blocks_and_broadcasts() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));

    assert_eq!(
        control
            .request(Request::AgentEvent {
                pane,
                event: AgentHookEvent::Blocked {
                    message: Some("allow edit?".into()),
                },
                conversation: None,
            })
            .await,
        Response::Ok
    );

    let mut order = Vec::new();
    wait_state_event(&mut viewer, pane, AgentState::Blocked, &mut order).await;

    server.stop().await;
}

/// Subagent hook events maintain the pane's subagent list: started rows are
/// appended and capped at 16 (oldest dropped), a stop finishes a row, and a
/// `Done` sweeps the finished rows out.
#[tokio::test]
async fn agent_event_maintains_subagent_list_with_cap() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_sleeper(&mut conn).await;

    // Twenty starts, list caps at 16 with the four oldest (s0..s3) dropped.
    for i in 0..20 {
        assert_eq!(
            conn.request(Request::AgentEvent {
                pane,
                event: AgentHookEvent::SubagentStarted {
                    id: format!("s{i}"),
                    desc: format!("task {i}"),
                },
                conversation: None,
            })
            .await,
            Response::Ok
        );
    }
    let subs = pane_subagents(&mut conn, pane).await;
    assert_eq!(subs.len(), 16, "the subagent list caps at 16");
    assert_eq!(
        subs.first().unwrap().id,
        "s4",
        "the oldest four are dropped"
    );
    assert!(subs.iter().all(|s| s.running), "all still running");

    // Stopping s4 (a matching id) finishes just that row.
    assert_eq!(
        conn.request(Request::AgentEvent {
            pane,
            event: AgentHookEvent::SubagentStopped { id: "s4".into() },
            conversation: None,
        })
        .await,
        Response::Ok
    );
    let subs = pane_subagents(&mut conn, pane).await;
    assert!(
        !subs.iter().find(|s| s.id == "s4").unwrap().running,
        "the stopped subagent is marked finished but kept"
    );
    assert_eq!(subs.len(), 16, "a finished subagent is kept until Done");

    // Done sweeps the finished row, leaving only the running ones.
    assert_eq!(
        conn.request(Request::AgentEvent {
            pane,
            event: AgentHookEvent::Done,
            conversation: None,
        })
        .await,
        Response::Ok
    );
    let subs = pane_subagents(&mut conn, pane).await;
    assert_eq!(subs.len(), 15, "Done sweeps the finished subagent");
    assert!(subs.iter().all(|s| s.running), "only running rows remain");

    server.stop().await;
}

/// Once a pane reports a hook event it is hook-driven: the screen classifier
/// stops touching it, so a working-pattern that would otherwise move a detected
/// agent to `Working` leaves the hook-set `Blocked` state untouched.
#[tokio::test]
async fn hook_driven_pane_ignores_screen_classification() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    // A `cat` renamed `claude` is detected as an agent and echoes sent text onto
    // its screen, so a working-pattern can be fed to the classifier.
    let bin = copy_bin("claude", "/bin/cat");
    new_workspace(&mut conn).await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec![bin.display().to_string()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );
    assert!(
        wait_agent(&mut conn, pane, "claude").await,
        "the pane should be detected as the claude agent first"
    );

    // A hook Blocked event sets the state and marks the pane hook-driven.
    assert_eq!(
        conn.request(Request::AgentEvent {
            pane,
            event: AgentHookEvent::Blocked { message: None },
            conversation: None,
        })
        .await,
        Response::Ok
    );
    assert!(
        wait_state(&mut conn, pane, AgentState::Blocked).await,
        "the hook event should set the pane Blocked"
    );

    // Feed a working-pattern: it would move a non-hook agent to Working, but the
    // classifier skips a hook-driven pane, so the state must stay Blocked.
    assert_eq!(
        conn.request(Request::PaneSend {
            pane,
            text: Some("esc to interrupt\n".into()),
            keys: None,
            expect: None,
        })
        .await,
        Response::Ok
    );
    // Several classify passes (300ms each) go by; the state must not budge.
    sleep(Duration::from_millis(900)).await;
    assert!(
        matches!(
            conn.request(Request::PaneList).await,
            Response::Panes { panes } if panes.iter().any(|p| p.id == pane && p.state == AgentState::Blocked)
        ),
        "a hook-driven pane must ignore the screen classifier and stay Blocked"
    );

    server.stop().await;
}

/// Poll `pane list` until `pane` satisfies `want` or the deadline elapses.
async fn wait_pane(conn: &mut Conn, pane: PaneId, want: impl Fn(&PaneInfo) -> bool) -> bool {
    timeout(DEADLINE, async {
        loop {
            if let Response::Panes { panes } = conn.request(Request::PaneList).await
                && panes.iter().any(|p| p.id == pane && want(p))
            {
                return true;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// Type a `printf` of `OSC <body> ST` into the shell in `pane`. The echoed
/// command line holds no raw ESC — only printf's output does.
async fn printf_osc(conn: &mut Conn, pane: PaneId, body: &str) {
    let sent = conn
        .request(Request::PaneSend {
            pane,
            text: Some(format!("printf '\\033]{body}\\033\\\\'\n")),
            keys: None,
            expect: None,
        })
        .await;
    assert_eq!(sent, Response::Ok);
}

/// A plain shell pane's own status reports drive its state: OSC 9;4 progress
/// works and clears, OSC 7501 works and finishes with a `msg` notification,
/// each listed under its label while it speaks — and no 9;4 ever surfaces as a
/// notification (it used to, as an OSC 9 with body `4;1;40`).
#[tokio::test]
async fn a_shell_panes_own_status_reports_drive_its_state() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/sh".into()],
                ephemeral: false,
                sandbox: None,
            })
            .await,
    );
    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    let label = |p: &PaneInfo| p.agent.as_ref().map(ToString::to_string);

    printf_osc(&mut control, pane, "9;4;1;40").await;
    assert!(
        wait_pane(&mut control, pane, |p| p.state == AgentState::Working
            && p.source == Some(StateSource::Progress)
            && label(p).as_deref() == Some("sh"))
        .await,
        "9;4;1 claims Working for Progress and lists the pane under its title"
    );
    printf_osc(&mut control, pane, "9;4;0").await;
    assert!(
        wait_pane(&mut control, pane, |p| p.state == AgentState::Idle
            && p.source.is_none()
            && p.agent.is_none())
        .await,
        "9;4;0 ends the claim, settles to Idle and unlists the pane"
    );
    printf_osc(&mut control, pane, "7501;state=working:app=build").await;
    assert!(
        wait_pane(&mut control, pane, |p| p.state == AgentState::Working
            && p.source == Some(StateSource::Program)
            && label(p).as_deref() == Some("build"))
        .await,
        "7501 working claims the pane for Program under its app label"
    );
    printf_osc(&mut control, pane, "7501;state=done:app=build:msg=QnVpbHQ=").await;
    assert!(
        wait_pane(&mut control, pane, |p| p.state == AgentState::Done
            && p.source == Some(StateSource::Program))
        .await,
        "7501 done lands Done"
    );

    let mut notes = Vec::new();
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Ok(Event::PaneNotification {
                    pane: p,
                    title,
                    body,
                }) = serde_json::from_slice::<Event>(&json)
                && p == pane
            {
                let done = body.as_deref() == Some("Built");
                notes.push((title, body));
                if done {
                    return;
                }
            }
        }
    })
    .await
    .expect("the 7501 msg should arrive as a notification");
    assert_eq!(
        notes,
        vec![(Some("build".to_string()), Some("Built".to_string()))],
        "only the 7501 msg notifies — no 9;4 report became a notification"
    );

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
            sandbox: None,
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
                sandbox: None,
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
                expect: None,
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
                expect: None,
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

/// A stand-in for an agent's permission dialog: a shell that asks, reads one
/// answer line, and reports it. The tty echoes in cooked mode, so any byte
/// written to the pane redraws its prompt row — a fingerprint unchanged after a
/// refusal proves the refusal wrote nothing.
const PROMPT: &str = "printf 'Proceed? [y/n] '; read a; echo \"answered:$a\"; sleep 30";

/// Run `PROMPT`, wait for it to draw, and put the pane in `Blocked` with a hook
/// event (deterministic, unlike the screen classifier). Returns the pane and
/// its prompt fingerprint.
async fn blocked_prompt(conn: &mut Conn) -> (PaneId, String) {
    let pane = run_marker(conn, PROMPT).await;
    assert!(
        read_until(conn, pane, "Proceed?").await,
        "the prompt never drew"
    );
    assert_eq!(
        conn.request(Request::AgentEvent {
            pane,
            event: AgentHookEvent::Blocked { message: None },
            conversation: None,
        })
        .await,
        Response::Ok
    );
    let fp = fingerprint(conn, pane).await;
    (pane, fp)
}

async fn fingerprint(conn: &mut Conn, pane: PaneId) -> String {
    match conn.request(Request::PaneFingerprint { pane }).await {
        Response::Fingerprint { fingerprint } => fingerprint,
        other => panic!("expected Fingerprint, got {other:?}"),
    }
}

fn guarded(pane: PaneId, text: &str, expect: &str) -> Request {
    Request::PaneSend {
        pane,
        text: Some(text.into()),
        keys: None,
        expect: Some(expect.into()),
    }
}

fn refusal(response: Response) -> String {
    match response {
        Response::Error { message } => message,
        other => panic!("expected the guarded send to be refused, got {other:?}"),
    }
}

/// A guarded send whose fingerprint still matches the blocked prompt lands.
#[tokio::test]
async fn guarded_send_answers_a_blocked_prompt() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let (pane, fp) = blocked_prompt(&mut conn).await;
    assert_eq!(fp.len(), 16, "a fingerprint is 16 hex digits: {fp}");

    assert_eq!(conn.request(guarded(pane, "y\n", &fp)).await, Response::Ok);
    assert!(
        read_until(&mut conn, pane, "answered:y").await,
        "the guarded answer never reached the prompt"
    );
    server.stop().await;
}

/// A guarded send carrying a stale fingerprint is refused and writes nothing.
#[tokio::test]
async fn guarded_send_refuses_a_changed_prompt_and_writes_nothing() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let (pane, fp) = blocked_prompt(&mut conn).await;

    let message = refusal(conn.request(guarded(pane, "y\n", "0000000000000000")).await);
    assert!(message.contains("prompt changed"), "{message}");
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fingerprint(&mut conn, pane).await,
        fp,
        "a refused send wrote to the pane (its echo moved the prompt)"
    );
    server.stop().await;
}

/// The guard answers blocked agents only: a matching fingerprint on a pane in
/// any other state is refused, nothing written.
#[tokio::test]
async fn guarded_send_refuses_a_pane_that_is_not_blocked() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_marker(&mut conn, PROMPT).await;
    assert!(read_until(&mut conn, pane, "Proceed?").await);
    let fp = fingerprint(&mut conn, pane).await;

    let message = refusal(conn.request(guarded(pane, "y\n", &fp)).await);
    assert!(message.contains("not blocked"), "{message}");
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fingerprint(&mut conn, pane).await,
        fp,
        "nothing was written"
    );
    server.stop().await;
}

/// First answer wins: two clients answering the same prompt — the second sent
/// at once, before or after the pane has echoed the first — only the first
/// lands; once the prompt has moved on, the old fingerprint stays stale.
#[tokio::test]
async fn of_two_answers_to_one_prompt_only_the_first_lands() {
    let server = TestServer::start();
    let mut first = server.connect().await;
    let mut second = server.connect().await;
    let (pane, fp) = blocked_prompt(&mut first).await;

    assert_eq!(first.request(guarded(pane, "y\n", &fp)).await, Response::Ok);
    let message = refusal(second.request(guarded(pane, "n\n", &fp)).await);
    assert!(
        message.contains("already answered") || message.contains("prompt changed"),
        "{message}"
    );
    assert!(read_until(&mut first, pane, "answered:y").await);

    let settled = fingerprint(&mut first, pane).await;
    let message = refusal(second.request(guarded(pane, "n\n", &fp)).await);
    assert!(message.contains("prompt changed"), "{message}");
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fingerprint(&mut first, pane).await,
        settled,
        "the refused second answer wrote nothing"
    );
    server.stop().await;
}

/// A client's mirror — a parser seeded from the attach snapshot, then fed the
/// deltas — fingerprints exactly like the live grid once caught up, over the
/// cases where the two grids' cells can differ: wide characters, written
/// trailing spaces, coloured blanks, a soft-wrapped row, and an absolutely
/// placed one. Checked on both the snapshot and the delta path.
#[tokio::test]
async fn a_mirror_fingerprints_like_the_live_grid() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let script = concat!(
        "printf '漢字 wide ✓ done\\n'; ",
        "printf 'trailing   \\n'; ",
        "printf '\\033[44m   \\033[0m\\n'; ",
        "printf '\\033[41m\\033[K\\033[0m\\n'; ",
        "printf '%0100d\\n' 0; ",
        "printf '\\033[20;30Hplaced\\n'; ",
        "printf 'phase one\\n'; read a; ",
        "printf '\\033[2J\\033[H❯ 1. Yes   \\n  2. No\\n\\033[42m \\033[0m\\n'; ",
        "printf '%0150d\\n' 1; printf 'phase two\\n'; sleep 30",
    );
    let pane = run_marker(&mut control, script).await;
    assert!(read_until(&mut control, pane, "phase one").await);

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    let mut mirror = None;
    let live = fingerprint(&mut control, pane).await;
    mirror_until(&mut viewer, pane, &mut mirror, &live).await;

    assert_eq!(
        control
            .request(Request::PaneSend {
                pane,
                text: Some("go\n".into()),
                keys: None,
                expect: None,
            })
            .await,
        Response::Ok
    );
    assert!(read_until(&mut control, pane, "phase two").await);
    let live = fingerprint(&mut control, pane).await;
    mirror_until(&mut viewer, pane, &mut mirror, &live).await;
    server.stop().await;
}

/// Feed `pane`'s snapshot and delta frames off an attached connection into
/// `mirror` the way the client does, until its rows fingerprint as `want`.
async fn mirror_until(
    viewer: &mut Conn,
    pane: PaneId,
    mirror: &mut Option<vt100::Parser>,
    want: &str,
) {
    let rows = |parser: &vt100::Parser| -> Vec<String> {
        let screen = parser.screen();
        screen.rows(0, screen.size().1).collect()
    };
    let caught_up = timeout(DEADLINE, async {
        loop {
            match viewer.read_frame().await {
                Frame::PaneSnapshot(d) if d.pane == pane => {
                    let mut parser = vt100::Parser::new(d.rows, d.cols, 0);
                    parser.process(&d.bytes);
                    *mirror = Some(parser);
                }
                Frame::PaneDelta(d) if d.pane == pane => mirror
                    .as_mut()
                    .expect("a delta arrived before the snapshot")
                    .process(&d.bytes),
                _ => continue,
            }
            if tutti_core::prompt_fingerprint(&rows(mirror.as_ref().unwrap())) == want {
                return;
            }
        }
    })
    .await;
    assert!(
        caught_up.is_ok(),
        "the mirror never fingerprinted like the live grid ({want}): {:#?}",
        mirror.as_ref().map(rows)
    );
}

/// A fresh empty temp directory, unique per call.
fn fresh_dir(prefix: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tutti-{prefix}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Whether `jj` is on PATH; the jj-dependent tests skip when it is not.
fn jj_on_path() -> bool {
    std::process::Command::new("jj")
        .arg("--version")
        .output()
        .is_ok()
}

/// `jj git init` in a fresh temp dir, with commit signing disabled for the repo.
/// A developer's global `signing.behavior = "own"` (GPG) makes every jj commit —
/// init, `workspace add`, `commit` — invoke gpg, which flakes under the full
/// suite's parallelism ("Signing error"/"Could not write object"); dropping
/// signing for these throwaway repos removes that. The init itself is retried a
/// few times against the same class of transient backend error, each attempt in
/// a fresh directory. Panics only after repeated failures.
fn jj_git_init(prefix: &str) -> PathBuf {
    for _ in 0..8 {
        let dir = fresh_dir(prefix);
        if run_jj(&dir, &["git", "init"]).status.success()
            && run_jj(
                &dir,
                &["config", "set", "--repo", "signing.behavior", "drop"],
            )
            .status
            .success()
        {
            return dir;
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("jj git init failed repeatedly");
}

/// Initialize a real jj repo in a fresh temp dir with two added files, so a
/// diff has real content and its `--stat` summary reads `2 files changed`.
/// Returns `None` (test skips) when `jj` is not on PATH.
fn init_jj_repo() -> Option<PathBuf> {
    if !jj_on_path() {
        eprintln!("skipping: jj is not on PATH");
        return None;
    }
    let dir = jj_git_init("jjrepo");
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

/// `workspace list` names a jj workspace's parent project — what `project
/// list` filters on and `workspace list` prints — and the project itself has
/// none. Skips cleanly without `jj`.
#[tokio::test]
async fn a_workspace_lists_the_project_it_was_added_to() {
    let Some(origin) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let project = workspace_id(
        conn.request(Request::WorkspaceNew {
            dir: origin.clone(),
        })
        .await,
    );
    let workspace = workspace_id(
        conn.request(Request::WorkspaceFork {
            id: project,
            name: "nested".into(),
            revision: None,
            dest: None,
        })
        .await,
    );

    match conn.request(Request::WorkspaceList).await {
        Response::Workspaces { workspaces } => {
            let parents: Vec<_> = workspaces.iter().map(|w| (w.id, w.parent)).collect();
            assert_eq!(parents, [(project, None), (workspace, Some(project))]);
        }
        other => panic!("expected Workspaces, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(fork_sibling(&origin, "nested"));
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
                sandbox: None,
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
            sandbox: None,
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

/// A jj operation made outside tutti — a file edit snapshotted by a bare
/// `jj status` from a shell: no agent, no pane, no tutti request — moves the
/// workspace's change stat. The op-heads watch notices and rebroadcasts the
/// view, so the sidebar and changes panel follow work done from any shell.
/// Skips cleanly without `jj`.
#[tokio::test]
async fn a_shell_side_jj_operation_refreshes_the_change_stat() {
    let Some(dir) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut control = server.connect().await;
    let ws = workspace_id(
        control
            .request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );
    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));
    // Let the watch take the repo's first mark, so the operation below is a
    // change it can see rather than its baseline.
    sleep(Duration::from_millis(1500)).await;

    std::fs::write(dir.join("third.txt"), "from a shell\n").unwrap();
    assert!(run_jj(&dir, &["status"]).status.success());

    assert!(
        wait_workspace_changes(&mut viewer, ws, "3 files", DEADLINE).await,
        "a shell-side jj operation should refresh the stat without any agent transition"
    );

    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// tutti's own probe snapshots the working copy, and a snapshot is a jj
/// operation too. The watch must not take it for a shell-side change: after a
/// tutti-triggered refresh, an edit with no jj command behind it stays unseen
/// until a real operation lands — otherwise every refresh would breed the next,
/// once a second, for as long as an agent keeps writing files. Skips cleanly
/// without `jj`.
#[tokio::test]
async fn the_watch_ignores_the_probes_own_snapshot() {
    let Some(dir) = init_jj_repo() else {
        return;
    };
    let server = TestServer::start();
    let mut control = server.connect().await;
    let ws = workspace_id(
        control
            .request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );
    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));
    sleep(Duration::from_millis(1500)).await;

    // An edit no jj command has seen, surfaced by a tutti-triggered refresh (a
    // second attach seeds every stat) — whose probe snapshots it.
    std::fs::write(
        dir.join("third.txt"),
        "edited, never snapshotted by a shell\n",
    )
    .unwrap();
    let mut second = server.connect().await;
    second.send(&Request::Attach).await;
    assert!(matches!(second.response().await, Response::Attached { .. }));
    assert!(
        wait_workspace_changes(&mut viewer, ws, "3 files", DEADLINE).await,
        "the attach-triggered probe should surface the edit"
    );

    // Another such edit: the probe's own snapshot must not trigger a second
    // probe, which would surface this one with no operation behind it.
    std::fs::write(dir.join("fourth.txt"), "also unsnapshotted\n").unwrap();
    assert!(
        !wait_workspace_changes(&mut viewer, ws, "4 files", Duration::from_millis(2200)).await,
        "the watch re-triggered on tutti's own snapshot"
    );

    // A real operation from a shell still lands it.
    assert!(run_jj(&dir, &["status"]).status.success());
    assert!(
        wait_workspace_changes(&mut viewer, ws, "4 files", DEADLINE).await,
        "a shell-side operation should still refresh the stat"
    );

    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// Read `LayoutChanged` events on an attached connection until workspace `ws`
/// reports a change stat starting with `prefix`, or `within` runs out.
async fn wait_workspace_changes(
    viewer: &mut Conn,
    ws: WorkspaceId,
    prefix: &str,
    within: Duration,
) -> bool {
    timeout(within, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Ok(Event::LayoutChanged { workspaces }) =
                    serde_json::from_slice::<Event>(&json)
                && workspaces.iter().any(|w| {
                    w.id == ws && w.changes.as_deref().is_some_and(|c| c.starts_with(prefix))
                })
            {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false)
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
            dest: None,
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
            dest: None,
        })
        .await,
    );
    // Second fork with the same name collides on the existing directory.
    match conn
        .request(Request::WorkspaceFork {
            id: ws,
            name: "dup".into(),
            revision: None,
            dest: None,
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
            dest: None,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("invalid workspace name"),
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
            dest: None,
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
            dest: None,
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
            dest: None,
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
            message.contains("did not create"),
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

/// Init a jj repo with a base commit and a `main` bookmark, so a fork has a trunk
/// to merge back into. Returns `None` (test skips) when `jj` is not on PATH.
fn init_trunk_repo() -> Option<PathBuf> {
    if !jj_on_path() {
        eprintln!("skipping: jj is not on PATH");
        return None;
    }
    let dir = jj_git_init("trunk");
    std::fs::write(dir.join("base.txt"), "base\n").unwrap();
    assert!(run_jj(&dir, &["commit", "-m", "base"]).status.success());
    assert!(
        run_jj(&dir, &["bookmark", "create", "main", "-r", "@-"])
            .status
            .success()
    );
    Some(dir)
}

/// The stdout of a `jj` read against `dir`, for asserting post-merge state.
fn jj_out(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&run_jj(dir, args).stdout).into_owned()
}

/// A `WorkspaceFork` with a client-chosen `dest` materializes the checkout at
/// exactly that directory rather than the sibling default. Skips without `jj`.
#[tokio::test]
async fn fork_with_a_custom_dest_places_the_checkout_there() {
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
    // A parent that exists with a leaf that does not — the guided-create shape.
    let dest = fresh_dir("custom-parent").join("my-workspace");
    let fork = workspace_id(
        conn.request(Request::WorkspaceFork {
            id: ws,
            name: "feature".into(),
            revision: None,
            dest: Some(dest.clone()),
        })
        .await,
    );
    assert!(
        dest.join(".jj").exists(),
        "the checkout should materialize at the custom dest {}",
        dest.display()
    );
    let ids = workspace_ids(&mut conn).await;
    assert!(ids.contains(&fork), "the custom-dest workspace is mounted");

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    server.stop().await;
}

/// Merge lands a child's non-empty working copy on trunk: the bookmark advances
/// to the child's own commit and the origin's `main` gains the child's file. With
/// no remote, `push: true` still reports `pushed: false`. Skips without `jj`.
#[tokio::test]
async fn merge_lands_a_non_empty_working_copy_on_trunk() {
    let Some(origin) = init_trunk_repo() else {
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
            dest: None,
        })
        .await,
    );
    let dest = fork_sibling(&origin, "feature");
    // The child leaves its work in the (non-empty) working copy.
    std::fs::write(dest.join("childwork.txt"), "child\n").unwrap();

    match conn
        .request(Request::WorkspaceMerge {
            id: fork,
            push: true,
        })
        .await
    {
        Response::Merged { pushed, bookmark } => {
            assert_eq!(bookmark, "main", "merged into the main trunk");
            assert!(!pushed, "no remote, so push is a silent no-op");
        }
        other => panic!("expected Merged, got {other:?}"),
    }
    assert!(
        jj_out(&origin, &["file", "list", "-r", "main"]).contains("childwork.txt"),
        "main should now carry the child's work"
    );

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&dest);
    server.stop().await;
}

/// When the child's `@` is an empty working-copy commit on top of its real work
/// (it committed), the bookmark advances to the parent `@-`, still landing the
/// work on trunk. Skips without `jj`.
#[tokio::test]
async fn merge_advances_to_the_parent_when_the_working_copy_is_empty() {
    let Some(origin) = init_trunk_repo() else {
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
            dest: None,
        })
        .await,
    );
    let dest = fork_sibling(&origin, "feature");
    // Commit the work so the child's `@` becomes a fresh empty commit on top.
    std::fs::write(dest.join("childwork.txt"), "child\n").unwrap();
    assert!(
        run_jj(&dest, &["commit", "-m", "child work"])
            .status
            .success()
    );

    match conn
        .request(Request::WorkspaceMerge {
            id: fork,
            push: false,
        })
        .await
    {
        Response::Merged { bookmark, .. } => assert_eq!(bookmark, "main"),
        other => panic!("expected Merged, got {other:?}"),
    }
    assert!(
        jj_out(&origin, &["file", "list", "-r", "main"]).contains("childwork.txt"),
        "the real work (under the empty @) still lands on main"
    );

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&dest);
    server.stop().await;
}

/// A merge that would conflict is refused and undone: the Error names the
/// conflict, `main` is left where it was (the origin's change, not the child's),
/// and no conflicted commit is reachable from `main`. Skips without `jj`.
#[tokio::test]
async fn merge_conflict_is_refused_and_undone() {
    let Some(origin) = init_trunk_repo() else {
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
            dest: None,
        })
        .await,
    );
    let dest = fork_sibling(&origin, "feature");

    // The origin advances main with its own edit to the shared line…
    std::fs::write(origin.join("base.txt"), "origin-change\n").unwrap();
    assert!(
        run_jj(&origin, &["commit", "-m", "origin-change"])
            .status
            .success()
    );
    assert!(
        run_jj(&origin, &["bookmark", "set", "main", "-r", "@-"])
            .status
            .success()
    );
    // …while the child edits the same line differently.
    std::fs::write(dest.join("base.txt"), "child-change\n").unwrap();

    match conn
        .request(Request::WorkspaceMerge {
            id: fork,
            push: false,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("conflict"),
            "the error should name the conflict, got {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
    // The undo restored main: it still holds the origin's change, not the child's.
    assert!(
        jj_out(&origin, &["file", "show", "-r", "main", "base.txt"]).contains("origin-change"),
        "main should be left at the origin's change after the abort"
    );
    assert!(
        jj_out(
            &origin,
            &[
                "log",
                "-r",
                "::main & conflicts()",
                "--no-graph",
                "--ignore-working-copy",
                "-T",
                "commit_id",
            ],
        )
        .trim()
        .is_empty(),
        "no conflicted commit should be reachable from main"
    );

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&dest);
    server.stop().await;
}

/// Merging a workspace tutti did not fork is refused fail-fast, before any jj
/// call — so this runs even without `jj` installed.
#[tokio::test]
async fn merge_on_a_non_child_errors() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let dir = fresh_dir("plain-merge");
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );
    match conn
        .request(Request::WorkspaceMerge {
            id: ws,
            push: false,
        })
        .await
    {
        Response::Error { message } => assert!(
            message.contains("only a workspace can merge"),
            "expected a non-child refusal, got {message:?}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// A tab keeps its default numeric name only while empty: once it holds a
/// pane, views borrow the active pane's title (and follow a later rename), so
/// tab chips describe content instead of repeating their position.
#[tokio::test]
async fn tab_name_follows_active_pane_title() {
    let server = TestServer::start();
    let mut conn = server.connect().await;

    new_workspace(&mut conn).await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/cat".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );
    assert_eq!(tab_names(&mut conn).await, ["cat"]);

    conn.request(Request::PaneRename {
        pane,
        title: "claude".into(),
    })
    .await;
    assert_eq!(tab_names(&mut conn).await, ["claude"]);

    // A fresh tab has no pane to borrow a title from; it stays numeric.
    conn.request(Request::TabNew { workspace: None }).await;
    assert_eq!(tab_names(&mut conn).await, ["claude", "2"]);

    server.stop().await;
}

async fn tab_names(conn: &mut Conn) -> Vec<String> {
    match conn.request(Request::TabList { workspace: None }).await {
        Response::Tabs { tabs } => tabs.into_iter().map(|t| t.name).collect(),
        other => panic!("expected Tabs, got {other:?}"),
    }
}

/// Poll `pane`'s checkpoints until at least `want` are recorded.
async fn wait_checkpoints(conn: &mut Conn, pane: PaneId, want: usize) -> Vec<Checkpoint> {
    timeout(DEADLINE, async {
        loop {
            if let Response::Checkpoints { checkpoints } =
                conn.request(Request::PaneCheckpoints { pane }).await
                && checkpoints.len() >= want
            {
                return checkpoints;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("pane {pane} never reached {want} checkpoints"))
}

/// Drive `pane` with one hook event, the way `tutti agent-event` does.
async fn hook(conn: &mut Conn, pane: PaneId, event: AgentHookEvent) {
    let request = Request::AgentEvent {
        pane,
        event,
        conversation: None,
    };
    assert_eq!(conn.request(request).await, Response::Ok);
}

/// One agent turn, driven by hook events against a real jj repo: a prompt
/// entering `Working` checkpoints where the agent started, leaving it
/// checkpoints the turn with its stat and that prompt; the turn's diff is
/// served from the two commits; and a
/// restore rewinds the files on disk — refused mid-turn unless forced, first
/// saving the state it replaces as a checkpoint so the restore itself undoes.
/// Skips cleanly without `jj`.
#[tokio::test]
async fn a_turn_checkpoints_its_diff_and_restores_round_trip() {
    if !jj_on_path() {
        eprintln!("skipping: jj is not on PATH");
        return;
    }
    let dir = jj_git_init("ckpt");
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let ws = workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );

    let prompt = Some("edit a and add b".to_string());
    let text = prompt.clone();
    hook(&mut conn, pane, AgentHookEvent::Prompt { text }).await;
    let start = wait_checkpoints(&mut conn, pane, 1).await.remove(0);
    assert_eq!((start.n, start.label.as_str()), (1, "start"));
    assert_eq!(start.from, None, "nothing was recorded before the start");
    assert_eq!(start.prompt, None, "the baseline predates the prompt");

    std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(dir.join("b.txt"), "bee\n").unwrap();
    hook(&mut conn, pane, AgentHookEvent::Done).await;
    let turn = wait_checkpoints(&mut conn, pane, 2).await.remove(1);
    assert_eq!((turn.n, turn.label.as_str()), (2, "done"));
    assert_eq!(turn.prompt, prompt, "the turn lists as its prompt");
    assert_eq!(
        turn.from.as_ref(),
        Some(&start.commit),
        "the turn diffs from the start"
    );
    assert_eq!((turn.files, turn.insertions, turn.deletions), (2, 3, 1));

    match conn
        .request(Request::PaneCheckpointDiff {
            pane,
            n: None,
            stat: false,
        })
        .await
    {
        Response::Content { lines } => {
            let text = lines.join("\n");
            assert!(text.contains("b.txt") && text.contains("TWO"), "{text}");
        }
        other => panic!("expected Content, got {other:?}"),
    }
    match conn
        .request(Request::PaneCheckpointDiff {
            pane,
            n: Some(2),
            stat: true,
        })
        .await
    {
        Response::Content { lines } => assert!(
            lines.iter().any(|l| l.contains("2 files changed")),
            "{lines:?}"
        ),
        other => panic!("expected Content, got {other:?}"),
    }
    assert!(matches!(
        conn.request(Request::PaneCheckpointDiff {
            pane,
            n: Some(1),
            stat: false,
        })
        .await,
        Response::Error { .. }
    ));

    // Mid-turn, a restore is refused unless forced.
    hook(&mut conn, pane, AgentHookEvent::Activity { detail: None }).await;
    match conn
        .request(Request::PaneRestore {
            pane,
            n: 1,
            force: false,
        })
        .await
    {
        Response::Error { message } => assert!(message.contains("working"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    let mut viewer = server.connect().await;
    viewer.send(&Request::Attach).await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));

    // An edit no turn recorded is saved as `before restore` on the way back.
    std::fs::write(dir.join("c.txt"), "unrecorded\n").unwrap();
    assert_eq!(
        conn.request(Request::PaneRestore {
            pane,
            n: 1,
            force: true,
        })
        .await,
        Response::Restored {
            n: 1,
            saved: Some(3)
        }
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(!dir.join("b.txt").exists() && !dir.join("c.txt").exists());
    let saved = wait_checkpoints(&mut conn, pane, 3).await.remove(2);
    assert_eq!(saved.label, "before restore");
    assert_eq!((saved.files, saved.insertions), (1, 1), "just c.txt");
    assert!(
        wait_workspace_changes(&mut viewer, ws, "1 file", DEADLINE).await,
        "the restore refreshes the change stat"
    );

    // Restoring the saved state undoes the restore.
    assert_eq!(
        conn.request(Request::PaneRestore {
            pane,
            n: 3,
            force: true,
        })
        .await,
        Response::Restored { n: 3, saved: None }
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
    assert!(dir.join("b.txt").exists() && dir.join("c.txt").exists());

    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// A turn a program reports itself (OSC 7501) checkpoints like a hook's: its
/// transitions go through the same funnel, and entering `Working` probes too,
/// so the baseline is taken before the turn's edits rather than at its end.
/// Skips cleanly without `jj`.
#[tokio::test]
async fn a_self_reported_turn_checkpoints_its_diff() {
    if !jj_on_path() {
        eprintln!("skipping: jj is not on PATH");
        return;
    }
    let dir = jj_git_init("osc-ckpt");
    let server = TestServer::start();
    let mut conn = server.connect().await;
    conn.request(Request::WorkspaceNew { dir: dir.clone() })
        .await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );

    printf_osc(&mut conn, pane, "7501;state=working:app=build").await;
    let start = wait_checkpoints(&mut conn, pane, 1).await.remove(0);
    assert_eq!(start.label, "start");
    std::fs::write(dir.join("out.txt"), "built\n").unwrap();
    printf_osc(&mut conn, pane, "7501;state=done:app=build").await;
    let turn = wait_checkpoints(&mut conn, pane, 2).await.remove(1);
    assert_eq!(turn.label, "done");
    assert_eq!(turn.from.as_ref(), Some(&start.commit));
    assert_eq!((turn.files, turn.insertions), (1, 1), "just out.txt");

    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}

/// A turn outside a jj workspace records nothing, silently.
#[tokio::test]
async fn checkpoints_stay_empty_outside_jj() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let dir = fresh_dir("nockpt");
    conn.request(Request::WorkspaceNew { dir: dir.clone() })
        .await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );
    hook(&mut conn, pane, AgentHookEvent::Activity { detail: None }).await;
    hook(&mut conn, pane, AgentHookEvent::Done).await;
    sleep(Duration::from_millis(300)).await;
    assert_eq!(
        conn.request(Request::PaneCheckpoints { pane }).await,
        Response::Checkpoints {
            checkpoints: Vec::new()
        }
    );
    assert!(matches!(
        conn.request(Request::PaneRestore {
            pane,
            n: 1,
            force: false,
        })
        .await,
        Response::Error { .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
    server.stop().await;
}
