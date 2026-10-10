//! Harvesting resumable conversations from the agent tools' own on-disk
//! session stores, so the launcher can offer "pick up where you left off"
//! rows for a project. Strictly read-only over the tools' files — tutti never
//! writes their stores. Claude Code, Muse Code, and Mistral Vibe are harvested
//! today; new agents slot in beside them in `resume_sessions`.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
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

/// The roots the agent tools keep their session stores under, resolved by the
/// caller so the harvest stays pure over paths (tests point them at a fixture
/// tree instead of the real ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stores {
    /// `$HOME` — Claude Code's `.claude/projects`.
    pub home: PathBuf,
    /// `$XDG_DATA_HOME` (default `~/.local/share`) — Muse Code's `muse/sessions`.
    pub data_home: PathBuf,
    /// `$VIBE_HOME` (default `~/.vibe`) — Mistral Vibe's `logs/session`.
    pub vibe_home: PathBuf,
}

impl Stores {
    /// The tools' default layout under `home`.
    pub fn under(home: &Path) -> Stores {
        Stores {
            home: home.to_path_buf(),
            data_home: home.join(".local/share"),
            vibe_home: home.join(".vibe"),
        }
    }

    /// The default layout with the tools' own environment overrides applied.
    pub fn from_env(home: &Path) -> Stores {
        let mut stores = Stores::under(home);
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            stores.data_home = dir.into();
        }
        if let Some(dir) = std::env::var_os("VIBE_HOME") {
            stores.vibe_home = dir.into();
        }
        stores
    }
}

/// The argv that resumes `agent`'s conversation `id` in a fresh pane — each
/// tool's own recipe — or `None` for an agent without one tutti knows.
/// opencode's is from its `--help` (1.18.5: `-s, --session  session id to
/// continue`); it is not harvested, but its plugin reports the id.
pub fn resume_cmd(agent: &str, id: &str) -> Option<Vec<String>> {
    let cmd = match agent {
        "claude" => ["claude", "--resume", id],
        "muse" => ["muse", "resume", id],
        "vibe" => ["vibe", "--resume", id],
        "opencode" => ["opencode", "--session", id],
        _ => return None,
    };
    Some(cmd.map(String::from).into())
}

/// The resumable conversations for the project at `dir` across every harvested
/// store, newest first, capped at `cap` per agent.
pub fn resume_sessions(dir: &Path, stores: &Stores, cap: usize) -> Vec<ResumeSession> {
    let mut sessions = claude_sessions(dir, &stores.home, cap);
    sessions.extend(muse_sessions(dir, &stores.data_home, cap));
    sessions.extend(vibe_sessions(dir, &stores.vibe_home, cap));
    sessions.sort_by_key(|s| Reverse(s.last_active));
    sessions
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
            cmd: resume_cmd("claude", id).expect("claude has a resume recipe"),
        });
    }
    sessions.sort_by_key(|s| Reverse(s.last_active));
    sessions.truncate(cap);
    sessions
}

/// Muse Code shards its store by local date —
/// `$XDG_DATA_HOME/muse/sessions/YYYY/MM/DD/<session-id>/session.jsonl` (the
/// layout its bundled `read-session` skill documents). Each log's head carries
/// a `runtime.session.metadata` record naming the canonicalised
/// `workspace_root`, which decides whether the session belongs to `dir`; the
/// first `turn_submit` intake record holds the opening prompt. Subagent logs
/// nest deeper (`<id>/subagent/…`) and are never touched.
fn muse_sessions(dir: &Path, data_home: &Path, cap: usize) -> Vec<ResumeSession> {
    let store = data_home.join("muse").join("sessions");
    let root = canonical(dir);
    let years = subdirs(&store);
    let mut sessions = Vec::new();
    for session_dir in years
        .iter()
        .flat_map(|y| subdirs(y))
        .flat_map(|m| subdirs(&m))
        .flat_map(|d| subdirs(&d))
    {
        let Some(id) = session_dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let path = session_dir.join("session.jsonl");
        let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        let Some(title) = scan_muse_session(&path, &root) else {
            continue; // another workspace's conversation
        };
        sessions.push(ResumeSession {
            agent: "muse".into(),
            id: id.into(),
            title,
            last_active: modified,
            cmd: resume_cmd("muse", id).expect("muse has a resume recipe"),
        });
    }
    sessions.sort_by_key(|s| Reverse(s.last_active));
    sessions.truncate(cap);
    sessions
}

