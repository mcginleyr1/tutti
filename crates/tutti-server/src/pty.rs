//! Persistent PTY management: one native pty per pane, driven by a background
//! reader thread that feeds bytes into a shared `vt100` grid. The child keeps
//! running whether or not anything is reading the UI — this is the daemon's core.

use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::watch;
use tutti_core::{AgentState, StateSource};

const SCROLLBACK_LINES: usize = 10_000;
/// The most bytes `Term::process` feeds between two readings of the scroll
/// count. A byte pushes at most one row (a scroll-up excepted, and it is fed
/// alone), so no run can push past what the 10k ring's offset counts.
const RUN_MAX: usize = 8192;

/// What to run in a pane.
#[derive(Debug, Clone)]
pub struct PtySpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Additional environment variables layered onto the inherited environment.
    pub env: Vec<(String, String)>,
    /// Inherited environment variables dropped before `env` is layered on.
    pub env_remove: Vec<String>,
}

impl PtySpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
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

/// A `pane read` reply: the rows committed to the scrollback from a cursor
/// on, then the live screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// The committed rows past the cursor, oldest first, then the screen
    /// with its trailing blank rows trimmed.
    pub lines: Vec<String>,
    /// The next read's `since`: the rows ever committed, as of this read. It
    /// counts screen rows, not logical lines — `unwrapped` folds each read on
    /// its own, so a line wrapped across two reads comes back in two pieces.
    pub cursor: u64,
    /// Rows past the cursor had already fallen out of the 10k ring.
    pub truncated: bool,
}

/// A scrollback-free copy of a pane's live screen: what the broadcast tick
/// diffs and keeps. Cloning the live screen drags its whole 10k-row ring
/// along (~0.7 ms at 80×24, ~2.6 ms at 200×50, release); formatting just the
/// visible state under the lock and replaying it costs ~25–100 µs, and the
/// replayed screen holds everything a diff reads.
pub struct Mirror {
    /// The live screen's `state_formatted` bytes — contents plus input modes,
    /// a new client's snapshot as is.
    pub snapshot: Vec<u8>,
    /// A fresh parser, without scrollback, that replayed `snapshot`.
    pub parser: vt100::Parser,
}

impl Mirror {
    /// Replay `snapshot`, taken from a `size` screen under its parser lock,
    /// into a fresh parser — off the lock. vt100 can hold a state it cannot
    /// replay (a wide character a shrink left in a one-column pane panics
    /// `process`), and the render tick must outlive that: such a mirror falls
    /// back to a blank screen, snapshot included — a client's parser, vt100
    /// too, would panic on the original.
    fn new(size: PaneSize, snapshot: Vec<u8>) -> Self {
        let replay = |bytes: &[u8]| {
            let mut parser = vt100::Parser::new(size.rows, size.cols, 0);
            parser.process(bytes);
            parser
        };
        match catch_unwind(|| replay(&snapshot)) {
            Ok(parser) => Self { snapshot, parser },
            Err(_) => {
                let parser = replay(b"");
                Self {
                    snapshot: parser.screen().state_formatted(),
                    parser,
                }
            }
        }
    }
}

/// A pane's terminal behind the parser lock: the vt100 parser and the count
/// numbering the rows its primary screen commits to scrollback, kept
/// together so a reader sees a grid and a count that agree.
struct Term {
    parser: vt100::Parser,
    /// Rows ever pushed into the primary screen's scrollback: the absolute
    /// index one past the newest committed row, and the `--since` cursor.
    scrolled: u64,
    /// How far into an escape sequence the bytes fed so far stand.
    seq: Seq,
}

impl Term {
    fn new(size: PaneSize) -> Self {
        Self {
            parser: vt100::Parser::new(size.rows, size.cols, SCROLLBACK_LINES),
            scrolled: 0,
            seq: Seq::default(),
        }
    }

    /// Start over on a blank screen of the same size, after the parser
    /// panicked mid-read and left its state suspect. The count only moves
    /// forward — by one, for whatever the failed read pushed uncounted — so
    /// every outstanding cursor reads as truncated instead of skipping rows
    /// silently.
    fn reset(&mut self) {
        let (rows, cols) = self.parser.screen().size();
        *self = Self {
            scrolled: self.scrolled + 1,
            ..Self::new(PaneSize::new(rows, cols))
        };
    }

