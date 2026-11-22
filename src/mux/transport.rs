//! Multiplexer byte-stream transports:
//! - Local Unix domain socket (macOS & Linux), auto-starting `tachyon server`
//!   when requested.
//! - Local Windows overlapped full-duplex named pipe (`\\.\pipe\tachyon-server-*`),
//!   auto-starting `tachyon server` detached when requested.
//! - Remote SSH stdio bridge (`ssh -T ... tachyon bridge`), supporting macOS,
//!   Linux, and Windows OpenSSH servers over one persistent SSH connection per host.
//! - Direct TCP (`tcp://host[:port]`, e.g. over Tailscale) with `TCP_NODELAY`
//!   and bearer-token authentication in `hello`.
//! - `run_bridge()`: remote-side stdio <-> local server splicer (Unix & Windows).

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::Target;

/// A connected full-duplex transport split into independent read/write halves
/// plus a lifetime guard (e.g. the SSH child process and captured stderr).
pub struct Transport {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Optional SSH stderr log captured in the background for clear error messages.
    pub diag: Arc<Mutex<String>>,
    pub _guard: Option<ChildGuard>,
}

/// Cleans up a spawned SSH bridge child process when the transport closes.
pub struct ChildGuard {
    child: Mutex<Option<Child>>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Mutex::new(Some(child)) }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Connect to `target`. If `auto_start` is true and `target` is local, spawns
/// `tachyon server run` automatically when no server is listening.
pub fn connect(target: &Target, auto_start: bool) -> io::Result<Transport> {
    match target {
        Target::Local { socket } => connect_local(socket, auto_start),
        Target::Ssh { destination, port, remote_bin, .. } => {
            connect_ssh_with_mode(destination, *port, remote_bin.as_deref(), false)
        }
        Target::Tcp { addr, .. } => connect_tcp(addr),
    }
}

/// Connect directly over TCP (`tcp://host:port`, e.g. over Tailscale).
pub fn connect_tcp(addr: &str) -> io::Result<Transport> {
    let mut addrs = addr.to_socket_addrs().map_err(|e| {
        io::Error::new(e.kind(), format!("failed to resolve `{addr}`: {e}"))
    })?;
    let mut last_err = None;
    let mut connected = None;
    for sock_addr in &mut addrs {
        match TcpStream::connect_timeout(&sock_addr, Duration::from_secs(10)) {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let stream = connected.ok_or_else(|| {
        last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, format!("no addresses for `{addr}`"))
        })
    })?;
    let _ = stream.set_nodelay(true);
    let writer = stream.try_clone()?;
    Ok(Transport {
        reader: Box::new(stream),
        writer: Box::new(writer),
        diag: Arc::new(Mutex::new(String::new())),
        _guard: None,
    })
}

#[cfg(unix)]
pub fn connect_local(socket: &Path, auto_start: bool) -> io::Result<Transport> {
    connect_local_with_args(socket, auto_start, &[])
}

#[cfg(unix)]
pub fn connect_local_with_args(
    socket: &Path,
    auto_start: bool,
    extra_args: &[String],
) -> io::Result<Transport> {
    use std::os::unix::net::UnixStream;

    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) if !auto_start => return Err(e),
        Err(_) => {
            spawn_local_daemon_with_args(socket, extra_args)?;
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                match UnixStream::connect(socket) {
                    Ok(s) => break s,
                    Err(e) if Instant::now() >= deadline => {
                        return Err(io::Error::new(
                            e.kind(),
                            format!("timed out waiting for tachyon server at {}: {e}", socket.display()),
                        ));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    };
    tune_unix_socket(&stream);
    let writer = stream.try_clone()?;
    Ok(Transport {
        reader: Box::new(stream),
        writer: Box::new(writer),
        diag: Arc::new(Mutex::new(String::new())),
        _guard: None,
    })
}

#[cfg(windows)]
pub fn connect_local(socket: &Path, auto_start: bool) -> io::Result<Transport> {
    connect_local_with_args(socket, auto_start, &[])
}

#[cfg(windows)]
pub fn connect_local_with_args(
    socket: &Path,
    auto_start: bool,
    extra_args: &[String],
) -> io::Result<Transport> {
    let pipe = match win_pipe::connect_named_pipe(socket) {
        Ok(p) => p,
        Err(e) if !auto_start => return Err(e),
        Err(_) => {
            spawn_local_daemon_with_args(socket, extra_args)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match win_pipe::connect_named_pipe(socket) {
                    Ok(p) => break p,
                    Err(e) if Instant::now() >= deadline => {
                        return Err(io::Error::new(
                            e.kind(),
                            format!(
                                "timed out waiting for tachyon server pipe {}: {e}",
                                socket.display()
                            ),
                        ));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(30)),
                }
            }
        }
    };
    let (reader, writer) = win_pipe::split_overlapped_pipe(pipe)?;
    Ok(Transport {
        reader: Box::new(reader),
        writer: Box::new(writer),
        diag: Arc::new(Mutex::new(String::new())),
        _guard: None,
    })
}

