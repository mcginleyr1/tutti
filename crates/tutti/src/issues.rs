//! Issue trackers: an `[[issues]]` profile names somewhere issues come from —
//! GitHub through the user's own `gh`, Linear's GraphQL API through `curl`
//! with a personal API key from the environment, or any tracker through a
//! command printing JSON — and deploying one launches an agent on it with the
//! issue as its prompt. Every tracker call runs in a tutti client process on
//! the daemon's machine — the TUI's own, or the `tutti proxy` a remote attach
//! runs — in the project's directory with that user's environment: the daemon
//! never talks to a tracker. A source that opts in hears back from a deploy
//! (`write_back`).

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tutti_agents::AgentSpec;
use tutti_core::{IssueRef, PaneId};

/// The prompt a deployed agent gets unless its source names another.
pub const DEFAULT_PROMPT: &str =
    "Work on this issue and commit your changes when done.\n\n{id}: {title}\n\n{body}\n\n{url}";

const LINEAR_API: &str = "https://api.linear.app/graphql";
const LINEAR_LIST: &str = "query($filter: IssueFilter, $first: Int) { issues(filter: $filter, \
     first: $first, orderBy: updatedAt) { nodes { identifier title description url labels { \
     nodes { name } } } } }";
const LINEAR_ISSUE: &str = "query($id: String!) { issue(id: $id) { identifier title description \
     url labels { nodes { name } } } }";
const LINEAR_COMMENT: &str = "mutation($id: String!, $body: String!) { commentCreate(input: { \
     issueId: $id, body: $body }) { success } }";
const LINEAR_STATES: &str =
    "query($id: String!) { issue(id: $id) { id team { states { nodes { id name } } } } }";
const LINEAR_MOVE: &str = "mutation($id: String!, $stateId: String!) { issueUpdate(id: $id, \
     input: { stateId: $stateId }) { success } }";
const GH_FIELDS: &str = "number,title,body,url,labels";

/// One configured tracker: where its issues come from, the prompt an agent
/// deployed on one gets, and whether a deploy comments on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// What the issues panel and `--source` call it, `[A-Za-z0-9_-]+`.
    pub name: String,
    pub kind: Kind,
    /// The deployed agent's first message, with `{id}`, `{title}`, `{body}`
    /// and `{url}` filled in.
    pub prompt: String,
    /// Whether a deploy comments on its issue where the agent runs
    /// (`deployed_comment`). Never on a command source.
    pub comment: bool,
}

impl Source {
    /// Whether a deploy from here writes anything back (`write_back`).
    pub fn writes_back(&self) -> bool {
        self.comment
            || matches!(
                &self.kind,
                Kind::Github {
                    assign: Some(_),
                    ..
                } | Kind::Github { label: Some(_), .. }
                    | Kind::Linear { state: Some(_), .. }
            )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    /// GitHub through `gh`: `repo` (`owner/name`), else the one the project's
    /// git remote names, narrowed by a `gh` `search`. A deploy assigns the
    /// issue to `assign` (a login, `@me`) and adds `label`, when set.
    Github {
        repo: Option<String>,
        search: Option<String>,
        limit: u32,
        assign: Option<String>,
        label: Option<String>,
    },
    /// Linear's API with the personal API key in the environment variable
    /// `api_key_env`: issues matching `filter` (an `IssueFilter`), else the
    /// viewer's assigned ones not yet done — within team `team` when set. A
    /// deploy moves the issue to its team's workflow state named `state`,
    /// when set.
    Linear {
        api_key_env: String,
        team: Option<String>,
        filter: Option<Map<String, Value>>,
        limit: u32,
        state: Option<String>,
    },
    /// Any tracker: this argv prints a JSON array of `{id, title, body?,
    /// url?, labels?}`.
    Command(Vec<String>),
}

/// One issue as its source lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// The configured source it came from.
    pub source: String,
    /// Its id there: a GitHub number, a Linear identifier, whatever a
    /// command printed.
    pub id: String,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub labels: Vec<String>,
}

impl Issue {
    /// The link a pane deployed on this issue carries back to it.
    pub fn link(&self) -> IssueRef {
        IssueRef {
            source: self.source.clone(),
            id: self.id.clone(),
            title: self.title.clone(),
            url: self.url.clone(),
        }
    }
}

