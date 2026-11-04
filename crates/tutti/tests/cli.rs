//! End-to-end tests that drive the built `tutti` binary against a fake server:
//! a std `UnixListener` thread that decodes one Request frame, asserts its
//! shape, and replies with a canned Response frame. The real daemon is out of
//! scope here — the coordinator wires that up at merge.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;

use tutti_core::{
    AgentHookEvent, AgentState, ApprovalDecision, Event, Frame, Note, NoteKind, PaneId, Request,
    Response, Tagged, Toward, WatchPane, WorkspaceId, WorkspaceInfo, WorkspaceView,
};

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

    // A pane's own session and id would otherwise steer the binary off the
    // fake server's `tutti` socket, and into its requests, when the suite runs
    // inside tutti.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(args)
        .env("XDG_RUNTIME_DIR", &base)
        .env_remove("TUTTI_SESSION")
        .env_remove("TUTTI_PANE")
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
    let payload = r#"{"session_id":"abc123","prompt_id":"p1","transcript_path":"/tmp/t.jsonl",
        "cwd":"/tmp","permission_mode":"default","hook_event_name":"UserPromptSubmit",
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
                        id: Some("p1".into()),
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

/// A recorded Codex `Stop` (shaped as codex-cli 0.166.0 sent it, ids and
/// paths made up), piped to `agent-event codex`, reaches the daemon as the
/// turn's end carrying its final message — and prints nothing.
#[test]
fn agent_event_codex_forwards_a_stop_with_its_reply() {
    let payload = r#"{"session_id":"01a1a704-0000-7000-8000-000000000001",
        "turn_id":"01a1a708-0000-7000-8000-000000000002","transcript_path":"/tmp/rollout-1.jsonl",
        "cwd":"/tmp","hook_event_name":"Stop","model":"a-model","permission_mode":"default",
        "stop_hook_active":false,"last_assistant_message":"pong\nping"}"#;
    let output = run_scenario_with(
        "cxstop",
        &["agent-event", "codex"],
        &[("TUTTI_PANE", "7"), ("TUTTI_SESSION", "tutti")],
        payload,
        |req| {
            assert_eq!(
                *req,
                Request::AgentEvent {
                    pane: PaneId(7),
                    event: AgentHookEvent::Done {
                        reply: Some("pong\nping".into()),
                    },
                    conversation: Some("01a1a704-0000-7000-8000-000000000001".into()),
                }
            );
        },
        Response::Ok,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

/// `pane read --last-reply` prints the agent's reply alone on stdout, a cut
/// noted on stderr.
#[test]
fn pane_read_last_reply_prints_the_reply_alone() {
    let output = run_scenario(
        "lastreply",
        &["pane", "read", "3", "--last-reply"],
        |req| {
            assert_eq!(
                *req,
                Request::PaneRead {
                    pane: PaneId(3),
                    lines: None,
                    unwrapped: false,
                    since: None,
                    last_output: false,
                    last_reply: true,
                }
            );
        },
        Response::LastReply {
            text: "pong\nping\n".into(),
            truncated: true,
        },
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "pong\nping\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("this is its beginning"));
}

/// Read one request frame, with the `req` it is tagged with.
fn read_tagged(stream: &mut UnixStream) -> (Option<u64>, Request) {
    let mut buf = Vec::new();
    loop {
        if let Some((Frame::Control(bytes), _)) = Frame::decode(&buf).unwrap() {
            let tagged: Tagged<Request> = serde_json::from_slice(&bytes).unwrap();
            return (tagged.req, tagged.body);
        }
        let mut byte = [0u8; 1];
        assert_eq!(stream.read(&mut byte).unwrap(), 1, "client closed early");
        buf.push(byte[0]);
    }
}

fn write_tagged(stream: &mut UnixStream, req: Option<u64>, body: Response) {
    let json = serde_json::to_vec(&Tagged { req, body }).unwrap();
    stream.write_all(&Frame::Control(json).encode()).unwrap();
}

/// A pane 7 in `state`, as `pane list` and a wait's answer carry it.
fn pane_in(state: &str, approval: &str) -> tutti_core::PaneInfo {
    serde_json::from_str(&format!(
        r#"{{"id":7,"title":"codex","agent":null,"state":"{state}","exited":null{approval}}}"#
    ))
    .unwrap()
}

/// Run `tutti ask 7 <message> --timeout 60` against a fake daemon whose pane
/// 7 is `listed`; the ask connection's wait answers with `waited`, and a
/// read of the last reply with `reply`. Hands back the output and every
/// request the ask connection made, with the gap between typing and enter.
fn ask_scenario(
    label: &str,
    listed: tutti_core::PaneInfo,
    waited: tutti_core::PaneInfo,
    reply: Response,
) -> (Output, Vec<Request>, std::time::Duration) {
    let base = unique_base(label);
    let sock_dir = base.join("tutti");
    std::fs::create_dir_all(&sock_dir).unwrap();
    let listener = UnixListener::bind(sock_dir.join("tutti.sock")).unwrap();
    let mid_turn = listed.state == AgentState::Working;
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        assert_eq!(read_request(&mut first), Request::PaneList);
        let panes = Response::Panes {
            panes: vec![listed],
        };
        first
            .write_all(&Frame::Control(serde_json::to_vec(&panes).unwrap()).encode())
            .unwrap();
        if mid_turn {
            return (Vec::new(), std::time::Duration::ZERO);
        }
        let (mut ask, _) = listener.accept().unwrap();
        let (wait_req, wait) = read_tagged(&mut ask);
        let (typed_req, typed) = read_tagged(&mut ask);
        let at = std::time::Instant::now();
        let (enter_req, enter) = read_tagged(&mut ask);
        let gap = at.elapsed();
        write_tagged(&mut ask, typed_req, Response::Ok);
        write_tagged(&mut ask, enter_req, Response::Ok);
        let blocked = waited.state == AgentState::Blocked;
        let waited = Response::Waited {
            pane: Box::new(waited),
            timed_out: false,
        };
        write_tagged(&mut ask, wait_req, waited);
        let mut asked = vec![wait, typed, enter];
        if !blocked {
            let (read_req, read) = read_tagged(&mut ask);
            write_tagged(&mut ask, read_req, reply);
            asked.push(read);
        }
        (asked, gap)
    });
    let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(["ask", "7", "reply with the word pong", "--timeout", "60"])
        .env("XDG_RUNTIME_DIR", &base)
        .env_remove("TUTTI_SESSION")
        .env_remove("TUTTI_PANE")
        .output()
        .unwrap();
    let (asked, gap) = server.join().unwrap();
    let _ = std::fs::remove_dir_all(&base);
    (output, asked, gap)
}

