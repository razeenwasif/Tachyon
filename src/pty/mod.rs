//! ConPTY backend.
//!
//! Windows' pseudoconsole gives us a real VT stream from any console program,
//! including ones that still speak the legacy console API -- conhost translates
//! on our behalf. The setup is fiddly but mechanical:
//!
//!  1. Two anonymous pipes: one carrying our keystrokes in, one carrying the
//!     application's output back.
//!  2. `CreatePseudoConsole` takes the child-side ends and returns an `HPCON`.
//!  3. The `HPCON` is passed to `CreateProcessW` through an extended startup
//!     info attribute list, which is how the child inherits it.
//!
//! Reads happen on a dedicated thread with a blocking `ReadFile`. Overlapped
//! I/O would let one thread service several sessions, but with a thread per
//! session there is nothing to gain: a blocked thread costs a stack and no CPU,
//! and the code that is not written cannot deadlock.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use windows::core::{Result, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_BROKEN_PIPE, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::*;

/// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`. Not exposed by the metadata, so it is
/// spelled out here; the value is stable and documented.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

/// A raw handle that we are willing to move between threads.
///
/// `HANDLE` wraps a pointer and so is not `Send` by default. Kernel handles are
/// process-wide and safe to use from any thread; the invariant we are asserting
/// is ownership, which the owning type enforces.
#[derive(Clone, Copy)]
pub struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

impl SendHandle {
    pub fn new(h: HANDLE) -> SendHandle {
        SendHandle(h)
    }

    /// Take the handle back out.
    ///
    /// This is a method rather than a public field on purpose: closure capture
    /// is field-precise since edition 2021, so reading `x.0` inside a `move`
    /// closure would capture the bare `HANDLE` and defeat the `Send` wrapper.
    /// Going through `self` forces the whole wrapper to be captured.
    #[inline]
    pub fn get(self) -> HANDLE {
        self.0
    }
}

pub struct Pty {
    hpcon: HPCON,
    /// We write child input here.
    input_write: HANDLE,
    /// We read child output here.
    output_read: HANDLE,
    process: HANDLE,
    thread: HANDLE,
    attr_list: Vec<u8>,
    closed: Arc<AtomicBool>,
    /// Serialises `WriteFile` on the input pipe.
    ///
    /// Both the UI thread (keystrokes, pastes) and the reader thread (device
    /// report replies) write here. A partial write from one interleaved with
    /// another would corrupt the byte stream the shell sees, so writes are
    /// mutually exclusive.
    write_lock: Mutex<()>,
}

// SAFETY: every handle is owned exclusively by this `Pty`, kernel handles are
// process-wide, and the one operation that is not inherently atomic --
// writing to the input pipe -- is serialised by `write_lock`.
unsafe impl Send for Pty {}
unsafe impl Sync for Pty {}

