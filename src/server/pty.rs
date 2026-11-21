//! Portable pseudo-terminal backend for `tachyon server`.
//!
//! - On Unix (macOS & Linux): POSIX `forkpty` with clean pre-fork environment
//!   preparation, login shell support, and exact child exit status reaping.
//! - On Windows: thin adapter over Tachyon's existing ConPTY (`src/pty/mod.rs`).

use std::path::PathBuf;

#[cfg(unix)]
pub use unix_pty::{default_shell, integration, HostPty, Integration};

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
    }
}
