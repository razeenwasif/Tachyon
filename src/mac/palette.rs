//! Floating Liquid Glass Command Palette (`⌘⇧P`, or `⌘` toolbar button).
//!
//! Lets you fuzzy-search every Tachyon command (including opening the floating
//! Keyboard Cheatsheet, Settings, splits, tools, coding-agent integration setup)
//! and jump directly to any live session or window across `This Mac` and remote
//! hosts (`kagutsuchi`, …).

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSControl,
    NSControlTextEditingDelegate, NSEvent, NSFont, NSFontAttributeName, NSFontWeightMedium,
    NSFontWeightRegular, NSForegroundColorAttributeName, NSGlassEffectView, NSLineBreakMode,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder, NSSearchField,
    NSSearchFieldDelegate, NSTextAlignment, NSTextFieldDelegate, NSTrackingArea,
    NSTrackingAreaOptions, NSView, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use super::host_mgr::HostView;
use super::{panes, with_window};
use crate::config::{ChromeStyle, PaneStyle, ThemePreset};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    OpenCheatsheet,
    OpenAgents,
    OpenWorkspaceSwitcher,
    OpenSettings,
    ToggleSidebar,
    SetAppearanceBundle {
        preset: ThemePreset,
        chrome: ChromeStyle,
        sidebar: Option<bool>,
        pane_style: Option<PaneStyle>,
    },
    SetPaneStyle(PaneStyle),
    NewTab,
    NewWindow,
    ClosePane,
    ReopenTab,
    SplitRight,
    SplitDown,
    ZoomPane,
    ToggleBroadcast,
    Find,
    RewindBack,
    Neutrino,
    Monitor,
    Fairy,
    TrainingChart,
    LatencyLedger,
    KeepAwake,
    QuickTerminal,
    NotchFullScreen,
    ToggleTitleBar,
    CopyOutput,
    FoldOutput,
    FollowJob,
    SetupAgentIntegrations,
    RefreshHosts,
    SaveServerSnapshot,
    LaunchWorkspace(String),
    StopWorkspace(String),
    SaveActiveSessionWorkspace,
    SaveAllHostsWorkspace,
    InstallStarterTemplates,
    NewGitWorktree,
    OpenWorkspacesFolder,
    NewSessionOnHost(String),
    SwitchWindow { host: String, window_id: String },
    SwitchBlock { host: String, window_id: String, block_id: String },
}

#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub title: String,
    pub subtitle: String,
    pub shortcut: &'static str,
    pub action: PaletteAction,
}

