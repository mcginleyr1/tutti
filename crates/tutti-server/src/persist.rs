//! Layout persistence: the session saved as it changes, so the next daemon can
//! offer it back. The live save `<session>.json` is rewritten whenever the
//! layout moves (and once more at a graceful shutdown); a starting daemon
//! turns it into the pending offer `<session>.restore.json`, which an attach
//! advertises and `SessionRestore` consumes or discards. A daemon never
//! restores on its own, and never dies over a bad file: one that does not parse
//! is logged and set aside as `<session>.restore.json.bad`.

use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tutti_core::{AgentKind, IssueRef, Layout, PaneId, RestoreSummary, TabId};

use crate::checkpoints::Turns;
use crate::session::ForkMeta;

/// The on-disk format's version: a file carrying another is set aside rather
/// than misread.
pub const FORMAT: u32 = 1;

/// A saved session: what `Session::snapshot` takes and `Session::restore`
/// rebuilds. Tabs and panes keep the ids they had when saved, which is all the
/// layout trees and the current tab refer to them by.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub format: u32,
    pub workspaces: Vec<SavedWorkspace>,
    /// The tab that was current.
    pub current_tab: Option<TabId>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub dir: PathBuf,
    pub name: String,
    /// Present for a workspace tutti created, so a restored one can still
    /// merge and discard.
    pub fork: Option<ForkMeta>,
    pub tabs: Vec<SavedTab>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub id: TabId,
    /// The tab's own name, when renamed away from its default.
    pub name: Option<String>,
    /// The split tree, ratios exact, leaves by the panes' saved ids.
    pub layout: Layout,
    pub active_pane: Option<PaneId>,
    /// The tab's panes, in layout order.
    pub panes: Vec<SavedPane>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedPane {
    pub id: PaneId,
    pub title: String,
    /// What the pane was spawned with. Kept to say what ran there, never to
    /// run it again: re-running `claude "fix the tests"` re-issues the prompt.
    pub argv: Vec<String>,
    pub agent: Option<AgentKind>,
    /// The agent's own id for the conversation it held.
    pub conversation: Option<String>,
    /// The tracker issue the agent was deployed on, kept so a revived agent
    /// stays linked to it. Absent from files saved before issue links.
    #[serde(default)]
    pub issue: Option<Box<IssueRef>>,
    pub turns: Turns,
}

impl SavedPane {
    /// The argv reviving this pane's agent in its own conversation — its
    /// tool's resume recipe — or `None` when it held no agent, no conversation,
    /// or a tool tutti knows no recipe for: such a pane comes back a shell.
    pub fn revive(&self) -> Option<Vec<String>> {
        let (agent, conversation) = (self.agent.as_ref()?, self.conversation.as_ref()?);
        tutti_agents::resume_cmd(&agent.0, conversation)
    }
}

/// `<dir>/<session>.json`: the live save.
pub fn live_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.json"))
}

/// `<dir>/<session>.restore.json`: the pending offer.
pub fn offer_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.restore.json"))
}

/// At startup: make sure `dir` exists (private, like the socket dir — it names
/// projects and conversations), turn the previous daemon's live save into the
/// pending offer, replacing an older one, and read the offer once so a bad file
/// is set aside now rather than at the first attach. Returns whether `dir` is
/// usable; with it missing, persistence is off.
pub fn offer_previous(dir: &Path, session: &str) -> bool {
    if let Err(err) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        eprintln!("tutti: layouts will not be saved: {}: {err}", dir.display());
        return false;
    }
    match std::fs::rename(live_path(dir, session), offer_path(dir, session)) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("tutti: cannot offer the last layout back: {err}"),
    }
    read_offer(dir, session);
    true
}

/// The pending offer, `None` when there is none. One that cannot be read or
/// parsed — or carries another format — is logged and renamed to
/// `<session>.restore.json.bad`, so it is reported once, not on every attach.
pub fn read_offer(dir: &Path, session: &str) -> Option<Saved> {
    let path = offer_path(dir, session);
    let problem = match std::fs::read(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => err.to_string(),
        Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
            Ok(saved) if saved.format == FORMAT => return Some(saved),
            Ok(saved) => format!("format {} (this daemon reads {FORMAT})", saved.format),
            Err(err) => err.to_string(),
        },
    };
    let bad = path.with_extension("json.bad");
    eprintln!(
        "tutti: set aside an unreadable saved layout as {}: {problem}",
        bad.display()
    );
    let _ = std::fs::rename(&path, &bad);
    None
}

/// What the pending offer would bring back, for the attach handshake: its
/// workspaces and panes, the agents among them that would revive, and when it
/// was saved — the file's mtime, since a save only lands when the layout moved
/// (and at a graceful shutdown).
pub fn summary(dir: &Path, session: &str) -> Option<RestoreSummary> {
    let saved = read_offer(dir, session)?;
    let saved_at = std::fs::metadata(offer_path(dir, session))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs());
    let panes = || {
        saved
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
    };
    Some(RestoreSummary {
        workspaces: saved.workspaces.len(),
        panes: panes().count(),
        agents: panes().filter(|p| p.revive().is_some()).count(),
        saved_at,
    })
}

/// `save`, unless the bytes are what the last save that landed (`last`) wrote
/// — so an idle session never touches the disk. Returns whether it wrote.
pub fn save_changed(path: &Path, last: &mut Option<Vec<u8>>, saved: Option<&Saved>) -> bool {
    let bytes = saved.map(encode);
    if bytes == *last || !write(path, bytes.as_deref()) {
        return false;
    }
    *last = bytes;
    true
}

