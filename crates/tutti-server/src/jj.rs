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
    let output = match run(cmd).await {
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
    let output = run(cmd).await.ok()?;
    if !output.status.success() {
        return None;
    }
    parse_stat(&String::from_utf8_lossy(&output.stdout))
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

/// Spawn `cmd` and collect its output within the timeout. On expiry the future
/// is dropped, which drops the owned `Child`; `kill_on_drop` then reaps it.
async fn run(mut cmd: Command) -> Result<std::process::Output, String> {
    let child = cmd.spawn().map_err(|e| format!("spawn jj: {e}"))?;
    match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(format!("jj diff failed: {e}")),
        Err(_) => Err(format!("jj diff timed out after {}s", TIMEOUT.as_secs())),
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
