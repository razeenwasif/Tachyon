//! Floating Liquid Glass **Workspace Switcher** (`⌘⇧W` or Command Palette).
//!
//! Combines live Mux v2 workspaces/windows across `This Mac` and connected
//! remote hosts (`kagutsuchi`, `amaterasu`, Linux servers) with saved
//! `~/.config/Tachyon/workspaces/*.json` templates in a fast keyboard-driven
//! card matching Agent Mission Control (`⌘⇧A`) and Command Palette (`⌘⇧P`).

use std::cell::{Cell, RefCell};
use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezelStyle, NSBezierPath, NSButton, NSColor,
    NSCompositingOperation, NSControl, NSControlSize, NSControlTextEditingDelegate, NSEvent,
    NSFont, NSFontAttributeName, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSGlassEffectView, NSImage, NSImageSymbolConfiguration,
    NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder,
    NSSearchField, NSSearchFieldDelegate, NSSegmentStyle, NSSegmentedControl, NSTextAlignment,
    NSTextField, NSTextFieldDelegate, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use super::host_mgr::{HostStatus, HostView};
use super::renderer::chrome_tokens;
use super::with_window;
use crate::config::ChromeStyle;
use crate::mux::model::{git_info_for_dir, resolve_host_color, space_monogram, HostColor};
use crate::mux::workspace::{list_workspaces, WorkspaceSummary};
use crate::term::ProgramState;

const ROW_H: f64 = 52.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SwitcherFilter {
    #[default]
    All,
    Live,
    Templates,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitcherKind {
    LiveWindow {
        host: String,
        host_display: String,
        host_color: Option<HostColor>,
        session_id: String,
        session_name: String,
        window_id: String,
        title: String,
        pane_count: usize,
        git_branch: Option<String>,
        git_dirty: bool,
        is_worktree: bool,
        blocked_count: usize,
        working: bool,
        active: bool,
    },
    Template {
        name: String,
        description: Option<String>,
        hosts: Vec<String>,
        window_count: usize,
        pane_count: usize,
        running: bool,
    },
}

/// Collect and rank all live Mux v2 windows across hosts and saved workspace
/// templates for the `⌘⇧W` Workspace Switcher.
pub fn collect_switcher_items(
    hosts: &[HostView],
    templates: &[WorkspaceSummary],
    color_rules: &[(String, String)],
    active_target: Option<&(String, String)>,
) -> Vec<SwitcherKind> {
    let mut live: Vec<(usize, SwitcherKind)> = Vec::new();
    for (host_idx, h) in hosts.iter().enumerate() {
        let Some(snap) = &h.snapshot else { continue };
        let os = match &h.status {
            HostStatus::Connected { info, .. } => Some(info.os.as_str()),
            _ => None,
        };
        let host_color = if h.is_local {
            None
        } else {
            resolve_host_color(&h.alias, os, h.color.as_deref(), color_rules)
        };
        for sess in &snap.sessions {
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
                let (git_branch, git_dirty, is_worktree) = if h.is_local {
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
                    match cwd.and_then(|dir| git_info_for_dir(Path::new(dir))) {
                        Some(info) => {
                            let dirty = super::sidebar::query_git_dirty(&info.worktree_root, 0);
                            (Some(info.branch), dirty, info.is_linked_worktree)
                        }
                        None => (None, false, false),
                    }
                } else {
                    (None, false, false)
                };
                let blocked_count = snap
                    .blocks
                    .iter()
                    .filter(|b| leaves.contains(&b.id))
                    .filter(|b| b.status.as_ref().is_some_and(|s| s.state == ProgramState::Blocked))
                    .count();
                let working = snap.blocks.iter().any(|b| {
                    leaves.contains(&b.id)
                        && (b.command_running
                            || b.status.as_ref().is_some_and(|s| s.state == ProgramState::Working))
                });
                let active = active_target
                    .is_some_and(|(ah, aw)| ah == &h.alias && aw == &win.id);

                live.push((
                    host_idx,
                    SwitcherKind::LiveWindow {
                        host: h.alias.clone(),
                        host_display: h.display_name.clone(),
                        host_color,
                        session_id: sess.id.clone(),
                        session_name: sess.name.clone(),
                        window_id: win.id.clone(),
                        title,
                        pane_count: leaves.len().max(1),
                        git_branch,
                        git_dirty,
                        is_worktree,
                        blocked_count,
                        working,
                        active,
                    },
                ));
            }
        }
    }

    live.sort_by(|(ha, a), (hb, b)| match (a, b) {
        (
            SwitcherKind::LiveWindow {
                active: aa,
                blocked_count: ba,
                working: wa,
                session_name: sa,
                title: ta,
                ..
            },
            SwitcherKind::LiveWindow {
                active: ab,
                blocked_count: bb,
                working: wb,
                session_name: sb,
                title: tb,
                ..
            },
        ) => ab
            .cmp(aa)
            .then_with(|| (*bb > 0).cmp(&(*ba > 0)))
            .then_with(|| bb.cmp(ba))
            .then_with(|| wb.cmp(wa))
            .then_with(|| ha.cmp(hb))
            .then_with(|| sa.cmp(sb))
            .then_with(|| ta.cmp(tb)),
        _ => std::cmp::Ordering::Equal,
    });

    let mut tmpls: Vec<SwitcherKind> = templates
        .iter()
        .map(|t| {
            let running = t.session_names.iter().any(|(h_alias, s_name)| {
                hosts.iter().any(|hv| {
                    hv.alias == *h_alias
                        && hv
                            .snapshot
                            .as_ref()
                            .is_some_and(|sn| sn.sessions.iter().any(|s| s.name == *s_name))
                })
            });
            SwitcherKind::Template {
                name: t.name.clone(),
                description: t.description.clone(),
                hosts: t.hosts.clone(),
                window_count: t.window_count,
                pane_count: t.pane_count,
                running,
            }
        })
        .collect();

    tmpls.sort_by(|a, b| match (a, b) {
        (
            SwitcherKind::Template { running: ra, name: na, .. },
            SwitcherKind::Template { running: rb, name: nb, .. },
        ) => rb.cmp(ra).then_with(|| na.to_ascii_lowercase().cmp(&nb.to_ascii_lowercase())),
        _ => std::cmp::Ordering::Equal,
    });

    let mut out: Vec<SwitcherKind> = live.into_iter().map(|(_, item)| item).collect();
    out.extend(tmpls);
    out
}

