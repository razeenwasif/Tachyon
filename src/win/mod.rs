//! Win32 window, event loop and session wiring.
//!
//! # Threading
//!
//! Two threads. The reader owns the VT [`Processor`] and applies output to the
//! terminal; the main thread pumps messages and renders. They share the
//! terminal behind a mutex, which the renderer holds only for as long as it
//! takes to build the frame's draw lists.
//!
//! That split is the point: a process spewing output can saturate the reader
//! thread without ever making a keystroke wait, because the main thread's only
//! blocking operation is the wait at the top of the loop.
//!
//! # Frame pacing
//!
//! The loop never spins. It parks in `MsgWaitForMultipleObjectsEx` on three
//! things at once: the window message queue, an event the reader signals when
//! the screen changes, and -- only when there is something to draw -- the swap
//! chain's frame latency waitable. Adding the waitable to the set exactly when
//! we have pending work is what makes "render at most once per refresh, as late
//! as possible, and not at all when idle" fall out naturally.

pub mod install;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::core::{s, w, Result, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_MAINWINDOW, DWMSBT_TABBEDWINDOW,
    DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWM_SYSTEMBACKDROP_TYPE,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Backdrop, Config};
use crate::input::{self, Modifiers, MouseButton, MouseEventKind};
use crate::pty::{self, Pty, SendHandle};
use crate::render::{FrameInput, Renderer, SelectionRange, TabInfo};
use crate::term::{Processor, Term, TermMode};
use crate::{APP_NAME, APP_USER_MODEL_ID};

const WINDOW_CLASS: PCWSTR = w!("TachyonWindowClass");

/// Cursor blink half-period.
const BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// `HWND` wraps a raw pointer and so is not `Send`. Window handles are valid
/// process-wide; the reader thread only uses this to post a message.
#[derive(Clone, Copy)]
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}

impl SendHwnd {
    /// See [`SendHandle::get`] for why this is a method.
    #[inline]
    fn get(self) -> HWND {
        self.0
    }
}

struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
    active: bool,
}

impl Selection {
    fn range(&self) -> Option<SelectionRange> {
        let (a, b) = (self.anchor, self.head);
        if a == b {
            return None;
        }
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        Some(SelectionRange {
            start_line: start.0,
            start_col: start.1,
            end_line: end.0,
            // The head sits *before* the cell it points at, so the inclusive
            // end is one to the left.
            end_col: end.1.saturating_sub(1),
        })
    }
}

/// One terminal session: a child process, its parsed screen, and the thread
/// feeding one into the other.
struct Session {
    term: Arc<Mutex<Term>>,
    pty: Arc<Pty>,
    /// Cleared when the child exits or the pipe closes.
    alive: Arc<AtomicBool>,
    title: String,
    last_title_rev: u64,
}