    /// Feed one read's bytes to the parser, counting the rows they push into
    /// the primary screen's scrollback.
    ///
    /// vt100 keeps no such count, but its scrollback offset can: `scroll_up`
    /// bumps a non-zero offset once per row it pushes, and the offset only
    /// steers the read APIs, never drawing. So each run of bytes is fed with
    /// the primary armed at offset 1, what the offset grew by is the count,
    /// and it is zeroed again before the lock is released — no reader ever
    /// sees it.
    ///
    /// Three things break that mid-run, each on an escape sequence's final
    /// byte: entering the alternate screen zeroes the primary's offset
    /// (`CSI ?47h`/`?1049h`), leaving it lands on a primary that could not be
    /// armed while hidden (`…l`), and a reset (`ESC c`) replaces the screen,
    /// ring and all. So `Seq` picks those bytes out and each is fed alone,
    /// once the run before it is counted. A scroll-up (`CSI n S`) is fed alone
    /// too: any other byte pushes at most one row and a run is at most
    /// `RUN_MAX`, while a lone `S` pushes at most a screen height — so the
    /// count is exact for any pane shorter than the ring.
    fn process(&mut self, bytes: &[u8]) {
        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if self.seq.step(b) {
                self.run(&bytes[start..i]);
                self.run(&bytes[i..=i]);
                start = i + 1;
            }
        }
        self.run(&bytes[start..]);
    }

    /// Feed bytes that switch screens, if at all, only as a lone byte, adding
    /// the rows they push to `scrolled`.
    fn run(&mut self, bytes: &[u8]) {
        for run in bytes.chunks(RUN_MAX) {
            let screen = self.parser.screen_mut();
            let primary = !screen.alternate_screen();
            if primary {
                screen.set_scrollback(1);
            }
            // An empty ring clamps the offset back to 0; its length after the
            // run is then the count, as nothing can have been popped.
            let armed = screen.scrollback() == 1;
            self.parser.process(run);
            let screen = self.parser.screen_mut();
            // Nothing reaches the primary while the alternate screen is up,
            // and a lone switching byte pushes nothing either way.
            if !primary || screen.alternate_screen() {
                continue;
            }
            let pushed = if armed {
                screen.scrollback().saturating_sub(1)
            } else {
                screen.set_scrollback(usize::MAX);
                screen.scrollback()
            };
            screen.set_scrollback(0);
            self.scrolled += pushed as u64;
        }
    }

    /// The committed rows from absolute index `since` on, then the screen;
    /// see `PtyPane::tail`.
    fn tail(&mut self, since: u64, lines: Option<usize>, unwrapped: bool) -> Result<Tail> {
        let scrolled = self.scrolled;
        if since > scrolled {
            bail!(
                "cursor {since} is past this pane's {scrolled}: it came from another pane or an \
                 earlier daemon"
            );
        }
        let screen = self.parser.screen_mut();
        let alternate = screen.alternate_screen();
        let (rows, cols) = screen.size();
        let rows = usize::from(rows);
        screen.set_scrollback(usize::MAX);
        // The alternate grid keeps no ring: 0 while it is up.
        let held = screen.scrollback();
        let oldest = scrolled - held as u64;
        let committed = (scrolled - since.max(oldest)) as usize;

        // Tile from the first wanted row to the foot of the screen with
        // screen-height windows: `visible_rows` shows `rows` rows at a time,
        // so step the offset and skip what an earlier window yielded.
        let mut out: Vec<(String, bool)> = Vec::with_capacity(committed + rows);
        let mut at = held - committed;
        while at < held + rows {
            let offset = held.saturating_sub(at);
            screen.set_scrollback(offset);
            let view = &*screen;
            out.extend(
                view.rows(0, cols)
                    .zip(0..)
                    .skip(at - (held - offset))
                    .map(|(text, row)| (text, view.row_wrapped(row))),
            );
            at = held - offset + rows;
        }
        screen.set_scrollback(0);

        while out.len() > committed && out.last().is_some_and(|(text, _)| text.is_empty()) {
            out.pop();
        }
        let mut text: Vec<String> = Vec::with_capacity(out.len());
        let mut continues = false;
        for (row, wrapped) in out {
            match text.last_mut() {
                Some(line) if unwrapped && continues => line.push_str(&row),
                _ => text.push(row),
            }
            continues = wrapped;
        }
        if let Some(n) = lines
            && text.len() > n
        {
            text.drain(..text.len() - n);
        }
        // vt100 reads only the active grid, so while the alternate screen is
        // up the ring is out of reach and the cursor holds: nothing is pushed
        // until it comes down, so the rows past the cursor wait for the next
        // read rather than being skipped.
        let (cursor, truncated) = if alternate {
            (since, false)
        } else {
            (scrolled, since < oldest)
        };
        Ok(Tail {
            lines: text,
            cursor,
            truncated,
        })
    }
}

