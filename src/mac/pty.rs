//! Unix pseudo-terminal: `forkpty`, a login shell, and a blocking reader.
//!
//! Same shape as the ConPTY backend: one thread per session blocks in `read`
//! and feeds the parser, and the main thread writes keystrokes. A blocked
//! thread costs a stack and no CPU.
//!
//! Everything that allocates (argv, the environment, the working directory) is
//! prepared *before* `fork`. Between `fork` and `exec` the child only calls
//! async-signal-safe functions: `chdir`, `execve` and `_exit`.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::ptr;

pub struct Pty {
    master: OwnedFd,
    pid: libc::pid_t,
}

impl Pty {
    /// Start `program` on a new PTY of `cols` x `rows`. With `login`, argv[0]
    /// is prefixed with `-`, which is how shells learn they are a login shell
    /// and read `.zprofile` -- the same thing Terminal.app does.
    pub fn spawn(
        cols: u16,
        rows: u16,
        program: &str,
        args: &[String],
        login: bool,
        extra_env: &[(String, String)],
    ) -> io::Result<Pty> {
        let path = resolve(program)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("`{program}` not found")))?;
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

        // New windows start in the home directory, as in Terminal.app, whether
        // Tachyon was launched from Finder (cwd "/") or from a shell.
        let home = std::env::var_os("HOME").and_then(|h| CString::new(h.as_bytes()).ok());

        let mut ws = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut master: libc::c_int = -1;

        // SAFETY: forkpty with valid out-pointers. The child branch below only
        // makes async-signal-safe calls on memory prepared before the fork.
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

        // SAFETY: forkpty returned a fresh descriptor that we now own.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok(Pty { master, pid })
    }

    /// Blocking read. `Ok(0)` means the child is gone: macOS reports a closed
    /// PTY as `EIO` rather than end-of-file, and both mean the same here.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EIO) => return Ok(0),
                _ => return Err(err),
            }
        }
    }

    /// Write all of `data`. The PTY's input buffer is small, so a large paste
    /// blocks until the shell catches up; the reader thread keeps draining its
    /// output meanwhile, so this cannot deadlock.
    pub fn write(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = unsafe { libc::write(self.master.as_raw_fd(), data.as_ptr().cast(), data.len()) };
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

    /// Tell the child its window changed size. The kernel delivers SIGWINCH.
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }
}

impl Pty {
    /// Hang up on the session, the way closing a real terminal does. The shell
    /// exits, its side of the PTY closes, and the reader thread's blocked
    /// `read` returns -- which is what finally drops the last reference.
    pub fn hangup(&self) {
        unsafe {
            libc::kill(self.pid, libc::SIGHUP);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Reap the child so it does not linger as a zombie. Usually it has
        // already exited; if not, hang up and wait for it off this thread.
        let pid = self.pid;
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == 0 {
            unsafe {
                libc::kill(pid, libc::SIGHUP);
            }
            let _ = std::thread::Builder::new()
                .name("tachyon-reaper".into())
                .spawn(move || unsafe {
                    let mut status = 0;
                    libc::waitpid(pid, &mut status, 0);
                });
        }
    }
}

impl Pty {
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    pub fn master_fd(&self) -> libc::c_int {
        self.master.as_raw_fd()
    }
}

/// How to start a shell with Tachyon's integration loaded.
#[derive(Debug, PartialEq, Eq)]
pub struct Integration {
    pub env: Vec<(String, String)>,
    /// Arguments to put before the configured ones.
    pub args: Vec<String>,
    /// Whether to start it as a login shell (`-name` as argv[0]).
    pub login: bool,
}

/// How to load Tachyon's shell integration (the scripts in `dir`) into
/// `program`, or `None` for a shell it has none for. Each shell gets it
/// without its startup files being touched:
///
/// - zsh: `ZDOTDIR` points at `dir/zsh`, whose `.zshenv` restores the real
///   one and loads the integration.
/// - bash: started with `--rcfile` set to the script, in place of
///   `~/.bashrc`. The script reads the startup files bash would have read (the
///   login ones when `login`), then adds its hooks. This works in every bash
///   from macOS's 3.2 on; `ENV` in POSIX mode doesn't in 3.2.
/// - fish: `dir/xdg` goes first in `XDG_DATA_DIRS`, so fish loads its
///   `fish/vendor_conf.d`; the script then puts `XDG_DATA_DIRS` back.
pub fn integration(program: &str, dir: &std::path::Path, login: bool) -> Option<Integration> {
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
            // --rcfile is ignored by login shells, so bash starts as a plain
            // interactive shell and the script reads the login files itself.
            let args = vec!["--rcfile".into(), path("tachyon-integration.bash"), "-i".into()];
            Some(Integration { env, args, login: false })
        }
        "fish" => {
            let orig = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty());
            // The XDG default, so fish still finds the system's data dirs.
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

/// The user's login shell: `$SHELL`, else the account's shell, else zsh.
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
    "/bin/zsh".to_string()
}

/// Absolute path for `program`, searching `$PATH` when it has no slash.
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

/// The parent's environment with the terminal-identity variables replaced and
/// `extra` added (overriding anything of the same name).
fn child_environment(extra: &[(String, String)]) -> Vec<CString> {
    const OURS: [&str; 6] = ["TERM", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "LC_TERMINAL", "LC_TERMINAL_VERSION"];
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(k, _)| !OURS.iter().any(|o| OsStr::new(o) == k))
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
    set("TERM", "xterm-256color");
    set("COLORTERM", "truecolor");
    set("TERM_PROGRAM", "Tachyon");
    set("TERM_PROGRAM_VERSION", crate::APP_VERSION);
    // ssh forwards LC_* by default, TERM_PROGRAM not, so a shell on a
    // cluster can tell it's in Tachyon from this. iTerm2 does the same.
    set("LC_TERMINAL", "Tachyon");
    set("LC_TERMINAL_VERSION", crate::APP_VERSION);
    // Launched from Finder there is no locale, and without one zsh and many
    // tools fall back to ASCII. Terminal.app sets it for the same reason.
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
        set("LANG", "en_US.UTF-8");
    }
    for (k, v) in extra {
        set(k, v);
    }
    env
}

#[cfg(test)]
mod integration_tests {
    use super::integration;
    use std::path::Path;

    #[test]
    fn each_shell_loads_its_own_way() {
        let dir = Path::new("/App/Resources/shell");
        let zsh = integration("/bin/zsh", dir, true).unwrap();
        assert!(zsh.login && zsh.args.is_empty());
        assert!(zsh.env.contains(&("ZDOTDIR".into(), "/App/Resources/shell/zsh".into())));

        // bash reads the login files itself, so it doesn't start as a login
        // shell, which would ignore --rcfile.
        let bash = integration("/opt/homebrew/bin/bash", dir, true).unwrap();
        assert!(!bash.login);
        assert_eq!(bash.args, ["--rcfile", "/App/Resources/shell/tachyon-integration.bash", "-i"]);
        assert!(bash.env.contains(&("_TACHYON_BASH_LOGIN".into(), "1".into())));
        let not_login = integration("bash", dir, false).unwrap();
        assert!(!not_login.env.iter().any(|(k, _)| k == "_TACHYON_BASH_LOGIN"));

        let fish = integration("/opt/homebrew/bin/fish", dir, true).unwrap();
        let xdg = fish.env.iter().find(|(k, _)| k == "XDG_DATA_DIRS").unwrap();
        assert!(xdg.1.starts_with("/App/Resources/shell/xdg:"));

        assert!(integration("/usr/bin/nu", dir, true).is_none());
    }
}