/// Filter Workspace Switcher items by tab (`All` / `Live` / `Templates`) and query.
pub fn filter_switcher_items(
    items: &[SwitcherKind],
    filter: SwitcherFilter,
    query: &str,
) -> Vec<SwitcherKind> {
    let q = query.trim().to_ascii_lowercase();
    let terms: Vec<&str> = q.split_whitespace().collect();
    items
        .iter()
        .filter(|item| match filter {
            SwitcherFilter::All => true,
            SwitcherFilter::Live => matches!(item, SwitcherKind::LiveWindow { .. }),
            SwitcherFilter::Templates => matches!(item, SwitcherKind::Template { .. }),
        })
        .filter(|item| {
            if terms.is_empty() {
                return true;
            }
            let hay = match item {
                SwitcherKind::LiveWindow {
                    host,
                    host_display,
                    session_name,
                    title,
                    git_branch,
                    ..
                } => format!(
                    "{} {} {} {} {}",
                    host,
                    host_display,
                    session_name,
                    title,
                    git_branch.as_deref().unwrap_or("")
                ),
                SwitcherKind::Template { name, description, hosts, .. } => {
                    format!(
                        "{} {} {}",
                        name,
                        description.as_deref().unwrap_or(""),
                        hosts.join(" ")
                    )
                }
            }
            .to_ascii_lowercase();
            terms.iter().all(|t| hay.contains(t))
        })
        .cloned()
        .collect()
}

#[derive(Default)]
struct ListState {
    window: Cell<u64>,
    chrome: Cell<ChromeStyle>,
    all: RefCell<Vec<SwitcherKind>>,
    filtered: RefCell<Vec<SwitcherKind>>,
    filter: Cell<SwitcherFilter>,
    query: RefCell<String>,
    selected: Cell<usize>,
    hover_stop: Cell<Option<usize>>,
    scroll_y: Cell<f64>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonWorkspacesListView"]
    #[ivars = ListState]
    struct WorkspacesListView;

