//! User configuration from `$XDG_CONFIG_HOME/tutti/config.toml` (falling back
//! to `~/.config/tutti/config.toml`): the prefix chord, the master mouse
//! switch, and the direct (prefix-less) key bindings. A missing file yields
//! defaults; a malformed file, an unknown key, or an unparseable chord is a
//! hard error naming the offending entry — never silently ignored.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// The default ratio boundary at which the direct nav keys resize a split.
pub const RESIZE_DELTA: f32 = 0.05;

/// A direct-binding action: what a bound chord does when pressed in terminal
/// mode, before the key would otherwise be forwarded to the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    ResizeLeft,
    ResizeDown,
    ResizeUp,
    ResizeRight,
    KillPane,
}

/// The defaults, exactly the documented table: `C-h/j/k/l` focus, `A-h/j/k/l`
/// resize, `A-x` kill.
const DEFAULTS: &[(Action, &str, &str)] = &[
    (Action::FocusLeft, "focus_left", "C-h"),
    (Action::FocusDown, "focus_down", "C-j"),
    (Action::FocusUp, "focus_up", "C-k"),
    (Action::FocusRight, "focus_right", "C-l"),
    (Action::ResizeLeft, "resize_left", "A-h"),
    (Action::ResizeDown, "resize_down", "A-j"),
    (Action::ResizeUp, "resize_up", "A-k"),
    (Action::ResizeRight, "resize_right", "A-l"),
    (Action::KillPane, "kill_pane", "A-x"),
];

const DEFAULT_PREFIX: &str = "C-b";

/// A prefix-mode action: the second key pressed after the prefix chord. One
/// source of truth for dispatch, the which-key popup, and the help overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixAction {
    SplitRight,
    SplitDown,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    FocusCycle,
    KillPane,
    Zoom,
    Scrollback,
    TabNext,
    TabPrev,
    TabNew,
    Sidebar,
    Detach,
    Help,
}

/// Whether the workspace/agent sidebar is rendered. `On` is the default — the
/// control column is the product. `Auto` shows it once the session is worth
/// surfacing (more than one workspace, or any agent pane); `Off` hides it until
/// focused. Focusing it with the sidebar key forces it visible regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarVisibility {
    Auto,
    On,
    Off,
}

/// One prefix binding: the follow-up key, its action, and a short description
/// rendered in the which-key popup and help overlay.
#[derive(Debug, Clone, Copy)]
pub struct PrefixBinding {
    pub key: KeyCode,
    pub action: PrefixAction,
    pub desc: &'static str,
}

/// The keymap preset selecting the default prefix binding table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Default,
    Vim,
}

/// The emacs-flavoured default: tmux-style prefix keys. `q` precedes `d` so it
/// is the detach key the hint and help surface first.
const DEFAULT_PREFIX_TABLE: &[PrefixBinding] = &[
    b(KeyCode::Char('%'), PrefixAction::SplitRight, "split right"),
    b(KeyCode::Char('"'), PrefixAction::SplitDown, "split below"),
    b(KeyCode::Char('x'), PrefixAction::KillPane, "kill pane"),
    b(KeyCode::Char('z'), PrefixAction::Zoom, "zoom pane"),
    b(KeyCode::Char('['), PrefixAction::Scrollback, "scrollback"),
    b(KeyCode::Char('n'), PrefixAction::TabNext, "next tab"),
    b(KeyCode::Char('p'), PrefixAction::TabPrev, "prev tab"),
    b(KeyCode::Char('c'), PrefixAction::TabNew, "new tab"),
    b(KeyCode::Char('w'), PrefixAction::Sidebar, "workspaces"),
    b(
        KeyCode::Char('o'),
        PrefixAction::FocusCycle,
        "focus next pane",
    ),
    b(KeyCode::Left, PrefixAction::FocusLeft, "focus left"),
    b(KeyCode::Down, PrefixAction::FocusDown, "focus down"),
    b(KeyCode::Up, PrefixAction::FocusUp, "focus up"),
    b(KeyCode::Right, PrefixAction::FocusRight, "focus right"),
    b(KeyCode::Char('q'), PrefixAction::Detach, "detach"),
    b(KeyCode::Char('d'), PrefixAction::Detach, "detach"),
    b(KeyCode::Char('?'), PrefixAction::Help, "help"),
];