/// `ask` waits for the pane's next turn end on the connection it types on —
/// the wait first, so it is in place before the keys land — types the
/// message, presses enter a beat later, and prints the reply the turn left,
/// alone on stdout.
#[test]
fn ask_types_waits_and_prints_the_reply() {
    let (output, asked, gap) = ask_scenario(
        "ask",
        pane_in("done", ""),
        pane_in("done", ""),
        Response::LastReply {
            text: "pong".into(),
            truncated: false,
        },
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "pong\n");
    let pane = PaneId(7);
    assert_eq!(
        asked,
        [
            Request::PaneWait {
                pane,
                until: vec![tutti_core::WaitFor::Done, tutti_core::WaitFor::Blocked],
                next: true,
                timeout_ms: Some(60_000),
            },
            Request::PaneSend {
                pane,
                text: Some("reply with the word pong".into()),
                keys: None,
                expect: None,
            },
            Request::PaneSend {
                pane,
                text: None,
                keys: Some("enter".into()),
                expect: None,
            },
            Request::PaneRead {
                pane,
                lines: None,
                unwrapped: false,
                since: None,
                last_output: false,
                last_reply: true,
            },
        ]
    );
    assert!(
        gap >= std::time::Duration::from_millis(100),
        "enter apart from the text: {gap:?}"
    );
}

/// An agent that blocks instead of answering exits 3, naming what it asks
/// and how to go on; a working pane is refused before anything is typed.
#[test]
fn ask_exits_3_on_a_block_and_refuses_a_working_pane() {
    let held = r#","approval":{"id":4,"tool":"Bash","summary":"rm -rf build"}"#;
    let (output, asked, _) = ask_scenario(
        "askblock",
        pane_in("idle", ""),
        pane_in("blocked", held),
        Response::Ok,
    );
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("blocked, waiting on an answer (Bash · rm -rf build)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("tutti pane read 7 --last-reply"),
        "{stderr}"
    );
    assert_eq!(asked.len(), 3, "no reply is read: {asked:?}");

    let (output, asked, _) = ask_scenario(
        "askbusy",
        pane_in("working", ""),
        pane_in("done", ""),
        Response::Ok,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("pane 7 is working: wait for it"));
    assert!(asked.is_empty(), "nothing typed into a running turn");
}

/// A Claude Code `PermissionRequest`, as the hooks reference records one.
const PERMISSION_REQUEST: &str = r#"{"session_id":"abc123","transcript_path":"/tmp/t.jsonl",
    "cwd":"/srv/api","permission_mode":"default","hook_event_name":"PermissionRequest",
    "tool_name":"Bash","tool_input":{"command":"rm -rf build","description":"Clean"},
    "permission_suggestions":[]}"#;

/// A fresh config dir holding `config`, for `XDG_CONFIG_HOME`.
fn config_dir(label: &str, config: &str) -> PathBuf {
    let dir = unique_base(label);
    std::fs::create_dir_all(dir.join("tutti")).unwrap();
    std::fs::write(dir.join("tutti/config.toml"), config).unwrap();
    dir
}

/// Pipe `PERMISSION_REQUEST` to `agent-event claude` in pane 7 under
/// `config`, the fake daemon checking the request and answering `reply`.
fn permission_request(
    label: &str,
    config: &str,
    assert_request: impl FnOnce(&Request) + Send + 'static,
    reply: Response,
) -> Output {
    let dir = config_dir(label, config);
    let xdg = dir.to_str().unwrap().to_string();
    let output = run_scenario_with(
        label,
        &["agent-event", "claude"],
        &[
            ("TUTTI_PANE", "7"),
            ("TUTTI_SESSION", "tutti"),
            ("XDG_CONFIG_HOME", &xdg),
        ],
        PERMISSION_REQUEST,
        assert_request,
        reply,
    );
    let _ = std::fs::remove_dir_all(&dir);
    output
}

