//! The character grid and its scrollback.
//!
//! # Screen and history
//!
//! The single hottest structural operation in a terminal is "scroll the whole
//! screen up by one line", which happens once per output line. The screen is a
//! ring of exactly `rows` rows: scrolling advances an index and clears one row,
//! O(cols) instead of O(rows * cols).
//!
//! The line that leaves the top goes into scrollback *packed*: its characters,
//! one byte each when the line is plain ASCII, plus runs of style, with trailing
//! blank cells dropped. A typical 70-character log line takes about 140 bytes
//! instead of 200 cells x 16 bytes. That is not only a memory saving. Measured
//! on an M2 Pro, a 10,000-line ring of full-width rows (32 MB) cost about 30%
//! of throughput, because every scroll recycled a row that had long left the
//! cache. With packed history, scrolling only ever touches the screen's rows,
//! which stay hot.
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

use std::borrow::Cow;
use std::collections::VecDeque;

use super::cell::{Cell, CellFlags, Pen};
use super::color::Color;

/// Shell-integration marks on a line (OSC 133): which command it belongs to
/// and whether that command's prompt, input or output starts here. Stored on
/// the row so it travels with its text through scrolling, packing and reflow.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct RowMark {
    pub cmd: u32,
    pub kinds: u8,
}

impl RowMark {
    pub const PROMPT: u8 = 1;
    pub const INPUT: u8 = 2;
    pub const OUTPUT: u8 = 4;
    /// Rerun diff: this line differs from the last run's (CHANGED), or the
    /// last run had nothing like it (NEW).
    pub const CHANGED: u8 = 8;
    pub const NEW: u8 = 16;

    #[inline]
    pub fn is_some(&self) -> bool {
        self.kinds != 0
    }

    #[inline]
    pub fn has(&self, kind: u8) -> bool {
        self.kinds & kind != 0
    }
}

/// One line of the grid.
#[derive(Clone)]
pub struct Row {
    cells: Vec<Cell>,
    /// Set when this line was continued from the previous one by autowrap.
    /// Needed so copy/paste and (eventually) reflow can rejoin logical lines.
    pub wrapped: bool,
    /// Content changed since the renderer last consumed this row.
    pub dirty: bool,
    pub mark: RowMark,
}

