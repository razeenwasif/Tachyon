//! Rex-style collapsible Liquid Glass left sidebar (`Sidebar`).
//!
//! Displays every managed host (`This Mac`, `kagutsuchi`, …) with its
//! persistent `Session -> Window` tree, live connection/RTT indicators,
//! inline `Filter` search box, resizable right edge, and a Remote Hosts menu
//! seeded from `~/.config/Tachyon/hosts.json` and `~/.ssh/config`.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSButton, NSColor, NSControl,
    NSControlTextEditingDelegate, NSCursor, NSEvent, NSFont, NSFontAttributeName,
    NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSImage, NSLineBreakMode, NSMenu, NSMenuItem,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder, NSSearchField,
    NSSearchFieldDelegate, NSTextAlignment, NSTextFieldDelegate, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use super::host_mgr::{HostStatus, HostView};
use super::renderer::chrome_tokens;
use super::{machines, pane_bar, with_window};
use crate::config::ChromeStyle;
use crate::term::{ProgramState, ProgramStatus};

pub const MIN_WIDTH: f64 = 180.0;
pub const MAX_WIDTH: f64 = 360.0;
const TOP_BAR_H: f64 = 38.0;
const FOOTER_H: f64 = 38.0;
const RESIZE_GRIP_W: f64 = 6.0;

#[derive(Clone, Debug)]
enum TreeRow {
    Host {
        alias: String,
        display_name: String,
        is_local: bool,
        os: Option<String>,
        color_spec: Option<String>,
        color: Option<crate::mux::model::HostColor>,
        connected: bool,
        connecting: bool,
        rtt_ms: Option<f64>,
        top_pad: f64,
    },
    Session {
        host: String,
        session_id: String,
        name: String,
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
            Self::Session { .. } => 22.0,
            Self::Window { .. } => 28.0,
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
    content_h: Cell<f64>,
    scroll_y: Cell<f64>,
    hover: Cell<Option<(usize, RowAction)>>,
    dragging_resize: Cell<bool>,
    chrome: Cell<ChromeStyle>,
}

define_class!(
    /// Custom-drawn scrollable Host -> Session -> Window tree inside the sidebar.
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
                NSCursor::columnResizeCursorInDirections(objc2_app_kit::NSHorizontalDirections::All)
                    .set();
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
            let x = event.locationInWindow().x.clamp(MIN_WIDTH, MAX_WIDTH);
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| w.set_sidebar_width(x));
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
    }
);

