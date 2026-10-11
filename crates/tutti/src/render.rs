use tutti_core::{AgentState, Checkpoint, Note, NoteKind, PaneInfo, TabInfo, WorkspaceInfo};

use crate::issues::Issue;

/// Every entry, projects and the workspaces nested under them, each with the
/// id of the entry it is nested under (`-` for a top-level project).
pub fn workspaces(items: &[WorkspaceInfo]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|w| {
            vec![
                w.id.to_string(),
                w.name.clone(),
                w.parent.map_or_else(|| "-".to_string(), |p| p.to_string()),
                w.dir.display().to_string(),
            ]
        })
        .collect();
    table(&["ID", "NAME", "PARENT", "DIR"], &rows)
}

/// Projects — the top-level entries `project list` keeps — with no parent
/// column, since none has one.
pub fn projects(items: &[WorkspaceInfo]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|w| {
            vec![
                w.id.to_string(),
                w.name.clone(),
                w.dir.display().to_string(),
            ]
        })
        .collect();
    table(&["ID", "NAME", "DIR"], &rows)
}

pub fn tabs(items: &[TabInfo]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|t| {
            vec![
                t.id.to_string(),
                t.name.clone(),
                if t.active { "*" } else { "" }.to_string(),
            ]
        })
        .collect();
    table(&["ID", "NAME", "ACTIVE"], &rows)
}

pub fn panes(items: &[PaneInfo]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|p| {
            vec![
                p.id.to_string(),
                p.title.clone(),
                p.agent
                    .as_ref()
                    .map_or_else(|| "-".to_string(), |a| a.to_string()),
                state_label(p.state).to_string(),
                p.exited.map_or_else(|| "-".to_string(), |c| c.to_string()),
            ]
        })
        .collect();
    table(&["ID", "TITLE", "AGENT", "STATE", "EXIT"], &rows)
}

/// Tracker issues: source, the id `issue deploy` takes, title, labels, url.
pub fn issues(items: &[Issue]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|i| {
            vec![
                i.source.clone(),
                i.id.clone(),
                i.title.clone(),
                if i.labels.is_empty() {
                    "-".into()
                } else {
                    i.labels.join(",")
                },
                i.url.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    table(&["SOURCE", "ID", "TITLE", "LABELS", "URL"], &rows)
}

/// A pane's checkpoints, newest first: number, age, the turn's stat, the short
/// commit id (usable anywhere jj takes a revision), and last — it runs up to
/// 80 characters — the prompt that started the turn, else what closed it.
pub fn checkpoints(items: &[Checkpoint], now: u64) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .rev()
        .map(|c| {
            vec![
                format!("#{}", c.n),
                age(c.time, now),
                turn_stat(c),
                c.commit.chars().take(12).collect(),
                c.prompt.clone().unwrap_or_else(|| c.label.clone()),
            ]
        })
        .collect();
    table(&["N", "AGE", "STAT", "COMMIT", "LABEL"], &rows)
}

/// The notification history, newest first: id, age, the pane's id and title
/// and its workspace when the note landed, what it records, `*` while unread,
/// and last — it runs as long as the notification — its text.
pub fn notes(items: &[Note], now: u64) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .rev()
        .map(|n| {
            vec![
                n.id.to_string(),
                age(n.time, now),
                n.pane.to_string(),
                n.pane_title.clone(),
                n.workspace.clone(),
                note_kind_label(n.kind).to_string(),
                if n.read { "" } else { "*" }.to_string(),
                n.text(),
            ]
        })
        .collect();
    table(
        &[
            "ID",
            "AGE",
            "PANE",
            "TITLE",
            "WORKSPACE",
            "KIND",
            "UNREAD",
            "TEXT",
        ],
        &rows,
    )
}

/// What a note records, in the words the CLI and the notifications panel use.
pub(crate) fn note_kind_label(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::Bell => "bell",
        NoteKind::Notify => "notify",
        NoteKind::Blocked => "blocked",
        NoteKind::Done => "done",
    }
}

/// A checkpoint's turn stat (`2 files +14 −3`), or `-` for the first state
/// recorded for a pane, which has nothing earlier to diff against.
fn turn_stat(c: &Checkpoint) -> String {
    if c.from.is_none() {
        return "-".into();
    }
    let unit = if c.files == 1 { "file" } else { "files" };
    format!("{} {unit} +{} −{}", c.files, c.insertions, c.deletions)
}

