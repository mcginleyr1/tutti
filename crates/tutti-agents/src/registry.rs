use std::collections::HashMap;
use std::path::Path;

use tutti_core::{AgentKind, Observation};

/// A cheap, owned snapshot of the process table sufficient for agent detection:
/// each pid's executable name plus its direct children. The server fills it from
/// `sysinfo`; tests fill it by hand, so the matcher needs no live processes.
#[derive(Debug, Default, Clone)]
pub struct ProcessTree {
    names: HashMap<u32, String>,
    children: HashMap<u32, Vec<u32>>,
}

impl ProcessTree {
    /// Record one process. `name` may be a bare basename or a full path; the
    /// registry extracts the basename when matching.
    pub fn insert(&mut self, pid: u32, parent: Option<u32>, name: impl Into<String>) {
        self.names.insert(pid, name.into());
        if let Some(parent) = parent {
            self.children.entry(parent).or_default().push(pid);
        }
    }

    fn name(&self, pid: u32) -> Option<&str> {
        self.names.get(&pid).map(String::as_str)
    }

    fn children(&self, pid: u32) -> &[u32] {
        self.children.get(&pid).map_or(&[], Vec::as_slice)
    }
}

/// One agent's identity plus the screen-text patterns that classify its state.
/// Patterns are plain case-sensitive substrings, so the whole registry stays a
/// data table: tuning a heuristic or adding an agent is an edit here, not code.
/// The seeded patterns are starting guesses to be tuned against live agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    pub kind: AgentKind,
    /// The human product name, for the launcher rows (`Claude Code`, `Crush`).
    pub display: String,
    /// The project's home, shown on a dim launcher row when the binary is not
    /// installed so the catalog doubles as a "where to get it" pointer.
    pub url: String,
    pub process_names: Vec<String>,
    /// Argv after the binary for the launcher — empty for agents that open
    /// on a bare run; `ori` needs `code` or it just prints its help.
    pub launch_args: Vec<String>,
    /// How the agent's interactive mode takes a first message on its command
    /// line: these args, then the message — empty for a positional prompt
    /// (`claude <prompt>`), `--prompt` for opencode. `None` when it takes
    /// none, so nothing can be deployed to it.
    pub prompt_flags: Option<Vec<String>>,
    pub blocked_patterns: Vec<String>,
    pub working_patterns: Vec<String>,
    pub done_patterns: Vec<String>,
}

impl AgentSpec {
    /// Prompts, permission dialogs, status lines, and idle markers all render at
    /// the bottom of the screen, so every pattern is matched only within the
    /// last `TAIL_LINES` visible lines. This keeps a prompt that has scrolled up
    /// into history from masking an active task below it — and keeps a
    /// transcript that merely *quotes* a marker (an agent discussing "esc to
    /// interrupt") from reading as that agent's own status line, which would
    /// pin it in `Working`.
    const TAIL_LINES: usize = 15;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: &str,
        display: &str,
        url: &str,
        process_names: &[&str],
        blocked_patterns: &[&str],
        working_patterns: &[&str],
        done_patterns: &[&str],
    ) -> Self {
        AgentSpec {
            kind: kind.into(),
            display: display.into(),
            url: url.into(),
            process_names: owned(process_names),
            launch_args: Vec::new(),
            prompt_flags: None,
            blocked_patterns: owned(blocked_patterns),
            working_patterns: owned(working_patterns),
            done_patterns: owned(done_patterns),
        }
    }

    /// This spec taking its first message after `flags` (see `prompt_flags`).
    fn prompted(self, flags: &[&str]) -> Self {
        AgentSpec {
            prompt_flags: Some(owned(flags)),
            ..self
        }
    }

    /// The argv launching the agent: its binary, then any launch args.
    pub fn argv(&self) -> Vec<String> {
        let binary = self
            .process_names
            .first()
            .cloned()
            .unwrap_or_else(|| self.kind.to_string());
        std::iter::once(binary)
            .chain(self.launch_args.iter().cloned())
            .collect()
    }

    /// The argv launching the agent with `prompt` as its first message, or
    /// `None` when it takes no prompt.
    pub fn prompted_argv(&self, prompt: &str) -> Option<Vec<String>> {
        let flags = self.prompt_flags.as_ref()?;
        Some(
            self.argv()
                .into_iter()
                .chain(flags.iter().cloned())
                .chain([prompt.to_string()])
                .collect(),
        )
    }

    /// Blocked (needs input) outranks Working (active task) outranks Done, so the
    /// most attention-worthy signal wins when several match. `None` means no
    /// pattern matched and the caller should fall back to activity heuristics.
    pub fn classify(&self, screen_text: &str) -> Option<Observation> {
        let tail = tail(screen_text, Self::TAIL_LINES);
        if self
            .blocked_patterns
            .iter()
            .any(|p| tail.contains(p.as_str()))
        {
            Some(Observation::Blocked)
        } else if self
            .working_patterns
            .iter()
            .any(|p| tail.contains(p.as_str()))
        {
            Some(Observation::Working)
        } else if self.done_patterns.iter().any(|p| tail.contains(p.as_str())) {
            Some(Observation::Done)
        } else {
            None
        }
    }
}

