//! What each terminal holds of each pane, so a second `pane attach --inline`
//! in the same terminal prints only the history the terminal lacks. One
//! private file, `$XDG_STATE_HOME/tutti/inline.json` (0600), keeps a record
//! per terminal, session and pane — the newest `KEPT` — each the `Printed`
//! the last inline attach there left.
//!
//! A terminal is its tty and the session that owns it (getsid): one closed,
//! and another opened on the tty number it freed, is another session, and so
//! a terminal that holds nothing.

use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tutti_core::{PaneId, Printed, state_dir};

/// The most records the file keeps, the oldest going first.
const KEPT: usize = 64;

#[derive(Serialize, Deserialize)]
struct Record {
    terminal: String,
    session: String,
    pane: PaneId,
    printed: Printed,
}

/// The file, `None` with no state dir.
pub fn file() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("inline.json"))
}

/// This terminal: stdin's tty and the session that owns it — `None` when
/// stdin is no tty or in no session.
pub fn terminal() -> Option<String> {
    let tty = std::io::stdin().as_fd().try_clone_to_owned().ok()?;
    let tty = std::fs::File::from(tty).metadata().ok()?.rdev();
    // SAFETY: getsid takes and returns plain integers.
    let session = unsafe { libc::getsid(0) };
    (session > 0).then(|| format!("tty {tty} session {session}"))
}

/// What `terminal` holds of `pane` of `session`, as `file` records it.
pub fn load(file: &Path, terminal: &str, session: &str, pane: PaneId) -> Option<Printed> {
    read(file)
        .into_iter()
        .find(|r| r.terminal == terminal && r.session == session && r.pane == pane)
        .map(|r| r.printed)
}

/// Record in `file` that `terminal` holds `printed` of `pane` of `session`,
/// in place of what it held of it before: a file of the process's own,
/// written whole, then renamed over it.
pub fn save(
    file: &Path,
    terminal: &str,
    session: &str,
    pane: PaneId,
    printed: Printed,
) -> std::io::Result<()> {
    let mut records = read(file);
    records.retain(|r| !(r.terminal == terminal && r.session == session && r.pane == pane));
    records.push(Record {
        terminal: terminal.into(),
        session: session.into(),
        pane,
        printed,
    });
    records.drain(..records.len().saturating_sub(KEPT));
    if let Some(dir) = file.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let tmp = file.with_extension(format!("json.{}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?
        .write_all(&serde_json::to_vec(&records)?)?;
    std::fs::rename(&tmp, file)
}

/// The records in `file`: none when it is missing or unreadable.
fn read(file: &Path) -> Vec<Record> {
    std::fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn printed(row: u64) -> Printed {
        Printed {
            epoch: 7,
            row,
            held: vec![1, 2],
        }
    }

    /// A record comes back for its terminal, session and pane only; a
    /// second for the same takes its place; the file is the owner's alone,
    /// and keeps the newest `KEPT`; one unreadable holds none.
    #[test]
    fn a_terminal_gets_back_what_it_last_printed_of_a_pane() {
        let dir = std::env::temp_dir().join(format!("tutti-printed-{}", std::process::id()));
        let file = dir.join("state").join("inline.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&file, "tty 1 session 2", "s", PaneId(3)), None);

        save(&file, "tty 1 session 2", "s", PaneId(3), printed(40)).unwrap();
        save(&file, "tty 1 session 9", "s", PaneId(3), printed(50)).unwrap();
        save(&file, "tty 1 session 2", "s", PaneId(3), printed(60)).unwrap();
        assert_eq!(
            load(&file, "tty 1 session 2", "s", PaneId(3)),
            Some(printed(60))
        );
        assert_eq!(
            load(&file, "tty 1 session 9", "s", PaneId(3)),
            Some(printed(50))
        );
        assert_eq!(load(&file, "tty 1 session 2", "s", PaneId(4)), None);
        assert_eq!(load(&file, "tty 1 session 2", "t", PaneId(3)), None);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(read(&file).len(), 2);

        for pane in 0..KEPT as u64 {
            save(&file, "tty 5 session 6", "s", PaneId(pane), printed(pane)).unwrap();
        }
        assert_eq!(read(&file).len(), KEPT);
        assert_eq!(
            load(&file, "tty 1 session 2", "s", PaneId(3)),
            None,
            "the oldest went"
        );

        std::fs::write(&file, b"not json").unwrap();
        assert_eq!(load(&file, "tty 5 session 6", "s", PaneId(1)), None);
        save(&file, "tty 1 session 2", "s", PaneId(3), printed(70)).unwrap();
        assert_eq!(read(&file).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
