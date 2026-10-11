//! The agent launcher's model: the selectable "what should run here?" rows.
//! No rendering, no app state and no machine probes — which agents are
//! installed, and the argv each runs as there, is the daemon's machine's to
//! say (`host::Launcher`) — so the row construction and the selection
//! arithmetic are unit-tested in isolation.

use std::collections::BTreeMap;
use std::time::SystemTime;

use tutti_agents::{Registry, ResumeSession};

/// What a launcher row runs when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchKind {
    /// Exec this agent — a `PaneRun` carrying its argv: the binary alone for
    /// most (`[claude]`), plus the spec's launch args for one that needs a
    /// subcommand to open its agent (`[ori, code]`).
    Agent(Vec<String>),
    /// The user's login shell — today's default pane.
    Shell,
    /// Prompt for an arbitrary command line, run via `$SHELL -lc <input>`.
    Command,
    /// Resume a harvested conversation — a `PaneRun` carrying the full argv
    /// (e.g. `claude --resume <session-id>`).
    Resume(Vec<String>),
}

/// One row in the launcher overlay: its two-tone label, what it runs, and
/// whether it can be selected — every fixed row can; an agent row only when its
/// binary was found on the daemon's machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherRow {
    pub name: String,
    pub role: String,
    pub kind: LaunchKind,
    pub available: bool,
}

impl LauncherRow {
    /// Whether the row can be launched (an absent agent binary cannot).
    pub fn selectable(&self) -> bool {
        self.available
    }
}

/// Build the launcher rows: the agents `installed` names first (registry
/// order, bold name, the product name as the dim role), each running the argv
/// its machine found it as, then the two fixed rows — the login `shell` and
/// the free-form command entry — then the rest of the catalog dim and
/// unselectable at the foot, each showing its project link so the picker
/// doubles as "what else is out there". Resume rows are inserted between the
/// fixed rows and the catalog by the caller.
pub fn build_rows(
    registry: &Registry,
    installed: &BTreeMap<String, Vec<String>>,
    shell: &str,
) -> Vec<LauncherRow> {
    let argv = |spec: &tutti_agents::AgentSpec| installed.get(&spec.kind.to_string()).cloned();
    let agent_row = |spec: &tutti_agents::AgentSpec, available: bool| {
        let role = if available {
            spec.display.clone()
        } else {
            format!("{} · {}", spec.display, short_url(&spec.url))
        };
        LauncherRow {
            name: spec.kind.to_string(),
            role,
            kind: LaunchKind::Agent(argv(spec).unwrap_or_else(|| spec.argv(None))),
            available,
        }
    };
    let installed = |spec: &&tutti_agents::AgentSpec| argv(spec).is_some();

    let mut rows: Vec<LauncherRow> = registry
        .specs()
        .iter()
        .filter(installed)
        .map(|spec| agent_row(spec, true))
        .collect();
    rows.push(LauncherRow {
        name: "shell".into(),
        role: shell.into(),
        kind: LaunchKind::Shell,
        available: true,
    });
    rows.push(LauncherRow {
        name: "command…".into(),
        role: "type any command".into(),
        kind: LaunchKind::Command,
        available: true,
    });
    rows.extend(
        registry
            .specs()
            .iter()
            .filter(|spec| !installed(spec))
            .map(|spec| agent_row(spec, false)),
    );
    rows
}

/// The rows for a launch into a sandbox: every catalog agent, selectable in
/// registry order — whether an image carries one can't be probed from here —
/// then the fixed rows, which run `sh` there.
pub fn sandbox_rows(registry: &Registry) -> Vec<LauncherRow> {
    registry
        .specs()
        .iter()
        .map(|spec| LauncherRow {
            name: spec.kind.to_string(),
            role: spec.display.clone(),
            kind: LaunchKind::Agent(spec.argv(None)),
            available: true,
        })
        .chain([
            LauncherRow {
                name: "shell".into(),
                role: "sh".into(),
                kind: LaunchKind::Shell,
                available: true,
            },
            LauncherRow {
                name: "command…".into(),
                role: "type any command".into(),
                kind: LaunchKind::Command,
                available: true,
            },
        ])
        .collect()
}

/// The rows of a launcher armed to deploy an issue: the rows of the agents
/// that take a first message (a shell, command or resume row names no agent),
/// still dim when not installed. The pick hands its agent the prompt
/// (`issues::deploy_argv`).
pub fn armed_rows(rows: Vec<LauncherRow>, registry: &Registry) -> Vec<LauncherRow> {
    rows.into_iter()
        .filter(|row| {
            matches!(row.kind, LaunchKind::Agent(_))
                && registry
                    .spec(&row.name.as_str().into())
                    .is_some_and(|spec| spec.prompt_flags.is_some())
        })
        .collect()
}