    impl WorkspacesListView {
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
            let content_h = self.ivars().filtered.borrow().len() as f64 * ROW_H;
            let max_scroll = (content_h - self.bounds().size.height).max(0.0);
            let next = (self.ivars().scroll_y.get() - dy).clamp(0.0, max_scroll);
            if (next - self.ivars().scroll_y.get()).abs() > 0.1 {
                self.ivars().scroll_y.set(next);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let w = self.bounds().size.width;
            let idx = ((p.y + self.ivars().scroll_y.get()) / ROW_H).floor() as isize;
            let filtered = self.ivars().filtered.borrow();
            let mut changed = false;
            if idx >= 0 && (idx as usize) < filtered.len() {
                let u = idx as usize;
                if self.ivars().selected.get() != u {
                    self.ivars().selected.set(u);
                    changed = true;
                }
                let stop_hit = matches!(
                    &filtered[u],
                    SwitcherKind::Template { running: true, .. }
                ) && p.x >= w - 58.0
                    && p.x <= w - 12.0;
                let next_stop = stop_hit.then_some(u);
                if self.ivars().hover_stop.get() != next_stop {
                    self.ivars().hover_stop.set(next_stop);
                    changed = true;
                }
            } else if self.ivars().hover_stop.get().is_some() {
                self.ivars().hover_stop.set(None);
                changed = true;
            }
            if changed {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            if self.ivars().hover_stop.get().take().is_some() {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let w = self.bounds().size.width;
            let idx = ((p.y + self.ivars().scroll_y.get()) / ROW_H).floor() as isize;
            if idx < 0 {
                return;
            }
            let u = idx as usize;
            let item = self.ivars().filtered.borrow().get(u).cloned();
            let Some(item) = item else { return };
            let win_id = self.ivars().window.get();
            if let SwitcherKind::Template { name, running: true, .. } = &item {
                if p.x >= w - 58.0 && p.x <= w - 12.0 {
                    super::mux_workspace_down(name);
                    with_window(win_id, |w| w.refresh_workspaces_card());
                    return;
                }
            }
            activate_item(win_id, &item);
        }
    }
);

impl WorkspacesListView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = ListState::default();
        state.window.set(window);
        let view: Retained<Self> =
            unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), initWithFrame: NSRect::ZERO] };
        view.setClipsToBounds(true);
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking);
        view
    }

    fn set_all(&self, items: Vec<SwitcherKind>) {
        *self.ivars().all.borrow_mut() = items;
        self.recompute(false);
    }

    fn set_filter(&self, filter: SwitcherFilter) {
        self.ivars().filter.set(filter);
        self.recompute(true);
    }

    fn set_query(&self, query: String) {
        *self.ivars().query.borrow_mut() = query;
        self.recompute(true);
    }

    fn recompute(&self, reset_scroll: bool) {
        let filtered = filter_switcher_items(
            &self.ivars().all.borrow(),
            self.ivars().filter.get(),
            &self.ivars().query.borrow(),
        );
        let len = filtered.len();
        *self.ivars().filtered.borrow_mut() = filtered;
        if reset_scroll {
            self.ivars().selected.set(0);
            self.ivars().scroll_y.set(0.0);
        } else if len == 0 {
            self.ivars().selected.set(0);
        } else if self.ivars().selected.get() >= len {
            self.ivars().selected.set(len - 1);
        }
        self.ivars().hover_stop.set(None);
        self.setNeedsDisplay(true);
    }

    fn move_selection(&self, delta: isize) {
        let len = self.ivars().filtered.borrow().len();
        if len == 0 {
            return;
        }
        let cur = self.ivars().selected.get() as isize;
        let next = (cur + delta).clamp(0, len as isize - 1) as usize;
        self.ivars().selected.set(next);

        let top = next as f64 * ROW_H;
        let bot = top + ROW_H;
        let scroll = self.ivars().scroll_y.get();
        let h = self.bounds().size.height;
        if top < scroll {
            self.ivars().scroll_y.set(top);
        } else if bot > scroll + h {
            self.ivars().scroll_y.set((bot - h).max(0.0));
        }
        self.setNeedsDisplay(true);
    }

    fn selected_item(&self) -> Option<SwitcherKind> {
        let idx = self.ivars().selected.get();
        self.ivars().filtered.borrow().get(idx).cloned()
    }

    fn draw(&self) {
        let b = self.bounds();
        NSBezierPath::clipRect(b);
        let w = b.size.width;
        let scroll = self.ivars().scroll_y.get();
        let selected = self.ivars().selected.get();
        let hover_stop = self.ivars().hover_stop.get();
        let minimal = self.ivars().chrome.get().is_minimal();
        let items = self.ivars().filtered.borrow();

        if items.is_empty() {
            draw_centered(
                "No live workspaces or templates match",
                12.5,
                FontWeight::Regular,
                &NSColor::secondaryLabelColor(),
                NSRect::new(NSPoint::new(16.0, (b.size.height / 2.0) - 12.0), NSSize::new(w - 32.0, 22.0)),
            );
            return;
        }

        for (i, item) in items.iter().enumerate() {
            let ry = i as f64 * ROW_H - scroll;
            if ry + ROW_H < 0.0 || ry > b.size.height {
                continue;
            }
            if i == selected {
                let hl = if minimal {
                    srgb_alpha([255, 255, 255], 0.09)
                } else {
                    NSColor::controlAccentColor().colorWithAlphaComponent(0.14)
                };
                hl.setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    NSRect::new(NSPoint::new(6.0, ry + 2.0), NSSize::new((w - 12.0).max(0.0), ROW_H - 4.0)),
                    8.0,
                    8.0,
                )
                .fill();
            }

            match item {
                SwitcherKind::LiveWindow {
                    host_display,
                    host_color,
                    session_name,
                    title,
                    pane_count,
                    git_branch,
                    git_dirty,
                    is_worktree,
                    blocked_count,
                    working,
                    active,
                    ..
                } => {
                    // 24x24 monogram avatar on left.
                    let av_rect = NSRect::new(NSPoint::new(14.0, ry + 14.0), NSSize::new(24.0, 24.0));
                    let (av_bg, av_fg) = match host_color {
                        Some(hc) => (srgb_alpha(hc.rgb, 0.20), srgb_alpha(hc.rgb, 0.95)),
                        None if minimal => (srgb_alpha([0x24, 0x26, 0x2B], 1.0), srgb_alpha([0xD2, 0xD4, 0xD9], 0.95)),
                        None => (NSColor::labelColor().colorWithAlphaComponent(0.10), NSColor::labelColor()),
                    };
                    av_bg.setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(av_rect, 6.0, 6.0).fill();
                    let mono = space_monogram(title);
                    draw_centered(
                        &mono,
                        12.0,
                        FontWeight::Semibold,
                        &av_fg,
                        NSRect::new(NSPoint::new(14.0, ry + 17.5), NSSize::new(24.0, 18.0)),
                    );

                    // Top row right-aligned pills.
                    let mut right_x = w - 14.0;
                    if *active {
                        let pw = 52.0;
                        right_x -= pw;
                        let p_rect = NSRect::new(NSPoint::new(right_x, ry + 10.0), NSSize::new(pw, 16.0));
                        let col = if minimal { srgb_alpha([0x76, 0xA8, 0xFF], 1.0) } else { NSColor::controlAccentColor() };
                        col.colorWithAlphaComponent(0.18).setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(p_rect, 8.0, 8.0).fill();
                        draw_centered("ACTIVE", 9.5, FontWeight::Semibold, &col, p_rect);
                        right_x -= 6.0;
                    }
                    if *blocked_count > 0 {
                        let txt = format!("{blocked_count} blocked");
                        let pw = ((txt.len() as f64) * 5.8 + 12.0).max(56.0);
                        right_x -= pw;
                        let p_rect = NSRect::new(NSPoint::new(right_x, ry + 10.0), NSSize::new(pw, 16.0));
                        let amber = if minimal { srgb_alpha([0xF2, 0xA3, 0x3C], 1.0) } else { NSColor::systemOrangeColor() };
                        amber.colorWithAlphaComponent(0.20).setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(p_rect, 8.0, 8.0).fill();
                        draw_centered(&txt, 9.5, FontWeight::Semibold, &amber, p_rect);
                        right_x -= 6.0;
                    } else if *working {
                        let dot_col = if minimal { srgb_alpha([0x76, 0xA8, 0xFF], 1.0) } else { NSColor::controlAccentColor() };
                        dot_col.setFill();
                        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                            NSPoint::new(right_x - 8.0, ry + 15.0),
                            NSSize::new(6.0, 6.0),
                        ))
                        .fill();
                        right_x -= 13.0;
                    }
                    if *pane_count > 1 {
                        let txt = format!("{pane_count} panes");
                        let pw = ((txt.len() as f64) * 5.6 + 10.0).max(46.0);
                        right_x -= pw;
                        let p_rect = NSRect::new(NSPoint::new(right_x, ry + 10.0), NSSize::new(pw, 16.0));
                        NSColor::labelColor().colorWithAlphaComponent(0.08).setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(p_rect, 8.0, 8.0).fill();
                        draw_centered(&txt, 9.5, FontWeight::Medium, &NSColor::secondaryLabelColor(), p_rect);
                        right_x -= 6.0;
                    }

                    let title_fg = if minimal { srgb_alpha([0xEE, 0xEE, 0xEF], 1.0) } else { NSColor::labelColor() };
                    draw_left(
                        title,
                        13.0,
                        FontWeight::Semibold,
                        &title_fg,
                        NSRect::new(NSPoint::new(46.0, ry + 8.0), NSSize::new((right_x - 46.0).max(60.0), 18.0)),
                    );

                    // Subtitle + right-aligned git branch.
                    let branch_w = if git_branch.is_some() { 136.0 } else { 0.0 };
                    let sub_w = (w - 46.0 - 14.0 - branch_w).max(60.0);
                    draw_left(
                        &format!("{host_display} · {session_name}"),
                        11.0,
                        FontWeight::Regular,
                        &NSColor::secondaryLabelColor(),
                        NSRect::new(NSPoint::new(46.0, ry + 28.0), NSSize::new(sub_w, 16.0)),
                    );
                    if let Some(br) = git_branch {
                        let txt = match (*is_worktree, *git_dirty) {
                            (true, true) => format!("⎇ {br}*"),
                            (true, false) => format!("⎇ {br}"),
                            (false, true) => format!("{br}*"),
                            (false, false) => br.clone(),
                        };
                        let col = if *git_dirty {
                            if minimal {
                                srgb_alpha([0xF2, 0xA3, 0x3C], 0.92)
                            } else {
                                NSColor::systemOrangeColor().colorWithAlphaComponent(0.92)
                            }
                        } else {
                            NSColor::tertiaryLabelColor()
                        };
                        draw_right(
                            &txt,
                            10.5,
                            FontWeight::Regular,
                            &col,
                            NSRect::new(NSPoint::new((w - branch_w - 14.0).max(120.0), ry + 29.0), NSSize::new(branch_w, 15.0)),
                        );
                    }
                }

                SwitcherKind::Template {
                    name,
                    description,
                    hosts,
                    window_count,
                    pane_count,
                    running,
                    ..
                } => {
                    let av_rect = NSRect::new(NSPoint::new(14.0, ry + 14.0), NSSize::new(24.0, 24.0));
                    let av_bg = if *running {
                        NSColor::systemGreenColor().colorWithAlphaComponent(0.16)
                    } else if minimal {
                        srgb_alpha([0x24, 0x26, 0x2B], 1.0)
                    } else {
                        NSColor::labelColor().colorWithAlphaComponent(0.10)
                    };
                    av_bg.setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(av_rect, 6.0, 6.0).fill();
                    let sym_col = if *running { NSColor::systemGreenColor() } else { NSColor::secondaryLabelColor() };
                    draw_symbol(
                        "square.grid.2x2",
                        NSRect::new(NSPoint::new(19.0, ry + 19.0), NSSize::new(14.0, 14.0)),
                        &sym_col,
                    );

                    let mut right_x = w - 14.0;
                    if *running {
                        let sw = 44.0;
                        right_x -= sw;
                        let s_rect = NSRect::new(NSPoint::new(right_x, ry + 9.0), NSSize::new(sw, 18.0));
                        let stop_hov = hover_stop == Some(i);
                        NSColor::systemRedColor()
                            .colorWithAlphaComponent(if stop_hov { 0.28 } else { 0.14 })
                            .setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(s_rect, 6.0, 6.0).fill();
                        draw_centered("Stop", 10.0, FontWeight::Semibold, &NSColor::systemRedColor(), s_rect);
                        right_x -= 6.0;
                    }

                    let (tag, tag_col) = if *running {
                        ("RUNNING", if minimal { srgb_alpha([0x4C, 0xC4, 0x8C], 1.0) } else { NSColor::systemGreenColor() })
                    } else {
                        ("TEMPLATE", NSColor::secondaryLabelColor())
                    };
                    let tw = if *running { 60.0 } else { 64.0 };
                    right_x -= tw;
                    let t_rect = NSRect::new(NSPoint::new(right_x, ry + 10.0), NSSize::new(tw, 16.0));
                    tag_col.colorWithAlphaComponent(0.16).setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(t_rect, 8.0, 8.0).fill();
                    draw_centered(tag, 9.5, FontWeight::Semibold, &tag_col, t_rect);
                    right_x -= 6.0;

                    let title_fg = if minimal { srgb_alpha([0xEE, 0xEE, 0xEF], 1.0) } else { NSColor::labelColor() };
                    draw_left(
                        name,
                        13.0,
                        FontWeight::Semibold,
                        &title_fg,
                        NSRect::new(NSPoint::new(46.0, ry + 8.0), NSSize::new((right_x - 46.0).max(60.0), 18.0)),
                    );

                    let summary = description
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| format!("{window_count} windows · {pane_count} panes"));
                    let hosts_str = hosts.join(", ");
                    let hosts_w = if hosts_str.is_empty() { 0.0 } else { 136.0 };
                    let sub_w = (w - 46.0 - 14.0 - hosts_w).max(60.0);
                    draw_left(
                        &summary,
                        11.0,
                        FontWeight::Regular,
                        &NSColor::secondaryLabelColor(),
                        NSRect::new(NSPoint::new(46.0, ry + 28.0), NSSize::new(sub_w, 16.0)),
                    );
                    if !hosts_str.is_empty() {
                        draw_right(
                            &hosts_str,
                            10.5,
                            FontWeight::Regular,
                            &NSColor::tertiaryLabelColor(),
                            NSRect::new(NSPoint::new((w - hosts_w - 14.0).max(120.0), ry + 29.0), NSSize::new(hosts_w, 15.0)),
                        );
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct CardDelegateState {
    window: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonWorkspacesDelegate"]
    #[ivars = CardDelegateState]
    struct WorkspacesDelegate;

    unsafe impl NSObjectProtocol for WorkspacesDelegate {}
    unsafe impl NSControlTextEditingDelegate for WorkspacesDelegate {
        #[unsafe(method(controlTextDidChange:))]
        fn text_did_change(&self, _note: &NSNotification) {
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| {
                if let Some(c) = &w.workspaces_card {
                    let q = c.search.stringValue().to_string();
                    c.list.set_query(q);
                }
            });
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(
            &self,
            _control: &NSControl,
            _tv: &objc2_app_kit::NSTextView,
            sel: objc2::runtime::Sel,
        ) -> bool {
            let win_id = self.ivars().window.get();
            if sel == sel!(cancelOperation:) {
                with_window(win_id, |w| {
                    w.close_workspaces_card();
                    w.focus_terminal();
                });
                true
            } else if sel == sel!(moveDown:) {
                with_window(win_id, |w| {
                    if let Some(c) = &w.workspaces_card {
                        c.list.move_selection(1);
                    }
                });
                true
            } else if sel == sel!(moveUp:) {
                with_window(win_id, |w| {
                    if let Some(c) = &w.workspaces_card {
                        c.list.move_selection(-1);
                    }
                });
                true
            } else if sel == sel!(insertNewline:) {
                let mut picked = None;
                with_window(win_id, |w| {
                    if let Some(c) = &w.workspaces_card {
                        picked = c.list.selected_item();
                    }
                });
                if let Some(item) = picked {
                    activate_item(win_id, &item);
                } else {
                    with_window(win_id, |w| {
                        w.close_workspaces_card();
                        w.focus_terminal();
                    });
                }
                true
            } else {
                false
            }
        }
    }
    unsafe impl NSTextFieldDelegate for WorkspacesDelegate {}
    unsafe impl NSSearchFieldDelegate for WorkspacesDelegate {}

    impl WorkspacesDelegate {
        #[unsafe(method(wsFilterChanged:))]
        fn filter_changed(&self, sender: Option<&AnyObject>) {
            let idx = sender
                .and_then(|s| s.downcast_ref::<NSSegmentedControl>())
                .map(|seg| seg.selectedSegment())
                .unwrap_or(0);
            let f = match idx {
                1 => SwitcherFilter::Live,
                2 => SwitcherFilter::Templates,
                _ => SwitcherFilter::All,
            };
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| {
                if let Some(c) = &w.workspaces_card {
                    c.list.set_filter(f);
                }
            });
        }

        #[unsafe(method(wsClose:))]
        fn close_card(&self, _sender: Option<&AnyObject>) {
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| {
                w.close_workspaces_card();
                w.focus_terminal();
            });
        }

        #[unsafe(method(wsSaveCurrent:))]
        fn save_current(&self, _sender: Option<&AnyObject>) {
            let win_id = self.ivars().window.get();
            if let Err(e) = super::mux_workspace_save(win_id, false) {
                crate::log::force(&format!("workspace save failed: {e}"));
            } else {
                with_window(win_id, |w| w.refresh_workspaces_card());
            }
        }

        #[unsafe(method(wsOpenSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| w.close_workspaces_card());
            super::open_settings_section(5);
        }
    }
);

