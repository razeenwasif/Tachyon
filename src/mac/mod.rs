//! macOS backend: AppKit windows with Metal layers, each driven by its own
//! display link and talking to its own shell over a Unix PTY.
//!
//! Threading follows the Windows backend. Each window's reader thread owns
//! parsing: it blocks in `read`, feeds the bytes to that window's terminal
//! under its lock, and wakes the main thread. The main thread handles input
//! and draws.
//!
//! Drawing is paced by each view's `CADisplayLink`, and a link is paused
//! whenever its window had nothing to draw for a couple of refreshes. An idle
//! window makes no frames and takes no wake-ups until its shell prints or a key
//! arrives.
//!
//! Windows behave like Terminal.app's: ⌘N opens another, each with its own
//! shell; closing one ends only that shell; the app keeps running with no
//! windows, and clicking the Dock icon opens a fresh one.

mod boxdraw;
mod chart;
mod dock;
mod font;
mod images;
pub mod host;
mod input;
mod ledger;
mod links;
mod notify;
mod panes;
mod power;
mod pty;
mod receipts;
mod renderer;
mod selection;
mod strip;
mod suggest;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSColor, NSCursor, NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSMenu, NSMenuItem, NSPasteboard,
    NSPasteboardTypeString, NSRequestUserAttentionType, NSResponder, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSTextInputClient, NSWindow, NSWindowButton, NSWindowDelegate, NSWindowStyleMask,
    NSWindowTitleVisibility, NSWorkspace, NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua,
    NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions, NSColorSpace, NSGlassEffectView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
};
use objc2_quick_look_ui::{QLPreviewPanel, QLPreviewPanelDataSource};
use objc2_app_kit::{NSControl, NSControlTextEditingDelegate, NSSearchField, NSSearchFieldDelegate, NSTextFieldDelegate, NSTextView};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSData, NSNotFound, NSRange, NSUserDefaults, NSNotification, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop,
    NSInteger, NSRunLoopCommonModes, NSSize, NSString, NSURL,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage, CGColor, CGColorSpace,
    CGImageAlphaInfo,
};
use objc2_quartz_core::{
    kCAAnimationDiscrete, CADisplayLink, CAKeyframeAnimation, CALayer, CAMediaTiming, CAMetalLayer,
    CATransaction,
};
use parking_lot::Mutex;

use crate::config::{Appearance, Backdrop, Config, Contrast, OptionAsMeta, ThemeConfig};
use crate::term::jobs::JobRef;
use crate::term::rewind::RewindLog;
use crate::term::{jobs, lens, CommandEvent, CommandState, Processor, RowMark, Term, TermMode};
use rustc_hash::FxHashMap;
use pty::Pty;
use input::{MouseButton, MouseEventKind};
use renderer::{CursorOverlay, Decor, Gpu, Outcome, PaneFrame, Renderer, Strip};
use selection::{Pos, Selection, Unit};

const ICON_SVG: &[u8] = include_bytes!("../../assets/icon/tachyon.svg");

/// Region in the top-left corner, in points, where the hidden window buttons
/// fade in.
const BUTTON_ZONE: (f64, f64) = (84.0, 34.0);

thread_local! {
    /// The file Quick Look is showing, from the last ⌘-click on a path.
    static QUICK_LOOK: RefCell<Option<Retained<NSURL>>> = const { RefCell::new(None) };
    /// Posts notifications for long commands; `None` outside the app bundle.
    static NOTIFIER: RefCell<Option<notify::Notifier>> = const { RefCell::new(None) };
    /// All main-thread state. AppKit calls into the views and delegates; each
    /// carries its window's id and forwards here.
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// The session host, when shells persist.
    static HOST: RefCell<Option<host::Client>> = const { RefCell::new(None) };
    /// Commands run before, for inline suggestions; `None` when they're off.
    static SUGGESTIONS: RefCell<Option<suggest::History>> = const { RefCell::new(None) };
    /// A layout save is queued.
    static LAYOUT_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Tachyon is quitting: the host's shells stay, for next time.
static QUITTING: AtomicBool = AtomicBool::new(false);
/// Ids for shells run here rather than at the host, apart from the host's.
static LOCAL_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 32);

/// Run `f` against the app state, unless it is already borrowed further up the
/// stack (AppKit can re-enter us from inside a call we made).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// Run `f` against one window's state.
fn with_window(id: u64, f: impl FnOnce(&mut TermWindow)) {
    with_app(|app| {
        if let Some(w) = app.windows.iter_mut().find(|w| w.id == id) {
            f(w);
        }
    });
}

/// Run `f` against the window holding a pane (a shell session), with the
/// index of the pane's tab.
fn with_tab(pane: u64, f: impl FnOnce(&mut TermWindow, usize)) {
    with_app(|app| {
        for w in &mut app.windows {
            if let Some(i) = w.tabs.iter().position(|t| t.index_of(pane).is_some()) {
                f(w, i);
                return;
            }
        }
    });
}

/// Like `with_window`, returning what `f` returns.
fn query_window<R>(id: u64, f: impl FnOnce(&mut TermWindow) -> R) -> Option<R> {
    with_app(|app| app.windows.iter_mut().find(|w| w.id == id).map(f)).flatten()
}

/// The text of an `insertText:` / `setMarkedText:` argument, which AppKit
/// passes as either an NSString or an NSAttributedString.
fn input_string(obj: &AnyObject) -> String {
    if let Some(a) = obj.downcast_ref::<NSAttributedString>() {
        a.string().to_string()
    } else if let Some(s) = obj.downcast_ref::<NSString>() {
        s.to_string()
    } else {
        String::new()
    }
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    /// Secure Keyboard Entry: while on, other apps can't read keystrokes.
    /// Reference-counted per process, so every enable needs one disable.
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
}

/// Whether the program on this PTY is asking for a password: echo off while
/// the terminal is still in line mode, which is how sudo, ssh and getpass
/// read one. Full-screen programs turn echo off too, but also leave line mode.
fn asking_for_password(fd: i32) -> bool {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::tcgetattr(fd, &mut t) } == 0;
    ok && t.c_lflag & libc::ECHO == 0 && t.c_lflag & libc::ICANON != 0
}

/// 418226903 as 418,226,903.
fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Whether a view is drawn in Dark Mode.
fn appearance_is_dark(view: &NSView) -> bool {
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark]);
    view.effectiveAppearance()
        .bestMatchFromAppearancesWithNames(&names)
        .is_some_and(|n| &*n == dark)
}

/// The system accent colour (System Settings > Appearance), in sRGB.
fn accent_color() -> Option<[u8; 3]> {
    let c = NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some([byte(c.redComponent()), byte(c.greenComponent()), byte(c.blueComponent())])
}

fn mix_rgb(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    std::array::from_fn(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8)
}

fn srgb_color(c: [u8; 3], alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(c[0] as f64 / 255.0, c[1] as f64 / 255.0, c[2] as f64 / 255.0, alpha)
}

/// Show `path` in Quick Look, as the Finder does with the space bar.
fn quick_look(path: std::path::PathBuf) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    QUICK_LOOK.with(|q| *q.borrow_mut() = Some(url));
    // SAFETY: main thread; the panel finds the view through the responder
    // chain and the view keeps itself alive as the data source.
    unsafe {
        let Some(panel) = QLPreviewPanel::sharedPreviewPanel(mtm) else {
            return;
        };
        if panel.isVisible() {
            panel.reloadData();
        } else {
            panel.makeKeyAndOrderFront(None);
        }
    }
}

/// A notification was clicked: show its window, tab and pane.
fn show_tab(pane: u64) {
    let window = with_app(|app| {
        for w in &mut app.windows {
            if let Some(i) = w.tabs.iter().position(|t| t.index_of(pane).is_some()) {
                w.switch_to(i);
                if let Some(p) = w.tabs[i].index_of(pane) {
                    w.focus_pane(p);
                }
                return Some(w.window.clone());
            }
        }
        None
    })
    .flatten();
    if let (Some(window), Some(mtm)) = (window, MainThreadMarker::new()) {
        if window.isMiniaturized() {
            window.deminiaturize(None);
        }
        window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();
    }
}

/// What double-clicking a title bar does. Edge-to-edge, the invisible
/// title bar sits over the top rows of text, where a double-click selects a
/// word, so it does nothing (for this app only; System Settings is
/// untouched). With a visible title bar, the system setting applies.
fn set_double_click_action(title_bar: bool) {
    let defaults = NSUserDefaults::standardUserDefaults();
    let key = NSString::from_str("AppleActionOnDoubleClick");
    // SAFETY: storing an NSString value under an NSString key.
    unsafe {
        if title_bar {
            defaults.removeObjectForKey(&key);
        } else {
            defaults.setObject_forKey(Some(&NSString::from_str("None")), &key);
        }
    }
}

/// The window id of a terminal view.
fn view_id(view: &NSView) -> u64 {
    view.downcast_ref::<TachyonView>().map_or(0, TachyonView::id)
}

/// Per-instance storage for the Objective-C classes: which window they serve.
#[derive(Default)]
struct WindowId {
    id: Cell<u64>,
}

define_class!(
    /// A terminal surface. Layer-backed by a `CAMetalLayer`.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonView"]
    #[ivars = WindowId]
    struct TachyonView;

    impl TachyonView {
        #[unsafe(method_id(makeBackingLayer))]
        fn make_backing_layer(&self) -> Retained<CALayer> {
            Retained::into_super(CAMetalLayer::new())
        }

        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            with_window(self.id(), |w| w.needs_redraw = true);
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            // Glass shows through the default background.
            false
        }

        /// Light or Dark Mode changed, or the accent colour with it.
        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn view_did_change_effective_appearance(&self) {
            with_window(self.id(), TermWindow::apply_appearance);
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // Clicks are for selecting text. The window moves with a ⌃⌘-drag
        // anywhere or a drag along the top edge; see `mouse_down`.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_down(event, MouseButton::Left));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_dragged(event, MouseButton::Left));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_up(event, MouseButton::Left));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_down(event, MouseButton::Right));
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_dragged(event, MouseButton::Right));
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_up(event, MouseButton::Right));
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_down(event, MouseButton::Middle));
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_dragged(event, MouseButton::Middle));
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.mouse_up(event, MouseButton::Middle));
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), TermWindow::copy);
        }

        #[unsafe(method(selectAll:))]
        fn select_all(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), TermWindow::select_all);
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> Bool {
            let f = event.modifierFlags();
            let plain_command = f.contains(NSEventModifierFlags::Command)
                && !f.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option);
            let shift = f.contains(NSEventModifierFlags::Shift);
            let chars = event.charactersIgnoringModifiers().map(|c| c.to_string()).unwrap_or_default();
            if plain_command && chars == "=" {
                with_window(self.id(), |w| w.zoom_font(1.0));
                return Bool::YES;
            }
            // ⌘1 to ⌘8 pick a tab, ⌘9 the last one, as in Safari.
            if plain_command && !shift {
                if let Some(n) = chars.parse::<usize>().ok().filter(|n| (1..=9).contains(n)) {
                    with_window(self.id(), |w| w.select_tab_number(n));
                    return Bool::YES;
                }
            }
            // ⌃⇥ and ⌃⇧⇥ step through tabs.
            const TAB_KEY: u16 = 48;
            if f.contains(NSEventModifierFlags::Control) && !f.contains(NSEventModifierFlags::Command) && event.keyCode() == TAB_KEY {
                with_window(self.id(), |w| w.cycle_tab(if shift { -1 } else { 1 }));
                return Bool::YES;
            }
            unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }

        #[unsafe(method(newTab:))]
        fn new_tab(&self, _sender: Option<&AnyObject>) {
            open_tab(self.id());
        }

        #[unsafe(method(tachyonRewindBack:))]
        fn rewind_back(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.rewind_step(true));
        }

        #[unsafe(method(tachyonRewindScrub:))]
        fn rewind_scrub(&self, sender: Option<&AnyObject>) {
            let Some(slider) = sender.and_then(|s| s.downcast_ref::<objc2_app_kit::NSSlider>()) else {
                return;
            };
            let index = slider.doubleValue().round().max(0.0) as usize;
            with_window(self.id(), |w| w.rewind_to(index));
        }

        #[unsafe(method(tachyonRewindForward:))]
        fn rewind_forward(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.rewind_step(false));
        }

        #[unsafe(method(tachyonFollowJob:))]
        fn follow_job(&self, _sender: Option<&AnyObject>) {
            follow_job(self.id(), None);
        }

        #[unsafe(method(tachyonSplitRight:))]
        fn split_right(&self, _sender: Option<&AnyObject>) {
            split_pane(self.id(), panes::Dir::Right);
        }

        #[unsafe(method(tachyonSplitDown:))]
        fn split_down(&self, _sender: Option<&AnyObject>) {
            split_pane(self.id(), panes::Dir::Down);
        }

        #[unsafe(method(tachyonPaneLeft:))]
        fn pane_left(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.move_focus(-1, 0));
        }

        #[unsafe(method(tachyonPaneRight:))]
        fn pane_right(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.move_focus(1, 0));
        }

        #[unsafe(method(tachyonPaneUp:))]
        fn pane_up(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.move_focus(0, -1));
        }

        #[unsafe(method(tachyonPaneDown:))]
        fn pane_down(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.move_focus(0, 1));
        }

        #[unsafe(method(tachyonNotch:))]
        fn toggle_notch(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), TermWindow::toggle_notch);
        }

        #[unsafe(method(tachyonTitleBar:))]
        fn toggle_title_bar(&self, _sender: Option<&AnyObject>) {
            let on = with_app(|app| {
                app.title_bar = !app.title_bar;
                let on = app.title_bar;
                app.windows.iter_mut().for_each(|w| w.set_title_bar(on));
                on
            });
            if let Some(on) = on {
                set_double_click_action(on);
            }
        }

        #[unsafe(method(tachyonChart:))]
        fn toggle_chart(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.toggle_chart(None));
        }

        #[unsafe(method(tachyonLedger:))]
        fn toggle_ledger(&self, _sender: Option<&AnyObject>) {
            ledger::SHOWN.fetch_xor(true, Ordering::Relaxed);
            with_app(|app| app.windows.iter_mut().for_each(|w| w.request_redraw()));
        }

        #[unsafe(method(tachyonFind:))]
        fn find(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), TermWindow::open_find);
        }

        #[unsafe(method(tachyonFindNext:))]
        fn find_next(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.find_step(true));
        }

        #[unsafe(method(tachyonFindPrevious:))]
        fn find_previous(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.find_step(false));
        }

        #[unsafe(method(tachyonFindRegex:))]
        fn find_regex(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), TermWindow::find_changed);
        }

        #[unsafe(method(tachyonKeepAwake:))]
        fn keep_awake(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.toggle_awake());
        }

        #[unsafe(method(tachyonZoomPane:))]
        fn zoom_pane(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.toggle_zoom());
        }

        /// Resize pointers over the dividers between panes.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let rects = query_window(self.id(), |w| w.divider_cursor_rects()).unwrap_or_default();
            for (rect, side_by_side) in rects {
                let cursor = if side_by_side {
                    NSCursor::columnResizeCursorInDirections(objc2_app_kit::NSHorizontalDirections::All)
                } else {
                    NSCursor::rowResizeCursorInDirections(objc2_app_kit::NSVerticalDirections::All)
                };
                self.addCursorRect_cursor(rect, &cursor);
            }
        }

        #[unsafe(method(tachyonCloseTab:))]
        fn close_tab(&self, _sender: Option<&AnyObject>) {
            close_active_tab(self.id());
        }

        #[unsafe(method(tachyonPreviousTab:))]
        fn previous_tab(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.cycle_tab(-1));
        }

        #[unsafe(method(tachyonNextTab:))]
        fn next_tab(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.cycle_tab(1));
        }

        #[unsafe(method(acceptsPreviewPanelControl:))]
        fn accepts_preview_panel_control(&self, _panel: Option<&QLPreviewPanel>) -> bool {
            QUICK_LOOK.with(|q| q.borrow().is_some())
        }

        #[unsafe(method(beginPreviewPanelControl:))]
        fn begin_preview_panel_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(panel) = panel {
                unsafe {
                    panel.setDataSource(Some(ProtocolObject::from_ref(self)));
                    panel.reloadData();
                }
            }
        }

        #[unsafe(method(endPreviewPanelControl:))]
        fn end_preview_panel_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(panel) = panel {
                unsafe { panel.setDataSource(None) };
            }
        }

        #[unsafe(method(tachyonPreviousCommand:))]
        fn previous_command(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.jump_command(true));
        }

        #[unsafe(method(tachyonNextCommand:))]
        fn next_command(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.jump_command(false));
        }

        #[unsafe(method(tachyonBiggerFont:))]
        fn bigger_font(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.zoom_font(1.0));
        }

        #[unsafe(method(tachyonSmallerFont:))]
        fn smaller_font(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.zoom_font(-1.0));
        }

        #[unsafe(method(tachyonResetFont:))]
        fn reset_font(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), |w| w.zoom_font(0.0));
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            unsafe { msg_send![super(self), setFrameSize: size] }
            with_window(self.id(), TermWindow::layout);
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            with_window(self.id(), TermWindow::rescale);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // Keys for the input system go through interpretKeyEvents, outside
            // the window borrow: it calls straight back into insertText: and
            // setMarkedText: below.
            if query_window(self.id(), |w| w.key_down(event)).unwrap_or(false) {
                self.interpretKeyEvents(&NSArray::from_slice(&[event]));
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.scroll(event));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = event.locationInWindow();
            with_window(self.id(), |w| {
                w.hover(Some(p));
                w.mouse_moved(event);
            });
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            with_window(self.id(), |w| w.hover(None));
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            with_window(self.id(), TermWindow::paste);
        }

        /// Display link callback: one per refresh while the link runs.
        #[unsafe(method(tick:))]
        fn tick(&self, _link: &CADisplayLink) {
            with_window(self.id(), TermWindow::tick);
        }
    }

    /// Quick Look asks the view what to show: the path last ⌘-clicked.
    unsafe impl NSObjectProtocol for TachyonView {}

    /// The find bar's search field reports to its terminal view.
    unsafe impl NSControlTextEditingDelegate for TachyonView {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _n: &NSNotification) {
            with_window(self.id(), TermWindow::find_changed);
        }

        /// Return finds the next match (older), Shift-Return the previous,
        /// Escape closes the bar.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_do_command(&self, _control: &NSControl, _view: &NSTextView, command: Sel) -> bool {
            let id = self.id();
            if command == sel!(insertNewline:) {
                let shift = NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Shift);
                with_window(id, |w| w.find_step(!shift));
                true
            } else if command == sel!(cancelOperation:) {
                with_window(id, TermWindow::close_find);
                true
            } else {
                false
            }
        }
    }

    unsafe impl NSTextFieldDelegate for TachyonView {}
    unsafe impl NSSearchFieldDelegate for TachyonView {}

    unsafe impl QLPreviewPanelDataSource for TachyonView {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        fn number_of_preview_items(&self, _panel: Option<&QLPreviewPanel>) -> NSInteger {
            QUICK_LOOK.with(|q| q.borrow().is_some() as NSInteger)
        }

        /// Returned autoreleased, as Objective-C expects of a getter. NSURL
        /// is a Quick Look preview item.
        #[unsafe(method(previewPanel:previewItemAtIndex:))]
        fn preview_item(&self, _panel: Option<&QLPreviewPanel>, _index: NSInteger) -> *mut NSURL {
            match QUICK_LOOK.with(|q| q.borrow().clone()) {
                Some(url) => Retained::autorelease_return(url),
                None => std::ptr::null_mut(),
            }
        }
    }

    /// The text-input protocol: how dead keys (Option-E then E gives é) and
    /// input methods (Japanese, Chinese, Korean) reach the terminal. Committed
    /// text goes to the shell; text still being composed is shown at the
    /// cursor until it is committed.
    unsafe impl NSTextInputClient for TachyonView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _range: NSRange) {
            let text = input_string(string);
            with_window(self.id(), |w| w.type_text(&text));
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command_by_selector(&self, _selector: Sel) {
            // Editing commands (moveLeft:, insertNewline: ...) come from keys
            // Tachyon already sends to the shell itself.
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replace: NSRange) {
            let text = input_string(string);
            with_window(self.id(), |w| w.set_preedit(text));
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            with_window(self.id(), |w| w.set_preedit(String::new()));
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            NSRange::new(NSNotFound as usize, 0)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            match query_window(self.id(), |w| w.preedit.encode_utf16().count()) {
                Some(n) if n > 0 => NSRange::new(0, n),
                _ => NSRange::new(NSNotFound as usize, 0),
            }
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            query_window(self.id(), |w| !w.preedit.is_empty()).unwrap_or(false)
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(&self, _range: NSRange, _actual: *mut NSRange) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        /// Where the input method's candidate window should appear: under
        /// the cursor, in screen coordinates.
        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, _range: NSRange, _actual: *mut NSRange) -> NSRect {
            query_window(self.id(), TermWindow::cursor_screen_rect).unwrap_or(NSRect::ZERO)
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _point: NSPoint) -> usize {
            NSNotFound as usize
        }
    }
);

