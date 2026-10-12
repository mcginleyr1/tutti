//! The daemon: a `UnixListener` fronting shared `Session` state. Each accepted
//! connection is a request/response loop; a connection that sends `Attach` also
//! joins the broadcast set and receives pane snapshots, coalesced deltas, and
//! control events until it detaches or disconnects. One that sends `Watch` is
//! pushed a whole-session snapshot whenever it changes, outside that set, and
//! one that sends `PaneAttach` is streamed one pane's raw output, for a real
//! terminal to parse. A panic is contained where it happens — one frame's ends
//! its connection, one background pass's ends that pass — so the daemon, and
//! every pane with it, lives on.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::os::unix::fs::DirBuilderExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot};

use tutti_agents::{ProcessTree, Registry};
use tutti_core::{
    AgentHookEvent, AgentKind, AgentState, ApprovalDecision, Checkpoint, Direction, Event, Frame,
    IssueRef, NOTES_CAP, Note, NoteKind, Observation, PaneData, PaneDims, PaneId, PaneInfo,
    Request, Response, StateEvent, Tagged, Toward, WaitFor, WorkspaceId, prompt_fingerprint,
};

use crate::herdr;
use crate::jj;
use crate::keys;
use crate::persist::{self, SavedPane};
use crate::pty::{Feed, PaneSize, PtyPane, Signal, StatusReport};
use crate::session::{ReportOutcome, Restored, RunTarget, Session};

/// Cadence of the render tick: coalesced pane deltas to attached clients, and
/// snapshots to watchers.
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
/// Depth of the pane-signal broadcast `pane wait` subscribes to. A waiter that
/// falls this far behind re-checks its pane's current state instead.
const SIGNAL_CAP: usize = 256;
/// Cadence of the layout save. A daemon that dies without a graceful shutdown
/// loses at most this much of its layout's history.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);
/// How long the accept loop pauses after a failed accept — EMFILE, out of
/// descriptors, is the likely one — before trying again: long enough not to
/// spin on an error that persists, short enough to soon use a freed one.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);
/// How long a pane stream waits, once its child has exited, for the reader to
/// reach the pty's end — so the child's last output goes out before the exit
/// does. A grandchild holding the pty open never lets it.
const EXIT_GRACE: Duration = Duration::from_millis(250);

/// One pane's news, broadcast to every `pane wait`.
#[derive(Debug, Clone, Copy)]
enum PaneSignal {
    /// Its state moved — sent by `publish_transition`, the one funnel.
    Moved {
        pane: PaneId,
        from: AgentState,
        to: AgentState,
    },
    /// Its child exited, whether or not that moved its state.
    Exited(PaneId),
    /// It left the session: killed, its workspace killed, or ephemeral and done.
    Gone(PaneId),
}

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
    /// The panes this client shows, each at the size it renders it: its vote
    /// in every one's size (`Request::ViewSizes`).
    view: Vec<PaneDims>,
    /// The terminal app the client runs in, from its `Attach`, for watchers.
    terminal: Option<String>,
    /// The client hands a move past tutti's edge on to the terminal it runs
    /// in (rtty), from its `Attach`: it takes top, bottom and resize edges too.
    handoff: bool,
    /// The pane the client last reported focused — what it shows, for watchers.
    focused: Option<PaneId>,
}

/// A `Watch` connection: read-only, outside the attached-client set.
struct Watcher {
    tx: mpsc::Sender<Vec<u8>>,
    writer: tokio::task::AbortHandle,
    /// The encoded snapshot last sent, so an unchanged session sends nothing.
    last: Vec<u8>,
}

/// Where a request's answer goes: the requester's own queue, tagged with the
/// request's `req` when it carried one — answers sent later from a spawned
/// task included.
#[derive(Clone)]
struct Reply {
    tx: mpsc::Sender<Vec<u8>>,
    req: Option<u64>,
}

impl Reply {
    fn encode(&self, response: &Response) -> Vec<u8> {
        encode_json(&Tagged {
            req: self.req,
            body: response,
        })
    }

    /// Answer from a spawned task, waiting for room in the queue.
    async fn send(&self, response: Response) {
        let _ = self.tx.send(self.encode(&response)).await;
    }
}

pub struct Hub {
    session: Mutex<Session>,
    clients: Mutex<HashMap<u64, Client>>,
    watchers: Mutex<HashMap<u64, Watcher>>,
    /// The pane streams (`PaneAttach`), by connection: the pane each streams
    /// and the size it votes for it. Outside `clients` — a stream gets no
    /// events, answers no approval, and is no watcher's attached terminal.
    taps: Mutex<HashMap<u64, PaneDims>>,
    /// What the render tick last broadcast of each pane (`Last`).
    last: Mutex<HashMap<PaneId, Last>>,
    next_client: AtomicU64,
    /// Set as `serve` winds down: the panes its shutdown kills did not exit on
    /// their own, so no pane stream is told they did — it loses its daemon.
    stopping: AtomicBool,
    /// Session name, reported to attaching clients. Derived from the socket file.
    name: String,
    /// The agent registry driving detection and state classification.
    registry: Registry,
    /// The VCS watch's book, one entry per workspace it has seen.
    vcs: Mutex<HashMap<WorkspaceId, VcsMark>>,
    /// Every pane's transitions, exits and removals, for `pane wait` to
    /// subscribe to. With nobody waiting a send fails, harmlessly.
    signals: broadcast::Sender<PaneSignal>,
    /// Where the layout is saved and the previous daemon's is offered back
    /// (`persist`); `None` with persistence off.
    state: Option<PathBuf>,
    /// Each held approval's way back to the hook holding it, by approval id:
    /// an answer sends its decision down the channel.
    approvals: Mutex<HashMap<u64, oneshot::Sender<ApprovalDecision>>>,
    /// The notification history, for clients that missed a note to catch up.
    notes: Mutex<Notes>,
}

/// What the render tick last broadcast of a pane: the screen's size and
/// which one was up — a delta needs the same — the running delta sequence
/// number, and the pty output generation it was looked at.
#[derive(Clone, Copy)]
struct Last {
    size: PaneSize,
    alt_screen: bool,
    seq: u32,
    generation: u64,
}

/// The notification history: the newest `NOTES_CAP` notes, oldest first, and
/// the last id handed out — ids only grow, across a clear too.
#[derive(Default)]
struct Notes {
    ring: Vec<Note>,
    last: u64,
}

/// One workspace's VCS-watch state: `mark` is the op-heads mtime as of the last
/// completed change probe (`None` until one has recorded it), and `in_flight`
/// counts the probes running now — the watch stands off while any is, since a
/// probe's own snapshot is an operation too and must never read as one made
/// outside tutti.
#[derive(Default)]
struct VcsMark {
    mark: Option<SystemTime>,
    in_flight: usize,
}

impl Hub {
    fn session(&self) -> MutexGuard<'_, Session> {
        lock(&self.session)
    }

    fn clients(&self) -> MutexGuard<'_, HashMap<u64, Client>> {
        lock(&self.clients)
    }

    fn watchers(&self) -> MutexGuard<'_, HashMap<u64, Watcher>> {
        lock(&self.watchers)
    }

    fn taps(&self) -> MutexGuard<'_, HashMap<u64, PaneDims>> {
        lock(&self.taps)
    }

    fn last(&self) -> MutexGuard<'_, HashMap<PaneId, Last>> {
        lock(&self.last)
    }

    fn vcs(&self) -> MutexGuard<'_, HashMap<WorkspaceId, VcsMark>> {
        lock(&self.vcs)
    }

    fn approvals(&self) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<ApprovalDecision>>> {
        lock(&self.approvals)
    }

    fn notes(&self) -> MutexGuard<'_, Notes> {
        lock(&self.notes)
    }
}

/// Lock a hub mutex, recovering it if a panicking task poisoned it: that panic
/// was caught and logged where it struck, and the daemon — every pane — must
/// outlive it rather than turn each later lock into another.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bootstrap the socket, install a SIGTERM/SIGINT shutdown, and serve until it
/// fires, saving the layout under `state`. This is the binary's entry point.
pub async fn run(socket_path: PathBuf, size: PaneSize, state: Option<PathBuf>) -> Result<()> {
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
    serve(listener, socket_path, size, state, shutdown).await
}

/// Serve connections on `listener` until `shutdown` resolves, then save the
/// layout, kill every pane and remove the socket file. With a `state`
/// directory the layout is saved there as it changes and the previous
/// daemon's save is offered back (`persist`); `None` turns that off.
pub async fn serve(
    listener: UnixListener,
    socket_path: PathBuf,
    size: PaneSize,
    state: Option<PathBuf>,
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
    // Before the first request is read: a `server restore` that auto-started
    // this daemon finds the offer already in place.
    let state = state.filter(|dir| persist::offer_previous(dir, &name));
    let hub = Arc::new(Hub {
        session: Mutex::new(Session::new(size, name.clone())),
        clients: Mutex::new(HashMap::new()),
        watchers: Mutex::new(HashMap::new()),
        taps: Mutex::new(HashMap::new()),
        last: Mutex::new(HashMap::new()),
        next_client: AtomicU64::new(0),
        stopping: AtomicBool::new(false),
        name,
        registry: Registry::default(),
        vcs: Mutex::new(HashMap::new()),
        signals: broadcast::channel(SIGNAL_CAP).0,
        state,
        notes: Mutex::new(Notes::default()),
        approvals: Mutex::new(HashMap::new()),
    });

    let tick_hub = Arc::clone(&hub);
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            guarded("render tick", || broadcast_tick(&tick_hub));
            guarded("watch snapshot", || snapshot_watchers(&tick_hub));
        }
    });

    let detect_hub = Arc::clone(&hub);
    let detector = tokio::spawn(async move {
        let mut interval = tokio::time::interval(DETECT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            // An async pass, so a task of its own: the task boundary catches
            // its panic as `guarded` does a sync pass's.
            if let Err(err) = tokio::spawn(detect_pass(Arc::clone(&detect_hub))).await
                && err.is_panic()
            {
                eprintln!("tutti: the agent-detection pass panicked; carrying on");
            }
        }
    });

    let vcs_hub = Arc::clone(&hub);
    let vcs_watcher = tokio::spawn(async move {
        let mut interval = tokio::time::interval(DETECT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            guarded("VCS watch", || vcs_pass(&vcs_hub));
        }
    });

    let classify_hub = Arc::clone(&hub);
    let classifier = tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLASSIFY_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_gen: HashMap<PaneId, u64> = HashMap::new();
        loop {
            interval.tick().await;
            guarded("classify", || classify_pass(&classify_hub, &mut last_gen));
        }
    });

    let saver = hub.state.as_deref().map(|dir| {
        let path = persist::live_path(dir, &hub.name);
        let save_hub = Arc::clone(&hub);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SAVE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last = None;
            loop {
                interval.tick().await;
                let saved = save_hub.session().snapshot();
                persist::save_changed(&path, &mut last, saved.as_ref());
            }
        })
    });

    // herdr's reporting socket, beside ours: agents that report to herdr
    // natively (Crush, …) find it through every pane's HERDR_* and report
    // here. Without it the daemon runs on and panes carry no HERDR_*. A
    // request that panics answers as one for an unknown pane.
    let herdr_path = herdr::socket_path(&socket_path);
    let herdr_listener = match herdr::bind(&herdr_path) {
        Ok(listener) => {
            hub.session().set_herdr_socket(herdr_path.clone());
            let herdr_hub = Arc::clone(&hub);
            Some(tokio::spawn(herdr::serve(listener, move |request| {
                guarded("herdr request", || herdr_request(&herdr_hub, request)).unwrap_or(false)
            })))
        }
        Err(err) => {
            eprintln!("tutti: no herdr socket at {}: {err}", herdr_path.display());
            None
        }
    };

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let hub = Arc::clone(&hub);
                    tokio::spawn(async move {
                        if let Err(err) = handle_conn(hub, stream).await {
                            eprintln!("connection error: {err:#}");
                        }
                    });
                }
                // Out of descriptors (EMFILE) and its kin pass: ending `serve`
                // here would kill every pane.
                Err(err) => {
                    eprintln!("tutti: accept failed: {err}; retrying");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                }
            }
        }
    }

    ticker.abort();
    detector.abort();
    vcs_watcher.abort();
    classifier.abort();
    if let Some(herdr) = herdr_listener {
        herdr.abort();
        let _ = std::fs::remove_file(&herdr_path);
    }
    // The last save lands before the panes die — a dead pane is not saved —
    // and after the saver has wound down, so none of its writes can land
    // after this one. It writes even when nothing moved, dating the save to
    // the shutdown.
    if let Some(saver) = saver {
        saver.abort();
        let _ = saver.await;
    }
    if let Some(dir) = &hub.state {
        let saved = hub.session().snapshot();
        persist::save(&persist::live_path(dir, &hub.name), saved.as_ref());
    }
    hub.stopping.store(true, Ordering::SeqCst);
    hub.session().kill_all();
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

