//! Harvesting resumable conversations from the agent tools' own on-disk
//! session stores, so the launcher can offer "pick up where you left off"
//! rows for a project. Strictly read-only over the tools' files — tutti never
//! writes their stores. Claude Code is the only harvester today; new agents
//! slot in beside it in `resume_sessions`.

use std::cmp::Reverse;
use std::path::Path;
use std::time::SystemTime;

/// One resumable conversation harvested from an agent tool's session store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeSession {
    /// The agent kind that owns the conversation (registry vocabulary).
    pub agent: String,
    /// The tool's session id — what its resume flag takes.
    pub id: String,
    /// The first typed user prompt, as a recognisable label. `None` when the
    /// session holds no readable prompt.
    pub title: Option<String>,
    /// When the conversation last progressed (the session file's mtime).
    pub last_active: SystemTime,
    /// The argv that resumes the conversation in a fresh pane.
    pub cmd: Vec<String>,
}

/// The resumable conversations for the project at `dir`, newest first, capped
/// at `cap` per agent. `home` is the harvest root (the user's home directory),
/// passed in so tests point it at a fixture tree instead of the real one.
pub fn resume_sessions(dir: &Path, home: &Path, cap: usize) -> Vec<ResumeSession> {
    claude_sessions(dir, home, cap)
}

/// Claude Code keeps one `<session-id>.jsonl` per conversation under
/// `~/.claude/projects/<munged-cwd>/`. The munge is lossy, so every candidate
/// is verified against the `cwd` field its own lines carry before it may
/// represent `dir`; subagent transcripts live in subdirectories and are never
/// touched.
fn claude_sessions(dir: &Path, home: &Path, cap: usize) -> Vec<ResumeSession> {
    let store = home.join(".claude").join("projects").join(munge(dir));
    let Ok(entries) = std::fs::read_dir(&store) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let Some(title) = scan_session(&path, dir) else {
            continue; // not this project's conversation (munge collision)
        };
        sessions.push(ResumeSession {
            agent: "claude".into(),
            id: id.into(),
            title,
            last_active: modified,
            cmd: vec!["claude".into(), "--resume".into(), id.into()],
        });
    }
    sessions.sort_by_key(|s| Reverse(s.last_active));
    sessions.truncate(cap);
    sessions
}

/// Claude Code's store-directory name for a cwd: every path character that is
/// not ASCII-alphanumeric or `-` becomes `-` (`/` and `.` both munge to `-`,
/// dashes survive — verified against real stores).
fn munge(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Scan the head of a session file: confirm the conversation really ran in
/// `dir` and pull the first typed prompt as a title. Returns `None` to reject
/// the file — a line placed it in another directory, or none placed it at all
/// (an unverifiable session is not worth offering). Only the head is read;
/// session files grow to megabytes.
fn scan_session(path: &Path, dir: &Path) -> Option<Option<String>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut belongs = false;
    let mut title = None;
    for line in std::io::BufReader::new(file).lines().take(100) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str()) {
            if Path::new(cwd) != dir {
                return None;
            }
            belongs = true;
        }
        if title.is_none() && value.get("type").and_then(|t| t.as_str()) == Some("user") {
            title = value
                .pointer("/message/content")
                .and_then(|c| c.as_str())
                .and_then(first_prompt_line);
        }
        if belongs && title.is_some() {
            break;
        }
    }
    belongs.then_some(title)
}

