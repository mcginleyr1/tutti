//! Turn a `tutti_core::Layout` tree into concrete ratatui rectangles. Pure
//! geometry: the same computation feeds rendering, the per-pane resize
//! requests, and the split borders a drag grabs, so it is unit-tested in
//! isolation.

use ratatui::layout::Rect;
use tutti_core::{Direction, Layout, PaneId};

/// The rectangle for every pane in `layout` inside `area`, in layout order.
/// When `zoom` names a pane present in the tree, that pane fills `area` and the
/// others are omitted (fullscreen focus).
pub fn pane_rects(layout: &Layout, area: Rect, zoom: Option<PaneId>) -> Vec<(PaneId, Rect)> {
    if let Some(pane) = zoom
        && layout.contains(pane)
    {
        return vec![(pane, area)];
    }
    let mut out = Vec::new();
    split_into(layout, area, &mut out);
    out
}

fn split_into(layout: &Layout, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match layout {
        Layout::Leaf(id) => out.push((*id, area)),
        Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_area(area, *direction, *ratio);
            split_into(first, a, out);
            split_into(second, b, out);
        }
    }
}

/// Where one split's two children meet: what a border drag grabs and moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// The split's axis: `Horizontal` children sit side by side, so the
    /// boundary runs down a column; `Vertical` children stack, so it runs
    /// along a row.
    pub direction: Direction,
    /// The split node's whole rect — a drag's ratio is the pointer's place in
    /// it along the axis.
    pub area: Rect,
    /// The second child's first column (side by side) or row (stacked); the
    /// first child's last one is `at - 1`.
    pub at: u16,
    /// A leaf from each side, `first` in the split's first subtree and
    /// `second` in its second: the pair `PaneSplitRatio` names the split by.
    pub first: PaneId,
    pub second: PaneId,
}

impl Boundary {
    /// The coordinate of `(col, row)` along this boundary's axis.
    pub fn along(&self, col: u16, row: u16) -> u16 {
        match self.direction {
            Direction::Horizontal => col,
            Direction::Vertical => row,
        }
    }

    /// The ratio that moves this boundary to `at` (a coordinate along its
    /// axis), clamped to the bounds the server holds splits to, and where that
    /// ratio really draws the boundary — the arithmetic `pane_rects` lays the
    /// panes out with, so the prediction is what the next layout renders.
    pub fn drag_to(&self, at: u16) -> (f32, u16) {
        let (start, len) = match self.direction {
            Direction::Horizontal => (self.area.x, self.area.width),
            Direction::Vertical => (self.area.y, self.area.height),
        };
        let ratio = (f32::from(at.saturating_sub(start)) / f32::from(len.max(1)))
            .clamp(Layout::MIN_RATIO, Layout::MAX_RATIO);
        let (_, second) = split_area(self.area, self.direction, ratio);
        (ratio, self.along(second.x, second.y))
    }
}

/// Every split's boundary in `layout` laid out in `area`, outermost first,
/// computed with the same split arithmetic as `pane_rects` so a boundary
/// always sits exactly where two of its pane rects meet. Empty while `zoom`
/// fills the area with one pane, as `pane_rects` does.
pub fn boundaries(layout: &Layout, area: Rect, zoom: Option<PaneId>) -> Vec<Boundary> {
    if zoom.is_some_and(|pane| layout.contains(pane)) {
        return Vec::new();
    }
    let mut out = Vec::new();
    boundaries_into(layout, area, &mut out);
    out
}

fn boundaries_into(layout: &Layout, area: Rect, out: &mut Vec<Boundary>) {
    let Layout::Split {
        direction,
        ratio,
        first,
        second,
    } = layout
    else {
        return;
    };
    let (a, b) = split_area(area, *direction, *ratio);
    out.push(Boundary {
        direction: *direction,
        area,
        at: match direction {
            Direction::Horizontal => b.x,
            Direction::Vertical => b.y,
        },
        first: first.panes()[0],
        second: second.panes()[0],
    });
    boundaries_into(first, a, out);
    boundaries_into(second, b, out);
}