/// The subdirectories of `dir`; empty when it cannot be read.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// Scan the head of a Muse session log: confirm its `workspace_root` is `root`
/// and pull the first submitted prompt as a title, falling back to the slug
/// Muse names the session (`brook-pyxis`) when no prompt was typed. `None`
/// rejects the file — it belongs to another workspace, or never named one.
fn scan_muse_session(path: &Path, root: &Path) -> Option<Option<String>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut belongs = false;
    let mut prompt = None;
    let mut name = None;
    for line in std::io::BufReader::new(file).lines().take(100) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let payload = &value["payload"];
        match value["payload_type"].as_str() {
            Some("runtime.session.metadata") => {
                let workspace = payload.pointer("/record/workspace_root")?.as_str()?;
                if canonical(Path::new(workspace)) != root {
                    return None;
                }
                belongs = true;
            }
            Some("runtime.command_intake.received")
                if prompt.is_none()
                    && payload
                        .pointer("/record/command/kind")
                        .and_then(|k| k.as_str())
                        == Some("turn_submit") =>
            {
                prompt = payload
                    .pointer("/record/command/prompt")
                    .and_then(|p| p.as_str())
                    .and_then(first_prompt_line);
            }
            Some("session.name.changed") => {
                name = payload["new_name"].as_str().map(str::to_string);
            }
            _ => {}
        }
        if belongs && prompt.is_some() {
            break;
        }
    }
    belongs.then_some(prompt.or(name))
}

/// Mistral Vibe keeps one `session_<stamp>_<short-id>/` directory per
/// conversation under `$VIBE_HOME/logs/session/`; its `meta.json` names the
/// full `session_id` (what `--resume` takes), the `working_directory`, and an
/// auto title cut from the first prompt. Delegated child sessions carry a
/// `parent_session_id` and are not offered; directories are matched on
/// resolved paths, as Vibe's own picker does.
fn vibe_sessions(dir: &Path, vibe_home: &Path, cap: usize) -> Vec<ResumeSession> {
    let root = canonical(dir);
    let mut sessions = Vec::new();
    for session_dir in subdirs(&vibe_home.join("logs").join("session")) {
        let meta = session_dir.join("meta.json");
        let Ok(modified) = std::fs::metadata(&meta).and_then(|m| m.modified()) else {
            continue;
        };
        let Some(value) = std::fs::read_to_string(&meta)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        else {
            continue;
        };
        let (Some(id), Some(workspace)) = (
            value["session_id"].as_str(),
            value
                .pointer("/environment/working_directory")
                .and_then(|w| w.as_str()),
        ) else {
            continue;
        };
        if !value["parent_session_id"].is_null() || canonical(Path::new(workspace)) != root {
            continue;
        }
        sessions.push(ResumeSession {
            agent: "vibe".into(),
            id: id.into(),
            title: value["title"].as_str().and_then(first_prompt_line),
            last_active: modified,
            cmd: resume_cmd("vibe", id).expect("vibe has a resume recipe"),
        });
    }
    sessions.sort_by_key(|s| Reverse(s.last_active));
    sessions.truncate(cap);
    sessions
}

