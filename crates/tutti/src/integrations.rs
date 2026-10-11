//! The integrations tutti installs: Claude Code's hooks, the opencode plugin,
//! the tutti skill, and the shell integration's line in the user's rc file.
//! `Integration::plan` inspects one's target file and works out what
//! installing would change, with no side effects — cheap enough to run
//! whenever a status is wanted — and `apply` carries a plan out. No stdin or
//! stdout here: the CLI's `--install` flows (and any TUI surface) wrap their
//! own confirmation around it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::hooks;

/// The tutti skill: teaches an agent in a tutti pane to run, wait on, read,
/// answer, review, and merge its own sub-agents. `tutti skill` prints it.
pub const SKILL: &str = include_str!("../../../skills/tutti/SKILL.md");

/// An integration tutti installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integration {
    /// Claude Code hooks in settings.json, running `tutti agent-event claude`.
    ClaudeHooks,
    /// The opencode plugin, piping to `tutti agent-event opencode`.
    OpencodePlugin,
    /// The tutti skill, for Claude Code.
    Skill,
    /// The line loading `tutti shell-init` in the rc file of the user's
    /// `$SHELL`.
    ShellInit,
}

/// A shell `tutti shell-init` speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    /// The snippet `tutti shell-init` prints: inside a tutti pane it reports
    /// the working directory (OSC 7) and marks each prompt and command (OSC
    /// 133); elsewhere it does nothing. Sourcing it twice changes nothing.
    pub fn snippet(self) -> &'static str {
        match self {
            Shell::Zsh => include_str!("shell/tutti.zsh"),
            Shell::Bash => include_str!("shell/tutti.bash"),
            Shell::Fish => include_str!("shell/tutti.fish"),
        }
    }

    /// The line an rc file loads the snippet with: only inside a tutti pane,
    /// so no other shell runs `tutti`, and only while tutti is installed.
    /// Its `if` leaves `$?` at 0 outside tutti.
    pub fn rc_line(self) -> &'static str {
        match self {
            Shell::Zsh => {
                r#"if [ -n "$TUTTI_PANE" ] && command -v tutti >/dev/null; then eval "$(tutti shell-init zsh)"; fi"#
            }
            Shell::Bash => {
                r#"if [ -n "$TUTTI_PANE" ] && command -v tutti >/dev/null; then eval "$(tutti shell-init bash)"; fi"#
            }
            Shell::Fish => {
                "if set -q TUTTI_PANE; and command -q tutti; tutti shell-init fish | source; end"
            }
        }
    }
}

/// The directories install targets resolve against: parameters, not
/// environment reads, so tests (and a caller picking a project) choose them.
#[derive(Clone, Debug, Default)]
pub struct Dirs {
    /// `$HOME`; only the user-config targets need it.
    pub home: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`, which opencode's user plugin dir honours.
    pub xdg_config_home: Option<PathBuf>,
    /// Install into this project instead of the user's config. A relative
    /// path resolves against the cwd — an empty one is the cwd itself.
    pub project: Option<PathBuf>,
    /// `$SHELL`, whose rc file the shell integration goes in.
    pub shell: Option<PathBuf>,
    /// `$ZDOTDIR`, where zsh reads `.zshrc` from instead of `$HOME`.
    pub zdotdir: Option<PathBuf>,
}

impl Dirs {
    /// The user's directories from the environment — `$HOME`,
    /// `$XDG_CONFIG_HOME`, `$SHELL` and `$ZDOTDIR`, each ignored when empty —
    /// and no project.
    pub fn from_env() -> Dirs {
        let var = |name| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Dirs {
            home: var("HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            project: None,
            shell: var("SHELL"),
            zdotdir: var("ZDOTDIR"),
        }
    }

    /// The shell `$SHELL` names, as `tutti shell-init` speaks it.
    fn shell(&self) -> Result<Shell> {
        let shell = self.shell.as_deref().context("SHELL is not set")?;
        let name = shell.file_name().and_then(|name| name.to_str());
        name.and_then(|name| clap::ValueEnum::from_str(name, false).ok())
            .with_context(|| {
                format!(
                    "tutti shell-init speaks zsh, bash and fish, not {}",
                    shell.display()
                )
            })
    }
}

/// What installing an integration would do, as `Integration::plan` found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The file the integration lives in.
    pub path: PathBuf,
    pub status: Status,
    /// The file as inspected (`None`: absent), so `apply` can refuse one that
    /// changed in the meantime.
    found: Option<String>,
}

/// Where an integration stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not installed: installing writes the file, or adds the hooks to it.
    Missing(Change),
    /// Partly there or out of date — an older plugin or skill, some hook
    /// events missing: installing brings it up to date.
    Outdated(Change),
    /// Installed and current.
    Installed,
    /// The file there is not tutti's, so installing is refused.
    Foreign,
    /// The file could not be read or understood: why.
    Unreadable(String),
}

/// What installing writes, and how to show it first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The whole new file.
    pub contents: String,
    /// The change in a phrase: `adding tutti hooks for Stop`, `replacing an
    /// older tutti plugin`.
    pub summary: String,
    /// The change in full, as the CLI shows it before asking: the hooks
    /// before and after, or the file to be written.
    pub preview: String,
}

