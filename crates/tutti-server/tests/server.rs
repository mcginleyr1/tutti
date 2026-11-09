//! Drive a real `tutti-server` through a real Unix socket: each test binds a
//! listener on a private temp path, serves it in-process, and talks the wire
//! protocol over `tokio::net::UnixStream`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rtty_vt::Terminal;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use tutti_core::{
    AgentHookEvent, AgentState, ApprovalDecision, Checkpoint, Direction, Event, Frame, HostColors,
    IssueRef, Layout, Note, NoteKind, PaneData, PaneDims, PaneId, PaneInfo, PendingApproval,
    Printed, Request, Response, Rgb, StateSource, SubagentInfo, Tagged, Toward, WIRE_FLOOR,
    WIRE_REV, WatchPane, Wire, WorkspaceId, attached_wire, row_hash,
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
        Self::saving(None)
    }

    /// A server whose layout persistence lives in `state` (`None`: off). Every
    /// test server's session is `s`, so a second one on the same `state` is
    /// the first one restarted.
    fn saving(state: Option<PathBuf>) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tutti-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (shutdown, rx) = oneshot::channel::<()>();
        let served = path.clone();
        let handle = tokio::spawn(async move {
            serve(listener, served, PaneSize::new(24, 80), state, async move {
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
                    last_output: false,
                    last_reply: false,
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
        reply_after: None,
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
    hook(&mut conn, pane, AgentHookEvent::Done { reply: None }).await;
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
        AgentHookEvent::Blocked {
            message: None,
            reminder: false,
        },
    )
    .await;
    assert_eq!(
        waited(waiter.response().await),
        (AgentState::Blocked, false)
    );

    server.stop().await;
}

/// A turn's end keeps its final message for `pane read --last-reply`, kept
/// before the wait that ends with the turn answers, until the next turn
/// ends; no turn yet, or one that ended with none, is an error saying so.
#[tokio::test]
async fn a_turns_reply_is_read_back_once_its_wait_answers() {
    use tutti_core::WaitFor;
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let read = || Request::PaneRead {
        pane,
        lines: None,
        unwrapped: false,
        since: None,
        last_output: false,
        last_reply: true,
    };
    let none = error_message(control.request(read()).await);
    assert!(none.contains("no turn has ended there"), "{none}");

    hook(
        &mut control,
        pane,
        AgentHookEvent::Activity { detail: None },
    )
    .await;
    let mut waiter = server.connect().await;
    let until = [WaitFor::Done, WaitFor::Blocked];
    start_wait(&mut waiter, wait_for(pane, &until, true, None)).await;
    let done = AgentHookEvent::Done {
        reply: Some("pong\nping".into()),
    };
    hook(&mut control, pane, done).await;
    assert_eq!(waited(waiter.response().await), (AgentState::Done, false));
    assert_eq!(
        waiter.request(read()).await,
        Response::LastReply {
            text: "pong\nping".into(),
            truncated: false
        }
    );

    hook(&mut control, pane, AgentHookEvent::Done { reply: None }).await;
    let ended = error_message(control.request(read()).await);
    assert!(ended.contains("ended without a reply"), "{ended}");
    for (lines, unwrapped, since, last_output) in [
        (Some(3), false, None, false),
        (None, true, None, false),
        (None, false, Some(0), false),
        (None, false, None, true),
    ] {
        let shaped = Request::PaneRead {
            pane,
            lines,
            unwrapped,
            since,
            last_output,
            last_reply: true,
        };
        let refused = error_message(control.request(shaped).await);
        assert!(refused.contains("takes no lines, unwrapped"), "{refused}");
    }

    server.stop().await;
}

/// An ask's send: typed only while the pane's agent is between turns.
fn ask_send(pane: PaneId, text: &str) -> Request {
    Request::PaneSend {
        pane,
        text: Some(text.into()),
        keys: None,
        expect: None,
        between_turns: true,
        hooks: None,
    }
}

/// The turn ends the pane's hooks had reported when an ask's send landed.
fn sent(response: Response) -> u64 {
    match response {
        Response::Sent { replies } => replies,
        other => panic!("expected Sent, got {other:?}"),
    }
}

/// An ask's wait: for a block, or a reported turn end past `after`.
fn ask_wait(pane: PaneId, after: u64, timeout_ms: u64) -> Request {
    Request::PaneWait {
        pane,
        until: vec![tutti_core::WaitFor::Blocked],
        next: false,
        timeout_ms: Some(timeout_ms),
        reply_after: Some(after),
    }
}

/// An ask's wait ends on the next turn end the hooks report, counted from
/// its send, and on nothing else that looks like one:
/// - a Stop whose turn start was never reported (Idle → Done, no fresh edge)
///   still ends it, with its reply;
/// - a turn end with no reply (Esc, an interrupt) ends it, and the read says
///   so instead of handing back the turn before's;
/// - one that lands before the wait starts still counts;
/// - a block ends it as blocked.
#[tokio::test]
async fn an_ask_waits_for_the_next_reported_turn_end() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let mut waiter = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let read = || Request::PaneRead {
        pane,
        lines: None,
        unwrapped: false,
        since: None,
        last_output: false,
        last_reply: true,
    };
    let said = |text: &str| AgentHookEvent::Done {
        reply: Some(text.into()),
    };
    hook(&mut control, pane, said("first")).await;
    let focus = Request::PaneFocus { pane, seen: false };
    assert_eq!(control.request(focus).await, Response::Ok);

    let after = sent(control.request(ask_send(pane, "x")).await);
    assert_eq!(after, 1);
    start_wait(&mut waiter, ask_wait(pane, after, 5_000)).await;
    hook(&mut control, pane, said("second")).await;
    assert_eq!(waited(waiter.response().await), (AgentState::Done, false));
    let second = Response::LastReply {
        text: "second".into(),
        truncated: false,
    };
    assert_eq!(control.request(read()).await, second);

    let after = sent(control.request(ask_send(pane, "x")).await);
    start_wait(&mut waiter, ask_wait(pane, after, 5_000)).await;
    hook(&mut control, pane, AgentHookEvent::Done { reply: None }).await;
    assert_eq!(waited(waiter.response().await), (AgentState::Done, false));
    let ended = error_message(control.request(read()).await);
    assert!(ended.contains("ended without a reply"), "{ended}");

    let after = sent(control.request(ask_send(pane, "x")).await);
    hook(&mut control, pane, said("early")).await;
    let early = waiter.request(ask_wait(pane, after, 5_000)).await;
    assert_eq!(waited(early), (AgentState::Done, false), "answered at once");

    let after = sent(control.request(ask_send(pane, "x")).await);
    start_wait(&mut waiter, ask_wait(pane, after, 5_000)).await;
    hook(
        &mut control,
        pane,
        AgentHookEvent::Blocked {
            message: None,
            reminder: false,
        },
    )
    .await;
    assert_eq!(
        waited(waiter.response().await),
        (AgentState::Blocked, false)
    );

    server.stop().await;
}

/// An ask's wait takes a block only from the agent's own word — a hook, or
/// a status report it claims the pane with — never from the screen, which
/// can misread a reply quoting a dialog: neither a pane the screen reads
/// blocked as the wait starts, nor a move into blocked the screen makes,
/// ends it. A hook's block does, the one re-asserting a block the screen
/// already read included.
#[tokio::test]
async fn an_asks_wait_takes_no_block_the_screen_read() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let mut waiter = server.connect().await;
    let pane = claude_cat(&mut control).await;
    let show = |text: &str| shown(pane, text);

    assert_eq!(
        control.request(show("Do you want to proceed?")).await,
        Response::Ok
    );
    assert!(wait_pane(&mut control, pane, screen(AgentState::Blocked)).await);
    let level = waiter.request(ask_wait(pane, 0, 800)).await;
    assert_eq!(waited(level), (AgentState::Blocked, true), "the level");

    for _ in 0..16 {
        assert_eq!(control.request(show("…")).await, Response::Ok);
    }
    assert_eq!(
        control.request(show("esc to interrupt")).await,
        Response::Ok
    );
    assert!(wait_pane(&mut control, pane, screen(AgentState::Working)).await);
    start_wait(&mut waiter, ask_wait(pane, 0, 1_500)).await;
    assert_eq!(
        control.request(show("Do you want to proceed?")).await,
        Response::Ok
    );
    assert!(wait_pane(&mut control, pane, screen(AgentState::Blocked)).await);
    assert_eq!(
        waited(waiter.response().await),
        (AgentState::Blocked, true),
        "the edge"
    );

    start_wait(&mut waiter, ask_wait(pane, 0, 5_000)).await;
    let blocked = AgentHookEvent::Blocked {
        message: None,
        reminder: false,
    };
    hook(&mut control, pane, blocked).await;
    assert_eq!(
        waited(waiter.response().await),
        (AgentState::Blocked, false),
        "the agent's own word"
    );

    server.stop().await;
}

/// Codex 0.166 reports its own blocked over OSC 7501. Such a report on a
/// pane the screen already read blocked moves no state, yet it is the
/// agent's own word: an ask's wait takes it, as it takes a hook's.
#[tokio::test]
async fn an_asks_wait_takes_a_report_claiming_a_block_the_screen_read() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let mut waiter = server.connect().await;
    let pane = claude_cat(&mut control).await;
    let shows = control
        .request(shown(pane, "Do you want to proceed?"))
        .await;
    assert_eq!(shows, Response::Ok);
    assert!(wait_pane(&mut control, pane, screen(AgentState::Blocked)).await);

    start_wait(&mut waiter, ask_wait(pane, 0, 5_000)).await;
    let report = shown(pane, "\u{1b}]7501;state=blocked\u{1b}\\");
    assert_eq!(control.request(report).await, Response::Ok);
    assert_eq!(
        waited(waiter.response().await),
        (AgentState::Blocked, false),
        "the report's own word"
    );
    assert!(
        wait_pane(&mut control, pane, |p| p.source
            == Some(StateSource::Program))
        .await
    );

    server.stop().await;
}

/// A `cat` renamed `claude`: detected as the agent, it draws on its screen
/// whatever is sent to it (`shown`), for the classifier to read.
async fn claude_cat(conn: &mut Conn) -> PaneId {
    let bin = copy_bin("claude", "/bin/cat");
    new_workspace(conn).await;
    let pane = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec![bin.display().to_string()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );
    assert!(wait_agent(conn, pane, "claude").await);
    pane
}

/// The line `text` sent to `pane`, for its program to echo.
fn shown(pane: PaneId, text: &str) -> Request {
    Request::PaneSend {
        pane,
        text: Some(format!("{text}\n")),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    }
}

/// Whether a pane is in `state` as its screen read it.
fn screen(state: AgentState) -> impl Fn(&PaneInfo) -> bool {
    move |p| p.state == state && p.source == Some(StateSource::Screen)
}

/// An agent that exits mid-ask ends the ask's wait with an error naming the
/// exit, though the exit moves the pane to Done and a reply from an earlier
/// turn is there to read. A pane no hook has reported through — a plain
/// shell — or one that is working, blocked or gone refuses the ask's send
/// and types nothing.
#[tokio::test]
async fn an_ask_reports_an_exit_mid_turn_and_refuses_a_busy_pane() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let mut waiter = server.connect().await;
    let enter = |pane| Request::PaneSend {
        pane,
        text: None,
        keys: Some("enter".into()),
        expect: None,
        between_turns: false,
        hooks: None,
    };

    let pane = run_marker(
        &mut control,
        "read a; echo A-$a-; read b; echo B-$b-; read code; exit 3",
    )
    .await;
    let shell = error_message(control.request(ask_send(pane, "typed")).await);
    assert!(
        shell.contains("has not reported through tutti's hooks"),
        "{shell}"
    );
    assert_eq!(control.request(enter(pane)).await, Response::Ok);
    assert!(
        read_until(&mut control, pane, "A--").await,
        "nothing was typed into the shell"
    );

    let earlier = AgentHookEvent::Done {
        reply: Some("earlier".into()),
    };
    hook(&mut control, pane, earlier).await;
    hook(
        &mut control,
        pane,
        AgentHookEvent::Activity { detail: None },
    )
    .await;
    let busy = error_message(control.request(ask_send(pane, "typed")).await);
    assert!(busy.contains("is working: wait for it first"), "{busy}");
    assert_eq!(control.request(enter(pane)).await, Response::Ok);
    assert!(
        read_until(&mut control, pane, "B--").await,
        "nothing was typed into the running turn"
    );
    hook(
        &mut control,
        pane,
        AgentHookEvent::Blocked {
            message: None,
            reminder: false,
        },
    )
    .await;
    let blocked = error_message(control.request(ask_send(pane, "y")).await);
    assert!(blocked.contains("is blocked"), "{blocked}");

    hook(&mut control, pane, AgentHookEvent::Done { reply: None }).await;
    let after = sent(control.request(ask_send(pane, "3")).await);
    start_wait(&mut waiter, ask_wait(pane, after, 5_000)).await;
    hook(
        &mut control,
        pane,
        AgentHookEvent::Activity { detail: None },
    )
    .await;
    assert_eq!(control.request(enter(pane)).await, Response::Ok);
    let exited = error_message(waiter.response().await);
    assert!(exited.contains("exited with code 3"), "{exited}");
    let gone = error_message(control.request(ask_send(pane, "x")).await);
    assert!(gone.contains("has exited (code 3)"), "{gone}");
    let missing = error_message(control.request(ask_send(PaneId(999), "x")).await);
    assert!(missing.contains("no pane 999"), "{missing}");

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
    hook(&mut control, pane, AgentHookEvent::Done { reply: None }).await;
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
    let focus = Request::PaneFocus { pane, seen: false };
    assert_eq!(control.request(focus.clone()).await, Response::Ok);
    hook(
        &mut control,
        pane,
        AgentHookEvent::Activity { detail: None },
    )
    .await;
    hook(&mut control, pane, AgentHookEvent::Done { reply: None }).await;
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
        between_turns: false,
        hooks: None,
    };

    let done = run_marker(&mut control, "read code; exit $code").await;
    hook(&mut control, done, AgentHookEvent::Done { reply: None }).await;
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
        control
            .request(Request::PaneFocus {
                pane: busy,
                seen: false
            })
            .await,
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
    hook(&mut control, pane, AgentHookEvent::Done { reply: None }).await;
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
        from: None,
        issue: None,
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

/// A `run` deploying an issue links its pane to it — what `pane list`, every
/// view and a watcher's snapshot carry — and a plain run links none.
#[tokio::test]
async fn run_links_its_pane_to_the_issue_it_deploys() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let workspace = new_workspace(&mut conn).await;
    let issue = IssueRef {
        source: "gh".into(),
        id: "123".into(),
        title: "Fix the login redirect".into(),
        url: Some("https://github.com/acme/app/issues/123".into()),
    };
    let (_, _, deployed) = ran(conn
        .request(Request::Run {
            workspace: Some(workspace),
            dir: None,
            cmd: vec!["/bin/sleep".into(), "30".into()],
            sandbox: None,
            title: None,
            issue: Some(issue.clone()),
            from: None,
        })
        .await);
    let (_, _, plain) = ran(conn.request(run_sleep(Some(workspace), None, None)).await);
    match conn.request(Request::PaneList).await {
        Response::Panes { panes } => {
            let linked = |pane| panes.iter().find(|p| p.id == pane).unwrap().issue.clone();
            assert_eq!(linked(deployed), Some(Box::new(issue.clone())));
            assert_eq!(linked(plain), None);
        }
        other => panic!("expected Panes, got {other:?}"),
    }
    let mut watcher = server.connect().await;
    watcher.send(&Request::Watch).await;
    let (_, panes) = snapshot_until(&mut watcher, |_, panes| panes.len() == 2).await;
    let linked = |pane| panes.iter().find(|p| p.id == pane).unwrap().issue.clone();
    assert_eq!(linked(deployed), Some(Box::new(issue)));
    assert_eq!(linked(plain), None);

    server.stop().await;
}

