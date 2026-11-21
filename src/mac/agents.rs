//! Floating Liquid Glass **Agent Mission Control** inbox (`⌘⇧A`, Command
//! Palette, or footer `sparkles` button).
//!
//! Aggregates live OSC 7501 coding-agent statuses (plus inferred running agents)
//! across `This Mac` and every connected remote host (`kagutsuchi`, `amaterasu`,
//! Linux servers), sorts urgent `blocked` / `error` agents to the top, lets you
//! filter by state or search query, and jumps straight to the exact host,
//! window, and pane on click or Return.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezelStyle, NSBezierPath, NSButton, NSColor, NSControl,
    NSControlTextEditingDelegate, NSEvent, NSFont, NSFontAttributeName, NSFontWeightMedium,
    NSFontWeightRegular, NSFontWeightSemibold, NSForegroundColorAttributeName, NSGlassEffectView,
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
use super::with_window;
use crate::mux::model::{
    collect_agent_entries, resolve_host_color, short_path, AgentCounts, AgentEntry, AgentFilter,
};
use crate::term::ProgramState;

const ROW_H: f64 = 56.0;

/// Collect all coding-agent entries across `hosts`, attaching each host's
/// resolved Feature #1 accent/danger colour.
pub fn collect_from_host_views(
    hosts: &[HostView],
    color_rules: &[(String, String)],
) -> Vec<AgentEntry> {
    let inputs: Vec<_> = hosts
        .iter()
        .filter_map(|h| {
            let snap = h.snapshot.as_ref()?;
            let os = match &h.status {
                HostStatus::Connected { info, .. } => Some(info.os.as_str()),
                _ => None,
            };
            let color = resolve_host_color(&h.alias, os, h.color.as_deref(), color_rules);
            Some((h.alias.clone(), h.display_name.clone(), color, snap))
        })
        .collect();
    collect_agent_entries(&inputs)
}

/// Filter `entries` by urgency tab (`AgentFilter`) and free-text query.
pub fn filter_entries(entries: &[AgentEntry], filter: AgentFilter, query: &str) -> Vec<AgentEntry> {
    let q = query.trim().to_ascii_lowercase();
    let terms: Vec<&str> = q.split_whitespace().collect();
    entries
        .iter()
        .filter(|e| filter.matches(e.state))
        .filter(|e| {
            if terms.is_empty() {
                return true;
            }
            let hay = format!(
                "{} {} {} {} {} {} {} {}",
                e.agent_name,
                e.app_id,
                e.state.as_str(),
                e.host,
                e.host_display,
                e.session_name,
                e.window_name,
                e.message.as_deref().unwrap_or("")
            )
            .to_ascii_lowercase();
            terms.iter().all(|t| hay.contains(t))
        })
        .cloned()
        .collect()
}

/// Human summary line for header/tooltips (e.g. `1 blocked · 2 working · 1 done`).
pub fn summary_line(counts: AgentCounts) -> String {
    if counts.total == 0 {
        return "No active agents".into();
    }
    let mut parts = Vec::new();
    if counts.blocked > 0 {
        parts.push(format!("{} blocked", counts.blocked));
    }
    if counts.error > 0 {
        parts.push(format!("{} error", counts.error));
    }
    if counts.working > 0 {
        parts.push(format!("{} working", counts.working));
    }
    if counts.done > 0 {
        parts.push(format!("{} done", counts.done));
    }
    if counts.idle > 0 {
        parts.push(format!("{} idle", counts.idle));
    }
    parts.join(" · ")
}

#[derive(Default)]
struct ListState {
    window: Cell<u64>,
    all: RefCell<Vec<AgentEntry>>,
    filtered: RefCell<Vec<AgentEntry>>,
    filter: Cell<AgentFilter>,
    query: RefCell<String>,
    selected: Cell<usize>,
    scroll_y: Cell<f64>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonAgentsListView"]
    #[ivars = ListState]
    struct AgentsListView;

