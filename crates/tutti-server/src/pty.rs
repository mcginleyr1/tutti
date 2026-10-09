//! Persistent PTY management: one native pty per pane, driven by a background
//! reader thread that feeds bytes into a shared `vt100` grid. The child keeps
//! running whether or not anything is reading the UI — this is the daemon's core.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::watch;
use tutti_core::{AgentState, StateSource};

const SCROLLBACK_LINES: usize = 10_000;

/// What to run in a pane.
#[derive(Debug, Clone)]
pub struct PtySpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Additional environment variables layered onto the inherited environment.
    pub env: Vec<(String, String)>,
}

impl PtySpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }
}

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSize {
    pub rows: u16,
    pub cols: u16,
}

impl PaneSize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

/// How a child terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneExit {
    pub success: bool,
    pub code: u32,
}

/// A point-in-time view of a pane's screen, suitable for serving `pane read`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// One entry per visible row, top to bottom.
    pub lines: Vec<String>,
    /// The output generation this grid reflects — exactly: both are read under
    /// the parser lock the reader thread bumps it under.
    pub generation: u64,
}

impl Snapshot {
    /// The screen flattened to a single newline-joined string.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// A persistent pty running a single child process.
pub struct PtyPane {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The child's OS process id, the root of the process-tree walk that agent
    /// detection matches against. `None` if the platform did not report one.
    pid: Option<u32>,
    size: Mutex<PaneSize>,
    exit_rx: watch::Receiver<Option<PaneExit>>,
    output_rx: watch::Receiver<u64>,
    /// Bells and OSC 9 / 777 desktop notifications pulled from the raw output
    /// stream, drained by the broadcast tick — interleaved, in stream order,
    /// with the program's own status reports (OSC 9;4 / 7501), the one signal
    /// here that does feed pane state (under the source ranking).
    notifications: Arc<Mutex<Vec<Signal>>>,
    reader_thread: Option<JoinHandle<()>>,
    wait_thread: Option<JoinHandle<()>>,
}

impl PtyPane {
    /// Spawn `spec` on a fresh native pty of the given size. The child runs
    /// detached from any UI; output is accumulated into the vt100 grid by a
    /// background thread from the moment this returns.
    pub fn spawn(spec: PtySpec, size: PaneSize) -> Result<Self> {
        // portable-pty silently substitutes $HOME when the requested cwd is not
        // a directory, which would launch an agent against the wrong tree.
        if let Some(cwd) = &spec.cwd
            && !cwd.is_dir()
        {
            bail!("working directory does not exist: {}", cwd.display());
        }
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn {}", spec.program))?;
        // Drop our copy of the slave so the master reader sees EOF once the
        // child's own descriptors close.
        drop(pair.slave);

        let killer = child.clone_killer();
        let pid = child.process_id();
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take pty writer")?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            size.rows,
            size.cols,
            SCROLLBACK_LINES,
        )));

        let (output_tx, output_rx) = watch::channel(0u64);
        let (exit_tx, exit_rx) = watch::channel(None);
        let notifications = Arc::new(Mutex::new(Vec::new()));

        let reader_parser = Arc::clone(&parser);
        let reader_notifications = Arc::clone(&notifications);
        let reader_thread = std::thread::Builder::new()
            .name("tutti-pty-reader".into())
            .spawn(move || read_loop(reader, reader_parser, output_tx, reader_notifications))
            .context("failed to spawn pty reader thread")?;

        let wait_thread = std::thread::Builder::new()
            .name("tutti-pty-wait".into())
            .spawn(move || {
                let mut child = child;
                let exit = match child.wait() {
                    Ok(status) => PaneExit {
                        success: status.success(),
                        code: status.exit_code(),
                    },
                    Err(_) => PaneExit {
                        success: false,
                        code: u32::MAX,
                    },
                };
                let _ = exit_tx.send(Some(exit));
            })
            .context("failed to spawn pty wait thread")?;

        Ok(Self {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            parser,
            killer: Mutex::new(killer),
            pid,
            size: Mutex::new(size),
            exit_rx,
            output_rx,
            notifications,
            reader_thread: Some(reader_thread),
            wait_thread: Some(wait_thread),
        })
    }

    /// Forward raw keystrokes to the child.
    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().expect("pty writer poisoned");
        writer.write_all(bytes).context("write to pty failed")?;
        writer.flush().context("flush pty failed")?;
        Ok(())
    }

    /// Resize both the kernel pty and the vt100 grid. A zero dimension is
    /// refused: `read` tiles scrollback in screen-height windows and would
    /// never advance.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        if rows == 0 || cols == 0 {
            bail!("pane size must be at least 1x1, got {rows}x{cols}");
        }
        self.master
            .lock()
            .expect("pty master poisoned")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty failed")?;
        self.parser
            .lock()
            .expect("pty parser poisoned")
            .screen_mut()
            .set_size(rows, cols);
        *self.size.lock().expect("pty size poisoned") = PaneSize { rows, cols };
        Ok(())
    }

    /// Current screen contents, cursor position, and size.
    pub fn snapshot(&self) -> Snapshot {
        let parser = self.parser.lock().expect("pty parser poisoned");
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let lines = screen.rows(0, cols).collect();
        Snapshot {
            rows,
            cols,
            cursor_row,
            cursor_col,
            lines,
            generation: *self.output_rx.borrow(),
        }
    }

    /// An owned clone of the live screen, for computing `contents_formatted`
    /// snapshots and `contents_diff` deltas on the broadcast tick.
    pub fn screen(&self) -> vt100::Screen {
        self.parser
            .lock()
            .expect("pty parser poisoned")
            .screen()
            .clone()
    }

    /// A clone of the screen scrolled `offset` rows back into the scrollback
    /// ring, for serving a scrollback view to an attached client.
    pub fn screen_scrolled(&self, offset: usize) -> vt100::Screen {
        let mut screen = self.screen();
        screen.set_scrollback(offset);
        screen
    }

    /// The pane's text content, oldest line first, including scrollback.
    ///
    /// `unwrapped` joins soft-wrapped rows back into logical lines; `lines`
    /// caps the result to that many most-recent lines. Reads a throwaway clone
    /// so scrolling the view does not disturb attached clients.
    pub fn read(&self, lines: Option<usize>, unwrapped: bool) -> Vec<String> {
        let mut screen = self.screen();
        let (rows, cols) = screen.size();
        let rows = rows as usize;

        screen.set_scrollback(usize::MAX);
        let scrollback = screen.scrollback();
        let total = scrollback + rows;

        // Tile the whole buffer with screen-height windows. `visible_rows`
        // exposes only `rows` lines at a time, so we step the scrollback
        // offset and skip the rows an earlier window already yielded.
        let mut visual: Vec<String> = Vec::with_capacity(total);
        let mut top = 0usize;
        while top < total {
            let offset = scrollback.saturating_sub(top);
            screen.set_scrollback(offset);
            let window_top = scrollback - offset;
            let skip = top - window_top;
            visual.extend(screen.rows(0, cols).skip(skip));
            top = window_top + rows;
        }

        let mut out = if unwrapped {
            fold_wrapped(visual, cols as usize)
        } else {
            visual
        };
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        if let Some(n) = lines
            && out.len() > n
        {
            out.drain(..out.len() - n);
        }
        out
    }

    /// The child's OS process id, if the platform reported one at spawn.
    pub fn child_pid(&self) -> Option<u32> {
        self.pid
    }

    /// The child's exit status, or `None` if it is still running.
    pub fn exit_status(&self) -> Option<PaneExit> {
        *self.exit_rx.borrow()
    }

    /// Await child termination, resolving immediately if it has already exited.
    pub async fn wait(&self) -> PaneExit {
        let mut rx = self.exit_rx.clone();
        loop {
            let current = *rx.borrow();
            if let Some(exit) = current {
                return exit;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().unwrap_or(PaneExit {
                    success: false,
                    code: u32::MAX,
                });
            }
        }
    }

    /// A receiver that fires whenever new output is folded into the grid.
    pub fn output_receiver(&self) -> watch::Receiver<u64> {
        self.output_rx.clone()
    }

    /// Drain the bells, desktop notifications and status reports seen since
    /// the last call, in stream order.
    pub fn take_notifications(&self) -> Vec<Signal> {
        std::mem::take(
            &mut *self
                .notifications
                .lock()
                .expect("pty notifications poisoned"),
        )
    }

    /// A receiver that resolves to `Some(exit)` when the child terminates.
    pub fn exit_receiver(&self) -> watch::Receiver<Option<PaneExit>> {
        self.exit_rx.clone()
    }

    /// Terminate the child and every descendant sharing its pty.
    ///
    /// portable-pty makes the child a session/process-group leader (`setsid`
    /// before exec), so its pid doubles as its process-group id. We SIGKILL the
    /// whole group: a descendant that inherited the slave pty (a backgrounded
    /// grandchild, say) would otherwise outlive the direct child — leaking, and
    /// on Linux holding the master reader open past EOF so the reader-thread
    /// join in `Drop` hangs. portable-pty's own killer only SIGHUPs the direct
    /// child, which a descendant can ignore or a grandchild never receives.
    pub fn kill(&self) -> Result<()> {
        if let Some(pid) = self.pid {
            signal_group(pid, SIGKILL);
            return Ok(());
        }
        // No pid was reported (not expected on Unix): fall back to the direct
        // child so we at least terminate it.
        self.killer
            .lock()
            .expect("pty killer poisoned")
            .kill()
            .context("kill child failed")
    }
}