impl Integration {
    /// Inspect this integration's file under `dirs` and plan its install:
    /// reads that one file, writes nothing. Errs only when the file cannot be
    /// named (no `$HOME` for a user install); a file that cannot be read is a
    /// plan whose status says so.
    pub fn plan(self, dirs: &Dirs) -> Result<Plan> {
        let path = self.path(dirs)?;
        let found = match read(&path) {
            Ok(found) => found,
            Err(e) => {
                let status = Status::Unreadable(format!("reading {}: {e}", path.display()));
                return Ok(Plan {
                    path,
                    status,
                    found: None,
                });
            }
        };
        let status = match self {
            Integration::ClaudeHooks => claude_hooks_status(&path, found.as_deref()),
            Integration::OpencodePlugin => whole_file_status(
                found.as_deref(),
                hooks::OPENCODE_PLUGIN,
                |text| text.starts_with(hooks::OPENCODE_PLUGIN_MARKER),
                "plugin",
            ),
            Integration::Skill => {
                whole_file_status(found.as_deref(), SKILL, is_tutti_skill, "skill")
            }
            Integration::ShellInit => rc_line_status(found.as_deref(), dirs.shell()?.rc_line()),
        };
        Ok(Plan {
            path,
            status,
            found,
        })
    }

    /// Where this integration's file lives under `dirs`.
    fn path(self, dirs: &Dirs) -> Result<PathBuf> {
        let home = || dirs.home.as_deref().context("HOME is not set");
        Ok(match (self, &dirs.project) {
            (Integration::ClaudeHooks, Some(project)) => project.join(".claude/settings.json"),
            (Integration::ClaudeHooks, None) => home()?.join(".claude/settings.json"),
            (Integration::OpencodePlugin, Some(project)) => {
                project.join(".opencode/plugins/tutti.js")
            }
            // Where opencode itself looks: `$XDG_CONFIG_HOME/opencode`, else
            // `~/.config/opencode`.
            (Integration::OpencodePlugin, None) => match &dirs.xdg_config_home {
                Some(config) => config.join("opencode/plugins/tutti.js"),
                None => home()?.join(".config/opencode/plugins/tutti.js"),
            },
            (Integration::Skill, Some(project)) => project.join(".claude/skills/tutti/SKILL.md"),
            (Integration::Skill, None) => home()?.join(".claude/skills/tutti/SKILL.md"),
            (Integration::ShellInit, Some(_)) => {
                bail!("shell integration goes in your own shell's rc file, not a project's")
            }
            // The rc file an interactive, non-login shell — a pane's — reads.
            (Integration::ShellInit, None) => match dirs.shell()? {
                Shell::Zsh => match &dirs.zdotdir {
                    Some(zdotdir) => zdotdir.join(".zshrc"),
                    None => home()?.join(".zshrc"),
                },
                Shell::Bash => home()?.join(".bashrc"),
                Shell::Fish => match &dirs.xdg_config_home {
                    Some(config) => config.join("fish/config.fish"),
                    None => home()?.join(".config/fish/config.fish"),
                },
            },
        })
    }
}

