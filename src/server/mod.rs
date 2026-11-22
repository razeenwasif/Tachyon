//! Persistent Tachyon Mux v2 server daemon.
//!
//! Runs identically on macOS (MacBook & Mac mini), Linux, and Windows
//! (`--no-default-features`), accepts multiplexed client connections over:
//! - local Unix domain sockets (macOS / Linux, peer-UID verified),
//! - local overlapped Windows named pipes (`\\.\pipe\tachyon-server-*`, local-only),
//! - optional direct TCP (`--listen <addr>`, e.g. Tailscale IPv4:7501) protected
//!   by a mandatory 256-bit bearer token verified in `hello`,
//! and keeps sessions, split windows, and PTYs alive across disconnects and
//! app restarts.

pub mod alerts;
pub mod persist;
pub mod pty;
pub mod receipts;
pub mod state;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::mux::proto::{self, Frame, FrameKind};
use crate::mux::{default_token_path, load_or_create_token, resolve_listen_addr};

use state::{ClientOutbox, OutboxPop, RpcOutcome, ServerState};

/// Startup options for `tachyon server run`.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Optional direct TCP bind specification (`<ip>:<port>`, `<ip>`,
    /// `tailscale`, or `tailscale:<port>`).
    pub listen: Option<String>,
    /// Explicit bearer token for TCP auth (takes precedence over `token_file`).
    pub auth_token: Option<String>,
    /// File storing the bearer token when `--listen` is enabled.
    pub token_file: Option<PathBuf>,
    /// Override persistent snapshot path (`none` / `off` disables persistence).
    pub state_file: Option<PathBuf>,
    /// Do not auto-restore sessions from `state.json` on startup.
    pub no_restore: bool,
    /// Do not auto-resume coding agents during startup restoration.
    pub no_resume: bool,
    /// Optional webhook URL override for `agent.alert` notifications.
    pub alert_webhook: Option<String>,
    /// Optional shell command hook override for `agent.alert` notifications.
    pub alert_command: Option<String>,
    /// Optional comma-separated states triggering `agent.alert` notifications.
    pub alert_states: Option<String>,
}

/// Information reported once listeners are bound (used by tests and CLI logging).
#[derive(Clone, Debug)]
pub struct BoundInfo {
    pub tcp_addr: Option<SocketAddr>,
    pub token: Option<String>,
    pub token_path: Option<PathBuf>,
    pub token_created: bool,
}

/// Options forwarded into persistent user service definitions
/// (`launchd` on macOS, `systemd --user` on Linux, Scheduled Task on Windows).
#[derive(Clone, Debug, Default)]
pub struct ServiceOptions {
    pub listen: Option<String>,
    pub token_file: Option<PathBuf>,
}

/// Run the persistent multiplexer server on `socket` until `server.shutdown`
/// is received or the process is terminated.
pub fn run(socket: &Path) -> io::Result<()> {
    run_with_options(socket, RunOptions::default())
}

/// Run the persistent multiplexer server on `socket` plus an optional direct
/// TCP listener (`opts.listen`).
pub fn run_with_options(socket: &Path, opts: RunOptions) -> io::Result<()> {
    run_with_ready(socket, opts, |info| {
        if let Some(addr) = info.tcp_addr {
            if info.token_created {
                if let Some(path) = &info.token_path {
                    eprintln!("tachyon server: generated bearer token -> {}", path.display());
                }
            }
            crate::log!("mux server: TCP listening on {addr}");
        }
    })
}