impl Row {
    fn new(cols: usize) -> Row {
        Row {
            cells: vec![Cell::EMPTY; cols],
            wrapped: false,
            dirty: true,
            mark: RowMark::default(),
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
        if blank == Cell::EMPTY {
            // A constant all-zero fill compiles to memset, which aarch64
            // clears a cache line at a time.
            self.cells.fill(Cell::EMPTY);
        } else {
            self.cells.fill(blank);
        }
        self.wrapped = false;
        self.mark = RowMark::default();
        self.dirty = true;
    }

    /// Reset to blanks, knowing cells from `len` on are already
    /// `Cell::EMPTY`. With the default pen only the content needs clearing;
    /// for a short line on a wide screen that is most of the work saved.
    fn reset_from_content(&mut self, len: usize, pen: &Pen) {
        let blank = Cell::blank(pen);
        if blank == Cell::EMPTY {
            self.cells[..len].fill(Cell::EMPTY);
        } else {
            self.cells.fill(blank);
        }
        self.wrapped = false;
        self.mark = RowMark::default();
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

/// A scrollback line in compact form, in a single allocation:
///
/// ```text
///   text   len bytes (all ASCII) or len x u32 (anything else)
///   runs   nruns x 14 bytes: start u16, fg u32, bg u32, flags|underline u32
/// ```
///
/// A run's style lasts until the next run starts. Trailing cells identical to
/// a fresh blank are dropped. Lossless: unpacking gives back exactly the cells
/// that were packed, at any width at least as wide as the content.
#[derive(Clone, Debug)]
pub struct PackedRow {
    /// A `Vec` rather than a boxed slice so an evicted line's buffer can be
    /// reused for the next one: once scrollback is full, packing allocates
    /// nothing.
    data: Vec<u8>,
    len: u16,
    runs: u16,
    wide: bool,
    wrapped: bool,
    mark: RowMark,
}

const RUN_BYTES: usize = 14;

/// A cell as its four 32-bit words: ch, fg, bg, flags | underline << 16. The
/// layout is pinned by `#[repr(C)]` and the 16-byte size assertion on `Cell`.
#[inline]
fn words(cells: &[Cell]) -> &[u32] {
    // SAFETY: Cell is repr(C), 16 bytes, 4-aligned, made only of integers.
    unsafe { core::slice::from_raw_parts(cells.as_ptr() as *const u32, cells.len() * 4) }
}

impl PackedRow {
    pub fn pack(row: &Row) -> PackedRow {
        Self::pack_into(row, Vec::new())
    }

    /// Pack `row`, reusing `buf`'s allocation when it is large enough.
    pub fn pack_into(row: &Row, mut data: Vec<u8>) -> PackedRow {
        let cells = &row.cells;
        let len = content_len(cells);
        let w = words(&cells[..len]);
        data.clear();
        if len == 0 {
            return PackedRow { data, len: 0, runs: 0, wide: false, wrapped: row.wrapped, mark: row.mark };
        }

        // Most lines are one style and all ASCII: one byte per cell plus a
        // single run. Checked and narrowed in one pass.
        data.resize(len + RUN_BYTES, 0);
        if uniform_ascii(w, &mut data[..len]) {
            put_run(&mut data[len..], 0, &w[1..4]);
            return PackedRow { data, len: len as u16, runs: 1, wide: false, wrapped: row.wrapped, mark: row.mark };
        }

        // General case: several styles, or characters beyond ASCII.
        let wide = cells[..len].iter().any(|c| c.ch >= 0x80);
        let mut starts: Vec<u16> = vec![0];
        for i in 1..len {
            if w[i * 4 + 1..i * 4 + 4] != w[(i - 1) * 4 + 1..(i - 1) * 4 + 4] {
                starts.push(i as u16);
            }
        }
        let text_bytes = if wide { len * 4 } else { len };
        data.clear();
        data.resize(text_bytes + starts.len() * RUN_BYTES, 0);
        for (i, c) in cells[..len].iter().enumerate() {
            if wide {
                data[i * 4..i * 4 + 4].copy_from_slice(&c.ch.to_le_bytes());
            } else {
                data[i] = c.ch as u8;
            }
        }
        for (k, &start) in starts.iter().enumerate() {
            let o = text_bytes + k * RUN_BYTES;
            let i = start as usize * 4;
            put_run(&mut data[o..o + RUN_BYTES], start, &w[i + 1..i + 4]);
        }
        PackedRow {
            data,
            len: len as u16,
            runs: starts.len() as u16,
            wide,
            wrapped: row.wrapped,
            mark: row.mark,
        }
    }

    pub fn unpack(&self, cols: usize) -> Row {
        let len = self.len as usize;
        let mut cells = vec![Cell::EMPTY; cols.max(len)];
        let text_bytes = if self.wide { len * 4 } else { len };
        let runs = &self.data[text_bytes..];
        let run_at = |k: usize| -> (usize, Cell) {
            let r = &runs[k * RUN_BYTES..(k + 1) * RUN_BYTES];
            let u = |o: usize| u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
            let fu = u(10);
            let style = Cell {
                ch: 0,
                fg: Color(u(2)),
                bg: Color(u(6)),
                flags: CellFlags::from_bits_retain(fu as u16),
                underline: (fu >> 16) as u16,
            };
            (u16::from_le_bytes([r[0], r[1]]) as usize, style)
        };
        for k in 0..self.runs as usize {
            let (start, style) = run_at(k);
            let end = if k + 1 < self.runs as usize { run_at(k + 1).0 } else { len };
            for (i, cell) in cells[start..end].iter_mut().enumerate() {
                let i = start + i;
                *cell = style;
                cell.ch = if self.wide {
                    u32::from_le_bytes([self.data[i * 4], self.data[i * 4 + 1], self.data[i * 4 + 2], self.data[i * 4 + 3]])
                } else {
                    self.data[i] as u32
                };
            }
        }
        cells.truncate(cols);
        Row { cells, wrapped: self.wrapped, dirty: true, mark: self.mark }
    }
}

/// Number of cells up to and including the last one that is not blank. A blank
/// (`Cell::EMPTY`) is sixteen zero bytes, so on aarch64 four cells are tested
/// at once by OR-ing them together.
fn content_len(cells: &[Cell]) -> usize {
    let mut n = cells.len();
    #[cfg(target_arch = "aarch64")]
    // SAFETY: each load reads the 4 cells (16 words) ending at cell `n`, and
    // the loop keeps `n >= 4`.
    unsafe {
        use core::arch::aarch64::*;
        let p = cells.as_ptr() as *const u32;
        while n >= 4 {
            let v = vld1q_u32_x4(p.add((n - 4) * 4));
            let any = vorrq_u32(vorrq_u32(v.0, v.1), vorrq_u32(v.2, v.3));
            if vmaxvq_u32(any) != 0 {
                break;
            }
            n -= 4;
        }
    }
    while n > 0 && cells[n - 1] == Cell::EMPTY {
        n -= 1;
    }
    n
}

fn put_run(out: &mut [u8], start: u16, style: &[u32]) {
    out[0..2].copy_from_slice(&start.to_le_bytes());
    out[2..6].copy_from_slice(&style[0].to_le_bytes());
    out[6..10].copy_from_slice(&style[1].to_le_bytes());
    out[10..14].copy_from_slice(&style[2].to_le_bytes());
}

/// If every cell has the first cell's style and an ASCII character, write the
/// characters as bytes into `out` and return true.
#[cfg(target_arch = "aarch64")]
fn uniform_ascii(w: &[u32], out: &mut [u8]) -> bool {
    use core::arch::aarch64::*;
    let len = out.len();
    let mut i = 0usize;
    // SAFETY: each vld4q_u32 reads 4 cells (16 words) at cell `i` or `i + 4`,
    // and the loop keeps `i + 8 <= len`; the store writes 8 bytes at `i`.
    unsafe {
        let p = w.as_ptr();
        let fg = vdupq_n_u32(w[1]);
        let bg = vdupq_n_u32(w[2]);
        let fu = vdupq_n_u32(w[3]);
        let limit = vdupq_n_u32(0x80);
        while i + 8 <= len {
            // De-interleaves four cells into ch, fg, bg and flags lanes.
            let a = vld4q_u32(p.add(i * 4));
            let b = vld4q_u32(p.add(i * 4 + 16));
            let ok = |v: uint32x4x4_t| {
                vandq_u32(
                    vandq_u32(vceqq_u32(v.1, fg), vceqq_u32(v.2, bg)),
                    vandq_u32(vceqq_u32(v.3, fu), vcltq_u32(v.0, limit)),
                )
            };
            if vminvq_u32(vandq_u32(ok(a), ok(b))) == 0 {
                return false;
            }
            let bytes = vmovn_u16(vcombine_u16(vmovn_u32(a.0), vmovn_u32(b.0)));
            vst1_u8(out.as_mut_ptr().add(i), bytes);
            i += 8;
        }
    }
    uniform_ascii_tail(w, out, i)
}

#[cfg(not(target_arch = "aarch64"))]
fn uniform_ascii(w: &[u32], out: &mut [u8]) -> bool {
    uniform_ascii_tail(w, out, 0)
}

fn uniform_ascii_tail(w: &[u32], out: &mut [u8], from: usize) -> bool {
    for i in from..out.len() {
        let c = &w[i * 4..i * 4 + 4];
        if c[0] >= 0x80 || c[1..4] != w[1..4] {
            return false;
        }
        out[i] = c[0] as u8;
    }
    true
}

pub struct Grid {
    cols: usize,
    rows: usize,
    /// Maximum number of lines retained above the viewport.
    max_scrollback: usize,

    /// The live screen: a ring of exactly `rows` rows.
    screen: Vec<Row>,
    /// Ring index of the screen's first line.
    top: usize,
    /// Scrollback, oldest first, packed.
    history: VecDeque<PackedRow>,
    /// Lines scrolled up from the live viewport, 0..=history.
    display_offset: usize,
    /// While scrolled back: the history lines at the top of the viewport,
    /// unpacked, so the renderer can borrow them like screen rows.
    view: Vec<Row>,

    /// Set when every row must be re-uploaded (resize, scroll, viewport move).
    full_damage: bool,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Grid {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Grid {
            cols,
            rows,
            max_scrollback,
            screen: (0..rows).map(|_| Row::new(cols)).collect(),
            top: 0,
            history: VecDeque::new(),
            display_offset: 0,
            view: Vec::new(),
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
    pub fn history_len(&self) -> usize {
        self.history.len()
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

    /// Ring index of *live* line `y`, ignoring any scrollback offset. All
    /// writes go through here: output must land on the real screen even while
    /// the user is looking at history.
    #[inline]
    fn live_index(&self, y: usize) -> usize {
        (self.top + y) % self.rows
    }

    /// Viewport line `y`, accounting for the scrollback offset.
    #[inline]
    pub fn row(&self, y: usize) -> &Row {
        if y < self.view.len() {
            &self.view[y]
        } else {
            &self.screen[self.live_index(y - self.display_offset)]
        }
    }

    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut Row {
        let i = self.live_index(y);
        &mut self.screen[i]
    }

    /// A line by absolute position, where 0 is the oldest scrollback line and
    /// `history + rows - 1` is the bottom of the screen. Screen lines are
    /// borrowed; history lines are unpacked.
    pub fn line(&self, absolute: usize) -> Cow<'_, Row> {
        let h = self.history.len();
        if absolute < h {
            Cow::Owned(self.history[absolute].unpack(self.cols))
        } else {
            Cow::Borrowed(&self.screen[self.live_index((absolute - h).min(self.rows - 1))])
        }
    }

    /// The shell-integration mark on a line, by absolute position, without
    /// unpacking history.
    pub fn mark(&self, absolute: usize) -> RowMark {
        let h = self.history.len();
        if absolute < h {
            self.history[absolute].mark
        } else if absolute - h < self.rows {
            self.screen[self.live_index(absolute - h)].mark
        } else {
            RowMark::default()
        }
    }

    /// The screen alone, without scrollback: what Rewind keeps as a snapshot.
    pub fn screen_copy(&self) -> Grid {
        Grid {
            cols: self.cols,
            rows: self.rows,
            max_scrollback: 0,
            screen: self.screen.clone(),
            top: self.top,
            history: VecDeque::new(),
            display_offset: 0,
            view: Vec::new(),
            full_damage: true,
        }
    }

    /// Set the mark on a line by absolute position.
    pub fn set_mark(&mut self, absolute: usize, mark: RowMark) {
        let h = self.history.len();
        if absolute < h {
            self.history[absolute].mark = mark;
        } else if absolute - h < self.rows {
            let i = self.live_index(absolute - h);
            self.screen[i].mark = mark;
        }
        self.full_damage = true;
    }

    #[inline]
    pub fn total_lines(&self) -> usize {
        self.history.len() + self.rows
    }

    #[inline]
    pub fn cell(&self, x: usize, y: usize) -> Cell {
        self.row(y).cells[x.min(self.cols - 1)]
    }

    #[inline]
    pub fn set_cell(&mut self, x: usize, y: usize, c: Cell) {
        if x < self.cols {
            let i = self.live_index(y);
            let row = &mut self.screen[i];
            row.cells[x] = c;
            row.dirty = true;
        }
    }

    /// Mark a live row dirty without touching its contents.
    #[inline]
    pub fn touch(&mut self, y: usize) {
        if y < self.rows {
            let i = self.live_index(y);
            self.screen[i].dirty = true;
        }
    }

    /// Re-unpack the history lines shown while scrolled back.
    fn refresh_view(&mut self) {
        self.view.clear();
        if self.display_offset == 0 {
            return;
        }
        let first = self.history.len() - self.display_offset;
        let n = self.display_offset.min(self.rows);
        self.view
            .extend((first..first + n).map(|i| self.history[i].unpack(self.cols)));
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
        for _ in 0..n {
            // The line leaving the top of the screen becomes history, packed.
            let i = self.top;
            if self.max_scrollback > 0 {
                // When scrollback is full, the evicted line's buffer holds the
                // new one.
                let recycled = if self.history.len() == self.max_scrollback {
                    self.history.pop_front().map(|p| p.data).unwrap_or_default()
                } else {
                    Vec::new()
                };
                self.history.push_back(PackedRow::pack_into(&self.screen[i], recycled));
            }
            // Its row, still hot in cache, becomes the new bottom line.
            let cols = self.cols;
            let row = &mut self.screen[i];
            if row.cells.len() != cols {
                row.resize(cols, pen);
                row.reset(pen);
            } else {
                let len = content_len(&row.cells);
                row.reset_from_content(len, pen);
            }
            self.top = (self.top + 1) % self.rows;
        }

        // Following the live screen keeps the user pinned to the bottom; if
        // they have scrolled back, hold their position by growing the offset
        // until it saturates against the history limit.
        if self.display_offset > 0 {
            self.display_offset = (self.display_offset + n).min(self.history.len());
            self.refresh_view();
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
                &mut self.screen[i],
                Row {
                    cells: Vec::new(),
                    wrapped: false,
                    dirty: true,
                    mark: RowMark::default(),
                },
            ));
        }
        for (slot, row) in indices.iter().zip(scratch) {
            self.screen[*slot] = row;
        }

        // Blank the lines rotated in at the bottom.
        for y in (bottom + 1 - n)..=bottom {
            let i = self.live_index(y);
            let cols = self.cols;
            if self.screen[i].cells.len() != cols {
                self.screen[i].resize(cols, pen);
            }
            self.screen[i].reset(pen);
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
                &mut self.screen[i],
                Row {
                    cells: Vec::new(),
                    wrapped: false,
                    dirty: true,
                    mark: RowMark::default(),
                },
            ));
        }
        for (slot, row) in indices.iter().zip(scratch) {
            self.screen[*slot] = row;
        }

        for y in top..(top + n) {
            let i = self.live_index(y);
            let cols = self.cols;
            if self.screen[i].cells.len() != cols {
                self.screen[i].resize(cols, pen);
            }
            self.screen[i].reset(pen);
        }
        self.full_damage = true;
    }

    // ---------------------------------------------------------------------
    // Viewport / scrollback navigation
    // ---------------------------------------------------------------------

    /// Move the viewport `delta` lines (positive scrolls towards history).
    /// Returns true if the position actually changed.
    pub fn scroll_display(&mut self, delta: isize) -> bool {
        let new = (self.display_offset as isize + delta).clamp(0, self.history.len() as isize) as usize;
        if new != self.display_offset {
            self.display_offset = new;
            self.refresh_view();
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
        for row in &mut self.screen {
            row.reset(pen);
        }
        self.full_damage = true;
    }

    /// Drop every scrollback line (`ED 3`).
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.display_offset = 0;
        self.view.clear();
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

        let old_total = self.total_lines();
        let mut lines: Vec<Row> = (0..old_total).map(|i| self.line(i).into_owned()).collect();
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

        let old_total = self.total_lines();
        let cursor_line = self.history.len() + cursor.1.min(self.rows - 1);

        // --- gather logical lines -------------------------------------------
        let mut logical: Vec<Vec<Cell>> = Vec::with_capacity(old_total);
        let mut logical_marks: Vec<RowMark> = Vec::with_capacity(old_total);
        let mut current: Vec<Cell> = Vec::new();
        let mut current_mark = RowMark::default();
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
            // A logical line keeps the marks of its rows (in practice only
            // its first row has one).
            if row.mark.is_some() {
                if current_mark.is_some() && current_mark.cmd == row.mark.cmd {
                    current_mark.kinds |= row.mark.kinds;
                } else if !current_mark.is_some() {
                    current_mark = row.mark;
                }
            }

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
                logical_marks.push(std::mem::take(&mut current_mark));
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
            logical_marks.push(current_mark);
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
                if pos == 0 {
                    row.mark = logical_marks[li];
                }
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
                let mut row = Row::new(cols);
                row.mark = logical_marks[li];
                lines.push(row);
                if li == cursor_at.0 && new_cursor.is_none() {
                    new_cursor = Some((0, lines.len() - 1));
                }
            }
        }

