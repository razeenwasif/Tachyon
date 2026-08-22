//! Regression tests built from real ConPTY output.
//!
//! Windows' pseudoconsole re-renders its own screen buffer and re-emits VT, so
//! the byte stream a terminal receives does not look like what the application
//! wrote. These cases were captured from an actual session (via `TACHYON_DUMP`)
//! and reduced; they exercise shapes that hand-written tests kept missing.

use tachyon::term::{CellFlags, Processor, Term};

fn run(stream: &[u8], cols: usize, rows: usize) -> Term {
    let mut t = Term::new(cols, rows, 500);
    let mut p = Processor::new();
    p.advance(&mut t, stream);
    t
}

fn text(t: &Term, y: usize, n: usize) -> String {
    let cells = t.grid().row(y).cells();
    (0..n.min(cells.len()))
        .map(|x| cells[x].scalar().unwrap_or(' '))
        .collect()
}

/// ConPTY sets the attribute *before* moving the cursor, and turns reverse off
/// with SGR 27 rather than a full reset. The characters must survive both.
///
/// This is the exact shape that exposed a renderer bug where reverse-video
/// cells drew as a solid block with no text.
#[test]
fn reverse_set_before_cursor_move_keeps_text() {
    let t = run(b"\x1b[K\x1b[7m\x1b[3;1H reverse \x1b[27m tail", 40, 5);

    assert_eq!(&text(&t, 2, 14), " reverse  tail");
    for x in 0..9 {
        assert!(
            t.grid().cell(x, 2).flags.contains(CellFlags::REVERSE),
            "cell {x} should be reversed"
        );
    }
    // SGR 27 must clear it without disturbing the characters after it.
    for x in 9..14 {
        assert!(
            !t.grid().cell(x, 2).flags.contains(CellFlags::REVERSE),
            "cell {x} should not be reversed"
        );
    }
}

/// ConPTY splits styling across separate SGR sequences and terminates lines
/// with an explicit erase-to-end-of-line.
#[test]
fn split_sgr_with_trailing_erase() {
    let t = run(
        b"\x1b[38;5;53mcoloured\x1b[K\x1b[m\r\n\x1b[4:3m\x1b[58;5;203mcurly\x1b[24m done",
        40,
        4,
    );

    assert_eq!(&text(&t, 0, 8), "coloured");
    assert_eq!(&text(&t, 1, 10), "curly done");

    let curly = t.grid().cell(0, 1);
    assert!(curly.flags.contains(CellFlags::CURLY_UNDERLINE));
    assert_ne!(curly.underline, 0, "underline colour should be recorded");

    // SGR 24 clears every underline style.
    assert!(!t
        .grid()
        .cell(6, 1)
        .flags
        .intersects(CellFlags::ANY_UNDERLINE));
}

/// A scroll region confines output, and resetting it with `CSI r` restores
/// full-screen addressing.
#[test]
fn scroll_region_then_reset() {
    let mut stream = Vec::new();
    stream.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[H");
    stream.extend_from_slice(b"header");
    stream.extend_from_slice(b"\x1b[5;12r\x1b[5;1H");
    for i in 1..=24 {
        stream.extend_from_slice(format!("  region line {i}\r\n").as_bytes());
    }
    stream.extend_from_slice(b"\x1b[r\x1b[14;1Hafter");

    let t = run(&stream, 40, 32);

    // Row 0 is outside the region and must be untouched.
    assert_eq!(&text(&t, 0, 6), "header");
    // Each line is followed by a newline, so the 24th also scrolls: the region
    // ends up holding lines 18-24 with a blank row at the bottom.
    assert_eq!(text(&t, 4, 20).trim_end(), "  region line 18");
    assert_eq!(text(&t, 10, 20).trim_end(), "  region line 24");
    assert_eq!(text(&t, 11, 20).trim_end(), "");
    // Addressing outside the old region works again after `CSI r`.
    assert_eq!(&text(&t, 13, 5), "after");
}