/// Internal entry point that invokes `on_ready` right after local & TCP
/// listeners are bound (enabling deterministic ephemeral-port tests).
pub(crate) fn run_with_ready(
    socket: &Path,
    opts: RunOptions,
    on_ready: impl FnOnce(&BoundInfo),
) -> io::Result<()> {
    // Prepare optional TCP listener + mandatory bearer token before binding local socket.
    let mut tcp_listener: Option<(TcpListener, SocketAddr)> = None;
    let mut resolved_token: Option<String> = None;
    let mut token_path: Option<PathBuf> = None;
    let mut token_created = false;

    if let Some(spec) = opts.listen.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let bind_addr = resolve_listen_addr(spec)?;
        let (tok, tpath, created) = if let Some(explicit) =
            opts.auth_token.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            (explicit.to_string(), None, false)
        } else {
            let path = opts.token_file.clone().unwrap_or_else(default_token_path);
            let (t, c) = load_or_create_token(&path)?;
            (t, Some(path), c)
        };
        let listener = TcpListener::bind(&bind_addr).map_err(|e| {
            io::Error::new(e.kind(), format!("failed to bind TCP listener on `{bind_addr}`: {e}"))
        })?;
        let local_addr = listener.local_addr()?;
        tcp_listener = Some((listener, local_addr));
        resolved_token = Some(tok);
        token_path = tpath;
        token_created = created;
    }

    let cfg = crate::config::Config::load_default();
    let state_path = if opts.state_file.is_some() || cfg.server.persist_sessions {
        persist::resolve_state_path(socket, opts.state_file.as_deref())
    } else {
        None
    };
    let auto_resume = cfg.server.auto_resume_agents && !opts.no_resume;
    let mut alert_cfg = alerts::AlertConfig::from_server_config(&cfg.server);
    if let Some(ref raw) = opts.alert_webhook {
        alert_cfg.webhook = alerts::normalize_webhook_url(raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    if let Some(ref raw) = opts.alert_command {
        let t = raw.trim();
        alert_cfg.command = (!t.is_empty()).then(|| t.to_string());
    }
    if let Some(ref raw) = opts.alert_states {
        alert_cfg.states = alerts::parse_alert_states(raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    let listen_addr_str = tcp_listener.as_ref().map(|(_, a)| a.to_string());
    let persisted_token_path = token_path.clone().or_else(|| opts.token_file.clone());
    let state = ServerState::with_full_options(
        resolved_token.clone(),
        persisted_token_path,
        listen_addr_str,
        state::PersistOptions {
            path: state_path.clone(),
            auto_resume,
        },
        alert_cfg,
    );
    if state_path.is_some() && !opts.no_restore {
        match state.restore_from_disk(None, None) {
            Ok(rep) if rep.panes > 0 => {
                crate::log!(
                    "mux server: restored {} session(s), {} window(s), {} pane(s) ({} agent(s) resumed)",
                    rep.sessions,
                    rep.windows,
                    rep.panes,
                    rep.resumed_agents
                );
            }
            Ok(_) => {}
            Err(e) => {
                crate::log!("mux server: skipped state restore: {e}");
            }
        }
    }
    let tcp_addr = tcp_listener.as_ref().map(|(_, a)| *a);
    let info = BoundInfo {
        tcp_addr,
        token: resolved_token,
        token_path,
        token_created,
    };

    #[cfg(unix)]
    {
        run_unix(socket, state, tcp_listener, &info, on_ready)
    }
    #[cfg(windows)]
    {
        run_windows(socket, state, tcp_listener, &info, on_ready)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (socket, state, tcp_listener, info, on_ready);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "multiplexer server is unsupported on this OS",
        ))
    }
}

fn spawn_tcp_accept_thread(
    state: Arc<ServerState>,
    listener: TcpListener,
    wake_all: Arc<dyn Fn() + Send + Sync>,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("tachyon-mux-tcp-accept".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if state.is_shutting_down() {
                    break;
                }
                let Ok(stream) = conn else { continue };
                let _ = stream.set_nodelay(true);
                let Ok(writer) = stream.try_clone() else { continue };
                let st = Arc::clone(&state);
                let wake = Arc::clone(&wake_all);
                let _ = std::thread::Builder::new()
                    .name("tachyon-mux-tcp-conn".into())
                    .spawn(move || serve_connection(st, stream, writer, false, wake));
            }
        })
}

#[cfg(unix)]
fn run_unix(
    socket: &Path,
    state: Arc<ServerState>,
    tcp_listener: Option<(TcpListener, SocketAddr)>,
    info: &BoundInfo,
    on_ready: impl FnOnce(&BoundInfo),
) -> io::Result<()> {
    use std::os::unix::net::{UnixListener, UnixStream};

    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::umask(0o077);
    }

    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Remove a stale socket file left by an earlier crashed daemon, but refuse
    // to clobber an active server listening on the same path.
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("tachyon server is already running at {}", socket.display()),
            ));
        }
        let _ = std::fs::remove_file(socket);
    }

    let listener = UnixListener::bind(socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    crate::log!("mux server: listening on {}", socket.display());

    let sock_wake = socket.to_path_buf();
    let tcp_wake = info.tcp_addr;
    let wake_all: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = UnixStream::connect(&sock_wake);
        if let Some(addr) = tcp_wake {
            let _ = TcpStream::connect(addr);
        }
    });

    let tcp_thread = match tcp_listener {
        Some((tcp, _)) => Some(spawn_tcp_accept_thread(
            Arc::clone(&state),
            tcp,
            Arc::clone(&wake_all),
        )?),
        None => None,
    };

    let _sig_guard = install_unix_signal_guard(&state, &wake_all);

    on_ready(info);

    for conn in listener.incoming() {
        if state.is_shutting_down() {
            break;
        }
        let Ok(stream) = conn else { continue };
        if !peer_is_self(&stream) {
            continue;
        }
        crate::mux::transport::tune_unix_socket(&stream);
        let Ok(writer) = stream.try_clone() else { continue };
        let st = Arc::clone(&state);
        let wake = Arc::clone(&wake_all);
        let _ = std::thread::Builder::new()
            .name("tachyon-mux-conn".into())
            .spawn(move || serve_connection(st, stream, writer, true, wake));
    }

    drop(_sig_guard);
    if !state.is_shutting_down() {
        let _ = state.flush_persist_now();
    }
    state.stop_persist_worker();

    if let Some(addr) = tcp_wake {
        let _ = TcpStream::connect(addr);
    }
    if let Some(t) = tcp_thread {
        let _ = t.join();
    }
    let _ = std::fs::remove_file(socket);
    crate::log!("mux server: stopped");
    Ok(())
}

#[cfg(unix)]
static UNIX_SIG_WAKE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn unix_term_signal_handler(_sig: libc::c_int) {
    let fd = UNIX_SIG_WAKE_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let b = 1u8;
        unsafe {
            let _ = libc::write(fd, (&b as *const u8).cast::<libc::c_void>(), 1);
        }
    }
}

