//! Agent hook integrations: map a Claude Code, Codex or Gemini CLI hook-event
//! JSON object, or an opencode bus event forwarded by tutti's opencode plugin
//! (each delivered on `tutti agent-event`'s stdin), to a tutti
//! `AgentHookEvent` and the agent's own conversation id; print the hooks
//! snippets that wire each tool's events to `tutti agent-event <agent>` (or,
//! for Claude in a sandbox image, straight to the terminal as OSC 7501), and
//! the opencode plugin that pipes to `tutti agent-event opencode`; and spell a
//! held permission request's answer the way each agent takes it.
//!
//! The mappings read only the fields tutti reacts to and ignore everything else,
//! so they tolerate each tool's evolving schema. They never fail: an
//! unrecognized or irrelevant event maps to `None` (send nothing), and the CLI
//! path around them swallows all errors so a hook can never break a session.

use serde_json::{Value, json};
use tutti_core::{AgentHookEvent, ApprovalDecision};

/// The command a hook runs; also the addressable verb this module documents.
pub const HOOK_COMMAND: &str = "tutti agent-event claude";
/// The command Codex's hooks run.
pub const CODEX_HOOK_COMMAND: &str = "tutti agent-event codex";
/// The command Gemini CLI's hooks run.
pub const GEMINI_HOOK_COMMAND: &str = "tutti agent-event gemini";

/// The opencode plugin `tutti hooks opencode` prints and installs.
pub const OPENCODE_PLUGIN: &str = include_str!("opencode_plugin.js");
/// The plugin's first line: marks a file of the plugin's name as tutti's to
/// replace — anything else there is never overwritten.
pub const OPENCODE_PLUGIN_MARKER: &str = "// tutti opencode plugin";

