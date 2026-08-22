//! Terminal state: grid, cursor, modes, and the VT dispatch implementation.

pub mod cell;
pub mod color;
pub mod grid;
pub mod parser;

use bitflags::bitflags;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub use cell::{Cell, CellFlags, Pen, CLUSTER_TAG};
pub use color::{Color, Palette};
pub use grid::Grid;
pub use parser::Processor;

bitflags! {
    /// Private and ANSI modes that change how input is interpreted or how the
    /// application expects us to behave.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct TermMode: u32 {
        /// DECAWM -- wrap at the right margin.
        const AUTOWRAP          = 1 << 0;
        /// DECOM -- cursor addressing is relative to the scroll region.
        const ORIGIN            = 1 << 1;
        /// DECTCEM -- cursor is drawn.
        const SHOW_CURSOR       = 1 << 2;
        /// DECCKM -- arrow keys send SS3 instead of CSI.
        const APP_CURSOR        = 1 << 3;
        /// DECKPAM -- keypad sends application sequences.
        const APP_KEYPAD        = 1 << 4;
        /// IRM -- printing shifts the rest of the line right.
        const INSERT            = 1 << 5;
        /// LNM -- Enter sends CR LF.
        const LINE_FEED_NL      = 1 << 6;
        const BRACKETED_PASTE   = 1 << 7;
        const FOCUS_REPORT      = 1 << 8;
        const MOUSE_CLICK       = 1 << 9;
        const MOUSE_DRAG        = 1 << 10;
        const MOUSE_MOTION      = 1 << 11;
        /// Extended SGR mouse coordinates (1006).
        const MOUSE_SGR         = 1 << 12;
        const ALT_SCREEN        = 1 << 13;
        /// DECSCNM -- swap default fg/bg for the whole screen.
        const REVERSE_VIDEO     = 1 << 14;
        /// Synchronised output (2026): hold presentation until released.
        const SYNC_UPDATE       = 1 << 15;
        /// Alternate scroll: wheel becomes arrow keys on the alt screen.
        const ALT_SCROLL        = 1 << 16;

        const ANY_MOUSE = Self::MOUSE_CLICK.bits()
            | Self::MOUSE_DRAG.bits()
            | Self::MOUSE_MOTION.bits();
    }
}

impl Default for TermMode {
    fn default() -> Self {
        TermMode::AUTOWRAP | TermMode::SHOW_CURSOR | TermMode::ALT_SCROLL
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Charset {
    Ascii,
    /// DEC Special Graphics -- the box-drawing set still used by ncurses.
    Graphics,
}

#[derive(Clone, Copy)]
struct Cursor {
    x: usize,
    y: usize,
    pen: Pen,
    /// VT100 deferred wrap: writing in the last column leaves the cursor there
    /// and only wraps when the *next* character arrives.
    wrap_pending: bool,
    g0: Charset,
    g1: Charset,
    /// Which of G0/G1 is currently mapped to GL.
    shifted: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            x: 0,
            y: 0,
            pen: Pen::default(),
            wrap_pending: false,
            g0: Charset::Ascii,
            g1: Charset::Graphics,
            shifted: false,
        }
    }
}

/// Everything the renderer needs about the cursor, snapshotted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CursorState {
    pub x: usize,
    pub y: usize,
    pub shape: CursorShape,
    pub visible: bool,
    pub blinking: bool,
}

pub struct Term {
    grid: Grid,
    /// The screen we are not currently showing. Swapping is O(1).
    inactive: Grid,

    cursor: Cursor,
    saved_cursor: Cursor,
    saved_cursor_alt: Cursor,

    pub mode: TermMode,
    cursor_shape: CursorShape,
    cursor_blinks: bool,

    /// Inclusive scroll region, viewport-relative.
    scroll_top: usize,
    scroll_bot: usize,

    tabs: Vec<bool>,

    /// Grapheme clusters that do not fit in a cell's single scalar.
    clusters: Vec<Box<[char]>>,
    /// Underline colours referenced by `Cell::underline`, biased by one.
    underline_colors: Vec<Color>,

    /// Bytes to write back to the PTY (device reports, etc.).
    replies: Vec<u8>,

    pub title: Option<String>,
    /// Incremented whenever the title changes, so the app can cheaply notice.
    pub title_revision: u64,
    pub bell_revision: u64,

    /// One-shot flag telling [`Processor`] the VT machine returned to Ground.
    ground_signal: bool,

    /// Set whenever anything visible changed.
    dirty: bool,
}

impl Term {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Term {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Term {
            grid: Grid::new(cols, rows, scrollback),
            // The alternate screen never has scrollback, by definition.
            inactive: Grid::new(cols, rows, 0),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            saved_cursor_alt: Cursor::default(),
            mode: TermMode::default(),
            cursor_shape: CursorShape::Block,
            cursor_blinks: true,
            scroll_top: 0,
            scroll_bot: rows - 1,
            tabs: default_tabs(cols),
            clusters: Vec::new(),
            underline_colors: Vec::new(),
            replies: Vec::new(),
            title: None,
            title_revision: 0,
            bell_revision: 0,
            ground_signal: false,
            dirty: true,
        }
    }

    // ---------------------------------------------------------------------
    // Accessors used by the renderer and the app
    // ---------------------------------------------------------------------