#[cfg(unix)]
struct UnixSignalGuard {
    claimed: bool,
    read_fd: libc::c_int,
    write_fd: libc::c_int,
    old_term: libc::sigaction,
    old_int: libc::sigaction,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl Drop for UnixSignalGuard {
    fn drop(&mut self) {
        if !self.claimed {
            return;
        }
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.old_term, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.old_int, std::ptr::null_mut());
        }
        UNIX_SIG_WAKE_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::close(self.write_fd);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            libc::close(self.read_fd);
        }
    }
}

#[cfg(unix)]
fn install_unix_signal_guard(
    state: &Arc<ServerState>,
    wake_all: &Arc<dyn Fn() + Send + Sync>,
) -> UnixSignalGuard {
    use std::sync::atomic::Ordering;

    let zero_sa: libc::sigaction = unsafe { std::mem::zeroed() };
    let mut fds = [-1 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return UnixSignalGuard {
            claimed: false,
            read_fd: -1,
            write_fd: -1,
            old_term: zero_sa,
            old_int: zero_sa,
            thread: None,
        };
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    unsafe {
        libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(write_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let flags = libc::fcntl(write_fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    if UNIX_SIG_WAKE_FD
        .compare_exchange(-1, write_fd, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return UnixSignalGuard {
            claimed: false,
            read_fd: -1,
            write_fd: -1,
            old_term: zero_sa,
            old_int: zero_sa,
            thread: None,
        };
    }

    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = unix_term_signal_handler as *const () as libc::sighandler_t;
    sa.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
    }
    let mut old_term = zero_sa;
    let mut old_int = zero_sa;
    unsafe {
        libc::sigaction(libc::SIGTERM, &sa, &mut old_term);
        libc::sigaction(libc::SIGINT, &sa, &mut old_int);
    }

    let st = Arc::clone(state);
    let wake = Arc::clone(wake_all);
    let thread = std::thread::Builder::new()
        .name("tachyon-srv-sig".into())
        .spawn(move || {
            let mut b = 0u8;
            loop {
                let n = unsafe {
                    libc::read(read_fd, (&mut b as *mut u8).cast::<libc::c_void>(), 1)
                };
                if n > 0 {
                    if !st.is_shutting_down() {
                        let _ = st.flush_persist_now();
                    }
                    st.request_shutdown();
                    wake();
                    break;
                }
                if n == 0 {
                    break;
                }
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        })
        .ok();

    UnixSignalGuard {
        claimed: true,
        read_fd,
        write_fd,
        old_term,
        old_int,
        thread,
    }
}

#[cfg(windows)]
fn run_windows(
    socket: &Path,
    state: Arc<ServerState>,
    tcp_listener: Option<(TcpListener, SocketAddr)>,
    info: &BoundInfo,
    on_ready: impl FnOnce(&BoundInfo),
) -> io::Result<()> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
        INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent};

    let pipe_path = crate::mux::normalize_pipe_path(&socket.to_string_lossy());
    if crate::mux::transport::win_pipe::connect_named_pipe(&pipe_path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("tachyon server is already running at {}", pipe_path.display()),
        ));
    }

    // Owner-only DACL (creator owner + LocalSystem) combined with
    // PIPE_REJECT_REMOTE_CLIENTS keeps the local named pipe private to this user.
    let sddl = HSTRING::from("D:P(A;;GA;;;OW)(A;;GA;;;SY)");
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let has_sd = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .is_ok()
    };

    let pipe_wide = HSTRING::from(pipe_path.as_os_str());
    let create_instance = || -> io::Result<HANDLE> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let sa_ptr = if has_sd { Some(&sa as *const SECURITY_ATTRIBUTES) } else { None };
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(pipe_wide.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                256 * 1024,
                256 * 1024,
                0,
                sa_ptr,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(h)
        }
    };

    // Create the first pipe instance before invoking `on_ready` so clients can
    // connect immediately once ready.
    let mut next_pipe = create_instance()?;
    let connect_ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|e| io::Error::other(format!("CreateEventW(connect): {e}")))?;

    let pipe_wake = pipe_path.clone();
    let tcp_wake = info.tcp_addr;
    let wake_all: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        if let Ok(h) = crate::mux::transport::win_pipe::connect_named_pipe(&pipe_wake) {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        if let Some(addr) = tcp_wake {
            let _ = TcpStream::connect(addr);
        }
    });

    let tcp_thread = match tcp_listener {
        Some((tcp, _)) => Some(spawn_tcp_accept_thread(
            Arc::clone(&state),
            tcp,
            Arc::clone(&wake_all),
        )?),
        None => None,
    };

    let _ctrl_guard = install_win_ctrl_guard(&state, &wake_all);

    crate::log!("mux server: listening on {}", pipe_path.display());
    on_ready(info);

    loop {
        if state.is_shutting_down() {
            unsafe {
                let _ = CloseHandle(next_pipe);
            }
            break;
        }
        unsafe {
            let _ = ResetEvent(connect_ev);
        }
        let mut ov = OVERLAPPED { hEvent: connect_ev, ..Default::default() };
        let connected = unsafe {
            match ConnectNamedPipe(next_pipe, Some(&mut ov)) {
                Ok(()) => true,
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => true,
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    let mut dummy = 0u32;
                    GetOverlappedResult(next_pipe, &ov, &mut dummy, true).is_ok()
                }
                Err(_) => false,
            }
        };

        if state.is_shutting_down() {
            unsafe {
                let _ = CloseHandle(next_pipe);
            }
            break;
        }

        if connected {
            if let Ok((reader, writer)) =
                crate::mux::transport::win_pipe::split_overlapped_pipe(next_pipe)
            {
                let st = Arc::clone(&state);
                let wake = Arc::clone(&wake_all);
                let _ = std::thread::Builder::new()
                    .name("tachyon-mux-pipe-conn".into())
                    .spawn(move || serve_connection(st, reader, writer, true, wake));
            }
        } else {
            unsafe {
                let _ = CloseHandle(next_pipe);
            }
        }

        match create_instance() {
            Ok(h) => next_pipe = h,
            Err(_) => break,
        }
    }

    drop(_ctrl_guard);
    if !state.is_shutting_down() {
        let _ = state.flush_persist_now();
    }
    state.stop_persist_worker();

    unsafe {
        let _ = CloseHandle(connect_ev);
        if has_sd && !sd.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
    }
    if let Some(addr) = tcp_wake {
        let _ = TcpStream::connect(addr);
    }
    if let Some(t) = tcp_thread {
        let _ = t.join();
    }
    crate::log!("mux server: stopped");
    Ok(())
}