impl Session {
    /// Start a shell and the reader thread that drives it.
    fn spawn(
        cfg: &Config,
        cols: usize,
        rows: usize,
        wake: SendHandle,
        hwnd: SendHwnd,
    ) -> Result<Session> {
        let (program, mut args) = if cfg.shell.program.is_empty() {
            pty::default_shell()
        } else {
            (cfg.shell.program.clone(), Vec::new())
        };
        if !cfg.shell.args.is_empty() {
            args = cfg.shell.args.clone();
        }

        crate::log!("spawning `{program}` {args:?} at {cols}x{rows}");
        let pty = Arc::new(Pty::spawn(cols as u16, rows as u16, &program, &args, None)?);
        let term = Arc::new(Mutex::new(Term::new(cols, rows, cfg.scrollback)));
        let alive = Arc::new(AtomicBool::new(true));

        {
            let term = Arc::clone(&term);
            let pty_r = Arc::clone(&pty);
            let alive = Arc::clone(&alive);
            let out = pty.output_handle();
            std::thread::Builder::new()
                .name("tachyon-pty-reader".into())
                .spawn(move || {
                    let mut processor = Processor::new();
                    let mut dump = std::env::var_os("TACHYON_DUMP")
                        .and_then(|p| std::fs::File::create(p).ok());
                    // 128 KiB amortises the syscall over a large burst without
                    // holding the terminal lock for an unreasonable stretch.
                    let mut buf = vec![0u8; 128 * 1024];

                    while alive.load(Ordering::Relaxed) {
                        match pty::read_output(out, &mut buf) {
                            Ok(0) => break,
                            Err(e) => {
                                crate::log!("pty: read failed: {e}");
                                break;
                            }
                            Ok(n) => {
                                if let Some(f) = dump.as_mut() {
                                    use std::io::Write;
                                    let _ = f.write_all(&buf[..n]);
                                }
                                let replies = {
                                    let mut t = term.lock();
                                    processor.advance(&mut t, &buf[..n]);
                                    t.take_replies()
                                };
                                if !replies.is_empty() {
                                    let _ = pty_r.write(&replies);
                                }
                                unsafe {
                                    let _ = SetEvent(wake.get());
                                }
                            }
                        }
                    }
                    alive.store(false, Ordering::Relaxed);
                    unsafe {
                        let _ = SetEvent(wake.get());
                        // Wake the main loop so it can retire the tab.
                        let _ = PostMessageW(
                            Some(hwnd.get()),
                            WM_APP_SESSION_ENDED,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                })
                .ok();
        }

        Ok(Session {
            term,
            pty,
            alive,
            title: String::new(),
            last_title_rev: 0,
        })
    }
}

/// Condense a window title into something that fits a tab.
///
/// ConPTY reports the child's full image path, so every tab would otherwise
/// read `...\v1.0\powershell.exe`. The leaf is what actually distinguishes one
/// session from another.
fn tab_label(title: &str) -> String {
    let t = title.trim();
    if t.is_empty() {
        return "shell".to_string();
    }
    let leaf = t.rsplit(['\\', '/']).next().unwrap_or(t);
    if leaf.is_empty() {
        t.to_string()
    } else {
        leaf.to_string()
    }
}

/// Posted by a reader thread when its child is gone.
const WM_APP_SESSION_ENDED: u32 = WM_APP + 1;

pub struct App {
    hwnd: HWND,
    renderer: Renderer,
    sessions: Vec<Session>,
    active: usize,
    cfg: Config,

    wake: SendHandle,
    running: Arc<AtomicBool>,

    focused: bool,
    dpi_scale: f32,
    /// Client size in physical pixels.
    client: (u32, u32),

    selection: Option<Selection>,
    /// Button held down on the application's behalf, if any. Its presence also
    /// records that the press *was* reported, so the matching release is sent
    /// even if the application turns mouse reporting off mid-drag -- otherwise
    /// it would be left believing the button is still down.
    mouse_held: Option<MouseButton>,
    /// Last cell reported. Motion is only sent when this changes: the protocol
    /// cannot express anything finer than a cell, so per-pixel reports would be
    /// pure pty traffic.
    mouse_cell: (usize, usize),
    blink_phase: bool,
    last_blink: Instant,
    /// Forces a redraw even when the terminal itself is unchanged.
    needs_redraw: bool,

    /// When the current synchronised-output batch (mode 2026) began.
    sync_since: Option<Instant>,
    sync_timeout: Duration,
    /// Lower bound on the gap between presents, from `render.max_fps`.
    min_frame_interval: Option<Duration>,
    last_present: Instant,

    window_title: String,
}

impl App {
    #[inline]
    fn term(&self) -> &Arc<Mutex<Term>> {
        &self.sessions[self.active].term
    }

    #[inline]
    fn pty(&self) -> &Arc<Pty> {
        &self.sessions[self.active].pty
    }

    fn tab_infos(&self) -> Vec<TabInfo> {
        self.sessions
            .iter()
            .map(|s| TabInfo {
                title: tab_label(&s.title),
            })
            .collect()
    }

    // ---------------------------------------------------------------------
    // Tabs
    // ---------------------------------------------------------------------

    fn new_tab(&mut self) {
        // Adding the second tab makes the strip appear, which shrinks every
        // grid; compute the new size first so the child starts at the right
        // dimensions and never sees a spurious resize.
        let count = self.sessions.len() + 1;
        let (cols, rows) = self
            .renderer
            .grid_size_for(self.client.0, self.client.1, count);

        match Session::spawn(
            &self.cfg,
            cols,
            rows,
            self.wake,
            SendHwnd(self.hwnd),
        ) {
            Ok(s) => {
                self.sessions.push(s);
                self.active = self.sessions.len() - 1;
                self.resize_sessions(cols, rows);
                self.selection = None;
                self.needs_redraw = true;
            }
            Err(e) => crate::log!("could not open a tab: {e}"),
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.sessions.len() {
            return;
        }
        self.sessions[index].alive.store(false, Ordering::Relaxed);
        self.sessions.remove(index);

        if self.sessions.is_empty() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            return;
        }
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len() - 1;
        }
        // Losing the strip gives the grid its rows back.
        let (cols, rows) = self.renderer.grid_size_for(
            self.client.0,
            self.client.1,
            self.sessions.len(),
        );
        self.resize_sessions(cols, rows);
        self.selection = None;
        self.needs_redraw = true;
    }

    fn select_tab(&mut self, index: usize) {
        if index < self.sessions.len() && index != self.active {
            self.active = index;
            self.selection = None;
            self.term().lock().grid_mut().damage_all();
            self.needs_redraw = true;
        }
    }

    fn cycle_tab(&mut self, forward: bool) {
        let n = self.sessions.len();
        if n < 2 {
            return;
        }
        let next = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
        self.select_tab(next);
    }

    /// Retire any session whose child has exited.
    fn reap_sessions(&mut self) {
        let mut i = 0;
        while i < self.sessions.len() {
            if !self.sessions[i].alive.load(Ordering::Relaxed) {
                self.close_tab(i);
            } else {
                i += 1;
            }
        }
    }

    fn resize_sessions(&mut self, cols: usize, rows: usize) {
        for s in &self.sessions {
            s.term.lock().resize(cols, rows);
            let _ = s.pty.resize(cols as u16, rows as u16);
        }
    }

    /// Translate a client-area point to a (absolute line, column) cell.
    fn point_to_cell(&self, x: i32, y: i32) -> (usize, usize) {
        let m = self.renderer.metrics();
        let pad_x = (self.cfg.window.padding_x * self.dpi_scale).round();
        let pad_y = (self.cfg.window.padding_y * self.dpi_scale).round()
            + self.renderer.tab_strip_height(self.sessions.len());

        let col = (((x as f32 - pad_x) / m.width).floor()).max(0.0) as usize;
        let row = (((y as f32 - pad_y) / m.height).floor()).max(0.0) as usize;

        let term = self.term().lock();
        let col = col.min(term.cols());
        let row = row.min(term.rows().saturating_sub(1));
        let first = term
            .grid()
            .history_len()
            .saturating_sub(term.grid().display_offset());
        (first + row, col)
    }

    /// The cell under a point in the form the mouse protocol wants: `(col,
    /// row)`, both zero-based, row measured from the top of the screen.
    ///
    /// This differs from `point_to_cell` in two ways that matter. The column is
    /// clamped inside the grid rather than being allowed one past the end --
    /// that extra column is a selection concept and has no cell to name. And
    /// the row is relative to the screen, not to the scrollback, because an
    /// application reasons in screen coordinates and knows nothing about the
    /// history above it.
    fn point_to_report_cell(&self, x: i32, y: i32) -> (usize, usize) {
        let (line, col) = self.point_to_cell(x, y);
        let term = self.term().lock();
        let first = term
            .grid()
            .history_len()
            .saturating_sub(term.grid().display_offset());
        let col = col.min(term.cols().saturating_sub(1));
        (col, line.saturating_sub(first))
    }

    /// Report a mouse event, if the application asked for that kind.
    fn report_mouse(&mut self, button: MouseButton, kind: MouseEventKind, col: usize, row: usize) {
        let mode = self.term().lock().mode;
        if let Some(b) = input::encode_mouse(button, kind, col, row, Modifiers::current(), mode) {
            self.write_pty(&b);
        }
    }

    /// Whether a mouse event at this point belongs to the application rather
    /// than to us.
    ///
    /// Shift is the conventional override that takes the mouse back for
    /// selection -- but not once a drag is already under way, since the
    /// application is owed the release for the press it has already seen.
    fn mouse_goes_to_app(&self, x: i32, y: i32) -> bool {
        if self.mouse_held.is_some() {
            return true;
        }
        // The tab strip is our chrome; hovering it must not report cells.
        if self
            .renderer
            .tab_at(x as f32, y as f32, self.sessions.len())
            .is_some()
        {
            return false;
        }
        self.term().lock().mode.intersects(TermMode::ANY_MOUSE) && !Modifiers::current().shift
    }

    fn write_pty(&self, data: &[u8]) {
        let _ = self.pty().write(data);
    }

    fn on_resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.client = (width, height);
        let _ = self.renderer.resize(width, height);

        let (cols, rows) = self
            .renderer
            .grid_size_for(width, height, self.sessions.len());
        // Every session is kept at the window size, not just the visible one,
        // so switching tabs never shows a screen laid out for a stale width.
        self.resize_sessions(cols, rows);
        self.needs_redraw = true;
    }

