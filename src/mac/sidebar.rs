//! Herdr-inspired collapsible two-zone left sidebar (`Sidebar`).
//!
//! Vertical zones:
//! - Top toolbar: Sidebar toggle, Hosts menu, Command Palette (`⌘⇧P`), New Session (`+`).
//! - Compact search filter + `✦` Live Agents quick filter.
//! - Zone 1 (Spaces & Hosts): `Host -> Session -> Space/Window` rows with
//!   `18×18` monogram avatars, right-aligned git branch & amber blocked badges.
//! - Zone 2 (Live Agents): hairline-separated, project-grouped live coding
//!   agents with animated spinner (`Working`), blocked status, and elapsed timer.
//! - Clean 2-row utility footer: **Settings** (`⌘,`) with server health dot
//!   and **Shortcuts** (`⌘/`).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSButton, NSColor, NSControl,
    NSControlTextEditingDelegate, NSCursor, NSEvent, NSFont, NSFontAttributeName,
    NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSImage, NSImageSymbolConfiguration, NSLineBreakMode, NSMenu,
    NSMenuItem, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder, NSSearchField,
    NSSearchFieldDelegate, NSTextAlignment, NSTextFieldDelegate, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer,
};
use parking_lot::Mutex;

use super::host_mgr::{HostStatus, HostView};
use super::renderer::chrome_tokens;
use super::{machines, with_window};
use crate::config::ChromeStyle;
use crate::mux::model::{
    format_elapsed, git_info_for_dir, group_agents_by_project, space_monogram, AgentCounts,
    AgentEntry, AgentFilter, GitDirInfo, HostColor,
};
use crate::term::{ProgramState, ProgramStatus};

pub const MIN_WIDTH: f64 = 180.0;
pub const MAX_WIDTH: f64 = 360.0;
const TOP_BAR_H: f64 = 38.0;
const FILTER_BAR_H: f64 = 32.0;
const UTILITY_FOOTER_H: f64 = 58.0;
const AGENTS_HEADER_H: f64 = 26.0;
const EMPTY_AGENTS_H: f64 = 50.0;
const RESIZE_GRIP_W: f64 = 6.0;

struct GitDirtyState {
    entries: HashMap<PathBuf, (Instant, bool)>,
    in_flight: HashSet<PathBuf>,
}

struct GitDirtyProbe {
    state: Mutex<GitDirtyState>,
    tx: mpsc::Sender<(PathBuf, u64)>,
}

fn git_dirty_probe() -> &'static GitDirtyProbe {
    static PROBE: OnceLock<GitDirtyProbe> = OnceLock::new();
    PROBE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<(PathBuf, u64)>();
        let probe = GitDirtyProbe {
            state: Mutex::new(GitDirtyState {
                entries: HashMap::new(),
                in_flight: HashSet::new(),
            }),
            tx,
        };
        let _ = std::thread::Builder::new()
            .name("tachyon-git-probe".into())
            .spawn(move || {
                while let Ok((root, win_id)) = rx.recv() {
                    let dirty = crate::mux::worktree::is_worktree_dirty(&root).unwrap_or(false);
                    let p = git_dirty_probe();
                    let changed = {
                        let mut st = p.state.lock();
                        st.in_flight.remove(&root);
                        let prev = st.entries.get(&root).map(|(_, d)| *d);
                        if st.entries.len() > 128 {
                            let now = Instant::now();
                            st.entries.retain(|_, (ts, _)| {
                                now.saturating_duration_since(*ts) < Duration::from_secs(60)
                            });
                        }
                        st.entries.insert(root, (Instant::now(), dirty));
                        prev.map_or(dirty, |old| old != dirty)
                    };
                    if changed && win_id != 0 {
                        DispatchQueue::main().exec_async(move || {
                            super::refresh_window_sidebar(win_id);
                        });
                    }
                }
            });
        probe
    })
}

/// Non-blocking cache lookup + background probe for dirty worktrees (zero idle wake-ups).
pub fn query_git_dirty(worktree_root: &Path, win_id: u64) -> bool {
    const TTL: Duration = Duration::from_secs(5);
    let probe = git_dirty_probe();
    let now = Instant::now();
    let (cached, should_send) = {
        let mut st = probe.state.lock();
        let (cached, fresh) = match st.entries.get(worktree_root) {
            Some((ts, d)) => (*d, now.saturating_duration_since(*ts) < TTL),
            None => (false, false),
        };
        let send = if !fresh && !st.in_flight.contains(worktree_root) {
            st.in_flight.insert(worktree_root.to_path_buf());
            true
        } else {
            false
        };
        (cached, send)
    };
    if should_send {
        let _ = probe.tx.send((worktree_root.to_path_buf(), win_id));
    }
    cached
}

const BRAILLE_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Pure vertical split calculation between Zone 1 (Spaces) and Zone 2 (Live Agents).
fn compute_agents_zone_height(mid_h: f64, agents_content_h: f64, has_agents: bool) -> f64 {
    if mid_h < 96.0 {
        return 0.0;
    }
    if !has_agents {
        return EMPTY_AGENTS_H.min(mid_h * 0.35);
    }
    let desired = (AGENTS_HEADER_H + agents_content_h + 6.0).max(62.0);
    let max_h = (mid_h * 0.44).max(96.0).min((mid_h - 60.0).max(50.0));
    desired.min(max_h)
}

#[derive(Clone, Debug)]
enum TreeRow {
    Host {
        alias: String,
        display_name: String,
        is_local: bool,
        os: Option<String>,
        color_spec: Option<String>,
        color: Option<HostColor>,
        connected: bool,
        connecting: bool,
        rtt_ms: Option<f64>,
        top_pad: f64,
    },
    Session {
        host: String,
        session_id: String,
        name: String,
        cwd: Option<String>,
        is_worktree: bool,
    },
    Window {
        host: String,
        session_id: String,
        window_id: String,
        title: String,
        pane_count: usize,
        running: bool,
        active: bool,
        status: Option<ProgramStatus>,
        host_color: Option<HostColor>,
        git_branch: Option<String>,
        git_dirty: bool,
        is_worktree: bool,
        cwd: Option<String>,
        blocked_count: usize,
        error_count: usize,
    },
    Note {
        host: String,
        text: String,
        reconnectable: bool,
    },
}