define_class!(
    /// The terminal window. Beyond NSWindow, it routes left-button events
    /// inside the terminal straight to the terminal view, so the (transparent)
    /// title bar strip over the top rows cannot claim a click on the text.
    /// Clicks on the revealed window buttons and along the edges, where the
    /// window is resized, take the normal path.
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonWindow"]
    struct TachyonWindow;

    impl TachyonWindow {
        /// The green button, ⌃⌘F and View > Enter Full Screen: around the
        /// notch with `window.fullscreen = notch`, else macOS's own.
        #[unsafe(method(toggleFullScreen:))]
        fn toggle_full_screen(&self, sender: Option<&AnyObject>) {
            let notch = with_app(|app| app.notch_full_screen).unwrap_or(false);
            if let (true, Some(view)) = (notch, self.terminal_view()) {
                let id = view_id(&view);
                DispatchQueue::main().exec_async(move || with_window(id, TermWindow::toggle_notch));
                return;
            }
            unsafe { msg_send![super(self), toggleFullScreen: sender] }
        }

        /// Borderless around the notch, it still takes keys.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            true
        }

        #[unsafe(method(sendEvent:))]
        fn send_event(&self, event: &NSEvent) {
            let t = event.r#type();
            let left = t == NSEventType::LeftMouseDown
                || t == NSEventType::LeftMouseDragged
                || t == NSEventType::LeftMouseUp;
            if left {
                if let Some(view) = self.terminal_view() {
                    // An inactive window lets AppKit take the click, so it can
                    // activate the window first.
                    if self.isKeyWindow() && self.owns_click(&view, event.locationInWindow()) {
                        match t {
                            NSEventType::LeftMouseDown => view.mouseDown(event),
                            NSEventType::LeftMouseDragged => view.mouseDragged(event),
                            _ => view.mouseUp(event),
                        }
                        return;
                    }
                }
            }
            unsafe { msg_send![super(self), sendEvent: event] }
        }
    }
);

impl TachyonWindow {
    /// Margin along the window edges left to AppKit for resizing, in points.
    const RESIZE_MARGIN: f64 = 4.0;

    /// The terminal view: the content view, or inside the glass that hosts it.
    fn terminal_view(&self) -> Option<Retained<NSView>> {
        fn find(view: Retained<NSView>, depth: u32) -> Option<Retained<NSView>> {
            if view.isKindOfClass(<TachyonView as objc2::ClassType>::class()) {
                return Some(view);
            }
            if depth == 0 {
                return None;
            }
            view.subviews().iter().find_map(|v| find(v, depth - 1))
        }
        find(self.contentView()?, 3)
    }

    fn owns_click(&self, view: &NSView, p: NSPoint) -> bool {
        let size = view.frame().size;
        let m = Self::RESIZE_MARGIN;
        let inside = p.x > m && p.x < size.width - m && p.y > m && p.y < size.height - m;
        // The window buttons, while revealed, take their own clicks.
        let buttons_showing = self
            .standardWindowButton(NSWindowButton::CloseButton)
            .is_some_and(|b| !b.isHidden());
        let over_buttons = buttons_showing && p.x < BUTTON_ZONE.0 && p.y > size.height - BUTTON_ZONE.1;
        // The glass tab capsule, a subview of the terminal view, takes its
        // own clicks.
        let over_capsule = view.subviews().iter().any(|sub| {
            let f = sub.frame();
            !sub.isHidden()
                && p.x >= f.origin.x
                && p.x <= f.origin.x + f.size.width
                && p.y >= f.origin.y
                && p.y <= f.origin.y + f.size.height
        });
        // In the standard layout the title bar is AppKit's: dragging,
        // double-clicking, the title.
        let bar = query_window(view_id(view), |w| w.title_bar_height()).unwrap_or(0.0);
        let in_bar = bar > 0.0 && p.y > size.height - bar;
        inside && !over_buttons && !over_capsule && !in_bar
    }
}

impl TachyonView {
    fn new(mtm: MainThreadMarker, frame: NSRect, id: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowId { id: Cell::new(id) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn id(&self) -> u64 {
        self.ivars().id.get()
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonWindowDelegate"]
    #[ivars = WindowId]
    struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            // Retire the session after AppKit's close sequence has unwound.
            let id = self.ivars().id.get();
            DispatchQueue::main().exec_async(move || remove_window(id));
        }

        #[unsafe(method(windowWillEnterFullScreen:))]
        fn window_will_enter_full_screen(&self, _n: &NSNotification) {
            with_window(self.ivars().id.get(), |w| w.renderer.set_full_screen(true));
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn window_did_exit_full_screen(&self, _n: &NSNotification) {
            with_window(self.ivars().id.get(), |w| w.renderer.set_full_screen(false));
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            with_window(self.ivars().id.get(), |w| w.set_focused(true));
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _n: &NSNotification) {
            with_window(self.ivars().id.get(), |w| w.set_focused(false));
        }
    }
);

impl WindowDelegate {
    fn new(mtm: MainThreadMarker, id: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowId { id: Cell::new(id) });
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    /// App-level behaviour: New Window, and what happens with no windows. It is
    /// the last stop in the responder chain, so ⌘N reaches it even when no
    /// window is open.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            // Stay running with no windows, like Terminal.app.
            false
        }

        /// Quitting: Rewind's folders for this run go too (the process exits
        /// without running destructors).
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _n: &NSNotification) {
            // The host keeps the shells; it keeps where they were too.
            save_layout();
            QUITTING.store(true, Ordering::Release);
            if let Some(root) = rewind_root() {
                let prefix = format!("{}-", std::process::id());
                if let Ok(entries) = std::fs::read_dir(&root) {
                    for e in entries.flatten() {
                        if e.file_name().to_string_lossy().starts_with(&prefix) {
                            let _ = std::fs::remove_dir_all(e.path());
                        }
                    }
                }
            }
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, has_visible_windows: bool) -> bool {
            // Clicking the Dock icon with no windows open starts a fresh one.
            if !has_visible_windows {
                open_window();
            }
            true
        }
    }

    impl AppDelegate {
        /// ⌥⌘Q: quit and end every shell, rather than keeping them for next
        /// time.
        #[unsafe(method(tachyonQuitEndingSessions:))]
        fn quit_ending_sessions(&self, _sender: Option<&AnyObject>) {
            with_app(|app| {
                for w in &app.windows {
                    for tab in &w.tabs {
                        for pane in &tab.panes {
                            pane.pty.hangup();
                        }
                    }
                }
            });
            if let Some(mtm) = MainThreadMarker::new() {
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        }

        #[unsafe(method(newWindow:))]
        fn new_window(&self, _sender: Option<&AnyObject>) {
            open_window();
        }

        /// ⌘T with no window open: a window is the first tab.
        #[unsafe(method(newTab:))]
        fn new_tab(&self, _sender: Option<&AnyObject>) {
            open_window();
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

struct App {
    cfg: Config,
    /// The green button and ⌃⌘F go around the notch.
    notch_full_screen: bool,
    /// New windows get a title bar (View > Show Title Bar).
    title_bar: bool,
    gpu: Rc<Gpu>,
    windows: Vec<TermWindow>,
    next_id: u64,
    /// Where the next window's top-left corner goes, so new windows cascade.
    cascade: NSPoint,
    _delegate: Retained<AppDelegate>,
}

/// An earlier screen Rewind is showing.
struct Rewound {
    term: Term,
    /// Which frame, of how many, and when it was drawn.
    seq: u64,
    index: usize,
    frames: usize,
    at: Instant,
    /// Cell provenance from the last Option-click, shown under the slider.
    note: Option<String>,
}

/// The glass panel at the top while rewound: when the screen is from, a
/// slider to scrub through the frames, and cell provenance when asked for.
/// What find found in the focused pane.
struct FindState {
    /// What was searched for, to notice the field changing without a
    /// keystroke (⌘G after editing, say).
    query: String,
    regex: bool,
    matches: Vec<crate::term::find::Match>,
    current: Option<usize>,
    /// The scrollback's length when searched: a different one means the
    /// lines moved (output arrived), so the next step searches again.
    lines: usize,
}

/// The most matches looked for; the count shows "10,000+" past it.
const FIND_LIMIT: usize = 10_000;

impl FindState {
    /// The matches on screen, as (row, first column, end column, current).
    fn on_screen(&self, term: &Term) -> Vec<(usize, usize, usize, bool)> {
        let g = term.grid();
        if g.total_lines() != self.lines {
            return Vec::new();
        }
        let first = g.history_len() - g.display_offset();
        let last = first + term.rows();
        let mut out = Vec::new();
        let from = self.matches.partition_point(|m| m.end.0 < first);
        for (i, m) in self.matches.iter().enumerate().skip(from) {
            if m.start.0 >= last {
                break;
            }
            for line in m.start.0.max(first)..=m.end.0.min(last - 1) {
                let x0 = if line == m.start.0 { m.start.1 } else { 0 };
                let x1 = if line == m.end.0 { m.end.1 } else { term.cols() };
                out.push((line - first, x0, x1, Some(i) == self.current));
            }
        }
        out
    }
}

/// The glass find bar at the top right: a search field, the match count,
/// and a regex switch.
struct FindBar {
    host: Retained<NSView>,
    field: Retained<NSSearchField>,
    count: Retained<objc2_app_kit::NSTextField>,
    regex: Retained<objc2_app_kit::NSButton>,
}

impl FindBar {
    const WIDTH: f64 = 400.0;
    const HEIGHT: f64 = 36.0;

    fn new(mtm: MainThreadMarker, parent: &TachyonView) -> FindBar {
        let content = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let host: Retained<NSView> = if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some() {
            let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), NSRect::ZERO);
            glass.setCornerRadius(Self::HEIGHT / 2.0);
            glass.setTintColor(Some(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.75)));
            glass.setContentView(Some(&content));
            Retained::into_super(glass)
        } else {
            let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), NSRect::ZERO);
            blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
            blur.setState(NSVisualEffectState::Active);
            blur.addSubview(&content);
            Retained::into_super(blur)
        };
        let field = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::ZERO);
        field.setPlaceholderString(Some(&NSString::from_str("Find in scrollback")));
        // SAFETY: the view outlives its own subview's delegate use.
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(parent))) };
        // Every keystroke searches, without waiting for Return.
        field.setSendsSearchStringImmediately(true);
        let count = objc2_app_kit::NSTextField::labelWithString(&NSString::from_str(""), mtm);
        count.setFont(Some(&objc2_app_kit::NSFont::monospacedDigitSystemFontOfSize_weight(11.0, unsafe { objc2_app_kit::NSFontWeightRegular })));
        count.setTextColor(Some(&NSColor::secondaryLabelColor()));
        count.setAlignment(objc2_app_kit::NSTextAlignment::Right);
        let target: &AnyObject = parent.as_ref();
        // A checkbox, so whether the query is a regular expression shows at
        // a glance.
        let regex = unsafe {
            objc2_app_kit::NSButton::checkboxWithTitle_target_action(&NSString::from_str("Regex"), Some(target), Some(sel!(tachyonFindRegex:)), mtm)
        };
        regex.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(12.0)));
        regex.setToolTip(Some(&NSString::from_str("Search with a regular expression, like loss=0\\.[0-4] or error|warn")));
        content.addSubview(&field);
        content.addSubview(&count);
        content.addSubview(&regex);
        host.setHidden(true);
        parent.addSubview(&host);
        FindBar { host, field, count, regex }
    }

    /// Place it at the top right of a view of `size`, below `top` points.
    fn place(&self, size: NSSize, top: f64) {
        let (w, h) = (Self::WIDTH.min(size.width - 24.0), Self::HEIGHT);
        let frame = NSRect::new(NSPoint::new(size.width - w - 12.0, size.height - top - h - 10.0), NSSize::new(w, h));
        self.host.setFrame(frame);
        // SAFETY: a plain view-hierarchy read on the main thread.
        if let Some(content) = unsafe { self.field.superview() } {
            content.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        }
        let (count_w, regex_w) = (80.0, 64.0);
        self.field.setFrame(NSRect::new(NSPoint::new(10.0, 6.0), NSSize::new(w - 10.0 - count_w - regex_w - 18.0, 24.0)));
        self.count.setFrame(NSRect::new(NSPoint::new(w - count_w - regex_w - 12.0, 10.0), NSSize::new(count_w, 16.0)));
        self.regex.setFrame(NSRect::new(NSPoint::new(w - regex_w - 8.0, 8.0), NSSize::new(regex_w, 20.0)));
    }
}

struct RewindBadge {
    glass: Retained<NSGlassEffectView>,
    label: Retained<objc2_app_kit::NSTextField>,
    slider: Retained<objc2_app_kit::NSSlider>,
    note: Retained<objc2_app_kit::NSTextField>,
}

impl RewindBadge {
    const WIDTH: f64 = 620.0;

    fn new(mtm: MainThreadMarker, parent: &TachyonView) -> Option<RewindBadge> {
        objc2::runtime::AnyClass::get(c"NSGlassEffectView")?;
        let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), NSRect::ZERO);
        glass.setCornerRadius(16.0);
        // Tinted, so the text under the panel doesn't show through its words.
        glass.setTintColor(Some(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.75)));
        let content = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let label = objc2_app_kit::NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize_weight(12.0, unsafe {
            objc2_app_kit::NSFontWeightMedium
        })));
        // The view is the slider's target: its tachyonRewindScrub: replays.
        let target: &AnyObject = parent.as_ref();
        let slider = unsafe {
            objc2_app_kit::NSSlider::sliderWithValue_minValue_maxValue_target_action(
                0.0,
                0.0,
                1.0,
                Some(target),
                Some(sel!(tachyonRewindScrub:)),
                mtm,
            )
        };
        let note = objc2_app_kit::NSTextField::labelWithString(&NSString::from_str(""), mtm);
        note.setFont(Some(&objc2_app_kit::NSFont::monospacedSystemFontOfSize_weight(11.0, unsafe {
            objc2_app_kit::NSFontWeightRegular
        })));
        note.setTextColor(Some(&NSColor::labelColor()));
        note.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
        content.addSubview(&label);
        content.addSubview(&slider);
        content.addSubview(&note);
        glass.setContentView(Some(&content));
        glass.setHidden(true);
        parent.addSubview(&glass);
        Some(RewindBadge { glass, label, slider, note })
    }

    /// Show the panel near the top of a view of `size`: `text` beside a
    /// slider at frame `index` of `frames`, and `note` below if any.
    fn show(&self, text: &str, index: usize, frames: usize, note: Option<&str>, size: NSSize) {
        self.label.setStringValue(&NSString::from_str(text));
        self.slider.setMaxValue(frames.saturating_sub(1) as f64);
        self.slider.setDoubleValue(index as f64);
        let w = Self::WIDTH.min(size.width - 24.0);
        let h = if note.is_some() { 52.0 } else { 32.0 };
        let frame = NSRect::new(NSPoint::new(((size.width - w) / 2.0).round(), size.height - h - 10.0), NSSize::new(w, h));
        self.glass.setFrame(frame);
        let top = h - 32.0;
        let label_w = (self.label.intrinsicContentSize().width + 4.0).min(w * 0.5);
        self.label.setFrame(NSRect::new(NSPoint::new(16.0, top + 8.0), NSSize::new(label_w, 16.0)));
        let sx = 16.0 + label_w + 12.0;
        self.slider.setFrame(NSRect::new(NSPoint::new(sx, top + 6.0), NSSize::new((w - sx - 16.0).max(40.0), 20.0)));
        self.note.setHidden(note.is_none());
        if let Some(n) = note {
            self.note.setStringValue(&NSString::from_str(n));
            self.note.setFrame(NSRect::new(NSPoint::new(16.0, 8.0), NSSize::new(w - 32.0, 15.0)));
        }
        self.glass.setHidden(false);
    }
}

/// One shell session: a pane of a tab. Its id is the session's, which the
/// reader thread, notifications and the Dock know it by.
struct Pane {
    id: u64,
    term: Arc<Mutex<Term>>,
    pty: Arc<Pty>,
    /// Set by the reader thread when it has asked the main thread to wake;
    /// cleared when the main thread handles it. Keeps the main queue from
    /// filling with redundant wake-ups during a flood of output.
    wake_pending: Arc<AtomicBool>,
    /// The window's per-pane state, kept here while another pane has focus.
    saved: PaneState,
    /// The cluster job this pane follows, if Tachyon opened it for one.
    job: Option<JobRef>,
    /// Its recent output, for Rewind.
    log: Arc<Mutex<RewindLog>>,
    /// Where it is in the window's grid.
    rect: panes::Rect,
    /// The keystroke in flight, for the latency ledger.
    ledger: Arc<ledger::Slot>,
}

/// A tab: one pane, or several split from it.
struct Tab {
    panes: Vec<Pane>,
    /// The pane with focus, which keys go to.
    focus: usize,
    layout: panes::Node,
    /// The focused pane fills the tab for now; the others keep running.
    zoomed: bool,
    dividers: Vec<panes::Divider>,
    /// What the tab is called: the focused pane's running command, folder,
    /// or a title its program set. Refreshed when `Term::title_revision`
    /// moves.
    label: String,
    title_rev: u64,
    /// A command finished here while the tab was in the background.
    unseen: Option<Outcome>,
}

/// Per-pane state the window works on directly while the pane has focus.
struct PaneState {
    selection: Option<Selection>,
    preedit: String,
    mouse_held: Option<MouseButton>,
    mouse_cell: (usize, usize),
    scroll_accum: f64,
    sync_since: Option<Instant>,
}

impl Default for PaneState {
    fn default() -> Self {
        PaneState {
            selection: None,
            preedit: String::new(),
            mouse_held: None,
            mouse_cell: (usize::MAX, usize::MAX),
            scroll_accum: 0.0,
            sync_since: None,
        }
    }
}

impl Pane {
    /// Start a shell, in `cwd` when given (a split starts where the pane it
    /// came from is), otherwise at home.
    fn start(cfg: &Config, cols: usize, rows: usize, job: Option<JobRef>, cwd: Option<&str>) -> Result<Pane, String> {
        let run = job.as_ref().and_then(follow_command);
        let (id, pty) = open_session(cfg, cols, rows, run.as_deref(), cwd)?;
        Pane::with(id, pty, cfg, cols, rows, job, false)
    }

    /// A shell the session host kept while the app was closed.
    fn attach(cfg: &Config, r: host::Remote) -> Result<Pane, String> {
        let awake = r.awake;
        let pane = Pane::with(r.id, r.pty, cfg, r.cols, r.rows, None, awake)?;
        Ok(pane)
    }

    fn with(id: u64, pty: Pty, cfg: &Config, cols: usize, rows: usize, job: Option<JobRef>, awake: bool) -> Result<Pane, String> {
        let wake_pending = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(ledger::Slot::default());
        let (term, pty, log) = start_session(id, pty, cfg, cols, rows, Arc::clone(&wake_pending), awake, Arc::clone(&slot))?;
        Ok(Pane {
            id,
            term,
            pty,
            wake_pending,
            saved: PaneState::default(),
            job,
            log,
            rect: panes::Rect { col: 0, row: 0, cols, rows },
            ledger: slot,
        })
    }
}

