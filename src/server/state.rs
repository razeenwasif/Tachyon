//! Multiplexer server state tree (`Session -> Window -> Block`), per-client
//! bounded outbox with slow-client resync, background E-core VT parsing, and
//! JSON RPC handler.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::{Condvar, Mutex, RwLock};
use serde_json::{json, Value};

use crate::mux::model::{
    generate_session_name, BlockInfo, ClientInfo, CommandReceipt, HostInfo, LayoutNode,
    ServerSnapshot, SessionInfo, SplitDir, WindowInfo,
};
use crate::mux::proto::{Frame, PROTOCOL_VERSION};
use crate::mux::{local_hostname, unix_now};
use crate::term::{CommandEvent, Processor, ProgramState, ProgramStatus, Term, TermMode};

use super::alerts;
use super::persist::{
    self, PersistedPane, PersistedSession, PersistedState, PersistedWindow, RestoreReport,
};
use super::pty::{default_shell, find_integration_dir, integration, HostPty};
use super::receipts::ServerSampler;

/// Output the server-side VT parser may fall behind by before reading pauses.
const MAX_PARSE_QUEUED: usize = 32 << 20;

/// Maximum queued `PTY_OUT` bytes per attached client stream before dropping
/// stale intermediate frames and scheduling a clean `PTY_SNAPSHOT` resync.
const MAX_CLIENT_STREAM_BYTES: usize = 4 << 20;

/// Default scrollback kept by server-side terminals for capture & restore.
const DEFAULT_SCROLLBACK: usize = 10_000;

/// Number of recently exited block exit codes remembered for `block.wait`.
const RECENT_EXITS_CAP: usize = 256;

/// Number of completed command resource receipts retained per block.
const BLOCK_RECEIPTS_CAP: usize = 64;

/// Ordered, priority-aware outbound queue for one connected client.
///
/// Control frames (`RPC_RES`, `EVENT`, `PONG`, `PTY_SNAPSHOT`) live in
/// `priority` and are always written before `PTY_OUT` data frames.
pub struct ClientOutbox {
    pub id: String,
    pub kind: Mutex<String>,
    pub name: Mutex<String>,
    pub subscribed_events: AtomicBool,
    pub authenticated: AtomicBool,
    inner: Mutex<OutboxInner>,
    ready: Condvar,
    /// Blocks this client currently holds streams on: `stream_id -> (block_id, interactive)`.
    streams: Mutex<HashMap<u32, (String, bool)>>,
}

#[derive(Default)]
struct OutboxInner {
    priority: VecDeque<Frame>,
    pty: VecDeque<Frame>,
    stream_bytes: HashMap<u32, usize>,
    resync_needed: HashSet<u32>,
    closed: bool,
}

pub enum OutboxPop {
    Frame(Frame),
    Resync(u32),
    Closed,
}

impl ClientOutbox {
    pub fn new(id: String, authenticated: bool) -> Arc<Self> {
        Arc::new(Self {
            id,
            kind: Mutex::new("cli".into()),
            name: Mutex::new(String::new()),
            subscribed_events: AtomicBool::new(false),
            authenticated: AtomicBool::new(authenticated),
            inner: Mutex::new(OutboxInner::default()),
            ready: Condvar::new(),
            streams: Mutex::new(HashMap::new()),
        })
    }

    pub fn info(&self) -> ClientInfo {
        ClientInfo {
            id: self.id.clone(),
            kind: self.kind.lock().clone(),
            name: self.name.lock().clone(),
        }
    }

    /// Push a priority control/snapshot frame (never dropped by PTY floods).
    pub fn push_priority(&self, frame: Frame) {
        let mut q = self.inner.lock();
        if q.closed {
            return;
        }
        q.priority.push_back(frame);
        self.ready.notify_one();
    }

    /// Push live `PTY_OUT` bytes for `stream_id`. Drops stale frames for that
    /// stream and triggers a `PTY_SNAPSHOT` resync if the client falls behind.
    pub fn push_pty_out(&self, stream_id: u32, bytes: Vec<u8>) {
        let mut q = self.inner.lock();
        if q.closed {
            return;
        }
        if q.resync_needed.contains(&stream_id) {
            return;
        }
        let queued = q.stream_bytes.get(&stream_id).copied().unwrap_or(0);
        if queued + bytes.len() > MAX_CLIENT_STREAM_BYTES {
            // Drop stale PTY_OUT frames for this stream; a fresh snapshot will
            // catch the client up once the socket drains.
            q.pty.retain(|f| f.channel != stream_id);
            q.stream_bytes.insert(stream_id, 0);
            q.resync_needed.insert(stream_id);
            self.ready.notify_one();
            return;
        }
        *q.stream_bytes.entry(stream_id).or_insert(0) += bytes.len();
        q.pty.push_back(Frame::pty_out(stream_id, bytes));
        self.ready.notify_one();
    }

    /// Pop the next frame to write, or a stream requiring snapshot resync.
    pub fn pop_next(&self) -> OutboxPop {
        let mut q = self.inner.lock();
        loop {
            if let Some(f) = q.priority.pop_front() {
                return OutboxPop::Frame(f);
            }
            if let Some(f) = q.pty.pop_front() {
                if let Some(bytes) = q.stream_bytes.get_mut(&f.channel) {
                    *bytes = bytes.saturating_sub(f.payload.len());
                }
                return OutboxPop::Frame(f);
            }
            if let Some(&stream_id) = q.resync_needed.iter().next() {
                q.resync_needed.remove(&stream_id);
                return OutboxPop::Resync(stream_id);
            }
            if q.closed {
                return OutboxPop::Closed;
            }
            self.ready.wait(&mut q);
        }
    }

    pub fn close(&self) {
        let mut q = self.inner.lock();
        q.closed = true;
        self.ready.notify_all();
    }

    pub fn stream_block(&self, stream_id: u32) -> Option<(String, bool)> {
        self.streams.lock().get(&stream_id).cloned()
    }
}

struct StreamSub {
    client_id: String,
    stream_id: u32,
    outbox: Arc<ClientOutbox>,
}

struct ParsedBlock {
    term: Term,
    processor: Processor,
    rewind: crate::term::rewind::RewindLog,
    size: (u16, u16),
    last_title_rev: u64,
    last_status_rev: u64,
    awake: bool,
    agent_tracker: crate::mux::agents::AgentTracker,
    inferred_status: Option<crate::term::ProgramStatus>,
    last_agent_eval: Option<std::time::Instant>,
    last_input_seq: u64,
    pending_cmd_events: Vec<(CommandEvent, Option<String>)>,
}

impl ParsedBlock {
    fn feed(&mut self, pty: &HostPty, chunk: &[u8]) -> (Vec<u8>, bool, bool, Vec<(CommandEvent, Option<String>)>) {
        let ws = pty.size();
        if ws != self.size {
            self.size = ws;
            let (c, r) = (ws.0.max(1) as usize, ws.1.max(1) as usize);
            self.term.resize(c, r);
            self.rewind.record_resize(c, r);
        }
        self.rewind.record(chunk);
        self.processor.advance(&mut self.term, chunk);
        if self.rewind.wants_snapshot() && self.processor.is_ground() {
            self.rewind.snapshot(self.term.rewind_snapshot());
        }
        let mut cmd_events = Vec::new();
        for ev in self.term.take_command_events() {
            let cmdline = match ev {
                CommandEvent::Started(id) | CommandEvent::Finished(id, _) => {
                    self.term.command(id).and_then(|c| c.cmdline.clone())
                }
            };
            cmd_events.push((ev, cmdline));
        }
        self.term.take_dirty();
        let rev = self.term.title_revision;
        let meta_changed = rev != self.last_title_rev;
        self.last_title_rev = rev;
        let srev = self.term.status_revision;
        let status_changed = srev != self.last_status_rev;
        self.last_status_rev = srev;
        (self.term.take_replies(), meta_changed, status_changed, cmd_events)
    }

    fn evaluate_agent(&mut self, block: &Block, now: std::time::Instant) -> (bool, Option<Duration>) {
        self.last_agent_eval = Some(now);
        if self.term.program_status.is_some() {
            self.agent_tracker.reset();
            let changed = self.inferred_status.take().is_some();
            return (changed, None);
        }
        let fg = block.pty.foreground_process(block.is_login_shell);
        let cmd_running = self.term.command_running()
            || (!block.is_login_shell && block.alive.load(Ordering::Acquire));
        let cmdline = self
            .term
            .running_cmdline()
            .or(block.spawn_cmdline.as_deref());
        let label_guard = block.label.lock();
        let lines = crate::mux::agents::extract_visible_lines(
            &self.term,
            crate::mux::agents::MAX_SCREEN_LINES,
        );
        let next = self
            .agent_tracker
            .evaluate(&crate::mux::agents::AgentObservation {
                fg: fg.as_ref(),
                cmdline,
                title: self.term.title.as_deref(),
                label: label_guard.as_deref(),
                command_running: cmd_running,
                alt_screen: self.term.in_alt_screen(),
                screen_lines: &lines,
                now,
            });
        drop(label_guard);
        let changed = self.inferred_status != next;
        self.inferred_status = next;
        let settle = self.agent_tracker.next_settle_deadline(now);
        (changed, settle)
    }
}

pub struct Block {
    pub id: String,
    pub session_id: String,
    pub window_id: String,
    pub label: Mutex<Option<String>>,
    pub created_at: u64,
    pub is_login_shell: bool,
    pub spawn_cmdline: Option<String>,
    pub input_seq: AtomicU64,
    pub pty: Arc<HostPty>,
    subscribers: Mutex<Vec<StreamSub>>,
    attached_count: AtomicUsize,
    queue: Mutex<(VecDeque<Vec<u8>>, usize, bool)>,
    work: Condvar,
    space: Condvar,
    parsed: Mutex<ParsedBlock>,
    alive: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    /// Keep block record around after exit (e.g. for `run` blocks inspected shortly after).
    keep_after_exit: bool,
    handoff: Mutex<Option<HandoffState>>,
    receipt_sampler: Arc<ServerSampler>,
    receipts: Mutex<VecDeque<CommandReceipt>>,
    started_at: Instant,
    has_pending_cmd: AtomicBool,
}

/// Wait target for `tachyon wait --until ...` / `block.wait` RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitCondition {
    Exit,
    State(ProgramState),
}

impl WaitCondition {
    pub fn parse(raw: &str) -> Option<Self> {
        let low = raw.trim().to_ascii_lowercase();
        if matches!(low.as_str(), "exit" | "exited") {
            Some(Self::Exit)
        } else {
            ProgramState::parse(&low).map(Self::State)
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exit => "exit",
            Self::State(st) => st.as_str(),
        }
    }
}

#[derive(Clone, Debug)]
struct WaitResult {
    matched: String,
    exit_code: Option<i32>,
    exited: bool,
    status: Option<ProgramStatus>,
    status_inferred: bool,
}

impl WaitResult {
    fn to_json(&self, block_id: &str) -> Value {
        json!({
            "block_id": block_id,
            "matched": self.matched,
            "exited": self.exited,
            "exit_code": self.exit_code,
            "status": self.status.as_ref().map(crate::mux::model::status_to_json),
            "status_inferred": self.status_inferred,
        })
    }
}

struct StateWaiter {
    id: u64,
    targets: Vec<WaitCondition>,
    tx: Sender<WaitResult>,
}

#[derive(Clone, Debug)]
pub struct HandoffConfig {
    pub report_to: String,
    pub from_label: Option<String>,
    pub notify_on: Vec<WaitCondition>,
    pub capture_lines: usize,
    pub send_enter: bool,
}

#[derive(Clone, Debug)]
struct HandoffState {
    config: HandoffConfig,
    last_reported: Option<String>,
    reported_exit: bool,
}

impl HandoffState {
    fn from_config(config: HandoffConfig) -> Self {
        Self {
            config,
            last_reported: None,
            reported_exit: false,
        }
    }

    fn to_json(&self) -> Value {
        let on: Vec<&'static str> = self.config.notify_on.iter().map(|c| c.as_str()).collect();
        json!({
            "report_to": self.config.report_to,
            "from": self.config.from_label,
            "notify_on": on,
            "capture_lines": self.config.capture_lines,
            "send_enter": self.config.send_enter,
        })
    }
}

impl Block {
    pub fn info(&self) -> BlockInfo {
        let p = self.parsed.lock();
        let (cols, rows) = (p.term.cols() as u16, p.term.rows() as u16);
        let cmd = p.term.running_cmdline().map(str::to_string);
        let explicit = p.term.program_status.clone();
        let status_inferred = explicit.is_none() && p.inferred_status.is_some();
        let status = explicit.or_else(|| p.inferred_status.clone());
        BlockInfo {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            window_id: self.window_id.clone(),
            label: self.label.lock().clone(),
            title: p.term.title.clone(),
            cwd: p.term.cwd.clone(),
            command_running: cmd.is_some(),
            command: cmd,
            pid: self.pty.pid(),
            cols,
            rows,
            alive: self.alive.load(Ordering::Acquire),
            exit_code: *self.exit_code.lock(),
            awake: p.awake,
            attached_clients: self.attached_count.load(Ordering::Acquire),
            created_at: self.created_at,
            status,
            status_inferred,
            last_receipt: self.receipts.lock().back().cloned(),
        }
    }

