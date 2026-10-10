//! Issue trackers: an `[[issues]]` profile names somewhere issues come from —
//! GitHub through the user's own `gh`, Linear's GraphQL API through `curl`
//! with a personal API key from the environment, or any tracker through a
//! command printing JSON — and deploying one launches an agent on it with the
//! issue as its prompt. Every fetch runs client-side, in the project's
//! directory with the user's environment: the daemon never talks to a
//! tracker, and nothing here writes to one.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tutti_core::IssueRef;

/// The prompt a deployed agent gets unless its source names another.
pub const DEFAULT_PROMPT: &str =
    "Work on this issue and commit your changes when done.\n\n{id}: {title}\n\n{body}\n\n{url}";

const LINEAR_API: &str = "https://api.linear.app/graphql";
const LINEAR_LIST: &str = "query($filter: IssueFilter, $first: Int) { issues(filter: $filter, \
     first: $first, orderBy: updatedAt) { nodes { identifier title description url labels { \
     nodes { name } } } } }";
const LINEAR_ISSUE: &str = "query($id: String!) { issue(id: $id) { identifier title description \
     url labels { nodes { name } } } }";
const GH_FIELDS: &str = "number,title,body,url,labels";

/// One configured tracker: where its issues come from, and the prompt an
/// agent deployed on one gets.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// What the issues panel and `--source` call it, `[A-Za-z0-9_-]+`.
    pub name: String,
    pub kind: Kind,
    /// The deployed agent's first message, with `{id}`, `{title}`, `{body}`
    /// and `{url}` filled in.
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// GitHub through `gh`: `repo` (`owner/name`), else the one the project's
    /// git remote names, narrowed by a `gh` `search`.
    Github {
        repo: Option<String>,
        search: Option<String>,
        limit: u32,
    },
    /// Linear's API with the personal API key in the environment variable
    /// `api_key_env`: issues matching `filter` (an `IssueFilter`), else the
    /// viewer's assigned ones not yet done — within team `team` when set.
    Linear {
        api_key_env: String,
        team: Option<String>,
        filter: Option<Map<String, Value>>,
        limit: u32,
    },
    /// Any tracker: this argv prints a JSON array of `{id, title, body?,
    /// url?, labels?}`.
    Command(Vec<String>),
}

/// One issue as its source lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// The jj workspace an issue deploys into: its id lowercased, every other
/// character a `-` (`ENG-42` → `eng-42`) — and, for an id that does not lead
/// with a letter, its source's name in front (`gh-123`), since a bare number
/// names nothing.
pub fn workspace_name(issue: &Issue) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                'a'..='z' | '0'..='9' | '_' | '-' => c,
                'A'..='Z' => c.to_ascii_lowercase(),
                _ => '-',
            })
            .collect()
    };
    if issue.id.starts_with(|c: char| c.is_ascii_alphabetic()) {
        clean(&issue.id)
    } else {
        format!("{}-{}", clean(&issue.source), clean(&issue.id))
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

    #[test]
    fn workspace_names_derive_from_the_id() {
        assert_eq!(workspace_name(&issue("gh", "123")), "gh-123");
        assert_eq!(workspace_name(&issue("linear", "ENG-42")), "eng-42");
        assert_eq!(workspace_name(&issue("jira", "PROJ 7/x")), "proj-7-x");
        assert_eq!(workspace_name(&issue("Work_GH", "9")), "work_gh-9");
    }

    #[test]
    fn a_bare_number_reads_as_a_github_issue() {
        assert_eq!(label("123"), "#123");
        assert_eq!(label("ENG-42"), "ENG-42");
    }
}