impl Tab {
    fn new(pane: Pane) -> Tab {
        Tab {
            layout: panes::Node::Leaf(pane.id),
            panes: vec![pane],
            focus: 0,
            zoomed: false,
            dividers: Vec::new(),
            label: String::new(),
            title_rev: u64::MAX,
            unseen: None,
        }
    }

    /// The pane with focus.
    fn pane(&self) -> &Pane {
        &self.panes[self.focus]
    }

    fn index_of(&self, pane: u64) -> Option<usize> {
        self.panes.iter().position(|p| p.id == pane)
    }

    /// The panes on screen and where: all of them, or the focused one alone
    /// when zoomed.
    fn visible(&self) -> Vec<usize> {
        if self.zoomed { vec![self.focus] } else { (0..self.panes.len()).collect() }
    }

    /// Lay the panes out in a grid of `cols` by `rows`, resizing each whose
    /// size changed. `cell` is a cell's size in device pixels.
    fn lay_out(&mut self, cols: usize, rows: usize, cell: (u32, u32)) {
        let area = panes::Rect { col: 0, row: 0, cols, rows };
        let (rects, dividers) = if self.zoomed {
            (vec![(self.pane().id, area)], Vec::new())
        } else {
            self.layout.layout(area)
        };
        self.dividers = dividers;
        for (id, rect) in rects {
            let Some(p) = self.panes.iter_mut().find(|p| p.id == id) else {
                continue;
            };
            let rect = panes::Rect { cols: rect.cols.max(1), rows: rect.rows.max(1), ..rect };
            let resized = (rect.cols, rect.rows) != (p.rect.cols, p.rect.rows);
            let cell_changed = {
                let mut t = p.term.lock();
                if resized {
                    t.resize(rect.cols, rect.rows);
                }
                let before = t.cell_pixels();
                t.set_cell_pixels(cell.0, cell.1);
                before != t.cell_pixels()
            };
            if resized {
                p.log.lock().record_resize(rect.cols, rect.rows);
            }
            if resized || cell_changed {
                p.pty.resize(rect.cols as u16, rect.rows as u16, cell);
            }
            p.rect = rect;
        }
    }
}

/// A cell's size in whole device pixels, as terminals report it.
fn cell_pixels(r: &renderer::Renderer) -> (u32, u32) {
    let m = r.metrics();
    (m.width.round().max(1.0) as u32, m.height.round().max(1.0) as u32)
}

impl Drop for Pane {
    fn drop(&mut self) {
        dock::set_progress(self.id, None);
        if !self.pty.is_remote() {
            power::set(self.id, None);
        }
        // Quitting leaves the host's shells running, to come back next time.
        if QUITTING.load(Ordering::Acquire) && self.pty.is_remote() {
            return;
        }
        // The reader thread holds the PTY too, so dropping ours does not end
        // the session; hanging up does.
        self.pty.hangup();
    }
}

/// One window: its AppKit objects, renderer, and tabs.
/// The training chart showing over a pane.
struct ChartView {
    pane: u64,
    /// The command it shows; `None` follows the newest with metrics.
    cmd: Option<u32>,
    /// What its picture was last made from.
    made: Option<ChartKey>,
}

/// What a chart's picture depends on: it's made again only when one changes.
#[derive(Clone, Copy, PartialEq)]
struct ChartKey {
    /// The command and its curves' revision.
    shows: Option<(u32, u64)>,
    size: (usize, usize),
    cell: (u32, u32),
    colors: chart::Colors,
}

struct TermWindow {
    id: u64,
    window: Retained<NSWindow>,
    view: Retained<TachyonView>,
    link: Retained<CADisplayLink>,
    renderer: Renderer,

    tabs: Vec<Tab>,
    active: usize,
    /// The tab in front's terminal and PTY, the same as `tabs[active]`'s.
    term: Arc<Mutex<Term>>,
    pty: Arc<Pty>,
    /// The window title last set, so it changes only when the label does.
    title_shown: String,

    needs_redraw: bool,
    idle_ticks: u32,
    sync_since: Option<Instant>,
    sync_timeout: Duration,
    grid: (usize, usize),
    scroll_accum: f64,
    buttons_visible: bool,
    focused: bool,
    /// The standard layout: a visible title bar, the grid below it.
    title_bar: bool,

    selection: Option<Selection>,
    /// Text an input method is composing, shown at the cursor until committed.
    preedit: String,
    /// A button an application is tracking: it is owed motion and a release.
    mouse_held: Option<MouseButton>,
    /// Last cell reported to the application, so motion is sent per cell.
    mouse_cell: (usize, usize),
    option_as_meta: OptionAsMeta,
    base_font_size: f32,
    /// How long a command must run to notify when it finishes unseen;
    /// `None` when notifications are off.
    notify_after: Option<Duration>,
    /// Secure Keyboard Entry is on for this window: it's in front and its
    /// tab is asking for a password.
    secure: bool,
    /// The two themes and which to use; `apply_appearance` picks one.
    theme_dark: ThemeConfig,
    theme_light: ThemeConfig,
    appearance: Appearance,
    /// Use the high-contrast themes: always, never, or with Increase
    /// Contrast.
    contrast: Contrast,
    /// Glass is behind the text, so the window itself is clear.
    glass: bool,
    /// With Liquid Glass, the tab strip is a floating glass capsule, created
    /// when the second tab opens.
    native_strip: bool,
    capsule: Option<strip::Capsule>,
    /// Full screen around the notch: the frame and style to go back to.
    notch: Option<(NSRect, NSWindowStyleMask)>,
    /// The status right of the notch.
    notch_status: Option<Retained<objc2_app_kit::NSTextField>>,
    /// Keeps the status's clock current while nothing else redraws.
    notch_timer: Option<Retained<objc2_foundation::NSTimer>>,
    /// The Liquid Glass behind the text, tinted with the theme's background.
    host_glass: Option<Retained<NSGlassEffectView>>,
    /// Jobs a tab was opened for, so their commands stop offering one.
    followed: Vec<String>,
    /// The rerun-diff tooltip showing, if any.
    tooltip: Option<String>,
    /// Rewind: the earlier screen being shown instead of the live one.
    rewound: Option<Rewound>,
    rewind_badge: Option<RewindBadge>,
    glass_tint: f32,
    /// A divider being dragged to resize the panes beside it.
    dragging_divider: Option<panes::Divider>,
    /// When the key being handled was pressed.
    key_time: Option<u64>,
    /// Frames drawn in a row without the link going idle.
    busy_frames: u32,
    /// Find in scrollback: the bar, and what it found.
    find_bar: Option<FindBar>,
    find: Option<FindState>,
    /// The suggestion showing after the cursor (all of it), for → to take.
    ghost: Option<String>,
    /// The last suggestion looked up: (typed, folder) and its answer.
    ghost_cache: Option<(String, Option<String>, Option<String>)>,
    /// The training chart, over one pane.
    chart: Option<ChartView>,

    /// Draws the focused, blinking cursor. Core Animation runs the blink in
    /// the window server, so a blinking cursor costs the app no wake-ups.
    cursor_layer: Retained<CALayer>,
    cursor_shown: Option<CursorOverlay>,
    _delegate: Retained<WindowDelegate>,
}

impl Drop for TermWindow {
    fn drop(&mut self) {
        // A display link retains its target; invalidate it or the view leaks.
        // Dropping the tabs hangs up their shells.
        self.link.invalidate();
        if self.secure {
            unsafe { DisableSecureEventInput() };
        }
    }
}

impl TermWindow {
    fn scale(&self) -> f64 {
        self.window.backingScaleFactor()
    }

    /// Wake from idle. Called on the main queue by the reader thread.
    fn set_frame_rate(&self, fps: f32) {
        let min = fps.min(60.0);
        self.link.setPreferredFrameRateRange(objc2_quartz_core::CAFrameRateRange { minimum: min, maximum: fps, preferred: fps });
    }

    fn resume(&mut self) {
        self.idle_ticks = 0;
        let was_idle = self.link.isPaused();
        self.link.setPaused(false);
        // Idle until now (typing at a prompt, say): draw at once rather than
        // waiting up to a refresh for the link's next tick. A window busy
        // with output keeps drawing on the link.
        if was_idle {
            self.tick();
        }
    }

    fn tick(&mut self) {
        let tab = &self.tabs[self.active];
        let mut dirty = false;
        for &p in &tab.visible() {
            let pane = &tab.panes[p];
            pane.wake_pending.store(false, Ordering::Release);
            dirty |= pane.term.lock().take_dirty();
        }
        let (syncing, title_rev) = {
            let t = self.term.lock();
            (t.mode.contains(TermMode::SYNC_UPDATE), t.title_revision)
        };
        if dirty {
            self.needs_redraw = true;
        }
        if title_rev != self.tabs[self.active].title_rev {
            self.refresh_label(self.active);
        }

        // Synchronised output (mode 2026): hold the frame while an app is
        // mid-update, up to a timeout so a crashed app cannot freeze us.
        if syncing && self.needs_redraw {
            let since = *self.sync_since.get_or_insert_with(Instant::now);
            if since.elapsed() < self.sync_timeout {
                return;
            }
        } else {
            self.sync_since = None;
        }

        if self.needs_redraw {
            self.needs_redraw = false;
            self.idle_ticks = 0;
            // Output streaming for a quarter of a second (a build, a cat):
            // 60 frames a second is plenty to watch it, and leaves the
            // terminal lock to the reader for twice as long.
            self.busy_frames += 1;
            if self.busy_frames == 30 {
                self.set_frame_rate(60.0);
            }
            self.update_secure_input();
            self.draw();
            self.update_notch_status();
        } else {
            // Nothing changed for a few refreshes: stop the display link. The
            // next byte from the shell or key press resumes it.
            self.idle_ticks += 1;
            if self.idle_ticks >= 3 {
                self.link.setPaused(true);
                // Next time it wakes it's for a keystroke, most likely: as
                // fast as the display goes.
                if self.busy_frames >= 30 {
                    self.set_frame_rate(120.0);
                }
                self.busy_frames = 0;
            }
        }
    }

    /// Draw the tab in front: each pane on screen, the dividers between
    /// them, and the tab strip.
    fn draw(&mut self) {
        let mut strip = None;
        if self.tabs.len() > 1 {
            let tabs: Vec<_> = self.tabs.iter().map(|t| (t.label.clone(), t.unseen)).collect();
            match &self.capsule {
                Some(c) if self.native_strip => c.view.set_tabs(tabs, self.active),
                _ => strip = Some(Strip { tabs, active: self.active }),
            }
        }
        let tab = &self.tabs[self.active];
        let visible = tab.visible();
        // Every pane's terminal, locked for the frame: the focused one through
        // the window's handle, the rest through their own.
        let guards: Vec<_> = visible.iter().map(|&p| tab.panes[p].term.lock()).collect();
        let focus_at = visible.iter().position(|&p| p == tab.focus);
        let mut decors = Vec::with_capacity(visible.len());
        let mut selections = Vec::with_capacity(visible.len());
        for (k, &p) in visible.iter().enumerate() {
            let term = &guards[k];
            let mut decor = command_decor(term, &self.followed);
            let selection = if p == tab.focus {
                decor.preedit = self.preedit.clone();
                if self.secure {
                    decor.notes.push((term.cursor_state().y, "Secure Keyboard Entry".into()));
                }
                if ledger::SHOWN.load(Ordering::Relaxed) {
                    if let Some(line) = ledger::summary() {
                        decor.notes.push((term.cursor_state().y, line));
                    }
                }
                if let (Some(f), None) = (&self.find, &self.rewound) {
                    decor.highlights = f.on_screen(term);
                }
                if self.rewound.is_none() && self.preedit.is_empty() {
                    decor.ghost = ghost_for(term, &mut self.ghost_cache, &mut self.ghost);
                } else {
                    self.ghost = None;
                }
                self.selection
            } else {
                tab.panes[p].saved.selection
            };
            selections.push(selection.and_then(|s| s.range(term)));
            decors.push(decor);
        }
        let blink = self.focused && focus_at.is_some_and(|k| guards[k].cursor_state().blinking) && self.rewound.is_none();
        let empty = Decor::default();
        let frames: Vec<PaneFrame> = visible
            .iter()
            .enumerate()
            .map(|(k, &p)| {
                let rect = tab.panes[p].rect;
                let focused = p == tab.focus;
                match (&self.rewound, focused) {
                    // An earlier screen: no selection, marks or cursor layer,
                    // since they belong to the live one.
                    (Some(rw), true) => PaneFrame {
                        term: &rw.term,
                        col: rect.col,
                        row: rect.row,
                        focused: self.focused,
                        selection: None,
                        cursor_on_layer: false,
                        decor: &empty,
                        dim: false,
                    },
                    _ => PaneFrame {
                        term: &guards[k],
                        col: rect.col,
                        row: rect.row,
                        focused: self.focused && focused,
                        selection: selections[k],
                        cursor_on_layer: focused && blink,
                        decor: &decors[k],
                        dim: self.focused && !focused,
                    },
                }
            })
            .collect();
        let dividers: Vec<[f32; 4]> = tab.dividers.iter().map(|d| self.divider_pixels(d)).collect();
        // The training chart, over its pane when that's on screen.
        let mut panel_pixels = None;
        let mut panel_labels = Vec::new();
        let mut panel_place = None;
        if let Some(view) = &mut self.chart {
            if let Some(k) = visible.iter().position(|&p| tab.panes[p].id == view.pane) {
                let term = &guards[k];
                let rect = tab.panes[visible[k]].rect;
                let chart = view.cmd.or_else(|| term.latest_with_metrics()).and_then(|c| chart::chart_for(term, c));
                let (col, row, cols, rows) = chart::panel_rect(rect.cols, rect.rows);
                let (bg, fg) = self.renderer.theme_colors();
                let colors = chart::Colors { bg, fg };
                let m = self.renderer.metrics();
                let key = ChartKey {
                    shows: chart.as_ref().map(|c| (c.cmd, c.revision)),
                    size: (cols, rows),
                    cell: (m.width.to_bits(), m.height.to_bits()),
                    colors,
                };
                if view.made != Some(key) {
                    let scale = self.window.backingScaleFactor() as f32;
                    panel_pixels = chart::pixels(chart.as_ref(), cols, rows, (m.width, m.height), scale, colors);
                    view.made = Some(key);
                }
                panel_labels = chart::labels(chart.as_ref(), cols, rows, colors);
                panel_place = Some((rect.col + col, rect.row + row, cols, rows));
            }
        }
        let panel = panel_place.map(|(col, row, cols, rows)| renderer::Panel {
            col,
            row,
            pixels: panel_pixels.as_ref(),
            labels: &panel_labels,
            cols,
            rows,
        });
        // The echo of a keystroke is in this frame: the ledger finishes when
        // it reaches the display.
        let presented: Option<Box<dyn Fn(f64)>> = tab.pane().ledger.take_for_frame().map(|p| {
            let window = self.id;
            Box::new(move |at: f64| {
                if p.finish(ledger::from_seconds(at)).is_some() && ledger::SHOWN.load(Ordering::Relaxed) {
                    DispatchQueue::main().exec_async(move || with_window(window, |w| w.request_redraw()));
                }
            }) as Box<dyn Fn(f64)>
        });
        self.renderer.draw_frame(&frames, strip.as_ref(), &dividers, panel.as_ref(), presented);
        let overlay = match focus_at {
            Some(k) if blink => {
                let rect = tab.panes[tab.focus].rect;
                self.renderer.cursor_overlay(&guards[k], rect.col, rect.row)
            }
            _ => None,
        };
        drop(frames);
        drop(guards);
        self.update_cursor_layer(overlay);
    }

    /// A divider as a hairline down (or across) the middle of its cells, in
    /// drawable pixels.
    fn divider_pixels(&self, d: &panes::Divider) -> [f32; 4] {
        let m = self.renderer.metrics();
        let (x, y) = self.renderer.pane_origin(d.rect.col, d.rect.row);
        let hair = (self.scale() as f32).round().max(1.0);
        match d.dir {
            panes::Dir::Right => [(x + (m.width - hair) / 2.0).round(), y, hair, d.rect.rows as f32 * m.height],
            panes::Dir::Down => [x, (y + (m.height - hair) / 2.0).round(), d.rect.cols as f32 * m.width, hair],
        }
    }

    /// A command finished in tab `i`. A background tab gets a mark in the
    /// tab strip; if it ran a while and the window is not in front, the Dock
    /// icon bounces once.
    fn command_finished(&mut self, i: usize, pane: u64, cmd: u32) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(p) = self.tabs[i].index_of(pane) else {
            return;
        };
        let finished = {
            let term = self.tabs[i].panes[p].term.lock();
            term.command(cmd).map(|info| {
                let elapsed = info.elapsed().unwrap_or_default();
                let failed = info.exit.is_some_and(|e| e != 0);
                let receipt = receipts::describe(elapsed, info.exit, false, info.receipt.as_ref()).unwrap_or_default();
                let title = info.cmdline.clone().unwrap_or_else(|| "Command finished".into());
                let folder = term.cwd.as_deref().map(short_path).unwrap_or_default();
                (elapsed, failed, title, folder, receipt)
            })
        };
        let Some((elapsed, failed, mut title, folder, receipt)) = finished else {
            return;
        };
        // Remember it for suggestions, with where it ran.
        {
            let term = self.tabs[i].panes[p].term.lock();
            if let Some(cmd) = term.command(cmd).and_then(|c| c.cmdline.clone()) {
                let cwd = term.cwd.clone();
                SUGGESTIONS.with(|s| {
                    if let Some(h) = s.borrow_mut().as_mut() {
                        h.record(&cmd, cwd.as_deref());
                    }
                });
            }
        }
        if let Some(job) = &self.tabs[i].panes[p].job {
            title = format!("{} {}", job.title(), if failed { "failed" } else { "finished" });
        }
        if i != self.active {
            self.tabs[i].unseen = Some(if failed { Outcome::Failed } else { Outcome::Succeeded });
        }
        self.request_redraw();

