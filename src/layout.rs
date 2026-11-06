//! Split layout: a binary tree whose leaves are windows.
//!
//! Kept free of terminal state so the whole thing is unit-testable: give it a `Rect`, get back one
//! rectangle per window, with a single separator cell between siblings.

use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// One above the other (`:sp`).
    Stacked,
    /// Side by side (`:vsp`).
    SideBySide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    Leaf(usize),
    Split {
        dir: SplitDir,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    pub fn leaf(window: usize) -> Self {
        Layout::Leaf(window)
    }

    /// Every window in the tree, left to right and top to bottom.
    pub fn leaves(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.walk(&mut |window, _| out.push(window));
        out
    }

    /// Fill `out` with one rectangle per window, carving a one-cell separator between siblings.
    pub fn rects(&self, area: Rect, out: &mut Vec<(usize, Rect)>) {
        match self {
            Layout::Leaf(window) => out.push((*window, area)),
            Layout::Split { dir, first, second } => {
                let (a, b) = divide(area, *dir);
                first.rects(a, out);
                second.rects(b, out);
            }
        }
    }

    /// Replace `window`'s leaf with a split of the old window and `new_window`.
    pub fn split_leaf(&mut self, window: usize, dir: SplitDir, new_window: usize) -> bool {
        match self {
            Layout::Leaf(existing) if *existing == window => {
                let old = Layout::Leaf(window);
                *self = Layout::Split {
                    dir,
                    first: Box::new(old),
                    second: Box::new(Layout::Leaf(new_window)),
                };
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split { first, second, .. } => {
                first.split_leaf(window, dir, new_window)
                    || second.split_leaf(window, dir, new_window)
            }
        }
    }

    /// Drop a window's leaf, collapsing its parent split. False when the window is not here or is
    /// the last one — an editor always has somewhere to draw.
    pub fn remove_leaf(&mut self, window: usize) -> bool {
        if self.leaves().len() <= 1 || !self.leaves().contains(&window) {
            return false;
        }
        let taken = std::mem::replace(self, Layout::Leaf(usize::MAX));
        match without(taken, window) {
            Some(layout) => {
                *self = layout;
                true
            }
            None => false,
        }
    }

    fn walk(&self, visit: &mut impl FnMut(usize, &Layout)) {
        match self {
            Layout::Leaf(window) => visit(*window, self),
            Layout::Split { first, second, .. } => {
                first.walk(visit);
                second.walk(visit);
            }
        }
    }
}

fn without(layout: Layout, window: usize) -> Option<Layout> {
    match layout {
        Layout::Leaf(found) => {
            if found == window {
                None
            } else {
                Some(Layout::Leaf(found))
            }
        }
        Layout::Split { dir, first, second } => {
            match (without(*first, window), without(*second, window)) {
                (Some(a), Some(b)) => Some(Layout::Split {
                    dir,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        }
    }
}

/// Split an area 50/50 with one cell of separator between the halves.
fn divide(area: Rect, dir: SplitDir) -> (Rect, Rect) {
    match dir {
        SplitDir::SideBySide => {
            let available = area.width.saturating_sub(1);
            let first_width = available / 2;
            let first = Rect {
                width: first_width,
                ..area
            };
            let second = Rect {
                x: area.x + first_width + 1,
                width: available - first_width,
                ..area
            };
            (first, second)
        }
        SplitDir::Stacked => {
            let available = area.height.saturating_sub(1);
            let first_height = available / 2;
            let first = Rect {
                height: first_height,
                ..area
            };
            let second = Rect {
                y: area.y + first_height + 1,
                height: available - first_height,
                ..area
            };
            (first, second)
        }
    }
}

/// Which rectangle the focus should move to when going `dir` from `from`.
///
/// Picks the adjacent rectangle in that direction with the largest overlap on the other axis, so
/// pressing `j` in a split layout lands in the window that is actually below the cursor.
pub fn neighbor(
    rects: &[(usize, Rect)],
    from: usize,
    direction: char,
) -> Option<usize> {
    let current = rects.iter().find(|(id, _)| *id == from)?.1;
    let mut best: Option<(usize, u32)> = None;

    for (id, rect) in rects {
        if *id == from {
            continue;
        }
        let adjacent = match direction {
            'h' => rect.x + rect.width <= current.x,
            'l' => rect.x >= current.x + current.width,
            'k' => rect.y + rect.height <= current.y,
            'j' => rect.y >= current.y + current.height,
            _ => false,
        };
        if !adjacent {
            continue;
        }
        let overlap = match direction {
            'h' | 'l' => vertical_overlap(current, *rect),
            _ => horizontal_overlap(current, *rect),
        };
        if overlap == 0 {
            continue;
        }
        if best.is_none_or(|(_, best_overlap)| overlap > best_overlap) {
            best = Some((*id, overlap));
        }
    }
    best.map(|(id, _)| id)
}

fn vertical_overlap(a: Rect, b: Rect) -> u32 {
    let start = a.y.max(b.y) as u32;
    let end = (a.y + a.height).min(b.y + b.height) as u32;
    end.saturating_sub(start)
}

fn horizontal_overlap(a: Rect, b: Rect) -> u32 {
    let start = a.x.max(b.x) as u32;
    let end = (a.x + a.width).min(b.x + b.width) as u32;
    end.saturating_sub(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        }
    }

    #[test]
    fn a_single_window_gets_the_whole_area() {
        let layout = Layout::leaf(0);
        let mut rects = Vec::new();
        layout.rects(area(), &mut rects);
        assert_eq!(rects, vec![(0, area())]);
    }

    #[test]
    fn side_by_side_splits_leave_one_column_between() {
        let mut layout = Layout::leaf(0);
        assert!(layout.split_leaf(0, SplitDir::SideBySide, 1));
        let mut rects = Vec::new();
        layout.rects(area(), &mut rects);
        let left = rects.iter().find(|(id, _)| *id == 0).unwrap().1;
        let right = rects.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(left.x, 0);
        assert_eq!(left.width + 1 + right.width, 20);
        assert_eq!(right.x, left.x + left.width + 1);
    }

    #[test]
    fn stacked_splits_leave_one_row_between() {
        let mut layout = Layout::leaf(0);
        assert!(layout.split_leaf(0, SplitDir::Stacked, 1));
        let mut rects = Vec::new();
        layout.rects(area(), &mut rects);
        let top = rects.iter().find(|(id, _)| *id == 0).unwrap().1;
        let bottom = rects.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(top.height + 1 + bottom.height, 10);
        assert_eq!(bottom.y, top.y + top.height + 1);
    }

    #[test]
    fn nested_splits_stay_inside_the_area() {
        let mut layout = Layout::leaf(0);
        layout.split_leaf(0, SplitDir::SideBySide, 1);
        layout.split_leaf(1, SplitDir::Stacked, 2);
        let mut rects = Vec::new();
        layout.rects(area(), &mut rects);
        assert_eq!(rects.len(), 3);
        for (_, rect) in &rects {
            assert!(rect.x + rect.width <= 20);
            assert!(rect.y + rect.height <= 10);
        }
    }

    #[test]
    fn removing_a_split_returns_its_sibling() {
        let mut layout = Layout::leaf(0);
        layout.split_leaf(0, SplitDir::SideBySide, 1);
        assert!(layout.remove_leaf(1));
        assert_eq!(layout, Layout::Leaf(0));
    }

    #[test]
    fn the_last_window_cannot_be_removed() {
        let mut layout = Layout::leaf(0);
        assert!(!layout.remove_leaf(0));
    }

    #[test]
    fn neighbour_looks_in_the_pressed_direction_only() {
        let mut layout = Layout::leaf(0);
        layout.split_leaf(0, SplitDir::SideBySide, 1);
        let mut rects = Vec::new();
        layout.rects(area(), &mut rects);

        assert_eq!(neighbor(&rects, 0, 'l'), Some(1));
        assert_eq!(neighbor(&rects, 1, 'h'), Some(0));
        assert_eq!(neighbor(&rects, 0, 'k'), None);
        assert_eq!(neighbor(&rects, 0, 'j'), None);
    }

    #[test]
    fn neighbour_requires_overlap_on_the_other_axis() {
        let rects = vec![
            (
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 4,
                },
            ),
            (
                1,
                Rect {
                    x: 11,
                    y: 5,
                    width: 9,
                    height: 4,
                },
            ),
        ];
        assert_eq!(neighbor(&rects, 0, 'l'), None, "no vertical overlap");
    }
}
