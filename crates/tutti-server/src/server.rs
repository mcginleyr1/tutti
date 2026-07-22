//! The daemon: a `UnixListener` fronting shared `Session` state. Each accepted
//! connection is a request/response loop; a connection that sends `Attach` also
//! joins the broadcast set and receives pane snapshots, coalesced deltas, and
//! control events until it detaches or disconnects.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
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
    WorkspaceId,
};

use crate::jj;
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
/// Depth of a client's outbound frame queue. A client that cannot drain this
/// many frames is wedged: rather than grow memory without bound or silently drop
/// frames (a lost delta corrupts its grid forever, a lost event is gone), we
/// disconnect it — it can simply reattach.
const CLIENT_QUEUE_CAP: usize = 256;

struct Client {
    tx: mpsc::Sender<Vec<u8>>,
    /// Aborts this client's writer task, dropping the socket's write half so a
    /// wedged client is forced to disconnect.
    writer: tokio::task::AbortHandle,
    /// Held back from the render tick until the `Attached` reply is queued, so
    /// the first pane frame a client sees always follows its `Attached`.
    ready: bool,
    /// Panes this client has already been sent a snapshot for; the rest of its
    /// panes fall into the shared delta cadence.
    seen: HashSet<PaneId>,
}

pub struct Hub {
    session: Mutex<Session>,
    clients: Mutex<HashMap<u64, Client>>,
    /// Last screen broadcast per pane, with its running delta sequence number
    /// and the pty output generation it was captured at.
    last: Mutex<HashMap<PaneId, (vt100::Screen, u32, u64)>>,
    next_client: AtomicU64,
    /// Session name, reported to attaching clients. Derived from the socket file.
    name: String,
    /// The agent registry driving detection and state classification.
    registry: Registry,
}

impl Hub {
    fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().expect("session poisoned")
    }
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
    hub.session().kill_all();
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
    let (tx, rx) = mpsc::channel::<Vec<u8>>(CLIENT_QUEUE_CAP);
    let writer = tokio::spawn(writer_loop(write_half, rx));
    let abort = writer.abort_handle();

    let mut buf = Vec::new();
    let mut wedged = false;
    while let Some(frame) = read_frame(&mut read_half, &mut buf).await? {
        if handle_frame(&hub, cid, &tx, &abort, frame).is_break() {
            wedged = true;
            break;
        }
    }

    hub.clients.lock().expect("clients poisoned").remove(&cid);
    if wedged {
        // The client's own reply queue backed up: it is not draining, so abort
        // the writer to drop the socket rather than block awaiting a flush.
        writer.abort();
    } else {
        drop(tx);
        let _ = writer.await;
    }
    Ok(())
}

