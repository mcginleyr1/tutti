//! The daemon: a `UnixListener` fronting shared `Session` state. Each accepted
//! connection is a request/response loop; a connection that sends `Attach` also
//! joins the broadcast set and receives pane snapshots, coalesced deltas, and
//! control events until it detaches or disconnects.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use tutti_agents::{ProcessTree, Registry};
use tutti_core::{
    AgentKind, Direction, Event, Frame, PaneData, PaneId, Request, Response, StateEvent,
};

use crate::keys;
use crate::pty::{PaneSize, PtyPane};
use crate::session::Session;

/// Cadence of the render tick: coalesced pane deltas to attached clients.
const TICK: Duration = Duration::from_millis(16);
/// Cadence of the agent-detection pass. Process trees change rarely, so this is
/// deliberately slow and kept off the render tick.
const DETECT_INTERVAL: Duration = Duration::from_secs(1);
/// Cadence of the state-classification pass over agent panes.
const CLASSIFY_INTERVAL: Duration = Duration::from_millis(300);

struct Client {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Panes this client has already been sent a snapshot for; the rest of its
    /// panes fall into the shared delta cadence.
    seen: HashSet<PaneId>,
}

pub struct Hub {
    session: Mutex<Session>,
    clients: Mutex<HashMap<u64, Client>>,
    /// Last screen broadcast per pane, with its running delta sequence number.
    last: Mutex<HashMap<PaneId, (vt100::Screen, u32)>>,
    next_client: AtomicU64,
    /// Session name, reported to attaching clients. Derived from the socket file.
    name: String,
    /// The agent registry driving detection and state classification.
    registry: Registry,
}

/// Bootstrap the socket, install a SIGTERM/SIGINT shutdown, and serve until it
/// fires. This is the binary's entry point.
pub async fn run(socket_path: PathBuf, size: PaneSize) -> Result<()> {
    prepare_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    let shutdown = async {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
    };
    serve(listener, socket_path, size, shutdown).await
}

/// Serve connections on `listener` until `shutdown` resolves, then kill every
/// pane and remove the socket file.
pub async fn serve(
    listener: UnixListener,
    socket_path: PathBuf,
    size: PaneSize,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    // The listener is already bound; advertise our pid so `tutti server stop`
    // can find us. Sits beside the socket: `<session>.sock` -> `<session>.pid`.
    let pid_path = socket_path.with_extension("pid");
    std::fs::write(&pid_path, std::process::id().to_string())
        .with_context(|| format!("write pidfile {}", pid_path.display()))?;

    let name = socket_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tutti".to_string());
    let hub = Arc::new(Hub {
        session: Mutex::new(Session::new(size)),
        clients: Mutex::new(HashMap::new()),
        last: Mutex::new(HashMap::new()),
        next_client: AtomicU64::new(0),
        name,
        registry: Registry::default(),
    });

    let tick_hub = Arc::clone(&hub);
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            broadcast_tick(&tick_hub);
        }
    });

    let detect_hub = Arc::clone(&hub);
    let detector = tokio::spawn(async move {
        let mut interval = tokio::time::interval(DETECT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            detect_pass(&detect_hub).await;
        }
    });

    let classify_hub = Arc::clone(&hub);
    let classifier = tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLASSIFY_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_gen: HashMap<PaneId, u64> = HashMap::new();
        loop {
            interval.tick().await;
            classify_pass(&classify_hub, &mut last_gen);
        }
    });

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept failed")?;
                let hub = Arc::clone(&hub);
                tokio::spawn(async move {
                    if let Err(err) = handle_conn(hub, stream).await {
                        eprintln!("connection error: {err:#}");
                    }
                });
            }
        }
    }

    ticker.abort();
    detector.abort();
    classifier.abort();
    hub.session.lock().expect("session poisoned").kill_all();
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

fn prepare_socket(path: &Path) -> Result<()> {
    let dir = path
        .parent()
        .context("socket path has no parent directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create socket dir {}", dir.display()))?;
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("a tutti server is already listening on {}", path.display());
        }
        std::fs::remove_file(path)
            .with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    Ok(())
}

