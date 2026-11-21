//! Persistent Tachyon Mux v2 server daemon (`tachyon server`).
//!
//! Listens on a private local Unix domain socket (macOS & Linux; Windows named
//! pipe in Milestone 4), accepts multiple concurrent clients (local GUI, CLI,
//! or remote SSH `tachyon bridge` relays), and keeps sessions, split windows,
//! and PTYs alive across disconnects and app restarts.

pub mod pty;
pub mod state;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::mux::proto::{self, Frame, FrameKind};
use crate::mux::transport::tune_unix_socket;

use state::{ClientOutbox, OutboxPop, ServerState};

/// Run the persistent multiplexer server on `socket` until `server.shutdown`
/// is received or the process is terminated.
#[cfg(unix)]
pub fn run(socket: &Path) -> io::Result<()> {
    use std::os::unix::net::{UnixListener, UnixStream};

    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::umask(0o077);
    }

    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Remove a stale socket file left by an earlier crashed daemon, but refuse
    // to clobber a live server listening on the same path.
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
    crate::log!("mux server: listening on {}", socket.display());

    let state = ServerState::new();
    let socket_path = socket.to_path_buf();

    for conn in listener.incoming() {
        if state.is_shutting_down() {
            break;
        }
        let Ok(stream) = conn else { continue };
        if !peer_is_self(&stream) {
            continue;
        }
        tune_unix_socket(&stream);
        let state = Arc::clone(&state);
        let sock_for_wake = socket_path.clone();
        let _ = std::thread::Builder::new()
            .name("tachyon-mux-conn".into())
            .spawn(move || serve_connection(state, stream, sock_for_wake));
    }

    let _ = std::fs::remove_file(socket);
    crate::log!("mux server: stopped");
    Ok(())
}

#[cfg(not(unix))]
pub fn run(_socket: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows named-pipe server listener is planned for Milestone 4",
    ))
}

#[cfg(unix)]
fn serve_connection(state: Arc<ServerState>, mut reader: std::os::unix::net::UnixStream, socket_path: PathBuf) {
    let Ok(mut writer) = reader.try_clone() else { return };
    let outbox = state.register_client();

    let writer_state = Arc::clone(&state);
    let writer_outbox = Arc::clone(&outbox);
    let writer_thread = std::thread::Builder::new()
        .name(format!("tachyon-mux-wr-{}", outbox.id))
        .spawn(move || writer_loop(&writer_state, &writer_outbox, &mut writer));

    let mut wake_shutdown = false;
    loop {
        let frame = match proto::read_frame(&mut reader) {
            Ok(f) => f,
            Err(_) => break,
        };
        match frame.kind {
            FrameKind::RpcReq => {
                if let Ok(req) = frame.parse_json() {
                    if state.handle_rpc(&outbox, frame.channel, &req) {
                        wake_shutdown = true;
                        break;
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
                if let Ok(ts) = proto::decode_timestamp(&frame.payload) {
                    outbox.push_priority(Frame::pong(frame.channel, ts));
                }
            }
            FrameKind::Pong | FrameKind::RpcRes | FrameKind::Event | FrameKind::PtyOut | FrameKind::PtySnapshot => {}
        }
    }

    state.unregister_client(&outbox);
    if let Ok(handle) = writer_thread {
        let _ = handle.join();
    }
    if wake_shutdown {
        // Poke the accept loop only after the shutdown RpcRes has been flushed.
        let _ = std::os::unix::net::UnixStream::connect(&socket_path);
    }
}

fn writer_loop<W: io::Write>(state: &ServerState, outbox: &ClientOutbox, writer: &mut W) {
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
// Persistent user service installer (launchd on macOS, systemd --user on Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub fn install_service(socket: &Path) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let exe = std::env::current_exe()?;
    let agents_dir = home.join("Library/LaunchAgents");
    let log_dir = home.join("Library/Logs/Tachyon");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::create_dir_all(&log_dir)?;
    let plist_path = agents_dir.join("dev.tachyon.server.plist");
    let log_out = log_dir.join("server.log");
    let log_err = log_dir.join("server.err.log");

    let plist = format!(
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
    </array>
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
    );
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

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "linux")]
pub fn install_service(socket: &Path) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let exe = std::env::current_exe()?;
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join("tachyon-server.service");
    let unit = format!(
        "[Unit]\nDescription=Tachyon Persistent Multiplexer Server\nAfter=network.target\n\n\
         [Service]\nExecStart={} server run --socket {}\nRestart=on-failure\nRestartSec=1\n\n\
         [Install]\nWantedBy=default.target\n",
        exe.display(),
        socket.display(),
    );
    std::fs::write(&unit_path, unit)?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let status = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "tachyon-server.service"])
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "wrote {}, but `systemctl --user enable --now` failed",
            unit_path.display()
        )));
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
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn install_service(_socket: &Path) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "service installation on this OS is planned for Milestone 4",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn uninstall_service() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "service removal on this OS is planned for Milestone 4",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
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
