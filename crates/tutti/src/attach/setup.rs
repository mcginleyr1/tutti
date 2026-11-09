//! The setup panel (`Mode::Integrations`): the agent integrations tutti
//! ships, each planned against the disk and installed on a confirm, for the
//! user or into the active project.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::app::{App, Mode};
use crate::integrations::{Integration, Plan, Status};

/// The setup panel's rows, top to bottom: each integration tutti installs and
/// the name it goes by there.
const SETUP: [(Integration, &str); 6] = [
    (Integration::ClaudeHooks, "Claude Code hooks"),
    (Integration::CodexHooks, "Codex hooks"),
    (Integration::GeminiHooks, "Gemini CLI hooks"),
    (Integration::OpencodePlugin, "opencode plugin"),
    (Integration::Skill, "tutti skill"),
    (Integration::ShellInit, "shell integration"),
];

/// The setup panel's state, made afresh each time the panel opens: it means
/// something only in `Mode::Integrations`.
#[derive(Default)]
pub(super) struct Setup {
    /// The project the setup panel installs into, as its name and directory,
    /// while `s` has switched it there; `None` installs for the user.
    project: Option<(String, PathBuf)>,
    /// The setup panel's plans, one per `SETUP` row: made on opening, on a
    /// scope switch, and after every install, so a row shows the disk.
    plans: Vec<Result<Plan, String>>,
    /// The highlighted setup row.
    pub(super) selected: usize,
    /// Whether the setup panel is asking to install the highlighted row.
    confirm: bool,
}

impl App {
    /// Open the setup panel on the user scope, every row planned afresh.
    pub(super) fn open_setup(&mut self) {
        self.mode = Mode::Integrations;
        self.setup = Setup::default();
        self.replan_setup();
    }

    /// Plan every setup row for the current scope. A plan reads one file and
    /// writes nothing, so this runs whenever a row may have changed.
    fn replan_setup(&mut self) {
        let project = self.setup.project.as_ref().map(|(_, dir)| dir.clone());
        let integrations = SETUP.iter().map(|(i, _)| *i).collect();
        self.setup.plans = self
            .host
            .setup_plans(integrations, project)
            .unwrap_or_else(|e| vec![Err(format!("{e:#}")); SETUP.len()]);
    }

