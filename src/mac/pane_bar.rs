//! Subtle Rex-style per-pane header bars (`PaneHeaderView`).
//!
//! Each visible pane gets a crisp lightweight native header strip occupying its
//! reserved top row:
//! - Left: terminal glyph, pane title, and an accent-tinted remote host pill
//!   (`kagutsuchi`, …) when hosted on a remote server.
//! - Right: four compact action buttons (`split right`, `split down`,
//!   `zoom/restore`, `close`) that brighten on hover and operate on that pane.
//!
//! Mounting one small view per visible pane (matching only that header strip's
//! frame) keeps clicks below the header going straight to `TachyonView`.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSFontWeightMedium, NSFontWeightRegular, NSForegroundColorAttributeName, NSLineBreakMode,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSObject, NSPoint, NSRect, NSSize,
    NSString,
};

use super::panes;
use super::with_window;

/// Width reserved for each compact right-hand action button, in points.
const BTN_W: f64 = 20.0;
const BTN_GAP: f64 = 2.0;
const RIGHT_PAD: f64 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    SplitRight,
    SplitDown,
    Zoom,
    Close,
}

const ACTIONS: [Action; 4] = [Action::SplitRight, Action::SplitDown, Action::Zoom, Action::Close];

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PaneHeaderModel {
    pub pane_id: u64,
    pub title: String,
    /// `None` for local panes; `Some("kagutsuchi")` for remote panes.
    pub remote_host: Option<String>,
    pub focused: bool,
    pub zoomed: bool,
    pub running: bool,
}

#[derive(Default)]
pub struct HeaderState {
    window: Cell<u64>,
    model: RefCell<PaneHeaderModel>,
    hover: Cell<Option<Action>>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonPaneHeaderView"]
    #[ivars = HeaderState]
    pub struct PaneHeaderView;

    impl PaneHeaderView {
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
            let hit = self.action_at(event);
            if self.ivars().hover.get() != hit {
                self.ivars().hover.set(hit);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            if self.ivars().hover.get().is_some() {
                self.ivars().hover.set(None);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let window = self.ivars().window.get();
            let pane_id = self.ivars().model.borrow().pane_id;
            let action = self.action_at(event);
            match action {
                None => with_window(window, |w| w.focus_pane_by_id(pane_id)),
                Some(Action::SplitRight) => {
                    with_window(window, |w| w.focus_pane_by_id(pane_id));
                    super::split_pane(window, panes::Dir::Right);
                }
                Some(Action::SplitDown) => {
                    with_window(window, |w| w.focus_pane_by_id(pane_id));
                    super::split_pane(window, panes::Dir::Down);
                }
                Some(Action::Zoom) => with_window(window, |w| {
                    w.focus_pane_by_id(pane_id);
                    w.toggle_zoom();
                }),
                Some(Action::Close) => {
                    with_window(window, |w| w.focus_pane_by_id(pane_id));
                    super::close_focused_pane(window);
                }
            }
        }
    }
);

impl PaneHeaderView {
    pub fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = HeaderState::default();
        state.window.set(window);
        let this = Self::alloc(mtm).set_ivars(state);
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
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

    pub fn update(&self, model: PaneHeaderModel) {
        let mut cur = self.ivars().model.borrow_mut();
        if *cur != model {
            *cur = model;
            self.setNeedsDisplay(true);
        }
    }

    fn button_rects(&self) -> [(Action, NSRect); 4] {
        let b = self.bounds();
        let h = b.size.height;
        let bh = (h - 4.0).clamp(14.0, 18.0);
        let y = ((h - bh) / 2.0).round();
        let mut right = b.size.width - RIGHT_PAD;
        let mut rects = [(Action::Close, NSRect::ZERO); 4];
        for (idx, action) in ACTIONS.iter().rev().enumerate() {
            let x = right - BTN_W;
            rects[3 - idx] = (*action, NSRect::new(NSPoint::new(x, y), NSSize::new(BTN_W, bh)));
            right = x - BTN_GAP;
        }
        rects
    }

    fn action_at(&self, event: &NSEvent) -> Option<Action> {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        self.button_rects().into_iter().find_map(|(a, r)| {
            (p.x >= r.origin.x
                && p.x <= r.origin.x + r.size.width
                && p.y >= r.origin.y
                && p.y <= r.origin.y + r.size.height)
                .then_some(a)
        })
    }

