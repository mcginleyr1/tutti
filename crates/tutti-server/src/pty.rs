//! Persistent PTY management: one native pty per pane, driven by a background
//! reader thread that feeds bytes into a shared `vt100` grid. The child keeps
//! running whether or not anything is reading the UI — this is the daemon's core.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::watch;

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
    /// stream, drained by the broadcast tick. A parallel attention channel that
    /// never feeds state classification.
    notifications: Arc<Mutex<Vec<Notification>>>,
    reader_thread: Option<JoinHandle<()>>,
    wait_thread: Option<JoinHandle<()>>,
}

impl PtyPane {
    /// Spawn `spec` on a fresh native pty of the given size. The child runs
    /// detached from any UI; output is accumulated into the vt100 grid by a
    /// background thread from the moment this returns.
    pub fn spawn(spec: PtySpec, size: PaneSize) -> Result<Self> {
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

    /// Resize both the kernel pty and the vt100 grid.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
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

    /// Drain the bells and desktop notifications seen since the last call.
    pub fn take_notifications(&self) -> Vec<Notification> {
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
    notifications: Arc<Mutex<Vec<Notification>>>,
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
                parser
                    .lock()
                    .expect("pty parser poisoned")
                    .process(&buf[..n]);
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

/// Largest OSC payload buffered; a longer sequence is consumed but discarded so
/// a runaway sequence cannot grow memory without bound.
const OSC_CAP: usize = 4096;
/// Cap on pending notifications per pane, bounding the queue when a burst
/// arrives with no client draining it.
const NOTIFY_QUEUE_CAP: usize = 256;

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

/// Incremental scanner pulling bells and OSC 9 / OSC 777 desktop-notification
/// sequences from a raw pty byte stream. State persists across `feed` calls, so
/// a sequence split over read-chunk boundaries is still recognised.
#[derive(Default)]
struct NotifyScanner {
    state: ScanState,
    buf: Vec<u8>,
    overflow: bool,
}

impl NotifyScanner {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<Notification>) {
        for &b in bytes {
            self.step(b, out);
        }
    }

    fn step(&mut self, b: u8, out: &mut Vec<Notification>) {
        match self.state {
            ScanState::Ground => match b {
                0x07 => out.push(Notification {
                    title: None,
                    body: None,
                }),
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

    fn finish_osc(&mut self, out: &mut Vec<Notification>) {
        let overflow = std::mem::take(&mut self.overflow);
        let buf = std::mem::take(&mut self.buf);
        if !overflow && let Some(note) = parse_osc(&buf) {
            out.push(note);
        }
    }
}

/// Parse an OSC payload (the bytes between `ESC ]` and the terminator) for the
/// two commands surfaced as notifications: `9;<body>` and
/// `777;notify;<title>;<body>`. Anything else yields `None`.
fn parse_osc(buf: &[u8]) -> Option<Notification> {
    let text = std::str::from_utf8(buf).ok()?;
    let (cmd, rest) = text.split_once(';')?;
    match cmd {
        "9" => non_empty(rest).map(|body| Notification {
            title: None,
            body: Some(body),
        }),
        "777" => {
            let mut fields = rest.splitn(3, ';');
            if fields.next()? != "notify" {
                return None;
            }
            let title = fields.next().and_then(non_empty);
            let body = fields.next().and_then(non_empty);
            (title.is_some() || body.is_some()).then_some(Notification { title, body })
        }
        _ => None,
    }
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

    fn scan(input: &[u8]) -> Vec<Notification> {
        let mut scanner = NotifyScanner::default();
        let mut out = Vec::new();
        scanner.feed(input, &mut out);
        out
    }

    /// Feed `input` in two chunks split at `cut`, exercising cross-chunk state.
    fn scan_split(input: &[u8], cut: usize) -> Vec<Notification> {
        let mut scanner = NotifyScanner::default();
        let mut out = Vec::new();
        scanner.feed(&input[..cut], &mut out);
        scanner.feed(&input[cut..], &mut out);
        out
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