/// Divide `area` along `direction`, giving the first child `ratio` of the axis.
/// `Horizontal` splits the width (panes sit side by side); `Vertical` splits the
/// height (panes stack).
fn split_area(area: Rect, direction: Direction, ratio: f32) -> (Rect, Rect) {
    let ratio = ratio.clamp(0.0, 1.0);
    match direction {
        Direction::Horizontal => {
            let first_w = ((area.width as f32) * ratio).round() as u16;
            let first_w = first_w.min(area.width);
            (
                Rect::new(area.x, area.y, first_w, area.height),
                Rect::new(area.x + first_w, area.y, area.width - first_w, area.height),
            )
        }
        Direction::Vertical => {
            let first_h = ((area.height as f32) * ratio).round() as u16;
            let first_h = first_h.min(area.height);
            (
                Rect::new(area.x, area.y, area.width, first_h),
                Rect::new(area.x, area.y + first_h, area.width, area.height - first_h),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn single_leaf_fills_area() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(pane_rects(&leaf(1), area, None), vec![(PaneId(1), area)]);
    }

    #[test]
    fn horizontal_split_divides_width() {
        let rects = pane_rects(
            &split(Direction::Horizontal, 0.5, leaf(1), leaf(2)),
            Rect::new(0, 0, 80, 24),
            None,
        );
        assert_eq!(
            rects,
            vec![
                (PaneId(1), Rect::new(0, 0, 40, 24)),
                (PaneId(2), Rect::new(40, 0, 40, 24)),
            ]
        );
    }

    #[test]
    fn vertical_split_divides_height() {
        let rects = pane_rects(
            &split(Direction::Vertical, 0.5, leaf(1), leaf(2)),
            Rect::new(0, 0, 80, 24),
            None,
        );
        assert_eq!(
            rects,
            vec![
                (PaneId(1), Rect::new(0, 0, 80, 12)),
                (PaneId(2), Rect::new(0, 12, 80, 12)),
            ]
        );
    }

    #[test]
    fn ratio_is_honored_and_widths_tile_exactly() {
        let area = Rect::new(0, 0, 100, 24);
        let rects = pane_rects(
            &split(Direction::Horizontal, 0.3, leaf(1), leaf(2)),
            area,
            None,
        );
        assert_eq!(rects[0].1.width, 30);
        assert_eq!(rects[1].1.width, 70);
        // Children tile the parent with no gap or overlap.
        assert_eq!(rects[0].1.right(), rects[1].1.x);
        assert_eq!(rects[1].1.right(), area.right());
    }

    #[test]
    fn nested_splits_recurse() {
        // Left leaf, right column split into top/bottom.
        let layout = split(
            Direction::Horizontal,
            0.5,
            leaf(1),
            split(Direction::Vertical, 0.5, leaf(2), leaf(3)),
        );
        let rects = pane_rects(&layout, Rect::new(0, 0, 80, 24), None);
        assert_eq!(
            rects,
            vec![
                (PaneId(1), Rect::new(0, 0, 40, 24)),
                (PaneId(2), Rect::new(40, 0, 40, 12)),
                (PaneId(3), Rect::new(40, 12, 40, 12)),
            ]
        );
    }

    #[test]
    fn zoom_fills_area_with_one_pane() {
        let layout = split(Direction::Horizontal, 0.5, leaf(1), leaf(2));
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            pane_rects(&layout, area, Some(PaneId(2))),
            vec![(PaneId(2), area)]
        );
    }

    #[test]
    fn zoom_of_absent_pane_falls_back_to_split() {
        let layout = split(Direction::Horizontal, 0.5, leaf(1), leaf(2));
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(pane_rects(&layout, area, Some(PaneId(9))).len(), 2);
    }

    #[test]
    fn boundaries_list_every_split_outermost_first() {
        // Left leaf, right column split into top/bottom.
        let layout = split(
            Direction::Horizontal,
            0.5,
            leaf(1),
            split(Direction::Vertical, 0.5, leaf(2), leaf(3)),
        );
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            boundaries(&layout, area, None),
            vec![
                Boundary {
                    direction: Direction::Horizontal,
                    area,
                    at: 40,
                    first: PaneId(1),
                    second: PaneId(2),
                },
                Boundary {
                    direction: Direction::Vertical,
                    area: Rect::new(40, 0, 40, 24),
                    at: 12,
                    first: PaneId(2),
                    second: PaneId(3),
                },
            ]
        );
        assert!(boundaries(&leaf(1), area, None).is_empty());
    }

    #[test]
    fn every_boundary_sits_where_its_pane_rects_meet() {
        // Odd sizes and uneven ratios, so any rounding drift would show.
        let column =
            |ratio, top, bottom| split(Direction::Vertical, ratio, leaf(top), leaf(bottom));
        let layout = split(
            Direction::Horizontal,
            0.37,
            column(0.61, 1, 2),
            split(Direction::Horizontal, 0.45, column(0.3, 3, 4), leaf(5)),
        );
        let area = Rect::new(31, 2, 97, 41);
        let rects = pane_rects(&layout, area, None);
        let found = boundaries(&layout, area, None);
        assert_eq!(found.len(), 4);
        let rect_of = |p: PaneId| rects.iter().find(|(id, _)| *id == p).unwrap().1;
        for b in found {
            // A pane rect's start and end along the boundary's axis.
            let span = |r: Rect| match b.direction {
                Direction::Horizontal => (r.x, r.right()),
                Direction::Vertical => (r.y, r.bottom()),
            };
            let inside: Vec<(u16, u16)> = rects
                .iter()
                .filter(|(_, r)| b.area.intersection(*r) == *r)
                .map(|(_, r)| span(*r))
                .collect();
            assert!(
                inside.iter().all(|&(s, e)| e <= b.at || s >= b.at),
                "no pane straddles {b:?}"
            );
            assert!(
                inside.iter().any(|&(_, e)| e == b.at),
                "a pane ends at {b:?}"
            );
            assert!(
                inside.iter().any(|&(s, _)| s == b.at),
                "a pane starts at {b:?}"
            );
            assert!(span(rect_of(b.first)).1 <= b.at, "first lies before {b:?}");
            assert!(span(rect_of(b.second)).0 >= b.at, "second lies after {b:?}");
        }
    }

    #[test]
    fn zoom_hides_every_boundary() {
        let layout = split(Direction::Horizontal, 0.5, leaf(1), leaf(2));
        let area = Rect::new(0, 0, 80, 24);
        assert!(boundaries(&layout, area, Some(PaneId(2))).is_empty());
        assert_eq!(boundaries(&layout, area, Some(PaneId(9))).len(), 1);
    }

    #[test]
    fn a_drag_predicts_the_boundary_the_new_ratio_draws_clamped() {
        let layout = split(Direction::Horizontal, 0.5, leaf(1), leaf(2));
        let area = Rect::new(10, 3, 51, 20);
        let b = boundaries(&layout, area, None)[0];
        assert_eq!(b.at, 36, "51 * 0.5 rounds up to 26 columns");
        for (to, want) in [(30, 30), (36, 36), (11, 15), (200, 56)] {
            let (ratio, at) = b.drag_to(to);
            assert_eq!(at, want, "dragging to {to}");
            assert!((Layout::MIN_RATIO..=Layout::MAX_RATIO).contains(&ratio));
            // The prediction is exactly what the layout renders at that ratio.
            let moved = layout.set_split_ratio(PaneId(1), PaneId(2), ratio).unwrap();
            assert_eq!(boundaries(&moved, area, None)[0].at, at);
        }
        let stacked = split(Direction::Vertical, 0.5, leaf(1), leaf(2));
        let b = boundaries(&stacked, area, None)[0];
        assert_eq!(
            (b.at, b.along(99, 7)),
            (13, 7),
            "a stacked split runs along rows"
        );
        assert_eq!(b.drag_to(8).1, 8);
    }
}