impl TreeRow {
    fn height(&self) -> f64 {
        match self {
            Self::Host { top_pad, .. } => 26.0 + *top_pad,
            Self::Session { .. } => 20.0,
            Self::Window { .. } => 30.0,
            Self::Note { .. } => 22.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowAction {
    Main,
    Add,
    Close,
}

#[derive(Default)]
struct TreeState {
    window: Cell<u64>,
    hosts: RefCell<Vec<HostView>>,
    filter: RefCell<String>,
    agents_only: Cell<bool>,
    active_target: RefCell<Option<(String, String)>>, // (host, window_id)
    rows: RefCell<Vec<(f64, f64, TreeRow)>>,          // (y_top, height, row) in flipped coords
    git_cache: RefCell<HashMap<String, (Instant, Option<GitDirInfo>)>>,
    content_h: Cell<f64>,
    scroll_y: Cell<f64>,
    hover: Cell<Option<(usize, RowAction)>>,
    dragging_resize: Cell<bool>,
    chrome: Cell<ChromeStyle>,
}

define_class!(
    /// Zone 1: Custom-drawn scrollable Host -> Session -> Space tree inside the sidebar.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSidebarTreeView"]
    #[ivars = TreeState]
    struct SidebarTreeView;

    impl SidebarTreeView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let dy = if event.hasPreciseScrollingDeltas() {
                event.scrollingDeltaY()
            } else {
                event.scrollingDeltaY() * 14.0
            };
            let max_scroll = (self.ivars().content_h.get() - self.bounds().size.height).max(0.0);
            let next = (self.ivars().scroll_y.get() - dy).clamp(0.0, max_scroll);
            if (next - self.ivars().scroll_y.get()).abs() > 0.1 {
                self.ivars().scroll_y.set(next);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.in_resize_grip(p.x) {
                set_resize_cursor();
                if self.ivars().hover.get().is_some() {
                    self.ivars().hover.set(None);
                    self.setNeedsDisplay(true);
                }
                return;
            }
            NSCursor::arrowCursor().set();
            let hit = self.hit_row(p);
            if self.ivars().hover.get() != hit {
                self.ivars().hover.set(hit);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            NSCursor::arrowCursor().set();
            if self.ivars().hover.get().is_some() {
                self.ivars().hover.set(None);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.in_resize_grip(p.x) {
                self.ivars().dragging_resize.set(true);
                return;
            }
            let Some((idx, action)) = self.hit_row(p) else { return };
            let row = self.ivars().rows.borrow().get(idx).map(|(_, _, r)| r.clone());
            let Some(row) = row else { return };
            let win_id = self.ivars().window.get();
            match (row, action) {
                (TreeRow::Host { alias, connected, .. }, RowAction::Add) => {
                    if connected {
                        super::mux_new_session(win_id, &alias);
                    } else {
                        super::mux_reconnect_host(&alias);
                    }
                }
                (TreeRow::Host { alias, connected, .. }, _) => {
                    if !connected {
                        super::mux_reconnect_host(&alias);
                    }
                }
                (TreeRow::Session { host, session_id, .. }, RowAction::Add) => {
                    super::mux_new_window(win_id, &host, &session_id);
                }
                (TreeRow::Session { host, session_id, .. }, RowAction::Close) => {
                    super::mux_kill_session(win_id, &host, &session_id);
                }
                (TreeRow::Window { host, window_id, .. }, RowAction::Close) => {
                    super::mux_kill_window(win_id, &host, &window_id);
                }
                (TreeRow::Window { host, window_id, .. }, _) => {
                    super::mux_select_window(win_id, &host, &window_id);
                }
                (TreeRow::Note { host, reconnectable, .. }, _) => {
                    if reconnectable {
                        super::mux_reconnect_host(&host);
                    } else {
                        super::mux_new_session(win_id, &host);
                    }
                }
                _ => {}
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if !self.ivars().dragging_resize.get() {
                return;
            }
            drag_sidebar_width(self.ivars().window.get(), event);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().dragging_resize.set(false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let Some((idx, _)) = self.hit_row(p) else { return };
            let row = self.ivars().rows.borrow().get(idx).map(|(_, _, r)| r.clone());
            let Some(row) = row else { return };
            let Some(mtm) = MainThreadMarker::new() else { return };
            self.show_context_menu(mtm, event, &row);
        }

        #[unsafe(method(ctxNewSession:))]
        fn ctx_new_session(&self, sender: Option<&AnyObject>) {
            if let Some(host) = rep_string(sender) {
                super::mux_new_session(self.ivars().window.get(), &host);
            }
        }

        #[unsafe(method(ctxReconnectHost:))]
        fn ctx_reconnect_host(&self, sender: Option<&AnyObject>) {
            if let Some(host) = rep_string(sender) {
                super::mux_reconnect_host(&host);
            }
        }

        #[unsafe(method(ctxRemoveHost:))]
        fn ctx_remove_host(&self, sender: Option<&AnyObject>) {
            if let Some(host) = rep_string(sender) {
                super::mux_remove_host(&host);
            }
        }

        #[unsafe(method(ctxSetHostColor:))]
        fn ctx_set_host_color(&self, sender: Option<&AnyObject>) {
            if let Some((host, spec)) = rep_pair(sender) {
                let color = (!spec.eq_ignore_ascii_case("auto")).then_some(spec);
                super::mux_set_host_color(&host, color);
            }
        }

        #[unsafe(method(ctxNewWindow:))]
        fn ctx_new_window(&self, sender: Option<&AnyObject>) {
            if let Some((host, sid)) = rep_pair(sender) {
                super::mux_new_window(self.ivars().window.get(), &host, &sid);
            }
        }

        #[unsafe(method(ctxKillSession:))]
        fn ctx_kill_session(&self, sender: Option<&AnyObject>) {
            if let Some((host, sid)) = rep_pair(sender) {
                super::mux_kill_session(self.ivars().window.get(), &host, &sid);
            }
        }

        #[unsafe(method(ctxOpenWindow:))]
        fn ctx_open_window(&self, sender: Option<&AnyObject>) {
            if let Some((host, wid)) = rep_pair(sender) {
                super::mux_select_window(self.ivars().window.get(), &host, &wid);
            }
        }

        #[unsafe(method(ctxKillWindow:))]
        fn ctx_kill_window(&self, sender: Option<&AnyObject>) {
            if let Some((host, wid)) = rep_pair(sender) {
                super::mux_kill_window(self.ivars().window.get(), &host, &wid);
            }
        }

        #[unsafe(method(ctxNewWorktree:))]
        fn ctx_new_worktree(&self, sender: Option<&AnyObject>) {
            if let Some(cwd) = rep_string(sender) {
                super::mux_prompt_new_worktree(self.ivars().window.get(), Some(cwd));
            }
        }

        #[unsafe(method(ctxRemoveWorktree:))]
        fn ctx_remove_worktree(&self, sender: Option<&AnyObject>) {
            if let Some(raw) = rep_string(sender) {
                let mut parts = raw.splitn(3, '\u{1F}');
                if let (Some(host), Some(sid), Some(cwd)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    super::mux_prompt_remove_worktree(
                        self.ivars().window.get(),
                        host.to_string(),
                        sid.to_string(),
                        cwd.to_string(),
                    );
                }
            }
        }
    }
);

impl SidebarTreeView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = TreeState::default();
        state.window.set(window);
        let this = Self::alloc(mtm).set_ivars(state);
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setClipsToBounds(true);
        install_tracking_area(&view);
        view
    }

    fn in_resize_grip(&self, x: f64) -> bool {
        let w = self.bounds().size.width;
        x >= (w - RESIZE_GRIP_W).max(0.0)
    }

    fn resolve_local_git_info(&self, cwd: &str) -> Option<GitDirInfo> {
        let now = Instant::now();
        let mut cache = self.ivars().git_cache.borrow_mut();
        if let Some((ts, cached)) = cache.get(cwd) {
            if now.duration_since(*ts) < Duration::from_secs(3) {
                return cached.clone();
            }
        }
        let info = git_info_for_dir(Path::new(cwd));
        if cache.len() > 64 {
            cache.retain(|_, (ts, _)| now.saturating_duration_since(*ts) < Duration::from_secs(30));
        }
        cache.insert(cwd.to_string(), (now, info.clone()));
        info
    }

    fn rebuild_rows(&self) {
        let hosts = self.ivars().hosts.borrow();
        let filter = self.ivars().filter.borrow().trim().to_ascii_lowercase();
        let active = self.ivars().active_target.borrow().clone();
        let win_id = self.ivars().window.get();

        let mut rows = Vec::new();
        let mut y = 6.0;

        let cfg_colors = crate::config::Config::load_default().host_colors;
        for (h_idx, h) in hosts.iter().enumerate() {
            let (connected, connecting, rtt_ms, os, error) = match &h.status {
                HostStatus::Connected { info, rtt_ms } => (true, false, *rtt_ms, Some(info.os.clone()), None),
                HostStatus::Connecting => (false, true, None, None, None),
                HostStatus::Disconnected { error } => (false, false, None, None, Some(error.clone())),
            };
            let resolved_color = if h.is_local {
                None
            } else {
                crate::mux::model::resolve_host_color(
                    &h.alias,
                    os.as_deref(),
                    h.color.as_deref(),
                    &cfg_colors,
                )
            };

            let host_row = TreeRow::Host {
                alias: h.alias.clone(),
                display_name: h.display_name.clone(),
                is_local: h.is_local,
                os,
                color_spec: h.color.clone(),
                color: resolved_color,
                connected,
                connecting,
                rtt_ms,
                top_pad: if h_idx == 0 { 0.0 } else { 10.0 },
            };
            let hh = host_row.height();
            rows.push((y, hh, host_row));
            y += hh;

            if let Some(snap) = &h.snapshot {
                let agents_only = self.ivars().agents_only.get();
                struct BuiltSession {
                    orig_idx: usize,
                    common_root: Option<PathBuf>,
                    is_worktree: bool,
                    branch: String,
                    srow: TreeRow,
                    win_rows: Vec<TreeRow>,
                }
                let mut built_sessions: Vec<BuiltSession> = Vec::new();
                let mut first_repo_idx: HashMap<PathBuf, usize> = HashMap::new();

                for (s_idx, sess) in snap.sessions.iter().enumerate() {
                    let sess_matches = filter.is_empty()
                        || sess.name.to_ascii_lowercase().contains(&filter)
                        || h.display_name.to_ascii_lowercase().contains(&filter)
                        || h.alias.to_ascii_lowercase().contains(&filter);

                    let mut win_rows = Vec::new();
                    let mut sess_git: Option<GitDirInfo> = None;
                    let mut sess_cwd: Option<String> = None;

                    for wid in &sess.windows {
                        let Some(win) = snap.windows.iter().find(|w| &w.id == wid) else {
                            continue;
                        };
                        let leaves = win.layout.leaves();
                        let focused = snap.blocks.iter().find(|b| b.id == win.focused_block);
                        let title = win
                            .name
                            .clone()
                            .filter(|s| !s.is_empty())
                            .or_else(|| focused.map(|b| b.display_name()))
                            .unwrap_or_else(|| "shell".to_string());
                        let running = snap
                            .blocks
                            .iter()
                            .any(|b| leaves.contains(&b.id) && b.command_running);
                        let status = pick_window_status(
                            snap.blocks
                                .iter()
                                .filter(|b| leaves.contains(&b.id))
                                .filter_map(|b| b.status.as_ref()),
                        );
                        let blocked_count = snap
                            .blocks
                            .iter()
                            .filter(|b| leaves.contains(&b.id))
                            .filter(|b| b.status.as_ref().is_some_and(|s| s.state == ProgramState::Blocked))
                            .count();
                        let error_count = snap
                            .blocks
                            .iter()
                            .filter(|b| leaves.contains(&b.id))
                            .filter(|b| b.status.as_ref().is_some_and(|s| s.state == ProgramState::Error))
                            .count();
                        let has_agent = status.is_some()
                            || snap.blocks.iter().any(|b| {
                                leaves.contains(&b.id)
                                    && crate::mux::model::infer_agent_from_block(b).is_some()
                            });
                        if agents_only && !has_agent {
                            continue;
                        }

                        let cwd = focused
                            .and_then(|b| b.cwd.as_deref())
                            .filter(|s| !s.is_empty())
                            .or_else(|| {
                                leaves.iter().find_map(|bid| {
                                    snap.blocks
                                        .iter()
                                        .find(|b| &b.id == bid)
                                        .and_then(|b| b.cwd.as_deref())
                                        .filter(|s| !s.is_empty())
                                })
                            });
                        if sess_cwd.is_none() {
                            sess_cwd = cwd.map(str::to_string);
                        }

                        let git_info = if h.is_local {
                            cwd.and_then(|dir| self.resolve_local_git_info(dir))
                        } else {
                            None
                        };
                        if sess_git.is_none() {
                            sess_git = git_info.clone();
                        }

                        let (git_branch, git_dirty, is_worktree, git_repo) = match git_info {
                            Some(info) => {
                                let dirty = query_git_dirty(&info.worktree_root, win_id);
                                (
                                    Some(info.branch),
                                    dirty,
                                    info.is_linked_worktree,
                                    Some(info.repo_name),
                                )
                            }
                            None => (None, false, false, None),
                        };

                        let branch_matches = git_branch
                            .as_deref()
                            .is_some_and(|br| br.to_ascii_lowercase().contains(&filter))
                            || git_repo
                                .as_deref()
                                .is_some_and(|r| r.to_ascii_lowercase().contains(&filter))
                            || (is_worktree && "worktree".contains(&filter));
                        if !sess_matches
                            && !title.to_ascii_lowercase().contains(&filter)
                            && !branch_matches
                        {
                            continue;
                        }
                        let is_active = active
                            .as_ref()
                            .is_some_and(|(ah, aw)| ah == &h.alias && aw == &win.id);
                        win_rows.push(TreeRow::Window {
                            host: h.alias.clone(),
                            session_id: sess.id.clone(),
                            window_id: win.id.clone(),
                            title,
                            pane_count: leaves.len(),
                            running,
                            active: is_active,
                            status,
                            host_color: resolved_color,
                            git_branch,
                            git_dirty,
                            is_worktree,
                            cwd: cwd.map(str::to_string),
                            blocked_count,
                            error_count,
                        });
                    }

                    if !win_rows.is_empty() || (!agents_only && sess_matches && sess.windows.is_empty()) {
                        let (common_root, is_worktree, branch) = match sess_git {
                            Some(g) => {
                                first_repo_idx.entry(g.common_repo_root.clone()).or_insert(s_idx);
                                (Some(g.common_repo_root), g.is_linked_worktree, g.branch)
                            }
                            None => (None, false, String::new()),
                        };
                        built_sessions.push(BuiltSession {
                            orig_idx: s_idx,
                            common_root,
                            is_worktree,
                            branch,
                            srow: TreeRow::Session {
                                host: h.alias.clone(),
                                session_id: sess.id.clone(),
                                name: sess.name.clone(),
                                cwd: sess_cwd,
                                is_worktree,
                            },
                            win_rows,
                        });
                    }
                }

                if h.is_local {
                    built_sessions.sort_by(|a, b| {
                        let ga = a
                            .common_root
                            .as_ref()
                            .and_then(|r| first_repo_idx.get(r).copied())
                            .unwrap_or(a.orig_idx);
                        let gb = b
                            .common_root
                            .as_ref()
                            .and_then(|r| first_repo_idx.get(r).copied())
                            .unwrap_or(b.orig_idx);
                        ga.cmp(&gb)
                            .then_with(|| a.is_worktree.cmp(&b.is_worktree))
                            .then_with(|| {
                                a.branch
                                    .to_ascii_lowercase()
                                    .cmp(&b.branch.to_ascii_lowercase())
                            })
                            .then_with(|| a.orig_idx.cmp(&b.orig_idx))
                    });
                }

                let shown_any = !built_sessions.is_empty();
                for item in built_sessions {
                    let sh = item.srow.height();
                    rows.push((y, sh, item.srow));
                    y += sh;
                    for wr in item.win_rows {
                        let wh = wr.height();
                        rows.push((y, wh, wr));
                        y += wh;
                    }
                }
                if !shown_any && filter.is_empty() {
                    let note = TreeRow::Note {
                        host: h.alias.clone(),
                        text: if agents_only {
                            "No active agents".into()
                        } else if connected {
                            "No sessions — click + to start".into()
                        } else {
                            "Offline — click to reconnect".into()
                        },
                        reconnectable: !connected && !agents_only,
                    };
                    let nh = note.height();
                    rows.push((y, nh, note));
                    y += nh;
                }
            } else {
                let text = if connecting {
                    "Connecting…".to_string()
                } else if let Some(err) = error {
                    let short = err.lines().next().unwrap_or("Offline");
                    if short.chars().count() > 36 {
                        let truncated: String = short.chars().take(35).collect();
                        format!("{truncated}…")
                    } else {
                        short.to_string()
                    }
                } else {
                    "Offline — click to reconnect".to_string()
                };
                let note = TreeRow::Note {
                    host: h.alias.clone(),
                    text,
                    reconnectable: !connecting,
                };
                let nh = note.height();
                rows.push((y, nh, note));
                y += nh;
            }
        }

        y += 8.0;
        *self.ivars().rows.borrow_mut() = rows;
        self.ivars().content_h.set(y);
        let max_scroll = (y - self.bounds().size.height).max(0.0);
        if self.ivars().scroll_y.get() > max_scroll {
            self.ivars().scroll_y.set(max_scroll);
        }
        self.setNeedsDisplay(true);
    }

    fn hit_row(&self, p: NSPoint) -> Option<(usize, RowAction)> {
        let w = self.bounds().size.width;
        let y = p.y + self.ivars().scroll_y.get();
        let rows = self.ivars().rows.borrow();
        for (idx, (ry, rh, row)) in rows.iter().enumerate() {
            if y >= *ry && y < *ry + *rh {
                let action = match row {
                    TreeRow::Host { .. } => {
                        if p.x >= w - 28.0 && p.x <= w - 8.0 {
                            RowAction::Add
                        } else {
                            RowAction::Main
                        }
                    }
                    TreeRow::Session { .. } => {
                        if p.x >= w - 24.0 && p.x <= w - 8.0 {
                            RowAction::Close
                        } else if p.x >= w - 42.0 && p.x < w - 24.0 {
                            RowAction::Add
                        } else {
                            RowAction::Main
                        }
                    }
                    TreeRow::Window { .. } => {
                        if p.x >= w - 26.0 && p.x <= w - 10.0 {
                            RowAction::Close
                        } else {
                            RowAction::Main
                        }
                    }
                    TreeRow::Note { .. } => RowAction::Main,
                };
                return Some((idx, action));
            }
        }
        None
    }

    fn draw(&self) {
        let b = self.bounds();
        NSBezierPath::clipRect(b);
        let w = b.size.width;
        let scroll = self.ivars().scroll_y.get();
        let hover = self.ivars().hover.get();
        let style = self.ivars().chrome.get();
        let minimal = style.is_minimal();
        let tok = chrome_tokens(style, [0x17, 0x19, 0x1D]);
        let rows = self.ivars().rows.borrow();

        for (idx, (ry, rh, row)) in rows.iter().enumerate() {
            let y = *ry - scroll;
            if y + *rh < 0.0 || y > b.size.height {
                continue;
            }
            let hovered = hover.is_some_and(|(hi, _)| hi == idx);
            let hovered_act = hover.and_then(|(hi, act)| (hi == idx).then_some(act));

            match row {
                TreeRow::Host {
                    display_name,
                    is_local,
                    os,
                    color,
                    connected,
                    connecting,
                    rtt_ms,
                    top_pad,
                    ..
                } => {
                    let y0 = y + *top_pad;
                    if *top_pad > 0.0 {
                        let sep = if minimal {
                            srgb_alpha(tok.hairline_rgb, tok.hairline_alpha as f64)
                        } else {
                            NSColor::separatorColor().colorWithAlphaComponent(0.28)
                        };
                        sep.setFill();
                        NSBezierPath::fillRect(NSRect::new(
                            NSPoint::new(12.0, y + 4.0),
                            NSSize::new((w - 24.0).max(0.0), 1.0),
                        ));
                    }

                    let sym = if *is_local {
                        "laptopcomputer"
                    } else {
                        match os.as_deref() {
                            Some("macos") => "macmini",
                            Some("linux") => "server.rack",
                            Some("windows") => "pc",
                            _ => "globe",
                        }
                    };
                    let icon_color = match color {
                        Some(hc) => srgb_alpha(hc.rgb, if *connected { 0.95 } else { 0.55 }),
                        None => {
                            if minimal {
                                if *connected {
                                    srgb_alpha([0xEE, 0xEE, 0xEF], 0.85)
                                } else {
                                    srgb_alpha([0x69, 0x6A, 0x6D], 0.85)
                                }
                            } else if *connected {
                                NSColor::labelColor().colorWithAlphaComponent(0.85)
                            } else {
                                NSColor::secondaryLabelColor().colorWithAlphaComponent(0.6)
                            }
                        }
                    };
                    draw_symbol(sym, NSRect::new(NSPoint::new(12.0, y0 + 5.0), NSSize::new(14.0, 14.0)), &icon_color);

                    let dot_color = if *connected {
                        if minimal { srgb_alpha([0x4C, 0xC4, 0x8C], 1.0) } else { NSColor::systemGreenColor() }
                    } else if *connecting {
                        if minimal { srgb_alpha([0xF2, 0xA3, 0x3C], 1.0) } else { NSColor::systemOrangeColor() }
                    } else if minimal {
                        srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
                    } else {
                        NSColor::tertiaryLabelColor()
                    };
                    dot_color.setFill();
                    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                        NSPoint::new(w - 36.0, y0 + 10.0),
                        NSSize::new(6.0, 6.0),
                    ))
                    .fill();

                    if hovered_act == Some(RowAction::Add) {
                        let hov = if minimal {
                            srgb_alpha([255, 255, 255], 0.08)
                        } else {
                            NSColor::labelColor().colorWithAlphaComponent(0.12)
                        };
                        fill_rounded_rect(
                            NSRect::new(NSPoint::new(w - 26.0, y0 + 4.0), NSSize::new(18.0, 18.0)),
                            4.0,
                            &hov,
                        );
                    }
                    let plus_col = if hovered_act == Some(RowAction::Add) {
                        if minimal { srgb_alpha([0xEE, 0xEE, 0xEF], 1.0) } else { NSColor::labelColor() }
                    } else if minimal {
                        srgb_alpha([0x98, 0x99, 0x9B], 0.85)
                    } else {
                        NSColor::secondaryLabelColor().colorWithAlphaComponent(0.75)
                    };
                    draw_text_centered(
                        "+",
                        13.5,
                        true,
                        &plus_col,
                        NSRect::new(NSPoint::new(w - 26.0, y0 + 3.0), NSSize::new(18.0, 18.0)),
                    );

                    let prefix = if color.is_some_and(|c| c.danger) { "▲ " } else { "" };
                    let label = match (is_local, rtt_ms) {
                        (false, Some(ms)) => format!("{prefix}{display_name}  {ms:.0}ms"),
                        _ => format!("{prefix}{display_name}"),
                    };
                    let fg = match color {
                        Some(hc) if hc.danger => srgb_alpha(hc.rgb, 0.95),
                        _ if minimal => srgb_alpha([0xEE, 0xEE, 0xEF], 0.92),
                        _ => NSColor::labelColor().colorWithAlphaComponent(0.9),
                    };
                    draw_text_left(
                        &label,
                        12.0,
                        FontWeight::Semibold,
                        &fg,
                        NSRect::new(NSPoint::new(31.0, y0 + 5.0), NSSize::new((w - 72.0).max(0.0), 16.0)),
                    );
                }

                TreeRow::Session { name, .. } => {
                    let fg = if minimal {
                        srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
                    } else {
                        NSColor::secondaryLabelColor().colorWithAlphaComponent(0.78)
                    };
                    let right_margin = if hovered { 46.0 } else { 14.0 };
                    draw_text_left(
                        name,
                        11.0,
                        FontWeight::Medium,
                        &fg,
                        NSRect::new(
                            NSPoint::new(14.0, y + 3.0),
                            NSSize::new((w - 14.0 - right_margin).max(0.0), 14.0),
                        ),
                    );
                    if hovered {
                        let add_col = if hovered_act == Some(RowAction::Add) {
                            if minimal { srgb_alpha([0xEE, 0xEE, 0xEF], 1.0) } else { NSColor::labelColor() }
                        } else if minimal {
                            srgb_alpha([0x98, 0x99, 0x9B], 1.0)
                        } else {
                            NSColor::secondaryLabelColor()
                        };
                        draw_text_centered(
                            "+",
                            12.5,
                            true,
                            &add_col,
                            NSRect::new(NSPoint::new(w - 40.0, y + 1.0), NSSize::new(16.0, 16.0)),
                        );
                        let close_col = if hovered_act == Some(RowAction::Close) {
                            NSColor::systemRedColor()
                        } else if minimal {
                            srgb_alpha([0x98, 0x99, 0x9B], 1.0)
                        } else {
                            NSColor::secondaryLabelColor()
                        };
                        draw_text_centered(
                            "×",
                            12.5,
                            false,
                            &close_col,
                            NSRect::new(NSPoint::new(w - 23.0, y + 1.0), NSSize::new(16.0, 16.0)),
                        );
                    }
                }

                TreeRow::Window {
                    title,
                    pane_count,
                    running,
                    active,
                    status,
                    host_color,
                    git_branch,
                    git_dirty,
                    is_worktree,
                    blocked_count,
                    error_count,
                    ..
                } => {
                    let pill = NSRect::new(
                        NSPoint::new(8.0, y + 1.0),
                        NSSize::new((w - 16.0).max(0.0), *rh - 2.0),
                    );
                    let radius = if minimal { 8.0 } else { 10.0 };
                    if *active {
                        let sel = if minimal {
                            srgb_alpha([255, 255, 255], 0.09)
                        } else {
                            NSColor::labelColor().colorWithAlphaComponent(0.14)
                        };
                        fill_rounded_rect(pill, radius, &sel);
                        if minimal {
                            stroke_rounded_rect(pill, radius, &srgb_alpha([255, 255, 255], 0.06), 1.0);
                        }
                    } else if hovered {
                        let hov = if minimal {
                            srgb_alpha([255, 255, 255], 0.055)
                        } else {
                            NSColor::labelColor().colorWithAlphaComponent(0.07)
                        };
                        fill_rounded_rect(pill, radius, &hov);
                    }

                    // Left 18×18 monogram avatar.
                    let avatar = NSRect::new(NSPoint::new(13.0, y + 6.0), NSSize::new(18.0, 18.0));
                    let (av_bg, av_border, av_fg) = match host_color {
                        Some(hc) => (
                            srgb_alpha(hc.rgb, 0.18),
                            srgb_alpha(hc.rgb, 0.32),
                            srgb_alpha(hc.rgb, 0.95),
                        ),
                        None if minimal => (
                            srgb_alpha([0x24, 0x26, 0x2B], 1.0),
                            if *blocked_count > 0 {
                                srgb_alpha([0xF2, 0xA3, 0x3C], 0.45)
                            } else {
                                srgb_alpha([255, 255, 255], 0.08)
                            },
                            srgb_alpha([0xD2, 0xD4, 0xD9], 0.95),
                        ),
                        None => (
                            NSColor::labelColor().colorWithAlphaComponent(0.10),
                            if *blocked_count > 0 {
                                NSColor::systemOrangeColor().colorWithAlphaComponent(0.50)
                            } else {
                                NSColor::labelColor().colorWithAlphaComponent(0.12)
                            },
                            NSColor::labelColor().colorWithAlphaComponent(0.85),
                        ),
                    };
                    fill_rounded_rect(avatar, 5.0, &av_bg);
                    stroke_rounded_rect(avatar, 5.0, &av_border, 1.0);
                    let mono = space_monogram(title);
                    draw_text_centered(
                        &mono,
                        10.5,
                        true,
                        &av_fg,
                        NSRect::new(NSPoint::new(13.0, y + 6.5), NSSize::new(18.0, 16.0)),
                    );

                    // Right-aligned close button / status badge + optional git branch.
                    let mut right_x = w - 12.0;
                    if hovered {
                        let x_col = if hovered_act == Some(RowAction::Close) {
                            NSColor::systemRedColor()
                        } else if minimal {
                            srgb_alpha([0x98, 0x99, 0x9B], 1.0)
                        } else {
                            NSColor::secondaryLabelColor()
                        };
                        draw_text_centered(
                            "×",
                            12.0,
                            false,
                            &x_col,
                            NSRect::new(NSPoint::new(w - 25.0, y + 6.0), NSSize::new(16.0, 16.0)),
                        );
                        right_x = w - 28.0;
                    } else if *blocked_count > 0 {
                        let text = blocked_count.to_string();
                        let bw = ((text.len() as f64) * 6.5 + 10.0).max(16.0);
                        let b_rect = NSRect::new(NSPoint::new(right_x - bw - 2.0, y + 7.5), NSSize::new(bw, 15.0));
                        let amber = if minimal { srgb_alpha([0xF2, 0xA3, 0x3C], 1.0) } else { NSColor::systemOrangeColor() };
                        fill_rounded_rect(b_rect, 7.5, &amber.colorWithAlphaComponent(0.22));
                        stroke_rounded_rect(b_rect, 7.5, &amber.colorWithAlphaComponent(0.48), 1.0);
                        draw_text_centered(&text, 10.0, true, &amber, b_rect);
                        right_x -= bw + 6.0;
                    } else if *error_count > 0 {
                        let text = error_count.to_string();
                        let bw = ((text.len() as f64) * 6.5 + 10.0).max(16.0);
                        let b_rect = NSRect::new(NSPoint::new(right_x - bw - 2.0, y + 7.5), NSSize::new(bw, 15.0));
                        let red = if minimal { srgb_alpha([0xE5, 0x6B, 0x6F], 1.0) } else { NSColor::systemRedColor() };
                        fill_rounded_rect(b_rect, 7.5, &red.colorWithAlphaComponent(0.22));
                        draw_text_centered(&text, 10.0, true, &red, b_rect);
                        right_x -= bw + 6.0;
                    } else if *pane_count > 1 {
                        let text = pane_count.to_string();
                        let b_rect = NSRect::new(NSPoint::new(right_x - 18.0, y + 8.0), NSSize::new(16.0, 14.0));
                        let bg = if minimal { srgb_alpha([255, 255, 255], 0.08) } else { NSColor::labelColor().colorWithAlphaComponent(0.10) };
                        let fg = if minimal { srgb_alpha([0x98, 0x99, 0x9B], 1.0) } else { NSColor::secondaryLabelColor() };
                        fill_rounded_rect(b_rect, 7.0, &bg);
                        draw_text_centered(&text, 9.5, true, &fg, b_rect);
                        right_x -= 22.0;
                    } else if status.as_ref().is_some_and(|s| s.state == ProgramState::Working) || *running {
                        let dot = if minimal { srgb_alpha([0x76, 0xA8, 0xFF], 1.0) } else { NSColor::controlAccentColor() };
                        dot.setFill();
                        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                            NSPoint::new(right_x - 8.0, y + 12.5),
                            NSSize::new(5.0, 5.0),
                        ))
                        .fill();
                        right_x -= 13.0;
                    } else if status.as_ref().is_some_and(|s| s.state == ProgramState::Done) {
                        let dot = if minimal { srgb_alpha([0x4C, 0xC4, 0x8C], 1.0) } else { NSColor::systemGreenColor() };
                        dot.setFill();
                        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                            NSPoint::new(right_x - 8.0, y + 12.5),
                            NSSize::new(5.0, 5.0),
                        ))
                        .fill();
                        right_x -= 13.0;
                    }

                    if let Some(branch) = git_branch {
                        if right_x - 38.0 >= 112.0 {
                            let display_branch = match (*is_worktree, *git_dirty) {
                                (true, true) => format!("⎇ {branch}*"),
                                (true, false) => format!("⎇ {branch}"),
                                (false, true) => format!("{branch}*"),
                                (false, false) => branch.clone(),
                            };
                            let bw = ((display_branch.chars().count() as f64) * 5.8 + 4.0)
                                .min((right_x - 38.0) * 0.44)
                                .min(94.0);
                            let br_col = if *git_dirty {
                                if minimal {
                                    srgb_alpha([0xF2, 0xA3, 0x3C], 0.92)
                                } else {
                                    NSColor::systemOrangeColor().colorWithAlphaComponent(0.92)
                                }
                            } else if minimal {
                                srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
                            } else {
                                NSColor::secondaryLabelColor().colorWithAlphaComponent(0.75)
                            };
                            draw_text_right(
                                &display_branch,
                                10.5,
                                FontWeight::Regular,
                                &br_col,
                                NSRect::new(NSPoint::new(right_x - bw, y + 8.0), NSSize::new(bw, 14.0)),
                            );
                            right_x -= bw + 6.0;
                        }
                    }

                    let fg = if minimal {
                        if *active { srgb_alpha([0xEE, 0xEE, 0xEF], 1.0) } else { srgb_alpha([0x98, 0x99, 0x9B], 1.0) }
                    } else if *active {
                        NSColor::labelColor()
                    } else {
                        NSColor::labelColor().colorWithAlphaComponent(0.76)
                    };
                    draw_text_left(
                        title,
                        12.0,
                        if *active { FontWeight::Medium } else { FontWeight::Regular },
                        &fg,
                        NSRect::new(
                            NSPoint::new(37.0, y + 7.0),
                            NSSize::new((right_x - 41.0).max(0.0), 16.0),
                        ),
                    );
                }

                TreeRow::Note { text, .. } => {
                    let fg = if minimal {
                        srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
                    } else {
                        NSColor::tertiaryLabelColor()
                    };
                    draw_text_left(
                        text,
                        11.0,
                        FontWeight::Regular,
                        &fg,
                        NSRect::new(NSPoint::new(16.0, y + 3.0), NSSize::new((w - 26.0).max(0.0), 15.0)),
                    );
                }
            }
        }
    }

    fn show_context_menu(&self, mtm: MainThreadMarker, event: &NSEvent, row: &TreeRow) {
        let menu = NSMenu::new(mtm);
        let target: &AnyObject = self.as_ref();
        let add = |title: &str, sel_name: objc2::runtime::Sel, rep: String| {
            let item = unsafe {
                menu.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str(title),
                    Some(sel_name),
                    &NSString::from_str(""),
                )
            };
            unsafe {
                item.setTarget(Some(target));
                item.setRepresentedObject(Some(NSString::from_str(&rep).as_ref()));
            }
        };

        match row {
            TreeRow::Host { alias, is_local, color_spec, .. } => {
                add("New Session", sel!(ctxNewSession:), alias.clone());
                add("Reconnect / Refresh", sel!(ctxReconnectHost:), alias.clone());
                if !is_local {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                    let color_root = unsafe {
                        menu.addItemWithTitle_action_keyEquivalent(
                            &NSString::from_str("Host Colour"),
                            None,
                            &NSString::from_str(""),
                        )
                    };
                    let sub = NSMenu::new(mtm);
                    let active_spec = color_spec.as_deref().unwrap_or("auto");
                    for &(id, title) in crate::mux::model::HOST_COLOR_PRESETS {
                        let label = if active_spec.eq_ignore_ascii_case(id) {
                            format!("✓ {title}")
                        } else {
                            title.to_string()
                        };
                        let item = unsafe {
                            sub.addItemWithTitle_action_keyEquivalent(
                                &NSString::from_str(&label),
                                Some(sel!(ctxSetHostColor:)),
                                &NSString::from_str(""),
                            )
                        };
                        let rep = format!("{alias}\u{1F}{id}");
                        unsafe {
                            item.setTarget(Some(target));
                            item.setRepresentedObject(Some(NSString::from_str(&rep).as_ref()));
                        }
                    }
                    color_root.setSubmenu(Some(&sub));
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                    add("Remove Host", sel!(ctxRemoveHost:), alias.clone());
                }
            }
            TreeRow::Session { host, session_id, cwd, is_worktree, .. } => {
                let key = format!("{host}\u{1F}{session_id}");
                add("New Window", sel!(ctxNewWindow:), key.clone());
                if host == "local" {
                    if let Some(dir) = cwd.as_deref().filter(|d| git_info_for_dir(Path::new(d)).is_some()) {
                        add("New Worktree Space…", sel!(ctxNewWorktree:), dir.to_string());
                    }
                    if *is_worktree {
                        if let Some(dir) = cwd.as_deref() {
                            add(
                                "Remove Worktree & Close…",
                                sel!(ctxRemoveWorktree:),
                                format!("{host}\u{1F}{session_id}\u{1F}{dir}"),
                            );
                        }
                    }
                }
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                add("Kill Session", sel!(ctxKillSession:), key);
            }
            TreeRow::Window { host, session_id, window_id, cwd, is_worktree, .. } => {
                add("Open Window", sel!(ctxOpenWindow:), format!("{host}\u{1F}{window_id}"));
                add("New Window in Session", sel!(ctxNewWindow:), format!("{host}\u{1F}{session_id}"));
                if host == "local" {
                    if let Some(dir) = cwd.as_deref().filter(|d| git_info_for_dir(Path::new(d)).is_some()) {
                        add("New Worktree Space…", sel!(ctxNewWorktree:), dir.to_string());
                    }
                    if *is_worktree {
                        if let Some(dir) = cwd.as_deref() {
                            add(
                                "Remove Worktree & Close…",
                                sel!(ctxRemoveWorktree:),
                                format!("{host}\u{1F}{session_id}\u{1F}{dir}"),
                            );
                        }
                    }
                }
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                add("Kill Window", sel!(ctxKillWindow:), format!("{host}\u{1F}{window_id}"));
            }
            TreeRow::Note { host, .. } => {
                add("New Session", sel!(ctxNewSession:), host.clone());
                add("Reconnect", sel!(ctxReconnectHost:), host.clone());
            }
        }

        NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
    }
}

