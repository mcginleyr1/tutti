//! End-to-end tests that drive the built `tutti` binary against a fake server:
//! a std `UnixListener` thread that decodes one Request frame, asserts its
//! shape, and replies with a canned Response frame. The real daemon is out of
//! scope here — the coordinator wires that up at merge.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;

use tutti_core::{Frame, PaneId, Request, Response, WorkspaceId, WorkspaceInfo};

fn unique_base(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // `/tmp` keeps the socket path within macOS's sun_path length limit.
    PathBuf::from("/tmp").join(format!("tt-{label}-{}-{nanos}", std::process::id()))
}

fn read_request(stream: &mut UnixStream) -> Request {
    let mut buf = Vec::new();
    loop {
        match Frame::decode(&buf).unwrap() {
            Some((Frame::Control(bytes), consumed)) => {
                buf.drain(..consumed);
                return serde_json::from_slice(&bytes).unwrap();
            }
            Some((_, consumed)) => {
                buf.drain(..consumed);
            }
            None => {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0, "client closed before sending a request");
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

fn run_scenario(
    label: &str,
    args: &[&str],
    assert_request: impl FnOnce(&Request) + Send + 'static,
    reply: Response,
) -> Output {
    let base = unique_base(label);
    let sock_dir = base.join("tutti");
    std::fs::create_dir_all(&sock_dir).unwrap();
    let listener = UnixListener::bind(sock_dir.join("tutti.sock")).unwrap();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        assert_request(&request);
        let frame = Frame::Control(serde_json::to_vec(&reply).unwrap());
        stream.write_all(&frame.encode()).unwrap();
        stream.flush().unwrap();
    });

    let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(args)
        .env("XDG_RUNTIME_DIR", &base)
        .output()
        .unwrap();

    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&base);
    output
}

#[test]
fn workspace_list_json_round_trips() {
    let expected = Response::Workspaces {
        workspaces: vec![WorkspaceInfo {
            id: WorkspaceId(1),
            name: "api".into(),
            dir: "/srv/api".into(),
        }],
    };
    let output = run_scenario(
        "wslist",
        &["workspace", "list", "--json"],
        |req| assert_eq!(*req, Request::WorkspaceList),
        expected.clone(),
    );

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: Response = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed, expected);
}

#[test]
fn pane_read_prints_plain_lines() {
    let output = run_scenario(
        "paneread",
        &["pane", "read", "3", "--lines", "2"],
        |req| {
            assert_eq!(
                *req,
                Request::PaneRead {
                    pane: PaneId(3),
                    lines: Some(2),
                    unwrapped: false,
                }
            );
        },
        Response::Content {
            lines: vec!["first".into(), "second".into()],
        },
    );

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "first\nsecond\n");
}

#[test]
fn error_response_exits_nonzero_on_stderr() {
    let output = run_scenario(
        "err",
        &["pane", "list"],
        |req| assert_eq!(*req, Request::PaneList),
        Response::Error {
            message: "no such pane".into(),
        },
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "stdout must stay empty on error");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no such pane"), "stderr was: {stderr}");
}