    fn record_receipt(&self, r: CommandReceipt) {
        let mut q = self.receipts.lock();
        while q.len() >= BLOCK_RECEIPTS_CAP {
            q.pop_front();
        }
        q.push_back(r);
    }

    fn receipts_list(&self, limit: usize) -> Vec<CommandReceipt> {
        let q = self.receipts.lock();
        let take = limit.min(q.len());
        q.iter().skip(q.len() - take).cloned().collect()
    }

    /// Drain all queued output chunks into `parsed.term` (caller holds `parsed` lock).
    fn drain_queue_into(&self, parsed: &mut ParsedBlock) {
        let pending: Vec<Vec<u8>> = {
            let mut q = self.queue.lock();
            q.1 = 0;
            q.0.drain(..).collect()
        };
        self.space.notify_all();
        for chunk in pending {
            parsed.term.unlink_temp_images = self.attached_count.load(Ordering::Acquire) == 0;
            let (_, _, _, evs) = parsed.feed(&self.pty, &chunk);
            if !evs.is_empty() {
                parsed.pending_cmd_events.extend(evs);
            }
        }
        if !parsed.pending_cmd_events.is_empty() {
            self.has_pending_cmd.store(true, Ordering::Release);
            self.work.notify_one();
        }
    }

    /// Capture screen + optional trailing scrollback lines as plain text or VT ANSI.
    pub fn capture(&self, lines: Option<usize>, ansi: bool) -> String {
        let mut parsed = self.parsed.lock();
        self.drain_queue_into(&mut parsed);
        if ansi {
            return String::from_utf8_lossy(&parsed.term.to_vt()).into_owned();
        }
        let g = parsed.term.grid();
        let total = g.total_lines();
        if total == 0 {
            return String::new();
        }
        // Trim trailing blank rows below the last written content first so
        // `--lines N` returns the last N populated lines rather than empty rows.
        let mut last = total;
        while last > 0 && parsed.term.line_text(last - 1).is_empty() {
            last -= 1;
        }
        let rows = parsed.term.rows();
        let want = lines.unwrap_or(rows).min(last);
        let start = last.saturating_sub(want);
        let out_lines: Vec<String> = (start..last).map(|i| parsed.term.line_text(i)).collect();
        out_lines.join("\n")
    }
}

struct WindowRecord {
    id: String,
    session_id: String,
    name: Option<String>,
    layout: LayoutNode,
    focused_block: String,
    zoomed: bool,
    created_at: u64,
}

impl WindowRecord {
    fn info(&self) -> WindowInfo {
        WindowInfo {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            name: self.name.clone(),
            layout: self.layout.clone(),
            focused_block: self.focused_block.clone(),
            zoomed: self.zoomed,
            created_at: self.created_at,
        }
    }
}

struct SessionRecord {
    id: String,
    name: String,
    windows: Vec<String>,
    active_window: Option<String>,
    created_at: u64,
}

impl SessionRecord {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            windows: self.windows.clone(),
            active_window: self.active_window.clone(),
            created_at: self.created_at,
        }
    }
}

struct StateInner {
    sessions: BTreeMap<String, SessionRecord>,
    windows: BTreeMap<String, WindowRecord>,
    blocks: BTreeMap<String, Arc<Block>>,
    clients: HashMap<String, Arc<ClientOutbox>>,
    recent_exits: VecDeque<(String, i32)>,
    state_waiters: HashMap<String, Vec<StateWaiter>>,
}

/// Control action returned by `ServerState::handle_rpc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcOutcome {
    Continue,
    /// Close this client connection after flushing the `RPC_RES` (e.g. failed auth).
    CloseClient,
    /// Shut down the entire server after flushing the `RPC_RES`.
    ShutdownServer,
}

/// Configuration for on-disk session persistence & agent auto-resume.
#[derive(Clone, Debug)]
pub struct PersistOptions {
    pub path: Option<PathBuf>,
    pub auto_resume: bool,
}

impl Default for PersistOptions {
    fn default() -> Self {
        Self {
            path: None,
            auto_resume: true,
        }
    }
}

#[derive(Default)]
struct PersistSignal {
    dirty: bool,
    shutdown: bool,
}