#[derive(Clone, Debug)]
enum AgentZoneRow {
    ProjectHeader {
        project: String,
        host: String,
        host_display: String,
        host_color: Option<HostColor>,
    },
    Agent {
        entry: AgentEntry,
        active: bool,
    },
}

impl AgentZoneRow {
    fn height(&self) -> f64 {
        match self {
            Self::ProjectHeader { .. } => 20.0,
            Self::Agent { .. } => 26.0,
        }
    }
}

#[derive(Default)]
struct AgentsZoneState {
    window: Cell<u64>,
    chrome: Cell<ChromeStyle>,
    rows: RefCell<Vec<(f64, f64, AgentZoneRow)>>,
    counts: Cell<AgentCounts>,
    content_h: Cell<f64>,
    scroll_y: Cell<f64>,
    hover: Cell<Option<usize>>,
    header_hover: Cell<bool>,
    state_since: RefCell<HashMap<(String, String), (ProgramState, Instant)>>,
    active_target: RefCell<Option<(String, String)>>,
    active_block: RefCell<Option<String>>,
    spinner_tick: Cell<usize>,
    dragging_resize: Cell<bool>,
}

define_class!(
    /// Zone 2: Hairline-separated, always-visible Live Agents pane grouped by project.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSidebarAgentsView"]
    #[ivars = AgentsZoneState]
    struct SidebarAgentsView;

    impl SidebarAgentsView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let dy = if event.hasPreciseScrollingDeltas() {
                event.scrollingDeltaY()
            } else {
                event.scrollingDeltaY() * 14.0
            };
            let scrollable_h = (self.bounds().size.height - AGENTS_HEADER_H).max(0.0);
            let max_scroll = (self.ivars().content_h.get() - scrollable_h).max(0.0);
            let next = (self.ivars().scroll_y.get() - dy).clamp(0.0, max_scroll);
            if (next - self.ivars().scroll_y.get()).abs() > 0.1 {
                self.ivars().scroll_y.set(next);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.in_resize_grip(p.x) {
                set_resize_cursor();
                self.clear_hover();
                return;
            }
            NSCursor::arrowCursor().set();
            let header_hit = p.y >= 0.0 && p.y < AGENTS_HEADER_H;
            let row_hit = if header_hit { None } else { self.hit_agent_row(p) };
            if self.ivars().header_hover.get() != header_hit || self.ivars().hover.get() != row_hit {
                self.ivars().header_hover.set(header_hit);
                self.ivars().hover.set(row_hit);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            NSCursor::arrowCursor().set();
            self.clear_hover();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.in_resize_grip(p.x) {
                self.ivars().dragging_resize.set(true);
                return;
            }
            let win_id = self.ivars().window.get();
            if p.y >= 0.0 && p.y < AGENTS_HEADER_H {
                with_window(win_id, |w| w.toggle_agents_card());
                return;
            }
            let Some(idx) = self.hit_agent_row(p) else { return };
            let entry = {
                let rows = self.ivars().rows.borrow();
                match rows.get(idx) {
                    Some((_, _, AgentZoneRow::Agent { entry, .. })) => Some(entry.clone()),
                    _ => None,
                }
            };
            if let Some(e) = entry {
                super::mux_select_window_and_block(
                    win_id,
                    &e.host,
                    &e.window_id,
                    Some(&e.block_id),
                );
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if !self.ivars().dragging_resize.get() {
                return;
            }
            drag_sidebar_width(self.ivars().window.get(), event);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().dragging_resize.set(false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let Some(idx) = self.hit_agent_row(p) else { return };
            let entry = {
                let rows = self.ivars().rows.borrow();
                match rows.get(idx) {
                    Some((_, _, AgentZoneRow::Agent { entry, .. })) => Some(entry.clone()),
                    _ => None,
                }
            };
            let Some(e) = entry else { return };
            let Some(mtm) = MainThreadMarker::new() else { return };

            let menu = NSMenu::new(mtm);
            let target: &AnyObject = self.as_ref();
            let rep = format!("{}\u{1F}{}\u{1F}{}", e.host, e.window_id, e.block_id);
            let focus_item = unsafe {
                menu.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str("Focus Agent Pane"),
                    Some(sel!(ctxFocusAgent:)),
                    &NSString::from_str(""),
                )
            };
            unsafe {
                focus_item.setTarget(Some(target));
                focus_item.setRepresentedObject(Some(NSString::from_str(&rep).as_ref()));
            }
            let mc_item = unsafe {
                menu.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str("Open Mission Control (⌘⇧A)"),
                    Some(sel!(ctxOpenMissionControl:)),
                    &NSString::from_str(""),
                )
            };
            unsafe { mc_item.setTarget(Some(target)) };
            NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
        }

        #[unsafe(method(ctxFocusAgent:))]
        fn ctx_focus_agent(&self, sender: Option<&AnyObject>) {
            let Some(s) = rep_string(sender) else { return };
            let mut parts = s.splitn(3, '\u{1F}');
            let (Some(host), Some(wid), Some(bid)) = (parts.next(), parts.next(), parts.next()) else {
                return;
            };
            super::mux_select_window_and_block(self.ivars().window.get(), host, wid, Some(bid));
        }

        #[unsafe(method(ctxOpenMissionControl:))]
        fn ctx_open_mission_control(&self, _sender: Option<&AnyObject>) {
            with_window(self.ivars().window.get(), |w| w.toggle_agents_card());
        }
    }
);