impl Drop for PtyPane {
    fn drop(&mut self) {
        // Kill the whole process group (see `kill`) so a lingering descendant
        // cannot keep the reader blocked and hang the joins below.
        let _ = self.kill();
        if let Some(handle) = self.reader_thread.take() {
            join_or_detach(handle);
        }
        if let Some(handle) = self.wait_thread.take() {
            join_or_detach(handle);
        }
    }
}

/// SIGKILL: the child called `setsid`, so its pid is its process-group id and a
/// negative pid signals the whole group. We bind `kill(2)` directly rather than
/// via the `libc` crate, which is not a declared dependency — mirroring how
/// `tutti-core` binds `getuid`.
const SIGKILL: i32 = 9;

/// Upper bound on how long `Drop` waits for a pty thread before detaching it, so
/// a pathological descendant that survives SIGKILL while holding the slave open
/// cannot hang shutdown indefinitely.
const JOIN_BACKSTOP: Duration = Duration::from_secs(2);

/// SIGKILL the process group led by `pid` (a negative pid targets the group).
fn signal_group(pid: u32, sig: i32) {
    let _ = libc_kill(-(pid as i32), sig);
}

/// Join `handle`, but detach rather than block indefinitely if it does not
/// finish within `JOIN_BACKSTOP`. After a process-group kill the reader sees EOF
/// and returns at once; this only guards a descendant that survives the kill
/// while holding the slave open.
fn join_or_detach(handle: JoinHandle<()>) {
    let deadline = std::time::Instant::now() + JOIN_BACKSTOP;
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = handle.join();
}

