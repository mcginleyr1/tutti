//! End-to-end tests that drive the built `tutti` binary against a fake server:
//! a std `UnixListener` thread that decodes one Request frame, asserts its
//! shape, and replies with a canned Response frame. The real daemon is out of
//! scope here — the coordinator wires that up at merge.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;

use tutti_core::{AgentHookEvent, Frame, PaneId, Request, Response, WorkspaceId, WorkspaceInfo};

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
    run_scenario_with(label, args, &[], "", assert_request, reply)
}

/// `run_scenario` with extra environment and `stdin` piped to the binary.
fn run_scenario_with(
    label: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &str,
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

    let mut child = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(args)
        .env("XDG_RUNTIME_DIR", &base)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&base);
    output
}

/// A recorded opencode event, piped to `agent-event opencode` inside a tutti
/// pane (`TUTTI_PANE`/`TUTTI_SESSION` set), reaches the daemon as the mapped
/// `AgentEvent` — the path tutti's opencode plugin drives.
#[test]
fn agent_event_opencode_forwards_a_recorded_permission_prompt() {
    let payload = r#"{"type":"permission.asked","properties":{"id":"per_1","sessionID":"ses_1",
        "permission":"bash","patterns":["rm -rf build"],"metadata":{},"always":[]}}"#;
    let output = run_scenario_with(
        "ocevent",
        &["agent-event", "opencode"],
        &[("TUTTI_PANE", "7"), ("TUTTI_SESSION", "tutti")],
        payload,
        |req| {
            assert_eq!(
                *req,
                Request::AgentEvent {
                    pane: PaneId(7),
                    event: AgentHookEvent::Blocked {
                        message: Some("bash".into()),
                    },
                    // A prompt may be a subagent's: it names no conversation.
                    conversation: None,
                }
            );
        },
        Response::Ok,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

/// A recorded Claude Code `UserPromptSubmit`, piped to `agent-event claude`
/// inside a tutti pane, reaches the daemon as a labelled prompt naming its
/// conversation — and prints nothing, as Claude hands this hook's stdout to
/// the model.
#[test]
fn agent_event_claude_forwards_a_prompt_with_its_conversation() {
    let payload = r#"{"session_id":"abc123","transcript_path":"/tmp/t.jsonl","cwd":"/tmp",
        "permission_mode":"default","hook_event_name":"UserPromptSubmit",
        "prompt":"fix the failing test\nit is in tests/server.rs"}"#;
    let output = run_scenario_with(
        "ccprompt",
        &["agent-event", "claude"],
        &[("TUTTI_PANE", "7"), ("TUTTI_SESSION", "tutti")],
        payload,
        |req| {
            assert_eq!(
                *req,
                Request::AgentEvent {
                    pane: PaneId(7),
                    event: AgentHookEvent::Prompt {
                        text: Some("fix the failing test".into()),
                    },
                    conversation: Some("abc123".into()),
                }
            );
        },
        Response::Ok,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

/// `agent-event opencode` never fails: outside a tutti pane, and on an
/// irrelevant event, it exits 0 having sent nothing (no daemon is listening).
#[test]
fn agent_event_opencode_is_a_silent_no_op_when_there_is_nothing_to_send() {
    for (envs, payload) in [
        (
            vec![],
            r#"{"type":"session.status","properties":{"status":{"type":"busy"}}}"#,
        ),
        (
            vec![("TUTTI_PANE", "7")],
            r#"{"type":"message.part.updated","properties":{}}"#,
        ),
        (vec![("TUTTI_PANE", "7")], "not json"),
    ] {
        let base = unique_base("ocnoop");
        let mut child = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(["agent-event", "opencode"])
            .env("XDG_RUNTIME_DIR", &base)
            .env_remove("TUTTI_PANE")
            .envs(envs)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "for {payload}");
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
}

/// `hooks opencode --install` writes the plugin into opencode's global plugin
/// dir under `XDG_CONFIG_HOME`, is idempotent, replaces an older tutti plugin
/// behind a `.bak`, and refuses to touch a foreign file of the same name.
#[test]
fn hooks_opencode_install_writes_backs_up_and_refuses_foreign_files() {
    let config = unique_base("ocinstall");
    let plugin = config.join("opencode/plugins/tutti.js");
    let install = || {
        Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(["hooks", "opencode", "--install", "--yes"])
            .env("XDG_CONFIG_HOME", &config)
            .output()
            .unwrap()
    };
    let printed = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(["hooks", "opencode", "--raw"])
        .output()
        .unwrap();
    let ours = String::from_utf8(printed.stdout).unwrap();
    assert!(ours.starts_with("// tutti opencode plugin"));

    assert!(install().status.success());
    assert_eq!(std::fs::read_to_string(&plugin).unwrap(), ours);
    let again = install();
    assert!(
        String::from_utf8(again.stdout)
            .unwrap()
            .contains("already installed")
    );

    std::fs::write(&plugin, "// tutti opencode plugin (older)\n").unwrap();
    assert!(install().status.success());
    assert_eq!(std::fs::read_to_string(&plugin).unwrap(), ours);
    assert_eq!(
        std::fs::read_to_string(plugin.with_file_name("tutti.js.bak")).unwrap(),
        "// tutti opencode plugin (older)\n"
    );

    std::fs::write(&plugin, "export const Mine = async () => ({})\n").unwrap();
    let refused = install();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8(refused.stderr)
            .unwrap()
            .contains("not tutti's plugin")
    );
    assert_eq!(
        std::fs::read_to_string(&plugin).unwrap(),
        "export const Mine = async () => ({})\n",
        "a foreign plugin is never overwritten"
    );
    let _ = std::fs::remove_dir_all(&config);
}

/// `pane run --sandbox <profile>` sends the profile with the command; a
/// profile the config does not name fails before anything reaches a daemon,
/// as does `sandbox run` with one.
#[test]
fn pane_run_sandbox_sends_a_configured_profile_and_refuses_an_unknown_one() {
    let config = unique_base("sbcfg");
    std::fs::create_dir_all(config.join("tutti")).unwrap();
    std::fs::write(
        config.join("tutti/config.toml"),
        "[[sandboxes]]\nname = \"orb\"\nimage = \"i\"\n",
    )
    .unwrap();
    let xdg = config.to_str().unwrap().to_string();
    let output = run_scenario_with(
        "sbrun",
        &["pane", "run", "--sandbox", "orb", "--", "claude"],
        &[("XDG_CONFIG_HOME", &xdg)],
        "",
        |req| {
            assert_eq!(
                *req,
                Request::PaneRun {
                    tab: None,
                    cmd: vec!["claude".into()],
                    ephemeral: false,
                    sandbox: Some("orb".into()),
                }
            );
        },
        Response::PaneCreated { id: PaneId(9) },
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    for args in [
        &["pane", "run", "--sandbox", "gke", "--", "claude"][..],
        &["sandbox", "run", "gke", "--", "claude"][..],
    ] {
        let refused = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(args)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_RUNTIME_DIR", unique_base("sbnone"))
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8(refused.stderr).unwrap();
        assert!(
            stderr.contains("no sandbox profile \"gke\"") && stderr.contains("configured: orb"),
            "{args:?}: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&config);
}

/// `pane wait` prints the state that ended the wait, and exits 124 — with a
/// note on stderr — when the daemon answers that it timed out.
#[test]
fn pane_wait_prints_the_state_and_exits_124_on_a_timeout() {
    use tutti_core::{PaneInfo, WaitFor};
    let pane = |state: &str| -> PaneInfo {
        serde_json::from_str(&format!(
            r#"{{"id":3,"title":"claude","agent":"claude","state":"{state}","exited":null}}"#
        ))
        .unwrap()
    };
    let output = run_scenario(
        "wait",
        &["pane", "wait", "3", "--until", "blocked,done"],
        |req| {
            assert_eq!(
                *req,
                Request::PaneWait {
                    pane: PaneId(3),
                    until: vec![WaitFor::Blocked, WaitFor::Done],
                    next: false,
                    timeout_ms: None,
                }
            );
        },
        Response::Waited {
            pane: pane("blocked"),
            timed_out: false,
        },
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "blocked\n");

    let output = run_scenario(
        "waitout",
        &["pane", "wait", "3", "--timeout", "5"],
        |req| {
            assert!(
                matches!(req, Request::PaneWait { timeout_ms: Some(ms), .. } if *ms <= 5000),
                "the deadline rides along: {req:?}"
            );
        },
        Response::Waited {
            pane: pane("working"),
            timed_out: true,
        },
    );
    assert_eq!(output.status.code(), Some(124));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("timed out waiting for pane 3 (state: working)"),
        "{stderr}"
    );
}

/// `run` prints just the new pane's id, so `p=$(tutti run -- …)` captures it.
#[test]
fn run_prints_the_bare_pane_id() {
    let output = run_scenario(
        "run",
        &["run", "--", "claude", "fix the tests"],
        |req| {
            assert_eq!(
                *req,
                Request::Run {
                    workspace: None,
                    dir: None,
                    cmd: vec!["claude".into(), "fix the tests".into()],
                    sandbox: None,
                    title: None,
                }
            );
        },
        Response::Ran {
            workspace: WorkspaceId(1),
            tab: tutti_core::TabId(2),
            pane: PaneId(7),
        },
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "7\n");
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
