use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use tutti_agents::Registry;
use tutti_core::{
    AgentHookEvent, ApprovalDecision, Direction, Event, PaneId, Request, Response, TabId, Toward,
    WaitFor, WorkspaceId, WorkspaceInfo,
};

use tutti::client::{self, Client, StopOutcome};
use tutti::config::{Approvals, Config, RESIZE_DELTA};
use tutti::editor;
use tutti::hooks;
use tutti::integrations::{self, Dirs, Integration, Shell, Status};
use tutti::issues;
use tutti::render;

#[derive(Parser)]
#[command(name = "tutti", version, about = "Terminal-native agent multiplexer")]
struct Cli {
    /// Session name (one daemon + socket per session). Inside a tutti pane it
    /// defaults to that pane's own session.
    #[arg(short, long, global = true, default_value_t = session_from(std::env::var("TUTTI_SESSION").ok()))]
    session: String,
    /// Print raw JSON responses instead of formatted output.
    #[arg(long, global = true)]
    json: bool,
    /// With no subcommand, `tutti` attaches (a fresh session asks where to start
    /// instead of assuming the current directory).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the tutti-server daemon.
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Open, list, and remove projects: the directories you work in, the
    /// sidebar's top rows.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Add, list, merge, and remove workspaces: jj checkouts nested under a
    /// project, so an agent works on its own copy of the tree.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Create, list, and select tabs.
    Tab {
        #[command(subcommand)]
        action: TabAction,
    },
    /// Run, split, inspect, and control panes.
    Pane {
        #[command(subcommand)]
        action: PaneAction,
    },
    /// Attach the interactive TUI (alias for bare `tutti`).
    Attach,
    /// Run a command in a fresh tab and print the new pane's id — the one
    /// call an orchestrator makes: `p=$(tutti run -- claude "fix the tests")`.
    Run {
        /// The workspace to run in (default: inside a tutti pane, that pane's
        /// own workspace; elsewhere, the current one).
        #[arg(long, conflicts_with = "dir")]
        workspace: Option<u64>,
        /// Run in the workspace open at this directory, mounting it if none is.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Run the command in a k8s sandbox from this config profile.
        #[arg(long)]
        sandbox: Option<String>,
        /// Title the pane (default: the command's name).
        #[arg(long)]
        title: Option<String>,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open a file in your workspace's editor pane — a Neovim or Emacs whose
    /// tutti plugin set the `TUTTI_EDITOR` user var — and focus that pane.
    /// Returns at once: not a blocking $EDITOR.
    Edit {
        /// `<path>[:<line>[:<col>]]`, relative to the current directory.
        target: String,
        /// The workspace whose editor to use (default: this pane's own).
        #[arg(long)]
        workspace: Option<u64>,
    },
    /// Run commands in k8s sandboxes (`[[sandboxes]]` in the config).
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
    /// List tracker issues (`[[issues]]` in the config) and deploy one to an
    /// agent. Read-only toward the trackers.
    Issue {
        #[command(subcommand)]
        action: IssueAction,
    },
    /// Stream the session's state as JSON lines: a full snapshot of every pane
    /// now and on each change, until the daemon exits. Read-only, beside any
    /// attached TUI; never starts a daemon.
    Watch,
    /// List the notification history, newest first: bells, desktop
    /// notifications, and agents that blocked or finished a turn — what
    /// happened while nobody was looking.
    #[command(args_conflicts_with_subcommands = true)]
    Notes {
        /// List only the notes nobody has read.
        #[arg(long)]
        unread: bool,
        #[command(subcommand)]
        action: Option<NotesAction>,
    },
    /// Forward an agent's hook event to tutti (reads its JSON on stdin): a
    /// Claude Code hook payload, or an opencode event from tutti's plugin.
    /// Wired up by `tutti hooks <agent>`; a silent no-op outside a tutti pane.
    AgentEvent {
        #[arg(value_enum)]
        agent: HookAgent,
    },
    /// Print or install the hooks wiring an agent to tutti: Claude Code's
    /// settings.json hooks, or opencode's plugin.
    Hooks {
        #[arg(value_enum)]
        agent: HookAgent,
        /// Print only the snippet (omit the install instructions on stderr).
        #[arg(long, conflicts_with = "install")]
        raw: bool,
        /// Claude only: hooks that report over OSC 7501 to the terminal, for
        /// baking into a k8s sandbox image — the pod has no route to the
        /// daemon, but its pty is the pane's. Print-only.
        #[arg(long, conflicts_with = "install")]
        tty: bool,
        /// Install it (shows the change, asks first).
        #[arg(long)]
        install: bool,
        /// Install into the project (./.claude/settings.json,
        /// ./.opencode/plugins/) instead of the user config.
        #[arg(long, requires = "install")]
        project: bool,
        /// Skip the confirmation prompt.
        #[arg(long, requires = "install")]
        yes: bool,
    },
    /// Print or install the tutti skill, which teaches a Claude Code agent in
    /// a tutti pane to run, wait on, answer, and review its own sub-agents.
    Skill {
        /// Print only the skill (omit the install instructions on stderr).
        #[arg(long, conflicts_with = "install")]
        raw: bool,
        /// Install it (shows the change, asks first).
        #[arg(long)]
        install: bool,
        /// Install into the project (./.claude/skills/) instead of the user
        /// config.
        #[arg(long, requires = "install")]
        project: bool,
        /// Skip the confirmation prompt.
        #[arg(long, requires = "install")]
        yes: bool,
    },
    /// Print the shell integration: inside a tutti pane it reports the
    /// working directory, so a split starts there, and marks each command,
    /// for `pane read --last-output`. Load it from your rc file with `eval
    /// "$(tutti shell-init zsh)"` (fish: `tutti shell-init fish | source`), or
    /// let the setup panel (prefix `i`) add that line.
    ShellInit {
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// The agent whose hook schema `agent-event`/`hooks` speak.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum HookAgent {
    Claude,
    Opencode,
}

#[derive(Subcommand)]
enum SandboxAction {
    /// Provision a sandbox from the profile, `kubectl exec -it` the command
    /// in it, and delete the sandbox when the command exits — what a `pane run
    /// --sandbox` pane runs.
    Run {
        profile: String,
        /// The sandbox's name, which the daemon picks per pane: it names the
        /// k8s object and, as `tutti/<name>`, the branch the sandbox pushes to.
        /// A daemon from before names existed passes none; one is derived the
        /// way that daemon expects, so a mid-upgrade sandbox still starts.
        #[arg(long)]
        name: Option<String>,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Fetch the branch a sandbox pane's agent pushed (`tutti/<sandbox
    /// name>`) and check it out as a workspace beside the pane's, for its
    /// diff and a merge.
    Pull {
        pane: u64,
        /// The workspace's name, `[A-Za-z0-9_-]+` (default: the sandbox's).
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum NotesAction {
    /// Mark notes read: all of them, or only those up to an id, or only one
    /// pane's — the two narrow together.
    Read {
        /// Only the notes up to this id.
        #[arg(long)]
        up_to: Option<u64>,
        /// Only this pane's notes.
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Empty the history.
    Clear,
}

#[derive(Subcommand)]
enum IssueAction {
    /// List the open issues of every configured source, fetched in the
    /// current directory (`gh` names its repository from there).
    List {
        /// Only this source.
        #[arg(long)]
        source: Option<String>,
    },
    /// Launch an agent on an issue, the issue as its prompt and the pane
    /// linked back to it: in a new jj workspace named after the issue under
    /// the project, or a fresh tab of the project itself with `--here` (or
    /// when the project is not a jj repo). Prints the new pane's id.
    Deploy {
        /// The configured source.
        source: String,
        /// The issue's id there (`123`, `ENG-42`).
        id: String,
        /// The agent to run; it must take a first message on its command
        /// line.
        #[arg(long, default_value = "claude")]
        agent: String,
        /// The project to deploy into (default: the one open at the current
        /// directory).
        #[arg(long)]
        project: Option<u64>,
        /// Run in a fresh tab of the project instead of a new workspace.
        #[arg(long)]
        here: bool,
        /// Run the agent in a k8s sandbox from this config profile.
        #[arg(long)]
        sandbox: Option<String>,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the daemon, auto-starting if not already running.
    Start {
        /// Run in the foreground instead of detaching.
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running daemon.
    Stop,
    /// Bring back the layout the daemon saved before it last stopped — agents
    /// resuming their own conversations, every other pane a fresh shell.
    Restore {
        /// Drop the saved layout instead of restoring it.
        #[arg(long)]
        discard: bool,
    },
}

/// Project verbs. A project is a top-level workspace entry, so each maps onto
/// a workspace request; its id works wherever a workspace id does.
#[derive(Subcommand)]
enum ProjectAction {
    /// Open an existing directory as a project.
    Add { dir: PathBuf },
    /// List the projects (`workspace list` adds the workspaces under them).
    List,
    /// Remove a project and its panes from tutti; the directory stays.
    Kill { id: u64 },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// Add a workspace under a project: `jj workspace add` a named checkout and
    /// open it with a shell pane.
    #[command(alias = "fork")]
    Add {
        /// The project (or workspace) to check out from.
        project: u64,
        /// The workspace's name, `[A-Za-z0-9_-]+`: it names the jj workspace
        /// and the default checkout directory.
        #[arg(long)]
        name: String,
        /// The revision to check out (`jj workspace add --revision`).
        #[arg(long, short = 'r')]
        revision: Option<String>,
        /// Where the checkout goes, which must not exist yet (default:
        /// `<repo>-<name>` beside the project's repo).
        #[arg(long)]
        dest: Option<PathBuf>,
    },
    /// List every project and workspace, each with the entry it is nested
    /// under.
    List,
    /// Remove a workspace and its panes from tutti; its checkout stays unless
    /// `--discard`.
    Kill {
        id: u64,
        /// Also discard the checkout: `jj workspace forget` it and delete its
        /// directory. Refused for a checkout tutti did not create.
        #[arg(long)]
        discard: bool,
    },
    /// Update a stale workspace (`jj workspace update-stale`).
    Update { id: u64 },
    /// Merge a workspace's work back into its project's trunk bookmark
    /// (`main`, else `master`). Only valid for a workspace tutti created.
    Merge {
        id: u64,
        /// Also `jj git push` the advanced bookmark when the project has a
        /// remote.
        #[arg(long)]
        push: bool,
    },
    /// Show the workspace's jj diff against trunk (`--stat` for the summary
    /// only).
    Diff {
        id: u64,
        #[arg(long)]
        stat: bool,
    },
    /// The old spelling of `project add`, kept so scripts keep working.
    #[command(hide = true)]
    New {
        #[arg(long)]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum TabAction {
    New {
        #[arg(long)]
        workspace: Option<u64>,
    },
    List {
        #[arg(long)]
        workspace: Option<u64>,
    },
    Select {
        id: u64,
    },
}

#[derive(Subcommand)]
enum PaneAction {
    /// Run a command in a new pane: `pane run --tab 3 -- claude --flag`.
    Run {
        #[arg(long)]
        tab: Option<u64>,
        /// Remove the pane entirely when its command exits (no exited row).
        #[arg(long)]
        ephemeral: bool,
        /// Run the command in a k8s sandbox from this config profile.
        #[arg(long)]
        sandbox: Option<String>,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    Split {
        pane: u64,
        direction: SplitDir,
    },
    List,
    Kill {
        pane: u64,
    },
    Rename {
        pane: u64,
        title: String,
    },
    #[command(group(ArgGroup::new("payload").required(true).args(["text", "keys"])))]
    Send {
        pane: u64,
        /// Literal text to type into the pane.
        #[arg(long)]
        text: Option<String>,
        /// Key names to send (interpreted by the server).
        #[arg(long)]
        keys: Option<String>,
        /// Answer a blocked agent: send only while the pane is blocked and its
        /// prompt still has this fingerprint (from `pane read --fingerprint`).
        #[arg(long)]
        expect: Option<String>,
    },
    Read {
        pane: u64,
        #[arg(long)]
        lines: Option<usize>,
        #[arg(long)]
        unwrapped: bool,
        /// Print the prompt fingerprint `pane send --expect` takes, instead of
        /// the screen.
        #[arg(long, conflicts_with_all = ["lines", "unwrapped", "since"])]
        fingerprint: bool,
        /// Tail incrementally: print the rows scrolled off since this cursor,
        /// then the screen, and the next cursor on stderr. Start from 0.
        #[arg(long)]
        since: Option<u64>,
        /// Print the output of the last command the pane's shell finished,
        /// and its exit code on stderr. Needs `tutti shell-init` in the shell.
        #[arg(long, conflicts_with_all = ["since", "fingerprint"])]
        last_output: bool,
    },
    /// Answer the pane's held approval — a Claude permission request waiting
    /// on tutti under `approvals = "hold"`: allow the tool call, or `--deny`
    /// it.
    Approve {
        pane: u64,
        /// Refuse the tool call instead.
        #[arg(long)]
        deny: bool,
        /// The reason Claude is told it was refused.
        #[arg(long, requires = "deny")]
        message: Option<String>,
    },
    /// Mark a pane focused (records it active and clears its Done badge) —
    /// or, with `--direction`, the pane beside one, printing its id. At a
    /// left or right edge (at any edge, from a TUI running in rtty) the
    /// attached TUI showing that pane takes the move on as Ctrl+h/j/k/l would
    /// (sidebar, changes panel, rtty's panes, next tab) and nothing prints; at
    /// any other edge it says `no pane <direction> of <id>` on stderr and
    /// exits 1.
    Focus {
        /// The pane to focus.
        #[arg(
            required_unless_present = "direction",
            conflicts_with_all = ["direction", "from", "wrap"]
        )]
        pane: Option<u64>,
        /// Focus the pane on this side instead.
        #[arg(long, value_enum)]
        direction: Option<Side>,
        /// The pane to look beside (default: this pane, else the attached
        /// TUI's focused one).
        #[arg(long, requires = "direction")]
        from: Option<u64>,
        /// At an edge, go round to the farthest pane on the other side.
        #[arg(long, requires = "direction")]
        wrap: bool,
    },
    /// Move the border of the split beside a pane toward a side, as the TUI's
    /// Alt+h/j/k/l do; with no such split, a TUI in rtty moves rtty's.
    Resize {
        /// The way the border moves.
        #[arg(long, value_enum)]
        direction: Side,
        /// How far, as a fraction of the split.
        #[arg(long, default_value_t = RESIZE_DELTA)]
        amount: f32,
        /// The pane whose split moves (default: this pane, else the attached
        /// TUI's focused one).
        #[arg(long)]
        from: Option<u64>,
    },
    /// Show one pane in this terminal, drawn by the terminal itself: its
    /// output streamed raw, your keys typed into it. The prefix then `d`
    /// detaches; the prefix twice sends one. Never starts a daemon.
    Attach {
        pane: u64,
    },
    /// Block until each pane reaches a state (`done` unless `--until` says
    /// otherwise), printing each one's; exits 124 on `--timeout`.
    Wait {
        #[arg(required = true)]
        panes: Vec<u64>,
        /// The states that end the wait, comma-separated or repeated.
        #[arg(long, value_enum, value_delimiter = ',', default_value = "done")]
        until: Vec<Until>,
        /// Only a fresh transition counts, not the state a pane is already in.
        #[arg(long)]
        next: bool,
        /// Give up after this many seconds, across all the panes.
        #[arg(long)]
        timeout: Option<f64>,
    },
    /// List the pane's per-turn checkpoints, newest first.
    Checkpoints {
        pane: u64,
    },
    /// Show one checkpoint's turn diff (the newest unless N is given).
    CheckpointDiff {
        pane: u64,
        n: Option<u64>,
        #[arg(long)]
        stat: bool,
    },
    /// Restore the pane's workspace to checkpoint N, first saving the current
    /// state as a checkpoint so the restore can itself be undone.
    Restore {
        pane: u64,
        n: u64,
        /// Restore even while the pane is mid-turn (working or blocked).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SplitDir {
    Right,
    Down,
}

impl From<SplitDir> for Direction {
    /// `right` places the new pane beside the old one (columns), `down` stacks
    /// them (rows) — matching ratatui's `Horizontal`/`Vertical` layout axes.
    fn from(dir: SplitDir) -> Direction {
        match dir {
            SplitDir::Right => Direction::Horizontal,
            SplitDir::Down => Direction::Vertical,
        }
    }
}

/// The side `pane focus|resize --direction` names: `Toward`'s CLI face.
#[derive(Clone, Copy, ValueEnum)]
enum Side {
    Left,
    Right,
    Up,
    Down,
}

impl From<Side> for Toward {
    fn from(side: Side) -> Toward {
        match side {
            Side::Left => Toward::Left,
            Side::Right => Toward::Right,
            Side::Up => Toward::Up,
            Side::Down => Toward::Down,
        }
    }
}

/// A state `pane wait --until` names: `WaitFor`'s CLI face.
#[derive(Clone, Copy, ValueEnum)]
enum Until {
    Working,
    Blocked,
    /// Finished, seen or not.
    Done,
    /// Finished and seen.
    Idle,
}

impl From<Until> for WaitFor {
    fn from(until: Until) -> WaitFor {
        match until {
            Until::Working => WaitFor::Working,
            Until::Blocked => WaitFor::Blocked,
            Until::Done => WaitFor::Done,
            Until::Idle => WaitFor::Idle,
        }
    }
}

/// A wait's exit code when it timed out: coreutils `timeout`'s.
const TIMED_OUT: i32 = 124;

/// How long `agent-event` waits on the daemon before giving up: a hook must
/// never stall its agent (Claude Code gives `UserPromptSubmit` 30 s).
const AGENT_EVENT_DEADLINE: Duration = Duration::from_secs(2);

fn main() {
    match run(Cli::parse()) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Some(Command::Server { action }) => run_server(action, &cli.session, cli.json),
        // Bare `tutti` and `tutti attach` both attach, mounting startup projects.
        None | Some(Command::Attach) => {
            attach_session(&cli.session)?;
            Ok(0)
        }
        Some(Command::AgentEvent { agent }) => run_agent_event(&cli.session, agent),
        Some(Command::Pane {
            action:
                PaneAction::Wait {
                    panes,
                    until,
                    next,
                    timeout,
                },
        }) => run_wait(&cli.session, cli.json, &panes, &until, next, timeout),
        Some(Command::Pane {
            action: PaneAction::Attach { pane },
        }) => {
            // Its own pane would echo its output back into itself forever.
            let own = std::env::var("TUTTI_SESSION").is_ok_and(|s| s == cli.session)
                && std::env::var("TUTTI_PANE").is_ok_and(|p| p == pane.to_string());
            if own {
                bail!("pane {pane} is the pane this runs in");
            }
            tutti::attach::raw::run(&cli.session, PaneId(pane), Config::load()?.prefix)
        }
        Some(Command::Sandbox {
            action: SandboxAction::Run { profile, name, cmd },
        }) => {
            let name = name.unwrap_or_else(|| {
                let pane: String = std::env::var("TUTTI_PANE")
                    .unwrap_or_default()
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect();
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as u64);
                format!("tutti-p{pane}-{nanos:x}")
            });
            tutti::sandbox::run(Config::load()?.sandbox(&profile)?, &name, &cmd)
        }
        Some(Command::Issue { action }) => run_issue(action, &cli.session, cli.json),
        Some(Command::Edit { target, workspace }) => run_edit(&cli.session, &target, workspace),
        Some(Command::Watch) => run_watch(&cli.session),
        Some(Command::Hooks {
            agent,
            raw,
            tty,
            install,
            project,
            yes,
        }) => match (agent, install) {
            (HookAgent::Opencode, _) if tty => {
                bail!("--tty is for claude: opencode reports through its plugin")
            }
            (HookAgent::Claude, true) => {
                install_integration(Integration::ClaudeHooks, project, yes)
            }
            (HookAgent::Opencode, true) => {
                install_integration(Integration::OpencodePlugin, project, yes)
            }
            (HookAgent::Claude, false) => {
                emit_hooks(raw, tty);
                Ok(0)
            }
            (HookAgent::Opencode, false) => {
                emit_opencode_plugin(raw);
                Ok(0)
            }
        },
        Some(Command::Skill {
            install: true,
            project,
            yes,
            ..
        }) => install_integration(Integration::Skill, project, yes),
        Some(Command::Skill { raw, .. }) => {
            emit_skill(raw);
            Ok(0)
        }
        // Nothing on stderr: an rc file evals this on every shell start.
        Some(Command::ShellInit { shell }) => {
            print!("{}", shell.snippet());
            Ok(0)
        }
        Some(command) => {
            let project = matches!(command, Command::Project { .. });
            let mut request = to_request(command)?;
            // A run from one of this session's own panes lands beside it; a
            // directional verb there starts from it unless told otherwise.
            if let Request::Run { from, .. }
            | Request::PaneFocusToward { from, .. }
            | Request::PaneResizeToward { from, .. } = &mut request
                && from.is_none()
            {
                *from = caller_pane(&cli.session);
            }
            let mut client = Client::connect_or_start(&cli.session)?;
            let response = client.request(&request)?;
            Ok(if project {
                emit_project(response, cli.json)
            } else {
                emit(response, cli.json)
            })
        }
    }
}

/// The pane this command runs in (`TUTTI_PANE`) when it is one of `session`'s
/// own: a pane id means nothing to another session's daemon.
fn caller_pane(session: &str) -> Option<PaneId> {
    std::env::var("TUTTI_SESSION")
        .ok()
        .filter(|s| s == session)?;
    std::env::var("TUTTI_PANE").ok()?.parse().ok().map(PaneId)
}

/// The session a command targets unless `-s` names one: inside a tutti pane
/// that pane's own (`TUTTI_SESSION`, exported into every pane), so an agent
/// driving tutti reaches the daemon running it instead of auto-starting a
/// stray one; else `tutti`.
fn session_from(pane_session: Option<String>) -> String {
    pane_session
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tutti".into())
}

/// Load config, mount any configured startup projects (idempotently), and
/// attach. An empty session with no configured projects attaches straight into
/// the first-run prompt (prefilled with the cwd) rather than assuming it —
/// `first_run` carries that prefill. A project that fails to start yields a
/// `notice` surfaced after attach; the rest still mount. Inside one of the
/// session's own panes it refuses before loading or starting anything.
fn attach_session(session: &str) -> Result<()> {
    let pane_session = std::env::var_os("TUTTI_PANE").and(std::env::var("TUTTI_SESSION").ok());
    refuse_nested_attach(session, pane_session.as_deref())?;
    let config = Config::load()?;
    // Absolutize project dirs client-side so the daemon (which has its own cwd)
    // records the real directory and its git-branch probe hits.
    let projects: Vec<PathBuf> = config.projects.iter().map(|d| absolutize(d)).collect();
    let mut client = Client::connect_or_start(session)?;
    let existing = existing_dirs(&mut client)?;
    let was_empty = existing.is_empty();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let notices = mount_projects(
        &projects,
        &existing,
        &shell,
        |p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()),
        |req| client.request(req),
    )?;
    drop(client);

    let first_run = (was_empty && config.projects.is_empty())
        .then(|| std::env::current_dir().map(|d| d.display().to_string()))
        .transpose()
        .context("resolve current directory")?;
    let notice = (!notices.is_empty()).then(|| notices.join("; "));

    tutti::attach::run(session, config, first_run, notice)?;
    Ok(())
}

/// Refuse to attach `session` from inside one of its own panes —
/// `pane_session` is the enclosing pane's `TUTTI_SESSION`, `None` outside
/// tutti — since the TUI would nest the session inside itself. A pane of
/// another session may attach this one.
fn refuse_nested_attach(session: &str, pane_session: Option<&str>) -> Result<()> {
    if pane_session == Some(session) {
        bail!(
            "this pane is inside tutti session '{session}' — attaching here would nest the session inside itself (use -s <other-session>)"
        );
    }
    Ok(())
}

/// Resolve `dir` to an absolute path client-side. Canonicalize when it exists
/// (collapsing `.`/`..`/symlinks); otherwise join a relative path onto the
/// client's cwd. The daemon has its own cwd, so a relative `--dir .` would
/// otherwise anchor to the wrong place and miss the workspace's git branch.
fn absolutize(dir: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(dir) {
        return canon;
    }
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| dir.to_path_buf(), |cwd| cwd.join(dir))
    }
}

/// Resolve `workspace add --dest` client-side, as the TUI's `where:` step
/// does: absolute, its parent canonicalized when it exists, the leaf kept — it
/// must not exist yet.
fn absolutize_dest(dest: &Path) -> PathBuf {
    let dest = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());
    match (dest.parent(), dest.file_name()) {
        (Some(parent), Some(leaf)) => absolutize(parent).join(leaf),
        _ => dest,
    }
}

/// The dirs of the session's live workspaces.
fn existing_dirs(client: &mut Client) -> Result<Vec<PathBuf>> {
    match client.request(&Request::WorkspaceList)? {
        Response::Workspaces { workspaces } => Ok(workspaces.into_iter().map(|w| w.dir).collect()),
        other => bail!("unexpected reply to workspace list: {other:?}"),
    }
}

/// The configured project dirs that need creating: those whose canonical path is
/// not already a live workspace's. `canon` is injected so this stays pure — it
/// collapses symlinks and trailing slashes so restarts are idempotent.
fn projects_to_create(
    projects: &[PathBuf],
    existing: &[PathBuf],
    canon: impl Fn(&Path) -> PathBuf,
) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = existing.iter().map(|d| canon(d)).collect();
    projects
        .iter()
        .filter(|dir| seen.insert(canon(dir)))
        .cloned()
        .collect()
}

/// Create a workspace + shell pane for each configured project not already
/// mounted. A project whose dir cannot be started (e.g. it does not exist)
/// yields a notice naming the path — surfaced as a transient message after
/// attach — and the remaining projects still mount. Startup mounting always
/// spawns a plain shell (never the agent launcher), so attaching a config with
/// many `[[projects]]` does not open a launcher per project.
fn mount_projects(
    projects: &[PathBuf],
    existing: &[PathBuf],
    shell: &str,
    canon: impl Fn(&Path) -> PathBuf,
    mut send: impl FnMut(&Request) -> Result<Response>,
) -> Result<Vec<String>> {
    let mut notices = Vec::new();
    for dir in projects_to_create(projects, existing, &canon) {
        match send(&Request::WorkspaceNew { dir: dir.clone() })? {
            Response::WorkspaceCreated { .. } => {}
            Response::Error { message } => {
                notices.push(format!("{}: {message}", dir.display()));
                continue;
            }
            other => bail!("unexpected reply to workspace new: {other:?}"),
        }
        match send(&Request::PaneRun {
            tab: None,
            cmd: vec![shell.to_string()],
            ephemeral: false,
            sandbox: None,
        })? {
            Response::PaneCreated { .. } | Response::Ok => {}
            Response::Error { message } => notices.push(format!("{}: {message}", dir.display())),
            other => bail!("unexpected reply to pane run: {other:?}"),
        }
    }
    Ok(notices)
}

fn run_server(action: ServerAction, session: &str, json: bool) -> Result<i32> {
    match action {
        ServerAction::Start { foreground: true } => {
            client::exec_foreground(session)?;
            Ok(1)
        }
        ServerAction::Start { foreground: false } => {
            let log = tutti_core::log_path(session);
            match Client::connect(session) {
                Ok(_) => println!(
                    "tutti-server already running for session '{session}' (log: {})",
                    log.display()
                ),
                Err(e) if client::not_running(&e) => {
                    Client::connect_or_start(session)?;
                    println!(
                        "tutti-server started for session '{session}' (log: {})",
                        log.display()
                    );
                }
                Err(e) => return Err(e.into()),
            }
            Ok(0)
        }
        ServerAction::Stop => match client::stop(session)? {
            StopOutcome::Signalled(pid) => {
                println!("stopped tutti-server (pid {pid}) for session '{session}'");
                Ok(0)
            }
            StopOutcome::NotRunning => {
                eprintln!("tutti-server not running for session '{session}'");
                Ok(0)
            }
        },
        // A daemon this auto-starts offers its predecessor's layout at once.
        ServerAction::Restore { discard } => {
            let mut client = Client::connect_or_start(session)?;
            let response = client.request(&Request::SessionRestore { discard })?;
            Ok(emit(response, json))
        }
    }
}

fn to_request(command: Command) -> Result<Request> {
    match command {
        Command::Project { action } => Ok(match action {
            ProjectAction::Add { dir } => Request::WorkspaceNew {
                dir: absolutize(&dir),
            },
            ProjectAction::List => Request::WorkspaceList,
            ProjectAction::Kill { id } => Request::WorkspaceKill {
                id: WorkspaceId(id),
                discard: false,
            },
        }),
        Command::Workspace { action } => Ok(match action {
            WorkspaceAction::Add {
                project,
                name,
                revision,
                dest,
            } => Request::WorkspaceFork {
                id: WorkspaceId(project),
                name,
                revision,
                dest: dest.as_deref().map(absolutize_dest),
            },
            WorkspaceAction::New { dir } => Request::WorkspaceNew {
                dir: absolutize(&dir),
            },
            WorkspaceAction::List => Request::WorkspaceList,
            WorkspaceAction::Kill { id, discard } => Request::WorkspaceKill {
                id: WorkspaceId(id),
                discard,
            },
            WorkspaceAction::Update { id } => Request::WorkspaceUpdate {
                id: WorkspaceId(id),
            },
            WorkspaceAction::Merge { id, push } => Request::WorkspaceMerge {
                id: WorkspaceId(id),
                push,
            },
            WorkspaceAction::Diff { id, stat } => Request::WorkspaceDiff {
                id: WorkspaceId(id),
                stat,
            },
        }),
        Command::Notes { unread, action } => Ok(match action {
            None => Request::NotesList { unread },
            Some(NotesAction::Read { up_to, pane }) => Request::NotesRead {
                up_to,
                pane: pane.map(PaneId),
            },
            Some(NotesAction::Clear) => Request::NotesClear,
        }),
        Command::Tab { action } => Ok(match action {
            TabAction::New { workspace } => Request::TabNew {
                workspace: workspace.map(WorkspaceId),
            },
            TabAction::List { workspace } => Request::TabList {
                workspace: workspace.map(WorkspaceId),
            },
            TabAction::Select { id } => Request::TabSelect { id: TabId(id) },
        }),
        Command::Run {
            workspace,
            dir,
            sandbox,
            title,
            cmd,
        } => {
            if let Some(name) = &sandbox {
                Config::load()?.sandbox(name)?;
            }
            Ok(Request::Run {
                workspace: workspace.map(WorkspaceId),
                dir: dir.as_deref().map(absolutize),
                cmd,
                sandbox,
                title,
                from: None,
                issue: None,
            })
        }
        Command::Pane { action } => Ok(match action {
            PaneAction::Run {
                tab,
                ephemeral,
                sandbox,
                cmd,
            } => {
                // Fail here, not in a pane that dies on arrival.
                if let Some(name) = &sandbox {
                    Config::load()?.sandbox(name)?;
                }
                Request::PaneRun {
                    tab: tab.map(TabId),
                    cmd,
                    ephemeral,
                    sandbox,
                }
            }
            PaneAction::Split { pane, direction } => Request::PaneSplit {
                pane: PaneId(pane),
                direction: direction.into(),
            },
            PaneAction::List => Request::PaneList,
            PaneAction::Kill { pane } => Request::PaneKill { pane: PaneId(pane) },
            PaneAction::Rename { pane, title } => Request::PaneRename {
                pane: PaneId(pane),
                title,
            },
            PaneAction::Send {
                pane,
                text,
                keys,
                expect,
            } => Request::PaneSend {
                pane: PaneId(pane),
                text,
                keys,
                expect,
            },
            PaneAction::Read {
                pane,
                fingerprint: true,
                ..
            } => Request::PaneFingerprint { pane: PaneId(pane) },
            PaneAction::Read {
                pane,
                lines,
                unwrapped,
                fingerprint: false,
                since,
                last_output,
            } => Request::PaneRead {
                pane: PaneId(pane),
                lines,
                unwrapped,
                since,
                last_output,
            },
            PaneAction::Approve {
                pane,
                deny,
                message,
            } => Request::ApprovalAnswer {
                pane: PaneId(pane),
                id: None,
                decision: if deny {
                    ApprovalDecision::Deny { message }
                } else {
                    ApprovalDecision::Allow
                },
            },
            PaneAction::Focus {
                pane: Some(pane), ..
            } => Request::PaneFocus { pane: PaneId(pane) },
            PaneAction::Focus {
                direction,
                from,
                wrap,
                ..
            } => Request::PaneFocusToward {
                from: from.map(PaneId),
                toward: direction.context("name a pane or a --direction")?.into(),
                wrap,
            },
            PaneAction::Resize {
                direction,
                amount,
                from,
            } => Request::PaneResizeToward {
                from: from.map(PaneId),
                toward: direction.into(),
                delta: amount,
            },
            PaneAction::Wait { .. } => bail!("pane wait sends a request per pane"),
            PaneAction::Attach { .. } => bail!("pane attach streams a pane"),
            PaneAction::Checkpoints { pane } => Request::PaneCheckpoints { pane: PaneId(pane) },
            PaneAction::CheckpointDiff { pane, n, stat } => Request::PaneCheckpointDiff {
                pane: PaneId(pane),
                n,
                stat,
            },
            PaneAction::Restore { pane, n, force } => Request::PaneRestore {
                pane: PaneId(pane),
                n,
                force,
            },
        }),
        Command::Sandbox {
            action: SandboxAction::Pull { pane, name },
        } => Ok(Request::SandboxPull {
            pane: PaneId(pane),
            name,
        }),
        Command::Server { .. }
        | Command::Attach
        | Command::Sandbox {
            action: SandboxAction::Run { .. },
        }
        | Command::Issue { .. }
        | Command::Edit { .. }
        | Command::Watch
        | Command::AgentEvent { .. }
        | Command::Hooks { .. }
        | Command::Skill { .. }
        | Command::ShellInit { .. } => bail!("not a protocol request"),
    }
}

/// `pane wait`: wait on each pane in turn, one request apiece on one
/// connection, all under one overall deadline, printing each answer. The first
/// that is not a match decides the exit code: 124 timed out, 1 an error.
fn run_wait(
    session: &str,
    json: bool,
    panes: &[u64],
    until: &[Until],
    next: bool,
    timeout: Option<f64>,
) -> Result<i32> {
    let timeout = timeout
        .map(Duration::try_from_secs_f64)
        .transpose()
        .context("invalid --timeout")?;
    // A deadline past what the clock can represent is no deadline.
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let until: Vec<WaitFor> = until.iter().copied().map(WaitFor::from).collect();
    let mut client = Client::connect_or_start(session)?;
    for &pane in panes {
        let timeout_ms = deadline.map(|d| {
            let left = d.saturating_duration_since(Instant::now());
            u64::try_from(left.as_millis()).unwrap_or(u64::MAX)
        });
        let response = client.request(&Request::PaneWait {
            pane: PaneId(pane),
            until: until.clone(),
            next,
            timeout_ms,
        })?;
        let code = emit(response, json);
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

/// `issue list` and `issue deploy`. A listing talks to the trackers alone,
/// from the current directory; a deploy fetches the issue in its project's
/// directory, then asks the daemon for a workspace named after it (unless
/// `--here`, or a project outside jj) and the run there.
fn run_issue(action: IssueAction, session: &str, json: bool) -> Result<i32> {
    let config = Config::load()?;
    match action {
        IssueAction::List { source } => {
            let sources = match &source {
                Some(name) => vec![config.issue_source(name)?],
                None if config.issues.is_empty() => {
                    bail!("no issue sources configured: add [[issues]] to the config")
                }
                None => config.issues.iter().collect(),
            };
            let dir = std::env::current_dir().context("resolve current directory")?;
            let mut listed = Vec::new();
            let mut failed = false;
            for source in sources {
                match issues::list(source, &dir) {
                    Ok(found) => listed.extend(found),
                    Err(e) => {
                        eprintln!("error: {}: {e:#}", source.name);
                        failed = true;
                    }
                }
            }
            if json {
                println!("{}", serde_json::to_string(&listed)?);
            } else {
                print!("{}", render::issues(&listed));
            }
            Ok(i32::from(failed))
        }
        IssueAction::Deploy {
            source,
            id,
            agent,
            project,
            here,
            sandbox,
        } => {
            let source = config.issue_source(&source)?;
            if let Some(name) = &sandbox {
                config.sandbox(name)?;
            }
            // The agent's argv on `prompt`, or why it can't take an issue.
            let registry = Registry::default();
            let on = |prompt: &str| {
                registry
                    .spec(&agent.as_str().into())
                    .and_then(|spec| spec.prompted_argv(prompt))
                    .with_context(|| {
                        let able: Vec<String> = registry
                            .specs()
                            .iter()
                            .filter(|s| s.prompt_flags.is_some())
                            .map(|s| s.kind.to_string())
                            .collect();
                        format!(
                            "{agent:?} can't take an issue: a deploy needs an agent that takes its first message on the command line ({})",
                            able.join(", ")
                        )
                    })
            };
            on("")?; // before fetching anything
            let mut client = Client::connect_or_start(session)?;
            let project = deploy_project(&mut client, project)?;
            let issue = issues::fetch(source, &id, &project.dir)?;
            let cmd = on(&issues::prompt(&source.prompt, &issue))?;
            let workspace = if here || !project.dir.ancestors().any(|d| d.join(".jj").exists()) {
                project.id
            } else {
                match client.request(&Request::WorkspaceFork {
                    id: project.id,
                    name: issues::workspace_name(&issue),
                    revision: None,
                    dest: None,
                })? {
                    Response::WorkspaceCreated { id } => id,
                    Response::Error { message } => bail!("{message}"),
                    other => bail!("unexpected reply to workspace add: {other:?}"),
                }
            };
            let response = client.request(&Request::Run {
                workspace: Some(workspace),
                dir: None,
                cmd,
                sandbox,
                title: None,
                issue: Some(issue.link()),
                from: None,
            })?;
            Ok(emit(response, json))
        }
    }
}

/// The project an issue deploys into: project `id`, else the one open at the
/// current directory — the top-level project either way, as a workspace
/// stands for the project it is nested under.
fn deploy_project(client: &mut Client, id: Option<u64>) -> Result<WorkspaceInfo> {
    let workspaces = match client.request(&Request::WorkspaceList)? {
        Response::Workspaces { workspaces } => workspaces,
        other => bail!("unexpected reply to workspace list: {other:?}"),
    };
    let cwd = absolutize(Path::new("."));
    let entry = match id {
        Some(id) => workspaces
            .iter()
            .find(|w| w.id == WorkspaceId(id))
            .with_context(|| format!("no project {id}"))?,
        None => workspaces
            .iter()
            .filter(|w| cwd.starts_with(&w.dir))
            .max_by_key(|w| w.dir.components().count())
            .with_context(|| {
                format!(
                    "no project is open at {}; pass --project <id>",
                    cwd.display()
                )
            })?,
    };
    Ok(entry
        .parent
        .and_then(|p| workspaces.iter().find(|w| w.id == p))
        .unwrap_or(entry)
        .clone())
}

/// `edit`: hand a file to the editor pane of a workspace — `--workspace`, else
/// this pane's own — through the editor's own remote CLI, then focus that pane
/// as `pane focus` does. The daemon only names the pane.
fn run_edit(session: &str, target: &str, workspace: Option<u64>) -> Result<i32> {
    let (path, line, col) = edit_target(target);
    let path = std::path::absolute(path).with_context(|| format!("resolve {path:?}"))?;
    let mut client = Client::connect(session).context("connecting to tutti-server")?;
    let workspaces = match client.request(&Request::View)? {
        Response::View { workspaces } => workspaces,
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply to view: {other:?}"),
    };
    let caller = caller_pane(session);
    let (name, found) = editor::pane(&workspaces, caller, workspace.map(WorkspaceId))?;
    let Some((pane, editor)) = found else {
        eprintln!("no editor pane in workspace {name}");
        return Ok(1);
    };
    editor::open(editor, &path, line, col)?;
    match client.request(&Request::PaneFocus { pane })? {
        Response::Ok => Ok(0),
        other => Ok(emit(other, false)),
    }
}

/// Split `edit`'s `<path>[:<line>[:<col>]]`; a suffix that is not a number
/// stays part of the path.
fn edit_target(target: &str) -> (&str, Option<u32>, Option<u32>) {
    /// `s` up to its last `:`, and the number after it — when it is one.
    fn suffix(s: &str) -> Option<(&str, u32)> {
        let (rest, n) = s.rsplit_once(':')?;
        let digits = n.bytes().all(|b| b.is_ascii_digit());
        Some((rest, n.parse().ok().filter(|_| digits)?))
    }
    match suffix(target) {
        None => (target, None, None),
        Some((rest, last)) => match suffix(rest) {
            Some((path, line)) => (path, Some(line), Some(last)),
            None => (rest, Some(last), None),
        },
    }
}

/// Forward one agent hook event (read as JSON on stdin) to the daemon. Built to
/// never break an agent session: outside a tutti-spawned pane (`TUTTI_PANE`
/// unset) it exits 0 silently; a malformed or irrelevant event sends nothing; a
/// connect/send failure, or a daemon that has not answered within
/// `AGENT_EVENT_DEADLINE`, is a stderr note, never a failure exit. This is the
/// one deliberately never-fail path in the CLI. It writes stdout only to answer
/// a held permission request (`approvals = "hold"`): on `UserPromptSubmit` and
/// `SessionStart` Claude Code hands a hook's stdout to the model as context.
fn run_agent_event(session: &str, agent: HookAgent) -> Result<i32> {
    let Some(pane) = std::env::var("TUTTI_PANE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return Ok(0);
    };
    // The pane's own daemon owns the socket; the env var wins over the -s value.
    let session = std::env::var("TUTTI_SESSION").unwrap_or_else(|_| session.to_string());
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(0);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&input) else {
        return Ok(0);
    };
    let mapped = match agent {
        HookAgent::Claude => hooks::map_claude_event(&value),
        HookAgent::Opencode => hooks::map_opencode_event(&value),
    };
    let Some((event, conversation)) = mapped else {
        return Ok(0);
    };
    if let Some((tool, summary)) = hooks::permission_request(&value)
        && let Some(timeout) = approval_hold_timeout()
    {
        let held = hold_approval(&session, PaneId(pane), conversation, tool, summary, timeout);
        match held.map(|decision| hooks::permission_decision(&decision)) {
            Ok(Some(output)) => println!("{output}"),
            Ok(None) => {}
            Err(err) => eprintln!("tutti agent-event: {err:#}"),
        }
        return Ok(0);
    }
    // A wedged daemon must not stall the hook: the whole round trip — connect,
    // write, read — runs against one deadline, and past it the process exits
    // regardless, the stuck thread with it.
    let (done, outcome) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(send_agent_event(
            &session,
            PaneId(pane),
            event,
            conversation,
        ));
    });
    match outcome.recv_timeout(AGENT_EVENT_DEADLINE) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("tutti agent-event: {err:#}"),
        Err(_) => eprintln!(
            "tutti agent-event: tutti-server did not answer within {}s",
            AGENT_EVENT_DEADLINE.as_secs()
        ),
    }
    Ok(0)
}