/// Where the bytes fed so far stand within an escape sequence — as much of
/// vte's state machine as `Term::process` needs to pick out the bytes it
/// feeds alone. It persists across reads, as a sequence can straddle two.
#[derive(Clone, Copy, Default)]
enum Seq {
    #[default]
    Ground,
    /// Just after `ESC`.
    Esc,
    /// Inside `ESC [`, before the final byte.
    Csi,
}

impl Seq {
    /// Step over `b`: true when it ends a sequence that can switch screens or
    /// push many rows — `CSI … h`/`l` (the alternate screen is `?47`/`?1049`),
    /// `CSI … S` (scroll up), `ESC c` (full reset). Liberal on purpose: a
    /// false hit only splits a run. As in vte, only `ESC` opens a sequence (a
    /// raw C1 `CSI` byte is executed, not parsed), CAN/SUB abort one, and
    /// other C0 controls execute inside one without ending it.
    fn step(&mut self, b: u8) -> bool {
        let (next, hit) = match (*self, b) {
            (_, 0x1b) => (Seq::Esc, false),
            (_, 0x18 | 0x1a) => (Seq::Ground, false),
            (Seq::Esc, b'[') => (Seq::Csi, false),
            (Seq::Esc, b'c') => (Seq::Ground, true),
            (Seq::Esc | Seq::Csi, 0x00..=0x1f) => (*self, false),
            (Seq::Csi, 0x40..=0x7e) => (Seq::Ground, matches!(b, b'h' | b'l' | b'S')),
            (Seq::Csi, _) => (Seq::Csi, false),
            (Seq::Ground | Seq::Esc, _) => (Seq::Ground, false),
        };
        *self = next;
        hit
    }
}

/// A persistent pty running a single child process.
pub struct PtyPane {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    term: Arc<Mutex<Term>>,
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
        for key in &spec.env_remove {
            cmd.env_remove(key);
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

        let term = Arc::new(Mutex::new(Term::new(size)));

        let (output_tx, output_rx) = watch::channel(0u64);
        let (exit_tx, exit_rx) = watch::channel(None);
        let notifications = Arc::new(Mutex::new(Vec::new()));

        let reader_term = Arc::clone(&term);
        let reader_notifications = Arc::clone(&notifications);
        let reader_thread = std::thread::Builder::new()
            .name("tutti-pty-reader".into())
            .spawn(move || read_loop(reader, reader_term, output_tx, reader_notifications))
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
            term,
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
        let mut writer = lock(&self.writer);
        writer.write_all(bytes).context("write to pty failed")?;
        writer.flush().context("flush pty failed")?;
        Ok(())
    }

