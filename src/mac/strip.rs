//! The tab strip as a floating Liquid Glass capsule, used when the window is
//! on glass: an NSGlassEffectView near the bottom edge, over the window's own
//! glass, with the tab names drawn by AppKit in the system font on top of it,
//! as Safari and Finder draw theirs.
//!
//! Without glass, the renderer draws the strip in cells instead.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSFontWeightMedium, NSFontWeightRegular, NSForegroundColorAttributeName, NSGlassEffectView,
    NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder, NSTextAlignment, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString, NSObject, NSPoint, NSRect,
    NSSize, NSString,
};

use super::renderer::Outcome;
use super::with_window;

/// Height of the capsule, and of the band reserved for it at the bottom of
/// the window, in points.
pub const CAPSULE_HEIGHT: f64 = 28.0;
pub const BAND_HEIGHT: f64 = 40.0;
/// The widest a tab gets, in points, so a few tabs make a compact capsule.
const MAX_TAB_WIDTH: f64 = 220.0;
/// Space kept between the capsule and the window's sides.
const SIDE_MARGIN: f64 = 12.0;

#[derive(Default)]
pub struct StripState {
    window: Cell<u64>,
    tabs: RefCell<Vec<(String, Option<Outcome>)>>,
    active: Cell<usize>,
}

define_class!(
    /// Draws the tab names on the capsule and takes clicks on them.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonTabStripView"]
    #[ivars = StripState]
    pub struct TabStripView;

    impl TabStripView {
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

        /// A click shows that tab.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(i) = self.tab_at(event) {
                with_window(self.ivars().window.get(), |w| w.switch_to(i));
            }
        }

        /// A middle-click closes it.
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            if event.buttonNumber() != 2 {
                return;
            }
            if let Some(i) = self.tab_at(event) {
                with_window(self.ivars().window.get(), |w| {
                    if w.tabs.len() > 1 {
                        w.remove_tab(i);
                    }
                });
            }
        }
    }
);

impl TabStripView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = StripState::default();
        state.window.set(window);
        let this = Self::alloc(mtm).set_ivars(state);
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }

    /// Show these tabs; redraws only when something changed.
    pub fn set_tabs(&self, tabs: Vec<(String, Option<Outcome>)>, active: usize) {
        let s = self.ivars();
        if *s.tabs.borrow() == tabs && s.active.get() == active {
            return;
        }
        *s.tabs.borrow_mut() = tabs;
        s.active.set(active);
        self.setNeedsDisplay(true);
    }

    fn tab_at(&self, event: &NSEvent) -> Option<usize> {
        let n = self.ivars().tabs.borrow().len();
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        let w = self.bounds().size.width / n.max(1) as f64;
        (n > 0 && w > 0.0).then(|| ((p.x / w).floor().max(0.0) as usize).min(n - 1))
    }

    fn draw(&self) {
        let tabs = self.ivars().tabs.borrow();
        let active = self.ivars().active.get();
        let bounds = self.bounds();
        let n = tabs.len();
        if n == 0 {
            return;
        }
        let w = bounds.size.width / n as f64;
        let h = bounds.size.height;
        for (i, (label, unseen)) in tabs.iter().enumerate() {
            let cell = NSRect::new(NSPoint::new(i as f64 * w, 0.0), NSSize::new(w, h));
            if i == active {
                // The tab in front: a softer pill inside the capsule.
                let pill = inset(cell, 3.0, 3.0);
                let r = pill.size.height / 2.0;
                NSColor::labelColor().colorWithAlphaComponent(0.13).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, r, r).fill();
            }
            let text = label_text(label, *unseen, i == active);
            let line = 16.0;
            let rect = NSRect::new(
                NSPoint::new(cell.origin.x + 10.0, (h - line) / 2.0 - 1.0),
                NSSize::new((w - 20.0).max(0.0), line),
            );
            text.drawInRect(rect);
        }
    }
}

fn inset(r: NSRect, dx: f64, dy: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(r.origin.x + dx, r.origin.y + dy),
        NSSize::new((r.size.width - 2.0 * dx).max(0.0), (r.size.height - 2.0 * dy).max(0.0)),
    )
}

/// A tab's name in the system font, centred and cut with an ellipsis, after
/// a green or red dot when a command finished there unseen.
fn label_text(label: &str, unseen: Option<Outcome>, active: bool) -> Retained<NSAttributedString> {
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(NSTextAlignment::Center);
    para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let weight = unsafe { if active { NSFontWeightMedium } else { NSFontWeightRegular } };
    let font = NSFont::systemFontOfSize_weight(12.0 as CGFloat, weight);
    let attrs = |color: &NSColor| {
        let keys: [&NSAttributedStringKey; 3] =
            unsafe { [NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName] };
        let values: [&AnyObject; 3] = [font.as_ref(), color.as_ref(), para.as_ref()];
        NSDictionary::from_slices(&keys, &values)
    };
    let out = NSMutableAttributedString::new();
    if let Some(outcome) = unseen {
        let dot = if outcome == Outcome::Failed { NSColor::systemRedColor() } else { NSColor::systemGreenColor() };
        // SAFETY: the attributes are a font, a colour and a paragraph style
        // under their AppKit keys.
        let run = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &NSString::from_str("● "),
                Some(&attrs(&dot)),
            )
        };
        out.appendAttributedString(&run);
    }
    let color = if active { NSColor::labelColor() } else { NSColor::secondaryLabelColor() };
    // SAFETY: as above.
    let run = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(label),
            Some(&attrs(&color)),
        )
    };
    out.appendAttributedString(&run);
    out.into_super()
}

/// The capsule: glass with the strip view on it.
pub struct Capsule {
    pub glass: Retained<NSGlassEffectView>,
    pub view: Retained<TabStripView>,
}

impl Capsule {
    /// A capsule for window `window`, added to `parent` (the terminal view).
    pub fn new(mtm: MainThreadMarker, parent: &NSView, window: u64) -> Capsule {
        let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), NSRect::ZERO);
        glass.setCornerRadius(CAPSULE_HEIGHT / 2.0);
        let view = TabStripView::new(mtm, window);
        glass.setContentView(Some(&view));
        parent.addSubview(&glass);
        Capsule { glass, view }
    }

    /// Centre the capsule along the bottom of a view `width` points wide.
    pub fn place(&self, width: f64, tabs: usize) {
        self.place_at(width, tabs, ((BAND_HEIGHT - CAPSULE_HEIGHT) / 2.0).round(), SIDE_MARGIN);
    }

    /// Centre the capsule across a view `width` points wide, at `y` from
    /// its bottom, keeping `margin` points from both sides (clear of the
    /// window buttons, in a title bar).
    pub fn place_at(&self, width: f64, tabs: usize, y: f64, margin: f64) {
        let room = (width - 2.0 * margin).max(0.0);
        let tab_w = (room / tabs.max(1) as f64).min(MAX_TAB_WIDTH);
        let w = (tab_w * tabs as f64).min(room);
        let frame = NSRect::new(NSPoint::new(((width - w) / 2.0).round(), y.round()), NSSize::new(w, CAPSULE_HEIGHT));
        self.glass.setFrame(frame);
        self.view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
    }
}
