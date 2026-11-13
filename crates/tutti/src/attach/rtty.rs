//! tutti inside rtty: the rtty pane the TUI runs in, its claim on rtty's
//! focus and resize keys, and a move past tutti's edge handed on to
//! `rtty cli` — so one set of keys moves through an editor's windows, tutti's
//! panes and rtty's panes alike.

use std::ffi::OsString;
use std::os::unix::fs::FileTypeExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tutti_core::{Frame as WireFrame, Toward};

use super::app::App;

/// The OSC 1337 user var that claims rtty's focus and resize keys for the
/// pane the TUI runs in (`pane`, base64), and the one that gives them back.
pub const CLAIM: &[u8] = b"\x1b]1337;SetUserVar=RTTY_KEYS=cGFuZQ==\x07";
/// Giving the keys back also unsets what `showing` set.
pub const RELEASE: &[u8] = b"\x1b]1337;SetUserVar=RTTY_KEYS=\x07\
    \x1b]1337;SetUserVar=TUTTI_TUI=\x07\x1b]1337;SetUserVar=TUTTI_TUI_HOST=\x07";

/// The user vars that say whose TUI runs in this rtty pane: `TUTTI_TUI`, the
/// session it shows, and `TUTTI_TUI_HOST`, the ssh `destination` of one on
/// another machine (empty for one on this). rtty reports a pane's user vars,
/// so what arranges rtty's windows from outside — a window manager — can
/// find the window that holds a session's TUI, and send it `tutti pane
/// focus`.
pub fn showing(session: &str, destination: Option<&str>) -> Vec<u8> {
    use base64::Engine;
    let var = |name: &str, value: &str| {
        let value = base64::engine::general_purpose::STANDARD.encode(value);
        format!("\x1b]1337;SetUserVar={name}={value}\x07")
    };
    let host = var("TUTTI_TUI_HOST", destination.unwrap_or(""));
    (var("TUTTI_TUI", session) + &host).into_bytes()
}

/// How long a `rtty cli` call gets before it reads as "did not move", as
/// rtty's own editor plugins give it.
const TIMEOUT: Duration = Duration::from_millis(500);

/// The rtty pane the TUI runs in, from its own environment — the client's,
/// never the daemon's, whose panes rtty's variables do not reach.
pub fn pane() -> Option<String> {
    detect(
        std::env::var("RTTY_PANE").ok(),
        std::env::var_os("RTTY_SOCKET"),
    )
}

/// `pane`, when `socket` names a socket — rtty's, there to answer.
/// (XTVERSION would not tell rtty apart: rtty and tutti's own panes both
/// answer as rtty-vt.)
fn detect(pane: Option<String>, socket: Option<OsString>) -> Option<String> {
    let pane = pane.filter(|pane| !pane.is_empty())?;
    let socket = std::fs::metadata(socket?).ok()?;
    socket.file_type().is_socket().then_some(pane)
}

/// A move handed on to rtty: focus its pane that way, or move its split
/// border that way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    Focus(Toward),
    Resize(Toward),
}

/// Run `program cli pane focus|resize --direction <toward> --from <pane>`:
/// whether rtty did it — exited 0 within `TIMEOUT`. A missing, slow or
/// refusing rtty, or its edge, is "no".
pub fn hand_off(program: &str, pane: &str, handoff: Handoff) -> bool {
    let (verb, toward) = match handoff {
        Handoff::Focus(toward) => ("focus", toward),
        Handoff::Resize(toward) => ("resize", toward),
    };
    let direction = toward.to_string();
    let args = [
        "cli",
        "pane",
        verb,
        "--direction",
        &direction,
        "--from",
        pane,
    ];
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// What tutti keeps while the TUI runs in an rtty pane: outside one there is
/// nothing to hand on to, and so no moves to hold.
pub(super) struct Rtty {
    /// The rtty pane the TUI runs in (`pane`).
    pane: String,
    /// Moves handed on to rtty, for the event loop to run (`take_rtty_jobs`).
    jobs: Vec<RttyJob>,
}

/// A move handed on to rtty (`App::take_rtty_jobs`), and the tab step tutti
/// takes should rtty not move (`App::rtty_done`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RttyJob {
    pub handoff: Handoff,
    fallback: Option<isize>,
}

impl App {
    /// The TUI runs in rtty pane `pane`: moves past tutti's edge go on to
    /// rtty from here on.
    pub fn in_rtty(&mut self, pane: Option<String>) {
        self.rtty = pane.map(|pane| Rtty {
            pane,
            jobs: Vec::new(),
        });
    }

    /// The moves handed on to rtty since the last call, for the event loop
    /// to run (`rtty::hand_off`), each with the rtty pane they start from.
    pub fn take_rtty_jobs(&mut self) -> Vec<(String, RttyJob)> {
        let Some(Rtty { pane, jobs }) = &mut self.rtty else {
            return Vec::new();
        };
        jobs.drain(..).map(|job| (pane.clone(), job)).collect()
    }