/// Whether `file` is gone within the deadline.
async fn vanishes(file: &str) -> bool {
    timeout(DEADLINE, async {
        while Path::new(file).exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// The session's prompt files a pane's argv names — as an argument, as
/// `@<file>`, inside a message — go once its program exits, killed or not;
/// another pane's stay while it runs, a file elsewhere is never touched, and
/// the daemon's stop takes what is left.
#[tokio::test]
async fn a_panes_prompt_files_go_once_its_program_exits() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let workspace = new_workspace(&mut conn).await;
    let prompts = server.path.with_extension("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    let write = |file: PathBuf| {
        std::fs::write(&file, "fix it").unwrap();
        file.to_string_lossy().into_owned()
    };
    let [bare, at, named, running, unclaimed] =
        [1, 2, 3, 4, 5].map(|n| write(prompts.join(format!("prompt-{n}.md"))));
    let elsewhere = write(server.path.with_extension("md"));
    let run = |script: &str, args: Vec<String>| Request::Run {
        workspace: Some(workspace),
        dir: None,
        cmd: [
            vec!["/bin/sh".into(), "-c".into(), script.into(), "sh".into()],
            args,
        ]
        .concat(),
        sandbox: None,
        title: None,
        issue: None,
        from: None,
    };
    let message = format!("Your task is in the file {named}. Read that file.");
    let quick = vec![bare.clone(), format!("@{at}"), message, elsewhere.clone()];
    ran(conn.request(run("exit 0", quick)).await);
    let (_, _, slow) = ran(conn.request(run("sleep 30", vec![running.clone()])).await);

    for file in [&bare, &at, &named] {
        assert!(vanishes(file).await, "{file} outlived its pane");
    }
    for file in [&running, &unclaimed, &elsewhere] {
        assert!(Path::new(file).exists(), "{file}");
    }
    assert_eq!(
        conn.request(Request::PaneKill { pane: slow }).await,
        Response::Ok
    );
    assert!(vanishes(&running).await, "a killed pane's file goes too");
    assert!(Path::new(&unclaimed).exists());

    server.stop().await;
    assert!(!prompts.exists(), "the stop clears the rest");
    assert!(Path::new(&elsewhere).exists(), "never a file of the user's");
    let _ = std::fs::remove_file(&elsewhere);
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
        from: None,
        issue: None,
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
            between_turns: false,
            hooks: None,
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
            between_turns: false,
            hooks: None,
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

/// `pane read --last-output` answers the rows between a command's OSC 133
/// `C` and `D` marks and the exit code `D` carried; a pane whose shell never
/// marked one is refused, naming shell integration.
#[tokio::test]
async fn pane_read_last_output_answers_the_marked_command() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let marked = run_marker(
        &mut conn,
        r"printf 'prompt\n\033]133;C\033\\a\nb\n\033]133;D;3\007done'; sleep 30",
    )
    .await;
    let plain = run_marker(&mut conn, "printf unmarked; sleep 30").await;
    assert!(read_until(&mut conn, marked, "done").await);
    assert!(read_until(&mut conn, plain, "unmarked").await);

    let read = |pane, since| Request::PaneRead {
        pane,
        lines: None,
        unwrapped: false,
        since,
        last_output: true,
        last_reply: false,
    };
    assert_eq!(
        conn.request(read(marked, None)).await,
        Response::LastOutput {
            lines: vec!["a".into(), "b".into()],
            exit: Some(3),
            truncated: false,
        }
    );
    assert_eq!(
        error_message(conn.request(read(plain, None)).await),
        format!("no finished command in pane {plain} (needs shell integration: tutti shell-init)")
    );
    assert!(error_message(conn.request(read(marked, Some(0))).await).contains("not both"));

    server.stop().await;
}

/// A split starts in the directory its source pane's shell last reported
/// (OSC 7, listed as the pane's `cwd`) when that lies inside the workspace;
/// a report outside it, or of a directory that is gone, starts the split in
/// the workspace's own directory.
#[tokio::test]
async fn a_split_starts_where_its_source_panes_shell_is() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let root = fresh_dir("split-cwd");
    let inside = root.join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    workspace_id(
        conn.request(Request::WorkspaceNew { dir: root.clone() })
            .await,
    );
    let source = pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    );

    for (reported, starts) in [
        (inside.clone(), &inside),
        (std::env::temp_dir(), &root),
        (root.join("gone"), &root),
    ] {
        let reported = reported.display().to_string();
        printf_osc(&mut conn, source, &format!("7;file://host{reported}")).await;
        assert!(
            wait_pane(&mut conn, source, |p| p.cwd.as_deref() == Some(&reported)).await,
            "the report of {reported} never listed"
        );
        let split = pane_id(
            conn.request(Request::PaneSplit {
                pane: source,
                direction: Direction::Vertical,
            })
            .await,
        );
        // Ask the split's own shell, whatever `$SHELL` is, where it started.
        let out = root.join(format!("pwd-{split}"));
        let sent = conn
            .request(Request::PaneSend {
                pane: split,
                text: Some(format!("pwd -P > '{}'\n", out.display())),
                keys: None,
                expect: None,
                between_turns: false,
                hooks: None,
            })
            .await;
        assert_eq!(sent, Response::Ok);
        let started = timeout(DEADLINE, async {
            loop {
                match std::fs::read_to_string(&out) {
                    Ok(pwd) if pwd.ends_with('\n') => return pwd.trim_end().to_string(),
                    _ => sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("the split's shell never answered");
        let want = std::fs::canonicalize(starts).unwrap();
        assert_eq!(
            started,
            want.display().to_string(),
            "after reporting {reported}"
        );
    }

    server.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// One `pane read --since`, as `(lines, cursor, truncated)`.
async fn read_since(conn: &mut Conn, pane: PaneId, since: u64) -> (Vec<String>, u64, bool) {
    match conn
        .request(Request::PaneRead {
            pane,
            lines: None,
            unwrapped: false,
            since: Some(since),
            last_output: false,
            last_reply: false,
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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

/// Attach `conn` and return the handshake's view and restore offer.
async fn attach(
    conn: &mut Conn,
) -> (
    Vec<tutti_core::WorkspaceView>,
    Option<tutti_core::RestoreSummary>,
) {
    conn.send(&Request::Attach {
        terminal: None,
        handoff: false,
        view: false,
        quiet: false,
    })
    .await;
    loop {
        if let Frame::Control(json) = conn.read_frame().await
            && let Ok(Response::Attached {
                workspaces,
                restorable,
                ..
            }) = serde_json::from_slice(&json)
        {
            return (workspaces, restorable);
        }
    }
}

/// `layout` with its pane ids blanked: what a restore must reproduce — the
/// same splits at the same ratios — though every pane in it is new.
fn shape(layout: &Layout) -> Layout {
    match layout {
        Layout::Leaf(_) => Layout::Leaf(PaneId(0)),
        Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => Layout::Split {
            direction: *direction,
            ratio: *ratio,
            first: Box::new(shape(first)),
            second: Box::new(shape(second)),
        },
    }
}

/// Each workspace's directory and the shapes of its tabs.
fn shapes(view: &[tutti_core::WorkspaceView]) -> Vec<(PathBuf, Vec<Layout>)> {
    view.iter()
        .map(|w| {
            let tabs = w.tabs.iter().filter_map(|t| t.layout.as_ref());
            (w.dir.clone(), tabs.map(shape).collect())
        })
        .collect()
}

fn sleeper_in(tab: Option<tutti_core::TabId>) -> Request {
    Request::PaneRun {
        tab,
        cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        ephemeral: false,
        sandbox: None,
    }
}

/// A daemon with a state dir saves its layout, and once more as it stops; the
/// next one on the same dir offers it on attach rather than restoring on its
/// own, and `session restore` brings back the workspaces, their tabs, the
/// splits at their exact ratios and a pane per leaf — then the offer is gone.
#[tokio::test]
async fn a_restarted_daemon_offers_its_layout_and_restores_it() {
    let state = fresh_dir("state");
    let (one, two) = (fresh_dir("saved-a"), fresh_dir("saved-b"));
    let server = TestServer::saving(Some(state.clone()));
    let mut conn = server.connect().await;
    let a = workspace_id(
        conn.request(Request::WorkspaceNew { dir: one.clone() })
            .await,
    );
    let first = pane_id(conn.request(sleeper_in(None)).await);
    let split = Request::PaneSplit {
        pane: first,
        direction: Direction::Vertical,
    };
    pane_id(conn.request(split).await);
    let resize = Request::PaneResizeSplit {
        pane: first,
        direction: Direction::Vertical,
        delta: 0.15,
    };
    assert_eq!(conn.request(resize).await, Response::Ok);
    let logs = match conn.request(Request::TabNew { workspace: Some(a) }).await {
        Response::TabCreated { id } => id,
        other => panic!("expected TabCreated, got {other:?}"),
    };
    pane_id(conn.request(sleeper_in(Some(logs))).await);
    workspace_id(
        conn.request(Request::WorkspaceNew { dir: two.clone() })
            .await,
    );
    pane_id(conn.request(sleeper_in(None)).await);
    let (saved, _) = attach(&mut server.connect().await).await;
    server.stop().await;
    assert!(state.join("s.json").exists(), "the stop saved the layout");

    let server = TestServer::saving(Some(state.clone()));
    let (view, offer) = attach(&mut server.connect().await).await;
    assert!(
        view.is_empty(),
        "a restarted daemon never restores on its own"
    );
    let offer = offer.expect("the saved layout is offered");
    assert_eq!((offer.workspaces, offer.panes, offer.agents), (2, 4, 0));
    assert!(state.join("s.restore.json").exists() && !state.join("s.json").exists());

    let mut conn = server.connect().await;
    let restore = Request::SessionRestore { discard: false };
    assert_eq!(
        conn.request(restore.clone()).await,
        Response::SessionRestored {
            workspaces: 2,
            panes: 4,
            revived: 0,
            skipped: Vec::new(),
        }
    );
    assert!(
        !state.join("s.restore.json").exists(),
        "a restore consumes the offer"
    );
    let (restored, offer) = attach(&mut server.connect().await).await;
    assert_eq!(offer, None);
    assert_eq!(shapes(&restored), shapes(&saved));
    assert_eq!(pane_ids(&mut conn).await.len(), 4, "a live pane per leaf");
    assert_eq!(
        error_message(conn.request(restore).await),
        "nothing to restore"
    );

    server.stop().await;
    for dir in [state, one, two] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Discarding the offer deletes it: the next attach offers nothing and a
/// restore finds nothing to bring back.
#[tokio::test]
async fn discarding_the_offered_layout_deletes_it() {
    let state = fresh_dir("state");
    let server = TestServer::saving(Some(state.clone()));
    run_sleeper(&mut server.connect().await).await;
    server.stop().await;

    let server = TestServer::saving(Some(state.clone()));
    let mut conn = server.connect().await;
    assert_eq!(
        conn.request(Request::SessionRestore { discard: true })
            .await,
        Response::SessionRestored {
            workspaces: 0,
            panes: 0,
            revived: 0,
            skipped: Vec::new(),
        }
    );
    assert!(!state.join("s.restore.json").exists());
    assert_eq!(attach(&mut server.connect().await).await.1, None);
    assert_eq!(
        error_message(
            conn.request(Request::SessionRestore { discard: false })
                .await
        ),
        "nothing to restore"
    );
    assert!(workspace_ids(&mut conn).await.is_empty());

    server.stop().await;
    let _ = std::fs::remove_dir_all(state);
}

/// Poll the history until it holds `count` notes; it, then.
async fn notes_landed(conn: &mut Conn, count: usize) -> Vec<Note> {
    timeout(DEADLINE, async {
        loop {
            let notes = list_notes(conn, false).await;
            if notes.len() == count {
                return notes;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the notes never landed")
}

/// The notification history is saved with the layout, and a restarted daemon
/// takes it back at once, before any restore: ids, texts and read marks as
/// they were. Its new panes take ids past every one a note names; a restore
/// points the notes of each pane that came back at the pane it came back as,
/// and a gone pane's note keeps naming a pane nobody is.
#[tokio::test]
async fn a_restarted_daemon_takes_the_notification_history_back() {
    let (state, dir) = (fresh_dir("state"), fresh_dir("noted"));
    let server = TestServer::saving(Some(state.clone()));
    let mut conn = server.connect().await;
    workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );
    let shell = || Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/sh".into()],
        ephemeral: false,
        sandbox: None,
    };
    let kept = pane_id(conn.request(shell()).await);
    let gone = pane_id(conn.request(shell()).await);
    printf_osc(&mut conn, kept, "9;kept").await;
    notes_landed(&mut conn, 1).await;
    printf_osc(&mut conn, gone, "9;gone").await;
    notes_landed(&mut conn, 2).await;
    let read = Request::NotesRead {
        up_to: None,
        pane: Some(kept),
    };
    assert_eq!(conn.request(read).await, Response::Ok);
    let saved = list_notes(&mut conn, false).await;
    assert_eq!(
        conn.request(Request::PaneKill { pane: gone }).await,
        Response::Ok
    );
    server.stop().await;

    let server = TestServer::saving(Some(state.clone()));
    let mut conn = server.connect().await;
    assert_eq!(list_notes(&mut conn, false).await, saved);
    let fresh = run_sleeper(&mut conn).await;
    assert!(
        fresh > kept && fresh > gone,
        "a new pane never takes a noted id"
    );
    assert!(matches!(
        conn.request(Request::SessionRestore { discard: false })
            .await,
        Response::SessionRestored { panes: 1, .. }
    ));
    let panes = pane_ids(&mut conn).await;
    let revived = *panes.iter().find(|p| **p != fresh).unwrap();
    let notes = list_notes(&mut conn, false).await;
    let named: Vec<(Option<&str>, PaneId, bool)> = notes
        .iter()
        .map(|n| (n.body.as_deref(), n.pane, n.read))
        .collect();
    assert_eq!(
        named,
        [(Some("kept"), revived, true), (Some("gone"), gone, false)]
    );
    assert!(!panes.contains(&gone));

    server.stop().await;
    for dir in [state, dir] {
        let _ = std::fs::remove_dir_all(dir);
    }
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    wedged
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;

    // Attaches and will keep draining.
    let mut good = server.connect().await;
    good.send(&Request::Attach {
        terminal: None,
        handoff: false,
        view: false,
        quiet: false,
    })
    .await;
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

/// The answer to the request just sent on `conn`, with the `req` it carries,
/// skipping events and pane frames.
async fn answer(conn: &mut Conn) -> Tagged<Response> {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = conn.read_frame().await
                && let Ok(tagged) = serde_json::from_slice(&json)
            {
                return tagged;
            }
        }
    })
    .await
    .expect("an answer")
}

/// Send `conn`'s visible set: each pane at the size it renders it.
async fn view(conn: &mut Conn, panes: &[(PaneId, u16, u16)]) {
    let panes = panes
        .iter()
        .map(|&(pane, rows, cols)| PaneDims { pane, rows, cols })
        .collect();
    conn.send(&Request::ViewSizes { panes }).await;
    assert_eq!(answer(conn).await.body, Response::Ok);
}

/// Skip frames until `pane`'s snapshot arrives at `rows`x`cols`.
async fn snapshot_at(conn: &mut Conn, pane: PaneId, rows: u16, cols: u16) {
    timeout(DEADLINE, async {
        loop {
            if let Frame::PaneSnapshot(data) = conn.read_frame().await
                && data.pane == pane
                && (data.rows, data.cols) == (rows, cols)
            {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("pane {pane} never came at {rows}x{cols}"));
}

/// Clients attached at different sizes share a pane at the smallest rows and
/// the smallest cols among those viewing it — tmux's smallest client wins —
/// and it grows back as they leave, by detaching or by just going away.
#[tokio::test]
async fn the_smallest_viewer_sizes_a_shared_pane_until_it_leaves() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_marker(&mut control, "sleep 30").await;

    let mut wide = server.connect().await;
    attach(&mut wide).await;
    view(&mut wide, &[(pane, 30, 100)]).await;
    snapshot_at(&mut wide, pane, 30, 100).await;

    let mut short = server.connect().await;
    attach(&mut short).await;
    view(&mut short, &[(pane, 20, 120)]).await;
    snapshot_at(&mut wide, pane, 20, 100).await;
    short.send(&Request::Detach).await;
    assert_eq!(answer(&mut short).await.body, Response::Ok);
    snapshot_at(&mut wide, pane, 30, 100).await;

    let mut gone = server.connect().await;
    attach(&mut gone).await;
    view(&mut gone, &[(pane, 12, 50)]).await;
    snapshot_at(&mut wide, pane, 12, 50).await;
    drop(gone);
    snapshot_at(&mut wide, pane, 30, 100).await;

    server.stop().await;
}

/// A client votes only for the panes it shows: switching tabs, its new set
/// replaces the old one and a pane it left takes the other viewers' size. A
/// pane nobody shows keeps its last.
#[tokio::test]
async fn a_tab_switch_stops_a_client_voting_for_panes_it_left() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let first = run_marker(&mut control, "sleep 30").await;
    let tab = match control.request(Request::TabNew { workspace: None }).await {
        Response::TabCreated { id } => id,
        other => panic!("expected TabCreated, got {other:?}"),
    };
    let second = pane_id(
        control
            .request(Request::PaneRun {
                tab: Some(tab),
                cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                ephemeral: false,
                sandbox: None,
            })
            .await,
    );

    let mut a = server.connect().await;
    attach(&mut a).await;
    view(&mut a, &[(first, 10, 40)]).await;
    let mut b = server.connect().await;
    attach(&mut b).await;
    view(&mut b, &[(first, 20, 60)]).await;
    snapshot_at(&mut b, first, 10, 40).await;

    view(&mut a, &[(second, 10, 40)]).await;
    snapshot_at(&mut b, first, 20, 60).await;
    snapshot_at(&mut a, second, 10, 40).await;

    view(&mut b, &[]).await;
    let mut late = server.connect().await;
    attach(&mut late).await;
    snapshot_at(&mut late, first, 20, 60).await;

    server.stop().await;
}

/// Feed `stream`'s pane frames to `terminal` — a snapshot reseeds it, a raw
/// read goes in as is — until `done` holds for its screen, returning the
/// raw bytes fed and the control frames passed on the way.
async fn stream_until(
    stream: &mut Conn,
    terminal: &mut Terminal,
    done: impl Fn(&Terminal) -> bool,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    let (mut raw, mut controls) = (Vec::new(), Vec::new());
    timeout(DEADLINE, async {
        while !done(terminal) {
            match stream.read_frame().await {
                Frame::PaneSnapshot(data) => {
                    *terminal = Terminal::new(data.rows, data.cols, 0);
                    terminal.advance(&data.bytes);
                }
                Frame::PaneRaw { bytes, .. } => {
                    terminal.advance(&bytes);
                    raw.extend(bytes);
                }
                Frame::Control(json) => controls.push(json),
                other => panic!("a pane stream got {other:?}"),
            }
        }
    })
    .await
    .expect("the stream never got there");
    (raw, controls)
}

fn pane_attach(pane: PaneId, rows: u16, cols: u16) -> Request {
    Request::PaneAttach {
        pane,
        rows,
        cols,
        history: 0,
        inline: false,
        since: None,
        keys: false,
    }
}

/// An inline `pane attach`'s, 10×40, for up to `history` rows past what
/// `since` holds.
fn inline_attach(pane: PaneId, history: u32, since: Option<Printed>) -> Request {
    Request::PaneAttach {
        pane,
        rows: 10,
        cols: 40,
        history,
        inline: true,
        since,
        keys: false,
    }
}

/// A stream that asks for history gets the newest rows of the scrollback
/// first — above the screen as its size vote left it — as lines to print,
/// then the snapshot; a resize's snapshot brings none again.
#[tokio::test]
async fn a_pane_stream_starts_with_the_history_it_asks_for() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_marker(&mut control, "seq 1 100; read stay").await;
    assert!(read_until(&mut control, pane, "100").await);
    let mut stream = server.connect().await;
    stream
        .send(&Request::PaneAttach {
            pane,
            rows: 5,
            cols: 20,
            history: 10,
            inline: false,
            since: None,
            keys: false,
        })
        .await;
    let mut history = Vec::new();
    let snapshot = loop {
        match stream.read_frame().await {
            Frame::PaneRaw { bytes, .. } => history.extend(bytes),
            Frame::PaneSnapshot(data) => break data,
            other => panic!("a pane stream got {other:?}"),
        }
    };
    let want: String = (87..=96).map(|n| format!("{n}\r\n")).collect();
    assert_eq!(String::from_utf8(history).unwrap(), want);
    let mut terminal = Terminal::new(snapshot.rows, snapshot.cols, 0);
    terminal.advance(&snapshot.bytes);
    assert_eq!(terminal.contents(), "97\n98\n99\n100");

    stream
        .send(&Request::ViewSizes {
            panes: vec![PaneDims {
                pane,
                rows: 6,
                cols: 20,
            }],
        })
        .await;
    loop {
        match stream.read_frame().await {
            Frame::PaneSnapshot(data) => {
                assert_eq!((data.rows, data.cols), (6, 20));
                break;
            }
            Frame::Control(_) => {}
            other => panic!("a resize brought {other:?} before its snapshot"),
        }
    }

    server.stop().await;
}

/// A pane stream opens on a snapshot of the screen, then carries the pane's
/// output raw and in order — a terminal fed both is level with the pane —
/// and its `Input` frames type into the pane. A pane once per connection,
/// and only a pane that exists.
#[tokio::test]
async fn a_pane_stream_sends_a_snapshot_then_raw_output_in_order() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_marker(&mut control, "printf 'before\\n'; exec cat").await;
    assert!(read_until(&mut control, pane, "before").await);

    let mut stream = server.connect().await;
    stream.send(&pane_attach(pane, 24, 80)).await;
    let Frame::PaneSnapshot(data) = stream.read_frame().await else {
        panic!("the snapshot comes first");
    };
    assert_eq!((data.pane, data.rows, data.cols), (pane, 24, 80));
    let mut terminal = Terminal::new(24, 80, 0);
    terminal.advance(&data.bytes);
    assert_eq!(terminal.contents(), "before");

    stream
        .write_frame(&Frame::Input {
            pane,
            bytes: b"typed-here\r".to_vec(),
        })
        .await;
    // The pty echoes the line, then `cat` prints it back.
    let (raw, _) = stream_until(&mut stream, &mut terminal, |screen| {
        screen.cursor_position() == (3, 0)
    })
    .await;
    assert_eq!(raw, b"typed-here\r\ntyped-here\r\n");
    let lines = match control
        .request(Request::PaneRead {
            pane,
            lines: None,
            unwrapped: false,
            since: None,
            last_output: false,
            last_reply: false,
        })
        .await
    {
        Response::Content { lines } => lines,
        other => panic!("expected Content, got {other:?}"),
    };
    assert_eq!(terminal.contents(), lines.join("\n"));

    stream.send(&pane_attach(pane, 24, 80)).await;
    let refusal = error_message(stream.response().await);
    assert!(refusal.contains("already streams"), "{refusal}");
    let mut stray = server.connect().await;
    stray.send(&pane_attach(PaneId(999), 24, 80)).await;
    assert_eq!(error_message(stray.response().await), "no pane 999");

    server.stop().await;
}

/// A pane stream votes in its pane's size like an attached client — at
/// attach, then by `ViewSizes` — and the pane grows back once it leaves.
#[tokio::test]
async fn a_pane_stream_votes_in_its_panes_size_until_it_leaves() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_marker(&mut control, "sleep 30").await;
    let mut wide = server.connect().await;
    attach(&mut wide).await;
    view(&mut wide, &[(pane, 30, 100)]).await;
    snapshot_at(&mut wide, pane, 30, 100).await;

    let mut stream = server.connect().await;
    stream.send(&pane_attach(pane, 20, 120)).await;
    snapshot_at(&mut stream, pane, 20, 100).await;
    snapshot_at(&mut wide, pane, 20, 100).await;
    stream
        .send(&Request::ViewSizes {
            panes: vec![PaneDims {
                pane,
                rows: 12,
                cols: 50,
            }],
        })
        .await;
    snapshot_at(&mut stream, pane, 12, 50).await;
    snapshot_at(&mut wide, pane, 12, 50).await;
    drop(stream);
    snapshot_at(&mut wide, pane, 30, 100).await;

    server.stop().await;
}

/// The child exiting ends its stream: the last output, then the exit code,
/// then the daemon closes the connection.
#[tokio::test]
async fn a_pane_stream_ends_with_the_exit_after_the_last_output() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_marker(&mut control, "read line; printf 'bye-%s' \"$line\"; exit 3").await;
    let mut stream = server.connect().await;
    stream.send(&pane_attach(pane, 24, 80)).await;
    let mut terminal = Terminal::new(24, 80, 0);
    stream
        .write_frame(&Frame::Input {
            pane,
            bytes: b"now\r".to_vec(),
        })
        .await;
    let (raw, controls) = stream_until(&mut stream, &mut terminal, |screen| {
        screen.contents().contains("bye-now")
    })
    .await;
    assert!(controls.is_empty(), "nothing before the output");
    assert!(raw.ends_with(b"bye-now"));
    let exit = loop {
        if let Frame::Control(json) = stream.read_frame().await {
            break serde_json::from_slice::<Event>(&json).unwrap();
        }
    };
    assert_eq!(exit, Event::PaneExited { pane, code: 3 });
    let mut rest = [0u8; 64];
    let read = timeout(DEADLINE, stream.stream.read(&mut rest)).await;
    assert_eq!(read.expect("the stream stayed open").unwrap(), 0);

    server.stop().await;
}

/// Read `stream` until `pane`'s exit: its code, and the raw bytes each pane
/// streamed on the way. Another pane's exit on the way fails the test.
async fn stream_to_exit(stream: &mut Conn, pane: PaneId) -> (i32, HashMap<PaneId, Vec<u8>>) {
    let mut raw: HashMap<PaneId, Vec<u8>> = HashMap::new();
    let code = timeout(DEADLINE, async {
        loop {
            match stream.read_frame().await {
                Frame::PaneRaw { pane, bytes } => raw.entry(pane).or_default().extend(bytes),
                Frame::Control(json) => {
                    if let Ok(Event::PaneExited { pane: exited, code }) =
                        serde_json::from_slice(&json)
                    {
                        assert_eq!(exited, pane, "another pane's exit came first");
                        return code;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("pane {pane} never exited"));
    (code, raw)
}

/// One connection streams several panes, each frame naming its pane: the
/// same pane twice is refused, a size it sends votes for each pane it
/// streams, a pane's exit ends that stream alone — the other streams on —
/// and the daemon closes the connection after the last.
#[tokio::test]
async fn a_connection_streams_several_panes_until_the_last_ends() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let first = run_marker(&mut control, "read line; printf 'one-%s' \"$line\"; exit 3").await;
    let second = run_marker(&mut control, "read line; printf 'two-%s' \"$line\"; exit 4").await;
    let mut stream = server.connect().await;
    stream.send(&pane_attach(first, 24, 80)).await;
    snapshot_at(&mut stream, first, 24, 80).await;
    stream.send(&pane_attach(second, 10, 40)).await;
    snapshot_at(&mut stream, second, 10, 40).await;
    stream.send(&pane_attach(first, 24, 80)).await;
    let refusal = error_message(stream.response().await);
    assert_eq!(
        refusal,
        format!("this connection already streams pane {first}")
    );

    // The snapshots at the new sizes go out before the answer.
    let votes = [(first, 20, 70), (second, 12, 50)];
    let panes = votes
        .iter()
        .map(|&(pane, rows, cols)| PaneDims { pane, rows, cols })
        .collect();
    stream.send(&Request::ViewSizes { panes }).await;
    let want = HashSet::from(votes);
    let mut resized = HashSet::new();
    timeout(DEADLINE, async {
        while resized != want {
            if let Frame::PaneSnapshot(data) = stream.read_frame().await {
                resized.insert((data.pane, data.rows, data.cols));
                resized.retain(|seen| want.contains(seen));
            }
        }
    })
    .await
    .expect("a pane streamed was never resized to its vote");

    let input = |pane, bytes: &[u8]| Frame::Input {
        pane,
        bytes: bytes.to_vec(),
    };
    stream.write_frame(&input(second, b"now\r")).await;
    let (code, raw) = stream_to_exit(&mut stream, second).await;
    assert_eq!(code, 4);
    assert!(raw[&second].ends_with(b"two-now"), "{raw:?}");
    stream.write_frame(&input(first, b"later\r")).await;
    let (code, raw) = stream_to_exit(&mut stream, first).await;
    assert_eq!(code, 3);
    assert!(raw[&first].ends_with(b"one-later"), "{raw:?}");
    let mut rest = [0u8; 64];
    let read = timeout(DEADLINE, stream.stream.read(&mut rest)).await;
    assert_eq!(read.expect("the connection stayed open").unwrap(), 0);

    server.stop().await;
}

/// A connection ends one of its streams with `PaneDetach`: its vote goes —
/// the pane grows back to the other viewer's size — the connection's other
/// stream goes on, and the pane may be streamed again, its exit then told
/// once, by the new stream alone. A pane it does not stream is refused.
#[tokio::test]
async fn a_connection_detaches_one_stream_and_may_stream_it_again() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let first = run_marker(&mut control, "read line; printf 'one-%s' \"$line\"; exit 3").await;
    let second = run_marker(&mut control, "read line; printf 'two-%s' \"$line\"; exit 4").await;
    let mut wide = server.connect().await;
    attach(&mut wide).await;
    view(&mut wide, &[(first, 30, 100)]).await;
    snapshot_at(&mut wide, first, 30, 100).await;
    let mut stream = server.connect().await;
    stream.send(&pane_attach(first, 20, 120)).await;
    snapshot_at(&mut stream, first, 20, 100).await;
    stream.send(&pane_attach(second, 10, 40)).await;
    snapshot_at(&mut stream, second, 10, 40).await;
    snapshot_at(&mut wide, first, 20, 100).await;

    let detach = Request::PaneDetach { pane: first };
    assert_eq!(stream.request(detach.clone()).await, Response::Ok);
    snapshot_at(&mut wide, first, 30, 100).await;
    let refusal = error_message(stream.request(detach).await);
    assert_eq!(refusal, format!("this connection streams no pane {first}"));

    stream.send(&pane_attach(first, 20, 120)).await;
    snapshot_at(&mut stream, first, 20, 100).await;
    let input = |pane, bytes: &[u8]| Frame::Input {
        pane,
        bytes: bytes.to_vec(),
    };
    stream.write_frame(&input(first, b"now\r")).await;
    let (code, raw) = stream_to_exit(&mut stream, first).await;
    assert_eq!(code, 3);
    assert!(raw[&first].ends_with(b"one-now"), "{raw:?}");
    stream.write_frame(&input(second, b"later\r")).await;
    let (code, raw) = stream_to_exit(&mut stream, second).await;
    assert_eq!(code, 4);
    assert!(raw[&second].ends_with(b"two-later"), "{raw:?}");
    let mut rest = [0u8; 64];
    let read = timeout(DEADLINE, stream.stream.read(&mut rest)).await;
    assert_eq!(read.expect("the connection stayed open").unwrap(), 0);

    server.stop().await;
}

/// rtty's window on one connection: a quiet view that streams its pane with
/// the key encodings. Its stream carries the kitty flags the pane's program
/// pushes and pops, which a plain stream of the same pane never sees; the
/// pane's exit ends the stream but not the connection, which is still told
/// of the session.
#[tokio::test]
async fn an_attached_connection_keeps_the_keys_and_outlives_its_streams() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let script = "read go; printf 'one\\033[>1utwo\\033[<u'; read line; exit 0";
    let pane = run_marker(&mut control, script).await;
    let mut window = server.connect().await;
    window
        .send(&Request::Attach {
            terminal: Some("local.rtty".into()),
            handoff: true,
            view: true,
            quiet: true,
        })
        .await;
    read_until_control(&mut window, |r: &Response| {
        matches!(r, Response::Attached { .. })
    })
    .await;
    window
        .send(&Request::PaneAttach {
            pane,
            rows: 24,
            cols: 80,
            history: 0,
            inline: false,
            since: None,
            keys: true,
        })
        .await;
    snapshot_at(&mut window, pane, 24, 80).await;
    let mut plain = server.connect().await;
    plain.send(&pane_attach(pane, 24, 80)).await;
    snapshot_at(&mut plain, pane, 24, 80).await;

    let input = |bytes: &[u8]| Frame::Input {
        pane,
        bytes: bytes.to_vec(),
    };
    window.write_frame(&input(b"go\rbye\r")).await;
    let (code, raw) = stream_to_exit(&mut window, pane).await;
    assert_eq!(code, 0);
    let keyed = String::from_utf8_lossy(&raw[&pane]).into_owned();
    assert!(keyed.contains("one\x1b[>1utwo\x1b[<u"), "{keyed:?}");
    let (_, raw) = stream_to_exit(&mut plain, pane).await;
    let unkeyed = String::from_utf8_lossy(&raw[&pane]).into_owned();
    assert!(unkeyed.contains("onetwo"), "{unkeyed:?}");

    assert_eq!(
        control.request(Request::PaneKill { pane }).await,
        Response::Ok
    );
    let layout = read_until_control(&mut window, |e: &Event| {
        assert!(
            !matches!(e, Event::PaneExited { .. }),
            "the exit was told twice"
        );
        matches!(e, Event::LayoutChanged { .. })
    })
    .await;
    assert!(matches!(layout, Event::LayoutChanged { .. }));

    server.stop().await;
}

/// A quiet client is sent no pane frame — no snapshot, no delta — across
/// the output a plain client beside it is sent both for; events reach both.
#[tokio::test]
async fn a_quiet_client_is_sent_no_pane_frames() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_marker(&mut control, "exec cat").await;
    let mut quiet = server.connect().await;
    quiet
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: true,
        })
        .await;
    let mut plain = server.connect().await;
    attach(&mut plain).await;
    expect_pane_frame(&mut plain, pane, true).await;
    control
        .write_frame(&Frame::Input {
            pane,
            bytes: b"typed\r".to_vec(),
        })
        .await;
    expect_pane_frame(&mut plain, pane, false).await;

    // What the ticks so far sent the quiet client came before this split's
    // layout.
    let split = Request::PaneSplit {
        pane,
        direction: Direction::Vertical,
    };
    let new = pane_id(control.request(split).await);
    let holds_new = |e: &Event| match e {
        Event::LayoutChanged { workspaces } => workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .any(|p| p.id == new),
        _ => false,
    };
    timeout(DEADLINE, async {
        loop {
            match quiet.read_frame().await {
                Frame::Control(json) => {
                    if serde_json::from_slice(&json).is_ok_and(|e| holds_new(&e)) {
                        return;
                    }
                }
                other => panic!("a quiet client got {other:?}"),
            }
        }
    })
    .await
    .expect("the quiet client never heard of the split");
    read_until_control(&mut plain, holds_new).await;

    server.stop().await;
}

/// The daemon answers a pane's device and cursor queries itself — nothing
/// else would: a TUI never sees them — a cursor report where the cursor
/// stands. A pane stream never carries them, or its
/// own terminal would answer them again.
#[tokio::test]
async fn a_panes_terminal_queries_are_answered_and_kept_from_its_streams() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let script = "stty -echo -icanon; echo ready; read -r go; \
                  printf 'ab\\033[c\\033[>c\\033[5n\\033[6n'; head -c 30 | tr '\\033' E; sleep 30";
    let pane = run_marker(&mut control, script).await;
    assert!(read_until(&mut control, pane, "ready").await);
    let mut stream = server.connect().await;
    stream.send(&pane_attach(pane, 24, 80)).await;
    stream
        .write_frame(&Frame::Input {
            pane,
            bytes: b"go\r".to_vec(),
        })
        .await;
    let answered = "abE[?62;22cE[>1;277;0cE[0nE[2;3R";
    let mut terminal = Terminal::new(24, 80, 0);
    let (raw, _) = stream_until(&mut stream, &mut terminal, |screen| {
        screen.contents().contains(answered)
    })
    .await;
    assert!(String::from_utf8_lossy(&raw).contains(answered), "{raw:?}");
    assert!(!raw.windows(2).any(|w| w == b"\x1b["), "{raw:?}");
    assert!(read_until(&mut control, pane, answered).await);

    server.stop().await;
}

/// Solarized Light — or, with `dark`, the same colours called dark.
fn host_colors(dark: bool) -> Request {
    let rgb = |r, g, b| Rgb { r, g, b };
    Request::HostColors(HostColors {
        fg: rgb(0x6565, 0x7b7b, 0x8383),
        bg: rgb(0xfdfd, 0xf6f6, 0xe3e3),
        cursor: None,
        dark,
    })
}

/// A colour query goes on to a pane's stream while no client has reported
/// its terminal's colours — the daemon invents none — and once one has, the
/// daemon answers it, closed as it was asked, and the stream never sees it.
#[tokio::test]
async fn a_colour_query_is_answered_once_a_client_reports_colours() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let script = "stty -echo -icanon; echo ready; read -r a; printf 'one\\033]11;?\\007'; \
                  read -r b; printf 'two\\033]11;?\\033\\\\'; head -c 25 | tr '\\033' E; sleep 30";
    let pane = run_marker(&mut control, script).await;
    assert!(read_until(&mut control, pane, "ready").await);
    let mut stream = server.connect().await;
    stream.send(&pane_attach(pane, 24, 80)).await;
    let go = Frame::Input {
        pane,
        bytes: b"go\r".to_vec(),
    };
    let mut terminal = Terminal::new(24, 80, 0);
    stream.write_frame(&go).await;
    let (raw, _) = stream_until(&mut stream, &mut terminal, |s| s.contents().contains("one")).await;
    assert!(
        raw.ends_with(b"one\x1b]11;?\x07"),
        "passed on unanswered: {raw:?}"
    );

    assert_eq!(control.request(host_colors(false)).await, Response::Ok);
    stream.write_frame(&go).await;
    let answer = "E]11;rgb:fdfd/f6f6/e3e3E\\";
    let (raw, _) = stream_until(&mut stream, &mut terminal, |s| {
        s.contents().contains(answer)
    })
    .await;
    let raw = String::from_utf8_lossy(&raw);
    assert!(raw.starts_with("two") && !raw.contains("]11;?"), "{raw:?}");

    server.stop().await;
}

/// A pane that set mode 2031 hears each light/dark change — the first
/// report counts as one — and a report that keeps it tells nothing; one
/// that reset the mode hears none, and nor does one whose shell marked the
/// end of the command that set it (OSC 133;D), as after nvim was killed
/// with SIGKILL: the report would reach the shell as typing.
#[tokio::test]
async fn a_2031_pane_hears_of_a_light_dark_flip() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let script = |modes: &str| {
        format!(
            "stty -echo; printf '{modes}'; echo ready; IFS= read -r go; \
             printf '[%s]' \"$go\" | tr '\\033' E; sleep 30"
        )
    };
    let on = run_marker(&mut control, &script("\\033[?2031h")).await;
    let off = run_marker(&mut control, &script("\\033[?2031h\\033[?2031l")).await;
    let ended = run_marker(&mut control, &script("\\033[?2031h\\033]133;D;137\\007")).await;
    for pane in [on, off, ended] {
        assert!(read_until(&mut control, pane, "ready").await);
    }
    for dark in [true, false, false] {
        assert_eq!(control.request(host_colors(dark)).await, Response::Ok);
    }
    for pane in [on, off, ended] {
        let go = Request::PaneSend {
            pane,
            text: Some("go\r".into()),
            keys: None,
            expect: None,
            between_turns: false,
            hooks: None,
        };
        assert_eq!(control.request(go).await, Response::Ok);
    }
    assert!(read_until(&mut control, on, "[E[?997;1nE[?997;2ngo]").await);
    assert!(read_until(&mut control, off, "[go]").await);
    assert!(read_until(&mut control, ended, "[go]").await);

    server.stop().await;
}

/// A read that switches screens reaches a pane stream with the switch in a
/// frame of its own, so `pane attach` — which redraws a switching frame from
/// its mirror — loses nothing else the read held: a bell, a cursor shape.
/// The queries a program sends as it switches (nvim's DECRQM, DECRQSS and
/// kitty's keyboard query, then DA1) are answered by the daemon and never
/// reach the stream.
#[tokio::test]
async fn a_screen_switching_read_loses_nothing_on_a_stream() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let script = "stty -echo -icanon; echo ready; read -r go; \
                  printf 'before\\007\\033[?1049h\\033[?2026$p\\033P$qm\\033\\\\\\033[?u\\033[c\\007\\033[6 qafter'; \
                  head -c 34 | tr '\\033' E; sleep 30";
    let pane = run_marker(&mut control, script).await;
    assert!(read_until(&mut control, pane, "ready").await);
    let mut stream = server.connect().await;
    stream.send(&pane_attach(pane, 24, 80)).await;
    stream
        .write_frame(&Frame::Input {
            pane,
            bytes: b"go\r".to_vec(),
        })
        .await;
    let mut frames: Vec<Vec<u8>> = Vec::new();
    timeout(DEADLINE, async {
        while !frames.concat().windows(5).any(|bytes| bytes == b"after") {
            if let Frame::PaneRaw { bytes, .. } = stream.read_frame().await {
                frames.push(bytes);
            }
        }
    })
    .await
    .expect("the stream never got there");
    let switch = b"\x1b[?1049h".as_slice();
    for frame in &frames {
        let holds = |bytes: &[u8]| frame.windows(bytes.len()).any(|seen| seen == bytes);
        assert!(!holds(switch) || frame == switch, "{frame:?}");
        for query in [b"$p".as_slice(), b"$q", b"\x1b[?u", b"\x1b[c"] {
            assert!(!holds(query), "{query:?} reached the stream: {frame:?}");
        }
    }
    let after: Vec<u8> = frames
        .iter()
        .skip_while(|frame| frame.as_slice() != switch)
        .skip(1)
        .flatten()
        .copied()
        .collect();
    assert_eq!(after, b"\x07\x1b[6 qafter", "{frames:?}");
    assert!(read_until(&mut control, pane, "E[?2026;2$yEP1$r0mE\\E[?0uE[?62;22c").await);

    server.stop().await;
}

/// A stream that stops reading never stalls its pane or anyone else: once
/// it is a backlog behind, what is queued for it is dropped for a fresh
/// snapshot. Read at last, it skips ahead to the pane's screen, and streams
/// on from there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_stream_that_stops_reading_skips_ahead_to_the_screen() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    // 6 MB of output: six times the backlog, and the socket's few KiB on
    // top — and no more, so a loaded machine still writes it inside the
    // deadline.
    let script = "stty -echo; read go; head -c 6000000 /dev/zero | tr '\\0' x; echo; \
                  echo flood-$((6*7)); read more; echo still-$((1+1)); sleep 30";
    let pane = run_marker(&mut control, script).await;
    let mut stuck = server.connect().await;
    stuck.send(&pane_attach(pane, 10, 40)).await;
    stuck
        .write_frame(&Frame::Input {
            pane,
            bytes: b"go\r".to_vec(),
        })
        .await;
    assert!(read_until(&mut control, pane, "flood-42").await);
    // Full rows of `x`, eight of them on screen, then the marker.
    let screen = format!("{}\nflood-42", "x".repeat(8 * 40));

    let (mut terminal, mut snapshots, mut raw) = (Terminal::new(10, 40, 0), 0, 0);
    timeout(DEADLINE, async {
        while terminal.contents() != screen {
            match stuck.read_frame().await {
                Frame::PaneSnapshot(data) => {
                    terminal = Terminal::new(data.rows, data.cols, 0);
                    terminal.advance(&data.bytes);
                    snapshots += 1;
                }
                Frame::PaneRaw { bytes, .. } => {
                    terminal.advance(&bytes);
                    raw += bytes.len();
                }
                other => panic!("a pane stream got {other:?}"),
            }
        }
    })
    .await
    .expect("the stream never reached the pane's screen");
    assert!(snapshots > 1, "it was resynced");
    assert!(raw < 3_000_000, "it skipped the backlog: {raw} bytes");

    stuck
        .write_frame(&Frame::Input {
            pane,
            bytes: b"more\r".to_vec(),
        })
        .await;
    stream_until(&mut stuck, &mut terminal, |screen| {
        screen.contents().ends_with("flood-42\nstill-2")
    })
    .await;

    server.stop().await;
}