    /// Resize both the kernel pty and the vt100 grid. A zero dimension is
    /// refused: `tail` tiles scrollback in screen-height windows and would
    /// never advance.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        if rows == 0 || cols == 0 {
            bail!("pane size must be at least 1x1, got {rows}x{cols}");
        }
        lock(&self.master)
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty failed")?;
        lock(&self.term).parser.screen_mut().set_size(rows, cols);
        *lock(&self.size) = PaneSize { rows, cols };
        Ok(())
    }

    /// The size the pane was spawned at or last resized to.
    pub fn size(&self) -> PaneSize {
        *lock(&self.size)
    }

    /// Current screen contents, cursor position, and size.
    pub fn snapshot(&self) -> Snapshot {
        let term = lock(&self.term);
        let screen = term.parser.screen();
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

    /// A scrollback-free mirror of the live screen for the broadcast tick:
    /// the live view's snapshot, taken under the parser lock, replayed off it.
    pub fn mirror(&self) -> Mirror {
        let (size, snapshot) = self.scrolled_snapshot(0);
        Mirror::new(size, snapshot)
    }

    /// A client snapshot (`state_formatted`) of the screen scrolled `offset`
    /// rows back into the ring, with its size. The offset is set, the screen
    /// formatted and the offset put back under one hold of the parser lock,
    /// so no other reader sees the scrolled view and no clone of the ring is
    /// made.
    pub fn scrolled_snapshot(&self, offset: usize) -> (PaneSize, Vec<u8>) {
        let mut term = lock(&self.term);
        let screen = term.parser.screen_mut();
        screen.set_scrollback(offset);
        let (rows, cols) = screen.size();
        let snapshot = screen.state_formatted();
        screen.set_scrollback(0);
        (PaneSize::new(rows, cols), snapshot)
    }

    /// The live screen's text, read under the parser lock rather than off a
    /// clone of the whole ring: the classifier only scans it.
    pub fn contents(&self) -> String {
        lock(&self.term).parser.screen().contents()
    }

    /// The rows committed to the primary scrollback from absolute index
    /// `since` on (clamped to the oldest the ring still holds), then the live
    /// screen with its trailing blank rows trimmed — `since: 0` reads it all.
    /// `lines` keeps only the newest that many; `unwrapped` joins soft-wrapped
    /// rows back into logical lines by vt100's own wrap flags. While the
    /// alternate screen is up the ring is out of reach: the reply is the
    /// screen alone and the cursor holds at `since`. Formats under the parser
    /// lock, in time proportional to the rows returned: a `--since` tail of a
    /// few rows holds it for microseconds, the whole 10k ring for a few
    /// milliseconds (release). A cursor past the pane's count — from another
    /// pane, or an earlier daemon — is refused.
    pub fn tail(&self, since: u64, lines: Option<usize>, unwrapped: bool) -> Result<Tail> {
        lock(&self.term).tail(since, lines, unwrapped)
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
        std::mem::take(&mut *lock(&self.notifications))
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
        lock(&self.killer).kill().context("kill child failed")
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

/// Lock a pty mutex, recovering it if a panicking thread poisoned it: one
/// pane's panic must not turn every later lock — the render tick's among them
/// — into another. The reader resets a parser its own panic left suspect; a
/// panic in a read path (`tail`, a snapshot) leaves at worst a scrollback
/// offset, which the reader's next arming overwrites.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read_loop(
    mut reader: Box<dyn Read + Send>,
    term: Arc<Mutex<Term>>,
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
                    let mut queue = lock(&notifications);
                    queue.extend(found);
                    let overflow = queue.len().saturating_sub(NOTIFY_QUEUE_CAP);
                    if overflow > 0 {
                        queue.drain(..overflow);
                    }
                }
                // Bumped under the parser lock, so a `snapshot` sees a grid and
                // a generation that agree.
                let mut term = lock(&term);
                // A vt100 panic (a wide character in a one-column pane is one)
                // must neither end this thread nor poison the lock the render
                // tick takes: caught inside the guard, the pane runs on from a
                // blank screen.
                if catch_unwind(AssertUnwindSafe(|| term.process(&buf[..n]))).is_err() {
                    eprintln!("tutti: a pane's terminal parser panicked; restarting it blank");
                    term.reset();
                }
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
/// Anything else yields `None` — ConEmu's other numbered `9;<n>[;…]`
/// subcommands included.
fn parse_osc(buf: &[u8]) -> Option<Signal> {
    let text = std::str::from_utf8(buf).ok()?;
    let (cmd, rest) = text.split_once(';')?;
    match cmd {
        // ConEmu's numbered subcommands are never notifications, even
        // malformed: `9;4` is progress, and the rest (`9;9;<cwd>`, which
        // shells emit for Windows Terminal, …) say nothing tutti reads.
        "9" => match rest.split_once(';').unwrap_or((rest, "")) {
            ("4", body) => parse_progress(body).map(Signal::Status),
            (sub, _) if !sub.is_empty() && sub.bytes().all(|b| b.is_ascii_digit()) => None,
            _ => non_empty(rest).map(|body| {
                Signal::Notify(Notification {
                    title: None,
                    body: Some(body),
                })
            }),
        },
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

    /// ConEmu's other numbered subcommands — `9;9;<cwd>`, which shells emit
    /// for Windows Terminal, and the rest — surface nothing either, while a
    /// body that merely starts with digits is still a notification.
    #[test]
    fn osc9_subcommands_are_never_notifications() {
        for input in [
            b"\x1b]9;9;/Users/me/src\x1b\\".as_slice(),
            b"\x1b]9;9;\x07",
            b"\x1b]9;1;500\x07",
            b"\x1b]9;2;hello there\x07",
            b"\x1b]9;10;1\x07",
            b"\x1b]9;12\x07",
        ] {
            assert!(signals(input).is_empty(), "{input:?} surfaced");
        }
        assert_eq!(
            scan(b"\x1b]9;42 tests passed\x07"),
            vec![Notification {
                title: None,
                body: Some("42 tests passed".into()),
            }]
        );
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

    /// A `rows`×`cols` terminal fed `bytes` in reads of `chunk` bytes, as the
    /// reader thread would — checking after each that the scrollback offset
    /// never leaks out of a read.
    fn fed(rows: u16, cols: u16, bytes: &[u8], chunk: usize) -> Term {
        let mut term = Term::new(PaneSize::new(rows, cols));
        for read in bytes.chunks(chunk) {
            term.process(read);
            assert_eq!(term.parser.screen().scrollback(), 0, "the offset leaked");
        }
        term
    }

    /// `n` lines `L<first>`, `L<first+1>`, …, each its own row.
    fn numbered(first: usize, n: usize) -> String {
        (first..first + n).map(|i| format!("L{i}\r\n")).collect()
    }

    /// The rows `numbered` printed for `range`, as `tail` returns them.
    fn rows_of(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("L{i}")).collect()
    }

    /// The newest row the ring holds. The count only picks where `tail`
    /// starts, not which row is newest, so `L<k>` here checks a count of k+1.
    fn newest_committed(term: &mut Term) -> String {
        let since = term.scrolled - 1;
        term.tail(since, None, false).unwrap().lines.remove(0)
    }

    #[test]
    fn scroll_count_matches_the_rows_plain_output_commits() {
        // Five rows: four lines fill the screen, each later one pushes one.
        let mut term = fed(5, 20, numbered(0, 40).as_bytes(), RUN_MAX);
        assert_eq!(term.scrolled, 36);
        assert_eq!(newest_committed(&mut term), "L35");
    }

    #[test]
    fn scroll_count_holds_across_a_read_boundary_anywhere() {
        let stream = [
            numbered(0, 12),
            "ABCDEFGHIJKLMNOPQRSTUVWXY\r\n".into(),
            format!("\x1b[?1049h{}\x1b[?1049l", numbered(100, 9)),
            numbered(12, 6),
            "\x1b[2S".into(),
        ]
        .concat();
        // 12 lines push 8; the 25-char line wraps twice on 10 columns and
        // ends, 3; the alternate screen pushes none; 6 lines, 6; the
        // scroll-up, 2.
        assert_eq!(fed(5, 10, stream.as_bytes(), RUN_MAX).scrolled, 19);
        for cut in 0..=stream.len() {
            let mut term = Term::new(PaneSize::new(5, 10));
            term.process(&stream.as_bytes()[..cut]);
            term.process(&stream.as_bytes()[cut..]);
            assert_eq!(term.scrolled, 19, "reads split at byte {cut}");
        }
        for chunk in [1, 2, 3, 5, 64] {
            assert_eq!(
                fed(5, 10, stream.as_bytes(), chunk).scrolled,
                19,
                "{chunk}-byte reads"
            );
        }
    }

    /// Entering the alternate screen zeroes the primary's offset, and the
    /// primary cannot be armed while hidden: rows pushed just before entering
    /// and just after leaving, in the same read, must still count.
    #[test]
    fn scroll_count_skips_the_alternate_screen_within_one_read() {
        let stream = format!(
            "{}\x1b[?1049h{}\x1b[?1049l{}",
            numbered(0, 10),
            numbered(100, 20),
            numbered(10, 7)
        );
        let mut term = fed(5, 20, stream.as_bytes(), RUN_MAX);
        assert_eq!(term.scrolled, 13);
        assert_eq!(newest_committed(&mut term), "L12");
    }

    #[test]
    fn scroll_count_skips_the_alternate_screen_across_reads() {
        let mut term = Term::new(PaneSize::new(5, 20));
        for read in [
            format!("{}\x1b[?1049h", numbered(0, 10)),
            numbered(100, 20),
            format!("\x1b[?1049l{}", numbered(10, 7)),
        ] {
            term.process(read.as_bytes());
            assert_eq!(term.parser.screen().scrollback(), 0, "the offset leaked");
        }
        assert_eq!(term.scrolled, 13);
        assert_eq!(newest_committed(&mut term), "L12");
    }

    #[test]
    fn scroll_count_stays_exact_once_the_ring_is_full() {
        // 25k lines overflow the 10k ring long before the end; after every
        // 8 KiB read the count still names the newest row.
        let mut term = Term::new(PaneSize::new(24, 80));
        for read in numbered(0, 25_000).as_bytes().chunks(8192) {
            term.process(read);
            assert_eq!(term.parser.screen().scrollback(), 0, "the offset leaked");
            if term.scrolled > 0 {
                let newest = format!("L{}", term.scrolled - 1);
                assert_eq!(newest_committed(&mut term), newest);
            }
        }
        assert_eq!(term.scrolled, 25_000 - 23);
    }

    #[test]
    fn scroll_count_continues_across_a_resize() {
        let mut term = fed(24, 80, numbered(0, 100).as_bytes(), RUN_MAX);
        assert_eq!(term.scrolled, 77);
        // A shrink drops the screen's foot (L87..L99) without pushing it and
        // pulls the cursor onto the new last row, over L86: every line pushes
        // again — L77..L85, then the new lines from L100.
        term.parser.screen_mut().set_size(10, 40);
        term.process(numbered(100, 50).as_bytes());
        assert_eq!(term.scrolled, 127);
        assert_eq!(newest_committed(&mut term), "L140");
        // A growth leaves the cursor where it was: 20 lines walk it down to
        // the new foot, pushing nothing, and the next 5 push 5.
        term.parser.screen_mut().set_size(30, 40);
        term.process(numbered(150, 20).as_bytes());
        assert_eq!(term.scrolled, 127);
        term.process(numbered(170, 5).as_bytes());
        assert_eq!(term.scrolled, 132);
    }

    #[test]
    fn scroll_count_handles_scroll_up_regions_and_resets() {
        let mut term = fed(5, 20, numbered(0, 10).as_bytes(), RUN_MAX);
        assert_eq!(term.scrolled, 6);
        // A scroll-up pushes up to a screen height per sequence: 3000 in one
        // read push 15k rows, past anything one run's offset could count.
        term.process("\x1b[100S".repeat(3000).as_bytes());
        assert_eq!(term.scrolled, 15_006);
        // Inside a scroll region nothing reaches the scrollback.
        term.process(format!("\x1b[2;4r\x1b[4;1H{}", numbered(0, 20)).as_bytes());
        assert_eq!(term.scrolled, 15_006);
        // A full reset empties the ring; the count carries on past it.
        term.process(format!("\x1b[r\x1bc{}", numbered(0, 10)).as_bytes());
        assert_eq!(term.scrolled, 15_012);
        assert_eq!(newest_committed(&mut term), "L5");
        assert!(term.tail(0, None, false).unwrap().truncated);
    }

    #[test]
    fn tail_reads_the_rows_committed_since_the_cursor_then_the_screen() {
        // 26 rows committed (L0..L25); the screen holds L26..L29 over a blank
        // cursor row, which is trimmed.
        let mut term = fed(5, 20, numbered(0, 30).as_bytes(), RUN_MAX);
        let all = term.tail(0, None, false).unwrap();
        assert_eq!(all.lines, rows_of(0..30));
        assert_eq!((all.cursor, all.truncated), (26, false));
        assert_eq!(term.tail(20, None, false).unwrap().lines, rows_of(20..30));
        // Caught up, a read is the screen alone; `lines` keeps the newest.
        assert_eq!(term.tail(26, None, false).unwrap().lines, rows_of(26..30));
        assert_eq!(term.tail(0, Some(3), false).unwrap().lines, rows_of(27..30));
        // More output: a reread from the old cursor starts where it left off.
        term.process(numbered(30, 3).as_bytes());
        let next = term.tail(26, None, false).unwrap();
        assert_eq!(next.lines, rows_of(26..33));
        assert_eq!(next.cursor, 29);
    }

    #[test]
    fn tail_reports_rows_that_left_the_ring() {
        // 10_077 rows committed; the ring keeps the newest 10k, from L77.
        let mut term = fed(24, 80, numbered(0, 10_100).as_bytes(), 8192);
        let tail = term.tail(50, None, false).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.lines[0], "L77");
        assert_eq!(tail.cursor, 10_077);
        assert!(!term.tail(77, None, false).unwrap().truncated);
    }

    #[test]
    fn tail_holds_the_cursor_while_the_alternate_screen_is_up() {
        let mut term = fed(5, 20, numbered(0, 10).as_bytes(), RUN_MAX);
        term.process(b"\x1b[?1049hfull screen");
        // The ring is out of reach: the screen alone, and the cursor stays so
        // the rows past it are read once the alternate screen comes down.
        let up = term.tail(2, None, false).unwrap();
        assert_eq!(up.lines, vec!["full screen"]);
        assert_eq!((up.cursor, up.truncated), (2, false));
        term.process(b"\x1b[?1049l");
        let down = term.tail(up.cursor, None, false).unwrap();
        assert_eq!(down.lines, rows_of(2..10));
        assert_eq!(down.cursor, 6);
    }

    #[test]
    fn tail_refuses_a_cursor_past_the_count() {
        let mut term = fed(5, 20, numbered(0, 10).as_bytes(), RUN_MAX);
        let err = term.tail(7, None, false).unwrap_err();
        assert!(err.to_string().contains("past this pane's 6"), "{err}");
    }

    #[test]
    fn unwrapped_joins_exactly_the_soft_wrapped_rows() {
        // Four rows of ten columns. A line of exactly ten fills its row but
        // never wraps — the old full-width guess merged it with the next. The
        // 25-char line wraps onto three rows, and the lines after push all of
        // it into the ring, across the boundary between two read windows.
        let mut term = fed(
            4,
            10,
            format!(
                "0123456789\r\nnext\r\nABCDEFGHIJKLMNOPQRSTUVWXY\r\n{}",
                numbered(0, 8)
            )
            .as_bytes(),
            RUN_MAX,
        );
        let mut want = vec!["0123456789", "next", "ABCDEFGHIJKLMNOPQRSTUVWXY"];
        let numbers = rows_of(0..8);
        want.extend(numbers.iter().map(String::as_str));
        assert_eq!(term.tail(0, None, true).unwrap().lines, want);
        assert_eq!(
            term.tail(0, None, false).unwrap().lines[2..5],
            ["ABCDEFGHIJ", "KLMNOPQRST", "UVWXY"]
        );

        // A 45-char line whose first row is committed and the rest on screen
        // joins across that boundary too.
        let long = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRS";
        term.process(long.as_bytes());
        let since = term.scrolled - 1;
        assert_eq!(term.tail(since, None, true).unwrap().lines, [long]);
        // Folding is per read: from a cursor inside the line, only its rest.
        let since = term.scrolled;
        assert_eq!(term.tail(since, None, true).unwrap().lines, [&long[10..]]);
    }

    /// The plan's `pane read` target — a full 10k-row scrollback in under 50
    /// ms — held here in a debug build, best of three to ride out load.
    #[test]
    fn reading_a_full_ring_takes_under_50ms() {
        let dense: String = (0..12_000)
            .map(|i| {
                format!(
                    "\x1b[3{}m{i:>6}\x1b[0m \x1b[1mINFO\x1b[0m compiling crate {i} of the workspace, step {}\r\n",
                    i % 8,
                    i * 7
                )
            })
            .collect();
        let mut term = fed(24, 80, dense.as_bytes(), 8192);
        let best = (0..3)
            .map(|_| {
                let start = std::time::Instant::now();
                let tail = term.tail(0, None, false).unwrap();
                assert_eq!(tail.lines.len(), 10_000 + 23);
                start.elapsed()
            })
            .min()
            .unwrap();
        assert!(
            best < Duration::from_millis(50),
            "a full read took {best:?}"
        );
    }

    /// Screens stressing what a snapshot must carry: colours and attributes,
    /// wide and combining characters, a soft-wrapped row, a moved and a
    /// hidden cursor, input modes, the alternate screen (with a colour left
    /// set).
    const SCREENS: &[&str] = &[
        "\x1b[1;3;4;7mbold italic under inverse\x1b[0m \x1b[31mred\x1b[38;5;200m idx\x1b[48;2;10;20;30m rgb\x1b[0m",
        "日本語 wide \u{1f600} emoji, e\u{301} combining",
        "a soft-wrapped line that runs well past the eighty columns of this screen, onto the next row",
        "\x1b[5;10Hmoved\x1b[12;40H",
        "\x1b[?25lhidden cursor",
        "\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b=modes",
        "primary\r\n\x1b[?1049h\x1b[2;3Halternate\x1b[32m green",
    ];

    #[test]
    fn mirror_round_trips_the_live_screen() {
        for (rows, cols) in [(24, 80), (50, 200)] {
            for screen in SCREENS {
                let stream = format!("{}{screen}", numbered(0, 30));
                let term = fed(rows, cols, stream.as_bytes(), RUN_MAX);
                let live = term.parser.screen();
                let mirror = Mirror::new(PaneSize::new(rows, cols), live.state_formatted());
                let mirror = mirror.parser.screen();
                assert_eq!(
                    mirror.contents_formatted(),
                    live.contents_formatted(),
                    "{screen:?}"
                );
                assert_eq!(
                    mirror.input_mode_formatted(),
                    live.input_mode_formatted(),
                    "{screen:?}"
                );
            }
        }
    }

    /// The render tick's contract: a client seeded with one snapshot and fed
    /// only the diffs between consecutive mirrors stays level with the live
    /// screen, step after step.
    #[test]
    fn mirror_deltas_keep_a_client_level_with_the_live_screen() {
        let size = PaneSize::new(24, 80);
        let mut live = Term::new(size);
        live.process(numbered(0, 30).as_bytes());
        let mut prev = Mirror::new(size, live.parser.screen().state_formatted());
        let mut client = vt100::Parser::new(size.rows, size.cols, 0);
        client.process(&prev.snapshot);
        let back = "\x1b[?1049l\x1b[?25h\x1b[?1002l\x1b[0mback on the primary\r\n";
        for screen in SCREENS.iter().chain([&back]) {
            live.process(screen.as_bytes());
            let cur = Mirror::new(size, live.parser.screen().state_formatted());
            client.process(&cur.parser.screen().state_diff(prev.parser.screen()));
            let (seen, want) = (client.screen(), live.parser.screen());
            assert_eq!(
                seen.contents_formatted(),
                want.contents_formatted(),
                "after {screen:?}"
            );
            assert_eq!(
                seen.input_mode_formatted(),
                want.input_mode_formatted(),
                "after {screen:?}"
            );
            prev = cur;
        }
    }

    #[test]
    fn a_mirror_vt100_cannot_replay_falls_back_to_a_blank_screen() {
        // A shrink leaves a wide character in a one-column row, and replaying
        // that panics vt100 — which must not take the render tick down.
        let mut live = Term::new(PaneSize::new(2, 4));
        live.process("日本".as_bytes());
        live.parser.screen_mut().set_size(2, 1);
        let snapshot = live.parser.screen().state_formatted();
        let replayed = catch_unwind(|| vt100::Parser::new(2, 1, 0).process(&snapshot));
        assert!(
            replayed.is_err(),
            "vt100 replays this now: drop the fallback"
        );
        let mirror = Mirror::new(PaneSize::new(2, 1), snapshot);
        assert_eq!(mirror.parser.screen().contents(), "");
        assert!(
            catch_unwind(|| vt100::Parser::new(2, 1, 0).process(&mirror.snapshot)).is_ok(),
            "a client is sent the blank screen's snapshot, which replays"
        );
    }

    /// Poll `done` for up to three seconds.
    fn wait_for(done: impl Fn() -> bool, failure: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{failure}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A vt100 panic on the reader thread — a wide character echoed into a
    /// one-column pane — restarts the screen blank rather than ending the
    /// reader or poisoning the parser lock: the pane runs on.
    #[test]
    fn a_parser_panic_restarts_the_screen_and_the_pane_runs_on() {
        let pane = PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 1)).unwrap();
        pane.write_input("日\n".as_bytes()).unwrap();
        // Nothing scrolls a 24-row screen here, so only a restart's bump moves
        // the count.
        wait_for(
            || lock(&pane.term).scrolled > 0,
            "the parser never restarted",
        );
        assert!(!pane.term.is_poisoned());
        pane.resize(24, 80).unwrap();
        pane.write_input(b"still-here\n").unwrap();
        wait_for(
            || pane.snapshot().text().contains("still-here"),
            "output stopped after the panic",
        );
        pane.kill().unwrap();
    }

    /// A thread panicking while it holds a pane's locks poisons them; every
    /// later lock recovers, so the pane still writes, reads and mirrors.
    #[test]
    fn poisoned_locks_recover() {
        let pane =
            Arc::new(PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 80)).unwrap());
        let poisoner = Arc::clone(&pane);
        let _ = std::thread::spawn(move || {
            let _term = poisoner.term.lock();
            let _writer = poisoner.writer.lock();
            let _notifications = poisoner.notifications.lock();
            panic!("poisoning the pane's locks");
        })
        .join();
        assert!(pane.term.is_poisoned() && pane.writer.is_poisoned());
        pane.write_input(b"after-poison\n").unwrap();
        wait_for(
            || pane.snapshot().text().contains("after-poison"),
            "the pane stopped reading after the poison",
        );
        let tail = pane.tail(0, None, false).unwrap();
        assert!(tail.lines.iter().any(|line| line == "after-poison"));
        assert!(
            pane.mirror()
                .parser
                .screen()
                .contents()
                .contains("after-poison")
        );
        assert!(pane.take_notifications().is_empty());
        pane.kill().unwrap();
    }
}
