//! Keyboard and mouse encoding.
//!
//! Text goes through `WM_CHAR` rather than being reconstructed from virtual key
//! codes: that is the only path that gets dead keys, IME composition and
//! non-US layouts right, and it hands us the control codes for `Ctrl`+letter
//! for free. Virtual keys are only consulted for keys that have no character --
//! arrows, function keys, navigation -- where we have to synthesise an escape
//! sequence ourselves.

use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::term::TermMode;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Modifiers {
    pub fn current() -> Modifiers {
        unsafe {
            Modifiers {
                shift: GetKeyState(VK_SHIFT.0 as i32) < 0,
                ctrl: GetKeyState(VK_CONTROL.0 as i32) < 0,
                alt: GetKeyState(VK_MENU.0 as i32) < 0,
            }
        }
    }

    pub fn any(&self) -> bool {
        self.shift || self.ctrl || self.alt
    }

    /// xterm's modifier parameter: 1 plus a bitmask.
    fn xterm_param(&self) -> u8 {
        1 + (self.shift as u8) + (self.alt as u8) * 2 + (self.ctrl as u8) * 4
    }
}

/// Encode a non-character key press, or `None` if the key produces text and
/// should be left to `WM_CHAR`.
pub fn encode_key(vk: VIRTUAL_KEY, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    // Cursor and navigation keys share a shape: `CSI [param;] final`, or
    // `SS3 final` in application mode when unmodified.
    let cursor = |final_byte: u8| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
        } else if app_cursor {
            vec![0x1b, b'O', final_byte]
        } else {
            vec![0x1b, b'[', final_byte]
        }
    };

    // Keys addressed by number: `CSI n ~`, with the modifier as a second param.
    let tilde = |n: u8| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[{};{}~", n, mods.xterm_param()).into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let bytes = match vk {
        VK_UP => cursor(b'A'),
        VK_DOWN => cursor(b'B'),
        VK_RIGHT => cursor(b'C'),
        VK_LEFT => cursor(b'D'),
        VK_HOME => cursor(b'H'),
        VK_END => cursor(b'F'),

        VK_INSERT => tilde(2),
        VK_DELETE => tilde(3),
        VK_PRIOR => tilde(5),
        VK_NEXT => tilde(6),

        // F1-F4 keep their historical SS3 encoding when unmodified.
        VK_F1 | VK_F2 | VK_F3 | VK_F4 => {
            let final_byte = b'P' + (vk.0 - VK_F1.0) as u8;
            if mods.any() {
                format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
            } else {
                vec![0x1b, b'O', final_byte]
            }
        }
        VK_F5 => tilde(15),
        VK_F6 => tilde(17),
        VK_F7 => tilde(18),
        VK_F8 => tilde(19),
        VK_F9 => tilde(20),
        VK_F10 => tilde(21),
        VK_F11 => tilde(23),
        VK_F12 => tilde(24),

        VK_BACK => {
            // DEL is what termios expects as the erase character; Ctrl makes it
            // the "delete word" variant that shells bind to BS.
            if mods.ctrl {
                vec![0x08]
            } else if mods.alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        // Shift+Tab is "back-tab"; plain Tab is just HT, which is what shells
        // bind completion to. Ctrl+Tab never reaches here -- the window layer
        // takes it for switching tabs.
        VK_TAB if mods.shift => b"\x1b[Z".to_vec(),
        VK_TAB => {
            let mut v = Vec::new();
            if mods.alt {
                v.push(0x1b);
            }
            v.push(0x09);
            v
        }
        VK_RETURN => {
            let mut v = Vec::new();
            if mods.alt {
                v.push(0x1b);
            }
            v.push(b'\r');
            if mode.contains(TermMode::LINE_FEED_NL) {
                v.push(b'\n');
            }
            v
        }

        _ => return None,
    };

    Some(bytes)
}

/// Mouse buttons we report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// No button held. Only meaningful for motion, which mode 1003 reports even
    /// when nothing is pressed.
    None,
    WheelUp,
    WheelDown,
}

impl MouseButton {
    /// The protocol's button number, before motion and modifier bits.
    fn code(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::None => 3,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
        }
    }

    fn is_wheel(self) -> bool {
        matches!(self, MouseButton::WheelUp | MouseButton::WheelDown)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
}

