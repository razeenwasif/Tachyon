//! Native macOS Liquid Glass Settings window (`⌘,`).
//!
//! Sidebar-navigated configuration window covering:
//! 0. Server        — local Mux v2 daemon status, start/restart & launchd service
//! 1. Remote Hosts  — live host list, reconnect/remove, ~/.ssh/config discovery & add
//! 2. Integrations  — Claude Code / Antigravity / Codex / OpenCode / Pi skills, hooks & auto-naming
//! 3. Interface     — sidebar, pane headers, title bar, shortcut hints & persistence
//! 4. Notifications — OSC 7501 coding-agent notifications & test alert

use std::cell::{Cell, RefCell};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBackingStoreType, NSBezelStyle, NSBezierPath, NSButton,
    NSColor, NSCompositingOperation, NSControlStateValueOff, NSControlStateValueOn, NSEvent,
    NSFont, NSFontAttributeName, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSGlassEffectView, NSImage, NSImageSymbolConfiguration,
    NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSPopUpButton,
    NSResponder, NSSwitch, NSTextField, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow,
    NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSObject, NSPoint, NSRect, NSSize,
    NSString,
};

use super::host_mgr::HostStatus;
use super::machines;
use crate::config::{Config, PaneHeaders};
use crate::integrations;
use crate::mux::default_socket_path;
use crate::server;

const WIN_W: f64 = 690.0;
const WIN_H: f64 = 524.0;
const RAIL_W: f64 = 186.0;
const NAV_ROW_H: f64 = 32.0;

struct NavSection {
    title: &'static str,
    symbol: &'static str,
}

const SECTIONS: [NavSection; 5] = [
    NavSection { title: "Server", symbol: "server.rack" },
    NavSection { title: "Remote Hosts", symbol: "globe" },
    NavSection { title: "Integrations", symbol: "sparkles" },
    NavSection { title: "Interface", symbol: "macwindow" },
    NavSection { title: "Notifications", symbol: "bell.badge" },
];

thread_local! {
    static SETTINGS: RefCell<Option<SettingsWindow>> = const { RefCell::new(None) };
}

/// Open (or bring to front) the Settings window, optionally selecting `section` (0..5).
pub fn open(mtm: MainThreadMarker, section: Option<usize>) {
    SETTINGS.with(|slot| {
        let mut guard = slot.borrow_mut();
        if guard.is_none() {
            *guard = Some(SettingsWindow::new(mtm));
        }
        if let Some(win) = guard.as_mut() {
            if let Some(idx) = section {
                win.active.set(idx.min(SECTIONS.len() - 1));
            }
            win.rebuild_detail(mtm);
            win.nav.set_active(win.active.get());
            win.window.makeKeyAndOrderFront(None);
        }
    });
}

/// Refresh the Settings window if it is currently visible (e.g. after host status changes).
pub fn refresh_if_open() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    SETTINGS.with(|slot| {
        if let Ok(mut guard) = slot.try_borrow_mut() {
            if let Some(win) = guard.as_mut() {
                if win.window.isVisible() {
                    win.rebuild_detail(mtm);
                }
            }
        }
    });
}

#[derive(Default)]
struct NavState {
    active: Cell<usize>,
    hover: Cell<Option<usize>>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSettingsNavView"]
    #[ivars = NavState]
    struct SettingsNavView;

    impl SettingsNavView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let w = b.size.width;
            NSColor::blackColor().colorWithAlphaComponent(0.14).setFill();
            NSBezierPath::fillRect(b);
            NSColor::separatorColor().colorWithAlphaComponent(0.35).setFill();
            NSBezierPath::fillRect(NSRect::new(NSPoint::new(w - 1.0, 0.0), NSSize::new(1.0, b.size.height)));

            draw_text(
                "Tachyon Settings",
                12.0,
                Weight::Semibold,
                &NSColor::secondaryLabelColor(),
                NSRect::new(NSPoint::new(16.0, 44.0), NSSize::new(w - 28.0, 18.0)),
            );

            let active = self.ivars().active.get();
            let hover = self.ivars().hover.get();
            for (i, sec) in SECTIONS.iter().enumerate() {
                let y = 72.0 + i as f64 * (NAV_ROW_H + 3.0);
                let pill = NSRect::new(NSPoint::new(10.0, y), NSSize::new(w - 20.0, NAV_ROW_H));
                if i == active {
                    NSColor::controlAccentColor().colorWithAlphaComponent(0.24).setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, 8.0, 8.0).fill();
                } else if hover == Some(i) {
                    NSColor::labelColor().colorWithAlphaComponent(0.08).setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, 8.0, 8.0).fill();
                }

                let icon_col = if i == active {
                    NSColor::controlAccentColor()
                } else {
                    NSColor::secondaryLabelColor()
                };
                draw_symbol(
                    sec.symbol,
                    NSRect::new(NSPoint::new(19.0, y + 8.0), NSSize::new(16.0, 16.0)),
                    &icon_col,
                );
                let fg = if i == active {
                    NSColor::labelColor()
                } else {
                    NSColor::labelColor().colorWithAlphaComponent(0.80)
                };
                draw_text(
                    sec.title,
                    12.5,
                    if i == active { Weight::Medium } else { Weight::Regular },
                    &fg,
                    NSRect::new(NSPoint::new(42.0, y + 8.0), NSSize::new(w - 54.0, 16.0)),
                );
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let hit = hit_section(p.y);
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
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            if let Some(idx) = hit_section(p.y) {
                let Some(mtm) = MainThreadMarker::new() else { return };
                select_section(mtm, idx);
            }
        }
    }
);