impl SidebarAgentsView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = AgentsZoneState::default();
        state.window.set(window);
        let view: Retained<Self> =
            unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), initWithFrame: NSRect::ZERO] };
        view.setClipsToBounds(true);
        install_tracking_area(&view);
        view
    }

    fn in_resize_grip(&self, x: f64) -> bool {
        let w = self.bounds().size.width;
        x >= (w - RESIZE_GRIP_W).max(0.0)
    }

    fn clear_hover(&self) {
        if self.ivars().header_hover.get() || self.ivars().hover.get().is_some() {
            self.ivars().header_hover.set(false);
            self.ivars().hover.set(None);
            self.setNeedsDisplay(true);
        }
    }

    fn set_agents(
        &self,
        hosts: &[HostView],
        filter: &str,
        active_target: Option<&(String, String)>,
        active_block: Option<&str>,
    ) {
        *self.ivars().active_target.borrow_mut() = active_target.cloned();
        *self.ivars().active_block.borrow_mut() = active_block.map(str::to_string);

        let all_entries = super::agents::collect_from_host_views(hosts, &[]);
        self.ivars().counts.set(AgentCounts::from_entries(&all_entries));

        // Maintain state transition timestamps and clean vanished agent keys.
        let now_inst = Instant::now();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        {
            let mut since = self.ivars().state_since.borrow_mut();
            let mut live_keys = HashSet::with_capacity(all_entries.len());
            for e in &all_entries {
                let key = (e.host.clone(), e.block_id.clone());
                live_keys.insert(key.clone());
                match since.get(&key) {
                    Some((prev, _)) if *prev == e.state => {}
                    Some(_) => {
                        since.insert(key, (e.state, now_inst));
                    }
                    None => {
                        let start = if e.created_at > 0
                            && now_unix > e.created_at
                            && (now_unix - e.created_at) < 86_400
                        {
                            now_inst
                                .checked_sub(Duration::from_secs(now_unix - e.created_at))
                                .unwrap_or(now_inst)
                        } else {
                            now_inst
                        };
                        since.insert(key, (e.state, start));
                    }
                }
            }
            since.retain(|k, _| live_keys.contains(k));
        }

        let filtered = super::agents::filter_entries(&all_entries, AgentFilter::All, filter);
        let groups = group_agents_by_project(&filtered);

        let mut rows = Vec::new();
        let mut y = 2.0;
        for g in groups {
            let hrow = AgentZoneRow::ProjectHeader {
                project: g.project,
                host: g.host,
                host_display: g.host_display,
                host_color: g.host_color,
            };
            let hh = hrow.height();
            rows.push((y, hh, hrow));
            y += hh;

            for e in g.entries {
                let is_active = active_target.is_some_and(|(ah, aw)| {
                    *ah == e.host
                        && *aw == e.window_id
                        && active_block.is_none_or(|ab| ab == e.block_id)
                });
                let arow = AgentZoneRow::Agent { entry: e, active: is_active };
                let ah = arow.height();
                rows.push((y, ah, arow));
                y += ah;
            }
        }

        let content_h = if rows.is_empty() { 0.0 } else { y + 4.0 };
        *self.ivars().rows.borrow_mut() = rows;
        self.ivars().content_h.set(content_h);
        let scrollable_h = (self.bounds().size.height - AGENTS_HEADER_H).max(0.0);
        let max_scroll = (content_h - scrollable_h).max(0.0);
        if self.ivars().scroll_y.get() > max_scroll {
            self.ivars().scroll_y.set(max_scroll);
        }
        self.setNeedsDisplay(true);
    }

    fn refilter(&self, hosts: &[HostView], filter: &str) {
        let active_target = self.ivars().active_target.borrow().clone();
        let active_block = self.ivars().active_block.borrow().clone();
        self.set_agents(hosts, filter, active_target.as_ref(), active_block.as_deref());
    }

    fn content_height(&self) -> f64 {
        self.ivars().content_h.get()
    }

    fn is_empty(&self) -> bool {
        self.ivars().rows.borrow().is_empty()
    }

    fn counts(&self) -> AgentCounts {
        self.ivars().counts.get()
    }

    fn tick(&self) {
        let next = self.ivars().spinner_tick.get().wrapping_add(1);
        self.ivars().spinner_tick.set(next);
        self.setNeedsDisplay(true);
    }

    fn hit_agent_row(&self, p: NSPoint) -> Option<usize> {
        if p.y < AGENTS_HEADER_H {
            return None;
        }
        let y = (p.y - AGENTS_HEADER_H) + self.ivars().scroll_y.get();
        let rows = self.ivars().rows.borrow();
        for (idx, (ry, rh, row)) in rows.iter().enumerate() {
            if y >= *ry && y < *ry + *rh && matches!(row, AgentZoneRow::Agent { .. }) {
                return Some(idx);
            }
        }
        None
    }

    fn draw(&self) {
        let b = self.bounds();
        if b.size.height < AGENTS_HEADER_H {
            return;
        }
        NSBezierPath::clipRect(b);
        let w = b.size.width;
        let style = self.ivars().chrome.get();
        let minimal = style.is_minimal();
        let counts = self.ivars().counts.get();

        // Pinned Zone 2 header (`LIVE AGENTS` + blocked/count pill; clicks open Mission Control).
        if self.ivars().header_hover.get() {
            let hov = if minimal {
                srgb_alpha([255, 255, 255], 0.04)
            } else {
                NSColor::labelColor().colorWithAlphaComponent(0.05)
            };
            fill_rounded_rect(
                NSRect::new(NSPoint::new(8.0, 2.0), NSSize::new((w - 16.0).max(0.0), AGENTS_HEADER_H - 4.0)),
                6.0,
                &hov,
            );
        }
        let hdr_col = if minimal {
            srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
        } else {
            NSColor::secondaryLabelColor().colorWithAlphaComponent(0.8)
        };
        draw_text_left(
            "LIVE AGENTS",
            10.5,
            FontWeight::Semibold,
            &hdr_col,
            NSRect::new(NSPoint::new(12.0, 6.0), NSSize::new((w - 110.0).max(0.0), 15.0)),
        );

        if counts.blocked > 0 {
            let badge = format!("{} blocked", counts.blocked);
            let bw = ((badge.len() as f64) * 5.8 + 12.0).clamp(52.0, 96.0);
            let b_rect = NSRect::new(NSPoint::new(w - bw - 10.0, 5.5), NSSize::new(bw, 15.0));
            let amber = if minimal { srgb_alpha([0xF2, 0xA3, 0x3C], 1.0) } else { NSColor::systemOrangeColor() };
            fill_rounded_rect(b_rect, 7.5, &amber.colorWithAlphaComponent(0.20));
            stroke_rounded_rect(b_rect, 7.5, &amber.colorWithAlphaComponent(0.45), 1.0);
            draw_text_centered(&badge, 10.0, true, &amber, b_rect);
        } else if counts.total > 0 {
            let badge = counts.total.to_string();
            let bw = ((badge.len() as f64) * 6.0 + 10.0).max(16.0);
            let b_rect = NSRect::new(NSPoint::new(w - bw - 10.0, 5.5), NSSize::new(bw, 15.0));
            let bg = if minimal { srgb_alpha([255, 255, 255], 0.08) } else { NSColor::labelColor().colorWithAlphaComponent(0.10) };
            let fg = if minimal { srgb_alpha([0x98, 0x99, 0x9B], 1.0) } else { NSColor::secondaryLabelColor() };
            fill_rounded_rect(b_rect, 7.5, &bg);
            draw_text_centered(&badge, 10.0, true, &fg, b_rect);
        }

        // Clip scrollable agent body below header.
        let body_rect = NSRect::new(
            NSPoint::new(0.0, AGENTS_HEADER_H),
            NSSize::new(w, (b.size.height - AGENTS_HEADER_H).max(0.0)),
        );
        NSBezierPath::clipRect(body_rect);

        let rows = self.ivars().rows.borrow();
        if rows.is_empty() {
            let empty_col = if minimal {
                srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
            } else {
                NSColor::tertiaryLabelColor()
            };
            draw_text_left(
                "No active agents",
                11.0,
                FontWeight::Regular,
                &empty_col,
                NSRect::new(NSPoint::new(14.0, AGENTS_HEADER_H + 2.0), NSSize::new((w - 28.0).max(0.0), 16.0)),
            );
            return;
        }

        let scroll = self.ivars().scroll_y.get();
        let hover = self.ivars().hover.get();
        let tick = self.ivars().spinner_tick.get();
        let since_map = self.ivars().state_since.borrow();

        for (idx, (ry, rh, row)) in rows.iter().enumerate() {
            let y = AGENTS_HEADER_H + *ry - scroll;
            if y + *rh < AGENTS_HEADER_H || y > b.size.height {
                continue;
            }
            match row {
                AgentZoneRow::ProjectHeader { project, host, host_display, host_color } => {
                    let dot_col = match host_color {
                        Some(hc) => srgb_alpha(hc.rgb, 0.9),
                        None if minimal => srgb_alpha([0x69, 0x6A, 0x6D], 1.0),
                        None => NSColor::tertiaryLabelColor(),
                    };
                    dot_col.setFill();
                    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                        NSPoint::new(13.0, y + 7.5),
                        NSSize::new(5.0, 5.0),
                    ))
                    .fill();

                    let mut text_right = w - 12.0;
                    if host != "local" {
                        let hw = ((host_display.chars().count() as f64) * 5.6 + 4.0)
                            .min((w - 40.0) * 0.4)
                            .min(74.0);
                        let hcol = match host_color {
                            Some(hc) => srgb_alpha(hc.rgb, 0.85),
                            None if minimal => srgb_alpha([0x69, 0x6A, 0x6D], 1.0),
                            None => NSColor::tertiaryLabelColor(),
                        };
                        draw_text_right(
                            host_display,
                            10.0,
                            FontWeight::Medium,
                            &hcol,
                            NSRect::new(NSPoint::new(text_right - hw, y + 3.5), NSSize::new(hw, 14.0)),
                        );
                        text_right -= hw + 6.0;
                    }

                    let pcol = if minimal {
                        srgb_alpha([0x98, 0x99, 0x9B], 1.0)
                    } else {
                        NSColor::secondaryLabelColor().colorWithAlphaComponent(0.9)
                    };
                    draw_text_left(
                        project,
                        11.0,
                        FontWeight::Medium,
                        &pcol,
                        NSRect::new(
                            NSPoint::new(24.0, y + 3.0),
                            NSSize::new((text_right - 24.0).max(0.0), 15.0),
                        ),
                    );
                }

                AgentZoneRow::Agent { entry, active } => {
                    let hovered = hover == Some(idx);
                    if *active || hovered {
                        let bg = if minimal {
                            srgb_alpha([255, 255, 255], if *active { 0.08 } else { 0.055 })
                        } else {
                            NSColor::labelColor().colorWithAlphaComponent(if *active { 0.12 } else { 0.07 })
                        };
                        fill_rounded_rect(
                            NSRect::new(NSPoint::new(8.0, y + 1.0), NSSize::new((w - 16.0).max(0.0), *rh - 2.0)),
                            8.0,
                            &bg,
                        );
                    }

                    let (ind, ind_col) = match entry.state {
                        ProgramState::Working => (
                            BRAILLE_SPINNER[tick % BRAILLE_SPINNER.len()],
                            if minimal { srgb_alpha([0x76, 0xA8, 0xFF], 1.0) } else { NSColor::controlAccentColor() },
                        ),
                        ProgramState::Blocked => (
                            "●",
                            if minimal { srgb_alpha([0xF2, 0xA3, 0x3C], 1.0) } else { NSColor::systemOrangeColor() },
                        ),
                        ProgramState::Error => (
                            "✕",
                            if minimal { srgb_alpha([0xE5, 0x6B, 0x6F], 1.0) } else { NSColor::systemRedColor() },
                        ),
                        ProgramState::Done => (
                            "✓",
                            if minimal { srgb_alpha([0x4C, 0xC4, 0x8C], 1.0) } else { NSColor::systemGreenColor() },
                        ),
                        ProgramState::Idle => (
                            "○",
                            if minimal { srgb_alpha([0x69, 0x6A, 0x6D], 1.0) } else { NSColor::tertiaryLabelColor() },
                        ),
                    };
                    draw_text_centered(
                        ind,
                        11.5,
                        true,
                        &ind_col,
                        NSRect::new(NSPoint::new(12.0, y + 5.0), NSSize::new(14.0, 16.0)),
                    );

                    // Monospaced elapsed timer on right.
                    let secs = since_map
                        .get(&(entry.host.clone(), entry.block_id.clone()))
                        .map(|(_, ts)| ts.elapsed().as_secs())
                        .unwrap_or(0);
                    let dur = format_elapsed(secs);
                    let dur_col = if entry.state == ProgramState::Blocked {
                        if minimal { srgb_alpha([0xF2, 0xA3, 0x3C], 1.0) } else { NSColor::systemOrangeColor() }
                    } else if minimal {
                        srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
                    } else {
                        NSColor::tertiaryLabelColor()
                    };
                    draw_text_aligned(
                        &dur,
                        10.5,
                        FontWeight::Regular,
                        true,
                        &dur_col,
                        NSTextAlignment::Right,
                        NSRect::new(NSPoint::new(w - 58.0, y + 5.5), NSSize::new(44.0, 15.0)),
                    );

                    let label = match entry
                        .message
                        .as_deref()
                        .map(str::trim)
                        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case(&entry.agent_name))
                    {
                        Some(msg) => format!("{} · {msg}", entry.agent_name),
                        None => entry.agent_name.clone(),
                    };
                    let fg = if minimal {
                        if *active { srgb_alpha([0xEE, 0xEE, 0xEF], 1.0) } else { srgb_alpha([0xD2, 0xD4, 0xD9], 0.92) }
                    } else if *active {
                        NSColor::labelColor()
                    } else {
                        NSColor::labelColor().colorWithAlphaComponent(0.82)
                    };
                    draw_text_left(
                        &label,
                        11.5,
                        if *active || entry.state == ProgramState::Blocked {
                            FontWeight::Medium
                        } else {
                            FontWeight::Regular
                        },
                        &fg,
                        NSRect::new(NSPoint::new(28.0, y + 5.0), NSSize::new((w - 90.0).max(0.0), 16.0)),
                    );
                }
            }
        }
    }
}

