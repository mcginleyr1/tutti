use tutti_core::{AgentState, PaneInfo, TabInfo, WorkspaceInfo};

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

fn state_label(state: AgentState) -> &'static str {
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
        }]);
        assert_eq!(
            rendered,
            "ID  TITLE  AGENT  STATE  EXIT\n\
             3   shell  -      idle   -\n"
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