/// Build the full command palette catalog, combining static Tachyon commands
/// with live multi-host sessions and windows from `HostManager`.
pub fn build_items(hosts: &[HostView]) -> Vec<PaletteItem> {
    let mut items = vec![
        PaletteItem {
            title: "Agent Mission Control".into(),
            subtitle: "Live cross-host coding-agent inbox (blocked, working & finished)".into(),
            shortcut: "⌘⇧A",
            action: PaletteAction::OpenAgents,
        },
        PaletteItem {
            title: "Workspace Switcher".into(),
            subtitle: "Switch live multi-host workspaces or launch saved JSON templates".into(),
            shortcut: "⌘⇧W",
            action: PaletteAction::OpenWorkspaceSwitcher,
        },
        PaletteItem {
            title: "Keyboard Cheatsheet".into(),
            subtitle: "Open floating shortcuts, tools & CLI reference pane".into(),
            shortcut: "⌘/",
            action: PaletteAction::OpenCheatsheet,
        },
        PaletteItem {
            title: "Settings".into(),
            subtitle: "Server, Remote Hosts, Coding Agent Integrations & Interface".into(),
            shortcut: "⌘,",
            action: PaletteAction::OpenSettings,
        },
        PaletteItem {
            title: "Toggle Sidebar".into(),
            subtitle: "Show or hide the multi-host Liquid Glass sidebar".into(),
            shortcut: "⌘B",
            action: PaletteAction::ToggleSidebar,
        },
        PaletteItem {
            title: "Split Pane Right".into(),
            subtitle: "Open a new split beside the focused pane".into(),
            shortcut: "⌘D",
            action: PaletteAction::SplitRight,
        },
        PaletteItem {
            title: "Split Pane Down".into(),
            subtitle: "Open a new split below the focused pane".into(),
            shortcut: "⌘⇧D",
            action: PaletteAction::SplitDown,
        },
        PaletteItem {
            title: "Zoom / Restore Pane".into(),
            subtitle: "Toggle focused pane filling the window".into(),
            shortcut: "⌘⇧↩",
            action: PaletteAction::ZoomPane,
        },
        PaletteItem {
            title: "Broadcast Input to All Panes".into(),
            subtitle: "Toggle sending keystrokes & paste to every split pane at once".into(),
            shortcut: "⌘⇧I",
            action: PaletteAction::ToggleBroadcast,
        },
        PaletteItem {
            title: "New Tab / Window".into(),
            subtitle: "Create a new shell in the current host".into(),
            shortcut: "⌘T",
            action: PaletteAction::NewTab,
        },
        PaletteItem {
            title: "New OS Window".into(),
            subtitle: "Open a second Tachyon window".into(),
            shortcut: "⌘N",
            action: PaletteAction::NewWindow,
        },
        PaletteItem {
            title: "Reopen Closed Tab".into(),
            subtitle: "Bring back the last closed tab with its shells".into(),
            shortcut: "⌥⌘T",
            action: PaletteAction::ReopenTab,
        },
        PaletteItem {
            title: "Close Focused Pane".into(),
            subtitle: "Close pane, tab, or window".into(),
            shortcut: "⌘W",
            action: PaletteAction::ClosePane,
        },
        PaletteItem {
            title: "Find in Scrollback".into(),
            subtitle: "Search screen and scrollback with optional regex".into(),
            shortcut: "⌘F",
            action: PaletteAction::Find,
        },
        PaletteItem {
            title: "Rewind Screen".into(),
            subtitle: "Scrub previous frames and inspect cell provenance".into(),
            shortcut: "⌘[",
            action: PaletteAction::RewindBack,
        },
        PaletteItem {
            title: "Neutrino Panel".into(),
            subtitle: "Open git repo status, CI runs & server inbox".into(),
            shortcut: "⌘⇧N",
            action: PaletteAction::Neutrino,
        },
        PaletteItem {
            title: "System Monitor".into(),
            subtitle: "Live CPU, GPU, memory, network & process monitor".into(),
            shortcut: "⌥⌘U",
            action: PaletteAction::Monitor,
        },
        PaletteItem {
            title: "Ask Fairy".into(),
            subtitle: "Explain screen, failure, or selection with Fairy".into(),
            shortcut: "⌥⌘I",
            action: PaletteAction::Fairy,
        },
        PaletteItem {
            title: "Training Metric Chart".into(),
            subtitle: "Plot live loss and training curves from command output".into(),
            shortcut: "⌥⌘C",
            action: PaletteAction::TrainingChart,
        },
        PaletteItem {
            title: "Latency Ledger".into(),
            subtitle: "Inspect keystroke-to-display latency breakdown".into(),
            shortcut: "⌥⌘L",
            action: PaletteAction::LatencyLedger,
        },
        PaletteItem {
            title: "Keep Mac Awake While Running".into(),
            subtitle: "Prevent idle sleep for the active pane's command".into(),
            shortcut: "⌥⌘A",
            action: PaletteAction::KeepAwake,
        },
        PaletteItem {
            title: "Quick Terminal".into(),
            subtitle: "Toggle drop-down global terminal".into(),
            shortcut: "⌃`",
            action: PaletteAction::QuickTerminal,
        },
        PaletteItem {
            title: "Full Screen Around the Notch".into(),
            subtitle: "Cover full display with tabs left of notch".into(),
            shortcut: "⌥⌘F",
            action: PaletteAction::NotchFullScreen,
        },
        PaletteItem {
            title: "Toggle Title Bar Layout".into(),
            subtitle: "Switch between edge-to-edge and standard title bar".into(),
            shortcut: "",
            action: PaletteAction::ToggleTitleBar,
        },
        PaletteItem {
            title: "Appearance: Herdr Dark Minimal".into(),
            subtitle: "Neutral charcoal #0C0D0F / #111214, calm ANSI, sidebar & inset cards".into(),
            shortcut: "",
            action: PaletteAction::SetAppearanceBundle {
                preset: ThemePreset::Obsidian,
                chrome: ChromeStyle::Herdr,
                sidebar: Some(true),
                pane_style: Some(PaneStyle::Cards),
            },
        },
        PaletteItem {
            title: "Appearance: Terminals Minimal (Pill Tabs)".into(),
            subtitle: "Charcoal pill tabs & terminals without the sidebar".into(),
            shortcut: "",
            action: PaletteAction::SetAppearanceBundle {
                preset: ThemePreset::Obsidian,
                chrome: ChromeStyle::Terminals,
                sidebar: Some(false),
                pane_style: Some(PaneStyle::Seamless),
            },
        },
        PaletteItem {
            title: "Appearance: Core (Zero Chrome)".into(),
            subtitle: "Pure #0A0B0C surface with dashed 1px dividers".into(),
            shortcut: "",
            action: PaletteAction::SetAppearanceBundle {
                preset: ThemePreset::Obsidian,
                chrome: ChromeStyle::Core,
                sidebar: Some(false),
                pane_style: Some(PaneStyle::Seamless),
            },
        },
        PaletteItem {
            title: "Appearance: Liquid Glass (Default)".into(),
            subtitle: "Restore translucent macOS Liquid Glass & Tachyon navy theme".into(),
            shortcut: "",
            action: PaletteAction::SetAppearanceBundle {
                preset: ThemePreset::Default,
                chrome: ChromeStyle::Glass,
                sidebar: None,
                pane_style: Some(PaneStyle::Seamless),
            },
        },
        PaletteItem {
            title: "Pane Style: Inset Cards".into(),
            subtitle: "Render split panes as rounded 10pt cards with focus outlines".into(),
            shortcut: "",
            action: PaletteAction::SetPaneStyle(PaneStyle::Cards),
        },
        PaletteItem {
            title: "Pane Style: Seamless Splits".into(),
            subtitle: "Edge-to-edge split panes with hairline dividers".into(),
            shortcut: "",
            action: PaletteAction::SetPaneStyle(PaneStyle::Seamless),
        },
        PaletteItem {
            title: "Copy Command Output".into(),
            subtitle: "Copy selected or latest command block output".into(),
            shortcut: "⌥⌘O",
            action: PaletteAction::CopyOutput,
        },
        PaletteItem {
            title: "Fold / Unfold Command Output".into(),
            subtitle: "Collapse or expand long command output".into(),
            shortcut: "⌥⌘K",
            action: PaletteAction::FoldOutput,
        },
        PaletteItem {
            title: "Follow Cluster Job".into(),
            subtitle: "Track latest Slurm or Nexus job in its own tab".into(),
            shortcut: "⌘J",
            action: PaletteAction::FollowJob,
        },
        PaletteItem {
            title: "Integrate Coding Agents (Claude Code, Antigravity, Codex, OpenCode, Pi)".into(),
            subtitle: "Install Tachyon skill & OSC 7501 hooks for detected agents".into(),
            shortcut: "",
            action: PaletteAction::SetupAgentIntegrations,
        },
        PaletteItem {
            title: "Reconnect & Refresh All Hosts".into(),
            subtitle: "Re-sync sessions across This Mac and remote servers".into(),
            shortcut: "",
            action: PaletteAction::RefreshHosts,
        },
        PaletteItem {
            title: "Server: Save Session Snapshot Now".into(),
            subtitle: "Flush sessions, splits, directories & active agents to state.json".into(),
            shortcut: "",
            action: PaletteAction::SaveServerSnapshot,
        },
        PaletteItem {
            title: "Workspace: Save Active Session".into(),
            subtitle: "Snapshot current session & split tree into ~/.config/Tachyon/workspaces/".into(),
            shortcut: "",
            action: PaletteAction::SaveActiveSessionWorkspace,
        },
        PaletteItem {
            title: "Workspace: Save All Connected Hosts".into(),
            subtitle: "Snapshot all live multi-host sessions into multi-host.json".into(),
            shortcut: "",
            action: PaletteAction::SaveAllHostsWorkspace,
        },
        PaletteItem {
            title: "Worktree: New Isolated Git Worktree Space…".into(),
            subtitle: "Create a linked git branch worktree & open a dedicated Mux session".into(),
            shortcut: "",
            action: PaletteAction::NewGitWorktree,
        },
        PaletteItem {
            title: "Workspace: Install Multi-Host Starter Templates".into(),
            subtitle: "Create dev-fleet, agent-swarm & ml-cluster JSON starters (keeps existing)".into(),
            shortcut: "",
            action: PaletteAction::InstallStarterTemplates,
        },
        PaletteItem {
            title: "Workspace: Open Templates Folder".into(),
            subtitle: "Reveal ~/.config/Tachyon/workspaces/ in Finder (creates starter if empty)".into(),
            shortcut: "",
            action: PaletteAction::OpenWorkspacesFolder,
        },
    ];

    // Surface active coding agents (blocked/error first) for 1-keystroke jump.
    for ag in super::agents::collect_from_host_views(hosts, &[]) {
        let msg = ag
            .message
            .as_deref()
            .map(|m| format!(" — {m}"))
            .unwrap_or_default();
        items.push(PaletteItem {
            title: format!(
                "{} {} [{}]: {} / {}",
                ag.glyph,
                ag.agent_name,
                ag.state.as_str(),
                ag.session_name,
                ag.window_name
            ),
            subtitle: format!("{} ({}){}", ag.host_display, ag.host, msg),
            shortcut: "",
            action: PaletteAction::SwitchBlock {
                host: ag.host,
                window_id: ag.window_id,
                block_id: ag.block_id,
            },
        });
    }

    for ws in crate::mux::workspace::list_workspaces() {
        let running = workspace_is_running(&ws, hosts);
        let host_list = ws.hosts.join(", ");
        let state_tag = if running { " · ● active" } else { "" };
        let desc = ws
            .description
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        items.push(PaletteItem {
            title: format!("Workspace: Up {}", ws.name),
            subtitle: format!(
                "[{}] · {}s/{}w/{}p{}{}",
                host_list, ws.session_count, ws.window_count, ws.pane_count, state_tag, desc
            ),
            shortcut: "",
            action: PaletteAction::LaunchWorkspace(ws.name.clone()),
        });
        if running {
            items.push(PaletteItem {
                title: format!("Workspace: Down {}", ws.name),
                subtitle: format!("Stop sessions for `{}` on [{}]", ws.name, host_list),
                shortcut: "",
                action: PaletteAction::StopWorkspace(ws.name),
            });
        }
    }

    for h in hosts {
        items.push(PaletteItem {
            title: format!("New Session on {}", h.display_name),
            subtitle: format!("Host `{}` · {}", h.alias, h.uri),
            shortcut: "",
            action: PaletteAction::NewSessionOnHost(h.alias.clone()),
        });
        if let Some(snap) = &h.snapshot {
            for sess in &snap.sessions {
                for wid in &sess.windows {
                    let Some(win) = snap.windows.iter().find(|w| &w.id == wid) else {
                        continue;
                    };
                    let focused = snap.blocks.iter().find(|b| b.id == win.focused_block);
                    let wname = win
                        .name
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or_else(|| focused.map(|b| b.display_name()))
                        .unwrap_or_else(|| "shell".into());
                    items.push(PaletteItem {
                        title: format!("Switch to {} / {}", sess.name, wname),
                        subtitle: format!("{} ({})", h.display_name, h.alias),
                        shortcut: "",
                        action: PaletteAction::SwitchWindow {
                            host: h.alias.clone(),
                            window_id: win.id.clone(),
                        },
                    });
                }
            }
        }
    }
    items
}