/// Run one pass of a background loop, catching its panic (`None`) so the loop
/// lives on: a dead render tick would freeze every pane on every client, a
/// dead classifier every agent's badge, until the daemon restarted. The panic
/// hook has logged the panic itself; this notes the recovery.
fn guarded<T>(pass: &str, run: impl FnOnce() -> T) -> Option<T> {
    let ran = catch_unwind(AssertUnwindSafe(run)).ok();
    if ran.is_none() {
        eprintln!("tutti: the {pass} pass panicked; carrying on");
    }
    ran
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
    let (hangup, hung_up) = oneshot::channel::<()>();
    let writer = tokio::spawn(writer_loop(write_half, rx, hung_up));
    let abort = writer.abort_handle();
    // Handed to a pane stream's tap (`pane_attach`), whose end drops it.
    let (kick, mut kicked) = oneshot::channel::<()>();
    let mut kick = Some(kick);

    let mut buf = Vec::new();
    let mut wedged = false;
    // A read error (reset socket, malformed frame) or a panic handling a frame
    // still falls through to the cleanup: a client left registered keeps its
    // writer task alive, defeats the ticker's nobody-attached shortcut, and
    // holds its vote in the sizes of the panes it viewed.
    let outcome = loop {
        let read = tokio::select! {
            read = read_frame(&mut read_half, &mut buf) => read,
            // The pane stream ended: what is queued drains, then it closes.
            _ = &mut kicked => break Ok(()),
        };
        match read {
            Ok(Some(frame)) => {
                let handled = catch_unwind(AssertUnwindSafe(|| {
                    handle_frame(&hub, cid, &tx, &abort, &mut kick, frame)
                }));
                match handled {
                    Ok(ControlFlow::Continue(())) => {}
                    Ok(ControlFlow::Break(())) => {
                        wedged = true;
                        break Ok(());
                    }
                    Err(_) => break Err(anyhow!("handling a frame panicked")),
                }
            }
            Ok(None) => break Ok(()),
            Err(err) => break Err(err),
        }
    };

    drop_client(&hub, cid);
    hub.watchers().remove(&cid);
    if wedged {
        // The client's own reply queue backed up: it is not draining, so abort
        // the writer to drop the socket rather than block awaiting a flush.
        writer.abort();
    } else {
        // Dropping `hangup` tells the writer the peer is gone.
        drop((tx, hangup));
        let _ = writer.await;
    }
    outcome
}

