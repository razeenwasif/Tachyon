//! The session host: a small process that owns the shells, so quitting,
//! crashing or updating the app leaves them running.
//!
//! It's the Tachyon binary started as `tachyon --host <socket>`, detached in
//! its own session, and it lives while it has shells or a connected app. For
//! each shell it holds the PTY, reads its output, sends it on to the app when
//! one is attached, and parses it into a terminal of its own, so a reopened
//! app can be handed the screen as it is ([`Term::to_vt`]). While no app is
//! attached it answers the terminal queries programs send, so they don't hang
//! waiting.
//!
//! The app talks to it over a Unix socket in its private temporary folder,
//! one per app binary and protocol version. Each shell's PTY and a stream of
//! its output are passed to the app as file descriptors: the app writes keys
//! and resizes straight to the PTY, and reads output from the stream exactly
//! as it read the PTY before. Each stream starts with eight bytes giving the
//! length of the restored screen that follows, zero for a new shell.
//!
//! Requests are fields separated by 0x1F, framed by a four-byte length.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Condvar, Mutex};

use super::power;
use super::pty::Pty;
use crate::term::{Processor, Term};

/// Bumped when requests or replies change shape. An app only talks to a host
/// of its own version (the socket's name carries it), so after an update the
/// old host keeps its shells and a new one starts beside it.
pub const PROTOCOL: u32 = 1;

const SEP: char = '\u{1F}';
const LIST_SEP: char = '\u{1E}';

/// Where the host for this app binary listens.
pub fn socket_path() -> PathBuf {
    use std::hash::{Hash, Hasher};
    let exe = std::env::current_exe().unwrap_or_default();
    let mut h = rustc_hash::FxHasher::default();
    exe.hash(&mut h);
    let dir = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join(format!("tachyon-host-{:08x}-v{PROTOCOL}.sock", h.finish() as u32))
}

// ---------------------------------------------------------------------------
// Framing, with file descriptors passed alongside
// ---------------------------------------------------------------------------

fn send(sock: &UnixStream, fields: &[&str], fds: &[RawFd]) -> io::Result<()> {
    let body = fields.iter().map(|f| f.replace(SEP, " ")).collect::<Vec<_>>().join(&SEP.to_string());
    let mut msg = (body.len() as u32).to_le_bytes().to_vec();
    msg.extend_from_slice(body.as_bytes());
    let mut iov = libc::iovec { iov_base: msg.as_mut_ptr().cast(), iov_len: msg.len() };
    let space = unsafe { libc::CMSG_SPACE((fds.len() * size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space.max(1)];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if !fds.is_empty() {
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = space as _;
        // SAFETY: the control buffer is sized by CMSG_SPACE for these fds.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * size_of::<RawFd>()) as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast(), fds.len());
        }
    }
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // The fds went with the first byte; send whatever didn't fit plainly.
    (&*sock).write_all(&msg[n as usize..])
}

fn recv(sock: &UnixStream) -> io::Result<(Vec<String>, Vec<OwnedFd>)> {
    let mut len = [0u8; 4];
    let mut iov = libc::iovec { iov_base: len.as_mut_ptr().cast(), iov_len: 4 };
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE((4 * size_of::<RawFd>()) as u32) } as usize];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = control.as_mut_ptr().cast();
    hdr.msg_controllen = control.len() as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut hdr, 0) };
        if n >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break n;
        }
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel filled in.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
                let data = libc::CMSG_DATA(c) as *const RawFd;
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
    }
    (&*sock).read_exact(&mut len[n as usize..])?;
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    (&*sock).read_exact(&mut body)?;
    let text = String::from_utf8_lossy(&body);
    Ok((text.split(SEP).map(str::to_string).collect(), fds))
}

// ---------------------------------------------------------------------------
// The app's side
// ---------------------------------------------------------------------------

/// A shell the host runs, as the app gets it.
pub struct Remote {
    pub id: u64,
    pub pid: libc::pid_t,
    /// The terminal's size at the host, which the restored screen is for.
    pub cols: usize,
    pub rows: usize,
    pub pty: Pty,
    /// It has keep-awake on, for the running command or the next.
    pub awake: bool,
}