/// The rows `terminal` has scrolled into its scrollback, as text.
fn scrolled(terminal: &Terminal) -> Vec<String> {
    terminal
        .rows(terminal.oldest_row(), terminal.screen_top())
        .map(|(_, row)| row.text().trim_end().to_string())
        .collect()
}

/// An inline stream that falls behind is handed, before each fresh
/// snapshot, the rows that scrolled off the screen past what it last had —
/// numbered by the `PaneRows` before them — so its terminal's scrollback,
/// what scrolled in it then each resync's rows, misses no line and repeats
/// none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inline_stream_behind_is_handed_the_rows_it_skipped() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    // 3000 lines of a row each, padded with resets to 900 bytes: 2.7 MB,
    // over twice the backlog, in fewer rows than a scrollback holds.
    let script = "stty -echo; read go; awk 'BEGIN { for (j = 0; j < 300; j++) f = f \"\\033[m\"; \
                  for (i = 1; i <= 3000; i++) printf \"%s%d\\r\\n\", f, i }'; echo flood-done; sleep 30";
    let pane = run_marker(&mut control, script).await;
    let mut stuck = server.connect().await;
    stuck.send(&inline_attach(pane, 0, None)).await;
    stuck
        .write_frame(&Frame::Input {
            pane,
            bytes: b"go\r".to_vec(),
        })
        .await;
    assert!(read_until(&mut control, pane, "flood-done").await);

    let (transcript, numbered) = inline_transcript(&mut stuck, "flood-done").await;
    assert!(numbered.len() > 1, "it was resynced: {numbered:?}");
    assert!(
        numbered.iter().all(|&(skipped, _)| skipped == 0),
        "{numbered:?}"
    );
    let numbers: Vec<u32> = transcript.iter().filter_map(|l| l.parse().ok()).collect();
    assert_eq!(numbers, (1..=3000).collect::<Vec<_>>());

    server.stop().await;
}

