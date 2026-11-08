//! Keyboard encoding for macOS.
//!
//! Text comes from `NSEvent.characters`, which already applies the keyboard
//! layout, Shift, and Control (Ctrl+C arrives as U+0003). Keys with no
//! character -- arrows, function keys, navigation -- are identified by their
//! hardware key code and turned into xterm escape sequences, matching the
//! Windows backend's `encode_key`.

use crate::term::TermMode;

#[derive(Clone, Copy, Default, Debug)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub option: bool,
}

impl Mods {
    fn any(&self) -> bool {
        self.shift || self.ctrl || self.option
    }

    /// xterm's modifier parameter: 1 plus a bitmask, Option counting as Alt.
    fn xterm_param(&self) -> u8 {
        1 + self.shift as u8 + (self.option as u8) * 2 + (self.ctrl as u8) * 4
    }
}

// Carbon virtual key codes (Events.h). They name physical keys, so they are the
// same on every layout.
const RETURN: u16 = 0x24;
const TAB: u16 = 0x30;
const DELETE: u16 = 0x33;
const ESCAPE: u16 = 0x35;
const KEYPAD_ENTER: u16 = 0x4C;
const HOME: u16 = 0x73;
const PAGE_UP: u16 = 0x74;
const FORWARD_DELETE: u16 = 0x75;
const END: u16 = 0x77;
const PAGE_DOWN: u16 = 0x79;
const LEFT: u16 = 0x7B;
const RIGHT: u16 = 0x7C;
const DOWN: u16 = 0x7D;
const UP: u16 = 0x7E;
const F_KEYS: [(u16, u8); 12] = [
    (0x7A, 1),
    (0x78, 2),
    (0x63, 3),
    (0x76, 4),
    (0x60, 5),
    (0x61, 6),
    (0x62, 7),
    (0x64, 8),
    (0x65, 9),
    (0x6D, 10),
    (0x67, 11),
    (0x6F, 12),
];

/// Bytes for one key press, or `None` if it produces nothing.
///
/// `chars` is `NSEvent.characters`; `bare` is `charactersIgnoringModifiers`,
/// used when Option acts as Meta. With `option_as_meta` off, Option types the
/// layout's special characters (Option+E then E gives é) as everywhere else on
/// the Mac.
pub fn encode(
    key_code: u16,
    chars: &str,
    bare: &str,
    mods: Mods,
    mode: TermMode,
    option_as_meta: bool,
) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor = |final_byte: u8| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
        } else if app_cursor {
            vec![0x1b, b'O', final_byte]
        } else {
            vec![0x1b, b'[', final_byte]
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[{};{}~", n, mods.xterm_param()).into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let meta = |b: u8| -> Vec<u8> {
        if mods.option && option_as_meta {
            vec![0x1b, b]
        } else {
            vec![b]
        }
    };

    let special = match key_code {
        UP => Some(cursor(b'A')),
        DOWN => Some(cursor(b'B')),
        RIGHT => Some(cursor(b'C')),
        LEFT => Some(cursor(b'D')),
        HOME => Some(cursor(b'H')),
        END => Some(cursor(b'F')),
        FORWARD_DELETE => Some(tilde(3)),
        PAGE_UP => Some(tilde(5)),
        PAGE_DOWN => Some(tilde(6)),
        DELETE => Some(if mods.ctrl { vec![0x08] } else { meta(0x7f) }),
        ESCAPE => Some(meta(0x1b)),
        TAB if mods.shift => Some(b"\x1b[Z".to_vec()),
        TAB => Some(meta(0x09)),
        RETURN | KEYPAD_ENTER => {
            let mut v = meta(b'\r');
            if mode.contains(TermMode::LINE_FEED_NL) {
                v.push(b'\n');
            }
            Some(v)
        }
        _ => F_KEYS.iter().find(|(k, _)| *k == key_code).map(|&(_, n)| match n {
            // F1-F4 keep their historical SS3 form when unmodified.
            1..=4 => {
                let final_byte = b'P' + (n - 1);
                if mods.any() {
                    format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
                } else {
                    vec![0x1b, b'O', final_byte]
                }
            }
            5 => tilde(15),
            6 => tilde(17),
            7 => tilde(18),
            8 => tilde(19),
            9 => tilde(20),
            10 => tilde(21),
            11 => tilde(23),
            _ => tilde(24),
        }),
    };
    if special.is_some() {
        return special;
    }

    // Ctrl+Space and Ctrl+@ are NUL; AppKit reports them as a plain space.
    if mods.ctrl && (bare == " " || bare == "@" || bare == "2") {
        return Some(vec![0]);
    }

    if mods.option && option_as_meta && !bare.is_empty() {
        let mut v = vec![0x1b];
        v.extend_from_slice(bare.as_bytes());
        return Some(v);
    }

    // AppKit puts private-use codepoints in `characters` for keys it has no
    // text for; never send those to the shell.
    let text: String = chars.chars().filter(|c| !('\u{F700}'..='\u{F8FF}').contains(c)).collect();
    (!text.is_empty()).then(|| text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(code: u16, chars: &str, mods: Mods) -> Vec<u8> {
        encode(code, chars, chars, mods, TermMode::default(), false).unwrap_or_default()
    }

    #[test]
    fn arrows_follow_cursor_key_mode() {
        assert_eq!(enc(UP, "\u{F700}", Mods::default()), b"\x1b[A");
        let app = encode(UP, "", "", Mods::default(), TermMode::APP_CURSOR, false).unwrap();
        assert_eq!(app, b"\x1bOA");
        let shift = Mods { shift: true, ..Mods::default() };
        assert_eq!(enc(LEFT, "", shift), b"\x1b[1;2D");
    }

    #[test]
    fn text_and_control_characters_pass_through() {
        assert_eq!(enc(0x00, "a", Mods::default()), b"a");
        let ctrl = Mods { ctrl: true, ..Mods::default() };
        assert_eq!(enc(0x08, "\u{3}", ctrl), b"\x03");
        assert_eq!(enc(0x31, " ", ctrl), b"\0");
        assert_eq!(enc(0x0E, "é", Mods::default()), "é".as_bytes());
    }

    #[test]
    fn option_is_meta_only_when_asked() {
        let opt = Mods { option: true, ..Mods::default() };
        assert_eq!(encode(0x0B, "∫", "b", opt, TermMode::default(), true).unwrap(), b"\x1bb");
        assert_eq!(encode(0x0B, "∫", "b", opt, TermMode::default(), false).unwrap(), "∫".as_bytes());
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(DELETE, "\u{7f}", Mods::default()), b"\x7f");
        assert_eq!(enc(RETURN, "\r", Mods::default()), b"\r");
        assert_eq!(enc(TAB, "\t", Mods { shift: true, ..Mods::default() }), b"\x1b[Z");
        assert_eq!(enc(0x7A, "", Mods::default()), b"\x1bOP");
        assert_eq!(enc(0x60, "", Mods::default()), b"\x1b[15~");
    }
}
