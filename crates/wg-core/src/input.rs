//! Keyboard events -> byte sequences the pty expects.
//! Pure functions so the mapping is testable without a window system.
//!
//! Control characters from Ctrl+letter arrive as WM_CHAR control bytes and are
//! written to the pty verbatim by the shell; this module only owns keys that
//! need *sequence* encoding.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    pub const NONE: Self = Self {
        shift: false,
        alt: false,
        ctrl: false,
    };

    /// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    fn xterm_param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
}

/// Encode a key press. `None` = no M0 mapping (caller drops the event).
pub fn encode(key: Key, mods: Mods) -> Option<Vec<u8>> {
    let p = mods.xterm_param();
    let tilde = |n: u8| {
        if mods == Mods::NONE {
            format!("\x1b[{n}~").into_bytes()
        } else {
            format!("\x1b[{n};{p}~").into_bytes()
        }
    };
    let cs = |letter: char| {
        if mods == Mods::NONE {
            format!("\x1b[{letter}").into_bytes()
        } else {
            format!("\x1b[1;{p}{letter}").into_bytes()
        }
    };
    Some(match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => cs('A'),
        Key::Down => cs('B'),
        Key::Right => cs('C'),
        Key::Left => cs('D'),
        Key::Home => cs('H'),
        Key::End => cs('F'),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, mods: Mods) -> Vec<u8> {
        encode(key, mods).expect("key should encode")
    }

    #[test]
    fn plain_navigation_keys() {
        assert_eq!(enc(Key::Enter, Mods::NONE), b"\r");
        assert_eq!(enc(Key::Backspace, Mods::NONE), b"\x7f");
        assert_eq!(enc(Key::Tab, Mods::NONE), b"\t");
        assert_eq!(enc(Key::Escape, Mods::NONE), b"\x1b");
        assert_eq!(enc(Key::Up, Mods::NONE), b"\x1b[A");
        assert_eq!(enc(Key::Down, Mods::NONE), b"\x1b[B");
        assert_eq!(enc(Key::Right, Mods::NONE), b"\x1b[C");
        assert_eq!(enc(Key::Left, Mods::NONE), b"\x1b[D");
        assert_eq!(enc(Key::Home, Mods::NONE), b"\x1b[H");
        assert_eq!(enc(Key::End, Mods::NONE), b"\x1b[F");
        assert_eq!(enc(Key::Delete, Mods::NONE), b"\x1b[3~");
        assert_eq!(enc(Key::PageUp, Mods::NONE), b"\x1b[5~");
        assert_eq!(enc(Key::PageDown, Mods::NONE), b"\x1b[6~");
    }

    #[test]
    fn shift_tab_is_backtab() {
        assert_eq!(
            enc(
                Key::Tab,
                Mods {
                    shift: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b[Z"
        );
    }

    #[test]
    fn modified_arrows_use_xterm_params() {
        // ctrl = +4 -> param 5
        assert_eq!(
            enc(
                Key::Up,
                Mods {
                    ctrl: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b[1;5A"
        );
        // ctrl+shift = +5 -> param 6
        assert_eq!(
            enc(
                Key::Right,
                Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b[1;6C"
        );
    }

    #[test]
    fn modified_tilde_keys_embed_param() {
        assert_eq!(
            enc(
                Key::Delete,
                Mods {
                    ctrl: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b[3;5~"
        );
        assert_eq!(
            enc(
                Key::PageUp,
                Mods {
                    shift: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b[5;2~"
        );
    }
}