#[cfg(not(any(unix, windows)))]
pub fn connect_local(_socket: &Path, _auto_start: bool) -> io::Result<Transport> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "local server transport is unsupported on this OS",
    ))
}

#[cfg(not(any(unix, windows)))]
pub fn connect_local_with_args(
    _socket: &Path,
    _auto_start: bool,
    _extra_args: &[String],
) -> io::Result<Transport> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "local server transport is unsupported on this OS",
    ))
}

#[cfg(unix)]
pub fn tune_unix_socket(sock: &std::os::unix::net::UnixStream) {
    use std::os::fd::AsRawFd;
    let size: libc::c_int = 1 << 20;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// Start `tachyon server run --socket <path>` detached in its own session so
/// it outlives the invoking CLI or GUI process.
pub fn spawn_local_daemon(socket: &Path) -> io::Result<()> {
    spawn_local_daemon_with_args(socket, &[])
}

#[cfg(unix)]
pub fn spawn_local_daemon_with_args(socket: &Path, extra_args: &[String]) -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["server", "run", "--socket"])
        .arg(socket)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    std::thread::Builder::new()
        .name("tachyon-daemon-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

#[cfg(windows)]
pub fn spawn_local_daemon_with_args(socket: &Path, extra_args: &[String]) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    // Prefer launching the installed Scheduled Task when using the default pipe
    // (without custom startup flags) so the server outlives Windows OpenSSH's
    // per-session Job Object.
    if extra_args.is_empty() && socket == crate::mux::default_socket_path() {
        let ok = Command::new("schtasks.exe")
            .args(["/Run", "/TN", "Tachyon Server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Ok(());
        }
    }

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["server", "run", "--socket"])
        .arg(socket)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(
            DETACHED_PROCESS
                | CREATE_NEW_PROCESS_GROUP
                | CREATE_NO_WINDOW
                | CREATE_BREAKAWAY_FROM_JOB,
        );
    if cmd.spawn().is_err() {
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        let _child = cmd.spawn()?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn spawn_local_daemon_with_args(_socket: &Path, _extra_args: &[String]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "detached daemon spawn is unsupported on this OS",
    ))
}

/// Build the remote shell command executed over `ssh -T <host>`.
pub fn build_ssh_remote_command(remote_bin: Option<&str>, windows_shell: bool) -> String {
    match remote_bin.map(str::trim).filter(|s| !s.is_empty()) {
        Some(bin) => {
            if windows_shell || looks_like_windows_path(bin) {
                format!("{} bridge", windows_quote(bin))
            } else {
                format!("exec {} bridge", shell_quote(bin))
            }
        }
        None if windows_shell => "tachyon bridge".to_string(),
        None => concat!(
            "PATH=\"$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\"; ",
            "if command -v tachyon >/dev/null 2>&1; then exec tachyon bridge; ",
            "elif [ -x /Applications/Tachyon.app/Contents/MacOS/tachyon ]; then ",
            "exec /Applications/Tachyon.app/Contents/MacOS/tachyon bridge; ",
            "else echo 'tachyon not found on remote host (checked PATH and /Applications/Tachyon.app)' >&2; exit 127; fi"
        )
        .to_string(),
    }
}

fn looks_like_windows_path(bin: &str) -> bool {
    let lower = bin.to_ascii_lowercase();
    bin.contains('\\')
        || lower.ends_with(".exe")
        || (bin.len() >= 2 && bin.as_bytes()[1] == b':' && bin.as_bytes()[0].is_ascii_alphabetic())
}

