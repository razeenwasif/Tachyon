//! Thread-safe multiplexed Mux v2 client.
//!
//! One `MuxClient` wraps a single transport connection (local Unix socket or
//! remote SSH bridge) and runs one background reader thread that demuxes:
//! - `RPC_RES` -> matched by `req_id` to the waiting caller
//! - `EVENT` -> fanned out to registered event subscribers
//! - `PTY_SNAPSHOT` / `PTY_OUT` -> routed by client-allocated `stream_id`
//! - `PONG` -> matched by ping sequence number to compute RTT
//!
//! Because the client allocates `stream_id` *before* sending `block.attach`,
//! there is zero race between `RPC_RES`, `PTY_SNAPSHOT`, and live `PTY_OUT`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use parking_lot::Mutex;
use serde_json::{json, Value};

use super::model::{BlockInfo, HostInfo, ServerSnapshot, SessionInfo, SplitDir, WindowInfo};
use super::proto::{self, Frame, FrameKind, PROTOCOL_VERSION};
use super::transport::{self, ChildGuard, Transport};
use super::Target;

/// Incoming event on an attached block stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamChunk {
    /// Complete restored VT screen (`Term::to_vt()`), sent on attach or resync.
    Snapshot(Vec<u8>),
    /// Live PTY output bytes.
    Output(Vec<u8>),
}

/// Handle to a live attached block stream. Dropping detaches cleanly.
pub struct AttachedStream {
    pub stream_id: u32,
    pub block_id: String,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    rx: Mutex<Receiver<StreamChunk>>,
    client: Arc<MuxClient>,
    detached: AtomicBool,
}

impl AttachedStream {
    /// Block until the next `StreamChunk` arrives (or the stream closes).
    pub fn recv(&self) -> Result<StreamChunk, mpsc::RecvError> {
        self.rx.lock().recv()
    }

    /// Wait up to `timeout` for the next `StreamChunk`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<StreamChunk, mpsc::RecvTimeoutError> {
        self.rx.lock().recv_timeout(timeout)
    }

    /// Send raw bytes (keystrokes, paste, terminal replies) to the block's PTY.
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.client.send_frame(Frame::pty_in(self.stream_id, bytes.to_vec()))
    }

    /// Resize the block's PTY (cells + cell pixel dimensions).
    pub fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> io::Result<()> {
        self.client
            .send_frame(Frame::pty_resize(self.stream_id, cols, rows, cell_w, cell_h))
    }

    /// Explicitly detach this stream from the server.
    pub fn detach(&self) {
        if !self.detached.swap(true, Ordering::AcqRel) {
            self.client.detach_stream(self.stream_id);
        }
    }
}

impl Drop for AttachedStream {
    fn drop(&mut self) {
        self.detach();
    }
}

struct ClientInner {
    writer: Mutex<Box<dyn Write + Send>>,
    pending_rpc: Mutex<HashMap<u32, Sender<io::Result<Value>>>>,
    pending_ping: Mutex<HashMap<u32, Sender<u64>>>,
    streams: Mutex<HashMap<u32, Sender<StreamChunk>>>,
    events: Mutex<Vec<Sender<Value>>>,
    diag: Arc<Mutex<String>>,
    closed: AtomicBool,
    _guard: Option<ChildGuard>,
}

pub struct MuxClient {
    inner: Arc<ClientInner>,
    next_req: AtomicU32,
    next_stream: AtomicU32,
    next_ping: AtomicU32,
    pub host: HostInfo,
    pub client_id: String,
}

impl MuxClient {
    /// Connect to `target`, negotiate `hello`, and start the demux reader thread.
    pub fn connect(target: &Target, auto_start: bool, kind: &str, name: &str) -> io::Result<Arc<Self>> {
        let transport = transport::connect(target, auto_start)?;
        Self::from_transport(transport, kind, name)
    }