/// What an inline stream's terminal would hold once its screen shows
/// `done`: what scrolled in it, each resync's rows where they came, then
/// the screen — and, for each `PaneRows`, its `skipped` and how many rows
/// came after it.
async fn inline_transcript(stream: &mut Conn, done: &str) -> (Vec<String>, Vec<(u64, usize)>) {
    let (mut terminal, mut transcript) = (Terminal::new(10, 40, 100_000), Vec::new());
    let (mut numbered, mut past) = (Vec::new(), None::<(u64, Vec<u8>)>);
    timeout(DEADLINE, async {
        while !terminal.contents().contains(done) {
            match stream.read_frame().await {
                Frame::Control(json) => {
                    if let Ok(Event::PaneRows { skipped, .. }) = serde_json::from_slice(&json) {
                        transcript.extend(scrolled(&terminal));
                        past = Some((skipped, Vec::new()));
                    }
                }
                Frame::PaneRaw { bytes, .. } => match &mut past {
                    Some((_, past)) => past.extend(bytes),
                    None => terminal.advance(&bytes),
                },
                Frame::PaneSnapshot(data) => {
                    let (skipped, past) = past.take().expect("numbered");
                    let past = String::from_utf8(past).unwrap();
                    numbered.push((skipped, past.lines().count()));
                    transcript.extend(past.lines().map(String::from));
                    terminal = Terminal::new(data.rows, data.cols, 100_000);
                    terminal.advance(&data.bytes);
                }
                other => panic!("a pane stream got {other:?}"),
            }
        }
    })
    .await
    .expect("the stream never caught up");
    transcript.extend(scrolled(&terminal));
    transcript.extend(terminal.contents().lines().map(String::from));
    (transcript, numbered)
}

/// A stream behind by more rows than a resync hands on gets the newest
/// `RESYNC_ROWS` (10 000) of them — a pane's whole scrollback — and
/// `skipped` counts exactly the rows its terminal never gets: no row comes
/// twice, and the ones missing are as many as were said skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resync_hands_on_at_most_a_scrollback_and_counts_the_rest() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    // 20 000 lines of a row each, padded with resets to 120 bytes: 2.4 MB,
    // over twice the backlog, in twice the rows a scrollback holds.
    let script = "stty -echo; read go; awk 'BEGIN { for (j = 0; j < 40; j++) f = f \"\\033[m\"; \
                  for (i = 1; i <= 20000; i++) printf \"%s%d\\r\\n\", f, i }'; echo flood-done; sleep 30";
    let pane = run_marker(&mut control, script).await;
    let mut stuck = server.connect().await;
    stuck.send(&inline_attach(pane, 0, None)).await;
    stuck
        .write_frame(&Frame::Input {
            pane,
            bytes: b"go\r".to_vec(),
        })
        .await;
    assert!(read_until(&mut control, pane, "flood-done").await);

    let (transcript, numbered) = inline_transcript(&mut stuck, "flood-done").await;
    let skipped: u64 = numbered.iter().map(|&(skipped, _)| skipped).sum();
    assert!(
        skipped > 0,
        "rows past a scrollback were skipped: {numbered:?}"
    );
    assert!(
        numbered.iter().all(|&(_, rows)| rows <= 10_000),
        "{numbered:?}"
    );
    let numbers: Vec<u64> = transcript.iter().filter_map(|l| l.parse().ok()).collect();
    assert!(
        numbers.windows(2).all(|w| w[0] < w[1]),
        "no row twice, none out of order"
    );
    assert_eq!(20_000 - numbers.len() as u64, skipped, "{numbered:?}");

    server.stop().await;
}

/// How an inline stream opens: its `PaneRows`, the history lines before
/// its snapshot, and what a terminal seeded by the snapshot holds of it
/// once it leaves — the screen's rows down to its last text or cursor.
async fn inline_opening(stream: &mut Conn) -> (Event, Vec<String>, Printed) {
    let rows = loop {
        if let Frame::Control(json) = stream.read_frame().await
            && let Ok(rows @ Event::PaneRows { .. }) = serde_json::from_slice(&json)
        {
            break rows;
        }
    };
    let mut history = Vec::new();
    let snapshot = loop {
        match stream.read_frame().await {
            Frame::PaneRaw { bytes, .. } => history.extend(bytes),
            Frame::PaneSnapshot(data) => break data,
            other => panic!("a pane stream got {other:?} before its snapshot"),
        }
    };
    let mut terminal = Terminal::new(snapshot.rows, snapshot.cols, 0);
    terminal.advance(&snapshot.bytes);
    let last = (0..snapshot.rows)
        .rev()
        .find(|&row| !terminal.screen_row(row).text().trim_end().is_empty())
        .unwrap_or(0)
        .max(terminal.cursor_position().0);
    let Event::PaneRows { epoch, top, .. } = rows else {
        unreachable!()
    };
    let held = (0..=last)
        .map(|row| row_hash(&terminal.screen_row(row).text()))
        .collect();
    let history = String::from_utf8(history).unwrap();
    let history = history.lines().map(String::from).collect();
    (
        rows,
        history,
        Printed {
            epoch,
            row: top,
            held,
        },
    )
}

/// A second inline stream into the terminal the first left starts its
/// history past what that terminal holds — a row it holds that has changed
/// since (the prompt typed on) and every row after come again; a record
/// from another daemon run counts for nothing; once the pane cleared its
/// scrollback the history starts where the pane's does, the rows between
/// counted as skipped, and so are those past `--history`.
#[tokio::test]
async fn a_second_inline_stream_starts_past_what_its_terminal_holds() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let script = "stty -echo; seq 1 30; printf '$ '; read a; printf '%s\\n' \"$a\"; seq 31 45; \
                  read b; printf '\\033[3J'; seq 46 50; sleep 30";
    let pane = run_marker(&mut control, script).await;
    assert!(read_until(&mut control, pane, "$").await);
    let mut first = server.connect().await;
    first.send(&inline_attach(pane, 1000, None)).await;
    let (rows, history, printed) = inline_opening(&mut first).await;
    let Event::PaneRows {
        epoch,
        top,
        skipped,
        ..
    } = rows
    else {
        unreachable!()
    };
    assert_eq!((top, skipped), (21, 0));
    assert_eq!(history, (1..=21).map(|n| n.to_string()).collect::<Vec<_>>());
    assert_eq!(printed.held.len(), 10, "down to the prompt");
    drop(first);

    let send = |text: &str| Request::PaneSend {
        pane,
        text: Some(text.into()),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    assert_eq!(control.request(send("ls\r")).await, Response::Ok);
    assert!(read_until(&mut control, pane, "45").await);
    let numbers = |range: std::ops::RangeInclusive<u32>| -> Vec<String> {
        range.map(|n| n.to_string()).collect()
    };
    let served = &server;
    let opening = move |since| async move {
        let mut stream = served.connect().await;
        stream.send(&inline_attach(pane, 1000, since)).await;
        let (rows, history, _) = inline_opening(&mut stream).await;
        let Event::PaneRows { epoch, skipped, .. } = rows else {
            unreachable!()
        };
        (epoch, skipped, history)
    };
    let (again, skipped, history) = opening(Some(printed.clone())).await;
    assert_eq!((again, skipped), (epoch, 0), "the same run");
    let mut want = vec!["$ ls".to_string()];
    want.extend(numbers(31..=36));
    assert_eq!(history, want, "from the changed prompt on");

    let other = Printed {
        epoch: epoch + 1,
        ..printed.clone()
    };
    let (_, skipped, history) = opening(Some(other)).await;
    assert_eq!(
        (skipped, history.len()),
        (0, 37),
        "another run's counts for nothing"
    );

    assert_eq!(control.request(send("go\r")).await, Response::Ok);
    assert!(read_until(&mut control, pane, "50").await);
    let (_, skipped, history) = opening(Some(printed.clone())).await;
    assert_eq!((skipped, history), (16, numbers(37..=41)), "cleared");
    let mut capped = server.connect().await;
    capped.send(&inline_attach(pane, 3, Some(printed))).await;
    let (rows, history, _) = inline_opening(&mut capped).await;
    assert!(
        matches!(rows, Event::PaneRows { skipped: 18, .. }),
        "{rows:?}"
    );
    assert_eq!(history, numbers(39..=41), "past --history");

    server.stop().await;
}

/// A pane whose program stops reading its input stalls nobody: each send to
/// it answers at once — queued until the queue is full, refused after —
/// and all the while the session lists, and another pane takes input and is
/// read, promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_that_stops_reading_its_input_stalls_nobody() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    // Raw, as an agent's terminal is: a cooked line discipline drops what
    // overflows its line, where a raw one blocks the write.
    let deaf = run_marker(&mut control, "stty raw -echo; echo deaf; exec sleep 600").await;
    assert!(read_until(&mut control, deaf, "deaf").await);
    let beside = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/cat".into()],
                ephemeral: false,
                sandbox: None,
            })
            .await,
    );
    let send = |pane, text: String| Request::PaneSend {
        pane,
        text: Some(text),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    let prompt = Duration::from_secs(2);

    // The first send overflows the pty's input buffer (1 KiB on macOS), so
    // the writer blocks on it; the queue's 64 then fill behind it.
    let mut queued = 0;
    let refused = loop {
        let text = if queued == 0 {
            "x".repeat(64 * 1024)
        } else {
            "x".into()
        };
        let answer = timeout(prompt, control.request(send(deaf, text)))
            .await
            .expect("a send to a pane that stopped reading answers promptly");
        match answer {
            Response::Ok => queued += 1,
            Response::Error { message } => break message,
            other => panic!("expected Ok or Error, got {other:?}"),
        }
        assert!(queued <= 65, "65 sends queued and none refused");
    };
    assert!(queued >= 64, "refused after {queued} sends");
    assert!(refused.contains("not reading its input"), "{refused}");

    for _ in 0..3 {
        let listed = timeout(prompt, control.request(Request::PaneList))
            .await
            .expect("pane list answers promptly");
        assert!(matches!(listed, Response::Panes { .. }), "{listed:?}");
    }
    let typed = timeout(prompt, control.request(send(beside, "beside\r".into())))
        .await
        .expect("another pane's send answers promptly");
    assert_eq!(typed, Response::Ok);
    let read = Request::PaneRead {
        pane: beside,
        lines: None,
        unwrapped: false,
        since: None,
        last_output: false,
        last_reply: false,
    };
    let screen = timeout(prompt, control.request(read))
        .await
        .expect("another pane's read answers promptly");
    assert!(matches!(screen, Response::Content { .. }), "{screen:?}");
    assert!(read_until(&mut control, beside, "beside").await);

    server.stop().await;
}

/// A send to a pane whose program has exited is refused, saying so.
#[tokio::test]
async fn a_send_to_an_exited_pane_is_refused() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_marker(&mut control, "exit 3").await;
    timeout(DEADLINE, async {
        loop {
            let Response::Panes { panes } = control.request(Request::PaneList).await else {
                panic!("expected the pane list");
            };
            if panes.iter().any(|p| p.id == pane && p.exited.is_some()) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the pane never exited");
    let refused = error_message(
        control
            .request(Request::PaneSend {
                pane,
                text: Some("late\r".into()),
                keys: None,
                expect: None,
                between_turns: false,
                hooks: None,
            })
            .await,
    );
    assert!(refused.contains("has exited (code 3)"), "{refused}");

    server.stop().await;
}

/// A request may carry a `req`, and its answer carries it back: a sync answer,
/// one sent later from a spawned task, even the refusal of a request that does
/// not parse. An untagged request's answer carries none, nor does an event.
#[tokio::test]
async fn a_requests_req_rides_back_on_its_answer() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_marker(&mut conn, "sleep 30").await;
    let tagged = |req, body| Frame::Control(serde_json::to_vec(&Tagged { req, body }).unwrap());

    conn.write_frame(&tagged(Some(7), Request::WorkspaceList))
        .await;
    let listed = answer(&mut conn).await;
    assert_eq!(listed.req, Some(7));
    assert!(matches!(listed.body, Response::Workspaces { .. }));

    let wait = wait_for(pane, &[tutti_core::WaitFor::Blocked], false, Some(20));
    conn.write_frame(&tagged(Some(8), wait)).await;
    let waited = answer(&mut conn).await;
    assert_eq!(waited.req, Some(8), "an async answer");
    assert!(matches!(
        waited.body,
        Response::Waited {
            timed_out: true,
            ..
        }
    ));

    conn.write_frame(&Frame::Control(
        br#"{"type":"no_such_request","req":9}"#.to_vec(),
    ))
    .await;
    let refused = answer(&mut conn).await;
    assert_eq!(
        refused.req,
        Some(9),
        "a refusal of a request that does not parse"
    );
    assert!(matches!(refused.body, Response::Error { .. }));

    conn.send(&Request::PaneList).await;
    assert_eq!(answer(&mut conn).await.req, None);

    let mut viewer = server.connect().await;
    viewer
        .write_frame(&tagged(
            Some(10),
            Request::Attach {
                terminal: None,
                handoff: false,
                view: false,
                quiet: false,
            },
        ))
        .await;
    assert_eq!(answer(&mut viewer).await.req, Some(10));
    conn.send(&Request::PaneKill { pane }).await;
    answer(&mut conn).await;
    let event = timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && serde_json::from_slice::<Event>(&json).is_ok()
            {
                return serde_json::from_slice::<serde_json::Value>(&json).unwrap();
            }
        }
    })
    .await
    .expect("an event");
    assert!(
        event.get("req").is_none(),
        "an event carries no req: {event}"
    );

    server.stop().await;
}