    impl AgentsListView {
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
            let idx = ((p.y + self.ivars().scroll_y.get()) / ROW_H).floor() as isize;
            let len = self.ivars().filtered.borrow().len() as isize;
            if idx >= 0 && idx < len {
                let u = idx as usize;
                if self.ivars().selected.get() != u {
                    self.ivars().selected.set(u);
                    self.setNeedsDisplay(true);
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let idx = ((p.y + self.ivars().scroll_y.get()) / ROW_H).floor() as isize;
            let entry = if idx >= 0 {
                self.ivars().filtered.borrow().get(idx as usize).cloned()
            } else {
                None
            };
            if let Some(e) = entry {
                jump_to_agent(self.ivars().window.get(), &e);
            }
        }
    }
);

impl AgentsListView {
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

    fn set_all(&self, entries: Vec<AgentEntry>) {
        *self.ivars().all.borrow_mut() = entries;
        self.recompute(false);
    }

    fn set_filter(&self, filter: AgentFilter) {
        self.ivars().filter.set(filter);
        self.recompute(true);
    }

    fn set_query(&self, query: String) {
        *self.ivars().query.borrow_mut() = query;
        self.recompute(true);
    }

    fn recompute(&self, reset_scroll: bool) {
        let filtered = filter_entries(
            &self.ivars().all.borrow(),
            self.ivars().filter.get(),
            &self.ivars().query.borrow(),
        );
        let len = filtered.len();
        *self.ivars().filtered.borrow_mut() = filtered;
        if reset_scroll {
            self.ivars().selected.set(0);
            self.ivars().scroll_y.set(0.0);
        } else {
            if len == 0 {
                self.ivars().selected.set(0);
            } else if self.ivars().selected.get() >= len {
                self.ivars().selected.set(len - 1);
            }
            let max_scroll = (len as f64 * ROW_H - self.bounds().size.height).max(0.0);
            if self.ivars().scroll_y.get() > max_scroll {
                self.ivars().scroll_y.set(max_scroll);
            }
        }
        self.setNeedsDisplay(true);
    }

    fn move_selection(&self, delta: isize) {
        let len = self.ivars().filtered.borrow().len();
        if len == 0 {
            return;
        }
        let cur = self.ivars().selected.get() as isize;
        let next = (cur + delta).rem_euclid(len as isize) as usize;
        self.ivars().selected.set(next);
        let top = next as f64 * ROW_H;
        let bot = top + ROW_H;
        let scroll = self.ivars().scroll_y.get();
        let vis_h = self.bounds().size.height;
        if top < scroll {
            self.ivars().scroll_y.set(top);
        } else if bot > scroll + vis_h {
            self.ivars().scroll_y.set((bot - vis_h).max(0.0));
        }
        self.setNeedsDisplay(true);
    }

    fn selected_entry(&self) -> Option<AgentEntry> {
        let sel_idx = self.ivars().selected.get();
        self.ivars().filtered.borrow().get(sel_idx).cloned()
    }

