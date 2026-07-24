//! jj (Jujutsu) is Tutti's required VCS for workspace-level features — no
//! git/hg adapters, by decree. This module is the only place that shells out to
//! it: detecting a jj workspace, serving per-workspace diffs, and parsing the
//! one-line change stat that drives the sidebar meta.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tutti_core::Response;

/// How long a `jj` invocation may run before it is killed. A wedged repo or a
/// hung external diff tool must never stall a client or a background refresh.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A longer cap for `jj workspace add`: forking materializes a whole working
/// copy on disk, so it needs more headroom than a read-only probe.
const FORK_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on each `jj` invocation in the merge sequence — rebase, conflict probe,
/// bookmark move, optional push — each of which can touch the store or network.
const MERGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on lines returned to a client. A larger diff is truncated with a final
/// marker so the protocol frame — and the consumer rendering it — stay bounded.
const MAX_LINES: usize = 10_000;

/// Walk from `dir` upward looking for a `.jj` directory. A tutti workspace may
/// point inside a jj repo (a subdirectory of the working copy), so ancestors are
/// checked too. Returns the directory that holds `.jj`, or `None` when there is
/// none on the path to the filesystem root.
pub fn workspace_root(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".jj").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Whether `dir` is under a jj repo.
pub fn is_workspace(dir: &Path) -> bool {
    workspace_root(dir).is_some()
}

/// The sibling destination for a fork of the repo at `repo_root`, named `name`:
/// `<repo-parent>/<repo-basename>-<name>`. `None` when `repo_root` has no parent
/// or no file name (e.g. the filesystem root).
pub fn fork_dest(repo_root: &Path, name: &str) -> Option<PathBuf> {
    let parent = repo_root.parent()?;
    let base = repo_root.file_name()?.to_string_lossy();
    Some(parent.join(format!("{base}-{name}")))
}

/// Whether `name` is a valid fork name: one or more `[A-Za-z0-9_-]`. It becomes
/// both a path component and a jj workspace name, so anything else is rejected.
pub fn valid_fork_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Run `jj diff` (or `--stat`) in `dir` and shape it into a protocol `Response`.
/// The protocol path renders plain text, so `--color=never`. Bounded by a
/// timeout (the child is killed on expiry) and a line cap. A non-`.jj` directory
/// answers Error, matching the git_branch probe's neighbourly failure mode.
pub async fn diff(dir: &Path, stat: bool) -> Response {
    if !is_workspace(dir) {
        return Response::Error {
            message: format!("not a jj workspace: {}", dir.display()),
        };
    }
    let mut cmd = base_command(dir, stat);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = match run(cmd, TIMEOUT).await {
        Ok(output) => output,
        Err(message) => return Response::Error { message },
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = match stderr.trim() {
            "" => format!("jj diff exited with status {}", output.status),
            text => text.to_string(),
        };
        return Response::Error { message };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<String> = text
        .lines()
        .take(MAX_LINES + 1)
        .map(str::to_string)
        .collect();
    if lines.len() > MAX_LINES {
        lines.truncate(MAX_LINES);
        lines.push("… truncated".to_string());
    }
    Response::Content { lines }
}

/// The short change stat for `dir`, e.g. `4 files +120 −33`, or `None` for a
/// non-repo, a clean working copy, or any failure. This is the always-on
/// sidebar path, so it never surfaces an error — it just stays quiet.
pub async fn change_stat(dir: &Path) -> Option<String> {
    if !is_workspace(dir) {
        return None;
    }
    let mut cmd = base_command(dir, true);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let output = run(cmd, TIMEOUT).await.ok()?;
    if !output.status.success() {
        return None;
    }
    parse_stat(&String::from_utf8_lossy(&output.stdout))
}

/// Whether the jj workspace at `dir` has a stale working copy — its `@` was
/// rewritten from another workspace, so it needs `jj workspace update-stale`.
/// Probes with `jj log -r @`, which snapshots the working copy (the operation
/// that surfaces staleness) and prints a `stale` warning to stderr when it is.
/// A non-repo or any spawn failure reads as not-stale, so the sidebar stays
/// quiet. `--ignore-working-copy` is deliberately *not* passed: it would skip
/// the snapshot and hide the very condition being probed.
pub async fn is_stale(dir: &Path) -> bool {
    if !is_workspace(dir) {
        return false;
    }
    let mut cmd = Command::new("jj");
    cmd.arg("--no-pager")
        .arg("log")
        .arg("-r")
        .arg("@")
        .arg("--no-graph")
        .arg("-T")
        .arg("")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match run(cmd, TIMEOUT).await {
        Ok(output) => String::from_utf8_lossy(&output.stderr).contains("stale"),
        Err(_) => false,
    }
}

/// Fork the jj repo rooted at `repo_root` into a new workspace at `dest`, named
/// `name` (optionally checked out at `revision`). Runs `jj workspace add` from
/// the repo root. `Ok(())` on success; the jj stderr (or a spawn/timeout error)
/// otherwise. The caller has already verified `dest` does not exist.
pub async fn fork(
    repo_root: &Path,
    dest: &Path,
    name: &str,
    revision: Option<&str>,
) -> Result<(), String> {
    let mut cmd = Command::new("jj");
    cmd.arg("--no-pager")
        .arg("workspace")
        .arg("add")
        .arg("--name")
        .arg(name);
    if let Some(rev) = revision {
        cmd.arg("--revision").arg(rev);
    }
    cmd.arg(dest)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match run(cmd, FORK_TIMEOUT).await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(stderr_or(&output, "jj workspace add failed")),
        Err(message) => Err(message),
    }
}

