//! Writing a terminal's state back out as the bytes that recreate it.
//!
//! The session host keeps a terminal for every shell, parsing its output, so
//! that when the app opens again it can be handed the screen as it is: the
//! scrollback, the screen (and the alternate screen a full-screen program is
//! drawing on), colours and attributes, wrapped lines, the cursor, modes,
//! title, folder, and the shell-integration marks. [`Term::to_vt`] writes all
//! of that as ordinary escape sequences, so restoring is just parsing, and
//! replaying them into a fresh terminal of the same size gives back the same
//! state.
//!
//! What isn't kept: how long restored commands took (they get no receipt
//! times), rerun-diff marks, character sets, and a pending wrap.

use std::fmt::Write as _;

use super::cell::{Cell, CellFlags, CLUSTER_TAG};
use super::color::Color;
use super::grid::RowMark;
use super::{CommandState, CursorShape, Term, TermMode};

impl Term {
    /// The bytes that turn a fresh terminal of this size into this one.
    pub fn to_vt(&self) -> Vec<u8> {
        let mut out = String::new();
        let alt = self.mode.contains(TermMode::ALT_SCREEN);

        // The folder, then the title if the shell set it or the program that
        // set it still runs.
        if let Some(cwd) = &self.cwd {
            let host = self.cwd_host.as_deref().unwrap_or("");
            let _ = write!(out, "\x1b]7;file://{host}{}\x07", url_path(cwd));
        }
        let title_live = match self.title_cmd {
            None => true,
            Some(id) => self.commands.get(&id).is_some_and(|c| c.state == CommandState::Running),
        };
        if let (Some(title), true) = (&self.title, title_live) {
            let _ = write!(out, "\x1b]2;{}\x07", title.replace(['\x07', '\x1b'], ""));
        }

        // The shell's screen, with its scrollback, and its marks.
        let primary = if alt { &self.inactive } else { &self.grid };
        let mut w = Writer { out, pen: None, term: self };
        let mut open: Option<(u32, CommandState)> = None;
        let total = primary.total_lines();
        for l in 0..total {
            let row = primary.line(l);
            w.marks(row.mark, &mut open);
            let last = l + 1 == total;
            // `wrapped` marks a row that continues on the next.
            let continues = !last && row.wrapped;
            w.row(row.cells(), primary.cols(), continues);
            if !last && !continues {
                w.reset();
                w.out.push_str("\r\n");
            }
        }
        // The command the last prompt belongs to: done ones get their exit.
        if let Some((id, CommandState::Done)) = open {
            w.finish(id);
        }

        // The cursor on the shell's screen: where the program that took the
        // alternate screen will return it.
        let shell_cursor = if alt { self.saved_cursor } else { self.cursor };
        w.reset();
        let _ = write!(w.out, "\x1b[{};{}H", shell_cursor.y + 1, shell_cursor.x + 1);

        if alt {
            // 1049 saves that cursor and clears the alternate screen.
            w.out.push_str("\x1b[?1049h");
            let screen = &self.grid;
            for y in 0..screen.rows() {
                let _ = write!(w.out, "\x1b[{};1H", y + 1);
                // Never let the last column wrap onto the next row.
                w.row(screen.row(y).cells(), screen.cols(), false);
            }
        }

        // Modes, the scroll region, the cursor and its pen.
        let modes: [(TermMode, &str); 11] = [
            (TermMode::APP_CURSOR, "?1"),
            (TermMode::REVERSE_VIDEO, "?5"),
            (TermMode::BRACKETED_PASTE, "?2004"),
            (TermMode::FOCUS_REPORT, "?1004"),
            (TermMode::MOUSE_CLICK, "?1000"),
            (TermMode::MOUSE_DRAG, "?1002"),
            (TermMode::MOUSE_MOTION, "?1003"),
            (TermMode::MOUSE_SGR, "?1006"),
            (TermMode::ALT_SCROLL, "?1007"),
            (TermMode::INSERT, "4"),
            (TermMode::LINE_FEED_NL, "20"),
        ];
        for (flag, code) in modes {
            if self.mode.contains(flag) {
                let _ = write!(w.out, "\x1b[{code}h");
            }
        }
        if !self.mode.contains(TermMode::AUTOWRAP) {
            w.out.push_str("\x1b[?7l");
        }
        if !self.mode.contains(TermMode::SHOW_CURSOR) {
            w.out.push_str("\x1b[?25l");
        }
        if self.mode.contains(TermMode::APP_KEYPAD) {
            w.out.push_str("\x1b=");
        }
        if self.scroll_top != 0 || self.scroll_bot + 1 != self.rows() {
            let _ = write!(w.out, "\x1b[{};{}r", self.scroll_top + 1, self.scroll_bot + 1);
        }
        let shape = match (self.cursor_shape, self.cursor_blinks) {
            (CursorShape::Block, true) => 1,
            (CursorShape::Block, false) => 2,
            (CursorShape::Underline, true) => 3,
            (CursorShape::Underline, false) => 4,
            (CursorShape::Beam, true) => 5,
            (CursorShape::Beam, false) => 6,
        };
        let _ = write!(w.out, "\x1b[{shape} q");
        let _ = write!(w.out, "\x1b[{};{}H", self.cursor.y + 1, self.cursor.x + 1);
        w.set_pen(&Cell { ch: 0, fg: self.cursor.pen.fg, bg: self.cursor.pen.bg, flags: self.cursor.pen.flags, underline: self.cursor.pen.underline });
        w.out.into_bytes()
    }
}

