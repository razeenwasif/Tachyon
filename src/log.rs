//! Minimal diagnostics.
//!
//! A GUI-subsystem process has nowhere to print, so failures during startup are
//! otherwise invisible: the window simply never appears. Everything interesting
//! goes to `%LOCALAPPDATA%\Tachyon\tachyon.log`, and a panic hook makes sure an
//! unwind lands there too rather than vanishing.
//!
//! Logging is off unless `TACHYON_LOG` is set, except for panics, which are
//! always recorded.

use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();
static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

fn log_path() -> Option<&'static PathBuf> {
    PATH.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))?;
        let dir = base.join("Tachyon");
        create_dir_all(&dir).ok()?;
        Some(dir.join("tachyon.log"))
    })
    .as_ref()
}

fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("TACHYON_LOG").is_some())
}

/// Append a line to the log. Always writes, regardless of `TACHYON_LOG`.
pub fn force(msg: &str) {
    let Some(path) = log_path() else { return };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

/// Append a line only when logging is switched on.
pub fn write(msg: &str) {
    if enabled() {
        force(msg);
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::write(&format!($($arg)*))
    };
}

/// Record panics to the log and, for a GUI build, show them once so a failed
/// start is not silent.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("PANIC: {info}");
        force(&msg);
        previous(info);
    }));
}

/// Where the log lives, for error messages that point the user at it.
pub fn path_string() -> String {
    log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unavailable)".into())
}
