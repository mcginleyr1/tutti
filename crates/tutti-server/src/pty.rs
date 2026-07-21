//! Persistent PTY management: one native pty per pane, driven by a background
//! reader thread that feeds bytes into a shared `vt100` grid. The child keeps
//! running whether or not anything is reading the UI — this is the daemon's core.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

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

        let reader_parser = Arc::clone(&parser);
        let reader_thread = std::thread::Builder::new()
            .name("tutti-pty-reader".into())
            .spawn(move || read_loop(reader, reader_parser, output_tx))
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

    /// A receiver that resolves to `Some(exit)` when the child terminates.
    pub fn exit_receiver(&self) -> watch::Receiver<Option<PaneExit>> {
        self.exit_rx.clone()
    }

    /// Terminate the child.
    pub fn kill(&self) -> Result<()> {
        self.killer
            .lock()
            .expect("pty killer poisoned")
            .kill()
            .context("kill child failed")?;
        Ok(())
    }
}

impl Drop for PtyPane {
    fn drop(&mut self) {
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.wait_thread.take() {
            let _ = handle.join();
        }
    }
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
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
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

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PtyPane>();
};