struct PersistCoordinator {
    path: Option<PathBuf>,
    auto_resume: bool,
    signal: Mutex<PersistSignal>,
    wake: Condvar,
    save_lock: Mutex<()>,
    last_saved_at: AtomicU64,
    last_error: Mutex<Option<String>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl PersistCoordinator {
    fn new(opts: PersistOptions) -> Self {
        Self {
            path: opts.path,
            auto_resume: opts.auto_resume,
            signal: Mutex::new(PersistSignal::default()),
            wake: Condvar::new(),
            save_lock: Mutex::new(()),
            last_saved_at: AtomicU64::new(0),
            last_error: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }
}

pub struct ServerState {
    host_info: HostInfo,
    auth_token: RwLock<Option<String>>,
    token_file: Option<PathBuf>,
    next_id: AtomicU64,
    shutdown: AtomicBool,
    inner: Mutex<StateInner>,
    persist: PersistCoordinator,
    alerts: Arc<alerts::AlertManager>,
}

impl Drop for ServerState {
    fn drop(&mut self) {
        self.persist.signal.lock().shutdown = true;
        self.persist.wake.notify_all();
    }
}

impl ServerState {
    pub fn new() -> Arc<Self> {
        Self::with_auth_token(None)
    }

    pub fn with_auth_token(auth_token: Option<String>) -> Arc<Self> {
        Self::with_options(auth_token, PersistOptions::default())
    }

    pub fn with_options(auth_token: Option<String>, persist: PersistOptions) -> Arc<Self> {
        Self::with_full_options(
            auth_token,
            None,
            None,
            persist,
            alerts::AlertConfig::default(),
        )
    }

    pub fn with_full_options(
        auth_token: Option<String>,
        token_file: Option<PathBuf>,
        listen_addr: Option<String>,
        persist: PersistOptions,
        alert_config: alerts::AlertConfig,
    ) -> Arc<Self> {
        if let Some(root) = server_rewind_root() {
            crate::term::rewind::remove_stale(&root);
        }
        let spawn_worker = persist.path.is_some();
        let state = Arc::new(Self {
            host_info: HostInfo {
                hostname: local_hostname(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                version: crate::APP_VERSION.to_string(),
                protocol: PROTOCOL_VERSION,
                pid: std::process::id(),
                build: Some(crate::APP_BUILD_SHA.to_string()),
                capabilities: crate::LOCAL_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                auth_enabled: Some(auth_token.is_some()),
                listen_addr,
            },
            auth_token: RwLock::new(auth_token),
            token_file,
            next_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
            inner: Mutex::new(StateInner {
                sessions: BTreeMap::new(),
                windows: BTreeMap::new(),
                blocks: BTreeMap::new(),
                clients: HashMap::new(),
                recent_exits: VecDeque::new(),
                state_waiters: HashMap::new(),
            }),
            persist: PersistCoordinator::new(persist),
            alerts: Arc::new(alerts::AlertManager::new(alert_config)),
        });
        if spawn_worker {
            let weak: Weak<ServerState> = Arc::downgrade(&state);
            if let Ok(handle) = std::thread::Builder::new()
                .name("tachyon-srv-persist".into())
                .spawn(move || persist_worker_loop(weak))
            {
                *state.persist.worker.lock() = Some(handle);
            }
        }
        state
    }

    pub fn host_info(&self) -> &HostInfo {
        &self.host_info
    }

    fn current_host_info(&self) -> HostInfo {
        let mut h = self.host_info.clone();
        h.auth_enabled = Some(self.auth_token.read().is_some());
        h
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Initiate orderly daemon shutdown and wake the persistence writer thread.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.persist.signal.lock().shutdown = true;
        self.persist.wake.notify_all();
    }

    /// Request shutdown and wait for the background persistence writer thread to finish.
    pub fn stop_persist_worker(&self) {
        self.request_shutdown();
        if let Some(h) = self.persist.worker.lock().take() {
            let _ = h.join();
        }
    }

    /// Schedule an event-driven debounced snapshot write (zero idle wake-ups).
    pub fn mark_persist_dirty(&self) {
        if self.persist.path.is_none() || self.is_shutting_down() {
            return;
        }
        let mut sig = self.persist.signal.lock();
        if !sig.shutdown {
            sig.dirty = true;
            self.persist.wake.notify_one();
        }
    }

    pub fn persist_info_json(&self) -> Value {
        json!({
            "enabled": self.persist.path.is_some(),
            "path": self.persist.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "auto_resume": self.persist.auto_resume,
            "last_saved_at": self.persist.last_saved_at.load(Ordering::Acquire),
            "last_error": self.persist.last_error.lock().clone(),
        })
    }

    /// Capture a portable, atomic snapshot of all live sessions, splits, directories,
    /// and currently running coding agents without holding `self.inner` across block locks.
    pub fn capture_persisted_state(&self) -> PersistedState {
        struct WinSnap {
            name: Option<String>,
            zoomed: bool,
            layout: LayoutNode,
            focused_block: String,
            blocks: Vec<Arc<Block>>,
        }
        struct SessSnap {
            name: String,
            active_window_index: usize,
            windows: Vec<WinSnap>,
        }

        let captured: Vec<SessSnap> = {
            let st = self.inner.lock();
            let mut sessions: Vec<&SessionRecord> = st.sessions.values().collect();
            sessions.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
            sessions
                .into_iter()
                .filter_map(|sess| {
                    let mut windows = Vec::new();
                    let mut active_window_index = 0usize;
                    for wid in &sess.windows {
                        let Some(win) = st.windows.get(wid) else {
                            continue;
                        };
                        let leaf_ids = win.layout.leaves();
                        let blocks: Vec<Arc<Block>> = leaf_ids
                            .iter()
                            .filter_map(|bid| st.blocks.get(bid).cloned())
                            .collect();
                        if blocks.is_empty() {
                            continue;
                        }
                        if sess.active_window.as_deref() == Some(wid.as_str()) {
                            active_window_index = windows.len();
                        }
                        windows.push(WinSnap {
                            name: win.name.clone(),
                            zoomed: win.zoomed,
                            layout: win.layout.clone(),
                            focused_block: win.focused_block.clone(),
                            blocks,
                        });
                    }
                    (!windows.is_empty()).then_some(SessSnap {
                        name: sess.name.clone(),
                        active_window_index,
                        windows,
                    })
                })
                .collect()
        };

        let mut persisted_sessions = Vec::with_capacity(captured.len());
        for sess in captured {
            let mut persisted_windows = Vec::with_capacity(sess.windows.len());
            for win in sess.windows {
                let mut block_index = HashMap::with_capacity(win.blocks.len());
                let mut focused_pane = 0usize;
                let mut panes = Vec::with_capacity(win.blocks.len());
                for (idx, block) in win.blocks.iter().enumerate() {
                    block_index.insert(block.id.clone(), idx);
                    if block.id == win.focused_block {
                        focused_pane = idx;
                    }
                    panes.push(capture_persisted_pane(block));
                }
                let Some(layout) = persist::layout_to_persisted(&win.layout, &block_index) else {
                    continue;
                };
                persisted_windows.push(PersistedWindow {
                    name: win.name,
                    zoomed: win.zoomed,
                    layout,
                    focused_pane,
                    panes,
                });
            }
            if !persisted_windows.is_empty() {
                let active_idx = sess
                    .active_window_index
                    .min(persisted_windows.len().saturating_sub(1));
                persisted_sessions.push(PersistedSession {
                    name: sess.name,
                    active_window_index: active_idx,
                    windows: persisted_windows,
                });
            }
        }

        PersistedState {
            version: persist::STATE_VERSION,
            saved_at: unix_now(),
            hostname: self.host_info.hostname.clone(),
            sessions: persisted_sessions,
        }
    }

    /// Immediately capture and atomically flush the current state to disk (when enabled).
    pub fn flush_persist_now(&self) -> Result<(Option<PathBuf>, PersistedState), String> {
        let snap = self.capture_persisted_state();
        if let Some(path) = self.persist.path.as_deref() {
            let _guard = self.persist.save_lock.lock();
            if let Err(e) = persist::save_atomic(path, &snap) {
                let msg = format!("save {}: {e}", path.display());
                *self.persist.last_error.lock() = Some(msg.clone());
                return Err(msg);
            }
            self.persist.last_saved_at.store(snap.saved_at, Ordering::Release);
            *self.persist.last_error.lock() = None;
        }
        Ok((self.persist.path.clone(), snap))
    }

    /// Restore persisted sessions, windows, split layouts, working directories,
    /// and curated active coding agents from `path_override` or the configured state file.
    pub fn restore_from_disk(
        self: &Arc<Self>,
        path_override: Option<&Path>,
        auto_resume_override: Option<bool>,
    ) -> Result<RestoreReport, String> {
        let Some(path) = path_override.or(self.persist.path.as_deref()) else {
            return Ok(RestoreReport::default());
        };
        let Some(persisted) = persist::load(path).map_err(|e| format!("load {}: {e}", path.display()))? else {
            return Ok(RestoreReport::default());
        };
        let allow_resume = auto_resume_override.unwrap_or(self.persist.auto_resume);
        let existing_names: HashSet<String> = {
            let st = self.inner.lock();
            st.sessions.values().map(|s| s.name.clone()).collect()
        };

        let mut report = RestoreReport::default();
        let mut created_events = Vec::new();
        let now = unix_now();

        for sess in persisted.sessions {
            if sess.windows.is_empty() || existing_names.contains(&sess.name) {
                continue;
            }
            let session_id = self.alloc_id("s");
            let mut window_ids = Vec::new();
            let mut inserted_windows = Vec::new();
            let mut inserted_blocks = Vec::new();

            for win in &sess.windows {
                if win.panes.is_empty() {
                    continue;
                }
                let window_id = self.alloc_id("w");
                let mut block_ids = Vec::with_capacity(win.panes.len());
                for pane in &win.panes {
                    let expanded = pane.cwd.as_deref().map(expand_tilde);
                    let cwd_exists = expanded
                        .as_deref()
                        .is_some_and(|p| Path::new(p).is_dir());
                    let effective_cwd = if cwd_exists { expanded.as_deref() } else { None };
                    // Security invariant: never execute raw `resume_cmd` text from disk;
                    // always resolve against the compiled `AgentManifest` allowlist.
                    let initial_run = if allow_resume && cwd_exists {
                        pane.agent_id
                            .as_deref()
                            .and_then(crate::mux::agents::resume_command_for_agent)
                    } else {
                        None
                    };
                    match self.spawn_block_with_options(
                        &session_id,
                        &window_id,
                        pane.cols,
                        pane.rows,
                        effective_cwd,
                        None,
                        pane.label.clone(),
                        false,
                        initial_run,
                        pane.awake,
                        None,
                    ) {
                        Ok(block) => {
                            if initial_run.is_some() {
                                report.resumed_agents += 1;
                            }
                            block_ids.push(block.id.clone());
                            inserted_blocks.push(block);
                        }
                        Err(e) => {
                            crate::log!("mux persist restore pane failed: {e}");
                        }
                    }
                }
                if block_ids.is_empty() {
                    continue;
                }
                let layout = persist::materialize_layout(&win.layout, &block_ids)
                    .unwrap_or_else(|| LayoutNode::Leaf(block_ids[0].clone()));
                let focused_block = block_ids
                    .get(win.focused_pane)
                    .cloned()
                    .unwrap_or_else(|| block_ids[0].clone());
                let win_rec = WindowRecord {
                    id: window_id.clone(),
                    session_id: session_id.clone(),
                    name: win.name.clone(),
                    layout,
                    focused_block,
                    zoomed: win.zoomed,
                    created_at: now,
                };
                window_ids.push(window_id);
                inserted_windows.push(win_rec);
            }

            if inserted_windows.is_empty() {
                continue;
            }
            let active_idx = sess
                .active_window_index
                .min(window_ids.len().saturating_sub(1));
            let active_window = Some(window_ids[active_idx].clone());
            let sess_rec = SessionRecord {
                id: session_id.clone(),
                name: sess.name,
                windows: window_ids,
                active_window,
                created_at: now,
            };

            let mut win_infos = Vec::with_capacity(inserted_windows.len());
            let sess_info = sess_rec.info();
            {
                let mut st = self.inner.lock();
                st.sessions.insert(session_id, sess_rec);
                for win_rec in inserted_windows {
                    win_infos.push(win_rec.info());
                    st.windows.insert(win_rec.id.clone(), win_rec);
                }
                for block in &inserted_blocks {
                    st.blocks.insert(block.id.clone(), Arc::clone(block));
                }
            }
            report.sessions += 1;
            report.windows += win_infos.len();
            report.panes += inserted_blocks.len();
            let block_infos: Vec<BlockInfo> = inserted_blocks.iter().map(|b| b.info()).collect();
            created_events.push((sess_info, win_infos, block_infos));
        }

        for (sess_info, win_infos, block_infos) in created_events {
            if let (Some(win_info), Some(block_info)) = (win_infos.first(), block_infos.first()) {
                self.broadcast_event(json!({
                    "event": "session.created",
                    "session": sess_info.to_json(),
                    "window": win_info.to_json(),
                    "block": block_info.to_json(),
                }));
            }
        }
        if report.panes > 0 {
            self.mark_persist_dirty();
        }
        Ok(report)
    }

    fn alloc_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}_{n:x}")
    }

    pub fn register_client(&self) -> Arc<ClientOutbox> {
        self.register_client_with_auth(true)
    }

    pub fn register_client_with_auth(&self, pre_authenticated: bool) -> Arc<ClientOutbox> {
        let id = self.alloc_id("c");
        let outbox = ClientOutbox::new(id.clone(), pre_authenticated);
        self.inner.lock().clients.insert(id, Arc::clone(&outbox));
        outbox
    }

    pub fn unregister_client(&self, outbox: &Arc<ClientOutbox>) {
        outbox.close();
        let attached: Vec<(u32, String)> = outbox
            .streams
            .lock()
            .drain()
            .map(|(sid, (bid, _))| (sid, bid))
            .collect();
        let blocks: Vec<Arc<Block>> = {
            let mut st = self.inner.lock();
            st.clients.remove(&outbox.id);
            attached
                .iter()
                .filter_map(|(_, bid)| st.blocks.get(bid).cloned())
                .collect()
        };
        for b in blocks {
            let mut subs = b.subscribers.lock();
            subs.retain(|s| s.client_id != outbox.id);
            b.attached_count.store(subs.len(), Ordering::Release);
        }
    }

    pub fn broadcast_event(&self, event: Value) {
        let Ok(frame) = Frame::event(&event) else { return };
        let targets: Vec<Arc<ClientOutbox>> = {
            let st = self.inner.lock();
            st.clients
                .values()
                .filter(|c| c.subscribed_events.load(Ordering::Acquire))
                .cloned()
                .collect()
        };
        for c in targets {
            c.push_priority(frame.clone());
        }
    }

    /// Generate a fresh LZ4 `PTY_SNAPSHOT` frame for a slow client catching up.
    pub fn resync_frame(&self, outbox: &ClientOutbox, stream_id: u32) -> Option<Frame> {
        let (block_id, _) = outbox.stream_block(stream_id)?;
        let block = self.inner.lock().blocks.get(&block_id).cloned()?;
        let mut parsed = block.parsed.lock();
        block.drain_queue_into(&mut parsed);
        let vt = parsed.term.to_vt();
        Some(Frame::pty_snapshot(stream_id, &vt))
    }

    /// Forward raw keystrokes/paste from `PTY_IN` directly to the attached block.
    pub fn handle_pty_in(&self, outbox: &ClientOutbox, stream_id: u32, bytes: &[u8]) {
        if !outbox.authenticated.load(Ordering::Acquire) {
            return;
        }
        let Some((block_id, interactive)) = outbox.stream_block(stream_id) else {
            return;
        };
        if !interactive || bytes.is_empty() {
            return;
        }
        let block = self.inner.lock().blocks.get(&block_id).cloned();
        if let Some(b) = block {
            if b.pty.write(bytes).is_ok() {
                b.input_seq.fetch_add(1, Ordering::Release);
            }
        }
    }

    /// Forward `PTY_RESIZE` from an interactive client to the attached block.
    pub fn handle_pty_resize(&self, outbox: &ClientOutbox, stream_id: u32, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
        if !outbox.authenticated.load(Ordering::Acquire) {
            return;
        }
        let Some((block_id, interactive)) = outbox.stream_block(stream_id) else {
            return;
        };
        if !interactive {
            return;
        }
        let block = self.inner.lock().blocks.get(&block_id).cloned();
        if let Some(b) = block {
            if cell_w > 0 && cell_h > 0 {
                b.parsed.lock().term.set_cell_pixels(cell_w as u32, cell_h as u32);
            }
            b.pty.resize(cols, rows, cell_w, cell_h);
        }
    }

    pub fn snapshot(&self) -> ServerSnapshot {
        let st = self.inner.lock();
        let sessions = st.sessions.values().map(SessionRecord::info).collect();
        let windows = st.windows.values().map(WindowRecord::info).collect();
        let blocks: Vec<Arc<Block>> = st.blocks.values().cloned().collect();
        let clients = st.clients.values().map(|c| c.info()).collect();
        drop(st);
        let block_infos = blocks.iter().map(|b| b.info()).collect();
        ServerSnapshot {
            host: self.current_host_info(),
            sessions,
            windows,
            blocks: block_infos,
            clients,
        }
    }

    /// Handle one JSON RPC request and enqueue its `RPC_RES` on `outbox`.
    pub fn handle_rpc(
        self: &Arc<Self>,
        outbox: &Arc<ClientOutbox>,
        req_id: u32,
        req: &Value,
    ) -> RpcOutcome {
        let op = req.get("op").and_then(Value::as_str).unwrap_or("");
        let is_authed = outbox.authenticated.load(Ordering::Acquire);
        if !is_authed && op != "hello" {
            let body = json!({
                "ok": false,
                "error": "unauthorized: authenticate via `hello` with bearer token first",
            });
            if let Ok(f) = Frame::rpc_res(req_id, &body) {
                outbox.push_priority(f);
            }
            return RpcOutcome::CloseClient;
        }

        // `block.attach` pushes RPC_RES + PTY_SNAPSHOT atomically under block locks.
        if op == "block.attach" {
            self.rpc_block_attach(outbox, req_id, req);
            return RpcOutcome::Continue;
        }
        // `block.wait`, `block.rewind.frame`, and `block.rewind.provenance` may
        // block or replay cold disk segments; run off the client reader loop so
        // live PTY I/O, pings, and events stay responsive.
        if matches!(
            op,
            "block.wait" | "block.rewind.frame" | "block.rewind.provenance"
        ) {
            let state = Arc::clone(self);
            let out = Arc::clone(outbox);
            let op_owned = op.to_string();
            let req_owned = req.clone();
            let _ = std::thread::Builder::new()
                .name("tachyon-srv-async-rpc".into())
                .spawn(move || {
                    let res = match op_owned.as_str() {
                        "block.wait" => state.rpc_block_wait(&req_owned),
                        "block.rewind.frame" => state.rpc_block_rewind_frame(&req_owned),
                        "block.rewind.provenance" => state.rpc_block_rewind_provenance(&req_owned),
                        _ => Err(format!("unknown op `{op_owned}`")),
                    };
                    let body = match res {
                        Ok(data) => json!({ "ok": true, "data": data }),
                        Err(err) => json!({ "ok": false, "error": err }),
                    };
                    if let Ok(f) = Frame::rpc_res(req_id, &body) {
                        out.push_priority(f);
                    }
                });
            return RpcOutcome::Continue;
        }
        if op == "server.shutdown" {
            let _ = self.flush_persist_now();
            self.request_shutdown();
            let res = json!({ "ok": true, "data": { "shutting_down": true } });
            if let Ok(f) = Frame::rpc_res(req_id, &res) {
                outbox.push_priority(f);
            }
            return RpcOutcome::ShutdownServer;
        }

        let result: Result<Value, String> = match op {
            "hello" => self.rpc_hello(outbox, req),
            "state.get" => Ok(self.snapshot().to_json()),
            "server.snapshot" => self.rpc_server_snapshot(),
            "server.persist_info" => Ok(self.persist_info_json()),
            "server.restore" => self.rpc_server_restore(req),
            "server.set_token" => self.rpc_server_set_token(req),
            "server.alerts.get" => Ok(self.alerts.config().to_json()),
            "server.alerts.set" => self.rpc_server_alerts_set(req),
            "server.alerts.test" => self.rpc_server_alerts_test(req),
            "events.subscribe" => {
                outbox.subscribed_events.store(true, Ordering::Release);
                Ok(json!({ "subscribed": true }))
            }
            "session.create" => self.rpc_session_create(req),
            "session.rename" => self.rpc_session_rename(req),
            "session.kill" => self.rpc_session_kill(req),
            "session.inspect" => self.rpc_session_inspect(req),
            "window.create" => self.rpc_window_create(req),
            "window.rename" => self.rpc_window_rename(req),
            "window.kill" => self.rpc_window_kill(req),
            "window.focus" => self.rpc_window_focus(req),
            "window.set_zoom" => self.rpc_window_set_zoom(req),
            "window.set_ratio" => self.rpc_window_set_ratio(req),
            "block.split" => self.rpc_block_split(req),
            "block.kill" => self.rpc_block_kill(req),
            "block.inspect" => self.rpc_block_inspect(req),
            "block.set_awake" => self.rpc_block_set_awake(req),
            "block.set_handoff" => self.rpc_block_set_handoff(req),
            "block.detach" => self.rpc_block_detach(outbox, req),
            "block.send" => self.rpc_block_send(req),
            "block.capture" => self.rpc_block_capture(req),
            "block.receipts" => self.rpc_block_receipts(req),
            "block.rewind.list" => self.rpc_block_rewind_list(req),
            "block.rewind.frame" => self.rpc_block_rewind_frame(req),
            "block.rewind.provenance" => self.rpc_block_rewind_provenance(req),
            "block.wait" => self.rpc_block_wait(req),
            _ => Err(format!("unknown op `{op}`")),
        };

        let mutates_persist = matches!(
            op,
            "session.create"
                | "session.rename"
                | "session.kill"
                | "window.create"
                | "window.rename"
                | "window.kill"
                | "window.focus"
                | "window.set_zoom"
                | "window.set_ratio"
                | "block.split"
                | "block.kill"
                | "block.set_awake"
        );
        if result.is_ok() && mutates_persist {
            self.mark_persist_dirty();
        }

        let failed_auth = op == "hello" && result.is_err() && !outbox.authenticated.load(Ordering::Acquire);
        let body = match result {
            Ok(data) => json!({ "ok": true, "data": data }),
            Err(err) => json!({ "ok": false, "error": err }),
        };
        if let Ok(f) = Frame::rpc_res(req_id, &body) {
            outbox.push_priority(f);
        }
        if failed_auth {
            RpcOutcome::CloseClient
        } else {
            RpcOutcome::Continue
        }
    }

    fn rpc_hello(&self, outbox: &Arc<ClientOutbox>, req: &Value) -> Result<Value, String> {
        let proto = req.get("protocol").and_then(Value::as_u64).unwrap_or(0) as u32;
        if proto != PROTOCOL_VERSION {
            return Err(format!(
                "protocol mismatch: client {proto}, server {PROTOCOL_VERSION}"
            ));
        }
        if !outbox.authenticated.load(Ordering::Acquire) {
            if let Some(expected) = self.auth_token.read().as_deref() {
                let supplied = req.get("token").and_then(Value::as_str).unwrap_or("");
                if !crate::mux::ct_eq(supplied.as_bytes(), expected.as_bytes()) {
                    return Err("unauthorized: invalid or missing bearer token".into());
                }
            }
            outbox.authenticated.store(true, Ordering::Release);
        }
        if let Some(k) = req.get("client_kind").and_then(Value::as_str) {
            *outbox.kind.lock() = k.to_string();
        }
        if let Some(n) = req.get("client_name").and_then(Value::as_str) {
            *outbox.name.lock() = n.to_string();
        }
        Ok(json!({
            "protocol": PROTOCOL_VERSION,
            "client_id": outbox.id,
            "host": self.current_host_info().to_json(),
        }))
    }

    fn rpc_server_snapshot(&self) -> Result<Value, String> {
        let (path, snap) = self.flush_persist_now()?;
        let windows: usize = snap.sessions.iter().map(|s| s.windows.len()).sum();
        let panes: usize = snap
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .map(|w| w.panes.len())
            .sum();
        Ok(json!({
            "saved": path.is_some(),
            "path": path.map(|p| p.to_string_lossy().into_owned()),
            "saved_at": snap.saved_at,
            "sessions": snap.sessions.len(),
            "windows": windows,
            "panes": panes,
        }))
    }

    fn rpc_server_restore(self: &Arc<Self>, req: &Value) -> Result<Value, String> {
        let path = req
            .get("state_file")
            .or_else(|| req.get("path"))
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let auto_resume = req.get("auto_resume").and_then(Value::as_bool);
        let report = self.restore_from_disk(path.as_deref(), auto_resume)?;
        Ok(report.to_json())
    }

    fn rpc_server_set_token(&self, req: &Value) -> Result<Value, String> {
        let regenerate = req.get("regenerate").and_then(Value::as_bool).unwrap_or(false);
        let new_token = if regenerate {
            crate::mux::generate_auth_token().map_err(|e| e.to_string())?
        } else if let Some(tok) = req
            .get("token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            tok.to_string()
        } else {
            crate::mux::generate_auth_token().map_err(|e| e.to_string())?
        };
        if let Some(ref path) = self.token_file {
            crate::mux::write_token_file(path, &new_token).map_err(|e| e.to_string())?;
        }
        *self.auth_token.write() = Some(new_token.clone());
        Ok(json!({
            "updated": true,
            "token": new_token,
            "path": self.token_file.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "auth_enabled": true,
        }))
    }

    fn rpc_server_alerts_set(&self, req: &Value) -> Result<Value, String> {
        let updated = self.alerts.update_config(req)?;
        Ok(updated.to_json())
    }

    fn rpc_server_alerts_test(&self, req: &Value) -> Result<Value, String> {
        let cfg = self.alerts.config();
        let state_str = req
            .get("state")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("blocked")
            .to_string();
        let msg = req
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "Test alert from Tachyon".into());
        let payload = alerts::AlertPayload {
            host: self.host_info.hostname.clone(),
            block_id: "test".into(),
            session_id: "test".into(),
            window_id: "test".into(),
            label: "test".into(),
            state: state_str,
            app: Some("tachyon".into()),
            message: Some(msg),
            cwd: None,
            inferred: false,
            interactive_clients: 0,
            timestamp_unix: unix_now(),
        };
        self.broadcast_event(payload.to_json());
        let delivery = alerts::dispatch_sync(&cfg, &payload);
        Ok(json!({
            "alert": payload.to_json(),
            "config": cfg.to_json(),
            "delivery": delivery.to_json(),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_block_with_options(
        self: &Arc<Self>,
        session_id: &str,
        window_id: &str,
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
        cmd: Option<&[String]>,
        label: Option<String>,
        keep_after_exit: bool,
        initial_run: Option<&str>,
        initial_awake: bool,
        handoff: Option<HandoffConfig>,
    ) -> Result<Arc<Block>, String> {
        let block_id = self.alloc_id("b");
        let is_login_shell = cmd.is_none_or(|parts| parts.is_empty());
        let spawn_cmdline = cmd
            .filter(|parts| !parts.is_empty())
            .map(|parts| parts.join(" "));
        let (program, args, login, mut extra_env, used_zsh_integration) = match cmd {
            Some(parts) if !parts.is_empty() => (
                parts[0].clone(),
                parts[1..].to_vec(),
                false,
                Vec::new(),
                false,
            ),
            _ => {
                let shell = default_shell();
                let int_dir = find_integration_dir();
                let maybe_int = match int_dir {
                    Some(ref d) => integration(&shell, d, true),
                    None => {
                        #[cfg(windows)]
                        {
                            integration(&shell, std::path::Path::new(""), true)
                        }
                        #[cfg(not(windows))]
                        {
                            None
                        }
                    }
                };
                let zsh_int = maybe_int.is_some()
                    && shell
                        .rsplit(['/', '\\'])
                        .next()
                        .is_some_and(|b| b.eq_ignore_ascii_case("zsh"));
                match maybe_int {
                    Some(i) => (shell, i.args, i.login, i.env, zsh_int),
                    None => (shell, Vec::new(), true, Vec::new(), false),
                }
            }
        };

        // Inject Rex-style context environment variables so CLI & coding agents
        // automatically know which host/session/window/block they inhabit.
        extra_env.push(("TACHYON_SERVER".into(), "local".into()));
        extra_env.push(("TACHYON_SESSION".into(), session_id.to_string()));
        extra_env.push(("TACHYON_WINDOW".into(), window_id.to_string()));
        extra_env.push(("TACHYON_BLOCK".into(), block_id.clone()));
        if let Some(run_cmd) = initial_run {
            if used_zsh_integration {
                extra_env.push(("TACHYON_RUN".into(), run_cmd.to_string()));
            }
        }

        let cols = cols.max(2);
        let rows = rows.max(1);
        let expanded_cwd = cwd.map(expand_tilde);
        let pty = HostPty::spawn(
            cols,
            rows,
            &program,
            &args,
            login,
            &extra_env,
            expanded_cwd.as_deref(),
        )
        .map_err(|e| format!("failed to spawn `{program}`: {e}"))?;

        if let Some(run_cmd) = initial_run {
            if !used_zsh_integration {
                let _ = pty.write(format!("{run_cmd}\r").as_bytes());
            }
        }

        let mut term = Term::new(cols as usize, rows as usize, DEFAULT_SCROLLBACK);
        term.cwd = expanded_cwd;
        if initial_awake {
            term.set_keep_awake(true);
        }

        let block = Arc::new(Block {
            id: block_id.clone(),
            session_id: session_id.to_string(),
            window_id: window_id.to_string(),
            label: Mutex::new(label),
            created_at: unix_now(),
            is_login_shell,
            spawn_cmdline: spawn_cmdline.clone(),
            input_seq: AtomicU64::new(0),
            receipt_sampler: {
                let sampler = Arc::new(ServerSampler::new(
                    pty.pid(),
                    pty.master_fd(),
                    !is_login_shell,
                ));
                if !is_login_shell {
                    sampler.start(Some(1), spawn_cmdline.clone());
                }
                sampler
            },
            receipts: Mutex::new(VecDeque::new()),
            started_at: Instant::now(),
            has_pending_cmd: AtomicBool::new(false),
            pty: Arc::new(pty),
            subscribers: Mutex::new(Vec::new()),
            attached_count: AtomicUsize::new(0),
            queue: Mutex::new((VecDeque::new(), 0, false)),
            work: Condvar::new(),
            space: Condvar::new(),
            parsed: Mutex::new(ParsedBlock {
                term,
                processor: Processor::new(),
                rewind: crate::term::rewind::RewindLog::new(
                    Term::new(cols as usize, rows as usize, 0),
                    server_rewind_dir(&block_id),
                ),
                size: (cols, rows),
                last_title_rev: 0,
                last_status_rev: 0,
                awake: initial_awake,
                agent_tracker: crate::mux::agents::AgentTracker::new(),
                inferred_status: None,
                last_agent_eval: None,
                last_input_seq: 0,
                pending_cmd_events: Vec::new(),
            }),
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            keep_after_exit,
            handoff: Mutex::new(handoff.map(HandoffState::from_config)),
        });

        let state_reader = Arc::clone(self);
        let block_reader = Arc::clone(&block);
        std::thread::Builder::new()
            .name(format!("tachyon-srv-rd-{block_id}"))
            .spawn(move || read_block_loop(state_reader, block_reader))
            .map_err(|e| format!("spawn reader thread: {e}"))?;

        // On Windows ConPTY, `conhost.exe` keeps the output pipe open even after
        // the child process exits until `ClosePseudoConsole` is called. Watch the
        // child handle and close the pseudoconsole cleanly once it exits.
        #[cfg(windows)]
        {
            let exit_block = Arc::clone(&block);
            let _ = std::thread::Builder::new()
                .name(format!("tachyon-srv-exit-{block_id}"))
                .spawn(move || {
                    let _ = exit_block.pty.wait_exit();
                    std::thread::sleep(Duration::from_millis(30));
                    exit_block.pty.hangup();
                });
        }

        let state_parser = Arc::clone(self);
        let block_parser = Arc::clone(&block);
        std::thread::Builder::new()
            .name(format!("tachyon-srv-vt-{block_id}"))
            .spawn(move || {
                #[cfg(target_os = "macos")]
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
                }
                parse_block_loop(state_parser, block_parser);
            })
            .map_err(|e| format!("spawn parser thread: {e}"))?;

        Ok(block)
    }

    fn resolve_session_id(&self, name_or_id: &str) -> Option<String> {
        let st = self.inner.lock();
        if st.sessions.contains_key(name_or_id) {
            return Some(name_or_id.to_string());
        }
        st.sessions
            .values()
            .find(|s| s.name == name_or_id)
            .map(|s| s.id.clone())
    }

    fn rpc_session_create(self: &Arc<Self>, req: &Value) -> Result<Value, String> {
        let cols = req.get("cols").and_then(Value::as_u64).unwrap_or(120) as u16;
        let rows = req.get("rows").and_then(Value::as_u64).unwrap_or(32) as u16;
        let cwd = req.get("cwd").and_then(Value::as_str);
        let cmd = parse_cmd_array(req.get("cmd"));
        let label = req.get("label").and_then(Value::as_str).map(str::to_string);
        let requested_name = req
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let session_id = self.alloc_id("s");
        let window_id = self.alloc_id("w");
        let name = match requested_name {
            Some(n) => n,
            None => {
                let st = self.inner.lock();
                let seed = self.next_id.load(Ordering::Relaxed) ^ unix_now();
                generate_session_name(seed, |cand| st.sessions.values().any(|s| s.name == cand))
            }
        };

        let handoff = parse_handoff_config(req.get("handoff"))?;
        let keep = cmd.is_some() || handoff.is_some();
        let block = self.spawn_block_with_options(
            &session_id,
            &window_id,
            cols,
            rows,
            cwd,
            cmd.as_deref(),
            label,
            keep,
            None,
            false,
            handoff,
        )?;
        let block_id = block.id.clone();

        let (session_info, window_info) = {
            let mut st = self.inner.lock();
            let now = unix_now();
            let win = WindowRecord {
                id: window_id.clone(),
                session_id: session_id.clone(),
                name: None,
                layout: LayoutNode::Leaf(block_id.clone()),
                focused_block: block_id.clone(),
                zoomed: false,
                created_at: now,
            };
            let sess = SessionRecord {
                id: session_id.clone(),
                name,
                windows: vec![window_id.clone()],
                active_window: Some(window_id.clone()),
                created_at: now,
            };
            let si = sess.info();
            let wi = win.info();
            st.sessions.insert(session_id, sess);
            st.windows.insert(window_id, win);
            st.blocks.insert(block_id, Arc::clone(&block));
            (si, wi)
        };

        let block_info = block.info();
        self.broadcast_event(json!({
            "event": "session.created",
            "session": session_info.to_json(),
            "window": window_info.to_json(),
            "block": block_info.to_json(),
        }));

        Ok(json!({
            "session": session_info.to_json(),
            "window": window_info.to_json(),
            "block": block_info.to_json(),
        }))
    }

    fn rpc_session_rename(&self, req: &Value) -> Result<Value, String> {
        let key = req
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or("missing session_id")?;
        let name = req
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("missing name")?;
        let sid = self.resolve_session_id(key).ok_or_else(|| format!("no session `{key}`"))?;
        let info = {
            let mut st = self.inner.lock();
            let s = st.sessions.get_mut(&sid).ok_or("no such session")?;
            s.name = name.to_string();
            s.info()
        };
        self.broadcast_event(json!({
            "event": "session.renamed",
            "session": info.to_json(),
        }));
        Ok(json!({ "session": info.to_json() }))
    }

    fn rpc_session_kill(&self, req: &Value) -> Result<Value, String> {
        let key = req
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or("missing session_id")?;
        let sid = self.resolve_session_id(key).ok_or_else(|| format!("no session `{key}`"))?;
        let blocks_to_kill: Vec<Arc<Block>> = {
            let mut st = self.inner.lock();
            let Some(sess) = st.sessions.remove(&sid) else {
                return Err(format!("no session `{key}`"));
            };
            let mut victims = Vec::new();
            for wid in sess.windows {
                if let Some(win) = st.windows.remove(&wid) {
                    for bid in win.layout.leaves() {
                        if let Some(b) = st.blocks.remove(&bid) {
                            victims.push(b);
                        }
                    }
                }
            }
            victims
        };
        for b in blocks_to_kill {
            b.pty.hangup();
        }
        self.broadcast_event(json!({ "event": "session.killed", "session_id": sid }));
        Ok(json!({ "killed": sid }))
    }

    fn rpc_session_inspect(&self, req: &Value) -> Result<Value, String> {
        let key = req
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or("missing session_id")?;
        let sid = self.resolve_session_id(key).ok_or_else(|| format!("no session `{key}`"))?;
        let st = self.inner.lock();
        let sess = st.sessions.get(&sid).ok_or("no such session")?.info();
        let windows: Vec<WindowInfo> = sess
            .windows
            .iter()
            .filter_map(|wid| st.windows.get(wid).map(WindowRecord::info))
            .collect();
        let blocks: Vec<Arc<Block>> = windows
            .iter()
            .flat_map(|w| w.layout.leaves())
            .filter_map(|bid| st.blocks.get(&bid).cloned())
            .collect();
        drop(st);
        Ok(json!({
            "session": sess.to_json(),
            "windows": windows.iter().map(WindowInfo::to_json).collect::<Vec<_>>(),
            "blocks": blocks.iter().map(|b| b.info().to_json()).collect::<Vec<_>>(),
        }))
    }

    fn rpc_window_create(self: &Arc<Self>, req: &Value) -> Result<Value, String> {
        let key = req
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or("missing session_id")?;
        let sid = self.resolve_session_id(key).ok_or_else(|| format!("no session `{key}`"))?;
        let cols = req.get("cols").and_then(Value::as_u64).unwrap_or(120) as u16;
        let rows = req.get("rows").and_then(Value::as_u64).unwrap_or(32) as u16;
        let cwd = req.get("cwd").and_then(Value::as_str);
        let cmd = parse_cmd_array(req.get("cmd"));
        let name = req.get("name").and_then(Value::as_str).map(str::to_string);
        let label = req.get("label").and_then(Value::as_str).map(str::to_string);

        let window_id = self.alloc_id("w");
        let handoff = parse_handoff_config(req.get("handoff"))?;
        let keep = cmd.is_some() || handoff.is_some();
        let block = self.spawn_block_with_options(
            &sid,
            &window_id,
            cols,
            rows,
            cwd,
            cmd.as_deref(),
            label,
            keep,
            None,
            false,
            handoff,
        )?;
        let block_id = block.id.clone();

        let window_info = {
            let mut st = self.inner.lock();
            let sess = st.sessions.get_mut(&sid).ok_or("session vanished")?;
            sess.windows.push(window_id.clone());
            sess.active_window = Some(window_id.clone());
            let win = WindowRecord {
                id: window_id.clone(),
                session_id: sid.clone(),
                name,
                layout: LayoutNode::Leaf(block_id.clone()),
                focused_block: block_id.clone(),
                zoomed: false,
                created_at: unix_now(),
            };
            let wi = win.info();
            st.windows.insert(window_id, win);
            st.blocks.insert(block_id, Arc::clone(&block));
            wi
        };

        let block_info = block.info();
        self.broadcast_event(json!({
            "event": "window.created",
            "window": window_info.to_json(),
            "block": block_info.to_json(),
        }));
        Ok(json!({
            "window": window_info.to_json(),
            "block": block_info.to_json(),
        }))
    }

    fn rpc_window_rename(&self, req: &Value) -> Result<Value, String> {
        let wid = req.get("window_id").and_then(Value::as_str).ok_or("missing window_id")?;
        let name = req.get("name").and_then(Value::as_str).ok_or("missing name")?;
        let info = {
            let mut st = self.inner.lock();
            let w = st.windows.get_mut(wid).ok_or_else(|| format!("no window `{wid}`"))?;
            w.name = if name.trim().is_empty() { None } else { Some(name.trim().to_string()) };
            w.info()
        };
        self.broadcast_event(json!({ "event": "window.renamed", "window": info.to_json() }));
        Ok(json!({ "window": info.to_json() }))
    }

    fn rpc_window_kill(&self, req: &Value) -> Result<Value, String> {
        let wid = req.get("window_id").and_then(Value::as_str).ok_or("missing window_id")?;
        let (sid, victims, session_removed) = {
            let mut st = self.inner.lock();
            let Some(win) = st.windows.remove(wid) else {
                return Err(format!("no window `{wid}`"));
            };
            let sid = win.session_id.clone();
            let mut victims = Vec::new();
            for bid in win.layout.leaves() {
                if let Some(b) = st.blocks.remove(&bid) {
                    victims.push(b);
                }
            }
            let mut session_removed = false;
            if let Some(sess) = st.sessions.get_mut(&sid) {
                sess.windows.retain(|id| id != wid);
                if sess.active_window.as_deref() == Some(wid) {
                    sess.active_window = sess.windows.last().cloned();
                }
                if sess.windows.is_empty() {
                    st.sessions.remove(&sid);
                    session_removed = true;
                }
            }
            (sid, victims, session_removed)
        };
        for b in victims {
            b.pty.hangup();
        }
        self.broadcast_event(json!({ "event": "window.killed", "window_id": wid, "session_id": sid }));
        if session_removed {
            self.broadcast_event(json!({ "event": "session.killed", "session_id": sid }));
        }
        Ok(json!({ "killed": wid }))
    }

    fn rpc_window_focus(&self, req: &Value) -> Result<Value, String> {
        let wid = req.get("window_id").and_then(Value::as_str).ok_or("missing window_id")?;
        let bid = req.get("block_id").and_then(Value::as_str);
        let info = {
            let mut st = self.inner.lock();
            let sid = st
                .windows
                .get(wid)
                .ok_or_else(|| format!("no window `{wid}`"))?
                .session_id
                .clone();
            if let Some(sess) = st.sessions.get_mut(&sid) {
                sess.active_window = Some(wid.to_string());
            }
            let w = st.windows.get_mut(wid).unwrap();
            if let Some(b) = bid {
                if w.layout.leaves().iter().any(|x| x == b) {
                    w.focused_block = b.to_string();
                }
            }
            w.info()
        };
        self.broadcast_event(json!({ "event": "window.focused", "window": info.to_json() }));
        Ok(json!({ "window": info.to_json() }))
    }

    fn rpc_window_set_zoom(&self, req: &Value) -> Result<Value, String> {
        let wid = req.get("window_id").and_then(Value::as_str).ok_or("missing window_id")?;
        let zoomed = req.get("zoomed").and_then(Value::as_bool).unwrap_or(false);
        let info = {
            let mut st = self.inner.lock();
            let w = st.windows.get_mut(wid).ok_or_else(|| format!("no window `{wid}`"))?;
            w.zoomed = zoomed;
            w.info()
        };
        self.broadcast_event(json!({ "event": "window.updated", "window": info.to_json() }));
        Ok(json!({ "window": info.to_json() }))
    }

    fn rpc_window_set_ratio(&self, req: &Value) -> Result<Value, String> {
        let wid = req.get("window_id").and_then(Value::as_str).ok_or("missing window_id")?;
        let ratio = req.get("ratio").and_then(Value::as_f64).ok_or("missing ratio")? as f32;
        let path: Vec<bool> = req
            .get("path")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_bool).collect())
            .unwrap_or_default();
        let info = {
            let mut st = self.inner.lock();
            let w = st.windows.get_mut(wid).ok_or_else(|| format!("no window `{wid}`"))?;
            w.layout.set_ratio(&path, ratio);
            w.info()
        };
        self.broadcast_event(json!({ "event": "window.updated", "window": info.to_json() }));
        Ok(json!({ "window": info.to_json() }))
    }

    fn rpc_block_split(self: &Arc<Self>, req: &Value) -> Result<Value, String> {
        let target_id = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let dir = req
            .get("dir")
            .and_then(Value::as_str)
            .and_then(SplitDir::parse)
            .unwrap_or(SplitDir::Right);
        let cols = req.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
        let rows = req.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
        let cmd = parse_cmd_array(req.get("cmd"));
        let label = req.get("label").and_then(Value::as_str).map(str::to_string);

        let (sid, wid, inherited_cwd) = {
            let st = self.inner.lock();
            let target = st
                .blocks
                .get(target_id)
                .ok_or_else(|| format!("no block `{target_id}`"))?;
            let cwd = target.parsed.lock().term.cwd.clone();
            (target.session_id.clone(), target.window_id.clone(), cwd)
        };
        let explicit_cwd = req.get("cwd").and_then(Value::as_str);
        let cwd = explicit_cwd.or(inherited_cwd.as_deref());

        let handoff = parse_handoff_config(req.get("handoff"))?;
        let keep = cmd.is_some() || handoff.is_some();
        let block = self.spawn_block_with_options(
            &sid,
            &wid,
            cols,
            rows,
            cwd,
            cmd.as_deref(),
            label,
            keep,
            None,
            false,
            handoff,
        )?;
        let new_id = block.id.clone();

        let window_info = {
            let mut st = self.inner.lock();
            let info = {
                let win = st.windows.get_mut(&wid).ok_or("window vanished")?;
                if !win.layout.split(target_id, new_id.clone(), dir) {
                    return Err(format!("block `{target_id}` is no longer in window `{wid}`"));
                }
                win.focused_block = new_id.clone();
                win.zoomed = false;
                win.info()
            };
            st.blocks.insert(new_id, Arc::clone(&block));
            info
        };

        let block_info = block.info();
        self.broadcast_event(json!({
            "event": "block.created",
            "window": window_info.to_json(),
            "block": block_info.to_json(),
        }));
        Ok(json!({
            "window": window_info.to_json(),
            "block": block_info.to_json(),
        }))
    }

    fn rpc_block_kill(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let block = self.remove_block_from_tree(bid);
        match block {
            Some(b) => {
                b.pty.hangup();
                Ok(json!({ "killed": bid }))
            }
            None => Err(format!("no block `{bid}`")),
        }
    }

    fn remove_block_from_tree(&self, bid: &str) -> Option<Arc<Block>> {
        let (block, wid, sid, window_updated, window_removed, session_removed) = {
            let mut st = self.inner.lock();
            let b = st.blocks.remove(bid)?;
            let wid = b.window_id.clone();
            let sid = b.session_id.clone();
            let mut window_updated = None;
            let mut window_removed = false;
            let mut session_removed = false;

            if let Some(win) = st.windows.get_mut(&wid) {
                if let Some(next_focus) = win.layout.remove(bid) {
                    if win.focused_block == bid {
                        win.focused_block = next_focus;
                    }
                    window_updated = Some(win.info());
                } else {
                    st.windows.remove(&wid);
                    window_removed = true;
                    if let Some(sess) = st.sessions.get_mut(&sid) {
                        sess.windows.retain(|id| id != &wid);
                        if sess.active_window.as_deref() == Some(&wid) {
                            sess.active_window = sess.windows.last().cloned();
                        }
                        if sess.windows.is_empty() {
                            st.sessions.remove(&sid);
                            session_removed = true;
                        }
                    }
                }
            }
            (b, wid, sid, window_updated, window_removed, session_removed)
        };

        self.broadcast_event(json!({ "event": "block.killed", "block_id": bid, "window_id": wid, "session_id": sid }));
        if let Some(w) = window_updated {
            self.broadcast_event(json!({ "event": "window.updated", "window": w.to_json() }));
        }
        if window_removed {
            self.broadcast_event(json!({ "event": "window.killed", "window_id": wid, "session_id": sid }));
        }
        if session_removed {
            self.broadcast_event(json!({ "event": "session.killed", "session_id": sid }));
        }
        Some(block)
    }

    fn rpc_block_inspect(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        Ok(json!({ "block": block.info().to_json() }))
    }

    fn rpc_block_set_awake(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let on = req.get("awake").and_then(Value::as_bool).unwrap_or(false);
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        {
            let mut p = block.parsed.lock();
            p.term.set_keep_awake(on);
            p.awake = on;
        }
        self.broadcast_event(json!({ "event": "block.updated", "block": block.info().to_json() }));
        Ok(json!({ "awake": on }))
    }

    /// Attach a client stream atomically: drains pending parser output, queues
    /// `RPC_RES` followed immediately by `PTY_SNAPSHOT`, then registers the
    /// subscriber under the block lock so zero bytes are lost or duplicated.
    fn rpc_block_attach(&self, outbox: &Arc<ClientOutbox>, req_id: u32, req: &Value) {
        let send_err = |msg: &str| {
            let body = json!({ "ok": false, "error": msg });
            if let Ok(f) = Frame::rpc_res(req_id, &body) {
                outbox.push_priority(f);
            }
        };
        let Some(bid) = req.get("block_id").and_then(Value::as_str) else {
            send_err("missing block_id");
            return;
        };
        let Some(stream_id) = req.get("stream_id").and_then(Value::as_u64).map(|x| x as u32) else {
            send_err("missing stream_id");
            return;
        };
        let interactive = req.get("mode").and_then(Value::as_str) != Some("readonly");
        let restore = req.get("restore").and_then(Value::as_bool).unwrap_or(true);

        let Some(block) = self.inner.lock().blocks.get(bid).cloned() else {
            send_err(&format!("no block `{bid}`"));
            return;
        };

        // Hold both `subscribers` and `parsed` locks so no live `PTY_OUT` can
        // sneak between the snapshot and subscription registration.
        let mut subs = block.subscribers.lock();
        let mut parsed = block.parsed.lock();
        block.drain_queue_into(&mut parsed);
        let (cols, rows) = (parsed.term.cols() as u16, parsed.term.rows() as u16);
        let alive = block.alive.load(Ordering::Acquire);
        let vt = if restore { parsed.term.to_vt() } else { Vec::new() };
        drop(parsed);

        outbox
            .streams
            .lock()
            .insert(stream_id, (bid.to_string(), interactive));

        let res = json!({
            "ok": true,
            "data": {
                "stream_id": stream_id,
                "block_id": bid,
                "cols": cols,
                "rows": rows,
                "alive": alive,
            }
        });
        if let Ok(f) = Frame::rpc_res(req_id, &res) {
            outbox.push_priority(f);
        }
        if restore {
            outbox.push_priority(Frame::pty_snapshot(stream_id, &vt));
        }

        subs.push(StreamSub {
            client_id: outbox.id.clone(),
            stream_id,
            outbox: Arc::clone(outbox),
        });
        block.attached_count.store(subs.len(), Ordering::Release);
    }

    fn rpc_block_detach(&self, outbox: &Arc<ClientOutbox>, req: &Value) -> Result<Value, String> {
        let stream_id = req
            .get("stream_id")
            .and_then(Value::as_u64)
            .ok_or("missing stream_id")? as u32;
        if let Some((bid, _)) = outbox.streams.lock().remove(&stream_id) {
            if let Some(block) = self.inner.lock().blocks.get(&bid).cloned() {
                let mut subs = block.subscribers.lock();
                subs.retain(|s| !(s.client_id == outbox.id && s.stream_id == stream_id));
                block.attached_count.store(subs.len(), Ordering::Release);
            }
        }
        Ok(json!({ "detached": stream_id }))
    }

    fn rpc_block_send(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let bytes = if let Some(b64) = req.get("data_b64").and_then(Value::as_str) {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("invalid base64: {e}"))?
        } else if let Some(text) = req.get("text").and_then(Value::as_str) {
            let mut b = text.as_bytes().to_vec();
            if req.get("enter").and_then(Value::as_bool).unwrap_or(false) {
                b.push(b'\r');
            }
            b
        } else {
            return Err("missing data_b64 or text".into());
        };
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        block.pty.write(&bytes).map_err(|e| format!("pty write failed: {e}"))?;
        block.input_seq.fetch_add(1, Ordering::Release);
        Ok(json!({ "written": bytes.len() }))
    }