#[derive(Default)]
struct FooterState {
    window: Cell<u64>,
    chrome: Cell<ChromeStyle>,
    hover_row: Cell<Option<usize>>,
    server_health: Cell<u8>, // 0=offline, 1=connecting, 2=connected
    server_tip: RefCell<String>,
    dragging_resize: Cell<bool>,
}

define_class!(
    /// Clean 2-row utility footer (`Settings` with server health dot + `Shortcuts`).
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSidebarFooterView"]
    #[ivars = FooterState]
    struct SidebarFooterView;

    impl SidebarFooterView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.in_resize_grip(p.x) {
                set_resize_cursor();
                if self.ivars().hover_row.get().is_some() {
                    self.ivars().hover_row.set(None);
                    self.setNeedsDisplay(true);
                }
                return;
            }
            NSCursor::arrowCursor().set();
            let hit = Self::hit_row(p.y);
            if self.ivars().hover_row.get() != hit {
                self.ivars().hover_row.set(hit);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            NSCursor::arrowCursor().set();
            if self.ivars().hover_row.get().is_some() {
                self.ivars().hover_row.set(None);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.in_resize_grip(p.x) {
                self.ivars().dragging_resize.set(true);
                return;
            }
            match Self::hit_row(p.y) {
                Some(0) => super::open_settings(),
                Some(1) => with_window(self.ivars().window.get(), |w| w.toggle_cheatsheet()),
                _ => {}
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if !self.ivars().dragging_resize.get() {
                return;
            }
            drag_sidebar_width(self.ivars().window.get(), event);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().dragging_resize.set(false);
        }
    }
);

impl SidebarFooterView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = FooterState::default();
        state.window.set(window);
        let view: Retained<Self> =
            unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), initWithFrame: NSRect::ZERO] };
        view.setClipsToBounds(true);
        install_tracking_area(&view);
        view
    }

    fn in_resize_grip(&self, x: f64) -> bool {
        let w = self.bounds().size.width;
        x >= (w - RESIZE_GRIP_W).max(0.0)
    }

    fn hit_row(y: f64) -> Option<usize> {
        if (3.0..29.0).contains(&y) {
            Some(0)
        } else if (29.0..55.0).contains(&y) {
            Some(1)
        } else {
            None
        }
    }

    fn set_server_health(&self, health: u8, tip: String) {
        self.ivars().server_health.set(health);
        *self.ivars().server_tip.borrow_mut() = tip.clone();
        self.setToolTip(Some(&NSString::from_str(&tip)));
        self.setNeedsDisplay(true);
    }

    fn draw(&self) {
        let b = self.bounds();
        NSBezierPath::clipRect(b);
        let w = b.size.width;
        let minimal = self.ivars().chrome.get().is_minimal();
        let hover_row = self.ivars().hover_row.get();
        let health = self.ivars().server_health.get();

        let rows = [
            (0usize, 3.0, "gearshape", "Settings", "⌘,"),
            (1usize, 29.0, "keyboard", "Shortcuts", "⌘/"),
        ];

        for (idx, ry, sym, label, shortcut) in rows {
            let hovered = hover_row == Some(idx);
            if hovered {
                let bg = if minimal {
                    srgb_alpha([255, 255, 255], 0.055)
                } else {
                    NSColor::labelColor().colorWithAlphaComponent(0.07)
                };
                fill_rounded_rect(
                    NSRect::new(NSPoint::new(8.0, ry + 1.0), NSSize::new((w - 16.0).max(0.0), 24.0)),
                    8.0,
                    &bg,
                );
            }

            let fg = if minimal {
                if hovered { srgb_alpha([0xEE, 0xEE, 0xEF], 1.0) } else { srgb_alpha([0x98, 0x99, 0x9B], 1.0) }
            } else if hovered {
                NSColor::labelColor()
            } else {
                NSColor::labelColor().colorWithAlphaComponent(0.80)
            };
            draw_symbol(
                sym,
                NSRect::new(NSPoint::new(14.0, ry + 6.5), NSSize::new(13.0, 13.0)),
                &fg,
            );
            draw_text_left(
                label,
                12.0,
                if hovered { FontWeight::Medium } else { FontWeight::Regular },
                &fg,
                NSRect::new(NSPoint::new(34.0, ry + 5.0), NSSize::new((w - 82.0).max(0.0), 16.0)),
            );

            if idx == 0 {
                let dot_col = match health {
                    2 => if minimal { srgb_alpha([0x4C, 0xC4, 0x8C], 1.0) } else { NSColor::systemGreenColor() },
                    1 => if minimal { srgb_alpha([0xF2, 0xA3, 0x3C], 1.0) } else { NSColor::systemOrangeColor() },
                    _ => if minimal { srgb_alpha([0xE5, 0x6B, 0x6F], 1.0) } else { NSColor::systemRedColor() },
                };
                dot_col.setFill();
                NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                    NSPoint::new(w - 42.0, ry + 10.5),
                    NSSize::new(5.0, 5.0),
                ))
                .fill();
            }

            let sc_col = if minimal {
                srgb_alpha([0x69, 0x6A, 0x6D], 1.0)
            } else {
                NSColor::tertiaryLabelColor()
            };
            draw_text_right(
                shortcut,
                11.0,
                FontWeight::Regular,
                &sc_col,
                NSRect::new(NSPoint::new(w - 34.0, ry + 5.5), NSSize::new(22.0, 15.0)),
            );
        }
    }
}