impl SidebarTreeView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = TreeState::default();
        state.window.set(window);
        let this = Self::alloc(mtm).set_ivars(state);
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
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

    fn in_resize_grip(&self, x: f64) -> bool {
        let w = self.bounds().size.width;
        x >= (w - RESIZE_GRIP_W).max(0.0)
    }

    fn rebuild_rows(&self) {
        let hosts = self.ivars().hosts.borrow();
        let filter = self.ivars().filter.borrow().trim().to_ascii_lowercase();
        let active = self.ivars().active_target.borrow().clone();

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
                let mut shown_any = false;
                for sess in &snap.sessions {
                    let sess_matches = filter.is_empty()
                        || sess.name.to_ascii_lowercase().contains(&filter)
                        || h.display_name.to_ascii_lowercase().contains(&filter)
                        || h.alias.to_ascii_lowercase().contains(&filter);

                    let mut win_rows = Vec::new();
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
                        let has_agent = status.is_some()
                            || snap.blocks.iter().any(|b| {
                                leaves.contains(&b.id)
                                    && crate::mux::model::infer_agent_from_block(b).is_some()
                            });
                        if agents_only && !has_agent {
                            continue;
                        }

                        if !sess_matches && !title.to_ascii_lowercase().contains(&filter) {
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
                        });
                    }

                    if !win_rows.is_empty() || (!agents_only && sess_matches && sess.windows.is_empty()) {
                        shown_any = true;
                        let srow = TreeRow::Session {
                            host: h.alias.clone(),
                            session_id: sess.id.clone(),
                            name: sess.name.clone(),
                        };
                        let sh = srow.height();
                        rows.push((y, sh, srow));
                        y += sh;
                        for wr in win_rows {
                            let wh = wr.height();
                            rows.push((y, wh, wr));
                            y += wh;
                        }
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

                    // OS-aware host icon (SF Symbol), tinted with host accent/danger colour.
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

                    // Status dot beside host name.
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
                    let dot = NSRect::new(NSPoint::new(w - 36.0, y0 + 10.0), NSSize::new(6.0, 6.0));
                    NSBezierPath::bezierPathWithOvalInRect(dot).fill();

                    // `+` new session button on right.
                    if hovered_act == Some(RowAction::Add) {
                        let hov = if minimal {
                            srgb_alpha([255, 255, 255], 0.08)
                        } else {
                            NSColor::labelColor().colorWithAlphaComponent(0.12)
                        };
                        hov.setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                            NSRect::new(NSPoint::new(w - 26.0, y0 + 4.0), NSSize::new(18.0, 18.0)),
                            4.0,
                            4.0,
                        )
                        .fill();
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

                    // Host title + danger marker + optional RTT suffix for remote hosts.
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
                        11.5,
                        FontWeight::Medium,
                        &fg,
                        NSRect::new(
                            NSPoint::new(14.0, y + 4.0),
                            NSSize::new((w - 14.0 - right_margin).max(0.0), 15.0),
                        ),
                    );
                    if hovered {
                        let add_col = if hovered_act == Some(RowAction::Add) {
                            NSColor::labelColor()
                        } else {
                            NSColor::secondaryLabelColor()
                        };
                        draw_text_centered(
                            "+",
                            12.5,
                            true,
                            &add_col,
                            NSRect::new(NSPoint::new(w - 40.0, y + 2.0), NSSize::new(16.0, 16.0)),
                        );
                        let close_col = if hovered_act == Some(RowAction::Close) {
                            NSColor::systemRedColor()
                        } else {
                            NSColor::secondaryLabelColor()
                        };
                        draw_text_centered(
                            "×",
                            12.5,
                            false,
                            &close_col,
                            NSRect::new(NSPoint::new(w - 23.0, y + 2.0), NSSize::new(16.0, 16.0)),
                        );
                    }
                }

                TreeRow::Window {
                    title,
                    pane_count,
                    running,
                    active,
                    status,
                    ..
                } => {
                    let pill = NSRect::new(
                        NSPoint::new(8.0, y + 1.0),
                        NSSize::new((w - 16.0).max(0.0), *rh - 2.0),
                    );
                    let radius = if minimal { 8.0 } else { 13.0 };
                    if *active {
                        let sel = if minimal {
                            srgb_alpha([255, 255, 255], 0.09)
                        } else {
                            NSColor::labelColor().colorWithAlphaComponent(0.14)
                        };
                        sel.setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, radius, radius).fill();
                    } else if hovered {
                        let hov = if minimal {
                            srgb_alpha([255, 255, 255], 0.055)
                        } else {
                            NSColor::labelColor().colorWithAlphaComponent(0.07)
                        };
                        hov.setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, radius, radius).fill();
                    }

                    // Agent or terminal glyph badge on left.
                    let badge = NSRect::new(NSPoint::new(14.0, y + 6.0), NSSize::new(16.0, 16.0));
                    let (glyph, glyph_col) = pane_bar::app_badge(
                        status.as_ref().and_then(|s| s.app.as_deref()),
                        status.as_ref().map(|s| s.state),
                        *running,
                        *active,
                    );
                    let badge_bg = if status.is_some() || *running {
                        glyph_col.colorWithAlphaComponent(0.20)
                    } else {
                        NSColor::labelColor().colorWithAlphaComponent(0.09)
                    };
                    badge_bg.setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(badge, 4.0, 4.0).fill();
                    draw_text_centered(
                        glyph,
                        if glyph == ">_" { 9.5 } else { 11.0 },
                        true,
                        &glyph_col,
                        badge,
                    );

                    let mut right_x = w - 14.0;
                    if hovered {
                        let x_col = if hovered_act == Some(RowAction::Close) {
                            NSColor::systemRedColor()
                        } else {
                            NSColor::secondaryLabelColor()
                        };
                        draw_text_centered(
                            "×",
                            12.0,
                            false,
                            &x_col,
                            NSRect::new(NSPoint::new(w - 25.0, y + 5.0), NSSize::new(16.0, 16.0)),
                        );
                        right_x = w - 28.0;
                    } else if let Some(st) = status {
                        let (sym, col) = pane_bar::status_style(st.state);
                        if st.state == ProgramState::Blocked {
                            let b_rect = NSRect::new(NSPoint::new(w - 30.0, y + 7.0), NSSize::new(16.0, 14.0));
                            col.colorWithAlphaComponent(0.22).setFill();
                            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b_rect, 7.0, 7.0).fill();
                            draw_text_centered(sym, 9.5, true, &col, b_rect);
                            right_x = w - 34.0;
                        } else {
                            col.setFill();
                            NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                                NSPoint::new(w - 22.0, y + 11.0),
                                NSSize::new(6.0, 6.0),
                            ))
                            .fill();
                            right_x = w - 26.0;
                        }
                    } else if *pane_count > 1 {
                        let badge_rect = NSRect::new(NSPoint::new(w - 30.0, y + 7.0), NSSize::new(16.0, 14.0));
                        NSColor::labelColor().colorWithAlphaComponent(0.10).setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(badge_rect, 7.0, 7.0).fill();
                        draw_text_centered(
                            &pane_count.to_string(),
                            10.0,
                            true,
                            &NSColor::secondaryLabelColor(),
                            badge_rect,
                        );
                        right_x = w - 34.0;
                    } else if *running {
                        NSColor::controlAccentColor().setFill();
                        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                            NSPoint::new(w - 22.0, y + 11.0),
                            NSSize::new(6.0, 6.0),
                        ))
                        .fill();
                        right_x = w - 26.0;
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
                            NSPoint::new(36.0, y + 6.0),
                            NSSize::new((right_x - 36.0).max(0.0), 16.0),
                        ),
                    );
                }

                TreeRow::Note { text, .. } => {
                    let fg = NSColor::tertiaryLabelColor();
                    draw_text_left(
                        text,
                        11.0,
                        FontWeight::Regular,
                        &fg,
                        NSRect::new(NSPoint::new(18.0, y + 3.0), NSSize::new((w - 28.0).max(0.0), 15.0)),
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
            TreeRow::Session { host, session_id, .. } => {
                let key = format!("{host}\u{1F}{session_id}");
                add("New Window", sel!(ctxNewWindow:), key.clone());
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                add("Kill Session", sel!(ctxKillSession:), key);
            }
            TreeRow::Window { host, session_id, window_id, .. } => {
                add("Open Window", sel!(ctxOpenWindow:), format!("{host}\u{1F}{window_id}"));
                add("New Window in Session", sel!(ctxNewWindow:), format!("{host}\u{1F}{session_id}"));
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

define_class!(
    /// Container view for the sidebar (subtle background tint + right hairline)
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
            if style.is_minimal() {
                let tok = chrome_tokens(style, [0x17, 0x19, 0x1D]);
                srgb_alpha(tok.chrome_bg, 1.0).setFill();
                NSBezierPath::fillRect(b);

                srgb_alpha(tok.hairline_rgb, tok.hairline_alpha as f64).setFill();
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(0.0, b.size.height - TOP_BAR_H),
                    NSSize::new(b.size.width, 1.0),
                ));
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(0.0, FOOTER_H),
                    NSSize::new(b.size.width, 1.0),
                ));
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(b.size.width - 1.0, 0.0),
                    NSSize::new(1.0, b.size.height),
                ));
                return;
            }
            // Subtle darker glass tint separating the sidebar from the terminal.
            NSColor::blackColor().colorWithAlphaComponent(0.16).setFill();
            NSBezierPath::fillRect(b);

            // Top bar hairline.
            NSColor::separatorColor().colorWithAlphaComponent(0.25).setFill();
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(0.0, b.size.height - TOP_BAR_H),
                NSSize::new(b.size.width, 1.0),
            ));
            // Footer hairline.
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(0.0, FOOTER_H),
                NSSize::new(b.size.width, 1.0),
            ));
            // Right edge vertical divider hairline.
            NSColor::separatorColor().colorWithAlphaComponent(0.40).setFill();
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

        #[unsafe(method(sidebarSettings:))]
        fn sidebar_settings(&self, _sender: Option<&AnyObject>) {
            super::open_settings();
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

            // Discovered SSH hosts from ~/.ssh/config not yet in hosts.json.
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
}