    fn rpc_block_capture(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let lines = req.get("lines").and_then(Value::as_u64).map(|n| n as usize);
        let ansi = req.get("ansi").and_then(Value::as_bool).unwrap_or(false);
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        let text = block.capture(lines, ansi);
        Ok(json!({ "block_id": bid, "text": text }))
    }

    fn rpc_block_receipts(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let limit = req
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, BLOCK_RECEIPTS_CAP))
            .unwrap_or(20);
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        let list: Vec<Value> = block
            .receipts_list(limit)
            .into_iter()
            .map(|r| r.to_json())
            .collect();
        let last = block.receipts.lock().back().map(CommandReceipt::to_json);
        Ok(json!({
            "block_id": bid,
            "last_receipt": last,
            "receipts": list,
        }))
    }

    fn rpc_block_rewind_list(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        let mut parsed = block.parsed.lock();
        block.drain_queue_into(&mut parsed);
        let frames: Vec<Value> = parsed
            .rewind
            .frames_with_age()
            .into_iter()
            .map(|(seq, age_ms)| json!([seq, age_ms]))
            .collect();
        Ok(json!({
            "block_id": bid,
            "frames": frames,
        }))
    }

    fn rpc_block_rewind_frame(&self, req: &Value) -> Result<Value, String> {
        use base64::Engine;
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let seq = req
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or("missing seq")?;
        let include_text = req
            .get("include_text")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let ansi = req.get("ansi").and_then(Value::as_bool).unwrap_or(false);
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        let (term, age_ms) = {
            let mut parsed = block.parsed.lock();
            block.drain_queue_into(&mut parsed);
            let age_ms = parsed
                .rewind
                .chunk_time(seq)
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            let term = parsed
                .rewind
                .replay(seq)
                .ok_or_else(|| format!("rewind frame `{seq}` unavailable"))?;
            (term, age_ms)
        };
        let cols = term.cols();
        let rows = term.rows();
        let vt = term.to_vt();
        let lz4 = lz4_flex::compress_prepend_size(&vt);
        let vt_lz4_b64 = base64::engine::general_purpose::STANDARD.encode(&lz4);
        let mut out = json!({
            "block_id": bid,
            "seq": seq,
            "cols": cols,
            "rows": rows,
            "age_ms": age_ms,
            "vt_lz4_b64": vt_lz4_b64,
        });
        if include_text {
            let text = if ansi {
                String::from_utf8_lossy(&vt).into_owned()
            } else {
                let h = term.grid().history_len();
                if rows == 0 || cols == 0 {
                    String::new()
                } else {
                    term.text_range(h, 0, h + rows - 1, cols - 1)
                }
            };
            out["text"] = json!(text);
        }
        Ok(out)
    }

    fn rpc_block_rewind_provenance(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let seq = req.get("seq").and_then(Value::as_u64).ok_or("missing seq")?;
        let x = req.get("x").and_then(Value::as_u64).ok_or("missing x")? as usize;
        let y = req.get("y").and_then(Value::as_u64).ok_or("missing y")? as usize;
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        let mut parsed = block.parsed.lock();
        block.drain_queue_into(&mut parsed);
        let frame_time = parsed.rewind.chunk_time(seq);
        match parsed.rewind.provenance(seq, x, y) {
            Some(p) => {
                let before_ms = frame_time
                    .map(|ft| ft.saturating_duration_since(p.time).as_millis() as u64)
                    .unwrap_or(0);
                Ok(json!({
                    "found": true,
                    "offset": p.offset,
                    "before_ms": before_ms,
                    "context": p.context,
                    "scrolled": p.scrolled,
                }))
            }
            None => Ok(json!({ "found": false })),
        }
    }

    fn rpc_block_set_handoff(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let cfg = parse_handoff_config(Some(req))?
            .ok_or("missing report_to")?;
        if cfg.report_to == bid {
            return Err("block cannot target itself for handoff".into());
        }
        let block = self
            .inner
            .lock()
            .blocks
            .get(bid)
            .cloned()
            .ok_or_else(|| format!("no block `{bid}`"))?;
        let st = HandoffState::from_config(cfg);
        let resp = st.to_json();
        *block.handoff.lock() = Some(st);

        if let Some(code) = *block.exit_code.lock() {
            self.deliver_handoff(&block, "exit", Some(code), None, true);
        }
        Ok(json!({ "block_id": bid, "handoff": resp }))
    }

    fn rpc_block_wait(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let timeout = req.get("timeout_ms").and_then(Value::as_u64).map(Duration::from_millis);
        let after_change = req.get("after_change").and_then(Value::as_bool).unwrap_or(false);
        let targets = parse_wait_until(req.get("until"))?;

        let waiter_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (rx, block_opt) = {
            let mut st = self.inner.lock();
            if let Some((_, code)) = st.recent_exits.iter().find(|(id, _)| id == bid) {
                let res = exited_wait_result(&targets, *code, None);
                return Ok(res.to_json(bid));
            }
            let block = st
                .blocks
                .get(bid)
                .cloned()
                .ok_or_else(|| format!("no block `{bid}`"))?;
            let (tx, rx) = mpsc::channel();
            st.state_waiters
                .entry(bid.to_string())
                .or_default()
                .push(StateWaiter {
                    id: waiter_id,
                    targets: targets.clone(),
                    tx,
                });
            (rx, Some(block))
        };

        if let Some(block) = block_opt {
            if !after_change {
                let info = block.info();
                if let Some(res) = evaluate_wait_conditions(&targets, &info) {
                    let mut st = self.inner.lock();
                    if let Some(list) = st.state_waiters.get_mut(bid) {
                        list.retain(|w| w.id != waiter_id);
                        if list.is_empty() {
                            st.state_waiters.remove(bid);
                        }
                    }
                    return Ok(res.to_json(bid));
                }
            }
        }

        let res = match timeout {
            Some(d) => match rx.recv_timeout(d) {
                Ok(res) => res,
                Err(_) => {
                    let mut st = self.inner.lock();
                    if let Some(list) = st.state_waiters.get_mut(bid) {
                        list.retain(|w| w.id != waiter_id);
                        if list.is_empty() {
                            st.state_waiters.remove(bid);
                        }
                    }
                    return Err(format!("timed out waiting for block `{bid}`"));
                }
            },
            None => rx
                .recv()
                .map_err(|_| format!("waiter dropped for block `{bid}`"))?,
        };
        Ok(res.to_json(bid))
    }

    fn notify_block_waiters(&self, bid: &str, info: &BlockInfo) {
        let mut ready: Vec<(Sender<WaitResult>, WaitResult)> = Vec::new();
        {
            let mut st = self.inner.lock();
            if let Some(list) = st.state_waiters.get_mut(bid) {
                let mut i = 0;
                while i < list.len() {
                    if let Some(res) = evaluate_wait_conditions(&list[i].targets, info) {
                        let w = list.swap_remove(i);
                        ready.push((w.tx, res));
                    } else {
                        i += 1;
                    }
                }
                if list.is_empty() {
                    st.state_waiters.remove(bid);
                }
            }
        }
        for (tx, res) in ready {
            let _ = tx.send(res);
        }
    }

    fn deliver_handoff(
        &self,
        block: &Arc<Block>,
        state_str: &str,
        exit_code: Option<i32>,
        msg: Option<&str>,
        is_exit: bool,
    ) {
        let cfg = {
            let mut guard = block.handoff.lock();
            let Some(ref mut hs) = *guard else {
                return;
            };
            let wanted = if is_exit {
                hs.config.notify_on.contains(&WaitCondition::Exit)
                    || (exit_code == Some(0)
                        && hs.config.notify_on.contains(&WaitCondition::State(ProgramState::Done)))
                    || (exit_code.is_some_and(|c| c != 0)
                        && hs.config.notify_on.contains(&WaitCondition::State(ProgramState::Error)))
            } else if let Some(ps) = ProgramState::parse(state_str) {
                hs.config.notify_on.contains(&WaitCondition::State(ps))
            } else {
                false
            };
            if !wanted {
                return;
            }
            if is_exit {
                if hs.reported_exit {
                    return;
                }
                hs.reported_exit = true;
            } else {
                if hs.last_reported.as_deref() == Some(state_str) {
                    return;
                }
                hs.last_reported = Some(state_str.to_string());
            }
            hs.config.clone()
        };

        let label = block.label.lock().clone();
        let from_name = cfg
            .from_label
            .clone()
            .or(label)
            .unwrap_or_else(|| block.id.clone());
        let captured = block.capture(Some(cfg.capture_lines), false);

        let parent = self.inner.lock().blocks.get(&cfg.report_to).cloned();
        let mut delivered = false;
        let mut formatted_report = None;
        if let Some(p) = parent {
            if p.alive.load(Ordering::Acquire) {
                let bracketed = p.parsed.lock().term.mode.contains(TermMode::BRACKETED_PASTE);
                let report = format_handoff_report(
                    &from_name,
                    &block.id,
                    state_str,
                    exit_code,
                    msg,
                    &captured,
                    bracketed,
                );
                let write_ok = if bracketed {
                    let wrapped = format!("\x1b[200~{report}\x1b[201~");
                    let ok = p.pty.write(wrapped.as_bytes()).is_ok();
                    if ok && cfg.send_enter {
                        let _ = p.pty.write(b"\r");
                    }
                    ok
                } else {
                    let mut payload = report.clone();
                    if cfg.send_enter {
                        payload.push('\r');
                    }
                    p.pty.write(payload.as_bytes()).is_ok()
                };
                if write_ok {
                    p.input_seq.fetch_add(1, Ordering::Release);
                    delivered = true;
                }
                formatted_report = Some(report);
            }
        }

        self.broadcast_event(json!({
            "event": "block.handoff",
            "from_block": block.id,
            "to_block": cfg.report_to,
            "from": from_name,
            "state": state_str,
            "exit_code": exit_code,
            "delivered": delivered,
            "report": formatted_report,
        }));
    }

    fn on_block_state_update(&self, block: &Arc<Block>, info: &BlockInfo, status_updated: bool) {
        self.notify_block_waiters(&block.id, info);
        if status_updated {
            if let Some(ref st) = info.status {
                if st.state == ProgramState::Working {
                    if let Some(ref mut hs) = *block.handoff.lock() {
                        hs.last_reported = None;
                    }
                } else {
                    self.deliver_handoff(
                        block,
                        st.state.as_str(),
                        None,
                        st.msg.as_deref(),
                        false,
                    );
                }
                let interactive = block.attached_count.load(Ordering::Acquire);
                if let Some(cfg) = self.alerts.evaluate_transition(
                    &block.id,
                    st.state.as_str(),
                    st.msg.as_deref(),
                    interactive,
                ) {
                    let payload = alerts::AlertPayload {
                        host: self.host_info.hostname.clone(),
                        block_id: block.id.clone(),
                        session_id: block.session_id.clone(),
                        window_id: block.window_id.clone(),
                        label: info.display_name().to_string(),
                        state: st.state.as_str().to_string(),
                        app: st.app.clone(),
                        message: st.msg.clone(),
                        cwd: info.cwd.clone(),
                        inferred: info.status_inferred,
                        interactive_clients: interactive,
                        timestamp_unix: unix_now(),
                    };
                    self.broadcast_event(payload.to_json());
                    alerts::dispatch_async(cfg, payload);
                }
            }
        }
    }

    fn record_block_exit(&self, block: &Arc<Block>, code: i32) {
        *block.exit_code.lock() = Some(code);
        block.alive.store(false, Ordering::Release);

        if !block.is_login_shell || block.receipt_sampler.is_active() {
            let wall = block.started_at.elapsed();
            if let Some(rcpt) = block.receipt_sampler.finish(
                None,
                Some(code),
                block.spawn_cmdline.clone(),
                (!block.is_login_shell).then_some(wall),
            ) {
                block.record_receipt(rcpt.clone());
                self.broadcast_event(json!({
                    "event": "block.receipt",
                    "block_id": block.id,
                    "session_id": block.session_id,
                    "window_id": block.window_id,
                    "receipt": rcpt.to_json(),
                }));
            }
        }

        let info = block.info();
        self.notify_block_waiters(&block.id, &info);
        self.deliver_handoff(block, "exit", Some(code), None, true);

        let leftovers: Vec<Sender<WaitResult>> = {
            let mut st = self.inner.lock();
            st.recent_exits.push_back((block.id.clone(), code));
            while st.recent_exits.len() > RECENT_EXITS_CAP {
                st.recent_exits.pop_front();
            }
            st.state_waiters
                .remove(&block.id)
                .unwrap_or_default()
                .into_iter()
                .map(|w| w.tx)
                .collect()
        };
        if !leftovers.is_empty() {
            let fallback = WaitResult {
                matched: "exited".into(),
                exit_code: Some(code),
                exited: true,
                status: info.status.clone(),
                status_inferred: info.status_inferred,
            };
            for tx in leftovers {
                let _ = tx.send(fallback.clone());
            }
        }

        self.broadcast_event(json!({
            "event": "block.exited",
            "block_id": block.id,
            "window_id": block.window_id,
            "session_id": block.session_id,
            "exit_code": code,
        }));

        if !block.keep_after_exit && !self.is_shutting_down() {
            if self.remove_block_from_tree(&block.id).is_some() {
                self.mark_persist_dirty();
            }
        }
    }
}

