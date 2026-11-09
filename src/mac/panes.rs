//! Split panes: how a tab's area is divided between its shells.
//!
//! A tab's layout is a tree. A leaf is one pane; a split divides its area in
//! two, side by side or one above the other, at a ratio. Everything is on cell
//! boundaries: each pane gets whole columns and rows, and a split gives one
//! column (or row) to the divider between its halves, so text never sits
//! under a divider and every pane's grid lines up with the window's.

/// Which way a split divides its area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Side by side, with a vertical divider: ⌘D.
    Right,
    /// One above the other, with a horizontal divider: ⌘⇧D.
    Down,
}

/// A rectangle of cells: where a pane or divider is in the window's grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub col: usize,
    pub row: usize,
    pub cols: usize,
    pub rows: usize,
}

impl Rect {
    pub fn contains(&self, col: usize, row: usize) -> bool {
        col >= self.col && col < self.col + self.cols && row >= self.row && row < self.row + self.rows
    }
}

/// A divider between the two halves of a split, for drawing and dragging.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// The column (for `Dir::Right`) or row (`Dir::Down`) it takes.
    pub rect: Rect,
    pub dir: Dir,
    /// The split it belongs to: from the root, `false` for the first half
    /// and `true` for the second.
    pub path: Vec<bool>,
    /// The area that split divides, to turn a drag into a ratio.
    pub area: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(u64),
    Split { dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// The fewest cells a pane is squeezed to, across a split.
const MIN: usize = 2;

impl Node {
    /// Each pane's rectangle and each divider, for `area`.
    pub fn layout(&self, area: Rect) -> (Vec<(u64, Rect)>, Vec<Divider>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.place(area, &mut Vec::new(), &mut panes, &mut dividers);
        (panes, dividers)
    }

    fn place(&self, area: Rect, path: &mut Vec<bool>, panes: &mut Vec<(u64, Rect)>, dividers: &mut Vec<Divider>) {
        match self {
            Node::Leaf(id) => panes.push((*id, area)),
            Node::Split { dir, ratio, a, b } => {
                let len = match dir {
                    Dir::Right => area.cols,
                    Dir::Down => area.rows,
                };
                let (first, gap) = split_len(len, *ratio);
                let (ra, rd, rb) = match dir {
                    Dir::Right => (
                        Rect { cols: first, ..area },
                        Rect { col: area.col + first, cols: gap, ..area },
                        Rect { col: area.col + first + gap, cols: len - first - gap, ..area },
                    ),
                    Dir::Down => (
                        Rect { rows: first, ..area },
                        Rect { row: area.row + first, rows: gap, ..area },
                        Rect { row: area.row + first + gap, rows: len - first - gap, ..area },
                    ),
                };
                if gap > 0 {
                    dividers.push(Divider { rect: rd, dir: *dir, path: path.clone(), area });
                }
                path.push(false);
                a.place(ra, path, panes, dividers);
                path.pop();
                path.push(true);
                b.place(rb, path, panes, dividers);
                path.pop();
            }
        }
    }

    /// Split the pane `target` in two, `new` taking the second half.
    pub fn split(&mut self, target: u64, new: u64, dir: Dir) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split { dir, ratio: 0.5, a: Box::new(Node::Leaf(target)), b: Box::new(Node::Leaf(new)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, dir) || b.split(target, new, dir),
        }
    }

    /// Take the pane `target` out; its split's other half takes the whole
    /// area. Returns the pane that should get focus: the first in that half.
    /// The last pane can't be removed.
    pub fn remove(&mut self, target: u64) -> Option<u64> {
        let Node::Split { a, b, .. } = self else {
            return None;
        };
        let keep = if **a == Node::Leaf(target) {
            Some((**b).clone())
        } else if **b == Node::Leaf(target) {
            Some((**a).clone())
        } else {
            None
        };
        match keep {
            Some(sibling) => {
                *self = sibling;
                Some(self.first())
            }
            None => a.remove(target).or_else(|| b.remove(target)),
        }
    }

    /// The first pane, in reading order.
    pub fn first(&self) -> u64 {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { a, .. } => a.first(),
        }
    }

    /// Set the ratio of the split at `path`.
    pub fn set_ratio(&mut self, path: &[bool], to: f32) {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => *ratio = to.clamp(0.0, 1.0),
            (Node::Split { a, b, .. }, Some((&second, rest))) => {
                if second { b.set_ratio(rest, to) } else { a.set_ratio(rest, to) }
            }
            _ => {}
        }
    }
}

/// How `len` cells divide at `ratio`: the first half's cells and the
/// divider's. Each half keeps at least `MIN` cells where there's room; with
/// no room for a divider, the first half takes everything.
fn split_len(len: usize, ratio: f32) -> (usize, usize) {
    if len < 3 {
        return (len, 0);
    }
    let avail = len - 1;
    let first = (ratio * avail as f32).round() as usize;
    let lo = MIN.min(avail / 2).max(1);
    let hi = avail - lo;
    (first.clamp(lo, hi.max(lo)), 1)
}

/// The ratio that puts a split's divider at cell `at` of `area`, where a
/// drag ended.
pub fn ratio_at(divider: &Divider, col: usize, row: usize) -> f32 {
    let (start, len, at) = match divider.dir {
        Dir::Right => (divider.area.col, divider.area.cols, col),
        Dir::Down => (divider.area.row, divider.area.rows, row),
    };
    if len < 3 {
        return 0.5;
    }
    (at.saturating_sub(start) as f32 / (len - 1) as f32).clamp(0.0, 1.0)
}

