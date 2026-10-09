//! Agent hook integrations: map a Claude Code hook-event JSON object, or an
//! opencode bus event forwarded by tutti's opencode plugin (each delivered on
//! `tutti agent-event`'s stdin), to a tutti `AgentHookEvent`; print the
//! settings.json snippet that wires Claude Code's events to `tutti agent-event
//! claude`, and the opencode plugin that pipes to `tutti agent-event opencode`.
//!
//! The mappings read only the fields tutti reacts to and ignore everything else,
//! so they tolerate each tool's evolving schema. They never fail: an
//! unrecognized or irrelevant event maps to `None` (send nothing), and the CLI
//! path around them swallows all errors so a hook can never break a session.

use serde_json::{Value, json};
use tutti_core::AgentHookEvent;

/// The command a hook runs; also the addressable verb this module documents.
pub const HOOK_COMMAND: &str = "tutti agent-event claude";

/// The opencode plugin `tutti hooks opencode` prints and installs.
pub const OPENCODE_PLUGIN: &str = include_str!("opencode_plugin.js");
/// The plugin's first line: marks a file of the plugin's name as tutti's to
/// replace — anything else there is never overwritten.
pub const OPENCODE_PLUGIN_MARKER: &str = "// tutti opencode plugin";

/// Map one opencode bus event, as the plugin forwards it (`{type,
/// properties}`), to a tutti `AgentHookEvent`, or `None` when irrelevant.
/// Verified against opencode 1.18.5's event schemas: `session.status` carries
/// `properties.status.type` (`busy` | `retry` | `idle`); `session.idle` and
/// `session.error` end the turn; `permission.asked` names its `permission`,
/// `question.asked` its `questions[].question`; a `*.replied` or
/// `question.rejected` means you answered and the agent goes on.
pub fn map_opencode_event(v: &Value) -> Option<AgentHookEvent> {
    let text = |pointer: &str| {
        v.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match v.get("type")?.as_str()? {
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
    }
}

/// Map one Claude Code hook event to a tutti `AgentHookEvent`, or `None` when the
/// event is irrelevant (nothing is sent). Verified against the hooks reference:
/// events carry `hook_event_name`; tool events carry `tool_name`/`tool_input`
/// (the `Task` tool's input has `description`/`title`); `Notification` carries
/// `message`; subagent events carry `agent_id`/`agent_type`.
pub fn map_claude_event(v: &Value) -> Option<AgentHookEvent> {
    match v.get("hook_event_name")?.as_str()? {
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
        "Notification" => Some(AgentHookEvent::Blocked {
            message: v.get("message").and_then(Value::as_str).map(str::to_string),
        }),
        "Stop" => Some(AgentHookEvent::Done),
        _ => None,
    }
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

/// The settings.json `hooks` snippet wiring every relevant event to
/// `tutti agent-event claude`. Built as JSON so it is always valid.
pub fn claude_hooks_json() -> Value {
    // Tool + subagent events filter on the tool/agent (matcher "*" = all); the
    // per-event mapping decides what each one means. Notification/Stop take no
    // matcher.
    let cmd = json!({ "type": "command", "command": HOOK_COMMAND });
    let with_matcher = json!([{ "matcher": "*", "hooks": [cmd] }]);
    let no_matcher = json!([{ "hooks": [cmd] }]);
    json!({
        "hooks": {
            "PreToolUse": with_matcher,
            "PostToolUse": with_matcher,
            "SubagentStop": with_matcher,
            "Notification": no_matcher,
            "Stop": no_matcher,
        }
    })
}

/// The pretty-printed snippet, ready to paste into settings.json.
pub fn claude_hooks_snippet() -> String {
    serde_json::to_string_pretty(&claude_hooks_json()).expect("hooks snippet serializes")
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
        map_claude_event(&serde_json::from_str(json).unwrap())
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
    fn notification_becomes_blocked_with_message() {
        assert_eq!(
            ev(r#"{ "hook_event_name": "Notification", "message": "allow edit?" }"#),
            Some(AgentHookEvent::Blocked {
                message: Some("allow edit?".into())
            })
        );
    }

    #[test]
    fn stop_becomes_done() {
        assert_eq!(
            ev(r#"{ "hook_event_name": "Stop", "last_assistant_message": "all set" }"#),
            Some(AgentHookEvent::Done)
        );
    }

    #[test]
    fn irrelevant_or_malformed_events_map_to_nothing() {
        assert_eq!(ev(r#"{ "hook_event_name": "SessionStart" }"#), None);
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
            "SubagentStop",
            "Notification",
            "Stop",
        ] {
            assert!(hooks.get(event).is_some(), "missing {event}");
        }
        assert!(claude_hooks_snippet().contains(HOOK_COMMAND));
    }

    #[test]
    fn merge_installs_into_empty_settings() {
        let (merged, added) = merge_claude_hooks(&json!({})).unwrap().unwrap();
        assert_eq!(added.len(), 5);
        assert!(merged["hooks"]["Stop"][0]["hooks"][0]["command"] == HOOK_COMMAND);
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
        map_opencode_event(&serde_json::from_str(json).unwrap())
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