impl SettingsNavView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let view: Retained<Self> =
            unsafe { msg_send![super(Self::alloc(mtm).set_ivars(NavState::default())), initWithFrame: NSRect::ZERO] };
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

    fn set_active(&self, idx: usize) {
        self.ivars().active.set(idx);
        self.setNeedsDisplay(true);
    }
}

fn hit_section(y: f64) -> Option<usize> {
    if y < 72.0 {
        return None;
    }
    let idx = ((y - 72.0) / (NAV_ROW_H + 3.0)).floor() as usize;
    (idx < SECTIONS.len()).then_some(idx)
}

fn select_section(mtm: MainThreadMarker, idx: usize) {
    SETTINGS.with(|slot| {
        if let Some(win) = slot.borrow_mut().as_mut() {
            win.active.set(idx);
            win.nav.set_active(idx);
            win.rebuild_detail(mtm);
        }
    });
}

define_class!(
    /// Flipped panel view used for the right-hand detail content so layout flows
    /// top-to-bottom cleanly.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSettingsDetailView"]
    struct SettingsDetailView;

    impl SettingsDetailView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }
    }
);

impl SettingsDetailView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(())), initWithFrame: frame] }
    }
}

define_class!(
    /// Subtle translucent card container inside a settings section.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSettingsCardView"]
    struct SettingsCardView;

    impl SettingsCardView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b, 10.0, 10.0);
            NSColor::labelColor().colorWithAlphaComponent(0.055).setFill();
            path.fill();
            NSColor::separatorColor().colorWithAlphaComponent(0.28).setStroke();
            path.setLineWidth(1.0);
            path.stroke();
        }
    }
);

impl SettingsCardView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(())), initWithFrame: frame] }
    }
}

#[derive(Default)]
struct ActionState {
    /// Dynamic aliases referenced by tag for host buttons.
    host_aliases: RefCell<Vec<String>>,
    ssh_candidates: RefCell<Vec<String>>,
    status_note: RefCell<Option<String>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonSettingsActions"]
    #[ivars = ActionState]
    struct SettingsActions;

    impl SettingsActions {
        #[unsafe(method(serverStartOrRestart:))]
        fn server_start_or_restart(&self, _sender: Option<&AnyObject>) {
            self.set_note("Starting / refreshing local server…");
            std::thread::spawn(|| {
                let _ = crate::mux::transport::spawn_local_daemon(&default_socket_path());
                DispatchQueue::main().exec_async(|| {
                    super::mux_reconnect_host("local");
                    refresh_if_open();
                });
            });
        }

        #[unsafe(method(serverInstallService:))]
        fn server_install_service(&self, _sender: Option<&AnyObject>) {
            match server::install_service(&default_socket_path()) {
                Ok(path) => self.set_note(&format!("Installed LaunchAgent -> {}", path.display())),
                Err(e) => self.set_note(&format!("Service install failed: {e}")),
            }
            refresh_if_open();
        }

        #[unsafe(method(serverUninstallService:))]
        fn server_uninstall_service(&self, _sender: Option<&AnyObject>) {
            match server::uninstall_service() {
                Ok(()) => self.set_note("Removed LaunchAgent service."),
                Err(e) => self.set_note(&format!("Service uninstall failed: {e}")),
            }
            refresh_if_open();
        }

        #[unsafe(method(hostReconnect:))]
        fn host_reconnect(&self, sender: Option<&AnyObject>) {
            if let Some(alias) = tagged_string(sender, &self.ivars().host_aliases.borrow()) {
                super::mux_reconnect_host(&alias);
                self.set_note(&format!("Reconnecting `{alias}`…"));
                refresh_if_open();
            }
        }

        #[unsafe(method(hostRemove:))]
        fn host_remove(&self, sender: Option<&AnyObject>) {
            if let Some(alias) = tagged_string(sender, &self.ivars().host_aliases.borrow()) {
                super::mux_remove_host(&alias);
                self.set_note(&format!("Removed remote host `{alias}`"));
                refresh_if_open();
            }
        }

        #[unsafe(method(hostAddSsh:))]
        fn host_add_ssh(&self, sender: Option<&AnyObject>) {
            if let Some(name) = tagged_string(sender, &self.ivars().ssh_candidates.borrow()) {
                super::mux_add_host(name.clone(), format!("ssh://{name}"));
                self.set_note(&format!("Added `ssh://{name}` — connecting…"));
                refresh_if_open();
            }
        }