fn workspace_is_running(ws: &crate::mux::workspace::WorkspaceSummary, hosts: &[HostView]) -> bool {
    ws.session_names.iter().any(|(host_alias, sess_name)| {
        hosts
            .iter()
            .find(|h| &h.alias == host_alias)
            .and_then(|h| h.snapshot.as_ref())
            .is_some_and(|snap| snap.sessions.iter().any(|s| &s.name == sess_name))
    })
}

/// Score and filter palette items by query (prefix > substring > fuzzy subsequence).
pub fn filter_items(all: &[PaletteItem], query: &str) -> Vec<PaletteItem> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return all.to_vec();
    }
    let mut scored: Vec<(i32, usize, PaletteItem)> = all
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let title_l = item.title.to_ascii_lowercase();
            let sub_l = item.subtitle.to_ascii_lowercase();
            let sc = if title_l.starts_with(&q) {
                300
            } else if title_l.contains(&q) {
                200
            } else if sub_l.contains(&q) {
                120
            } else if is_subseq(&q, &title_l) {
                80
            } else if is_subseq(&q, &format!("{title_l} {sub_l}")) {
                40
            } else {
                return None;
            };
            Some((sc, idx, item.clone()))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, it)| it).collect()
}

fn is_subseq(needle: &str, hay: &str) -> bool {
    let mut chars = hay.chars();
    for nc in needle.chars() {
        if nc.is_whitespace() {
            continue;
        }
        if !chars.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}

const ROW_H: f64 = 38.0;

#[derive(Default)]
struct ListState {
    window: Cell<u64>,
    all: RefCell<Vec<PaletteItem>>,
    filtered: RefCell<Vec<PaletteItem>>,
    selected: Cell<usize>,
    scroll_y: Cell<f64>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonPaletteListView"]
    #[ivars = ListState]
    struct PaletteListView;

    impl PaletteListView {
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
            let action = if idx >= 0 {
                self.ivars()
                    .filtered
                    .borrow()
                    .get(idx as usize)
                    .map(|it| it.action.clone())
            } else {
                None
            };
            if let Some(act) = action {
                execute_palette_action(self.ivars().window.get(), act);
            }
        }
    }
);