/// By default a permission request is only watched: it reaches the daemon as
/// a `Blocked` naming the tool and its input, and the hook prints nothing, so
/// Claude shows its own dialog.
#[test]
fn agent_event_claude_watches_a_permission_request_by_default() {
    let output = permission_request(
        "ccwatch",
        "",
        |req| {
            assert_eq!(
                *req,
                Request::AgentEvent {
                    pane: PaneId(7),
                    event: AgentHookEvent::Blocked {
                        message: Some("Bash · rm -rf build".into()),
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

/// Under `approvals = "hold"` the hook holds the request on the daemon and
/// prints its answer as Claude's decision object — allow, or deny with the
/// reason. A pass prints nothing, so Claude's own dialog appears. Every way,
/// the hook exits 0.
#[test]
fn agent_event_claude_holds_a_permission_request_for_its_answer() {
    let held = |label: &str, decision: ApprovalDecision| {
        let output = permission_request(
            label,
            "approvals = \"hold\"\n",
            |req| {
                assert_eq!(
                    *req,
                    Request::ApprovalHold {
                        pane: PaneId(7),
                        conversation: Some("abc123".into()),
                        tool: "Bash".into(),
                        summary: "rm -rf build".into(),
                    }
                );
            },
            Response::Approval { decision },
        );
        assert!(output.status.success(), "{label}");
        output.stdout
    };
    let decision =
        |stdout: Vec<u8>| -> serde_json::Value { serde_json::from_slice(&stdout).unwrap() };
    let output = |decision: serde_json::Value| {
        serde_json::json!({
            "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
        })
    };
    assert_eq!(
        decision(held("holdallow", ApprovalDecision::Allow)),
        output(serde_json::json!({ "behavior": "allow" }))
    );
    assert_eq!(
        decision(held(
            "holddeny",
            ApprovalDecision::Deny {
                message: Some("not on main".into())
            }
        )),
        output(serde_json::json!({ "behavior": "deny", "message": "not on main" }))
    );
    assert!(held("holdpass", ApprovalDecision::Pass).is_empty());
}

/// Under `approvals = "hold"` an opencode `permission.asked` holds on the
/// daemon too, naming its permission and patterns, and the answer prints as
/// the reply body tutti's plugin posts to opencode; a pass prints nothing,
/// leaving it to opencode's own dialog.
#[test]
fn agent_event_opencode_holds_a_permission_request_for_its_answer() {
    let asked = r#"{"type":"permission.asked","properties":{"id":"per_1","sessionID":"ses_1",
        "permission":"bash","patterns":["rm -rf build"],"metadata":{},"always":[]}}"#;
    let held = |label: &str, decision: ApprovalDecision| {
        let dir = config_dir(label, "approvals = \"hold\"\n");
        let xdg = dir.to_str().unwrap().to_string();
        let output = run_scenario_with(
            label,
            &["agent-event", "opencode"],
            &[
                ("TUTTI_PANE", "7"),
                ("TUTTI_SESSION", "tutti"),
                ("XDG_CONFIG_HOME", &xdg),
            ],
            asked,
            |req| {
                assert_eq!(
                    *req,
                    Request::ApprovalHold {
                        pane: PaneId(7),
                        conversation: None,
                        tool: "bash".into(),
                        summary: "rm -rf build".into(),
                    }
                );
            },
            Response::Approval { decision },
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(output.status.success(), "{label}");
        output.stdout
    };
    let reply = |stdout: Vec<u8>| -> serde_json::Value { serde_json::from_slice(&stdout).unwrap() };
    assert_eq!(
        reply(held("ocallow", ApprovalDecision::Allow)),
        serde_json::json!({ "reply": "once" })
    );
    assert_eq!(
        reply(held(
            "ocdeny",
            ApprovalDecision::Deny {
                message: Some("not on main".into())
            }
        )),
        serde_json::json!({ "reply": "reject", "message": "not on main" })
    );
    assert!(held("ocpass", ApprovalDecision::Pass).is_empty());
}

/// A hold nobody answers gives up after `approval_timeout_secs`: the hook
/// prints nothing and exits 0, and Claude's own dialog appears.
#[test]
fn agent_event_claude_passes_an_unanswered_hold_on_after_its_timeout() {
    let config = config_dir(
        "holdcfg",
        "approvals = \"hold\"\napproval_timeout_secs = 1\n",
    );
    let base = unique_base("holdwait");
    std::fs::create_dir_all(base.join("tutti")).unwrap();
    let listener = UnixListener::bind(base.join("tutti/tutti.sock")).unwrap();
    // A daemon that takes the hold and never answers it: it waits out the
    // hook hanging up.
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            read_request(&mut stream),
            Request::ApprovalHold { .. }
        ));
        let _ = stream.read_to_end(&mut Vec::new());
    });
    let started = std::time::Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(["agent-event", "claude"])
        .env("XDG_RUNTIME_DIR", &base)
        .env("XDG_CONFIG_HOME", &config)
        .env("TUTTI_PANE", "7")
        .env("TUTTI_SESSION", "tutti")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PERMISSION_REQUEST.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "Claude's own dialog appears");
    let waited = started.elapsed();
    assert!(
        waited >= std::time::Duration::from_secs(1) && waited < std::time::Duration::from_secs(5),
        "{waited:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("did not answer within 1s"), "{stderr}");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&config);
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

/// A wedged daemon — it takes the connection but never answers — cannot stall
/// a Claude hook: `agent-event` gives up at its deadline, still exiting 0 and
/// printing nothing on stdout.
#[test]
fn agent_event_gives_up_on_a_wedged_daemon() {
    let base = unique_base("wedged");
    let sock_dir = base.join("tutti");
    std::fs::create_dir_all(&sock_dir).unwrap();
    let listener = UnixListener::bind(sock_dir.join("tutti.sock")).unwrap();
    let daemon = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Take everything until the client hangs up; answer nothing.
        let _ = stream.read_to_end(&mut Vec::new());
    });

    let started = std::time::Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(["agent-event", "claude"])
        .env("XDG_RUNTIME_DIR", &base)
        .env("TUTTI_PANE", "7")
        .env("TUTTI_SESSION", "tutti")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"abc123","hook_event_name":"Stop"}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();

    daemon.join().unwrap();
    let _ = std::fs::remove_dir_all(&base);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the hook waited {elapsed:?}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not answer"));
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

/// `hooks claude --tty` prints the OSC 7501 hooks for a sandbox image —
/// `--raw` puts the JSON alone on stdout — and opencode has no such form.
#[test]
fn hooks_claude_tty_prints_the_osc_hooks() {
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(args)
            .output()
            .unwrap()
    };
    let raw = run(&["hooks", "claude", "--tty", "--raw"]);
    assert!(raw.status.success() && raw.stderr.is_empty());
    let printed: serde_json::Value = serde_json::from_slice(&raw.stdout).unwrap();
    assert_eq!(printed, tutti::hooks::claude_tty_hooks_json());
    let noted = run(&["hooks", "claude", "--tty"]);
    assert_eq!(noted.stdout, raw.stdout);
    assert!(
        String::from_utf8(noted.stderr)
            .unwrap()
            .contains("/dev/tty")
    );
    assert_eq!(run(&["hooks", "opencode", "--tty"]).status.code(), Some(1));
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
        &[
            "sandbox",
            "run",
            "gke",
            "--name",
            "tutti-p1-a",
            "--",
            "claude",
        ][..],
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

/// `issue list` runs a `kind = "command"` source's script in the current
/// directory and prints what it lists — a table, or the issues as JSON — and
/// a source that fails is named on stderr, failing the exit without hiding
/// the others. No daemon is involved.
#[test]
fn issue_list_prints_what_a_command_source_lists() {
    use std::os::unix::fs::PermissionsExt;
    let base = unique_base("issues");
    let project = base.join("api");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(base.join("tutti")).unwrap();
    let script = base.join("jira-issues");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '[{\"id\":\"PROJ-7\",\"title\":\"Fix the login\",\
         \"url\":\"https://jira.example/PROJ-7\",\"labels\":[\"p1\"]},\
         {\"id\":8,\"title\":\"Listed from %s\"}]' \"$(basename \"$(pwd)\")\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        base.join("tutti/config.toml"),
        format!(
            "[[issues]]\nname = \"jira\"\nkind = \"command\"\ncommand = [\"{}\"]\n\n\
             [[issues]]\nname = \"broken\"\nkind = \"command\"\n\
             command = [\"/bin/sh\", \"-c\", \"echo offline >&2; exit 1\"]\n",
            script.display()
        ),
    )
    .unwrap();
    let issue_list = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(args)
            .env("XDG_CONFIG_HOME", &base)
            .current_dir(&project)
            .output()
            .unwrap()
    };

    let jira = issue_list(&["issue", "list", "--source", "jira"]);
    assert!(
        jira.status.success(),
        "{}",
        String::from_utf8_lossy(&jira.stderr)
    );
    assert_eq!(
        String::from_utf8(jira.stdout).unwrap(),
        "SOURCE  ID      TITLE            LABELS  URL\n\
         jira    PROJ-7  Fix the login    p1      https://jira.example/PROJ-7\n\
         jira    8       Listed from api  -       -\n",
        "the script ran in the current directory, `api`"
    );

    let json = issue_list(&["--json", "issue", "list", "--source", "jira"]);
    let listed: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(
        listed[0],
        serde_json::json!({
            "source": "jira", "id": "PROJ-7", "title": "Fix the login", "body": "",
            "url": "https://jira.example/PROJ-7", "labels": ["p1"],
        })
    );

    let all = issue_list(&["issue", "list"]);
    assert_eq!(all.status.code(), Some(1), "a source failed");
    assert_eq!(
        String::from_utf8(all.stderr).unwrap(),
        "error: broken: /bin/sh: offline\n"
    );
    assert!(
        String::from_utf8(all.stdout)
            .unwrap()
            .contains("jira    PROJ-7"),
        "the other source still lists"
    );

    let unknown = issue_list(&["issue", "list", "--source", "gitlab"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        String::from_utf8(unknown.stderr)
            .unwrap()
            .contains("no issue source \"gitlab\""),
        "an unknown source is named"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A fake daemon on `listener` for one connection: each of its first `n`
/// requests gets `answer`'s reply. Hands back the requests.
fn serve_requests(
    listener: UnixListener,
    n: usize,
    answer: impl Fn(&Request) -> Response + Send + 'static,
) -> thread::JoinHandle<Vec<Request>> {
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        (0..n)
            .map(|_| {
                let request = read_request(&mut stream);
                let reply = Frame::Control(serde_json::to_vec(&answer(&request)).unwrap());
                stream.write_all(&reply.encode()).unwrap();
                request
            })
            .collect()
    })
}

