//! Keyboard-driven Vi Copy Mode for scrollback navigation, search, and yanking.
//!
//! Operates purely on [`Term`] so the motion, visual-selection, and search logic
//! is OS-independent and thoroughly unit-testable.

use crate::term::cell::{CellFlags, CLUSTER_TAG};
use crate::term::find::{self, Match};
use crate::term::Term;

const MAX_COUNT: usize = 100_000;
const SEARCH_LIMIT: usize = 4_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualMode {
    None,
    Char,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDir {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKey {
    Char(char),
    Ctrl(char),
    Esc,
    Enter,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyAction {
    Redraw,
    YankAndExit(String),
    Exit,
    Bell,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordKind {
    Small,
    Big,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Blank,
    Word,
    Punct,
}

/// Interactive Vi-style cursor and selection state over a [`Term`] grid.
#[derive(Clone, Debug)]
pub struct CopyMode {
    /// Absolute grid line (`0` is the oldest line in scrollback).
    pub line: usize,
    /// Column in `0..cols`.
    pub col: usize,
    pub visual: VisualMode,
    /// Visual anchor `(line, col)`.
    pub anchor: (usize, usize),
    pending_g: bool,
    pending_count: usize,
    pub search_prompt: Option<(SearchDir, String)>,
    search_origin: Option<(usize, usize)>,
    pub last_search: Option<(SearchDir, String)>,
    pub matches: Vec<Match>,
    pub current_match: Option<usize>,
    matches_total_lines: usize,
}

impl CopyMode {
    /// Enter Copy Mode at the live cursor (when un-scrolled) or viewport middle.
    pub fn enter(term: &Term) -> Self {
        let g = term.grid();
        let total = g.total_lines();
        let hist = g.history_len();
        let off = g.display_offset();
        let cols = term.cols().max(1);
        let (line, col) = if total == 0 {
            (0, 0)
        } else if off == 0 {
            let c = term.cursor_state();
            ((hist + c.y).min(total - 1), c.x.min(cols - 1))
        } else {
            let top = hist.saturating_sub(off);
            ((top + term.rows() / 2).min(total - 1), 0)
        };
        Self {
            line,
            col,
            visual: VisualMode::None,
            anchor: (line, col),
            pending_g: false,
            pending_count: 0,
            search_prompt: None,
            search_origin: None,
            last_search: None,
            matches: Vec::new(),
            current_match: None,
            matches_total_lines: total,
        }
    }

    /// Process one keystroke in Copy Mode, scrolling the terminal viewport so
    /// the Vi cursor always stays on screen.
    pub fn handle_key(&mut self, term: &mut Term, key: CopyKey) -> CopyAction {
        self.clamp_cursor(term);

        if self.search_prompt.is_some() {
            return self.handle_search_key(term, key);
        }

        // Accumulate numeric repeat counts (except `0` when no count started yet).
        if let CopyKey::Char(d @ '0'..='9') = key {
            if !self.pending_g && (d != '0' || self.pending_count > 0) {
                let digit = (d as u8 - b'0') as usize;
                self.pending_count = self
                    .pending_count
                    .saturating_mul(10)
                    .saturating_add(digit)
                    .min(MAX_COUNT);
                return CopyAction::Redraw;
            }
        }

        let explicit_count = self.pending_count;
        let count = explicit_count.max(1);
        let was_g = self.pending_g;
        self.pending_g = false;
        self.pending_count = 0;

        if was_g {
            match key {
                CopyKey::Char('g') => {
                    let target = if explicit_count > 0 {
                        explicit_count - 1
                    } else {
                        0
                    };
                    let max_line = term.grid().total_lines().saturating_sub(1);
                    self.line = target.min(max_line);
                    self.col = self.col.min(term.cols().saturating_sub(1));
                    self.keep_visible(term);
                    return CopyAction::Redraw;
                }
                _ => {
                    // Cancel `g` chord cleanly.
                    return CopyAction::Bell;
                }
            }
        }

        let mut moved = false;
        let max_line = term.grid().total_lines().saturating_sub(1);
        let max_col = term.cols().saturating_sub(1);

        match key {
            CopyKey::Char('h') | CopyKey::Left => {
                self.col = self.col.saturating_sub(count);
                moved = true;
            }
            CopyKey::Char('l') | CopyKey::Right => {
                self.col = (self.col.saturating_add(count)).min(max_col);
                moved = true;
            }
            CopyKey::Char('j') | CopyKey::Down => {
                self.line = (self.line.saturating_add(count)).min(max_line);
                moved = true;
            }
            CopyKey::Char('k') | CopyKey::Up => {
                self.line = self.line.saturating_sub(count);
                moved = true;
            }
            CopyKey::Char('0') | CopyKey::Home => {
                self.col = 0;
                moved = true;
            }
            CopyKey::Char('^') => {
                self.col = first_nonblank(term, self.line);
                moved = true;
            }
            CopyKey::Char('$') | CopyKey::End => {
                self.col = last_nonblank(term, self.line);
                moved = true;
            }
            CopyKey::Char('w') => {
                for _ in 0..count {
                    self.move_word_forward(term, WordKind::Small);
                }
                moved = true;
            }
            CopyKey::Char('W') => {
                for _ in 0..count {
                    self.move_word_forward(term, WordKind::Big);
                }
                moved = true;
            }
            CopyKey::Char('b') => {
                for _ in 0..count {
                    self.move_word_backward(term, WordKind::Small);
                }
                moved = true;
            }
            CopyKey::Char('B') => {
                for _ in 0..count {
                    self.move_word_backward(term, WordKind::Big);
                }
                moved = true;
            }
            CopyKey::Char('e') => {
                for _ in 0..count {
                    self.move_word_end(term, WordKind::Small);
                }
                moved = true;
            }
            CopyKey::Char('E') => {
                for _ in 0..count {
                    self.move_word_end(term, WordKind::Big);
                }
                moved = true;
            }
            CopyKey::Char('g') => {
                self.pending_count = explicit_count;
                self.pending_g = true;
            }
            CopyKey::Char('G') => {
                self.line = if explicit_count > 0 {
                    (explicit_count - 1).min(max_line)
                } else {
                    max_line
                };
                self.col = 0;
                moved = true;
            }
            CopyKey::Char('H') => {
                let (top, bot) = viewport_bounds(term);
                let target = top + explicit_count.saturating_sub(1);
                self.line = target.min(bot);
                self.col = first_nonblank(term, self.line);
                moved = true;
            }
            CopyKey::Char('M') => {
                let (top, bot) = viewport_bounds(term);
                self.line = top + (bot.saturating_sub(top)) / 2;
                self.col = first_nonblank(term, self.line);
                moved = true;
            }
            CopyKey::Char('L') => {
                let (top, bot) = viewport_bounds(term);
                self.line = bot.saturating_sub(explicit_count.saturating_sub(1)).max(top);
                self.col = first_nonblank(term, self.line);
                moved = true;
            }
            CopyKey::Ctrl('u') => {
                let step = (term.rows().max(2) / 2).saturating_mul(count).max(1);
                self.line = self.line.saturating_sub(step);
                term.scroll_display(step as isize);
                moved = true;
            }
            CopyKey::Ctrl('d') => {
                let step = (term.rows().max(2) / 2).saturating_mul(count).max(1);
                self.line = (self.line.saturating_add(step)).min(max_line);
                term.scroll_display(-(step as isize));
                moved = true;
            }
            CopyKey::Ctrl('b') | CopyKey::PageUp => {
                let step = term.rows().max(1).saturating_mul(count);
                self.line = self.line.saturating_sub(step);
                term.scroll_display(step as isize);
                moved = true;
            }
            CopyKey::Ctrl('f') | CopyKey::PageDown => {
                let step = term.rows().max(1).saturating_mul(count);
                self.line = (self.line.saturating_add(step)).min(max_line);
                term.scroll_display(-(step as isize));
                moved = true;
            }
            CopyKey::Ctrl('y') => {
                term.scroll_display(count as isize);
                let (top, bot) = viewport_bounds(term);
                self.line = self.line.clamp(top, bot);
            }
            CopyKey::Ctrl('e') => {
                term.scroll_display(-(count as isize));
                let (top, bot) = viewport_bounds(term);
                self.line = self.line.clamp(top, bot);
            }
            CopyKey::Char('v') | CopyKey::Char(' ') => {
                if self.visual == VisualMode::Char {
                    self.visual = VisualMode::None;
                } else {
                    if self.visual == VisualMode::None {
                        self.anchor = (self.line, self.col);
                    }
                    self.visual = VisualMode::Char;
                }
            }
            CopyKey::Char('V') => {
                if self.visual == VisualMode::Line {
                    self.visual = VisualMode::None;
                } else {
                    if self.visual == VisualMode::None {
                        self.anchor = (self.line, self.col);
                    }
                    self.visual = VisualMode::Line;
                }
            }
            CopyKey::Char('o') => {
                if self.visual != VisualMode::None {
                    std::mem::swap(&mut self.anchor.0, &mut self.line);
                    std::mem::swap(&mut self.anchor.1, &mut self.col);
                    moved = true;
                }
            }
            CopyKey::Char('/') => {
                self.search_origin = Some((self.line, self.col));
                self.search_prompt = Some((SearchDir::Forward, String::new()));
            }
            CopyKey::Char('?') => {
                self.search_origin = Some((self.line, self.col));
                self.search_prompt = Some((SearchDir::Backward, String::new()));
            }
            CopyKey::Char('n') | CopyKey::Char('N') => {
                let rev = key == CopyKey::Char('N');
                let Some((base_dir, query)) = self.last_search.clone() else {
                    return CopyAction::Bell;
                };
                if term.grid().total_lines() != self.matches_total_lines {
                    self.run_search(term, &query);
                }
                let dir = match (base_dir, rev) {
                    (SearchDir::Forward, false) | (SearchDir::Backward, true) => SearchDir::Forward,
                    _ => SearchDir::Backward,
                };
                let mut ok = true;
                for _ in 0..count {
                    if !self.step_match((self.line, self.col), dir, false) {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    return CopyAction::Bell;
                }
                self.keep_visible(term);
                return CopyAction::Redraw;
            }
            CopyKey::Char('y') | CopyKey::Enter => {
                let text = self.yank_text(term);
                return CopyAction::YankAndExit(text);
            }
            CopyKey::Char('Y') => {
                let prev = self.visual;
                self.visual = VisualMode::None;
                let text = self.yank_text(term);
                self.visual = prev;
                return CopyAction::YankAndExit(text);
            }
            CopyKey::Char('q') | CopyKey::Ctrl('c') => {
                return CopyAction::Exit;
            }
            CopyKey::Esc => {
                if self.visual != VisualMode::None {
                    self.visual = VisualMode::None;
                } else {
                    return CopyAction::Exit;
                }
            }
            _ => return CopyAction::Bell,
        }

        if moved {
            self.keep_visible(term);
        }
        CopyAction::Redraw
    }

    fn handle_search_key(&mut self, term: &mut Term, key: CopyKey) -> CopyAction {
        let Some((dir, mut query)) = self.search_prompt.take() else {
            return CopyAction::Redraw;
        };
        let origin = self.search_origin.unwrap_or((self.line, self.col));

        match key {
            CopyKey::Esc | CopyKey::Ctrl('c') => {
                self.line = origin.0;
                self.col = origin.1;
                self.search_origin = None;
                self.clamp_cursor(term);
                self.keep_visible(term);
                CopyAction::Redraw
            }
            CopyKey::Enter => {
                self.search_origin = None;
                if query.is_empty() {
                    if let Some((prev_dir, prev_q)) = self.last_search.clone() {
                        self.run_search(term, &prev_q);
                        let found = self.step_match((self.line, self.col), dir, false);
                        self.last_search = Some((prev_dir, prev_q));
                        self.keep_visible(term);
                        return if found { CopyAction::Redraw } else { CopyAction::Bell };
                    }
                    return CopyAction::Redraw;
                }
                self.run_search(term, &query);
                self.last_search = Some((dir, query));
                if self.step_match(origin, dir, true) {
                    self.keep_visible(term);
                    CopyAction::Redraw
                } else {
                    CopyAction::Bell
                }
            }
            CopyKey::Backspace => {
                if query.pop().is_none() {
                    self.line = origin.0;
                    self.col = origin.1;
                    self.search_origin = None;
                    self.matches.clear();
                    self.current_match = None;
                    self.clamp_cursor(term);
                    self.keep_visible(term);
                    return CopyAction::Redraw;
                }
                self.run_search(term, &query);
                let _ = self.step_match(origin, dir, true);
                self.keep_visible(term);
                self.search_prompt = Some((dir, query));
                CopyAction::Redraw
            }
            CopyKey::Char(ch) if !ch.is_control() => {
                query.push(ch);
                self.run_search(term, &query);
                let _ = self.step_match(origin, dir, true);
                self.keep_visible(term);
                self.search_prompt = Some((dir, query));
                CopyAction::Redraw
            }
            _ => {
                self.search_prompt = Some((dir, query));
                CopyAction::Bell
            }
        }
    }

    /// Inclusive selection bounds `((start_line, start_col), (end_line, end_col))`.
    pub fn selection_range(&self, term: &Term) -> Option<((usize, usize), (usize, usize))> {
        let cols = term.cols().max(1);
        let max_line = term.grid().total_lines().saturating_sub(1);
        match self.visual {
            VisualMode::None => None,
            VisualMode::Char => {
                let a = (self.anchor.0.min(max_line), self.anchor.1.min(cols - 1));
                let b = (self.line.min(max_line), self.col.min(cols - 1));
                Some(if a <= b { (a, b) } else { (b, a) })
            }
            VisualMode::Line => {
                let a = self.anchor.0.min(max_line);
                let b = self.line.min(max_line);
                let top = logical_start(term, a.min(b));
                let bot = logical_end(term, a.max(b));
                Some(((top, 0), (bot, cols - 1)))
            }
        }
    }

    /// Text to place on the clipboard (trimmed per-line padding, logical wraps preserved).
    pub fn yank_text(&self, term: &Term) -> String {
        let cols = term.cols().max(1);
        let ((s_line, s_col), (e_line, e_col)) = if let Some(r) = self.selection_range(term) {
            r
        } else {
            let s = logical_start(term, self.line);
            let e = logical_end(term, self.line);
            ((s, 0), (e, cols - 1))
        };
        let raw = term.text_range(s_line, s_col, e_line, e_col);
        raw.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
    }

    /// Cursor position inside the current visible viewport `(col, row)`, or
    /// `None` if scrolled away manually after a key motion.
    pub fn viewport_cursor(&self, term: &Term) -> Option<(usize, usize)> {
        let g = term.grid();
        let top = g.history_len().saturating_sub(g.display_offset());
        let rows = term.rows();
        if self.line >= top && self.line < top + rows {
            Some((self.col.min(term.cols().saturating_sub(1)), self.line - top))
        } else {
            None
        }
    }

    /// Search highlights converted into viewport coordinates `(row, x0, x1, current)`
    /// compatible with [`crate::mac::renderer::Decor::highlights`].
    pub fn highlights_on_screen(&self, term: &Term) -> Vec<(usize, usize, usize, bool)> {
        if self.matches.is_empty() {
            return Vec::new();
        }
        let g = term.grid();
        let rows = term.rows();
        let cols = term.cols();
        let top = g.history_len().saturating_sub(g.display_offset());
        let end = top + rows;
        let mut out = Vec::new();
        for (idx, m) in self.matches.iter().enumerate() {
            if m.end.0 < top || m.start.0 >= end {
                continue;
            }
            let current = self.current_match == Some(idx);
            let lo = m.start.0.max(top);
            let hi = m.end.0.min(end.saturating_sub(1));
            for l in lo..=hi {
                let x0 = if l == m.start.0 { m.start.1 } else { 0 };
                let x1 = if l == m.end.0 { m.end.1.min(cols) } else { cols };
                if x1 > x0 {
                    out.push((l - top, x0, x1, current));
                }
            }
        }
        out
    }

    /// Concise status pill text for the bottom status strip.
    pub fn status_label(&self, term: &Term) -> String {
        if let Some((dir, ref q)) = self.search_prompt {
            let prefix = if dir == SearchDir::Forward { '/' } else { '?' };
            if q.is_empty() {
                return format!("COPY {prefix}_");
            }
            let count = if self.matches.is_empty() {
                "no match".to_string()
            } else if let Some(i) = self.current_match {
                format!("{}/{}", i + 1, self.matches.len())
            } else {
                format!("{}", self.matches.len())
            };
            return format!("COPY {prefix}{q}_ ({count})");
        }
        let total = term.grid().total_lines().max(1);
        let mode = match self.visual {
            VisualMode::None => "COPY",
            VisualMode::Char => "VISUAL",
            VisualMode::Line => "V-LINE",
        };
        let mut s = format!("{mode} · L{}/{} C{}", self.line + 1, total, self.col + 1);
        if self.pending_count > 0 {
            s.push_str(&format!(" · {}", self.pending_count));
        } else if self.pending_g {
            s.push_str(" · g");
        }
        s
    }

    fn clamp_cursor(&mut self, term: &Term) {
        let max_line = term.grid().total_lines().saturating_sub(1);
        let max_col = term.cols().saturating_sub(1);
        self.line = self.line.min(max_line);
        self.col = self.col.min(max_col);
        self.anchor.0 = self.anchor.0.min(max_line);
        self.anchor.1 = self.anchor.1.min(max_col);
    }

    fn keep_visible(&self, term: &mut Term) {
        let g = term.grid();
        let total = g.total_lines();
        if total == 0 {
            return;
        }
        let h = g.history_len();
        let rows = term.rows().max(1);
        let off = g.display_offset();
        let top = h.saturating_sub(off);
        let bot = (top + rows - 1).min(total - 1);

        let want_off = if self.line < top {
            (h.saturating_sub(self.line)).min(h)
        } else if self.line > bot {
            let new_top = (self.line + 1).saturating_sub(rows).min(h);
            h.saturating_sub(new_top)
        } else {
            return;
        };
        let delta = want_off as isize - off as isize;
        if delta != 0 {
            term.scroll_display(delta);
        }
    }

    fn run_search(&mut self, term: &Term, query: &str) {
        self.matches.clear();
        self.current_match = None;
        self.matches_total_lines = term.grid().total_lines();
        if query.is_empty() {
            return;
        }
        if let Ok(re) = find::pattern(query, false) {
            self.matches = find::find(term, &re, SEARCH_LIMIT);
        }
    }

    fn step_match(&mut self, origin: (usize, usize), dir: SearchDir, inclusive: bool) -> bool {
        if self.matches.is_empty() {
            self.current_match = None;
            return false;
        }
        let idx = match dir {
            SearchDir::Forward => self
                .matches
                .iter()
                .position(|m| if inclusive { m.start >= origin } else { m.start > origin })
                .unwrap_or(0),
            SearchDir::Backward => self
                .matches
                .iter()
                .rposition(|m| if inclusive { m.start <= origin } else { m.start < origin })
                .unwrap_or(self.matches.len() - 1),
        };
        self.current_match = Some(idx);
        self.line = self.matches[idx].start.0;
        self.col = self.matches[idx].start.1;
        true
    }

    fn move_word_forward(&mut self, term: &Term, kind: WordKind) {
        let total = term.grid().total_lines();
        let cols = term.cols().max(1);
        let mut pos = (self.line, self.col);

        if char_class(term, pos.0, pos.1, kind) != CharClass::Blank {
            let k0 = char_class(term, pos.0, pos.1, kind);
            while let Some(next) = next_cell(pos, total, cols) {
                if char_class(term, next.0, next.1, kind) != k0 {
                    pos = next;
                    break;
                }
                pos = next;
                if next_cell(pos, total, cols).is_none() {
                    self.line = pos.0;
                    self.col = pos.1;
                    return;
                }
            }
        }

        while char_class(term, pos.0, pos.1, kind) == CharClass::Blank {
            let Some(next) = next_cell(pos, total, cols) else {
                self.line = pos.0;
                self.col = last_nonblank(term, pos.0);
                return;
            };
            pos = next;
        }
        self.line = pos.0;
        self.col = pos.1;
    }

    fn move_word_end(&mut self, term: &Term, kind: WordKind) {
        let total = term.grid().total_lines();
        let cols = term.cols().max(1);
        let Some(mut pos) = next_cell((self.line, self.col), total, cols) else {
            return;
        };
        while char_class(term, pos.0, pos.1, kind) == CharClass::Blank {
            let Some(next) = next_cell(pos, total, cols) else {
                self.line = pos.0;
                self.col = last_nonblank(term, pos.0);
                return;
            };
            pos = next;
        }
        let k0 = char_class(term, pos.0, pos.1, kind);
        while let Some(next) = next_cell(pos, total, cols) {
            if char_class(term, next.0, next.1, kind) != k0 {
                break;
            }
            pos = next;
        }
        self.line = pos.0;
        self.col = pos.1;
    }

    fn move_word_backward(&mut self, term: &Term, kind: WordKind) {
        let cols = term.cols().max(1);
        let Some(mut pos) = prev_cell((self.line, self.col), cols) else {
            return;
        };
        while char_class(term, pos.0, pos.1, kind) == CharClass::Blank {
            let Some(prev) = prev_cell(pos, cols) else {
                self.line = 0;
                self.col = 0;
                return;
            };
            pos = prev;
        }
        let k0 = char_class(term, pos.0, pos.1, kind);
        while let Some(prev) = prev_cell(pos, cols) {
            if char_class(term, prev.0, prev.1, kind) != k0 {
                break;
            }
            pos = prev;
        }
        self.line = pos.0;
        self.col = pos.1;
    }
}

fn viewport_bounds(term: &Term) -> (usize, usize) {
    let g = term.grid();
    let total = g.total_lines();
    let top = g.history_len().saturating_sub(g.display_offset());
    let bot = (top + term.rows().saturating_sub(1)).min(total.saturating_sub(1));
    (top, bot)
}

fn next_cell(pos: (usize, usize), total_lines: usize, cols: usize) -> Option<(usize, usize)> {
    if pos.1 + 1 < cols {
        Some((pos.0, pos.1 + 1))
    } else if pos.0 + 1 < total_lines {
        Some((pos.0 + 1, 0))
    } else {
        None
    }
}

fn prev_cell(pos: (usize, usize), cols: usize) -> Option<(usize, usize)> {
    if pos.1 > 0 {
        Some((pos.0, pos.1 - 1))
    } else if pos.0 > 0 {
        Some((pos.0 - 1, cols - 1))
    } else {
        None
    }
}

fn cell_char(term: &Term, line: usize, col: usize) -> char {
    let row = term.grid().line(line);
    let Some(c) = row.cells().get(col) else {
        return ' ';
    };
    if c.flags.contains(CellFlags::WIDE_SPACER) {
        return ' ';
    }
    if c.ch & CLUSTER_TAG != 0 {
        return term
            .cluster(c.ch)
            .and_then(|cl| cl.first().copied())
            .unwrap_or(' ');
    }
    c.scalar().unwrap_or(' ')
}

fn char_class(term: &Term, line: usize, col: usize, kind: WordKind) -> CharClass {
    let ch = cell_char(term, line, col);
    if ch == ' ' || ch == '\t' || ch == '\0' {
        return CharClass::Blank;
    }
    match kind {
        WordKind::Small => {
            if ch.is_alphanumeric() || ch == '_' {
                CharClass::Word
            } else {
                CharClass::Punct
            }
        }
        WordKind::Big => CharClass::Word,
    }
}

fn first_nonblank(term: &Term, line: usize) -> usize {
    let row = term.grid().line(line);
    let cols = term.cols().min(row.cells().len());
    for col in 0..cols {
        let ch = cell_char(term, line, col);
        if ch != ' ' && ch != '\t' && ch != '\0' {
            return col;
        }
    }
    0
}

fn last_nonblank(term: &Term, line: usize) -> usize {
    let row = term.grid().line(line);
    let len = row.trimmed_len().min(term.cols());
    if len == 0 {
        return 0;
    }
    for col in (0..len).rev() {
        let ch = cell_char(term, line, col);
        if ch != ' ' && ch != '\t' && ch != '\0' {
            return col;
        }
    }
    0
}

fn logical_start(term: &Term, mut line: usize) -> usize {
    while line > 0 && term.grid().line(line - 1).wrapped {
        line -= 1;
    }
    line
}

fn logical_end(term: &Term, mut line: usize) -> usize {
    let total = term.grid().total_lines();
    while line + 1 < total && term.grid().line(line).wrapped {
        line += 1;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Processor;

    fn term_with(cols: usize, rows: usize, text: &str) -> Term {
        let mut t = Term::new(cols, rows, 200);
        Processor::new().advance(&mut t, text.as_bytes());
        t
    }

    #[test]
    fn copy_mode_motions_scrollback_and_counts_work() {
        let mut text = String::new();
        for i in 0..30 {
            text.push_str(&format!("  line-{i:02} alpha_beta foo/bar end\r\n"));
        }
        let mut t = term_with(50, 6, &text);
        let mut cm = CopyMode::enter(&t);
        assert!(cm.line >= t.grid().history_len());

        // `gg` jumps to the oldest scrollback line and scrolls the viewport up.
        cm.handle_key(&mut t, CopyKey::Char('g'));
        cm.handle_key(&mut t, CopyKey::Char('g'));
        assert_eq!(cm.line, 0);
        assert!(t.grid().display_offset() > 0);
        assert!(cm.viewport_cursor(&t).is_some());

        // `^` skips leading spaces; `$` jumps to `d` of `end`.
        cm.handle_key(&mut t, CopyKey::Char('^'));
        assert_eq!(cm.col, 2);
        cm.handle_key(&mut t, CopyKey::Char('$'));
        assert_eq!(cell_char(&t, cm.line, cm.col), 'd');

        // `0`, `w`, `e`, `W`, `b` motions.
        cm.handle_key(&mut t, CopyKey::Char('0'));
        assert_eq!(cm.col, 0);
        cm.handle_key(&mut t, CopyKey::Char('w'));
        assert_eq!(cm.col, 2); // `line`
        cm.handle_key(&mut t, CopyKey::Char('w'));
        assert_eq!(cell_char(&t, cm.line, cm.col), '-');
        cm.handle_key(&mut t, CopyKey::Char('W'));
        assert_eq!(cell_char(&t, cm.line, cm.col), 'a');
        cm.handle_key(&mut t, CopyKey::Char('e'));
        assert_eq!(cell_char(&t, cm.line, cm.col), 'a'); // end of `alpha_beta`
        cm.handle_key(&mut t, CopyKey::Char('b'));
        assert_eq!(cell_char(&t, cm.line, cm.col), 'a');

        // Numeric count `3j` moves down 3 lines.
        let before_l = cm.line;
        cm.handle_key(&mut t, CopyKey::Char('3'));
        cm.handle_key(&mut t, CopyKey::Char('j'));
        assert_eq!(cm.line, before_l + 3);

        // `H`, `M`, `L` jump inside viewport.
        cm.handle_key(&mut t, CopyKey::Char('L'));
        let bot = cm.line;
        cm.handle_key(&mut t, CopyKey::Char('H'));
        let top = cm.line;
        assert_eq!(bot - top, 5);
        cm.handle_key(&mut t, CopyKey::Char('M'));
        assert_eq!(cm.line, top + 2);

        // `Ctrl-d` and `G` reach the bottom of the buffer cleanly.
        cm.handle_key(&mut t, CopyKey::Ctrl('d'));
        assert!(cm.line > top + 2);
        cm.handle_key(&mut t, CopyKey::Char('G'));
        assert_eq!(cm.line, t.grid().total_lines() - 1);
        assert_eq!(t.grid().display_offset(), 0);
    }

    #[test]
    fn copy_mode_visual_char_line_and_yank_work_across_wraps() {
        let mut t = term_with(10, 4, "hello world!\r\nabcdefghijklmno\r\nlast");
        let mut cm = CopyMode::enter(&t);

        // Navigate to line 0 (`hello world!`, which wrapped across lines 0..=1).
        cm.handle_key(&mut t, CopyKey::Char('g'));
        cm.handle_key(&mut t, CopyKey::Char('g'));
        cm.handle_key(&mut t, CopyKey::Char('0'));
        cm.handle_key(&mut t, CopyKey::Char('l'));
        cm.handle_key(&mut t, CopyKey::Char('l'));
        cm.handle_key(&mut t, CopyKey::Char('v'));
        assert_eq!(cm.visual, VisualMode::Char);

        // Extend to col 6 and swap anchor with `o`.
        cm.handle_key(&mut t, CopyKey::Char('4'));
        cm.handle_key(&mut t, CopyKey::Char('l'));
        assert_eq!(cm.col, 6);
        cm.handle_key(&mut t, CopyKey::Char('o'));
        assert_eq!(cm.col, 2);

        // Esc in visual clears selection without exiting Copy Mode.
        assert_eq!(cm.handle_key(&mut t, CopyKey::Esc), CopyAction::Redraw);
        assert_eq!(cm.visual, VisualMode::None);

        // Re-select `llo w` and yank.
        cm.handle_key(&mut t, CopyKey::Char('v'));
        cm.handle_key(&mut t, CopyKey::Char('4'));
        cm.handle_key(&mut t, CopyKey::Char('l'));
        assert_eq!(
            cm.handle_key(&mut t, CopyKey::Char('y')),
            CopyAction::YankAndExit("llo w".to_string())
        );

        // Line visual on wrapped `abcdefghijklmno` selects the full logical line.
        let mut cm2 = CopyMode::enter(&t);
        cm2.handle_key(&mut t, CopyKey::Char('3'));
        cm2.handle_key(&mut t, CopyKey::Char('G'));
        cm2.handle_key(&mut t, CopyKey::Char('V'));
        assert_eq!(cm2.visual, VisualMode::Line);
        assert_eq!(
            cm2.handle_key(&mut t, CopyKey::Char('y')),
            CopyAction::YankAndExit("abcdefghijklmno".to_string())
        );

        // Pressing Esc when not in Visual exits Copy Mode.
        let mut cm3 = CopyMode::enter(&t);
        assert_eq!(cm3.handle_key(&mut t, CopyKey::Esc), CopyAction::Exit);
    }

    #[test]
    fn copy_mode_search_forward_backward_and_cancel() {
        let mut text = String::new();
        for i in 0..20 {
            if i == 3 || i == 15 {
                text.push_str(&format!("row {i:02} needle-hit\r\n"));
            } else {
                text.push_str(&format!("row {i:02} filler\r\n"));
            }
        }
        let mut t = term_with(40, 5, &text);
        let mut cm = CopyMode::enter(&t);
        let start_pos = (cm.line, cm.col);

        // Canceling `/` prompt with Esc restores origin.
        cm.handle_key(&mut t, CopyKey::Char('/'));
        cm.handle_key(&mut t, CopyKey::Char('r'));
        assert!(cm.search_prompt.is_some());
        cm.handle_key(&mut t, CopyKey::Esc);
        assert!(cm.search_prompt.is_none());
        assert_eq!((cm.line, cm.col), start_pos);

        // Backward search `?needle` finds row 15, then `n` finds row 3.
        cm.handle_key(&mut t, CopyKey::Char('?'));
        for ch in "needle".chars() {
            cm.handle_key(&mut t, CopyKey::Char(ch));
        }
        assert_eq!(cm.handle_key(&mut t, CopyKey::Enter), CopyAction::Redraw);
        assert_eq!(cm.line, 15);
        assert_eq!(cm.matches.len(), 2);
        assert!(!cm.highlights_on_screen(&t).is_empty());

        cm.handle_key(&mut t, CopyKey::Char('n'));
        assert_eq!(cm.line, 3);
        cm.handle_key(&mut t, CopyKey::Char('N'));
        assert_eq!(cm.line, 15);
    }
}