        let (cursor_col, cursor_abs) = new_cursor.unwrap_or((0, lines.len().saturating_sub(1)));

        // Blank rows below the cursor are unused screen, not content. Left in,
        // they count towards the "last `rows` lines" that become the viewport,
        // so shrinking a window would push the real text -- prompt included --
        // into scrollback and keep empty padding on screen. Without them the
        // viewport ends at the cursor when the text no longer fits, and growing
        // the window brings scrollback back into view, as in other terminals.
        while lines.len() > cursor_abs + 1
            && lines
                .last()
                .is_some_and(|r| !r.wrapped && !r.mark.is_some() && r.cells.iter().all(|c| c.is_empty()))
        {
            lines.pop();
        }

        // The viewport is the last `rows` physical lines; pad if reflow produced
        // fewer than a screenful.
        while lines.len() < rows {
            lines.push(Row::new(cols));
        }

        // --- rebuild --------------------------------------------------------
        let cap = rows + self.max_scrollback;
        let dropped = lines.len().saturating_sub(cap);
        self.rebuild(lines, cols, rows);

        let total = self.total_lines();
        let abs = cursor_abs.saturating_sub(dropped).min(total.saturating_sub(1));
        let cursor_row = abs.saturating_sub(self.history.len()).min(rows - 1);
        let _ = pen;

