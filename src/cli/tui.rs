//! Lightweight, zero-extra-dependency interactive terminal UI (`tachyon tui`)
//! compiled directly into both full GUI and 1.1 MB `--no-default-features`
//! binaries.
//!
//! Designed for responsive use over SSH, Mosh, and narrow mobile terminals as
//! well as wide desktop splits:
//! - `< 76` columns: single-column agent/pane switcher
//! - `>= 76` columns: sidebar list + live captured pane preview
//! - Attached view: full-screen raw PTY stream with persistent `^] list` bar

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

use unicode_width::UnicodeWidthChar;

use crate::cli::{connect, GlobalOpts};
use crate::mux::client::{AttachedStream, MuxClient, StreamChunk};
use crate::mux::model::{
    agent_identity, git_info_for_dir, short_path, BlockInfo, ServerSnapshot,
};
use crate::term::cell::{CellFlags, CLUSTER_TAG};
use crate::term::color::Color;
use crate::term::{Processor, ProgramState, ProgramStatus, Term};

const NARROW_THRESHOLD: u16 = 76;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    Narrow,
    Wide,
}

impl LayoutMode {
    pub fn for_cols(cols: u16) -> Self {
        if cols < NARROW_THRESHOLD {
            Self::Narrow
        } else {
            Self::Wide
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    LiveAgents,
    Spaces,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiBlockItem {
    pub block_id: String,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_name: String,
    pub label: String,
    pub cwd: Option<String>,
    pub cwd_short: String,
    pub git_branch: Option<String>,
    pub alive: bool,
    pub status: Option<ProgramStatus>,
    pub status_inferred: bool,
    pub urgency: u8,
    pub section: Section,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiRow {
    Header(String),
    Item(TuiBlockItem),
}

impl TuiRow {
    fn as_item(&self) -> Option<&TuiBlockItem> {
        match self {
            Self::Item(it) => Some(it),
            Self::Header(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiKey {
    Char(char),
    Ctrl(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Backspace,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Pure ANSI/UTF-8 input byte decoder used in List Mode.
pub fn decode_list_keys(bytes: &[u8]) -> Vec<TuiKey> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            if i + 2 < bytes.len() && (bytes[i + 1] == b'[' || bytes[i + 1] == b'O') {
                match bytes[i + 2] {
                    b'A' => {
                        out.push(TuiKey::Up);
                        i += 3;
                        continue;
                    }
                    b'B' => {
                        out.push(TuiKey::Down);
                        i += 3;
                        continue;
                    }
                    b'C' => {
                        out.push(TuiKey::Right);
                        i += 3;
                        continue;
                    }
                    b'D' => {
                        out.push(TuiKey::Left);
                        i += 3;
                        continue;
                    }
                    b'H' => {
                        out.push(TuiKey::Home);
                        i += 3;
                        continue;
                    }
                    b'F' => {
                        out.push(TuiKey::End);
                        i += 3;
                        continue;
                    }
                    b'1' | b'4' | b'5' | b'6' | b'7' | b'8' => {
                        if let Some(tilde) = bytes[i + 2..].iter().position(|&c| c == b'~') {
                            let param = &bytes[i + 2..i + 2 + tilde];
                            match param {
                                b"1" | b"7" => out.push(TuiKey::Home),
                                b"4" | b"8" => out.push(TuiKey::End),
                                b"5" => out.push(TuiKey::PageUp),
                                b"6" => out.push(TuiKey::PageDown),
                                _ => {}
                            }
                            i += 3 + tilde;
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            out.push(TuiKey::Esc);
            i += 1;
            continue;
        }
        match b {
            b'\r' | b'\n' => {
                out.push(TuiKey::Enter);
                i += 1;
            }
            0x7f | 0x08 => {
                out.push(TuiKey::Backspace);
                i += 1;
            }
            0x01..=0x1a => {
                let ch = (b + b'a' - 1) as char;
                out.push(TuiKey::Ctrl(ch));
                i += 1;
            }
            0x1c..=0x1f => {
                out.push(TuiKey::Ctrl((b - 0x1b + b'[') as char));
                i += 1;
            }
            _ => {
                if let Ok(s) = std::str::from_utf8(&bytes[i..]) {
                    if let Some(ch) = s.chars().next() {
                        out.push(TuiKey::Char(ch));
                        i += ch.len_utf8();
                        continue;
                    }
                }
                i += 1;
            }
        }
    }
    out
}

pub struct AttachedView {
    pub block_id: String,
    pub title: String,
    pub term: Term,
    pub processor: Processor,
    pub exited: Option<i32>,
}

impl AttachedView {
    pub fn new(block_id: String, title: String, cols: u16, rows: u16) -> Self {
        let inner_rows = rows.saturating_sub(1).max(1) as usize;
        Self {
            block_id,
            title,
            term: Term::new(cols.max(20) as usize, inner_rows, 4_000),
            processor: Processor::new(),
            exited: None,
        }
    }
}

pub struct TuiApp {
    pub host_label: String,
    pub cols: u16,
    pub rows: u16,
    pub readonly: bool,
    pub snapshot: ServerSnapshot,
    pub rows_list: Vec<TuiRow>,
    pub selected: usize,
    pub scroll_top: usize,
    pub agents_only: bool,
    pub filter: String,
    pub filtering: bool,
    pub confirm_kill: Option<String>,
    pub toast: Option<String>,
    pub preview_block: Option<String>,
    pub preview_lines: Vec<String>,
    pub attached: Option<AttachedView>,
}

impl TuiApp {
    pub fn new(
        host_label: String,
        cols: u16,
        rows: u16,
        readonly: bool,
        agents_only: bool,
        filter: String,
        snapshot: ServerSnapshot,
    ) -> Self {
        let mut app = Self {
            host_label,
            cols: cols.max(20),
            rows: rows.max(6),
            readonly,
            snapshot,
            rows_list: Vec::new(),
            selected: 0,
            scroll_top: 0,
            agents_only,
            filter,
            filtering: false,
            confirm_kill: None,
            toast: None,
            preview_block: None,
            preview_lines: Vec::new(),
            attached: None,
        };
        app.rebuild_items();
        app
    }

    pub fn layout_mode(&self) -> LayoutMode {
        LayoutMode::for_cols(self.cols)
    }

    pub fn body_height(&self) -> usize {
        self.rows.saturating_sub(2).max(1) as usize
    }

    pub fn selected_item(&self) -> Option<&TuiBlockItem> {
        self.rows_list.get(self.selected).and_then(TuiRow::as_item)
    }

    pub fn update_snapshot(&mut self, snap: ServerSnapshot) {
        self.snapshot = snap;
        self.rebuild_items();
    }

    pub fn rebuild_items(&mut self) {
        let prev_id = self
            .selected_item()
            .map(|it| (it.block_id.clone(), it.section));

        let sess_map: HashMap<&str, &str> = self
            .snapshot
            .sessions
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str()))
            .collect();
        let win_map: HashMap<&str, String> = self
            .snapshot
            .windows
            .iter()
            .map(|w| {
                let name = w
                    .name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| w.id.clone());
                (w.id.as_str(), name)
            })
            .collect();

        let mut git_cache: HashMap<String, Option<String>> = HashMap::new();
        let query = self.filter.trim().to_ascii_lowercase();

        let build_item = |b: &BlockInfo, section: Section, cache: &mut HashMap<String, Option<String>>| {
            let sess_name = sess_map
                .get(b.session_id.as_str())
                .copied()
                .unwrap_or(&b.session_id)
                .to_string();
            let win_name = win_map
                .get(b.window_id.as_str())
                .cloned()
                .unwrap_or_else(|| b.window_id.clone());
            let cwd_short = b
                .cwd
                .as_deref()
                .map(short_path)
                .unwrap_or_else(|| "—".into());
            let git_branch = b.cwd.as_deref().and_then(|dir| {
                cache
                    .entry(dir.to_string())
                    .or_insert_with(|| {
                        let p = Path::new(dir);
                        if p.exists() {
                            git_info_for_dir(p).map(|g| g.branch)
                        } else {
                            None
                        }
                    })
                    .clone()
            });
            let urgency = match b.status.as_ref().map(|s| s.state) {
                Some(ProgramState::Blocked) => 0,
                Some(ProgramState::Error) => 1,
                Some(ProgramState::Working) => 2,
                Some(ProgramState::Done) => 3,
                Some(ProgramState::Idle) => 4,
                None => 5,
            };
            TuiBlockItem {
                block_id: b.id.clone(),
                session_id: b.session_id.clone(),
                session_name: sess_name,
                window_id: b.window_id.clone(),
                window_name: win_name,
                label: b.display_name(),
                cwd: b.cwd.clone(),
                cwd_short,
                git_branch,
                alive: b.alive,
                status: b.status.clone(),
                status_inferred: b.status_inferred,
                urgency,
                section,
            }
        };

        let matches_query = |it: &TuiBlockItem| -> bool {
            if query.is_empty() {
                return true;
            }
            let st_str = it
                .status
                .as_ref()
                .map(|s| {
                    format!(
                        "{} {} {}",
                        s.state.as_str(),
                        s.app.as_deref().unwrap_or(""),
                        s.msg.as_deref().unwrap_or("")
                    )
                })
                .unwrap_or_default();
            let hay = format!(
                "{} {} {} {} {} {} {}",
                it.session_name,
                it.window_name,
                it.label,
                it.block_id,
                it.cwd_short,
                it.git_branch.as_deref().unwrap_or(""),
                st_str
            )
            .to_ascii_lowercase();
            query.split_whitespace().all(|term| hay.contains(term))
        };

        let mut agents: Vec<TuiBlockItem> = self
            .snapshot
            .blocks
            .iter()
            .filter(|b| b.status.is_some())
            .map(|b| build_item(b, Section::LiveAgents, &mut git_cache))
            .filter(&matches_query)
            .collect();
        agents.sort_by(|a, b| {
            a.urgency
                .cmp(&b.urgency)
                .then_with(|| a.session_name.cmp(&b.session_name))
                .then_with(|| a.block_id.cmp(&b.block_id))
        });

        let mut spaces: Vec<TuiBlockItem> = if self.agents_only {
            Vec::new()
        } else {
            self.snapshot
                .blocks
                .iter()
                .map(|b| build_item(b, Section::Spaces, &mut git_cache))
                .filter(&matches_query)
                .collect()
        };
        spaces.sort_by(|a, b| {
            a.session_name
                .cmp(&b.session_name)
                .then_with(|| a.window_name.cmp(&b.window_name))
                .then_with(|| a.block_id.cmp(&b.block_id))
        });

        self.rows_list.clear();
        if !agents.is_empty() {
            self.rows_list
                .push(TuiRow::Header(format!("LIVE AGENTS ({})", agents.len())));
            for a in agents {
                self.rows_list.push(TuiRow::Item(a));
            }
        }
        if !self.agents_only && !spaces.is_empty() {
            self.rows_list
                .push(TuiRow::Header(format!("SPACES & PANES ({})", spaces.len())));
            for s in spaces {
                self.rows_list.push(TuiRow::Item(s));
            }
        }

        if let Some((bid, sec)) = prev_id {
            if let Some(idx) = self
                .rows_list
                .iter()
                .position(|r| r.as_item().is_some_and(|it| it.block_id == bid && it.section == sec))
                .or_else(|| {
                    self.rows_list
                        .iter()
                        .position(|r| r.as_item().is_some_and(|it| it.block_id == bid))
                })
            {
                self.selected = idx;
                self.ensure_selected_visible();
                return;
            }
        }
        self.selected = self
            .rows_list
            .iter()
            .position(|r| r.as_item().is_some())
            .unwrap_or(0);
        self.ensure_selected_visible();
    }

    pub fn move_selection(&mut self, delta: isize) {
        let item_indices: Vec<usize> = self
            .rows_list
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.as_item().map(|_| i))
            .collect();
        if item_indices.is_empty() {
            self.selected = 0;
            self.scroll_top = 0;
            return;
        }
        let cur = item_indices
            .iter()
            .position(|&idx| idx == self.selected)
            .unwrap_or(0) as isize;
        let len = item_indices.len() as isize;
        let next = (cur + delta).clamp(0, len - 1) as usize;
        self.selected = item_indices[next];
        self.ensure_selected_visible();
    }

    pub fn jump_first(&mut self) {
        if let Some(idx) = self.rows_list.iter().position(|r| r.as_item().is_some()) {
            self.selected = idx;
            self.scroll_top = 0;
        }
    }

    pub fn jump_last(&mut self) {
        if let Some(idx) = self.rows_list.iter().rposition(|r| r.as_item().is_some()) {
            self.selected = idx;
            self.ensure_selected_visible();
        }
    }

    pub fn jump_next_blocked(&mut self) -> bool {
        let n = self.rows_list.len();
        if n == 0 {
            return false;
        }
        for step in 1..=n {
            let idx = (self.selected + step) % n;
            if let Some(it) = self.rows_list[idx].as_item() {
                if it.status.as_ref().is_some_and(|s| {
                    matches!(s.state, ProgramState::Blocked | ProgramState::Error)
                }) {
                    self.selected = idx;
                    self.ensure_selected_visible();
                    return true;
                }
            }
        }
        false
    }

    fn ensure_selected_visible(&mut self) {
        let h = self.body_height();
        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
            // Include the section header directly above when selecting the first item.
            if self.scroll_top > 0
                && matches!(self.rows_list.get(self.scroll_top - 1), Some(TuiRow::Header(_)))
            {
                self.scroll_top -= 1;
            }
        } else if self.selected >= self.scroll_top + h {
            self.scroll_top = self.selected + 1 - h;
        }
    }

    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8192);
        // Synchronized update + hide cursor while painting.
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l\x1b[0m");
        if let Some(att) = &self.attached {
            self.render_attached(&mut out, att);
        } else {
            self.render_list(&mut out);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        out
    }

    fn render_list(&self, out: &mut Vec<u8>) {
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        let body_h = self.body_height();
        let mode = self.layout_mode();

        let total_panes = self.snapshot.blocks.len();
        let blocked_cnt = self
            .snapshot
            .blocks
            .iter()
            .filter(|b| {
                b.status
                    .as_ref()
                    .is_some_and(|s| matches!(s.state, ProgramState::Blocked | ProgramState::Error))
            })
            .count();

        // Row 1: Top Header Bar (#17191D charcoal).
        let mut hdr = format!(" TACHYON · {} · {total_panes}p", self.host_label);
        if self.agents_only {
            hdr.push_str(" · [AGENTS]");
        }
        if !self.filter.is_empty() && !self.filtering {
            hdr.push_str(&format!(" · /{}", self.filter));
        }
        let alert_tag = if blocked_cnt > 0 {
            format!(" ! {blocked_cnt} blocked ")
        } else {
            String::new()
        };
        let hdr_avail = cols.saturating_sub(display_width(&alert_tag));
        let left_hdr = fit_columns(&hdr, hdr_avail);
        let pad_hdr = cols.saturating_sub(display_width(&left_hdr) + display_width(&alert_tag));

        out.extend_from_slice(b"\x1b[1;1H\x1b[48;2;23;25;29m\x1b[38;2;238;238;239m");
        out.extend_from_slice(left_hdr.as_bytes());
        if !alert_tag.is_empty() {
            for _ in 0..pad_hdr {
                out.push(b' ');
            }
            out.extend_from_slice(b"\x1b[38;2;245;158;11m\x1b[1m");
            out.extend_from_slice(alert_tag.as_bytes());
        } else {
            for _ in 0..pad_hdr {
                out.push(b' ');
            }
        }
        out.extend_from_slice(b"\x1b[0m");

        // Body rows (2 .. rows - 1).
        let (left_w, right_w) = match mode {
            LayoutMode::Narrow => (cols, 0),
            LayoutMode::Wide => {
                let lw = ((cols * 38) / 100).clamp(32, 46).min(cols.saturating_sub(24));
                (lw, cols.saturating_sub(lw + 1))
            }
        };

        let preview_meta = self.build_wide_preview_lines(right_w, body_h);

        for r in 0..body_h {
            let screen_row = 2 + r;
            let list_idx = self.scroll_top + r;
            let _ = write!(out, "\x1b[{screen_row};1H\x1b[48;2;17;18;20m");

            if let Some(row_entry) = self.rows_list.get(list_idx) {
                match row_entry {
                    TuiRow::Header(title) => {
                        out.extend_from_slice(b"\x1b[38;2;150;178;215m\x1b[1m");
                        let text = fit_columns(&format!(" ── {title}"), left_w);
                        let w = display_width(&text);
                        out.extend_from_slice(text.as_bytes());
                        for _ in w..left_w {
                            out.push(b' ');
                        }
                        out.extend_from_slice(b"\x1b[0m");
                    }
                    TuiRow::Item(it) => {
                        let is_sel = list_idx == self.selected;
                        if is_sel {
                            out.extend_from_slice("\x1b[48;2;44;46;49m\x1b[38;2;150;178;215m▌".as_bytes());
                        } else {
                            out.extend_from_slice(b" ");
                        }
                        let (glyph, color_seq) = status_badge(it.status.as_ref(), it.alive);
                        out.extend_from_slice(color_seq.as_bytes());
                        out.extend_from_slice(glyph.as_bytes());
                        out.push(b' ');

                        if is_sel {
                            out.extend_from_slice(b"\x1b[38;2;238;238;239m\x1b[1m");
                        } else {
                            out.extend_from_slice(b"\x1b[38;2;210;212;217m\x1b[22m");
                        }

                        let mut label = if it.section == Section::LiveAgents {
                            let app = it
                                .status
                                .as_ref()
                                .and_then(|s| s.app.as_deref())
                                .filter(|s| !s.is_empty())
                                .map(|a| agent_identity(a).1)
                                .unwrap_or_else(|| it.label.clone());
                            format!("{app} · {}", it.session_name)
                        } else if it.session_name == it.window_name {
                            it.session_name.clone()
                        } else {
                            format!("{}/{}", it.session_name, it.window_name)
                        };

                        let right_tag = if it.section == Section::LiveAgents {
                            it.status
                                .as_ref()
                                .map(|s| format!(" [{}]", s.state.as_str()))
                                .unwrap_or_default()
                        } else if let Some(br) = &it.git_branch {
                            format!(" [{br}]")
                        } else {
                            String::new()
                        };

                        let used_prefix = 3; // marker + glyph + space
                        let body_avail = left_w.saturating_sub(used_prefix);
                        let tag_w = if body_avail > 18 {
                            display_width(&right_tag).min(body_avail / 2)
                        } else {
                            0
                        };
                        let name_avail = body_avail.saturating_sub(tag_w);
                        if !it.alive {
                            label.push_str(" (exit)");
                        }
                        let name_fit = fit_columns(&label, name_avail);
                        out.extend_from_slice(name_fit.as_bytes());

                        let mut written = used_prefix + display_width(&name_fit);
                        if tag_w > 0 && !right_tag.is_empty() {
                            let tag_fit = fit_columns(&right_tag, left_w.saturating_sub(written));
                            let gap = left_w.saturating_sub(written + display_width(&tag_fit));
                            for _ in 0..gap {
                                out.push(b' ');
                            }
                            out.extend_from_slice(b"\x1b[22m\x1b[38;2;152;153;155m");
                            out.extend_from_slice(tag_fit.as_bytes());
                            written = left_w;
                        }
                        for _ in written..left_w {
                            out.push(b' ');
                        }
                        out.extend_from_slice(b"\x1b[0m");
                    }
                }
            } else if r == 0 && self.rows_list.is_empty() {
                out.extend_from_slice(b"\x1b[38;2;152;153;155m");
                let empty = fit_columns(" No matching panes or live agents", left_w);
                let ew = display_width(&empty);
                out.extend_from_slice(empty.as_bytes());
                for _ in ew..left_w {
                    out.push(b' ');
                }
                out.extend_from_slice(b"\x1b[0m");
            } else {
                for _ in 0..left_w {
                    out.push(b' ');
                }
                out.extend_from_slice(b"\x1b[0m");
            }

            if mode == LayoutMode::Wide {
                out.extend_from_slice("\x1b[48;2;17;18;20m\x1b[38;2;60;63;70m│\x1b[38;2;210;212;217m".as_bytes());
                if let Some((text, sgr)) = preview_meta.get(r) {
                    out.extend_from_slice(sgr.as_bytes());
                    let fit = fit_columns(text, right_w);
                    let w = display_width(&fit);
                    out.extend_from_slice(fit.as_bytes());
                    for _ in w..right_w {
                        out.push(b' ');
                    }
                } else {
                    for _ in 0..right_w {
                        out.push(b' ');
                    }
                }
                out.extend_from_slice(b"\x1b[0m");
            }
        }

        // Bottom Key / Prompt Bar (row = rows).
        let bottom = if self.filtering {
            format!(" /{}█  (Enter apply · Esc cancel)", self.filter)
        } else if let Some(bid) = &self.confirm_kill {
            format!(" Kill pane {bid}? [y/N]")
        } else if let Some(msg) = &self.toast {
            format!(" {msg}")
        } else if mode == LayoutMode::Narrow {
            " j/k ↵:open !:blk a:ag /:flt q:quit".to_string()
        } else {
            " j/k:move  Enter:attach  !:blocked  a:agents  /:filter  n:new  x:kill  r:refresh  q:quit"
                .to_string()
        };
        let fit_bot = fit_columns(&bottom, cols);
        let pad_bot = cols.saturating_sub(display_width(&fit_bot));
        let _ = write!(
            out,
            "\x1b[{rows};1H\x1b[48;2;12;13;15m\x1b[38;2;152;153;155m"
        );
        out.extend_from_slice(fit_bot.as_bytes());
        for _ in 0..pad_bot {
            out.push(b' ');
        }
        out.extend_from_slice(b"\x1b[0m");
    }

    fn build_wide_preview_lines(&self, right_w: usize, body_h: usize) -> Vec<(String, &'static str)> {
        if right_w == 0 || body_h == 0 {
            return Vec::new();
        }
        let Some(it) = self.selected_item() else {
            return vec![(" Select a pane to preview its output".into(), "\x1b[38;2;105;106;109m")];
        };
        let mut lines = Vec::new();
        let header = format!(
            " {} / {}  [{}]",
            it.session_name, it.window_name, it.block_id
        );
        lines.push((header, "\x1b[38;2;238;238;239m\x1b[1m"));

        let mut meta = format!(" cwd: {}", it.cwd_short);
        if let Some(br) = &it.git_branch {
            meta.push_str(&format!("   branch: {br}"));
        }
        if let Some(st) = &it.status {
            let app = st
                .app
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|a| agent_identity(a).1)
                .unwrap_or_else(|| "agent".into());
            meta.push_str(&format!("   agent: {app} [{}]", st.state.as_str()));
            if let Some(msg) = &st.msg {
                meta.push_str(&format!(" — {msg}"));
            }
        }
        lines.push((meta, "\x1b[38;2;150;178;215m\x1b[22m"));

        let sep = " ".to_string() + &"─".repeat(right_w.saturating_sub(2).max(1));
        lines.push((sep, "\x1b[38;2;60;63;70m"));

        let remain = body_h.saturating_sub(lines.len());
        let start = self.preview_lines.len().saturating_sub(remain);
        for line in self.preview_lines.iter().skip(start) {
            lines.push((format!(" {line}"), "\x1b[38;2;210;212;217m"));
        }
        lines
    }

    fn render_attached(&self, out: &mut Vec<u8>, att: &AttachedView) {
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        let inner_rows = rows.saturating_sub(1).max(1);
        let term = &att.term;
        let grid = term.grid();
        let draw_rows = inner_rows.min(term.rows());
        let draw_cols = cols.min(term.cols());

        const SGR_STYLE: CellFlags = CellFlags::from_bits_truncate(
            CellFlags::BOLD.bits()
                | CellFlags::DIM.bits()
                | CellFlags::ITALIC.bits()
                | CellFlags::ANY_UNDERLINE.bits()
                | CellFlags::REVERSE.bits()
                | CellFlags::STRIKEOUT.bits(),
        );

        for y in 0..draw_rows {
            let _ = write!(out, "\x1b[{};1H\x1b[0m", y + 1);
            let cells = grid.row(y).cells();
            let mut cur_fg = Color::DEFAULT;
            let mut cur_bg = Color::DEFAULT;
            let mut cur_flags = CellFlags::empty();

            for c in cells.iter().take(draw_cols) {
                if c.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                let f = c.flags & SGR_STYLE;
                if c.fg != cur_fg || c.bg != cur_bg || f != cur_flags {
                    cur_fg = c.fg;
                    cur_bg = c.bg;
                    cur_flags = f;
                    emit_sgr(out, cur_fg, cur_bg, cur_flags);
                }
                if c.flags.contains(CellFlags::HIDDEN) {
                    out.push(b' ');
                } else if c.ch & CLUSTER_TAG != 0 {
                    if let Some(cl) = term.cluster(c.ch) {
                        for &ch in cl {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                    } else {
                        out.push(b' ');
                    }
                } else if let Some(ch) = c.scalar() {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                } else {
                    out.push(b' ');
                }
            }
            out.extend_from_slice(b"\x1b[0m\x1b[K");
        }
        for y in draw_rows..inner_rows {
            let _ = write!(out, "\x1b[{};1H\x1b[0m\x1b[K", y + 1);
        }

        // Persistent status bar on row `rows` (never clobbered by app output).
        let state_str = if let Some(code) = att.exited {
            format!("exited ({code}) — press Ctrl-]")
        } else if self.readonly {
            "RO".to_string()
        } else {
            "live".to_string()
        };
        let status = format!(
            " ^] list · {} · {} · {state_str}",
            self.host_label, att.title
        );
        let fit = fit_columns(&status, cols);
        let pad = cols.saturating_sub(display_width(&fit));
        let _ = write!(
            out,
            "\x1b[{rows};1H\x1b[48;2;23;25;29m\x1b[38;2;150;178;215m\x1b[1m"
        );
        out.extend_from_slice(fit.as_bytes());
        for _ in 0..pad {
            out.push(b' ');
        }
        out.extend_from_slice(b"\x1b[0m");

        let cur = term.cursor_state();
        if cur.visible && cur.y < inner_rows && cur.x < cols {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", cur.y + 1, cur.x + 1);
        }
    }
}

fn emit_sgr(out: &mut Vec<u8>, fg: Color, bg: Color, flags: CellFlags) {
    out.extend_from_slice(b"\x1b[0");
    if flags.contains(CellFlags::BOLD) {
        out.extend_from_slice(b";1");
    }
    if flags.contains(CellFlags::DIM) {
        out.extend_from_slice(b";2");
    }
    if flags.contains(CellFlags::ITALIC) {
        out.extend_from_slice(b";3");
    }
    if flags.intersects(CellFlags::ANY_UNDERLINE) {
        out.extend_from_slice(b";4");
    }
    if flags.contains(CellFlags::REVERSE) {
        out.extend_from_slice(b";7");
    }
    if flags.contains(CellFlags::STRIKEOUT) {
        out.extend_from_slice(b";9");
    }
    if let Some([r, g, b]) = fg.rgb_parts() {
        let _ = write!(out, ";38;2;{r};{g};{b}");
    } else if let Some(idx) = fg.index() {
        let _ = write!(out, ";38;5;{idx}");
    }
    if let Some([r, g, b]) = bg.rgb_parts() {
        let _ = write!(out, ";48;2;{r};{g};{b}");
    } else if let Some(idx) = bg.index() {
        let _ = write!(out, ";48;5;{idx}");
    }
    out.push(b'm');
}

fn status_badge(st: Option<&ProgramStatus>, alive: bool) -> (&'static str, &'static str) {
    if !alive {
        return ("×", "\x1b[38;2;105;106;109m");
    }
    match st.map(|s| s.state) {
        Some(ProgramState::Blocked) => ("!", "\x1b[38;2;245;158;11m"),
        Some(ProgramState::Error) => ("✖", "\x1b[38;2;220;140;149m"),
        Some(ProgramState::Working) => ("◉", "\x1b[38;2;150;178;215m"),
        Some(ProgramState::Done) => ("✓", "\x1b[38;2;166;189;145m"),
        _ => ("○", "\x1b[38;2;105;106;109m"),
    }
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn fit_columns(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let total = display_width(s);
    if total <= max_cols {
        return s.to_string();
    }
    let target = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > target {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

enum TuiEvent {
    Input(Vec<u8>),
    Resize(u16, u16),
    ServerEvent,
    Stream(u64, StreamChunk),
    StreamClosed(u64),
    Eof,
}

#[cfg(unix)]
mod term_driver {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    static WINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn handle_sigwinch(_sig: libc::c_int) {
        let fd = WINCH_WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte: u8 = b'w';
            unsafe {
                let _ = libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }

    pub fn is_tty() -> bool {
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
    }

    pub fn term_size() -> (u16, u16) {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            (ws.ws_col.max(20), ws.ws_row.max(6))
        } else {
            (80, 24)
        }
    }

    fn set_nonblock(fd: i32) {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    pub struct RawTermGuard {
        orig: libc::termios,
        prev_sigwinch: libc::sigaction,
        stop_w: i32,
        winch_w: i32,
        input_thread: Option<std::thread::JoinHandle<()>>,
    }

    impl RawTermGuard {
        pub fn enter(tx: Sender<TuiEvent>) -> Result<Self, String> {
            let mut orig: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut orig) } != 0 {
                return Err("failed to read terminal attributes".into());
            }
            let mut raw = orig;
            unsafe { libc::cfmakeraw(&mut raw) };
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
                return Err("failed to enter raw terminal mode".into());
            }

            let mut winch_fds = [-1i32; 2];
            let mut stop_fds = [-1i32; 2];
            if unsafe { libc::pipe(winch_fds.as_mut_ptr()) } != 0
                || unsafe { libc::pipe(stop_fds.as_mut_ptr()) } != 0
            {
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &orig) };
                return Err("failed to create event pipe".into());
            }
            set_nonblock(winch_fds[0]);
            set_nonblock(winch_fds[1]);
            set_nonblock(stop_fds[0]);
            WINCH_WRITE_FD.store(winch_fds[1], Ordering::Release);

            let mut prev_sigwinch: libc::sigaction = unsafe { std::mem::zeroed() };
            let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
            sa.sa_sigaction = handle_sigwinch as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            unsafe {
                libc::sigemptyset(&mut sa.sa_mask);
                let _ = libc::sigaction(libc::SIGWINCH, &sa, &mut prev_sigwinch);
            }

            let winch_r = winch_fds[0];
            let stop_r = stop_fds[0];
            let input_thread = std::thread::Builder::new()
                .name("tachyon-tui-input".into())
                .spawn(move || {
                    let mut pfds = [
                        libc::pollfd {
                            fd: libc::STDIN_FILENO,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: winch_r,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: stop_r,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    let mut buf = [0u8; 2048];
                    loop {
                        for p in &mut pfds {
                            p.revents = 0;
                        }
                        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), 3, -1) };
                        if rc < 0 {
                            let err = io::Error::last_os_error();
                            if err.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            break;
                        }
                        if pfds[2].revents != 0 {
                            break;
                        }
                        if pfds[1].revents & libc::POLLIN != 0 {
                            let mut drain = [0u8; 64];
                            while unsafe {
                                libc::read(
                                    winch_r,
                                    drain.as_mut_ptr() as *mut libc::c_void,
                                    drain.len(),
                                )
                            } > 0
                            {}
                            let (c, r) = term_size();
                            if tx.send(TuiEvent::Resize(c, r)).is_err() {
                                break;
                            }
                        }
                        if pfds[0].revents & libc::POLLIN != 0 {
                            let n = unsafe {
                                libc::read(
                                    libc::STDIN_FILENO,
                                    buf.as_mut_ptr() as *mut libc::c_void,
                                    buf.len(),
                                )
                            };
                            if n == 0 {
                                let _ = tx.send(TuiEvent::Eof);
                                break;
                            } else if n > 0
                                && tx.send(TuiEvent::Input(buf[..n as usize].to_vec())).is_err()
                            {
                                break;
                            }
                        } else if pfds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                            let _ = tx.send(TuiEvent::Eof);
                            break;
                        }
                    }
                    unsafe {
                        libc::close(winch_r);
                        libc::close(stop_r);
                    }
                })
                .ok();

            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[?1049h\x1b[?25l");
            let _ = out.flush();

            Ok(Self {
                orig,
                prev_sigwinch,
                stop_w: stop_fds[1],
                winch_w: winch_fds[1],
                input_thread,
            })
        }
    }

    impl Drop for RawTermGuard {
        fn drop(&mut self) {
            WINCH_WRITE_FD.store(-1, Ordering::Release);
            unsafe {
                let _ = libc::sigaction(libc::SIGWINCH, &self.prev_sigwinch, std::ptr::null_mut());
            }
            if self.stop_w >= 0 {
                let b: u8 = b'q';
                unsafe {
                    let _ = libc::write(self.stop_w, &b as *const u8 as *const libc::c_void, 1);
                    libc::close(self.stop_w);
                }
            }
            if let Some(h) = self.input_thread.take() {
                let _ = h.join();
            }
            if self.winch_w >= 0 {
                unsafe { libc::close(self.winch_w) };
            }
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[?25h\x1b[?1049l\x1b[0m");
            let _ = out.flush();
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.orig);
            }
        }
    }
}

#[cfg(windows)]
mod term_driver {
    use super::*;
    use std::io::Read;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    pub fn is_tty() -> bool {
        unsafe {
            let Ok(hi) = GetStdHandle(STD_INPUT_HANDLE) else {
                return false;
            };
            let Ok(ho) = GetStdHandle(STD_OUTPUT_HANDLE) else {
                return false;
            };
            let mut mi = CONSOLE_MODE(0);
            let mut mo = CONSOLE_MODE(0);
            GetConsoleMode(hi, &mut mi).is_ok() && GetConsoleMode(ho, &mut mo).is_ok()
        }
    }

    pub fn term_size() -> (u16, u16) {
        unsafe {
            if let Ok(ho) = GetStdHandle(STD_OUTPUT_HANDLE) {
                let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
                if GetConsoleScreenBufferInfo(ho, &mut info).is_ok() {
                    let c = (info.srWindow.Right - info.srWindow.Left + 1).max(20) as u16;
                    let r = (info.srWindow.Bottom - info.srWindow.Top + 1).max(6) as u16;
                    return (c, r);
                }
            }
        }
        (80, 24)
    }

    pub struct RawTermGuard {
        h_in: HANDLE,
        h_out: HANDLE,
        orig_in: CONSOLE_MODE,
        orig_out: CONSOLE_MODE,
    }

    impl RawTermGuard {
        pub fn enter(tx: Sender<TuiEvent>) -> Result<Self, String> {
            let h_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.map_err(|e| e.to_string())?;
            let h_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.map_err(|e| e.to_string())?;
            let mut orig_in = CONSOLE_MODE(0);
            let mut orig_out = CONSOLE_MODE(0);
            unsafe {
                GetConsoleMode(h_in, &mut orig_in).map_err(|e| e.to_string())?;
                GetConsoleMode(h_out, &mut orig_out).map_err(|e| e.to_string())?;
                let raw_in = CONSOLE_MODE(
                    (orig_in.0
                        & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
                        | ENABLE_VIRTUAL_TERMINAL_INPUT.0,
                );
                SetConsoleMode(h_in, raw_in).map_err(|e| e.to_string())?;
                let _ = SetConsoleMode(
                    h_out,
                    CONSOLE_MODE(orig_out.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0),
                );
            }

            let _ = std::thread::Builder::new()
                .name("tachyon-tui-win-in".into())
                .spawn(move || {
                    let mut stdin = io::stdin();
                    let mut buf = [0u8; 2048];
                    let mut last_sz = term_size();
                    loop {
                        match stdin.read(&mut buf) {
                            Ok(0) => {
                                let _ = tx.send(TuiEvent::Eof);
                                break;
                            }
                            Ok(n) => {
                                let cur = term_size();
                                if cur != last_sz {
                                    last_sz = cur;
                                    let _ = tx.send(TuiEvent::Resize(cur.0, cur.1));
                                }
                                if tx.send(TuiEvent::Input(buf[..n].to_vec())).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                let _ = tx.send(TuiEvent::Eof);
                                break;
                            }
                        }
                    }
                });

            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[?1049h\x1b[?25l");
            let _ = out.flush();

            Ok(Self {
                h_in,
                h_out,
                orig_in,
                orig_out,
            })
        }
    }

    impl Drop for RawTermGuard {
        fn drop(&mut self) {
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[?25h\x1b[?1049l\x1b[0m");
            let _ = out.flush();
            unsafe {
                let _ = SetConsoleMode(self.h_in, self.orig_in);
                let _ = SetConsoleMode(self.h_out, self.orig_out);
            }
        }
    }
}

pub fn cmd_tui(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let mut initial_block: Option<String> = None;
    let mut filter = String::new();
    let mut agents_only = false;
    let mut readonly = false;
    let mut once = false;
    let mut force_cols: Option<u16> = None;
    let mut force_rows: Option<u16> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-b" | "--block" => {
                initial_block = Some(
                    args.get(i + 1)
                        .ok_or("-b/--block requires a block id")?
                        .clone(),
                );
                i += 2;
            }
            "--filter" => {
                filter = args
                    .get(i + 1)
                    .ok_or("--filter requires a string")?
                    .clone();
                i += 2;
            }
            "-a" | "--agents-only" => {
                agents_only = true;
                i += 1;
            }
            "--readonly" => {
                readonly = true;
                i += 1;
            }
            "--once" => {
                once = true;
                i += 1;
            }
            "--cols" => {
                let v = args.get(i + 1).ok_or("--cols requires a number")?;
                force_cols = Some(v.parse::<u16>().map_err(|_| format!("invalid --cols `{v}`"))?);
                i += 2;
            }
            "--rows" => {
                let v = args.get(i + 1).ok_or("--rows requires a number")?;
                force_rows = Some(v.parse::<u16>().map_err(|_| format!("invalid --rows `{v}`"))?);
                i += 2;
            }
            other => return Err(format!("unknown tui option `{other}`")),
        }
    }

    if !once && !term_driver::is_tty() {
        return Err(
            "tachyon tui requires an interactive terminal (pass `--once` to render one snapshot)"
                .into(),
        );
    }

    let (target, client) = connect(opts, false)?;
    let snap = client.state().map_err(|e| e.to_string())?;
    let (det_cols, det_rows) = term_driver::term_size();
    let cols = force_cols.unwrap_or(det_cols);
    let rows = force_rows.unwrap_or(det_rows);

    let mut app = TuiApp::new(
        target.display_name().to_string(),
        cols,
        rows,
        readonly,
        agents_only,
        filter,
        snap,
    );
    refresh_preview(&client, &mut app);

    if once {
        let mut out = io::stdout();
        out.write_all(&app.render()).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        return Ok(0);
    }

    let (tx, rx) = mpsc::channel::<TuiEvent>();
    let _raw_guard = term_driver::RawTermGuard::enter(tx.clone())?;

    if let Ok(ev_rx) = client.subscribe_events() {
        let tx_ev = tx.clone();
        let _ = std::thread::Builder::new()
            .name("tachyon-tui-events".into())
            .spawn(move || {
                while ev_rx.recv().is_ok() {
                    if tx_ev.send(TuiEvent::ServerEvent).is_err() {
                        break;
                    }
                }
            });
    }

    let mut active_stream: Option<Arc<AttachedStream>> = None;
    let mut attach_gen = 0u64;

    if let Some(bid) = initial_block {
        begin_attach(
            &client,
            &mut app,
            &mut active_stream,
            &mut attach_gen,
            &tx,
            &bid,
        );
    }

    draw_to_stdout(&app)?;

    while let Ok(ev) = rx.recv() {
        let mut need_draw = false;
        match ev {
            TuiEvent::Eof => break,
            TuiEvent::Resize(c, r) => {
                app.cols = c.max(20);
                app.rows = r.max(6);
                if let Some(att) = &mut app.attached {
                    let inner_r = app.rows.saturating_sub(1).max(1);
                    att.term.resize(app.cols as usize, inner_r as usize);
                    if let Some(stream) = &active_stream {
                        if !app.readonly {
                            let _ = stream.resize(app.cols, inner_r, 0, 0);
                        }
                    }
                }
                app.ensure_selected_visible();
                need_draw = true;
            }
            TuiEvent::ServerEvent => {
                if let Ok(new_snap) = client.state() {
                    app.update_snapshot(new_snap);
                    if let Some(att) = &mut app.attached {
                        if let Some(b) = app.snapshot.blocks.iter().find(|b| b.id == att.block_id) {
                            if !b.alive {
                                att.exited = Some(b.exit_code.unwrap_or(0));
                                need_draw = true;
                            }
                        }
                    } else {
                        refresh_preview(&client, &mut app);
                        need_draw = true;
                    }
                }
            }
            TuiEvent::Stream(g, chunk) => {
                if g == attach_gen {
                    if let Some(att) = &mut app.attached {
                        let bytes = match chunk {
                            StreamChunk::Snapshot(b) | StreamChunk::Output(b) => b,
                        };
                        att.processor.advance(&mut att.term, &bytes);
                        let replies = att.term.take_replies();
                        if !replies.is_empty() && !app.readonly {
                            if let Some(stream) = &active_stream {
                                let _ = stream.write(&replies);
                            }
                        }
                        need_draw = true;
                    }
                }
            }
            TuiEvent::StreamClosed(g) => {
                if g == attach_gen {
                    if let Some(att) = &mut app.attached {
                        if att.exited.is_none() {
                            let code = app
                                .snapshot
                                .blocks
                                .iter()
                                .find(|b| b.id == att.block_id)
                                .and_then(|b| b.exit_code)
                                .unwrap_or(0);
                            att.exited = Some(code);
                            need_draw = true;
                        }
                    }
                }
            }
            TuiEvent::Input(bytes) => {
                if app.attached.is_some() {
                    // `Ctrl-]` (`0x1D`) detaches instantly back to List Mode.
                    if let Some(pos) = bytes.iter().position(|&b| b == 0x1D) {
                        if !app.readonly && pos > 0 {
                            if let Some(stream) = &active_stream {
                                let _ = stream.write(&bytes[..pos]);
                            }
                        }
                        if let Some(stream) = active_stream.take() {
                            stream.detach();
                        }
                        app.attached = None;
                        if let Ok(new_snap) = client.state() {
                            app.update_snapshot(new_snap);
                        }
                        refresh_preview(&client, &mut app);
                        if pos + 1 < bytes.len()
                            && handle_list_bytes(&client, &mut app, &mut active_stream, &mut attach_gen, &tx, &bytes[pos + 1..])
                        {
                            break;
                        }
                        need_draw = true;
                    } else if !app.readonly {
                        if let Some(stream) = &active_stream {
                            let _ = stream.write(&bytes);
                        }
                    }
                } else {
                    if handle_list_bytes(
                        &client,
                        &mut app,
                        &mut active_stream,
                        &mut attach_gen,
                        &tx,
                        &bytes,
                    ) {
                        break;
                    }
                    need_draw = true;
                }
            }
        }
        if need_draw {
            draw_to_stdout(&app)?;
        }
    }

    if let Some(stream) = active_stream.take() {
        stream.detach();
    }
    Ok(0)
}

fn draw_to_stdout(app: &TuiApp) -> Result<(), String> {
    let mut out = io::stdout();
    out.write_all(&app.render()).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn refresh_preview(client: &Arc<MuxClient>, app: &mut TuiApp) {
    if app.layout_mode() != LayoutMode::Wide {
        return;
    }
    let Some(bid) = app.selected_item().map(|it| it.block_id.clone()) else {
        app.preview_block = None;
        app.preview_lines.clear();
        return;
    };
    app.preview_block = Some(bid.clone());
    match client.capture(&bid, Some(40), false) {
        Ok(text) => {
            app.preview_lines = text.lines().map(str::to_string).collect();
        }
        Err(_) => {
            app.preview_lines.clear();
        }
    }
}

fn begin_attach(
    client: &Arc<MuxClient>,
    app: &mut TuiApp,
    active_stream: &mut Option<Arc<AttachedStream>>,
    attach_gen: &mut u64,
    tx: &Sender<TuiEvent>,
    block_id: &str,
) {
    if let Some(old) = active_stream.take() {
        old.detach();
    }
    match client.attach(block_id, !app.readonly, true) {
        Ok(stream) => {
            let stream = Arc::new(stream);
            let inner_rows = app.rows.saturating_sub(1).max(1);
            if !app.readonly {
                let _ = stream.resize(app.cols, inner_rows, 0, 0);
            }
            *attach_gen = attach_gen.wrapping_add(1);
            let g = *attach_gen;
            let title = app
                .snapshot
                .blocks
                .iter()
                .find(|b| b.id == block_id)
                .map(BlockInfo::display_name)
                .unwrap_or_else(|| block_id.to_string());
            let mut att = AttachedView::new(block_id.to_string(), title, app.cols, app.rows);
            if !stream.alive {
                att.exited = Some(0);
            }
            app.attached = Some(att);

            let rx_stream = Arc::clone(&stream);
            let tx_stream = tx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("tachyon-tui-stream-{block_id}"))
                .spawn(move || {
                    while let Ok(chunk) = rx_stream.recv() {
                        if tx_stream.send(TuiEvent::Stream(g, chunk)).is_err() {
                            return;
                        }
                    }
                    let _ = tx_stream.send(TuiEvent::StreamClosed(g));
                });
            *active_stream = Some(stream);
        }
        Err(e) => {
            app.toast = Some(format!("attach `{block_id}` failed: {e}"));
        }
    }
}

/// Returns `true` when the user requested quit.
fn handle_list_bytes(
    client: &Arc<MuxClient>,
    app: &mut TuiApp,
    active_stream: &mut Option<Arc<AttachedStream>>,
    attach_gen: &mut u64,
    tx: &Sender<TuiEvent>,
    bytes: &[u8],
) -> bool {
    for key in decode_list_keys(bytes) {
        app.toast = None;

        if let Some(bid) = app.confirm_kill.take() {
            if matches!(key, TuiKey::Char('y') | TuiKey::Char('Y')) {
                match client.kill_block(&bid) {
                    Ok(()) => {
                        if let Ok(snap) = client.state() {
                            app.update_snapshot(snap);
                        }
                        app.toast = Some(format!("killed `{bid}`"));
                        refresh_preview(client, app);
                    }
                    Err(e) => {
                        app.toast = Some(format!("kill `{bid}` failed: {e}"));
                    }
                }
            }
            continue;
        }

        if app.filtering {
            match key {
                TuiKey::Enter => {
                    app.filtering = false;
                }
                TuiKey::Esc => {
                    app.filtering = false;
                    app.filter.clear();
                    app.rebuild_items();
                    refresh_preview(client, app);
                }
                TuiKey::Backspace => {
                    app.filter.pop();
                    app.rebuild_items();
                    refresh_preview(client, app);
                }
                TuiKey::Char(ch) if !ch.is_control() => {
                    app.filter.push(ch);
                    app.rebuild_items();
                    refresh_preview(client, app);
                }
                _ => {}
            }
            continue;
        }

        match key {
            TuiKey::Char('q') | TuiKey::Ctrl('c') => return true,
            TuiKey::Esc => {
                if !app.filter.is_empty() {
                    app.filter.clear();
                    app.rebuild_items();
                    refresh_preview(client, app);
                }
            }
            TuiKey::Char('j') | TuiKey::Down => {
                app.move_selection(1);
                refresh_preview(client, app);
            }
            TuiKey::Char('k') | TuiKey::Up => {
                app.move_selection(-1);
                refresh_preview(client, app);
            }
            TuiKey::Char('g') | TuiKey::Home => {
                app.jump_first();
                refresh_preview(client, app);
            }
            TuiKey::Char('G') | TuiKey::End => {
                app.jump_last();
                refresh_preview(client, app);
            }
            TuiKey::PageDown | TuiKey::Ctrl('d') => {
                let step = (app.body_height() / 2).max(1) as isize;
                app.move_selection(step);
                refresh_preview(client, app);
            }
            TuiKey::PageUp | TuiKey::Ctrl('u') => {
                let step = (app.body_height() / 2).max(1) as isize;
                app.move_selection(-step);
                refresh_preview(client, app);
            }
            TuiKey::Char('!') => {
                if !app.jump_next_blocked() {
                    app.toast = Some("no blocked or errored agents".into());
                } else {
                    refresh_preview(client, app);
                }
            }
            TuiKey::Char('a') => {
                app.agents_only = !app.agents_only;
                app.rebuild_items();
                refresh_preview(client, app);
            }
            TuiKey::Char('/') => {
                app.filtering = true;
            }
            TuiKey::Enter | TuiKey::Char('l') | TuiKey::Right => {
                if let Some(bid) = app.selected_item().map(|it| it.block_id.clone()) {
                    begin_attach(client, app, active_stream, attach_gen, tx, &bid);
                }
            }
            TuiKey::Char('n') => {
                let target_sess = app.selected_item().map(|it| it.session_id.clone());
                let res = match target_sess {
                    Some(sid) => client.create_window(&sid, None, None, None, None, 80, 24).map(|(w, _)| w.id),
                    None => client.create_session(None, None, None, None, 80, 24).map(|(_, w, _)| w.id),
                };
                match res {
                    Ok(_) => {
                        if let Ok(snap) = client.state() {
                            app.update_snapshot(snap);
                        }
                        refresh_preview(client, app);
                    }
                    Err(e) => app.toast = Some(format!("new pane failed: {e}")),
                }
            }
            TuiKey::Char('x') | TuiKey::Char('d') => {
                if let Some(bid) = app.selected_item().map(|it| it.block_id.clone()) {
                    app.confirm_kill = Some(bid);
                }
            }
            TuiKey::Char('r') => {
                if let Ok(snap) = client.state() {
                    app.update_snapshot(snap);
                    refresh_preview(client, app);
                    app.toast = Some("refreshed".into());
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::model::{HostInfo, LayoutNode, SessionInfo, WindowInfo};

    fn sample_snapshot() -> ServerSnapshot {
        ServerSnapshot {
            host: HostInfo {
                hostname: "tsukuyomi".into(),
                os: "macos".into(),
                arch: "aarch64".into(),
                version: "0.1.0".into(),
                protocol: 2,
                pid: 123,
                build: None,
                capabilities: Vec::new(),
                auth_enabled: None,
                listen_addr: None,
            },
            sessions: vec![
                SessionInfo {
                    id: "s_1".into(),
                    name: "backend-api".into(),
                    windows: vec!["w_1".into()],
                    active_window: Some("w_1".into()),
                    created_at: 1,
                },
                SessionInfo {
                    id: "s_2".into(),
                    name: "plain-shell".into(),
                    windows: vec!["w_2".into()],
                    active_window: Some("w_2".into()),
                    created_at: 2,
                },
            ],
            windows: vec![
                WindowInfo {
                    id: "w_1".into(),
                    session_id: "s_1".into(),
                    name: Some("claude-pane".into()),
                    layout: LayoutNode::Leaf("b_1".into()),
                    focused_block: "b_1".into(),
                    zoomed: false,
                    created_at: 1,
                },
                WindowInfo {
                    id: "w_2".into(),
                    session_id: "s_2".into(),
                    name: Some("zsh".into()),
                    layout: LayoutNode::Leaf("b_2".into()),
                    focused_block: "b_2".into(),
                    zoomed: false,
                    created_at: 2,
                },
            ],
            blocks: vec![
                BlockInfo {
                    id: "b_1".into(),
                    session_id: "s_1".into(),
                    window_id: "w_1".into(),
                    label: Some("claude".into()),
                    title: None,
                    cwd: None,
                    command: Some("claude".into()),
                    command_running: true,
                    pid: 1001,
                    cols: 80,
                    rows: 24,
                    alive: true,
                    exit_code: None,
                    awake: false,
                    attached_clients: 0,
                    created_at: 1,
                    status: Some(ProgramStatus {
                        state: ProgramState::Blocked,
                        app: Some("claude".into()),
                        kind: None,
                        msg: Some("Approve migration".into()),
                        label: None,
                    }),
                    status_inferred: false,
                    last_receipt: None,
                },
                BlockInfo {
                    id: "b_2".into(),
                    session_id: "s_2".into(),
                    window_id: "w_2".into(),
                    label: Some("logs".into()),
                    title: None,
                    cwd: None,
                    command: Some("zsh".into()),
                    command_running: false,
                    pid: 1002,
                    cols: 80,
                    rows: 24,
                    alive: true,
                    exit_code: None,
                    awake: false,
                    attached_clients: 0,
                    created_at: 2,
                    status: None,
                    status_inferred: false,
                    last_receipt: None,
                },
            ],
            clients: Vec::new(),
        }
    }

    fn strip_ansi_visible_lines(raw: &[u8]) -> Vec<String> {
        let s = String::from_utf8_lossy(raw);
        let mut lines = Vec::new();
        let mut cur = String::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if ('@'..='~').contains(&c) {
                            if c == 'H' && !cur.is_empty() {
                                lines.push(std::mem::take(&mut cur));
                            }
                            break;
                        }
                    }
                }
                continue;
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    #[test]
    fn narrow_and_wide_layouts_render_cleanly_within_column_budget() {
        let mut app = TuiApp::new(
            "local".into(),
            44,
            16,
            false,
            false,
            String::new(),
            sample_snapshot(),
        );
        assert_eq!(app.layout_mode(), LayoutMode::Narrow);
        assert_eq!(app.selected_item().unwrap().block_id, "b_1");

        let narrow_out = String::from_utf8(app.render()).unwrap();
        assert!(!narrow_out.contains('│'));
        assert!(narrow_out.contains("LIVE AGENTS (1)"));
        assert!(narrow_out.contains("SPACES & PANES (2)"));
        for line in strip_ansi_visible_lines(narrow_out.as_bytes()) {
            assert!(
                display_width(&line) <= 44,
                "narrow line exceeded 44 cols ({}): {line:?}",
                display_width(&line)
            );
        }

        // Switch to Wide layout with live preview lines.
        app.cols = 120;
        app.rows = 30;
        app.preview_block = Some("b_1".into());
        app.preview_lines = vec!["Migration pending approval...".into()];
        assert_eq!(app.layout_mode(), LayoutMode::Wide);
        let wide_out = String::from_utf8(app.render()).unwrap();
        assert!(wide_out.contains('│'));
        assert!(wide_out.contains("Approve migration"));
        assert!(wide_out.contains("Migration pending approval..."));
        for line in strip_ansi_visible_lines(wide_out.as_bytes()) {
            assert!(
                display_width(&line) <= 120,
                "wide line exceeded 120 cols ({}): {line:?}",
                display_width(&line)
            );
        }

        // Agents-only + filter excludes non-agent `b_2`.
        app.agents_only = true;
        app.rebuild_items();
        let ag_out = String::from_utf8(app.render()).unwrap();
        assert!(!ag_out.contains("SPACES & PANES"));
        assert!(ag_out.contains("[AGENTS]"));
    }

    #[test]
    fn attached_term_renderer_preserves_colors_and_status_bar() {
        let mut app = TuiApp::new(
            "kagutsuchi".into(),
            60,
            12,
            true,
            false,
            String::new(),
            sample_snapshot(),
        );
        let mut att = AttachedView::new("b_1".into(), "claude".into(), 60, 12);
        att.processor
            .advance(&mut att.term, b"\x1b[38;2;245;158;11mhello\x1b[0m world");
        app.attached = Some(att);

        let frame = String::from_utf8(app.render()).unwrap();
        assert!(frame.contains("38;2;245;158;11"));
        assert!(frame.contains("hello"));
        assert!(frame.contains("world"));
        assert!(frame.contains("^] list · kagutsuchi · claude · RO"));
    }

    #[test]
    fn list_key_decoder_parses_arrows_ctrl_and_utf8() {
        let keys = decode_list_keys(b"\x1b[A\x1b[B\x1b[5~\x03\x1d/foo\r");
        assert_eq!(
            keys,
            vec![
                TuiKey::Up,
                TuiKey::Down,
                TuiKey::PageUp,
                TuiKey::Ctrl('c'),
                TuiKey::Ctrl(']'),
                TuiKey::Char('/'),
                TuiKey::Char('f'),
                TuiKey::Char('o'),
                TuiKey::Char('o'),
                TuiKey::Enter,
            ]
        );
    }
}