        // Only when you weren't looking at this window.
        let app = NSApplication::sharedApplication(mtm);
        if app.isActive() && self.window.isKeyWindow() {
            return;
        }
        let threshold = self.notify_after.unwrap_or(Duration::from_secs(10));
        if elapsed < threshold {
            return;
        }
        let tab = pane;
        let posted = self.notify_after.is_some()
            && NOTIFIER.with(|n| n.borrow().as_ref().map(|n| n.post(tab, cmd, &title, &folder, &receipt)).is_some());
        if !posted {
            app.requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
        }
    }

    /// Use the light or dark theme to match the system (or the setting), with
    /// the system accent colour where the theme asks for it.
    fn apply_appearance(&mut self) {
        let dark = match self.appearance {
            Appearance::Dark => true,
            Appearance::Light => false,
            Appearance::Auto => appearance_is_dark(&self.view),
        };
        let high = match self.contrast {
            Contrast::High => true,
            Contrast::Normal => false,
            Contrast::Auto => NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast(),
        };
        let mut theme = if dark { self.theme_dark.clone() } else { self.theme_light.clone() };
        if high {
            theme = theme.high_contrast(dark);
        }
        if theme.cursor_accent || theme.selection_accent {
            if let Some(accent) = accent_color() {
                if theme.cursor_accent {
                    theme.cursor = accent;
                }
                if theme.selection_accent {
                    theme.selection_bg = mix_rgb(theme.background, accent, 0.4);
                }
            }
        }
        if !self.glass {
            self.window.setBackgroundColor(Some(&srgb_color(theme.background, 1.0)));
        }
        if let Some(glass) = &self.host_glass {
            // Increase Contrast: the glass keeps its shape and edge, but the
            // text sits on a solid background, as macOS does with its own
            // materials.
            let alpha = if high { 1.0 } else { self.glass_tint as f64 };
            let tint = (alpha > 0.0).then(|| srgb_color(theme.background, alpha));
            glass.setTintColor(tint.as_deref());
        }
        self.renderer.set_theme(&theme);
        self.cursor_shown = None;
        self.request_redraw();
    }

    /// Turn Secure Keyboard Entry on while this window is in front and its
    /// tab is reading a password, and off again after.
    fn update_secure_input(&mut self) {
        let want = self.focused && asking_for_password(self.pty.master_fd());
        if want == self.secure {
            return;
        }
        self.secure = want;
        unsafe {
            if want {
                EnableSecureEventInput();
            } else {
                DisableSecureEventInput();
            }
        }
        crate::log!("window {}: secure keyboard entry {}", self.id, if want { "on" } else { "off" });
        self.needs_redraw = true;
    }

    /// ⌘-click: open a URL in the browser, or show a file in Quick Look.
    /// Returns false when there's nothing there to open.
    fn open_under_pointer(&mut self, event: &NSEvent) -> bool {
        let (pos, _) = self.cell_at(event);
        let (word, cwd) = {
            let term = self.term.lock();
            (selection::word_at(&term, pos.line, pos.cell), term.cwd.clone())
        };
        // A job id this tab submitted: follow it.
        let id = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        let job = self.term.lock().jobs.iter().rev().find(|j| j.id == id).cloned();
        if let Some(job) = job {
            let window = self.id;
            DispatchQueue::main().exec_async(move || follow_job(window, Some(job)));
            return true;
        }
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let target = links::resolve(&word, cwd.as_deref().map(std::path::Path::new), home.as_deref(), |p| p.exists());
        match target {
            Some(links::Target::Url(u)) => {
                if let Some(url) = NSURL::URLWithString(&NSString::from_str(&u)) {
                    NSWorkspace::sharedWorkspace().openURL(&url);
                }
                true
            }
            Some(links::Target::Path(path)) => {
                // After this event, outside the app-state borrow: the panel
                // taking key status calls back into the window delegate.
                DispatchQueue::main().exec_async(move || quick_look(path));
                true
            }
            None => false,
        }
    }

    /// ⌘↑ / ⌘↓: scroll so the previous or next command's prompt is at the top.
    fn jump_command(&mut self, up: bool) {
        {
            let mut t = self.term.lock();
            let g = t.grid();
            let hist = g.history_len();
            let first = hist - g.display_offset();
            // The start of a command's prompt, which may be a line or two
            // above where typing started.
            let prompt_of = |l: usize| {
                let cmd = g.mark(l).cmd;
                (l.saturating_sub(4)..=l)
                    .find(|&p| g.mark(p).cmd == cmd && g.mark(p).has(RowMark::PROMPT))
                    .unwrap_or(l)
            };
            let is_input = |l: &usize| g.mark(*l).has(RowMark::INPUT);
            let target = if up {
                (0..first).rev().filter(is_input).map(prompt_of).find(|&p| p < first)
            } else {
                // Skip the command already at the top: its input line is just
                // below its own prompt.
                (first + 1..g.total_lines()).filter(is_input).map(prompt_of).find(|&p| p > first)
            };
            let offset = match target {
                Some(l) => hist.saturating_sub(l),
                None if !up => 0,
                None => return,
            };
            let delta = offset as isize - g.display_offset() as isize;
            t.scroll_display(delta);
        }
        self.request_redraw();
    }

    fn request_redraw(&mut self) {
        self.needs_redraw = true;
        self.resume();
    }

    /// Output arrived in a tab that is not in front. Its screen is drawn when
    /// it comes forward; only its name in the strip may need redrawing now.
    fn background_output(&mut self, i: usize) {
        for pane in &self.tabs[i].panes {
            pane.wake_pending.store(false, Ordering::Release);
        }
        let rev = self.tabs[i].pane().term.lock().title_revision;
        if rev != self.tabs[i].title_rev {
            self.refresh_label(i);
        }
    }

    /// Rename tab `i` from its terminal, and the window if it is in front.
    fn refresh_label(&mut self, i: usize) {
        let (label, rev) = {
            let pane = self.tabs[i].pane();
            let term = pane.term.lock();
            let label = match &pane.job {
                Some(job) => job_label(&term, job),
                None => tab_label(&term),
            };
            (label, term.title_revision)
        };
        let tab = &mut self.tabs[i];
        tab.title_rev = rev;
        if tab.label != label {
            tab.label = label;
            if self.tabs.len() > 1 {
                self.request_redraw();
            }
        }
        if i == self.active && self.tabs[i].label != self.title_shown {
            self.title_shown = self.tabs[i].label.clone();
            // Hidden in the title bar, but it names the window in the Window
            // menu, Mission Control and the ⌘` cycle.
            self.window.setTitle(&NSString::from_str(&self.title_shown));
        }
    }

    /// Bring tab `i` to the front.
    fn switch_to(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.active {
            return;
        }
        self.save_live();
        self.active = i;
        self.load_live();
        self.window.invalidateCursorRectsForView(&self.view);
        schedule_layout_save();
    }

    /// ⌘1 to ⌘8 pick that tab; ⌘9 picks the last.
    fn select_tab_number(&mut self, n: usize) {
        let i = if n == 9 { self.tabs.len() - 1 } else { n - 1 };
        self.switch_to(i);
    }

    /// The next (`1`) or previous (`-1`) tab, wrapping around.
    fn cycle_tab(&mut self, step: isize) {
        let n = self.tabs.len() as isize;
        if n > 1 {
            self.switch_to((self.active as isize + step).rem_euclid(n) as usize);
        }
    }

    /// Insert a started tab to the right of the one in front, and show it.
    fn add_tab(&mut self, tab: Tab) {
        self.tabs.insert(self.active + 1, tab);
        self.switch_to(self.active + 1);
        // The strip appears with the second tab, taking a row from the grid.
        self.layout();
    }

    /// Close tab `i`, which must not be the last one. The tab to its right
    /// comes forward, as in Safari, or the one to its left at the end.
    fn remove_tab(&mut self, i: usize) {
        if i == self.active {
            // Its live state goes with it.
            self.tabs.remove(i);
            self.active = i.min(self.tabs.len() - 1);
            self.load_live();
        } else {
            self.tabs.remove(i);
            if i < self.active {
                self.active -= 1;
            }
        }
        self.layout();
    }

    /// Give focus to pane `p` of the tab in front.
    fn focus_pane(&mut self, p: usize) {
        let tab = &self.tabs[self.active];
        if p >= tab.panes.len() || p == tab.focus {
            return;
        }
        self.save_live();
        let tab = &mut self.tabs[self.active];
        tab.focus = p;
        // Another pane can't have focus behind a zoomed one.
        if std::mem::take(&mut tab.zoomed) {
            self.layout();
        }
        self.load_live();
        schedule_layout_save();
    }

    /// ⌘⌥ and an arrow: focus the pane on that side.
    fn move_focus(&mut self, dx: i32, dy: i32) {
        let tab = &self.tabs[self.active];
        let area = panes::Rect { col: 0, row: 0, cols: self.grid.0, rows: self.grid.1 };
        let (rects, _) = tab.layout.layout(area);
        let from = rects.iter().find(|r| r.0 == tab.pane().id).map(|r| r.1).unwrap_or(area);
        match panes::neighbor(&rects, from, dx, dy).and_then(|id| tab.index_of(id)) {
            Some(p) => self.focus_pane(p),
            None => objc2_app_kit::NSBeep(),
        }
    }

    /// ⌥⌘A: keep the Mac awake while the focused pane's command runs (or
    /// its next one, at a prompt), or stop.
    fn toggle_awake(&mut self) {
        let pane = self.tabs[self.active].pane();
        let (on, cmdline) = {
            let mut t = pane.term.lock();
            let on = !t.awake_armed();
            t.set_keep_awake(on);
            (on, t.keeps_awake().then(|| t.running_cmdline().unwrap_or("").to_string()))
        };
        if pane.pty.is_remote() {
            let id = pane.id;
            let sent = HOST.with(|h| h.borrow().as_ref().map(|c| c.set_awake(id, on)));
            if let Some(Err(e)) = sent {
                crate::log::force(&format!("session host: could not set keep-awake: {e}"));
            }
        } else {
            power::set(pane.id, cmdline.as_deref());
        }
        self.request_redraw();
    }

    /// ⌘⇧↩: the focused pane fills the tab, or goes back to its place.
    fn toggle_zoom(&mut self) {
        let tab = &mut self.tabs[self.active];
        if tab.panes.len() < 2 {
            objc2_app_kit::NSBeep();
            return;
        }
        tab.zoomed = !tab.zoomed;
        self.rewind_exit();
        self.layout();
    }

    /// Close pane `p` of tab `i`: the rest of its split takes its place and,
    /// if it had focus, the focus. Its last pane closes the tab; the last
    /// tab's last pane returns the window, for the caller to close outside
    /// the app-state borrow.
    fn close_pane(&mut self, i: usize, p: usize) -> Option<Retained<NSWindow>> {
        if self.tabs[i].panes.len() == 1 {
            if self.tabs.len() == 1 {
                return Some(self.window.clone());
            }
            self.remove_tab(i);
            return None;
        }
        let in_front = i == self.active && p == self.tabs[i].focus;
        if in_front {
            self.rewind_exit();
        }
        let tab = &mut self.tabs[i];
        let next = tab.layout.remove(tab.panes[p].id);
        tab.zoomed = false;
        tab.panes.remove(p);
        if p == tab.focus {
            tab.focus = next.and_then(|id| tab.index_of(id)).unwrap_or(0);
            if i == self.active {
                // Its live state goes with it.
                self.load_live();
            }
        } else if p < tab.focus {
            tab.focus -= 1;
        }
        self.layout();
        None
    }

    /// Where a mouse event is in the window's grid, in fractional cells.
    fn grid_point(&self, event: &NSEvent) -> (f32, f32) {
        let p = self.view.convertPoint_fromView(event.locationInWindow(), None);
        let scale = self.scale() as f32;
        let height = self.view.bounds().size.height as f32;
        let (x, y) = (p.x as f32 * scale, (height - p.y as f32) * scale);
        let m = self.renderer.metrics();
        let (pad_x, pad_y) = self.renderer.padding();
        ((x - pad_x) / m.width, (y - pad_y) / m.height)
    }

    /// The pane on screen under a mouse event, if any.
    fn pane_at(&self, event: &NSEvent) -> Option<usize> {
        let (fx, fy) = self.grid_point(event);
        if fx < 0.0 || fy < 0.0 {
            return None;
        }
        let tab = &self.tabs[self.active];
        tab.visible().into_iter().find(|&p| tab.panes[p].rect.contains(fx as usize, fy as usize))
    }

    /// Open the training chart over the focused pane, for `cmd` or (with
    /// `None`) the newest command with metrics; or close it.
    fn toggle_chart(&mut self, cmd: Option<u32>) {
        self.chart = match self.chart {
            Some(_) => None,
            None => Some(ChartView { pane: self.tabs[self.active].pane().id, cmd, made: None }),
        };
        self.request_redraw();
    }

    /// Whether a mouse event is on the training chart.
    fn chart_at(&self, event: &NSEvent) -> bool {
        let Some(view) = &self.chart else { return false };
        let tab = &self.tabs[self.active];
        let Some(pane) = tab.visible().into_iter().map(|p| &tab.panes[p]).find(|p| p.id == view.pane) else {
            return false;
        };
        let (fx, fy) = self.grid_point(event);
        let (col, row, cols, rows) = chart::panel_rect(pane.rect.cols, pane.rect.rows);
        let (x, y) = (fx - (pane.rect.col + col) as f32, fy - (pane.rect.row + row) as f32);
        x >= 0.0 && y >= 0.0 && x < cols as f32 && y < rows as f32
    }

    /// The pane and command whose metrics readout is under a mouse event.
    fn metrics_at(&self, event: &NSEvent) -> Option<(u64, u32)> {
        let p = self.pane_at(event)?;
        let pane = &self.tabs[self.active].panes[p];
        let (fx, fy) = self.grid_point(event);
        let (col, row) = ((fx - pane.rect.col as f32) as usize, (fy - pane.rect.row as f32) as usize);
        let term = pane.term.lock();
        let decor = command_decor(&term, &self.followed);
        let &(_, cmd, _, _) = decor.metrics.iter().find(|&&(r, _, s, e)| r == row && col >= s && col < e)?;
        Some((pane.id, cmd))
    }

    /// The divider under a mouse event, if any.
    fn divider_at(&self, event: &NSEvent) -> Option<panes::Divider> {
        let (fx, fy) = self.grid_point(event);
        if fx < 0.0 || fy < 0.0 {
            return None;
        }
        self.tabs[self.active].dividers.iter().find(|d| d.rect.contains(fx as usize, fy as usize)).cloned()
    }

    /// Drag a divider to the cell under the pointer.
    fn drag_divider(&mut self, event: &NSEvent) {
        let Some(d) = &self.dragging_divider else {
            return;
        };
        let (fx, fy) = self.grid_point(event);
        let ratio = panes::ratio_at(d, fx.max(0.0) as usize, fy.max(0.0) as usize);
        let tab = &mut self.tabs[self.active];
        let before = tab.dividers.clone();
        tab.layout.set_ratio(&d.path, ratio);
        let (cols, rows) = self.grid;
        tab.lay_out(cols, rows, cell_pixels(&self.renderer));
        if tab.dividers != before {
            self.view.window().map(|w| w.invalidateCursorRectsForView(&self.view));
            self.request_redraw();
        }
    }

    /// Where the dividers are, in view points, and whether each one divides
    /// side by side; the view gives them resize pointers.
    fn divider_cursor_rects(&self) -> Vec<(NSRect, bool)> {
        let m = self.renderer.metrics();
        let s = self.scale();
        let height = self.view.bounds().size.height;
        self.tabs[self.active]
            .dividers
            .iter()
            .map(|d| {
                let (x, y) = self.renderer.pane_origin(d.rect.col, d.rect.row);
                let (w, h) = (d.rect.cols as f32 * m.width, d.rect.rows as f32 * m.height);
                let (x, y, w, h) = (x as f64 / s, y as f64 / s, w as f64 / s, h as f64 / s);
                (NSRect::new(NSPoint::new(x, height - y - h), NSSize::new(w, h)), d.dir == panes::Dir::Right)
            })
            .collect()
    }

    /// Park the per-pane state of the focused pane in it.
    fn save_live(&mut self) {
        let saved = PaneState {
            selection: self.selection.take(),
            preedit: std::mem::take(&mut self.preedit),
            mouse_held: self.mouse_held.take(),
            mouse_cell: self.mouse_cell,
            scroll_accum: self.scroll_accum,
            sync_since: self.sync_since.take(),
        };
        let tab = &mut self.tabs[self.active];
        tab.panes[tab.focus].saved = saved;
    }

    /// Take up the per-tab state of the tab now in front.
    fn load_live(&mut self) {
        // What the find bar found belongs to the pane it searched.
        self.close_find();
        self.rewind_exit();
        let tab = &mut self.tabs[self.active];
        tab.unseen = None;
        let pane = &mut tab.panes[tab.focus];
        let s = std::mem::take(&mut pane.saved);
        self.term = Arc::clone(&pane.term);
        self.pty = Arc::clone(&pane.pty);
        self.selection = s.selection;
        self.preedit = s.preedit;
        self.mouse_held = s.mouse_held;
        self.mouse_cell = s.mouse_cell;
        self.scroll_accum = s.scroll_accum;
        self.sync_since = s.sync_since;
        // The cursor layer shows the old tab's cursor until redrawn.
        self.cursor_shown = None;
        self.refresh_label(self.active);
        self.request_redraw();
    }

    /// The tab under a mouse event in the tab strip, if it is there.
    fn strip_hit(&self, event: &NSEvent) -> Option<usize> {
        if self.tabs.len() < 2 {
            return None;
        }
        let p = self.view.convertPoint_fromView(event.locationInWindow(), None);
        let scale = self.scale() as f32;
        let height = self.view.bounds().size.height as f32;
        self.renderer
            .strip_index(p.x as f32 * scale, (height - p.y as f32) * scale, self.tabs.len())
    }

    /// The view changed size: resize the drawable, and the grid if a whole
    /// cell more or fewer now fits.
    fn layout(&mut self) {
        let scale = self.scale();
        let bounds = self.view.bounds();
        let (w, h) = ((bounds.size.width * scale) as f32, (bounds.size.height * scale) as f32);
        self.renderer.resize(w, h);

        // Around the notch the tabs always show, left of it.
        let strip = self.tabs.len() > 1 || self.notch.is_some();
        self.renderer.set_strip(strip);
        self.apply_chrome();
        let inset = self.title_bar_height();
        self.renderer.set_top_inset((inset * scale).round() as f32);
        if self.native_strip {
            // In a title bar the capsule sits there, taking no rows.
            let band = if self.title_bar || self.notch.is_some() { 0.0 } else { strip::BAND_HEIGHT };
            self.renderer.set_strip_band(Some((band * scale).round() as f32));
            self.place_capsule(strip);
        }
        let grid = self.renderer.grid_size_for(w, h);
        if grid != self.grid {
            crate::log!("window {}: grid {}x{} -> {}x{}", self.id, self.grid.0, self.grid.1, grid.0, grid.1);
            self.grid = grid;
        }
        // Every tab, so one brought forward is already the right size.
        let cell = cell_pixels(&self.renderer);
        for tab in &mut self.tabs {
            tab.lay_out(grid.0, grid.1, cell);
        }
        self.window.invalidateCursorRectsForView(&self.view);
        schedule_layout_save();
        self.update_resize_increments();
        self.request_redraw();
    }

    /// Show, place or hide the glass tab capsule.
    fn place_capsule(&mut self, show: bool) {
        if show && self.capsule.is_none() {
            if let Some(mtm) = MainThreadMarker::new() {
                self.capsule = Some(strip::Capsule::new(mtm, &self.view, self.id));
            }
        }
        if let Some(c) = &self.capsule {
            c.glass.setHidden(!show);
            if show {
                let size = self.view.bounds().size;
                let bar = self.title_bar_height();
                let left = self.window.screen().map(|s| s.auxiliaryTopLeftArea().size.width).unwrap_or(0.0);
                if self.notch.is_some() && bar > 0.0 && left > 0.0 {
                    // Left of the notch.
                    let y = size.height - bar + (bar - strip::CAPSULE_HEIGHT) / 2.0;
                    c.place_within(0.0, left, self.tabs.len(), y, 12.0);
                } else if self.title_bar && bar > 0.0 {
                    // In the title bar, clear of the window buttons.
                    let y = size.height - bar + (bar - strip::CAPSULE_HEIGHT) / 2.0;
                    c.place_at(size.width, self.tabs.len(), y, BUTTON_ZONE.0 + 8.0);
                } else {
                    c.place(size.width, self.tabs.len());
                }
                let tabs = self.tabs.iter().map(|t| (t.label.clone(), t.unseen)).collect();
                c.view.set_tabs(tabs, self.active);
            }
        }
    }

    /// How tall the title bar is, in points: zero edge-to-edge.
    fn title_bar_height(&self) -> f64 {
        if self.notch.is_some() {
            return self.window.screen().map_or(0.0, |s| s.safeAreaInsets().top);
        }
        if !self.title_bar {
            return 0.0;
        }
        (self.view.bounds().size.height - self.window.contentLayoutRect().size.height).max(0.0)
    }

    /// Show the title bar and its buttons for the standard layout, with the
    /// title unless the tabs are there instead; or hide them, edge-to-edge,
    /// until the pointer reaches the corner.
    fn apply_chrome(&mut self) {
        if self.notch.is_some() {
            return;
        }
        if self.title_bar {
            set_titlebar_hidden(&self.window, false);
            self.buttons_visible = true;
            let tabs_in_bar = self.native_strip && self.tabs.len() > 1;
            self.window.setTitleVisibility(if tabs_in_bar { NSWindowTitleVisibility::Hidden } else { NSWindowTitleVisibility::Visible });
        } else {
            self.window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
            if self.buttons_visible {
                self.buttons_visible = false;
                set_titlebar_hidden(&self.window, true);
            }
        }
    }

    /// ⌘F: show the find bar, its text selected, ready to type.
    fn open_find(&mut self) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let top = self.title_bar_height();
        let size = self.view.bounds().size;
        let bar = self.find_bar.get_or_insert_with(|| FindBar::new(mtm, &self.view));
        bar.place(size, top);
        bar.host.setHidden(false);
        self.window.makeFirstResponder(Some(&bar.field));
        // SAFETY: selecting a field's own text on the main thread.
        unsafe { bar.field.selectText(None) };
        if self.find.is_none() {
            self.find_changed();
        }
    }

    /// The query or the regex switch changed: search again, and show the
    /// match nearest the bottom of the screen.
    fn find_changed(&mut self) {
        let Some(bar) = &self.find_bar else { return };
        let query = bar.field.stringValue().to_string();
        let regex = bar.regex.state() == objc2_app_kit::NSControlStateValueOn;
        if query.is_empty() {
            self.find = None;
            bar.count.setStringValue(&NSString::from_str(""));
            self.request_redraw();
            return;
        }
        let re = match crate::term::find::pattern(&query, regex) {
            Ok(re) => re,
            Err(e) => {
                self.find = None;
                bar.count.setStringValue(&NSString::from_str(&e));
                self.request_redraw();
                return;
            }
        };
        let (matches, lines, bottom) = {
            let t = self.term.lock();
            let g = t.grid();
            let bottom = g.history_len() - g.display_offset() + t.rows();
            (crate::term::find::find(&t, &re, FIND_LIMIT), g.total_lines(), bottom)
        };
        // The newest match that starts on or above the screen's last row.
        let current = matches.iter().rposition(|m| m.start.0 < bottom).or((!matches.is_empty()).then_some(0));
        self.find = Some(FindState { query, regex, matches, current, lines });
        self.reveal_match();
    }

    /// Return or ⌘G: the next match up (older); with Shift or ⌘⇧G, down.
    fn find_step(&mut self, older: bool) {
        let Some(f) = &self.find else {
            let has_text = self.find_bar.as_ref().is_some_and(|b| !b.host.isHidden() && !b.field.stringValue().is_empty());
            if has_text {
                self.find_changed();
            } else {
                self.open_find();
            }
            return;
        };
        // The text changed without a keystroke: search it, and stop there.
        if let Some(bar) = &self.find_bar {
            let regex = bar.regex.state() == objc2_app_kit::NSControlStateValueOn;
            if bar.field.stringValue().to_string() != f.query || regex != f.regex {
                self.find_changed();
                return;
            }
        }
        // Output arrived since: the lines moved, so search again first.
        let lines = self.term.lock().grid().total_lines();
        if lines != f.lines {
            self.find_changed();
        }
        let Some(f) = self.find.as_mut() else { return };
        let n = f.matches.len();
        if n == 0 {
            objc2_app_kit::NSBeep();
            return;
        }
        let i = f.current.unwrap_or(0);
        f.current = Some(if older { (i + n - 1) % n } else { (i + 1) % n });
        self.reveal_match();
    }

    /// Scroll the current match into view, near the middle, and update the
    /// count.
    fn reveal_match(&mut self) {
        let Some(f) = &self.find else { return };
        if let Some(bar) = &self.find_bar {
            let total = if f.matches.len() >= FIND_LIMIT { format!("{}+", group_digits(FIND_LIMIT as u64)) } else { group_digits(f.matches.len() as u64) };
            let text = match f.current {
                Some(i) => format!("{} of {total}", group_digits(i as u64 + 1)),
                None => "No matches".to_string(),
            };
            bar.count.setStringValue(&NSString::from_str(&text));
        }
        if let Some(m) = f.current.map(|i| f.matches[i]) {
            let mut t = self.term.lock();
            let g = t.grid();
            let (h, rows, off) = (g.history_len(), t.rows(), g.display_offset());
            let top = h - off;
            if m.start.0 < top || m.start.0 >= top + rows {
                let want = (h + rows / 2).saturating_sub(m.start.0).min(h);
                t.scroll_display(want as isize - off as isize);
            }
        }
        self.request_redraw();
    }

    /// Escape: close the bar and clear the highlights; keys go back to the
    /// terminal.
    fn close_find(&mut self) {
        let Some(bar) = &self.find_bar else { return };
        if bar.host.isHidden() {
            return;
        }
        bar.host.setHidden(true);
        self.find = None;
        self.window.makeFirstResponder(Some(&self.view));
        self.request_redraw();
    }

    /// View > Full Screen Around the Notch: the window covers the whole
    /// screen, the menu bar and Dock hidden while Tachyon is in front; the
    /// tabs go left of the notch, a status line right of it, and every row
    /// below is the terminal. On a screen without a notch, the band is just
    /// the space the menu bar had. Again to go back.
    fn toggle_notch(&mut self) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let app = NSApplication::sharedApplication(mtm);
        match self.notch.take() {
            None => {
                let Some(screen) = self.window.screen().or_else(|| NSScreen::mainScreen(mtm)) else { return };
                self.notch = Some((self.window.frame(), self.window.styleMask()));
                self.window.setStyleMask(NSWindowStyleMask::Borderless);
                // Still a full-screen window as far as AppKit is concerned,
                // so ⌃⌘F and View > Exit Full Screen stay enabled to leave.
                self.window.setCollectionBehavior(
                    self.window.collectionBehavior() | objc2_app_kit::NSWindowCollectionBehavior::FullScreenPrimary,
                );
                self.window.setFrame_display(screen.frame(), true);
                app.setPresentationOptions(
                    objc2_app_kit::NSApplicationPresentationOptions::HideDock
                        | objc2_app_kit::NSApplicationPresentationOptions::HideMenuBar,
                );
                self.window.makeKeyAndOrderFront(None);
                self.window.makeFirstResponder(Some(&self.view));
                let id = self.id;
                let tick = block2::RcBlock::new(move |_t: std::ptr::NonNull<objc2_foundation::NSTimer>| {
                    with_window(id, |w| {
                        w.update_notch_status();
                    });
                });
                self.notch_timer = Some(unsafe { objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(15.0, true, &tick) });
            }
            Some((frame, style)) => {
                app.setPresentationOptions(objc2_app_kit::NSApplicationPresentationOptions::Default);
                self.window.setStyleMask(style);
                self.window.setFrame_display(frame, true);
                set_titlebar_hidden(&self.window, true);
                self.buttons_visible = false;
                if let Some(label) = &self.notch_status {
                    label.setHidden(true);
                }
                if let Some(t) = self.notch_timer.take() {
                    t.invalidate();
                }
                // A new style rebuilds the window's frame, and focus falls
                // back to the window itself: give it to the terminal again,
                // or keys and menu commands have nowhere to go.
                self.window.makeKeyAndOrderFront(None);
                self.window.makeFirstResponder(Some(&self.view));
            }
        }
        self.layout();
    }

    /// Right of the notch: what the pane in front is doing (its command and
    /// for how long, or its folder) and the time, since the menu bar's clock
    /// is hidden.
    fn update_notch_status(&mut self) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        if self.notch.is_none() {
            return;
        }
        let Some(screen) = self.window.screen() else { return };
        let right = screen.auxiliaryTopRightArea();
        let bar = self.title_bar_height();
        if right.size.width <= 0.0 || bar <= 0.0 {
            return;
        }
        let label = self.notch_status.get_or_insert_with(|| {
            let l = objc2_app_kit::NSTextField::labelWithString(&NSString::from_str(""), mtm);
            l.setFont(Some(&objc2_app_kit::NSFont::monospacedSystemFontOfSize_weight(12.0, unsafe { objc2_app_kit::NSFontWeightMedium })));
            l.setTextColor(Some(&NSColor::secondaryLabelColor()));
            l.setAlignment(objc2_app_kit::NSTextAlignment::Right);
            l.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingHead);
            self.view.addSubview(&l);
            l
        });
        let text = {
            let term = self.term.lock();
            let doing = match term.running_cmdline().zip(term.running_elapsed()) {
                Some((cmd, t)) => format!("▶ {cmd} · {}", receipts::duration(t)),
                None => term.cwd.as_deref().map(short_path).unwrap_or_default(),
            };
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            let now = unsafe { libc::time(std::ptr::null_mut()) };
            unsafe { libc::localtime_r(&now, &mut tm) };
            format!("{doing}   {:02}:{:02}", tm.tm_hour, tm.tm_min)
        };
        label.setStringValue(&NSString::from_str(&text));
        let size = self.view.bounds().size;
        // Room at the right edge for macOS's camera and microphone dots.
        let x = right.origin.x - screen.frame().origin.x + 12.0;
        let w = (right.size.width - 12.0 - 40.0).max(0.0);
        label.setFrame(NSRect::new(NSPoint::new(x, size.height - bar + (bar - 16.0) / 2.0), NSSize::new(w, 16.0)));
        label.setHidden(false);
    }

    /// View > Show Title Bar: switch layouts.
    fn set_title_bar(&mut self, on: bool) {
        if on == self.title_bar {
            return;
        }
        self.title_bar = on;
        if !on {
            // Hide everything now; hovering the corner brings it back.
            self.buttons_visible = true;
        }
        self.layout();
    }

    fn rescale(&mut self) {
        self.renderer.set_scale(self.scale() as f32);
        self.layout();
    }

    /// Snap window resizing to whole cells, so the grid never has a sliver.
    fn update_resize_increments(&self) {
        let m = self.renderer.metrics();
        let s = self.scale();
        self.window.setContentResizeIncrements(NSSize::new(m.width as f64 / s, m.height as f64 / s));
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.update_secure_input();
        if !focused {
            self.hover(None);
        }
        self.request_redraw();
    }

    /// Handle a key press. Returns true when it should go to the input
    /// system instead (ordinary typing, dead keys, input methods).
    fn key_down(&mut self, event: &NSEvent) -> bool {
        // When the key was pressed, for the latency ledger: text the input
        // system commits from this event is stamped with it too.
        self.key_time = Some(ledger::from_seconds(event.timestamp()));
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            // Command shortcuts belong to the menu bar; one that reaches us
            // has no binding.
            return false;
        }
        // Rewound: Escape goes back to live; any other key does too, and
        // then goes to the shell as usual.
        if self.rewound.is_some() {
            self.rewind_exit();
            const ESCAPE: u16 = 53;
            if event.keyCode() == ESCAPE {
                return false;
            }
        }
        // Mid-composition, every key belongs to the input method.
        if !self.preedit.is_empty() {
            return true;
        }
        // Escape closes the training chart, when it's open.
        const ESCAPE_KEY: u16 = 53;
        if event.keyCode() == ESCAPE_KEY && self.chart.is_some() {
            self.chart = None;
            self.request_redraw();
            return false;
        }
        // A suggestion showing: Tab, →, End or ⌃F take it, ⌥→ a word of it.
        // Tab only while one shows: otherwise it's the shell's completion.
        if let Some(rest) = self.ghost.clone() {
            const RIGHT: u16 = 124;
            const END: u16 = 119;
            const F: u16 = 3;
            const TAB: u16 = 48;
            let (opt, ctrl, shift) = (
                flags.contains(NSEventModifierFlags::Option),
                flags.contains(NSEventModifierFlags::Control),
                flags.contains(NSEventModifierFlags::Shift),
            );
            let take = match event.keyCode() {
                RIGHT if !ctrl && !shift => Some(if opt { suggest::next_word(&rest).to_string() } else { rest }),
                END | TAB if !ctrl && !shift && !opt => Some(rest),
                F if ctrl && !opt && !shift => Some(rest),
                _ => None,
            };
            if let Some(text) = take.filter(|t| !t.is_empty()) {
                self.term.lock().scroll_to_bottom();
                let _ = self.pty.write(text.as_bytes());
                self.stamp_keystroke();
                self.ghost = None;
                self.ghost_cache = None;
                self.cursor_shown = None;
                self.request_redraw();
                return false;
            }
        }
        // ⌃⇥ normally arrives as a key equivalent; this catches it if not.
        if flags.contains(NSEventModifierFlags::Control) && event.keyCode() == 48 {
            self.cycle_tab(if flags.contains(NSEventModifierFlags::Shift) { -1 } else { 1 });
            return false;
        }
        let mods = input::Mods {
            shift: flags.contains(NSEventModifierFlags::Shift),
            ctrl: flags.contains(NSEventModifierFlags::Control),
            option: flags.contains(NSEventModifierFlags::Option),
        };
        let chars = event.characters().map(|s| s.to_string()).unwrap_or_default();
        let bare = event
            .charactersIgnoringModifiers()
            .map(|s| s.to_string())
            .unwrap_or_default();
        // Device-dependent modifier bits tell the two Option keys apart.
        const LEFT_OPTION: usize = 0x20;
        const RIGHT_OPTION: usize = 0x40;
        let raw = flags.0 as usize;
        let meta = match self.option_as_meta {
            OptionAsMeta::Off => false,
            OptionAsMeta::Both => true,
            OptionAsMeta::Left => raw & LEFT_OPTION != 0,
            OptionAsMeta::Right => raw & RIGHT_OPTION != 0,
        };
        // Plain typing goes through the input system, so dead keys compose and
        // input methods work. Keys the shell needs exactly come straight here.
        let direct = input::is_shell_key(event.keyCode()) || mods.ctrl || (mods.option && meta);
        if !direct {
            return true;
        }
        let mode = self.term.lock().mode;
        if let Some(bytes) = input::encode(event.keyCode(), &chars, &bare, mods, mode, meta) {
            self.term.lock().scroll_to_bottom();
            let _ = self.pty.write(&bytes);
            self.stamp_keystroke();
            NSCursor::setHiddenUntilMouseMoves(true);
            if self.selection.take().is_some() {
                self.request_redraw();
            }
            // Typing restarts the blink in its visible phase.
            self.cursor_shown = None;
        }
        false
    }

    /// Text committed by the input system: typed characters, a composed
    /// accent, or an input method's conversion.
    fn type_text(&mut self, text: &str) {
        self.preedit.clear();
        if text.is_empty() {
            self.request_redraw();
            return;
        }
        self.term.lock().scroll_to_bottom();
        let _ = self.pty.write(text.as_bytes());
        self.stamp_keystroke();
        NSCursor::setHiddenUntilMouseMoves(true);
        self.selection = None;
        self.cursor_shown = None;
        self.request_redraw();
    }

    /// A keystroke went to the focused pane's shell: start timing it.
    fn stamp_keystroke(&mut self) {
        let key = self.key_time.take().unwrap_or_else(ledger::now);
        self.tabs[self.active].pane().ledger.keystroke(key);
    }

    fn set_preedit(&mut self, text: String) {
        if text != self.preedit {
            self.preedit = text;
            self.request_redraw();
        }
    }

    /// The cursor cell in screen coordinates, for placing an input method's
    /// candidate window.
    fn cursor_screen_rect(&mut self) -> NSRect {
        let c = self.term.lock().cursor_state();
        let m = self.renderer.metrics();
        let rect = self.tabs[self.active].pane().rect;
        let (pad_x, pad_y) = self.renderer.pane_origin(rect.col, rect.row);
        let s = self.scale();
        let h = self.view.bounds().size.height;
        let x = (pad_x + c.x as f32 * m.width) as f64 / s;
        let top = (pad_y + c.y as f32 * m.height) as f64 / s;
        let cell_h = m.height as f64 / s;
        let in_view = NSRect::new(NSPoint::new(x, h - top - cell_h), NSSize::new(m.width as f64 / s, cell_h));
        self.window.convertRectToScreen(self.view.convertRect_toView(in_view, None))
    }

    fn paste(&mut self) {
        let board = NSPasteboard::generalPasteboard();
        let Some(text) = board.stringForType(unsafe { NSPasteboardTypeString }) else {
            return;
        };
        // Terminals send CR for a newline, as if each line were typed.
        let text = text.to_string().replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = {
            let mut t = self.term.lock();
            t.scroll_to_bottom();
            t.mode.contains(TermMode::BRACKETED_PASTE)
        };
        let mut bytes = Vec::with_capacity(text.len() + 12);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        let _ = self.pty.write(&bytes);
    }

    fn scroll(&mut self, event: &NSEvent) {
        let dy = event.scrollingDeltaY();
        let lines = if event.hasPreciseScrollingDeltas() {
            // Trackpads report points; turn them into whole lines and keep
            // the remainder so slow swipes still add up.
            let line_pts = self.renderer.metrics().height as f64 / self.scale();
            self.scroll_accum += dy / line_pts;
            let whole = self.scroll_accum.trunc();
            self.scroll_accum -= whole;
            whole as isize
        } else {
            (dy.round() as isize) * 3
        };
        if lines == 0 {
            return;
        }
        // Rewound: the wheel steps through frames instead.
        if self.rewound.is_some() {
            for _ in 0..lines.unsigned_abs().min(5) {
                self.rewind_step(lines > 0);
                if self.rewound.is_none() {
                    break;
                }
            }
            return;
        }

        // The wheel scrolls the pane under the pointer, focused or not.
        let p = self.pane_at(event).unwrap_or(self.tabs[self.active].focus);
        let (term, pty) = {
            let pane = &self.tabs[self.active].panes[p];
            (Arc::clone(&pane.term), Arc::clone(&pane.pty))
        };
        let mode = term.lock().mode;
        if mode.intersects(TermMode::ANY_MOUSE) {
            let (_, cell) = self.cell_in(event, p);
            let button = if lines > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            for _ in 0..lines.unsigned_abs() {
                if let Some(bytes) = input::encode_mouse(button, MouseEventKind::Press, cell.0, cell.1, Self::mods_of(event), mode) {
                    let _ = pty.write(&bytes);
                }
            }
            return;
        }
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALT_SCROLL) {
            // Full-screen apps without mouse reporting get arrow keys, so the
            // wheel scrolls less, man pages and the like.
            let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let _ = pty.write(&arrow.repeat(lines.unsigned_abs()));
        } else {
            term.lock().scroll_display(lines);
            self.request_redraw();
        }
    }

    /// Edge-to-edge: the traffic lights stay hidden until the pointer reaches
    /// the top-left corner.
    fn hover(&mut self, location: Option<NSPoint>) {
        if self.title_bar || self.notch.is_some() {
            return;
        }
        let height = self.view.bounds().size.height;
        let show = location.is_some_and(|p| p.x < BUTTON_ZONE.0 && p.y > height - BUTTON_ZONE.1);
        if show != self.buttons_visible {
            self.buttons_visible = show;
            set_titlebar_hidden(&self.window, !show);
        }
    }
}

