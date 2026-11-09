//! Keeping the Mac awake while a command runs: one IOKit power assertion for
//! the process, held while any shell's command that asked for it runs, and
//! released when the last one finishes.
//!
//! It stops idle system sleep only; the display still sleeps, and closing
//! the lid of a MacBook on battery still sleeps it, as with `caffeinate -i`.
//! `pmset -g assertions` lists it, named after the commands.

use std::collections::BTreeMap;
use std::ffi::c_void;

use objc2_foundation::NSString;
use parking_lot::Mutex;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

const LEVEL_ON: u32 = 255;

/// Shells keeping the Mac awake, with their command lines, and the
/// assertion held for them.
static STATE: Mutex<(BTreeMap<u64, String>, Option<u32>)> = Mutex::new((BTreeMap::new(), None));

/// Shell `session`'s command `cmdline` keeps the Mac awake, or (`None`) no
/// longer does.
pub fn set(session: u64, cmdline: Option<&str>) {
    let mut st = STATE.lock();
    let changed = match cmdline {
        Some(c) => st.0.insert(session, c.to_string()).as_deref() != Some(c),
        None => st.0.remove(&session).is_some(),
    };
    if !changed {
        return;
    }
    // Re-made whenever the set changes, so its name lists the commands.
    if let Some(id) = st.1.take() {
        unsafe { IOPMAssertionRelease(id) };
    }
    if st.0.is_empty() {
        crate::log!("power: the Mac may sleep again");
        return;
    }
    let names: Vec<String> = st.0.values().map(|c| if c.is_empty() { "a command".into() } else { c.clone() }).collect();
    let name = NSString::from_str(&format!("Tachyon keeps the Mac awake for {}", names.join(", ")));
    let kind = NSString::from_str("PreventUserIdleSystemSleep");
    let mut id = 0u32;
    // SAFETY: NSString is toll-free bridged with CFString.
    let err = unsafe {
        IOPMAssertionCreateWithName(
            objc2::rc::Retained::as_ptr(&kind).cast(),
            LEVEL_ON,
            objc2::rc::Retained::as_ptr(&name).cast(),
            &mut id,
        )
    };
    if err == 0 {
        st.1 = Some(id);
        crate::log!("power: keeping the Mac awake for {}", names.join(", "));
    } else {
        crate::log::force(&format!("power: could not keep the Mac awake ({err:#x})"));
    }
}

/// Whether this process is keeping the Mac awake now.
#[cfg(test)]
pub fn holding() -> bool {
    STATE.lock().1.is_some()
}

/// Whether shell `session` is keeping the Mac awake now.
#[cfg(test)]
pub fn holds(session: u64) -> bool {
    STATE.lock().0.contains_key(&session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_while_any_command_asks_and_listed_by_pmset() {
        set(901, Some("python train.py"));
        set(902, Some("make"));
        assert!(holding());
        let listed = std::process::Command::new("pmset").args(["-g", "assertions"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        assert!(listed.contains("Tachyon keeps the Mac awake for python train.py, make"), "{listed}");
        set(901, None);
        assert!(holding(), "make still runs");
        set(902, None);
        assert!(!holding());
    }
}
