use tutti_core::{AgentState, Checkpoint, PaneInfo, TabInfo, WorkspaceInfo};

pub fn workspaces(items: &[WorkspaceInfo]) -> String {
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

/// A checkpoint's turn stat (`2 files +14 −3`), or `-` for the first state
/// recorded for a pane, which has nothing earlier to diff against.
fn turn_stat(c: &Checkpoint) -> String {
    if c.from.is_none() {
        return "-".into();
    }
    let unit = if c.files == 1 { "file" } else { "files" };
    format!("{} {unit} +{} −{}", c.files, c.insertions, c.deletions)
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

    #[test]
    fn workspace_table_aligns_columns() {
        let rendered = workspaces(&[
            WorkspaceInfo {
                id: WorkspaceId(1),
                name: "api".into(),
                dir: "/srv/api".into(),
            },
            WorkspaceInfo {
                id: WorkspaceId(22),
                name: "frontend".into(),
                dir: "/srv/web".into(),
            },
        ]);
        assert_eq!(
            rendered,
            "ID  NAME      DIR\n\
             1   api       /srv/api\n\
             22  frontend  /srv/web\n"
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
            source: None,
            sandbox: None,
            conversation: None,
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
        assert_eq!(workspaces(&[]), "ID  NAME  DIR\n");
    }
}