/// The label-worthy first line of a prompt: trimmed, skipping tool-injected
/// content (`<command-name>…`, `<system-reminder>`, the resume `Caveat:`
/// preamble), capped so a pasted wall of text stays a label.
fn first_prompt_line(content: &str) -> Option<String> {
    let line = content.lines().next()?.trim();
    if line.is_empty() || line.starts_with('<') || line.starts_with("Caveat:") {
        return None;
    }
    Some(line.chars().take(80).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn fixture_home() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tutti-resume-{}-{n}", std::process::id()))
    }

    /// Write a session file whose first user line carries `cwd` and `prompt`,
    /// and pin its mtime `age` seconds in the past for deterministic ordering.
    fn write_session(store: &Path, id: &str, cwd: &str, prompt: &str, age: u64) {
        let lines = format!(
            "{}\n{}\n",
            serde_json::json!({"type": "mode", "mode": "normal", "sessionId": id}),
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": prompt},
                "cwd": cwd,
                "sessionId": id,
            }),
        );
        let path = store.join(format!("{id}.jsonl"));
        std::fs::write(&path, lines).unwrap();
        let then = SystemTime::now() - Duration::from_secs(age);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(then)
            .unwrap();
    }

    #[test]
    fn munge_replaces_everything_but_alphanumerics_and_dashes() {
        assert_eq!(
            munge(Path::new("/Users/me/develop/tutti")),
            "-Users-me-develop-tutti"
        );
        assert_eq!(munge(Path::new("/Users/me/.emacs.d")), "-Users-me--emacs-d");
        assert_eq!(munge(Path::new("/tmp/jj-tools_x")), "-tmp-jj-tools-x");
    }

    #[test]
    fn harvest_orders_newest_first_verifies_cwd_and_caps() {
        let home = fixture_home();
        let dir = Path::new("/tmp/proj_a");
        let store = home.join(".claude/projects").join(munge(dir));
        std::fs::create_dir_all(&store).unwrap();
        write_session(&store, "old", "/tmp/proj_a", "fix the sidebar", 7200);
        write_session(&store, "new", "/tmp/proj_a", "add resume rows", 60);
        // A munge collision: same store name, different real directory.
        write_session(&store, "alien", "/tmp/proj.a", "not ours", 30);
        // Subagent transcripts live in subdirectories — never scanned.
        std::fs::create_dir_all(store.join("new")).unwrap();

        let sessions = resume_sessions(dir, &home, 3);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "old"], "newest first, the alien rejected");
        assert_eq!(sessions[0].title.as_deref(), Some("add resume rows"));
        assert_eq!(
            sessions[0].cmd,
            vec!["claude", "--resume", "new"],
            "the resume argv carries the session id"
        );
        assert_eq!(sessions[0].agent, "claude");
        assert_eq!(
            resume_sessions(dir, &home, 1).len(),
            1,
            "the cap trims the tail"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn harvest_of_an_unknown_project_is_empty() {
        let home = fixture_home();
        assert!(resume_sessions(Path::new("/nowhere"), &home, 3).is_empty());
    }

    #[test]
    fn tool_injected_first_prompts_are_skipped_for_the_title() {
        let home = fixture_home();
        let dir = Path::new("/tmp/proj_b");
        let store = home.join(".claude/projects").join(munge(dir));
        std::fs::create_dir_all(&store).unwrap();
        let lines = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": "<command-name>/retro</command-name>"},
                "cwd": "/tmp/proj_b",
            }),
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": [{"type": "tool_result"}]},
                "cwd": "/tmp/proj_b",
            }),
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": "real prompt\nsecond line"},
                "cwd": "/tmp/proj_b",
            }),
        );
        std::fs::write(store.join("s1.jsonl"), lines).unwrap();

        let sessions = resume_sessions(dir, &home, 3);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("real prompt"),
            "the wrapper and the tool-result lines are passed over"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_session_that_never_names_a_cwd_is_rejected() {
        let home = fixture_home();
        let dir = Path::new("/tmp/proj_c");
        let store = home.join(".claude/projects").join(munge(dir));
        std::fs::create_dir_all(&store).unwrap();
        let line = serde_json::json!({"type": "mode", "mode": "normal"});
        std::fs::write(store.join("s1.jsonl"), format!("{line}\n")).unwrap();

        assert!(
            resume_sessions(dir, &home, 3).is_empty(),
            "an unverifiable session is not offered"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
