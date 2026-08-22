//! The character grid and its scrollback.
//!
//! # Why a ring
//!
//! The single hottest structural operation in a terminal is "scroll the whole
//! screen up by one line", which happens once per output line. A naive
//! implementation memmoves the entire grid; at 200x50 that is 160 KB of traffic
//! per newline, and `cat`-ing a large file becomes memory-bound.
//!
//! Storage here is a ring of rows. Scrolling the full screen advances an index
//! and clears exactly one row: O(cols) instead of O(rows * cols), and it falls
//! out for free that evicted lines land in scrollback.
//!
//! Sub-region scrolls (`DECSTBM` with margins) cannot use the index trick, so
//! they rotate row handles instead of row contents -- still only moving 32-byte
//! `Row` structs, never the cell arrays.
//!
//! # Damage
//!
//! Every row carries a `dirty` flag. The renderer converts only dirty rows into
//! GPU instances, which keeps the terminal lock held for microseconds on the
//! common "user typed one character" path.

use super::cell::{Cell, CellFlags, Pen};

/// One line of the grid.
#[derive(Clone)]
pub struct Row {
    cells: Vec<Cell>,
    /// Set when this line was continued from the previous one by autowrap.
    /// Needed so copy/paste and (eventually) reflow can rejoin logical lines.
    pub wrapped: bool,
    /// Content changed since the renderer last consumed this row.
    pub dirty: bool,
}

impl Row {
    fn new(cols: usize) -> Row {
        Row {
            cells: vec![Cell::EMPTY; cols],
            wrapped: false,
            dirty: true,
        }
    }

    #[inline]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    #[inline]
    pub fn cells_mut(&mut self) -> &mut [Cell] {
        self.dirty = true;
        &mut self.cells
    }

    /// Reset to blanks carrying `pen`'s colours.
    fn reset(&mut self, pen: &Pen) {
        let blank = Cell::blank(pen);
        self.cells.fill(blank);
        self.wrapped = false;
        self.dirty = true;
    }

    fn resize(&mut self, cols: usize, pen: &Pen) {
        self.cells.resize(cols, Cell::blank(pen));
        self.dirty = true;
    }

    /// Index of the last non-blank cell, plus one. Used to trim trailing
    /// whitespace when copying text out.
    pub fn trimmed_len(&self) -> usize {
        self.cells
            .iter()
            .rposition(|c| !c.is_empty())
            .map_or(0, |i| i + 1)
    }
}

pub struct Grid {
    cols: usize,
    rows: usize,
    /// Maximum number of lines retained above the viewport.
    max_scrollback: usize,

    /// Ring of `rows + max_scrollback` lines.
    storage: Vec<Row>,
    /// Ring index of the first visible line when `display_offset == 0`.
    top: usize,
    /// How many scrollback lines currently sit above `top`.
    history: usize,
    /// Lines scrolled up from the live viewport, 0..=history.
    display_offset: usize,

