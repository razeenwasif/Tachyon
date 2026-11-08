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
mod renderer;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSColor, NSCursor, NSEvent, NSEventModifierFlags, NSImage, NSMenu, NSMenuItem, NSPasteboard,
    NSPasteboardTypeString, NSResponder, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSWindow, NSWindowButton, NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSData, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString,
};
use objc2_quartz_core::{CADisplayLink, CALayer, CAMetalLayer};
use parking_lot::Mutex;

use crate::config::Config;
use crate::term::{Processor, Term, TermMode};
use pty::Pty;
use renderer::{Gpu, Renderer};

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

        // Lets a drag in the (invisible) title bar strip move the window.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            true
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
            with_window(self.id(), |w| w.key_down(event));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            with_window(self.id(), |w| w.scroll(event));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = event.locationInWindow();
            with_window(self.id(), |w| w.hover(Some(p)));
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
);

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
    /// Where the next window's top-left corner goes, so new windows cascade.
    cascade: NSPoint,
    _delegate: Retained<AppDelegate>,
}

/// One window: its AppKit objects, renderer, and shell session.
struct TermWindow {
    id: u64,
    window: Retained<NSWindow>,
    view: Retained<TachyonView>,
    link: Retained<CADisplayLink>,
    renderer: Renderer,

    term: Arc<Mutex<Term>>,
    pty: Arc<Pty>,
    /// Set by the reader thread when it has asked the main thread to wake;
    /// cleared at the start of each tick. Keeps the main queue from filling
    /// with redundant wake-ups during a flood of output.
    wake_pending: Arc<AtomicBool>,

    needs_redraw: bool,
    idle_ticks: u32,
    sync_since: Option<Instant>,
    sync_timeout: Duration,
    grid: (usize, usize),
    scroll_accum: f64,
    title_rev: u64,
    buttons_visible: bool,
    focused: bool,
    _delegate: Retained<WindowDelegate>,
}

impl Drop for TermWindow {
    fn drop(&mut self) {
        // A display link retains its target; invalidate it or the view leaks.
        self.link.invalidate();
        // The reader thread holds the PTY too, so dropping ours does not end
        // the session; hanging up does.
        self.pty.hangup();
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
        self.wake_pending.store(false, Ordering::Release);

        let (dirty, syncing, title_rev) = {
            let mut t = self.term.lock();
            (t.take_dirty(), t.mode.contains(TermMode::SYNC_UPDATE), t.title_revision)
        };
        if dirty {
            self.needs_redraw = true;
        }
        if title_rev != self.title_rev {
            self.title_rev = title_rev;
            let title = self.term.lock().title.clone().unwrap_or_default();
            let title = if title.is_empty() { "Tachyon".to_string() } else { title };
            // Hidden in the title bar, but it names the window in the Window
            // menu, Mission Control and the ⌘` cycle.
            self.window.setTitle(&NSString::from_str(&title));
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
            self.renderer.draw(&term, self.focused);
        } else {
            // Nothing changed for a few refreshes: stop the display link. The
            // next byte from the shell or key press resumes it.
            self.idle_ticks += 1;
            if self.idle_ticks >= 3 {
                self.link.setPaused(true);
            }
        }
    }

    fn request_redraw(&mut self) {
        self.needs_redraw = true;
        self.resume();
    }

    /// The view changed size: resize the drawable, and the grid if a whole
    /// cell more or fewer now fits.
    fn layout(&mut self) {
        let scale = self.scale();
        let bounds = self.view.bounds();
        let (w, h) = ((bounds.size.width * scale) as f32, (bounds.size.height * scale) as f32);
        self.renderer.resize(w, h);

        let grid = self.renderer.grid_size_for(w, h);
        if grid != self.grid {
            self.grid = grid;
            self.term.lock().resize(grid.0, grid.1);
            self.pty.resize(grid.0 as u16, grid.1 as u16);
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

    fn key_down(&mut self, event: &NSEvent) {
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            // Command shortcuts belong to the menu bar; one that reaches us
            // has no binding.
            return;
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
        let mode = self.term.lock().mode;
        if let Some(bytes) = input::encode(event.keyCode(), &chars, &bare, mods, mode, false) {
            self.term.lock().scroll_to_bottom();
            let _ = self.pty.write(&bytes);
            NSCursor::setHiddenUntilMouseMoves(true);
        }
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
            set_buttons_hidden(&self.window, !show);
        }
    }
}

fn set_buttons_hidden(window: &NSWindow, hidden: bool) {
    for b in [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ] {
        if let Some(button) = window.standardWindowButton(b) {
            button.setHidden(hidden);
        }
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
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), content),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
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
        set_buttons_hidden(&window, true);