/// Where the caller inserts rows that must stay above the dim uninstalled
/// catalog (the resume rows): right before the first unavailable row, or the
/// end when everything is installed.
pub fn catalog_start(rows: &[LauncherRow]) -> usize {
    rows.iter().position(|r| !r.available).unwrap_or(rows.len())
}

/// A URL shortened for a dim launcher row: scheme and `www.` stripped.
fn short_url(url: &str) -> &str {
    let url = url.strip_prefix("https://").unwrap_or(url);
    url.strip_prefix("www.").unwrap_or(url)
}

/// Rows for harvested conversations, appended below the fixed rows — the
/// "pick up where you left off" foot of the picker. Each shows its agent, a
/// compact age, and the conversation's first prompt, and is selectable only
/// when its agent's own row is (resuming claude without claude installed
/// cannot work). `now` is passed in so the age labels are testable.
pub fn resume_rows(
    sessions: &[ResumeSession],
    rows: &[LauncherRow],
    now: SystemTime,
) -> Vec<LauncherRow> {
    sessions
        .iter()
        .map(|s| {
            let available = rows.iter().any(|r| r.name == s.agent && r.available);
            let title = s.title.as_deref().unwrap_or("untitled");
            LauncherRow {
                name: "resume".into(),
                role: format!(
                    "{} · {} · {}",
                    s.agent,
                    age_label(s.last_active, now),
                    truncate(title, 28)
                ),
                kind: LaunchKind::Resume(s.cmd.clone()),
                available,
            }
        })
        .collect()
}