/// Join rows that filled the full width onto their predecessor: `vt100` marks
/// such rows as soft-wrapped, and a full row is the only signal reachable
/// through the public screen API. A genuine full-width line is merged too — an
/// accepted approximation, since the wire snapshot keeps exact wrapping.
fn fold_wrapped(rows: Vec<String>, cols: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(rows.len());
    for row in rows {
        if out.last().is_some_and(|prev| prev.chars().count() == cols) {
            out.last_mut().expect("checked non-empty").push_str(&row);
        } else {
            out.push(row);
        }
    }
    out
}

fn read_loop(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    output_tx: watch::Sender<u64>,
    notifications: Arc<Mutex<Vec<Signal>>>,
) {
    let mut buf = [0u8; 8192];
    let mut scanner = NotifyScanner::default();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Scan the raw bytes for bells/notifications before the vt100
                // parser swallows them, keeping scanner state across chunks.
                let mut found = Vec::new();
                scanner.feed(&buf[..n], &mut found);
                if !found.is_empty() {
                    let mut queue = notifications.lock().expect("pty notifications poisoned");
                    queue.extend(found);
                    let overflow = queue.len().saturating_sub(NOTIFY_QUEUE_CAP);
                    if overflow > 0 {
                        queue.drain(..overflow);
                    }
                }
                // Bumped under the parser lock, so a `snapshot` sees a grid and
                // a generation that agree.
                let mut grid = parser.lock().expect("pty parser poisoned");
                grid.process(&buf[..n]);
                output_tx.send_modify(|generation| *generation = generation.wrapping_add(1));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// A desktop notification or bell surfaced from a pane's raw output. A bare
/// bell carries no text (both fields `None`); OSC 9 fills `body`; OSC 777 fills
/// `title` and/or `body`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: Option<String>,
    pub body: Option<String>,
}

/// A program's own word on its state, from its output: an OSC 9;4 progress
/// report (`source` Progress) or an OSC 7501 program status (`source`
/// Program). `state: None` is a clear — it ends that source's claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub source: StateSource,
    pub state: Option<AgentState>,
    /// The OSC 7501 `app`: the label the pane shows under where agent state
    /// does, when no agent was detected there.
    pub app: Option<String>,
    /// The decoded OSC 7501 `msg`: the notification body on a move to
    /// done/blocked.
    pub msg: Option<String>,
}