/// Map one opencode bus event, as the plugin forwards it (`{type,
/// properties}`, and on a root session's idle the plugin's own `reply`), to a
/// tutti `AgentHookEvent` and the root session's id, or `None` when
/// irrelevant. Verified against opencode 1.18.5's event schemas:
/// `session.status` carries `properties.status.type` (`busy` | `retry` |
/// `idle`); `session.idle` and `session.error` end the turn, an idle with the
/// `reply` the plugin fetched (verified live); `permission.asked`
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
    let idle = || AgentHookEvent::Done {
        reply: text("/reply").filter(|reply| !reply.is_empty()),
    };
    let event = match kind {
        "session.status" => match v.pointer("/properties/status/type")?.as_str()? {
            "busy" | "retry" => Some(AgentHookEvent::Activity { detail: None }),
            "idle" => Some(idle()),
            _ => None,
        },
        "session.idle" => Some(idle()),
        "session.error" => Some(AgentHookEvent::Done { reply: None }),
        "permission.asked" => Some(AgentHookEvent::Blocked {
            message: text("/properties/permission"),
            reminder: false,
        }),
        "question.asked" => Some(AgentHookEvent::Blocked {
            message: text("/properties/questions/0/question"),
            reminder: false,
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
/// session's); tool events carry `tool_name`/`tool_input` (the subagent
/// tool's input has `description`/`title`; it is `Agent` since Claude Code
/// 2.1, verified live, `Task` before), `PermissionRequest` among them, fired
/// the moment Claude would show a permission dialog; `Notification` carries
/// `message` and `notification_type`; `UserPromptSubmit` carries the
/// `prompt` and (Claude Code 2.1, verified live) the `prompt_id` every event
/// of its turn shares — a message typed into a running turn keeps it; subagent
/// events carry `agent_id`/`agent_type`; `Stop` carries the turn's final
/// message as `last_assistant_message` (verified live on 2.1.318).
pub fn map_claude_event(v: &Value) -> Option<(AgentHookEvent, Option<String>)> {
    let event = match v.get("hook_event_name")?.as_str()? {
        // Blocked at once — the `permission_prompt` notification only comes
        // seconds later — naming what would run.
        "PermissionRequest" => {
            permission_request(v).map(|(tool, summary)| AgentHookEvent::Blocked {
                message: Some(format!("{tool} · {summary}")),
                reminder: false,
            })
        }
        "PreToolUse" if matches!(tool_name(v), Some("Agent" | "Task")) => {
            let desc = subagent_desc(v);
            Some(AgentHookEvent::SubagentStarted {
                id: desc.clone(),
                desc,
            })
        }
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Some(AgentHookEvent::Activity {
            detail: tool_name(v).map(str::to_string),
        }),
        "SubagentStop" => Some(AgentHookEvent::SubagentStopped { id: subagent_id(v) }),
        // Only a Notification that waits on you blocks (`WAITING`), and one
        // from an older Claude, which names no type. The rest report: the
        // idle reminder (a minute after Stop marked the pane done), auth,
        // elicitation results, background agents, usage-limit resumes, a
        // mod's `$.ui.notify` — and any type tutti does not know yet. A
        // permission dialog's notice comes six seconds into a dialog its
        // PermissionRequest reported.
        "Notification" => match v.get("notification_type").and_then(Value::as_str) {
            Some(kind) if !WAITING.split('|').any(|waiting| waiting == kind) => None,
            kind => Some(AgentHookEvent::Blocked {
                message: v.get("message").and_then(Value::as_str).map(str::to_string),
                reminder: kind == Some("permission_prompt"),
            }),
        },
        "Stop" => Some(AgentHookEvent::Done {
            reply: text(v, "last_assistant_message"),
        }),
        // An API error ended the turn, with no Stop and no reply.
        "StopFailure" => Some(AgentHookEvent::Done { reply: None }),
        "UserPromptSubmit" => Some(AgentHookEvent::Prompt {
            text: v
                .get("prompt")
                .and_then(Value::as_str)
                .and_then(tutti_agents::first_prompt_line),
            id: v
                .get("prompt_id")
                .and_then(Value::as_str)
                .map(str::to_string),
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

/// Map one Codex hook event. Codex's hooks speak Claude Code's schema —
/// observed live from codex-cli 0.166.0: the same `hook_event_name`,
/// `session_id` (the UUID `codex resume` takes), `prompt`, and `tool_name`/
/// `tool_input` (`Bash` with its `command`, a `PermissionRequest`'s
/// `description` beside it), `Stop`'s `last_assistant_message` — so it maps
/// as Claude's does, plus the two events Codex adds: `Interrupt` ends the
/// turn with no reply, as no `Stop` follows an interrupted one, and
/// `SubagentStart` opens a subagent row under its `agent_id`, which its
/// `SubagentStop` names again.
pub fn map_codex_event(v: &Value) -> Option<(AgentHookEvent, Option<String>)> {
    let event = match v.get("hook_event_name")?.as_str()? {
        "Interrupt" => AgentHookEvent::Done { reply: None },
        "SubagentStart" => AgentHookEvent::SubagentStarted {
            id: subagent_id(v),
            desc: text(v, "agent_type").unwrap_or_else(|| "subagent".into()),
        },
        _ => return map_claude_event(v),
    };
    Some((event, text(v, "session_id")))
}

/// Map one Gemini CLI hook event, per its hooks reference (0.66.0): every
/// event carries `hook_event_name` and `session_id` (the UUID `gemini
/// --resume` takes; `SessionStart` observed live); `BeforeAgent` carries the
/// submitted `prompt`, a turn's start; `BeforeTool`/`AfterTool` the
/// `tool_name`; `Notification` a `notification_type` and `message` — only a
/// `ToolPermission` waits on you, so any other type is a report and sends
/// nothing; `AfterAgent` ends the turn, its `prompt_response` the turn's text
/// — every model response of the turn, joined, per Gemini CLI 0.66.0's source,
/// or `[no response text]` for none (not verified live).
pub fn map_gemini_event(v: &Value) -> Option<(AgentHookEvent, Option<String>)> {
    let event = match v.get("hook_event_name")?.as_str()? {
        "SessionStart" => AgentHookEvent::SessionStarted,
        "BeforeAgent" => AgentHookEvent::Prompt {
            text: text(v, "prompt").and_then(|p| tutti_agents::first_prompt_line(&p)),
            id: None,
        },
        "BeforeTool" | "AfterTool" => AgentHookEvent::Activity {
            detail: tool_name(v).map(str::to_string),
        },
        "Notification" if text(v, "notification_type").as_deref() == Some("ToolPermission") => {
            AgentHookEvent::Blocked {
                message: text(v, "message"),
                reminder: false,
            }
        }
        "AfterAgent" => AgentHookEvent::Done {
            reply: text(v, "prompt_response").filter(|reply| reply != "[no response text]"),
        },
        _ => return None,
    };
    Some((event, text(v, "session_id")))
}

/// A non-empty string field of an event.
fn text(v: &Value, field: &str) -> Option<String> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
/// beneath it — else its first string field, through `one_line`.
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
    one_line(
        cwd.and_then(|cwd| text.strip_prefix(cwd)?.strip_prefix('/'))
            .unwrap_or(text),
    )
}

/// `text` on one line, for a held approval: line breaks shown as `⏎`,
/// control characters blanked so nothing reaches the terminal raw, and cut
/// with `…` past 500 characters, so nothing is hidden without a mark.
fn one_line(text: &str) -> String {
    let text: String = text
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

/// What an agent is told of a denial that gave no reason.
const DENIED: &str = "the user denied this in tutti";

/// The `PermissionRequest` hook output carrying `decision` to Claude, or
/// `None` for a pass: the hook prints nothing and Claude shows its own
/// dialog. A denial gives Claude its reason, else says tutti's user denied it.
pub fn permission_decision(decision: &ApprovalDecision) -> Option<Value> {
    let decision = match decision {
        ApprovalDecision::Allow => json!({ "behavior": "allow" }),
        ApprovalDecision::Deny { message } => json!({
            "behavior": "deny",
            "message": message.as_deref().unwrap_or(DENIED),
        }),
        ApprovalDecision::Pass => return None,
    };
    Some(json!({
        "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
    }))
}

/// What an opencode `permission.asked` asks to run: the permission (`bash`,
/// `edit`, …) and its patterns — the command, the paths — on one line
/// (`one_line`). `None` for any other event.
pub fn opencode_permission(v: &Value) -> Option<(String, String)> {
    (v.get("type")?.as_str()? == "permission.asked").then(|| {
        let tool = v.pointer("/properties/permission").and_then(Value::as_str);
        let patterns = v.pointer("/properties/patterns").and_then(Value::as_array);
        let patterns: Vec<&str> = patterns
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        (
            tool.unwrap_or("tool").to_string(),
            one_line(&patterns.join(" ")),
        )
    })
}

/// The reply tutti's opencode plugin posts for `decision` — the body of
/// opencode's `POST /permission/{requestID}/reply` — or `None` for a pass:
/// opencode's own dialog, showing all along, takes it. A denial gives the
/// agent its reason, else says tutti's user denied it.
pub fn opencode_reply(decision: &ApprovalDecision) -> Option<Value> {
    match decision {
        ApprovalDecision::Allow => Some(json!({ "reply": "once" })),
        ApprovalDecision::Deny { message } => Some(json!({
            "reply": "reject",
            "message": message.as_deref().unwrap_or(DENIED),
        })),
        ApprovalDecision::Pass => None,
    }
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
            "PostToolUseFailure": with_matcher,
            "PermissionRequest": with_matcher,
            "SubagentStop": with_matcher,
            "Notification": no_matcher,
            "Stop": no_matcher,
            "StopFailure": no_matcher,
            "UserPromptSubmit": no_matcher,
            "SessionStart": no_matcher,
        }
    })
}

/// The pretty-printed snippet, ready to paste into settings.json.
pub fn claude_hooks_snippet() -> String {
    serde_json::to_string_pretty(&claude_hooks_json()).expect("hooks snippet serializes")
}

/// Codex's hooks.json wiring every relevant event to `tutti agent-event
/// codex`. Tool and subagent events take the match-all matcher; the rest
/// take none (Codex ignores one on prompts, stops and interrupts). An
/// `Interrupt` hook gets 1s unless it asks for more (3s at most): the round
/// trip to the daemon may take up to its 2s deadline.
pub fn codex_hooks_json() -> Value {
    let cmd = json!({ "type": "command", "command": CODEX_HOOK_COMMAND });
    let every_tool = json!([{ "matcher": "*", "hooks": [cmd] }]);
    let every = json!([{ "hooks": [cmd] }]);
    json!({
        "hooks": {
            "SessionStart": every,
            "UserPromptSubmit": every,
            "PreToolUse": every_tool,
            "PostToolUse": every_tool,
            "PermissionRequest": every_tool,
            "SubagentStart": every_tool,
            "SubagentStop": every_tool,
            "Stop": every,
            "Interrupt": [{ "hooks": [{ "type": "command", "command": CODEX_HOOK_COMMAND, "timeout": 3 }] }],
        }
    })
}

/// Gemini CLI's settings.json `hooks` wiring every relevant event to `tutti
/// agent-event gemini`, each named `tutti` for its `/hooks enable|disable`.
pub fn gemini_hooks_json() -> Value {
    let cmd = json!({ "name": "tutti", "type": "command", "command": GEMINI_HOOK_COMMAND });
    let every_tool = json!([{ "matcher": "*", "hooks": [cmd] }]);
    let every = json!([{ "hooks": [cmd] }]);
    json!({
        "hooks": {
            "SessionStart": every,
            "BeforeAgent": every,
            "BeforeTool": every_tool,
            "AfterTool": every_tool,
            "Notification": every,
            "AfterAgent": every,
        }
    })
}

/// The Claude Notification types that wait on you, per the hooks reference:
/// a permission dialog, an MCP form or URL, a background session's or
/// teammate's question, and an Enter after a usage limit reset during sleep
/// (`quota_auto_resume_stale`). Both `agent-event` and the `--tty` hooks'
/// matcher block on these alone.
const WAITING: &str = "permission_prompt|elicitation_dialog|elicitation_url_dialog|\
    agent_needs_input|quota_auto_resume_stale";

/// The settings.json `hooks` snippet for an agent with no route to the daemon
/// — one in a k8s sandbox. Each hook writes an OSC 7501 report straight to its
/// terminal, which `kubectl exec -it` carries back to the pane like any other
/// output, so the image needs only `printf`: a prompt or tool use reports
/// `working`, a waiting Notification `blocked`, Stop (or StopFailure, an API
/// error) `done`, and SessionEnd `clear`. Every command is silent and never fails, and writes nothing to
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
            "PostToolUseFailure": report("working", Some("*")),
            "Notification": report("blocked", Some(WAITING)),
            "Stop": report("done", None),
            "StopFailure": report("done", None),
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
    merge_hooks(settings, &claude_hooks_json(), HOOK_COMMAND)
}

/// `merge_claude_hooks` for any tool whose hooks file nests as Claude's
/// does — `{"hooks": {event: [{matcher?, hooks: [{command}]}]}}`, as Codex's
/// hooks.json and Gemini CLI's settings.json do: `ours` merged in, an event
/// counting as wired once a group of it runs `command`.
pub fn merge_hooks(
    settings: &Value,
    ours: &Value,
    command: &str,
) -> anyhow::Result<Option<(Value, Vec<String>)>> {
    use anyhow::bail;

    let mut merged = match settings {
        Value::Null => json!({}),
        Value::Object(_) => settings.clone(),
        other => bail!("the file's root is not an object: {other}"),
    };
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
        if existing.iter().any(|group| has_tutti_hook(group, command)) {
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

/// Whether a hook group already runs the tutti `command`.
fn has_tutti_hook(group: &Value, command: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks
                .iter()
                .any(|h| h.get("command").and_then(Value::as_str) == Some(command))
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

    /// Claude Code 2.1 calls its subagent tool `Agent` (payload shaped as
    /// 2.1.318 sent it live); its `PostToolUse` stays a heartbeat.
    #[test]
    fn pre_tool_use_agent_becomes_subagent_started() {
        let agent = |event: &str| {
            ev(&json!({
                "session_id": "abc123",
                "hook_event_name": event,
                "tool_name": "Agent",
                "tool_input": { "description": "Touch a.txt", "prompt": "Run touch a.txt" },
            })
            .to_string())
        };
        assert_eq!(
            agent("PreToolUse"),
            Some(AgentHookEvent::SubagentStarted {
                id: "Touch a.txt".into(),
                desc: "Touch a.txt".into(),
            })
        );
        assert_eq!(
            agent("PostToolUse"),
            Some(AgentHookEvent::Activity {
                detail: Some("Agent".into())
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
                message: Some("allow edit?".into()),
                reminder: false,
            })
        );
    }

    fn notification(kind: &str) -> Option<AgentHookEvent> {
        ev(&format!(
            r#"{{ "hook_event_name": "Notification", "message": "m", "notification_type": "{kind}" }}"#
        ))
    }

    /// Each blocks; only a permission dialog's notice is a reminder, its
    /// request reported by the PermissionRequest before it.
    #[test]
    fn notifications_that_wait_on_you_block() {
        for (kind, reminder) in [
            ("permission_prompt", true),
            ("elicitation_dialog", false),
            ("elicitation_url_dialog", false),
            ("agent_needs_input", false),
            ("quota_auto_resume_stale", false),
        ] {
            assert_eq!(
                notification(kind),
                Some(AgentHookEvent::Blocked {
                    message: Some("m".into()),
                    reminder,
                }),
                "{kind}"
            );
        }
    }

    /// Every other type the hooks reference lists reports — a mod's notice
    /// (`plugin_notification`) included — and so does one tutti does not
    /// know yet: none of them marks the pane blocked.
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
            "plugin_notification",
            "some_future_type",
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
                reminder: false,
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
            ..
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

    /// A `Stop` shaped as Claude Code 2.1.318 sent it live (ids and paths
    /// made up) ends the turn with its final message, lines and all.
    #[test]
    fn stop_becomes_done_with_the_final_message() {
        let stop = json!({
            "session_id": "abc123",
            "transcript_path": "/home/u/.claude/projects/-srv-api/abc123.jsonl",
            "cwd": "/srv/api",
            "prompt_id": "0b1c2d3e-4f50-4617-8293-a4b5c6d7e8f9",
            "permission_mode": "auto",
            "effort": { "level": "high" },
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "pong\nping",
            "background_tasks": [],
            "session_crons": [],
        });
        assert_eq!(
            map_claude_event(&stop),
            Some((
                AgentHookEvent::Done {
                    reply: Some("pong\nping".into())
                },
                Some("abc123".into())
            ))
        );
        for none in [
            r#"{ "hook_event_name": "Stop" }"#,
            r#"{ "hook_event_name": "Stop", "last_assistant_message": "" }"#,
            r#"{ "hook_event_name": "Stop", "last_assistant_message": null }"#,
        ] {
            assert_eq!(
                ev(none),
                Some(AgentHookEvent::Done { reply: None }),
                "{none}"
            );
        }
    }

    /// A turn an API error ends sends `StopFailure`, not `Stop`: it ends the
    /// turn all the same, with no reply. A tool that fails sends
    /// `PostToolUseFailure`, not `PostToolUse`: the agent works on.
    #[test]
    fn a_failed_turn_ends_and_a_failed_tool_works_on() {
        assert_eq!(
            ev(r#"{ "hook_event_name": "StopFailure" }"#),
            Some(AgentHookEvent::Done { reply: None })
        );
        assert_eq!(
            ev(r#"{ "hook_event_name": "PostToolUseFailure", "tool_name": "Bash" }"#),
            Some(AgentHookEvent::Activity {
                detail: Some("Bash".into())
            })
        );
    }

    #[test]
    fn a_submitted_prompt_starts_a_turn_labelled_by_its_first_line() {
        let prompt = |text: &str| {
            ev(&json!({ "hook_event_name": "UserPromptSubmit", "prompt": text }).to_string())
        };
        let labelled = |text: Option<String>| Some(AgentHookEvent::Prompt { text, id: None });
        assert_eq!(
            prompt("  fix the failing test  \nit is in tests/server.rs"),
            labelled(Some("fix the failing test".into()))
        );
        assert_eq!(
            prompt(&"x".repeat(200)),
            labelled(Some("x".repeat(80))),
            "a wall of text is capped to a label"
        );
        for injected in [
            "<pasted_content id=\"1\">\nlog\n</pasted_content id=\"1\">\nfix this",
            "<task-notification>done</task-notification>",
            "",
        ] {
            assert_eq!(
                prompt(injected),
                labelled(None),
                "still a turn, but nothing to label it by: {injected:?}"
            );
        }
        assert_eq!(
            ev(r#"{ "hook_event_name": "UserPromptSubmit" }"#),
            labelled(None)
        );
    }

    /// The payload shaped as Claude Code 2.1.318 sent it live: the prompt's
    /// id rides along, so a turn can tell more typed into it from the next.
    #[test]
    fn a_submitted_prompt_names_its_prompt_id() {
        assert_eq!(
            ev(&json!({
                "session_id": "abc123",
                "prompt_id": "0b1c2d3e-4f50-4617-8293-a4b5c6d7e8f9",
                "cwd": "/srv/api",
                "permission_mode": "default",
                "hook_event_name": "UserPromptSubmit",
                "prompt": "then also say hi",
            })
            .to_string()),
            Some(AgentHookEvent::Prompt {
                text: Some("then also say hi".into()),
                id: Some("0b1c2d3e-4f50-4617-8293-a4b5c6d7e8f9".into()),
            })
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
        assert_eq!(out, Some(AgentHookEvent::Done { reply: None }));
    }

    #[test]
    fn snippet_is_valid_json_naming_every_event_and_the_command() {
        let value = claude_hooks_json();
        let hooks = value.get("hooks").unwrap();
        for event in [
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionRequest",
            "SubagentStop",
            "Notification",
            "Stop",
            "StopFailure",
            "UserPromptSubmit",
            "SessionStart",
        ] {
            assert!(hooks.get(event).is_some(), "missing {event}");
        }
        assert_eq!(hooks.as_object().unwrap().len(), 10);
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
            ("PostToolUseFailure", Some("*"), "working"),
            ("Notification", Some(WAITING), "blocked"),
            ("Stop", None, "done"),
            ("StopFailure", None, "done"),
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
        assert_eq!(hooks.len(), 8);
        assert_eq!(
            WAITING.split('|').collect::<Vec<_>>(),
            [
                "permission_prompt",
                "elicitation_dialog",
                "elicitation_url_dialog",
                "agent_needs_input",
                "quota_auto_resume_stale"
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
        assert_eq!(added.len(), 10);
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
            [
                "PermissionRequest",
                "PostToolUseFailure",
                "SessionStart",
                "StopFailure",
                "UserPromptSubmit"
            ]
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
            Some(AgentHookEvent::Done { reply: None })
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
            Some(AgentHookEvent::Done { reply: None })
        );
        assert_eq!(
            oc(
                r#"{"type":"session.error","properties":{"sessionID":"ses_1",
                "error":{"name":"MessageAbortedError","data":{"message":"aborted"}}}}"#
            ),
            Some(AgentHookEvent::Done { reply: None })
        );
    }

    /// The plugin adds the turn's last assistant text to a root session's
    /// idle as `reply`; an error carries none, whatever rides along.
    #[test]
    fn an_opencode_idle_carries_the_reply_the_plugin_fetched() {
        let said = Some(AgentHookEvent::Done {
            reply: Some("pong\nping".into()),
        });
        assert_eq!(
            oc(
                r#"{"type":"session.idle","properties":{"sessionID":"ses_1"},"reply":"pong\nping"}"#
            ),
            said
        );
        assert_eq!(
            oc(
                r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"idle"}},"reply":"pong\nping"}"#
            ),
            said
        );
        assert_eq!(
            oc(r#"{"type":"session.idle","properties":{"sessionID":"ses_1"},"reply":""}"#),
            Some(AgentHookEvent::Done { reply: None })
        );
        assert_eq!(
            oc(r#"{"type":"session.error","properties":{"sessionID":"ses_1"},"reply":"x"}"#),
            Some(AgentHookEvent::Done { reply: None })
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
                message: Some("bash".into()),
                reminder: false,
            })
        );
        assert_eq!(
            oc(
                r#"{"type":"question.asked","properties":{"id":"que_1","sessionID":"ses_1",
                "questions":[{"header":"Scope","question":"Which crate?","options":[]}]}}"#
            ),
            Some(AgentHookEvent::Blocked {
                message: Some("Which crate?".into()),
                reminder: false,
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

    /// A `permission.asked` as opencode 1.18.5 sent it live holds as its
    /// permission over its patterns; the answer goes back as the body of
    /// opencode's permission reply — a pass is opencode's own dialog's.
    #[test]
    fn an_opencode_permission_holds_and_its_answer_is_opencodes_reply() {
        let asked = json!({
            "type": "permission.asked",
            "properties": {
                "id": "per_1", "sessionID": "ses_1", "permission": "bash",
                "patterns": ["touch oc.txt"], "metadata": { "command": "touch oc.txt" },
                "always": ["touch *"], "tool": { "messageID": "msg_1", "callID": "call_1" },
            },
        });
        assert_eq!(
            opencode_permission(&asked),
            Some(("bash".into(), "touch oc.txt".into()))
        );
        let edit = json!({ "type": "permission.asked",
            "properties": { "permission": "edit", "patterns": ["a.rs", "b\nrs"] } });
        assert_eq!(
            opencode_permission(&edit),
            Some(("edit".into(), "a.rs b ⏎ rs".into())),
            "every pattern, on one line"
        );
        assert_eq!(
            opencode_permission(&json!({ "type": "permission.replied", "properties": {} })),
            None
        );
        assert_eq!(
            opencode_reply(&ApprovalDecision::Allow),
            Some(json!({ "reply": "once" }))
        );
        assert_eq!(
            opencode_reply(&ApprovalDecision::Deny {
                message: Some("not on main".into())
            }),
            Some(json!({ "reply": "reject", "message": "not on main" }))
        );
        assert_eq!(
            opencode_reply(&ApprovalDecision::Deny { message: None }),
            Some(json!({ "reply": "reject", "message": "the user denied this in tutti" }))
        );
        assert_eq!(opencode_reply(&ApprovalDecision::Pass), None);
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
        assert!(
            OPENCODE_PLUGIN.contains(r#"url: "/permission/{requestID}/reply""#),
            "the plugin posts a held request's answer as opencode's reply"
        );
        assert!(
            OPENCODE_PLUGIN.contains("payload.reply = await reply(")
                && OPENCODE_PLUGIN.contains("client.session.messages("),
            "an idle carries the reply the plugin fetched, as the mapper reads it"
        );
    }

    /// A Codex hook payload as codex-cli 0.166.0 sends it (observed live; ids
    /// and paths made up), with `fields` on top.
    fn codex(event: &str, fields: Value) -> Option<(AgentHookEvent, Option<String>)> {
        let mut payload = json!({
            "session_id": "01a1a704-0000-7000-8000-000000000001",
            "turn_id": "01a1a708-0000-7000-8000-000000000002",
            "transcript_path": "/home/u/.codex/sessions/2026/11/04/rollout-1.jsonl",
            "cwd": "/srv/api",
            "hook_event_name": event,
            "model": "a-model",
            "permission_mode": "default",
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        map_codex_event(&payload)
    }

    #[test]
    fn codex_events_map_as_claudes_naming_the_session() {
        let id = Some("01a1a704-0000-7000-8000-000000000001".to_string());
        assert_eq!(
            codex("SessionStart", json!({ "source": "startup" })),
            Some((AgentHookEvent::SessionStarted, id.clone()))
        );
        assert_eq!(
            codex(
                "UserPromptSubmit",
                json!({ "prompt": "fix the build\nit is red" })
            ),
            Some((
                AgentHookEvent::Prompt {
                    text: Some("fix the build".into()),
                    id: None,
                },
                id.clone()
            ))
        );
        let bash = json!({ "tool_name": "Bash", "tool_input": { "command": "ls" }, "tool_use_id": "exec-1" });
        let working = Some((
            AgentHookEvent::Activity {
                detail: Some("Bash".into()),
            },
            id.clone(),
        ));
        assert_eq!(codex("PreToolUse", bash.clone()), working);
        let mut ran = bash.clone();
        ran["tool_response"] = json!("notes.txt\n");
        assert_eq!(codex("PostToolUse", ran), working);
        assert_eq!(
            codex(
                "PermissionRequest",
                json!({ "tool_name": "Bash", "tool_input": {
                    "command": "touch hello.txt",
                    "description": "Allow creating hello.txt in the workspace?"
                } })
            ),
            Some((
                AgentHookEvent::Blocked {
                    message: Some("Bash · touch hello.txt".into()),
                    reminder: false,
                },
                id.clone()
            ))
        );
        let stop = json!({ "stop_hook_active": false, "last_assistant_message": "pong\nping" });
        let said = AgentHookEvent::Done {
            reply: Some("pong\nping".into()),
        };
        assert_eq!(codex("Stop", stop), Some((said, id.clone())));
        assert_eq!(
            codex("Stop", json!({ "last_assistant_message": null })),
            Some((AgentHookEvent::Done { reply: None }, id.clone())),
            "Codex names it `string | null`"
        );
        assert_eq!(codex("SessionEnd", json!({ "reason": "other" })), None);
    }

    /// An interrupted turn sends `Interrupt` and never `Stop` (observed
    /// live), so it ends the turn, with no reply.
    #[test]
    fn a_codex_interrupt_ends_the_turn() {
        assert_eq!(
            codex("Interrupt", json!({})).map(|(event, _)| event),
            Some(AgentHookEvent::Done { reply: None })
        );
    }

    #[test]
    fn codex_subagents_open_and_close_a_row_under_their_agent_id() {
        let started = codex(
            "SubagentStart",
            json!({ "agent_id": "agent-7", "agent_type": "explorer" }),
        );
        assert_eq!(
            started.map(|(event, _)| event),
            Some(AgentHookEvent::SubagentStarted {
                id: "agent-7".into(),
                desc: "explorer".into(),
            })
        );
        let stopped = codex(
            "SubagentStop",
            json!({ "agent_id": "agent-7", "agent_type": "explorer", "last_assistant_message": "done" }),
        );
        assert_eq!(
            stopped.map(|(event, _)| event),
            Some(AgentHookEvent::SubagentStopped {
                id: "agent-7".into()
            })
        );
    }

    #[test]
    fn codex_hooks_wire_every_event_to_agent_event_codex() {
        let value = codex_hooks_json();
        let hooks = value["hooks"].as_object().unwrap();
        for (event, matcher) in [
            ("SessionStart", None),
            ("UserPromptSubmit", None),
            ("PreToolUse", Some("*")),
            ("PostToolUse", Some("*")),
            ("PermissionRequest", Some("*")),
            ("SubagentStart", Some("*")),
            ("SubagentStop", Some("*")),
            ("Stop", None),
            ("Interrupt", None),
        ] {
            let group = &hooks[event][0];
            assert_eq!(group["matcher"].as_str(), matcher, "{event}");
            assert_eq!(group["hooks"][0]["command"], CODEX_HOOK_COMMAND, "{event}");
            let payload = json!({ "hook_event_name": event });
            assert!(map_codex_event(&payload).is_some(), "{event} maps");
        }
        assert_eq!(hooks.len(), 9);
        assert_eq!(
            hooks["Interrupt"][0]["hooks"][0]["timeout"], 3,
            "room for the daemon's round trip"
        );

        let (merged, added) = merge_hooks(&json!({}), &value, CODEX_HOOK_COMMAND)
            .unwrap()
            .unwrap();
        assert_eq!(added.len(), 9);
        assert!(
            merge_hooks(&merged, &value, CODEX_HOOK_COMMAND)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            merge_claude_hooks(&merged).unwrap().unwrap().1.len(),
            10,
            "Codex's hooks are not Claude's"
        );
    }

    /// A Gemini CLI hook payload: the base fields its hooks reference lists
    /// (and its `SessionStart` sent live, ids and paths made up), with
    /// `fields` on top.
    fn gemini(event: &str, fields: Value) -> Option<(AgentHookEvent, Option<String>)> {
        let mut payload = json!({
            "session_id": "a864e70e-0000-4000-8000-000000000001",
            "transcript_path": "/home/u/.gemini/tmp/api/chats/session-1.jsonl",
            "cwd": "/srv/api",
            "hook_event_name": event,
            "timestamp": "2026-11-04T14:51:39.343Z",
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        map_gemini_event(&payload)
    }

    #[test]
    fn gemini_events_map_onto_a_turn_naming_the_session() {
        let id = Some("a864e70e-0000-4000-8000-000000000001".to_string());
        assert_eq!(
            gemini("SessionStart", json!({ "source": "startup" })),
            Some((AgentHookEvent::SessionStarted, id.clone()))
        );
        assert_eq!(
            gemini(
                "BeforeAgent",
                json!({ "prompt": "fix the build\nit is red" })
            ),
            Some((
                AgentHookEvent::Prompt {
                    text: Some("fix the build".into()),
                    id: None,
                },
                id.clone()
            ))
        );
        let shell = json!({ "tool_name": "run_shell_command", "tool_input": { "command": "ls" } });
        let working = Some((
            AgentHookEvent::Activity {
                detail: Some("run_shell_command".into()),
            },
            id.clone(),
        ));
        assert_eq!(gemini("BeforeTool", shell.clone()), working);
        let mut ran = shell.clone();
        ran["tool_response"] = json!({ "llmContent": "notes.txt", "returnDisplay": "notes.txt" });
        assert_eq!(gemini("AfterTool", ran), working);
        assert_eq!(
            gemini(
                "Notification",
                json!({
                    "notification_type": "ToolPermission",
                    "message": "Allow run_shell_command: rm -rf build?",
                    "details": { "tool_name": "run_shell_command" }
                })
            ),
            Some((
                AgentHookEvent::Blocked {
                    message: Some("Allow run_shell_command: rm -rf build?".into()),
                    reminder: false,
                },
                id.clone()
            ))
        );
        let answered =
            json!({ "prompt": "fix it", "prompt_response": "done", "stop_hook_active": false });
        let said = AgentHookEvent::Done {
            reply: Some("done".into()),
        };
        assert_eq!(gemini("AfterAgent", answered), Some((said, id.clone())));
        let silent = json!({ "prompt": "fix it", "prompt_response": "[no response text]" });
        assert_eq!(
            gemini("AfterAgent", silent),
            Some((AgentHookEvent::Done { reply: None }, id.clone())),
            "Gemini's stand-in for a turn with no text is no reply"
        );
        for quiet in [
            "SessionEnd",
            "BeforeModel",
            "AfterModel",
            "BeforeToolSelection",
            "PreCompress",
        ] {
            assert_eq!(gemini(quiet, json!({})), None, "{quiet}");
        }
    }

    /// Only a tool permission waits on you; any other notification type —
    /// one Gemini adds later, or none named — is a report and sends nothing.
    #[test]
    fn a_gemini_notification_blocks_only_for_a_tool_permission() {
        for report in [
            json!({ "notification_type": "ExtensionUpdate", "message": "an update is ready" }),
            json!({ "message": "no type named" }),
        ] {
            assert_eq!(gemini("Notification", report.clone()), None, "{report}");
        }
    }

    #[test]
    fn gemini_hooks_wire_every_event_to_agent_event_gemini() {
        let value = gemini_hooks_json();
        let hooks = value["hooks"].as_object().unwrap();
        for (event, matcher) in [
            ("SessionStart", None),
            ("BeforeAgent", None),
            ("BeforeTool", Some("*")),
            ("AfterTool", Some("*")),
            ("Notification", None),
            ("AfterAgent", None),
        ] {
            let group = &hooks[event][0];
            assert_eq!(group["matcher"].as_str(), matcher, "{event}");
            assert_eq!(
                group["hooks"][0],
                json!({ "name": "tutti", "type": "command", "command": GEMINI_HOOK_COMMAND }),
                "{event}"
            );
            let payload =
                json!({ "hook_event_name": event, "notification_type": "ToolPermission" });
            assert!(map_gemini_event(&payload).is_some(), "{event} maps");
        }
        assert_eq!(hooks.len(), 6);
        let settings = json!({ "theme": "GitHub", "hooksConfig": { "disabled": ["lint"] } });
        let (merged, added) = merge_hooks(&settings, &value, GEMINI_HOOK_COMMAND)
            .unwrap()
            .unwrap();
        assert_eq!(added.len(), 6);
        assert_eq!(merged["theme"], "GitHub");
        assert_eq!(merged["hooksConfig"], settings["hooksConfig"]);
        assert!(
            merge_hooks(&merged, &value, GEMINI_HOOK_COMMAND)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn merge_rejects_non_object_shapes() {
        assert!(merge_claude_hooks(&json!([])).is_err());
        assert!(merge_claude_hooks(&json!({ "hooks": 3 })).is_err());
        assert!(merge_claude_hooks(&json!({ "hooks": { "Stop": {} } })).is_err());
    }
}