impl PaletteListView {
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

    fn set_all(&self, items: Vec<PaletteItem>, query: &str) {
        *self.ivars().all.borrow_mut() = items;
        self.apply_query(query);
    }

    fn apply_query(&self, query: &str) {
        let filtered = filter_items(&self.ivars().all.borrow(), query);
        *self.ivars().filtered.borrow_mut() = filtered;
        self.ivars().selected.set(0);
        self.ivars().scroll_y.set(0.0);
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
        let view_h = self.bounds().size.height;
        let mut sc = self.ivars().scroll_y.get();
        if top < sc {
            sc = top;
        } else if bot > sc + view_h {
            sc = (bot - view_h).max(0.0);
        }
        self.ivars().scroll_y.set(sc);
        self.setNeedsDisplay(true);
    }

    fn selected_action(&self) -> Option<PaletteAction> {
        let idx = self.ivars().selected.get();
        self.ivars().filtered.borrow().get(idx).map(|i| i.action.clone())
    }

    fn draw(&self) {
        let b = self.bounds();
        NSBezierPath::clipRect(b);
        let w = b.size.width;
        let scroll = self.ivars().scroll_y.get();
        let sel = self.ivars().selected.get();
        let items = self.ivars().filtered.borrow();

        if items.is_empty() {
            draw_left(
                "No matching commands or sessions.",
                12.0,
                false,
                &NSColor::tertiaryLabelColor(),
                NSRect::new(NSPoint::new(12.0, 14.0), NSSize::new((w - 24.0).max(0.0), 18.0)),
            );
            return;
        }

        for (i, item) in items.iter().enumerate() {
            let y = i as f64 * ROW_H - scroll;
            if y + ROW_H < 0.0 || y > b.size.height {
                continue;
            }
            let is_sel = i == sel;
            if is_sel {
                NSColor::controlAccentColor().colorWithAlphaComponent(0.24).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    NSRect::new(NSPoint::new(6.0, y + 2.0), NSSize::new((w - 12.0).max(0.0), ROW_H - 4.0)),
                    8.0,
                    8.0,
                )
                .fill();
            }

            let mut text_right = w - 14.0;
            if !item.shortcut.is_empty() {
                let sw = ((item.shortcut.chars().count() as f64) * 7.5 + 14.0).clamp(32.0, 96.0);
                let sx = (w - sw - 12.0).max(120.0);
                let srect = NSRect::new(NSPoint::new(sx, y + 9.0), NSSize::new(sw, 20.0));
                NSColor::labelColor().colorWithAlphaComponent(if is_sel { 0.16 } else { 0.09 }).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(srect, 5.0, 5.0).fill();
                draw_centered(item.shortcut, 10.5, true, &NSColor::labelColor(), srect);
                text_right = sx - 10.0;
            }

            let tw = (text_right - 14.0).max(40.0);
            draw_left(
                &item.title,
                12.5,
                true,
                &NSColor::labelColor(),
                NSRect::new(NSPoint::new(14.0, y + 4.0), NSSize::new(tw, 16.0)),
            );
            draw_left(
                &item.subtitle,
                10.5,
                false,
                &NSColor::secondaryLabelColor(),
                NSRect::new(NSPoint::new(14.0, y + 20.0), NSSize::new(tw, 14.0)),
            );
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
    #[name = "TachyonPaletteSearchDelegate"]
    #[ivars = DelegateState]
    struct PaletteSearchDelegate;

    unsafe impl NSObjectProtocol for PaletteSearchDelegate {}

    unsafe impl NSControlTextEditingDelegate for PaletteSearchDelegate {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, note: &NSNotification) {
            if let Some(obj) = note.object() {
                if let Some(field) = obj.downcast_ref::<NSSearchField>() {
                    let q = field.stringValue().to_string();
                    with_window(self.ivars().window.get(), |w| {
                        if let Some(p) = &w.palette_card {
                            p.list.apply_query(&q);
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
            let win_id = self.ivars().window.get();
            if command == sel!(moveDown:) {
                with_window(win_id, |w| {
                    if let Some(p) = &w.palette_card {
                        p.list.move_selection(1);
                    }
                });
                true
            } else if command == sel!(moveUp:) {
                with_window(win_id, |w| {
                    if let Some(p) = &w.palette_card {
                        p.list.move_selection(-1);
                    }
                });
                true
            } else if command == sel!(insertNewline:) {
                let mut picked = None;
                with_window(win_id, |w| {
                    if let Some(p) = &w.palette_card {
                        picked = p.list.selected_action();
                    }
                });
                if let Some(act) = picked {
                    execute_palette_action(win_id, act);
                } else {
                    with_window(win_id, |w| {
                        w.close_palette();
                        w.focus_terminal();
                    });
                }
                true
            } else if command == sel!(cancelOperation:) {
                with_window(win_id, |w| {
                    w.close_palette();
                    w.focus_terminal();
                });
                true
            } else {
                false
            }
        }
    }

    unsafe impl NSTextFieldDelegate for PaletteSearchDelegate {}
    unsafe impl NSSearchFieldDelegate for PaletteSearchDelegate {}
);

impl PaletteSearchDelegate {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let state = DelegateState::default();
        state.window.set(window);
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), init] }
    }
}

pub struct PaletteCard {
    pub host: Retained<NSView>,
    content: Retained<NSView>,
    pub search: Retained<NSSearchField>,
    list: Retained<PaletteListView>,
    _delegate: Retained<PaletteSearchDelegate>,
}

impl PaletteCard {
    const WIDTH: f64 = 480.0;
    const HEIGHT: f64 = 380.0;