    fn draw(&self) {
        let bounds = self.bounds();
        let w = bounds.size.width;
        let scroll = self.ivars().scroll_y.get();
        let selected = self.ivars().selected.get();
        let all_len = self.ivars().all.borrow().len();
        let items = self.ivars().filtered.borrow();

        if items.is_empty() {
            let (headline, body) = if all_len == 0 {
                (
                    "No coding agents active across your hosts",
                    "Run `tachyon skill setup` or open Integrations… below to connect Claude Code, Antigravity, Codex, OpenCode, or Pi.",
                )
            } else {
                (
                    "No agents match this filter",
                    "Switch tabs above or clear the search box to see all active agents.",
                )
            };
            draw_centered(
                headline,
                13.0,
                FontWeight::Semibold,
                &NSColor::labelColor(),
                NSRect::new(NSPoint::new(24.0, 56.0), NSSize::new((w - 48.0).max(80.0), 22.0)),
            );
            draw_centered(
                body,
                11.5,
                FontWeight::Regular,
                &NSColor::secondaryLabelColor(),
                NSRect::new(NSPoint::new(36.0, 82.0), NSSize::new((w - 72.0).max(80.0), 48.0)),
            );
            return;
        }

        for (i, entry) in items.iter().enumerate() {
            let ry = i as f64 * ROW_H - scroll;
            if ry + ROW_H < 0.0 || ry > bounds.size.height {
                continue;
            }
            let row_rect = NSRect::new(
                NSPoint::new(6.0, ry + 3.0),
                NSSize::new((w - 12.0).max(40.0), ROW_H - 6.0),
            );
            let is_sel = i == selected;
            let state_col = state_color(entry.state);

            // Card background for row.
            let card_bg = if is_sel {
                NSColor::controlAccentColor().colorWithAlphaComponent(0.20)
            } else if matches!(entry.state, ProgramState::Blocked | ProgramState::Error) {
                state_col.colorWithAlphaComponent(0.08)
            } else {
                NSColor::labelColor().colorWithAlphaComponent(0.04)
            };
            card_bg.setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(row_rect, 9.0, 9.0).fill();

            if is_sel {
                NSColor::controlAccentColor()
                    .colorWithAlphaComponent(0.45)
                    .setStroke();
                let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(row_rect, 9.0, 9.0);
                p.setLineWidth(1.0);
                p.stroke();
            }

            // Left urgency stripe.
            state_col.setFill();
            let stripe = NSRect::new(
                NSPoint::new(row_rect.origin.x + 1.0, row_rect.origin.y + 8.0),
                NSSize::new(3.0, (row_rect.size.height - 16.0).max(8.0)),
            );
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(stripe, 1.5, 1.5).fill();

            // Line 1: Glyph + Agent name | State badge | Session / Window | Host badge.
            let name_label = format!("{} {}", entry.glyph, entry.agent_name);
            draw_left(
                &name_label,
                12.5,
                FontWeight::Semibold,
                &NSColor::labelColor(),
                NSRect::new(NSPoint::new(18.0, ry + 8.0), NSSize::new(118.0, 18.0)),
            );

            let badge_text = state_badge_label(entry.state);
            let badge_rect = NSRect::new(NSPoint::new(140.0, ry + 9.0), NSSize::new(66.0, 16.0));
            state_col.colorWithAlphaComponent(0.20).setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(badge_rect, 5.0, 5.0).fill();
            draw_centered(
                badge_text,
                9.5,
                FontWeight::Semibold,
                &state_col,
                NSRect::new(NSPoint::new(140.0, ry + 10.5), NSSize::new(66.0, 14.0)),
            );

            let host_pill_w = 104.0;
            let host_pill_x = (w - host_pill_w - 14.0).max(220.0);
            let loc_w = (host_pill_x - 216.0).max(40.0);
            let loc = format!("{} / {}", entry.session_name, entry.window_name);
            draw_left(
                &loc,
                11.5,
                FontWeight::Medium,
                &NSColor::secondaryLabelColor(),
                NSRect::new(NSPoint::new(212.0, ry + 9.0), NSSize::new(loc_w, 17.0)),
            );

            // Right-aligned host pill tinted with host's Feature #1 accent/danger color.
            let (host_fg, host_bg, host_prefix) = match entry.host_color {
                Some(hc) => {
                    let c = NSColor::colorWithSRGBRed_green_blue_alpha(
                        hc.rgb[0] as f64 / 255.0,
                        hc.rgb[1] as f64 / 255.0,
                        hc.rgb[2] as f64 / 255.0,
                        1.0,
                    );
                    let bg = c.colorWithAlphaComponent(if hc.danger { 0.24 } else { 0.16 });
                    let prefix = if hc.danger { "▲ " } else { "● " };
                    (c, bg, prefix)
                }
                None => (
                    NSColor::secondaryLabelColor(),
                    NSColor::labelColor().colorWithAlphaComponent(0.08),
                    "",
                ),
            };
            let host_rect = NSRect::new(NSPoint::new(host_pill_x, ry + 8.5), NSSize::new(host_pill_w, 17.0));
            host_bg.setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(host_rect, 5.0, 5.0).fill();
            draw_centered(
                &format!("{host_prefix}{}", entry.host_display),
                10.0,
                FontWeight::Medium,
                &host_fg,
                NSRect::new(NSPoint::new(host_pill_x + 4.0, ry + 10.0), NSSize::new(host_pill_w - 8.0, 14.0)),
            );

            // Line 2: OSC 7501 message + shortened CWD.
            let msg_text = entry
                .message
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if entry.inferred {
                        "Detected running agent process".into()
                    } else {
                        match entry.state {
                            ProgramState::Blocked => "Waiting for approval or input".into(),
                            ProgramState::Error => "Agent reported an error".into(),
                            ProgramState::Working => "Working…".into(),
                            ProgramState::Done => "Finished task".into(),
                            ProgramState::Idle => "Ready at prompt".into(),
                        }
                    }
                });
            let cwd_w = 132.0;
            let msg_w = (w - cwd_w - 38.0).max(80.0);
            draw_left(
                &msg_text,
                11.5,
                FontWeight::Regular,
                &NSColor::labelColor().colorWithAlphaComponent(0.82),
                NSRect::new(NSPoint::new(18.0, ry + 30.0), NSSize::new(msg_w, 17.0)),
            );