/// What a fake daemon answers an `issue deploy`: project 1 is `api` at
/// `dir`, a workspace fork makes workspace 2, and the run lands in pane 7,
/// whose sandbox is `claude-7-x`.
fn answer_deploy(dir: PathBuf) -> impl Fn(&Request) -> Response + Send + 'static {
    move |request| match request {
        Request::WorkspaceList => Response::Workspaces {
            workspaces: vec![WorkspaceInfo {
                id: WorkspaceId(1),
                name: "api".into(),
                dir: dir.clone(),
                parent: None,
            }],
        },
        Request::WorkspaceFork { .. } => Response::WorkspaceCreated { id: WorkspaceId(2) },
        Request::Run { workspace, .. } => Response::Ran {
            workspace: workspace.unwrap(),
            tab: tutti_core::TabId(1),
            pane: PaneId(7),
        },
        Request::PaneList => Response::Panes {
            panes: vec![
                serde_json::from_str(
                    r#"{"id":7,"title":"claude","agent":null,"state":"unknown","exited":null,
                        "sandbox_name":"claude-7-x"}"#,
                )
                .unwrap(),
            ],
        },
        other => panic!("a deploy asked {other:?}"),
    }
}

/// A deploy writes back to its tracker as far as its source opts in, through
/// the same tools a fetch uses, in the project's directory: on GitHub `gh`
/// comments where the agent runs — a sandbox deploy names the branch it
/// pushes to — then assigns and labels the issue; on Linear `curl` comments
/// and moves the issue to the named state, the key only ever on its stdin. A
/// source asking for nothing writes nothing, and a refused write-back exits 1
/// after the agent is running.
#[test]
fn issue_deploy_writes_back_through_stub_gh_and_curl() {
    use std::os::unix::fs::PermissionsExt;
    let base = unique_base("writeback");
    let project = base.join("api");
    let bin = base.join("bin");
    let log = base.join("log");
    for dir in [&project.join(".jj"), &bin, &log, &base.join("tutti")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let gh = format!(
        "#!/bin/sh\n{{ basename \"$(pwd)\"; printf '%s\\n' \"$@\" --; }} >> {log}/gh\n\
         [ \"$2\" = view ] && printf '{{\"number\":123,\"title\":\"Fix the login\",\
         \"body\":\"It loops.\",\"url\":\"https://github.com/acme/api/issues/123\"}}'\n\
         exit 0\n",
        log = log.display()
    );
    let curl = format!(
        "#!/bin/sh\ninput=$(cat)\n\
         {{ printf '%s\\n' \"$@\" --; }} >> {log}/curl.args\n\
         printf '%s\\n--\\n' \"$input\" >> {log}/curl.stdin\n\
         case \"$input\" in\n\
         *commentCreate*) printf '{{\"data\":{{\"commentCreate\":{{\"success\":true}}}}}}' ;;\n\
         *issueUpdate*) printf '{{\"data\":{{\"issueUpdate\":{{\"success\":true}}}}}}' ;;\n\
         *states*) printf '{{\"data\":{{\"issue\":{{\"id\":\"uuid-42\",\"team\":{{\"states\":\
         {{\"nodes\":[{{\"id\":\"st-1\",\"name\":\"Todo\"}},{{\"id\":\"st-2\",\"name\":\"In Progress\"}}]}}}}}}}}}}' ;;\n\
         *) printf '{{\"data\":{{\"issue\":{{\"identifier\":\"ENG-42\",\"title\":\"Fix the login\",\
         \"description\":null,\"url\":\"https://linear.app/acme/issue/ENG-42\",\"labels\":{{\"nodes\":[]}}}}}}}}' ;;\n\
         esac\n",
        log = log.display()
    );
    for (name, script) in [("gh", gh), ("curl", curl)] {
        std::fs::write(bin.join(name), script).unwrap();
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        base.join("tutti/config.toml"),
        "[[issues]]\nname = \"gh\"\nkind = \"github\"\ncomment = true\nassign = \"@me\"\n\
         label = \"agent\"\n\n\
         [[issues]]\nname = \"quiet\"\nkind = \"github\"\n\n\
         [[issues]]\nname = \"linear\"\nkind = \"linear\"\napi_key_env = \"TUTTI_TEST_KEY\"\n\
         comment = true\nstate = \"In Progress\"\n\n\
         [[issues]]\nname = \"shipped\"\nkind = \"linear\"\napi_key_env = \"TUTTI_TEST_KEY\"\n\
         state = \"Shipped\"\n\n\
         [[sandboxes]]\nname = \"orb\"\nimage = \"agent:1\"\n",
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let deploy = |args: &[&str], n: usize| {
        let listener = UnixListener::bind(base.join("tutti/tutti.sock")).unwrap();
        let server = serve_requests(listener, n, answer_deploy(project.clone()));
        let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(["issue", "deploy"])
            .args(args)
            .args(["--project", "1"])
            .env("XDG_RUNTIME_DIR", &base)
            .env("XDG_CONFIG_HOME", &base)
            .env("PATH", &path)
            .env("TUTTI_TEST_KEY", "lin_api_secret")
            .env_remove("TUTTI_SESSION")
            .env_remove("TUTTI_PANE")
            .output()
            .unwrap();
        std::fs::remove_file(base.join("tutti/tutti.sock")).unwrap();
        (output, server.join().unwrap())
    };
    let read = |name: &str| std::fs::read_to_string(log.join(name)).unwrap_or_default();

    let (output, requests) = deploy(&["gh", "123", "--sandbox", "orb"], 4);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests[3], Request::PaneList, "for the sandbox's branch");
    assert_eq!(
        read("gh"),
        "api\nissue\nview\n123\n--json\nnumber,title,body,url,labels\n--\n\
         api\nissue\ncomment\n123\n--body\n\
         tutti deployed claude on this issue: workspace `gh-123` of project `api`, \
         pane 7 of session `tutti`, pushing to branch `tutti/claude-7-x`.\n--\n\
         api\nissue\nedit\n123\n--add-assignee\n@me\n--add-label\nagent\n--\n"
    );

    std::fs::remove_file(log.join("gh")).unwrap();
    let (output, requests) = deploy(&["quiet", "123"], 3);
    assert!(output.status.success());
    assert!(matches!(requests[2], Request::Run { .. }));
    assert_eq!(
        read("gh"),
        "api\nissue\nview\n123\n--json\nnumber,title,body,url,labels\n--\n",
        "a source asking for nothing writes nothing"
    );

    let (output, _) = deploy(&["linear", "ENG-42", "--agent", "codex", "--here"], 2);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = read("curl.args");
    assert!(!args.contains("lin_api_secret"), "the key never rides argv");
    assert_eq!(
        args.matches("--config\n-\nhttps://api.linear.app/graphql\n--\n")
            .count(),
        4
    );
    let calls: Vec<serde_json::Value> = read("curl.stdin")
        .split("\n--\n")
        .filter(|call| !call.is_empty())
        .map(|call| {
            assert!(call.contains("header = \"Authorization: lin_api_secret\""));
            let data = call.split("data = \"").nth(1).unwrap().trim_end();
            let data = data.strip_suffix('"').unwrap();
            serde_json::from_str(&data.replace("\\\"", "\"").replace("\\\\", "\\")).unwrap()
        })
        .collect();
    assert_eq!(calls.len(), 4, "fetch, comment, states, move");
    assert!(
        calls[1]["query"]
            .as_str()
            .unwrap()
            .contains("commentCreate")
    );
    assert_eq!(
        calls[1]["variables"],
        serde_json::json!({
            "id": "ENG-42",
            "body": "tutti deployed codex on this issue: project `api`, pane 7 of session `tutti`.",
        })
    );
    assert!(calls[2]["query"].as_str().unwrap().contains("states"));
    assert_eq!(calls[2]["variables"], serde_json::json!({"id": "ENG-42"}));
    assert!(calls[3]["query"].as_str().unwrap().contains("issueUpdate"));
    assert_eq!(
        calls[3]["variables"],
        serde_json::json!({"id": "uuid-42", "stateId": "st-2"}),
        "moved by the issue's own id to the state named"
    );

    let (output, requests) = deploy(&["shipped", "ENG-42", "--here"], 2);
    assert_eq!(output.status.code(), Some(1));
    assert!(matches!(requests[1], Request::Run { .. }), "the agent ran");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: shipped: writing back to ENG-42: ENG-42's team has no state \"Shipped\" \
         (it has Todo, In Progress)\n"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// An issue whose prompt is past what Linux's exec takes in one argument
/// deploys with every argument under it: the prompt goes to a file only the
/// user can read, which a short message names. A sandbox deploy, whose agent
/// can't read that file, is refused before anything runs.
#[test]
fn a_prompt_too_long_for_one_argument_deploys_through_a_file() {
    use std::os::unix::fs::PermissionsExt;
    use tutti::issues::MAX_ARG;
    let base = unique_base("longprompt");
    let project = base.join("api");
    let tmp = base.join("tmp");
    for dir in [&project, &tmp, &base.join("tutti")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let script = base.join("jira-issues");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '[{\"id\":\"PROJ-7\",\"title\":\"Long\",\"body\":\"'\n\
         head -c 200000 /dev/zero | tr '\\0' a\nprintf '\"}]'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        base.join("tutti/config.toml"),
        format!(
            "[[issues]]\nname = \"jira\"\nkind = \"command\"\ncommand = [\"{}\"]\n\n\
             [[sandboxes]]\nname = \"orb\"\nimage = \"agent:1\"\n",
            script.display()
        ),
    )
    .unwrap();
    let deploy = |extra: &[&str], n: usize| {
        let listener = UnixListener::bind(base.join("tutti/tutti.sock")).unwrap();
        let server = serve_requests(listener, n, answer_deploy(project.clone()));
        let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(["issue", "deploy", "jira", "PROJ-7", "--project", "1"])
            .args(extra)
            .env("XDG_RUNTIME_DIR", &base)
            .env("XDG_CONFIG_HOME", &base)
            .env("TMPDIR", &tmp)
            .env_remove("TUTTI_SESSION")
            .env_remove("TUTTI_PANE")
            .output()
            .unwrap();
        std::fs::remove_file(base.join("tutti/tutti.sock")).unwrap();
        (output, server.join().unwrap())
    };

    let (output, requests) = deploy(&[], 2);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let Request::Run { cmd, .. } = &requests[1] else {
        panic!("expected a run, got {:?}", requests[1]);
    };
    assert_eq!(cmd[0], "claude");
    assert!(cmd.iter().all(|arg| arg.len() < 1024), "{cmd:?}");
    let file = std::fs::read_dir(&tmp)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(cmd[1].contains(&*file.to_string_lossy()), "{cmd:?}");
    let prompt = std::fs::read_to_string(&file).unwrap();
    assert!(prompt.len() > MAX_ARG && prompt.starts_with("Work on this issue"));
    assert!(prompt.contains(&"a".repeat(200_000)));
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let (refused, requests) = deploy(&["--sandbox", "orb"], 1);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("too long for a command line"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(requests, [Request::WorkspaceList], "nothing ran");
    let _ = std::fs::remove_dir_all(&base);
}

/// `pane wait` prints the state that ended the wait, and exits 124 — with a
/// note on stderr — when the daemon answers that it timed out.
#[test]
fn pane_wait_prints_the_state_and_exits_124_on_a_timeout() {
    use tutti_core::{PaneInfo, WaitFor};
    let pane = |state: &str| -> Box<PaneInfo> {
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
                    from: None,
                    issue: None,
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

/// `run` from inside a pane names that pane as its caller, so the run lands
/// beside it — but only in the pane's own session: a pane id means nothing
/// to another session's daemon.
#[test]
fn run_from_a_pane_names_its_caller_in_its_own_session_only() {
    for (session, from) in [("tutti", Some(PaneId(4))), ("work", None)] {
        let output = run_scenario_with(
            "runfrom",
            &["-s", "tutti", "run", "--", "claude"],
            &[("TUTTI_PANE", "4"), ("TUTTI_SESSION", session)],
            "",
            move |req| {
                assert!(
                    matches!(req, Request::Run { from: f, .. } if *f == from),
                    "from {session}: {req:?}"
                );
            },
            Response::Ran {
                workspace: WorkspaceId(1),
                tab: tutti_core::TabId(2),
                pane: PaneId(7),
            },
        );
        assert!(output.status.success());
    }
}

/// `pane focus --direction` from inside a pane starts from it and prints the
/// bare id it moved to; at an edge the attached TUI took on it prints nothing
/// and exits 0; at any other edge it says `no pane <direction> of <id>` on
/// stderr and exits 1 — the contract smart-splits.nvim's backend reads.
#[test]
fn pane_focus_by_direction_prints_the_pane_or_exits_1_at_an_edge() {
    let output = run_scenario_with(
        "focusdir",
        &["pane", "focus", "--direction", "left"],
        &[("TUTTI_PANE", "4"), ("TUTTI_SESSION", "tutti")],
        "",
        |req| {
            assert_eq!(
                *req,
                Request::PaneFocusToward {
                    from: Some(PaneId(4)),
                    toward: Toward::Left,
                    wrap: false,
                }
            );
        },
        Response::Neighbor {
            from: PaneId(4),
            toward: Toward::Left,
            pane: Some(PaneId(2)),
            relayed: false,
        },
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "2\n");

    let output = run_scenario(
        "focusrelay",
        &["pane", "focus", "--direction", "left", "--from", "3"],
        |_| {},
        Response::Neighbor {
            from: PaneId(3),
            toward: Toward::Left,
            pane: None,
            relayed: true,
        },
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());

    let output = run_scenario(
        "focusedge",
        &["pane", "focus", "--direction", "up", "--from", "3"],
        |req| {
            assert!(matches!(
                req,
                Request::PaneFocusToward {
                    from: Some(PaneId(3)),
                    ..
                }
            ))
        },
        Response::Neighbor {
            from: PaneId(3),
            toward: Toward::Up,
            pane: None,
            relayed: false,
        },
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "no pane up of 3\n"
    );
}

/// `edit` with no editor pane in the caller's workspace says so and exits 1,
/// having asked the daemon for nothing but its view.
#[test]
fn edit_without_an_editor_pane_exits_1() {
    let view: Vec<WorkspaceView> = serde_json::from_str(
        r#"[{"id":1,"name":"api","dir":"/srv/api","branch":null,"tabs":[{"id":1,"name":"1",
            "active":true,"layout":{"Leaf":4},"active_pane":4,"panes":[{"id":4,"title":"sh",
            "agent":null,"state":"idle","exited":null}]}]}]"#,
    )
    .unwrap();
    let output = run_scenario_with(
        "editnone",
        &["edit", "src/main.rs:3"],
        &[("TUTTI_PANE", "4"), ("TUTTI_SESSION", "tutti")],
        "",
        |req| assert_eq!(*req, Request::View),
        Response::View { workspaces: view },
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "no editor pane in workspace api\n"
    );
}

/// Kills its child when dropped, so a failing test leaves no editor behind.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `edit` hands the file to a real Neovim through the server address its
/// pane's `TUTTI_EDITOR` names — at the line and column asked, a quote in the
/// name and all — then focuses that pane. Skipped without `nvim` on PATH.
#[test]
fn edit_opens_the_file_in_a_real_nvim_then_focuses_its_pane() {
    if Command::new("nvim").arg("--version").output().is_err() {
        return;
    }
    let base = unique_base("editnvim");
    std::fs::create_dir_all(base.join("tutti")).unwrap();
    let address = base.join("nvim.sock");
    let _nvim = Reaped(
        Command::new("nvim")
            .args(["--clean", "--headless", "--listen"])
            .arg(&address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !address.exists() {
        assert!(std::time::Instant::now() < deadline, "nvim never listened");
        thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::write(base.join("it's here.txt"), "one\ntwo\nthree\n").unwrap();
    let view: Vec<WorkspaceView> = serde_json::from_value(serde_json::json!([{
        "id": 1, "name": "api", "dir": "/srv/api", "branch": null,
        "tabs": [{"id": 1, "name": "1", "active": true, "layout": null, "active_pane": 4,
            "panes": [
                {"id": 4, "title": "sh", "agent": null, "state": "idle", "exited": null},
                {"id": 5, "title": "nvim", "agent": null, "state": "idle", "exited": null,
                 "user_vars": {"TUTTI_EDITOR": format!("nvim {}", address.display())}}
            ]}]
    }]))
    .unwrap();
    let listener = UnixListener::bind(base.join("tutti/tutti.sock")).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for (want, reply) in [
            (Request::View, Response::View { workspaces: view }),
            (Request::PaneFocus { pane: PaneId(5) }, Response::Ok),
        ] {
            assert_eq!(read_request(&mut stream), want);
            let frame = Frame::Control(serde_json::to_vec(&reply).unwrap());
            stream.write_all(&frame.encode()).unwrap();
        }
    });

    let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(["edit", "it's here.txt:2:3"])
        .current_dir(&base)
        .env("XDG_RUNTIME_DIR", &base)
        .env("TUTTI_PANE", "4")
        .env("TUTTI_SESSION", "tutti")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "edit is quiet: {output:?}");
    let probe = Command::new("nvim")
        .arg("--server")
        .arg(&address)
        .args([
            "--remote-expr",
            "bufname() .. ':' .. line('.') .. ':' .. col('.')",
        ])
        .output()
        .unwrap();
    let file = std::fs::canonicalize(base.join("it's here.txt")).unwrap();
    assert_eq!(
        String::from_utf8(probe.stdout).unwrap().trim(),
        format!("{}:2:3", file.display())
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// `notes --unread` asks for the unread history and prints it newest first —
/// what an agent reads to learn what blocked while it was busy — and `notes
/// read --pane` marks one pane's notes read.
#[test]
fn notes_lists_the_unread_history_newest_first() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let blocked = Note {
        id: 4,
        time: now,
        pane: PaneId(7),
        pane_title: "fix-tests".into(),
        workspace: "api".into(),
        kind: NoteKind::Blocked,
        title: None,
        body: None,
        read: false,
    };
    let done = Note {
        id: 9,
        pane: PaneId(8),
        pane_title: "docs".into(),
        kind: NoteKind::Done,
        ..blocked.clone()
    };
    let output = run_scenario(
        "notes",
        &["notes", "--unread"],
        |req| assert_eq!(*req, Request::NotesList { unread: true }),
        Response::Notes {
            notes: vec![blocked, done],
        },
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "ID  AGE  PANE  TITLE      WORKSPACE  KIND     UNREAD  TEXT\n\
         9   now  8     docs       api        done     *\n\
         4   now  7     fix-tests  api        blocked  *\n"
    );

    let output = run_scenario(
        "notesread",
        &["notes", "read", "--pane", "7"],
        |req| {
            assert_eq!(
                *req,
                Request::NotesRead {
                    up_to: None,
                    pane: Some(PaneId(7)),
                }
            );
        },
        Response::Ok,
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "ok\n");
}

/// `server restore` prints what came back on one line and names each skipped
/// entry on stderr; `--discard` says the saved layout is gone.
#[test]
fn server_restore_reports_what_came_back_and_what_did_not() {
    let output = run_scenario(
        "restore",
        &["server", "restore"],
        |req| assert_eq!(*req, Request::SessionRestore { discard: false }),
        Response::SessionRestored {
            workspaces: 2,
            panes: 5,
            revived: 1,
            skipped: vec!["/srv/gone: the directory is gone".into()],
        },
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "restored 2 projects · revived 1 agent · skipped 1\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "skipped /srv/gone: the directory is gone\n"
    );

    let output = run_scenario(
        "discard",
        &["server", "restore", "--discard"],
        |req| assert_eq!(*req, Request::SessionRestore { discard: true }),
        Response::SessionRestored {
            workspaces: 0,
            panes: 0,
            revived: 0,
            skipped: Vec::new(),
        },
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "discarded the saved layout\n"
    );
}

/// `watch` never starts a daemon: with none running it says so on stderr and
/// exits 1, having created nothing.
#[test]
fn watch_without_a_daemon_exits_1() {
    let base = unique_base("watchnone");
    let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .args(["watch", "-s", "nobody"])
        .env("XDG_RUNTIME_DIR", &base)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("no tutti-server is running for session 'nobody'"),
        "stderr was: {stderr}"
    );
    assert!(!base.exists(), "no daemon was started");
}

/// `watch` prints each snapshot as one line, verbatim, and exits 0 when the
/// daemon closes the connection.
#[test]
fn watch_prints_snapshot_lines_until_the_daemon_goes() {
    let base = unique_base("watch");
    let sock_dir = base.join("tutti");
    std::fs::create_dir_all(&sock_dir).unwrap();
    let listener = UnixListener::bind(sock_dir.join("tutti.sock")).unwrap();
    let snapshot = |state| Event::Snapshot {
        session: "tutti".into(),
        wire_rev: tutti_core::WIRE_REV,
        attached_terminal: None,
        panes: vec![WatchPane {
            id: PaneId(1),
            workspace: "api".into(),
            dir: "/srv/api".into(),
            tab: "claude".into(),
            title: "claude".into(),
            agent: Some("claude".into()),
            state,
            source: None,
            exited: None,
            focused: false,
            user_vars: Default::default(),
            cwd: None,
            approval: None,
            issue: None,
        }],
    };
    let lines: Vec<String> = [AgentState::Working, AgentState::Blocked]
        .into_iter()
        .map(|state| serde_json::to_string(&snapshot(state)).unwrap())
        .collect();
    let sent = lines.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(read_request(&mut stream), Request::Watch);
        for line in sent {
            stream
                .write_all(&Frame::Control(line.into_bytes()).encode())
                .unwrap();
        }
    });

    let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
        .arg("watch")
        .env("XDG_RUNTIME_DIR", &base)
        .output()
        .unwrap();
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&base);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n{}\n", lines[0], lines[1])
    );
}

