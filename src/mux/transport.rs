//! Multiplexer byte-stream transports:
//! - Local Unix domain socket (macOS & Linux), auto-starting `tachyon server`
//!   when requested.
//! - Remote SSH stdio bridge (`ssh -T ... tachyon bridge`), carrying the exact
//!   same Mux v2 framed stream over one persistent SSH connection per host.
//! - `run_bridge()`: remote-side stdio <-> local Unix socket splicer.

use std::io::{self, Read, Write};
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
/// `tachyon server run --daemon` automatically when no server is listening.
pub fn connect(target: &Target, auto_start: bool) -> io::Result<Transport> {
    match target {
        Target::Local { socket } => connect_local(socket, auto_start),
        Target::Ssh { destination, port, remote_bin, .. } => {
            connect_ssh(destination, *port, remote_bin.as_deref())
        }
    }
}

#[cfg(unix)]
pub fn connect_local(socket: &Path, auto_start: bool) -> io::Result<Transport> {
    use std::os::unix::net::UnixStream;

    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) if !auto_start => return Err(e),
        Err(_) => {
            spawn_local_daemon(socket)?;
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

#[cfg(not(unix))]
pub fn connect_local(_socket: &Path, _auto_start: bool) -> io::Result<Transport> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "local named-pipe server transport for Windows is planned for Milestone 4",
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
#[cfg(unix)]
pub fn spawn_local_daemon(socket: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["server", "run", "--socket"])
        .arg(socket)
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

/// Connect to a remote Tachyon server via one persistent `ssh` stdio bridge.
pub fn connect_ssh(destination: &str, port: Option<u16>, remote_bin: Option<&str>) -> io::Result<Transport> {
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

    // Search common user/Homebrew binary locations on the remote host unless an
    // explicit binary path was configured for this host.
    let remote_cmd = match remote_bin {
        Some(bin) if !bin.trim().is_empty() => format!("exec {} bridge", shell_quote(bin.trim())),
        _ => concat!(
            "PATH=\"$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\"; ",
            "if command -v tachyon >/dev/null 2>&1; then exec tachyon bridge; ",
            "elif [ -x /Applications/Tachyon.app/Contents/MacOS/tachyon ]; then ",
            "exec /Applications/Tachyon.app/Contents/MacOS/tachyon bridge; ",
            "else echo 'tachyon not found on remote host (checked PATH and /Applications/Tachyon.app)' >&2; exit 127; fi"
        )
        .to_string(),
    };
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

#[cfg(not(unix))]
pub fn run_bridge(_socket: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows bridge transport is planned for Milestone 4",
    ))
}
