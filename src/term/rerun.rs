//! Rerun diff: when a command runs again, which lines of its output differ
//! from the last run.
//!
//! "Again" means the same command line with numbers ignored, so a learning
//! rate sweep (`train.py --lr 3e-4`, then `--lr 1e-3`) compares run to run.
//! Lines are matched by shape first (the line with its numbers ignored), so
//! `epoch 2 loss=0.39` pairs with the old `epoch 2 loss=0.41` and reads as
//! changed rather than as one line removed and another added; a Myers diff on
//! the shapes does the matching.

use similar::{capture_diff_slices, Algorithm, DiffOp};

/// Outputs longer than this aren't compared: a flood isn't a run to compare.
pub const MAX_LINES: usize = 2000;

/// A line with each number replaced by `#`: `epoch 2 loss=0.41` and
/// `epoch 2 loss=0.39` have the same shape, `epoch 2` and `epoch 3` too.
pub fn shape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            // The rest of the number: digits, a point, an exponent.
            while let Some(&n) = chars.peek() {
                if n.is_ascii_digit() || n == '.' || n == 'e' || n == 'E' {
                    chars.next();
                } else {
                    break;
                }
            }
            out.push('#');
        } else {
            out.push(c);
        }
    }
    out
}

/// What a line of the new run is, compared with the last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// It differs from this line of the last run.
    Changed(String),
    /// The last run had nothing like it.
    New,
}

/// The lines of `new` that differ from `old`, by index into `new`.
pub fn diff(old: &[String], new: &[String]) -> Vec<(usize, Change)> {
    let old_shapes: Vec<String> = old.iter().map(|l| shape(l)).collect();
    let new_shapes: Vec<String> = new.iter().map(|l| shape(l)).collect();
    let mut out = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &old_shapes, &new_shapes) {
        match op {
            DiffOp::Equal { old_index, new_index, len } => {
                for k in 0..len {
                    if old[old_index + k] != new[new_index + k] {
                        out.push((new_index + k, Change::Changed(old[old_index + k].clone())));
                    }
                }
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                out.extend((new_index..new_index + new_len).map(|i| (i, Change::New)));
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                // Pair them up in order; what's left over is new.
                for k in 0..new_len {
                    let change = if k < old_len { Change::Changed(old[old_index + k].clone()) } else { Change::New };
                    out.push((new_index + k, change));
                }
            }
            DiffOp::Delete { .. } => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn shapes_ignore_numbers() {
        assert_eq!(shape("epoch 2  train 3.902  val 3.807  acc 12.4%"), "epoch #  train #  val #  acc #%");
        assert_eq!(shape("python train.py --lr 3e-4"), shape("python train.py --lr 1e-3"));
        assert_ne!(shape("make test"), shape("make bench"));
    }

    #[test]
    fn changed_lines_pair_with_their_old_selves() {
        let old = lines("epoch 1 loss=0.90\nepoch 2 loss=0.71\ndone");
        let new = lines("epoch 1 loss=0.90\nepoch 2 loss=0.65\ndone");
        assert_eq!(diff(&old, &new), vec![(1, Change::Changed("epoch 2 loss=0.71".into()))]);
    }

    #[test]
    fn new_and_missing_lines() {
        let old = lines("a\nb\nc");
        let new = lines("a\nwarning: x\nb\nc\nd");
        assert_eq!(diff(&old, &new), vec![(1, Change::New), (4, Change::New)]);
        // Lines only the old run had leave nothing to mark in the new one.
        assert_eq!(diff(&lines("a\nb\nc"), &lines("a\nc")), vec![]);
    }

    #[test]
    fn a_different_line_in_place_counts_as_changed() {
        let old = lines("test a ... ok\ntest b ... ok");
        let new = lines("test a ... ok\ntest b ... FAILED");
        assert_eq!(diff(&old, &new), vec![(1, Change::Changed("test b ... ok".into()))]);
    }
}