    fn on_dpi_changed(&mut self, dpi: u32, suggested: &RECT) {
        self.dpi_scale = dpi as f32 / 96.0;
        let _ = self.renderer.set_dpi_scale(self.dpi_scale);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.needs_redraw = true;
    }

    fn on_key_down(&mut self, vk: VIRTUAL_KEY) -> bool {
        let mods = Modifiers::current();

        // Shortcuts are checked before anything reaches the application.
        if mods.ctrl && mods.shift {
            match vk {
                VK_C => {
                    self.copy_selection();
                    return true;
                }
                VK_V => {
                    self.paste();
                    return true;
                }
                VK_T => {
                    self.new_tab();
                    return true;
                }
                VK_W => {
                    self.close_tab(self.active);
                    return true;
                }
                _ => {}
            }
        }

        // Ctrl+Tab cycles tabs. This has to be intercepted here: the shell would
        // otherwise receive a plain tab and try to complete something.
        if mods.ctrl && vk == VK_TAB {
            self.cycle_tab(!mods.shift);
            return true;
        }

        // Alt+1..9 jumps to a tab by position, Alt+0 to the last.
        if mods.alt && !mods.ctrl {
            let digit = match vk.0 {
                d if (0x30..=0x39).contains(&d) => Some(d - 0x30),
                _ => None,
            };
            if let Some(d) = digit {
                let index = if d == 0 {
                    self.sessions.len().saturating_sub(1)
                } else {
                    (d as usize) - 1
                };
                self.select_tab(index);
                return true;
            }
        }
        // Scrollback navigation.
        if mods.shift {
            let mut term = self.term().lock();
            let page = term.rows() as isize;
            match vk {
                VK_PRIOR => {
                    term.scroll_display(page);
                    drop(term);
                    self.needs_redraw = true;
                    return true;
                }
                VK_NEXT => {
                    term.scroll_display(-page);
                    drop(term);
                    self.needs_redraw = true;
                    return true;
                }
                VK_HOME => {
                    let n = term.grid().history_len() as isize;
                    term.scroll_display(n);
                    drop(term);
                    self.needs_redraw = true;
                    return true;
                }
                VK_END => {
                    term.scroll_to_bottom();
                    drop(term);
                    self.needs_redraw = true;
                    return true;
                }
                _ => {}
            }
        }

        let mode = self.term().lock().mode;
        if let Some(bytes) = input::encode_key(vk, mods, mode) {
            self.term().lock().scroll_to_bottom();
            self.write_pty(&bytes);
            self.needs_redraw = true;
            return true;
        }
        false
    }