/// A `SessionRestore`'s outcome on one line — `restored 3 projects · revived 2
/// agents`, plus how many entries were skipped — shared by the CLI and the
/// TUI's flash. A discard answers all zero with nothing skipped, which no real
/// restore does (an empty layout is never saved).
pub fn restored(workspaces: usize, revived: usize, skipped: usize) -> String {
    if workspaces == 0 && skipped == 0 {
        return "discarded the saved layout".into();
    }
    let mut line = format!(
        "restored {} · revived {}",
        count(workspaces, "project"),
        count(revived, "agent")
    );
    if skipped > 0 {
        line.push_str(&format!(" · skipped {skipped}"));
    }
    line
}

/// `n` and its `unit`, pluralised past one: `1 project`, `3 projects`.
pub(crate) fn count(n: usize, unit: &str) -> String {
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// A compact age from `then` to `now`, both unix seconds: `now`, `Nm`, `Nh`,
/// or `Nd`.
pub(crate) fn age(then: u64, now: u64) -> String {
    match now.saturating_sub(then) {
        0..=59 => "now".into(),
        secs @ 60..=3599 => format!("{}m", secs / 60),
        secs @ 3600..=86_399 => format!("{}h", secs / 3600),
        secs => format!("{}d", secs / 86_400),
    }
}

/// The current time in unix seconds, the clock checkpoint ages read against.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Unknown => "unknown",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Done => "done",
        AgentState::Idle => "idle",
    }
}