    /// rtty did `job`'s move, or not (`moved`): at rtty's own left or right
    /// edge too, the move falls back to tutti's own — the next tab that way.
    pub fn rtty_done(&mut self, job: RttyJob, moved: bool) -> Vec<WireFrame> {
        match job.fallback {
            Some(step) if !moved => self.switch_tab(step),
            _ => Vec::new(),
        }
    }

    /// Hand `handoff` on to rtty when the TUI runs in it — `fallback` the
    /// tab step to take should rtty not move — and say whether it did.
    pub(super) fn rtty_handoff(&mut self, handoff: Handoff, fallback: Option<isize>) -> bool {
        if let Some(rtty) = &mut self.rtty {
            rtty.jobs.push(RttyJob { handoff, fallback });
        }
        self.rtty.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::app::{Mode, control};
    use crate::attach::fixtures::{
        alt, attach_with, ctrl, leaf, shell, split, tab, view_two_tabs, workspace,
    };
    use crate::config::{Config, RESIZE_DELTA};
    use ratatui::layout::Rect;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tutti_core::{Direction, Event, PaneId, Request, TabId};

    /// A fresh scratch directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tutti-rtty-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in `rtty` that logs its argv and exits with `code`, after
    /// sleeping `secs`.
    fn stub(dir: &std::path::Path, code: i32, secs: f32) -> (String, PathBuf) {
        let (program, log) = (dir.join("rtty"), dir.join("argv"));
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nsleep {secs}\nexit {code}\n",
            log.display()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (program.display().to_string(), log)
    }

    /// The pane says whose TUI it holds in user vars rtty can report: the
    /// session, and the ssh destination of one on another machine. Giving
    /// the terminal back unsets both with the claim on the keys.
    #[test]
    fn the_pane_names_the_session_its_tui_shows() {
        let var = |name: &str, value: &str| format!("\x1b]1337;SetUserVar={name}={value}\x07");
        let local = var("TUTTI_TUI", "d29yaw==") + &var("TUTTI_TUI_HOST", "");
        assert_eq!(showing("work", None), local.into_bytes());
        let far = var("TUTTI_TUI", "d29yaw==") + &var("TUTTI_TUI_HOST", "bWVAYm94");
        assert_eq!(showing("work", Some("me@box")), far.into_bytes());
        let unset = ["RTTY_KEYS", "TUTTI_TUI", "TUTTI_TUI_HOST"].map(|name| var(name, ""));
        assert_eq!(RELEASE, unset.concat().as_bytes());
    }

    #[test]
    fn rtty_is_a_pane_and_a_socket_there() {
        let dir = scratch("detect");
        let socket = dir.join("rtty.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let file = dir.join("not-a-socket");
        std::fs::write(&file, "").unwrap();
        let found = |pane: Option<&str>, at: &std::path::Path| {
            detect(pane.map(String::from), Some(at.into()))
        };
        assert_eq!(found(Some("7"), &socket), Some("7".into()));
        assert_eq!(found(None, &socket), None, "no pane");
        assert_eq!(found(Some(""), &socket), None, "an empty pane");
        assert_eq!(found(Some("7"), &file), None, "no socket there");
        assert_eq!(found(Some("7"), &dir.join("gone")), None, "nothing there");
        assert_eq!(detect(Some("7".into()), None), None, "no socket named");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact command rtty is handed, and its answer: moved on exit 0,
    /// not on exit 1 (its own edge), nor when it takes too long or is not
    /// there.
    #[test]
    fn a_handoff_runs_rtty_cli_and_reads_its_exit() {
        let dir = scratch("handoff");
        let (moved, log) = stub(&dir, 0, 0.0);
        assert!(hand_off(&moved, "7", Handoff::Focus(Toward::Left)));
        assert!(hand_off(&moved, "7", Handoff::Resize(Toward::Down)));
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "cli pane focus --direction left --from 7\n\
             cli pane resize --direction down --from 7\n"
        );
        let edge = scratch("edge");
        assert!(!hand_off(
            &stub(&edge, 1, 0.0).0,
            "7",
            Handoff::Focus(Toward::Up)
        ));
        let slow = scratch("slow");
        let started = Instant::now();
        assert!(!hand_off(
            &stub(&slow, 0, 5.0).0,
            "7",
            Handoff::Focus(Toward::Up)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "given up on in time"
        );
        assert!(!hand_off(
            &dir.join("missing").display().to_string(),
            "7",
            Handoff::Focus(Toward::Up)
        ));
        for dir in [dir, edge, slow] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// What a test App has queued for rtty, from rtty pane 7.
    fn rtty_jobs(app: &mut App) -> Vec<RttyJob> {
        app.take_rtty_jobs()
            .into_iter()
            .map(|(pane, job)| {
                assert_eq!(pane, "7");
                job
            })
            .collect()
    }

    /// In rtty, a move past tutti's edge goes on to rtty's pane that way: up
    /// and down at once, left and right at the layout's edge. At rtty's own
    /// edge too, left and right fall back to tutti's tab wrap. Outside rtty
    /// nothing is handed on.
    #[test]
    fn in_rtty_a_move_past_every_edge_goes_on_to_rtty() {
        let focus = |toward, fallback| RttyJob {
            handoff: Handoff::Focus(toward),
            fallback,
        };
        let mut app = App::with_config(Config::parse("sidebar = \"off\"\n").unwrap());
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 80, 24));
        app.on_key(ctrl('j'));
        assert!(rtty_jobs(&mut app).is_empty(), "outside rtty");

        app.in_rtty(Some("7".into()));
        for (key, toward) in [('j', Toward::Down), ('k', Toward::Up)] {
            assert!(app.on_key(ctrl(key)).is_empty());
            assert_eq!(rtty_jobs(&mut app), [focus(toward, None)]);
        }
        assert!(app.on_key(ctrl('h')).is_empty(), "rtty first, not the tab");
        let left = focus(Toward::Left, Some(-1));
        assert_eq!(rtty_jobs(&mut app), [left]);
        assert!(app.rtty_done(left, true).is_empty(), "rtty moved");
        assert_eq!(app.active_tab, Some(TabId(1)));
        assert_eq!(
            app.rtty_done(left, false),
            vec![control(&Request::TabSelect { id: TabId(2) })],
            "at rtty's own edge too: tutti's tab wrap"
        );
        app.on_key(ctrl('l'));
        assert_eq!(rtty_jobs(&mut app), [focus(Toward::Right, Some(1))]);
    }

    /// tutti's own panels come before rtty: the left edge steps into the
    /// sidebar, and only past it does the move go on to rtty — as an
    /// editor's move handed back through the daemon (`FocusEdge`) does.
    #[test]
    fn in_rtty_the_sidebar_comes_before_rtty() {
        let mut app = App::new();
        attach_with(&mut app, view_two_tabs());
        app.sync_sizes(Rect::new(0, 0, 100, 24));
        app.in_rtty(Some("7".into()));
        let edge = |toward| {
            let event = Event::FocusEdge {
                pane: PaneId(1),
                toward,
            };
            WireFrame::Control(serde_json::to_vec(&event).unwrap())
        };
        assert!(app.handle_frame(edge(Toward::Left)).is_empty());
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(rtty_jobs(&mut app).is_empty());
        assert!(app.on_key(ctrl('h')).is_empty());
        assert_eq!(app.mode, Mode::Terminal);
        assert_eq!(
            rtty_jobs(&mut app),
            [RttyJob {
                handoff: Handoff::Focus(Toward::Left),
                fallback: Some(-1),
            }]
        );
        app.handle_frame(edge(Toward::Up));
        assert_eq!(
            rtty_jobs(&mut app),
            [RttyJob {
                handoff: Handoff::Focus(Toward::Up),
                fallback: None,
            }],
            "an editor's upper edge, through tutti, on to rtty"
        );
    }

    /// In rtty, a resize no split of tutti's takes moves rtty's split that
    /// way — from the direct keys, and from an editor's resize the daemon
    /// handed back (`ResizeEdge`); one a split takes stays tutti's.
    #[test]
    fn in_rtty_a_resize_with_no_split_of_tuttis_goes_on_to_rtty() {
        let resize = |toward| RttyJob {
            handoff: Handoff::Resize(toward),
            fallback: None,
        };
        let mut app = App::new();
        let split = split(Direction::Horizontal, leaf(1), leaf(2));
        attach_with(
            &mut app,
            vec![workspace(
                1,
                "w",
                None,
                vec![tab(1, "1", true, split, vec![shell(1), shell(2)])],
            )],
        );
        app.in_rtty(Some("7".into()));
        assert_eq!(
            app.on_key(alt('l')),
            vec![control(&Request::PaneResizeSplit {
                pane: PaneId(1),
                direction: Direction::Horizontal,
                delta: RESIZE_DELTA,
            })]
        );
        assert!(rtty_jobs(&mut app).is_empty(), "tutti's own split");
        assert!(app.on_key(alt('j')).is_empty());
        assert_eq!(rtty_jobs(&mut app), [resize(Toward::Down)]);
        let event = Event::ResizeEdge {
            pane: PaneId(1),
            toward: Toward::Up,
        };
        app.handle_frame(WireFrame::Control(serde_json::to_vec(&event).unwrap()));
        assert_eq!(rtty_jobs(&mut app), [resize(Toward::Up)]);
    }
}