/// A daemon that predates `watch` answers it with an error: exit 1, and say
/// what to do on stderr.
#[test]
fn watch_against_an_older_daemon_exits_1() {
    let output = run_scenario(
        "watchold",
        &["watch"],
        |req| assert_eq!(*req, Request::Watch),
        Response::Error {
            message: "bad request: unknown variant `watch`".into(),
        },
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unknown variant `watch`") && stderr.contains("tutti server stop"),
        "stderr was: {stderr}"
    );
}

/// A project and the workspace nested under it, as the daemon lists them.
fn project_and_workspace() -> Vec<WorkspaceInfo> {
    vec![
        WorkspaceInfo {
            id: WorkspaceId(1),
            name: "api".into(),
            dir: "/srv/api".into(),
            parent: None,
        },
        WorkspaceInfo {
            id: WorkspaceId(2),
            name: "api-fix".into(),
            dir: "/srv/api-fix".into(),
            parent: Some(WorkspaceId(1)),
        },
    ]
}

#[test]
fn workspace_list_json_round_trips() {
    let expected = Response::Workspaces {
        workspaces: project_and_workspace(),
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

/// `workspace list` shows every entry with its parent; `project list` asks
/// the same question and keeps the top-level entries, `--json` included.
#[test]
fn project_list_keeps_the_top_level_entries() {
    let reply = || Response::Workspaces {
        workspaces: project_and_workspace(),
    };
    let listed = |label, args: &[&str]| {
        let output = run_scenario(
            label,
            args,
            |req| assert_eq!(*req, Request::WorkspaceList),
            reply(),
        );
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(
        listed("wstable", &["workspace", "list"]),
        "ID  NAME     PARENT  DIR\n\
         1   api      -       /srv/api\n\
         2   api-fix  1       /srv/api-fix\n"
    );
    assert_eq!(
        listed("pjtable", &["project", "list"]),
        "ID  NAME  DIR\n\
         1   api   /srv/api\n"
    );
    let json: Response =
        serde_json::from_str(listed("pjjson", &["--json", "project", "list"]).trim()).unwrap();
    assert_eq!(
        json,
        Response::Workspaces {
            workspaces: project_and_workspace()[..1].to_vec()
        }
    );
}

/// `project add` opens a directory and says so in project words.
#[test]
fn project_add_prints_the_new_project() {
    let output = run_scenario(
        "pjadd",
        &["project", "add", "/srv/api"],
        |req| {
            assert_eq!(
                *req,
                Request::WorkspaceNew {
                    dir: "/srv/api".into()
                }
            );
        },
        Response::WorkspaceCreated { id: WorkspaceId(3) },
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "project 3 added\n"
    );
}

/// `tutti skill` prints the skill (`--raw` without the stderr notes), and
/// `shell-init <shell>` prints the snippet alone and says nothing on stderr:
/// an rc file evals it on every shell start, inside tutti or not.
#[test]
fn shell_init_prints_only_the_snippet() {
    use tutti::integrations::Shell;
    for (name, shell) in [
        ("zsh", Shell::Zsh),
        ("bash", Shell::Bash),
        ("fish", Shell::Fish),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(["shell-init", name])
            .env_remove("TUTTI_SESSION")
            .env_remove("TUTTI_PANE")
            .output()
            .unwrap();
        assert!(out.status.success() && out.stderr.is_empty(), "{name}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), shell.snippet());
    }
}

/// `skill --install --yes` writes it under `$HOME` — idempotently, replacing
/// an older tutti skill behind a `.bak`, never a skill that is not tutti's.
#[test]
fn skill_prints_and_installs_under_home() {
    let home = unique_base("skill");
    std::fs::create_dir_all(&home).unwrap();
    let skill = home.join(".claude/skills/tutti/SKILL.md");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(args)
            .env("HOME", &home)
            .output()
            .unwrap()
    };

    let raw = run(&["skill", "--raw"]);
    assert!(raw.status.success() && raw.stderr.is_empty());
    let ours = String::from_utf8(raw.stdout).unwrap();
    assert_eq!(ours, tutti::integrations::SKILL);
    let printed = run(&["skill"]);
    assert_eq!(String::from_utf8(printed.stdout).unwrap(), ours);
    assert!(
        String::from_utf8(printed.stderr)
            .unwrap()
            .contains("tutti skill --install")
    );

    let installed = run(&["skill", "--install", "--yes"]);
    assert!(installed.status.success());
    let stdout = String::from_utf8(installed.stdout).unwrap();
    assert!(
        stdout.starts_with(&format!("{}: adding the tutti skill\n", skill.display()))
            && stdout.ends_with(&format!(
                "installed the tutti skill into {}\n",
                skill.display()
            )),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), ours);
    let again = run(&["skill", "--install", "--yes"]);
    assert_eq!(
        String::from_utf8(again.stdout).unwrap(),
        format!("tutti skill already installed at {}\n", skill.display())
    );

    let older = "---\nname: tutti\ndescription: older\n---\n";
    std::fs::write(&skill, older).unwrap();
    assert!(run(&["skill", "--install", "--yes"]).status.success());
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), ours);
    assert_eq!(
        std::fs::read_to_string(skill.with_file_name("SKILL.md.bak")).unwrap(),
        older
    );

    let mine = "---\nname: deploy\ndescription: mine\n---\n";
    std::fs::write(&skill, mine).unwrap();
    let refused = run(&["skill", "--install", "--yes"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8(refused.stderr)
            .unwrap()
            .contains("is not tutti's skill; refusing to overwrite it")
    );
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), mine);
    let _ = std::fs::remove_dir_all(&home);
}