    fn on_char(&mut self, c: u16, alt: bool) {
        // Nothing is filtered here. Keys we encode ourselves have already had
        // their queued WM_CHAR removed (see `drain_queued_char`), so anything
        // arriving now is genuine text -- including the control codes produced
        // by Ctrl+H, Ctrl+I and Ctrl+M, which must reach the shell as 0x08,
        // 0x09 and 0x0D rather than being mistaken for Backspace/Tab/Enter.
        let ch = c as u32;

        let mut buf = [0u8; 8];
        let text = match char::from_u32(ch) {
            Some(c) => c.encode_utf8(&mut buf).as_bytes().to_vec(),
            None => return,
        };

        let mut out = Vec::with_capacity(text.len() + 1);
        if alt {
            // Meta-prefixing is what readline and every ncurses application
            // expect from Alt.
            out.push(0x1b);
        }
        out.extend_from_slice(&text);

        self.term().lock().scroll_to_bottom();
        self.write_pty(&out);
        self.needs_redraw = true;
    }

    fn on_wheel(&mut self, delta: i16, x: i32, y: i32) {
        let lines = (delta as f32 / WHEEL_DELTA as f32 * 3.0).round() as isize;
        if lines == 0 {
            return;
        }

        let mode = self.term().lock().mode;

        // While an application owns the mouse, forward the wheel to it.
        if mode.intersects(TermMode::ANY_MOUSE) {
            let (col, row) = self.point_to_report_cell(x, y);
            let button = if lines > 0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            for _ in 0..lines.abs() {
                self.report_mouse(button, MouseEventKind::Press, col, row);
            }
            return;
        }

        // Full-screen applications with no mouse mode still expect the wheel to
        // move the cursor, since they have no scrollback of their own.
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALT_SCROLL) {
            let key = if lines > 0 { b'A' } else { b'B' };
            let seq: Vec<u8> = if mode.contains(TermMode::APP_CURSOR) {
                vec![0x1b, b'O', key]
            } else {
                vec![0x1b, b'[', key]
            };
            for _ in 0..lines.abs() {
                self.write_pty(&seq);
            }
            return;
        }

