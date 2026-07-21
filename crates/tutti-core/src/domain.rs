use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{PaneId, TabId, WorkspaceId};
use crate::state::AgentState;

/// The kind of agent running in a pane. A newtype over `String` keeps the
/// registry data-driven: new agents are added as data, not enum variants.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentKind(pub String);

impl From<&str> for AgentKind {
    fn from(s: &str) -> Self {
        AgentKind(s.to_string())
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub dir: PathBuf,
    pub name: String,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub name: String,
    pub layout: Layout,
    pub active_pane: Option<PaneId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub title: String,
    pub agent: Option<AgentKind>,
    pub state: AgentState,
    pub exited: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Layout {
    Leaf(PaneId),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    /// Replace `target`'s leaf with a split holding `target` and `new_pane`.
    /// A pane that isn't present leaves the layout unchanged.
    pub fn split(&self, target: PaneId, new_pane: PaneId, direction: Direction) -> Layout {
        match self {
            Layout::Leaf(id) if *id == target => Layout::Split {
                direction,
                ratio: 0.5,
                first: Box::new(Layout::Leaf(target)),
                second: Box::new(Layout::Leaf(new_pane)),
            },
            Layout::Leaf(id) => Layout::Leaf(*id),
            Layout::Split {
                direction: d,
                ratio,
                first,
                second,
            } => Layout::Split {
                direction: *d,
                ratio: *ratio,
                first: Box::new(first.split(target, new_pane, direction)),
                second: Box::new(second.split(target, new_pane, direction)),
            },
        }
    }

    /// Remove `pane`, collapsing the split that held it into its sibling.
    /// Returns `None` when the removed pane was the last one in the tree.
    pub fn remove(&self, pane: PaneId) -> Option<Layout> {
        match self {
            Layout::Leaf(id) if *id == pane => None,
            Layout::Leaf(id) => Some(Layout::Leaf(*id)),
            Layout::Split {
                direction,
                ratio,
                first,
                second,
            } => match (first.remove(pane), second.remove(pane)) {
                (None, None) => None,
                (None, Some(sibling)) | (Some(sibling), None) => Some(sibling),
                (Some(first), Some(second)) => Some(Layout::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
            },
        }
    }

    pub fn panes(&self) -> Vec<PaneId> {
        match self {
            Layout::Leaf(id) => vec![*id],
            Layout::Split { first, second, .. } => {
                let mut panes = first.panes();
                panes.extend(second.panes());
                panes
            }
        }
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Layout::Leaf(id) => *id == pane,
            Layout::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_leaf_creates_split() {
        let split = Layout::Leaf(PaneId(1)).split(PaneId(1), PaneId(2), Direction::Vertical);
        assert_eq!(
            split,
            Layout::Split {
                direction: Direction::Vertical,
                ratio: 0.5,
                first: Box::new(Layout::Leaf(PaneId(1))),
                second: Box::new(Layout::Leaf(PaneId(2))),
            }
        );
    }

    #[test]
    fn split_targets_nested_leaf() {
        let layout = Layout::Leaf(PaneId(1))
            .split(PaneId(1), PaneId(2), Direction::Horizontal)
            .split(PaneId(2), PaneId(3), Direction::Vertical);
        assert_eq!(layout.panes(), vec![PaneId(1), PaneId(2), PaneId(3)]);
    }

    #[test]
    fn split_unknown_target_is_noop() {
        let layout = Layout::Leaf(PaneId(1));
        assert_eq!(
            layout.split(PaneId(99), PaneId(2), Direction::Vertical),
            Layout::Leaf(PaneId(1))
        );
    }

    #[test]
    fn remove_collapses_parent_into_sibling() {
        let layout = Layout::Leaf(PaneId(1)).split(PaneId(1), PaneId(2), Direction::Horizontal);
        assert_eq!(layout.remove(PaneId(1)), Some(Layout::Leaf(PaneId(2))));
    }

    #[test]
    fn remove_last_pane_yields_none() {
        assert!(Layout::Leaf(PaneId(1)).remove(PaneId(1)).is_none());
    }

    #[test]
    fn remove_missing_is_noop() {
        let layout = Layout::Leaf(PaneId(1));
        assert_eq!(layout.remove(PaneId(2)), Some(Layout::Leaf(PaneId(1))));
    }

    #[test]
    fn panes_and_contains() {
        let layout = Layout::Leaf(PaneId(1))
            .split(PaneId(1), PaneId(2), Direction::Horizontal)
            .split(PaneId(2), PaneId(3), Direction::Vertical);
        assert_eq!(layout.panes(), vec![PaneId(1), PaneId(2), PaneId(3)]);
        assert!(layout.contains(PaneId(2)));
        assert!(!layout.contains(PaneId(42)));
    }

    #[test]
    fn agent_kind_from_str_and_display() {
        let kind: AgentKind = "claude".into();
        assert_eq!(kind, AgentKind("claude".to_string()));
        assert_eq!(kind.to_string(), "claude");
    }
}
