//! Multiplexer server state tree (`Session -> Window -> Block`), per-client
//! bounded outbox with slow-client resync, background E-core VT parsing, and
//! JSON RPC handler.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use parking_lot::{Condvar, Mutex};
use serde_json::{json, Value};

use crate::mux::model::{
    generate_session_name, BlockInfo, ClientInfo, HostInfo, LayoutNode, ServerSnapshot, SessionInfo,
    SplitDir, WindowInfo,
};
use crate::mux::proto::{Frame, PROTOCOL_VERSION};
use crate::mux::{local_hostname, unix_now};
use crate::term::{Processor, Term};

use super::pty::{default_shell, find_integration_dir, integration, HostPty};

/// Output the server-side VT parser may fall behind by before reading pauses.
const MAX_PARSE_QUEUED: usize = 32 << 20;

/// Maximum queued `PTY_OUT` bytes per attached client stream before dropping
/// stale intermediate frames and scheduling a clean `PTY_SNAPSHOT` resync.
const MAX_CLIENT_STREAM_BYTES: usize = 4 << 20;

/// Default scrollback kept by server-side terminals for capture & restore.
const DEFAULT_SCROLLBACK: usize = 10_000;

/// Number of recently exited block exit codes remembered for `block.wait`.
const RECENT_EXITS_CAP: usize = 256;

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
    size: (u16, u16),
    last_title_rev: u64,
    last_status_rev: u64,
    awake: bool,
}

impl ParsedBlock {
    fn feed(&mut self, pty: &HostPty, chunk: &[u8]) -> (Vec<u8>, bool, bool) {
        let ws = pty.size();
        if ws != self.size {
            self.size = ws;
            self.term.resize(ws.0.max(1) as usize, ws.1.max(1) as usize);
        }
        self.processor.advance(&mut self.term, chunk);
        self.term.take_command_events();
        self.term.take_dirty();
        let rev = self.term.title_revision;
        let meta_changed = rev != self.last_title_rev;
        self.last_title_rev = rev;
        let srev = self.term.status_revision;
        let status_changed = srev != self.last_status_rev;
        self.last_status_rev = srev;
        (self.term.take_replies(), meta_changed, status_changed)
    }
}

pub struct Block {
    pub id: String,
    pub session_id: String,
    pub window_id: String,
    pub label: Mutex<Option<String>>,
    pub created_at: u64,
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
}

impl Block {
    pub fn info(&self) -> BlockInfo {
        let p = self.parsed.lock();
        let (cols, rows) = (p.term.cols() as u16, p.term.rows() as u16);
        let cmd = p.term.running_cmdline().map(str::to_string);
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
            status: p.term.program_status.clone(),
        }
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
            parsed.feed(&self.pty, &chunk);
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
    exit_waiters: HashMap<String, Vec<Sender<i32>>>,
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

pub struct ServerState {
    host_info: HostInfo,
    auth_token: Option<String>,
    next_id: AtomicU64,
    shutdown: AtomicBool,
    inner: Mutex<StateInner>,
}

impl ServerState {
    pub fn new() -> Arc<Self> {
        Self::with_auth_token(None)
    }

    pub fn with_auth_token(auth_token: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            host_info: HostInfo {
                hostname: local_hostname(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                version: crate::APP_VERSION.to_string(),
                protocol: PROTOCOL_VERSION,
                pid: std::process::id(),
            },
            auth_token,
            next_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
            inner: Mutex::new(StateInner {
                sessions: BTreeMap::new(),
                windows: BTreeMap::new(),
                blocks: BTreeMap::new(),
                clients: HashMap::new(),
                recent_exits: VecDeque::new(),
                exit_waiters: HashMap::new(),
            }),
        })
    }

