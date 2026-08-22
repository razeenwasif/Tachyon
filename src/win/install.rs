//! Installation: put Tachyon somewhere permanent and give it a Start menu
//! entry that Windows will let the user pin.
//!
//! # Why this is needed for pinning
//!
//! Windows 11 will not pin an arbitrary `.exe` sitting in a downloads folder in
//! a way that survives; what it pins is an *application identity*. Two things
//! establish that identity, and they must agree:
//!
//!  * The running process calls `SetCurrentProcessExplicitAppUserModelID` (we
//!    do that in [`crate::win::run`]).
//!  * A shortcut in the Start menu carries the same string in its
//!    `System.AppUserModel.ID` property.
//!
//! When they match, the taskbar button for a live window and the pinned
//! shortcut are the same entry, so "Pin to taskbar" sticks and relaunching from
//! the pin reuses the icon slot. When they do not, you get two buttons and the
//! pin appears to do nothing -- which is the failure everyone hits.
//!
//! Setting that property needs `IPropertyStore` on the shell link, which is why
//! this lives in Rust rather than in the installer script.

use std::path::{Path, PathBuf};

use windows::core::{w, Interface, Result, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PROPVARIANT, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SHStrDupW};

use crate::{APP_NAME, APP_USER_MODEL_ID};

/// `%LOCALAPPDATA%\Programs\Tachyon`
pub fn install_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("Programs").join("Tachyon"))
}