/// Complete collapsible sidebar mounted beside `TachyonView`.
pub struct Sidebar {
    pub root: Retained<NSView>,
    root_view: Retained<SidebarRootView>,
    tree: Retained<SidebarTreeView>,
    toggle_btn: Retained<NSButton>,
    palette_btn: Retained<NSButton>,
    refresh_btn: Retained<NSButton>,
    new_btn: Retained<NSButton>,
    filter: Retained<NSSearchField>,
    agents_btn: Retained<NSButton>,
    settings_btn: Retained<NSButton>,
    hosts_btn: Retained<NSButton>,
    server_btn: Retained<NSButton>,
}

impl Sidebar {
    pub fn new(mtm: MainThreadMarker, parent: &NSView, window: u64) -> Self {
        let state = RootState::default();
        state.window.set(window);
        *state.active_host.borrow_mut() = "local".into();
        let root_view: Retained<SidebarRootView> =
            unsafe { msg_send![super(SidebarRootView::alloc(mtm).set_ivars(state)), initWithFrame: NSRect::ZERO] };

        let tree = SidebarTreeView::new(mtm, window);
        root_view.addSubview(&tree);

        let target: &AnyObject = root_view.as_ref();
        let toggle_btn = symbol_button(mtm, "sidebar.left", "Toggle Sidebar (⌘B)", target, sel!(sidebarToggle:));
        let palette_btn = symbol_button(mtm, "command", "Command Palette (⌘⇧P)", target, sel!(sidebarPalette:));
        let refresh_btn = symbol_button(mtm, "arrow.clockwise", "Refresh Hosts", target, sel!(sidebarRefresh:));
        let new_btn = symbol_button(mtm, "plus", "New Session", target, sel!(sidebarNewSession:));
        let agents_btn = symbol_button(
            mtm,
            "sparkles",
            "Filter Sidebar to Coding Agents (⌘⇧A for Mission Control)",
            target,
            sel!(sidebarAgentsFilter:),
        );
        let settings_btn = symbol_button(mtm, "gearshape", "Settings (⌘,)", target, sel!(sidebarSettings:));
        let hosts_btn = symbol_button(mtm, "globe", "Remote Hosts", target, sel!(sidebarHostsMenu:));
        let server_btn = symbol_button(mtm, "server.rack", "Server Status", target, sel!(sidebarSettings:));

        let filter = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::ZERO);
        filter.setPlaceholderString(Some(&NSString::from_str("Filter")));
        unsafe { filter.setDelegate(Some(ProtocolObject::from_ref(&*root_view))) };
        filter.setFocusRingType(objc2_app_kit::NSFocusRingType::None);

