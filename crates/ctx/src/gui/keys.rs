//! Turns key presses into the bytes a terminal program expects, following xterm.

/// The modifier keys held with a key, apart from Command, which the app keeps for itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

impl Mods {
    fn any(self) -> bool {
        self.shift || self.alt || self.control
    }

    /// xterm's modifier parameter: 1 plus shift 1, alt 2 and control 4.
    fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.control)
    }
}

/// The bytes for a key, or `None` when the key sends nothing.
///
/// `key` is GPUI's name for the key, such as `a`, `enter` or `pageup`. `typed` is the text the key
/// produces with its modifiers, such as `A` for shift-a or `ß` for option-s. `app_cursor` is set
/// when the program asked for application cursor keys.
pub fn encode(key: &str, typed: Option<&str>, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    let esc = |rest: &str| format!("\x1b{rest}").into_bytes();
    let alt_prefix = |bytes: Vec<u8>| {
        if mods.alt {
            [vec![0x1b], bytes].concat()
        } else {
            bytes
        }
    };
    // Cursor and editing keys: ESC [ <code> with modifiers as ESC [ 1 ; <m> <code>.
    let cursor = |code: char| {
        if mods.any() {
            esc(&format!("[1;{}{code}", mods.param()))
        } else if app_cursor {
            esc(&format!("O{code}"))
        } else {
            esc(&format!("[{code}"))
        }
    };
    let tilde = |n: u8| {
        if mods.any() {
            esc(&format!("[{n};{}~", mods.param()))
        } else {
            esc(&format!("[{n}~"))
        }
    };
    let function = |code: char| {
        if mods.any() {
            esc(&format!("[1;{}{code}", mods.param()))
        } else {
            esc(&format!("O{code}"))
        }
    };
    let bytes = match key {
        "enter" => alt_prefix(b"\r".to_vec()),
        "tab" if mods.shift => esc("[Z"),
        "tab" => alt_prefix(b"\t".to_vec()),
        "backspace" if mods.control => alt_prefix(vec![0x08]),
        "backspace" => alt_prefix(vec![0x7f]),
        "escape" => alt_prefix(vec![0x1b]),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => function('P'),
        "f2" => function('Q'),
        "f3" => function('R'),
        "f4" => function('S'),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        _ if mods.control => alt_prefix(vec![control_byte(key)?]),
        "space" => alt_prefix(b" ".to_vec()),
        _ => {
            let typed = typed.filter(|t| !t.is_empty())?;
            // Option that composes a character, such as option-s for ß, sends the character.
            // Option on a key it leaves alone sends ESC first, as Meta.
            if mods.alt && typed.eq_ignore_ascii_case(key) {
                alt_prefix(typed.as_bytes().to_vec())
            } else {
                typed.as_bytes().to_vec()
            }
        }
    };
    Some(bytes)
}

/// The control character for control and a key, such as 0x03 for control-c.
fn control_byte(key: &str) -> Option<u8> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return (key == "space").then_some(0);
    }
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// Mouse buttons as terminal programs number them.
pub const LEFT: u8 = 0;
pub const MOTION: u8 = 32;
pub const WHEEL_UP: u8 = 64;
pub const WHEEL_DOWN: u8 = 65;

/// A mouse report for a program that asked for mouse events. `column` and `line` count from 0.
/// Without SGR mode, positions past 222 cannot be sent, and this returns `None`.
pub fn mouse(
    button: u8,
    mods: Mods,
    column: usize,
    line: usize,
    pressed: bool,
    sgr: bool,
) -> Option<Vec<u8>> {
    let code =
        button + 4 * u8::from(mods.shift) + 8 * u8::from(mods.alt) + 16 * u8::from(mods.control);
    if sgr {
        let end = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{};{}{end}", column + 1, line + 1).into_bytes());
    }
    if column > 222 || line > 222 {
        return None;
    }
    // X10 encoding reports every release as button 3.
    let code = if pressed { code } else { 3 + (code & !3) };
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        33 + column as u8,
        33 + line as u8,
    ])
}

