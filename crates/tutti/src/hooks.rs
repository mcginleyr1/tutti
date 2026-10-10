//! Agent hook integrations: map a Claude Code hook-event JSON object, or an
//! opencode bus event forwarded by tutti's opencode plugin (each delivered on
//! `tutti agent-event`'s stdin), to a tutti `AgentHookEvent` and the agent's
//! own conversation id; print the settings.json snippet that wires Claude
//! Code's events to `tutti agent-event claude` (or, for a sandbox image,
//! straight to the terminal as OSC 7501), and the opencode plugin that pipes
//! to `tutti agent-event opencode`.
//!
//! The mappings read only the fields tutti reacts to and ignore everything else,
//! so they tolerate each tool's evolving schema. They never fail: an
//! unrecognized or irrelevant event maps to `None` (send nothing), and the CLI
//! path around them swallows all errors so a hook can never break a session.

use serde_json::{Value, json};
use tutti_core::{AgentHookEvent, ApprovalDecision};

/// The command a hook runs; also the addressable verb this module documents.
pub const HOOK_COMMAND: &str = "tutti agent-event claude";

/// The opencode plugin `tutti hooks opencode` prints and installs.
pub const OPENCODE_PLUGIN: &str = include_str!("opencode_plugin.js");
/// The plugin's first line: marks a file of the plugin's name as tutti's to
/// replace — anything else there is never overwritten.
pub const OPENCODE_PLUGIN_MARKER: &str = "// tutti opencode plugin";