    /// Set when every row must be re-uploaded (resize, scroll, viewport move).
    full_damage: bool,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Grid {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let cap = rows + max_scrollback;
        Grid {
            cols,
            rows,
            max_scrollback,
            storage: (0..cap).map(|_| Row::new(cols)).collect(),
            top: 0,
            history: 0,
            display_offset: 0,
            full_damage: true,
        }
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[inline]
    fn cap(&self) -> usize {
        self.storage.len()
    }

    #[inline]
    pub fn history_len(&self) -> usize {
        self.history
    }

    #[inline]
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    #[inline]
    pub fn take_full_damage(&mut self) -> bool {
        core::mem::take(&mut self.full_damage)
    }

    #[inline]
    pub fn damage_all(&mut self) {
        self.full_damage = true;
    }

    /// Ring index of viewport line `y`, accounting for the scrollback offset.
    #[inline]
    fn ring_index(&self, y: usize) -> usize {
        let cap = self.cap();
        // `top - display_offset + y`, done in modular arithmetic without going
        // negative.
        (self.top + cap - self.display_offset % cap + y) % cap
    }

    /// Ring index of *live* line `y`, ignoring any scrollback offset. All
    /// writes go through here: output must land on the real screen even while
    /// the user is looking at history.
    #[inline]
    fn live_index(&self, y: usize) -> usize {
        (self.top + y) % self.cap()
    }

    #[inline]
    pub fn row(&self, y: usize) -> &Row {
        &self.storage[self.ring_index(y)]
    }

    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut Row {
        let i = self.live_index(y);
        &mut self.storage[i]
    }

    /// Read-only access to a line by absolute position, where 0 is the oldest
    /// scrollback line and `history + rows - 1` is the bottom of the screen.
    pub fn line(&self, absolute: usize) -> &Row {
        let cap = self.cap();
        let start = (self.top + cap - self.history % cap) % cap;
        &self.storage[(start + absolute) % cap]
    }

    #[inline]
    pub fn total_lines(&self) -> usize {
        self.history + self.rows
    }

    #[inline]
    pub fn cell(&self, x: usize, y: usize) -> Cell {
        self.row(y).cells[x.min(self.cols - 1)]
    }

    #[inline]
    pub fn set_cell(&mut self, x: usize, y: usize, c: Cell) {
        if x < self.cols {
            let i = self.live_index(y);
            let row = &mut self.storage[i];
            row.cells[x] = c;
            row.dirty = true;
        }
    }

    /// Mark a live row dirty without touching its contents.
    #[inline]
    pub fn touch(&mut self, y: usize) {
        if y < self.rows {
            let i = self.live_index(y);
            self.storage[i].dirty = true;
        }
    }

    // ---------------------------------------------------------------------
    // Scrolling
    // ---------------------------------------------------------------------

    /// Scroll lines `[top, bottom]` (inclusive, viewport-relative) up by `n`,
    /// filling from the bottom with blanks.
    ///
    /// When the region covers the whole screen this is the O(1) ring advance
    /// and evicted lines enter scrollback. Otherwise rows are rotated in place
    /// and the evicted content is discarded, matching every other terminal.
    pub fn scroll_up(&mut self, top: usize, bottom: usize, n: usize, pen: &Pen) {
        if n == 0 || top > bottom || bottom >= self.rows {
            return;
        }
        let n = n.min(bottom - top + 1);

        if top == 0 && bottom == self.rows - 1 {
            self.scroll_up_ring(n, pen);
        } else {
            self.scroll_up_region(top, bottom, n, pen);
        }
        self.full_damage = true;
    }

    fn scroll_up_ring(&mut self, n: usize, pen: &Pen) {
        let cap = self.cap();
        for _ in 0..n {
            // The line leaving the top of the viewport becomes history.
            if self.history < self.max_scrollback {
                self.history += 1;
            }
            self.top = (self.top + 1) % cap;

            // The line rotating in at the bottom is the one that just fell out
            // of the far end of the ring; recycle it.
            let bottom = (self.top + self.rows - 1) % cap;
            let cols = self.cols;
            let row = &mut self.storage[bottom];
            if row.cells.len() != cols {
                row.resize(cols, pen);
            }
            row.reset(pen);
        }

        // Following the live screen keeps the user pinned to the bottom; if
        // they have scrolled back, hold their position by growing the offset
        // until it saturates against the history limit.
        if self.display_offset > 0 {
            self.display_offset = (self.display_offset + n).min(self.history);
        }
    }

    fn scroll_up_region(&mut self, top: usize, bottom: usize, n: usize, pen: &Pen) {
        // Rotate row handles, not row contents: each swap moves a 32-byte
        // struct, never the cell array behind it.
        let indices: Vec<usize> = (top..=bottom).map(|y| self.live_index(y)).collect();
        let len = indices.len();
        let mut order: Vec<usize> = indices.clone();
        order.rotate_left(n);

        // Apply the permutation by swapping the underlying `Row`s into place.
        let mut scratch: Vec<Row> = Vec::with_capacity(len);
        for &i in &order {
            scratch.push(core::mem::replace(
                &mut self.storage[i],
                Row {
                    cells: Vec::new(),
                    wrapped: false,
                    dirty: true,
                },
            ));
        }
        for (slot, row) in indices.iter().zip(scratch) {
            self.storage[*slot] = row;
        }

        // Blank the lines rotated in at the bottom.
        for y in (bottom + 1 - n)..=bottom {
            let i = self.live_index(y);
            let cols = self.cols;
            if self.storage[i].cells.len() != cols {
                self.storage[i].resize(cols, pen);
            }
            self.storage[i].reset(pen);
        }
    }

    /// Scroll lines `[top, bottom]` down by `n`, filling from the top.
    /// Never produces scrollback -- reverse scroll discards off the bottom.
    pub fn scroll_down(&mut self, top: usize, bottom: usize, n: usize, pen: &Pen) {
        if n == 0 || top > bottom || bottom >= self.rows {
            return;
        }
        let n = n.min(bottom - top + 1);

        let indices: Vec<usize> = (top..=bottom).map(|y| self.live_index(y)).collect();
        let len = indices.len();
        let mut order: Vec<usize> = indices.clone();
        order.rotate_right(n);

        let mut scratch: Vec<Row> = Vec::with_capacity(len);
        for &i in &order {
            scratch.push(core::mem::replace(
                &mut self.storage[i],
                Row {
                    cells: Vec::new(),
                    wrapped: false,
                    dirty: true,
                },
            ));
        }
        for (slot, row) in indices.iter().zip(scratch) {
            self.storage[*slot] = row;
        }

        for y in top..(top + n) {
            let i = self.live_index(y);
            let cols = self.cols;
            if self.storage[i].cells.len() != cols {
                self.storage[i].resize(cols, pen);
            }
            self.storage[i].reset(pen);
        }
        self.full_damage = true;
    }

    // ---------------------------------------------------------------------
    // Viewport / scrollback navigation
    // ---------------------------------------------------------------------

    /// Move the viewport `delta` lines (positive scrolls towards history).
    /// Returns true if the position actually changed.
    pub fn scroll_display(&mut self, delta: isize) -> bool {
        let new = (self.display_offset as isize + delta).clamp(0, self.history as isize) as usize;
        if new != self.display_offset {
            self.display_offset = new;
            self.full_damage = true;
            true
        } else {
            false
        }
    }

    pub fn scroll_to_bottom(&mut self) -> bool {
        self.scroll_display(-(self.display_offset as isize))
    }

    // ---------------------------------------------------------------------
    // Bulk edits
    // ---------------------------------------------------------------------

    pub fn clear_all(&mut self, pen: &Pen) {
        for y in 0..self.rows {
            let i = self.live_index(y);
            self.storage[i].reset(pen);
        }
        self.full_damage = true;
    }

    /// Drop every scrollback line (`ED 3`).
    pub fn clear_history(&mut self) {
        self.history = 0;
        self.display_offset = 0;
        self.full_damage = true;
    }

    /// Resize without re-wrapping. Used for the alternate screen, whose owner
    /// redraws everything anyway and would be confused by lines moving.
    pub fn resize(&mut self, cols: usize, rows: usize, pen: &Pen) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }

