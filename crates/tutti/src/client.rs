use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tutti_core::{Frame, Request, Response, socket_dir, socket_path};

/// A one-shot control-plane connection to `tutti-server`. Sends a single
/// `Request` as a Control frame and reads Control frames until the matching
/// `Response` arrives, skipping any asynchronous `Event` frames in between.
pub struct Client {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Client {
    pub fn connect(session: &str) -> std::io::Result<Client> {
        let stream = UnixStream::connect(socket_path(session))?;
        Ok(Client {
            stream,
            buf: Vec::new(),
        })
    }

    /// Connect, auto-starting the daemon on a missing or refused socket.
    pub fn connect_or_start(session: &str) -> Result<Client> {
        Ok(Client {
            stream: open(session)?,
            buf: Vec::new(),
        })
    }

    /// Bound every read and write on this connection by `timeout`, so a
    /// request whose reply has not come by then fails instead of blocking.
    pub fn set_timeout(&self, timeout: Duration) -> std::io::Result<()> {
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))
    }

    /// Send `request` and read its answer, giving up past the request's
    /// `deadline`; a wait or an approval hold keeps the connection's own
    /// timeout (`set_timeout`), none by default.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        if let Some(deadline) = deadline(request) {
            self.set_timeout(deadline)?;
        }
        let json = serde_json::to_vec(request)?;
        self.stream.write_all(&Frame::Control(json).encode())?;
        self.stream.flush()?;
        loop {
            // A Control frame that is not a Response is an Event notification
            // (and non-Control frames carry pane data); skip both and keep
            // waiting for the reply.
            if let Frame::Control(bytes) = self.read_frame()?
                && let Ok(response) = serde_json::from_slice::<Response>(&bytes)
            {
                return Ok(response);
            }
        }
    }

    /// Subscribe to the session's snapshots (`Request::Watch`), handing each
    /// Control frame's JSON to `each` as it lands, until the daemon closes the
    /// connection.
    pub fn watch(&mut self, mut each: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let json = serde_json::to_vec(&Request::Watch)?;
        self.stream.write_all(&Frame::Control(json).encode())?;
        loop {
            while let Some((frame, consumed)) = Frame::decode(&self.buf)? {
                self.buf.drain(..consumed);
                if let Frame::Control(json) = frame {
                    each(&json)?;
                }
            }
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Ok(());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn read_frame(&mut self) -> Result<Frame> {
        loop {
            if let Some((frame, consumed)) = Frame::decode(&self.buf)? {
                self.buf.drain(..consumed);
                return Ok(frame);
            }
            let mut chunk = [0u8; 8192];
            let n = match self.stream.read(&mut chunk) {
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    let waited = self.stream.read_timeout()?.unwrap_or_default();
                    bail!("tutti-server did not answer within {}s", waited.as_secs());
                }
                read => read?,
            };
            if n == 0 {
                bail!("tutti-server closed the connection before replying");
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// How long the CLI waits on the daemon's answer to `request` before giving
/// up on it: `None` for one that answers when something happens (a wait, an
/// approval hold). The daemon answers anything else from memory at once, but
/// what it runs jj for only within its jj runs' own caps (a merge's eight
/// steps, 15 s apiece, the longest).
fn deadline(request: &Request) -> Option<Duration> {
    match request {
        Request::PaneWait { .. } | Request::ApprovalHold { .. } => None,
        Request::WorkspaceFork { .. }
        | Request::SandboxPull { .. }
        | Request::WorkspaceMerge { .. }
        | Request::WorkspaceKill { discard: true, .. }
        | Request::WorkspaceUpdate { .. }
        | Request::WorkspaceDiff { .. }
        | Request::PaneCheckpointDiff { .. }
        | Request::PaneRestore { .. } => Some(Duration::from_secs(180)),
        _ => Some(Duration::from_secs(10)),
    }
}

/// Open a raw socket to the session's daemon, auto-starting it on a missing or
/// refused socket. Shared by the control-plane `Client` and the attach TUI.
pub fn open(session: &str) -> Result<UnixStream> {
    match UnixStream::connect(socket_path(session)) {
        Ok(stream) => Ok(stream),
        Err(e) if not_running(&e) => {
            spawn_server(session)?;
            await_server(session).map(|client| client.stream)
        }
        Err(e) => Err(e).context("connecting to tutti-server"),
    }
}

pub fn not_running(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Replace the current process with `tutti-server` in the foreground.
/// Only returns on failure to exec.
pub fn exec_foreground(session: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let error = Command::new(server_exe()?)
        .arg("--session")
        .arg(session)
        .arg("--foreground")
        .exec();
    Err(error).context("exec tutti-server --foreground")
}

/// Terminate a running daemon via the pid file it writes next to its socket.
pub fn stop(session: &str) -> Result<StopOutcome> {
    let pidfile = socket_dir().join(format!("{session}.pid"));
    let text = match std::fs::read_to_string(&pidfile) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StopOutcome::NotRunning),
        Err(e) => return Err(e).with_context(|| format!("reading pid file {}", pidfile.display())),
    };
    let pid: i32 = text
        .trim()
        .parse()
        .with_context(|| format!("parsing pid from {}", pidfile.display()))?;
    if unsafe { kill(pid, SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("signalling pid {pid}"));
    }
    Ok(StopOutcome::Signalled(pid))
}

pub enum StopOutcome {
    Signalled(i32),
    NotRunning,
}

/// The pid of `session`'s daemon, from the pid file it writes next to its
/// socket; `None` when none is running.
pub fn daemon_pid(session: &str) -> Option<u32> {
    let pidfile = socket_dir().join(format!("{session}.pid"));
    std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()
}

/// Launch the daemon. The launched process only re-execs a detached copy of
/// itself, whose stderr it sends to the session's log (`log_path`); its own
/// stderr stays ours, so a launch that fails says why.
fn spawn_server(session: &str) -> Result<()> {
    Command::new(server_exe()?)
        .arg("--session")
        .arg(session)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .context("spawning tutti-server")?;
    Ok(())
}

fn await_server(session: &str) -> Result<Client> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        std::thread::sleep(Duration::from_millis(50));
        match Client::connect(session) {
            Ok(client) => return Ok(client),
            Err(e) if not_running(&e) && Instant::now() < deadline => continue,
            Err(e) => return Err(e).context("connecting to tutti-server after auto-start"),
        }
    }
}

/// Locate the daemon binary: a sibling of the current executable, else `PATH`.
fn server_exe() -> Result<PathBuf> {
    let current = std::env::current_exe().context("locating current executable")?;
    let sibling = current.with_file_name("tutti-server");
    if sibling.is_file() {
        Ok(sibling)
    } else {
        Ok(PathBuf::from("tutti-server"))
    }
}

const SIGTERM: i32 = 15;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{PaneId, WaitFor, WorkspaceId};

    /// A wait and a held approval answer when something happens, so only
    /// their own timeouts bound them; the CLI gives up on any other request
    /// the daemon leaves unanswered, later on one it runs jj for.
    #[test]
    fn only_a_wait_or_a_hold_waits_on_the_daemon_unbounded() {
        let wait = Request::PaneWait {
            pane: PaneId(1),
            until: vec![WaitFor::Done],
            next: true,
            timeout_ms: None,
            reply_after: None,
        };
        let hold = Request::ApprovalHold {
            pane: PaneId(1),
            conversation: None,
            tool: "Bash".into(),
            summary: "ls".into(),
        };
        assert_eq!((deadline(&wait), deadline(&hold)), (None, None));
        let merge = Request::WorkspaceMerge {
            id: WorkspaceId(1),
            push: true,
        };
        assert!(deadline(&Request::PaneList).is_some_and(|d| d < deadline(&merge).unwrap()));
    }
}