/// The open issues `source` lists, fetched in the project directory `dir`.
pub fn list(source: &Source, dir: &Path) -> Result<Vec<Issue>> {
    match &source.kind {
        Kind::Github {
            repo,
            search,
            limit,
            ..
        } => {
            let limit = limit.to_string();
            let args = [
                "issue",
                "list",
                "--json",
                GH_FIELDS,
                "--limit",
                limit.as_str(),
            ];
            let mut gh = gh(&args, repo.as_deref(), dir);
            if let Some(search) = search {
                gh.args(["--search", search]);
            }
            let issues: Vec<GhIssue> =
                serde_json::from_str(&capture(&mut gh, None)?).context("parsing gh's issues")?;
            Ok(issues
                .into_iter()
                .map(|i| i.into_issue(&source.name))
                .collect())
        }
        Kind::Linear {
            api_key_env,
            team,
            filter,
            limit,
            ..
        } => {
            let variables = json!({
                "filter": linear_filter(filter.as_ref(), team.as_deref()),
                "first": limit,
            });
            let data: Value = linear(api_key_env, LINEAR_LIST, variables)?;
            let issues: Vec<LinearIssue> = serde_json::from_value(data["issues"]["nodes"].clone())
                .context("parsing Linear's issues")?;
            Ok(issues
                .into_iter()
                .map(|i| i.into_issue(&source.name))
                .collect())
        }
        Kind::Command(argv) => {
            let (program, args) = argv.split_first().context("an empty command")?;
            let out = capture(Command::new(program).args(args).current_dir(dir), None)?;
            parse_command(&source.name, &out)
        }
    }
}

/// The issue `id` of `source`. A command source has no way to ask for one,
/// so it is looked up in the list.
pub fn fetch(source: &Source, id: &str, dir: &Path) -> Result<Issue> {
    match &source.kind {
        Kind::Github { repo, .. } => {
            let args = [
                "issue",
                "view",
                id.trim_start_matches('#'),
                "--json",
                GH_FIELDS,
            ];
            let json = capture(&mut gh(&args, repo.as_deref(), dir), None)?;
            let issue: GhIssue = serde_json::from_str(&json).context("parsing gh's issue")?;
            Ok(issue.into_issue(&source.name))
        }
        Kind::Linear { api_key_env, .. } => {
            let data: Value = linear(api_key_env, LINEAR_ISSUE, json!({ "id": id }))?;
            let issue: LinearIssue =
                serde_json::from_value(data["issue"].clone()).context("parsing Linear's issue")?;
            Ok(issue.into_issue(&source.name))
        }
        Kind::Command(_) => list(source, dir)?
            .into_iter()
            .find(|issue| issue.id == id)
            .with_context(|| format!("{} lists no issue {id}", source.name)),
    }
}

/// Tell issue `id` of `source` it was deployed, as far as the source opts in:
/// leave `comment` on it, then on GitHub assign and label it, on Linear move
/// it to a workflow state. Runs in the project directory `dir`, as a fetch
/// does; the first refusal ends it.
pub fn write_back(source: &Source, id: &str, comment: &str, dir: &Path) -> Result<()> {
    match &source.kind {
        Kind::Github {
            repo,
            assign,
            label,
            ..
        } => {
            if source.comment {
                let args = ["issue", "comment", id, "--body", comment];
                capture(&mut gh(&args, repo.as_deref(), dir), None)?;
            }
            let mut edit = vec!["issue", "edit", id];
            if let Some(assign) = assign {
                edit.extend(["--add-assignee", assign]);
            }
            if let Some(label) = label {
                edit.extend(["--add-label", label]);
            }
            if edit.len() > 3 {
                capture(&mut gh(&edit, repo.as_deref(), dir), None)?;
            }
        }
        Kind::Linear {
            api_key_env, state, ..
        } => {
            if source.comment {
                let data = linear(
                    api_key_env,
                    LINEAR_COMMENT,
                    json!({"id": id, "body": comment}),
                )?;
                if data["commentCreate"]["success"] != true {
                    bail!("Linear did not comment on {id}");
                }
            }
            if let Some(state) = state {
                #[derive(Deserialize)]
                struct State {
                    id: String,
                    name: String,
                }
                let data = linear(api_key_env, LINEAR_STATES, json!({ "id": id }))?;
                let issue = &data["issue"];
                let states: Vec<State> =
                    serde_json::from_value(issue["team"]["states"]["nodes"].clone())
                        .context("parsing Linear's workflow states")?;
                let to = states.iter().find(|s| s.name == *state).with_context(|| {
                    let names: Vec<&str> = states.iter().map(|s| s.name.as_str()).collect();
                    format!(
                        "{id}'s team has no state {state:?} (it has {})",
                        names.join(", ")
                    )
                })?;
                let moved = json!({"id": issue["id"], "stateId": to.id});
                let data = linear(api_key_env, LINEAR_MOVE, moved)?;
                if data["issueUpdate"]["success"] != true {
                    bail!("Linear did not move {id} to {state:?}");
                }
            }
        }
        Kind::Command(_) => {}
    }
    Ok(())
}