define_class!(
    /// Container view for the sidebar (surface tint + zone hairlines)
    /// and target for toolbar buttons, filter search field, and host menus.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSidebarRootView"]
    #[ivars = RootState]
    struct SidebarRootView;

    impl SidebarRootView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let style = self.ivars().chrome.get();
            let minimal = style.is_minimal();
            let agents_h = self.ivars().agents_h.get();

            if minimal {
                let tok = chrome_tokens(style, [0x17, 0x19, 0x1D]);
                srgb_alpha(tok.chrome_bg, 1.0).setFill();
                NSBezierPath::fillRect(b);
                srgb_alpha(tok.hairline_rgb, tok.hairline_alpha as f64).setFill();
            } else {
                NSColor::blackColor().colorWithAlphaComponent(0.16).setFill();
                NSBezierPath::fillRect(b);
                NSColor::separatorColor().colorWithAlphaComponent(0.28).setFill();
            }

            // 1. Hairline below top toolbar + filter bar.
            let header_bottom = (b.size.height - TOP_BAR_H - FILTER_BAR_H).max(0.0);
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(0.0, header_bottom),
                NSSize::new(b.size.width, 1.0),
            ));
            // 2. Hairline between Zone 1 (Spaces) and Zone 2 (Live Agents).
            if agents_h >= 1.0 {
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(0.0, UTILITY_FOOTER_H + agents_h),
                    NSSize::new(b.size.width, 1.0),
                ));
            }
            // 3. Hairline above 2-row utility footer.
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(0.0, UTILITY_FOOTER_H),
                NSSize::new(b.size.width, 1.0),
            ));
            // 4. Right edge vertical divider hairline.
            if !minimal {
                NSColor::separatorColor().colorWithAlphaComponent(0.40).setFill();
            }
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(b.size.width - 1.0, 0.0),
                NSSize::new(1.0, b.size.height),
            ));
        }

        #[unsafe(method(sidebarToggle:))]
        fn sidebar_toggle(&self, _sender: Option<&AnyObject>) {
            with_window(self.ivars().window.get(), |w| w.toggle_sidebar());
        }

        #[unsafe(method(sidebarNewSession:))]
        fn sidebar_new_session(&self, _sender: Option<&AnyObject>) {
            let win_id = self.ivars().window.get();
            let host = self.ivars().active_host.borrow().clone();
            super::mux_new_session(win_id, &host);
        }

        #[unsafe(method(sidebarRefresh:))]
        fn sidebar_refresh(&self, _sender: Option<&AnyObject>) {
            super::mux_refresh_all();
        }

        #[unsafe(method(sidebarPalette:))]
        fn sidebar_palette(&self, _sender: Option<&AnyObject>) {
            with_window(self.ivars().window.get(), |w| w.toggle_palette());
        }

        #[unsafe(method(sidebarAgentsFilter:))]
        fn sidebar_agents_filter(&self, _sender: Option<&AnyObject>) {
            with_window(self.ivars().window.get(), |w| w.toggle_sidebar_agents_filter());
        }

        #[unsafe(method(sidebarHostsMenu:))]
        fn sidebar_hosts_menu(&self, sender: Option<&AnyObject>) {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let menu = NSMenu::new(mtm);
            let target: &AnyObject = self.as_ref();
            let hosts = super::mux_host_views();

            let refresh_item = unsafe {
                menu.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str("↻ Refresh All Hosts"),
                    Some(sel!(sidebarRefresh:)),
                    &NSString::from_str(""),
                )
            };
            unsafe { refresh_item.setTarget(Some(target)) };
            menu.addItem(&NSMenuItem::separatorItem(mtm));

            let header = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str("Connected & Saved Hosts"),
                    None,
                    &NSString::from_str(""),
                )
            };
            header.setEnabled(false);
            menu.addItem(&header);

            for h in &hosts {
                let status = match &h.status {
                    HostStatus::Connected { rtt_ms: Some(ms), .. } if !h.is_local => {
                        format!("🟢 {} ({ms:.0} ms) · {}", h.display_name, h.uri)
                    }
                    HostStatus::Connected { .. } => format!("🟢 {}", h.display_name),
                    HostStatus::Connecting => format!("🟡 {} (connecting…)", h.display_name),
                    HostStatus::Disconnected { .. } => {
                        format!("⚪️ {} ({}) — click to reconnect", h.display_name, h.uri)
                    }
                };
                let item = unsafe {
                    menu.addItemWithTitle_action_keyEquivalent(
                        &NSString::from_str(&status),
                        Some(sel!(menuReconnectHost:)),
                        &NSString::from_str(""),
                    )
                };
                unsafe {
                    item.setTarget(Some(target));
                    item.setRepresentedObject(Some(NSString::from_str(&h.alias).as_ref()));
                }
            }

            let ssh_hosts: Vec<_> = machines::all()
                .iter()
                .filter(|m| !hosts.iter().any(|h| h.alias == m.name))
                .collect();
            if !ssh_hosts.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                let add_hdr = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str("Add Remote Host from ~/.ssh/config"),
                        None,
                        &NSString::from_str(""),
                    )
                };
                add_hdr.setEnabled(false);
                menu.addItem(&add_hdr);
                for m in ssh_hosts {
                    let title = format!("+ {} ({})", m.name, m.address);
                    let item = unsafe {
                        menu.addItemWithTitle_action_keyEquivalent(
                            &NSString::from_str(&title),
                            Some(sel!(menuAddSshHost:)),
                            &NSString::from_str(""),
                        )
                    };
                    unsafe {
                        item.setTarget(Some(target));
                        item.setRepresentedObject(Some(NSString::from_str(&m.name).as_ref()));
                    }
                }
            }

            if let Some(btn) = sender.and_then(|s| s.downcast_ref::<NSView>()) {
                let loc = NSPoint::new(0.0, btn.bounds().size.height + 4.0);
                menu.popUpMenuPositioningItem_atLocation_inView(None, loc, Some(btn));
            }
        }

        #[unsafe(method(menuReconnectHost:))]
        fn menu_reconnect_host(&self, sender: Option<&AnyObject>) {
            if let Some(alias) = rep_string(sender) {
                super::mux_reconnect_host(&alias);
            }
        }

        #[unsafe(method(menuAddSshHost:))]
        fn menu_add_ssh_host(&self, sender: Option<&AnyObject>) {
            if let Some(name) = rep_string(sender) {
                super::mux_add_host(name.clone(), format!("ssh://{name}"));
            }
        }
    }

    unsafe impl NSObjectProtocol for SidebarRootView {}

    unsafe impl NSControlTextEditingDelegate for SidebarRootView {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, note: &NSNotification) {
            if let Some(obj) = note.object() {
                if let Some(field) = obj.downcast_ref::<NSSearchField>() {
                    let text = field.stringValue().to_string();
                    let win_id = self.ivars().window.get();
                    with_window(win_id, |w| w.set_sidebar_filter(text));
                }
            }
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_do_command(
            &self,
            control: &NSControl,
            _text_view: &objc2_app_kit::NSTextView,
            command: objc2::runtime::Sel,
        ) -> bool {
            if command == sel!(cancelOperation:) {
                if let Some(field) = control.downcast_ref::<NSSearchField>() {
                    field.setStringValue(&NSString::from_str(""));
                }
                let win_id = self.ivars().window.get();
                with_window(win_id, |w| {
                    w.set_sidebar_filter(String::new());
                    w.focus_terminal();
                });
                true
            } else {
                false
            }
        }
    }

    unsafe impl NSTextFieldDelegate for SidebarRootView {}
    unsafe impl NSSearchFieldDelegate for SidebarRootView {}
);