/// Carry out `plan`: create the file's directory, back an existing file up to
/// `<name>.bak`, then write `<name>.tmp` beside it and rename it into place.
/// Refused for a plan with nothing to install, and for one whose file changed
/// since it was inspected (plan again). Returns the backup, when one was made.
pub fn apply(plan: &Plan) -> Result<Option<PathBuf>> {
    let path = &plan.path;
    let (Status::Missing(change) | Status::Outdated(change)) = &plan.status else {
        bail!("nothing to install at {}", path.display());
    };
    let now = read(path).with_context(|| format!("reading {}", path.display()))?;
    if now != plan.found {
        bail!(
            "{} changed since it was inspected; look again",
            path.display()
        );
    }
    let beside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let backup = now.is_some().then(|| beside(".bak"));
    if let Some(backup) = &backup {
        std::fs::copy(path, backup)
            .with_context(|| format!("backing up to {}", backup.display()))?;
    }
    let tmp = beside(".tmp");
    std::fs::write(&tmp, &change.contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(backup)
}

/// Claude Code's settings.json: the tutti hooks merged into whatever else it
/// holds, foreign hooks included. With none of tutti's events wired yet it is
/// `Missing`; with some, `Outdated` (an older install, or a hand-pruned one).
fn claude_hooks_status(path: &Path, found: Option<&str>) -> Status {
    let existing = match found.map(serde_json::from_str::<Value>).transpose() {
        Ok(existing) => existing.unwrap_or(Value::Null),
        Err(e) => return Status::Unreadable(format!("{} is not valid JSON: {e}", path.display())),
    };
    let (merged, added) = match hooks::merge_claude_hooks(&existing) {
        Ok(Some(merge)) => merge,
        Ok(None) => return Status::Installed,
        Err(e) => return Status::Unreadable(format!("{e:#}")),
    };
    let pretty = |v: &Value| serde_json::to_string_pretty(v).expect("settings serialize");
    let before = existing
        .get("hooks")
        .map_or_else(|| "(no hooks yet)".into(), pretty);
    let change = Change {
        contents: pretty(&merged),
        summary: format!("adding tutti hooks for {}", added.join(", ")),
        preview: format!(
            "--- hooks before ---\n{before}\n--- hooks after ---\n{}\n",
            pretty(&merged["hooks"])
        ),
    };
    let events = hooks::claude_hooks_json()["hooks"]
        .as_object()
        .map_or(0, |hooks| hooks.len());
    if added.len() == events {
        Status::Missing(change)
    } else {
        Status::Outdated(change)
    }
}

/// A file tutti owns whole — the plugin, the skill: written fresh, replacing
/// an older one of tutti's (`is_ours`), never anyone else's.
fn whole_file_status(
    found: Option<&str>,
    ours: &str,
    is_ours: fn(&str) -> bool,
    noun: &str,
) -> Status {
    let change = |summary: String| Change {
        contents: ours.to_string(),
        summary,
        preview: format!("--- {noun} ---\n{ours}"),
    };
    match found {
        None => Status::Missing(change(format!("adding the tutti {noun}"))),
        Some(text) if text == ours => Status::Installed,
        Some(text) if is_ours(text) => {
            Status::Outdated(change(format!("replacing an older tutti {noun}")))
        }
        Some(_) => Status::Foreign,
    }
}

/// A shell's rc file, the user's own: `line` appended to whatever it holds,
/// unless a `tutti shell-init` line is there already — tutti's, or one the
/// user wrote.
fn rc_line_status(found: Option<&str>, line: &str) -> Status {
    let found = found.unwrap_or_default();
    if found.contains("tutti shell-init") {
        return Status::Installed;
    }
    let newline = if found.is_empty() || found.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    Status::Missing(Change {
        contents: format!("{found}{newline}{line}\n"),
        summary: "appending the line that loads tutti's shell integration".into(),
        preview: format!("--- appended ---\n{line}\n"),
    })
}

/// Whether a SKILL.md is tutti's: its frontmatter names it `tutti`.
fn is_tutti_skill(text: &str) -> bool {
    let mut lines = text.lines();
    lines.next() == Some("---")
        && lines
            .take_while(|line| *line != "---")
            .any(|line| line.trim() == "name: tutti")
}

/// A file's text, `None` when there is no file.
fn read(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh empty directory to stand in for `$HOME` (no tempdir crate: the
    /// workspace takes no new dependencies).
    fn temp_home() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tutti-integrations-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user(home: &Path) -> Dirs {
        Dirs {
            home: Some(home.to_path_buf()),
            ..Dirs::default()
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn text(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn claude_hooks_go_from_missing_to_installed() {
        let home = temp_home();
        let dirs = user(&home);
        let plan = Integration::ClaudeHooks.plan(&dirs).unwrap();
        assert_eq!(plan.path, home.join(".claude/settings.json"));
        let Status::Missing(change) = &plan.status else {
            panic!("expected missing, got {:?}", plan.status);
        };
        assert!(
            change.summary.starts_with("adding tutti hooks for ")
                && change.summary.contains("Stop"),
            "{}",
            change.summary
        );
        assert!(
            change
                .preview
                .contains("--- hooks before ---\n(no hooks yet)")
        );
        assert!(!plan.path.exists(), "planning writes nothing");

        assert_eq!(apply(&plan).unwrap(), None, "nothing to back up");
        let written: Value = serde_json::from_str(&text(&plan.path)).unwrap();
        assert!(hooks::merge_claude_hooks(&written).unwrap().is_none());
        assert_eq!(
            Integration::ClaudeHooks.plan(&dirs).unwrap().status,
            Status::Installed
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn partial_claude_hooks_update_behind_a_backup_keeping_the_rest() {
        let home = temp_home();
        let path = home.join(".claude/settings.json");
        let (full, _) = hooks::merge_claude_hooks(&serde_json::json!({ "model": "opus" }))
            .unwrap()
            .unwrap();
        let mut partial = full.clone();
        partial["hooks"].as_object_mut().unwrap().remove("Stop");
        let old = serde_json::to_string(&partial).unwrap();
        write(&path, &old);

        let plan = Integration::ClaudeHooks.plan(&user(&home)).unwrap();
        let Status::Outdated(change) = &plan.status else {
            panic!("expected outdated, got {:?}", plan.status);
        };
        assert_eq!(change.summary, "adding tutti hooks for Stop");
        assert_eq!(
            apply(&plan).unwrap(),
            Some(home.join(".claude/settings.json.bak"))
        );
        assert_eq!(text(&home.join(".claude/settings.json.bak")), old);
        let now: Value = serde_json::from_str(&text(&path)).unwrap();
        assert_eq!(
            now, full,
            "the model setting survives, the Stop hook is back"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn settings_tutti_cannot_read_are_never_written() {
        let home = temp_home();
        let path = home.join(".claude/settings.json");
        for (bad, why) in [
            ("{nope", "is not valid JSON"),
            ("[1]", "root is not an object"),
        ] {
            write(&path, bad);
            let plan = Integration::ClaudeHooks.plan(&user(&home)).unwrap();
            assert!(
                matches!(&plan.status, Status::Unreadable(message) if message.contains(why)),
                "{bad}: {:?}",
                plan.status
            );
            assert!(apply(&plan).is_err());
            assert_eq!(text(&path), bad);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_opencode_plugin_installs_replaces_ours_and_refuses_a_foreign_file() {
        let home = temp_home();
        let config = home.join("xdg");
        let dirs = Dirs {
            xdg_config_home: Some(config.clone()),
            ..user(&home)
        };
        let plan = Integration::OpencodePlugin.plan(&dirs).unwrap();
        assert_eq!(plan.path, config.join("opencode/plugins/tutti.js"));
        let Status::Missing(change) = &plan.status else {
            panic!("expected missing, got {:?}", plan.status);
        };
        assert_eq!(change.summary, "adding the tutti plugin");
        apply(&plan).unwrap();
        assert_eq!(text(&plan.path), hooks::OPENCODE_PLUGIN);
        assert_eq!(
            Integration::OpencodePlugin.plan(&dirs).unwrap().status,
            Status::Installed
        );

        let older = "// tutti opencode plugin (older)\n";
        write(&plan.path, older);
        let plan = Integration::OpencodePlugin.plan(&dirs).unwrap();
        let Status::Outdated(change) = &plan.status else {
            panic!("expected outdated, got {:?}", plan.status);
        };
        assert_eq!(change.summary, "replacing an older tutti plugin");
        let backup = apply(&plan).unwrap().unwrap();
        assert_eq!(text(&backup), older);
        assert_eq!(text(&plan.path), hooks::OPENCODE_PLUGIN);

        let mine = "export const Mine = async () => ({})\n";
        write(&plan.path, mine);
        let plan = Integration::OpencodePlugin.plan(&dirs).unwrap();
        assert_eq!(plan.status, Status::Foreign);
        assert!(apply(&plan).is_err());
        assert_eq!(
            text(&plan.path),
            mine,
            "a foreign plugin is never overwritten"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_skill_installs_per_user_or_per_project_and_refuses_a_foreign_one() {
        let home = temp_home();
        let plan = Integration::Skill.plan(&user(&home)).unwrap();
        assert_eq!(plan.path, home.join(".claude/skills/tutti/SKILL.md"));
        assert!(matches!(&plan.status, Status::Missing(change) if change.contents == SKILL));

        // A project install needs no `$HOME` at all.
        let project = Dirs {
            project: Some(home.join("repo")),
            ..Dirs::default()
        };
        let plan = Integration::Skill.plan(&project).unwrap();
        assert_eq!(plan.path, home.join("repo/.claude/skills/tutti/SKILL.md"));
        apply(&plan).unwrap();
        assert_eq!(text(&plan.path), SKILL);
        assert_eq!(
            Integration::Skill.plan(&project).unwrap().status,
            Status::Installed
        );

        let older = "---\nname: tutti\ndescription: an older tutti skill\n---\nold\n";
        write(&plan.path, older);
        let plan = Integration::Skill.plan(&project).unwrap();
        assert!(
            matches!(&plan.status, Status::Outdated(change)
                if change.summary == "replacing an older tutti skill"),
            "{:?}",
            plan.status
        );
        assert_eq!(text(&apply(&plan).unwrap().unwrap()), older);

        let mine = "---\nname: deploy\ndescription: ship it\n---\nname: tutti\n";
        write(&plan.path, mine);
        let plan = Integration::Skill.plan(&project).unwrap();
        assert_eq!(
            plan.status,
            Status::Foreign,
            "only the frontmatter names it"
        );
        assert!(apply(&plan).is_err());
        assert_eq!(text(&plan.path), mine);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_user_install_needs_home_unless_opencode_has_its_config_dir() {
        let err = Integration::ClaudeHooks.plan(&Dirs::default()).unwrap_err();
        assert_eq!(err.to_string(), "HOME is not set");
        assert!(Integration::Skill.plan(&Dirs::default()).is_err());

        let home = temp_home();
        let config = Dirs {
            xdg_config_home: Some(home.clone()),
            ..Dirs::default()
        };
        assert_eq!(
            Integration::OpencodePlugin.plan(&config).unwrap().path,
            home.join("opencode/plugins/tutti.js")
        );
        assert_eq!(
            Integration::OpencodePlugin.plan(&user(&home)).unwrap().path,
            home.join(".config/opencode/plugins/tutti.js"),
            "without it, opencode's own fallback"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn apply_refuses_a_file_that_changed_since_it_was_inspected() {
        let home = temp_home();
        let plan = Integration::Skill.plan(&user(&home)).unwrap();
        let theirs = "---\nname: mine\n---\n";
        write(&plan.path, theirs);
        let err = apply(&plan).unwrap_err();
        assert!(
            err.to_string().contains("changed since it was inspected"),
            "{err:#}"
        );
        assert_eq!(text(&plan.path), theirs);

        let installed = Plan {
            status: Status::Installed,
            ..plan
        };
        assert!(apply(&installed).is_err(), "nothing to install");
        let _ = std::fs::remove_dir_all(&home);
    }

    fn shell_user(home: &Path, shell: &str) -> Dirs {
        Dirs {
            shell: Some(shell.into()),
            ..user(home)
        }
    }

    #[test]
    fn the_shell_integration_targets_the_rc_file_of_the_users_shell() {
        let home = temp_home();
        let path = |dirs: &Dirs| Integration::ShellInit.plan(dirs).unwrap().path;
        assert_eq!(path(&shell_user(&home, "/bin/zsh")), home.join(".zshrc"));
        let zdotdir = Dirs {
            zdotdir: Some(home.join("zsh")),
            ..shell_user(&home, "/bin/zsh")
        };
        assert_eq!(path(&zdotdir), home.join("zsh/.zshrc"));
        assert_eq!(
            path(&shell_user(&home, "/opt/homebrew/bin/bash")),
            home.join(".bashrc")
        );
        assert_eq!(
            path(&shell_user(&home, "/usr/local/bin/fish")),
            home.join(".config/fish/config.fish")
        );
        let xdg = Dirs {
            xdg_config_home: Some(home.join("xdg")),
            ..shell_user(&home, "fish")
        };
        assert_eq!(path(&xdg), home.join("xdg/fish/config.fish"));

        let err = |dirs: &Dirs| Integration::ShellInit.plan(dirs).unwrap_err().to_string();
        assert_eq!(err(&user(&home)), "SHELL is not set");
        assert!(err(&shell_user(&home, "/bin/tcsh")).contains("not /bin/tcsh"));
        let project = Dirs {
            project: Some(home.join("repo")),
            ..shell_user(&home, "/bin/zsh")
        };
        assert!(err(&project).contains("not a project's"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_shell_line_is_appended_once_keeping_the_rc_file() {
        let home = temp_home();
        let dirs = shell_user(&home, "/opt/homebrew/bin/bash");
        let bashrc = home.join(".bashrc");
        let line = Shell::Bash.rc_line();

        let plan = Integration::ShellInit.plan(&dirs).unwrap();
        assert!(
            matches!(&plan.status, Status::Missing(change) if change.contents == format!("{line}\n"))
        );
        write(&bashrc, "alias ll='ls -l'");
        let plan = Integration::ShellInit.plan(&dirs).unwrap();
        assert_eq!(
            apply(&plan).unwrap(),
            Some(home.join(".bashrc.bak")),
            "the old rc file is kept"
        );
        assert_eq!(text(&bashrc), format!("alias ll='ls -l'\n{line}\n"));
        assert_eq!(
            Integration::ShellInit.plan(&dirs).unwrap().status,
            Status::Installed
        );

        // A line of the user's own counts too.
        write(&bashrc, "eval \"$(tutti shell-init bash)\"\n");
        assert_eq!(
            Integration::ShellInit.plan(&dirs).unwrap().status,
            Status::Installed
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every snippet does nothing outside a tutti pane and nothing twice.
    #[test]
    fn each_snippet_is_guarded_by_the_pane_and_against_a_second_load() {
        for shell in [Shell::Zsh, Shell::Bash, Shell::Fish] {
            let guard = shell.snippet().lines().find(|l| l.starts_with("if "));
            assert!(
                guard.is_some_and(|g| g.contains("TUTTI_PANE") && g.contains("__tutti_shell")),
                "{shell:?}: {guard:?}"
            );
            assert!(shell.rc_line().contains("TUTTI_PANE"), "{shell:?}");
        }
    }

    /// The shipped skill is a Claude Code skill tutti recognises as its own:
    /// frontmatter first, naming it `tutti`, with a description short enough
    /// for the skill loader to take.
    #[test]
    fn the_skill_carries_its_frontmatter() {
        assert!(is_tutti_skill(SKILL));
        let frontmatter: Vec<&str> = SKILL
            .lines()
            .skip(1)
            .take_while(|line| *line != "---")
            .collect();
        let description = frontmatter
            .iter()
            .find_map(|line| line.strip_prefix("description: "))
            .expect("a description line");
        assert!(
            description.contains("tutti") && description.len() < 1024,
            "{description}"
        );
        assert!(
            SKILL.lines().count() > frontmatter.len() + 2,
            "a body follows the frontmatter"
        );
    }
}