    #[inline]
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    #[inline]
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.grid.cols()
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.grid.rows()
    }

    #[inline]
    pub fn cluster(&self, idx: u32) -> Option<&[char]> {
        self.clusters.get((idx & !CLUSTER_TAG) as usize).map(|b| &**b)
    }

    #[inline]
    pub fn underline_color(&self, idx: u16) -> Option<Color> {
        if idx == 0 {
            None
        } else {
            self.underline_colors.get(idx as usize - 1).copied()
        }
    }

    pub fn cursor_state(&self) -> CursorState {
        CursorState {
            x: self.cursor.x.min(self.cols().saturating_sub(1)),
            y: self.cursor.y,
            shape: self.cursor_shape,
            // Hide the cursor while the user is reading scrollback: it refers
            // to the live screen, which is not what they are looking at.
            visible: self.mode.contains(TermMode::SHOW_CURSOR)
                && self.grid.display_offset() == 0,
            blinking: self.cursor_blinks,
        }
    }

    #[inline]
    pub fn take_dirty(&mut self) -> bool {
        core::mem::take(&mut self.dirty)
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[inline]
    pub fn take_replies(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.replies)
    }

    #[inline]
    pub fn has_replies(&self) -> bool {
        !self.replies.is_empty()
    }

    /// Consume the "returned to Ground" signal. See [`parser`].
    #[inline]
    pub fn take_ground_signal(&mut self) -> bool {
        core::mem::take(&mut self.ground_signal)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols() && rows == self.rows() {
            return;
        }
        let pen = self.cursor.pen;

        // The alternate screen belongs to a full-screen application that will
        // redraw it from scratch after SIGWINCH. Re-wrapping it would only
        // scramble a frame that is about to be replaced, so it is resized
        // plainly; the primary screen, which holds the user's history, is
        // reflowed.
        let cursor = (self.cursor.x, self.cursor.y);
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.grid.resize(cols, rows, &pen);
            self.inactive.reflow(cols, rows, &pen, cursor);
            self.cursor.x = self.cursor.x.min(cols - 1);
            self.cursor.y = self.cursor.y.min(rows - 1);
        } else {
            let (cx, cy) = self.grid.reflow(cols, rows, &pen, cursor);
            self.inactive.resize(cols, rows, &pen);
            self.cursor.x = cx;
            self.cursor.y = cy;
        }

        self.tabs = default_tabs(cols);
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.cursor.wrap_pending = false;
        self.dirty = true;
    }

    pub fn scroll_display(&mut self, delta: isize) {
        if self.grid.scroll_display(delta) {
            self.dirty = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.grid.scroll_to_bottom() {
            self.dirty = true;
        }
    }

    /// Extract the visible selection-free text of a line, for clipboard use.
    pub fn line_text(&self, absolute: usize) -> String {
        let row = self.grid.line(absolute);
        let n = row.trimmed_len();
        let mut s = String::with_capacity(n);
        for c in &row.cells()[..n] {
            if c.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            if c.ch & CLUSTER_TAG != 0 {
                if let Some(cl) = self.cluster(c.ch) {
                    s.extend(cl.iter());
                }
            } else if let Some(ch) = c.scalar() {
                s.push(ch);
            } else {
                s.push(' ');
            }
        }
        s
    }

    /// Text between two absolute positions, inclusive.
    ///
    /// Lines joined by autowrap are emitted without a newline: the user
    /// selected one logical line, so that is what lands on the clipboard.
    pub fn text_range(
        &self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
    ) -> String {
        let total = self.grid.total_lines();
        if start_line >= total {
            return String::new();
        }
        let end_line = end_line.min(total - 1);

        let mut out = String::new();
        for line in start_line..=end_line {
            let row = self.grid.line(line);
            let cells = row.cells();

            let from = if line == start_line { start_col } else { 0 };
            let to = if line == end_line {
                (end_col + 1).min(cells.len())
            } else {
                cells.len()
            };
            if from >= to {
                if line != end_line && !row.wrapped {
                    out.push('\n');
                }
                continue;
            }

            // Trailing blanks on a line are padding, not content -- except on
            // the last line of the selection, where the user chose the extent.
            let slice = &cells[from..to];
            let keep = if line == end_line {
                slice.len()
            } else {
                slice.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1)
            };

            for c in &slice[..keep] {
                if c.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                if c.ch & CLUSTER_TAG != 0 {
                    if let Some(cl) = self.cluster(c.ch) {
                        out.extend(cl.iter());
                    }
                } else if let Some(ch) = c.scalar() {
                    out.push(ch);
                } else {
                    out.push(' ');
                }
            }

            // A wrapped line continues into the next one; no break belongs here.
            if line != end_line && !row.wrapped {
                out.push('\n');
            }
        }
        out
    }

    // ---------------------------------------------------------------------
    // The bulk-ASCII fast path
    // ---------------------------------------------------------------------

    /// Write a run of printable ASCII, which by construction contains no
    /// control characters, no wide characters and no combining marks. That lets
    /// us fill whole spans of a row without re-checking anything per byte.
    pub fn write_ascii_run(&mut self, mut bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.dirty = true;
        // Output always brings the viewport back to the live screen.
        self.grid.scroll_to_bottom();

        // With a non-ASCII charset mapped into GL, every byte needs translating
        // and the bulk fill is invalid. Line-drawing runs are short, so the
        // per-character path costs nothing in practice.
        if !self.charset_is_ascii() {
            for &b in bytes {
                let c = dec_graphics(b as char);
                self.put_char(c, 1);
            }
            return;
        }

        let cols = self.cols();
        let pen = self.cursor.pen;
        let template = Cell {
            ch: 0,
            fg: pen.fg,
            bg: pen.bg,
            flags: pen.flags,
            underline: pen.underline,
        };

        // Insert mode has to shift the tail of the line for every character, so
        // there is nothing to batch; fall back to the general path.
        if self.mode.contains(TermMode::INSERT) {
            for &b in bytes {
                self.put_char(b as char, 1);
            }
            return;
        }

        while !bytes.is_empty() {
            if self.cursor.wrap_pending {
                self.wrap_line();
            }

            let x = self.cursor.x;
            let avail = cols - x;
            let take = avail.min(bytes.len());

            let y = self.cursor.y;
            {
                let row = self.grid.row_mut(y);
                let cells = row.cells_mut();
                for (i, &b) in bytes[..take].iter().enumerate() {
                    let mut c = template;
                    c.ch = b as u32;
                    cells[x + i] = c;
                }
            }

            bytes = &bytes[take..];
            if take == avail {
                // Landed exactly on the right margin: defer the wrap.
                self.cursor.x = cols - 1;
                if self.mode.contains(TermMode::AUTOWRAP) {
                    self.cursor.wrap_pending = true;
                } else if !bytes.is_empty() {
                    // No autowrap: everything else overstrikes the last column.
                    let last = bytes[bytes.len() - 1];
                    let mut c = template;
                    c.ch = last as u32;
                    self.grid.set_cell(cols - 1, y, c);
                    break;
                }
            } else {
                self.cursor.x = x + take;
            }
        }
    }

    /// Handle a C0 control byte that does not change the parser state.
    #[inline]
    pub fn execute_c0(&mut self, byte: u8) {
        self.execute(byte);
    }

    /// True when the character set currently mapped into GL passes ASCII
    /// through unchanged, which is what makes the bulk fast path sound.
    #[inline]
    fn charset_is_ascii(&self) -> bool {
        let active = if self.cursor.shifted {
            self.cursor.g1
        } else {
            self.cursor.g0
        };
        matches!(active, Charset::Ascii)
    }

    // ---------------------------------------------------------------------
    // Character output
    // ---------------------------------------------------------------------

    fn wrap_line(&mut self) {
        let y = self.cursor.y;
        self.grid.row_mut(y).wrapped = true;
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
        self.linefeed_raw();
    }

    fn put_char(&mut self, c: char, width: usize) {
        let cols = self.cols();

        if self.cursor.wrap_pending {
            self.wrap_line();
        }

        // A wide character that will not fit in the last column moves to the
        // next line rather than being split.
        if width == 2 && self.cursor.x + 1 >= cols {
            if self.mode.contains(TermMode::AUTOWRAP) {
                let y = self.cursor.y;
                self.grid.row_mut(y).wrapped = true;
                self.cursor.x = 0;
                self.linefeed_raw();
            } else {
                return;
            }
        }

        let x = self.cursor.x;
        let y = self.cursor.y;
        let pen = self.cursor.pen;

        if self.mode.contains(TermMode::INSERT) {
            self.shift_right(x, y, width);
        }

        let mut cell = Cell {
            ch: c as u32,
            fg: pen.fg,
            bg: pen.bg,
            flags: pen.flags,
            underline: pen.underline,
        };
        if width == 2 {
            cell.flags |= CellFlags::WIDE;
        }
        self.grid.set_cell(x, y, cell);

        if width == 2 {
            let mut spacer = cell;
            spacer.ch = 0;
            spacer.flags = (spacer.flags & !CellFlags::WIDE) | CellFlags::WIDE_SPACER;
            self.grid.set_cell(x + 1, y, spacer);
        }

        let next = x + width;
        if next >= cols {
            self.cursor.x = cols - 1;
            if self.mode.contains(TermMode::AUTOWRAP) {
                self.cursor.wrap_pending = true;
            }
        } else {
            self.cursor.x = next;
        }
    }

    /// Attach a zero-width mark to the cell the cursor just left.
    fn combine_with_previous(&mut self, mark: char) {
        let y = self.cursor.y;
        // The character we last wrote is behind the cursor, unless a wrap is
        // pending in which case the cursor is still sitting on it.
        let mut x = self.cursor.x;
        if !self.cursor.wrap_pending {
            if x == 0 {
                return;
            }
            x -= 1;
        }
        // Step back over the filler half of a wide character.
        let cur = self.grid.cell(x, y);
        if cur.flags.contains(CellFlags::WIDE_SPACER) && x > 0 {
            x -= 1;
        }

        let cell = self.grid.cell(x, y);
        let mut chars: Vec<char> = if cell.ch & CLUSTER_TAG != 0 {
            self.cluster(cell.ch).map(|c| c.to_vec()).unwrap_or_default()
        } else if let Some(ch) = cell.scalar() {
            vec![ch]
        } else {
            vec![' ']
        };
        // Bound the cluster: pathological input should not grow without limit.
        if chars.len() >= 8 {
            return;
        }
        chars.push(mark);

        self.clusters.push(chars.into_boxed_slice());
        let idx = (self.clusters.len() - 1) as u32 | CLUSTER_TAG;
        let mut updated = cell;
        updated.ch = idx;
        self.grid.set_cell(x, y, updated);
        self.dirty = true;
    }

    fn shift_right(&mut self, x: usize, y: usize, n: usize) {
        let cols = self.cols();
        if x >= cols {
            return;
        }
        let n = n.min(cols - x);
        let blank = Cell::blank(&self.cursor.pen);
        let row = self.grid.row_mut(y);
        let cells = row.cells_mut();
        cells[x..].rotate_right(n);
        for c in &mut cells[x..x + n] {
            *c = blank;
        }
    }

    // ---------------------------------------------------------------------
    // Cursor movement
    // ---------------------------------------------------------------------

    /// Linefeed without honouring LNM; used internally by wrapping.
    fn linefeed_raw(&mut self) {
        if self.cursor.y == self.scroll_bot {
            let pen = self.cursor.pen;
            let (t, b) = (self.scroll_top, self.scroll_bot);
            self.grid.scroll_up(t, b, 1, &pen);
        } else if self.cursor.y + 1 < self.rows() {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.y == self.scroll_top {
            let pen = self.cursor.pen;
            let (t, b) = (self.scroll_top, self.scroll_bot);
            self.grid.scroll_down(t, b, 1, &pen);
        } else {
            self.cursor.y = self.cursor.y.saturating_sub(1);
        }
    }

    /// Topmost and bottommost addressable rows, respecting DECOM.
    #[inline]
    fn bounds(&self) -> (usize, usize) {
        if self.mode.contains(TermMode::ORIGIN) {
            (self.scroll_top, self.scroll_bot)
        } else {
            (0, self.rows() - 1)
        }
    }

    fn goto(&mut self, x: usize, y: usize) {
        let (top, bot) = self.bounds();
        self.cursor.x = x.min(self.cols() - 1);
        self.cursor.y = (top + y).min(bot);
        self.cursor.wrap_pending = false;
        self.dirty = true;
    }

    fn move_up(&mut self, n: usize) {
        let top = if self.mode.contains(TermMode::ORIGIN) {
            self.scroll_top
        } else {
            0
        };
        self.cursor.y = self.cursor.y.saturating_sub(n).max(top);
        self.cursor.wrap_pending = false;
    }

    fn move_down(&mut self, n: usize) {
        let bot = if self.mode.contains(TermMode::ORIGIN) {
            self.scroll_bot
        } else {
            self.rows() - 1
        };
        self.cursor.y = (self.cursor.y + n).min(bot);
        self.cursor.wrap_pending = false;
    }

    fn move_left(&mut self, n: usize) {
        self.cursor.x = self.cursor.x.saturating_sub(n);
        self.cursor.wrap_pending = false;
    }

    fn move_right(&mut self, n: usize) {
        self.cursor.x = (self.cursor.x + n).min(self.cols() - 1);
        self.cursor.wrap_pending = false;
    }

    fn tab(&mut self, n: usize) {
        let cols = self.cols();
        for _ in 0..n.max(1) {
            let mut x = self.cursor.x + 1;
            while x < cols && !self.tabs[x] {
                x += 1;
            }
            self.cursor.x = x.min(cols - 1);
        }
        self.cursor.wrap_pending = false;
    }

    fn back_tab(&mut self, n: usize) {
        for _ in 0..n.max(1) {
            let mut x = self.cursor.x;
            while x > 0 {
                x -= 1;
                if self.tabs[x] {
                    break;
                }
            }
            self.cursor.x = x;
        }
        self.cursor.wrap_pending = false;
    }

    // ---------------------------------------------------------------------
    // Screen switching
    // ---------------------------------------------------------------------

    fn set_alt_screen(&mut self, enable: bool, save_restore: bool) {
        let on_alt = self.mode.contains(TermMode::ALT_SCREEN);
        if enable == on_alt {
            return;
        }

        if enable {
            if save_restore {
                self.saved_cursor = self.cursor;
            }
            core::mem::swap(&mut self.grid, &mut self.inactive);
            let pen = self.cursor.pen;
            self.grid.clear_all(&pen);
            self.mode |= TermMode::ALT_SCREEN;
            if save_restore {
                self.cursor = Cursor {
                    pen: self.cursor.pen,
                    ..Cursor::default()
                };
            }
        } else {
            self.saved_cursor_alt = self.cursor;
            core::mem::swap(&mut self.grid, &mut self.inactive);
            self.mode -= TermMode::ALT_SCREEN;
            if save_restore {
                self.cursor = self.saved_cursor;
            }
        }
        self.grid.damage_all();
        self.dirty = true;
    }

    fn full_reset(&mut self) {
        let rows = self.rows();
        let cols = self.cols();
        self.set_alt_screen(false, false);
        self.cursor = Cursor::default();
        self.saved_cursor = Cursor::default();
        self.mode = TermMode::default();
        self.cursor_shape = CursorShape::Block;
        self.cursor_blinks = true;
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.tabs = default_tabs(cols);
        self.clusters.clear();
        self.underline_colors.clear();
        let pen = Pen::default();
        self.grid.clear_all(&pen);
        self.grid.clear_history();
        self.inactive.clear_all(&pen);
        self.dirty = true;
    }

    // ---------------------------------------------------------------------
    // Erase / edit
    // ---------------------------------------------------------------------

    fn erase_in_line(&mut self, mode: u16) {
        let (x, y, cols) = (self.cursor.x, self.cursor.y, self.cols());
        let blank = Cell::blank(&self.cursor.pen);
        let range = match mode {
            1 => 0..(x + 1).min(cols),
            2 => 0..cols,
            _ => x..cols,
        };
        let row = self.grid.row_mut(y);
        row.cells_mut()[range].fill(blank);
        row.wrapped = false;
        self.dirty = true;
    }

    fn erase_in_display(&mut self, mode: u16) {
        let (y, rows) = (self.cursor.y, self.rows());
        let blank = Cell::blank(&self.cursor.pen);
        match mode {
            // Cursor to end of screen.
            0 => {
                self.erase_in_line(0);
                for row in (y + 1)..rows {
                    self.grid.row_mut(row).cells_mut().fill(blank);
                }
            }
            // Beginning of screen to cursor.
            1 => {
                for row in 0..y {
                    self.grid.row_mut(row).cells_mut().fill(blank);
                }
                self.erase_in_line(1);
            }
            // Whole screen.
            2 => {
                let pen = self.cursor.pen;
                self.grid.clear_all(&pen);
            }
            // Whole screen and scrollback.
            3 => {
                let pen = self.cursor.pen;
                self.grid.clear_all(&pen);
                self.grid.clear_history();
            }
            _ => {}
        }
        self.grid.damage_all();
        self.dirty = true;
    }

    fn erase_chars(&mut self, n: usize) {
        let (x, y, cols) = (self.cursor.x, self.cursor.y, self.cols());
        let end = (x + n.max(1)).min(cols);
        let blank = Cell::blank(&self.cursor.pen);
        self.grid.row_mut(y).cells_mut()[x..end].fill(blank);
        self.dirty = true;
    }

    fn delete_chars(&mut self, n: usize) {
        let (x, y, cols) = (self.cursor.x, self.cursor.y, self.cols());
        if x >= cols {
            return;
        }
        let n = n.max(1).min(cols - x);
        let blank = Cell::blank(&self.cursor.pen);
        let row = self.grid.row_mut(y);
        let cells = row.cells_mut();
        cells[x..].rotate_left(n);
        let start = cols - n;
        cells[start..].fill(blank);
        self.dirty = true;
    }

    fn insert_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bot {
            return;
        }
        let pen = self.cursor.pen;
        let (top, bot) = (self.cursor.y, self.scroll_bot);
        self.grid.scroll_down(top, bot, n.max(1), &pen);
        self.cursor.x = 0;
        self.dirty = true;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bot {
            return;
        }
        let pen = self.cursor.pen;
        let (top, bot) = (self.cursor.y, self.scroll_bot);
        self.grid.scroll_up(top, bot, n.max(1), &pen);
        self.cursor.x = 0;
        self.dirty = true;
    }

    // ---------------------------------------------------------------------
    // SGR
    // ---------------------------------------------------------------------

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.cursor.pen.reset();
            return;
        }

        let flat: Vec<&[u16]> = params.iter().collect();
        let mut i = 0;
        while i < flat.len() {
            let sub = flat[i];
            let code = sub.first().copied().unwrap_or(0);

            match code {
                0 => self.cursor.pen.reset(),
                1 => self.cursor.pen.flags |= CellFlags::BOLD,
                2 => self.cursor.pen.flags |= CellFlags::DIM,
                3 => self.cursor.pen.flags |= CellFlags::ITALIC,
                4 => {
                    // `4:n` selects an underline style.
                    let style = sub.get(1).copied().unwrap_or(1);
                    self.cursor.pen.flags -= CellFlags::ANY_UNDERLINE;
                    self.cursor.pen.flags |= match style {
                        0 => CellFlags::empty(),
                        2 => CellFlags::DOUBLE_UNDERLINE,
                        3 => CellFlags::CURLY_UNDERLINE,
                        4 => CellFlags::DOTTED_UNDERLINE,
                        5 => CellFlags::DASHED_UNDERLINE,
                        _ => CellFlags::UNDERLINE,
                    };
                }
                5 | 6 => self.cursor.pen.flags |= CellFlags::BLINK,
                7 => self.cursor.pen.flags |= CellFlags::REVERSE,
                8 => self.cursor.pen.flags |= CellFlags::HIDDEN,
                9 => self.cursor.pen.flags |= CellFlags::STRIKEOUT,
                21 => {
                    self.cursor.pen.flags -= CellFlags::ANY_UNDERLINE;
                    self.cursor.pen.flags |= CellFlags::DOUBLE_UNDERLINE;
                }
                22 => self.cursor.pen.flags -= CellFlags::BOLD | CellFlags::DIM,
                23 => self.cursor.pen.flags -= CellFlags::ITALIC,
                24 => self.cursor.pen.flags -= CellFlags::ANY_UNDERLINE,
                25 => self.cursor.pen.flags -= CellFlags::BLINK,
                27 => self.cursor.pen.flags -= CellFlags::REVERSE,
                28 => self.cursor.pen.flags -= CellFlags::HIDDEN,
                29 => self.cursor.pen.flags -= CellFlags::STRIKEOUT,
                30..=37 => self.cursor.pen.fg = Color::indexed((code - 30) as u8),
                38 => {
                    if let Some((c, used)) = parse_extended_color(&flat, i, sub) {
                        self.cursor.pen.fg = c;
                        i += used;
                        continue;
                    }
                }
                39 => self.cursor.pen.fg = Color::DEFAULT,
                40..=47 => self.cursor.pen.bg = Color::indexed((code - 40) as u8),
                48 => {
                    if let Some((c, used)) = parse_extended_color(&flat, i, sub) {
                        self.cursor.pen.bg = c;
                        i += used;
                        continue;
                    }
                }
                49 => self.cursor.pen.bg = Color::DEFAULT,
                53 => self.cursor.pen.flags |= CellFlags::OVERLINE,
                55 => self.cursor.pen.flags -= CellFlags::OVERLINE,
                58 => {
                    if let Some((c, used)) = parse_extended_color(&flat, i, sub) {
                        self.underline_colors.push(c);
                        self.cursor.pen.underline = self.underline_colors.len() as u16;
                        i += used;
                        continue;
                    }
                }
                59 => self.cursor.pen.underline = 0,
                90..=97 => self.cursor.pen.fg = Color::indexed((code - 90) as u8 + 8),
                100..=107 => self.cursor.pen.bg = Color::indexed((code - 100) as u8 + 8),
                _ => {}
            }
            i += 1;
        }
    }

    // ---------------------------------------------------------------------
    // Modes
    // ---------------------------------------------------------------------

    fn set_private_mode(&mut self, mode: u16, enable: bool) {
        let set = |m: &mut TermMode, f: TermMode| {
            if enable {
                *m |= f;
            } else {
                *m -= f;
            }
        };
        match mode {
            1 => set(&mut self.mode, TermMode::APP_CURSOR),
            3 => {
                // DECCOLM: the side effect applications actually rely on is the
                // screen clear and cursor home, not the column count.
                let pen = self.cursor.pen;
                self.grid.clear_all(&pen);
                self.cursor.x = 0;
                self.cursor.y = 0;
                self.scroll_top = 0;
                self.scroll_bot = self.rows() - 1;
            }
            5 => set(&mut self.mode, TermMode::REVERSE_VIDEO),
            6 => {
                set(&mut self.mode, TermMode::ORIGIN);
                self.goto(0, 0);
            }
            7 => set(&mut self.mode, TermMode::AUTOWRAP),
            12 => self.cursor_blinks = enable,
            25 => set(&mut self.mode, TermMode::SHOW_CURSOR),
            1000 => set(&mut self.mode, TermMode::MOUSE_CLICK),
            1002 => set(&mut self.mode, TermMode::MOUSE_DRAG),
            1003 => set(&mut self.mode, TermMode::MOUSE_MOTION),
            1004 => set(&mut self.mode, TermMode::FOCUS_REPORT),
            1006 => set(&mut self.mode, TermMode::MOUSE_SGR),
            1007 => set(&mut self.mode, TermMode::ALT_SCROLL),
            47 => self.set_alt_screen(enable, false),
            1047 => self.set_alt_screen(enable, false),
            1048 => {
                if enable {
                    self.saved_cursor = self.cursor;
                } else {
                    self.cursor = self.saved_cursor;
                }
            }
            1049 => self.set_alt_screen(enable, true),
            2004 => set(&mut self.mode, TermMode::BRACKETED_PASTE),
            2026 => set(&mut self.mode, TermMode::SYNC_UPDATE),
            _ => {}
        }
        self.dirty = true;
    }

    fn set_ansi_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            4 => {
                if enable {
                    self.mode |= TermMode::INSERT;
                } else {
                    self.mode -= TermMode::INSERT;
                }
            }
            20 => {
                if enable {
                    self.mode |= TermMode::LINE_FEED_NL;
                } else {
                    self.mode -= TermMode::LINE_FEED_NL;
                }
            }
            _ => {}
        }
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

