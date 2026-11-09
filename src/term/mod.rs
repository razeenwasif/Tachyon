//! Terminal state: grid, cursor, modes, and the VT dispatch implementation.

pub mod jobs;
pub mod lens;
pub mod rerun;
pub mod rewind;
pub mod find;
pub mod image;
mod restore;
pub mod cell;
pub mod color;
pub mod grid;
pub mod parser;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bitflags::bitflags;
use rustc_hash::FxHashMap;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub use cell::{Cell, CellFlags, Pen, CLUSTER_TAG};
pub use color::{Color, Palette};
pub use grid::{Grid, RowMark};
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

/// Where a command is in its life, as the shell reported it through OSC 133.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandState {
    /// The prompt is showing; nothing has run yet.
    Prompt,
    Running,
    Done,
}

/// What a command cost, filled in by the platform layer. CPU time is exact;
/// the sampled figures are present only for commands that ran long enough to
/// be sampled.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Receipt {
    pub cpu: Option<Duration>,
    /// Share of CPU time spent on performance cores, 0..=1.
    pub pcore_share: Option<f32>,
    pub energy_joules: Option<f64>,
    pub peak_footprint: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub state: CommandState,
    pub started: Option<Instant>,
    pub finished: Option<Instant>,
    pub exit: Option<i32>,
    pub receipt: Option<Receipt>,
    /// The command line, when the shell reports it (`OSC 133;C;cmdline=...`).
    pub cmdline: Option<String>,
    /// Metrics seen in its output while it ran.
    pub lens: lens::Lens,
    /// The first job it submitted to a cluster, if it did.
    pub job: Option<jobs::JobRef>,
    /// How its output differs from the last run of the same command, by
    /// logical output line; `None` when there was no last run to compare.
    pub rerun: Option<Vec<(usize, rerun::Change)>>,
}

impl CommandInfo {
    /// How long it ran, or has been running.
    pub fn elapsed(&self) -> Option<Duration> {
        let start = self.started?;
        Some(self.finished.unwrap_or_else(Instant::now).duration_since(start))
    }
}

/// A command started or finished. Drained by the platform layer, which
/// measures what the command cost.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandEvent {
    Started(u32),
    Finished(u32, Option<i32>),
}

/// Commands remembered for gutter marks and receipts.
const MAX_COMMANDS: u32 = 4096;

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
    /// The command that was running when the title was set, or `None` when
    /// the shell itself set it (at a prompt, or without shell integration).
    pub title_cmd: Option<u32>,
    /// The working directory the shell last reported with OSC 7.
    pub cwd: Option<String>,
    /// The host in that report.
    cwd_host: Option<String>,
    /// An ssh command running in this terminal, and the host the shell
    /// reported before it, so jobs submitted on the far side can be followed
    /// through the same ssh. Over when OSC 7 reports that host again.
    ssh_session: Option<(String, Option<String>)>,
    /// Jobs submitted here, newest last.
    pub jobs: Vec<jobs::JobRef>,
    /// The last output of recent commands, by command-line shape, for
    /// rerun diff.
    last_outputs: std::collections::VecDeque<(String, Vec<String>)>,
    /// Incremented whenever anything a tab's name is made from changes (the
    /// title, the working directory, a command starting or ending), so the
    /// app can cheaply notice.
    pub title_revision: u64,
    pub bell_revision: u64,

    /// One-shot flag telling [`Processor`] the VT machine returned to Ground.
    ground_signal: bool,

    /// Set whenever anything visible changed.
    dirty: bool,

    /// Shell integration (OSC 133): every command seen, by id.
    commands: FxHashMap<u32, CommandInfo>,
    command_events: Vec<CommandEvent>,
    /// The command whose prompt is showing or which is running.
    current_cmd: Option<u32>,
    next_cmd: u32,

    /// Output being parsed is a restored session's past, not happening now:
    /// commands in it get no start or finish times and raise no events.
    pub restoring: bool,

    /// Keep-awake: the command that keeps the Mac from sleeping while it
    /// runs, or the next one to start, if asked.
    awake_cmd: Option<u32>,
    awake_next: bool,

    /// Reused by `write_bulk_lines`.
    bulk_scratch: Vec<(usize, usize)>,

    /// The column where the command line starts on its row (OSC 133 B).
    input_col: usize,

    /// Inline images on either screen, oldest first.
    images: Vec<image::Placement>,
    /// Images kitty clients sent to show later, by their id.
    kitty_images: FxHashMap<u32, Arc<image::Image>>,
    /// A kitty image arriving in chunks: its first command, and the payload
    /// so far.
    kitty_partial: Option<(image::Kitty, Vec<u8>)>,
    /// An iTerm2 image arriving in parts (`MultipartFile`): its arguments
    /// and the payload so far.
    iterm_partial: Option<(Vec<u8>, Vec<u8>)>,
    /// A sixel image arriving (DCS q): its parameters and data so far.
    sixel: Option<(Vec<u16>, Vec<u8>)>,
    next_image: u64,
    /// A cell's size in device pixels: what images are sized by, and what
    /// `CSI 16 t` reports.
    cell_px: (u32, u32),
}

/// An image on screen, for the renderer.
#[derive(Clone, Debug)]
pub struct VisibleImage {
    pub image: Arc<image::Image>,
    /// The viewport row its top is on: negative when it starts above.
    pub row: i64,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
}