impl TermWindow {
    /// The grid position under a mouse event: the selection position (with
    /// an absolute line) and the screen cell an application would be told.
    fn cell_at(&self, event: &NSEvent) -> (Pos, (usize, usize)) {
        self.cell_in(event, self.tabs[self.active].focus)
    }

    /// Like `cell_at`, in pane `p` of the tab in front; a point outside the
    /// pane gives its nearest cell.
    fn cell_in(&self, event: &NSEvent, p: usize) -> (Pos, (usize, usize)) {
        let (gx, gy) = self.grid_point(event);
        let pane = &self.tabs[self.active].panes[p];
        let fx = gx - pane.rect.col as f32;
        let fy = gy - pane.rect.row as f32;

        let term = pane.term.lock();
        let (cols, rows) = (term.cols(), term.rows());
        let row = (fy.floor().max(0.0) as usize).min(rows.saturating_sub(1));
        let cell = (fx.floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let edge = (fx.round().max(0.0) as usize).min(cols);
        let first = term.grid().history_len().saturating_sub(term.grid().display_offset());
        (Pos { line: first + row, cell, edge }, (cell, row))
    }

    fn mods_of(event: &NSEvent) -> input::Mods {
        let f = event.modifierFlags();
        input::Mods {
            shift: f.contains(NSEventModifierFlags::Shift),
            ctrl: f.contains(NSEventModifierFlags::Control),
            option: f.contains(NSEventModifierFlags::Option),
        }
    }

    fn report(&mut self, button: MouseButton, kind: MouseEventKind, cell: (usize, usize), event: &NSEvent) {
        let mode = self.term.lock().mode;
        if let Some(bytes) = input::encode_mouse(button, kind, cell.0, cell.1, Self::mods_of(event), mode) {
            let _ = self.pty.write(&bytes);
        }
        self.mouse_cell = cell;
    }

    /// Whether the mouse belongs to the application. Shift takes it back for
    /// selecting, except mid-drag: the app is owed the release.
    fn mouse_for_app(&self, event: &NSEvent) -> bool {
        if self.mouse_held.is_some() {
            return true;
        }
        let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
        self.term.lock().mode.intersects(TermMode::ANY_MOUSE) && !shift
    }

    fn mouse_down(&mut self, event: &NSEvent, button: MouseButton) {
        // Rewound: Option-click tells where a cell came from; other clicks
        // do nothing, since selection belongs to the live screen.
        if self.rewound.is_some() && self.strip_hit(event).is_none() {
            if button == MouseButton::Left && event.modifierFlags().contains(NSEventModifierFlags::Option) {
                self.rewind_provenance(event);
            }
            return;
        }
        // The tab strip: click a tab to show it, middle-click to close it.
        if let Some(i) = self.strip_hit(event) {
            match button {
                MouseButton::Left => self.switch_to(i),
                MouseButton::Middle => self.remove_tab(i),
                _ => {}
            }
            return;
        }
        // Moving the window: ⌃⌘-drag anywhere, or a drag on the top edge.
        if button == MouseButton::Left {
            let f = event.modifierFlags();
            let p = self.view.convertPoint_fromView(event.locationInWindow(), None);
            let top_edge = self.view.bounds().size.height - p.y
                < (self.renderer.padding().1 / self.scale() as f32) as f64;
            if top_edge || f.contains(NSEventModifierFlags::Control | NSEventModifierFlags::Command) {
                self.window.performWindowDragWithEvent(event);
                return;
            }
            if let Some(d) = self.divider_at(event) {
                self.dragging_divider = Some(d);
                return;
            }
        }
        // The training chart: a click on it closes it, and a click on a
        // command's metrics opens it for that command.
        if button == MouseButton::Left {
            if self.chart_at(event) {
                self.chart = None;
                self.request_redraw();
                return;
            }
            if let Some((pane, cmd)) = self.metrics_at(event) {
                self.chart = Some(ChartView { pane, cmd: Some(cmd), made: None });
                self.request_redraw();
                return;
            }
        }
        // A click in another pane gives it focus, and then counts there.
        if let Some(p) = self.pane_at(event) {
            self.focus_pane(p);
        }
        if button == MouseButton::Left
            && event.modifierFlags().contains(NSEventModifierFlags::Command)
            && self.open_under_pointer(event)
        {
            return;
        }

        let (pos, cell) = self.cell_at(event);
        if self.mouse_for_app(event) {
            self.mouse_held = Some(button);
            self.report(button, MouseEventKind::Press, cell, event);
            return;
        }
        if button != MouseButton::Left {
            return;
        }

        let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
        match (&mut self.selection, shift) {
            // Shift-click extends the existing selection.
            (Some(sel), true) => {
                sel.extend(pos);
                sel.dragging = true;
            }
            _ => self.selection = Some(Selection::new(pos, Unit::from_clicks(event.clickCount() as isize))),
        }
        self.request_redraw();
    }

    fn mouse_dragged(&mut self, event: &NSEvent, button: MouseButton) {
        if self.dragging_divider.is_some() {
            self.drag_divider(event);
            return;
        }
        let (pos, cell) = self.cell_at(event);
        if self.mouse_held == Some(button) {
            if cell != self.mouse_cell {
                self.report(button, MouseEventKind::Move, cell, event);
            }
            return;
        }
        if let Some(sel) = self.selection.as_mut().filter(|s| s.dragging) {
            sel.extend(pos);
            self.request_redraw();
        }
    }

    fn mouse_up(&mut self, event: &NSEvent, button: MouseButton) {
        if self.dragging_divider.take().is_some() {
            schedule_layout_save();
            return;
        }
        if self.mouse_held == Some(button) {
            self.mouse_held = None;
            let (_, cell) = self.cell_at(event);
            self.report(button, MouseEventKind::Release, cell, event);
            return;
        }
        if let Some(sel) = &mut self.selection {
            sel.dragging = false;
            // A click with no drag selects nothing.
            let empty = sel.range(&self.term.lock()).is_none();
            if empty {
                self.selection = None;
                self.request_redraw();
            }
        }
    }

    /// ⌘[ and ⌘]: show the screen one frame earlier or later. Stepping
    /// forward past the newest frame goes back to live.
    fn rewind_step(&mut self, back: bool) {
        let log = Arc::clone(&self.tabs[self.active].pane().log);
        let log = log.lock();
        let frames = log.frames();
        if frames.len() < 2 {
            objc2_app_kit::NSBeep();
            return;
        }
        let live = frames.len() - 1;
        let index = match (&self.rewound, back) {
            (None, true) => live - 1,
            (None, false) => return,
            (Some(rw), true) => rw.index.saturating_sub(1),
            (Some(rw), false) if rw.index + 1 >= live => {
                drop(log);
                self.rewind_exit();
                return;
            }
            (Some(rw), false) => rw.index + 1,
        };
        drop(log);
        self.rewind_to(index);
    }

    /// Show frame `index`, from ⌘[ / ⌘], the slider or the wheel.
    fn rewind_to(&mut self, index: usize) {
        let log = Arc::clone(&self.tabs[self.active].pane().log);
        let log = log.lock();
        let frames = log.frames();
        let Some(&(seq, at)) = frames.get(index) else {
            return;
        };
        if self.rewound.as_ref().is_some_and(|rw| rw.seq == seq) {
            return;
        }
        let Some(term) = log.replay(seq) else {
            return;
        };
        drop(log);
        self.rewound = Some(Rewound { term, seq, index, frames: frames.len(), at, note: None });
        self.show_rewind_badge();
        self.request_redraw();
    }

    /// Option-click on a rewound screen: which output last wrote that cell.
    fn rewind_provenance(&mut self, event: &NSEvent) {
        let (_, (x, y)) = self.cell_at(event);
        let Some(rw) = &self.rewound else { return };
        let ch = rw.term.grid().row(y).cells().get(x).and_then(|c| c.scalar()).unwrap_or(' ');
        let log = Arc::clone(&self.tabs[self.active].pane().log);
        let found = log.lock().provenance(rw.seq, x, y);
        let note = match found {
            Some(p) => {
                let before = rw.at.saturating_duration_since(p.time).as_secs_f64();
                let how = if p.scrolled { "scrolled here by" } else { "written by" };
                // The context last, so a long one is what gets cut short.
                format!(
                    "‘{ch}’ column {}, row {} · byte {} · {before:.1} s before this frame · {how} {}",
                    x + 1,
                    y + 1,
                    group_digits(p.offset),
                    p.context
                )
            }
            None => format!("‘{ch}’ at column {}, row {}: not changed in the output kept", x + 1, y + 1),
        };
        if let Some(rw) = &mut self.rewound {
            rw.note = Some(note);
        }
        self.show_rewind_badge();
    }

    fn rewind_exit(&mut self) {
        if self.rewound.take().is_some() {
            if let Some(b) = &self.rewind_badge {
                b.glass.setHidden(true);
            }
            self.cursor_shown = None;
            self.request_redraw();
        }
    }

    fn show_rewind_badge(&mut self) {
        let Some(rw) = &self.rewound else { return };
        let ago = rw.at.elapsed().as_secs();
        let when = match ago {
            0 => "just now".to_string(),
            s if s < 120 => format!("{s} s ago"),
            s if s < 7200 => format!("{} min ago", s / 60),
            s => format!("{} h ago", s / 3600),
        };
        let text = format!("Rewind · {when} · {} of {}", rw.index + 1, rw.frames);
        let (index, frames, note) = (rw.index, rw.frames, rw.note.clone());
        if self.rewind_badge.is_none() {
            if let Some(mtm) = MainThreadMarker::new() {
                self.rewind_badge = RewindBadge::new(mtm, &self.view);
            }
        }
        if let Some(b) = &self.rewind_badge {
            // Below a title bar, when there is one.
            let mut size = self.view.bounds().size;
            size.height -= self.title_bar_height();
            b.show(&text, index, frames, note.as_deref(), size);
        }
    }

    /// Over a line rerun diff marked: a tooltip with the line it replaced.
    fn update_tooltip(&mut self, event: &NSEvent) {
        let (pos, _) = self.cell_at(event);
        let here = self.pane_at(event) == Some(self.tabs[self.active].focus);
        let note = if here { self.term.lock().rerun_note(pos.line) } else { None };
        if note != self.tooltip {
            self.view.setToolTip(note.as_deref().map(NSString::from_str).as_deref());
            self.tooltip = note;
        }
    }

    /// Bare motion, for applications that asked for every move (mode 1003).
    fn mouse_moved(&mut self, event: &NSEvent) {
        self.update_tooltip(event);
        if self.mouse_held.is_some() || !self.term.lock().mode.contains(TermMode::MOUSE_MOTION) {
            return;
        }
        if self.pane_at(event) != Some(self.tabs[self.active].focus) {
            return;
        }
        let (_, cell) = self.cell_at(event);
        if cell != self.mouse_cell {
            self.report(MouseButton::None, MouseEventKind::Move, cell, event);
        }
    }

    fn copy(&mut self) {
        let text = {
            let term = self.term.lock();
            let Some(r) = self.selection.and_then(|s| s.range(&term)) else {
                return;
            };
            term.text_range(r.start.0, r.start.1, r.end.0, r.end.1)
        };
        // Trailing blanks at a line end are screen padding, not text.
        let text = text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
        if text.is_empty() {
            return;
        }
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        board.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
    }

    fn select_all(&mut self) {
        self.selection = Some(Selection::all(&self.term.lock()));
        self.request_redraw();
    }

    /// ⌘+ and ⌘- step the font by a point; `0.0` returns to the configured
    /// size. The window keeps its size and the grid gains or loses cells.
    fn zoom_font(&mut self, step: f32) {
        let size = if step == 0.0 {
            self.base_font_size
        } else {
            (self.renderer.font_size() + step).clamp(6.0, 72.0)
        };
        if (size - self.renderer.font_size()).abs() < f32::EPSILON {
            return;
        }
        self.renderer.set_font_size(size);
        self.cursor_shown = None;
        self.layout();
    }

    /// Show, move or hide the cursor layer to match `overlay`, restarting the
    /// blink whenever the cursor changes so it is visible right after a move.
    fn update_cursor_layer(&mut self, overlay: Option<CursorOverlay>) {
        if overlay == self.cursor_shown {
            return;
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let layer = &self.cursor_layer;
        match &overlay {
            None => layer.setHidden(true),
            Some(o) => {
                let scale = self.scale();
                let height = self.view.bounds().size.height;
                let [x, y, w, h] = o.rect.map(|v| v as f64 / scale);
                layer.setFrame(CGRect::new(CGPoint::new(x, height - y - h), CGSize::new(w, h)));
                layer.setContentsScale(scale);
                match &o.image {
                    Some((iw, ih, pixels)) => {
                        let image = rgba_image(*iw as usize, *ih as usize, pixels);
                        unsafe {
                            layer.setContents(image.as_deref().map(|i| &*(i as *const _ as *const AnyObject)));
                        }
                        layer.setBackgroundColor(None);
                    }
                    None => {
                        unsafe { layer.setContents(None) };
                        let c = o.color.map(|v| v as f64 / 255.0);
                        layer.setBackgroundColor(Some(&CGColor::new_srgb(c[0], c[1], c[2], 1.0)));
                    }
                }
                layer.setHidden(false);
                restart_blink(layer);
            }
        }
        CATransaction::commit();
        self.cursor_shown = overlay;
    }
}

/// The inline suggestion for what's typed at `term`'s prompt, clipped to the
/// blank cells after the cursor (short of a right prompt), and the whole of it
/// in `full` for → to take. `cache` saves looking up the same text again on
/// every frame.
fn ghost_for(
    term: &Term,
    cache: &mut Option<(String, Option<String>, Option<String>)>,
    full: &mut Option<String>,
) -> Option<(usize, usize, String)> {
    *full = None;
    let (typed, blank) = term.prompt_input()?;
    let cwd = term.cwd.clone();
    let rest = match cache {
        Some((t, c, r)) if *t == typed && *c == cwd => r.clone(),
        _ => {
            let r = SUGGESTIONS.with(|s| s.borrow().as_ref().and_then(|h| h.suggest(&typed, cwd.as_deref()).map(str::to_string)));
            *cache = Some((typed, cwd, r.clone()));
            r
        }
    }?;
    let c = term.cursor_state();
    // Up to the end of the row, or two cells short of a right prompt.
    let room = if c.x + blank >= term.cols() { blank } else { blank.saturating_sub(2) };
    let mut width = 0;
    let shown: String = rest
        .chars()
        .take_while(|ch| {
            width += unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(1);
            width <= room
        })
        .collect();
    if shown.is_empty() {
        return None;
    }
    *full = Some(rest);
    Some((c.x, c.y, shown))
}

/// What a tab is called. While a command runs: a title it set, or else its
/// command line. At a prompt: the shell's own title if it sets one, or else
/// the folder. Titles a finished program left behind are ignored, so a tab
/// doesn't stay called "vim" after vim exits.
fn tab_label(term: &Term) -> String {
    let running = term
        .current_command()
        .filter(|&id| term.command(id).is_some_and(|c| c.state == CommandState::Running));
    let title = term.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    if let Some(t) = title {
        if term.title_cmd.is_none() || (running.is_some() && term.title_cmd == running) {
            return t.to_string();
        }
    }
    if let Some(cmdline) = running.and_then(|id| term.command(id)).and_then(|c| c.cmdline.as_deref()) {
        return cmdline.to_string();
    }
    if let Some(cwd) = term.cwd.as_deref() {
        return short_path(cwd);
    }
    title.unwrap_or("Tachyon").to_string()
}

/// A job tab's name: what the follow script reports while it runs
/// (`nexus j-1a2b3c4d · running on forge`), then how the job ended.
fn job_label(term: &Term, job: &JobRef) -> String {
    let base = job.title();
    if term.command_running() {
        let label = tab_label(term);
        return if label.starts_with(&base) { label } else { format!("{base} · following") };
    }
    match term.last_finished() {
        Some(c) if c.exit == Some(0) => format!("{base} · finished"),
        Some(c) => format!("{base} · failed ({})", c.exit.unwrap_or(1)),
        None => format!("{base} · starting"),
    }
}

/// A folder as a tab shows it: `~` for home, otherwise its own name.
fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return "/".into();
    }
    if !home.is_empty() && path == home.trim_end_matches('/') {
        return "~".into();
    }
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Gutter bars and receipts for the rows on screen. Each row belongs to the
/// command whose prompt is the nearest one at or above it.
fn command_decor(term: &Term, followed: &[String]) -> Decor {
    let g = term.grid();
    let first = g.history_len() - g.display_offset();
    let mut decor = Decor::default();

    // The command owning the top row may have started above the viewport.
    let mut owner = (first.saturating_sub(5000)..first)
        .rev()
        .map(|l| g.mark(l))
        .find(|m| m.has(RowMark::PROMPT) || m.has(RowMark::INPUT))
        .map(|m| m.cmd);

    for y in 0..term.rows() {
        let mark = g.mark(first + y);
        if mark.has(RowMark::PROMPT) || mark.has(RowMark::INPUT) {
            owner = Some(mark.cmd);
        }
        let Some(info) = owner.and_then(|id| term.command(id)) else {
            continue;
        };
        let outcome = match info.state {
            CommandState::Prompt => continue,
            CommandState::Running => Outcome::Running,
            CommandState::Done if info.exit.is_some_and(|e| e != 0) => Outcome::Failed,
            CommandState::Done => Outcome::Succeeded,
        };
        let changed = mark.has(RowMark::CHANGED) || mark.has(RowMark::NEW);
        decor.gutter.push((y, if changed { Outcome::Changed } else { outcome }));
        if mark.has(RowMark::INPUT) {
            let running = info.state == CommandState::Running;
            let plain_receipt = info
                .elapsed()
                .and_then(|e| receipts::describe(e, info.exit, running, info.receipt.as_ref()));
            // Rerun diff: how this run compares with the last.
            let rerun = info.rerun.as_ref().map(|c| match c.len() {
                0 => "same as last run".to_string(),
                1 => "1 line changed".to_string(),
                n => format!("{n} lines changed"),
            });
            let receipt = match (&rerun, &plain_receipt) {
                (Some(d), Some(r)) => Some(format!("{d} · {r}")),
                (Some(d), None) => Some(d.clone()),
                (None, r) => r.clone(),
            };
            // Keep-awake: said on the command's line while it holds the Mac.
            let receipt = match (term.awake_command().is_some() && term.awake_command() == owner, receipt) {
                (true, Some(r)) => Some(format!("☕ awake · {r}")),
                (true, None) => Some("☕ keeping the Mac awake".to_string()),
                (false, r) => r,
            };
            let lens = info.lens.describe();
            let metric_texts: Vec<String> = lens.iter().cloned().chain(info.lens.compact()).collect();
            // Both if they fit, else the metrics while it runs and the
            // receipt once it's done.
            let mut candidates = Vec::new();
            if let (Some(l), Some(r)) = (&lens, &receipt) {
                candidates.push(format!("{l}  ·  {r}"));
            }
            let compact = info.lens.compact();
            // A job it submitted that no tab follows yet: offer one.
            if let Some(job) = info.job.as_ref().filter(|j| !followed.contains(&j.id)) {
                let hint = format!("⌘J follows {}", job.id);
                candidates.insert(0, hint.clone());
                if let Some(r) = &receipt {
                    candidates.insert(0, format!("{hint}  ·  {r}"));
                }
            }
            if running {
                // While it runs, the metrics matter more than the time.
                candidates.extend(lens);
                candidates.extend(compact);
                candidates.extend(receipt);
            } else {
                candidates.extend(receipt);
                candidates.extend(lens);
                candidates.extend(compact);
            }
            if rerun.is_some() {
                candidates.extend(plain_receipt);
            }
            // The input line, or if the command fills it, the first of its
            // wrapped continuation lines with room at the right.
            let width = |t: &str| t.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1)).sum::<usize>();
            let room = |r: usize, t: &str| renderer::note_start(g.row(r).cells(), width(t)).is_some();
            'place: for text in candidates {
                let mut r = y;
                while !room(r, &text) && r + 1 < term.rows() && g.row(r).wrapped {
                    r += 1;
                }
                if room(r, &text) {
                    // Metrics in it: a click there opens the chart.
                    if let (true, Some(id), Some(start)) =
                        (metric_texts.iter().any(|m| text.contains(m.as_str())), owner, renderer::note_start(g.row(r).cells(), width(&text)))
                    {
                        decor.metrics.push((r, id, start, start + width(&text)));
                    }
                    decor.notes.push((r, text));
                    break 'place;
                }
            }
        }
    }
    // Keep-awake asked for at a prompt: the next command will have it.
    if term.awake_armed() && !term.keeps_awake() {
        decor.notes.push((term.cursor_state().y, "☕ the next command keeps the Mac awake".into()));
    }
    decor
}