impl Pty {
    pub fn spawn(
        cols: u16,
        rows: u16,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
    ) -> Result<Pty> {
        let mut environment = build_environment();
        unsafe {
            // Pipe A: us -> child stdin.
            let mut in_read = HANDLE::default();
            let mut in_write = HANDLE::default();
            CreatePipe(&mut in_read, &mut in_write, None, 0)?;

            // Pipe B: child stdout -> us. A generous buffer keeps a bursty
            // producer from stalling on us between reads.
            let mut out_read = HANDLE::default();
            let mut out_write = HANDLE::default();
            CreatePipe(&mut out_read, &mut out_write, None, 1 << 20)?;

            let size = COORD {
                X: cols.max(1) as i16,
                Y: rows.max(1) as i16,
            };
            let hpcon = match CreatePseudoConsole(size, in_read, out_write, 0) {
                Ok(h) => h,
                Err(e) => {
                    let _ = CloseHandle(in_read);
                    let _ = CloseHandle(in_write);
                    let _ = CloseHandle(out_read);
                    let _ = CloseHandle(out_write);
                    return Err(e);
                }
            };

            // The pseudoconsole duplicated what it needs; our copies of the
            // child-side ends must go, or the pipes never report EOF.
            let _ = CloseHandle(in_read);
            let _ = CloseHandle(out_write);

            // Build the attribute list that carries the HPCON to the child.
            let mut attr_size: usize = 0;
            let _ = InitializeProcThreadAttributeList(
                Some(LPPROC_THREAD_ATTRIBUTE_LIST::default()),
                1,
                None,
                &mut attr_size,
            );
            let mut attr_list = vec![0u8; attr_size];
            let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(attr_list.as_mut_ptr() as *mut c_void);
            InitializeProcThreadAttributeList(Some(attrs), 1, None, &mut attr_size)?;
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                Some(hpcon.0 as *const c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )?;

            let mut si = STARTUPINFOEXW {
                StartupInfo: STARTUPINFOW {
                    cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                    ..Default::default()
                },
                lpAttributeList: attrs,
            };

            let mut cmdline: Vec<u16> = build_command_line(program, args);

            // Without an explicit directory the child inherits ours, which is
            // wherever the process was launched from -- often C:\Windows. Shell
            // prompts that inspect the working directory (starship, oh-my-posh)
            // then scan a system folder on every prompt, which is both wrong and
            // slow.
            let start_dir = cwd
                .map(str::to_string)
                .or_else(|| std::env::var("USERPROFILE").ok())
                .filter(|p| !p.is_empty());
            let cwd_h = start_dir.map(HSTRING::from);
            let cwd_ptr = cwd_h
                .as_ref()
                .map(|h| PCWSTR(h.as_ptr()))
                .unwrap_or(PCWSTR::null());

            let mut pi = PROCESS_INFORMATION::default();
            let res = CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                Some(environment.as_mut_ptr() as *mut c_void),
                cwd_ptr,
                &mut si.StartupInfo,
                &mut pi,
            );

            if let Err(e) = res {
                DeleteProcThreadAttributeList(attrs);
                ClosePseudoConsole(hpcon);
                let _ = CloseHandle(in_write);
                let _ = CloseHandle(out_read);
                return Err(e);
            }

            Ok(Pty {
                hpcon,
                input_write: in_write,
                output_read: out_read,
                process: pi.hProcess,
                thread: pi.hThread,
                attr_list,
                closed: Arc::new(AtomicBool::new(false)),
                write_lock: Mutex::new(()),
            })
        }
    }

    /// Handle the reader thread should poll. Cloning it is safe; the `Pty`
    /// remains the owner and closes it on drop.
    pub fn output_handle(&self) -> SendHandle {
        SendHandle(self.output_read)
    }

    pub fn process_handle(&self) -> SendHandle {
        SendHandle(self.process)
    }

    pub fn closed_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closed)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let size = COORD {
            X: cols.max(1) as i16,
            Y: rows.max(1) as i16,
        };
        unsafe { ResizePseudoConsole(self.hpcon, size) }
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() || self.closed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _guard = self.write_lock.lock();
        let mut offset = 0usize;
        while offset < data.len() {
            let mut written = 0u32;
            unsafe {
                WriteFile(
                    self.input_write,
                    Some(&data[offset..]),
                    Some(&mut written),
                    None,
                )?;
            }
            if written == 0 {
                break;
            }
            offset += written as usize;
        }
        Ok(())
    }

    /// Has the child exited?
    pub fn has_exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }

    pub fn exit_code(&self) -> u32 {
        let mut code = 0u32;
        unsafe {
            let _ = GetExitCodeProcess(self.process, &mut code);
        }
        code
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        unsafe {
            // Closing the pseudoconsole signals the child and unblocks the
            // reader thread's `ReadFile` with a broken pipe.
            ClosePseudoConsole(self.hpcon);
            let _ = CloseHandle(self.input_write);
            let _ = CloseHandle(self.output_read);
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
            if !self.attr_list.is_empty() {
                DeleteProcThreadAttributeList(LPPROC_THREAD_ATTRIBUTE_LIST(
                    self.attr_list.as_mut_ptr() as *mut c_void,
                ));
            }
        }
    }
}

/// Blocking read of the next chunk of child output.
///
/// Returns `Ok(0)` when the pipe closes, which is how we learn the session is
/// over.
pub fn read_output(handle: SendHandle, buf: &mut [u8]) -> Result<usize> {
    let mut read = 0u32;
    unsafe {
        match ReadFile(handle.get(), Some(buf), Some(&mut read), None) {
            Ok(()) => Ok(read as usize),
            Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
            Err(e) => Err(e),
        }
    }
}