/// Parse `38`/`48`/`58` extended colour arguments in both the colon form
/// (`38:2::r:g:b`, one parameter with subparams) and the legacy semicolon form
/// (`38;2;r;g;b`, several parameters). Returns the colour and how many
/// parameters were consumed.
fn parse_extended_color(flat: &[&[u16]], i: usize, sub: &[u16]) -> Option<(Color, usize)> {
    // Colon form: everything is inside `sub`.
    if sub.len() >= 2 {
        return match sub[1] {
            2 => {
                // Some emitters include a colour-space id, some do not.
                let base = if sub.len() >= 6 { 3 } else { 2 };
                let r = *sub.get(base)? as u8;
                let g = *sub.get(base + 1)? as u8;
                let b = *sub.get(base + 2)? as u8;
                Some((Color::rgb(r, g, b), 1))
            }
            5 => Some((Color::indexed(*sub.get(2)? as u8), 1)),
            _ => None,
        };
    }

    // Semicolon form: read ahead across parameters.
    let kind = flat.get(i + 1)?.first().copied()?;
    match kind {
        2 => {
            let r = flat.get(i + 2)?.first().copied()? as u8;
            let g = flat.get(i + 3)?.first().copied()? as u8;
            let b = flat.get(i + 4)?.first().copied()? as u8;
            Some((Color::rgb(r, g, b), 5))
        }
        5 => {
            let idx = flat.get(i + 2)?.first().copied()? as u8;
            Some((Color::indexed(idx), 3))
        }
        _ => None,
    }
}