/// Forget the workspace named `jj_name`, run at its origin repo `origin_root`
/// (`jj workspace forget` must run from a workspace that still exists, not the
/// one being removed). `Ok(())` on success; the jj stderr otherwise.
pub async fn forget(origin_root: &Path, jj_name: &str) -> Result<(), String> {
    let mut cmd = Command::new("jj");
    cmd.arg("--no-pager")
        .arg("workspace")
        .arg("forget")
        .arg(jj_name)
        .current_dir(origin_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match run(cmd, TIMEOUT).await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(stderr_or(&output, "jj workspace forget failed")),
        Err(message) => Err(message),
    }
}

/// Update a stale working copy in `dir` (`jj workspace update-stale`). Answered
/// as a protocol `Response`: `Ok` on success, `Error` for a non-repo, a jj
/// failure, or a spawn/timeout error.
pub async fn update_stale(dir: &Path) -> Response {
    if !is_workspace(dir) {
        return Response::Error {
            message: format!("not a jj workspace: {}", dir.display()),
        };
    }
    let mut cmd = Command::new("jj");
    cmd.arg("--no-pager")
        .arg("workspace")
        .arg("update-stale")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match run(cmd, TIMEOUT).await {
        Ok(output) if output.status.success() => Response::Ok,
        Ok(output) => Response::Error {
            message: stderr_or(&output, "jj workspace update-stale failed"),
        },
        Err(message) => Response::Error { message },
    }
}

/// The outcome of a successful `merge`: the trunk bookmark it advanced and
/// whether a push ran and succeeded. `push_error` carries the push stderr when a
/// push was attempted and failed — the merge itself still landed, so it is a note
/// (surfaced to the operator), never a failure.
pub struct MergeOutcome {
    pub bookmark: String,
    pub pushed: bool,
    pub push_error: Option<String>,
}

/// Merge a child jj-workspace's work back into its origin's trunk bookmark and,
/// when `push`, `jj git push` it if the origin has a remote. `origin_root` is the
/// origin repo the child was forked from; `child_dir` its checkout; `jj_name` its
/// `jj workspace add --name`. All subprocess, run from the origin root against
/// explicit revsets so no cwd juggling is needed:
///
/// 1. snapshot the child's working copy (so `<name>@` reflects its on-disk work);
/// 2. pick the trunk bookmark — `main`, else `master`, else Error;
/// 3. `jj rebase -b <name>@ -d <trunk>` to replay the child's branch onto trunk;
/// 4. if any rebased commit is conflicted, `jj undo` and Error — never land a
///    conflict;
/// 5. advance `<trunk>` to the child's real work: its `@` when non-empty, else its
///    parent `@-` (the `@` is usually an empty working-copy commit on top);
/// 6. optionally push.
///
/// `Ok(MergeOutcome)` on success; a jj stderr / spawn error otherwise.
pub async fn merge(
    origin_root: &Path,
    child_dir: &Path,
    jj_name: &str,
    push: bool,
) -> Result<MergeOutcome, String> {
    // Snapshot the child so `<name>@` reflects its latest on-disk work — jj only
    // records a working-copy commit when a command runs in that workspace.
    snapshot(child_dir).await;

    let trunk = trunk_bookmark(origin_root).await?;
    let child = format!("{jj_name}@");

    // Replay the child's branch onto the current trunk tip.
    run_mutation(origin_root, &["rebase", "-b", &child, "-d", &trunk]).await?;

    // A conflicted commit anywhere in the rebased range means the merge would
    // land a conflict: undo the rebase and refuse.
    let conflicts = log_template(
        origin_root,
        &format!("{trunk}..{child} & conflicts()"),
        "commit_id ++ \"\\n\"",
    )
    .await?;
    if !conflicts.trim().is_empty() {
        let _ = run_mutation(origin_root, &["undo"]).await;
        return Err("merge would conflict — resolve manually in the workspace".to_string());
    }

    // The child's `@` is typically an empty working-copy commit on top of the
    // real work; advance the bookmark to `@` only when it carries changes, else
    // to its parent `@-`.
    let empty = log_template(origin_root, &child, "empty").await?;
    let target = if empty.trim() == "true" {
        format!("{jj_name}@-")
    } else {
        child.clone()
    };
    run_mutation(origin_root, &["bookmark", "set", &trunk, "-r", &target]).await?;

    // Push only when asked and the origin actually has a remote; a push failure
    // is a note on an already-landed merge, never a rollback.
    let mut pushed = false;
    let mut push_error = None;
    if push && has_remote(origin_root).await {
        match run_mutation(origin_root, &["git", "push", "--bookmark", &trunk]).await {
            Ok(()) => pushed = true,
            Err(message) => push_error = Some(message),
        }
    }

    Ok(MergeOutcome {
        bookmark: trunk,
        pushed,
        push_error,
    })
}