    /// Keys in the setup panel: `j`/`k` (or the arrows) move, `s` switches
    /// between the user's config and the active project's, enter on a row
    /// with something to install asks first, and `esc` closes. While asking,
    /// `y` installs and any other key backs out to the list.
    pub(super) fn on_key_setup(&mut self, key: KeyEvent) {
        if std::mem::take(&mut self.setup.confirm) {
            if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                self.install_selected();
            }
            return;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.setup.selected = (self.setup.selected + 1).min(SETUP.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.setup.selected = self.setup.selected.saturating_sub(1);
            }
            KeyCode::Char('s') => self.toggle_setup_scope(),
            KeyCode::Enter => self.confirm_setup(),
            KeyCode::Esc => self.mode = Mode::Terminal,
            _ => {}
        }
    }

    /// Switch the setup panel between the user's config and the active
    /// project's, and plan again there. With no project open it stays put.
    fn toggle_setup_scope(&mut self) {
        if self.setup.project.take().is_none() {
            let Some(w) = self.active_workspace() else {
                self.set_status("no project open to install into".into());
                return;
            };
            self.setup.project = Some((w.name.clone(), w.dir.clone()));
        }
        self.replan_setup();
    }

    /// Enter on a setup row: one with something to install asks first; the
    /// rest flash why there is nothing to do — a refusal in red.
    fn confirm_setup(&mut self) {
        let label = SETUP[self.setup.selected].1;
        let message = match &self.setup.plans[self.setup.selected] {
            Ok(plan) => match &plan.status {
                Status::Missing(_) | Status::Outdated(_) => {
                    self.setup.confirm = true;
                    return;
                }
                Status::Installed => format!("{label} already installed"),
                Status::Foreign => format!(
                    "error: {} is not tutti's; refusing to overwrite it",
                    plan.path.display()
                ),
                Status::Unreadable(why) => format!("error: {why}"),
            },
            Err(e) => format!("error: {e:#}"),
        };
        self.set_status(message);
    }

    /// `y` at the setup confirm: install the highlighted row's plan and flash
    /// where it went — and where the file it replaced was backed up — or why
    /// it was refused, then plan again so the row shows the disk as it is.
    fn install_selected(&mut self) {
        let label = SETUP[self.setup.selected].1;
        if let Ok(plan) = &self.setup.plans[self.setup.selected] {
            let path = plan.path.display();
            let message = match self.host.setup_apply(plan) {
                Ok(None) => format!("{label} installed into {path}"),
                Ok(Some(backup)) => format!(
                    "{label} installed into {path} · backed up the old one to {}",
                    backup.display()
                ),
                Err(e) => format!("error: {e:#}"),
            };
            self.set_status(message);
        }
        self.replan_setup();
    }

    /// The setup panel's rows — each integration's name and plan — for the
    /// renderer.
    pub fn setup_rows(&self) -> impl Iterator<Item = (&'static str, &Result<Plan, String>)> {
        SETUP.iter().map(|(_, label)| *label).zip(&self.setup.plans)
    }

    /// Whether the setup panel is asking to install the highlighted row.
    pub fn setup_confirming(&self) -> bool {
        self.setup.confirm
    }

    /// The name of the project the setup panel installs into; `None` while it
    /// installs for the user.
    pub fn setup_project(&self) -> Option<&str> {
        self.setup.project.as_ref().map(|(name, _)| name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::*;
    use crate::config::Config;
    use crate::integrations::{self, Dirs};
    use ratatui::crossterm::event::KeyModifiers;

    /// A fresh empty directory standing in for `$HOME` or a project, so the
    /// setup panel never reads or writes the real config.
    fn fresh_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tutti-app-setup-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `view_one_pane` (workspace `w`) attached under `config` with the setup
    /// panel's user scope on a fixture home and the workspace on a fixture
    /// project dir, then the panel opened with prefix `i`. Returns the app,
    /// the home, and the project.
    fn setup_fixture(config: Config) -> (App, PathBuf, PathBuf) {
        let (home, project) = (fresh_dir(), fresh_dir());
        let mut app = App::with_config(config);
        app.set_local(|local| {
            local.dirs = Dirs {
                home: Some(home.clone()),
                shell: Some("/bin/zsh".into()),
                ..Dirs::default()
            }
        });
        let mut view = view_one_pane();
        view[0].dir = project.clone();
        attach_with(&mut app, view);
        app.on_key(ctrl('b'));
        app.on_key(plain('i'));
        (app, home, project)
    }

    fn setup_status(app: &App, row: usize) -> Status {
        app.setup.plans[row].as_ref().unwrap().status.clone()
    }

    /// The target of every row that could be planned.
    fn setup_paths(app: &App) -> Vec<PathBuf> {
        app.setup_rows()
            .filter_map(|(_, plan)| Some(plan.as_ref().ok()?.path.clone()))
            .collect()
    }

    #[test]
    fn prefix_i_opens_the_setup_panel_in_both_presets_and_esc_closes_it() {
        for config in [Config::default(), vim_config()] {
            let (mut app, home, project) = setup_fixture(config);
            assert_eq!(app.mode, Mode::Integrations);
            let labels: Vec<&str> = app.setup_rows().map(|(label, _)| label).collect();
            assert_eq!(
                labels,
                [
                    "Claude Code hooks",
                    "Codex hooks",
                    "Gemini CLI hooks",
                    "opencode plugin",
                    "tutti skill",
                    "shell integration"
                ]
            );
            assert_eq!(
                setup_paths(&app),
                [
                    home.join(".claude/settings.json"),
                    home.join(".codex/hooks.json"),
                    home.join(".gemini/settings.json"),
                    home.join(".config/opencode/plugins/tutti.js"),
                    home.join(".claude/skills/tutti/SKILL.md"),
                    home.join(".zshrc"),
                ],
                "the user scope plans against the home"
            );
            assert!((0..6).all(|row| matches!(setup_status(&app, row), Status::Missing(_))));

            let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            assert_eq!(app.on_key(esc), Vec::new());
            assert_eq!(app.mode, Mode::Terminal);
            let _ = std::fs::remove_dir_all(&home);
            let _ = std::fs::remove_dir_all(&project);
        }
    }

    #[test]
    fn enter_then_y_installs_into_the_home_and_the_row_replans_to_installed() {
        let (mut app, home, project) = setup_fixture(Config::default());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let settings = home.join(".claude/settings.json");
        assert_eq!(app.on_key(enter), Vec::new());
        assert!(app.setup_confirming(), "enter asks first");
        assert!(!settings.exists(), "asking writes nothing");

        assert_eq!(
            app.on_key(plain('y')),
            Vec::new(),
            "the daemon hears nothing"
        );
        assert!(!app.setup_confirming());
        assert_eq!(
            app.mode,
            Mode::Integrations,
            "the panel stays on the result"
        );
        assert!(
            std::fs::read_to_string(&settings)
                .unwrap()
                .contains(crate::hooks::HOOK_COMMAND)
        );
        assert_eq!(setup_status(&app, 0), Status::Installed, "planned again");
        assert!(
            app.transient().is_some_and(
                |t| t == format!("Claude Code hooks installed into {}", settings.display())
            ),
            "{:?}",
            app.transient()
        );

        app.on_key(enter);
        assert!(!app.setup_confirming(), "nothing left to install");
        assert_eq!(app.transient(), Some("Claude Code hooks already installed"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn the_codex_and_gemini_rows_install_their_hooks_into_the_home() {
        let (mut app, home, project) = setup_fixture(Config::default());
        for (row, file, command) in [
            (1, ".codex/hooks.json", crate::hooks::CODEX_HOOK_COMMAND),
            (
                2,
                ".gemini/settings.json",
                crate::hooks::GEMINI_HOOK_COMMAND,
            ),
        ] {
            app.on_key(plain('j'));
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(app.setup_confirming(), "{file}: enter asks first");
            app.on_key(plain('y'));
            let written = std::fs::read_to_string(home.join(file)).unwrap();
            assert!(written.contains(command), "{file}: {written}");
            assert_eq!(setup_status(&app, row), Status::Installed, "{file}");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn updating_an_older_install_flashes_where_the_old_file_went() {
        let (mut app, home, project) = setup_fixture(Config::default());
        let skill = home.join(".claude/skills/tutti/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "---\nname: tutti\n---\nold\n").unwrap();
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_key(ctrl('b'));
        app.on_key(plain('i'));
        assert!(
            matches!(setup_status(&app, 4), Status::Outdated(_)),
            "reopening plans again"
        );

        for _ in 0..4 {
            app.on_key(plain('j'));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(plain('y'));
        assert_eq!(setup_status(&app, 4), Status::Installed);
        assert_eq!(
            std::fs::read_to_string(&skill).unwrap(),
            integrations::SKILL
        );
        assert!(
            app.transient().is_some_and(
                |t| t.contains("backed up the old one to") && t.ends_with("SKILL.md.bak")
            ),
            "{:?}",
            app.transient()
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn n_or_esc_backs_out_of_the_setup_confirm_writing_nothing() {
        let (mut app, home, project) = setup_fixture(Config::default());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.on_key(enter);
        assert_eq!(app.on_key(plain('n')), Vec::new());
        assert!(!app.setup_confirming());
        assert_eq!(app.mode, Mode::Integrations, "back on the list");

        app.on_key(enter);
        app.on_key(esc);
        assert!(!app.setup_confirming());
        assert_eq!(app.mode, Mode::Integrations, "esc steps back a step");
        assert!(!home.join(".claude").exists(), "nothing was written");
        app.on_key(esc);
        assert_eq!(app.mode, Mode::Terminal, "then closes");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn s_switches_the_setup_panel_to_the_active_project_and_back() {
        let (mut app, home, project) = setup_fixture(Config::default());
        app.on_key(plain('s'));
        assert_eq!(app.setup_project(), Some("w"));
        assert_eq!(
            setup_paths(&app),
            [
                project.join(".claude/settings.json"),
                project.join(".codex/hooks.json"),
                project.join(".gemini/settings.json"),
                project.join(".opencode/plugins/tutti.js"),
                project.join(".claude/skills/tutti/SKILL.md"),
            ],
            "the project scope plans against the workspace's dir"
        );
        for _ in 0..4 {
            app.on_key(plain('j'));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(plain('y'));
        assert_eq!(
            std::fs::read_to_string(project.join(".claude/skills/tutti/SKILL.md")).unwrap(),
            integrations::SKILL
        );

        app.on_key(plain('s'));
        assert_eq!(app.setup_project(), None);
        assert_eq!(
            setup_paths(&app)[4],
            home.join(".claude/skills/tutti/SKILL.md")
        );
        assert!(
            matches!(setup_status(&app, 4), Status::Missing(_)),
            "the project install left the home alone"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&project);
    }

    /// The shell row appends its one line to the `$SHELL`'s rc file behind a
    /// backup, keeping what the file held, and then reads as installed; a
    /// project has no rc file, so the row is unavailable there.
    #[test]
    fn the_shell_row_appends_its_line_to_the_rc_file_once() {
        let (mut app, home, project) = setup_fixture(Config::default());
        let zshrc = home.join(".zshrc");
        std::fs::write(&zshrc, "export EDITOR=vi").unwrap();
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_key(ctrl('b'));
        app.on_key(plain('i'));
        for _ in 0..5 {
            app.on_key(plain('j'));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.setup_confirming(), "enter asks first");
        app.on_key(plain('y'));
        let line = integrations::Shell::Zsh.rc_line();
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            format!("export EDITOR=vi\n{line}\n")
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".zshrc.bak")).unwrap(),
            "export EDITOR=vi"
        );
        assert_eq!(setup_status(&app, 5), Status::Installed);

        app.on_key(plain('s'));
        assert!(
            app.setup_rows().nth(5).is_some_and(|(_, plan)| plan
                .as_ref()
                .is_err_and(|e| e.to_string().contains("not a project's"))),
            "a project has no rc file"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn s_with_no_project_open_stays_on_the_user_scope() {
        let mut app = App::new();
        let home = fresh_dir();
        app.set_local(|local| {
            local.dirs = Dirs {
                home: Some(home.clone()),
                ..Dirs::default()
            }
        });
        attach_with(&mut app, Vec::new());
        app.on_key(ctrl('b'));
        app.on_key(plain('i'));
        app.on_key(plain('s'));
        assert_eq!(app.setup_project(), None);
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("no project open"))
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn rows_tutti_will_not_write_refuse_in_red_without_asking() {
        let (mut app, home, project) = setup_fixture(Config::default());
        let settings = home.join(".claude/settings.json");
        let plugin = home.join(".config/opencode/plugins/tutti.js");
        for (path, text) in [(&settings, "{nope"), (&plugin, "export const Mine = 1\n")] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_key(ctrl('b'));
        app.on_key(plain('i'));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        app.on_key(enter);
        assert!(
            !app.setup_confirming(),
            "an unreadable file is never offered"
        );
        assert!(
            app.transient()
                .is_some_and(|t| t.starts_with("error: ") && t.contains("not valid JSON")),
            "{:?}",
            app.transient()
        );
        for _ in 0..3 {
            app.on_key(plain('j'));
        }
        app.on_key(enter);
        assert!(!app.setup_confirming(), "nor is someone else's");
        assert_eq!(
            app.transient(),
            Some(
                format!(
                    "error: {} is not tutti's; refusing to overwrite it",
                    plugin.display()
                )
                .as_str()
            )
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{nope");
        assert_eq!(
            std::fs::read_to_string(&plugin).unwrap(),
            "export const Mine = 1\n"
        );

        // Without a home to name, a user-scope row cannot be planned at all.
        app.set_local(|local| local.dirs = Dirs::default());
        app.on_key(plain('k'));
        app.on_key(plain('s'));
        app.on_key(plain('s'));
        app.on_key(enter);
        assert_eq!(app.transient(), Some("error: HOME is not set"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&project);
    }
}