fn windows_quote(s: &str) -> String {
    if s.contains([' ', '\t']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Detect stderr produced when a Windows OpenSSH server (`cmd.exe` or
/// `powershell.exe`) rejects our default POSIX `PATH=...; if ...` command.
pub fn looks_like_windows_ssh_shell_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("is not recognized as an internal or external command")
        || lower.contains("the term 'path=")
        || lower.contains("the syntax of the command is incorrect")
        || lower.contains("unexpected token")
        || lower.contains("was unexpected at this time")
}

/// Connect to a remote Tachyon server via one persistent `ssh` stdio bridge.
pub fn connect_ssh(destination: &str, port: Option<u16>, remote_bin: Option<&str>) -> io::Result<Transport> {
    connect_ssh_with_mode(destination, port, remote_bin, false)
}

/// Connect over SSH with explicit POSIX vs Windows OpenSSH command mode.
pub fn connect_ssh_with_mode(
    destination: &str,
    port: Option<u16>,
    remote_bin: Option<&str>,
    windows_shell: bool,
) -> io::Result<Transport> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ConnectTimeout=10",
    ]);
    if let Some(p) = port {
        cmd.args(["-p", &p.to_string()]);
    }
    cmd.arg(destination);

    let remote_cmd = build_ssh_remote_command(remote_bin, windows_shell);
    cmd.arg(remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        io::Error::new(e.kind(), format!("failed to start ssh to {destination}: {e}"))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("ssh child has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("ssh child has no stdout"))?;
    let stderr = child.stderr.take();

    let diag = Arc::new(Mutex::new(String::new()));
    if let Some(mut err) = stderr {
        let diag_clone = Arc::clone(&diag);
        let _ = std::thread::Builder::new()
            .name("tachyon-ssh-stderr".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut d = diag_clone.lock();
                    if d.len() < 8192 {
                        d.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            });
    }

    Ok(Transport {
        reader: Box::new(stdout),
        writer: Box::new(stdin),
        diag,
        _guard: Some(ChildGuard::new(child)),
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Remote-side bridge (`tachyon bridge`): connects to the local server socket
/// (auto-starting the daemon if needed) and splices stdin/stdout directly to it.
#[cfg(unix)]
pub fn run_bridge(socket: &Path) -> io::Result<()> {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let sock = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(_) => {
            spawn_local_daemon(socket)?;
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                match UnixStream::connect(socket) {
                    Ok(s) => break s,
                    Err(e) if Instant::now() >= deadline => return Err(e),
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    };
    tune_unix_socket(&sock);
    let mut sock_in = sock.try_clone()?;
    let mut sock_out = sock;

    // Copy stdin -> server socket on a dedicated thread.
    let t_in = std::thread::Builder::new()
        .name("tachyon-bridge-in".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock_in.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = sock_in.shutdown(Shutdown::Write);
        })?;

    // Copy server socket -> stdout on the main thread.
    let mut stdout = io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match sock_out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        }
    }
    let _ = sock_out.shutdown(Shutdown::Both);
    drop(t_in);
    Ok(())
}

/// Remote-side bridge on Windows (`tachyon.exe bridge`): connects to the local
/// overlapped named pipe (auto-starting `tachyon server run` if needed) and
/// splices binary stdin/stdout directly to it.
#[cfg(windows)]
pub fn run_bridge(socket: &Path) -> io::Result<()> {
    let Transport {
        mut reader,
        mut writer,
        ..
    } = connect_local(socket, true)?;

    let t_in = std::thread::Builder::new()
        .name("tachyon-bridge-in".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        })?;

    let mut stdout = io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        }
    }
    drop(t_in);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn run_bridge(_socket: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bridge transport is unsupported on this OS",
    ))
}

