//! VoiceOver & macOS Accessibility (`NSAccessibility`) grid projection.
//!
//! Translates the visible terminal viewport of the focused pane (or an active
//! Rewind frame) into a UTF-16-indexed text model matching AppKit's `AXTextArea`
//! protocol: full screen text, line ranges, cursor insertion line/offset,
//! active selection mapping, safe UTF-16 substring slicing, and cell-grid
//! bounding rectangles for `accessibilityFrameForRange:`.
//!
//! Keeping the projection pure Rust makes every UTF-16 index, wide-glyph,
//! emoji-cluster, scrollback-offset, and selection edge case unit-testable
//! without needing VoiceOver active.

use crate::term::{CellFlags, Term, CLUSTER_TAG};

use super::selection::Selection;

/// One visible terminal row projected into UTF-16 coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A11yLine {
    /// UTF-16 offset in `A11yGrid::text` where this row begins.
    pub utf16_start: usize,
    /// UTF-16 code units in this row excluding any trailing `'\n'`.
    pub utf16_len: usize,
    /// UTF-16 code units in this row including the trailing `'\n'` (all rows except the last).
    pub utf16_len_with_nl: usize,
    /// For each column `0..=cols`, its UTF-16 offset relative to `utf16_start`.
    col_utf16: Vec<usize>,
}

/// Complete VoiceOver snapshot of a pane's visible grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A11yGrid {
    pub text: String,
    pub utf16_len: usize,
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<A11yLine>,
    /// 0-indexed visible viewport row containing the insertion cursor.
    pub cursor_row: usize,
    /// UTF-16 code-unit offset of the insertion cursor inside `text`.
    pub cursor_utf16: usize,
    /// Selected text (may span beyond the visible viewport into scrollback).
    pub selected_text: Option<String>,
    /// Active selection projected onto visible UTF-16 `(location, length)`,
    /// or `(cursor_utf16, 0)` when nothing is selected.
    pub selected_range: (usize, usize),
}