    /// Wrap an already-connected transport (also useful for in-process tests).
    pub fn from_transport(transport: Transport, kind: &str, name: &str) -> io::Result<Arc<Self>> {
        let Transport { reader, writer, diag, _guard } = transport;
        let inner = Arc::new(ClientInner {
            writer: Mutex::new(writer),
            pending_rpc: Mutex::new(HashMap::new()),
            pending_ping: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            diag,
            closed: AtomicBool::new(false),
            _guard,
        });

        let reader_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("tachyon-mux-client".into())
            .spawn(move || reader_loop(reader, reader_inner))?;

        let mut client = Self {
            inner,
            next_req: AtomicU32::new(1),
            next_stream: AtomicU32::new(1),
            next_ping: AtomicU32::new(1),
            host: HostInfo {
                hostname: String::new(),
                os: String::new(),
                arch: String::new(),
                version: String::new(),
                protocol: PROTOCOL_VERSION,
                pid: 0,
            },
            client_id: String::new(),
        };

        let reply = client.rpc(json!({
            "op": "hello",
            "protocol": PROTOCOL_VERSION,
            "client_kind": kind,
            "client_name": name,
        }))?;

        client.host = HostInfo::from_json(reply.get("host").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing host info in hello"))?;
        client.client_id = reply
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        Ok(Arc::new(client))
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    fn send_frame(&self, frame: Frame) -> io::Result<()> {
        let mut w = self.inner.writer.lock();
        proto::write_frame(&mut *w, &frame).map_err(|e| self.decorate_err(e))
    }

    fn decorate_err(&self, err: io::Error) -> io::Error {
        let note = self.inner.diag.lock().trim().to_string();
        if note.is_empty() {
            err
        } else {
            io::Error::new(err.kind(), format!("{err} (remote stderr: {note})"))
        }
    }

    /// Execute a request/response JSON RPC call.
    pub fn rpc(&self, body: Value) -> io::Result<Value> {
        let req_id = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.inner.pending_rpc.lock().insert(req_id, tx);

        if let Err(e) = self.send_frame(Frame::rpc_req(req_id, &body)?) {
            self.inner.pending_rpc.lock().remove(&req_id);
            return Err(e);
        }

        let res = rx
            .recv()
            .map_err(|_| self.decorate_err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed")))??;

        if res.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(res.get("data").cloned().unwrap_or(Value::Null))
        } else {
            let msg = res
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("server returned error");
            Err(io::Error::other(msg.to_string()))
        }
    }

    /// Measure round-trip latency to the server using `PING`/`PONG`.
    pub fn ping(&self, timeout: Duration) -> io::Result<Duration> {
        let seq = self.next_ping.fetch_add(1, Ordering::Relaxed);
        let sent = super::mono_now_ns();
        let (tx, rx) = mpsc::channel();
        self.inner.pending_ping.lock().insert(seq, tx);

        if let Err(e) = self.send_frame(Frame::ping(seq, sent)) {
            self.inner.pending_ping.lock().remove(&seq);
            return Err(e);
        }

        match rx.recv_timeout(timeout) {
            Ok(echoed) => {
                let now = super::mono_now_ns();
                Ok(Duration::from_nanos(now.saturating_sub(echoed)))
            }
            Err(_) => {
                self.inner.pending_ping.lock().remove(&seq);
                Err(io::Error::new(io::ErrorKind::TimedOut, "ping timed out"))
            }
        }
    }

    /// Fetch a full snapshot of Sessions, Windows, Blocks, and Clients.
    pub fn state(&self) -> io::Result<ServerSnapshot> {
        let data = self.rpc(json!({ "op": "state.get" }))?;
        ServerSnapshot::from_json(&data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid state.get response"))
    }

    /// Subscribe to live server events (`session.*`, `window.*`, `block.*`).
    pub fn subscribe_events(&self) -> io::Result<Receiver<Value>> {
        let (tx, rx) = mpsc::channel();
        self.inner.events.lock().push(tx);
        self.rpc(json!({ "op": "events.subscribe" }))?;
        Ok(rx)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        name: Option<&str>,
        cwd: Option<&str>,
        cmd: Option<&[String]>,
        label: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> io::Result<(SessionInfo, WindowInfo, BlockInfo)> {
        let mut req = json!({
            "op": "session.create",
            "cols": cols,
            "rows": rows,
        });
        if let Some(n) = name {
            req["name"] = json!(n);
        }
        if let Some(d) = cwd {
            req["cwd"] = json!(d);
        }
        if let Some(c) = cmd {
            req["cmd"] = json!(c);
        }
        if let Some(l) = label {
            req["label"] = json!(l);
        }
        let data = self.rpc(req)?;
        let session = SessionInfo::from_json(data.get("session").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing session"))?;
        let window = WindowInfo::from_json(data.get("window").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing window"))?;
        let block = BlockInfo::from_json(data.get("block").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing block"))?;
        Ok((session, window, block))
    }

    pub fn rename_session(&self, session_id: &str, name: &str) -> io::Result<SessionInfo> {
        let data = self.rpc(json!({
            "op": "session.rename",
            "session_id": session_id,
            "name": name,
        }))?;
        SessionInfo::from_json(data.get("session").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing session"))
    }

    pub fn kill_session(&self, session_id: &str) -> io::Result<()> {
        self.rpc(json!({ "op": "session.kill", "session_id": session_id }))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_window(
        &self,
        session_id: &str,
        name: Option<&str>,
        cwd: Option<&str>,
        cmd: Option<&[String]>,
        label: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> io::Result<(WindowInfo, BlockInfo)> {
        let mut req = json!({
            "op": "window.create",
            "session_id": session_id,
            "cols": cols,
            "rows": rows,
        });
        if let Some(n) = name {
            req["name"] = json!(n);
        }
        if let Some(d) = cwd {
            req["cwd"] = json!(d);
        }
        if let Some(c) = cmd {
            req["cmd"] = json!(c);
        }
        if let Some(l) = label {
            req["label"] = json!(l);
        }
        let data = self.rpc(req)?;
        let window = WindowInfo::from_json(data.get("window").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing window"))?;
        let block = BlockInfo::from_json(data.get("block").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing block"))?;
        Ok((window, block))
    }

    pub fn rename_window(&self, window_id: &str, name: &str) -> io::Result<WindowInfo> {
        let data = self.rpc(json!({
            "op": "window.rename",
            "window_id": window_id,
            "name": name,
        }))?;
        WindowInfo::from_json(data.get("window").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing window"))
    }

    pub fn kill_window(&self, window_id: &str) -> io::Result<()> {
        self.rpc(json!({ "op": "window.kill", "window_id": window_id }))?;
        Ok(())
    }

    pub fn focus_window(&self, window_id: &str, block_id: Option<&str>) -> io::Result<()> {
        let mut req = json!({ "op": "window.focus", "window_id": window_id });
        if let Some(b) = block_id {
            req["block_id"] = json!(b);
        }
        self.rpc(req)?;
        Ok(())
    }

    pub fn set_window_zoom(&self, window_id: &str, zoomed: bool) -> io::Result<()> {
        self.rpc(json!({ "op": "window.set_zoom", "window_id": window_id, "zoomed": zoomed }))?;
        Ok(())
    }

    pub fn set_window_ratio(&self, window_id: &str, path: &[bool], ratio: f32) -> io::Result<()> {
        self.rpc(json!({
            "op": "window.set_ratio",
            "window_id": window_id,
            "path": path,
            "ratio": ratio,
        }))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_block(
        &self,
        target_block: &str,
        dir: SplitDir,
        cwd: Option<&str>,
        cmd: Option<&[String]>,
        label: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> io::Result<(WindowInfo, BlockInfo)> {
        let mut req = json!({
            "op": "block.split",
            "block_id": target_block,
            "dir": dir.as_str(),
            "cols": cols,
            "rows": rows,
        });
        if let Some(d) = cwd {
            req["cwd"] = json!(d);
        }
        if let Some(c) = cmd {
            req["cmd"] = json!(c);
        }
        if let Some(l) = label {
            req["label"] = json!(l);
        }
        let data = self.rpc(req)?;
        let window = WindowInfo::from_json(data.get("window").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing window"))?;
        let block = BlockInfo::from_json(data.get("block").unwrap_or(&Value::Null))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing block"))?;
        Ok((window, block))
    }

    pub fn kill_block(&self, block_id: &str) -> io::Result<()> {
        self.rpc(json!({ "op": "block.kill", "block_id": block_id }))?;
        Ok(())
    }

    pub fn set_awake(&self, block_id: &str, on: bool) -> io::Result<()> {
        self.rpc(json!({ "op": "block.set_awake", "block_id": block_id, "awake": on }))?;
        Ok(())
    }

    /// Send input bytes to a block without attaching a stream.
    pub fn send_input(&self, block_id: &str, data: &[u8]) -> io::Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        self.rpc(json!({
            "op": "block.send",
            "block_id": block_id,
            "data_b64": b64,
        }))?;
        Ok(())
    }

    /// Capture rendered screen (and optional scrollback lines) from a block.
    pub fn capture(&self, block_id: &str, lines: Option<usize>, ansi: bool) -> io::Result<String> {
        let mut req = json!({
            "op": "block.capture",
            "block_id": block_id,
            "ansi": ansi,
        });
        if let Some(n) = lines {
            req["lines"] = json!(n);
        }
        let data = self.rpc(req)?;
        Ok(data
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Block until `block_id` exits (or optional `timeout_ms` elapses) and return its exit code.
    pub fn wait_block(&self, block_id: &str, timeout_ms: Option<u64>) -> io::Result<i32> {
        let mut req = json!({
            "op": "block.wait",
            "block_id": block_id,
        });
        if let Some(ms) = timeout_ms {
            req["timeout_ms"] = json!(ms);
        }
        let data = self.rpc(req)?;
        Ok(data.get("exit_code").and_then(Value::as_i64).unwrap_or(0) as i32)
    }

    /// Attach a live stream to `block_id`. The first item on `rx` (when `restore`
    /// is true) is `StreamChunk::Snapshot(vt_bytes)`, followed gap-free by live
    /// `StreamChunk::Output(bytes)`.
    pub fn attach(self: &Arc<Self>, block_id: &str, interactive: bool, restore: bool) -> io::Result<AttachedStream> {
        let stream_id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        // Register before sending RPC so snapshots/output can never race ahead.
        self.inner.streams.lock().insert(stream_id, tx);

        let reply = self.rpc(json!({
            "op": "block.attach",
            "block_id": block_id,
            "stream_id": stream_id,
            "mode": if interactive { "interactive" } else { "readonly" },
            "restore": restore,
        }));

        let data = match reply {
            Ok(d) => d,
            Err(e) => {
                self.inner.streams.lock().remove(&stream_id);
                return Err(e);
            }
        };

        Ok(AttachedStream {
            stream_id,
            block_id: block_id.to_string(),
            cols: data.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16,
            rows: data.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16,
            alive: data.get("alive").and_then(Value::as_bool).unwrap_or(true),
            rx: Mutex::new(rx),
            client: Arc::clone(self),
            detached: AtomicBool::new(false),
        })
    }

    fn detach_stream(&self, stream_id: u32) {
        self.inner.streams.lock().remove(&stream_id);
        let _ = self.rpc(json!({ "op": "block.detach", "stream_id": stream_id }));
    }

    /// Request a graceful server shutdown (`tachyon server stop`).
    pub fn shutdown_server(&self) -> io::Result<()> {
        self.rpc(json!({ "op": "server.shutdown" }))?;
        Ok(())
    }
}

fn reader_loop(mut reader: Box<dyn Read + Send>, inner: Arc<ClientInner>) {
    loop {
        match proto::read_frame(&mut reader) {
            Ok(frame) => match frame.kind {
                FrameKind::RpcRes => {
                    let parsed = frame.parse_json();
                    if let Some(tx) = inner.pending_rpc.lock().remove(&frame.channel) {
                        let _ = tx.send(parsed);
                    }
                }
                FrameKind::Event => {
                    if let Ok(val) = frame.parse_json() {
                        let mut subs = inner.events.lock();
                        subs.retain(|tx| tx.send(val.clone()).is_ok());
                    }
                }
                FrameKind::PtySnapshot => {
                    if let Ok(vt) = proto::decode_snapshot(&frame.payload) {
                        if let Some(tx) = inner.streams.lock().get(&frame.channel).cloned() {
                            let _ = tx.send(StreamChunk::Snapshot(vt));
                        }
                    }
                }
                FrameKind::PtyOut => {
                    if let Some(tx) = inner.streams.lock().get(&frame.channel).cloned() {
                        let _ = tx.send(StreamChunk::Output(frame.payload));
                    }
                }
                FrameKind::Pong => {
                    if let Ok(ts) = proto::decode_timestamp(&frame.payload) {
                        if let Some(tx) = inner.pending_ping.lock().remove(&frame.channel) {
                            let _ = tx.send(ts);
                        }
                    }
                }
                FrameKind::Ping => {
                    if let Ok(ts) = proto::decode_timestamp(&frame.payload) {
                        let mut w = inner.writer.lock();
                        let _ = proto::write_frame(&mut *w, &Frame::pong(frame.channel, ts));
                    }
                }
                FrameKind::RpcReq | FrameKind::PtyIn | FrameKind::PtyResize => {}
            },
            Err(e) => {
                inner.closed.store(true, Ordering::Release);
                let note = inner.diag.lock().trim().to_string();
                let msg = if note.is_empty() {
                    format!("transport closed: {e}")
                } else {
                    format!("transport closed: {e} (remote stderr: {note})")
                };
                for (_, tx) in inner.pending_rpc.lock().drain() {
                    let _ = tx.send(Err(io::Error::new(e.kind(), msg.clone())));
                }
                inner.pending_ping.lock().clear();
                inner.streams.lock().clear();
                inner.events.lock().clear();
                break;
            }
        }
    }
}
