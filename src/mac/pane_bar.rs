//! Optional minimal per-pane header strip (`PaneHeaderView`), off by default
//! (`window.pane_headers = never`).
//!
//! When explicitly enabled (`window.pane_headers = auto` or `always`), displays
//! a clean title + OSC 7501 status pill + remote host badge without chrome
//! action buttons (pane split/zoom/close stay keyboard-driven: `⌘D`, `⌘⇧D`,
//! `⌘⇧↩`, `⌘W`).

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSFontWeightMedium, NSFontWeightRegular, NSForegroundColorAttributeName, NSLineBreakMode,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSObject, NSPoint, NSRect, NSSize,
    NSString,
};

use super::with_window;
use crate::term::{ProgramState, ProgramStatus};

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PaneHeaderModel {
    pub pane_id: u64,
    pub title: String,
    /// `None` for local panes; `Some("kagutsuchi")` for remote panes.
    pub remote_host: Option<String>,
    /// Live OSC 7501 agent/program status, if any.
    pub status: Option<ProgramStatus>,
    pub focused: bool,
    pub running: bool,
}

#[derive(Default)]
pub struct HeaderState {
    window: Cell<u64>,
    model: RefCell<PaneHeaderModel>,
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

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let window = self.ivars().window.get();
            let pane_id = self.ivars().model.borrow().pane_id;
            with_window(window, |w| w.focus_pane_by_id(pane_id));
        }
    }
);

impl PaneHeaderView {
    pub fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = HeaderState::default();
        state.window.set(window);
        let this = Self::alloc(mtm).set_ivars(state);
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setClipsToBounds(true);
        view
    }

    pub fn update(&self, model: PaneHeaderModel) {
        let mut cur = self.ivars().model.borrow_mut();
        if *cur != model {
            *cur = model;
            self.setNeedsDisplay(true);
        }
    }

    fn draw(&self) {
        let b = self.bounds();
        if b.size.width < 24.0 || b.size.height < 8.0 {
            return;
        }
        NSBezierPath::clipRect(b);
        let m = self.ivars().model.borrow().clone();

        let bg_alpha = if m.focused { 0.06 } else { 0.03 };
        NSColor::labelColor().colorWithAlphaComponent(bg_alpha).setFill();
        NSBezierPath::fillRect(b);

        NSColor::separatorColor().colorWithAlphaComponent(0.35).setFill();
        NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(b.size.width, 1.0)));

        // Left: agent/terminal glyph + title + optional status & remote host pills.
        let mut x = 8.0;
        let line_h = 15.0;
        let text_y = ((b.size.height - line_h) / 2.0).round();

        let (glyph, icon_color) = app_badge(
            m.status.as_ref().and_then(|s| s.app.as_deref()),
            m.status.as_ref().map(|s| s.state),
            m.running,
            m.focused,
        );
        draw_left(
            glyph,
            if glyph == ">_" { 10.5 } else { 12.0 },
            true,
            &icon_color,
            NSRect::new(NSPoint::new(x, text_y), NSSize::new(18.0, line_h)),
        );
        x += 19.0;

        // Right-aligned pills: remote host badge (`kagutsuchi`), then live OSC 7501 status pill.
        let mut title_right = b.size.width - 8.0;
        if let Some(host) = &m.remote_host {
            let pill_w = ((host.len() as f64) * 6.5 + 14.0).clamp(44.0, 120.0);
            if title_right - pill_w > x + 48.0 {
                let pill_x = title_right - pill_w;
                let pill_h = (b.size.height - 6.0).clamp(14.0, 17.0);
                let pill_y = ((b.size.height - pill_h) / 2.0).round();
                let pill_rect = NSRect::new(NSPoint::new(pill_x, pill_y), NSSize::new(pill_w, pill_h));
                NSColor::controlAccentColor().colorWithAlphaComponent(0.18).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill_rect, pill_h / 2.0, pill_h / 2.0).fill();
                let pill_fg = NSColor::controlAccentColor();
                draw_centered(host, 10.5, true, &pill_fg, pill_rect);
                title_right = pill_x - 6.0;
            }
        }
        if let Some(st) = &m.status {
            let (prefix, tint) = status_style(st.state);
            let raw = st.short_text();
            let text = if raw.chars().count() > 22 {
                let cut: String = raw.chars().take(21).collect();
                format!("{prefix} {cut}…")
            } else {
                format!("{prefix} {raw}")
            };
            let pill_w = ((text.chars().count() as f64) * 6.2 + 14.0).clamp(52.0, 170.0);
            if title_right - pill_w > x + 48.0 {
                let pill_x = title_right - pill_w;
                let pill_h = (b.size.height - 6.0).clamp(14.0, 17.0);
                let pill_y = ((b.size.height - pill_h) / 2.0).round();
                let pill_rect = NSRect::new(NSPoint::new(pill_x, pill_y), NSSize::new(pill_w, pill_h));
                tint.colorWithAlphaComponent(0.18).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill_rect, pill_h / 2.0, pill_h / 2.0).fill();
                draw_centered(&text, 10.0, true, &tint, pill_rect);
                title_right = pill_x - 6.0;
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

pub(crate) fn app_badge(
    app: Option<&str>,
    state: Option<ProgramState>,
    running: bool,
    focused: bool,
) -> (&'static str, Retained<NSColor>) {
    if let Some(name) = app {
        match name.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => {
                let c = NSColor::colorWithSRGBRed_green_blue_alpha(0.87, 0.49, 0.36, 1.0);
                return ("✳", c);
            }
            "codex" => return ("◎", NSColor::systemTealColor()),
            "opencode" | "open-code" => return ("◈", NSColor::systemIndigoColor()),
            "pi" => return ("π", NSColor::systemPurpleColor()),
            "antigravity" | "agy" => return ("✦", NSColor::systemBlueColor()),
            _ => {}
        }
    }
    let color = match state {
        Some(ProgramState::Blocked) => NSColor::systemOrangeColor(),
        Some(ProgramState::Error) => NSColor::systemRedColor(),
        Some(ProgramState::Done) => NSColor::systemGreenColor(),
        Some(ProgramState::Working) => NSColor::controlAccentColor(),
        _ if running => NSColor::controlAccentColor(),
        _ if focused => NSColor::secondaryLabelColor(),
        _ => NSColor::tertiaryLabelColor(),
    };
    (">_", color)
}

pub(crate) fn status_style(state: ProgramState) -> (&'static str, Retained<NSColor>) {
    match state {
        ProgramState::Working => ("●", NSColor::controlAccentColor()),
        ProgramState::Blocked => ("▲", NSColor::systemOrangeColor()),
        ProgramState::Done => ("✓", NSColor::systemGreenColor()),
        ProgramState::Error => ("✕", NSColor::systemRedColor()),
        ProgramState::Idle => ("○", NSColor::secondaryLabelColor()),
    }
}