/// What the scanner pulls from a raw pty stream: an attention signal, or the
/// program's word on its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Notify(Notification),
    Status(StatusReport),
}

/// Largest OSC payload buffered; a longer sequence is consumed but discarded so
/// a runaway sequence cannot grow memory without bound. Doubles as OSC 7501's
/// whole-report cap.
const OSC_CAP: usize = 4096;
/// Cap on pending notifications per pane, bounding the queue when a burst
/// arrives with no client draining it.
const NOTIFY_QUEUE_CAP: usize = 256;
/// OSC 7501 limits from the program-status spec: the longest key and `app`,
/// and the base64 `msg`/`title` as (encoded, decoded) bytes.
const KEY_MAX: usize = 16;
const APP_MAX: usize = 32;
const MSG_MAX: (usize, usize) = (2732, 2048);
const TITLE_MAX: (usize, usize) = (256, 192);
/// Standard-alphabet base64 that takes padding or leaves it off, as OSC 7501
/// allows.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Default)]
enum ScanState {
    #[default]
    Ground,
    /// Saw `ESC`, awaiting the `]` OSC introducer.
    Esc,
    /// Inside an OSC string, accumulating the payload.
    Osc,
    /// Saw `ESC` inside an OSC string, awaiting the `\` of a String Terminator.
    OscEsc,
}

/// Incremental scanner pulling bells, OSC 9 / OSC 777 desktop-notification
/// sequences, and OSC 9;4 / OSC 7501 status reports from a raw pty byte
/// stream. State persists across `feed` calls, so a sequence split over
/// read-chunk boundaries is still recognised.
#[derive(Default)]
struct NotifyScanner {
    state: ScanState,
    buf: Vec<u8>,
    overflow: bool,
}

impl NotifyScanner {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<Signal>) {
        for &b in bytes {
            self.step(b, out);
        }
    }

    fn step(&mut self, b: u8, out: &mut Vec<Signal>) {
        match self.state {
            ScanState::Ground => match b {
                0x07 => out.push(Signal::Notify(Notification {
                    title: None,
                    body: None,
                })),
                0x1b => self.state = ScanState::Esc,
                _ => {}
            },
            ScanState::Esc => match b {
                b']' => {
                    self.state = ScanState::Osc;
                    self.buf.clear();
                    self.overflow = false;
                }
                0x1b => {} // a run of ESCs stays poised for the introducer
                _ => {
                    // Not the OSC introducer: abandon the escape and reinterpret
                    // this byte from ground, so an `ESC BEL` still rings (matches
                    // the re-feed the `OscEsc` state already does).
                    self.state = ScanState::Ground;
                    self.step(b, out);
                }
            },
            ScanState::Osc => match b {
                // A bare BEL here is the OSC terminator, not a bell.
                0x07 => {
                    self.finish_osc(out);
                    self.state = ScanState::Ground;
                }
                0x1b => self.state = ScanState::OscEsc,
                _ => {
                    if self.buf.len() < OSC_CAP {
                        self.buf.push(b);
                    } else {
                        self.overflow = true;
                    }
                }
            },
            ScanState::OscEsc => match b {
                b'\\' => {
                    self.finish_osc(out);
                    self.state = ScanState::Ground;
                }
                other => {
                    // ESC not completing an ST cancels the OSC string; drop it
                    // and reinterpret this byte from ground.
                    self.buf.clear();
                    self.overflow = false;
                    self.state = ScanState::Ground;
                    self.step(other, out);
                }
            },
        }
    }

    fn finish_osc(&mut self, out: &mut Vec<Signal>) {
        let overflow = std::mem::take(&mut self.overflow);
        let buf = std::mem::take(&mut self.buf);
        if !overflow && let Some(signal) = parse_osc(&buf) {
            out.push(signal);
        }
    }
}

/// Parse an OSC payload (the bytes between `ESC ]` and the terminator) for the
/// commands tutti surfaces: notifications `9;<body>` and
/// `777;notify;<title>;<body>`, and status reports `9;4;…` and `7501;…`.
/// Anything else yields `None`.
fn parse_osc(buf: &[u8]) -> Option<Signal> {
    let text = std::str::from_utf8(buf).ok()?;
    let (cmd, rest) = text.split_once(';')?;
    match cmd {
        // ConEmu's progress subcommand, never a notification — even malformed.
        "9" if rest == "4" || rest.starts_with("4;") => {
            parse_progress(rest.get(2..).unwrap_or_default()).map(Signal::Status)
        }
        "9" => non_empty(rest).map(|body| {
            Signal::Notify(Notification {
                title: None,
                body: Some(body),
            })
        }),
        "777" => {
            let mut fields = rest.splitn(3, ';');
            if fields.next()? != "notify" {
                return None;
            }
            let title = fields.next().and_then(non_empty);
            let body = fields.next().and_then(non_empty);
            (title.is_some() || body.is_some())
                .then_some(Signal::Notify(Notification { title, body }))
        }
        "7501" => parse_program_status(rest).map(Signal::Status),
        _ => None,
    }
}