        self.term().lock().scroll_display(lines);
        self.needs_redraw = true;
    }

    fn on_mouse_down(&mut self, button: MouseButton, x: i32, y: i32) {
        // The strip is chrome, not terminal content.
        if let Some(tab) = self
            .renderer
            .tab_at(x as f32, y as f32, self.sessions.len())
        {
            match button {
                MouseButton::Left => self.select_tab(tab),
                MouseButton::Middle => self.close_tab(tab),
                _ => {}
            }
            return;
        }

        if self.mouse_goes_to_app(x, y) {
            let cell = self.point_to_report_cell(x, y);
            self.report_mouse(button, MouseEventKind::Press, cell.0, cell.1);
            self.mouse_held = Some(button);
            self.mouse_cell = cell;
            // Capture so a drag that wanders outside the window keeps
            // reporting, and so the release is not delivered to some other
            // window -- which would leave the application stuck mid-drag.
            unsafe { SetCapture(self.hwnd) };
            return;
        }

        let (line, col) = self.point_to_cell(x, y);
        match button {
            MouseButton::Left => {
                self.selection = Some(Selection {
                    anchor: (line, col),
                    head: (line, col),
                    active: true,
                });
                unsafe { SetCapture(self.hwnd) };
                self.needs_redraw = true;
            }
            MouseButton::Right => self.paste(),
            MouseButton::Middle => self.paste(),
            _ => {}
        }
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        // A selection already in progress wins: the drag began as a selection
        // and has to finish as one, even if the application has asked for the
        // mouse in the meantime.
        if self.selection.as_ref().is_some_and(|s| s.active) {
            // Resolve the cell before touching the selection: `point_to_cell`
            // needs an immutable borrow of self.
            let cell = self.point_to_cell(x, y);
            if let Some(sel) = &mut self.selection {
                if sel.head != cell {
                    sel.head = cell;
                    self.needs_redraw = true;
                }
            }
            return;
        }

        if !self.mouse_goes_to_app(x, y) {
            return;
        }
        let cell = self.point_to_report_cell(x, y);
        if cell == self.mouse_cell {
            return;
        }
        self.mouse_cell = cell;
        let button = self.mouse_held.unwrap_or(MouseButton::None);
        self.report_mouse(button, MouseEventKind::Move, cell.0, cell.1);
    }

    fn on_mouse_up(&mut self, button: MouseButton, x: i32, y: i32) {
        if self.mouse_held == Some(button) {
            self.mouse_held = None;
            let cell = self.point_to_report_cell(x, y);
            self.mouse_cell = cell;
            self.report_mouse(button, MouseEventKind::Release, cell.0, cell.1);
            unsafe {
                let _ = ReleaseCapture();
            }
            return;
        }

        if button == MouseButton::Left {
            if let Some(sel) = &mut self.selection {
                sel.active = false;
            }
            unsafe {
                let _ = ReleaseCapture();
            }
        }
    }

    fn selection_text(&self) -> Option<String> {
        let range = self.selection.as_ref()?.range()?;
        let term = self.term().lock();
        Some(term.text_range(
            range.start_line,
            range.start_col,
            range.end_line,
            range.end_col,
        ))
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selection_text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        set_clipboard(self.hwnd, &text);
    }

    fn paste(&mut self) {
        let Some(text) = get_clipboard(self.hwnd) else {
            return;
        };
        // Normalise line endings: applications expect CR, and a stray LF is
        // read as a second Enter.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");

        let bracketed = self.term().lock().mode.contains(TermMode::BRACKETED_PASTE);
        self.term().lock().scroll_to_bottom();

        if bracketed {
            self.write_pty(b"\x1b[200~");
            self.write_pty(text.as_bytes());
            self.write_pty(b"\x1b[201~");
        } else {
            self.write_pty(text.as_bytes());
        }
        self.needs_redraw = true;
    }

    fn update_title(&mut self) {
        // Every session tracks its own title so the strip stays live even for
        // tabs that are not on screen.
        let mut changed = false;
        for s in &mut self.sessions {
            let (rev, title) = {
                let term = s.term.lock();
                (term.title_revision, term.title.clone())
            };
            if rev != s.last_title_rev {
                s.last_title_rev = rev;
                s.title = title.unwrap_or_default();
                changed = true;
            }
        }
        if changed {
            self.needs_redraw = true;
        }

        let active = self.sessions.get(self.active);
        let text = match active.map(|s| s.title.as_str()) {
            Some(t) if !t.is_empty() => format!("{t} - {APP_NAME}"),
            _ => APP_NAME.to_string(),
        };
        if text == self.window_title {
            return;
        }
        self.window_title = text.clone();
        unsafe {
            let h = HSTRING::from(text);
            let _ = SetWindowTextW(self.hwnd, PCWSTR(h.as_ptr()));
        }
    }

    /// Draw a frame if anything changed. Returns whether we presented.
    fn maybe_render(&mut self) -> bool {
        let (dirty, syncing) = {
            let mut term = self.term().lock();
            (term.take_dirty(), term.mode.contains(TermMode::SYNC_UPDATE))
        };
        if dirty {
            self.needs_redraw = true;
        }
        if !self.needs_redraw {
            return false;
        }

        // Synchronised output (mode 2026): an application has told us it is
        // mid-update, so showing the screen now would show it half-drawn. Hold
        // the frame until the batch ends -- but only up to a timeout, because
        // an application that sets the mode and then dies must not freeze the
        // display permanently.
        if syncing {
            let started = *self.sync_since.get_or_insert_with(Instant::now);
            if started.elapsed() < self.sync_timeout {
                return false;
            }
        } else {
            self.sync_since = None;
        }

        // Optional frame rate cap. Distinct from vsync: this is for people who
        // want to bound GPU wake-ups on a battery, not for tearing control.
        if let Some(min) = self.min_frame_interval {
            if self.last_present.elapsed() < min {
                return false;
            }
        }

        self.needs_redraw = false;

        let selection = self.selection.as_ref().and_then(|s| s.range());
        let tabs = self.tab_infos();
        let active_tab = self.active;
        let term = self.sessions[self.active].term.lock();
        let input = FrameInput {
            term: &term,
            selection,
            focused: self.focused,
            cursor_blink_on: self.blink_phase,
            tabs: &tabs,
            active_tab,
        };
        let ok = self.renderer.draw(&input).is_ok();
        drop(term);

        if ok {
            self.renderer.present();
            self.last_present = Instant::now();
        }
        true
    }

    fn tick_blink(&mut self) {
        if self.last_blink.elapsed() >= BLINK_INTERVAL {
            self.last_blink = Instant::now();
            self.blink_phase = !self.blink_phase;
            let blinking = self.term().lock().cursor_state().blinking;
            if blinking && self.focused {
                self.needs_redraw = true;
            }
        }
    }
}