/// Variables we set for every child, and the reason each one is needed.
const TERMINAL_ENV: &[(&str, &str)] = &[
    // What we actually implement. Applications key their capability lookups off
    // this, and getting it wrong is the usual cause of "colours are broken" or
    // "arrow keys print garbage".
    ("TERM", "xterm-256color"),
    // TERM says 256; this is how an application learns we do 24-bit.
    ("COLORTERM", "truecolor"),
    ("TERM_PROGRAM", "Tachyon"),
    ("TERM_PROGRAM_VERSION", crate::APP_VERSION),
];

/// Build the child's environment block: ours, plus the terminal variables.
///
/// The block is `KEY=VALUE\0` repeated, terminated by an extra `\0`. Windows
/// expects it sorted case-insensitively by name.
fn build_environment() -> Vec<u16> {
    use std::collections::BTreeMap;

    // Key the map on the uppercased name so our overrides replace an inherited
    // `Term` or `term` rather than sitting alongside it.
    let mut vars: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (k, v) in std::env::vars() {
        // Skip our own diagnostics; a child shell has no use for them and
        // `TACHYON_DUMP` would make nested instances fight over the file.
        if k.starts_with("TACHYON_") {
            continue;
        }
        vars.insert(k.to_uppercase(), (k, v));
    }

    for (k, v) in TERMINAL_ENV {
        vars.insert(k.to_uppercase(), ((*k).to_string(), (*v).to_string()));
    }

    // WSLENV is how Win32 variables cross into a WSL distribution: each name
    // listed with the `/u` flag is copied in when wsl.exe is invoked from
    // Windows. Without this, `wsl.exe` sessions get the distro's default TERM
    // and lose our truecolour advertisement. Append rather than replace, so a
    // user's existing WSLENV survives.
    let mut wslenv = vars
        .get("WSLENV")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    for (k, _) in TERMINAL_ENV {
        let entry = format!("{k}/u");
        if !wslenv.split(':').any(|e| e == entry) {
            if !wslenv.is_empty() {
                wslenv.push(':');
            }
            wslenv.push_str(&entry);
        }
    }
    vars.insert("WSLENV".into(), ("WSLENV".into(), wslenv));

    let mut block: Vec<u16> = Vec::new();
    for (_, (name, value)) in vars {
        block.extend(name.encode_utf16());
        block.push(b'=' as u16);
        block.extend(value.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// Quote an argument the way `CommandLineToArgvW` will parse it back.
fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"', '\n']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Backslashes immediately before a quote must be doubled.
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // ...and again before the closing quote.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
}

fn build_command_line(program: &str, args: &[String]) -> Vec<u16> {
    let mut s = String::new();
    quote_arg(program, &mut s);
    for a in args {
        s.push(' ');
        quote_arg(a, &mut s);
    }
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Pick a shell: the configured one, else the best available, in the order most
/// users would expect on a modern Windows install.
pub fn default_shell() -> (String, Vec<String>) {
    for candidate in ["pwsh.exe", "powershell.exe"] {
        if let Some(path) = which(candidate) {
            // `-NoLogo` keeps the banner out of a fresh window.
            return (path, vec!["-NoLogo".to_string()]);
        }
    }
    let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    (comspec, Vec::new())
}

/// Resolve an executable against `PATH`, the way `CreateProcess` would.
pub fn which(name: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::SearchPathW;

    let wide = HSTRING::from(name);
    let mut buf = vec![0u16; 1024];
    let len = unsafe {
        SearchPathW(
            PCWSTR::null(),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            Some(&mut buf),
            None,
        )
    };
    if len == 0 || len as usize >= buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    fn q(s: &str) -> String {
        let mut out = String::new();
        quote_arg(s, &mut out);
        out
    }

    #[test]
    fn simple_arguments_are_not_quoted() {
        assert_eq!(q("hello"), "hello");
        assert_eq!(q("-NoLogo"), "-NoLogo");
    }

    #[test]
    fn spaces_and_quotes_are_escaped() {
        assert_eq!(q("hello world"), "\"hello world\"");
        assert_eq!(q("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(q("C:\\Program Files\\"), "\"C:\\Program Files\\\\\"");
        assert_eq!(q(""), "\"\"");
    }
}