/// Map one opencode bus event, as the plugin forwards it (`{type,
/// properties}`), to a tutti `AgentHookEvent` and the root session's id, or
/// `None` when irrelevant. Verified against opencode 1.18.5's event schemas:
/// `session.status` carries `properties.status.type` (`busy` | `retry` |
/// `idle`); `session.idle` and `session.error` end the turn; `permission.asked`
/// names its `permission`, `question.asked` its `questions[].question`; a
/// `*.replied` or `question.rejected` means you answered and the agent goes on.
/// Every one names its `sessionID` (optional only on `session.error`), but
/// only the `session.*` events are surely the root session's: the plugin drops
/// a child (subagent) session's, while a child's prompts pass — so only those
/// name the conversation.
pub fn map_opencode_event(v: &Value) -> Option<(AgentHookEvent, Option<String>)> {
    let text = |pointer: &str| {
        v.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let kind = v.get("type")?.as_str()?;
    let event = match kind {
        "session.status" => match v.pointer("/properties/status/type")?.as_str()? {
            "busy" | "retry" => Some(AgentHookEvent::Activity { detail: None }),
            "idle" => Some(AgentHookEvent::Done),
            _ => None,
        },
        "session.idle" | "session.error" => Some(AgentHookEvent::Done),
        "permission.asked" => Some(AgentHookEvent::Blocked {
            message: text("/properties/permission"),
        }),
        "question.asked" => Some(AgentHookEvent::Blocked {
            message: text("/properties/questions/0/question"),
        }),
        "permission.replied" | "question.replied" | "question.rejected" => {
            Some(AgentHookEvent::Activity { detail: None })
        }
        _ => None,
    }?;
    let root = matches!(kind, "session.status" | "session.idle" | "session.error");
    Some((event, text("/properties/sessionID").filter(|_| root)))
}

/// Map one Claude Code hook event to a tutti `AgentHookEvent` and the
/// conversation's `session_id`, or `None` when the event is irrelevant (nothing
/// is sent). Verified against the hooks reference: every event carries
/// `hook_event_name` and `session_id` (a subagent's events carry its parent
/// session's); tool events carry `tool_name`/`tool_input` (the `Task` tool's
/// input has `description`/`title`), `PermissionRequest` among them, fired
/// the moment Claude would show a permission dialog; `Notification` carries
/// `message` and `notification_type`; `UserPromptSubmit` carries the
/// `prompt`; subagent events carry `agent_id`/`agent_type`.
pub fn map_claude_event(v: &Value) -> Option<(AgentHookEvent, Option<String>)> {
    let event = match v.get("hook_event_name")?.as_str()? {
        // Blocked at once — the `permission_prompt` notification only comes
        // seconds later — naming what would run.
        "PermissionRequest" => {
            permission_request(v).map(|(tool, summary)| AgentHookEvent::Blocked {
                message: Some(format!("{tool} · {summary}")),
            })
        }
        "PreToolUse" if tool_name(v) == Some("Task") => {
            let desc = subagent_desc(v);
            Some(AgentHookEvent::SubagentStarted {
                id: desc.clone(),
                desc,
            })
        }
        "PreToolUse" | "PostToolUse" => Some(AgentHookEvent::Activity {
            detail: tool_name(v).map(str::to_string),
        }),
        "SubagentStop" => Some(AgentHookEvent::SubagentStopped { id: subagent_id(v) }),
        "Notification" => match v.get("notification_type").and_then(Value::as_str) {
            // Reports, not requests. The idle reminder lands a minute after
            // Stop marked the pane done: re-raising it would queue a pane you
            // have already seen.
            Some(
                "idle_prompt"
                | "auth_success"
                | "elicitation_complete"
                | "elicitation_response"
                | "agent_completed"
                | "quota_auto_resume_fired"
                | "quota_auto_resume_disabled",
            ) => None,
            // Claude waits on you — a permission or MCP dialog, a question, an
            // Enter after a usage limit (`quota_auto_resume_stale`). An older
            // Claude names no type, and a type tutti does not know yet keeps
            // the safe reading: blocked.
            _ => Some(AgentHookEvent::Blocked {
                message: v.get("message").and_then(Value::as_str).map(str::to_string),
            }),
        },
        "Stop" => Some(AgentHookEvent::Done),
        "UserPromptSubmit" => Some(AgentHookEvent::Prompt {
            text: v
                .get("prompt")
                .and_then(Value::as_str)
                .and_then(tutti_agents::first_prompt_line),
        }),
        "SessionStart" => Some(AgentHookEvent::SessionStarted),
        _ => None,
    }?;
    let conversation = v
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((event, conversation))
}

fn tool_name(v: &Value) -> Option<&str> {
    v.get("tool_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// A subagent's description from the `Task` tool input: `description`, then
/// `title`, falling back to a generic label so a row always has text.
fn subagent_desc(v: &Value) -> String {
    let input = v.get("tool_input");
    input
        .and_then(|i| i.get("description").or_else(|| i.get("title")))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("subagent")
        .to_string()
}

/// A subagent's id from a `SubagentStop`: its `agent_id`, then `agent_type`. The
/// server tolerates a value that matches no started row (it finishes the oldest
/// running one), so an empty string here is fine.
fn subagent_id(v: &Value) -> String {
    v.get("agent_id")
        .or_else(|| v.get("agent_type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// What a `PermissionRequest` asks to run: its tool, and the tool's input on
/// one line (`input_summary`). `None` for any other event.
pub fn permission_request(v: &Value) -> Option<(String, String)> {
    (v.get("hook_event_name")?.as_str()? == "PermissionRequest").then(|| {
        let tool = tool_name(v).unwrap_or("tool").to_string();
        (tool, input_summary(v))
    })
}

/// The `tool_input` fields a summary reads, first present wins: Bash's
/// command, the file tools' paths, WebFetch's URL, WebSearch's query, Glob
/// and Grep's pattern, then a search path and a subagent's task.
const SUMMARY_FIELDS: &[&str] = &[
    "command",
    "file_path",
    "notebook_path",
    "url",
    "query",
    "pattern",
    "path",
    "description",
    "prompt",
];

/// A tool call's input on one line, for a held approval: the first of
/// `SUMMARY_FIELDS` present — a path relative to the session's `cwd` when
/// beneath it — else its first string field; line breaks shown as `⏎`,
/// control characters blanked so nothing reaches the terminal raw, and cut
/// with `…` past 500 characters, so nothing is hidden without a mark.
fn input_summary(v: &Value) -> String {
    let input = v.get("tool_input");
    let (field, text) = SUMMARY_FIELDS
        .iter()
        .find_map(|&field| Some((field, input?.get(field)?.as_str()?)))
        .or_else(|| Some(("", input?.as_object()?.values().find_map(Value::as_str)?)))
        .unwrap_or(("", ""));
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|_| field.ends_with("path"));
    let text: String = cwd
        .and_then(|cwd| text.strip_prefix(cwd)?.strip_prefix('/'))
        .unwrap_or(text)
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    let line = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    let mut chars = line.chars();
    let mut summary: String = chars.by_ref().take(500).collect();
    if chars.next().is_some() {
        summary.push('…');
    }
    summary
}

/// The `PermissionRequest` hook output carrying `decision` to Claude, or
/// `None` for a pass: the hook prints nothing and Claude shows its own
/// dialog. A denial gives Claude its reason, else says tutti's user denied it.
pub fn permission_decision(decision: &ApprovalDecision) -> Option<Value> {
    let decision = match decision {
        ApprovalDecision::Allow => json!({ "behavior": "allow" }),
        ApprovalDecision::Deny { message } => json!({
            "behavior": "deny",
            "message": message.as_deref().unwrap_or("the user denied this in tutti"),
        }),
        ApprovalDecision::Pass => return None,
    };
    Some(json!({
        "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
    }))
}

/// The settings.json `hooks` snippet wiring every relevant event to
/// `tutti agent-event claude`. Built as JSON so it is always valid.
pub fn claude_hooks_json() -> Value {
    // Tool + subagent events filter on the tool/agent (matcher "*" = all); the
    // per-event mapping decides what each one means. The rest take no matcher:
    // every notification type, every session source, every prompt and stop.
    let cmd = json!({ "type": "command", "command": HOOK_COMMAND });
    let with_matcher = json!([{ "matcher": "*", "hooks": [cmd] }]);
    let no_matcher = json!([{ "hooks": [cmd] }]);
    json!({
        "hooks": {
            "PreToolUse": with_matcher,
            "PostToolUse": with_matcher,
            "PermissionRequest": with_matcher,
            "SubagentStop": with_matcher,
            "Notification": no_matcher,
            "Stop": no_matcher,
            "UserPromptSubmit": no_matcher,
            "SessionStart": no_matcher,
        }
    })
}

/// The pretty-printed snippet, ready to paste into settings.json.
pub fn claude_hooks_snippet() -> String {
    serde_json::to_string_pretty(&claude_hooks_json()).expect("hooks snippet serializes")
}

/// The Notification types that wait on you, as the `--tty` hooks' matcher. A
/// matcher can only name types, so an unknown one cannot read as blocked the
/// way it does through `tutti agent-event`.
const WAITING: &str =
    "permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input";

/// The settings.json `hooks` snippet for an agent with no route to the daemon
/// — one in a k8s sandbox. Each hook writes an OSC 7501 report straight to its
/// terminal, which `kubectl exec -it` carries back to the pane like any other
/// output, so the image needs only `printf`: a prompt or tool use reports
/// `working`, a waiting Notification `blocked`, Stop `done`, and SessionEnd
/// `clear`. Every command is silent and never fails, and writes nothing to
/// stdout (Claude hands a UserPromptSubmit hook's stdout to the model).
pub fn claude_tty_hooks_json() -> Value {
    let report = |state: &str, matcher: Option<&str>| {
        let command = format!(
            r"{{ printf '\033]7501;state={state}:app=claude\007' >/dev/tty; }} 2>/dev/null || true"
        );
        let mut group = json!({ "hooks": [{ "type": "command", "command": command }] });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        json!([group])
    };
    json!({
        "hooks": {
            "UserPromptSubmit": report("working", None),
            "PreToolUse": report("working", Some("*")),
            "PostToolUse": report("working", Some("*")),
            "Notification": report("blocked", Some(WAITING)),
            "Stop": report("done", None),
            "SessionEnd": report("clear", None),
        }
    })
}

/// Merge the tutti hook entries into an existing settings.json value. Returns
/// `None` when every event already carries the tutti hook (nothing to do),
/// otherwise the merged settings plus the events that were added. Existing
/// settings keys and foreign hook entries are preserved untouched; a
/// non-object `settings` or `hooks` value is a hard error rather than a guess.
pub fn merge_claude_hooks(settings: &Value) -> anyhow::Result<Option<(Value, Vec<String>)>> {
    use anyhow::bail;

    let mut merged = match settings {
        Value::Null => json!({}),
        Value::Object(_) => settings.clone(),
        other => bail!("settings.json root is not an object: {other}"),
    };
    let ours = claude_hooks_json();
    let ours = ours["hooks"].as_object().expect("snippet has hooks");

    let hooks_slot = merged
        .as_object_mut()
        .expect("merged is an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let Value::Object(hooks) = hooks_slot else {
        bail!("existing \"hooks\" value is not an object");
    };

    let mut added = Vec::new();
    for (event, groups) in ours {
        let slot = hooks.entry(event.clone()).or_insert_with(|| json!([]));
        let Value::Array(existing) = slot else {
            bail!("existing hooks entry for {event} is not an array");
        };
        if existing.iter().any(has_tutti_hook) {
            continue;
        }
        existing.extend(groups.as_array().expect("snippet groups").iter().cloned());
        added.push(event.clone());
    }
    if added.is_empty() {
        return Ok(None);
    }
    Ok(Some((merged, added)))
}

/// Whether a hook group already runs the tutti command.
fn has_tutti_hook(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks
                .iter()
                .any(|h| h.get("command").and_then(Value::as_str) == Some(HOOK_COMMAND))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(json: &str) -> Option<AgentHookEvent> {
        map_claude_event(&serde_json::from_str(json).unwrap()).map(|(event, _)| event)
    }

    /// The conversation id a Claude event names, `None` when it maps to
    /// nothing at all.
    fn conversation(json: &str) -> Option<String> {
        map_claude_event(&serde_json::from_str(json).unwrap()).and_then(|(_, id)| id)
    }

    #[test]
    fn pre_tool_use_task_becomes_subagent_started() {
        let out = ev(r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Task",
            "tool_input": { "description": "build the core", "title": "core" }
        }"#);
        assert_eq!(
            out,
            Some(AgentHookEvent::SubagentStarted {
                id: "build the core".into(),
                desc: "build the core".into(),
            })
        );
    }

    #[test]
    fn task_without_description_falls_back_to_title_then_placeholder() {
        let title = ev(r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Task",
            "tool_input": { "title": "only a title" }
        }"#);
        assert_eq!(
            title,
            Some(AgentHookEvent::SubagentStarted {
                id: "only a title".into(),
                desc: "only a title".into(),
            })
        );
        let bare =
            ev(r#"{ "hook_event_name": "PreToolUse", "tool_name": "Task", "tool_input": {} }"#);
        assert_eq!(
            bare,
            Some(AgentHookEvent::SubagentStarted {
                id: "subagent".into(),
                desc: "subagent".into(),
            })
        );
    }

    #[test]
    fn other_tools_and_post_tool_use_become_activity() {
        assert_eq!(
            ev(r#"{ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {} }"#),
            Some(AgentHookEvent::Activity {
                detail: Some("Bash".into())
            })
        );
        assert_eq!(
            ev(r#"{ "hook_event_name": "PostToolUse", "tool_name": "Edit" }"#),
            Some(AgentHookEvent::Activity {
                detail: Some("Edit".into())
            })
        );
    }

    #[test]
    fn subagent_stop_reads_agent_id_then_type() {
        assert_eq!(
            ev(r#"{ "hook_event_name": "SubagentStop", "agent_id": "abc" }"#),
            Some(AgentHookEvent::SubagentStopped { id: "abc".into() })
        );
        assert_eq!(
            ev(r#"{ "hook_event_name": "SubagentStop", "agent_type": "Explore" }"#),
            Some(AgentHookEvent::SubagentStopped {
                id: "Explore".into()
            })
        );
    }

    #[test]
    fn an_untyped_notification_from_an_older_claude_still_blocks() {
        assert_eq!(
            ev(r#"{ "hook_event_name": "Notification", "message": "allow edit?" }"#),
            Some(AgentHookEvent::Blocked {
                message: Some("allow edit?".into())
            })
        );
    }

    fn notification(kind: &str) -> Option<AgentHookEvent> {
        ev(&format!(
            r#"{{ "hook_event_name": "Notification", "message": "m", "notification_type": "{kind}" }}"#
        ))
    }

    #[test]
    fn notifications_that_wait_on_you_block() {
        for kind in [
            "permission_prompt",
            "elicitation_dialog",
            "elicitation_url_dialog",
            "agent_needs_input",
            "quota_auto_resume_stale",
            "some_future_type",
        ] {
            assert_eq!(
                notification(kind),
                Some(AgentHookEvent::Blocked {
                    message: Some("m".into())
                }),
                "{kind}"
            );
        }
    }

    #[test]
    fn notifications_that_only_report_send_nothing() {
        for kind in [
            "idle_prompt",
            "auth_success",
            "elicitation_complete",
            "elicitation_response",
            "agent_completed",
            "quota_auto_resume_fired",
            "quota_auto_resume_disabled",
        ] {
            assert_eq!(notification(kind), None, "{kind}");
        }
    }

    /// A `PermissionRequest` (its payload as the hooks reference shows it)
    /// blocks the pane at once, naming the tool and its input on one line.
    #[test]
    fn a_permission_request_blocks_naming_the_tool_and_its_input() {
        let blocked = |tool: &str, input: Value| {
            ev(&json!({
                "session_id": "abc123",
                "cwd": "/srv/api",
                "permission_mode": "default",
                "hook_event_name": "PermissionRequest",
                "tool_name": tool,
                "tool_input": input,
                "permission_suggestions": [],
            })
            .to_string())
        };
        let message = |text: &str| {
            Some(AgentHookEvent::Blocked {
                message: Some(text.into()),
            })
        };
        assert_eq!(
            blocked(
                "Bash",
                json!({ "command": "rm -rf build", "description": "Clean" })
            ),
            message("Bash · rm -rf build")
        );
        assert_eq!(
            blocked(
                "Edit",
                json!({ "file_path": "/srv/api/src/main.rs", "old_string": "a", "new_string": "b" })
            ),
            message("Edit · src/main.rs"),
            "a path beneath the session's cwd reads relative to it"
        );
        assert_eq!(
            blocked(
                "Write",
                json!({ "file_path": "/etc/hosts", "content": "x" })
            ),
            message("Write · /etc/hosts")
        );
        assert_eq!(
            blocked("Bash", json!({ "command": "/srv/api/run.sh" })),
            message("Bash · /srv/api/run.sh"),
            "a command is never rewritten"
        );
        assert_eq!(
            blocked(
                "WebFetch",
                json!({ "url": "https://example.com", "prompt": "summarize" })
            ),
            message("WebFetch · https://example.com")
        );
        assert_eq!(
            blocked("Grep", json!({ "pattern": "TODO", "path": "/srv/api/src" })),
            message("Grep · TODO")
        );
        assert_eq!(
            blocked(
                "mcp__github__create_issue",
                json!({ "count": 3, "repo": "tutti" })
            ),
            message("mcp__github__create_issue · tutti"),
            "another tool shows its first string field"
        );
        assert_eq!(
            blocked("Bash", json!({ "command": "cd /tmp\n\n  rm -rf build\n" })),
            message("Bash · cd /tmp ⏎ rm -rf build"),
            "every line shows, the breaks marked"
        );
        assert_eq!(
            blocked("Bash", json!({ "command": "echo \u{1b}]52;c;aGk=\u{7}" })),
            message("Bash · echo  ]52;c;aGk="),
            "no control character reaches the terminal"
        );
        let Some(AgentHookEvent::Blocked {
            message: Some(long),
        }) = blocked("Bash", json!({ "command": "x".repeat(600) }))
        else {
            panic!("expected a blocked event");
        };
        assert_eq!(
            long,
            format!("Bash · {}…", "x".repeat(500)),
            "a cut is marked"
        );
        assert_eq!(
            permission_request(&json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash" })),
            None
        );
    }

    /// A held approval's answer goes back to Claude as the documented
    /// `PermissionRequest` decision object; a pass prints nothing, so
    /// Claude's own dialog appears.
    #[test]
    fn decisions_print_as_claudes_permission_request_output() {
        let output = |decision: Value| {
            json!({
                "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
            })
        };
        assert_eq!(
            permission_decision(&ApprovalDecision::Allow),
            Some(output(json!({ "behavior": "allow" })))
        );
        assert_eq!(
            permission_decision(&ApprovalDecision::Deny {
                message: Some("not on main".into())
            }),
            Some(output(
                json!({ "behavior": "deny", "message": "not on main" })
            ))
        );
        assert_eq!(
            permission_decision(&ApprovalDecision::Deny { message: None }),
            Some(output(
                json!({ "behavior": "deny", "message": "the user denied this in tutti" })
            ))
        );
        assert_eq!(permission_decision(&ApprovalDecision::Pass), None);
    }

    #[test]
    fn stop_becomes_done() {
        assert_eq!(
            ev(r#"{ "hook_event_name": "Stop", "last_assistant_message": "all set" }"#),
            Some(AgentHookEvent::Done)
        );
    }

    #[test]
    fn a_submitted_prompt_starts_a_turn_labelled_by_its_first_line() {
        let prompt = |text: &str| {
            ev(&json!({ "hook_event_name": "UserPromptSubmit", "prompt": text }).to_string())
        };
        assert_eq!(
            prompt("  fix the failing test  \nit is in tests/server.rs"),
            Some(AgentHookEvent::Prompt {
                text: Some("fix the failing test".into())
            })
        );
        let long = "x".repeat(200);
        assert_eq!(
            prompt(&long),
            Some(AgentHookEvent::Prompt {
                text: Some("x".repeat(80))
            }),
            "a wall of text is capped to a label"
        );
        for injected in [
            "<pasted_content id=\"1\">\nlog\n</pasted_content id=\"1\">\nfix this",
            "<task-notification>done</task-notification>",
            "",
        ] {
            assert_eq!(
                prompt(injected),
                Some(AgentHookEvent::Prompt { text: None }),
                "still a turn, but nothing to label it by: {injected:?}"
            );
        }
        assert_eq!(
            ev(r#"{ "hook_event_name": "UserPromptSubmit" }"#),
            Some(AgentHookEvent::Prompt { text: None })
        );
    }

    #[test]
    fn every_event_names_its_session_and_a_session_start_maps() {
        assert_eq!(
            map_claude_event(&json!({
                "hook_event_name": "SessionStart",
                "session_id": "abc123",
                "source": "clear",
            })),
            Some((AgentHookEvent::SessionStarted, Some("abc123".into())))
        );
        assert_eq!(
            conversation(
                r#"{ "hook_event_name": "PostToolUse", "tool_name": "Edit", "session_id": "abc123", "agent_id": "a1" }"#
            ),
            Some("abc123".into()),
            "a subagent's event names its parent session"
        );
        assert_eq!(
            conversation(r#"{ "hook_event_name": "Stop", "session_id": "" }"#),
            None
        );
        assert_eq!(conversation(r#"{ "hook_event_name": "Stop" }"#), None);
    }

    #[test]
    fn irrelevant_or_malformed_events_map_to_nothing() {
        assert_eq!(ev(r#"{ "hook_event_name": "SessionEnd" }"#), None);
        assert_eq!(ev(r#"{ "hook_event_name": "PreCompact" }"#), None);
        // Missing the event name entirely.
        assert_eq!(ev(r#"{ "tool_name": "Bash" }"#), None);
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // A future schema adds fields we do not read; the mapping still works.
        let out = ev(r#"{
            "hook_event_name": "Stop",
            "session_id": "x",
            "cwd": "/tmp",
            "effort": { "level": "high" },
            "brand_new_field": [1, 2, 3]
        }"#);
        assert_eq!(out, Some(AgentHookEvent::Done));
    }

    #[test]
    fn snippet_is_valid_json_naming_every_event_and_the_command() {
        let value = claude_hooks_json();
        let hooks = value.get("hooks").unwrap();
        for event in [
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "SubagentStop",
            "Notification",
            "Stop",
            "UserPromptSubmit",
            "SessionStart",
        ] {
            assert!(hooks.get(event).is_some(), "missing {event}");
        }
        assert_eq!(hooks.as_object().unwrap().len(), 8);
        assert_eq!(
            hooks["PermissionRequest"][0]["matcher"], "*",
            "every tool's permission request"
        );
        assert!(claude_hooks_snippet().contains(HOOK_COMMAND));
    }

    /// The `--tty` hooks report each event's state over OSC 7501 with plain
    /// `printf` — no tutti binary — silently, never failing, never on stdout.
    #[test]
    fn tty_hooks_report_each_event_over_osc_7501() {
        let value = claude_tty_hooks_json();
        let hooks = value["hooks"].as_object().unwrap();
        for (event, matcher, state) in [
            ("UserPromptSubmit", None, "working"),
            ("PreToolUse", Some("*"), "working"),
            ("PostToolUse", Some("*"), "working"),
            ("Notification", Some(WAITING), "blocked"),
            ("Stop", None, "done"),
            ("SessionEnd", None, "clear"),
        ] {
            let group = &hooks[event][0];
            assert_eq!(group["matcher"].as_str(), matcher, "{event}");
            assert_eq!(
                group["hooks"][0]["command"],
                format!(
                    r"{{ printf '\033]7501;state={state}:app=claude\007' >/dev/tty; }} 2>/dev/null || true"
                ),
                "{event}"
            );
        }
        assert_eq!(hooks.len(), 6);
        assert_eq!(
            WAITING.split('|').collect::<Vec<_>>(),
            [
                "permission_prompt",
                "elicitation_dialog",
                "elicitation_url_dialog",
                "agent_needs_input"
            ]
        );
        for kind in WAITING.split('|') {
            assert!(
                matches!(notification(kind), Some(AgentHookEvent::Blocked { .. })),
                "{kind} blocks through agent-event too"
            );
        }
    }

    #[test]
    fn merge_installs_into_empty_settings() {
        let (merged, added) = merge_claude_hooks(&json!({})).unwrap().unwrap();
        assert_eq!(added.len(), 8);
        assert!(merged["hooks"]["Stop"][0]["hooks"][0]["command"] == HOOK_COMMAND);
        assert!(
            merged["hooks"]["UserPromptSubmit"][0]
                .get("matcher")
                .is_none()
        );
    }

    /// Re-running `--install` over what the five-event release installed adds
    /// exactly the events since and leaves every existing entry alone.
    #[test]
    fn merge_upgrades_an_old_install_with_just_the_new_events() {
        let cmd = json!({ "type": "command", "command": HOOK_COMMAND });
        let with_matcher = json!([{ "matcher": "*", "hooks": [cmd] }]);
        let no_matcher = json!([{ "hooks": [cmd] }]);
        let old = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": with_matcher,
                "PostToolUse": with_matcher,
                "SubagentStop": with_matcher,
                "Notification": no_matcher,
                "Stop": no_matcher,
            }
        });
        let (merged, mut added) = merge_claude_hooks(&old).unwrap().unwrap();
        added.sort();
        assert_eq!(
            added,
            ["PermissionRequest", "SessionStart", "UserPromptSubmit"]
        );
        for event in [
            "PreToolUse",
            "PostToolUse",
            "SubagentStop",
            "Notification",
            "Stop",
        ] {
            assert_eq!(merged["hooks"][event], old["hooks"][event], "{event}");
        }
        assert_eq!(merged["model"], "opus");
        assert!(merge_claude_hooks(&merged).unwrap().is_none());
    }

    #[test]
    fn merge_preserves_foreign_settings_and_hooks() {
        let existing = json!({
            "model": "opus",
            "hooks": { "PreToolUse": [{ "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "my-linter" }] }] }
        });
        let (merged, added) = merge_claude_hooks(&existing).unwrap().unwrap();
        assert_eq!(merged["model"], "opus");
        assert_eq!(
            merged["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "my-linter"
        );
        assert_eq!(
            merged["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            HOOK_COMMAND
        );
        assert!(added.contains(&"PreToolUse".to_string()));
    }

    #[test]
    fn merge_is_idempotent() {
        let (merged, _) = merge_claude_hooks(&json!({})).unwrap().unwrap();
        assert!(merge_claude_hooks(&merged).unwrap().is_none());
    }

    #[test]
    fn merge_adds_only_missing_events_on_a_partial_install() {
        let (full, _) = merge_claude_hooks(&json!({})).unwrap().unwrap();
        let mut partial = full.clone();
        partial["hooks"].as_object_mut().unwrap().remove("Stop");
        let (_, added) = merge_claude_hooks(&partial).unwrap().unwrap();
        assert_eq!(added, vec!["Stop".to_string()]);
    }

    fn oc(json: &str) -> Option<AgentHookEvent> {
        map_opencode_event(&serde_json::from_str(json).unwrap()).map(|(event, _)| event)
    }

    /// The root session id an opencode event names, `None` when it names none
    /// or maps to nothing at all.
    fn oc_conversation(json: &str) -> Option<String> {
        map_opencode_event(&serde_json::from_str(json).unwrap()).and_then(|(_, id)| id)
    }

    #[test]
    fn opencode_names_the_conversation_from_root_session_events_only() {
        for root in [
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"busy"}}}"#,
            r#"{"type":"session.idle","properties":{"sessionID":"ses_1"}}"#,
            r#"{"type":"session.error","properties":{"sessionID":"ses_1","error":{"name":"UnknownError"}}}"#,
        ] {
            assert_eq!(oc_conversation(root), Some("ses_1".into()), "{root}");
        }
        assert_eq!(
            oc_conversation(
                r#"{"type":"session.error","properties":{"error":{"name":"UnknownError"}}}"#
            ),
            None,
            "session.error may name no session"
        );
        // A subagent's prompts reach the pane too, under the child's id.
        for maybe_child in [
            r#"{"type":"permission.asked","properties":{"id":"per_1","sessionID":"ses_child","permission":"bash"}}"#,
            r#"{"type":"question.asked","properties":{"id":"que_1","sessionID":"ses_child","questions":[]}}"#,
            r#"{"type":"permission.replied","properties":{"sessionID":"ses_child","requestID":"per_1","reply":"once"}}"#,
        ] {
            assert!(oc(maybe_child).is_some());
            assert_eq!(oc_conversation(maybe_child), None, "{maybe_child}");
        }
    }

    #[test]
    fn opencode_session_status_maps_busy_retry_and_idle() {
        let working = Some(AgentHookEvent::Activity { detail: None });
        assert_eq!(
            oc(
                r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"busy"}}}"#
            ),
            working
        );
        assert_eq!(
            oc(
                r#"{"type":"session.status","properties":{"sessionID":"ses_1",
                "status":{"type":"retry","attempt":2,"message":"overloaded","next":1}}}"#
            ),
            working
        );
        assert_eq!(
            oc(
                r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"idle"}}}"#
            ),
            Some(AgentHookEvent::Done)
        );
        assert_eq!(
            oc(r#"{"type":"session.status","properties":{"status":{"type":"later"}}}"#),
            None,
            "an unknown status type sends nothing"
        );
    }

    #[test]
    fn opencode_idle_and_error_end_the_turn() {
        assert_eq!(
            oc(r#"{"type":"session.idle","properties":{"sessionID":"ses_1"}}"#),
            Some(AgentHookEvent::Done)
        );
        assert_eq!(
            oc(
                r#"{"type":"session.error","properties":{"sessionID":"ses_1",
                "error":{"name":"MessageAbortedError","data":{"message":"aborted"}}}}"#
            ),
            Some(AgentHookEvent::Done)
        );
    }

    #[test]
    fn opencode_permission_and_question_block_until_answered() {
        assert_eq!(
            oc(
                r#"{"type":"permission.asked","properties":{"id":"per_1","sessionID":"ses_1",
                "permission":"bash","patterns":["rm -rf build"],"metadata":{},"always":[]}}"#
            ),
            Some(AgentHookEvent::Blocked {
                message: Some("bash".into())
            })
        );
        assert_eq!(
            oc(
                r#"{"type":"question.asked","properties":{"id":"que_1","sessionID":"ses_1",
                "questions":[{"header":"Scope","question":"Which crate?","options":[]}]}}"#
            ),
            Some(AgentHookEvent::Blocked {
                message: Some("Which crate?".into())
            })
        );
        for answered in [
            r#"{"type":"permission.replied","properties":{"sessionID":"ses_1","requestID":"per_1","reply":"once"}}"#,
            r#"{"type":"question.replied","properties":{"sessionID":"ses_1","requestID":"que_1","answers":[]}}"#,
            r#"{"type":"question.rejected","properties":{"sessionID":"ses_1","requestID":"que_1"}}"#,
        ] {
            assert_eq!(
                oc(answered),
                Some(AgentHookEvent::Activity { detail: None }),
                "{answered}"
            );
        }
    }

    #[test]
    fn opencode_irrelevant_or_malformed_events_map_to_nothing() {
        assert_eq!(
            oc(r#"{"type":"message.part.updated","properties":{}}"#),
            None
        );
        assert_eq!(oc(r#"{"type":"session.status","properties":{}}"#), None);
        assert_eq!(oc(r#"{"properties":{"status":{"type":"busy"}}}"#), None);
    }

    /// The plugin forwards exactly the event types the mapper speaks, so a
    /// renamed event cannot silently drop out of one side.
    #[test]
    fn the_opencode_plugin_forwards_what_the_mapper_maps() {
        assert!(OPENCODE_PLUGIN.starts_with(OPENCODE_PLUGIN_MARKER));
        assert!(OPENCODE_PLUGIN.contains("tutti agent-event opencode"));
        assert!(OPENCODE_PLUGIN.contains("process.env.TUTTI_PANE"));
        for (event, sample) in [
            ("session.status", r#"{"status":{"type":"idle"}}"#),
            ("session.idle", "{}"),
            ("session.error", "{}"),
            ("permission.asked", "{}"),
            ("permission.replied", "{}"),
            ("question.asked", "{}"),
            ("question.replied", "{}"),
            ("question.rejected", "{}"),
        ] {
            assert!(
                OPENCODE_PLUGIN.contains(&format!("\"{event}\"")),
                "the plugin forwards {event}"
            );
            let payload = format!(r#"{{"type":"{event}","properties":{sample}}}"#);
            assert!(oc(&payload).is_some(), "the mapper handles {event}");
        }
    }

    #[test]
    fn merge_rejects_non_object_shapes() {
        assert!(merge_claude_hooks(&json!([])).is_err());
        assert!(merge_claude_hooks(&json!({ "hooks": 3 })).is_err());
        assert!(merge_claude_hooks(&json!({ "hooks": { "Stop": {} } })).is_err());
    }
}