// ===========================================================================
// Window plumbing
// ===========================================================================

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The App pointer is installed on WM_NCCREATE; messages before that get the
    // default handling.
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let app = &mut *ptr;

    match msg {
        WM_SIZE => {
            let w = (lparam.0 & 0xFFFF) as u32;
            let h = ((lparam.0 >> 16) & 0xFFFF) as u32;
            app.on_resize(w, h);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xFFFF) as u32;
            let rect = &*(lparam.0 as *const RECT);
            app.on_dpi_changed(dpi, rect);
            LRESULT(0)
        }
        WM_SETFOCUS => {
            app.focused = true;
            app.needs_redraw = true;
            let mode = app.term().lock().mode;
            if mode.contains(TermMode::FOCUS_REPORT) {
                app.write_pty(b"\x1b[I");
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            app.focused = false;
            app.needs_redraw = true;
            let mode = app.term().lock().mode;
            if mode.contains(TermMode::FOCUS_REPORT) {
                app.write_pty(b"\x1b[O");
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = VIRTUAL_KEY(wparam.0 as u16);
            // `TranslateMessage` runs in the message loop *before* we get here,
            // so the WM_CHAR for this key is already sitting in the queue.
            // Handling the key is therefore not enough on its own: without
            // removing it, Ctrl+Shift+C would copy *and* deliver 0x03 to the
            // shell, interrupting whatever is running.
            if app.on_key_down(vk) {
                drain_queued_char(hwnd);
                return LRESULT(0);
            }
            // Let Alt+F4 and friends through.
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CHAR => {
            app.on_char(wparam.0 as u16, false);
            LRESULT(0)
        }
        WM_SYSCHAR => {
            // Alt+key. Swallow it so Windows does not beep looking for a menu.
            app.on_char(wparam.0 as u16, true);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            // Wheel coordinates are screen-relative.
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let _ = ScreenToClient(hwnd, &mut pt);
            app.on_wheel(delta, pt.x, pt.y);
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let (x, y) = lparam_to_point(lparam);
            let button = match msg {
                WM_LBUTTONDOWN => MouseButton::Left,
                WM_RBUTTONDOWN => MouseButton::Right,
                _ => MouseButton::Middle,
            };
            let _ = SetFocus(Some(hwnd));
            app.on_mouse_down(button, x, y);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_to_point(lparam);
            app.on_mouse_move(x, y);
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let (x, y) = lparam_to_point(lparam);
            let button = match msg {
                WM_LBUTTONUP => MouseButton::Left,
                WM_RBUTTONUP => MouseButton::Right,
                _ => MouseButton::Middle,
            };
            app.on_mouse_up(button, x, y);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // Something took the mouse away -- a modal dialog, Alt+Tab. There
            // will be no button-up, so end the drag here rather than leaving a
            // selection or a reported button stuck down.
            if let Some(sel) = &mut app.selection {
                sel.active = false;
            }
            app.mouse_held = None;
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // I-beam over the text area, arrow over the frame.
            if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                SetCursor(Some(LoadCursorW(None, IDC_IBEAM).unwrap_or_default()));
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        // We paint every pixel ourselves; letting GDI erase first only flickers.
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            app.needs_redraw = true;
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // Keep the window snapping to whole cells while the user drags it.
        WM_SIZING => {
            let rect = &mut *(lparam.0 as *mut RECT);
            app.snap_to_cells(wparam.0 as u32, rect);
            LRESULT(1)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            app.running.store(false, Ordering::Relaxed);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Remove the `WM_CHAR` that `TranslateMessage` queued for a key press we have
/// already handled ourselves.
///
/// Peeking the queue is used rather than a "swallow the next character" flag
/// because it is exact: it consumes the character belonging to *this*
/// keystroke, and cannot mis-fire on a later one that arrives without a
/// preceding key-down, as IME and dead-key input does.
fn drain_queued_char(hwnd: HWND) {
    unsafe {
        let mut msg = MSG::default();
        for (first, last) in [(WM_CHAR, WM_DEADCHAR), (WM_SYSCHAR, WM_SYSDEADCHAR)] {
            while PeekMessageW(&mut msg, Some(hwnd), first, last, PM_REMOVE).as_bool() {}
        }
    }
}

fn lparam_to_point(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

impl App {
    /// Round a drag-resize to a whole number of cells so the grid never has a
    /// partially visible row or column.
    fn snap_to_cells(&self, edge: u32, rect: &mut RECT) {
        let m = self.renderer.metrics();
        let pad_x = (self.cfg.window.padding_x * self.dpi_scale).round() * 2.0;
        let pad_y = (self.cfg.window.padding_y * self.dpi_scale).round() * 2.0;

        // Non-client area is the difference between window and client size.
        let (mut wr, mut cr) = (RECT::default(), RECT::default());
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut wr);
            let _ = GetClientRect(self.hwnd, &mut cr);
        }
        let chrome_w = (wr.right - wr.left) - (cr.right - cr.left);
        let chrome_h = (wr.bottom - wr.top) - (cr.bottom - cr.top);

        let want_w = (rect.right - rect.left - chrome_w) as f32 - pad_x;
        let want_h = (rect.bottom - rect.top - chrome_h) as f32 - pad_y;

        let cols = (want_w / m.width).round().max(8.0);
        let rows = (want_h / m.height).round().max(2.0);

        let new_w = (cols * m.width + pad_x).round() as i32 + chrome_w;
        let new_h = (rows * m.height + pad_y).round() as i32 + chrome_h;

        // Move the edge the user is actually dragging.
        match edge {
            WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT => rect.left = rect.right - new_w,
            _ => rect.right = rect.left + new_w,
        }
        match edge {
            WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT => rect.top = rect.bottom - new_h,
            _ => rect.bottom = rect.top + new_h,
        }
    }
}

/// Ask the compositor to draw a backdrop behind the window.
///
/// Windows offers two different things under the word "acrylic", and the
/// difference matters:
///
///  * **System backdrops** (`DWMWA_SYSTEMBACKDROP_TYPE`, Windows 11 22H2+) are
///    documented and stable, but they sample the *desktop wallpaper*. Mica is
///    exactly this and looks right for a main window. What they do not do is
///    blur the windows sitting behind yours -- so on a dark wallpaper the
///    effect is close to a flat tint.
///
///  * **Accent policy blur** (`SetWindowCompositionAttribute`) blurs whatever is
///    actually behind the window, live. That is the "frosted glass" people mean.
///    It is undocumented -- the entry point is exported from user32 but absent
///    from the headers, so it has to be resolved by name -- and Microsoft has
///    never committed to it. It has worked since Windows 10 1803 and is what
///    most Win32 applications with glass effects use.
///
/// We use the second for `acrylic`/`blur` and the first for `mica`/`tabbed`,
/// and fall back to the documented one if the undocumented entry point is
/// missing. Either way a failure just leaves a plainly translucent window.
fn apply_backdrop(hwnd: HWND, backdrop: Backdrop) {
    match backdrop {
        Backdrop::None => {}
        Backdrop::Acrylic | Backdrop::Blur => {
            let acrylic = backdrop == Backdrop::Acrylic;
            if !apply_accent_blur(hwnd, acrylic) {
                crate::log!("accent blur unavailable; falling back to system backdrop");
                apply_system_backdrop(hwnd, DWMSBT_TRANSIENTWINDOW);
            }
        }
        Backdrop::Mica => apply_system_backdrop(hwnd, DWMSBT_MAINWINDOW),
        Backdrop::Tabbed => apply_system_backdrop(hwnd, DWMSBT_TABBEDWINDOW),
    }
}

fn apply_system_backdrop(hwnd: HWND, kind: DWM_SYSTEMBACKDROP_TYPE) {
    unsafe {
        // The backdrop is painted in the frame region, so the frame has to
        // cover the client area for it to appear behind the text. Negative
        // margins mean "the whole window".
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);

        let result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &kind as *const _ as *const _,
            std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        );
        crate::log!("system backdrop {}: {result:?}", kind.0);
    }
}

// --- undocumented accent policy ---------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    /// ABGR. The alpha byte is the tint strength.
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttribData {
    attribute: u32,
    data: *mut std::ffi::c_void,
    size: usize,
}

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

/// Returns false if the entry point is missing or the call fails.
fn apply_accent_blur(hwnd: HWND, acrylic: bool) -> bool {
    type SetWindowCompositionAttributeFn =
        unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> BOOL;

    unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else {
            return false;
        };
        let Some(proc) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else {
            return false;
        };
        let set_attr: SetWindowCompositionAttributeFn = std::mem::transmute(proc);

        let mut policy = AccentPolicy {
            accent_state: if acrylic {
                ACCENT_ENABLE_ACRYLICBLURBEHIND
            } else {
                ACCENT_ENABLE_BLURBEHIND
            },
            // Fully transparent tint: our own swap chain already paints the
            // theme background at the configured opacity, and letting the
            // accent policy tint as well would darken it twice.
            gradient_color: 0x0000_0000,
            ..Default::default()
        };

        let mut data = WindowCompositionAttribData {
            attribute: WCA_ACCENT_POLICY,
            data: &mut policy as *mut _ as *mut std::ffi::c_void,
            size: std::mem::size_of::<AccentPolicy>(),
        };

        let ok = set_attr(hwnd, &mut data).as_bool();
        crate::log!("accent blur (acrylic={acrylic}): {ok}");
        ok
    }
}

fn set_clipboard(hwnd: HWND, text: &str) {
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return;
        }
        let _ = EmptyClipboard();

        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = std::mem::size_of_val(&wide[..]);
        if let Ok(mem) = GlobalAlloc(GHND, bytes) {
            let ptr = GlobalLock(mem);
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
                let _ = GlobalUnlock(mem);
                // Ownership of `mem` transfers to the clipboard on success.
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

fn get_clipboard(hwnd: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return None;
        }
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let mem = windows::Win32::Foundation::HGLOBAL(handle.0);
            let ptr = GlobalLock(mem) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(mem);
            Some(s)
        })();
        let _ = CloseClipboard();
        result
    }
}