/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Tachyon.lnk`
pub fn shortcut_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(base)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join(format!("{APP_NAME}.lnk")),
    )
}

/// Copy the running executable into the install directory and create the Start
/// menu shortcut. Returns the installed executable path.
pub fn install() -> std::result::Result<PathBuf, String> {
    let dir = install_dir().ok_or("LOCALAPPDATA is not set")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = dir.join("tachyon.exe");

    // Running from the install location already (a re-run of --install to
    // repair the shortcut) is fine; copying a file onto itself is not.
    let same = current
        .canonicalize()
        .ok()
        .zip(target.canonicalize().ok())
        .map_or(false, |(a, b)| a == b);
    if !same {
        std::fs::copy(&current, &target)
            .map_err(|e| format!("could not copy to {}: {e}", target.display()))?;
    }

    // Ship the icon alongside so the shortcut has something to point at even if
    // the exe is later replaced.
    let ico = dir.join("tachyon.ico");
    if let Some(src) = current.parent().map(|p| p.join("tachyon.ico")) {
        if src.exists() {
            let _ = std::fs::copy(&src, &ico);
        }
    }

    // Put the install directory on PATH so `tachyon` works as a command --
    // notably `tachyon -e wsl.exe`, which is otherwise unreachable because
    // %LOCALAPPDATA%\Programs is not searched by default.
    match add_to_user_path(&dir) {
        Ok(true) => crate::log!("added {} to the user PATH", dir.display()),
        Ok(false) => {}
        Err(e) => crate::log!("could not update PATH: {e}"),
    }

    let link = shortcut_path().ok_or("APPDATA is not set")?;
    if let Some(parent) = link.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    create_shortcut(&target, &dir, &link).map_err(|e| format!("could not write shortcut: {e}"))?;
    crate::log!("installed to {}", target.display());

    Ok(target)
}

pub fn uninstall() -> std::result::Result<(), String> {
    if let Some(link) = shortcut_path() {
        let _ = std::fs::remove_file(link);
    }
    if let Some(dir) = install_dir() {
        let _ = remove_from_user_path(&dir);
    }
    // The executable itself is left in place: on Windows a running process
    // cannot delete its own image, and leaving it is less surprising than
    // scheduling a delete-on-reboot.
    Ok(())
}

/// Build a `.lnk` carrying our AppUserModelID.
fn create_shortcut(exe: &Path, working_dir: &Path, link_path: &Path) -> Result<()> {
    unsafe {
        // `--install` runs before the window path, so nothing has initialised COM
        // for this thread yet. Repeat calls are harmless.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;

        let exe_h = HSTRING::from(exe.as_os_str());
        link.SetPath(PCWSTR(exe_h.as_ptr()))?;

        let wd_h = HSTRING::from(working_dir.as_os_str());
        link.SetWorkingDirectory(PCWSTR(wd_h.as_ptr()))?;

        let desc = HSTRING::from("GPU-accelerated terminal");
        link.SetDescription(PCWSTR(desc.as_ptr()))?;

        // Icon index 0 of the executable is the group icon build.rs embedded.
        link.SetIconLocation(PCWSTR(exe_h.as_ptr()), 0)?;

        // The part that makes pinning work.
        //
        // The string must be owned by the COM allocator. `IPropertyStore`
        // documents that it copies the value and the caller keeps ownership,
        // but the shell's implementation frees what it is handed with
        // `CoTaskMemFree` -- pointing it at a Rust-heap buffer corrupts the
        // heap. `SHStrDupW` allocates the copy correctly, and
        // `PropVariantClear` releases it through the matching allocator.
        let store: IPropertyStore = link.cast()?;
        let aumid = HSTRING::from(APP_USER_MODEL_ID);
        let owned: PWSTR = SHStrDupW(PCWSTR(aumid.as_ptr()))?;

        let mut value = PROPVARIANT::default();
        value.Anonymous.Anonymous = std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
            vt: VT_LPWSTR,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: PROPVARIANT_0_0_0 { pwszVal: owned },
        });

        store.SetValue(&PKEY_AppUserModel_ID, &value)?;
        store.Commit()?;
        PropVariantClear(&mut value)?;

        let file: IPersistFile = link.cast()?;
        let path_h = HSTRING::from(link_path.as_os_str());
        file.Save(PCWSTR(path_h.as_ptr()), true)?;
    }
    Ok(())
}

/// Human-readable summary for the message box shown after `--install`.
pub fn describe_install(exe: &Path) -> String {
    let link = shortcut_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "Tachyon installed.\n\n\
         Program:  {}\n\
         Shortcut: {}\n\n\
         To pin it: open the Start menu, search for Tachyon, right-click it and \
         choose \"Pin to taskbar\".\n\n\
         The install directory was added to your PATH, so `tachyon` works as a \
         command in any new shell -- for example `tachyon -e wsl.exe` to open a \
         window running WSL. Run `tachyon --uninstall` to undo both.",
        exe.display(),
        link
    )
}

// ===========================================================================
// PATH registration
// ===========================================================================

/// Read `HKCU\Environment\Path`, returning the value and its registry type.
///
/// The type matters: this value is normally `REG_EXPAND_SZ` because it contains
/// entries like `%USERPROFILE%\bin`. Rewriting it as a plain `REG_SZ` would stop
/// those expanding and quietly break other programs, so it is preserved.
fn read_user_path() -> Result<(String, REG_VALUE_TYPE)> {
    unsafe {
        let mut key = HKEY::default();
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            None,
            KEY_READ,
            &mut key,
        )
        .ok()?;

        let mut kind = REG_VALUE_TYPE::default();
        let mut size: u32 = 0;
        let query = RegQueryValueExW(
            key,
            w!("Path"),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        );

        let value = if query.is_ok() && size > 0 {
            let mut buf = vec![0u8; size as usize];
            RegQueryValueExW(
                key,
                w!("Path"),
                None,
                Some(&mut kind),
                Some(buf.as_mut_ptr()),
                Some(&mut size),
            )
            .ok()?;
            let wide: &[u16] = std::slice::from_raw_parts(
                buf.as_ptr() as *const u16,
                (size as usize) / 2,
            );
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            String::from_utf16_lossy(&wide[..end])
        } else {
            // No user PATH at all is normal on a fresh profile.
            kind = REG_EXPAND_SZ;
            String::new()
        };

        let _ = RegCloseKey(key);
        Ok((value, kind))
    }
}

fn write_user_path(value: &str, kind: REG_VALUE_TYPE) -> Result<()> {
    unsafe {
        let mut key = HKEY::default();
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            None,
            KEY_SET_VALUE,
            &mut key,
        )
        .ok()?;

        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = std::slice::from_raw_parts(
            wide.as_ptr() as *const u8,
            std::mem::size_of_val(&wide[..]),
        );
        let result = RegSetValueExW(key, w!("Path"), None, kind, Some(bytes));
        let _ = RegCloseKey(key);
        result.ok()?;

        // Without this, only processes started after the next sign-in would see
        // the change. Explorer rebroadcasts it to everything it launches.
        let env = w!("Environment");
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(env.0 as isize),
            SMTO_ABORTIFHUNG,
            5000,
            None,
        );
    }
    Ok(())
}

/// Returns `Ok(true)` if the directory was added, `Ok(false)` if already there.
fn add_to_user_path(dir: &Path) -> Result<bool> {
    let target = dir.to_string_lossy().to_string();
    let (current, kind) = read_user_path()?;

    if current
        .split(';')
        .any(|e| e.trim().trim_end_matches('\\').eq_ignore_ascii_case(target.trim_end_matches('\\')))
    {
        return Ok(false);
    }

    let mut updated = current.clone();
    if !updated.is_empty() && !updated.ends_with(';') {
        updated.push(';');
    }
    updated.push_str(&target);

    write_user_path(&updated, kind)?;
    Ok(true)
}

fn remove_from_user_path(dir: &Path) -> Result<bool> {
    let target = dir.to_string_lossy().to_string();
    let (current, kind) = read_user_path()?;
    if current.is_empty() {
        return Ok(false);
    }

    let kept: Vec<&str> = current
        .split(';')
        .filter(|e| {
            !e.trim()
                .trim_end_matches('\\')
                .eq_ignore_ascii_case(target.trim_end_matches('\\'))
        })
        .collect();

    if kept.len() == current.split(';').count() {
        return Ok(false);
    }
    write_user_path(&kept.join(";"), kind)?;
    Ok(true)
}
