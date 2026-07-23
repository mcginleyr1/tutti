//! The agent launcher's model: the selectable "what should run here?" rows and
//! a pure PATH probe for agent availability. No rendering and no app state, so
//! the row construction, the availability check, and the selection arithmetic
//! are unit-tested in isolation.

use std::ffi::OsStr;
use std::path::Path;

use tutti_agents::Registry;

/// What a launcher row runs when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchKind {
    /// Exec this agent binary directly — a `PaneRun` carrying `[name]`.
    Agent(String),
    /// The user's login shell — today's default pane.
    Shell,
    /// Prompt for an arbitrary command line, run via `$SHELL -lc <input>`.
    Command,
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

/// Build the launcher rows: one per registry agent spec (bold name, dim role,
/// dimmed and unselectable when its binary is absent from `path`), then the two
/// fixed rows — the login shell and the free-form command entry.
pub fn build_rows(registry: &Registry, path: Option<&OsStr>) -> Vec<LauncherRow> {
    let mut rows: Vec<LauncherRow> = registry
        .specs()
        .iter()
        .map(|spec| {
            let name = spec.kind.to_string();
            let available = spec.process_names.iter().any(|n| on_path(n, path));
            let command = spec
                .process_names
                .first()
                .cloned()
                .unwrap_or_else(|| name.clone());
            LauncherRow {
                role: role_label(&name),
                name,
                kind: LaunchKind::Agent(command),
                available,
            }
        })
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
    rows
}

/// A human one-line role for an agent kind, prettifying the known agents and
/// falling back to the bare kind for a config-added one.
fn role_label(kind: &str) -> String {
    match kind {
        "claude" => "Claude Code",
        "codex" => "Codex",
        other => other,
    }
    .to_string()
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
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["claude", "codex", "shell", "command…"]);
        assert!(rows[0].available, "claude is on the fake PATH");
        assert!(!rows[1].available, "codex is absent");
        assert_eq!(rows[0].role, "Claude Code", "the agent role is prettified");
        assert_eq!(rows[0].kind, LaunchKind::Agent("claude".into()));
        assert!(
            rows[2].available && rows[3].available,
            "the fixed rows are always selectable"
        );
        assert!(matches!(rows[2].kind, LaunchKind::Shell));
        assert!(matches!(rows[3].kind, LaunchKind::Command));

        std::fs::remove_dir_all(&dir).ok();
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