#[cfg(windows)]
static WIN_CTRL_TARGET: std::sync::OnceLock<
    parking_lot::Mutex<Option<(Arc<ServerState>, Arc<dyn Fn() + Send + Sync>)>>,
> = std::sync::OnceLock::new();

#[cfg(windows)]
unsafe extern "system" fn win_ctrl_handler(ctrl_type: u32) -> windows::core::BOOL {
    use windows::core::BOOL;
    use windows::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    if matches!(
        ctrl_type,
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    ) {
        if let Some(slot) = WIN_CTRL_TARGET.get() {
            let pair = slot.lock().clone();
            if let Some((st, wake)) = pair {
                if !st.is_shutting_down() {
                    let _ = st.flush_persist_now();
                }
                st.request_shutdown();
                wake();
                return BOOL(1);
            }
        }
    }
    BOOL(0)
}

#[cfg(windows)]
struct WinCtrlGuard {
    registered: bool,
}

#[cfg(windows)]
impl Drop for WinCtrlGuard {
    fn drop(&mut self) {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        if self.registered {
            unsafe {
                let _ = SetConsoleCtrlHandler(Some(win_ctrl_handler), false);
            }
            if let Some(slot) = WIN_CTRL_TARGET.get() {
                *slot.lock() = None;
            }
        }
    }
}

#[cfg(windows)]
fn install_win_ctrl_guard(
    state: &Arc<ServerState>,
    wake_all: &Arc<dyn Fn() + Send + Sync>,
) -> WinCtrlGuard {
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    let slot = WIN_CTRL_TARGET.get_or_init(|| parking_lot::Mutex::new(None));
    let mut guard = slot.lock();
    if guard.is_some() {
        return WinCtrlGuard { registered: false };
    }
    *guard = Some((Arc::clone(state), Arc::clone(wake_all)));
    drop(guard);
    let registered = unsafe { SetConsoleCtrlHandler(Some(win_ctrl_handler), true) }.is_ok();
    if !registered {
        *slot.lock() = None;
    }
    WinCtrlGuard { registered }
}

fn serve_connection<R, W>(
    state: Arc<ServerState>,
    mut reader: R,
    mut writer: W,
    pre_authenticated: bool,
    wake_shutdown: Arc<dyn Fn() + Send + Sync>,
) where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let outbox = state.register_client_with_auth(pre_authenticated);

    let writer_state = Arc::clone(&state);
    let writer_outbox = Arc::clone(&outbox);
    let writer_thread = std::thread::Builder::new()
        .name(format!("tachyon-mux-wr-{}", outbox.id))
        .spawn(move || writer_loop(&writer_state, &writer_outbox, &mut writer));

    let mut trigger_shutdown = false;
    loop {
        let frame = match proto::read_frame(&mut reader) {
            Ok(f) => f,
            Err(_) => break,
        };
        match frame.kind {
            FrameKind::RpcReq => {
                if let Ok(req) = frame.parse_json() {
                    match state.handle_rpc(&outbox, frame.channel, &req) {
                        RpcOutcome::Continue => {}
                        RpcOutcome::CloseClient => break,
                        RpcOutcome::ShutdownServer => {
                            trigger_shutdown = true;
                            break;
                        }
                    }
                }
            }
            FrameKind::PtyIn => {
                state.handle_pty_in(&outbox, frame.channel, &frame.payload);
            }
            FrameKind::PtyResize => {
                if let Ok((cols, rows, cw, ch)) = proto::decode_resize(&frame.payload) {
                    state.handle_pty_resize(&outbox, frame.channel, cols, rows, cw, ch);
                }
            }
            FrameKind::Ping => {
                if outbox.authenticated.load(std::sync::atomic::Ordering::Acquire) {
                    if let Ok(ts) = proto::decode_timestamp(&frame.payload) {
                        outbox.push_priority(Frame::pong(frame.channel, ts));
                    }
                }
            }
            FrameKind::Pong
            | FrameKind::RpcRes
            | FrameKind::Event
            | FrameKind::PtyOut
            | FrameKind::PtySnapshot => {}
        }
    }

    state.unregister_client(&outbox);
    if let Ok(handle) = writer_thread {
        let _ = handle.join();
    }
    if trigger_shutdown {
        // Poke accept loops only after the shutdown RpcRes has been flushed.
        wake_shutdown();
    }
}