/// A compact age for a resume row: `now`, `Nm`, `Nh`, or `Nd`.
fn age_label(then: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(then).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `text` capped to `max` characters, the last one replaced by `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.into();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The first selectable row's index, or 0 when none is (there is always at
/// least the shell row, so this only falls back for an empty list).
pub fn first_selectable(rows: &[LauncherRow]) -> usize {
    rows.iter().position(LauncherRow::selectable).unwrap_or(0)
}

/// The next selectable row after `from`, skipping unavailable rows; stays on
/// `from` when nothing selectable lies ahead.
pub fn next_selectable(rows: &[LauncherRow], from: usize) -> usize {
    let mut i = from;
    while i + 1 < rows.len() {
        i += 1;
        if rows[i].selectable() {
            return i;
        }
    }
    from
}

/// The previous selectable row before `from`, skipping unavailable rows; stays
/// on `from` when nothing selectable lies behind.
pub fn prev_selectable(rows: &[LauncherRow], from: usize) -> usize {
    let mut i = from;
    while i > 0 {
        i -= 1;
        if rows[i].selectable() {
            return i;
        }
    }
    from
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_row(name: &str, available: bool) -> LauncherRow {
        LauncherRow {
            name: name.into(),
            role: name.into(),
            kind: LaunchKind::Agent(vec![name.into()]),
            available,
        }
    }

    /// What a host's launcher answer names installed: each kind with its argv.
    fn installed(argvs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        argvs
            .iter()
            .map(|(kind, argv)| {
                (
                    kind.to_string(),
                    argv.iter().map(|a| a.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn build_rows_lists_agents_then_shell_and_command_with_availability() {
        // Only `claude` is installed; `codex` stays unavailable.
        let rows = build_rows(
            &Registry::default(),
            &installed(&[("claude", &["claude"])]),
            "/bin/zsh",
        );
        // Installed agents lead, the fixed rows follow, and the rest of the
        // catalog sits dim at the foot with its project links.
        assert_eq!(rows[0].name, "claude");
        assert!(rows[0].available, "claude is installed");
        assert_eq!(
            rows[0].role, "Claude Code",
            "an installed row shows the product name"
        );
        assert_eq!(rows[0].kind, LaunchKind::Agent(vec!["claude".into()]));
        assert!(matches!(rows[1].kind, LaunchKind::Shell));
        assert_eq!(
            rows[1].role, "/bin/zsh",
            "the shell row names the host's shell"
        );
        assert!(matches!(rows[2].kind, LaunchKind::Command));
        assert!(
            rows[1].available && rows[2].available,
            "the fixed rows are always selectable"
        );
        assert_eq!(
            catalog_start(&rows),
            3,
            "the dim catalog starts after the fixed rows"
        );
        assert_eq!(
            rows.len(),
            Registry::default().specs().len() + 2,
            "every catalog agent gets a row"
        );
        let codex = rows[3..].iter().find(|r| r.name == "codex").unwrap();
        assert!(!codex.available, "codex is not installed");
        assert_eq!(
            codex.role, "Codex CLI · github.com/openai/codex",
            "an uninstalled row links to the project, scheme stripped"
        );
        assert!(
            rows[3..].iter().all(|r| !r.available),
            "everything after the fixed rows is the uninstalled catalog"
        );
    }

    #[test]
    fn sandbox_rows_offer_the_whole_catalog_then_sh() {
        let registry = Registry::default();
        let rows = sandbox_rows(&registry);
        assert_eq!(rows.len(), registry.specs().len() + 2);
        assert!(
            rows.iter().all(LauncherRow::selectable),
            "nothing can be probed in an image, so nothing is dimmed"
        );
        assert_eq!(rows[0].name, "claude");
        assert_eq!(rows[0].kind, LaunchKind::Agent(vec!["claude".into()]));
        let ori = rows.iter().find(|r| r.name == "ori").unwrap();
        assert_eq!(
            ori.kind,
            LaunchKind::Agent(vec!["ori".into(), "code".into()])
        );
        let shell = &rows[rows.len() - 2];
        assert_eq!(
            (shell.kind.clone(), shell.role.as_str()),
            (LaunchKind::Shell, "sh")
        );
        assert_eq!(rows[rows.len() - 1].kind, LaunchKind::Command);
    }

    /// An installed row runs the argv its machine found it as — Cursor by the
    /// `agent` path checked there — and an uninstalled one the spec's own,
    /// launch args included.
    #[test]
    fn an_agent_row_runs_the_argv_its_machine_found() {
        let cursor = "/home/u/.local/bin/agent";
        let rows = build_rows(
            &Registry::default(),
            &installed(&[("cursor", &[cursor])]),
            "/bin/sh",
        );
        assert_eq!(rows[0].name, "cursor");
        assert_eq!(rows[0].kind, LaunchKind::Agent(vec![cursor.into()]));
        let ori = rows.iter().find(|r| r.name == "ori").unwrap();
        assert!(!ori.available);
        assert_eq!(
            ori.kind,
            LaunchKind::Agent(vec!["ori".into(), "code".into()]),
            "bare `ori` only prints its help; the row opens its agent"
        );
    }

    /// Armed to deploy, the picker keeps only the agents that take a first
    /// message, each with its own argv, an uninstalled one still dim.
    #[test]
    fn armed_rows_keep_the_agents_that_take_a_prompt() {
        let registry = Registry::default();
        let rows = vec![
            agent_row("claude", true),
            agent_row("crush", true),
            agent_row("codex", false),
            LauncherRow {
                name: "shell".into(),
                role: "zsh".into(),
                kind: LaunchKind::Shell,
                available: true,
            },
            LauncherRow {
                name: "resume".into(),
                role: "claude · 2h · old".into(),
                kind: LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "a".into()]),
                available: true,
            },
        ];
        let armed = armed_rows(rows, &registry);
        let names: Vec<&str> = armed.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["claude", "codex"], "crush takes no prompt");
        assert_eq!(armed[0].kind, LaunchKind::Agent(vec!["claude".into()]));
        assert!(armed[0].selectable());
        assert!(!armed[1].selectable(), "codex is not installed");
    }

    #[test]
    fn resume_rows_label_age_and_gate_on_the_agents_availability() {
        use std::time::Duration;
        let now = SystemTime::now();
        let sessions = vec![
            ResumeSession {
                agent: "claude".into(),
                id: "abc".into(),
                title: Some("fix the sidebar filtering because it is way too long".into()),
                last_active: now - Duration::from_secs(2 * 3600),
                cmd: vec!["claude".into(), "--resume".into(), "abc".into()],
            },
            ResumeSession {
                agent: "codex".into(),
                id: "def".into(),
                title: None,
                last_active: now - Duration::from_secs(30),
                cmd: vec!["codex".into(), "resume".into()],
            },
        ];
        let fixed = vec![agent_row("claude", true), agent_row("codex", false)];

        let rows = resume_rows(&sessions, &fixed, now);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "resume");
        assert_eq!(rows[0].role, "claude · 2h · fix the sidebar filtering b…");
        assert_eq!(
            rows[0].kind,
            LaunchKind::Resume(vec!["claude".into(), "--resume".into(), "abc".into()])
        );
        assert!(rows[0].available, "claude is installed, so resumable");
        assert_eq!(rows[1].role, "codex · now · untitled");
        assert!(
            !rows[1].available,
            "a conversation for an absent binary cannot resume"
        );
    }

    #[test]
    fn navigation_skips_unavailable_rows() {
        let rows = vec![
            agent_row("a", true),
            agent_row("b", false),
            agent_row("c", true),
        ];
        assert_eq!(first_selectable(&rows), 0);
        assert_eq!(
            next_selectable(&rows, 0),
            2,
            "j jumps past the unavailable middle row"
        );
        assert_eq!(prev_selectable(&rows, 2), 0, "k jumps back past it");
        assert_eq!(next_selectable(&rows, 2), 2, "stays put at the end");
        assert_eq!(prev_selectable(&rows, 0), 0, "stays put at the start");
    }
}