        root_view.addSubview(&toggle_btn);
        root_view.addSubview(&palette_btn);
        root_view.addSubview(&refresh_btn);
        root_view.addSubview(&new_btn);
        root_view.addSubview(&filter);
        root_view.addSubview(&agents_btn);
        root_view.addSubview(&settings_btn);
        root_view.addSubview(&hosts_btn);
        root_view.addSubview(&server_btn);

        let root: Retained<NSView> = Retained::into_super(root_view.clone());
        parent.addSubview(&root);

        Self {
            root,
            root_view,
            tree,
            toggle_btn,
            palette_btn,
            refresh_btn,
            new_btn,
            filter,
            agents_btn,
            settings_btn,
            hosts_btn,
            server_btn,
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
        self.new_btn.setFrame(NSRect::new(NSPoint::new((w - 28.0).max(104.0), btn_y), NSSize::new(22.0, 22.0)));
        self.palette_btn.setFrame(NSRect::new(NSPoint::new((w - 52.0).max(104.0), btn_y), NSSize::new(22.0, 22.0)));
        self.refresh_btn.setFrame(NSRect::new(NSPoint::new((w - 76.0).max(104.0), btn_y), NSSize::new(22.0, 22.0)));

        // Middle tree area.
        let tree_h = (h - TOP_BAR_H - FOOTER_H).max(0.0);
        self.tree.setFrame(NSRect::new(NSPoint::new(0.0, FOOTER_H), NSSize::new(w, tree_h)));

        // Bottom footer: Filter box + agents filter, settings, server & hosts buttons.
        let fy = (FOOTER_H - 22.0) / 2.0;
        let filter_w = (w - 116.0).max(52.0);
        self.filter.setFrame(NSRect::new(NSPoint::new(8.0, fy), NSSize::new(filter_w, 22.0)));
        self.agents_btn.setFrame(NSRect::new(NSPoint::new((w - 100.0).max(64.0), fy), NSSize::new(22.0, 22.0)));
        self.settings_btn.setFrame(NSRect::new(NSPoint::new((w - 76.0).max(86.0), fy), NSSize::new(22.0, 22.0)));
        self.server_btn.setFrame(NSRect::new(NSPoint::new((w - 52.0).max(108.0), fy), NSSize::new(22.0, 22.0)));
        self.hosts_btn.setFrame(NSRect::new(NSPoint::new((w - 28.0).max(130.0), fy), NSSize::new(22.0, 22.0)));

        self.tree.rebuild_rows();
    }

