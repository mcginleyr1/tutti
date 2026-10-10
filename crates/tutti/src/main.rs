use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use tutti_core::{
    AgentHookEvent, Direction, PaneId, Request, Response, TabId, WaitFor, WorkspaceId,
};

use tutti::client::{self, Client, StopOutcome};
use tutti::config::Config;
use tutti::hooks;
use tutti::integrations::{self, Dirs, Integration, Status};
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
        /// The workspace to run in (default: the current one).
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
    /// Run commands in k8s sandboxes (`[[sandboxes]]` in the config).
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
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
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
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
    },
    /// Mark a pane focused (records it active and clears its Done badge).
    Focus {
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
        Some(Command::Sandbox {
            action: SandboxAction::Run { profile, cmd },
        }) => tutti::sandbox::run(Config::load()?.sandbox(&profile)?, &cmd),
        Some(Command::Hooks {
            agent,
            raw,
            install,
            project,
            yes,
        }) => match (agent, install) {
            (HookAgent::Claude, true) => {
                install_integration(Integration::ClaudeHooks, project, yes)
            }
            (HookAgent::Opencode, true) => {
                install_integration(Integration::OpencodePlugin, project, yes)
            }
            (HookAgent::Claude, false) => {
                emit_hooks(raw);
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
        Some(command) => {
            let project = matches!(command, Command::Project { .. });
            let request = to_request(command)?;
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
            match Client::connect(session) {
                Ok(_) => println!("tutti-server already running for session '{session}'"),
                Err(e) if client::not_running(&e) => {
                    Client::connect_or_start(session)?;
                    println!("tutti-server started for session '{session}'");
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
            } => Request::PaneRead {
                pane: PaneId(pane),
                lines,
                unwrapped,
                since,
            },
            PaneAction::Focus { pane } => Request::PaneFocus { pane: PaneId(pane) },
            PaneAction::Wait { .. } => bail!("pane wait sends a request per pane"),
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
        Command::Server { .. }
        | Command::Attach
        | Command::Sandbox { .. }
        | Command::AgentEvent { .. }
        | Command::Hooks { .. }
        | Command::Skill { .. } => bail!("not a protocol request"),
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

/// Forward one agent hook event (read as JSON on stdin) to the daemon. Built to
/// never break an agent session: outside a tutti-spawned pane (`TUTTI_PANE`
/// unset) it exits 0 silently; a malformed or irrelevant event sends nothing; a
/// connect/send failure is a stderr note, never a failure exit. This is the one
/// deliberately never-fail path in the CLI. It never writes stdout either: on
/// `UserPromptSubmit` and `SessionStart` Claude Code hands a hook's stdout to
/// the model as context.
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
    if let Err(err) = send_agent_event(&session, PaneId(pane), event, conversation) {
        eprintln!("tutti agent-event: {err:#}");
    }
    Ok(0)
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

/// Print the Claude Code hooks snippet: the JSON to stdout, plus — unless
/// `--raw` — a few install lines to stderr so the JSON pipes cleanly on its own.
fn emit_hooks(raw: bool) {
    println!("{}", hooks::claude_hooks_snippet());
    if !raw {
        eprintln!("# The object above already includes the \"hooks\" key: merge it into");
        eprintln!("# ~/.claude/settings.json at the TOP level (or .claude/settings.json in");
        eprintln!("# a project) — or let tutti do it: `tutti hooks claude --install`.");
        eprintln!("# Outside a tutti pane the hook exits silently; if you ever uninstall");
        eprintln!("# tutti, remove these entries or Claude will warn on every tool call.");
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
            i32::from(is_error)
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
        Response::Fingerprint { fingerprint } => {
            println!("{fingerprint}");
            0
        }
        Response::Checkpoints { checkpoints } => {
            print!("{}", render::checkpoints(&checkpoints, render::unix_now()));
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
    fn pane_read_and_rename_parse() {
        assert_eq!(
            request_of(&["tutti", "pane", "read", "5", "--lines", "40", "--unwrapped"]),
            Request::PaneRead {
                pane: PaneId(5),
                lines: Some(40),
                unwrapped: true,
                since: None,
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "read", "5"]),
            Request::PaneRead {
                pane: PaneId(5),
                lines: None,
                unwrapped: false,
                since: None,
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
        // --yes/--project require --install; --raw conflicts with --install.
        assert!(Cli::try_parse_from(["tutti", "hooks", "claude", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["tutti", "hooks", "claude", "--raw", "--install"]).is_err());
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