        #[unsafe(method(hostAddCustom:))]
        fn host_add_custom(&self, _sender: Option<&AnyObject>) {
            let (alias, uri, token) = SETTINGS.with(|slot| {
                let guard = slot.borrow();
                let Some(win) = guard.as_ref() else {
                    return (String::new(), String::new(), None);
                };
                let a = win
                    .custom_alias
                    .as_ref()
                    .map(|f| f.stringValue().to_string().trim().to_string())
                    .unwrap_or_default();
                let u = win
                    .custom_uri
                    .as_ref()
                    .map(|f| f.stringValue().to_string().trim().to_string())
                    .unwrap_or_default();
                let t = win
                    .custom_token
                    .as_ref()
                    .map(|f| f.stringValue().to_string().trim().to_string())
                    .filter(|s| !s.is_empty());
                (a, u, t)
            });
            if alias.is_empty() || uri.is_empty() {
                self.set_note("Enter both an alias (e.g. win-pc) and an ssh:// or tcp:// URI.");
                refresh_if_open();
                return;
            }
            let normalized = if uri.contains("://") {
                uri
            } else {
                format!("ssh://{uri}")
            };
            super::mux_add_host_with_token(alias.clone(), normalized, token);
            self.set_note(&format!("Added `{alias}` — connecting…"));
            refresh_if_open();
        }

        #[unsafe(method(toggleIntegrateAgents:))]
        fn toggle_integrate_agents(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let home = integrations::user_home();
            let auto_name = Config::load_default().agents.auto_name_sessions;
            let res = if on {
                integrations::setup_all_detected(&home, auto_name)
                    .map(|_| "Installed Tachyon skills & OSC 7501 hooks for detected agents.")
            } else {
                integrations::remove_all(&home)
                    .map(|_| "Removed Tachyon agent skills & hooks (your settings preserved).")
            };
            match res {
                Ok(msg) => self.set_note(msg),
                Err(e) => self.set_note(&format!("Integration error: {e}")),
            }
            refresh_if_open();
        }

        #[unsafe(method(toggleAutoName:))]
        fn toggle_auto_name(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("agents.auto_name_sessions", if on { "true" } else { "false" });
            super::apply_settings_config(move |cfg| cfg.agents.auto_name_sessions = on);
            // Keep installed SKILL.md files aligned with the setting.
            let home = integrations::user_home();
            if integrations::any_installed(&home) {
                let _ = integrations::setup_all_detected(&home, on);
            }
            self.set_note(if on {
                "Agents will label new sessions after the task."
            } else {
                "Automatic session naming turned off."
            });
            refresh_if_open();
        }

        #[unsafe(method(toggleSidebarSetting:))]
        fn toggle_sidebar_setting(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("window.sidebar", if on { "true" } else { "false" });
            super::apply_interface_sidebar(on);
            self.set_note("Saved `window.sidebar` to tachyon.conf.");
            refresh_if_open();
        }

        #[unsafe(method(changePaneHeaders:))]
        fn change_pane_headers(&self, sender: Option<&AnyObject>) {
            let idx = sender
                .and_then(|s| s.downcast_ref::<NSPopUpButton>())
                .map(|p| p.indexOfSelectedItem())
                .unwrap_or(0);
            let (mode, val) = match idx {
                1 => (PaneHeaders::Auto, "auto"),
                2 => (PaneHeaders::Never, "never"),
                _ => (PaneHeaders::Always, "always"),
            };
            let _ = Config::update_default_key("window.pane_headers", val);
            super::apply_interface_pane_headers(mode);
            self.set_note(&format!("Saved `window.pane_headers = {val}`."));
            refresh_if_open();
        }

        #[unsafe(method(toggleTitleBarSetting:))]
        fn toggle_title_bar_setting(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let val = if on { "standard" } else { "edge" };
            let _ = Config::update_default_key("window.layout", val);
            super::apply_interface_title_bar(on);
            self.set_note(&format!("Saved `window.layout = {val}`."));
            refresh_if_open();
        }

        #[unsafe(method(toggleHintsSetting:))]
        fn toggle_hints_setting(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("window.hints", if on { "true" } else { "false" });
            super::apply_interface_hints(on);
            self.set_note("Saved `window.hints` to tachyon.conf.");
            refresh_if_open();
        }

        #[unsafe(method(togglePersistSetting:))]
        fn toggle_persist_setting(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("shell.persist", if on { "true" } else { "false" });
            super::apply_settings_config(move |cfg| cfg.shell.persist = on);
            self.set_note("Saved `shell.persist` (applies to new shells).");
            refresh_if_open();
        }

        #[unsafe(method(toggleAgentNotify:))]
        fn toggle_agent_notify(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("notify.agent_status", if on { "true" } else { "false" });
            super::apply_settings_config(move |cfg| cfg.macos.notify_agent_status = on);
            self.set_note("Saved `notify.agent_status` to tachyon.conf.");
            refresh_if_open();
        }

        #[unsafe(method(sendTestNotification:))]
        fn send_test_notification(&self, _sender: Option<&AnyObject>) {
            super::send_test_agent_notification();
            self.set_note("Sent test notification (macOS banners require /Applications/Tachyon.app).");
            refresh_if_open();
        }
    }
);

