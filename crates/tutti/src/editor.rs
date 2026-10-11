//! The editor pane a workspace advertises — a pane whose Neovim or Emacs set
//! the `TUTTI_EDITOR` user var — and handing it a file through the editor's
//! own remote CLI. One implementation behind `tutti edit` and the changes
//! panel's `e`.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tutti_core::{PaneId, TabView, WorkspaceId, WorkspaceView};

/// The workspace to look in — `workspace`, else `caller`'s — by name, and its
/// editor pane with that pane's `TUTTI_EDITOR` value: a live pane whose editor
/// set the var, the caller's own tab first, then layout order.
pub fn pane(
    workspaces: &[WorkspaceView],
    caller: Option<PaneId>,
    workspace: Option<WorkspaceId>,
) -> Result<(&str, Option<(PaneId, &str)>)> {
    let holds_caller = |tab: &TabView| tab.panes.iter().any(|p| Some(p.id) == caller);
    let found = match workspace {
        Some(id) => workspaces.iter().find(|w| w.id == id),
        None => workspaces.iter().find(|w| w.tabs.iter().any(holds_caller)),
    };
    let found = found.with_context(|| match (workspace, caller) {
        (Some(id), _) => format!("no workspace {id}"),
        (None, Some(pane)) => format!("no pane {pane} in this session"),
        (None, None) => "not inside a tutti pane: pass --workspace <id>".into(),
    })?;
    let mut tabs: Vec<&TabView> = found.tabs.iter().collect();
    tabs.sort_by_key(|tab| !holds_caller(tab));
    let editor = tabs
        .into_iter()
        .flat_map(|tab| &tab.panes)
        .filter(|p| p.exited.is_none())
        .find_map(|p| Some((p.id, p.user_vars.get("TUTTI_EDITOR")?.as_str())));
    Ok((&found.name, editor))
}

/// Open `path` in the editor a `TUTTI_EDITOR` value names, at `line` and `col`
/// (default 1) when given: `nvim <server address>` through `nvim --server`,
/// `emacs [<server name or socket>]` through `emacsclient -n`. Returns once it
/// has the file. The editor's CLI never touches the caller's terminal: no
/// stdin, its output captured.
pub fn open(editor: &str, path: &Path, line: Option<u32>, col: Option<u32>) -> Result<()> {
    let col = col.unwrap_or(1);
    let mut command = match editor.split_once(' ').unwrap_or((editor, "")) {
        ("nvim", address) if !address.is_empty() => {
            // An expression, not `--remote-send` keys, which would be typed
            // into whatever mode the editor is in. A Vim string doubles its
            // quotes; `fnameescape` guards the command line.
            let path = path.display().to_string().replace('\'', "''");
            let jump = line.map_or_else(String::new, |line| {
                format!(", 'call cursor({line}, {col})'")
            });
            let expr = format!("execute(['drop ' .. fnameescape('{path}'){jump}])");
            let mut command = Command::new("nvim");
            command.args(["--server", address, "--remote-expr", &expr]);
            command
        }
        ("emacs", server) => {
            let mut command = Command::new("emacsclient");
            command.arg("-n");
            if !server.is_empty() {
                command.args(["-s", server]);
            }
            if let Some(line) = line {
                command.arg(format!("+{line}:{col}"));
            }
            command.arg(path);
            command
        }
        _ => bail!(
            "TUTTI_EDITOR {editor:?} names no editor tutti can drive: `nvim <server address>` or `emacs [<server name or socket>]`"
        ),
    };
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} could not open {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tutti_core::{PaneInfo, TabId};

    use super::*;

    /// `edit` finds the editor in the caller's workspace — its own tab first,
    /// then layout order, never an exited one — or in `--workspace`'s.
    #[test]
    fn edit_finds_the_editor_pane_of_the_callers_workspace() {
        let pane_info = |id: u64, editor: Option<&str>, exited: Option<i32>| {
            let mut info: PaneInfo = serde_json::from_str(&format!(
                r#"{{"id":{id},"title":"sh","agent":null,"state":"idle","exited":null}}"#
            ))
            .unwrap();
            if let Some(editor) = editor {
                info.user_vars.insert("TUTTI_EDITOR".into(), editor.into());
            }
            info.exited = exited;
            info
        };
        let tab = |id: u64, panes: Vec<PaneInfo>| TabView {
            id: TabId(id),
            name: id.to_string(),
            active: false,
            layout: None,
            active_pane: None,
            panes,
        };
        let workspace = |id: u64, name: &str, tabs: Vec<TabView>| WorkspaceView {
            id: WorkspaceId(id),
            name: name.into(),
            dir: PathBuf::from("/w"),
            branch: None,
            changes: None,
            changed_files: Vec::new(),
            stale: false,
            parent: None,
            tabs,
        };
        let view = vec![
            workspace(
                1,
                "api",
                vec![
                    tab(
                        1,
                        vec![
                            pane_info(1, Some("emacs"), Some(0)),
                            pane_info(2, Some("nvim /a"), None),
                        ],
                    ),
                    tab(
                        2,
                        vec![
                            pane_info(3, None, None),
                            pane_info(4, Some("nvim /b"), None),
                        ],
                    ),
                ],
            ),
            workspace(2, "web", vec![tab(3, vec![pane_info(5, None, None)])]),
        ];
        let find = |caller: Option<u64>, workspace: Option<u64>| {
            pane(&view, caller.map(PaneId), workspace.map(WorkspaceId))
                .map(|(name, editor)| (name.to_string(), editor.map(|(p, e)| (p.0, e.to_string()))))
                .map_err(|e| e.to_string())
        };
        let found = |name: &str, pane: u64, editor: &str| {
            Ok((name.to_string(), Some((pane, editor.to_string()))))
        };
        assert_eq!(
            find(Some(3), None),
            found("api", 4, "nvim /b"),
            "the caller's tab first"
        );
        assert_eq!(
            find(Some(1), None),
            found("api", 2, "nvim /a"),
            "an exited editor is passed over"
        );
        assert_eq!(
            find(None, Some(1)),
            found("api", 2, "nvim /a"),
            "else layout order"
        );
        assert_eq!(
            find(Some(5), Some(1)),
            found("api", 2, "nvim /a"),
            "--workspace wins"
        );
        assert_eq!(find(Some(5), None), Ok(("web".to_string(), None)));
        assert_eq!(find(None, Some(9)), Err("no workspace 9".to_string()));
        assert_eq!(
            find(Some(9), None),
            Err("no pane 9 in this session".to_string())
        );
        assert!(find(None, None).unwrap_err().contains("--workspace"));
    }
}