/// Save `saved` as the live file at `path`. An empty session (`None`) removes
/// the file instead: saving nothing would push a pending offer aside at the
/// next start. Returns whether it landed.
pub fn save(path: &Path, saved: Option<&Saved>) -> bool {
    write(path, saved.map(encode).as_deref())
}

/// The live file's bytes: pretty JSON, for whoever opens it.
fn encode(saved: &Saved) -> Vec<u8> {
    serde_json::to_vec_pretty(saved).expect("a layout serializes")
}

/// Write `bytes` to `path` whole — a synced temp file beside it, renamed over
/// it, so a crash mid-write leaves the previous save rather than half of a new
/// one — or, for `None`, remove it. Returns whether that landed; a failure is
/// logged.
fn write(path: &Path, bytes: Option<&[u8]>) -> bool {
    let landed = match bytes {
        Some(bytes) => {
            let tmp = path.with_extension("json.tmp");
            std::fs::File::create(&tmp)
                .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
                .and_then(|()| std::fs::rename(&tmp, path))
        }
        None => match std::fs::remove_file(path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    };
    if let Err(err) = &landed {
        eprintln!("tutti: cannot save the layout to {}: {err}", path.display());
    }
    landed.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tutti_core::Direction;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh state directory under the system temp dir, never the user's.
    fn state_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tutti-persist-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn pane(id: u64, agent: Option<&str>, conversation: Option<&str>) -> SavedPane {
        SavedPane {
            id: PaneId(id),
            title: "claude".into(),
            argv: vec!["claude".into(), "fix the tests".into()],
            agent: agent.map(Into::into),
            conversation: conversation.map(Into::into),
            issue: None,
            turns: Turns::default(),
        }
    }

    fn saved() -> Saved {
        Saved {
            format: FORMAT,
            workspaces: vec![SavedWorkspace {
                dir: "/srv/api".into(),
                name: "api".into(),
                fork: None,
                tabs: vec![SavedTab {
                    id: TabId(4),
                    name: None,
                    layout: Layout::Split {
                        direction: Direction::Vertical,
                        ratio: 0.55,
                        first: Box::new(Layout::Leaf(PaneId(7))),
                        second: Box::new(Layout::Leaf(PaneId(9))),
                    },
                    active_pane: Some(PaneId(9)),
                    panes: vec![
                        pane(7, Some("claude"), Some("c1")),
                        pane(9, Some("claude"), None),
                    ],
                }],
            }],
            current_tab: Some(TabId(4)),
        }
    }

    #[test]
    fn only_an_agent_holding_a_resumable_conversation_revives() {
        assert_eq!(
            pane(1, Some("claude"), Some("c1")).revive(),
            Some(vec!["claude".into(), "--resume".into(), "c1".into()]),
            "its tool's own recipe — never the saved argv"
        );
        assert_eq!(pane(1, Some("claude"), None).revive(), None);
        assert_eq!(pane(1, None, Some("c1")).revive(), None);
        assert_eq!(pane(1, Some("codex"), Some("c1")).revive(), None);
    }

    #[test]
    fn startup_turns_the_live_save_into_the_offer() {
        let dir = state_dir();
        assert!(offer_previous(&dir, "s"), "the state dir is created");
        assert_eq!(
            read_offer(&dir, "s"),
            None,
            "nothing saved, nothing offered"
        );

        let mut last = None;
        assert!(save_changed(
            &live_path(&dir, "s"),
            &mut last,
            Some(&saved())
        ));
        std::fs::write(offer_path(&dir, "s"), b"an older offer").unwrap();
        assert!(offer_previous(&dir, "s"));
        assert!(!live_path(&dir, "s").exists());
        assert_eq!(
            read_offer(&dir, "s"),
            Some(saved()),
            "the live save replaced the older offer"
        );
        let summary = summary(&dir, "s").unwrap();
        assert_eq!(
            (summary.workspaces, summary.panes, summary.agents),
            (1, 2, 1)
        );
        assert!(summary.saved_at > 0, "stamped with the file's mtime");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_or_foreign_offer_is_set_aside_not_fatal() {
        let dir = state_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(live_path(&dir, "s"), b"{ not json").unwrap();
        assert!(offer_previous(&dir, "s"));
        assert!(!offer_path(&dir, "s").exists());
        assert_eq!(
            std::fs::read(dir.join("s.restore.json.bad")).unwrap(),
            b"{ not json"
        );
        assert_eq!(summary(&dir, "s"), None);

        let future = serde_json::to_vec(&Saved {
            format: FORMAT + 1,
            ..saved()
        })
        .unwrap();
        std::fs::write(offer_path(&dir, "s"), future).unwrap();
        assert_eq!(read_offer(&dir, "s"), None, "another format is not misread");
        assert!(!offer_path(&dir, "s").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_live_file_is_written_only_when_its_bytes_change() {
        let dir = state_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = live_path(&dir, "s");
        let mut last = None;
        assert!(
            !save_changed(&path, &mut last, None),
            "an empty session writes nothing"
        );
        assert!(!path.exists());

        assert!(save_changed(&path, &mut last, Some(&saved())));
        std::fs::write(&path, b"untouched").unwrap();
        assert!(!save_changed(&path, &mut last, Some(&saved())));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"untouched",
            "the same layout is not rewritten"
        );

        let mut moved = saved();
        moved.current_tab = None;
        assert!(save_changed(&path, &mut last, Some(&moved)));
        let on_disk: Saved = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk, moved);

        assert!(save_changed(&path, &mut last, None));
        assert!(!path.exists(), "a session that emptied leaves no save");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