impl SettingsActions {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(ActionState::default())), init] }
    }

    fn set_note(&self, text: &str) {
        *self.ivars().status_note.borrow_mut() = Some(text.to_string());
    }
}

fn switch_is_on(sender: Option<&AnyObject>) -> bool {
    sender
        .and_then(|s| s.downcast_ref::<NSSwitch>())
        .is_some_and(|sw| sw.state() == NSControlStateValueOn)
}

fn tagged_string(sender: Option<&AnyObject>, list: &[String]) -> Option<String> {
    let btn = sender?.downcast_ref::<NSButton>()?;
    let idx = btn.tag();
    if idx < 0 {
        return None;
    }
    list.get(idx as usize).cloned()
}

struct SettingsWindow {
    window: Retained<NSWindow>,
    nav: Retained<SettingsNavView>,
    detail: Retained<SettingsDetailView>,
    actions: Retained<SettingsActions>,
    active: Cell<usize>,
    custom_alias: Option<Retained<NSTextField>>,
    custom_uri: Option<Retained<NSTextField>>,
    custom_token: Option<Retained<NSTextField>>,
}

impl SettingsWindow {
    fn new(mtm: MainThreadMarker) -> Self {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::FullSizeContentView;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, WIN_H));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Tachyon Settings"));
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.center();

        let container = NSView::initWithFrame(NSView::alloc(mtm), frame);
        let nav = SettingsNavView::new(mtm);
        nav.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(RAIL_W, WIN_H)));
        let detail = SettingsDetailView::new(
            mtm,
            NSRect::new(NSPoint::new(RAIL_W, 0.0), NSSize::new(WIN_W - RAIL_W, WIN_H)),
        );
        container.addSubview(&nav);
        container.addSubview(&detail);

        if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some() {
            let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), frame);
            glass.setCornerRadius(16.0);
            glass.setTintColor(Some(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.84)));
            glass.setContentView(Some(&container));
            window.setContentView(Some(&glass));
        } else {
            let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
            blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
            blur.setState(NSVisualEffectState::Active);
            blur.addSubview(&container);
            window.setContentView(Some(&blur));
        }

        let actions = SettingsActions::new(mtm);
        let mut this = Self {
            window,
            nav,
            detail,
            actions,
            active: Cell::new(2), // Open directly to Integrations by default unless specified.
            custom_alias: None,
            custom_uri: None,
            custom_token: None,
        };
        this.rebuild_detail(mtm);
        this
    }

    fn rebuild_detail(&mut self, mtm: MainThreadMarker) {
        // Clear previous section subviews.
        let subs: Vec<Retained<NSView>> = self.detail.subviews().iter().collect();
        for v in subs {
            v.removeFromSuperview();
        }
        self.custom_alias = None;
        self.custom_uri = None;
        self.custom_token = None;

        let w = WIN_W - RAIL_W;
        let pad = 22.0;
        let inner_w = w - 2.0 * pad;
        let sec = self.active.get();

        // Section heading.
        let heading = label(mtm, SECTIONS[sec].title, 18.0, true, &NSColor::labelColor());
        heading.setFrame(NSRect::new(NSPoint::new(pad, 24.0), NSSize::new(inner_w, 24.0)));
        self.detail.addSubview(&heading);

        let mut y = 62.0;
        match sec {
            0 => self.build_server(mtm, pad, inner_w, &mut y),
            1 => self.build_remote_hosts(mtm, pad, inner_w, &mut y),
            2 => self.build_integrations(mtm, pad, inner_w, &mut y),
            3 => self.build_interface(mtm, pad, inner_w, &mut y),
            _ => self.build_notifications(mtm, pad, inner_w, &mut y),
        }

        if let Some(note) = self.actions.ivars().status_note.borrow().as_deref() {
            let n = label(mtm, note, 11.5, false, &NSColor::secondaryLabelColor());
            n.setFrame(NSRect::new(
                NSPoint::new(pad, (WIN_H - 34.0).max(y + 8.0)),
                NSSize::new(inner_w, 18.0),
            ));
            self.detail.addSubview(&n);
        }
    }

    fn build_server(&self, mtm: MainThreadMarker, pad: f64, w: f64, y: &mut f64) {
        let target: &AnyObject = self.actions.as_ref();
        let hosts = super::mux_host_views();
        let local = hosts.iter().find(|h| h.is_local);

        let (state_text, detail_text) = match local.map(|h| &h.status) {
            Some(HostStatus::Connected { info, .. }) => {
                let (s, b) = local
                    .and_then(|h| h.snapshot.as_ref())
                    .map(|sn| (sn.sessions.len(), sn.blocks.len()))
                    .unwrap_or((0, 0));
                (
                    format!("🟢 Running · PID {} · Mux v{}", info.pid, info.protocol),
                    format!("{} sessions, {} blocks · {}", s, b, default_socket_path().display()),
                )
            }
            Some(HostStatus::Connecting) => (
                "🟡 Starting local server…".into(),
                default_socket_path().display().to_string(),
            ),
            _ => (
                "⚪️ Stopped".into(),
                format!("Socket: {}", default_socket_path().display()),
            ),
        };

        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 116.0)));
        let l1 = label(mtm, "Local Multiplexer Daemon", 13.0, true, &NSColor::labelColor());
        l1.setFrame(NSRect::new(NSPoint::new(14.0, 12.0), NSSize::new(w - 28.0, 18.0)));
        let l2 = label(mtm, &state_text, 12.5, false, &NSColor::labelColor());
        l2.setFrame(NSRect::new(NSPoint::new(14.0, 34.0), NSSize::new(w - 28.0, 18.0)));
        let l3 = label(mtm, &detail_text, 11.0, false, &NSColor::secondaryLabelColor());
        l3.setFrame(NSRect::new(NSPoint::new(14.0, 54.0), NSSize::new(w - 28.0, 16.0)));

        let start_btn = push_button(mtm, "Start / Reconnect", target, sel!(serverStartOrRestart:));
        start_btn.setFrame(NSRect::new(NSPoint::new(14.0, 78.0), NSSize::new(140.0, 26.0)));
        let inst_btn = push_button(mtm, "Install LaunchAgent", target, sel!(serverInstallService:));
        inst_btn.setFrame(NSRect::new(NSPoint::new(162.0, 78.0), NSSize::new(146.0, 26.0)));
        let uninst_btn = push_button(mtm, "Uninstall Service", target, sel!(serverUninstallService:));
        uninst_btn.setFrame(NSRect::new(NSPoint::new(316.0, 78.0), NSSize::new(130.0, 26.0)));

        for v in [
            &*l1 as &NSView,
            &*l2 as &NSView,
            &*l3 as &NSView,
            &*start_btn as &NSView,
            &*inst_btn as &NSView,
            &*uninst_btn as &NSView,
        ] {
            card.addSubview(v);
        }
        self.detail.addSubview(&card);
        *y += 130.0;

        let tip = label(
            mtm,
            "CLI linked at ~/.local/bin/tachyon · Shells keep running across app quits and restarts.",
            11.5,
            false,
            &NSColor::secondaryLabelColor(),
        );
        tip.setFrame(NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 18.0)));
        self.detail.addSubview(&tip);
        *y += 28.0;
    }

    fn build_remote_hosts(&mut self, mtm: MainThreadMarker, pad: f64, w: f64, y: &mut f64) {
        let target: &AnyObject = self.actions.as_ref();
        let hosts = super::mux_host_views();
        let remotes: Vec<_> = hosts.into_iter().filter(|h| !h.is_local).collect();

        let mut aliases = Vec::new();
        let rows_h = (remotes.len().max(1) as f64) * 34.0 + 36.0;
        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, rows_h)));
        let hdr = label(mtm, "Configured Remote Hosts", 13.0, true, &NSColor::labelColor());
        hdr.setFrame(NSRect::new(NSPoint::new(14.0, 10.0), NSSize::new(w - 28.0, 18.0)));
        card.addSubview(&hdr);

        if remotes.is_empty() {
            let empty = label(
                mtm,
                "No remote hosts saved yet. Add one below or from ~/.ssh/config.",
                12.0,
                false,
                &NSColor::secondaryLabelColor(),
            );
            empty.setFrame(NSRect::new(NSPoint::new(14.0, 36.0), NSSize::new(w - 28.0, 18.0)));
            card.addSubview(&empty);
        } else {
            for (idx, h) in remotes.iter().enumerate() {
                aliases.push(h.alias.clone());
                let ry = 34.0 + idx as f64 * 34.0;
                let line = match &h.status {
                    HostStatus::Connected { info, rtt_ms: Some(ms) } => {
                        format!("🟢 {} · {}/{} ({ms:.0} ms) · {}", h.display_name, info.os, info.arch, h.uri)
                    }
                    HostStatus::Connected { info, .. } => {
                        format!("🟢 {} · {}/{} · {}", h.display_name, info.os, info.arch, h.uri)
                    }
                    HostStatus::Connecting => format!("🟡 {} (connecting…)", h.display_name),
                    HostStatus::Disconnected { .. } => format!("⚪️ {} (offline) · {}", h.display_name, h.uri),
                };
                let lbl = label(mtm, &line, 12.0, false, &NSColor::labelColor());
                lbl.setFrame(NSRect::new(NSPoint::new(14.0, ry + 5.0), NSSize::new(w - 190.0, 18.0)));
                let rec = push_button(mtm, "Reconnect", target, sel!(hostReconnect:));
                rec.setTag(idx as isize);
                rec.setFrame(NSRect::new(NSPoint::new(w - 166.0, ry + 1.0), NSSize::new(82.0, 24.0)));
                let rem = push_button(mtm, "Remove", target, sel!(hostRemove:));
                rem.setTag(idx as isize);
                rem.setFrame(NSRect::new(NSPoint::new(w - 80.0, ry + 1.0), NSSize::new(68.0, 24.0)));
                card.addSubview(&lbl);
                card.addSubview(&rec);
                card.addSubview(&rem);
            }
        }
        *self.actions.ivars().host_aliases.borrow_mut() = aliases.clone();
        self.detail.addSubview(&card);
        *y += rows_h + 14.0;

        // Discovered hosts from ~/.ssh/config not yet configured.
        let ssh_new: Vec<_> = machines::all()
            .iter()
            .filter(|m| !aliases.iter().any(|a| a == &m.name))
            .take(4)
            .collect();
        *self.actions.ivars().ssh_candidates.borrow_mut() =
            ssh_new.iter().map(|m| m.name.clone()).collect();
        if !ssh_new.is_empty() {
            let sh = 34.0 + ssh_new.len() as f64 * 28.0 + 8.0;
            let scard = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, sh)));
            let shdr = label(mtm, "Discovered in ~/.ssh/config", 12.5, true, &NSColor::labelColor());
            shdr.setFrame(NSRect::new(NSPoint::new(14.0, 10.0), NSSize::new(w - 28.0, 18.0)));
            scard.addSubview(&shdr);
            for (idx, m) in ssh_new.iter().enumerate() {
                let ry = 32.0 + idx as f64 * 28.0;
                let lbl = label(mtm, &format!("{}  ({})", m.name, m.address), 12.0, false, &NSColor::secondaryLabelColor());
                lbl.setFrame(NSRect::new(NSPoint::new(14.0, ry + 4.0), NSSize::new(w - 110.0, 16.0)));
                let add = push_button(mtm, "Add", target, sel!(hostAddSsh:));
                add.setTag(idx as isize);
                add.setFrame(NSRect::new(NSPoint::new(w - 80.0, ry), NSSize::new(66.0, 22.0)));
                scard.addSubview(&lbl);
                scard.addSubview(&add);
            }
            self.detail.addSubview(&scard);
            *y += sh + 14.0;
        }

        // Manual Add Host row (Mac mini, Linux, Windows OpenSSH, or Tailscale TCP).
        let acard = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 102.0)));
        let ahdr = label(
            mtm,
            "Add Host (Mac mini, Linux, Windows OpenSSH, or Tailscale TCP)",
            12.0,
            true,
            &NSColor::labelColor(),
        );
        ahdr.setFrame(NSRect::new(NSPoint::new(14.0, 10.0), NSSize::new(w - 28.0, 16.0)));
        let alias_field = NSTextField::initWithFrame(
            NSTextField::alloc(mtm),
            NSRect::new(NSPoint::new(14.0, 34.0), NSSize::new(124.0, 24.0)),
        );
        alias_field.setPlaceholderString(Some(&NSString::from_str("alias (e.g. win-pc)")));
        let uri_field = NSTextField::initWithFrame(
            NSTextField::alloc(mtm),
            NSRect::new(NSPoint::new(146.0, 34.0), NSSize::new(w - 240.0, 24.0)),
        );
        uri_field.setPlaceholderString(Some(&NSString::from_str("ssh://host or tcp://100.x.y.z:7501")));
        let add_btn = push_button(mtm, "Add", target, sel!(hostAddCustom:));
        add_btn.setFrame(NSRect::new(NSPoint::new(w - 84.0, 33.0), NSSize::new(70.0, 26.0)));
        let token_field = NSTextField::initWithFrame(
            NSTextField::alloc(mtm),
            NSRect::new(NSPoint::new(14.0, 66.0), NSSize::new(w - 28.0, 24.0)),
        );
        token_field.setPlaceholderString(Some(&NSString::from_str(
            "Bearer token (required only for direct tcp:// — run `tachyon server token` on host)",
        )));
        acard.addSubview(&ahdr);
        acard.addSubview(&alias_field);
        acard.addSubview(&uri_field);
        acard.addSubview(&add_btn);
        acard.addSubview(&token_field);
        self.custom_alias = Some(alias_field);
        self.custom_uri = Some(uri_field);
        self.custom_token = Some(token_field);
        self.detail.addSubview(&acard);
        *y += 116.0;
    }

    fn build_integrations(&self, mtm: MainThreadMarker, pad: f64, w: f64, y: &mut f64) {
        let target: &AnyObject = self.actions.as_ref();
        let home = integrations::user_home();
        let statuses = integrations::inspect_all(&home);
        let any_on = statuses.iter().any(|s| s.skill_installed || s.hooks_installed);
        let cfg = Config::load_default();

        // Card 1: Coding Agents (matches Rex screenshot layout!)
        let sub1 = label(mtm, "Coding Agents", 12.5, true, &NSColor::secondaryLabelColor());
        sub1.setFrame(NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 18.0)));
        self.detail.addSubview(&sub1);
        *y += 22.0;

        let card1 = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 138.0)));
        let summary_line: String = statuses
            .iter()
            .map(|s| {
                let icon = match s.id {
                    "claude" => "✳",
                    "antigravity" => "✦",
                    "codex" => "◎",
                    "opencode" => "◈",
                    _ => "π",
                };
                let state = if s.skill_installed { "✓" } else if s.detected { "•" } else { "·" };
                format!("{icon} {} {state}", s.name)
            })
            .collect::<Vec<_>>()
            .join("   ");
        let agents_lbl = label(mtm, &summary_line, 11.5, true, &NSColor::labelColor());
        agents_lbl.setFrame(NSRect::new(NSPoint::new(14.0, 12.0), NSSize::new(w - 28.0, 28.0)));

        let row1_t = label(mtm, "Integrate with available agents", 13.0, true, &NSColor::labelColor());
        row1_t.setFrame(NSRect::new(NSPoint::new(14.0, 50.0), NSSize::new(w - 86.0, 18.0)));
        let row1_d = label(
            mtm,
            "Installs Tachyon skill & OSC 7501 hooks for detected agents (Claude Code, Antigravity, Codex, OpenCode, Pi)",
            11.0,
            false,
            &NSColor::secondaryLabelColor(),
        );
        row1_d.setFrame(NSRect::new(NSPoint::new(14.0, 70.0), NSSize::new(w - 86.0, 30.0)));
        let sw1 = toggle_switch(mtm, any_on, target, sel!(toggleIntegrateAgents:));
        sw1.setFrame(NSRect::new(NSPoint::new(w - 56.0, 54.0), NSSize::new(42.0, 24.0)));

        let foot1 = label(
            mtm,
            "CLI equivalent: `tachyon skill setup` / `tachyon skill remove` (preserves your existing hooks).",
            10.5,
            false,
            &NSColor::tertiaryLabelColor(),
        );
        foot1.setFrame(NSRect::new(NSPoint::new(14.0, 108.0), NSSize::new(w - 28.0, 16.0)));

        for v in [
            &*agents_lbl as &NSView,
            &*row1_t as &NSView,
            &*row1_d as &NSView,
            &*sw1 as &NSView,
            &*foot1 as &NSView,
        ] {
            card1.addSubview(v);
        }
        self.detail.addSubview(&card1);
        *y += 154.0;

        // Card 2: Agent Behavior
        let sub2 = label(mtm, "Agent Behavior", 12.5, true, &NSColor::secondaryLabelColor());
        sub2.setFrame(NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 18.0)));
        self.detail.addSubview(&sub2);
        *y += 22.0;

        let card2 = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 84.0)));
        let row2_t = label(mtm, "Let agents name sessions", 13.0, true, &NSColor::labelColor());
        row2_t.setFrame(NSRect::new(NSPoint::new(14.0, 14.0), NSSize::new(w - 86.0, 18.0)));
        let row2_d = label(
            mtm,
            "Agents label generated sessions after the task they're working on.",
            11.0,
            false,
            &NSColor::secondaryLabelColor(),
        );
        row2_d.setFrame(NSRect::new(NSPoint::new(14.0, 34.0), NSSize::new(w - 86.0, 16.0)));
        let foot2 = label(
            mtm,
            "Changes apply to new agent sessions (`agents.auto_name_sessions`).",
            10.5,
            false,
            &NSColor::tertiaryLabelColor(),
        );
        foot2.setFrame(NSRect::new(NSPoint::new(14.0, 56.0), NSSize::new(w - 28.0, 16.0)));
        let sw2 = toggle_switch(mtm, cfg.agents.auto_name_sessions, target, sel!(toggleAutoName:));
        sw2.setFrame(NSRect::new(NSPoint::new(w - 56.0, 18.0), NSSize::new(42.0, 24.0)));

        for v in [&*row2_t as &NSView, &*row2_d as &NSView, &*foot2 as &NSView, &*sw2 as &NSView] {
            card2.addSubview(v);
        }
        self.detail.addSubview(&card2);
        *y += 98.0;
    }

    fn build_interface(&self, mtm: MainThreadMarker, pad: f64, w: f64, y: &mut f64) {
        let target: &AnyObject = self.actions.as_ref();
        let cfg = Config::load_default();
        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 232.0)));

        let rows: [(&str, &str, bool, objc2::runtime::Sel); 4] = [
            ("Show Multi-Host Sidebar", "Collapsible Liquid Glass sidebar (⌘B)", cfg.window.sidebar, sel!(toggleSidebarSetting:)),
            ("Show Standard Title Bar", "Off uses Tachyon's edge-to-edge layout", cfg.window.title_bar, sel!(toggleTitleBarSetting:)),
            ("Bottom Shortcut Hints Band", "Contextual shortcuts & footer tool icons", cfg.window.hints, sel!(toggleHintsSetting:)),
            ("Persist Shells Across Quits", "Run shells inside Tachyon's background server", cfg.shell.persist, sel!(togglePersistSetting:)),
        ];

        for (i, (title, sub, val, action)) in rows.iter().enumerate() {
            let ry = 12.0 + i as f64 * 44.0;
            let t = label(mtm, title, 12.5, true, &NSColor::labelColor());
            t.setFrame(NSRect::new(NSPoint::new(14.0, ry), NSSize::new(w - 86.0, 16.0)));
            let d = label(mtm, sub, 11.0, false, &NSColor::secondaryLabelColor());
            d.setFrame(NSRect::new(NSPoint::new(14.0, ry + 17.0), NSSize::new(w - 86.0, 15.0)));
            let sw = toggle_switch(mtm, *val, target, *action);
            sw.setFrame(NSRect::new(NSPoint::new(w - 56.0, ry + 4.0), NSSize::new(42.0, 24.0)));
            card.addSubview(&t);
            card.addSubview(&d);
            card.addSubview(&sw);
        }

        // Pane headers mode popup.
        let ry = 12.0 + 4.0 * 44.0;
        let pt = label(mtm, "Per-Pane Header Bars", 12.5, true, &NSColor::labelColor());
        pt.setFrame(NSRect::new(NSPoint::new(14.0, ry), NSSize::new(w - 160.0, 16.0)));
        let pd = label(mtm, "Optional title, OSC 7501 status pill & host badge (off by default)", 11.0, false, &NSColor::secondaryLabelColor());
        pd.setFrame(NSRect::new(NSPoint::new(14.0, ry + 17.0), NSSize::new(w - 160.0, 15.0)));
        let popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect::new(NSPoint::new(w - 124.0, ry + 4.0), NSSize::new(110.0, 24.0)),
            false,
        );
        popup.addItemWithTitle(&NSString::from_str("Always"));
        popup.addItemWithTitle(&NSString::from_str("Auto"));
        popup.addItemWithTitle(&NSString::from_str("Never"));
        popup.selectItemAtIndex(match cfg.window.pane_headers {
            PaneHeaders::Always => 0,
            PaneHeaders::Auto => 1,
            PaneHeaders::Never => 2,
        });
        unsafe {
            popup.setTarget(Some(target));
            popup.setAction(Some(sel!(changePaneHeaders:)));
        }
        card.addSubview(&pt);
        card.addSubview(&pd);
        card.addSubview(&popup);

        self.detail.addSubview(&card);
        *y += 248.0;
    }

    fn build_notifications(&self, mtm: MainThreadMarker, pad: f64, w: f64, y: &mut f64) {
        let target: &AnyObject = self.actions.as_ref();
        let cfg = Config::load_default();
        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 136.0)));

        let t1 = label(mtm, "Notify on coding agent status (OSC 7501)", 13.0, true, &NSColor::labelColor());
        t1.setFrame(NSRect::new(NSPoint::new(14.0, 14.0), NSSize::new(w - 86.0, 18.0)));
        let d1 = label(
            mtm,
            "Alert when any local or remote agent blocks for approval, finishes, or errors. Clicking jumps straight to that pane.",
            11.0,
            false,
            &NSColor::secondaryLabelColor(),
        );
        d1.setFrame(NSRect::new(NSPoint::new(14.0, 34.0), NSSize::new(w - 86.0, 30.0)));
        let sw = toggle_switch(mtm, cfg.macos.notify_agent_status, target, sel!(toggleAgentNotify:));
        sw.setFrame(NSRect::new(NSPoint::new(w - 56.0, 18.0), NSSize::new(42.0, 24.0)));

        let d2 = label(
            mtm,
            &format!(
                "Long-command receipts notify after {}s in background (`notify.after` in tachyon.conf).",
                cfg.macos.notify_after_secs
            ),
            11.0,
            false,
            &NSColor::tertiaryLabelColor(),
        );
        d2.setFrame(NSRect::new(NSPoint::new(14.0, 72.0), NSSize::new(w - 28.0, 16.0)));

        let test_btn = push_button(mtm, "Send Test Notification", target, sel!(sendTestNotification:));
        test_btn.setFrame(NSRect::new(NSPoint::new(14.0, 96.0), NSSize::new(172.0, 26.0)));

        for v in [&*t1 as &NSView, &*d1 as &NSView, &*sw as &NSView, &*d2 as &NSView, &*test_btn as &NSView] {
            card.addSubview(v);
        }
        self.detail.addSubview(&card);
        *y += 150.0;
    }
}