    pub fn host_info(&self) -> &HostInfo {
        &self.host_info
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
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
            let _ = b.pty.write(bytes);
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
            host: self.host_info.clone(),
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
        // `block.wait` blocks until process exit; run off the reader thread so
        // the client's connection stays responsive for events/pings/output.
        if op == "block.wait" {
            let state = Arc::clone(self);
            let out = Arc::clone(outbox);
            let req_owned = req.clone();
            let _ = std::thread::Builder::new()
                .name("tachyon-srv-wait".into())
                .spawn(move || {
                    let body = match state.rpc_block_wait(&req_owned) {
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
            self.shutdown.store(true, Ordering::Release);
            let res = json!({ "ok": true, "data": { "shutting_down": true } });
            if let Ok(f) = Frame::rpc_res(req_id, &res) {
                outbox.push_priority(f);
            }
            return RpcOutcome::ShutdownServer;
        }

        let result: Result<Value, String> = match op {
            "hello" => self.rpc_hello(outbox, req),
            "state.get" => Ok(self.snapshot().to_json()),
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
            "block.detach" => self.rpc_block_detach(outbox, req),
            "block.send" => self.rpc_block_send(req),
            "block.capture" => self.rpc_block_capture(req),
            "block.wait" => self.rpc_block_wait(req),
            _ => Err(format!("unknown op `{op}`")),
        };

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
            if let Some(expected) = self.auth_token.as_deref() {
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
            "host": self.host_info.to_json(),
        }))
    }

    fn spawn_block(
        self: &Arc<Self>,
        session_id: &str,
        window_id: &str,
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
        cmd: Option<&[String]>,
        label: Option<String>,
        keep_after_exit: bool,
    ) -> Result<Arc<Block>, String> {
        let block_id = self.alloc_id("b");
        let (program, args, login, mut extra_env) = match cmd {
            Some(parts) if !parts.is_empty() => (
                parts[0].clone(),
                parts[1..].to_vec(),
                false,
                Vec::new(),
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
                match maybe_int {
                    Some(i) => (shell, i.args, i.login, i.env),
                    None => (shell, Vec::new(), true, Vec::new()),
                }
            }
        };

        // Inject Rex-style context environment variables so CLI & coding agents
        // automatically know which host/session/window/block they inhabit.
        extra_env.push(("TACHYON_SERVER".into(), "local".into()));
        extra_env.push(("TACHYON_SESSION".into(), session_id.to_string()));
        extra_env.push(("TACHYON_WINDOW".into(), window_id.to_string()));
        extra_env.push(("TACHYON_BLOCK".into(), block_id.clone()));

        let cols = cols.max(2);
        let rows = rows.max(1);
        let pty = HostPty::spawn(cols, rows, &program, &args, login, &extra_env, cwd)
            .map_err(|e| format!("failed to spawn `{program}`: {e}"))?;

        let block = Arc::new(Block {
            id: block_id.clone(),
            session_id: session_id.to_string(),
            window_id: window_id.to_string(),
            label: Mutex::new(label),
            created_at: unix_now(),
            pty: Arc::new(pty),
            subscribers: Mutex::new(Vec::new()),
            attached_count: AtomicUsize::new(0),
            queue: Mutex::new((VecDeque::new(), 0, false)),
            work: Condvar::new(),
            space: Condvar::new(),
            parsed: Mutex::new(ParsedBlock {
                term: Term::new(cols as usize, rows as usize, DEFAULT_SCROLLBACK),
                processor: Processor::new(),
                size: (cols, rows),
                last_title_rev: 0,
                last_status_rev: 0,
                awake: false,
            }),
            alive: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            keep_after_exit,
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

        let keep = cmd.is_some();
        let block = self.spawn_block(
            &session_id,
            &window_id,
            cols,
            rows,
            cwd,
            cmd.as_deref(),
            label,
            keep,
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
        let keep = cmd.is_some();
        let block = self.spawn_block(&sid, &window_id, cols, rows, cwd, cmd.as_deref(), label, keep)?;
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

        let keep = cmd.is_some();
        let block = self.spawn_block(&sid, &wid, cols, rows, cwd, cmd.as_deref(), label, keep)?;
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

    fn rpc_block_wait(&self, req: &Value) -> Result<Value, String> {
        let bid = req.get("block_id").and_then(Value::as_str).ok_or("missing block_id")?;
        let timeout = req.get("timeout_ms").and_then(Value::as_u64).map(Duration::from_millis);

        let rx = {
            let mut st = self.inner.lock();
            if let Some((_, code)) = st.recent_exits.iter().find(|(id, _)| id == bid) {
                return Ok(json!({ "block_id": bid, "exit_code": *code }));
            }
            if let Some(b) = st.blocks.get(bid) {
                if let Some(code) = *b.exit_code.lock() {
                    return Ok(json!({ "block_id": bid, "exit_code": code }));
                }
            } else {
                return Err(format!("no block `{bid}`"));
            }
            let (tx, rx) = mpsc::channel();
            st.exit_waiters.entry(bid.to_string()).or_default().push(tx);
            rx
        };

        let code = match timeout {
            Some(d) => rx
                .recv_timeout(d)
                .map_err(|_| format!("timed out waiting for block `{bid}`"))?,
            None => rx
                .recv()
                .map_err(|_| format!("waiter dropped for block `{bid}`"))?,
        };
        Ok(json!({ "block_id": bid, "exit_code": code }))
    }

    fn record_block_exit(&self, block: &Arc<Block>, code: i32) {
        *block.exit_code.lock() = Some(code);
        block.alive.store(false, Ordering::Release);

        let waiters = {
            let mut st = self.inner.lock();
            st.recent_exits.push_back((block.id.clone(), code));
            while st.recent_exits.len() > RECENT_EXITS_CAP {
                st.recent_exits.pop_front();
            }
            st.exit_waiters.remove(&block.id).unwrap_or_default()
        };
        for tx in waiters {
            let _ = tx.send(code);
        }

        self.broadcast_event(json!({
            "event": "block.exited",
            "block_id": block.id,
            "window_id": block.window_id,
            "session_id": block.session_id,
            "exit_code": code,
        }));

        if !block.keep_after_exit {
            self.remove_block_from_tree(&block.id);
        }
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

    let code = block.pty.wait_exit();
    state.record_block_exit(&block, code);
}

/// Parse queued PTY output on a background thread so forwarding never blocks
/// on terminal parsing. Answers terminal queries while no client is attached.
fn parse_block_loop(state: Arc<ServerState>, block: Arc<Block>) {
    loop {
        {
            let mut q = block.queue.lock();
            while q.0.is_empty() && !q.2 {
                block.work.wait(&mut q);
            }
            if q.0.is_empty() {
                return;
            }
        }
        let mut parsed = block.parsed.lock();
        let chunk = {
            let mut q = block.queue.lock();
            let c = q.0.pop_front();
            q.1 -= c.as_ref().map_or(0, Vec::len);
            c
        };
        block.space.notify_one();
        let Some(chunk) = chunk else { continue };
        let (replies, meta_changed, status_changed) = parsed.feed(&block.pty, &chunk);
        drop(parsed);

        if !replies.is_empty() && block.attached_count.load(Ordering::Acquire) == 0 {
            let _ = block.pty.write(&replies);
        }
        if status_changed {
            let info = block.info();
            state.broadcast_event(json!({
                "event": "block.status",
                "block": info.to_json(),
                "status": info.status.as_ref().map(crate::mux::model::status_to_json),
            }));
        }
        if meta_changed {
            state.broadcast_event(json!({
                "event": "block.updated",
                "block": block.info().to_json(),
            }));
        }
    }
}
