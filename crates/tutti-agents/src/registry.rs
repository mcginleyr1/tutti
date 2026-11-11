use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

/// The agents whose tutti hooks report nothing before their first turn:
/// Codex and Antigravity CLI send SessionStart only with the first prompt,
/// and opencode's plugin hears no event before a turn. An ask may take a
/// fresh one on its caller's word that the hooks are installed; every other
/// agent's hooks report as it starts, so one that has not reported never
/// will.
pub const FIRST_TURN_REPORTERS: &[&str] = &["codex", "opencode", "antigravity"];

/// The agents whose hooks say nothing of a permission dialog: Antigravity
/// CLI's `PreToolUse` comes before its dialog and no event with it (observed
/// live, 1.5.6). While their hooks hold such a pane working, its screen still
/// says when it waits on you.
pub const SCREEN_PROMPTERS: &[&str] = &["antigravity"];

/// The name detection knows a process by: its executable's `name`, or — for
/// a node or bun program, which runs as `node [flags] <script>` or `bun
/// [flags] <script>` — its script's, from `args` (the argv after the
/// executable). Gemini CLI runs so, then relaunches itself as `node
/// --max-old-space-size=… <script>`: its executable is only ever `node`. Oh
/// My Pi installed by `bun install -g` is a `#!/usr/bin/env bun` script, so
/// it runs as `bun ~/.bun/bin/omp`. A flag's value given as the argument
/// after it (`node --require x <script>`) is never taken for the script.
pub fn program_name<'a>(name: &'a str, mut args: impl Iterator<Item = &'a str>) -> &'a str {
    let value_flags = match name {
        "node" => NODE_VALUE_FLAGS,
        "bun" => BUN_VALUE_FLAGS,
        _ => return name,
    };
    while let Some(arg) = args.next() {
        if value_flags.contains(&arg) {
            args.next();
        } else if !arg.starts_with('-') {
            return arg;
        }
    }
    name
}

/// Node's options that take a value, which may come as the next argument:
/// those `node --help` lists as `--flag=...`, but for the profiler, report,
/// test-runner, snapshot and TLS ones no agent's launcher passes.
const NODE_VALUE_FLAGS: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "-C",
    "--conditions",
    "-e",
    "--eval",
    "--env-file",
    "--env-file-if-exists",
    "--experimental-config-file",
    "--disable-warning",
    "--input-type",
    "--title",
    "--inspect-port",
    "--debug-port",
    "--watch-path",
    "--redirect-warnings",
    "--unhandled-rejections",
    "--dns-result-order",
    "--max-http-header-size",
    "--icu-data-dir",
    "--openssl-config",
    "--localstorage-file",
];

/// Bun's options that take a value, which may come as the next argument:
/// those `bun --help` (1.4.4) lists as `--flag=<val>`, but for the profiler,
/// inspector, cron and package-script ones no agent's launcher passes. Not
/// node's: bun's `-c` is `--config`, node's `--check`.
const BUN_VALUE_FLAGS: &[&str] = &[
    "-r",
    "--preload",
    "--require",
    "--import",
    "-e",
    "--eval",
    "-p",
    "--print",
    "--install",
    "--port",
    "--conditions",
    "--fetch-preconnect",
    "--max-http-header-size",
    "--dns-result-order",
    "--redirect-warnings",
    "--disable-warning",
    "--title",
    "--unhandled-rejections",
    "--console-depth",
    "--user-agent",
    "--main-fields",
    "--extension-order",
    "--tsconfig-override",
    "-d",
    "--define",
    "--drop",
    "--feature",
    "-l",
    "--loader",
    "--jsx-factory",
    "--jsx-fragment",
    "--jsx-import-source",
    "--jsx-runtime",
    "--env-file",
    "--cwd",
    "-c",
    "--config",
];

/// How an agent's interactive mode takes its first message from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptFile {
    /// These args, then the file's path, in place of its prompt flags and
    /// the message: the file's text is the message (Ori's `--prompt-file`,
    /// Goose's `--instructions`).
    Flags(Vec<String>),
    /// `@<path>` after its prompt flags, then `ATTACHED_TASK`: the file rides
    /// in the first message, which points at it (Pi's and Augment's `@file`).
    Attached,
}