/// The comment a deploy leaves on its issue: which agent took it and where
/// it runs — the workspace made for it in `project`, or the project itself
/// when deployed there; the pane and session `tutti pane focus` finds it
/// by; and the branch a sandbox pushes its work to.
pub fn deployed_comment(
    agent: &str,
    project: &str,
    workspace: Option<&str>,
    pane: PaneId,
    session: &str,
    branch: Option<&str>,
) -> String {
    let place = match workspace {
        Some(workspace) => format!("workspace `{workspace}` of project `{project}`"),
        None => format!("project `{project}`"),
    };
    let branch = branch.map_or(String::new(), |b| format!(", pushing to branch `{b}`"));
    format!(
        "tutti deployed {agent} on this issue: {place}, pane {pane} of session `{session}`{branch}."
    )
}

/// The longest argument Linux's exec takes, its NUL included (MAX_ARG_STRLEN,
/// 32 pages). A first message that long rides a file instead.
pub const MAX_ARG: usize = 128 * 1024;

/// From this long on, a first message goes through the agent's own file
/// input where it has one (`AgentSpec::prompt_file`). An eighth of `MAX_ARG`,
/// so an argument this size is far from both Linux's limit per argument and
/// macOS's 1 MiB for the argv and environment together; but past it — some
/// 300 lines — a message is a document, which an agent that reads one from a
/// file gets as one, and which then stays out of `ps` and out of the layout
/// the daemon saves with each pane's argv.
pub const FILE_PROMPT: usize = 16 * 1024;

/// `argv`, the agent's own (`AgentSpec::argv`), handed `prompt` as its first
/// message. One of `FILE_PROMPT` or more goes through the agent's own file
/// input where it has one; one too long for an argument (`MAX_ARG`) goes to a
/// file a short message names, for the agent to read as it reads any file.
/// Either file is written 0600 into `files`, the session's private prompt
/// directory (`tutti_core::prompts_dir`), and returned beside the argv: the
/// caller removes it should no pane end up with it (a fork or run refused,
/// the daemon gone); once a run lands, the daemon removes it as the agent
/// exits. A sandbox (`files`
/// `None`) can't read this machine's files: there a prompt rides as itself,
/// and one too long for that is refused.
pub fn deploy_argv(
    spec: &AgentSpec,
    argv: Vec<String>,
    prompt: &str,
    files: Option<&Path>,
) -> Result<(Vec<String>, Option<PathBuf>)> {
    let refuse = || format!("{} takes no first message on its command line", spec.kind);
    let inline = spec.prompt_args(prompt).with_context(refuse)?;
    let (args, file) = match files {
        Some(dir) if prompt.len() >= FILE_PROMPT && spec.prompt_file.is_some() => {
            let file = write_prompt(dir, prompt)?;
            (spec.prompt_file_args(&file), Some(file))
        }
        Some(dir) if prompt.len() >= MAX_ARG => {
            let file = write_prompt(dir, prompt)?;
            let message = format!(
                "Your task is too long for a command line, so it is in the file {}. Read that file and do what it says.",
                file.display()
            );
            (spec.prompt_args(&message), Some(file))
        }
        None if prompt.len() >= MAX_ARG => bail!(
            "the issue's prompt is {} bytes, too long for a command line ({MAX_ARG}), and a sandbox can't read it from a file here",
            prompt.len()
        ),
        _ => (Some(inline), None),
    };
    Ok(([argv, args.with_context(refuse)?].concat(), file))
}

