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

/// Encode a key press into the byte sequence the pty expects.
///
/// `app_cursor` mirrors DECCKM (DECSET 1, application cursor keys): when set
/// and *no* modifier is held, Up/Down/Right/Left/Home/End emit SS3 sequences
/// (`\x1bO…`). SS3 carries no modifier parameter, so any held modifier keeps
/// the CSI form (`\x1b[1;<p><ch>`).
///
/// Alt uses xterm meta encoding: any non-empty sequence gains a leading ESC
/// (Alt+Backspace -> ESC DEL, Alt+Enter -> ESC CR, Alt+Up -> ESC + CSI).
/// Alt+Escape therefore doubles to ESC ESC — indistinguishable from a bare
/// ESC followed by another, the same ambiguity xterm itself has. Alt+Tab
/// would encode as ESC + TAB, but Windows consumes Alt+Tab at the OS level
/// before it ever reaches the window.
pub fn encode(key: Key, mods: Mods, app_cursor: bool) -> Vec<u8> {
    let p = mods.xterm_param();
    let app_nav = app_cursor && mods == Mods::NONE;
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
    let seq = match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up if app_nav => b"\x1bOA".to_vec(),
        Key::Down if app_nav => b"\x1bOB".to_vec(),
        Key::Right if app_nav => b"\x1bOC".to_vec(),
        Key::Left if app_nav => b"\x1bOD".to_vec(),
        Key::Home if app_nav => b"\x1bOH".to_vec(),
        Key::End if app_nav => b"\x1bOF".to_vec(),
        Key::Up => cs('A'),
        Key::Down => cs('B'),
        Key::Right => cs('C'),
        Key::Left => cs('D'),
        Key::Home => cs('H'),
        Key::End => cs('F'),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
    };
    if mods.alt {
        let mut out = Vec::with_capacity(seq.len() + 1);
        out.push(b'\x1b');
        out.extend_from_slice(&seq);
        out
    } else {
        seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, mods: Mods) -> Vec<u8> {
        encode(key, mods, false)
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

    #[test]
    fn app_cursor_mode_emits_ss3_for_six_nav_keys() {
        // DECCKM: 无修饰时六个导航键走 SS3(\x1bO<ch>)
        let cases = [
            (Key::Up, b'A'),
            (Key::Down, b'B'),
            (Key::Right, b'C'),
            (Key::Left, b'D'),
            (Key::Home, b'H'),
            (Key::End, b'F'),
        ];
        for (key, ch) in cases {
            assert_eq!(encode(key, Mods::NONE, true), [0x1b, b'O', ch]);
        }
    }

    #[test]
    fn app_cursor_mode_with_any_modifier_stays_csi() {
        // SS3 没有修饰参数;xterm 规范规定带修饰的键回落 CSI
        assert_eq!(
            encode(
                Key::Up,
                Mods {
                    ctrl: true,
                    ..Mods::NONE
                },
                true
            ),
            b"\x1b[1;5A"
        );
        assert_eq!(
            encode(
                Key::End,
                Mods {
                    shift: true,
                    ..Mods::NONE
                },
                true
            ),
            b"\x1b[1;2F"
        );
    }

    #[test]
    fn app_cursor_mode_leaves_non_nav_keys_alone() {
        assert_eq!(encode(Key::Enter, Mods::NONE, true), b"\r");
        assert_eq!(encode(Key::Backspace, Mods::NONE, true), b"\x7f");
        assert_eq!(encode(Key::Delete, Mods::NONE, true), b"\x1b[3~");
        assert_eq!(encode(Key::Escape, Mods::NONE, true), b"\x1b");
    }

    #[test]
    fn alt_prepends_escape_meta_encoding() {
        // xterm meta 编码:任何非空序列前加 ESC
        assert_eq!(
            enc(
                Key::Backspace,
                Mods {
                    alt: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b\x7f"
        );
        assert_eq!(
            enc(
                Key::Enter,
                Mods {
                    alt: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b\r"
        );
        // alt 使方向键走 CSI(参数 3 = 1 + alt),meta 前缀再叠在上面
        assert_eq!(
            enc(
                Key::Up,
                Mods {
                    alt: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b\x1b[1;3A"
        );
    }

    #[test]
    fn alt_escape_and_alt_tab_are_documented_edge_cases() {
        // Alt+Escape 变成 ESC ESC —— 与裸 ESC 难以区分,xterm 的 meta
        // 编码同样如此,接受这个歧义(不改特判)。
        assert_eq!(
            enc(
                Key::Escape,
                Mods {
                    alt: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b\x1b"
        );
        // Alt+Tab 本应编码为 ESC + TAB,但 Windows 在 OS 层就吃掉了
        // Alt+Tab,此路径实际只在测试里可达。
        assert_eq!(
            enc(
                Key::Tab,
                Mods {
                    alt: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b\t"
        );
    }

    #[test]
    fn alt_shift_tab_prepends_escape_to_backtab() {
        assert_eq!(
            enc(
                Key::Tab,
                Mods {
                    shift: true,
                    alt: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b\x1b[Z"
        );
    }
}