/// A path for a `file://` URL: bytes that would end or confuse the URL
/// percent-encoded.
fn url_path(path: &str) -> String {
    let mut s = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            let _ = write!(s, "%{b:02X}");
        }
    }
    s
}

struct Writer<'a> {
    out: String,
    /// The pen last set, so runs of the same attributes share one SGR.
    pen: Option<(Color, Color, CellFlags, u16)>,
    term: &'a Term,
}

impl Writer<'_> {
    fn reset(&mut self) {
        if self.pen.is_some_and(|p| p != (Color::DEFAULT, Color::DEFAULT, CellFlags::empty(), 0)) {
            self.out.push_str("\x1b[0m");
        }
        self.pen = Some((Color::DEFAULT, Color::DEFAULT, CellFlags::empty(), 0));
    }

    /// One row's cells. With `continues`, the row is written to its last
    /// column so the next character wraps, marking the next row wrapped.
    fn row(&mut self, cells: &[Cell], cols: usize, continues: bool) {
        let keep = |c: &Cell| c.ch != 0 || !c.bg.is_default() || c.flags.contains(CellFlags::REVERSE);
        let end = if continues { cols.min(cells.len()) } else { cells.iter().rposition(keep).map_or(0, |i| i + 1) };
        let mut x = 0;
        while x < end {
            let c = cells[x];
            if c.flags.contains(CellFlags::WIDE_SPACER) {
                x += 1;
                continue;
            }
            if c.ch == 0 {
                // A run of blanks: erase with their background, then step over,
                // so they stay blank rather than becoming spaces.
                let run = cells[x..end]
                    .iter()
                    .take_while(|b| b.ch == 0 && b.fg == c.fg && b.bg == c.bg && b.flags == c.flags)
                    .count();
                let stops_short = x + run < end || !continues;
                if !c.bg.is_default() || c.flags.contains(CellFlags::REVERSE) {
                    self.set_pen(&c);
                    let _ = write!(self.out, "\x1b[{run}X");
                }
                if stops_short {
                    let _ = write!(self.out, "\x1b[{run}C");
                    x += run;
                    continue;
                }
                // A wrapped row ending in blanks: the wrap needs a character
                // in the last column, so write spaces.
                self.set_pen(&c);
                for _ in 0..run {
                    self.out.push(' ');
                }
                x += run;
                continue;
            }
            self.set_pen(&c);
            if c.ch & CLUSTER_TAG != 0 {
                match self.term.cluster(c.ch) {
                    Some(chars) => self.out.extend(chars.iter()),
                    None => self.out.push(' '),
                }
            } else {
                self.out.push(char::from_u32(c.ch).unwrap_or(' '));
            }
            x += if c.flags.contains(CellFlags::WIDE) { 2 } else { 1 };
        }
    }

    fn set_pen(&mut self, c: &Cell) {
        let want = (c.fg, c.bg, c.flags - (CellFlags::WIDE | CellFlags::WIDE_SPACER), c.underline);
        if self.pen == Some(want) {
            return;
        }
        let (fg, bg, flags, underline) = want;
        let mut s = String::from("\x1b[0");
        for (flag, code) in [
            (CellFlags::BOLD, "1"),
            (CellFlags::DIM, "2"),
            (CellFlags::ITALIC, "3"),
            (CellFlags::UNDERLINE, "4"),
            (CellFlags::DOUBLE_UNDERLINE, "4:2"),
            (CellFlags::CURLY_UNDERLINE, "4:3"),
            (CellFlags::DOTTED_UNDERLINE, "4:4"),
            (CellFlags::DASHED_UNDERLINE, "4:5"),
            (CellFlags::BLINK, "5"),
            (CellFlags::REVERSE, "7"),
            (CellFlags::HIDDEN, "8"),
            (CellFlags::STRIKEOUT, "9"),
            (CellFlags::OVERLINE, "53"),
        ] {
            if flags.contains(flag) {
                s.push(';');
                s.push_str(code);
            }
        }
        color(&mut s, fg, 38);
        color(&mut s, bg, 48);
        if underline != 0 {
            if let Some(&c) = self.term.underline_colors.get(underline as usize - 1) {
                color(&mut s, c, 58);
            }
        }
        s.push('m');
        self.out.push_str(&s);
        self.pen = Some(want);
    }

    /// The shell-integration marks a row starts, in the order a shell sends
    /// them. `open` is the command whose marks were written last.
    fn marks(&mut self, mark: RowMark, open: &mut Option<(u32, CommandState)>) {
        if !mark.is_some() {
            return;
        }
        let info = self.term.commands.get(&mark.cmd);
        if mark.has(RowMark::PROMPT) && open.map(|o| o.0) != Some(mark.cmd) {
            if let Some((id, CommandState::Done)) = *open {
                self.finish(id);
            }
            self.out.push_str("\x1b]133;A\x07");
            *open = Some((mark.cmd, info.map_or(CommandState::Prompt, |c| c.state)));
        }
        if open.map(|o| o.0) != Some(mark.cmd) {
            return;
        }
        if mark.has(RowMark::INPUT) {
            self.out.push_str("\x1b]133;B\x07");
        }
        if mark.has(RowMark::OUTPUT) {
            let cmdline = info.and_then(|c| c.cmdline.as_deref()).unwrap_or("");
            let _ = write!(self.out, "\x1b]133;C;cmdline={}\x07", cmdline.replace(['\x07', '\x1b'], ""));
        }
    }

    /// End command `id`, open at a prompt: a start if its row lost one (it
    /// printed nothing, so the next prompt took the row), then its exit.
    fn finish(&mut self, id: u32) {
        let info = self.term.commands.get(&id);
        let cmdline = info.and_then(|c| c.cmdline.as_deref()).unwrap_or("");
        let _ = write!(self.out, "\x1b]133;C;cmdline={}\x07", cmdline.replace(['\x07', '\x1b'], ""));
        match info.and_then(|c| c.exit) {
            Some(e) => {
                let _ = write!(self.out, "\x1b]133;D;{e}\x07");
            }
            None => self.out.push_str("\x1b]133;D\x07"),
        }
    }
}

