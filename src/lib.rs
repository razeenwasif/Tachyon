//! Tachyon -- a GPU-accelerated terminal emulator for Windows.
//!
//! The crate is split so that everything which does not touch Win32 lives in
//! portable modules. That is not architectural purity for its own sake: the
//! terminal state machine is where correctness bugs hide, and keeping it
//! host-agnostic means the whole test suite runs anywhere, including under WSL
//! while cross-building.

pub mod config;
pub mod log;
pub mod term;

#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod pty;
#[cfg(windows)]
pub mod render;
#[cfg(windows)]
pub mod win;

/// Identity used for taskbar grouping and jump lists. Must match the
/// `System.AppUserModel.ID` stamped on the Start menu shortcut by the
/// installer, or Windows will treat a pinned shortcut and a running window as
/// two different applications.
pub const APP_USER_MODEL_ID: &str = "Tachyon.Terminal";

pub const APP_NAME: &str = "Tachyon";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