/// How long a permission request is held under `approvals = "hold"`, `None`
/// when it is only watched. The config is read only once a request comes;
/// one that will not load is a stderr note and reads as watch.
fn approval_hold_timeout() -> Option<Duration> {
    match Config::load() {
        Ok(config) => (config.approvals == Approvals::Hold).then_some(config.approval_timeout),
        Err(err) => {
            eprintln!("tutti agent-event: {err:#}");
            None
        }
    }
}

/// Hold a permission request on the session's daemon until it is answered —
/// in place of its `AgentEvent`, which the hold blocks the pane for just the
/// same. `timeout` running out is an error, as is any failure: Claude's own
/// dialog then appears. Never auto-starts a server.
fn hold_approval(
    session: &str,
    pane: PaneId,
    conversation: Option<String>,
    tool: String,
    summary: String,
    timeout: Duration,
) -> Result<ApprovalDecision> {
    let mut client = Client::connect(session).context("connecting to tutti-server")?;
    client.set_timeout(timeout)?;
    let hold = Request::ApprovalHold {
        pane,
        conversation,
        tool,
        summary,
    };
    let reply = client
        .request(&hold)
        .with_context(|| format!("no answer within {}s", timeout.as_secs()))?;
    match reply {
        Response::Approval { decision } => Ok(decision),
        other => bail!("unexpected reply to an approval hold: {other:?}"),
    }
}