    pub fn new(mtm: MainThreadMarker, parent: &NSView, window: u64) -> Self {
        let content = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let host: Retained<NSView> = if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some() {
            let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), NSRect::ZERO);
            glass.setCornerRadius(16.0);
            glass.setTintColor(Some(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.88)));
            glass.setContentView(Some(&content));
            Retained::into_super(glass)
        } else {
            let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), NSRect::ZERO);
            blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
            blur.setState(NSVisualEffectState::Active);
            blur.addSubview(&content);
            Retained::into_super(blur)
        };

        let delegate = PaletteSearchDelegate::new(mtm, window);
        let search = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::ZERO);
        search.setPlaceholderString(Some(&NSString::from_str(
            "Type a command, workspace, cheatsheet, session, or host…",
        )));
        unsafe { search.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        search.setFocusRingType(objc2_app_kit::NSFocusRingType::None);

        let list = PaletteListView::new(mtm, window);
        content.addSubview(&list);
        content.addSubview(&search);
        host.setHidden(true);
        parent.addSubview(&host);

        Self {
            host,
            content,
            search,
            list,
            _delegate: delegate,
        }
    }

    pub fn refresh(&self, hosts: &[HostView]) {
        self.search.setStringValue(&NSString::from_str(""));
        self.list.set_all(build_items(hosts), "");
    }

    pub fn place(&self, size: NSSize, top: f64) {
        let w = Self::WIDTH.min(size.width - 32.0).max(260.0);
        let h = Self::HEIGHT.min(size.height - top - 56.0).max(180.0);
        let x = ((size.width - w) / 2.0).round();
        let y = (size.height - top - h - 28.0).max(16.0).round();
        let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        self.host.setFrame(frame);
        self.content.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));

        let pad = 12.0;
        self.search.setFrame(NSRect::new(
            NSPoint::new(pad, h - 40.0),
            NSSize::new((w - 2.0 * pad).max(80.0), 28.0),
        ));
        self.list.setFrame(NSRect::new(
            NSPoint::new(6.0, 8.0),
            NSSize::new((w - 12.0).max(80.0), (h - 56.0).max(60.0)),
        ));
    }
}