/// The runtime dir of a daemon this test runs from the real binary — its
/// socket under `tutti/`, its saved layout under `state/` (pass it as
/// `XDG_STATE_HOME`, or the daemon saves into the real home) — on a short
/// `/tmp` path: macOS caps a socket path near 104 bytes.
fn runtime_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = PathBuf::from("/tmp").join(format!("tt-{label}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(base.join("tutti")).unwrap();
    base
}

/// Connect to a daemon the test started, waiting for it to come up.
async fn connect_when_up(path: &Path) -> Conn {
    timeout(DEADLINE, async {
        loop {
            if let Ok(stream) = UnixStream::connect(path).await {
                return Conn {
                    stream,
                    buf: Vec::new(),
                };
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the daemon came up")
}

/// Whether the daemon at `path` answers a `WorkspaceList`, retrying while it
/// drops the connection unanswered: macOS destroys a queued connection that
/// an accept dequeued and then failed on with EMFILE.
async fn answers(path: &Path) -> bool {
    let list = Frame::Control(serde_json::to_vec(&Request::WorkspaceList).unwrap()).encode();
    timeout(DEADLINE, async {
        loop {
            if let Ok(mut stream) = UnixStream::connect(path).await
                && stream.write_all(&list).await.is_ok()
            {
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
                while let Ok(n @ 1..) = stream.read(&mut chunk).await {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Ok(Some((Frame::Control(json), _))) = Frame::decode(&buf) {
                        let answer = serde_json::from_slice(&json);
                        return matches!(answer, Ok(Response::Workspaces { .. }));
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// Wait until `log` contains `needle`.
async fn log_says(log: &Path, needle: &str) -> bool {
    timeout(DEADLINE, async {
        while !std::fs::read_to_string(log).is_ok_and(|text| text.contains(needle)) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// SIGTERMs the daemon serving this socket once the test ends, however it
/// ends — a daemon run from the binary outlives a failed assertion. Only while
/// the socket still answers: a dead daemon's stale pidfile may name a pid
/// reused since.
struct StopOnDrop(PathBuf);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if std::os::unix::net::UnixStream::connect(&self.0).is_ok()
            && let Ok(pid) = std::fs::read_to_string(self.0.with_extension("pid"))
        {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
    }
}

/// An accept error — EMFILE here: the daemon runs under a 16-descriptor cap and
/// more clients connect than it can hold — is logged and retried. It used to
/// end `serve`, and with it every pane.
#[tokio::test]
async fn an_accept_error_is_logged_and_retried() {
    let base = runtime_dir("emfile");
    let log = base.join("daemon.log");
    let mut daemon = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "ulimit -n 16 && exec \"$0\" --session cap --foreground",
        ])
        .arg(env!("CARGO_BIN_EXE_tutti-server"))
        .env("XDG_RUNTIME_DIR", &base)
        .env("XDG_STATE_HOME", base.join("state"))
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let path = base.join("tutti").join("cap.sock");
    let stop = StopOnDrop(path.clone());

    let mut held = Vec::new();
    for _ in 0..24 {
        held.push(connect_when_up(&path).await);
    }
    assert!(
        log_says(&log, "accept failed").await,
        "the daemon never ran out of descriptors"
    );
    drop(held);
    assert!(
        answers(&path).await,
        "the daemon stopped serving after the accept error"
    );

    drop(stop);
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&base);
}

/// A daemon that died without stopping leaves its prompt files behind; the
/// next one for the session clears them before it binds its socket — before
/// any client can write a new one there. (`TestServer` skips `run`.)
#[tokio::test]
async fn a_starting_daemon_clears_the_prompt_files_a_dead_one_left() {
    let base = runtime_dir("prompts");
    let prompts = base.join("tutti").join("left.prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("prompt-1-2.md"), "fix it").unwrap();
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_tutti-server"))
        .args(["--session", "left", "--foreground"])
        .env("XDG_RUNTIME_DIR", &base)
        .env("XDG_STATE_HOME", base.join("state"))
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let path = base.join("tutti").join("left.sock");
    let stop = StopOnDrop(path.clone());
    assert!(answers(&path).await, "the daemon came up");
    assert!(!prompts.exists(), "the leftovers went before it bound");

    drop(stop);
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&base);
}

/// Started detached, the daemon appends its stderr to `<session>.log` beside
/// its socket, so crash notes outlive it — trimming a log grown past 4 MiB to
/// its newest MiB, from a line start, as it starts and again while it runs,
/// its own lines landing after what it kept.
#[tokio::test]
async fn a_detached_daemon_logs_beside_its_socket() {
    let base = runtime_dir("log");
    let dir = base.join("tutti");
    let log = dir.join("logged.log");
    let mut old = "an old line\n".repeat(400_000);
    old.push_str("the newest old line\n");
    std::fs::write(&log, old).unwrap();
    let launched = std::process::Command::new(env!("CARGO_BIN_EXE_tutti-server"))
        .args(["--session", "logged"])
        .env("XDG_RUNTIME_DIR", &base)
        .env("XDG_STATE_HOME", base.join("state"))
        .status()
        .unwrap();
    assert!(launched.success());
    let stop = StopOnDrop(dir.join("logged.sock"));
    let mut conn = connect_when_up(&stop.0).await;

    // An undecodable frame ends its connection with a logged error.
    conn.stream.write_all(&[1, 0, 0, 0, 0xff]).await.unwrap();
    assert!(
        log_says(&log, "connection error").await,
        "the daemon's stderr never reached its log"
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.len() < (1 << 20) + 4096,
        "trimmed: {} bytes",
        text.len()
    );
    assert!(text.starts_with("an old line\n") && text.contains("the newest old line\n"));

    let mut grown = "a later line\n".repeat(400_000);
    grown.push_str("the newest later line\n");
    let mut appending = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    std::io::Write::write_all(&mut appending, grown.as_bytes()).unwrap();
    let trimmed = timeout(DEADLINE, async {
        while std::fs::metadata(&log).unwrap().len() > 1 << 20 {
            sleep(Duration::from_millis(50)).await;
        }
    });
    assert!(
        trimmed.await.is_ok(),
        "the running daemon never trimmed its log"
    );
    let mut conn = connect_when_up(&stop.0).await;
    conn.stream.write_all(&[1, 0, 0, 0, 0xff]).await.unwrap();
    assert!(log_says(&log, "connection error").await);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.len() < (1 << 20) + 4096,
        "trimmed: {} bytes",
        text.len()
    );
    let kept = text.split_once("the newest later line\n");
    assert!(
        text.starts_with("a later line\n")
            && kept.is_some_and(|(_, after)| after.contains("connection error")),
        "the daemon's line follows what was kept"
    );

    drop(stop);
    let _ = std::fs::remove_dir_all(&base);
}

/// Every spawned pane carries `TUTTI_PANE` (its id) and `TUTTI_SESSION` (the
/// socket's session name) in its environment, so a Claude Code hook running
/// inside it can address this daemon, and `COLORTERM=truecolor`, so editors
/// turn on 24-bit colour. A daemon started inside an rtty pane hands its own
/// panes neither that pane's `RTTY_PANE` nor `RTTY_SOCKET`, which would be
/// wrong there; `TERM_PROGRAM` goes through.
#[tokio::test]
async fn spawned_pane_carries_tutti_env() {
    let base = runtime_dir("env");
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_tutti-server"))
        .args(["--session", "env", "--foreground"])
        .env("XDG_RUNTIME_DIR", &base)
        .env("XDG_STATE_HOME", base.join("state"))
        .env("RTTY_PANE", "7")
        .env("RTTY_SOCKET", base.join("rtty.sock"))
        .env("TERM_PROGRAM", "rtty")
        .spawn()
        .unwrap();
    let stop = StopOnDrop(base.join("tutti").join("env.sock"));
    let mut conn = connect_when_up(&stop.0).await;
    let pane = run_marker(
        &mut conn,
        "printf 'ENV %s %s %s [%s] [%s] %s\\n' \"$TUTTI_PANE\" \"$TUTTI_SESSION\" \"$COLORTERM\" \
         \"${RTTY_PANE-unset}\" \"${RTTY_SOCKET-unset}\" \"$TERM_PROGRAM\"; sleep 30",
    )
    .await;
    assert!(
        read_until(
            &mut conn,
            pane,
            &format!("ENV {pane} env truecolor [unset] [unset] rtty")
        )
        .await,
        "the pane env should carry TUTTI_PANE=<id>, TUTTI_SESSION=<name>, COLORTERM=truecolor \
         and TERM_PROGRAM, and no RTTY_PANE or RTTY_SOCKET"
    );

    drop(stop);
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&base);
}

/// Send one request line to herdr's socket at `socket`; its reply, parsed.
async fn herdr(socket: &Path, request: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    timeout(DEADLINE, stream.read_to_string(&mut reply))
        .await
        .expect("timed out waiting for a herdr reply")
        .unwrap();
    serde_json::from_str(&reply).unwrap()
}

/// A pane carries herdr's env, so an agent that reports to herdr natively
/// reports to tutti: Crush-shaped requests on that socket drive the pane's
/// state, label and conversation, a stale one changes nothing, and the
/// release ends the claim.
#[tokio::test]
async fn a_herdr_reporter_drives_its_pane() {
    use serde_json::json;
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let socket = server.path.with_extension("herdr.sock");
    let pane = run_marker(
        &mut conn,
        &format!(
            "[ \"$HERDR_SOCKET_PATH\" = '{}' ] && printf 'HERDR %s %s %s %s\\n' \"$HERDR_ENV\" \
             \"$HERDR_PANE_ID\" \"$HERDR_TAB_ID\" \"$HERDR_WORKSPACE_ID\"; sleep 30",
            socket.display()
        ),
    )
    .await;
    let id = format!("w1:p{pane}");
    assert!(
        read_until(&mut conn, pane, &format!("HERDR 1 {id} w1:t1 w1")).await,
        "the pane env should point herdr reporters at this daemon's socket"
    );

    // Crush's seq starts at UnixNano, past what an f64 holds exactly.
    let seq = 1_791_590_400_123_456_789_u64;
    let report = |state: &str, seq: u64, session: &str| {
        json!({
            "id": format!("crush:report:{seq}"),
            "method": "pane.report_agent",
            "params": {"pane_id": id, "source": "crush", "agent": "crush", "state": state,
                "seq": seq, "agent_session_id": session},
        })
    };
    let ok = |seq: u64| json!({"id": format!("crush:report:{seq}"), "result": {"type": "ok"}});
    let crush = |p: &PaneInfo| p.agent.as_ref().is_some_and(|a| a.0 == "crush");

    assert_eq!(herdr(&socket, report("idle", seq, "")).await, ok(seq));
    assert!(
        wait_pane(&mut conn, pane, |p| p.state == AgentState::Idle
            && p.source == Some(StateSource::Hook)
            && crush(p))
        .await,
        "Crush's startup idle claims the pane at the hook rank, labelled crush"
    );
    assert_eq!(
        herdr(&socket, report("working", seq + 1, "")).await,
        ok(seq + 1)
    );
    let session = json!({
        "id": "crush:session",
        "method": "pane.report_agent_session",
        "params": {"pane_id": id, "source": "crush", "agent": "crush", "seq": seq + 2,
            "agent_session_id": "c1"},
    });
    assert_eq!(herdr(&socket, session).await["result"]["type"], "ok");
    assert_eq!(
        herdr(&socket, report("blocked", seq, "")).await,
        ok(seq),
        "a stale report is answered, and ignored"
    );
    assert!(
        wait_pane(&mut conn, pane, |p| p.state == AgentState::Working
            && p.conversation.as_deref() == Some("c1"))
        .await,
        "working, its conversation named, the stale blocked dropped"
    );
    assert_eq!(
        herdr(&socket, report("idle", seq + 3, "c1")).await,
        ok(seq + 3)
    );
    assert!(
        wait_pane(&mut conn, pane, |p| p.state == AgentState::Done).await,
        "idle after work is a finish"
    );

    let release = json!({
        "id": "crush:release",
        "method": "pane.release_agent",
        "params": {"pane_id": id, "source": "crush", "agent": "crush", "state": "",
            "seq": seq + 4, "agent_session_id": "c1"},
    });
    assert_eq!(herdr(&socket, release).await["result"]["type"], "ok");
    assert!(
        wait_pane(&mut conn, pane, |p| p.state == AgentState::Done
            && p.source.is_none())
        .await,
        "the release ends the claim; the unseen finish stays"
    );
    let mut stranger = report("working", seq + 5, "");
    stranger["params"]["pane_id"] = json!("w1:p999");
    assert_eq!(
        herdr(&socket, stranger).await["error"]["code"],
        "pane_not_found"
    );

    server.stop().await;
    assert!(
        !socket.exists(),
        "the daemon removes herdr's socket on its way out"
    );
}

/// `run` from inside a pane lands in that pane's workspace rather than the
/// session's current one, unless a workspace is named; an unknown caller is
/// an error.
#[tokio::test]
async fn run_lands_beside_its_calling_pane() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let caller = run_sleeper(&mut conn).await;
    let home = workspace_ids(&mut conn).await[0];
    let away = fresh_dir("run-from");
    let current = workspace_id(
        conn.request(Request::WorkspaceNew { dir: away.clone() })
            .await,
    );
    let run = |workspace, from| Request::Run {
        workspace,
        dir: None,
        cmd: vec!["/bin/sleep".into(), "30".into()],
        sandbox: None,
        title: None,
        issue: None,
        from,
    };

    let (beside, _, _) = ran(conn.request(run(None, Some(caller))).await);
    assert_eq!(beside, home, "beside its caller");
    let (unnamed, _, _) = ran(conn.request(run(None, None)).await);
    assert_eq!(unnamed, current, "no caller: the current workspace");
    let (named, _, _) = ran(conn.request(run(Some(current), Some(caller))).await);
    assert_eq!(named, current, "a named workspace wins");
    let message = error_message(conn.request(run(None, Some(PaneId(999)))).await);
    assert!(message.contains("no pane 999"), "{message}");

    let _ = std::fs::remove_dir_all(&away);
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));

    assert_eq!(
        control
            .request(Request::AgentEvent {
                pane,
                event: AgentHookEvent::Blocked {
                    message: Some("allow edit?".into()),
                    reminder: false,
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

/// The hold `tutti agent-event` sends for a Claude permission request on
/// `pane` under `approvals = "hold"`.
fn hold(pane: PaneId) -> Request {
    Request::ApprovalHold {
        pane,
        conversation: Some("c1".into()),
        tool: "Bash".into(),
        summary: "rm -rf build".into(),
        agent: None,
        call: Some("rm-build".into()),
    }
}

fn approval(response: Response) -> ApprovalDecision {
    match response {
        Response::Approval { decision } => decision,
        other => panic!("expected Approval, got {other:?}"),
    }
}

/// Connect a client and attach it, so a hold has someone to answer it.
async fn attached_viewer(server: &TestServer) -> Conn {
    let mut viewer = server.connect().await;
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
    assert!(matches!(viewer.response().await, Response::Attached { .. }));
    viewer
}

/// Read an attached `viewer`'s frames until a view shows `pane` holding an
/// approval (`held`) or none, returning what it holds.
async fn view_approval(viewer: &mut Conn, pane: PaneId, held: bool) -> Option<PendingApproval> {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Ok(Event::LayoutChanged { workspaces }) =
                    serde_json::from_slice::<Event>(&json)
                && let Some(info) = workspaces
                    .iter()
                    .flat_map(|w| &w.tabs)
                    .flat_map(|t| &t.panes)
                    .find(|p| p.id == pane)
                && info.approval.is_some() == held
            {
                return info.approval.clone();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no view with the approval held: {held}"))
}

/// With no client attached nobody could answer: the hold answers `Pass` at
/// once — Claude's own dialog appears — yet the pane blocks as the request's
/// hook event would have blocked it, and nothing stays held. A view, which
/// answers nothing, is nobody to wait for either.
#[tokio::test]
async fn an_approval_hold_with_no_client_attached_passes_at_once() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;

    let mut hook = server.connect().await;
    assert_eq!(
        approval(hook.request(hold(pane)).await),
        ApprovalDecision::Pass
    );
    assert!(
        wait_pane(&mut control, pane, |p| p.state == AgentState::Blocked
            && p.source == Some(StateSource::Hook)
            && p.conversation.as_deref() == Some("c1")
            && p.approval.is_none())
        .await
    );

    let mut viewer = server.connect().await;
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: true,
            quiet: false,
        })
        .await;
    read_until_control(&mut viewer, |r: &Response| {
        matches!(r, Response::Attached { .. })
    })
    .await;
    let mut hook = server.connect().await;
    let answer = timeout(DEADLINE, hook.request(hold(pane))).await;
    assert_eq!(
        approval(answer.expect("a view held the request")),
        ApprovalDecision::Pass
    );

    server.stop().await;
}

/// With a client attached the hold waits: the pane blocks holding the
/// approval, the view says so, and the answer naming it reaches the hook —
/// the approval leaving the pane and the view.
#[tokio::test]
async fn a_held_approval_waits_for_the_answer_naming_it() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let mut viewer = attached_viewer(&server).await;

    let mut hook = server.connect().await;
    hook.send(&hold(pane)).await;
    let held = view_approval(&mut viewer, pane, true).await.unwrap();
    assert_eq!(
        (held.tool.as_str(), held.summary.as_str()),
        ("Bash", "rm -rf build")
    );
    assert!(wait_state(&mut control, pane, AgentState::Blocked).await);

    let deny = ApprovalDecision::Deny {
        message: Some("not on main".into()),
    };
    let answered = control
        .request(Request::ApprovalAnswer {
            pane,
            id: Some(held.id),
            decision: deny.clone(),
        })
        .await;
    assert_eq!(answered, Response::Ok);
    assert_eq!(approval(hook.response().await), deny);
    assert_eq!(view_approval(&mut viewer, pane, false).await, None);
    assert!(wait_pane(&mut control, pane, |p| p.approval.is_none()).await);
    assert!(
        wait_state(&mut control, pane, AgentState::Working).await,
        "an answer sets the agent going — no Blocked through the tool's run"
    );

    server.stop().await;
}

/// The approval lives as long as its hook: the hook hanging up — Claude
/// timed it out, or quit — drops it from the pane and the view.
#[tokio::test]
async fn a_hook_hanging_up_drops_its_approval() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let mut viewer = attached_viewer(&server).await;

    let mut hook = server.connect().await;
    hook.send(&hold(pane)).await;
    let held = view_approval(&mut viewer, pane, true).await.unwrap();
    drop(hook);
    assert_eq!(view_approval(&mut viewer, pane, false).await, None);
    let late = control
        .request(Request::ApprovalAnswer {
            pane,
            id: Some(held.id),
            decision: ApprovalDecision::Allow,
        })
        .await;
    assert!(error_message(late).contains("no pending approval"));

    server.stop().await;
}

/// Claude (2.1.318) shows its own dialog beside a held request. Answered
/// there, the tool runs — under a call naming no held request, or none — and
/// the turn ends while the hook still holds. An agent's turn end passes the
/// requests that agent raised — the main agent's at Stop, a subagent's at its
/// SubagentStop — and no other: a background subagent can still run, and
/// ask, past the main agent's Stop. Each passed request counts as gone to the
/// agent's dialog, as a timed-out one does, and an answer that lost the race
/// to the turn end is told so.
#[tokio::test]
async fn a_turns_end_passes_the_requests_its_own_agent_raised() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let _viewer = attached_viewer(&server).await;

    let mut main = server.connect().await;
    main.send(&hold(pane)).await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
    let mut background = server.connect().await;
    background
        .send(&Request::ApprovalHold {
            pane,
            conversation: None,
            tool: "Bash".into(),
            summary: "touch b.txt".into(),
            agent: Some("sub1".into()),
            call: Some("sub1-touch-b".into()),
        })
        .await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 2).await);
    let held = held_approvals(&mut control, pane).await;
    let ran = AgentHookEvent::Activity { detail: None };
    hook(&mut control, pane, ran).await;
    let other = AgentHookEvent::Activity {
        detail: Some("main-touch-b".into()),
    };
    hook(&mut control, pane, other).await;
    assert_eq!(
        held_approvals(&mut control, pane).await,
        held,
        "a run of another call, or of none named, lets go of nothing"
    );

    let done = AgentHookEvent::Done {
        reply: Some("done".into()),
    };
    hook(&mut control, pane, done).await;
    assert_eq!(approval(main.response().await), ApprovalDecision::Pass);
    assert_eq!(
        held_approvals(&mut control, pane).await,
        [held[1].clone()],
        "the background subagent's request still waits"
    );
    let late = control
        .request(Request::ApprovalAnswer {
            pane,
            id: Some(held[0].id),
            decision: ApprovalDecision::Allow,
        })
        .await;
    let message = error_message(late);
    assert!(message.contains("ended with its agent's turn"), "{message}");

    let stopped = AgentHookEvent::SubagentStopped { id: "sub1".into() };
    hook(&mut control, pane, stopped).await;
    assert_eq!(
        approval(background.response().await),
        ApprovalDecision::Pass
    );
    assert!(held_approvals(&mut control, pane).await.is_empty());

    let mut next = server.connect().await;
    next.send(&hold(pane)).await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
    let allow = Request::ApprovalAnswer {
        pane,
        id: None,
        decision: ApprovalDecision::Allow,
    };
    assert_eq!(control.request(allow).await, Response::Ok);
    assert_eq!(approval(next.response().await), ApprovalDecision::Allow);
    let Response::Panes { panes } = control.request(Request::PaneList).await else {
        panic!("expected the panes");
    };
    assert_eq!(
        panes.iter().find(|p| p.id == pane).map(|p| p.state),
        Some(AgentState::Blocked),
        "a released request may still show in the agent's dialog, so tutti's \
         answer to the next one keeps the pane blocked until a hook moves it"
    );

    server.stop().await;
}

/// Codex 0.166 runs parallel tool calls, each asking on its own (seen live):
/// allowed one of two held requests, it runs that tool and its hooks report
/// the work, while the other still waits on you — so the pane stays blocked
/// until that one is answered. A tool run is also how a request answered in
/// the agent's own dialog shows itself: a run naming a held request's own
/// call lets that request go, passed to its hook, and the agent works on. A
/// new prompt lets go of what the main agent still held: the turn it asked
/// in is over.
#[tokio::test]
async fn a_pane_stays_blocked_while_a_request_it_holds_waits() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let _viewer = attached_viewer(&server).await;
    let asks = |summary: &str| Request::ApprovalHold {
        pane,
        conversation: None,
        tool: "Bash".into(),
        summary: summary.into(),
        agent: None,
        call: Some(format!("call {summary}")),
    };
    let ran = |summary: &str| AgentHookEvent::Activity {
        detail: Some(format!("call {summary}")),
    };
    let state = async |control: &mut Conn| {
        let Response::Panes { panes } = control.request(Request::PaneList).await else {
            panic!("expected the panes");
        };
        let info = panes.into_iter().find(|p| p.id == pane).unwrap();
        (info.state, info.approvals.len())
    };

    let (mut c, mut d) = (server.connect().await, server.connect().await);
    c.send(&asks("touch c.txt")).await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
    d.send(&asks("touch d.txt")).await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 2).await);
    let held = held_approvals(&mut control, pane).await;
    let allow = Request::ApprovalAnswer {
        pane,
        id: Some(held[0].id),
        decision: ApprovalDecision::Allow,
    };
    assert_eq!(control.request(allow).await, Response::Ok);
    assert_eq!(approval(c.response().await), ApprovalDecision::Allow);
    hook(&mut control, pane, ran("touch c.txt")).await;
    assert_eq!(
        state(&mut control).await,
        (AgentState::Blocked, 1),
        "the other request still waits on you"
    );

    hook(&mut control, pane, ran("touch d.txt")).await;
    assert_eq!(approval(d.response().await), ApprovalDecision::Pass);
    assert_eq!(state(&mut control).await, (AgentState::Working, 0));

    let mut e = server.connect().await;
    e.send(&asks("touch e.txt")).await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
    let next = AgentHookEvent::Prompt {
        text: Some("try something else".into()),
        id: Some("p2".into()),
    };
    hook(&mut control, pane, next).await;
    assert_eq!(approval(e.response().await), ApprovalDecision::Pass);
    assert_eq!(state(&mut control).await, (AgentState::Working, 0));

    server.stop().await;
}

/// An answer lands only on the approval it names: a stale id is refused, as
/// is any answer once nothing is held, while `None` answers the oldest.
#[tokio::test]
async fn a_stale_approval_answer_is_refused() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let _viewer = attached_viewer(&server).await;

    let mut hook = server.connect().await;
    hook.send(&hold(pane)).await;
    assert!(wait_pane(&mut control, pane, |p| p.approval.is_some()).await);
    let answer = |id| Request::ApprovalAnswer {
        pane,
        id,
        decision: ApprovalDecision::Allow,
    };
    let held = held_approvals(&mut control, pane).await.remove(0);
    let stale = control.request(answer(Some(held.id + 1))).await;
    assert_eq!(
        error_message(stale),
        format!("pane {pane} has no pending approval #{}", held.id + 1)
    );
    assert_eq!(control.request(answer(None)).await, Response::Ok);
    assert_eq!(approval(hook.response().await), ApprovalDecision::Allow);
    let again = control.request(answer(Some(held.id))).await;
    assert!(error_message(again).contains("no pending approval"));
    let none = control.request(answer(None)).await;
    assert_eq!(
        error_message(none),
        format!("pane {pane} has no pending approval")
    );

    server.stop().await;
}

