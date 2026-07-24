use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use tutti_core::{AgentHookEvent, Direction, PaneId, Request, Response, TabId, WorkspaceId};

use tutti::client::{self, Client, StopOutcome};
use tutti::config::Config;
use tutti::hooks;
use tutti::render;

#[derive(Parser)]
#[command(name = "tutti", version, about = "Terminal-native agent multiplexer")]
struct Cli {
    /// Session name (one daemon + socket per session).
    #[arg(short, long, global = true, default_value = "tutti")]
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
    /// Create, list, and remove workspaces.
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
    /// Forward a Claude Code hook event to tutti (reads the hook JSON on stdin).
    /// Wired up by `tutti hooks claude`; a silent no-op outside a tutti pane.
    AgentEvent {
        #[arg(value_enum)]
        agent: HookAgent,
    },
    /// Print or install the settings.json hooks wiring Claude Code to tutti.
    Hooks {
        #[arg(value_enum)]
        agent: HookAgent,
        /// Print only the JSON (omit the install instructions on stderr).
        #[arg(long, conflicts_with = "install")]
        raw: bool,
        /// Merge the hooks into settings.json (shows the change, asks first).
        #[arg(long)]
        install: bool,
        /// Install into ./.claude/settings.json instead of ~/.claude/.
        #[arg(long, requires = "install")]
        project: bool,
        /// Skip the confirmation prompt.
        #[arg(long, requires = "install")]
        yes: bool,
    },
}

/// The agent whose hook schema `agent-event`/`hooks` speak. Only Claude Code is
/// wired up today; the value-enum keeps room for more without changing the shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum HookAgent {
    Claude,
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
}

#[derive(Subcommand)]
enum WorkspaceAction {
    New {
        #[arg(long)]
        dir: PathBuf,
    },
    List,
    Kill {
        id: u64,
        /// Discard a forked workspace's checkout: `jj workspace forget` it and
        /// delete its directory. Refused for a workspace tutti did not fork.
        #[arg(long)]
        discard: bool,
    },
    /// Fork a jj workspace into a named sibling checkout (`jj workspace add`) and
    /// mount it with a shell pane.
    Fork {
        id: u64,
        #[arg(long)]
        name: String,
        /// The revision to check out in the fork (`jj workspace add --revision`).
        #[arg(long, short = 'r')]
        revision: Option<String>,
    },
    /// Update a stale forked workspace (`jj workspace update-stale`).
    Update {
        id: u64,
    },
    /// Merge a child workspace's work back into its origin's trunk bookmark
    /// (`main`, else `master`). Only valid for a workspace tutti created.
    Merge {
        id: u64,
        /// Also `jj git push` the advanced bookmark when the origin has a remote.
        #[arg(long)]
        push: bool,
    },
    /// Show the workspace's jj diff (`--stat` for the summary only).
    Diff {
        id: u64,
        #[arg(long)]
        stat: bool,
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
    },
    Read {
        pane: u64,
        #[arg(long)]
        lines: Option<usize>,
        #[arg(long)]
        unwrapped: bool,
    },
    /// Mark a pane focused (records it active and clears its Done badge).
    Focus {
        pane: u64,
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
        Some(Command::Server { action }) => run_server(action, &cli.session),
        // Bare `tutti` and `tutti attach` both attach, mounting startup projects.
        None | Some(Command::Attach) => {
            attach_session(&cli.session)?;
            Ok(0)
        }
        Some(Command::AgentEvent {
            agent: HookAgent::Claude,
        }) => run_agent_event(&cli.session),
        Some(Command::Hooks {
            agent: HookAgent::Claude,
            raw,
            install,
            project,
            yes,
        }) => {
            if install {
                install_hooks(project, yes)
            } else {
                emit_hooks(raw);
                Ok(0)
            }
        }
        Some(command) => {
            let request = to_request(command)?;
            let mut client = Client::connect_or_start(&cli.session)?;
            let response = client.request(&request)?;
            Ok(emit(response, cli.json))
        }
    }
}

/// Load config, mount any configured startup projects (idempotently), and
/// attach. An empty session with no configured projects attaches straight into
/// the first-run prompt (prefilled with the cwd) rather than assuming it —
/// `first_run` carries that prefill. A project that fails to start yields a
/// `notice` surfaced after attach; the rest still mount.
fn attach_session(session: &str) -> Result<()> {
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
        })? {
            Response::PaneCreated { .. } | Response::Ok => {}
            Response::Error { message } => notices.push(format!("{}: {message}", dir.display())),
            other => bail!("unexpected reply to pane run: {other:?}"),
        }
    }
    Ok(notices)
}

fn run_server(action: ServerAction, session: &str) -> Result<i32> {
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
    }
}

