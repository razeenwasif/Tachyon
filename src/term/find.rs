//! Find in scrollback: every match of a search across the scrollback and the
//! screen, oldest first. Lines wrapped by the terminal are searched as the
//! one line they were written as, so a match can run across a wrap.

use regex::{Regex, RegexBuilder};

use super::cell::{CellFlags, CLUSTER_TAG};
use super::Term;

/// Where a match is: from (line, cell) up to but not including (line, cell),
/// lines counted from the oldest in scrollback, as `Grid::line` counts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// What to look for. Case is ignored unless the query has a capital
/// ("smart case"); with `regex`, the query is a regular expression.
pub fn pattern(query: &str, regex: bool) -> Result<Regex, String> {
    let source = if regex { query.to_string() } else { regex::escape(query) };
    let insensitive = !query.chars().any(char::is_uppercase);
    RegexBuilder::new(&source).case_insensitive(insensitive).build().map_err(|e| {
        // The first line of the error is the useful part.
        e.to_string().lines().last().unwrap_or("not a valid expression").trim().to_string()
    })
}

/// Every match of `re`, oldest first, up to `limit` of them.
pub fn find(term: &Term, re: &Regex, limit: usize) -> Vec<Match> {
    let grid = term.grid();
    let total = grid.total_lines();
    let mut out = Vec::new();
    let mut text = String::new();
    // The cell each character of `text` starts at, by byte offset.
    let mut at: Vec<(usize, usize, usize)> = Vec::new();
    let mut l = 0;
    while l < total && out.len() < limit {
        text.clear();
        at.clear();
        // One logical line: this row and the rows it wraps onto.
        loop {
            // Most scrollback is plain ASCII: read its bytes as they're kept.
            if let Some((bytes, wrapped)) = grid.line_ascii(l) {
                for (x, &b) in bytes.iter().enumerate() {
                    at.push((text.len(), l, x));
                    text.push(if b == 0 { ' ' } else { b as char });
                }
                l += 1;
                if !wrapped || l >= total {
                    break;
                }
                // A wrapped line is full width: the rest of its cells are blank.
                for x in bytes.len()..grid.cols() {
                    at.push((text.len(), l - 1, x));
                    text.push(' ');
                }
                continue;
            }
            let row = grid.line(l);
            for (x, c) in row.cells().iter().enumerate() {
                if c.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                at.push((text.len(), l, x));
                if c.ch & CLUSTER_TAG != 0 {
                    text.extend(term.cluster(c.ch).unwrap_or(&[' ']));
                } else {
                    text.push(char::from_u32(c.ch).filter(|&ch| ch != '\0').unwrap_or(' '));
                }
            }
            let wrapped = row.wrapped;
            l += 1;
            if !wrapped || l >= total {
                break;
            }
        }
        // Trailing blanks are padding, not text.
        let trimmed = text.trim_end_matches(' ').len();
        let cell_at = |byte: usize| -> (usize, usize) {
            let i = at.partition_point(|&(b, _, _)| b <= byte).saturating_sub(1);
            (at[i].1, at[i].2)
        };
        for m in re.find_iter(&text[..trimmed]) {
            if m.is_empty() {
                continue;
            }
            let start = cell_at(m.start());
            let last = cell_at(m.end() - 1);
            // Past the last character's cell, which a wide one takes two of.
            let wide = grid.line(last.0).cells().get(last.1).is_some_and(|c| c.flags.contains(CellFlags::WIDE));
            out.push(Match { start, end: (last.0, last.1 + if wide { 2 } else { 1 }) });
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Processor;

    fn term(cols: usize, rows: usize, text: &str) -> Term {
        let mut t = Term::new(cols, rows, 1000);
        Processor::new().advance(&mut t, text.as_bytes());
        t
    }

    fn found(t: &Term, q: &str, regex: bool) -> Vec<Match> {
        find(t, &pattern(q, regex).unwrap(), 1000)
    }

    #[test]
    fn finds_in_scrollback_and_on_screen() {
        let mut text = String::new();
        for i in 0..50 {
            text.push_str(&format!("epoch {i} loss=0.{i:02}\r\n"));
        }
        let t = term(30, 5, &text);
        let m = found(&t, "epoch 7 ", false);
        assert_eq!(m, vec![Match { start: (7, 0), end: (7, 8) }]);
        assert_eq!(found(&t, "loss=0.4", false).len(), 10, "40 to 49");
        assert_eq!(found(&t, "nothing", false), vec![]);
    }

    #[test]
    fn smart_case_and_regex() {
        let t = term(40, 4, "Error: disk full\r\nerror: retry\r\nERROR\r\n");
        assert_eq!(found(&t, "error", false).len(), 3, "lower case ignores case");
        assert_eq!(found(&t, "Error", false).len(), 1, "a capital makes it exact");
        assert_eq!(found(&t, r"e\w+:", true).len(), 2);
        assert_eq!(found(&t, "a.b", false), vec![], "plain text: the dot is a dot");
        assert!(pattern("(unclosed", true).is_err());
    }

    #[test]
    fn a_match_can_run_across_a_wrap() {
        let t = term(10, 4, "0123456789abcdefghij\r\n");
        assert_eq!(found(&t, "89ab", false), vec![Match { start: (0, 8), end: (1, 2) }]);
        // The same once it has scrolled into scrollback (read as stored bytes).
        let t = term(10, 2, "0123456789abcdefghij\r\nx\r\ny\r\nz\r\n");
        assert!(t.grid().line_ascii(1).is_some(), "in scrollback, kept as ASCII");
        assert_eq!(found(&t, "89ab", false), vec![Match { start: (0, 8), end: (1, 2) }]);
        assert_eq!(found(&t, "y", false), vec![Match { start: (3, 0), end: (3, 1) }]);
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let t = term(20, 3, "a 日本語 b\r\n");
        assert_eq!(found(&t, "本", false), vec![Match { start: (0, 4), end: (0, 6) }]);
    }

    /// How long a search over a full 100,000-line scrollback takes. Run with
    /// `cargo test --release find_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn find_cost() {
        let mut t = Term::new(200, 50, 100_000);
        let mut p = Processor::new();
        for i in 0..100_050 {
            p.advance(&mut t, format!("   Compiling some-crate-name v0.{}.{} (/home/user/projects/workspace/crate-{})\r\n", i % 40, i % 13, i % 97).as_bytes());
        }
        for (q, regex) in [("crate-42)", false), ("v0.3.7 ", false), (r"crate-\d{2}\)$", true), ("nothing like this", false)] {
            let re = pattern(q, regex).unwrap();
            let start = std::time::Instant::now();
            let n = find(&t, &re, 10_000).len();
            println!("{q:<20} {n:>6} matches in {:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
