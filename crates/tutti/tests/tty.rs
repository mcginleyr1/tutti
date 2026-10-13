//! The TUI on a terminal: `tutti attach` runs on a pty under rtty-vt — a
//! `PtyPane`, standing in for the host terminal, which answers the TUI's
//! colour queries and DA1 from the colours it is given and tells a program
//! that set mode 2031 of each light/dark change — against a daemon serving
//! in-process on a private socket.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rtty_vt::{Color, Terminal};
use tokio::net::UnixListener;
use tokio::sync::oneshot;
use tutti_core::{Frame, HostColors, PaneId, Request, Response, Rgb};
use tutti_server::{PaneSize, PtyPane, PtySpec, serve};

const DEADLINE: Duration = Duration::from_secs(10);

/// A daemon serving `<dir>/tutti/<session>.sock`, where a client run with
/// `XDG_RUNTIME_DIR=<dir>` finds it.
struct Daemon {
    dir: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    fn start(dir: &Path, session: &str) -> Self {
        let sockets = dir.join("tutti");
        std::fs::create_dir_all(&sockets).unwrap();
        let path = sockets.join(format!("{session}.sock"));
        let (shutdown, stopped) = oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = UnixListener::bind(&path).unwrap();
                let shutdown = async move {
                    let _ = stopped.await;
                };
                serve(listener, path, PaneSize::new(24, 80), None, shutdown)
                    .await
                    .unwrap();
            });
        });
        Daemon {
            dir: dir.to_path_buf(),
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A control connection to the daemon.
struct Wire {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Wire {
    fn connect(path: &Path) -> Self {
        let stream = wait(
            || UnixStream::connect(path).ok(),
            "the daemon never came up",
        );
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        Wire {
            stream,
            buf: Vec::new(),
        }
    }

    fn request(&mut self, request: &Request) -> Response {
        let frame = Frame::Control(serde_json::to_vec(request).unwrap());
        self.stream.write_all(&frame.encode()).unwrap();
        wait(
            || {
                let mut chunk = [0u8; 8192];
                if let Ok(n) = self.stream.read(&mut chunk) {
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                let (frame, used) = Frame::decode(&self.buf).unwrap()?;
                self.buf.drain(..used);
                match frame {
                    Frame::Control(json) => serde_json::from_slice(&json).ok(),
                    _ => None,
                }
            },
            "no reply",
        )
    }

    fn screen(&mut self, pane: PaneId) -> String {
        let read = Request::PaneRead {
            pane,
            lines: None,
            unwrapped: false,
            since: None,
            last_output: false,
        };
        match self.request(&read) {
            Response::Content { lines } => lines.join("\n"),
            other => panic!("{other:?}"),
        }
    }
}

fn wait<T>(mut found: impl FnMut() -> Option<T>, failure: &str) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(found) = found() {
            return found;
        }
        assert!(Instant::now() < deadline, "{failure}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn colors(dark: bool) -> HostColors {
    let rgb = |r, g, b| Rgb { r, g, b };
    let (fg, bg) = if dark {
        (rgb(0xabab, 0xb2b2, 0xbfbf), rgb(0x2828, 0x2c2c, 0x3434))
    } else {
        (rgb(0x6565, 0x7b7b, 0x8383), rgb(0xfdfd, 0xf6f6, 0xe3e3))
    };
    HostColors {
        fg,
        bg,
        cursor: None,
        dark,
    }
}

/// The ANSI greys the `terminal` theme's selection row is drawn on that
/// the host's screen shows: 8 on a dark terminal, 7 on a light one.
fn selection_greys(host: &PtyPane) -> Vec<u8> {
    let look = host.look(None, true);
    let mut screen = Terminal::new(look.size.rows, look.size.cols, 0);
    screen.advance(&look.snapshot.unwrap());
    let top = screen.screen_top();
    let mut greys: Vec<u8> = screen
        .rows(top, top + u64::from(look.size.rows))
        .flat_map(|(_, row)| row.cells().to_vec())
        .filter_map(|cell| match cell.bg() {
            Color::Named(n @ (7 | 8)) | Color::Indexed(n @ (7 | 8)) => Some(n),
            _ => None,
        })
        .collect();
    greys.sort_unstable();
    greys.dedup();
    greys
}

/// A light/dark flip the terminal reports while the TUI is attached reaches
/// it live: the chrome recolours — the selection row's grey — and the TUI
/// asks the terminal's colours again and hands them to the daemon, which
/// tells a pane whose program set 2031.
#[test]
fn a_light_dark_flip_recolors_the_tui_and_reaches_a_2031_pane() {
    let dir = std::env::temp_dir().join(format!("tutti-tty-{}", std::process::id()));
    let session = "flip";
    let daemon = Daemon::start(&dir, session);
    let mut wire = Wire::connect(&dir.join("tutti").join(format!("{session}.sock")));
    match wire.request(&Request::WorkspaceNew { dir: dir.clone() }) {
        Response::WorkspaceCreated { .. } => {}
        other => panic!("{other:?}"),
    }
    // Each report the pane hears, a line apiece, its ESC shown as E.
    let script = "stty raw -echo; printf '\\033[?2031h'; echo ready; \
                  while :; do head -c 9 | tr '\\033' E; printf '\\r\\n'; done";
    let pane = match wire.request(&Request::PaneRun {
        tab: None,
        cmd: vec!["/bin/sh".into(), "-c".into(), script.into()],
        ephemeral: false,
        sandbox: None,
    }) {
        Response::PaneCreated { id } => id,
        other => panic!("{other:?}"),
    };
    wait(
        || wire.screen(pane).contains("ready").then_some(()),
        "the pane never started",
    );

    let mut spec = PtySpec::new(env!("CARGO_BIN_EXE_tutti"));
    spec.args = vec!["-s".into(), session.into(), "attach".into()];
    let config = dir.join("config");
    spec.env = vec![
        ("XDG_RUNTIME_DIR".into(), dir.display().to_string()),
        ("XDG_CONFIG_HOME".into(), config.display().to_string()),
        ("HOME".into(), dir.display().to_string()),
        ("TERM".into(), "xterm-256color".into()),
    ];
    spec.env_remove = [
        "EDITOR",
        "VISUAL",
        "TUTTI_PANE",
        "TUTTI_SESSION",
        "RTTY_PANE",
        "RTTY_SOCKET",
    ]
    .map(String::from)
    .to_vec();
    spec.colors = Some(colors(true));
    let host = PtyPane::spawn(spec, PaneSize::new(30, 120)).unwrap();

    // Attached: the TUI asked the terminal's colours and handed them on —
    // the first report the pane hears.
    wait(
        || wire.screen(pane).contains("E[?997;1n").then_some(()),
        "the TUI never reported the terminal's colours",
    );
    // Ctrl+H from the only pane steps into the sidebar, whose selected row
    // the chrome draws in the dark terminal's grey.
    host.write_input(b"\x08").unwrap();
    wait(
        || (selection_greys(&host) == [8]).then_some(()),
        "no dark selection row",
    );

    host.set_colors(colors(false));
    wait(
        || wire.screen(pane).contains("E[?997;2n").then_some(()),
        "the flip never reached the pane",
    );
    wait(
        || (selection_greys(&host) == [7]).then_some(()),
        "the TUI never recoloured",
    );

    // A resize, and a stop and a continue, ask the terminal nothing the
    // TUI's own reader would take first: it reads on, and hears the next
    // flip.
    host.resize(32, 110).unwrap();
    let pid = host.child_pid().unwrap().to_string();
    let stat = || {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let signal = |name: &str| {
        let sent = std::process::Command::new("kill")
            .args([name, &pid])
            .status()
            .unwrap();
        assert!(sent.success());
    };
    signal("-TSTP");
    wait(
        || stat().contains('T').then_some(()),
        "the TUI never stopped",
    );
    signal("-CONT");
    wait(
        || (!stat().contains('T')).then_some(()),
        "the TUI never went on",
    );
    host.set_colors(colors(true));
    wait(
        || (wire.screen(pane).matches("E[?997;1n").count() == 2).then_some(()),
        "the TUI stopped reading after a resize or a stop",
    );
    wait(
        || (selection_greys(&host) == [8]).then_some(()),
        "the TUI never recoloured back",
    );

    host.kill().unwrap();
    drop(daemon);
}