/// The set of agents Tutti can recognize. Seeded with the alpha set; callers
/// extend it with `add` (config-driven overrides land later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    specs: Vec<AgentSpec>,
}

impl Registry {
    /// Look up the spec for a process by its basename, so an absolute path like
    /// `/usr/local/bin/claude` matches the `claude` entry.
    pub fn match_process(&self, process_name: &str) -> Option<&AgentSpec> {
        let basename = Path::new(process_name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(process_name);
        self.specs
            .iter()
            .find(|spec| spec.process_names.iter().any(|n| n == basename))
    }

    /// The spec for an already-identified agent kind, for classifying its state.
    pub fn spec(&self, kind: &AgentKind) -> Option<&AgentSpec> {
        self.specs.iter().find(|spec| &spec.kind == kind)
    }

    /// The agent kind running in the process subtree rooted at `root`, if any.
    /// The deepest match wins, so an agent launched inside a shell (e.g. in a
    /// split pane) is found under its shell rather than the shell masking it.
    pub fn detect(&self, tree: &ProcessTree, root: u32) -> Option<AgentKind> {
        let mut best: Option<(usize, AgentKind)> = None;
        let mut stack = vec![(root, 0usize)];
        while let Some((pid, depth)) = stack.pop() {
            if let Some(name) = tree.name(pid)
                && let Some(spec) = self.match_process(name)
                && best.as_ref().is_none_or(|(d, _)| depth > *d)
            {
                best = Some((depth, spec.kind.clone()));
            }
            for &child in tree.children(pid) {
                stack.push((child, depth + 1));
            }
        }
        best.map(|(_, kind)| kind)
    }

    pub fn add(&mut self, spec: AgentSpec) {
        self.specs.push(spec);
    }

    pub fn specs(&self) -> &[AgentSpec] {
        &self.specs
    }
}

/// The generic guess patterns newcomer agents ship with until someone tunes
/// them against a live pane: permission-ish words for Blocked, spinner-ish
/// words for Working, and — deliberately — no Done marker: an empty list never
/// fires, which beats a generic prompt glyph misfiring. Detection by process
/// name is the reliable part; states fall back to activity heuristics.
const GUESS_BLOCKED: &[&str] = &["y/n", "Allow"];
const GUESS_WORKING: &[&str] = &["esc to interrupt", "esc to cancel", "Working", "Thinking"];

impl Default for Registry {
    fn default() -> Self {
        let guess = |kind: &str, display: &str, url: &str, names: &[&str]| {
            AgentSpec::new(kind, display, url, names, GUESS_BLOCKED, GUESS_WORKING, &[])
        };
        // Prompt flags come from each tool's own `--help` (claude, opencode,
        // ori, vibe, muse), or its docs where it isn't installed here (codex:
        // "Optional text instruction to start the session"; gemini's
        // `--prompt-interactive`: "Execute prompt and continue in interactive
        // mode"). crush's interactive mode takes no prompt — `crush run` is
        // headless — and the rest are unverified, so they take none.
        Registry {
            specs: vec![
                AgentSpec::new(
                    "claude",
                    "Claude Code",
                    "https://claude.com/claude-code",
                    &["claude"],
                    &["Do you want", "y/n", "Waiting for your input", "permission"],
                    &["esc to interrupt", "Thinking", "Running"],
                    &["❯"],
                )
                .prompted(&[]),
                AgentSpec::new(
                    "codex",
                    "Codex CLI",
                    "https://github.com/openai/codex",
                    &["codex"],
                    &["Allow command", "approve", "y/n"],
                    &["esc to interrupt", "Working", "Thinking"],
                    &["»"],
                )
                .prompted(&[]),
                // The catalog: name-detection plus guess patterns only. crush
                // resumes via `crush --session <id>`, but its store is sqlite,
                // so the resume harvest stays claude-only for now.
                guess(
                    "gemini",
                    "Gemini CLI",
                    "https://github.com/google-gemini/gemini-cli",
                    &["gemini"],
                )
                .prompted(&["--prompt-interactive"]),
                // Its TUI submits `--prompt` once a model is ready.
                guess("opencode", "OpenCode", "https://opencode.ai", &["opencode"])
                    .prompted(&["--prompt"]),
                guess(
                    "crush",
                    "Crush",
                    "https://github.com/charmbracelet/crush",
                    &["crush"],
                ),
                guess("aider", "Aider", "https://aider.chat", &["aider"]),
                guess(
                    "goose",
                    "Goose",
                    "https://github.com/block/goose",
                    &["goose"],
                ),
                guess("pi", "Pi", "https://github.com/badlogic/pi-mono", &["pi"]),
                guess(
                    "qwen",
                    "Qwen Code",
                    "https://github.com/QwenLM/qwen-code",
                    &["qwen"],
                ),
                guess(
                    "cursor",
                    "Cursor CLI",
                    "https://cursor.com/cli",
                    &["cursor-agent"],
                ),
                guess(
                    "copilot",
                    "Copilot CLI",
                    "https://github.com/github/copilot-cli",
                    &["copilot"],
                ),
                guess("amp", "Amp", "https://ampcode.com", &["amp"]),
                guess("droid", "Factory Droid", "https://factory.ai", &["droid"]),
                // Observed live (Muse Code 1.0.1 in a pty): the status line
                // reads `Working (3s · esc to interrupt)` and `◇ Thinking`
                // while a turn runs; the idle input box leads with `⟩`. Its
                // approval dialog is untested, so Blocked keeps the guesses.
                AgentSpec::new(
                    "muse",
                    "Muse Code",
                    "https://dev.meta.ai/docs/muse-code",
                    &["muse"],
                    GUESS_BLOCKED,
                    &["esc to interrupt", "Thinking", "Working"],
                    &["⟩"],
                )
                .prompted(&[]),
                // Mistral Vibe's strings, from its source (2.23.3): the loading
                // line reads `Thinking (3s Esc to interrupt)` and the approval
                // dialog is titled `Permission for the <tool> tool`. Its input
                // box carries no idle glyph, so there is no Done marker.
                AgentSpec::new(
                    "vibe",
                    "Mistral Vibe",
                    "https://github.com/mistralai/mistral-vibe",
                    &["vibe"],
                    &["Permission for the", "Always allow"],
                    &["to interrupt", "Thinking", "Retrying"],
                    &[],
                )
                .prompted(&[]),
                guess(
                    "auggie",
                    "Augment Code",
                    "https://www.augmentcode.com",
                    &["auggie"],
                ),
                // Amazon Q's binary is literally `q`, which can collide with
                // other tools of that name (kdb+); the cost of a false match is
                // only a mislabelled badge, so it stays in.
                guess(
                    "amazon-q",
                    "Amazon Q",
                    "https://github.com/aws/amazon-q-developer-cli",
                    &["q"],
                ),
                // Ori's bare binary is a harness launcher (`ori claude`,
                // `ori codex`, …) — the agent it wraps is detected as itself,
                // deeper in the tree — so the launcher runs `ori code`, its
                // own agent. Observed live (0.13.0 in a pty): the input box
                // reads `❯ Ask Ori anything…` when idle and `❯ Queue · ↑↓
                // select · x delete · Enter steer · Esc` for the whole turn;
                // the approval dialog is titled `Permission requested` with
                // `Allow once` first. Its lowercase transcript echo,
                // `⚠ permission requested: …`, outlives the dialog — never a
                // Blocked marker. A bare `--prompt` runs one headless turn;
                // `--interactive` opens the TUI with it as the first.
                AgentSpec {
                    launch_args: owned(&["code"]),
                    ..AgentSpec::new(
                        "ori",
                        "Ori",
                        "https://openrouter.ai/labs/ori",
                        &["ori"],
                        &["Permission requested", "Allow once"],
                        &["Enter steer", "Thinking…"],
                        &["Ask Ori anything"],
                    )
                }
                .prompted(&["--interactive", "--prompt"]),
            ],
        }
    }
}

fn owned(patterns: &[&str]) -> Vec<String> {
    patterns.iter().map(|s| s.to_string()).collect()
}

/// The trailing slice of `text` holding at most its last `max_lines` lines.
fn tail(text: &str, max_lines: usize) -> &str {
    text.match_indices('\n')
        .rev()
        .nth(max_lines.saturating_sub(1))
        .map_or(text, |(idx, _)| &text[idx + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> AgentSpec {
        Registry::default().match_process("claude").unwrap().clone()
    }

    fn padded(top: &str, filler: &str, lines: usize) -> String {
        let mut s = String::from(top);
        for _ in 0..lines {
            s.push('\n');
            s.push_str(filler);
        }
        s
    }

    #[test]
    fn blocked_beats_working_and_done() {
        let screen = "esc to interrupt\nDo you want to continue?\n❯";
        assert_eq!(claude().classify(screen), Some(Observation::Blocked));
    }

    #[test]
    fn working_beats_done() {
        let screen = "Thinking about it\nsome output\n❯";
        assert_eq!(claude().classify(screen), Some(Observation::Working));
    }

    #[test]
    fn done_marker_in_tail_classifies_done() {
        assert_eq!(
            claude().classify("all finished\n❯"),
            Some(Observation::Done)
        );
    }

    #[test]
    fn done_marker_above_tail_is_ignored() {
        let screen = padded("❯", "plain output", 20);
        assert_eq!(claude().classify(&screen), None);
    }

    #[test]
    fn working_marker_above_the_tail_is_ignored() {
        let screen = padded("Running the build", "log line", 20);
        assert_eq!(claude().classify(&screen), None);
    }

    /// An agent talking *about* markers must not classify as working: the quoted
    /// "esc to interrupt" sits in the transcript body, above the tail, while
    /// the real status line — here the idle prompt — is at the bottom.
    #[test]
    fn a_transcript_quoting_a_marker_does_not_read_as_working() {
        let mut screen = padded(
            "we tuned the \"esc to interrupt\" marker today",
            "more discussion",
            20,
        );
        screen.push_str("\n❯");
        assert_eq!(claude().classify(&screen), Some(Observation::Done));
    }

    #[test]
    fn no_signal_returns_none() {
        assert_eq!(
            claude().classify("regular shell output\nnothing special"),
            None
        );
    }

    #[test]
    fn match_process_uses_basename() {
        let reg = Registry::default();
        assert_eq!(
            reg.match_process("/usr/local/bin/claude").map(|s| &s.kind),
            Some(&AgentKind::from("claude"))
        );
        assert_eq!(
            reg.match_process("/opt/bin/codex").map(|s| &s.kind),
            Some(&AgentKind::from("codex"))
        );
    }

    #[test]
    fn match_process_unknown_returns_none() {
        assert!(Registry::default().match_process("bash").is_none());
    }

    #[test]
    fn detect_matches_direct_child() {
        let reg = Registry::default();
        let mut tree = ProcessTree::default();
        tree.insert(100, None, "/usr/local/bin/claude");
        assert_eq!(reg.detect(&tree, 100), Some(AgentKind::from("claude")));
    }

    #[test]
    fn detect_finds_agent_under_a_shell() {
        // A split pane runs a shell that launches an agent: the shell does not
        // match, but the agent nested below it does.
        let reg = Registry::default();
        let mut tree = ProcessTree::default();
        tree.insert(100, None, "zsh");
        tree.insert(101, Some(100), "claude");
        assert_eq!(reg.detect(&tree, 100), Some(AgentKind::from("claude")));
    }

    #[test]
    fn detect_deepest_match_wins() {
        // Both the root and a grandchild are known agents; the deepest wins so a
        // wrapper agent does not mask the one actually doing the work.
        let reg = Registry::default();
        let mut tree = ProcessTree::default();
        tree.insert(100, None, "claude");
        tree.insert(101, Some(100), "bash");
        tree.insert(102, Some(101), "codex");
        assert_eq!(reg.detect(&tree, 100), Some(AgentKind::from("codex")));
    }

    #[test]
    fn detect_returns_none_without_a_match() {
        let reg = Registry::default();
        let mut tree = ProcessTree::default();
        tree.insert(100, None, "zsh");
        tree.insert(101, Some(100), "vim");
        assert_eq!(reg.detect(&tree, 100), None);
    }

    #[test]
    fn detect_ignores_processes_outside_the_subtree() {
        let reg = Registry::default();
        let mut tree = ProcessTree::default();
        tree.insert(100, None, "zsh");
        tree.insert(200, None, "claude"); // sibling tree, not a descendant of 100
        assert_eq!(reg.detect(&tree, 100), None);
    }

    /// Drive one agent pane through the same event sequence the server's
    /// classification cadence produces: screen text becomes `Classified`
    /// observations, focus and activity become their own events. Proves the
    /// classifier and the state machine compose, including `Done → Idle` on
    /// focus and the `Idle → Working` re-entry.
    #[test]
    fn classification_cadence_advances_state() {
        use tutti_core::{AgentState, StateEvent};

        let spec = claude();
        let classify = |text: &str| spec.classify(text).map(StateEvent::Classified);

        let mut state = AgentState::Unknown;
        state = state.apply(classify("esc to interrupt").unwrap());
        assert_eq!(state, AgentState::Working);

        state = state.apply(classify("esc to interrupt\nDo you want to continue?").unwrap());
        assert_eq!(state, AgentState::Blocked);

        // Answering resumes work: the working marker reappears.
        state = state.apply(classify("esc to interrupt").unwrap());
        assert_eq!(state, AgentState::Working);

        // The task finishes: only the idle prompt remains.
        state = state.apply(classify("all done\n❯").unwrap());
        assert_eq!(state, AgentState::Done);

        // Focusing a Done pane marks it seen.
        state = state.apply(StateEvent::Focused);
        assert_eq!(state, AgentState::Idle);

        // Fresh output re-enters Working even before a classifier match.
        assert!(classify("quiet shell output").is_none());
        state = state.apply(StateEvent::Activity);
        assert_eq!(state, AgentState::Working);
    }

    /// Screens captured from Muse Code 1.0.1 driving its echo provider in a
    /// pty: the working status line outranks the ever-present input prompt,
    /// and the prompt alone reads as Done once the turn lands.
    #[test]
    fn muse_screen_markers_classify_working_then_done() {
        let spec = Registry::default().match_process("muse").unwrap().clone();
        let working = "⟩ tutti pty probe prompt\n◇ Thinking\n── Voice input (⌥ + v to start) ──\n⟩ \n  echo · /tmp/proj\nWorking (0s · esc to interrupt)";
        assert_eq!(spec.classify(working), Some(Observation::Working));
        let idle = "⟩ tutti pty probe prompt\n◆ echo: tutti pty probe prompt\n── Voice input (⌥ + v to start) ──\n⟩ \n  echo · /tmp/proj";
        assert_eq!(spec.classify(idle), Some(Observation::Done));
    }

    #[test]
    fn vibe_screen_markers_classify_working_and_blocked() {
        let spec = Registry::default().match_process("vibe").unwrap().clone();
        let working = "fix the failing test\n⠋ Vibing (3s Esc to interrupt)";
        assert_eq!(spec.classify(working), Some(Observation::Working));
        let blocked = "⠋ Thinking (12s Esc to interrupt)\nPermission for the bash tool\n  › Yes\n    Allow for remainder of this session\n    Always allow\n    No";
        assert_eq!(spec.classify(blocked), Some(Observation::Blocked));
        assert_eq!(
            spec.classify("plain output\nnothing special"),
            None,
            "no idle glyph means no Done marker"
        );
    }

    /// Screens captured from Ori 0.13.0 driving a free model in a pty: the
    /// input box carries the state — the steer hint for a running turn, the
    /// placeholder once it lands — and the approval dialog outranks both.
    #[test]
    fn ori_screen_markers_classify_working_done_and_blocked() {
        let spec = Registry::default().match_process("ori").unwrap().clone();
        let working = " ❯ write a story\n ✻ Thinking…\n╭────╮\n│❯ Queue · ↑↓ select · x delete · Enter steer · Esc │\n╰────╯\n  liquid/lfm-2.5-2.6b:free  ori   ⏱ high";
        assert_eq!(spec.classify(working), Some(Observation::Working));
        let idle = " ⏺ ok\n   ↑ 101  ↓ 105  cache 5.6k  1.7s  61 tok/s\n╭────╮\n│❯ Ask Ori anything…   │\n╰────╯\n  liquid/lfm-2.5-2.6b:free  ori   ⏱ high  ○ 5%  Σt1.7s";
        assert_eq!(spec.classify(idle), Some(Observation::Done));
        let blocked = " ⏺ Thought briefly, running 1 command\n   ⎿ $ ls .\n┌────┐\n│ Permission requested │\n│ bash command: ls . │\n│  ▶ Allow once │\n│    Allow always │\n│    Reject once │\n│    Reject always │\n│ ↑/↓ choose · Enter confirm · Esc cancel │\n└────┘\n  liquid/lfm-2.5-2.6b:free  ori   ⏱ high";
        assert_eq!(spec.classify(blocked), Some(Observation::Blocked));
        // The answered dialog leaves a lowercase echo in the transcript that
        // must not read as Blocked once the turn has landed.
        let answered = " ⏺ Thought briefly, ran 1 command\n ⚠ permission requested: bash command: ls .\n ✻ Thought briefly\n ⏺ ok\n╭────╮\n│❯ Ask Ori anything…   │\n╰────╯\n  liquid/lfm-2.5-2.6b:free  ori   ⏱ high";
        assert_eq!(spec.classify(answered), Some(Observation::Done));
    }

    #[test]
    fn spec_looks_up_by_kind() {
        let reg = Registry::default();
        assert_eq!(
            reg.spec(&AgentKind::from("codex")).map(|s| &s.kind),
            Some(&AgentKind::from("codex"))
        );
        assert!(reg.spec(&AgentKind::from("nope")).is_none());
    }

    /// Each agent takes its first message the way its own CLI does — after
    /// its launch argv — and one whose interactive mode takes none, or whose
    /// way is unverified, can't be deployed to.
    #[test]
    fn agents_take_a_first_message_their_own_way() {
        let reg = Registry::default();
        let argv = |kind: &str| reg.spec(&AgentKind::from(kind))?.prompted_argv("fix it");
        for (kind, expected) in [
            ("claude", &["claude", "fix it"][..]),
            ("codex", &["codex", "fix it"]),
            ("gemini", &["gemini", "--prompt-interactive", "fix it"]),
            ("opencode", &["opencode", "--prompt", "fix it"]),
            (
                "ori",
                &["ori", "code", "--interactive", "--prompt", "fix it"],
            ),
            ("vibe", &["vibe", "fix it"]),
            ("muse", &["muse", "fix it"]),
        ] {
            assert_eq!(argv(kind).expect(kind), expected);
        }
        for kind in ["crush", "aider", "goose", "amazon-q"] {
            assert_eq!(argv(kind), None, "{kind}");
        }
        assert_eq!(
            reg.spec(&AgentKind::from("ori")).unwrap().argv(),
            ["ori", "code"],
            "a plain launch runs the binary and its launch args"
        );
    }

    #[test]
    fn registry_is_extensible() {
        let mut reg = Registry::default();
        let seeded = reg.specs().len();
        reg.add(AgentSpec::new(
            "myagent",
            "My Agent",
            "https://example.com/myagent",
            &["myagent-cli"],
            &["Approve?"],
            &["Generating"],
            &["›"],
        ));
        assert_eq!(reg.specs().len(), seeded + 1);
        let spec = reg.match_process("/usr/bin/myagent-cli").unwrap();
        assert_eq!(spec.kind, AgentKind::from("myagent"));
        assert_eq!(spec.classify("Generating code"), Some(Observation::Working));
    }

    #[test]
    fn catalog_agents_match_by_process_name() {
        let reg = Registry::default();
        for (process, kind) in [
            ("crush", "crush"),
            ("opencode", "opencode"),
            ("pi", "pi"),
            ("gemini", "gemini"),
            ("aider", "aider"),
            ("goose", "goose"),
            ("qwen", "qwen"),
            ("cursor-agent", "cursor"),
            ("copilot", "copilot"),
            ("amp", "amp"),
            ("droid", "droid"),
            ("auggie", "auggie"),
            ("q", "amazon-q"),
        ] {
            let spec = reg.match_process(process).unwrap_or_else(|| {
                panic!("{process} should match a catalog entry");
            });
            assert_eq!(spec.kind, AgentKind::from(kind));
            assert!(!spec.display.is_empty() && spec.url.starts_with("https://"));
            assert!(
                spec.done_patterns.is_empty(),
                "catalog newcomers carry no done marker until tuned live"
            );
        }
    }
}