fn to_request(command: Command) -> Result<Request> {
    match command {
        Command::Workspace { action } => Ok(match action {
            WorkspaceAction::New { dir } => Request::WorkspaceNew {
                dir: absolutize(&dir),
            },
            WorkspaceAction::List => Request::WorkspaceList,
            WorkspaceAction::Kill { id, discard } => Request::WorkspaceKill {
                id: WorkspaceId(id),
                discard,
            },
            WorkspaceAction::Fork { id, name, revision } => Request::WorkspaceFork {
                id: WorkspaceId(id),
                name,
                revision,
                // The CLI keeps the sibling-default placement; the guided TUI flow
                // is the only path that chooses a destination.
                dest: None,
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
        Command::Pane { action } => Ok(match action {
            PaneAction::Run {
                tab,
                ephemeral,
                cmd,
            } => Request::PaneRun {
                tab: tab.map(TabId),
                cmd,
                ephemeral,
            },
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
            PaneAction::Send { pane, text, keys } => Request::PaneSend {
                pane: PaneId(pane),
                text,
                keys,
            },
            PaneAction::Read {
                pane,
                lines,
                unwrapped,
            } => Request::PaneRead {
                pane: PaneId(pane),
                lines,
                unwrapped,
            },
            PaneAction::Focus { pane } => Request::PaneFocus { pane: PaneId(pane) },
        }),
        Command::Server { .. }
        | Command::Attach
        | Command::AgentEvent { .. }
        | Command::Hooks { .. } => bail!("not a protocol request"),
    }
}

/// Forward one Claude Code hook event (read as JSON on stdin) to the daemon.
/// Built to never break a Claude session: outside a tutti-spawned pane
/// (`TUTTI_PANE` unset) it exits 0 silently; a malformed or irrelevant event
/// sends nothing; a connect/send failure is a stderr note, never a failure exit.
/// This is the one deliberately never-fail path in the CLI.
fn run_agent_event(session: &str) -> Result<i32> {
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
    let Some(event) = hooks::map_claude_event(&value) else {
        return Ok(0);
    };
    if let Err(err) = send_agent_event(&session, PaneId(pane), event) {
        eprintln!("tutti agent-event: {err:#}");
    }
    Ok(0)
}

/// Send an `AgentEvent` to the session's daemon. Never auto-starts a server — a
/// hook firing with no daemon listening has nothing to report to.
fn send_agent_event(session: &str, pane: PaneId, event: AgentHookEvent) -> Result<()> {
    let mut client = Client::connect(session).context("connecting to tutti-server")?;
    client.request(&Request::AgentEvent { pane, event })?;
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

/// Merge the hooks into settings.json: show before/after, ask (unless `--yes`),
/// back up the old file, write atomically.
fn install_hooks(project: bool, yes: bool) -> Result<i32> {
    use std::io::Write;

    let path = if project {
        PathBuf::from(".claude/settings.json")
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        PathBuf::from(home).join(".claude/settings.json")
    };
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let Some((merged, added)) = hooks::merge_claude_hooks(&existing)? else {
        println!("tutti hooks already installed in {}", path.display());
        return Ok(0);
    };

    let before = existing
        .get("hooks")
        .map(|h| serde_json::to_string_pretty(h).expect("hooks serialize"))
        .unwrap_or_else(|| "(no hooks yet)".into());
    let after = serde_json::to_string_pretty(&merged["hooks"]).expect("hooks serialize");
    println!(
        "{}: adding tutti hooks for {}\n",
        path.display(),
        added.join(", ")
    );
    println!("--- hooks before ---\n{before}\n--- hooks after ---\n{after}\n");

    if !yes {
        eprint!("Write {}? [y/N] ", path.display());
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            eprintln!("aborted; nothing written");
            return Ok(1);
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension("json.bak");
        std::fs::copy(&path, &backup)
            .with_context(|| format!("backing up to {}", backup.display()))?;
        eprintln!("backed up existing file to {}", backup.display());
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&merged).expect("settings serialize"),
    )
    .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    println!("installed tutti hooks into {}", path.display());
    Ok(0)
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
        return i32::from(is_error);
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
        assert_eq!(cli.session, "tutti");
        assert!(!cli.json);

        let cli = Cli::try_parse_from(["tutti", "-s", "work", "--json", "pane", "list"]).unwrap();
        assert_eq!(cli.session, "work");
        assert!(cli.json);
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
    fn workspace_verbs_parse() {
        assert_eq!(
            request_of(&["tutti", "workspace", "new", "--dir", "/srv/api"]),
            Request::WorkspaceNew {
                dir: PathBuf::from("/srv/api")
            }
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
    }

    #[test]
    fn workspace_fork_update_and_discard_parse() {
        assert_eq!(
            request_of(&["tutti", "workspace", "kill", "4", "--discard"]),
            Request::WorkspaceKill {
                id: WorkspaceId(4),
                discard: true,
            }
        );
        assert_eq!(
            request_of(&["tutti", "workspace", "fork", "2", "--name", "feature"]),
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
                "fork",
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
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "send", "2", "--keys", "C-c Enter"]),
            Request::PaneSend {
                pane: PaneId(2),
                text: None,
                keys: Some("C-c Enter".to_string()),
            }
        );
        assert!(Cli::try_parse_from(["tutti", "pane", "send", "2"]).is_err());
        assert!(
            Cli::try_parse_from(["tutti", "pane", "send", "2", "--text", "a", "--keys", "b"])
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
            }
        );
        assert_eq!(
            request_of(&["tutti", "pane", "read", "5"]),
            Request::PaneRead {
                pane: PaneId(5),
                lines: None,
                unwrapped: false,
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
    fn workspace_new_absolutizes_a_relative_dir() {
        // A relative `--dir .` must reach the daemon as an absolute path so its
        // git-branch probe resolves against the client's directory, not the
        // daemon's.
        let req = request_of(&["tutti", "workspace", "new", "--dir", "."]);
        let Request::WorkspaceNew { dir } = req else {
            panic!("expected a workspace-new request");
        };
        assert!(
            dir.is_absolute(),
            "the dir is absolutized client-side: {dir:?}"
        );
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
                },
                Request::WorkspaceNew {
                    dir: PathBuf::from("/b")
                },
                Request::PaneRun {
                    tab: None,
                    cmd: vec!["/bin/zsh".into()],
                    ephemeral: false,
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
                },
            ],
            "only the not-yet-mounted /b is created",
        );
    }

    #[test]
    fn mount_projects_surfaces_a_failed_project_and_mounts_the_rest() {
        // The server errors at pane-run when the dir does not exist on disk.
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