/// Send an `AgentEvent` to the session's daemon. Never auto-starts a server — a
/// hook firing with no daemon listening has nothing to report to.
fn send_agent_event(
    session: &str,
    pane: PaneId,
    event: AgentHookEvent,
    conversation: Option<String>,
) -> Result<()> {
    let mut client = Client::connect(session).context("connecting to tutti-server")?;
    client.request(&Request::AgentEvent {
        pane,
        event,
        conversation,
    })?;
    Ok(())
}

/// Print the session's snapshots to stdout, one JSON object per line, flushed
/// as each lands, until the daemon closes the connection (exit 0). Never
/// auto-starts a daemon: with none running there is nothing to watch (exit 1).
fn run_watch(session: &str) -> Result<i32> {
    use std::io::Write;

    let mut client = match Client::connect(session) {
        Ok(client) => client,
        Err(e) if client::not_running(&e) => {
            eprintln!("tutti watch: no tutti-server is running for session '{session}'");
            return Ok(1);
        }
        Err(e) => return Err(e).context("connecting to tutti-server"),
    };
    let mut out = std::io::stdout().lock();
    client.watch(|json| {
        // The one reply a `Watch` can get is a daemon that predates it.
        if let Ok(Response::Error { message }) = serde_json::from_slice(json) {
            bail!("the daemon cannot watch ({message}); restart it: `tutti server stop`");
        }
        if let Ok(Event::Snapshot { .. }) = serde_json::from_slice(json) {
            out.write_all(json)?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
        Ok(())
    })?;
    Ok(0)
}

/// Print the Claude Code hooks snippet — with `tty`, the OSC 7501 one for a
/// sandbox image: the JSON to stdout, plus — unless `--raw` — a few lines on
/// where it goes to stderr so the JSON pipes cleanly on its own.
fn emit_hooks(raw: bool, tty: bool) {
    let notes: &[&str] = if tty {
        let snippet = serde_json::to_string_pretty(&hooks::claude_tty_hooks_json());
        println!("{}", snippet.expect("hooks snippet serializes"));
        &[
            "# For a sandbox image: each hook writes OSC 7501 to /dev/tty, which",
            "# `kubectl exec -it` carries back to the tutti pane — the image needs",
            "# printf, not tutti. Merge the object above into the agent user's",
            "# ~/.claude/settings.json at the TOP level as the image is built (or",
            "# commit it as .claude/settings.json in the repo the sandbox clones).",
        ]
    } else {
        println!("{}", hooks::claude_hooks_snippet());
        &[
            "# The object above already includes the \"hooks\" key: merge it into",
            "# ~/.claude/settings.json at the TOP level (or .claude/settings.json in",
            "# a project) — or let tutti do it: `tutti hooks claude --install`.",
            "# Outside a tutti pane the hook exits silently; if you ever uninstall",
            "# tutti, remove these entries or Claude will warn on every tool call.",
        ]
    };
    if !raw {
        for line in notes {
            eprintln!("{line}");
        }
    }
}

/// Print the opencode plugin: the JavaScript to stdout, plus — unless `--raw` —
/// where it goes on stderr, so the plugin pipes cleanly on its own.
fn emit_opencode_plugin(raw: bool) {
    print!("{}", hooks::OPENCODE_PLUGIN);
    if !raw {
        eprintln!("# Save the plugin above as ~/.config/opencode/plugins/tutti.js (or");
        eprintln!("# .opencode/plugins/tutti.js in a project) — or let tutti do it:");
        eprintln!("# `tutti hooks opencode --install`. Outside a tutti pane it does nothing.");
    }
}

/// Print the tutti skill: the markdown to stdout, plus — unless `--raw` —
/// where it goes on stderr, so the skill pipes cleanly on its own.
fn emit_skill(raw: bool) {
    print!("{}", integrations::SKILL);
    if !raw {
        eprintln!("# Save the skill above as ~/.claude/skills/tutti/SKILL.md (or");
        eprintln!("# .claude/skills/tutti/SKILL.md in a project) — or let tutti do it:");
        eprintln!("# `tutti skill --install`.");
    }
}

/// `hooks <agent> --install` and `skill --install`: into the user config, or
/// with `--project` the cwd's — show what changes, ask (unless `--yes`), then
/// back the old file up and write the new one atomically. A file that is not
/// tutti's, or one tutti cannot read, is never touched.
fn install_integration(integration: Integration, project: bool, yes: bool) -> Result<i32> {
    use std::io::Write;

    let dirs = Dirs {
        project: project.then(PathBuf::new),
        ..Dirs::from_env()
    };
    let plan = integration.plan(&dirs)?;
    let path = plan.path.display();
    // Hooks merge into a file of Claude's; the plugin and skill are files of tutti's.
    let (noun, the, at) = match integration {
        Integration::ClaudeHooks => ("hooks", "", "in"),
        Integration::OpencodePlugin => ("plugin", "the ", "at"),
        Integration::Skill => ("skill", "the ", "at"),
        Integration::ShellInit => ("shell integration", "", "in"),
    };
    let change = match &plan.status {
        Status::Missing(change) | Status::Outdated(change) => change,
        Status::Installed => {
            println!("tutti {noun} already installed {at} {path}");
            return Ok(0);
        }
        Status::Foreign => {
            bail!("{path} exists and is not tutti's {noun}; refusing to overwrite it")
        }
        Status::Unreadable(why) => bail!("{why}"),
    };
    println!("{path}: {}\n", change.summary);
    println!("{}", change.preview);
    if !yes {
        eprint!("Write {path}? [y/N] ");
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            eprintln!("aborted; nothing written");
            return Ok(1);
        }
    }
    if let Some(backup) = integrations::apply(&plan)? {
        eprintln!("backed up existing file to {}", backup.display());
    }
    println!("installed {the}tutti {noun} into {path}");
    Ok(0)
}