        let old_total = self.history + self.rows;
        let mut lines: Vec<Row> = (0..old_total).map(|i| self.line(i).clone()).collect();
        for row in &mut lines {
            row.resize(cols, pen);
            row.dirty = true;
        }
        self.rebuild(lines, cols, rows);
    }

    /// Resize, re-wrapping logical lines to the new width.
    ///
    /// Physical rows joined by autowrap are rejoined into the logical line the
    /// user actually typed or the program actually printed, then split again at
    /// the new width. Without this, narrowing a window leaves text truncated and
    /// widening it leaves ragged half-empty lines -- the single most visible
    /// difference between a terminal that feels finished and one that does not.
    ///
    /// `cursor` is `(col, row)` in viewport coordinates; the updated position is
    /// returned, since the cursor has to follow its character.
    pub fn reflow(
        &mut self,
        cols: usize,
        rows: usize,
        pen: &Pen,
        cursor: (usize, usize),
    ) -> (usize, usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return cursor;
        }

        let old_total = self.history + self.rows;
        let cursor_line = self.history + cursor.1.min(self.rows - 1);

        // --- gather logical lines -------------------------------------------
        let mut logical: Vec<Vec<Cell>> = Vec::with_capacity(old_total);
        let mut current: Vec<Cell> = Vec::new();
        // Where the cursor sits: (logical line index, offset within it).
        let mut cursor_at = (0usize, 0usize);
        let mut cursor_found = false;

        for i in 0..old_total {
            let row = self.line(i);
            if i == cursor_line {
                cursor_at = (logical.len(), current.len() + cursor.0);
                cursor_found = true;
            }
            current.extend_from_slice(row.cells());

            if !row.wrapped {
                // Trailing blanks are padding, not content: keeping them would
                // re-wrap whitespace onto its own lines. The filler half of a
                // double-width character is also "empty" by that test, but it
                // is structure -- dropping it detaches the pair and the next
                // split puts a wide glyph in the last column with nothing to
                // hold its second cell.
                while current
                    .last()
                    .is_some_and(|c| c.is_empty() && !c.flags.contains(CellFlags::WIDE_SPACER))
                {
                    current.pop();
                }
                logical.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() || logical.is_empty() {
            while current
                .last()
                .is_some_and(|c| c.is_empty() && !c.flags.contains(CellFlags::WIDE_SPACER))
            {
                current.pop();
            }
            logical.push(current);
        }
        if !cursor_found {
            cursor_at = (logical.len().saturating_sub(1), 0);
        }

        // --- split again at the new width -----------------------------------
        let mut lines: Vec<Row> = Vec::with_capacity(logical.len());
        let mut new_cursor: Option<(usize, usize)> = None;

        for (li, cells) in logical.iter().enumerate() {
            let start_row = lines.len();
            let mut pos = 0usize;

            loop {
                let mut take = cols.min(cells.len() - pos);
                // Never split a double-width character across the margin: leave
                // the last column blank and start it on the next row.
                if take == cols
                    && pos + take < cells.len()
                    && cells[pos + take - 1].flags.contains(CellFlags::WIDE)
                {
                    take -= 1;
                }

                let mut row = Row::new(cols);
                row.cells[..take].copy_from_slice(&cells[pos..pos + take]);
                // Every chunk but the last continues into the next one.
                row.wrapped = pos + take < cells.len();
                lines.push(row);

                // Place the cursor once we pass its offset.
                if li == cursor_at.0 && new_cursor.is_none() {
                    let off = cursor_at.1;
                    if off < pos + take || pos + take >= cells.len() {
                        let local = off.saturating_sub(pos).min(cols - 1);
                        new_cursor = Some((local, lines.len() - 1));
                    }
                }

                pos += take;
                if pos >= cells.len() {
                    break;
                }
            }

            // An empty logical line still occupies a row.
            if lines.len() == start_row {
                lines.push(Row::new(cols));
                if li == cursor_at.0 && new_cursor.is_none() {
                    new_cursor = Some((0, lines.len() - 1));
                }
            }
        }

        // The viewport is the last `rows` physical lines; pad if reflow produced
        // fewer than a screenful.
        while lines.len() < rows {
            lines.push(Row::new(cols));
        }

        let (cursor_col, cursor_abs) = new_cursor.unwrap_or((0, lines.len().saturating_sub(1)));

        // --- rebuild --------------------------------------------------------
        let cap = rows + self.max_scrollback;
        let dropped = lines.len().saturating_sub(cap);
        self.rebuild(lines, cols, rows);

        let total = self.history + self.rows;
        let abs = cursor_abs.saturating_sub(dropped).min(total.saturating_sub(1));
        let cursor_row = abs.saturating_sub(self.history).min(rows - 1);
        let _ = pen;

        (cursor_col.min(cols - 1), cursor_row)
    }

    /// Install a fresh set of lines as the ring contents.
    fn rebuild(&mut self, mut lines: Vec<Row>, cols: usize, rows: usize) {
        let cap = rows + self.max_scrollback;
        if lines.len() > cap {
            lines.drain(..lines.len() - cap);
        }
        let history = lines.len().saturating_sub(rows);
        while lines.len() < cap {
            lines.push(Row::new(cols));
        }

        self.storage = lines;
        self.cols = cols;
        self.rows = rows;
        self.history = history;
        self.top = history % cap;
        self.display_offset = 0;
        self.full_damage = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::cell::Pen;

    fn put(g: &mut Grid, y: usize, s: &str) {
        for (x, ch) in s.chars().enumerate() {
            let mut c = Cell::EMPTY;
            c.ch = ch as u32;
            g.set_cell(x, y, c);
        }
    }

    fn line_text(g: &Grid, absolute: usize) -> String {
        g.line(absolute)
            .cells()
            .iter()
            .map(|c| c.scalar().unwrap_or(' '))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn ring_scroll_pushes_into_history() {
        let pen = Pen::default();
        let mut g = Grid::new(8, 3, 10);
        put(&mut g, 0, "aaa");
        put(&mut g, 1, "bbb");
        put(&mut g, 2, "ccc");

        g.scroll_up(0, 2, 1, &pen);
        assert_eq!(g.history_len(), 1);
        // Viewport shifted up by one; the old top line is now history line 0.
        assert_eq!(line_text(&g, 0), "aaa");
        assert_eq!(line_text(&g, 1), "bbb");
        assert_eq!(line_text(&g, 2), "ccc");
        assert_eq!(line_text(&g, 3), "");
    }

    #[test]
    fn history_saturates_at_limit() {
        let pen = Pen::default();
        let mut g = Grid::new(4, 2, 3);
        for i in 0..20 {
            put(&mut g, 1, &format!("{i}"));
            g.scroll_up(0, 1, 1, &pen);
        }
        assert_eq!(g.history_len(), 3);
        assert_eq!(g.total_lines(), 5);
        // The most recent lines survived.
        assert_eq!(line_text(&g, 3), "19");
    }

    #[test]
    fn region_scroll_leaves_history_alone() {
        let pen = Pen::default();
        let mut g = Grid::new(8, 4, 10);
        put(&mut g, 0, "keep");
        put(&mut g, 1, "one");
        put(&mut g, 2, "two");
        put(&mut g, 3, "three");

        g.scroll_up(1, 3, 1, &pen);
        assert_eq!(g.history_len(), 0);
        assert_eq!(line_text(&g, 0), "keep");
        assert_eq!(line_text(&g, 1), "two");
        assert_eq!(line_text(&g, 2), "three");
        assert_eq!(line_text(&g, 3), "");
    }

    #[test]
    fn scroll_down_fills_from_top() {
        let pen = Pen::default();
        let mut g = Grid::new(8, 3, 4);
        put(&mut g, 0, "one");
        put(&mut g, 1, "two");
        put(&mut g, 2, "three");

        g.scroll_down(0, 2, 1, &pen);
        assert_eq!(line_text(&g, 0), "");
        assert_eq!(line_text(&g, 1), "one");
        assert_eq!(line_text(&g, 2), "two");
    }

    #[test]
    fn display_offset_walks_history() {
        let pen = Pen::default();
        let mut g = Grid::new(8, 2, 5);
        for i in 0..5 {
            put(&mut g, 1, &format!("L{i}"));
            g.scroll_up(0, 1, 1, &pen);
        }
        assert_eq!(g.history_len(), 5);
        assert!(g.scroll_display(2));
        assert_eq!(g.display_offset(), 2);
        // Clamps at the top of history.
        g.scroll_display(100);
        assert_eq!(g.display_offset(), 5);
        assert!(g.scroll_to_bottom());
        assert_eq!(g.display_offset(), 0);
    }

    #[test]
    fn resize_preserves_recent_content() {
        let pen = Pen::default();
        let mut g = Grid::new(8, 3, 5);
        put(&mut g, 0, "alpha");
        put(&mut g, 1, "beta");
        put(&mut g, 2, "gamma");

        g.resize(12, 3, &pen);
        assert_eq!(g.cols(), 12);
        assert_eq!(line_text(&g, 0), "alpha");
        assert_eq!(line_text(&g, 2), "gamma");
    }

    #[test]
    fn writes_target_live_screen_while_scrolled_back() {
        let pen = Pen::default();
        let mut g = Grid::new(8, 2, 5);
        put(&mut g, 0, "old");
        g.scroll_up(0, 1, 1, &pen);
        g.scroll_display(1);
        assert_eq!(g.display_offset(), 1);

        // This must land on the live bottom row, not on what is displayed.
        put(&mut g, 1, "new");
        g.scroll_to_bottom();
        assert_eq!(line_text(&g, g.total_lines() - 1), "new");
    }
}

#[cfg(test)]
mod reflow_tests {
    use super::*;
    use crate::term::cell::Pen;

    fn put(g: &mut Grid, y: usize, s: &str) {
        for (x, ch) in s.chars().enumerate() {
            if x >= g.cols() {
                break;
            }
            let mut c = Cell::EMPTY;
            c.ch = ch as u32;
            g.set_cell(x, y, c);
        }
    }

    fn text(g: &Grid, absolute: usize) -> String {
        g.line(absolute)
            .cells()
            .iter()
            .map(|c| c.scalar().unwrap_or(' '))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// A wrapped line joined and re-split when the window gets wider.
    #[test]
    fn widening_rejoins_wrapped_lines() {
        let pen = Pen::default();
        let mut g = Grid::new(10, 4, 20);
        put(&mut g, 0, "abcdefghij");
        g.row_mut(0).wrapped = true;
        put(&mut g, 1, "klmnop");

        g.reflow(20, 4, &pen, (6, 1));

        assert_eq!(text(&g, 0), "abcdefghijklmnop");
        assert!(!g.line(0).wrapped);
    }

    /// ...and split again when it gets narrower.
    #[test]
    fn narrowing_splits_long_lines() {
        let pen = Pen::default();
        let mut g = Grid::new(20, 4, 20);
        put(&mut g, 0, "abcdefghijklmnop");

        g.reflow(10, 4, &pen, (0, 1));

        assert_eq!(text(&g, 0), "abcdefghij");
        assert_eq!(text(&g, 1), "klmnop");
        assert!(g.line(0).wrapped, "the first half must be marked wrapped");
        assert!(!g.line(1).wrapped);
    }

    /// Reflow is lossless: narrow then widen returns the original text.
    #[test]
    fn reflow_round_trips() {
        let pen = Pen::default();
        let mut g = Grid::new(40, 6, 50);
        put(&mut g, 0, "the quick brown fox jumps over it");
        put(&mut g, 1, "second line");

        g.reflow(13, 6, &pen, (0, 2));
        g.reflow(40, 6, &pen, (0, 2));

        assert_eq!(text(&g, 0), "the quick brown fox jumps over it");
        assert_eq!(text(&g, 1), "second line");
    }

    /// Blank lines between content are structure, not padding, and must survive.
    #[test]
    fn blank_lines_are_preserved() {
        let pen = Pen::default();
        let mut g = Grid::new(20, 6, 20);
        put(&mut g, 0, "first");
        put(&mut g, 2, "third");

        g.reflow(30, 6, &pen, (0, 3));

        assert_eq!(text(&g, 0), "first");
        assert_eq!(text(&g, 1), "");
        assert_eq!(text(&g, 2), "third");
    }

    /// The cursor has to follow the character it was sitting on.
    #[test]
    fn cursor_follows_its_position() {
        let pen = Pen::default();
        let mut g = Grid::new(10, 4, 20);
        put(&mut g, 0, "abcdefghij");
        g.row_mut(0).wrapped = true;
        put(&mut g, 1, "klmno");

        // Cursor just past the 'o' on the second physical row.
        let (cx, cy) = g.reflow(20, 4, &pen, (5, 1));
        // After joining, that is column 15 of the single row.
        assert_eq!((cx, cy), (15, 0));
    }

    /// A double-width character must never be split across the margin.
    #[test]
    fn wide_characters_are_not_split() {
        let pen = Pen::default();
        let mut g = Grid::new(20, 4, 20);
        // Nine columns of 'a', then a wide pair straddling column 9-10.
        for x in 0..9 {
            let mut c = Cell::EMPTY;
            c.ch = 'a' as u32;
            g.set_cell(x, 0, c);
        }
        let mut wide = Cell::EMPTY;
        wide.ch = '\u{4f60}' as u32;
        wide.flags = CellFlags::WIDE;
        g.set_cell(9, 0, wide);
        let mut spacer = Cell::EMPTY;
        spacer.flags = CellFlags::WIDE_SPACER;
        g.set_cell(10, 0, spacer);

        g.reflow(10, 4, &pen, (0, 1));

        // Row 0 holds the nine 'a's; the wide char moves down rather than being
        // cut in half.
        assert_eq!(text(&g, 0), "aaaaaaaaa");
        assert!(g.line(1).cells()[0].flags.contains(CellFlags::WIDE));
    }
}