/// The approvals `pane` holds, oldest first, from a fresh `pane list`.
async fn held_approvals(conn: &mut Conn, pane: PaneId) -> Vec<PendingApproval> {
    match conn.request(Request::PaneList).await {
        Response::Panes { panes } => panes.into_iter().find(|p| p.id == pane).unwrap().approvals,
        other => panic!("expected Panes, got {other:?}"),
    }
}

/// Parallel subagents ask at once: the pane holds every request, oldest
/// first, and each is answered on its own. The pane stays blocked while any
/// still waits on you; a hook hanging up (its own timeout) drops only its
/// own request; the last answer sets the agent going.
#[tokio::test]
async fn a_pane_holds_parallel_approvals_each_answered_on_its_own() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let _viewer = attached_viewer(&server).await;

    let mut hooks = Vec::new();
    for summary in ["touch a.txt", "touch b.txt", "touch c.txt"] {
        let mut hook = server.connect().await;
        hook.send(&Request::ApprovalHold {
            pane,
            conversation: None,
            tool: "Bash".into(),
            summary: summary.into(),
            agent: None,
            call: Some(summary.into()),
        })
        .await;
        hooks.push(hook);
        let count = hooks.len();
        assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == count).await);
    }
    let held = held_approvals(&mut control, pane).await;
    assert_eq!(
        held.iter().map(|a| a.summary.as_str()).collect::<Vec<_>>(),
        ["touch a.txt", "touch b.txt", "touch c.txt"],
        "in the order they came"
    );
    assert!(held.windows(2).all(|w| w[0].id < w[1].id));
    let Response::Notes { notes } = control.request(Request::NotesList { unread: false }).await
    else {
        panic!("expected the notes");
    };
    let noted: Vec<_> = notes
        .iter()
        .filter(|n| n.kind == NoteKind::Blocked)
        .map(|n| n.body.as_deref())
        .collect();
    assert_eq!(
        noted,
        [
            Some("Bash · touch a.txt"),
            Some("Bash · touch b.txt"),
            Some("Bash · touch c.txt")
        ],
        "each request lands in the history, the blocked pane's later ones too"
    );

    let answer = |id, decision| Request::ApprovalAnswer {
        pane,
        id: Some(id),
        decision,
    };
    assert_eq!(
        control
            .request(answer(held[1].id, ApprovalDecision::Allow))
            .await,
        Response::Ok
    );
    assert_eq!(approval(hooks[1].response().await), ApprovalDecision::Allow);
    let waiting = |p: &PaneInfo| p.approvals.len() == 2 && p.state == AgentState::Blocked;
    assert!(
        wait_pane(&mut control, pane, waiting).await,
        "the others still wait on you"
    );

    drop(hooks.remove(2));
    assert!(
        wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await,
        "a hook giving up drops its own request only"
    );
    assert_eq!(held_approvals(&mut control, pane).await[0].id, held[0].id);
    assert_eq!(
        control
            .request(answer(held[0].id, ApprovalDecision::Pass))
            .await,
        Response::Ok
    );
    assert_eq!(approval(hooks[0].response().await), ApprovalDecision::Pass);
    assert!(
        wait_pane(&mut control, pane, |p| p.approvals.is_empty()
            && p.state == AgentState::Blocked)
        .await,
        "a pass leaves the last one to Claude's dialog"
    );
    // The tool Claude's dialog let through has run: the agent moved on.
    hook(
        &mut control,
        pane,
        AgentHookEvent::Activity { detail: None },
    )
    .await;

    let mut last = server.connect().await;
    last.send(&hold(pane)).await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
    let deny = ApprovalDecision::Deny { message: None };
    assert_eq!(
        control
            .request(Request::ApprovalAnswer {
                pane,
                id: None,
                decision: deny.clone(),
            })
            .await,
        Response::Ok
    );
    assert_eq!(approval(last.response().await), deny);
    assert!(
        wait_state(&mut control, pane, AgentState::Working).await,
        "the last answer sets the agent going"
    );

    server.stop().await;
}

/// A pane's child exiting drops the approvals it held — nobody is left to
/// ask — and each hook's own hang-up ends its hold quietly.
#[tokio::test]
async fn an_exiting_pane_drops_its_held_approvals() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let pane = pane_id(
        control
            .request(Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/sh".into(), "-c".into(), "read line".into()],
                ephemeral: false,
                sandbox: None,
            })
            .await,
    );
    let _viewer = attached_viewer(&server).await;
    let mut hook = server.connect().await;
    hook.send(&hold(pane)).await;
    assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
    let enter = Request::PaneSend {
        pane,
        text: Some("\n".into()),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    assert_eq!(control.request(enter).await, Response::Ok);
    assert!(
        wait_pane(&mut control, pane, |p| p.exited.is_some()
            && p.approvals.is_empty()
            && p.approval.is_none())
        .await
    );
    drop(hook);
    assert!(held_approvals(&mut control, pane).await.is_empty());

    server.stop().await;
}

/// A request handed on to the agent's own dialog — passed from tutti, or its
/// hook gone — still waits on you there, so tutti's answer to another keeps
/// the pane blocked until a hook shows the agent moved on.
#[tokio::test]
async fn a_request_left_to_the_agents_dialog_keeps_the_pane_blocked() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_sleeper(&mut control).await;
    let _viewer = attached_viewer(&server).await;
    let answer = |id, decision| Request::ApprovalAnswer {
        pane,
        id: Some(id),
        decision,
    };

    for passed in [true, false] {
        let (mut first, mut second) = (server.connect().await, server.connect().await);
        first.send(&hold(pane)).await;
        assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
        second.send(&hold(pane)).await;
        assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 2).await);
        let held = held_approvals(&mut control, pane).await;
        if passed {
            let pass = answer(held[0].id, ApprovalDecision::Pass);
            assert_eq!(control.request(pass).await, Response::Ok);
        } else {
            drop(first);
            assert!(wait_pane(&mut control, pane, |p| p.approvals.len() == 1).await);
        }
        let allow = answer(held[1].id, ApprovalDecision::Allow);
        assert_eq!(control.request(allow).await, Response::Ok);
        assert_eq!(approval(second.response().await), ApprovalDecision::Allow);
        assert!(
            wait_pane(&mut control, pane, |p| p.approvals.is_empty()
                && p.state == AgentState::Blocked)
            .await,
            "Claude's dialog still asks (passed: {passed})"
        );
        hook(
            &mut control,
            pane,
            AgentHookEvent::Activity { detail: None },
        )
        .await;
        assert!(wait_state(&mut control, pane, AgentState::Working).await);
    }

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
            event: AgentHookEvent::Done { reply: None },
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

/// The next note an attached `conn` is told of, skipping every other frame.
async fn noted(conn: &mut Conn) -> Note {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = conn.read_frame().await
                && let Ok(Event::Noted { note }) = serde_json::from_slice(&json)
            {
                return note;
            }
        }
    })
    .await
    .expect("no note landed within the deadline")
}

/// The next whole history an attached `conn` is sent, skipping every other
/// frame.
async fn notes_changed(conn: &mut Conn) -> Vec<Note> {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = conn.read_frame().await
                && let Ok(Event::NotesChanged { notes }) = serde_json::from_slice(&json)
            {
                return notes;
            }
        }
    })
    .await
    .expect("the history was not rebroadcast within the deadline")
}

/// The history as `notes` lists it: only the unread notes with `unread`.
async fn list_notes(conn: &mut Conn, unread: bool) -> Vec<Note> {
    match conn.request(Request::NotesList { unread }).await {
        Response::Notes { notes } => notes,
        other => panic!("expected Notes, got {other:?}"),
    }
}

/// An interactive shell in a fresh workspace, for `printf_osc` to drive.
async fn run_shell(conn: &mut Conn) -> PaneId {
    new_workspace(conn).await;
    pane_id(
        conn.request(Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into()],
            ephemeral: false,
            sandbox: None,
        })
        .await,
    )
}

/// A pane's desktop notification and its agent's attention transitions — into
/// blocked, carrying the hook's message, and working to done — land in the
/// history as they happen, each broadcast as `Noted` and keeping the pane's
/// title and workspace name; a turn starting wants no attention. A later
/// attach is handed the history.
#[tokio::test]
async fn notifications_and_attention_transitions_land_in_the_history() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_shell(&mut control).await;
    let mut viewer = server.connect().await;
    attach(&mut viewer).await;

    printf_osc(&mut control, pane, "9;tests pass").await;
    let note = noted(&mut viewer).await;
    let workspace = std::env::temp_dir();
    let workspace = workspace.file_name().unwrap().to_string_lossy();
    assert_eq!(
        note,
        Note {
            id: note.id,
            time: note.time,
            pane,
            pane_title: "sh".into(),
            workspace: workspace.into_owned(),
            kind: NoteKind::Notify,
            title: None,
            body: Some("tests pass".into()),
            read: false,
        }
    );

    for event in [
        AgentHookEvent::Activity { detail: None },
        AgentHookEvent::Blocked {
            message: Some("Bash · rm -rf build".into()),
            reminder: false,
        },
        AgentHookEvent::Activity { detail: None },
        AgentHookEvent::Done { reply: None },
    ] {
        hook(&mut control, pane, event).await;
    }
    let blocked = noted(&mut viewer).await;
    assert_eq!(
        (blocked.kind, blocked.title, blocked.body.as_deref()),
        (NoteKind::Blocked, None, Some("Bash · rm -rf build")),
        "the hook's message rides on its note"
    );
    assert_eq!(noted(&mut viewer).await.kind, NoteKind::Done);

    let history = list_notes(&mut control, false).await;
    let kinds: Vec<NoteKind> = history.iter().map(|n| n.kind).collect();
    assert_eq!(kinds, [NoteKind::Notify, NoteKind::Blocked, NoteKind::Done]);
    assert!(
        history.windows(2).all(|w| w[0].id < w[1].id),
        "ids count up"
    );

    let mut late = server.connect().await;
    late.send(&Request::Attach {
        terminal: None,
        handoff: false,
        view: false,
        quiet: false,
    })
    .await;
    let caught_up = timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = late.read_frame().await
                && let Ok(Response::Attached { notes, .. }) = serde_json::from_slice(&json)
            {
                return notes;
            }
        }
    })
    .await
    .expect("no Attached reply");
    assert_eq!(caught_up, history, "an attach carries the whole history");

    server.stop().await;
}

/// A pane already blocked notes each further thing its agent waits on — a
/// second request, a question — but not Claude's `permission_prompt`
/// reminder of a dialog whose request is noted. A reminder that blocks a
/// working pane itself — the first word of a block with no PermissionRequest
/// hook wired, or a late one after the dialog's answer — notes that move.
#[tokio::test]
async fn a_second_blocked_event_notes_what_is_new_but_not_a_reminder() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_shell(&mut control).await;
    let blocked = |message: &str, reminder| AgentHookEvent::Blocked {
        message: Some(message.into()),
        reminder,
    };
    for event in [
        blocked("Bash · rm -rf build", false),
        blocked("Claude needs your permission to use Bash", true),
        blocked("Edit · src/main.rs", false),
        blocked("Claude needs your permission to use Edit", true),
        blocked("Which crate?", false),
        AgentHookEvent::Activity { detail: None },
        blocked("Claude needs your permission to use Write", true),
    ] {
        hook(&mut control, pane, event).await;
    }
    let notes = list_notes(&mut control, false).await;
    let noted: Vec<_> = notes
        .iter()
        .map(|n| (n.kind, n.body.as_deref().unwrap_or_default()))
        .collect();
    assert_eq!(
        noted,
        [
            (NoteKind::Blocked, "Bash · rm -rf build"),
            (NoteKind::Blocked, "Edit · src/main.rs"),
            (NoteKind::Blocked, "Which crate?"),
            (
                NoteKind::Blocked,
                "Claude needs your permission to use Write"
            ),
        ]
    );

    server.stop().await;
}