#[derive(Default)]
struct RootState {
    window: Cell<u64>,
    active_host: RefCell<String>,
    chrome: Cell<ChromeStyle>,
    agents_h: Cell<f64>,
}

/// Complete collapsible two-zone sidebar mounted beside `TachyonView`.
pub struct Sidebar {
    pub root: Retained<NSView>,
    window_id: u64,
    visible: Cell<bool>,
    root_view: Retained<SidebarRootView>,
    tree: Retained<SidebarTreeView>,
    agents_view: Retained<SidebarAgentsView>,
    footer: Retained<SidebarFooterView>,
    toggle_btn: Retained<NSButton>,
    hosts_btn: Retained<NSButton>,
    palette_btn: Retained<NSButton>,
    new_btn: Retained<NSButton>,
    filter: Retained<NSSearchField>,
    agents_btn: Retained<NSButton>,
    agent_timer: RefCell<Option<(f64, Retained<NSTimer>)>>,
}

impl Drop for Sidebar {
    fn drop(&mut self) {
        if let Some((_, timer)) = self.agent_timer.borrow_mut().take() {
            timer.invalidate();
        }
    }
}

impl Sidebar {
    pub fn new(mtm: MainThreadMarker, parent: &NSView, window: u64) -> Self {
        let state = RootState::default();
        state.window.set(window);
        *state.active_host.borrow_mut() = "local".into();
        let root_view: Retained<SidebarRootView> =
            unsafe { msg_send![super(SidebarRootView::alloc(mtm).set_ivars(state)), initWithFrame: NSRect::ZERO] };

        let tree = SidebarTreeView::new(mtm, window);
        let agents_view = SidebarAgentsView::new(mtm, window);
        let footer = SidebarFooterView::new(mtm, window);
        root_view.addSubview(&tree);
        root_view.addSubview(&agents_view);
        root_view.addSubview(&footer);

        let target: &AnyObject = root_view.as_ref();
        let toggle_btn = symbol_button(mtm, "sidebar.left", "Toggle Sidebar (⌘B)", target, sel!(sidebarToggle:));
        let hosts_btn = symbol_button(mtm, "globe", "Hosts & Refresh", target, sel!(sidebarHostsMenu:));
        let palette_btn = symbol_button(mtm, "command", "Command Palette (⌘⇧P)", target, sel!(sidebarPalette:));
        let new_btn = symbol_button(mtm, "plus", "New Session", target, sel!(sidebarNewSession:));
        let agents_btn = symbol_button(
            mtm,
            "sparkles",
            "Filter Sidebar to Coding Agents (⌘⇧A for Mission Control)",
            target,
            sel!(sidebarAgentsFilter:),
        );

        let filter = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::ZERO);
        filter.setPlaceholderString(Some(&NSString::from_str("Filter spaces & agents…")));
        unsafe { filter.setDelegate(Some(ProtocolObject::from_ref(&*root_view))) };
        filter.setFocusRingType(objc2_app_kit::NSFocusRingType::None);

        root_view.addSubview(&toggle_btn);
        root_view.addSubview(&hosts_btn);
        root_view.addSubview(&palette_btn);
        root_view.addSubview(&new_btn);
        root_view.addSubview(&filter);
        root_view.addSubview(&agents_btn);

        let root: Retained<NSView> = Retained::into_super(root_view.clone());
        parent.addSubview(&root);