/// Text to paste, wrapped for bracketed paste when the program asked for it.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // A pasted end marker would let the text out of the bracket, so ESC is dropped.
        let text = text.replace('\x1b', "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        shift: false,
        alt: false,
        control: false,
    };

    fn key(key: &str, typed: Option<&str>, mods: Mods) -> Vec<u8> {
        encode(key, typed, mods, false).unwrap()
    }

    #[test]
    fn typed_characters_pass_through() {
        assert_eq!(key("a", Some("a"), NONE), b"a");
        let shift = Mods {
            shift: true,
            ..NONE
        };
        assert_eq!(key("a", Some("A"), shift), b"A");
        assert_eq!(key("space", Some(" "), NONE), b" ");
        assert_eq!(encode("a", None, NONE, false), None);
    }

    #[test]
    fn control_keys_send_control_characters() {
        let control = Mods {
            control: true,
            ..NONE
        };
        assert_eq!(key("c", Some("c"), control), [0x03]);
        assert_eq!(key("d", None, control), [0x04]);
        assert_eq!(key("[", None, control), [0x1b]);
        assert_eq!(key("space", None, control), [0x00]);
        assert_eq!(key("backspace", None, control), [0x08]);
    }

    #[test]
    fn option_is_meta_unless_it_composes_a_character() {
        let alt = Mods { alt: true, ..NONE };
        assert_eq!(key("b", Some("b"), alt), b"\x1bb");
        assert_eq!(key("s", Some("ß"), alt), "ß".as_bytes());
        assert_eq!(key("enter", Some("\n"), alt), b"\x1b\r");
        assert_eq!(key("backspace", None, alt), b"\x1b\x7f");
    }

    #[test]
    fn cursor_keys_follow_the_cursor_mode_and_modifiers() {
        assert_eq!(key("up", None, NONE), b"\x1b[A");
        assert_eq!(encode("up", None, NONE, true).unwrap(), b"\x1bOA");
        let shift = Mods {
            shift: true,
            ..NONE
        };
        assert_eq!(key("left", None, shift), b"\x1b[1;2D");
        let control_alt = Mods {
            alt: true,
            control: true,
            ..NONE
        };
        assert_eq!(key("right", None, control_alt), b"\x1b[1;7C");
    }

    #[test]
    fn editing_and_function_keys_use_xterm_codes() {
        assert_eq!(key("enter", Some("\n"), NONE), b"\r");
        assert_eq!(key("tab", Some("\t"), NONE), b"\t");
        let shift = Mods {
            shift: true,
            ..NONE
        };
        assert_eq!(key("tab", Some("\t"), shift), b"\x1b[Z");
        assert_eq!(key("backspace", None, NONE), [0x7f]);
        assert_eq!(key("delete", None, NONE), b"\x1b[3~");
        assert_eq!(key("pagedown", None, NONE), b"\x1b[6~");
        assert_eq!(key("f1", None, NONE), b"\x1bOP");
        assert_eq!(key("f5", None, NONE), b"\x1b[15~");
        assert_eq!(key("f12", None, shift), b"\x1b[24;2~");
    }

    #[test]
    fn mouse_reports_use_sgr_or_x10() {
        assert_eq!(
            mouse(LEFT, NONE, 4, 2, true, true).unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            mouse(LEFT, NONE, 4, 2, false, true).unwrap(),
            b"\x1b[<0;5;3m"
        );
        let shift = Mods {
            shift: true,
            ..NONE
        };
        assert_eq!(
            mouse(WHEEL_UP, shift, 0, 0, true, true).unwrap(),
            b"\x1b[<68;1;1M"
        );
        assert_eq!(
            mouse(LEFT, NONE, 0, 0, true, false).unwrap(),
            [0x1b, b'[', b'M', 32, 33, 33]
        );
        assert_eq!(
            mouse(LEFT, NONE, 0, 0, false, false).unwrap(),
            [0x1b, b'[', b'M', 35, 33, 33]
        );
        assert_eq!(mouse(LEFT, NONE, 300, 0, true, false), None);
    }

    #[test]
    fn pastes_are_bracketed_when_asked() {
        assert_eq!(paste("a\nb", false), b"a\rb");
        assert_eq!(paste("a\x1b[201~b", true), b"\x1b[200~a[201~b\x1b[201~");
    }
}
