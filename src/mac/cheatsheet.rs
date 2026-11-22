//! Floating Liquid Glass Keyboard & CLI Cheatsheet card (`CheatsheetCard`).
//!
//! Accessible from:
//! - Footer icon button (`keyboard`) in the bottom-right hints band
//! - Command Palette (`⌘⇧P` -> "Keyboard Cheatsheet")
//! - Keyboard shortcut `⌘/` and `View > Keyboard Cheatsheet` menu item
//!
//! Includes a live filter box, scrollable grouped sections, rounded keycap
//! badges, and click-to-run (for shortcuts) or click-to-copy (for CLI commands).

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSButton, NSColor, NSControl,
    NSControlTextEditingDelegate, NSEvent, NSFont, NSFontAttributeName, NSFontWeightMedium,
    NSFontWeightRegular, NSFontWeightSemibold, NSForegroundColorAttributeName,
    NSGlassEffectView, NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName,
    NSPasteboard, NSPasteboardTypeString, NSResponder, NSSearchField, NSSearchFieldDelegate,
    NSTextAlignment, NSTextField, NSTextFieldDelegate, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use super::panes;
use super::with_window;

/// Optional action executed when a cheatsheet row is clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetAction {
    ToggleSidebar,
    CommandPalette,
    OpenSettings,
    NewTab,
    NewWindow,
    ClosePane,
    ReopenTab,
    SplitRight,
    SplitDown,
    ZoomPane,
    ToggleBroadcast,
    Neutrino,
    Monitor,
    Fairy,
    QuickTerminal,
    NotchFullScreen,
    RewindBack,
    CopyOutput,
    FoldOutput,
    TrainingChart,
    FollowJob,
    KeepAwake,
    LatencyLedger,
    Find,
    ToggleCopyMode,
    ResetFont,
    AgentMissionControl,
    WorkspaceSwitcher,
    CopyCli(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct SheetEntry {
    pub category: &'static str,
    pub keys: &'static str,
    pub title: &'static str,
    pub detail: &'static str,
    pub action: Option<SheetAction>,
}

pub const ENTRIES: &[SheetEntry] = &[
    // Workspace & Multi-Host
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘B", title: "Toggle multi-host sidebar", detail: "Show or hide This Mac + remote servers", action: Some(SheetAction::ToggleSidebar) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘⇧P", title: "Command Palette", detail: "Jump to any action, host, or session", action: Some(SheetAction::CommandPalette) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘⇧W", title: "Workspace Switcher", detail: "Switch live multi-host workspaces or launch templates", action: Some(SheetAction::WorkspaceSwitcher) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘⇧A", title: "Agent Mission Control", detail: "Cross-host coding-agent inbox (blocked, working & done)", action: Some(SheetAction::AgentMissionControl) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘,", title: "Settings", detail: "Server, Remote Hosts, Agent Integrations & Interface", action: Some(SheetAction::OpenSettings) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘/", title: "Keyboard Cheatsheet", detail: "Show or hide this floating cheatsheet", action: None },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘T", title: "New tab / window", detail: "Open a new persistent shell in the current host", action: Some(SheetAction::NewTab) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘N", title: "New window", detail: "Open an additional Tachyon window", action: Some(SheetAction::NewWindow) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘W / ⌥⌘W", title: "Close pane or window", detail: "⌘W closes focused pane or tab; ⌥⌘W closes window", action: Some(SheetAction::ClosePane) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌥⌘T", title: "Reopen closed tab", detail: "Restore recently closed tab with shells running", action: Some(SheetAction::ReopenTab) },
    SheetEntry { category: "Workspace & Multi-Host", keys: "⌘Q / ⌥⌘Q", title: "Quit (keep vs end shells)", detail: "⌘Q detaches cleanly; ⌥⌘Q terminates shells", action: None },

    // Panes & Splits
    SheetEntry { category: "Panes & Splits", keys: "⌘D", title: "Split right", detail: "Side-by-side split on same host & folder", action: Some(SheetAction::SplitRight) },
    SheetEntry { category: "Panes & Splits", keys: "⌘⇧D", title: "Split down", detail: "Stacked split below focused pane", action: Some(SheetAction::SplitDown) },
    SheetEntry { category: "Panes & Splits", keys: "⌘⌥←→↑↓", title: "Focus adjacent pane", detail: "Move keyboard focus across splits", action: None },
    SheetEntry { category: "Panes & Splits", keys: "⌘⇧↩", title: "Zoom / restore pane", detail: "Temporarily fill window with focused pane", action: Some(SheetAction::ZoomPane) },
    SheetEntry { category: "Panes & Splits", keys: "⌘⇧I", title: "Broadcast input", detail: "Toggle typing & pasting into all split panes at once", action: Some(SheetAction::ToggleBroadcast) },
    SheetEntry { category: "Panes & Splits", keys: "Drag divider", title: "Resize split", detail: "Drag any pane border or sidebar right edge", action: None },

    // Cards & Tools
    SheetEntry { category: "Cards & Tools", keys: "⌘⇧N", title: "Neutrino card", detail: "Repo status, CI runs & server inbox", action: Some(SheetAction::Neutrino) },
    SheetEntry { category: "Cards & Tools", keys: "⌥⌘U", title: "System monitor", detail: "Live CPU, GPU, memory, network & processes", action: Some(SheetAction::Monitor) },
    SheetEntry { category: "Cards & Tools", keys: "⌥⌘I", title: "Ask Fairy", detail: "Ask about the screen, failure, or selection", action: Some(SheetAction::Fairy) },
    SheetEntry { category: "Cards & Tools", keys: "⌃`", title: "Quick terminal", detail: "Drop-down global terminal from the notch", action: Some(SheetAction::QuickTerminal) },
    SheetEntry { category: "Cards & Tools", keys: "⌥⌘F", title: "Full screen around notch", detail: "Tabs left of notch, live status right", action: Some(SheetAction::NotchFullScreen) },
    SheetEntry { category: "Cards & Tools", keys: "⌘[ / ⌘]", title: "Rewind screen", detail: "Scrub past frames; ⌥-click cell for provenance", action: Some(SheetAction::RewindBack) },

    // Commands, ML & HPC
    SheetEntry { category: "Commands, ML & HPC", keys: "⌥⌘O", title: "Copy command output", detail: "Copy selected or latest command block output", action: Some(SheetAction::CopyOutput) },
    SheetEntry { category: "Commands, ML & HPC", keys: "⌥⌘K", title: "Fold / unfold output", detail: "Collapse long command output (or ⌥-click bar)", action: Some(SheetAction::FoldOutput) },
    SheetEntry { category: "Commands, ML & HPC", keys: "⌥⌘C", title: "Training chart", detail: "Live loss/metric curves from training logs", action: Some(SheetAction::TrainingChart) },
    SheetEntry { category: "Commands, ML & HPC", keys: "⌘J", title: "Follow cluster job", detail: "Follow latest Slurm / Nexus job in its own tab", action: Some(SheetAction::FollowJob) },
    SheetEntry { category: "Commands, ML & HPC", keys: "⌥⌘A", title: "Keep Mac awake", detail: "Prevent idle sleep while pane command runs", action: Some(SheetAction::KeepAwake) },
    SheetEntry { category: "Commands, ML & HPC", keys: "⌥⌘L", title: "Latency ledger", detail: "Per-stage keystroke-to-display breakdown", action: Some(SheetAction::LatencyLedger) },
    SheetEntry { category: "Commands, ML & HPC", keys: "⌘↑ / ⌘↓", title: "Previous / next command", detail: "Jump across OSC 133 prompt marks", action: None },

    // Editing, Find & Links
    SheetEntry { category: "Editing, Find & Links", keys: "⌘⇧X / ⌃⇧Space", title: "Vi Copy Mode", detail: "Keyboard scrollback navigation (hjkl, /, v/V, y)", action: Some(SheetAction::ToggleCopyMode) },
    SheetEntry { category: "Editing, Find & Links", keys: "⌘F", title: "Find in scrollback", detail: "Regex & smart-case search (⌘G / ⌘⇧G)", action: Some(SheetAction::Find) },
    SheetEntry { category: "Editing, Find & Links", keys: "Tab / → / ⌥→", title: "Accept suggestion", detail: "Take full inline history suggestion or one word", action: None },
    SheetEntry { category: "Editing, Find & Links", keys: "⌘-click", title: "Open URL, Quick Look, or traceback", detail: "File:line opens in your editor; ⌥⌘-click any file", action: None },
    SheetEntry { category: "Editing, Find & Links", keys: "⌘+ / ⌘- / ⌘0", title: "Zoom font size", detail: "Bigger, smaller, or reset to configured size", action: Some(SheetAction::ResetFont) },
    SheetEntry { category: "Editing, Find & Links", keys: "Shift-drag", title: "Force text selection", detail: "Select text even inside vim, tmux, or htop", action: None },

    // CLI & Coding Agents
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon ls", detail: "List sessions, windows & blocks (add --json)", action: Some(SheetAction::CopyCli("tachyon ls")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon tui [-a]", detail: "Lightweight narrow-screen interactive pane & agent switcher", action: Some(SheetAction::CopyCli("tachyon tui")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon -S kagutsuchi new work", detail: "Start persistent session on Mac mini host", action: Some(SheetAction::CopyCli("tachyon -S kagutsuchi new work")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon run --wait --capture -- CMD", detail: "Run command in a managed pane & collect output", action: Some(SheetAction::CopyCli("tachyon run --wait --capture -- ")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon split --right", detail: "Split current pane from CLI or coding agent", action: Some(SheetAction::CopyCli("tachyon split --right")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon send --all --enter \"CMD\"", detail: "Broadcast input to every split pane in a window", action: Some(SheetAction::CopyCli("tachyon send --all --enter ")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon rewind --list", detail: "Inspect server-side Rewind frames across restarts & remote hosts", action: Some(SheetAction::CopyCli("tachyon rewind --list")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon skill setup", detail: "Install Claude Code, Antigravity, Codex, OpenCode & Pi hooks", action: Some(SheetAction::CopyCli("tachyon skill setup")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon agents --all-hosts", detail: "List active & blocked coding agents across every host", action: Some(SheetAction::CopyCli("tachyon agents --all-hosts")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon status --state working --tty", detail: "Emit OSC 7501 status pill & notification", action: Some(SheetAction::CopyCli("tachyon status --state working --app agent --msg \"Working\" --tty")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon server install-service --listen tailscale", detail: "Install persistent host service + Tailscale TCP (port 7501)", action: Some(SheetAction::CopyCli("tachyon server install-service --listen tailscale")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon hosts add pc tcp://<ip>:7501 --token <tok>", detail: "Connect a Linux or Windows PC directly over Tailscale TCP", action: Some(SheetAction::CopyCli("tachyon hosts add pc tcp://<tailscale-ip>:7501 --token ")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon hosts color <host> <blue|amber|red|auto|none>", detail: "Assign an accent or danger colour to a remote host", action: Some(SheetAction::CopyCli("tachyon hosts color ")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon workspace up <name> [--recreate]", detail: "Launch or reuse a declarative multi-host workspace template", action: Some(SheetAction::CopyCli("tachyon workspace up ")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon workspace save <name> [--all-hosts]", detail: "Snapshot live multi-host sessions & split trees into a template", action: Some(SheetAction::CopyCli("tachyon workspace save ")) },
    SheetEntry { category: "CLI & Coding Agents", keys: "CLI", title: "tachyon workspace down <name>", detail: "Stop all sessions belonging to a workspace across hosts", action: Some(SheetAction::CopyCli("tachyon workspace down ")) },
];

pub fn filter_entries(query: &str) -> Vec<&'static SheetEntry> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return ENTRIES.iter().collect();
    }
    let terms: Vec<&str> = q.split_whitespace().collect();
    ENTRIES
        .iter()
        .filter(|e| {
            let hay = format!("{} {} {} {}", e.category, e.keys, e.title, e.detail).to_ascii_lowercase();
            terms.iter().all(|t| hay.contains(t))
        })
        .collect()
}

#[derive(Clone)]
enum Row {
    Header(&'static str),
    Item(&'static SheetEntry),
}

impl Row {
    fn height(&self) -> f64 {
        match self {
            Self::Header(_) => 24.0,
            Self::Item(_) => 34.0,
        }
    }
}

#[derive(Default)]
struct ListState {
    window: Cell<u64>,
    rows: RefCell<Vec<(f64, f64, Row)>>,
    content_h: Cell<f64>,
    scroll_y: Cell<f64>,
    hover: Cell<Option<usize>>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonCheatsheetListView"]
    #[ivars = ListState]
    struct CheatsheetListView;

    impl CheatsheetListView {
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
            let hit = self.hit_item(p);
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
            let Some(idx) = self.hit_item(p) else { return };
            let entry = self.ivars().rows.borrow().get(idx).and_then(|(_, _, r)| match r {
                Row::Item(e) => Some(*e),
                Row::Header(_) => None,
            });
            let Some(entry) = entry else { return };
            let Some(action) = entry.action else { return };
            let win_id = self.ivars().window.get();
            execute_action(win_id, action);
        }
    }
);

impl CheatsheetListView {
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
        view.set_query("");
        view
    }

    fn set_query(&self, query: &str) {
        let filtered = filter_entries(query);
        let mut rows = Vec::new();
        let mut y = 2.0;
        let mut last_cat = "";
        for entry in filtered {
            if entry.category != last_cat {
                if !last_cat.is_empty() {
                    y += 6.0;
                }
                last_cat = entry.category;
                let r = Row::Header(entry.category);
                let h = r.height();
                rows.push((y, h, r));
                y += h;
            }
            let r = Row::Item(entry);
            let h = r.height();
            rows.push((y, h, r));
            y += h;
        }
        y += 8.0;
        *self.ivars().rows.borrow_mut() = rows;
        self.ivars().content_h.set(y);
        self.ivars().scroll_y.set(0.0);
        self.ivars().hover.set(None);
        self.setNeedsDisplay(true);
    }

    fn hit_item(&self, p: NSPoint) -> Option<usize> {
        let y = p.y + self.ivars().scroll_y.get();
        let rows = self.ivars().rows.borrow();
        for (i, (ry, rh, r)) in rows.iter().enumerate() {
            if y >= *ry && y < *ry + *rh {
                if matches!(r, Row::Item(e) if e.action.is_some()) {
                    return Some(i);
                }
                return None;
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
        let rows = self.ivars().rows.borrow();

        if rows.is_empty() {
            draw_left(
                "No matching shortcuts or commands.",
                12.0,
                Weight::Regular,
                &NSColor::tertiaryLabelColor(),
                NSRect::new(NSPoint::new(12.0, 24.0), NSSize::new((w - 24.0).max(0.0), 18.0)),
            );
            return;
        }

        for (idx, (ry, rh, row)) in rows.iter().enumerate() {
            let y = *ry - scroll;
            if y + *rh < 0.0 || y > b.size.height {
                continue;
            }
            match row {
                Row::Header(cat) => {
                    draw_left(
                        cat,
                        10.5,
                        Weight::Semibold,
                        &NSColor::secondaryLabelColor(),
                        NSRect::new(NSPoint::new(8.0, y + 6.0), NSSize::new((w - 16.0).max(0.0), 14.0)),
                    );
                    NSColor::separatorColor().colorWithAlphaComponent(0.28).setFill();
                    NSBezierPath::fillRect(NSRect::new(
                        NSPoint::new(8.0, y + *rh - 1.0),
                        NSSize::new((w - 16.0).max(0.0), 1.0),
                    ));
                }
                Row::Item(entry) => {
                    if hover == Some(idx) {
                        NSColor::labelColor().colorWithAlphaComponent(0.09).setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                            NSRect::new(NSPoint::new(4.0, y + 1.0), NSSize::new((w - 8.0).max(0.0), *rh - 2.0)),
                            7.0,
                            7.0,
                        )
                        .fill();
                    }

                    // Right-aligned keycap pill.
                    let key_w = ((entry.keys.chars().count() as f64) * 7.2 + 14.0).clamp(38.0, 118.0);
                    let key_x = (w - key_w - 10.0).max(120.0);
                    let key_rect = NSRect::new(NSPoint::new(key_x, y + 7.0), NSSize::new(key_w, 20.0));
                    let is_cli = entry.keys == "CLI";
                    let pill_bg = if is_cli {
                        NSColor::controlAccentColor().colorWithAlphaComponent(0.16)
                    } else {
                        NSColor::labelColor().colorWithAlphaComponent(0.10)
                    };
                    pill_bg.setFill();
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(key_rect, 5.0, 5.0).fill();
                    let pill_fg = if is_cli {
                        NSColor::controlAccentColor()
                    } else {
                        NSColor::labelColor().colorWithAlphaComponent(0.88)
                    };
                    draw_centered(entry.keys, 10.5, true, &pill_fg, key_rect);

                    let text_w = (key_x - 18.0).max(40.0);
                    draw_left(
                        entry.title,
                        12.0,
                        Weight::Medium,
                        &NSColor::labelColor(),
                        NSRect::new(NSPoint::new(10.0, y + 3.0), NSSize::new(text_w, 15.0)),
                    );
                    draw_left(
                        entry.detail,
                        10.5,
                        Weight::Regular,
                        &NSColor::secondaryLabelColor(),
                        NSRect::new(NSPoint::new(10.0, y + 18.0), NSSize::new(text_w, 13.0)),
                    );
                }
            }
        }
    }
}

#[derive(Default)]
struct DelegateState {
    window: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonCheatsheetSearchDelegate"]
    #[ivars = DelegateState]
    struct CheatsheetSearchDelegate;

    unsafe impl NSObjectProtocol for CheatsheetSearchDelegate {}

    unsafe impl NSControlTextEditingDelegate for CheatsheetSearchDelegate {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, note: &NSNotification) {
            if let Some(obj) = note.object() {
                if let Some(field) = obj.downcast_ref::<NSSearchField>() {
                    let q = field.stringValue().to_string();
                    with_window(self.ivars().window.get(), |w| {
                        if let Some(card) = &w.cheatsheet_card {
                            card.list.set_query(&q);
                        }
                    });
                }
            }
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_do_command(
            &self,
            _control: &NSControl,
            _text_view: &objc2_app_kit::NSTextView,
            command: objc2::runtime::Sel,
        ) -> bool {
            if command == sel!(cancelOperation:) {
                let win_id = self.ivars().window.get();
                with_window(win_id, |w| {
                    w.close_cheatsheet();
                    w.focus_terminal();
                });
                true
            } else {
                false
            }
        }
    }

    unsafe impl NSTextFieldDelegate for CheatsheetSearchDelegate {}
    unsafe impl NSSearchFieldDelegate for CheatsheetSearchDelegate {}
);

impl CheatsheetSearchDelegate {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = DelegateState::default();
        state.window.set(window);
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), init] }
    }
}

pub struct CheatsheetCard {
    pub host: Retained<NSView>,
    content: Retained<NSView>,
    title: Retained<NSTextField>,
    hint: Retained<NSTextField>,
    close: Retained<NSButton>,
    pub search: Retained<NSSearchField>,
    list: Retained<CheatsheetListView>,
    _delegate: Retained<CheatsheetSearchDelegate>,
}

impl CheatsheetCard {
    const WIDTH: f64 = 540.0;
    const HEIGHT: f64 = 540.0;

    pub fn new(mtm: MainThreadMarker, parent: &NSView, target: &AnyObject, window: u64) -> Self {
        let content = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let host: Retained<NSView> = if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some() {
            let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), NSRect::ZERO);
            glass.setCornerRadius(18.0);
            glass.setTintColor(Some(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.86)));
            glass.setContentView(Some(&content));
            Retained::into_super(glass)
        } else {
            let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), NSRect::ZERO);
            blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
            blur.setState(NSVisualEffectState::Active);
            blur.addSubview(&content);
            Retained::into_super(blur)
        };

        let title = NSTextField::labelWithString(&NSString::from_str("Tachyon Cheatsheet"), mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));

        let hint = NSTextField::labelWithString(
            &NSString::from_str("Click an action to run or a CLI snippet to copy · Esc to close"),
            mtm,
        );
        hint.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        hint.setTextColor(Some(&NSColor::secondaryLabelColor()));

        let close = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("✕"),
                Some(target),
                Some(sel!(tachyonCheatsheet:)),
                mtm,
            )
        };
        close.setBordered(false);
        close.setToolTip(Some(&NSString::from_str("Close (⌘/ or Esc)")));

        let delegate = CheatsheetSearchDelegate::new(mtm, window);
        let search = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::ZERO);
        search.setPlaceholderString(Some(&NSString::from_str("Filter shortcuts, tools, or CLI commands…")));
        unsafe { search.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        search.setFocusRingType(objc2_app_kit::NSFocusRingType::None);

        let list = CheatsheetListView::new(mtm, window);

        for v in [
            &*list as &NSView,
            &*title as &NSView,
            &*hint as &NSView,
            &*close as &NSView,
            &*search as &NSView,
        ] {
            content.addSubview(v);
        }
        host.setHidden(true);
        parent.addSubview(&host);

        Self {
            host,
            content,
            title,
            hint,
            close,
            search,
            list,
            _delegate: delegate,
        }
    }

    pub fn place(&self, size: NSSize, bottom: f64, top: f64) {
        let w = Self::WIDTH.min(size.width - 24.0).max(280.0);
        let h = Self::HEIGHT.min(size.height - bottom - top - 16.0).max(200.0);
        let frame = NSRect::new(NSPoint::new(size.width - w - 12.0, bottom + 6.0), NSSize::new(w, h));
        self.host.setFrame(frame);
        self.content.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));

        let pad = 14.0;
        self.title.setFrame(NSRect::new(
            NSPoint::new(pad, h - 32.0),
            NSSize::new((w - 2.0 * pad - 30.0).max(60.0), 20.0),
        ));
        self.hint.setFrame(NSRect::new(
            NSPoint::new(pad, h - 50.0),
            NSSize::new((w - 2.0 * pad - 30.0).max(60.0), 15.0),
        ));
        self.close.setFrame(NSRect::new(
            NSPoint::new(w - pad - 24.0, h - 35.0),
            NSSize::new(24.0, 22.0),
        ));
        self.search.setFrame(NSRect::new(
            NSPoint::new(pad, h - 86.0),
            NSSize::new((w - 2.0 * pad).max(80.0), 28.0),
        ));
        self.list.setFrame(NSRect::new(
            NSPoint::new(pad - 4.0, 10.0),
            NSSize::new((w - 2.0 * pad + 8.0).max(80.0), (h - 104.0).max(60.0)),
        ));
    }

    pub fn set_feedback(&self, msg: &str) {
        self.hint.setStringValue(&NSString::from_str(msg));
    }

    pub fn reset_filter(&self) {
        self.search.setStringValue(&NSString::from_str(""));
        self.list.set_query("");
        self.hint.setStringValue(&NSString::from_str(
            "Click an action to run or a CLI snippet to copy · Esc to close",
        ));
    }
}

