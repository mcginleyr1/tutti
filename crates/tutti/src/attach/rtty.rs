//! tutti inside rtty: the rtty pane the TUI runs in, its claim on rtty's
//! focus and resize keys, and a move past tutti's edge handed on to
//! `rtty cli` — so one set of keys moves through an editor's windows, tutti's
//! panes and rtty's panes alike.

use std::ffi::OsString;
use std::os::unix::fs::FileTypeExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tutti_core::Toward;

/// The OSC 1337 user var that claims rtty's focus and resize keys for the
/// pane the TUI runs in (`pane`, base64), and the one that gives them back.
pub const CLAIM: &[u8] = b"\x1b]1337;SetUserVar=RTTY_KEYS=cGFuZQ==\x07";
pub const RELEASE: &[u8] = b"\x1b]1337;SetUserVar=RTTY_KEYS=\x07";

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

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
}