/// Snapshot a workspace's working copy by running a no-op `jj log -r @` in it
/// (without `--ignore-working-copy`, which would skip the snapshot). Best-effort:
/// a failure just leaves the last recorded `@` in place.
async fn snapshot(dir: &Path) {
    let _ = run(
        jj_cmd(dir, &["log", "-r", "@", "--no-graph", "-T", ""]),
        MERGE_TIMEOUT,
    )
    .await;
}

/// The trunk bookmark to merge into: `main` if present, else `master`, else an
/// Error naming the local bookmarks that do exist (never guessing further).
/// Reads `jj bookmark list`, taking the name before the first `:`/space on each
/// line and dropping remote-tracking (`name@remote`) entries.
async fn trunk_bookmark(dir: &Path) -> Result<String, String> {
    let output = run(
        jj_cmd(dir, &["bookmark", "list", "--ignore-working-copy"]),
        MERGE_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Err(stderr_or(&output, "jj bookmark list failed"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let names: Vec<&str> = text
        .lines()
        .filter_map(|line| line.split(|c: char| c == ':' || c.is_whitespace()).next())
        .filter(|name| !name.is_empty() && !name.contains('@'))
        .collect();
    if names.contains(&"main") {
        Ok("main".to_string())
    } else if names.contains(&"master") {
        Ok("master".to_string())
    } else {
        let found = if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        };
        Err(format!(
            "no main or master bookmark to merge into (found: {found})"
        ))
    }
}

/// Whether the origin has any git remote (`jj git remote list` non-empty).
async fn has_remote(dir: &Path) -> bool {
    match run(jj_cmd(dir, &["git", "remote", "list"]), MERGE_TIMEOUT).await {
        Ok(output) if output.status.success() => {
            !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        }
        _ => false,
    }
}

/// Evaluate `template` over `revset` (read-only, `--ignore-working-copy` so it
/// creates no operation that a later `jj undo` would target), returning stdout.
async fn log_template(dir: &Path, revset: &str, template: &str) -> Result<String, String> {
    let output = run(
        jj_cmd(
            dir,
            &[
                "log",
                "-r",
                revset,
                "--no-graph",
                "--ignore-working-copy",
                "-T",
                template,
            ],
        ),
        MERGE_TIMEOUT,
    )
    .await?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(stderr_or(&output, "jj log failed"))
    }
}

/// Run a mutating jj command, mapping a non-zero exit to its stderr.
async fn run_mutation(dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = run(jj_cmd(dir, args), MERGE_TIMEOUT).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(stderr_or(&output, "jj command failed"))
    }
}

/// A `jj --no-pager <args…>` command rooted at `dir`, stdout/stderr captured,
/// stdin closed, child reaped on drop (so a timeout kills it).
fn jj_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("jj");
    cmd.arg("--no-pager");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// The trimmed stderr of a failed jj command, or `fallback` (with the exit
/// status) when jj printed nothing.
fn stderr_or(output: &std::process::Output, fallback: &str) -> String {
    match String::from_utf8_lossy(&output.stderr).trim() {
        "" => format!("{fallback} (status {})", output.status),
        text => text.to_string(),
    }
}