/// The most a pending image (chunks still coming) may grow to.
const MAX_IMAGE_PAYLOAD: usize = 64 << 20;
/// Image bytes kept per terminal, placed and waiting: the oldest go first.
const MAX_IMAGE_BYTES: usize = 320 << 20;

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
            title_cmd: None,
            cwd: None,
            cwd_host: None,
            ssh_session: None,
            jobs: Vec::new(),
            last_outputs: std::collections::VecDeque::new(),
            title_revision: 0,
            bell_revision: 0,
            ground_signal: false,
            dirty: true,
            commands: FxHashMap::default(),
            command_events: Vec::new(),
            current_cmd: None,
            next_cmd: 1,
            restoring: false,
            awake_cmd: None,
            awake_next: false,
            bulk_scratch: Vec::new(),
            input_col: 0,
            images: Vec::new(),
            kitty_images: FxHashMap::default(),
            kitty_partial: None,
            iterm_partial: None,
            sixel: None,
            next_image: 1,
            cell_px: (8, 16),
        }
    }

    // ---------------------------------------------------------------------
    // Shell integration
    // ---------------------------------------------------------------------

    pub fn command(&self, id: u32) -> Option<&CommandInfo> {
        self.commands.get(&id)
    }

    /// The command whose prompt is showing or which is running.
    pub fn current_command(&self) -> Option<u32> {
        self.current_cmd
    }

    pub fn take_command_events(&mut self) -> Vec<CommandEvent> {
        core::mem::take(&mut self.command_events)
    }

    /// The first output row of command `id`, searching back from `from`.
    fn output_start(&self, id: u32, from: usize) -> Option<usize> {
        let lo = from.saturating_sub(rerun::MAX_LINES * 4);
        (lo..=from.min(self.grid.total_lines().saturating_sub(1)))
            .rev()
            .find(|&l| {
                let m = self.grid.mark(l);
                m.cmd == id && m.has(RowMark::OUTPUT)
            })
    }

    /// Command `id` just finished: compare its output with the last run of
    /// the same command (numbers in the command line ignored), and mark the
    /// lines that differ.
    fn compare_run(&mut self, id: u32) {
        let Some(cmdline) = self.commands.get(&id).and_then(|c| c.cmdline.clone()) else {
            return;
        };
        let end = self.grid.history_len() + self.cursor.y;
        let Some(first) = self.output_start(id, end) else {
            return;
        };
        // Logical lines (wrapped rows joined) and the row each starts on.
        let mut starts = vec![first];
        for l in first..end {
            if !self.grid.line(l).wrapped && l + 1 < end {
                starts.push(l + 1);
            }
        }
        if starts.len() > rerun::MAX_LINES || end <= first {
            return;
        }
        let cols = self.cols();
        let text = self.text_range(first, 0, end - 1, cols.saturating_sub(1));
        let lines: Vec<String> = text.split('\n').map(|l| l.trim_end().to_string()).take(starts.len()).collect();
        let key = rerun::shape(cmdline.trim());
        if let Some(pos) = self.last_outputs.iter().position(|(k, _)| *k == key) {
            let (_, old) = self.last_outputs.remove(pos).expect("found above");
            let changes = rerun::diff(&old, &lines);
            for (i, change) in &changes {
                let Some(&abs) = starts.get(*i) else { continue };
                let mut m = self.grid.mark(abs);
                if m.cmd != id {
                    m = RowMark { cmd: id, kinds: 0 };
                }
                m.kinds |= if *change == rerun::Change::New { RowMark::NEW } else { RowMark::CHANGED };
                self.grid.set_mark(abs, m);
            }
            if let Some(c) = self.commands.get_mut(&id) {
                c.rerun = Some(changes);
            }
            self.dirty = true;
        }
        self.last_outputs.push_back((key, lines));
        if self.last_outputs.len() > 16 {
            self.last_outputs.pop_front();
        }
    }

    /// What a rerun-diff mark on line `abs` means: the line it replaced, or
    /// that it's new.
    pub fn rerun_note(&self, abs: usize) -> Option<String> {
        let mark = self.grid.mark(abs);
        if !mark.has(RowMark::CHANGED) && !mark.has(RowMark::NEW) {
            return None;
        }
        let changes = self.commands.get(&mark.cmd)?.rerun.as_ref()?;
        let first = self.output_start(mark.cmd, abs)?;
        // Which logical output line `abs` starts.
        let index = (first..abs).filter(|&l| !self.grid.line(l).wrapped).count();
        changes.iter().find(|(i, _)| *i == index).map(|(_, c)| match c {
            rerun::Change::Changed(old) => format!("Last run: {old}"),
            rerun::Change::New => "Not in the last run".to_string(),
        })
    }

    /// The screen state without scrollback or shell-integration bookkeeping,
    /// for Rewind to replay output onto.
    pub fn rewind_snapshot(&self) -> Term {
        Term {
            grid: self.grid.screen_copy(),
            inactive: self.inactive.screen_copy(),
            cursor: self.cursor,
            saved_cursor: self.saved_cursor,
            saved_cursor_alt: self.saved_cursor_alt,
            mode: self.mode,
            cursor_shape: self.cursor_shape,
            cursor_blinks: self.cursor_blinks,
            scroll_top: self.scroll_top,
            scroll_bot: self.scroll_bot,
            tabs: self.tabs.clone(),
            clusters: self.clusters.clone(),
            underline_colors: self.underline_colors.clone(),
            replies: Vec::new(),
            title: self.title.clone(),
            title_cmd: None,
            cwd: None,
            cwd_host: None,
            ssh_session: None,
            jobs: Vec::new(),
            last_outputs: std::collections::VecDeque::new(),
            title_revision: 0,
            bell_revision: 0,
            ground_signal: false,
            dirty: true,
            commands: FxHashMap::default(),
            command_events: Vec::new(),
            current_cmd: None,
            next_cmd: 1,
            restoring: false,
            awake_cmd: None,
            awake_next: false,
            bulk_scratch: Vec::new(),
            input_col: 0,
            images: Vec::new(),
            kitty_images: FxHashMap::default(),
            kitty_partial: None,
            iterm_partial: None,
            sixel: None,
            next_image: 1,
            cell_px: self.cell_px,
        }
    }

    /// A job id seen in the running command's output: remember it, and tie
    /// it to the command if it's the first.
    pub fn record_job(&mut self, kind: jobs::JobKind, id: String) {
        let via = self.ssh_session.as_ref().map(|(cmd, _)| cmd.clone());
        let job = jobs::JobRef { kind, id, via };
        if self.jobs.iter().any(|j| j.kind == job.kind && j.id == job.id) {
            return;
        }
        if let Some(c) = self.current_cmd.and_then(|id| self.commands.get_mut(&id)) {
            if c.job.is_none() {
                c.job = Some(job.clone());
            }
        }
        if self.jobs.len() == 16 {
            self.jobs.remove(0);
        }
        self.jobs.push(job);
        self.dirty = true;
    }

    /// The running command's line, for telling a submission from other output.
    /// The folder the shell last reported, when it's on this Mac: not while
    /// an ssh session runs here.
    pub fn local_cwd(&self) -> Option<&str> {
        if self.ssh_session.is_some() {
            return None;
        }
        self.cwd.as_deref()
    }

    pub fn running_cmdline(&self) -> Option<&str> {
        self.current_cmd
            .and_then(|id| self.commands.get(&id))
            .filter(|c| c.state == CommandState::Running)
            .and_then(|c| c.cmdline.as_deref())
    }

    /// The most recent command that has finished.
    pub fn last_finished(&self) -> Option<&CommandInfo> {
        self.commands
            .iter()
            .filter(|(_, c)| c.state == CommandState::Done)
            .max_by_key(|(id, _)| self.next_cmd.wrapping_sub(**id).wrapping_neg())
            .map(|(_, c)| c)
    }

    /// How long the running command has run, if one is.
    pub fn running_elapsed(&self) -> Option<Duration> {
        let id = self.current_cmd?;
        let c = self.commands.get(&id)?;
        (c.state == CommandState::Running).then(|| c.elapsed()).flatten()
    }

    /// Keep the Mac awake while the running command runs, or while the next
    /// one does when none is running; `false` takes that back.
    pub fn set_keep_awake(&mut self, on: bool) {
        let running = self.current_cmd.filter(|id| self.commands.get(id).is_some_and(|c| c.state == CommandState::Running));
        (self.awake_cmd, self.awake_next) = match (on, running) {
            (false, _) => (None, false),
            (true, Some(id)) => (Some(id), false),
            (true, None) => (None, true),
        };
        self.dirty = true;
        self.title_revision = self.title_revision.wrapping_add(1);
    }

    /// A command running now keeps the Mac awake.
    pub fn keeps_awake(&self) -> bool {
        self.awake_cmd.is_some()
    }

    /// Keep-awake is on: for the running command, or the next one.
    pub fn awake_armed(&self) -> bool {
        self.awake_cmd.is_some() || self.awake_next
    }

    /// The command keeping the Mac awake, if one is.
    pub fn awake_command(&self) -> Option<u32> {
        self.awake_cmd
    }

    /// What's typed at the prompt, when the cursor is at its end: the text
    /// from where the command line starts (OSC 133 B) to the cursor, and how
    /// many blank cells follow the cursor before anything else on its row (a
    /// right prompt, say). `None` while a command runs, on the alternate
    /// screen, scrolled back, with the cursor inside the text, or without
    /// shell integration.
    pub fn prompt_input(&self) -> Option<(String, usize)> {
        if self.mode.contains(TermMode::ALT_SCREEN) || self.grid.display_offset() != 0 {
            return None;
        }
        let id = self.current_cmd?;
        if self.commands.get(&id)?.state != CommandState::Prompt {
            return None;
        }
        let cy = self.cursor.y.min(self.rows() - 1);
        let cx = if self.cursor.wrap_pending { self.cols() } else { self.cursor.x };
        // The row the command line starts on: at or above the cursor's.
        let start = (cy.saturating_sub(8)..=cy)
            .rev()
            .find(|&y| self.grid.row(y).mark.cmd == id && self.grid.row(y).mark.has(RowMark::INPUT))?;
        let mut text = String::new();
        for y in start..=cy {
            let row = self.grid.row(y);
            let from = if y == start { self.input_col } else { 0 };
            let to = if y == cy { cx } else { self.cols() };
            for c in row.cells().get(from..to.max(from))? {
                if c.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                if c.ch & CLUSTER_TAG != 0 {
                    text.extend(self.cluster(c.ch).unwrap_or(&[]));
                } else {
                    text.push(char::from_u32(c.ch).filter(|&ch| ch != '\0').unwrap_or(' '));
                }
            }
            if y < cy && !row.wrapped {
                return None; // a second line of input: not a simple command line
            }
        }
        let after = self.grid.row(cy).cells().get(cx..).unwrap_or(&[]);
        let blank = after.iter().take_while(|c| c.ch == 0 || c.ch == ' ' as u32).count();
        Some((text, blank))
    }

    /// Whether a command is running, so its output is worth reading for
    /// metrics.
    pub fn command_running(&self) -> bool {
        self.current_cmd
            .and_then(|id| self.commands.get(&id))
            .is_some_and(|c| c.state == CommandState::Running)
    }

    /// Add what a line of output said to the running command's metrics.
    /// Returns the command's progress percentage, if it has one.
    pub fn record_reading(&mut self, r: &lens::Reading) -> Option<f32> {
        let id = self.current_cmd?;
        let c = self.commands.get_mut(&id).filter(|c| c.state == CommandState::Running)?;
        c.lens.record(r);
        self.dirty = true;
        c.lens.percent
    }

    pub fn set_receipt(&mut self, id: u32, receipt: Receipt) {
        if let Some(c) = self.commands.get_mut(&id) {
            c.receipt = Some(receipt);
            self.dirty = true;
        }
    }

    /// OSC 133: A prompt starts, B input starts, C output starts (the command
    /// runs), D;exit the command finished.
    fn semantic_mark(&mut self, params: &[&[u8]]) {
        // Marks belong to the shell's own screen, not to full-screen apps.
        if self.mode.contains(TermMode::ALT_SCREEN) {
            return;
        }
        let Some(&kind) = params.get(1).and_then(|p| p.first()) else {
            return;
        };
        match kind {
            b'A' => {
                // A prompt redrawn, or marked twice (fish 4 marks its own
                // prompts as well as ours): still the same command.
                if let Some(id) = self.current_cmd.filter(|id| {
                    self.commands.get(id).is_some_and(|c| c.state == CommandState::Prompt)
                }) {
                    self.mark_row(id, RowMark::PROMPT);
                    self.dirty = true;
                    return;
                }
                let id = self.next_cmd;
                self.next_cmd = self.next_cmd.wrapping_add(1).max(1);
                self.current_cmd = Some(id);
                self.commands.insert(
                    id,
                    CommandInfo {
                        state: CommandState::Prompt,
                        started: None,
                        finished: None,
                        exit: None,
                        receipt: None,
                        cmdline: None,
                        lens: lens::Lens::default(),
                        job: None,
                        rerun: None,
                    },
                );
                self.mark_row(id, RowMark::PROMPT);
                if self.commands.len() > MAX_COMMANDS as usize {
                    let floor = self.next_cmd.wrapping_sub(MAX_COMMANDS);
                    self.commands.retain(|&k, _| k.wrapping_sub(floor) < MAX_COMMANDS);
                }
            }
            b'B' => {
                if let Some(id) = self.current_cmd {
                    self.mark_row(id, RowMark::INPUT);
                    self.input_col = self.cursor.x;
                }
            }
            b'C' => {
                if let Some(id) = self.current_cmd {
                    // Everything after `C;` is options; a `;` inside the
                    // command line split it into more parameters.
                    let rest = params.get(2..).unwrap_or(&[]);
                    let rest = rest.iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join(";");
                    let cmdline = rest.strip_prefix("cmdline=").map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
                    let Some(c) = self.commands.get_mut(&id) else {
                        return;
                    };
                    if c.state != CommandState::Prompt {
                        // Marked twice: keep the first start, take the command
                        // line if only the second has one.
                        if c.cmdline.is_none() && cmdline.is_some() {
                            c.cmdline = cmdline;
                            self.title_revision = self.title_revision.wrapping_add(1);
                        }
                        return;
                    }
                    c.state = CommandState::Running;
                    c.started = (!self.restoring).then(Instant::now);
                    if cmdline.as_deref().is_some_and(is_ssh) && self.ssh_session.is_none() {
                        self.ssh_session = cmdline.clone().map(|c| (c, self.cwd_host.clone()));
                    }
                    let c = self.commands.get_mut(&id).expect("looked up above");
                    c.cmdline = cmdline;
                    self.title_revision = self.title_revision.wrapping_add(1);
                    self.mark_row(id, RowMark::OUTPUT);
                    if std::mem::take(&mut self.awake_next) {
                        self.awake_cmd = Some(id);
                    }
                    if !self.restoring {
                        self.command_events.push(CommandEvent::Started(id));
                    }
                }
            }
            b'D' => {
                if let Some(id) = self.current_cmd {
                    let exit = params
                        .get(2)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|s| s.parse().ok());
                    if let Some(c) = self.commands.get_mut(&id) {
                        if c.state == CommandState::Running {
                            c.state = CommandState::Done;
                            c.finished = (!self.restoring).then(Instant::now);
                            c.exit = exit;
                            if self.awake_cmd == Some(id) {
                                self.awake_cmd = None;
                            }
                            if !self.restoring {
                                self.command_events.push(CommandEvent::Finished(id, exit));
                            }
                            self.compare_run(id);
                            self.title_revision = self.title_revision.wrapping_add(1);
                        }
                    }
                }
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn mark_row(&mut self, id: u32, kind: u8) {
        let y = self.cursor.y.min(self.rows() - 1);
        let row = self.grid.row_mut(y);
        if row.mark.cmd != id {
            row.mark = RowMark { cmd: id, kinds: 0 };
        }
        row.mark.kinds |= kind;
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
        let line_before = self.grid.scrolled() as i64 + self.cursor.y as i64;
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
            // Reflowing moves lines: images keep their place relative to the
            // cursor's line, where the newest ones are.
            let shift = self.grid.scrolled() as i64 + cy as i64 - line_before;
            if shift != 0 {
                for p in self.images.iter_mut().filter(|p| !p.alt) {
                    p.line += shift;
                }
            }
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
            if !self.images.is_empty() {
                self.punch_images(y, x, x + take, false);
            }
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

    /// A flood of plain lines, written in bulk. When the cursor is at the
    /// start of a blank bottom line with the whole screen scrolling, and
    /// `bytes` starts with at least a screenful of lines of printable ASCII
    /// that each fit and end in CR LF, the lines that would scroll straight
    /// through go to the scrollback packed from their bytes, and only the
    /// last screenful is written as cells; fewer lines are written as usual.
    /// The result is exactly what writing them one by one gives. Returns the
    /// bytes consumed, or `Err` when nothing could be taken.
    #[inline(never)]
    pub fn write_bulk_lines(&mut self, bytes: &[u8]) -> Result<usize, usize> {
        let (rows, cols) = (self.rows(), self.cols());
        let pen = self.cursor.pen;
        if rows < 2
            || self.cursor.x != 0
            || self.cursor.wrap_pending
            || self.cursor.y != rows - 1
            || self.scroll_top != 0
            || self.scroll_bot != rows - 1
            || !self.mode.contains(TermMode::AUTOWRAP)
            || self.mode.contains(TermMode::INSERT)
            || !self.charset_is_ascii()
            || Cell::blank(&pen) != Cell::EMPTY
        {
            return Err(0);
        }
        // The lines, as (start, end) within `bytes`, in a buffer kept between
        // calls so looking costs no allocation.
        let mut lines = std::mem::take(&mut self.bulk_scratch);
        lines.clear();
        let result = self.bulk_lines_in(bytes, &mut lines, cols, rows, pen);
        self.bulk_scratch = lines;
        result
    }

    fn bulk_lines_in(&mut self, bytes: &[u8], lines: &mut Vec<(usize, usize)>, cols: usize, rows: usize, pen: Pen) -> Result<usize, usize> {
        let mut pos = 0;
        loop {
            let n = parser::scan_printable_ascii(&bytes[pos..]);
            if n > cols || bytes.get(pos + n..pos + n + 2) != Some(b"\r\n") {
                break;
            }
            lines.push((pos, pos + n));
            pos += n + 2;
        }
        if lines.len() < rows || !self.grid.row_is_blank(rows - 1) {
            if lines.is_empty() {
                return Err(pos);
            }
            // Fewer than a screenful: write them as usual, without scanning
            // them a second time.
            for &(a, b) in lines.iter() {
                self.write_ascii_run(&bytes[a..b]);
                self.execute(b'\r');
                self.execute(b'\n');
            }
            return Ok(pos);
        }
        self.dirty = true;
        self.grid.scroll_to_bottom();
        let style = Cell { ch: 0, fg: pen.fg, bg: pen.bg, flags: pen.flags, underline: pen.underline };
        // The screen's lines above the cursor scroll off as they would; the
        // blank bottom line moves to the top, keeping its mark for the first
        // new line, which is written on it.
        let mark = self.grid.row(rows - 1).mark;
        self.grid.scroll_up(0, rows - 1, rows - 1, &pen);
        // The new lines that would scroll off too, straight to history.
        let shown = lines.len() - (rows - 1);
        for (k, &(a, b)) in lines[..shown].iter().enumerate() {
            let m = if k == 0 { mark } else { RowMark::default() };
            self.grid.push_history_ascii(&bytes[a..b], &style, m);
        }
        // The last screenful, as cells, with the cursor below it.
        for (y, &(a, b)) in lines[shown..].iter().enumerate() {
            let row = self.grid.row_mut(y);
            row.mark = RowMark::default();
            row.wrapped = false;
            row.dirty = true;
            let cells = row.cells_mut();
            for (i, &ch) in bytes[a..b].iter().enumerate() {
                cells[i] = Cell { ch: ch as u32, ..style };
            }
        }
        self.cursor.x = 0;
        self.cursor.y = rows - 1;
        Ok(pos)
    }

    /// The cursor is where a flood of lines would start: the beginning of
    /// the bottom line. Cheap enough to ask after every escape sequence.
    #[inline]
    pub fn at_line_start_at_bottom(&self) -> bool {
        self.cursor.x == 0 && !self.cursor.wrap_pending && self.cursor.y + 1 == self.rows()
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
        if !self.images.is_empty() {
            self.punch_images(y, x, x + width, false);
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
            self.scroll_lines_up(t, b, 1, &pen);
        } else if self.cursor.y + 1 < self.rows() {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.y == self.scroll_top {
            let pen = self.cursor.pen;
            let (t, b) = (self.scroll_top, self.scroll_bot);
            self.scroll_lines_down(t, b, 1, &pen);
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
            self.images.retain(|p| !p.alt);
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
            // A full-screen program's images go with its screen.
            self.images.retain(|p| !p.alt);
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
        self.images.clear();
        self.kitty_images.clear();
        self.kitty_partial = None;
        self.iterm_partial = None;
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
        if !self.images.is_empty() {
            self.punch_images(y, range.start, range.end, true);
        }
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
                    if !self.images.is_empty() {
                        self.punch_images(row, 0, usize::MAX, true);
                    }
                    self.grid.row_mut(row).cells_mut().fill(blank);
                }
            }
            // Beginning of screen to cursor.
            1 => {
                for row in 0..y {
                    if !self.images.is_empty() {
                        self.punch_images(row, 0, usize::MAX, true);
                    }
                    self.grid.row_mut(row).cells_mut().fill(blank);
                }
                self.erase_in_line(1);
            }
            // Whole screen.
            2 => {
                let pen = self.cursor.pen;
                self.grid.clear_all(&pen);
                self.drop_images(true, false);
            }
            // Whole screen and scrollback.
            3 => {
                let pen = self.cursor.pen;
                self.grid.clear_all(&pen);
                self.grid.clear_history();
                self.drop_images(true, true);
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
        if !self.images.is_empty() {
            self.punch_images(y, x, end, true);
        }
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
        self.scroll_lines_down(top, bot, n.max(1), &pen);
        self.cursor.x = 0;
        self.dirty = true;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bot {
            return;
        }
        let pen = self.cursor.pen;
        let (top, bot) = (self.cursor.y, self.scroll_bot);
        self.scroll_lines_up(top, bot, n.max(1), &pen);
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
            ('S', None) => {
                let pen = self.cursor.pen;
                let (t, b) = (self.scroll_top, self.scroll_bot);
                self.scroll_lines_up(t, b, n(0), &pen);
            }
            ('T', None) => {
                let pen = self.cursor.pen;
                let (t, b) = (self.scroll_top, self.scroll_bot);
                self.scroll_lines_down(t, b, n(0), &pen);
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
            // `CSI > 4;1 m` and the like (xterm's key modifiers, which tmux
            // sends) aren't SGR.
            ('m', None) => self.apply_sgr(params),
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
            ('s', None) => self.saved_cursor = self.cursor,
            ('u', None) => self.cursor = self.saved_cursor,
            ('c', None) => {
                // Primary device attributes: a VT220 with colour (22) and
                // sixel graphics (4), which tmux looks for before passing
                // images on.
                if arg(params, 0, 0) == 0 {
                    self.replies.extend_from_slice(b"\x1b[?62;4;22c");
                }
            }
            ('c', Some(b'>')) => {
                // Secondary device attributes: a VT100-class terminal.
                if arg(params, 0, 0) == 0 {
                    self.replies.extend_from_slice(b"\x1b[>0;10;1c");
                }
            }
            ('S', Some(b'?')) => {
                // XTSMGRAPHICS: sixel colour registers (1) and the largest
                // image (2), asked by sixel tools before drawing.
                let (cw, ch) = self.cell_px;
                let s = match (arg(params, 0, 0), arg(params, 1, 0)) {
                    (1, 1 | 4) => "\x1b[?1;0;1024S".to_string(),
                    (2, 1 | 4) => format!("\x1b[?2;0;{};{}S", self.cols() as u32 * cw, self.rows() as u32 * ch),
                    (item, _) => format!("\x1b[?{item};1;0S"),
                };
                self.replies.extend_from_slice(s.as_bytes());
            }
            ('t', _) => {
                // Window reports: the text area in cells (18), in pixels
                // (14), and a cell's size in pixels (16), which is how image
                // tools size what they send.
                let (cw, ch) = self.cell_px;
                let s = match arg(params, 0, 0) {
                    18 => format!("\x1b[8;{};{}t", self.rows(), self.cols()),
                    14 => format!("\x1b[4;{};{}t", self.rows() as u32 * ch, self.cols() as u32 * cw),
                    16 => format!("\x1b[6;{ch};{cw}t"),
                    _ => String::new(),
                };
                self.replies.extend_from_slice(s.as_bytes());
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

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        // Ended by BEL, the parser is back in Ground. Ended by ESC (the start
        // of ST, `ESC \`), it is in Escape until the `\`, whose esc_dispatch
        // signals Ground; signalling here would print that `\` as text.
        self.ground_signal = bell_terminated;
        let Some(cmd) = params.first() else { return };

        match *cmd {
            // Window / icon title.
            b"0" | b"2" => {
                if params.len() > 1 {
                    // A `;` in the title split it into more parameters.
                    let title = params[1..].iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join(";");
                    let running = self
                        .current_cmd
                        .filter(|id| self.commands.get(id).is_some_and(|c| c.state == CommandState::Running));
                    self.title = Some(title);
                    self.title_cmd = running;
                    self.title_revision = self.title_revision.wrapping_add(1);
                }
            }
            // OSC 7: the working directory, as file://host/path.
            b"7" => {
                let host = params.get(1).and_then(|p| file_url_host(p));
                // Back on the machine the ssh started from: the session ended.
                if self.ssh_session.as_ref().is_some_and(|(_, from)| from.is_some() && *from == host) {
                    self.ssh_session = None;
                }
                self.cwd_host = host;
                if let Some(path) = params.get(1).and_then(|p| parse_file_url(p)) {
                    if self.cwd.as_deref() != Some(path.as_str()) {
                        self.cwd = Some(path);
                        self.title_revision = self.title_revision.wrapping_add(1);
                    }
                }
            }
            // Shell integration: prompt, input, output and exit marks.
            b"133" => self.semantic_mark(params),
            // iTerm2's: inline images, among other things.
            b"1337" => self.iterm_osc(&params[1..].join(&b';')),
            _ => {}
        }
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        // Entering DCS -- explicitly *not* a return to Ground.
        if action == 'q' && intermediates.is_empty() && !ignore {
            let p: Vec<u16> = params.iter().map(|p| p.first().copied().unwrap_or(0)).collect();
            self.sixel = Some((p, Vec::new()));
        }
    }

    fn put(&mut self, byte: u8) {
        if let Some((_, data)) = &mut self.sixel {
            if data.len() < MAX_IMAGE_PAYLOAD {
                data.push(byte);
            }
        }
    }

    fn unhook(&mut self) {
        if let Some((params, data)) = self.sixel.take() {
            self.sixel_image(&params, &data);
        }
        // A DCS ends on the ESC of ST too; see osc_dispatch. (CAN and SUB end
        // it in Ground without a signal, which only means the next bytes take
        // the slow path until something does signal.)
    }
}

// ---------------------------------------------------------------------------
// Inline images
// ---------------------------------------------------------------------------

impl Term {
    /// A cell's size in device pixels, which images are sized by.
    pub fn set_cell_pixels(&mut self, width: u32, height: u32) {
        self.cell_px = (width.max(1), height.max(1));
    }

    pub fn cell_pixels(&self) -> (u32, u32) {
        self.cell_px
    }

    /// The images in the viewport now, oldest first (newer ones draw on top).
    pub fn visible_images(&self) -> Vec<VisibleImage> {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        let first = self.grid.scrolled() as i64 - self.grid.display_offset() as i64;
        let rows = self.rows() as i64;
        self.images
            .iter()
            .filter(|p| p.alt == alt)
            .filter_map(|p| {
                let row = p.line - first;
                (row + p.rows as i64 > 0 && row < rows).then(|| VisibleImage { image: p.image.clone(), row, col: p.col, cols: p.cols, rows: p.rows })
            })
            .collect()
    }

    /// Scroll lines `top..=bottom` up by `n`, images on them too. The whole
    /// screen scrolling takes its images along by itself (they're anchored to
    /// lines); a region (tmux keeps its status line out of one) needs this.
    fn scroll_lines_up(&mut self, top: usize, bottom: usize, n: usize, pen: &Pen) {
        self.grid.scroll_up(top, bottom, n, pen);
        if !self.images.is_empty() && !(top == 0 && bottom + 1 == self.rows()) {
            self.shift_images(top, bottom, -(n.min(bottom + 1 - top) as i64));
        }
    }

    fn scroll_lines_down(&mut self, top: usize, bottom: usize, n: usize, pen: &Pen) {
        self.grid.scroll_down(top, bottom, n, pen);
        if !self.images.is_empty() && top <= bottom {
            self.shift_images(top, bottom, n.min(bottom + 1 - top) as i64);
        }
    }

    /// Move the images whose top is in rows `top..=bottom` by `delta` rows.
    /// One pushed out of the region goes; at the top of the screen it stays
    /// while any of it shows (the screen's edge clips it).
    fn shift_images(&mut self, top: usize, bottom: usize, delta: i64) {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        let base = self.grid.scrolled() as i64;
        let (top, bottom) = (top as i64, bottom as i64);
        self.images.retain_mut(|p| {
            let row = p.line - base;
            // One hanging off the top of the screen moves with what shows of it.
            let hangs = top == 0 && row < 0 && row + p.rows as i64 > 0;
            if p.alt != alt || (row < top && !hangs) || row > bottom {
                return true;
            }
            let new = row + delta;
            if new > bottom || (new < top && (top > 0 || new + p.rows as i64 <= 0)) {
                return false;
            }
            p.line = base + new;
            true
        });
    }

    /// A sixel image (DCS q) at the cursor, which then moves to the line
    /// below it.
    fn sixel_image(&mut self, params: &[u16], data: &[u8]) {
        let Some((w, h, rgba)) = image::decode_sixel(params, data) else { return };
        let (cols, rows) = self.image_cells(w, h, (None, None));
        let img = self.new_image(image::Format::Rgba, w, h, rgba);
        let col = self.cursor.x;
        self.place_image(img, cols, rows, true, None, true);
        self.linefeed_raw();
        self.cursor.x = col;
    }

    /// Cells `x0..x1` of screen row `y` were written, or with `erase`
    /// erased: the images that took their place go. So do kitty images (a
    /// layer of their own) on a row erased whole, which is how tmux
    /// repaints: otherwise one would hang over the next window shown.
    fn punch_images(&mut self, y: usize, x0: usize, x1: usize, erase: bool) {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        let line = self.grid.scrolled() as i64 + y as i64;
        let whole_row = erase && x0 == 0 && x1 >= self.cols();
        let before = self.images.len();
        self.images.retain(|p| {
            let hit = p.alt == alt && line >= p.line && line < p.line + p.rows as i64 && x0 < p.col + p.cols && x1 > p.col;
            !(hit && (p.in_cells || whole_row))
        });
        if self.images.len() != before {
            self.grid.damage_all();
        }
    }

    /// Whether there are any images to draw on this screen.
    pub fn has_images(&self) -> bool {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        self.images.iter().any(|p| p.alt == alt)
    }

    fn new_image(&mut self, format: image::Format, width: u32, height: u32, data: Vec<u8>) -> Arc<image::Image> {
        let id = self.next_image;
        self.next_image += 1;
        Arc::new(image::Image { id, format, width, height, data })
    }

    /// Put an image at the cursor, `cols` by `rows` cells, and (unless
    /// asked not to) move the cursor to just after its bottom-right corner,
    /// scrolling as text would.
    fn place_image(&mut self, image: Arc<image::Image>, cols: usize, rows: usize, move_cursor: bool, kitty_id: Option<u32>, in_cells: bool) {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        let (col, line) = (self.cursor.x, self.grid.scrolled() as i64 + self.cursor.y as i64);
        // A new image over older ones hides them: they go, as their pixels
        // would in a terminal that draws images into its cells.
        self.images.retain(|p| {
            p.alt != alt || !(p.line >= line && p.line + p.rows as i64 <= line + rows as i64 && p.col >= col && p.col + p.cols <= col + cols)
        });
        self.images.push(image::Placement { image, line, col, cols, rows, alt, kitty_id, in_cells });
        if move_cursor {
            for _ in 1..rows {
                self.linefeed_raw();
            }
            // Just after it; at the right margin, the next character wraps
            // rather than landing on it.
            let end = col + cols;
            self.cursor.x = end.min(self.cols() - 1);
            self.cursor.wrap_pending = end >= self.cols() && self.mode.contains(TermMode::AUTOWRAP);
        }
        self.prune_images();
        self.grid.damage_all();
        self.dirty = true;
    }

    /// Forget images whose lines have left the scrollback, then the oldest
    /// while there are too many bytes of them.
    fn prune_images(&mut self) {
        let alt_now = self.mode.contains(TermMode::ALT_SCREEN);
        let gone_before = |g: &Grid| g.scrolled() as i64 - g.history_len() as i64;
        let (here, other) = (gone_before(&self.grid), gone_before(&self.inactive));
        self.images.retain(|p| p.line + p.rows as i64 > if p.alt == alt_now { here } else { other });
        let mut bytes: usize = self.kitty_images.values().map(|i| i.data.len()).sum();
        bytes += self.images.iter().filter(|p| p.kitty_id.is_none()).map(|p| p.image.data.len()).sum::<usize>();
        while bytes > MAX_IMAGE_BYTES {
            if let Some(oldest) = self.kitty_images.keys().min().copied().filter(|_| self.images.is_empty() || self.kitty_images.len() > 1) {
                bytes -= self.kitty_images.remove(&oldest).map_or(0, |i| i.data.len());
            } else if !self.images.is_empty() {
                let p = self.images.remove(0);
                bytes = bytes.saturating_sub(if p.kitty_id.is_none() { p.image.data.len() } else { 0 });
            } else {
                break;
            }
        }
    }

    /// `ED 2` and `ED 3`: drop the images on the screen being erased, and
    /// with the scrollback, those above it too.
    fn drop_images(&mut self, screen: bool, history: bool) {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        let top = self.grid.scrolled() as i64;
        let bottom = top + self.rows() as i64;
        self.images.retain(|p| {
            if p.alt != alt {
                return true;
            }
            let on_screen = p.line + (p.rows as i64) > top && p.line < bottom;
            let above = p.line + (p.rows as i64) <= top;
            !((screen && on_screen) || (history && above))
        });
    }

    /// The cells an image of this many pixels takes at the cursor, as asked
    /// for, else its own size, scaled down to fit what's left of the line.
    fn image_cells(&self, width: u32, height: u32, want: (Option<usize>, Option<usize>)) -> (usize, usize) {
        image::fit(width, height, self.cell_px, want, (self.cols() - self.cursor.x).max(1))
    }

    /// An APC string: a kitty graphics command, or something to ignore.
    pub(crate) fn apc(&mut self, data: &[u8]) {
        let Some((k, payload)) = image::parse_kitty(data) else { return };
        // A chunked transfer: the first chunk's keys apply to the whole.
        let (k, payload) = match self.kitty_partial.take() {
            Some((first, mut so_far)) => {
                if so_far.len() + payload.len() > MAX_IMAGE_PAYLOAD {
                    return;
                }
                so_far.extend_from_slice(payload);
                if k.more {
                    self.kitty_partial = Some((first, so_far));
                    return;
                }
                (first, so_far)
            }
            None if k.more => {
                self.kitty_partial = Some((k, payload.to_vec()));
                return;
            }
            None => (k, payload.to_vec()),
        };
        match k.action {
            b'd' => self.kitty_delete(&k),
            b'p' => match k.id.and_then(|id| self.kitty_images.get(&id).cloned()) {
                Some(img) => {
                    let (cols, rows) = self.image_cells(img.width, img.height, (k.cols, k.rows));
                    self.place_image(img, cols, rows, !k.stay, k.id, false);
                    self.kitty_reply(&k, Ok(()));
                }
                None => self.kitty_reply(&k, Err("ENOENT:no such image")),
            },
            b't' | b'T' | b'q' => match image::kitty_image(&k, &payload) {
                Ok((format, w, h, data)) => {
                    if k.action != b'q' {
                        let img = self.new_image(format, w, h, data);
                        if let Some(id) = k.id {
                            self.kitty_images.insert(id, img.clone());
                        }
                        if k.action == b'T' {
                            let (cols, rows) = self.image_cells(w, h, (k.cols, k.rows));
                            self.place_image(img, cols, rows, !k.stay, k.id, false);
                        } else {
                            self.prune_images();
                        }
                    }
                    self.kitty_reply(&k, Ok(()));
                }
                Err(e) => self.kitty_reply(&k, Err(e)),
            },
            _ => {}
        }
    }

    /// kitty's `a=d`: all placements (`d=a`), or one image's (`d=i`); in
    /// capitals, the image data goes too.
    fn kitty_delete(&mut self, k: &image::Kitty) {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        match k.delete {
            b'a' | b'A' => self.images.retain(|p| p.alt != alt),
            b'i' | b'I' => self.images.retain(|p| p.kitty_id.is_none() || p.kitty_id != k.id),
            _ => return,
        }
        if k.delete.is_ascii_uppercase() {
            let placed: Vec<u32> = self.images.iter().filter_map(|p| p.kitty_id).collect();
            match (k.delete, k.id) {
                (b'I', Some(id)) => {
                    self.kitty_images.remove(&id);
                }
                _ => self.kitty_images.retain(|id, _| placed.contains(id)),
            }
        }
        self.grid.damage_all();
        self.dirty = true;
    }

    /// kitty's acknowledgement, for commands that gave an id and didn't ask
    /// for quiet.
    fn kitty_reply(&mut self, k: &image::Kitty, result: Result<(), &str>) {
        let Some(id) = k.id else { return };
        let msg = match result {
            Ok(()) if k.quiet == 0 => "OK",
            Err(e) if k.quiet < 2 => e,
            _ => return,
        };
        self.replies.extend_from_slice(format!("\x1b_Gi={id};{msg}\x1b\\").as_bytes());
    }

    /// OSC 1337, given everything after `1337;`: an inline image, whole
    /// (`File=`) or in parts (`MultipartFile=`, `FilePart=`, `FileEnd`).
    fn iterm_osc(&mut self, rest: &[u8]) {
        if let Some(args) = rest.strip_prefix(b"MultipartFile=") {
            self.iterm_partial = Some((args.to_vec(), Vec::new()));
            return;
        }
        if let Some(part) = rest.strip_prefix(b"FilePart=") {
            if let Some((_, payload)) = &mut self.iterm_partial {
                if payload.len() + part.len() <= MAX_IMAGE_PAYLOAD {
                    payload.extend_from_slice(part);
                }
            }
            return;
        }
        let whole;
        let rest = if rest == b"FileEnd" {
            let Some((args, payload)) = self.iterm_partial.take() else { return };
            whole = [b"File=".as_slice(), &args, b":", &payload].concat();
            &whole[..]
        } else {
            rest
        };
        let Some(i) = image::parse_iterm(rest, self.cell_px, (self.cols(), self.rows())) else { return };
        let (cols, rows) = self.image_cells(i.width, i.height, i.want);
        let img = self.new_image(image::Format::Encoded, i.width, i.height, i.data);
        self.place_image(img, cols, rows, true, None, true);
    }
}

/// Whether a command line starts an ssh session.
fn is_ssh(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .find(|w| !w.contains('='))
        .is_some_and(|w| w.rsplit('/').next() == Some("ssh"))
}

/// The host of an OSC 7 `file://host/path` URL.
fn file_url_host(url: &[u8]) -> Option<String> {
    let rest = url.strip_prefix(b"file://")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let host = std::str::from_utf8(&rest[..slash]).ok()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// The path of an OSC 7 `file://host/path` URL, percent-decoded.
fn parse_file_url(url: &[u8]) -> Option<String> {
    let rest = url.strip_prefix(b"file://")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let path = &rest[slash..];
    let mut out = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (path[i], path.get(i + 1).and_then(|&b| hex(b)), path.get(i + 2).and_then(|&b| hex(b))) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod input_tests {
    use super::*;

    fn feed(t: &mut Term, s: &str) {
        Processor::new().advance(t, s.as_bytes());
    }

    #[test]
    fn what_is_typed_at_the_prompt() {
        let mut t = Term::new(40, 6, 100);
        assert_eq!(t.prompt_input(), None, "no shell integration");
        feed(&mut t, "\x1b]133;A\x07~/proj \x1b]133;B\x07");
        assert_eq!(t.prompt_input(), Some((String::new(), 33)));
        feed(&mut t, "cargo t");
        assert_eq!(t.prompt_input(), Some(("cargo t".into(), 26)));
        // A right prompt further along: the blank cells before it.
        feed(&mut t, "\x1b[34G(base)\x1b[15G");
        assert_eq!(t.prompt_input(), Some(("cargo t".into(), 19)));
        // Cursor moved back into the text: nothing blank after it.
        feed(&mut t, "\x1b[12G");
        assert_eq!(t.prompt_input(), Some(("carg".into(), 0)));
        feed(&mut t, "\x1b[15G\r\n\x1b]133;C;cmdline=cargo t\x07");
        assert_eq!(t.prompt_input(), None, "running");
    }

    #[test]
    fn a_long_command_line_wraps() {
        let mut t = Term::new(10, 6, 100);
        feed(&mut t, "\x1b]133;A\x07$ \x1b]133;B\x07echo abcdefghij");
        assert_eq!(t.prompt_input().map(|p| p.0), Some("echo abcdefghij".into()));
    }
}

#[cfg(test)]
mod bulk_tests {
    use super::*;

    /// Everything a terminal holds that output can change.
    fn state(t: &Term) -> (Vec<(Vec<Cell>, bool, RowMark)>, usize, usize, usize) {
        let g = t.grid();
        let lines = (0..g.total_lines()).map(|l| (g.line(l).cells().to_vec(), g.line(l).wrapped, g.line(l).mark)).collect();
        (lines, g.history_len(), t.cursor.x, t.cursor.y)
    }

    /// Fed in big chunks (the bulk path) and in 16-byte ones (never), the
    /// same stream must leave the same terminal.
    fn same_both_ways(cols: usize, rows: usize, scrollback: usize, data: &[u8]) {
        let mut bulk = Term::new(cols, rows, scrollback);
        let mut p = Processor::new();
        for c in data.chunks(64 * 1024) {
            p.advance(&mut bulk, c);
        }
        let mut each = Term::new(cols, rows, scrollback);
        let mut p = Processor::new();
        for c in data.chunks(16) {
            p.advance(&mut each, c);
        }
        assert!(state(&bulk) == state(&each), "bulk and one-by-one differ ({cols}x{rows}, {scrollback} lines kept)");
    }

    fn lines(n: usize, width: impl Fn(usize) -> usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..n {
            out.extend((0..width(i)).map(|k| b'!' + ((i * 7 + k) % 90) as u8));
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    #[test]
    fn a_flood_of_lines_lands_the_same() {
        same_both_ways(20, 10, 100, &lines(500, |i| i % 21));
        // Scrollback full: the oldest lines go, in both.
        same_both_ways(20, 10, 30, &lines(500, |i| (i * 3) % 21));
        // Exactly a screenful, and one short of it.
        same_both_ways(20, 10, 100, &lines(10, |_| 20));
        same_both_ways(20, 10, 100, &lines(9, |_| 5));
    }

    #[test]
    fn what_came_before_and_between_is_kept() {
        let mut data = b"\x1b[1;31mred prompt\x1b[0m $ \r\n\x1b]133;C\x07".to_vec();
        data.extend(lines(200, |i| i % 25));
        data.extend_from_slice(b"\ttab\x1b[42mgreen\r\n");
        data.extend(lines(200, |i| 30 - i % 7));
        data.extend_from_slice(b"\x1b[0mwide \xe6\x97\xa5\r\nno newline at the end");
        same_both_ways(25, 12, 1000, &data);
        // A coloured background stays on the careful path.
        let mut bg = b"\x1b[44m".to_vec();
        bg.extend(lines(100, |_| 8));
        same_both_ways(20, 10, 100, &bg);
    }
}

#[cfg(test)]
mod awake_tests {
    use super::*;

    fn feed(t: &mut Term, s: &str) {
        Processor::new().advance(t, s.as_bytes());
    }

    #[test]
    fn keep_awake_lasts_one_command() {
        let mut t = Term::new(40, 10, 100);
        feed(&mut t, "\x1b]133;A\x07$ \x1b]133;B\x07");
        // At a prompt: the next command.
        t.set_keep_awake(true);
        assert!(t.awake_armed() && !t.keeps_awake());
        feed(&mut t, "train\r\n\x1b]133;C;cmdline=train\x07");
        assert!(t.keeps_awake());
        feed(&mut t, "\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07");
        assert!(!t.awake_armed(), "only that one");
        // While one runs: that one; and it can be taken back.
        t.set_keep_awake(true);
        assert!(t.keeps_awake());
        t.set_keep_awake(false);
        assert!(!t.awake_armed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut Term, p: &mut Processor, s: &str) {
        p.advance(t, s.as_bytes());
    }

    /// What names a tab: the folder from OSC 7, the command line from
    /// OSC 133;C (with any `;` in it kept), and who set the title.
    #[test]
    fn records_cwd_cmdline_and_title_owner() {
        let mut t = Term::new(40, 10, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b]133;A\x07\x1b]7;file://mac.local/Users/me/My%20Code\x07$ ");
        assert_eq!(t.cwd.as_deref(), Some("/Users/me/My Code"));
        let rev = t.title_revision;
        feed(&mut t, &mut p, "\x1b]2;shell title\x07");
        assert_eq!(t.title_cmd, None, "set at a prompt, so the shell's own");

        feed(&mut t, &mut p, "\x1b]133;B\x07\x1b]133;C;cmdline=make; echo done\x07");
        let id = t.current_command().unwrap();
        assert_eq!(t.command(id).unwrap().cmdline.as_deref(), Some("make; echo done"));
        assert!(t.title_revision != rev);
        feed(&mut t, &mut p, "\x1b]0;vim; main.rs\x07");
        assert_eq!(t.title.as_deref(), Some("vim; main.rs"));
        assert_eq!(t.title_cmd, Some(id), "set while the command ran");
    }

    /// fish 4 marks its own prompts and commands as well as ours: the
    /// repeats are the same command, and the command line is kept.
    #[test]
    fn repeated_marks_are_one_command() {
        let mut t = Term::new(40, 10, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b]133;A;click_events=1\x07\x1b]133;A\x07> \x1b]133;B\x07");
        let id = t.current_command().unwrap();
        feed(&mut t, &mut p, "\x1b]133;C\x07\x1b]133;C;cmdline=make\x07");
        assert_eq!(t.current_command(), Some(id));
        assert_eq!(t.command(id).unwrap().cmdline.as_deref(), Some("make"));
        let events = t.take_command_events();
        assert_eq!(events.len(), 1, "one start, not two: {events:?}");
        feed(&mut t, &mut p, "\x1b]133;D;0\x07\x1b]133;D;0\x07");
        assert_eq!(t.take_command_events().len(), 1);
    }

    /// Strings ended by ST (`ESC \`) instead of BEL: fish 4, tmux and vim
    /// send them. The `\` belongs to the terminator, not the screen.
    #[test]
    fn st_terminated_strings_leave_no_backslash() {
        let mut t = Term::new(40, 5, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b]0;title\x1b\\\x1b]133;A\x1b\\\x1bP+q696e646e\x1b\\abc");
        let row0 = |t: &Term| {
            let h = t.grid().history_len();
            t.text_range(h, 0, h, 39).trim_end().to_string()
        };
        assert_eq!(row0(&t), "abc");
        assert_eq!(t.title.as_deref(), Some("title"));
        assert!(t.current_command().is_some(), "the OSC 133 still counts");
        // And BEL-ended ones still go straight back to the fast path.
        feed(&mut t, &mut p, "\x1b]2;x\x07def");
        assert_eq!(row0(&t), "abcdef");
    }

    /// A job submitted over ssh is followed through the same ssh, until the
    /// shell back home reports its folder again.
    #[test]
    fn jobs_remember_the_ssh_they_came_through() {
        let mut t = Term::new(60, 10, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b]133;A\x07\x1b]7;file://mac.local/Users/me\x07$ \x1b]133;B\x07");
        feed(&mut t, &mut p, "\x1b]133;C;cmdline=ssh -J bastion hpc\x07");
        // The remote shell, with its own integration.
        feed(&mut t, &mut p, "\x1b]133;A\x07\x1b]7;file://login1/home/me\x07$ \x1b]133;B\x07\x1b]133;C;cmdline=sbatch run.sh\x07");
        t.record_job(jobs::JobKind::Slurm, "4821337".into());
        let job = t.jobs.last().unwrap().clone();
        assert_eq!(job.via.as_deref(), Some("ssh -J bastion hpc"));
        assert_eq!(t.command(t.current_command().unwrap()).unwrap().job.as_ref(), Some(&job));
        // Recorded once.
        t.record_job(jobs::JobKind::Slurm, "4821337".into());
        assert_eq!(t.jobs.len(), 1);
        // Home again: later jobs are local.
        feed(&mut t, &mut p, "\x1b]133;D;0\x07\x1b]133;A\x07\x1b]7;file://mac.local/Users/me\x07$ \x1b]133;B\x07\x1b]133;C;cmdline=nexus cpu --detach make\x07");
        t.record_job(jobs::JobKind::Nexus, "j-1a2b3c4d".into());
        assert_eq!(t.jobs.last().unwrap().via, None);
        assert_eq!(t.running_cmdline(), Some("nexus cpu --detach make"));
    }

    /// A rerun (numbers in the command line ignored) marks the lines that
    /// differ from the last run, and says what they were.
    #[test]
    fn reruns_mark_what_changed() {
        let mut t = Term::new(60, 20, 1000);
        let mut p = Processor::new();
        let run = |t: &mut Term, p: &mut Processor, cmd: &str, out: &str| {
            feed(t, p, &format!("\x1b]133;A\x07$ \x1b]133;B\x07{cmd}\r\n\x1b]133;C;cmdline={cmd}\x07{out}\x1b]133;D;0\x07"));
        };
        run(&mut t, &mut p, "python train.py --lr 3e-4", "epoch 1 loss=0.90\r\nepoch 2 loss=0.71\r\ndone\r\n");
        let first = t.current_command().unwrap();
        assert!(t.command(first).unwrap().rerun.is_none(), "nothing to compare the first run with");
        run(&mut t, &mut p, "python train.py --lr 1e-3", "epoch 1 loss=0.90\r\nepoch 2 loss=0.65\r\nwarning: lr high\r\ndone\r\n");
        let second = t.current_command().unwrap();
        let changes = t.command(second).unwrap().rerun.clone().unwrap();
        assert_eq!(changes.len(), 2, "{changes:?}");
        // Find the marked rows and what they say.
        let notes: Vec<String> = (0..t.grid().total_lines()).filter_map(|l| t.rerun_note(l)).collect();
        assert_eq!(notes, ["Last run: epoch 2 loss=0.71", "Not in the last run"]);
        // A different command isn't a rerun.
        run(&mut t, &mut p, "make test", "ok\r\n");
        assert!(t.command(t.current_command().unwrap()).unwrap().rerun.is_none());
    }

    #[test]
    fn file_urls_decode() {
        assert_eq!(parse_file_url(b"file:///tmp").as_deref(), Some("/tmp"));
        assert_eq!(parse_file_url(b"file://h/a%2Fb%zz").as_deref(), Some("/a/b%zz"));
        assert_eq!(parse_file_url(b"http://h/x"), None);
    }

    /// A command as zsh with Tachyon's integration reports it.
    #[test]
    fn osc_133_tracks_a_command() {
        let mut t = Term::new(40, 10, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b]133;A\x07~\r\n$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a b c\r\n");
        let id = t.current_command().unwrap();
        assert_eq!(t.command(id).unwrap().state, CommandState::Running);
        assert_eq!(t.take_command_events(), [CommandEvent::Started(id)]);

        // Marks sit on the rows: prompt on 0, input on 1, output on 2.
        assert!(t.grid().row(0).mark.has(RowMark::PROMPT));
        assert!(t.grid().row(1).mark.has(RowMark::INPUT));
        assert!(t.grid().row(2).mark.has(RowMark::OUTPUT));
        assert_eq!(t.grid().row(1).mark.cmd, id);

        feed(&mut t, &mut p, "\x1b]133;D;2\x07\x1b]133;A\x07");
        assert_eq!(t.command(id).unwrap().state, CommandState::Done);
        assert_eq!(t.command(id).unwrap().exit, Some(2));
        assert_eq!(t.take_command_events(), [CommandEvent::Finished(id, Some(2))]);
        assert_ne!(t.current_command(), Some(id));
    }

    /// Marks survive scrolling into packed history and a reflowing resize.
    #[test]
    fn marks_survive_history_and_reflow() {
        let mut t = Term::new(20, 3, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07");
        let id = t.current_command().unwrap();
        for i in 0..6 {
            feed(&mut t, &mut p, &format!("line {i}\r\n"));
        }
        assert!(t.grid().history_len() > 0);
        assert!(t.grid().mark(0).has(RowMark::INPUT));
        t.resize(10, 3);
        let found = (0..t.grid().total_lines()).find(|&l| t.grid().mark(l).has(RowMark::INPUT));
        assert_eq!(found.map(|l| t.grid().mark(l).cmd), Some(id));
        assert_eq!(t.line_text(found.unwrap()), "$ make");
    }

    /// Growing and then shrinking a window (macOS Stage Manager animates a
    /// window this way when it comes forward) must leave the text where it
    /// was, not push it into scrollback behind empty rows.
    #[test]
    fn grow_then_shrink_keeps_the_screen() {
        let mut t = Term::new(120, 32, 1000);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "alpha beta gamma\r\n/usr/local/bin/tool\r\n> ");
        for (c, r) in [(160, 48), (214, 66), (120, 32)] {
            t.resize(c, r);
        }
        assert_eq!(t.grid().history_len(), 0);
        assert_eq!(&screen(&t)[..3], ["alpha beta gamma", "/usr/local/bin/tool", ">"]);
        assert_eq!((t.cursor_state().x, t.cursor_state().y), (2, 2));
    }

    /// Making a window taller brings scrollback back into view, keeping the
    /// cursor on the bottom row, rather than adding empty rows below it.
    #[test]
    fn growing_brings_scrollback_back() {
        let mut t = Term::new(20, 3, 100);
        let mut p = Processor::new();
        feed(&mut t, &mut p, "one\r\ntwo\r\nthree\r\nfour\r\n$ ");
        assert_eq!(t.grid().history_len(), 2);
        t.resize(20, 5);
        assert_eq!(t.grid().history_len(), 0);
        assert_eq!(screen(&t), ["one", "two", "three", "four", "$"]);
        assert_eq!(t.cursor_state().y, 4);
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

#[cfg(test)]
mod image_tests {
    use super::*;
    use base64::Engine;

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    /// A PNG header for a `w` by `h` image: enough for the terminal, which
    /// leaves decoding to the renderer.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    fn term() -> (Term, Processor) {
        let mut t = Term::new(40, 10, 100);
        t.set_cell_pixels(10, 20);
        (t, Processor::new())
    }

    #[test]
    fn iterm_images_sit_on_lines_and_scroll_with_them() {
        let (mut t, mut p) = term();
        p.advance(&mut t, b"before\r\n");
        let seq = format!("\x1b]1337;File=name=eC5wbmc=;inline=1:{}\x07", b64(&png(100, 60)));
        p.advance(&mut t, seq.as_bytes());
        let v = t.visible_images();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].row, v[0].col, v[0].cols, v[0].rows), (1, 0, 10, 3));
        // The cursor is after its bottom-right corner.
        assert_eq!((t.cursor.x, t.cursor.y), (10, 3));
        // Output scrolls it up, then off the screen into the scrollback.
        p.advance(&mut t, "\r\n".repeat(8).as_bytes());
        assert_eq!(t.visible_images()[0].row, -1, "partly above the screen");
        p.advance(&mut t, "\r\n".repeat(5).as_bytes());
        assert!(t.visible_images().is_empty());
        // Scrolled back to, it's there again.
        t.scroll_display(6);
        assert_eq!(t.visible_images()[0].row, 0);
        // Clearing the scrollback drops it.
        t.scroll_to_bottom();
        p.advance(&mut t, b"\x1b[3J");
        t.scroll_display(6);
        assert!(t.visible_images().is_empty());
    }

    #[test]
    fn images_leave_with_their_scrollback() {
        let mut t = Term::new(20, 4, 3);
        t.set_cell_pixels(10, 20);
        let mut p = Processor::new();
        let seq = format!("\x1b]1337;File=inline=1:{}\x07\r\n", b64(&png(10, 20)));
        p.advance(&mut t, seq.as_bytes());
        assert_eq!(t.images.len(), 1);
        p.advance(&mut t, "line\r\n".repeat(20).as_bytes());
        // Further back than the scrollback keeps: forgotten on the next one.
        p.advance(&mut t, seq.as_bytes());
        assert_eq!(t.images.len(), 1);
    }

    #[test]
    fn kitty_chunks_split_across_reads() {
        let (mut t, mut p) = term();
        let pixels = b64(&[200u8; 4 * 20 * 40]);
        let (a, b) = pixels.split_at(pixels.len() / 2);
        let seq = format!("\x1b_Ga=T,f=32,s=20,v=40,i=5,m=1;{a}\x1b\\\x1b_Gm=0;{b}\x1b\\after");
        // Fed a few bytes at a time, so terminators straddle reads.
        for chunk in seq.as_bytes().chunks(7) {
            p.advance(&mut t, chunk);
        }
        let v = t.visible_images();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].cols, v[0].rows, v[0].image.width), (2, 2, 20));
        assert_eq!(t.take_replies(), b"\x1b_Gi=5;OK\x1b\\");
        // The text after it printed normally, right of the image's last row.
        assert_eq!(t.grid.row(1).cells()[2].ch, 'a' as u32);
        // Shown again by id, then deleted with its data.
        p.advance(&mut t, b"\r\n\x1b_Ga=p,i=5,q=2\x1b\\");
        assert_eq!(t.visible_images().len(), 2);
        p.advance(&mut t, b"\x1b_Ga=d,d=I,i=5\x1b\\");
        assert!(t.visible_images().is_empty() && t.kitty_images.is_empty());
        p.advance(&mut t, b"\x1b_Ga=p,i=5\x1b\\");
        assert_eq!(t.take_replies(), b"\x1b_Gi=5;ENOENT:no such image\x1b\\");
    }

    #[test]
    fn kitty_queries_and_cell_size_reports() {
        let (mut t, mut p) = term();
        p.advance(&mut t, format!("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;{}\x1b\\", b64(&[0, 0, 0])).as_bytes());
        assert_eq!(t.take_replies(), b"\x1b_Gi=31;OK\x1b\\");
        assert!(t.visible_images().is_empty(), "a query shows nothing");
        p.advance(&mut t, b"\x1b[16t\x1b[14t");
        assert_eq!(t.take_replies(), b"\x1b[6;20;10t\x1b[4;200;400t");
        // Other APC strings are ignored, and the parser carries on.
        p.advance(&mut t, b"\x1b_something else\x1b\\ok");
        assert_eq!(t.grid.row(0).cells()[0].ch, 'o' as u32);
    }

    #[test]
    fn full_screen_programs_images_go_with_their_screen() {
        let (mut t, mut p) = term();
        p.advance(&mut t, b"\x1b[?1049h");
        p.advance(&mut t, format!("\x1b]1337;File=inline=1:{}\x07", b64(&png(10, 20))).as_bytes());
        assert_eq!(t.visible_images().len(), 1);
        p.advance(&mut t, b"\x1b[?1049l");
        assert!(t.visible_images().is_empty() && t.images.is_empty());
    }

    #[test]
    fn device_attributes_answer_what_was_asked() {
        let (mut t, mut p) = term();
        p.advance(&mut t, b"\x1b[c\x1b[>c\x1b[=c");
        assert_eq!(t.take_replies(), b"\x1b[?62;4;22c\x1b[>0;10;1c", "DA1 with sixel, DA2, no DA3");
        // tmux's key-modifier request isn't an underline.
        p.advance(&mut t, b"\x1b[>4;1mx");
        assert!(!t.grid.row(0).cells()[0].flags.contains(CellFlags::UNDERLINE));
        // A sixel tool's size query isn't a scroll.
        p.advance(&mut t, b"\x1b[?2;1;0S\x1b[?1;1;0S");
        assert_eq!(t.take_replies(), b"\x1b[?2;0;400;200S\x1b[?1;0;1024S");
        assert_eq!(t.grid.row(0).cells()[0].ch, 'x' as u32);
    }

    #[test]
    fn sixel_images_are_placed_at_the_cursor() {
        let (mut t, mut p) = term();
        p.advance(&mut t, b"ab\x1bPq\"1;1;20;40#1;2;100;0;0#1!20~-!20~-!20~-!20~-!20~-!20~-!20~\x1b\\");
        let v = t.visible_images();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].col, v[0].row, v[0].cols, v[0].rows, v[0].image.width), (2, 0, 2, 2, 20));
        assert_eq!((t.cursor.x, t.cursor.y), (2, 2), "below it, at its left edge");
        // The same place drawn again (tmux redrawing): the old one goes.
        p.advance(&mut t, b"\x1b[1;3H\x1bPq\"1;1;20;40#1!20~\x1b\\");
        assert_eq!(t.images.len(), 1);
    }

    #[test]
    fn images_follow_region_scrolls() {
        // tmux's layout: rows 0-8 scroll, row 9 is its status line.
        let (mut t, mut p) = term();
        p.advance(&mut t, b"\x1b[?1049h\x1b[1;9r\x1b[5;1H");
        p.advance(&mut t, format!("\x1b_Ga=T,f=32,s=10,v=40,q=2;{}\x1b\\", b64(&[9u8; 10 * 40 * 4])).as_bytes());
        assert_eq!(t.visible_images()[0].row, 4);
        p.advance(&mut t, b"\x1b[9;1H\n\n");
        assert_eq!(t.visible_images()[0].row, 2, "up with the region's lines");
        p.advance(&mut t, b"\n\n\n");
        assert_eq!(t.visible_images()[0].row, -1, "partly off the top: clipped there");
        p.advance(&mut t, b"\x1b[1;1H\x1bM");
        assert_eq!(t.visible_images()[0].row, 0, "reverse index brings it down");
        p.advance(&mut t, b"\n\n\n\n\n\n\n\n\n\n\n");
        assert!(t.visible_images().is_empty(), "wholly above: gone");
    }

    /// Replays a recording of a real program's output (`TACHYON_REPLAY`),
    /// such as tmux showing images, and prints where the images ended up.
    #[test]
    #[ignore]
    fn replay_images() {
        let Some(path) = std::env::var_os("TACHYON_REPLAY") else { return };
        let bytes = std::fs::read(path).unwrap();
        let mut t = Term::new(100, 40, 1000);
        t.set_cell_pixels(14, 28);
        let mut p = Processor::new();
        if std::env::var_os("TACHYON_TRACE").is_some() {
            // Where the number of images changes, and what came then.
            let mut n = 0;
            for (i, b) in bytes.iter().enumerate() {
                p.advance(&mut t, std::slice::from_ref(b));
                if t.images.len() != n {
                    n = t.images.len();
                    let around = &bytes[i.saturating_sub(120)..(i + 40).min(bytes.len())];
                    println!("@{i}: {n} images after {:?}", String::from_utf8_lossy(around).chars().filter(|c| !c.is_ascii_alphanumeric() || true).take(200).collect::<String>().escape_debug().to_string());
                }
            }
        } else {
            p.advance(&mut t, &bytes);
        }
        for v in t.visible_images() {
            println!("image {}x{} px at row {} col {}, {}x{} cells", v.image.width, v.image.height, v.row, v.col, v.cols, v.rows);
        }
        for y in 0..t.rows() {
            let line: String = t.grid.row(y).cells().iter().map(|c| char::from_u32(c.ch).filter(|c| *c >= ' ').unwrap_or(' ')).collect();
            println!("{y:2}|{}", line.trim_end());
        }
    }

    #[test]
    fn text_over_an_iterm_or_sixel_image_replaces_it() {
        let (mut t, mut p) = term();
        p.advance(&mut t, format!("\x1b]1337;File=inline=1:{}\x07", b64(&png(100, 60))).as_bytes());
        p.advance(&mut t, format!("\x1b[5;1H\x1b_Ga=T,f=32,s=10,v=20,q=2;{}\x1b\\", b64(&[9u8; 10 * 20 * 4])).as_bytes());
        assert_eq!(t.images.len(), 2);
        // tmux repainting: spaces over the first image's rows.
        p.advance(&mut t, b"\x1b[2;5H    ");
        assert_eq!(t.images.len(), 1, "the iTerm2 image gave way");
        // A kitty image is a layer under the text, as in kitty.
        p.advance(&mut t, b"\x1b[5;1Hkitty\x1b[K");
        assert_eq!(t.images.len(), 1);
        // Its row erased whole, as tmux repaints for another window: gone.
        p.advance(&mut t, b"\x1b[5;1H\x1b[2K");
        assert!(t.images.is_empty());
    }
}
