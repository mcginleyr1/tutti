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
    pub process_names: Vec<String>,
    pub blocked_patterns: Vec<String>,
    pub working_patterns: Vec<String>,
    pub done_patterns: Vec<String>,
}

impl AgentSpec {
    /// Prompts, permission dialogs, and idle markers render at the bottom of the
    /// screen, so `Blocked`/`Done` are detected only within the last
    /// `TAIL_LINES` visible lines. This keeps a prompt that has scrolled up into
    /// history from masking an active task running below it. `Working` markers
    /// can appear anywhere, so they are matched against the whole screen.
    const TAIL_LINES: usize = 15;

    pub fn new(
        kind: &str,
        process_names: &[&str],
        blocked_patterns: &[&str],
        working_patterns: &[&str],
        done_patterns: &[&str],
    ) -> Self {
        AgentSpec {
            kind: kind.into(),
            process_names: owned(process_names),
            blocked_patterns: owned(blocked_patterns),
            working_patterns: owned(working_patterns),
            done_patterns: owned(done_patterns),
        }
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
            .any(|p| screen_text.contains(p.as_str()))
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

impl Default for Registry {
    fn default() -> Self {
        Registry {
            specs: vec![
                AgentSpec::new(
                    "claude",
                    &["claude"],
                    &["Do you want", "y/n", "Waiting for your input", "permission"],
                    &["esc to interrupt", "Thinking", "Running"],
                    &["❯"],
                ),
                AgentSpec::new(
                    "codex",
                    &["codex"],
                    &["Allow command", "approve", "y/n"],
                    &["esc to interrupt", "Working", "Thinking"],
                    &["»"],
                ),
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
    fn working_marker_is_matched_across_whole_screen() {
        let screen = padded("Running the build", "log line", 20);
        assert_eq!(claude().classify(&screen), Some(Observation::Working));
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

    #[test]
    fn spec_looks_up_by_kind() {
        let reg = Registry::default();
        assert_eq!(
            reg.spec(&AgentKind::from("codex")).map(|s| &s.kind),
            Some(&AgentKind::from("codex"))
        );
        assert!(reg.spec(&AgentKind::from("nope")).is_none());
    }

    #[test]
    fn registry_is_extensible() {
        let mut reg = Registry::default();
        reg.add(AgentSpec::new(
            "cursor",
            &["cursor-agent"],
            &["Approve?"],
            &["Generating"],
            &["›"],
        ));
        assert_eq!(reg.specs().len(), 3);
        let spec = reg.match_process("/usr/bin/cursor-agent").unwrap();
        assert_eq!(spec.kind, AgentKind::from("cursor"));
        assert_eq!(spec.classify("Generating code"), Some(Observation::Working));
    }
}