            if let Some(cwd) = entry.cwd.as_deref().filter(|s| !s.is_empty()) {
                draw_right(
                    &short_path(cwd),
                    10.5,
                    FontWeight::Regular,
                    &NSColor::tertiaryLabelColor(),
                    NSRect::new(
                        NSPoint::new((w - cwd_w - 14.0).max(120.0), ry + 31.0),
                        NSSize::new(cwd_w, 15.0),
                    ),
                );
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
    #[name = "TachyonAgentsDelegate"]
    #[ivars = CardDelegateState]
    struct AgentsDelegate;

    unsafe impl NSObjectProtocol for AgentsDelegate {}
    unsafe impl NSControlTextEditingDelegate for AgentsDelegate {
        #[unsafe(method(controlTextDidChange:))]
        fn text_did_change(&self, _note: &NSNotification) {
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| {
                if let Some(c) = &w.agents_card {
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
                    w.close_agents_card();
                    w.focus_terminal();
                });
                true
            } else if sel == sel!(moveDown:) {
                with_window(win_id, |w| {
                    if let Some(c) = &w.agents_card {
                        c.list.move_selection(1);
                    }
                });
                true
            } else if sel == sel!(moveUp:) {
                with_window(win_id, |w| {
                    if let Some(c) = &w.agents_card {
                        c.list.move_selection(-1);
                    }
                });
                true
            } else if sel == sel!(insertNewline:) {
                let mut picked = None;
                with_window(win_id, |w| {
                    if let Some(c) = &w.agents_card {
                        picked = c.list.selected_entry();
                    }
                });
                if let Some(e) = picked {
                    jump_to_agent(win_id, &e);
                } else {
                    with_window(win_id, |w| {
                        w.close_agents_card();
                        w.focus_terminal();
                    });
                }
                true
            } else {
                false
            }
        }
    }
    unsafe impl NSTextFieldDelegate for AgentsDelegate {}
    unsafe impl NSSearchFieldDelegate for AgentsDelegate {}

    impl AgentsDelegate {
        #[unsafe(method(agentsFilterChanged:))]
        fn filter_changed(&self, sender: Option<&AnyObject>) {
            let idx = sender
                .and_then(|s| s.downcast_ref::<NSSegmentedControl>())
                .map(|seg| seg.selectedSegment())
                .unwrap_or(0);
            let f = match idx {
                1 => AgentFilter::Attention,
                2 => AgentFilter::Working,
                3 => AgentFilter::Done,
                _ => AgentFilter::All,
            };
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| {
                if let Some(c) = &w.agents_card {
                    c.list.set_filter(f);
                }
            });
        }

        #[unsafe(method(agentsClose:))]
        fn close_card(&self, _sender: Option<&AnyObject>) {
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| {
                w.close_agents_card();
                w.focus_terminal();
            });
        }

        #[unsafe(method(agentsOpenIntegrations:))]
        fn open_integrations(&self, _sender: Option<&AnyObject>) {
            let win_id = self.ivars().window.get();
            with_window(win_id, |w| w.close_agents_card());
            super::open_settings_section(2);
        }
    }
);

impl AgentsDelegate {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = CardDelegateState::default();
        state.window.set(window);
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), init] }
    }
}

pub struct AgentsCard {
    pub host: Retained<NSView>,
    content: Retained<NSView>,
    title: Retained<NSTextField>,
    summary: Retained<NSTextField>,
    subtitle: Retained<NSTextField>,
    close_btn: Retained<NSButton>,
    tabs: Retained<NSSegmentedControl>,
    pub search: Retained<NSSearchField>,
    list: Retained<AgentsListView>,
    footer_hint: Retained<NSTextField>,
    integrations_btn: Retained<NSButton>,
    _delegate: Retained<AgentsDelegate>,
}