/// Full-duplex overlapped Windows Named Pipe wrappers so independent reader
/// and writer threads never serialize/deadlock inside the Win32 kernel.
#[cfg(windows)]
pub mod win_pipe {
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::sync::Arc;

    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
        ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent};

    pub struct SharedPipeHandle(pub HANDLE);
    unsafe impl Send for SharedPipeHandle {}
    unsafe impl Sync for SharedPipeHandle {}

    impl Drop for SharedPipeHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CancelIoEx(self.0, None);
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub struct WinPipeReader {
        pipe: Arc<SharedPipeHandle>,
        event: HANDLE,
    }
    unsafe impl Send for WinPipeReader {}

    impl Drop for WinPipeReader {
        fn drop(&mut self) {
            unsafe {
                let _ = CancelIoEx(self.pipe.0, None);
                let _ = CloseHandle(self.event);
            }
        }
    }

    pub struct WinPipeWriter {
        pipe: Arc<SharedPipeHandle>,
        event: HANDLE,
    }
    unsafe impl Send for WinPipeWriter {}

    impl Drop for WinPipeWriter {
        fn drop(&mut self) {
            unsafe {
                let _ = CancelIoEx(self.pipe.0, None);
                let _ = CloseHandle(self.event);
            }
        }
    }

    /// Split an overlapped named pipe `HANDLE` into full-duplex `Read` + `Write` halves.
    pub fn split_overlapped_pipe(handle: HANDLE) -> io::Result<(WinPipeReader, WinPipeWriter)> {
        let read_ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| io::Error::other(format!("CreateEventW(read): {e}")))?;
        let write_ev = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(ev) => ev,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(read_ev);
                    let _ = CloseHandle(handle);
                }
                return Err(io::Error::other(format!("CreateEventW(write): {e}")));
            }
        };
        let shared = Arc::new(SharedPipeHandle(handle));
        Ok((
            WinPipeReader { pipe: Arc::clone(&shared), event: read_ev },
            WinPipeWriter { pipe: shared, event: write_ev },
        ))
    }

    fn is_eof_error(err: &windows::core::Error) -> bool {
        let code = err.code();
        code == ERROR_BROKEN_PIPE.to_hresult()
            || code == ERROR_PIPE_NOT_CONNECTED.to_hresult()
            || code == ERROR_OPERATION_ABORTED.to_hresult()
    }

    impl Read for WinPipeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            unsafe {
                let _ = ResetEvent(self.event);
                let mut ov = OVERLAPPED { hEvent: self.event, ..Default::default() };
                match ReadFile(self.pipe.0, Some(buf), None, Some(&mut ov)) {
                    Ok(()) => {
                        let mut n = 0u32;
                        match GetOverlappedResult(self.pipe.0, &ov, &mut n, true) {
                            Ok(()) => Ok(n as usize),
                            Err(e) if is_eof_error(&e) => Ok(0),
                            Err(e) => Err(io::Error::other(e.to_string())),
                        }
                    }
                    Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                        let mut n = 0u32;
                        match GetOverlappedResult(self.pipe.0, &ov, &mut n, true) {
                            Ok(()) => Ok(n as usize),
                            Err(e) if is_eof_error(&e) => Ok(0),
                            Err(e) => Err(io::Error::other(e.to_string())),
                        }
                    }
                    Err(e) if is_eof_error(&e) => Ok(0),
                    Err(e) => Err(io::Error::other(e.to_string())),
                }
            }
        }
    }

    impl Write for WinPipeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            unsafe {
                let _ = ResetEvent(self.event);
                let mut ov = OVERLAPPED { hEvent: self.event, ..Default::default() };
                match WriteFile(self.pipe.0, Some(buf), None, Some(&mut ov)) {
                    Ok(()) => {
                        let mut n = 0u32;
                        GetOverlappedResult(self.pipe.0, &ov, &mut n, true)
                            .map_err(|e| io::Error::other(e.to_string()))?;
                        Ok(n as usize)
                    }
                    Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                        let mut n = 0u32;
                        GetOverlappedResult(self.pipe.0, &ov, &mut n, true)
                            .map_err(|e| io::Error::other(e.to_string()))?;
                        Ok(n as usize)
                    }
                    Err(e) => Err(io::Error::other(e.to_string())),
                }
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Connect as a client to an existing Windows named pipe with `FILE_FLAG_OVERLAPPED`.
    pub fn connect_named_pipe(pipe_path: &Path) -> io::Result<HANDLE> {
        let wide = HSTRING::from(pipe_path.as_os_str());
        for _ in 0..8 {
            let res = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            };
            match res {
                Ok(h) => return Ok(h),
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), 1000) };
                    continue;
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("open named pipe {}: {e}", pipe_path.display()),
                    ));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("named pipe busy: {}", pipe_path.display()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_remote_command_handles_posix_and_windows_targets() {
        let posix = build_ssh_remote_command(None, false);
        assert!(posix.contains("exec tachyon bridge"));

        let win_default = build_ssh_remote_command(None, true);
        assert_eq!(win_default, "tachyon bridge");

        let win_bin = build_ssh_remote_command(Some(r"C:\Users\razeen\.local\bin\tachyon.exe"), false);
        assert_eq!(win_bin, r"C:\Users\razeen\.local\bin\tachyon.exe bridge");

        let win_spaced = build_ssh_remote_command(Some(r"C:\Program Files\Tachyon\tachyon.exe"), false);
        assert_eq!(win_spaced, "\"C:\\Program Files\\Tachyon\\tachyon.exe\" bridge");

        assert!(looks_like_windows_ssh_shell_error(
            "'PATH' is not recognized as an internal or external command"
        ));
        assert!(looks_like_windows_ssh_shell_error(
            "The term 'PATH=/bin' is not recognized as the name of a cmdlet"
        ));
        assert!(!looks_like_windows_ssh_shell_error("Permission denied (publickey)."));
    }
}