/// On for 530 ms, off for 530 ms, forever -- the same rhythm as before, now
/// run by the window server.
fn restart_blink(layer: &CALayer) {
    let key = NSString::from_str("blink");
    layer.removeAnimationForKey(&key);
    let anim = CAKeyframeAnimation::animationWithKeyPath(Some(&NSString::from_str("opacity")));
    let values = NSArray::from_retained_slice(&[NSNumber::new_f32(1.0), NSNumber::new_f32(0.0)]);
    // Discrete keyframes need one more key time than values: each value holds
    // from its time to the next, and the last interval ends at 1.
    let times = NSArray::from_retained_slice(&[
        NSNumber::new_f32(0.0),
        NSNumber::new_f32(0.5),
        NSNumber::new_f32(1.0),
    ]);
    unsafe {
        anim.setValues(Some(&*(Retained::as_ptr(&values) as *const NSArray)));
    }
    anim.setKeyTimes(Some(&times));
    anim.setCalculationMode(unsafe { kCAAnimationDiscrete });
    anim.setDuration(1.06);
    anim.setRepeatCount(f32::INFINITY);
    layer.addAnimation_forKey(&anim, Some(&key));
}

/// A CGImage from straight-alpha-free RGBA8 rows (every pixel opaque).
fn rgba_image(w: usize, h: usize, pixels: &[u8]) -> Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    let mut data = pixels.to_vec();
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let ctx = unsafe {
        CGBitmapContextCreate(
            data.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    CGBitmapContextCreateImage(Some(&ctx))
}

/// Show or hide the window buttons together with the transparent title bar
/// strip they sit in. While hidden, the strip is gone entirely, so clicks on
/// the top rows of text reach the terminal; hovering the top-left corner brings
/// the native buttons back.
fn set_titlebar_hidden(window: &NSWindow, hidden: bool) {
    for b in [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ] {
        if let Some(button) = window.standardWindowButton(b) {
            button.setHidden(hidden);
        }
    }
    // Button -> NSTitlebarView -> NSTitlebarContainerView.
    let container = window
        .standardWindowButton(NSWindowButton::CloseButton)
        // SAFETY: plain view-hierarchy reads on the main thread.
        .and_then(|b| unsafe { b.superview() })
        .and_then(|v| unsafe { v.superview() });
    if let Some(container) = container {
        container.setHidden(hidden);
    }
}

impl App {
    /// Build a window and its session, and add it to the list. Ordering it on
    /// screen is left to the caller, outside the app-state borrow, because
    /// AppKit calls back into the delegate while it does that.
    /// A new window: with a fresh shell, or with `restored` tabs (and the
    /// index of the one in front).
    fn create_window(
        &mut self,
        mtm: MainThreadMarker,
        restored: Option<(Vec<Tab>, usize)>,
    ) -> Result<(Retained<NSWindow>, Retained<TachyonView>, u64), String> {
        let id = self.next_id;
        self.next_id += 1;
        let cfg = &self.cfg;

        // Size from the configured grid, at the main screen's scale until
        // the window knows which display it is on.
        let scale = NSScreen::mainScreen(mtm).map(|s| s.backingScaleFactor()).unwrap_or(2.0);
        let probe = renderer::grid_metrics(&cfg.font, scale as f32);
        let pad_x = (cfg.window.padding_x as f64 * scale).round();
        let pad_y = (cfg.window.padding_y as f64 * scale).round();
        let content = NSSize::new(
            (cfg.window.cols as f64 * probe.width as f64 + 2.0 * pad_x) / scale,
            (cfg.window.rows as f64 * probe.height as f64 + 2.0 * pad_y) / scale,
        );

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let window: Retained<TachyonWindow> = unsafe {
            msg_send![
                super(TachyonWindow::alloc(mtm).set_ivars(())),
                initWithContentRect: NSRect::new(NSPoint::new(0.0, 0.0), content),
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        };
        let window: Retained<NSWindow> = Retained::into_super(window);
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Tachyon"));
        // Edge-to-edge: no visible title bar, content runs to the top edge.
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        // Glass behind the text, unless Reduce Transparency is on, in which
        // case the window is opaque as macOS asks.
        let glass = cfg.window.backdrop != Backdrop::None
            && !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency();
        if glass {
            window.setOpaque(false);
            window.setBackgroundColor(Some(&NSColor::clearColor()));
        } else {
            window.setBackgroundColor(Some(&srgb_color(cfg.theme.background, 1.0)));
        }
        // A fixed theme also fixes the window's own appearance, so the glass
        // and window buttons match it.
        let fixed = match cfg.appearance {
            Appearance::Dark => Some(unsafe { NSAppearanceNameDarkAqua }),
            Appearance::Light => Some(unsafe { NSAppearanceNameAqua }),
            Appearance::Auto => None,
        };
        if let Some(name) = fixed {
            window.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
        }
        window.setContentMinSize(NSSize::new(
            (20.0 * probe.width as f64) / scale,
            (4.0 * probe.height as f64) / scale,
        ));

        let view = TachyonView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), content), id);
        view.setWantsLayer(true);
        let layer = view.layer().ok_or("the view has no layer")?;
        // SAFETY: makeBackingLayer always returns a CAMetalLayer.
        let layer: Retained<CAMetalLayer> = unsafe { Retained::cast_unchecked(layer) };
        let cursor_layer = CALayer::new();
        cursor_layer.setHidden(true);
        layer.addSublayer(&cursor_layer);
        let mut renderer_cfg = cfg.clone();
        if !glass {
            renderer_cfg.window.opacity = 1.0;
        }
        let renderer = Renderer::new(Rc::clone(&self.gpu), layer, &renderer_cfg, scale as f32)?;

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

        let mut host_glass = None;
        if glass {
            let (host, glass_view) = glass_host(mtm, &view, content, cfg.window.glass_clear);
            window.setContentView(Some(&host));
            host_glass = glass_view;
        } else {
            window.setContentView(Some(&view));
        }
        let delegate = WindowDelegate::new(mtm, id);
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        set_titlebar_hidden(&window, true);

        // The first window is centred; later ones cascade down and right.
        if self.windows.is_empty() {
            window.center();
            self.cascade = window.cascadeTopLeftFromPoint(NSPoint::new(0.0, 0.0));
        } else {
            self.cascade = window.cascadeTopLeftFromPoint(self.cascade);
        }

        let (tabs, active) = match restored {
            Some(r) => r,
            None => (vec![Tab::new(Pane::start(cfg, cfg.window.cols, cfg.window.rows, None, None)?)], 0),
        };
        let pane = tabs[active].pane();
        let (term, pty) = (Arc::clone(&pane.term), Arc::clone(&pane.pty));

        let link = unsafe { view.displayLinkWithTarget_selector(&view, sel!(tick:)) };
        // ProMotion displays run at 120 Hz only when asked; a frame sooner is
        // a keystroke on screen sooner. The link pauses when nothing changes,
        // so an idle window still costs nothing.
        link.setPreferredFrameRateRange(objc2_quartz_core::CAFrameRateRange { minimum: 60.0, maximum: 120.0, preferred: 120.0 });
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };

        self.windows.push(TermWindow {
            id,
            window: window.clone(),
            view: view.clone(),
            link,
            renderer,
            tabs,
            active,
            term,
            pty,
            title_shown: String::new(),
            needs_redraw: true,
            idle_ticks: 0,
            sync_since: None,
            sync_timeout: Duration::from_millis(cfg.render.sync_timeout_ms),
            grid: (0, 0),
            scroll_accum: 0.0,
            buttons_visible: false,
            title_bar: self.title_bar,
            focused: true,
            selection: None,
            preedit: String::new(),
            mouse_held: None,
            mouse_cell: (usize::MAX, usize::MAX),
            option_as_meta: cfg.macos.option_as_meta,
            base_font_size: cfg.font.size,
            notify_after: (cfg.macos.notify_after_secs > 0).then(|| Duration::from_secs(cfg.macos.notify_after_secs)),
            secure: false,
            theme_dark: cfg.theme.clone(),
            theme_light: cfg.theme_light.clone(),
            appearance: cfg.appearance,
            contrast: cfg.contrast,
            glass,
            native_strip: glass && objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some(),
            capsule: None,
            notch: None,
            notch_status: None,
            notch_timer: None,
            host_glass,
            glass_tint: cfg.window.glass_tint,
            followed: Vec::new(),
            tooltip: None,
            rewound: None,
            rewind_badge: None,
            dragging_divider: None,
            key_time: None,
            busy_frames: 0,
            ghost: None,
            ghost_cache: None,
            chart: None,
            find_bar: None,
            find: None,
            cursor_layer,
            cursor_shown: None,
            _delegate: delegate,
        });
        Ok((window, view, id))
    }

    /// Start a shell in a new tab of window `window`.
    fn new_tab(&mut self, window: u64) -> Result<(), String> {
        self.open_tab_for(window, None)
    }

    /// A new tab in window `window`, following `job` if given. A job that
    /// already has a tab there is brought forward instead.
    fn open_tab_for(&mut self, window: u64, job: Option<JobRef>) -> Result<(), String> {
        let Some(w) = self.windows.iter_mut().find(|w| w.id == window) else {
            return Ok(());
        };
        if let Some(j) = &job {
            let following = |p: &Pane| p.job.as_ref().is_some_and(|tj| tj.kind == j.kind && tj.id == j.id);
            if let Some(i) = w.tabs.iter().position(|t| t.panes.iter().any(following)) {
                w.switch_to(i);
                return Ok(());
            }
            if !w.followed.contains(&j.id) {
                w.followed.push(j.id.clone());
            }
        }
        let (cols, rows) = w.grid;
        let tab = Tab::new(Pane::start(&self.cfg, cols.max(1), rows.max(1), job, None)?);
        w.add_tab(tab);
        Ok(())
    }
}