/// Where focus goes from the pane at `from` in direction (`dx`, `dy`): the
/// nearest pane on that side that overlaps it across, the one overlapping
/// most when two are as near.
pub fn neighbor(panes: &[(u64, Rect)], from: Rect, dx: i32, dy: i32) -> Option<u64> {
    let overlap = |a0: usize, a1: usize, b0: usize, b1: usize| a1.min(b1).saturating_sub(a0.max(b0));
    panes
        .iter()
        .filter_map(|&(id, r)| {
            let (gap, across) = if dx > 0 && r.col >= from.col + from.cols {
                (r.col - (from.col + from.cols), overlap(r.row, r.row + r.rows, from.row, from.row + from.rows))
            } else if dx < 0 && r.col + r.cols <= from.col {
                (from.col - (r.col + r.cols), overlap(r.row, r.row + r.rows, from.row, from.row + from.rows))
            } else if dy > 0 && r.row >= from.row + from.rows {
                (r.row - (from.row + from.rows), overlap(r.col, r.col + r.cols, from.col, from.col + from.cols))
            } else if dy < 0 && r.row + r.rows <= from.row {
                (from.row - (r.row + r.rows), overlap(r.col, r.col + r.cols, from.col, from.col + from.cols))
            } else {
                return None;
            };
            (across > 0).then_some((gap, std::cmp::Reverse(across), id))
        })
        .min()
        .map(|(_, _, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { col: 0, row: 0, cols: 81, rows: 24 };

    #[test]
    fn one_pane_takes_the_whole_area() {
        let (panes, dividers) = Node::Leaf(1).layout(AREA);
        assert_eq!(panes, vec![(1, AREA)]);
        assert!(dividers.is_empty());
    }

    #[test]
    fn splits_land_on_cells_with_a_divider_between() {
        let mut t = Node::Leaf(1);
        assert!(t.split(1, 2, Dir::Right));
        let (panes, dividers) = t.layout(AREA);
        // 81 columns: 40, the divider, 40.
        assert_eq!(panes[0], (1, Rect { col: 0, row: 0, cols: 40, rows: 24 }));
        assert_eq!(panes[1], (2, Rect { col: 41, row: 0, cols: 40, rows: 24 }));
        assert_eq!(dividers[0].rect, Rect { col: 40, row: 0, cols: 1, rows: 24 });

        assert!(t.split(2, 3, Dir::Down));
        let (panes, dividers) = t.layout(AREA);
        assert_eq!(panes[1], (2, Rect { col: 41, row: 0, cols: 40, rows: 12 }));
        assert_eq!(panes[2], (3, Rect { col: 41, row: 13, cols: 40, rows: 11 }));
        assert_eq!(dividers[1].rect, Rect { col: 41, row: 12, cols: 40, rows: 1 });
        assert_eq!(dividers[1].path, vec![true]);
        // Every cell is a pane's or a divider's, once.
        let cells: usize = panes.iter().map(|(_, r)| r.cols * r.rows).sum::<usize>()
            + dividers.iter().map(|d| d.rect.cols * d.rect.rows).sum::<usize>();
        assert_eq!(cells, AREA.cols * AREA.rows);
    }

    #[test]
    fn removing_a_pane_gives_its_area_back() {
        let mut t = Node::Leaf(1);
        t.split(1, 2, Dir::Right);
        t.split(2, 3, Dir::Down);
        assert_eq!(t.remove(2), Some(3));
        assert_eq!(t.layout(AREA).0[1], (3, Rect { col: 41, row: 0, cols: 40, rows: 24 }));
        assert_eq!(t.remove(1), Some(3));
        assert_eq!(t, Node::Leaf(3));
        assert_eq!(t.remove(3), None, "the last pane stays");
    }

    #[test]
    fn dragging_a_divider_moves_it_and_keeps_panes_usable() {
        let mut t = Node::Leaf(1);
        t.split(1, 2, Dir::Right);
        let d = t.layout(AREA).1[0].clone();
        t.set_ratio(&d.path, ratio_at(&d, 20, 5));
        assert_eq!(t.layout(AREA).1[0].rect.col, 20);
        // Dragged to the edge, a pane keeps two columns.
        t.set_ratio(&d.path, ratio_at(&d, 0, 5));
        assert_eq!(t.layout(AREA).0[0].1.cols, 2);
        t.set_ratio(&d.path, ratio_at(&d, 80, 5));
        assert_eq!(t.layout(AREA).0[1].1.cols, 2);
    }

    #[test]
    fn tiny_areas_still_lay_out() {
        let mut t = Node::Leaf(1);
        t.split(1, 2, Dir::Right);
        let (panes, _) = t.layout(Rect { cols: 2, ..AREA });
        assert_eq!(panes[0].1.cols, 2);
        assert_eq!(panes[1].1.cols, 0);
        let (panes, _) = t.layout(Rect { cols: 4, ..AREA });
        assert!(panes.iter().all(|(_, r)| r.cols >= 1));
    }

    #[test]
    fn focus_moves_to_the_pane_beside() {
        // 1 | 2
        //   | 3
        let mut t = Node::Leaf(1);
        t.split(1, 2, Dir::Right);
        t.split(2, 3, Dir::Down);
        let (panes, _) = t.layout(AREA);
        let rect = |id| panes.iter().find(|p| p.0 == id).unwrap().1;
        assert_eq!(neighbor(&panes, rect(1), 1, 0), Some(2), "the most overlap wins");
        assert_eq!(neighbor(&panes, rect(3), -1, 0), Some(1));
        assert_eq!(neighbor(&panes, rect(2), 0, 1), Some(3));
        assert_eq!(neighbor(&panes, rect(3), 0, -1), Some(2));
        assert_eq!(neighbor(&panes, rect(1), -1, 0), None);
    }
}