fn execute_palette_action(win_id: u64, action: PaletteAction) {
    with_window(win_id, |w| w.close_palette());
    match action {
        PaletteAction::OpenCheatsheet => {
            with_window(win_id, |w| w.open_cheatsheet());
            return;
        }
        PaletteAction::OpenAgents => {
            with_window(win_id, |w| w.open_agents_card());
            return;
        }
        PaletteAction::OpenWorkspaceSwitcher => {
            with_window(win_id, |w| w.open_workspaces_card());
            return;
        }
        PaletteAction::OpenSettings => {
            super::open_settings();
            return;
        }
        PaletteAction::ToggleSidebar => with_window(win_id, |w| w.toggle_sidebar()),
        PaletteAction::SetAppearanceBundle { preset, chrome, sidebar, pane_style } => {
            super::apply_appearance_bundle(preset, chrome, sidebar, pane_style);
        }
        PaletteAction::SetPaneStyle(style) => super::apply_interface_pane_style(style),
        PaletteAction::NewTab => super::open_tab(win_id),
        PaletteAction::NewWindow => super::open_window(),
        PaletteAction::ClosePane => super::close_active_tab(win_id),
        PaletteAction::ReopenTab => super::reopen_closed_tab(win_id),
        PaletteAction::SplitRight => super::split_pane(win_id, panes::Dir::Right),
        PaletteAction::SplitDown => super::split_pane(win_id, panes::Dir::Down),
        PaletteAction::ZoomPane => with_window(win_id, |w| w.toggle_zoom()),
        PaletteAction::ToggleBroadcast => with_window(win_id, |w| w.toggle_broadcast()),
        PaletteAction::Find => {
            with_window(win_id, |w| w.open_find());
            return;
        }
        PaletteAction::RewindBack => with_window(win_id, |w| w.rewind_step(true)),
        PaletteAction::Neutrino => with_window(win_id, |w| w.toggle_neutrino()),
        PaletteAction::Monitor => with_window(win_id, |w| w.toggle_monitor()),
        PaletteAction::Fairy => with_window(win_id, |w| w.toggle_fairy()),
        PaletteAction::TrainingChart => with_window(win_id, |w| w.toggle_chart(None)),
        PaletteAction::LatencyLedger => with_window(win_id, |w| w.toggle_ledger()),
        PaletteAction::KeepAwake => with_window(win_id, |w| w.toggle_awake()),
        PaletteAction::QuickTerminal => super::toggle_quick(),
        PaletteAction::NotchFullScreen => with_window(win_id, |w| w.toggle_notch()),
        PaletteAction::ToggleTitleBar => with_window(win_id, |w| w.toggle_title_bar()),
        PaletteAction::CopyOutput => with_window(win_id, |w| w.copy_command_output()),
        PaletteAction::FoldOutput => with_window(win_id, |w| w.toggle_fold_output()),
        PaletteAction::FollowJob => super::follow_job(win_id, None),
        PaletteAction::SetupAgentIntegrations => {
            let home = crate::integrations::user_home();
            let auto_name = crate::config::Config::load_default().agents.auto_name_sessions;
            let _ = crate::integrations::setup_all_detected(&home, auto_name);
            super::open_settings_section(2);
            return;
        }
        PaletteAction::RefreshHosts => super::mux_refresh_all(),
        PaletteAction::SaveServerSnapshot => super::mux_save_server_snapshot(),
        PaletteAction::LaunchWorkspace(name) => super::mux_workspace_up(win_id, &name),
        PaletteAction::StopWorkspace(name) => super::mux_workspace_down(&name),
        PaletteAction::SaveActiveSessionWorkspace => {
            if let Err(e) = super::mux_workspace_save(win_id, false) {
                crate::log::force(&format!("workspace save failed: {e}"));
            } else {
                super::open_settings_section(5);
                return;
            }
        }
        PaletteAction::SaveAllHostsWorkspace => {
            if let Err(e) = super::mux_workspace_save(win_id, true) {
                crate::log::force(&format!("multi-host workspace save failed: {e}"));
            } else {
                super::open_settings_section(5);
                return;
            }
        }
        PaletteAction::InstallStarterTemplates => {
            if let Err(e) = super::mux_workspace_install_starters() {
                crate::log::force(&format!("starter install failed: {e}"));
            } else {
                super::open_settings_section(5);
                return;
            }
        }
        PaletteAction::NewGitWorktree => {
            super::mux_prompt_new_worktree(win_id, None);
            return;
        }
        PaletteAction::OpenWorkspacesFolder => {
            super::open_workspaces_folder();
            return;
        }
        PaletteAction::NewSessionOnHost(host) => super::mux_new_session(win_id, &host),
        PaletteAction::SwitchWindow { host, window_id } => {
            super::mux_select_window(win_id, &host, &window_id);
        }
        PaletteAction::SwitchBlock { host, window_id, block_id } => {
            super::mux_select_window_and_block(win_id, &host, &window_id, Some(&block_id));
        }
    }
    with_window(win_id, |w| w.focus_terminal());
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
    fn palette_ranks_cheatsheet_and_fuzzy_queries_first() {
        let items = build_items(&[]);
        let hits = filter_items(&items, "cheat");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].action, PaletteAction::OpenCheatsheet);

        let split_hits = filter_items(&items, "spr");
        assert!(split_hits.iter().any(|i| i.action == PaletteAction::SplitRight));
    }
}