impl A11yGrid {
    /// Build a VoiceOver snapshot of `term`'s visible viewport and optional `selection`.
    pub fn from_term(term: &Term, selection: Option<&Selection>) -> A11yGrid {
        let cols = term.cols();
        let rows = term.rows();
        let grid = term.grid();
        let cursor = term.cursor_state();
        let first_abs = grid.history_len().saturating_sub(grid.display_offset());
        let cursor_abs = grid.history_len() + cursor.y;
        let cursor_on_screen = rows > 0 && cursor_abs >= first_abs && cursor_abs < first_abs + rows;
        let cursor_row = if cursor_on_screen {
            cursor_abs - first_abs
        } else {
            rows.saturating_sub(1)
        };
        let cursor_col = if cursor_on_screen { cursor.x.min(cols) } else { 0 };

        let mut text = String::with_capacity(rows * (cols / 2 + 1));
        let mut lines = Vec::with_capacity(rows);
        let mut total_utf16 = 0usize;

        for y in 0..rows {
            let row = grid.row(y);
            let cells = row.cells();
            let trimmed = row.trimmed_len().min(cols);
            // Keep trailing spaces up to the insertion cursor on the active
            // prompt row so VoiceOver places the caret after `"❯ "` rather than
            // collapsing into `"❯"`.
            let keep = if cursor_on_screen && y == cursor_row {
                trimmed.max(cursor_col)
            } else {
                trimmed
            };

            let utf16_start = total_utf16;
            let mut local_utf16 = 0usize;
            let mut last_char_utf16 = 0usize;
            let mut col_utf16 = Vec::with_capacity(cols + 1);

            for col in 0..cols {
                if col < keep {
                    let c = &cells[col];
                    if c.flags.contains(CellFlags::WIDE_SPACER) {
                        // Second column of a wide glyph shares the glyph's starting UTF-16 offset
                        // so column <-> UTF-16 lookup spans both columns cleanly.
                        col_utf16.push(last_char_utf16);
                        continue;
                    }
                    last_char_utf16 = local_utf16;
                    col_utf16.push(local_utf16);
                    if c.ch & CLUSTER_TAG != 0 {
                        if let Some(cl) = term.cluster(c.ch) {
                            for &ch in cl {
                                text.push(ch);
                                local_utf16 += ch.len_utf16();
                            }
                        }
                    } else if let Some(ch) = c.scalar() {
                        text.push(ch);
                        local_utf16 += ch.len_utf16();
                    } else {
                        text.push(' ');
                        local_utf16 += 1;
                    }
                } else {
                    col_utf16.push(local_utf16);
                }
            }
            col_utf16.push(local_utf16);

            let utf16_len = local_utf16;
            let has_nl = y + 1 < rows;
            if has_nl {
                text.push('\n');
            }
            let utf16_len_with_nl = utf16_len + usize::from(has_nl);
            lines.push(A11yLine {
                utf16_start,
                utf16_len,
                utf16_len_with_nl,
                col_utf16,
            });
            total_utf16 += utf16_len_with_nl;
        }

        let cursor_utf16 = lines
            .get(cursor_row)
            .map(|l| l.utf16_start + l.col_utf16[cursor_col.min(cols)])
            .unwrap_or(0);

        let mut selected_text = None;
        let mut selected_range = (cursor_utf16, 0);

        if let Some(sel) = selection {
            if let Some(range) = sel.range(term) {
                let s = term.text_range(range.start.0, range.start.1, range.end.0, range.end.1);
                selected_text = Some(s);
                let last_abs = first_abs + rows;
                if rows > 0 && range.end.0 >= first_abs && range.start.0 < last_abs {
                    let start_row = range.start.0.saturating_sub(first_abs);
                    let start_col = if range.start.0 < first_abs {
                        0
                    } else {
                        range.start.1.min(cols)
                    };
                    let end_row = (range.end.0 - first_abs).min(rows - 1);
                    let end_col = if range.end.0 >= last_abs {
                        cols
                    } else {
                        (range.end.1 + 1).min(cols)
                    };
                    let loc = lines[start_row].utf16_start + lines[start_row].col_utf16[start_col];
                    let end = lines[end_row].utf16_start + lines[end_row].col_utf16[end_col];
                    selected_range = (loc, end.saturating_sub(loc));
                }
            }
        }

        A11yGrid {
            text,
            utf16_len: total_utf16,
            cols,
            rows,
            lines,
            cursor_row,
            cursor_utf16,
            selected_text,
            selected_range,
        }
    }

    /// 0-based visible line containing UTF-16 code-unit `index`.
    pub fn line_for_utf16(&self, index: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let idx = index.min(self.utf16_len);
        // Binary search by line end; the final line covers `idx == utf16_len`.
        let pos = self
            .lines
            .partition_point(|l| l.utf16_start + l.utf16_len_with_nl <= idx);
        pos.min(self.lines.len() - 1)
    }

    /// UTF-16 `(location, length)` range for 0-based visible `line` (including
    /// the trailing newline on non-final lines, matching `NSTextView`).
    pub fn range_for_line(&self, line: usize) -> Option<(usize, usize)> {
        self.lines.get(line).map(|l| (l.utf16_start, l.utf16_len_with_nl))
    }

    /// Safely slice `self.text` by UTF-16 `(location, length)`.
    /// Surrogate pairs that straddle an endpoint are handled without panicking.
    pub fn substring_utf16(&self, loc: usize, len: usize) -> String {
        if len == 0 || loc >= self.utf16_len {
            return String::new();
        }
        let end = loc.saturating_add(len).min(self.utf16_len);
        let mut out = String::new();
        let mut cur = 0usize;
        for ch in self.text.chars() {
            let w = ch.len_utf16();
            let next = cur + w;
            if next > loc && cur < end {
                out.push(ch);
            }
            cur = next;
            if cur >= end {
                break;
            }
        }
        out
    }