fn parse_wait_until(v: Option<&Value>) -> Result<Vec<WaitCondition>, String> {
    let Some(val) = v else {
        return Ok(vec![WaitCondition::Exit]);
    };
    let mut out = Vec::new();
    match val {
        Value::String(s) => {
            for part in s.split(',') {
                let t = part.trim();
                if !t.is_empty() {
                    let c = WaitCondition::parse(t)
                        .ok_or_else(|| format!("invalid wait condition `{t}`"))?;
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                let s = item
                    .as_str()
                    .ok_or("wait condition array items must be strings")?;
                for part in s.split(',') {
                    let t = part.trim();
                    if !t.is_empty() {
                        let c = WaitCondition::parse(t)
                            .ok_or_else(|| format!("invalid wait condition `{t}`"))?;
                        if !out.contains(&c) {
                            out.push(c);
                        }
                    }
                }
            }
        }
        _ => return Err("`until` must be a string or string array".into()),
    }
    if out.is_empty() {
        out.push(WaitCondition::Exit);
    }
    Ok(out)
}

fn exited_wait_result(
    targets: &[WaitCondition],
    code: i32,
    status: Option<&BlockInfo>,
) -> WaitResult {
    let matched = if targets.contains(&WaitCondition::Exit) {
        "exit"
    } else if code == 0 && targets.contains(&WaitCondition::State(ProgramState::Done)) {
        "done"
    } else if code != 0 && targets.contains(&WaitCondition::State(ProgramState::Error)) {
        "error"
    } else {
        "exited"
    };
    WaitResult {
        matched: matched.into(),
        exit_code: Some(code),
        exited: true,
        status: status.and_then(|i| i.status.clone()),
        status_inferred: status.is_some_and(|i| i.status_inferred),
    }
}

fn evaluate_wait_conditions(targets: &[WaitCondition], info: &BlockInfo) -> Option<WaitResult> {
    if !info.alive {
        let code = info.exit_code.unwrap_or(0);
        return Some(exited_wait_result(targets, code, Some(info)));
    }
    let st = info.status.as_ref().map(|s| s.state);
    for &cond in targets {
        let matched = match cond {
            WaitCondition::Exit => false,
            WaitCondition::State(want) => {
                st == Some(want)
                    || (want == ProgramState::Working && info.command_running)
                    || (want == ProgramState::Idle
                        && !info.command_running
                        && matches!(st, None | Some(ProgramState::Idle) | Some(ProgramState::Done)))
            }
        };
        if matched {
            return Some(WaitResult {
                matched: cond.as_str().into(),
                exit_code: info.exit_code,
                exited: false,
                status: info.status.clone(),
                status_inferred: info.status_inferred,
            });
        }
    }
    None
}

fn parse_handoff_config(v: Option<&Value>) -> Result<Option<HandoffConfig>, String> {
    let Some(val) = v else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let report_to = val
        .get("report_to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("handoff missing non-empty `report_to`")?
        .to_string();
    let from_label = val
        .get("from")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let notify_on = if val.get("notify_on").is_some() {
        parse_wait_until(val.get("notify_on"))?
    } else {
        vec![
            WaitCondition::Exit,
            WaitCondition::State(ProgramState::Blocked),
            WaitCondition::State(ProgramState::Done),
            WaitCondition::State(ProgramState::Error),
        ]
    };
    let capture_lines = val
        .get("capture_lines")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, 200))
        .unwrap_or(40);
    let send_enter = val
        .get("send_enter")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok(Some(HandoffConfig {
        report_to,
        from_label,
        notify_on,
        capture_lines,
        send_enter,
    }))
}