/// Mnemonics vim users expect: `v`/`s` split, `h/j/k/l` focus, `q` kill (vim
/// `:q` closes a window), `d` detach so detach stays reachable.
const VIM_PREFIX_TABLE: &[PrefixBinding] = &[
    b(KeyCode::Char('v'), PrefixAction::SplitRight, "split right"),
    b(KeyCode::Char('s'), PrefixAction::SplitDown, "split below"),
    b(KeyCode::Char('h'), PrefixAction::FocusLeft, "focus left"),
    b(KeyCode::Char('j'), PrefixAction::FocusDown, "focus down"),
    b(KeyCode::Char('k'), PrefixAction::FocusUp, "focus up"),
    b(KeyCode::Char('l'), PrefixAction::FocusRight, "focus right"),
    b(KeyCode::Char('q'), PrefixAction::KillPane, "kill pane"),
    b(KeyCode::Char('d'), PrefixAction::Detach, "detach"),
    b(KeyCode::Char('t'), PrefixAction::TabNew, "new tab"),
    b(KeyCode::Char('n'), PrefixAction::TabNext, "next tab"),
    b(KeyCode::Char('p'), PrefixAction::TabPrev, "prev tab"),
    b(KeyCode::Char('w'), PrefixAction::Sidebar, "workspaces"),
    b(KeyCode::Char('z'), PrefixAction::Zoom, "zoom pane"),
    b(KeyCode::Char('['), PrefixAction::Scrollback, "scrollback"),
    b(KeyCode::Char('?'), PrefixAction::Help, "help"),
];

const fn b(key: KeyCode, action: PrefixAction, desc: &'static str) -> PrefixBinding {
    PrefixBinding { key, action, desc }
}

/// A short human label for a key code, for the hint, which-key, and help.
pub fn key_label(code: KeyCode) -> String {
    match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Esc => "esc".into(),
        other => format!("{other:?}"),
    }
}

/// A single key chord: a key code plus its Ctrl/Alt modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    /// Parse `C-<char>` (Ctrl), `A-<char>` (Alt), or a bare printable char.
    pub fn parse(spec: &str) -> Result<Chord> {
        let (mods, rest) = match spec.strip_prefix("C-") {
            Some(rest) => (KeyModifiers::CONTROL, rest),
            None => match spec.strip_prefix("A-") {
                Some(rest) => (KeyModifiers::ALT, rest),
                None => (KeyModifiers::NONE, spec),
            },
        };
        let mut chars = rest.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Ok(Chord {
                code: KeyCode::Char(c),
                mods,
            }),
            _ => bail!("invalid key chord {spec:?}"),
        }
    }

    /// Whether `key` is this chord: the same code with Ctrl and Alt matching
    /// exactly. Shift is ignored, so it rides along with the base character.
    pub fn matches(&self, key: KeyEvent) -> bool {
        self.code == key.code
            && key.modifiers.contains(KeyModifiers::CONTROL)
                == self.mods.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT) == self.mods.contains(KeyModifiers::ALT)
    }

    /// A short human label, e.g. `C-b` / `A-x` / `%`.
    pub fn label(&self) -> String {
        let base = key_label(self.code);
        if self.mods.contains(KeyModifiers::CONTROL) {
            format!("C-{base}")
        } else if self.mods.contains(KeyModifiers::ALT) {
            format!("A-{base}")
        } else {
            base
        }
    }
}

/// The resolved direct bindings: the chords that are enabled, paired with the
/// action they trigger. A binding set to `"none"` is absent here.
#[derive(Debug, Clone)]
pub struct Keys {
    bindings: Vec<(Chord, Action)>,
}

impl Keys {
    /// The action bound to `key`, if any.
    pub fn action_for(&self, key: KeyEvent) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(chord, _)| chord.matches(key))
            .map(|(_, action)| *action)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub prefix: Chord,
    pub mouse: bool,
    pub keys: Keys,
    pub preset: Preset,
    pub sidebar: SidebarVisibility,
    /// Whether pane notifications re-emit to the real terminal and flash the
    /// status bar. The sidebar bell mark is unaffected — always on.
    pub notifications: bool,
    /// Startup projects: workspace dirs to mount (idempotently) on attach, each
    /// `~`-expanded. Empty by default.
    pub projects: Vec<PathBuf>,
    prefix_bindings: Vec<PrefixBinding>,
}

impl Default for Config {
    fn default() -> Self {
        Config::parse("").expect("empty config parses to defaults")
    }
}