async fn writer_loop(mut half: OwnedWriteHalf, mut rx: mpsc::Receiver<Vec<u8>>) {
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

fn handle_frame(
    hub: &Arc<Hub>,
    cid: u64,
    tx: &mpsc::Sender<Vec<u8>>,
    writer: &tokio::task::AbortHandle,
    frame: Frame,
) -> ControlFlow<()> {
    match frame {
        Frame::Control(json) => match serde_json::from_slice::<Request>(&json) {
            Ok(Request::Attach) => {
                // Register this client before snapshotting the view, so a
                // StateChanged/LayoutChanged broadcast in the gap is delivered
                // (an early or duplicate event the client applies idempotently)
                // rather than lost. It stays held back from the render tick
                // until its Attached reply is queued below, so no pane frame can
                // slip ahead of Attached. Locks stay un-nested: clients and
                // session are never held together.
                hub.clients.lock().expect("clients poisoned").insert(
                    cid,
                    Client {
                        tx: tx.clone(),
                        writer: writer.clone(),
                        ready: false,
                        seen: HashSet::new(),
                    },
                );
                let workspaces = hub.session().view();
                let reply = encode_json(&Response::Attached {
                    session: hub.name.clone(),
                    workspaces,
                });
                let flow = send_reply(hub, cid, tx, reply);
                if flow.is_continue()
                    && let Some(client) =
                        hub.clients.lock().expect("clients poisoned").get_mut(&cid)
                {
                    client.ready = true;
                }
                // Seed every workspace's change stat now that a client is looking.
                refresh_all_changes(hub);
                flow
            }
            Ok(Request::Detach) => {
                hub.clients.lock().expect("clients poisoned").remove(&cid);
                send_reply(hub, cid, tx, encode_json(&Response::Ok))
            }
            Ok(Request::PaneScroll { pane, offset }) => scroll(hub, cid, tx, pane, offset),
            Ok(Request::WorkspaceDiff { id, stat }) => workspace_diff(hub, tx, id, stat),
            Ok(Request::WorkspaceKill { id, discard }) => workspace_kill(hub, cid, tx, id, discard),
            Ok(Request::WorkspaceFork { id, name, revision }) => {
                workspace_fork(hub, cid, tx, id, name, revision)
            }
            Ok(Request::WorkspaceUpdate { id }) => workspace_update(hub, tx, id),
            Ok(request) => {
                let response = dispatch(hub, request);
                send_reply(hub, cid, tx, encode_json(&response))
            }
            Err(err) => send_reply(
                hub,
                cid,
                tx,
                encode_json(&Response::Error {
                    message: format!("bad request: {err}"),
                }),
            ),
        },
        Frame::Input { pane, bytes } => {
            let _ = hub.session().pane_send(pane, &bytes);
            ControlFlow::Continue(())
        }
        Frame::PaneSnapshot(_) | Frame::PaneDelta(_) => ControlFlow::Continue(()),
    }
}

/// Enqueue a reply on the client's own channel. A full or closed queue means the
/// client is not draining: drop it from the broadcast set and tell the caller to
/// tear the connection down.
fn send_reply(
    hub: &Arc<Hub>,
    cid: u64,
    tx: &mpsc::Sender<Vec<u8>>,
    bytes: Vec<u8>,
) -> ControlFlow<()> {
    match tx.try_send(bytes) {
        Ok(()) => ControlFlow::Continue(()),
        Err(_) => {
            hub.clients.lock().expect("clients poisoned").remove(&cid);
            ControlFlow::Break(())
        }
    }
}

fn dispatch(hub: &Arc<Hub>, request: Request) -> Response {
    match request {
        Request::WorkspaceNew { dir } => {
            let id = hub.session().workspace_new(dir);
            refresh_changes(hub, id);
            Response::WorkspaceCreated { id }
        }
        Request::WorkspaceList => Response::Workspaces {
            workspaces: hub.session().workspace_list(),
        },
        Request::TabNew { workspace } => {
            let r = hub.session().tab_new(workspace);
            match r {
                Ok(id) => Response::TabCreated { id },
                Err(err) => error(err),
            }
        }
        Request::TabList { workspace } => {
            let r = hub.session().tab_list(workspace);
            match r {
                Ok(tabs) => Response::Tabs { tabs },
                Err(err) => error(err),
            }
        }
        Request::TabSelect { id } => {
            let r = hub.session().tab_select(id);
            match r {
                Ok(()) => Response::Ok,
                Err(err) => error(err),
            }
        }
        Request::PaneRun {
            tab,
            cmd,
            ephemeral,
        } => spawn_pane(hub, |s| s.pane_run(tab, cmd, ephemeral)),
        Request::PaneSplit { pane, direction } => {
            spawn_pane(hub, |s| s.pane_split(pane, direction))
        }
        Request::PaneResize { pane, rows, cols } => resize_pane(hub, pane, rows, cols),
        Request::PaneList => Response::Panes {
            panes: hub.session().pane_list(),
        },
        Request::PaneKill { pane } => {
            let mut session = hub.session();
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
            let r = hub.session().pane_rename(pane, title);
            match r {
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
            let r = hub.session().pane_send(pane, &bytes);
            match r {
                Ok(()) => Response::Ok,
                Err(err) => error(err),
            }
        }
        Request::PaneRead {
            pane,
            lines,
            unwrapped,
        } => {
            let r = hub.session().pane_read(pane, lines, unwrapped);
            match r {
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
        Request::Attach
        | Request::Detach
        | Request::PaneScroll { .. }
        | Request::WorkspaceDiff { .. }
        | Request::WorkspaceKill { .. }
        | Request::WorkspaceFork { .. }
        | Request::WorkspaceUpdate { .. } => Response::Ok,
    }
}

/// Adjust the ratio of the nearest matching-axis split enclosing `pane` and
/// broadcast the fresh view. The attached client re-syncs pane sizes off the
/// new layout, so the ptys resize and reseed on the next tick.
fn resize_split(hub: &Arc<Hub>, pane: PaneId, axis: Direction, delta: f32) -> Response {
    let mut session = hub.session();
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
    let mut session = hub.session();
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
    let mut session = hub.session();
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
    let pty = match hub.session().pty(pane) {
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
    tx: &mpsc::Sender<Vec<u8>>,
    pane: PaneId,
    offset: usize,
) -> ControlFlow<()> {
    if offset == 0 {
        if let Some(client) = hub.clients.lock().expect("clients poisoned").get_mut(&cid) {
            client.seen.remove(&pane);
        }
        return ControlFlow::Continue(());
    }
    let Some(pty) = hub.session().pty(pane) else {
        return ControlFlow::Continue(());
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
    send_reply(hub, cid, tx, frame.encode())
}

/// Serve a workspace diff. jj is shelled out to on a spawned task so a slow (or
/// large) diff never stalls this connection's frame loop; the reply lands on the
/// requester's own outbound channel when it is ready. A vanished workspace
/// answers Error without touching jj.
fn workspace_diff(
    hub: &Arc<Hub>,
    tx: &mpsc::Sender<Vec<u8>>,
    id: WorkspaceId,
    stat: bool,
) -> ControlFlow<()> {
    let dir = hub.session().workspace_dir(id);
    let tx = tx.clone();
    tokio::spawn(async move {
        let response = match dir {
            Some(dir) => jj::diff(&dir, stat).await,
            None => Response::Error {
                message: format!("no workspace {id}"),
            },
        };
        let _ = tx.send(encode_json(&response)).await;
    });
    ControlFlow::Continue(())
}

/// Kill a workspace. `--discard` additionally scrubs a *forked* checkout from
/// disk: the panes die and tutti drops the workspace right away (a half-cleaned
/// fork must still disappear), then a spawned task runs `jj workspace forget` at
/// the origin and removes the directory, replying once cleanup finishes. Discard
/// on a non-fork is refused outright — tutti never deletes a checkout it did not
/// create. The plain (non-discard) path keeps the old behaviour: panes die,
/// tutti forgets the entry, the checkout stays on disk.
fn workspace_kill(
    hub: &Arc<Hub>,
    cid: u64,
    tx: &mpsc::Sender<Vec<u8>>,
    id: WorkspaceId,
    discard: bool,
) -> ControlFlow<()> {
    let mut session = hub.session();
    // Read what discard needs *before* the kill removes the entry.
    let fork = session.workspace_fork_meta(id);
    let dir = session.workspace_dir(id);
    if discard && fork.is_none() {
        drop(session);
        return send_reply(
            hub,
            cid,
            tx,
            encode_json(&Response::Error {
                message: "refusing to discard a workspace tutti did not fork".into(),
            }),
        );
    }
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
            if !discard {
                return send_reply(hub, cid, tx, encode_json(&Response::Ok));
            }
            // Fork discard: forget at the origin, then remove the checkout. Each
            // failure is surfaced but neither aborts the other, so a half-cleaned
            // fork has still left tutti (the LayoutChanged above already dropped
            // it). The reply follows the cleanup on the requester's channel.
            let fork = fork.expect("discard on a non-fork was rejected above");
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut errors = Vec::new();
                if let Err(e) = jj::forget(&fork.origin_root, &fork.jj_name).await {
                    errors.push(e);
                }
                if let Some(dir) = dir
                    && let Err(e) = tokio::fs::remove_dir_all(&dir).await
                {
                    errors.push(format!("remove {}: {e}", dir.display()));
                }
                let response = if errors.is_empty() {
                    Response::Ok
                } else {
                    Response::Error {
                        message: errors.join("; "),
                    }
                };
                let _ = tx.send(encode_json(&response)).await;
            });
            ControlFlow::Continue(())
        }
        Err(err) => {
            drop(session);
            send_reply(hub, cid, tx, encode_json(&error(err)))
        }
    }
}

/// Fork a jj workspace into a named sibling checkout and mount it. Validation
/// (name shape, source is a jj repo, destination is free) is synchronous; the
/// `jj workspace add` — which materializes a working copy — runs on a spawned
/// task so it never stalls the frame loop, and the `WorkspaceCreated` reply
/// lands on the requester's channel once the fork's shell pane is up.
fn workspace_fork(
    hub: &Arc<Hub>,
    cid: u64,
    tx: &mpsc::Sender<Vec<u8>>,
    id: WorkspaceId,
    name: String,
    revision: Option<String>,
) -> ControlFlow<()> {
    if !jj::valid_fork_name(&name) {
        return send_reply(
            hub,
            cid,
            tx,
            encode_json(&Response::Error {
                message: format!("invalid fork name {name:?}: use letters, digits, '-' or '_'"),
            }),
        );
    }
    // Bind the workspace dir before dropping the session guard; the rest of the
    // checks are pure (no lock) so nothing re-locks the Mutex here.
    let source = hub.session().workspace_dir(id);
    let Some(source) = source else {
        return send_reply(
            hub,
            cid,
            tx,
            encode_json(&Response::Error {
                message: format!("no workspace {id}"),
            }),
        );
    };
    let Some(repo_root) = jj::workspace_root(&source) else {
        return send_reply(
            hub,
            cid,
            tx,
            encode_json(&Response::Error {
                message: format!("not a jj workspace: {}", source.display()),
            }),
        );
    };
    let Some(dest) = jj::fork_dest(&repo_root, &name) else {
        return send_reply(
            hub,
            cid,
            tx,
            encode_json(&Response::Error {
                message: format!("cannot place a fork beside {}", repo_root.display()),
            }),
        );
    };
    if dest.exists() {
        return send_reply(
            hub,
            cid,
            tx,
            encode_json(&Response::Error {
                message: format!("fork destination already exists: {}", dest.display()),
            }),
        );
    }

    let hub = Arc::clone(hub);
    let tx = tx.clone();
    tokio::spawn(async move {
        if let Err(message) = jj::fork(&repo_root, &dest, &name, revision.as_deref()).await {
            let _ = tx.send(encode_json(&Response::Error { message })).await;
            return;
        }
        let meta = crate::session::ForkMeta {
            origin_root: repo_root,
            jj_name: name,
        };
        // Create the tutti workspace, then spawn its shell into that exact tab.
        // The session guard is dropped before `spawn_pane` re-locks the Mutex.
        let (ws_id, tab_id) = {
            let mut session = hub.session();
            session.workspace_new_forked(dest, meta)
        };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        spawn_pane(&hub, |s| s.pane_run(Some(tab_id), vec![shell], false));
        // Seed the fork's change stat / stale flag now that it exists.
        refresh_changes(&hub, ws_id);
        let _ = tx
            .send(encode_json(&Response::WorkspaceCreated { id: ws_id }))
            .await;
    });
    ControlFlow::Continue(())
}

/// Clear a stale working copy with `jj workspace update-stale`, then refresh the
/// workspace's stale flag. Runs on a spawned task (it touches the working copy)
/// and replies when done.
fn workspace_update(
    hub: &Arc<Hub>,
    tx: &mpsc::Sender<Vec<u8>>,
    id: WorkspaceId,
) -> ControlFlow<()> {
    let dir = hub.session().workspace_dir(id);
    let hub = Arc::clone(hub);
    let tx = tx.clone();
    tokio::spawn(async move {
        let response = match dir {
            Some(dir) => {
                let response = jj::update_stale(&dir).await;
                if matches!(response, Response::Ok) {
                    refresh_changes(&hub, id);
                }
                response
            }
            None => Response::Error {
                message: format!("no workspace {id}"),
            },
        };
        let _ = tx.send(encode_json(&response)).await;
    });
    ControlFlow::Continue(())
}

fn spawn_reaper(hub: &Arc<Hub>, pane: PaneId, pty: Arc<PtyPane>) {
    let hub = Arc::clone(hub);
    tokio::spawn(async move {
        let exit = pty.wait().await;
        let code = exit.code as i32;
        // An ephemeral pane (e.g. the diff view) leaves no corpse: drop it from
        // the layout + pane map and rebroadcast the view. `pane_kill` also fixes
        // the tab's active pane, so focus falls back to a remaining pane. The
        // probe is bound (not called inline in the `if`) so no session guard is
        // live when the block re-locks the Mutex.
        let ephemeral = hub.session().is_ephemeral(pane);
        if ephemeral {
            let mut session = hub.session();
            let workspace = session.pane_kill(pane).ok();
            let view = session.view();
            drop(session);
            hub.last.lock().expect("last poisoned").remove(&pane);
            broadcast_event(&hub, Event::LayoutChanged { workspaces: view });
            if let Some(workspace) = workspace {
                refresh_changes(&hub, workspace);
            }
            return;
        }
        let transition = hub.session().mark_exited(pane, code);
        if let Some((from, to)) = transition {
            if from != to {
                broadcast_event(&hub, Event::StateChanged { pane, from, to });
            }
            broadcast_event(&hub, Event::PaneExited { pane, code });
            // A finished agent likely touched files; refresh its workspace stat.
            refresh_pane_workspace(&hub, pane);
        }
    });
}

/// Recompute a workspace's jj change stat *and* stale flag off the
/// render/dispatch path and, when either moved, rebroadcast the view so sidebars
/// update. jj is shelled out to on a spawned task, so a slow repo never stalls
/// the caller (the tick or dispatch). The two probes ride the same task so the
/// stat and the stale tag stay in lockstep.
fn refresh_changes(hub: &Arc<Hub>, workspace: WorkspaceId) {
    let Some(dir) = hub.session().workspace_dir(workspace) else {
        return;
    };
    let hub = Arc::clone(hub);
    tokio::spawn(async move {
        let changes = jj::change_stat(&dir).await;
        let stale = jj::is_stale(&dir).await;
        let mut session = hub.session();
        let stat_moved = session.set_changes(workspace, changes);
        let stale_moved = session.set_stale(workspace, stale);
        if stat_moved || stale_moved {
            let view = session.view();
            drop(session);
            broadcast_event(&hub, Event::LayoutChanged { workspaces: view });
        }
    });
}

/// Refresh the change stat of the workspace owning `pane`, if it still exists.
/// The lookup is bound before the `if let` so the session guard is released
/// before `refresh_changes` re-locks it (the std Mutex is non-reentrant).
fn refresh_pane_workspace(hub: &Arc<Hub>, pane: PaneId) {
    let workspace = hub.session().workspace_of_pane(pane);
    if let Some(workspace) = workspace {
        refresh_changes(hub, workspace);
    }
}

/// Refresh every workspace's change stat — used on attach to seed the sidebars.
/// The ids are bound before the loop: a `for` holds its iterator's temporaries
/// (here the session guard) across the whole body, so leaving the guard live
/// would deadlock against `refresh_changes` re-locking the same Mutex.
fn refresh_all_changes(hub: &Arc<Hub>) {
    let ids = hub.session().workspace_ids();
    for workspace in ids {
        refresh_changes(hub, workspace);
    }
}

fn broadcast_tick(hub: &Hub) {
    let panes = hub.session().live_panes();

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
    // Clients whose bounded queue filled this tick, disconnected once the loop
    // releases the clients lock.
    let mut doomed: HashSet<u64> = HashSet::new();
    for (pane, pty) in panes {
        let generation = *pty.output_receiver().borrow();

        // A quiescent pane (output generation unchanged since we last stored it)
        // whose screen every client already holds needs no fresh clone/diff this
        // tick. A fresh client, missing it from `seen`, still gets its snapshot
        // below, so only skip when every client has already seen it.
        let unchanged = matches!(
            hub.last.lock().expect("last poisoned").get(&pane),
            Some((_, _, stored)) if *stored == generation
        );
        if unchanged
            && hub
                .clients
                .lock()
                .expect("clients poisoned")
                .values()
                .all(|c| c.seen.contains(&pane))
        {
            continue;
        }

        let cur = pty.screen();
        let (rows, cols) = cur.size();

        let (seq, delta) = {
            let mut last = hub.last.lock().expect("last poisoned");
            let outcome = match last.get(&pane) {
                None => (0, None),
                Some((prev, seq, _)) => {
                    let diff = cur.contents_diff(prev);
                    if diff.is_empty() {
                        (*seq, None)
                    } else {
                        (seq + 1, Some(diff))
                    }
                }
            };
            last.insert(pane, (cur.clone(), outcome.0, generation));
            outcome
        };

        let mut clients = hub.clients.lock().expect("clients poisoned");
        for (cid, client) in clients.iter_mut() {
            if !client.ready || doomed.contains(cid) {
                continue;
            }
            let bytes = if client.seen.insert(pane) {
                Frame::PaneSnapshot(PaneData {
                    pane,
                    rows,
                    cols,
                    seq,
                    bytes: cur.contents_formatted(),
                })
                .encode()
            } else if let Some(diff) = &delta {
                Frame::PaneDelta(PaneData {
                    pane,
                    rows,
                    cols,
                    seq,
                    bytes: diff.clone(),
                })
                .encode()
            } else {
                continue;
            };
            if client.tx.try_send(bytes).is_err() {
                doomed.insert(*cid);
            }
        }
    }
    disconnect(hub, doomed);
}

/// One agent-detection pass: snapshot the process tree, walk each live pane's
/// child subtree, and record the matched agent kind. A change to any pane's
/// agent broadcasts the fresh view so clients relabel their badges.
async fn detect_pass(hub: &Arc<Hub>) {
    let panes = hub.session().live_panes();
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

    let mut session = hub.session();
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
            process.parent().map(sysinfo::Pid::as_u32),
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
    let panes = hub.session().agent_panes();
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
    // Drop generations for panes that have gone away so the map cannot grow
    // without bound as panes come and go.
    last_gen.retain(|pane, _| panes.iter().any(|(p, _, _)| p == pane));

    let mut session = hub.session();
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
    // Dedupe the workspaces of the changed panes so a burst of transitions in
    // one workspace triggers a single jj refresh, not one per pane.
    let mut workspaces: HashSet<WorkspaceId> = HashSet::new();
    for (pane, from, to) in changes {
        broadcast_event(hub, Event::StateChanged { pane, from, to });
        let workspace = hub.session().workspace_of_pane(pane);
        if let Some(workspace) = workspace {
            workspaces.insert(workspace);
        }
    }
    for workspace in workspaces {
        refresh_changes(hub, workspace);
    }
}

fn broadcast_event(hub: &Hub, event: Event) {
    let bytes = encode_json(&event);
    let mut doomed = Vec::new();
    {
        let clients = hub.clients.lock().expect("clients poisoned");
        for (cid, client) in clients.iter() {
            if client.tx.try_send(bytes.clone()).is_err() {
                doomed.push(*cid);
            }
        }
    }
    disconnect(hub, doomed);
}

/// Drop wedged clients: remove each from the broadcast set and abort its writer
/// so the socket closes. Callers collect the ids while iterating, release the
/// clients lock, then call this — the clients lock must not be held here.
fn disconnect(hub: &Hub, cids: impl IntoIterator<Item = u64>) {
    let mut cids = cids.into_iter().peekable();
    if cids.peek().is_none() {
        return;
    }
    let mut clients = hub.clients.lock().expect("clients poisoned");
    for cid in cids {
        if let Some(client) = clients.remove(&cid) {
            client.writer.abort();
            eprintln!("tutti: client {cid} send queue full; disconnecting");
        }
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