/// Print a project verb's answer as `emit` would, in project words: a new
/// project is added, and a listing keeps only the top-level entries — with
/// `--json` too.
fn emit_project(response: Response, json: bool) -> i32 {
    match response {
        Response::WorkspaceCreated { id } if !json => {
            println!("project {id} added");
            0
        }
        Response::Workspaces { mut workspaces } => {
            workspaces.retain(|w| w.parent.is_none());
            if json {
                return emit(Response::Workspaces { workspaces }, true);
            }
            print!("{}", render::projects(&workspaces));
            0
        }
        response => emit(response, json),
    }
}

fn emit(response: Response, json: bool) -> i32 {
    let is_error = matches!(response, Response::Error { .. });
    // A directional focus at an edge no attached TUI took on exits 1 with
    // `--json` too.
    let edge = matches!(
        response,
        Response::Neighbor {
            pane: None,
            relayed: false,
            ..
        }
    );
    if json {
        let text = serde_json::to_string(&response).expect("Response serializes");
        if is_error {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
        let timed_out = matches!(
            response,
            Response::Waited {
                timed_out: true,
                ..
            }
        );
        return if timed_out {
            TIMED_OUT
        } else {
            i32::from(is_error || edge)
        };
    }
    match response {
        Response::Error { message } => {
            eprintln!("error: {message}");
            1
        }
        Response::Ok => {
            println!("ok");
            0
        }
        Response::WorkspaceCreated { id } => {
            println!("workspace {id} created");
            0
        }
        Response::Merged { pushed, bookmark } => {
            println!(
                "merged into {bookmark}{}",
                if pushed { " and pushed" } else { "" }
            );
            0
        }
        Response::TabCreated { id } => {
            println!("tab {id} created");
            0
        }
        Response::PaneCreated { id } => {
            println!("pane {id} created");
            0
        }
        // Bare, for `p=$(tutti pane focus --direction left)`.
        Response::Neighbor {
            pane: Some(pane), ..
        } => {
            println!("{pane}");
            0
        }
        // The attached TUI took the move on: into a panel, or another tab.
        Response::Neighbor {
            pane: None,
            relayed: true,
            ..
        } => 0,
        Response::Neighbor {
            from,
            toward,
            pane: None,
            relayed: false,
        } => {
            eprintln!("no pane {toward} of {from}");
            1
        }
        Response::Waited {
            pane,
            timed_out: false,
        } => {
            println!("{}", render::state_label(pane.state));
            0
        }
        Response::Waited {
            pane,
            timed_out: true,
        } => {
            eprintln!(
                "timed out waiting for pane {} (state: {})",
                pane.id,
                render::state_label(pane.state)
            );
            TIMED_OUT
        }
        // Bare, so `p=$(tutti run -- claude)` captures it.
        Response::Ran { pane, .. } => {
            println!("{pane}");
            0
        }
        Response::Workspaces { workspaces } => {
            print!("{}", render::workspaces(&workspaces));
            0
        }
        Response::Tabs { tabs } => {
            print!("{}", render::tabs(&tabs));
            0
        }
        Response::Panes { panes } => {
            print!("{}", render::panes(&panes));
            0
        }
        Response::Content { lines } => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        // The cursor goes to stderr so stdout stays the pane's text alone.
        Response::Tail {
            lines,
            cursor,
            truncated,
        } => {
            for line in lines {
                println!("{line}");
            }
            if truncated {
                eprintln!("note: rows past the cursor had already left the pane's scrollback");
            }
            eprintln!("next cursor: {cursor}");
            0
        }
        // The exit code goes to stderr so stdout stays the command's output.
        Response::LastOutput {
            lines,
            exit,
            truncated,
        } => {
            for line in lines {
                println!("{line}");
            }
            if truncated {
                eprintln!("note: its first rows had already left the pane's scrollback");
            }
            if let Some(exit) = exit {
                eprintln!("exit: {exit}");
            }
            0
        }
        Response::Fingerprint { fingerprint } => {
            println!("{fingerprint}");
            0
        }
        Response::Checkpoints { checkpoints } => {
            print!("{}", render::checkpoints(&checkpoints, render::unix_now()));
            0
        }
        Response::Notes { notes } => {
            print!("{}", render::notes(&notes, render::unix_now()));
            0
        }
        Response::Restored { n, saved } => {
            match saved {
                Some(saved) => println!("restored to #{n} (the state it replaced is #{saved})"),
                None => println!("restored to #{n}"),
            }
            0
        }
        Response::SessionRestored {
            workspaces,
            revived,
            skipped,
            ..
        } => {
            for entry in &skipped {
                eprintln!("skipped {entry}");
            }
            println!("{}", render::restored(workspaces, revived, skipped.len()));
            0
        }
        // The attach handshake reply is consumed by the TUI, never the CLI.
        Response::Attached { session, .. } => {
            println!("attached to {session}");
            0
        }
        // An approval hold's reply is consumed by `agent-event`, a view by
        // `edit`, never here.
        Response::Approval { .. } | Response::View { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_of(args: &[&str]) -> Request {
        to_request(Cli::try_parse_from(args).unwrap().command.unwrap()).unwrap()
    }

    #[test]
    fn global_flags_default_and_override() {
        let cli = Cli::try_parse_from(["tutti", "pane", "list"]).unwrap();
        assert_eq!(
            cli.session,
            session_from(std::env::var("TUTTI_SESSION").ok())
        );
        assert!(!cli.json);

        let cli = Cli::try_parse_from(["tutti", "-s", "work", "--json", "pane", "list"]).unwrap();
        assert_eq!(cli.session, "work");
        assert!(cli.json);
    }

    #[test]
    fn the_session_defaults_to_the_panes_own() {
        assert_eq!(session_from(None), "tutti");
        assert_eq!(session_from(Some("work".into())), "work");
        assert_eq!(session_from(Some(String::new())), "tutti");
    }

    #[test]
    fn attaching_is_refused_only_from_the_sessions_own_pane() {
        assert_eq!(
            refuse_nested_attach("work", Some("work"))
                .unwrap_err()
                .to_string(),
            "this pane is inside tutti session 'work' — attaching here would nest the session inside itself (use -s <other-session>)"
        );
        assert!(
            refuse_nested_attach("tutti", Some("work")).is_ok(),
            "another session's pane may attach it"
        );
        assert!(refuse_nested_attach("tutti", None).is_ok(), "outside tutti");
    }

    #[test]
    fn bare_command_defaults_to_attach() {
        // No subcommand parses (routed to the bootstrap-attach path in `run`).
        let cli = Cli::try_parse_from(["tutti"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.session, "tutti");

        // A subcommand still parses alongside the now-optional default.
        let cli = Cli::try_parse_from(["tutti", "pane", "list"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Pane { .. })));
    }

    #[test]
    fn project_verbs_parse() {
        assert_eq!(
            request_of(&["tutti", "project", "add", "/srv/api"]),
            Request::WorkspaceNew {
                dir: PathBuf::from("/srv/api")
            }
        );
        assert_eq!(
            request_of(&["tutti", "project", "list"]),
            Request::WorkspaceList
        );
        assert_eq!(
            request_of(&["tutti", "project", "kill", "4"]),
            Request::WorkspaceKill {
                id: WorkspaceId(4),
                discard: false,
            }
        );
        assert!(
            Cli::try_parse_from(["tutti", "project", "kill", "4", "--discard"]).is_err(),
            "a project's directory is the user's: there is nothing to discard"
        );
        assert!(Cli::try_parse_from(["tutti", "project", "add"]).is_err());
    }

    #[test]
    fn notes_verbs_parse() {
        assert_eq!(
            request_of(&["tutti", "notes"]),
            Request::NotesList { unread: false }
        );
        assert_eq!(
            request_of(&["tutti", "notes", "--unread"]),
            Request::NotesList { unread: true }
        );
        assert_eq!(
            request_of(&["tutti", "notes", "read", "--up-to", "12", "--pane", "3"]),
            Request::NotesRead {
                up_to: Some(12),
                pane: Some(PaneId(3)),
            }
        );
        assert_eq!(
            request_of(&["tutti", "notes", "read"]),
            Request::NotesRead {
                up_to: None,
                pane: None,
            }
        );
        assert_eq!(
            request_of(&["tutti", "notes", "clear"]),
            Request::NotesClear
        );
        assert!(
            Cli::try_parse_from(["tutti", "notes", "--unread", "clear"]).is_err(),
            "--unread narrows the listing, not a verb"
        );
    }

    #[test]
    fn workspace_verbs_parse() {
        assert_eq!(
            request_of(&["tutti", "workspace", "add", "2", "--name", "feature"]),
            Request::WorkspaceFork {
                id: WorkspaceId(2),
                name: "feature".into(),
                revision: None,
                dest: None,
            }
        );
        assert_eq!(
            request_of(&[
                "tutti",
                "workspace",
                "add",
                "2",
                "--name",
                "feature",
                "-r",
                "@-"
            ]),
            Request::WorkspaceFork {
                id: WorkspaceId(2),
                name: "feature".into(),
                revision: Some("@-".into()),
                dest: None,
            }
        );
        assert!(
            Cli::try_parse_from(["tutti", "workspace", "add", "2"]).is_err(),
            "a workspace needs a name"
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "list"]),
            Request::WorkspaceList
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "kill", "4"]),
            Request::WorkspaceKill {
                id: WorkspaceId(4),
                discard: false,
            }
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "kill", "4", "--discard"]),
            Request::WorkspaceKill {
                id: WorkspaceId(4),
                discard: true,
            }
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "update", "5"]),
            Request::WorkspaceUpdate { id: WorkspaceId(5) }
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "merge", "6"]),
            Request::WorkspaceMerge {
                id: WorkspaceId(6),
                push: false,
            }
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "merge", "6", "--push"]),
            Request::WorkspaceMerge {
                id: WorkspaceId(6),
                push: true,
            }
        );
    }

    /// `--dest` reaches the daemon as the TUI's `where:` step sends it:
    /// absolute, the parent canonicalized, the not-yet-existing leaf kept.
    #[test]
    fn workspace_add_resolves_its_dest_client_side() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let dest = |path: &str| match request_of(&[
            "tutti",
            "workspace",
            "add",
            "2",
            "--name",
            "x",
            "--dest",
            path,
        ]) {
            Request::WorkspaceFork { dest, .. } => dest,
            other => panic!("expected a workspace add, got {other:?}"),
        };
        assert_eq!(dest("api-x"), Some(cwd.join("api-x")));
        assert_eq!(
            dest("../api-x"),
            Some(cwd.parent().unwrap().join("api-x")),
            "the parent is canonicalized, `..` and all"
        );
        assert_eq!(
            dest("/nowhere/api-x"),
            Some(PathBuf::from("/nowhere/api-x")),
            "a parent missing on disk is left for the server to refuse"
        );
    }

    /// The pre-rename spellings still parse — hidden from `--help` — to
    /// exactly the requests their new names send, so scripts keep working.
    #[test]
    fn the_legacy_workspace_spellings_send_the_same_requests() {
        assert_eq!(
            request_of(&["tutti", "workspace", "new", "--dir", "/srv/api"]),
            request_of(&["tutti", "project", "add", "/srv/api"]),
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "new", "--dir", "/srv/api"]),
            Request::WorkspaceNew {
                dir: PathBuf::from("/srv/api")
            }
        );
        for (legacy, new) in [
            (
                &["tutti", "workspace", "fork", "2", "--name", "feature"][..],
                &["tutti", "workspace", "add", "2", "--name", "feature"][..],
            ),
            (
                &[
                    "tutti",
                    "workspace",
                    "fork",
                    "2",
                    "--name",
                    "feature",
                    "-r",
                    "@-",
                ][..],
                &[
                    "tutti",
                    "workspace",
                    "add",
                    "2",
                    "--name",
                    "feature",
                    "-r",
                    "@-",
                ][..],
            ),
        ] {
            assert_eq!(request_of(legacy), request_of(new), "{legacy:?}");
        }
        assert_eq!(
            request_of(&["tutti", "workspace", "fork", "2", "--name", "feature"]),
            Request::WorkspaceFork {
                id: WorkspaceId(2),
                name: "feature".into(),
                revision: None,
                dest: None,
            }
        );
    }

    /// "fork" is banned from user-facing text: no visible help screen says
    /// it, and the legacy spellings that still run are listed nowhere.
    #[test]
    fn no_help_screen_says_fork() {
        fn walk(cmd: &mut clap::Command, path: &str) {
            let help = cmd.render_long_help().to_string();
            assert!(!help.to_lowercase().contains("fork"), "{path}:\n{help}");
            for sub in cmd.get_subcommands_mut().filter(|sub| !sub.is_hide_set()) {
                let path = format!("{path} {}", sub.get_name());
                walk(sub, &path);
            }
        }
        walk(&mut <Cli as clap::CommandFactory>::command(), "tutti");

        let cli = <Cli as clap::CommandFactory>::command();
        let workspace = cli.find_subcommand("workspace").unwrap();
        let visible: Vec<&str> = workspace
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(|sub| sub.get_name())
            .collect();
        assert_eq!(visible, ["add", "list", "kill", "update", "merge", "diff"]);
        assert!(
            workspace
                .get_subcommands()
                .all(|sub| sub.get_visible_aliases().next().is_none())
        );
    }

    #[test]
    fn skill_parses_like_the_hooks_verbs() {
        assert!(matches!(
            Cli::try_parse_from(["tutti", "skill"]).unwrap().command,
            Some(Command::Skill {
                raw: false,
                install: false,
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "skill", "--install", "--project", "--yes"])
                .unwrap()
                .command,
            Some(Command::Skill {
                install: true,
                project: true,
                yes: true,
                ..
            })
        ));
        assert!(Cli::try_parse_from(["tutti", "skill", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["tutti", "skill", "--raw", "--install"]).is_err());
    }

    /// Every `tutti …` line in the skill's code blocks parses against this
    /// CLI, so the skill can never teach a verb or flag that is gone. The
    /// lines hold literal values an agent can adapt — no `<placeholder>`s, no
    /// shell variables (its commands do not share a shell).
    #[test]
    fn the_skill_teaches_only_commands_that_parse() {
        let mut fenced = false;
        let mut parsed = 0;
        for (number, line) in integrations::SKILL.lines().enumerate() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if !fenced || !line.trim_start().starts_with("tutti ") {
                continue;
            }
            let words = shell_words(line);
            assert!(
                words.iter().all(|word| !word.contains(['<', '$'])),
                "line {}: use literal values: {line}",
                number + 1
            );
            if let Err(e) = Cli::try_parse_from(&words) {
                panic!("line {} does not parse: {line}\n{e}", number + 1);
            }
            parsed += 1;
        }
        assert!(!fenced, "an unclosed code block");
        assert!(parsed >= 20, "only {parsed} commands found in the skill");
    }

    /// Split a documented command line as a shell would — words, '…' and "…"
    /// quoting — dropping a trailing `# comment`.
    fn shell_words(line: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word: Option<String> = None;
        let mut quote = None;
        for c in line.chars() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), c) => word.get_or_insert_with(String::new).push(c),
                (None, '\'' | '"') => {
                    quote = Some(c);
                    word.get_or_insert_with(String::new);
                }
                (None, '#') if word.is_none() => break,
                (None, c) if c.is_whitespace() => words.extend(word.take()),
                (None, c) => word.get_or_insert_with(String::new).push(c),
            }
        }
        words.extend(word);
        words
    }

    #[test]
    fn tab_verbs_parse() {
        assert_eq!(
            request_of(&["tutti", "tab", "new", "--workspace", "2"]),
            Request::TabNew {
                workspace: Some(WorkspaceId(2))
            }
        );
        assert_eq!(
            request_of(&["tutti", "tab", "list", "--workspace", "2"]),
            Request::TabList {
                workspace: Some(WorkspaceId(2))
            }
        );
        assert_eq!(
            request_of(&["tutti", "tab", "select", "9"]),
            Request::TabSelect { id: TabId(9) }
        );
    }

    #[test]
    fn tab_new_defaults_to_current_workspace() {
        assert_eq!(
            request_of(&["tutti", "tab", "new"]),
            Request::TabNew { workspace: None }
        );
    }

    #[test]
    fn pane_run_captures_command_after_double_dash() {
        assert_eq!(
            request_of(&[
                "tutti", "pane", "run", "--tab", "3", "--", "claude", "--flag"
            ]),
            Request::PaneRun {
                tab: Some(TabId(3)),
                cmd: vec!["claude".to_string(), "--flag".to_string()],
                ephemeral: false,
                sandbox: None,
            }
        );
    }

    #[test]
    fn pane_run_requires_command_but_not_tab() {
        assert!(Cli::try_parse_from(["tutti", "pane", "run", "--tab", "3"]).is_err());
        assert_eq!(
            request_of(&["tutti", "pane", "run", "--", "ls"]),
            Request::PaneRun {
                tab: None,
                cmd: vec!["ls".into()],
                ephemeral: false,
                sandbox: None,
            }
        );
    }

    #[test]
    fn pane_run_ephemeral_flag_sets_the_field() {
        assert_eq!(
            request_of(&["tutti", "pane", "run", "--ephemeral", "--", "less"]),
            Request::PaneRun {
                tab: None,
                cmd: vec!["less".into()],
                ephemeral: true,
                sandbox: None,
            }
        );
    }

    #[test]
    fn workspace_diff_verb_parses() {
        assert_eq!(
            request_of(&["tutti", "workspace", "diff", "3"]),
            Request::WorkspaceDiff {
                id: WorkspaceId(3),
                stat: false,
            }
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "diff", "3", "--stat"]),
            Request::WorkspaceDiff {
                id: WorkspaceId(3),
                stat: true,
            }
        );
    }

    #[test]
    fn pane_split_direction_maps_to_layout_axis() {
        assert_eq!(
            request_of(&["tutti", "pane", "split", "1", "right"]),
            Request::PaneSplit {
                pane: PaneId(1),
                direction: Direction::Horizontal,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "split", "1", "down"]),
            Request::PaneSplit {
                pane: PaneId(1),
                direction: Direction::Vertical,
            }
        );
    }

    #[test]
    fn pane_send_maps_text_and_keys_exclusively() {
        assert_eq!(
            request_of(&["tutti", "pane", "send", "2", "--text", "hello"]),
            Request::PaneSend {
                pane: PaneId(2),
                text: Some("hello".to_string()),
                keys: None,
                expect: None,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "send", "2", "--keys", "C-c Enter"]),
            Request::PaneSend {
                pane: PaneId(2),
                text: None,
                keys: Some("C-c Enter".to_string()),
                expect: None,
            }
        );
        assert!(Cli::try_parse_from(["tutti", "pane", "send", "2"]).is_err());
        assert!(
            Cli::try_parse_from(["tutti", "pane", "send", "2", "--text", "a", "--keys", "b"])
                .is_err()
        );
    }

    #[test]
    fn guarded_send_and_fingerprint_read_parse() {
        assert_eq!(
            request_of(&[
                "tutti", "pane", "send", "2", "--keys", "enter", "--expect", "9f3a"
            ]),
            Request::PaneSend {
                pane: PaneId(2),
                text: None,
                keys: Some("enter".into()),
                expect: Some("9f3a".into()),
            }
        );
        // `--expect` guards a payload; it is not one.
        assert!(Cli::try_parse_from(["tutti", "pane", "send", "2", "--expect", "9f3a"]).is_err());
        assert_eq!(
            request_of(&["tutti", "pane", "read", "5", "--fingerprint"]),
            Request::PaneFingerprint { pane: PaneId(5) }
        );
        assert!(
            Cli::try_parse_from([
                "tutti",
                "pane",
                "read",
                "5",
                "--fingerprint",
                "--lines",
                "3"
            ])
            .is_err()
        );
    }

    #[test]
    fn pane_approve_allows_or_denies_with_a_reason() {
        assert_eq!(
            request_of(&["tutti", "pane", "approve", "3"]),
            Request::ApprovalAnswer {
                pane: PaneId(3),
                id: None,
                decision: ApprovalDecision::Allow,
            }
        );
        assert_eq!(
            request_of(&[
                "tutti",
                "pane",
                "approve",
                "3",
                "--deny",
                "--message",
                "not on main"
            ]),
            Request::ApprovalAnswer {
                pane: PaneId(3),
                id: None,
                decision: ApprovalDecision::Deny {
                    message: Some("not on main".into())
                },
            }
        );
        assert!(
            Cli::try_parse_from(["tutti", "pane", "approve", "3", "--message", "why"]).is_err(),
            "a reason is a denial's"
        );
    }

    #[test]
    fn pane_read_and_rename_parse() {
        assert_eq!(
            request_of(&["tutti", "pane", "read", "5", "--lines", "40", "--unwrapped"]),
            Request::PaneRead {
                pane: PaneId(5),
                lines: Some(40),
                unwrapped: true,
                since: None,
                last_output: false,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "read", "5"]),
            Request::PaneRead {
                pane: PaneId(5),
                lines: None,
                unwrapped: false,
                since: None,
                last_output: false,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "rename", "5", "builder"]),
            Request::PaneRename {
                pane: PaneId(5),
                title: "builder".to_string(),
            }
        );
    }

    #[test]
    fn pane_read_since_combines_with_lines_and_unwrapped_but_not_fingerprint() {
        assert_eq!(
            request_of(&[
                "tutti",
                "pane",
                "read",
                "5",
                "--since",
                "177",
                "--lines",
                "40",
                "--unwrapped"
            ]),
            Request::PaneRead {
                pane: PaneId(5),
                lines: Some(40),
                unwrapped: true,
                since: Some(177),
                last_output: false,
            }
        );
        assert!(
            Cli::try_parse_from([
                "tutti",
                "pane",
                "read",
                "5",
                "--since",
                "0",
                "--fingerprint"
            ])
            .is_err()
        );
    }

    #[test]
    fn pane_read_last_output_combines_with_unwrapped_but_not_since_or_fingerprint() {
        assert_eq!(
            request_of(&["tutti", "pane", "read", "5", "--last-output", "--unwrapped"]),
            Request::PaneRead {
                pane: PaneId(5),
                lines: None,
                unwrapped: true,
                since: None,
                last_output: true,
            }
        );
        for other in [["--since", "0"].as_slice(), &["--fingerprint"]] {
            let args = ["tutti", "pane", "read", "5", "--last-output"];
            assert!(
                Cli::try_parse_from(args.iter().chain(other)).is_err(),
                "{other:?}"
            );
        }
    }

    #[test]
    fn pane_kill_and_list_parse() {
        assert_eq!(
            request_of(&["tutti", "pane", "kill", "7"]),
            Request::PaneKill { pane: PaneId(7) }
        );
        assert_eq!(request_of(&["tutti", "pane", "list"]), Request::PaneList);
    }

    #[test]
    fn pane_focus_parses() {
        assert_eq!(
            request_of(&["tutti", "pane", "focus", "3"]),
            Request::PaneFocus { pane: PaneId(3) }
        );
    }

    /// A directional focus or resize names its source only when told to:
    /// left out, the caller's pane (or the daemon's attached one) fills in.
    #[test]
    fn pane_focus_and_resize_by_direction_parse() {
        assert_eq!(
            request_of(&["tutti", "pane", "focus", "--direction", "left"]),
            Request::PaneFocusToward {
                from: None,
                toward: Toward::Left,
                wrap: false,
            }
        );
        assert_eq!(
            request_of(&[
                "tutti",
                "pane",
                "focus",
                "--direction=down",
                "--from",
                "4",
                "--wrap"
            ]),
            Request::PaneFocusToward {
                from: Some(PaneId(4)),
                toward: Toward::Down,
                wrap: true,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "resize", "--direction", "up"]),
            Request::PaneResizeToward {
                from: None,
                toward: Toward::Up,
                delta: RESIZE_DELTA,
            }
        );
        assert_eq!(
            request_of(&[
                "tutti",
                "pane",
                "resize",
                "--direction",
                "right",
                "--amount",
                "0.1",
                "--from",
                "2"
            ]),
            Request::PaneResizeToward {
                from: Some(PaneId(2)),
                toward: Toward::Right,
                delta: 0.1,
            }
        );
        for refused in [
            &["tutti", "pane", "focus"][..],
            &["tutti", "pane", "focus", "3", "--direction", "left"],
            &["tutti", "pane", "focus", "3", "--wrap"],
            &["tutti", "pane", "focus", "--direction", "north"],
            &["tutti", "pane", "resize"],
        ] {
            assert!(Cli::try_parse_from(refused).is_err(), "{refused:?} parsed");
        }
    }

    #[test]
    fn edit_targets_split_off_a_line_and_column() {
        assert_eq!(edit_target("src/a.rs"), ("src/a.rs", None, None));
        assert_eq!(edit_target("src/a.rs:12"), ("src/a.rs", Some(12), None));
        assert_eq!(
            edit_target("src/a.rs:12:5"),
            ("src/a.rs", Some(12), Some(5))
        );
        assert_eq!(edit_target("a:b.rs:3"), ("a:b.rs", Some(3), None));
        assert_eq!(edit_target("notes:todo"), ("notes:todo", None, None));
        assert_eq!(edit_target("a.rs:+3"), ("a.rs:+3", None, None));
        assert_eq!(edit_target("a.rs:"), ("a.rs:", None, None));
    }

    #[test]
    fn pane_wait_parses_panes_states_and_flags() {
        let wait = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Some(Command::Pane {
                action:
                    PaneAction::Wait {
                        panes,
                        until,
                        next,
                        timeout,
                    },
            }) => (
                panes,
                until.into_iter().map(WaitFor::from).collect::<Vec<_>>(),
                next,
                timeout,
            ),
            _ => panic!("expected pane wait"),
        };
        assert_eq!(
            wait(&["tutti", "pane", "wait", "3"]),
            (vec![3], vec![WaitFor::Done], false, None),
            "done, level-triggered and unbounded by default"
        );
        assert_eq!(
            wait(&[
                "tutti",
                "pane",
                "wait",
                "3",
                "4",
                "--until",
                "blocked,done",
                "--until",
                "idle",
                "--next",
                "--timeout",
                "1.5"
            ]),
            (
                vec![3, 4],
                vec![WaitFor::Blocked, WaitFor::Done, WaitFor::Idle],
                true,
                Some(1.5)
            )
        );
        assert!(Cli::try_parse_from(["tutti", "pane", "wait"]).is_err());
        assert!(
            Cli::try_parse_from(["tutti", "pane", "wait", "3", "--until", "finished"]).is_err()
        );
    }

    #[test]
    fn run_parses_its_target_and_command() {
        assert_eq!(
            request_of(&["tutti", "run", "--title", "fixer", "--", "claude", "fix it"]),
            Request::Run {
                workspace: None,
                dir: None,
                cmd: vec!["claude".into(), "fix it".into()],
                sandbox: None,
                title: Some("fixer".into()),
                from: None,
                issue: None,
            }
        );
        assert_eq!(
            request_of(&["tutti", "run", "--workspace", "2", "--", "sh"]),
            Request::Run {
                workspace: Some(WorkspaceId(2)),
                dir: None,
                cmd: vec!["sh".into()],
                sandbox: None,
                title: None,
                from: None,
                issue: None,
            }
        );
        let Request::Run { dir: Some(dir), .. } =
            request_of(&["tutti", "run", "--dir", ".", "--", "sh"])
        else {
            panic!("expected a run with a dir");
        };
        assert!(dir.is_absolute(), "absolutized client-side: {dir:?}");
        assert!(
            Cli::try_parse_from(["tutti", "run", "--workspace", "2", "--dir", ".", "--", "sh"])
                .is_err(),
            "one target at most"
        );
        assert!(
            Cli::try_parse_from(["tutti", "run"]).is_err(),
            "a command is required"
        );
    }

    #[test]
    fn sandbox_verbs_parse() {
        assert_eq!(
            request_of(&["tutti", "sandbox", "pull", "3"]),
            Request::SandboxPull {
                pane: PaneId(3),
                name: None,
            }
        );
        assert_eq!(
            request_of(&["tutti", "sandbox", "pull", "3", "--name", "review"]),
            Request::SandboxPull {
                pane: PaneId(3),
                name: Some("review".into()),
            }
        );
        let run = Cli::try_parse_from([
            "tutti",
            "sandbox",
            "run",
            "orb",
            "--name",
            "tutti-p3-ab",
            "--",
            "claude",
        ]);
        assert!(matches!(
            run.unwrap().command,
            Some(Command::Sandbox {
                action: SandboxAction::Run { name, cmd, .. },
            }) if name.as_deref() == Some("tutti-p3-ab") && cmd == ["claude"]
        ));
        assert!(
            matches!(
                Cli::try_parse_from(["tutti", "sandbox", "run", "orb", "--", "claude"])
                    .unwrap()
                    .command,
                Some(Command::Sandbox {
                    action: SandboxAction::Run { name: None, .. },
                })
            ),
            "a daemon from before names existed still launches the wrapper"
        );
    }

    #[test]
    fn issue_verbs_parse() {
        let issue = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Some(Command::Issue { action }) => action,
            _ => panic!("expected an issue verb: {args:?}"),
        };
        assert!(matches!(
            issue(&["tutti", "issue", "list"]),
            IssueAction::List { source: None }
        ));
        assert!(matches!(
            issue(&["tutti", "issue", "list", "--source", "gh"]),
            IssueAction::List { source: Some(s) } if s == "gh"
        ));
        let IssueAction::Deploy {
            source,
            id,
            agent,
            project,
            here,
            sandbox,
        } = issue(&["tutti", "issue", "deploy", "linear", "ENG-42"])
        else {
            panic!("expected a deploy");
        };
        assert_eq!(
            (source.as_str(), id.as_str(), agent.as_str()),
            ("linear", "ENG-42", "claude"),
            "claude unless told otherwise"
        );
        assert_eq!((project, here, sandbox), (None, false, None));
        assert!(matches!(
            issue(&[
                "tutti", "issue", "deploy", "gh", "123", "--agent", "opencode", "--project",
                "2", "--here", "--sandbox", "orb"
            ]),
            IssueAction::Deploy { agent, project: Some(2), here: true, sandbox: Some(s), .. }
                if agent == "opencode" && s == "orb"
        ));
        assert!(
            Cli::try_parse_from(["tutti", "issue", "deploy", "gh"]).is_err(),
            "deploy names the issue"
        );
        let cli = Cli::try_parse_from(["tutti", "issue", "list"]).unwrap();
        assert!(
            to_request(cli.command.unwrap()).is_err(),
            "issues are the client's business, not the daemon's"
        );
    }

    #[test]
    fn checkpoint_verbs_parse() {
        assert_eq!(
            request_of(&["tutti", "pane", "checkpoints", "3"]),
            Request::PaneCheckpoints { pane: PaneId(3) }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "checkpoint-diff", "3"]),
            Request::PaneCheckpointDiff {
                pane: PaneId(3),
                n: None,
                stat: false,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "checkpoint-diff", "3", "2", "--stat"]),
            Request::PaneCheckpointDiff {
                pane: PaneId(3),
                n: Some(2),
                stat: true,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "restore", "3", "1"]),
            Request::PaneRestore {
                pane: PaneId(3),
                n: 1,
                force: false,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "restore", "3", "1", "--force"]),
            Request::PaneRestore {
                pane: PaneId(3),
                n: 1,
                force: true,
            }
        );
        assert!(
            Cli::try_parse_from(["tutti", "pane", "restore", "3"]).is_err(),
            "restore names the checkpoint explicitly"
        );
    }

    #[test]
    fn server_and_attach_parse() {
        assert!(matches!(
            Cli::try_parse_from(["tutti", "server", "start", "--foreground"])
                .unwrap()
                .command,
            Some(Command::Server {
                action: ServerAction::Start { foreground: true }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "server", "stop"])
                .unwrap()
                .command,
            Some(Command::Server {
                action: ServerAction::Stop
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "server", "restore"])
                .unwrap()
                .command,
            Some(Command::Server {
                action: ServerAction::Restore { discard: false }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "server", "restore", "--discard"])
                .unwrap()
                .command,
            Some(Command::Server {
                action: ServerAction::Restore { discard: true }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "attach"]).unwrap().command,
            Some(Command::Attach)
        ));
        let cli = Cli::try_parse_from(["tutti", "watch", "-s", "work"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Watch)));
        assert_eq!(cli.session, "work");
    }

    #[test]
    fn agent_event_and_hooks_parse() {
        assert!(matches!(
            Cli::try_parse_from(["tutti", "agent-event", "claude"])
                .unwrap()
                .command,
            Some(Command::AgentEvent {
                agent: HookAgent::Claude
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "hooks", "claude"])
                .unwrap()
                .command,
            Some(Command::Hooks {
                agent: HookAgent::Claude,
                raw: false,
                install: false,
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "hooks", "claude", "--install", "--yes"])
                .unwrap()
                .command,
            Some(Command::Hooks {
                install: true,
                yes: true,
                project: false,
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "agent-event", "opencode"])
                .unwrap()
                .command,
            Some(Command::AgentEvent {
                agent: HookAgent::Opencode
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "hooks", "opencode", "--install", "--project"])
                .unwrap()
                .command,
            Some(Command::Hooks {
                agent: HookAgent::Opencode,
                install: true,
                project: true,
                yes: false,
                ..
            })
        ));
        // --yes/--project require --install; --raw and --tty conflict with it.
        assert!(Cli::try_parse_from(["tutti", "hooks", "claude", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["tutti", "hooks", "claude", "--raw", "--install"]).is_err());
        assert!(Cli::try_parse_from(["tutti", "hooks", "claude", "--tty", "--install"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["tutti", "hooks", "claude", "--tty", "--raw"])
                .unwrap()
                .command,
            Some(Command::Hooks {
                agent: HookAgent::Claude,
                tty: true,
                raw: true,
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["tutti", "hooks", "claude", "--raw"])
                .unwrap()
                .command,
            Some(Command::Hooks {
                agent: HookAgent::Claude,
                raw: true,
                ..
            })
        ));
        // An unknown agent is rejected at parse time.
        assert!(Cli::try_parse_from(["tutti", "agent-event", "codex"]).is_err());
    }

    #[test]
    fn project_add_absolutizes_a_relative_dir() {
        // A relative `.` must reach the daemon as an absolute path so its
        // git-branch probe resolves against the client's directory, not the
        // daemon's.
        for args in [
            &["tutti", "project", "add", "."][..],
            &["tutti", "workspace", "new", "--dir", "."][..],
        ] {
            let Request::WorkspaceNew { dir } = request_of(args) else {
                panic!("expected a workspace-new request");
            };
            assert!(
                dir.is_absolute(),
                "the dir is absolutized client-side: {dir:?}"
            );
        }
    }

    #[test]
    fn projects_to_create_skips_already_mounted_dirs_and_trailing_slashes() {
        let existing = vec![PathBuf::from("/a/b")];
        let projects = vec![PathBuf::from("/a/b/"), PathBuf::from("/c")];
        let out = projects_to_create(&projects, &existing, Path::to_path_buf);
        assert_eq!(
            out,
            vec![PathBuf::from("/c")],
            "the trailing-slash duplicate is skipped, /c is new"
        );
    }

    #[test]
    fn projects_to_create_dedups_via_canonicalization() {
        // A symlink and its target canonicalize to the same path.
        let existing = vec![PathBuf::from("/real")];
        let projects = vec![PathBuf::from("/link"), PathBuf::from("/other")];
        let canon = |p: &Path| {
            if p == Path::new("/link") {
                PathBuf::from("/real")
            } else {
                p.to_path_buf()
            }
        };
        let out = projects_to_create(&projects, &existing, canon);
        assert_eq!(
            out,
            vec![PathBuf::from("/other")],
            "the symlink resolves to an existing dir"
        );
    }

    #[test]
    fn mount_projects_requests_workspace_then_shell_per_project() {
        let mut sent = Vec::new();
        let notices = mount_projects(
            &[PathBuf::from("/a"), PathBuf::from("/b")],
            &[],
            "/bin/zsh",
            Path::to_path_buf,
            |req| {
                sent.push(req.clone());
                Ok(match req {
                    Request::WorkspaceNew { .. } => {
                        Response::WorkspaceCreated { id: WorkspaceId(1) }
                    }
                    Request::PaneRun { .. } => Response::PaneCreated { id: PaneId(1) },
                    _ => Response::Ok,
                })
            },
        )
        .unwrap();
        assert!(notices.is_empty());
        assert_eq!(
            sent,
            vec![
                Request::WorkspaceNew {
                    dir: PathBuf::from("/a")
                },
                Request::PaneRun {
                    tab: None,
                    cmd: vec!["/bin/zsh".into()],
                    ephemeral: false,
                    sandbox: None,
                },
                Request::WorkspaceNew {
                    dir: PathBuf::from("/b")
                },
                Request::PaneRun {
                    tab: None,
                    cmd: vec!["/bin/zsh".into()],
                    ephemeral: false,
                    sandbox: None,
                },
            ],
        );
    }

    #[test]
    fn mount_projects_skips_a_dir_already_mounted() {
        let mut sent = Vec::new();
        mount_projects(
            &[PathBuf::from("/a"), PathBuf::from("/b")],
            &[PathBuf::from("/a")],
            "/bin/zsh",
            Path::to_path_buf,
            |req| {
                sent.push(req.clone());
                Ok(match req {
                    Request::WorkspaceNew { .. } => {
                        Response::WorkspaceCreated { id: WorkspaceId(1) }
                    }
                    Request::PaneRun { .. } => Response::PaneCreated { id: PaneId(1) },
                    _ => Response::Ok,
                })
            },
        )
        .unwrap();
        assert_eq!(
            sent,
            vec![
                Request::WorkspaceNew {
                    dir: PathBuf::from("/b")
                },
                Request::PaneRun {
                    tab: None,
                    cmd: vec!["/bin/zsh".into()],
                    ephemeral: false,
                    sandbox: None,
                },
            ],
            "only the not-yet-mounted /b is created",
        );
    }

    #[test]
    fn mount_projects_surfaces_a_failed_project_and_mounts_the_rest() {
        // The server errors at workspace-new for a dir missing on disk, and at
        // pane-run for one that vanishes after mounting; this drives the latter.
        let mut sent = Vec::new();
        let mut last_dir: Option<PathBuf> = None;
        let notices = mount_projects(
            &[PathBuf::from("/missing"), PathBuf::from("/ok")],
            &[],
            "/bin/zsh",
            Path::to_path_buf,
            |req| {
                sent.push(req.clone());
                Ok(match req {
                    Request::WorkspaceNew { dir } => {
                        last_dir = Some(dir.clone());
                        Response::WorkspaceCreated { id: WorkspaceId(1) }
                    }
                    Request::PaneRun { .. } => {
                        if last_dir.as_deref() == Some(Path::new("/missing")) {
                            Response::Error {
                                message: "spawn pty: No such file or directory".into(),
                            }
                        } else {
                            Response::PaneCreated { id: PaneId(1) }
                        }
                    }
                    _ => Response::Ok,
                })
            },
        )
        .unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("/missing"),
            "the notice names the failing path: {notices:?}"
        );
        assert!(
            sent.contains(&Request::WorkspaceNew {
                dir: PathBuf::from("/ok")
            }),
            "the healthy project still mounts after the failure: {sent:?}"
        );
    }

    #[test]
    fn error_response_exits_nonzero() {
        assert_eq!(
            emit(
                Response::Error {
                    message: "boom".into()
                },
                false
            ),
            1
        );
        assert_eq!(emit(Response::Ok, false), 0);
    }
}
