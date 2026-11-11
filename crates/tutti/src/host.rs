//! The daemon's machine. Everything the TUI needs to know about the machine
//! its panes run on — its directories, its config's sandboxes and issue
//! trackers, its installed agents and resumable conversations — and every
//! change it makes there (installing the setup panel's integrations) goes
//! through a `Host`: answered in-process when the TUI runs beside its daemon,
//! and over the attach connection by `tutti proxy` when it reached the daemon
//! over ssh (`tutti --host`). Either way the answer comes from the daemon's
//! machine, never the one the TUI happens to run on.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tutti_agents::{Registry, ResumeSession};
use tutti_core::Frame;

use crate::config::Config;
use crate::integrations::{self, Dirs, Integration, Plan};
use crate::issues::{self, Issue, Source};
use crate::sandbox::Profile;

/// The revision of the host frames' shapes, checked in the proxy's hello. Bump
/// it on any change to `HostFrame`, `Query` or `Answer`: the tutti on each end
/// of an ssh connection is installed separately.
pub const HOST_REV: u32 = 4;

/// Most answers are a directory read or a file plan away.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// An issue fetch waits on a tracker; it runs off the event loop.
const ISSUES_TIMEOUT: Duration = Duration::from_secs(120);

/// The most directory completions offered at once.
pub const MAX_COMPLETIONS: usize = 8;

/// One `Frame::Host` payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// The proxy's first frame, once it holds a connection to the daemon.
    Hello {
        rev: u32,
        startup: Startup,
    },
    Call {
        id: u64,
        query: Query,
    },
    Reply {
        id: u64,
        answer: Result<Answer, String>,
    },
}

/// What attaching found to say before the TUI opens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Startup {
    /// Arms the first-run new-project prompt, prefilled with this directory:
    /// an empty session with no configured projects.
    pub first_run: Option<String>,
    /// A startup project that failed to mount, to show once attached.
    pub notice: Option<String>,
}

/// A question about the daemon's machine.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "q", rename_all = "snake_case")]
pub enum Query {
    /// Its home and the config sections that live with the daemon.
    Info,
    /// The directories a typed path could go on to.
    CompleteDirs { input: String },
    /// A typed directory, absolute and canonical, and whether it is one.
    ExpandDir { input: String },
    /// A typed destination: its parent canonical, its (new) leaf kept.
    ExpandDest { input: String },
    /// Guided create's `where:` prefill: `<repo>-<name>` beside the repo.
    SiblingDest { dir: PathBuf, name: String },
    /// Whether `dir` lies in a jj repo.
    IsJj { dir: PathBuf },
    /// What the launcher offers there: the installed agents, the login shell,
    /// and the conversations to resume in `dir`.
    Launcher { dir: Option<PathBuf> },
    /// The issues the configured source `source` lists, fetched in `dir`.
    Issues { source: String, dir: PathBuf },
    /// Tell issue `id` of the configured source `source` it was deployed,
    /// from `dir` (`issues::write_back`).
    WriteBack {
        source: String,
        id: String,
        comment: String,
        dir: PathBuf,
    },
    /// `agent`'s `argv` handed `prompt` as its first message, a long one
    /// written to a file in the prompt directory of `session` there
    /// (`issues::deploy_argv`); no `session` for a sandbox, which reads no
    /// file of the host's.
    DeployArgv {
        agent: String,
        argv: Vec<String>,
        prompt: String,
        session: Option<String>,
    },
    /// Remove a prompt file `DeployArgv` wrote that no pane took.
    ForgetPrompt { file: PathBuf },
    /// Each integration planned for the user, or for the project at `project`.
    SetupPlans {
        integrations: Vec<Integration>,
        project: Option<PathBuf>,
    },
    /// Install a plan `SetupPlans` made.
    SetupApply { plan: Plan },
    /// Open `file` — relative to the jj repo root at or above `dir` — in the
    /// editor a pane's `TUTTI_EDITOR` names, through the editor's own CLI.
    Edit {
        editor: String,
        dir: PathBuf,
        file: String,
    },
}

