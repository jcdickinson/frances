//! f32 → cell-integer conversion helpers shared by [`super::flex::Flex`]
//! and [`super::grid::Grid`]. Taffy operates in `f32`; the widget
//! tree is integer-cell. We truncate at the taffy boundary so the
//! `Widget` trait stays clean of taffy types.

use ratatui::layout::Rect;
use taffy::Layout;

/// Convert a taffy [`Layout`] (its `location` is relative to its
/// parent's content box) into an absolute cell-grid [`Rect`] within
/// `parent`. Floats are truncated to integer cells (`floor` on
/// `location` + `size`).
///
/// The returned rect is clamped to `parent`'s extent so a child
/// whose taffy layout rounded outside the parent won't paint into
/// a sibling's cells.
pub fn to_cell_rect(layout: &Layout, parent: Rect) -> Rect {
    let x = parent
        .x
        .saturating_add(layout.location.x.max(0.0).floor() as u16);
    let y = parent
        .y
        .saturating_add(layout.location.y.max(0.0).floor() as u16);
    let w = layout.size.width.max(0.0).floor() as u16;
    let h = layout.size.height.max(0.0).floor() as u16;
    let max_x = parent.x.saturating_add(parent.width);
    let max_y = parent.y.saturating_add(parent.height);
    let w = w.min(max_x.saturating_sub(x));
    let h = h.min(max_y.saturating_sub(y));
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use taffy::geometry::{Point, Size};

    fn layout(x: f32, y: f32, w: f32, h: f32) -> Layout {
        Layout {
            order: 0,
            location: Point { x, y },
            size: Size {
                width: w,
                height: h,
            },
            content_size: Size {
                width: 0.0,
                height: 0.0,
            },
            scrollbar_size: Size {
                width: 0.0,
                height: 0.0,
            },
            border: taffy::Rect {
                left: 0.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
            padding: taffy::Rect {
                left: 0.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
            margin: taffy::Rect {
                left: 0.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
        }
    }

    #[test]
    fn translates_into_parent_coords() {
        let parent = Rect::new(10, 5, 20, 10);
        let l = layout(2.0, 3.0, 5.0, 4.0);
        assert_eq!(to_cell_rect(&l, parent), Rect::new(12, 8, 5, 4));
    }

    #[test]
    fn clamps_overflow_to_parent_extent() {
        let parent = Rect::new(0, 0, 10, 5);
        // Child wants to extend to x=15, y=8 — clamped.
        let l = layout(7.0, 4.0, 10.0, 10.0);
        let r = to_cell_rect(&l, parent);
        assert_eq!(r.x, 7);
        assert_eq!(r.y, 4);
        assert_eq!(r.x + r.width, parent.x + parent.width);
        assert_eq!(r.y + r.height, parent.y + parent.height);
    }

    #[test]
    fn fractional_values_truncate() {
        let parent = Rect::new(0, 0, 100, 100);
        let l = layout(1.7, 2.4, 3.9, 4.1);
        let r = to_cell_rect(&l, parent);
        assert_eq!(r.x, 1);
        assert_eq!(r.y, 2);
        assert_eq!(r.width, 3);
        assert_eq!(r.height, 4);
    }
}