/// Left-aligned columns separated by two spaces, header row on top. The final
/// column is never padded so rows carry no trailing whitespace.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let header: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
    write_row(&mut out, &header, &widths);
    for row in rows {
        write_row(&mut out, row, &widths);
    }
    out
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if i + 1 < cells.len() {
            for _ in 0..widths[i].saturating_sub(cell.chars().count()) {
                line.push(' ');
            }
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{PaneId, TabId, WorkspaceId};

    fn entries() -> Vec<WorkspaceInfo> {
        vec![
            WorkspaceInfo {
                id: WorkspaceId(1),
                name: "api".into(),
                dir: "/srv/api".into(),
                parent: None,
            },
            WorkspaceInfo {
                id: WorkspaceId(22),
                name: "api-fix".into(),
                dir: "/srv/api-fix".into(),
                parent: Some(WorkspaceId(1)),
            },
        ]
    }

    #[test]
    fn workspace_table_names_each_entrys_parent() {
        assert_eq!(
            workspaces(&entries()),
            "ID  NAME     PARENT  DIR\n\
             1   api      -       /srv/api\n\
             22  api-fix  1       /srv/api-fix\n"
        );
    }

    #[test]
    fn project_table_aligns_columns_without_a_parent() {
        let mut items = entries();
        items[1].parent = None;
        items[1].name = "frontend".into();
        assert_eq!(
            projects(&items),
            "ID  NAME      DIR\n\
             1   api       /srv/api\n\
             22  frontend  /srv/api-fix\n"
        );
    }

    #[test]
    fn pane_table_shows_missing_agent_and_exit_as_dash() {
        let rendered = panes(&[PaneInfo {
            id: PaneId(3),
            title: "shell".into(),
            agent: None,
            state: AgentState::Idle,
            exited: None,
            subagents: Vec::new(),
            checkpoints: Vec::new(),
            approval: None,
            source: None,
            sandbox: None,
            issue: None,
            conversation: None,
            sandbox_name: None,
            user_vars: Default::default(),
            cwd: None,
        }]);
        assert_eq!(
            rendered,
            "ID  TITLE  AGENT  STATE  EXIT\n\
             3   shell  -      idle   -\n"
        );
    }

    #[test]
    fn checkpoint_table_lists_newest_first_with_age_and_stat() {
        let start = Checkpoint {
            n: 1,
            commit: "da7330dd4aa56869ad23109aba788bb16deb53af".into(),
            from: None,
            time: 1_000,
            label: "start".into(),
            prompt: None,
            files: 0,
            insertions: 0,
            deletions: 0,
        };
        let turn = Checkpoint {
            n: 2,
            commit: "0fe66d0872aa60dad12f02a7c49e1d220a67229d".into(),
            from: Some(start.commit.clone()),
            time: 1_000 + 3 * 3600,
            label: "done".into(),
            prompt: None,
            files: 1,
            insertions: 2,
            deletions: 1,
        };
        assert_eq!(
            checkpoints(&[start, turn], 1_000 + 3 * 3600 + 30),
            "N   AGE  STAT          COMMIT        LABEL\n\
             #2  now  1 file +2 −1  0fe66d0872aa  done\n\
             #1  3h   -             da7330dd4aa5  start\n"
        );
    }

    #[test]
    fn a_checkpoint_lists_as_the_prompt_that_started_its_turn() {
        let turn = Checkpoint {
            n: 3,
            commit: "0fe66d0872aa60dad12f02a7c49e1d220a67229d".into(),
            from: Some("da7330dd4aa56869ad23109aba788bb16deb53af".into()),
            time: 1_000,
            label: "done".into(),
            prompt: Some("fix the failing sidebar test".into()),
            files: 2,
            insertions: 14,
            deletions: 3,
        };
        assert_eq!(
            checkpoints(&[turn], 1_000),
            "N   AGE  STAT            COMMIT        LABEL\n\
             #3  now  2 files +14 −3  0fe66d0872aa  fix the failing sidebar test\n"
        );
    }

    #[test]
    fn notes_table_lists_newest_first_marking_the_unread() {
        let bell = Note {
            id: 7,
            time: 1_000,
            pane: PaneId(3),
            pane_title: "claude".into(),
            workspace: "api".into(),
            kind: NoteKind::Bell,
            title: None,
            body: None,
            read: true,
        };
        let built = Note {
            id: 8,
            time: 1_000 + 120,
            pane: PaneId(12),
            pane_title: "build".into(),
            workspace: "web".into(),
            kind: NoteKind::Notify,
            title: Some("make".into()),
            body: Some("Built".into()),
            read: false,
        };
        assert_eq!(
            notes(&[bell, built], 1_000 + 150),
            "ID  AGE  PANE  TITLE   WORKSPACE  KIND    UNREAD  TEXT\n\
             8   now  12    build   web        notify  *       make: Built\n\
             7   2m   3     claude  api        bell\n"
        );
        assert_eq!(
            notes(&[], 0),
            "ID  AGE  PANE  TITLE  WORKSPACE  KIND  UNREAD  TEXT\n"
        );
    }

    #[test]
    fn a_restore_reports_what_came_back_and_a_discard_says_so() {
        assert_eq!(restored(3, 2, 0), "restored 3 projects · revived 2 agents");
        assert_eq!(
            restored(1, 0, 1),
            "restored 1 project · revived 0 agents · skipped 1"
        );
        assert_eq!(
            restored(0, 0, 2),
            "restored 0 projects · revived 0 agents · skipped 2",
            "every saved directory gone is still a restore"
        );
        assert_eq!(restored(0, 0, 0), "discarded the saved layout");
    }

    #[test]
    fn age_buckets_into_minutes_hours_and_days() {
        assert_eq!(age(100, 159), "now");
        assert_eq!(age(100, 160), "1m");
        assert_eq!(age(0, 3599), "59m");
        assert_eq!(age(0, 3600), "1h");
        assert_eq!(age(0, 2 * 86_400), "2d");
        assert_eq!(
            age(500, 100),
            "now",
            "a clock that stepped back reads as now"
        );
    }

    #[test]
    fn tab_table_marks_active() {
        let rendered = tabs(&[
            TabInfo {
                id: TabId(1),
                name: "main".into(),
                active: true,
            },
            TabInfo {
                id: TabId(2),
                name: "logs".into(),
                active: false,
            },
        ]);
        assert_eq!(
            rendered,
            "ID  NAME  ACTIVE\n\
             1   main  *\n\
             2   logs\n"
        );
    }

    #[test]
    fn empty_list_renders_header_only() {
        assert_eq!(workspaces(&[]), "ID  NAME  PARENT  DIR\n");
        assert_eq!(projects(&[]), "ID  NAME  DIR\n");
    }
}