/// `prompt` in a fresh file of `dir`, which is made private to the user
/// (0700) if it is new; the file only the user can read (0600). A file the
/// prompt could not be written into whole is removed, never left half full.
fn write_prompt(dir: &Path, prompt: &str) -> Result<PathBuf> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = dir.join(format!("prompt-{}-{nanos}.md", std::process::id()));
    let writing = || format!("writing the prompt to {}", path.display());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(writing)?;
    if let Err(e) = file.write_all(prompt.as_bytes()) {
        let _ = std::fs::remove_file(&path);
        return Err(e).with_context(writing);
    }
    Ok(path)
}

/// `template` with `{id}`, `{title}`, `{body}` and `{url}` filled in from
/// `issue` in one pass — so a title that happens to say `{body}` stays as
/// written — and trimmed.
pub fn prompt(template: &str, issue: &Issue) -> String {
    let fields = [
        ("{id}", issue.id.as_str()),
        ("{title}", issue.title.as_str()),
        ("{body}", issue.body.as_str()),
        ("{url}", issue.url.as_deref().unwrap_or("")),
    ];
    let mut out = String::new();
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        match fields.iter().find(|(key, _)| rest.starts_with(key)) {
            Some((key, value)) => {
                out.push_str(value);
                rest = &rest[key.len()..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out.trim().to_string()
}

/// The jj workspace issue `id` of `source` deploys into: the id lowercased,
/// every other character a `-` (`ENG-42` → `eng-42`) — and, for an id that
/// does not lead with a letter, the source's name in front (`gh-123`), since a
/// bare number names nothing.
pub fn workspace_name(source: &str, id: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                'a'..='z' | '0'..='9' | '_' | '-' => c,
                'A'..='Z' => c.to_ascii_lowercase(),
                _ => '-',
            })
            .collect()
    };
    if id.starts_with(|c: char| c.is_ascii_alphabetic()) {
        clean(id)
    } else {
        format!("{}-{}", clean(source), clean(id))
    }
}

/// How an issue id reads on screen: GitHub's `#123` for a bare number, any
/// other id as it is (`ENG-42`).
pub fn label(id: &str) -> String {
    if id.bytes().all(|b| b.is_ascii_digit()) {
        format!("#{id}")
    } else {
        id.to_string()
    }
}

/// An issue as `gh issue list/view --json` prints it.
#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    url: String,
    #[serde(default)]
    labels: Vec<Label>,
}

impl GhIssue {
    fn into_issue(self, source: &str) -> Issue {
        Issue {
            source: source.into(),
            id: self.number.to_string(),
            title: self.title,
            body: self.body,
            url: Some(self.url),
            labels: self.labels.into_iter().map(|l| l.name).collect(),
        }
    }
}

/// An issue as Linear's API answers it.
#[derive(Deserialize)]
struct LinearIssue {
    identifier: String,
    title: String,
    description: Option<String>,
    url: String,
    labels: Nodes<Label>,
}

impl LinearIssue {
    fn into_issue(self, source: &str) -> Issue {
        Issue {
            source: source.into(),
            id: self.identifier,
            title: self.title,
            body: self.description.unwrap_or_default(),
            url: Some(self.url),
            labels: self.labels.nodes.into_iter().map(|l| l.name).collect(),
        }
    }
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

/// The issues in a command source's output: a JSON array of `{id, title,
/// body?, url?, labels?}`, the id a string or a number.
fn parse_command(source: &str, json: &str) -> Result<Vec<Issue>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Text(String),
        Number(u64),
    }
    #[derive(Deserialize)]
    struct Printed {
        id: Id,
        title: String,
        body: Option<String>,
        url: Option<String>,
        #[serde(default)]
        labels: Vec<String>,
    }
    let printed: Vec<Printed> = serde_json::from_str(json)
        .with_context(|| format!("{source} printed no JSON array of issues"))?;
    Ok(printed
        .into_iter()
        .map(|p| Issue {
            source: source.into(),
            id: match p.id {
                Id::Text(id) => id,
                Id::Number(n) => n.to_string(),
            },
            title: p.title,
            body: p.body.unwrap_or_default(),
            url: p.url,
            labels: p.labels,
        })
        .collect())
}

/// The `IssueFilter` a Linear source lists by: its own `filter`, else the
/// viewer's assigned issues not yet done — within team `team` when set.
fn linear_filter(filter: Option<&Map<String, Value>>, team: Option<&str>) -> Value {
    let mut filter = filter.map_or_else(
        || {
            json!({
                "assignee": {"isMe": {"eq": true}},
                "state": {"type": {"nin": ["completed", "canceled", "duplicate"]}},
            })
        },
        |filter| Value::Object(filter.clone()),
    );
    if let Some(team) = team {
        filter["team"] = json!({"key": {"eq": team}});
    }
    filter
}