/// Parse an OSC 9;4 progress body (after `9;4;`): `<state>[;<percent>]`.
/// States 1 (normal), 3 (indeterminate) and 4 (paused/warning) are Working; 2
/// (error) is Done — tutti has no errored state, and Done queues it for
/// attention; 0 clears. The percentage is not shown, so it is not read.
fn parse_progress(body: &str) -> Option<StatusReport> {
    let state = match body.split(';').next()? {
        "0" => None,
        "1" | "3" | "4" => Some(AgentState::Working),
        "2" => Some(AgentState::Done),
        _ => return None,
    };
    Some(StatusReport {
        source: StateSource::Progress,
        state,
        app: None,
        msg: None,
    })
}

/// Parse an OSC 7501 program-status body: `key=value` pairs joined by `:`
/// (tuios.dev/docs/program-status.md). Only the root record is tracked, so a
/// report naming an `id` is ignored, as is the `?` support probe (answering
/// means writing into the pty). Per the spec, a report with a missing/unknown
/// `state`, a `kind` on a non-blocked state, or a `progress` outside 0–100 is
/// ignored; one breaking a length limit, or whose `msg`/`title` is not base64
/// of control-free UTF-8, is discarded whole. A malformed pair is skipped and
/// the rest still read; a repeated key's last value wins.
fn parse_program_status(body: &str) -> Option<StatusReport> {
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let well_formed = !key.is_empty()
            && key.bytes().all(|b| b.is_ascii_lowercase())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b));
        if !well_formed {
            continue;
        }
        if key.len() > KEY_MAX {
            return None;
        }
        pairs.push((key, value));
    }
    let get = |key: &str| pairs.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let state = match get("state")? {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        "done" | "error" => Some(AgentState::Done),
        "clear" => None,
        _ => return None,
    };
    let percent =
        |p: &str| p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u8>().is_ok_and(|p| p <= 100);
    if get("id").is_some()
        || (get("kind").is_some() && state != Some(AgentState::Blocked))
        || get("progress").is_some_and(|p| !percent(p))
    {
        return None;
    }
    let app = get("app");
    if app.is_some_and(|app| app.len() > APP_MAX) {
        return None;
    }
    let msg = match get("msg") {
        Some(msg) => Some(decode_text(msg, MSG_MAX)?),
        None => None,
    };
    if let Some(title) = get("title") {
        decode_text(title, TITLE_MAX)?;
    }
    Some(StatusReport {
        source: StateSource::Program,
        state,
        app: app.map(str::to_string),
        msg,
    })
}