    pub fn set_chrome(&self, style: ChromeStyle) {
        self.root_view.ivars().chrome.set(style);
        self.tree.ivars().chrome.set(style);
        self.root_view.setNeedsDisplay(true);
        self.tree.setNeedsDisplay(true);
    }

    pub fn set_hosts(&self, hosts: Vec<HostView>, active: Option<(String, String)>) {
        if let Some((ref h, _)) = active {
            *self.root_view.ivars().active_host.borrow_mut() = h.clone();
        }
        // Update local server button tint & tooltip.
        if let Some(local) = hosts.iter().find(|h| h.is_local) {
            let (color, tip) = match &local.status {
                HostStatus::Connected { info, .. } => {
                    let (s, b) = local
                        .snapshot
                        .as_ref()
                        .map(|sn| (sn.sessions.len(), sn.blocks.len()))
                        .unwrap_or((0, 0));
                    (
                        NSColor::systemGreenColor(),
                        format!("Local server running (pid {}, {} sessions, {} blocks)", info.pid, s, b),
                    )
                }
                HostStatus::Connecting => (NSColor::systemOrangeColor(), "Local server starting…".into()),
                HostStatus::Disconnected { error } => (NSColor::systemRedColor(), format!("Local server offline: {error}")),
            };
            self.server_btn.setContentTintColor(Some(&color));
            self.server_btn.setToolTip(Some(&NSString::from_str(&tip)));
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

        *self.tree.ivars().hosts.borrow_mut() = hosts;
        *self.tree.ivars().active_target.borrow_mut() = active;
        self.update_agents_filter_button();
        self.tree.rebuild_rows();
    }

    pub fn toggle_agents_only(&self) {
        let next = !self.tree.ivars().agents_only.get();
        self.tree.ivars().agents_only.set(next);
        self.tree.ivars().scroll_y.set(0.0);
        self.update_agents_filter_button();
        self.tree.rebuild_rows();
    }

    fn update_agents_filter_button(&self) {
        let on = self.tree.ivars().agents_only.get();
        let hosts = self.tree.ivars().hosts.borrow();
        let entries = super::agents::collect_from_host_views(&hosts, &[]);
        let counts = crate::mux::model::AgentCounts::from_entries(&entries);
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

    pub fn set_filter(&self, query: String) {
        *self.tree.ivars().filter.borrow_mut() = query;
        self.tree.ivars().scroll_y.set(0.0);
        self.tree.rebuild_rows();
    }
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

fn draw_text_left(text: &str, size: f64, weight: FontWeight, color: &NSColor, rect: NSRect) {
    let para = NSMutableParagraphStyle::new();
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

fn draw_text_centered(text: &str, size: f64, medium: bool, color: &NSColor, rect: NSRect) {
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(NSTextAlignment::Center);
    para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let weight = unsafe { if medium { NSFontWeightMedium } else { NSFontWeightRegular } };
    let font = NSFont::systemFontOfSize_weight(size as CGFloat, weight);
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

fn draw_symbol(name: &str, rect: NSRect, color: &NSColor) {
    let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    ) else {
        return;
    };
    let tinted = img.imageWithSymbolConfiguration(
        &objc2_app_kit::NSImageSymbolConfiguration::configurationWithHierarchicalColor(color),
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