fn writer_loop<W: Write>(state: &ServerState, outbox: &ClientOutbox, writer: &mut W) {
    loop {
        match outbox.pop_next() {
            OutboxPop::Frame(frame) => {
                if proto::write_frame(writer, &frame).is_err() {
                    break;
                }
            }
            OutboxPop::Resync(stream_id) => {
                if let Some(snap) = state.resync_frame(outbox, stream_id) {
                    if proto::write_frame(writer, &snap).is_err() {
                        break;
                    }
                }
            }
            OutboxPop::Closed => break,
        }
    }
}

/// Verify the connecting Unix socket peer belongs to the same UID.
#[cfg(target_os = "macos")]
fn peer_is_self(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    unsafe {
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::getuid()
    }
}

#[cfg(target_os = "linux")]
fn peer_is_self(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        ) == 0
            && cred.uid == libc::getuid()
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn peer_is_self(_stream: &std::os::unix::net::UnixStream) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Persistent user service installers (macOS launchd, Linux systemd --user,
// Windows Scheduled Task) + portable template renderers
// ---------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a macOS `launchd` LaunchAgent plist for `tachyon server run`.
pub fn render_launchd_plist(
    exe: &Path,
    socket: &Path,
    home: &Path,
    opts: &ServiceOptions,
) -> String {
    let log_dir = home.join("Library/Logs/Tachyon");
    let log_out = log_dir.join("server.log");
    let log_err = log_dir.join("server.err.log");
    let mut extra_args = String::new();
    if let Some(listen) = opts.listen.as_deref().filter(|s| !s.trim().is_empty()) {
        extra_args.push_str(&format!(
            "        <string>--listen</string>\n        <string>{}</string>\n",
            xml_escape(listen.trim()),
        ));
    }
    if let Some(tf) = &opts.token_file {
        extra_args.push_str(&format!(
            "        <string>--token-file</string>\n        <string>{}</string>\n",
            xml_escape(&tf.to_string_lossy()),
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.tachyon.server</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>server</string>
        <string>run</string>
        <string>--socket</string>
        <string>{sock}</string>
{extra_args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    </dict>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.to_string_lossy()),
        sock = xml_escape(&socket.to_string_lossy()),
        out = xml_escape(&log_out.to_string_lossy()),
        err = xml_escape(&log_err.to_string_lossy()),
        home = xml_escape(&home.to_string_lossy()),
    )
}

fn systemd_quote(s: &str) -> String {
    if s.contains([' ', '\t', '"', '\\']) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Render a Linux `systemd --user` service unit for `tachyon server run`.
pub fn render_systemd_unit(exe: &Path, socket: &Path, opts: &ServiceOptions) -> String {
    let mut exec = format!(
        "{} server run --socket {}",
        systemd_quote(&exe.to_string_lossy()),
        systemd_quote(&socket.to_string_lossy()),
    );
    if let Some(listen) = opts.listen.as_deref().filter(|s| !s.trim().is_empty()) {
        exec.push_str(&format!(" --listen {}", systemd_quote(listen.trim())));
    }
    if let Some(tf) = &opts.token_file {
        exec.push_str(&format!(
            " --token-file {}",
            systemd_quote(&tf.to_string_lossy()),
        ));
    }
    format!(
        "[Unit]\n\
         Description=Tachyon Persistent Multiplexer Server\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         Environment=\"PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin\"\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=1\n\
         KillMode=process\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Render the Windows Scheduled Task (`/TR`) command line for `tachyon.exe server run`.
pub fn render_windows_task_command(exe: &Path, socket: &Path, opts: &ServiceOptions) -> String {
    let mut cmd = format!(
        "\"{}\" server run --socket \"{}\"",
        exe.to_string_lossy(),
        socket.to_string_lossy(),
    );
    if let Some(listen) = opts.listen.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.push_str(&format!(" --listen \"{}\"", listen.trim()));
    }
    if let Some(tf) = &opts.token_file {
        cmd.push_str(&format!(" --token-file \"{}\"", tf.to_string_lossy()));
    }
    cmd
}

pub fn install_service(socket: &Path) -> io::Result<PathBuf> {
    install_service_with_options(socket, &ServiceOptions::default())
}

#[cfg(target_os = "macos")]
pub fn install_service_with_options(socket: &Path, opts: &ServiceOptions) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let exe = std::env::current_exe()?;
    let agents_dir = home.join("Library/LaunchAgents");
    let log_dir = home.join("Library/Logs/Tachyon");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::create_dir_all(&log_dir)?;
    let plist_path = agents_dir.join("dev.tachyon.server.plist");

    if opts.listen.is_some() {
        let tf = opts.token_file.clone().unwrap_or_else(default_token_path);
        let _ = load_or_create_token(&tf)?;
    }

    let plist = render_launchd_plist(&exe, socket, &home, opts);
    std::fs::write(&plist_path, plist)?;

    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &domain, &plist_path.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let status = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_path.to_string_lossy()])
        .status()?;
    if !status.success() {
        // Fallback for SSH sessions without an active Aqua GUI domain.
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .status();
        let fallback = std::process::Command::new("launchctl")
            .args(["load", "-w", &plist_path.to_string_lossy()])
            .status()?;
        if !fallback.success() {
            return Err(io::Error::other(format!(
                "wrote {}, but launchctl could not load it in this session",
                plist_path.display()
            )));
        }
    }
    Ok(plist_path)
}

#[cfg(target_os = "macos")]
pub fn uninstall_service() -> io::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let plist_path = home.join("Library/LaunchAgents/dev.tachyon.server.plist");
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &domain, &plist_path.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status();
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn install_service_with_options(socket: &Path, opts: &ServiceOptions) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let exe = std::env::current_exe()?;
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join("tachyon-server.service");

    if opts.listen.is_some() {
        let tf = opts.token_file.clone().unwrap_or_else(default_token_path);
        let _ = load_or_create_token(&tf)?;
    }

    let unit = render_systemd_unit(&exe, socket, opts);
    std::fs::write(&unit_path, unit)?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let status = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "tachyon-server.service"])
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "wrote {}, but `systemctl --user enable --now` failed (if over SSH, ensure `loginctl enable-linger $USER`)",
            unit_path.display()
        )));
    }
    // Enable user lingering best-effort so the daemon stays alive after SSH logout.
    if let Ok(user) = std::env::var("USER") {
        let _ = std::process::Command::new("loginctl")
            .args(["enable-linger", &user])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(unit_path)
}

