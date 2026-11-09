//! Text selection: by character (drag), word (double-click) or line
//! (triple-click), like every other Mac text view.
//!
//! Positions are absolute lines (0 is the oldest scrollback line) so a
//! selection stays on the same text while output scrolls. A pointer position
//! carries two columns: the cell under it, used to pick words and lines, and
//! the cell boundary nearest to it, used when dragging by character -- which is
//! why a drag that starts on the right half of a letter does not include it.

use crate::term::Term;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    Char,
    Word,
    Line,
}

impl Unit {
    pub fn from_clicks(clicks: isize) -> Unit {
        match clicks {
            2 => Unit::Word,
            n if n >= 3 => Unit::Line,
            _ => Unit::Char,
        }
    }
}

/// Where the pointer is, in grid terms.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pos {
    pub line: usize,
    /// The cell under the pointer, `0..cols`.
    pub cell: usize,
    /// The cell boundary nearest the pointer, `0..=cols`.
    pub edge: usize,
}

/// Inclusive cell range, in reading order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

impl Range {
    pub fn contains(&self, line: usize, col: usize) -> bool {
        (line, col) >= self.start && (line, col) <= self.end
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    anchor: Pos,
    head: Pos,
    unit: Unit,
    /// The mouse button is still down.
    pub dragging: bool,
}

impl Selection {
    pub fn new(at: Pos, unit: Unit) -> Selection {
        Selection {
            anchor: at,
            head: at,
            unit,
            dragging: true,
        }
    }

    /// Everything: all of scrollback and the screen.
    pub fn all(term: &Term) -> Selection {
        let last = term.grid().total_lines().saturating_sub(1);
        Selection {
            anchor: Pos { line: 0, cell: 0, edge: 0 },
            head: Pos { line: last, cell: 0, edge: 0 },
            unit: Unit::Line,
            dragging: false,
        }
    }

    pub fn extend(&mut self, to: Pos) {
        self.head = to;
    }

    pub fn range(&self, term: &Term) -> Option<Range> {
        let cols = term.cols();
        let (a, b) = if (self.anchor.line, self.anchor.edge) <= (self.head.line, self.head.edge) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        match self.unit {
            Unit::Char => {
                // Boundaries to cells: from the cell after `a`'s boundary to
                // the cell before `b`'s.
                let start = if a.edge >= cols { (a.line + 1, 0) } else { (a.line, a.edge) };
                let end = if b.edge > 0 {
                    (b.line, b.edge - 1)
                } else if b.line > 0 {
                    (b.line - 1, cols - 1)
                } else {
                    return None;
                };
                (start <= end).then_some(Range { start, end })
            }
            Unit::Word => {
                let (a, b) = if (self.anchor.line, self.anchor.cell) <= (self.head.line, self.head.cell) {
                    (self.anchor, self.head)
                } else {
                    (self.head, self.anchor)
                };
                let (a0, _) = word_bounds(term, a.line, a.cell);
                let (_, b1) = word_bounds(term, b.line, b.cell);
                Some(Range {
                    start: (a.line, a0),
                    end: (b.line, b1),
                })
            }
            Unit::Line => {
                let top = a.line.min(b.line);
                let bottom = a.line.max(b.line);
                Some(Range {
                    start: (logical_start(term, top), 0),
                    end: (logical_end(term, bottom), cols.saturating_sub(1)),
                })
            }
        }
    }
}

/// Character classes for double-click: a run of one class is a word. Paths and
/// URLs count as one word, which is what you want to copy in a terminal.
#[derive(PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Other(char),
}

fn class(ch: char) -> Class {
    if ch == ' ' || ch == '\t' || ch == '\0' {
        Class::Blank
    } else if ch.is_alphanumeric() || "_-./~:@%+#?=&,".contains(ch) {
        Class::Word
    } else {
        Class::Other(ch)
    }
}

fn char_at(term: &Term, line: usize, col: usize) -> char {
    let row = term.grid().line(line);
    row.cells()
        .get(col)
        .and_then(|c| c.scalar())
        .unwrap_or(' ')
}

/// The run of same-class cells around `col` on one row.
fn word_bounds(term: &Term, line: usize, col: usize) -> (usize, usize) {
    let cols = term.cols();
    let col = col.min(cols.saturating_sub(1));
    let here = class(char_at(term, line, col));
    if let Class::Other(_) = here {
        return (col, col);
    }
    let mut start = col;
    while start > 0 && class(char_at(term, line, start - 1)) == here {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols && class(char_at(term, line, end + 1)) == here {
        end += 1;
    }
    (start, end)
}

/// The word under a cell, as double-click would select it: a path or URL
/// counts as one word.
pub fn word_at(term: &Term, line: usize, col: usize) -> String {
    let (a, b) = word_bounds(term, line, col);
    (a..=b).map(|c| char_at(term, line, c)).collect::<String>().trim().to_string()
}

/// First row of the logical line containing `line`, following autowrap back.
fn logical_start(term: &Term, mut line: usize) -> usize {
    while line > 0 && term.grid().line(line - 1).wrapped {
        line -= 1;
    }
    line
}

/// Last row of the logical line containing `line`, following autowrap on.
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

    fn term_with(text: &str, cols: usize) -> Term {
        let mut t = Term::new(cols, 5, 100);
        Processor::new().advance(&mut t, text.as_bytes());
        t
    }

    fn pos(t: &Term, row: usize, col: usize) -> Pos {
        let first = t.grid().history_len();
        Pos { line: first + row, cell: col, edge: col }
    }

    fn text(t: &Term, s: &Selection) -> String {
        let r = s.range(t).unwrap();
        t.text_range(r.start.0, r.start.1, r.end.0, r.end.1)
    }

    #[test]
    fn char_drag_selects_between_boundaries() {
        let t = term_with("hello world", 40);
        let mut s = Selection::new(pos(&t, 0, 2), Unit::Char);
        s.extend(pos(&t, 0, 7));
        assert_eq!(text(&t, &s), "llo w");
        // Dragging backwards selects the same cells.
        let mut back = Selection::new(pos(&t, 0, 7), Unit::Char);
        back.extend(pos(&t, 0, 2));
        assert_eq!(text(&t, &back), "llo w");
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        let t = term_with("hello", 40);
        assert!(Selection::new(pos(&t, 0, 3), Unit::Char).range(&t).is_none());
    }

    #[test]
    fn double_click_takes_a_whole_path() {
        let t = term_with("cd ~/Github/TUI-Applications/Tachyon && ls", 60);
        let s = Selection::new(pos(&t, 0, 10), Unit::Word);
        assert_eq!(text(&t, &s), "~/Github/TUI-Applications/Tachyon");
    }

    #[test]
    fn punctuation_is_a_word_of_its_own() {
        let t = term_with("a (b) c", 20);
        let s = Selection::new(pos(&t, 0, 2), Unit::Word);
        assert_eq!(text(&t, &s), "(");
    }

    #[test]
    fn triple_click_takes_the_logical_line_across_wraps() {
        // 25 characters on a 10-column grid wrap onto three rows.
        let t = term_with("abcdefghijklmnopqrstuvwxy\r\nnext", 10);
        let s = Selection::new(pos(&t, 1, 3), Unit::Line);
        assert_eq!(text(&t, &s).trim_end(), "abcdefghijklmnopqrstuvwxy");
    }

    #[test]
    fn select_all_covers_everything() {
        let t = term_with("one\r\ntwo\r\nthree", 20);
        let all = text(&t, &Selection::all(&t));
        assert!(all.starts_with("one\ntwo\nthree"), "{all:?}");
    }
}
