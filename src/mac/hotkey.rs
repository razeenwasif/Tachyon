//! A global hotkey: one key combination that reaches Tachyon from any app,
//! for the quick terminal.
//!
//! It's registered with the Carbon Event Manager (`RegisterEventHotKey`),
//! which needs no accessibility permission: the system takes the combination
//! for Tachyon alone and sends it to the app's event target, which AppKit's
//! run loop delivers on the main thread.

use std::ffi::c_void;
use std::sync::OnceLock;

/// A key combination: a virtual key code and Carbon modifier bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Combo {
    pub key: u32,
    pub modifiers: u32,
}

const CMD: u32 = 1 << 8;
const SHIFT: u32 = 1 << 9;
const OPTION: u32 = 1 << 11;
const CONTROL: u32 = 1 << 12;

/// Virtual key codes on an ANSI keyboard, by the name a setting uses.
fn key_code(name: &str) -> Option<u32> {
    const LETTERS: [(char, u32); 26] = [
        ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7), ('c', 8), ('v', 9),
        ('b', 11), ('q', 12), ('w', 13), ('e', 14), ('r', 15), ('y', 16), ('t', 17), ('o', 31), ('u', 32),
        ('i', 34), ('p', 35), ('l', 37), ('j', 38), ('k', 40), ('n', 45), ('m', 46),
    ];
    const DIGITS: [u32; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    const F: [u32; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    let n = name.to_ascii_lowercase();
    let mut chars = n.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some(&(_, code)) = LETTERS.iter().find(|(l, _)| *l == c) {
            return Some(code);
        }
        if let Some(d) = c.to_digit(10) {
            return Some(DIGITS[d as usize]);
        }
    }
    if let Some(f) = n.strip_prefix('f').and_then(|d| d.parse::<usize>().ok()).filter(|d| (1..=12).contains(d)) {
        return Some(F[f - 1]);
    }
    Some(match n.as_str() {
        "`" | "grave" | "backtick" | "backquote" => 50,
        "space" => 49,
        "tab" => 48,
        "return" | "enter" => 36,
        "escape" | "esc" => 53,
        "-" | "minus" => 27,
        "=" | "equal" => 24,
        "[" => 33,
        "]" => 30,
        ";" => 41,
        "'" => 39,
        "," => 43,
        "." | "period" => 47,
        "/" | "slash" => 44,
        "\\" | "backslash" => 42,
        _ => return None,
    })
}

/// A setting like `ctrl+\``, `cmd+shift+t`, `option+space` or `f12`; `none`
/// (or empty) for no hotkey. A key with no modifier is only allowed for the
/// function keys, so typing elsewhere isn't taken.
pub fn parse(setting: &str) -> Result<Option<Combo>, String> {
    let s = setting.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    // `+` itself can't be the key, so splitting on it is safe; `ctrl++`
    // isn't supported.
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    let (key, mods) = parts.split_last().ok_or("empty hotkey")?;
    let mut modifiers = 0;
    for m in mods {
        modifiers |= match m.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "⌘" => CMD,
            "shift" | "⇧" => SHIFT,
            "opt" | "option" | "alt" | "⌥" => OPTION,
            "ctrl" | "control" | "⌃" => CONTROL,
            other => return Err(format!("unknown modifier `{other}`")),
        };
    }
    let code = key_code(key).ok_or_else(|| format!("unknown key `{key}`"))?;
    let function_key = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111].contains(&code);
    if modifiers == 0 && !function_key {
        return Err(format!("`{key}` alone would be taken from every app: add a modifier"));
    }
    Ok(Some(Combo { key: code, modifiers }))
}

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: Handler,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: *mut c_void,
        options: u32,
        out_ref: *mut *mut c_void,
    ) -> i32;
}

/// What the hotkey does: set once, called on the main thread.
static ACTION: OnceLock<fn()> = OnceLock::new();

extern "C" fn pressed(_next: *mut c_void, _event: *mut c_void, _data: *mut c_void) -> i32 {
    if let Some(f) = ACTION.get() {
        f();
    }
    0
}

/// Register `combo` to call `action`. Only one hotkey is ever registered.
pub fn register(combo: Combo, action: fn()) -> Result<(), String> {
    if ACTION.set(action).is_err() {
        return Err("a hotkey is already registered".into());
    }
    let four = |s: &[u8; 4]| u32::from_be_bytes(*s);
    let spec = EventTypeSpec { event_class: four(b"keyb"), event_kind: 5 };
    let mut handler_ref = std::ptr::null_mut();
    let mut hotkey_ref = std::ptr::null_mut();
    // SAFETY: Carbon calls on the main thread; the handler and its type list
    // outlive the call (the handler is a plain function).
    unsafe {
        let target = GetApplicationEventTarget();
        let err = InstallEventHandler(target, pressed, 1, &spec, std::ptr::null_mut(), &mut handler_ref);
        if err != 0 {
            return Err(format!("InstallEventHandler failed ({err})"));
        }
        let err = RegisterEventHotKey(combo.key, combo.modifiers, EventHotKeyId { signature: four(b"Tchy"), id: 1 }, target, 0, &mut hotkey_ref);
        if err != 0 {
            // -9878: another app holds this combination.
            return Err(format!("the combination is taken (RegisterEventHotKey {err})"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_to_key_combinations() {
        assert_eq!(parse("ctrl+`"), Ok(Some(Combo { key: 50, modifiers: CONTROL })));
        assert_eq!(parse("Cmd+Shift+T"), Ok(Some(Combo { key: 17, modifiers: CMD | SHIFT })));
        assert_eq!(parse("option+space"), Ok(Some(Combo { key: 49, modifiers: OPTION })));
        assert_eq!(parse("⌃+grave"), Ok(Some(Combo { key: 50, modifiers: CONTROL })));
        assert_eq!(parse("f12"), Ok(Some(Combo { key: 111, modifiers: 0 })), "a function key alone");
        assert_eq!(parse("ctrl+1"), Ok(Some(Combo { key: 18, modifiers: CONTROL })));
        assert_eq!(parse("none"), Ok(None));
        assert_eq!(parse(""), Ok(None));
        assert!(parse("t").is_err(), "a letter alone would take typing");
        assert!(parse("hyper+t").is_err());
        assert!(parse("ctrl+banana").is_err());
    }
}