impl WorkspacesDelegate {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = CardDelegateState::default();
        state.window.set(window);
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), init] }
    }
}

pub struct WorkspacesCard {
    pub host: Retained<NSView>,
    glass: Option<Retained<NSGlassEffectView>>,
    content: Retained<NSView>,
    title: Retained<NSTextField>,
    summary: Retained<NSTextField>,
    subtitle: Retained<NSTextField>,
    close_btn: Retained<NSButton>,
    tabs: Retained<NSSegmentedControl>,
    pub search: Retained<NSSearchField>,
    list: Retained<WorkspacesListView>,
    footer_hint: Retained<NSTextField>,
    save_btn: Retained<NSButton>,
    manage_btn: Retained<NSButton>,
    _delegate: Retained<WorkspacesDelegate>,
}

impl WorkspacesCard {
    const WIDTH: f64 = 560.0;
    const HEIGHT: f64 = 430.0;

    pub fn new(mtm: MainThreadMarker, parent: &NSView, window: u64) -> Self {
        let content = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let (host, glass): (Retained<NSView>, Option<Retained<NSGlassEffectView>>) =
            if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some() {
                let g = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), NSRect::ZERO);
                g.setCornerRadius(16.0);
                g.setTintColor(Some(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.90)));
                g.setContentView(Some(&content));
                let hv: Retained<NSView> = Retained::into_super(g.clone());
                (hv, Some(g))
            } else {
                let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), NSRect::ZERO);
                blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
                blur.setState(NSVisualEffectState::Active);
                blur.addSubview(&content);
                (Retained::into_super(blur), None)
            };

        let delegate = WorkspacesDelegate::new(mtm, window);
        let target: &AnyObject = delegate.as_ref();

        let title = make_label(mtm, "Workspace Switcher", 15.0, true, &NSColor::labelColor());
        let summary = make_label(mtm, "0 live · 0 templates", 11.5, true, &NSColor::secondaryLabelColor());
        summary.setAlignment(NSTextAlignment::Right);
        let subtitle = make_label(
            mtm,
            "Switch live workspaces across hosts or launch saved templates — Return to focus",
            11.0,
            false,
            &NSColor::secondaryLabelColor(),
        );
        subtitle.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);

        let close_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("✕"),
                Some(target),
                Some(sel!(wsClose:)),
                mtm,
            )
        };
        close_btn.setBordered(false);

        let labels = [
            NSString::from_str("All (0)"),
            NSString::from_str("Live (0)"),
            NSString::from_str("Templates (0)"),
        ];
        let label_refs: Vec<&NSString> = labels.iter().map(|s| &**s).collect();
        let tab_arr = objc2_foundation::NSArray::from_slice(&label_refs);
        let tabs = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &tab_arr,
                objc2_app_kit::NSSegmentSwitchTracking::SelectOne,
                Some(target),
                Some(sel!(wsFilterChanged:)),
                mtm,
            )
        };
        tabs.setSegmentStyle(NSSegmentStyle::Rounded);
        tabs.setSelectedSegment(0);

        let search = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::ZERO);
        search.setPlaceholderString(Some(&NSString::from_str(
            "Filter by host, session, branch, or template…",
        )));
        unsafe { search.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };

        let list = WorkspacesListView::new(mtm, window);

        let footer_hint = make_label(
            mtm,
            "↑↓ navigate · ↩ switch / launch · esc close",
            10.5,
            false,
            &NSColor::secondaryLabelColor(),
        );
        let save_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Save Current"),
                Some(target),
                Some(sel!(wsSaveCurrent:)),
                mtm,
            )
        };
        save_btn.setBezelStyle(NSBezelStyle::AccessoryBarAction);
        save_btn.setControlSize(NSControlSize::Small);

        let manage_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Manage…"),
                Some(target),
                Some(sel!(wsOpenSettings:)),
                mtm,
            )
        };
        manage_btn.setBezelStyle(NSBezelStyle::AccessoryBarAction);
        manage_btn.setControlSize(NSControlSize::Small);

        for v in [
            &*list as &NSView,
            &*title as &NSView,
            &*summary as &NSView,
            &*subtitle as &NSView,
            &*close_btn as &NSView,
            &*tabs as &NSView,
            &*search as &NSView,
            &*footer_hint as &NSView,
            &*save_btn as &NSView,
            &*manage_btn as &NSView,
        ] {
            content.addSubview(v);
        }

        host.setHidden(true);
        parent.addSubview(&host);

        Self {
            host,
            glass,
            content,
            title,
            summary,
            subtitle,
            close_btn,
            tabs,
            search,
            list,
            footer_hint,
            save_btn,
            manage_btn,
            _delegate: delegate,
        }
    }

    pub fn set_chrome(&self, style: ChromeStyle) {
        self.list.ivars().chrome.set(style);
        self.list.setNeedsDisplay(true);
        if let Some(glass) = &self.glass {
            let tint = if style.is_minimal() {
                srgb_alpha(chrome_tokens(style, [0x11, 0x12, 0x14]).chrome_bg, 0.96)
            } else {
                NSColor::windowBackgroundColor().colorWithAlphaComponent(0.90)
            };
            glass.setTintColor(Some(&tint));
        }
    }

    pub fn refresh(
        &self,
        hosts: &[HostView],
        color_rules: &[(String, String)],
        active_target: Option<&(String, String)>,
        reset: bool,
    ) {
        let templates = list_workspaces();
        let items = collect_switcher_items(hosts, &templates, color_rules, active_target);
        let live_n = items
            .iter()
            .filter(|i| matches!(i, SwitcherKind::LiveWindow { .. }))
            .count();
        let tmpl_n = items.len().saturating_sub(live_n);
        self.summary
            .setStringValue(&NSString::from_str(&format!("{live_n} live · {tmpl_n} templates")));

        self.tabs
            .setLabel_forSegment(&NSString::from_str(&format!("All ({})", items.len())), 0);
        self.tabs
            .setLabel_forSegment(&NSString::from_str(&format!("Live ({live_n})")), 1);
        self.tabs
            .setLabel_forSegment(&NSString::from_str(&format!("Templates ({tmpl_n})")), 2);

        if reset {
            self.search.setStringValue(&NSString::from_str(""));
            self.tabs.setSelectedSegment(0);
            self.list.ivars().filter.set(SwitcherFilter::All);
            *self.list.ivars().query.borrow_mut() = String::new();
        }
        self.list.set_all(items);
    }

    pub fn place(&self, size: NSSize, top: f64) {
        let w = Self::WIDTH.min(size.width - 32.0).max(340.0);
        let h = Self::HEIGHT.min(size.height - top - 56.0).max(240.0);
        let x = ((size.width - w) / 2.0).round();
        let y = ((size.height - top - h) / 2.0 + 12.0).max(20.0).round();
        let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        self.host.setFrame(frame);
        self.content.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));

        let pad = 16.0;
        self.title.setFrame(NSRect::new(NSPoint::new(pad, h - 34.0), NSSize::new(200.0, 22.0)));
        self.close_btn.setFrame(NSRect::new(NSPoint::new(w - 34.0, h - 35.0), NSSize::new(22.0, 22.0)));
        self.summary.setFrame(NSRect::new(
            NSPoint::new(218.0, h - 32.0),
            NSSize::new((w - 256.0).max(60.0), 18.0),
        ));
        self.subtitle.setFrame(NSRect::new(
            NSPoint::new(pad, h - 52.0),
            NSSize::new((w - 2.0 * pad).max(80.0), 16.0),
        ));
        self.tabs.setFrame(NSRect::new(
            NSPoint::new(pad, h - 84.0),
            NSSize::new((w - 2.0 * pad).max(160.0), 24.0),
        ));
        self.search.setFrame(NSRect::new(
            NSPoint::new(pad, h - 118.0),
            NSSize::new((w - 2.0 * pad).max(100.0), 26.0),
        ));

        let list_y = 34.0;
        let list_h = (h - 126.0 - list_y).max(60.0);
        self.list.setFrame(NSRect::new(
            NSPoint::new(8.0, list_y),
            NSSize::new((w - 16.0).max(100.0), list_h),
        ));

        self.footer_hint.setFrame(NSRect::new(
            NSPoint::new(pad, 9.0),
            NSSize::new((w - 208.0).max(80.0), 16.0),
        ));
        self.save_btn.setFrame(NSRect::new(
            NSPoint::new(w - 190.0, 6.0),
            NSSize::new(96.0, 22.0),
        ));
        self.manage_btn.setFrame(NSRect::new(
            NSPoint::new(w - 88.0, 6.0),
            NSSize::new(72.0, 22.0),
        ));
    }
}

