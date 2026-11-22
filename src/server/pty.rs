//! Portable pseudo-terminal backend for `tachyon server`.
//!
//! - On Unix (macOS & Linux): POSIX `forkpty` with clean pre-fork environment
//!   preparation, login shell support, and exact child exit status reaping.
//! - On Windows: thin adapter over Tachyon's existing ConPTY (`src/pty/mod.rs`).

use std::path::PathBuf;

#[cfg(unix)]
pub use unix_pty::{default_shell, foreground_process_fd, integration, HostPty, Integration};

#[cfg(windows)]
pub use win_pty::{default_shell, integration, HostPty, Integration};

/// Locate Tachyon's `shell/` integration directory if present on this host.
pub fn find_integration_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // App bundle: Contents/MacOS/tachyon -> Contents/Resources/shell
            candidates.push(dir.join("../Resources/shell"));
            // Dev build: target/{debug,release}/tachyon -> ../../shell
            candidates.push(dir.join("../../shell"));
            // Sibling install: ~/.local/share/tachyon/shell
            candidates.push(dir.join("../share/tachyon/shell"));
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let h = PathBuf::from(home);
        candidates.push(h.join(".config/Tachyon/shell"));
        candidates.push(h.join(".local/share/tachyon/shell"));
    }
    if let Some(local_app) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local_app).join("Tachyon/shell"));
    }
    candidates.push(PathBuf::from("/Applications/Tachyon.app/Contents/Resources/shell"));
    candidates.into_iter().find(|p| p.join("tachyon-integration.zsh").is_file())
}

#[cfg(unix)]
mod unix_pty {
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use parking_lot::Mutex;

    pub struct HostPty {
        master: OwnedFd,
        pid: libc::pid_t,
        reaped: AtomicBool,
        exit_status: Mutex<Option<i32>>,
    }