/// The history keeps the newest 200 notes, the oldest leaving first, and ids
/// are never reused: not past the cap, and not after a clear.
#[tokio::test]
async fn the_history_caps_and_never_reuses_an_id() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_shell(&mut conn).await;
    let burst = Request::PaneSend {
        pane,
        text: Some(
            "i=0; while [ $i -lt 205 ]; do i=$((i+1)); printf '\\033]9;n%d\\007' $i; done\n".into(),
        ),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    assert_eq!(conn.request(burst).await, Response::Ok);
    let history = timeout(DEADLINE, async {
        loop {
            let notes = list_notes(&mut conn, false).await;
            if notes
                .last()
                .is_some_and(|n| n.body.as_deref() == Some("n205"))
            {
                return notes;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the burst never fully landed");
    assert_eq!(history.len(), 200, "the history caps at 200");
    assert_eq!(
        (history[0].id, history[0].body.as_deref()),
        (6, Some("n6")),
        "the five oldest left first"
    );
    assert_eq!(history[199].id, 205);

    assert_eq!(conn.request(Request::NotesClear).await, Response::Ok);
    assert!(list_notes(&mut conn, false).await.is_empty());
    printf_osc(&mut conn, pane, "9;after").await;
    let after = timeout(DEADLINE, async {
        loop {
            if let [note] = list_notes(&mut conn, false).await.as_slice() {
                return note.clone();
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no note after the clear");
    assert_eq!(
        (after.id, after.body.as_deref()),
        (206, Some("after")),
        "ids go on counting past a clear"
    );

    server.stop().await;
}

/// Focusing a pane marks its notes read — and only its own — broadcasting the
/// history so every client agrees.
#[tokio::test]
async fn focusing_a_pane_marks_its_notes_read() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let first = run_shell(&mut control).await;
    let second = run_shell(&mut control).await;
    let mut viewer = server.connect().await;
    attach(&mut viewer).await;
    printf_osc(&mut control, first, "9;one").await;
    noted(&mut viewer).await;
    printf_osc(&mut control, second, "9;two").await;
    noted(&mut viewer).await;

    assert_eq!(
        control
            .request(Request::PaneFocus {
                pane: first,
                seen: false
            })
            .await,
        Response::Ok
    );
    let read: Vec<(PaneId, bool)> = notes_changed(&mut viewer)
        .await
        .iter()
        .map(|n| (n.pane, n.read))
        .collect();
    assert_eq!(read, [(first, true), (second, false)]);
    let unread = list_notes(&mut control, true).await;
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].pane, second);

    server.stop().await;
}

/// `notes read` marks every note, or those up to an id, or one pane's — the
/// two narrowing together — and `notes clear` empties the history; each is
/// broadcast whole.
#[tokio::test]
async fn reading_and_clearing_the_history_broadcast_it() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let first = run_shell(&mut control).await;
    let second = run_shell(&mut control).await;
    let mut viewer = server.connect().await;
    attach(&mut viewer).await;
    let mut ids = Vec::new();
    for (pane, body) in [(first, "one"), (second, "two"), (first, "three")] {
        printf_osc(&mut control, pane, &format!("9;{body}")).await;
        ids.push(noted(&mut viewer).await.id);
    }
    let read = |up_to, pane| Request::NotesRead { up_to, pane };
    let flags = |notes: Vec<Note>| notes.iter().map(|n| n.read).collect::<Vec<_>>();

    assert_eq!(
        control.request(read(Some(ids[0]), None)).await,
        Response::Ok
    );
    assert_eq!(
        flags(notes_changed(&mut viewer).await),
        [true, false, false]
    );
    // Both narrow: the first pane's notes up to the second id are just the
    // one already read, so nothing moves and nothing is broadcast — the next
    // broadcast is the first pane's own read, reaching its third note.
    let narrowed = read(Some(ids[1]), Some(first));
    assert_eq!(control.request(narrowed).await, Response::Ok);
    assert_eq!(control.request(read(None, Some(first))).await, Response::Ok);
    assert_eq!(flags(notes_changed(&mut viewer).await), [true, false, true]);
    assert_eq!(control.request(read(None, None)).await, Response::Ok);
    assert_eq!(flags(notes_changed(&mut viewer).await), [true, true, true]);

    assert_eq!(control.request(Request::NotesClear).await, Response::Ok);
    assert!(notes_changed(&mut viewer).await.is_empty());
    assert!(list_notes(&mut control, false).await.is_empty());

    server.stop().await;
}

/// A bell says nothing but "look": a storm of them from one pane lands one
/// unread note, and a bell from the pane an attached client is looking at
/// lands none — while a notification there still lands, already read.
#[tokio::test]
async fn bells_land_once_and_never_from_the_pane_in_view() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let pane = run_shell(&mut control).await;
    let mut viewer = server.connect().await;
    attach(&mut viewer).await;

    let bells = Request::PaneSend {
        pane,
        text: Some("printf '\\a\\a\\a'; printf '\\033]9;storm\\007'\n".into()),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    assert_eq!(control.request(bells).await, Response::Ok);
    let bell = noted(&mut viewer).await;
    assert_eq!((bell.kind, bell.read), (NoteKind::Bell, false));
    assert_eq!(
        noted(&mut viewer).await.body.as_deref(),
        Some("storm"),
        "the two repeat bells landed nothing"
    );

    // The focus's reply follows its broadcasts and the server noting what
    // the viewer looks at.
    viewer.send(&Request::PaneFocus { pane, seen: false }).await;
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && serde_json::from_slice::<Response>(&json).is_ok_and(|r| r == Response::Ok)
            {
                return;
            }
        }
    })
    .await
    .expect("the focus was never answered");
    let watched = Request::PaneSend {
        pane,
        text: Some("printf '\\a'; printf '\\033]9;watched\\007'\n".into()),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    assert_eq!(control.request(watched).await, Response::Ok);
    let note = noted(&mut viewer).await;
    assert_eq!(
        (note.kind, note.body.as_deref(), note.read),
        (NoteKind::Notify, Some("watched"), true),
        "the bell in view landed nothing; the notification landed read"
    );
    assert_eq!(list_notes(&mut control, false).await.len(), 3);

    server.stop().await;
}

/// A herdr reporter's `notification.show` is a notification like any other:
/// it lands in the history, answered once it has.
#[tokio::test]
async fn a_herdr_notification_lands_in_the_history() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let pane = run_sleeper(&mut conn).await;
    let socket = server.path.with_extension("herdr.sock");
    let show = serde_json::json!({
        "id": "n1",
        "method": "notification.show",
        "params": {"pane_id": format!("w1:p{pane}"), "title": "crush", "body": "ready"},
    });
    assert_eq!(herdr(&socket, show).await["result"]["type"], "ok");
    let notes = list_notes(&mut conn, false).await;
    let landed: Vec<_> = notes
        .iter()
        .map(|n| (n.pane, n.kind, n.title.as_deref(), n.body.as_deref()))
        .collect();
    assert_eq!(
        landed,
        [(pane, NoteKind::Notify, Some("crush"), Some("ready"))]
    );

    server.stop().await;
}

/// A `notification.show` naming no pane lands on the pane whose process sent
/// it — here a zsh under the pane's shell, found through the socket's peer
/// pid — and one from outside every pane is refused as it always was. Skips
/// cleanly without `/bin/zsh`, whose `zsocket` speaks to the socket.
#[tokio::test]
async fn a_herdr_notification_naming_no_pane_lands_on_its_senders() {
    if !Path::new("/bin/zsh").exists() {
        eprintln!("skipping: no /bin/zsh");
        return;
    }
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let show = r#"{\"id\":\"n1\",\"method\":\"notification.show\",\"params\":{\"title\":\"crush\",\"body\":\"ready\"}}"#;
    let pane = run_marker(
        &mut conn,
        &format!(
            "/bin/zsh -f -c 'zmodload zsh/net/socket && zsocket \"$HERDR_SOCKET_PATH\" \
             && print -r -- \"{show}\" >&$REPLY && read -r reply <&$REPLY \
             && print -r -- \"REPLY $reply\"'; sleep 30"
        ),
    )
    .await;
    assert!(
        read_until(
            &mut conn,
            pane,
            r#"REPLY {"id":"n1","result":{"type":"ok"}}"#
        )
        .await,
        "the sender's pane took the notification"
    );
    let notes = list_notes(&mut conn, false).await;
    let landed: Vec<_> = notes
        .iter()
        .map(|n| (n.pane, n.kind, n.title.as_deref(), n.body.as_deref()))
        .collect();
    assert_eq!(
        landed,
        [(pane, NoteKind::Notify, Some("crush"), Some("ready"))]
    );

    let socket = server.path.with_extension("herdr.sock");
    let stray = serde_json::json!({"method": "notification.show", "params": {"title": "t"}});
    assert_eq!(
        herdr(&socket, stray).await["error"]["code"],
        "invalid_request",
        "this test's own process is in no pane"
    );
    assert_eq!(list_notes(&mut conn, false).await.len(), 1);

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
            event: AgentHookEvent::Blocked {
                message: None,
                reminder: false
            },
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
            between_turns: false,
            hooks: None,
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

/// Claude's dialog, answered in the pane, sends no hook: once the screen has
/// shown the dialog the hook blocked for, the screen showing the tool running
/// moves the pane to `Working`, still hook-driven.
#[tokio::test]
async fn a_dialog_answered_in_the_pane_ends_the_hooks_block() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
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
    assert!(wait_agent(&mut conn, pane, "claude").await);
    hook(
        &mut conn,
        pane,
        AgentHookEvent::Blocked {
            message: None,
            reminder: false,
        },
    )
    .await;
    let send = |text: String| Request::PaneSend {
        pane,
        text: Some(text),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    assert_eq!(
        conn.request(send("Do you want to proceed?\n".into())).await,
        Response::Ok
    );
    // A few classify passes (300ms each) see the dialog.
    sleep(Duration::from_millis(900)).await;
    assert!(wait_state(&mut conn, pane, AgentState::Blocked).await);
    // The dialog scrolls out of the classifier's tail, the tool runs.
    assert_eq!(
        conn.request(send(format!("{}Running\n", "\n".repeat(20))))
            .await,
        Response::Ok
    );
    assert!(
        wait_pane(&mut conn, pane, |p| p.state == AgentState::Working
            && p.source == Some(StateSource::Hook))
        .await,
        "the approved tool runs: working, and still the hook's"
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
            between_turns: false,
            hooks: None,
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    let history = list_notes(&mut control, false).await;
    let history: Vec<_> = history
        .iter()
        .map(|n| (n.kind, n.title.as_deref(), n.body.as_deref()))
        .collect();
    assert_eq!(
        history,
        [(NoteKind::Done, Some("build"), Some("Built"))],
        "the msg rides on the finish's own note: one note, not two"
    );

    // Typing past the finished report ends its claim, so the next one lands:
    // a reporter that never sends its clear no longer shuts 9;4 out for good.
    printf_osc(&mut control, pane, "9;4;1;10").await;
    assert!(
        wait_pane(&mut control, pane, |p| p.state == AgentState::Working
            && p.source == Some(StateSource::Progress))
        .await,
        "typing ended the finished 7501 claim, so 9;4 claims the pane"
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;

    let mut order = Vec::new();

    assert_eq!(
        control
            .request(Request::PaneSend {
                pane,
                text: Some("esc to interrupt\n".into()),
                keys: None,
                expect: None,
                between_turns: false,
                hooks: None,
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
                between_turns: false,
                hooks: None,
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
            event: AgentHookEvent::Blocked {
                message: None,
                reminder: false
            },
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
        between_turns: false,
        hooks: None,
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
                between_turns: false,
                hooks: None,
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
async fn mirror_until(viewer: &mut Conn, pane: PaneId, mirror: &mut Option<Terminal>, want: &str) {
    let rows = |term: &Terminal| -> Vec<String> {
        (0..term.size().0)
            .map(|r| term.screen_row(r).text())
            .collect()
    };
    let caught_up = timeout(DEADLINE, async {
        loop {
            match viewer.read_frame().await {
                Frame::PaneSnapshot(d) if d.pane == pane => {
                    let mut term = Terminal::new(d.rows, d.cols, 0);
                    term.advance(&d.bytes);
                    *mirror = Some(term);
                }
                Frame::PaneDelta(d) if d.pane == pane => mirror
                    .as_mut()
                    .expect("a delta arrived before the snapshot")
                    .advance(&d.bytes),
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
        conn.request(Request::PaneFocus { pane, seen: false }).await,
        Response::Ok
    );
    assert!(
        wait_state(&mut conn, pane, AgentState::Idle).await,
        "focusing a Done pane should transition it to Idle"
    );

    server.stop().await;
}

/// Focus `pane` from `conn`, an attached client, waiting for the answer.
async fn focus(conn: &mut Conn, pane: PaneId) {
    conn.send(&Request::PaneFocus { pane, seen: false }).await;
    read_until_control(conn, |r: &Response| *r == Response::Ok).await;
}

/// A view's focus is its own: a done pane it focuses stays done and an
/// unread note on one stays unread, for everybody; a bell from the pane it
/// looks at lands; it is "the" attached client only with no other. Yet the
/// daemon knows where it looks: a move past that pane's edge is handed to
/// it. An attached client's focus then marks both seen.
#[tokio::test]
async fn a_views_focus_marks_nothing_seen() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let run = Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        ephemeral: false,
        sandbox: None,
    };
    let done = pane_id(control.request(run).await);
    assert!(wait_state(&mut control, done, AgentState::Done).await);
    let shell = run_shell(&mut control).await;
    let other = run_shell(&mut control).await;
    printf_osc(&mut control, shell, "9;look").await;
    timeout(DEADLINE, async {
        while list_notes(&mut control, true).await.is_empty() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the note never landed");
    let attach = |view| Request::Attach {
        terminal: None,
        handoff: false,
        view,
        quiet: false,
    };
    let mut tui = server.connect().await;
    tui.send(&attach(false)).await;
    focus(&mut tui, other).await;
    let mut viewer = server.connect().await;
    viewer.send(&attach(true)).await;
    focus(&mut viewer, done).await;
    focus(&mut viewer, shell).await;

    let bell = Request::PaneSend {
        pane: shell,
        text: Some("printf '\\a'\n".into()),
        keys: None,
        expect: None,
        between_turns: false,
        hooks: None,
    };
    assert_eq!(control.request(bell).await, Response::Ok);
    assert_eq!(noted(&mut tui).await.kind, NoteKind::Bell);
    let unread = |notes: Vec<Note>| -> Vec<(PaneId, NoteKind)> {
        notes.iter().map(|n| (n.pane, n.kind)).collect()
    };
    assert_eq!(
        unread(list_notes(&mut control, true).await),
        [(shell, NoteKind::Notify), (shell, NoteKind::Bell)]
    );
    // Nor may it mark them read, or clear them, for everybody.
    for request in [
        Request::NotesRead {
            up_to: None,
            pane: None,
        },
        Request::NotesClear,
    ] {
        viewer.send(&request).await;
        let refused = read_until_control(&mut viewer, |r: &Response| {
            matches!(r, Response::Error { .. })
        })
        .await;
        let message = "a view marks no note read".to_string();
        assert_eq!(refused, Response::Error { message }, "{request:?}");
    }
    assert_eq!(list_notes(&mut control, true).await.len(), 2);
    assert!(wait_state(&mut control, done, AgentState::Done).await);
    let toward = |from| Request::PaneFocusToward {
        from,
        toward: Toward::Left,
        wrap: false,
    };
    let Response::Neighbor { from, relayed, .. } = control.request(toward(None)).await else {
        panic!("no neighbor answer");
    };
    assert_eq!(
        (from, relayed),
        (other, true),
        "the attached client's focus"
    );
    let Response::Neighbor { relayed, .. } = control.request(toward(Some(shell))).await else {
        panic!("no neighbor answer");
    };
    assert!(relayed, "the view looks at the pane");
    let edge = read_until_control(&mut viewer, |e: &Event| {
        matches!(e, Event::FocusEdge { .. })
    })
    .await;
    assert_eq!(
        edge,
        Event::FocusEdge {
            pane: shell,
            toward: Toward::Left
        }
    );

    focus(&mut tui, done).await;
    focus(&mut tui, shell).await;
    assert!(wait_state(&mut control, done, AgentState::Idle).await);
    assert!(list_notes(&mut control, true).await.is_empty());

    server.stop().await;
}

/// A view's focus that says someone looks at the pane (`seen`) sees it as a
/// client's focus does — done turns idle, its notes read — yet moves no pane
/// of the session's and relays no `PaneFocused` to the attached client
/// beside it. Without `seen` it sees nothing.
#[tokio::test]
async fn a_views_seen_focus_sees_but_moves_nothing() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let run = Request::PaneRun {
        tab: None,
        cmd: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '\\033]9;look\\007'; exit 0".into(),
        ],
        ephemeral: false,
        sandbox: None,
    };
    let done = pane_id(control.request(run).await);
    assert!(wait_state(&mut control, done, AgentState::Done).await);
    timeout(DEADLINE, async {
        while list_notes(&mut control, true).await.is_empty() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the note never landed");
    let other = run_shell(&mut control).await;
    let attach = |view| Request::Attach {
        terminal: None,
        handoff: false,
        view,
        quiet: false,
    };
    let mut tui = server.connect().await;
    tui.send(&attach(false)).await;
    focus(&mut tui, other).await;
    let mut viewer = server.connect().await;
    viewer.send(&attach(true)).await;

    focus(&mut viewer, done).await;
    assert!(wait_state(&mut control, done, AgentState::Done).await);
    assert_eq!(list_notes(&mut control, true).await.len(), 1);

    viewer
        .send(&Request::PaneFocus {
            pane: done,
            seen: true,
        })
        .await;
    read_until_control(&mut viewer, |r: &Response| *r == Response::Ok).await;
    assert!(wait_state(&mut control, done, AgentState::Idle).await);
    assert!(list_notes(&mut control, true).await.is_empty());
    let Response::View { workspaces } = control.request(Request::View).await else {
        panic!("no view");
    };
    let current: Vec<_> = workspaces
        .iter()
        .flat_map(|w| &w.tabs)
        .filter(|t| t.active)
        .map(|t| t.active_pane)
        .collect();
    assert_eq!(current, [Some(other)], "the session's focus moved");
    // Anything the focus relayed to the TUI comes before this answer.
    tui.send(&Request::View).await;
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = tui.read_frame().await {
                let relayed = serde_json::from_slice::<Event>(&json);
                assert!(
                    !matches!(relayed, Ok(Event::PaneFocused { .. })),
                    "a view's focus was relayed"
                );
                if let Ok(Response::View { .. }) = serde_json::from_slice(&json) {
                    return;
                }
            }
        }
    })
    .await
    .expect("the TUI's view was never answered");

    server.stop().await;
}

/// Snapshots off a watcher until one satisfies `want`, returning it as
/// `(attached_terminal, panes)`. A watcher must be sent nothing else — no pane
/// frame, no event — so anything else fails the test.
async fn snapshot_until(
    watcher: &mut Conn,
    want: impl Fn(&Option<String>, &[WatchPane]) -> bool,
) -> (Option<String>, Vec<WatchPane>) {
    timeout(DEADLINE, async {
        loop {
            let Frame::Control(json) = watcher.read_frame().await else {
                panic!("a watcher was sent a pane frame");
            };
            match serde_json::from_slice(&json).unwrap() {
                Event::Snapshot {
                    session,
                    wire_rev,
                    attached_terminal,
                    panes,
                } => {
                    assert_eq!((session.as_str(), wire_rev), ("s", tutti_core::WIRE_REV));
                    if want(&attached_terminal, &panes) {
                        return (attached_terminal, panes);
                    }
                }
                other => panic!("a watcher was sent {other:?}"),
            }
        }
    })
    .await
    .expect("no snapshot matched within the deadline")
}

/// A watcher gets a whole-session snapshot on connect and another on each
/// change, beside an attached client it never disturbs: the client still gets
/// its frames and events, the snapshot carries the client's terminal and
/// focus while it is attached, and a `Done` stays unseen however many watchers
/// come and go.
#[tokio::test]
async fn a_watcher_snapshots_the_session_beside_an_attached_client() {
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

    let mut watcher = server.connect().await;
    watcher.send(&Request::Watch).await;
    let (terminal, panes) = snapshot_until(&mut watcher, |_, _| true).await;
    assert_eq!(terminal, None, "nobody is attached yet");
    let dir = std::env::temp_dir();
    assert_eq!(
        panes,
        vec![WatchPane {
            id: pane,
            workspace: dir.file_name().unwrap().to_string_lossy().into_owned(),
            dir,
            tab: "cat".into(),
            title: "cat".into(),
            agent: None,
            state: AgentState::Unknown,
            source: None,
            exited: None,
            focused: false,
            user_vars: Default::default(),
            cwd: None,
            approval: None,
            issue: None,
        }]
    );

    let mut viewer = server.connect().await;
    viewer
        .send(&Request::Attach {
            terminal: Some("com.mitchellh.ghostty".into()),
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
    viewer.send(&Request::PaneFocus { pane, seen: false }).await;
    snapshot_until(&mut watcher, |terminal, panes| {
        terminal.as_deref() == Some("com.mitchellh.ghostty") && panes[0].focused
    })
    .await;

    for (event, state) in [
        (
            AgentHookEvent::Activity { detail: None },
            AgentState::Working,
        ),
        (AgentHookEvent::Done { reply: None }, AgentState::Done),
    ] {
        assert_eq!(
            control
                .request(Request::AgentEvent {
                    pane,
                    event,
                    conversation: None,
                })
                .await,
            Response::Ok
        );
        snapshot_until(&mut watcher, |_, panes| {
            panes[0].state == state && panes[0].source == Some(StateSource::Hook)
        })
        .await;
        wait_state_event(&mut viewer, pane, state, &mut Vec::new()).await;
    }

    let mut second = server.connect().await;
    second.send(&Request::Watch).await;
    let (_, panes) = snapshot_until(&mut second, |_, _| true).await;
    assert_eq!(panes[0].state, AgentState::Done, "watching is not viewing");

    viewer.send(&Request::Detach).await;
    let (_, panes) = snapshot_until(&mut watcher, |terminal, _| terminal.is_none()).await;
    assert!(!panes[0].focused, "nobody attached, nothing focused");
    assert_eq!(panes[0].state, AgentState::Done);

    server.stop().await;
}

/// A watcher's snapshot carries each pane's reported directory and the
/// approval its agent holds, as `pane list` does, so a tool reads both from
/// one place; `pane list` names each pane's workspace.
#[tokio::test]
async fn a_snapshot_carries_cwd_and_the_held_approval_and_the_list_the_workspace() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    let ws = new_workspace(&mut control).await;
    let shell = Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/sh".into()],
        ephemeral: false,
        sandbox: None,
    };
    let pane = pane_id(control.request(shell).await);
    assert!(
        wait_pane(&mut control, pane, |p| p.workspace == Some(ws)).await,
        "the list names the pane's workspace"
    );

    let mut watcher = server.connect().await;
    watcher.send(&Request::Watch).await;
    printf_osc(&mut control, pane, "7;file://host/srv/api").await;
    snapshot_until(&mut watcher, |_, panes| {
        panes[0].cwd.as_deref() == Some("/srv/api")
    })
    .await;

    let _viewer = attached_viewer(&server).await;
    let mut hook = server.connect().await;
    hook.send(&hold(pane)).await;
    let (_, panes) = snapshot_until(&mut watcher, |_, panes| panes[0].approval.is_some()).await;
    let held = panes[0].approval.clone().unwrap();
    assert_eq!(
        (held.tool.as_str(), held.summary.as_str()),
        ("Bash", "rm -rf build")
    );
    let answer = Request::ApprovalAnswer {
        pane,
        id: Some(held.id),
        decision: ApprovalDecision::Allow,
    };
    assert_eq!(control.request(answer).await, Response::Ok);
    assert_eq!(approval(hook.response().await), ApprovalDecision::Allow);
    snapshot_until(&mut watcher, |_, panes| panes[0].approval.is_none()).await;

    server.stop().await;
}

/// A script's `pane focus` is relayed to the attached client as `PaneFocused`
/// so it can jump there; the client's own focus reports are never echoed.
#[tokio::test]
async fn a_scripted_focus_is_relayed_to_the_attached_client() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let mut panes = Vec::new();
    for _ in 0..2 {
        let run = Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/cat".into()],
            ephemeral: false,
            sandbox: None,
        };
        panes.push(pane_id(control.request(run).await));
    }
    let (own, scripted) = (panes[0], panes[1]);

    let mut viewer = server.connect().await;
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
    viewer
        .send(&Request::PaneFocus {
            pane: own,
            seen: false,
        })
        .await;
    let relayed = |json: &[u8]| match serde_json::from_slice(json) {
        Ok(Event::PaneFocused { pane }) => Some(pane),
        _ => None,
    };
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await {
                assert_eq!(relayed(&json), None, "the client's own focus was echoed");
                if serde_json::from_slice::<Response>(&json).is_ok_and(|r| r == Response::Ok) {
                    return;
                }
            }
        }
    })
    .await
    .expect("the focus was never answered");

    assert_eq!(
        control
            .request(Request::PaneFocus {
                pane: scripted,
                seen: false
            })
            .await,
        Response::Ok
    );
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = viewer.read_frame().await
                && let Some(pane) = relayed(&json)
            {
                assert_eq!(pane, scripted);
                return;
            }
        }
    })
    .await
    .expect("the scripted focus was never relayed");

    server.stop().await;
}

/// Read control frames off `conn` until one parses as `want`.
async fn read_until_control<T: serde::de::DeserializeOwned>(
    conn: &mut Conn,
    want: impl Fn(&T) -> bool,
) -> T {
    timeout(DEADLINE, async {
        loop {
            if let Frame::Control(json) = conn.read_frame().await
                && let Ok(found) = serde_json::from_slice::<T>(&json)
                && want(&found)
            {
                return found;
            }
        }
    })
    .await
    .expect("the awaited control frame never came")
}