fn activate_item(win_id: u64, item: &SwitcherKind) {
    with_window(win_id, |w| w.close_workspaces_card());
    match item {
        SwitcherKind::LiveWindow { host, window_id, .. } => {
            super::mux_select_window(win_id, host, window_id);
        }
        SwitcherKind::Template { name, .. } => {
            super::mux_workspace_up(win_id, name);
        }
    }
    with_window(win_id, |w| w.focus_terminal());
}

fn srgb_alpha(rgb: [u8; 3], alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        rgb[0] as f64 / 255.0,
        rgb[1] as f64 / 255.0,
        rgb[2] as f64 / 255.0,
        alpha,
    )
}

fn make_label(
    mtm: MainThreadMarker,
    text: &str,
    size: f64,
    bold: bool,
    color: &NSColor,
) -> Retained<NSTextField> {
    let f = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    let font = if bold {
        NSFont::boldSystemFontOfSize(size as CGFloat)
    } else {
        NSFont::systemFontOfSize(size as CGFloat)
    };
    f.setFont(Some(&font));
    f.setTextColor(Some(color));
    f
}

#[derive(Clone, Copy)]
enum FontWeight {
    Regular,
    Medium,
    Semibold,
}

fn draw_aligned(
    text: &str,
    size: f64,
    weight: FontWeight,
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
    let font = NSFont::systemFontOfSize_weight(size as CGFloat, w);
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

fn draw_left(text: &str, size: f64, weight: FontWeight, color: &NSColor, rect: NSRect) {
    draw_aligned(text, size, weight, color, NSTextAlignment::Left, rect);
}

fn draw_right(text: &str, size: f64, weight: FontWeight, color: &NSColor, rect: NSRect) {
    draw_aligned(text, size, weight, color, NSTextAlignment::Right, rect);
}

fn draw_centered(text: &str, size: f64, weight: FontWeight, color: &NSColor, rect: NSRect) {
    draw_aligned(text, size, weight, color, NSTextAlignment::Center, rect);
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
            NSCompositingOperation::SourceOver,
            1.0,
            true,
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::model::{
        BlockInfo, HostInfo, LayoutNode, ServerSnapshot, SessionInfo, WindowInfo,
    };
    use crate::term::ProgramStatus;
    use std::path::PathBuf;

    #[test]
    fn switcher_collects_ranks_and_filters_live_windows_and_templates() {
        let base = std::env::temp_dir().join(format!("tachyon-ws-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/cards\n").unwrap();

        let make_block = |id: &str, sid: &str, wid: &str, cwd: &str, blocked: bool| BlockInfo {
            id: id.into(),
            session_id: sid.into(),
            window_id: wid.into(),
            label: None,
            title: None,
            cwd: Some(cwd.into()),
            command: Some("zsh".into()),
            command_running: false,
            pid: 1,
            cols: 80,
            rows: 24,
            alive: true,
            exit_code: None,
            awake: false,
            attached_clients: 0,
            created_at: 0,
            status: blocked.then(|| ProgramStatus {
                state: ProgramState::Blocked,
                app: Some("claude".into()),
                kind: None,
                msg: Some("Approve".into()),
                label: None,
            }),
            status_inferred: false,
            last_receipt: None,
        };

        let local = HostView {
            alias: "local".into(),
            display_name: "This Mac".into(),
            uri: "unix://local".into(),
            is_local: true,
            color: None,
            status: HostStatus::Connected {
                info: HostInfo {
                    hostname: "mac".into(),
                    os: "macos".into(),
                    arch: "aarch64".into(),
                    version: "0.1.0".into(),
                    protocol: 2,
                    pid: 10,
                    build: None,
                    capabilities: Vec::new(),
                    auth_enabled: None,
                    listen_addr: None,
                },
                rtt_ms: None,
            },
            snapshot: Some(ServerSnapshot {
                host: HostInfo {
                    hostname: "mac".into(),
                    os: "macos".into(),
                    arch: "aarch64".into(),
                    version: "0.1.0".into(),
                    protocol: 2,
                    pid: 10,
                    build: None,
                    capabilities: Vec::new(),
                    auth_enabled: None,
                    listen_addr: None,
                },
                sessions: vec![SessionInfo {
                    id: "s1".into(),
                    name: "work".into(),
                    windows: vec!["w1".into(), "w2".into()],
                    active_window: Some("w1".into()),
                    created_at: 0,
                }],
                windows: vec![
                    WindowInfo {
                        id: "w1".into(),
                        session_id: "s1".into(),
                        name: Some("Tachyon".into()),
                        layout: LayoutNode::Leaf("b1".into()),
                        focused_block: "b1".into(),
                        zoomed: false,
                        created_at: 0,
                    },
                    WindowInfo {
                        id: "w2".into(),
                        session_id: "s1".into(),
                        name: Some("Other".into()),
                        layout: LayoutNode::Leaf("b2".into()),
                        focused_block: "b2".into(),
                        zoomed: false,
                        created_at: 0,
                    },
                ],
                blocks: vec![
                    make_block("b1", "s1", "w1", &repo.to_string_lossy(), false),
                    make_block("b2", "s1", "w2", "/tmp", true),
                ],
                clients: Vec::new(),
            }),
        };

        let templates = vec![
            WorkspaceSummary {
                name: "dev".into(),
                path: PathBuf::from("/tmp/dev.json"),
                description: Some("Local dev".into()),
                hosts: vec!["local".into()],
                session_names: vec![("local".into(), "work".into())],
                session_count: 1,
                window_count: 2,
                pane_count: 2,
            },
            WorkspaceSummary {
                name: "cluster".into(),
                path: PathBuf::from("/tmp/cluster.json"),
                description: Some("Remote training".into()),
                hosts: vec!["kagutsuchi".into()],
                session_names: vec![("kagutsuchi".into(), "train".into())],
                session_count: 1,
                window_count: 1,
                pane_count: 1,
            },
        ];

        let active = ("local".to_string(), "w1".to_string());
        let items = collect_switcher_items(&[local], &templates, &[], Some(&active));
        assert_eq!(items.len(), 4);

        // Active window w1 ranks first, then blocked w2, then running template dev, then stopped cluster.
        match &items[0] {
            SwitcherKind::LiveWindow { window_id, git_branch, active, .. } => {
                assert_eq!(window_id, "w1");
                assert!(*active);
                assert_eq!(git_branch.as_deref(), Some("feat/cards"));
            }
            _ => panic!("expected live active window first"),
        }
        match &items[1] {
            SwitcherKind::LiveWindow { window_id, blocked_count, .. } => {
                assert_eq!(window_id, "w2");
                assert_eq!(*blocked_count, 1);
            }
            _ => panic!("expected blocked window second"),
        }
        match &items[2] {
            SwitcherKind::Template { name, running, .. } => {
                assert_eq!(name, "dev");
                assert!(*running);
            }
            _ => panic!("expected running template third"),
        }
        match &items[3] {
            SwitcherKind::Template { name, running, .. } => {
                assert_eq!(name, "cluster");
                assert!(!*running);
            }
            _ => panic!("expected stopped template fourth"),
        }

        assert_eq!(filter_switcher_items(&items, SwitcherFilter::Live, "").len(), 2);
        assert_eq!(filter_switcher_items(&items, SwitcherFilter::Templates, "").len(), 2);
        assert_eq!(filter_switcher_items(&items, SwitcherFilter::All, "feat/cards").len(), 1);
        assert_eq!(filter_switcher_items(&items, SwitcherFilter::Templates, "kagutsuchi train").len(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }
}
