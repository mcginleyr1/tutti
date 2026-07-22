//! Claude Code hook integration: map a hook-event JSON object (delivered on the
//! hook command's stdin) to a tutti `AgentHookEvent`, and print the settings.json
//! snippet that wires the events to `tutti agent-event claude`.
//!
//! The mapping reads only the fields tutti reacts to and ignores everything else,
//! so it tolerates Claude Code's evolving hook schema. It never fails: an
//! unrecognized or irrelevant event maps to `None` (send nothing), and the CLI
//! path around it swallows all errors so a hook can never break a Claude session.

use serde_json::{Value, json};
use tutti_core::AgentHookEvent;

/// The command a hook runs; also the addressable verb this module documents.
pub const HOOK_COMMAND: &str = "tutti agent-event claude";

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
}