/// `path` with symlinks resolved, or itself when it cannot be resolved — a
/// workspace recorded as `/tmp/x` and one as `/private/tmp/x` are one place.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
/// preamble), capped so a pasted wall of text stays a label. Also what a hook
/// reports of a live prompt, so a turn and a resume row label alike.
pub fn first_prompt_line(content: &str) -> Option<String> {
    let line = content.lines().next()?.trim();
    if line.is_empty() || line.starts_with('<') || line.starts_with("Caveat:") {
        return None;
    }
    Some(line.chars().take(80).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let sessions = resume_sessions(dir, &Stores::under(&home), 3);
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
            resume_sessions(dir, &Stores::under(&home), 1).len(),
            1,
            "the cap trims the tail"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Write a Muse session log under the date shard `date`: the head metadata
    /// record naming `root`, the automatic session-name record, and — when
    /// given — a `turn_submit` intake carrying `prompt`; pinned `age` seconds
    /// old. Returns the session directory.
    fn write_muse_session(
        data_home: &Path,
        date: &str,
        id: &str,
        root: &str,
        name: &str,
        prompt: Option<&str>,
        age: u64,
    ) -> PathBuf {
        let session_dir = data_home.join("muse/sessions").join(date).join(id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut lines = vec![
            serde_json::json!({
                "payload_type": "runtime.session.metadata",
                "payload": {"kind": "metadata", "record": {"workspace_root": root}},
            }),
            serde_json::json!({
                "payload_type": "session.name.changed",
                "payload": {"new_name": name, "source": "automatic"},
            }),
        ];
        if let Some(prompt) = prompt {
            lines.push(serde_json::json!({
                "payload_type": "runtime.command_intake.received",
                "payload": {"kind": "command_intake", "record": {
                    "kind": "received",
                    "command": {"kind": "turn_submit", "prompt": prompt},
                }},
            }));
        }
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let path = session_dir.join("session.jsonl");
        std::fs::write(&path, text).unwrap();
        let then = SystemTime::now() - Duration::from_secs(age);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(then)
            .unwrap();
        session_dir
    }

    #[test]
    fn muse_harvest_walks_date_shards_and_matches_the_canonical_root() {
        let home = fixture_home();
        let data_home = home.join(".local/share");
        // Muse records the canonicalised workspace root (`/tmp` → `/private/tmp`
        // on macOS); the harvest is asked with the path as tutti holds it.
        let dir = home.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.canonicalize().unwrap();
        let root = root.to_str().unwrap();
        write_muse_session(
            &data_home,
            "2026/08/31",
            "old",
            root,
            "brook-pyxis",
            Some("fix the sidebar"),
            7200,
        );
        let new = write_muse_session(
            &data_home,
            "2026/09/01",
            "new",
            root,
            "acorn-pictor",
            Some("add resume rows"),
            60,
        );
        // Opened and closed without a prompt: the automatic name labels it.
        write_muse_session(
            &data_home,
            "2026/09/01",
            "quiet",
            root,
            "cedar-lyra",
            None,
            3600,
        );
        write_muse_session(
            &data_home,
            "2026/09/01",
            "alien",
            "/somewhere/else",
            "moss-vela",
            Some("not ours"),
            30,
        );
        // A subagent log nests below its parent and is never a row.
        write_muse_session(
            &new.join("subagent"),
            "",
            "child",
            root,
            "fern-ara",
            Some("delegated"),
            10,
        );

        let sessions = resume_sessions(&dir, &Stores::under(&home), 3);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["new", "quiet", "old"],
            "newest first, alien and subagent skipped"
        );
        let titles: Vec<Option<&str>> = sessions.iter().map(|s| s.title.as_deref()).collect();
        assert_eq!(
            titles,
            vec![
                Some("add resume rows"),
                Some("cedar-lyra"),
                Some("fix the sidebar")
            ]
        );
        assert!(sessions.iter().all(|s| s.agent == "muse"));
        assert_eq!(
            sessions[0].cmd,
            vec!["muse", "resume", "new"],
            "the resume argv is muse's own recipe"
        );
        assert_eq!(resume_sessions(&dir, &Stores::under(&home), 1).len(), 1);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn harvest_merges_agents_newest_first() {
        let home = fixture_home();
        let data_home = home.join(".local/share");
        let dir = home.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let store = home.join(".claude/projects").join(munge(&dir));
        std::fs::create_dir_all(&store).unwrap();
        write_session(&store, "c1", dir.to_str().unwrap(), "claude first", 600);
        let root = dir.canonicalize().unwrap();
        write_muse_session(
            &data_home,
            "2026/09/01",
            "m1",
            root.to_str().unwrap(),
            "brook-pyxis",
            Some("muse later"),
            60,
        );

        write_vibe_session(&home.join(".vibe"), "v1", dir.to_str().unwrap(), None, 300);

        let sessions = resume_sessions(&dir, &Stores::under(&home), 3);
        let agents: Vec<&str> = sessions.iter().map(|s| s.agent.as_str()).collect();
        assert_eq!(
            agents,
            vec!["muse", "vibe", "claude"],
            "one recency order across stores"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Write a Vibe session directory: `meta.json` naming `working_directory`,
    /// an auto `title`, and `parent` (a child session's parent id) — pinned
    /// `age` seconds old.
    fn write_vibe_session(vibe_home: &Path, id: &str, cwd: &str, parent: Option<&str>, age: u64) {
        let session_dir = vibe_home
            .join("logs/session")
            .join(format!("session_20260901_120000_{id}"));
        std::fs::create_dir_all(&session_dir).unwrap();
        let meta = serde_json::json!({
            "session_id": format!("{id}-5494-4bfe-33e9-a54dd8ab56b8"),
            "parent_session_id": parent,
            "environment": {"working_directory": cwd},
            "title": format!("prompt of {id} ...…"),
            "title_source": "auto",
        });
        let path = session_dir.join("meta.json");
        std::fs::write(&path, meta.to_string()).unwrap();
        std::fs::write(session_dir.join("messages.jsonl"), "").unwrap();
        let then = SystemTime::now() - Duration::from_secs(age);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(then)
            .unwrap();
    }

    #[test]
    fn vibe_harvest_matches_the_working_directory_and_skips_children() {
        let home = fixture_home();
        let vibe_home = home.join(".vibe");
        let dir = home.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_str().unwrap();
        write_vibe_session(&vibe_home, "old", cwd, None, 7200);
        write_vibe_session(&vibe_home, "new", cwd, None, 60);
        write_vibe_session(&vibe_home, "child", cwd, Some("new-5494"), 30);
        write_vibe_session(&vibe_home, "alien", "/somewhere/else", None, 10);
        // The per-tty last-session pointers live beside the sessions.
        std::fs::create_dir_all(vibe_home.join("logs/session/.last_session")).unwrap();
        std::fs::write(
            vibe_home.join("logs/session/.last_session/ttys000"),
            "new-5494-4bfe-33e9-a54dd8ab56b8\n",
        )
        .unwrap();

        let sessions = resume_sessions(&dir, &Stores::under(&home), 3);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "new-5494-4bfe-33e9-a54dd8ab56b8",
                "old-5494-4bfe-33e9-a54dd8ab56b8"
            ],
            "newest first; the child and the alien are not offered"
        );
        assert_eq!(sessions[0].agent, "vibe");
        assert_eq!(sessions[0].title.as_deref(), Some("prompt of new ...…"));
        assert_eq!(
            sessions[0].cmd,
            vec!["vibe", "--resume", "new-5494-4bfe-33e9-a54dd8ab56b8"],
            "the resume argv carries the full session id"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resume_cmd_is_each_tools_own_recipe() {
        let cmd = |agent| resume_cmd(agent, "s1").unwrap();
        assert_eq!(cmd("claude"), ["claude", "--resume", "s1"]);
        assert_eq!(cmd("muse"), ["muse", "resume", "s1"]);
        assert_eq!(cmd("vibe"), ["vibe", "--resume", "s1"]);
        assert_eq!(cmd("opencode"), ["opencode", "--session", "s1"]);
        assert_eq!(resume_cmd("codex", "s1"), None, "no recipe known");
    }

    #[test]
    fn stores_default_layout_hangs_off_home() {
        let stores = Stores::under(Path::new("/home/u"));
        assert_eq!(stores.data_home, Path::new("/home/u/.local/share"));
        assert_eq!(stores.vibe_home, Path::new("/home/u/.vibe"));
    }

    #[test]
    fn harvest_of_an_unknown_project_is_empty() {
        let home = fixture_home();
        assert!(resume_sessions(Path::new("/nowhere"), &Stores::under(&home), 3).is_empty());
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

        let sessions = resume_sessions(dir, &Stores::under(&home), 3);
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
            resume_sessions(dir, &Stores::under(&home), 3).is_empty(),
            "an unverifiable session is not offered"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