impl Config {
    /// Load the config, returning defaults when the file is absent. A present
    /// but malformed file is a hard error.
    pub fn load() -> Result<Config> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Parse config text. An empty string yields the defaults. Every `[keys]`
    /// entry is optional; `"none"` disables it; anything else must be a chord.
    pub fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text)?;
        let prefix = match raw.prefix.as_deref() {
            None => Chord::parse(DEFAULT_PREFIX).expect("valid default prefix"),
            Some(spec) => {
                Chord::parse(spec).with_context(|| format!("prefix: invalid value {spec:?}"))?
            }
        };
        let mouse = raw.mouse.unwrap_or(true);
        let notifications = raw.notifications.unwrap_or(true);

        let preset = match raw.preset.as_deref() {
            None | Some("default") => Preset::Default,
            Some("vim") => Preset::Vim,
            Some(other) => {
                bail!("preset: unknown value {other:?} (expected \"default\" or \"vim\")")
            }
        };
        let prefix_bindings = match preset {
            Preset::Default => DEFAULT_PREFIX_TABLE,
            Preset::Vim => VIM_PREFIX_TABLE,
        }
        .to_vec();

        let sidebar = match raw.sidebar.as_deref() {
            None | Some("on") => SidebarVisibility::On,
            Some("auto") => SidebarVisibility::Auto,
            Some("off") => SidebarVisibility::Off,
            Some(other) => {
                bail!("sidebar: unknown value {other:?} (expected \"auto\", \"on\", or \"off\")")
            }
        };

        let home = std::env::var_os("HOME").map(PathBuf::from);
        let projects = raw
            .projects
            .into_iter()
            .map(|p| expand_home(&p.dir, home.as_deref()))
            .collect();

        let mut overrides = raw.keys.into_map();
        let mut bindings = Vec::new();
        for (action, name, default) in DEFAULTS {
            let chord = match overrides.remove(*name) {
                None => Some(Chord::parse(default).expect("valid default chord")),
                Some(spec) if spec == "none" => None,
                Some(spec) => Some(
                    Chord::parse(&spec)
                        .with_context(|| format!("keys.{name}: invalid value {spec:?}"))?,
                ),
            };
            if let Some(chord) = chord {
                bindings.push((chord, *action));
            }
        }
        Ok(Config {
            prefix,
            mouse,
            keys: Keys { bindings },
            preset,
            sidebar,
            notifications,
            projects,
            prefix_bindings,
        })
    }

    /// The active prefix binding table — the single source dispatch, the
    /// which-key popup, and the help overlay all read.
    pub fn prefix_bindings(&self) -> &[PrefixBinding] {
        &self.prefix_bindings
    }

    /// The prefix action bound to `code`, if any.
    pub fn prefix_action(&self, code: KeyCode) -> Option<PrefixAction> {
        self.prefix_bindings
            .iter()
            .find(|b| b.key == code)
            .map(|b| b.action)
    }

    /// The first key bound to `action`, for surfacing a representative key in
    /// the hint and help overlay.
    pub fn prefix_key(&self, action: PrefixAction) -> Option<KeyCode> {
        self.prefix_bindings
            .iter()
            .find(|b| b.action == action)
            .map(|b| b.key)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    prefix: Option<String>,
    mouse: Option<bool>,
    preset: Option<String>,
    sidebar: Option<String>,
    notifications: Option<bool>,
    #[serde(default)]
    keys: RawKeys,
    #[serde(default)]
    projects: Vec<RawProject>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    dir: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawKeys {
    focus_left: Option<String>,
    focus_down: Option<String>,
    focus_up: Option<String>,
    focus_right: Option<String>,
    resize_left: Option<String>,
    resize_down: Option<String>,
    resize_up: Option<String>,
    resize_right: Option<String>,
    kill_pane: Option<String>,
}

impl RawKeys {
    /// The present entries as `name -> spec`, so the resolver can consume them
    /// and default the rest.
    fn into_map(self) -> std::collections::HashMap<&'static str, String> {
        [
            ("focus_left", self.focus_left),
            ("focus_down", self.focus_down),
            ("focus_up", self.focus_up),
            ("focus_right", self.focus_right),
            ("resize_left", self.resize_left),
            ("resize_down", self.resize_down),
            ("resize_up", self.resize_up),
            ("resize_right", self.resize_right),
            ("kill_pane", self.kill_pane),
        ]
        .into_iter()
        .filter_map(|(name, spec)| spec.map(|spec| (name, spec)))
        .collect()
    }
}

/// Expand a configured project dir: `~` / `~/rest` against `$HOME`, everything
/// else taken as written (absolute or relative to the client's cwd at mount).
fn expand_home(input: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if input == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = input.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

fn config_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".config"),
            None => PathBuf::from(".config"),
        },
    };
    base.join("tutti").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::fixtures::{alt, ctrl, plain};

    const EXAMPLE: &str = r#"