/// A directional focus moves to the neighbor in its source pane's tab — the
/// pane named, else the attached client's focused one — relayed like a
/// scripted `pane focus`; at an edge it names no pane and moves nothing,
/// unless it wraps. A directional resize nudges the split a direct key would.
#[tokio::test]
async fn directional_focus_and_resize_start_from_the_named_or_attached_pane() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let run = Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/cat".into()],
        ephemeral: false,
        sandbox: None,
    };
    let left = pane_id(control.request(run).await);
    let split = Request::PaneSplit {
        pane: left,
        direction: Direction::Horizontal,
    };
    let right = pane_id(control.request(split).await);
    let focus = |from, toward, wrap| Request::PaneFocusToward { from, toward, wrap };
    let moved = |from, toward, pane| Response::Neighbor {
        from,
        toward,
        pane,
        relayed: false,
    };
    assert!(
        matches!(
            control.request(focus(None, Toward::Right, false)).await,
            Response::Error { .. }
        ),
        "no pane named and nobody attached"
    );

    let mut viewer = server.connect().await;
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
    viewer
        .send(&Request::PaneFocus {
            pane: left,
            seen: false,
        })
        .await;
    read_until_control(&mut viewer, |r: &Response| *r == Response::Ok).await;

    assert_eq!(
        control.request(focus(None, Toward::Right, false)).await,
        moved(left, Toward::Right, Some(right))
    );
    read_until_control(&mut viewer, |e: &Event| {
        *e == Event::PaneFocused { pane: right }
    })
    .await;
    assert_eq!(
        control
            .request(focus(Some(right), Toward::Right, false))
            .await,
        moved(right, Toward::Right, None),
        "the right edge"
    );
    assert_eq!(
        control
            .request(focus(Some(right), Toward::Right, true))
            .await,
        moved(right, Toward::Right, Some(left)),
        "wrapping round it"
    );
    assert!(matches!(
        control
            .request(focus(Some(PaneId(99)), Toward::Left, false))
            .await,
        Response::Error { .. }
    ));

    // The viewer never reported the relayed focus, so it still has `left`.
    let resize = Request::PaneResizeToward {
        from: None,
        toward: Toward::Left,
        delta: 0.05,
    };
    assert_eq!(control.request(resize).await, Response::Ok);
    let ratio = next_split_ratio(&mut viewer).await;
    assert!(
        (ratio - 0.45).abs() < 1e-3,
        "left moves the border left: {ratio}"
    );

    // Once the viewer shows `right`, its right edge is the viewer's to take
    // on — sideways only, and never instead of a wrap.
    viewer
        .send(&Request::PaneFocus {
            pane: right,
            seen: false,
        })
        .await;
    read_until_control(&mut viewer, |r: &Response| *r == Response::Ok).await;
    assert_eq!(
        control
            .request(focus(Some(right), Toward::Right, false))
            .await,
        Response::Neighbor {
            from: right,
            toward: Toward::Right,
            pane: None,
            relayed: true,
        }
    );
    read_until_control(&mut viewer, |e: &Event| {
        *e == Event::FocusEdge {
            pane: right,
            toward: Toward::Right,
        }
    })
    .await;
    assert_eq!(
        control.request(focus(Some(right), Toward::Up, false)).await,
        moved(right, Toward::Up, None),
        "an upper edge stays an edge"
    );
    assert_eq!(
        control
            .request(focus(Some(left), Toward::Left, false))
            .await,
        moved(left, Toward::Left, None),
        "nobody shows `left` focused"
    );
    assert_eq!(
        control
            .request(focus(Some(right), Toward::Right, true))
            .await,
        moved(right, Toward::Right, Some(left)),
        "a wrap still wraps"
    );

    server.stop().await;
}

/// A client that hands moves on (rtty's, `Attach`'s `handoff`) is handed the
/// top and bottom edges too, and a resize no split of tutti's takes: the
/// editor's move goes on through tutti to the terminal tutti runs in. A
/// resize a split takes stays tutti's.
#[tokio::test]
async fn a_handoff_client_takes_every_edge_and_unsplit_resizes() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let run = Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/cat".into()],
        ephemeral: false,
        sandbox: None,
    };
    let left = pane_id(control.request(run).await);
    let split = Request::PaneSplit {
        pane: left,
        direction: Direction::Horizontal,
    };
    let right = pane_id(control.request(split).await);
    let mut viewer = server.connect().await;
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: true,
            view: false,
            quiet: false,
        })
        .await;
    viewer
        .send(&Request::PaneFocus {
            pane: right,
            seen: false,
        })
        .await;
    read_until_control(&mut viewer, |r: &Response| *r == Response::Ok).await;

    let up = Request::PaneFocusToward {
        from: Some(right),
        toward: Toward::Up,
        wrap: false,
    };
    assert_eq!(
        control.request(up).await,
        Response::Neighbor {
            from: right,
            toward: Toward::Up,
            pane: None,
            relayed: true,
        }
    );
    read_until_control(&mut viewer, |e: &Event| {
        *e == Event::FocusEdge {
            pane: right,
            toward: Toward::Up,
        }
    })
    .await;

    let resize = |toward| Request::PaneResizeToward {
        from: Some(right),
        toward,
        delta: 0.05,
    };
    assert_eq!(control.request(resize(Toward::Down)).await, Response::Ok);
    read_until_control(&mut viewer, |e: &Event| {
        *e == Event::ResizeEdge {
            pane: right,
            toward: Toward::Down,
        }
    })
    .await;
    assert_eq!(control.request(resize(Toward::Left)).await, Response::Ok);
    let ratio = next_split_ratio(&mut viewer).await;
    assert!((ratio - 0.45).abs() < 1e-3, "tutti's own split: {ratio}");

    server.stop().await;
}

/// A pane's OSC 1337 user vars land on its `PaneInfo` — in `pane list`, the
/// attached client's view and a watcher's snapshot — and an empty value
/// unsets one.
#[tokio::test]
async fn a_panes_user_vars_reach_its_listing_view_and_watchers() {
    let server = TestServer::start();
    let mut control = server.connect().await;
    new_workspace(&mut control).await;
    let run = Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/sh".into()],
        ephemeral: false,
        sandbox: None,
    };
    let pane = pane_id(control.request(run).await);
    let mut viewer = server.connect().await;
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
    let mut watcher = server.connect().await;
    watcher.send(&Request::Watch).await;
    let keys = |p: &PaneInfo| p.user_vars.get("TUTTI_KEYS").cloned();

    printf_osc(&mut control, pane, "1337;SetUserVar=TUTTI_KEYS=cGFuZQ==").await;
    assert!(
        wait_pane(&mut control, pane, |p| keys(p).as_deref() == Some("pane")).await,
        "pane list carries the var"
    );
    read_until_control(&mut viewer, |e: &Event| match e {
        Event::LayoutChanged { workspaces } => workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .any(|p| p.id == pane && keys(p).as_deref() == Some("pane")),
        _ => false,
    })
    .await;
    snapshot_until(&mut watcher, |_, panes| {
        panes[0].user_vars.get("TUTTI_KEYS").map(String::as_str) == Some("pane")
    })
    .await;

    printf_osc(&mut control, pane, "1337;SetUserVar=TUTTI_KEYS=").await;
    assert!(
        wait_pane(&mut control, pane, |p| p.user_vars.is_empty()).await,
        "an empty value unsets it"
    );
    snapshot_until(&mut watcher, |_, panes| panes[0].user_vars.is_empty()).await;

    // An editor that died holding the keys: the shell's next prompt frees
    // them, and frees nothing else.
    printf_osc(&mut control, pane, "1337;SetUserVar=TUTTI_KEYS=cGFuZQ==").await;
    printf_osc(&mut control, pane, "1337;SetUserVar=OTHER=eA==").await;
    assert!(wait_pane(&mut control, pane, |p| p.user_vars.len() == 2).await);
    printf_osc(&mut control, pane, "133;A").await;
    assert!(
        wait_pane(&mut control, pane, |p| keys(p).is_none()
            && p.user_vars.get("OTHER").map(String::as_str) == Some("x"))
        .await,
        "a prompt start unsets the editor vars only"
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    second
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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

/// A `workspace add` whose `-r` names no commit — a missing revision, or a
/// revset resolving to none — is refused before jj makes anything: no
/// checkout on disk and no jj workspace, so the same name then works. Skips
/// without `jj`.
#[tokio::test]
async fn fork_at_a_revision_naming_no_commit_leaves_nothing() {
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
    let fork = |revision: &str| Request::WorkspaceFork {
        id: ws,
        name: "feature".into(),
        revision: Some(revision.into()),
        dest: None,
    };
    let missing = error_message(conn.request(fork("no-such-rev")).await);
    assert!(missing.contains("doesn't exist"), "{missing}");
    let none = error_message(conn.request(fork("none()")).await);
    assert!(none.contains("names no commit"), "{none}");
    let dest = fork_sibling(&origin, "feature");
    assert!(!dest.exists(), "a checkout was left at {}", dest.display());
    let listed = jj_out(&origin, &["workspace", "list"]);
    assert!(!listed.contains("feature:"), "{listed}");

    workspace_id(conn.request(fork("root()")).await);
    assert!(dest.join(".jj").exists());

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&dest);
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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

/// A workspace forgotten outside tutti and added again elsewhere under the
/// same name is someone else's: discarding tutti's checkout removes that
/// checkout and forgets nothing. Skips without `jj`.
#[tokio::test]
async fn discard_never_forgets_a_same_named_workspace_added_since() {
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
    assert!(
        run_jj(&origin, &["workspace", "forget", "gone"])
            .status
            .success()
    );
    let parent = fresh_dir("theirs");
    let theirs = parent.join("gone");
    let add = [
        "workspace",
        "add",
        "--name",
        "gone",
        theirs.to_str().unwrap(),
    ];
    assert!(run_jj(&origin, &add).status.success());

    assert_eq!(
        conn.request(Request::WorkspaceKill {
            id: fork,
            discard: true,
        })
        .await,
        Response::Ok
    );
    assert!(!dest.exists(), "discard should remove tutti's checkout");
    let listed = jj_out(&origin, &["workspace", "list"]);
    assert!(listed.contains("gone:"), "theirs was forgotten: {listed}");
    assert!(theirs.join(".jj").exists(), "theirs was touched");

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&parent);
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

/// `SandboxPull` fetches the branch a sandbox pane's sandbox pushed —
/// `tutti/<its name>` — from the project's remote and checks it out as a
/// workspace nested under the project, named after the sandbox; it refuses a
/// pane that is not a sandbox's, a repo with no remote, and a branch not yet
/// pushed. The sandbox pane runs a stand-in for the `tutti` wrapper — the
/// server looks beside its own executable, here this test binary — that exits
/// at once: no cluster involved. Skips without `jj`.
#[tokio::test]
async fn sandbox_pull_checks_out_the_branch_the_sandbox_pushed() {
    if !jj_on_path() {
        return;
    }
    let remote = jj_git_init("sbremote");
    std::fs::write(remote.join("agent.txt"), "from the sandbox\n").unwrap();
    assert!(
        run_jj(&remote, &["commit", "-m", "sandbox work"])
            .status
            .success()
    );
    let local = jj_git_init("sblocal");
    // Kept, never removed: without it the server would fall back to a `tutti`
    // on PATH — the user's real install — for any sandbox pane a test spawns.
    let wrapper = std::env::current_exe().unwrap().with_file_name("tutti");
    match std::os::unix::fs::symlink("/usr/bin/true", &wrapper) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            panic!("placing the wrapper stand-in at {}: {e}", wrapper.display())
        }
        _ => {}
    }
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let project = workspace_id(
        conn.request(Request::WorkspaceNew { dir: local.clone() })
            .await,
    );
    let run = |cmd: &str, sandbox: Option<&str>| Request::PaneRun {
        tab: None,
        cmd: vec![cmd.into()],
        ephemeral: false,
        sandbox: sandbox.map(Into::into),
    };

    let shell = pane_id(conn.request(run("/bin/sh", None)).await);
    let refused = Request::SandboxPull {
        pane: shell,
        name: None,
    };
    assert!(error_message(conn.request(refused).await).contains("not a sandbox pane"));
    let pane = pane_id(conn.request(run("claude", Some("orb"))).await);
    let name = match conn.request(Request::PaneList).await {
        Response::Panes { panes } => panes.into_iter().find(|p| p.id == pane),
        other => panic!("expected Panes, got {other:?}"),
    }
    .and_then(|p| p.sandbox_name)
    .expect("a sandbox pane carries its sandbox's name");
    assert!(name.starts_with(&format!("tutti-p{pane}-")), "{name}");
    let pull = Request::SandboxPull { pane, name: None };
    let message = error_message(conn.request(pull.clone()).await);
    assert!(message.contains("no git remote"), "{message}");
    let url = remote.join(".git");
    let added = run_jj(
        &local,
        &["git", "remote", "add", "origin", url.to_str().unwrap()],
    );
    assert!(added.status.success());
    let branch = format!("tutti/{name}");
    let message = error_message(conn.request(pull.clone()).await);
    assert!(
        message.contains(&branch) && message.contains("has the sandbox pushed it?"),
        "not pushed yet: {message}"
    );
    assert!(
        !fork_sibling(&local, &name).exists(),
        "a branch the remote lacks checks nothing out"
    );

    assert!(
        run_jj(&remote, &["bookmark", "create", &branch, "-r", "@-"])
            .status
            .success()
    );
    let pulled = workspace_id(conn.request(pull).await);
    let dest = fork_sibling(&local, &name);
    assert_eq!(
        std::fs::read_to_string(dest.join("agent.txt")).unwrap(),
        "from the sandbox\n",
        "the checkout holds the sandbox's work"
    );
    match conn.request(Request::WorkspaceList).await {
        Response::Workspaces { workspaces } => assert!(
            workspaces
                .iter()
                .any(|w| w.id == pulled && w.parent == Some(project)),
            "nested under the project: {workspaces:?}"
        ),
        other => panic!("expected Workspaces, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&remote);
    let _ = std::fs::remove_dir_all(&local);
    let _ = std::fs::remove_dir_all(&dest);
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

/// A turn still in flight when the daemon stops keeps its prompt: the restore
/// closes it where its pane comes back, as a `restart` checkpoint listed under
/// that prompt. Skips cleanly without `jj`.
#[tokio::test]
async fn a_turn_in_flight_at_a_restart_keeps_its_prompt() {
    if !jj_on_path() {
        eprintln!("skipping: jj is not on PATH");
        return;
    }
    let (dir, state) = (jj_git_init("inflight"), fresh_dir("state"));
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    let server = TestServer::saving(Some(state.clone()));
    let mut conn = server.connect().await;
    workspace_id(
        conn.request(Request::WorkspaceNew { dir: dir.clone() })
            .await,
    );
    let pane = pane_id(conn.request(sleeper_in(None)).await);
    let text = Some("rename the module".to_string());
    hook(&mut conn, pane, AgentHookEvent::Prompt { text, id: None }).await;
    wait_checkpoints(&mut conn, pane, 1).await;
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    server.stop().await;

    let server = TestServer::saving(Some(state.clone()));
    let mut conn = server.connect().await;
    assert!(matches!(
        conn.request(Request::SessionRestore { discard: false })
            .await,
        Response::SessionRestored { panes: 1, .. }
    ));
    let pane = pane_ids(&mut conn).await[0];
    let turn = wait_checkpoints(&mut conn, pane, 2).await.remove(1);
    assert_eq!(
        (turn.n, turn.label.as_str(), turn.prompt.as_deref()),
        (2, "restart", Some("rename the module"))
    );
    assert_eq!((turn.files, turn.insertions), (1, 1));

    server.stop().await;
    for dir in [dir, state] {
        let _ = std::fs::remove_dir_all(dir);
    }
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
    hook(&mut conn, pane, AgentHookEvent::Prompt { text, id: None }).await;
    let start = wait_checkpoints(&mut conn, pane, 1).await.remove(0);
    assert_eq!((start.n, start.label.as_str()), (1, "start"));
    assert_eq!(start.from, None, "nothing was recorded before the start");
    assert_eq!(start.prompt, None, "the baseline predates the prompt");

    std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(dir.join("b.txt"), "bee\n").unwrap();
    hook(&mut conn, pane, AgentHookEvent::Done { reply: None }).await;
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
    viewer
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
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

/// A turn interrupted with Esc sends no Stop: the next prompt — a new
/// `prompt_id` with none since — closes it as `interrupted` under its own
/// prompt, its edits apart from the next turn's. More typed into a running
/// turn (its own `prompt_id`) carries on in it. Skips cleanly without `jj`.
#[tokio::test]
async fn the_next_prompt_closes_an_interrupted_turn_under_its_own() {
    if !jj_on_path() {
        eprintln!("skipping: jj is not on PATH");
        return;
    }
    let dir = jj_git_init("intr");
    let server = TestServer::start();
    let mut conn = server.connect().await;
    workspace_id(
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
    let prompt = |text: &str, id: &str| AgentHookEvent::Prompt {
        text: Some(text.into()),
        id: Some(id.into()),
    };

    hook(&mut conn, pane, prompt("add a", "p1")).await;
    wait_checkpoints(&mut conn, pane, 1).await;
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    hook(&mut conn, pane, prompt("and b too", "p1")).await;
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    // Esc: no Stop. The next prompt closes the turn.
    hook(&mut conn, pane, prompt("now add c", "p2")).await;
    let interrupted = wait_checkpoints(&mut conn, pane, 2).await.remove(1);
    assert_eq!(interrupted.label, "interrupted");
    assert_eq!(
        interrupted.prompt.as_deref(),
        Some("add a"),
        "under the turn's first prompt, the one typed into it not a new turn"
    );
    assert_eq!(interrupted.files, 2);

    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    hook(&mut conn, pane, AgentHookEvent::Done { reply: None }).await;
    let done = wait_checkpoints(&mut conn, pane, 3).await.remove(2);
    assert_eq!(
        (done.label.as_str(), done.prompt.as_deref(), done.files),
        ("done", Some("now add c"), 1),
        "the next turn holds its own edits only"
    );

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

/// The answer to an attach names the daemon's revision and its floor, the
/// floor in a field a client from before floors reads past. By the two, a
/// client as old as the floor attaches and goes on, and one below it
/// refuses, naming both ends and the one to update.
#[tokio::test]
async fn a_client_at_the_daemons_floor_attaches_and_one_below_it_refuses() {
    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum BeforeFloors {
        Attached { session: String, wire_rev: u32 },
    }

    let server = TestServer::start();
    let mut client = server.connect().await;
    client
        .send(&Request::Attach {
            terminal: None,
            handoff: false,
            view: false,
            quiet: false,
        })
        .await;
    let Frame::Control(json) = client.read_frame().await else {
        panic!("expected the answer to the attach");
    };
    let BeforeFloors::Attached { session, wire_rev } = serde_json::from_slice(&json).unwrap();
    assert_eq!((session.as_str(), wire_rev), ("s", WIRE_REV));
    let (rev, floor) = attached_wire(&json).expect("the answer's wire");
    assert_eq!((rev, floor), (WIRE_REV, Some(WIRE_FLOOR)));

    let client_at = |client| {
        Wire {
            rev: client,
            floor: client,
        }
        .accepts("the client", rev, floor)
    };
    assert_eq!(client_at(WIRE_FLOOR), Ok(()));
    assert_eq!(
        client.request(Request::PaneList).await,
        Response::Panes { panes: Vec::new() },
        "attached, it goes on"
    );
    let below = WIRE_FLOOR - 1;
    assert_eq!(
        client_at(below).unwrap_err(),
        format!(
            "tutti's daemon speaks wire revision {WIRE_REV} (floor {WIRE_FLOOR}), \
             the client {below} (floor {below}): update the client"
        )
    );
    server.stop().await;
}

/// What a newer client counts on from an older daemon: a request the daemon
/// does not know, or one it knows carrying a value it does not, is refused
/// under the request's `req`; a field it does not know is read past; and the
/// connection goes on.
#[tokio::test]
async fn a_request_the_daemon_does_not_know_is_refused_and_a_field_read_past() {
    let server = TestServer::start();
    let mut conn = server.connect().await;
    let control = |json: &str| Frame::Control(json.as_bytes().to_vec());
    for unknown in [
        r#"{"req":7,"type":"pane_teleport","pane":1}"#,
        r#"{"req":7,"type":"pane_wait","pane":1,"until":["asleep"]}"#,
    ] {
        conn.write_frame(&control(unknown)).await;
        let Frame::Control(json) = conn.read_frame().await else {
            panic!("expected a refusal");
        };
        let refusal: Tagged<Response> = serde_json::from_slice(&json).unwrap();
        assert_eq!(refusal.req, Some(7));
        assert!(
            matches!(&refusal.body, Response::Error { message } if message.starts_with("bad request")),
            "{refusal:?}"
        );
    }
    conn.write_frame(&control(r#"{"type":"pane_list","sorted":true}"#))
        .await;
    assert_eq!(conn.response().await, Response::Panes { panes: Vec::new() });
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
    hook(&mut conn, pane, AgentHookEvent::Done { reply: None }).await;
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