fn execute_action(win_id: u64, action: SheetAction) {
    if let SheetAction::CopyCli(cmd) = action {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let _ = pb.setString_forType(&NSString::from_str(cmd), unsafe { NSPasteboardTypeString });
        with_window(win_id, |w| {
            if let Some(card) = &w.cheatsheet_card {
                card.set_feedback(&format!("Copied `{cmd}` to clipboard"));
            }
        });
        return;
    }

    with_window(win_id, |w| w.close_cheatsheet());
    match action {
        SheetAction::ToggleSidebar => with_window(win_id, |w| w.toggle_sidebar()),
        SheetAction::CommandPalette => with_window(win_id, |w| w.toggle_palette()),
        SheetAction::OpenSettings => super::open_settings(),
        SheetAction::NewTab => super::open_tab(win_id),
        SheetAction::NewWindow => super::open_window(),
        SheetAction::ClosePane => super::close_active_tab(win_id),
        SheetAction::ReopenTab => super::reopen_closed_tab(win_id),
        SheetAction::SplitRight => super::split_pane(win_id, panes::Dir::Right),
        SheetAction::SplitDown => super::split_pane(win_id, panes::Dir::Down),
        SheetAction::ZoomPane => with_window(win_id, |w| w.toggle_zoom()),
        SheetAction::ToggleBroadcast => with_window(win_id, |w| w.toggle_broadcast()),
        SheetAction::Neutrino => with_window(win_id, |w| w.toggle_neutrino()),
        SheetAction::Monitor => with_window(win_id, |w| w.toggle_monitor()),
        SheetAction::Fairy => with_window(win_id, |w| w.toggle_fairy()),
        SheetAction::QuickTerminal => super::toggle_quick(),
        SheetAction::NotchFullScreen => with_window(win_id, |w| w.toggle_notch()),
        SheetAction::RewindBack => with_window(win_id, |w| w.rewind_step(true)),
        SheetAction::CopyOutput => with_window(win_id, |w| w.copy_command_output()),
        SheetAction::FoldOutput => with_window(win_id, |w| w.toggle_fold_output()),
        SheetAction::TrainingChart => with_window(win_id, |w| w.toggle_chart(None)),
        SheetAction::FollowJob => super::follow_job(win_id, None),
        SheetAction::KeepAwake => with_window(win_id, |w| w.toggle_awake()),
        SheetAction::LatencyLedger => with_window(win_id, |w| w.toggle_ledger()),
        SheetAction::Find => with_window(win_id, |w| w.open_find()),
        SheetAction::ToggleCopyMode => with_window(win_id, |w| w.toggle_copy_mode()),
        SheetAction::ResetFont => with_window(win_id, |w| w.zoom_font(0.0)),
        SheetAction::AgentMissionControl => {
            with_window(win_id, |w| w.open_agents_card());
            return;
        }
        SheetAction::WorkspaceSwitcher => {
            with_window(win_id, |w| w.open_workspaces_card());
            return;
        }
        SheetAction::CopyCli(_) => {}
    }
    with_window(win_id, |w| w.focus_terminal());
}

#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Medium,
    Semibold,
}

fn draw_left(text: &str, size: f64, weight: Weight, color: &NSColor, rect: NSRect) {
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

fn draw_centered(text: &str, size: f64, medium: bool, color: &NSColor, rect: NSRect) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cheatsheet_filters_across_shortcuts_and_cli() {
        assert!(filter_entries("").len() >= 30);
        let hits = filter_entries("kagutsuchi");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.contains("kagutsuchi"));
        let split_hits = filter_entries("split");
        assert!(split_hits.len() >= 3);
    }
}