prefix = "C-b"
mouse = true

[keys]
focus_left  = "C-h"
focus_down  = "C-j"
focus_up    = "C-k"
focus_right = "C-l"
resize_left  = "A-h"
resize_down  = "A-j"
resize_up    = "A-k"
resize_right = "A-l"
kill_pane    = "A-x"
"#;

    #[test]
    fn chord_parses_ctrl_alt_and_bare() {
        assert_eq!(
            Chord::parse("C-x").unwrap(),
            Chord {
                code: KeyCode::Char('x'),
                mods: KeyModifiers::CONTROL
            }
        );
        assert_eq!(
            Chord::parse("A-x").unwrap(),
            Chord {
                code: KeyCode::Char('x'),
                mods: KeyModifiers::ALT
            }
        );
        assert_eq!(
            Chord::parse("x").unwrap(),
            Chord {
                code: KeyCode::Char('x'),
                mods: KeyModifiers::NONE
            }
        );
    }

    #[test]
    fn chord_matching_is_modifier_exact() {
        let c = Chord::parse("C-h").unwrap();
        assert!(c.matches(ctrl('h')));
        assert!(!c.matches(plain('h')));
        assert!(!c.matches(alt('h')));
    }

    #[test]
    fn default_prefix_matches_ctrl_b_only() {
        let config = Config::default();
        assert!(config.prefix.matches(ctrl('b')));
        assert!(!config.prefix.matches(plain('b')));
    }

    #[test]
    fn defaults_when_file_missing() {
        let config = Config::default();
        assert!(config.mouse);
        assert_eq!(config.keys.action_for(ctrl('h')), Some(Action::FocusLeft));
        assert_eq!(config.keys.action_for(ctrl('l')), Some(Action::FocusRight));
        assert_eq!(config.keys.action_for(alt('j')), Some(Action::ResizeDown));
        assert_eq!(config.keys.action_for(alt('x')), Some(Action::KillPane));
    }

    #[test]
    fn parses_full_example() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert!(config.mouse);
        assert!(config.prefix.matches(ctrl('b')));
        assert_eq!(config.keys.action_for(ctrl('k')), Some(Action::FocusUp));
        assert_eq!(config.keys.action_for(alt('l')), Some(Action::ResizeRight));
        assert_eq!(config.keys.action_for(alt('x')), Some(Action::KillPane));
    }

    #[test]
    fn none_disables_a_binding() {
        let config = Config::parse("[keys]\nfocus_left = \"none\"\n").unwrap();
        assert_eq!(config.keys.action_for(ctrl('h')), None);
        // The others keep their defaults.
        assert_eq!(config.keys.action_for(ctrl('l')), Some(Action::FocusRight));
    }

    #[test]
    fn custom_prefix_and_mouse_off() {
        let config = Config::parse("prefix = \"C-a\"\nmouse = false\n").unwrap();
        assert!(config.prefix.matches(ctrl('a')));
        assert!(!config.mouse);
    }

    #[test]
    fn malformed_chord_names_the_entry() {
        let err = Config::parse("[keys]\nfocus_left = \"C-\"\n").unwrap_err();
        assert!(
            err.to_string().contains("focus_left"),
            "error should name the offending entry: {err}"
        );
    }

    #[test]
    fn unknown_action_is_rejected() {
        let err = Config::parse("[keys]\nfocus_lft = \"C-h\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("focus_lft"),
            "error should name the unknown key: {err:#}"
        );
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let err = Config::parse("prefixx = \"C-a\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("prefixx"),
            "error should name the unknown key: {err:#}"
        );
    }

    #[test]
    fn preset_selection_changes_prefix_bindings() {
        let default = Config::default();
        assert_eq!(default.preset, Preset::Default);
        assert_eq!(
            default.prefix_action(KeyCode::Char('%')),
            Some(PrefixAction::SplitRight)
        );

        let vim = Config::parse("preset = \"vim\"\n").unwrap();
        assert_eq!(vim.preset, Preset::Vim);
        assert_eq!(
            vim.prefix_action(KeyCode::Char('v')),
            Some(PrefixAction::SplitRight)
        );
        assert_eq!(
            vim.prefix_action(KeyCode::Char('s')),
            Some(PrefixAction::SplitDown)
        );
        assert_eq!(
            vim.prefix_action(KeyCode::Char('q')),
            Some(PrefixAction::KillPane)
        );
        // The default's `%` is not a vim binding.
        assert_eq!(vim.prefix_action(KeyCode::Char('%')), None);
    }

    #[test]
    fn both_presets_keep_detach_reachable() {
        assert_eq!(
            Config::default().prefix_key(PrefixAction::Detach),
            Some(KeyCode::Char('q'))
        );
        let vim = Config::parse("preset = \"vim\"\n").unwrap();
        assert_eq!(
            vim.prefix_action(KeyCode::Char('d')),
            Some(PrefixAction::Detach)
        );
        assert_eq!(
            vim.prefix_key(PrefixAction::Detach),
            Some(KeyCode::Char('d'))
        );
    }

    #[test]
    fn whichkey_rows_are_the_dispatch_table() {
        // Every row the which-key popup renders dispatches to its own action —
        // one source of truth for both presets.
        for cfg in [
            Config::default(),
            Config::parse("preset = \"vim\"\n").unwrap(),
        ] {
            assert!(!cfg.prefix_bindings().is_empty());
            for binding in cfg.prefix_bindings() {
                assert_eq!(cfg.prefix_action(binding.key), Some(binding.action));
            }
        }
    }

    #[test]
    fn keys_override_beats_preset_default() {
        // A [keys] entry wins over the preset-provided default binding table.
        let cfg = Config::parse("preset = \"vim\"\n[keys]\nfocus_left = \"none\"\n").unwrap();
        assert_eq!(cfg.keys.action_for(ctrl('h')), None);

        let cfg = Config::parse("[keys]\nfocus_left = \"C-y\"\n").unwrap();
        assert_eq!(cfg.keys.action_for(ctrl('y')), Some(Action::FocusLeft));
        assert_eq!(cfg.keys.action_for(ctrl('h')), None);
    }

    #[test]
    fn unknown_preset_is_rejected() {
        let err = Config::parse("preset = \"zap\"\n").unwrap_err();
        assert!(
            err.to_string().contains("zap"),
            "error should name the preset: {err}"
        );
    }

    #[test]
    fn sidebar_defaults_to_on_and_parses_values() {
        assert_eq!(Config::default().sidebar, SidebarVisibility::On);
        assert_eq!(
            Config::parse("sidebar = \"on\"\n").unwrap().sidebar,
            SidebarVisibility::On
        );
        assert_eq!(
            Config::parse("sidebar = \"off\"\n").unwrap().sidebar,
            SidebarVisibility::Off
        );
        assert_eq!(
            Config::parse("sidebar = \"auto\"\n").unwrap().sidebar,
            SidebarVisibility::Auto
        );
    }

    #[test]
    fn projects_default_empty_and_parse_a_dir_list() {
        assert!(Config::default().projects.is_empty());
        let cfg =
            Config::parse("[[projects]]\ndir = \"/srv/api\"\n\n[[projects]]\ndir = \"/srv/web\"\n")
                .unwrap();
        assert_eq!(
            cfg.projects,
            vec![PathBuf::from("/srv/api"), PathBuf::from("/srv/web")]
        );
    }

    #[test]
    fn project_requires_a_dir() {
        let err = Config::parse("[[projects]]\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("dir"),
            "error should name the missing key: {err:#}"
        );
    }

    #[test]
    fn expand_home_expands_tilde_only() {
        let home = Path::new("/home/alice");
        assert_eq!(expand_home("~", Some(home)), PathBuf::from("/home/alice"));
        assert_eq!(
            expand_home("~/develop/x", Some(home)),
            PathBuf::from("/home/alice/develop/x")
        );
        assert_eq!(expand_home("/abs", Some(home)), PathBuf::from("/abs"));
        assert_eq!(
            expand_home("~", None),
            PathBuf::from("~"),
            "without a home, ~ is taken literally"
        );
    }

    #[test]
    fn unknown_sidebar_value_is_rejected() {
        let err = Config::parse("sidebar = \"maybe\"\n").unwrap_err();
        assert!(
            err.to_string().contains("maybe"),
            "error should name the offending value: {err}"
        );
    }

    #[test]
    fn notifications_default_on_and_toggle_off() {
        assert!(Config::default().notifications);
        assert!(
            !Config::parse("notifications = false\n")
                .unwrap()
                .notifications
        );
        assert!(
            Config::parse("notifications = true\n")
                .unwrap()
                .notifications
        );
    }

    #[test]
    fn both_presets_bind_the_sidebar_key() {
        for cfg in [
            Config::default(),
            Config::parse("preset = \"vim\"\n").unwrap(),
        ] {
            assert_eq!(
                cfg.prefix_action(KeyCode::Char('w')),
                Some(PrefixAction::Sidebar),
                "w should focus the sidebar in every preset"
            );
        }
    }
}