/// A shell the host has, from [`Client::list`].
#[derive(Debug, Clone)]
pub struct Listed {
    pub id: u64,
    pub attached: bool,
}

/// The app's connection to the host.
pub struct Client {
    sock: UnixStream,
}

impl Client {
    /// Connect to this app's host, starting it first if it isn't running.
    pub fn connect() -> io::Result<Client> {
        Client::connect_to(&socket_path(), true)
    }

    /// Connect to the host at `path`, starting one there if `start`.
    pub fn connect_to(path: &Path, start: bool) -> io::Result<Client> {
        let sock = match UnixStream::connect(path) {
            Ok(s) => s,
            Err(e) if !start => return Err(e),
            Err(_) => {
                start_host(path)?;
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    match UnixStream::connect(path) {
                        Ok(s) => break s,
                        Err(e) if Instant::now() > deadline => return Err(e),
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            }
        };
        let c = Client { sock };
        let reply = c.ask(&["hello", &PROTOCOL.to_string()], &[])?.0;
        if reply.first().map(String::as_str) != Some("ok") {
            return Err(io::Error::other(format!("host refused: {reply:?}")));
        }
        Ok(c)
    }

    fn ask(&self, fields: &[&str], fds: &[RawFd]) -> io::Result<(Vec<String>, Vec<OwnedFd>)> {
        send(&self.sock, fields, fds)?;
        let (reply, fds) = recv(&self.sock)?;
        if reply.first().map(String::as_str) == Some("error") {
            return Err(io::Error::other(reply.get(1).cloned().unwrap_or_default()));
        }
        Ok((reply, fds))
    }

    /// Start a shell at the host.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        cols: usize,
        rows: usize,
        scrollback: usize,
        program: &str,
        args: &[String],
        login: bool,
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> io::Result<Remote> {
        let args = args.join(&LIST_SEP.to_string());
        let env = env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(&LIST_SEP.to_string());
        let (cols_s, rows_s, sb) = (cols.to_string(), rows.to_string(), scrollback.to_string());
        let reply = self.ask(
            &["spawn", &cols_s, &rows_s, &sb, program, if login { "1" } else { "0" }, cwd.unwrap_or(""), &args, &env],
            &[],
        )?;
        remote(reply)
    }

    /// Take over a shell the host already runs: its restored screen comes
    /// first on the output stream.
    pub fn attach(&self, id: u64) -> io::Result<Remote> {
        remote(self.ask(&["attach", &id.to_string()], &[])?)
    }

    pub fn list(&self) -> io::Result<Vec<Listed>> {
        let (reply, _) = self.ask(&["list"], &[])?;
        Ok(reply
            .get(1)
            .map(|s| {
                s.split(LIST_SEP)
                    .filter_map(|e| {
                        let (id, attached) = e.split_once(':')?;
                        Some(Listed { id: id.parse().ok()?, attached: attached == "1" })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Keep the Mac awake while shell `id`'s command runs (or its next one).
    pub fn set_awake(&self, id: u64, on: bool) -> io::Result<()> {
        self.ask(&["awake", &id.to_string(), if on { "1" } else { "0" }], &[]).map(|_| ())
    }

    /// Keep the app's windows, tabs and splits, to put them back next time.
    pub fn set_layout(&self, layout: &str) -> io::Result<()> {
        self.ask(&["layout-set", layout], &[]).map(|_| ())
    }

    pub fn layout(&self) -> io::Result<String> {
        Ok(self.ask(&["layout-get"], &[])?.0.get(1).cloned().unwrap_or_default())
    }
}

fn remote((reply, mut fds): (Vec<String>, Vec<OwnedFd>)) -> io::Result<Remote> {
    let field = |i: usize| reply.get(i).and_then(|s| s.parse::<u64>().ok()).ok_or_else(|| io::Error::other("bad reply"));
    let (id, pid, cols, rows) = (field(1)?, field(2)?, field(3)?, field(4)?);
    if fds.len() != 2 {
        return Err(io::Error::other("the host sent no terminal"));
    }
    let output = fds.pop().expect("two");
    let master = fds.pop().expect("two");
    Ok(Remote {
        id,
        pid: pid as libc::pid_t,
        cols: cols as usize,
        rows: rows as usize,
        pty: Pty::remote(master, pid as libc::pid_t, output),
        awake: reply.get(5).map(String::as_str) == Some("1"),
    })
}

/// Start the host, detached from the app so it outlives it.
fn start_host(path: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--host").arg(path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // It runs on its own; the reaper only collects the exit status if it
    // ever ends while the app is still running.
    std::thread::Builder::new().name("tachyon-host-reaper".into()).spawn(move || {
        let mut child = child;
        let _ = child.wait();
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The host's side
// ---------------------------------------------------------------------------

/// A shell the host runs. Output is forwarded to the app as soon as it's
/// read, and parsed into the host's own terminal on a thread of its own, so
/// parsing never holds up forwarding. A hand-over takes the forwarding lock
/// (so no more output is sent), then parses whatever is still queued, which
/// makes the restored screen and the stream after it meet exactly.
struct Session {
    id: u64,
    pty: Arc<Pty>,
    /// The attached app's end of the output stream.
    client: Mutex<Option<UnixStream>>,
    attached: AtomicBool,
    /// Output read but not yet parsed, and whether the shell has ended.
    queue: Mutex<(VecDeque<Vec<u8>>, usize, bool)>,
    work: Condvar,
    space: Condvar,
    term: Mutex<Parsed>,
}

struct Parsed {
    term: Term,
    processor: Processor,
    /// The PTY size the terminal was last resized to.
    size: (u16, u16),
    /// A command here was keeping the Mac awake, last we looked.
    awake: bool,
}

/// Output the parser may fall behind by before reading waits for it.
const MAX_QUEUED: usize = 32 << 20;

impl Parsed {
    /// Parse a chunk, following the PTY's size first: the app resizes the
    /// PTY directly.
    fn feed(&mut self, pty: &Pty, chunk: &[u8]) -> Vec<u8> {
        let ws = pty.size();
        if ws != self.size {
            self.size = ws;
            self.term.resize(ws.0.max(1) as usize, ws.1.max(1) as usize);
        }
        self.processor.advance(&mut self.term, chunk);
        self.term.take_command_events();
        self.term.take_dirty();
        self.term.take_replies()
    }

    /// Hold or release the power assertion for shell `id` when its
    /// keep-awake command starts or ends.
    fn sync_awake(&mut self, id: u64) {
        let now = self.term.keeps_awake();
        if now != self.awake {
            self.awake = now;
            power::set(id, now.then(|| self.term.running_cmdline().unwrap_or("").to_string()).as_deref());
        }
    }
}

#[derive(Default)]
struct Host {
    sessions: BTreeMap<u64, Arc<Session>>,
    next_id: u64,
    layout: String,
    clients: usize,
    /// When it last had neither shells nor an app, to exit after a while.
    idle_since: Option<Instant>,
}

/// How long the host lingers with nothing to do before exiting.
const IDLE_EXIT: Duration = Duration::from_secs(20);

/// Run the host on `path` until it has had no shells and no app for a while.
pub fn run(path: &Path) -> io::Result<()> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::umask(0o077);
    }
    // A socket file left by a host that crashed: nobody answers it.
    if path.exists() && UnixStream::connect(path).is_err() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    crate::log!("host: listening on {}", path.display());
    let host = Arc::new(Mutex::new(Host { next_id: 1, idle_since: Some(Instant::now()), ..Host::default() }));

    let watcher = Arc::clone(&host);
    let socket = path.to_path_buf();
    std::thread::Builder::new().name("tachyon-host-idle".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        let h = watcher.lock();
        if h.idle_since.is_some_and(|t| t.elapsed() > IDLE_EXIT) {
            crate::log!("host: nothing to do, exiting");
            let _ = std::fs::remove_file(&socket);
            std::process::exit(0);
        }
    })?;

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        // Only this user's processes.
        let (mut uid, mut gid) = (0, 0);
        if unsafe { libc::getpeereid(conn.as_raw_fd(), &mut uid, &mut gid) } != 0 || uid != unsafe { libc::getuid() } {
            continue;
        }
        let host = Arc::clone(&host);
        let _ = std::thread::Builder::new().name("tachyon-host-client".into()).spawn(move || serve(host, conn));
    }
    Ok(())
}

fn update_idle(h: &mut Host) {
    h.idle_since = if h.sessions.is_empty() && h.clients == 0 { Some(h.idle_since.unwrap_or_else(Instant::now)) } else { None };
}

/// Answer one app's requests until it disconnects.
fn serve(host: Arc<Mutex<Host>>, conn: UnixStream) {
    {
        let mut h = host.lock();
        h.clients += 1;
        update_idle(&mut h);
    }
    while let Ok((req, _)) = recv(&conn) {
        let verb = req.first().map(String::as_str).unwrap_or("");
        let result = match verb {
            "hello" => {
                let theirs = req.get(1).and_then(|v| v.parse::<u32>().ok());
                if theirs == Some(PROTOCOL) {
                    send(&conn, &["ok", &PROTOCOL.to_string()], &[])
                } else {
                    send(&conn, &["error", "protocol version"], &[])
                }
            }
            "spawn" => spawn_session(&host, &conn, &req),
            "attach" => match req.get(1).and_then(|s| s.parse::<u64>().ok()).and_then(|id| host.lock().sessions.get(&id).cloned()) {
                Some(s) => hand_over(&conn, &s, true),
                None => send(&conn, &["error", "no such session"], &[]),
            },
            "list" => {
                let list = host
                    .lock()
                    .sessions
                    .values()
                    .map(|s| {
                        let mut client = s.client.lock();
                        // An app that quit while its shell was quiet: nothing
                        // has been written to find out yet.
                        if client.as_ref().is_some_and(|c| !alive(c)) {
                            *client = None;
                            s.attached.store(false, Ordering::Release);
                        }
                        format!("{}:{}", s.id, if client.is_some() { 1 } else { 0 })
                    })
                    .collect::<Vec<_>>()
                    .join(&LIST_SEP.to_string());
                send(&conn, &["sessions", &list], &[])
            }
            "awake" => {
                let s = req.get(1).and_then(|s| s.parse::<u64>().ok()).and_then(|id| host.lock().sessions.get(&id).cloned());
                match s {
                    Some(s) => {
                        let mut parsed = s.term.lock();
                        parsed.term.set_keep_awake(req.get(2).map(String::as_str) == Some("1"));
                        parsed.sync_awake(s.id);
                        send(&conn, &["ok"], &[])
                    }
                    None => send(&conn, &["error", "no such session"], &[]),
                }
            }
            "layout-set" => {
                host.lock().layout = req.get(1).cloned().unwrap_or_default();
                send(&conn, &["ok"], &[])
            }
            "layout-get" => {
                let layout = host.lock().layout.clone();
                send(&conn, &["layout", &layout], &[])
            }
            _ => send(&conn, &["error", "unknown request"], &[]),
        };
        if result.is_err() {
            break;
        }
    }
    let mut h = host.lock();
    h.clients -= 1;
    update_idle(&mut h);
}

fn spawn_session(host: &Arc<Mutex<Host>>, conn: &UnixStream, req: &[String]) -> io::Result<()> {
    let num = |i: usize| req.get(i).and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
    let (cols, rows, scrollback) = (num(1).max(1), num(2).max(1), num(3));
    let program = req.get(4).cloned().unwrap_or_default();
    let login = req.get(5).map(String::as_str) == Some("1");
    let cwd = req.get(6).filter(|s| !s.is_empty()).cloned();
    let split = |i: usize| req.get(i).filter(|s| !s.is_empty()).map(|s| s.split(LIST_SEP).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
    let args = split(7);
    let env: Vec<(String, String)> =
        split(8).into_iter().filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let pty = match Pty::spawn(cols as u16, rows as u16, &program, &args, login, &env, cwd.as_deref()) {
        Ok(p) => Arc::new(p),
        Err(e) => return send(conn, &["error", &format!("could not start {program}: {e}")], &[]),
    };
    let session = {
        let mut h = host.lock();
        let id = h.next_id;
        h.next_id += 1;
        let s = Arc::new(Session {
            id,
            pty,
            client: Mutex::new(None),
            attached: AtomicBool::new(false),
            queue: Mutex::new((VecDeque::new(), 0, false)),
            work: Condvar::new(),
            space: Condvar::new(),
            term: Mutex::new(Parsed {
                term: Term::new(cols, rows, scrollback),
                processor: Processor::new(),
                size: (cols as u16, rows as u16),
                awake: false,
            }),
        });
        h.sessions.insert(id, Arc::clone(&s));
        update_idle(&mut h);
        s
    };
    crate::log!("host: session {} started, pid {}", session.id, session.pty.pid());
    let reader = Arc::clone(&session);
    let host = Arc::clone(host);
    std::thread::Builder::new().name(format!("tachyon-host-reader-{}", session.id)).spawn(move || read_session(host, reader))?;
    let parser = Arc::clone(&session);
    std::thread::Builder::new().name(format!("tachyon-host-parser-{}", session.id)).spawn(move || {
        // Background work: it may run on the efficiency cores.
        unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
        parse_session(parser)
    })?;
    hand_over(conn, &session, false)
}

/// Give the app a session: reply with its PTY and a fresh output stream, then
/// start the stream with the restored screen (empty for a new shell) and
/// switch live output to it, all under the session's lock so nothing slips
/// between the two.
fn hand_over(conn: &UnixStream, s: &Session, restore: bool) -> io::Result<()> {
    let (ours, theirs) = UnixStream::pair()?;
    // macOS gives Unix streams 8 KB buffers, which would cut every big read
    // into many small writes; give the output stream room for a whole one.
    for (sock, opt) in [(&ours, libc::SO_SNDBUF), (&theirs, libc::SO_RCVBUF)] {
        let size: libc::c_int = 1 << 20;
        unsafe {
            libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, opt, (&size as *const libc::c_int).cast(), size_of::<libc::c_int>() as u32);
        }
    }
    let master = unsafe { libc::dup(s.pty.master_fd()) };
    if master < 0 {
        return send(conn, &["error", "dup failed"], &[]);
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // Stop forwarding, and catch the host's terminal up with everything
    // already forwarded.
    let mut client = s.client.lock();
    let mut parsed = s.term.lock();
    let pending: Vec<Vec<u8>> = {
        let mut q = s.queue.lock();
        q.1 = 0;
        q.0.drain(..).collect()
    };
    s.space.notify_all();
    for chunk in pending {
        parsed.feed(&s.pty, &chunk);
    }
    parsed.sync_awake(s.id);
    let (cols, rows, awake) = (parsed.term.cols(), parsed.term.rows(), parsed.term.awake_armed());
    send(
        conn,
        &["session", &s.id.to_string(), &s.pty.pid().to_string(), &cols.to_string(), &rows.to_string(), if awake { "1" } else { "0" }],
        &[master.as_raw_fd(), theirs.as_raw_fd()],
    )?;
    drop(master);
    drop(theirs);
    let screen = if restore { parsed.term.to_vt() } else { Vec::new() };
    drop(parsed);
    let mut out = ours;
    let ok = out.write_all(&(screen.len() as u64).to_le_bytes()).and_then(|_| out.write_all(&screen));
    // A newer app taking over drops the older one's stream.
    *client = ok.is_ok().then_some(out);
    s.attached.store(client.is_some(), Ordering::Release);
    crate::log!("host: session {} handed over, {} bytes restored", s.id, screen.len());
    Ok(())
}

/// Whether the app at the other end of an output stream is still there. It
/// never writes to it, so anything to read means it closed its end.
fn alive(c: &UnixStream) -> bool {
    let mut b = 0u8;
    let n = unsafe { libc::recv(c.as_raw_fd(), (&mut b as *mut u8).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN)
}

/// Read a shell's output until it exits: straight on to the app, and queued
/// for the host's own terminal.
fn read_session(host: Arc<Mutex<Host>>, s: Arc<Session>) {
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = match s.pty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut client = s.client.lock();
        if let Some(c) = client.as_mut() {
            if c.write_all(&buf[..n]).is_err() {
                *client = None;
                s.attached.store(false, Ordering::Release);
            }
        }
        let mut q = s.queue.lock();
        while q.1 > MAX_QUEUED {
            s.space.wait(&mut q);
        }
        q.0.push_back(buf[..n].to_vec());
        q.1 += n;
        s.work.notify_one();
    }
    crate::log!("host: session {} ended", s.id);
    s.queue.lock().2 = true;
    s.work.notify_one();
    // Dropping the stream tells the app the shell is gone.
    *s.client.lock() = None;
    let mut h = host.lock();
    h.sessions.remove(&s.id);
    update_idle(&mut h);
}

/// Parse a shell's queued output into the host's terminal until it ends.
fn parse_session(s: Arc<Session>) {
    loop {
        {
            let mut q = s.queue.lock();
            while q.0.is_empty() && !q.2 {
                s.work.wait(&mut q);
            }
            if q.0.is_empty() {
                // The shell ended: it keeps nothing awake.
                power::set(s.id, None);
                return;
            }
        }
        // Pop under the terminal's lock, so a hand-over that drains the queue
        // never misses a chunk taken but not yet parsed.
        let mut parsed = s.term.lock();
        let chunk = {
            let mut q = s.queue.lock();
            let c = q.0.pop_front();
            q.1 -= c.as_ref().map_or(0, Vec::len);
            c
        };
        s.space.notify_one();
        let Some(chunk) = chunk else { continue };
        let replies = parsed.feed(&s.pty, &chunk);
        parsed.sync_awake(s.id);
        drop(parsed);
        // Nobody else is answering: answer queries, so programs don't wait.
        if !replies.is_empty() && !s.attached.load(Ordering::Acquire) {
            let _ = s.pty.write(&replies);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything a stream carries until it ends or `want` shows up.
    fn read_until(pty: &Pty, want: &str) -> String {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !String::from_utf8_lossy(&got).contains(want) {
            match pty.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&got).into_owned()
    }

    /// What the host costs a flood of output: `cat` of a file through the
    /// host (forwarded and parsed there too) against straight from a PTY,
    /// both parsed here as the app does. Run with `TACHYON_BENCH_FILE=<big
    /// file> cargo test --release host_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn host_cost() {
        let Some(file) = std::env::var_os("TACHYON_BENCH_FILE") else { return };
        let file = file.to_string_lossy().into_owned();
        let size = std::fs::metadata(&file).unwrap().len() as f64 / (1 << 20) as f64;
        let dir = std::env::temp_dir().join(format!("tachyon-host-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sock");
        let p = path.clone();
        std::thread::spawn(move || run(&p));
        std::thread::sleep(Duration::from_millis(200));
        let client = Client::connect_to(&path, false).unwrap();
        let args = vec![file.clone()];
        let drain = |pty: &Pty| {
            let mut t = Term::new(200, 50, 10_000);
            let mut p = Processor::new();
            let mut buf = vec![0u8; 128 * 1024];
            let start = Instant::now();
            loop {
                match pty.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => p.advance(&mut t, &buf[..n]),
                }
            }
            start.elapsed().as_secs_f64()
        };
        let mut best = (f64::MAX, f64::MAX);
        for _ in 0..4 {
            let local = Pty::spawn(200, 50, "/bin/cat", &args, false, &[], None).unwrap();
            best.0 = best.0.min(drain(&local));
            let r = client.spawn(200, 50, 10_000, "/bin/cat", &args, false, &[], None).unwrap();
            let mut header = [0u8; 8];
            let mut got = 0;
            while got < 8 {
                got += r.pty.read(&mut header[got..]).unwrap();
            }
            best.1 = best.1.min(drain(&r.pty));
        }
        println!(
            "straight from a PTY: {:.0} MiB/s; through the host: {:.0} MiB/s ({:+.1}%)",
            size / best.0,
            size / best.1,
            (best.0 / best.1 - 1.0) * 100.0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Keystroke echo through the host against straight from a PTY: a key
    /// written to `cat` until it comes back. Run with `cargo test --release
    /// host_echo -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn host_echo() {
        let dir = std::env::temp_dir().join(format!("tachyon-host-echo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sock");
        let p = path.clone();
        std::thread::spawn(move || run(&p));
        std::thread::sleep(Duration::from_millis(200));
        let client = Client::connect_to(&path, false).unwrap();
        let echo = |pty: &Pty| {
            let mut buf = [0u8; 64];
            let mut times: Vec<f64> = (0..500)
                .map(|_| {
                    let start = Instant::now();
                    pty.write(b"x").unwrap();
                    let _ = pty.read(&mut buf);
                    start.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            times.sort_by(f64::total_cmp);
            (times[times.len() / 2], times[times.len() * 99 / 100])
        };
        let local = Pty::spawn(80, 24, "/bin/cat", &[], false, &[], None).unwrap();
        let r = client.spawn(80, 24, 100, "/bin/cat", &[], false, &[], None).unwrap();
        let mut header = [0u8; 8];
        let mut got = 0;
        while got < 8 {
            got += r.pty.read(&mut header[got..]).unwrap();
        }
        let (a, b) = (echo(&local), echo(&r.pty));
        println!("echo straight: median {:.0} µs, p99 {:.0} µs; through the host: median {:.0} µs, p99 {:.0} µs", a.0, a.1, b.0, b.1);
        local.hangup();
        r.pty.hangup();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_host_keeps_the_mac_awake_for_one_command_even_with_no_app() {
        let dir = std::env::temp_dir().join(format!("tachyon-host-awake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sock");
        let p = path.clone();
        std::thread::spawn(move || run(&p));
        std::thread::sleep(Duration::from_millis(200));
        let client = Client::connect_to(&path, false).unwrap();
        let script = "printf '\\033]133;A\\007$ '; read x; printf '\\033]133;C;cmdline=train\\007'; sleep 1; printf '\\033]133;D;0\\007'; sleep 30";
        let r = client.spawn(40, 10, 100, "/bin/sh", &["-c".into(), script.into()], false, &[], None).unwrap();
        let id = r.id;
        read_until(&r.pty, "$ ");
        // Asked at the prompt: for the next command. Then the app goes.
        client.set_awake(id, true).unwrap();
        r.pty.write(b"go\n").unwrap();
        drop(r);
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !power::holds(id) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(power::holds(id), "held while train runs");
        std::thread::sleep(Duration::from_millis(1600));
        assert!(!power::holds(id), "released when it finished");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shell_outlives_its_app_and_comes_back_with_its_screen() {
        let dir = std::env::temp_dir().join(format!("tachyon-host-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sock");
        let p = path.clone();
        std::thread::spawn(move || run(&p));
        let deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            match Client::connect_to(&path, false) {
                Ok(c) => break c,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("no host: {e}"),
            }
        };
        let args = vec!["-c".to_string(), "printf 'first\\nsecond'; read line; printf \"got %s\" \"$line\"".to_string()];
        let s = first.spawn(30, 5, 100, "/bin/sh", &args, false, &[], None).unwrap();
        let mut header = [0u8; 8];
        let mut got = 0;
        while got < 8 {
            got += s.pty.read(&mut header[got..]).unwrap();
        }
        assert_eq!(u64::from_le_bytes(header), 0, "a new shell has nothing to restore");
        assert!(read_until(&s.pty, "second").contains("first"));
        let id = s.id;
        // The app goes away; the shell doesn't.
        drop(s);
        drop(first);
        std::thread::sleep(Duration::from_millis(100));

        let second = Client::connect_to(&path, false).unwrap();
        assert!(second.list().unwrap().iter().any(|l| l.id == id && !l.attached));
        second.set_layout("w 0").unwrap();
        assert_eq!(second.layout().unwrap(), "w 0");
        let s = second.attach(id).unwrap();
        assert_eq!((s.cols, s.rows), (30, 5));
        let restored = read_until(&s.pty, "second");
        assert!(restored.contains("first") && restored.contains("second"), "{restored:?}");
        // Keys still reach it, and its output still comes here.
        s.pty.write(b"hi\n").unwrap();
        assert!(read_until(&s.pty, "got hi").contains("got hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