        Self {
            root,
            window_id: window,
            visible: Cell::new(false),
            root_view,
            tree,
            agents_view,
            footer,
            toggle_btn,
            hosts_btn,
            palette_btn,
            new_btn,
            filter,
            agents_btn,
            agent_timer: RefCell::new(None),
        }
    }

    pub fn set_frame(&self, frame: NSRect) {
        self.root.setFrame(frame);
        let w = frame.size.width;
        let h = frame.size.height;
        let top_y = (h - TOP_BAR_H).max(0.0);

        // Top toolbar buttons (clear of traffic lights on the left).
        let btn_y = top_y + (TOP_BAR_H - 22.0) / 2.0;
        self.toggle_btn.setFrame(NSRect::new(NSPoint::new(76.0, btn_y), NSSize::new(24.0, 22.0)));
        self.hosts_btn.setFrame(NSRect::new(NSPoint::new((w - 76.0).max(104.0), btn_y), NSSize::new(22.0, 22.0)));
        self.palette_btn.setFrame(NSRect::new(NSPoint::new((w - 52.0).max(104.0), btn_y), NSSize::new(22.0, 22.0)));
        self.new_btn.setFrame(NSRect::new(NSPoint::new((w - 28.0).max(104.0), btn_y), NSSize::new(22.0, 22.0)));

        // Filter row directly beneath the top toolbar.
        let filter_y = (h - TOP_BAR_H - FILTER_BAR_H + (FILTER_BAR_H - 22.0) / 2.0).max(UTILITY_FOOTER_H);
        let filter_w = (w - 40.0).max(40.0);
        self.filter.setFrame(NSRect::new(NSPoint::new(8.0, filter_y), NSSize::new(filter_w, 22.0)));
        self.agents_btn.setFrame(NSRect::new(NSPoint::new((w - 28.0).max(52.0), filter_y), NSSize::new(22.0, 22.0)));

        // Bottom 2-row utility footer.
        self.footer.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, UTILITY_FOOTER_H)));

        self.tree.rebuild_rows();
        self.relayout_zones();
    }

    fn relayout_zones(&self) {
        let size = self.root.bounds().size;
        let w = size.width;
        let h = size.height;
        let mid_h = (h - TOP_BAR_H - FILTER_BAR_H - UTILITY_FOOTER_H).max(0.0);
        let agents_h = compute_agents_zone_height(
            mid_h,
            self.agents_view.content_height(),
            !self.agents_view.is_empty(),
        );
        self.root_view.ivars().agents_h.set(agents_h);

        if agents_h < 1.0 {
            self.agents_view.setHidden(true);
            self.agents_view.setFrame(NSRect::ZERO);
        } else {
            self.agents_view.setHidden(false);
            self.agents_view.setFrame(NSRect::new(
                NSPoint::new(0.0, UTILITY_FOOTER_H),
                NSSize::new(w, agents_h),
            ));
        }

        let spaces_y = UTILITY_FOOTER_H + agents_h;
        let spaces_h = (mid_h - agents_h).max(0.0);
        self.tree.setFrame(NSRect::new(NSPoint::new(0.0, spaces_y), NSSize::new(w, spaces_h)));
        self.root_view.setNeedsDisplay(true);
    }

    pub fn set_chrome(&self, style: ChromeStyle) {
        self.root_view.ivars().chrome.set(style);
        self.tree.ivars().chrome.set(style);
        self.agents_view.ivars().chrome.set(style);
        self.footer.ivars().chrome.set(style);
        self.root_view.setNeedsDisplay(true);
        self.tree.setNeedsDisplay(true);
        self.agents_view.setNeedsDisplay(true);
        self.footer.setNeedsDisplay(true);
    }

    pub fn set_visible(&self, visible: bool) {
        if self.visible.get() != visible {
            self.visible.set(visible);
            self.sync_agent_timer();
        }
    }

    pub fn tick_agents(&self) {
        self.agents_view.tick();
    }

    fn sync_agent_timer(&self) {
        let counts = self.agents_view.counts();
        let desired = if !self.visible.get() {
            None
        } else if counts.working > 0 {
            Some(0.25)
        } else if counts.blocked + counts.error > 0 {
            Some(1.0)
        } else {
            None
        };

        let same = self
            .agent_timer
            .borrow()
            .as_ref()
            .is_some_and(|(cur, _)| Some(*cur) == desired);
        if same {
            return;
        }
        if let Some((_, old)) = self.agent_timer.borrow_mut().take() {
            old.invalidate();
        }
        if let Some(interval) = desired {
            let win_id = self.window_id;
            let block = RcBlock::new(move |_t: std::ptr::NonNull<NSTimer>| {
                with_window(win_id, |w| w.tick_sidebar_agents());
            });
            let timer = unsafe {
                NSTimer::scheduledTimerWithTimeInterval_repeats_block(interval, true, &block)
            };
            *self.agent_timer.borrow_mut() = Some((interval, timer));
        }
    }

    pub fn set_hosts(
        &self,
        hosts: Vec<HostView>,
        active: Option<(String, String)>,
        active_block: Option<String>,
    ) {
        if let Some((ref h, _)) = active {
            *self.root_view.ivars().active_host.borrow_mut() = h.clone();
        }

        if let Some(local) = hosts.iter().find(|h| h.is_local) {
            let (health, tip) = match &local.status {
                HostStatus::Connected { info, .. } => {
                    let (s, b) = local
                        .snapshot
                        .as_ref()
                        .map(|sn| (sn.sessions.len(), sn.blocks.len()))
                        .unwrap_or((0, 0));
                    (
                        2u8,
                        format!("Local server running (pid {}, {} sessions, {} blocks)", info.pid, s, b),
                    )
                }
                HostStatus::Connecting => (1u8, "Local server starting…".into()),
                HostStatus::Disconnected { error } => (0u8, format!("Local server offline: {error}")),
            };
            self.footer.set_server_health(health, tip);
        }

        let remote_connected = hosts
            .iter()
            .filter(|h| !h.is_local && matches!(h.status, HostStatus::Connected { .. }))
            .count();
        let hosts_color = if remote_connected > 0 {
            NSColor::controlAccentColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        self.hosts_btn.setContentTintColor(Some(&hosts_color));

        let filter = self.tree.ivars().filter.borrow().clone();
        self.agents_view.set_agents(
            &hosts,
            &filter,
            active.as_ref(),
            active_block.as_deref(),
        );
        *self.tree.ivars().hosts.borrow_mut() = hosts;
        *self.tree.ivars().active_target.borrow_mut() = active;
        self.update_agents_filter_button();
        self.tree.rebuild_rows();
        self.relayout_zones();
        self.sync_agent_timer();
    }

    pub fn toggle_agents_only(&self) {
        let next = !self.tree.ivars().agents_only.get();
        self.tree.ivars().agents_only.set(next);
        self.tree.ivars().scroll_y.set(0.0);
        self.update_agents_filter_button();
        self.tree.rebuild_rows();
    }

    pub fn set_filter(&self, query: String) {
        *self.tree.ivars().filter.borrow_mut() = query.clone();
        self.tree.ivars().scroll_y.set(0.0);
        self.agents_view.ivars().scroll_y.set(0.0);
        self.tree.rebuild_rows();
        {
            let hosts = self.tree.ivars().hosts.borrow();
            self.agents_view.refilter(&hosts, &query);
        }
        self.relayout_zones();
        self.sync_agent_timer();
    }

    fn update_agents_filter_button(&self) {
        let on = self.tree.ivars().agents_only.get();
        let hosts = self.tree.ivars().hosts.borrow();
        let entries = super::agents::collect_from_host_views(&hosts, &[]);
        let counts = AgentCounts::from_entries(&entries);
        let color = if counts.blocked > 0 {
            NSColor::systemOrangeColor()
        } else if counts.error > 0 {
            NSColor::systemRedColor()
        } else if on || counts.working > 0 {
            NSColor::controlAccentColor()
        } else if counts.done > 0 {
            NSColor::systemGreenColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        self.agents_btn.setContentTintColor(Some(&color));
        let mode = if on { "Agents filter: ON" } else { "Filter sidebar to agents" };
        let tip = format!(
            "{mode} ({} · ⌘⇧A for Mission Control)",
            super::agents::summary_line(counts)
        );
        self.agents_btn.setToolTip(Some(&NSString::from_str(&tip)));
    }
}

fn install_tracking_area(view: &NSView) {
    let tracking = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect,
            Some(view),
            None,
        )
    };
    view.addTrackingArea(&tracking);
}

fn set_resize_cursor() {
    NSCursor::columnResizeCursorInDirections(objc2_app_kit::NSHorizontalDirections::All).set();
}

fn drag_sidebar_width(win_id: u64, event: &NSEvent) {
    let x = event.locationInWindow().x.clamp(MIN_WIDTH, MAX_WIDTH);
    with_window(win_id, |w| w.set_sidebar_width(x));
}

fn fill_rounded_rect(rect: NSRect, radius: f64, color: &NSColor) {
    color.setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius).fill();
}

fn stroke_rounded_rect(rect: NSRect, radius: f64, color: &NSColor, width: f64) {
    color.setStroke();
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius);
    path.setLineWidth(width);
    path.stroke();
}

fn srgb_alpha(rgb: [u8; 3], alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        rgb[0] as f64 / 255.0,
        rgb[1] as f64 / 255.0,
        rgb[2] as f64 / 255.0,
        alpha,
    )
}

fn symbol_button(
    mtm: MainThreadMarker,
    symbol: &str,
    tooltip: &str,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSButton> {
    let icon = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        Some(&NSString::from_str(tooltip)),
    );
    let btn = match icon {
        Some(img) => unsafe { NSButton::buttonWithImage_target_action(&img, Some(target), Some(action), mtm) },
        None => unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("•"), Some(target), Some(action), mtm)
        },
    };
    btn.setBordered(false);
    btn.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
    btn.setToolTip(Some(&NSString::from_str(tooltip)));
    btn
}

fn pick_window_status<'a>(statuses: impl Iterator<Item = &'a ProgramStatus>) -> Option<ProgramStatus> {
    fn rank(s: ProgramState) -> u8 {
        match s {
            ProgramState::Blocked => 5,
            ProgramState::Error => 4,
            ProgramState::Working => 3,
            ProgramState::Done => 2,
            ProgramState::Idle => 1,
        }
    }
    statuses.max_by_key(|s| rank(s.state)).cloned()
}

fn rep_string(sender: Option<&AnyObject>) -> Option<String> {
    let item = sender?.downcast_ref::<NSMenuItem>()?;
    let obj = item.representedObject()?;
    obj.downcast_ref::<NSString>().map(|s| s.to_string())
}

fn rep_pair(sender: Option<&AnyObject>) -> Option<(String, String)> {
    let s = rep_string(sender)?;
    let (a, b) = s.split_once('\u{1F}')?;
    Some((a.to_string(), b.to_string()))
}

#[derive(Clone, Copy)]
enum FontWeight {
    Regular,
    Medium,
    Semibold,
}

fn draw_text_aligned(
    text: &str,
    size: f64,
    weight: FontWeight,
    mono_digits: bool,
    color: &NSColor,
    align: NSTextAlignment,
    rect: NSRect,
) {
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(align);
    para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let w = unsafe {
        match weight {
            FontWeight::Regular => NSFontWeightRegular,
            FontWeight::Medium => NSFontWeightMedium,
            FontWeight::Semibold => NSFontWeightSemibold,
        }
    };
    let font = if mono_digits {
        NSFont::monospacedDigitSystemFontOfSize_weight(size as CGFloat, w)
    } else {
        NSFont::systemFontOfSize_weight(size as CGFloat, w)
    };
    let keys: [&NSAttributedStringKey; 3] =
        unsafe { [NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName] };
    let values: [&AnyObject; 3] = [font.as_ref(), color.as_ref(), para.as_ref()];
    let attrs = NSDictionary::from_slices(&keys, &values);
    let s = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attrs),
        )
    };
    s.drawInRect(rect);
}

fn draw_text_left(text: &str, size: f64, weight: FontWeight, color: &NSColor, rect: NSRect) {
    draw_text_aligned(text, size, weight, false, color, NSTextAlignment::Left, rect);
}

fn draw_text_right(text: &str, size: f64, weight: FontWeight, color: &NSColor, rect: NSRect) {
    draw_text_aligned(text, size, weight, false, color, NSTextAlignment::Right, rect);
}

fn draw_text_centered(text: &str, size: f64, medium: bool, color: &NSColor, rect: NSRect) {
    let w = if medium { FontWeight::Medium } else { FontWeight::Regular };
    draw_text_aligned(text, size, w, false, color, NSTextAlignment::Center, rect);
}

fn draw_symbol(name: &str, rect: NSRect, color: &NSColor) {
    let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    ) else {
        return;
    };
    let tinted = img.imageWithSymbolConfiguration(
        &NSImageSymbolConfiguration::configurationWithHierarchicalColor(color),
    );
    let draw_img = tinted.as_deref().unwrap_or(&img);
    unsafe {
        draw_img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            rect,
            NSRect::ZERO,
            objc2_app_kit::NSCompositingOperation::SourceOver,
            1.0,
            true,
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_zone_layout_heights_clamp_cleanly() {
        // Too short mid section collapses Live Agents completely.
        assert_eq!(compute_agents_zone_height(80.0, 120.0, true), 0.0);

        // Empty Live Agents reserves a compact placeholder strip.
        assert_eq!(compute_agents_zone_height(600.0, 0.0, false), EMPTY_AGENTS_H);

        // Small number of agents size to content + header + breathing room.
        assert_eq!(compute_agents_zone_height(600.0, 46.0, true), AGENTS_HEADER_H + 46.0 + 6.0);

        // Many agents cap at 44% so Spaces retain the majority of sidebar height.
        let capped = compute_agents_zone_height(600.0, 500.0, true);
        assert!((capped - 264.0).abs() < 0.01);
        assert!(600.0 - capped >= 336.0);

        // Tight vertical height preserves at least 60pt for Spaces.
        assert_eq!(compute_agents_zone_height(110.0, 200.0, true), 50.0);
    }
}