    /// Map a UTF-16 `(location, length)` range onto visible grid cell bounds
    /// `(start_row, start_col, end_row, end_col_exclusive)`.
    ///
    /// For zero-length ranges (insertion point), returns a 1-cell box at the
    /// character position so VoiceOver can draw a focus cursor around it.
    pub fn cell_bounds_for_utf16(&self, loc: usize, len: usize) -> (usize, usize, usize, usize) {
        if self.lines.is_empty() || self.cols == 0 {
            return (0, 0, 0, 1);
        }
        let clamped_loc = loc.min(self.utf16_len);
        let start_row = self.line_for_utf16(clamped_loc);
        let start_col = self.col_for_utf16_in_line(start_row, clamped_loc).min(self.cols.saturating_sub(1));

        if len == 0 {
            return (start_row, start_col, start_row, (start_col + 1).min(self.cols));
        }

        let end_utf16 = clamped_loc.saturating_add(len).min(self.utf16_len);
        let last_char = end_utf16.saturating_sub(1);
        let end_row = self.line_for_utf16(last_char);
        if end_row > start_row {
            // Multi-line range spans the full width of those rows.
            return (start_row, 0, end_row, self.cols);
        }
        let end_col = self
            .col_for_utf16_in_line(end_row, end_utf16)
            .max(start_col + 1)
            .min(self.cols);
        (start_row, start_col, end_row, end_col)
    }

    fn col_for_utf16_in_line(&self, row: usize, abs_utf16: usize) -> usize {
        let line = &self.lines[row];
        let rel = abs_utf16.saturating_sub(line.utf16_start).min(line.utf16_len);
        // First column whose UTF-16 offset reaches `rel`. Wide-glyph spacers
        // carry their leading glyph's offset, so both start and end boundaries
        // land cleanly on full glyph cell edges.
        let pos = line.col_utf16.partition_point(|&off| off < rel);
        pos.min(self.cols)
    }
}