        // The first window is centred; later ones cascade down and right.
        if self.windows.is_empty() {
            window.center();
            self.cascade = window.cascadeTopLeftFromPoint(NSPoint::new(0.0, 0.0));
        } else {
            self.cascade = window.cascadeTopLeftFromPoint(self.cascade);
        }

        let wake_pending = Arc::new(AtomicBool::new(false));
        let (term, pty) = start_session(id, cfg, cfg.window.cols, cfg.window.rows, Arc::clone(&wake_pending))?;

        let link = unsafe { view.displayLinkWithTarget_selector(&view, sel!(tick:)) };
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };

        self.windows.push(TermWindow {
            id,
            window: window.clone(),
            view: view.clone(),
            link,
            renderer,
            term,
            pty,
            wake_pending,
            needs_redraw: true,
            idle_ticks: 0,
            sync_since: None,
            sync_timeout: Duration::from_millis(cfg.render.sync_timeout_ms),
            grid: (0, 0),
            scroll_accum: 0.0,
            title_rev: u64::MAX,
            buttons_visible: false,
            focused: true,
            _delegate: delegate,
        });
        Ok((window, view, id))
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

/// Close a window whose shell exited, the way Terminal.app does by default.
fn close_window(id: u64) {
    let window = APP.with(|cell| {
        let guard = cell.try_borrow().ok()?;
        guard.as_ref()?.windows.iter().find(|w| w.id == id).map(|w| w.window.clone())
    });
    // `close` sends windowWillClose:, which removes the session.
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
    item(&file, "New Window", sel!(newWindow:), "n");
    item(&file, "Close Window", sel!(performClose:), "w");
    submenu(&main, &file);

    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Edit"));
    item(&edit, "Paste", sel!(paste:), "v");
    submenu(&main, &edit);

    let window = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Window"));
    item(&window, "Minimize", sel!(performMiniaturize:), "m");
    item(&window, "Zoom", sel!(performZoom:), "");
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
    crate::log!("window {id}: spawning `{program}` {:?} at {cols}x{rows}", cfg.shell.args);
    let pty = Arc::new(
        Pty::spawn(cols as u16, rows as u16, &program, &cfg.shell.args, login)
            .map_err(|e| format!("could not start {program}: {e}"))?,
    );
    let term = Arc::new(Mutex::new(Term::new(cols, rows, cfg.scrollback)));

    let reader_term = Arc::clone(&term);
    let reader_pty = Arc::clone(&pty);
    std::thread::Builder::new()
        .name(format!("tachyon-pty-reader-{id}"))
        .spawn(move || {
            let mut processor = Processor::new();
            let mut dump = std::env::var_os("TACHYON_DUMP").and_then(|p| std::fs::File::create(p).ok());
            // 128 KiB amortises the syscall over a burst without holding the
            // terminal lock for long.
            let mut buf = vec![0u8; 128 * 1024];
            loop {
                let n = match reader_pty.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        crate::log!("window {id}: pty read failed: {e}");
                        break;
                    }
                };
                if let Some(f) = dump.as_mut() {
                    use std::io::Write;
                    let _ = f.write_all(&buf[..n]);
                }
                let replies = {
                    let mut t = reader_term.lock();
                    processor.advance(&mut t, &buf[..n]);
                    t.take_replies()
                };
                if !replies.is_empty() {
                    let _ = reader_pty.write(&replies);
                }
                if !wake_pending.swap(true, Ordering::AcqRel) {
                    DispatchQueue::main().exec_async(move || with_window(id, TermWindow::resume));
                }
            }
            DispatchQueue::main().exec_async(move || close_window(id));
        })
        .map_err(|e| format!("could not start the reader thread: {e}"))?;

    Ok((term, pty))
}

pub fn run(cfg: Config) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("Tachyon must start on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(ICON_SVG)) {
        unsafe { app.setApplicationIconImage(Some(&icon)) };
    }
    // Native window tabs would merge our windows behind a tab bar we hide;
    // Tachyon's own tabs come later.
    NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
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