// ===========================================================================
// Entry point
// ===========================================================================

pub fn run(cfg: Config) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Taskbar identity. Must match the AppUserModelID the installer stamps
        // on the Start menu shortcut, or a pinned icon and a live window show
        // up as two separate taskbar entries.
        let aumid = HSTRING::from(APP_USER_MODEL_ID);
        let _ = SetCurrentProcessExplicitAppUserModelID(PCWSTR(aumid.as_ptr()));

        let instance = GetModuleHandleW(None)?;

        // Icon 1 is the RT_GROUP_ICON emitted by build.rs.
        let icon = LoadImageW(
            Some(instance.into()),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();

        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_IBEAM).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: WINDOW_CLASS,
            hIcon: icon,
            hIconSm: icon,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }

        // A composition swap chain paints the window itself, so the redirection
        // surface GDI would otherwise allocate is pure waste -- and leaving it
        // out avoids an opaque white flash before the first frame.
        let ex_style = if cfg.translucent() {
            WS_EX_NOREDIRECTIONBITMAP
        } else {
            WINDOW_EX_STYLE::default()
        };

        // Provisional size; corrected once we know the cell metrics.
        let title = HSTRING::from(APP_NAME);
        let hwnd = CreateWindowExW(
            ex_style,
            WINDOW_CLASS,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1000,
            640,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        // Dark title bar to match the terminal body. Best effort: older builds
        // simply ignore the attribute.
        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );

        apply_backdrop(hwnd, cfg.window.backdrop);

        let dpi = GetDpiForWindow(hwnd).max(96);
        let dpi_scale = dpi as f32 / 96.0;

        crate::log!("window created, dpi={dpi}");
        let renderer = Renderer::new(hwnd, &cfg, dpi_scale)?;
        crate::log!(
            "renderer ready: cell {}x{} ascent {}",
            renderer.metrics().width,
            renderer.metrics().height,
            renderer.metrics().ascent
        );

        // Now that cell metrics exist, size the window to the configured grid.
        let m = renderer.metrics();
        let pad_x = (cfg.window.padding_x * dpi_scale).round() * 2.0;
        let pad_y = (cfg.window.padding_y * dpi_scale).round() * 2.0;
        let want_w = (cfg.window.cols as f32 * m.width + pad_x).round() as i32;
        let want_h = (cfg.window.rows as f32 * m.height + pad_y).round() as i32;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: want_w,
            bottom: want_h,
        };
        let _ = AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE::default());
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );

        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let (cw, ch) = (
            (client.right - client.left) as u32,
            (client.bottom - client.top) as u32,
        );
        // One tab to start, so the strip is hidden and the grid gets the full
        // client area.
        let (cols, rows) = renderer.grid_size_for(cw, ch, 1);

        let wake = CreateEventW(None, false, false, PCWSTR::null())?;
        let running = Arc::new(AtomicBool::new(true));
        let wake_h = SendHandle::new(wake);

        let first = Session::spawn(&cfg, cols, rows, wake_h, SendHwnd(hwnd))?;

        let mut app = Box::new(App {
            hwnd,
            renderer,
            sessions: vec![first],
            active: 0,
            cfg: cfg.clone(),
            wake: wake_h,
            running: Arc::clone(&running),
            focused: true,
            dpi_scale,
            client: (cw, ch),
            selection: None,
            mouse_held: None,
            mouse_cell: (usize::MAX, usize::MAX),
            blink_phase: true,
            last_blink: Instant::now(),
            needs_redraw: true,
            sync_since: None,
            sync_timeout: Duration::from_millis(cfg.render.sync_timeout_ms),
            min_frame_interval: (cfg.render.max_fps > 0)
                .then(|| Duration::from_secs_f64(1.0 / cfg.render.max_fps as f64)),
            last_present: Instant::now(),
            window_title: String::new(),
        });

        let app_ptr = &mut *app as *mut App;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(hwnd));

        // --- main loop ---
        let waitable = app.renderer.gpu().frame_latency_waitable;
        let mut msg = MSG::default();

        'main: loop {
            // Drain the message queue first: input should never wait behind a
            // frame.
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    crate::log!("WM_QUIT");
                    break 'main;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !running.load(Ordering::Relaxed) && msg.message == WM_QUIT {
                break;
            }

            app.reap_sessions();
            if app.sessions.is_empty() {
                break;
            }
            app.update_title();
            app.tick_blink();
            app.maybe_render();

            // Park until something needs us. The frame latency waitable only
            // joins the set when there is pending work; otherwise it stays
            // signalled and would spin the loop.
            let pending = app.needs_redraw || app.term().lock().is_dirty();
            let mut handles: Vec<HANDLE> = vec![wake];
            if pending {
                if let Some(h) = waitable {
                    handles.push(h);
                }
            }

            let mut timeout = if app.term().lock().cursor_state().blinking && app.focused {
                BLINK_INTERVAL.as_millis() as u32
            } else {
                1000
            };
            // A frame we chose not to present (synchronised output, or the fps
            // cap) has to be revisited soon, not on the blink timer.
            if app.needs_redraw {
                timeout = timeout.min(16);
            }

            MsgWaitForMultipleObjectsEx(
                Some(&handles),
                timeout,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }

        crate::log!("shutting down");
        running.store(false, Ordering::Relaxed);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(app);
        let _ = CloseHandle(wake);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::tab_label;

    #[test]
    fn tab_labels_use_the_leaf_of_a_path() {
        assert_eq!(
            tab_label("C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"),
            "powershell.exe"
        );
        assert_eq!(tab_label("/usr/bin/fish"), "fish");
        assert_eq!(tab_label("vim README.md"), "vim README.md");
        assert_eq!(tab_label("   "), "shell");
        assert_eq!(tab_label(""), "shell");
    }
}