fn color(s: &mut String, c: Color, base: u8) {
    if c.is_default() {
        return;
    }
    if let Some(i) = c.index() {
        let _ = write!(s, ";{base};5;{i}");
    } else {
        let (r, g, b) = ((c.0 >> 16) & 0xFF, (c.0 >> 8) & 0xFF, c.0 & 0xFF);
        let _ = write!(s, ";{base};2;{r};{g};{b}");
    }
}

/// Whether a grid's rows have any shell-integration marks, for tests.
#[cfg(test)]
fn marked(g: &super::grid::Grid) -> Vec<(usize, u8)> {
    (0..g.total_lines()).filter_map(|l| g.line(l).mark.is_some().then(|| (l, g.line(l).mark.kinds))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Processor;

    fn feed(t: &mut Term, s: &str) {
        let mut p = Processor::new();
        p.advance(t, s.as_bytes());
    }

    fn restored(t: &Term) -> Term {
        let mut r = Term::new(t.cols(), t.rows(), 1000);
        r.restoring = true;
        let mut p = Processor::new();
        p.advance(&mut r, &t.to_vt());
        r.restoring = false;
        r
    }

    /// Every line of a grid, as cells, for comparing.
    fn lines(g: &crate::term::Grid) -> Vec<(Vec<Cell>, bool)> {
        (0..g.total_lines()).map(|l| (g.line(l).cells().to_vec(), g.line(l).wrapped)).collect()
    }

    fn same(a: &Term, b: &Term) {
        assert_eq!(lines(a.grid()), lines(b.grid()), "screen and scrollback");
        assert_eq!(lines(&a.inactive), lines(&b.inactive), "the other screen");
        assert_eq!((a.cursor.x, a.cursor.y), (b.cursor.x, b.cursor.y), "cursor");
        assert_eq!(a.mode, b.mode, "modes");
        assert_eq!(a.title, b.title);
        assert_eq!(a.cwd, b.cwd);
        assert_eq!((a.scroll_top, a.scroll_bot), (b.scroll_top, b.scroll_bot));
    }

    #[test]
    fn text_colours_and_scrollback_come_back() {
        let mut t = Term::new(20, 5, 1000);
        feed(&mut t, "\x1b]7;file://mac/Users/me/My%20Project\x07\x1b]2;build\x07");
        for i in 0..12 {
            feed(&mut t, &format!("line {i} \x1b[1;31mred\x1b[0m \x1b[38;2;10;20;30;48;5;200mrgb\x1b[0m\r\n"));
        }
        feed(&mut t, "\x1b[4:3;58;5;9mcurly\x1b[0m 日本 e\u{301} \x1b[44m  \x1b[0m end");
        let r = restored(&t);
        same(&t, &r);
        assert_eq!(r.cwd.as_deref(), Some("/Users/me/My Project"));
        assert!(r.grid().history_len() > 0);
    }

    #[test]
    fn wrapped_lines_stay_wrapped() {
        let mut t = Term::new(10, 4, 100);
        feed(&mut t, "abcdefghijklmnopqrstuvwxy\r\nshort\r\n0123456789          x");
        let r = restored(&t);
        same(&t, &r);
        assert!(r.grid().row(1).wrapped || r.grid().line(1).wrapped);
    }

    #[test]
    fn the_alternate_screen_and_modes_come_back() {
        let mut t = Term::new(20, 6, 100);
        feed(&mut t, "$ htop\r\n");
        feed(&mut t, "\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?25l\x1b[5 q\x1b[2;5r");
        feed(&mut t, "\x1b[1;1H\x1b[7m CPU 59% \x1b[0m\x1b[3;2Hrow three\x1b[6;20HZ\x1b[4;7H");
        let r = restored(&t);
        same(&t, &r);
        assert_eq!(r.cursor_state().shape, CursorShape::Beam);
        // Leaving the program's screen goes back to the shell's, cursor and all.
        let (mut a, mut b) = (t, r);
        feed(&mut a, "\x1b[?1049l");
        feed(&mut b, "\x1b[?1049l");
        same(&a, &b);
    }

    #[test]
    fn commands_keep_their_marks_and_exits_but_no_times() {
        let mut t = Term::new(30, 8, 100);
        feed(&mut t, "\x1b]133;A\x07~ \x1b]133;B\x07make\r\n\x1b]133;C;cmdline=make\x07error\r\n\x1b]133;D;2\x07");
        feed(&mut t, "\x1b]133;A\x07~ \x1b]133;B\x07cd x\r\n\x1b]133;C;cmdline=cd x\x07\x1b]133;D;0\x07");
        feed(&mut t, "\x1b]133;A\x07~ \x1b]133;B\x07");
        let mut r = restored(&t);
        same(&t, &r);
        assert_eq!(marked(t.grid()), marked(r.grid()));
        let exits = |t: &Term| {
            let mut v: Vec<_> = t.commands.values().map(|c| (c.cmdline.clone(), c.exit, c.state)).collect();
            v.sort_by_key(|c| c.0.clone());
            v
        };
        assert_eq!(exits(&t), exits(&r));
        assert!(r.commands.values().all(|c| c.started.is_none()), "no made-up times");
        assert!(r.take_command_events().is_empty(), "nothing to notify about");
    }
}
