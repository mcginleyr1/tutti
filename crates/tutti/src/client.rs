use std::io::{Read, Write};
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
        match Client::connect(session) {
            Ok(client) => Ok(client),
            Err(e) if not_running(&e) => {
                spawn_server(session)?;
                await_server(session)
            }
            Err(e) => Err(e).context("connecting to tutti-server"),
        }
    }

    pub fn request(&mut self, request: &Request) -> Result<Response> {
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

    fn read_frame(&mut self) -> Result<Frame> {
        loop {
            if let Some((frame, consumed)) = Frame::decode(&self.buf)? {
                self.buf.drain(..consumed);
                return Ok(frame);
            }
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                bail!("tutti-server closed the connection before replying");
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
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

fn spawn_server(session: &str) -> Result<()> {
    Command::new(server_exe()?)
        .arg("--session")
        .arg(session)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