#[cfg(target_os = "linux")]
pub fn uninstall_service() -> io::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "tachyon-server.service"])
        .status();
    let unit_path = home.join(".config/systemd/user/tachyon-server.service");
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(())
}

#[cfg(windows)]
const WIN_TASK_NAME: &str = "Tachyon Server";

#[cfg(windows)]
pub fn install_service_with_options(socket: &Path, opts: &ServiceOptions) -> io::Result<PathBuf> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = std::env::current_exe()?;
    let pipe = crate::mux::normalize_pipe_path(&socket.to_string_lossy());
    if opts.listen.is_some() {
        let tf = opts.token_file.clone().unwrap_or_else(default_token_path);
        let _ = load_or_create_token(&tf)?;
    }

    let task_cmd = render_windows_task_command(&exe, &pipe, opts);
    let status = std::process::Command::new("schtasks.exe")
        .args([
            "/Create",
            "/TN",
            WIN_TASK_NAME,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
            "/TR",
            &task_cmd,
        ])
        .status()?;
    if !status.success() {
        return Err(io::Error::other(
            "`schtasks.exe /Create` failed to register the Tachyon Server logon task",
        ));
    }

    // Launch via Task Scheduler so the daemon runs outside any caller Job Object
    // (critical when invoked over Windows OpenSSH, whose Job Object otherwise
    // kills child processes on SSH disconnect). Fall back to detached spawn if
    // `schtasks /Run` does not bring the pipe up.
    if crate::mux::transport::win_pipe::connect_named_pipe(&pipe).is_err() {
        let _ = std::process::Command::new("schtasks.exe")
            .args(["/Run", "/TN", WIN_TASK_NAME])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut up = false;
        while std::time::Instant::now() < deadline {
            if crate::mux::transport::win_pipe::connect_named_pipe(&pipe).is_ok() {
                up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if !up {
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            let mut cmd = std::process::Command::new(&exe);
            cmd.args(["server", "run", "--socket"]).arg(&pipe);
            if let Some(listen) = opts.listen.as_deref().filter(|s| !s.trim().is_empty()) {
                cmd.args(["--listen", listen.trim()]);
            }
            if let Some(tf) = &opts.token_file {
                cmd.arg("--token-file").arg(tf);
            }
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(
                    DETACHED_PROCESS
                        | CREATE_NEW_PROCESS_GROUP
                        | CREATE_NO_WINDOW
                        | CREATE_BREAKAWAY_FROM_JOB,
                );
            if cmd.spawn().is_err() {
                cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
                let _ = cmd.spawn();
            }
        }
    }

    Ok(PathBuf::from(format!("TaskScheduler:\\{WIN_TASK_NAME}")))
}

#[cfg(windows)]
pub fn uninstall_service() -> io::Result<()> {
    let _ = std::process::Command::new("schtasks.exe")
        .args(["/Delete", "/TN", WIN_TASK_NAME, "/F"])
        .status();
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn install_service_with_options(_socket: &Path, _opts: &ServiceOptions) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "service installation is unsupported on this OS",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn uninstall_service() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "service removal is unsupported on this OS",
    ))
}

#[cfg(test)]
mod template_tests {
    use super::*;