impl Query {
    fn timeout(&self) -> Duration {
        match self {
            Query::Issues { .. } | Query::WriteBack { .. } => ISSUES_TIMEOUT,
            _ => ANSWER_TIMEOUT,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Info(Info),
    Dirs(Vec<String>),
    Dir {
        path: PathBuf,
        exists: bool,
    },
    Path(PathBuf),
    Text(String),
    Yes(bool),
    Launcher(Launcher),
    Issues(Vec<Issue>),
    Plans(Vec<Result<Plan, String>>),
    Installed {
        backup: Option<PathBuf>,
    },
    Deploy {
        cmd: Vec<String>,
        file: Option<PathBuf>,
    },
    Done,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub home: Option<PathBuf>,
    /// Its `$SHELL`, else `/bin/sh`: what a shell pane runs.
    pub shell: String,
    pub sandboxes: Vec<Profile>,
    pub issues: Vec<Source>,
    /// Its config's `[agents.<name>] source` files (`Config::sources`).
    pub sources: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launcher {
    /// The registry kinds installed on its `PATH`, each with the argv that
    /// launches it there (`AgentSpec::argv`: Cursor's `agent` by the path
    /// checked).
    pub installed: BTreeMap<String, Vec<String>>,
    pub resume: Vec<ResumeSession>,
}

/// Where the TUI's questions about the daemon's machine go.
#[derive(Clone)]
pub enum Host {
    Local(Arc<Local>),
    Remote(Remote),
}

impl Host {
    pub fn ask(&self, query: Query) -> Result<Answer> {
        match self {
            Host::Local(local) => local.answer(query),
            Host::Remote(remote) => remote.ask(query),
        }
    }

    pub fn info(&self) -> Result<Info> {
        match self.ask(Query::Info)? {
            Answer::Info(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    /// No completions when the host cannot say: a half-typed path never
    /// flashes an error.
    pub fn complete_dirs(&self, input: &str) -> Vec<String> {
        match self.ask(Query::CompleteDirs {
            input: input.into(),
        }) {
            Ok(Answer::Dirs(dirs)) => dirs,
            _ => Vec::new(),
        }
    }

    pub fn expand_dir(&self, input: &str) -> Result<(PathBuf, bool)> {
        match self.ask(Query::ExpandDir {
            input: input.into(),
        })? {
            Answer::Dir { path, exists } => Ok((path, exists)),
            other => Err(unexpected(other)),
        }
    }

    pub fn expand_dest(&self, input: &str) -> Result<PathBuf> {
        match self.ask(Query::ExpandDest {
            input: input.into(),
        })? {
            Answer::Path(path) => Ok(path),
            other => Err(unexpected(other)),
        }
    }

    pub fn sibling_dest(&self, dir: &Path, name: &str) -> Result<String> {
        match self.ask(Query::SiblingDest {
            dir: dir.into(),
            name: name.into(),
        })? {
            Answer::Text(text) => Ok(text),
            other => Err(unexpected(other)),
        }
    }

    pub fn is_jj(&self, dir: &Path) -> Result<bool> {
        match self.ask(Query::IsJj { dir: dir.into() })? {
            Answer::Yes(yes) => Ok(yes),
            other => Err(unexpected(other)),
        }
    }

    pub fn launcher(&self, dir: Option<&Path>) -> Result<Launcher> {
        match self.ask(Query::Launcher {
            dir: dir.map(Into::into),
        })? {
            Answer::Launcher(launcher) => Ok(launcher),
            other => Err(unexpected(other)),
        }
    }

    pub fn issues(&self, source: &str, dir: &Path) -> Result<Vec<Issue>> {
        match self.ask(Query::Issues {
            source: source.into(),
            dir: dir.into(),
        })? {
            Answer::Issues(issues) => Ok(issues),
            other => Err(unexpected(other)),
        }
    }

    pub fn write_back(&self, source: &str, id: &str, comment: &str, dir: &Path) -> Result<()> {
        match self.ask(Query::WriteBack {
            source: source.into(),
            id: id.into(),
            comment: comment.into(),
            dir: dir.into(),
        })? {
            Answer::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub fn deploy_argv(
        &self,
        agent: &str,
        argv: Vec<String>,
        prompt: &str,
        session: Option<&str>,
    ) -> Result<(Vec<String>, Option<PathBuf>)> {
        match self.ask(Query::DeployArgv {
            agent: agent.into(),
            argv,
            prompt: prompt.into(),
            session: session.map(Into::into),
        })? {
            Answer::Deploy { cmd, file } => Ok((cmd, file)),
            other => Err(unexpected(other)),
        }
    }

    pub fn forget_prompt(&self, file: &Path) -> Result<()> {
        match self.ask(Query::ForgetPrompt { file: file.into() })? {
            Answer::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub fn setup_plans(
        &self,
        integrations: Vec<Integration>,
        project: Option<PathBuf>,
    ) -> Result<Vec<Result<Plan, String>>> {
        match self.ask(Query::SetupPlans {
            integrations,
            project,
        })? {
            Answer::Plans(plans) => Ok(plans),
            other => Err(unexpected(other)),
        }
    }

    pub fn setup_apply(&self, plan: &Plan) -> Result<Option<PathBuf>> {
        match self.ask(Query::SetupApply { plan: plan.clone() })? {
            Answer::Installed { backup } => Ok(backup),
            other => Err(unexpected(other)),
        }
    }

    pub fn edit(&self, editor: &str, dir: &Path, file: &str) -> Result<()> {
        match self.ask(Query::Edit {
            editor: editor.into(),
            dir: dir.into(),
            file: file.into(),
        })? {
            Answer::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}

fn unexpected(answer: Answer) -> anyhow::Error {
    anyhow!("the host answered out of turn: {answer:?}")
}

/// The machine this process runs on, as its environment and config describe
/// it.
#[derive(Clone)]
pub struct Local {
    /// Only its `sandboxes` and `issues` are the host's to say.
    pub config: Config,
    pub dirs: Dirs,
    pub cwd: PathBuf,
    pub path: Option<OsString>,
    /// Where a deploy's long prompts are written: `None` for each session's
    /// own (`tutti_core::prompts_dir`), a scratch directory in tests.
    pub prompts: Option<PathBuf>,
}

impl Local {
    pub fn from_env(config: Config) -> Local {
        Local {
            config,
            dirs: Dirs::from_env(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            path: std::env::var_os("PATH"),
            prompts: None,
        }
    }

    fn source(&self, name: &str) -> Result<&Source> {
        self.config
            .issues
            .iter()
            .find(|s| s.name == name)
            .with_context(|| format!("no issue source named {name}"))
    }

    pub fn answer(&self, query: Query) -> Result<Answer> {
        let home = self.dirs.home.as_deref();
        Ok(match query {
            Query::Info => Answer::Info(Info {
                home: self.dirs.home.clone(),
                shell: self
                    .dirs
                    .shell
                    .as_ref()
                    .map_or_else(|| "/bin/sh".into(), |shell| shell.display().to_string()),
                sandboxes: self.config.sandboxes.clone(),
                issues: self.config.issues.clone(),
                sources: self.config.sources.clone(),
            }),
            Query::CompleteDirs { input } => Answer::Dirs(complete_dirs(&input, home, &self.cwd)),
            Query::ExpandDir { input } => {
                let resolved = resolve_dir(&input, home, &self.cwd);
                let path = std::fs::canonicalize(&resolved).unwrap_or(resolved);
                Answer::Dir {
                    exists: path.is_dir(),
                    path,
                }
            }
            Query::ExpandDest { input } => {
                let resolved = resolve_dir(&input, home, &self.cwd);
                Answer::Path(match (resolved.parent(), resolved.file_name()) {
                    (Some(parent), Some(leaf)) => std::fs::canonicalize(parent)
                        .unwrap_or_else(|_| parent.to_path_buf())
                        .join(leaf),
                    _ => resolved,
                })
            }
            Query::SiblingDest { dir, name } => Answer::Text(sibling_dest(&dir, &name, home)),
            Query::IsJj { dir } => Answer::Yes(jj_root(&dir).is_some()),
            Query::Launcher { dir } => {
                let path = self.path.as_deref();
                let installed = Registry::default()
                    .specs()
                    .iter()
                    .filter(|spec| spec.installed(path).is_some())
                    .map(|spec| (spec.kind.to_string(), spec.argv(path)))
                    .collect();
                let resume = match (dir, home) {
                    (Some(dir), Some(home)) => tutti_agents::resume_sessions(
                        &dir,
                        &tutti_agents::Stores::from_env(home),
                        3,
                    ),
                    _ => Vec::new(),
                };
                Answer::Launcher(Launcher { installed, resume })
            }
            Query::Issues { source, dir } => {
                Answer::Issues(issues::list(self.source(&source)?, &dir)?)
            }
            Query::WriteBack {
                source,
                id,
                comment,
                dir,
            } => {
                issues::write_back(self.source(&source)?, &id, &comment, &dir)?;
                Answer::Done
            }
            Query::DeployArgv {
                agent,
                argv,
                prompt,
                session,
            } => {
                let registry = Registry::default();
                let spec = registry
                    .spec(&agent.as_str().into())
                    .with_context(|| format!("no agent named {agent}"))?;
                let files = session.map(|session| {
                    self.prompts
                        .clone()
                        .unwrap_or_else(|| tutti_core::prompts_dir(&session))
                });
                let (cmd, file) = issues::deploy_argv(spec, argv, &prompt, files.as_deref())?;
                Answer::Deploy { cmd, file }
            }
            Query::ForgetPrompt { file } => {
                let _ = std::fs::remove_file(file);
                Answer::Done
            }
            Query::SetupPlans {
                integrations,
                project,
            } => {
                let dirs = Dirs {
                    project,
                    ..self.dirs.clone()
                };
                Answer::Plans(
                    integrations
                        .into_iter()
                        .map(|i| i.plan(&dirs).map_err(|e| format!("{e:#}")))
                        .collect(),
                )
            }
            Query::SetupApply { plan } => Answer::Installed {
                backup: integrations::apply(&plan)?,
            },
            Query::Edit { editor, dir, file } => {
                let root = jj_root(&dir).unwrap_or(dir);
                crate::editor::open(&editor, &root.join(file), None, None)?;
                Answer::Done
            }
        })
    }
}

/// Answers from the `tutti proxy` at the far end of an attach connection.
/// Calls may overlap (the issue fetches run on threads of their own): each
/// waits on its own channel for the reply that carries its id.
#[derive(Clone)]
pub struct Remote {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    waiting: Waiting,
    next: Arc<AtomicU64>,
}

/// The calls a `Remote` has out, by id, for the connection's reader to route
/// replies to (`route`).
pub type Waiting = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Answer, String>>>>>;

impl Remote {
    pub fn new(writer: Arc<Mutex<Box<dyn Write + Send>>>, waiting: Waiting) -> Remote {
        Remote {
            writer,
            waiting,
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    fn ask(&self, query: Query) -> Result<Answer> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let timeout = query.timeout();
        let (tx, rx) = mpsc::channel();
        lock(&self.waiting).insert(id, tx);
        let frame = host_frame(&HostFrame::Call { id, query });
        let sent = {
            let mut writer = lock(&self.writer);
            writer.write_all(&frame).and_then(|()| writer.flush())
        };
        let answer = sent.map(|()| rx.recv_timeout(timeout));
        lock(&self.waiting).remove(&id);
        match answer.context("asking the host")? {
            Ok(answer) => answer.map_err(|e| anyhow!(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("the host did not answer within {}s", timeout.as_secs())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the connection to the host closed"),
        }
    }
}

/// Hand a host frame the connection read to whoever waits on it: a reply to
/// its call, the hello to `hello`. Anything else is dropped.
pub fn route(json: &[u8], waiting: &Waiting, hello: &mpsc::Sender<(u32, Startup)>) {
    match serde_json::from_slice(json) {
        Ok(HostFrame::Reply { id, answer }) => {
            if let Some(tx) = lock(waiting).remove(&id) {
                let _ = tx.send(answer);
            }
        }
        Ok(HostFrame::Hello { rev, startup }) => {
            let _ = hello.send((rev, startup));
        }
        _ => {}
    }
}

fn host_frame(frame: &HostFrame) -> Vec<u8> {
    Frame::Host(serde_json::to_vec(frame).expect("host frames serialize")).encode()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `tutti proxy`: carry frames between stdin/stdout — an ssh channel — and the
/// daemon at `daemon`, answering host calls from this machine (`local`) along
/// the way. Says hello first, so the far end knows the daemon is reached.
/// Every frame goes to stdout whole, under its lock, so replies never land
/// inside a pane frame. Returns when the daemon closes the connection.
pub fn proxy(daemon: UnixStream, local: Local, startup: Startup) -> Result<()> {
    let out: Out = Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let hello = HostFrame::Hello {
        rev: HOST_REV,
        startup,
    };
    emit(&out, &host_frame(&hello))?;
    let local = Arc::new(local);
    let mut to_daemon = daemon.try_clone().context("cloning the daemon socket")?;
    let replies = Arc::clone(&out);
    std::thread::spawn(move || {
        let _ = forward(
            &mut std::io::stdin().lock(),
            &mut to_daemon,
            &local,
            &replies,
        );
        let _ = to_daemon.shutdown(Shutdown::Write);
    });
    each_frame(&mut &daemon, |bytes, _| emit(&out, bytes))
}

/// Where the proxy writes, shared by the daemon's frames and host replies.
type Out = Arc<Mutex<Box<dyn Write + Send>>>;

/// Client → daemon: every frame passes through but host calls, each answered
/// on a thread of its own (an issue fetch can take a while) onto `out`.
fn forward(
    input: &mut impl Read,
    daemon: &mut impl Write,
    local: &Arc<Local>,
    out: &Out,
) -> Result<()> {
    each_frame(input, |bytes, frame| {
        let Frame::Host(json) = frame else {
            return daemon.write_all(bytes).context("writing to the daemon");
        };
        let Ok(HostFrame::Call { id, query }) = serde_json::from_slice(&json) else {
            return Ok(());
        };
        let (local, out) = (Arc::clone(local), Arc::clone(out));
        std::thread::spawn(move || {
            let answer = local.answer(query).map_err(|e| format!("{e:#}"));
            let _ = emit(&out, &host_frame(&HostFrame::Reply { id, answer }));
        });
        Ok(())
    })
}

/// Read whole frames from `input` until it closes, handing each one's bytes
/// and its decoding to `each`.
fn each_frame(
    input: &mut impl Read,
    mut each: impl FnMut(&[u8], Frame) -> Result<()>,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        while let Some((frame, consumed)) = Frame::decode(&buf)? {
            each(&buf[..consumed], frame)?;
            buf.drain(..consumed);
        }
        match input.read(&mut chunk)? {
            0 => return Ok(()),
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// One whole frame onto `out`, under its lock: nothing lands inside it.
fn emit(out: &Out, bytes: &[u8]) -> Result<()> {
    let mut out = lock(out);
    out.write_all(bytes)?;
    out.flush().context("writing the connection")
}

/// Pure directory resolution: `~`/`~/rest` against `home`, relative paths
/// against `cwd`, absolute paths untouched.
pub fn resolve_dir(input: &str, home: Option<&Path>, cwd: &Path) -> PathBuf {
    if let Some(home) = home {
        if input == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = input.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    let path = Path::new(input);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Directory completions for a typed path: the sub-directories of the input's
/// parent whose names begin with its final path component (an empty
/// component lists every sub-directory). Directories only, dot-dirs hidden
/// unless the component itself starts with `.`, alphabetical, capped at
/// `MAX_COMPLETIONS`. An unreadable parent yields nothing. Tilde/relative
/// resolution matches `resolve_dir`.
pub fn complete_dirs(input: &str, home: Option<&Path>, cwd: &Path) -> Vec<String> {
    let (dir_part, comp) = match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    };
    let base = resolve_dir(dir_part, home, cwd);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|name| name.starts_with(comp) && (comp.starts_with('.') || !name.starts_with('.')))
        .collect();
    names.sort();
    names.truncate(MAX_COMPLETIONS);
    names
}

/// The `where:` prefill for guided create: the sibling default
/// `<repo-parent>/<repo>-<name>`, home-shortened for display so the parent is
/// canonicalized on submit. Falls back to a sibling of `dir` itself when `dir`
/// is not under a `.jj` repo (the server rejects a non-jj source).
pub fn sibling_dest(dir: &Path, name: &str, home: Option<&Path>) -> String {
    let root = jj_root(dir).unwrap_or_else(|| dir.to_path_buf());
    let base = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sibling = root
        .parent()
        .unwrap_or(&root)
        .join(format!("{base}-{name}"));
    if let Some(rest) = home.and_then(|home| sibling.strip_prefix(home).ok()) {
        return format!("~/{}", rest.display());
    }
    sibling.display().to_string()
}

/// The `.jj` repo root at or above `dir`, as the server's `workspace_root`
/// finds it.
pub fn jj_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| d.join(".jj").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory tree seeded with `subdirs`, uniquely named so
    /// parallel test runs never collide.
    fn temp_tree(subdirs: &[&str]) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("tutti-host-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for d in subdirs {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::canonicalize(root).unwrap()
    }

    /// A host whose home and cwd are `home`, with nothing on its PATH.
    fn local(home: &Path) -> Local {
        Local {
            config: Config::default(),
            dirs: Dirs {
                home: Some(home.into()),
                shell: Some("/bin/zsh".into()),
                ..Dirs::default()
            },
            cwd: home.into(),
            path: None,
            prompts: None,
        }
    }

    #[test]
    fn resolve_dir_expands_home_and_relative_paths() {
        let home = Path::new("/home/alice");
        let cwd = Path::new("/work/proj");
        assert_eq!(
            resolve_dir("~", Some(home), cwd),
            PathBuf::from("/home/alice")
        );
        assert_eq!(
            resolve_dir("~/src/api", Some(home), cwd),
            PathBuf::from("/home/alice/src/api")
        );
        assert_eq!(
            resolve_dir("sub/dir", Some(home), cwd),
            PathBuf::from("/work/proj/sub/dir")
        );
        assert_eq!(resolve_dir("/etc", Some(home), cwd), PathBuf::from("/etc"));
        assert_eq!(
            resolve_dir("~", None, cwd),
            PathBuf::from("/work/proj/~"),
            "without a home, ~ is treated as a relative path"
        );
    }

    #[test]
    fn complete_dirs_lists_matching_subdirs_only() {
        let root = temp_tree(&["alpha", "altair", "beta", ".hidden"]);
        std::fs::write(root.join("apple.txt"), b"x").unwrap();
        let home = Path::new("/no-home");
        let cwd = Path::new("/no-cwd");

        // A prefix matches directories only — the `apple.txt` file is excluded.
        let got = complete_dirs(&format!("{}/al", root.display()), Some(home), cwd);
        assert_eq!(got, vec!["alpha".to_string(), "altair".to_string()]);

        // An empty component lists every visible sub-directory, alphabetical.
        let got = complete_dirs(&format!("{}/", root.display()), Some(home), cwd);
        assert_eq!(got, vec!["alpha", "altair", "beta"]);

        // A `.`-leading component reveals the dot-directory it would otherwise hide.
        let got = complete_dirs(&format!("{}/.", root.display()), Some(home), cwd);
        assert_eq!(got, vec![".hidden"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn complete_dirs_caps_at_eight_and_treats_unreadable_as_empty() {
        let many: Vec<String> = (0..12).map(|i| format!("d{i:02}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let root = temp_tree(&refs);

        let got = complete_dirs(&format!("{}/", root.display()), None, Path::new("/"));
        assert_eq!(got.len(), MAX_COMPLETIONS, "capped at eight matches");
        assert_eq!(got[0], "d00", "the first eight, alphabetical");
        assert_eq!(got[7], "d07");

        // A parent that cannot be read yields nothing rather than an error.
        let missing = format!("{}/nope/x", root.display());
        assert!(complete_dirs(&missing, None, Path::new("/")).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The launcher's installed agents are what the host's own PATH holds,
    /// each with the argv it launches as there; a file that is not executable
    /// installs nothing.
    #[test]
    fn a_hosts_launcher_names_what_its_own_path_holds() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_tree(&[]);
        for bin in ["claude", "ori"] {
            std::fs::write(dir.join(bin), b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(dir.join(bin), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(dir.join("codex"), b"x").unwrap();
        let host = Local {
            path: Some(OsString::from(&dir)),
            ..local(&dir)
        };
        match host.answer(Query::Launcher { dir: None }).unwrap() {
            Answer::Launcher(found) => {
                let installed: Vec<(&str, Vec<&str>)> = found
                    .installed
                    .iter()
                    .map(|(kind, argv)| (kind.as_str(), argv.iter().map(String::as_str).collect()))
                    .collect();
                assert_eq!(
                    installed,
                    [("claude", vec!["claude"]), ("ori", vec!["ori", "code"])]
                );
                assert!(found.resume.is_empty(), "no directory, no conversations");
            }
            other => panic!("{other:?}"),
        }
        match local(&dir).answer(Query::Launcher { dir: None }).unwrap() {
            Answer::Launcher(found) => assert!(found.installed.is_empty(), "no PATH, nothing"),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_host_answers_from_its_own_home_and_disk() {
        let home = temp_tree(&["src/repo/.jj", "src/repo/sub", "plain"]);
        let host = Host::Local(Arc::new(local(&home)));

        let info = host.info().unwrap();
        assert_eq!(
            (info.home.as_deref(), info.shell.as_str()),
            (Some(home.as_path()), "/bin/zsh")
        );
        assert_eq!(host.complete_dirs("~/s"), ["src"], "~ is the host's home");
        assert_eq!(
            host.expand_dir("src/repo").unwrap(),
            (home.join("src/repo"), true),
            "relative to the host's cwd, canonical"
        );
        assert_eq!(
            host.expand_dir("~/nope").unwrap(),
            (home.join("nope"), false)
        );
        assert_eq!(
            host.expand_dest("~/src/new").unwrap(),
            home.join("src/new"),
            "a destination keeps its leaf"
        );
        assert!(host.is_jj(&home.join("src/repo/sub")).unwrap());
        assert!(!host.is_jj(&home.join("plain")).unwrap());
        assert_eq!(
            host.sibling_dest(&home.join("src/repo/sub"), "fix")
                .unwrap(),
            "~/src/repo-fix",
            "beside the repo root, home-shortened"
        );
        assert!(
            host.issues("nope", &home)
                .unwrap_err()
                .to_string()
                .contains("no issue source named nope")
        );
        assert!(
            host.write_back("nope", "1", "deployed", &home)
                .unwrap_err()
                .to_string()
                .contains("no issue source named nope"),
            "a write-back goes by the host's own sources"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn host_frames_keep_their_shape_on_the_wire() {
        let call = HostFrame::Call {
            id: 3,
            query: Query::CompleteDirs {
                input: "~/s".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&call).unwrap(),
            r#"{"type":"call","id":3,"query":{"q":"complete_dirs","input":"~/s"}}"#
        );
        let reply = HostFrame::Reply {
            id: 3,
            answer: Ok(Answer::Dirs(vec!["src".into()])),
        };
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"type":"reply","id":3,"answer":{"Ok":{"dirs":["src"]}}}"#
        );
        let hello = r#"{"type":"hello","rev":4,"startup":{"first_run":"/srv","notice":null}}"#;
        match serde_json::from_str(hello).unwrap() {
            HostFrame::Hello { rev, startup } => {
                assert_eq!(
                    (rev, startup.first_run.as_deref()),
                    (HOST_REV, Some("/srv"))
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A `Remote` against the proxy's own forwarding over a socket pair: host
    /// calls are answered by the far side's machine — overlapping calls each
    /// get their own reply — while every other frame passes on to the daemon.
    #[test]
    fn a_remote_host_is_answered_by_the_proxy_and_daemon_frames_pass_through() {
        let home = temp_tree(&["far/away"]);
        let (near, far) = UnixStream::pair().unwrap();
        let (daemon, daemon_side) = UnixStream::pair().unwrap();

        // The proxy end: reads `far`, answers host calls back onto it, sends
        // the rest to the daemon.
        let local = Arc::new(local(&home));
        let out: Out = Arc::new(Mutex::new(Box::new(far.try_clone().unwrap())));
        let mut proxy_in = far.try_clone().unwrap();
        let mut to_daemon = daemon.try_clone().unwrap();
        std::thread::spawn(move || forward(&mut proxy_in, &mut to_daemon, &local, &out));

        // The TUI end: replies are routed to their calls.
        let waiting = Waiting::default();
        let (hello, _) = mpsc::channel();
        let routes = Arc::clone(&waiting);
        let mut near_in = near.try_clone().unwrap();
        std::thread::spawn(move || {
            each_frame(&mut near_in, |_, frame| {
                if let Frame::Host(json) = frame {
                    route(&json, &routes, &hello);
                }
                Ok(())
            })
        });
        let writer: Box<dyn Write + Send> = Box::new(near.try_clone().unwrap());
        let host = Host::Remote(Remote::new(Arc::new(Mutex::new(writer)), waiting));

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let host = host.clone();
                std::thread::spawn(move || host.complete_dirs("~/far/"))
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), ["away"]);
        }
        assert_eq!(
            host.info().unwrap().home.as_deref(),
            Some(home.as_path()),
            "the far side's home, not this one's"
        );
        assert!(
            host.expand_dest("/no/such/parent/x").is_ok(),
            "a missing parent is left for the daemon to refuse"
        );
        assert_eq!(
            host.deploy_argv("claude", vec!["claude".into()], "fix it", Some("s"))
                .unwrap(),
            (vec!["claude".to_string(), "fix it".to_string()], None),
            "a short first message rides as itself"
        );

        // A daemon frame goes through whole.
        let request = Frame::Control(br#"{"type":"pane_list"}"#.to_vec());
        lock(match &host {
            Host::Remote(remote) => &remote.writer,
            Host::Local(_) => unreachable!(),
        })
        .write_all(&request.encode())
        .unwrap();
        let mut got = Vec::new();
        each_frame(&mut &daemon_side, |_, frame| {
            got.push(frame);
            Err(anyhow!("one is enough"))
        })
        .unwrap_err();
        assert_eq!(got, [request]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