/// `skill --install --project` writes into the cwd's `.claude/skills`, and
/// answering anything but yes writes nothing.
#[test]
fn skill_install_into_a_project_asks_first() {
    let project = unique_base("skillproj");
    std::fs::create_dir_all(&project).unwrap();
    let install = |answer: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(["skill", "--install", "--project"])
            .current_dir(&project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let declined = install("n\n");
    assert_eq!(declined.status.code(), Some(1));
    assert!(
        String::from_utf8(declined.stderr)
            .unwrap()
            .ends_with("Write .claude/skills/tutti/SKILL.md? [y/N] aborted; nothing written\n")
    );
    assert!(!project.join(".claude").exists());

    assert!(install("y\n").status.success());
    assert_eq!(
        std::fs::read_to_string(project.join(".claude/skills/tutti/SKILL.md")).unwrap(),
        tutti::integrations::SKILL
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// Inside a pane of session `work`, bare `tutti`, `attach`, and an explicit
/// `-s work` all refuse to attach `work` — it would nest the session inside
/// its own pane — and exit 1 having started nothing.
#[test]
fn attach_refuses_to_nest_a_session_inside_its_own_pane() {
    // A socket dir too long for sun_path: should the guard ever regress, the
    // connect fails outright rather than auto-starting a daemon.
    let runtime = unique_base("nest").join("x".repeat(100));
    for args in [&[][..], &["attach"][..], &["-s", "work", "attach"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("TUTTI_PANE", "7")
            .env("TUTTI_SESSION", "work")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "error: this pane is inside tutti session 'work' — attaching here would nest \
             the session inside itself (use -s <other-session>)\n",
            "{args:?}"
        );
    }
    assert!(!runtime.exists(), "nothing started");
}

/// `pane attach` refuses its own pane, whose output it would echo back into
/// itself forever, and a stdin that is no terminal — exit 1, nothing started.
#[test]
fn pane_attach_refuses_its_own_pane_and_a_stdin_that_is_no_terminal() {
    let runtime = unique_base("paneattach");
    for (pane_env, want) in [
        ("7", "error: pane 7 is the pane this runs in\n"),
        ("3", "error: pane attach needs a terminal on stdin\n"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tutti"))
            .args(["-s", "work", "pane", "attach", "7"])
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &runtime)
            .env("TUTTI_PANE", pane_env)
            .env("TUTTI_SESSION", "work")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{want}");
        assert!(output.stdout.is_empty(), "{want}");
        assert_eq!(String::from_utf8(output.stderr).unwrap(), want);
    }
    assert!(!runtime.exists(), "nothing started");
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
                    since: None,
                    last_output: false,
                    last_reply: false,
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

/// `pane read --since` keeps stdout the pane's text alone: the next cursor,
/// and the note when rows were lost, go to stderr.
#[test]
fn pane_read_since_prints_lines_then_the_cursor_on_stderr() {
    let output = run_scenario(
        "panetail",
        &["pane", "read", "3", "--since", "40"],
        |req| {
            assert_eq!(
                *req,
                Request::PaneRead {
                    pane: PaneId(3),
                    lines: None,
                    unwrapped: false,
                    since: Some(40),
                    last_output: false,
                    last_reply: false,
                }
            );
        },
        Response::Tail {
            lines: vec!["committed".into(), "screen".into()],
            cursor: 52,
            truncated: true,
        },
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "committed\nscreen\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("next cursor: 52"), "stderr was: {stderr}");
    assert!(
        stderr.contains("left the pane's scrollback"),
        "stderr was: {stderr}"
    );
}

/// `pane read --last-output` keeps stdout the command's output alone: the
/// exit code, and the note when rows were lost, go to stderr, and the CLI
/// exits 0 whatever the command's code.
#[test]
fn pane_read_last_output_prints_lines_then_the_exit_code_on_stderr() {
    let output = run_scenario(
        "lastout",
        &["pane", "read", "3", "--last-output", "--unwrapped"],
        |req| {
            assert_eq!(
                *req,
                Request::PaneRead {
                    pane: PaneId(3),
                    lines: None,
                    unwrapped: true,
                    since: None,
                    last_output: true,
                    last_reply: false,
                }
            );
        },
        Response::LastOutput {
            lines: vec!["a".into(), "b".into()],
            exit: Some(2),
            truncated: true,
        },
    );

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "a\nb\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.ends_with("exit: 2\n"), "stderr was: {stderr}");
    assert!(
        stderr.contains("left the pane's scrollback"),
        "stderr was: {stderr}"
    );
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