fn escape_xml_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn sanitize_handoff_body(raw: &str, multiline: bool) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch == '\r' {
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        cleaned.push(ch);
    }
    let trimmed_lines: Vec<&str> = cleaned
        .lines()
        .map(|l| l.trim_end())
        .skip_while(|l| l.is_empty())
        .collect();
    let end = trimmed_lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let slice = &trimmed_lines[..end];
    let escaped_close = |line: &str| {
        line.replace("</tachyon>", "&lt;/tachyon&gt;")
            .replace("</Tachyon>", "&lt;/Tachyon&gt;")
    };
    let mut body = if multiline {
        slice
            .iter()
            .map(|l| escaped_close(l))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        slice
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| escaped_close(l.trim()))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    const MAX_BODY: usize = 4096;
    if body.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push_str("…");
    }
    body
}

/// Format a sanitized `<tachyon ...>...</tachyon>` agent handoff payload.
pub fn format_handoff_report(
    from: &str,
    block_id: &str,
    state: &str,
    exit_code: Option<i32>,
    msg: Option<&str>,
    captured: &str,
    multiline: bool,
) -> String {
    let mut attrs = format!(
        "from=\"{}\" block=\"{}\" state=\"{}\"",
        escape_xml_attr(from),
        escape_xml_attr(block_id),
        escape_xml_attr(state)
    );
    if let Some(code) = exit_code {
        attrs.push_str(&format!(" exit_code=\"{code}\""));
    }
    if let Some(m) = msg.filter(|s| !s.trim().is_empty()) {
        attrs.push_str(&format!(" msg=\"{}\"", escape_xml_attr(m)));
    }
    let body = sanitize_handoff_body(captured, multiline);
    if body.is_empty() {
        format!("<tachyon {attrs}></tachyon>")
    } else if multiline {
        format!("<tachyon {attrs}>\n{body}\n</tachyon>")
    } else {
        format!("<tachyon {attrs}>{body}</tachyon>")
    }
}