fn label(mtm: MainThreadMarker, text: &str, size: f64, bold: bool, color: &NSColor) -> Retained<NSTextField> {
    let f = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    let font = if bold {
        NSFont::boldSystemFontOfSize(size as CGFloat)
    } else {
        NSFont::systemFontOfSize(size as CGFloat)
    };
    f.setFont(Some(&font));
    f.setTextColor(Some(color));
    f.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    f
}

fn push_button(
    mtm: MainThreadMarker,
    title: &str,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSButton> {
    let btn = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), Some(target), Some(action), mtm)
    };
    btn.setBezelStyle(NSBezelStyle::Push);
    btn
}

fn toggle_switch(
    mtm: MainThreadMarker,
    on: bool,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSSwitch> {
    let sw = NSSwitch::initWithFrame(NSSwitch::alloc(mtm), NSRect::ZERO);
    sw.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    unsafe {
        sw.setTarget(Some(target));
        sw.setAction(Some(action));
    }
    sw
}

#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Medium,
    Semibold,
}

fn draw_text(text: &str, size: f64, weight: Weight, color: &NSColor, rect: NSRect) {
    let para = NSMutableParagraphStyle::new();
    para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let w = unsafe {
        match weight {
            Weight::Regular => NSFontWeightRegular,
            Weight::Medium => NSFontWeightMedium,
            Weight::Semibold => NSFontWeightSemibold,
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