    #[test]
    fn service_templates_render_with_and_without_tcp_listen() {
        let opts = ServiceOptions {
            listen: Some("tailscale".into()),
            token_file: Some(PathBuf::from("/var/lib/tachyon/token")),
        };

        let plist = render_launchd_plist(
            Path::new("/Users/me/.local/bin/tachyon"),
            Path::new("/Users/me/.local/state/tachyon/server.sock"),
            Path::new("/Users/me"),
            &opts,
        );
        assert!(plist.contains("<string>--listen</string>"));
        assert!(plist.contains("<string>tailscale</string>"));
        assert!(plist.contains("<string>--token-file</string>"));
        assert!(plist.contains("<string>/var/lib/tachyon/token</string>"));

        let unit = render_systemd_unit(
            Path::new("/home/me/.local/bin/tachyon"),
            Path::new("/run/user/1000/tachyon/server.sock"),
            &opts,
        );
        assert!(unit.contains("ExecStart=/home/me/.local/bin/tachyon server run --socket /run/user/1000/tachyon/server.sock --listen tailscale --token-file /var/lib/tachyon/token"));
        assert!(unit.contains("KillMode=process"));
        assert!(unit.contains("Environment=\"PATH=%h/.local/bin"));

        let win = render_windows_task_command(
            Path::new(r"C:\Users\me\.local\bin\tachyon.exe"),
            Path::new(r"\\.\pipe\tachyon-server-me"),
            &opts,
        );
        assert!(win.contains(r#""C:\Users\me\.local\bin\tachyon.exe" server run --socket "\\.\pipe\tachyon-server-me""#));
        assert!(win.contains(r#"--listen "tailscale""#));
        assert!(win.contains(r#"--token-file "/var/lib/tachyon/token""#));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crate::mux::client::{MuxClient, StreamChunk};
    use crate::mux::model::SplitDir;
    use crate::mux::Target;

    fn start_test_server(tag: &str) -> (PathBuf, PathBuf, std::thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "tachyon-mux-test-{tag}-{}-{}",
            std::process::id(),
            crate::mux::mono_now_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("server.sock");
        let sock_srv = sock.clone();
        let handle = std::thread::spawn(move || {
            let _ = run(&sock_srv);
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        (dir, sock, handle)
    }

    fn drain_until(stream: &crate::mux::client::AttachedStream, want: &str) -> String {
        let mut buf = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if String::from_utf8_lossy(&buf).contains(want) {
                break;
            }
            match stream.recv_timeout(Duration::from_millis(100)) {
                Ok(StreamChunk::Snapshot(b)) | Ok(StreamChunk::Output(b)) => buf.extend_from_slice(&b),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn full_mux_lifecycle_attach_split_capture_wait_and_restore() {
        let (dir, sock, handle) = start_test_server("lifecycle");
        let target = Target::Local { socket: sock.clone() };
        let c1 = MuxClient::connect(&target, false, "cli", "test-1").unwrap();

        // Ping RTT check.
        let rtt = c1.ping(Duration::from_secs(2)).unwrap();
        assert!(rtt < Duration::from_secs(1), "rtt too high: {rtt:?}");

        // Subscribe to events.
        let events = c1.subscribe_events().unwrap();

        // Create session running an interactive sh script.
        let cmd = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf 'BOOT_OK\\n'; read line; printf 'ECHO:%s\\n' \"$line\"; sleep 10".to_string(),
        ];
        let (sess, win, blk) = c1
            .create_session(Some("work"), None, Some(&cmd), Some("main-pane"), 80, 24)
            .unwrap();
        assert_eq!(sess.name, "work");
        assert_eq!(win.focused_block, blk.id);

        // Verify session.created event arrived.
        let ev = events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(ev["event"], "session.created");

        // Attach and wait for initial output.
        let stream = c1.attach(&blk.id, true, true).unwrap();
        let seen = drain_until(&stream, "BOOT_OK");
        assert!(seen.contains("BOOT_OK"), "got: {seen:?}");

        // Send input over the live stream and verify echo.
        stream.write(b"tachyon-v2\n").unwrap();
        let seen2 = drain_until(&stream, "ECHO:tachyon-v2");
        assert!(seen2.contains("ECHO:tachyon-v2"), "got: {seen2:?}");

        // Capture plain text from server Term.
        let captured = c1.capture(&blk.id, Some(20), false).unwrap();
        assert!(captured.contains("BOOT_OK") && captured.contains("ECHO:tachyon-v2"), "captured: {captured:?}");

        // Split pane with a command that exits with code 42 and wait for it.
        let run_cmd = vec!["/bin/sh".into(), "-c".into(), "echo SPLIT_DONE; exit 42".into()];
        let (win2, blk2) = c1
            .split_block(&blk.id, SplitDir::Right, None, Some(&run_cmd), Some("worker"), 40, 24)
            .unwrap();
        assert_eq!(win2.layout.leaves().len(), 2);
        let code = c1.wait_block(&blk2.id, Some(5000)).unwrap();
        assert_eq!(code, 42);
        let cap2 = c1.capture(&blk2.id, Some(10), false).unwrap();
        assert!(cap2.contains("SPLIT_DONE"), "cap2: {cap2:?}");

        // Disconnect client 1, reconnect client 2, and verify LZ4 screen restore.
        drop(stream);
        drop(c1);

        let c2 = MuxClient::connect(&target, false, "gui", "test-2").unwrap();
        let snap = c2.state().unwrap();
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].name, "work");

        let restored_stream = c2.attach(&blk.id, true, true).unwrap();
        let first = restored_stream.recv_timeout(Duration::from_secs(2)).unwrap();
        let StreamChunk::Snapshot(vt) = first else {
            panic!("expected Snapshot as first chunk on attach");
        };
        let vt_str = String::from_utf8_lossy(&vt);
        assert!(vt_str.contains("BOOT_OK") && vt_str.contains("ECHO:tachyon-v2"), "vt: {vt_str:?}");

        // Clean shutdown.
        drop(restored_stream);
        c2.kill_session(&sess.id).unwrap();
        c2.shutdown_server().unwrap();
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tcp_listener_enforces_bearer_token_and_shares_state_with_local_socket() {
        let dir = std::env::temp_dir().join(format!(
            "tachyon-tcp-test-{}-{}",
            std::process::id(),
            crate::mux::mono_now_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("server.sock");
        let tok_file = dir.join("server.token");

        let (tx, rx) = mpsc::channel();
        let sock_srv = sock.clone();
        let tok_srv = tok_file.clone();
        let handle = std::thread::spawn(move || {
            let _ = run_with_ready(
                &sock_srv,
                RunOptions {
                    listen: Some("127.0.0.1:0".into()),
                    auth_token: None,
                    token_file: Some(tok_srv),
                    ..RunOptions::default()
                },
                |info| {
                    let _ = tx.send(info.clone());
                },
            );
        });

        let bound = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let addr = bound.tcp_addr.expect("tcp addr bound").to_string();
        let token = bound.token.expect("bearer token generated");
        assert!(bound.token_created);
        assert!(tok_file.is_file());

        // 1. TCP without token must be rejected.
        let no_tok = Target::Tcp { alias: "tcp-none".into(), addr: addr.clone(), token: None };
        let err = MuxClient::connect(&no_tok, false, "cli", "unauth").err().unwrap();
        assert!(
            err.to_string().contains("unauthorized"),
            "expected unauthorized error, got: {err}"
        );

        // 2. TCP with wrong token must be rejected.
        let bad_tok = Target::Tcp {
            alias: "tcp-bad".into(),
            addr: addr.clone(),
            token: Some("0000000000000000".into()),
        };
        let err = MuxClient::connect(&bad_tok, false, "cli", "wrong").err().unwrap();
        assert!(
            err.to_string().contains("unauthorized"),
            "expected unauthorized error, got: {err}"
        );

        // 3. TCP with valid bearer token connects, creates a session & captures output.
        let good_tok = Target::Tcp {
            alias: "tcp-ok".into(),
            addr: addr.clone(),
            token: Some(token),
        };
        let tcp_client = MuxClient::connect(&good_tok, false, "gui", "tcp-client").unwrap();
        let rtt = tcp_client.ping(Duration::from_secs(2)).unwrap();
        assert!(rtt < Duration::from_secs(1));

        let cmd = vec!["/bin/sh".into(), "-c".into(), "echo TCP_HELLO; sleep 10".into()];
        let (sess, _win, blk) = tcp_client
            .create_session(Some("tailscale-work"), None, Some(&cmd), None, 80, 24)
            .unwrap();
        let stream = tcp_client.attach(&blk.id, true, true).unwrap();
        let out = drain_until(&stream, "TCP_HELLO");
        assert!(out.contains("TCP_HELLO"), "out: {out:?}");

        // 4. Simultaneous local Unix-socket client sees the exact same session & block!
        let local_client =
            MuxClient::connect(&Target::Local { socket: sock.clone() }, false, "cli", "local").unwrap();
        let snap = local_client.state().unwrap();
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].name, "tailscale-work");
        let cap = local_client.capture(&blk.id, Some(10), false).unwrap();
        assert!(cap.contains("TCP_HELLO"), "cap: {cap:?}");

        drop(stream);
        local_client.kill_session(&sess.id).unwrap();
        local_client.shutdown_server().unwrap();
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_vars_are_injected_into_managed_blocks() {
        let (dir, sock, handle) = start_test_server("env");
        let target = Target::Local { socket: sock.clone() };
        let c = MuxClient::connect(&target, false, "cli", "env-test").unwrap();
        let cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'S=%s W=%s B=%s SRV=%s\\n' \"$TACHYON_SESSION\" \"$TACHYON_WINDOW\" \"$TACHYON_BLOCK\" \"$TACHYON_SERVER\"".into(),
        ];
        let (sess, win, blk) = c.create_session(Some("envcheck"), None, Some(&cmd), None, 100, 10).unwrap();
        assert_eq!(c.wait_block(&blk.id, Some(5000)).unwrap(), 0);
        let text = c.capture(&blk.id, Some(10), false).unwrap();
        let expected = format!("S={} W={} B={} SRV=local", sess.id, win.id, blk.id);
        assert!(text.contains(&expected), "expected `{expected}` in `{text}`");
        c.shutdown_server().unwrap();
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measure keystroke echo round-trip through Mux v2 framed Unix socket.
    /// Run with `cargo test --release mux_echo -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn mux_echo() {
        let (dir, sock, handle) = start_test_server("echo");
        let target = Target::Local { socket: sock.clone() };
        let c = MuxClient::connect(&target, false, "gui", "bench").unwrap();
        let cmd = vec!["/bin/cat".into()];
        let (_s, _w, blk) = c.create_session(Some("echo"), None, Some(&cmd), None, 80, 24).unwrap();
        let stream = c.attach(&blk.id, true, false).unwrap();
        // Warm up.
        for _ in 0..10 {
            stream.write(b"x").unwrap();
            let _ = stream.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let mut times = Vec::with_capacity(500);
        for _ in 0..500 {
            let start = Instant::now();
            stream.write(b"x").unwrap();
            let _ = stream.recv_timeout(Duration::from_secs(2)).unwrap();
            times.push(start.elapsed().as_secs_f64() * 1e6);
        }
        times.sort_by(f64::total_cmp);
        let med = times[times.len() / 2];
        let p99 = times[times.len() * 99 / 100];
        println!("Mux v2 echo over Unix socket: median {med:.0} µs, p99 {p99:.0} µs");
        drop(stream);
        c.kill_block(&blk.id).unwrap();
        c.shutdown_server().unwrap();
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
