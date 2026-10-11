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
    Response, Toward, WatchPane, WorkspaceId, WorkspaceInfo, WorkspaceView,
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
/// bare id it moved to; at an edge it says `no pane <direction> of <id>` on
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
        },
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "2\n");

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