/// Decode a base64 `msg`/`title` within its (encoded, decoded) byte caps into
/// control-free UTF-8; `None` discards the report.
fn decode_text(encoded: &str, (encoded_max, decoded_max): (usize, usize)) -> Option<String> {
    if encoded.len() > encoded_max {
        return None;
    }
    let text = String::from_utf8(BASE64.decode(encoded).ok()?).ok()?;
    (text.len() <= decoded_max && !text.chars().any(char::is_control)).then_some(text)
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

unsafe extern "C" {
    #[link_name = "kill"]
    safe fn libc_kill(pid: i32, sig: i32) -> i32;
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PtyPane>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(input: &[u8]) -> Vec<Signal> {
        let mut scanner = NotifyScanner::default();
        let mut out = Vec::new();
        scanner.feed(input, &mut out);
        out
    }

    fn notes(signals: Vec<Signal>) -> Vec<Notification> {
        signals
            .into_iter()
            .filter_map(|s| match s {
                Signal::Notify(note) => Some(note),
                Signal::Status(_) => None,
            })
            .collect()
    }

    fn scan(input: &[u8]) -> Vec<Notification> {
        notes(signals(input))
    }

    /// The status reports in `input`, one entry per report.
    fn scan_status(input: &[u8]) -> Vec<StatusReport> {
        signals(input)
            .into_iter()
            .filter_map(|s| match s {
                Signal::Status(report) => Some(report),
                Signal::Notify(_) => None,
            })
            .collect()
    }

    /// Feed `input` in two chunks split at `cut`, exercising cross-chunk state.
    fn signals_split(input: &[u8], cut: usize) -> Vec<Signal> {
        let mut scanner = NotifyScanner::default();
        let mut out = Vec::new();
        scanner.feed(&input[..cut], &mut out);
        scanner.feed(&input[cut..], &mut out);
        out
    }

    fn scan_split(input: &[u8], cut: usize) -> Vec<Notification> {
        notes(signals_split(input, cut))
    }

    fn progress(state: Option<AgentState>) -> StatusReport {
        StatusReport {
            source: StateSource::Progress,
            state,
            app: None,
            msg: None,
        }
    }

    fn program(state: Option<AgentState>, app: Option<&str>, msg: Option<&str>) -> StatusReport {
        StatusReport {
            source: StateSource::Program,
            state,
            app: app.map(Into::into),
            msg: msg.map(Into::into),
        }
    }

    /// Regression: an OSC 9;4 progress report used to surface as an OSC 9
    /// notification with body `4;1;50`.
    #[test]
    fn osc9_4_progress_is_never_a_notification() {
        for input in [
            b"\x1b]9;4;1;50\x07".as_slice(),
            b"\x1b]9;4;0\x1b\\",
            b"\x1b]9;4;9;10\x07",
            b"\x1b]9;4\x07",
            b"\x1b]9;4;\x07",
        ] {
            assert!(scan(input).is_empty(), "{input:?} became a notification");
        }
    }

    #[test]
    fn osc9_4_states_map_onto_progress_reports() {
        use AgentState::*;
        let cases: &[(&[u8], Option<StatusReport>)] = &[
            (b"\x1b]9;4;0\x1b\\", Some(progress(None))),
            (b"\x1b]9;4;0;0\x07", Some(progress(None))),
            (b"\x1b]9;4;1;40\x1b\\", Some(progress(Some(Working)))),
            (b"\x1b]9;4;1\x07", Some(progress(Some(Working)))),
            (b"\x1b]9;4;2;100\x07", Some(progress(Some(Done)))),
            (b"\x1b]9;4;3\x07", Some(progress(Some(Working)))),
            (b"\x1b]9;4;4;70\x1b\\", Some(progress(Some(Working)))),
            (b"\x1b]9;4;5\x07", None),
            (b"\x1b]9;4\x07", None),
        ];
        for (input, want) in cases {
            assert_eq!(scan_status(input).pop(), *want, "for {input:?}");
        }
    }

    #[test]
    fn osc9_4_is_incremental_across_every_cut_point() {
        let input = b"a\x1b]9;4;1;40\x1b\\b\x1b]9;4;0\x07";
        let whole = signals(input);
        assert_eq!(
            whole,
            vec![
                Signal::Status(progress(Some(AgentState::Working))),
                Signal::Status(progress(None)),
            ]
        );
        for cut in 0..=input.len() {
            assert_eq!(
                signals_split(input, cut),
                whole,
                "mismatch cutting at {cut}"
            );
        }
    }

    #[test]
    fn osc7501_valid_reports_with_bel_and_st() {
        use AgentState::*;
        // "Compiling" / "Built" in standard base64, padded and not.
        assert_eq!(
            scan_status(b"\x1b]7501;state=working:app=build:progress=40:msg=Q29tcGlsaW5n\x1b\\"),
            vec![program(Some(Working), Some("build"), Some("Compiling"))]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=done:app=build:msg=QnVpbHQ=\x07"),
            vec![program(Some(Done), Some("build"), Some("Built"))]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=done:msg=QnVpbHQ\x07"),
            vec![program(Some(Done), None, Some("Built"))]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=blocked:kind=permission\x07"),
            vec![program(Some(Blocked), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=error\x07"),
            vec![program(Some(Done), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=idle:title=QnVpbGQ=\x07"),
            vec![program(Some(Idle), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=clear\x1b\\"),
            vec![program(None, None, None)]
        );
        // Whitespace is trimmed, unknown keys are ignored, a malformed pair (no
        // `=`, or a byte outside the value set) is skipped while the rest still
        // counts, and a repeated key's last value wins.
        assert_eq!(
            scan_status(b"\x1b]7501; state = idle :future=1:junk:app=a b:state=working\x07"),
            vec![program(Some(Working), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=done:msg=Zm9v$\x07"),
            vec![program(Some(Done), None, None)]
        );
        assert!(
            scan(b"\x1b]7501;state=working\x07").is_empty(),
            "no notification"
        );
    }

    #[test]
    fn osc7501_invalid_reports_are_dropped() {
        let long_key = format!("\x1b]7501;state=working:{}=1\x07", "k".repeat(KEY_MAX + 1));
        let long_app = format!(
            "\x1b]7501;state=working:app={}\x07",
            "a".repeat(APP_MAX + 1)
        );
        let long_msg = format!(
            "\x1b]7501;state=done:msg={}\x07",
            BASE64.encode("m".repeat(MSG_MAX.1 + 1))
        );
        let long_title = format!(
            "\x1b]7501;state=done:title={}\x07",
            BASE64.encode("t".repeat(TITLE_MAX.1 + 1))
        );
        let control_msg = format!("\x1b]7501;state=done:msg={}\x07", BASE64.encode("a\nb"));
        let non_utf8_msg = format!(
            "\x1b]7501;state=done:msg={}\x07",
            BASE64.encode([0xff, 0xfe])
        );
        let control_title = format!(
            "\x1b]7501;state=done:title={}\x07",
            BASE64.encode("\x1b[2J")
        );
        // Ignored: no/unknown state, kind off blocked, progress outside 0–100,
        // a non-root record, the support probe. Discarded: a broken limit, or a
        // msg/title that is not base64 of control-free UTF-8.
        for input in [
            "\x1b]7501;app=build\x07",
            "\x1b]7501;state=sleeping\x07",
            "\x1b]7501;state=working:kind=permission\x07",
            "\x1b]7501;state=working:progress=101\x07",
            "\x1b]7501;state=working:progress=-1\x07",
            "\x1b]7501;state=working:progress=+5\x07",
            "\x1b]7501;state=working:id=build/test\x07",
            "\x1b]7501;?\x1b\\",
            "\x1b]7501;state=done:msg=Z\x07",
            &long_key,
            &long_app,
            &long_msg,
            &long_title,
            &control_msg,
            &non_utf8_msg,
            &control_title,
        ] {
            let reports = scan_status(input.as_bytes());
            assert!(
                reports.is_empty(),
                "{input:?} should be dropped: {reports:?}"
            );
        }
    }

    #[test]
    fn osc7501_is_incremental_across_every_cut_point() {
        let input = b"x\x1b]7501;state=working:app=build\x1b\\y\x1b]7501;state=clear\x07";
        let whole = signals(input);
        assert_eq!(
            whole,
            vec![
                Signal::Status(program(Some(AgentState::Working), Some("build"), None)),
                Signal::Status(program(None, None, None)),
            ]
        );
        for cut in 0..=input.len() {
            assert_eq!(
                signals_split(input, cut),
                whole,
                "mismatch cutting at {cut}"
            );
        }
    }

    #[test]
    fn oversized_osc7501_is_discarded() {
        let mut input = b"\x1b]7501;state=working:app=x:pad=".to_vec();
        input.resize(OSC_CAP + 10, b'a');
        input.push(0x07);
        assert!(scan_status(&input).is_empty());
    }

    fn bell() -> Notification {
        Notification {
            title: None,
            body: None,
        }
    }

    #[test]
    fn bare_bel_is_a_bell() {
        assert_eq!(scan(b"\x07"), vec![bell()]);
        assert_eq!(scan(b"ab\x07cd\x07"), vec![bell(), bell()]);
    }

    #[test]
    fn osc9_with_bel_terminator() {
        assert_eq!(
            scan(b"\x1b]9;build done\x07"),
            vec![Notification {
                title: None,
                body: Some("build done".into()),
            }]
        );
    }

    #[test]
    fn osc9_with_st_terminator() {
        assert_eq!(
            scan(b"\x1b]9;hi\x1b\\"),
            vec![Notification {
                title: None,
                body: Some("hi".into()),
            }]
        );
    }

    #[test]
    fn osc777_carries_title_and_body() {
        assert_eq!(
            scan(b"\x1b]777;notify;Agent;ready to merge\x07"),
            vec![Notification {
                title: Some("Agent".into()),
                body: Some("ready to merge".into()),
            }]
        );
    }

    #[test]
    fn osc777_body_may_contain_semicolons() {
        assert_eq!(
            scan(b"\x1b]777;notify;T;a;b;c\x1b\\"),
            vec![Notification {
                title: Some("T".into()),
                body: Some("a;b;c".into()),
            }]
        );
    }

    #[test]
    fn bel_terminating_an_osc_is_not_also_a_bell() {
        // One notification, and no spurious bell from the terminating BEL.
        assert_eq!(scan(b"\x1b]9;x\x07").len(), 1);
    }

    #[test]
    fn non_notification_osc_is_ignored() {
        // OSC 0 (window title), and OSC 777 with a non-notify subcommand.
        assert!(scan(b"\x1b]0;my title\x07").is_empty());
        assert!(scan(b"\x1b]777;precmd\x07").is_empty());
    }

    #[test]
    fn oversized_payload_is_discarded_but_the_stream_recovers() {
        let mut input = b"\x1b]9;".to_vec();
        input.resize(input.len() + OSC_CAP + 100, b'x');
        input.push(0x07); // terminates the (overflowed, discarded) OSC
        input.push(0x07); // a following bell still registers
        assert_eq!(
            scan(&input),
            vec![bell()],
            "the huge OSC is dropped, the trailing bell survives"
        );
    }

    #[test]
    fn esc_bel_still_counts_as_a_bell() {
        // A bare `ESC BEL`, and an `ESC <byte> BEL`, both surface the bell that
        // the `Esc` state used to swallow.
        assert_eq!(scan(b"\x1b\x07"), vec![bell()]);
        assert_eq!(scan(b"\x1bx\x07"), vec![bell()]);
    }

    #[test]
    fn esc_bel_is_incremental_across_every_cut_point() {
        for input in [b"\x1b\x07".as_slice(), b"\x1bx\x07".as_slice()] {
            let whole = scan(input);
            assert_eq!(whole, vec![bell()], "whole scan of {input:?}");
            for cut in 0..=input.len() {
                assert_eq!(
                    scan_split(input, cut),
                    whole,
                    "mismatch cutting {input:?} at {cut}"
                );
            }
        }
    }

    #[test]
    fn scanning_is_incremental_across_every_cut_point() {
        let input = b"pre\x07\x1b]9;done\x07mid\x1b]777;notify;A;B\x1b\\post\x07";
        let whole = scan(input);
        assert_eq!(whole.len(), 4, "bell, osc9, osc777, bell");
        for cut in 0..=input.len() {
            assert_eq!(scan_split(input, cut), whole, "mismatch cutting at {cut}");
        }
    }

    /// A missing cwd must fail the spawn: portable-pty would otherwise fall
    /// back to $HOME and quietly run the child against the wrong tree.
    #[test]
    fn spawn_refuses_a_cwd_that_does_not_exist() {
        let mut spec = PtySpec::new("/bin/sh");
        spec.cwd = Some(PathBuf::from("/nonexistent/tutti-test-dir"));
        match PtyPane::spawn(spec, PaneSize::new(24, 80)) {
            Err(err) => assert!(
                err.to_string().contains("does not exist"),
                "unexpected error: {err}"
            ),
            Ok(_) => panic!("spawn accepted a missing cwd"),
        }
    }

    #[test]
    fn resize_refuses_a_zero_dimension() {
        let pane = PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 80)).unwrap();
        assert!(pane.resize(0, 80).is_err());
        assert!(pane.resize(24, 0).is_err());
        assert_eq!(pane.snapshot().rows, 24, "a refused resize leaves the grid");
        pane.kill().unwrap();
    }

    /// Killing a pane must terminate the child's whole process group. A
    /// backgrounded grandchild that ignores SIGHUP and holds the slave pty
    /// survives portable-pty's SIGHUP-to-the-direct-child kill (and, on Linux,
    /// keeps the master reader blocked so `Drop` hangs); only a process-group
    /// kill takes it down. We assert the group is fully reaped, then that
    /// dropping the pane does not hang.
    #[test]
    fn kill_reaps_the_whole_process_group() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Instant;

        let mut spec = PtySpec::new("/bin/sh");
        spec.args = vec![
            "-c".into(),
            "sh -c 'trap \"\" HUP; while :; do sleep 1; done' & exec sleep 30".into(),
        ];
        let pane = PtyPane::spawn(spec, PaneSize::new(24, 80)).unwrap();
        let pid = pane.child_pid().expect("unix reports a child pid");

        // Let the shell fork the backgrounded grandchild into the group.
        std::thread::sleep(Duration::from_millis(250));
        pane.kill().unwrap();

        // The child is its own group leader, so `kill(-pid, 0)` succeeds while
        // any group member (the grandchild) is still alive.
        let reaped = {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if libc_kill(-(pid as i32), 0) != 0 {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        assert!(
            reaped,
            "process group survived the kill: a descendant was left running"
        );

        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        std::thread::spawn(move || {
            drop(pane);
            flag.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "dropping a killed pane hung: the reader thread never saw EOF"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