/// Ask Linear's API one GraphQL `query` with the personal API key from the
/// environment variable `key_env`, answering the reply's `data`. The key
/// reaches `curl` on stdin as part of its config, never on a command line.
fn linear(key_env: &str, query: &str, variables: Value) -> Result<Value> {
    let key = std::env::var(key_env)
        .ok()
        .with_context(|| format!("{key_env} is not set (a Linear personal API key)"))?;
    let body = json!({"query": query, "variables": variables}).to_string();
    let config = format!(
        "header = \"Content-Type: application/json\"\nheader = \"Authorization: {}\"\ndata = \"{}\"\n",
        curl_quote(key.trim()),
        curl_quote(&body)
    );
    let mut curl = Command::new("curl");
    curl.args(["--silent", "--show-error", "--max-time", "30"])
        .args(["--config", "-", LINEAR_API]);
    linear_data(&capture(&mut curl, Some(config.as_bytes()))?)
}

/// A Linear reply's `data`, or its first error.
fn linear_data(reply: &str) -> Result<Value> {
    #[derive(Deserialize)]
    struct Reply {
        data: Option<Value>,
        #[serde(default)]
        errors: Vec<Message>,
    }
    #[derive(Deserialize)]
    struct Message {
        message: String,
    }
    let reply: Reply = serde_json::from_str(reply)
        .with_context(|| format!("unexpected reply from Linear: {}", reply.trim()))?;
    if let Some(error) = reply.errors.first() {
        bail!("Linear: {}", error.message);
    }
    reply.data.context("Linear answered no data")
}

/// `s` escaped for a double-quoted `curl --config` value.
fn curl_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `gh` running `args` in the project directory `dir`, so it infers the
/// repository from the checkout's git remote unless `repo` names one.
fn gh(args: &[&str], repo: Option<&str>, dir: &Path) -> Command {
    let mut gh = Command::new("gh");
    gh.args(args).current_dir(dir);
    if let Some(repo) = repo {
        gh.args(["--repo", repo]);
    }
    gh
}

