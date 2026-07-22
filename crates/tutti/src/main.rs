use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use tutti_core::{Direction, PaneId, Request, Response, TabId, WorkspaceId};

use tutti::client::{self, Client, StopOutcome};
use tutti::config::Config;
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
    /// With no subcommand, `tutti` attaches (bootstrapping a shell pane when the
    /// session is empty).
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
        // Bare `tutti` and `tutti attach` both attach, bootstrapping the session.
        None | Some(Command::Attach) => {
            attach_session(&cli.session)?;
            Ok(0)
        }
        Some(command) => {
            let request = to_request(command)?;
            let mut client = Client::connect_or_start(&cli.session)?;
            let response = client.request(&request)?;
            Ok(emit(response, cli.json))
        }
    }
}

/// Load config, ensure the session has at least one pane, and attach. A fresh
/// session is bootstrapped with a shell pane in the current directory so
/// `cd repo && tutti` drops straight into a working session.
fn attach_session(session: &str) -> Result<()> {
    let config = Config::load()?;
    bootstrap_if_empty(session)?;
    tutti::attach::run(session, config)?;
    Ok(())
}

/// When the session has no workspaces yet, create one anchored to the current
/// directory and run a login shell in it.
fn bootstrap_if_empty(session: &str) -> Result<()> {
    let mut client = Client::connect_or_start(session)?;
    let workspaces = match client.request(&Request::WorkspaceList)? {
        Response::Workspaces { workspaces } => workspaces,
        other => bail!("unexpected reply to workspace list: {other:?}"),
    };
    if workspaces.is_empty() {
        let dir = std::env::current_dir().context("resolve current directory")?;
        client.request(&Request::WorkspaceNew { dir })?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        client.request(&Request::PaneRun {
            tab: None,
            cmd: vec![shell],
        })?;
    }
    Ok(())
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
            WorkspaceAction::New { dir } => Request::WorkspaceNew { dir },
            WorkspaceAction::List => Request::WorkspaceList,
            WorkspaceAction::Kill { id } => Request::WorkspaceKill {
                id: WorkspaceId(id),
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
            PaneAction::Run { tab, cmd } => Request::PaneRun {
                tab: tab.map(TabId),
                cmd,
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
        Command::Server { .. } | Command::Attach => bail!("not a protocol request"),
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
            Request::WorkspaceKill { id: WorkspaceId(4) }
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