async fn handle_conn(hub: Arc<Hub>, stream: tokio::net::UnixStream) -> Result<()> {
    let cid = hub.next_client.fetch_add(1, Ordering::Relaxed);
    let (mut read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(writer_loop(write_half, rx));

    let mut buf = Vec::new();
    while let Some(frame) = read_frame(&mut read_half, &mut buf).await? {
        handle_frame(&hub, cid, &tx, frame);
    }

    hub.clients.lock().expect("clients poisoned").remove(&cid);
    drop(tx);
    let _ = writer.await;
    Ok(())
}

async fn writer_loop(mut half: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if half.write_all(&bytes).await.is_err() {
            break;
        }
    }
}

async fn read_frame(reader: &mut OwnedReadHalf, buf: &mut Vec<u8>) -> Result<Option<Frame>> {
    loop {
        if let Some((frame, consumed)) = Frame::decode(buf).context("frame decode")? {
            buf.drain(..consumed);
            return Ok(Some(frame));
        }
        let mut chunk = [0u8; 8192];
        let n = reader.read(&mut chunk).await.context("socket read")?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn handle_frame(hub: &Arc<Hub>, cid: u64, tx: &mpsc::UnboundedSender<Vec<u8>>, frame: Frame) {
    match frame {
        Frame::Control(json) => match serde_json::from_slice::<Request>(&json) {
            // Send the Attached view before joining the broadcast set so it
            // always precedes the pane snapshots the next tick pushes here.
            Ok(Request::Attach) => {
                let workspaces = hub.session.lock().expect("session poisoned").view();
                let _ = tx.send(encode_json(&Response::Attached {
                    session: hub.name.clone(),
                    workspaces,
                }));
                hub.clients.lock().expect("clients poisoned").insert(
                    cid,
                    Client {
                        tx: tx.clone(),
                        seen: HashSet::new(),
                    },
                );
            }
            Ok(Request::Detach) => {
                hub.clients.lock().expect("clients poisoned").remove(&cid);
                let _ = tx.send(encode_json(&Response::Ok));
            }
            Ok(Request::PaneScroll { pane, offset }) => scroll(hub, cid, tx, pane, offset),
            Ok(request) => {
                let response = dispatch(hub, request);
                let _ = tx.send(encode_json(&response));
            }
            Err(err) => {
                let _ = tx.send(encode_json(&Response::Error {
                    message: format!("bad request: {err}"),
                }));
            }
        },
        Frame::Input { pane, bytes } => {
            let _ = hub
                .session
                .lock()
                .expect("session poisoned")
                .pane_send(pane, &bytes);
        }
        Frame::PaneSnapshot(_) | Frame::PaneDelta(_) => {}
    }
}

fn dispatch(hub: &Arc<Hub>, request: Request) -> Response {
    match request {
        Request::WorkspaceNew { dir } => {
            let id = hub
                .session
                .lock()
                .expect("session poisoned")
                .workspace_new(dir);
            Response::WorkspaceCreated { id }
        }
        Request::WorkspaceList => Response::Workspaces {
            workspaces: hub
                .session
                .lock()
                .expect("session poisoned")
                .workspace_list(),
        },
        Request::WorkspaceKill { id } => {
            let mut session = hub.session.lock().expect("session poisoned");
            match session.workspace_kill(id) {
                Ok(panes) => {
                    let view = session.view();
                    drop(session);
                    let mut last = hub.last.lock().expect("last poisoned");
                    for pane in &panes {
                        last.remove(pane);
                    }
                    drop(last);
                    broadcast_event(hub, Event::LayoutChanged { workspaces: view });
                    Response::Ok
                }
                Err(err) => error(err),
            }
        }
        Request::TabNew { workspace } => {
            match hub
                .session
                .lock()
                .expect("session poisoned")
                .tab_new(workspace)
            {
                Ok(id) => Response::TabCreated { id },
                Err(err) => error(err),
            }
        }
        Request::TabList { workspace } => {
            match hub
                .session
                .lock()
                .expect("session poisoned")
                .tab_list(workspace)
            {
                Ok(tabs) => Response::Tabs { tabs },
                Err(err) => error(err),
            }
        }
        Request::TabSelect { id } => {
            match hub.session.lock().expect("session poisoned").tab_select(id) {
                Ok(()) => Response::Ok,
                Err(err) => error(err),
            }
        }
        Request::PaneRun { tab, cmd } => spawn_pane(hub, |s| s.pane_run(tab, cmd)),
        Request::PaneSplit { pane, direction } => {
            spawn_pane(hub, |s| s.pane_split(pane, direction))
        }
        Request::PaneResize { pane, rows, cols } => resize_pane(hub, pane, rows, cols),
        Request::PaneList => Response::Panes {
            panes: hub.session.lock().expect("session poisoned").pane_list(),
        },
        Request::PaneKill { pane } => {
            let mut session = hub.session.lock().expect("session poisoned");
            match session.pane_kill(pane) {
                Ok(_workspace) => {
                    let view = session.view();
                    drop(session);
                    hub.last.lock().expect("last poisoned").remove(&pane);
                    broadcast_event(hub, Event::LayoutChanged { workspaces: view });
                    Response::Ok
                }
                Err(err) => error(err),
            }
        }
        Request::PaneRename { pane, title } => {
            match hub
                .session
                .lock()
                .expect("session poisoned")
                .pane_rename(pane, title)
            {
                Ok(()) => Response::Ok,
                Err(err) => error(err),
            }
        }
        Request::PaneSend {
            pane,
            text,
            keys: key_names,
        } => {
            let mut bytes = Vec::new();
            if let Some(text) = &text {
                bytes.extend_from_slice(text.as_bytes());
            }
            if let Some(spec) = &key_names {
                match keys::to_bytes(spec) {
                    Ok(translated) => bytes.extend_from_slice(&translated),
                    Err(err) => return error(err),
                }
            }
            if bytes.is_empty() {
                return Response::Error {
                    message: "pane send needs text or keys".into(),
                };
            }
            match hub
                .session
                .lock()
                .expect("session poisoned")
                .pane_send(pane, &bytes)
            {
                Ok(()) => Response::Ok,
                Err(err) => error(err),
            }
        }
        Request::PaneRead {
            pane,
            lines,
            unwrapped,
        } => {
            match hub
                .session
                .lock()
                .expect("session poisoned")
                .pane_read(pane, lines, unwrapped)
            {
                Ok(lines) => Response::Content { lines },
                Err(err) => error(err),
            }
        }
        Request::PaneFocus { pane } => focus_pane(hub, pane),
        Request::PaneResizeSplit {
            pane,
            direction,
            delta,
        } => resize_split(hub, pane, direction, delta),
        // Handled in `handle_frame`; unreachable here.
        Request::Attach | Request::Detach | Request::PaneScroll { .. } => Response::Ok,
    }
}

/// Adjust the ratio of the nearest matching-axis split enclosing `pane` and
/// broadcast the fresh view. The attached client re-syncs pane sizes off the
/// new layout, so the ptys resize and reseed on the next tick.
fn resize_split(hub: &Arc<Hub>, pane: PaneId, axis: Direction, delta: f32) -> Response {
    let mut session = hub.session.lock().expect("session poisoned");
    match session.pane_resize_split(pane, axis, delta) {
        Ok(true) => {
            let view = session.view();
            drop(session);
            broadcast_event(hub, Event::LayoutChanged { workspaces: view });
            Response::Ok
        }
        Ok(false) => Response::Ok,
        Err(err) => error(err),
    }
}

/// Focus a pane: record it active and apply a `Focused` state event, flipping a
/// `Done` pane to `Idle` (marked seen). Broadcasts the transition if it changed.
fn focus_pane(hub: &Arc<Hub>, pane: PaneId) -> Response {
    let mut session = hub.session.lock().expect("session poisoned");
    if session.set_active_pane(pane).is_err() {
        return Response::Error {
            message: format!("no pane {pane}"),
        };
    }
    let from = session.pane_state(pane).expect("pane exists");
    let to = from.apply(StateEvent::Focused);
    session.set_pane_state(pane, to);
    drop(session);
    if from != to {
        broadcast_event(hub, Event::StateChanged { pane, from, to });
    }
    Response::Ok
}

/// Run a pane-spawning session op, then wire up the new pane's reaper and emit
/// `LayoutChanged`.
fn spawn_pane(hub: &Arc<Hub>, op: impl FnOnce(&mut Session) -> Result<PaneId>) -> Response {
    let mut session = hub.session.lock().expect("session poisoned");
    match op(&mut session) {
        Ok(pane) => {
            let pty = session.pty(pane).expect("pane just created");
            let view = session.view();
            drop(session);
            spawn_reaper(hub, pane, pty);
            broadcast_event(hub, Event::LayoutChanged { workspaces: view });
            Response::PaneCreated { id: pane }
        }
        Err(err) => error(err),
    }
}

/// Resize a pane's pty and grid, then force a fresh baseline-paired snapshot for
/// every client (clear the last broadcast screen and each client's seen set) so
/// the next tick reseeds their parsers at the new size.
fn resize_pane(hub: &Arc<Hub>, pane: PaneId, rows: u16, cols: u16) -> Response {
    let pty = match hub.session.lock().expect("session poisoned").pty(pane) {
        Some(pty) => pty,
        None => {
            return Response::Error {
                message: format!("no pane {pane}"),
            };
        }
    };
    match pty.resize(rows, cols) {
        Ok(()) => {
            hub.last.lock().expect("last poisoned").remove(&pane);
            for client in hub.clients.lock().expect("clients poisoned").values_mut() {
                client.seen.remove(&pane);
            }
            Response::Ok
        }
        Err(err) => error(err),
    }
}

/// Serve a scrollback request. A positive `offset` ships a one-off snapshot of
/// the scrolled region to the requesting client only; `offset == 0` clears the
/// pane from that client's seen set so the next tick reseeds it live.
fn scroll(
    hub: &Arc<Hub>,
    cid: u64,
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    pane: PaneId,
    offset: usize,
) {
    if offset == 0 {
        if let Some(client) = hub.clients.lock().expect("clients poisoned").get_mut(&cid) {
            client.seen.remove(&pane);
        }
        return;
    }
    let Some(pty) = hub.session.lock().expect("session poisoned").pty(pane) else {
        return;
    };
    let screen = pty.screen_scrolled(offset);
    let (rows, cols) = screen.size();
    let frame = Frame::PaneSnapshot(PaneData {
        pane,
        rows,
        cols,
        seq: 0,
        bytes: screen.contents_formatted(),
    });
    let _ = tx.send(frame.encode());
}

fn spawn_reaper(hub: &Arc<Hub>, pane: PaneId, pty: Arc<PtyPane>) {
    let hub = Arc::clone(hub);
    tokio::spawn(async move {
        let exit = pty.wait().await;
        let code = exit.code as i32;
        let transition = hub
            .session
            .lock()
            .expect("session poisoned")
            .mark_exited(pane, code);
        if let Some((from, to)) = transition {
            if from != to {
                broadcast_event(&hub, Event::StateChanged { pane, from, to });
            }
            broadcast_event(&hub, Event::PaneExited { pane, code });
        }
    });
}

fn broadcast_tick(hub: &Hub) {
    let panes = hub
        .session
        .lock()
        .expect("session poisoned")
        .panes_with_pty();

    // Drain each pane's bells/notifications every tick so the queues stay
    // bounded even with nobody attached; `broadcast_event` is a no-op then.
    for (pane, pty) in &panes {
        for note in pty.take_notifications() {
            broadcast_event(
                hub,
                Event::PaneNotification {
                    pane: *pane,
                    title: note.title,
                    body: note.body,
                },
            );
        }
    }

    if hub.clients.lock().expect("clients poisoned").is_empty() {
        return;
    }
    for (pane, pty) in panes {
        let cur = pty.screen();
        let (rows, cols) = cur.size();

        let (seq, delta) = {
            let mut last = hub.last.lock().expect("last poisoned");
            let outcome = match last.get(&pane) {
                None => (0, None),
                Some((prev, seq)) => {
                    let diff = cur.contents_diff(prev);
                    if diff.is_empty() {
                        (*seq, None)
                    } else {
                        (seq + 1, Some(diff))
                    }
                }
            };
            last.insert(pane, (cur.clone(), outcome.0));
            outcome
        };

        let mut clients = hub.clients.lock().expect("clients poisoned");
        for client in clients.values_mut() {
            if client.seen.insert(pane) {
                let frame = Frame::PaneSnapshot(PaneData {
                    pane,
                    rows,
                    cols,
                    seq,
                    bytes: cur.contents_formatted(),
                });
                let _ = client.tx.send(frame.encode());
            } else if let Some(diff) = &delta {
                let frame = Frame::PaneDelta(PaneData {
                    pane,
                    rows,
                    cols,
                    seq,
                    bytes: diff.clone(),
                });
                let _ = client.tx.send(frame.encode());
            }
        }
    }
}

/// One agent-detection pass: snapshot the process tree, walk each live pane's
/// child subtree, and record the matched agent kind. A change to any pane's
/// agent broadcasts the fresh view so clients relabel their badges.
async fn detect_pass(hub: &Arc<Hub>) {
    let panes = hub.session.lock().expect("session poisoned").live_panes();
    if panes.is_empty() {
        return;
    }
    let tree = tokio::task::spawn_blocking(process_tree)
        .await
        .unwrap_or_default();
    let detected: Vec<(PaneId, Option<AgentKind>)> = panes
        .iter()
        .map(|(id, pty)| {
            let agent = pty
                .child_pid()
                .and_then(|pid| hub.registry.detect(&tree, pid));
            (*id, agent)
        })
        .collect();

    let mut session = hub.session.lock().expect("session poisoned");
    let mut changed = false;
    for (pane, agent) in detected {
        changed |= session.set_agent(pane, agent);
    }
    if changed {
        let view = session.view();
        drop(session);
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
}

/// A `sysinfo` snapshot reduced to the pid → name and pid → children maps the
/// registry walks. Built on a blocking thread so the refresh never stalls the
/// async runtime.
fn process_tree() -> ProcessTree {
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let mut tree = ProcessTree::default();
    for (pid, process) in sys.processes() {
        tree.insert(
            pid.as_u32(),
            process.parent().map(|p| p.as_u32()),
            process.name().to_string_lossy(),
        );
    }
    tree
}

/// One state-classification pass over agent panes. PTY output since the last
/// pass is an `Activity` event; a classifier match on the screen text is a
/// `Classified` event. Both are folded onto the pane's state — activity first
/// so a screen match takes precedence — and any net change is broadcast.
fn classify_pass(hub: &Arc<Hub>, last_gen: &mut HashMap<PaneId, u64>) {
    let panes = hub.session.lock().expect("session poisoned").agent_panes();
    if panes.is_empty() {
        return;
    }
    let events: Vec<(PaneId, bool, Option<StateEvent>)> = panes
        .iter()
        .map(|(pane, kind, pty)| {
            let generation = *pty.output_receiver().borrow();
            let advanced = last_gen
                .insert(*pane, generation)
                .map_or(generation != 0, |prev| prev != generation);
            let classified = hub
                .registry
                .spec(kind)
                .and_then(|spec| spec.classify(&pty.screen().contents()))
                .map(StateEvent::Classified);
            (*pane, advanced, classified)
        })
        .collect();

    let mut session = hub.session.lock().expect("session poisoned");
    let mut changes = Vec::new();
    for (pane, advanced, classified) in events {
        let Some(from) = session.pane_state(pane) else {
            continue;
        };
        let mut to = from;
        if advanced {
            to = to.apply(StateEvent::Activity);
        }
        if let Some(event) = classified {
            to = to.apply(event);
        }
        if to != from {
            session.set_pane_state(pane, to);
            changes.push((pane, from, to));
        }
    }
    drop(session);
    for (pane, from, to) in changes {
        broadcast_event(hub, Event::StateChanged { pane, from, to });
    }
}

fn broadcast_event(hub: &Hub, event: Event) {
    let bytes = encode_json(&event);
    for client in hub.clients.lock().expect("clients poisoned").values() {
        let _ = client.tx.send(bytes.clone());
    }
}

fn encode_json<T: Serialize>(value: &T) -> Vec<u8> {
    Frame::Control(serde_json::to_vec(value).expect("serialize control frame")).encode()
}

fn error(err: anyhow::Error) -> Response {
    Response::Error {
        message: format!("{err:#}"),
    }
}