/// Encode a mouse event for an application that asked for mouse reporting.
///
/// Only SGR (`1006`) and the original X10 encoding are produced. SGR is what
/// every modern application negotiates, and it is the only one that works past
/// column 223.
///
/// Which events an application actually wants depends on the mode it selected:
/// `1000` is presses and releases only, `1002` adds motion while a button is
/// held -- this is the one tmux and nvim enable to drive drag-resizing of
/// splits -- and `1003` adds motion with no button at all.
pub fn encode_mouse(
    button: MouseButton,
    kind: MouseEventKind,
    col: usize,
    row: usize,
    mods: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::ANY_MOUSE) {
        return None;
    }
    // The wheel has no release; reporting one would look like a button 4/5 that
    // was let go, which nothing expects.
    if button.is_wheel() && kind != MouseEventKind::Press {
        return None;
    }
    match kind {
        MouseEventKind::Move => {
            if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                return None;
            }
            // Bare motion is 1003's alone; 1002 wants it only during a drag.
            if button == MouseButton::None && !mode.contains(TermMode::MOUSE_MOTION) {
                return None;
            }
        }
        // "No button" is not a thing that can be pressed or released.
        _ if button == MouseButton::None => return None,
        _ => {}
    }

    let base = button.code();
    let mut code = base;
    if kind == MouseEventKind::Move {
        code += 32;
    }
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.ctrl {
        code += 16;
    }

    // Protocol coordinates are 1-based.
    let (cx, cy) = (col + 1, row + 1);

    if mode.contains(TermMode::MOUSE_SGR) {
        let final_byte = if kind == MouseEventKind::Release {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{code};{cx};{cy}{final_byte}").into_bytes())
    } else {
        // X10: coordinates are offset by 32 and must fit in a byte.
        if cx > 223 || cy > 223 {
            return None;
        }
        // X10 has no way to say *which* button came up, so a release is button
        // 3. The modifier bits stay, which is why this swaps out the base
        // rather than discarding the whole code.
        let code = if kind == MouseEventKind::Release {
            code - base + 3
        } else {
            code
        };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + code as u8,
            32 + cx as u8,
            32 + cy as u8,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
    };

    #[test]
    fn arrows_switch_on_application_cursor_mode() {
        let normal = TermMode::default();
        assert_eq!(encode_key(VK_UP, PLAIN, normal).unwrap(), b"\x1b[A");

        let app = normal | TermMode::APP_CURSOR;
        assert_eq!(encode_key(VK_UP, PLAIN, app).unwrap(), b"\x1bOA");
    }

    #[test]
    fn modifiers_use_the_xterm_parameter_form() {
        let ctrl = Modifiers {
            ctrl: true,
            ..PLAIN
        };
        // Ctrl is bit 4, so the parameter is 1 + 4 = 5.
        assert_eq!(
            encode_key(VK_RIGHT, ctrl, TermMode::default()).unwrap(),
            b"\x1b[1;5C"
        );

        let shift_alt = Modifiers {
            shift: true,
            alt: true,
            ctrl: false,
        };
        // 1 + 1 + 2 = 4.
        assert_eq!(
            encode_key(VK_F1, shift_alt, TermMode::default()).unwrap(),
            b"\x1b[1;4P"
        );
    }

    #[test]
    fn navigation_keys_use_tilde_sequences() {
        let m = TermMode::default();
        assert_eq!(encode_key(VK_DELETE, PLAIN, m).unwrap(), b"\x1b[3~");
        assert_eq!(encode_key(VK_PRIOR, PLAIN, m).unwrap(), b"\x1b[5~");
        assert_eq!(encode_key(VK_F12, PLAIN, m).unwrap(), b"\x1b[24~");
    }

    #[test]
    fn backspace_sends_del_by_default() {
        let m = TermMode::default();
        assert_eq!(encode_key(VK_BACK, PLAIN, m).unwrap(), vec![0x7f]);
        let ctrl = Modifiers {
            ctrl: true,
            ..PLAIN
        };
        assert_eq!(encode_key(VK_BACK, ctrl, m).unwrap(), vec![0x08]);
    }

    #[test]
    fn enter_honours_line_feed_mode() {
        assert_eq!(
            encode_key(VK_RETURN, PLAIN, TermMode::default()).unwrap(),
            b"\r"
        );
        assert_eq!(
            encode_key(VK_RETURN, PLAIN, TermMode::default() | TermMode::LINE_FEED_NL).unwrap(),
            b"\r\n"
        );
    }

    #[test]
    fn tab_sends_ht_so_shells_can_complete() {
        let m = TermMode::default();
        assert_eq!(encode_key(VK_TAB, PLAIN, m).unwrap(), vec![0x09]);

        let shift = Modifiers {
            shift: true,
            ..PLAIN
        };
        assert_eq!(encode_key(VK_TAB, shift, m).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn character_keys_are_left_to_wm_char() {
        assert!(encode_key(VK_A, PLAIN, TermMode::default()).is_none());
        assert!(encode_key(VK_SPACE, PLAIN, TermMode::default()).is_none());
    }

    #[test]
    fn sgr_mouse_encodes_button_and_position() {
        let mode = TermMode::default() | TermMode::MOUSE_CLICK | TermMode::MOUSE_SGR;
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseEventKind::Press, 9, 4, PLAIN, mode).unwrap(),
            b"\x1b[<0;10;5M"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseEventKind::Release, 9, 4, PLAIN, mode).unwrap(),
            b"\x1b[<0;10;5m"
        );
    }

    #[test]
    fn drag_motion_is_reported_in_button_event_mode() {
        // 1002 + 1006 is what tmux and nvim turn on to resize splits.
        let mode = TermMode::default() | TermMode::MOUSE_DRAG | TermMode::MOUSE_SGR;
        // Motion with the left button held: 0 + 32.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseEventKind::Move, 20, 7, PLAIN, mode).unwrap(),
            b"\x1b[<32;21;8M"
        );
        // ... but bare motion belongs to 1003, not 1002.
        assert!(
            encode_mouse(MouseButton::None, MouseEventKind::Move, 20, 7, PLAIN, mode).is_none()
        );

        let any = TermMode::default() | TermMode::MOUSE_MOTION | TermMode::MOUSE_SGR;
        // No button is code 3, so bare motion is 3 + 32.
        assert_eq!(
            encode_mouse(MouseButton::None, MouseEventKind::Move, 20, 7, PLAIN, any).unwrap(),
            b"\x1b[<35;21;8M"
        );
    }

    #[test]
    fn click_mode_reports_no_motion_at_all() {
        let mode = TermMode::default() | TermMode::MOUSE_CLICK | TermMode::MOUSE_SGR;
        assert!(
            encode_mouse(MouseButton::Left, MouseEventKind::Move, 3, 3, PLAIN, mode).is_none()
        );
        assert!(encode_mouse(MouseButton::Left, MouseEventKind::Press, 3, 3, PLAIN, mode).is_some());
    }

    #[test]
    fn x10_release_keeps_modifier_bits() {
        // No 1006: the legacy encoding, where a release is always button 3.
        let mode = TermMode::default() | TermMode::MOUSE_CLICK;
        let ctrl = Modifiers {
            ctrl: true,
            ..PLAIN
        };
        // Ctrl is 16, release is 3, so the button byte is 32 + 19.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseEventKind::Release, 0, 0, ctrl, mode).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 19, 33, 33]
        );
    }

    #[test]
    fn the_wheel_has_no_release() {
        let mode = TermMode::default() | TermMode::MOUSE_DRAG | TermMode::MOUSE_SGR;
        assert!(encode_mouse(
            MouseButton::WheelUp,
            MouseEventKind::Release,
            0,
            0,
            PLAIN,
            mode
        )
        .is_none());
        assert_eq!(
            encode_mouse(MouseButton::WheelUp, MouseEventKind::Press, 0, 0, PLAIN, mode).unwrap(),
            b"\x1b[<64;1;1M"
        );
    }

    #[test]
    fn mouse_is_silent_when_the_application_did_not_ask() {
        assert!(encode_mouse(
            MouseButton::Left,
            MouseEventKind::Press,
            0,
            0,
            PLAIN,
            TermMode::default()
        )
        .is_none());
    }
}