/// A `jj --no-pager diff [--stat] --color=never` command rooted at `dir`, with
/// stdin closed and the child killed if dropped (so a timeout reaps it).
fn base_command(dir: &Path, stat: bool) -> Command {
    let mut cmd = Command::new("jj");
    cmd.arg("--no-pager").arg("diff").arg("--color=never");
    if stat {
        cmd.arg("--stat");
    }
    cmd.current_dir(dir).stdin(Stdio::null()).kill_on_drop(true);
    cmd
}

/// Spawn `cmd` and collect its output within `timeout`. On expiry the future is
/// dropped, which drops the owned `Child`; `kill_on_drop` then reaps it.
async fn run(mut cmd: Command, timeout: Duration) -> Result<std::process::Output, String> {
    let child = cmd.spawn().map_err(|e| format!("spawn jj: {e}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(format!("jj failed: {e}")),
        Err(_) => Err(format!("jj timed out after {}s", timeout.as_secs())),
    }
}

/// Parse the trailing summary line of `jj diff --stat` into a short sidebar
/// stat like `4 files +120 −33`. jj prints `N files changed, A insertions(+),
/// B deletions(-)` (singular `file`/`insertion`/`deletion` at a count of 1, and
/// a clause is dropped when jj omits it). Returns `None` for a clean working
/// copy (`0 files changed`) or anything unparseable, so the sidebar stays quiet.
pub fn parse_stat(output: &str) -> Option<String> {
    let line = output.lines().rev().find(|l| l.contains("changed"))?.trim();
    let files = clause_before(line, "file")?;
    if files == 0 {
        return None;
    }
    let insertions = clause_before(line, "insertion").unwrap_or(0);
    let deletions = clause_before(line, "deletion").unwrap_or(0);
    let unit = if files == 1 { "file" } else { "files" };
    Some(format!("{files} {unit} +{insertions} −{deletions}"))
}

/// The integer immediately preceding the first token starting with `noun`
/// (matching both `file` and `files`, `insertion` and `insertions`). `None` when
/// the noun is absent or not preceded by a number.
fn clause_before(line: &str, noun: &str) -> Option<u64> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let idx = tokens.iter().position(|t| t.starts_with(noun))?;
    tokens.get(idx.checked_sub(1)?)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_reads_plural_counts() {
        assert_eq!(
            parse_stat("a.txt | 3 +++\n2 files changed, 120 insertions(+), 33 deletions(-)")
                .as_deref(),
            Some("2 files +120 −33"),
        );
    }

    #[test]
    fn parse_stat_reads_singular_forms() {
        assert_eq!(
            parse_stat("one.txt | 1 +\n1 file changed, 1 insertion(+), 0 deletions(-)").as_deref(),
            Some("1 file +1 −0"),
        );
        // A summary that omits the deletions clause entirely still parses.
        assert_eq!(
            parse_stat("1 file changed, 1 insertion(+)").as_deref(),
            Some("1 file +1 −0"),
        );
    }

    #[test]
    fn parse_stat_zero_change_is_none() {
        assert_eq!(
            parse_stat("0 files changed, 0 insertions(+), 0 deletions(-)"),
            None,
        );
    }

    #[test]
    fn parse_stat_garbage_is_none() {
        assert_eq!(parse_stat(""), None);
        assert_eq!(parse_stat("not a stat line at all"), None);
        assert_eq!(parse_stat("files changed, but no number"), None);
    }

    #[test]
    fn fork_dest_is_a_named_sibling_of_the_repo_root() {
        assert_eq!(
            fork_dest(Path::new("/home/me/proj"), "feat").as_deref(),
            Some(Path::new("/home/me/proj-feat"))
        );
        // No parent (filesystem root) yields None.
        assert_eq!(fork_dest(Path::new("/"), "feat"), None);
    }

    #[test]
    fn valid_fork_name_accepts_word_chars_and_rejects_the_rest() {
        assert!(valid_fork_name("feature-1_x"));
        assert!(valid_fork_name("ABC"));
        assert!(!valid_fork_name(""));
        assert!(!valid_fork_name("has space"));
        assert!(!valid_fork_name("has/slash"));
        assert!(!valid_fork_name("dots.bad"));
    }

    #[test]
    fn workspace_root_finds_dot_jj_in_an_ancestor() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("tutti-jj-{}-{n}", std::process::id()));
        let nested = root.join("crates").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".jj")).unwrap();

        assert_eq!(workspace_root(&nested).as_deref(), Some(root.as_path()));
        assert!(is_workspace(&nested));

        let bare = std::env::temp_dir().join(format!("tutti-nojj-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(workspace_root(&bare), None);
        assert!(!is_workspace(&bare));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }
}