/// The first message beside an attached prompt file (`PromptFile::Attached`).
pub const ATTACHED_TASK: &str = "Your task is in the attached file. Do what it says.";

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
    /// What detection knows the agent's process by (`names`): a basename,
    /// or — with a `/` — the end of a node program's script path, `*`
    /// standing for any one directory.
    pub process_names: Vec<String>,
    /// The commands launching it, the preferred first (`installed`): its
    /// process names, unless it is installed under others.
    pub binaries: Vec<String>,
    /// Argv after the binary for the launcher — empty for agents that open
    /// on a bare run; `ori` needs `code` or it just prints its help.
    pub launch_args: Vec<String>,
    /// How the agent's interactive mode takes a first message on its command
    /// line: these args, then the message — empty for a positional prompt
    /// (`claude <prompt>`), `--prompt` for opencode. `None` when it takes
    /// none, so nothing can be deployed to it.
    pub prompt_flags: Option<Vec<String>>,
    /// How its interactive mode takes a first message from a file, for one
    /// too long to ride an argument well; `None` when it can't.
    pub prompt_file: Option<PromptFile>,
    /// What the screen's tail shows in each state. A pattern of several
    /// lines (`\n`-separated) is a dialog's phrase or option over the footer
    /// it is drawn with, or an idle prompt over its footer: its last line
    /// must show on the tail's bottom line and the lines before it above, in
    /// order. A transcript quoting the phrase sits above the agent's own
    /// prompt and footer, so it matches nothing.
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
            binaries: owned(process_names),
            launch_args: Vec::new(),
            prompt_flags: None,
            prompt_file: None,
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

    /// Whether `program` is this agent's: a process's name as detection
    /// knows it (`program_name`), or a path. A process name without a `/`
    /// matches the basename; one with a `/` matches the path's end, so
    /// Cursor's script — `node …/cursor-agent/versions/<version>/index.js` —
    /// is Cursor's while a bare `index.js` is nobody's.
    pub fn names(&self, program: &str) -> bool {
        self.process_names.iter().any(|name| {
            let mut have = Path::new(program).components().rev();
            Path::new(name).components().rev().all(|want| {
                have.next()
                    .is_some_and(|got| want.as_os_str() == "*" || got == want)
            })
        })
    }

    /// The first of its binaries installed on `path` (`which`) as this
    /// agent's, as an argv names it. One detection knows by name counts as
    /// found and stays bare. Any other — Cursor's `agent`, a name plenty of
    /// programs answer to — counts only when the file it resolves to is one
    /// detection knows (Cursor's installer links `agent` to its
    /// `cursor-agent`), and is named by the path checked here: the daemon
    /// spawns the argv on its own PATH, which may find another `agent`.
    pub fn installed(&self, path: Option<&OsStr>) -> Option<String> {
        self.binaries.iter().find_map(|binary| {
            let found = which(binary, path)?;
            if self.names(binary) {
                return Some(binary.clone());
            }
            let real = found.canonicalize().ok()?;
            self.names(&real.to_string_lossy())
                .then(|| found.to_string_lossy().into_owned())
        })
    }

    /// The argv launching the agent from `path`, the `PATH` value: the binary
    /// installed there (`installed`) — or, with none, or no PATH to look on
    /// (a sandbox's image), the first one detection knows by name, which is
    /// what declares a sandbox pane's agent — then its launch args.
    pub fn argv(&self, path: Option<&OsStr>) -> Vec<String> {
        let binary = self
            .installed(path)
            .or_else(|| self.binaries.iter().find(|b| self.names(b)).cloned())
            .unwrap_or_else(|| self.kind.to_string());
        std::iter::once(binary)
            .chain(self.launch_args.iter().cloned())
            .collect()
    }

    /// This spec taking a first message from a file (see `prompt_file`).
    fn filed(self, file: PromptFile) -> Self {
        AgentSpec {
            prompt_file: Some(file),
            ..self
        }
    }

    /// The args after its argv handing the agent `prompt` as its first
    /// message, or `None` when it takes no prompt.
    pub fn prompt_args(&self, prompt: &str) -> Option<Vec<String>> {
        let flags = self.prompt_flags.as_ref()?;
        Some(flags.iter().cloned().chain([prompt.to_string()]).collect())
    }

    /// The args after its argv handing the agent the first message `file`
    /// holds, through its own file input; `None` when it has none.
    pub fn prompt_file_args(&self, file: &Path) -> Option<Vec<String>> {
        let file = file.to_string_lossy();
        match self.prompt_file.as_ref()? {
            PromptFile::Flags(flags) => {
                Some(flags.iter().cloned().chain([file.into_owned()]).collect())
            }
            PromptFile::Attached => {
                let flags = self.prompt_flags.as_ref()?;
                let attached = [format!("@{file}"), ATTACHED_TASK.to_string()];
                Some(flags.iter().cloned().chain(attached).collect())
            }
        }
    }

    /// Blocked (needs input) outranks Working (active task) outranks Done, so the
    /// most attention-worthy signal wins when several match. `None` means no
    /// pattern matched and the caller should fall back to activity heuristics.
    pub fn classify(&self, screen_text: &str) -> Option<Observation> {
        let tail = tail(screen_text, Self::TAIL_LINES);
        let shows = |patterns: &[String]| patterns.iter().any(|p| drawn(tail, p));
        if shows(&self.blocked_patterns) {
            Some(Observation::Blocked)
        } else if shows(&self.working_patterns) {
            Some(Observation::Working)
        } else if shows(&self.done_patterns) {
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
    /// Look up the spec for a process by its name (`AgentSpec::names`), so an
    /// absolute path like `/usr/local/bin/claude` matches the `claude` entry.
    pub fn match_process(&self, process_name: &str) -> Option<&AgentSpec> {
        self.specs.iter().find(|spec| spec.names(process_name))
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
        // How each agent's interactive mode takes a first message, as the
        // note on its row says: from its own `--help` where it is installed
        // here (checked 2026-11-04, versions named), else from its own docs,
        // quoted. Only Ori (`--prompt-file`), Pi, Oh My Pi and Augment
        // (`@file`) and Goose (`--instructions <file>`) take it from a file while staying
        // interactive (`prompt_file`, which a long message rides), and only
        // Amp from stdin; any other agent is told to read a message too long
        // for an argument from a file (tutti's `issues::deploy_argv`).
        Registry {
            specs: vec![
                // Observed live at 80 columns, Claude Code 2.1.318 in both its
                // renderers: no `esc to interrupt` any more; a running turn's
                // spinner reads `✶ Dilly-dallying… (12s · ↓ 98 tokens)` or
                // `(3s · ↓ 345 tokens · thinking)` once tokens flow, an ended
                // one `✻ Brewed for 23s · done 9:43 AM`. A new folder's
                // trust dialog waits on you: its option `Yes, I trust this
                // folder` over its footer `Enter to confirm · Esc to cancel`.
                AgentSpec::new(
                    "claude",
                    "Claude Code",
                    "https://claude.com/claude-code",
                    &["claude"],
                    &[
                        "Do you want",
                        "y/n",
                        "Waiting for your input",
                        "permission",
                        "Yes, I trust this folder\nEnter to confirm · Esc to cancel",
                    ],
                    &["esc to interrupt", "Thinking", "Running", "· ↓ "],
                    &["❯"],
                )
                // `--help` (2.1.318): `claude [prompt]` "starts an interactive
                // session by default".
                .prompted(&[]),
                // `--no-daemon`: Codex's shared app-server (its default since
                // 0.16x) runs every session's hooks with the environment of
                // whichever codex started it, so their `TUTTI_PANE` would be
                // another pane's. Embedded, they are this pane's. Observed
                // live, 0.166.0: idle, the composer (`› Ask Codex to do
                // anything`, or what you typed) over its `? for shortcuts`
                // footer on the bottom line — its dialogs and its hooks
                // browser draw `›` over other footers. Each dialog waits on you, matched by an
                // option over its footer: the approval (`3. No, and tell Codex
                // what to do differently (esc)` / `Press enter to confirm or
                // esc to cancel`), folder trust (`1. Trust and continue` /
                // `enter continue · esc quit`), hook review (`Hooks need
                // review` / `3. Continue without trusting` / `enter confirm ·
                // esc skip`), each footer the screen's bottom line. An answered
                // approval leaves `✔ You approved codex to run …` in the
                // transcript, so no bare `approve`, and a lone phrase quoted
                // in a reply matches nothing.
                AgentSpec {
                    launch_args: owned(&["--no-daemon"]),
                    ..AgentSpec::new(
                        "codex",
                        "Codex CLI",
                        "https://github.com/openai/codex",
                        &["codex"],
                        &[
                            "y/n",
                            "tell Codex what to do differently\nPress enter to confirm or esc to cancel",
                            "Trust and continue\nenter continue · esc quit",
                            "Hooks need review\nContinue without trusting\nenter confirm · esc skip",
                        ],
                        &["esc to interrupt", "Working", "Thinking"],
                        &["› \n? for shortcuts"],
                    )
                }
                // `--help` (0.166.0): `codex [PROMPT]`, "Optional user prompt
                // to start the session".
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
                // `--help` (0.66.0): "Execute the provided prompt and continue
                // in interactive mode".
                .prompted(&["--prompt-interactive"]),
                // Observed live (Antigravity CLI 1.5.6 in a pty): a native
                // binary that runs as `agy`. The left of its footer, the
                // screen's bottom line, carries the state: `? for shortcuts`
                // under the idle composer (`>` between two rules), `esc to
                // cancel` under it for the whole turn, and `esc to cancel`
                // under a dialog too, whose options sit over a `↑/↓ Navigate
                // · …` hint in the composer's place (`Run this command?`,
                // `Allow creation of this file?`). A new folder's trust
                // dialog has only the model's name under it, so nothing ties
                // its question to the bottom line.
                AgentSpec::new(
                    "antigravity",
                    "Antigravity CLI",
                    "https://antigravity.google",
                    &["agy"],
                    &[
                        "↑/↓ Navigate\nesc to cancel",
                        "Do you trust the contents of this project?",
                    ],
                    &["────\n>\n────\nesc to cancel"],
                    &["────\n>\n────\n? for shortcuts"],
                )
                // `--help` (1.5.5): `--prompt-interactive`, "Run an initial
                // prompt interactively and continue the session"; run live
                // on 1.5.6.
                .prompted(&["--prompt-interactive"]),
                // `--help` (1.18.5): `--prompt`, "prompt to use"; its TUI
                // submits it once a model is ready.
                guess("opencode", "OpenCode", "https://opencode.ai", &["opencode"])
                    .prompted(&["--prompt"]),
                // `--help` (0.87.0): a bare `crush` takes no prompt; `crush
                // run` is headless.
                guess(
                    "crush",
                    "Crush",
                    "https://github.com/charmbracelet/crush",
                    &["crush"],
                ),
                // Docs (aider.chat/docs/config/options.html): `--message` and
                // `--message-file` "process reply then exit (disables chat
                // mode)"; nothing sends one and stays.
                guess("aider", "Aider", "https://aider.chat", &["aider"]),
                // Docs (the goose CLI commands guide): `goose run --text` is
                // "Input text to provide to goose directly", and
                // `--interactive` will "Continue in interactive mode after
                // processing initial input". The `goose run` man page:
                // `--instructions` is "Path to instruction file containing
                // commands", and the running-tasks guide shows `goose run -i
                // instructions.txt -s`. Not run here.
                guess(
                    "goose",
                    "Goose",
                    "https://github.com/block/goose",
                    &["goose"],
                )
                .prompted(&["run", "--interactive", "--text"])
                .filed(PromptFile::Flags(owned(&[
                    "run",
                    "--interactive",
                    "--instructions",
                ]))),
                // `--help` (0.83.0): `pi "List all .ts files in src/"` is
                // "Interactive mode with initial prompt". Docs (pi-mono's
                // docs/cli.md): `pi [options] [--] [@files...] [messages...]`,
                // `@path` to "Include a text file or image in the first
                // prompt".
                guess("pi", "Pi", "https://github.com/badlogic/pi-mono", &["pi"])
                    .prompted(&[])
                    .filed(PromptFile::Attached),
                // Oh My Pi is a fork of Pi with its own binary, `omp`.
                // Observed (2026-11-08, both in a pty): Homebrew's 18.16.0 is a
                // compiled executable that runs as `omp`, and `bun install -g`'s
                // 18.13.1 is a `#!/usr/bin/env bun` script that runs as `bun
                // ~/.bun/bin/omp` (`program_name`). Its idle composer has no
                // fixed glyph, and no turn ran here (no model was logged in),
                // so it gets the guess patterns. `--help` (18.16.0): `omp "List
                // all .ts files in src/"` is "Interactive mode with initial
                // prompt", and `omp @prompt.md @image.png "What color is the
                // sky?"` will "Include files in initial message".
                guess(
                    "omp",
                    "Oh My Pi",
                    "https://github.com/can1357/oh-my-pi",
                    &["omp"],
                )
                .prompted(&[])
                .filed(PromptFile::Attached),
                // Prime Agent began as a fork of Pi but runs as its own
                // binary — its installer's `prime-agent` launcher execs
                // `share/prime-agent/prime-agent` — so it never reads as Pi.
                // It reports to herdr as agent `prime-agent`, the kind named
                // here, so detection catching up keeps that claim. Docs (its
                // README): a bare `prime-agent` opens the TUI and `--resume`
                // takes a session; no first-message argument is documented,
                // so none. Not run here.
                guess(
                    "prime-agent",
                    "Prime Agent",
                    "https://github.com/PrimeIntellect-ai/prime-agent",
                    &["prime-agent"],
                ),
                // Docs (Qwen Code's settings reference): `--prompt-interactive`
                // "Starts an interactive session with the provided prompt as the
                // initial input". Not run here.
                guess(
                    "qwen",
                    "Qwen Code",
                    "https://github.com/QwenLM/qwen-code",
                    &["qwen"],
                )
                .prompted(&["--prompt-interactive"]),
                // Cursor's installer (cursor.com/install, read 2026-11-05)
                // links `~/.local/bin/agent` ("primary") and `cursor-agent`
                // ("legacy") to `~/.local/share/cursor-agent/versions/<v>/
                // cursor-agent`, a bash script that `exec -a "$0"`s its
                // bundled `node` on the `index.js` beside it: the process is
                // `node` on that script, whichever name started it. The
                // launcher runs `agent` where it is Cursor's (`installed`),
                // by the path it checked, else `cursor-agent`. Docs (cursor.com/docs/cli/reference/
                // parameters): `prompt`, "Initial prompt for the agent", in
                // the agent mode it starts interactively by default. Not run
                // here.
                AgentSpec {
                    binaries: owned(&["agent", "cursor-agent"]),
                    ..guess(
                        "cursor",
                        "Cursor CLI",
                        "https://cursor.com/cli",
                        &["cursor-agent", "cursor-agent/versions/*/index.js"],
                    )
                }
                .prompted(&[]),
                // Docs (GitHub's Copilot CLI command reference): `-i PROMPT`,
                // "Start an interactive session and automatically execute this
                // prompt". Not run here.
                guess(
                    "copilot",
                    "Copilot CLI",
                    "https://github.com/github/copilot-cli",
                    &["copilot"],
                )
                .prompted(&["-i"]),
                // Docs (ampcode.com/docs/cli): only piped stdin becomes "the
                // first user message in interactive mode", and a pane's stdin
                // is its terminal; `-x` is headless. So none.
                guess("amp", "Amp", "https://ampcode.com", &["amp"]),
                // Docs (Factory's CLI reference): `droid "query"`, "Start REPL
                // with initial prompt". Not run here.
                guess("droid", "Factory Droid", "https://factory.ai", &["droid"]).prompted(&[]),
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
                // `--help` (1.4.14): "pass a prompt to start a session".
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
                // `--help` (3.0.3): `[INITIAL_PROMPT]`, "Initial prompt for the
                // interactive session".
                .prompted(&[]),
                // Docs (docs.augmentcode.com/cli/reference): `auggie "Fix the
                // typescript errors"`, "Provide an initial instruction in
                // interactive mode", and `auggie @prompt.md @screenshot.png
                // "Explain this UI error"`, "Include files and images in the
                // initial message". Not run here.
                guess(
                    "auggie",
                    "Augment Code",
                    "https://www.augmentcode.com",
                    &["auggie"],
                )
                .prompted(&[])
                .filed(PromptFile::Attached),
                // Amazon Q's binary is literally `q`, which can collide with
                // other tools of that name (kdb+); the cost of a false match is
                // only a mislabelled badge, so it stays in. Docs (the AWS
                // command reference, read through a search excerpt — the page
                // itself did not render here): `q chat [INPUT]`, "The first
                // question to ask", interactive unless `--no-interactive`. Not
                // run here.
                guess(
                    "amazon-q",
                    "Amazon Q",
                    "https://github.com/aws/amazon-q-developer-cli",
                    &["q"],
                )
                .prompted(&["chat"]),
                // Ori's bare binary is a harness launcher (`ori claude`,
                // `ori codex`, …) — the agent it wraps is detected as itself,
                // deeper in the tree — so the launcher runs `ori code`, its
                // own agent. Observed live (0.13.0 in a pty): the input box
                // reads `❯ Ask Ori anything…` when idle and `❯ Queue · ↑↓
                // select · x delete · Enter steer · Esc` for the whole turn;
                // the approval dialog is titled `Permission requested` with
                // `Allow once` first. Its lowercase transcript echo,
                // `⚠ permission requested: …`, outlives the dialog — never a
                // Blocked marker. `code --help` (0.13.0): a bare `--prompt` runs
                // one headless turn; `--interactive` will "Open the interactive
                // TUI with --prompt/--prompt-file text as the first turn". Its
                // changelog (0.2.4): "An initial prompt accepts `--prompt`,
                // `-p`, or `--prompt-file`."
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
                .prompted(&["--interactive", "--prompt"])
                .filed(PromptFile::Flags(owned(&[
                    "--interactive",
                    "--prompt-file",
                ]))),
            ],
        }
    }
}

fn owned(patterns: &[&str]) -> Vec<String> {
    patterns.iter().map(|s| s.to_string()).collect()
}

/// The executable file `name` finds on `path` — the `PATH` value, os-split —
/// as a shell would. Pure over the passed value (it reads no global env), so
/// a test can point it at a fake PATH; an unset (`None`) path finds nothing.
fn which(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|file| {
            std::fs::metadata(file)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The trailing slice of `text` holding at most its last `max_lines` lines.
fn tail(text: &str, max_lines: usize) -> &str {
    text.match_indices('\n')
        .rev()
        .nth(max_lines.saturating_sub(1))
        .map_or(text, |(idx, _)| &text[idx + 1..])
}

/// Whether `tail` shows `pattern` (see `AgentSpec::blocked_patterns`): one
/// line anywhere, or several — the last on the tail's bottom line, the ones
/// before it in their order.
fn drawn(tail: &str, pattern: &str) -> bool {
    let Some((above, bottom)) = pattern.rsplit_once('\n') else {
        return tail.contains(pattern);
    };
    let mut rest = tail;
    let last = tail.trim_end().rsplit('\n').next().unwrap_or_default();
    last.contains(bottom)
        && above.split('\n').all(|line| match rest.find(line) {
            Some(at) => {
                rest = &rest[at + line.len()..];
                true
            }
            None => false,
        })
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

    /// Claude Code 2.1.318's screens as observed live: an approved tool
    /// running under its spinner reads working in either renderer, the
    /// dialog before it blocked, and the turn's end done.
    #[test]
    fn claudes_spinner_reads_working_and_its_end_done() {
        let fullscreen = "❯ run it\n\n  Sleeping then creating a file · 15s\n  ⎿  $ sleep 15; touch b.txt (10s)\n     (ctrl+b to run in background)\n\n✶ Dilly-dallying… (12s · ↓ 98 tokens)\n\n────\n❯ \n────";
        let thinking = "⏺ Write(f1.txt)\n  ⎿  Wrote 1 line to f1.txt\n\n✢ Simmering… (3s · ↓ 345 tokens · thinking)\n\n────\n❯ \n────";
        let classic = "⏺ Bash(sleep 20; touch a.txt)\n  ⎿  (No output)\n\n✳ Ebbing… (1s · ↓ 96 tokens)\n\n────\n❯ \n────";
        let dialog = "⏺ Bash(sleep 20; touch a.txt)\n  ⎿  Waiting…\n────\n Bash command\n sleep 20; touch a.txt\n Do you want to proceed?\n ❯ 1. Yes\n   4. No\n\n Esc to cancel · Tab to amend";
        let ended =
            "⏺ Ran sleep 20; touch a.txt.\n\n✻ Brewed for 23s · done 9:43 AM\n\n────\n❯ \n────";
        assert_eq!(claude().classify(fullscreen), Some(Observation::Working));
        assert_eq!(claude().classify(thinking), Some(Observation::Working));
        assert_eq!(claude().classify(classic), Some(Observation::Working));
        assert_eq!(claude().classify(dialog), Some(Observation::Blocked));
        assert_eq!(claude().classify(ended), Some(Observation::Done));
    }

    /// Claude Code 2.1.318 in a folder it has not been trusted in: the
    /// trust dialog waits on you, its `❯` menu cursor no turn's end.
    #[test]
    fn claudes_folder_trust_dialog_reads_blocked() {
        let trust = " Quick safety check: Is this a project you created or one you trust?\n\n Claude Code'll be able to read, edit, and execute files here.\n\n Security guide\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm · Esc to cancel";
        assert_eq!(claude().classify(trust), Some(Observation::Blocked));
    }

    /// Codex 0.166.0's screens as observed live: a running turn, its end at
    /// the `›` composer, its approval, folder-trust and hook-review dialogs,
    /// and the transcript an answered approval leaves, which is no dialog.
    #[test]
    fn codex_screen_markers_classify_working_done_and_blocked() {
        let spec = Registry::default().match_process("codex").unwrap().clone();
        let working = "• I’ll add sq(x) to calc.py without running commands.\n\n• Working (4s • esc to interrupt)\n\n\n› Ask Codex to do anything\n\n  GPT · ~/repo-b\n  ? for shortcuts";
        let ended = "• Added sq(x) returning x * x in calc.py without running commands.\n\n  Worked for 19s • 10:01 AM\n\n\n› Ask Codex to do anything\n\n  GPT · ~/repo-b\n  ? for shortcuts";
        let approval = "• Running touch hello.txt\n\n\n  Would you like to run the following command?\n\n  Reason: May I run touch hello.txt outside the read-only sandbox?\n\n  $ touch hello.txt\n\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `touch hello.txt` (p)\n  3. No, and tell Codex what to do differently (esc)\n\n  Press enter to confirm or esc to cancel";
        let trust = "  Trust this folder? Codex can read, edit, and run files here, subject to your\n  permission settings. Your trust decision will be saved.\n\n› 1. Trust and continue\n  2. Quit\n\n  enter continue · esc quit";
        let hooks = "  Hooks need review\n  15 hooks are new or changed.\n  Hooks can run outside the sandbox after you trust them.\n\n\n› 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting (hooks won't run)\n\n  enter confirm · esc skip";
        let approved = "✔ You approved codex to run touch hello.txt this time\n\n• Ran touch hello.txt\n  └ (no output)\n\n• Created hello.txt after approval.\n\n  Worked for 25s • 10:03 AM\n\n\n› Ask Codex to do anything\n\n  GPT · ~/repo-b\n  ? for shortcuts";
        assert_eq!(spec.classify(working), Some(Observation::Working));
        assert_eq!(spec.classify(ended), Some(Observation::Done));
        assert_eq!(spec.classify(approval), Some(Observation::Blocked));
        assert_eq!(spec.classify(trust), Some(Observation::Blocked));
        assert_eq!(spec.classify(hooks), Some(Observation::Blocked));
        assert_eq!(spec.classify(approved), Some(Observation::Done));
    }

    /// A dialog's words quoted in a reply — option and footer both, in the
    /// last lines of a finished turn, its idle prompt below — are no dialog:
    /// each blocked marker needs the live dialog's footer on the screen's
    /// bottom line. Codex's `›` over another footer, as its dialogs and its
    /// hooks browser draw it, is no idle composer either.
    #[test]
    fn a_reply_quoting_a_dialog_reads_as_the_screen_it_is() {
        let claude_reply = "⏺ Its dialog offers \"Yes, I trust this folder\" over\n  \"Enter to confirm · Esc to cancel\".\n\n✻ Brewed for 3s · done 9:43 AM\n\n────\n❯ \n────";
        assert_eq!(claude().classify(claude_reply), Some(Observation::Done));
        let codex = Registry::default().match_process("codex").unwrap().clone();
        for quoted in [
            "3. No, and tell Codex what to do differently (esc)\n  Press enter to confirm or esc to cancel",
            "Trust this folder? 1. Trust and continue\n  enter continue · esc quit",
            "Hooks need review\n  3. Continue without trusting\n  enter confirm · esc skip",
            "Allow command?",
        ] {
            let reply = format!(
                "• Codex's dialog reads \"{quoted}\".\n\n  Worked for 3s • 10:01 AM\n\n\n› Ask Codex to do anything\n\n  GPT · ~/repo-b\n  ? for shortcuts"
            );
            assert_eq!(codex.classify(&reply), Some(Observation::Done), "{quoted}");
        }
        let browser = "  PreToolUse hooks\n  2 hooks need review before they can run.\n\n› [!]  1  Unnamed hook · new\n  [!]  2  Unnamed hook · new\n\n  t trust · esc back";
        assert_eq!(codex.classify(browser), None);
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

    /// Gemini CLI's process tree as observed (0.66.0): `node <script>`, and
    /// the child it relaunches with a bigger heap.
    #[test]
    fn a_node_program_is_known_by_its_script() {
        let args = |line: &'static str| line.split(' ');
        let script = "/usr/local/lib/node_modules/.bin/gemini";
        let relaunch = "--max-old-space-size=24576 /usr/local/lib/node_modules/.bin/gemini";
        assert_eq!(program_name("node", args(script)), script);
        assert_eq!(program_name("node", args(relaunch)), script);
        let preloaded = "-r ./trace.js --import tsx --env-file .env --no-warnings \
                         /usr/local/lib/node_modules/.bin/gemini";
        assert_eq!(
            program_name("node", args(preloaded)),
            script,
            "values skipped"
        );
        assert_eq!(program_name("node", std::iter::empty()), "node", "a REPL");
        assert_eq!(program_name("node", args("--require x")), "node", "a REPL");
        assert_eq!(program_name("claude", args("--resume s1")), "claude");

        let mut tree = ProcessTree::default();
        tree.insert(100, None, "zsh");
        tree.insert(101, Some(100), program_name("node", args(script)));
        tree.insert(102, Some(101), program_name("node", args(relaunch)));
        assert_eq!(
            Registry::default().detect(&tree, 100),
            Some(AgentKind::from("gemini"))
        );
    }

    /// Oh My Pi's processes as observed: Homebrew's compiled `omp`, and `bun
    /// install -g`'s script, which its `#!/usr/bin/env bun` runs as `bun
    /// ~/.bun/bin/omp`. A bun flag's value is never the script, and bun's
    /// `-c` takes one where node's does not.
    #[test]
    fn a_bun_program_is_known_by_its_script() {
        let args = |line: &'static str| line.split(' ');
        let script = "/Users/u/.bun/bin/omp";
        assert_eq!(program_name("bun", args(script)), script);
        let flagged = "--smol -r ./trace.ts --cwd /tmp -c bunfig.toml /Users/u/.bun/bin/omp";
        assert_eq!(program_name("bun", args(flagged)), script, "values skipped");
        assert_eq!(program_name("bun", std::iter::empty()), "bun", "a REPL");
        assert_eq!(program_name("node", args("-c app.js")), "app.js");

        let reg = Registry::default();
        for program in [program_name("bun", args(script)), "/opt/homebrew/bin/omp"] {
            let mut tree = ProcessTree::default();
            tree.insert(100, None, "zsh");
            tree.insert(101, Some(100), program);
            assert_eq!(
                reg.detect(&tree, 100),
                Some(AgentKind::from("omp")),
                "{program}"
            );
        }
        assert_eq!(reg.match_process("bun").map(|s| &s.kind), None);
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

    /// Screens captured from Antigravity CLI 1.5.6 in a pty: a running turn
    /// over its composer, the command and file approvals, a new folder's
    /// trust dialog, and the turn's end at the idle composer.
    #[test]
    fn antigravity_screen_markers_classify_working_done_and_blocked() {
        let spec = Registry::default().match_process("agy").unwrap().clone();
        assert_eq!(spec.kind, AgentKind::from("antigravity"));
        let rule = "────────────────────────────────────────";
        let footer = "                    Gemini 3.8 Flash · high";
        let echo = format!("{rule}\n> Run the shell command `sleep 6; echo tutti-probe`.\n\n");
        let composer = format!("{rule}\n>\n{rule}\n");
        let working = format!(
            "{echo}● Running (sleep 6; echo tutti-probe) (ctrl+o to expand)\n⢿  Running command...\n{composer}esc to cancel{footer}"
        );
        let command = format!(
            "{echo}● Ran (sleep 6; echo tutti-probe) (ctrl+o to expand)\n\nCommand\n{rule}\n\nRequesting permission for:\n   sleep 6; echo tutti-probe\n\nRun this command?\n> 1. Yes, run command\n  4. No, cancel\n\n  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\nesc to cancel{footer}"
        );
        let file = format!(
            "{echo}● Edited 1 file (probe.txt) (ctrl+o to expand)\n\nCreate file\n{rule}\n\n  1  +  hi\n\nAllow creation of this file?\n> 1. Yes, allow creation\n  2. No, deny creation\n\n  ↑/↓ Navigate · f full diff\nesc to cancel{footer}"
        );
        let trust = format!(
            "Accessing workspace:\n\n/tmp/proj\n\nDo you trust the contents of this project?\n\nAntigravity CLI requires permission to read, edit, and execute files here.\n\n> Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm\n{footer}"
        );
        let ended = format!(
            "{echo}● Ran (sleep 6; echo tutti-probe) (ctrl+o to expand)\n\n  The command completed and printed:\n\n    tutti-probe\n\n{composer}? for shortcuts{footer}"
        );
        assert_eq!(spec.classify(&working), Some(Observation::Working));
        assert_eq!(spec.classify(&command), Some(Observation::Blocked));
        assert_eq!(spec.classify(&file), Some(Observation::Blocked));
        assert_eq!(spec.classify(&trust), Some(Observation::Blocked));
        assert_eq!(spec.classify(&ended), Some(Observation::Done));
        // A reply naming the dialog's hint, over the idle footer, is no dialog.
        let quoted = format!(
            "{echo}  Its dialogs end in \"↑/↓ Navigate\" over \"esc to cancel\".\n\n{composer}? for shortcuts{footer}"
        );
        assert_eq!(spec.classify(&quoted), Some(Observation::Done));
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
        let argv = |kind: &str| {
            let spec = reg.spec(&AgentKind::from(kind))?;
            Some([spec.argv(None), spec.prompt_args("fix it")?].concat())
        };
        for (kind, expected) in [
            ("claude", &["claude", "fix it"][..]),
            ("codex", &["codex", "--no-daemon", "fix it"]),
            ("gemini", &["gemini", "--prompt-interactive", "fix it"]),
            ("antigravity", &["agy", "--prompt-interactive", "fix it"]),
            ("opencode", &["opencode", "--prompt", "fix it"]),
            (
                "ori",
                &["ori", "code", "--interactive", "--prompt", "fix it"],
            ),
            ("vibe", &["vibe", "fix it"]),
            ("muse", &["muse", "fix it"]),
            ("pi", &["pi", "fix it"]),
            ("omp", &["omp", "fix it"]),
            (
                "goose",
                &["goose", "run", "--interactive", "--text", "fix it"],
            ),
            ("qwen", &["qwen", "--prompt-interactive", "fix it"]),
            ("cursor", &["cursor-agent", "fix it"]),
            ("copilot", &["copilot", "-i", "fix it"]),
            ("droid", &["droid", "fix it"]),
            ("auggie", &["auggie", "fix it"]),
            ("amazon-q", &["q", "chat", "fix it"]),
        ] {
            assert_eq!(argv(kind).expect(kind), expected);
        }
        for kind in ["crush", "aider", "amp", "prime-agent"] {
            assert_eq!(argv(kind), None, "{kind}");
        }
        assert_eq!(
            reg.spec(&AgentKind::from("ori")).unwrap().argv(None),
            ["ori", "code"],
            "a plain launch runs the binary and its launch args"
        );
    }

    /// The five agents whose interactive mode reads a first message from a
    /// file take it their own documented way; every other agent has none.
    #[test]
    fn agents_with_a_file_input_take_a_first_message_from_a_file() {
        let reg = Registry::default();
        let file = Path::new("/run/tutti/t.prompts/prompt-1-2.md");
        let argv = |kind: &str| {
            let spec = reg.spec(&AgentKind::from(kind))?;
            Some([spec.argv(None), spec.prompt_file_args(file)?].concat())
        };
        let at = "@/run/tutti/t.prompts/prompt-1-2.md";
        for (kind, expected) in [
            (
                "ori",
                &[
                    "ori",
                    "code",
                    "--interactive",
                    "--prompt-file",
                    &*file.to_string_lossy(),
                ][..],
            ),
            (
                "goose",
                &[
                    "goose",
                    "run",
                    "--interactive",
                    "--instructions",
                    &*file.to_string_lossy(),
                ],
            ),
            ("pi", &["pi", at, ATTACHED_TASK]),
            ("omp", &["omp", at, ATTACHED_TASK]),
            ("auggie", &["auggie", at, ATTACHED_TASK]),
        ] {
            assert_eq!(argv(kind).expect(kind), expected);
        }
        let filed: Vec<&str> = reg
            .specs()
            .iter()
            .filter(|s| s.prompt_file.is_some())
            .map(|s| s.kind.0.as_str())
            .collect();
        assert_eq!(filed, ["goose", "pi", "omp", "auggie", "ori"]);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tutti-registry-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn which_finds_only_executables_on_the_path_given() {
        let dir = scratch("which");
        executable(&dir.join("myagent"));
        std::fs::write(dir.join("plainfile"), b"x").unwrap();
        let path = std::ffi::OsString::from(&dir);
        let path = Some(path.as_os_str());

        assert_eq!(which("myagent", path), Some(dir.join("myagent")));
        assert_eq!(which("missing", path), None);
        assert_eq!(which("plainfile", path), None, "not executable");
        assert_eq!(which("myagent", None), None, "an unset PATH finds nothing");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Cursor's process as its installer's launcher leaves it (read from
    /// the 2026.11.05 package): `exec -a "$0"` of its bundled `node` on the
    /// `index.js` beside it, argv[0] the name that started it. Detection
    /// knows it by that script's place, never by the generic name.
    #[test]
    fn cursor_is_known_by_its_scripts_place_never_by_a_name_like_agent() {
        let reg = Registry::default();
        let script = "/home/u/.local/share/cursor-agent/versions/2026.11.05-86047df/index.js";
        let argv = ["--use-system-ca", script, "fix it"];
        assert_eq!(program_name("node", argv.into_iter()), script);
        let mut tree = ProcessTree::default();
        tree.insert(100, None, "zsh");
        tree.insert(101, Some(100), program_name("node", argv.into_iter()));
        assert_eq!(reg.detect(&tree, 100), Some(AgentKind::from("cursor")));

        for nobody in [
            "agent",
            "/usr/local/bin/agent",
            "/srv/app/index.js",
            "index.js",
            "/opt/cursor-agent/index.js",
            "versions/1/index.js",
        ] {
            assert_eq!(reg.match_process(nobody).map(|s| &s.kind), None, "{nobody}");
        }
        assert_eq!(
            reg.match_process("/usr/bin/cursor-agent").map(|s| &s.kind),
            Some(&AgentKind::from("cursor"))
        );
    }

    /// The launcher runs `agent` where it is Cursor's — linked into Cursor's
    /// install as its installer leaves it — by the path it checked, so a
    /// daemon on another PATH runs that same `agent`; else `cursor-agent`,
    /// bare like every name detection knows. An unrelated `agent` never
    /// stands in for Cursor. With nothing to look on (a sandbox's image) it
    /// runs `cursor-agent`, the name that declares the pane's agent.
    #[test]
    fn cursor_launches_as_agent_only_where_agent_is_cursors() {
        let reg = Registry::default();
        let cursor = reg.spec(&AgentKind::from("cursor")).unwrap().clone();
        let base = scratch("cursor");
        let real = base.join("share/cursor-agent/versions/2026.11.05-86047df/cursor-agent");
        executable(&real);
        let (current, legacy, other) = (
            base.join("current"),
            base.join("legacy"),
            base.join("other"),
        );
        for dir in [&current, &legacy, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(&real, current.join("agent")).unwrap();
        std::os::unix::fs::symlink(&real, current.join("cursor-agent")).unwrap();
        std::os::unix::fs::symlink(&real, legacy.join("cursor-agent")).unwrap();
        executable(&other.join("agent"));
        let path = |dirs: &[&PathBuf]| std::env::join_paths(dirs).unwrap();
        let (both, old) = (path(&[&current]), path(&[&legacy]));
        let (shadowed, unrelated) = (path(&[&other, &legacy]), path(&[&other]));

        let checked = current.join("agent").to_string_lossy().into_owned();
        assert_eq!(
            cursor.installed(Some(both.as_os_str())),
            Some(checked.clone())
        );
        assert_eq!(cursor.argv(Some(both.as_os_str())), [checked]);
        assert_eq!(
            cursor.installed(Some(old.as_os_str())).as_deref(),
            Some("cursor-agent")
        );
        assert_eq!(
            cursor.installed(Some(shadowed.as_os_str())).as_deref(),
            Some("cursor-agent"),
            "the `agent` a shell would run is not Cursor's"
        );
        assert_eq!(cursor.installed(Some(unrelated.as_os_str())), None);
        assert_eq!(cursor.argv(Some(unrelated.as_os_str())), ["cursor-agent"]);
        let sandboxed = cursor.argv(None);
        assert_eq!(sandboxed, ["cursor-agent"]);
        assert_eq!(
            reg.match_process(&sandboxed[0]).map(|s| &s.kind),
            Some(&cursor.kind),
            "a sandbox pane's agent is declared from its program"
        );
        std::fs::remove_dir_all(&base).ok();
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
            ("omp", "omp"),
            ("prime-agent", "prime-agent"),
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