impl AgentsCard {
    const WIDTH: f64 = 560.0;
    const HEIGHT: f64 = 430.0;

    pub fn new(mtm: MainThreadMarker, parent: &NSView, window: u64) -> Self {
        let content = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let host: Retained<NSView> = if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some() {
            let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), NSRect::ZERO);
            glass.setCornerRadius(16.0);
            glass.setTintColor(Some(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.90)));
            glass.setContentView(Some(&content));
            Retained::into_super(glass)
        } else {
            let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), NSRect::ZERO);
            blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
            blur.setState(NSVisualEffectState::Active);
            blur.addSubview(&content);
            Retained::into_super(blur)
        };

        let delegate = AgentsDelegate::new(mtm, window);
        let target: &AnyObject = delegate.as_ref();

        let title = make_label(mtm, "Agent Mission Control", 15.0, true, &NSColor::labelColor());
        let summary = make_label(mtm, "No active agents", 11.5, true, &NSColor::secondaryLabelColor());
        summary.setAlignment(NSTextAlignment::Right);
        let subtitle = make_label(
            mtm,
            "Live OSC 7501 inbox across This Mac & remote hosts — click or press Return to jump to a pane",
            11.0,
            false,
            &NSColor::secondaryLabelColor(),
        );
        subtitle.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);

        let close_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("✕"),
                Some(target),
                Some(sel!(agentsClose:)),
                mtm,
            )
        };
        close_btn.setBordered(false);
        close_btn.setContentTintColor(Some(&NSColor::secondaryLabelColor()));

        let tabs = NSSegmentedControl::initWithFrame(NSSegmentedControl::alloc(mtm), NSRect::ZERO);
        tabs.setSegmentCount(4);
        tabs.setLabel_forSegment(&NSString::from_str("All"), 0);
        tabs.setLabel_forSegment(&NSString::from_str("Needs Attention"), 1);
        tabs.setLabel_forSegment(&NSString::from_str("Working"), 2);
        tabs.setLabel_forSegment(&NSString::from_str("Done"), 3);
        tabs.setSelectedSegment(0);
        tabs.setSegmentStyle(NSSegmentStyle::Rounded);
        unsafe {
            tabs.setTarget(Some(target));
            tabs.setAction(Some(sel!(agentsFilterChanged:)));
        }

        let search = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::ZERO);
        search.setPlaceholderString(Some(&NSString::from_str(
            "Filter by agent, host, session, or OSC 7501 message…",
        )));
        unsafe { search.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        search.setFocusRingType(objc2_app_kit::NSFocusRingType::None);

        let list = AgentsListView::new(mtm, window);

        let footer_hint = make_label(
            mtm,
            "↑↓ select · ↩ jump to pane · Esc close · ⌘⇧A toggle",
            10.5,
            false,
            &NSColor::tertiaryLabelColor(),
        );
        let integrations_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Integrations…"),
                Some(target),
                Some(sel!(agentsOpenIntegrations:)),
                mtm,
            )
        };
        integrations_btn.setBezelStyle(NSBezelStyle::Push);
        integrations_btn.setControlSize(objc2_app_kit::NSControlSize::Small);

        for v in [
            &*list as &NSView,
            &*title as &NSView,
            &*summary as &NSView,
            &*subtitle as &NSView,
            &*close_btn as &NSView,
            &*tabs as &NSView,
            &*search as &NSView,
            &*footer_hint as &NSView,
            &*integrations_btn as &NSView,
        ] {
            content.addSubview(v);
        }

        host.setHidden(true);
        parent.addSubview(&host);

        Self {
            host,
            content,
            title,
            summary,
            subtitle,
            close_btn,
            tabs,
            search,
            list,
            footer_hint,
            integrations_btn,
            _delegate: delegate,
        }
    }

    /// Update the live inbox from current `HostManager` views; optionally reset
    /// search/filter when opening fresh.
    pub fn refresh(&self, hosts: &[HostView], color_rules: &[(String, String)], reset: bool) {
        let entries = collect_from_host_views(hosts, color_rules);
        let counts = AgentCounts::from_entries(&entries);

        self.summary.setStringValue(&NSString::from_str(&summary_line(counts)));
        let sum_col = if counts.blocked > 0 {
            NSColor::systemOrangeColor()
        } else if counts.error > 0 {
            NSColor::systemRedColor()
        } else if counts.working > 0 {
            NSColor::controlAccentColor()
        } else if counts.done > 0 {
            NSColor::systemGreenColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        self.summary.setTextColor(Some(&sum_col));

        self.tabs
            .setLabel_forSegment(&NSString::from_str(&format!("All ({})", counts.total)), 0);
        self.tabs.setLabel_forSegment(
            &NSString::from_str(&format!("Attention ({})", counts.attention())),
            1,
        );
        self.tabs.setLabel_forSegment(
            &NSString::from_str(&format!("Working ({})", counts.working)),
            2,
        );
        self.tabs.setLabel_forSegment(
            &NSString::from_str(&format!("Done ({})", counts.done + counts.idle)),
            3,
        );

        if reset {
            self.search.setStringValue(&NSString::from_str(""));
            self.tabs.setSelectedSegment(0);
            self.list.ivars().filter.set(AgentFilter::All);
            *self.list.ivars().query.borrow_mut() = String::new();
        }
        self.list.set_all(entries);
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
            NSSize::new((w - 140.0).max(80.0), 16.0),
        ));
        self.integrations_btn.setFrame(NSRect::new(
            NSPoint::new(w - 116.0, 6.0),
            NSSize::new(100.0, 22.0),
        ));
    }
}