    fn draw(&self) {
        let b = self.bounds();
        if b.size.width < 24.0 || b.size.height < 8.0 {
            return;
        }
        let m = self.ivars().model.borrow().clone();
        let hover = self.ivars().hover.get();

        // Subtle header background band and bottom hairline matching Rex.
        let bg_alpha = if m.focused { 0.06 } else { 0.03 };
        NSColor::labelColor().colorWithAlphaComponent(bg_alpha).setFill();
        NSBezierPath::fillRect(b);

        NSColor::separatorColor().colorWithAlphaComponent(0.35).setFill();
        NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(b.size.width, 1.0)));

        // Action icons on the right.
        let buttons = self.button_rects();
        let buttons_left = buttons[0].1.origin.x - 8.0;
        let show_buttons = b.size.width >= 150.0;
        if show_buttons {
            for (action, rect) in buttons {
                let is_hover = hover == Some(action);
                if is_hover {
                    let fill = if action == Action::Close {
                        NSColor::systemRedColor().colorWithAlphaComponent(0.22)
                    } else {
                        NSColor::labelColor().colorWithAlphaComponent(0.12)
                    };
                    fill.setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 4.0, 4.0).fill();
                }
                let glyph = match action {
                    Action::SplitRight => "◫",
                    Action::SplitDown => "⬓",
                    Action::Zoom => if m.zoomed { "⤡" } else { "⤢" },
                    Action::Close => "×",
                };
                let color = if is_hover {
                    if action == Action::Close {
                        NSColor::systemRedColor()
                    } else {
                        NSColor::labelColor()
                    }
                } else {
                    NSColor::secondaryLabelColor().colorWithAlphaComponent(if m.focused { 0.72 } else { 0.42 })
                };
                draw_centered(glyph, 11.5, false, &color, rect);
            }
        }

        // Left: terminal prompt glyph + title + optional remote host pill.
        let mut x = 8.0;
        let line_h = 15.0;
        let text_y = ((b.size.height - line_h) / 2.0).round();

        let icon_color = if m.running {
            NSColor::controlAccentColor()
        } else if m.focused {
            NSColor::secondaryLabelColor()
        } else {
            NSColor::tertiaryLabelColor()
        };
        draw_left(
            ">_",
            10.5,
            true,
            &icon_color,
            NSRect::new(NSPoint::new(x, text_y), NSSize::new(18.0, line_h)),
        );
        x += 19.0;

        // Optional remote host badge pill (`kagutsuchi`, etc.).
        let max_right = if show_buttons { buttons_left } else { b.size.width - 8.0 };
        let mut title_right = max_right;
        if let Some(host) = &m.remote_host {
            let pill_w = ((host.len() as f64) * 6.5 + 14.0).clamp(44.0, 120.0);
            if max_right - pill_w > x + 48.0 {
                let pill_x = max_right - pill_w;
                let pill_h = (b.size.height - 6.0).clamp(14.0, 17.0);
                let pill_y = ((b.size.height - pill_h) / 2.0).round();
                let pill_rect = NSRect::new(NSPoint::new(pill_x, pill_y), NSSize::new(pill_w, pill_h));
                NSColor::controlAccentColor().colorWithAlphaComponent(0.18).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill_rect, pill_h / 2.0, pill_h / 2.0).fill();
                let pill_fg = NSColor::controlAccentColor();
                draw_centered(host, 10.5, true, &pill_fg, pill_rect);
                title_right = pill_x - 8.0;
            }
        }

        let title_w = (title_right - x).max(0.0);
        if title_w > 12.0 {
            let fg = if m.focused {
                NSColor::labelColor().colorWithAlphaComponent(0.92)
            } else {
                NSColor::secondaryLabelColor().colorWithAlphaComponent(0.75)
            };
            draw_left(
                &m.title,
                11.5,
                m.focused,
                &fg,
                NSRect::new(NSPoint::new(x, text_y), NSSize::new(title_w, line_h)),
            );
        }
    }
}

fn draw_left(text: &str, size: f64, medium: bool, color: &NSColor, rect: NSRect) {
    let para = NSMutableParagraphStyle::new();
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

fn draw_centered(text: &str, size: f64, medium: bool, color: &NSColor, rect: NSRect) {
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(objc2_app_kit::NSTextAlignment::Center);
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