fn persist_worker_loop(weak: Weak<ServerState>) {
    const DEBOUNCE: Duration = Duration::from_millis(500);
    loop {
        let Some(state) = weak.upgrade() else {
            break;
        };
        {
            let mut sig = state.persist.signal.lock();
            loop {
                if sig.shutdown {
                    return;
                }
                if sig.dirty {
                    break;
                }
                state.persist.wake.wait(&mut sig);
            }
            // Coalesce bursty updates (splits, OSC cwd/title bursts, agent inference)
            // without any periodic idle wake-ups.
            let _ = state.persist.wake.wait_for(&mut sig, DEBOUNCE);
            if sig.shutdown {
                return;
            }
            if !sig.dirty {
                continue;
            }
            sig.dirty = false;
        }
        let _ = state.flush_persist_now();
    }
}

fn capture_persisted_pane(block: &Block) -> PersistedPane {
    let fg = block.pty.foreground_process(block.is_login_shell);
    let label = block.label.lock().clone();
    let mut p = block.parsed.lock();
    block.drain_queue_into(&mut p);

    let (cols, rows) = (p.term.cols() as u16, p.term.rows() as u16);
    let title = p.term.title.clone();
    let cwd = p.term.cwd.clone();
    let awake = p.awake;

    let agent_active = (!block.is_login_shell && block.alive.load(Ordering::Acquire))
        || p.term.command_running()
        || fg.is_some()
        || p.term.in_alt_screen();

    let (agent_id, resume_cmd, last_state) = if agent_active {
        let status = p
            .term
            .program_status
            .as_ref()
            .or(p.inferred_status.as_ref());
        let last_state = status.map(|s| s.state.as_str().to_string());
        let manifest_from_status = status
            .and_then(|s| s.app.as_deref())
            .and_then(crate::mux::agents::find_manifest);
        let manifest = manifest_from_status.or_else(|| {
            let cmdline = p
                .term
                .running_cmdline()
                .or(block.spawn_cmdline.as_deref());
            let allow_screen = crate::mux::agents::should_allow_screen_fallback(
                fg.as_ref(),
                cmdline,
                p.term.in_alt_screen(),
            );
            let lines = if allow_screen {
                crate::mux::agents::extract_visible_lines(
                    &p.term,
                    crate::mux::agents::MAX_SCREEN_LINES,
                )
            } else {
                Vec::new()
            };
            crate::mux::agents::identify_agent(
                fg.as_ref(),
                cmdline,
                title.as_deref(),
                label.as_deref(),
                &lines,
                allow_screen,
            )
        });
        match manifest {
            Some(m) => (
                Some(m.id.to_string()),
                m.resume_cmd.map(str::to_string),
                last_state,
            ),
            None => (None, None, None),
        }
    } else {
        (None, None, None)
    };

    PersistedPane {
        label,
        title,
        cwd,
        cols,
        rows,
        awake,
        agent_id,
        resume_cmd,
        last_state,
    }
}