        (cursor_col.min(cols - 1), cursor_row)
    }

    /// Install a fresh set of lines: everything above the last `rows` becomes
    /// packed history, the rest the screen.
    fn rebuild(&mut self, mut lines: Vec<Row>, cols: usize, rows: usize) {
        let cap = rows + self.max_scrollback;
        if lines.len() > cap {
            lines.drain(..lines.len() - cap);
        }
        while lines.len() < rows {
            lines.push(Row::new(cols));
        }
        let split = lines.len() - rows;
        let screen = lines.split_off(split);

        self.history = lines.iter().map(PackedRow::pack).collect();
        self.screen = screen;
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.display_offset = 0;
        self.view.clear();
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

    fn styled(ch: char, fg: Color, bg: Color, flags: CellFlags) -> Cell {
        Cell { ch: ch as u32, fg, bg, flags, underline: 0 }
    }

    #[test]
    fn packing_is_lossless() {
        let mut row = Row::new(12);
        row.cells[0] = styled('a', Color::indexed(1), Color::DEFAULT, CellFlags::BOLD);
        row.cells[1] = styled('b', Color::indexed(1), Color::DEFAULT, CellFlags::BOLD);
        row.cells[2] = styled('\u{4f60}', Color::rgb(1, 2, 3), Color::DEFAULT, CellFlags::WIDE);
        row.cells[3] = styled('\0', Color::rgb(1, 2, 3), Color::DEFAULT, CellFlags::WIDE_SPACER);
        row.cells[4].ch = crate::term::CLUSTER_TAG | 7;
        // A blank with a background colour is content: it must survive.
        row.cells[8] = styled('\0', Color::DEFAULT, Color::indexed(4), CellFlags::empty());
        row.wrapped = true;

        let back = PackedRow::pack(&row).unpack(12);
        assert_eq!(back.cells, row.cells);
        assert!(back.wrapped);
    }

    /// The NEON fast path must bail out cleanly wherever the line stops being
    /// one ASCII style, and handle lengths that are not a multiple of 8.
    #[test]
    fn fast_path_edges_round_trip() {
        for (len, style_break, wide_at) in [(37, None, None), (40, Some(9), None), (40, None, Some(13)), (8, Some(7), None), (5, None, None)] {
            let mut row = Row::new(64);
            for i in 0..len {
                row.cells[i].ch = b'a' as u32 + (i % 26) as u32;
                if style_break.is_some_and(|b| i >= b) {
                    row.cells[i].flags = CellFlags::BOLD;
                }
            }
            if let Some(i) = wide_at {
                row.cells[i].ch = '\u{e9}' as u32;
            }
            let back = PackedRow::pack(&row).unpack(64);
            assert_eq!(back.cells, row.cells, "len={len} break={style_break:?} wide={wide_at:?}");
        }
    }

    #[test]
    fn plain_lines_pack_small() {
        let mut row = Row::new(200);
        for (i, c) in "cargo build --release: Compiling tachyon v0.1.0".chars().enumerate() {
            row.cells[i].ch = c as u32;
        }
        let p = PackedRow::pack(&row);
        assert!(!p.wide);
        assert_eq!((p.len, p.runs), (47, 1));
        // 47 characters and one 14-byte style run, against 200 x 16 bytes.
        assert_eq!(p.data.len(), 47 + RUN_BYTES);
        // Unpacking to a narrower or wider grid keeps the text.
        assert_eq!(p.unpack(60).cells[..47], row.cells[..47]);
        assert_eq!(p.unpack(300).cells.len(), 300);
    }

    #[test]
    fn scrolled_back_view_follows_new_output() {
        let pen = Pen::default();
        let mut g = Grid::new(8, 2, 10);
        for i in 0..4 {
            put(&mut g, 1, &format!("L{i}"));
            g.scroll_up(0, 1, 1, &pen);
        }
        // History: "", L0, L1, L2 (the first scroll pushed the blank top row).
        g.scroll_display(2);
        let shown = |g: &Grid| -> Vec<String> {
            (0..2).map(|y| g.row(y).cells().iter().map(|c| c.scalar().unwrap_or(' ')).collect::<String>().trim_end().to_string()).collect()
        };
        assert_eq!(shown(&g), ["L1", "L2"]);
        // More output while scrolled back keeps the same text on screen.
        put(&mut g, 1, "L4");
        g.scroll_up(0, 1, 1, &pen);
        assert_eq!(shown(&g), ["L1", "L2"]);
        assert_eq!(g.display_offset(), 3);
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