impl App {
    /// Split the focused pane of window `window`'s tab in front: a new shell,
    /// in the same folder, takes the right or bottom half and focus.
    fn split_pane(&mut self, window: u64, dir: panes::Dir) -> Result<(), String> {
        // Where the new pane goes, worked out with a stand-in id until the
        // shell has its own.
        const NEW: u64 = u64::MAX;
        let Some(w) = self.windows.iter_mut().find(|w| w.id == window) else {
            return Ok(());
        };
        w.rewind_exit();
        let tab = &w.tabs[w.active];
        let from = tab.pane();
        let cwd = from.term.lock().local_cwd().map(str::to_string);
        let mut layout = tab.layout.clone();
        let from_id = from.id;
        layout.split(from_id, NEW, dir);
        let area = panes::Rect { col: 0, row: 0, cols: w.grid.0, rows: w.grid.1 };
        let rect = layout.layout(area).0.into_iter().find(|r| r.0 == NEW).map(|r| r.1).unwrap_or_default();
        if rect.cols < 2 || rect.rows < 1 {
            // No room for another pane.
            objc2_app_kit::NSBeep();
            return Ok(());
        }
        let mut pane = Pane::start(&self.cfg, rect.cols, rect.rows, None, cwd.as_deref())?;
        pane.rect = rect;
        let mut layout = w.tabs[w.active].layout.clone();
        layout.split(from_id, pane.id, dir);
        let tab = &mut w.tabs[w.active];
        tab.layout = layout;
        tab.zoomed = false;
        tab.panes.push(pane);
        let new = tab.panes.len() - 1;
        w.layout();
        w.focus_pane(new);
        Ok(())
    }
}

/// ⌘J, or ⌘-click on a job id: follow a job in a tab of its own.
fn follow_job(window: u64, job: Option<JobRef>) {
    let job = job.or_else(|| query_window(window, |w| w.term.lock().jobs.last().cloned()).flatten());
    let Some(job) = job else {
        objc2_app_kit::NSBeep();
        return;
    };
    if let Some(Err(e)) = with_app(|app| app.open_tab_for(window, Some(job))) {
        crate::log::force(&format!("could not open a job tab: {e}"));
    }
}

/// The command a job tab runs: Tachyon's follow script, through the same ssh
/// the job was submitted over.
fn follow_command(job: &JobRef) -> Option<String> {
    let script = integration_dir()?.join("tachyon-job");
    let script = script.to_string_lossy().replace('\'', "'\\''");
    Some(match &job.via {
        Some(ssh) => format!("{ssh} sh -s {} {} < '{script}'", job.kind.name(), job.id),
        None => format!("'{script}' {} {}", job.kind.name(), job.id),
    })
}

/// ⌘D, ⌘⇧D: split the focused pane.
fn split_pane(window: u64, dir: panes::Dir) {
    if let Some(Err(e)) = with_app(|app| app.split_pane(window, dir)) {
        crate::log::force(&format!("could not split: {e}"));
    }
}

/// ⌘T: a new tab in this window.
fn open_tab(window: u64) {
    if let Some(Err(e)) = with_app(|app| app.new_tab(window)) {
        crate::log::force(&format!("could not open a tab: {e}"));
    }
}

/// ⌘W: close the focused pane, or the tab with its last pane, or the
/// window with its last tab.
fn close_active_tab(window: u64) {
    let last = with_app(|app| {
        let w = app.windows.iter_mut().find(|w| w.id == window)?;
        let (i, p) = (w.active, w.tabs[w.active].focus);
        w.close_pane(i, p)
    })
    .flatten();
    // Outside the borrow: closing calls back into the window delegate.
    if let Some(window) = last {
        window.performClose(None);
    }
}

/// A view that puts `view` on glass: Liquid Glass on macOS 26 and later,
/// otherwise the blurred window material macOS used before.
fn glass_host(
    mtm: MainThreadMarker,
    view: &TachyonView,
    size: NSSize,
    clear: bool,
) -> (Retained<NSView>, Option<Retained<NSGlassEffectView>>) {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), size);
    if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some() {
        let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), frame);
        if clear {
            glass.setStyle(objc2_app_kit::NSGlassEffectViewStyle::Clear);
        }
        // Round like the window, so the glass's rim and the refraction along
        // it follow the window's edge.
        glass.setCornerRadius(16.0);
        glass.setContentView(Some(view));
        return (Retained::into_super(glass.clone()), Some(glass));
    }
    let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
    blur.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
    blur.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    blur.setState(NSVisualEffectState::Active);
    view.setFrame(frame);
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    blur.addSubview(view);
    (Retained::into_super(blur), None)
}

/// Open a new terminal window and bring it to the front.
fn open_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    match with_app(|app| app.create_window(mtm, None)) {
        Some(Ok((window, view, id))) => {
            with_window(id, |w| {
                w.apply_appearance();
                w.layout();
            });
            window.makeKeyAndOrderFront(None);
            window.makeFirstResponder(Some(&view));
            NSApplication::sharedApplication(mtm).activate();
        }
        Some(Err(e)) => {
            crate::log::force(&format!("could not open a window: {e}"));
            eprintln!("tachyon: could not open a window: {e}");
        }
        None => {}
    }
}

/// A shell exited: close its pane, or its tab with the last pane, or the
/// window with the last tab, the way Terminal.app does by default.
fn shell_exited(pane: u64) {
    let window = with_app(|app| {
        let w = app.windows.iter_mut().find(|w| w.tabs.iter().any(|t| t.index_of(pane).is_some()))?;
        let i = w.tabs.iter().position(|t| t.index_of(pane).is_some())?;
        let p = w.tabs[i].index_of(pane)?;
        w.close_pane(i, p)
    })
    .flatten();
    // `close` sends windowWillClose:, which removes the window's state.
    if let Some(window) = window {
        window.close();
    }
}

/// Drop a window's state: its display link, renderer, and PTY. Dropping the
/// PTY hangs up the shell.
fn remove_window(id: u64) {
    let removed = with_app(|app| {
        let i = app.windows.iter().position(|w| w.id == id)?;
        Some(app.windows.remove(i))
    });
    schedule_layout_save();
    drop(removed);
}

fn build_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let ns = NSString::from_str;
    let item = |menu: &NSMenu, title: &str, action: Sel, key: &str| unsafe {
        menu.addItemWithTitle_action_keyEquivalent(&ns(title), Some(action), &ns(key))
    };
    let submenu = |main: &NSMenu, menu: &NSMenu| {
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(menu));
        main.addItem(&holder);
    };

    let main = NSMenu::new(mtm);

    let app_menu = NSMenu::new(mtm);
    item(&app_menu, "Hide Tachyon", sel!(hide:), "h");
    item(&app_menu, "Quit Tachyon", sel!(terminate:), "q");
    // ⌥⌘Q: shells end too, instead of waiting at the host for next time.
    item(&app_menu, "Quit and End Shells", sel!(tachyonQuitEndingSessions:), "q")
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    submenu(&main, &app_menu);

    let file = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Shell"));
    item(&file, "New Tab", sel!(newTab:), "t");
    item(&file, "New Window", sel!(newWindow:), "n");
    item(&file, "Follow Job", sel!(tachyonFollowJob:), "j");
    file.addItem(&NSMenuItem::separatorItem(mtm));
    item(&file, "Split Right", sel!(tachyonSplitRight:), "d");
    item(&file, "Split Down", sel!(tachyonSplitDown:), "D");
    file.addItem(&NSMenuItem::separatorItem(mtm));
    item(&file, "Keep Awake While Running", sel!(tachyonKeepAwake:), "a")
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    file.addItem(&NSMenuItem::separatorItem(mtm));
    item(&file, "Close", sel!(tachyonCloseTab:), "w");
    // A capital letter means Shift as well: ⌘⇧W.
    item(&file, "Close Window", sel!(performClose:), "W");
    submenu(&main, &file);

    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Edit"));
    item(&edit, "Copy", sel!(copy:), "c");
    item(&edit, "Paste", sel!(paste:), "v");
    item(&edit, "Select All", sel!(selectAll:), "a");
    edit.addItem(&NSMenuItem::separatorItem(mtm));
    item(&edit, "Find…", sel!(tachyonFind:), "f");
    item(&edit, "Find Next", sel!(tachyonFindNext:), "g");
    item(&edit, "Find Previous", sel!(tachyonFindPrevious:), "G");
    submenu(&main, &edit);

    let view = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("View"));
    // ⌘= (the same key without Shift) is handled by the view; a hidden menu
    // item's shortcut does not fire.
    item(&view, "Bigger", sel!(tachyonBiggerFont:), "+");
    item(&view, "Smaller", sel!(tachyonSmallerFont:), "-");
    item(&view, "Actual Size", sel!(tachyonResetFont:), "0");
    // Up and down arrows: the function-key codepoints AppKit uses for them.
    item(&view, "Previous Command", sel!(tachyonPreviousCommand:), "\u{F700}");
    item(&view, "Next Command", sel!(tachyonNextCommand:), "\u{F701}");
    view.addItem(&NSMenuItem::separatorItem(mtm));
    item(&view, "Show Title Bar", sel!(tachyonTitleBar:), "");
    item(&view, "Full Screen Around the Notch", sel!(tachyonNotch:), "f")
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    item(&view, "Latency Ledger", sel!(tachyonLedger:), "l")
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    item(&view, "Training Chart", sel!(tachyonChart:), "c")
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    view.addItem(&NSMenuItem::separatorItem(mtm));
    item(&view, "Rewind", sel!(tachyonRewindBack:), "[");
    item(&view, "Forward", sel!(tachyonRewindForward:), "]");
    submenu(&main, &view);

    let window = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Window"));
    item(&window, "Minimize", sel!(performMiniaturize:), "m");
    item(&window, "Zoom", sel!(performZoom:), "");
    window.addItem(&NSMenuItem::separatorItem(mtm));
    // "{" and "}" are ⌘⇧[ and ⌘⇧], as in Safari and Terminal.
    item(&window, "Show Previous Tab", sel!(tachyonPreviousTab:), "{");
    item(&window, "Show Next Tab", sel!(tachyonNextTab:), "}");
    window.addItem(&NSMenuItem::separatorItem(mtm));
    // ⌘⌥ and an arrow: the function-key codepoints for the arrows.
    let option_command = NSEventModifierFlags::Command | NSEventModifierFlags::Option;
    for (title, action, key) in [
        ("Select Pane Left", sel!(tachyonPaneLeft:), "\u{F702}"),
        ("Select Pane Right", sel!(tachyonPaneRight:), "\u{F703}"),
        ("Select Pane Above", sel!(tachyonPaneUp:), "\u{F700}"),
        ("Select Pane Below", sel!(tachyonPaneDown:), "\u{F701}"),
    ] {
        item(&window, title, action, key).setKeyEquivalentModifierMask(option_command);
    }
    item(&window, "Zoom Pane", sel!(tachyonZoomPane:), "\r")
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
    submenu(&main, &window);
    app.setWindowsMenu(Some(&window));

    app.setMainMenu(Some(&main));
}

/// Start a shell on a PTY and the reader thread that feeds its terminal.
/// Start a shell: at the session host when there is one, so it outlives the
/// app, else here. Returns its id and PTY.
fn open_session(cfg: &Config, cols: usize, rows: usize, run: Option<&str>, cwd: Option<&str>) -> Result<(u64, Pty), String> {
    let (program, login) = if cfg.shell.program.is_empty() {
        (pty::default_shell(), true)
    } else {
        (cfg.shell.program.clone(), false)
    };
    let integration = integration_dir()
        .filter(|_| cfg.shell.integration)
        .and_then(|dir| pty::integration(&program, &dir, login));
    let (mut extra_env, args, login) = match integration {
        Some(i) => (i.env, i.args.into_iter().chain(cfg.shell.args.iter().cloned()).collect(), i.login),
        None => (Vec::new(), cfg.shell.args.clone(), login),
    };
    // A command for the new shell to run: zsh's integration runs it at the
    // first prompt as if typed; other shells get it typed ahead.
    let zsh_runs_it = run.is_some() && extra_env.iter().any(|(k, _)| k == "ZDOTDIR");
    if let (Some(cmd), true) = (run, zsh_runs_it) {
        extra_env.push(("TACHYON_RUN".into(), cmd.to_string()));
    }
    let at_host = HOST.with(|h| {
        h.borrow()
            .as_ref()
            .map(|c| c.spawn(cols, rows, cfg.scrollback, &program, &args, login, &extra_env, cwd))
    });
    let (id, pty) = match at_host {
        Some(Ok(r)) => (r.id, r.pty),
        failed => {
            if let Some(Err(e)) = failed {
                crate::log::force(&format!("session host: {e}; starting the shell here"));
            }
            let pty = Pty::spawn(cols as u16, rows as u16, &program, &args, login, &extra_env, cwd)
                .map_err(|e| format!("could not start {program}: {e}"))?;
            (LOCAL_IDS.fetch_add(1, Ordering::Relaxed), pty)
        }
    };
    crate::log!("tab {id}: spawned `{program}` {args:?} at {cols}x{rows}, integration: {}", !extra_env.is_empty());
    if let (Some(cmd), false) = (run, zsh_runs_it) {
        let _ = pty.write(format!("{cmd}\r").as_bytes());
    }
    Ok((id, pty))
}

