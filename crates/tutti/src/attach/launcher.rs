//! The agent launcher's model: the selectable "what should run here?" rows and
//! a pure PATH probe for agent availability. No rendering and no app state, so
//! the row construction, the availability check, and the selection arithmetic
//! are unit-tested in isolation.

use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

use tutti_agents::{Registry, ResumeSession};

/// What a launcher row runs when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchKind {
    /// Exec this agent binary directly — a `PaneRun` carrying `[name]`.
    Agent(String),
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
/// binary was found on PATH.
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

/// Build the launcher rows: the **installed** agents first (registry order,
/// bold name, the product name as the dim role), then the two fixed rows —
/// the login shell and the free-form command entry — then the rest of the
/// catalog dim and unselectable at the foot, each showing its project link so
/// the picker doubles as "what else is out there". Resume rows are inserted
/// between the fixed rows and the catalog by the caller.
pub fn build_rows(registry: &Registry, path: Option<&OsStr>) -> Vec<LauncherRow> {
    let agent_row = |spec: &tutti_agents::AgentSpec, available: bool| {
        let name = spec.kind.to_string();
        let command = spec
            .process_names
            .first()
            .cloned()
            .unwrap_or_else(|| name.clone());
        let role = if available {
            spec.display.clone()
        } else {
            format!("{} · {}", spec.display, short_url(&spec.url))
        };
        LauncherRow {
            name,
            role,
            kind: LaunchKind::Agent(command),
            available,
        }
    };
    let installed =
        |spec: &&tutti_agents::AgentSpec| spec.process_names.iter().any(|n| on_path(n, path));

    let mut rows: Vec<LauncherRow> = registry
        .specs()
        .iter()
        .filter(installed)
        .map(|spec| agent_row(spec, true))
        .collect();
    rows.push(LauncherRow {
        name: "shell".into(),
        role: login_shell(),
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
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.into();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The user's login shell, `$SHELL` or `/bin/sh`. Shared with the app so the
/// shell row and the shell fallback exec the same program.
pub fn login_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// Whether `name` resolves to an executable file on `path` — the `PATH` value,
/// os-split. Pure over the passed value (it reads no global env), so a test can
/// point it at a fake PATH; an unset (`None`) path finds nothing.
pub fn on_path(name: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| is_executable(&dir.join(name)))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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
    use std::path::PathBuf;

    fn temp_dir_unique() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tutti-launcher-{}-{n}", std::process::id()))
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn agent_row(name: &str, available: bool) -> LauncherRow {
        LauncherRow {
            name: name.into(),
            role: name.into(),
            kind: LaunchKind::Agent(name.into()),
            available,
        }
    }

    #[test]
    fn on_path_finds_only_executables_present_on_the_fake_path() {
        let dir = temp_dir_unique();
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("myagent");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        make_executable(&bin);
        // A present-but-not-executable file must not count.
        let plain = dir.join("plainfile");
        std::fs::write(&plain, b"x").unwrap();
        let path = std::ffi::OsString::from(&dir);

        assert!(on_path("myagent", Some(path.as_os_str())));
        assert!(!on_path("missing", Some(path.as_os_str())));
        assert!(!on_path("plainfile", Some(path.as_os_str())));
        assert!(!on_path("myagent", None), "an unset PATH finds nothing");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_rows_lists_agents_then_shell_and_command_with_availability() {
        let dir = temp_dir_unique();
        std::fs::create_dir_all(&dir).unwrap();
        // Only `claude` is on the fake PATH; `codex` stays unavailable.
        let bin = dir.join("claude");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        make_executable(&bin);
        let path = std::ffi::OsString::from(&dir);

        let rows = build_rows(&Registry::default(), Some(path.as_os_str()));
        // Installed agents lead, the fixed rows follow, and the rest of the
        // catalog sits dim at the foot with its project links.
        assert_eq!(rows[0].name, "claude");
        assert!(rows[0].available, "claude is on the fake PATH");
        assert_eq!(
            rows[0].role, "Claude Code",
            "an installed row shows the product name"
        );
        assert_eq!(rows[0].kind, LaunchKind::Agent("claude".into()));
        assert!(matches!(rows[1].kind, LaunchKind::Shell));
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
        assert!(!codex.available, "codex is absent from the fake PATH");
        assert_eq!(
            codex.role, "Codex CLI · github.com/openai/codex",
            "an uninstalled row links to the project, scheme stripped"
        );
        assert!(
            rows[3..].iter().all(|r| !r.available),
            "everything after the fixed rows is the uninstalled catalog"
        );

        std::fs::remove_dir_all(&dir).ok();
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