/// Descriptive VoiceOver label for the focused terminal surface.
pub fn accessibility_label(
    tab_title: &str,
    host: Option<&str>,
    rewound: bool,
    broadcasting: bool,
) -> String {
    let title = if tab_title.trim().is_empty() {
        "Terminal"
    } else {
        tab_title.trim()
    };
    let mut tags: Vec<&str> = Vec::new();
    if let Some(h) = host.filter(|h| !h.is_empty() && *h != "local") {
        tags.push(h);
    }
    if rewound {
        tags.push("Rewind");
    }
    if broadcasting {
        tags.push("Broadcast");
    }
    if tags.is_empty() {
        format!("Tachyon — {title}")
    } else {
        format!("Tachyon — {title} ({})", tags.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac::selection::{Pos, Unit};
    use crate::term::Processor;

    fn term_with(cols: usize, rows: usize, bytes: &str) -> Term {
        let mut t = Term::new(cols, rows, 64);
        let mut p = Processor::new();
        p.advance(&mut t, bytes.as_bytes());
        t
    }

    #[test]
    fn grid_projects_prompt_output_and_cursor_space() {
        let t = term_with(40, 4, "hello world\r\n❯ ");
        let g = A11yGrid::from_term(&t, None);

        assert_eq!(g.rows, 4);
        assert_eq!(g.lines.len(), 4);
        // Row 0: "hello world", Row 1: "❯ " (trailing space preserved up to cursor!), Rows 2..3 empty.
        assert_eq!(g.text, "hello world\n❯ \n\n");
        assert_eq!(g.cursor_row, 1);
        // "hello world\n" = 12 UTF-16 units; "❯ " = 2 UTF-16 units => cursor at 14.
        assert_eq!(g.cursor_utf16, 14);
        assert_eq!(g.selected_range, (14, 0));
        assert_eq!(g.selected_text, None);

        // Line <-> index round-trip.
        assert_eq!(g.range_for_line(0), Some((0, 12)));
        assert_eq!(g.range_for_line(1), Some((12, 3)));
        assert_eq!(g.range_for_line(2), Some((15, 1)));
        assert_eq!(g.range_for_line(3), Some((16, 0)));
        assert_eq!(g.range_for_line(4), None);

        assert_eq!(g.line_for_utf16(0), 0);
        assert_eq!(g.line_for_utf16(11), 0);
        assert_eq!(g.line_for_utf16(12), 1);
        assert_eq!(g.line_for_utf16(14), 1);
        assert_eq!(g.line_for_utf16(15), 2);
        assert_eq!(g.line_for_utf16(16), 3);

        assert_eq!(g.substring_utf16(6, 5), "world");
        assert_eq!(g.cell_bounds_for_utf16(6, 5), (0, 6, 0, 11));
        assert_eq!(g.cell_bounds_for_utf16(14, 0), (1, 2, 1, 3));
    }

    #[test]
    fn grid_handles_utf16_surrogate_pairs_and_wide_glyphs() {
        // 🚀 is 2 UTF-16 code units and 2 terminal columns; 中 is 1 UTF-16 code unit and 2 columns.
        let t = term_with(20, 2, "a🚀中b");
        let g = A11yGrid::from_term(&t, None);

        assert_eq!(g.text, "a🚀中b\n");
        // UTF-16 lengths: 'a'(1) + '🚀'(2) + '中'(1) + 'b'(1) = 5, plus '\n'(1) = 6.
        assert_eq!(g.utf16_len, 6);
        assert_eq!(g.substring_utf16(1, 2), "🚀");
        assert_eq!(g.substring_utf16(3, 1), "中");
        assert_eq!(g.substring_utf16(4, 1), "b");
        // Mid-surrogate slice must not panic.
        assert_eq!(g.substring_utf16(2, 2), "🚀中");
        // Wide glyphs span both terminal columns; 'b' starts at column 5 (a=0, 🚀=1..3, 中=3..5, b=5).
        assert_eq!(g.cell_bounds_for_utf16(1, 2), (0, 1, 0, 3));
        assert_eq!(g.cell_bounds_for_utf16(3, 1), (0, 3, 0, 5));
        assert_eq!(g.cell_bounds_for_utf16(4, 1), (0, 5, 0, 6));
        assert_eq!(g.cursor_utf16, 5);
    }

    #[test]
    fn grid_maps_active_selection_and_multiline_bounds() {
        let t = term_with(20, 3, "alpha beta\r\ngamma delta\r\n");
        let hist = t.grid().history_len();
        let mut sel = Selection::new(Pos { line: hist, cell: 6, edge: 6 }, Unit::Char);
        sel.extend(Pos { line: hist + 1, cell: 5, edge: 5 });

        let g = A11yGrid::from_term(&t, Some(&sel));
        assert_eq!(g.selected_text.as_deref(), Some("beta\ngamma"));
        let (loc, len) = g.selected_range;
        assert_eq!(g.substring_utf16(loc, len), "beta\ngamma");
        // Multi-line selection covers full width of rows 0..=1.
        assert_eq!(g.cell_bounds_for_utf16(loc, len), (0, 0, 1, 20));
    }

    #[test]
    fn grid_respects_scrollback_viewport_offset() {
        let mut t = term_with(20, 2, "L0\r\nL1\r\nL2\r\nL3");
        assert!(t.grid().history_len() >= 2);
        t.scroll_display(2);
        let g = A11yGrid::from_term(&t, None);
        assert_eq!(g.text, "L0\nL1");
        assert_eq!(g.range_for_line(0), Some((0, 3)));
        assert_eq!(g.range_for_line(1), Some((3, 2)));
    }

    #[test]
    fn accessibility_label_includes_context_tags() {
        assert_eq!(accessibility_label("zsh", None, false, false), "Tachyon — zsh");
        assert_eq!(accessibility_label("zsh", Some("local"), false, false), "Tachyon — zsh");
        assert_eq!(
            accessibility_label("htop", Some("kagutsuchi"), true, false),
            "Tachyon — htop (kagutsuchi, Rewind)"
        );
        assert_eq!(
            accessibility_label("bash", Some("amaterasu"), false, true),
            "Tachyon — bash (amaterasu, Broadcast)"
        );
        assert_eq!(accessibility_label("  ", None, false, false), "Tachyon — Terminal");
    }
}