fn parse_cmd_array(v: Option<&Value>) -> Option<Vec<String>> {
    let arr = v?.as_array()?;
    let parts: Vec<String> = arr
        .iter()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Read PTY output continuously: forward straight to every attached subscriber
/// and enqueue into the block's background E-core VT parser.
fn read_block_loop(state: Arc<ServerState>, block: Arc<Block>) {
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = match block.pty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = buf[..n].to_vec();
        // Lock order: subscribers -> queue (strictly matching attach's
        // subscribers -> parsed -> queue), guaranteeing gap-free handoff and
        // ensuring any output seen by a subscriber is already queued for capture.
        let subs = block.subscribers.lock();
        {
            let mut q = block.queue.lock();
            while q.1 > MAX_PARSE_QUEUED {
                block.space.wait(&mut q);
            }
            q.0.push_back(chunk.clone());
            q.1 += n;
            block.work.notify_one();
        }
        for sub in subs.iter() {
            sub.outbox.push_pty_out(sub.stream_id, chunk.clone());
        }
    }

    // Signal parser thread to finish draining and exit.
    block.queue.lock().2 = true;
    block.work.notify_one();

    // Sample final CPU / peak RSS before `wait_exit` reaps direct child processes.
    block.receipt_sampler.sample_now();
    let code = block.pty.wait_exit();
    state.record_block_exit(&block, code);
}

/// Parse queued PTY output on a background thread so forwarding never blocks
/// on terminal parsing. Answers terminal queries while no client is attached
/// and runs zero-hook agent state classification with zero idle wake-ups.
fn parse_block_loop(state: Arc<ServerState>, block: Arc<Block>) {
    let mut settle_wait: Option<Duration> = None;
    loop {
        let timed_out = {
            let mut q = block.queue.lock();
            let mut hit_timeout = false;
            while q.0.is_empty() && !q.2 && !block.has_pending_cmd.load(Ordering::Acquire) {
                if let Some(dur) = settle_wait {
                    let res = block.work.wait_for(&mut q, dur);
                    if q.0.is_empty()
                        && !q.2
                        && !block.has_pending_cmd.load(Ordering::Acquire)
                        && res.timed_out()
                    {
                        hit_timeout = true;
                        break;
                    }
                } else {
                    block.work.wait(&mut q);
                }
            }
            if q.0.is_empty() && !hit_timeout && !block.has_pending_cmd.load(Ordering::Acquire) {
                return;
            }
            hit_timeout
        };

        let now = std::time::Instant::now();
        if timed_out {
            let mut parsed = block.parsed.lock();
            let cur_in = block.input_seq.load(Ordering::Acquire);
            if cur_in != parsed.last_input_seq {
                parsed.last_input_seq = cur_in;
                parsed.agent_tracker.note_input(now);
            }
            let (inferred_changed, next_settle) = parsed.evaluate_agent(&block, now);
            drop(parsed);
            settle_wait = next_settle;
            if inferred_changed {
                let info = block.info();
                state.broadcast_event(json!({
                    "event": "block.status",
                    "block": info.to_json(),
                    "status": info.status.as_ref().map(crate::mux::model::status_to_json),
                }));
                state.on_block_state_update(&block, &info, true);
            }
            continue;
        }

        let mut parsed = block.parsed.lock();
        let mut cmd_events = std::mem::take(&mut parsed.pending_cmd_events);
        block.has_pending_cmd.store(false, Ordering::Release);
        let (chunk, queue_empty) = {
            let mut q = block.queue.lock();
            let c = q.0.pop_front();
            q.1 -= c.as_ref().map_or(0, Vec::len);
            (c, q.0.is_empty())
        };
        block.space.notify_one();

        let (replies, meta_changed, status_changed, inferred_changed, processed_chunk) =
            if let Some(chunk) = chunk {
                parsed.term.unlink_temp_images = block.attached_count.load(Ordering::Acquire) == 0;
                let (replies, meta_changed, status_changed, evs) = parsed.feed(&block.pty, &chunk);
                cmd_events.extend(evs);

                let cur_in = block.input_seq.load(Ordering::Acquire);
                if cur_in != parsed.last_input_seq {
                    parsed.last_input_seq = cur_in;
                    parsed.agent_tracker.note_input(now);
                }
                parsed.agent_tracker.note_output(chunk.len(), now);

                let should_eval = meta_changed
                    || status_changed
                    || queue_empty
                    || parsed
                        .last_agent_eval
                        .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_millis(250));
                let inferred_changed = if should_eval {
                    let (chg, next_settle) = parsed.evaluate_agent(&block, now);
                    settle_wait = next_settle;
                    chg
                } else {
                    false
                };
                (replies, meta_changed, status_changed, inferred_changed, true)
            } else {
                (Vec::new(), false, false, false, false)
            };
        drop(parsed);

        if block.is_login_shell {
            for (ev, cmdline) in cmd_events {
                match ev {
                    CommandEvent::Started(cmd_id) => {
                        block.receipt_sampler.start(Some(cmd_id), cmdline);
                    }
                    CommandEvent::Finished(cmd_id, exit_opt) => {
                        if let Some(rcpt) = block.receipt_sampler.finish(
                            Some(cmd_id),
                            exit_opt,
                            cmdline,
                            None,
                        ) {
                            block.parsed.lock().term.set_receipt_with_wall(
                                cmd_id,
                                rcpt.to_term_receipt(),
                                Some(rcpt.wall_duration()),
                            );
                            block.record_receipt(rcpt.clone());
                            state.broadcast_event(json!({
                                "event": "block.receipt",
                                "block_id": block.id,
                                "session_id": block.session_id,
                                "window_id": block.window_id,
                                "receipt": rcpt.to_json(),
                            }));
                        }
                    }
                }
            }
        }
        if !processed_chunk {
            continue;
        }

        if !replies.is_empty() && block.attached_count.load(Ordering::Acquire) == 0 {
            let _ = block.pty.write(&replies);
        }
        if status_changed || inferred_changed {
            let info = block.info();
            state.broadcast_event(json!({
                "event": "block.status",
                "block": info.to_json(),
                "status": info.status.as_ref().map(crate::mux::model::status_to_json),
            }));
            state.on_block_state_update(&block, &info, true);
        } else if meta_changed {
            let info = block.info();
            state.on_block_state_update(&block, &info, false);
        }
        if meta_changed || status_changed || inferred_changed {
            state.mark_persist_dirty();
        }
        if meta_changed {
            state.broadcast_event(json!({
                "event": "block.updated",
                "block": block.info().to_json(),
            }));
        }
    }
}

/// Expand leading `~` or `~/` / `~\` against this host's home directory so
/// portable multi-host workspace `cwd` values work identically on macOS, Linux,
/// and Windows.
fn expand_tilde(path: &str) -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|h| !h.is_empty());
    let Some(home) = home else {
        return path.to_string();
    };
    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix(r"~\")) {
        return std::path::PathBuf::from(home).join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Root folder for server-owned Rewind cold segments (cleaned of dead-PID
/// directories on server startup).
fn server_rewind_root() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(|d| std::path::PathBuf::from(d).join("Tachyon").join("rewind"))
            .or_else(|| Some(std::env::temp_dir().join("tachyon-rewind")))
    }
    #[cfg(not(windows))]
    {
        let sock = crate::mux::default_socket_path();
        sock.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("rewind"))
    }
}

fn server_rewind_dir(block_id: &str) -> Option<std::path::PathBuf> {
    let root = server_rewind_root()?;
    Some(root.join(format!("{}-{block_id}", std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_block_info(alive: bool, exit_code: Option<i32>, command_running: bool, state: Option<ProgramState>) -> BlockInfo {
        BlockInfo {
            id: "b_1".into(),
            session_id: "s_1".into(),
            window_id: "w_1".into(),
            label: Some("worker".into()),
            title: None,
            cwd: None,
            command_running,
            command: None,
            pid: 1,
            cols: 80,
            rows: 24,
            alive,
            exit_code,
            awake: false,
            attached_clients: 0,
            created_at: 0,
            status: state.map(|st| ProgramStatus {
                state: st,
                app: Some("claude".into()),
                kind: None,
                msg: Some("Needs approval".into()),
                label: None,
            }),
            status_inferred: false,
            last_receipt: None,
        }
    }

    #[test]
    fn handoff_report_formats_sanitizes_and_escapes_xml_safely() {
        let single = format_handoff_report(
            "build\"&<1>",
            "b_9",
            "exit",
            Some(0),
            Some("All \"tests\" <pass>"),
            "\n  line one\x07\nline </tachyon> two\n\n",
            false,
        );
        assert!(!single.contains('\n'));
        assert!(!single.contains('\x07'));
        assert!(single.contains("from=\"build&quot;&amp;&lt;1&gt;\""));
        assert!(single.contains("msg=\"All &quot;tests&quot; &lt;pass&gt;\""));
        assert!(single.contains("line one | line &lt;/tachyon&gt; two</tachyon>"));

        let multi = format_handoff_report(
            "reviewer",
            "b_2",
            "blocked",
            None,
            Some("Approve tool"),
            "line A\nline B",
            true,
        );
        assert!(multi.starts_with("<tachyon from=\"reviewer\" block=\"b_2\" state=\"blocked\" msg=\"Approve tool\">\n"));
        assert!(multi.ends_with("line A\nline B\n</tachyon>"));
    }

    #[test]
    fn wait_condition_evaluation_handles_states_and_exit_safely() {
        let working = test_block_info(true, None, true, Some(ProgramState::Working));
        assert!(evaluate_wait_conditions(&[WaitCondition::State(ProgramState::Blocked)], &working).is_none());
        let w = evaluate_wait_conditions(&[WaitCondition::State(ProgramState::Working)], &working).unwrap();
        assert_eq!(w.matched, "working");
        assert!(!w.exited);

        let blocked = test_block_info(true, None, true, Some(ProgramState::Blocked));
        let b = evaluate_wait_conditions(
            &[WaitCondition::State(ProgramState::Blocked), WaitCondition::State(ProgramState::Done)],
            &blocked,
        )
        .unwrap();
        assert_eq!(b.matched, "blocked");
        assert!(!b.exited);

        let idle_shell = test_block_info(true, None, false, None);
        let id = evaluate_wait_conditions(&[WaitCondition::State(ProgramState::Idle)], &idle_shell).unwrap();
        assert_eq!(id.matched, "idle");

        // Waiting for `blocked` on an already-exited block must wake cleanly instead of hanging.
        let dead = test_block_info(false, Some(3), false, None);
        let res = evaluate_wait_conditions(&[WaitCondition::State(ProgramState::Blocked)], &dead).unwrap();
        assert_eq!(res.matched, "exited");
        assert!(res.exited);
        assert_eq!(res.exit_code, Some(3));
    }
}
