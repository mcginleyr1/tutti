use serde::{Deserialize, Serialize};

use crate::ids::PaneId;

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
    /// The ratio bounds a split is clamped to when resized — by key or by a
    /// border drag — so neither child can be shrunk to nothing. Public so a
    /// client predicting where a drag lands clamps exactly as the server does.
    pub const MIN_RATIO: f32 = 0.10;
    pub const MAX_RATIO: f32 = 0.90;

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

    /// Adjust the ratio of the nearest ancestor split of `target` whose axis is
    /// `axis`, by `delta`, clamping the result to `[MIN_RATIO, MAX_RATIO]`.
    /// Returns the rebuilt layout, or `None` when `target` has no enclosing
    /// split on that axis. "Nearest" is the split closest to the leaf.
    pub fn resize_split(&self, target: PaneId, axis: Direction, delta: f32) -> Option<Layout> {
        let Layout::Split {
            direction,
            ratio,
            first,
            second,
        } = self
        else {
            return None;
        };
        let into_first = first.contains(target);
        if !into_first && !second.contains(target) {
            return None;
        }
        let child = if into_first { first } else { second };
        // Prefer the deepest matching-axis split on the path to the target.
        if let Some(new_child) = child.resize_split(target, axis, delta) {
            let (first, second) = if into_first {
                (Box::new(new_child), second.clone())
            } else {
                (first.clone(), Box::new(new_child))
            };
            return Some(Layout::Split {
                direction: *direction,
                ratio: *ratio,
                first,
                second,
            });
        }
        if *direction == axis {
            return Some(Layout::Split {
                direction: *direction,
                ratio: (ratio + delta).clamp(Self::MIN_RATIO, Self::MAX_RATIO),
                first: first.clone(),
                second: second.clone(),
            });
        }
        None
    }

    /// Set the ratio of the split that separates `a` from `b` — their lowest
    /// common ancestor, with `a` in its first subtree and `b` in its second —
    /// clamped to `[MIN_RATIO, MAX_RATIO]`. Returns the rebuilt layout, or
    /// `None` when either pane is absent or the order is reversed, so a stale
    /// drag can never move some other split.
    pub fn set_split_ratio(&self, a: PaneId, b: PaneId, ratio: f32) -> Option<Layout> {
        let Layout::Split {
            direction,
            ratio: current,
            first,
            second,
        } = self
        else {
            return None;
        };
        if first.contains(a) && second.contains(b) {
            return Some(Layout::Split {
                direction: *direction,
                ratio: ratio.clamp(Self::MIN_RATIO, Self::MAX_RATIO),
                first: first.clone(),
                second: second.clone(),
            });
        }
        // Not separated here: the split lies deeper, on `b`'s side.
        let (first, second) = if first.contains(b) {
            (
                Box::new(first.set_split_ratio(a, b, ratio)?),
                second.clone(),
            )
        } else {
            (
                first.clone(),
                Box::new(second.set_split_ratio(a, b, ratio)?),
            )
        };
        Some(Layout::Split {
            direction: *direction,
            ratio: *current,
            first,
            second,
        })
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

    fn leaf(id: u64) -> Layout {
        Layout::Leaf(PaneId(id))
    }
    fn split(direction: Direction, ratio: f32, first: Layout, second: Layout) -> Layout {
        Layout::Split {
            direction,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }
    fn ratio_of(layout: &Layout) -> f32 {
        match layout {
            Layout::Split { ratio, .. } => *ratio,
            Layout::Leaf(_) => panic!("expected a split"),
        }
    }

    #[test]
    fn resize_split_adjusts_matching_axis() {
        let layout = split(Direction::Horizontal, 0.5, leaf(1), leaf(2));
        let resized = layout
            .resize_split(PaneId(1), Direction::Horizontal, 0.05)
            .unwrap();
        assert!((ratio_of(&resized) - 0.55).abs() < 1e-6);
    }

    #[test]
    fn resize_split_clamps_to_bounds() {
        let layout = split(Direction::Horizontal, 0.88, leaf(1), leaf(2));
        let resized = layout
            .resize_split(PaneId(2), Direction::Horizontal, 0.10)
            .unwrap();
        assert!((ratio_of(&resized) - 0.90).abs() < 1e-6);
    }

    #[test]
    fn resize_split_ignores_wrong_axis() {
        let layout = split(Direction::Horizontal, 0.5, leaf(1), leaf(2));
        assert!(
            layout
                .resize_split(PaneId(1), Direction::Vertical, 0.05)
                .is_none()
        );
    }

    #[test]
    fn resize_split_targets_nearest_matching_ancestor() {
        // Outer horizontal split; the right child is itself a horizontal split.
        // Resizing horizontally from pane 3 should adjust the inner split, not
        // the outer one.
        let inner = split(Direction::Horizontal, 0.5, leaf(2), leaf(3));
        let layout = split(Direction::Horizontal, 0.5, leaf(1), inner);
        let resized = layout
            .resize_split(PaneId(3), Direction::Horizontal, 0.05)
            .unwrap();
        // Outer ratio unchanged; inner ratio nudged.
        assert!((ratio_of(&resized) - 0.5).abs() < 1e-6);
        if let Layout::Split { second, .. } = &resized {
            assert!((ratio_of(second) - 0.55).abs() < 1e-6);
        } else {
            panic!("expected a split");
        }
    }

    /// The ratios of a two-level layout: the root's, then each child split's
    /// (a leaf child reads as `None`).
    fn ratios(layout: &Layout) -> (f32, Option<f32>, Option<f32>) {
        let Layout::Split {
            ratio,
            first,
            second,
            ..
        } = layout
        else {
            panic!("expected a split");
        };
        let inner = |child: &Layout| matches!(child, Layout::Split { .. }).then(|| ratio_of(child));
        (*ratio, inner(first), inner(second))
    }

    #[test]
    fn set_split_ratio_sets_the_split_separating_the_two_panes() {
        // Two columns, each stacked: 1 over 2 on the left, 3 over 4 on the right.
        let layout = split(
            Direction::Horizontal,
            0.5,
            split(Direction::Vertical, 0.5, leaf(1), leaf(2)),
            split(Direction::Vertical, 0.5, leaf(3), leaf(4)),
        );
        let set = |a, b| layout.set_split_ratio(PaneId(a), PaneId(b), 0.3).unwrap();
        assert_eq!(ratios(&set(1, 2)), (0.5, Some(0.3), Some(0.5)));
        assert_eq!(ratios(&set(3, 4)), (0.5, Some(0.5), Some(0.3)));
        // Panes from different columns meet only at the root, whichever leaf
        // of each side names it.
        assert_eq!(ratios(&set(2, 3)), (0.3, Some(0.5), Some(0.5)));
        assert_eq!(ratios(&set(1, 4)), (0.3, Some(0.5), Some(0.5)));
    }

    #[test]
    fn set_split_ratio_reaches_a_split_nested_beside_a_leaf() {
        let layout = split(
            Direction::Horizontal,
            0.5,
            leaf(1),
            split(Direction::Vertical, 0.5, leaf(2), leaf(3)),
        );
        let inner = layout.set_split_ratio(PaneId(2), PaneId(3), 0.7).unwrap();
        assert_eq!(ratios(&inner), (0.5, None, Some(0.7)));
        let outer = layout.set_split_ratio(PaneId(1), PaneId(3), 0.7).unwrap();
        assert_eq!(ratios(&outer), (0.7, None, Some(0.5)));
    }

    #[test]
    fn set_split_ratio_clamps_to_the_resize_bounds() {
        let layout = split(Direction::Vertical, 0.5, leaf(1), leaf(2));
        let high = layout.set_split_ratio(PaneId(1), PaneId(2), 0.99).unwrap();
        assert!((ratio_of(&high) - Layout::MAX_RATIO).abs() < 1e-6);
        let low = layout.set_split_ratio(PaneId(1), PaneId(2), 0.0).unwrap();
        assert!((ratio_of(&low) - Layout::MIN_RATIO).abs() < 1e-6);
    }

    #[test]
    fn set_split_ratio_refuses_reversed_absent_or_identical_panes() {
        let layout = split(
            Direction::Horizontal,
            0.5,
            leaf(1),
            split(Direction::Vertical, 0.5, leaf(2), leaf(3)),
        );
        let refuses = |a, b| layout.set_split_ratio(PaneId(a), PaneId(b), 0.3).is_none();
        assert!(
            refuses(2, 1),
            "b must sit in the separating split's second subtree"
        );
        assert!(refuses(3, 2), "the order matters at a nested split too");
        assert!(refuses(1, 9), "an absent pane names no split");
        assert!(refuses(9, 1));
        assert!(refuses(2, 2), "one pane is never separated from itself");
        assert!(leaf(1).set_split_ratio(PaneId(1), PaneId(2), 0.3).is_none());
    }
}
