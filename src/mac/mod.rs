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
mod font;
mod input;
mod pty;
mod receipts;
mod renderer;
mod selection;

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
    NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSData, NSNotFound, NSRange, NSUserDefaults, NSNotification, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString,
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

use crate::config::{Config, OptionAsMeta};
use crate::term::{CommandEvent, CommandState, Processor, RowMark, Term, TermMode};
use rustc_hash::FxHashMap;
use pty::Pty;
use input::{MouseButton, MouseEventKind};
use renderer::{CursorOverlay, Decor, Gpu, Outcome, Renderer, Strip};
use selection::{Pos, Selection, Unit};

const ICON_SVG: &[u8] = include_bytes!("../../assets/icon/tachyon.svg");

/// Region in the top-left corner, in points, where the hidden window buttons
/// fade in.
const BUTTON_ZONE: (f64, f64) = (84.0, 34.0);

thread_local! {
    /// All main-thread state. AppKit calls into the views and delegates; each
    /// carries its window's id and forwards here.
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

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

/// Run `f` against the window holding a tab, with the tab's index in it.
fn with_tab(tab: u64, f: impl FnOnce(&mut TermWindow, usize)) {
    with_app(|app| {
        for w in &mut app.windows {
            if let Some(i) = w.tabs.iter().position(|t| t.id == tab) {
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
            true
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
        #[unsafe(method(sendEvent:))]
        fn send_event(&self, event: &NSEvent) {
            let t = event.r#type();
            let left = t == NSEventType::LeftMouseDown
                || t == NSEventType::LeftMouseDragged
                || t == NSEventType::LeftMouseUp;
            if left {
                if let Some(view) = self.contentView() {
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

    fn owns_click(&self, view: &NSView, p: NSPoint) -> bool {
        let size = view.frame().size;
        let m = Self::RESIZE_MARGIN;
        let inside = p.x > m && p.x < size.width - m && p.y > m && p.y < size.height - m;
        // The window buttons, while revealed, take their own clicks.
        let buttons_showing = self
            .standardWindowButton(NSWindowButton::CloseButton)
            .is_some_and(|b| !b.isHidden());
        let over_buttons = buttons_showing && p.x < BUTTON_ZONE.0 && p.y > size.height - BUTTON_ZONE.1;
        inside && !over_buttons
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
    gpu: Rc<Gpu>,
    windows: Vec<TermWindow>,
    next_id: u64,
    next_tab: u64,
    /// Where the next window's top-left corner goes, so new windows cascade.
    cascade: NSPoint,
    _delegate: Retained<AppDelegate>,
}

/// One shell session in a window.
struct Tab {
    id: u64,
    term: Arc<Mutex<Term>>,
    pty: Arc<Pty>,
    /// Set by the reader thread when it has asked the main thread to wake;
    /// cleared when the main thread handles it. Keeps the main queue from
    /// filling with redundant wake-ups during a flood of output.
    wake_pending: Arc<AtomicBool>,
    /// What the tab is called: the running command, the folder, or a title
    /// the program set. Refreshed when `Term::title_revision` moves.
    label: String,
    title_rev: u64,
    /// A command finished here while the tab was in the background.
    unseen: Option<Outcome>,
    /// The window's per-tab state, kept here while another tab is in front.
    saved: TabState,
}

/// Per-tab state the window works on directly while the tab is in front.
struct TabState {
    selection: Option<Selection>,
    preedit: String,
    mouse_held: Option<MouseButton>,
    mouse_cell: (usize, usize),
    scroll_accum: f64,
    sync_since: Option<Instant>,
}

impl Default for TabState {
    fn default() -> Self {
        TabState {
            selection: None,
            preedit: String::new(),
            mouse_held: None,
            mouse_cell: (usize::MAX, usize::MAX),
            scroll_accum: 0.0,
            sync_since: None,
        }
    }
}

impl Tab {
    fn start(id: u64, cfg: &Config, cols: usize, rows: usize) -> Result<Tab, String> {
        let wake_pending = Arc::new(AtomicBool::new(false));
        let (term, pty) = start_session(id, cfg, cols, rows, Arc::clone(&wake_pending))?;
        Ok(Tab {
            id,
            term,
            pty,
            wake_pending,
            label: String::new(),
            title_rev: u64::MAX,
            unseen: None,
            saved: TabState::default(),
        })
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        // The reader thread holds the PTY too, so dropping ours does not end
        // the session; hanging up does.
        self.pty.hangup();
    }
}

/// One window: its AppKit objects, renderer, and tabs.
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

    selection: Option<Selection>,
    /// Text an input method is composing, shown at the cursor until committed.
    preedit: String,
    /// A button an application is tracking: it is owed motion and a release.
    mouse_held: Option<MouseButton>,
    /// Last cell reported to the application, so motion is sent per cell.
    mouse_cell: (usize, usize),
    option_as_meta: OptionAsMeta,
    base_font_size: f32,

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
    }
}

impl TermWindow {
    fn scale(&self) -> f64 {
        self.window.backingScaleFactor()
    }

    /// Wake from idle. Called on the main queue by the reader thread.
    fn resume(&mut self) {
        self.idle_ticks = 0;
        self.link.setPaused(false);
    }

    fn tick(&mut self) {
        self.tabs[self.active].wake_pending.store(false, Ordering::Release);

        let (dirty, syncing, title_rev) = {
            let mut t = self.term.lock();
            (t.take_dirty(), t.mode.contains(TermMode::SYNC_UPDATE), t.title_revision)
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
            let term = self.term.lock();
            let blink = self.focused && term.cursor_state().blinking;
            let selection = self.selection.and_then(|s| s.range(&term));
            let mut decor = command_decor(&term);
            decor.preedit = self.preedit.clone();
            if self.tabs.len() > 1 {
                decor.strip = Some(Strip {
                    tabs: self.tabs.iter().map(|t| (t.label.clone(), t.unseen)).collect(),
                    active: self.active,
                });
            }
            self.renderer.draw(&term, self.focused, selection, blink, &decor);
            let overlay = if blink { self.renderer.cursor_overlay(&term) } else { None };
            drop(term);
            self.update_cursor_layer(overlay);
        } else {
            // Nothing changed for a few refreshes: stop the display link. The
            // next byte from the shell or key press resumes it.
            self.idle_ticks += 1;
            if self.idle_ticks >= 3 {
                self.link.setPaused(true);
            }
        }
    }

    /// A command finished in tab `i`. A background tab gets a mark in the
    /// tab strip; if it ran a while and the window is not in front, the Dock
    /// icon bounces once.
    fn command_finished(&mut self, i: usize, cmd: u32) {
        let (long, outcome) = {
            let term = self.tabs[i].term.lock();
            let info = term.command(cmd);
            let long = info.and_then(|c| c.elapsed()).is_some_and(|d| d >= Duration::from_secs(10));
            let failed = info.and_then(|c| c.exit).is_some_and(|e| e != 0);
            (long, if failed { Outcome::Failed } else { Outcome::Succeeded })
        };
        if i != self.active {
            self.tabs[i].unseen = Some(outcome);
        }
        self.request_redraw();
        if long && !self.window.isKeyWindow() {
            if let Some(mtm) = MainThreadMarker::new() {
                NSApplication::sharedApplication(mtm)
                    .requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
            }
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
        self.tabs[i].wake_pending.store(false, Ordering::Release);
        let rev = self.tabs[i].term.lock().title_revision;
        if rev != self.tabs[i].title_rev {
            self.refresh_label(i);
        }
    }

    /// Rename tab `i` from its terminal, and the window if it is in front.
    fn refresh_label(&mut self, i: usize) {
        let (label, rev) = {
            let term = self.tabs[i].term.lock();
            (tab_label(&term), term.title_revision)
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

    /// Park the per-tab state of the tab in front in it.
    fn save_live(&mut self) {
        let saved = TabState {
            selection: self.selection.take(),
            preedit: std::mem::take(&mut self.preedit),
            mouse_held: self.mouse_held.take(),
            mouse_cell: self.mouse_cell,
            scroll_accum: self.scroll_accum,
            sync_since: self.sync_since.take(),
        };
        self.tabs[self.active].saved = saved;
    }

    /// Take up the per-tab state of the tab now in front.
    fn load_live(&mut self) {
        let tab = &mut self.tabs[self.active];
        let s = std::mem::take(&mut tab.saved);
        tab.unseen = None;
        self.term = Arc::clone(&tab.term);
        self.pty = Arc::clone(&tab.pty);
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

        self.renderer.set_strip(self.tabs.len() > 1);
        let grid = self.renderer.grid_size_for(w, h);
        if grid != self.grid {
            crate::log!("window {}: grid {}x{} -> {}x{}", self.id, self.grid.0, self.grid.1, grid.0, grid.1);
            self.grid = grid;
            // Every tab, so one brought forward is already the right size.
            for tab in &self.tabs {
                tab.term.lock().resize(grid.0, grid.1);
                tab.pty.resize(grid.0 as u16, grid.1 as u16);
            }
        }
        self.update_resize_increments();
        self.request_redraw();
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
        if !focused {
            self.hover(None);
        }
        self.request_redraw();
    }

    /// Handle a key press. Returns true when it should go to the input
    /// system instead (ordinary typing, dead keys, input methods).
    fn key_down(&mut self, event: &NSEvent) -> bool {
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            // Command shortcuts belong to the menu bar; one that reaches us
            // has no binding.
            return false;
        }
        // Mid-composition, every key belongs to the input method.
        if !self.preedit.is_empty() {
            return true;
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
        NSCursor::setHiddenUntilMouseMoves(true);
        self.selection = None;
        self.cursor_shown = None;
        self.request_redraw();
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
        let (pad_x, pad_y) = self.renderer.padding();
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

        let mode = self.term.lock().mode;
        if mode.intersects(TermMode::ANY_MOUSE) {
            let (_, cell) = self.cell_at(event);
            let button = if lines > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            for _ in 0..lines.unsigned_abs() {
                self.report(button, MouseEventKind::Press, cell, event);
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
            let _ = self.pty.write(&arrow.repeat(lines.unsigned_abs()));
        } else {
            self.term.lock().scroll_display(lines);
            self.request_redraw();
        }
    }

    /// Edge-to-edge: the traffic lights stay hidden until the pointer reaches
    /// the top-left corner.
    fn hover(&mut self, location: Option<NSPoint>) {
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
        let p = self.view.convertPoint_fromView(event.locationInWindow(), None);
        let scale = self.scale() as f32;
        let height = self.view.bounds().size.height as f32;
        let (x, y) = (p.x as f32 * scale, (height - p.y as f32) * scale);
        let m = self.renderer.metrics();
        let (pad_x, pad_y) = self.renderer.padding();
        let fx = (x - pad_x) / m.width;
        let fy = (y - pad_y) / m.height;

        let term = self.term.lock();
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

    /// Bare motion, for applications that asked for every move (mode 1003).
    fn mouse_moved(&mut self, event: &NSEvent) {
        if self.mouse_held.is_some() || !self.term.lock().mode.contains(TermMode::MOUSE_MOTION) {
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
fn command_decor(term: &Term) -> Decor {
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
        decor.gutter.push((y, outcome));
        if mark.has(RowMark::INPUT) {
            let running = info.state == CommandState::Running;
            if let Some(text) = info
                .elapsed()
                .and_then(|e| receipts::describe(e, info.exit, running, info.receipt.as_ref()))
            {
                // The input line, or if the command fills it, the first of its
                // wrapped continuation lines with room at the right.
                let room = |r: usize| g.row(r).trimmed_len() + text.chars().count() + 3 <= term.cols();
                let mut r = y;
                while !room(r) && r + 1 < term.rows() && g.row(r).wrapped {
                    r += 1;
                }
                if room(r) {
                    decor.notes.push((r, text));
                }
            }
        }
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
    fn create_window(&mut self, mtm: MainThreadMarker) -> Result<(Retained<NSWindow>, Retained<TachyonView>, u64), String> {
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
        let bg = cfg.theme.background;
        window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            bg[0] as f64 / 255.0,
            bg[1] as f64 / 255.0,
            bg[2] as f64 / 255.0,
            1.0,
        )));
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
        let renderer = Renderer::new(Rc::clone(&self.gpu), layer, cfg, scale as f32)?;

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

        window.setContentView(Some(&view));
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

        let tab_id = self.next_tab;
        self.next_tab += 1;
        let tab = Tab::start(tab_id, cfg, cfg.window.cols, cfg.window.rows)?;
        let (term, pty) = (Arc::clone(&tab.term), Arc::clone(&tab.pty));

        let link = unsafe { view.displayLinkWithTarget_selector(&view, sel!(tick:)) };
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };

        self.windows.push(TermWindow {
            id,
            window: window.clone(),
            view: view.clone(),
            link,
            renderer,
            tabs: vec![tab],
            active: 0,
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
            focused: true,
            selection: None,
            preedit: String::new(),
            mouse_held: None,
            mouse_cell: (usize::MAX, usize::MAX),
            option_as_meta: cfg.macos.option_as_meta,
            base_font_size: cfg.font.size,
            cursor_layer,
            cursor_shown: None,
            _delegate: delegate,
        });
        Ok((window, view, id))
    }

    /// Start a shell in a new tab of window `window`.
    fn new_tab(&mut self, window: u64) -> Result<(), String> {
        let tab_id = self.next_tab;
        self.next_tab += 1;
        let Some(w) = self.windows.iter_mut().find(|w| w.id == window) else {
            return Ok(());
        };
        let (cols, rows) = w.grid;
        let tab = Tab::start(tab_id, &self.cfg, cols.max(1), rows.max(1))?;
        w.add_tab(tab);
        Ok(())
    }
}

/// ⌘T: a new tab in this window.
fn open_tab(window: u64) {
    if let Some(Err(e)) = with_app(|app| app.new_tab(window)) {
        crate::log::force(&format!("could not open a tab: {e}"));
    }
}

/// ⌘W: close the tab in front, or the window with its last tab.
fn close_active_tab(window: u64) {
    let last = with_app(|app| {
        let w = app.windows.iter_mut().find(|w| w.id == window)?;
        if w.tabs.len() == 1 {
            return Some(w.window.clone());
        }
        let i = w.active;
        w.remove_tab(i);
        None
    })
    .flatten();
    // Outside the borrow: closing calls back into the window delegate.
    if let Some(window) = last {
        window.performClose(None);
    }
}

/// Open a new terminal window and bring it to the front.
fn open_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    match with_app(|app| app.create_window(mtm)) {
        Some(Ok((window, view, id))) => {
            with_window(id, TermWindow::layout);
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

/// A tab's shell exited: close the tab, or its window if it was the last
/// one, the way Terminal.app does by default.
fn shell_exited(tab: u64) {
    let window = with_app(|app| {
        let w = app.windows.iter_mut().find(|w| w.tabs.iter().any(|t| t.id == tab))?;
        if w.tabs.len() == 1 {
            return Some(w.window.clone());
        }
        let i = w.tabs.iter().position(|t| t.id == tab)?;
        w.remove_tab(i);
        None
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
    submenu(&main, &app_menu);

    let file = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Shell"));
    item(&file, "New Tab", sel!(newTab:), "t");
    item(&file, "New Window", sel!(newWindow:), "n");
    file.addItem(&NSMenuItem::separatorItem(mtm));
    item(&file, "Close Tab", sel!(tachyonCloseTab:), "w");
    // A capital letter means Shift as well: ⌘⇧W.
    item(&file, "Close Window", sel!(performClose:), "W");
    submenu(&main, &file);

    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Edit"));
    item(&edit, "Copy", sel!(copy:), "c");
    item(&edit, "Paste", sel!(paste:), "v");
    item(&edit, "Select All", sel!(selectAll:), "a");
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
    submenu(&main, &view);

    let window = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Window"));
    item(&window, "Minimize", sel!(performMiniaturize:), "m");
    item(&window, "Zoom", sel!(performZoom:), "");
    window.addItem(&NSMenuItem::separatorItem(mtm));
    // "{" and "}" are ⌘⇧[ and ⌘⇧], as in Safari and Terminal.
    item(&window, "Show Previous Tab", sel!(tachyonPreviousTab:), "{");
    item(&window, "Show Next Tab", sel!(tachyonNextTab:), "}");
    submenu(&main, &window);
    app.setWindowsMenu(Some(&window));

    app.setMainMenu(Some(&main));
}

/// Start a shell on a PTY and the reader thread that feeds its terminal.
fn start_session(
    id: u64,
    cfg: &Config,
    cols: usize,
    rows: usize,
    wake_pending: Arc<AtomicBool>,
) -> Result<(Arc<Mutex<Term>>, Arc<Pty>), String> {
    let (program, login) = if cfg.shell.program.is_empty() {
        (pty::default_shell(), true)
    } else {
        (cfg.shell.program.clone(), false)
    };
    let extra_env = match integration_dir() {
        Some(dir) if cfg.shell.integration => pty::zsh_integration_env(&program, &dir),
        _ => Vec::new(),
    };
    crate::log!("tab {id}: spawning `{program}` {:?} at {cols}x{rows}, integration: {}", cfg.shell.args, !extra_env.is_empty());
    let pty = Arc::new(
        Pty::spawn(cols as u16, rows as u16, &program, &cfg.shell.args, login, &extra_env)
            .map_err(|e| format!("could not start {program}: {e}"))?,
    );
    let term = Arc::new(Mutex::new(Term::new(cols, rows, cfg.scrollback)));

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
            // 128 KiB amortises the syscall over a burst without holding the
            // terminal lock for long.
            let mut buf = vec![0u8; 128 * 1024];
            loop {
                let n = match reader_pty.read(&mut buf) {
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
                let (replies, events) = {
                    let mut t = reader_term.lock();
                    processor.advance(&mut t, &buf[..n]);
                    (t.take_replies(), t.take_command_events())
                };
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
                            let start = cpu_at_start.remove(&cmd).flatten();
                            let cpu = start
                                .zip(receipts::children_cpu(shell))
                                .map(|(a, b)| b.saturating_sub(a));
                            let receipt = sampler.finish(cpu);
                            reader_term.lock().set_receipt(cmd, receipt);
                            DispatchQueue::main().exec_async(move || with_tab(id, |w, i| w.command_finished(i, cmd)));
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

    Ok((term, pty))
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
    unsafe {
        NSUserDefaults::standardUserDefaults().setObject_forKey(
            Some(&NSString::from_str("None")),
            &NSString::from_str("AppleActionOnDoubleClick"),
        );
    }
    build_menu(mtm, &app);

    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    let gpu = Gpu::new()?;
    APP.with(|cell| {
        *cell.borrow_mut() = Some(App {
            cfg,
            gpu,
            windows: Vec::new(),
            next_id: 1,
            next_tab: 1,
            cascade: NSPoint::new(0.0, 0.0),
            _delegate: delegate,
        });
    });

    open_window();
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