fn jump_to_agent(win_id: u64, entry: &AgentEntry) {
    with_window(win_id, |w| w.close_agents_card());
    super::mux_select_window_and_block(
        win_id,
        &entry.host,
        &entry.window_id,
        Some(&entry.block_id),
    );
}

pub fn state_color(state: ProgramState) -> Retained<NSColor> {
    match state {
        ProgramState::Blocked => NSColor::systemOrangeColor(),
        ProgramState::Error => NSColor::systemRedColor(),
        ProgramState::Working => NSColor::controlAccentColor(),
        ProgramState::Done => NSColor::systemGreenColor(),
        ProgramState::Idle => NSColor::secondaryLabelColor(),
    }
}

fn state_badge_label(state: ProgramState) -> &'static str {
    match state {
        ProgramState::Blocked => "BLOCKED",
        ProgramState::Error => "ERROR",
        ProgramState::Working => "WORKING",
        ProgramState::Done => "DONE",
        ProgramState::Idle => "IDLE",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_filter_and_query_match_across_host_state_and_message() {
        let entries = vec![
            AgentEntry {
                host: "kagutsuchi".into(),
                host_display: "kagutsuchi".into(),
                host_color: None,
                session_id: "s1".into(),
                session_name: "deploy".into(),
                window_id: "w1".into(),
                window_name: "release".into(),
                block_id: "b1".into(),
                app_id: "claude".into(),
                agent_name: "Claude Code".into(),
                glyph: "✳",
                state: ProgramState::Blocked,
                message: Some("Approve migration".into()),
                cwd: Some("/srv/app".into()),
                command: Some("claude".into()),
                inferred: false,
            },
            AgentEntry {
                host: "local".into(),
                host_display: "This Mac".into(),
                host_color: None,
                session_id: "s2".into(),
                session_name: "ui".into(),
                window_id: "w2".into(),
                window_name: "mac".into(),
                block_id: "b2".into(),
                app_id: "antigravity".into(),
                agent_name: "Antigravity".into(),
                glyph: "✦",
                state: ProgramState::Working,
                message: Some("Building shaders".into()),
                cwd: Some("/Users/dev/Tachyon".into()),
                command: None,
                inferred: false,
            },
        ];

        assert_eq!(filter_entries(&entries, AgentFilter::Attention, "").len(), 1);
        assert_eq!(filter_entries(&entries, AgentFilter::Working, "").len(), 1);
        assert_eq!(filter_entries(&entries, AgentFilter::All, "kagutsuchi approve").len(), 1);
        assert_eq!(filter_entries(&entries, AgentFilter::All, "shader").len(), 1);
        assert_eq!(summary_line(AgentCounts::from_entries(&entries)), "1 blocked · 1 working");
    }
}
