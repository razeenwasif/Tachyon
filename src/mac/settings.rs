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
use crate::config::{ChromeStyle, Config, PaneHeaders, PaneStyle, ThemePreset};
use crate::integrations;
use crate::mux::default_socket_path;
use crate::server;

const WIN_W: f64 = 690.0;
const WIN_H: f64 = 560.0;
const RAIL_W: f64 = 186.0;
const NAV_ROW_H: f64 = 32.0;

struct NavSection {
    title: &'static str,
    symbol: &'static str,
}

const SECTIONS: [NavSection; 6] = [
    NavSection { title: "Server", symbol: "server.rack" },
    NavSection { title: "Remote Hosts", symbol: "globe" },
    NavSection { title: "Integrations", symbol: "sparkles" },
    NavSection { title: "Interface", symbol: "macwindow" },
    NavSection { title: "Notifications", symbol: "bell.badge" },
    NavSection { title: "Workspaces", symbol: "square.grid.2x2" },
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

fn set_status_note_if_open(text: &str) {
    SETTINGS.with(|slot| {
        if let Ok(guard) = slot.try_borrow() {
            if let Some(win) = guard.as_ref() {
                *win.actions.ivars().status_note.borrow_mut() = Some(text.to_string());
            }
        }
    });
    refresh_if_open();
}

/// Run Fleet Doctor asynchronously across local and all configured remote hosts
/// and update the Settings status bar.
pub fn trigger_fleet_doctor() {
    set_status_note_if_open("Running Fleet Doctor across all configured hosts…");
    std::thread::spawn(|| {
        let hosts = crate::mux::load_hosts();
        let diags = crate::mux::doctor::diagnose_fleet(&hosts, true, std::time::Duration::from_secs(4));
        let total = diags.len();
        let healthy = diags
            .iter()
            .filter(|d| d.health == crate::mux::doctor::HostHealth::Healthy)
            .count();
        let stale: Vec<String> = diags
            .iter()
            .filter(|d| d.update_available)
            .map(|d| d.alias.clone())
            .collect();
        let offline: Vec<String> = diags
            .iter()
            .filter(|d| d.health == crate::mux::doctor::HostHealth::Offline)
            .map(|d| d.alias.clone())
            .collect();
        let summary = if healthy == total {
            format!("Fleet Doctor: {healthy}/{total} hosts healthy (v{}, {}).", crate::APP_VERSION, crate::APP_BUILD_SHA)
        } else {
            let mut parts = vec![format!("Fleet Doctor: {healthy}/{total} healthy")];
            if !stale.is_empty() {
                parts.push(format!("update available: {}", stale.join(", ")));
            }
            if !offline.is_empty() {
                parts.push(format!("offline: {}", offline.join(", ")));
            }
            parts.join(" · ")
        };
        DispatchQueue::main().exec_async(move || {
            super::mux_refresh_all();
            set_status_note_if_open(&summary);
        });
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
    workspace_names: RefCell<Vec<String>>,
    workspace_paths: RefCell<Vec<std::path::PathBuf>>,
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

        #[unsafe(method(serverUpgrade:))]
        fn server_upgrade(&self, _sender: Option<&AnyObject>) {
            self.set_note("Snapshotting sessions & upgrading local server…");
            std::thread::spawn(|| {
                let hosts = crate::mux::load_hosts();
                let res = crate::mux::doctor::execute_upgrade(
                    "local",
                    &hosts,
                    &crate::mux::doctor::UpgradeOptions::default(),
                );
                let msg = match res {
                    Ok(r) => {
                        let lbl = r.after.as_ref().map_or("restarted", |a| a.health.label());
                        format!("Local server upgraded -> {lbl}.")
                    }
                    Err(e) => format!("Local upgrade failed: {e}"),
                };
                DispatchQueue::main().exec_async(move || {
                    super::mux_reconnect_host("local");
                    set_status_note_if_open(&msg);
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

        #[unsafe(method(hostRunDoctor:))]
        fn host_run_doctor(&self, _sender: Option<&AnyObject>) {
            trigger_fleet_doctor();
        }

        #[unsafe(method(hostUpgrade:))]
        fn host_upgrade(&self, sender: Option<&AnyObject>) {
            if let Some(alias) = tagged_string(sender, &self.ivars().host_aliases.borrow()) {
                self.set_note(&format!("Upgrading `{alias}` (snapshot + deploy + restart)…"));
                refresh_if_open();
                std::thread::spawn(move || {
                    let hosts = crate::mux::load_hosts();
                    let uopts = crate::mux::doctor::UpgradeOptions {
                        install_service: true,
                        ..Default::default()
                    };
                    let res = crate::mux::doctor::execute_upgrade(&alias, &hosts, &uopts);
                    let msg = match res {
                        Ok(r) => {
                            let status = r.after.as_ref().map_or("upgraded", |a| a.health.label());
                            format!("Upgraded `{}` ({}) -> {status}", r.plan.alias, r.plan.strategy)
                        }
                        Err(e) => format!("Upgrade `{alias}` failed: {e}"),
                    };
                    DispatchQueue::main().exec_async(move || {
                        super::mux_reconnect_host(&alias);
                        set_status_note_if_open(&msg);
                    });
                });
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

        #[unsafe(method(hostChangeColor:))]
        fn host_change_color(&self, sender: Option<&AnyObject>) {
            let Some(popup) = sender.and_then(|s| s.downcast_ref::<NSPopUpButton>()) else {
                return;
            };
            let row_idx = popup.tag();
            if row_idx < 0 {
                return;
            }
            let alias = self
                .ivars()
                .host_aliases
                .borrow()
                .get(row_idx as usize)
                .cloned();
            let Some(alias) = alias else { return };
            let sel_idx = popup.indexOfSelectedItem();
            let spec = crate::mux::model::HOST_COLOR_PRESETS
                .get(sel_idx as usize)
                .map(|(id, _)| *id)
                .unwrap_or("auto");
            let color = (!spec.eq_ignore_ascii_case("auto")).then(|| spec.to_string());
            super::mux_set_host_color(&alias, color);
            self.set_note(&format!("Set colour for `{alias}` -> {spec}"));
            refresh_if_open();
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

        #[unsafe(method(toggleHostTintSetting:))]
        fn toggle_host_tint_setting(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("window.host_tint", if on { "true" } else { "false" });
            super::apply_interface_host_tint(on);
            self.set_note("Saved `window.host_tint` to tachyon.conf.");
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

        #[unsafe(method(changeThemePreset:))]
        fn change_theme_preset(&self, sender: Option<&AnyObject>) {
            let idx = sender
                .and_then(|s| s.downcast_ref::<NSPopUpButton>())
                .map(|p| p.indexOfSelectedItem())
                .unwrap_or(0);
            let preset = match idx {
                1 => ThemePreset::Obsidian,
                _ => ThemePreset::Default,
            };
            super::apply_interface_theme_preset(preset);
            self.set_note(&format!("Saved `theme.preset = {}`.", preset.as_str()));
            refresh_if_open();
        }

        #[unsafe(method(changeChromeStyle:))]
        fn change_chrome_style(&self, sender: Option<&AnyObject>) {
            let idx = sender
                .and_then(|s| s.downcast_ref::<NSPopUpButton>())
                .map(|p| p.indexOfSelectedItem())
                .unwrap_or(0);
            let style = match idx {
                1 => ChromeStyle::Herdr,
                2 => ChromeStyle::Terminals,
                3 => ChromeStyle::Core,
                _ => ChromeStyle::Glass,
            };
            super::apply_interface_chrome(style);
            self.set_note(&format!("Saved `window.chrome = {}`.", style.as_str()));
            refresh_if_open();
        }

        #[unsafe(method(changePaneStyle:))]
        fn change_pane_style(&self, sender: Option<&AnyObject>) {
            let idx = sender
                .and_then(|s| s.downcast_ref::<NSPopUpButton>())
                .map(|p| p.indexOfSelectedItem())
                .unwrap_or(0);
            let style = match idx {
                1 => PaneStyle::Cards,
                _ => PaneStyle::Seamless,
            };
            super::apply_interface_pane_style(style);
            self.set_note(&format!("Saved `window.pane_style = {}`.", style.as_str()));
            refresh_if_open();
        }

        #[unsafe(method(toggleStartFullscreen:))]
        fn toggle_start_fullscreen(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("window.start_fullscreen", if on { "true" } else { "false" });
            super::apply_settings_config(move |cfg| cfg.window.start_fullscreen = on);
            self.set_note(if on {
                "Tachyon will open in notch full screen on next launch."
            } else {
                "Tachyon will open windowed on launch (`window.start_fullscreen = false`)."
            });
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

        #[unsafe(method(toggleSoundCues:))]
        fn toggle_sound_cues(&self, sender: Option<&AnyObject>) {
            let on = switch_is_on(sender);
            let _ = Config::update_default_key("notify.sound_cues", if on { "true" } else { "false" });
            super::apply_settings_config(move |cfg| cfg.macos.sound_cues = on);
            if on {
                let name = Config::load_default().macos.sound_blocked;
                super::preview_agent_sound(&name);
            }
            self.set_note("Saved `notify.sound_cues` to tachyon.conf.");
            refresh_if_open();
        }

        #[unsafe(method(selectBlockedSound:))]
        fn select_blocked_sound(&self, sender: Option<&AnyObject>) {
            if let Some(name) = selected_sound_name(sender) {
                let _ = Config::update_default_key("notify.sound_blocked", &name);
                let n = name.clone();
                super::apply_settings_config(move |cfg| cfg.macos.sound_blocked = n);
                super::preview_agent_sound(&name);
                self.set_note(&format!("Saved `notify.sound_blocked = {name}`."));
                refresh_if_open();
            }
        }

        #[unsafe(method(selectDoneSound:))]
        fn select_done_sound(&self, sender: Option<&AnyObject>) {
            if let Some(name) = selected_sound_name(sender) {
                let _ = Config::update_default_key("notify.sound_done", &name);
                let n = name.clone();
                super::apply_settings_config(move |cfg| cfg.macos.sound_done = n);
                super::preview_agent_sound(&name);
                self.set_note(&format!("Saved `notify.sound_done = {name}`."));
                refresh_if_open();
            }
        }

        #[unsafe(method(previewSoundCue:))]
        fn preview_sound_cue(&self, _sender: Option<&AnyObject>) {
            let cfg = Config::load_default();
            let name = if cfg.macos.sound_blocked.eq_ignore_ascii_case("none") {
                cfg.macos.sound_done
            } else {
                cfg.macos.sound_blocked
            };
            super::preview_agent_sound(&name);
            self.set_note(&format!("Previewed subtle agent cue (`{name}`)."));
            refresh_if_open();
        }

        #[unsafe(method(sendTestNotification:))]
        fn send_test_notification(&self, _sender: Option<&AnyObject>) {
            super::send_test_agent_notification();
            self.set_note("Sent test notification (macOS banners require /Applications/Tachyon.app).");
            refresh_if_open();
        }

        #[unsafe(method(wsLaunch:))]
        fn ws_launch(&self, sender: Option<&AnyObject>) {
            if let Some(name) = tagged_string(sender, &self.ivars().workspace_names.borrow()) {
                super::mux_workspace_up(0, &name);
                self.set_note(&format!("Launching workspace `{name}`…"));
                refresh_if_open();
            }
        }

        #[unsafe(method(wsStop:))]
        fn ws_stop(&self, sender: Option<&AnyObject>) {
            if let Some(name) = tagged_string(sender, &self.ivars().workspace_names.borrow()) {
                super::mux_workspace_down(&name);
                self.set_note(&format!("Stopping workspace `{name}`…"));
                refresh_if_open();
            }
        }

        #[unsafe(method(wsEdit:))]
        fn ws_edit(&self, sender: Option<&AnyObject>) {
            let idx = sender
                .and_then(|s| s.downcast_ref::<NSButton>())
                .map(|b| b.tag());
            if let Some(i) = idx.filter(|&i| i >= 0) {
                if let Some(path) = self.ivars().workspace_paths.borrow().get(i as usize).cloned() {
                    super::open_workspace_file(&path);
                    self.set_note(&format!("Opened {}", crate::mux::workspace::display_path(&path)));
                    refresh_if_open();
                }
            }
        }

        #[unsafe(method(wsDelete:))]
        fn ws_delete(&self, sender: Option<&AnyObject>) {
            if let Some(name) = tagged_string(sender, &self.ivars().workspace_names.borrow()) {
                match crate::mux::workspace::remove_workspace(&name) {
                    Ok(_) => self.set_note(&format!("Removed workspace `{name}`.")),
                    Err(e) => self.set_note(&format!("Remove failed: {e}")),
                }
                refresh_if_open();
            }
        }

        #[unsafe(method(wsSaveActive:))]
        fn ws_save_active(&self, _sender: Option<&AnyObject>) {
            match super::mux_workspace_save(0, false) {
                Ok((name, path)) => self.set_note(&format!(
                    "Saved `{name}` -> {}",
                    crate::mux::workspace::display_path(&path)
                )),
                Err(e) => self.set_note(&format!("Save failed: {e}")),
            }
            refresh_if_open();
        }

        #[unsafe(method(wsSaveAll:))]
        fn ws_save_all(&self, _sender: Option<&AnyObject>) {
            match super::mux_workspace_save(0, true) {
                Ok((name, path)) => self.set_note(&format!(
                    "Saved `{name}` -> {}",
                    crate::mux::workspace::display_path(&path)
                )),
                Err(e) => self.set_note(&format!("Save failed: {e}")),
            }
            refresh_if_open();
        }

        #[unsafe(method(wsNewStarter:))]
        fn ws_new_starter(&self, _sender: Option<&AnyObject>) {
            match super::mux_workspace_init_starter() {
                Ok((name, path)) => self.set_note(&format!(
                    "Created `{name}` -> {}",
                    crate::mux::workspace::display_path(&path)
                )),
                Err(e) => self.set_note(&format!("Init failed: {e}")),
            }
            refresh_if_open();
        }

        #[unsafe(method(wsInstallStarters:))]
        fn ws_install_starters(&self, _sender: Option<&AnyObject>) {
            match super::mux_workspace_install_starters() {
                Ok(created) if created.is_empty() => {
                    self.set_note("Starter templates already installed (dev-fleet, agent-swarm, ml-cluster).");
                }
                Ok(created) => {
                    self.set_note(&format!("Installed starter template(s): {}", created.join(", ")));
                }
                Err(e) => self.set_note(&format!("Starter install failed: {e}")),
            }
            refresh_if_open();
        }

        #[unsafe(method(wsOpenFolder:))]
        fn ws_open_folder(&self, _sender: Option<&AnyObject>) {
            super::open_workspaces_folder();
            self.set_note("Opened workspaces folder in Finder.");
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

fn selected_sound_name(sender: Option<&AnyObject>) -> Option<String> {
    let popup = sender?.downcast_ref::<NSPopUpButton>()?;
    popup.titleOfSelectedItem().map(|s| s.to_string())
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
            4 => self.build_notifications(mtm, pad, inner_w, &mut y),
            _ => self.build_workspaces(mtm, pad, inner_w, &mut y),
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

        let (state_text, detail_text, local_update) = match local.map(|h| &h.status) {
            Some(HostStatus::Connected { info, rtt_ms }) => {
                let (s, b, c) = local
                    .and_then(|h| h.snapshot.as_ref())
                    .map(|sn| (sn.sessions.len(), sn.blocks.len(), sn.clients.len()))
                    .unwrap_or((0, 0, 0));
                let diag = crate::mux::doctor::diagnose_from_status(
                    "local",
                    &crate::mux::default_local_uri(),
                    true,
                    Some(info),
                    *rtt_ms,
                    s,
                    b,
                    c,
                    None,
                );
                let ver = format!(
                    "v{} ({})",
                    info.version,
                    info.build.as_deref().unwrap_or("legacy")
                );
                let head = if diag.update_available {
                    format!(
                        "{} Running · Update available · PID {} · {ver} · caps:{}/{}",
                        diag.health.dot(),
                        info.pid,
                        diag.capabilities.len(),
                        crate::LOCAL_CAPABILITIES.len(),
                    )
                } else {
                    format!(
                        "{} Running · PID {} · Mux v{} · {ver} · caps:{}/{}",
                        diag.health.dot(),
                        info.pid,
                        info.protocol,
                        diag.capabilities.len(),
                        crate::LOCAL_CAPABILITIES.len(),
                    )
                };
                (
                    head,
                    format!("{} sessions, {} blocks · {}", s, b, default_socket_path().display()),
                    diag.update_available,
                )
            }
            Some(HostStatus::Connecting) => (
                "🟡 Starting local server…".into(),
                default_socket_path().display().to_string(),
                false,
            ),
            _ => (
                "⚪️ Stopped".into(),
                format!("Socket: {}", default_socket_path().display()),
                false,
            ),
        };

        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 116.0)));
        let l1 = label(mtm, "Local Multiplexer Daemon", 13.0, true, &NSColor::labelColor());
        l1.setFrame(NSRect::new(NSPoint::new(14.0, 12.0), NSSize::new(w - 28.0, 18.0)));
        let l2 = label(mtm, &state_text, 12.5, false, &NSColor::labelColor());
        l2.setFrame(NSRect::new(NSPoint::new(14.0, 34.0), NSSize::new(w - 28.0, 18.0)));
        let l3 = label(mtm, &detail_text, 11.0, false, &NSColor::secondaryLabelColor());
        l3.setFrame(NSRect::new(NSPoint::new(14.0, 54.0), NSSize::new(w - 28.0, 16.0)));

        let start_btn = if local_update {
            push_button(mtm, "Restart & Upgrade", target, sel!(serverUpgrade:))
        } else {
            push_button(mtm, "Start / Reconnect", target, sel!(serverStartOrRestart:))
        };
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
        let rows_h = (remotes.len().max(1) as f64) * 52.0 + 40.0;
        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, rows_h)));
        let hdr = label(mtm, "Configured Remote Hosts", 13.0, true, &NSColor::labelColor());
        hdr.setFrame(NSRect::new(NSPoint::new(14.0, 10.0), NSSize::new(w - 170.0, 18.0)));
        let doc_btn = push_button(mtm, "Run Fleet Doctor", target, sel!(hostRunDoctor:));
        doc_btn.setFrame(NSRect::new(NSPoint::new(w - 148.0, 6.0), NSSize::new(134.0, 24.0)));
        card.addSubview(&hdr);
        card.addSubview(&doc_btn);

        if remotes.is_empty() {
            let empty = label(
                mtm,
                "No remote hosts saved yet. Add one below or from ~/.ssh/config.",
                12.0,
                false,
                &NSColor::secondaryLabelColor(),
            );
            empty.setFrame(NSRect::new(NSPoint::new(14.0, 42.0), NSSize::new(w - 28.0, 18.0)));
            card.addSubview(&empty);
        } else {
            for (idx, h) in remotes.iter().enumerate() {
                aliases.push(h.alias.clone());
                let ry = 36.0 + idx as f64 * 52.0;
                let (line1, line2, can_upgrade) = match &h.status {
                    HostStatus::Connected { info, rtt_ms } => {
                        let (s, b, cl) = h
                            .snapshot
                            .as_ref()
                            .map(|sn| (sn.sessions.len(), sn.blocks.len(), sn.clients.len()))
                            .unwrap_or((0, 0, 0));
                        let diag = crate::mux::doctor::diagnose_from_status(
                            &h.alias,
                            &h.uri,
                            false,
                            Some(info),
                            *rtt_ms,
                            s,
                            b,
                            cl,
                            None,
                        );
                        let rtt_str = rtt_ms.map_or_else(String::new, |ms| format!(" · {ms:.0} ms"));
                        let ver = format!(
                            "v{} ({})",
                            info.version,
                            info.build.as_deref().unwrap_or("legacy")
                        );
                        let badge = diag.health.label();
                        let l1 = format!(
                            "{} {} · {}/{}{rtt_str} · {ver} [{badge}]",
                            diag.health.dot(),
                            h.display_name,
                            info.os,
                            info.arch,
                        );
                        let cap_str = if diag.missing_capabilities.is_empty() {
                            format!(
                                "caps:{}/{} · {}",
                                diag.capabilities.len(),
                                crate::LOCAL_CAPABILITIES.len(),
                                diag.auth_status
                            )
                        } else {
                            format!("missing: {}", diag.missing_capabilities.join(", "))
                        };
                        let l2 = format!("{} · {cap_str}", h.uri);
                        (l1, l2, diag.update_available)
                    }
                    HostStatus::Connecting => (
                        format!("🟡 {} (connecting…)", h.display_name),
                        h.uri.clone(),
                        false,
                    ),
                    HostStatus::Disconnected { error } => (
                        format!("🔴 {} (offline)", h.display_name),
                        format!("{} · {error}", h.uri),
                        true,
                    ),
                };
                let lbl = label(mtm, &line1, 12.0, false, &NSColor::labelColor());
                lbl.setFrame(NSRect::new(NSPoint::new(14.0, ry + 5.0), NSSize::new(w - 354.0, 17.0)));
                let sub = label(mtm, &line2, 10.5, false, &NSColor::secondaryLabelColor());
                sub.setFrame(NSRect::new(NSPoint::new(14.0, ry + 24.0), NSSize::new(w - 354.0, 15.0)));

                let color_pop = NSPopUpButton::initWithFrame_pullsDown(
                    NSPopUpButton::alloc(mtm),
                    NSRect::new(NSPoint::new(w - 334.0, ry + 11.0), NSSize::new(82.0, 24.0)),
                    false,
                );
                let cur_spec = h.color.as_deref().unwrap_or("auto");
                let mut sel_idx = 0isize;
                for (pi, &(id, title)) in crate::mux::model::HOST_COLOR_PRESETS.iter().enumerate() {
                    let short = title.split(" (").next().unwrap_or(title);
                    color_pop.addItemWithTitle(&NSString::from_str(short));
                    if cur_spec.eq_ignore_ascii_case(id) {
                        sel_idx = pi as isize;
                    }
                }
                color_pop.selectItemAtIndex(sel_idx);
                color_pop.setTag(idx as isize);
                unsafe {
                    color_pop.setTarget(Some(target));
                    color_pop.setAction(Some(sel!(hostChangeColor:)));
                }
                let upg_title = if can_upgrade { "Upgrade" } else { "Deploy" };
                let upg = push_button(mtm, upg_title, target, sel!(hostUpgrade:));
                upg.setTag(idx as isize);
                upg.setFrame(NSRect::new(NSPoint::new(w - 246.0, ry + 11.0), NSSize::new(72.0, 24.0)));
                let rec = push_button(mtm, "Reconnect", target, sel!(hostReconnect:));
                rec.setTag(idx as isize);
                rec.setFrame(NSRect::new(NSPoint::new(w - 168.0, ry + 11.0), NSSize::new(82.0, 24.0)));
                let rem = push_button(mtm, "Remove", target, sel!(hostRemove:));
                rem.setTag(idx as isize);
                rem.setFrame(NSRect::new(NSPoint::new(w - 82.0, ry + 11.0), NSSize::new(68.0, 24.0)));
                card.addSubview(&lbl);
                card.addSubview(&sub);
                card.addSubview(&color_pop);
                card.addSubview(&upg);
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
        let row_h = 43.0;
        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 445.0)));

        // Theme Preset (row 0), Chrome Style (row 1), and Pane Style (row 2).
        let popups: [(&str, &str, &[&str], isize, objc2::runtime::Sel); 3] = [
            (
                "Theme Preset",
                "Dark colour palette (obsidian matches Herdr's low-glare charcoal)",
                &["Default (Tachyon)", "Obsidian (Dark Minimal)"],
                match cfg.theme_preset {
                    ThemePreset::Default => 0,
                    ThemePreset::Obsidian => 1,
                },
                sel!(changeThemePreset:),
            ),
            (
                "Chrome Style",
                "Window surface & chrome: Liquid Glass or opaque dark minimal presets",
                &["Liquid Glass (Default)", "Herdr (Dark Minimal)", "Terminals (Pill Tabs)", "Core (Zero Chrome)"],
                match cfg.window.chrome {
                    ChromeStyle::Glass => 0,
                    ChromeStyle::Herdr => 1,
                    ChromeStyle::Terminals => 2,
                    ChromeStyle::Core => 3,
                },
                sel!(changeChromeStyle:),
            ),
            (
                "Pane Style",
                "Split pane geometry: edge-to-edge dividers or rounded inset cards (`window.pane_style`)",
                &["Seamless Splits", "Inset Cards"],
                match cfg.window.pane_style {
                    PaneStyle::Seamless => 0,
                    PaneStyle::Cards => 1,
                },
                sel!(changePaneStyle:),
            ),
        ];

        for (i, (title, sub, items, sel_idx, action)) in popups.iter().enumerate() {
            let ry = 12.0 + i as f64 * row_h;
            let pt = label(mtm, title, 12.5, true, &NSColor::labelColor());
            pt.setFrame(NSRect::new(NSPoint::new(14.0, ry), NSSize::new(w - 184.0, 16.0)));
            let pd = label(mtm, sub, 11.0, false, &NSColor::secondaryLabelColor());
            pd.setFrame(NSRect::new(NSPoint::new(14.0, ry + 17.0), NSSize::new(w - 184.0, 15.0)));
            let popup = NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                NSRect::new(NSPoint::new(w - 166.0, ry + 4.0), NSSize::new(152.0, 24.0)),
                false,
            );
            for item_title in *items {
                popup.addItemWithTitle(&NSString::from_str(item_title));
            }
            popup.selectItemAtIndex(*sel_idx);
            unsafe {
                popup.setTarget(Some(target));
                popup.setAction(Some(*action));
            }
            card.addSubview(&pt);
            card.addSubview(&pd);
            card.addSubview(&popup);
        }

        let rows: [(&str, &str, bool, objc2::runtime::Sel); 6] = [
            ("Show Multi-Host Sidebar", "Collapsible sidebar (⌘B)", cfg.window.sidebar, sel!(toggleSidebarSetting:)),
            ("Show Standard Title Bar", "Off uses Tachyon's edge-to-edge layout", cfg.window.title_bar, sel!(toggleTitleBarSetting:)),
            ("Bottom Shortcut Hints Band", "Contextual shortcuts & footer tool icons", cfg.window.hints, sel!(toggleHintsSetting:)),
            ("Host Accent & Danger Tint", "Subtle glass tint, top edge hairline & badge per remote/SSH host", cfg.window.host_tint, sel!(toggleHostTintSetting:)),
            ("Open in Full Screen at Launch", "Start in notch full screen on open (`window.start_fullscreen`)", cfg.window.start_fullscreen, sel!(toggleStartFullscreen:)),
            ("Persist Shells Across Quits", "Run shells inside Tachyon's background server", cfg.shell.persist, sel!(togglePersistSetting:)),
        ];

        for (i, (title, sub, val, action)) in rows.iter().enumerate() {
            let ry = 12.0 + (i + 3) as f64 * row_h;
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

        // Pane headers mode popup (row 9).
        let ry = 12.0 + 9.0 * row_h;
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
        *y += 461.0;
    }

    fn build_notifications(&self, mtm: MainThreadMarker, pad: f64, w: f64, y: &mut f64) {
        let target: &AnyObject = self.actions.as_ref();
        let cfg = Config::load_default();
        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 266.0)));

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

        let t2 = label(mtm, "Subtle audio cues for background agents", 12.5, true, &NSColor::labelColor());
        t2.setFrame(NSRect::new(NSPoint::new(14.0, 74.0), NSSize::new(w - 86.0, 16.0)));
        let d_snd = label(
            mtm,
            "Play a gentle macOS chime (`notify.sound_cues`, `notify.sound_volume`) on blocked/done/error transitions.",
            11.0,
            false,
            &NSColor::secondaryLabelColor(),
        );
        d_snd.setFrame(NSRect::new(NSPoint::new(14.0, 92.0), NSSize::new(w - 86.0, 26.0)));
        let sw2 = toggle_switch(mtm, cfg.macos.sound_cues, target, sel!(toggleSoundCues:));
        sw2.setFrame(NSRect::new(NSPoint::new(w - 56.0, 78.0), NSSize::new(42.0, 24.0)));

        for v in [&*t1 as &NSView, &*d1 as &NSView, &*sw as &NSView, &*t2 as &NSView, &*d_snd as &NSView, &*sw2 as &NSView] {
            card.addSubview(v);
        }

        let sound_rows: [(&str, &str, objc2::runtime::Sel); 2] = [
            ("Blocked cue sound", &cfg.macos.sound_blocked, sel!(selectBlockedSound:)),
            ("Done cue sound", &cfg.macos.sound_done, sel!(selectDoneSound:)),
        ];
        for (idx, (lbl, current, action)) in sound_rows.iter().enumerate() {
            let ry = 126.0 + idx as f64 * 34.0;
            let s_lbl = label(mtm, lbl, 12.0, false, &NSColor::labelColor());
            s_lbl.setFrame(NSRect::new(NSPoint::new(14.0, ry + 4.0), NSSize::new(w - 170.0, 18.0)));
            let popup = NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                NSRect::new(NSPoint::new(w - 150.0, ry), NSSize::new(136.0, 24.0)),
                false,
            );
            let mut selected_idx: Option<isize> = None;
            for (s_i, &title) in super::sound::SYSTEM_SOUNDS.iter().enumerate() {
                popup.addItemWithTitle(&NSString::from_str(title));
                if title.eq_ignore_ascii_case(current) {
                    selected_idx = Some(s_i as isize);
                }
            }
            if selected_idx.is_none() && !current.trim().is_empty() {
                popup.addItemWithTitle(&NSString::from_str(current));
                selected_idx = Some(super::sound::SYSTEM_SOUNDS.len() as isize);
            }
            popup.selectItemAtIndex(selected_idx.unwrap_or(0));
            unsafe {
                popup.setTarget(Some(target));
                popup.setAction(Some(*action));
            }
            card.addSubview(&s_lbl);
            card.addSubview(&popup);
        }

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
        d2.setFrame(NSRect::new(NSPoint::new(14.0, 198.0), NSSize::new(w - 28.0, 16.0)));

        let test_btn = push_button(mtm, "Send Test Notification", target, sel!(sendTestNotification:));
        test_btn.setFrame(NSRect::new(NSPoint::new(14.0, 224.0), NSSize::new(172.0, 26.0)));

        let preview_btn = push_button(mtm, "Preview Chime", target, sel!(previewSoundCue:));
        preview_btn.setFrame(NSRect::new(NSPoint::new(196.0, 224.0), NSSize::new(132.0, 26.0)));

        for v in [&*d2 as &NSView, &*test_btn as &NSView, &*preview_btn as &NSView] {
            card.addSubview(v);
        }
        self.detail.addSubview(&card);
        *y += 280.0;
    }

    fn build_workspaces(&self, mtm: MainThreadMarker, pad: f64, w: f64, y: &mut f64) {
        let target: &AnyObject = self.actions.as_ref();
        let hosts = super::mux_host_views();
        let all_ws = crate::mux::workspace::list_workspaces();
        let shown: Vec<_> = all_ws.iter().take(5).cloned().collect();

        {
            let mut names = self.actions.ivars().workspace_names.borrow_mut();
            let mut paths = self.actions.ivars().workspace_paths.borrow_mut();
            names.clear();
            paths.clear();
            for ws in &shown {
                names.push(ws.name.clone());
                paths.push(ws.path.clone());
            }
        }

        let sub = label(
            mtm,
            "Multi-host Session Templates (launch from here, ⌘⇧P, or `tachyon workspace up`)",
            11.5,
            false,
            &NSColor::secondaryLabelColor(),
        );
        sub.setFrame(NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 16.0)));
        self.detail.addSubview(&sub);
        *y += 22.0;

        let row_h = 46.0;
        let extra_row = if all_ws.len() > shown.len() { 20.0 } else { 0.0 };
        let list_h = if shown.is_empty() {
            66.0
        } else {
            16.0 + shown.len() as f64 * row_h + extra_row
        };
        let card = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, list_h)));

        if shown.is_empty() {
            let empty = label(
                mtm,
                "No saved workspaces yet. Click “Save Active Session” or “New Starter” below, or run `tachyon workspace init dev`.",
                11.5,
                false,
                &NSColor::secondaryLabelColor(),
            );
            empty.setFrame(NSRect::new(NSPoint::new(14.0, 16.0), NSSize::new(w - 28.0, 36.0)));
            card.addSubview(&empty);
        } else {
            for (idx, ws) in shown.iter().enumerate() {
                let ry = 10.0 + idx as f64 * row_h;
                let active = ws.session_names.iter().any(|(host_alias, sess_name)| {
                    hosts
                        .iter()
                        .find(|h| &h.alias == host_alias)
                        .and_then(|h| h.snapshot.as_ref())
                        .is_some_and(|snap| snap.sessions.iter().any(|s| &s.name == sess_name))
                });
                let title_text = if active {
                    format!("🟢 {}", ws.name)
                } else {
                    ws.name.clone()
                };
                let t = label(mtm, &title_text, 12.5, true, &NSColor::labelColor());
                t.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                t.setFrame(NSRect::new(NSPoint::new(14.0, ry), NSSize::new((w - 236.0).max(80.0), 17.0)));

                let meta = format!(
                    "[{}] · {}s/{}w/{}p",
                    ws.hosts.join(", "),
                    ws.session_count,
                    ws.window_count,
                    ws.pane_count
                );
                let d = label(mtm, &meta, 10.5, false, &NSColor::secondaryLabelColor());
                d.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                d.setFrame(NSRect::new(NSPoint::new(14.0, ry + 18.0), NSSize::new((w - 236.0).max(80.0), 15.0)));

                let launch_btn = push_button(mtm, "Up", target, sel!(wsLaunch:));
                launch_btn.setTag(idx as isize);
                launch_btn.setFrame(NSRect::new(NSPoint::new(w - 210.0, ry + 5.0), NSSize::new(48.0, 24.0)));

                let stop_btn = push_button(mtm, "Down", target, sel!(wsStop:));
                stop_btn.setTag(idx as isize);
                stop_btn.setFrame(NSRect::new(NSPoint::new(w - 158.0, ry + 5.0), NSSize::new(56.0, 24.0)));

                let edit_btn = push_button(mtm, "Edit", target, sel!(wsEdit:));
                edit_btn.setTag(idx as isize);
                edit_btn.setFrame(NSRect::new(NSPoint::new(w - 98.0, ry + 5.0), NSSize::new(48.0, 24.0)));

                let del_btn = push_button(mtm, "×", target, sel!(wsDelete:));
                del_btn.setTag(idx as isize);
                del_btn.setFrame(NSRect::new(NSPoint::new(w - 46.0, ry + 5.0), NSSize::new(32.0, 24.0)));

                for v in [
                    &*t as &NSView,
                    &*d as &NSView,
                    &*launch_btn as &NSView,
                    &*stop_btn as &NSView,
                    &*edit_btn as &NSView,
                    &*del_btn as &NSView,
                ] {
                    card.addSubview(v);
                }
            }
            if all_ws.len() > shown.len() {
                let more = label(
                    mtm,
                    &format!(
                        "+{} more template(s) — launch any via ⌘⇧P or Open Folder",
                        all_ws.len() - shown.len()
                    ),
                    10.5,
                    false,
                    &NSColor::tertiaryLabelColor(),
                );
                more.setFrame(NSRect::new(
                    NSPoint::new(14.0, 10.0 + shown.len() as f64 * row_h),
                    NSSize::new(w - 28.0, 16.0),
                ));
                card.addSubview(&more);
            }
        }
        self.detail.addSubview(&card);
        *y += list_h + 14.0;

        // Actions card: Snapshot / Starter / Open Folder.
        let acard = SettingsCardView::new(mtm, NSRect::new(NSPoint::new(pad, *y), NSSize::new(w, 88.0)));
        let b_save = push_button(mtm, "Save Active Session", target, sel!(wsSaveActive:));
        b_save.setFrame(NSRect::new(NSPoint::new(14.0, 12.0), NSSize::new(150.0, 26.0)));
        let b_all = push_button(mtm, "Save All Hosts", target, sel!(wsSaveAll:));
        b_all.setFrame(NSRect::new(NSPoint::new(170.0, 12.0), NSSize::new(120.0, 26.0)));
        let b_init = push_button(mtm, "New Starter", target, sel!(wsNewStarter:));
        b_init.setFrame(NSRect::new(NSPoint::new(296.0, 12.0), NSSize::new(104.0, 26.0)));

        let b_open = push_button(mtm, "Open Folder", target, sel!(wsOpenFolder:));
        b_open.setFrame(NSRect::new(NSPoint::new(14.0, 48.0), NSSize::new(104.0, 26.0)));
        let b_starters = push_button(mtm, "Install Starters", target, sel!(wsInstallStarters:));
        b_starters.setFrame(NSRect::new(NSPoint::new(124.0, 48.0), NSSize::new(122.0, 26.0)));
        let dir_lbl = label(
            mtm,
            &crate::mux::workspace::display_path(&crate::mux::workspace::workspaces_dir()),
            11.0,
            false,
            &NSColor::tertiaryLabelColor(),
        );
        dir_lbl.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        dir_lbl.setFrame(NSRect::new(NSPoint::new(254.0, 53.0), NSSize::new(w - 268.0, 16.0)));

        for v in [
            &*b_save as &NSView,
            &*b_all as &NSView,
            &*b_init as &NSView,
            &*b_open as &NSView,
            &*b_starters as &NSView,
            &*dir_lbl as &NSView,
        ] {
            acard.addSubview(v);
        }
        self.detail.addSubview(&acard);
        *y += 102.0;
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
