//! Turn a `tutti_core::Layout` tree into concrete ratatui rectangles. Pure
//! geometry: the same computation feeds both rendering and the per-pane resize
//! requests, so it is unit-tested in isolation.

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
}