/// Write the connection's outbound frames in order. Once the reader sees the
/// peer hang up the queue closes — what is already queued still drains — so a
/// task still holding a reply sender (a `pane wait`, which may block for
/// hours) sees `closed()` and gives up, instead of keeping this writer, and
/// with it the connection, alive.
async fn writer_loop(
    mut half: OwnedWriteHalf,
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut hung_up: oneshot::Receiver<()>,
) {
    loop {
        let bytes = tokio::select! {
            bytes = rx.recv() => bytes,
            _ = &mut hung_up, if !rx.is_closed() => {
                rx.close();
                continue;
            }
        };
        let Some(bytes) = bytes else { break };
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
    kick: &mut Option<oneshot::Sender<()>>,
    frame: Frame,
) -> ControlFlow<()> {
    #[cfg(test)]
    tests::inject_panic(hub, &frame);
    // A request's `req` rides back on its answer — even on the refusal of one
    // that does not parse.
    let req = match &frame {
        Frame::Control(json) => serde_json::from_slice::<Tagged<serde_json::Value>>(json)
            .ok()
            .and_then(|tagged| tagged.req),
        _ => None,
    };
    let reply = Reply {
        tx: tx.clone(),
        req,
    };
    match frame {
        Frame::Control(json) => match serde_json::from_slice::<Request>(&json) {
            Ok(Request::Attach { terminal, handoff }) => {
                // Register this client before snapshotting the view, so a
                // StateChanged/LayoutChanged broadcast in the gap is delivered
                // (an early or duplicate event the client applies idempotently)
                // rather than lost. It stays held back from the render tick
                // until its Attached reply is queued below, so no pane frame can
                // slip ahead of Attached. Locks stay un-nested: clients and
                // session are never held together.
                hub.clients().insert(
                    cid,
                    Client {
                        tx: tx.clone(),
                        writer: writer.clone(),
                        ready: false,
                        seen: HashSet::new(),
                        view: Vec::new(),
                        terminal,
                        handoff,
                        focused: None,
                    },
                );
                let workspaces = hub.session().view();
                let notes = hub.notes().ring.clone();
                let attached = reply.encode(&Response::Attached {
                    session: hub.name.clone(),
                    workspaces,
                    wire_rev: tutti_core::WIRE_REV,
                    restorable: hub
                        .state
                        .as_deref()
                        .and_then(|dir| persist::summary(dir, &hub.name)),
                    notes,
                });
                let flow = send_reply(hub, cid, tx, attached);
                if flow.is_continue()
                    && let Some(client) = hub.clients().get_mut(&cid)
                {
                    client.ready = true;
                }
                // Seed every workspace's change stat now that a client is looking.
                refresh_all_changes(hub);
                flow
            }
            Ok(Request::Detach) => {
                drop_client(hub, cid);
                send_reply(hub, cid, tx, reply.encode(&Response::Ok))
            }
            Ok(Request::ViewSizes { panes }) => view_sizes(hub, cid, &reply, panes),
            Ok(Request::PaneAttach { pane, rows, cols }) => {
                pane_attach(hub, cid, &reply, kick, PaneDims { pane, rows, cols })
            }
            Ok(Request::Watch) => {
                hub.watchers().insert(
                    cid,
                    Watcher {
                        tx: tx.clone(),
                        writer: writer.clone(),
                        last: Vec::new(),
                    },
                );
                // The first snapshot goes out now, not on the next tick.
                snapshot_watchers(hub);
                ControlFlow::Continue(())
            }
            Ok(Request::PaneFocus { pane }) => {
                let response = focus_pane(hub, cid, pane);
                send_reply(hub, cid, tx, reply.encode(&response))
            }
            Ok(Request::PaneFocusToward { from, toward, wrap }) => {
                let response = focus_toward(hub, cid, from, toward, wrap);
                send_reply(hub, cid, tx, reply.encode(&response))
            }
            Ok(Request::PaneScroll { pane, offset }) => scroll(hub, cid, tx, pane, offset),
            Ok(Request::WorkspaceDiff { id, stat }) => workspace_diff(hub, &reply, id, stat),
            Ok(Request::ApprovalHold {
                pane,
                conversation,
                tool,
                summary,
            }) => approval_hold(hub, cid, &reply, pane, conversation, tool, summary),
            Ok(Request::PaneWait {
                pane,
                until,
                next,
                timeout_ms,
            }) => pane_wait(hub, &reply, pane, until, next, timeout_ms),
            Ok(Request::WorkspaceKill { id, discard }) => {
                workspace_kill(hub, cid, &reply, id, discard)
            }
            Ok(Request::WorkspaceFork {
                id,
                name,
                revision,
                dest,
            }) => workspace_fork(
                hub,
                cid,
                &reply,
                id,
                name,
                Checkout::Revision(revision),
                dest,
            ),
            Ok(Request::SandboxPull { pane, name }) => sandbox_pull(hub, cid, &reply, pane, name),
            Ok(Request::WorkspaceMerge { id, push }) => workspace_merge(hub, cid, &reply, id, push),
            Ok(Request::WorkspaceUpdate { id }) => workspace_update(hub, &reply, id),
            Ok(Request::PaneCheckpointDiff { pane, n, stat }) => {
                checkpoint_diff(hub, &reply, pane, n, stat)
            }
            Ok(Request::PaneRestore { pane, n, force }) => {
                pane_restore(hub, &reply, pane, n, force)
            }
            Ok(request) => {
                let response = dispatch(hub, request);
                send_reply(hub, cid, tx, reply.encode(&response))
            }
            Err(err) => send_reply(
                hub,
                cid,
                tx,
                reply.encode(&Response::Error {
                    message: format!("bad request: {err}"),
                }),
            ),
        },
        Frame::Input { pane, bytes } => {
            let _ = pane_input(hub, pane, &bytes);
            ControlFlow::Continue(())
        }
        Frame::PaneSnapshot(_) | Frame::PaneDelta(_) | Frame::PaneRaw { .. } => {
            ControlFlow::Continue(())
        }
    }
}

/// Enqueue a reply on the client's own channel. A full or closed queue means the
/// client is not draining: drop it from the broadcast set and tell the caller to
/// tear the connection down.
fn send_reply(hub: &Hub, cid: u64, tx: &mpsc::Sender<Vec<u8>>, bytes: Vec<u8>) -> ControlFlow<()> {
    match tx.try_send(bytes) {
        Ok(()) => ControlFlow::Continue(()),
        Err(_) => {
            drop_client(hub, cid);
            ControlFlow::Break(())
        }
    }
}

/// Drop client `cid` from the broadcast set — detached, gone, or wedged — and
/// let the panes it viewed re-arbitrate their size without its vote; a pane
/// stream's too, untapping it. Returns the client, for a caller that must
/// tear it down.
fn drop_client(hub: &Hub, cid: u64) -> Option<Client> {
    let tap = hub.taps().remove(&cid);
    if let Some(PaneDims { pane, .. }) = tap {
        let pty = hub.session().pty(pane);
        if let Some(pty) = pty {
            pty.untap(cid);
        }
        arbitrate(hub, [pane]);
    }
    let client = hub.clients().remove(&cid)?;
    arbitrate(hub, client.view.iter().map(|d| d.pane));
    Some(client)
}

/// Record an attached client's visible set in place of its last, then size
/// every pane either set names by the viewers it has now. A pane stream's set
/// is its one pane: a size for any other casts no vote. A connection that
/// never attached has no set to keep.
fn view_sizes(hub: &Hub, cid: u64, reply: &Reply, panes: Vec<PaneDims>) -> ControlFlow<()> {
    let old = hub
        .clients()
        .get_mut(&cid)
        .map(|client| std::mem::replace(&mut client.view, panes.clone()));
    let old = old.or_else(|| {
        let mut taps = hub.taps();
        let vote = taps.get_mut(&cid)?;
        let new = panes.iter().find(|d| d.pane == vote.pane).copied();
        Some(vec![std::mem::replace(vote, new.unwrap_or(*vote))])
    });
    let response = match old {
        Some(old) => {
            arbitrate(hub, old.iter().chain(&panes).map(|d| d.pane));
            Response::Ok
        }
        None => Response::Error {
            message: "view sizes come from an attached client".into(),
        },
    };
    send_reply(hub, cid, &reply.tx, reply.encode(&response))
}

/// Size each of `panes` to the smallest rows and the smallest cols among the
/// clients viewing it now, pane streams included — tmux's smallest client
/// wins. A pane nobody views keeps its size. The votes are read under the
/// clients and taps locks, released before `resize_pane` takes the others.
fn arbitrate(hub: &Hub, panes: impl IntoIterator<Item = PaneId>) {
    let panes: HashSet<PaneId> = panes.into_iter().collect();
    let sizes: Vec<PaneDims> = {
        let clients = hub.clients();
        let taps = hub.taps();
        panes
            .into_iter()
            .filter_map(|pane| {
                clients
                    .values()
                    .flat_map(|client| &client.view)
                    .chain(taps.values())
                    .filter(|dims| dims.pane == pane)
                    .copied()
                    .reduce(|a, b| PaneDims {
                        pane,
                        rows: a.rows.min(b.rows),
                        cols: a.cols.min(b.cols),
                    })
            })
            .collect()
    };
    for PaneDims { pane, rows, cols } in sizes {
        let _ = resize_pane(hub, pane, rows, cols);
    }
}

/// `PaneAttach`: stream a pane raw to this connection through a tap
/// (`PtyPane::tap`), voting `dims` for its size as an attached client's view
/// does. The vote goes in first, so the snapshot comes at the size it
/// settles. The tap owns `kick`, so its end — behind, untapped, or gone with
/// its pane — ends the connection's read loop, which drains what is queued
/// and closes. A tap is behind when its queue is all but full: the reader
/// never waits on it, and the slot left carries the Error it ends with. The
/// child exiting is told once its last output is queued — unless the daemon
/// is stopping, which killed it.
fn pane_attach(
    hub: &Arc<Hub>,
    cid: u64,
    reply: &Reply,
    kick: &mut Option<oneshot::Sender<()>>,
    dims: PaneDims,
) -> ControlFlow<()> {
    let refuse = |message: String| {
        send_reply(
            hub,
            cid,
            &reply.tx,
            reply.encode(&Response::Error { message }),
        )
    };
    let pane = dims.pane;
    let pty = hub.session().pty(pane);
    let Some(pty) = pty else {
        return refuse(format!("no pane {pane}"));
    };
    let Some(kick) = kick.take() else {
        return refuse("this connection already streams a pane".into());
    };
    hub.taps().insert(cid, dims);
    arbitrate(hub, [pane]);
    let tx = reply.tx.clone();
    let mut behind = reply.encode(&Response::Error {
        message: format!("fell behind pane {pane}'s output"),
    });
    pty.tap(
        cid,
        Box::new(move |feed| {
            // Named, so the tap owns `kick` and drops it with itself.
            let _owned = &kick;
            if tx.capacity() < 2 {
                eprintln!("tutti: a stream of pane {pane} fell behind; dropping it");
                let _ = tx.try_send(std::mem::take(&mut behind));
                return false;
            }
            let frame = match feed {
                Feed::Snapshot(size, bytes) => Frame::PaneSnapshot(PaneData {
                    pane,
                    rows: size.rows,
                    cols: size.cols,
                    seq: 0,
                    bytes,
                }),
                Feed::Raw(bytes) => Frame::PaneRaw {
                    pane,
                    bytes: bytes.to_vec(),
                },
            };
            tx.try_send(frame.encode()).is_ok()
        }),
    );
    let (hub, tx) = (Arc::clone(hub), reply.tx.clone());
    tokio::spawn(async move {
        let exit = tokio::select! {
            exit = pty.wait() => exit,
            () = tx.closed() => return,
        };
        let mut output = pty.output_receiver();
        let eof = async { while output.changed().await.is_ok() {} };
        let _ = tokio::time::timeout(EXIT_GRACE, eof).await;
        if hub.stopping.load(Ordering::SeqCst) {
            return;
        }
        let code = exit.code as i32;
        let _ = tx
            .send(encode_json(&Event::PaneExited { pane, code }))
            .await;
        pty.untap(cid);
    });
    ControlFlow::Continue(())
}

fn dispatch(hub: &Arc<Hub>, request: Request) -> Response {
    match request {
        Request::WorkspaceNew { dir } => {
            // Refuse to mount a directory that is not on disk: every pane spawn
            // in it would fail, leaving a dead sidebar entry.
            if !dir.is_dir() {
                return Response::Error {
                    message: format!("directory does not exist: {}", dir.display()),
                };
            }
            let id = hub.session().workspace_new(dir);
            refresh_changes(hub, id);
            Response::WorkspaceCreated { id }
        }
        Request::WorkspaceList => Response::Workspaces {
            workspaces: hub.session().workspace_list(),
        },
        Request::SessionRestore { discard } => session_restore(hub, discard),
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
        Request::Run {
            workspace,
            dir,
            cmd,
            sandbox,
            title,
            issue,
            from,
        } => {
            let target = RunTarget {
                workspace,
                dir,
                from,
            };
            run_in_fresh_tab(hub, target, cmd, sandbox, title, issue)
        }
        Request::PaneRun {
            tab,
            cmd,
            ephemeral,
            sandbox,
        } => {
            let declared = declared_agent(hub, sandbox.as_deref(), &cmd);
            spawn_pane(hub, |s| {
                let pane = s.pane_run(tab, cmd, ephemeral, sandbox)?;
                s.set_agent(pane, declared);
                Ok(pane)
            })
        }
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
                    hub.last().remove(&pane);
                    let _ = hub.signals.send(PaneSignal::Gone(pane));
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
            expect,
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
            // The guard's check and write share one session guard, so no other
            // send can slip between them.
            let r = match &expect {
                Some(expect) => hub.session().pane_answer(pane, &bytes, expect),
                None => pane_input(hub, pane, &bytes),
            };
            match r {
                Ok(()) => Response::Ok,
                Err(err) => error(err),
            }
        }
        Request::PaneFingerprint { pane } => {
            let pty = hub.session().pty(pane);
            match pty {
                Some(pty) => Response::Fingerprint {
                    fingerprint: prompt_fingerprint(&pty.snapshot().lines),
                },
                None => Response::Error {
                    message: format!("no pane {pane}"),
                },
            }
        }
        Request::PaneRead {
            pane,
            lines,
            unwrapped,
            since,
            last_output,
        } => {
            // Bound first: a read can format the whole 10k-row ring, and the
            // session lock must not be held across it.
            let pty = hub.session().pty(pane);
            let Some(pty) = pty else {
                return Response::Error {
                    message: format!("no pane {pane}"),
                };
            };
            if last_output {
                return match (since, pty.last_output(lines, unwrapped)) {
                    (Some(_), _) => Response::Error {
                        message: "a read takes since or last_output, not both".into(),
                    },
                    (None, Ok(Some(out))) => Response::LastOutput {
                        lines: out.lines,
                        exit: out.exit,
                        truncated: out.truncated,
                    },
                    (None, Ok(None)) => Response::Error {
                        message: format!(
                            "no finished command in pane {pane} (needs shell integration: tutti \
                             shell-init)"
                        ),
                    },
                    (None, Err(err)) => error(err),
                };
            }
            match (pty.tail(since.unwrap_or(0), lines, unwrapped), since) {
                (Ok(tail), None) => Response::Content { lines: tail.lines },
                (Ok(tail), Some(_)) => Response::Tail {
                    lines: tail.lines,
                    cursor: tail.cursor,
                    truncated: tail.truncated,
                },
                (Err(err), _) => error(err),
            }
        }
        Request::PaneResizeSplit {
            pane,
            direction,
            delta,
        } => match resize_split(hub, pane, direction, delta) {
            Ok(_) => Response::Ok,
            Err(err) => error(err),
        },
        Request::PaneResizeToward {
            from,
            toward,
            delta,
        } => {
            let pane = match source_pane(hub, from) {
                Ok(pane) => pane,
                Err(err) => return error(err),
            };
            let (axis, delta) = match toward {
                Toward::Left => (Direction::Horizontal, -delta),
                Toward::Right => (Direction::Horizontal, delta),
                Toward::Up => (Direction::Vertical, -delta),
                Toward::Down => (Direction::Vertical, delta),
            };
            match resize_split(hub, pane, axis, delta) {
                // No split of tutti's that way: a client that hands moves on
                // resizes its terminal's split instead.
                Ok(false) => {
                    let edge = Event::ResizeEdge { pane, toward };
                    relay_edge(hub, pane, edge, |client| client.handoff);
                    Response::Ok
                }
                Ok(true) => Response::Ok,
                Err(err) => error(err),
            }
        }
        Request::View => Response::View {
            workspaces: hub.session().view(),
        },
        Request::PaneSplitRatio { a, b, ratio } => split_ratio(hub, a, b, ratio),
        Request::AgentEvent {
            pane,
            event,
            conversation,
        } => agent_event(hub, pane, event, conversation),
        Request::ApprovalAnswer { pane, id, decision } => approval_answer(hub, pane, id, decision),
        Request::PaneCheckpoints { pane } => {
            let checkpoints = hub.session().turns(pane).map(|t| t.entries().to_vec());
            match checkpoints {
                Some(checkpoints) => Response::Checkpoints { checkpoints },
                None => Response::Error {
                    message: format!("no pane {pane}"),
                },
            }
        }
        Request::NotesList { unread } => Response::Notes {
            notes: hub
                .notes()
                .ring
                .iter()
                .filter(|n| !(unread && n.read))
                .cloned()
                .collect(),
        },
        Request::NotesRead { up_to, pane } => {
            mark_read(hub, |n| {
                up_to.is_none_or(|id| n.id <= id) && pane.is_none_or(|p| n.pane == p)
            });
            Response::Ok
        }
        Request::NotesClear => {
            hub.notes().ring.clear();
            broadcast_event(hub, Event::NotesChanged { notes: Vec::new() });
            Response::Ok
        }
        // The panes are bound under the session guard and told off it.
        Request::HostColors(colors) => {
            let ptys = {
                let mut session = hub.session();
                session.set_colors(colors);
                session.panes_with_pty()
            };
            for (_, pty) in ptys {
                pty.set_colors(colors);
            }
            Response::Ok
        }
        // Handled in `handle_frame`; unreachable here.
        Request::Attach { .. }
        | Request::Detach
        | Request::ViewSizes { .. }
        | Request::PaneAttach { .. }
        | Request::Watch
        | Request::PaneFocus { .. }
        | Request::PaneFocusToward { .. }
        | Request::PaneScroll { .. }
        | Request::WorkspaceDiff { .. }
        | Request::ApprovalHold { .. }
        | Request::PaneWait { .. }
        | Request::WorkspaceKill { .. }
        | Request::WorkspaceFork { .. }
        | Request::WorkspaceMerge { .. }
        | Request::WorkspaceUpdate { .. }
        | Request::PaneCheckpointDiff { .. }
        | Request::PaneRestore { .. }
        | Request::SandboxPull { .. } => Response::Ok,
    }
}