/// Feed a shell's output to a new terminal, on a reader thread of its own.
/// A host's shell sends the screen to restore first, which is parsed as the
/// past: no replies to old queries, no notifications.
fn start_session(
    id: u64,
    pty: Pty,
    cfg: &Config,
    cols: usize,
    rows: usize,
    wake_pending: Arc<AtomicBool>,
    // Keep-awake was on at the host: on again once the screen is restored.
    awake_after_restore: bool,
    stamps: Arc<ledger::Slot>,
) -> Result<(Arc<Mutex<Term>>, Arc<Pty>, Arc<Mutex<RewindLog>>), String> {
    let pty = Arc::new(pty);
    let term = Arc::new(Mutex::new(Term::new(cols, rows, cfg.scrollback)));
    let log = Arc::new(Mutex::new(RewindLog::new(
        term.lock().rewind_snapshot(),
        rewind_root().map(|r| r.join(format!("{}-{id}", std::process::id()))),
    )));
    let reader_log = Arc::clone(&log);

    let reader_term = Arc::clone(&term);
    let reader_pty = Arc::clone(&pty);
    std::thread::Builder::new()
        .name(format!("tachyon-pty-reader-{id}"))
        .spawn(move || {
            let mut processor = Processor::new();
            // Receipts: the sampler wakes the window while a command runs so
            // its elapsed time stays current.
            let sampler = receipts::Sampler::spawn(move || {
                DispatchQueue::main().exec_async(move || {
                    with_tab(id, |w, i| {
                        if i == w.active {
                            w.request_redraw();
                        }
                    })
                });
            });
            let shell = reader_pty.pid();
            let mut cpu_at_start: FxHashMap<u32, Option<Duration>> = FxHashMap::default();
            let mut dump = std::env::var_os("TACHYON_DUMP").and_then(|p| std::fs::File::create(p).ok());
            // The metric lens reads a running command's output as lines.
            let mut lines = lens::LineSplitter::default();
            let mut dock_percent: Option<i32> = None;
            // Output rate, to tell a training log (read every line) from a
            // flood like cat of a big file (only sampled).
            let mut window = (Instant::now(), 0usize);
            let mut last_sample = Instant::now();
            // 128 KiB amortises the syscall over a burst without holding the
            // terminal lock for long.
            let mut buf = vec![0u8; 128 * 1024];
            // A host's shell: how much of what follows is the restored screen.
            let mut replay = 0usize;
            let mut awake_here = false;
            if reader_pty.is_remote() {
                let mut header = [0u8; 8];
                let mut got = 0;
                while got < 8 {
                    match reader_pty.read(&mut header[got..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got += n,
                    }
                }
                replay = u64::from_le_bytes(header) as usize;
                if replay > 0 {
                    reader_term.lock().restoring = true;
                }
            }
            loop {
                let want = if replay > 0 { replay.min(buf.len()) } else { buf.len() };
                let n = match reader_pty.read(&mut buf[..want]) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        crate::log!("tab {id}: pty read failed: {e}");
                        break;
                    }
                };
                if let Some(f) = dump.as_mut() {
                    use std::io::Write;
                    let _ = f.write_all(&buf[..n]);
                }
                let restoring = replay > 0;
                stamps.output_read();
                let (replies, events, running, cmdline) = {
                    let mut t = reader_term.lock();
                    processor.advance(&mut t, &buf[..n]);
                    if restoring {
                        replay -= n.min(replay);
                        t.restoring = replay > 0;
                        if replay == 0 && awake_after_restore {
                            t.set_keep_awake(true);
                        }
                    }
                    // Shells run here hold the power assertion themselves;
                    // the host holds it for its own.
                    if !reader_pty.is_remote() && t.keeps_awake() != awake_here {
                        awake_here = t.keeps_awake();
                        power::set(id, awake_here.then(|| t.running_cmdline().unwrap_or("").to_string()).as_deref());
                    }
                    {
                        // Rewind keeps the raw output, and every few MB a
                        // copy of the screen to replay it onto.
                        let mut log = reader_log.lock();
                        log.record(&buf[..n]);
                        if log.wants_snapshot() && processor.is_ground() {
                            log.snapshot(t.rewind_snapshot());
                        }
                    }
                    let cmdline = t.running_cmdline().map(str::to_string);
                    stamps.output_parsed();
                    let (replies, events) = (t.take_replies(), t.take_command_events());
                    if restoring {
                        // The past: its queries were answered long ago.
                        (Vec::new(), Vec::new(), false, None)
                    } else {
                        (replies, events, t.command_running(), cmdline)
                    }
                };
                if running {
                    // A training run prints a few lines a second, and every
                    // one is read. A flood (cat of a big log) isn't where
                    // metrics live: over 256 KB in 100 ms, only the end of a
                    // read is looked at, at most every 50 ms, which keeps the
                    // lens off the throughput.
                    let now = Instant::now();
                    if now.duration_since(window.0) > Duration::from_millis(100) {
                        window = (now, 0);
                    }
                    window.1 += n;
                    let flood = window.1 > 256 * 1024;
                    const TAIL: usize = 2 * 1024;
                    let chunk: &[u8] = if !flood {
                        &buf[..n]
                    } else if now.duration_since(last_sample) >= Duration::from_millis(50) {
                        last_sample = now;
                        lines.reset();
                        &buf[n.saturating_sub(TAIL)..n]
                    } else {
                        lines.reset();
                        &[]
                    };
                    let mut readings = Vec::new();
                    let mut found = Vec::new();
                    lines.feed(chunk, |line| {
                        let r = lens::read_line(line);
                        if !r.is_empty() {
                            readings.push(r);
                        }
                        if let Some(job) = jobs::detect(line, cmdline.as_deref()) {
                            found.push(job);
                        }
                    });
                    if !found.is_empty() {
                        let mut t = reader_term.lock();
                        for (kind, job_id) in found {
                            t.record_job(kind, job_id);
                        }
                    }
                    // A progress bar is redrawn after a \r rather than ended
                    // with one, so the line being written is its latest state.
                    if let Some(bar) = lines.pending().filter(|l| l.contains("%|")) {
                        readings.push(lens::read_line(&bar));
                    }
                    if !readings.is_empty() {
                        let percent = {
                            let mut t = reader_term.lock();
                            readings.iter().fold(None, |p, r| t.record_reading(r).or(p))
                        };
                        if let Some(p) = percent {
                            if dock_percent != Some(p as i32) {
                                dock_percent = Some(p as i32);
                                DispatchQueue::main().exec_async(move || dock::set_progress(id, Some(p)));
                            }
                        }
                    }
                }
                if !replies.is_empty() {
                    let _ = reader_pty.write(&replies);
                }
                for event in events {
                    match event {
                        CommandEvent::Started(cmd) => {
                            cpu_at_start.insert(cmd, receipts::children_cpu(shell));
                            sampler.start(shell, reader_pty.master_fd());
                        }
                        CommandEvent::Finished(cmd, _) => {
                            if dock_percent.take().is_some() {
                                DispatchQueue::main().exec_async(move || dock::set_progress(id, None));
                            }
                            let start = cpu_at_start.remove(&cmd).flatten();
                            let cpu = start
                                .zip(receipts::children_cpu(shell))
                                .map(|(a, b)| b.saturating_sub(a));
                            let receipt = sampler.finish(cpu);
                            reader_term.lock().set_receipt(cmd, receipt);
                            DispatchQueue::main().exec_async(move || with_tab(id, |w, i| w.command_finished(i, id, cmd)));
                        }
                    }
                }
                if !wake_pending.swap(true, Ordering::AcqRel) {
                    DispatchQueue::main().exec_async(move || {
                        with_tab(id, |w, i| {
                            if i == w.active {
                                w.resume();
                            } else {
                                w.background_output(i);
                            }
                        })
                    });
                }
            }
            DispatchQueue::main().exec_async(move || shell_exited(id));
        })
        .map_err(|e| format!("could not start the reader thread: {e}"))?;

    Ok((term, pty, log))
}

/// Queue a save of the windows' layout to the host, once per run of the
/// main queue, outside any borrow of the app state.
fn schedule_layout_save() {
    if LAYOUT_PENDING.get() || HOST.with(|h| h.borrow().is_none()) {
        return;
    }
    LAYOUT_PENDING.set(true);
    DispatchQueue::main().exec_async(save_layout);
}

/// Tell the host where every shell is: each window's frame and tab in front,
/// and each tab's split tree, focus and zoom. Lines of
/// `window <active tab> <x> <y> <width> <height>`, each followed by its
/// `tab <focus> <zoomed> <tree>` lines.
fn save_layout() {
    LAYOUT_PENDING.set(false);
    let Some(text) = with_app(|app| {
        let mut out = String::new();
        for w in &app.windows {
            let f = w.window.frame();
            out.push_str(&format!(
                "window {} {} {} {} {}\n",
                w.active, f.origin.x, f.origin.y, f.size.width, f.size.height
            ));
            for t in &w.tabs {
                out.push_str(&format!("tab {} {} {}\n", t.pane().id, t.zoomed as u8, t.layout.encode()));
            }
        }
        out
    }) else {
        return;
    };
    HOST.with(|h| {
        if let Some(c) = &*h.borrow() {
            if let Err(e) = c.set_layout(&text) {
                crate::log!("host: could not save the layout: {e}");
            }
        }
    });
}

/// A window to restore: its frame, tab in front, and tabs.
struct SavedWindow {
    frame: Option<NSRect>,
    active: usize,
    tabs: Vec<(u64, bool, panes::Node)>,
}

fn parse_layout(text: &str) -> Vec<SavedWindow> {
    let mut windows: Vec<SavedWindow> = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("window") => {
                let n: Vec<f64> = words.filter_map(|w| w.parse().ok()).collect();
                let frame = (n.len() == 5).then(|| NSRect::new(NSPoint::new(n[1], n[2]), NSSize::new(n[3], n[4])));
                windows.push(SavedWindow { frame, active: n.first().copied().unwrap_or(0.0) as usize, tabs: Vec::new() });
            }
            Some("tab") => {
                let (Some(focus), Some(zoomed)) = (words.next().and_then(|w| w.parse().ok()), words.next()) else {
                    continue;
                };
                if let (Some(w), Some(tree)) = (windows.last_mut(), panes::Node::decode(&mut words)) {
                    w.tabs.push((focus, zoomed == "1", tree));
                }
            }
            _ => {}
        }
    }
    windows
}

/// Reopen the windows the host's shells were in when Tachyon last closed.
/// Shells it has that no saved window holds get tabs of their own. Returns
/// whether any window opened.
fn restore_sessions(mtm: MainThreadMarker) -> bool {
    let found = HOST.with(|h| {
        let h = h.borrow();
        let c = h.as_ref()?;
        let ids: Vec<u64> = c.list().ok()?.into_iter().filter(|l| !l.attached).map(|l| l.id).collect();
        Some((ids, c.layout().unwrap_or_default()))
    });
    let Some((ids, layout)) = found.filter(|(ids, _)| !ids.is_empty()) else {
        return false;
    };
    crate::log!("restoring {} shells from the session host", ids.len());
    let mut saved = parse_layout(&layout);
    let mut placed = std::collections::HashSet::new();
    for w in &mut saved {
        for (_, _, tree) in &mut w.tabs {
            for id in tree.leaves() {
                if !ids.contains(&id) || !placed.insert(id) {
                    tree.remove(id);
                }
            }
        }
        // A tab whose last pane is gone (removing a lone pane can't).
        w.tabs.retain(|(_, _, tree)| tree.leaves().iter().all(|id| ids.contains(id)));
    }
    let rest: Vec<u64> = ids.iter().copied().filter(|id| !placed.contains(id)).collect();
    if !rest.is_empty() {
        if saved.is_empty() {
            saved.push(SavedWindow { frame: None, active: 0, tabs: Vec::new() });
        }
        saved[0].tabs.extend(rest.into_iter().map(|id| (id, false, panes::Node::Leaf(id))));
    }

    let mut opened = Vec::new();
    for w in saved {
        let tabs: Vec<Tab> = w
            .tabs
            .into_iter()
            .filter_map(|(focus, zoomed, mut tree)| {
                let mut panes_ = Vec::new();
                for id in tree.leaves() {
                    let attached = HOST.with(|h| h.borrow().as_ref().map(|c| c.attach(id)));
                    let pane = match attached {
                        Some(Ok(r)) => with_app(|app| Pane::attach(&app.cfg, r)).and_then(Result::ok),
                        _ => None,
                    };
                    match pane {
                        Some(p) => panes_.push(p),
                        None => {
                            tree.remove(id);
                        }
                    }
                }
                if panes_.is_empty() {
                    return None;
                }
                let mut tab = Tab::new(panes_.remove(0));
                tab.panes.extend(panes_);
                tab.layout = tree;
                tab.focus = tab.index_of(focus).unwrap_or(0);
                tab.zoomed = zoomed && tab.panes.len() > 1;
                Some(tab)
            })
            .collect();
        if tabs.is_empty() {
            continue;
        }
        let active = w.active.min(tabs.len() - 1);
        match with_app(|app| app.create_window(mtm, Some((tabs, active)))) {
            Some(Ok((window, view, id))) => {
                if let Some(frame) = w.frame {
                    window.setFrame_display(frame, false);
                }
                with_window(id, |w| {
                    w.apply_appearance();
                    w.layout();
                    w.refresh_label(w.active);
                });
                opened.push((window, view));
            }
            Some(Err(e)) => crate::log::force(&format!("could not restore a window: {e}")),
            None => {}
        }
    }
    let Some((window, view)) = opened.last() else {
        return false;
    };
    for (w, _) in &opened {
        w.orderFront(None);
    }
    window.makeKeyAndOrderFront(None);
    window.makeFirstResponder(Some(view));
    NSApplication::sharedApplication(mtm).activate();
    true
}

/// Where Rewind keeps older output, compressed: in the user's caches, one
/// private folder per tab, removed when the tab closes.
fn rewind_root() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join("Library/Caches/Tachyon/Rewind"))
}

/// Where the shell-integration scripts live: inside the app bundle, or the
/// source tree when running with `cargo run`.
fn integration_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bundled = exe.parent()?.parent()?.join("Resources/shell");
    if bundled.is_dir() {
        return Some(bundled);
    }
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("shell");
    dev.is_dir().then_some(dev)
}

pub fn run(cfg: Config) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("Tachyon must start on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(ICON_SVG)) {
        unsafe { app.setApplicationIconImage(Some(&icon)) };
    }
    // Native window tabs would merge our windows behind a title bar we hide;
    // Tachyon draws its own tab strip.
    NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
    // A double-click on a title bar fills the screen (or zooms, or
    // minimises), per the system setting. Tachyon's title bar is invisible and
    // sits over the top rows of text, where a double-click selects a word, so
    // turn the action off for this app only; System Settings is untouched.
    // SAFETY: storing an NSString value under an NSString key.
    let cfg_title_bar = cfg.window.title_bar;
    let cfg_notch = cfg.window.notch_full_screen;
    set_double_click_action(cfg_title_bar);
    build_menu(mtm, &app);

    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    NOTIFIER.with(|n| *n.borrow_mut() = notify::Notifier::new(mtm));
    // Increase Contrast (and Reduce Transparency) switched in System
    // Settings: every window picks its theme again. The observer lives as
    // long as the app.
    let contrast_changed = block2::RcBlock::new(|_n: std::ptr::NonNull<NSNotification>| {
        with_app(|app| app.windows.iter_mut().for_each(TermWindow::apply_appearance));
    });
    let observer = unsafe {
        NSWorkspace::sharedWorkspace().notificationCenter().addObserverForName_object_queue_usingBlock(
            Some(objc2_app_kit::NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification),
            None,
            Some(&objc2_foundation::NSOperationQueue::mainQueue()),
            &contrast_changed,
        )
    };
    std::mem::forget(observer);
    // Rewind folders left by a Tachyon that crashed.
    if let Some(root) = rewind_root() {
        crate::term::rewind::remove_stale(&root);
    }
    let gpu = Gpu::new()?;
    APP.with(|cell| {
        *cell.borrow_mut() = Some(App {
            cfg,
            gpu,
            windows: Vec::new(),
            next_id: 1,
            title_bar: cfg_title_bar,
            notch_full_screen: cfg_notch,
            cascade: NSPoint::new(0.0, 0.0),
            _delegate: delegate,
        });
    });

    if with_app(|a| a.cfg.shell.suggestions).unwrap_or(false) {
        SUGGESTIONS.with(|s| *s.borrow_mut() = Some(suggest::History::load()));
    }
    let persist = with_app(|a| a.cfg.shell.persist).unwrap_or(false);
    if persist {
        match host::Client::connect() {
            Ok(c) => HOST.with(|h| *h.borrow_mut() = Some(c)),
            Err(e) => crate::log::force(&format!("no session host ({e}); shells will end with the app")),
        }
    }
    if !restore_sessions(mtm) {
        open_window();
    }
    // Open around the notch, when that's what full screen means here.
    if cfg_notch {
        let key = with_app(|app| {
            let keyw = app.windows.iter().position(|w| w.window.isKeyWindow()).unwrap_or(0);
            app.windows.get(keyw).map(|w| w.id)
        })
        .flatten();
        if let Some(id) = key {
            DispatchQueue::main().exec_async(move || with_window(id, TermWindow::toggle_notch));
        }
    }
    if with_app(|a| a.windows.is_empty()).unwrap_or(true) {
        return Err("could not open the first window".into());
    }
    app.run();

    APP.with(|cell| cell.borrow_mut().take());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{short_path, tab_label};
    use crate::term::{Processor, Term};

    fn term_after(script: &str) -> Term {
        let mut t = Term::new(40, 10, 100);
        Processor::new().advance(&mut t, script.as_bytes());
        t
    }

    #[test]
    fn a_tab_is_named_after_its_folder_then_its_command() {
        let home = std::env::var("HOME").unwrap();
        let at_prompt = format!("\x1b]133;A\x07\x1b]7;file://h{home}/Github/Tachyon\x07$ \x1b]133;B\x07");
        let t = term_after(&at_prompt);
        assert_eq!(tab_label(&t), "Tachyon");

        let running = format!("{at_prompt}\x1b]133;C;cmdline=python train.py\x07");
        assert_eq!(tab_label(&term_after(&running)), "python train.py");

        // A title the program sets while it runs wins, and is dropped once it
        // exits.
        let vim = format!("{at_prompt}\x1b]133;C;cmdline=vim\x07\x1b]2;main.rs - VIM\x07");
        assert_eq!(tab_label(&term_after(&vim)), "main.rs - VIM");
        let after = format!("{vim}\x1b]133;D;0\x07\x1b]133;A\x07");
        assert_eq!(tab_label(&term_after(&after)), "Tachyon");
    }

    #[test]
    fn without_integration_the_title_names_the_tab() {
        assert_eq!(tab_label(&term_after("\x1b]2;remote host\x07")), "remote host");
        assert_eq!(tab_label(&term_after("")), "Tachyon");
    }

    #[test]
    fn folders_shorten() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(short_path(&home), "~");
        assert_eq!(short_path("/"), "/");
        assert_eq!(short_path("/usr/local/bin/"), "bin");
    }
}