/// Run `cmd` to completion, feeding it `stdin`: its stdout on success, its
/// stderr as the error otherwise.
fn capture(cmd: &mut Command, stdin: Option<&[u8]>) -> Result<String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program} (is it on PATH?)"))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(bytes)
            .with_context(|| format!("writing to {program}"))?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for {program}"))?;
    if !output.status.success() {
        bail!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(source: &str, id: &str) -> Issue {
        Issue {
            source: source.into(),
            id: id.into(),
            title: "Fix the login redirect".into(),
            body: "It loops back to /login.".into(),
            url: Some("https://example.com/issues/1".into()),
            labels: Vec::new(),
        }
    }

    fn command_source(script: &str) -> Source {
        Source {
            name: "jira".into(),
            kind: Kind::Command(vec!["/bin/sh".into(), "-c".into(), script.into()]),
            prompt: DEFAULT_PROMPT.into(),
            comment: false,
        }
    }

    /// A unique scratch directory named `jira-<label>-…`.
    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jira-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn gh_output_parses_into_issues() {
        // Trimmed from `gh issue list --json number,title,body,url,labels`.
        let json = r#"[
            {"body":"Steps:\n1. log in","labels":[{"id":"LA_1","name":"bug","description":"","color":"d73a4a"}],
             "number":123,"title":"Login loops","url":"https://github.com/acme/app/issues/123"},
            {"body":"","labels":[],"number":120,"title":"Docs typo","url":"https://github.com/acme/app/issues/120"}
        ]"#;
        let issues: Vec<GhIssue> = serde_json::from_str(json).unwrap();
        let issues: Vec<Issue> = issues.into_iter().map(|i| i.into_issue("gh")).collect();
        assert_eq!(
            issues[0],
            Issue {
                source: "gh".into(),
                id: "123".into(),
                title: "Login loops".into(),
                body: "Steps:\n1. log in".into(),
                url: Some("https://github.com/acme/app/issues/123".into()),
                labels: vec!["bug".into()],
            }
        );
        assert_eq!(
            (issues[1].id.as_str(), issues[1].body.as_str()),
            ("120", "")
        );
    }

    #[test]
    fn linear_replies_parse_into_issues_or_their_error() {
        let list = r#"{"data":{"issues":{"nodes":[
            {"identifier":"ENG-42","title":"Fix the login redirect","description":null,
             "url":"https://linear.app/acme/issue/ENG-42/fix-the-login-redirect",
             "labels":{"nodes":[{"name":"Bug"},{"name":"Auth"}]}}
        ]}}}"#;
        let data = linear_data(list).unwrap();
        let issues: Vec<LinearIssue> =
            serde_json::from_value(data["issues"]["nodes"].clone()).unwrap();
        let issue = issues.into_iter().next().unwrap().into_issue("linear");
        assert_eq!(
            issue,
            Issue {
                source: "linear".into(),
                id: "ENG-42".into(),
                title: "Fix the login redirect".into(),
                body: String::new(),
                url: Some("https://linear.app/acme/issue/ENG-42/fix-the-login-redirect".into()),
                labels: vec!["Bug".into(), "Auth".into()],
            }
        );

        let one = r#"{"data":{"issue":{"identifier":"ENG-7","title":"t","description":"d",
            "url":"https://linear.app/acme/issue/ENG-7/t","labels":{"nodes":[]}}}}"#;
        let issue: LinearIssue =
            serde_json::from_value(linear_data(one).unwrap()["issue"].clone()).unwrap();
        assert_eq!(issue.into_issue("linear").body, "d");

        let refused = r#"{"errors":[{"message":"Authentication required, not authenticated",
            "extensions":{"code":"AUTHENTICATION_ERROR"}}],"data":null}"#;
        assert_eq!(
            linear_data(refused).unwrap_err().to_string(),
            "Linear: Authentication required, not authenticated"
        );
        let html = linear_data("<html>502 Bad Gateway</html>").unwrap_err();
        assert!(format!("{html:#}").contains("unexpected reply from Linear: <html>"));
    }

    #[test]
    fn the_linear_filter_defaults_to_my_open_issues_within_a_team() {
        assert_eq!(
            linear_filter(None, None),
            json!({
                "assignee": {"isMe": {"eq": true}},
                "state": {"type": {"nin": ["completed", "canceled", "duplicate"]}},
            })
        );
        assert_eq!(
            linear_filter(None, Some("ENG"))["team"],
            json!({"key": {"eq": "ENG"}})
        );
        let mine = json!({"priority": {"lte": 2}});
        assert_eq!(
            linear_filter(mine.as_object(), Some("ENG")),
            json!({"priority": {"lte": 2}, "team": {"key": {"eq": "ENG"}}}),
            "a configured filter replaces the default"
        );
    }

    #[test]
    fn curl_config_values_escape_quotes_and_backslashes() {
        assert_eq!(curl_quote(r#"{"q":"a\nb"}"#), r#"{\"q\":\"a\\nb\"}"#);
    }

    #[test]
    fn command_output_parses_string_and_number_ids() {
        let json = r#"[
            {"id":"PROJ-7","title":"Fix it","body":"why","url":"https://jira.example/PROJ-7","labels":["p1"]},
            {"id":8,"title":"Numbered","body":null}
        ]"#;
        let issues = parse_command("jira", json).unwrap();
        assert_eq!(issues[0].id, "PROJ-7");
        assert_eq!(issues[0].labels, ["p1"]);
        assert_eq!(
            issues[1],
            Issue {
                source: "jira".into(),
                id: "8".into(),
                title: "Numbered".into(),
                body: String::new(),
                url: None,
                labels: Vec::new(),
            }
        );
        let err = parse_command("jira", r#"{"id":1}"#).unwrap_err();
        assert!(format!("{err:#}").contains("jira printed no JSON array of issues"));
    }

    /// A command source runs in the project directory, and its failure is its
    /// stderr; deploying from one looks the id up in what it lists.
    #[test]
    fn a_command_source_runs_in_the_project_dir() {
        let dir = scratch("cwd");
        let listing = command_source(
            r#"printf '[{"id":"%s","title":"from the project dir"}]' "$(basename "$(pwd)")""#,
        );
        let issues = list(&listing, &dir).unwrap();
        let here = dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(issues[0].id, here);
        assert_eq!(
            fetch(&listing, here, &dir).unwrap().title,
            "from the project dir"
        );
        let missing = fetch(&listing, "PROJ-1", &dir).unwrap_err();
        assert_eq!(missing.to_string(), "jira lists no issue PROJ-1");

        let failing = command_source("echo 'jira: not logged in' >&2; exit 3");
        let err = list(&failing, &dir).unwrap_err();
        assert_eq!(err.to_string(), "/bin/sh: jira: not logged in");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_prompt_fills_each_field_once() {
        let mut deployed = issue("linear", "ENG-42");
        assert_eq!(
            prompt(DEFAULT_PROMPT, &deployed),
            "Work on this issue and commit your changes when done.\n\n\
             ENG-42: Fix the login redirect\n\nIt loops back to /login.\n\n\
             https://example.com/issues/1"
        );
        deployed.title = "Render {body} literally".into();
        deployed.url = None;
        assert_eq!(
            prompt("{title} — {body} {url}", &deployed),
            "Render {body} literally — It loops back to /login.",
            "a field's own braces are never filled, and an empty end is trimmed"
        );
        assert_eq!(
            prompt("fix {id}: {unknown}", &deployed),
            "fix ENG-42: {unknown}"
        );
    }

    /// A first message rides as itself until it is long. Then an agent with
    /// a file input of its own reads it through that; any other gets a file
    /// named by a short message once it is too long for one argument. Each
    /// file only the user can read, in the private prompt directory. A
    /// sandbox, which can't read this machine's files, takes what fits and
    /// refuses the rest.
    #[test]
    fn a_long_first_message_rides_the_agents_own_file_input_or_a_named_file() {
        use std::os::unix::fs::PermissionsExt;
        use tutti_agents::{ATTACHED_TASK, Registry};
        let registry = Registry::default();
        let base = scratch("prompts");
        let dir = base.join("t.prompts");
        // The file returned is the one the argv names, and none when it
        // names none.
        let deploy = |kind: &str, prompt: &str, files: Option<&Path>| {
            let spec = registry.spec(&kind.into()).unwrap();
            deploy_argv(spec, spec.argv(None), prompt, files).map(|(argv, file)| {
                let names = |path: &Path| argv.iter().any(|a| a.contains(&*path.to_string_lossy()));
                assert_eq!(file.as_deref().is_some_and(names), names(&dir), "{argv:?}");
                argv
            })
        };
        let read = |file: &str| {
            let file = Path::new(file.strip_prefix('@').unwrap_or(file));
            assert_eq!(file.parent(), Some(&*dir), "{file:?}");
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            std::fs::read_to_string(file).unwrap()
        };

        let short = "x".repeat(FILE_PROMPT - 1);
        for kind in ["claude", "ori", "pi"] {
            let argv = deploy(kind, &short, Some(&dir)).unwrap();
            assert_eq!(argv.last(), Some(&short), "{kind}");
        }
        assert!(!dir.exists(), "nothing written");

        let long = "é".repeat(FILE_PROMPT / 2);
        let ori = deploy("ori", &long, Some(&dir)).unwrap();
        assert_eq!(ori[..4], ["ori", "code", "--interactive", "--prompt-file"]);
        assert_eq!(read(&ori[4]), long);
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let goose = deploy("goose", &long, Some(&dir)).unwrap();
        assert_eq!(
            goose[..4],
            ["goose", "run", "--interactive", "--instructions"]
        );
        assert_eq!(read(&goose[4]), long);
        for kind in ["pi", "omp", "auggie"] {
            let argv = deploy(kind, &long, Some(&dir)).unwrap();
            assert_eq!((argv.len(), argv[2].as_str()), (3, ATTACHED_TASK), "{kind}");
            assert!(argv[1].starts_with('@'), "{argv:?}");
            assert_eq!(read(&argv[1]), long);
        }
        let claude = deploy("claude", &long, Some(&dir)).unwrap();
        assert_eq!(claude, ["claude", long.as_str()], "fits one argument");

        let huge = "é".repeat(MAX_ARG / 2);
        let claude = deploy("claude", &huge, Some(&dir)).unwrap();
        assert!(claude[1].len() < 1024, "{claude:?}");
        let named = claude[1]
            .split("in the file ")
            .nth(1)
            .and_then(|rest| rest.split(". Read").next())
            .unwrap();
        assert_eq!(read(named), huge);

        let sandboxed = deploy("ori", &long, None).unwrap();
        assert_eq!(sandboxed.last(), Some(&long), "a sandbox takes what fits");
        let refused = deploy("ori", &huge, None).unwrap_err().to_string();
        assert!(refused.contains("too long for a command line"), "{refused}");
        let crush = deploy("crush", &huge, Some(&base.join("none"))).unwrap_err();
        assert!(
            crush.to_string().contains("takes no first message"),
            "{crush}"
        );
        assert!(!base.join("none").exists(), "refused before writing");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Run by `a_prompt_file_written_short_is_removed` under a one-block
    /// file-size limit, never on its own: a prompt past the limit fails part
    /// way, and no file of it is left.
    #[test]
    #[ignore = "run by a_prompt_file_written_short_is_removed, under a file-size limit"]
    fn write_a_prompt_past_the_file_size_limit() {
        let Some(dir) = std::env::var_os("TUTTI_PROMPTS_UNDER_LIMIT") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let failed = write_prompt(&dir, &"x".repeat(64 * 1024)).unwrap_err();
        assert!(
            format!("{failed:#}").contains("writing the prompt"),
            "{failed:#}"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "a partial file"
        );
    }

    /// A prompt file that can't be written whole — a full disk, a quota — is
    /// removed, never left half written. The write fails for real: this test
    /// runs the one above in a child under `ulimit -f 1` with SIGXFSZ
    /// ignored, so writing past the limit returns EFBIG.
    #[test]
    fn a_prompt_file_written_short_is_removed() {
        let dir = scratch("short");
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                "trap '' XFSZ; ulimit -f 1; exec \"$0\" --exact --ignored \
                 issues::tests::write_a_prompt_past_the_file_size_limit",
            ])
            .arg(std::env::current_exe().unwrap())
            .env("TUTTI_PROMPTS_UNDER_LIMIT", dir.join("t.prompts"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_deploy_comment_names_where_its_agent_runs() {
        assert_eq!(
            deployed_comment("claude", "api", Some("gh-123"), PaneId(7), "tutti", None),
            "tutti deployed claude on this issue: workspace `gh-123` of project `api`, \
             pane 7 of session `tutti`."
        );
        assert_eq!(
            deployed_comment(
                "codex",
                "api",
                None,
                PaneId(3),
                "work",
                Some("tutti/codex-3")
            ),
            "tutti deployed codex on this issue: project `api`, pane 3 of session `work`, \
             pushing to branch `tutti/codex-3`."
        );
    }

    /// Write-back is opt-in, piece by piece: a comment, a GitHub assignee or
    /// label, a Linear state. A source asking none writes nothing.
    #[test]
    fn only_a_source_that_opts_in_writes_back() {
        let github = |assign: Option<&str>, label: Option<&str>| Kind::Github {
            repo: None,
            search: None,
            limit: 50,
            assign: assign.map(String::from),
            label: label.map(String::from),
        };
        let linear = |state: Option<&str>| Kind::Linear {
            api_key_env: "LINEAR_API_KEY".into(),
            team: None,
            filter: None,
            limit: 50,
            state: state.map(String::from),
        };
        let source = |kind: Kind, comment: bool| Source {
            kind,
            comment,
            ..command_source("true")
        };
        for (kind, writes) in [
            (github(None, None), false),
            (github(Some("@me"), None), true),
            (github(None, Some("agent")), true),
            (linear(None), false),
            (linear(Some("In Progress")), true),
        ] {
            assert_eq!(
                source(kind.clone(), false).writes_back(),
                writes,
                "{kind:?}"
            );
            assert!(source(kind, true).writes_back(), "a comment writes back");
        }
        let quiet = source(github(None, None), false);
        write_back(&quiet, "1", "never sent", Path::new("/nonexistent")).unwrap();
    }

    #[test]
    fn workspace_names_derive_from_the_id() {
        assert_eq!(workspace_name("gh", "123"), "gh-123");
        assert_eq!(workspace_name("linear", "ENG-42"), "eng-42");
        assert_eq!(workspace_name("jira", "PROJ 7/x"), "proj-7-x");
        assert_eq!(workspace_name("Work_GH", "9"), "work_gh-9");
    }

    #[test]
    fn a_bare_number_reads_as_a_github_issue() {
        assert_eq!(label("123"), "#123");
        assert_eq!(label("ENG-42"), "ENG-42");
    }
}
