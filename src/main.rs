//! Tachyon entry point.
//!
//! `windows_subsystem = "windows"` keeps a console window from flashing up
//! behind us on launch, which also matters for a clean taskbar experience.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
use tachyon::config::Config;

#[cfg(windows)]
fn main() {
    tachyon::log::install_panic_hook();
    // `--quiet` suppresses the message boxes so the CLI paths can be scripted
    // (and so automated checks do not leave dialogs on someone's desktop).
    let quiet = std::env::args().any(|a| a == "--quiet");
    tachyon::log!("--- tachyon {} starting ---", tachyon::APP_VERSION);

    let mut cfg = Config::load_default();

    // A `--config <path>` override is useful for trying settings without
    // touching the installed file.
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(path) = args.next() {
                    cfg = Config::load(std::path::Path::new(&path));
                }
            }
            "-e" | "--command" => {
                // Everything after this is the program to run instead of a shell.
                let rest: Vec<String> = args.by_ref().collect();
                if let Some((program, params)) = rest.split_first() {
                    cfg.shell.program = program.clone();
                    cfg.shell.args = params.to_vec();
                }
                break;
            }
            "--install" => {
                match tachyon::win::install::install() {
                    Ok(path) => {
                        report(quiet, &tachyon::win::install::describe_install(&path));
                    }
                    Err(e) => {
                        report(quiet, &format!("Install failed:\n\n{e}"));
                        std::process::exit(1);
                    }
                }
                return;
            }
            "--uninstall" => {
                match tachyon::win::install::uninstall() {
                    Ok(()) => report(quiet, "Tachyon's Start menu shortcut was removed."),
                    Err(e) => {
                        report(quiet, &format!("Uninstall failed:\n\n{e}"));
                        std::process::exit(1);
                    }
                }
                return;
            }
            "--quiet" => {}
            "--version" | "-V" => {
                message_box(&format!(
                    "{} {}",
                    tachyon::APP_NAME,
                    tachyon::APP_VERSION
                ));
                return;
            }
            "--help" | "-h" => {
                message_box(HELP);
                return;
            }
            _ => {}
        }
    }

    if !cfg.warnings.is_empty() {
        // Surface configuration problems rather than starting with settings the
        // user did not ask for and cannot see.
        let path = Config::default_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        message_box(&format!(
            "Tachyon found problems in {path}:\n\n{}\n\nStarting with defaults for those settings.",
            cfg.warnings.join("\n")
        ));
    }

    if let Err(e) = tachyon::win::run(cfg) {
        tachyon::log::force(&format!("FATAL: {e}"));
        message_box(&format!(
            "Tachyon failed to start:\n\n{e}\n\nDetails: {}",
            tachyon::log::path_string()
        ));
    }
    tachyon::log!("--- exit ---");
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Tachyon targets Windows. Run `cargo test` to exercise the portable core.");
    std::process::exit(1);
}

#[cfg(windows)]
const HELP: &str = "\
Tachyon - GPU-accelerated terminal

  tachyon                       start the configured shell
  tachyon --config <path>       use an alternate config file
  tachyon -e <program> [args]   run a program instead of the shell
  tachyon --install             install to %LOCALAPPDATA% and add a Start
                                menu entry you can pin to the taskbar
  tachyon --uninstall           remove the Start menu entry
  tachyon --quiet               with --install/--uninstall, no dialogs
  tachyon --version
";

/// Show `text` in a dialog, or record it to the log under `--quiet`.
#[cfg(windows)]
fn report(quiet: bool, text: &str) {
    tachyon::log::force(text);
    if !quiet {
        message_box(text);
    }
}

#[cfg(windows)]
fn message_box(text: &str) {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    let body = HSTRING::from(text);
    let title = HSTRING::from(tachyon::APP_NAME);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}