/// Adjust the ratio of the nearest matching-axis split enclosing `pane` and
/// broadcast the fresh view; whether there was such a split. The attached
/// client re-syncs pane sizes off the new layout, so the ptys resize and
/// reseed on the next tick.
fn resize_split(hub: &Arc<Hub>, pane: PaneId, axis: Direction, delta: f32) -> Result<bool> {
    let mut session = hub.session();
    let resized = session.pane_resize_split(pane, axis, delta)?;
    if resized {
        let view = session.view();
        drop(session);
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    Ok(resized)
}

/// Type `bytes` into `pane` (an `Input` frame or a plain `PaneSend`),
/// rebroadcasting the view when that settled a finished program's report out
/// of the agent surfaces (`Session::pane_send`).
fn pane_input(hub: &Hub, pane: PaneId, bytes: &[u8]) -> Result<()> {
    let mut session = hub.session();
    let view = session.pane_send(pane, bytes)?.then(|| session.view());
    drop(session);
    if let Some(view) = view {
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    Ok(())
}

/// Set the ratio of the split separating `a` from `b` — a border drag — and
/// broadcast the fresh view when it moved, as `resize_split` does.
fn split_ratio(hub: &Arc<Hub>, a: PaneId, b: PaneId, ratio: f32) -> Response {
    let mut session = hub.session();
    match session.pane_split_ratio(a, b, ratio) {
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
/// `Done` pane to `Idle` (marked seen), and mark its notes read. Broadcasts the
/// transition if it changed — and the view, when that settles a self-reported
/// pane out of the agent surfaces. An attached client's focus is recorded as
/// what it shows; anyone else's (a script, a notch app's click) is relayed as
/// `PaneFocused`, so the attached client jumps there.
fn focus_pane(hub: &Arc<Hub>, cid: u64, pane: PaneId) -> Response {
    let mut session = hub.session();
    if session.set_active_pane(pane).is_err() {
        return Response::Error {
            message: format!("no pane {pane}"),
        };
    }
    let from = session.pane_state(pane).expect("pane exists");
    let to = from.apply(StateEvent::Focused);
    session.set_pane_state(pane, to);
    let view = session.settle(pane).then(|| session.view());
    drop(session);
    if let Some(view) = view {
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    publish_transition(hub, pane, from, to);
    mark_read(hub, |n| n.pane == pane);
    let attached = match hub.clients().get_mut(&cid) {
        Some(client) => {
            client.focused = Some(pane);
            true
        }
        None => false,
    };
    if !attached {
        broadcast_event(hub, Event::PaneFocused { pane });
    }
    Response::Ok
}

/// Focus the pane `toward` of `from` as `focus_pane` does, and answer which it
/// was — `None` at an edge, where nothing moves here. At an edge (no `wrap`),
/// each attached client focused on `from` is handed the move (`FocusEdge`):
/// an editor holding the direct keys then reaches the sidebar, the changes
/// panel or the next tab, as the key itself would — or, from a client that
/// hands moves on, the rtty pane beyond, at the top and bottom edges too.
fn focus_toward(
    hub: &Arc<Hub>,
    cid: u64,
    from: Option<PaneId>,
    toward: Toward,
    wrap: bool,
) -> Response {
    let from = match source_pane(hub, from) {
        Ok(from) => from,
        Err(err) => return error(err),
    };
    // Bound first: `focus_pane` re-locks the session.
    let neighbor = hub.session().pane_neighbor(from, toward, wrap);
    let pane = match neighbor {
        Ok(pane) => pane,
        Err(err) => return error(err),
    };
    let sideways = matches!(toward, Toward::Left | Toward::Right);
    let relayed = pane.is_none()
        && !wrap
        && relay_edge(
            hub,
            from,
            Event::FocusEdge { pane: from, toward },
            |client| sideways || client.handoff,
        );
    match pane.map(|pane| focus_pane(hub, cid, pane)) {
        None | Some(Response::Ok) => Response::Neighbor {
            from,
            toward,
            pane,
            relayed,
        },
        Some(refusal) => refusal,
    }
}

/// Send `edge` to each attached client focused on `pane` that `takes` it;
/// whether any did.
fn relay_edge(hub: &Hub, pane: PaneId, edge: Event, takes: impl Fn(&Client) -> bool) -> bool {
    let bytes = encode_json(&edge);
    let mut doomed = Vec::new();
    let mut sent = false;
    {
        let clients = hub.clients();
        let focused = |c: &&Client| c.focused == Some(pane) && takes(c);
        for (cid, client) in clients.iter().filter(|(_, c)| focused(c)) {
            match client.tx.try_send(bytes.clone()) {
                Ok(()) => sent = true,
                Err(_) => doomed.push(*cid),
            }
        }
    }
    disconnect(hub, doomed);
    sent
}

/// The pane a directional request starts from: `from`, else the attached
/// client's focused pane — the newest client's, as a watcher's snapshot
/// names it.
fn source_pane(hub: &Hub, from: Option<PaneId>) -> Result<PaneId> {
    from.or_else(|| {
        hub.clients()
            .iter()
            .max_by_key(|(cid, _)| **cid)
            .and_then(|(_, client)| client.focused)
    })
    .context("no pane to start from: name one, or focus one in an attached client")
}

/// Publish one pane's state transition — the single funnel every transition
/// source goes through, so each one gets checkpoints, waiters and the
/// notification history for free: broadcast `StateChanged`, signal the `pane
/// wait`s, note the turn edge on the pane (leaving `Working` ends a turn;
/// entering it with nothing recorded takes the baseline), and note one that
/// wants attention — into `Blocked`, or `Working` → `Done` — in the history.
/// The caller then refreshes the pane's workspace (batched per pass where it
/// can): that probe's snapshot of `@` is what gets recorded. A no-op when
/// `from == to`.
fn publish_transition(hub: &Hub, pane: PaneId, from: AgentState, to: AgentState) {
    if from == to {
        return;
    }
    hub.session().note_transition(pane, from, to);
    broadcast_event(hub, Event::StateChanged { pane, from, to });
    let _ = hub.signals.send(PaneSignal::Moved { pane, from, to });
    let kind = match (from, to) {
        (_, AgentState::Blocked) => NoteKind::Blocked,
        (AgentState::Working, AgentState::Done) => NoteKind::Done,
        _ => return,
    };
    add_note(hub, pane, kind, None, None);
}

/// Note a pane's bell or desktop notification in the history, then broadcast
/// it — in that order, so a client attaching in between finds the note in the
/// history it is handed.
fn pane_notification(hub: &Hub, pane: PaneId, title: Option<String>, body: Option<String>) {
    let kind = if title.is_none() && body.is_none() {
        NoteKind::Bell
    } else {
        NoteKind::Notify
    };
    add_note(hub, pane, kind, title.clone(), body.clone());
    broadcast_event(hub, Event::PaneNotification { pane, title, body });
}

/// Record a note about `pane` in the history and broadcast it as `Noted`. It
/// keeps the pane's title and workspace name as they are now, so it outlives
/// the pane. A note about a pane an attached client is looking at lands read —
/// and a bell there, which says nothing more, lands not at all; nor does a
/// bell repeating its pane's newest note, an unread bell, so a bell storm
/// cannot flush the history. Each lock is taken and released on its own.
fn add_note(hub: &Hub, pane: PaneId, kind: NoteKind, title: Option<String>, body: Option<String>) {
    // Read straight off the model, as a watcher's snapshot is.
    let names = hub
        .session()
        .watch_panes(None)
        .into_iter()
        .find(|p| p.id == pane);
    let Some(names) = names else {
        return;
    };
    let seen = hub.clients().values().any(|c| c.focused == Some(pane));
    let bell = kind == NoteKind::Bell;
    if bell && seen {
        return;
    }
    let note = {
        let mut notes = hub.notes();
        let repeat = notes
            .ring
            .iter()
            .rev()
            .find(|n| n.pane == pane)
            .is_some_and(|n| n.kind == NoteKind::Bell && !n.read);
        if bell && repeat {
            return;
        }
        notes.last += 1;
        let note = Note {
            id: notes.last,
            time: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            pane,
            pane_title: names.title,
            workspace: names.workspace,
            kind,
            title,
            body,
            read: seen,
        };
        notes.ring.push(note.clone());
        if notes.ring.len() > NOTES_CAP {
            notes.ring.remove(0);
        }
        note
    };
    broadcast_event(hub, Event::Noted { note });
}

/// Mark the notes `which` picks read and, when any of them was unread,
/// broadcast the history so every client agrees.
fn mark_read(hub: &Hub, which: impl Fn(&Note) -> bool) {
    let notes = {
        let mut notes = hub.notes();
        let mut moved = false;
        for note in notes.ring.iter_mut().filter(|n| !n.read && which(n)) {
            note.read = true;
            moved = true;
        }
        moved.then(|| notes.ring.clone())
    };
    if let Some(notes) = notes {
        broadcast_event(hub, Event::NotesChanged { notes });
    }
}

/// Fold a program's own status report (OSC 9;4 / 7501, pulled from `pane`'s
/// output) onto its state under the source ranking, then publish it.
fn program_status(hub: &Arc<Hub>, pane: PaneId, report: StatusReport) {
    let outcome = hub.session().apply_report(pane, report, Instant::now());
    publish_report(hub, pane, outcome);
}

/// Apply one herdr request (see `herdr`): a lifecycle report folds onto the
/// pane's state much as a program's own report does and is published the
/// same way; a notification is raised on its pane. `false` for an unknown
/// pane.
fn herdr_request(hub: &Arc<Hub>, request: herdr::Request) -> bool {
    match request {
        herdr::Request::Report(pane, report) => {
            let outcome = hub.session().apply_herdr(pane, report, Instant::now());
            let Some(outcome) = outcome else {
                return false;
            };
            publish_report(hub, pane, outcome);
            true
        }
        herdr::Request::Notify { pane, title, body } => {
            let known = hub.session().pane_state(pane).is_some();
            if known {
                pane_notification(hub, pane, Some(title), body);
            }
            known
        }
    }
}

/// Broadcast what a status report moved: the view first when the pane's
/// agent-surface listing changed (so the client has the row before its
/// state), the transition, and the report's `msg` as a notification. The
/// session guard is dropped before any broadcast. A state move refreshes the
/// workspace, as a hook's does: that probe records the turn's checkpoint, and
/// entering `Working` must probe too, or a first turn's baseline would only be
/// snapshotted at its end. Progress ticks within one state move nothing, so a
/// progress bar never runs jj.
fn publish_report(hub: &Arc<Hub>, pane: PaneId, outcome: ReportOutcome) {
    let view = {
        let mut session = hub.session();
        (outcome.relisted | session.settle(pane)).then(|| session.view())
    };
    if let Some(view) = view {
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    if let Some((from, to)) = outcome.transition {
        publish_transition(hub, pane, from, to);
        refresh_pane_workspace(hub, pane);
    }
    if let Some((title, body)) = outcome.notify {
        pane_notification(hub, pane, title, Some(body));
    }
}

/// Ingest an agent hook event for `pane`: fold it onto the pane's exact state,
/// subagent list and conversation, then broadcast what moved. A subagent-list
/// or conversation change broadcasts the fresh view (`LayoutChanged`); a state
/// move broadcasts `StateChanged` so a Blocked/Done still rings the client
/// bell. Both can fire. A state move also refreshes the workspace's changes,
/// which records the turn's checkpoint. An unknown pane is a silent `Ok` — the
/// hook must never fail on tutti's side. The session guard is dropped before
/// any broadcast (which re-locks `clients`).
fn agent_event(
    hub: &Arc<Hub>,
    pane: PaneId,
    event: AgentHookEvent,
    conversation: Option<String>,
) -> Response {
    let mut session = hub.session();
    let Some(outcome) = session.apply_agent_event(pane, event, conversation) else {
        return Response::Ok;
    };
    let view = outcome.relisted.then(|| session.view());
    drop(session);
    if let Some(view) = view {
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    if let Some((from, to)) = outcome.transition {
        publish_transition(hub, pane, from, to);
        refresh_pane_workspace(hub, pane);
    }
    Response::Ok
}

/// Hold a permission request on `pane` until it is answered. The approval is
/// registered, and the view carrying it broadcast, before the pane blocks —
/// as the request's own hook event would block it — so whoever that move
/// wakes (a `pane wait`) finds an approval to answer. A spawned task then
/// holds the reply until an answer comes down the approval's channel or the
/// hook hangs up; either way the approval leaves the pane. With no client
/// attached nobody could answer: the reply is `Pass` at once, as it is for an
/// unknown pane or one already holding an approval.
fn approval_hold(
    hub: &Arc<Hub>,
    cid: u64,
    reply: &Reply,
    pane: PaneId,
    conversation: Option<String>,
    tool: String,
    summary: String,
) -> ControlFlow<()> {
    let message = Some(format!("{tool} · {summary}"));
    let attached = !hub.clients().is_empty();
    let held = if attached {
        hub.session().hold_approval(pane, tool, summary)
    } else {
        None
    };
    let (answer, answered) = oneshot::channel();
    if let Some(id) = held {
        hub.approvals().insert(id, answer);
        let view = hub.session().view();
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    agent_event(hub, pane, AgentHookEvent::Blocked { message }, conversation);
    let Some(id) = held else {
        let pass = Response::Approval {
            decision: ApprovalDecision::Pass,
        };
        return send_reply(hub, cid, &reply.tx, reply.encode(&pass));
    };
    let hub = Arc::clone(hub);
    let reply = reply.clone();
    tokio::spawn(async move {
        let decision = tokio::select! {
            () = reply.tx.closed() => None,
            decision = answered => decision.ok(),
        };
        hub.approvals().remove(&id);
        let view = {
            let mut session = hub.session();
            session
                .take_approval(pane, Some(id))
                .map(|_| session.view())
        };
        if let Some(view) = view {
            broadcast_event(&hub, Event::LayoutChanged { workspaces: view });
        }
        if let Some(decision) = decision {
            reply.send(Response::Approval { decision }).await;
        }
    });
    ControlFlow::Continue(())
}

/// Answer `pane`'s held approval — `id`'s, or whichever it holds when `None`:
/// end it, send the decision down its channel to the hook holding the
/// request (one that has just hung up hears nothing, which is fine), and
/// broadcast the view without it. An allow or a deny sets the agent going
/// again — the tool runs, or the model hears the refusal — and Claude sends
/// no hook until that tool is done, so the pane moves to Working here rather
/// than sitting Blocked through it; a pass hands the request to Claude's own
/// dialog, still blocked. Error when the pane holds no such approval.
fn approval_answer(
    hub: &Arc<Hub>,
    pane: PaneId,
    id: Option<u64>,
    decision: ApprovalDecision,
) -> Response {
    let mut session = hub.session();
    let Some(held) = session.take_approval(pane, id) else {
        let which = id.map_or_else(String::new, |id| format!(" #{id}"));
        return Response::Error {
            message: format!("pane {pane} has no pending approval{which}"),
        };
    };
    let view = session.view();
    drop(session);
    let resumes = !matches!(decision, ApprovalDecision::Pass);
    let answer = hub.approvals().remove(&held);
    if let Some(answer) = answer {
        let _ = answer.send(decision);
    }
    broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    if resumes {
        agent_event(hub, pane, AgentHookEvent::Activity { detail: None }, None);
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

/// The agent a sandbox pane declares from its command: it runs in a pod, out
/// of detection's sight. `None` for a local pane, which detection finds.
fn declared_agent(hub: &Hub, sandbox: Option<&str>, cmd: &[String]) -> Option<AgentKind> {
    sandbox
        .and(cmd.first())
        .and_then(|program| hub.registry.match_process(program))
        .map(|spec| spec.kind.clone())
}

/// `tutti run`: spawn `cmd` in a fresh tab of the target workspace (see
/// `Session::run_in_fresh_tab`) through `spawn_pane`, so the reaper and the
/// one `LayoutChanged` come with it. A workspace it mounted gets its change
/// stat seeded, as `WorkspaceNew` does.
fn run_in_fresh_tab(
    hub: &Arc<Hub>,
    target: RunTarget,
    cmd: Vec<String>,
    sandbox: Option<String>,
    title: Option<String>,
    issue: Option<IssueRef>,
) -> Response {
    let declared = declared_agent(hub, sandbox.as_deref(), &cmd);
    let mut ran = None;
    let response = spawn_pane(hub, |s| {
        let (workspace, tab, pane, mounted) =
            s.run_in_fresh_tab(target, cmd, sandbox, title, issue)?;
        s.set_agent(pane, declared);
        ran = Some((workspace, tab, mounted));
        Ok(pane)
    });
    match (response, ran) {
        (Response::PaneCreated { id }, Some((workspace, tab, mounted))) => {
            if mounted {
                refresh_changes(hub, workspace);
            }
            Response::Ran {
                workspace,
                tab,
                pane: id,
            }
        }
        (response, _) => response,
    }
}

/// Bring back the layout the previous daemon saved (`Session::restore`), or
/// with `discard` drop it. The offer is read, restored and deleted under one
/// session guard, so two clients racing to restore bring it back once. The
/// fresh panes get their reapers, the clients one `LayoutChanged`, and each
/// workspace it touched a change probe — which also records where each
/// revived agent starts.
fn session_restore(hub: &Arc<Hub>, discard: bool) -> Response {
    let nothing = || Response::Error {
        message: "nothing to restore".into(),
    };
    let Some(dir) = hub.state.as_deref() else {
        return nothing();
    };
    let mut session = hub.session();
    let restored = if discard {
        Restored::default()
    } else {
        match persist::read_offer(dir, &hub.name) {
            Some(saved) => session.restore(saved, SavedPane::revive),
            None => return nothing(),
        }
    };
    match std::fs::remove_file(persist::offer_path(dir, &hub.name)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            eprintln!("tutti: cannot remove the offered layout: {err}");
        }
        _ => {}
    }
    let reapers: Vec<(PaneId, Arc<PtyPane>)> = restored
        .panes
        .iter()
        .filter_map(|&pane| session.pty(pane).map(|pty| (pane, pty)))
        .collect();
    let view = (!restored.workspaces.is_empty()).then(|| session.view());
    drop(session);
    for (pane, pty) in reapers {
        spawn_reaper(hub, pane, pty);
    }
    if let Some(view) = view {
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    for &workspace in &restored.workspaces {
        refresh_changes(hub, workspace);
    }
    Response::SessionRestored {
        workspaces: restored.workspaces.len(),
        panes: restored.panes.len(),
        revived: restored.revived,
        skipped: restored.skipped,
    }
}

/// Resize a pane's pty and grid — a no-op at the size it has — then force a
/// fresh baseline-paired snapshot for every client (clear the last broadcast
/// screen and each client's seen set) so the next tick reseeds their parsers
/// at the new size.
fn resize_pane(hub: &Hub, pane: PaneId, rows: u16, cols: u16) -> Response {
    let pty = match hub.session().pty(pane) {
        Some(pty) => pty,
        None => {
            return Response::Error {
                message: format!("no pane {pane}"),
            };
        }
    };
    if pty.size() == PaneSize::new(rows, cols) {
        return Response::Ok;
    }
    match pty.resize(rows, cols) {
        Ok(()) => {
            hub.last().remove(&pane);
            for client in hub.clients().values_mut() {
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
        if let Some(client) = hub.clients().get_mut(&cid) {
            client.seen.remove(&pane);
        }
        return ControlFlow::Continue(());
    }
    let Some(pty) = hub.session().pty(pane) else {
        return ControlFlow::Continue(());
    };
    let (size, snapshot) = pty.scrolled_snapshot(offset);
    let frame = Frame::PaneSnapshot(PaneData {
        pane,
        rows: size.rows,
        cols: size.cols,
        seq: 0,
        bytes: snapshot,
    });
    send_reply(hub, cid, tx, frame.encode())
}

/// Serve a workspace diff. jj is shelled out to on a spawned task so a slow (or
/// large) diff never stalls this connection's frame loop; the reply lands on the
/// requester's own outbound channel when it is ready. A vanished workspace
/// answers Error without touching jj.
fn workspace_diff(hub: &Arc<Hub>, reply: &Reply, id: WorkspaceId, stat: bool) -> ControlFlow<()> {
    let dir = hub.session().workspace_dir(id);
    let reply = reply.clone();
    tokio::spawn(async move {
        let response = match dir {
            Some(dir) => jj::diff(&dir, stat).await,
            None => Response::Error {
                message: format!("no workspace {id}"),
            },
        };
        reply.send(response).await;
    });
    ControlFlow::Continue(())
}

/// Serve a `pane wait` on a spawned task, replying on the requester's own
/// channel like `workspace_diff`. No polling: it subscribes to the pane
/// signals *before* reading the pane, so a transition landing between the two
/// is queued rather than lost, then answers on the first move into an `until`
/// state — or at once, when the pane already holds one and `next` is off.
/// Under `next` a move counts only when it enters a state from outside it, so
/// a human focusing a Done pane (Done → Idle) is never a fresh finish. The
/// child exiting ends the wait (`wait_verdict`); so does the pane leaving. A
/// lagging subscription re-checks the pane as it is now, level-triggered —
/// best effort: an edge it skipped is lost. The task gives up silently once
/// its client hangs up.
fn pane_wait(
    hub: &Arc<Hub>,
    reply: &Reply,
    pane: PaneId,
    until: Vec<WaitFor>,
    next: bool,
    timeout_ms: Option<u64>,
) -> ControlFlow<()> {
    let mut signals = hub.signals.subscribe();
    let start = hub.session().pane(pane);
    let hub = Arc::clone(hub);
    let reply = reply.clone();
    tokio::spawn(async move {
        let gone = || Response::Error {
            message: format!("pane {pane} is gone"),
        };
        let wait = async {
            if until.is_empty() {
                return Response::Error {
                    message: "pane wait needs a state to wait for".into(),
                };
            }
            let Some(info) = start else {
                return Response::Error {
                    message: format!("no pane {pane}"),
                };
            };
            if let Some(answer) = wait_verdict(info, &until, !next) {
                return answer;
            }
            loop {
                let signal = signals.recv().await;
                let recheck = match signal {
                    Ok(PaneSignal::Moved { pane: p, from, to }) if p == pane => {
                        if until
                            .iter()
                            .any(|w| w.matches(to) && !(next && w.matches(from)))
                        {
                            let info = hub.session().pane(pane);
                            return match info {
                                Some(info) => Response::Waited {
                                    pane: PaneInfo { state: to, ..info },
                                    timed_out: false,
                                },
                                None => gone(),
                            };
                        }
                        false
                    }
                    Ok(PaneSignal::Exited(p)) => p == pane,
                    Ok(PaneSignal::Gone(p)) if p == pane => return gone(),
                    Ok(PaneSignal::Moved { .. } | PaneSignal::Gone(_)) => false,
                    Err(RecvError::Lagged(_)) => true,
                    Err(RecvError::Closed) => unreachable!("the hub owns the sender"),
                };
                if !recheck {
                    continue;
                }
                let info = hub.session().pane(pane);
                let Some(info) = info else {
                    return gone();
                };
                if let Some(answer) = wait_verdict(info, &until, true) {
                    return answer;
                }
            }
        };
        let bounded = async {
            match timeout_ms {
                Some(ms) => tokio::time::timeout(Duration::from_millis(ms), wait)
                    .await
                    .ok(),
                None => Some(wait.await),
            }
        };
        let response = tokio::select! {
            () = reply.tx.closed() => return,
            outcome = bounded => match outcome {
                Some(response) => response,
                None => {
                    let info = hub.session().pane(pane);
                    info.map_or_else(gone, |pane| Response::Waited {
                        pane,
                        timed_out: true,
                    })
                }
            },
        };
        reply.send(response).await;
    });
    ControlFlow::Continue(())
}

/// Whether a wait on the pane `info` describes is over, and its answer: a
/// pane already in an `until` state ends it when `level`. An exited pane ends
/// it too — as an error naming the exit — unless the one move it has left, a
/// human's look turning Done into Idle, is what the wait is for.
fn wait_verdict(info: PaneInfo, until: &[WaitFor], level: bool) -> Option<Response> {
    if level && until.iter().any(|w| w.matches(info.state)) {
        return Some(Response::Waited {
            pane: info,
            timed_out: false,
        });
    }
    let code = info.exited?;
    if info.state == AgentState::Done && until.contains(&WaitFor::Idle) {
        return None;
    }
    let state = format!("{:?}", info.state).to_lowercase();
    Some(Response::Error {
        message: format!("pane {} exited with code {code} (state: {state})", info.id),
    })
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
    reply: &Reply,
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
            &reply.tx,
            reply.encode(&Response::Error {
                message: "refusing to discard a workspace tutti did not create".into(),
            }),
        );
    }
    match session.workspace_kill(id) {
        Ok(panes) => {
            let view = session.view();
            drop(session);
            let mut last = hub.last();
            for pane in &panes {
                last.remove(pane);
                let _ = hub.signals.send(PaneSignal::Gone(*pane));
            }
            drop(last);
            broadcast_event(hub, Event::LayoutChanged { workspaces: view });
            if !discard {
                return send_reply(hub, cid, &reply.tx, reply.encode(&Response::Ok));
            }
            // Fork discard: forget at the origin, then remove the checkout. Each
            // failure is surfaced but neither aborts the other, so a half-cleaned
            // fork has still left tutti (the LayoutChanged above already dropped
            // it). The reply follows the cleanup on the requester's channel.
            let fork = fork.expect("discard on a non-fork was rejected above");
            let reply = reply.clone();
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
                reply.send(response).await;
            });
            ControlFlow::Continue(())
        }
        Err(err) => {
            drop(session);
            send_reply(hub, cid, &reply.tx, reply.encode(&error(err)))
        }
    }
}

/// What a new workspace checks out: a revision of the repo (`None`: jj's
/// default), or a branch first fetched from the repo's remote — a sandbox's
/// pushed work.
enum Checkout {
    Revision(Option<String>),
    Fetched(String),
}

/// Fork a jj workspace into a named checkout and mount it. `dest`, when given, is
/// the client-chosen absolute checkout directory (its parent already
/// canonicalized client-side); when absent the server places a named sibling of
/// the repo root. Validation (name shape, source is a jj repo, an absolute
/// destination that does not yet exist) is synchronous; the fetch a `checkout`
/// may need and the `jj workspace add` — which materializes a working copy —
/// run on a spawned task so they never stall the frame loop, and the
/// `WorkspaceCreated` reply lands on the requester's channel once the
/// checkout's shell pane is up.
fn workspace_fork(
    hub: &Arc<Hub>,
    cid: u64,
    reply: &Reply,
    id: WorkspaceId,
    name: String,
    checkout: Checkout,
    dest: Option<PathBuf>,
) -> ControlFlow<()> {
    if !jj::valid_fork_name(&name) {
        return send_reply(
            hub,
            cid,
            &reply.tx,
            reply.encode(&Response::Error {
                message: format!(
                    "invalid workspace name {name:?}: use letters, digits, '-' or '_'"
                ),
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
            &reply.tx,
            reply.encode(&Response::Error {
                message: format!("no workspace {id}"),
            }),
        );
    };
    let Some(repo_root) = jj::workspace_root(&source) else {
        return send_reply(
            hub,
            cid,
            &reply.tx,
            reply.encode(&Response::Error {
                message: format!("not a jj workspace: {}", source.display()),
            }),
        );
    };
    // A client-chosen destination must be absolute (its parent is canonicalized
    // client-side); with none given, place a named sibling of the repo root.
    let dest = match dest {
        Some(dest) if !dest.is_absolute() => {
            return send_reply(
                hub,
                cid,
                &reply.tx,
                reply.encode(&Response::Error {
                    message: format!("workspace destination must be absolute: {}", dest.display()),
                }),
            );
        }
        Some(dest) => dest,
        None => {
            let Some(dest) = jj::fork_dest(&repo_root, &name) else {
                return send_reply(
                    hub,
                    cid,
                    &reply.tx,
                    reply.encode(&Response::Error {
                        message: format!("cannot place a workspace beside {}", repo_root.display()),
                    }),
                );
            };
            dest
        }
    };
    if dest.exists() {
        return send_reply(
            hub,
            cid,
            &reply.tx,
            reply.encode(&Response::Error {
                message: format!("workspace destination already exists: {}", dest.display()),
            }),
        );
    }

    let hub = Arc::clone(hub);
    let reply = reply.clone();
    tokio::spawn(async move {
        let forked = async {
            let revision = match &checkout {
                Checkout::Revision(revision) => revision.clone(),
                Checkout::Fetched(branch) => Some(jj::fetch_branch(&repo_root, branch).await?),
            };
            jj::fork(&repo_root, &dest, &name, revision.as_deref()).await
        };
        if let Err(message) = forked.await {
            reply.send(Response::Error { message }).await;
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
        spawn_pane(&hub, |s| s.pane_run(Some(tab_id), vec![shell], false, None));
        // Seed the fork's change stat / stale flag now that it exists.
        refresh_changes(&hub, ws_id);
        reply.send(Response::WorkspaceCreated { id: ws_id }).await;
    });
    ControlFlow::Continue(())
}

/// Pull a sandbox pane's pushed work into a workspace of its own: the branch
/// its sandbox pushes to, fetched from the repo's remote and checked out by
/// `workspace_fork` beside the pane's workspace — named `name`, else after the
/// sandbox — so its diff and merge work as any workspace's do.
fn sandbox_pull(
    hub: &Arc<Hub>,
    cid: u64,
    reply: &Reply,
    pane: PaneId,
    name: Option<String>,
) -> ControlFlow<()> {
    let found = {
        let session = hub.session();
        session
            .pane(pane)
            .map(|info| (info.sandbox_name, session.workspace_of_pane(pane)))
    };
    let message = match found {
        Some((Some(sandbox), Some(id))) => {
            let branch = tutti_core::sandbox_branch(&sandbox);
            let name = name.unwrap_or(sandbox);
            return workspace_fork(hub, cid, reply, id, name, Checkout::Fetched(branch), None);
        }
        Some((None, _)) => format!("pane {pane} is not a sandbox pane"),
        _ => format!("no pane {pane}"),
    };
    send_reply(
        hub,
        cid,
        &reply.tx,
        reply.encode(&Response::Error { message }),
    )
}

/// Merge a child workspace's work into its origin's trunk bookmark. Only a
/// workspace tutti forked (one with fork metadata) can merge — a plain project
/// answers Error. The jj sequence (rebase, conflict-guarded, bookmark advance,
/// optional push) runs on a spawned task in the origin repo; on success it
/// broadcasts the fresh view, refreshes the child's stat, and replies `Merged`
/// (carrying the trunk it advanced and whether it pushed). A push failure is a
/// note logged server-side — the merge still landed.
fn workspace_merge(
    hub: &Arc<Hub>,
    cid: u64,
    reply: &Reply,
    id: WorkspaceId,
    push: bool,
) -> ControlFlow<()> {
    // Bind both lookups before dropping the guard; neither re-locks the Mutex.
    let session = hub.session();
    let fork = session.workspace_fork_meta(id);
    let dir = session.workspace_dir(id);
    drop(session);
    let (Some(fork), Some(dir)) = (fork, dir) else {
        return send_reply(
            hub,
            cid,
            &reply.tx,
            reply.encode(&Response::Error {
                message: "only a workspace can merge (this is a top-level project)".into(),
            }),
        );
    };
    let hub = Arc::clone(hub);
    let reply = reply.clone();
    tokio::spawn(async move {
        let response = match jj::merge(&fork.origin_root, &dir, &fork.jj_name, push).await {
            Ok(outcome) => {
                if let Some(note) = &outcome.push_error {
                    eprintln!(
                        "tutti: merge into {} landed but push failed: {note}",
                        outcome.bookmark
                    );
                }
                // The merge advanced the origin's trunk and rewrote the child's
                // commits: rebroadcast the view and refresh the child's stat.
                let view = hub.session().view();
                broadcast_event(&hub, Event::LayoutChanged { workspaces: view });
                refresh_changes(&hub, id);
                Response::Merged {
                    pushed: outcome.pushed,
                    bookmark: outcome.bookmark,
                }
            }
            Err(message) => Response::Error { message },
        };
        reply.send(response).await;
    });
    ControlFlow::Continue(())
}

/// Clear a stale working copy with `jj workspace update-stale`, then refresh the
/// workspace's stale flag. Runs on a spawned task (it touches the working copy)
/// and replies when done.
fn workspace_update(hub: &Arc<Hub>, reply: &Reply, id: WorkspaceId) -> ControlFlow<()> {
    let dir = hub.session().workspace_dir(id);
    let hub = Arc::clone(hub);
    let reply = reply.clone();
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
        reply.send(response).await;
    });
    ControlFlow::Continue(())
}

/// Resolve `pane`'s checkpoint `n` (its newest when `None`) with what acting on
/// it needs: the pane's workspace, that workspace's directory, and the pane's
/// state. The error names whatever is missing.
fn find_checkpoint(
    hub: &Hub,
    pane: PaneId,
    n: Option<u64>,
) -> Result<(WorkspaceId, PathBuf, AgentState, Checkpoint), String> {
    let mut session = hub.session();
    let gone = || format!("no pane {pane}");
    let state = session.pane_state(pane).ok_or_else(gone)?;
    let workspace = session.workspace_of_pane(pane).ok_or_else(gone)?;
    let dir = session.workspace_dir(workspace).ok_or_else(gone)?;
    let turns = session.turns(pane).ok_or_else(gone)?;
    let checkpoint = match n {
        Some(n) => turns.get(n),
        None => turns.entries().last(),
    };
    let checkpoint = checkpoint.cloned().ok_or_else(|| match n {
        Some(n) => format!("pane {pane} has no checkpoint #{n}"),
        None => format!("pane {pane} has no checkpoints yet"),
    })?;
    Ok((workspace, dir, state, checkpoint))
}

/// Serve one checkpoint's turn diff, `from..commit`, on a spawned task like
/// `workspace_diff`. The pane's first recorded state has nothing earlier to
/// diff against and answers Error saying so.
fn checkpoint_diff(
    hub: &Arc<Hub>,
    reply: &Reply,
    pane: PaneId,
    n: Option<u64>,
    stat: bool,
) -> ControlFlow<()> {
    let target = find_checkpoint(hub, pane, n);
    let reply = reply.clone();
    tokio::spawn(async move {
        let response = match target {
            Ok((
                _,
                dir,
                _,
                Checkpoint {
                    from: Some(from),
                    commit,
                    ..
                },
            )) => jj::diff_between(&dir, &from, &commit, stat).await,
            Ok((_, _, _, checkpoint)) => Response::Error {
                message: format!(
                    "#{} is the first state recorded for pane {pane} — nothing earlier to diff",
                    checkpoint.n
                ),
            },
            Err(message) => Response::Error { message },
        };
        reply.send(response).await;
    });
    ControlFlow::Continue(())
}

/// Restore `pane`'s workspace to checkpoint `n`. Refused while the pane is
/// working or blocked — its agent is mid-turn and would write straight over the
/// restore — unless `force`. The jj side runs on a spawned task inside the VCS
/// watch's in-flight window, so its snapshot and restore (operations tutti
/// itself made) never read as outside work; the refresh it ends with re-marks
/// the repo behind them and rebroadcasts the stat.
fn pane_restore(
    hub: &Arc<Hub>,
    reply: &Reply,
    pane: PaneId,
    n: u64,
    force: bool,
) -> ControlFlow<()> {
    let target = find_checkpoint(hub, pane, Some(n)).and_then(|(workspace, dir, state, c)| {
        if !force && matches!(state, AgentState::Working | AgentState::Blocked) {
            let state = format!("{state:?}").to_lowercase();
            return Err(format!(
                "pane {pane} is {state} — restore once its turn ends, or force it"
            ));
        }
        Ok((workspace, dir, c.commit))
    });
    if let Ok((workspace, ..)) = &target {
        hub.vcs().entry(*workspace).or_default().in_flight += 1;
    }
    let hub = Arc::clone(hub);
    let reply = reply.clone();
    tokio::spawn(async move {
        let response = match target {
            Ok((workspace, dir, commit)) => {
                let response = restore_to(&hub, &dir, pane, n, &commit).await;
                refresh_changes(&hub, workspace);
                {
                    let mut vcs = hub.vcs();
                    let entry = vcs.entry(workspace).or_default();
                    entry.in_flight = entry.in_flight.saturating_sub(1);
                }
                response
            }
            Err(message) => Response::Error { message },
        };
        reply.send(response).await;
    });
    ControlFlow::Continue(())
}

/// The jj half of a restore: snapshot `@` (refusing a stale working copy), save
/// it as a `before restore` checkpoint — so the restore is itself undoable from
/// the same list — unless nothing changed since the pane's head, restore
/// checkpoint `n`'s `commit`, and point the pane's next turn at the result.
async fn restore_to(hub: &Hub, dir: &Path, pane: PaneId, n: u64, commit: &str) -> Response {
    let (stale, before) = jj::snapshot_head(dir).await;
    if stale {
        return Response::Error {
            message: "the workspace is stale — update it before restoring".into(),
        };
    }
    let Some(before) = before else {
        return Response::Error {
            message: "jj could not snapshot the working copy".into(),
        };
    };
    let saved = record_checkpoint(hub, dir, pane, &before, "before restore", None).await;
    match jj::restore(dir, commit).await {
        Ok(after) => {
            let mut session = hub.session();
            if let Some(turns) = session.turns(pane) {
                turns.set_head(after);
            }
            drop(session);
            Response::Restored { n, saved }
        }
        Err(message) => Response::Error { message },
    }
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
            hub.last().remove(&pane);
            let _ = hub.signals.send(PaneSignal::Gone(pane));
            broadcast_event(&hub, Event::LayoutChanged { workspaces: view });
            if let Some(workspace) = workspace {
                refresh_changes(&hub, workspace);
            }
            return;
        }
        let transition = hub.session().mark_exited(pane, code);
        if let Some((from, to)) = transition {
            publish_transition(&hub, pane, from, to);
            broadcast_event(&hub, Event::PaneExited { pane, code });
            // Even when the state did not move (a Done pane exiting): a
            // waiter must learn the pane can move no more.
            let _ = hub.signals.send(PaneSignal::Exited(pane));
            // A finished agent likely touched files; refresh its workspace stat.
            refresh_pane_workspace(&hub, pane);
        }
    });
}

/// Recompute a workspace's jj change stat *and* stale flag off the
/// render/dispatch path and, when either moved, rebroadcast the view so sidebars
/// update. jj is shelled out to on a spawned task, so a slow repo never stalls
/// the caller (the tick or dispatch). Both readings come from one `jj::probe`,
/// so the stat and the stale tag stay in lockstep — and the probe's op-heads
/// mark lands in the VCS watch's book behind it, so the watch never re-triggers
/// on the snapshot the probe itself took. Marks only move forward: two probes
/// overlapping cannot roll one back.
///
/// The probe is also the checkpoint recorder: the turn edges the workspace's
/// panes noted are drained *before* it spawns, so its snapshot of `@` is
/// guaranteed to postdate them, and each is recorded against that commit.
fn refresh_changes(hub: &Arc<Hub>, workspace: WorkspaceId) {
    let (dir, turns) = {
        let mut session = hub.session();
        let Some(dir) = session.workspace_dir(workspace) else {
            return;
        };
        (dir, session.take_turns(workspace))
    };
    hub.vcs().entry(workspace).or_default().in_flight += 1;
    let hub = Arc::clone(hub);
    tokio::spawn(async move {
        let probe = jj::probe(&dir).await;
        let mut recorded = false;
        if let Some(head) = &probe.head {
            for (pane, noted) in turns {
                for (label, prompt) in noted {
                    recorded |=
                        record_checkpoint(&hub, &dir, pane, head, &label, prompt.as_deref())
                            .await
                            .is_some();
                }
            }
        }
        let mut session = hub.session();
        let stat_moved = session.set_changes(workspace, probe.changes, probe.files);
        let stale_moved = session.set_stale(workspace, probe.stale);
        let view = (stat_moved || stale_moved || recorded).then(|| session.view());
        drop(session);
        {
            let mut vcs = hub.vcs();
            let entry = vcs.entry(workspace).or_default();
            entry.in_flight = entry.in_flight.saturating_sub(1);
            entry.mark = entry.mark.max(probe.mark);
        }
        if let Some(view) = view {
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

/// Record `commit` — an `@` the caller just snapshotted in `dir` — as `pane`'s
/// next checkpoint (with the prompt that started the turn, when known), its
/// stat taken over the turn (`head..commit`) once, here. `None` when nothing
/// was recorded: the pane is gone, `@` has not moved since its head, the
/// turn's diff is empty (a `jj new` moves `@` without changing a file), or jj
/// failed. The session lock is never held across the jj call; `Turns::record`
/// re-checks the head it diffed from instead.
async fn record_checkpoint(
    hub: &Hub,
    dir: &Path,
    pane: PaneId,
    commit: &str,
    label: &str,
    prompt: Option<&str>,
) -> Option<u64> {
    let from = hub.session().turns(pane)?.head();
    if from.as_deref() == Some(commit) {
        return None;
    }
    let files = match &from {
        Some(from) => jj::files_between(dir, from, commit).await?,
        None => Vec::new(),
    };
    if from.is_some() && files.is_empty() {
        return None;
    }
    let time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    hub.session()
        .turns(pane)?
        .record(from, commit, label, prompt, time, &files)
}

/// One pass of the VCS watch: a workspace whose jj repo recorded an operation
/// since its last probe — a commit, bookmark move, or push made from any shell,
/// not just through tutti — gets its change stat refreshed, so the sidebar and
/// changes panel follow work done outside tutti rather than waiting for an
/// agent state transition that may never come. A probe's own snapshot never
/// counts: the watch stands off while one is in flight, and the probe re-marks
/// the repo behind itself. The first sighting of a workspace only records its
/// mark (attach and creation already seed the stat); vanished workspaces are
/// forgotten. The session guard is bound and released before the book is
/// opened, and both before `refresh_changes` re-locks them.
fn vcs_pass(hub: &Arc<Hub>) {
    let workspaces: Vec<(WorkspaceId, PathBuf)> = {
        let session = hub.session();
        session
            .workspace_ids()
            .into_iter()
            .filter_map(|id| session.workspace_dir(id).map(|dir| (id, dir)))
            .collect()
    };
    let moved: Vec<WorkspaceId> = {
        let mut vcs = hub.vcs();
        vcs.retain(|id, _| workspaces.iter().any(|(w, _)| w == id));
        workspaces
            .iter()
            .filter_map(|(id, dir)| {
                let modified = jj::op_heads_modified(dir)?;
                let entry = vcs.entry(*id).or_default();
                if entry.in_flight > 0 {
                    return None;
                }
                let previous = entry.mark.replace(modified);
                previous.is_some_and(|p| p != modified).then_some(*id)
            })
            .collect()
    };
    for id in moved {
        refresh_changes(hub, id);
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

fn broadcast_tick(hub: &Arc<Hub>) {
    // Exited panes too: what a child wrote just before its exit — its last
    // screen, its last notification, a crashed engine's word — may land after
    // the reaper marks it, and a corpse stays on screen. A quiescent corpse
    // costs a generation check.
    let panes = hub.session().panes_with_pty();

    // Drain each pane's bells/notifications every tick so the queues stay
    // bounded even with nobody attached; `broadcast_event` is a no-op then,
    // but the history still records them. Status reports ride the same
    // queue, so a program's word lands within a tick, in stream order with
    // the notifications around it. User vars do too, and whatever they moved
    // goes out as one fresh view per tick. A shell drawing its prompt unsets
    // the editor vars: an editor that crashed without unsetting them would
    // otherwise keep the direct keys away from the shell beneath it.
    let mut vars_moved = false;
    for (pane, pty) in &panes {
        for signal in pty.take_notifications() {
            match signal {
                Signal::Notify(note) => pane_notification(hub, *pane, note.title, note.body),
                Signal::Status(report) => program_status(hub, *pane, report),
                Signal::UserVar { name, value } => {
                    vars_moved |= hub.session().set_user_var(*pane, name, value);
                }
                Signal::Prompt => {
                    for name in ["TUTTI_KEYS", "TUTTI_EDITOR"] {
                        vars_moved |= hub
                            .session()
                            .set_user_var(*pane, name.into(), String::new());
                    }
                }
            }
        }
    }
    if vars_moved {
        let view = hub.session().view();
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }

    if hub.clients().is_empty() {
        return;
    }
    // Clients whose bounded queue filled this tick, disconnected once the loop
    // releases the clients lock.
    let mut doomed: HashSet<u64> = HashSet::new();
    for (pane, pty) in panes {
        let generation = *pty.output_receiver().borrow();
        let last = hub.last().get(&pane).copied();
        let unseen = hub
            .clients()
            .values()
            .any(|c| c.ready && !c.seen.contains(&pane));

        // A quiescent pane (output generation unchanged since we last looked)
        // whose screen every client already holds needs no look this tick.
        let moved = last.is_none_or(|last| last.generation != generation);
        if !moved && !unseen {
            continue;
        }
        // A frame its program is still drawing waits: the look comes on a
        // later tick, once the update ends, with all of it.
        if pty.holding() {
            continue;
        }

        // The tick is the one consumer of the screen's damage: one look
        // takes it, and fans the same delta out to every client.
        let look = pty.look(last.map(|last| (last.size, last.alt_screen)), unseen);
        let delta = last.filter(|_| moved).map(|_| look.delta);
        let seq = last.map_or(0, |last| last.seq + u32::from(delta.is_some()));
        hub.last().insert(
            pane,
            Last {
                size: look.size,
                alt_screen: look.alt_screen,
                seq,
                generation,
            },
        );
        let PaneSize { rows, cols } = look.size;

        let mut clients = hub.clients();
        for (cid, client) in clients.iter_mut() {
            if !client.ready || doomed.contains(cid) {
                continue;
            }
            let bytes = if !client.seen.contains(&pane) {
                // A client that came between the look and here waits a tick.
                let Some(snapshot) = &look.snapshot else {
                    continue;
                };
                client.seen.insert(pane);
                Frame::PaneSnapshot(PaneData {
                    pane,
                    rows,
                    cols,
                    seq,
                    bytes: snapshot.clone(),
                })
                .encode()
            } else if let Some(delta) = &delta {
                Frame::PaneDelta(PaneData {
                    pane,
                    rows,
                    cols,
                    seq,
                    bytes: delta.clone(),
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

/// Send each watcher the session's snapshot when it differs from the last one
/// that watcher got. Run on the render tick, so a burst of changes coalesces
/// into one snapshot, and on `Watch`, so the first goes out at once. "The"
/// attached client is the newest. A watcher whose queue is full is dropped,
/// as a wedged client is.
fn snapshot_watchers(hub: &Hub) {
    if hub.watchers().is_empty() {
        return;
    }
    let (attached_terminal, focused) = hub
        .clients()
        .iter()
        .max_by_key(|(cid, _)| **cid)
        .map_or((None, None), |(_, client)| {
            (client.terminal.clone(), client.focused)
        });
    let panes = hub.session().watch_panes(focused);
    let snapshot = encode_json(&Event::Snapshot {
        session: hub.name.clone(),
        wire_rev: tutti_core::WIRE_REV,
        attached_terminal,
        panes,
    });
    hub.watchers().retain(|_, watcher| {
        if watcher.last == snapshot {
            return true;
        }
        if watcher.tx.try_send(snapshot.clone()).is_err() {
            watcher.writer.abort();
            return false;
        }
        watcher.last.clone_from(&snapshot);
        true
    });
}

/// One agent-detection pass: snapshot the process tree, walk each live local
/// pane's child subtree, and record the matched agent kind. A change to any
/// pane's agent broadcasts the fresh view so clients relabel their badges, and
/// an agent appearing refreshes its workspace so where it started is
/// checkpointed.
async fn detect_pass(hub: Arc<Hub>) {
    let panes = hub.session().detectable_panes();
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
    let mut started: HashSet<WorkspaceId> = HashSet::new();
    for (pane, agent) in detected {
        let appeared = agent.is_some();
        if session.set_agent(pane, agent) {
            changed = true;
            if appeared {
                started.extend(session.workspace_of_pane(pane));
            }
        }
    }
    let view = changed.then(|| session.view());
    drop(session);
    if let Some(view) = view {
        broadcast_event(&hub, Event::LayoutChanged { workspaces: view });
    }
    for workspace in started {
        refresh_changes(&hub, workspace);
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

/// One state-classification pass over agent panes (and panes holding a
/// program/progress claim). PTY output since the last pass is an `Activity`
/// event; a classifier match on a detected agent's screen text is a
/// `Classified` event. `Session::observe` folds both onto the pane's state —
/// activity first so a screen match takes precedence, each only where the live
/// claim allows — and runs the stall timer; any net change is broadcast, plus
/// the view when one settles a self-reported pane out of the agent surfaces.
fn classify_pass(hub: &Arc<Hub>, last_gen: &mut HashMap<PaneId, u64>) {
    let panes = hub.session().agent_panes();
    if panes.is_empty() {
        return;
    }
    let events: Vec<(PaneId, bool, Option<Observation>)> = panes
        .iter()
        .map(|(pane, kind, pty)| {
            let generation = *pty.output_receiver().borrow();
            let advanced = last_gen
                .insert(*pane, generation)
                .map_or(generation != 0, |prev| prev != generation);
            let classified = kind
                .as_ref()
                .and_then(|kind| hub.registry.spec(kind))
                .and_then(|spec| spec.classify(&pty.contents()));
            (*pane, advanced, classified)
        })
        .collect();
    // Drop generations for panes that have gone away so the map cannot grow
    // without bound as panes come and go.
    last_gen.retain(|pane, _| panes.iter().any(|(p, _, _)| p == pane));

    let now = Instant::now();
    let mut session = hub.session();
    let mut changes = Vec::new();
    let mut relisted = false;
    for (pane, advanced, classified) in events {
        if let Some((from, to)) = session.observe(pane, advanced, classified, now) {
            changes.push((pane, from, to));
            relisted |= session.settle(pane);
        }
    }
    let view = relisted.then(|| session.view());
    drop(session);
    if let Some(view) = view {
        broadcast_event(hub, Event::LayoutChanged { workspaces: view });
    }
    // Dedupe the workspaces of the changed panes so a burst of transitions in
    // one workspace triggers a single jj refresh, not one per pane.
    let mut workspaces: HashSet<WorkspaceId> = HashSet::new();
    for (pane, from, to) in changes {
        publish_transition(hub, pane, from, to);
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
        let clients = hub.clients();
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
    for cid in cids {
        if let Some(client) = drop_client(hub, cid) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A control frame of exactly `"panic"` panics its handling — holding
    /// every hub lock, so each is poisoned after.
    pub(super) fn inject_panic(hub: &Hub, frame: &Frame) {
        if matches!(frame, Frame::Control(json) if json.as_slice() == b"\"panic\"") {
            let _held = (
                hub.session(),
                hub.clients(),
                hub.watchers(),
                hub.last(),
                hub.vcs(),
                hub.approvals(),
                hub.taps(),
            );
            panic!("injected: a frame panics holding every hub lock");
        }
    }

    /// A client of the daemon a test serves in-process.
    struct Peer {
        stream: tokio::net::UnixStream,
        buf: Vec<u8>,
    }

    impl Peer {
        async fn connect(path: &Path) -> Peer {
            let stream = tokio::net::UnixStream::connect(path).await.unwrap();
            Peer {
                stream,
                buf: Vec::new(),
            }
        }

        async fn send(&mut self, json: Vec<u8>) {
            let frame = Frame::Control(json).encode();
            self.stream.write_all(&frame).await.unwrap();
        }

        /// Send `request` and await its answer, skipping events and pane frames.
        async fn request(&mut self, request: &Request) -> Response {
            self.send(serde_json::to_vec(request).unwrap()).await;
            self.until(|frame| match frame {
                Frame::Control(json) => serde_json::from_slice(&json).ok(),
                _ => None,
            })
            .await
        }

        /// Read frames until `pick` takes one; a hangup first fails the test.
        async fn until<T>(&mut self, mut pick: impl FnMut(Frame) -> Option<T>) -> T {
            let read = async {
                loop {
                    if let Some((frame, used)) = Frame::decode(&self.buf).unwrap() {
                        self.buf.drain(..used);
                        if let Some(picked) = pick(frame) {
                            return picked;
                        }
                        continue;
                    }
                    let mut chunk = [0u8; 8192];
                    match self.stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => panic!("the daemon hung up"),
                        Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(5), read)
                .await
                .expect("the awaited frame never came")
        }

        async fn snapshot_at(&mut self, pane: PaneId, rows: u16, cols: u16) {
            self.until(|frame| match frame {
                Frame::PaneSnapshot(data) => {
                    (data.pane == pane && (data.rows, data.cols) == (rows, cols)).then_some(())
                }
                _ => None,
            })
            .await
        }
    }

    /// A panic handling one frame — injected while it holds every hub lock —
    /// ends that connection alone: its client is dropped, its vote in a shared
    /// pane's size with it, and the daemon serves on through the poisoned locks.
    #[tokio::test]
    async fn a_panicking_frame_drops_only_its_connection() {
        let dir = std::env::temp_dir().join(format!("tutti-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("panic.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            path.clone(),
            PaneSize::new(24, 80),
            None,
            async {
                let _ = stopped.await;
            },
        ));

        let mut viewer = Peer::connect(&path).await;
        let workspace = Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        };
        viewer.request(&workspace).await;
        let run = Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/cat".into()],
            ephemeral: false,
            sandbox: None,
        };
        let Response::PaneCreated { id: pane } = viewer.request(&run).await else {
            panic!("expected the cat pane");
        };
        let vote = |rows, cols| Request::ViewSizes {
            panes: vec![PaneDims { pane, rows, cols }],
        };
        let attach = Request::Attach {
            terminal: None,
            handoff: false,
        };
        viewer.request(&attach).await;
        assert_eq!(viewer.request(&vote(20, 60)).await, Response::Ok);
        let mut doomed = Peer::connect(&path).await;
        doomed.request(&attach).await;
        assert_eq!(doomed.request(&vote(10, 40)).await, Response::Ok);
        viewer.snapshot_at(pane, 10, 40).await;

        doomed.send(b"\"panic\"".to_vec()).await;
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), doomed.stream.read_to_end(&mut rest))
            .await
            .expect("the panicking connection is dropped")
            .unwrap();
        // Dropping it re-arbitrated the pane through the poisoned locks...
        viewer.snapshot_at(pane, 20, 60).await;
        // ...and the session and the VCS book serve on.
        assert!(matches!(
            viewer.request(&workspace).await,
            Response::WorkspaceCreated { .. }
        ));

        let _ = stop.send(());
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a child writes just before it exits is not lost to the reaper
    /// marking it first: its notification reaches the history, and a client
    /// attaching afterwards is drawn its last screen.
    #[tokio::test]
    async fn a_pane_s_last_words_outlive_its_exit() {
        let dir = std::env::temp_dir().join(format!("tutti-unit-last-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("last.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            path.clone(),
            PaneSize::new(24, 80),
            None,
            async {
                let _ = stopped.await;
            },
        ));

        let mut peer = Peer::connect(&path).await;
        let workspace = Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        };
        peer.request(&workspace).await;
        let script = "read go; printf 'last words\\033]9;bye\\007'";
        let run = Request::PaneRun {
            tab: None,
            cmd: vec!["/bin/sh".into(), "-c".into(), script.into()],
            ephemeral: false,
            sandbox: None,
        };
        let Response::PaneCreated { id: pane } = peer.request(&run).await else {
            panic!("expected a pane");
        };
        let go = Request::PaneSend {
            pane,
            text: Some("go\r".into()),
            keys: None,
            expect: None,
        };
        assert_eq!(peer.request(&go).await, Response::Ok);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let Response::Panes { panes } = peer.request(&Request::PaneList).await else {
                panic!("expected the pane list");
            };
            if panes.iter().any(|p| p.id == pane && p.exited.is_some()) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the pane never ended"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        loop {
            let Response::Notes { notes } =
                peer.request(&Request::NotesList { unread: false }).await
            else {
                panic!("expected the notes");
            };
            if notes.iter().any(|note| note.body.as_deref() == Some("bye")) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the last notification was lost"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        peer.request(&Request::Attach {
            terminal: None,
            handoff: false,
        })
        .await;
        peer.until(|frame| match frame {
            Frame::PaneSnapshot(data) => (data.pane == pane
                && data.bytes.windows(10).any(|bytes| bytes == b"last words"))
            .then_some(()),
            _ => None,
        })
        .await;

        let _ = stop.send(());
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pane is the unit of failure. One pane's engine panicking on a read,
    /// and another's drawing its screen for the render tick (the test hooks
    /// in pty.rs), ends each alone: it exits, its screen tells of the crash,
    /// and a note says so. The daemon serves on, and the pane beside them
    /// still echoes and still reaches the attached client.
    #[tokio::test]
    async fn a_pane_whose_engine_panics_ends_alone() {
        let dir = std::env::temp_dir().join(format!("tutti-unit-crash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crash.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            path.clone(),
            PaneSize::new(24, 80),
            None,
            async {
                let _ = stopped.await;
            },
        ));

        let mut viewer = Peer::connect(&path).await;
        let workspace = Request::WorkspaceNew {
            dir: std::env::temp_dir(),
        };
        viewer.request(&workspace).await;
        let mut panes = Vec::new();
        // `tutti\001panic` and the `tutti-panic-look` title are the hooks'
        // triggers (`PANIC_READ`, `PANIC_LOOK`).
        for script in [
            "read go; printf 'tutti\\001panic'; sleep 30",
            "read go; printf '\\033]2;tutti-panic-look\\007drawn'; sleep 30",
            "exec cat",
        ] {
            let run = Request::PaneRun {
                tab: None,
                cmd: vec!["/bin/sh".into(), "-c".into(), script.into()],
                ephemeral: false,
                sandbox: None,
            };
            let Response::PaneCreated { id } = viewer.request(&run).await else {
                panic!("expected a pane");
            };
            panes.push(id);
        }
        let [reading, drawing, beside] = panes[..] else {
            unreachable!()
        };
        viewer
            .request(&Request::Attach {
                terminal: None,
                handoff: false,
            })
            .await;
        let send = |pane, text: &str| Request::PaneSend {
            pane,
            text: Some(text.into()),
            keys: None,
            expect: None,
        };
        let read = |pane| Request::PaneRead {
            pane,
            lines: None,
            unwrapped: false,
            since: None,
            last_output: false,
        };

        for pane in [reading, drawing] {
            assert_eq!(viewer.request(&send(pane, "go\r")).await, Response::Ok);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let Response::Panes { panes } = viewer.request(&Request::PaneList).await else {
                    panic!("expected the pane list");
                };
                if panes.iter().any(|p| p.id == pane && p.exited.is_some()) {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "pane {pane} never ended"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let Response::Content { lines } = viewer.request(&read(pane)).await else {
                panic!("expected pane {pane}'s screen");
            };
            assert!(
                lines.join("\n").contains("terminal crashed: injected"),
                "{lines:?}"
            );
        }

        assert_eq!(
            viewer.request(&send(beside, "after-both\r")).await,
            Response::Ok
        );
        viewer
            .until(|frame| match frame {
                Frame::PaneDelta(data) | Frame::PaneSnapshot(data) => (data.pane == beside
                    && data.bytes.windows(10).any(|bytes| bytes == b"after-both"))
                .then_some(()),
                _ => None,
            })
            .await;
        let Response::Notes { notes } = viewer.request(&Request::NotesList { unread: false }).await
        else {
            panic!("expected the notes");
        };
        let crashes: Vec<PaneId> = notes
            .iter()
            .filter(|note| note.title.as_deref() == Some("pane crashed"))
            .map(|note| note.pane)
            .collect();
        assert_eq!(crashes, [reading, drawing]);

        let _ = stop.send(());
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A background pass that panics is caught, and its loop runs the next.
    #[test]
    fn a_guarded_pass_survives_its_panic() {
        let mut passes = 0;
        for _ in 0..3 {
            guarded("test", || {
                passes += 1;
                panic!("injected pass panic");
            });
        }
        assert_eq!(passes, 3);
    }

    /// A peer hanging up closes the reply queue once what was queued drains,
    /// so a task still holding a sender — a pane wait — sees `closed()`
    /// rather than keeping the writer alive.
    #[tokio::test]
    async fn a_hangup_drains_the_reply_queue_then_closes_it() {
        let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let (_read, write) = server.into_split();
        let (tx, rx) = mpsc::channel(4);
        let (hangup, hung_up) = oneshot::channel();
        let writer = tokio::spawn(writer_loop(write, rx, hung_up));
        let waiter = tx.clone();
        tx.send(b"queued".to_vec()).await.unwrap();
        drop((tx, hangup));
        let deadline = Duration::from_secs(5);
        tokio::time::timeout(deadline, waiter.closed())
            .await
            .expect("the hangup closes the queue under a live sender");
        tokio::time::timeout(deadline, writer)
            .await
            .expect("the writer ends")
            .unwrap();
        let mut written = Vec::new();
        peer.read_to_end(&mut written).await.unwrap();
        assert_eq!(written, b"queued", "what was queued still drained");
    }
}