    impl HostPty {
        pub fn spawn(
            cols: u16,
            rows: u16,
            program: &str,
            args: &[String],
            login: bool,
            extra_env: &[(String, String)],
            cwd: Option<&str>,
        ) -> io::Result<Self> {
            let path = resolve(program).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("`{program}` not found"))
            })?;
            let path_c = CString::new(path.as_bytes())?;

            let base = path.rsplit('/').next().unwrap_or(&path);
            let argv0 = if login { format!("-{base}") } else { base.to_string() };
            let mut argv_c = vec![CString::new(argv0)?];
            for a in args {
                argv_c.push(CString::new(a.as_str())?);
            }
            let mut argv: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
            argv.push(ptr::null());

            let env_c = child_environment(extra_env);
            let mut envp: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
            envp.push(ptr::null());

            let start = cwd
                .filter(|d| !d.is_empty() && Path::new(d).is_dir())
                .map(std::ffi::OsString::from);
            let home = start
                .or_else(|| std::env::var_os("HOME"))
                .and_then(|h| CString::new(h.as_bytes()).ok());

            let mut ws = libc::winsize {
                ws_row: rows.max(1),
                ws_col: cols.max(1),
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let mut master: libc::c_int = -1;

            // SAFETY: all memory used between fork and exec was prepared before fork;
            // the child only invokes async-signal-safe chdir / execve / _exit.
            let pid = unsafe { libc::forkpty(&mut master, ptr::null_mut(), ptr::null_mut(), &mut ws) };
            if pid < 0 {
                return Err(io::Error::last_os_error());
            }
            if pid == 0 {
                unsafe {
                    if let Some(h) = &home {
                        libc::chdir(h.as_ptr());
                    }
                    libc::execve(path_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
                    libc::_exit(127);
                }
            }

            // SAFETY: forkpty returned a fresh valid master fd.
            let master = unsafe { OwnedFd::from_raw_fd(master) };
            unsafe {
                libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(Self {
                master,
                pid,
                reaped: AtomicBool::new(false),
                exit_status: Mutex::new(None),
            })
        }

        pub fn pid(&self) -> u32 {
            self.pid.max(0) as u32
        }

        pub fn size(&self) -> (u16, u16) {
            let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
            unsafe {
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCGWINSZ as _, &mut ws);
            }
            (ws.ws_col.max(1), ws.ws_row.max(1))
        }

        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let fd = self.master.as_raw_fd();
            loop {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // macOS and Linux both signal slave close via EIO.
                    Some(libc::EIO) => return Ok(0),
                    _ => return Err(err),
                }
            }
        }

        pub fn write(&self, mut data: &[u8]) -> io::Result<()> {
            let fd = self.master.as_raw_fd();
            while !data.is_empty() {
                let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
                if n < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(err);
                }
                data = &data[n as usize..];
            }
            Ok(())
        }

        pub fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
            let ws = libc::winsize {
                ws_row: rows.max(1),
                ws_col: cols.max(1),
                ws_xpixel: (cols as u32 * cell_w as u32).min(u16::MAX as u32) as u16,
                ws_ypixel: (rows as u32 * cell_h as u32).min(u16::MAX as u32) as u16,
            };
            unsafe {
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws);
            }
        }

        pub fn hangup(&self) {
            if !self.reaped.load(Ordering::Acquire) {
                unsafe {
                    libc::kill(self.pid, libc::SIGHUP);
                }
            }
        }

        /// Reap the child process and return its POSIX exit code.
        pub fn wait_exit(&self) -> i32 {
            let mut guard = self.exit_status.lock();
            if let Some(code) = *guard {
                return code;
            }
            let mut status: libc::c_int = 0;
            let code = loop {
                let r = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                if r <= 0 {
                    break 0;
                }
                if libc::WIFEXITED(status) {
                    break libc::WEXITSTATUS(status);
                }
                if libc::WIFSIGNALED(status) {
                    break 128 + libc::WTERMSIG(status);
                }
                break 0;
            };
            self.reaped.store(true, Ordering::Release);
            *guard = Some(code);
            code
        }

        /// Inspect the active foreground process occupying this PTY.
        ///
        /// When `is_login_shell` is true, returns `None` while the shell itself
        /// owns the foreground process group (idle prompt). Returns `Some` for
        /// direct `cmd` blocks even before child forks.
        pub fn foreground_process(&self, is_login_shell: bool) -> Option<crate::mux::agents::ForegroundProc> {
            foreground_process_fd(self.master.as_raw_fd(), self.pid, is_login_shell)
        }
    }

    /// Inspect the active foreground process on a raw PTY master fd.
    pub fn foreground_process_fd(
        master_fd: libc::c_int,
        shell_pid: libc::pid_t,
        is_login_shell: bool,
    ) -> Option<crate::mux::agents::ForegroundProc> {
        if master_fd < 0 {
            return None;
        }
        let pgid = unsafe { libc::tcgetpgrp(master_fd) };
        if pgid <= 0 {
            return None;
        }
        if is_login_shell && pgid == shell_pid {
            return None;
        }
        inspect_pgrp(pgid, shell_pid, is_login_shell)
    }

    impl Drop for HostPty {
        fn drop(&mut self) {
            if self.reaped.swap(true, Ordering::AcqRel) {
                return;
            }
            let pid = self.pid;
            let mut status = 0;
            if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == 0 {
                unsafe {
                    libc::kill(pid, libc::SIGHUP);
                }
                let _ = std::thread::Builder::new()
                    .name("tachyon-server-reaper".into())
                    .spawn(move || unsafe {
                        let mut status = 0;
                        libc::waitpid(pid, &mut status, 0);
                    });
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct Integration {
        pub env: Vec<(String, String)>,
        pub args: Vec<String>,
        pub login: bool,
    }

    pub fn integration(program: &str, dir: &Path, login: bool) -> Option<Integration> {
        let base = program.rsplit('/').next().unwrap_or(program);
        let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
        let var = |k: &str, v: String| (k.to_string(), v);
        let mut env = vec![var("TACHYON_SHELL_INTEGRATION", path(""))];
        match base {
            "zsh" => {
                env.push(var("ZDOTDIR", path("zsh")));
                if let Some(orig) = std::env::var_os("ZDOTDIR") {
                    env.push(var("TACHYON_ORIG_ZDOTDIR", orig.to_string_lossy().into_owned()));
                    env.push(var("TACHYON_ZDOTDIR_WAS_SET", "1".into()));
                }
                Some(Integration { env, args: Vec::new(), login })
            }
            "bash" => {
                env.push(var("_TACHYON_BASH_INJECT", "1".into()));
                if login {
                    env.push(var("_TACHYON_BASH_LOGIN", "1".into()));
                }
                let args = vec!["--rcfile".into(), path("tachyon-integration.bash"), "-i".into()];
                Some(Integration { env, args, login: false })
            }
            "fish" => {
                let orig = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty());
                let rest = orig.clone().unwrap_or_else(|| "/usr/local/share:/usr/share".into());
                env.push(var("XDG_DATA_DIRS", format!("{}:{rest}", path("xdg"))));
                if let Some(orig) = orig {
                    env.push(var("TACHYON_ORIG_XDG_DATA_DIRS", orig));
                }
                Some(Integration { env, args: Vec::new(), login })
            }
            _ => None,
        }
    }

    pub fn default_shell() -> String {
        if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
            return s.to_string_lossy().into_owned();
        }
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if !pw.is_null() && !(*pw).pw_shell.is_null() {
                let s = std::ffi::CStr::from_ptr((*pw).pw_shell);
                if !s.to_bytes().is_empty() {
                    return s.to_string_lossy().into_owned();
                }
            }
        }
        if Path::new("/bin/zsh").is_file() {
            "/bin/zsh".to_string()
        } else {
            "/bin/sh".to_string()
        }
    }

    fn resolve(program: &str) -> Option<String> {
        if program.contains('/') {
            return Some(program.to_string());
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn child_environment(extra: &[(String, String)]) -> Vec<CString> {
        const OURS: [&str; 6] = [
            "TERM",
            "COLORTERM",
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
        ];
        // Prepend the tachyon binary directory to PATH so managed panes can
        // invoke `tachyon` directly without manual PATH setup.
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()));
        let orig_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        let augmented_path = match exe_dir {
            Some(ref d) if !d.is_empty() && !orig_path.split(':').any(|p| p == d) => {
                format!("{d}:{orig_path}")
            }
            _ => orig_path,
        };

        let mut env: Vec<CString> = std::env::vars_os()
            .filter(|(k, _)| !OURS.iter().any(|o| OsStr::new(o) == k))
            .filter(|(k, _)| OsStr::new("PATH") != k)
            .filter(|(k, _)| !extra.iter().any(|(e, _)| OsStr::new(e) == k))
            .filter_map(|(k, v)| {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                CString::new(kv).ok()
            })
            .collect();

        let mut set = |k: &str, v: &str| {
            if let Ok(c) = CString::new(format!("{k}={v}")) {
                env.push(c);
            }
        };
        if !extra.iter().any(|(k, _)| k == "PATH") {
            set("PATH", &augmented_path);
        }
        set("TERM", "xterm-256color");
        set("COLORTERM", "truecolor");
        set("TERM_PROGRAM", "Tachyon");
        set("TERM_PROGRAM_VERSION", crate::APP_VERSION);
        set("LC_TERMINAL", "Tachyon");
        set("LC_TERMINAL_VERSION", crate::APP_VERSION);
        if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
            set("LANG", "en_US.UTF-8");
        }
        for (k, v) in extra {
            set(k, v);
        }
        env
    }

    #[cfg(target_os = "macos")]
    fn inspect_pgrp(
        pgid: libc::pid_t,
        shell_pid: libc::pid_t,
        exclude_shell: bool,
    ) -> Option<crate::mux::agents::ForegroundProc> {
        const PROC_PGRP_ONLY: u32 = 2;
        let mut pids = [0 as libc::pid_t; 64];
        let bytes = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                pgid as u32,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(&pids) as libc::c_int,
            )
        };
        let n = if bytes > 0 {
            (bytes as usize / std::mem::size_of::<libc::pid_t>()).min(pids.len())
        } else {
            0
        };
        let mut candidates: Vec<libc::pid_t> = pids[..n]
            .iter()
            .copied()
            .filter(|&p| p > 0 && (!exclude_shell || p != shell_pid))
            .collect();
        if candidates.is_empty() && (!exclude_shell || pgid != shell_pid) {
            candidates.push(pgid);
        }
        // Scan highest PID first so the active leaf child (e.g. `node` under `sh`)
        // is preferred while avoiding zombie/exiting ancestors.
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        for pid in candidates {
            if let Some(proc) = inspect_macos_pid(pid) {
                return Some(proc);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    fn inspect_macos_pid(pid: libc::pid_t) -> Option<crate::mux::agents::ForegroundProc> {
        let name = macos_pid_name(pid)?;
        let cmdline = macos_pid_cmdline(pid).unwrap_or_else(|| name.clone());
        Some(crate::mux::agents::ForegroundProc {
            pid: pid.max(0) as u32,
            name,
            cmdline,
        })
    }

    #[cfg(target_os = "macos")]
    fn macos_pid_name(pid: libc::pid_t) -> Option<String> {
        let mut path_buf = vec![0u8; 4096];
        let len = unsafe {
            libc::proc_pidpath(pid, path_buf.as_mut_ptr().cast(), path_buf.len() as u32)
        };
        if len > 0 {
            let full = String::from_utf8_lossy(&path_buf[..len as usize]);
            if let Some(base) = full.rsplit('/').next().filter(|s| !s.is_empty()) {
                return Some(base.to_string());
            }
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if got == size {
            let comm = unsafe { std::ffi::CStr::from_ptr(info.pbi_comm.as_ptr()) };
            let s = comm.to_string_lossy().trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    fn macos_pid_cmdline(pid: libc::pid_t) -> Option<String> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut buf = vec![0u8; 8192];
        let mut size = buf.len();
        let mut rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            // Large inherited environments can exceed 8 KiB; probe exact kernel size.
            let mut need: usize = 0;
            let probe = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as u32,
                    std::ptr::null_mut(),
                    &mut need,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if probe != 0 || need < 4 {
                return None;
            }
            buf.resize((need + 256).min(1024 * 1024), 0);
            size = buf.len();
            rc = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as u32,
                    buf.as_mut_ptr().cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
        }
        if rc != 0 || size < 4 {
            return None;
        }
        let data = &buf[..size.min(buf.len())];
        let argc = i32::from_ne_bytes([data[0], data[1], data[2], data[3]]).max(0) as usize;
        if argc == 0 {
            return None;
        }
        let mut i = 4usize;
        // Skip exec_path\0 and trailing NUL padding before argv[0].
        while i < data.len() && data[i] != 0 {
            i += 1;
        }
        while i < data.len() && data[i] == 0 {
            i += 1;
        }
        let mut out = String::new();
        for _ in 0..argc {
            if i >= data.len() || out.len() >= 512 {
                break;
            }
            let start = i;
            while i < data.len() && data[i] != 0 {
                i += 1;
            }
            if i > start {
                if !out.is_empty() {
                    out.push(' ');
                }
                let piece = String::from_utf8_lossy(&data[start..i]);
                out.push_str(&piece);
                if out.len() > 512 {
                    out.truncate(512);
                    break;
                }
            }
            i += 1;
        }
        (!out.is_empty()).then_some(out)
    }

    #[cfg(target_os = "linux")]
    fn inspect_pgrp(
        pgid: libc::pid_t,
        shell_pid: libc::pid_t,
        exclude_shell: bool,
    ) -> Option<crate::mux::agents::ForegroundProc> {
        if exclude_shell && pgid == shell_pid {
            return None;
        }
        let pid = pgid;
        let comm_path = format!("/proc/{pid}/comm");
        let name = std::fs::read_to_string(&comm_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        let raw_cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let cap = raw_cmd.len().min(512);
        let cmdline_joined: String = raw_cmd[..cap]
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let cmdline = if cmdline_joined.is_empty() {
            name.clone()
        } else {
            cmdline_joined
        };
        Some(crate::mux::agents::ForegroundProc {
            pid: pid.max(0) as u32,
            name,
            cmdline,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn inspect_pgrp(
        _pgid: libc::pid_t,
        _shell_pid: libc::pid_t,
        _exclude_shell: bool,
    ) -> Option<crate::mux::agents::ForegroundProc> {
        None
    }
}

#[cfg(windows)]
mod win_pty {
    use std::io;
    use std::path::Path;
    use std::sync::atomic::{AtomicU16, Ordering};

    pub struct Integration {
        pub env: Vec<(String, String)>,
        pub args: Vec<String>,
        pub login: bool,
    }

    pub fn integration(program: &str, _dir: &Path, _login: bool) -> Option<Integration> {
        let lower = program.to_ascii_lowercase();
        if lower.ends_with("pwsh.exe")
            || lower.ends_with("powershell.exe")
            || lower == "pwsh"
            || lower == "powershell"
        {
            return Some(Integration {
                env: Vec::new(),
                args: vec!["-NoLogo".to_string()],
                login: false,
            });
        }
        None
    }

    pub fn default_shell() -> String {
        crate::pty::default_shell().0
    }

    pub struct HostPty {
        inner: crate::pty::Pty,
        cols: AtomicU16,
        rows: AtomicU16,
    }

    impl HostPty {
        pub fn spawn(
            cols: u16,
            rows: u16,
            program: &str,
            args: &[String],
            _login: bool,
            extra_env: &[(String, String)],
            cwd: Option<&str>,
        ) -> io::Result<Self> {
            let inner = crate::pty::Pty::spawn_with_env(cols, rows, program, args, extra_env, cwd)
                .map_err(|e| io::Error::other(e.to_string()))?;
            Ok(Self {
                inner,
                cols: AtomicU16::new(cols.max(1)),
                rows: AtomicU16::new(rows.max(1)),
            })
        }

        pub fn pid(&self) -> u32 {
            self.inner.pid()
        }

        pub fn size(&self) -> (u16, u16) {
            (self.cols.load(Ordering::Relaxed), self.rows.load(Ordering::Relaxed))
        }

        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf).map_err(|e| io::Error::other(e.to_string()))
        }

        pub fn write(&self, data: &[u8]) -> io::Result<()> {
            self.inner.write(data).map_err(|e| io::Error::other(e.to_string()))
        }

        pub fn resize(&self, cols: u16, rows: u16, _cell_w: u16, _cell_h: u16) {
            self.cols.store(cols.max(1), Ordering::Relaxed);
            self.rows.store(rows.max(1), Ordering::Relaxed);
            let _ = self.inner.resize(cols.max(1), rows.max(1));
        }

        pub fn hangup(&self) {
            self.inner.close();
        }

        pub fn wait_exit(&self) -> i32 {
            self.inner.wait_exit()
        }

        pub fn foreground_process(
            &self,
            _is_login_shell: bool,
        ) -> Option<crate::mux::agents::ForegroundProc> {
            None
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::HostPty;

    #[test]
    fn foreground_process_inspects_direct_child_command() {
        let pty = HostPty::spawn(
            80,
            24,
            "/bin/sh",
            &["-c".into(), "sleep 2".into()],
            false,
            &[],
            None,
        )
        .expect("spawn /bin/sh");
        let mut found = None;
        for _ in 0..40 {
            if let Some(fg) = pty.foreground_process(false) {
                if fg.name == "sh" || fg.name == "sleep" {
                    found = Some(fg);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let fg = found.expect("expected active foreground process after execve");
        assert!(
            fg.cmdline.contains("sh") || fg.cmdline.contains("sleep"),
            "unexpected fg cmdline: {fg:?}"
        );
        pty.hangup();
        let _ = pty.wait_exit();
    }
}