/// DEC Special Graphics: maps ASCII 0x5F..0x7E onto box-drawing and symbol
/// characters. Still how ncurses draws borders on many systems.
fn dec_graphics(c: char) -> char {
    match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '\u{2409}',
        'c' => '\u{240C}',
        'd' => '\u{240D}',
        'e' => '\u{240A}',
        'f' => '°',
        'g' => '±',
        'h' => '\u{2424}',
        'i' => '\u{240B}',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => c,
    }
}

// ===========================================================================
// VT dispatch
// ===========================================================================

/// Helper: `params[i]`, or `default` when absent or zero.
fn arg(params: &Params, i: usize, default: u16) -> u16 {
    match params.iter().nth(i).and_then(|s| s.first().copied()) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.ground_signal = true;
        self.dirty = true;
        self.grid.scroll_to_bottom();

        let c = match (self.cursor.shifted, self.cursor.g0, self.cursor.g1) {
            (false, Charset::Graphics, _) | (true, _, Charset::Graphics) => dec_graphics(c),
            _ => c,
        };

        match c.width() {
            Some(0) | None => self.combine_with_previous(c),
            Some(w) => self.put_char(c, w),
        }
    }

    fn execute(&mut self, byte: u8) {
        // Deliberately does not set `ground_signal`: a C0 byte inside a CSI is
        // executed without leaving the sequence. See `parser`.
        self.dirty = true;
        match byte {
            // BEL
            0x07 => self.bell_revision = self.bell_revision.wrapping_add(1),
            // BS
            0x08 => {
                if self.cursor.wrap_pending {
                    self.cursor.wrap_pending = false;
                } else {
                    self.cursor.x = self.cursor.x.saturating_sub(1);
                }
            }
            // HT
            0x09 => self.tab(1),
            // LF, VT, FF
            0x0A | 0x0B | 0x0C => {
                self.grid.scroll_to_bottom();
                self.linefeed_raw();
                if self.mode.contains(TermMode::LINE_FEED_NL) {
                    self.cursor.x = 0;
                }
                self.cursor.wrap_pending = false;
            }
            // CR
            0x0D => {
                self.cursor.x = 0;
                self.cursor.wrap_pending = false;
            }
            // SO / SI -- shift out to G1, shift in to G0.
            0x0E => self.cursor.shifted = true,
            0x0F => self.cursor.shifted = false,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.ground_signal = true;
        if ignore {
            return;
        }
        self.dirty = true;

        let private = intermediates.first() == Some(&b'?');
        let n = |i: usize| arg(params, i, 1) as usize;

        match (action, intermediates.first()) {
            ('@', _) => {
                let (x, y) = (self.cursor.x, self.cursor.y);
                self.shift_right(x, y, n(0));
            }
            ('A', _) => self.move_up(n(0)),
            ('B', _) | ('e', _) => self.move_down(n(0)),
            ('C', _) | ('a', _) => self.move_right(n(0)),
            ('D', _) => self.move_left(n(0)),
            ('E', _) => {
                self.move_down(n(0));
                self.cursor.x = 0;
            }
            ('F', _) => {
                self.move_up(n(0));
                self.cursor.x = 0;
            }
            ('G', _) | ('`', _) => {
                let x = n(0) - 1;
                self.cursor.x = x.min(self.cols() - 1);
                self.cursor.wrap_pending = false;
            }
            ('H', _) | ('f', _) => {
                let row = n(0) - 1;
                let col = arg(params, 1, 1) as usize - 1;
                self.goto(col, row);
            }
            ('I', _) => self.tab(n(0)),
            ('J', _) => self.erase_in_display(arg(params, 0, 0)),
            ('K', _) => self.erase_in_line(arg(params, 0, 0)),
            ('L', _) => self.insert_lines(n(0)),
            ('M', _) => self.delete_lines(n(0)),
            ('P', _) => self.delete_chars(n(0)),
            ('S', _) => {
                let pen = self.cursor.pen;
                let (t, b) = (self.scroll_top, self.scroll_bot);
                self.grid.scroll_up(t, b, n(0), &pen);
            }
            ('T', _) => {
                let pen = self.cursor.pen;
                let (t, b) = (self.scroll_top, self.scroll_bot);
                self.grid.scroll_down(t, b, n(0), &pen);
            }
            ('X', _) => self.erase_chars(n(0)),
            ('Z', _) => self.back_tab(n(0)),
            ('d', _) => {
                let y = n(0) - 1;
                let x = self.cursor.x;
                self.goto(x, y);
            }
            ('g', _) => match arg(params, 0, 0) {
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {
                    let x = self.cursor.x;
                    if x < self.tabs.len() {
                        self.tabs[x] = false;
                    }
                }
            },
            ('h', _) => {
                for p in params.iter().filter_map(|s| s.first().copied()) {
                    if private {
                        self.set_private_mode(p, true);
                    } else {
                        self.set_ansi_mode(p, true);
                    }
                }
            }
            ('l', _) => {
                for p in params.iter().filter_map(|s| s.first().copied()) {
                    if private {
                        self.set_private_mode(p, false);
                    } else {
                        self.set_ansi_mode(p, false);
                    }
                }
            }
            ('m', _) => self.apply_sgr(params),
            ('n', _) => match arg(params, 0, 0) {
                // Device status report: ready.
                5 => self.replies.extend_from_slice(b"\x1b[0n"),
                // Cursor position report.
                6 => {
                    let (top, _) = self.bounds();
                    let row = self.cursor.y.saturating_sub(top) + 1;
                    let col = self.cursor.x + 1;
                    self.replies
                        .extend_from_slice(format!("\x1b[{row};{col}R").as_bytes());
                }
                _ => {}
            },
            ('q', Some(b' ')) => {
                // DECSCUSR
                let (shape, blink) = match arg(params, 0, 1) {
                    0 | 1 => (CursorShape::Block, true),
                    2 => (CursorShape::Block, false),
                    3 => (CursorShape::Underline, true),
                    4 => (CursorShape::Underline, false),
                    5 => (CursorShape::Beam, true),
                    _ => (CursorShape::Beam, false),
                };
                self.cursor_shape = shape;
                self.cursor_blinks = blink;
            }
            ('r', _) => {
                let rows = self.rows();
                let top = arg(params, 0, 1) as usize - 1;
                let bot = match params.iter().nth(1).and_then(|s| s.first().copied()) {
                    Some(0) | None => rows - 1,
                    Some(v) => (v as usize - 1).min(rows - 1),
                };
                if top < bot {
                    self.scroll_top = top;
                    self.scroll_bot = bot;
                    self.goto(0, 0);
                }
            }
            ('s', _) => self.saved_cursor = self.cursor,
            ('u', _) => self.cursor = self.saved_cursor,
            ('c', _) => {
                // Primary device attributes: claim to be a VT220 with colour.
                self.replies.extend_from_slice(b"\x1b[?62;22c");
            }
            ('t', _) => {
                // Window manipulation; report a plausible size for `18t`.
                if arg(params, 0, 0) == 18 {
                    let s = format!("\x1b[8;{};{}t", self.rows(), self.cols());
                    self.replies.extend_from_slice(s.as_bytes());
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.ground_signal = true;
        if ignore {
            return;
        }
        self.dirty = true;

        match (byte, intermediates.first()) {
            // Charset designation.
            (b'0', Some(b'(')) => self.cursor.g0 = Charset::Graphics,
            (b'B', Some(b'(')) => self.cursor.g0 = Charset::Ascii,
            (b'0', Some(b')')) => self.cursor.g1 = Charset::Graphics,
            (b'B', Some(b')')) => self.cursor.g1 = Charset::Ascii,
            // DECSC / DECRC
            (b'7', _) => self.saved_cursor = self.cursor,
            (b'8', Some(b'#')) => {
                // DECALN: fill the screen with 'E'. Used by test suites.
                let mut c = Cell::blank(&self.cursor.pen);
                c.ch = 'E' as u32;
                for y in 0..self.rows() {
                    self.grid.row_mut(y).cells_mut().fill(c);
                }
                self.grid.damage_all();
            }
            (b'8', _) => self.cursor = self.saved_cursor,
            // IND / NEL / RI
            (b'D', _) => self.linefeed_raw(),
            (b'E', _) => {
                self.linefeed_raw();
                self.cursor.x = 0;
            }
            (b'M', _) => self.reverse_index(),
            // HTS
            (b'H', _) => {
                let x = self.cursor.x;
                if x < self.tabs.len() {
                    self.tabs[x] = true;
                }
            }
            // Keypad modes.
            (b'=', _) => self.mode |= TermMode::APP_KEYPAD,
            (b'>', _) => self.mode -= TermMode::APP_KEYPAD,
            // RIS
            (b'c', _) => self.full_reset(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.ground_signal = true;
        let Some(cmd) = params.first() else { return };

        match *cmd {
            // Window / icon title.
            b"0" | b"2" => {
                if let Some(t) = params.get(1) {
                    self.title = Some(String::from_utf8_lossy(t).into_owned());
                    self.title_revision = self.title_revision.wrapping_add(1);
                }
            }
            // OSC 7: report working directory. Recorded for future use (new
            // tab inherits cwd); parsing it here keeps shells quiet.
            b"7" => {}
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // Entering DCS -- explicitly *not* a return to Ground.
    }

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {
        self.ground_signal = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut Term, p: &mut Processor, s: &str) {
        p.advance(t, s.as_bytes());
    }

    fn screen(t: &Term) -> Vec<String> {
        (0..t.rows())
            .map(|y| {
                t.grid()
                    .row(y)
                    .cells()
                    .iter()
                    .map(|c| c.scalar().unwrap_or(' '))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn plain_text_lands_on_the_grid() {
        let mut t = Term::new(20, 4, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "hello world");
        assert_eq!(screen(&t)[0], "hello world");
    }

    #[test]
    fn newline_and_carriage_return() {
        let mut t = Term::new(20, 4, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "one\r\ntwo\r\nthree");
        let s = screen(&t);
        assert_eq!(s[0], "one");
        assert_eq!(s[1], "two");
        assert_eq!(s[2], "three");
    }

    #[test]
    fn autowrap_is_deferred_at_the_margin() {
        let mut t = Term::new(5, 3, 10);
        let mut p = Processor::new();
        // Exactly fills row 0; the cursor must stay put, not wrap yet.
        feed(&mut t, &mut p, "abcde");
        assert_eq!(t.cursor_state().y, 0);
        feed(&mut t, &mut p, "f");
        let s = screen(&t);
        assert_eq!(s[0], "abcde");
        assert_eq!(s[1], "f");
        assert!(t.grid().row(0).wrapped);
    }

    #[test]
    fn fast_path_and_slow_path_agree_across_chunk_splits() {
        // The same stream fed in one piece and byte-by-byte must produce the
        // same screen: this is the property the ground-tracking relies on.
        let input = "\x1b[1;32mgreen\x1b[0m plain \x1b[Ktail\r\n\
                     wide: \u{4f60}\u{597d} combining: e\u{0301}\r\n\
                     \x1b(0lqqqk\x1b(B ascii again\r\n\
                     \x1b[10;5Hpositioned\x1b[?25l";

        let mut a = Term::new(40, 6, 50);
        let mut pa = Processor::new();
        pa.advance(&mut a, input.as_bytes());

        let mut b = Term::new(40, 6, 50);
        let mut pb = Processor::new();
        for byte in input.as_bytes() {
            pb.advance(&mut b, &[*byte]);
        }

        assert_eq!(screen(&a), screen(&b));
        assert_eq!(a.cursor_state(), b.cursor_state());
        assert_eq!(a.mode, b.mode);
    }

    #[test]
    fn sgr_sets_colours_both_forms() {
        let mut t = Term::new(20, 2, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b[38;2;10;20;30mX");
        assert_eq!(t.grid().cell(0, 0).fg, Color::rgb(10, 20, 30));

        feed(&mut t, &mut p, "\x1b[38:2::40:50:60mY");
        assert_eq!(t.grid().cell(1, 0).fg, Color::rgb(40, 50, 60));

        feed(&mut t, &mut p, "\x1b[38;5;123mZ");
        assert_eq!(t.grid().cell(2, 0).fg, Color::indexed(123));
    }

    #[test]
    fn erase_in_line_respects_pen_background() {
        let mut t = Term::new(10, 2, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "abcdefghij\r");
        feed(&mut t, &mut p, "\x1b[41m\x1b[K");
        for x in 0..10 {
            assert_eq!(t.grid().cell(x, 0).bg, Color::indexed(1), "col {x}");
        }
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        let mut t = Term::new(10, 2, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\u{4f60}");
        assert!(t.grid().cell(0, 0).flags.contains(CellFlags::WIDE));
        assert!(t.grid().cell(1, 0).flags.contains(CellFlags::WIDE_SPACER));
        assert_eq!(t.cursor_state().x, 2);
    }

    #[test]
    fn combining_mark_attaches_to_previous_cell() {
        let mut t = Term::new(10, 2, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "e\u{0301}");
        let c = t.grid().cell(0, 0);
        assert_ne!(c.ch & CLUSTER_TAG, 0);
        assert_eq!(t.cluster(c.ch).unwrap(), &['e', '\u{0301}']);
    }

    #[test]
    fn alt_screen_round_trips() {
        let mut t = Term::new(10, 3, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "primary");
        feed(&mut t, &mut p, "\x1b[?1049h");
        assert!(t.mode.contains(TermMode::ALT_SCREEN));
        assert_eq!(screen(&t)[0], "");
        feed(&mut t, &mut p, "alt");
        assert_eq!(screen(&t)[0], "alt");
        feed(&mut t, &mut p, "\x1b[?1049l");
        assert!(!t.mode.contains(TermMode::ALT_SCREEN));
        assert_eq!(screen(&t)[0], "primary");
    }

    #[test]
    fn scroll_region_confines_scrolling() {
        let mut t = Term::new(10, 5, 20);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b[1;1Htop");
        // Region covers rows 2..4 (1-based).
        feed(&mut t, &mut p, "\x1b[2;4r");
        feed(&mut t, &mut p, "\x1b[2;1Ha\r\nb\r\nc\r\nd");
        let s = screen(&t);
        assert_eq!(s[0], "top", "line outside the region must not move");
        assert_eq!(s[3], "d");
    }

    #[test]
    fn cpr_reports_cursor_position() {
        let mut t = Term::new(20, 5, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b[3;7H\x1b[6n");
        assert_eq!(t.take_replies(), b"\x1b[3;7R".to_vec());
    }

    #[test]
    fn dec_graphics_maps_line_drawing() {
        let mut t = Term::new(10, 2, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b(0qx\x1b(B");
        assert_eq!(t.grid().cell(0, 0).scalar(), Some('─'));
        assert_eq!(t.grid().cell(1, 0).scalar(), Some('│'));
    }

    #[test]
    fn insert_and_delete_characters() {
        let mut t = Term::new(10, 2, 10);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "abcdef\x1b[1;1H\x1b[2@");
        assert_eq!(screen(&t)[0], "  abcdef");
        feed(&mut t, &mut p, "\x1b[2P");
        assert_eq!(screen(&t)[0], "abcdef");
    }

    #[test]
    fn long_ascii_run_crosses_simd_boundaries() {
        // Exercises the vectorised bulk fill across many wraps.
        let mut t = Term::new(80, 24, 200);
        let mut p = Processor::new();
        let line = "x".repeat(1000);
        feed(&mut t, &mut p, &line);
        // 1000 characters over 80 columns = 12 full rows plus 40.
        assert_eq!(t.cursor_state().x, 40);
        assert_eq!(screen(&t)[0], "x".repeat(80));
    }

    #[test]
    fn output_snaps_viewport_back_to_the_bottom() {
        let mut t = Term::new(10, 2, 50);
        let mut p = Processor::new();
        for i in 0..10 {
            feed(&mut t, &mut p, &format!("line{i}\r\n"));
        }
        t.scroll_display(5);
        assert!(t.grid().display_offset() > 0);
        feed(&mut t, &mut p, "new");
        assert_eq!(t.grid().display_offset(), 0);
    }
}

#[cfg(test)]
mod reverse_tests {
    use super::*;

    #[test]
    fn reverse_video_keeps_the_characters() {
        let mut t = Term::new(20, 2, 10);
        let mut p = Processor::new();
        p.advance(&mut t, b"\x1b[7m reverse \x1b[0m");

        // The cells must still carry their characters; REVERSE is presentation.
        let text: String = (0..9)
            .map(|x| t.grid().cell(x, 0).scalar().unwrap_or('?'))
            .collect();
        assert_eq!(text, " reverse ");
        for x in 0..9 {
            assert!(
                t.grid().cell(x, 0).flags.contains(CellFlags::REVERSE),
                "cell {x} lost REVERSE"
            );
        }
    }
}
